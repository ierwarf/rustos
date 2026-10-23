//! Synchronous IPC round-trip probes.
//!
//! Two intra-process probes run the same client against two server loops. The
//! rendezvous fastpath requires a receiver already parked on the endpoint,
//! exactly as seL4's does, so a server that returns to ring3 between its reply
//! and its next receive loses that race to the caller it just woke. Every
//! production RustOS service uses the fused reply-and-receive call for that
//! reason; keeping both loops here measures the fastpath and its fallback
//! separately instead of averaging them into one number.
//!
//! - **Boundary:** every probe uses an already-published ABI. No probe here
//!   may use a bench-only kernel path.
//! - **Forbidden:** no formatting or logging inside a measured interval.

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    IpcReplyRecvWithSenderArgs, IPC_ABI_VERSION, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV_WITH_SENDER, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER, SYS_RUSTOS_IPC_TRY_RECV,
    ipc_register_syscall, SYS_RUSTOS_IPC_REGISTER_CALL, SYS_RUSTOS_IPC_REGISTER_RECV,
    SYS_RUSTOS_IPC_REGISTER_REPLY_RECV, SYS_RUSTOS_IPC_REGISTER_REPLY,
};

use crate::{
    measure, measure_stamped, report, skip, summarize, syscall0, syscall1, syscall3, syscall4, syscall5, syscall6,
    tsc, IPC_ITERS, SYSCALL_ITERS, SYS_LINUX_GETUID, WARMUP,
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct BenchMsg {
    pub(crate) op: u64,
    pub(crate) seq: u64,
}

pub(crate) const BENCH_OP_PING: u64 = 0;
pub(crate) const BENCH_OP_STOP: u64 = 1;
const _: [(); 16] = [(); size_of::<BenchMsg>()];

fn valid_round_trip_reply(status: i64, request: &BenchMsg, response: &BenchMsg) -> bool {
    status == size_of::<BenchMsg>() as i64
        && response.op == request.op
        && response.seq == request.seq
}

fn round_trip_transcript_complete(
    warmup_ok: bool,
    stop_verified: bool,
    joined: bool,
    samples: usize,
) -> bool {
    warmup_ok && stop_verified && joined && samples == IPC_ITERS
}

#[inline(always)]
fn validated_round_trip_sample(
    request: &BenchMsg,
    response: &mut BenchMsg,
    call: &mut impl FnMut(&BenchMsg, &mut BenchMsg) -> i64,
    mut stamp: impl FnMut() -> u64,
) -> Option<(u64, u64)> {
    let start = stamp();
    let status = call(request, response);
    let end = stamp();
    // Validation is outside the measured interval. No invalid transaction may
    // contribute a latency sample, including a success with stale/short bytes.
    valid_round_trip_reply(status, request, response).then_some((start, end))
}

fn empty_receive_status_valid(status: i64, reply_cap: u64) -> bool {
    status == -(libc::EAGAIN as i64) && reply_cap == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_receive_requires_exact_would_block_and_no_reply_capability() {
        assert!(empty_receive_status_valid(-11, 0));
        for status in [-38, -22, -9, -1, 0, 16] {
            assert!(!empty_receive_status_valid(status, 0), "status {status} is not an empty receive");
        }
        assert!(!empty_receive_status_valid(-11, 0x10001));
    }

    #[test]
    fn incomplete_warmup_stop_join_or_sample_set_never_becomes_a_result() {
        assert!(round_trip_transcript_complete(true, true, true, IPC_ITERS));
        assert!(!round_trip_transcript_complete(
            false, true, true, IPC_ITERS
        ));
        assert!(!round_trip_transcript_complete(
            true, false, true, IPC_ITERS
        ));
        assert!(!round_trip_transcript_complete(
            true, true, false, IPC_ITERS
        ));
        assert!(!round_trip_transcript_complete(
            true,
            true,
            true,
            IPC_ITERS - 1
        ));
        assert!(!round_trip_transcript_complete(
            true,
            true,
            true,
            IPC_ITERS + 1
        ));
    }

    #[test]
    fn actual_sample_path_stamps_call_before_validating_and_never_records_bad_echo() {
        use std::cell::Cell;
        let request = BenchMsg {
            op: BENCH_OP_PING,
            seq: 91,
        };
        for fault in 0..5 {
            let order = Cell::new(0);
            let mut response = BenchMsg::default();
            let mut call = |sent: &BenchMsg, received: &mut BenchMsg| {
                assert_eq!(order.replace(2), 1);
                *received = *sent;
                match fault {
                    1 => received.seq -= 1,
                    2 => received.op = BENCH_OP_STOP,
                    _ => {}
                }
                match fault {
                    3 => 0,
                    4 => -110,
                    _ => 16,
                }
            };
            let result = validated_round_trip_sample(&request, &mut response, &mut call, || {
                let old = order.get();
                assert!(old == 0 || old == 2);
                order.set(old + 1);
                if old == 0 {
                    100
                } else {
                    300
                }
            });
            assert_eq!(order.get(), 3);
            assert_eq!(result, (fault == 0).then_some((100, 300)));
        }
    }

    #[test]
    fn actual_round_trip_reply_requires_exact_bytes_operation_and_sequence() {
        let request = BenchMsg {
            op: BENCH_OP_PING,
            seq: 0x1234_5678_9abc_def0,
        };
        let response = request;
        assert!(valid_round_trip_reply(16, &request, &response));
        for status in [-110, -1, 0, 15, 17, i64::MAX] {
            assert!(
                !valid_round_trip_reply(status, &request, &response),
                "status={status}"
            );
        }
        assert!(!valid_round_trip_reply(
            16,
            &request,
            &BenchMsg {
                op: BENCH_OP_STOP,
                ..response
            }
        ));
        assert!(!valid_round_trip_reply(
            16,
            &request,
            &BenchMsg {
                seq: response.seq - 1,
                ..response
            }
        ));
        assert!(!valid_round_trip_reply(16, &request, &BenchMsg::default()));
    }
}

/// Timestamps the server stamps inside one round trip.
///
/// The client is blocked in `call` from before `RECV_TSC` is written until
/// after `REPLY_TSC` is, so a plain Release/Acquire pair is enough: the reply
/// that unblocks the client is published after both stores.
static RECV_TSC: AtomicU64 = AtomicU64::new(0);
static REPLY_TSC: AtomicU64 = AtomicU64::new(0);

pub(crate) fn bench_server(endpoint: u64) {
    let mut request = BenchMsg::default();
    loop {
        let mut reply_cap: u64 = 0;
        let mut sender_pid: u64 = 0;
        let mut sender_tid: u64 = 0;
        let received = unsafe {
            syscall6(
                SYS_RUSTOS_IPC_RECV_WITH_SENDER,
                endpoint,
                (&mut request as *mut BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (&mut reply_cap as *mut u64) as u64,
                (&mut sender_pid as *mut u64) as u64,
                (&mut sender_tid as *mut u64) as u64,
            )
        };
        if received != size_of::<BenchMsg>() as i64 {
            return;
        }
        RECV_TSC.store(tsc(), Ordering::Relaxed);
        let response = request;
        let stop = request.op == BENCH_OP_STOP;
        REPLY_TSC.store(tsc(), Ordering::Release);
        unsafe {
            syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
            );
        }
        if stop {
            return;
        }
    }
}

/// The server loop every production RustOS service runs: one fused
/// reply-and-receive syscall instead of a reply followed by a receive.
///
/// The distinction is not stylistic. The rendezvous fastpath requires a
/// receiver already parked on the endpoint, exactly as seL4's does, and a
/// server that returns to ring3 between its reply and its next receive loses
/// that race to the caller it just woke. `syscalld`, `loaderd`, and `inputd`
/// all use the fused call; [`bench_server`] deliberately does not, so the two
/// probes measure the fastpath and the fallback separately rather than
/// averaging them.
fn bench_server_reply_recv(endpoint: u64) {
    let mut request = BenchMsg::default();
    let mut reply_cap: u64 = 0;
    let mut sender_pid: u64 = 0;
    let mut sender_tid: u64 = 0;
    // The first turn has no reply to send, so it is an ordinary receive.
    let received = unsafe {
        syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            (&mut request as *mut BenchMsg) as u64,
            size_of::<BenchMsg>() as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        )
    };
    if received != size_of::<BenchMsg>() as i64 {
        return;
    }
    loop {
        RECV_TSC.store(tsc(), Ordering::Relaxed);
        let response = request;
        let stop = request.op == BENCH_OP_STOP;
        REPLY_TSC.store(tsc(), Ordering::Release);
        if stop {
            unsafe {
                syscall3(
                    SYS_RUSTOS_IPC_REPLY,
                    reply_cap,
                    (&response as *const BenchMsg) as u64,
                    size_of::<BenchMsg>() as u64,
                );
            }
            return;
        }
        let args = IpcReplyRecvWithSenderArgs {
            abi_version: IPC_ABI_VERSION,
            flags: 0,
            reserved0: 0,
            endpoint,
            reply_cap,
            response_ptr: (&response as *const BenchMsg) as u64,
            response_len: size_of::<BenchMsg>() as u64,
            request_ptr: (&mut request as *mut BenchMsg) as u64,
            request_capacity: size_of::<BenchMsg>() as u64,
            next_reply_cap_ptr: (&mut reply_cap as *mut u64) as u64,
            sender_pid_ptr: (&mut sender_pid as *mut u64) as u64,
            sender_tid_ptr: (&mut sender_tid as *mut u64) as u64,
            reserved1: 0,
        };
        let result = unsafe {
            syscall1(
                SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
                (&args as *const IpcReplyRecvWithSenderArgs) as u64,
            )
        };
        if result != size_of::<BenchMsg>() as i64 {
            return;
        }
    }
}

/// Same-address-space round trip: this isolates the kernel IPC mechanism from
/// the address-space switch, so the gap against the cross-process probe is the
/// switch and the second process's scheduling cost.
pub(crate) fn probe_ipc_intra_process(tsc_khz: u64) {
    measure_intra_process_round_trip(
        tsc_khz,
        "ipc_rt_intra_process",
        [
            "ipc_split_call_to_recv",
            "ipc_split_server_body",
            "ipc_split_reply_to_return",
        ],
        bench_server,
        false,
    );
}

/// The same round trip against a server that replies and receives in one
/// syscall, which is what every production service does.
pub(crate) fn probe_ipc_intra_process_reply_recv(tsc_khz: u64) {
    measure_intra_process_round_trip(
        tsc_khz,
        "ipc_rt_intra_process_reply_recv",
        [
            "ipc_split_reply_recv_call_to_recv",
            "ipc_split_reply_recv_server_body",
            "ipc_split_reply_recv_reply_to_return",
        ],
        bench_server_reply_recv,
        false,
    );
}

pub(crate) fn probe_state_checkpoint(name: &str, tsc_khz: u64) {
    let legs = [format!("{name}_to_recv"), format!("{name}_body"), format!("{name}_to_return")];
    measure_intra_process_round_trip(
        tsc_khz, name, [&legs[0], &legs[1], &legs[2]], bench_server_reply_recv, false,
    );
}

/// Same payload, stamps, warmups, counts and echo/STOP/join oracle as the
/// buffer control. This measures two threads in ONE address space, not CR3.
pub(crate) fn probe_ipc_register_reply_recv(tsc_khz: u64) {
    if !crate::ipc_service::preflight() || !register_transport_preflight() {
        skip("ipc_rt_intra_process_register_reply_recv", "register-preflight-failed");
        return;
    }
    measure_intra_process_round_trip(tsc_khz, "ipc_rt_intra_process_register_reply_recv",
        ["ipc_split_register_call_to_recv", "ipc_split_register_server_body", "ipc_split_register_reply_to_return"],
        bench_server_register_reply_recv, true);
}

// Outside all timing windows: length/padding, queued/fast interoperability,
// rejected fused endpoint before one-shot consume, and duplicate reply refusal.
fn register_transport_preflight() -> bool {
    use rustos_user_abi::syscall::{ipc_register_pack, ipc_register_unpack};
    unsafe fn ipc_register_syscall(operation: u64, endpoint: u64, len: u64,
        words: [u64; 2], reply: u64) -> rustos_user_abi::syscall::IpcRegisterResult {
        let (result, valid) = unsafe { crate::ipc_simd::checked_syscall(
            operation, endpoint, len, words, reply, true) };
        assert!(valid, "register IPC leaked FP data or changed controls");
        result
    }
    // Ordinary Linux syscalls still preserve data, unlike this opt-in ABI.
    assert!(unsafe { crate::ipc_simd::checked_syscall(39, 0, 0, [0; 2], 0, false) }.1);
    for (operation, len) in [(SYS_RUSTOS_IPC_REGISTER_CALL, 17),
        (SYS_RUSTOS_IPC_REGISTER_CALL, 0), (SYS_RUSTOS_IPC_REGISTER_RECV, 1)] {
        let result = unsafe { ipc_register_syscall(operation, 0, len, [u64::MAX; 2], 0) };
        if result.status != -22 || result.words != [0; 2] || result.reply != 0
            || result.sender_pid != 0 || result.sender_tid != 0 { return false; }
    }
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 { return false; }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let mut received = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_RECV, endpoint, 0, [0; 2], 0) };
        for len in 1..=16 {
            assert_eq!(received.status, len);
            assert_ne!(received.reply, 0);
            assert_ne!(received.sender_pid, 0);
            assert_ne!(received.sender_tid, 0);
            let data = ipc_register_unpack(received.words);
            assert!(data[..len as usize].iter().all(|&b| b == len as u8));
            assert!(data[len as usize..].iter().all(|&b| b == 0));
            let denied = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
                0, len as u64, received.words, received.reply) };
            assert!(denied.status < 0 && denied.status > -4096);
            assert_eq!(denied.words, [0; 2]);
            if len == 16 {
                let cap = received.reply;
                let result = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_REPLY, cap,
                    len as u64, received.words, 0) };
                assert_eq!(result.status, 0);
                let duplicate = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_REPLY, cap,
                    len as u64, received.words, 0) };
                assert!(duplicate.status < 0);
            } else {
                received = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
                    endpoint, len as u64, received.words, received.reply) };
            }
        }
    });
    // The server delays its first receive so the first call exercises queued
    // fallback; subsequent fused receives exercise the parked-receiver path.
    let mut ok = true;
    for len in 1..=16 {
        let data = [len as u8; 16];
        let words = ipc_register_pack(&data[..len]).unwrap();
        let result = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_CALL,
            endpoint, len as u64, words, 0) };
        ok &= result.status == len as i64 && result.words == words
            && result.reply == 0 && result.sender_pid == 0 && result.sender_tid == 0;
        if !ok { break; }
    }
    if !server.join().is_ok() || !ok { return false; }
    // Both cross-ABI directions use the same authenticated endpoint objects.
    for register_client in [true, false] {
        let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
        if endpoint < 0 { return false; }
        let endpoint = endpoint as u64;
        let server = thread::spawn(move || if register_client {
            bench_server_reply_recv(endpoint)
        } else { bench_server_register_reply_recv(endpoint) });
        for op in [BENCH_OP_PING, BENCH_OP_STOP] {
            let request = BenchMsg { op, seq: 0x1234_5678_90ab_cdef };
            let mut response = BenchMsg::default();
            let status = if register_client {
                let result = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_CALL,
                    endpoint, 16, [request.op, request.seq], 0) };
                response = BenchMsg { op: result.words[0], seq: result.words[1] };
                result.status
            } else { unsafe { syscall5(SYS_RUSTOS_IPC_CALL, endpoint,
                (&request as *const BenchMsg) as u64, 16,
                (&mut response as *mut BenchMsg) as u64, 16) } };
            ok &= valid_round_trip_reply(status, &request, &response);
        }
        ok &= server.join().is_ok();
    }
    ok
}

fn bench_server_register_reply_recv(endpoint: u64) {
    let mut received = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_RECV, endpoint, 0, [0; 2], 0) };
    loop {
        if received.status != 16 || received.reply == 0 || received.sender_pid == 0 || received.sender_tid == 0 {
            return;
        }
        RECV_TSC.store(tsc(), Ordering::Relaxed);
        let stop = received.words[0] == BENCH_OP_STOP;
        REPLY_TSC.store(tsc(), Ordering::Release);
        if stop {
            let result = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_REPLY,
                received.reply, 16, received.words, 0) };
            assert_eq!(result.status, 0, "register STOP reply failed");
            return;
        }
        received = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
            endpoint, 16, received.words, received.reply) };
    }
}

fn measure_intra_process_round_trip(
    tsc_khz: u64,
    name: &str,
    split_names: [&str; 3],
    server_entry: fn(u64),
    registers: bool,
) {
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip(name, "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || server_entry(endpoint));
    // Let the server reach its first receive. A call that lands before the
    // receiver exists would measure the enqueue-and-block path instead.
    thread::sleep(Duration::from_millis(50));

    let mut request = BenchMsg {
        op: BENCH_OP_PING,
        seq: 0,
    };
    let mut response = BenchMsg::default();
    let mut call = |request: &BenchMsg, response: &mut BenchMsg| -> i64 {
        if registers {
            let result = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_CALL,
                endpoint, 16, [request.op, request.seq], 0) };
            *response = BenchMsg { op: result.words[0], seq: result.words[1] };
            return result.status;
        }
        unsafe {
            syscall5(
                SYS_RUSTOS_IPC_CALL,
                endpoint,
                (request as *const BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (response as *mut BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
            )
        }
    };

    // Split the round trip with the server's own timestamps. This needs no
    // kernel change and it separates the two blocking transitions from the
    // server's (near-zero) work, which is the part the in-kernel phase
    // instrumentation cannot see.
    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut worst = crate::tails::Worst::new();
    let mut to_recv = Vec::with_capacity(IPC_ITERS);
    let mut server_body = Vec::with_capacity(IPC_ITERS);
    let mut to_return = Vec::with_capacity(IPC_ITERS);
    let mut ok = true;
    for _ in 0..WARMUP {
        request.seq = request.seq.wrapping_add(1);
        if !valid_round_trip_reply(call(&request, &mut response), &request, &response) {
            ok = false;
            break;
        }
    }
    for _ in 0..IPC_ITERS {
        request.seq = request.seq.wrapping_add(1);
        let Some((start, end)) =
            validated_round_trip_sample(&request, &mut response, &mut call, tsc)
        else {
            ok = false;
            break;
        };
        let recv_at = RECV_TSC.load(Ordering::Relaxed);
        let reply_at = REPLY_TSC.load(Ordering::Acquire);
        total.push(end.wrapping_sub(start));
        // A stamp outside the interval means the server lapped this sample;
        // drop it rather than record a wrapped difference as a cost.
        let parts = if recv_at > start && reply_at >= recv_at && end >= reply_at {
            to_recv.push(recv_at - start);
            server_body.push(reply_at - recv_at);
            to_return.push(end - reply_at);
            Some([recv_at - start, reply_at - recv_at, end - reply_at])
        } else { None };
        worst.observe(crate::tails::Sample { seq: request.seq, start, end, parts });
    }
    request.op = BENCH_OP_STOP;
    request.seq = request.seq.wrapping_add(1);
    let stopped = valid_round_trip_reply(call(&request, &mut response), &request, &response);
    let joined = server.join().is_ok();
    if !round_trip_transcript_complete(ok, stopped, joined, total.len()) {
        skip(name, "ipc-transcript-invalid");
        return;
    }
    report(name, &summarize(&mut total), tsc_khz);
    worst.defer(name);
    for (split, samples) in
        split_names
            .into_iter()
            .zip([&mut to_recv, &mut server_body, &mut to_return])
    {
        if samples.is_empty() {
            skip(split, "no-paired-server-stamps");
        } else {
            report(split, &summarize(samples), tsc_khz);
        }
    }
}

/// Non-blocking receive on an endpoint that is known to be empty. This walks
/// the same handle table, endpoint slab, and tracked locks a real receive
/// walks, but never blocks and never reschedules, so it separates the IPC
/// object cost from the scheduler cost that a round trip also pays.
pub(crate) fn probe_ipc_mechanism_only(tsc_khz: u64) {
    probe_ipc_mechanism_with_iterations(tsc_khz, SYSCALL_ITERS);
}

pub(crate) fn probe_ipc_mechanism_with_iterations(tsc_khz: u64, iterations: usize) {
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip("ipc_try_recv_empty", "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let mut buffer = BenchMsg::default();
    let mut reply_cap: u64 = 0;
    // Wrong-handle/unsupported/permission failures must never become cheap
    // samples. Validate the exact empty result after the ending timestamp.
    let result = measure_stamped(iterations, WARMUP, || {
        let start = tsc();
        let status = unsafe {
            syscall4(
                SYS_RUSTOS_IPC_TRY_RECV,
                endpoint,
                (&mut buffer as *mut BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (&mut reply_cap as *mut u64) as u64,
            )
        };
        let end = tsc();
        empty_receive_status_valid(status, reply_cap).then_some(end.wrapping_sub(start))
    });
    match result {
        Some(mut samples) => report("ipc_try_recv_empty", &summarize(&mut samples), tsc_khz),
        None => skip("ipc_try_recv_empty", "try-recv-failed"),
    }
}

/// Cross-process round trip over the ordinary application path: `getuid` is
/// offloaded to `syscalld`, so one call is kernel entry, a full IPC round trip
/// to a second process, and kernel exit. Subtracting `null_syscall_getpid`
/// leaves the round trip itself.
pub(crate) fn probe_syscall_offload(tsc_khz: u64) {
    let result = measure(IPC_ITERS, WARMUP, || unsafe {
        syscall0(SYS_LINUX_GETUID) >= 0
    });
    match result {
        Some(mut samples) => report(
            "ipc_rt_cross_process_syscalld_getuid",
            &summarize(&mut samples),
            tsc_khz,
        ),
        None => skip("ipc_rt_cross_process_syscalld_getuid", "getuid-failed"),
    }
}
