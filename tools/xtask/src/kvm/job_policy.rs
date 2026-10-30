/* include!-fragment module contract (inner rustdoc is not legal here):
//! Acceptance for the opt-in JobPolicy boot controller.
//!
//! - Owner: host KVM acceptance independently checks sealed guest observations.
//! - Boundary: checksums provide integrity, not authentication; sealed sources
//!   and the exact bootstrap subject supply the provenance assumptions.
//! - Lifecycle: one completed controller report requires denial and restoration
//!   after the exact root bootstrap witness; retained cleanup is a failure.
//! - Concurrency: serial log validation is not a guest scheduling/liveness proof.
//! - Failure: foreign, replayed, malformed or incomplete records refuse.
//! - Forbidden: no acceptance from a controller string without kernel evidence.
//! - Evidence: native acceptance mutants and job-policy-publication model.
*/
const JOB_POLICY_OK: &str = "rootd: job policy ok root/pid:";
const JOB_POLICY_CACHE_EVENT: &str = "job-policy-current-cache-checked";

#[derive(Debug, Eq, PartialEq)]
struct JobPolicyEvidence { rootd_pid: u64, denied: usize, allowed: usize }

fn validate_job_policy_log(log: &str) -> Result<JobPolicyEvidence> {
    for forbidden in ["rootd: job policy failed", "rootd: job policy cleanup retained",
                      "rootd: job policy root restore failed"] {
        if log.contains(forbidden) { bail!("job policy proof contains failure: {forbidden}"); }
    }
    let reports: Vec<Vec<u64>> = log.lines().filter_map(|line| numbers_after(line, JOB_POLICY_OK)).collect();
    let [report] = reports.as_slice() else { bail!("job policy proof needs exactly one rootd report"); };
    let [root, rootd_pid] = report.as_slice() else { bail!("malformed job policy report"); };
    if *root != rustos_user_abi::syscall::JOB_ID_ROOT || *rootd_pid == 0 {
        bail!("job policy report lacks exact root identity and process");
    }
    let mut cores = Vec::new();
    let mut checks = Vec::new();
    for (index, line) in log.lines().enumerate() {
        let core = line.contains("name=product-root-core-ready ");
        let check = line.contains("name=job-policy-current-cache-checked ");
        if !core && !check { continue; }
        let frame = parse_verified_milestone_frame(line, index + 1)
            .context("job policy proof requires complete checksum-framed kernel records")?;
        if frame.category != "compat" || frame.process_id != Some(*rootd_pid)
            || frame.thread_id.is_none_or(|tid| tid == 0) {
            bail!("job policy proof has a foreign or unbound kernel subject");
        }
        if core {
            if frame.event != "product-root-core-ready" || frame.event_arg0 != 0 || frame.event_arg1 != 0 {
                bail!("malformed root bootstrap authority witness");
            }
            cores.push(frame);
        } else {
            if frame.event != JOB_POLICY_CACHE_EVENT || frame.event_arg0 != *root || frame.event_arg1 > 1 {
                bail!("malformed current policy cache witness");
            }
            checks.push(frame);
        }
    }
    let [core] = cores.as_slice() else { bail!("job policy proof needs one exact root bootstrap witness"); };
    let first_denial = checks.iter().filter(|frame| frame.event_arg1 == 0)
        .map(|frame| frame.milestone_seq).min().unwrap_or(u64::MAX);
    let mut sequences = std::collections::BTreeSet::new();
    let mut denied = 0;
    let mut allowed = 0;
    for frame in checks {
        if frame.milestone_seq <= core.milestone_seq || !sequences.insert(frame.milestone_seq) {
            bail!("job policy cache witness precedes bootstrap or repeats a record");
        }
        if frame.event_arg1 == 0 { denied += 1; }
        else if frame.milestone_seq > first_denial { allowed += 1; }
    }
    if denied == 0 || allowed == 0 {
        bail!("job policy proof lacks both denied and restored current-cache observations");
    }
    Ok(JobPolicyEvidence { rootd_pid: *rootd_pid, denied, allowed })
}

#[cfg(test)]
mod job_policy_tests {
    use super::*;

    fn frame(sequence: u64, event: &str, arg0: u64, arg1: u64, pid: u64) -> String {
        let semantic = format!("milestone-begin v=1 output_seq={sequence} seq={sequence} ts_us=100 tick=1 cat=compat name={event} arg0=0x{arg0:x} arg1=0x{arg1:x} pid={pid} tid=20 dropped=0 discarded_bytes=0");
        let checksum = milestone_frame_checksum(semantic.as_bytes());
        format!("seq={sequence} ts_us=100 tick=1 lvl=info cat=compat mod=nucleus_core::debug line=0 pid={pid} tid=20 msg=\"{semantic} checksum={checksum:016x} milestone-end\"")
    }

    fn log() -> String {
        [frame(1, "product-root-core-ready", 0, 0, 19),
         frame(2, JOB_POLICY_CACHE_EVENT, 1, 1, 19),
         frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 19),
         frame(4, JOB_POLICY_CACHE_EVENT, 1, 1, 19),
         "user-debug payload=rootd: job policy ok root/pid: 1 19\\n".into()].join("\n")
    }

    #[test]
    fn policy_probe_option_is_off_by_default_and_refuses_mixed_stress() {
        assert!(!parse_smoke_options(Vec::new().into_iter()).unwrap().job_policy_probe);
        let options = parse_smoke_options([
            "--job-policy-probe", "--no-build", "--no-auto-verify", "--expect", "ipcbench: end",
        ].into_iter().map(str::to_owned)).unwrap();
        assert!(options.job_policy_probe);
        assert!(!options.build_image && !options.auto_verify);
        assert!(options.expected_markers.iter().any(|marker| marker == JOB_POLICY_OK));
        assert!(options.expected_markers.iter().any(|marker| marker == "ipcbench: end"));
        for stress in ["--oom-hog", "--oom-hog-job-limit", "--oom-hog-watchdog", "--proc-stress"] {
            assert!(parse_smoke_options(["--job-policy-probe", stress]
                .into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn policy_acceptance_requires_rootd_and_kernel_denial_and_restoration() {
        assert_eq!(validate_job_policy_log(&log()).unwrap(),
                   JobPolicyEvidence { rootd_pid: 19, denied: 1, allowed: 1 });
        let no_denial = log().lines().filter(|line| !line.contains("seq=3 "))
            .collect::<Vec<_>>().join("\n");
        assert!(validate_job_policy_log(&no_denial).is_err());
        let no_restoration = log().lines().filter(|line| !line.contains("seq=4 "))
            .collect::<Vec<_>>().join("\n");
        assert!(validate_job_policy_log(&no_restoration).is_err(),
                "initial allowance cannot prove restoration after denial");
        let no_allow = [frame(1, "product-root-core-ready", 0, 0, 19),
                        frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 19),
                        "rootd: job policy ok root/pid: 1 19".into()].join("\n");
        assert!(validate_job_policy_log(&no_allow).is_err());
        assert!(validate_job_policy_log("rootd: job policy ok root/pid: 1 19").is_err());
    }

    #[test]
    fn policy_acceptance_refuses_wrong_subject_identity_replay_or_malformed_frames() {
        for invalid in [
            log().replace("root/pid: 1 19", "root/pid: 2 19"),
            log().replace("root/pid: 1 19", "root/pid: 1 21"),
            log().replace(&frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 19),
                          &frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 21)),
            log().replace(&frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 19),
                          &frame(3, JOB_POLICY_CACHE_EVENT, 2, 0, 19)),
            log().replace(&frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 19),
                          &frame(3, JOB_POLICY_CACHE_EVENT, 1, 2, 19)),
            log().replace("arg1=0x0 pid=19 tid=20", "arg1=0x1 pid=19 tid=20"),
            format!("{}\n{}", log(), frame(3, JOB_POLICY_CACHE_EVENT, 1, 0, 19)),
            format!("{}\nrootd: job policy ok root/pid: 1 19", log()),
        ] {
            assert!(validate_job_policy_log(&invalid).is_err());
        }
    }

    #[test]
    fn policy_acceptance_refuses_retained_cleanup_or_restore_failure() {
        for failure in ["rootd: job policy failed code/errno: 5 5",
                        "rootd: job policy cleanup retained authority",
                        "rootd: job policy root restore failed"] {
            assert!(validate_job_policy_log(&format!("{}\n{failure}", log())).is_err());
        }
    }
}
