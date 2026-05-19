use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    InputStatsBrokerArgs, InputStatsWire, InputdIpcRequest, InputdIpcResponse, INPUTD_ACCESS_EVDEV,
    INPUTD_ACCESS_NATIVE, INPUTD_IPC_ABI_VERSION, INPUTD_IPC_OP_AUTHORIZE_READ, INPUTD_IPC_OP_PING,
    INPUTD_IPC_OP_STATS, INPUTD_READ_FLAG_NONBLOCK, IPC_SERVICE_INPUTD, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_INPUT_STATS_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const INPUTD_MAX_NATIVE_READ_BYTES: u64 = 1024 * 24;
const INPUTD_MAX_EVDEV_READ_BYTES: u64 = 1024 * 24 * 3;

fn main() {
    observability_client::info!("inputd", service, "service started");
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_INPUTD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("inputd: input policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = InputdIpcRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut InputdIpcRequest) as u64,
            size_of::<InputdIpcRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        let mut response = InputdIpcResponse {
            version: INPUTD_IPC_ABI_VERSION,
            op: request.op,
            ..InputdIpcResponse::default()
        };
        response.status = match validate(received as usize, &request) {
            Ok(()) => dispatch(&request, &mut response),
            Err(errno) => errno,
        };
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const InputdIpcResponse) as u64,
            size_of::<InputdIpcResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
        }
    }
}

fn dispatch(request: &InputdIpcRequest, response: &mut InputdIpcResponse) -> i32 {
    match request.op {
        INPUTD_IPC_OP_PING => {
            response.approved_len = request.requested_len;
            0
        }
        INPUTD_IPC_OP_STATS => match fetch_stats() {
            Ok(stats) => {
                response.stats = stats;
                0
            }
            Err(errno) => errno,
        },
        INPUTD_IPC_OP_AUTHORIZE_READ => authorize_read(request, response),
        _ => libc::EINVAL,
    }
}

fn authorize_read(request: &InputdIpcRequest, response: &mut InputdIpcResponse) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    let max_len = match request.access {
        INPUTD_ACCESS_NATIVE => INPUTD_MAX_NATIVE_READ_BYTES,
        INPUTD_ACCESS_EVDEV => INPUTD_MAX_EVDEV_READ_BYTES,
        _ => return libc::EINVAL,
    };
    response.approved_len = request.requested_len.min(max_len);
    match fetch_stats() {
        Ok(stats) => response.stats = stats,
        Err(errno) => return errno,
    }
    0
}

fn fetch_stats() -> Result<InputStatsWire, i32> {
    let mut stats = InputStatsWire::default();
    let args = InputStatsBrokerArgs {
        abi_version: 1,
        reserved0: 0,
        reserved1: 0,
        out_stats_ptr: (&mut stats as *mut InputStatsWire) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_INPUT_STATS_BROKER,
        (&args as *const InputStatsBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    Ok(stats)
}

fn validate(received: usize, request: &InputdIpcRequest) -> Result<(), i32> {
    if received != size_of::<InputdIpcRequest>()
        || request.version != INPUTD_IPC_ABI_VERSION
        || request.flags & !INPUTD_READ_FLAG_NONBLOCK != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        INPUTD_IPC_OP_PING => Ok(()),
        INPUTD_IPC_OP_STATS => Ok(()),
        INPUTD_IPC_OP_AUTHORIZE_READ => Ok(()),
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
