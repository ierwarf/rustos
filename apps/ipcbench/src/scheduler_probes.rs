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

fn collect(
    workers: usize,
    rounds: usize,
    warmup: usize,
) -> Option<(Vec<u64>, BTreeMap<u32, usize>)> {
    let gates: Vec<_> = (0..workers).map(|_| Arc::new(Gate::default())).collect();
    let mut threads = Vec::with_capacity(workers);
    for gate in &gates {
        let gate = Arc::clone(gate);
        match std::thread::Builder::new().spawn(move || {
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
        for (gate, start) in gates.iter().zip(&mut starts) {
            *start = tsc();
            gate.command.store(1, Ordering::Release);
            if futex(&gate.command, libc::FUTEX_WAKE, 1) != 1 {
                valid = false;
                break 'rounds;
            }
        }
        for (gate, start) in gates.iter().zip(&starts) {
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
                samples.push(end - start);
                *placement
                    .entry(gate.apic.load(Ordering::Relaxed))
                    .or_insert(0) += 1;
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
    (valid && samples.len() == workers * rounds).then_some((samples, placement))
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

pub fn run(khz: u64) {
    crate::probe_sched_yield(khz);
    for (name, workers, rounds, warmup) in [
        ("wake_to_run_futex", 1, contract::ITERS, contract::WARMUP),
        ("smp_balance_futex", 8, 256, 8),
    ] {
        match collect(workers, rounds, warmup) {
            Some((mut samples, placement)) => {
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
