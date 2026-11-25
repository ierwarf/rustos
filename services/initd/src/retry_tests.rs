use super::*;

#[test]
fn backoff_doubles_from_fifty_ms_and_is_capped_at_five_seconds() {
    let expected_ms = [50, 100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000];
    for (i, ms) in expected_ms.into_iter().enumerate() {
        assert_eq!(retry_backoff(i as u32 + 1), Duration::from_millis(ms));
    }
    assert_eq!(retry_backoff(u32::MAX), RETRY_BACKOFF_MAX);
    assert_eq!(retry_backoff(0), RETRY_BACKOFF);
}

#[test]
fn successful_replacements_do_not_erase_the_crash_loop_budget() {
    // Deterministic clock: every incarnation dies as soon as its backoff ends.
    let mut now = Instant::now();
    let mut retry = RetryState::new(now);
    for failure in 1..=INITD_LAUNCH_MAX_FAILURES {
        retry.note_spawned(now);
        assert_eq!(retry.failures, failure - 1);
        let exhausted = retry.note_failure(now);
        assert_eq!(exhausted, failure == INITD_LAUNCH_MAX_FAILURES);
        assert_eq!(retry.next_attempt, now + retry_backoff(failure));
        now = retry.next_attempt;
    }
    // Backoff made the crash loop take ~51 s of wall time, not 1.6 s.
    let total = now.saturating_duration_since(retry.recent[0]);
    assert!(total >= Duration::from_secs(45), "{total:?}");
    assert!(total < INITD_LAUNCH_FAILURE_WINDOW, "{total:?}");
}

#[test]
fn sustained_health_decays_the_failure_count_and_the_backoff() {
    let start = Instant::now();
    let mut retry = RetryState::new(start);
    for _ in 0..(INITD_LAUNCH_MAX_FAILURES - 1) {
        assert!(!retry.note_failure(start));
    }
    assert_eq!(retry.failures, INITD_LAUNCH_MAX_FAILURES - 1);
    assert_eq!(retry.next_attempt, start + RETRY_BACKOFF_MAX);

    // One incarnation that then stays up for the whole window is forgiven.
    let died = start + INITD_LAUNCH_FAILURE_WINDOW;
    assert!(!retry.note_failure(died));
    assert_eq!(retry.failures, 1);
    assert_eq!(retry.next_attempt, died + RETRY_BACKOFF);

    // Just short of the window the old failures still count.
    let mut retry = RetryState::new(start);
    for _ in 0..(INITD_LAUNCH_MAX_FAILURES - 1) {
        retry.note_failure(start);
    }
    assert!(retry.note_failure(start + INITD_LAUNCH_FAILURE_WINDOW - Duration::from_millis(1)));
}

#[test]
fn slow_crash_cadence_never_exhausts() {
    // A service that dies once per backoff cap (> 60 s / 16) keeps at most
    // 12 failures in the window, so it is retried forever.
    let mut now = Instant::now();
    let mut retry = RetryState::new(now);
    for _ in 0..1_000 {
        assert!(!retry.note_failure(now));
        now += RETRY_BACKOFF_MAX;
    }
    assert!(retry.failures < INITD_LAUNCH_MAX_FAILURES);
    assert!(retry.recent.len() < INITD_LAUNCH_MAX_FAILURES as usize);
}

#[test]
fn crash_loop_exhaustion_terminates_initd_with_status_111() {
    const CHILD: &str = "RUSTOS_INITD_CRASH_LOOP_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let mut retries = BTreeMap::new();
        for _ in 0..INITD_LAUNCH_MAX_FAILURES {
            if let Some(retry) = retries.get_mut("/test/service.elf") {
                RetryState::note_spawned(retry, Instant::now());
            }
            record_launch_failure(&mut retries, "/test/service.elf", "exit", 1);
        }
        panic!("initd did not fail closed at its crash-loop limit");
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retry_tests::crash_loop_exhaustion_terminates_initd_with_status_111",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(111),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn slow_lease_report_defers_relaunch_without_spending_the_crash_budget() {
    let now = Instant::now();
    let mut retry_state = BTreeMap::new();
    for _ in 0..(INITD_LAUNCH_MAX_FAILURES * 3) {
        defer_launch_without_charge(&mut retry_state, "/svc/netd", now);
    }
    let state = &retry_state["/svc/netd"];
    assert_eq!(state.failures, 0, "a slow rootd must never exhaust the budget");
    assert_eq!(state.next_attempt, now + RETRY_BACKOFF);

    // An existing budget is preserved, not reset or advanced.
    retry_state.get_mut("/svc/netd").unwrap().failures = 2;
    defer_launch_without_charge(&mut retry_state, "/svc/netd", now);
    assert_eq!(retry_state["/svc/netd"].failures, 2);
}
