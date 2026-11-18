//! AF_UNIX `SOCK_SEQPACKET`: one queued segment is exactly one message.
//!
//! - **Owner:** `netd` owns message boundaries and per-message ancillary
//!   custody; the kernel owns only the fd/ticket substrate and its stream
//!   position counters.
//! - **Boundary:** payload length, control bytes and receive buffer sizes are
//!   untrusted. A message is queued whole or refused (`EMSGSIZE`, `EAGAIN`).
//! - **Lifecycle:** send appends one `UnixStreamSegment` (never merged with the
//!   tail); recv pops exactly the front segment, whatever the buffer size.
//!   `SCM_RIGHTS` tickets travel with the segment that carried them.
//! - **Failure:** an oversized message is `EMSGSIZE`; a full queue is `EAGAIN`;
//!   a short receive buffer discards the tail and reports `MSG_TRUNC`.
//! - **Stream position:** each record advances by max(1, message length).
//!   `discarded_tail` carries truncated bytes; RECORD_PRESENT distinguishes
//!   a consumed empty record from EOF.
//! - **Forbidden:** coalescing messages, splitting a message across receives,
//!   or treating a zero-length message as end of stream.

use super::*;

/// Per-socket bound on queued messages. Bytes are bounded by
/// `SOCKET_BUFFER_CAPACITY`; this stops zero-length messages from queueing
/// unbounded segment headers.
pub(super) const SEQPACKET_MAX_QUEUED_MESSAGES: usize = 1024;

/// Validates a message before any queue is touched.
pub(super) fn validate_send(bytes: &[u8], _control: &[u8]) -> Result<(), i32> {
    if bytes.len() > SOCKET_BUFFER_CAPACITY {
        return Err(libc::EMSGSIZE);
    }
    Ok(())
}

/// Dequeues exactly the front message. EOF (`read == 0`, empty queue) is only
/// reported once the peer is closed/shut down and the queue is drained; a
/// queued zero-length message is a real message and returns 0 bytes.
pub(super) fn recv_message(
    connected: &mut ConnectedState,
    dest: &mut [u8],
    collect_control: bool,
) -> Result<SocketReceive, i32> {
    let Some(segment) = connected.incoming.pop_front() else {
        if connected.peer_closed || connected.peer_write_closed {
            return Ok(SocketReceive::default());
        }
        return Err(libc::EAGAIN);
    };
    let total = segment.bytes.len();
    let read = total.min(dest.len());
    let (first, second) = segment.bytes.as_slices();
    let first_len = read.min(first.len());
    dest[..first_len].copy_from_slice(&first[..first_len]);
    dest[first_len..read].copy_from_slice(&second[..read - first_len]);
    connected.incoming_bytes = connected.incoming_bytes.saturating_sub(total);
    let mut control = Vec::new();
    let mut discarded = Vec::new();
    if !segment.control.is_empty() {
        connected.incoming_control_bytes = connected
            .incoming_control_bytes
            .saturating_sub(cmsg_align(segment.control.len()));
        if collect_control {
            control = segment.control;
        } else {
            discarded = segment.control;
        }
    }
    connected.recv_drain_handoff_armed = true;
    Ok(SocketReceive {
        record_present: true,
        read,
        control,
        discarded,
        extra: total - read,
        truncated: total > read,
    })
}

/// `MSG_PEEK`: front message only. Returns `(copied, has_control, total_len)`.
pub(super) fn peek_message(
    state: &NetState,
    token: u64,
    dest: &mut [u8],
) -> Result<(usize, bool, usize), i32> {
    let socket = state.sockets.get(&token).ok_or(libc::EBADF)?;
    let UnixSocketState::Connected(connected) = &socket.state else {
        return Err(libc::ENOTCONN);
    };
    let Some(segment) = connected.incoming.front() else {
        if connected.peer_closed || connected.peer_write_closed {
            return Ok((0, false, 0));
        }
        return Err(libc::EAGAIN);
    };
    let total = segment.bytes.len();
    let read = total.min(dest.len());
    let (first, second) = segment.bytes.as_slices();
    let first_len = read.min(first.len());
    dest[..first_len].copy_from_slice(&first[..first_len]);
    dest[first_len..read].copy_from_slice(&second[..read - first_len]);
    Ok((read, !segment.control.is_empty(), total))
}

/// Returns `(syscall return value, discarded_tail extra)` for a receive.
///
/// Linux returns the real message length when `MSG_TRUNC` is in the *receive*
/// flags (the kernel then advances its position by that value alone) and the
/// copied length otherwise (the kernel adds `discarded_tail` for the discarded tail).
pub(super) fn receive_accounting(flags: u64, received: &SocketReceive) -> (u64, u32) {
    if received.extra == 0 {
        (received.read as u64, 0)
    } else if flags & libc::MSG_TRUNC as u64 != 0 {
        ((received.read + received.extra) as u64, 0)
    } else {
        (received.read as u64, received.extra as u32)
    }
}
