//! SMP and private-KVM qualification test implementations.
//!
//! Owner: xtask KVM qualification admission. The parent module retains the
//! stable libtest witness names required by implementation-mutation evidence;
//! this module owns their focused records, hostile-frame cases, and option
//! admission checks. Evidence: `formal/implementation-mutations.tsv`.

use super::*;

pub(super) fn smoke_readiness_budget_starts_only_after_both_guests_spawn() {
    let source = include_str!("../options.rs");
    let smoke_start = source
        .find("pub(crate) fn kvm_smoke_command")
        .expect("bounded smoke command");
    let interactive_start = source
        .find("pub(crate) fn kvm_run_command")
        .expect("interactive KVM command");
    let smoke = &source[smoke_start..interactive_start];
    let capture = smoke
        .find("smp_qualification::capture_kvm_launch_evidence(")
        .expect("prelaunch evidence capture");
    let bounded_relay = smoke
        .find("let control_relay = start_dvm_input_relay(")
        .expect("bounded control-relay start");
    let spawn = smoke
        .find("let (mut rustos, mut dvm) = spawn_guests(")
        .expect("parallel guest spawn");
    let boot_started = smoke
        .find("let boot_started = Instant::now();")
        .expect("readiness budget start");
    let deadline = smoke
        .find("let deadline = boot_started + options.timeout;")
        .expect("readiness deadline");
    assert_eq!(
        smoke
            .matches("let control_relay = start_dvm_input_relay(")
            .count(),
        1
    );
    assert!(!smoke[..capture].contains("start_dvm_input_relay("));
    assert!(capture < bounded_relay);
    assert!(bounded_relay < spawn);
    assert!(spawn < boot_started);
    assert!(boot_started < deadline);
}

/// The interactive path seals its formal profile, and seals it early.
///
/// A missing seal on this path means an edited tree, not an intent to launch
/// unverified, so `kvm_run_command` runs the profile's own verification rather
/// than failing. That has to happen before the run claims a layout, a
/// doorbell, or a relay: sealing takes minutes, and a launch that already owns
/// host resources would hold them for the whole verification.
pub(super) fn interactive_multicore_run_seals_formal_evidence_before_claiming_resources() {
    let source = include_str!("../options.rs");
    let interactive_start = source
        .find("pub(crate) fn kvm_run_command")
        .expect("interactive KVM command");
    let interactive = &source[interactive_start..];
    let seal = interactive
        .find("crate::formal_contracts::ensure_smp_launch_evidence(")
        .expect("interactive formal seal");
    let layout = interactive
        .find("let layout = prepare_layout(config, &options)?;")
        .expect("interactive layout");
    let doorbell = interactive
        .find("let input_doorbell = start_dvm_input_doorbell(&layout)?;")
        .expect("interactive input doorbell");
    let spawn = interactive
        .find("let (mut rustos, mut dvm) = spawn_guests(")
        .expect("interactive guest spawn");
    assert!(seal < layout);
    assert!(layout < doorbell);
    assert!(doorbell < spawn);
    // The seal is a repair, never a bypass: the single-CPU launch has no
    // profile to seal, and `--no-auto-verify` restores the refusal.
    assert!(interactive[..seal].contains("options.smp_iteration || options.rustos_vcpus > 1"));
    assert!(interactive[..seal].contains("if auto_verify {"));
}

fn framed_smp_event(output_seq: u64, name: &str, cpu: u8, arg1: u64) -> String {
    let category = if name == "smp-cpu-online" {
        "boot"
    } else {
        "sched"
    };
    let semantic = format!(
        "milestone-begin v=1 output_seq={output_seq} seq={output_seq} ts_us={} tick={} cat={category} name={name} arg0={:#x} arg1={:#x} pid=- tid=- dropped=0 discarded_bytes=0",
        output_seq * 10,
        output_seq * 2,
        cpu,
        arg1,
    );
    let checksum = milestone_frame_checksum(semantic.as_bytes());
    format!(
        "seq={output_seq} ts_us={} tick={} lvl=info cat={category} mod=nucleus_core::debug line=0 pid=- tid=- msg=\"{semantic} checksum={checksum:016x} milestone-end\"\n",
        output_seq * 10,
        output_seq * 2,
    )
}

#[derive(Clone)]
struct QualificationRecord {
    phase: &'static str,
    worker_id: u32,
    observed_cpu: u32,
    process_id: u64,
    thread_id: u64,
    work_units: u64,
    binding_id: u64,
    guest_ts_us: u64,
    milestone_seq: Option<u64>,
    milestones_dropped: u64,
    debug_bytes_discarded: u64,
}

fn qualification_records(workers: u8) -> Vec<QualificationRecord> {
    let mut records = Vec::new();
    let mut guest_ts_us = 1_000_u64;
    for phase in [
        "smp-qualification-ready",
        "smp-qualification-start",
        "smp-qualification-finish",
    ] {
        for worker_id in 0..u32::from(workers) {
            records.push(QualificationRecord {
                phase,
                worker_id,
                observed_cpu: worker_id,
                process_id: 71,
                thread_id: 1_000 + u64::from(worker_id),
                work_units: SMP_QUALIFICATION_WORK_UNITS,
                binding_id: 0x5a_0001,
                guest_ts_us,
                milestone_seq: None,
                milestones_dropped: 0,
                debug_bytes_discarded: 0,
            });
            guest_ts_us += 1_000;
        }
    }
    records.push(QualificationRecord {
        phase: "smp-qualification-complete",
        worker_id: 0,
        observed_cpu: 0,
        process_id: 71,
        thread_id: 1_000,
        work_units: SMP_QUALIFICATION_WORK_UNITS,
        binding_id: 0x5a_0001,
        guest_ts_us,
        milestone_seq: None,
        milestones_dropped: 0,
        debug_bytes_discarded: 0,
    });
    records
}

fn framed_qualification_records(records: &[QualificationRecord]) -> String {
    records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let output_seq = u64::try_from(index + 1).unwrap();
            let milestone_seq = record.milestone_seq.unwrap_or(output_seq);
            let arg0 = (u64::from(record.observed_cpu) << 32) | u64::from(record.worker_id);
            let arg1 = (record.binding_id << SMP_QUALIFICATION_WORK_BITS) | record.work_units;
            let semantic = format!(
                "milestone-begin v=1 output_seq={output_seq} seq={milestone_seq} ts_us={} tick={} cat=compat name={} arg0={arg0:#x} arg1={:#x} pid={} tid={} dropped={} discarded_bytes={}",
                record.guest_ts_us,
                output_seq * 2,
                record.phase,
                arg1,
                record.process_id,
                record.thread_id,
                record.milestones_dropped,
                record.debug_bytes_discarded,
            );
            let checksum = milestone_frame_checksum(semantic.as_bytes());
            format!(
                "seq={output_seq} ts_us={} tick={} lvl=info cat=compat mod=nucleus_core::debug line=0 pid={} tid={} msg=\"{semantic} checksum={checksum:016x} milestone-end\"\n",
                record.guest_ts_us,
                output_seq * 2,
                record.process_id,
                record.thread_id,
            )
        })
        .collect()
}

fn assert_qualification_rejected(records: &[QualificationRecord], workers: u8) {
    let log = framed_qualification_records(records);
    let events = verified_smp_qualification_events(&log);
    assert!(validate_smp_ring3_qualification_events(&events, workers).is_err());
    assert!(!smp_ring3_qualification_is_complete(&log, workers));
}

pub(super) fn rustos_smp_topology_is_machine_gated_on_complete_prerequisites() {
    assert_eq!(RUSTOS_SMP_READINESS.rustos_vcpus, 1);
    assert!(RUSTOS_SMP_READINESS.validate(None).is_ok());

    let incomplete_multi = RustosSmpReadiness {
        rustos_vcpus: 2,
        ..RUSTOS_SMP_READINESS
    };
    assert!(incomplete_multi.validate(None).is_err());

    let evidence = crate::formal_contracts::validated_smp_launch_evidence_for_tests();
    assert!(incomplete_multi.validate(Some(&evidence)).is_ok());
}

pub(super) fn rustos_smp_runtime_requires_every_requested_cpu_event_class() {
    let mut log = String::new();
    let mut sequence = 1_u64;
    for cpu in 0..2 {
        for name in [
            "smp-cpu-online",
            "smp-cpu-idle-enter",
            "smp-cpu-first-clockevent",
            "smp-cpu-first-user-dispatch",
        ] {
            log.push_str(&framed_smp_event(sequence, name, cpu, 1));
            sequence += 1;
        }
        log.push_str(&framed_smp_event(
            sequence,
            "smp-cpu-first-reschedule-ipi",
            cpu,
            1,
        ));
        sequence += 1;
    }
    assert!(smp_runtime_missing_markers(&log, 2).is_empty());
    assert_eq!(verified_smp_runtime_events(&log, 2).len(), 10);
    let incomplete = log
        .lines()
        .filter(|line| {
            !(line.contains("name=smp-cpu-first-user-dispatch") && line.contains("arg0=0x1"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        smp_runtime_missing_markers(&incomplete, 2),
        vec!["name=smp-cpu-first-user-dispatch arg0=0x1"]
    );
}

pub(super) fn smp_runtime_rejects_interleaved_or_route_only_substrings_as_evidence() {
    let complete = framed_smp_event(9, "smp-cpu-first-user-dispatch", 5, 1);
    assert!(parse_verified_smp_runtime_event(complete.trim_end(), 1).is_some());

    let interleaved = complete.replace(
        "name=smp-cpu-first-user-dispatch",
        "name=smp-cpu-first-usipc slow call: endpoint=65537 er-dispatch",
    );
    assert!(interleaved.contains("arg0=0x5"));
    assert!(parse_verified_smp_runtime_event(interleaved.trim_end(), 1).is_none());

    let tampered_but_parseable = complete.replacen("dropped=0", "dropped=1", 1);
    assert!(parse_verified_smp_runtime_event(tampered_but_parseable.trim_end(), 1).is_none());

    let mut noncanonical_checksum = complete.clone().into_bytes();
    let checksum_start = complete.find(" checksum=").unwrap() + " checksum=".len();
    let lowercase_hex = noncanonical_checksum[checksum_start..checksum_start + 16]
        .iter()
        .position(|byte| matches!(byte, b'a'..=b'f'))
        .expect("test checksum must contain one alphabetic nibble");
    noncanonical_checksum[checksum_start + lowercase_hex] =
        noncanonical_checksum[checksum_start + lowercase_hex].to_ascii_uppercase();
    let noncanonical_checksum = String::from_utf8(noncanonical_checksum).unwrap();
    assert!(parse_verified_smp_runtime_event(noncanonical_checksum.trim_end(), 1).is_none());

    let route_only = framed_smp_event(10, "smp-resched-route", 5, 1);
    assert!(parse_verified_smp_runtime_event(route_only.trim_end(), 1).is_none());
}

pub(super) fn smp_ring3_qualification_accepts_complete_exact_worker_sets() {
    for workers in [1_u8, 2, 4, 8] {
        let log = framed_qualification_records(&qualification_records(workers));
        let events = verified_smp_qualification_events(&log);
        assert_eq!(events.len(), usize::from(workers) * 3 + 1);
        assert!(events.iter().all(|event| {
            event.binding_id == 0x5a_0001 && event.work_units == SMP_QUALIFICATION_WORK_UNITS
        }));
        assert!(validate_smp_ring3_qualification_events(&events, workers).is_ok());
        assert!(smp_ring3_qualification_is_complete(&log, workers));
    }
    let mut concurrent_ready_publication = qualification_records(2);
    concurrent_ready_publication[0].milestone_seq = Some(2);
    concurrent_ready_publication[1].milestone_seq = Some(1);
    let log = framed_qualification_records(&concurrent_ready_publication);
    let events = verified_smp_qualification_events(&log);
    assert!(validate_smp_ring3_qualification_events(&events, 2).is_ok());
    assert_qualification_rejected(&qualification_records(3), 3);
}

pub(super) fn smp_ring3_qualification_rejects_missing_duplicate_and_replayed_phases() {
    let mut missing = qualification_records(2);
    missing.retain(|record| !(record.worker_id == 1 && record.phase == "smp-qualification-finish"));
    assert_qualification_rejected(&missing, 2);

    let mut missing_terminal = qualification_records(2);
    missing_terminal.retain(|record| record.phase != "smp-qualification-complete");
    assert_qualification_rejected(&missing_terminal, 2);

    let mut substituted_terminal = qualification_records(2);
    substituted_terminal.last_mut().unwrap().phase = "smp-qualification-finish";
    assert_qualification_rejected(&substituted_terminal, 2);

    let duplicate = qualification_records(2);
    let mut replayed_log = framed_qualification_records(&duplicate);
    let first = replayed_log.lines().next().unwrap().to_owned();
    replayed_log.push_str(&first);
    replayed_log.push('\n');
    let events = verified_smp_qualification_events(&replayed_log);
    assert!(validate_smp_ring3_qualification_events(&events, 2).is_err());
    assert!(!smp_ring3_qualification_is_complete(&replayed_log, 2));
}

pub(super) fn smp_ring3_qualification_rejects_process_and_thread_substitution() {
    let mut process_substitution = qualification_records(2);
    process_substitution
        .iter_mut()
        .find(|record| record.worker_id == 1 && record.phase == "smp-qualification-start")
        .unwrap()
        .process_id = 72;
    assert_qualification_rejected(&process_substitution, 2);

    let mut two_consistent_processes = qualification_records(2);
    for record in two_consistent_processes
        .iter_mut()
        .filter(|record| record.worker_id == 1)
    {
        record.process_id = 72;
    }
    assert_qualification_rejected(&two_consistent_processes, 2);

    let mut thread_substitution = qualification_records(2);
    thread_substitution
        .iter_mut()
        .find(|record| record.worker_id == 1 && record.phase == "smp-qualification-finish")
        .unwrap()
        .thread_id = 2_000;
    assert_qualification_rejected(&thread_substitution, 2);

    let mut shared_thread = qualification_records(2);
    for record in shared_thread
        .iter_mut()
        .filter(|record| record.worker_id == 1)
    {
        record.thread_id = 1_000;
    }
    assert_qualification_rejected(&shared_thread, 2);
}

pub(super) fn smp_ring3_qualification_rejects_loss_wrong_cpu_and_work() {
    let mut loss = qualification_records(1);
    loss[0].milestones_dropped = 1;
    assert_qualification_rejected(&loss, 1);
    let mut discarded = qualification_records(1);
    discarded[0].debug_bytes_discarded = 1;
    assert_qualification_rejected(&discarded, 1);

    let mut wrong_cpu = qualification_records(2);
    wrong_cpu[0].observed_cpu = 1;
    assert_qualification_rejected(&wrong_cpu, 2);
    let mut wrong_work = qualification_records(1);
    for record in &mut wrong_work {
        record.work_units = SMP_QUALIFICATION_WORK_UNITS - 1;
    }
    assert_qualification_rejected(&wrong_work, 1);
}

pub(super) fn smp_ring3_qualification_rejects_zero_or_nonuniform_kernel_binding() {
    let mut zero_binding = qualification_records(2);
    for record in &mut zero_binding {
        record.binding_id = 0;
    }
    assert_qualification_rejected(&zero_binding, 2);

    let mut nonuniform_binding = qualification_records(2);
    nonuniform_binding
        .iter_mut()
        .find(|record| record.worker_id == 1 && record.phase == "smp-qualification-start")
        .unwrap()
        .binding_id = 0x5a_0002;
    assert_qualification_rejected(&nonuniform_binding, 2);

    let mut wrong_low_work_bits = qualification_records(2);
    for record in &mut wrong_low_work_bits {
        record.work_units = SMP_QUALIFICATION_WORK_UNITS - 1;
    }
    assert_qualification_rejected(&wrong_low_work_bits, 2);
}

pub(super) fn smp_ring3_qualification_rejects_phase_order_and_deadline() {
    let mut phase_order = qualification_records(2);
    phase_order.swap(1, 2);
    assert_qualification_rejected(&phase_order, 2);

    let mut equal_timestamp_barrier_bypass = qualification_records(2);
    equal_timestamp_barrier_bypass.swap(1, 2);
    equal_timestamp_barrier_bypass[1].guest_ts_us = 3_000;
    equal_timestamp_barrier_bypass[2].guest_ts_us = 3_000;
    assert_qualification_rejected(&equal_timestamp_barrier_bypass, 2);

    let mut equal_timestamp_finish_before_start = qualification_records(2);
    equal_timestamp_finish_before_start.swap(2, 4);
    for record in &mut equal_timestamp_finish_before_start[2..=4] {
        record.guest_ts_us = 5_000;
    }
    assert_qualification_rejected(&equal_timestamp_finish_before_start, 2);

    let mut terminal_before_last_finish = qualification_records(2);
    terminal_before_last_finish.swap(5, 6);
    terminal_before_last_finish[5].guest_ts_us = 7_000;
    terminal_before_last_finish[6].guest_ts_us = 7_000;
    assert_qualification_rejected(&terminal_before_last_finish, 2);

    let mut deadline = qualification_records(1);
    deadline[2].guest_ts_us = deadline[1].guest_ts_us + SMP_QUALIFICATION_DEADLINE_US + 1;
    assert_qualification_rejected(&deadline, 1);

    let mut refreshed_global_deadline = qualification_records(2);
    let final_index = refreshed_global_deadline.len() - 1;
    refreshed_global_deadline[final_index].guest_ts_us =
        refreshed_global_deadline[0].guest_ts_us + SMP_QUALIFICATION_DEADLINE_US + 1;
    assert!(
        refreshed_global_deadline[final_index].guest_ts_us
            - refreshed_global_deadline[3].guest_ts_us
            <= SMP_QUALIFICATION_DEADLINE_US
    );
    assert_qualification_rejected(&refreshed_global_deadline, 2);
}

pub(super) fn smp_ring3_qualification_rejects_interleaved_tampered_and_plain_frames() {
    let complete = framed_qualification_records(&qualification_records(1));
    let line = complete.lines().next().unwrap();
    assert!(parse_verified_milestone_frame(line, 1).is_some());
    assert!(parse_verified_smp_qualification_event(line, 1).is_some());

    let interleaved = line.replace(
        "name=smp-qualification-ready",
        "name=smp-qualification-reipc slow call: endpoint=65537 ady",
    );
    assert!(parse_verified_smp_qualification_event(interleaved.trim_end(), 1).is_none());

    let tampered = line.replacen("arg1=0x", "arg1=0x1", 1);
    assert!(parse_verified_smp_qualification_event(tampered.trim_end(), 1).is_none());
    let outer_pid_substitution = line.replacen("pid=71", "pid=72", 1);
    assert!(parse_verified_milestone_frame(outer_pid_substitution.trim_end(), 1).is_none());
    assert!(verified_smp_qualification_events("smp-qualification-ready worker=0").is_empty());
}

pub(super) fn private_kvm_contract_renderers_are_canonical() {
    assert_eq!(
        render_private_acceptance_contract(true, false),
        "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\n"
    );
    assert_eq!(
        render_smp_ring3_qualification_contract(8),
        "contract=rustos-kvm-smp-qualification-v1\nworkers=8\nwork_units=1000000\ndeadline_ms=5000\n"
    );
}

pub(super) fn smp_boot_acceptance_uses_kernel_stamped_milestones_when_text_interleaves() {
    let log = "seq=107 msg=\"milestone name=product-root-core-ready\"\n\
seq=119 msg=\"milestone name=product-init-identity-ready\"";
    assert!(rustos_marker_present(log, RUSTOS_BOOT_MARKER));
    assert!(rustos_marker_present(log, RUSTOS_INIT_IDENTITY_MARKER));
    assert!(!rustos_marker_present(
        log,
        RUSTOS_GPU_SCENE_COMPILER_MARKER
    ));

    let successor_only =
        "seq=119 msg=\"milestone name=product-init-identity-ready arg0=0x0 arg1=0x0\"";
    assert!(rustos_marker_present(successor_only, RUSTOS_BOOT_MARKER));
    assert!(rustos_marker_present(
        successor_only,
        RUSTOS_INIT_IDENTITY_MARKER
    ));
}

pub(super) fn smp_iteration_is_bounded_and_cannot_claim_acceptance() {
    let options = parse_smoke_options(
        vec![
            "--rustos-vcpus".into(),
            "2".into(),
            "--smp-iteration".into(),
            "--timeout".into(),
            "30".into(),
        ]
        .into_iter(),
    )
    .unwrap();
    assert!(options.smp_iteration);
    assert_eq!(options.rustos_vcpus, 2);
    assert!(
        parse_smoke_options(
            vec![
                "--rustos-vcpus".into(),
                "2".into(),
                "--smp-iteration".into(),
                "--timeout".into(),
                "31".into(),
            ]
            .into_iter()
        )
        .is_err()
    );
    assert!(
        parse_smoke_options(
            vec![
                "--rustos-vcpus".into(),
                "2".into(),
                "--smp-iteration".into(),
                "--timeout".into(),
                "30".into(),
                "--min-ui-fps".into(),
                "55".into(),
            ]
            .into_iter()
        )
        .is_err()
    );
    assert!(
        parse_smoke_options(
            vec![
                "--rustos-vcpus".into(),
                "2".into(),
                "--smp-iteration".into(),
                "--timeout".into(),
                "30".into(),
                "--recovery-probe".into(),
                "all".into(),
            ]
            .into_iter()
        )
        .is_err()
    );
}

pub(super) fn smp_ring3_qualification_has_exact_private_kvm_admission() {
    let default_options = parse_smoke_options(Vec::new().into_iter()).unwrap();
    assert!(!default_options.smp_ring3_qualification);
    assert!(!default_options.dvm_block_shmem);
    let nonqualification = parse_smoke_options(
        vec![
            "--rustos-vcpus".into(),
            "2".into(),
            "--smp-iteration".into(),
            "--timeout".into(),
            "30".into(),
        ]
        .into_iter(),
    )
    .unwrap();
    assert!(!nonqualification.smp_ring3_qualification);
    assert!(!nonqualification.dvm_block_shmem);

    for rustos_vcpus in ["1", "2", "4", "8"] {
        let qualified = parse_smoke_options(
            vec![
                "--rustos-vcpus".into(),
                rustos_vcpus.into(),
                "--smp-iteration".into(),
                "--smp-ring3-qualification".into(),
                "--smp-evidence-cohort".into(),
                "0123456789abcdef0123456789abcdef".into(),
                "--timeout".into(),
                "30".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert!(qualified.smp_iteration);
        assert!(qualified.smp_ring3_qualification);
        assert!(qualified.dvm_block_shmem);
        assert_eq!(
            qualified.smp_evidence_cohort.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(qualified.rustos_vcpus.to_string(), rustos_vcpus);
    }

    for rustos_vcpus in ["3", "5", "6", "7"] {
        assert!(
            parse_smoke_options(
                vec![
                    "--rustos-vcpus".into(),
                    rustos_vcpus.into(),
                    "--smp-iteration".into(),
                    "--smp-ring3-qualification".into(),
                    "--smp-evidence-cohort".into(),
                    "0123456789abcdef0123456789abcdef".into(),
                    "--timeout".into(),
                    "30".into(),
                ]
                .into_iter(),
            )
            .is_err()
        );
    }
    assert!(
        parse_smoke_options(
            vec![
                "--rustos-vcpus".into(),
                "2".into(),
                "--smp-ring3-qualification".into(),
                "--smp-evidence-cohort".into(),
                "0123456789abcdef0123456789abcdef".into(),
                "--timeout".into(),
                "30".into(),
            ]
            .into_iter(),
        )
        .is_err()
    );
    for incompatible in [
        vec!["--min-ui-fps", "60"],
        vec!["--recovery-probe", "all"],
        vec!["--physical-gpu", "0000:65:00.0"],
    ] {
        let mut args = vec![
            "--rustos-vcpus".to_owned(),
            "2".to_owned(),
            "--smp-iteration".to_owned(),
            "--smp-ring3-qualification".to_owned(),
            "--smp-evidence-cohort".to_owned(),
            "0123456789abcdef0123456789abcdef".to_owned(),
            "--timeout".to_owned(),
            "30".to_owned(),
        ];
        args.extend(incompatible.into_iter().map(str::to_owned));
        assert!(parse_smoke_options(args.into_iter()).is_err());
    }
    assert!(
        parse_smoke_options(
            vec![
                "--rustos-vcpus".into(),
                "2".into(),
                "--smp-iteration".into(),
                "--smp-ring3-qualification".into(),
                "--smp-evidence-cohort".into(),
                "0123456789abcdef0123456789abcdef".into(),
                "--timeout".into(),
                "31".into(),
            ]
            .into_iter(),
        )
        .is_err()
    );
}

pub(super) fn smp_evidence_cohort_is_strict_and_paired() {
    let paired = vec![
        "--rustos-vcpus".into(),
        "2".into(),
        "--smp-iteration".into(),
        "--smp-ring3-qualification".into(),
        "--smp-evidence-cohort".into(),
        "0123456789abcdef0123456789abcdef".into(),
        "--timeout".into(),
        "30".into(),
    ];
    assert!(parse_smoke_options(paired.clone().into_iter()).is_ok());

    for invalid in [
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeF",
        "../../0123456789abcdef0123456789ab",
        "0123456789abcdef0123456789abcdeg",
    ] {
        let mut args = paired.clone();
        let value = args
            .iter()
            .position(|arg| arg == "0123456789abcdef0123456789abcdef")
            .unwrap();
        args[value] = invalid.to_owned();
        assert!(parse_smoke_options(args.into_iter()).is_err(), "{invalid}");
    }

    let missing = paired
        .iter()
        .filter(|arg| {
            arg.as_str() != "--smp-evidence-cohort"
                && arg.as_str() != "0123456789abcdef0123456789abcdef"
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(parse_smoke_options(missing.into_iter()).is_err());
    assert!(
        parse_smoke_options(
            vec![
                "--smp-evidence-cohort".into(),
                "0123456789abcdef0123456789abcdef".into(),
            ]
            .into_iter(),
        )
        .is_err()
    );
}
