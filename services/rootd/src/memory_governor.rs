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

use rustos_user_abi::syscall::RustosRootdMemoryBrokerArgs;

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
    /// Terminate this process.
    Kill {
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
    pub(super) fn assess(
        &mut self,
        now_ms: u64,
        census: &RustosRootdMemoryBrokerArgs,
        rootd_pid: u64,
        mut is_protected: impl FnMut(u64) -> bool,
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
        for entry in &census.entries[..count] {
            let pid = entry.process_id;
            if pid == 0
                || pid == rootd_pid
                || entry.resident_pages < MIN_VICTIM_PAGES
                || is_protected(pid)
            {
                continue;
            }
            // The census is largest-first, but the policy does not depend on
            // the kernel's ordering: compare explicitly, ties to the younger.
            if best.is_none_or(|(pages, best_pid)| (entry.resident_pages, pid) > (pages, best_pid))
            {
                best = Some((entry.resident_pages, pid));
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
}
