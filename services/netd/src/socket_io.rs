//! `send*`/`recv*` entry points for the sockets netd owns: INET (TCP, UDP, ICMP) and `AF_NETLINK`.
//!
//! `AF_UNIX` data movement is kernel-local (`docs/ai/abi/unix-socket.md`) and never reaches netd.

use super::*;

pub(super) fn handle_send(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
        if inet.transport.is_datagram() {
            return inet_datagram::send_in_state(&mut state, request, response);
        }
        if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDMSG {
            // The sendmsg envelope is a 16-byte header plus data and control
            // sections; only the data section is TCP stream payload. TCP has
            // no ancillary data, so a control section is refused.
            let (data, control) = match split_sendmsg_payload(request) {
                Ok(sections) => sections,
                Err(errno) => return errno,
            };
            if !control.is_empty() {
                return libc::EOPNOTSUPP;
            }
            return handle_inet_send_in_state(
                &mut state,
                request,
                &request.payload[data],
                response,
            );
        }
        let len = request.payload_len as usize;
        return handle_inet_send_in_state(&mut state, request, &request.payload[..len], response);
    }
    let (data, control) = if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDMSG {
        match split_sendmsg_payload(request) {
            Ok(sections) => sections,
            Err(errno) => return errno,
        }
    } else {
        let len = request.payload_len as usize;
        (0..len, len..len)
    };
    match netlink::send_message(
        &mut state,
        request,
        &request.payload[data],
        &request.payload[control],
    ) {
        Ok(sent) => {
            response.value = sent as u64;
            0
        }
        Err(errno) => errno,
    }
}

/// Split a SENDMSG envelope into its data and control byte ranges inside
/// `request.payload`, rejecting any section that leaves the received payload.
pub(super) fn split_sendmsg_payload(
    request: &NetdIpcRequest,
) -> Result<(std::ops::Range<usize>, std::ops::Range<usize>), i32> {
    let payload_len = request.payload_len as usize;
    if payload_len < NETD_SENDMSG_PAYLOAD_HEADER_SIZE || payload_len > request.payload.len() {
        return Err(libc::EINVAL);
    }
    let data_len = u32::from_ne_bytes(request.payload[0..4].try_into().unwrap_or([0; 4])) as usize;
    let control_len =
        u32::from_ne_bytes(request.payload[4..8].try_into().unwrap_or([0; 4])) as usize;
    let data_start = NETD_SENDMSG_PAYLOAD_HEADER_SIZE;
    let control_start = data_start.checked_add(data_len).ok_or(libc::EINVAL)?;
    let end = control_start.checked_add(control_len).ok_or(libc::EINVAL)?;
    if end > payload_len {
        return Err(libc::EINVAL);
    }
    Ok((data_start..control_start, control_start..end))
}

pub(super) fn handle_recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    if let Some(inet) = state.inet_sockets.get(&request.socket_token) {
        if inet.transport.is_datagram() {
            return inet_datagram::recv_in_state(&mut state, request, response);
        }
        return handle_inet_recv_in_state(&mut state, request, response);
    }
    // A netlink `recvmsg` reply carries the msghdr envelope the kernel parses;
    // `recvfrom` carries the bare datagram.
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        netlink::handle_recvmsg(&mut state, request, response)
    } else {
        netlink::handle_recv(&mut state, request, response)
    }
}
