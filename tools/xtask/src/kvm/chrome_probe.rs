// Evidence for `kvm-smoke --chrome-compat-probe`: the `apps/chromeprobe` session
// program exercises the Linux surface a Chromium-class browser leans on that
// the other probes do not: tmpfs file semantics under /tmp, /dev/shm and
// /run/user/1000, /data persistence across boots, POSIX/OFD/flock locks,
// AF_UNIX seqpacket/SCM_RIGHTS/path/abstract sockets, rtnetlink dumps, mremap
// and friends, `/proc/self`, and fd identity (fstat, O_PATH, sendfile).
//
// The probe is armed only by a private contract file on the per-run boot disk;
// without it the session program exits at once, silently. The flag implies a
// writable `/data` partition; pass `--data-disk <path>` to keep it across two
// boots with `--chrome-probe-persist first` and then `second`: each requires
// its corresponding marker, and second verifies the exact persisted bytes.
//
// Per-case failures print `chromeprobe: <case> FAIL <detail>` and do not end the
// boot; the aggregate `chromeprobe: FAIL ...` verdict does.

const PRIVATE_CHROME_PROBE_CONTRACT_PATH: &str = "system/registry/system/chromeprobe-v1.env";
/// Must equal `CONTRACT_BODY` in `apps/chromeprobe/chrome_probe.c`.
const CHROME_PROBE_CONTRACT_BODY: &[u8] = b"contract=rustos-chrome-probe-v1\n";
fn chrome_probe_contract(phase: Option<&str>) -> Vec<u8> {
    let mut body = CHROME_PROBE_CONTRACT_BODY.to_vec();
    if let Some(phase) = phase { body.extend_from_slice(format!("persist={phase}\n").as_bytes()); }
    body
}
/// The aggregate failure verdict; any line containing this ends the boot at once.
const CHROME_PROBE_FAIL_MARKER: &str = "chromeprobe: FAIL";
const CHROME_PROBE_PASSED_MARKER: &str = "chromeprobe: PASS";
const CHROME_PROBE_PERSIST_MARKERS: [&str; 2] = [
    "chromeprobe: persist first-boot ok",
    "chromeprobe: persist second-boot ok",
];
/// One `chromeprobe: <case> ok` record per case, all required. Must match the
/// case names in `apps/chromeprobe/chrome_probe.c`.
const CHROME_PROBE_CASES: [&str; 43] = [
    "tmpfs.tmp",
    "tmpfs.devshm",
    "tmpfs.run",
    "data.dirs",
    "data.atomic-write",
    "lock.posix-tmpfs",
    "lock.posix-data",
    "lock.ofd",
    "lock.flock",
    "lock.autorelease",
    "lock.exec-cloexec",
    "sock.seqpacket",
    "sock.scm-rights",
    "sock.unix-path",
    "sock.unix-rel-umask",
    "sock.abstract",
    "sock.netlink-link",
    "sock.netlink-addr",
    "mem.mremap",
    "mem.mincore",
    "mem.mlock-msync",
    "mem.vm-semantics",
    "mem.truncate-sigbus-fat",
    "mem.membarrier",
    "mem.sigtimedwait",
    "mem.rlimit-sched",
    "mem.mmsg",
    "mem.epoll-pwait2",
    "mem.sysinfo",
    "proc.maps",
    "proc.status",
    "proc.stat",
    "proc.oom-score-adj",
    "proc.cmdline-environ",
    "proc.fd-dir",
    "proc.fd-readlink",
    "proc.fd-memfd-seals",
    "proc.mounts",
    "fdkind.fstat",
    "fdkind.memfd-ino",
    "fdkind.opath-tmpfile",
    "fdkind.sendfile-copyrange",
    "fileops.extended",
];

fn chrome_probe_markers() -> Vec<String> {
    let mut markers: Vec<String> = CHROME_PROBE_CASES
        .iter()
        .map(|case| format!("chromeprobe: {case} ok"))
        .collect();
    markers.push(CHROME_PROBE_PASSED_MARKER.to_owned());
    markers
}

/// The probe's failure records (per-case `FAIL` lines and the aggregate
/// verdict), trimmed of the debugcon framing, if any are present. Per-case
/// lines alone do not end a boot; the aggregate verdict does.
fn chrome_probe_failure(log: &str) -> Option<String> {
    let _ = log.lines().find(|line| line.contains(CHROME_PROBE_FAIL_MARKER))?;
    Some(chrome_probe_failure_lines(log).join("; "))
}

fn chrome_probe_failure_lines(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| {
            let at = line.find("chromeprobe: ")?;
            let record = line[at..].trim().trim_end_matches("\\n");
            (record.contains(" FAIL")).then(|| record.to_owned())
        })
        .collect()
}

/// Which `/data` persistence record the probe logged, if either.
fn chrome_probe_persist_boot(log: &str) -> Option<&'static str> {
    CHROME_PROBE_PERSIST_MARKERS
        .iter()
        .copied()
        .find(|marker| log.lines().any(|line| line.contains(marker)))
}

fn validate_chrome_probe_log(log: &str, phase: Option<&str>) -> Result<()> {
    if let Some(failure) = chrome_probe_failure(log) {
        bail!("chrome compat probe reported a failure: {failure}");
    }
    let per_case = chrome_probe_failure_lines(log);
    if !per_case.is_empty() {
        bail!("chrome compat probe case failure: {}", per_case.join("; "));
    }
    for marker in chrome_probe_markers() {
        if !log.lines().any(|line| line.contains(&marker)) {
            bail!("chrome compat probe never logged `{marker}`");
        }
    }
    if let Some(phase) = phase {
        let expected = if phase == "first" { CHROME_PROBE_PERSIST_MARKERS[0] } else { CHROME_PROBE_PERSIST_MARKERS[1] };
        if !log.lines().any(|line| line.contains(expected)) { bail!("chrome compat probe never logged expected `{expected}`"); }
    } else if chrome_probe_persist_boot(log).is_none() {
        bail!(
            "chrome compat probe never logged `{}` or `{}`",
            CHROME_PROBE_PERSIST_MARKERS[0],
            CHROME_PROBE_PERSIST_MARKERS[1]
        );
    }
    Ok(())
}

#[cfg(test)]
mod chrome_probe_tests {
    use super::*;

    fn passing_log(persist: &str) -> String {
        let mut log = String::from("boot noise\nchromeprobe: start\n");
        for case in CHROME_PROBE_CASES {
            log.push_str(&format!("[dbg] chromeprobe: {case} ok\n"));
        }
        log.push_str(&format!("chromeprobe: persist {persist} ok\n"));
        log.push_str("chromeprobe: PASS (400 checks, 0 failures)\n");
        log
    }

    #[test]
    fn complete_log_validates_for_either_boot() {
        for boot in ["first-boot", "second-boot"] {
            let log = passing_log(boot);
            assert!(validate_chrome_probe_log(&log, None).is_ok());
            assert!(chrome_probe_persist_boot(&log).unwrap().contains(boot));
        }
    }

    #[test]
    fn the_aggregate_failure_ends_the_wait_with_every_case_failure() {
        let log = format!(
            "{}chromeprobe: proc.maps FAIL no [stack] (errno=2)\nchromeprobe: FAIL 1 checks failed\n",
            passing_log("first-boot")
        );
        let failure = chrome_probe_failure(&log).unwrap();
        assert!(failure.contains("proc.maps") && failure.contains("1 checks failed"), "{failure}");
        assert!(validate_chrome_probe_log(&log, None).is_err());
        assert!(chrome_probe_failure("noise").is_none());
    }

    #[test]
    fn a_case_failure_alone_does_not_end_the_wait_but_fails_validation() {
        let log = "chromeprobe: mem.mremap FAIL x (errno=22)\n";
        assert!(chrome_probe_failure(log).is_none());
        assert!(validate_chrome_probe_log(&passing_log("first-boot").replace("ok\n[", "ok\nchromeprobe: lock.ofd FAIL x\n["), None).is_err());
    }

    #[test]
    fn a_missing_case_or_persist_record_is_refused() {
        let log = passing_log("first-boot").replace("chromeprobe: sock.abstract ok", "");
        assert!(validate_chrome_probe_log(&log, None).unwrap_err().to_string().contains("sock.abstract"));
        let log = passing_log("first-boot").replace("chromeprobe: persist first-boot ok", "");
        assert!(validate_chrome_probe_log(&log, None).unwrap_err().to_string().contains("persist"));
    }

    #[test]
    fn explicit_persistence_phase_requires_matching_record() {
        for phase in ["first", "second"] {
            let matching = passing_log(&format!("{phase}-boot"));
            let wrong = passing_log(if phase == "first" { "second-boot" } else { "first-boot" });
            assert!(validate_chrome_probe_log(&matching, Some(phase)).is_ok());
            assert!(validate_chrome_probe_log(&wrong, Some(phase)).is_err());
            assert!(validate_chrome_probe_log(&matching.replace(&format!("chromeprobe: persist {phase}-boot ok"), "chromeprobe: persist FAIL marker"), Some(phase)).is_err());
            assert!(String::from_utf8(chrome_probe_contract(Some(phase))).unwrap().ends_with(&format!("persist={phase}\n")));
        }
        assert!(parse_smoke_options(["--chrome-probe-persist", "second"].into_iter().map(String::from)).is_err());
        assert!(parse_smoke_options(["--chrome-compat-probe", "--data-disk", "/tmp/x", "--chrome-probe-persist", "second"].into_iter().map(String::from)).is_ok());
    }

    #[test]
    fn flag_implies_writable_storage_and_data_disk_is_accepted() {
        let parse = |extra: &[&str]| parse_smoke_options(extra.iter().map(|arg| arg.to_string()));
        let options = parse(&["--chrome-compat-probe"]).unwrap();
        assert!(options.chrome_compat_probe && options.writable_storage && options.dvm_block_shmem);
        assert!(options.gui_dvm_surfaces && options.data_disk.is_none());
        assert!(options.expected_markers.iter().any(|m| m == CHROME_PROBE_PASSED_MARKER));
        let options = parse(&["--chrome-compat-probe", "--data-disk", "/tmp/x.img"]).unwrap();
        assert_eq!(options.data_disk.as_deref(), Some(std::path::Path::new("/tmp/x.img")));
        assert!(parse(&["--data-disk", "/tmp/x.img"]).unwrap().writable_storage);
    }
}
