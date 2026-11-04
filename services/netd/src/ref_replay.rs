//! Reference mutation (dup/close) operation-id replay log and ACK handling.

use super::*;

pub(super) fn ref_replay_log() -> &'static Mutex<VecDeque<RefReplayEntry>> {
    static LOG: OnceLock<Mutex<VecDeque<RefReplayEntry>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(VecDeque::with_capacity(REF_REPLAY_CAPACITY)))
}

pub(super) fn begin_ref_result(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> Result<RefReplayAction, i32> {
    if request.operation_hi == 0 && request.operation_lo == 0 {
        return Err(libc::EINVAL);
    }
    let mut log = ref_replay_log().lock().map_err(|_| libc::EIO)?;
    if let Some(entry) = log.iter().find(|entry| {
        entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
    }) {
        if entry.op != request.op || entry.socket_token != request.socket_token {
            return Err(libc::EPROTO);
        }
        if !entry.complete {
            return Err(libc::EBUSY);
        }
        response.value = entry.value;
        response.payload[..entry.release.len()].copy_from_slice(entry.release.as_slice());
        response.payload_len = entry.release.len() as u32;
        response.reserved0 = entry.release.len() as u32;
        return Ok(RefReplayAction::Replay(entry.status));
    }
    if log.len() >= REF_REPLAY_CAPACITY {
        reclaim_stale_ref_results(&mut log, StdInstant::now());
        if log.len() >= REF_REPLAY_CAPACITY {
            return Err(libc::ENOSPC);
        }
    }
    log.push_back(RefReplayEntry {
        operation_hi: request.operation_hi,
        operation_lo: request.operation_lo,
        op: request.op,
        socket_token: request.socket_token,
        status: libc::EINPROGRESS,
        value: 0,
        release: Vec::new(),
        complete: false,
        admitted: StdInstant::now(),
    });
    Ok(RefReplayAction::Execute)
}

/// Drops completed results older than `REF_REPLAY_MIN_RETENTION`. Incomplete
/// entries belong to an executing request and are never reclaimed.
pub(super) fn reclaim_stale_ref_results(log: &mut VecDeque<RefReplayEntry>, now: StdInstant) {
    log.retain(|entry| {
        !entry.complete
            || now
                .checked_duration_since(entry.admitted)
                .is_none_or(|age| age < REF_REPLAY_MIN_RETENTION)
    });
}

pub(super) fn complete_ref_result(request: &NetdIpcRequest, response: &NetdIpcResponse, status: i32) {
    let Ok(mut log) = ref_replay_log().lock() else {
        debug_line("netd: ref replay completion lock poisoned");
        std::process::exit(134);
    };
    let Some(entry) = log.iter_mut().find(|entry| {
        entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
    }) else {
        debug_line("netd: ref replay reservation lost");
        std::process::exit(134);
    };
    entry.status = status;
    entry.value = response.value;
    // `reserved0` is handler-written; clamp so a handler bug cannot turn the
    // replay bookkeeping into an out-of-range slice panic that aborts netd.
    let release_len = (response.reserved0 as usize).min(response.payload.len());
    entry.release.clear();
    entry
        .release
        .extend_from_slice(&response.payload[..release_len]);
    entry.complete = true;
}

pub(super) fn acknowledge_ref_result(request: &NetdIpcRequest) -> i32 {
    // `try_from`, not `as u16`: a high-bit-polluted operand must not alias a
    // real op code.
    let acked_op = u16::try_from(request.arg0).ok().filter(|op| {
        matches!(
            *op,
            SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        )
    });
    if request.operation_hi == 0 && request.operation_lo == 0 || acked_op.is_none() {
        return libc::EINVAL;
    }
    let Ok(mut log) = ref_replay_log().lock() else {
        return libc::EIO;
    };
    let Some(index) = log.iter().position(|entry| {
        entry.operation_hi == request.operation_hi && entry.operation_lo == request.operation_lo
    }) else {
        return 0;
    };
    let entry = &log[index];
    if Some(entry.op) != acked_op || entry.socket_token != request.socket_token {
        return libc::EPROTO;
    }
    if !entry.complete {
        return libc::EBUSY;
    }
    log.remove(index);
    0
}

pub(super) fn handle_dup(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        return increment_socket_refs(&mut socket.refs);
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    increment_socket_refs(&mut socket.refs)
}

pub(super) fn increment_socket_refs(refs: &mut usize) -> i32 {
    match refs.checked_add(1) {
        Some(next) if *refs != 0 => {
            *refs = next;
            0
        }
        _ => libc::EOVERFLOW,
    }
}
