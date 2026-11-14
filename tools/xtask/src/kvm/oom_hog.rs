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
const OOM_HOG_JOB_KILLED_MARKER: &str = "rootd: oom killed job/pid/pages/free:";
const OOM_HOG_JOB_PLACED_MARKER: &str = "loaderd: application job placed job/pid/limit:";
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
    job: Option<u64>,
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
    if log.contains(OOM_HOG_REJECTED_MARKER) || log.contains("rootd: oom job kill rejected") {
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
    let mut kills = Vec::new();
    for line in log.lines() {
        if let Some(fields) = numbers_after(line, OOM_HOG_JOB_KILLED_MARKER) {
            let [job, pid, pages, free] = fields.as_slice() else {
                bail!("malformed rootd job oom kill record: {fields:?}");
            };
            if *job == 0 || *job == rustos_user_abi::syscall::JOB_ID_ROOT {
                bail!("rootd job oom kill did not name an application child job");
            }
            let matching_placement = log.lines().any(|line| {
                numbers_after(line, OOM_HOG_JOB_PLACED_MARKER).is_some_and(|placement| {
                    matches!(placement.as_slice(), [placed_job, placed_pid, limit]
                        if placed_job == job && placed_pid == pid && *limit > 0 && *limit != u64::MAX)
                })
            });
            if !matching_placement {
                bail!("rootd job oom kill has no matching limited application placement");
            }
            kills.push((Some(*job), vec![*pid, *pages, *free]));
        } else if let Some(fields) = numbers_after(line, OOM_HOG_KILLED_MARKER) {
            kills.push((None, fields));
        }
    }
    let [(job, kill)] = kills.as_slice() else {
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
        job: *job,
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
    if log.contains(OOM_HOG_KILLED_MARKER) || log.contains(OOM_HOG_JOB_KILLED_MARKER) {
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
        job: None,
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
                job: None,
                resident_pages: 380_000,
                free_pages: 3_900,
                recovered_free_pages: 380_000
            }
        );
    }

    #[test]
    fn oom_readiness_accepts_process_or_job_termination_before_exact_validation() {
        let options = parse_smoke_options(vec!["--oom-hog".to_owned()].into_iter()).unwrap();
        assert!(options.expected_markers.iter().any(|marker| marker == "rootd: oom killed "));
        assert!(!options.expected_markers.iter().any(|marker| marker == OOM_HOG_KILLED_MARKER));
        for marker in [OOM_HOG_KILLED_MARKER, OOM_HOG_JOB_KILLED_MARKER] {
            assert!(marker.contains("rootd: oom killed "));
        }
    }

    #[test]
    fn a_job_kill_requires_the_exact_limited_application_placement_and_no_collateral() {
        let placed = "loaderd: application job placed job/pid/limit: 80 41 524288";
        let killed = "rootd: oom killed job/pid/pages/free: 80 41 380000 3900";
        let valid = log(&[START, placed, killed, RECOVERED]);
        assert_eq!(validate_oom_hog_log(&valid).unwrap().job, Some(80));
        for bad in [
            valid.replace(placed, ""),
            valid.replace("80 41 524288", "81 41 524288"),
            valid.replace("80 41 524288", "80 42 524288"),
            valid.replace("80 41 524288", "80 41 0"),
            valid.replace("80 41 524288", "80 41 18446744073709551615"),
            valid.replace("80 41 380000 3900", "0 41 380000 3900"),
            valid.replace("80 41 380000 3900", "80 41 380000"),
            format!("{valid}\n{KILL}"),
            format!("{valid}\nrootd: oom job kill rejected job/pid/pages/free: 80 41 1 1"),
        ] { assert!(validate_oom_hog_log(&bad).is_err(), "accepted: {bad}"); }
        assert!(validate_oom_hog_watchdog_log(&log(&[START, W_KILL, W_BACK, killed])).is_err());
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

/// Frame count named by the single success line of `apps/libmapprobe`, after
/// refusing any failure record and requiring each intermediate proof line.
fn validate_library_map_probe_log(log: &str, reclaim: bool) -> Result<u64> {
    if log.contains(LIBRARY_MAP_PROBE_FAIL_MARKER) {
        bail!("libmapprobe reported a failure; see the library-map-probe: FAIL line");
    }
    if reclaim {
        if !log.contains(PAGERD_LIBRARY_RECLAIMED_MARKER) {
            bail!("pagerd never logged `{PAGERD_LIBRARY_RECLAIMED_MARKER}`");
        }
        let survived: Vec<&str> = log
            .lines()
            .filter(|line| line.contains(LIBRARY_RECLAIM_SURVIVED_MARKER))
            .collect();
        let [line] = survived.as_slice() else {
            bail!("expected exactly one reclaim-survived line, found {}", survived.len());
        };
        let fields = numbers_after(line, "checks=").unwrap_or_default();
        let pages = numbers_after(line, "pages=")
            .and_then(|f| f.first().copied())
            .filter(|pages| (6..=64).contains(pages))
            .ok_or_else(|| anyhow::anyhow!("reclaim-survived line has no plausible page count: {line}"))?;
        if fields.first().copied().unwrap_or(0) < 2 {
            bail!("reclaim-survived needs at least two verifications: {line}");
        }
        let (before, after) = (
            numbers_after(line, "sealed=").and_then(|f| f.first().copied()),
            numbers_after(line, "->").and_then(|f| f.first().copied()),
        );
        if !matches!((before, after), (Some(b), Some(a)) if a < b) {
            bail!("reclaim-survived does not show a smaller registry: {line}");
        }
        return Ok(pages);
    }
    for required in [
        "library-map-probe: mapped pages=",
        "library-map-probe: cow-isolated parent-write",
        "library-map-probe: eof-zero tail=",
        "library-map-probe: fork-semantics split-grandchild ok post-write-fork ok",
    ] {
        if !log.contains(required) {
            bail!("libmapprobe never logged `{required}`");
        }
    }
    let done: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("library-map-probe: shared frames=") && line.contains(LIBRARY_MAP_PROBE_DONE_MARKER))
        .collect();
    let [line] = done.as_slice() else {
        bail!("expected exactly one libmapprobe completion line, found {}", done.len());
    };
    let pages = numbers_after(line, "library-map-probe: shared frames=")
        .and_then(|fields| fields.first().copied())
        .filter(|pages| (6..=64).contains(pages))
        .ok_or_else(|| anyhow::anyhow!("libmapprobe completion line has no plausible frame count: {line}"))?;
    Ok(pages)
}

#[cfg(test)]
mod library_probe_tests {
    use super::*;

    const REQUIRED: [&str; 4] = [
        "library-map-probe: mapped pages=45 alias_maps=1 leaves+45 free-0",
        "library-map-probe: cow-isolated parent-write child-frame-unchanged",
        "library-map-probe: eof-zero tail=1296",
        "library-map-probe: fork-semantics split-grandchild ok post-write-fork ok",
    ];
    const DONE: &str = "library-map-probe: shared frames=45 cow-isolated eof-zero exit-clean leaves=863 free-drift=5";

    fn log(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("user-debug payload={l}\\n\n")).collect()
    }

    #[test]
    fn mapping_proof_needs_every_stage_and_one_completion() {
        let mut all: Vec<&str> = REQUIRED.to_vec();
        all.push(DONE);
        assert_eq!(validate_library_map_probe_log(&log(&all), false).unwrap(), 45);
        for skip in 0..REQUIRED.len() {
            let mut partial: Vec<&str> = REQUIRED.to_vec();
            partial.remove(skip);
            partial.push(DONE);
            assert!(validate_library_map_probe_log(&log(&partial), false).is_err());
        }
        all.push(DONE);
        assert!(validate_library_map_probe_log(&log(&all), false).is_err());
        let failed = log(&["library-map-probe: FAIL fork 0 0 errno=1", DONE]);
        assert!(validate_library_map_probe_log(&failed, false).is_err());
    }

    #[test]
    fn reclaim_proof_needs_pagerd_retire_and_a_surviving_mapping() {
        let pagerd = "pagerd: library cache reclaimed objects=2 pages=300 free=900 kept=4";
        let ok = "library-map-probe: reclaim-survived sealed=6->4 checks=5 pages=45";
        assert_eq!(validate_library_map_probe_log(&log(&[pagerd, ok]), true).unwrap(), 45);
        assert!(validate_library_map_probe_log(&log(&[ok]), true).is_err());
        assert!(validate_library_map_probe_log(&log(&[pagerd]), true).is_err());
        let same = "library-map-probe: reclaim-survived sealed=6->6 checks=5 pages=45";
        assert!(validate_library_map_probe_log(&log(&[pagerd, same]), true).is_err());
        let once = "library-map-probe: reclaim-survived sealed=6->4 checks=1 pages=45";
        assert!(validate_library_map_probe_log(&log(&[pagerd, once]), true).is_err());
        let broken = "library-map-probe: FAIL mapping bytes changed under reclaim 3 0 errno=0";
        assert!(validate_library_map_probe_log(&log(&[pagerd, ok, broken]), true).is_err());
    }

    #[test]
    fn probe_failure_is_extracted_for_fail_fast() {
        assert_eq!(library_map_probe_failure(&log(&[DONE])), None);
        let text = log(&["x", "library-map-probe: FAIL child frames 0 0 errno=3"]);
        assert_eq!(
            library_map_probe_failure(&text).as_deref(),
            Some("library-map-probe: FAIL child frames 0 0 errno=3")
        );
    }

    #[test]
    fn reclaim_probe_flag_requires_plain_oom_hog_and_swaps_markers() {
        let parse = |args: &[&str]| {
            parse_smoke_options(args.iter().map(|a| a.to_string()))
        };
        assert!(parse(&["--library-reclaim-probe"]).is_err());
        assert!(parse(&["--library-reclaim-probe", "--oom-hog-watchdog"]).is_err());
        let ok = parse(&["--library-reclaim-probe", "--oom-hog"]).unwrap();
        assert!(ok.expected_markers.iter().any(|m| m == LIBRARY_RECLAIM_SURVIVED_MARKER));
        assert!(ok.expected_markers.iter().any(|m| m == PAGERD_LIBRARY_RECLAIMED_MARKER));
        assert!(!ok.expected_markers.iter().any(|m| m == LIBRARY_MAP_PROBE_DONE_MARKER));
        assert_eq!(library_probe_contract(true), b"contract=rustos-libmapprobe-v1\nmode=reclaim\n");
    }
}
