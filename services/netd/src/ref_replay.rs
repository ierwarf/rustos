//! Reference mutation (dup/close) operation-id replay log and ACK handling.

use super::*;

pub(super) fn ref_replay_log() -> &'static Mutex<HashMap<(u64, u64, u64), RefReplayEntry>> {
    static LOG: OnceLock<Mutex<HashMap<(u64, u64, u64), RefReplayEntry>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(HashMap::with_capacity(REF_REPLAY_CAPACITY)))
}

pub(super) fn begin_ref_result(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> Result<RefReplayAction, i32> {
    let mut log = ref_replay_log().lock().map_err(|_| libc::EIO)?;
    begin_ref_result_in(&mut log, request, response)
}

pub(super) fn begin_ref_result_in(
    log: &mut HashMap<(u64, u64, u64), RefReplayEntry>,
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
) -> Result<RefReplayAction, i32> {
    if request.operation_hi == 0 && request.operation_lo == 0 {
        return Err(libc::EINVAL);
    }
    // Receive admission matched this PID to the IPC-stamped sender. Kernel
    // PID zero is a separate namespace: a guessed sequence cannot poison or
    // acknowledge another caller's reference mutation.
    let key = (request.pid, request.operation_hi, request.operation_lo);
    if let Some(entry) = log.get(&key) {
        if entry.operation_hi != request.operation_hi
            || entry.operation_lo != request.operation_lo
            || entry.op != request.op
            || entry.socket_token != request.socket_token
        {
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
    let limit = if request.pid == 0 { REF_REPLAY_CAPACITY }
        else { REF_REPLAY_CAPACITY - REF_REPLAY_KERNEL_RESERVE };
    if log.len() >= limit {
        return Err(libc::ENOSPC);
    }
    log.insert(
        key,
        RefReplayEntry {
            operation_hi: request.operation_hi,
            operation_lo: request.operation_lo,
            op: request.op,
            socket_token: request.socket_token,
            status: libc::EINPROGRESS,
            value: 0,
            release: Vec::new(),
            complete: false,
        },
    );
    Ok(RefReplayAction::Execute)
}

pub(super) fn complete_ref_result(request: &NetdIpcRequest, response: &NetdIpcResponse, status: i32) {
    let Ok(mut log) = ref_replay_log().lock() else {
        debug_line("netd: ref replay completion lock poisoned");
        std::process::exit(134);
    };
    let Some(entry) = log.get_mut(&(request.pid, request.operation_hi, request.operation_lo)) else {
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
    acknowledge_ref_result_in(&mut log, request.pid, &NetdRefAckWire {
        operation_hi: request.operation_hi,
        operation_lo: request.operation_lo,
        socket_token: request.socket_token,
        op: acked_op.unwrap(),
        ..NetdRefAckWire::default()
    })
}

pub(super) fn acknowledge_ref_result_in(
    log: &mut HashMap<(u64, u64, u64), RefReplayEntry>, pid: u64, ack: &NetdRefAckWire,
) -> i32 {
    let key = (pid, ack.operation_hi, ack.operation_lo);
    let Some(entry) = log.get(&key) else {
        return 0;
    };
    if entry.op != ack.op || entry.socket_token != ack.socket_token {
        return libc::EPROTO;
    }
    if !entry.complete {
        return libc::EBUSY;
    }
    log.remove(&key);
    0
}

fn decode_ref_ack_batch(request: &NetdIpcRequest) -> Result<Vec<NetdRefAckWire>, i32> {
    let count = usize::try_from(request.arg0).map_err(|_| libc::EINVAL)?;
    if count == 0 || count > NETD_REF_ACK_BATCH_MAX
        || request.payload_len as usize != count * size_of::<NetdRefAckWire>()
        || request.socket_token != 0 || request.operation_hi != 0 || request.operation_lo != 0
        || request.arg1 != 0 || request.arg2 != 0 || request.arg3 != 0
        || request.arg4 != 0 || request.arg5 != 0 || request.status_flags != 0
    {
        return Err(libc::EINVAL);
    }
    let mut batch = Vec::with_capacity(count);
    for bytes in request.payload[..request.payload_len as usize].chunks_exact(size_of::<NetdRefAckWire>()) {
        // SAFETY: exact bounded wire record; every field is an integer and the
        // payload need not be naturally aligned for this copied value.
        let ack = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<NetdRefAckWire>()) };
        if ack.reserved0 != 0 || ack.reserved1 != 0
            || ack.operation_hi == 0 && ack.operation_lo == 0
            || !matches!(ack.op, SYSCALL_OFFLOAD_OP_LINUX_DUP | SYSCALL_OFFLOAD_OP_LINUX_CLOSE)
        {
            return Err(libc::EINVAL);
        }
        batch.push(ack);
    }
    Ok(batch)
}

pub(super) fn acknowledge_ref_batch(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    // Validate the entire envelope before changing the replay log. Per-entry
    // EBUSY/EPROTO remains independent, so one bad neighbour cannot block ACKs.
    let batch = match decode_ref_ack_batch(request) {
        Ok(batch) => batch,
        Err(errno) => return errno,
    };
    let Ok(mut log) = ref_replay_log().lock() else { return libc::EIO; };
    apply_ref_ack_batch(&mut log, request.pid, &batch, response);
    0
}

fn apply_ref_ack_batch(
    log: &mut HashMap<(u64, u64, u64), RefReplayEntry>,
    pid: u64,
    batch: &[NetdRefAckWire],
    response: &mut NetdIpcResponse,
) {
    for (index, ack) in batch.iter().enumerate() {
        let status = acknowledge_ref_result_in(log, pid, ack);
        let start = index * size_of::<i32>();
        response.payload[start..start + size_of::<i32>()].copy_from_slice(&status.to_le_bytes());
    }
    response.value = batch.len() as u64;
    response.payload_len = (batch.len() * size_of::<i32>()) as u32;
    response.reserved0 = 0;
}

pub(super) fn handle_dup(request: &NetdIpcRequest) -> i32 {
    let mut state = lock_net_state();
    if let Some(socket) = state.inet_sockets.get_mut(&request.socket_token) {
        return increment_socket_refs(&mut socket.refs);
    }
    let Some(socket) = state.netlink_sockets.get_mut(&request.socket_token) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(operation: u64, token: u64) -> NetdRefAckWire {
        NetdRefAckWire {
            operation_hi: 7, operation_lo: operation, socket_token: token,
            op: SYSCALL_OFFLOAD_OP_LINUX_CLOSE, ..NetdRefAckWire::default()
        }
    }

    fn entry(ack: NetdRefAckWire, complete: bool) -> RefReplayEntry {
        RefReplayEntry {
            operation_hi: ack.operation_hi, operation_lo: ack.operation_lo,
            op: ack.op, socket_token: ack.socket_token, status: 0, value: 0,
            release: Vec::new(), complete,
        }
    }

    #[test]
    fn kernel_and_user_replay_identities_cannot_poison_or_acknowledge_each_other() {
        let mut log = HashMap::new();
        let user = NetdIpcRequest {
            pid: 17, op: SYSCALL_OFFLOAD_OP_LINUX_CLOSE, socket_token: 9,
            operation_hi: 7, operation_lo: 1, ..NetdIpcRequest::default()
        };
        let kernel = NetdIpcRequest { pid: 0, ..user };
        for request in [&user, &kernel] {
            assert!(matches!(begin_ref_result_in(&mut log, request,
                &mut NetdIpcResponse::default()), Ok(RefReplayAction::Execute)));
            log.get_mut(&(request.pid, 7, 1)).unwrap().complete = true;
        }
        assert_eq!(acknowledge_ref_result_in(&mut log, 17, &ack(1, 9)), 0);
        assert!(log.contains_key(&(0, 7, 1)));
        assert!(!log.contains_key(&(17, 7, 1)));
        assert_eq!(acknowledge_ref_result_in(&mut log, 99, &ack(1, 9)), 0);
        assert!(log.contains_key(&(0, 7, 1)));
        assert_eq!(acknowledge_ref_result_in(&mut log, 0, &ack(1, 9)), 0);
        assert!(log.is_empty());
    }

    #[test]
    fn ack_batch_progress_is_independent_and_repeating_a_lost_reply_is_idempotent() {
        let mut log = HashMap::new();
        for (operation, complete) in [(1, true), (2, false), (3, true)] {
            log.insert((0, 7, operation), entry(ack(operation, 9), complete));
        }
        let batch = [ack(1, 9), ack(2, 9), ack(3, 10), ack(4, 9)];
        for _ in 0..2 {
            let mut response = NetdIpcResponse::default();
            apply_ref_ack_batch(&mut log, 0, &batch, &mut response);
            let statuses: Vec<i32> = response.payload[..response.payload_len as usize]
                .chunks_exact(4).map(|bytes| i32::from_le_bytes(bytes.try_into().unwrap())).collect();
            assert_eq!(statuses, [0, libc::EBUSY, libc::EPROTO, 0]);
            assert_eq!(response.value, 4);
            assert_eq!(log.len(), 2);
            assert!(log.contains_key(&(0, 7, 2)) && log.contains_key(&(0, 7, 3)));
        }
    }

    #[test]
    fn ack_batch_rejects_truncated_oversized_and_reserved_operands_before_mutation() {
        let mut request = NetdIpcRequest {
            op: NETD_IPC_OP_REF_ACK_BATCH, arg0: 1,
            payload_len: size_of::<NetdRefAckWire>() as u32, ..NetdIpcRequest::default()
        };
        let mut encoded = ack(1, 9);
        // SAFETY: the exact initialized integer-only C record fits the payload.
        unsafe { core::ptr::write_unaligned(request.payload.as_mut_ptr().cast::<NetdRefAckWire>(), encoded); }
        assert_eq!(decode_ref_ack_batch(&request).unwrap(), [encoded]);
        request.arg0 = u64::MAX;
        assert!(matches!(decode_ref_ack_batch(&request), Err(libc::EINVAL)));
        request.arg0 = 1;
        request.payload_len -= 1;
        assert!(matches!(decode_ref_ack_batch(&request), Err(libc::EINVAL)));
        request.payload_len += 1;
        encoded.reserved0 = 1;
        // SAFETY: same bounded initialized wire storage as above.
        unsafe { core::ptr::write_unaligned(request.payload.as_mut_ptr().cast::<NetdRefAckWire>(), encoded); }
        assert!(matches!(decode_ref_ack_batch(&request), Err(libc::EINVAL)));
    }

    #[test]
    fn non_kernel_backlog_cannot_consume_final_release_reserve() {
        let mut log = HashMap::new();
        for operation in 1..=(REF_REPLAY_CAPACITY - REF_REPLAY_KERNEL_RESERVE) as u64 {
            log.insert((17, 7, operation), entry(ack(operation, 9), true));
        }
        let request = NetdIpcRequest {
            pid: 17, op: SYSCALL_OFFLOAD_OP_LINUX_CLOSE, socket_token: 9,
            operation_hi: 7, operation_lo: u64::MAX, ..NetdIpcRequest::default()
        };
        assert!(matches!(begin_ref_result_in(&mut log, &request,
            &mut NetdIpcResponse::default()), Err(libc::ENOSPC)));
        let kernel = NetdIpcRequest { pid: 0, ..request };
        assert!(matches!(begin_ref_result_in(&mut log, &kernel,
            &mut NetdIpcResponse::default()), Ok(RefReplayAction::Execute)));
    }
}
