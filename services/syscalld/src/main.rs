use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_GETEGID, SYSCALL_OFFLOAD_OP_LINUX_GETEUID,
    SYSCALL_OFFLOAD_OP_LINUX_GETGID, SYSCALL_OFFLOAD_OP_LINUX_GETUID,
    SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64, SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY,
    SYSCALL_OFFLOAD_OP_LINUX_SETGID, SYSCALL_OFFLOAD_OP_LINUX_SETUID,
    SYSCALL_OFFLOAD_OP_LINUX_UNAME, SYSCALL_OFFLOAD_PATH_CAPACITY, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

mod linux_policy;

const RECV_BACKOFF: Duration = Duration::from_millis(10);

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "syscalld: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register = syscall1(
        SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "syscalld: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("syscalld: linux syscall endpoint registered");
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

        let mut response = LinuxSyscallOffloadResponse::default();
        handle_request(received as usize, &request, &mut response);
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const LinuxSyscallOffloadResponse) as u64,
            size_of::<LinuxSyscallOffloadResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "syscalld: reply failed errno={}", -reply);
        }
    }
}

fn handle_request(
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
        _ => response.status = libc::EINVAL,
    }
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
        SYSCALL_OFFLOAD_OP_LINUX_UNAME
        | SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64
        | SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY
        | SYSCALL_OFFLOAD_OP_LINUX_GETUID
        | SYSCALL_OFFLOAD_OP_LINUX_GETGID
        | SYSCALL_OFFLOAD_OP_LINUX_GETEUID
        | SYSCALL_OFFLOAD_OP_LINUX_GETEGID
        | SYSCALL_OFFLOAD_OP_LINUX_SETUID
        | SYSCALL_OFFLOAD_OP_LINUX_SETGID => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
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

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
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
            Err(libc::EINVAL)
        );

        request.version = 99;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(libc::EINVAL)
        );
        request.version = SYSCALL_OFFLOAD_ABI_VERSION;

        request.op = 99;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(libc::EINVAL)
        );
        request.op = SYSCALL_OFFLOAD_OP_LINUX_UNAME;

        request.reserved0 = 1;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(libc::EINVAL)
        );
        request.reserved0 = 0;

        request.path_len = (SYSCALL_OFFLOAD_PATH_CAPACITY + 1) as u32;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(libc::EINVAL)
        );
    }
}
