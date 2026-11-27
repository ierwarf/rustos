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
use std::sync::Barrier;
use std::thread;
use std::time::Duration;

use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    ipc_register_buffer_syscall, ipc_register_syscall, IpcCallWithHandlesArgs,
    IpcRecvWithHandlesArgs, IpcRecvWithSenderArgs, IpcReplyRecvWithSenderArgs,
    IpcReplyWithHandlesArgs, IPC_ABI_VERSION, IPC_REGISTER_BUFFER_MODE, SYS_RUSTOS_IPC_CALL,
    SYS_RUSTOS_IPC_CALL_BOUNDED, SYS_RUSTOS_IPC_CALL_WITH_HANDLES,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV_WITH_HANDLES,
    SYS_RUSTOS_IPC_RECV_WITH_SENDER, SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
    SYS_RUSTOS_IPC_REGISTER_CALL, SYS_RUSTOS_IPC_REGISTER_RECV, SYS_RUSTOS_IPC_REGISTER_REPLY,
    SYS_RUSTOS_IPC_REGISTER_REPLY_RECV, SYS_RUSTOS_IPC_REPLY,
    SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER, SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
    SYS_RUSTOS_IPC_TRY_RECV,
};

use crate::tails::{current_guest_cpu, Sample, SeqRing, Worst, PHASE_PROFILE_ENABLED};
use crate::{
    measure, measure_stamped, report, skip, summarize, syscall0, syscall1, syscall3, syscall4,
    syscall5, syscall6, tsc, IPC_ITERS, SYSCALL_ITERS, SYS_LINUX_GETUID, WARMUP,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallMode {
    Buffer,
    Registers,
    /// 16-byte echo into a 272-byte response buffer: past the inline image, inside
    /// the rendezvous limit, so it rides the frame's spill.
    Large,
    /// The handle-bearing call form with no descriptors: the transport is the queued
    /// message/reply objects even though its server is parked.
    Queued,
}

// The rendezvous frame carries up to one maximum IPC message each way, so a
// declared reply capacity above it is clamped, not a reason to enqueue.
const FAST_REPLY_LIMIT: usize = 64 * 1024;
const LARGE_REPLY_TAIL: usize = 256;

impl CallMode {
    /// Bytes behind the response message, all of which the bench checks stay untouched.
    fn tail_bytes(self) -> usize {
        match self {
            Self::Large => LARGE_REPLY_TAIL,
            Self::Buffer | Self::Registers | Self::Queued => 0,
        }
    }

    fn reply_capacity(self) -> usize {
        size_of::<BenchMsg>() + self.tail_bytes()
    }
}

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
    fn queued_probe_exceeds_the_actual_fast_response_capacity_with_real_backing() {
        assert_eq!(CallMode::Queued.reply_capacity(), 16);
        assert_eq!(CallMode::Large.reply_capacity(), 272);
        assert!(CallMode::Large.reply_capacity() <= FAST_REPLY_LIMIT);
        assert_eq!(CallMode::Buffer.reply_capacity(), 16);
        assert_eq!(CallMode::Registers.reply_capacity(), 16);
        // This is an intentional shape rejection, not a statistical assertion
        // that a server happened to be unparked. Fail if the kernel limit or
        // admission condition changes under the benchmark.
        let runtime = include_str!("../../../kernel/ipc-runtime/src/ipc/mod.rs");
        assert!(runtime.contains("pub const IPC_FAST_INLINE_BYTES: usize = 256;"));
        assert!(runtime.contains(
            "pub const IPC_FAST_MAX_BYTES: usize = rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES;"
        ));
        assert_eq!(rustos_user_abi::syscall::IPC_MAX_INLINE_BYTES, FAST_REPLY_LIMIT);
        let compat = include_str!("../../../kernel/compat/src/user/syscall/linux/ipc_ops.rs");
        let admission = compat.split("fn try_fast_ipc_call(").nth(1).unwrap()
            .split("struct FastCallResponse").next().unwrap();
        assert!(admission.contains("declared_capacity.min(kernel_ipc_runtime::api::IPC_FAST_MAX_BYTES)"));
        assert!(admission.contains("return Ok(None);"));
    }

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
        CallMode::Buffer,
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
        CallMode::Buffer,
    );
}

/// Large-buffer rendezvous: same 16-byte echo and fused server, but the caller
/// advertises a genuinely backed 272-byte response capacity.
pub(crate) fn probe_ipc_large_reply_recv(tsc_khz: u64) {
    measure_intra_process_round_trip(
        tsc_khz, "ipc_rt_intra_process_large_reply_recv",
        ["ipc_split_large_call_to_recv", "ipc_split_large_server_body", "ipc_split_large_reply_to_return"],
        bench_server_reply_recv, CallMode::Large,
    );
}

/// Current queued slowpath only: same 16-byte echo and fused server, but the
/// caller uses the handle-bearing form (no descriptors), which always enqueues.
pub(crate) fn probe_ipc_queued_reply_recv(tsc_khz: u64) {
    measure_intra_process_round_trip(
        tsc_khz, "ipc_rt_intra_process_queued_reply_recv",
        ["ipc_split_queued_call_to_recv", "ipc_split_queued_server_body", "ipc_split_queued_reply_to_return"],
        bench_server_reply_recv, CallMode::Queued,
    );
}

pub(crate) fn probe_state_checkpoint(name: &str, tsc_khz: u64) {
    let legs = [format!("{name}_to_recv"), format!("{name}_body"), format!("{name}_to_return")];
    measure_intra_process_round_trip(
        tsc_khz, name, [&legs[0], &legs[1], &legs[2]], bench_server_reply_recv, CallMode::Buffer,
    );
}

/// Same payload, stamps, warmups, counts and echo/STOP/join oracle as the
/// buffer control. This measures two threads in ONE address space, not CR3.
pub(crate) fn probe_ipc_register_reply_recv(tsc_khz: u64) {
    if !crate::ipc_service::preflight() {
        skip("ipc_rt_intra_process_register_reply_recv", "service-buffer-preflight-failed");
        return;
    }
    if !register_transport_preflight() {
        skip("ipc_rt_intra_process_register_reply_recv", "register-preflight-failed");
        return;
    }
    measure_intra_process_round_trip(tsc_khz, "ipc_rt_intra_process_register_reply_recv",
        ["ipc_split_register_call_to_recv", "ipc_split_register_server_body", "ipc_split_register_reply_to_return"],
        bench_server_register_reply_recv, CallMode::Registers);
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
    mode: CallMode,
) {
    measure_intra_process_round_trip_with_seq(tsc_khz, name, split_names, server_entry, mode, false);
}

fn measure_intra_process_round_trip_with_seq(
    tsc_khz: u64,
    name: &str,
    split_names: [&str; 3],
    server_entry: fn(u64),
    mode: CallMode,
    explicit_seq: bool,
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
    // One word-aligned allocation: the response message first, the checked tail behind it.
    let mut backing = vec![0xcccc_cccc_cccc_cccc_u64; mode.reply_capacity().div_ceil(8)];
    // SAFETY: the allocation is at least `size_of::<BenchMsg>()` bytes and 8-aligned.
    let mut response = unsafe { &mut *(backing.as_mut_ptr() as *mut BenchMsg) };
    *response = BenchMsg::default();
    let reply_capacity = mode.reply_capacity();
    let mut call = |request: &BenchMsg, response: &mut BenchMsg| -> i64 {
        if mode == CallMode::Registers {
            let result = unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_CALL,
                endpoint, 16, [request.op, request.seq], 0) };
            *response = BenchMsg { op: result.words[0], seq: result.words[1] };
            return result.status;
        }
        if mode == CallMode::Queued {
            return call_with_handles(
                endpoint,
                (request as *const BenchMsg) as *const u8,
                size_of::<BenchMsg>(),
                (response as *mut BenchMsg) as *mut u8,
                reply_capacity,
                &[],
            );
        }
        unsafe {
            syscall5(
                SYS_RUSTOS_IPC_CALL,
                endpoint,
                (request as *const BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (response as *mut BenchMsg) as u64,
                reply_capacity as u64,
            )
        }
    };

    // Split the round trip with the server's own timestamps. This needs no
    // kernel change and it separates the two blocking transitions from the
    // server's (near-zero) work, which is the part the in-kernel phase
    // instrumentation cannot see.
    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut worst = Worst::new();
    let mut to_recv = Vec::with_capacity(IPC_ITERS);
    let mut server_body = Vec::with_capacity(IPC_ITERS);
    let mut to_return = Vec::with_capacity(IPC_ITERS);
    let mut seq_ring = SeqRing::new(explicit_seq);
    let mut ok = true;
    for _ in 0..WARMUP {
        request.seq = request.seq.wrapping_add(1);
        if !valid_round_trip_reply(call(&request, &mut response), &request, &response) {
            ok = false;
            break;
        }
    }
    if PHASE_PROFILE_ENABLED || explicit_seq {
        run_intra_loop::<true>(
            &mut request,
            &mut response,
            &mut call,
            &mut total,
            &mut worst,
            &mut seq_ring,
            &mut to_recv,
            &mut server_body,
            &mut to_return,
            &mut ok,
        );
    } else {
        run_intra_loop::<false>(
            &mut request,
            &mut response,
            &mut call,
            &mut total,
            &mut worst,
            &mut seq_ring,
            &mut to_recv,
            &mut server_body,
            &mut to_return,
            &mut ok,
        );
    }
    request.op = BENCH_OP_STOP;
    request.seq = request.seq.wrapping_add(1);
    let stopped = valid_round_trip_reply(call(&request, &mut response), &request, &response);
    let joined = server.join().is_ok();
    // SAFETY: `backing` outlives this view; `response` is not used past this point.
    let tail_intact = unsafe {
        core::slice::from_raw_parts(
            (backing.as_ptr() as *const u8).add(size_of::<BenchMsg>()),
            mode.tail_bytes(),
        )
    }
    .iter()
    .all(|byte| *byte == 0xcc);
    if !tail_intact {
        skip(name, "reply-tail-overwritten");
        return;
    }
    if !round_trip_transcript_complete(ok, stopped, joined, total.len()) {
        skip(name, "ipc-transcript-invalid");
        return;
    }
    if matches!(mode, CallMode::Queued | CallMode::Large) {
        let transport = if mode == CallMode::Queued { "queued" } else { "rendezvous-spill" };
        crate::debug_line(&format!(
            "ipcbench: proof name={name} transport={transport} request_bytes=16 response_bytes=16 reply_capacity={reply_capacity} fast_reply_limit={FAST_REPLY_LIMIT} warmup={WARMUP} samples={IPC_ITERS} tail_intact=1"
        ));
    }
    finish_round_trip_report(
        tsc_khz,
        name,
        split_names,
        &mut total,
        worst,
        seq_ring,
        &mut to_recv,
        &mut server_body,
        &mut to_return,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_intra_loop<const SEQ: bool>(
    request: &mut BenchMsg,
    response: &mut BenchMsg,
    call: &mut impl FnMut(&BenchMsg, &mut BenchMsg) -> i64,
    total: &mut Vec<u64>,
    worst: &mut Worst,
    seq_ring: &mut SeqRing,
    to_recv: &mut Vec<u64>,
    server_body: &mut Vec<u64>,
    to_return: &mut Vec<u64>,
    ok: &mut bool,
) {
    for _ in 0..IPC_ITERS {
        request.seq = request.seq.wrapping_add(1);
        let Some((start, end)) =
            validated_round_trip_sample(request, response, call, tsc)
        else {
            *ok = false;
            break;
        };
        let recv_at = RECV_TSC.load(Ordering::Relaxed);
        let reply_at = REPLY_TSC.load(Ordering::Acquire);
        record_round_trip_sample::<SEQ>(
            0,
            request.seq,
            start,
            end,
            recv_at,
            reply_at,
            total,
            worst,
            seq_ring,
            to_recv,
            server_body,
            to_return,
        );
    }
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn record_round_trip_sample<const SEQ: bool>(
    run: u32,
    seq: u64,
    start: u64,
    end: u64,
    recv_at: u64,
    reply_at: u64,
    total: &mut Vec<u64>,
    worst: &mut Worst,
    seq_ring: &mut SeqRing,
    to_recv: &mut Vec<u64>,
    server_body: &mut Vec<u64>,
    to_return: &mut Vec<u64>,
) {
    total.push(end.wrapping_sub(start));
    // A stamp outside the interval means the server lapped this sample;
    // drop it rather than record a wrapped difference as a cost.
    let parts = if recv_at > start && reply_at >= recv_at && end >= reply_at {
        to_recv.push(recv_at - start);
        server_body.push(reply_at - recv_at);
        to_return.push(end - reply_at);
        Some([recv_at - start, reply_at - recv_at, end - reply_at])
    } else {
        None
    };
    let sample = Sample {
        seq,
        start,
        end,
        parts,
    };
    worst.observe(sample);
    if SEQ {
        seq_ring.observe(run, sample, current_guest_cpu());
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_round_trip_report(
    tsc_khz: u64,
    name: &str,
    split_names: [&str; 3],
    total: &mut [u64],
    worst: Worst,
    seq_ring: SeqRing,
    to_recv: &mut [u64],
    server_body: &mut [u64],
    to_return: &mut [u64],
) {
    report(name, &summarize(total), tsc_khz);
    worst.defer(name);
    seq_ring.defer(name);
    for (split, samples) in split_names
        .into_iter()
        .zip([to_recv, server_body, to_return])
    {
        if samples.is_empty() {
            skip(split, "no-paired-server-stamps");
        } else {
            report(split, &summarize(samples), tsc_khz);
        }
    }
}

// ------------------------------------------------------------------- B0.e

const DIAG_ECHO_WORD0: u64 = 0xB0EC_0001;
const DIAG_ECHO_WORD1: u64 = 0xB0EC_0002;
const DIAG_ECHO_MAGIC: u64 = 0xB0EC_4000_0000_0001;
const DIAG_SWITCH_SERVER_MAGIC: u64 = 0xB0EC_4000_0000_0002;
const DIAG_SWITCH_CALL_MAGIC: u64 = 0xB0EC_4000_0000_0003;
const DIAG_SWITCH_STOP_MAGIC: u64 = 0xB0EC_4000_0000_0004;

#[inline(always)]
unsafe fn diag_register_magic_syscall(magic: u64, w0: u64, w1: u64) -> (i64, [u64; 2]) {
    let (status, out0, out1);
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_RUSTOS_IPC_REGISTER_CALL => status,
            inlateout("rdi") 0_u64 => _,
            inlateout("rsi") 16_u64 => _,
            inlateout("rdx") w0 => out0,
            inlateout("r10") w1 => out1,
            inlateout("r8") 0_u64 => _,
            inlateout("r9") magic => _,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, [out0, out1])
}

/// B0.e: measures the bare two-crossing register fastcall boundary without
/// endpoint/scheduler dispatch when the kernel enables the diagnostic echo gate.
pub(crate) fn probe_ipc_diag_register_echo(tsc_khz: u64) {
    const NAME: &str = "ipc_rt_diag_register_echo";
    let (status, words) = unsafe { diag_register_magic_syscall(DIAG_ECHO_MAGIC, DIAG_ECHO_WORD0, DIAG_ECHO_WORD1) };
    if status != 16 {
        skip(NAME, "diag-echo-not-enabled");
        return;
    }
    if words != [DIAG_ECHO_WORD0, DIAG_ECHO_WORD1] {
        skip(NAME, "diag-echo-mismatch");
        return;
    }
    for _ in 0..WARMUP {
        let (s0, r0) = unsafe { diag_register_magic_syscall(DIAG_ECHO_MAGIC, DIAG_ECHO_WORD0, DIAG_ECHO_WORD1) };
        let (s1, r1) = unsafe { diag_register_magic_syscall(DIAG_ECHO_MAGIC, DIAG_ECHO_WORD0, DIAG_ECHO_WORD1) };
        if s0 != 16
            || r0 != [DIAG_ECHO_WORD0, DIAG_ECHO_WORD1]
            || s1 != 16
            || r1 != [DIAG_ECHO_WORD0, DIAG_ECHO_WORD1]
        {
            skip(NAME, "diag-echo-warmup-failed");
            return;
        }
    }
    let mut total = Vec::with_capacity(IPC_ITERS);
    for _ in 0..IPC_ITERS {
        let start = tsc();
        let (s0, r0) = unsafe { diag_register_magic_syscall(DIAG_ECHO_MAGIC, DIAG_ECHO_WORD0, DIAG_ECHO_WORD1) };
        let (s1, r1) = unsafe { diag_register_magic_syscall(DIAG_ECHO_MAGIC, DIAG_ECHO_WORD0, DIAG_ECHO_WORD1) };
        let end = tsc();
        if s0 != 16
            || r0 != [DIAG_ECHO_WORD0, DIAG_ECHO_WORD1]
            || s1 != 16
            || r1 != [DIAG_ECHO_WORD0, DIAG_ECHO_WORD1]
        {
            skip(NAME, "diag-echo-runtime-failed");
            return;
        }
        total.push(end.wrapping_sub(start));
    }
    report(NAME, &summarize(&mut total), tsc_khz);
}

/// B0.s: measures two actual L2 paired kernel switches (`C -> S -> C`) with no
/// endpoint lookups or transfer objects, checking cold formation separately
/// and verifying foreign callers cannot enter the diagnostic pair.
pub(crate) fn probe_ipc_diag_switch_floor(tsc_khz: u64) {
    const NAME: &str = "ipc_rt_diag_switch_floor";
    let (probe_status, _) =
        unsafe { diag_register_magic_syscall(DIAG_ECHO_MAGIC, DIAG_ECHO_WORD0, DIAG_ECHO_WORD1) };
    if probe_status != 16 {
        skip(NAME, "diag-switch-not-enabled");
        return;
    }

    let server = thread::spawn(|| {
        let _ = unsafe { diag_register_magic_syscall(DIAG_SWITCH_SERVER_MAGIC, 0, 0) };
    });
    thread::sleep(Duration::from_millis(50));

    // Confirm a foreign process cannot enter the kernel-bound diagnostic pair.
    let child = unsafe { libc::fork() };
    if child == 0 {
        let (foreign_status, _) =
            unsafe { diag_register_magic_syscall(DIAG_SWITCH_CALL_MAGIC, 1, 2) };
        let code = if foreign_status == -1 { 0 } else { 1 };
        unsafe { libc::_exit(code) };
    }
    let mut wait_status: libc::c_int = 0;
    if child < 0
        || unsafe { libc::waitpid(child, &mut wait_status, 0) } != child
        || !libc::WIFEXITED(wait_status)
        || libc::WEXITSTATUS(wait_status) != 0
    {
        let _ = unsafe { diag_register_magic_syscall(DIAG_SWITCH_STOP_MAGIC, 0, 0) };
        let _ = server.join();
        skip(NAME, "foreign-caller-check-failed");
        return;
    }

    // Measure cold pair formation separately on the very first call.
    let cold_start = tsc();
    let (cold_status, cold_words) =
        unsafe { diag_register_magic_syscall(DIAG_SWITCH_CALL_MAGIC, 0xB05E_0000, 1) };
    let cold_end = tsc();
    if cold_status != 16 || cold_words != [0xB05E_0000, 1] {
        let _ = unsafe { diag_register_magic_syscall(DIAG_SWITCH_STOP_MAGIC, 0, 0) };
        let _ = server.join();
        skip(NAME, "diag-switch-cold-failed");
        return;
    }
    let mut cold = vec![cold_end.wrapping_sub(cold_start)];
    report("ipc_rt_diag_switch_floor_cold", &summarize(&mut cold), tsc_khz);

    for i in 0..WARMUP {
        let (st, w) =
            unsafe { diag_register_magic_syscall(DIAG_SWITCH_CALL_MAGIC, DIAG_ECHO_WORD0, i as u64) };
        if st != 16 || w != [DIAG_ECHO_WORD0, i as u64] {
            let _ = unsafe { diag_register_magic_syscall(DIAG_SWITCH_STOP_MAGIC, 0, 0) };
            let _ = server.join();
            skip(NAME, "diag-switch-warmup-failed");
            return;
        }
    }

    let mut total = Vec::with_capacity(IPC_ITERS);
    for i in 0..IPC_ITERS {
        let w1 = i as u64 + 1;
        let start = tsc();
        let (st, w) =
            unsafe { diag_register_magic_syscall(DIAG_SWITCH_CALL_MAGIC, DIAG_ECHO_WORD0, w1) };
        let end = tsc();
        if st != 16 || w != [DIAG_ECHO_WORD0, w1] {
            let _ = unsafe { diag_register_magic_syscall(DIAG_SWITCH_STOP_MAGIC, 0, 0) };
            let _ = server.join();
            skip(NAME, "diag-switch-runtime-failed");
            return;
        }
        total.push(end.wrapping_sub(start));
    }

    let _ = unsafe { diag_register_magic_syscall(DIAG_SWITCH_STOP_MAGIC, 0, 0) };
    let _ = server.join();
    report(NAME, &summarize(&mut total), tsc_khz);
}

// --------------------------------------------------------------------- Q0

#[inline(always)]
fn call_with_handles(
    endpoint: u64,
    request_ptr: *const u8,
    request_len: usize,
    reply_ptr: *mut u8,
    reply_capacity: usize,
    send_fds: &[i32],
) -> i64 {
    let mut received_fd_count: u16 = 0;
    let args = IpcCallWithHandlesArgs {
        endpoint,
        request_ptr: request_ptr as u64,
        request_len: request_len as u64,
        reply_ptr: reply_ptr as u64,
        reply_capacity: reply_capacity as u64,
        send_fds_ptr: if send_fds.is_empty() {
            0
        } else {
            send_fds.as_ptr() as u64
        },
        send_fd_count: send_fds.len() as u16,
        recv_fd_capacity: 0,
        reserved0: 0,
        recv_fds_ptr: 0,
        recv_fd_count_ptr: (&mut received_fd_count as *mut u16) as u64,
    };
    unsafe {
        syscall1(
            SYS_RUSTOS_IPC_CALL_WITH_HANDLES,
            (&args as *const IpcCallWithHandlesArgs) as u64,
        )
    }
}

#[inline(always)]
fn recv_with_handles(
    endpoint: u64,
    request_ptr: *mut u8,
    request_capacity: usize,
    reply_cap: &mut u64,
    recv_fds: &mut [i32],
    recv_fd_count: &mut u16,
) -> i64 {
    *recv_fd_count = 0;
    let args = IpcRecvWithHandlesArgs {
        endpoint,
        request_ptr: request_ptr as u64,
        request_capacity: request_capacity as u64,
        reply_cap_ptr: (reply_cap as *mut u64) as u64,
        recv_fds_ptr: if recv_fds.is_empty() {
            0
        } else {
            recv_fds.as_mut_ptr() as u64
        },
        recv_fd_count_ptr: (recv_fd_count as *mut u16) as u64,
        recv_fd_capacity: recv_fds.len() as u16,
        reserved0: 0,
        reserved1: 0,
    };
    unsafe {
        syscall1(
            SYS_RUSTOS_IPC_RECV_WITH_HANDLES,
            (&args as *const IpcRecvWithHandlesArgs) as u64,
        )
    }
}

#[inline(always)]
fn reply_with_handles(reply_cap: u64, response_ptr: *const u8, response_len: usize) -> i64 {
    let args = IpcReplyWithHandlesArgs {
        reply_cap,
        response_ptr: response_ptr as u64,
        response_len: response_len as u64,
        send_fds_ptr: 0,
        send_fd_count: 0,
        reserved0: 0,
        reserved1: 0,
    };
    unsafe {
        syscall1(
            SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
            (&args as *const IpcReplyWithHandlesArgs) as u64,
        )
    }
}

pub(crate) fn probe_ipc_queued_0fd_16b(tsc_khz: u64) {
    measure_intra_process_round_trip_with_seq(
        tsc_khz,
        "ipc_rt_queued_0fd_16b",
        [
            "ipc_split_queued_0fd_call_to_recv",
            "ipc_split_queued_0fd_server_body",
            "ipc_split_queued_0fd_reply_to_return",
        ],
        bench_server_reply_recv,
        CallMode::Queued,
        true,
    );
}

pub(crate) fn probe_ipc_queued_fd_16b(tsc_khz: u64) {
    const NAME: &str = "ipc_rt_queued_fd_16b";
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip(NAME, "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let mut pipe_fds = [-1i32; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        skip(NAME, "pipe2-failed");
        return;
    }
    let server = thread::spawn(move || -> bool {
        let mut request = BenchMsg::default();
        let mut recv_fds = [-1i32; 1];
        let mut recv_fd_count = 0_u16;
        loop {
            let mut reply_cap = 0_u64;
            let received = recv_with_handles(
                endpoint,
                (&mut request as *mut BenchMsg) as *mut u8,
                size_of::<BenchMsg>(),
                &mut reply_cap,
                &mut recv_fds,
                &mut recv_fd_count,
            );
            if received != size_of::<BenchMsg>() as i64 || reply_cap == 0 {
                return false;
            }
            RECV_TSC.store(tsc(), Ordering::Relaxed);
            if recv_fd_count != 1 || recv_fds[0] < 0 {
                let _ = reply_with_handles(
                    reply_cap,
                    (&request as *const BenchMsg) as *const u8,
                    size_of::<BenchMsg>(),
                );
                return false;
            }
            if unsafe { libc::close(recv_fds[0]) } != 0 {
                return false;
            }
            let stop = request.op == BENCH_OP_STOP;
            let response = request;
            REPLY_TSC.store(tsc(), Ordering::Release);
            if reply_with_handles(
                reply_cap,
                (&response as *const BenchMsg) as *const u8,
                size_of::<BenchMsg>(),
            ) != 0
            {
                return false;
            }
            if stop {
                return true;
            }
        }
    });
    thread::sleep(Duration::from_millis(50));

    let send_fds = [pipe_fds[0]];
    let mut request = BenchMsg {
        op: BENCH_OP_PING,
        seq: 0,
    };
    let mut response = BenchMsg::default();
    let mut call = |req: &BenchMsg, resp: &mut BenchMsg| -> i64 {
        call_with_handles(
            endpoint,
            (req as *const BenchMsg) as *const u8,
            size_of::<BenchMsg>(),
            (resp as *mut BenchMsg) as *mut u8,
            size_of::<BenchMsg>(),
            &send_fds,
        )
    };
    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut worst = Worst::new();
    let mut seq_ring = SeqRing::new(true);
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
    if ok {
        run_intra_loop::<true>(
            &mut request,
            &mut response,
            &mut call,
            &mut total,
            &mut worst,
            &mut seq_ring,
            &mut to_recv,
            &mut server_body,
            &mut to_return,
            &mut ok,
        );
    }
    request.op = BENCH_OP_STOP;
    request.seq = request.seq.wrapping_add(1);
    let stopped = valid_round_trip_reply(call(&request, &mut response), &request, &response);
    let joined = server.join().is_ok_and(|valid| valid);
    unsafe {
        libc::close(pipe_fds[0]);
        libc::close(pipe_fds[1]);
    }
    if !round_trip_transcript_complete(ok, stopped, joined, total.len()) {
        skip(NAME, "ipc-queued-fd-transcript-invalid");
        return;
    }
    finish_round_trip_report(
        tsc_khz,
        NAME,
        [
            "ipc_split_queued_fd_call_to_recv",
            "ipc_split_queued_fd_server_body",
            "ipc_split_queued_fd_reply_to_return",
        ],
        &mut total,
        worst,
        seq_ring,
        &mut to_recv,
        &mut server_body,
        &mut to_return,
    );
}

pub(crate) fn probe_ipc_queued_reg_16b(tsc_khz: u64) {
    const NAME: &str = "ipc_rt_queued_reg_16b";
    if !crate::ipc_service::preflight() {
        skip(NAME, "service-buffer-preflight-failed");
        return;
    }
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip(NAME, "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || -> bool {
        let mut request = BenchMsg::default();
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let recv_args = IpcRecvWithSenderArgs {
            endpoint,
            request_ptr: (&mut request as *mut BenchMsg) as u64,
            request_capacity: size_of::<BenchMsg>() as u64,
            reply_cap_ptr: (&mut reply_cap as *mut u64) as u64,
            sender_pid_ptr: (&mut sender_pid as *mut u64) as u64,
            sender_tid_ptr: (&mut sender_tid as *mut u64) as u64,
        };
        let Some(mut received) = (unsafe {
            ipc_register_buffer_syscall(
                SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
                [
                    (&recv_args as *const IpcRecvWithSenderArgs) as u64,
                    IPC_BOOT_CONTROL_HARD_LIMIT_MS,
                    0,
                    0,
                    0,
                    0,
                ],
            )
        }) else {
            return false;
        };
        loop {
            if received != size_of::<BenchMsg>() as i64 || reply_cap == 0 {
                return false;
            }
            RECV_TSC.store(tsc(), Ordering::Relaxed);
            let response = request;
            let stop = request.op == BENCH_OP_STOP;
            REPLY_TSC.store(tsc(), Ordering::Release);
            if stop {
                return unsafe {
                    ipc_register_buffer_syscall(
                        SYS_RUSTOS_IPC_REPLY,
                        [
                            reply_cap,
                            (&response as *const BenchMsg) as u64,
                            size_of::<BenchMsg>() as u64,
                            0,
                            0,
                            0,
                        ],
                    )
                } == Some(0);
            }
            let fused = IpcReplyRecvWithSenderArgs {
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
            let Some(next) = (unsafe {
                ipc_register_buffer_syscall(
                    SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
                    [(&fused as *const IpcReplyRecvWithSenderArgs) as u64, 0, 0, 0, 0, 0],
                )
            }) else {
                return false;
            };
            received = next;
        }
    });
    thread::sleep(Duration::from_millis(50));

    let mut request = BenchMsg {
        op: BENCH_OP_PING,
        seq: 0,
    };
    let mut response = BenchMsg::default();
    let mut call = |req: &BenchMsg, resp: &mut BenchMsg| -> i64 {
        let block = rustos_user_abi::syscall::IpcRegisterBufferArgs {
            operation: SYS_RUSTOS_IPC_CALL_BOUNDED,
            args: [
                endpoint,
                (req as *const BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (resp as *mut BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                IPC_BOOT_CONTROL_HARD_LIMIT_MS,
            ],
        };
        let status: i64;
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_RUSTOS_IPC_REGISTER_CALL => status,
                inlateout("rdi") (&block as *const rustos_user_abi::syscall::IpcRegisterBufferArgs) as u64 => _,
                inlateout("rsi") 0_u64 => _,
                inlateout("rdx") 0_u64 => _,
                inlateout("r10") 0_u64 => _,
                inlateout("r8") 0_u64 => _,
                inlateout("r9") IPC_REGISTER_BUFFER_MODE => _,
                clobber_abi("C"),
                options(nostack),
            );
        }
        status
    };
    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut worst = Worst::new();
    let mut seq_ring = SeqRing::new(true);
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
    if ok {
        run_intra_loop::<true>(
            &mut request,
            &mut response,
            &mut call,
            &mut total,
            &mut worst,
            &mut seq_ring,
            &mut to_recv,
            &mut server_body,
            &mut to_return,
            &mut ok,
        );
    }
    request.op = BENCH_OP_STOP;
    request.seq = request.seq.wrapping_add(1);
    let stopped = valid_round_trip_reply(call(&request, &mut response), &request, &response);
    let joined = server.join().is_ok_and(|valid| valid);
    if !round_trip_transcript_complete(ok, stopped, joined, total.len()) {
        skip(NAME, "ipc-queued-reg-transcript-invalid");
        return;
    }
    finish_round_trip_report(
        tsc_khz,
        NAME,
        [
            "ipc_split_queued_reg_call_to_recv",
            "ipc_split_queued_reg_server_body",
            "ipc_split_queued_reg_reply_to_return",
        ],
        &mut total,
        worst,
        seq_ring,
        &mut to_recv,
        &mut server_body,
        &mut to_return,
    );
}

pub(crate) fn probe_ipc_queued_256b(tsc_khz: u64) {
    probe_ipc_queued_sized(
        tsc_khz,
        "ipc_rt_queued_256b",
        [
            "ipc_split_queued_256b_call_to_recv",
            "ipc_split_queued_256b_server_body",
            "ipc_split_queued_256b_reply_to_return",
        ],
        256,
        64,
    );
}

pub(crate) fn probe_ipc_queued_32k(tsc_khz: u64) {
    probe_ipc_queued_sized(
        tsc_khz,
        "ipc_rt_queued_32k",
        [
            "ipc_split_queued_32k_call_to_recv",
            "ipc_split_queued_32k_server_body",
            "ipc_split_queued_32k_reply_to_return",
        ],
        32_768,
        256,
    );
}

fn fill_sized_request(buf: &mut [u8], op: u64, seq: u64) {
    buf[..8].copy_from_slice(&op.to_le_bytes());
    buf[8..16].copy_from_slice(&seq.to_le_bytes());
    let len = buf.len();
    buf[len - 8..].copy_from_slice(&seq.to_le_bytes());
}

fn read_bench_header(buf: &[u8]) -> BenchMsg {
    BenchMsg {
        op: u64::from_le_bytes(buf[..8].try_into().unwrap()),
        seq: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
    }
}

fn probe_ipc_queued_sized(
    tsc_khz: u64,
    name: &str,
    split_names: [&str; 3],
    msg_bytes: usize,
    canary_bytes: usize,
) {
    debug_assert!(msg_bytes >= size_of::<BenchMsg>());
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip(name, "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || -> bool {
        let mut buf = vec![0_u8; msg_bytes];
        let mut recv_fd_count = 0_u16;
        loop {
            let mut reply_cap = 0_u64;
            let received = recv_with_handles(
                endpoint,
                buf.as_mut_ptr(),
                msg_bytes,
                &mut reply_cap,
                &mut [],
                &mut recv_fd_count,
            );
            if received != msg_bytes as i64 || reply_cap == 0 {
                return false;
            }
            RECV_TSC.store(tsc(), Ordering::Relaxed);
            let stop = read_bench_header(&buf).op == BENCH_OP_STOP;
            REPLY_TSC.store(tsc(), Ordering::Release);
            if reply_with_handles(reply_cap, buf.as_ptr(), msg_bytes) != 0 {
                return false;
            }
            if stop {
                return true;
            }
        }
    });
    thread::sleep(Duration::from_millis(50));

    let mut req = vec![0_u8; msg_bytes];
    for (idx, byte) in req.iter_mut().enumerate() {
        *byte = ((idx.wrapping_mul(31)) ^ 0x5a) as u8;
    }
    let mut resp = vec![0xcc_u8; msg_bytes + canary_bytes];
    let reply_capacity = msg_bytes + canary_bytes;
    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut worst = Worst::new();
    let mut seq_ring = SeqRing::new(true);
    let mut to_recv = Vec::with_capacity(IPC_ITERS);
    let mut server_body = Vec::with_capacity(IPC_ITERS);
    let mut to_return = Vec::with_capacity(IPC_ITERS);
    let mut seq = 0_u64;
    let mut ok = true;
    for _ in 0..WARMUP {
        seq = seq.wrapping_add(1);
        fill_sized_request(&mut req, BENCH_OP_PING, seq);
        let status = call_with_handles(
            endpoint,
            req.as_ptr(),
            msg_bytes,
            resp.as_mut_ptr(),
            reply_capacity,
            &[],
        );
        if status != msg_bytes as i64 || resp[..msg_bytes] != req[..msg_bytes] {
            ok = false;
            break;
        }
    }
    if ok {
        for _ in 0..IPC_ITERS {
            seq = seq.wrapping_add(1);
            fill_sized_request(&mut req, BENCH_OP_PING, seq);
            let start = tsc();
            let status = call_with_handles(
                endpoint,
                req.as_ptr(),
                msg_bytes,
                resp.as_mut_ptr(),
                reply_capacity,
                &[],
            );
            let end = tsc();
            if status != msg_bytes as i64 || resp[..msg_bytes] != req[..msg_bytes] {
                ok = false;
                break;
            }
            let recv_at = RECV_TSC.load(Ordering::Relaxed);
            let reply_at = REPLY_TSC.load(Ordering::Acquire);
            record_round_trip_sample::<true>(
                0,
                seq,
                start,
                end,
                recv_at,
                reply_at,
                &mut total,
                &mut worst,
                &mut seq_ring,
                &mut to_recv,
                &mut server_body,
                &mut to_return,
            );
        }
    }
    seq = seq.wrapping_add(1);
    fill_sized_request(&mut req, BENCH_OP_STOP, seq);
    let stop_status = call_with_handles(
        endpoint,
        req.as_ptr(),
        msg_bytes,
        resp.as_mut_ptr(),
        reply_capacity,
        &[],
    );
    let stopped = stop_status == msg_bytes as i64 && resp[..msg_bytes] == req[..msg_bytes];
    let joined = server.join().is_ok_and(|valid| valid);
    if !resp[msg_bytes..].iter().all(|&byte| byte == 0xcc) {
        skip(name, "reply-tail-overwritten");
        return;
    }
    if !round_trip_transcript_complete(ok, stopped, joined, total.len()) {
        skip(name, "ipc-queued-sized-transcript-invalid");
        return;
    }
    finish_round_trip_report(
        tsc_khz,
        name,
        split_names,
        &mut total,
        worst,
        seq_ring,
        &mut to_recv,
        &mut server_body,
        &mut to_return,
    );
}

static BACKLOG_RECV_TSC: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static BACKLOG_REPLY_TSC: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

pub(crate) fn probe_ipc_queued_backlog(tsc_khz: u64) {
    const NAME: &str = "ipc_rt_queued_backlog";
    const PER_CLIENT_ITERS: usize = IPC_ITERS / 2;
    const PER_CLIENT_WARMUP: usize = WARMUP / 2;
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip(NAME, "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || -> bool {
        let mut request = BenchMsg::default();
        let mut recv_fd_count = 0_u16;
        loop {
            let mut reply_cap = 0_u64;
            let received = recv_with_handles(
                endpoint,
                (&mut request as *mut BenchMsg) as *mut u8,
                size_of::<BenchMsg>(),
                &mut reply_cap,
                &mut [],
                &mut recv_fd_count,
            );
            if received != size_of::<BenchMsg>() as i64 || reply_cap == 0 {
                return false;
            }
            let recv_at = tsc();
            let stop = request.op == BENCH_OP_STOP;
            let slot = ((request.seq >> 48) as usize) & 1;
            BACKLOG_RECV_TSC[slot].store(recv_at, Ordering::Relaxed);
            let response = request;
            BACKLOG_REPLY_TSC[slot].store(tsc(), Ordering::Release);
            if reply_with_handles(
                reply_cap,
                (&response as *const BenchMsg) as *const u8,
                size_of::<BenchMsg>(),
            ) != 0
            {
                return false;
            }
            if stop {
                return true;
            }
        }
    });
    thread::sleep(Duration::from_millis(50));

    let barrier = Barrier::new(2);
    let results = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(2);
        for client_idx in 0..2usize {
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                let mut request = BenchMsg {
                    op: BENCH_OP_PING,
                    seq: (client_idx as u64) << 48,
                };
                let mut response = BenchMsg::default();
                let mut total = Vec::with_capacity(PER_CLIENT_ITERS);
                let mut worst = Worst::new();
                let mut seq_ring = SeqRing::with_capacity(true, PER_CLIENT_ITERS);
                let mut to_recv = Vec::with_capacity(PER_CLIENT_ITERS);
                let mut server_body = Vec::with_capacity(PER_CLIENT_ITERS);
                let mut to_return = Vec::with_capacity(PER_CLIENT_ITERS);
                let mut ok = true;
                for _ in 0..PER_CLIENT_WARMUP {
                    request.seq += 1;
                    let status = call_with_handles(
                        endpoint,
                        (&request as *const BenchMsg) as *const u8,
                        size_of::<BenchMsg>(),
                        (&mut response as *mut BenchMsg) as *mut u8,
                        size_of::<BenchMsg>(),
                        &[],
                    );
                    if !valid_round_trip_reply(status, &request, &response) {
                        ok = false;
                        break;
                    }
                }
                barrier.wait();
                if ok {
                    for _ in 0..PER_CLIENT_ITERS {
                        request.seq += 1;
                        let start = tsc();
                        let status = call_with_handles(
                            endpoint,
                            (&request as *const BenchMsg) as *const u8,
                            size_of::<BenchMsg>(),
                            (&mut response as *mut BenchMsg) as *mut u8,
                            size_of::<BenchMsg>(),
                            &[],
                        );
                        let end = tsc();
                        if !valid_round_trip_reply(status, &request, &response) {
                            ok = false;
                            break;
                        }
                        let recv_at = BACKLOG_RECV_TSC[client_idx].load(Ordering::Relaxed);
                        let reply_at = BACKLOG_REPLY_TSC[client_idx].load(Ordering::Acquire);
                        record_round_trip_sample::<true>(
                            client_idx as u32,
                            request.seq,
                            start,
                            end,
                            recv_at,
                            reply_at,
                            &mut total,
                            &mut worst,
                            &mut seq_ring,
                            &mut to_recv,
                            &mut server_body,
                            &mut to_return,
                        );
                    }
                }
                (ok, total, worst, seq_ring, to_recv, server_body, to_return)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().ok())
            .collect::<Vec<_>>()
    });

    let stop_req = BenchMsg {
        op: BENCH_OP_STOP,
        seq: 0xFFFF_FFFF,
    };
    let mut stop_resp = BenchMsg::default();
    let stopped = valid_round_trip_reply(
        call_with_handles(
            endpoint,
            (&stop_req as *const BenchMsg) as *const u8,
            size_of::<BenchMsg>(),
            (&mut stop_resp as *mut BenchMsg) as *mut u8,
            size_of::<BenchMsg>(),
            &[],
        ),
        &stop_req,
        &stop_resp,
    );
    let joined = server.join().is_ok_and(|valid| valid);

    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut worst = Worst::new();
    let mut seq_ring = SeqRing::new(true);
    let mut to_recv = Vec::with_capacity(IPC_ITERS);
    let mut server_body = Vec::with_capacity(IPC_ITERS);
    let mut to_return = Vec::with_capacity(IPC_ITERS);
    let mut clients_ok = true;
    for res in results {
        let Some((ok, c_total, c_worst, c_seq, c_recv, c_body, c_ret)) = res else {
            clients_ok = false;
            break;
        };
        if !ok {
            clients_ok = false;
        }
        total.extend(c_total);
        worst.merge_from(c_worst);
        seq_ring.merge_from(c_seq);
        to_recv.extend(c_recv);
        server_body.extend(c_body);
        to_return.extend(c_ret);
    }
    if !round_trip_transcript_complete(clients_ok, stopped, joined, total.len()) {
        skip(NAME, "ipc-queued-backlog-transcript-invalid");
        return;
    }
    finish_round_trip_report(
        tsc_khz,
        NAME,
        [
            "ipc_split_queued_backlog_call_to_recv",
            "ipc_split_queued_backlog_server_body",
            "ipc_split_queued_backlog_reply_to_return",
        ],
        &mut total,
        worst,
        seq_ring,
        &mut to_recv,
        &mut server_body,
        &mut to_return,
    );
}

pub(crate) fn probe_ipc_queued_cross_proc(tsc_khz: u64) {
    const NAME: &str = "ipc_rt_queued_cross_proc";
    // `getgroups(0, NULL)` uses `call_syscalld_value` -> `call_linux_syscall_endpoint`,
    // exercising the queued cross-process IPC path into `syscalld` without private-handle ACLs.
    let result = crate::measure(IPC_ITERS, WARMUP, || unsafe {
        crate::syscall2(libc::SYS_getgroups as u64, 0, 0) >= 0
    });
    match result {
        Some(mut samples) => report(NAME, &summarize(&mut samples), tsc_khz),
        None => skip(NAME, "getgroups-failed"),
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
