use std::mem::size_of;

use rustos_user_abi::syscall::{
    LinuxRlimit, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxUtsName,
    LINUX_CPUSET_BYTES, LINUX_DEFAULT_STACK_RLIMIT_BYTES, LINUX_RLIMIT_SIZE, LINUX_UTSNAME_SIZE,
    SYSCALL_OFFLOAD_PAYLOAD_CAPACITY,
};

pub(crate) fn handle_uname(response: &mut LinuxSyscallOffloadResponse) {
    let mut uts = LinuxUtsName::default();
    write_uts_field(&mut uts.sysname, b"RustOS");
    write_uts_field(&mut uts.nodename, b"rustos");
    write_uts_field(&mut uts.release, b"0.1");
    write_uts_field(&mut uts.version, b"RustOS 0.1");
    write_uts_field(&mut uts.machine, b"x86_64");
    write_uts_field(&mut uts.domainname, b"localdomain");
    copy_payload(response, &uts);
}

pub(crate) fn handle_prlimit64(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = request.dirfd;
    let resource = request.flags;
    let has_new_limit = request.mask & 0x1 != 0;
    let wants_old_limit = request.mask & 0x2 != 0;
    if target_pid != 0 && target_pid != request.pid {
        response.status = libc::EACCES;
        return;
    }
    if resource != libc::RLIMIT_STACK as u64 {
        response.status = libc::EINVAL;
        return;
    }
    if has_new_limit && request.path_len as usize != LINUX_RLIMIT_SIZE {
        response.status = libc::EINVAL;
        return;
    }
    if wants_old_limit {
        let current = LinuxRlimit {
            rlim_cur: LINUX_DEFAULT_STACK_RLIMIT_BYTES,
            rlim_max: LINUX_DEFAULT_STACK_RLIMIT_BYTES,
        };
        copy_payload(response, &current);
    } else {
        response.status = 0;
        response.payload_len = 0;
    }
}

pub(crate) fn handle_sched_getaffinity(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = request.dirfd;
    let requested_len = request.flags as usize;
    if requested_len == 0 {
        response.status = libc::EINVAL;
        return;
    }
    if target_pid != 0 && target_pid != request.pid {
        response.status = libc::ESRCH;
        return;
    }
    let payload_len = requested_len.min(LINUX_CPUSET_BYTES);
    response.payload.fill(0);
    response.payload[0] = 0x1;
    response.status = 0;
    response.payload_len = payload_len as u32;
}

pub(crate) fn handle_id(value: u32, response: &mut LinuxSyscallOffloadResponse) {
    response.status = 0;
    response.payload_len = size_of::<u32>() as u32;
    response.payload[..size_of::<u32>()].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn handle_setuid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let requested = request.mask;
    if request.euid != 0
        && request.uid != 0
        && requested != request.uid
        && requested != request.euid
    {
        response.status = libc::EACCES;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_setgid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let requested = request.mask;
    if request.euid != 0
        && request.uid != 0
        && requested != request.gid
        && requested != request.egid
    {
        response.status = libc::EACCES;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

fn write_uts_field(dest: &mut [u8; 65], value: &[u8]) {
    let len = value.len().min(dest.len().saturating_sub(1));
    dest[..len].copy_from_slice(&value[..len]);
    dest[len] = 0;
}

fn copy_payload<T>(response: &mut LinuxSyscallOffloadResponse, value: &T) {
    let len = size_of::<T>();
    debug_assert!(len <= SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
    let bytes = unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), len) };
    response.status = 0;
    response.payload_len = len as u32;
    response.payload[..len].copy_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_payload_fits_inline_response() {
        let mut response = LinuxSyscallOffloadResponse::default();
        handle_uname(&mut response);
        assert_eq!(response.status, 0);
        assert_eq!(response.payload_len as usize, LINUX_UTSNAME_SIZE);
    }

    #[test]
    fn setuid_policy_matches_linux_subset() {
        let mut response = LinuxSyscallOffloadResponse::default();
        let request = LinuxSyscallOffloadRequest {
            uid: 1000,
            euid: 1000,
            mask: 2000,
            ..LinuxSyscallOffloadRequest::default()
        };
        handle_setuid(&request, &mut response);
        assert_eq!(response.status, libc::EACCES);

        let request = LinuxSyscallOffloadRequest {
            uid: 0,
            euid: 1000,
            mask: 2000,
            ..LinuxSyscallOffloadRequest::default()
        };
        handle_setuid(&request, &mut response);
        assert_eq!(response.status, 0);
    }
}
