// SPDX-License-Identifier: MIT
//
// Smoke-lane argument admission.
//
// Owner: this file owns the shape of one KVM smoke topology request and the
// only parser that may produce it. Booting, evidence, and acceptance live in
// `kvm/options.rs`; nothing here launches anything.
//
// Boundary: the argument vector is untrusted operator input. Every value is
// range-checked or rejected before it becomes a `SmokeOptions` field, so no
// launch path has to re-validate a CLI string.
//
// Failure: an unknown flag, a missing value, or an out-of-range count fails
// the command; it never silently degrades to a weaker topology.

#[derive(Debug)]
struct SmokeOptions {
    dry_run: bool,
    storage_only: bool,
    expect_block_flush_fault: bool,
    exercise_input: bool,
    exercise_network: bool,
    gui_dvm_surfaces: bool,
    dvm_network_shmem: bool,
    dvm_block_shmem: bool,
    rustos_vcpus: u8,
    smp_iteration: bool,
    smp_ring3_qualification: bool,
    smp_evidence_cohort: Option<String>,
    physical_gpu_bdf: Option<String>,
    physical_gpu_firmware: Option<PathBuf>,
    ipcbench_probe: Option<String>,
    min_ui_fps: Option<u32>,
    ui_proof_windows: usize,
    recovery_probe: RecoveryProbe,
    timeout: Duration,
    /// How many times to boot this exact topology before reporting. A defect
    /// that shows up in one boot of six is a pass *rate*, and a rate cannot be
    /// read off a single run.
    repeat: usize,
    /// Whether the lane refreshes the boot image before it launches.
    build_image: bool,
    /// Seal an unsealed formal profile instead of refusing the launch.
    ///
    /// A multi-vCPU boot is admitted only by the formal profile that models
    /// its topology, and an unsealed profile fails in a way that reads exactly
    /// like a boot failure - `formal verification run binding mismatch`. It is
    /// never what the caller wanted: it means the tree was edited since the
    /// last seal, which is the normal state of a working tree. Seal it with
    /// the profile's own command instead of failing and asking for that
    /// command by hand, exactly as the interactive `kvm-run` path already
    /// does. The spawn-time gate still validates independently, so nothing
    /// launches on a verification that did not pass.
    auto_verify: bool,
    expected_markers: Vec<String>,
    expected_dvm_markers: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RecoveryProbe {
    #[default]
    None,
    DvmRestart,
    RustosReboot,
    All,
}

impl RecoveryProbe {
    const fn includes_dvm_restart(self) -> bool {
        matches!(self, Self::DvmRestart | Self::All)
    }

    const fn includes_rustos_reboot(self) -> bool {
        matches!(self, Self::RustosReboot | Self::All)
    }
}

fn parse_smoke_options<I>(mut args: I) -> Result<SmokeOptions>
where
    I: Iterator<Item = String>,
{
    let mut options = SmokeOptions {
        dry_run: false,
        storage_only: false,
        expect_block_flush_fault: false,
        exercise_input: false,
        exercise_network: false,
        gui_dvm_surfaces: false,
        dvm_network_shmem: false,
        dvm_block_shmem: false,
        rustos_vcpus: 1,
        smp_iteration: false,
        smp_ring3_qualification: false,
        smp_evidence_cohort: None,
        physical_gpu_bdf: None,
        physical_gpu_firmware: None,
        ipcbench_probe: None,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        recovery_probe: RecoveryProbe::None,
        timeout: Duration::from_secs(MAX_SMOKE_TIMEOUT),
        repeat: 1,
        build_image: true,
        auto_verify: true,
        expected_markers: vec![
            RUSTOS_BOOT_MARKER.to_owned(),
            RUSTOS_INIT_IDENTITY_MARKER.to_owned(),
            RUSTOS_POST_INIT_PROVENANCE_MARKER.to_owned(),
            RUSTOS_GPU_SCENE_COMPILER_MARKER.to_owned(),
        ],
        expected_dvm_markers: vec![DVM_GPU_COMPOSITOR_MARKER.to_owned()],
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => options.dry_run = true,
            "--no-build" => options.build_image = false,
            "--no-auto-verify" => options.auto_verify = false,
            "--repeat" => {
                let value = next_value(&mut args, "--repeat")?;
                let runs = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid --repeat value: {value}"))?;
                if !(1..=64).contains(&runs) {
                    bail!("--repeat must be in 1..=64, got {runs}");
                }
                options.repeat = runs;
            }
            "--storage-dvm-only" => options.storage_only = true,
            "--storage-dvm-expect-flush-fault" => options.expect_block_flush_fault = true,
            "--exercise-input" => options.exercise_input = true,
            "--exercise-network" => options.exercise_network = true,
            "--gui-dvm-surfaces" => options.gui_dvm_surfaces = true,
            "--dvm-network-shmem" => options.dvm_network_shmem = true,
            "--dvm-block-shmem" => options.dvm_block_shmem = true,
            "--rustos-vcpus" => {
                options.rustos_vcpus = parse_rustos_vcpus(next_value(&mut args, "--rustos-vcpus")?)?
            }
            "--smp-iteration" => options.smp_iteration = true,
            "--smp-ring3-qualification" => options.smp_ring3_qualification = true,
            "--smp-evidence-cohort" => {
                if options.smp_evidence_cohort.is_some() {
                    bail!("--smp-evidence-cohort was supplied more than once");
                }
                let cohort = next_value(&mut args, "--smp-evidence-cohort")?;
                validate_smp_evidence_cohort(&cohort)?;
                options.smp_evidence_cohort = Some(cohort);
            }
            "--physical-gpu" | "--physical-amdgpu" => {
                if options.physical_gpu_bdf.is_some() {
                    bail!("physical GPU BDF was supplied more than once");
                }
                options.physical_gpu_bdf = Some(next_value(&mut args, &arg)?);
            }
            "--gpu-firmware" | "--amd-vfct" => {
                if options.physical_gpu_firmware.is_some() {
                    bail!("physical GPU firmware was supplied more than once");
                }
                options.physical_gpu_firmware = Some(PathBuf::from(next_value(&mut args, &arg)?));
            }
            "--ipcbench-probe" => {
                if options.ipcbench_probe.is_some() {
                    bail!("--ipcbench-probe was supplied more than once");
                }
                let probe = next_value(&mut args, "--ipcbench-probe")?;
                if probe.is_empty()
                    || !probe
                        .bytes()
                        .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                {
                    bail!(
                        "--ipcbench-probe must be a non-empty name of letters, digits, and underscores, got {probe}"
                    );
                }
                options.ipcbench_probe = Some(probe);
            }
            "--min-ui-fps" => {
                let value = next_value(&mut args, "--min-ui-fps")?;
                let fps = value
                    .parse::<u32>()
                    .with_context(|| format!("invalid --min-ui-fps value: {value}"))?;
                if !(1..=240).contains(&fps) {
                    bail!("--min-ui-fps must be in 1..=240, got {fps}");
                }
                options.min_ui_fps = Some(fps);
            }
            "--ui-proof-windows" => {
                let value = next_value(&mut args, "--ui-proof-windows")?;
                let windows = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid --ui-proof-windows value: {value}"))?;
                if !(DEFAULT_UI_FPS_ACTIVE_WINDOWS..=MAX_UI_FPS_ACTIVE_WINDOWS).contains(&windows) {
                    bail!(
                        "--ui-proof-windows must be in {}..={}, got {windows}",
                        DEFAULT_UI_FPS_ACTIVE_WINDOWS,
                        MAX_UI_FPS_ACTIVE_WINDOWS
                    );
                }
                options.ui_proof_windows = windows;
            }
            "--recovery-probe" => {
                let value = next_value(&mut args, "--recovery-probe")?;
                options.recovery_probe = match value.as_str() {
                    "dvm-restart" => RecoveryProbe::DvmRestart,
                    "rustos-reboot" => RecoveryProbe::RustosReboot,
                    "all" => RecoveryProbe::All,
                    _ => bail!(
                        "--recovery-probe must be dvm-restart, rustos-reboot, or all, got {value}"
                    ),
                };
            }
            "--timeout" => {
                let value = next_value(&mut args, "--timeout")?;
                let seconds = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid --timeout value: {value}"))?;
                if !(1..=MAX_SMOKE_TIMEOUT).contains(&seconds) {
                    bail!("--timeout must be in 1..={MAX_SMOKE_TIMEOUT}, got {seconds}");
                }
                options.timeout = Duration::from_secs(seconds);
            }
            "--expect" => {
                let marker = next_value(&mut args, "--expect")?;
                if marker.is_empty() || marker.contains(['\n', '\r']) {
                    bail!("--expect must be a non-empty single-line marker");
                }
                options.expected_markers.push(marker);
            }
            "--expect-dvm" => {
                let marker = next_value(&mut args, "--expect-dvm")?;
                if marker.is_empty() || marker.contains(['\n', '\r']) {
                    bail!("--expect-dvm must be a non-empty single-line marker");
                }
                options.expected_dvm_markers.push(marker);
            }
            unknown => bail!("unknown KVM smoke option: {unknown}"),
        }
    }

    // The graphical product topology launches its shell and WayClick from the
    // production DVM volume. Attaching the display transport without its
    // storage provider produces a compositor-only boot that can never satisfy
    // the visible desktop contract. Keep the two providers atomic at the
    // runner boundary; storage-only remains independently selectable below.
    if options.gui_dvm_surfaces {
        options.dvm_block_shmem = true;
    }

    // A frame-rate proof is the complete graphical product topology: shared
    // display control/pixels, its production DVM-backed app volume, and real
    // input. Reuse those providers atomically; a GTK consumer without the
    // shared display aperture can render only QEMU's unrelated guest console.
    if options.min_ui_fps.is_some() {
        options.gui_dvm_surfaces = true;
        options.exercise_input = true;
        options.dvm_block_shmem = true;
    }
    if options.smp_iteration {
        if options.timeout > Duration::from_secs(30) {
            bail!("--smp-iteration is restricted to a bounded --timeout of at most 30 seconds");
        }
        if options.min_ui_fps.is_some()
            || options.recovery_probe != RecoveryProbe::None
            || options.physical_gpu_bdf.is_some()
            || options.physical_gpu_firmware.is_some()
        {
            bail!("--smp-iteration cannot be used for FPS, recovery, or physical-GPU acceptance");
        }
    }
    if options.smp_ring3_qualification {
        if !options.smp_iteration {
            bail!("--smp-ring3-qualification requires --smp-iteration");
        }
        if options.smp_evidence_cohort.is_none() {
            bail!("--smp-ring3-qualification requires --smp-evidence-cohort");
        }
        if !matches!(options.rustos_vcpus, 1 | 2 | 4 | 8) {
            bail!("--smp-ring3-qualification requires --rustos-vcpus to be one of 1, 2, 4, or 8");
        }
        // The per-run private qualification contract is outside the signed
        // early-system closure and is visible to runtimed only through the
        // production DVM-backed FAT volume. The evidence executable itself is
        // separately pinned inside early-system against DVM substitution.
        options.dvm_block_shmem = true;
    } else if options.smp_evidence_cohort.is_some() {
        bail!("--smp-evidence-cohort requires --smp-ring3-qualification");
    }
    if options.storage_only {
        if options.exercise_input
            || options.exercise_network
            || options.gui_dvm_surfaces
            || options.dvm_network_shmem
            || options.min_ui_fps.is_some()
            || options.physical_gpu_bdf.is_some()
            || options.physical_gpu_firmware.is_some()
        {
            bail!(
                "--storage-dvm-only cannot be combined with UI, input, network, or physical-GPU proof options"
            );
        }
        options.dvm_block_shmem = true;
        options
            .expected_markers
            .retain(|marker| marker != RUSTOS_GPU_SCENE_COMPILER_MARKER);
        options
            .expected_dvm_markers
            .retain(|marker| marker != DVM_GPU_COMPOSITOR_MARKER);
    }
    if options.expect_block_flush_fault && !options.storage_only {
        bail!("--storage-dvm-expect-flush-fault requires --storage-dvm-only");
    }
    if options.recovery_probe != RecoveryProbe::None
        && (options.storage_only || options.expect_block_flush_fault)
    {
        bail!("--recovery-probe requires the normal positive product topology");
    }
    if options.recovery_probe != RecoveryProbe::None && options.min_ui_fps.is_some() {
        bail!("--recovery-probe and --min-ui-fps are separate bounded acceptance runs");
    }
    if options.exercise_input {
        options
            .expected_markers
            .push(DVM_KEYBOARD_INGRESS_MARKER.to_owned());
        options
            .expected_markers
            .push(DVM_POINTER_INGRESS_MARKER.to_owned());
    }
    if options.gui_dvm_surfaces {
        options
            .expected_markers
            .push(RUSTOS_GPU_ACTIVE_MARKER.to_owned());
        options
            .expected_dvm_markers
            .push(DVM_GPU_LIVE_MARKER.to_owned());
        options
            .expected_dvm_markers
            .push(DVM_BOOTSTRAP_FRAME_MARKER.to_owned());
    }
    if options.dvm_block_shmem {
        options
            .expected_markers
            .push(RUSTOS_DVM_BLOCK_MARKER.to_owned());
        options
            .expected_markers
            .push(RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER.to_owned());
        options.expected_markers.push(
            if options.expect_block_flush_fault {
                RUSTOS_DVM_BLOCK_FLUSH_FAULT_MARKER
            } else {
                RUSTOS_DVM_BLOCK_E2E_MARKER
            }
            .to_owned(),
        );
        options
            .expected_dvm_markers
            .push(DVM_BLOCK_READY_MARKER.to_owned());
        if options.gui_dvm_surfaces && !options.expect_block_flush_fault {
            options
                .expected_markers
                .push(WAYCLICK_FIRST_FRAME_MARKER.to_owned());
        }
    }
    if options.exercise_network && !options.dvm_network_shmem {
        bail!("--exercise-network requires --dvm-network-shmem");
    }
    if options.exercise_network && !options.gui_dvm_surfaces {
        bail!(
            "--exercise-network requires --gui-dvm-surfaces so runtimed can admit the app catalog"
        );
    }
    if options.ui_proof_windows != DEFAULT_UI_FPS_ACTIVE_WINDOWS && options.min_ui_fps.is_none() {
        bail!("--ui-proof-windows requires --min-ui-fps");
    }
    match (&options.physical_gpu_bdf, &options.physical_gpu_firmware) {
        (Some(_), Some(_)) if !options.gui_dvm_surfaces => {
            bail!("--physical-gpu requires --gui-dvm-surfaces")
        }
        (Some(_), Some(_)) => {}
        (None, None) => {}
        _ => bail!("--physical-gpu and --gpu-firmware must be supplied together"),
    }
    Ok(options)
}

fn validate_storage_fault_expectation(
    enabled: bool,
    rules: &[String],
    options: &SmokeOptions,
) -> Result<()> {
    if !options.expect_block_flush_fault {
        return Ok(());
    }
    if !enabled {
        bail!(
            "--storage-dvm-expect-flush-fault requires enabled fault injection with exactly block.flush=fail"
        );
    }
    let flush_actions = rules
        .iter()
        .map(|rule| {
            rustos_fault_injection::parse_rule(rule)
                .map_err(|error| anyhow::anyhow!("invalid configured fault rule {rule:?}: {error}"))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|rule| (rule.location == "block.flush").then_some(rule.action))
        .collect::<Vec<_>>();
    if flush_actions.as_slice() != [rustos_fault_injection::FaultAction::Fail] {
        bail!(
            "--storage-dvm-expect-flush-fault requires exactly one block.flush=fail rule and no competing block.flush rule"
        );
    }
    Ok(())
}

fn next_value<I>(args: &mut I, option: &str) -> Result<String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("missing value for {option}"))
}

fn validate_smp_evidence_cohort(value: &str) -> Result<()> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("--smp-evidence-cohort must be exactly 32 lowercase hexadecimal characters");
    }
    Ok(())
}
