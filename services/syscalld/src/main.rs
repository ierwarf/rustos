#![no_std]
#![no_main]

extern crate alloc;

use core::mem::size_of;

use rustos_user_abi::syscall::{
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_BRK, SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME,
    SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP, SYSCALL_OFFLOAD_OP_LINUX_GETEGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETEUID, SYSCALL_OFFLOAD_OP_LINUX_GETGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETPGID, SYSCALL_OFFLOAD_OP_LINUX_GETPPID,
    SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM, SYSCALL_OFFLOAD_OP_LINUX_GETSID,
    SYSCALL_OFFLOAD_OP_LINUX_GETUID, SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST,
    SYSCALL_OFFLOAD_OP_LINUX_MADVISE, SYSCALL_OFFLOAD_OP_LINUX_MMAP,
    SYSCALL_OFFLOAD_OP_LINUX_MPROTECT, SYSCALL_OFFLOAD_OP_LINUX_MUNMAP,
    SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP, SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64,
    SYSCALL_OFFLOAD_OP_LINUX_RSEQ, SYSCALL_OFFLOAD_OP_LINUX_RT_SIGACTION,
    SYSCALL_OFFLOAD_OP_LINUX_RT_SIGPROCMASK, SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY,
    SYSCALL_OFFLOAD_OP_LINUX_SETGID, SYSCALL_OFFLOAD_OP_LINUX_SETPGID,
    SYSCALL_OFFLOAD_OP_LINUX_SETSID, SYSCALL_OFFLOAD_OP_LINUX_SETUID,
    SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST, SYSCALL_OFFLOAD_OP_LINUX_SIGALTSTACK,
    SYSCALL_OFFLOAD_OP_LINUX_UMASK, SYSCALL_OFFLOAD_OP_LINUX_UNAME, SYSCALL_OFFLOAD_PATH_CAPACITY,
};
use rustos_svc_runtime::ipc;

mod errno;
mod linux_policy;

rustos_svc_runtime::entry!(service_main);

fn service_main() {
    let endpoint = ipc::endpoint_create();
    if endpoint < 0 {
        ipc::debug_line("syscalld: endpoint create failed");
        return;
    }

    let register = ipc::register_linux_syscall_endpoint(endpoint as u64);
    if register < 0 {
        ipc::debug_line("syscalld: endpoint register failed");
        return;
    }

    ipc::debug_line("syscalld: linux syscall endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = LinuxSyscallOffloadRequest::default();
        let mut reply_cap = 0_u64;
        let received = unsafe {
            ipc::recv(
                endpoint,
                (&mut request as *mut LinuxSyscallOffloadRequest).cast::<u8>(),
                size_of::<LinuxSyscallOffloadRequest>(),
                &mut reply_cap as *mut u64,
            )
        };
        if received < 0 {
            // Brief back-off (raw nanosleep, ~10 ms) before retrying.
            errno::sleep_millis(10);
            continue;
        }

        let mut response = LinuxSyscallOffloadResponse::default();
        handle_request(received as usize, &request, &mut response);
        let reply = unsafe {
            ipc::reply(
                reply_cap,
                (&response as *const LinuxSyscallOffloadResponse).cast::<u8>(),
                size_of::<LinuxSyscallOffloadResponse>(),
            )
        };
        if reply < 0 {
            ipc::debug_line("syscalld: reply failed");
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
        SYSCALL_OFFLOAD_OP_LINUX_UMASK => linux_policy::handle_umask(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM => linux_policy::handle_getrandom(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPPID => linux_policy::handle_getppid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPGID => linux_policy::handle_getpgid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETPGID => linux_policy::handle_setpgid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETSID => linux_policy::handle_getsid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETSID => linux_policy::handle_setsid(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_RT_SIGACTION => {
            linux_policy::handle_rt_sigaction(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RT_SIGPROCMASK => {
            linux_policy::handle_rt_sigprocmask(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP => linux_policy::handle_nanosleep(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME => {
            linux_policy::handle_clock_gettime(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP => {
            linux_policy::handle_clock_nanosleep(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST => {
            linux_policy::handle_set_robust_list(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST => {
            linux_policy::handle_get_robust_list(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RSEQ => linux_policy::handle_rseq(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MADVISE => linux_policy::handle_madvise(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SIGALTSTACK => linux_policy::handle_sigaltstack(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_BRK => linux_policy::handle_brk(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MMAP => linux_policy::handle_mmap(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MPROTECT => linux_policy::handle_mprotect(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_MUNMAP => linux_policy::handle_munmap(request, response),
        _ => response.status = errno::EINVAL,
    }
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
        | SYSCALL_OFFLOAD_OP_LINUX_RT_SIGACTION
        | SYSCALL_OFFLOAD_OP_LINUX_RT_SIGPROCMASK
        | SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST
        | SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST
        | SYSCALL_OFFLOAD_OP_LINUX_RSEQ
        | SYSCALL_OFFLOAD_OP_LINUX_MADVISE
        | SYSCALL_OFFLOAD_OP_LINUX_SIGALTSTACK
        | SYSCALL_OFFLOAD_OP_LINUX_BRK
        | SYSCALL_OFFLOAD_OP_LINUX_MMAP
        | SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
        | SYSCALL_OFFLOAD_OP_LINUX_MUNMAP => Ok(()),
        _ => Err(errno::EINVAL),
    }
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
