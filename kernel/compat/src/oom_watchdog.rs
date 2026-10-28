//! Ring0 fallback for the root supervisor's memory-pressure policy.
//!
//! - **Owner:** `kernel-compat` owns this watchdog. The victim *policy* stays
//!   in `rootd` (`memory_governor`); this only ends the one process that kept
//!   asking for memory that did not exist, after `rootd` has had time to act.
//! - **Boundary:** The exception path only records a process id. The kill is
//!   issued from normal task context, never from the IRQ-off fault gate.
//! - **Lifecycle:** observe -> episode ticks -> defer to rootd -> kill or
//!   refuse -> cooldown.
//! - **Concurrency:** The state machine is single-owner (the fault-frame
//!   producer task); the only shared words are relaxed atomics.
//! - **Failure:** A protected (service) process is reported once and never
//!   killed; a stale or absent record does nothing.
//! - **Forbidden:** Searching for a "largest" or "best" victim, acting while
//!   `rootd` is terminating processes, or acting from exception context.
//! - **Evidence:** `oom_watchdog` unit tests and the
//!   `rootd.memory.census` fault-injected KVM proof.

use core::sync::atomic::{AtomicU64, Ordering};

/// One look every 100 ms; the caller rate-limits.
pub(crate) const LOOK_PERIOD_MS: u64 = 100;
/// Looks of sustained refusal, with no `rootd` termination, before acting:
/// 30 looks is 3 s, 300 times `rootd`'s 10 ms supervisor pass.
pub(crate) const EPISODE_LOOKS: u32 = 30;
/// After a kill, no further kill for this long.
pub(crate) const COOLDOWN_MS: u64 = 5_000;
/// A refusal older than this no longer names a process worth ending.
pub(crate) const REFUSAL_MAX_AGE_MS: u64 = 10_000;
/// A kill is reported as recovered once free memory has risen by this many
/// pages (256 MiB) over what it was when the kill was issued.
pub(crate) const RECOVERY_MIN_PAGES: u64 = 65_536;

/// The last refused process and when, in one word so a reader can never pair
/// one refusal's process with another's time: `pid << 32 | time_ms as u32`.
/// Two separate words let the watchdog read a stale process with a fresh time
/// and end a process that was not the one being refused.
static LAST_REFUSAL: AtomicU64 = AtomicU64::new(0);
static ROOTD_TERMINATIONS: AtomicU64 = AtomicU64::new(0);

/// Records, from the IRQ-off fault gate, which process was refused a frame.
/// One relaxed store: no lock, allocation, or process lookup. A process id that
/// does not fit 32 bits is not recorded, which only makes the watchdog inert for
/// it; the time wraps every 49 days and ages are computed modulo that.
pub(crate) fn note_refused_fault(process_id: u64, now_ms: u64) {
    if let Ok(pid) = u32::try_from(process_id) {
        LAST_REFUSAL.store(
            (u64::from(pid) << 32) | u64::from(now_ms as u32),
            Ordering::Relaxed,
        );
    }
}

/// Counts a termination issued by the root supervisor, so the watchdog can see
/// that the policy owner is alive and acting.
pub(crate) fn note_rootd_termination() {
    ROOTD_TERMINATIONS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn rootd_terminations() -> u64 {
    ROOTD_TERMINATIONS.load(Ordering::Relaxed)
}

/// The last recorded refusal as `(process_id, age_ms)`, if any.
pub(crate) fn last_refusal(now_ms: u64) -> Option<(u64, u64)> {
    let word = LAST_REFUSAL.load(Ordering::Relaxed);
    let pid = word >> 32;
    (pid != 0).then(|| (pid, u64::from((now_ms as u32).wrapping_sub(word as u32))))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Observation {
    pub(crate) now_ms: u64,
    pub(crate) free_pages: u64,
    pub(crate) min_free_pages: u64,
    pub(crate) denials: u64,
    pub(crate) rootd_terminations: u64,
    /// `(process_id, age_ms)` of the last refused fault.
    pub(crate) refusal: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    Idle,
    /// Terminate this process.
    Kill {
        process_id: u64,
    },
    /// The refused process owns a service endpoint; reported once an episode.
    Protected {
        process_id: u64,
    },
    /// The last kill's memory is back; reported once per kill.
    Recovered {
        free_pages: u64,
    },
}

#[derive(Debug)]
pub struct OomWatchdog {
    last_denials: u64,
    last_rootd_terminations: u64,
    last_look_ms: u64,
    looks: u32,
    cooldown_until_ms: u64,
    protected_reported: bool,
    /// Free pages that mean the last kill's memory is back; zero when none is
    /// awaiting recovery.
    recovery_floor_pages: u64,
}

impl OomWatchdog {
    pub const fn new() -> Self {
        Self {
            last_denials: 0,
            last_rootd_terminations: 0,
            last_look_ms: 0,
            looks: 0,
            cooldown_until_ms: 0,
            protected_reported: false,
            recovery_floor_pages: 0,
        }
    }

    /// Whether `now_ms` is far enough from the previous look to take another.
    pub(crate) fn due(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.last_look_ms) < LOOK_PERIOD_MS {
            return false;
        }
        self.last_look_ms = now_ms;
        true
    }

    pub(crate) fn assess(
        &mut self,
        observation: &Observation,
        mut owns_service_endpoint: impl FnMut(u64) -> bool,
    ) -> Verdict {
        let denied = observation.denials > self.last_denials;
        self.last_denials = observation.denials;
        let rootd_acted = observation.rootd_terminations != self.last_rootd_terminations;
        self.last_rootd_terminations = observation.rootd_terminations;

        if self.recovery_floor_pages != 0 && observation.free_pages >= self.recovery_floor_pages {
            self.recovery_floor_pages = 0;
            return Verdict::Recovered {
                free_pages: observation.free_pages,
            };
        }
        if observation.free_pages > observation.min_free_pages {
            self.looks = 0;
            self.protected_reported = false;
            return Verdict::Idle;
        }
        if rootd_acted {
            // The policy owner is alive and terminating: this is its episode.
            self.looks = 0;
            self.protected_reported = false;
            return Verdict::Idle;
        }
        // Below the reserve with no new refusal means the asker has already
        // stopped (a retired thread asks no more); keep the count, not grow it.
        if denied {
            self.looks = self.looks.saturating_add(1);
        }
        if self.looks < EPISODE_LOOKS || observation.now_ms < self.cooldown_until_ms {
            return Verdict::Idle;
        }
        let Some((process_id, age_ms)) = observation.refusal else {
            return Verdict::Idle;
        };
        if age_ms > REFUSAL_MAX_AGE_MS {
            return Verdict::Idle;
        }
        if owns_service_endpoint(process_id) {
            if self.protected_reported {
                return Verdict::Idle;
            }
            self.protected_reported = true;
            return Verdict::Protected { process_id };
        }
        self.looks = 0;
        self.cooldown_until_ms = observation.now_ms.saturating_add(COOLDOWN_MS);
        self.recovery_floor_pages = observation
            .free_pages
            .saturating_add(RECOVERY_MIN_PAGES)
            .max(1);
        Verdict::Kill { process_id }
    }
}

impl Default for OomWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

/// One watchdog pass, from the normal-context fault-frame producer task.
/// Returns the amount of work done, for the caller's idle accounting.
pub(crate) fn service(watchdog: &mut OomWatchdog) -> usize {
    let now_ms = crate::arch::clock::monotonic_nanos() / 1_000_000;
    if !watchdog.due(now_ms) {
        return 0;
    }
    let page_bytes = rustos_user_abi::pager::PAGER_PAGE_BYTES;
    let free_pages = crate::memory::phys::free_bytes() / page_bytes;
    let observation = Observation {
        now_ms,
        free_pages,
        min_free_pages: crate::memory::phys::min_free_bytes() / page_bytes,
        denials: crate::memory::phys::user_watermark_denials(),
        rootd_terminations: rootd_terminations(),
        refusal: last_refusal(now_ms),
    };
    match watchdog.assess(
        &observation,
        crate::user::syscall::linux::ipc_ops::process_owns_service_endpoint,
    ) {
        Verdict::Idle => 0,
        Verdict::Protected { process_id } => {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Compat,
                "oom-watchdog-protected",
                process_id,
                free_pages,
            );
            1
        }
        Verdict::Recovered { free_pages } => {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Compat,
                "oom-watchdog-recovered",
                free_pages,
                0,
            );
            1
        }
        Verdict::Kill { process_id } => {
            let ended =
                crate::user::syscall::linux::terminate_process_once_and_finalize(process_id);
            crate::debug::record_milestone(
                crate::debug::LogCategory::Compat,
                if ended {
                    "oom-watchdog-kill"
                } else {
                    "oom-watchdog-gone"
                },
                process_id,
                free_pages,
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 100;

    fn look(now_ms: u64, free: u64, denials: u64, rootd: u64, pid: u64) -> Observation {
        Observation {
            now_ms,
            free_pages: free,
            min_free_pages: MIN,
            denials,
            rootd_terminations: rootd,
            refusal: (pid != 0).then_some((pid, 0)),
        }
    }

    fn none_protected(_: u64) -> bool {
        false
    }

    /// Drives `looks` consecutive 100 ms looks, each with one new denial, and
    /// requires every one of them to be `Idle`: acting early is a failure even
    /// when a later look happens to be quiet.
    fn sustain(dog: &mut OomWatchdog, looks: u32, rootd: u64, pid: u64) -> Verdict {
        for index in 0..looks {
            let denials = u64::from(index) + 1;
            let verdict = dog.assess(
                &look(u64::from(index) * 100, MIN, denials, rootd, pid),
                none_protected,
            );
            assert_eq!(verdict, Verdict::Idle, "look {index} acted early");
        }
        Verdict::Idle
    }

    #[test]
    fn a_sustained_refusal_with_a_silent_rootd_ends_the_refused_process() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 42), Verdict::Idle);
        let at_limit = look(5_000, MIN, 1_000, 0, 42);
        assert_eq!(
            dog.assess(&at_limit, none_protected),
            Verdict::Kill { process_id: 42 }
        );
    }

    #[test]
    fn a_rootd_termination_during_the_episode_restarts_it() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 42), Verdict::Idle);
        // rootd acts: the count moves, so the watchdog stands down...
        assert_eq!(
            dog.assess(&look(5_000, MIN, 500, 1, 42), none_protected),
            Verdict::Idle
        );
        // ...and needs a whole new episode before it may act.
        let mut denials = 500;
        for tick in 0..EPISODE_LOOKS - 1 {
            denials += 1;
            assert_eq!(
                dog.assess(
                    &look(6_000 + u64::from(tick) * 100, MIN, denials, 1, 42),
                    none_protected
                ),
                Verdict::Idle
            );
        }
    }

    #[test]
    fn free_memory_above_the_reserve_ends_the_episode() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 42), Verdict::Idle);
        assert_eq!(
            dog.assess(&look(5_000, MIN + 1, 900, 0, 42), none_protected),
            Verdict::Idle
        );
        assert_eq!(
            dog.assess(&look(5_100, MIN, 901, 0, 42), none_protected),
            Verdict::Idle
        );
    }

    #[test]
    fn looks_without_new_refusals_do_not_count_toward_an_episode() {
        let mut dog = OomWatchdog::new();
        for index in 0..EPISODE_LOOKS * 3 {
            assert_eq!(
                dog.assess(&look(u64::from(index) * 100, MIN, 7, 0, 42), none_protected),
                Verdict::Idle,
                "denials stayed at 7"
            );
        }
    }

    #[test]
    fn a_service_owning_process_is_reported_once_and_never_killed() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 9), Verdict::Idle);
        let obs = look(5_000, MIN, 1_000, 0, 9);
        assert_eq!(
            dog.assess(&obs, |pid| pid == 9),
            Verdict::Protected { process_id: 9 }
        );
        let again = look(5_100, MIN, 1_001, 0, 9);
        assert_eq!(dog.assess(&again, |pid| pid == 9), Verdict::Idle);
    }

    #[test]
    fn a_stale_or_missing_refusal_names_no_one() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 42), Verdict::Idle);
        let mut stale = look(5_000, MIN, 1_000, 0, 42);
        stale.refusal = Some((42, REFUSAL_MAX_AGE_MS + 1));
        assert_eq!(dog.assess(&stale, none_protected), Verdict::Idle);
        let mut absent = look(5_100, MIN, 1_001, 0, 42);
        absent.refusal = None;
        assert_eq!(dog.assess(&absent, none_protected), Verdict::Idle);
    }

    #[test]
    fn a_kill_starts_a_cooldown_and_a_fresh_episode() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 42), Verdict::Idle);
        assert_eq!(
            dog.assess(&look(5_000, MIN, 1_000, 0, 42), none_protected),
            Verdict::Kill { process_id: 42 }
        );
        // Still starved and still being refused, but inside the cooldown.
        let mut denials = 1_000;
        for tick in 0..EPISODE_LOOKS + 5 {
            denials += 1;
            let now = 5_100 + u64::from(tick) * 10;
            assert_eq!(
                dog.assess(&look(now, MIN, denials, 0, 43), none_protected),
                Verdict::Idle
            );
        }
        // After the cooldown a full episode has accumulated and it may act.
        denials += 1;
        assert_eq!(
            dog.assess(
                &look(5_000 + COOLDOWN_MS, MIN, denials, 0, 43),
                none_protected
            ),
            Verdict::Kill { process_id: 43 }
        );
    }

    #[test]
    fn a_kill_is_followed_by_exactly_one_recovery_report() {
        let mut dog = OomWatchdog::new();
        assert_eq!(sustain(&mut dog, EPISODE_LOOKS - 1, 0, 42), Verdict::Idle);
        assert_eq!(
            dog.assess(&look(5_000, MIN, 1_000, 0, 42), none_protected),
            Verdict::Kill { process_id: 42 }
        );
        let almost = look(5_100, MIN + RECOVERY_MIN_PAGES - 1, 1_000, 0, 42);
        assert_eq!(dog.assess(&almost, none_protected), Verdict::Idle);
        let back = look(5_200, MIN + RECOVERY_MIN_PAGES, 1_000, 0, 42);
        assert_eq!(
            dog.assess(&back, none_protected),
            Verdict::Recovered {
                free_pages: MIN + RECOVERY_MIN_PAGES
            }
        );
        assert_eq!(dog.assess(&back, none_protected), Verdict::Idle);
    }

    #[test]
    fn looks_are_rate_limited() {
        let mut dog = OomWatchdog::new();
        assert!(dog.due(LOOK_PERIOD_MS));
        assert!(!dog.due(LOOK_PERIOD_MS + LOOK_PERIOD_MS - 1));
        assert!(dog.due(LOOK_PERIOD_MS * 2));
    }

    #[test]
    fn the_exception_path_record_round_trips_with_its_age() {
        note_refused_fault(77, 1_000);
        assert_eq!(last_refusal(1_250), Some((77, 250)));
        // The 32-bit clock wraps; the age stays correct across the wrap.
        note_refused_fault(78, u64::from(u32::MAX) - 5);
        assert_eq!(last_refusal(u64::from(u32::MAX) + 10), Some((78, 15)));
        // A process id that does not fit is not recorded over the last one.
        note_refused_fault(u64::from(u32::MAX) + 1, 5);
        assert_eq!(
            last_refusal(u64::from(u32::MAX) + 10).map(|(pid, _)| pid),
            Some(78)
        );
        note_refused_fault(77, 1_000);
        let before = rootd_terminations();
        note_rootd_termination();
        assert_eq!(rootd_terminations(), before + 1);
    }
}
