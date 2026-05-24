use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    BOOT_EXTENT_FLAG_READONLY, BOOT_EXTENT_MAX_EXTENTS, BOOT_EXTENT_PATH_CAPACITY,
    BootExtentLeaseWire, BootExtentWire, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS, COMMERCIAL_MAX_PROTOCOL_STORAGED,
    COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY, COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE,
    COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN, COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT,
    COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, IPC_SERVICE_STORAGED, STORAGE_FLAG_READONLY,
    STORAGE_LIST_MAX_DESCRIPTORS, STORAGED_IPC_ABI_VERSION, STORAGED_OP_BOOT_EXTENT_LOOKUP,
    STORAGED_OP_LIST_COUNT, STORAGED_OP_LIST_GET, STORAGED_OP_PING, STORAGED_OP_ROOT_STATUS,
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_STORAGE_LIST_BROKER,
    StorageBlockDescriptorWire, StorageListBrokerArgs, StoragedRequest, StoragedResponse,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const ROOT_FILE_EXTENTS_REGISTRY_PATH: &str = "system/registry/kernel/root-file-extents.tsv";

fn main() {
    observability_client::info!("storaged", service, "service started");
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_STORAGED,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("storaged: storage policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = CommercialMaxProtocolRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            reply_commercial_request(reply_cap, &request);
            continue;
        }
        if received as usize != size_of::<StoragedRequest>() {
            continue;
        }
        let legacy = unsafe {
            &*((&request as *const CommercialMaxProtocolRequest).cast::<StoragedRequest>())
        };
        reply_legacy_request(reply_cap, legacy, received as usize);
    }
}

fn reply_legacy_request(reply_cap: u64, request: &StoragedRequest, received: usize) {
    let mut response = StoragedResponse {
        version: STORAGED_IPC_ABI_VERSION,
        op: request.op,
        ..StoragedResponse::default()
    };
    response.status = match validate(received as usize, &request) {
        Ok(()) => dispatch(&request, &mut response),
        Err(errno) => errno,
    };
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const StoragedResponse) as u64,
        size_of::<StoragedResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(std::io::stderr(), "storaged: reply failed errno={}", -reply);
    }
}

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = validate_commercial_request(request)
        .and_then(|_| dispatch_commercial(request, &mut response))
        .err()
        .unwrap_or(0);
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(std::io::stderr(), "storaged: reply failed errno={}", -reply);
    }
}

fn dispatch(request: &StoragedRequest, response: &mut StoragedResponse) -> i32 {
    match request.op {
        STORAGED_OP_PING => {
            response.value = request.arg0;
            0
        }
        STORAGED_OP_LIST_COUNT => match list_descriptors() {
            Ok(descriptors) => {
                response.value = descriptors.len() as u64;
                0
            }
            Err(errno) => errno,
        },
        STORAGED_OP_LIST_GET => match list_descriptors() {
            Ok(descriptors) => {
                let index = request.arg0 as usize;
                if index >= descriptors.len() {
                    libc::ERANGE
                } else {
                    response.payload = descriptors[index];
                    response.value = descriptors.len() as u64;
                    0
                }
            }
            Err(errno) => errno,
        },
        STORAGED_OP_ROOT_STATUS => match selected_root_descriptor() {
            Ok(descriptor) => {
                response.payload = descriptor;
                response.value = descriptor.id as u64;
                0
            }
            Err(errno) => errno,
        },
        STORAGED_OP_BOOT_EXTENT_LOOKUP => match boot_extent_lookup(request) {
            Ok(lease) => {
                response.boot_extent = lease;
                response.value = lease.file_len;
                0
            }
            Err(errno) => errno,
        },
        _ => libc::EINVAL,
    }
}

fn dispatch_commercial(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY | COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN => {
            let descriptors = list_descriptors()?;
            response.value0 = descriptors.len() as u64;
            fill_storage_descriptors(&descriptors, request.header.op, response);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT
        | COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA => {
            let descriptor = selected_root_descriptor()?;
            response.value0 = descriptor.id as u64;
            response.descriptor_count = 1;
            response.descriptors[0] = storage_descriptor(&descriptor, request.header.op);
            response.capability = storage_capability(&descriptor, request.header.op);
            response.payload_len = write_payload_struct(&descriptor, &mut response.payload);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE => {
            let storaged_request = storaged_request_from_commercial(request)?;
            let lease = boot_extent_lookup(&storaged_request)?;
            response.value0 = lease.file_len;
            response.value1 = lease.hash_or_generation;
            response.capability = boot_extent_capability(&lease);
            response.payload_len = write_payload_struct(&lease, &mut response.payload);
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
}

fn selected_root_descriptor() -> Result<StorageBlockDescriptorWire, i32> {
    let descriptors = list_descriptors()?;
    descriptors
        .iter()
        .copied()
        .min_by_key(root_selection_rank)
        .ok_or(libc::ENODEV)
}

fn root_selection_rank(descriptor: &StorageBlockDescriptorWire) -> (u8, u8, u32) {
    let partition_rank = u8::from(descriptor.start_block == 0);
    let readonly_rank = u8::from((descriptor.flags & STORAGE_FLAG_READONLY) != 0);
    (partition_rank, readonly_rank, descriptor.id)
}

fn boot_extent_lookup(request: &StoragedRequest) -> Result<BootExtentLeaseWire, i32> {
    if request.path_len == 0 || request.path_len as usize > BOOT_EXTENT_PATH_CAPACITY {
        return Err(libc::EINVAL);
    }
    let len = request.path_len as usize;
    let path = std::str::from_utf8(&request.path[..len]).map_err(|_| libc::EINVAL)?;
    let mut lease = BootExtentLeaseWire {
        path_len: request.path_len,
        flags: BOOT_EXTENT_FLAG_READONLY,
        ..BootExtentLeaseWire::default()
    };
    lease.path[..len].copy_from_slice(&request.path[..len]);
    let registry_lease = boot_extent_lookup_registry(path, &request.path[..len])?;
    lease.file_len = registry_lease.file_len;
    lease.hash_or_generation = registry_lease.hash_or_generation;
    lease.extent_count = registry_lease.extent_count;
    lease.extents = registry_lease.extents;
    Ok(lease)
}

fn boot_extent_lookup_registry(
    request_path: &str,
    request_path_bytes: &[u8],
) -> Result<BootExtentLeaseWire, i32> {
    let Some(normalized) = normalize_extent_path(request_path) else {
        return Err(libc::EINVAL);
    };
    let text = match std::fs::read_to_string(ROOT_FILE_EXTENTS_REGISTRY_PATH) {
        Ok(text) => text,
        Err(_) => return Err(libc::ENOENT),
    };
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(path) = registry_field(line, "path") else {
            continue;
        };
        if normalize_extent_path(path) != Some(normalized) {
            continue;
        }
        let len = registry_field(line, "len")
            .ok_or(libc::EINVAL)?
            .parse::<u64>()
            .map_err(|_| libc::EINVAL)?;
        let extents = parse_extent_list(registry_field(line, "extents").ok_or(libc::EINVAL)?)?;
        if extents.len() > BOOT_EXTENT_MAX_EXTENTS {
            return Err(libc::EOVERFLOW);
        }
        let mut lease = BootExtentLeaseWire {
            path_len: request_path_bytes.len() as u32,
            flags: BOOT_EXTENT_FLAG_READONLY,
            file_len: len,
            hash_or_generation: boot_extent_generation(normalized, len, &extents),
            extent_count: extents.len() as u32,
            ..BootExtentLeaseWire::default()
        };
        lease.path[..request_path_bytes.len()].copy_from_slice(request_path_bytes);
        for (dest, src) in lease.extents.iter_mut().zip(extents.iter()) {
            *dest = *src;
        }
        return Ok(lease);
    }
    Err(libc::ENOENT)
}

fn normalize_extent_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    (!path.is_empty() && !path.contains("..")).then_some(path)
}

fn registry_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split('\t').find_map(|field| {
        let (field_key, value) = field.split_once('=')?;
        (field_key == key).then_some(value)
    })
}

fn parse_extent_list(text: &str) -> Result<Vec<BootExtentWire>, i32> {
    let mut extents = Vec::new();
    if text.is_empty() {
        return Ok(extents);
    }
    for item in text.split(',') {
        let (offset, len) = item.split_once(':').ok_or(libc::EINVAL)?;
        let disk_offset = offset.parse::<u64>().map_err(|_| libc::EINVAL)?;
        let len = len.parse::<u64>().map_err(|_| libc::EINVAL)?;
        if len != 0 {
            extents.push(BootExtentWire { disk_offset, len });
        }
    }
    Ok(extents)
}

fn boot_extent_generation(path: &str, file_len: u64, extents: &[BootExtentWire]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for value in [file_len] {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for extent in extents {
        for value in [extent.disk_offset, extent.len] {
            for byte in value.to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash.max(1)
}

fn list_descriptors() -> Result<Vec<StorageBlockDescriptorWire>, i32> {
    let mut buffer = vec![StorageBlockDescriptorWire::default(); STORAGE_LIST_MAX_DESCRIPTORS];
    let mut count: u32 = 0;
    let args = StorageListBrokerArgs {
        abi_version: 1,
        reserved0: 0,
        reserved1: 0,
        out_descriptors_ptr: buffer.as_mut_ptr() as u64,
        out_capacity: STORAGE_LIST_MAX_DESCRIPTORS as u32,
        reserved2: 0,
        out_count_ptr: (&mut count as *mut u32) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_STORAGE_LIST_BROKER,
        (&args as *const StorageListBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    buffer.truncate(count as usize);
    Ok(buffer)
}

fn validate(received: usize, request: &StoragedRequest) -> Result<(), i32> {
    if received != size_of::<StoragedRequest>()
        || request.version != STORAGED_IPC_ABI_VERSION
        || request.flags != 0
        || request.reserved0 != 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        STORAGED_OP_PING => Ok(()),
        STORAGED_OP_LIST_COUNT => Ok(()),
        STORAGED_OP_LIST_GET => Ok(()),
        STORAGED_OP_ROOT_STATUS => Ok(()),
        STORAGED_OP_BOOT_EXTENT_LOOKUP => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_STORAGED
        || request.header.flags != 0
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY
        | COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN
        | COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT
        | COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE
        | COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn storaged_request_from_commercial(
    request: &CommercialMaxProtocolRequest,
) -> Result<StoragedRequest, i32> {
    let len = request.path_len as usize;
    if len == 0 || len > BOOT_EXTENT_PATH_CAPACITY {
        return Err(libc::EINVAL);
    }
    let mut storaged_request = StoragedRequest {
        version: STORAGED_IPC_ABI_VERSION,
        op: STORAGED_OP_BOOT_EXTENT_LOOKUP,
        path_len: len as u32,
        ..StoragedRequest::default()
    };
    storaged_request.path[..len].copy_from_slice(&request.path[..len]);
    Ok(storaged_request)
}

fn fill_storage_descriptors(
    descriptors: &[StorageBlockDescriptorWire],
    op: u16,
    response: &mut CommercialMaxProtocolResponse,
) {
    let count = descriptors
        .len()
        .min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    for (index, descriptor) in descriptors.iter().take(count).enumerate() {
        response.descriptors[index] = storage_descriptor(descriptor, op);
    }
}

fn storage_descriptor(
    descriptor: &StorageBlockDescriptorWire,
    op: u16,
) -> CommercialMaxProtocolDescriptorWire {
    let mut wire = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_STORAGED,
        op,
        flags: descriptor.flags,
        service_id: descriptor.id as u64,
        capability_mask: storaged_capability_mask(op),
        value0: descriptor.block_count,
        value1: descriptor.start_block,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    let name_len = (descriptor.path_len as usize)
        .min(descriptor.path.len())
        .min(wire.name.len());
    wire.name_len = name_len as u16;
    wire.name[..name_len].copy_from_slice(&descriptor.path[..name_len]);
    wire
}

fn storage_capability(
    descriptor: &StorageBlockDescriptorWire,
    op: u16,
) -> CommercialMaxCapabilityLeaseWire {
    let mut wire = CommercialMaxCapabilityLeaseWire {
        lease_id: descriptor.id as u64,
        service_id: IPC_SERVICE_STORAGED,
        capability_mask: storaged_capability_mask(op),
        rights_mask: storaged_capability_mask(op),
        generation: descriptor.id as u64,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    let label_len = (descriptor.path_len as usize)
        .min(descriptor.path.len())
        .min(wire.label.len());
    wire.label_len = label_len as u16;
    wire.label[..label_len].copy_from_slice(&descriptor.path[..label_len]);
    wire
}

fn boot_extent_capability(lease: &BootExtentLeaseWire) -> CommercialMaxCapabilityLeaseWire {
    let mut wire = CommercialMaxCapabilityLeaseWire {
        lease_id: lease.hash_or_generation,
        service_id: IPC_SERVICE_STORAGED,
        capability_mask: storaged_capability_mask(COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE),
        rights_mask: storaged_capability_mask(COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE),
        generation: lease.hash_or_generation,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    let label_len = (lease.path_len as usize)
        .min(lease.path.len())
        .min(wire.label.len());
    wire.label_len = label_len as u16;
    wire.label[..label_len].copy_from_slice(&lease.path[..label_len]);
    wire
}

fn storaged_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY => 1 << 0,
        COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN => 1 << 1,
        COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT => 1 << 2,
        COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE => 1 << 3,
        COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA => 1 << 4,
        _ => 0,
    }
}

fn write_payload_struct<T>(value: &T, dest: &mut [u8]) -> u32 {
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    let count = bytes.len().min(dest.len());
    dest[..count].copy_from_slice(&bytes[..count]);
    count as u32
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
