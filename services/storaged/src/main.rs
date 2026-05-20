use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    BootExtentLeaseWire, RustosBootExtentBrokerArgs, StorageBlockDescriptorWire,
    StorageListBrokerArgs, StoragedRequest, StoragedResponse, BOOT_EXTENT_FLAG_READONLY,
    BOOT_EXTENT_PATH_CAPACITY, IPC_SERVICE_STORAGED, STORAGED_IPC_ABI_VERSION,
    STORAGED_OP_BOOT_EXTENT_LOOKUP, STORAGED_OP_LIST_COUNT, STORAGED_OP_LIST_GET, STORAGED_OP_PING,
    STORAGED_OP_ROOT_STATUS, STORAGE_LIST_MAX_DESCRIPTORS, SYS_RUSTOS_BOOT_EXTENT_BROKER,
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_STORAGE_LIST_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);

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
        let mut request = StoragedRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut StoragedRequest) as u64,
            size_of::<StoragedRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
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

fn selected_root_descriptor() -> Result<StorageBlockDescriptorWire, i32> {
    let descriptors = list_descriptors()?;
    descriptors.first().copied().ok_or(libc::ENODEV)
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
    let args = RustosBootExtentBrokerArgs {
        abi_version: STORAGED_IPC_ABI_VERSION,
        flags: 0,
        reserved0: 0,
        path_ptr: path.as_ptr() as u64,
        path_len: len as u64,
        out_lease_ptr: (&mut lease as *mut BootExtentLeaseWire) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_BOOT_EXTENT_BROKER,
        (&args as *const RustosBootExtentBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    Ok(lease)
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
