// SPDX-License-Identifier: MIT

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalGpuFirmwareKind {
    AmdVfct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PhysicalGpuProfile {
    id: &'static str,
    vendor: &'static str,
    device: &'static str,
    drm_driver: &'static str,
    guest_address: &'static str,
    backend_class: &'static str,
    firmware_kind: PhysicalGpuFirmwareKind,
}

const PHYSICAL_GPU_PROFILES: &[PhysicalGpuProfile] = &[PhysicalGpuProfile {
    id: "amd-hawkpoint-1002-1900",
    vendor: "0x1002",
    device: "0x1900",
    drm_driver: "amdgpu",
    guest_address: "08.0",
    backend_class: "physical-direct",
    firmware_kind: PhysicalGpuFirmwareKind::AmdVfct,
}];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GpuEvidenceExpectation {
    drm_driver: &'static str,
    backend_class: &'static str,
}

const VIRTUAL_GPU_EVIDENCE: GpuEvidenceExpectation = GpuEvidenceExpectation {
    drm_driver: "virtio_gpu",
    backend_class: "virtual-staged",
};

#[derive(Debug)]
struct DvmArtifacts {
    kernel: PathBuf,
    rootfs: PathBuf,
    control: DvmControlContract,
}

#[derive(Debug, Eq, PartialEq)]
struct DvmControlContract {
    protocol: String,
    state: String,
    transport: String,
    authentication: String,
    capabilities: Vec<String>,
}

impl DvmControlContract {
    fn control_plane(&self) -> String {
        format!("{}-{}", self.protocol, self.state)
    }
}

#[derive(Debug)]
struct KvmLayout {
    run_dir: PathBuf,
    guest_cid: u32,
    runtime_disk: PathBuf,
    debugcon_log: PathBuf,
    rustos_serial_log: PathBuf,
    dvm_serial_log: PathBuf,
    rustos_stderr_log: PathBuf,
    dvm_stderr_log: PathBuf,
    dvm_input_ring: PathBuf,
    dvm_input_doorbell: PathBuf,
    rustos_monitor: PathBuf,
    dvm_control_secret: PathBuf,
    // Keep the private tmpfs directory alive until both QEMU children have
    // exited. Dropping it then removes both DMA-pinnable display backings.
    _display_backing_dir: Option<TempDir>,
    gui_dvm_surfaces: Option<PathBuf>,
    gui_dvm_pixels: Option<PathBuf>,
    dvm_display_doorbell: Option<PathBuf>,
    dvm_network_shmem: Option<PathBuf>,
    dvm_block_aperture: Option<PathBuf>,
    dvm_block_doorbell: Option<PathBuf>,
    dvm_block_disk: Option<PathBuf>,
}

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
    physical_gpu_bdf: Option<String>,
    physical_gpu_firmware: Option<PathBuf>,
    min_ui_fps: Option<u32>,
    ui_proof_windows: usize,
    recovery_probe: RecoveryProbe,
    timeout: Duration,
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

#[derive(Debug)]
struct KvmLaunchLock {
    _file: std::fs::File,
}

fn acquire_kvm_launch_lock(run_dir: &Path) -> Result<KvmLaunchLock> {
    fs::create_dir_all(run_dir)?;
    fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o700))?;
    let path = run_dir.join("launch.lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open KVM launch lock {}", path.display()))?;
    fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            bail!(
                "another RustOS KVM launch owns {}; close it before starting F5 or kvm-smoke",
                path.display()
            );
        }
        return Err(error).with_context(|| format!("lock KVM launch {}", path.display()));
    }
    file.set_len(0)?;
    writeln!(file, "pid={}", std::process::id())?;
    file.sync_data()?;
    Ok(KvmLaunchLock { _file: file })
}

fn guest_cid_for_process(pid: u32) -> u32 {
    MIN_DVM_GUEST_CID + pid % (u32::MAX - MIN_DVM_GUEST_CID)
}

pub(crate) fn build_dvm_command(config: &Config) -> Result<()> {
    let dvm_dir = dvm_dir(config);
    let mut command = Command::new("make");
    command.arg("-C").arg(&dvm_dir).arg("build");
    run_command(&mut command)?;
    verify_dvm_artifacts(config)?;
    println!(
        "xtask: verified Linux DVM artifacts in {}",
        dvm_dir.display()
    );
    Ok(())
}

pub(crate) fn verify_dvm_command(config: &Config) -> Result<()> {
    let artifacts = verify_dvm_artifacts(config)?;
    println!(
        "xtask: Linux DVM verified kernel={} rootfs={} control-plane={} capabilities={}",
        artifacts.kernel.display(),
        artifacts.rootfs.display(),
        artifacts.control.control_plane(),
        artifacts.control.capabilities.join(","),
    );
    Ok(())
}

pub(crate) fn kvm_smoke_command<I>(config: &Config, args: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    let args = args.collect::<Vec<_>>();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        print_kvm_smoke_help();
        return Ok(());
    }
    let options = parse_smoke_options(args.into_iter())?;
    let _launch_lock = acquire_kvm_launch_lock(&config.build_dir.join("kvm"))?;
    validate_storage_fault_expectation(
        config.project.fault_injection.enabled,
        &config.project.fault_injection.rules,
        &options,
    )?;
    let artifacts = verify_dvm_artifacts(config)?;
    let qemu = require_qemu(config)?;
    let layout = prepare_layout(config, &options)?;

    if options.physical_gpu_bdf.is_some() {
        validate_physical_gpu_inputs(&options)?;
    }

    if options.dry_run {
        println!(
            "xtask: KVM smoke inputs prepared in {}",
            layout.run_dir.display()
        );
        return Ok(());
    }

    require_vhost_vsock()?;
    if options.physical_gpu_bdf.is_some() {
        claim_physical_gpu_launch(&layout, &options)?;
    }
    let input_doorbell = start_dvm_input_doorbell(&layout)?;
    let input_relay_gate = Arc::new(AtomicBool::new(false));
    let control_relay = start_dvm_input_relay(
        config,
        options.timeout,
        layout.guest_cid,
        layout.dvm_input_doorbell.clone(),
        layout.dvm_input_ring.clone(),
        layout.dvm_control_secret.clone(),
        Arc::clone(&input_relay_gate),
    )?;
    let display_doorbell = start_dvm_display_doorbell(&layout)?;
    let block_doorbell = start_dvm_block_doorbell(&layout)?;
    let guest_display = smoke_guest_display(&options)?;
    let host_render_node = if guest_display != GuestDisplay::Physical {
        Some(require_host_render_node()?)
    } else {
        None
    };
    let (mut rustos, mut dvm) = spawn_guests(
        &qemu,
        config,
        &artifacts,
        &layout,
        &options,
        guest_display,
        host_render_node.as_deref(),
        display_doorbell.as_ref(),
        block_doorbell.as_ref(),
        &input_doorbell,
        Arc::clone(&input_relay_gate),
    )?;
    // `--timeout` is the readiness budget promised by the CLI, not a budget
    // for host-side doorbell setup, render-node admission, or process
    // creation. Start it only after both guest processes exist.
    let boot_started = Instant::now();
    let deadline = boot_started + options.timeout;
    let result: Result<ProbeResult> = (|| {
        let probe = wait_for_parallel_boot(
            &mut rustos,
            &mut dvm,
            &layout,
            &options,
            boot_started,
            deadline,
            &control_relay,
        )?;
        if options.dvm_block_shmem {
            verify_dvm_block_ready(&layout)?;
        }
        RecoveryHarness {
            qemu: &qemu,
            config,
            artifacts: &artifacts,
            layout: &layout,
            options: &options,
            guest_display,
            host_render_node: host_render_node.as_deref(),
            input_doorbell: &input_doorbell,
            display_doorbell: display_doorbell.as_ref(),
            block_doorbell: block_doorbell.as_ref(),
            input_relay_gate: Arc::clone(&input_relay_gate),
        }
        .run(&mut rustos, &mut dvm, probe)
    })();
    stop_guest(&mut rustos);
    stop_guest(&mut dvm);
    let probe = result?;
    validate_ui_fps_proof(&layout, &options)?;
    if let (Some(shared_display), Some(shared_pixels)) = (
        layout.gui_dvm_surfaces.as_deref(),
        layout.gui_dvm_pixels.as_deref(),
    ) {
        verify_dvm_display_surface(shared_display, shared_pixels)?;
    }
    if options.exercise_network {
        let shared_network = layout
            .dvm_network_shmem
            .as_deref()
            .context("network exercise lost its shared DVM network aperture")?;
        verify_dvm_network_round_trip(shared_network)?;
    }
    // A deliberately failed flush is a negative fault proof, not a successful
    // storage-ready scenario. Replaying it through the positive product trace
    // would either fabricate readiness or reject an otherwise valid fault run.
    if !options.expect_block_flush_fault {
        crate::formal_contracts::record_kvm_runtime_trace(
            &config.root_dir,
            crate::formal_contracts::KvmRuntimeObservation {
                elapsed_ms: boot_started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                storage: options.dvm_block_shmem,
                input: true,
                display: options.gui_dvm_surfaces,
                network: options.exercise_network,
                ui_budget: options.min_ui_fps.is_some(),
                storage_only: options.storage_only,
            },
            &layout.debugcon_log,
            &layout.dvm_serial_log,
        )?;
    }
    println!(
        "xtask: parallel KVM boot passed (RustOS + Linux DVM); control={} established authenticated L0 input relay (DVM cid={}, inventory={}, virtio-net={}, virtio-gpu={}) without QMP",
        artifacts.control.control_plane(),
        probe.peer_cid,
        probe.inventory_count,
        if probe.driver_inventory.virtio_net_bound {
            "bound"
        } else {
            "missing"
        },
        if probe.driver_inventory.virtio_gpu_bound {
            "bound"
        } else {
            "missing"
        },
    );
    Ok(())
}

/// A profile rate is a visual-performance claim only when QEMU has a real
/// display consumer.  `-display none` deliberately suppresses output and its
/// virtio-GPU completion cadence is not the interactive GTK cadence observed
/// by F5 users, so accepting it would turn a headless timing artifact into a
/// false desktop-performance success.
fn smoke_guest_display(options: &SmokeOptions) -> Result<GuestDisplay> {
    select_smoke_guest_display(
        options,
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some(),
    )
}

fn select_smoke_guest_display(options: &SmokeOptions, host_has_gui: bool) -> Result<GuestDisplay> {
    if options.physical_gpu_bdf.is_some() {
        return Ok(GuestDisplay::Physical);
    }
    if options.min_ui_fps.is_none() {
        return Ok(GuestDisplay::Headless);
    }
    if !host_has_gui {
        bail!(
            "--min-ui-fps requires a host GUI session so the Linux DVM uses QEMU GTK rather than -display none"
        );
    }
    Ok(GuestDisplay::DvmGtk)
}

/// Start the normal KVM driver-domain topology as an interactive session.
/// Unlike `kvm-smoke`, this is an operator-owned interactive session. It
/// reports topology readiness when observed and remains alive until the user
/// closes the DVM QEMU window or interrupts it. Acceptance is still enforced
/// when the session closes; a slow but progressing debug boot is not killed by
/// an arbitrary startup deadline.
pub(crate) fn kvm_run_command(config: &Config, build_image: bool) -> Result<()> {
    let _launch_lock = acquire_kvm_launch_lock(&config.build_dir.join("kvm"))?;
    if build_image {
        crate::build::build(config, false)?;
    }
    let started_at = Instant::now();
    let artifacts = verify_dvm_artifacts(config)?;
    log_kvm_start_phase("verified-dvm-artifacts", started_at);
    let qemu = require_qemu(config)?;
    log_kvm_start_phase("resolved-qemu", started_at);
    let options = SmokeOptions {
        dry_run: false,
        storage_only: false,
        expect_block_flush_fault: false,
        exercise_input: false,
        exercise_network: false,
        gui_dvm_surfaces: true,
        dvm_network_shmem: true,
        dvm_block_shmem: true,
        physical_gpu_bdf: None,
        physical_gpu_firmware: None,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        recovery_probe: RecoveryProbe::None,
        timeout: Duration::ZERO,
        expected_markers: vec![
            RUSTOS_GPU_ACTIVE_MARKER.to_owned(),
            RUSTOS_DVM_BLOCK_MARKER.to_owned(),
            WAYCLICK_FIRST_FRAME_MARKER.to_owned(),
        ],
        expected_dvm_markers: vec![
            DVM_GPU_COMPOSITOR_MARKER.to_owned(),
            DVM_GPU_LIVE_MARKER.to_owned(),
            DVM_BOOTSTRAP_FRAME_MARKER.to_owned(),
            DVM_BLOCK_READY_MARKER.to_owned(),
        ],
    };
    let layout = prepare_layout(config, &options)?;
    log_kvm_start_phase("prepared-kvm-layout", started_at);
    require_vhost_vsock()?;
    let host_render_node = require_host_render_node()?;
    log_kvm_start_phase("verified-vhost-vsock", started_at);
    let input_doorbell = start_dvm_input_doorbell(&layout)?;
    log_kvm_start_phase("started-input-doorbell", started_at);
    let input_relay_gate = Arc::new(AtomicBool::new(false));
    start_dvm_input_relay_unbounded(
        config,
        layout.guest_cid,
        layout.dvm_input_doorbell.clone(),
        layout.dvm_input_ring.clone(),
        layout.dvm_control_secret.clone(),
        Arc::clone(&input_relay_gate),
    )?;
    log_kvm_start_phase("started-input-relay", started_at);
    let display_doorbell = start_dvm_display_doorbell(&layout)?;
    log_kvm_start_phase("started-display-doorbell", started_at);
    let block_doorbell = start_dvm_block_doorbell(&layout)?;
    log_kvm_start_phase("started-block-doorbell", started_at);
    let (mut rustos, mut dvm) = spawn_guests(
        &qemu,
        config,
        &artifacts,
        &layout,
        &options,
        GuestDisplay::DvmGtk,
        Some(&host_render_node),
        display_doorbell.as_ref(),
        block_doorbell.as_ref(),
        &input_doorbell,
        input_relay_gate,
    )?;
    log_kvm_start_phase("spawned-guests", started_at);
    let interactive_boot_started = Instant::now();

    println!(
        "xtask: interactive KVM DVM guests started in {} ms; user-visible first-frame readiness is required within {} ms of guest monotonic boot time",
        started_at.elapsed().as_millis(),
        BOOT_TO_UI_HARD_LIMIT_MS,
    );
    let mut pointer_observed = false;
    let mut readiness_verified = false;
    loop {
        if let Some(status) = dvm
            .try_wait()
            .context("poll interactive Linux DVM QEMU session")?
        {
            stop_guest(&mut rustos);
            if !status.success() {
                bail!("interactive Linux DVM QEMU session exited with {status}")
            }
            validate_interactive_session(&layout, pointer_observed)?;
            return Ok(());
        }
        if let Some(status) = rustos
            .try_wait()
            .context("poll interactive RustOS QEMU session")?
        {
            stop_guest(&mut dvm);
            bail!("interactive RustOS QEMU session exited with {status}");
        }
        let rustos_log = read_runtime_log_if_present(&layout.debugcon_log)?;
        let dvm_log = read_runtime_log_if_present(&layout.dvm_serial_log)?;
        if !rustos_log.contains(WAYCLICK_FIRST_FRAME_MARKER)
            && guest_deadline_reached(&rustos_log, BOOT_TO_UI_HARD_LIMIT_MS)
        {
            let reason = format!(
                "interactive RustOS missed the {} ms user-visible boot limit",
                BOOT_TO_UI_HARD_LIMIT_MS
            );
            let missing_rustos = vec![WAYCLICK_FIRST_FRAME_MARKER.to_owned()];
            let evidence = write_kvm_failure_summary(
                &layout,
                &reason,
                interactive_boot_started.elapsed(),
                &rustos_log,
                &dvm_log,
                &missing_rustos,
                &[],
            )?;
            stop_guest(&mut dvm);
            stop_guest(&mut rustos);
            bail!(
                "{reason}; missing={WAYCLICK_FIRST_FRAME_MARKER:?}; evidence={}; inspect {} and {}",
                evidence.display(),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
            );
        }
        if runtime_stall_or_crash_observed(&rustos_log) || runtime_stall_or_crash_observed(&dvm_log)
        {
            let reason =
                "interactive KVM DVM session observed a watchdog, stall, crash, or relay stop";
            let evidence = write_kvm_failure_summary(
                &layout,
                reason,
                interactive_boot_started.elapsed(),
                &rustos_log,
                &dvm_log,
                &[],
                &[],
            )?;
            stop_guest(&mut dvm);
            stop_guest(&mut rustos);
            bail!(
                "{reason}; evidence={}; inspect {} and {}",
                evidence.display(),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
            );
        }
        if !readiness_verified && interactive_display_ready(&layout, &rustos_log, &dvm_log) {
            if let Err(error) = crate::formal_contracts::record_kvm_runtime_trace(
                &config.root_dir,
                crate::formal_contracts::KvmRuntimeObservation {
                    elapsed_ms: interactive_boot_started
                        .elapsed()
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                    storage: true,
                    input: true,
                    display: true,
                    network: false,
                    ui_budget: false,
                    storage_only: false,
                },
                &layout.debugcon_log,
                &layout.dvm_serial_log,
            ) {
                stop_guest(&mut dvm);
                stop_guest(&mut rustos);
                return Err(error).context("record interactive product boot trace");
            }
            readiness_verified = true;
            println!(
                "xtask: interactive KVM DVM display/storage verified in {} ms; move the pointer into the Linux DVM window to record real-input acceptance evidence, then close it or press Ctrl-C to stop",
                started_at.elapsed().as_millis(),
            );
        }
        if !pointer_observed && rustos_log.contains(DVM_POINTER_INGRESS_MARKER) {
            pointer_observed = true;
            println!(
                "xtask: interactive KVM DVM observed real absolute-pointer ingress; acceptance evidence will be checked when the session closes"
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn log_kvm_start_phase(phase: &str, started_at: Instant) {
    println!(
        "xtask: KVM start phase={phase} elapsed_ms={}",
        started_at.elapsed().as_millis()
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuestDisplay {
    Headless,
    DvmGtk,
    Physical,
}

fn qemu_display_backend(display: GuestDisplay, render_node: Option<&Path>) -> Result<String> {
    match display {
        // Keep the noninteractive proof on the physical host render node.
        // Allowing QEMU to pick a software EGL device would fabricate GPU
        // execution evidence even though the guest-visible driver is virgl.
        GuestDisplay::Headless => {
            let render_node =
                render_node.context("headless KVM display requires a validated render node")?;
            Ok(format!("egl-headless,rendernode={}", render_node.display()))
        }
        // GTK captures keyboard focus on hover. Pointer input is supplied by
        // the absolute tablet below, so F5 never needs a manual mouse grab.
        // Force-hide the host cursor. Leaving this option unset delegates the
        // result to the frontend default and previously left the host pointer
        // visible over the guest UI on some GTK versions.
        //
        GuestDisplay::DvmGtk => Ok(
            "gtk,gl=on,show-tabs=off,zoom-to-fit=off,grab-on-hover=on,show-cursor=off".to_owned(),
        ),
        GuestDisplay::Physical => Ok("none".to_owned()),
    }
}

fn dvm_machine() -> &'static str {
    // The DVM receives only the explicit virtio input devices below. Leaving
    // q35's implicit i8042 enabled creates a second PS/2 keyboard/pointer pair,
    // so Linux may relay a different event device than the GUI frontend feeds.
    "q35,accel=kvm,i8042=off"
}

fn dvm_gpu_device(display: GuestDisplay) -> Option<String> {
    (display != GuestDisplay::Physical).then(|| {
        format!(
            // Virgl executes the same fixed GLES vocabulary intended for the AMD
            // DVM. The physical AMD relay has a separate read-only DMA-BUF source
            // import and atomic-KMS path; this virtual device remains the explicit
            // staged-copy fallback and enables no CPU-composition success path.
            "virtio-gpu-gl-pci,id=dvm-virtio-gpu,xres={},yres={},edid=off,blob=on,hostmem=256M",
            DVM_DISPLAY_WIDTH, DVM_DISPLAY_HEIGHT
        )
    })
}

fn dvm_keyboard_device(display: GuestDisplay) -> &'static str {
    if display == GuestDisplay::Physical {
        "virtio-keyboard-pci,id=dvm-keyboard"
    } else {
        "virtio-keyboard-pci,id=dvm-keyboard,display=dvm-virtio-gpu,head=0"
    }
}

fn dvm_pointer_device(display: GuestDisplay) -> &'static str {
    // An absolute tablet keeps host pointer motion available while the GTK
    // window is merely hovered. The DVM agent normalizes the tablet range to
    // the fixed 1600x900 scanout before it emits the authenticated RDI3 frame.
    if display == GuestDisplay::Physical {
        "virtio-tablet-pci,id=dvm-pointer"
    } else {
        "virtio-tablet-pci,id=dvm-pointer,display=dvm-virtio-gpu,head=0"
    }
}

fn append_dvm_virtual_gpu(command: &mut Command, display: GuestDisplay) -> bool {
    let Some(gpu) = dvm_gpu_device(display) else {
        return false;
    };
    // QEMU resolves virtio-input's `display=` property while realizing the
    // device. Register the target GPU console before either input device.
    command.arg("-no-shutdown").arg("-device").arg(gpu);
    true
}

fn append_dvm_input_devices(command: &mut Command, display: GuestDisplay) {
    command
        .arg("-device")
        .arg(dvm_keyboard_device(display))
        .arg("-device")
        .arg(dvm_pointer_device(display));
}

fn append_dvm_network_device(command: &mut Command, display: GuestDisplay) {
    if display == GuestDisplay::Physical {
        // The physical-display laboratory topology intentionally contains no
        // network device. `-net none` is required because omitting all net
        // arguments lets QEMU synthesize a default virtual NIC.
        command.args(["-net", "none"]);
    } else {
        command
            .arg("-netdev")
            .arg("user,id=dvm-net")
            .arg("-device")
            .arg("virtio-net-pci,netdev=dvm-net,id=dvm-virtio-net,mac=52:54:00:12:34:56");
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
        physical_gpu_bdf: None,
        physical_gpu_firmware: None,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        recovery_probe: RecoveryProbe::None,
        timeout: Duration::from_secs(MAX_SMOKE_TIMEOUT),
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
            "--storage-dvm-only" => options.storage_only = true,
            "--storage-dvm-expect-flush-fault" => options.expect_block_flush_fault = true,
            "--exercise-input" => options.exercise_input = true,
            "--exercise-network" => options.exercise_network = true,
            "--gui-dvm-surfaces" => options.gui_dvm_surfaces = true,
            "--dvm-network-shmem" => options.dvm_network_shmem = true,
            "--dvm-block-shmem" => options.dvm_block_shmem = true,
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

    // A frame-rate proof without input and the production storage provider can
    // only time out with idle windows or ENODEV launch retries. Reuse the normal
    // DVM uinput and block paths; no QMP or embedded-volume shortcut is added.
    if options.min_ui_fps.is_some() {
        options.exercise_input = true;
        options.dvm_block_shmem = true;
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

fn dvm_dir(config: &Config) -> PathBuf {
    config.root_dir.join("driver-domains/linux")
}

fn dvm_artifact_dir(config: &Config) -> PathBuf {
    dvm_dir(config).join("out/artifacts")
}

fn verify_dvm_artifacts(config: &Config) -> Result<DvmArtifacts> {
    let dvm_root = dvm_dir(config);
    let dev_output_marker = dvm_root.join(DVM_DEV_OUTPUT_MARKER);
    if dev_output_marker.is_file() {
        bail!(
            "Linux DVM artifacts are stale after a development-only package build ({}); run the matching make rebuild-* target before verification or KVM",
            dev_output_marker.display()
        );
    }
    let artifact_dir = dvm_artifact_dir(config);
    let manifest_path = artifact_dir.join(DVM_MANIFEST);
    let values = parse_manifest(&manifest_path)?;
    let control = validate_manifest_values(&values)?;

    let kernel = artifact_dir.join(DVM_KERNEL);
    let rootfs = artifact_dir.join(DVM_ROOTFS);
    let build_config = artifact_dir.join(DVM_CONFIG);
    let kernel_config = artifact_dir.join(DVM_KERNEL_CONFIG);
    let module_signing_cert = artifact_dir.join(DVM_MODULE_SIGNING_CERT);
    let packaged_sources_lock = artifact_dir.join(DVM_SOURCES_LOCK);
    let packaged_control_contract = artifact_dir.join(DVM_CONTROL_ARTIFACT);
    verify_manifest_hash(&kernel, manifest_value(&values, "kernel_sha256")?)?;
    verify_manifest_hash(&rootfs, manifest_value(&values, "rootfs_sha256")?)?;
    verify_manifest_hash(&build_config, manifest_value(&values, "config_sha256")?)?;
    verify_manifest_hash(
        &kernel_config,
        manifest_value(&values, "kernel-config-sha256")?,
    )?;
    validate_signed_module_kernel_config(&kernel_config)?;
    verify_manifest_hash(
        &module_signing_cert,
        manifest_value(&values, "module-signing-cert-sha256")?,
    )?;
    verify_manifest_hash(
        &packaged_sources_lock,
        manifest_value(&values, "sources_lock_sha256")?,
    )?;
    verify_manifest_hash(
        &dvm_dir(config).join("sources.lock"),
        manifest_value(&values, "sources_lock_sha256")?,
    )?;
    let control_contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    verify_manifest_hash(
        &packaged_control_contract,
        manifest_value(&values, "control-contract-sha256")?,
    )?;
    verify_manifest_hash(
        &control_contract_path,
        manifest_value(&values, "control-contract-sha256")?,
    )?;
    let source_contract = parse_dvm_control_contract(&control_contract_path)?;
    let packaged_contract = parse_dvm_control_contract(&packaged_control_contract)?;
    if control != packaged_contract {
        bail!(
            "Linux DVM control contract mismatch between manifest and {}",
            packaged_control_contract.display()
        );
    }
    if control != source_contract {
        bail!(
            "Linux DVM control contract mismatch between manifest and {}",
            control_contract_path.display()
        );
    }

    Ok(DvmArtifacts {
        kernel,
        rootfs,
        control,
    })
}

fn parse_manifest(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "missing Linux DVM manifest {}; run `cargo xtask build-dvm` first",
            path.display()
        )
    })?;
    parse_manifest_text(&text, &path.display().to_string())
}

fn parse_manifest_text(text: &str, source: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for (line_number, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line != raw_line {
            bail!("invalid DVM manifest {source}:{}", line_number + 1);
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("invalid DVM manifest {source}:{}", line_number + 1))?;
        if key.is_empty() || value.is_empty() || key.contains(char::is_whitespace) {
            bail!("invalid DVM manifest {source}:{}", line_number + 1);
        }
        if values.insert(key.to_owned(), value.to_owned()).is_some() {
            bail!("duplicate DVM manifest key {key:?} in {source}");
        }
    }
    Ok(values)
}

fn validate_manifest_values(values: &BTreeMap<String, String>) -> Result<DvmControlContract> {
    const REQUIRED_KEYS: [&str; 25] = [
        "schema",
        "id",
        "architecture",
        "boot",
        "data-plane",
        "control-plane",
        "control-protocol",
        "control-state",
        "control-transport",
        "control-authentication",
        "control-capabilities",
        "control-contract-sha256",
        "buildroot_version",
        "linux_version",
        "nvidia-open-version",
        "nvidia-open-sha256",
        "nvidia-open-redistribute",
        "display-kernel-modules",
        "module-signing-enforced",
        "module-signing-cert-sha256",
        "kernel_sha256",
        "rootfs_sha256",
        "config_sha256",
        "kernel-config-sha256",
        "sources_lock_sha256",
    ];
    if values.len() != REQUIRED_KEYS.len()
        || values
            .keys()
            .any(|key| !REQUIRED_KEYS.contains(&key.as_str()))
    {
        bail!("unsupported Linux DVM manifest key set");
    }
    require_manifest_value(values, "schema", DVM_MANIFEST_SCHEMA)?;
    require_manifest_value(values, "id", "rustos-linux-dvm-x86_64")?;
    require_manifest_value(values, "architecture", "x86_64")?;
    require_manifest_value(values, "boot", "linux-bzimage+cpio-zstd")?;
    require_manifest_value(values, "data-plane", "hostd-input-ring-msix")?;
    require_manifest_value(values, "buildroot_version", "2026.05")?;
    require_manifest_value(values, "linux_version", "6.12.94")?;
    require_manifest_value(values, "nvidia-open-version", "580.173.02")?;
    require_manifest_value(
        values,
        "nvidia-open-sha256",
        "8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b",
    )?;
    require_manifest_value(values, "nvidia-open-redistribute", "no")?;
    require_manifest_value(
        values,
        "display-kernel-modules",
        "i915,xe,amdgpu,nvidia-drm",
    )?;
    require_manifest_value(values, "module-signing-enforced", "yes")?;
    for key in [
        "kernel_sha256",
        "rootfs_sha256",
        "config_sha256",
        "kernel-config-sha256",
        "sources_lock_sha256",
        "nvidia-open-sha256",
        "module-signing-cert-sha256",
    ] {
        if !is_sha256(manifest_value(values, key)?) {
            bail!("invalid SHA-256 value for Linux DVM manifest key {key}");
        }
    }
    let control = DvmControlContract {
        protocol: manifest_value(values, "control-protocol")?.to_owned(),
        state: manifest_value(values, "control-state")?.to_owned(),
        transport: manifest_value(values, "control-transport")?.to_owned(),
        authentication: manifest_value(values, "control-authentication")?.to_owned(),
        capabilities: parse_control_capabilities(manifest_value(values, "control-capabilities")?)?,
    };
    validate_control_contract(&control)?;
    require_manifest_value(values, "control-plane", &control.control_plane())?;
    if !is_sha256(manifest_value(values, "control-contract-sha256")?) {
        bail!("invalid SHA-256 value for Linux DVM control contract");
    }
    Ok(control)
}

fn validate_signed_module_kernel_config(path: &Path) -> Result<()> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read Linux DVM kernel configuration {}", path.display()))?;
    for required in [
        "CONFIG_MODULE_SIG=y",
        "CONFIG_MODULE_SIG_FORCE=y",
        "CONFIG_MODULE_SIG_ALL=y",
        "CONFIG_MODULE_SIG_SHA256=y",
        "CONFIG_MODULE_SIG_HASH=\"sha256\"",
        "CONFIG_MODULE_SIG_KEY=\"certs/signing_key.pem\"",
        "CONFIG_MODULE_SIG_KEY_TYPE_RSA=y",
    ] {
        if !source.lines().any(|line| line == required) {
            bail!(
                "Linux DVM kernel configuration {} lacks {required}",
                path.display()
            );
        }
    }
    Ok(())
}

fn parse_dvm_control_contract(path: &Path) -> Result<DvmControlContract> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("missing Linux DVM control contract {}", path.display()))?;
    parse_dvm_control_contract_text(&text, &path.display().to_string())
}

fn parse_dvm_control_contract_text(text: &str, source: &str) -> Result<DvmControlContract> {
    let values = parse_manifest_text(text, source)?;
    const REQUIRED_KEYS: [&str; 6] = [
        "CONTROL_SCHEMA",
        "CONTROL_PROTOCOL",
        "CONTROL_STATE",
        "CONTROL_TRANSPORT",
        "CONTROL_AUTHENTICATION",
        "CONTROL_CAPABILITIES",
    ];
    if values.len() != REQUIRED_KEYS.len()
        || values
            .keys()
            .any(|key| !REQUIRED_KEYS.contains(&key.as_str()))
    {
        bail!("unsupported Linux DVM control-contract key set in {source}");
    }
    let control = DvmControlContract {
        protocol: manifest_value(&values, "CONTROL_PROTOCOL")?.to_owned(),
        state: manifest_value(&values, "CONTROL_STATE")?.to_owned(),
        transport: manifest_value(&values, "CONTROL_TRANSPORT")?.to_owned(),
        authentication: manifest_value(&values, "CONTROL_AUTHENTICATION")?.to_owned(),
        capabilities: parse_control_capabilities(manifest_value(&values, "CONTROL_CAPABILITIES")?)?,
    };
    require_manifest_value(&values, "CONTROL_SCHEMA", "1")?;
    validate_control_contract(&control)?;
    Ok(control)
}

fn validate_control_contract(control: &DvmControlContract) -> Result<()> {
    if control.protocol != DVM_CONTROL_PROTOCOL
        || control.state != DVM_CONTROL_STATE
        || control.transport != DVM_CONTROL_TRANSPORT
        || control.authentication != DVM_CONTROL_AUTHENTICATION
        || control.capabilities.join(",") != DVM_CONTROL_CAPABILITIES
    {
        bail!(
            "unsupported Linux DVM control contract {}; expected {DVM_CONTROL_PROTOCOL}-{DVM_CONTROL_STATE} with transport={} authentication={} capabilities={}",
            control.control_plane(),
            DVM_CONTROL_TRANSPORT,
            DVM_CONTROL_AUTHENTICATION,
            DVM_CONTROL_CAPABILITIES,
        );
    }
    Ok(())
}

fn parse_control_capabilities(value: &str) -> Result<Vec<String>> {
    if value.is_empty() {
        bail!("Linux DVM control capabilities must not be empty");
    }
    let mut seen = BTreeSet::new();
    let mut capabilities = Vec::new();
    for capability in value.split(',') {
        if capability.is_empty()
            || capability.len() > 64
            || !capability
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || !seen.insert(capability)
        {
            bail!("invalid Linux DVM control capability {capability:?}");
        }
        capabilities.push(capability.to_owned());
    }
    Ok(capabilities)
}

pub(crate) fn validate_dvm_manifest_text_for_testinfra(text: &str) -> Result<()> {
    let values = parse_manifest_text(text, "fuzz input")?;
    let _control_plane = validate_manifest_values(&values)?;
    Ok(())
}

fn require_manifest_value(
    values: &BTreeMap<String, String>,
    key: &str,
    expected: &str,
) -> Result<()> {
    let actual = manifest_value(values, key)?;
    if actual != expected {
        bail!("unsupported Linux DVM manifest {key}={actual:?}; expected {expected:?}");
    }
    Ok(())
}

fn manifest_value<'a>(values: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("Linux DVM manifest is missing {key}"))
}

fn verify_manifest_hash(path: &Path, expected: &str) -> Result<()> {
    if !is_sha256(expected) {
        bail!("invalid SHA-256 value for {}", path.display());
    }
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "Linux DVM manifest hash mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> Result<String> {
    let sha256sum = resolve_command_path(OsStr::new("sha256sum"))
        .context("missing sha256sum required to verify Linux DVM artifacts")?;
    let output = Command::new(sha256sum)
        .arg(path)
        .output()
        .with_context(|| format!("failed to hash {}", path.display()))?;
    if !output.status.success() {
        bail!("sha256sum failed for {}", path.display());
    }
    let digest = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .context("sha256sum produced no digest")?
        .to_ascii_lowercase();
    if !is_sha256(&digest) {
        bail!(
            "sha256sum produced an invalid digest for {}",
            path.display()
        );
    }
    Ok(digest)
}
