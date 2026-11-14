// Evidence for `kvm-smoke --local-fd-probe`: the `apps/localfdprobe` session
// program exercises kernel-local pipe/pipe2/eventfd/eventfd2 (EOF, EPIPE and
// SIGPIPE, atomic and partial writes, blocking wakeups, dup/fork/exec
// inheritance, poll/epoll readiness). Owner contract: `docs/ai/abi/local-fd.md`.
//
// The probe is armed only by a private contract file on the per-run boot disk;
// without it the session program exits at once, silently.

const PRIVATE_LOCAL_FD_PROBE_CONTRACT_PATH: &str = "system/registry/system/localfdprobe-v1.env";
/// Must equal `CONTRACT_BODY` in `apps/localfdprobe/localfd_probe.c`.
const LOCAL_FD_PROBE_CONTRACT_BODY: &[u8] = b"contract=rustos-localfd-probe-v1\n";
/// Any line containing this ends the boot at once.
const LOCAL_FD_PROBE_FAIL_MARKER: &str = "localfd: FAIL";
const LOCAL_FD_PROBE_PASSED_MARKER: &str = "localfd: PASS";
/// One `localfd: <case> ok` record per case, all required.
const LOCAL_FD_PROBE_CASES: [&str; 10] = [
    "pipe-basic",
    "pipe-flags",
    "pipe-eof-epipe",
    "pipe-dup-close",
    "pipe-blocking",
    "pipe-fork-exec",
    "pipe-poll-epoll",
    "pipe-many",
    "eventfd-basic",
    "eventfd-blocking",
];

fn local_fd_probe_markers() -> Vec<String> {
    let mut markers: Vec<String> = LOCAL_FD_PROBE_CASES
        .iter()
        .map(|case| format!("localfd: {case} ok"))
        .collect();
    markers.push(LOCAL_FD_PROBE_PASSED_MARKER.to_owned());
    markers
}

/// The probe's own `FAIL` record, trimmed of the debugcon framing, if present.
fn local_fd_probe_failure(log: &str) -> Option<String> {
    let line = log.lines().find(|line| line.contains(LOCAL_FD_PROBE_FAIL_MARKER))?;
    let at = line.find(LOCAL_FD_PROBE_FAIL_MARKER)?;
    Some(line[at..].trim().trim_end_matches("\\n").to_owned())
}

fn validate_local_fd_probe_log(log: &str) -> Result<()> {
    if let Some(failure) = local_fd_probe_failure(log) {
        bail!("local fd probe reported a failure: {failure}");
    }
    for marker in local_fd_probe_markers() {
        if !log.lines().any(|line| line.contains(&marker)) {
            bail!("local fd probe never logged `{marker}`");
        }
    }
    Ok(())
}

#[cfg(test)]
mod local_fd_probe_tests {
    use super::*;

    fn passing_log() -> String {
        let mut log = String::from("boot noise\nlocalfd: start mode=all\n");
        for case in LOCAL_FD_PROBE_CASES {
            log.push_str(&format!("[dbg] localfd: {case} ok\n"));
        }
        log.push_str("localfd: PASS (120 checks, 0 failures)\n");
        log
    }

    #[test]
    fn complete_log_validates() {
        assert!(validate_local_fd_probe_log(&passing_log()).is_ok());
    }

    #[test]
    fn a_failure_record_wins_over_the_pass_marker() {
        let log = format!("{}localfd: FAIL pipe-flags: x (errno=1)\n", passing_log());
        let err = validate_local_fd_probe_log(&log).unwrap_err().to_string();
        assert!(err.contains("pipe-flags"), "{err}");
        assert!(local_fd_probe_failure("noise").is_none());
    }

    #[test]
    fn a_missing_case_is_refused() {
        let log = passing_log().replace("localfd: eventfd-basic ok", "");
        assert!(validate_local_fd_probe_log(&log).unwrap_err().to_string().contains("eventfd-basic"));
    }
}
