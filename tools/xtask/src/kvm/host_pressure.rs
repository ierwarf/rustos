// SPDX-License-Identifier: MIT
//
// Opt-in host CPU contention for a KVM smoke run (`--host-cpu-pressure`).
//
// Owner: this file owns the pressure profile: how many busy workers a
// percentage means, which host cores they run on, and their lifetime. The
// argument parser lives in `smoke_args.rs`; launching lives in `options.rs`.
//
// Why it exists: readiness and timing claims taken on an otherwise idle host
// describe the best case. A guest that boots in time only when it owns every
// core has not shown that it boots in time on a loaded workstation, which is
// where the timing contract is actually consumed. The profile makes the
// loaded case reproducible: a fixed fraction of the cores the guest is allowed
// to run on is kept 100% busy for the whole run.
//
// Boundary: the profile adds host load only. It never touches the guest, the
// QEMU command line, or the evidence the run records, so a run under pressure
// is a different *timing profile* of the same topology, not a different
// topology. Evidence modes that publish sealed acceptance records
// (`--smp-iteration`, `--smp-ring3-qualification`) do not record the profile,
// so they refuse it instead of mixing profiles.
//
// Failure: a core set that cannot be read, a failed affinity call, or a
// worker that cannot start fails the command before QEMU launches. A run is
// never silently measured without the pressure it asked for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Context, bail};

use crate::Result;

/// Inclusive bounds of `--host-cpu-pressure`, in percent of the allowed cores.
pub(crate) const HOST_CPU_PRESSURE_RANGE: std::ops::RangeInclusive<u8> = 1..=100;

/// Parses the operand of `--host-cpu-pressure`.
pub(crate) fn parse_host_cpu_pressure(value: &str) -> Result<u8> {
    let percent = value
        .parse::<u8>()
        .with_context(|| format!("invalid --host-cpu-pressure value: {value}"))?;
    if !HOST_CPU_PRESSURE_RANGE.contains(&percent) {
        bail!(
            "--host-cpu-pressure must be in {}..={} (percent of the allowed host cores), got {percent}",
            HOST_CPU_PRESSURE_RANGE.start(),
            HOST_CPU_PRESSURE_RANGE.end()
        );
    }
    Ok(percent)
}

/// Number of busy workers for `percent` of `allowed_cores`.
///
/// Rounds up so any requested pressure keeps at least one core busy, and never
/// exceeds the core count: one worker per core is the 100% profile.
pub(crate) fn pressure_worker_count(allowed_cores: usize, percent: u8) -> usize {
    if allowed_cores == 0 || percent == 0 {
        return 0;
    }
    let percent = usize::from(percent.min(*HOST_CPU_PRESSURE_RANGE.end()));
    (allowed_cores * percent).div_ceil(100).min(allowed_cores)
}

/// Spreads `workers` over `cores` so the busy cores are as far apart as the
/// core list allows (every `n`th entry) instead of filling one socket first.
pub(crate) fn pressure_core_assignment(cores: &[usize], workers: usize) -> Vec<usize> {
    if cores.is_empty() || workers == 0 {
        return Vec::new();
    }
    let workers = workers.min(cores.len());
    (0..workers)
        .map(|worker| cores[worker * cores.len() / workers])
        .collect()
}

/// Host cores this process (and therefore the QEMU it launches) may run on.
fn allowed_host_cores() -> Result<Vec<usize>> {
    // SAFETY: `cpu_set_t` is plain data and `sched_getaffinity` writes at most
    // `size_of::<cpu_set_t>()` bytes into it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let result =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if result != 0 {
        bail!(
            "sched_getaffinity failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let cores = (0..libc::CPU_SETSIZE as usize)
        // SAFETY: `set` was initialised above and `core` is below CPU_SETSIZE.
        .filter(|core| unsafe { libc::CPU_ISSET(*core, &set) })
        .collect::<Vec<_>>();
    if cores.is_empty() {
        bail!("sched_getaffinity returned an empty core set");
    }
    Ok(cores)
}

/// Pins the calling thread to one host core.
fn pin_current_thread(core: usize) -> Result<()> {
    // SAFETY: as above; `pid` 0 selects the calling thread.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_SET(core, &mut set) };
    let result =
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if result != 0 {
        bail!(
            "sched_setaffinity(core {core}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Busy workers that exist exactly as long as this value.
#[derive(Debug)]
pub(crate) struct HostCpuPressure {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    cores: Vec<usize>,
    allowed_cores: usize,
    percent: u8,
}

impl HostCpuPressure {
    /// Starts the profile and returns only after every worker is pinned.
    pub(crate) fn start(percent: u8) -> Result<Self> {
        let allowed = allowed_host_cores()?;
        let count = pressure_worker_count(allowed.len(), percent);
        let cores = pressure_core_assignment(&allowed, count);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let mut workers = Vec::with_capacity(cores.len());
        for core in &cores {
            let core = *core;
            let worker_stop = Arc::clone(&stop);
            let ready = ready_tx.clone();
            let spawned = thread::Builder::new()
                .name(format!("host-pressure-{core}"))
                .spawn(move || {
                    let pinned = pin_current_thread(core);
                    let failed = pinned.is_err();
                    let _ = ready.send(pinned);
                    if failed {
                        return;
                    }
                    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ core as u64;
                    while !worker_stop.load(Ordering::Relaxed) {
                        // Opaque to the optimiser, so the loop stays busy.
                        for _ in 0..4096 {
                            state = std::hint::black_box(
                                state
                                    .wrapping_mul(6_364_136_223_846_793_005)
                                    .wrapping_add(1_442_695_040_888_963_407),
                            );
                        }
                    }
                });
            match spawned {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    let partial = Self::assemble(stop, workers, Vec::new(), allowed.len(), percent);
                    drop(partial);
                    return Err(error).context("start host CPU pressure worker");
                }
            }
        }
        drop(ready_tx);
        let mut result = Ok(());
        for _ in 0..workers.len() {
            match ready_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(error),
                Err(_) => result = Err(anyhow::anyhow!("host CPU pressure worker exited early")),
            }
        }
        let pressure = Self::assemble(stop, workers, cores, allowed.len(), percent);
        result?;
        Ok(pressure)
    }

    fn assemble(
        stop: Arc<AtomicBool>,
        workers: Vec<JoinHandle<()>>,
        cores: Vec<usize>,
        allowed_cores: usize,
        percent: u8,
    ) -> Self {
        Self {
            stop,
            workers,
            cores,
            allowed_cores,
            percent,
        }
    }

    /// One-line description printed before the run so the profile is part of
    /// every transcript that quotes a timing result.
    pub(crate) fn describe(&self) -> String {
        format!(
            "xtask: timing profile host-cpu-pressure={}% workers={} of {} allowed host cores (pinned to cores {:?})",
            self.percent,
            self.cores.len(),
            self.allowed_cores,
            self.cores
        )
    }
}

impl Drop for HostCpuPressure {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operand_is_a_percentage_of_the_allowed_cores() {
        assert_eq!(parse_host_cpu_pressure("1").unwrap(), 1);
        assert_eq!(parse_host_cpu_pressure("50").unwrap(), 50);
        assert_eq!(parse_host_cpu_pressure("100").unwrap(), 100);
        for bad in ["0", "101", "255", "256", "-1", "", "fifty", "12.5", "50%"] {
            assert!(parse_host_cpu_pressure(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn worker_count_rounds_up_and_never_exceeds_the_core_count() {
        assert_eq!(pressure_worker_count(16, 50), 8);
        assert_eq!(pressure_worker_count(16, 100), 16);
        assert_eq!(pressure_worker_count(16, 1), 1);
        assert_eq!(pressure_worker_count(16, 6), 1);
        assert_eq!(pressure_worker_count(16, 7), 2);
        assert_eq!(pressure_worker_count(12, 25), 3);
        assert_eq!(pressure_worker_count(1, 100), 1);
        assert_eq!(pressure_worker_count(3, 34), 2);
        assert_eq!(pressure_worker_count(0, 50), 0);
        assert_eq!(pressure_worker_count(8, 0), 0);
        // Out-of-range input is clamped, never over-subscribed.
        assert_eq!(pressure_worker_count(8, 200), 8);
        for cores in 1..=64 {
            for percent in HOST_CPU_PRESSURE_RANGE {
                let workers = pressure_worker_count(cores, percent);
                assert!((1..=cores).contains(&workers), "{cores} cores {percent}%");
                assert!(
                    workers * 100 >= cores * usize::from(percent),
                    "under-delivers"
                );
                assert!(
                    (workers - 1) * 100 < cores * usize::from(percent),
                    "over-delivers"
                );
            }
        }
    }

    #[test]
    fn workers_land_on_distinct_allowed_cores_spread_across_the_list() {
        let cores = [2, 3, 4, 5, 8, 9, 10, 11];
        assert_eq!(pressure_core_assignment(&cores, 4), vec![2, 4, 8, 10]);
        assert_eq!(pressure_core_assignment(&cores, 8), cores.to_vec());
        assert_eq!(pressure_core_assignment(&cores, 20), cores.to_vec());
        assert_eq!(pressure_core_assignment(&cores, 1), vec![2]);
        assert!(pressure_core_assignment(&cores, 0).is_empty());
        assert!(pressure_core_assignment(&[], 3).is_empty());
        for workers in 1..=cores.len() {
            let assigned = pressure_core_assignment(&cores, workers);
            assert_eq!(assigned.len(), workers);
            let mut unique = assigned.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(
                unique.len(),
                workers,
                "duplicate core for {workers} workers"
            );
            assert!(assigned.iter().all(|core| cores.contains(core)));
        }
    }

    #[test]
    fn started_workers_stop_when_the_guard_drops() {
        let allowed = allowed_host_cores().unwrap();
        let pressure = HostCpuPressure::start(1).unwrap();
        assert_eq!(
            pressure.cores.len(),
            pressure_worker_count(allowed.len(), 1)
        );
        assert!(pressure.cores.iter().all(|core| allowed.contains(core)));
        assert!(pressure.describe().contains("host-cpu-pressure=1%"));
        let stop = Arc::clone(&pressure.stop);
        drop(pressure);
        assert!(stop.load(Ordering::Relaxed));
    }
}
