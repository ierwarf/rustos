use super::*;
use rustos_user_abi::syscall::{
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
};

pub(super) fn syscall_linux_socket(domain: u64, type_: u64, protocol: u64) -> u64 {
    if let Err(errno) = net_policy(
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        domain,
        type_,
        u32::try_from(protocol).unwrap_or(u32::MAX),
    ) {
        return linux_errno(errno);
    }
    match linux_ops::socket(domain, type_, protocol) {
        Ok(fd) => fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_sendto(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    flags: u64,
    addr_ptr: u64,
    addr_len: u64,
) -> u64 {
    match linux_ops::sendto(fd, user_ptr, user_len, flags, addr_ptr, addr_len) {
        Ok(written) => written as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_socketpair(domain: u64, type_: u64, protocol: u64, sv_ptr: u64) -> u64 {
    if let Err(errno) = net_policy(
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
        domain,
        type_,
        u32::try_from(protocol).unwrap_or(u32::MAX),
    ) {
        return linux_errno(errno);
    }
    match linux_ops::socketpair(domain, type_, protocol, sv_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_bind(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
    if let Err(errno) = net_policy(
        SYSCALL_OFFLOAD_OP_LINUX_BIND,
        fd,
        addr_len,
        u32::try_from(addr_ptr).unwrap_or(u32::MAX),
    ) {
        return linux_errno(errno);
    }
    match linux_ops::bind(fd, addr_ptr, addr_len) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_listen(fd: u64, backlog: u64) -> u64 {
    if let Err(errno) = net_policy(SYSCALL_OFFLOAD_OP_LINUX_LISTEN, fd, backlog, 0) {
        return linux_errno(errno);
    }
    match linux_ops::listen(fd, backlog) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_accept(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    if let Err(errno) = net_policy(SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, fd, 0, 0) {
        return linux_errno(errno);
    }
    match linux_ops::accept(fd, addr_ptr, addr_len_ptr) {
        Ok(new_fd) => new_fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_accept4(fd: u64, addr_ptr: u64, addr_len_ptr: u64, flags: u64) -> u64 {
    if let Err(errno) = net_policy(SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, fd, flags, 0) {
        return linux_errno(errno);
    }
    match linux_ops::accept4(fd, addr_ptr, addr_len_ptr, flags) {
        Ok(new_fd) => new_fd,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_connect(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
    if let Err(errno) = net_policy(
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT,
        fd,
        addr_len,
        u32::try_from(addr_ptr).unwrap_or(u32::MAX),
    ) {
        return linux_errno(errno);
    }
    match linux_ops::connect(fd, addr_ptr, addr_len) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

fn net_policy(op: u16, dirfd: u64, flags: u64, arg0: u32) -> Result<(), i64> {
    offload_ops::call_service_policy(linux_abi::IPC_SERVICE_NETD, op, dirfd, flags, arg0)
}

pub(super) fn syscall_linux_getsockname(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    match linux_ops::getsockname(fd, addr_ptr, addr_len_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_getpeername(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
    match linux_ops::getpeername(fd, addr_ptr, addr_len_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_setsockopt(
    fd: u64,
    level: u64,
    optname: u64,
    optval_ptr: u64,
    optlen: u64,
) -> u64 {
    match linux_ops::setsockopt(fd, level, optname, optval_ptr, optlen) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_getsockopt(
    fd: u64,
    level: u64,
    optname: u64,
    optval_ptr: u64,
    optlen_ptr: u64,
) -> u64 {
    match linux_ops::getsockopt(fd, level, optname, optval_ptr, optlen_ptr) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_shutdown(fd: u64, how: u64) -> u64 {
    match linux_ops::shutdown(fd, how) {
        Ok(()) => 0,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_sendmsg(fd: u64, msghdr_ptr: u64, flags: u64) -> u64 {
    match linux_ops::sendmsg(fd, msghdr_ptr, flags) {
        Ok(written) => written as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_recvmsg(fd: u64, msghdr_ptr: u64, flags: u64) -> u64 {
    match linux_ops::recvmsg(fd, msghdr_ptr, flags) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}

pub(super) fn syscall_linux_recvfrom(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    flags: u64,
    addr_ptr: u64,
    addr_len_ptr: u64,
) -> u64 {
    match linux_ops::recvfrom(fd, user_ptr, user_len, flags, addr_ptr, addr_len_ptr) {
        Ok(read) => read as u64,
        Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
    }
}
