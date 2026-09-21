use super::*;

#[test]
fn benchmark_timeout_distinguishes_short_control_from_smp_and_isolation() {
    assert_eq!(benchmark_timeout_seconds(1, false), 30);
    assert_eq!(benchmark_timeout_seconds(8, false), 60);
    assert_eq!(benchmark_timeout_seconds(1, true), 60);
    assert_eq!(benchmark_timeout_seconds(8, true), 90);
}

#[test]
fn smp_isolated_runs_never_claim_system_wide_phase_counters_as_probe_private() {
    assert!(strict_phase_attribution_required(1, "fork_exit_wait"));
    assert!(!strict_phase_attribution_required(8, "fork_exit_wait"));
    assert!(!strict_phase_attribution_required(
        1,
        "scheduling_budget_exhaust_refill"
    ));
}

/// Two real runs of this lane, four minutes apart, with a guest change
/// between them that touches neither `vmexit_cpuid` nor `null_syscall_getpid`.
const DRIFTED_BASELINE: &str = "\
tsc_overhead                                50000         40         80         80         68        10        20
null_syscall_getpid                         50000       3840       3880       9720       7504       962       972
vmexit_cpuid                                50000       4760       4800       5120       6919      1192      1202
ipc_rt_intra_process                        20000     118160     121720     394400     198472     29604     30496
";

/// The lane reports ticks of an invariant TSC, and the core clock is not
/// the TSC. A host that boosts higher finishes the same work in fewer
/// ticks, and every probe improves at once -- including one with no code of
/// ours in it. Reading that as a win is the failure this guards.
#[test]
fn a_host_clock_shift_is_reported_as_drift_rather_than_as_an_improvement() {
    let results = vec![
        BenchResult {
            name: "tsc_overhead".to_owned(),
            iters: 50_000,
            min: 40,
            p50: 40,
            p99: 80,
            mean: 58,
            min_ns: 10,
            p50_ns: 10,
        },
        BenchResult {
            name: "vmexit_cpuid".to_owned(),
            iters: 50_000,
            min: 3_960,
            p50: 4_040,
            p99: 6_840,
            mean: 9_072,
            min_ns: 992,
            p50_ns: 1_012,
        },
        BenchResult {
            name: "ipc_rt_intra_process".to_owned(),
            iters: 20_000,
            min: 97_680,
            p50: 100_000,
            p99: 200_000,
            mean: 150_000,
            min_ns: 24_474,
            p50_ns: 25_000,
        },
    ];
    let report = render_comparison(Path::new("baseline.txt"), DRIFTED_BASELINE, &results);

    assert!(report.contains("MOVED"), "drift must be stated: {report}");
    // The raw column still says -17%, which is exactly why it must not be
    // the only column: the anchor moved by the same proportion.
    assert!(
        report.contains("-17.3%"),
        "raw delta belongs in the report: {report}"
    );
    // Normalized against the anchor, the round trip did not move.
    assert!(
        report.contains("-0.6%") || report.contains("-0.7%"),
        "normalized delta must show the change was not the guest: {report}"
    );
    // Both runs read 40 ticks for `tsc_overhead`; scaling that would print
    // a 20% regression in the measurement counter itself.
    assert!(
        report.contains("floor"),
        "a probe at the counter granularity must not be normalized: {report}"
    );
    assert!(report.contains("Rerun both sides"), "{report}");
}

/// The instrument's own spread was measured at about two percent across
/// three runs of one unchanged image. A delta inside that is a reading of
/// the harness, and saying so is the difference between a measurement and
/// a story about one.
#[test]
fn a_delta_inside_the_instrument_spread_is_labelled_noise() {
    let results = vec![
        BenchResult {
            name: "vmexit_cpuid".to_owned(),
            iters: 50_000,
            min: 4_760,
            p50: 4_800,
            p99: 5_120,
            mean: 6_919,
            min_ns: 1_192,
            p50_ns: 1_202,
        },
        // One percent above where the anchor says it should have landed.
        BenchResult {
            name: "ipc_rt_intra_process".to_owned(),
            iters: 20_000,
            min: 119_340,
            p50: 122_000,
            p99: 400_000,
            mean: 200_000,
            min_ns: 29_900,
            p50_ns: 30_500,
        },
    ];
    let baseline = concat!(
        "probe                                       iters    min_cyc\n",
        "vmexit_cpuid                                50000       4760\n",
        "ipc_rt_intra_process                        20000     118160\n",
    );
    let report = render_comparison(Path::new("baseline.txt"), baseline, &results);

    assert!(
        report.contains("1.0% noise"),
        "a sub-spread delta must carry its label: {report}"
    );
}

/// A comparison with no anchor is not a comparison. Reporting deltas anyway
/// would be the same error with the evidence removed.
#[test]
fn a_comparison_without_the_anchor_refuses_to_report_deltas() {
    let baseline = "ipc_rt_intra_process 20000 118160 121720 394400 198472 29604 30496\n";
    let results = vec![BenchResult {
        name: "ipc_rt_intra_process".to_owned(),
        iters: 20_000,
        min: 97_680,
        p50: 100_000,
        p99: 200_000,
        mean: 150_000,
        min_ns: 24_474,
        p50_ns: 25_000,
    }];
    let report = render_comparison(Path::new("baseline.txt"), baseline, &results);
    assert!(report.contains("no comparison"), "{report}");
    assert!(
        !report.contains("-17"),
        "no delta may be reported: {report}"
    );
}

/// A stable anchor is the case the lane is for, and it must not warn.
#[test]
fn a_held_anchor_reports_the_raw_delta_without_a_drift_warning() {
    let results = vec![
        BenchResult {
            name: "vmexit_cpuid".to_owned(),
            iters: 50_000,
            min: 4_800,
            p50: 4_840,
            p99: 5_120,
            mean: 6_919,
            min_ns: 1_202,
            p50_ns: 1_212,
        },
        BenchResult {
            name: "ipc_rt_intra_process".to_owned(),
            iters: 20_000,
            min: 106_344,
            p50: 110_000,
            p99: 200_000,
            mean: 150_000,
            min_ns: 26_646,
            p50_ns: 27_000,
        },
    ];
    let report = render_comparison(Path::new("baseline.txt"), DRIFTED_BASELINE, &results);
    assert!(report.contains("held"), "{report}");
    assert!(!report.contains("MOVED"), "{report}");
    assert!(!report.contains("Rerun both sides"), "{report}");
}

const SAMPLE: &str = "\
user-debug payload=ipcbench: tsc_khz=3990809\\n
user-debug payload=ipcbench: result name=null_syscall_getpid iters=50000 min=3360 p50=3400 p90=3400 p99=5960 max=71972000 mean=6751 min_ns=841 p50_ns=851 mean_ns=1691\\n
user-debug payload=ipcbench: result name=vmexit_cpuid iters=50000 min=4760 p50=4800 p90=5000 p99=5200 max=8000 mean=4900 min_ns=1192 p50_ns=1202 mean_ns=1227\\n
user-debug payload=ipcbench: skip name=other reason=unavailable\\n
user-debug payload=ipcbench: end\\n";

/// One real milestone line, verbatim, including the debugcon envelope the
/// guest wraps it in.
const MILESTONE: &str = "seq=316 ts_us=3238281 tick=3316 lvl=info cat=compat mod=nucleus_core::debug line=0 pid=- tid=- msg=\"milestone-begin v=1 output_seq=316 seq=281 ts_us=3238281 tick=3316 cat=compat name=ipc-call-phase-wait-take arg0=0x100 arg1=0x4 pid=- tid=- dropped=0 discarded_bytes=0 checksum=be4fc44b6e85f301 milestone-end\"";
const FAST_COUNTER: &str = "seq=317 ts_us=3238282 tick=3317 lvl=info cat=compat mod=nucleus_core::debug line=0 pid=- tid=- msg=\"milestone-begin v=1 output_seq=317 seq=282 ts_us=3238282 tick=3317 cat=compat name=ipc-fastpath-counter arg0=0xd arg1=0x20 pid=- tid=- dropped=0 discarded_bytes=0 checksum=0 milestone-end\"";
const LIFECYCLE_MILESTONE: &str = "seq=318 ts_us=3238283 tick=3318 lvl=info cat=process mod=nucleus_core::debug line=0 pid=- tid=- msg=\"milestone-begin v=1 output_seq=318 seq=283 ts_us=3238283 tick=3318 cat=process name=lifecycle-exec-publish arg0=0x700000003 arg1=0x90000000b pid=- tid=- dropped=0 discarded_bytes=0 checksum=0 milestone-end\"";
const LOCAL_DISPATCH_MILESTONE: &str = "seq=319 ts_us=3238284 tick=3319 lvl=info cat=sched mod=nucleus_core::debug line=0 pid=- tid=- msg=\"milestone-begin v=1 output_seq=319 seq=284 ts_us=3238284 tick=3319 cat=sched name=kernel-scheduler-local-shadow arg0=0x0000000200000003 arg1=0x0000000400000005 pid=- tid=- dropped=0 discarded_bytes=0 checksum=0 milestone-end\"";

#[test]
fn parses_wrapped_debugcon_payload_lines() {
    let run = parse_log(SAMPLE).expect("sample parses");
    assert_eq!(run.tsc_khz, 3_990_809);
    assert_eq!(run.results.len(), 2);
    assert_eq!(run.results[0].name, "null_syscall_getpid");
    assert_eq!(run.results[0].min, 3360);
    assert_eq!(run.results[0].p50_ns, 851);
    assert_eq!(run.skipped.len(), 1);
}

#[test]
fn scheduler_policy_window_after_begin_and_end_is_retained() {
    let log = format!(
        "{LOCAL_DISPATCH_MILESTONE}\nuser-debug payload=ipcbench: begin\n{LOCAL_DISPATCH_MILESTONE}\n{SAMPLE}\n{LOCAL_DISPATCH_MILESTONE}"
    );
    let run = parse_log(&log).expect("begin-to-final policy window parses");
    assert_eq!(run.local_shadow.qualified, 4);
    assert_eq!(run.local_shadow.matches, 6);
    assert_eq!(run.local_shadow.mismatches, 8);
    assert_eq!(run.local_shadow.catalog_fallbacks, 10);
}

#[test]
fn isolated_probe_requires_its_exact_primary_result_without_a_skip() {
    let run = parse_log(SAMPLE).expect("sample parses");
    assert!(isolated_primary_result_holds(&run, "null_syscall_getpid"));
    assert!(!isolated_primary_result_holds(&run, "other"));
    assert!(!isolated_primary_result_holds(&run, "missing"));
}

#[test]
fn isolated_probe_without_hardware_anchor_fails_closed() {
    let without_anchor = SAMPLE
        .lines()
        .filter(|line| !line.contains("name=vmexit_cpuid"))
        .collect::<Vec<_>>()
        .join("\n");
    let run = parse_log(&without_anchor).expect("otherwise complete sample parses");
    assert!(!isolated_primary_result_holds(&run, "null_syscall_getpid"));
}

#[test]
fn isolated_report_keeps_primary_distribution_tsc_rate_and_vcpu_count() {
    let run = parse_log(SAMPLE).expect("sample parses");
    let rendered = render_isolated(&run, "null_syscall_getpid", 8, "semantic result");
    assert!(rendered.contains("rustos_vcpus=8"), "{rendered}");
    assert!(rendered.contains("tsc_khz=3990809"), "{rendered}");
    assert!(
        rendered.contains("null_syscall_getpid"),
        "primary distribution was hidden: {rendered}"
    );
    assert!(
        rendered.contains("vmexit_cpuid"),
        "hardware anchor was hidden: {rendered}"
    );
    assert!(rendered.contains("semantic result"), "{rendered}");
}

#[test]
fn lifecycle_report_decodes_exact_process_mm_and_transaction_generations() {
    let log = format!(
        "user-debug payload=ipcbench: tsc_khz=3990809\\n\n{LIFECYCLE_MILESTONE}\nuser-debug payload=ipcbench: result name=exec_replace_single_thread iters=1 min=10 p50=10 p90=10 p99=10 max=10 mean=10 min_ns=2 p50_ns=2 mean_ns=2\\n\nuser-debug payload=ipcbench: end\\n"
    );
    let run = parse_log(&log).expect("lifecycle sample parses");
    assert_eq!(run.lifecycle.len(), 1);
    let rendered = render_lifecycle(&run.lifecycle);
    assert!(rendered.contains("lifecycle-exec-publish"), "{rendered}");
    assert!(
        rendered.contains("3:7/9:11..3:7/9:11"),
        "decoded identity missing: {rendered}"
    );
}

#[test]
fn lifecycle_marker_without_exact_generation_fails_closed() {
    let malformed = LIFECYCLE_MILESTONE.replace("arg0=0x700000003", "arg0=0x3");
    let log = format!(
        "user-debug payload=ipcbench: tsc_khz=3990809\\n\n{malformed}\nuser-debug payload=ipcbench: result name=exec_replace_single_thread iters=1 min=10 p50=10 p90=10 p99=10 max=10 mean=10 min_ns=2 p50_ns=2 mean_ns=2\\n\nuser-debug payload=ipcbench: end\\n"
    );
    assert!(parse_log(&log).is_err());
}

#[test]
fn semantic_probes_do_not_misattribute_system_wide_ipc_housekeeping() {
    assert!(!requires_phase_attribution(
        "scheduling_budget_exhaust_refill"
    ));
    assert!(!requires_phase_attribution("ipc_nested_passive_server"));
    assert!(requires_phase_attribution("ipc_rt_intra_process"));
}

#[test]
fn a_run_without_the_end_marker_fails_instead_of_reporting_partial_costs() {
    let truncated = SAMPLE.replace("ipcbench: end", "ipcbench: still-running");
    assert!(parse_log(&truncated).is_err());
}

#[test]
fn a_finished_run_with_no_results_is_a_failure_not_an_empty_table() {
    let empty = "user-debug payload=ipcbench: end\\n";
    assert!(parse_log(empty).is_err());
}

#[test]
fn a_phase_charged_by_more_probes_than_the_round_trip_is_marked_unattributable() {
    // This is the trap the marker exists for. `usermem-phase-bind-visible`
    // is charged by every user copy in the run, so dividing its total by the
    // round-trip count invents a per-call cost it never had. A published
    // figure was wrong by five times before this was rendered.
    let phases = vec![
        PhaseTotal {
            name: String::from("ipc-call-phase-copy-request"),
            cycles: 1_762 * 22_987,
            samples: 22_987,
        },
        PhaseTotal {
            name: String::from("ipc-call-phase-write-response"),
            cycles: 1_618 * 22_958,
            samples: 22_958,
        },
        PhaseTotal {
            name: String::from("usermem-phase-bind-visible"),
            cycles: 677 * 335_989,
            samples: 335_989,
        },
    ];
    let rendered = render_phases(&phases);
    assert!(rendered.contains("ipc-call-phase-write-response"));
    // 22,958 / 22,987 is one per round trip; 335,989 / 22,987 is not.
    assert!(rendered.contains("1.00"), "{rendered}");
    assert!(rendered.contains("(14.62)"), "{rendered}");
    assert!(rendered.contains("shared with other probes"), "{rendered}");
}

#[test]
fn isolation_holds_when_every_phase_divides_cleanly() {
    let phases = vec![
        PhaseTotal {
            name: String::from("ipc-call-phase-copy-request"),
            cycles: 1_762 * 22_987,
            samples: 22_987,
        },
        PhaseTotal {
            name: String::from("ipc-call-phase-write-response"),
            cycles: 1_618 * 22_958,
            samples: 22_958,
        },
    ];
    assert!(isolation_holds(&phases));
}

#[test]
fn isolation_fails_when_a_once_per_call_phase_is_charged_by_more_than_the_round_trip() {
    let phases = vec![
        PhaseTotal {
            name: String::from("ipc-call-phase-copy-request"),
            cycles: 1_762 * 22_987,
            samples: 22_987,
        },
        PhaseTotal {
            name: String::from("ipc-call-phase-enqueue"),
            cycles: 677 * 335_989,
            samples: 335_989,
        },
    ];
    assert!(!isolation_holds(&phases));
}

#[test]
fn isolation_ignores_shared_topology_phase_multiplicity() {
    let phases = vec![
        PhaseTotal {
            name: String::from("ipc-call-phase-copy-request"),
            cycles: 1_762 * 22_987,
            samples: 22_987,
        },
        PhaseTotal {
            name: String::from("usermem-phase-bind-visible"),
            cycles: 677 * 335_989,
            samples: 335_989,
        },
    ];
    assert!(isolation_holds(&phases));
}

#[test]
fn isolation_holds_vacuously_when_the_probe_charges_no_ipc_call_phase() {
    // Isolating `vmexit_cpuid` charges nothing in either family, so there
    // is no round trip to divide and nothing to contradict isolation.
    assert!(isolation_holds(&[]));
    let phases = vec![PhaseTotal {
        name: String::from("usermem-phase-bind-visible"),
        cycles: 677 * 12,
        samples: 12,
    }];
    assert!(isolation_holds(&phases));
}

#[test]
fn a_run_without_the_unit_phase_claims_no_attribution_at_all() {
    let phases = vec![PhaseTotal {
        name: String::from("usermem-phase-bind-visible"),
        cycles: 677 * 335_989,
        samples: 335_989,
    }];
    let rendered = render_phases(&phases);
    assert!(rendered.contains(" -"), "{rendered}");
    assert!(!rendered.contains("shared with other probes"), "{rendered}");
}

#[test]
fn phase_milestones_inside_the_run_are_summed_per_name() {
    let log = format!(
        "user-debug payload=ipcbench: tsc_khz=3990809\n{MILESTONE}\n{MILESTONE}\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
    );
    let run = parse_log(&log).expect("run parses");
    assert_eq!(run.phases.len(), 1);
    assert_eq!(run.phases[0].name, "ipc-call-phase-wait-take");
    // Two identical windows: 0x100 cycles over 4 samples, twice.
    assert_eq!(run.phases[0].samples, 8);
    assert_eq!(run.phases[0].per_sample(), 64);
}

#[test]
fn a_phase_milestone_before_the_run_is_not_attributed_to_it() {
    // A window that closed during boot describes boot. Folding it in would
    // report a cost the benchmark never provoked.
    let log = format!(
        "{MILESTONE}\nuser-debug payload=ipcbench: tsc_khz=3990809\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
    );
    let run = parse_log(&log).expect("run parses");
    assert!(run.phases.is_empty(), "boot window leaked into the run");
}

#[test]
fn a_milestone_outside_the_profile_families_is_ignored() {
    let unrelated = MILESTONE.replace("ipc-call-phase-wait-take", "kernel-scheduler-hold-max");
    let log = format!(
        "user-debug payload=ipcbench: tsc_khz=3990809\n{unrelated}\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
    );
    let run = parse_log(&log).expect("run parses");
    assert!(run.phases.is_empty());
}

#[test]
fn fast_path_reason_counters_are_named_and_summed_inside_the_run() {
    let log = format!(
        "user-debug payload=ipcbench: tsc_khz=3990809\n{FAST_COUNTER}\n{FAST_COUNTER}\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
    );
    let run = parse_log(&log).expect("run parses");
    assert_eq!(run.counters.len(), 1);
    assert_eq!(
        run.counters[0].name,
        "ipc-fast-fallback-no-waiting-receiver"
    );
    assert_eq!(run.counters[0].units, 64);
    assert_eq!(run.counters[0].operations, 0);
    let rendered = render_counters(&run.counters);
    assert!(rendered.contains("ipc-fast-fallback-no-waiting-receiver"));
}
