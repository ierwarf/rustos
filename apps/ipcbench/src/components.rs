//! Diagnostic component suite. Every operation uses the production syscall/IPC
//! path. No allocation, output, or semantic checking is inside its timed span.

use crate::{
    component_contract as contract, debug_line, force_drain_phase_profiles, ipc_probes,
    measure_stamped, probe_tsc_overhead, probe_vmexit_cpuid, report, skip, summarize, syscall0,
    syscall2, tsc, KTimespec, CLOCK_MONOTONIC, SYS_LINUX_CLOCK_GETTIME, SYS_LINUX_GETPID,
    SYS_LINUX_SCHED_YIELD, WARMUP,
};

const SYS_LINUX_GETTID: u64 = 186;

fn bracketed_probe(
    name: &str,
    mut emit: impl FnMut(&str),
    mut drain: impl FnMut(),
    probe: impl FnOnce(),
) {
    drain();
    emit(&format!("{}{name}", contract::BEGIN));
    probe();
    drain();
    emit(&format!("{}{name}", contract::END));
}

pub(crate) fn run(tsc_khz: u64) {
    for &(name, _) in contract::PROBES {
        bracketed_probe(
            name,
            debug_line,
            force_drain_phase_profiles,
            || match name {
                "vmexit_cpuid" => probe_vmexit_cpuid(tsc_khz),
                "tsc_overhead" => probe_tsc_overhead(tsc_khz),
                "null_syscall_getpid" => probe_identity(name, SYS_LINUX_GETPID, tsc_khz),
                "null_syscall_gettid" => probe_identity(name, SYS_LINUX_GETTID, tsc_khz),
                "clock_gettime" => probe_clock(tsc_khz),
                "sched_yield" => probe_yield(tsc_khz),
                "ipc_try_recv_empty" => ipc_probes::probe_ipc_mechanism_with_iterations(
                    tsc_khz,
                    contract::PRIMITIVE_ITERS,
                ),
                "ipc_rt_intra_process" => ipc_probes::probe_ipc_intra_process(tsc_khz),
                "ipc_rt_intra_process_reply_recv" => {
                    ipc_probes::probe_ipc_intra_process_reply_recv(tsc_khz)
                }
                _ => unreachable!("closed component contract has no app implementation"),
            },
        );
    }
}

fn valid_identity(expected: i64, observed: i64) -> bool {
    expected > 0 && observed == expected
}

#[inline(never)]
fn probe_identity(name: &str, number: u64, tsc_khz: u64) {
    let expected = unsafe { syscall0(number) };
    let samples = if expected > 0 {
        measure_stamped(contract::PRIMITIVE_ITERS, WARMUP, || {
            let start = tsc();
            let observed = unsafe { syscall0(number) };
            let end = tsc();
            valid_identity(expected, observed).then_some(end.wrapping_sub(start))
        })
    } else {
        None
    };
    finish(name, samples, tsc_khz);
}

fn valid_clock(status: i64, value: &KTimespec) -> bool {
    status == 0 && value.tv_sec >= 0 && (0..1_000_000_000).contains(&value.tv_nsec)
}

#[inline(never)]
fn probe_clock(tsc_khz: u64) {
    let samples = measure_stamped(contract::PRIMITIVE_ITERS, WARMUP, || {
        // Poison before the interval: a successful syscall that forgot to copy
        // either output field cannot reuse a previous sample's valid bytes.
        let mut value = KTimespec {
            tv_sec: -1,
            tv_nsec: -1,
        };
        let start = tsc();
        let status = unsafe {
            syscall2(
                SYS_LINUX_CLOCK_GETTIME,
                CLOCK_MONOTONIC,
                (&mut value as *mut KTimespec) as u64,
            )
        };
        let end = tsc();
        valid_clock(status, &value).then_some(end.wrapping_sub(start))
    });
    finish("clock_gettime", samples, tsc_khz);
}

#[inline(never)]
fn probe_yield(tsc_khz: u64) {
    let samples = measure_stamped(contract::PRIMITIVE_ITERS, WARMUP, || {
        let start = tsc();
        let status = unsafe { syscall0(SYS_LINUX_SCHED_YIELD) };
        let end = tsc();
        (status == 0).then_some(end.wrapping_sub(start))
    });
    finish("sched_yield", samples, tsc_khz);
}

fn finish(name: &str, samples: Option<Vec<u64>>, tsc_khz: u64) {
    match samples {
        Some(mut samples) => report(name, &summarize(&mut samples), tsc_khz),
        None => skip(name, "component-semantic-check-failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn actual_component_bracket_drains_outside_operation_and_closes_after_it() {
        let events = RefCell::new(Vec::new());
        bracketed_probe(
            "sched_yield",
            |line| events.borrow_mut().push(line.to_owned()),
            || events.borrow_mut().push("drain".into()),
            || events.borrow_mut().push("actual operation".into()),
        );
        assert_eq!(
            *events.borrow(),
            [
                "drain",
                "ipcbench: component begin name=sched_yield",
                "actual operation",
                "drain",
                "ipcbench: component end name=sched_yield"
            ]
        );
    }

    #[test]
    fn identity_and_clock_failures_are_not_cheap_component_samples() {
        assert!(valid_identity(17, 17));
        for (expected, observed) in [(0, 0), (-1, -1), (17, 0), (17, 18), (17, -38)] {
            assert!(!valid_identity(expected, observed));
        }
        assert!(valid_clock(
            0,
            &KTimespec {
                tv_sec: 1,
                tv_nsec: 999_999_999
            }
        ));
        for (status, sec, nsec) in [(-1, 1, 1), (0, -1, 1), (0, 1, -1), (0, 1, 1_000_000_000)] {
            assert!(!valid_clock(
                status,
                &KTimespec {
                    tv_sec: sec,
                    tv_nsec: nsec
                }
            ));
        }
    }
}
