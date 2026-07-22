#![no_std]
#![no_main]

extern crate alloc;

use core::mem::size_of;
#[cfg(not(test))]
use core::panic::PanicInfo;

use rustos_svc_runtime::ipc;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, LinuxSyscallOffloadRequest,
    LinuxSyscallOffloadResponse, Win32SyscallOffloadRequest, Win32SyscallOffloadResponse,
    COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE,
    COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY, COMMERCIAL_MAX_PAGERD_OP_WRITEBACK_POLICY,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_PAGERD, COMMERCIAL_MAX_PROTOCOL_SYSCALLD,
    COMMERCIAL_MAX_SYSCALLD_OP_CLOCK_POLICY, COMMERCIAL_MAX_SYSCALLD_OP_COLD_SYSCALL_OFFLOAD,
    COMMERCIAL_MAX_SYSCALLD_OP_CREDS_LIMITS, COMMERCIAL_MAX_SYSCALLD_OP_LINUX_POLICY,
    COMMERCIAL_MAX_SYSCALLD_OP_MM_POLICY, COMMERCIAL_MAX_SYSCALLD_OP_RANDOM_POLICY,
    COMMERCIAL_MAX_SYSCALLD_OP_WIN32_POLICY, IPC_MAX_INLINE_BYTES, IPC_SERVICE_LINUX_SYSCALLD,
    IPC_SERVICE_PAGERD, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY,
    SYSCALL_OFFLOAD_OP_LINUX_BRK, SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME,
    SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP, SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY,
    SYSCALL_OFFLOAD_OP_LINUX_GETEGID, SYSCALL_OFFLOAD_OP_LINUX_GETEUID,
    SYSCALL_OFFLOAD_OP_LINUX_GETGID, SYSCALL_OFFLOAD_OP_LINUX_GETPGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETPPID, SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM,
    SYSCALL_OFFLOAD_OP_LINUX_GETSID, SYSCALL_OFFLOAD_OP_LINUX_GETUID,
    SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST, SYSCALL_OFFLOAD_OP_LINUX_MADVISE,
    SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE, SYSCALL_OFFLOAD_OP_LINUX_MMAP,
    SYSCALL_OFFLOAD_OP_LINUX_MPROTECT, SYSCALL_OFFLOAD_OP_LINUX_MUNMAP,
    SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP, SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64,
    SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT, SYSCALL_OFFLOAD_OP_LINUX_RSEQ,
    SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY, SYSCALL_OFFLOAD_OP_LINUX_SETGID,
    SYSCALL_OFFLOAD_OP_LINUX_SETPGID, SYSCALL_OFFLOAD_OP_LINUX_SETSID,
    SYSCALL_OFFLOAD_OP_LINUX_SETUID, SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST,
    SYSCALL_OFFLOAD_OP_LINUX_UMASK, SYSCALL_OFFLOAD_OP_LINUX_UNAME,
    SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_CLOSE,
    SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION, SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
    SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE, SYSCALL_OFFLOAD_OP_WIN32_READ_FILE,
    SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE, SYSCALL_OFFLOAD_PATH_CAPACITY,
    WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
};

mod errno;
mod linux_policy;
mod win32_policy;

rustos_svc_runtime::entry!(service_main);

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

fn service_main() {
    ipc::debug_line("syscalld: service_main enter");
    let endpoint = ipc::endpoint_create();
    if endpoint < 0 {
        ipc::debug_line("syscalld: endpoint create failed");
        return;
    }

    ipc::debug_line("syscalld: endpoint created");
    let register = ipc::register_linux_syscall_endpoint(endpoint as u64);
    if register < 0 {
        ipc::debug_line("syscalld: endpoint register failed");
        return;
    }
    ipc::debug_line("syscalld: linux syscall endpoint registered");
    let pager_register = ipc::register_service_endpoint(IPC_SERVICE_PAGERD, endpoint as u64);
    if pager_register < 0 {
        ipc::debug_line("syscalld: pager endpoint register failed");
        return;
    }

    ipc::debug_line("syscalld: pager policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
        let mut reply_cap = 0_u64;
        let received = unsafe {
            ipc::recv(
                endpoint,
                request.as_mut_ptr(),
                request.len(),
                &mut reply_cap as *mut u64,
            )
        };
        if received < 0 {
            // Brief back-off (raw nanosleep, ~10 ms) before retrying.
            errno::sleep_millis(1);
            continue;
        }

        let response = handle_request(received as usize, &request);
        let reply = unsafe { ipc::reply(reply_cap, response.as_ptr(), response.len()) };
        if reply < 0 {
            ipc::debug_line("syscalld: reply failed");
        }
    }
}

// Replies are sent immediately; boxing the commercial response would leak
// from this early policy service's bootstrap allocator.
#[allow(clippy::large_enum_variant)]
enum SyscallOffloadReply {
    Linux(LinuxSyscallOffloadResponse),
    Win32(Win32SyscallOffloadResponse),
    Commercial(CommercialMaxProtocolResponse),
}

impl SyscallOffloadReply {
    fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Linux(response) => (response as *const LinuxSyscallOffloadResponse).cast::<u8>(),
            Self::Win32(response) => (response as *const Win32SyscallOffloadResponse).cast::<u8>(),
            Self::Commercial(response) => {
                (response as *const CommercialMaxProtocolResponse).cast::<u8>()
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Linux(_) => size_of::<LinuxSyscallOffloadResponse>(),
            Self::Win32(_) => size_of::<Win32SyscallOffloadResponse>(),
            Self::Commercial(_) => size_of::<CommercialMaxProtocolResponse>(),
        }
    }
}

fn handle_request(received: usize, bytes: &[u8]) -> SyscallOffloadReply {
    if received == size_of::<CommercialMaxProtocolRequest>() {
        let request = read_unaligned::<CommercialMaxProtocolRequest>(bytes);
        let response = handle_commercial_request(&request);
        return SyscallOffloadReply::Commercial(response);
    }
    if received == size_of::<LinuxSyscallOffloadRequest>() {
        let request = read_unaligned::<LinuxSyscallOffloadRequest>(bytes);
        let mut response = LinuxSyscallOffloadResponse::default();
        handle_linux_request(received, &request, &mut response);
        return SyscallOffloadReply::Linux(response);
    }
    if received == size_of::<Win32SyscallOffloadRequest>() {
        let request = read_unaligned::<Win32SyscallOffloadRequest>(bytes);
        let mut response = Win32SyscallOffloadResponse {
            version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
            op: request.op,
            ..Win32SyscallOffloadResponse::default()
        };
        handle_win32_request(received, &request, &mut response);
        return SyscallOffloadReply::Win32(response);
    }
    let response = LinuxSyscallOffloadResponse {
        status: errno::EINVAL,
        ..LinuxSyscallOffloadResponse::default()
    };
    SyscallOffloadReply::Linux(response)
}

fn handle_commercial_request(
    request: &CommercialMaxProtocolRequest,
) -> CommercialMaxProtocolResponse {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    if let Err(errno) = validate_commercial_request(request) {
        response.status = errno;
        return response;
    }
    if request.header.protocol == COMMERCIAL_MAX_PROTOCOL_PAGERD {
        handle_pager_request(request, &mut response);
        return response;
    }
    match request.header.op {
        COMMERCIAL_MAX_SYSCALLD_OP_LINUX_POLICY => {
            fill_syscall_descriptors(
                &mut response,
                &[
                    ("uname", SYSCALL_OFFLOAD_OP_LINUX_UNAME),
                    ("ids", SYSCALL_OFFLOAD_OP_LINUX_GETUID),
                    ("process-group", SYSCALL_OFFLOAD_OP_LINUX_GETPGID),
                    ("robust-list", SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST),
                    ("rseq", SYSCALL_OFFLOAD_OP_LINUX_RSEQ),
                ],
            );
        }
        COMMERCIAL_MAX_SYSCALLD_OP_WIN32_POLICY => {
            fill_syscall_descriptors(
                &mut response,
                &[
                    ("write-file", SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE),
                    ("read-file", SYSCALL_OFFLOAD_OP_WIN32_READ_FILE),
                    ("console-mode", SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE),
                    (
                        "virtual-memory",
                        SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY,
                    ),
                    ("exit-process", SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS),
                ],
            );
        }
        COMMERCIAL_MAX_SYSCALLD_OP_MM_POLICY => {
            fill_syscall_descriptors(
                &mut response,
                &[
                    ("brk", SYSCALL_OFFLOAD_OP_LINUX_BRK),
                    ("mmap", SYSCALL_OFFLOAD_OP_LINUX_MMAP),
                    ("mprotect", SYSCALL_OFFLOAD_OP_LINUX_MPROTECT),
                    ("munmap", SYSCALL_OFFLOAD_OP_LINUX_MUNMAP),
                    ("madvise", SYSCALL_OFFLOAD_OP_LINUX_MADVISE),
                    ("memfd-create", SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE),
                ],
            );
            response.capability = syscalld_capability("mm-policy", request.header.op);
        }
        COMMERCIAL_MAX_SYSCALLD_OP_CREDS_LIMITS => {
            fill_syscall_descriptors(
                &mut response,
                &[
                    ("getuid", SYSCALL_OFFLOAD_OP_LINUX_GETUID),
                    ("setuid", SYSCALL_OFFLOAD_OP_LINUX_SETUID),
                    ("getgid", SYSCALL_OFFLOAD_OP_LINUX_GETGID),
                    ("setgid", SYSCALL_OFFLOAD_OP_LINUX_SETGID),
                    ("prlimit64", SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64),
                    ("umask", SYSCALL_OFFLOAD_OP_LINUX_UMASK),
                ],
            );
        }
        COMMERCIAL_MAX_SYSCALLD_OP_CLOCK_POLICY => {
            fill_syscall_descriptors(
                &mut response,
                &[
                    ("nanosleep", SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP),
                    ("clock-gettime", SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME),
                    ("clock-nanosleep", SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP),
                    ("futex-policy", SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY),
                    ("delay-execution", SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION),
                    ("close", SYSCALL_OFFLOAD_OP_WIN32_CLOSE),
                ],
            );
        }
        COMMERCIAL_MAX_SYSCALLD_OP_RANDOM_POLICY => {
            fill_syscall_descriptors(
                &mut response,
                &[("getrandom", SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM)],
            );
            response.capability = syscalld_capability("random-policy", request.header.op);
        }
        COMMERCIAL_MAX_SYSCALLD_OP_COLD_SYSCALL_OFFLOAD => {
            response.value0 = 2;
            response.descriptor_count = 2;
            response.descriptors[0] = syscalld_descriptor(
                "linux-offload",
                request.header.op,
                SYSCALL_OFFLOAD_ABI_VERSION as u64,
            );
            response.descriptors[1] = syscalld_descriptor(
                "win32-offload",
                request.header.op,
                WIN32_SYSCALL_OFFLOAD_ABI_VERSION as u64,
            );
        }
        _ => response.status = errno::EINVAL,
    }
    response
}

fn handle_pager_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) {
    match request.header.op {
        COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT => {
            response.descriptor_count = 1;
            response.descriptors[0] = pager_descriptor(
                "backing-object",
                request.header.op,
                request.arg0,
                request.arg1,
            );
            response.capability = pager_capability("backing-object", request.header.op);
        }
        COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY => {
            response.descriptor_count = 1;
            response.descriptors[0] =
                pager_descriptor("page-cache", request.header.op, request.arg0, request.arg1);
            response.capability = pager_capability("page-cache", request.header.op);
        }
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE => {
            response.descriptor_count = 1;
            response.descriptors[0] = pager_descriptor(
                "fault-resolve",
                request.header.op,
                request.arg0,
                request.arg1,
            );
            response.capability = pager_capability("fault-resolve", request.header.op);
        }
        COMMERCIAL_MAX_PAGERD_OP_WRITEBACK_POLICY => {
            response.descriptor_count = 1;
            response.descriptors[0] =
                pager_descriptor("writeback", request.header.op, request.arg0, request.arg1);
            response.capability = pager_capability("writeback", request.header.op);
        }
        _ => response.status = errno::EINVAL,
    }
}

fn handle_linux_request(
    received: usize,
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    response.op = request.op;
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
        return;
    }

    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_UNAME => linux_policy::handle_uname(response),
        SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64 => linux_policy::handle_prlimit64(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY => {
            linux_policy::handle_sched_getaffinity(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETUID => {
            linux_policy::handle_id(request, linux_policy::IdKind::Uid, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETGID => {
            linux_policy::handle_id(request, linux_policy::IdKind::Gid, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETEUID => {
            linux_policy::handle_id(request, linux_policy::IdKind::Euid, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETEGID => {
            linux_policy::handle_id(request, linux_policy::IdKind::Egid, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SETUID => linux_policy::handle_setuid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETGID => linux_policy::handle_setgid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_UMASK => linux_policy::handle_umask(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM => linux_policy::handle_getrandom(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPPID => linux_policy::handle_getppid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPGID => linux_policy::handle_getpgid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETPGID => linux_policy::handle_setpgid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETSID => linux_policy::handle_getsid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETSID => linux_policy::handle_setsid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST => {
            linux_policy::handle_set_robust_list(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST => {
            linux_policy::handle_get_robust_list(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RSEQ => linux_policy::handle_rseq(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP => linux_policy::handle_nanosleep(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME => {
            linux_policy::handle_clock_gettime(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP => {
            linux_policy::handle_clock_nanosleep(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY => {
            linux_policy::handle_futex_policy(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY => {
            linux_policy::handle_arch_prctl_policy(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_MADVISE => linux_policy::handle_madvise(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_BRK => linux_policy::handle_brk(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MMAP => linux_policy::handle_mmap(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MPROTECT => linux_policy::handle_mprotect(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MUNMAP => linux_policy::handle_munmap(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE => {
            linux_policy::handle_memfd_create(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT => linux_policy::handle_process_exit(request),
        _ => response.status = errno::EINVAL,
    }
}

fn handle_win32_request(
    received: usize,
    request: &Win32SyscallOffloadRequest,
    response: &mut Win32SyscallOffloadResponse,
) {
    response.op = request.op;
    if received != size_of::<Win32SyscallOffloadRequest>()
        || request.version != WIN32_SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.pid == 0
    {
        response.status = win32_policy::ERROR_INVALID_PARAMETER;
        return;
    }
    win32_policy::handle_request(request, response);
}

fn validate_request(received: usize, request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if received != size_of::<LinuxSyscallOffloadRequest>()
        || request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(errno::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_UNAME
        | SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64
        | SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY
        | SYSCALL_OFFLOAD_OP_LINUX_GETUID
        | SYSCALL_OFFLOAD_OP_LINUX_GETGID
        | SYSCALL_OFFLOAD_OP_LINUX_GETEUID
        | SYSCALL_OFFLOAD_OP_LINUX_GETEGID
        | SYSCALL_OFFLOAD_OP_LINUX_SETUID
        | SYSCALL_OFFLOAD_OP_LINUX_SETGID
        | SYSCALL_OFFLOAD_OP_LINUX_UMASK
        | SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM
        | SYSCALL_OFFLOAD_OP_LINUX_GETPPID
        | SYSCALL_OFFLOAD_OP_LINUX_GETPGID
        | SYSCALL_OFFLOAD_OP_LINUX_SETPGID
        | SYSCALL_OFFLOAD_OP_LINUX_GETSID
        | SYSCALL_OFFLOAD_OP_LINUX_SETSID
        | SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST
        | SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST
        | SYSCALL_OFFLOAD_OP_LINUX_RSEQ
        | SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY
        | SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY
        | SYSCALL_OFFLOAD_OP_LINUX_MADVISE
        | SYSCALL_OFFLOAD_OP_LINUX_BRK
        | SYSCALL_OFFLOAD_OP_LINUX_MMAP
        | SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
        | SYSCALL_OFFLOAD_OP_LINUX_MUNMAP
        | SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE
        | SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT => Ok(()),
        _ => Err(errno::EINVAL),
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope() {
        return Err(errno::EINVAL);
    }
    match request.header.protocol {
        COMMERCIAL_MAX_PROTOCOL_SYSCALLD => match request.header.op {
            COMMERCIAL_MAX_SYSCALLD_OP_LINUX_POLICY
            | COMMERCIAL_MAX_SYSCALLD_OP_WIN32_POLICY
            | COMMERCIAL_MAX_SYSCALLD_OP_MM_POLICY
            | COMMERCIAL_MAX_SYSCALLD_OP_CREDS_LIMITS
            | COMMERCIAL_MAX_SYSCALLD_OP_CLOCK_POLICY
            | COMMERCIAL_MAX_SYSCALLD_OP_RANDOM_POLICY
            | COMMERCIAL_MAX_SYSCALLD_OP_COLD_SYSCALL_OFFLOAD => Ok(()),
            _ => Err(errno::EINVAL),
        },
        COMMERCIAL_MAX_PROTOCOL_PAGERD => match request.header.op {
            COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT
            | COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY
            | COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE
            | COMMERCIAL_MAX_PAGERD_OP_WRITEBACK_POLICY => Ok(()),
            _ => Err(errno::EINVAL),
        },
        _ => Err(errno::EINVAL),
    }
}

fn fill_syscall_descriptors(response: &mut CommercialMaxProtocolResponse, entries: &[(&str, u16)]) {
    let count = entries.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    response.value0 = entries.len() as u64;
    for (index, (name, op)) in entries.iter().take(count).enumerate() {
        response.descriptors[index] = syscalld_descriptor(name, *op, *op as u64);
    }
}

fn syscalld_descriptor(name: &str, op: u16, value0: u64) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_SYSCALLD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_LINUX_SYSCALLD,
        capability_mask: syscalld_capability_mask(op),
        value0,
        value1: 0,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn syscalld_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_SYSCALLD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_LINUX_SYSCALLD,
        capability_mask: syscalld_capability_mask(op),
        rights_mask: syscalld_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn syscalld_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_SYSCALLD_OP_LINUX_POLICY | SYSCALL_OFFLOAD_OP_LINUX_UNAME => 1 << 0,
        COMMERCIAL_MAX_SYSCALLD_OP_WIN32_POLICY | SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE => 1 << 1,
        COMMERCIAL_MAX_SYSCALLD_OP_MM_POLICY
        | SYSCALL_OFFLOAD_OP_LINUX_BRK
        | SYSCALL_OFFLOAD_OP_LINUX_MMAP
        | SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
        | SYSCALL_OFFLOAD_OP_LINUX_MUNMAP => 1 << 2,
        COMMERCIAL_MAX_SYSCALLD_OP_CREDS_LIMITS
        | SYSCALL_OFFLOAD_OP_LINUX_GETUID
        | SYSCALL_OFFLOAD_OP_LINUX_SETUID => 1 << 3,
        COMMERCIAL_MAX_SYSCALLD_OP_CLOCK_POLICY
        | SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_FUTEX_POLICY
        | SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY
        | SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION => 1 << 4,
        COMMERCIAL_MAX_SYSCALLD_OP_RANDOM_POLICY | SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM => 1 << 5,
        COMMERCIAL_MAX_SYSCALLD_OP_COLD_SYSCALL_OFFLOAD => 1 << 6,
        _ => 0,
    }
}

fn pager_descriptor(
    name: &str,
    op: u16,
    value0: u64,
    value1: u64,
) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_PAGERD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_PAGERD,
        capability_mask: pager_capability_mask(op),
        value0,
        value1,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn pager_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_PAGERD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_PAGERD,
        capability_mask: pager_capability_mask(op),
        rights_mask: pager_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn pager_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT => 1 << 0,
        COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY => 1 << 1,
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE => 1 << 2,
        COMMERCIAL_MAX_PAGERD_OP_WRITEBACK_POLICY => 1 << 3,
        _ => 0,
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = if bytes.len() < target.len() {
        bytes.len()
    } else {
        target.len()
    };
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_policy_requests_are_rejected() {
        let mut request = LinuxSyscallOffloadRequest {
            op: SYSCALL_OFFLOAD_OP_LINUX_UNAME,
            ..LinuxSyscallOffloadRequest::default()
        };
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Ok(())
        );

        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>() - 1, &request),
            Err(errno::EINVAL)
        );

        request.version = 99;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(errno::EINVAL)
        );
        request.version = SYSCALL_OFFLOAD_ABI_VERSION;

        request.op = 99;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(errno::EINVAL)
        );
        request.op = SYSCALL_OFFLOAD_OP_LINUX_UNAME;

        request.reserved0 = 1;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(errno::EINVAL)
        );
        request.reserved0 = 0;

        request.path_len = (SYSCALL_OFFLOAD_PATH_CAPACITY + 1) as u32;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(errno::EINVAL)
        );
    }
}
