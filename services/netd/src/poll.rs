//! Socket readiness queries: single, batch and blocking wait.

use super::*;

/// `NETD_IPC_OP_POLL_BATCH`: evaluate every requested socket under one state
/// lock acquisition and one stack advance. A socket that cannot be evaluated
/// reports its own errno; it never fails its neighbours.
pub(super) fn handle_poll_batch(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    const QUERY: usize = size_of::<NetdPollQueryWire>();
    const RESULT: usize = size_of::<NetdPollResultWire>();
    let len = request.payload_len as usize;
    let count = len / QUERY;
    if len == 0
        || len % QUERY != 0
        || count > NETD_POLL_BATCH_MAX
        || count * RESULT > response.payload.len()
    {
        return libc::EINVAL;
    }
    let payload = &request.payload[..len];
    for bytes in payload.chunks_exact(QUERY) {
        let socket_token = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let reserved0 = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        if socket_token == 0 || reserved0 != 0 {
            return libc::EINVAL;
        }
    }
    let mut state = lock_net_state();
    if payload.chunks_exact(QUERY).any(|bytes| {
        let socket_token = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        state.inet_sockets.contains_key(&socket_token)
    }) {
        if let Ok(stack) = state.inet_stack() {
            stack.poll_budget(8);
        }
    }
    for (index, bytes) in payload.chunks_exact(QUERY).enumerate() {
        let socket_token = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let events = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let outcome = if state.inet_sockets.contains_key(&socket_token) {
            inet_socket_revents(&mut state, socket_token, events)
        } else {
            netlink::refresh_for_member(&mut state, socket_token);
            unix_socket_revents(&state, socket_token, events)
        };
        let result = match outcome {
            Ok(revents) => NetdPollResultWire {
                generation: state.socket_readiness_generation(socket_token),
                revents,
                status: 0,
            },
            Err(errno) => NetdPollResultWire {
                generation: 0,
                revents: 0,
                status: errno,
            },
        };
        let offset = index * RESULT;
        response.payload[offset..offset + 8].copy_from_slice(&result.generation.to_le_bytes());
        response.payload[offset + 8..offset + 12].copy_from_slice(&result.revents.to_le_bytes());
        response.payload[offset + 12..offset + 16].copy_from_slice(&result.status.to_le_bytes());
    }
    response.value = count as u64;
    response.payload_len = (count * RESULT) as u32;
    0
}

pub(super) fn handle_poll_socket(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let mut state = lock_net_state();
    let outcome = if state.inet_sockets.contains_key(&request.socket_token) {
        match state.inet_stack() {
            Ok(stack) => { stack.poll_budget(8); }
            Err(errno) => return errno,
        }
        inet_socket_revents(&mut state, request.socket_token, request.arg1 as u32)
    } else {
        netlink::refresh_for_member(&mut state, request.socket_token);
        unix_socket_revents(&state, request.socket_token, request.arg1 as u32)
    };
    match outcome {
        Ok(revents) => {
            response.value = revents as u64;
            let generation = state.socket_readiness_generation(request.socket_token);
            response.payload[..8].copy_from_slice(&generation.to_le_bytes());
            response.payload_len = 8;
            0
        }
        Err(errno) => errno,
    }
}

pub(super) fn unix_socket_revents(state: &NetState, token: u64, requested: u32) -> Result<u32, i32> {
    let socket = state.sockets.get(&token).ok_or(libc::EBADF)?;
    Ok(unix_socket_revents_for(state, socket, requested))
}

pub(super) fn unix_socket_revents_for(state: &NetState, socket: &UnixSocket, requested: u32) -> u32 {
    let mut revents = 0_u32;
    match &socket.state {
        UnixSocketState::Listening { pending, .. } => {
            if requested & linux_abi::POLLIN as u32 != 0 && !pending.is_empty() {
                revents |= linux_abi::POLLIN as u32;
            }
        }
        UnixSocketState::Connected(connected) => {
            let peer_connected =
                state
                    .sockets
                    .get(&connected.peer)
                    .and_then(|peer| match &peer.state {
                        UnixSocketState::Connected(peer_connected) => Some(peer_connected),
                        _ => None,
                    });
            let peer_limit = state.sockets.get(&connected.peer).map_or(SOCKET_BUFFER_CAPACITY, |peer| {
                if peer.kind.preserves_message_boundaries() { (peer.options.recv_buffer as usize).min(SOCKET_BUFFER_CAPACITY) } else { SOCKET_BUFFER_CAPACITY }
            });
            let peer_seqpacket = state
                .sockets
                .get(&connected.peer)
                .is_some_and(|peer| peer.kind.preserves_message_boundaries());
            if requested & linux_abi::POLLIN as u32 != 0
                && (connected.incoming_bytes != 0
                    || !connected.incoming.is_empty()
                    || connected.peer_write_closed
                    || connected.recv_closed
                    || connected.peer_closed)
            {
                revents |= linux_abi::POLLIN as u32;
            }
            if requested & linux_abi::POLLOUT as u32 != 0 {
                let writable = state.unix_queued_bytes < UNIX_QUEUED_BYTE_CAPACITY
                    && !connected.send_closed
                    && !connected.peer_read_closed
                    && !connected.peer_closed
                    && peer_connected.is_some_and(|peer| {
                        !peer.recv_closed
                            && peer.incoming_bytes < peer_limit
                            && !(peer_seqpacket
                                && peer.incoming.len() >= SEQPACKET_MAX_QUEUED_MESSAGES)
                            && peer.incoming_control_bytes < SOCKET_CONTROL_BUFFER_CAPACITY
                    });
                if writable {
                    revents |= linux_abi::POLLOUT as u32;
                }
            }
            if requested & linux_abi::POLLRDHUP as u32 != 0
                && (connected.peer_write_closed || connected.recv_closed || connected.peer_closed)
            {
                revents |= linux_abi::POLLRDHUP as u32;
            }
            // Linux reports POLLHUP once both directions are shut down, whether
            // by this end, by the peer, or by one of each.
            let read_shut = connected.recv_closed || connected.peer_write_closed;
            let write_shut = connected.send_closed || connected.peer_read_closed;
            if connected.peer_closed || (read_shut && write_shut) {
                revents |= linux_abi::POLLHUP as u32;
            }
            if peer_connected.is_none() {
                revents |= linux_abi::POLLERR as u32;
            }
        }
        UnixSocketState::Dgram(dgram) => {
            if requested & linux_abi::POLLIN as u32 != 0
                && (!dgram.incoming.is_empty() || dgram.recv_closed)
            {
                revents |= linux_abi::POLLIN as u32;
            }
            if requested & linux_abi::POLLOUT as u32 != 0 {
                let peer_ok = match dgram.peer {
                    Some(peer_token) => state.sockets.get(&peer_token).is_some_and(|peer| {
                        let peer_limit = (peer.options.recv_buffer as usize).min(SOCKET_BUFFER_CAPACITY);
                        matches!(&peer.state, UnixSocketState::Dgram(pd)
                            if !pd.recv_closed
                                && pd.incoming_bytes < peer_limit
                                && pd.incoming.len() < UNIX_DGRAM_MAX_QUEUED_MESSAGES
                                && pd.incoming_control_bytes < SOCKET_CONTROL_BUFFER_CAPACITY)
                    }),
                    None => true,
                };
                if !dgram.send_closed
                    && !dgram.peer_closed
                    && state.unix_queued_bytes < UNIX_QUEUED_BYTE_CAPACITY
                    && peer_ok
                {
                    revents |= linux_abi::POLLOUT as u32;
                }
            }
            if requested & linux_abi::POLLRDHUP as u32 != 0 && dgram.recv_closed {
                revents |= linux_abi::POLLRDHUP as u32;
            }
            if dgram.peer_closed {
                revents |= linux_abi::POLLERR as u32;
            }
        }
        UnixSocketState::Netlink(netlink) => {
            if requested & linux_abi::POLLIN as u32 != 0 && netlink.readable() {
                revents |= linux_abi::POLLIN as u32;
            }
            if requested & linux_abi::POLLOUT as u32 != 0 {
                revents |= linux_abi::POLLOUT as u32;
            }
        }
        UnixSocketState::Idle => {
            revents |= linux_abi::POLLERR as u32;
        }
    }
    revents
}

pub(super) fn wait_for_socket_readiness(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if !inet_socket_exists(request.socket_token) {
        return libc::EBADF;
    }
    for attempt in 0..INET_IO_POLL_BUDGET {
        if let Err(errno) = request_deadline_remaining_ns(request) {
            return errno;
        }
        let epoch = state_waiter_epoch();
        let status = handle_poll_socket(request, response);
        if status != 0 || response.value != 0 {
            return status;
        }
        if let Err(errno) =
            wait_state_within_request_deadline(epoch, request, blocking_retry_backoff(attempt))
        {
            return errno;
        }
    }
    libc::EAGAIN
}
