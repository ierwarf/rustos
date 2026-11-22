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
        let default = parse_smoke_options(Vec::<String>::new().into_iter()).unwrap();
        assert_eq!(default.timeout.as_secs(), DEFAULT_SMOKE_TIMEOUT);
        // The isolated multi-vCPU bench passes this explicit bound to the
        // very same parser. Rejecting it prevents the guest from booting.
        assert_eq!(MAX_SMOKE_TIMEOUT, 180);
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
    fn smoke_option_values_cannot_swallow_topology_flags() {
        for option in ["--expect", "--expect-dvm", "--gpu-firmware", "--physical-usb"] {
            let error = parse_smoke_options(
                [option, "--storage-dvm-only"].map(str::to_owned).into_iter(),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), format!("missing value for {option}"));
        }
        let options = parse_smoke_options(
            ["--expect", "-single-dash-marker", "--expect-dvm", "marker --detail"]
                .map(str::to_owned)
                .into_iter(),
        )
        .unwrap();
        assert!(options.expected_markers.contains(&"-single-dash-marker".to_owned()));
        assert!(options.expected_dvm_markers.contains(&"marker --detail".to_owned()));
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

    /// The interactive lane must reject an unbound or non-xHCI controller before
    /// it starts the doorbells or spawns either guest.
    #[test]
    fn interactive_run_validates_the_physical_usb_controller_before_spawning() {
        let source = include_str!("../options.rs");
        let interactive = &source[source
            .find("pub(crate) fn kvm_run_command")
            .expect("interactive KVM command")..];
        let validate = interactive
            .find("validate_physical_usb_inputs(&options)?;")
            .expect("interactive physical USB validation");
        let doorbell = interactive
            .find("start_dvm_input_doorbell(&layout)?")
            .expect("interactive input doorbell");
        let spawn = interactive
            .find("let (mut rustos, mut dvm) = spawn_guests(")
            .expect("interactive guest spawn");
        assert!(validate < doorbell && doorbell < spawn);
    }

    /// Only the operator-owned interactive session scales the GTK window to
    /// the host screen. Smoke and the FPS proof keep the exact 1:1 mode their
    /// evidence was written against.
    #[test]
    fn only_the_interactive_session_fits_the_window_to_the_host() {
        let smoke = parse_smoke_options(Vec::new().into_iter()).unwrap();
        assert!(!smoke.fit_host_window);
        let fps =
            parse_smoke_options(vec!["--min-ui-fps".into(), "60".into()].into_iter()).unwrap();
        assert!(!fps.fit_host_window);

        let source = include_str!("../options.rs");
        let interactive = &source[source
            .find("pub(crate) fn kvm_run_command")
            .expect("interactive KVM command")..];
        assert!(interactive.contains("        fit_host_window,\n"));
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

    fn parse(args: &[&str]) -> anyhow::Result<super::SmokeOptions> {
        parse_smoke_options(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn hold_seconds_is_bounded_and_implies_the_display_provider() {
        let options = parse(&["--hold-seconds", "45"]).unwrap();
        assert_eq!(options.hold_seconds, Some(45));
        assert!(options.gui_dvm_surfaces && options.dvm_block_shmem);
        assert_eq!(parse(&["--hold-seconds", "1"]).unwrap().hold_seconds, Some(1));
        assert_eq!(
            parse(&["--hold-seconds", "180"]).unwrap().hold_seconds,
            Some(super::MAX_SMOKE_HOLD_SECONDS)
        );
        for bad in ["0", "181", "-1", "x", ""] {
            assert!(parse(&["--hold-seconds", bad]).is_err(), "{bad:?}");
        }
        let range = parse(&["--hold-seconds", "181"]).unwrap_err().to_string();
        assert!(range.contains("--hold-seconds") && range.contains("1..=180"), "{range}");
        assert!(parse(&["--hold-seconds"]).is_err());
        assert!(parse(&["--hold-seconds", "5", "--hold-seconds", "6"]).is_err());
        assert!(parse(&[]).unwrap().hold_seconds.is_none());
    }

    #[test]
    fn hold_seconds_refuses_what_it_contradicts() {
        for other in [
            vec!["--storage-dvm-only"],
            vec!["--recovery-probe", "all"],
            vec!["--min-ui-fps", "30"],
            vec!["--smp-iteration", "--timeout", "30"],
        ] {
            let mut args = vec!["--hold-seconds", "10"];
            args.extend(other.iter().copied());
            let err = parse(&args).unwrap_err().to_string();
            assert!(err.contains("--hold-seconds"), "{args:?}: {err}");
        }
    }

    #[test]
    fn launch_desktop_is_strict_and_requires_its_marker() {
        let options = parse(&["--launch-desktop", "chrome.desktop"]).unwrap();
        assert_eq!(options.launch_desktop.as_deref(), Some("chrome.desktop"));
        assert!(options.gui_dvm_surfaces && options.dvm_block_shmem);
        assert!(options
            .expected_markers
            .contains(&super::launch_desktop_launched_marker("chrome.desktop")));
        for bad in ["chrome", "../x.desktop", "a/b.desktop", ".desktop", "a b.desktop"] {
            assert!(parse(&["--launch-desktop", bad]).is_err(), "{bad:?}");
        }
        assert!(parse(&["--launch-desktop"]).is_err());
        assert!(parse(&["--launch-desktop", "a.desktop", "--launch-desktop", "b.desktop"]).is_err());
        assert!(parse(&["--launch-desktop", "a.desktop", "--storage-dvm-only"]).is_err());
        assert!(parse(&["--launch-desktop", "a.desktop", "--smp-iteration", "--timeout", "30"]).is_err());
        assert!(parse(&[]).unwrap().launch_desktop.is_none());
    }

    #[test]
    fn numeric_flag_errors_name_the_valid_range() {
        for (flag, range) in [("--timeout", "1..=180"), ("--repeat", "1..=64"), ("--min-ui-fps", "1..=240")] {
            let err = format!("{:#}", parse(&[flag, "abc"]).unwrap_err());
            assert!(err.contains(flag) && err.contains(range), "{flag}: {err}");
        }
    }

    #[test]
    fn display_failure_without_a_display_provider_names_the_topology() {
        let detail = String::from("uiserver readiness failed: dependency=display errno=11 gen=120 (display=failed#120)");
        let hinted = super::with_display_topology_hint(detail.clone(), false);
        assert!(hinted.starts_with(&detail) && hinted.contains("--gui-dvm-surfaces"), "{hinted}");
        assert_eq!(super::with_display_topology_hint(detail.clone(), true), detail);
        let other = String::from("uiserver readiness failed: dependency=input errno=5");
        assert_eq!(super::with_display_topology_hint(other.clone(), false), other);
    }

    #[test]
    fn held_state_fails_on_any_regression_and_passes_when_clean() {
        let options = parse(&["--hold-seconds", "10"]).unwrap();
        let mut rustos = options.expected_markers.join("\n");
        rustos.push('\n');
        let mut dvm = options.expected_dvm_markers.join("\n");
        dvm.push('\n');
        // Clean logs hold only when the display relay is also healthy; the
        // DVM log here carries only the expected markers, which is not a
        // display failure.
        assert_eq!(super::held_state_failure(&rustos, &dvm, &options), None);
        let lost = super::held_state_failure(&rustos.replace(RUSTOS_BOOT_MARKER, ""), &dvm, &options);
        assert!(lost.is_some_and(|reason: String| reason.contains("disappeared")));
        let refused = format!("{rustos}runtimed: kvm launch-desktop FAIL desktop_id=x.desktop errno=2\n");
        assert!(super::held_state_failure(&refused, &dvm, &options).is_some_and(|r: String| r.contains("errno=2")));
        let panicked = format!("{rustos}initd: fatal service endpoint not ready exec=a pid=1 errno=5\n");
        assert!(super::held_state_failure(&panicked, &dvm, &options).is_some());
    }

    #[test]
    fn screendump_and_chrome_web_probe_options_enforce_topology() {
        let sd = parse(&["--screendump", "build/kvm/test.ppm"]).unwrap();
        assert_eq!(sd.screendump.as_deref(), Some(Path::new("build/kvm/test.ppm")));
        assert!(sd.gui_dvm_surfaces);
        assert!(parse(&["--screendump", "a.ppm", "--physical-gpu", "0000:03:00.0"]).is_err());
        assert!(parse(&["--screendump", "a.ppm", "--storage-dvm-only"]).is_err());

        let probe = parse(&["--chrome-web-probe"]).unwrap();
        assert!(probe.chrome_web_probe);
        assert!(probe.gui_dvm_surfaces && probe.dvm_block_shmem && probe.dvm_network_shmem);
        assert_eq!(probe.launch_desktop.as_deref(), Some("chrome.desktop"));
        assert!(probe.expected_markers.contains(&super::launch_desktop_launched_marker("chrome.desktop")));
        assert!(probe.expected_markers.iter().any(|m| m == "wayland_windows=2"));
        assert_eq!(probe.hold_seconds, Some(8));
        assert!(parse(&["--chrome-web-probe", "--storage-dvm-only"]).is_err());
    }
