// Acceptance for the opt-in hog job-limit proof, not a physical OOM proof.
// Requires independently framed worker faults plus rootd accounting/drain and
// loader placement. A near-limit census alone cannot establish SIGSEGV.
const APP_LIMIT_OBSERVED: &str = "rootd: app limit observed job/pid/pages/limit/threads/free/min:";
const APP_LIMIT_DRAINED: &str = "rootd: app limit drained job/pid:";
const APP_LIMIT_REJECTED: &str = "rootd: app limit rejected job/errno:";
const APP_LIMIT_PAGES: u64 = rustos_user_abi::syscall::LOADER_APPLICATION_LIMIT_PROBE_PAGES;
const APP_LIMIT_WORKERS: usize = 4;

#[derive(Debug, Eq, PartialEq)]
struct ApplicationLimitEvidence { pid: u64, job: u64, pages: u64, free_pages: u64, faulted_workers: usize }

fn validate_application_limit_log(log: &str) -> Result<ApplicationLimitEvidence> {
    for forbidden in [
        OOM_HOG_SURVIVED_MARKER, APP_LIMIT_REJECTED, OOM_HOG_KILLED_MARKER,
        OOM_HOG_JOB_KILLED_MARKER, OOM_HOG_WATCHDOG_KILLED_MARKER,
        OOM_HOG_REJECTED_MARKER, "rootd: oom job kill rejected", OOM_HOG_NO_VICTIM_MARKER,
        "oomhog: pthread_create failed", "oomhog: mmap refused",
    ] {
        if log.contains(forbidden) { bail!("application quota proof contains forbidden outcome: {forbidden}"); }
    }
    let starts: Vec<Vec<u64>> = log.lines().filter_map(|line| numbers_after(line, OOM_HOG_STARTED_MARKER)).collect();
    let [start] = starts.as_slice() else { bail!("application quota proof needs exactly one hog start"); };
    let [pid, 0] = start.as_slice() else { bail!("malformed quota hog start: {start:?}"); };
    if *pid == 0 { bail!("quota hog pid is zero"); }
    let placements: Vec<Vec<u64>> = log.lines()
        .filter_map(|line| numbers_after(line, OOM_HOG_JOB_PLACED_MARKER))
        .filter(|fields| fields.get(1) == Some(pid)).collect();
    let [placement] = placements.as_slice() else { bail!("quota hog needs one exact loader placement"); };
    let [job, placed_pid, limit] = placement.as_slice() else { bail!("malformed quota placement"); };
    if job == &0 || *job == rustos_user_abi::syscall::JOB_ID_ROOT || placed_pid != pid || *limit != APP_LIMIT_PAGES {
        bail!("quota proof needs the opt-in hog-only child job limit");
    }
    let observations: Vec<Vec<u64>> = log.lines().filter_map(|line| numbers_after(line, APP_LIMIT_OBSERVED)).collect();
    let [observation] = observations.as_slice() else { bail!("quota proof needs exactly one accounting observation"); };
    let [seen_job, seen_pid, pages, seen_limit, threads, free, reserve] = observation.as_slice() else {
        bail!("malformed quota accounting observation");
    };
    if seen_job != job || seen_pid != pid || seen_limit != limit || *threads != 1
        || *pages < APP_LIMIT_PAGES - 512 || *pages > APP_LIMIT_PAGES + 256
        || *reserve == 0 || *free <= reserve.saturating_mul(2) {
        bail!("quota accounting lacks exact identity, last-thread state, near-limit charge or physical headroom");
    }
    let mut faulted = std::collections::BTreeSet::new();
    for (source_line, line) in log.lines().enumerate() {
        if !line.contains("name=linux-user-fault ") { continue; }
        let frame = parse_verified_milestone_frame(line, source_line + 1)
            .context("quota worker fault lacks a complete checksum-verified transport frame")?;
        let fault_pid = frame.event_arg0 >> 32;
        let tid = frame.event_arg0 & u64::from(u32::MAX);
        if frame.event != "linux-user-fault" || frame.category != "compat"
            || fault_pid != *pid || frame.process_id != Some(*pid) || tid == 0
            || frame.thread_id != Some(tid) || frame.event_arg1 >> 56 != 14 {
            bail!("quota proof has a foreign worker, inconsistent identity or non-page fault");
        }
        if !faulted.insert(tid) { bail!("quota proof repeats a faulted worker"); }
    }
    if faulted.len() != APP_LIMIT_WORKERS { bail!("quota proof needs four unique vector-14 worker faults, found {}", faulted.len()); }
    let drains: Vec<Vec<u64>> = log.lines().filter_map(|line| numbers_after(line, APP_LIMIT_DRAINED)).collect();
    let [drain] = drains.as_slice() else { bail!("quota proof needs exactly one drained job record"); };
    if drain.as_slice() != [*job, *pid] { bail!("quota drain belongs to a different job or process"); }
    Ok(ApplicationLimitEvidence { pid: *pid, job: *job, pages: *pages, free_pages: *free, faulted_workers: faulted.len() })
}

#[cfg(test)]
mod application_limit_tests {
    use super::*;

    fn fault(tid: u64, vector: u64) -> String {
        let packed = (41_u64 << 32) | tid;
        let rip = (vector << 56) | 0x1000;
        let semantic = format!("milestone-begin v=1 output_seq={tid} seq={tid} ts_us=100 tick=1 cat=compat name=linux-user-fault arg0=0x{packed:x} arg1=0x{rip:x} pid=41 tid={tid} dropped=0 discarded_bytes=0");
        let checksum = milestone_frame_checksum(semantic.as_bytes());
        format!("seq={tid} ts_us=100 tick=1 lvl=info cat=compat mod=nucleus_core::debug line=0 pid=41 tid={tid} msg=\"{semantic} checksum={checksum:016x} milestone-end\"")
    }
    fn log() -> String {
        let mut lines = vec![
            "oomhog: start pid 41 0".to_owned(),
            "loaderd: application job placed job/pid/limit: 80 41 32768".to_owned(),
            "rootd: app limit observed job/pid/pages/limit/threads/free/min: 80 41 32768 32768 1 100000 4096".to_owned(),
            "rootd: app limit drained job/pid: 80 41".to_owned(),
        ];
        for tid in 50..54 { lines.push(fault(tid, 14)); }
        lines.join("\n")
    }

    #[test]
    fn quota_acceptance_requires_four_framed_faults_exact_accounting_and_drain() {
        assert_eq!(validate_application_limit_log(&log()).unwrap(), ApplicationLimitEvidence {
            pid: 41, job: 80, pages: 32768, free_pages: 100_000, faulted_workers: 4,
        });
        for bad in [
            log().replace(&fault(50, 14), ""),
            format!("{}\n{}", log(), fault(50, 14)),
            log().replace(&fault(50, 14), &fault(50, 6)),
            log().replace(&fault(50, 14), &fault(50, 14).replace("pid=41", "pid=42")),
            log().replace(&fault(50, 14), &fault(50, 14).replace("arg1=0xe00000000001000", "arg1=0xe00000000001001")),
            log().replace("32768 32768 1 100000 4096", "30000 32768 1 100000 4096"),
            log().replace("32768 32768 1 100000 4096", "32768 32768 2 100000 4096"),
            log().replace("32768 32768 1 100000 4096", "32768 32768 1 8192 4096"),
            log().replace("80 41 32768", "80 41 524288"),
            log().replace("rootd: app limit drained job/pid: 80 41", "rootd: app limit drained job/pid: 81 41"),
            log().replace("rootd: app limit drained job/pid: 80 41", ""),
        ] { assert!(validate_application_limit_log(&bad).is_err(), "accepted: {bad}"); }
    }

    #[test]
    fn physical_oom_thread_creation_failure_and_probe_rejection_are_not_quota_proofs() {
        for bad in [
            OOM_HOG_KILLED_MARKER, OOM_HOG_JOB_KILLED_MARKER, OOM_HOG_WATCHDOG_KILLED_MARKER,
            OOM_HOG_SURVIVED_MARKER, APP_LIMIT_REJECTED, "oomhog: pthread_create failed",
            "oomhog: mmap refused",
        ] { assert!(validate_application_limit_log(&format!("{}\n{bad}", log())).is_err(), "accepted: {bad}"); }
    }

    #[test]
    fn quota_mode_is_explicit_separate_from_watchdog_and_matches_the_hog_source() {
        let normal = parse_smoke_options(Vec::<String>::new().into_iter()).unwrap();
        assert!(!normal.oom_hog_job_limit);
        let quota = parse_smoke_options(vec!["--oom-hog-job-limit".into()].into_iter()).unwrap();
        assert!(quota.oom_hog && quota.oom_hog_job_limit && quota.gui_dvm_surfaces);
        assert!(quota.expected_markers.iter().any(|marker| marker == APP_LIMIT_DRAINED));
        assert!(!quota.expected_markers.iter().any(|marker| marker == OOM_HOG_RECOVERED_MARKER));
        for args in [vec!["--oom-hog-job-limit", "--oom-hog-watchdog"], vec!["--oom-hog-watchdog", "--oom-hog-job-limit"]] {
            assert!(parse_smoke_options(args.into_iter().map(str::to_owned)).is_err());
        }
        assert!(include_str!("../../../../apps/oomhog/oomhog.c").contains("#define WORKERS 4\n"));
        assert_eq!(APP_LIMIT_PAGES, 32_768);
    }
}
