use super::*;

#[test]
fn successful_replacements_do_not_erase_the_crash_loop_budget() {
    let now = Instant::now();
    let mut retry = RetryState {
        next_attempt: now,
        failures: 0,
        spawned_at: None,
    };
    for failure in 1..=INITD_LAUNCH_MAX_FAILURES {
        retry.note_spawned(now);
        assert_eq!(retry.failures, failure - 1);
        assert_eq!(
            retry.note_failure(now),
            failure == INITD_LAUNCH_MAX_FAILURES
        );
        assert_eq!(retry.next_attempt, now + RETRY_BACKOFF);
    }
    retry.note_spawned(now);
    assert!(retry.note_failure(now), "exhaustion remains fail-closed");
}

#[test]
fn sustained_health_starts_a_fresh_failure_budget() {
    let start = Instant::now();
    let mut retry = RetryState {
        next_attempt: start,
        failures: INITD_LAUNCH_MAX_FAILURES - 1,
        spawned_at: None,
    };
    retry.note_spawned(start);
    let died = start + INITD_LAUNCH_HEALTHY_RESET;
    assert!(!retry.note_failure(died), "a long-lived incarnation is forgiven");
    assert_eq!(retry.failures, 1);

    // Uptime is consumed by the failure it ended: a launch error that never
    // ran must not inherit it.
    retry.failures = INITD_LAUNCH_MAX_FAILURES - 1;
    assert!(retry.note_failure(died + INITD_LAUNCH_HEALTHY_RESET));

    // A short-lived incarnation keeps consuming the budget.
    retry.failures = INITD_LAUNCH_MAX_FAILURES - 1;
    retry.note_spawned(start);
    assert!(retry.note_failure(start + INITD_LAUNCH_HEALTHY_RESET / 2));
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
