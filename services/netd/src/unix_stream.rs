//! AF_UNIX stream send/receive, ancillary data custody and SCM_* validation.

use super::*;

pub(super) fn handle_send(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if inet_datagram::is_datagram(request.socket_token) {
        return inet_datagram::send(request, response);
    }
    if inet_socket_exists(request.socket_token) {
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
            return handle_inet_send(request, &request.payload[data], response);
        }
        let len = request.payload_len as usize;
        return handle_inet_send(request, &request.payload[..len], response);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_SENDMSG {
        return handle_sendmsg(request, response);
    }
    let len = request.payload_len as usize;
    let bytes = &request.payload[..len];
    match send_socket_bytes(request, bytes) {
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

pub(super) fn handle_sendmsg(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let (data, control) = match split_sendmsg_payload(request) {
        Ok(sections) => sections,
        Err(errno) => return errno,
    };
    if let Err(errno) = validate_sendmsg_control(&request.payload[control.clone()]) {
        return errno;
    }
    match send_socket_message(request, &request.payload[data], &request.payload[control]) {
        Ok(sent) => {
            response.value = sent as u64;
            0
        }
        Err(errno) => errno,
    }
}

pub(super) fn handle_recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if inet_datagram::is_datagram(request.socket_token) {
        return inet_datagram::recv(request, response);
    }
    if inet_socket_exists(request.socket_token) {
        return handle_inet_recv(request, response);
    }
    if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        return handle_recvmsg(request, response);
    }
    let requested = if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        request.payload_len as usize
    } else {
        usize::try_from(request.arg2).unwrap_or(usize::MAX)
    };
    let limit = requested.min(response.payload.len() - SOCKET_CONTROL_BUFFER_CAPACITY);
    match recv_socket_bytes(request, &mut response.payload[..limit], false) {
        Ok(received) => {
            let release_start = received.read;
            let release_end = release_start + received.discarded.len();
            response.payload[release_start..release_end]
                .copy_from_slice(received.discarded.as_slice());
            response.reserved0 = received.discarded.len() as u32;
            response.value = received.read as u64;
            response.payload_len = release_end as u32;
            0
        }
        Err(errno) => errno,
    }
}

pub(super) fn handle_recvmsg(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    recvmsg_in_state(&mut state, request, response)
}

pub(super) fn recvmsg_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> i32 {
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return libc::ENOTCONN;
    };
    let requested = request.payload_len as usize;
    let credentials = socket
        .options
        .passcred
        .then_some(connected.peer_credentials);
    let control_len = pending_recvmsg_control_len(connected, requested, credentials.is_some());
    // Preflight the whole reply before moving bytes or ancillary custody. The
    // same state transaction owns options, queue selection, dequeue and encode.
    let Some(available) = response
        .payload
        .len()
        .checked_sub(NETD_RECVMSG_PAYLOAD_HEADER_SIZE)
        .and_then(|room| room.checked_sub(control_len))
    else {
        return libc::EMSGSIZE;
    };
    let data_start = NETD_RECVMSG_PAYLOAD_HEADER_SIZE;
    let data_end = data_start + requested.min(available);
    match recv_socket_bytes_in_state(state,request,&mut response.payload[data_start..data_end],true) {
        Ok(received) => {
            let control = append_recvmsg_credentials(credentials, received.control);
            let control_start = data_start + received.read;
            let control_end = control_start + control.len();
            response.payload[control_start..control_end].copy_from_slice(&control);
            response.payload[0..4].copy_from_slice(&(received.read as u32).to_ne_bytes());
            response.payload[4..8].copy_from_slice(&(control.len() as u32).to_ne_bytes());
            response.payload[8..12].copy_from_slice(&0_u32.to_ne_bytes());
            response.payload[12..16].copy_from_slice(&0_u32.to_ne_bytes());
            response.payload_len = control_end as u32;
            response.value = received.read as u64;
            0
        }
        Err(errno) => errno,
    }
}

pub(super) fn send_socket_bytes(request: &NetdIpcRequest, bytes: &[u8]) -> Result<usize, i32> {
    send_socket_message(request, bytes, &[])
}

pub(super) fn send_socket_message(
    request: &NetdIpcRequest,
    bytes: &[u8],
    control: &[u8],
) -> Result<usize, i32> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut state = lock_net_state();
    send_socket_message_in_state(&mut state, request, bytes, control)
}

pub(super) fn send_socket_message_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    bytes: &[u8],
    control: &[u8],
) -> Result<usize, i32> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let peer = {
        let socket = state
            .sockets
            .get(&request.socket_token)
            .ok_or(libc::EBADF)?;
        let UnixSocketState::Connected(connected) = &socket.state else {
            return Err(libc::ENOTCONN);
        };
        if connected.send_closed || connected.peer_read_closed || connected.peer_closed {
            return Err(libc::EPIPE);
        }
        connected.peer
    };
    let control_charge = cmsg_align(control.len());
    if control_charge != 0 {
        if let Some(owner) = state
            .sockets
            .get(&peer)
            .and_then(|socket| match &socket.state {
                UnixSocketState::Connected(connected) => connected.pending_listener,
                _ => None,
            })
        {
            let pending_charge: usize = state
                .sockets
                .values()
                .filter_map(|socket| match &socket.state {
                    UnixSocketState::Connected(connected)
                        if connected.pending_listener == Some(owner) =>
                    {
                        Some(connected.incoming_control_bytes)
                    }
                    _ => None,
                })
                .sum();
            if pending_charge.saturating_add(control_charge) > SOCKET_CONTROL_BUFFER_CAPACITY {
                return Err(libc::EAGAIN);
            }
        }
    }
    let global_room=UNIX_QUEUED_BYTE_CAPACITY.saturating_sub(state.unix_queued_bytes)
        .saturating_sub(control_charge);
    let peer_socket = state.sockets.get_mut(&peer).ok_or(libc::EPIPE)?;
    let UnixSocketState::Connected(peer_connected) = &mut peer_socket.state else {
        return Err(libc::ENOTCONN);
    };
    if peer_connected.recv_closed {
        return Err(libc::EPIPE);
    }
    let room = SOCKET_BUFFER_CAPACITY.saturating_sub(peer_connected.incoming_bytes).min(global_room);
    if room == 0 {
        return Err(libc::EAGAIN);
    }
    if !control.is_empty() && room < bytes.len() {
        return Err(libc::EAGAIN);
    }
    if !control.is_empty()
        && peer_connected
            .incoming_control_bytes
            .saturating_add(control_charge)
            > SOCKET_CONTROL_BUFFER_CAPACITY
    {
        return Err(libc::EAGAIN);
    }
    let write_len = room.min(bytes.len());
    peer_connected.incoming_bytes += write_len;
    if !control.is_empty() {
        peer_connected.incoming_control_bytes = peer_connected
            .incoming_control_bytes
            .saturating_add(control_charge);
    }
    // A plain send carries no message boundary a reader could observe, so it
    // extends the tail segment. Without this, one-byte sends would cost a
    // heap segment each (up to SOCKET_BUFFER_CAPACITY of them per socket).
    match peer_connected.incoming.back_mut() {
        Some(tail) if control.is_empty() => tail.bytes.extend(&bytes[..write_len]),
        _ => peer_connected.incoming.push_back(UnixStreamSegment {
            bytes: bytes[..write_len].iter().copied().collect(),
            control: control.to_vec(),
        }),
    }
    state.set_unix_queued_bytes(state.unix_queued_bytes + write_len + control_charge);
    Ok(write_len)
}

pub(super) fn recv_socket_bytes(
    request: &NetdIpcRequest,
    dest: &mut [u8],
    collect_control: bool,
) -> Result<SocketReceive, i32> {
    if dest.is_empty() {
        return Ok(SocketReceive {
            read: 0,
            control: Vec::new(),
            discarded: Vec::new(),
        });
    }
    let mut state = lock_net_state();
    recv_socket_bytes_in_state(&mut state,request,dest,collect_control)
}

pub(super) fn recv_socket_bytes_in_state(
    state: &mut NetState,
    request: &NetdIpcRequest,
    dest: &mut [u8],
    collect_control: bool,
) -> Result<SocketReceive, i32> {
    let socket = state
        .sockets
        .get_mut(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return Err(libc::ENOTCONN);
    };
    let before = connected.incoming_bytes + connected.incoming_control_bytes;
    let result = recv_connected_bytes(connected, dest, collect_control);
    let consumed = before - (connected.incoming_bytes + connected.incoming_control_bytes);
    state.release_unix_queued_bytes(consumed);
    result
}

pub(super) fn recv_connected_bytes(
    connected: &mut ConnectedState,
    dest: &mut [u8],
    collect_control: bool,
) -> Result<SocketReceive, i32> {
    if dest.is_empty() {
        return Ok(SocketReceive {
            read: 0,
            control: Vec::new(),
            discarded: Vec::new(),
        });
    }
    if connected.incoming_bytes == 0 {
        if connected.peer_closed || connected.peer_write_closed {
            return Ok(SocketReceive {
                read: 0,
                control: Vec::new(),
                discarded: Vec::new(),
            });
        }
        return Err(libc::EAGAIN);
    }
    let mut read = 0usize;
    let mut control = Vec::new();
    let mut discarded = Vec::new();
    while read < dest.len() {
        let Some(segment) = connected.incoming.front_mut() else {
            break;
        };
        if collect_control && !control.is_empty() && !segment.control.is_empty() {
            break;
        }
        if !segment.control.is_empty() {
            let ancillary = core::mem::take(&mut segment.control);
            connected.incoming_control_bytes = connected
                .incoming_control_bytes
                .saturating_sub(cmsg_align(ancillary.len()));
            if collect_control {
                control = ancillary;
            } else {
                let start = cmsg_align(discarded.len());
                discarded.resize(start, 0);
                discarded.extend_from_slice(ancillary.as_slice());
            }
        }
        let count = (dest.len() - read).min(segment.bytes.len());
        let (first, second) = segment.bytes.as_slices();
        let first_len = count.min(first.len());
        dest[read..read + first_len].copy_from_slice(&first[..first_len]);
        dest[read + first_len..read + count].copy_from_slice(&second[..count - first_len]);
        segment.bytes.drain(..count);
        read += count;
        connected.incoming_bytes = connected.incoming_bytes.saturating_sub(count);
        if segment.bytes.is_empty() {
            connected.incoming.pop_front();
        }
        if count == 0 {
            break;
        }
    }
    connected.recv_drain_handoff_armed = true;
    Ok(SocketReceive {
        read,
        control,
        discarded,
    })
}

pub(super) fn pending_recvmsg_control_len(
    connected: &ConnectedState,
    requested: usize,
    passcred: bool,
) -> usize {
    let mut remaining = requested;
    let mut queued_len = 0usize;
    for segment in &connected.incoming {
        if remaining == 0 {
            break;
        }
        if !segment.control.is_empty() {
            queued_len = segment.control.len();
            break;
        }
        remaining = remaining.saturating_sub(segment.bytes.len());
    }
    let credentials_len = if passcred {
        size_of::<linux_abi::LinuxCmsghdr>() + size_of::<linux_abi::LinuxUCred>()
    } else {
        0
    };
    if passcred {
        cmsg_align(queued_len) + credentials_len
    } else {
        queued_len
    }
}

pub(super) fn append_recvmsg_credentials(credentials: Option<Credentials>, mut control: Vec<u8>) -> Vec<u8> {
    let Some(peer_credentials) = credentials else {
        return control;
    };
    let cmsg_len = size_of::<linux_abi::LinuxCmsghdr>() + size_of::<linux_abi::LinuxUCred>();
    let start = cmsg_align(control.len());
    control.resize(start + cmsg_align(cmsg_len), 0);
    let header = linux_abi::LinuxCmsghdr {
        cmsg_len: cmsg_len as u64,
        cmsg_level: linux_abi::SOL_SOCKET as u32,
        cmsg_type: linux_abi::SCM_CREDENTIALS as u32,
    };
    let credentials = linux_abi::LinuxUCred {
        pid: peer_credentials.pid,
        uid: peer_credentials.uid,
        gid: peer_credentials.gid,
    };
    write_plain_old_data(
        &mut control[start..start + size_of::<linux_abi::LinuxCmsghdr>()],
        &header,
    );
    write_plain_old_data(
        &mut control[start + size_of::<linux_abi::LinuxCmsghdr>()..start + cmsg_len],
        &credentials,
    );
    control.truncate(start + cmsg_len);
    control
}

pub(super) fn validate_sendmsg_control(control: &[u8]) -> Result<(), i32> {
    let mut offset = 0usize;
    while offset + size_of::<linux_abi::LinuxCmsghdr>() <= control.len() {
        let header = read_cmsghdr(&control[offset..])?;
        let cmsg_len = usize::try_from(header.cmsg_len).map_err(|_| libc::EINVAL)?;
        if cmsg_len < size_of::<linux_abi::LinuxCmsghdr>()
            || cmsg_len > control.len() - offset
        {
            return Err(libc::EINVAL);
        }
        if header.cmsg_level != linux_abi::SOL_SOCKET as u32 {
            return Err(libc::EOPNOTSUPP);
        }
        match header.cmsg_type {
            value if value == linux_abi::SCM_CREDENTIALS as u32 => {}
            value if value == linux_abi::SCM_RIGHTS as u32 => {}
            _ => return Err(libc::EOPNOTSUPP),
        }
        let aligned_len = cmsg_len
            .checked_add(size_of::<usize>() - 1)
            .ok_or(libc::EINVAL)?
            & !(size_of::<usize>() - 1);
        let aligned_next = offset.checked_add(aligned_len).ok_or(libc::EINVAL)?;
        let unaligned_next = offset.checked_add(cmsg_len).ok_or(libc::EINVAL)?;
        let next = if aligned_next <= control.len() {
            aligned_next
        } else if unaligned_next == control.len() {
            unaligned_next
        } else {
            return Err(libc::EINVAL);
        };
        if next <= offset {
            return Err(libc::EINVAL);
        }
        offset = next;
    }
    if offset != control.len() {
        return Err(libc::EINVAL);
    }
    Ok(())
}

pub(super) fn read_cmsghdr(bytes: &[u8]) -> Result<linux_abi::LinuxCmsghdr, i32> {
    if bytes.len() < size_of::<linux_abi::LinuxCmsghdr>() {
        return Err(libc::EINVAL);
    }
    Ok(linux_abi::LinuxCmsghdr {
        cmsg_len: u64::from_ne_bytes(bytes[0..8].try_into().map_err(|_| libc::EINVAL)?),
        cmsg_level: u32::from_ne_bytes(bytes[8..12].try_into().map_err(|_| libc::EINVAL)?),
        cmsg_type: u32::from_ne_bytes(bytes[12..16].try_into().map_err(|_| libc::EINVAL)?),
    })
}

pub(super) fn write_plain_old_data<T: Copy>(dest: &mut [u8], value: &T) {
    let bytes =
        unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    dest.copy_from_slice(bytes);
}

pub(super) fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}
