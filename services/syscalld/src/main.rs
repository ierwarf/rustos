#![no_std]
#![no_main]

extern crate alloc;

use core::mem::size_of;

use rustos_svc_runtime::ipc;
use rustos_user_abi::syscall::{
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, Win32SyscallOffloadRequest,
    Win32SyscallOffloadResponse, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_BRK,
    SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME, SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP,
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
    SYSCALL_OFFLOAD_PATH_CAPACITY, WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
};

mod errno;
mod linux_policy;
mod win32_policy;

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
        let mut request = [0_u8; size_of::<LinuxSyscallOffloadRequest>()];
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
            errno::sleep_millis(10);
            continue;
        }

        let response = handle_request(received as usize, &request);
        let reply = unsafe { ipc::reply(reply_cap, response.as_ptr(), response.len()) };
        if reply < 0 {
            ipc::debug_line("syscalld: reply failed");
        }
    }
}

enum SyscallOffloadReply {
    Linux(LinuxSyscallOffloadResponse),
    Win32(Win32SyscallOffloadResponse),
}

impl SyscallOffloadReply {
    fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Linux(response) => (response as *const LinuxSyscallOffloadResponse).cast::<u8>(),
            Self::Win32(response) => (response as *const Win32SyscallOffloadResponse).cast::<u8>(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Linux(_) => size_of::<LinuxSyscallOffloadResponse>(),
            Self::Win32(_) => size_of::<Win32SyscallOffloadResponse>(),
        }
    }
}

fn handle_request(received: usize, bytes: &[u8]) -> SyscallOffloadReply {
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
        | SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME
        | SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP
        | SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST
        | SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST
        | SYSCALL_OFFLOAD_OP_LINUX_RSEQ
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
