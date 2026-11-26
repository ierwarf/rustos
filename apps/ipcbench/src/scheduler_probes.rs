//! Real futex wake -> first measured user turn, plus observed APIC distribution.
//! A wake must actually find one kernel waiter. Early/unparked wakes fail the
//! probe instead of becoming deceptively cheap samples. CPUID is after the end
//! stamp. No pinning: the distribution observes the production wake placement.

use crate::{core_contract as contract, debug_line, report, skip, summarize, tsc};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Default)]
struct Gate {
    command: AtomicU32,
    done: AtomicU32,
    ready: AtomicBool,
    cancelled: AtomicBool,
    failed: AtomicBool,
    end: AtomicU64,
    apic: AtomicU32,
    tid: AtomicU64,
}

fn futex(word: &AtomicU32, operation: i32, value: u32) -> i64 {
    unsafe {
        crate::syscall6(
            libc::SYS_futex as u64,
            word as *const _ as u64,
            (operation | libc::FUTEX_PRIVATE_FLAG) as u64,
            u64::from(value),
            0,
            0,
            0,
        )
    }
}

fn apic_id() -> u32 {
    let max = std::arch::x86_64::__cpuid(0).eax;
    if max >= 0x1f && std::arch::x86_64::__cpuid_count(0x1f, 0).ebx != 0 {
        std::arch::x86_64::__cpuid_count(0x1f, 0).edx
    } else if max >= 0xb && std::arch::x86_64::__cpuid_count(0xb, 0).ebx != 0 {
        std::arch::x86_64::__cpuid_count(0xb, 0).edx
    } else {
        std::arch::x86_64::__cpuid(1).ebx >> 24
    }
}

fn wait(word: &AtomicU32, expected: u32) -> bool {
    let result = futex(word, libc::FUTEX_WAIT, expected);
    result == 0 || result == -i64::from(libc::EAGAIN) || result == -i64::from(libc::EINTR)
}

fn worker(gate: &Gate) -> bool {
    loop {
        if gate.cancelled.load(Ordering::Acquire) {
            return true;
        }
        gate.ready.store(true, Ordering::Release);
        while gate.command.load(Ordering::Acquire) == 0 {
            if gate.cancelled.load(Ordering::Acquire) {
                return true;
            }
            if !wait(&gate.command, 0) {
                return false;
            }
        }
        let end = tsc();
        if gate.cancelled.load(Ordering::Acquire) {
            return true;
        }
        gate.end.store(end, Ordering::Relaxed);
        gate.apic.store(apic_id(), Ordering::Relaxed);
        gate.command.store(0, Ordering::Release);
        gate.done.store(1, Ordering::Release);
        if futex(&gate.done, libc::FUTEX_WAKE, 1) < 0 {
            return false;
        }
    }
}

#[derive(Clone, Copy)]
struct WakeSample {
    elapsed: u64,
    syscall: u64,
    after_return: u64,
    apic: u32,
    tid: u64,
    start: u64,
    end: u64,
}

type WakeCollection = (Vec<u64>, BTreeMap<u32, usize>, Option<Vec<WakeSample>>);

fn collect<const TRACE: bool>(workers: usize, rounds: usize, warmup: usize) -> Option<WakeCollection> {
    let gates: Vec<_> = (0..workers).map(|_| Arc::new(Gate::default())).collect();
    let mut threads = Vec::with_capacity(workers);
    for gate in &gates {
        let gate = Arc::clone(gate);
        match std::thread::Builder::new().spawn(move || {
            if TRACE {
                gate.tid.store(unsafe { crate::syscall0(libc::SYS_gettid as u64) } as u64, Ordering::Relaxed);
            }
            let valid = worker(&gate);
            if !valid {
                gate.failed.store(true, Ordering::Release);
                gate.ready.store(true, Ordering::Release);
                gate.done.store(1, Ordering::Release);
                futex(&gate.done, libc::FUTEX_WAKE, 1);
            }
            valid
        }) {
            Ok(thread) => threads.push(thread),
            Err(_) => {
                for gate in &gates {
                    gate.cancelled.store(true, Ordering::Release);
                    gate.command.store(1, Ordering::Release);
                    futex(&gate.command, libc::FUTEX_WAKE, 1);
                }
                for thread in threads {
                    let _ = thread.join();
                }
                return None;
            }
        }
    }
    let mut samples = Vec::with_capacity(workers * rounds);
    let mut placement = BTreeMap::new();
    let mut starts = vec![0u64; workers];
    let mut returns = TRACE.then(|| vec![0u64; workers]);
    let mut trace = TRACE.then(|| Vec::with_capacity(workers * rounds));
    let mut valid = true;
    'rounds: for round in 0..rounds + warmup {
        for gate in &gates {
            while !gate.ready.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            gate.ready.store(false, Ordering::Release);
            if gate.failed.load(Ordering::Acquire) {
                valid = false;
                break 'rounds;
            }
        }
        // Untimed park opportunity. Actual wake count, not this sleep, proves
        // each sample had a real waiter; the sleep is not part of wake latency.
        std::thread::sleep(Duration::from_micros(200));
        for (index, (gate, start)) in gates.iter().zip(&mut starts).enumerate() {
            *start = tsc();
            gate.command.store(1, Ordering::Release);
            let result = futex(&gate.command, libc::FUTEX_WAKE, 1);
            if let Some(returns) = returns.as_mut() {
                returns[index] = tsc();
            }
            if result != 1 {
                valid = false;
                break 'rounds;
            }
        }
        for (index, (gate, start)) in gates.iter().zip(&starts).enumerate() {
            while gate.done.load(Ordering::Acquire) == 0 {
                if !wait(&gate.done, 0) {
                    valid = false;
                    break 'rounds;
                }
            }
            if gate.failed.load(Ordering::Acquire) {
                valid = false;
                break 'rounds;
            }
            let end = gate.end.load(Ordering::Relaxed);
            if end < *start {
                valid = false;
                break 'rounds;
            }
            if round >= warmup {
                let apic = gate.apic.load(Ordering::Relaxed);
                samples.push(end - start);
                *placement.entry(apic).or_insert(0) += 1;
                if let (Some(trace), Some(returns)) = (trace.as_mut(), returns.as_ref()) {
                    let returned = returns[index];
                    trace.push(WakeSample {
                        elapsed: end - start,
                        syscall: returned - start,
                        after_return: end.saturating_sub(returned),
                        apic,
                        tid: gate.tid.load(Ordering::Relaxed),
                        start: *start,
                        end,
                    });
                }
            }
            gate.done.store(0, Ordering::Release);
        }
    }
    for gate in &gates {
        gate.cancelled.store(true, Ordering::Release);
        gate.command.store(1, Ordering::Release);
        futex(&gate.command, libc::FUTEX_WAKE, 1);
    }
    for thread in threads {
        valid &= thread.join().unwrap_or(false);
    }
    (valid && samples.len() == workers * rounds).then_some((samples, placement, trace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_publishes_a_nonwaiting_value_before_waking() {
        for _ in 0..64 {
            let gate = Arc::new(Gate::default());
            let waiter = Arc::clone(&gate);
            let thread = std::thread::spawn(move || worker(&waiter));
            while !gate.ready.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            gate.cancelled.store(true, Ordering::Release);
            gate.command.store(1, Ordering::Release);
            futex(&gate.command, libc::FUTEX_WAKE, 1);
            assert!(thread.join().unwrap());
        }
    }
}

/// Diagnostic sibling of `smp_balance_futex`. A worker may run in parallel
/// before the waker returns; after_return is then zero, not a negative wait.
pub fn trace_balance(khz: u64) {
    const NAME: &str = "smp_balance_futex_trace";
    let Some((mut elapsed, _, Some(mut trace))) = collect::<true>(8, 256, 8) else {
        skip(NAME, "futex-wake-not-parked-or-transcript-failed");
        return;
    };
    report(NAME, &summarize(&mut elapsed), khz);
    let mut syscalls: Vec<_> = trace.iter().map(|sample| sample.syscall).collect();
    let mut after: Vec<_> = trace.iter().map(|sample| sample.after_return).collect();
    let syscall = summarize(&mut syscalls);
    let post = summarize(&mut after);
    let before_or_at_return = trace.iter().filter(|sample| sample.after_return == 0).count();
    debug_line(&format!(
        "ipcbench: wake trace n={} before_or_at_return={before_or_at_return} \
         syscall_p50={} syscall_p99={} after_p50={} after_p99={}",
        trace.len(), syscall.p50, syscall.p99, post.p50, post.p99,
    ));
    trace.sort_unstable_by_key(|sample| std::cmp::Reverse(sample.elapsed));
    for (rank, sample) in trace.iter().take(32).enumerate() {
        debug_line(&format!(
            "ipcbench: wake trace rank={} total={} syscall={} after_return={} apic={} tid={} start={} end={}",
            rank + 1, sample.elapsed, sample.syscall, sample.after_return, sample.apic,
            sample.tid, sample.start, sample.end,
        ));
    }
}

pub fn run(khz: u64) {
    crate::probe_sched_yield(khz);
    for (name, workers, rounds, warmup) in [
        ("wake_to_run_futex", 1, contract::ITERS, contract::WARMUP),
        ("smp_balance_futex", 8, 256, 8),
    ] {
        match collect::<false>(workers, rounds, warmup) {
            Some((mut samples, placement, _)) => {
                report(name, &summarize(&mut samples), khz);
                for (apic, samples) in placement {
                    debug_line(&format!(
                        "ipcbench: placement name={name} apic={apic} samples={samples}"
                    ));
                }
            }
            None => skip(name, "futex-wake-not-parked-or-transcript-failed"),
        }
    }
}
