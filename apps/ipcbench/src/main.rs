//! Ring3 syscall, scheduler, and IPC round-trip cost harness.
//!
//! - **Owner:** this app owns only measurement and reporting; every probe uses
//!   an already-published ABI and no privileged or bench-only kernel path.
//! - **Boundary:** results are advisory measurements, never a policy input.
//! - **Lifecycle:** calibrate the TSC, run each probe warm, report, exit.
//! - **Failure:** a probe that cannot run reports `skip` and the harness
//!   continues, so one unavailable service cannot void the whole run.
//! - **Forbidden:** no allocation, formatting, or logging inside a measured
//!   interval; samples are collected into preallocated storage and only
//!   formatted after the loop ends.

use std::arch::asm;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_RECV_WITH_SENDER, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_TRY_RECV,
    SYS_RUSTOS_PHASE_PROFILE_DRAIN,
};

mod scheduling_context_probe;

const SYS_LINUX_GETPID: u64 = 39;
/// Offloaded to `syscalld`, so one call is one complete cross-process IPC
/// round trip over the ordinary application path. The gap against
/// `SYS_LINUX_GETPID`, which the kernel answers locally, is that round trip.
const SYS_LINUX_GETUID: u64 = 102;
const SYS_LINUX_SCHED_YIELD: u64 = 24;
const SYS_LINUX_CLOCK_GETTIME: u64 = 228;
const CLOCK_MONOTONIC: u64 = 1;

/// Warm the branch predictors, the handle-table cache lines, and the receiver
/// state machine before sampling. A cold first call is a boot-path cost, not
/// the steady-state cost this harness reports.
const WARMUP: usize = 2_000;
const SYSCALL_ITERS: usize = 50_000;
const IPC_ITERS: usize = 20_000;

/// `--isolate-probe` wants every phase-profile sample charged while this
/// probe runs to belong to it alone. The rest of the session catalog starts
/// at roughly the same wall-clock moment this program does, so without a
/// settle the one-time startup burst of every other session program lands
/// inside the measured window just from launch order, not from anything the
/// isolated probe did.
const ISOLATE_SETTLE: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- syscalls

#[inline(always)]
unsafe fn syscall0(n: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall2(n: u64, a0: u64, a1: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall3(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall4(n: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall5(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall6(n: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// See `SYS_RUSTOS_PHASE_PROFILE_DRAIN`: flushes the kernel's
/// `ipc-call-phase-*`/`usermem-phase-*` counters immediately rather than
/// waiting for their ordinary once-per-second housekeeping drain.
fn force_drain_phase_profiles() {
    unsafe {
        syscall0(SYS_RUSTOS_PHASE_PROFILE_DRAIN);
    }
}

fn debug_line(message: &str) {
    let mut line = Vec::with_capacity(message.len() + 1);
    line.extend_from_slice(message.as_bytes());
    line.push(b'\n');
    unsafe {
        syscall2(
            SYS_RUSTOS_DEBUG_PRINT,
            line.as_ptr() as u64,
            line.len() as u64,
        );
    }
}

// -------------------------------------------------------------------- TSC

/// `lfence` before the read keeps earlier work from drifting past the sample.
/// The measured regions are all syscalls, which serialize on their own, so a
/// full `cpuid` barrier would only add its own cost to every sample.
#[inline(always)]
fn tsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn monotonic_nanos() -> u64 {
    let mut ts = KTimespec::default();
    let status = unsafe {
        syscall2(
            SYS_LINUX_CLOCK_GETTIME,
            CLOCK_MONOTONIC,
            (&mut ts as *mut KTimespec) as u64,
        )
    };
    if status < 0 {
        return 0;
    }
    let seconds = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Returns TSC kHz, or 0 when the monotonic clock is unavailable. A zero
/// result degrades the report to cycles only rather than inventing a scale.
fn calibrate_tsc_khz() -> u64 {
    let t0 = monotonic_nanos();
    if t0 == 0 {
        return 0;
    }
    let c0 = tsc();
    thread::sleep(Duration::from_millis(300));
    let c1 = tsc();
    let t1 = monotonic_nanos();
    if t1 <= t0 || c1 <= c0 {
        return 0;
    }
    let cycles = c1 - c0;
    let nanos = t1 - t0;
    // cycles/ns * 1e6 = kHz
    cycles.saturating_mul(1_000_000) / nanos
}

// ------------------------------------------------------------------ stats

struct Stats {
    iters: usize,
    min: u64,
    p50: u64,
    p90: u64,
    p99: u64,
    max: u64,
    mean: u64,
}

fn summarize(samples: &mut [u64]) -> Stats {
    samples.sort_unstable();
    let len = samples.len();
    let sum: u128 = samples.iter().map(|value| u128::from(*value)).sum();
    let pick = |q: usize| samples[(len - 1).min(len * q / 100)];
    Stats {
        iters: len,
        min: samples[0],
        p50: pick(50),
        p90: pick(90),
        p99: pick(99),
        max: samples[len - 1],
        mean: (sum / len as u128) as u64,
    }
}

fn report(name: &str, stats: &Stats, tsc_khz: u64) {
    let ns = |cycles: u64| -> u64 {
        if tsc_khz == 0 {
            0
        } else {
            cycles.saturating_mul(1_000_000) / tsc_khz
        }
    };
    debug_line(&format!(
        "ipcbench: result name={name} iters={} min={} p50={} p90={} p99={} max={} mean={} \
         min_ns={} p50_ns={} mean_ns={}",
        stats.iters,
        stats.min,
        stats.p50,
        stats.p90,
        stats.p99,
        stats.max,
        stats.mean,
        ns(stats.min),
        ns(stats.p50),
        ns(stats.mean),
    ));
}

fn skip(name: &str, reason: &str) {
    debug_line(&format!("ipcbench: skip name={name} reason={reason}"));
}

/// Runs `op` `iters` times, sampling each call. `op` returns false to abort the
/// probe. Sample storage is allocated up front so no measured interval can
/// contain an allocation.
fn measure<F>(iters: usize, warmup: usize, mut op: F) -> Option<Vec<u64>>
where
    F: FnMut() -> bool,
{
    for _ in 0..warmup {
        if !op() {
            return None;
        }
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = tsc();
        let ok = op();
        let end = tsc();
        if !ok {
            return None;
        }
        samples.push(end.wrapping_sub(start));
    }
    Some(samples)
}

// ------------------------------------------------------------------ probes

/// The cost of the measurement itself. Every other number in the report
/// includes this, so the reader needs it to interpret the small ones.
fn probe_tsc_overhead(tsc_khz: u64) {
    let mut samples = Vec::with_capacity(SYSCALL_ITERS);
    for _ in 0..WARMUP {
        let _ = tsc();
    }
    for _ in 0..SYSCALL_ITERS {
        let start = tsc();
        let end = tsc();
        samples.push(end.wrapping_sub(start));
    }
    report("tsc_overhead", &summarize(&mut samples), tsc_khz);
}

fn probe_null_syscall(tsc_khz: u64) {
    let result = measure(SYSCALL_ITERS, WARMUP, || unsafe {
        syscall0(SYS_LINUX_GETPID) >= 0
    });
    match result {
        Some(mut samples) => report("null_syscall_getpid", &summarize(&mut samples), tsc_khz),
        None => skip("null_syscall_getpid", "getpid-failed"),
    }
}

fn probe_sched_yield(tsc_khz: u64) {
    let result = measure(SYSCALL_ITERS, WARMUP, || unsafe {
        syscall0(SYS_LINUX_SCHED_YIELD) >= 0
    });
    match result {
        Some(mut samples) => report("sched_yield", &summarize(&mut samples), tsc_khz),
        None => skip("sched_yield", "sched-yield-failed"),
    }
}

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

/// Same-address-space round trip: this isolates the kernel IPC mechanism from
/// the address-space switch, so the gap against the cross-process probe is the
/// switch and the second process's scheduling cost.
fn probe_ipc_intra_process(tsc_khz: u64) {
    let endpoint = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint < 0 {
        skip("ipc_rt_intra_process", "endpoint-create-failed");
        return;
    }
    let endpoint = endpoint as u64;
    let server = thread::spawn(move || bench_server(endpoint));
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
        Some(mut samples) => report("ipc_rt_intra_process", &summarize(&mut samples), tsc_khz),
        None => skip("ipc_rt_intra_process", "ipc-call-failed"),
    }
    for (name, samples) in [
        ("ipc_split_call_to_recv", &mut to_recv),
        ("ipc_split_server_body", &mut server_body),
        ("ipc_split_reply_to_return", &mut to_return),
    ] {
        if samples.is_empty() {
            skip(name, "no-paired-server-stamps");
        } else {
            report(name, &summarize(samples), tsc_khz);
        }
    }
}

/// `cpuid` is unconditionally intercepted by the hypervisor, so this measures
/// one VM exit and nothing else. Every other probe runs in the same guest, so
/// this is the constant that says whether an unexplained cost *could* be
/// hypervisor exits and how many would be needed to account for it.
fn probe_vmexit_cpuid(tsc_khz: u64) {
    let result = measure(SYSCALL_ITERS, WARMUP, || {
        // The intrinsic is used rather than raw `asm!` because LLVM reserves
        // `rbx`. Leaf 0 is architecturally available at any privilege level and
        // only reads vendor/leaf identification.
        let leaf = core::arch::x86_64::__cpuid(0);
        core::hint::black_box(leaf.eax);
        true
    });
    match result {
        Some(mut samples) => report("vmexit_cpuid", &summarize(&mut samples), tsc_khz),
        None => skip("vmexit_cpuid", "cpuid-failed"),
    }
}

/// Non-blocking receive on an endpoint that is known to be empty. This walks
/// the same handle table, endpoint slab, and tracked locks a real receive
/// walks, but never blocks and never reschedules, so it separates the IPC
/// object cost from the scheduler cost that a round trip also pays.
fn probe_ipc_mechanism_only(tsc_khz: u64) {
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
fn probe_syscall_offload(tsc_khz: u64) {
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

// ------------------------------------------------------------ probe filter

/// Every `ipc-call-phase-*`/`usermem-phase-*` counter this harness's probes
/// charge is process-wide and drains on a wall-clock window that has nothing
/// to do with probe boundaries, so a phase total charged by more than one
/// probe in the same boot cannot be divided into either probe's round-trip
/// count. `cargo xtask bench --isolate-probe <name>` writes this contract to
/// the private per-run KVM disk so exactly one probe runs, and every counter
/// in the boot belongs to it. Reading it directly, the same way `uiserver`
/// reads its own acceptance contract, keeps this a two-file change: no
/// service mediates it.
const IPCBENCH_PROBE_CONTRACT_PATH: &str = "/system/registry/system/ipcbench-probe-v1.env";
const IPCBENCH_PROBE_CONTRACT_MAX_BYTES: usize = 256;

fn probe_filter() -> Option<String> {
    let contents = runtime_control::read_bounded_config_snapshot(
        IPCBENCH_PROBE_CONTRACT_PATH,
        IPCBENCH_PROBE_CONTRACT_MAX_BYTES,
    )
    .ok()?;
    parse_ipcbench_probe_contract(&contents)
}

fn parse_ipcbench_probe_contract(contents: &str) -> Option<String> {
    let mut contract = false;
    let mut probe = None;
    for line in contents.lines() {
        if !contract && line == "contract=rustos-ipcbench-probe-v1" {
            contract = true;
            continue;
        }
        match line.strip_prefix("probe=") {
            Some(name) if probe.is_none() && !name.is_empty() => probe = Some(name.to_owned()),
            _ => return None,
        }
    }
    probe.filter(|_| contract)
}

fn run_all_probes(tsc_khz: u64) {
    probe_tsc_overhead(tsc_khz);
    probe_null_syscall(tsc_khz);
    probe_vmexit_cpuid(tsc_khz);
    probe_sched_yield(tsc_khz);
    probe_ipc_mechanism_only(tsc_khz);
    probe_ipc_intra_process(tsc_khz);
    probe_syscall_offload(tsc_khz);
}

/// A closed vocabulary rather than a dispatch table keyed by the report name:
/// an unrecognized filter must not silently fall back to running everything,
/// which would defeat the isolation this exists for.
fn run_single_probe(name: &str, tsc_khz: u64) {
    match name {
        "tsc_overhead" => probe_tsc_overhead(tsc_khz),
        "null_syscall_getpid" => probe_null_syscall(tsc_khz),
        "vmexit_cpuid" => probe_vmexit_cpuid(tsc_khz),
        "sched_yield" => probe_sched_yield(tsc_khz),
        "ipc_try_recv_empty" => probe_ipc_mechanism_only(tsc_khz),
        "ipc_rt_intra_process" => probe_ipc_intra_process(tsc_khz),
        "ipc_rt_cross_process_syscalld_getuid" => probe_syscall_offload(tsc_khz),
        "ipc_nested_passive_server" => {
            scheduling_context_probe::probe_nested_passive_server(tsc_khz)
        }
        "scheduling_budget_exhaust_refill" => {
            scheduling_context_probe::probe_budget_exhaust_refill(tsc_khz)
        }
        other => skip(other, "unrecognized-probe-filter"),
    }
}

#[cfg(test)]
mod probe_filter_tests {
    use super::parse_ipcbench_probe_contract;

    #[test]
    fn exact_contract_with_a_probe_name_parses() {
        assert_eq!(
            parse_ipcbench_probe_contract(
                "contract=rustos-ipcbench-probe-v1\nprobe=ipc_rt_intra_process\n"
            ),
            Some("ipc_rt_intra_process".to_owned())
        );
    }

    #[test]
    fn a_missing_contract_line_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("probe=ipc_rt_intra_process\n"),
            None
        );
    }

    #[test]
    fn an_empty_probe_name_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("contract=rustos-ipcbench-probe-v1\nprobe=\n"),
            None
        );
    }

    #[test]
    fn a_duplicated_probe_line_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("contract=rustos-ipcbench-probe-v1\nprobe=a\nprobe=b\n"),
            None
        );
    }

    #[test]
    fn an_unrecognized_line_is_rejected() {
        assert_eq!(
            parse_ipcbench_probe_contract("contract=rustos-ipcbench-probe-v1\nprobe=a\nextra=1\n"),
            None
        );
    }
}

fn main() {
    debug_line("ipcbench: begin");

    let filter = probe_filter();
    if filter.is_some() {
        // `--isolate-probe` wants every `ipc-call-phase-*`/`usermem-phase-*`
        // sample in the window to belong to this probe alone, but every
        // session-startup program launches around the same moment: uiserver's
        // first scene compile, wayclick's first frame, and netprobe's
        // self-test are a one-time burst that would otherwise land inside the
        // measured window just by starting at the same time as this probe
        // does. Letting that burst pass before calibrating or charging
        // anything is cheap; a contaminated sample is not recoverable after
        // the fact.
        debug_line("ipcbench: isolate settle begin");
        thread::sleep(ISOLATE_SETTLE);
        debug_line("ipcbench: isolate settle done");
        // The ordinary housekeeping drain runs on its own once-per-second
        // cadence, decoupled from this boundary, so whatever it has not yet
        // flushed from boot and the settle sleep would otherwise leak into
        // the measured window the moment it next fires.
        force_drain_phase_profiles();
    }

    let tsc_khz = calibrate_tsc_khz();
    debug_line(&format!("ipcbench: tsc_khz={tsc_khz}"));

    match filter {
        Some(name) => {
            run_single_probe(&name, tsc_khz);
            // Flush the probe's own tail charges before the log capture can
            // see "end": the ordinary once-per-second drain cadence is not
            // synchronized to a probe's own finish time, so whatever
            // accumulated since its last window closed would otherwise sit in
            // the live counters, undrained and invisible to the log.
            force_drain_phase_profiles();
        }
        None => run_all_probes(tsc_khz),
    }

    debug_line("ipcbench: end");
}
