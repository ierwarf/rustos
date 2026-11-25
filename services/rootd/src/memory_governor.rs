//! Memory-pressure victim policy for the root supervisor.
//!
//! - **Owner:** `rootd` decides whether memory pressure warrants a kill and
//!   which process dies; ring0 only measures (`SYS_RUSTOS_ROOTD_MEMORY_BROKER`)
//!   and tears the victim down (`SYS_RUSTOS_ROOTD_TERMINATE_BROKER`).
//! - **Boundary:** The census is a kernel snapshot; a process in it may have
//!   exited by the time it is terminated, which the terminate path tolerates.
//! - **Lifecycle:** One assessment per pressure pass. A kill opens a *settle
//!   window*: no further victim is chosen until every killed pid has left the
//!   census **and** frames have stopped arriving for `SETTLE_QUIET_MS` (or
//!   `SETTLE_MAX_MS` passed). A victim that is still listed after
//!   `STUCK_RETRY_MS` is terminated again, never replaced by an innocent one.
//! - **Concurrency:** Single-threaded supervisor state; no lock is held.
//! - **Failure:** No qualifying victim is reported once per pressure episode
//!   and leaves the system degrading into refused faults, not a wider kill.
//! - **Forbidden:** Killing `rootd`, any core or post-init lease, or a lease's
//!   reporter; allocating while governing, because this runs exactly when the
//!   user-class allocator is refusing.
//! - **Evidence:** `memory_governor` unit tests; contract
//!   `address-space-lifecycle-contract.md` §4.

use rustos_user_abi::syscall::{ROOTD_MEMORY_CENSUS_MAX_ENTRIES, RustosRootdMemoryBrokerArgs};

/// Shortest time after a kill before a follow-up victim may be chosen: the
/// victim's teardown, not its disappearance from the census, returns frames.
pub(super) const SETTLE_MIN_MS: u64 = 1_000;
/// Free memory must have stopped rising for this long before a follow-up kill:
/// frames still arriving mean the earlier victim is still being retired.
pub(super) const SETTLE_QUIET_MS: u64 = 300;
/// Hard bound of the settle window once every victim is gone from the census.
pub(super) const SETTLE_MAX_MS: u64 = 5_000;
/// A victim still listed this long after its kill is terminated again.
pub(super) const STUCK_RETRY_MS: u64 = 2_000;
/// After a rejected kill the governor waits this long before asking again.
pub(super) const REJECTED_KILL_BACKOFF_MS: u64 = 200;
/// A victim must be able to return at least this fraction (1/N) of the reserve,
/// otherwise killing it cannot lift the machine out of pressure.
pub(super) const MIN_HELP_RESERVE_DIVISOR: u64 = 4;
/// A victim's frames are considered back once free memory is this many times
/// the reserve, well clear of the watermark that triggered the kill, **and**
/// at least half of the victim's private pages have been returned. Retirement
/// runs after the kill, so a bare reserve multiple is crossed while most of
/// the victim is still being torn down.
pub(super) const RECOVERY_RESERVE_MULTIPLE: u64 = 4;
/// Processes with fewer private pages than this are never worth a kill.
pub(super) const MIN_VICTIM_PAGES: u64 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Verdict {
    /// No pressure, or pressure already being handled.
    Idle,
    /// Pressure, but an earlier victim is still being retired or a rejected
    /// kill is backing off. Silent: `Settling` announced it once.
    Cooldown,
    /// First pass after a kill: no other victim will be considered until the
    /// victim is gone and its frames have stopped arriving. Reported once.
    Settling {
        process_id: u64,
        victims: u64,
    },
    /// An earlier victim is still alive after `STUCK_RETRY_MS`: terminate the
    /// same victim again (`job` is 0 for a single process).
    Stuck {
        job: u64,
        process_id: u64,
    },
    /// Pressure with nothing eligible to kill; reported once per episode.
    NoVictim,
    /// An earlier victim's frames are back: free memory is well above the
    /// reserve again. Reported once per kill.
    Recovered { free_pages: u64 },
    /// After `Recovered`: frames stopped arriving. `free_pages` is the most
    /// free memory seen, so a short sample at the recovery edge is not mistaken
    /// for the end state. Reported once per kill.
    Settled { free_pages: u64 },
    /// Terminate this process alone.
    Kill {
        process_id: u64,
        private_pages: u64,
    },
    /// Kill this whole job: `private_pages` is the census sum of its eligible
    /// members and `process_id` its largest one, for the log line only.
    KillJob {
        job: u64,
        process_id: u64,
        private_pages: u64,
    },
}

/// The kill whose effect has not been observed yet.
#[derive(Clone, Copy)]
struct Pending {
    victims: [u64; ROOTD_MEMORY_CENSUS_MAX_ENTRIES],
    victim_count: usize,
    /// 0 for a single-process kill.
    job: u64,
    lead: u64,
    killed_at_ms: u64,
    /// The most free pages seen since the kill, and when it last grew.
    best_free: u64,
    progress_ms: u64,
    last_retry_ms: u64,
    settling_reported: bool,
}

pub(super) struct MemoryGovernor {
    last_denials: u64,
    backoff_until_ms: u64,
    pending: Option<Pending>,
    /// (best free, last growth ms, started ms) after a reported recovery.
    watch: Option<(u64, u64, u64)>,
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
            backoff_until_ms: 0,
            pending: None,
            watch: None,
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
        if let Some((best, progress_ms, started_ms)) = self.watch.as_mut() {
            if census.free_pages > *best {
                *best = census.free_pages;
                *progress_ms = now_ms;
            }
            if now_ms.saturating_sub(*progress_ms) >= SETTLE_QUIET_MS
                || now_ms.saturating_sub(*started_ms) >= SETTLE_MAX_MS
            {
                let free_pages = *best;
                self.watch = None;
                return Verdict::Settled { free_pages };
            }
        }
        let denied_since_last = census.watermark_denials > self.last_denials;
        self.last_denials = census.watermark_denials;
        let below_reserve = census.free_pages <= census.min_free_pages;
        if !below_reserve {
            self.reported_no_victim = false;
            // Pressure ended: nothing is awaiting a follow-up decision.
            self.pending = None;
        }
        let count = (census.entry_count as usize).min(census.entries.len());
        if self.recovery_floor_pages != 0
            && census.free_pages
                >= self.recovery_floor_pages.max(
                    census
                        .min_free_pages
                        .saturating_mul(RECOVERY_RESERVE_MULTIPLE),
                )
        {
            self.recovery_floor_pages = 0;
            self.pending = None;
            self.watch = Some((census.free_pages, now_ms, now_ms));
            return Verdict::Recovered {
                free_pages: census.free_pages,
            };
        }
        if let Some(pending) = self.pending.as_mut() {
            // Frames arriving, whether or not pressure is being reported this
            // pass, mean the victim is still being retired.
            if census.free_pages > pending.best_free {
                pending.best_free = census.free_pages;
                pending.progress_ms = now_ms;
            }
        }
        if !(below_reserve && denied_since_last) {
            return Verdict::Idle;
        }
        if now_ms < self.backoff_until_ms {
            return Verdict::Cooldown;
        }
        if let Some(pending) = self.pending.as_mut() {
            let elapsed = now_ms.saturating_sub(pending.killed_at_ms);
            let still_listed = census.entries[..count]
                .iter()
                .any(|entry| pending.victims[..pending.victim_count].contains(&entry.process_id));
            if still_listed {
                // Never replace a victim that is still being torn down: the
                // frames an innocent process would free do not exist yet.
                if elapsed >= STUCK_RETRY_MS
                    && now_ms.saturating_sub(pending.last_retry_ms) >= STUCK_RETRY_MS
                {
                    pending.last_retry_ms = now_ms;
                    return Verdict::Stuck {
                        job: pending.job,
                        process_id: pending.lead,
                    };
                }
                return Self::settling(pending);
            }
            let quiet = now_ms.saturating_sub(pending.progress_ms) >= SETTLE_QUIET_MS;
            let settled = elapsed >= SETTLE_MIN_MS && (quiet || elapsed >= SETTLE_MAX_MS);
            if !settled {
                return Self::settling(pending);
            }
            self.pending = None;
        }
        // The census charge is `private_pages`: frames only that process
        // maps, which a kill returns. Shared aliases and retained snapshots
        // are reported beside it and never scored, so a charge cannot exceed
        // physical memory and needs no cap.
        // A victim that cannot return a useful share of the reserve is never
        // worth a death, however empty the rest of the census is.
        let min_help = (census.min_free_pages / MIN_HELP_RESERVE_DIVISOR).max(MIN_VICTIM_PAGES);
        let mut best: Option<(u64, u64)> = None;
        // (job, eligible private pages, largest eligible pid, holds a
        // protected member) per distinct job. A job with one protected member
        // anywhere in the census is never killed whole, whatever its size:
        // the kernel-sourced protection answer is per process and a job kill
        // cannot spare one.
        let mut jobs = [(0_u64, 0_u64, 0_u64, false); ROOTD_MEMORY_CENSUS_MAX_ENTRIES];
        let mut job_count = 0;
        for entry in &census.entries[..count] {
            let pid = entry.process_id;
            if pid == 0 {
                continue;
            }
            let charge = entry.private_pages;
            let protected = pid == rootd_pid || is_protected(pid);
            let eligible = !protected && charge >= min_help;
            if entry.job != 0 {
                let slot = match jobs[..job_count].iter().position(|slot| slot.0 == entry.job) {
                    Some(index) => index,
                    None => {
                        jobs[job_count] = (entry.job, 0, 0, false);
                        job_count += 1;
                        job_count - 1
                    }
                };
                if protected {
                    jobs[slot].3 = true;
                } else if eligible {
                    jobs[slot].1 = jobs[slot].1.saturating_add(charge);
                    jobs[slot].2 = jobs[slot].2.max(pid);
                }
            }
            if !eligible {
                continue;
            }
            // The census is largest-first, but the policy does not depend on
            // the kernel's ordering: compare explicitly, ties to the younger.
            if best.is_none_or(|(pages, best_pid)| (charge, pid) > (pages, best_pid))
            {
                best = Some((charge, pid));
            }
        }
        // Largest job first. The census is partial (largest processes only), so
        // a job's sum is a lower bound and never overstates what it frees.
        let mut probed = [false; ROOTD_MEMORY_CENSUS_MAX_ENTRIES];
        while best.is_some() {
            let Some(next) = (0..job_count)
                .filter(|&i| !probed[i] && !jobs[i].3 && jobs[i].1 != 0)
                .max_by_key(|&i| (jobs[i].1, jobs[i].2))
            else {
                break;
            };
            probed[next] = true;
            let (job, private_pages, process_id, _) = jobs[next];
            if job_killable(job) {
                self.arm(now_ms, census, job, process_id, private_pages);
                return Verdict::KillJob {
                    job,
                    process_id,
                    private_pages,
                };
            }
        }
        match best {
            Some((private_pages, process_id)) => {
                self.arm(now_ms, census, 0, process_id, private_pages);
                Verdict::Kill {
                    process_id,
                    private_pages,
                }
            }
            None if self.reported_no_victim => Verdict::Idle,
            None => {
                self.reported_no_victim = true;
                Verdict::NoVictim
            }
        }
    }

    fn settling(pending: &mut Pending) -> Verdict {
        if core::mem::replace(&mut pending.settling_reported, true) {
            Verdict::Cooldown
        } else {
            Verdict::Settling {
                process_id: pending.lead,
                victims: pending.victim_count as u64,
            }
        }
    }

    /// Records the kill about to be issued. `job` is 0 for one process; for a
    /// job every census member of it is a victim whose exit must be observed.
    fn arm(
        &mut self,
        now_ms: u64,
        census: &RustosRootdMemoryBrokerArgs,
        job: u64,
        lead: u64,
        private_pages: u64,
    ) {
        let mut pending = Pending {
            victims: [0; ROOTD_MEMORY_CENSUS_MAX_ENTRIES],
            victim_count: 0,
            job,
            lead,
            killed_at_ms: now_ms,
            best_free: census.free_pages,
            progress_ms: now_ms,
            last_retry_ms: now_ms,
            settling_reported: false,
        };
        let count = (census.entry_count as usize).min(census.entries.len());
        for entry in &census.entries[..count] {
            if entry.process_id == lead || (job != 0 && entry.job == job) {
                pending.victims[pending.victim_count] = entry.process_id;
                pending.victim_count += 1;
            }
        }
        self.pending = Some(pending);
        self.reported_no_victim = false;
        self.recovery_floor_pages = recovery_floor(census, private_pages);
    }

    /// The kill the last verdict asked for was refused by the kernel: nothing
    /// is being retired, so do not wait for it, but do not hammer either.
    pub(super) fn kill_failed(&mut self, now_ms: u64) {
        self.pending = None;
        self.recovery_floor_pages = 0;
        self.backoff_until_ms = now_ms.saturating_add(REJECTED_KILL_BACKOFF_MS);
    }
}

/// Free pages that prove a victim's memory is back: half its private charge
/// above today's free pages. The charge is what the kill returns, so the floor
/// is reachable.
fn recovery_floor(census: &RustosRootdMemoryBrokerArgs, private_pages: u64) -> u64 {
    census.free_pages.saturating_add(private_pages / 2).max(1)
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
        for (slot, &(process_id, private_pages)) in reply.entries.iter_mut().zip(entries) {
            *slot = RootdMemoryCensusEntry {
                process_id,
                parent_process_id: 1,
                private_pages,
                job: 1,
                ..RootdMemoryCensusEntry::default()
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
                private_pages: 5000
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
                private_pages: 400
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
                private_pages: 500
            }
        );
    }

    /// The 1400 MiB Chrome boot: the Chrome job died, free memory never
    /// rose, and 200 ms later an innocent 349-page process was killed too.
    #[test]
    fn an_innocent_small_process_is_never_killed_after_the_big_holder_dies() {
        let mut governor = MemoryGovernor::new();
        let first = census(2_627, 2_627, 1, &[(226, 42_418), (109, 349)]);
        assert_eq!(
            governor.assess(1_000, &first, ROOTD, none_protected),
            Verdict::Kill { process_id: 226, private_pages: 42_418 }
        );
        // Victim gone, free unchanged, denials continue, long after the settle
        // window: 349 pages cannot lift a 2627-page reserve, so it is not a victim.
        let later = census(2_627, 2_627, 2, &[(109, 349)]);
        assert_eq!(
            governor.assess(1_000 + SETTLE_MAX_MS + 1, &later, ROOTD, none_protected),
            Verdict::NoVictim
        );
    }

    #[test]
    fn no_follow_up_victim_while_the_first_is_listed_then_the_same_victim_is_retried() {
        let mut governor = MemoryGovernor::new();
        let first = census(90, 100, 1, &[(7, 5000), (8, 300)]);
        assert!(matches!(
            governor.assess(1_000, &first, ROOTD, none_protected),
            Verdict::Kill { process_id: 7, .. }
        ));
        let still = |denials| census(90, 100, denials, &[(7, 5000), (8, 300)]);
        assert_eq!(
            governor.assess(1_010, &still(2), ROOTD, none_protected),
            Verdict::Settling { process_id: 7, victims: 1 }
        );
        assert_eq!(
            governor.assess(1_020, &still(3), ROOTD, none_protected),
            Verdict::Cooldown
        );
        assert_eq!(
            governor.assess(1_000 + STUCK_RETRY_MS - 1, &still(4), ROOTD, none_protected),
            Verdict::Cooldown
        );
        assert_eq!(
            governor.assess(1_000 + STUCK_RETRY_MS, &still(5), ROOTD, none_protected),
            Verdict::Stuck { job: 0, process_id: 7 }
        );
        // Never the innocent pid 8, however long victim 7 lingers.
        assert_eq!(
            governor.assess(1_000 + STUCK_RETRY_MS + 1, &still(6), ROOTD, none_protected),
            Verdict::Cooldown
        );
    }

    #[test]
    fn a_follow_up_waits_for_frames_to_stop_arriving_then_kills_the_next_holder() {
        let mut governor = MemoryGovernor::new();
        let first = census(90, 100, 1, &[(7, 5000), (8, 3000)]);
        assert!(matches!(
            governor.assess(0, &first, ROOTD, none_protected),
            Verdict::Kill { process_id: 7, .. }
        ));
        // Victim retired from the census at once, but frames trickle back.
        let trickle = |free, denials| census(free, 100, denials, &[(8, 3000)]);
        assert_eq!(
            governor.assess(50, &trickle(90, 2), ROOTD, none_protected),
            Verdict::Settling { process_id: 7, victims: 1 }
        );
        assert_eq!(governor.assess(700, &trickle(95, 3), ROOTD, none_protected), Verdict::Cooldown);
        // Past the minimum window, but frames arrived at 1000 ms: wait.
        assert_eq!(governor.assess(1_000, &trickle(96, 4), ROOTD, none_protected), Verdict::Cooldown);
        assert_eq!(governor.assess(1_100, &trickle(96, 5), ROOTD, none_protected), Verdict::Cooldown);
        // Quiet for 300 ms and still below the reserve: the next holder dies.
        assert_eq!(
            governor.assess(1_000 + SETTLE_QUIET_MS, &trickle(96, 6), ROOTD, none_protected),
            Verdict::Kill { process_id: 8, private_pages: 3000 }
        );
    }

    #[test]
    fn a_settle_window_never_outlives_its_hard_bound() {
        let mut governor = MemoryGovernor::new();
        let first = census(9_000, 10_000, 1, &[(7, 5000)]);
        assert!(matches!(
            governor.assess(0, &first, ROOTD, none_protected),
            Verdict::Kill { .. }
        ));
        // Free keeps creeping up, so it is never "quiet", but the bound holds.
        let mut denials = 2;
        let mut free = 9_001;
        let mut now = 10;
        while now < SETTLE_MAX_MS {
            let reply = census(free, 10_000, denials, &[(8, 3000)]);
            assert!(!matches!(
                governor.assess(now, &reply, ROOTD, none_protected),
                Verdict::Kill { .. }
            ));
            free += 1;
            denials += 1;
            now += 100;
        }
        let reply = census(free, 10_000, denials, &[(8, 3000)]);
        assert!(matches!(
            governor.assess(SETTLE_MAX_MS, &reply, ROOTD, none_protected),
            Verdict::Kill { process_id: 8, .. }
        ));
    }

    #[test]
    fn a_rejected_kill_waits_for_nothing_but_a_short_backoff() {
        let mut governor = MemoryGovernor::new();
        let first = census(90, 100, 1, &[(7, 5000)]);
        assert!(matches!(
            governor.assess(0, &first, ROOTD, none_protected),
            Verdict::Kill { .. }
        ));
        governor.kill_failed(0);
        let again = census(90, 100, 2, &[(7, 5000)]);
        assert_eq!(
            governor.assess(REJECTED_KILL_BACKOFF_MS - 1, &again, ROOTD, none_protected),
            Verdict::Cooldown
        );
        let later = census(90, 100, 3, &[(7, 5000)]);
        assert!(matches!(
            governor.assess(REJECTED_KILL_BACKOFF_MS, &later, ROOTD, none_protected),
            Verdict::Kill { process_id: 7, .. }
        ));
    }

    #[test]
    fn a_job_kill_waits_for_every_census_member_of_the_job() {
        let mut governor = MemoryGovernor::new();
        let census0 = census_in_jobs(&[(10, 50, 7), (11, 40, 7), (12, 30, 8)]);
        assert!(matches!(
            governor.assess_with_jobs(0, &census0, ROOTD, |_| false, |job| job == 7),
            Verdict::KillJob { job: 7, .. }
        ));
        // Member 11 lingers: the verdict is the job again, never pid 12.
        let mut linger = census_in_jobs(&[(11, 40, 7), (12, 30, 8)]);
        linger.watermark_denials = 6;
        assert_eq!(
            governor.assess_with_jobs(10, &linger, ROOTD, |_| false, |job| job == 7),
            Verdict::Settling { process_id: 11, victims: 2 }
        );
        let mut linger = census_in_jobs(&[(11, 40, 7), (12, 30, 8)]);
        linger.watermark_denials = 7;
        assert_eq!(
            governor.assess_with_jobs(STUCK_RETRY_MS, &linger, ROOTD, |_| false, |job| job == 7),
            Verdict::Stuck { job: 7, process_id: 11 }
        );
    }

    #[test]
    fn refusals_consumed_while_settling_do_not_trigger_a_kill_after_it() {
        let mut governor = MemoryGovernor::new();
        let first = census(90, 100, 1, &[(7, 5000)]);
        assert!(matches!(
            governor.assess(0, &first, ROOTD, none_protected),
            Verdict::Kill { .. }
        ));
        let during = census(90, 100, 2, &[(8, 3000)]);
        assert!(matches!(
            governor.assess(10, &during, ROOTD, none_protected),
            Verdict::Settling { .. }
        ));
        let quiet = census(90, 100, 2, &[(8, 3000)]);
        assert_eq!(
            governor.assess(SETTLE_MAX_MS + 50, &quiet, ROOTD, none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn a_victim_below_a_quarter_of_the_reserve_cannot_help_and_is_not_chosen() {
        let mut governor = MemoryGovernor::new();
        let reply = census(3_900, 3_903, 1, &[(9, 975), (8, 900)]);
        assert_eq!(
            governor.assess(0, &reply, ROOTD, none_protected),
            Verdict::Kill { process_id: 9, private_pages: 975 }
        );
        let mut governor = MemoryGovernor::new();
        let reply = census(3_900, 3_903, 1, &[(8, 975 - 1)]);
        assert_eq!(governor.assess(0, &reply, ROOTD, none_protected), Verdict::NoVictim);
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
        for (slot, &(process_id, private_pages, job)) in reply.entries.iter_mut().zip(entries) {
            *slot = RootdMemoryCensusEntry {
                process_id,
                parent_process_id: 1,
                private_pages,
                job,
                ..RootdMemoryCensusEntry::default()
            };
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
            Verdict::KillJob { job: 7, process_id: 11, private_pages: 90 },
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
        assert_eq!(verdict, Verdict::KillJob { job: 7, process_id: 12, private_pages: 50 });
    }

    #[test]
    fn with_no_killable_job_the_single_largest_process_is_the_victim() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 7), (11, 40, 7), (12, 70, 8)]);
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |_| false);
        assert_eq!(verdict, Verdict::Kill { process_id: 12, private_pages: 70 });
    }

    #[test]
    fn a_job_holding_a_protected_member_is_never_killed_whole() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 7), (11, 40, 7), (12, 70, 8)]);
        let mut asked = std::vec::Vec::new();
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |pid| pid == 10, |job| {
            asked.push(job);
            true
        });
        // Job 7 would sum to 40 without the protected pid 10, but a job kill
        // cannot spare it: job 8 (a single process) is the answer.
        assert_eq!(asked, [8]);
        assert_eq!(verdict, Verdict::KillJob { job: 8, process_id: 12, private_pages: 70 });
    }

    #[test]
    fn jobs_are_not_probed_without_pressure_or_while_settling() {
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
        assert_eq!(
            (verdict, asked),
            (Verdict::Settling { process_id: 10, victims: 1 }, false)
        );
    }

    #[test]
    fn a_process_with_no_known_job_is_only_ever_a_single_process_victim() {
        let mut governor = MemoryGovernor::new();
        let census = census_in_jobs(&[(10, 50, 0)]);
        let verdict = governor.assess_with_jobs(1_000, &census, ROOTD, |_| false, |_| true);
        assert_eq!(verdict, Verdict::Kill { process_id: 10, private_pages: 50 });
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

    #[test]
    fn shared_and_retained_pages_never_enter_the_victim_score() {
        // The 2 GiB Chrome boot: a child with 848866 shared/retained pages and
        // few private ones sits above a real private hog in the census, yet
        // killing it frees nothing. The score is the private figure alone.
        let mut governor = MemoryGovernor::new();
        let mut reply = census_in_jobs(&[(10, 300, 7), (12, 90_000, 8), (11, 40_000, 9)]);
        reply.entries[0].shared_pages = 400_000;
        reply.entries[0].retained_pages = 448_566;
        reply.usable_pages = 522_576;
        reply.free_pages = 3_903;
        reply.min_free_pages = 3_903;
        assert_eq!(
            governor.assess_with_jobs(1_000, &reply, ROOTD, |_| false, |_| false),
            Verdict::Kill { process_id: 12, private_pages: 90_000 }
        );
    }

    #[test]
    fn a_process_that_only_shares_memory_is_not_a_victim() {
        let mut governor = MemoryGovernor::new();
        let mut reply = census_in_jobs(&[(10, 4, 7)]);
        reply.entries[0].shared_pages = 800_000;
        reply.entries[0].retained_pages = 48_866;
        assert_eq!(
            governor.assess_with_jobs(1_000, &reply, ROOTD, |_| false, |_| false),
            Verdict::NoVictim
        );
    }

    #[test]
    fn ties_in_private_pages_go_to_the_younger_pid() {
        let mut governor = MemoryGovernor::new();
        let reply = census_in_jobs(&[(10, 5_000, 7), (12, 5_000, 8), (11, 5_000, 9)]);
        assert_eq!(
            governor.assess_with_jobs(1_000, &reply, ROOTD, |_| false, |_| false),
            Verdict::Kill { process_id: 12, private_pages: 5_000 }
        );
    }

    #[test]
    fn recovery_is_armed_from_the_private_figure_a_kill_actually_returns() {
        let mut governor = MemoryGovernor::new();
        let mut reply = census_in_jobs(&[(10, 100_000, 7)]);
        reply.entries[0].retained_pages = 300_000;
        reply.free_pages = 3_903;
        reply.min_free_pages = 3_903;
        assert!(matches!(
            governor.assess_with_jobs(1_000, &reply, ROOTD, |_| false, |_| false),
            Verdict::Kill { .. }
        ));
        let mut after = reply;
        after.free_pages = 3_903 + 100_000 / 2;
        after.watermark_denials = reply.watermark_denials;
        assert_eq!(
            governor.assess_with_jobs(2_000, &after, ROOTD, |_| false, |_| false),
            Verdict::Recovered { free_pages: after.free_pages }
        );
    }
}
