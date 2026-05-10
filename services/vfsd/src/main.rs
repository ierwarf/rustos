use std::ffi::CString;
use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    IpcReplyWithHandlesArgs, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse,
    RustosVfsMountBrokerArgs, IPC_SERVICE_VFSD, LINUX_STATX_SIZE, LINUX_STAT_SIZE,
    SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_ACCESS, SYSCALL_OFFLOAD_OP_LINUX_CHDIR,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_DUP, SYSCALL_OFFLOAD_OP_LINUX_FCNTL,
    SYSCALL_OFFLOAD_OP_LINUX_GETCWD, SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64,
    SYSCALL_OFFLOAD_OP_LINUX_MKDIR, SYSCALL_OFFLOAD_OP_LINUX_MOUNT,
    SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT, SYSCALL_OFFLOAD_OP_LINUX_OPENAT,
    SYSCALL_OFFLOAD_OP_LINUX_READLINKAT, SYSCALL_OFFLOAD_OP_LINUX_STATX,
    SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2, SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT,
    SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, SYS_RUSTOS_ACCESS_METADATA,
    SYS_RUSTOS_CHDIR_METADATA, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_FD_CLOSE_BROKER,
    SYS_RUSTOS_FD_DUP_BROKER, SYS_RUSTOS_FD_FCNTL_BROKER, SYS_RUSTOS_FD_GETDENTS64_BROKER,
    SYS_RUSTOS_GETCWD_METADATA, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_IPC_REPLY_WITH_HANDLES, SYS_RUSTOS_READLINK_METADATA, SYS_RUSTOS_STATX_METADATA,
    SYS_RUSTOS_STAT_METADATA, SYS_RUSTOS_VFS_MOUNT_BROKER, SYS_RUSTOS_VFS_UMOUNT_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(10);
const DUP_MODE_DUP: u64 = 0;
const DUP_MODE_DUP2: u64 = 1;
const DUP_MODE_DUP3: u64 = 2;

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "vfsd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_VFSD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "vfsd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("vfsd: vfs policy endpoint registered");
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
        let reply_fd = handle_request(received as usize, &request, &mut response);
        let reply = reply_response(reply_cap, &response, reply_fd);
        if let Some(fd) = reply_fd {
            let _ = unsafe { libc::close(fd) };
        }
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "vfsd: reply failed errno={}", -reply);
        }
    }
}

fn handle_request(
    received: usize,
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) -> Option<i32> {
    response.op = request.op;
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
        return None;
    }

    let reply_fd = match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_STATX => {
            handle_statx(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT => {
            handle_newfstatat(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_READLINKAT => {
            handle_readlinkat(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_ACCESS => {
            handle_access(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETCWD => {
            handle_getcwd(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_CHDIR => {
            handle_chdir(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_MKDIR => {
            handle_mkdir(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_OPENAT => handle_openat(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE => {
            handle_close(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_DUP => {
            handle_dup(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64 => {
            handle_getdents64(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_FCNTL => {
            handle_fcntl(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_MOUNT => {
            handle_mount(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2 => {
            handle_umount2(request, response);
            None
        }
        SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT => {
            handle_policy_allow(response);
            None
        }
        _ => {
            response.status = libc::EINVAL;
            None
        }
    };
    reply_fd
}

fn handle_statx(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let mut statx = [0_u8; LINUX_STATX_SIZE];
    let status = syscall4(
        SYS_RUSTOS_STATX_METADATA,
        request.path.as_ptr() as u64,
        request.path_len as u64,
        request.mask as u64,
        statx.as_mut_ptr() as u64,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }

    response.status = 0;
    response.payload_len = LINUX_STATX_SIZE as u32;
    response.payload[..LINUX_STATX_SIZE].copy_from_slice(&statx);
}

fn handle_newfstatat(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let mut stat = [0_u8; LINUX_STAT_SIZE];
    let status = syscall3(
        SYS_RUSTOS_STAT_METADATA,
        request.path.as_ptr() as u64,
        request.path_len as u64,
        stat.as_mut_ptr() as u64,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }

    response.status = 0;
    response.payload_len = LINUX_STAT_SIZE as u32;
    response.payload[..LINUX_STAT_SIZE].copy_from_slice(&stat);
}

fn handle_readlinkat(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let out_len = (request.mask as usize).min(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
    let status = syscall4(
        SYS_RUSTOS_READLINK_METADATA,
        request.path.as_ptr() as u64,
        request.path_len as u64,
        response.payload.as_mut_ptr() as u64,
        out_len as u64,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }

    response.status = 0;
    response.payload_len = status as u32;
}

fn handle_access(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let status = syscall4(
        SYS_RUSTOS_ACCESS_METADATA,
        request.path.as_ptr() as u64,
        request.path_len as u64,
        request.mask as u64,
        request.pid,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }

    response.status = 0;
    response.payload_len = 0;
}

fn handle_getcwd(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let status = syscall3(
        SYS_RUSTOS_GETCWD_METADATA,
        request.pid,
        response.payload.as_mut_ptr() as u64,
        SYSCALL_OFFLOAD_PAYLOAD_CAPACITY as u64,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }

    response.status = 0;
    response.payload_len = status as u32;
}

fn handle_chdir(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let status = syscall3(
        SYS_RUSTOS_CHDIR_METADATA,
        request.path.as_ptr() as u64,
        request.path_len as u64,
        request.pid,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }

    response.status = 0;
    response.payload_len = 0;
}

fn handle_mkdir(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let Some(path) = request_path(request) else {
        response.status = libc::EINVAL;
        return;
    };
    if path == "/run" || path == "/run/user" || path == format!("/run/user/{}", request.euid) {
        response.status = 0;
        response.payload_len = 0;
    } else {
        response.status = libc::EROFS;
    }
}

fn handle_openat(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) -> Option<i32> {
    let Some(path) = request_path(request) else {
        response.status = libc::EINVAL;
        return None;
    };
    let Ok(path) = CString::new(path) else {
        response.status = libc::EINVAL;
        return None;
    };

    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat as libc::c_long,
            libc::AT_FDCWD,
            path.as_ptr(),
            request.flags as libc::c_long,
            request.mask as libc::c_long,
        ) as i64
    };
    if fd < 0 {
        response.status = (-fd) as i32;
        return None;
    }
    let Ok(fd) = i32::try_from(fd) else {
        response.status = libc::EOVERFLOW;
        return None;
    };
    response.status = 0;
    response.payload_len = 0;
    Some(fd)
}

fn handle_close(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let status = syscall2(SYS_RUSTOS_FD_CLOSE_BROKER, request.pid, request.dirfd);
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

fn handle_dup(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let mode = u64::from(request.mask);
    let flags = request.arg1;
    if !matches!(mode, DUP_MODE_DUP | DUP_MODE_DUP2 | DUP_MODE_DUP3) {
        response.status = libc::EINVAL;
        return;
    }
    let fd = syscall5(
        SYS_RUSTOS_FD_DUP_BROKER,
        request.pid,
        request.dirfd,
        request.arg0,
        flags,
        mode,
    );
    if fd < 0 {
        response.status = (-fd) as i32;
        return;
    }
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&(fd as u64).to_ne_bytes());
}

fn handle_getdents64(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let read = syscall4(
        SYS_RUSTOS_FD_GETDENTS64_BROKER,
        request.pid,
        request.dirfd,
        request.arg0,
        request.arg1,
    );
    if read < 0 {
        response.status = (-read) as i32;
        return;
    }
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&(read as u64).to_ne_bytes());
}

fn handle_fcntl(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let result = syscall4(
        SYS_RUSTOS_FD_FCNTL_BROKER,
        request.pid,
        request.dirfd,
        request.arg0,
        request.arg1,
    );
    if result < 0 {
        response.status = (-result) as i32;
        return;
    }
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&(result as u64).to_ne_bytes());
}

fn handle_mount(request: &LinuxSyscallOffloadRequest, response: &mut LinuxSyscallOffloadResponse) {
    let args = RustosVfsMountBrokerArgs {
        process_id: request.pid,
        source_ptr: request.arg0,
        target_path_ptr: request.path.as_ptr() as u64,
        target_path_len: request.path_len as u64,
        fstype_ptr: request.arg1,
        flags: request.flags,
        data_ptr: request.dirfd,
        reserved0: 0,
    };
    let status = syscall1(
        SYS_RUSTOS_VFS_MOUNT_BROKER,
        (&args as *const RustosVfsMountBrokerArgs) as u64,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

fn handle_umount2(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let status = syscall4(
        SYS_RUSTOS_VFS_UMOUNT_BROKER,
        request.pid,
        request.path.as_ptr() as u64,
        request.path_len as u64,
        request.flags,
    );
    if status < 0 {
        response.status = (-status) as i32;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

fn reply_response(
    reply_cap: u64,
    response: &LinuxSyscallOffloadResponse,
    reply_fd: Option<i32>,
) -> i64 {
    let Some(fd) = reply_fd else {
        return syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (response as *const LinuxSyscallOffloadResponse) as u64,
            size_of::<LinuxSyscallOffloadResponse>() as u64,
        );
    };

    let args = IpcReplyWithHandlesArgs {
        reply_cap,
        response_ptr: (response as *const LinuxSyscallOffloadResponse) as u64,
        response_len: size_of::<LinuxSyscallOffloadResponse>() as u64,
        send_fds_ptr: (&fd as *const i32) as u64,
        send_fd_count: 1,
        ..IpcReplyWithHandlesArgs::default()
    };
    syscall1(
        SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
        (&args as *const IpcReplyWithHandlesArgs) as u64,
    )
}

fn validate_request(received: usize, request: &LinuxSyscallOffloadRequest) -> Result<(), i32> {
    if received != size_of::<LinuxSyscallOffloadRequest>()
        || request.version != SYSCALL_OFFLOAD_ABI_VERSION
        || request.reserved0 != 0
        || request.path_len as usize > SYSCALL_OFFLOAD_PATH_CAPACITY
    {
        return Err(libc::EINVAL);
    }
    if request.op != SYSCALL_OFFLOAD_OP_LINUX_GETCWD
        && !is_handle_policy_op(request.op)
        && request.path_len == 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_STATX
        | SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT
        | SYSCALL_OFFLOAD_OP_LINUX_READLINKAT
        | SYSCALL_OFFLOAD_OP_LINUX_ACCESS
        | SYSCALL_OFFLOAD_OP_LINUX_GETCWD
        | SYSCALL_OFFLOAD_OP_LINUX_CHDIR
        | SYSCALL_OFFLOAD_OP_LINUX_MKDIR
        | SYSCALL_OFFLOAD_OP_LINUX_OPENAT
        | SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_FCNTL
        | SYSCALL_OFFLOAD_OP_LINUX_MOUNT
        | SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2
        | SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn handle_policy_allow(response: &mut LinuxSyscallOffloadResponse) {
    response.status = 0;
    response.payload_len = 0;
}

fn is_handle_policy_op(op: u16) -> bool {
    matches!(
        op,
        SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64
            | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
            | SYSCALL_OFFLOAD_OP_LINUX_DUP
            | SYSCALL_OFFLOAD_OP_LINUX_FCNTL
    )
}

fn request_path(request: &LinuxSyscallOffloadRequest) -> Option<&str> {
    let len = request.path_len as usize;
    if len == 0 || len > request.path.len() {
        return None;
    }
    std::str::from_utf8(&request.path[..len]).ok()
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

fn syscall5(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3, arg4) as i64 }
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_vfs_requests_are_rejected() {
        let mut request = LinuxSyscallOffloadRequest {
            path_len: 1,
            ..LinuxSyscallOffloadRequest::default()
        };
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Ok(())
        );
        request.op = SYSCALL_OFFLOAD_OP_LINUX_GETUID;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(libc::EINVAL)
        );

        request.op = SYSCALL_OFFLOAD_OP_LINUX_GETCWD;
        request.path_len = 0;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Ok(())
        );

        request.op = SYSCALL_OFFLOAD_OP_LINUX_STATX;
        request.path_len = 0;
        assert_eq!(
            validate_request(size_of::<LinuxSyscallOffloadRequest>(), &request),
            Err(libc::EINVAL)
        );
    }
}
