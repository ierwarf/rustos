//! Windows Winsock AFD (`\Device\Afd`) and NSI (`\??\Nsi`) kernel adapters.
//!
//! Routes `ws2_32.dll`'s `NtDeviceIoControlFile(IOCTL_AFD_*)` directly to RustOS
//! `netd` (`syscall_linux_net6_with_timeout` / `poll_netd_socket_batch`), and
//! handles `nsi.dll`/`iphlpapi.dll` loopback & interface queries without `wineserver`
//! or `nsiproxy.sys`.

use alloc::vec;
use alloc::vec::Vec;
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_LISTEN, SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN, SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
};
use rustos_user_abi::windows::{
    STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED, STATUS_NO_MEMORY,
    STATUS_PENDING, STATUS_SUCCESS, WIN32_AFD_HANDLE_BASE, WIN32_NSI_HANDLE_BASE,
};

use super::super::SyscallFrame;
use super::super::linux::error_ops::{
    LINUX_EAGAIN, LINUX_EINPROGRESS, LINUX_ENOTCONN,
};
use super::super::linux::service_ops::{vfs_meta, vfs_socket};
use super::nt_dispatch::{
    linux_errno_to_ntstatus, read_stack_arg, write_io_status, write_user_u32_checked,
    write_user_u64,
};
use super::sync;
use crate::multitask;
use crate::user::process_state::{Win32AfdSocketObject, Win32NsiHandleObject};
use crate::user::sysops::usermem;

const IOCTL_AFD_BIND: u32 = 0x0001_2003;
const IOCTL_AFD_LISTEN: u32 = 0x0001_200b;
const IOCTL_AFD_POLL: u32 = 0x0001_2024;
const IOCTL_AFD_GETSOCKNAME: u32 = 0x0001_202f;
const IOCTL_AFD_EVENT_SELECT: u32 = 0x0001_2087;
const IOCTL_AFD_GET_EVENTS: u32 = 0x0001_208b;

const fn wine_afd_ioc(func: u32) -> u32 {
    (0x0012 << 16) | (func << 2)
}

const IOCTL_AFD_WINE_CREATE: u32 = wine_afd_ioc(200);
const IOCTL_AFD_WINE_ACCEPT: u32 = wine_afd_ioc(201);
const IOCTL_AFD_WINE_ACCEPT_INTO: u32 = wine_afd_ioc(202);
const IOCTL_AFD_WINE_CONNECT: u32 = wine_afd_ioc(203);
const IOCTL_AFD_WINE_SHUTDOWN: u32 = wine_afd_ioc(204);
const IOCTL_AFD_WINE_RECVMSG: u32 = wine_afd_ioc(205);
const IOCTL_AFD_WINE_SENDMSG: u32 = wine_afd_ioc(206);
const IOCTL_AFD_WINE_FIONBIO: u32 = wine_afd_ioc(209);
const IOCTL_AFD_WINE_COMPLETE_ASYNC: u32 = wine_afd_ioc(210);
const IOCTL_AFD_WINE_FIONREAD: u32 = wine_afd_ioc(211);
const IOCTL_AFD_WINE_GETPEERNAME: u32 = wine_afd_ioc(216);
const IOCTL_AFD_WINE_GET_INFO: u32 = wine_afd_ioc(218);
const IOCTL_AFD_WINE_GET_SO_ACCEPTCONN: u32 = wine_afd_ioc(219);
const IOCTL_AFD_WINE_GET_SO_BROADCAST: u32 = wine_afd_ioc(220);
const IOCTL_AFD_WINE_SET_SO_BROADCAST: u32 = wine_afd_ioc(221);
const IOCTL_AFD_WINE_GET_SO_ERROR: u32 = wine_afd_ioc(222);
const IOCTL_AFD_WINE_GET_SO_KEEPALIVE: u32 = wine_afd_ioc(223);
const IOCTL_AFD_WINE_SET_SO_KEEPALIVE: u32 = wine_afd_ioc(224);
const IOCTL_AFD_WINE_SET_SO_RCVBUF: u32 = wine_afd_ioc(229);
const IOCTL_AFD_WINE_GET_SO_RCVBUF: u32 = wine_afd_ioc(230);
const IOCTL_AFD_WINE_GET_SO_REUSEADDR: u32 = wine_afd_ioc(233);
const IOCTL_AFD_WINE_SET_SO_REUSEADDR: u32 = wine_afd_ioc(234);
const IOCTL_AFD_WINE_SET_SO_SNDBUF: u32 = wine_afd_ioc(235);
const IOCTL_AFD_WINE_GET_SO_SNDBUF: u32 = wine_afd_ioc(236);
const IOCTL_AFD_WINE_GET_TCP_NODELAY: u32 = wine_afd_ioc(284);
const IOCTL_AFD_WINE_SET_TCP_NODELAY: u32 = wine_afd_ioc(285);

const AFD_POLL_READ: i32 = 0x0001;
const AFD_POLL_OOB: i32 = 0x0002;
const AFD_POLL_WRITE: i32 = 0x0004;
const AFD_POLL_HUP: i32 = 0x0008;
const AFD_POLL_RESET: i32 = 0x0010;
const AFD_POLL_CONNECT: i32 = 0x0040;
const AFD_POLL_ACCEPT: i32 = 0x0080;

const LINUX_SOL_SOCKET: u64 = 1;
const LINUX_SO_REUSEADDR: u64 = 2;
const LINUX_SO_ERROR: u64 = 4;
const LINUX_SO_BROADCAST: u64 = 6;
const LINUX_SO_SNDBUF: u64 = 7;
const LINUX_SO_RCVBUF: u64 = 8;
const LINUX_SO_KEEPALIVE: u64 = 9;
const LINUX_SO_ACCEPTCONN: u64 = 30;
const LINUX_IPPROTO_TCP: u64 = 6;
const LINUX_TCP_NODELAY: u64 = 1;

const STATUS_DEVICE_NOT_READY: u64 = 0xc000_00a3;
const STATUS_CANT_WAIT: u64 = 0xc000_00d8;
const STATUS_CONNECTION_REFUSED: u64 = 0xc000_0236;

pub(super) fn is_afd_handle(h: u64) -> bool {
    (WIN32_AFD_HANDLE_BASE..WIN32_NSI_HANDLE_BASE).contains(&h)
}

pub(super) fn is_nsi_handle(h: u64) -> bool {
    (WIN32_NSI_HANDLE_BASE..0x7000_0000).contains(&h)
}

pub(super) fn open_afd_device(handle_ptr: u64, io_status_ptr: u64) -> u64 {
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_AFD_HANDLE_BASE + objs.next_afd_seq + 1;
        objs.next_afd_seq += 1;
        objs.afd_sockets.push(Win32AfdSocketObject {
            handle: h,
            fd: None,
            family: 2,
            sock_type: 1,
            protocol: 6,
            flags: 0,
            nonblocking: false,
            event_handle: 0,
            event_mask: 0,
            connect_error: 0,
        });
        h
    });
    match handle {
        Some(h) => {
            let status = write_user_u64(handle_ptr, h);
            if status == STATUS_SUCCESS {
                write_io_status(io_status_ptr, STATUS_SUCCESS, 1);
            }
            status
        }
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn open_nsi_device(handle_ptr: u64, io_status_ptr: u64) -> u64 {
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_NSI_HANDLE_BASE + objs.next_nsi_seq + 1;
        objs.next_nsi_seq += 1;
        objs.nsi_handles.push(Win32NsiHandleObject { handle: h });
        h
    });
    match handle {
        Some(h) => {
            let status = write_user_u64(handle_ptr, h);
            if status == STATUS_SUCCESS {
                write_io_status(io_status_ptr, STATUS_SUCCESS, 1);
            }
            status
        }
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn close_afd_or_nsi_handle(handle: u64) -> u64 {
    if is_afd_handle(handle) {
        let fd = multitask::with_current_user_process_state_mut(|_, _, st| {
            let socks = &mut st.win32_objects_mut().afd_sockets;
            let idx = socks.iter().position(|s| s.handle == handle)?;
            Some(socks.remove(idx).fd)
        })
        .flatten();
        return match fd {
            Some(Some(fd)) => {
                let _ = vfs_socket::syscall_linux_vfs_close(fd);
                STATUS_SUCCESS
            }
            Some(None) => STATUS_SUCCESS,
            None => STATUS_INVALID_HANDLE,
        };
    }
    if is_nsi_handle(handle) {
        let removed = multitask::with_current_user_process_state_mut(|_, _, st| {
            let ns = &mut st.win32_objects_mut().nsi_handles;
            let idx = ns.iter().position(|n| n.handle == handle)?;
            ns.remove(idx);
            Some(())
        })
        .flatten();
        return if removed.is_some() {
            STATUS_SUCCESS
        } else {
            STATUS_INVALID_HANDLE
        };
    }
    STATUS_INVALID_HANDLE
}

fn afd_socket_fd(handle: u64) -> Option<Win32AfdSocketObject> {
    multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .afd_sockets
            .iter()
            .find(|s| s.handle == handle)
            .cloned()
    })
    .flatten()
}

fn netd_errno_to_ntstatus(errno: i64) -> u64 {
    match errno {
        e if e == LINUX_EAGAIN => STATUS_CANT_WAIT,
        e if e == LINUX_EINPROGRESS => STATUS_PENDING,
        e if e == LINUX_ENOTCONN => STATUS_INVALID_PARAMETER,
        111 /* ECONNREFUSED */ => STATUS_CONNECTION_REFUSED,
        _ => linux_errno_to_ntstatus(errno),
    }
}

fn afd_map_wsabufs(buffers_ptr: u64, count: u32) -> Result<Vec<(u64, usize)>, u64> {
    if count == 0 || buffers_ptr == 0 || count > 64 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as u64 {
        let mut raw = [0_u8; 16];
        usermem::copy_from_current_user_exact(buffers_ptr + i * 16, &mut raw)
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        let len = u32::from_le_bytes(raw[0..4].try_into().unwrap_or([0; 4])) as usize;
        let buf_ptr = u64::from_le_bytes(raw[8..16].try_into().unwrap_or([0; 8]));
        out.push((buf_ptr, len));
    }
    Ok(out)
}

fn signal_event_if_valid(event_handle: u64) {
    if event_handle != 0 {
        let _ = sync::nt_modify_event(event_handle, 0, true, false);
    }
}

pub(super) fn nt_device_io_control_file(frame: &SyscallFrame) -> u64 {
    let (handle, event_handle, _apc, _apc_ctx) = (frame.r10, frame.rdx, frame.r8, frame.r9);
    let io_status_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let ioctl_code = read_stack_arg(frame, 5).unwrap_or(0) as u32;
    let in_buf = read_stack_arg(frame, 6).unwrap_or(0);
    let in_len = read_stack_arg(frame, 7).unwrap_or(0) as usize;
    let out_buf = read_stack_arg(frame, 8).unwrap_or(0);
    let out_len = read_stack_arg(frame, 9).unwrap_or(0) as usize;

    if is_nsi_handle(handle) {
        return handle_nsi_ioctl(ioctl_code, in_buf, in_len, out_buf, out_len, io_status_ptr);
    }
    if !is_afd_handle(handle) {
        return STATUS_NOT_IMPLEMENTED;
    }

    let status = match ioctl_code {
        IOCTL_AFD_WINE_CREATE => afd_create(handle, in_buf, in_len),
        IOCTL_AFD_WINE_GET_INFO => afd_get_info(handle, out_buf, out_len, io_status_ptr),
        IOCTL_AFD_BIND => afd_bind(handle, in_buf, in_len, out_buf, out_len, io_status_ptr),
        IOCTL_AFD_LISTEN => afd_listen(handle, in_buf, in_len),
        IOCTL_AFD_WINE_CONNECT => afd_connect(handle, in_buf, in_len),
        IOCTL_AFD_WINE_ACCEPT | IOCTL_AFD_WINE_ACCEPT_INTO => {
            afd_accept(handle, ioctl_code, in_buf, in_len, out_buf, out_len, io_status_ptr)
        }
        IOCTL_AFD_WINE_SENDMSG => afd_sendmsg(handle, in_buf, in_len, io_status_ptr),
        IOCTL_AFD_WINE_RECVMSG => afd_recvmsg(handle, in_buf, in_len, io_status_ptr),
        IOCTL_AFD_WINE_SHUTDOWN => afd_shutdown(handle, in_buf, in_len),
        IOCTL_AFD_GETSOCKNAME | IOCTL_AFD_WINE_GETPEERNAME => {
            afd_get_sock_or_peer_name(handle, ioctl_code, out_buf, out_len, io_status_ptr)
        }
        IOCTL_AFD_WINE_FIONBIO => afd_fionbio(handle, in_buf, in_len),
        IOCTL_AFD_WINE_FIONREAD => afd_fionread(handle, out_buf, out_len, io_status_ptr),
        IOCTL_AFD_POLL => afd_poll(in_buf, in_len, out_buf, out_len, io_status_ptr),
        IOCTL_AFD_EVENT_SELECT => afd_event_select(handle, in_buf, in_len),
        IOCTL_AFD_GET_EVENTS => afd_get_events(handle, out_buf, out_len, io_status_ptr),
        IOCTL_AFD_WINE_COMPLETE_ASYNC => STATUS_SUCCESS,
        c if is_sockopt_ioctl(c) => {
            afd_sockopt(handle, c, in_buf, in_len, out_buf, out_len, io_status_ptr)
        }
        _ => STATUS_NOT_IMPLEMENTED,
    };

    if status == STATUS_SUCCESS {
        signal_event_if_valid(event_handle);
    }
    status
}

fn afd_create(handle: u64, in_buf: u64, in_len: usize) -> u64 {
    if in_buf == 0 || in_len < 16 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 16];
    if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let family = i32::from_le_bytes(raw[0..4].try_into().unwrap_or([0; 4]));
    let sock_type = i32::from_le_bytes(raw[4..8].try_into().unwrap_or([0; 4]));
    let protocol = i32::from_le_bytes(raw[8..12].try_into().unwrap_or([0; 4]));
    let flags = u32::from_le_bytes(raw[12..16].try_into().unwrap_or([0; 4]));

    let res = vfs_meta::syscall_linux_net6(
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
        family as u64,
        sock_type as u64,
        protocol as u64,
        0,
        0,
        0,
    );
    let fd = match vfs_socket::kernel_vfs_result(res) {
        Ok(fd) => fd,
        Err(errno) => return netd_errno_to_ntstatus(errno),
    };
    let ok = multitask::with_current_user_process_state_mut(|_, _, st| {
        if let Some(s) = st
            .win32_objects_mut()
            .afd_sockets
            .iter_mut()
            .find(|s| s.handle == handle)
        {
            s.fd = Some(fd);
            s.family = family;
            s.sock_type = sock_type;
            s.protocol = protocol;
            s.flags = flags;
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if !ok {
        let _ = vfs_socket::syscall_linux_vfs_close(fd);
        return STATUS_INVALID_HANDLE;
    }
    STATUS_SUCCESS
}

fn afd_get_info(handle: u64, out_buf: u64, out_len: usize, io_status_ptr: u64) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    if out_buf == 0 || out_len < 12 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 12];
    raw[0..4].copy_from_slice(&sock.family.to_le_bytes());
    raw[4..8].copy_from_slice(&sock.sock_type.to_le_bytes());
    raw[8..12].copy_from_slice(&sock.protocol.to_le_bytes());
    if usermem::write_current_user_bytes(out_buf, &raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    write_io_status(io_status_ptr, STATUS_SUCCESS, 12);
    STATUS_SUCCESS
}

fn afd_bind(
    handle: u64,
    in_buf: u64,
    in_len: usize,
    _out_buf: u64,
    _out_len: usize,
    io_status_ptr: u64,
) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if in_buf == 0 || in_len <= 4 {
        return STATUS_INVALID_PARAMETER;
    }
    let addr_ptr = in_buf + 4;
    let addr_len = (in_len - 4) as u64;
    let res = vfs_meta::syscall_linux_net6(
        SYSCALL_OFFLOAD_OP_LINUX_BIND,
        fd,
        addr_ptr,
        addr_len,
        0,
        0,
        0,
    );
    match vfs_socket::kernel_vfs_result(res) {
        Ok(_) => {
            write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
            STATUS_SUCCESS
        }
        Err(errno) => netd_errno_to_ntstatus(errno),
    }
}

fn afd_listen(handle: u64, in_buf: u64, in_len: usize) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if in_buf == 0 || in_len < 12 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 12];
    if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let backlog = i32::from_le_bytes(raw[4..8].try_into().unwrap_or([0; 4]));
    let res = vfs_meta::syscall_linux_net6(
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
        fd,
        backlog as u64,
        0,
        0,
        0,
        0,
    );
    match vfs_socket::kernel_vfs_result(res) {
        Ok(_) => STATUS_SUCCESS,
        Err(errno) => netd_errno_to_ntstatus(errno),
    }
}

fn afd_connect(handle: u64, in_buf: u64, in_len: usize) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if in_buf == 0 || in_len < 8 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut hdr = [0_u8; 8];
    if usermem::copy_from_current_user_exact(in_buf, &mut hdr).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let addr_len = i32::from_le_bytes(hdr[0..4].try_into().unwrap_or([0; 4])) as usize;
    if addr_len == 0 || 8 + addr_len > in_len {
        return STATUS_INVALID_PARAMETER;
    }
    let res = vfs_meta::syscall_linux_net6(
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT,
        fd,
        in_buf + 8,
        addr_len as u64,
        0,
        0,
        0,
    );
    match vfs_socket::kernel_vfs_result(res) {
        Ok(_) => STATUS_SUCCESS,
        Err(errno) => netd_errno_to_ntstatus(errno),
    }
}

fn afd_accept(
    handle: u64,
    ioctl_code: u32,
    in_buf: u64,
    in_len: usize,
    out_buf: u64,
    out_len: usize,
    io_status_ptr: u64,
) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    let res = vfs_meta::syscall_linux_net6(SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, fd, 0, 0, 0, 0, 0);
    let new_fd = match vfs_socket::kernel_vfs_result(res) {
        Ok(new_fd) => new_fd,
        Err(errno) => return netd_errno_to_ntstatus(errno),
    };
    if ioctl_code == IOCTL_AFD_WINE_ACCEPT_INTO && in_buf != 0 && in_len >= 12 {
        let mut raw = [0_u8; 12];
        if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
            let _ = vfs_socket::syscall_linux_vfs_close(new_fd);
            return STATUS_INVALID_PARAMETER;
        }
        let target_h = u32::from_le_bytes(raw[0..4].try_into().unwrap_or([0; 4])) as u64;
        let bound = multitask::with_current_user_process_state_mut(|_, _, st| {
            if let Some(target) = st
                .win32_objects_mut()
                .afd_sockets
                .iter_mut()
                .find(|s| s.handle == target_h)
            {
                target.fd = Some(new_fd);
                target.family = sock.family;
                target.sock_type = sock.sock_type;
                target.protocol = sock.protocol;
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
        if !bound {
            let _ = vfs_socket::syscall_linux_vfs_close(new_fd);
            return STATUS_INVALID_HANDLE;
        }
        write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
        return STATUS_SUCCESS;
    }

    if out_buf != 0 && out_len >= 8 {
        let new_h = multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects_mut();
            let h = WIN32_AFD_HANDLE_BASE + objs.next_afd_seq + 1;
            objs.next_afd_seq += 1;
            objs.afd_sockets.push(Win32AfdSocketObject {
                handle: h,
                fd: Some(new_fd),
                family: sock.family,
                sock_type: sock.sock_type,
                protocol: sock.protocol,
                flags: 0,
                nonblocking: false,
                event_handle: 0,
                event_mask: 0,
                connect_error: 0,
            });
            h
        });
        if let Some(h) = new_h
            && write_user_u64(out_buf, h) == STATUS_SUCCESS
        {
            write_io_status(io_status_ptr, STATUS_SUCCESS, 8);
            return STATUS_SUCCESS;
        }
    }
    let _ = vfs_socket::syscall_linux_vfs_close(new_fd);
    STATUS_INVALID_PARAMETER
}

fn afd_sendmsg(handle: u64, in_buf: u64, in_len: usize, io_status_ptr: u64) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if in_buf == 0 || in_len < 32 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 32];
    if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let addr_ptr = u64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8]));
    let addr_len = u32::from_le_bytes(raw[8..12].try_into().unwrap_or([0; 4])) as u64;
    let ws_flags = u32::from_le_bytes(raw[12..16].try_into().unwrap_or([0; 4])) as u64;
    let force_async = i32::from_le_bytes(raw[16..20].try_into().unwrap_or([0; 4]));
    let count = u32::from_le_bytes(raw[20..24].try_into().unwrap_or([0; 4]));
    let buffers_ptr = u64::from_le_bytes(raw[24..32].try_into().unwrap_or([0; 8]));

    let Ok(bufs) = afd_map_wsabufs(buffers_ptr, count) else {
        return STATUS_INVALID_PARAMETER;
    };
    let flags = ws_flags
        | if force_async != 0 || sock.nonblocking {
            linux_abi::MSG_DONTWAIT
        } else {
            0
        };
    let mut total_sent = 0_u64;
    for (buf_ptr, len) in bufs {
        let res = vfs_meta::syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
            fd,
            buf_ptr,
            len as u64,
            flags,
            addr_ptr,
            addr_len,
        );
        match vfs_socket::kernel_vfs_result(res) {
            Ok(n) => total_sent += n,
            Err(errno) if total_sent > 0 && errno == LINUX_EAGAIN => break,
            Err(errno) => return netd_errno_to_ntstatus(errno),
        }
    }
    write_io_status(io_status_ptr, STATUS_SUCCESS, total_sent);
    STATUS_SUCCESS
}

fn afd_recvmsg(handle: u64, in_buf: u64, in_len: usize, io_status_ptr: u64) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if in_buf == 0 || in_len < 48 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 48];
    if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let addr_ptr = u64::from_le_bytes(raw[8..16].try_into().unwrap_or([0; 8]));
    let addr_len_ptr = u64::from_le_bytes(raw[16..24].try_into().unwrap_or([0; 8]));
    let ws_flags_ptr = u64::from_le_bytes(raw[24..32].try_into().unwrap_or([0; 8]));
    let force_async = i32::from_le_bytes(raw[32..36].try_into().unwrap_or([0; 4]));
    let count = u32::from_le_bytes(raw[36..40].try_into().unwrap_or([0; 4]));
    let buffers_ptr = u64::from_le_bytes(raw[40..48].try_into().unwrap_or([0; 8]));

    let Ok(bufs) = afd_map_wsabufs(buffers_ptr, count) else {
        return STATUS_INVALID_PARAMETER;
    };
    let mut ws_flags = 0_u64;
    if ws_flags_ptr != 0 {
        let mut f = [0_u8; 4];
        if usermem::copy_from_current_user_exact(ws_flags_ptr, &mut f).is_ok() {
            ws_flags = u32::from_le_bytes(f) as u64;
        }
    }
    let flags = ws_flags
        | if force_async != 0 || sock.nonblocking {
            linux_abi::MSG_DONTWAIT
        } else {
            0
        };
    let (buf_ptr, len) = bufs[0];
    let res = vfs_meta::syscall_linux_net6(
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
        fd,
        buf_ptr,
        len as u64,
        flags,
        addr_ptr,
        addr_len_ptr,
    );
    match vfs_socket::kernel_vfs_result(res) {
        Ok(n) => {
            if ws_flags_ptr != 0 {
                let _ = write_user_u32_checked(ws_flags_ptr, 0);
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, n);
            STATUS_SUCCESS
        }
        Err(errno) => netd_errno_to_ntstatus(errno),
    }
}

fn afd_shutdown(handle: u64, in_buf: u64, in_len: usize) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if in_buf == 0 || in_len < 4 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 4];
    if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let how = i32::from_le_bytes(raw);
    let res = vfs_meta::syscall_linux_net6(
        SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN,
        fd,
        how as u64,
        0,
        0,
        0,
        0,
    );
    match vfs_socket::kernel_vfs_result(res) {
        Ok(_) => STATUS_SUCCESS,
        Err(errno) => netd_errno_to_ntstatus(errno),
    }
}

fn afd_get_sock_or_peer_name(
    handle: u64,
    ioctl_code: u32,
    out_buf: u64,
    out_len: usize,
    io_status_ptr: u64,
) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if out_buf == 0 || out_len == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let op = if ioctl_code == IOCTL_AFD_GETSOCKNAME {
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
    } else {
        SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
    };
    match vfs_meta::kernel_netd_raw_socket_op(op, fd, [0, out_len as u64, 0, 0, 0], &[], Some(500))
    {
        Ok((_, payload)) => {
            let copy_len = payload.len().min(out_len);
            if copy_len > 0 && usermem::write_current_user_bytes(out_buf, &payload[..copy_len]).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            write_io_status(io_status_ptr, STATUS_SUCCESS, copy_len as u64);
            STATUS_SUCCESS
        }
        Err(errno) => netd_errno_to_ntstatus(errno),
    }
}

fn afd_fionbio(handle: u64, in_buf: u64, in_len: usize) -> u64 {
    if in_buf == 0 || in_len < 4 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 4];
    if usermem::copy_from_current_user_exact(in_buf, &mut raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let nonblocking = i32::from_le_bytes(raw) != 0;
    let fd_opt = multitask::with_current_user_process_state_mut(|_, _, st| {
        let s = st
            .win32_objects_mut()
            .afd_sockets
            .iter_mut()
            .find(|s| s.handle == handle)?;
        s.nonblocking = nonblocking;
        Some(s.fd)
    })
    .flatten();
    let Some(fd_opt) = fd_opt else {
        return STATUS_INVALID_HANDLE;
    };
    if let Some(fd) = fd_opt {
        let flags = if nonblocking { linux_abi::O_NONBLOCK } else { 0 };
        let _ = vfs_socket::syscall_linux_vfs_fcntl(fd, linux_abi::F_SETFL, flags);
    }
    STATUS_SUCCESS
}

fn afd_fionread(handle: u64, out_buf: u64, out_len: usize, io_status_ptr: u64) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    if out_buf == 0 || out_len < 4 {
        return STATUS_INVALID_PARAMETER;
    }
    let readable = vfs_meta::kernel_netd_poll_fd(fd, linux_abi::POLLIN as u32, 0)
        .map(|rev| if (rev & linux_abi::POLLIN as u32) != 0 { 1_u32 } else { 0 })
        .unwrap_or(0);
    write_io_status(io_status_ptr, STATUS_SUCCESS, 4);
    write_user_u32_checked(out_buf, readable)
}

fn afd_poll(
    in_buf: u64,
    in_len: usize,
    out_buf: u64,
    out_len: usize,
    io_status_ptr: u64,
) -> u64 {
    if in_buf == 0 || in_len < 16 || out_buf == 0 || out_len < 16 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut hdr = [0_u8; 16];
    if usermem::copy_from_current_user_exact(in_buf, &mut hdr).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let timeout_100ns = i64::from_le_bytes(hdr[0..8].try_into().unwrap_or([0; 8]));
    let count = u32::from_le_bytes(hdr[8..12].try_into().unwrap_or([0; 4])) as usize;
    if count == 0 || count > 64 || 16 + count * 16 > in_len || 16 + count * 16 > out_len {
        return STATUS_INVALID_PARAMETER;
    }
    let timeout_ms: u64 = if timeout_100ns == 0 {
        0
    } else if timeout_100ns < 0 {
        (timeout_100ns.unsigned_abs() / 10_000).clamp(1, 200)
    } else {
        16
    };

    let mut ready_count = 0_u32;
    let mut out_entries = vec![0_u8; 16 + count * 16];
    out_entries[0..16].copy_from_slice(&hdr);

    for i in 0..count {
        let off = 16 + i * 16;
        let mut ent = [0_u8; 16];
        if usermem::copy_from_current_user_exact(in_buf + off as u64, &mut ent).is_err() {
            return STATUS_INVALID_PARAMETER;
        }
        let sock_handle = u64::from_le_bytes(ent[0..8].try_into().unwrap_or([0; 8]));
        let req_flags = i32::from_le_bytes(ent[8..12].try_into().unwrap_or([0; 4]));
        let mut events = 0_u32;
        if (req_flags & (AFD_POLL_READ | AFD_POLL_ACCEPT)) != 0 {
            events |= linux_abi::POLLIN as u32;
        }
        if (req_flags & AFD_POLL_OOB) != 0 {
            events |= linux_abi::POLLPRI as u32;
        }
        if (req_flags & (AFD_POLL_WRITE | AFD_POLL_CONNECT)) != 0 {
            events |= linux_abi::POLLOUT as u32;
        }
        let mut out_flags = 0_i32;
        if let Some(sock) = afd_socket_fd(sock_handle)
            && let Some(fd) = sock.fd
            && let Ok(rev) = vfs_meta::kernel_netd_poll_fd(fd, events, timeout_ms)
        {
            if (rev & linux_abi::POLLIN as u32) != 0 {
                out_flags |= req_flags & (AFD_POLL_READ | AFD_POLL_ACCEPT);
            }
            if (rev & linux_abi::POLLPRI as u32) != 0 {
                out_flags |= AFD_POLL_OOB;
            }
            if (rev & linux_abi::POLLOUT as u32) != 0 {
                out_flags |= req_flags & (AFD_POLL_WRITE | AFD_POLL_CONNECT);
            }
            if (rev & linux_abi::POLLHUP as u32) != 0 {
                out_flags |= AFD_POLL_HUP;
            }
            if (rev & linux_abi::POLLERR as u32) != 0 {
                out_flags |= AFD_POLL_RESET;
            }
        }
        if out_flags != 0 {
            ready_count += 1;
        }
        out_entries[off..off + 8].copy_from_slice(&sock_handle.to_le_bytes());
        out_entries[off + 8..off + 12].copy_from_slice(&out_flags.to_le_bytes());
        out_entries[off + 12..off + 16].copy_from_slice(&0_i32.to_le_bytes());
    }
    let _ = ready_count;
    if usermem::write_current_user_bytes(out_buf, &out_entries).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    write_io_status(io_status_ptr, STATUS_SUCCESS, out_entries.len() as u64);
    STATUS_SUCCESS
}

fn afd_event_select(handle: u64, in_buf: u64, in_len: usize) -> u64 {
    if in_buf == 0 || in_len < 12 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut raw = [0_u8; 16];
    let copy_len = in_len.min(16);
    if usermem::copy_from_current_user_exact(in_buf, &mut raw[..copy_len]).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let event_h = u64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8]));
    let mask = u32::from_le_bytes(raw[8..12].try_into().unwrap_or([0; 4]));
    let ok = multitask::with_current_user_process_state_mut(|_, _, st| {
        let s = st
            .win32_objects_mut()
            .afd_sockets
            .iter_mut()
            .find(|s| s.handle == handle)?;
        s.event_handle = event_h;
        s.event_mask = mask;
        Some(())
    })
    .flatten();
    if ok.is_some() {
        STATUS_SUCCESS
    } else {
        STATUS_INVALID_HANDLE
    }
}

fn afd_get_events(handle: u64, out_buf: u64, out_len: usize, io_status_ptr: u64) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    if out_buf == 0 || out_len < 56 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut flags = 0_i32;
    if let Some(fd) = sock.fd
        && let Ok(rev) = vfs_meta::kernel_netd_poll_fd(
            fd,
            (linux_abi::POLLIN | linux_abi::POLLOUT) as u32,
            0,
        )
    {
        if (rev & linux_abi::POLLIN as u32) != 0 {
            flags |= AFD_POLL_READ | AFD_POLL_ACCEPT;
        }
        if (rev & linux_abi::POLLOUT as u32) != 0 {
            flags |= AFD_POLL_WRITE | AFD_POLL_CONNECT;
        }
        if (rev & linux_abi::POLLHUP as u32) != 0 {
            flags |= AFD_POLL_HUP;
        }
    }
    let mut raw = [0_u8; 56];
    raw[0..4].copy_from_slice(&flags.to_le_bytes());
    if usermem::write_current_user_bytes(out_buf, &raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    write_io_status(io_status_ptr, STATUS_SUCCESS, 56);
    STATUS_SUCCESS
}

const fn is_sockopt_ioctl(code: u32) -> bool {
    matches!(
        code,
        IOCTL_AFD_WINE_GET_SO_ACCEPTCONN
            | IOCTL_AFD_WINE_GET_SO_BROADCAST
            | IOCTL_AFD_WINE_SET_SO_BROADCAST
            | IOCTL_AFD_WINE_GET_SO_ERROR
            | IOCTL_AFD_WINE_GET_SO_KEEPALIVE
            | IOCTL_AFD_WINE_SET_SO_KEEPALIVE
            | IOCTL_AFD_WINE_SET_SO_RCVBUF
            | IOCTL_AFD_WINE_GET_SO_RCVBUF
            | IOCTL_AFD_WINE_GET_SO_REUSEADDR
            | IOCTL_AFD_WINE_SET_SO_REUSEADDR
            | IOCTL_AFD_WINE_SET_SO_SNDBUF
            | IOCTL_AFD_WINE_GET_SO_SNDBUF
            | IOCTL_AFD_WINE_GET_TCP_NODELAY
            | IOCTL_AFD_WINE_SET_TCP_NODELAY
    )
}

fn afd_sockopt(
    handle: u64,
    code: u32,
    in_buf: u64,
    in_len: usize,
    out_buf: u64,
    out_len: usize,
    io_status_ptr: u64,
) -> u64 {
    let Some(sock) = afd_socket_fd(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let Some(fd) = sock.fd else {
        return STATUS_DEVICE_NOT_READY;
    };
    let (is_set, level, optname) = match code {
        IOCTL_AFD_WINE_GET_SO_ACCEPTCONN => (false, LINUX_SOL_SOCKET, LINUX_SO_ACCEPTCONN),
        IOCTL_AFD_WINE_GET_SO_BROADCAST => (false, LINUX_SOL_SOCKET, LINUX_SO_BROADCAST),
        IOCTL_AFD_WINE_SET_SO_BROADCAST => (true, LINUX_SOL_SOCKET, LINUX_SO_BROADCAST),
        IOCTL_AFD_WINE_GET_SO_ERROR => (false, LINUX_SOL_SOCKET, LINUX_SO_ERROR),
        IOCTL_AFD_WINE_GET_SO_KEEPALIVE => (false, LINUX_SOL_SOCKET, LINUX_SO_KEEPALIVE),
        IOCTL_AFD_WINE_SET_SO_KEEPALIVE => (true, LINUX_SOL_SOCKET, LINUX_SO_KEEPALIVE),
        IOCTL_AFD_WINE_GET_SO_RCVBUF => (false, LINUX_SOL_SOCKET, LINUX_SO_RCVBUF),
        IOCTL_AFD_WINE_SET_SO_RCVBUF => (true, LINUX_SOL_SOCKET, LINUX_SO_RCVBUF),
        IOCTL_AFD_WINE_GET_SO_REUSEADDR => (false, LINUX_SOL_SOCKET, LINUX_SO_REUSEADDR),
        IOCTL_AFD_WINE_SET_SO_REUSEADDR => (true, LINUX_SOL_SOCKET, LINUX_SO_REUSEADDR),
        IOCTL_AFD_WINE_GET_SO_SNDBUF => (false, LINUX_SOL_SOCKET, LINUX_SO_SNDBUF),
        IOCTL_AFD_WINE_SET_SO_SNDBUF => (true, LINUX_SOL_SOCKET, LINUX_SO_SNDBUF),
        IOCTL_AFD_WINE_GET_TCP_NODELAY => (false, LINUX_IPPROTO_TCP, LINUX_TCP_NODELAY),
        IOCTL_AFD_WINE_SET_TCP_NODELAY => (true, LINUX_IPPROTO_TCP, LINUX_TCP_NODELAY),
        _ => return STATUS_NOT_IMPLEMENTED,
    };

    if is_set {
        let copy_len = in_len.min(64);
        let mut val = vec![0_u8; copy_len];
        if copy_len > 0 && (in_buf == 0 || usermem::copy_from_current_user_exact(in_buf, &mut val).is_err()) {
            return STATUS_INVALID_PARAMETER;
        }
        match vfs_meta::kernel_netd_raw_socket_op(
            SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
            fd,
            [level, optname, 0, copy_len as u64, 0],
            &val,
            Some(500),
        ) {
            Ok(_) => STATUS_SUCCESS,
            Err(errno) => netd_errno_to_ntstatus(errno),
        }
    } else {
        if out_buf == 0 || out_len < 4 {
            return STATUS_INVALID_PARAMETER;
        }
        match vfs_meta::kernel_netd_raw_socket_op(
            SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
            fd,
            [level, optname, 0, out_len as u64, 0],
            &[],
            Some(500),
        ) {
            Ok((_, payload)) => {
                let copy_len = payload.len().min(out_len);
                if copy_len > 0 && usermem::write_current_user_bytes(out_buf, &payload[..copy_len]).is_err() {
                    return STATUS_INVALID_PARAMETER;
                }
                write_io_status(io_status_ptr, STATUS_SUCCESS, copy_len as u64);
                STATUS_SUCCESS
            }
            Err(errno) => netd_errno_to_ntstatus(errno),
        }
    }
}

fn handle_nsi_ioctl(
    _ioctl_code: u32,
    _in_buf: u64,
    _in_len: usize,
    out_buf: u64,
    out_len: usize,
    io_status_ptr: u64,
) -> u64 {
    // Minimal NSI response returning zero-count enumeration cleanly without wineserver/nsiproxy.sys.
    if out_buf != 0 && out_len >= 8 {
        let zeros = [0_u8; 8];
        let _ = usermem::write_current_user_bytes(out_buf, &zeros);
        write_io_status(io_status_ptr, STATUS_SUCCESS, 8);
    } else {
        write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
    }
    STATUS_SUCCESS
}
