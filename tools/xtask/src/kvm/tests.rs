// SPDX-License-Identifier: MIT

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_UI_FPS_ACTIVE_WINDOWS, DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_FLAG_DVM_READY,
        DVM_BLOCK_FLAG_READ_ONLY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_MEDIA_BLOCK_BYTES,
        DVM_BLOCK_MEDIA_FEATURES, DVM_BLOCK_READY_MARKER, DVM_BOOTSTRAP_FRAME_MARKER,
        DVM_CONTROL_AUTHENTICATION,
        DVM_CONTROL_CAPABILITIES, DVM_CONTROL_PROTOCOL, DVM_CONTROL_STATE, DVM_CONTROL_TRANSPORT,
        DVM_DISPLAY_REGION_BYTES, DVM_GPU_COMPOSITOR_MARKER, DVM_KEYBOARD_INGRESS_MARKER,
        DVM_POINTER_INGRESS_MARKER, DvmNetworkCounters, GuestDisplay, MAX_SMOKE_TIMEOUT,
        PHYSICAL_GPU_PROFILES, RUSTOS_BOOT_MARKER, RUSTOS_DVM_BLOCK_E2E_MARKER,
        RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER, RUSTOS_DVM_BLOCK_FLUSH_FAULT_MARKER,
        RUSTOS_DVM_BLOCK_MARKER, RUSTOS_GPU_SCENE_COMPILER_MARKER, RUSTOS_INIT_IDENTITY_MARKER,
        RUSTOS_POST_INIT_PROVENANCE_MARKER, RUSTOS_SMP_READINESS, RustosSmpReadiness,
        SMP_QUALIFICATION_DEADLINE_US, SMP_QUALIFICATION_WORK_BITS, SMP_QUALIFICATION_WORK_UNITS,
        VIRTUAL_GPU_EVIDENCE, WAYCLICK_FIRST_FRAME_MARKER, WayclickProfileObservation,
        acquire_kvm_launch_lock, append_dvm_display_pixels, append_dvm_input_devices,
        append_dvm_network_device, append_dvm_virtual_gpu, append_dvm_virtual_storage,
        append_physical_gpu, causal_tail, claim_physical_gpu_launch_in,
        dvm_block_header_matches_ready_generation, dvm_display_failure, dvm_display_provider_ready,
        dvm_display_relay_meets_fps, dvm_display_relay_ready, dvm_gpu_compositor_ready,
        dvm_gpu_device, dvm_machine, dvm_physical_frames_ready, dvm_pointer_device,
        dvm_read_only_block_header, guest_cid_for_process, is_sha256, mesa_dri_prime_for_pci_bdf,
        milestone_frame_checksum, parse_dvm_control_contract_text, parse_manifest_text,
        parse_smoke_options, parse_verified_milestone_frame,
        parse_verified_smp_qualification_event, parse_verified_smp_runtime_event,
        physical_gpu_profile, prepare_runtime_log, qemu_display_backend,
        render_ipcbench_probe_contract, render_private_acceptance_contract,
        render_smp_ring3_qualification_contract,
        required_dvm_gpu_ready, runtime_stall_or_crash_observed, rustos_marker_present,
        select_smoke_guest_display, smp_ring3_qualification_is_complete,
        smp_runtime_missing_markers, uiserver_has_interactive_slow_loop,
        uiserver_idle_ticks_healthy, uiserver_profile_input_pipeline_healthy,
        uiserver_profile_meets_fps, validate_manifest_values,
        validate_smp_ring3_qualification_events, validate_storage_fault_expectation,
        verified_smp_qualification_events, verified_smp_runtime_events, vfio_device_cdev_path,
        wayclick_profile_meets_fps, wayclick_profile_observation,
    };
    use std::{fs, path::Path, process::Command, time::Duration};

    #[path = "smp_ring3_qualification.rs"]
    mod smp_ring3_qualification;

    #[path = "dvm_block.rs"]
    mod dvm_block;

    #[test]
    fn rustos_smp_topology_is_machine_gated_on_complete_prerequisites() {
        smp_ring3_qualification::rustos_smp_topology_is_machine_gated_on_complete_prerequisites();
    }

    #[test]
    fn rustos_smp_runtime_requires_every_requested_cpu_event_class() {
        smp_ring3_qualification::rustos_smp_runtime_requires_every_requested_cpu_event_class();
    }

    #[test]
    fn smp_runtime_rejects_interleaved_or_route_only_substrings_as_evidence() {
        smp_ring3_qualification::smp_runtime_rejects_interleaved_or_route_only_substrings_as_evidence();
    }

    #[test]
    fn smp_ring3_qualification_accepts_complete_exact_worker_sets() {
        smp_ring3_qualification::smp_ring3_qualification_accepts_complete_exact_worker_sets();
    }

    #[test]
    fn smp_ring3_qualification_rejects_missing_duplicate_and_replayed_phases() {
        smp_ring3_qualification::smp_ring3_qualification_rejects_missing_duplicate_and_replayed_phases();
    }

    #[test]
    fn smp_ring3_qualification_rejects_process_and_thread_substitution() {
        smp_ring3_qualification::smp_ring3_qualification_rejects_process_and_thread_substitution();
    }

    #[test]
    fn smp_ring3_qualification_rejects_loss_wrong_cpu_and_work() {
        smp_ring3_qualification::smp_ring3_qualification_rejects_loss_wrong_cpu_and_work();
    }

    #[test]
    fn smp_ring3_qualification_rejects_zero_or_nonuniform_kernel_binding() {
        smp_ring3_qualification::smp_ring3_qualification_rejects_zero_or_nonuniform_kernel_binding(
        );
    }

    #[test]
    fn smp_ring3_qualification_rejects_phase_order_and_deadline() {
        smp_ring3_qualification::smp_ring3_qualification_rejects_phase_order_and_deadline();
    }

    #[test]
    fn smp_ring3_qualification_rejects_interleaved_tampered_and_plain_frames() {
        smp_ring3_qualification::smp_ring3_qualification_rejects_interleaved_tampered_and_plain_frames();
    }

    #[test]
    fn private_kvm_contract_renderers_are_canonical() {
        smp_ring3_qualification::private_kvm_contract_renderers_are_canonical();
    }

    #[test]
    fn smp_boot_acceptance_uses_kernel_stamped_milestones_when_text_interleaves() {
        smp_ring3_qualification::smp_boot_acceptance_uses_kernel_stamped_milestones_when_text_interleaves();
    }

    #[test]
    fn kvm_launch_lock_rejects_concurrent_log_and_cid_owners() {
        let root = tempfile::tempdir().unwrap();
        let _owner = acquire_kvm_launch_lock(root.path()).unwrap();
        assert!(acquire_kvm_launch_lock(root.path()).is_err());
    }

    #[test]
    fn kvm_guest_cid_is_process_scoped_and_never_reserved() {
        let first = guest_cid_for_process(100);
        let second = guest_cid_for_process(101);
        assert!(first >= 3);
        assert_ne!(first, u32::MAX);
        assert_ne!(first, second);
    }

    #[test]
    fn dry_run_log_preparation_preserves_existing_evidence() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("linux-dvm-serial.log");
        fs::write(&log, "physical-gpu-evidence\n").unwrap();

        prepare_runtime_log(&log, false).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "physical-gpu-evidence\n");

        prepare_runtime_log(&log, true).unwrap();
        assert!(fs::read_to_string(&log).unwrap().is_empty());
    }

    #[test]
    fn physical_vfio_cdev_path_is_unique_and_canonical() {
        let root = tempfile::tempdir().unwrap();
        let vfio_dev = root.path().join("vfio-dev");
        fs::create_dir(&vfio_dev).unwrap();
        fs::create_dir(vfio_dev.join("vfio7")).unwrap();
        assert_eq!(
            vfio_device_cdev_path(root.path()).unwrap(),
            Path::new("/dev/vfio/devices/vfio7")
        );

        fs::create_dir(vfio_dev.join("vfio8")).unwrap();
        assert!(vfio_device_cdev_path(root.path()).is_err());
    }

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

    #[test]
    fn interactive_ui_boot_uses_only_the_outer_smoke_timeout() {
        let parallel_source = include_str!("guest.rs");
        let interactive_source = include_str!("options.rs");
        assert!(!parallel_source.contains("guest_deadline_reached"));
        assert!(!parallel_source.contains("BOOT_TO_UI_HARD_LIMIT_MS"));
        assert!(!interactive_source.contains("guest_deadline_reached"));
        assert!(!interactive_source.contains("BOOT_TO_UI_HARD_LIMIT_MS"));
        assert!(interactive_source.contains("before the outer smoke timeout"));
    }

    #[test]
    fn dvm_display_mode_requires_the_observed_display_contract() {
        let options = parse_smoke_options(vec!["--gui-dvm-surfaces".into()].into_iter()).unwrap();
        assert!(options.gui_dvm_surfaces);
        assert!(options.dvm_block_shmem);
        assert!(
            options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_E2E_MARKER.to_owned())
        );
        assert!(
            options
                .expected_markers
                .contains(&WAYCLICK_FIRST_FRAME_MARKER.to_owned())
        );
        assert!(
            options
                .expected_dvm_markers
                .contains(&DVM_BOOTSTRAP_FRAME_MARKER.to_owned())
        );
        assert!(dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=7168 bpp=4 fmt=1 flags=0x6 gen=1\n\
             seq=9 msg=\"milestone name=product-display-ready arg0=0x1 arg1=0x1\""
        ));
        assert!(!dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=7168 bpp=4 fmt=1 flags=0x6 gen=1"
        ));
        assert!(!dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=7168 bpp=4 fmt=1 flags=0x4 gen=1\n\
             seq=9 msg=\"milestone name=product-display-ready arg0=0x1 arg1=0x1\""
        ));
        assert!(dvm_display_relay_ready(
            "rustos-dvm-display: peer readiness sent event=ivshmem-msix-uio\n\
             rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n\
             rustos-dvm-display: scheduler admitted policy=rr priority=9 rttime_soft_us=50000 rttime_hard_us=100000 rttime_hard_action=terminate\n\
             rustos-dvm-display: active width=1600 height=900 stride=7168 format=BGRA8888 event=ivshmem-msix-uio irq_count=1 source-path=staged-copy zero-copy=0 gpu-composition=1 explicit-fence=1 scanout_buffers=3 cpu-final-compose=0 staged-damage-copy=1",
            false,
        ));
        assert!(!dvm_display_relay_ready(
            "rustos-dvm-display: active width=1600 height=900 stride=7168 format=BGRA8888 dmabuf-direct-scanout",
            false,
        ));
        assert!(!dvm_display_relay_ready(
            "rustos-dvm-display: peer readiness sent event=ivshmem-msix-uio\n\
             rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n\
             rustos-dvm-display: active width=1600 height=900 stride=7168 format=BGRA8888 event=ivshmem-msix-uio irq_count=0 source-path=staged-copy zero-copy=0 gpu-composition=1 explicit-fence=1 scanout_buffers=3 cpu-final-compose=0 staged-damage-copy=1",
            false,
        ));
        assert!(dvm_display_relay_ready(
            "rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n\
             rustos-dvm-display: scheduler admitted policy=rr priority=9 rttime_soft_us=50000 rttime_hard_us=100000 rttime_hard_action=terminate\n\
             rustos-dvm-display: active width=1600 height=900 stride=7168 format=BGRA8888 event=ivshmem-msix-uio irq_count=1 source-path=dmabuf zero-copy=1 gpu-composition=1 explicit-fence=1 scanout_buffers=3 cpu-final-compose=0 staged-damage-copy=0",
            true,
        ));
        assert_eq!(
            dvm_display_failure(
                "boot\nStarting crond: rustos-dvm-display: GPU KMS setup unavailable stage=gpu-kms-target errno=6\n",
                false,
            ),
            Some("Starting crond: rustos-dvm-display: GPU KMS setup unavailable stage=gpu-kms-target errno=6".to_owned())
        );
        assert_eq!(dvm_display_failure("boot\nrelay pending\n", false), None);
        assert!(dvm_display_failure(
            "rustos-dvm-display: gpu-compositor offline frames=1 stage=gpu-dmabuf-acquire gpu-stage=gpu-batch-validate errno=2\n",
            true,
        )
        .is_some());
        assert_eq!(
            dvm_display_failure(
                "rustos-dvm-gpu: evidence publish failed errno=75\n",
                false,
            ),
            Some(
                "Linux DVM GPU evidence publication failed detail=rustos-dvm-gpu: evidence publish failed errno=75"
                    .to_owned()
            )
        );
        assert_eq!(
            dvm_display_failure(
                "amdgpu: PSP create ring failed!\namdgpu: Fatal error during GPU init\n",
                true,
            ),
            Some("physical GPU kernel probe failed stage=device-security-processor; the assigned device did not enter a reusable post-reset state".to_owned())
        );
    }

    #[test]
    fn physical_gpu_smoke_requires_one_complete_pool_reuse() {
        let frames = "rustos-dvm-display: gpu-frame sequence=7 submit=11 output=0 render_us=3000 source-path=dmabuf zero-copy=1 gpu-fence=1 present-fence=1\n\
                      rustos-dvm-display: gpu-frame sequence=8 submit=12 output=1 render_us=3100 source-path=dmabuf zero-copy=1 gpu-fence=1 present-fence=1\n\
                      rustos-dvm-display: gpu-frame sequence=9 submit=13 output=2 render_us=3200 source-path=dmabuf zero-copy=1 gpu-fence=1 present-fence=1\n\
                      rustos-dvm-display: gpu-frame sequence=10 submit=14 output=0 render_us=3300 source-path=dmabuf zero-copy=1 gpu-fence=1 present-fence=1";
        assert!(dvm_physical_frames_ready(frames));
        assert!(!dvm_physical_frames_ready(
            &frames.lines().take(3).collect::<Vec<_>>().join("\n")
        ));
        assert!(!dvm_physical_frames_ready(
            &frames.replace("sequence=9", "sequence=10")
        ));
        assert!(!dvm_physical_frames_ready(
            &frames.replace("present-fence=1", "present-fence=0")
        ));
    }

    #[test]
    fn dvm_gpu_compositor_requires_real_virgl_fences_and_bounded_latency() {
        let ready = "rustos-dvm-gpu: ready contract=1 driver=virtio_gpu renderer=virgl_(AMD_Radeon_780M) backend-class=virtual-staged certification=registered commands=3 gpu-fence=1 acquire-fence=1 prime_us=12000 frames=120 fps_milli=60001 avg_us=400 max_us=900 wall_max_us=1000 frame_hash_a=ac8906df9029660b frame_hash_b=bc8906df9029660b hash-stable=1 hash-dynamic=1 negative=5 software=0 scheduler=rr priority=8 rttime-soft-us=50000 rttime-hard-us=100000 rttime-hard-action=terminate scheduler-restored=normal performance-target=1 scope-public-abi=0 scope-ui-connected=0 scope-scanout=0\nrustos-dvm-gpu: health sequence=1 completion_us=900 acquire-fence=1\nrustos-dvm-gpu: health sequence=2 completion_us=900 acquire-fence=1\nrustos-dvm-gpu: health sequence=3 completion_us=900 acquire-fence=1";
        assert!(dvm_gpu_compositor_ready(ready, VIRTUAL_GPU_EVIDENCE));
        let recovered = format!(
            "{}\nrustos-dvm-gpu: health sequence=1 completion_us=20000 acquire-fence=1\nrustos-dvm-gpu: health sequence=2 completion_us=900 acquire-fence=1\nrustos-dvm-gpu: health sequence=3 completion_us=900 acquire-fence=1\nrustos-dvm-gpu: health sequence=4 completion_us=900 acquire-fence=1",
            ready.lines().next().unwrap()
        );
        assert!(dvm_gpu_compositor_ready(&recovered, VIRTUAL_GPU_EVIDENCE));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("software=0", "software=1"),
            VIRTUAL_GPU_EVIDENCE,
        ));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("scheduler-restored=normal", "scheduler-restored=rr"),
            VIRTUAL_GPU_EVIDENCE,
        ));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("rttime-hard-action=terminate", "rttime-hard-action=ignore"),
            VIRTUAL_GPU_EVIDENCE
        ));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("performance-target=1", "performance-target=0"),
            VIRTUAL_GPU_EVIDENCE,
        ));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("max_us=900", "max_us=16668"),
            VIRTUAL_GPU_EVIDENCE,
        ));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("wall_max_us=1000", "wall_max_us=16668"),
            VIRTUAL_GPU_EVIDENCE,
        ));
        assert!(!dvm_gpu_compositor_ready(
            &ready.replace("prime_us=12000", "prime_us=500001"),
            VIRTUAL_GPU_EVIDENCE,
        ));
        assert!(!dvm_gpu_compositor_ready(
            &format!("{ready}\nrustos-dvm-gpu: context lost errno=5"),
            VIRTUAL_GPU_EVIDENCE
        ));

        let physical = ready
            .replace("driver=virtio_gpu", "driver=amdgpu")
            .replace(
                "renderer=virgl_(AMD_Radeon_780M)",
                "renderer=AMD_Radeon_780M",
            )
            .replace(
                "backend-class=virtual-staged",
                "backend-class=physical-direct",
            );
        let physical_expected = super::GpuEvidenceExpectation {
            drm_driver: "amdgpu",
            backend_class: "physical-direct",
        };
        assert!(dvm_gpu_compositor_ready(&physical, physical_expected));
        assert!(!dvm_gpu_compositor_ready(&physical, VIRTUAL_GPU_EVIDENCE));
    }

    #[test]
    fn dvm_network_mode_is_explicit() {
        let options = parse_smoke_options(vec!["--dvm-network-shmem".into()].into_iter()).unwrap();
        assert!(options.dvm_network_shmem);
        let exercised = parse_smoke_options(
            vec![
                "--gui-dvm-surfaces".into(),
                "--dvm-network-shmem".into(),
                "--exercise-network".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert!(exercised.exercise_network);
        assert!(
            parse_smoke_options(
                vec!["--dvm-network-shmem".into(), "--exercise-network".into()].into_iter()
            )
            .is_err()
        );
        assert!(parse_smoke_options(vec!["--exercise-network".into()].into_iter()).is_err());
    }

    #[test]
    fn dvm_block_mode_requires_both_peer_readiness_markers() {
        let options = parse_smoke_options(vec!["--dvm-block-shmem".into()].into_iter()).unwrap();
        assert!(options.dvm_block_shmem);
        assert!(
            options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_MARKER.to_owned())
        );
        assert!(
            options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER.to_owned())
        );
        assert!(
            options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_E2E_MARKER.to_owned())
        );
        assert!(
            options
                .expected_dvm_markers
                .contains(&DVM_BLOCK_READY_MARKER.to_owned())
        );
    }

    #[test]
    fn storage_only_gate_is_independent_of_gpu_and_enables_block_proof() {
        let options = parse_smoke_options(vec!["--storage-dvm-only".into()].into_iter()).unwrap();
        assert!(options.storage_only);
        assert!(options.dvm_block_shmem);
        assert!(
            !options
                .expected_markers
                .contains(&RUSTOS_GPU_SCENE_COMPILER_MARKER.to_owned())
        );
        assert!(
            !options
                .expected_dvm_markers
                .contains(&DVM_GPU_COMPOSITOR_MARKER.to_owned())
        );
        assert!(
            options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_E2E_MARKER.to_owned())
        );
        assert!(required_dvm_gpu_ready(&options, "", VIRTUAL_GPU_EVIDENCE));
        assert!(
            parse_smoke_options(
                vec!["--storage-dvm-only".into(), "--gui-dvm-surfaces".into()].into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn storage_flush_fault_gate_requires_one_exact_fail_rule_and_rejects_success() {
        assert!(
            parse_smoke_options(vec!["--storage-dvm-expect-flush-fault".into()].into_iter())
                .is_err()
        );
        let options = parse_smoke_options(
            vec![
                "--storage-dvm-only".into(),
                "--storage-dvm-expect-flush-fault".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert!(options.expect_block_flush_fault);
        assert!(
            options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_FLUSH_FAULT_MARKER.to_owned())
        );
        assert!(
            !options
                .expected_markers
                .contains(&RUSTOS_DVM_BLOCK_E2E_MARKER.to_owned())
        );
        assert!(
            validate_storage_fault_expectation(true, &["block.flush=fail".into()], &options)
                .is_ok()
        );
        assert!(
            validate_storage_fault_expectation(true, &[" block.flush = fail ".into()], &options)
                .is_ok()
        );
        assert!(validate_storage_fault_expectation(false, &[], &options).is_err());
        assert!(
            validate_storage_fault_expectation(
                true,
                &["block.flush=fail-after:1".into()],
                &options
            )
            .is_err()
        );
        assert!(
            validate_storage_fault_expectation(
                true,
                &["block.flush=fail".into(), "block.flush=fail".into()],
                &options
            )
            .is_err()
        );
        assert!(
            validate_storage_fault_expectation(
                true,
                &["block.flush=fail".into(), "block.flush=off".into()],
                &options
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_probe_requires_fresh_rustos_and_dvm_process_epochs() {
        let options =
            parse_smoke_options(vec!["--recovery-probe".into(), "all".into()].into_iter()).unwrap();
        assert!(options.recovery_probe.includes_rustos_reboot());
        assert!(options.recovery_probe.includes_dvm_restart());
        assert!(
            parse_smoke_options(
                vec![
                    "--recovery-probe".into(),
                    "rustos-reboot".into(),
                    "--min-ui-fps".into(),
                    "55".into(),
                ]
                .into_iter(),
            )
            .is_err()
        );
        assert!(
            parse_smoke_options(
                vec![
                    "--recovery-probe".into(),
                    "dvm-restart".into(),
                    "--storage-dvm-only".into(),
                ]
                .into_iter(),
            )
            .is_err()
        );
        let source = include_str!("guest.rs");
        let stop = source
            .find("stop_guest(rustos);")
            .expect("fresh reboot stops the old RustOS process");
        let spawn = source
            .find("*rustos = spawn_rustos_guest(")
            .expect("fresh reboot launches a new RustOS process");
        let archive = source
            .find("archive_recovery_log(&self.layout.debugcon_log)?;")
            .expect("fresh reboot archives its predecessor capture");
        assert!(stop < spawn);
        assert!(stop < archive && archive < spawn);
    }

    #[test]
    fn dvm_network_counters_are_bounded_and_bidirectional() {
        let observed = DvmNetworkCounters {
            tx_producer: 2,
            tx_consumer: 2,
            rx_producer: 3,
            rx_consumer: 2,
            dvm_ready: true,
        };
        assert!(observed.is_valid(64));
        assert!(observed.round_trip_observed());
        assert!(
            !DvmNetworkCounters {
                tx_producer: 65,
                tx_consumer: 0,
                rx_producer: 0,
                rx_consumer: 0,
                dvm_ready: false,
            }
            .is_valid(64)
        );
    }

    #[test]
    fn ui_fps_option_requires_sustained_high_volume_input_rate() {
        let options =
            parse_smoke_options(vec!["--min-ui-fps".into(), "20".into()].into_iter()).unwrap();
        assert_eq!(options.min_ui_fps, Some(20));
        assert_eq!(options.ui_proof_windows, DEFAULT_UI_FPS_ACTIVE_WINDOWS);
        assert!(options.exercise_input);
        assert!(options.dvm_block_shmem);
        assert!(
            options
                .expected_markers
                .iter()
                .any(|marker| marker == DVM_POINTER_INGRESS_MARKER)
        );
        let soak = parse_smoke_options(
            vec![
                "--min-ui-fps".into(),
                "60".into(),
                "--ui-proof-windows".into(),
                "15".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(soak.ui_proof_windows, 15);
        let minute_soak = parse_smoke_options(
            vec![
                "--min-ui-fps".into(),
                "55".into(),
                "--ui-proof-windows".into(),
                "60".into(),
                "--timeout".into(),
                "90".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(minute_soak.ui_proof_windows, 60);
        assert_eq!(minute_soak.timeout, Duration::from_secs(90));
        assert!(
            parse_smoke_options(vec!["--ui-proof-windows".into(), "15".into()].into_iter())
                .is_err()
        );
        assert!(
            parse_smoke_options(
                vec![
                    "--min-ui-fps".into(),
                    "55".into(),
                    "--ui-proof-windows".into(),
                    "61".into(),
                ]
                .into_iter(),
            )
            .is_err()
        );
        assert!(
            parse_smoke_options(
                vec!["--timeout".into(), (MAX_SMOKE_TIMEOUT + 1).to_string()].into_iter(),
            )
            .is_err()
        );
        assert!(
            parse_smoke_options(vec!["--min-ui-fps".into(), "241".into()].into_iter()).is_err()
        );
        assert!(uiserver_profile_meets_fps(
            "[INFO ] service=uiserver uiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20\nuiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20\nuiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20",
            20,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!uiserver_profile_meets_fps(
            "uiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20\nuiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20\nuiserver profile: elapsed_ms=1000 frame_hz_milli=19999 input_events=192 full=0 part=19",
            20,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(uiserver_profile_meets_fps(
            "uiserver profile: elapsed_ms=1000 frame_hz_milli=19999 input_events=192 full=0 part=19\nuiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20\nuiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20\nuiserver profile: elapsed_ms=1000 frame_hz_milli=20000 input_events=192 full=0 part=20",
            20,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        let wayclick = "wayclick profile: elapsed_ms=1000 commit_hz_milli=60000 callback_hz_milli=60000 redraw_requests=60 pointer_updates=0 commits=60 callbacks=60 buffer_releases=60 max_callback_gap_ms=18 callback_in_flight=1 redraw_pending=0\n".repeat(3);
        assert!(wayclick_profile_meets_fps(
            &wayclick,
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!wayclick_profile_meets_fps(
            &wayclick.replacen("callback_hz_milli=60000", "callback_hz_milli=1000", 1),
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!wayclick_profile_meets_fps(
            &wayclick.replace("max_callback_gap_ms=18", "max_callback_gap_ms=51"),
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        let phase_shifted = concat!(
            "wayclick profile: elapsed_ms=1007 commit_hz_milli=67472 callback_hz_milli=67472 commits=68 callbacks=68 buffer_releases=68 max_callback_gap_ms=40\n",
            "wayclick profile: elapsed_ms=1026 commit_hz_milli=53587 callback_hz_milli=53587 commits=55 callbacks=55 buffer_releases=55 max_callback_gap_ms=45\n",
            "wayclick profile: elapsed_ms=1044 commit_hz_milli=54549 callback_hz_milli=54549 commits=57 callbacks=57 buffer_releases=57 max_callback_gap_ms=50"
        );
        assert!(wayclick_profile_meets_fps(
            phase_shifted,
            55,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!wayclick_profile_meets_fps(
            &phase_shifted.replace("max_callback_gap_ms=50", "max_callback_gap_ms=51"),
            55,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert_eq!(
            wayclick_profile_observation(&wayclick),
            Some(WayclickProfileObservation {
                windows: 3,
                commit_hz_milli_min: 60_000,
                commit_hz_milli_max: 60_000,
                callback_hz_milli_min: 60_000,
                callback_hz_milli_max: 60_000,
                max_callback_gap_ms: 18,
                max_redraw_ms: 0,
            })
        );
        assert!(dvm_display_relay_meets_fps(
            "rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60001 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60",
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!dvm_display_relay_meets_fps(
            "rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=59999 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60",
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!dvm_display_relay_meets_fps(
            "rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=1 atomic_commit_us_avg=5000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=11000 gpu_fence_completions=60 present_fence_completions=60\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_completions=60 relay_cpu_copy_us_avg=0 atomic_commit_us_avg=1000 gpu_render_us_avg=9000 gpu_render_us_max=16668 gpu_fence_completions=60 present_fence_completions=60",
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
    }

    #[test]
    fn ui_fps_gate_accepts_post_present_production_heartbeats() {
        let window = |cursor: &str| {
            format!(
                "uiserver: update tick elapsed_ms=1000 loops=200 total_loops=200 frames=60 cursor_moves=60 cursor={cursor} presented_cursor={cursor} backlog=false backlog_loops=0 input_loop_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 background_thread_demotions=7"
            )
        };
        let log = [window("800,450"), window("992,450"), window("992,642")].join("\n");
        assert!(uiserver_profile_meets_fps(&log, 55, 3));
        assert!(uiserver_profile_input_pipeline_healthy(&log, 3, Some(55)));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &log.replace("presented_cursor=992,642", "presented_cursor=991,642"),
            3,
            Some(55)
        ));
    }

    #[test]
    fn ui_input_gate_uses_exact_rolling_rates_without_hiding_a_stall() {
        let log = [
            "uiserver: update tick elapsed_ms=1000 frames=61 cursor_moves=49 cursor=800,555 presented_cursor=800,555 backlog=false input_loop_events=58 input_gap_ms=50 input_last_age_ms=9 input_drops_window=0 input_slow_window=0 input_errors_window=0 background_thread_demotions=10",
            "uiserver: update tick elapsed_ms=1000 frames=61 cursor_moves=49 cursor=893,450 presented_cursor=893,450 backlog=false input_loop_events=58 input_gap_ms=50 input_last_age_ms=9 input_drops_window=0 input_slow_window=0 input_errors_window=0 background_thread_demotions=10",
            "uiserver: update tick elapsed_ms=1007 frames=65 cursor_moves=55 cursor=992,555 presented_cursor=992,555 backlog=false input_loop_events=66 input_gap_ms=34 input_last_age_ms=1 input_drops_window=0 input_slow_window=0 input_errors_window=0 background_thread_demotions=10",
        ]
        .join("\n");
        assert!(uiserver_profile_input_pipeline_healthy(&log, 3, Some(55)));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &log.replacen("input_gap_ms=50", "input_gap_ms=51", 1),
            3,
            Some(55)
        ));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &log.replacen("cursor_moves=49", "cursor_moves=39", 1),
            3,
            Some(55)
        ));
    }

    #[test]
    fn ui_runtime_health_rejects_allocator_and_core_service_failure_markers() {
        let healthy = "uiserver profile: elapsed_ms=1000 frame_hz_milli=60000 input_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor=800,450 presented_cursor=800,450 background_thread_demotions=7 backlog=0 cursor_moves=60\n\
uiserver profile: elapsed_ms=1000 frame_hz_milli=60000 input_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor=992,450 presented_cursor=992,450 background_thread_demotions=7 backlog=0 cursor_moves=60\n\
uiserver profile: elapsed_ms=1000 frame_hz_milli=60000 input_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor=992,642 presented_cursor=992,642 background_thread_demotions=7 backlog=0 cursor_moves=60";
        assert!(uiserver_profile_input_pipeline_healthy(
            healthy,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
            Some(60),
        ));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &healthy.replace("cursor=992,642", "cursor=803,452"),
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
            Some(60),
        ));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &healthy.replace("input_drops=0", "input_drops=1"),
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
            Some(60),
        ));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &healthy.replacen("frame_hz_milli=60000", "frame_hz_milli=59999", 1),
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
            Some(60),
        ));
        assert!(runtime_stall_or_crash_observed(
            "scheduler long ready wait: task=uiserver elapsed_ms=500"
        ));
        assert!(runtime_stall_or_crash_observed(
            "[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1200 (command 0x105)"
        ));
        assert!(runtime_stall_or_crash_observed(
            "memory allocation of 1664000 bytes failed"
        ));
        assert!(runtime_stall_or_crash_observed(
            "initd: fatal service endpoint not ready exec=services/runtimed/runtimed.elf"
        ));
        assert!(!runtime_stall_or_crash_observed(
            "uiserver: panic hook installed"
        ));
    }

    #[test]
    fn failure_evidence_keeps_only_bounded_causal_records() {
        let rustos = "unrelated payload\n\
milestone: name=product-root-core-ready pid=1 tid=1 ts_us=100\n\
vfsd: volume-read begin path=apps/wayclick/wayclick.elf ts_us=200";
        let dvm = "rustos-dvm-block: ready abi=2 generation=1 ts_us=150\nsecret payload";
        let events = causal_tail(rustos, dvm);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].guest_ts_us, Some(100));
        assert_eq!(events[1].guest_ts_us, Some(150));
        assert_eq!(events[2].guest_ts_us, Some(200));
        assert!(
            events.iter().all(
                |event| !event.record.contains("unrelated") && !event.record.contains("secret")
            )
        );
    }

    #[test]
    fn interactive_acceptance_requires_healthy_idle_ticks() {
        let healthy = "uiserver: update tick backlog=false input_drops=0 input_slow=0 input_errors=0\n\
uiserver: update tick backlog=false input_drops=0 input_slow=0 input_errors=0\n\
uiserver: update tick backlog=false input_drops=0 input_slow=0 input_errors=0";
        assert!(uiserver_idle_ticks_healthy(healthy, 3));
        assert!(!uiserver_idle_ticks_healthy(
            &healthy.replace("input_errors=0", "input_errors=1"),
            3,
        ));
    }

    #[test]
    fn ui_fps_gate_separates_bounded_topology_startup_from_steady_frames() {
        assert!(!uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=72 wayland_ms=70 console_windows=0 wayland_windows=0"
        ));
        assert!(!uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=72 wayland_ms=70 console_windows=1 wayland_windows=0"
        ));
        assert!(uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=101 wayland_ms=99 console_windows=0 wayland_windows=1"
        ));
        assert!(uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=72 wayland_ms=70 console_windows=1 wayland_windows=0\n\
wayclick profile: elapsed_ms=1000 callbacks=60\n\
uiserver: slow loop iter_ms=51 wayland_ms=0 present_ms=51 console_windows=1 wayland_windows=1"
        ));
    }

    #[test]
    fn interactive_gtk_display_uses_absolute_pointer_and_hides_host_cursor() {
        assert_eq!(
            mesa_dri_prime_for_pci_bdf("0000:65:00.0").unwrap(),
            "pci-0000_65_00_0"
        );
        assert!(mesa_dri_prime_for_pci_bdf("65:00.0").is_err());
        assert_eq!(
            qemu_display_backend(
                GuestDisplay::Headless,
                Some(Path::new("/dev/dri/renderD129"))
            )
            .unwrap(),
            "egl-headless,rendernode=/dev/dri/renderD129"
        );
        assert_eq!(
            qemu_display_backend(GuestDisplay::DvmGtk, None).unwrap(),
            "gtk,gl=on,show-tabs=off,zoom-to-fit=off,grab-on-hover=on,show-cursor=off"
        );
        assert_eq!(
            qemu_display_backend(GuestDisplay::Physical, None).unwrap(),
            "none"
        );
        assert!(qemu_display_backend(GuestDisplay::Headless, None).is_err());
        assert!(
            dvm_gpu_device(GuestDisplay::DvmGtk)
                .unwrap()
                .starts_with("virtio-gpu-gl-pci,id=")
        );
        assert!(
            dvm_gpu_device(GuestDisplay::Headless)
                .unwrap()
                .contains("hostmem=256M")
        );
        assert!(dvm_gpu_device(GuestDisplay::Physical).is_none());
        assert_eq!(dvm_machine(), "q35,accel=kvm,i8042=off");
        assert_eq!(
            dvm_pointer_device(GuestDisplay::DvmGtk),
            "virtio-tablet-pci,id=dvm-pointer,display=dvm-virtio-gpu,head=0"
        );
        assert_eq!(
            dvm_pointer_device(GuestDisplay::Physical),
            "virtio-tablet-pci,id=dvm-pointer"
        );

        let mut input = Command::new("qemu-system-x86_64");
        assert!(append_dvm_virtual_gpu(&mut input, GuestDisplay::DvmGtk));
        append_dvm_input_devices(&mut input, GuestDisplay::DvmGtk);
        let input_args = input
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let gpu_position = input_args
            .iter()
            .position(|arg| arg.starts_with("virtio-gpu-gl-pci,id=dvm-virtio-gpu"))
            .unwrap();
        let keyboard_position = input_args
            .iter()
            .position(|arg| arg.starts_with("virtio-keyboard-pci,id=dvm-keyboard"))
            .unwrap();
        let pointer_position = input_args
            .iter()
            .position(|arg| arg.starts_with("virtio-tablet-pci,id=dvm-pointer"))
            .unwrap();
        assert!(gpu_position < keyboard_position);
        assert!(keyboard_position < pointer_position);
        assert!(input_args[keyboard_position].contains("display=dvm-virtio-gpu,head=0"));
        assert!(input_args[pointer_position].contains("display=dvm-virtio-gpu,head=0"));

        let mut physical = Command::new("qemu-system-x86_64");
        append_dvm_network_device(&mut physical, GuestDisplay::Physical);
        let physical_args = physical
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(physical_args, ["-net", "none"]);

        let mut virtual_display = Command::new("qemu-system-x86_64");
        append_dvm_network_device(&mut virtual_display, GuestDisplay::Headless);
        let virtual_args = virtual_display
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(virtual_args.iter().any(|arg| arg == "user,id=dvm-net"));
        assert!(
            virtual_args
                .iter()
                .any(|arg| arg.starts_with("virtio-net-pci,netdev=dvm-net"))
        );
    }

    #[test]
    fn physical_pixel_backing_is_preallocated_and_dvm_read_only() {
        assert_eq!(DVM_DISPLAY_REGION_BYTES, 128 * 1024 * 1024);
        let path = std::path::Path::new("/dev/shm/rustos-kvm-test-pixels");

        let mut producer = Command::new("qemu-system-x86_64");
        append_dvm_display_pixels(&mut producer, path, false);
        let producer_args = producer
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(producer_args.iter().any(|arg| {
            arg.contains("memory-backend-file,id=dvm-display-pixels")
                && arg.contains("size=134217728")
                && arg.contains("share=on")
                && arg.contains("prealloc=on")
                && !arg.contains("readonly=on")
        }));

        let mut consumer = Command::new("qemu-system-x86_64");
        append_dvm_display_pixels(&mut consumer, path, true);
        let consumer_args = consumer
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(consumer_args.iter().any(|arg| {
            arg.contains("memory-backend-file,id=dvm-display-pixels")
                && arg.contains("readonly=on,rom=on")
                && !arg.contains("prealloc=on")
        }));
    }

    #[test]
    fn dvm_attached_block_disk_requires_qemu_read_only_backing() {
        dvm_block::dvm_attached_block_disk_requires_qemu_read_only_backing();
    }

    #[test]
    fn dvm_block_transport_header_matches_read_only_qemu_backing() {
        dvm_block::dvm_block_transport_header_matches_read_only_qemu_backing();
    }

    #[test]
    fn dvm_block_read_only_media_geometry_matches_atapi_capacity() {
        dvm_block::dvm_block_read_only_media_geometry_matches_atapi_capacity();
    }

    #[test]
    fn dvm_block_read_only_media_driver_closure_is_explicit() {
        dvm_block::dvm_block_read_only_media_driver_closure_is_explicit();
    }

    #[test]
    fn dvm_block_recovery_readiness_tracks_the_exact_successor_generation() {
        dvm_block::dvm_block_recovery_readiness_tracks_the_exact_successor_generation();
    }

    #[test]
    fn physical_gpu_profile_drives_vfio_bar_dmabuf_mapping() {
        let profile = PHYSICAL_GPU_PROFILES[0];
        assert_eq!(
            physical_gpu_profile(profile.vendor, profile.device),
            Some(profile)
        );
        assert_eq!(physical_gpu_profile("0x8086", "0x0000"), None);
        let mut command = Command::new("qemu-system-x86_64");
        append_physical_gpu(
            &mut command,
            profile,
            "0000:65:00.0",
            Path::new("/tmp/rustos-amdgpu-vfct.bin"),
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args.iter().any(|arg| arg == "iommufd,id=iommufd0"));
        assert!(
            args.iter()
                .any(|arg| arg == "file=/tmp/rustos-amdgpu-vfct.bin")
        );
        assert!(args.iter().any(|arg| {
            arg == "vfio-pci,host=0000:65:00.0,iommufd=iommufd0,addr=08.0,rombar=0"
        }));
        assert!(!args.iter().any(|arg| arg.contains("x-no-mmap")));
        assert!(
            args.iter()
                .any(|arg| arg == "enable=iommufd_backend_map_file_dma")
        );
        assert!(args.iter().any(|arg| arg == "enable=vfio_region_dmabuf"));
    }

    #[test]
    fn resetless_physical_gpu_profile_is_single_launch_per_host_boot() {
        let root = tempfile::tempdir().unwrap();
        let profile = PHYSICAL_GPU_PROFILES[0];
        let first_boot = "11111111-2222-3333-4444-555555555555";
        claim_physical_gpu_launch_in(root.path(), first_boot, profile, "0000:65:00.0").unwrap();
        assert!(
            claim_physical_gpu_launch_in(root.path(), first_boot, profile, "0000:65:00.0").is_err()
        );
        claim_physical_gpu_launch_in(
            root.path(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            profile,
            "0000:65:00.0",
        )
        .unwrap();
    }

    #[test]
    fn fps_proof_requires_a_real_gtk_display_consumer() {
        let standard = parse_smoke_options(Vec::<String>::new().into_iter()).unwrap();
        assert_eq!(
            select_smoke_guest_display(&standard, false).unwrap(),
            GuestDisplay::Headless
        );

        let fps =
            parse_smoke_options(vec!["--min-ui-fps".into(), "60".into()].into_iter()).unwrap();
        assert!(select_smoke_guest_display(&fps, false).is_err());
        assert_eq!(
            select_smoke_guest_display(&fps, true).unwrap(),
            GuestDisplay::DvmGtk
        );
        let physical = parse_smoke_options(
            vec![
                "--gui-dvm-surfaces".into(),
                "--physical-gpu".into(),
                "0000:65:00.0".into(),
                "--gpu-firmware".into(),
                "/tmp/amd-vfct.bin".into(),
                "--min-ui-fps".into(),
                "60".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(
            select_smoke_guest_display(&physical, false).unwrap(),
            GuestDisplay::Physical
        );
        assert!(
            parse_smoke_options(
                vec![
                    "--physical-amdgpu".into(),
                    "0000:65:00.0".into(),
                    "--amd-vfct".into(),
                    "/tmp/amd-vfct.bin".into(),
                ]
                .into_iter(),
            )
            .is_err()
        );
    }

    #[test]
    fn dvm_input_selftest_keeps_pointer_selection_and_one_keyboard_probe() {
        let source = include_str!(
            "../../../../driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c"
        );
        assert!(source.contains("UI_SET_KEYBIT, BTN_LEFT"));
        assert!(source.contains("UI_SET_ABSBIT, ABS_X"));
        assert!(source.contains("UI_SET_ABSBIT, ABS_Y"));
        assert!(source.contains("selftest->motion_phase == 0U"));
        assert!(source.contains("#define INPUT_SELFTEST_CYCLES 6000U"));
        assert!(source.contains("#define INPUT_SELFTEST_LEG_CYCLES 64U"));
        assert!(source.contains("#define INPUT_SELFTEST_POLL_MS 15"));
        assert!(source.contains("#define INPUT_RELAY_RR_PRIORITY 10"));
        assert!(source.contains("#define INPUT_RELAY_RTTIME_SOFT_US 50000U"));
        assert!(source.contains("#define INPUT_RELAY_RTTIME_HARD_US 100000U"));
        assert!(source.contains("sched_setscheduler(0, SCHED_RR, &realtime)"));
        assert!(source.contains("setrlimit(RLIMIT_RTTIME, &bounded_rttime)"));
        assert!(source.contains("guard->saved_policy != SCHED_OTHER"));
        assert!(source.contains("observed_rttime.rlim_cur != guard->saved_rttime.rlim_cur"));
        assert!(source.contains("die(\"input scheduler restore failed\")"));
        assert!(source.contains("input_scheduler_leave(&scheduler)"));
        assert!(source.contains("#define INPUT_POINTER_FLUSH_MS 5"));
        assert!(source.contains("case 0U:\n        dx = 3;\n        dy = 0;"));
        assert!(source.contains("case 1U:\n        dx = 0;\n        dy = 3;"));
        assert!(source.contains("write_input_event(fd, EV_KEY, KEY_F12, 1)"));
        assert!(source.contains("write_input_event(fd, EV_ABS, ABS_X, selftest->pointer_x)"));
        assert!(!source.contains("write_input_event(fd, EV_KEY, KEY_A, 1)"));
    }

    #[test]
    fn dvm_agent_local_readiness_is_process_owned_and_atomic() {
        let source = include_str!(
            "../../../../driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c"
        );
        assert!(source.contains("#define READY_OWNER_NAME \"agent-owner.lock\""));
        assert!(source.contains("#define READY_CANDIDATE_NAME \".ready.next\""));
        assert!(source.contains("O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW"));
        assert!(source.contains("flock(guard->singleton_fd, LOCK_EX | LOCK_NB)"));
        assert!(source.contains("flock(guard->ready_fd, LOCK_EX | LOCK_NB)"));
        assert!(
            source
                .contains("renameat(directory_fd, READY_CANDIDATE_NAME, directory_fd, \"ready\")")
        );
        assert!(source.contains("state.st_size != (off_t)expected_length"));
        assert!(source.contains("locked = flock(ready_fd, LOCK_EX | LOCK_NB)"));
        assert!(source.contains("return local_health(&contract) ? EXIT_SUCCESS : EXIT_FAILURE;"));
        assert!(!source.contains("access(READY_FILE"));
        assert!(!source.contains("fopen(READY_FILE"));
    }

    #[test]
    fn dvm_display_relay_has_bounded_authenticated_scheduler_admission() {
        let source = include_str!(
            "../../../../driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c"
        );
        let init = include_str!(
            "../../../../driver-domains/linux/board/overlay/etc/init.d/S48rustos-dvm-net"
        );
        assert!(source.contains("#define DISPLAY_RELAY_RR_PRIORITY 9"));
        assert!(source.contains("#define DISPLAY_RELAY_RTTIME_SOFT_US 50000U"));
        assert!(source.contains("#define DISPLAY_RELAY_RTTIME_HARD_US 100000U"));
        assert!(source.contains("sched_setscheduler(0, SCHED_RR, &realtime)"));
        assert!(source.contains("setrlimit(RLIMIT_RTTIME, &bounded_rttime)"));
        assert!(source.contains("guard->saved_policy != SCHED_OTHER"));
        assert!(source.contains("observed_rttime.rlim_cur != guard->saved_rttime.rlim_cur"));
        assert!(source.contains("return scheduler.fatal ? DISPLAY_SERVE_FATAL"));
        assert!(source.contains("result == DISPLAY_SERVE_FATAL"));
        assert!(source.contains("host_confirmed_peer_ready(shared)"));
        assert!(source.contains("display_scheduler_leave(&scheduler)"));
        assert!(source.contains("rttime_hard_action=terminate"));
        assert!(source.contains("#define RUSTOS_DVM_DISPLAY_OWNER_NAME \"display-owner.lock\""));
        assert!(
            source.contains("#define RUSTOS_DVM_DISPLAY_READY_CANDIDATE \".display-ready.next\"")
        );
        assert!(source.contains("owner_fd = claim_display_process_owner();"));
        assert!(source.contains("flock(owner_fd, LOCK_EX | LOCK_NB)"));
        assert!(source.contains("flock(ready_fd, LOCK_EX | LOCK_NB)"));
        assert!(
            source.contains(
                "renameat(directory_fd, RUSTOS_DVM_DISPLAY_READY_CANDIDATE, directory_fd,"
            )
        );
        let revoke = source
            .find("fail:\n    if (ready_lock >= 0) {")
            .expect("display failure must revoke readiness");
        let restore = source[revoke..]
            .find("display_scheduler_leave(&scheduler)")
            .expect("display failure must restore its scheduler");
        assert!(
            restore > 0,
            "readiness must be revoked before scheduler restore"
        );
        assert!(!source.contains("ftruncate(fd, 0)"));
        assert!(!source.contains("unlink(RUSTOS_DVM_DISPLAY_READY_LOCK)"));
        assert!(init.contains("mkdir -p \"$run_dir\" && chmod 0700 \"$run_dir\" || return 1"));
    }

    #[test]
    fn dvm_gpu_proof_has_lower_bounded_scheduler_admission_and_restore() {
        let source = include_str!(
            "../../../../driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-gpu-probe.c"
        );
        assert!(source.contains("#define GPU_PROOF_RR_PRIORITY 8"));
        assert!(source.contains("#define GPU_PROOF_RTTIME_SOFT_US 50000U"));
        assert!(source.contains("#define GPU_PROOF_RTTIME_HARD_US 100000U"));
        assert!(source.contains("sched_setscheduler(0, SCHED_RR, &realtime)"));
        assert!(source.contains("setrlimit(RLIMIT_RTTIME, &bounded_rttime)"));
        assert!(source.contains("guard->saved_policy != SCHED_OTHER"));
        assert!(source.contains("observed_rttime.rlim_cur != guard->saved_rttime.rlim_cur"));
        assert!(source.contains("proof_scheduler_leave(&proof_scheduler)"));
        assert!(source.contains("PROOF_RTTIME_HARD_ACTION=terminate"));
        assert!(source.contains("scheduler-restored=normal"));
    }

    #[test]
    fn sha256_shape_is_strict() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"g".repeat(64)));
        assert!(!is_sha256("abc"));
    }

    #[test]
    fn dvm_control_contract_and_manifest_are_bound() {
        let contract_source = include_str!(
            "../../../../driver-domains/linux/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"
        );
        let contract =
            parse_dvm_control_contract_text(contract_source, "control contract").unwrap();
        assert_eq!(contract.protocol, DVM_CONTROL_PROTOCOL);
        assert_eq!(contract.state, DVM_CONTROL_STATE);
        assert_eq!(contract.transport, DVM_CONTROL_TRANSPORT);
        assert_eq!(contract.authentication, DVM_CONTROL_AUTHENTICATION);
        assert_eq!(contract.capabilities.join(","), DVM_CONTROL_CAPABILITIES);

        let hash = "0".repeat(64);
        let manifest = format!(
            "schema=9\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-zstd\ndata-plane=hostd-input-ring-msix\ncontrol-plane=agent-v1-control\ncontrol-protocol=agent-v1\ncontrol-state=control\ncontrol-transport=kvm-vsock\ncontrol-authentication=dvm-agent-hmac-sha256-v1\ncontrol-capabilities=health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream\ncontrol-contract-sha256={hash}\nbuildroot_version=2026.05\nlinux_version=6.12.94\nnvidia-open-version=580.173.02\nnvidia-open-sha256=8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b\nnvidia-open-redistribute=no\ndisplay-kernel-modules=i915,xe,amdgpu,nvidia-drm\nmodule-signing-enforced=yes\nmodule-signing-cert-sha256={hash}\nkernel_sha256={hash}\nrootfs_sha256={hash}\nconfig_sha256={hash}\nkernel-config-sha256={hash}\nsources_lock_sha256={hash}\n"
        );
        let values = parse_manifest_text(&manifest, "manifest").unwrap();
        assert_eq!(validate_manifest_values(&values).unwrap(), contract);

        let mut extra = values;
        extra.insert("unexpected".to_owned(), "1".to_owned());
        assert!(validate_manifest_values(&extra).is_err());
    }

    #[test]
    fn dvm_control_contract_rejects_data_plane_capability() {
        let contract_source = include_str!(
            "../../../../driver-domains/linux/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"
        );
        let invalid = contract_source.replace(
            "CONTROL_CAPABILITIES=health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream",
            "CONTROL_CAPABILITIES=health,network-rx",
        );
        assert!(parse_dvm_control_contract_text(&invalid, "invalid contract").is_err());
    }


}
