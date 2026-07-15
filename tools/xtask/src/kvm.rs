use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc,
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use driver_domain_protocol::{
    DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET, DVM_GUI_SURFACE_SLOT_COUNT,
    DVM_INPUT_RING_APERTURE_BYTES, DVM_NET_APERTURE_BYTES, DvmGuiSurfaceMessage,
    DvmGuiSurfacePoolHeader, DvmInputRingHeader, DvmNetHeader,
};
use fatfs::Seek as FatSeek;
use fatfs::Write as FatWrite;
use fs_err as fs;
use rustos_driver_domain_host::{
    ControlContract as HostControlContract, ControlSecret, HostControlListener, InputRingSink,
    IvshmemDoorbellServer, ProbeResult,
};

use crate::Result;
use crate::config::Config;
use crate::util::{resolve_command_path, run_command};

const DVM_KERNEL: &str = "rustos-linux-dvm-x86_64.bzImage";
const DVM_ROOTFS: &str = "rustos-linux-dvm-x86_64.rootfs.cpio.xz";
const DVM_CONFIG: &str = "rustos-linux-dvm-x86_64.config";
const DVM_MANIFEST: &str = "rustos-linux-dvm-x86_64.manifest";
const DVM_MANIFEST_SCHEMA: &str = "4";
const DVM_CONTROL_CONTRACT: &str = "board/overlay/usr/share/rustos-dvm/control-plane-v1.env";
const DVM_CONTROL_PROTOCOL: &str = "agent-v1";
const DVM_CONTROL_STATE: &str = "control";
const DVM_CONTROL_TRANSPORT: &str = "kvm-vsock";
const DVM_CONTROL_AUTHENTICATION: &str = "dvm-agent-hmac-sha256-v1";
const DVM_CONTROL_CAPABILITIES: &str = "health,device-inventory,driver-inventory,input-stream";
const RUSTOS_BOOT_MARKER: &str = "rootd: core services ready, spawning initd via loaderd";
const DVM_KEYBOARD_INGRESS_MARKER: &str = "inputd: DVM keyboard ingress observed";
const DVM_POINTER_INGRESS_MARKER: &str = "inputd: DVM pointer ingress observed";
const DEFAULT_UI_FPS_ACTIVE_WINDOWS: usize = 3;
const MAX_UI_FPS_ACTIVE_WINDOWS: usize = 20;
// The end-to-end cursor contract is 60 accepted motion updates per second.
// Require at least 55 in every measured one-second window (over 90%) so a
// single timer boundary cannot fail an otherwise continuous 60 Hz stream.
const MIN_UI_FPS_INPUT_EVENTS: u64 = 55;
const MIN_UI_FPS_CURSOR_MOVES: u64 = 50;
const MAX_UI_INPUT_GAP_MS: u64 = 50;
const MIN_UI_CURSOR_SPAN: u64 = 96;
// Copying the immutable frame snapshot and completing the DRM damage update
// must leave meaningful headroom inside a 16.67 ms 60 Hz frame.
const MAX_DVM_DISPLAY_RELAY_US: u64 = 12_000;
const DVM_DISPLAY_WIDTH: u32 = 1600;
const DVM_DISPLAY_HEIGHT: u32 = 900;
const DVM_DISPLAY_REGION_BYTES: u64 = 32 * 1024 * 1024;
// QEMU maps the shared pixel backend as cacheable device memory at this
// reserved, 2 MiB-aligned guest-physical address in both guests. The ivshmem
// BAR carries only bounded control records and MSI-X doorbells.
const DVM_DISPLAY_PIXEL_PHYS_ADDR: u64 = 0x1_0000_0000;
const DVM_DISPLAY_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const INTERACTIVE_DISPLAY_READY_TIMEOUT: Duration = Duration::from_secs(15);
const INTERACTIVE_IDLE_TICKS: usize = 3;
const DVM_INPUT_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const DVM_INPUT_RELAY_SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Authentication remains a five-second setup gate, but a real RustOS input
/// consumer appears only after the policy services and uiserver are running.
/// Keep that distinct boot dependency bounded without falsely admitting the
/// transport-only MSI-X state.
// Match the public kvm-smoke maximum. The former 20-second private deadline
// could abort a valid boot before the caller's explicit --timeout 30 elapsed,
// even though UI readiness and proof windows were still within that bound.
const DVM_INPUT_POLICY_READY_TIMEOUT: Duration = Duration::from_secs(30);
// The RustOS MSI-X receive substrate deliberately rejects x2APIC until an
// interrupt-remapping implementation can supply a non-truncated destination
// ID. The KVM validation topology therefore pins the guest to xAPIC instead of
// weakening the kernel with a guessed x2APIC message format.
// RustOS admits TSC as a clocksource only when CPUID advertises invariant TSC.
// QEMU masks `invtsc` by default because it constrains live migration, even on
// a constant/nonstop-TSC KVM host. This local validation topology is not live
// migrated, so expose the host guarantee explicitly; HPET remains the guest's
// independent calibration/watchdog reference.
const RUSTOS_DVM_KVM_CPU: &str = "host,-x2apic,+invtsc";
const DVM_NET_REGION_BYTES: u64 = DVM_NET_APERTURE_BYTES;
// `sessiond` reads the desktop registry during its early bootstrap, while
// `runtimed` reads the launch registry later. Both must agree for a private
// KVM-only profiling override to reach the uiserver process.
const UISERVER_PROFILE_REGISTRY_PATHS: &[&str] = &[
    "system/registry/system/desktop-programs.tsv",
    "system/registry/system/runtime-launch-programs.tsv",
];
const UISERVER_PROFILE_DISABLED: &str = "RUSTOS_UI_PROFILE=0";
const UISERVER_PROFILE_ENABLED: &str = "RUSTOS_UI_PROFILE=1";
const UISERVER_BOOT_TRACE_DISABLED: &str = "RUSTOS_UI_BOOT_TRACE=0";
const UISERVER_BOOT_TRACE_ENABLED: &str = "RUSTOS_UI_BOOT_TRACE=1";
const NETPROBE_REGISTRY_PATHS: &[&str] = &[
    "system/registry/system/desktop-programs.tsv",
    "system/registry/system/runtime-launch-programs.tsv",
];
const NETPROBE_QEMU_DISABLED: &str = "RUSTOS_NETPROBE_QEMU=0";
const NETPROBE_QEMU_ENABLED: &str = "RUSTOS_NETPROBE_QEMU=1";
const NETPROBE_QEMU_REACHABLE_MARKER: &str = "netprobe: qemu gateway reachable";
const DVM_GUEST_CID: u32 = 4;
const VHOST_VSOCK_DEVICE: &str = "/dev/vhost-vsock";
const MAX_SMOKE_TIMEOUT: u64 = 30;

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
    runtime_disk: PathBuf,
    debugcon_log: PathBuf,
    rustos_serial_log: PathBuf,
    dvm_serial_log: PathBuf,
    rustos_stderr_log: PathBuf,
    dvm_stderr_log: PathBuf,
    dvm_input_ring: PathBuf,
    dvm_input_doorbell: PathBuf,
    dvm_control_secret: PathBuf,
    gui_dvm_surfaces: Option<PathBuf>,
    gui_dvm_pixels: Option<PathBuf>,
    dvm_display_doorbell: Option<PathBuf>,
    dvm_network_shmem: Option<PathBuf>,
}

#[derive(Debug)]
struct SmokeOptions {
    dry_run: bool,
    exercise_input: bool,
    exercise_network: bool,
    gui_dvm_surfaces: bool,
    dvm_network_shmem: bool,
    min_ui_fps: Option<u32>,
    ui_proof_windows: usize,
    timeout: Duration,
    expected_markers: Vec<String>,
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
    let artifacts = verify_dvm_artifacts(config)?;
    let qemu = require_qemu(config)?;
    let layout = prepare_layout(config, &options)?;

    if options.dry_run {
        println!(
            "xtask: KVM smoke inputs prepared in {}",
            layout.run_dir.display()
        );
        return Ok(());
    }

    require_vhost_vsock()?;
    let deadline = Instant::now() + options.timeout;
    let input_doorbell = start_dvm_input_doorbell(&layout)?;
    let input_relay_gate = Arc::new(AtomicBool::new(false));
    let control_relay = start_dvm_input_relay(
        config,
        DVM_INPUT_RELAY_SETUP_TIMEOUT,
        layout.dvm_input_doorbell.clone(),
        layout.dvm_input_ring.clone(),
        layout.dvm_control_secret.clone(),
        Arc::clone(&input_relay_gate),
    )?;
    let display_doorbell = start_dvm_display_doorbell(&layout)?;
    let guest_display = smoke_guest_display(&options)?;
    let (mut rustos, mut dvm) = spawn_guests(
        &qemu,
        config,
        &artifacts,
        &layout,
        &options,
        guest_display,
        display_doorbell.as_ref(),
        &input_doorbell,
        input_relay_gate,
    )?;
    let result: Result<ProbeResult> = (|| {
        let probe = wait_for_parallel_boot(
            &mut rustos,
            &mut dvm,
            &layout,
            &options,
            deadline,
            &control_relay,
        )?;
        Ok(probe)
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
/// Unlike `kvm-smoke`, this has no readiness deadline or success criteria: it
/// remains alive until the user closes the DVM QEMU window or interrupts it.
pub(crate) fn kvm_run_command(config: &Config) -> Result<()> {
    let started_at = Instant::now();
    let artifacts = verify_dvm_artifacts(config)?;
    log_kvm_start_phase("verified-dvm-artifacts", started_at);
    let qemu = require_qemu(config)?;
    log_kvm_start_phase("resolved-qemu", started_at);
    let options = SmokeOptions {
        dry_run: false,
        exercise_input: false,
        exercise_network: false,
        gui_dvm_surfaces: true,
        dvm_network_shmem: true,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        timeout: Duration::ZERO,
        expected_markers: Vec::new(),
    };
    let layout = prepare_layout(config, &options)?;
    log_kvm_start_phase("prepared-kvm-layout", started_at);
    require_vhost_vsock()?;
    log_kvm_start_phase("verified-vhost-vsock", started_at);
    let input_doorbell = start_dvm_input_doorbell(&layout)?;
    log_kvm_start_phase("started-input-doorbell", started_at);
    let input_relay_gate = Arc::new(AtomicBool::new(false));
    start_dvm_input_relay_unbounded(
        config,
        layout.dvm_input_doorbell.clone(),
        layout.dvm_input_ring.clone(),
        layout.dvm_control_secret.clone(),
        Arc::clone(&input_relay_gate),
    )?;
    log_kvm_start_phase("started-input-relay", started_at);
    let display_doorbell = start_dvm_display_doorbell(&layout)?;
    log_kvm_start_phase("started-display-doorbell", started_at);
    let (mut rustos, mut dvm) = spawn_guests(
        &qemu,
        config,
        &artifacts,
        &layout,
        &options,
        GuestDisplay::DvmGtk,
        display_doorbell.as_ref(),
        &input_doorbell,
        input_relay_gate,
    )?;
    log_kvm_start_phase("spawned-guests", started_at);

    if let Err(error) = wait_for_interactive_display(&layout, &mut rustos, &mut dvm) {
        stop_guest(&mut dvm);
        stop_guest(&mut rustos);
        return Err(error);
    }

    println!(
        "xtask: interactive KVM DVM display verified in {} ms; move the pointer into the Linux DVM window to record real-input acceptance evidence, then close it or press Ctrl-C to stop",
        started_at.elapsed().as_millis(),
    );
    let mut pointer_observed = false;
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
        let rustos_log = read_runtime_log_if_present(&layout.debugcon_log)?;
        if runtime_stall_or_crash_observed(&rustos_log) {
            stop_guest(&mut dvm);
            stop_guest(&mut rustos);
            bail!(
                "interactive KVM DVM session observed a RustOS watchdog, stall, or crash; inspect {}",
                layout.debugcon_log.display(),
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

pub(crate) fn print_kvm_smoke_help() {
    println!(
        "\
usage: cargo xtask kvm-smoke [options]

Boots the Linux DVM and RustOS concurrently with QEMU/KVM. This verifies the
host-authenticated Linux-DVM input relay endpoint and RustOS's dedicated
framed virtual input transport. It does not synthesize QMP input.

options:
  --timeout <seconds>  wait for readiness markers (1..={MAX_SMOKE_TIMEOUT}, default 30)
  --expect <marker>    require an additional RustOS debugcon marker (repeatable)
  --exercise-input     run the DVM's bounded evdev loopback self-test and require
                       RustOS inputd keyboard and pointer ingress markers
  --exercise-network   run netprobe through netd and the DVM Ethernet ring
  --min-ui-fps <fps>   enable the private KVM-only UI profiler and require three
                       high-volume input windows and three accepted DVM atomic-page-flip
                       relay samples at or above this integer FPS; this uses
                       QEMU GTK and therefore requires a host GUI session
  --ui-proof-windows <count>
                       require 3..={MAX_UI_FPS_ACTIVE_WINDOWS} consecutive one-second UI/input
                       and DVM relay samples (default {DEFAULT_UI_FPS_ACTIVE_WINDOWS}); requires
                       --min-ui-fps and supports bounded active soak proofs
  --gui-dvm-surfaces   enable the production fixed three-slot GUI-DVM ivshmem
                       pool; no V2 or native-GPU compatibility path is accepted
  --dvm-network-shmem  attach the bounded RustOS↔DVM Ethernet ring; RustOS keeps
                       no native virtio-net device in this topology
  --dry-run            validate inputs and prepare KVM log paths without launching QEMU
  -h, --help           show this help

The default proof requires RustOS to reach init handoff and the L0-style host
broker to complete an authenticated Linux-DVM health/inventory/input-stream
handshake. A real key requires a physical input source assigned to the DVM;
the default smoke command does not fabricate one. `--exercise-input` is an
explicit KVM-only self-test: its Linux agent writes a bounded uinput device,
then consumes it through its ordinary evdev relay. It never opens QMP or a
host-to-DVM input injection endpoint.
"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuestDisplay {
    Headless,
    DvmGtk,
}

fn qemu_display_backend(display: GuestDisplay) -> &'static str {
    match display {
        GuestDisplay::Headless => "none",
        // GTK captures keyboard focus on hover. Pointer input is supplied by
        // the absolute tablet below, so F5 never needs a manual mouse grab.
        // Force-hide the host cursor. Leaving this option unset delegates the
        // result to the frontend default and previously left the host pointer
        // visible over the guest UI on some GTK versions.
        //
        // The DVM relay uses atomic KMS page flips over 2D virtio-gpu and never
        // submits virgl commands. Keep the interactive path on QEMU's 2D
        // frontend: gtk,gl=on has had independent black-scanout regressions,
        // while adding no capability to this display contract.
        GuestDisplay::DvmGtk => {
            "gtk,gl=off,show-tabs=off,zoom-to-fit=off,grab-on-hover=on,show-cursor=off"
        }
    }
}

fn dvm_gpu_device(_display: GuestDisplay) -> String {
    format!(
        // The DVM display transport is deliberately fixed at 1600x900.
        // GTK's resize-aware EDID starts at its tiny bootstrap window and can
        // otherwise replace that mode with 640x480 before the Linux DRM relay
        // starts. Disable EDID so QEMU's explicit dimensions are authoritative.
        "virtio-gpu-pci,id=dvm-virtio-gpu,xres={},yres={},edid=off",
        DVM_DISPLAY_WIDTH, DVM_DISPLAY_HEIGHT
    )
}

fn dvm_pointer_device() -> &'static str {
    // An absolute tablet keeps host pointer motion available while the GTK
    // window is merely hovered. The DVM agent normalizes the tablet range to
    // the fixed 1600x900 scanout before it emits the authenticated RDI3 frame.
    "virtio-tablet-pci,id=dvm-pointer"
}

fn parse_smoke_options<I>(mut args: I) -> Result<SmokeOptions>
where
    I: Iterator<Item = String>,
{
    let mut options = SmokeOptions {
        dry_run: false,
        exercise_input: false,
        exercise_network: false,
        gui_dvm_surfaces: false,
        dvm_network_shmem: false,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        timeout: Duration::from_secs(MAX_SMOKE_TIMEOUT),
        expected_markers: vec![RUSTOS_BOOT_MARKER.to_owned()],
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => options.dry_run = true,
            "--exercise-input" => options.exercise_input = true,
            "--exercise-network" => options.exercise_network = true,
            "--gui-dvm-surfaces" => options.gui_dvm_surfaces = true,
            "--dvm-network-shmem" => options.dvm_network_shmem = true,
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
            unknown => bail!("unknown KVM smoke option: {unknown}"),
        }
    }

    if options.exercise_input {
        options
            .expected_markers
            .push(DVM_KEYBOARD_INGRESS_MARKER.to_owned());
        options
            .expected_markers
            .push(DVM_POINTER_INGRESS_MARKER.to_owned());
    }
    if options.exercise_network && !options.dvm_network_shmem {
        bail!("--exercise-network requires --dvm-network-shmem");
    }
    if options.ui_proof_windows != DEFAULT_UI_FPS_ACTIVE_WINDOWS && options.min_ui_fps.is_none() {
        bail!("--ui-proof-windows requires --min-ui-fps");
    }
    Ok(options)
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
    let artifact_dir = dvm_artifact_dir(config);
    let manifest_path = artifact_dir.join(DVM_MANIFEST);
    let values = parse_manifest(&manifest_path)?;
    let control = validate_manifest_values(&values)?;

    let kernel = artifact_dir.join(DVM_KERNEL);
    let rootfs = artifact_dir.join(DVM_ROOTFS);
    let build_config = artifact_dir.join(DVM_CONFIG);
    verify_manifest_hash(&kernel, manifest_value(&values, "kernel_sha256")?)?;
    verify_manifest_hash(&rootfs, manifest_value(&values, "rootfs_sha256")?)?;
    verify_manifest_hash(&build_config, manifest_value(&values, "config_sha256")?)?;
    verify_manifest_hash(
        &dvm_dir(config).join("sources.lock"),
        manifest_value(&values, "sources_lock_sha256")?,
    )?;
    let control_contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    verify_manifest_hash(
        &control_contract_path,
        manifest_value(&values, "control-contract-sha256")?,
    )?;
    let source_contract = parse_dvm_control_contract(&control_contract_path)?;
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
    require_manifest_value(values, "schema", DVM_MANIFEST_SCHEMA)?;
    require_manifest_value(values, "id", "rustos-linux-dvm-x86_64")?;
    require_manifest_value(values, "architecture", "x86_64")?;
    require_manifest_value(values, "boot", "linux-bzimage+cpio-xz")?;
    require_manifest_value(values, "data-plane", "hostd-input-ring-msix")?;
    for key in [
        "kernel_sha256",
        "rootfs_sha256",
        "config_sha256",
        "sources_lock_sha256",
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

fn parse_dvm_control_contract(path: &Path) -> Result<DvmControlContract> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("missing Linux DVM control contract {}", path.display()))?;
    parse_dvm_control_contract_text(&text, &path.display().to_string())
}

fn parse_dvm_control_contract_text(text: &str, source: &str) -> Result<DvmControlContract> {
    let values = parse_manifest_text(text, source)?;
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
            "unsupported Linux DVM control contract {}; expected {} with transport={} authentication={} capabilities={}",
            control.control_plane(),
            format!("{DVM_CONTROL_PROTOCOL}-{DVM_CONTROL_STATE}"),
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

fn prepare_layout(config: &Config, options: &SmokeOptions) -> Result<KvmLayout> {
    if !config.boot_disk_image.is_file() {
        bail!(
            "missing RustOS boot disk {}; run `cargo xtask build` first",
            config.boot_disk_image.display()
        );
    }
    if !config.ovmf_path.is_file() {
        bail!(
            "missing pinned OVMF firmware {}",
            config.ovmf_path.display()
        );
    }

    let run_dir = config.build_dir.join("kvm");
    fs::create_dir_all(&run_dir)?;
    fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "restrict KVM runtime directory permissions {}",
            run_dir.display()
        )
    })?;
    // Normal interactive KVM sessions do not alter the boot image. QEMU's
    // snapshot mode below protects it from guest writes, avoiding a full disk
    // copy on every F5. Only proof options that patch private boot content
    // receive a per-run image.
    let runtime_disk = if options.min_ui_fps.is_some() || options.exercise_network {
        let runtime_disk = run_dir.join("rustos-kvm.img");
        fs::copy(&config.boot_disk_image, &runtime_disk).with_context(|| {
            format!(
                "failed to create KVM runtime disk from {}",
                config.boot_disk_image.display()
            )
        })?;
        runtime_disk
    } else {
        config.boot_disk_image.clone()
    };
    if options.min_ui_fps.is_some() {
        enable_private_ui_profile(&runtime_disk)?;
    }
    if options.exercise_network {
        enable_private_network_exercise(&runtime_disk)?;
    }
    let gui_dvm_surfaces = if options.gui_dvm_surfaces {
        let path = run_dir.join("dvm-display.ivshmem");
        create_gui_dvm_surfaces(&path)?;
        Some(path)
    } else {
        None
    };
    let gui_dvm_pixels = if options.gui_dvm_surfaces {
        let path = run_dir.join("dvm-display.pmem");
        create_gui_dvm_surfaces(&path)?;
        Some(path)
    } else {
        None
    };
    let dvm_display_doorbell = gui_dvm_surfaces
        .as_ref()
        .map(|_| run_dir.join("dvm-display-doorbell.sock"));
    let dvm_network_shmem = if options.dvm_network_shmem {
        let path = run_dir.join("dvm-network.ivshmem");
        create_dvm_network_shmem(&path)?;
        Some(path)
    } else {
        None
    };

    let debugcon_log = run_dir.join("rustos-debugcon.log");
    let rustos_serial_log = run_dir.join("rustos-serial.log");
    let dvm_serial_log = run_dir.join("linux-dvm-serial.log");
    let rustos_stderr_log = run_dir.join("rustos-qemu.stderr.log");
    let dvm_stderr_log = run_dir.join("linux-dvm-qemu.stderr.log");
    let dvm_input_ring = run_dir.join("dvm-input.ivshmem");
    let dvm_input_doorbell = run_dir.join("dvm-input-doorbell.sock");
    let dvm_control_secret = run_dir.join("linux-dvm-control.secret");
    let control_secret = ControlSecret::random()?;
    fs::write(&dvm_control_secret, control_secret.as_hex())?;
    fs::set_permissions(&dvm_control_secret, std::fs::Permissions::from_mode(0o600))?;
    fs::write(&debugcon_log, "")?;
    fs::write(&rustos_serial_log, "")?;
    fs::write(&dvm_serial_log, "")?;
    fs::write(&rustos_stderr_log, "")?;
    fs::write(&dvm_stderr_log, "")?;
    create_dvm_input_ring(&dvm_input_ring)?;

    Ok(KvmLayout {
        run_dir,
        runtime_disk,
        debugcon_log,
        rustos_serial_log,
        dvm_serial_log,
        rustos_stderr_log,
        dvm_stderr_log,
        dvm_input_ring,
        dvm_input_doorbell,
        dvm_control_secret,
        gui_dvm_surfaces,
        gui_dvm_pixels,
        dvm_display_doorbell,
        dvm_network_shmem,
    })
}

fn create_dvm_input_ring(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM input-ring path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmInputRingHeader::new(DVM_INPUT_RING_APERTURE_BYTES, 1);
    if !header.is_valid() {
        bail!("refusing to create invalid fixed DVM input-ring header");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create DVM input-ring backing {}", path.display()))?;
    file.set_len(DVM_INPUT_RING_APERTURE_BYTES)
        .with_context(|| format!("size DVM input-ring backing {}", path.display()))?;
    file.write_all(&header.encode())
        .with_context(|| format!("write DVM input-ring header {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync DVM input-ring backing {}", path.display()))?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn create_dvm_network_shmem(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM shared-network path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmNetHeader::new(DVM_NET_REGION_BYTES, 1);
    if !header.is_valid() {
        bail!("refusing to create invalid DVM shared-network header");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    file.set_len(DVM_NET_REGION_BYTES)?;
    file.write_all(&header.encode())?;
    file.sync_all()?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Allocate the production GUI-DVM three-surface pool for the KVM topology.
/// The L0 runner creates every slot and control record before either guest
/// starts. Neither guest gets to select an address, slot count, or queue size.
fn create_gui_dvm_surfaces(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM shared-display path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmGuiSurfacePoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
    );
    if !header.is_valid() {
        bail!("refusing to create invalid GUI-DVM surface-pool header");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create DVM shared display {}", path.display()))?;
    file.set_len(DVM_DISPLAY_REGION_BYTES)
        .with_context(|| format!("size DVM shared display {}", path.display()))?;
    file.write_all(&header.encode())
        .with_context(|| format!("initialize DVM shared display {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush DVM shared display {}", path.display()))?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict DVM shared display permissions {}", path.display()))?;
    Ok(())
}

/// A successful GUI-DVM smoke must prove a valid host `PRESENT` record and
/// pixels in exactly the slot named by that record. This rejects a host-only
/// pool, a stale record, and pixels written outside the fixed slot capability.
fn verify_dvm_display_surface(control_path: &Path, pixel_path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(control_path)
        .with_context(|| format!("open DVM display control {}", control_path.display()))?;
    let mut encoded = [0_u8; DvmGuiSurfacePoolHeader::encoded_len()];
    file.read_exact(&mut encoded)
        .with_context(|| format!("read DVM display control header {}", control_path.display()))?;
    let header = DvmGuiSurfacePoolHeader::decode(&encoded)
        .context("GUI-DVM surface-pool header changed or became invalid during smoke")?;
    if header.region_bytes != DVM_DISPLAY_REGION_BYTES
        || header.width != DVM_DISPLAY_WIDTH
        || header.height != DVM_DISPLAY_HEIGHT
    {
        bail!(
            "GUI-DVM surface-pool header differs from launch contract: region={} width={} height={}",
            header.region_bytes,
            header.width,
            header.height
        );
    }
    let mut newest = None;
    for slot in 0..DVM_GUI_SURFACE_SLOT_COUNT {
        let offset = u64::try_from(DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET)?
            .checked_add(u64::from(slot) * DvmGuiSurfaceMessage::encoded_len() as u64)
            .context("GUI-DVM host record offset overflow")?;
        file.seek(SeekFrom::Start(offset))?;
        let mut record = [0_u8; DvmGuiSurfaceMessage::encoded_len()];
        file.read_exact(&mut record)?;
        let Some(message) = DvmGuiSurfaceMessage::decode(&record) else {
            continue;
        };
        if !message.is_valid_for_dimensions(header.width, header.height)
            || !matches!(
                message.kind,
                driver_domain_protocol::DvmGuiSurfaceMessageKind::Present
            )
            || message.slot != slot
        {
            bail!(
                "GUI-DVM host record {} is malformed or exceeds its capability",
                slot
            );
        }
        if newest
            .is_none_or(|existing: DvmGuiSurfaceMessage| existing.generation < message.generation)
        {
            newest = Some(message);
        }
    }
    let message = newest.context("GUI-DVM surface pool contains no host PRESENT record")?;
    let slot_offset = header
        .slot_offset(message.slot)
        .context("GUI-DVM PRESENT names an out-of-range slot")?;
    let mut pixel_file = std::fs::File::open(pixel_path)
        .with_context(|| format!("open DVM cacheable pixel pool {}", pixel_path.display()))?;
    pixel_file.seek(SeekFrom::Start(slot_offset))?;
    let mut remaining = header.slot_bytes;
    let mut block = [0_u8; 4096];
    let mut wrote_pixels = false;
    while remaining > 0 {
        let bytes = usize::try_from(remaining.min(block.len() as u64))?;
        pixel_file.read_exact(&mut block[..bytes])?;
        if block[..bytes].iter().any(|byte| *byte != 0) {
            wrote_pixels = true;
            break;
        }
        remaining -= bytes as u64;
    }
    if !wrote_pixels {
        bail!("GUI-DVM provider published a slot but RustOS wrote no pixels into it");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct DvmNetworkCounters {
    tx_producer: u32,
    tx_consumer: u32,
    rx_producer: u32,
    rx_consumer: u32,
    dvm_ready: bool,
}

impl DvmNetworkCounters {
    fn is_valid(self, slots: u32) -> bool {
        self.tx_producer.wrapping_sub(self.tx_consumer) <= slots
            && self.rx_producer.wrapping_sub(self.rx_consumer) <= slots
    }

    fn round_trip_observed(self) -> bool {
        self.tx_producer != 0
            && self.tx_consumer != 0
            && self.rx_producer != 0
            && self.rx_consumer != 0
    }
}

fn dvm_network_counters(path: &Path) -> Result<DvmNetworkCounters> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open DVM shared network {}", path.display()))?;
    let mut bytes = [0_u8; DvmNetHeader::encoded_len()];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read DVM shared network header {}", path.display()))?;
    let header = DvmNetHeader::decode(&bytes)
        .context("DVM shared network header changed or became invalid during smoke")?;
    let counters = DvmNetworkCounters {
        tx_producer: u32::from_le_bytes(bytes[40..44].try_into().expect("fixed counter offset")),
        tx_consumer: u32::from_le_bytes(bytes[44..48].try_into().expect("fixed counter offset")),
        rx_producer: u32::from_le_bytes(bytes[48..52].try_into().expect("fixed counter offset")),
        rx_consumer: u32::from_le_bytes(bytes[52..56].try_into().expect("fixed counter offset")),
        dvm_ready: header.dvm_ready(),
    };
    if !counters.is_valid(header.slot_count) {
        bail!(
            "DVM shared network counters violate bounded-ring invariant: tx={}/{} rx={}/{} slots={}",
            counters.tx_producer,
            counters.tx_consumer,
            counters.rx_producer,
            counters.rx_consumer,
            header.slot_count,
        );
    }
    Ok(counters)
}

fn verify_dvm_network_round_trip(path: &Path) -> Result<()> {
    let counters = dvm_network_counters(path)?;
    if !counters.round_trip_observed() {
        bail!(
            "DVM network exercise did not show bidirectional ring consumption: tx={}/{} rx={}/{}",
            counters.tx_producer,
            counters.tx_consumer,
            counters.rx_producer,
            counters.rx_consumer,
        );
    }
    Ok(())
}

fn enable_private_ui_profile(runtime_disk: &Path) -> Result<()> {
    replace_private_registry_anchor(
        runtime_disk,
        UISERVER_PROFILE_REGISTRY_PATHS,
        UISERVER_PROFILE_DISABLED,
        UISERVER_PROFILE_ENABLED,
        "UI profile",
    )?;
    replace_private_registry_anchor(
        runtime_disk,
        UISERVER_PROFILE_REGISTRY_PATHS,
        UISERVER_BOOT_TRACE_DISABLED,
        UISERVER_BOOT_TRACE_ENABLED,
        "UI boot trace",
    )
}

fn enable_private_network_exercise(runtime_disk: &Path) -> Result<()> {
    replace_private_registry_anchor(
        runtime_disk,
        NETPROBE_REGISTRY_PATHS,
        NETPROBE_QEMU_DISABLED,
        NETPROBE_QEMU_ENABLED,
        "network exercise",
    )
}

fn replace_private_registry_anchor(
    runtime_disk: &Path,
    paths: &[&str],
    disabled: &str,
    enabled: &str,
    feature: &str,
) -> Result<()> {
    if disabled.len() != enabled.len() {
        bail!("private KVM {feature} anchor changes length");
    }
    let disk = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(runtime_disk)
        .with_context(|| format!("open private KVM disk {}", runtime_disk.display()))?;
    let mut image = fatfs::StdIoWrapper::new(disk);
    image.seek(fatfs::SeekFrom::Start(0))?;
    let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new())?;
    {
        let root = fs.root_dir();
        for path in paths {
            let mut registry = root
                .open_file(path)
                .with_context(|| format!("open {path} for private KVM {feature}"))?;
            let mut contents = String::new();
            registry.read_to_string(&mut contents)?;
            let updated = contents.replacen(disabled, enabled, 1);
            if updated == contents || updated.len() != contents.len() {
                bail!("private KVM {feature} anchor missing or length-changing in {path}");
            }
            FatSeek::seek(&mut registry, fatfs::SeekFrom::Start(0))?;
            FatWrite::write_all(&mut registry, updated.as_bytes())?;
            FatWrite::flush(&mut registry)?;
        }
    }
    fs.unmount()?;
    Ok(())
}

fn require_qemu(config: &Config) -> Result<PathBuf> {
    resolve_command_path(&config.kvm_qemu_bin).with_context(|| {
        format!(
            "missing KVM QEMU command {}; install qemu-system-x86 or set KVM_QEMU_BIN",
            Path::new(&config.kvm_qemu_bin).display()
        )
    })
}

fn require_vhost_vsock() -> Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(VHOST_VSOCK_DEVICE)
        .with_context(|| {
            format!(
                "KVM DVM control requires read/write access to {VHOST_VSOCK_DEVICE}; grant the launch user access before running kvm-smoke"
            )
        })?;
    Ok(())
}

fn start_dvm_input_relay(
    config: &Config,
    timeout: Duration,
    input_doorbell: PathBuf,
    input_ring: PathBuf,
    control_secret_path: PathBuf,
    gate: Arc<AtomicBool>,
) -> Result<Receiver<Result<ProbeResult>>> {
    let contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    let contract = HostControlContract::from_env_file(&contract_path)?;
    let control_secret = ControlSecret::from_hex_file(&control_secret_path)?;
    let listener = HostControlListener::bind(DVM_GUEST_CID, contract, control_secret)?;
    // Preserve both the one-time readiness proof and a terminal relay error.
    // A one-slot channel can otherwise drop an error that races immediately
    // after readiness, turning a broken input stream into an unrelated timeout.
    let (sender, receiver) = mpsc::sync_channel(2);
    thread::spawn(move || {
        let sender_ready = sender.clone();
        let result = (|| {
            wait_for_input_relay_gate(&gate, timeout)?;
            let mut sink = InputRingSink::connect(&input_doorbell, &input_ring, timeout)?;
            listener.relay_input_once_with_ready(
                timeout,
                DVM_INPUT_POLICY_READY_TIMEOUT,
                &mut sink,
                |probe| {
                    sender_ready
                        .send(Ok(probe.clone()))
                        .context("report Linux DVM input relay readiness")
                },
            )
        })();
        if let Err(error) = result {
            let _ = sender.try_send(Err(error));
        }
    });
    Ok(receiver)
}

fn start_dvm_input_relay_unbounded(
    config: &Config,
    input_doorbell: PathBuf,
    input_ring: PathBuf,
    control_secret_path: PathBuf,
    gate: Arc<AtomicBool>,
) -> Result<()> {
    let contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    let contract = HostControlContract::from_env_file(&contract_path)?;
    let control_secret = ControlSecret::from_hex_file(&control_secret_path)?;
    let listener = HostControlListener::bind(DVM_GUEST_CID, contract, control_secret)?;
    thread::spawn(move || {
        while !gate.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        loop {
            let Ok(mut sink) =
                InputRingSink::connect(&input_doorbell, &input_ring, Duration::from_secs(1))
            else {
                thread::sleep(Duration::from_millis(100));
                continue;
            };
            if let Err(error) = listener.relay_input_once_unbounded(&mut sink) {
                eprintln!("xtask: interactive DVM input relay disconnected: {error:#}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    });
    Ok(())
}

fn wait_for_input_relay_gate(gate: &AtomicBool, timeout: Duration) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("input relay gate deadline overflow")?;
    while !gate.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            bail!("RustOS did not claim the fixed input ivshmem peer before deadline");
        }
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// The doorbell server owns the backing FD for the entire paired launch. It is
/// started before either QEMU process; `spawn_guests` then observes the RustOS
/// connection as peer 0 before it starts the GUI DVM, which becomes peer 1.
/// The device contract never lets either guest select an ID.
fn start_dvm_display_doorbell(layout: &KvmLayout) -> Result<Option<IvshmemDoorbellServer>> {
    let (Some(shared_display), Some(doorbell)) = (
        layout.gui_dvm_surfaces.as_deref(),
        layout.dvm_display_doorbell.as_deref(),
    ) else {
        return Ok(None);
    };
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(shared_display)
        .with_context(|| format!("open DVM display backing {}", shared_display.display()))?;
    Ok(Some(IvshmemDoorbellServer::start(doorbell, &backing)?))
}

/// The L0 input producer is the second fixed ivshmem peer. It is started
/// before RustOS, but not connected until `spawn_guests` proves that RustOS
/// claimed peer 0. The DVM itself never receives this aperture or a doorbell.
fn start_dvm_input_doorbell(layout: &KvmLayout) -> Result<IvshmemDoorbellServer> {
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&layout.dvm_input_ring)
        .with_context(|| {
            format!(
                "open DVM input-ring backing {}",
                layout.dvm_input_ring.display()
            )
        })?;
    IvshmemDoorbellServer::start_input(&layout.dvm_input_doorbell, &backing)
}

fn spawn_guests(
    qemu: &Path,
    config: &Config,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
    options: &SmokeOptions,
    guest_display: GuestDisplay,
    display_doorbell: Option<&IvshmemDoorbellServer>,
    input_doorbell: &IvshmemDoorbellServer,
    input_relay_gate: Arc<AtomicBool>,
) -> Result<(Child, Child)> {
    let mut rustos_command = Command::new(qemu);
    rustos_command
        .arg("-name")
        .arg("rustos-kvm")
        .args([
            "-machine",
            "q35,accel=kvm,hpet=on",
            "-cpu",
            RUSTOS_DVM_KVM_CPU,
            "-m",
            "2048M,maxmem=3G,slots=2",
            "-smp",
            "2",
        ])
        .arg("-bios")
        .arg(&config.ovmf_path)
        .arg("-drive")
        .arg(format!(
            "file={},format=raw,if=ide",
            layout.runtime_disk.display()
        ))
        // Keep the smoke headless. The normal topology exposes a direct
        // virtio-gpu test device; the DVM-display topology gets a separately
        // initialized, fixed-layout ivshmem aperture below.
        .args([
            "-display",
            "none",
            "-vga",
            "none",
            "-nic",
            "none",
            "-no-reboot",
            "-no-shutdown",
            "-snapshot",
        ])
        .arg("-chardev")
        .arg(format!(
            "file,id=debugcon,path={},append=off",
            layout.debugcon_log.display()
        ))
        .arg("-device")
        .arg("isa-debugcon,iobase=0xe9,chardev=debugcon")
        .arg("-chardev")
        .arg(format!(
            "file,id=serial,path={},append=off",
            layout.rustos_serial_log.display()
        ))
        .args(["-serial", "chardev:serial"]);
    append_dvm_input_doorbell(&mut rustos_command, &layout.dvm_input_doorbell);
    if let Some(doorbell) = layout.dvm_display_doorbell.as_deref() {
        append_dvm_display_doorbell(&mut rustos_command, doorbell);
        append_dvm_display_pixels(
            &mut rustos_command,
            layout
                .gui_dvm_pixels
                .as_deref()
                .context("GUI-DVM control exists without a pixel backend")?,
            false,
        );
    } else {
        rustos_command
            .arg("-device")
            .arg("virtio-gpu-pci,id=rustos-virtio-gpu,xres=1280,yres=800");
    }
    if let Some(shared_network) = layout.dvm_network_shmem.as_deref() {
        append_dvm_network_ivshmem(&mut rustos_command, shared_network);
    }
    append_fault_injection(config, &mut rustos_command);
    rustos_command
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(
            &layout.rustos_stderr_log,
        )?));
    let rustos = rustos_command
        .spawn()
        .context("failed to start RustOS QEMU/KVM guest")?;

    if let Err(error) = input_doorbell.wait_for_peer_count(1, DVM_INPUT_FIRST_PEER_TIMEOUT) {
        let mut rustos = rustos;
        stop_guest(&mut rustos);
        return Err(error)
            .context("RustOS did not claim fixed input ivshmem peer ID 0 before DVM launch");
    }
    input_relay_gate.store(true, Ordering::Release);

    if let Some(display_doorbell) = display_doorbell {
        if let Err(error) = display_doorbell.wait_for_peer_count(1, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
        {
            let mut rustos = rustos;
            stop_guest(&mut rustos);
            return Err(error).context("RustOS did not claim ivshmem peer ID 0 before DVM launch");
        }
    }

    let mut dvm_command = Command::new(qemu);
    let dvm_append = if options.exercise_input {
        "console=ttyS0 rustos.dvm.input-selftest=1"
    } else {
        "console=ttyS0"
    };
    let dvm_display = qemu_display_backend(guest_display);
    dvm_command
        .arg("-name")
        .arg("rustos-linux-dvm-kvm")
        .args([
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-m",
            "512M,maxmem=1G,slots=2",
            "-smp",
            "2",
        ])
        .arg("-kernel")
        .arg(&artifacts.kernel)
        .arg("-initrd")
        .arg(&artifacts.rootfs)
        .args([
            "-append",
            dvm_append,
            "-display",
            dvm_display,
            "-vga",
            "none",
            "-no-reboot",
            "-no-shutdown",
        ])
        .arg("-chardev")
        .arg(format!(
            "file,id=serial,path={},append=off",
            layout.dvm_serial_log.display()
        ))
        .args(["-serial", "chardev:serial"])
        .arg("-device")
        .arg(format!("vhost-vsock-pci,guest-cid={DVM_GUEST_CID}"))
        .arg("-fw_cfg")
        .arg(format!(
            "name=opt/rustos/dvm-control-secret,file={}",
            layout.dvm_control_secret.display()
        ))
        .arg("-device")
        .arg("virtio-keyboard-pci,id=dvm-keyboard")
        .arg("-device")
        .arg(dvm_pointer_device())
        .arg("-netdev")
        .arg("user,id=dvm-net")
        .arg("-device")
        .arg("virtio-net-pci,netdev=dvm-net,id=dvm-virtio-net,mac=52:54:00:12:34:56")
        .arg("-device")
        .arg(dvm_gpu_device(guest_display))
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&layout.dvm_stderr_log)?));
    if let Some(doorbell) = layout.dvm_display_doorbell.as_deref() {
        append_dvm_display_doorbell(&mut dvm_command, doorbell);
        append_dvm_display_pixels(
            &mut dvm_command,
            layout
                .gui_dvm_pixels
                .as_deref()
                .context("GUI-DVM control exists without a pixel backend")?,
            true,
        );
    }
    if let Some(shared_network) = layout.dvm_network_shmem.as_deref() {
        append_dvm_network_ivshmem(&mut dvm_command, shared_network);
    }
    let dvm = match dvm_command.spawn() {
        Ok(dvm) => dvm,
        Err(error) => {
            let mut rustos = rustos;
            stop_guest(&mut rustos);
            return Err(error).context("failed to start Linux DVM QEMU/KVM guest");
        }
    };
    Ok((rustos, dvm))
}

fn append_dvm_input_doorbell(command: &mut Command, socket_path: &Path) {
    command
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-input-doorbell,path={}",
            socket_path.display(),
        ))
        .arg("-device")
        .arg("ivshmem-doorbell,vectors=1,chardev=dvm-input-doorbell");
}

fn append_dvm_display_doorbell(command: &mut Command, socket_path: &Path) {
    command
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-display-doorbell,path={}",
            socket_path.display(),
        ))
        .arg("-device")
        .arg("ivshmem-doorbell,vectors=2,chardev=dvm-display-doorbell");
}

fn append_dvm_display_pixels(command: &mut Command, path: &Path, read_only: bool) {
    let mut backend = format!(
        "memory-backend-file,id=dvm-display-pixels,mem-path={},size={},share=on",
        path.display(),
        DVM_DISPLAY_REGION_BYTES
    );
    if read_only {
        backend.push_str(",readonly=on,rom=on");
    }
    command
        .arg("-object")
        .arg(backend)
        .arg("-device")
        .arg(format!(
            "virtio-pmem-pci,id=dvm-display-pmem,memdev=dvm-display-pixels,memaddr={DVM_DISPLAY_PIXEL_PHYS_ADDR}"
        ));
}

fn append_dvm_network_ivshmem(command: &mut Command, path: &Path) {
    command
        .arg("-object")
        .arg(format!(
            "memory-backend-file,id=dvm-network-shm,mem-path={},size={},share=on",
            path.display(),
            DVM_NET_REGION_BYTES
        ))
        .arg("-device")
        .arg("ivshmem-plain,memdev=dvm-network-shm");
}

fn append_fault_injection(config: &Config, command: &mut Command) {
    if !config.project.fault_injection.enabled {
        return;
    }
    let payload = config.project.fault_injection.rules.join(";");
    if !payload.is_empty() {
        command
            .arg("-fw_cfg")
            .arg(format!("name=opt/rustos/fault-injection,string={payload}"));
    }
}

fn wait_for_parallel_boot(
    rustos: &mut Child,
    dvm: &mut Child,
    layout: &KvmLayout,
    options: &SmokeOptions,
    deadline: Instant,
    control_relay: &Receiver<Result<ProbeResult>>,
) -> Result<ProbeResult> {
    let mut control_ready = None;
    loop {
        check_guest_running(rustos, "RustOS", &layout.rustos_stderr_log)?;
        check_guest_running(dvm, "Linux DVM", &layout.dvm_stderr_log)?;
        let rustos_log = fs::read_to_string(&layout.debugcon_log)?;
        let dvm_log = fs::read_to_string(&layout.dvm_serial_log)?;
        let rustos_ready = options
            .expected_markers
            .iter()
            .all(|marker| rustos_log.contains(marker));
        match control_relay.try_recv() {
            Ok(Ok(probe)) => {
                if control_ready.replace(probe).is_some() {
                    bail!("Linux DVM input relay reported readiness more than once");
                }
            }
            Ok(Err(error)) => {
                if control_ready.is_some() {
                    bail!("Linux DVM input relay failed after readiness: {error:#}");
                }
                bail!("Linux DVM input relay failed before readiness: {error:#}");
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) if control_ready.is_none() => {
                bail!("Linux DVM host input relay terminated without a readiness result")
            }
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
        let ui_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            uiserver_profile_meets_fps(&rustos_log, minimum, options.ui_proof_windows)
                && uiserver_profile_input_pipeline_healthy(&rustos_log, options.ui_proof_windows)
                && !runtime_stall_or_crash_observed(&rustos_log)
                && (!options.gui_dvm_surfaces
                    || (dvm_display_relay_meets_fps(&dvm_log, minimum, options.ui_proof_windows)
                        && !runtime_stall_or_crash_observed(&dvm_log)))
        });
        let dvm_display_ready = !options.gui_dvm_surfaces
            || (dvm_display_provider_ready(&rustos_log) && dvm_display_relay_ready(&dvm_log));
        let dvm_network = if options.dvm_network_shmem {
            let shared_network = layout
                .dvm_network_shmem
                .as_deref()
                .context("network mode lost its shared DVM network aperture")?;
            Some(dvm_network_counters(shared_network)?)
        } else {
            None
        };
        let dvm_network_ready = dvm_network.is_none_or(|state| state.dvm_ready);
        let dvm_network_traffic_ready = !options.exercise_network
            || (dvm_network.is_some_and(DvmNetworkCounters::round_trip_observed)
                && rustos_log.contains(NETPROBE_QEMU_REACHABLE_MARKER));
        if rustos_ready
            && control_ready.is_some()
            && ui_fps_ready
            && dvm_display_ready
            && dvm_network_ready
            && dvm_network_traffic_ready
        {
            return Ok(control_ready.expect("checked above"));
        }
        if Instant::now() >= deadline {
            let missing_rustos = options
                .expected_markers
                .iter()
                .filter(|marker| !rustos_log.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            bail!(
                "KVM parallel boot did not reach readiness within {:?}; RustOS missing={:?}; ui-fps-ready={}; dvm-display-ready={}; dvm-network-ready={}; dvm-network-traffic-ready={}; host-input-relay-pending={}; inspect {}, {}, {}, and {}",
                options.timeout,
                missing_rustos,
                ui_fps_ready,
                dvm_display_ready,
                dvm_network_ready,
                dvm_network_traffic_ready,
                control_ready.is_none(),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
                layout.rustos_stderr_log.display(),
                layout.dvm_stderr_log.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// The kernel's bootstrap trace intentionally does not promise runtime
/// debugcon delivery. The userspace display-info ABI is the authoritative
/// observation: the runner's fixed ivshmem header must emerge unchanged as the
/// active primary display provider.
fn dvm_display_provider_ready(log: &str) -> bool {
    log.lines().any(|line| {
        let Some((_, fields)) = line.split_once("uiserver: display_get_info ") else {
            return false;
        };
        uiserver_display_field_is(fields, "width", DVM_DISPLAY_WIDTH)
            && uiserver_display_field_is(fields, "height", DVM_DISPLAY_HEIGHT)
            && uiserver_display_field_is(fields, "stride", DVM_DISPLAY_WIDTH * 4)
            && uiserver_display_field_is(fields, "bpp", 4)
            && uiserver_display_field_is(fields, "fmt", 1)
            // A DVM scanout is still the active primary provider. Requiring
            // both provenance bits prevents the smoke from accepting either a
            // generic primary framebuffer or a non-primary DVM aperture.
            && fields
                .split_whitespace()
                .any(|field| field == "flags=0x6")
    })
}

fn dvm_display_relay_ready(log: &str) -> bool {
    let active = log.lines().any(|line| {
        let has_interrupt = line
            .split_whitespace()
            .find_map(|field| field.strip_prefix("irq_count="))
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|count| count > 0);
        line.contains("rustos-dvm-display: active")
            && line.contains(&format!("width={DVM_DISPLAY_WIDTH}"))
            && line.contains(&format!("height={DVM_DISPLAY_HEIGHT}"))
            && line.contains(&format!("stride={}", DVM_DISPLAY_WIDTH * 4))
            && line.contains("event=ivshmem-msix-uio")
            && has_interrupt
            && line.contains("format=BGRA8888")
            && line.contains("cacheable-atomic-scanout=")
            && line.contains("atomic-pageflip-fence=1")
            && line.contains("scanout_buffers=3")
    });
    active
        && log.lines().any(|line| {
            line.contains(
                "rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio",
            )
        })
}

fn read_runtime_log_if_present(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(log) => Ok(log),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read runtime log {}", path.display())),
    }
}

fn uiserver_idle_ticks_healthy(log: &str, required_ticks: usize) -> bool {
    let mut consecutive = 0_usize;
    for line in log.lines() {
        let Some((_, fields)) = line.split_once("uiserver: update tick ") else {
            continue;
        };
        let healthy = [
            "backlog=false",
            "input_drops=0",
            "input_slow=0",
            "input_errors=0",
        ]
        .into_iter()
        .all(|field| fields.split_whitespace().any(|observed| observed == field));
        if healthy {
            consecutive = consecutive.saturating_add(1);
            if consecutive >= required_ticks {
                return true;
            }
        } else {
            consecutive = 0;
        }
    }
    false
}

fn wait_for_interactive_display(
    layout: &KvmLayout,
    rustos: &mut Child,
    dvm: &mut Child,
) -> Result<()> {
    let deadline = Instant::now() + INTERACTIVE_DISPLAY_READY_TIMEOUT;
    let mut last_surface_error = None;
    loop {
        if let Some(status) = rustos
            .try_wait()
            .context("poll RustOS QEMU during interactive display startup")?
        {
            bail!("RustOS QEMU exited before interactive display readiness with {status}");
        }
        if let Some(status) = dvm
            .try_wait()
            .context("poll Linux DVM QEMU during interactive display startup")?
        {
            bail!("Linux DVM QEMU exited before interactive display readiness with {status}");
        }
        let rustos_log = read_runtime_log_if_present(&layout.debugcon_log)?;
        let dvm_log = read_runtime_log_if_present(&layout.dvm_serial_log)?;
        if runtime_stall_or_crash_observed(&rustos_log) || runtime_stall_or_crash_observed(&dvm_log)
        {
            bail!(
                "interactive display startup observed a watchdog, stall, crash, or relay stop; inspect {} and {}",
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
            );
        }
        let surface_ready = match (
            layout.gui_dvm_surfaces.as_deref(),
            layout.gui_dvm_pixels.as_deref(),
        ) {
            (Some(control), Some(pixels)) => match verify_dvm_display_surface(control, pixels) {
                Ok(()) => true,
                Err(error) => {
                    last_surface_error = Some(error.to_string());
                    false
                }
            },
            _ => false,
        };
        if dvm_display_provider_ready(&rustos_log)
            && dvm_display_relay_ready(&dvm_log)
            && surface_ready
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "interactive display was not proven ready within {:?}; surface={}; inspect {} and {}",
                INTERACTIVE_DISPLAY_READY_TIMEOUT,
                last_surface_error.unwrap_or_else(|| "no valid PRESENT yet".to_owned()),
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn validate_interactive_session(layout: &KvmLayout, pointer_observed: bool) -> Result<()> {
    let rustos_log = read_runtime_log_if_present(&layout.debugcon_log)?;
    let dvm_log = read_runtime_log_if_present(&layout.dvm_serial_log)?;
    if runtime_stall_or_crash_observed(&rustos_log) || runtime_stall_or_crash_observed(&dvm_log) {
        bail!(
            "interactive KVM DVM acceptance found a watchdog, stall, crash, or relay stop; inspect {} and {}",
            layout.debugcon_log.display(),
            layout.dvm_serial_log.display(),
        );
    }
    if !dvm_display_provider_ready(&rustos_log) || !dvm_display_relay_ready(&dvm_log) {
        bail!(
            "interactive KVM DVM acceptance lacks the active atomic GUI-DVM display contract; inspect {} and {}",
            layout.debugcon_log.display(),
            layout.dvm_serial_log.display(),
        );
    }
    verify_dvm_display_surface(
        layout
            .gui_dvm_surfaces
            .as_deref()
            .context("interactive session lost GUI-DVM control backing")?,
        layout
            .gui_dvm_pixels
            .as_deref()
            .context("interactive session lost GUI-DVM pixel backing")?,
    )?;
    if !uiserver_idle_ticks_healthy(&rustos_log, INTERACTIVE_IDLE_TICKS) {
        bail!(
            "interactive KVM DVM acceptance lacks {} consecutive healthy idle update ticks; inspect {}",
            INTERACTIVE_IDLE_TICKS,
            layout.debugcon_log.display(),
        );
    }
    if !pointer_observed || !rustos_log.contains(DVM_POINTER_INGRESS_MARKER) {
        bail!(
            "interactive KVM DVM acceptance did not observe a real absolute-pointer event; move the host pointer over the DVM window before closing it"
        );
    }
    println!(
        "xtask: interactive KVM DVM acceptance passed (atomic display, non-black source frame, healthy idle ticks, real pointer ingress)"
    );
    Ok(())
}

fn dvm_display_relay_meets_fps(log: &str, minimum_fps: u32, required_windows: usize) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    let mut consecutive_samples = 0_usize;
    for line in log.lines() {
        let Some((_, fields)) = line.split_once("rustos-dvm-display: stats ") else {
            continue;
        };
        let pageflip_submissions = fields.split_whitespace().find_map(|field| {
            field
                .strip_prefix("pageflip_submissions=")
                .and_then(|value| value.parse::<u64>().ok())
        });
        let frame_hz_milli = fields.split_whitespace().find_map(|field| {
            field
                .strip_prefix("frame_hz_milli=")
                .and_then(|value| value.parse::<u64>().ok())
        });
        let copy_us = log_u64(fields, "copy_us_avg");
        let atomic_commit_us = log_u64(fields, "atomic_commit_us_avg");
        let Some((pageflip_submissions, frame_hz_milli, copy_us, atomic_commit_us)) =
            pageflip_submissions
                .zip(frame_hz_milli)
                .zip(copy_us.zip(atomic_commit_us))
                .map(|((submissions, hz), (copy, commit))| (submissions, hz, copy, commit))
        else {
            continue;
        };
        if pageflip_submissions == 0
            || frame_hz_milli < required_milli
            || copy_us.saturating_add(atomic_commit_us) > MAX_DVM_DISPLAY_RELAY_US
        {
            consecutive_samples = 0;
            continue;
        }
        consecutive_samples = consecutive_samples.saturating_add(1);
        if consecutive_samples >= required_windows {
            return true;
        }
    }
    false
}

fn log_u64(fields: &str, name: &str) -> Option<u64> {
    fields.split_whitespace().find_map(|field| {
        field
            .strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            .and_then(|value| value.parse::<u64>().ok())
    })
}

fn log_point(fields: &str, name: &str) -> Option<(u64, u64)> {
    let value = fields
        .split_whitespace()
        .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))?;
    let (x, y) = value.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

#[derive(Clone, Copy, Debug)]
struct UiProfileInputWindow {
    input_events: u64,
    backlog: u64,
    cursor_moves: u64,
    input_gap_ms: u64,
    input_last_age_ms: u64,
    input_drops: u64,
    input_slow: u64,
    input_errors: u64,
    cursor_mismatches: u64,
    cursor_x: u64,
    cursor_y: u64,
    presented_cursor_x: u64,
    presented_cursor_y: u64,
    background_thread_demotions: u64,
}

fn parse_ui_profile_input_window(line: &str) -> Option<UiProfileInputWindow> {
    let (_, fields) = line.split_once("uiserver profile: ")?;
    let (cursor_x, cursor_y) = log_point(fields, "cursor")?;
    let (presented_cursor_x, presented_cursor_y) = log_point(fields, "presented_cursor")?;
    Some(UiProfileInputWindow {
        input_events: log_u64(fields, "input_events")?,
        backlog: log_u64(fields, "backlog")?,
        cursor_moves: log_u64(fields, "cursor_moves")?,
        input_gap_ms: log_u64(fields, "input_gap_ms")?,
        input_last_age_ms: log_u64(fields, "input_last_age_ms")?,
        input_drops: log_u64(fields, "input_drops")?,
        input_slow: log_u64(fields, "input_slow")?,
        input_errors: log_u64(fields, "input_errors")?,
        cursor_mismatches: log_u64(fields, "cursor_mismatches")?,
        cursor_x,
        cursor_y,
        presented_cursor_x,
        presented_cursor_y,
        background_thread_demotions: log_u64(fields, "background_thread_demotions")?,
    })
}

fn uiserver_profile_input_pipeline_healthy(log: &str, required_windows: usize) -> bool {
    let mut windows = Vec::new();
    for window in log.lines().filter_map(parse_ui_profile_input_window) {
        if window.input_events < MIN_UI_FPS_INPUT_EVENTS
            || window.cursor_moves < MIN_UI_FPS_CURSOR_MOVES
            || window.backlog != 0
            || window.input_gap_ms > MAX_UI_INPUT_GAP_MS
            || window.input_last_age_ms > MAX_UI_INPUT_GAP_MS
            || window.input_drops != 0
            || window.input_slow != 0
            || window.input_errors != 0
            || window.cursor_mismatches != 0
            || window.background_thread_demotions == 0
            || window.cursor_x != window.presented_cursor_x
            || window.cursor_y != window.presented_cursor_y
        {
            windows.clear();
            continue;
        }
        windows.push(window);
        if windows.len() > required_windows {
            windows.remove(0);
        }
        if windows.len() == required_windows {
            let min_x = windows
                .iter()
                .map(|window| window.cursor_x)
                .min()
                .unwrap_or(0);
            let max_x = windows
                .iter()
                .map(|window| window.cursor_x)
                .max()
                .unwrap_or(0);
            let min_y = windows
                .iter()
                .map(|window| window.cursor_y)
                .min()
                .unwrap_or(0);
            let max_y = windows
                .iter()
                .map(|window| window.cursor_y)
                .max()
                .unwrap_or(0);
            if max_x.saturating_sub(min_x) >= MIN_UI_CURSOR_SPAN
                && max_y.saturating_sub(min_y) >= MIN_UI_CURSOR_SPAN
            {
                return true;
            }
        }
    }
    false
}

fn runtime_stall_or_crash_observed(log: &str) -> bool {
    const FAILURES: &[&str] = &[
        "uiserver watchdog panic:",
        "uiserver input watchdog panic:",
        "uiserver panic:",
        "scheduler long ready wait:",
        "scheduler stall:",
        "rustos-dvm-display: relay stopped",
        "[drm:virtio_gpu_dequeue_ctrl_func] *ERROR*",
        "panicked at ",
        "fatal runtime error",
        "BUG:",
    ];
    log.lines()
        .any(|line| FAILURES.iter().any(|failure| line.contains(failure)))
}

fn uiserver_display_field_is(fields: &str, name: &str, expected: u32) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .split_once('=')
            .is_some_and(|(key, value)| key == name && value == expected.to_string())
    })
}

fn uiserver_profile_meets_fps(log: &str, minimum_fps: u32, required_windows: usize) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    let active_windows = log.lines().filter_map(|line| {
        // Service logs normally carry an observability prefix, while early
        // debugcon output may be bare. The KVM gate accepts either form but
        // still requires the exact profile payload.  An idle desktop has no
        // presents by design, so only a window that actually processed input
        // is an FPS sample.
        line.split_once("uiserver profile: ")
            .map(|(_, profile)| profile)
            .and_then(|fields| {
                let input_events = fields.split_whitespace().find_map(|field| {
                    field
                        .strip_prefix("input_events=")
                        .and_then(|value| value.parse::<u64>().ok())
                })?;
                let frame_hz_milli = fields.split_whitespace().find_map(|field| {
                    field
                        .strip_prefix("frame_hz_milli=")
                        .and_then(|value| value.parse::<u64>().ok())
                })?;
                (input_events >= MIN_UI_FPS_INPUT_EVENTS).then_some(frame_hz_milli)
            })
    });
    let mut count = 0_usize;
    for frame_hz_milli in active_windows {
        if frame_hz_milli < required_milli {
            count = 0;
            continue;
        }
        count = count.saturating_add(1);
        if count >= required_windows {
            return true;
        }
    }
    false
}

fn validate_ui_fps_proof(layout: &KvmLayout, options: &SmokeOptions) -> Result<()> {
    let Some(minimum_fps) = options.min_ui_fps else {
        return Ok(());
    };
    let log = fs::read_to_string(&layout.debugcon_log)?;
    if !uiserver_profile_meets_fps(&log, minimum_fps, options.ui_proof_windows) {
        bail!(
            "KVM UI FPS proof failed after guest shutdown: require {} high-volume input windows at or above {} FPS; inspect {}",
            options.ui_proof_windows,
            minimum_fps,
            layout.debugcon_log.display(),
        );
    }
    if uiserver_has_interactive_slow_loop(&log) {
        bail!(
            "KVM UI FPS proof found an interactive slow uiserver loop; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if !uiserver_profile_input_pipeline_healthy(&log, options.ui_proof_windows) {
        bail!(
            "KVM UI proof found input loss, backlog, excessive input gap, cursor/present mismatch, or insufficient cursor travel; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if runtime_stall_or_crash_observed(&log) {
        bail!(
            "KVM UI proof found a uiserver/scheduler watchdog, stall, or crash marker; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if options.gui_dvm_surfaces {
        let dvm_log = fs::read_to_string(&layout.dvm_serial_log)?;
        if !dvm_display_relay_meets_fps(&dvm_log, minimum_fps, options.ui_proof_windows) {
            bail!(
                "KVM UI FPS proof failed after guest shutdown: require {} external DVM atomic-page-flip relay samples at or above {} FPS; inspect {}",
                options.ui_proof_windows,
                minimum_fps,
                layout.dvm_serial_log.display(),
            );
        }
        if runtime_stall_or_crash_observed(&dvm_log) {
            bail!(
                "KVM UI proof found a DVM display relay crash marker; inspect {}",
                layout.dvm_serial_log.display(),
            );
        }
    }
    Ok(())
}

fn uiserver_has_interactive_slow_loop(log: &str) -> bool {
    log.lines().any(|line| {
        let Some((_, fields)) = line.split_once("uiserver: slow loop ") else {
            return false;
        };
        uiserver_log_field_is_nonzero(fields, "console_windows")
            || uiserver_log_field_is_nonzero(fields, "wayland_windows")
    })
}

fn uiserver_log_field_is_nonzero(fields: &str, name: &str) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .split_once('=')
            .and_then(|(key, value)| (key == name).then_some(value))
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value != 0)
    })
}

fn check_guest_running(guest: &mut Child, label: &str, stderr_log: &Path) -> Result<()> {
    if let Some(status) = guest.try_wait()? {
        bail!(
            "{label} QEMU/KVM guest exited before readiness with {status}; inspect {}",
            stderr_log.display()
        );
    }
    Ok(())
}

fn stop_guest(guest: &mut Child) {
    if guest.try_wait().ok().flatten().is_none() {
        let _ = guest.kill();
    }
    let _ = guest.wait();
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_UI_FPS_ACTIVE_WINDOWS, DVM_CONTROL_AUTHENTICATION, DVM_CONTROL_CAPABILITIES,
        DVM_CONTROL_PROTOCOL, DVM_CONTROL_STATE, DVM_CONTROL_TRANSPORT,
        DVM_KEYBOARD_INGRESS_MARKER, DVM_POINTER_INGRESS_MARKER, DvmNetworkCounters, GuestDisplay,
        RUSTOS_BOOT_MARKER, dvm_display_provider_ready, dvm_display_relay_meets_fps,
        dvm_display_relay_ready, dvm_gpu_device, dvm_pointer_device, is_sha256,
        parse_dvm_control_contract_text, parse_manifest_text, parse_smoke_options,
        qemu_display_backend, runtime_stall_or_crash_observed, select_smoke_guest_display,
        uiserver_has_interactive_slow_loop, uiserver_idle_ticks_healthy,
        uiserver_profile_input_pipeline_healthy, uiserver_profile_meets_fps,
        validate_manifest_values,
    };

    #[test]
    fn smoke_timeout_is_bounded() {
        let options =
            parse_smoke_options(vec!["--timeout".into(), "30".into()].into_iter()).unwrap();
        assert_eq!(options.timeout.as_secs(), 30);
        assert_eq!(
            options.expected_markers,
            vec![RUSTOS_BOOT_MARKER.to_owned()]
        );
        assert!(parse_smoke_options(vec!["--timeout".into(), "31".into()].into_iter()).is_err());
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
    fn dvm_display_mode_requires_the_observed_display_contract() {
        let options = parse_smoke_options(vec!["--gui-dvm-surfaces".into()].into_iter()).unwrap();
        assert!(options.gui_dvm_surfaces);
        assert!(dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=6400 bpp=4 fmt=1 flags=0x6 gen=1"
        ));
        assert!(!dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=6400 bpp=4 fmt=1 flags=0x2 gen=1"
        ));
        assert!(dvm_display_relay_ready(
            "rustos-dvm-display: peer readiness sent event=ivshmem-msix-uio\n\
             rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n\
             rustos-dvm-display: active width=1600 height=900 stride=6400 format=BGRA8888 event=ivshmem-msix-uio irq_count=1 cacheable-atomic-scanout=1600x900 atomic-pageflip-fence=1 scanout_buffers=3"
        ));
        assert!(!dvm_display_relay_ready(
            "rustos-dvm-display: active width=1600 height=900 stride=6400 format=BGRA8888 cacheable-atomic-scanout"
        ));
        assert!(!dvm_display_relay_ready(
            "rustos-dvm-display: peer readiness sent event=ivshmem-msix-uio\n\
             rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio\n\
             rustos-dvm-display: active width=1600 height=900 stride=6400 format=BGRA8888 event=ivshmem-msix-uio irq_count=0 cacheable-atomic-scanout=1600x900 atomic-pageflip-fence=1 scanout_buffers=3"
        ));
    }

    #[test]
    fn dvm_network_mode_is_explicit() {
        let options = parse_smoke_options(vec!["--dvm-network-shmem".into()].into_iter()).unwrap();
        assert!(options.dvm_network_shmem);
        let exercised = parse_smoke_options(
            vec!["--dvm-network-shmem".into(), "--exercise-network".into()].into_iter(),
        )
        .unwrap();
        assert!(exercised.exercise_network);
        assert!(parse_smoke_options(vec!["--exercise-network".into()].into_iter()).is_err());
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
        assert!(
            parse_smoke_options(vec!["--ui-proof-windows".into(), "15".into()].into_iter())
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
        assert!(dvm_display_relay_meets_fps(
            "rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60001 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000",
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!dvm_display_relay_meets_fps(
            "rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=59999 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000",
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!dvm_display_relay_meets_fps(
            "rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=8000 atomic_commit_us_avg=5000\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000\n\
             rustos-dvm-display: stats elapsed_ms=1000 frame_hz_milli=60000 pageflip_submissions=60 copy_us_avg=3000 atomic_commit_us_avg=1000",
            60,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
    }

    #[test]
    fn ui_profile_gate_rejects_tremble_loss_and_stalls() {
        let healthy = "uiserver profile: elapsed_ms=1000 frame_hz_milli=60000 input_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor=800,450 presented_cursor=800,450 background_thread_demotions=7 backlog=0 cursor_moves=60\n\
uiserver profile: elapsed_ms=1000 frame_hz_milli=60000 input_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor=992,450 presented_cursor=992,450 background_thread_demotions=7 backlog=0 cursor_moves=60\n\
uiserver profile: elapsed_ms=1000 frame_hz_milli=60000 input_events=60 input_gap_ms=20 input_last_age_ms=5 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor=992,642 presented_cursor=992,642 background_thread_demotions=7 backlog=0 cursor_moves=60";
        assert!(uiserver_profile_input_pipeline_healthy(
            healthy,
            DEFAULT_UI_FPS_ACTIVE_WINDOWS
        ));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &healthy.replace("cursor=992,642", "cursor=803,452"),
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(!uiserver_profile_input_pipeline_healthy(
            &healthy.replace("input_drops=0", "input_drops=1"),
            DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        ));
        assert!(runtime_stall_or_crash_observed(
            "scheduler long ready wait: task=uiserver elapsed_ms=500"
        ));
        assert!(runtime_stall_or_crash_observed(
            "[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1200 (command 0x105)"
        ));
        assert!(!runtime_stall_or_crash_observed(
            "uiserver: panic hook installed"
        ));
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
    fn ui_fps_gate_ignores_pre_window_slow_loop_but_not_interactive_one() {
        assert!(!uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=72 wayland_ms=70 console_windows=0 wayland_windows=0"
        ));
        assert!(uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=72 wayland_ms=70 console_windows=1 wayland_windows=0"
        ));
        assert!(uiserver_has_interactive_slow_loop(
            "uiserver: slow loop iter_ms=72 wayland_ms=70 console_windows=0 wayland_windows=1"
        ));
    }

    #[test]
    fn interactive_gtk_display_uses_absolute_pointer_and_hides_host_cursor() {
        assert_eq!(qemu_display_backend(GuestDisplay::Headless), "none");
        assert_eq!(
            qemu_display_backend(GuestDisplay::DvmGtk),
            "gtk,gl=off,show-tabs=off,zoom-to-fit=off,grab-on-hover=on,show-cursor=off"
        );
        assert!(dvm_gpu_device(GuestDisplay::DvmGtk).starts_with("virtio-gpu-pci,id="));
        assert_eq!(dvm_pointer_device(), "virtio-tablet-pci,id=dvm-pointer");
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
    }

    #[test]
    fn dvm_input_selftest_keeps_pointer_selection_and_one_keyboard_probe() {
        let source = include_str!(
            "../../../driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c"
        );
        assert!(source.contains("UI_SET_KEYBIT, BTN_LEFT"));
        assert!(source.contains("UI_SET_ABSBIT, ABS_X"));
        assert!(source.contains("UI_SET_ABSBIT, ABS_Y"));
        assert!(source.contains("selftest->motion_phase == 0U"));
        assert!(source.contains("#define INPUT_SELFTEST_CYCLES 3200U"));
        assert!(source.contains("#define INPUT_SELFTEST_LEG_CYCLES 64U"));
        assert!(source.contains("#define INPUT_SELFTEST_POLL_MS 5"));
        assert!(source.contains("#define INPUT_POINTER_FLUSH_MS 5"));
        assert!(source.contains("case 0U:\n        dx = 3;\n        dy = 0;"));
        assert!(source.contains("case 1U:\n        dx = 0;\n        dy = 3;"));
        assert!(source.contains("write_input_event(fd, EV_KEY, KEY_F12, 1)"));
        assert!(source.contains("write_input_event(fd, EV_ABS, ABS_X, selftest->pointer_x)"));
        assert!(!source.contains("write_input_event(fd, EV_KEY, KEY_A, 1)"));
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
            "../../../driver-domains/linux/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"
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
            "schema=4\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-xz\ndata-plane=hostd-input-ring-msix\ncontrol-plane=agent-v1-control\ncontrol-protocol=agent-v1\ncontrol-state=control\ncontrol-transport=kvm-vsock\ncontrol-authentication=dvm-agent-hmac-sha256-v1\ncontrol-capabilities=health,device-inventory,driver-inventory,input-stream\ncontrol-contract-sha256={hash}\nkernel_sha256={hash}\nrootfs_sha256={hash}\nconfig_sha256={hash}\nsources_lock_sha256={hash}\n"
        );
        let values = parse_manifest_text(&manifest, "manifest").unwrap();
        assert_eq!(validate_manifest_values(&values).unwrap(), contract);
    }

    #[test]
    fn dvm_control_contract_rejects_data_plane_capability() {
        let contract_source = include_str!(
            "../../../driver-domains/linux/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"
        );
        let invalid = contract_source.replace(
            "CONTROL_CAPABILITIES=health,device-inventory,driver-inventory,input-stream",
            "CONTROL_CAPABILITIES=health,network-rx",
        );
        assert!(parse_dvm_control_contract_text(&invalid, "invalid contract").is_err());
    }
}
