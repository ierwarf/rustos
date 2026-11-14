// Evidence for `kvm-smoke --fault-probe`: the `apps/faultprobe` session program
// forks one child per case and proves process-group fault semantics in a real
// guest: a fault in any thread kills the whole group exactly once (SIGSEGV via
// wait4, every sibling gone), a group whose leader already retired is still
// killable, and a fault racing exit_group reports exactly one outcome.
//
// The probe is armed only by a private contract file on the per-run boot disk;
// without it the session program exits at once, silently.

const PRIVATE_FAULT_PROBE_CONTRACT_PATH: &str = "system/registry/system/faultprobe-v1.env";
/// Must equal `CONTRACT_BODY` in `apps/faultprobe/fault_probe.c`.
const FAULT_PROBE_CONTRACT_BODY: &[u8] = b"contract=rustos-fault-probe-v1\n";
/// Any line containing this ends the boot at once.
const FAULT_PROBE_FAIL_MARKER: &str = "fault-probe: FAIL";
const FAULT_PROBE_PASSED_MARKER: &str = "fault-probe: all passed";
/// One `fault-probe: <case> ok` record per case, all required, in this order.
const FAULT_PROBE_CASES: [&str; 4] = [
    "single-thread-segv",
    "mt-nonleader-fault",
    "leader-exit-sigkill",
    "fault-vs-exit-group",
];

fn fault_probe_markers() -> Vec<String> {
    let mut markers: Vec<String> = FAULT_PROBE_CASES
        .iter()
        .map(|case| format!("fault-probe: {case} ok"))
        .collect();
    markers.push(FAULT_PROBE_PASSED_MARKER.to_owned());
    markers
}

/// The probe's own `FAIL` record, trimmed of the debugcon framing, if present.
fn fault_probe_failure(log: &str) -> Option<String> {
    let line = log.lines().find(|line| line.contains(FAULT_PROBE_FAIL_MARKER))?;
    let at = line.find(FAULT_PROBE_FAIL_MARKER)?;
    Some(line[at..].trim().trim_end_matches("\\n").to_owned())
}

fn validate_fault_probe_log(log: &str) -> Result<()> {
    if let Some(failure) = fault_probe_failure(log) {
        bail!("fault probe reported a failure: {failure}");
    }
    let lines: Vec<&str> = log.lines().collect();
    let mut from = 0;
    for marker in fault_probe_markers() {
        let Some(offset) = lines[from..].iter().position(|line| line.contains(&marker)) else {
            bail!("fault probe never logged `{marker}` (or logged it out of order)");
        };
        from += offset + 1;
    }
    Ok(())
}

#[cfg(test)]
mod fault_probe_tests {
    use super::*;

    fn passing_log() -> String {
        let mut log = String::from("boot noise\nfault-probe: start\n");
        for case in FAULT_PROBE_CASES {
            log.push_str(&format!("[dbg] fault-probe: {case} ok\n"));
        }
        log.push_str("fault-probe: all passed\n");
        log
    }

    #[test]
    fn complete_log_validates() {
        assert!(validate_fault_probe_log(&passing_log()).is_ok());
    }

    #[test]
    fn a_failure_record_wins_over_the_pass_marker() {
        let log = format!("{}fault-probe: FAIL mt-nonleader-fault: x\n", passing_log());
        let err = validate_fault_probe_log(&log).unwrap_err().to_string();
        assert!(err.contains("mt-nonleader-fault"), "{err}");
        assert!(fault_probe_failure("noise").is_none());
    }

    #[test]
    fn a_missing_or_reordered_case_is_refused() {
        let log = passing_log().replace("fault-probe: leader-exit-sigkill ok", "");
        assert!(validate_fault_probe_log(&log).unwrap_err().to_string().contains("leader-exit-sigkill"));
        let swapped = "fault-probe: mt-nonleader-fault ok\nfault-probe: single-thread-segv ok\n";
        assert!(validate_fault_probe_log(swapped).is_err());
    }

    #[test]
    fn flag_arms_probe_and_implies_gui_surfaces() {
        let parsed = parse_smoke_options(["--fault-probe".to_string()].into_iter()).unwrap();
        assert!(parsed.fault_probe && parsed.gui_dvm_surfaces);
        assert!(parsed.expected_markers.contains(&FAULT_PROBE_PASSED_MARKER.to_owned()));
        let plain = parse_smoke_options(std::iter::empty::<String>()).unwrap();
        assert!(!plain.fault_probe);
    }
}
