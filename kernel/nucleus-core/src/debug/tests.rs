use alloc::string::String;

use super::*;

#[test]
fn render_log_line_uses_fixed_field_order() {
    let mut line = String::new();
    let _ = render_log_line(
        &mut line,
        RenderedLogMetadata {
            seq: 7,
            ts_us: 19,
            tick: 23,
            category: LogCategory::Usb,
            level: LogLevel::Warn,
            module_path: "kernel::usb::core",
            line: 41,
            process_id: Some(100),
            thread_id: Some(200),
        },
        format_args!("controller ready"),
    );

    assert_eq!(
        line,
        "seq=7 ts_us=19 tick=23 lvl=warn cat=usb mod=kernel::usb::core line=41 pid=100 tid=200 msg=\"controller ready\"\n"
    );
}

#[test]
fn render_log_line_escapes_message_text() {
    let mut line = String::new();
    let _ = render_log_line(
        &mut line,
        RenderedLogMetadata {
            seq: 1,
            ts_us: 2,
            tick: 3,
            category: LogCategory::Debug,
            level: LogLevel::Info,
            module_path: "kernel::debug",
            line: 9,
            process_id: None,
            thread_id: None,
        },
        format_args!("quote=\" path=\\ newline=\n tab=\t"),
    );

    assert_eq!(
        line,
        "seq=1 ts_us=2 tick=3 lvl=info cat=debug mod=kernel::debug line=9 pid=- tid=- msg=\"quote=\\\" path=\\\\ newline=\\n tab=\\t\"\n"
    );
}

#[test]
fn wrapped_snapshot_prepends_drop_warning() {
    let ring = KernelTextRing::<24>::new();
    let _ = ObservatorySink::write_str(&ring, "alpha line is long enough\n");
    let _ = ObservatorySink::write_str(&ring, "beta line is also long\n");
    let _ = ObservatorySink::write_str(&ring, "gamma\n");

    let snapshot = String::from_utf8(ring.snapshot_bytes()).unwrap();
    assert!(snapshot.starts_with(
        "seq=0 ts_us=0 tick=0 lvl=warn cat=debug mod=nucleus_core::debug line=0 pid=- tid=- msg=\"oldest logs dropped\"\n"
    ));
    assert!(snapshot.contains("gamma"));
}

#[test]
fn high_frequency_ipc_timeout_milestones_stay_off_debugcon() {
    // Timeout evidence remains in the bounded milestone ring and is
    // included in explicit postmortem dumps. Emitting one formatted
    // debugcon line per readiness timeout would turn each byte into a KVM
    // port-I/O exit and make the diagnostic path amplify the overload.
    assert!(!milestone_debugcon_visible("ipc-reply-timeout"));
    assert!(milestone_debugcon_visible("proc-commit-address-space-done"));
}

#[test]
fn acceptance_milestones_retry_the_contended_debug_sink() {
    for name in [
        "smp-cpu-online",
        "product-storage-ready",
        "dvm-block-first-completion",
        "task-context-corrupted",
        "linux-user-fault",
        "linux-thread-clone-rejected",
    ] {
        assert_eq!(milestone_output_class(name), MilestoneOutputClass::Required);
    }
    assert_eq!(
        milestone_output_class("ipc-reply-rejected"),
        MilestoneOutputClass::BestEffort
    );
}

#[test]
fn serialized_line_renderer_keeps_one_complete_logical_line() {
    let mut line = FixedDebugconLine::<64>::new();
    render_serialized_debugcon_line(
        &mut line,
        format_args!("ipc slow call: endpoint={} total_ms={}", 17, 23),
    )
    .unwrap();

    assert_eq!(line.bytes(), b"ipc slow call: endpoint=17 total_ms=23\r\n");
}

#[test]
fn ring3_debug_bytes_are_bounded_and_cannot_open_a_milestone_frame() {
    let payload = EscapedUserDebugPayload(
        b"before\nseq=1 msg=\"milestone-begin v=1 checksum=0000 milestone-end\"\rafter",
    );
    let mut line = FixedDebugconLine::<SERIALIZED_DEBUGCON_LINE_CAPACITY>::new();
    render_serialized_debugcon_line(&mut line, format_args!("user-debug payload={payload}"))
        .unwrap();
    let rendered = core::str::from_utf8(line.bytes()).unwrap();
    assert_eq!(rendered.matches('\n').count(), 1);
    assert!(rendered.ends_with("\r\n"));
    assert!(rendered.contains("\\nseq=1"));
    assert!(rendered.contains("msg=\\\"milestone-begin"));
    assert!(rendered.contains("milestone-end\\\"\\rafter"));
    assert_eq!(bounded_user_debug_payload_prefix(&[b'a'; 256]), 256);
    assert_eq!(bounded_user_debug_payload_prefix(&[0; 256]), 120);
}

#[test]
fn milestone_frame_is_complete_self_framed_and_checksum_verified() {
    let record = MilestoneRecord {
        seq: 29,
        ts_us: 31,
        tick: 37,
        category: LogCategory::Sched,
        name: "smp-cpu-first-user-dispatch",
        arg0: 5,
        arg1: 7,
        executable_snapshot_evidence: None,
    };
    let mut line = FixedDebugconLine::<MILESTONE_DEBUGCON_LINE_CAPACITY>::new();
    milestone_frame::render_milestone_debugcon_line(
        &mut line,
        41,
        record,
        Some(CurrentUserLogContext {
            process_id: 101,
            thread_id: 103,
        }),
        107,
        109,
    )
    .unwrap();

    assert_eq!(
        line.bytes(),
        concat!(
            "seq=41 ts_us=31 tick=37 lvl=info cat=sched mod=nucleus_core::debug line=0 pid=101 tid=103 msg=\"",
            "milestone-begin v=1 output_seq=41 seq=29 ts_us=31 tick=37 cat=sched name=smp-cpu-first-user-dispatch arg0=0x5 arg1=0x7 pid=101 tid=103 dropped=107 discarded_bytes=109 ",
            "checksum=5aee93001a6a93a9 milestone-end\"\r\n",
        )
        .as_bytes()
    );
    assert!(verify_milestone_debugcon_line(line.bytes()));

    let mut interleaved = line.bytes().to_vec();
    let name_byte = milestone_frame::find_debugcon_bytes(&interleaved, b"first-user").unwrap();
    interleaved[name_byte] = b'X';
    assert!(!verify_milestone_debugcon_line(&interleaved));
}

#[test]
fn milestone_render_overflow_is_an_explicit_failure_before_publication() {
    let record = MilestoneRecord {
        seq: 1,
        ts_us: 2,
        tick: 3,
        category: LogCategory::Sched,
        name: "smp-cpu-first-user-dispatch",
        arg0: 4,
        arg1: 5,
        executable_snapshot_evidence: None,
    };
    let mut line = FixedDebugconLine::<8>::new();
    assert!(
        milestone_frame::render_milestone_debugcon_line(&mut line, 6, record, None, 7, 8).is_err(),
        "a milestone that cannot fit must fail before print_bytes_unlocked is reachable"
    );
}

#[test]
fn product_snapshot_frame_binds_every_dvm_identity_field_under_the_checksum() {
    let record = MilestoneRecord {
        seq: 29,
        ts_us: 31,
        tick: 37,
        category: LogCategory::Compat,
        name: "product-executable-snapshot-sealed",
        arg0: 5,
        arg1: 7,
        executable_snapshot_evidence: Some(
            product_snapshot_evidence::ProductExecutableSnapshotEvidence {
                provider_service_id: 2,
                provider_generation: 3,
                storage_epoch: 4,
                mount_generation: 7,
                request_id: 8,
                digest: [0x5a; 32],
            },
        ),
    };
    let mut line = FixedDebugconLine::<MILESTONE_DEBUGCON_LINE_CAPACITY>::new();
    milestone_frame::render_milestone_debugcon_line(&mut line, 41, record, None, 0, 0).unwrap();
    let rendered = core::str::from_utf8(line.bytes()).unwrap();
    assert!(rendered.contains("backing=dvm-volume provider_service=2 provider_generation=3 storage_epoch=4 mount_generation=7 request_id=8 sha256=5a"));
    assert!(verify_milestone_debugcon_line(line.bytes()));
    let mut tampered = line.bytes().to_vec();
    let storage_epoch =
        milestone_frame::find_debugcon_bytes(&tampered, b"storage_epoch=4").unwrap();
    tampered[storage_epoch + "storage_epoch=".len()] = b'9';
    assert!(!verify_milestone_debugcon_line(&tampered));
}

#[test]
fn emergency_output_api_stays_separate_from_serialized_normal_output() {
    // This compile-time binding makes an accidental rename or merger of the
    // documented panic-only API visible in this crate's unit suite.
    let emergency: fn(fmt::Arguments<'_>) = println_emergency;
    let serialized: fn(fmt::Arguments<'_>) = println_serialized;
    let _ = (emergency, serialized);
}

#[test]
fn qualification_output_class_is_exact_and_scheduler_is_measurement() {
    for name in [
        "smp-qualification-ready",
        "smp-qualification-start",
        "smp-qualification-finish",
        "smp-qualification-complete",
    ] {
        assert_eq!(
            milestone_output_class(name),
            MilestoneOutputClass::QualificationCritical
        );
    }
    assert_eq!(
        milestone_output_class("kernel-scheduler-phase"),
        MilestoneOutputClass::Measurement
    );
    // A measurement gets one attempt; the classes the harness reads as
    // evidence keep the bounded retry.
    assert_eq!(
        milestone_output_class("kernel-scheduler-phase").output_attempts(),
        1
    );
    assert!(milestone_output_class("smp-qualification-ready").output_attempts() > 1);
    assert!(milestone_output_class("dvm-block-transport-revoked").output_attempts() > 1);
    assert_eq!(
        milestone_output_class("proc-prepare-published").output_attempts(),
        1
    );
    assert_eq!(
        milestone_output_class("smp-qualification-ready-extra"),
        MilestoneOutputClass::Required
    );
    assert_eq!(
        milestone_output_class("dvm-block-transport-revoked"),
        MilestoneOutputClass::Required
    );
}

#[test]
fn qualification_loss_snapshot_isolated_and_fail_closed() {
    assert_eq!(
        milestone_loss_snapshot(MilestoneOutputClass::QualificationCritical, 17, 19, 0, 0),
        (0, 0)
    );
    assert_eq!(
        milestone_loss_snapshot(MilestoneOutputClass::QualificationCritical, 17, 19, 3, 23),
        (3, 23)
    );
}

#[test]
fn qualification_drop_is_visible_only_to_following_critical_evidence() {
    let global = AtomicU64::new(17);
    let critical = AtomicU64::new(19);
    let discarded = AtomicU64::new(23);

    record_milestone_output_drop_to(
        MilestoneOutputClass::QualificationCritical,
        29,
        &global,
        &critical,
        &discarded,
    );

    assert_eq!(global.load(Ordering::Relaxed), 18);
    assert_eq!(critical.load(Ordering::Relaxed), 20);
    assert_eq!(discarded.load(Ordering::Relaxed), 52);
}
