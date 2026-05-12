// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// use super::*;
// use rustos_user_abi::syscall::{
//     SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND,
//     SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
//     SYSCALL_OFFLOAD_OP_LINUX_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
// };
//
// pub(super) fn syscall_linux_socket(domain: u64, type_: u64, protocol: u64) -> u64 {
//     match net_broker(SYSCALL_OFFLOAD_OP_LINUX_SOCKET, domain, type_, protocol, 0) {
//         Ok(fd) => fd,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// pub(super) fn syscall_linux_sendto(
//     fd: u64,
//     user_ptr: u64,
//     user_len: u64,
//     flags: u64,
//     addr_ptr: u64,
//     addr_len: u64,
// ) -> u64 {
//     match linux_ops::sendto(fd, user_ptr, user_len, flags, addr_ptr, addr_len) {
//         Ok(written) => written as u64,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_socketpair(domain: u64, type_: u64, protocol: u64, sv_ptr: u64) -> u64 {
//     match net_broker(
//         SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
//         domain,
//         type_,
//         protocol,
//         sv_ptr,
//     ) {
//         Ok(value) => value,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// pub(super) fn syscall_linux_bind(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
//     match net_broker(SYSCALL_OFFLOAD_OP_LINUX_BIND, fd, addr_ptr, addr_len, 0) {
//         Ok(value) => value,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// pub(super) fn syscall_linux_listen(fd: u64, backlog: u64) -> u64 {
//     match net_broker(SYSCALL_OFFLOAD_OP_LINUX_LISTEN, fd, backlog, 0, 0) {
//         Ok(value) => value,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// pub(super) fn syscall_linux_accept(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
//     match net_broker(
//         SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
//         fd,
//         addr_ptr,
//         addr_len_ptr,
//         0,
//     ) {
//         Ok(new_fd) => new_fd,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// pub(super) fn syscall_linux_accept4(fd: u64, addr_ptr: u64, addr_len_ptr: u64, flags: u64) -> u64 {
//     match net_broker(
//         SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
//         fd,
//         addr_ptr,
//         addr_len_ptr,
//         flags,
//     ) {
//         Ok(new_fd) => new_fd,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// pub(super) fn syscall_linux_connect(fd: u64, addr_ptr: u64, addr_len: u64) -> u64 {
//     match net_broker(SYSCALL_OFFLOAD_OP_LINUX_CONNECT, fd, addr_ptr, addr_len, 0) {
//         Ok(value) => value,
//         Err(errno) => linux_errno(errno),
//     }
// }
//
// fn net_broker(op: u16, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> Result<u64, i64> {
//     offload_ops::call_net_broker(op, arg0, arg1, arg2, arg3)
// }
//
// pub(super) fn syscall_linux_getsockname(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
//     match linux_ops::getsockname(fd, addr_ptr, addr_len_ptr) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_getpeername(fd: u64, addr_ptr: u64, addr_len_ptr: u64) -> u64 {
//     match linux_ops::getpeername(fd, addr_ptr, addr_len_ptr) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_setsockopt(
//     fd: u64,
//     level: u64,
//     optname: u64,
//     optval_ptr: u64,
//     optlen: u64,
// ) -> u64 {
//     match linux_ops::setsockopt(fd, level, optname, optval_ptr, optlen) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_getsockopt(
//     fd: u64,
//     level: u64,
//     optname: u64,
//     optval_ptr: u64,
//     optlen_ptr: u64,
// ) -> u64 {
//     match linux_ops::getsockopt(fd, level, optname, optval_ptr, optlen_ptr) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_shutdown(fd: u64, how: u64) -> u64 {
//     match linux_ops::shutdown(fd, how) {
//         Ok(()) => 0,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_sendmsg(fd: u64, msghdr_ptr: u64, flags: u64) -> u64 {
//     match linux_ops::sendmsg(fd, msghdr_ptr, flags) {
//         Ok(written) => written as u64,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_recvmsg(fd: u64, msghdr_ptr: u64, flags: u64) -> u64 {
//     match linux_ops::recvmsg(fd, msghdr_ptr, flags) {
//         Ok(read) => read as u64,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
//
// pub(super) fn syscall_linux_recvfrom(
//     fd: u64,
//     user_ptr: u64,
//     user_len: u64,
//     flags: u64,
//     addr_ptr: u64,
//     addr_len_ptr: u64,
// ) -> u64 {
//     match linux_ops::recvfrom(fd, user_ptr, user_len, flags, addr_ptr, addr_len_ptr) {
//         Ok(read) => read as u64,
//         Err(err) => linux_errno(linux_sysop_error_to_errno(err)),
//     }
// }
