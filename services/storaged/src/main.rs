use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    StorageBlockDescriptorWire, StorageListBrokerArgs, IPC_SERVICE_STORAGED,
    STORAGE_LIST_MAX_DESCRIPTORS, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_STORAGE_LIST_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const STORAGED_ABI_VERSION: u16 = 1;
const STORAGED_OP_PING: u16 = 1;
const STORAGED_OP_LIST_COUNT: u16 = 2;
const STORAGED_OP_LIST_GET: u16 = 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StoragedRequest {
    version: u16,
    op: u16,
    flags: u32,
    arg0: u64,
    arg1: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StoragedResponse {
    version: u16,
    op: u16,
    status: i32,
    reserved0: u32,
    value: u64,
    payload: StorageBlockDescriptorWire,
}

impl Default for StoragedResponse {
    fn default() -> Self {
        Self {
            version: 0,
            op: 0,
            status: 0,
            reserved0: 0,
            value: 0,
            payload: StorageBlockDescriptorWire::default(),
        }
    }
}

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
            version: STORAGED_ABI_VERSION,
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
        _ => libc::EINVAL,
    }
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
    if received != size_of::<StoragedRequest>() || request.version != STORAGED_ABI_VERSION {
        return Err(libc::EINVAL);
    }
    match request.op {
        STORAGED_OP_PING => Ok(()),
        STORAGED_OP_LIST_COUNT => Ok(()),
        STORAGED_OP_LIST_GET => Ok(()),
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
