//! Memory-pressure victim policy for the root supervisor.
//!
//! - **Owner:** `rootd` decides whether memory pressure warrants a kill and
//!   which process dies; ring0 only measures (`SYS_RUSTOS_ROOTD_MEMORY_BROKER`)
//!   and tears the victim down (`SYS_RUSTOS_ROOTD_TERMINATE_BROKER`).
//! - **Boundary:** The census is a kernel snapshot; a process in it may have
//!   exited by the time it is terminated, which the terminate path tolerates.
//! - **Lifecycle:** One assessment per supervisor pass. A kill starts a
//!   cooldown so frames that are still being retired do not cascade into a
//!   second kill.
//! - **Concurrency:** Single-threaded supervisor state; no lock is held.
//! - **Failure:** No qualifying victim is reported once per pressure episode
//!   and leaves the system degrading into refused faults, not a wider kill.
//! - **Forbidden:** Killing `rootd`, any core or post-init lease, or a lease's
//!   reporter; allocating while governing, because this runs exactly when the
//!   user-class allocator is refusing.
//! - **Evidence:** `memory_governor` unit tests; contract
//!   `address-space-lifecycle-contract.md` §4.

use rustos_user_abi::syscall::{ROOTD_MEMORY_CENSUS_MAX_ENTRIES, RustosRootdMemoryBrokerArgs};

/// A kill is followed by this long during which no further kill is issued.
pub(super) const KILL_COOLDOWN_MS: u64 = 200;
/// A victim's frames are considered back once free memory is this many times
/// the reserve, well clear of the watermark that triggered the kill, **and**
/// at least half of the victim's resident set has been returned. Retirement
/// runs after the kill, so a bare reserve multiple is crossed while most of
/// the victim is still being torn down.
pub(super) const RECOVERY_RESERVE_MULTIPLE: u64 = 4;
/// Processes mapping fewer pages than this are never worth a kill.
pub(super) const MIN_VICTIM_PAGES: u64 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Verdict {
    /// No pressure, or pressure already being handled.
    Idle,
    /// Pressure, but an earlier victim is still inside its cooldown.
    Cooldown,
    /// Pressure with nothing eligible to kill; reported once per episode.
    NoVictim,
    /// An earlier victim's frames are back: free memory is well above the
    /// reserve again. Reported once per kill.
    Recovered { free_pages: u64 },
    /// Terminate this process alone.
    Kill {
        process_id: u64,
        resident_pages: u64,
    },
    /// Kill this whole job: `resident_pages` is the census sum of its eligible
    /// members and `process_id` its largest one, for the log line only.
    KillJob {
        job: u64,
        process_id: u64,
        resident_pages: u64,
    },
}

pub(super) struct MemoryGovernor {
    last_denials: u64,
    cooldown_until_ms: u64,
    reported_no_victim: bool,
    announced: bool,
    /// Free pages that mean the last victim's memory is back; zero when no
    /// kill is awaiting recovery.
    recovery_floor_pages: u64,
}

impl MemoryGovernor {
    pub(super) const fn new() -> Self {
        Self {
            last_denials: 0,
            cooldown_until_ms: 0,
            reported_no_victim: false,
            announced: false,
            recovery_floor_pages: 0,
        }
    }

    /// True exactly once, on the first call: lets the supervisor log a single
    /// line proving the census path works without logging every pass.
    pub(super) fn first_report(&mut self) -> bool {
        !core::mem::replace(&mut self.announced, true)
    }

    /// Decides what one census warrants.
    ///
    /// Pressure is free memory at or below the kernel reserve **and** new
    /// refusals since the previous pass: a quiet machine resting at the
    /// watermark has nobody to protect and nobody to blame.
    #[cfg(test)]
    pub(super) fn assess(
        &mut self,
        now_ms: u64,
        census: &RustosRootdMemoryBrokerArgs,
        rootd_pid: u64,
        is_protected: impl FnMut(u64) -> bool,
    ) -> Verdict {
        self.assess_with_jobs(now_ms, census, rootd_pid, is_protected, |_| false)
    }

    /// `assess` that prefers ending a whole job. `job_killable(job)` must be
    /// true only when killing `job` and everything below it cannot end a
    /// protected process; it may be expensive and is called only after the
    /// pressure and cooldown checks, largest job first, until one answers yes.
    /// With no killable job the verdict is the single largest eligible process.
    pub(super) fn assess_with_jobs(
        &mut self,
        now_ms: u64,
        census: &RustosRootdMemoryBrokerArgs,
        rootd_pid: u64,
        mut is_protected: impl FnMut(u64) -> bool,
        mut job_killable: impl FnMut(u64) -> bool,
    ) -> Verdict {
        let denied_since_last = census.watermark_denials > self.last_denials;
        self.last_denials = census.watermark_denials;
        let below_reserve = census.free_pages <= census.min_free_pages;
        if !below_reserve {
            self.reported_no_victim = false;
        }
        if self.recovery_floor_pages != 0
            && census.free_pages
                >= self.recovery_floor_pages.max(
                    census
                        .min_free_pages
                        .saturating_mul(RECOVERY_RESERVE_MULTIPLE),
                )
        {
            self.recovery_floor_pages = 0;
            return Verdict::Recovered {
                free_pages: census.free_pages,
            };
        }
        if !(below_reserve && denied_since_last) {
            return Verdict::Idle;
        }
        if now_ms < self.cooldown_until_ms {
            return Verdict::Cooldown;
        }
        let count = (census.entry_count as usize).min(census.entries.len());
        let mut best: Option<(u64, u64)> = None;
        // (job, eligible resident pages, largest eligible pid) per distinct job.
        let mut jobs = [(0_u64, 0_u64, 0_u64); ROOTD_MEMORY_CENSUS_MAX_ENTRIES];
        let mut job_count = 0;
        for entry in &census.entries[..count] {
            let pid = entry.process_id;
            if pid == 0
                || pid == rootd_pid
                || entry.resident_pages < MIN_VICTIM_PAGES
                || is_protected(pid)
            {
                continue;
            }
            if entry.job != 0 {
                match jobs[..job_count].iter_mut().find(|slot| slot.0 == entry.job) {
                    Some(slot) => {
                        slot.1 = slot.1.saturating_add(entry.resident_pages);
                        slot.2 = slot.2.max(pid);
                    }
                    None => {
                        jobs[job_count] = (entry.job, entry.resident_pages, pid);
                        job_count += 1;
                    }
                }
            }
            // The census is largest-first, but the policy does not depend on
            // the kernel's ordering: compare explicitly, ties to the younger.
            if best.is_none_or(|(pages, best_pid)| (entry.resident_pages, pid) > (pages, best_pid))
            {
                best = Some((entry.resident_pages, pid));
            }
        }
        // Largest job first. The census is partial (largest processes only), so
        // a job's sum is a lower bound and never overstates what it frees.
        let mut probed = [false; ROOTD_MEMORY_CENSUS_MAX_ENTRIES];
        while best.is_some() {
            let Some(next) = (0..job_count)
                .filter(|&i| !probed[i])
                .max_by_key(|&i| (jobs[i].1, jobs[i].2))
            else {
                break;
            };
            probed[next] = true;
            let (job, resident_pages, process_id) = jobs[next];
            if job_killable(job) {
                self.cooldown_until_ms = now_ms.saturating_add(KILL_COOLDOWN_MS);
                self.reported_no_victim = false;
                self.recovery_floor_pages =
                    census.free_pages.saturating_add(resident_pages / 2).max(1);
                return Verdict::KillJob {
                    job,
                    process_id,
                    resident_pages,
                };
            }
        }
        match best {
            Some((resident_pages, process_id)) => {
                self.cooldown_until_ms = now_ms.saturating_add(KILL_COOLDOWN_MS);
                self.reported_no_victim = false;
                self.recovery_floor_pages =
                    census.free_pages.saturating_add(resident_pages / 2).max(1);
                Verdict::Kill {
                    process_id,
                    resident_pages,
                }
            }
            None if self.reported_no_victim => Verdict::Idle,
            None => {
                self.reported_no_victim = true;
                Verdict::NoVictim
            }
        }
    }
}

/// Formats `prefix` followed by space-separated decimal `values` and a newline
/// into `out`, returning the length. Truncates rather than allocating.
pub(super) fn format_line(out: &mut [u8], prefix: &[u8], values: &[u64]) -> usize {
    let mut len = 0;
    let mut put = |byte: u8, len: &mut usize| {
        if *len < out.len() {
            out[*len] = byte;
            *len += 1;
        }
    };
    for &byte in prefix {
        put(byte, &mut len);
    }
    for &value in values {
        put(b' ', &mut len);
        let mut digits = [0_u8; 20];
        let mut count = 0;
        let mut rest = value;
        loop {
            digits[count] = b'0' + (rest % 10) as u8;
            count += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            put(digits[count], &mut len);
        }
    }
    put(b'\n', &mut len);
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::RootdMemoryCensusEntry;

    const ROOTD: u64 = 1;

    fn census(
        free: u64,
        min_free: u64,
        denials: u64,
        entries: &[(u64, u64)],
    ) -> RustosRootdMemoryBrokerArgs {
        let mut reply = RustosRootdMemoryBrokerArgs {
            free_pages: free,
            min_free_pages: min_free,
            watermark_denials: denials,
            entry_count: entries.len() as u32,
            ..RustosRootdMemoryBrokerArgs::default()
        };
        for (slot, &(process_id, resident_pages)) in reply.entries.iter_mut().zip(entries) {
            *slot = RootdMemoryCensusEntry {
                process_id,
                parent_process_id: 1,
                resident_pages,
                job: 1,
            };
        }
        reply
    }

    fn none_protected(_: u64) -> bool {
        false
    }

    #[test]
    fn a_machine_resting_at_the_watermark_with_no_new_refusals_is_left_alone() {
        let mut governor = MemoryGovernor::new();
        let reply = census(100, 100, 0, &[(7, 5000)]);
        assert_eq!(
            governor.assess(0, &reply, ROOTD, none_protected),
            Verdict::Idle
        );
        let reply = census(100, 100, 0, &[(7, 5000)]);
        assert_eq!(
            governor.assess(10, &reply, ROOTD, none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn refusals_with_free_memory_above_the_reserve_are_not_pressure() {
        let mut governor = MemoryGovernor::new();
        let reply = census(500, 100, 3, &[(7, 5000)]);
        assert_eq!(
            governor.assess(0, &reply, ROOTD, none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn new_refusals_at_the_reserve_kill_the_largest_eligible_process() {
        let mut governor = MemoryGovernor::new();
        let reply = census(90, 100, 1, &[(9, 40), (7, 5000), (8, 300)]);
        assert_eq!(
            governor.assess(0, &reply, ROOTD, none_protected),
            Verdict::Kill {
                process_id: 7,
                resident_pages: 5000
            }
        );
    }

    #[test]
    fn rootd_protected_pids_and_tiny_processes_are_never_victims() {
        let mut governor = MemoryGovernor::new();
        let reply = census(
            90,
            100,
            1,
            &[
                (ROOTD, 9000),
                (4, 8000),
                (5, 7000),
                (6, MIN_VICTIM_PAGES - 1),
                (8, 400),
            ],
        );
        assert_eq!(
            governor.assess(0, &reply, ROOTD, |pid| pid == 4 || pid == 5),
            Verdict::Kill {
                process_id: 8,
                resident_pages: 400
            }
        );
    }

    #[test]
    fn equal_sized_processes_resolve_to_the_younger_pid_whatever_the_order() {
        let mut governor = MemoryGovernor::new();
        let reply = census(90, 100, 1, &[(30, 500), (31, 500), (29, 500)]);
        assert_eq!(
            governor.assess(0, &reply, ROOTD, none_protected),
            Verdict::Kill {
                process_id: 31,
                resident_pages: 500
            }
        );
    }

    #[test]
    fn a_second_kill_waits_for_the_cooldown_even_if_refusals_continue() {
        let mut governor = MemoryGovernor::new();
        let first = census(90, 100, 1, &[(7, 5000), (8, 300)]);
        assert!(matches!(
            governor.assess(1_000, &first, ROOTD, none_protected),
            Verdict::Kill { process_id: 7, .. }
        ));
        let during = census(90, 100, 2, &[(8, 300)]);
        assert_eq!(
            governor.assess(1_000 + KILL_COOLDOWN_MS - 1, &during, ROOTD, none_protected),
            Verdict::Cooldown
        );
        let after = census(90, 100, 3, &[(8, 300)]);
        assert_eq!(
            governor.assess(1_000 + KILL_COOLDOWN_MS, &after, ROOTD, none_protected),
            Verdict::Kill {
                process_id: 8,
                resident_pages: 300
            }
        );
    }

    #[test]
    fn refusals_consumed_during_a_cooldown_do_not_trigger_a_kill_after_it() {
        let mut governor = MemoryGovernor::new();
        let first = census(90, 100, 1, &[(7, 5000)]);
        assert!(matches!(
            governor.assess(0, &first, ROOTD, none_protected),
            Verdict::Kill { .. }
        ));
        let during = census(90, 100, 2, &[(8, 300)]);
        assert_eq!(
            governor.assess(10, &during, ROOTD, none_protected),
            Verdict::Cooldown
        );
        let quiet = census(90, 100, 2, &[(8, 300)]);
        assert_eq!(
            governor.assess(KILL_COOLDOWN_MS + 50, &quiet, ROOTD, none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn no_victim_is_reported_once_per_episode_and_rearms_after_recovery() {
        let mut governor = MemoryGovernor::new();
        let only_protected = |denials| census(90, 100, denials, &[(4, 9000)]);
        assert_eq!(
            governor.assess(0, &only_protected(1), ROOTD, |pid| pid == 4),
            Verdict::NoVictim
        );
        assert_eq!(
            governor.assess(10, &only_protected(2), ROOTD, |pid| pid == 4),
            Verdict::Idle
        );
        let recovered = census(900, 100, 2, &[(4, 9000)]);
        assert_eq!(
            governor.assess(20, &recovered, ROOTD, |pid| pid == 4),
            Verdict::Idle
        );
        assert_eq!(
            governor.assess(30, &only_protected(3), ROOTD, |pid| pid == 4),
            Verdict::NoVictim
        );
    }

    #[test]
    fn an_entry_count_beyond_the_array_is_clamped_not_trusted() {
        let mut governor = MemoryGovernor::new();
        let mut reply = census(90, 100, 1, &[(7, 5000)]);
        reply.entry_count = u32::MAX;
        assert!(matches!(
            governor.assess(0, &reply, ROOTD, none_protected),
            Verdict::Kill { process_id: 7, .. }
        ));
    }

    #[test]
    fn recovery_is_reported_once_after_the_victims_memory_is_back() {
        let mut governor = MemoryGovernor::new();
        // Free 90 at the kill and a 5000-page victim: half of it is 2500.
        let pressure = census(90, 100, 1, &[(7, 5000)]);
        assert!(matches!(
            governor.assess(0, &pressure, ROOTD, none_protected),
            Verdict::Kill { .. }
        ));
        // Past four reserves (400) but only part of the victim returned.
        let partial = census(2_589, 100, 1, &[]);
        assert_eq!(
            governor.assess(10, &partial, ROOTD, none_protected),
            Verdict::Idle
        );
        let back = census(2_590, 100, 1, &[]);
        assert_eq!(
            governor.assess(20, &back, ROOTD, none_protected),
            Verdict::Recovered { free_pages: 2_590 }
        );
        assert_eq!(
            governor.assess(30, &back, ROOTD, none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn no_recovery_is_reported_when_nothing_was_killed() {
        let mut governor = MemoryGovernor::new();
        let roomy = census(10_000, 100, 0, &[]);
        assert_eq!(
            governor.assess(0, &roomy, ROOTD, none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn the_first_report_is_announced_exactly_once() {
        let mut governor = MemoryGovernor::new();
        assert!(governor.first_report());
        assert!(!governor.first_report());
        assert!(!governor.first_report());
    }

    #[test]
    fn format_line_writes_decimals_and_truncates_without_panicking() {
        let mut out = [0_u8; 64];
        let len = format_line(&mut out, b"oom:", &[0, 7, 18_446_744_073_709_551_615]);
        assert_eq!(&out[..len], b"oom: 0 7 18446744073709551615\n");
        let mut small = [0_u8; 6];
        let len = format_line(&mut small, b"oom:", &[12345]);
        assert_eq!(len, 6);
        assert_eq!(&small[..len], b"oom: 1");
    }

    fn census_in_jobs(entries: &[(u64, u64, u64)]) -> RustosRootdMemoryBrokerArgs {
        let mut reply = census(10, 64, 5, &[]);
        reply.entry_count = entries.len() as u32;
        for (slot, &(process_id, resident_pages, job)) in reply.entries.iter_mut().zip(entries) {
            *slot = RootdMemoryCensusEntry { process_id, parent_process_id: 1, resident_pages, job };
        }
        reply
    }

    #[test]
    fn the_largest_killable_job_dies_as_a_whole() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 7), (11, 40, 7), (12, 70, 8)]);
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |job| job == 7);
        assert_eq!(
            verdict,
            Verdict::KillJob { job: 7, process_id: 11, resident_pages: 90 },
            "job 7 holds 90 pages across two processes, more than job 8's single 70"
        );
    }

    #[test]
    fn jobs_are_probed_largest_first_and_probing_stops_at_the_first_yes() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 30, 5), (11, 80, 6), (12, 50, 7)]);
        let mut probed = std::vec::Vec::new();
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |job| {
            probed.push(job);
            job == 7
        });
        assert_eq!(probed, [6, 7], "job 5 is never asked after job 7 answers yes");
        assert_eq!(verdict, Verdict::KillJob { job: 7, process_id: 12, resident_pages: 50 });
    }

    #[test]
    fn with_no_killable_job_the_single_largest_process_is_the_victim() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 7), (11, 40, 7), (12, 70, 8)]);
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |_| false);
        assert_eq!(verdict, Verdict::Kill { process_id: 12, resident_pages: 70 });
    }

    #[test]
    fn a_protected_member_never_counts_toward_its_jobs_sum() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 7), (11, 40, 7), (12, 70, 8)]);
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |pid| pid == 10, |job| job == 7);
        assert_eq!(verdict, Verdict::KillJob { job: 7, process_id: 11, resident_pages: 40 });
    }

    #[test]
    fn jobs_are_not_probed_without_pressure_or_during_a_cooldown() {
        let mut governor = MemoryGovernor::new();
        let mut quiet = census_in_jobs(&[(10, 50, 7)]);
        quiet.free_pages = quiet.min_free_pages + 1;
        let mut asked = false;
        let verdict = governor.assess_with_jobs(1_000, &quiet, ROOTD, |_| false, |_| { asked = true; true });
        assert_eq!((verdict, asked), (Verdict::Idle, false));

        // New refusals since the quiet pass above, otherwise pressure is "resting".
        let mut pressured = census_in_jobs(&[(10, 50, 7)]);
        pressured.watermark_denials = 6;
        assert!(matches!(
            governor.assess_with_jobs(1_000, &pressured, ROOTD, |_| false, |_| true),
            Verdict::KillJob { .. }
        ));
        let mut again = census_in_jobs(&[(10, 50, 7)]);
        again.watermark_denials = 7;
        asked = false;
        let verdict = governor.assess_with_jobs(1_001, &again, ROOTD, |_| false, |_| { asked = true; true });
        assert_eq!((verdict, asked), (Verdict::Cooldown, false));
    }

    #[test]
    fn a_process_with_no_known_job_is_only_ever_a_single_process_victim() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 0)]);
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |_| true);
        assert_eq!(verdict, Verdict::Kill { process_id: 10, resident_pages: 50 });
    }

    #[test]
    fn a_whole_job_kill_is_authorized_only_after_every_protected_pid_is_proven_outside_it() {
        // Source witness for the syscall-backed edge, which has no host fake.
        let main = include_str!("main.rs");
        let safe = main.split_once("fn job_kill_is_safe(").unwrap().1
            .split_once("fn kill_job_by_id(").unwrap().0;
        assert!(safe.contains("if job == rustos_user_abi::syscall::JOB_ID_ROOT"));
        assert!(safe.contains(".all(|&pid| jobs::process_job_in(handle, pid) == Err(ESRCH))"));
        // Inspection only: a probe handle must not carry kill authority.
        assert!(safe.contains("rustos_user_abi::syscall::JOB_RIGHT_INSPECT,\n    ) else"));
        assert!(!safe.contains("JOB_RIGHT_KILL"));
        let kill = main.split_once("fn kill_job_by_id(").unwrap().1
            .split_once("fn terminate_service_process(").unwrap().0;
        assert!(kill.contains("JOB_RIGHT_INSPECT | rustos_user_abi::syscall::JOB_RIGHT_KILL"));
        let supervise = main.split_once("fn supervise_memory_pressure(").unwrap().1
            .split_once("fn job_kill_is_safe(").unwrap().0;
        assert!(supervise.contains("|job| !protected_overflow && job_kill_is_safe(job, protected)"));
    }

    #[test]
    fn a_job_kill_arms_recovery_reporting_like_a_process_kill() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 7), (11, 40, 7)]);
        assert!(matches!(
            governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |_| true),
            Verdict::KillJob { .. }
        ));
        let mut after = census_in_jobs(&[]);
        after.free_pages = 300;
        after.watermark_denials = census.watermark_denials;
        assert_eq!(
            governor.assess_with_jobs(2_000, &after, ROOTD, |_| false, |_| true),
            Verdict::Recovered { free_pages: 300 }
        );
    }
}
