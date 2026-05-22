#![no_std]
#![no_main]

use core::arch::asm;
use core::mem::size_of;
use core::panic::PanicInfo;
use core::slice;

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, CoreServiceLeaseWire,
    LifecycleDrainBrokerArgs, LifecycleEventWire, LoaderSpawnRequest, LoaderSpawnResponse,
    RootdIpcRequest, RootdIpcResponse, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST, COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE,
    COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH, COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL,
    COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY, IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_LOADERD,
    IPC_SERVICE_PROCD, IPC_SERVICE_ROOTD, IPC_SERVICE_VFSD, LIFECYCLE_DRAIN_MAX_EVENTS,
    LIFECYCLE_EVENT_EXIT, LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_EXEC_PATH_CAPACITY, ROOTD_IPC_ABI_VERSION, ROOTD_IPC_OP_LEASE_LIST,
    ROOTD_IPC_OP_STATUS, ROOTD_LEASE_STATE_EXITED, ROOTD_LEASE_STATE_FAILED,
    ROOTD_LEASE_STATE_RESTART_PENDING, ROOTD_LEASE_STATE_RUNNING, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_TRY_RECV,
    SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER, SYS_RUSTOS_SPAWN_EXEC,
};

const SYS_SCHED_YIELD: u64 = 24;
const SPAWN_FLAG_LOGICAL_ADMIN: u64 = 1;
// Bootstrap IPC hosts sit on hot syscall/loader/VFS paths. Give them a modest
// Linux nice-like service boost so dynamic-linker and driver bursts do not
// leave runnable servers behind for hundreds of milliseconds.
const CORE_SERVICE_WEIGHT_MICROS: u64 = 4_000;
const INITD_WEIGHT_MICROS: u64 = 2_000;

const SYSCALLD_EXEC: &[u8] = b"services/syscalld/syscalld.elf\0";
const VFSD_EXEC: &[u8] = b"services/vfsd/vfsd.elf\0";
const LOADERD_EXEC: &[u8] = b"services/loaderd/loaderd.elf\0";
const PROCD_EXEC: &[u8] = b"services/procd/procd.elf\0";
const INITD_EXEC: &[u8] = b"services/initd/initd.elf\0";
const INITD_LEASE_ID: u64 = 0;

#[derive(Clone, Copy)]
struct Lease {
    service_id: u64,
    exec_path: &'static [u8],
    pid: u64,
    restart_budget: u32,
    backoff_ms: u32,
    state: u16,
    exit_status: i32,
    weight_micros: u64,
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    debug_line(b"rootd: bootstrap enter\n");
    let endpoint = create_rootd_endpoint();
    let mut leases = [
        lease(
            IPC_SERVICE_LINUX_SYSCALLD,
            SYSCALLD_EXEC,
            CORE_SERVICE_WEIGHT_MICROS,
        ),
        lease(IPC_SERVICE_VFSD, VFSD_EXEC, CORE_SERVICE_WEIGHT_MICROS),
        lease(
            IPC_SERVICE_LOADERD,
            LOADERD_EXEC,
            CORE_SERVICE_WEIGHT_MICROS,
        ),
        lease(IPC_SERVICE_PROCD, PROCD_EXEC, CORE_SERVICE_WEIGHT_MICROS),
        lease(INITD_LEASE_ID, INITD_EXEC, INITD_WEIGHT_MICROS),
    ];

    // Start the core hosts first, then hand off to initd immediately so the
    // remaining bootstrap work can overlap with their own initialization.
    // Initd already gates the services it needs before it launches them, so
    // rootd does not need to serialize the entire bootstrap on readiness.
    for index in 0..4 {
        spawn_core_service_without_wait(&mut leases[index]);
    }

    debug_line(b"rootd: core services spawned, spawning initd\n");
    spawn_tracked_without_wait(&mut leases[4]);

    debug_line(b"rootd: initd spawned\n");
    loop {
        drain_lifecycle_events(&mut leases);
        serve_rootd_once(endpoint, &leases);
        restart_failed_leases(&mut leases);
        yield_now();
    }
}

fn lease(service_id: u64, exec_path: &'static [u8], weight_micros: u64) -> Lease {
    Lease {
        service_id,
        exec_path,
        pid: 0,
        restart_budget: 3,
        backoff_ms: 250,
        state: rustos_user_abi::syscall::ROOTD_LEASE_STATE_EMPTY,
        exit_status: 0,
        weight_micros,
    }
}

fn create_rootd_endpoint() -> u64 {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        debug_line(b"rootd: endpoint create failed\n");
        return 0;
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_ROOTD,
        endpoint as u64,
    );
    if register < 0 {
        debug_line(b"rootd: endpoint register failed\n");
        return 0;
    }
    debug_line(b"rootd: supervisor endpoint registered\n");
    endpoint as u64
}

fn spawn_core_service_without_wait(lease: &mut Lease) {
    if service_ready(lease.service_id) {
        lease.state = ROOTD_LEASE_STATE_RUNNING;
        return;
    }
    spawn_tracked_without_wait(lease);
}

fn spawn_tracked_without_wait(lease: &mut Lease) {
    loop {
        match spawn_exec(lease.exec_path, lease.weight_micros) {
            Ok(pid) => {
                lease.pid = pid;
                lease.state = ROOTD_LEASE_STATE_RUNNING;
                lease.exit_status = 0;
                break;
            }
            Err(_) => yield_now(),
        }
    }
}

fn spawn_exec(path: &'static [u8], weight_micros: u64) -> Result<u64, i64> {
    let argv = [path.as_ptr(), core::ptr::null()];
    let result = syscall6(
        SYS_RUSTOS_SPAWN_EXEC,
        path.as_ptr() as u64,
        argv.as_ptr() as u64,
        0,
        SPAWN_FLAG_LOGICAL_ADMIN,
        0,
        weight_micros,
    );
    if result < 0 {
        Err(-result)
    } else {
        Ok(result as u64)
    }
}

fn service_ready(service_id: u64) -> bool {
    if service_id == INITD_LEASE_ID {
        return false;
    }
    syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, service_id) > 0
}

fn drain_lifecycle_events(leases: &mut [Lease]) {
    let mut events = [LifecycleEventWire::default(); LIFECYCLE_DRAIN_MAX_EVENTS];
    let mut count = 0_u32;
    let args = LifecycleDrainBrokerArgs {
        abi_version: 1,
        reserved0: 0,
        reserved1: 0,
        out_events_ptr: events.as_mut_ptr() as u64,
        out_capacity: events.len() as u32,
        reserved2: 0,
        out_count_ptr: (&mut count as *mut u32) as u64,
    };
    if syscall1(
        SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER,
        (&args as *const LifecycleDrainBrokerArgs) as u64,
    ) < 0
    {
        return;
    }
    for event in events.iter().take(count as usize) {
        if event.event != LIFECYCLE_EVENT_EXIT {
            continue;
        }
        for lease in leases.iter_mut() {
            if lease.pid == event.pid {
                lease.state = ROOTD_LEASE_STATE_EXITED;
                lease.exit_status = event.exit_status;
                break;
            }
        }
    }
}

fn serve_rootd_once(endpoint: u64, leases: &[Lease]) {
    if endpoint == 0 {
        return;
    }
    let mut request = CommercialMaxProtocolRequest::default();
    let mut reply_cap = 0_u64;
    let received = syscall4(
        SYS_RUSTOS_IPC_TRY_RECV,
        endpoint,
        (&mut request as *mut CommercialMaxProtocolRequest) as u64,
        size_of::<CommercialMaxProtocolRequest>() as u64,
        (&mut reply_cap as *mut u64) as u64,
    );
    if received < 0 {
        return;
    }
    if received as usize == size_of::<CommercialMaxProtocolRequest>() {
        reply_commercial_max_request(reply_cap, &request, leases);
        return;
    }
    if received as usize != size_of::<RootdIpcRequest>() {
        return;
    }
    let legacy =
        unsafe { &*((&request as *const CommercialMaxProtocolRequest).cast::<RootdIpcRequest>()) };
    reply_legacy_rootd_request(reply_cap, legacy, leases, received as usize);
}

fn reply_legacy_rootd_request(
    reply_cap: u64,
    request: &RootdIpcRequest,
    leases: &[Lease],
    received: usize,
) {
    let mut response = RootdIpcResponse {
        version: ROOTD_IPC_ABI_VERSION,
        op: request.op,
        lease_count: leases.len() as u32,
        ..RootdIpcResponse::default()
    };
    response.status = match validate_rootd_request(received, request) {
        Ok(()) => match fill_rootd_response(request, leases, &mut response) {
            Ok(()) => 0,
            Err(errno) => errno,
        },
        Err(errno) => errno,
    };
    let _ = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const RootdIpcResponse) as u64,
        size_of::<RootdIpcResponse>() as u64,
    );
}

fn reply_commercial_max_request(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    leases: &[Lease],
) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        value0: leases.len() as u64,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_max_request(request) {
        Ok(()) => match fill_commercial_max_response(request, leases, &mut response) {
            Ok(()) => 0,
            Err(errno) => errno,
        },
        Err(errno) => errno,
    };
    let _ = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
}

fn validate_rootd_request(received: usize, request: &RootdIpcRequest) -> Result<(), i32> {
    if received != size_of::<RootdIpcRequest>()
        || request.version != ROOTD_IPC_ABI_VERSION
        || request.flags != 0
        || request.reserved0 != 0
    {
        return Err(22);
    }
    match request.op {
        ROOTD_IPC_OP_STATUS | ROOTD_IPC_OP_LEASE_LIST => Ok(()),
        _ => Err(22),
    }
}

fn fill_rootd_response(
    request: &RootdIpcRequest,
    leases: &[Lease],
    response: &mut RootdIpcResponse,
) -> Result<(), i32> {
    match request.op {
        ROOTD_IPC_OP_STATUS => {
            let mut running = 0_u64;
            for lease in leases {
                if lease.state == ROOTD_LEASE_STATE_RUNNING {
                    running += 1;
                }
            }
            response.value = running;
            Ok(())
        }
        ROOTD_IPC_OP_LEASE_LIST => {
            let index = request.index as usize;
            if index >= leases.len() {
                return Err(34);
            }
            response.lease = lease_wire(&leases[index]);
            Ok(())
        }
        _ => Err(22),
    }
}

fn validate_commercial_max_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR
        || request.header.flags != 0
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(22);
    }
    match request.header.op {
        COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST
        | COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE
        | COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH
        | COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY
        | COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL => Ok(()),
        _ => Err(22),
    }
}

fn fill_commercial_max_response(
    request: &CommercialMaxProtocolRequest,
    leases: &[Lease],
    response: &mut CommercialMaxProtocolResponse,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST => {
            fill_manifest_descriptors(leases, response);
            response.payload_len = write_manifest_payload(leases, &mut response.payload) as u32;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE => {
            let lease = match lease_by_index(leases, request.arg0 as usize) {
                Ok(lease) => lease,
                Err(errno) => return Err(errno),
            };
            response.descriptor_count = 1;
            response.descriptors[0] = lease_descriptor(lease, request.header.op, 0);
            response.capability = lease_capability(lease, request.header.op);
            response.payload_len = write_payload_struct(&lease_wire(lease), &mut response.payload);
            response.value1 = lease.pid;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH => {
            fill_dependency_graph(leases, response);
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY => {
            let lease = match lease_by_index(leases, request.arg0 as usize) {
                Ok(lease) => lease,
                Err(errno) => return Err(errno),
            };
            response.descriptor_count = 1;
            response.descriptors[0] = lease_descriptor(lease, request.header.op, 0);
            response.value0 = lease.restart_budget as u64;
            response.value1 = lease.backoff_ms as u64;
            Ok(())
        }
        COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL => {
            let lease = match lease_by_service_or_index(leases, request.arg0, request.arg1 as usize)
            {
                Ok(lease) => lease,
                Err(errno) => return Err(errno),
            };
            response.descriptor_count = 1;
            response.descriptors[0] = lease_descriptor(lease, request.header.op, 0);
            response.value0 =
                if lease.service_id != INITD_LEASE_ID && service_ready(lease.service_id) {
                    1
                } else {
                    0
                };
            response.value1 = lease.state as u64;
            Ok(())
        }
        _ => Err(22),
    }
}

fn lease_wire(lease: &Lease) -> CoreServiceLeaseWire {
    let mut wire = CoreServiceLeaseWire {
        service_id: lease.service_id,
        pid: lease.pid,
        restart_budget: lease.restart_budget,
        backoff_ms: lease.backoff_ms,
        state: lease.state,
        exit_status: lease.exit_status,
        ..CoreServiceLeaseWire::default()
    };
    let path = trim_nul(lease.exec_path);
    let len = path.len().min(wire.exec_path.len());
    wire.exec_path_len = len as u32;
    wire.exec_path[..len].copy_from_slice(&path[..len]);
    wire
}

fn lease_by_index(leases: &[Lease], index: usize) -> Result<&Lease, i32> {
    if index < leases.len() {
        Ok(&leases[index])
    } else {
        Err(34)
    }
}

fn lease_by_service_or_index(
    leases: &[Lease],
    service_id: u64,
    fallback_index: usize,
) -> Result<&Lease, i32> {
    if service_id != 0 {
        for lease in leases {
            if lease.service_id == service_id {
                return Ok(lease);
            }
        }
        return Err(34);
    }
    lease_by_index(leases, fallback_index)
}

fn fill_manifest_descriptors(leases: &[Lease], response: &mut CommercialMaxProtocolResponse) {
    let count = leases.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    let mut index = 0usize;
    while index < count {
        let lease = &leases[index];
        response.descriptors[index] = lease_descriptor(
            lease,
            COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE,
            index as u64,
        );
        index += 1;
    }
}

fn fill_dependency_graph(leases: &[Lease], response: &mut CommercialMaxProtocolResponse) {
    let count = leases.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    let mut index = 0usize;
    while index < count {
        let lease = &leases[index];
        let mut descriptor = lease_descriptor(
            lease,
            COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH,
            index as u64,
        );
        descriptor.value1 = if index == 0 {
            0
        } else {
            leases[index - 1].service_id
        };
        response.descriptors[index] = descriptor;
        index += 1;
    }
}

fn lease_descriptor(lease: &Lease, op: u16, index: u64) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
        op,
        service_id: lease.service_id,
        capability_mask: rootd_capability_mask(op),
        value0: index,
        value1: lease.pid,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    let name = service_name(lease.exec_path);
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn lease_capability(lease: &Lease, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: lease.pid,
        service_id: lease.service_id,
        subject_pid: lease.pid,
        capability_mask: rootd_capability_mask(op),
        rights_mask: rootd_capability_mask(op),
        generation: lease.pid,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    let name = service_name(lease.exec_path);
    copy_label(name, &mut capability.label, &mut capability.label_len);
    capability
}

fn rootd_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST => 1 << 0,
        COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE => 1 << 1,
        COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH => 1 << 2,
        COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY => 1 << 3,
        COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL => 1 << 4,
        _ => 0,
    }
}

fn service_name(path: &'static [u8]) -> &'static [u8] {
    let path = trim_nul(path);
    let mut start = 0;
    let mut index = 0;
    while index < path.len() {
        if path[index] == b'/' {
            start = index + 1;
        }
        index += 1;
    }
    &path[start..]
}

fn copy_label(src: &[u8], dest: &mut [u8], len: &mut u16) {
    let count = src.len().min(dest.len());
    dest[..count].copy_from_slice(&src[..count]);
    *len = count as u16;
}

fn write_manifest_payload(leases: &[Lease], dest: &mut [u8]) -> usize {
    let mut written = 0;
    for lease in leases {
        let path = trim_nul(lease.exec_path);
        if written != 0 {
            if written >= dest.len() {
                break;
            }
            dest[written] = b'\n';
            written += 1;
        }
        let remaining = dest.len().saturating_sub(written);
        let count = path.len().min(remaining);
        dest[written..written + count].copy_from_slice(&path[..count]);
        written += count;
        if count < path.len() {
            break;
        }
    }
    written
}

fn write_payload_struct<T>(value: &T, dest: &mut [u8]) -> u32 {
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    let count = bytes.len().min(dest.len());
    dest[..count].copy_from_slice(&bytes[..count]);
    count as u32
}

fn trim_nul(bytes: &'static [u8]) -> &'static [u8] {
    if bytes.last() == Some(&0) {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    }
}

fn restart_failed_leases(leases: &mut [Lease]) {
    for lease in leases.iter_mut() {
        if !matches!(
            lease.state,
            ROOTD_LEASE_STATE_EXITED | ROOTD_LEASE_STATE_RESTART_PENDING
        ) {
            continue;
        }
        if lease.restart_budget == 0 {
            lease.state = ROOTD_LEASE_STATE_FAILED;
            continue;
        }
        match restart_lease(lease) {
            Ok(pid) => {
                lease.pid = pid;
                lease.restart_budget -= 1;
                lease.state = ROOTD_LEASE_STATE_RUNNING;
                lease.exit_status = 0;
            }
            Err(_) => {
                lease.state = ROOTD_LEASE_STATE_RESTART_PENDING;
            }
        }
    }
}

fn restart_lease(lease: &Lease) -> Result<u64, i64> {
    if lease.service_id == IPC_SERVICE_LOADERD {
        return spawn_exec(lease.exec_path, lease.weight_micros);
    }
    if !service_ready(IPC_SERVICE_LOADERD) {
        return Err(11);
    }
    spawn_exec_via_loaderd(lease.exec_path, lease.weight_micros)
}

fn spawn_exec_via_loaderd(path: &'static [u8], weight_micros: u64) -> Result<u64, i64> {
    let endpoint = syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, IPC_SERVICE_LOADERD);
    if endpoint <= 0 {
        return Err(if endpoint < 0 { -endpoint } else { 11 });
    }
    let path = trim_nul(path);
    if path.is_empty()
        || path.len() > LOADER_SPAWN_EXEC_PATH_CAPACITY
        || path.len() >= LOADER_SPAWN_ARG_BYTES
        || contains_nul(path)
    {
        return Err(22);
    }
    let mut request = LoaderSpawnRequest::default();
    request.version = LOADER_REQUEST_ABI_VERSION;
    request.op = LOADER_OP_SPAWN_EXEC;
    request.flags = SPAWN_FLAG_LOGICAL_ADMIN as u32;
    request.weight_micros = weight_micros;
    request.exec_path_len = path.len() as u32;
    request.argv_count = 1;
    request.argv_bytes_len = (path.len() + 1) as u32;
    copy_bytes(path, &mut request.exec_path);
    copy_bytes(path, &mut request.argv_bytes);
    request.argv_bytes[path.len()] = 0;

    let mut response = LoaderSpawnResponse::default();
    let result = syscall5(
        SYS_RUSTOS_IPC_CALL,
        endpoint as u64,
        (&request as *const LoaderSpawnRequest) as u64,
        size_of::<LoaderSpawnRequest>() as u64,
        (&mut response as *mut LoaderSpawnResponse) as u64,
        size_of::<LoaderSpawnResponse>() as u64,
    );
    if result < 0 {
        return Err(-result);
    }
    if result as usize != size_of::<LoaderSpawnResponse>()
        || response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_SPAWN_EXEC
    {
        return Err(22);
    }
    if response.status != 0 {
        return Err(response.status as i64);
    }
    if response.pid <= 0 {
        return Err(22);
    }
    Ok(response.pid as u64)
}

fn contains_nul(bytes: &[u8]) -> bool {
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 0 {
            return true;
        }
        index += 1;
    }
    false
}

fn copy_bytes(src: &[u8], dest: &mut [u8]) {
    let mut index = 0usize;
    while index < src.len() {
        dest[index] = src[index];
        index += 1;
    }
}

fn debug_line(bytes: &[u8]) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    );
}

fn yield_now() {
    let _ = syscall0(SYS_SCHED_YIELD);
}

fn syscall0(number: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall5(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            in("r8") arg4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            in("r8") arg4,
            in("r9") arg5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    debug_line(b"rootd: panic\n");
    loop {
        yield_now();
    }
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, len: usize) -> *mut u8 {
    let mut offset = 0usize;
    while offset < len {
        dest.add(offset).write(value as u8);
        offset += 1;
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    let mut offset = 0usize;
    while offset < len {
        dest.add(offset).write(src.add(offset).read());
        offset += 1;
    }
    dest
}
