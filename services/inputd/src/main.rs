use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    InputStatsBrokerArgs, InputStatsWire, IPC_SERVICE_INPUTD, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_INPUT_STATS_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const INPUTD_ABI_VERSION: u16 = 1;
const INPUTD_OP_PING: u16 = 1;
const INPUTD_OP_STATS: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputdRequest {
    version: u16,
    op: u16,
    flags: u32,
    arg0: u64,
    arg1: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputdResponse {
    version: u16,
    op: u16,
    status: i32,
    reserved0: u32,
    value: u64,
    payload: InputStatsWire,
}

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
        let mut request = InputdRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut InputdRequest) as u64,
            size_of::<InputdRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        let mut response = InputdResponse {
            version: INPUTD_ABI_VERSION,
            op: request.op,
            ..InputdResponse::default()
        };
        response.status = match validate(received as usize, &request) {
            Ok(()) => dispatch(&request, &mut response),
            Err(errno) => errno,
        };
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const InputdResponse) as u64,
            size_of::<InputdResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
        }
    }
}

fn dispatch(request: &InputdRequest, response: &mut InputdResponse) -> i32 {
    match request.op {
        INPUTD_OP_PING => {
            response.value = request.arg0;
            0
        }
        INPUTD_OP_STATS => match fetch_stats() {
            Ok(stats) => {
                response.payload = stats;
                0
            }
            Err(errno) => errno,
        },
        _ => libc::EINVAL,
    }
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
        return Err((-result) as i32);
    }
    Ok(stats)
}

fn validate(received: usize, request: &InputdRequest) -> Result<(), i32> {
    if received != size_of::<InputdRequest>() || request.version != INPUTD_ABI_VERSION {
        return Err(libc::EINVAL);
    }
    match request.op {
        INPUTD_OP_PING => Ok(()),
        INPUTD_OP_STATS => Ok(()),
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

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
