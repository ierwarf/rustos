// Evidence for `kvm-smoke --oom-hog`: under real memory pressure the root
// supervisor kills exactly the hog, and nothing else dies for it.
//
// The smoke's own wait loop already refuses a boot whose log shows a panic, a
// scheduler stall, or a failed relay (`runtime_stall_or_crash_observed`), so a
// service lost to the pressure fails the run there. What it cannot know is
// *who* was killed, which this decides from three log facts: the hog's own pid,
// the pid rootd reports killing, and that rootd killed once.

const OOM_HOG_STARTED_MARKER: &str = "oomhog: start pid";
const OOM_HOG_KILLED_MARKER: &str = "rootd: oom killed pid/pages/free:";
const OOM_HOG_RECOVERED_MARKER: &str = "rootd: oom recovered free/min:";
/// Milestone names as the debug channel renders them (`name=...`).
const OOM_HOG_WATCHDOG_KILLED_MARKER: &str = "name=oom-watchdog-kill ";
const OOM_HOG_WATCHDOG_RECOVERED_MARKER: &str = "name=oom-watchdog-recovered ";
const OOM_HOG_REJECTED_MARKER: &str = "rootd: oom kill rejected";
const OOM_HOG_NO_VICTIM_MARKER: &str = "rootd: oom no eligible victim";
const OOM_HOG_SURVIVED_MARKER: &str = "oomhog: survived";
/// 256 MiB. A kill below this would not be a kill under real pressure.
const OOM_HOG_MIN_KILLED_PAGES: u64 = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OomHogEvidence {
    pid: u64,
    resident_pages: u64,
    /// Free pages when the kill was issued, at the kernel reserve.
    free_pages: u64,
    /// Free pages once the victim's frames had been returned.
    recovered_free_pages: u64,
}

/// The decimal fields that follow `marker` on its log line, in order.
fn numbers_after(line: &str, marker: &str) -> Option<Vec<u64>> {
    let rest = &line[line.find(marker)? + marker.len()..];
    Some(
        rest.split(|c: char| !c.is_ascii_digit())
            .filter(|field| !field.is_empty())
            .map_while(|field| field.parse::<u64>().ok())
            .collect(),
    )
}

fn validate_oom_hog_log(log: &str) -> Result<OomHogEvidence> {
    if log.contains(OOM_HOG_SURVIVED_MARKER) {
        bail!("oomhog survived its deadline: the memory governor did not terminate it");
    }
    if log.contains(OOM_HOG_REJECTED_MARKER) {
        bail!("rootd selected a victim but the kernel rejected the termination");
    }
    if log.contains(OOM_HOG_NO_VICTIM_MARKER) {
        bail!("rootd reported pressure with no eligible victim");
    }
    let started: Vec<u64> = log
        .lines()
        .filter_map(|line| numbers_after(line, OOM_HOG_STARTED_MARKER))
        .filter_map(|fields| fields.first().copied())
        .collect();
    let [hog_pid] = started.as_slice() else {
        bail!(
            "expected exactly one oomhog start record, found {}",
            started.len()
        );
    };
    let kills: Vec<Vec<u64>> = log
        .lines()
        .filter_map(|line| numbers_after(line, OOM_HOG_KILLED_MARKER))
        .collect();
    let [kill] = kills.as_slice() else {
        bail!(
            "expected rootd to kill exactly one process under pressure, found {}",
            kills.len()
        );
    };
    let [killed_pid, resident_pages, free_pages] = kill.as_slice() else {
        bail!("malformed rootd oom kill record: {kill:?}");
    };
    if killed_pid != hog_pid {
        bail!("rootd killed pid {killed_pid}, not the hog (pid {hog_pid})");
    }
    let recoveries: Vec<Vec<u64>> = log
        .lines()
        .filter_map(|line| numbers_after(line, OOM_HOG_RECOVERED_MARKER))
        .collect();
    let [recovery] = recoveries.as_slice() else {
        bail!(
            "expected exactly one rootd recovery record after the kill, found {}",
            recoveries.len()
        );
    };
    let [recovered_free_pages, reserve_pages] = recovery.as_slice() else {
        bail!("malformed rootd oom recovery record: {recovery:?}");
    };
    if recovered_free_pages.saturating_sub(*free_pages) < *resident_pages / 2 {
        bail!(
            "free memory rose from {free_pages} to {recovered_free_pages} pages after the kill; \
             the victim's {resident_pages} resident pages were not returned"
        );
    }
    let _ = reserve_pages;
    if *resident_pages < OOM_HOG_MIN_KILLED_PAGES {
        bail!(
            "rootd killed the hog at only {resident_pages} resident pages; \
             that is not pressure from a memory hog"
        );
    }
    Ok(OomHogEvidence {
        pid: *killed_pid,
        resident_pages: *resident_pages,
        free_pages: *free_pages,
        recovered_free_pages: *recovered_free_pages,
    })
}

/// The first `arg0` of a milestone named by `marker`, parsed from `0x..` hex,
/// together with `arg1`.
fn milestone_args(line: &str, marker: &str) -> Option<(u64, u64)> {
    let rest = &line[line.find(marker)? + marker.len()..];
    let hex = |key: &str| -> Option<u64> {
        let value = rest
            .split_whitespace()
            .find_map(|field| field.strip_prefix(key))?;
        u64::from_str_radix(value.strip_prefix("0x")?, 16).ok()
    };
    Some((hex("arg0=")?, hex("arg1=")?))
}

/// Evidence for the ring0 fallback: with `rootd` blinded, exactly the hog is
/// ended by the watchdog and its memory comes back.
fn validate_oom_hog_watchdog_log(log: &str) -> Result<OomHogEvidence> {
    if log.contains(OOM_HOG_SURVIVED_MARKER) {
        bail!("oomhog survived its deadline: the watchdog did not terminate it");
    }
    if log.contains(OOM_HOG_KILLED_MARKER) {
        bail!("rootd killed a process although its census was supposed to be blind");
    }
    if log.contains("name=oom-watchdog-protected ") {
        bail!("the watchdog refused to end a process because it owns a service endpoint");
    }
    let started: Vec<u64> = log
        .lines()
        .filter_map(|line| numbers_after(line, OOM_HOG_STARTED_MARKER))
        .filter_map(|fields| fields.first().copied())
        .collect();
    let [hog_pid] = started.as_slice() else {
        bail!(
            "expected exactly one oomhog start record, found {}",
            started.len()
        );
    };
    let kills: Vec<(u64, u64)> = log
        .lines()
        .filter_map(|line| milestone_args(line, OOM_HOG_WATCHDOG_KILLED_MARKER))
        .collect();
    let [(killed_pid, free_pages)] = kills.as_slice() else {
        bail!("expected exactly one watchdog kill, found {}", kills.len());
    };
    if killed_pid != hog_pid {
        bail!("the watchdog killed pid {killed_pid}, not the hog (pid {hog_pid})");
    }
    let recoveries: Vec<(u64, u64)> = log
        .lines()
        .filter_map(|line| milestone_args(line, OOM_HOG_WATCHDOG_RECOVERED_MARKER))
        .collect();
    let [(recovered_free_pages, _)] = recoveries.as_slice() else {
        bail!(
            "expected exactly one watchdog recovery record, found {}",
            recoveries.len()
        );
    };
    if recovered_free_pages.saturating_sub(*free_pages) < OOM_HOG_MIN_KILLED_PAGES {
        bail!(
            "free memory rose from {free_pages} to {recovered_free_pages} pages after the \
             watchdog kill; the hog's memory was not returned"
        );
    }
    Ok(OomHogEvidence {
        pid: *killed_pid,
        resident_pages: 0,
        free_pages: *free_pages,
        recovered_free_pages: *recovered_free_pages,
    })
}

#[cfg(test)]
mod oom_hog_tests {
    use super::*;

    const START: &str = "user-debug payload=oomhog: start pid 41 0\\n";
    const KILL: &str = "user-debug payload=rootd: oom killed pid/pages/free: 41 380000 3900\\n";
    const RECOVERED: &str = "user-debug payload=rootd: oom recovered free/min: 380000 3900\\n";

    fn log(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn the_hog_killed_once_with_a_large_resident_set_is_accepted() {
        let evidence =
            validate_oom_hog_log(&log(&["noise", START, "noise", KILL, RECOVERED])).unwrap();
        assert_eq!(
            evidence,
            OomHogEvidence {
                pid: 41,
                resident_pages: 380_000,
                free_pages: 3_900,
                recovered_free_pages: 380_000
            }
        );
    }

    #[test]
    fn killing_a_different_process_is_rejected() {
        let other = "user-debug payload=rootd: oom killed pid/pages/free: 7 380000 3900\\n";
        let error = validate_oom_hog_log(&log(&[START, other, RECOVERED])).unwrap_err();
        assert!(error.to_string().contains("not the hog"), "{error}");
    }

    #[test]
    fn a_second_kill_is_collateral_and_is_rejected() {
        let second = "user-debug payload=rootd: oom killed pid/pages/free: 9 90000 3900\\n";
        assert!(validate_oom_hog_log(&log(&[START, KILL, second, RECOVERED])).is_err());
    }

    #[test]
    fn no_kill_is_rejected_even_if_the_hog_started() {
        assert!(validate_oom_hog_log(&log(&[START])).is_err());
    }

    #[test]
    fn a_survivor_a_rejection_or_an_empty_candidate_list_is_rejected() {
        for bad in [
            "user-debug payload=oomhog: survived touched_mib 1500 0\\n",
            "user-debug payload=rootd: oom kill rejected pid/pages/free: 41 380000 3900\\n",
            "user-debug payload=rootd: oom no eligible victim free/min/denials: 1 2 3\\n",
        ] {
            assert!(
                validate_oom_hog_log(&log(&[START, KILL, RECOVERED, bad])).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_kill_below_the_pressure_floor_is_rejected() {
        let small = "user-debug payload=rootd: oom killed pid/pages/free: 41 1000 3900\\n";
        let small_recovered = "user-debug payload=rootd: oom recovered free/min: 5000 3900\\n";
        let error = validate_oom_hog_log(&log(&[START, small, small_recovered])).unwrap_err();
        assert!(error.to_string().contains("not pressure"), "{error}");
    }

    #[test]
    fn a_kill_without_a_recovery_record_is_rejected() {
        let error = validate_oom_hog_log(&log(&[START, KILL])).unwrap_err();
        assert!(error.to_string().contains("recovery record"), "{error}");
    }

    #[test]
    fn a_recovery_that_does_not_return_the_victims_pages_is_rejected() {
        let barely = "user-debug payload=rootd: oom recovered free/min: 20000 3900\\n";
        let error = validate_oom_hog_log(&log(&[START, KILL, barely])).unwrap_err();
        assert!(error.to_string().contains("were not returned"), "{error}");
    }

    #[test]
    fn two_hog_starts_are_rejected() {
        assert!(validate_oom_hog_log(&log(&[START, START, KILL, RECOVERED])).is_err());
    }

    const W_KILL: &str =
        "seq=1 lvl=info cat=compat name=oom-watchdog-kill arg0=0x29 arg1=0xf69 pid=- tid=-";
    const W_BACK: &str =
        "seq=2 lvl=info cat=compat name=oom-watchdog-recovered arg0=0x30000 arg1=0x0 pid=- tid=-";

    #[test]
    fn the_watchdog_ending_the_hog_with_memory_back_is_accepted() {
        let evidence = validate_oom_hog_watchdog_log(&log(&[START, W_KILL, W_BACK])).unwrap();
        assert_eq!(evidence.pid, 41);
        assert_eq!(evidence.free_pages, 0xf69);
        assert_eq!(evidence.recovered_free_pages, 0x30000);
    }

    #[test]
    fn the_watchdog_proof_rejects_a_rootd_kill_a_wrong_pid_or_missing_recovery() {
        assert!(validate_oom_hog_watchdog_log(&log(&[START, W_KILL, W_BACK, KILL])).is_err());
        let other = W_KILL.replace("0x29", "0x7");
        assert!(validate_oom_hog_watchdog_log(&log(&[START, &other, W_BACK])).is_err());
        assert!(validate_oom_hog_watchdog_log(&log(&[START, W_KILL])).is_err());
        let barely = W_BACK.replace("0x30000", "0x2000");
        assert!(validate_oom_hog_watchdog_log(&log(&[START, W_KILL, &barely])).is_err());
    }

    #[test]
    fn a_protected_refusal_or_a_survivor_fails_the_watchdog_proof() {
        let protected = "name=oom-watchdog-protected arg0=0x9 arg1=0x1 pid=- tid=-";
        assert!(validate_oom_hog_watchdog_log(&log(&[START, W_KILL, W_BACK, protected])).is_err());
        let survived = "user-debug payload=oomhog: survived touched_mib 1500 0\\n";
        assert!(validate_oom_hog_watchdog_log(&log(&[START, W_KILL, W_BACK, survived])).is_err());
    }
}
