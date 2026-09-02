// SPDX-License-Identifier: MIT
//
// Smoke-lane option admission witnesses.
//
// These pin what `parse_smoke_options` accepts and refuses, plus the two
// lane-level controls that decide *what* gets booted and *how many times*.
// They are separated from marker acceptance and SMP ring3 qualification so a
// change to one group cannot quietly rewrite the evidence for another.
//
// Included into `kvm::tests`, so every witness keeps its stable libtest path.

    #[test]
    fn smoke_timeout_is_bounded() {
        let options =
            parse_smoke_options(vec!["--timeout".into(), "30".into()].into_iter()).unwrap();
        assert_eq!(options.timeout.as_secs(), 30);
        assert_eq!(
            options.expected_markers,
            vec![
                RUSTOS_BOOT_MARKER.to_owned(),
                RUSTOS_INIT_IDENTITY_MARKER.to_owned(),
                RUSTOS_POST_INIT_PROVENANCE_MARKER.to_owned(),
                RUSTOS_GPU_SCENE_COMPILER_MARKER.to_owned()
            ]
        );
        assert_eq!(
            options.expected_dvm_markers,
            vec![DVM_GPU_COMPOSITOR_MARKER.to_owned()]
        );
        let extra =
            parse_smoke_options(vec!["--expect-dvm".into(), "gpu-extra".into()].into_iter())
                .unwrap();
        assert!(extra.expected_dvm_markers.contains(&"gpu-extra".to_owned()));
        assert!(
            parse_smoke_options(
                vec!["--timeout".into(), MAX_SMOKE_TIMEOUT.to_string()].into_iter()
            )
            .is_ok()
        );
        assert!(
            parse_smoke_options(
                vec!["--timeout".into(), (MAX_SMOKE_TIMEOUT + 1).to_string()].into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn interactive_multicore_run_seals_formal_evidence_before_claiming_resources() {
        smp_ring3_qualification::interactive_multicore_run_seals_formal_evidence_before_claiming_resources();
    }

    #[test]
    fn smp_iteration_is_bounded_and_cannot_claim_acceptance() {
        smp_ring3_qualification::smp_iteration_is_bounded_and_cannot_claim_acceptance();
    }

    #[test]
    fn smp_ring3_qualification_has_exact_private_kvm_admission() {
        smp_ring3_qualification::smp_ring3_qualification_has_exact_private_kvm_admission();
    }

    #[test]
    fn smp_evidence_cohort_is_strict_and_paired() {
        smp_ring3_qualification::smp_evidence_cohort_is_strict_and_paired();
    }

    #[test]
    fn ipcbench_probe_option_is_a_strict_singular_name() {
        smp_ring3_qualification::ipcbench_probe_option_is_a_strict_singular_name();
    }

    #[test]
    fn input_exercise_requires_both_ring3_ingress_markers() {
        let options = parse_smoke_options(vec!["--exercise-input".into()].into_iter()).unwrap();
        assert!(options.exercise_input);
        assert!(
            options
                .expected_markers
                .contains(&DVM_KEYBOARD_INGRESS_MARKER.to_owned())
        );
        assert!(
            options
                .expected_markers
                .contains(&DVM_POINTER_INGRESS_MARKER.to_owned())
        );
    }

    #[test]
    fn smoke_readiness_budget_starts_only_after_both_guests_spawn() {
        smp_ring3_qualification::smoke_readiness_budget_starts_only_after_both_guests_spawn();
    }

    /// The smoke lane must boot the tree it was asked about.
    ///
    /// `kvm-smoke` used to launch whatever image happened to be on disk, so an
    /// edit followed by a smoke run silently answered a question about the
    /// previous build. Entire investigations were recorded against hypotheses
    /// that were never actually booted. The refresh has to happen before
    /// `prepare_layout` copies the boot disk, or the copy is the stale one.
    #[test]
    fn a_smoke_run_refreshes_the_boot_image_before_it_copies_it() {
        let source = include_str!("../options.rs");
        let smoke_start = source
            .find("pub(crate) fn kvm_smoke_command")
            .expect("bounded smoke command");
        let interactive_start = source
            .find("pub(crate) fn kvm_run_command")
            .expect("interactive KVM command");
        let smoke = &source[smoke_start..interactive_start];
        let build = smoke
            .find("crate::build::build(config, false)?")
            .expect("smoke lane refreshes the boot image");
        let layout = smoke
            .find("let layout = prepare_layout(config, &options)?;")
            .expect("smoke lane layout");
        assert!(build < layout);

        let default_options = parse_smoke_options(Vec::new().into_iter()).unwrap();
        assert!(default_options.build_image);
        let opted_out = parse_smoke_options(vec!["--no-build".into()].into_iter()).unwrap();
        assert!(!opted_out.build_image);
    }

    /// A pass rate needs more than one sample, and the loop belongs here.
    ///
    /// Repeating the lane by hand in a shell loses each failing run's debugcon
    /// log to the next run's truncation, which is exactly the evidence a rare
    /// defect leaves behind.
    #[test]
    fn repeat_is_bounded_and_defaults_to_a_single_run() {
        assert_eq!(
            parse_smoke_options(Vec::new().into_iter()).unwrap().repeat,
            1
        );
        assert_eq!(
            parse_smoke_options(vec!["--repeat".into(), "6".into()].into_iter())
                .unwrap()
                .repeat,
            6
        );
        for rejected in ["0", "65", "many", "-1"] {
            assert!(
                parse_smoke_options(vec!["--repeat".into(), rejected.into()].into_iter()).is_err(),
                "--repeat accepted {rejected}"
            );
        }
        assert!(parse_smoke_options(vec!["--repeat".into()].into_iter()).is_err());
    }
