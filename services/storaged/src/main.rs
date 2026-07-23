use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::thread;
use std::time::Duration;

mod block;

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, DvmBlockInfoWire,
    StorageBlockDescriptorWire, StoragedBulkReadResponse, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS, COMMERCIAL_MAX_PROTOCOL_STORAGED,
    COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA, COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ, COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK,
    COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE, COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN,
    COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT, COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA,
    IPC_SERVICE_STORAGED, IPC_SERVICE_VFSD, STORAGED_BULK_READ_PAYLOAD_CAPACITY,
    STORAGE_FLAG_READONLY, STORAGE_TRANSPORT_DVM_BLOCK, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
static mut STORAGED_BULK_RESPONSE_SLOT: StoragedBulkReadResponse =
    StoragedBulkReadResponse::zeroed();

fn main() {
    observability_client::info!("storaged", service, "service started");
    debug_line("storaged: endpoint create begin");
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        debug_line("storaged: endpoint create failed");
        let _ = writeln!(
            std::io::stderr(),
            "storaged: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    debug_line("storaged: endpoint create done");
    debug_line("storaged: endpoint register begin");
    let register = register_service_endpoint(IPC_SERVICE_STORAGED, endpoint as u64);
    if register < 0 {
        debug_line(format!("storaged: endpoint register failed errno={}", -register).as_str());
        let _ = writeln!(
            std::io::stderr(),
            "storaged: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("storaged: storage policy endpoint registered");
    prove_dvm_block_end_to_end();
    serve(endpoint as u64);
}

fn prove_dvm_block_end_to_end() {
    let result = block::info().and_then(|info| {
        block::flush(info.generation)?;
        Ok(info.generation)
    });
    if let Ok(generation) = result {
        debug_line(dvm_block_e2e_marker(generation).as_str());
    }
}

fn dvm_block_e2e_marker(generation: u64) -> String {
    format!(
        "storaged: dvm-block e2e flush completed generation={generation} \
         path=vfs-policy->block-broker->shared-ring->linux-dvm->backing"
    )
}

fn serve(endpoint: u64) {
    loop {
        let mut request = CommercialMaxProtocolRequest::default();
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            (&mut request as *mut CommercialMaxProtocolRequest) as u64,
            size_of::<CommercialMaxProtocolRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        if received as usize != size_of::<CommercialMaxProtocolRequest>() {
            reply_commercial_error(reply_cap, &request, libc::EINVAL);
            continue;
        }
        if !request.subject_is_exact_sender(sender_pid, sender_tid)
            || rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_VFSD, sender_pid) < 0
        {
            reply_commercial_error(reply_cap, &request, libc::EACCES);
            continue;
        }
        reply_commercial_request(reply_cap, &request);
    }
}

fn reply_commercial_error(reply_cap: u64, request: &CommercialMaxProtocolRequest, status: i32) {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        status,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
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

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) {
    if request.header.op == COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK {
        reply_bulk_read(reply_cap, request);
        return;
    }
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Err(errno) => {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=envelope op={} errno={errno}",
                request.header.op
            );
            errno
        }
        Ok(()) => match dispatch_commercial(request, &mut response) {
            Ok(()) => 0,
            Err(errno) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "storaged: request rejected stage=dispatch op={} errno={errno}",
                    request.header.op
                );
                errno
            }
        },
    };
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

fn reply_bulk_read(reply_cap: u64, request: &CommercialMaxProtocolRequest) {
    let response = core::ptr::addr_of_mut!(STORAGED_BULK_RESPONSE_SLOT);
    unsafe {
        core::ptr::write_bytes(
            response.cast::<u8>(),
            0,
            size_of::<StoragedBulkReadResponse>(),
        );
        (*response).header = request.header;
        (*response).generation = request.arg0;
        (*response).lba = request.arg1;
        (*response).block_count = request.arg2;
    }

    let status = match validate_commercial_request(request) {
        Err(errno) => {
            let _ = writeln!(
                std::io::stderr(),
                "storaged: request rejected stage=envelope op={} errno={errno}",
                request.header.op
            );
            errno
        }
        Ok(()) => match block::read(request.arg0, request.arg1, request.arg2) {
            Ok(bytes) if bytes.len() <= STORAGED_BULK_READ_PAYLOAD_CAPACITY => {
                unsafe {
                    (*response).payload_len = bytes.len() as u32;
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        core::ptr::addr_of_mut!((*response).payload).cast::<u8>(),
                        bytes.len(),
                    );
                }
                0
            }
            Ok(_) => libc::EOVERFLOW,
            Err(errno) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "storaged: request rejected stage=dispatch op={} errno={errno}",
                    request.header.op
                );
                errno
            }
        },
    };
    unsafe {
        (*response).status = status;
    }
    let reply = syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        response as u64,
        size_of::<StoragedBulkReadResponse>() as u64,
    );
    if reply < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "storaged: bulk reply failed errno={}",
            -reply
        );
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
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO => {
            let info = block::info()?;
            response.value0 = info.generation;
            response.value1 = info.capacity_sectors;
            let wire = DvmBlockInfoWire {
                generation: info.generation,
                capacity_sectors: info.capacity_sectors,
                features: info.features,
                logical_block_size: info.logical_block_size,
                physical_block_size: info.physical_block_size,
                flags: info.flags,
                reserved0: 0,
            };
            response.payload_len = write_payload_struct(&wire, &mut response.payload);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ => {
            let bytes = block::read(request.arg0, request.arg1, request.arg2)?;
            if bytes.len() > response.payload.len() {
                return Err(libc::EOVERFLOW);
            }
            response.value0 = request.arg0;
            response.value1 = bytes.len() as u64;
            response.payload_len = bytes.len() as u32;
            response.payload[..bytes.len()].copy_from_slice(&bytes);
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE => {
            let fua = request.arg3 & COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA != 0;
            block::write(
                request.arg0,
                request.arg1,
                &request.payload[..request.payload_len as usize],
                fua,
            )?;
            response.value0 = request.arg0;
            response.value1 = request.payload_len as u64;
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH => {
            block::flush(request.arg0)?;
            response.value0 = request.arg0;
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

fn list_descriptors() -> Result<Vec<StorageBlockDescriptorWire>, i32> {
    let info = block::info()?;
    let sectors_per_block = u64::from(info.logical_block_size / 512);
    if sectors_per_block == 0 || !info.capacity_sectors.is_multiple_of(sectors_per_block) {
        return Err(libc::EIO);
    }
    let path = b"/dev/dvm-block0";
    let mut descriptor = StorageBlockDescriptorWire {
        id: 1,
        transport: STORAGE_TRANSPORT_DVM_BLOCK,
        flags: if info.flags & rustos_user_abi::syscall::BLOCK_BROKER_INFO_FLAG_READ_ONLY != 0 {
            STORAGE_FLAG_READONLY
        } else {
            0
        },
        logical_block_size: info.logical_block_size,
        start_block: 0,
        block_count: info.capacity_sectors / sectors_per_block,
        path_len: path.len() as u32,
        reserved0: 0,
        ..StorageBlockDescriptorWire::default()
    };
    descriptor.path[..path.len()].copy_from_slice(path);
    Ok(vec![descriptor])
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_STORAGED
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY
        | COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN
        | COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT
        | COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 == 0
                && request.arg1 == 0
                && request.arg2 == 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 == 0
                && request.arg1 == 0
                && request.arg2 == 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ
        | COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 != 0
                && request.arg2 != 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE
            if request.path_len == 0
                && request.payload_len != 0
                && request.arg0 != 0
                && request.arg2 != 0
                && request.arg3 & !COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA == 0 =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH
            if request.path_len == 0
                && request.payload_len == 0
                && request.arg0 != 0
                && request.arg1 == 0
                && request.arg2 == 0
                && request.arg3 == 0 =>
        {
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
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

fn storaged_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY => 1 << 0,
        COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN => 1 << 1,
        COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT => 1 << 2,
        COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA => 1 << 4,
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO => 1 << 7,
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ
        | COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK => 1 << 8,
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE => 1 << 9,
        COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH => 1 << 10,
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
    unsafe { rustos_svc_runtime::syscall::syscall0(number) }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall2(number, arg0, arg1) }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall3(number, arg0, arg1, arg2) }
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3, arg4, arg5) as i64 }
}

fn register_service_endpoint(service_id: u64, endpoint: u64) -> i64 {
    let mut last = 0;
    for _ in 0..65_536 {
        last = syscall2(
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
            service_id,
            endpoint,
        );
        if last >= 0 {
            return last;
        }
        let errno = (-last) as i32;
        if errno != libc::EACCES && errno != libc::EPERM && errno != libc::ENOENT {
            return last;
        }
        thread::yield_now();
    }
    last
}

fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        line.as_ptr() as u64,
        (len + 1) as u64,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u16) -> CommercialMaxProtocolRequest {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_STORAGED;
        request.header.op = op;
        request
    }

    #[test]
    fn commercial_storage_operations_reject_unconsumed_fields() {
        let mut inventory = request(COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY);
        assert_eq!(validate_commercial_request(&inventory), Ok(()));
        inventory.path_len = 1;
        inventory.path[0] = b'x';
        assert_eq!(validate_commercial_request(&inventory), Err(libc::EINVAL));

        let mut info = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO);
        info.arg0 = 1;
        assert_eq!(validate_commercial_request(&info), Err(libc::EINVAL));

        let mut bulk = request(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK);
        bulk.arg0 = 7;
        bulk.arg1 = 11;
        bulk.arg2 = 15;
        assert_eq!(validate_commercial_request(&bulk), Ok(()));
        bulk.payload_len = 1;
        assert_eq!(validate_commercial_request(&bulk), Err(libc::EINVAL));
    }

    #[test]
    fn bulk_read_reuses_read_authority_instead_of_minting_a_new_right() {
        assert_eq!(
            storaged_capability_mask(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK),
            storaged_capability_mask(COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ)
        );
    }

    #[test]
    fn dvm_block_e2e_marker_names_the_complete_authority_path() {
        assert_eq!(
            dvm_block_e2e_marker(7),
            "storaged: dvm-block e2e flush completed generation=7 \
             path=vfs-policy->block-broker->shared-ring->linux-dvm->backing"
        );
    }
}
