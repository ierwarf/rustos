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
};

use crate::{
    measure, report, skip, summarize, syscall0, syscall1, syscall3, syscall4, syscall5, syscall6,
    tsc, IPC_ITERS, SYSCALL_ITERS, SYS_LINUX_GETUID, WARMUP,
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BenchMsg {
    op: u64,
    seq: u64,
}

const BENCH_OP_PING: u64 = 0;
const BENCH_OP_STOP: u64 = 1;

/// Timestamps the server stamps inside one round trip.
///
/// The client is blocked in `call` from before `RECV_TSC` is written until
/// after `REPLY_TSC` is, so a plain Release/Acquire pair is enough: the reply
/// that unblocks the client is published after both stores.
static RECV_TSC: AtomicU64 = AtomicU64::new(0);
static REPLY_TSC: AtomicU64 = AtomicU64::new(0);

fn bench_server(endpoint: u64) {
    let mut request = BenchMsg::default();
    let response = BenchMsg::default();
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
        if received < 0 {
            return;
        }
        RECV_TSC.store(tsc(), Ordering::Relaxed);
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
    let response = BenchMsg::default();
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
    if received < 0 {
        return;
    }
    loop {
        RECV_TSC.store(tsc(), Ordering::Relaxed);
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
        if result < 0 {
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
    );
}

fn measure_intra_process_round_trip(
    tsc_khz: u64,
    name: &str,
    split_names: [&str; 3],
    server_entry: fn(u64),
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
    let call = |request: &BenchMsg, response: &mut BenchMsg| -> bool {
        let status = unsafe {
            syscall5(
                SYS_RUSTOS_IPC_CALL,
                endpoint,
                (request as *const BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (response as *mut BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
            )
        };
        status >= 0
    };

    // Split the round trip with the server's own timestamps. This needs no
    // kernel change and it separates the two blocking transitions from the
    // server's (near-zero) work, which is the part the in-kernel phase
    // instrumentation cannot see.
    let mut total = Vec::with_capacity(IPC_ITERS);
    let mut to_recv = Vec::with_capacity(IPC_ITERS);
    let mut server_body = Vec::with_capacity(IPC_ITERS);
    let mut to_return = Vec::with_capacity(IPC_ITERS);
    let mut ok = true;
    for _ in 0..WARMUP {
        request.seq = request.seq.wrapping_add(1);
        ok &= call(&request, &mut response);
    }
    for _ in 0..IPC_ITERS {
        request.seq = request.seq.wrapping_add(1);
        let start = tsc();
        let sent = call(&request, &mut response);
        let end = tsc();
        if !sent {
            ok = false;
            break;
        }
        let recv_at = RECV_TSC.load(Ordering::Relaxed);
        let reply_at = REPLY_TSC.load(Ordering::Acquire);
        total.push(end.wrapping_sub(start));
        // A stamp outside the interval means the server lapped this sample;
        // drop it rather than record a wrapped difference as a cost.
        if recv_at > start && reply_at >= recv_at && end >= reply_at {
            to_recv.push(recv_at - start);
            server_body.push(reply_at - recv_at);
            to_return.push(end - reply_at);
        }
    }
    let result = if ok { Some(total) } else { None };

    request.op = BENCH_OP_STOP;
    unsafe {
        syscall5(
            SYS_RUSTOS_IPC_CALL,
            endpoint,
            (&request as *const BenchMsg) as u64,
            size_of::<BenchMsg>() as u64,
            (&mut response as *mut BenchMsg) as u64,
            size_of::<BenchMsg>() as u64,
        );
    }
    let _ = server.join();

    match result {
        Some(mut samples) => report(name, &summarize(&mut samples), tsc_khz),
        None => skip(name, "ipc-call-failed"),
    }
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
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip("ipc_try_recv_empty", "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let mut buffer = BenchMsg::default();
    let mut reply_cap: u64 = 0;
    // An empty endpoint answers with an errno rather than a length, so the
    // probe only requires that the call returns, not that it succeeds.
    let result = measure(SYSCALL_ITERS, WARMUP, || {
        unsafe {
            syscall4(
                SYS_RUSTOS_IPC_TRY_RECV,
                endpoint,
                (&mut buffer as *mut BenchMsg) as u64,
                size_of::<BenchMsg>() as u64,
                (&mut reply_cap as *mut u64) as u64,
            );
        }
        true
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
