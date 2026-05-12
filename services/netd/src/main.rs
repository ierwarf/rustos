use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, RustosNetBrokerArgs, IPC_SERVICE_NETD,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_LISTEN, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
    SYSCALL_OFFLOAD_OP_LINUX_RECVMSG, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, SYSCALL_OFFLOAD_PATH_CAPACITY, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_NET_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(10);

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_NETD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("netd: network policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = LinuxSyscallOffloadRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut LinuxSyscallOffloadRequest) as u64,
            size_of::<LinuxSyscallOffloadRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        let mut response = LinuxSyscallOffloadResponse {
            op: request.op,
            ..LinuxSyscallOffloadResponse::default()
        };
        response.status = match validate_request(received as usize, &request) {
            Ok(()) => dispatch_request(&request, &mut response),
            Err(errno) => errno,
        };
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const LinuxSyscallOffloadResponse) as u64,
            size_of::<LinuxSyscallOffloadResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "netd: reply failed errno={}", -reply);
        }
    }
}

fn dispatch_request(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) -> i32 {
    let args = RustosNetBrokerArgs {
        process_id: request.pid,
        op: request.op,
        reserved0: 0,
        reserved1: 0,
        arg0: request.dirfd,
        arg1: request.flags,
        arg2: request.arg0,
        arg3: request.arg1,
        arg4: request.arg2,
        arg5: request.arg3,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        return (-result) as i32;
    }
    let bytes = (result as u64).to_le_bytes();
    response.payload[..bytes.len()].copy_from_slice(&bytes);
    response.payload_len = bytes.len() as u32;
    0
}

fn validate_request(received: usize, request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if received != size_of::<LinuxSyscallOffloadRequest>()
        || request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => Ok(()),
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
