use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
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
    DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_FEATURE_FUA,
    DVM_BLOCK_FLAG_DVM_READY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_HEADER_RECORD_BYTES,
    DVM_GPU_ATLAS_POOL_HEADER_OFFSET, DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET,
    DVM_GUI_SURFACE_SLOT_COUNT, DVM_INPUT_RING_APERTURE_BYTES, DVM_NET_APERTURE_BYTES,
    DvmBlockHeader, DvmGpuAtlasPoolHeader, DvmGuiSurfaceMessage, DvmGuiSurfacePoolHeader,
    DvmInputRingHeader, DvmNetHeader,
};
use fatfs::Seek as FatSeek;
use fatfs::Write as FatWrite;
use fs_err as fs;
use rustos_driver_domain_host::{
    ControlContract as HostControlContract, ControlSecret, HostControlListener, InputRingSink,
    IvshmemDoorbellServer, ProbeResult,
};
use tempfile::TempDir;

use crate::Result;
use crate::config::Config;
use crate::util::{resolve_command_path, run_command};

const DVM_KERNEL: &str = "rustos-linux-dvm-x86_64.bzImage";
const DVM_ROOTFS: &str = "rustos-linux-dvm-x86_64.rootfs.cpio.xz";
const DVM_CONFIG: &str = "rustos-linux-dvm-x86_64.config";
const DVM_KERNEL_CONFIG: &str = "rustos-linux-dvm-x86_64.kernel.config";
const DVM_MODULE_SIGNING_CERT: &str = "rustos-linux-dvm-x86_64.module-signing.x509";
const DVM_SOURCES_LOCK: &str = "rustos-linux-dvm-x86_64.sources.lock";
const DVM_CONTROL_ARTIFACT: &str = "rustos-linux-dvm-x86_64.control.env";
const DVM_DEV_OUTPUT_MARKER: &str = "out/buildroot-output/.rustos-dvm-dev-output-v1";
const DVM_MANIFEST: &str = "rustos-linux-dvm-x86_64.manifest";
const DVM_MANIFEST_SCHEMA: &str = "8";
const DVM_CONTROL_CONTRACT: &str = "board/overlay/usr/share/rustos-dvm/control-plane-v1.env";
const DVM_CONTROL_PROTOCOL: &str = "agent-v1";
const DVM_CONTROL_STATE: &str = "control";
const DVM_CONTROL_TRANSPORT: &str = "kvm-vsock";
const DVM_CONTROL_AUTHENTICATION: &str = "dvm-agent-hmac-sha256-v1";
const DVM_CONTROL_CAPABILITIES: &str =
    "health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream";
const RUSTOS_BOOT_MARKER: &str = "rootd: core services ready, spawning initd via loaderd";
const RUSTOS_INIT_IDENTITY_MARKER: &str = "initd: identity endpoint registered";
const RUSTOS_POST_INIT_PROVENANCE_MARKER: &str =
    "rootd: post-init deferred-spawn provenance verified";
const RUSTOS_GPU_SCENE_COMPILER_MARKER: &str =
    "uiserver: gpu-scene compiler ready contract=3 public-abi=0 dvm-submit=1";
const RUSTOS_GPU_ACTIVE_MARKER: &str = "uiserver: gpu-compositor active contract=3";
const DVM_KEYBOARD_INGRESS_MARKER: &str = "inputd: DVM keyboard ingress observed";
const DVM_POINTER_INGRESS_MARKER: &str = "inputd: DVM pointer ingress observed";
const DVM_GPU_COMPOSITOR_MARKER: &str = "rustos-dvm-gpu: ready contract=1";
const DVM_GPU_LIVE_MARKER: &str = "rustos-dvm-display: gpu-compositor primed contract=3";
const RUSTOS_DVM_BLOCK_MARKER: &str = "dvm-block: transport installed generation=1";
const RUSTOS_DVM_BLOCK_E2E_MARKER: &str = "storaged: dvm-block e2e flush completed generation=1";
const DVM_BLOCK_READY_MARKER: &str = "rustos-dvm-block: ready abi=1 generation=1";
const DVM_GPU_PIPELINE_PRIME_TIMEOUT_US: u64 = 500_000;
const DVM_GPU_HEALTH_SAMPLES: u64 = 3;
const PHYSICAL_GPU_SMOKE_MIN_FRAMES: usize = 4;
const DEFAULT_UI_FPS_ACTIVE_WINDOWS: usize = 3;
const MAX_UI_FPS_ACTIVE_WINDOWS: usize = 20;
// The end-to-end cursor contract is 60 accepted motion updates per second.
// Require at least 55 in every measured one-second window (over 90%) so a
// single timer boundary cannot fail an otherwise continuous 60 Hz stream.
const MIN_UI_FPS_INPUT_EVENTS: u64 = 55;
const MIN_UI_FPS_CURSOR_MOVES: u64 = 50;
const MAX_UI_INPUT_GAP_MS: u64 = 50;
const MIN_UI_CURSOR_SPAN: u64 = 96;
// Completing the direct DMA-BUF atomic commit must leave meaningful headroom
// inside a 16.67 ms 60 Hz frame.  The relay never copies pixel payloads.
const MAX_DVM_DISPLAY_RELAY_US: u64 = 12_000;
const MAX_DVM_GPU_RENDER_US: u64 = 16_667;
const DVM_DISPLAY_WIDTH: u32 = 1600;
const DVM_DISPLAY_HEIGHT: u32 = 900;
const DVM_DISPLAY_REGION_BYTES: u64 = 128 * 1024 * 1024;
const DVM_GPU_ATLAS_WIDTH: u32 = 2048;
const DVM_GPU_ATLAS_HEIGHT: u32 = 2048;
// Keep the KVM proof topology identical to the supervised physical-display
// DVM. Mesa virgl plus the AMD radeonsi/LLVM runtime, firmware, the compressed
// initrd, XZ workspace, and unpacked ramfs coexist during early boot. Keep a
// measured two-GiB floor so GPU enablement cannot turn memory pressure into a
// nondeterministic 30-second readiness failure.
const DVM_GUEST_MEMORY: &str = "2048M,maxmem=3G,slots=2";
// QEMU maps the shared pixel backend as cacheable device memory at this
// reserved, 2 MiB-aligned guest-physical address in both guests. The ivshmem
// BAR carries only bounded control records and MSI-X doorbells.
const DVM_DISPLAY_PIXEL_PHYS_ADDR: u64 = 0x1_0000_0000;
const DVM_DISPLAY_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const DVM_BLOCK_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const INTERACTIVE_DISPLAY_READY_TIMEOUT: Duration = Duration::from_secs(15);
const INTERACTIVE_IDLE_TICKS: usize = 3;
const DVM_INPUT_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
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
const WAYCLICK_PROFILE_DISABLED: &str = "RUSTOS_WAYCLICK_PROFILE=0";
const WAYCLICK_PROFILE_ENABLED: &str = "RUSTOS_WAYCLICK_PROFILE=1";
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
const PHYSICAL_GPU_REQUIRED_MEMLOCK: u64 = 4 * 1024 * 1024 * 1024;
const ACPI_VFCT_HEADER_BYTES: usize = 0x4c;
const ACPI_VFCT_VBIOS_OFFSET: usize = 0x34;
const ACPI_VFCT_IMAGE_HEADER_BYTES: usize = 28;
const ACPI_VFCT_IMAGE_LENGTH_OFFSET: usize = 24;
const ACPI_VFCT_MAX_BYTES: usize = 4 * 1024 * 1024;

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
    runtime_disk: PathBuf,
    debugcon_log: PathBuf,
    rustos_serial_log: PathBuf,
    dvm_serial_log: PathBuf,
    rustos_stderr_log: PathBuf,
    dvm_stderr_log: PathBuf,
    dvm_input_ring: PathBuf,
    dvm_input_doorbell: PathBuf,
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
    exercise_input: bool,
    exercise_network: bool,
    gui_dvm_surfaces: bool,
    dvm_network_shmem: bool,
    dvm_block_shmem: bool,
    physical_gpu_bdf: Option<String>,
    physical_gpu_firmware: Option<PathBuf>,
    min_ui_fps: Option<u32>,
    ui_proof_windows: usize,
    timeout: Duration,
    expected_markers: Vec<String>,
    expected_dvm_markers: Vec<String>,
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
        input_relay_gate,
    )?;
    // `--timeout` is the readiness budget promised by the CLI, not a budget
    // for host-side doorbell setup, render-node admission, or process
    // creation. Start it only after both guest processes exist.
    let deadline = Instant::now() + options.timeout;
    let result: Result<ProbeResult> = (|| {
        let probe = wait_for_parallel_boot(
            &mut rustos,
            &mut dvm,
            &layout,
            &options,
            deadline,
            &control_relay,
        )?;
        if options.dvm_block_shmem {
            verify_dvm_block_ready(&layout)?;
        }
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
        dvm_block_shmem: true,
        physical_gpu_bdf: None,
        physical_gpu_firmware: None,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
        timeout: Duration::ZERO,
        expected_markers: vec![
            RUSTOS_GPU_ACTIVE_MARKER.to_owned(),
            RUSTOS_DVM_BLOCK_MARKER.to_owned(),
        ],
        expected_dvm_markers: vec![
            DVM_GPU_COMPOSITOR_MARKER.to_owned(),
            DVM_GPU_LIVE_MARKER.to_owned(),
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
  --expect-dvm <marker>
                       require an additional Linux-DVM serial marker (repeatable)
  --exercise-input     run the DVM's bounded evdev loopback self-test and require
                       RustOS inputd keyboard and pointer ingress markers
  --exercise-network   run netprobe through netd and the DVM Ethernet ring;
                       requires --gui-dvm-surfaces and --dvm-network-shmem
  --min-ui-fps <fps>   enable the private KVM-only UI profiler and bounded DVM
                       uinput/evdev load, then require three high-volume input
                       windows, WayClick commit/frame-callback windows, and three
                       accepted DVM atomic-page-flip relay samples at or above
                       this integer FPS; virtual display proof uses QEMU GTK,
                       while --physical-gpu observes the physical KMS path
  --ui-proof-windows <count>
                       require 3..={MAX_UI_FPS_ACTIVE_WINDOWS} consecutive one-second UI/input
                       and DVM relay samples (default {DEFAULT_UI_FPS_ACTIVE_WINDOWS}); requires
                       --min-ui-fps and supports bounded active soak proofs
  --gui-dvm-surfaces   enable the V3 GUI-DVM control/pixel backing and private
                       three-slot GPU atlas transport; no standalone legacy
                       surface renderer or native-GPU path is accepted
  --dvm-network-shmem  attach the bounded RustOS↔DVM Ethernet ring; RustOS keeps
                       no native virtio-net device in this topology
  --dvm-block-shmem    attach a private virtual NVMe namespace only to Linux
                       DVM and the fixed RustOS↔DVM block ring; RustOS receives
                       no native storage controller
  --physical-gpu <BDF> non-commercial lab mode: attach one already-bound GPU
                       from the sealed physical-GPU profile registry through
                       IOMMUFD instead of virtio-GPU; never binds, unbinds, or
                       resets the device
  --gpu-firmware <path>
                       profile-specific owner-private firmware table. The
                       currently certified AMD profile requires a relocated
                       VFCT produced by rustos-hostd prepare-amd-vfct
  --physical-amdgpu <BDF>, --amd-vfct <path>
                       compatibility aliases for the current AMD profile
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
        exercise_input: false,
        exercise_network: false,
        gui_dvm_surfaces: false,
        dvm_network_shmem: false,
        dvm_block_shmem: false,
        physical_gpu_bdf: None,
        physical_gpu_firmware: None,
        min_ui_fps: None,
        ui_proof_windows: DEFAULT_UI_FPS_ACTIVE_WINDOWS,
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

    // A frame-rate proof without an input producer can only time out with idle
    // zero-event windows. Reuse the normal DVM uinput -> evdev -> authenticated
    // relay path; no QMP-only shortcut is introduced.
    if options.min_ui_fps.is_some() {
        options.exercise_input = true;
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
    }
    if options.dvm_block_shmem {
        options
            .expected_markers
            .push(RUSTOS_DVM_BLOCK_MARKER.to_owned());
        options
            .expected_markers
            .push(RUSTOS_DVM_BLOCK_E2E_MARKER.to_owned());
        options
            .expected_dvm_markers
            .push(DVM_BLOCK_READY_MARKER.to_owned());
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
    require_manifest_value(values, "boot", "linux-bzimage+cpio-xz")?;
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
    let display_backing_dir = if options.gui_dvm_surfaces {
        Some(create_dma_pinnable_display_directory()?)
    } else {
        None
    };
    let gui_dvm_surfaces = if let Some(directory) = display_backing_dir.as_ref() {
        // A physical VFIO device causes QEMU to map every guest RAM section
        // into its IOMMUFD IOAS. Keep the writable ivshmem BAR on tmpfs: a
        // regular build-directory file cannot be write-pinned by MAP_FILE.
        let path = directory.path().join("dvm-display.ivshmem");
        create_gui_dvm_surfaces(&path)?;
        Some(path)
    } else {
        None
    };
    let gui_dvm_pixels = if let Some(directory) = display_backing_dir.as_ref() {
        let path = directory.path().join("dvm-display.pmem");
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
    let (dvm_block_aperture, dvm_block_doorbell, dvm_block_disk) = if options.dvm_block_shmem {
        let disk = run_dir.join("dvm-block-disk.img");
        fs::copy(&config.boot_disk_image, &disk).with_context(|| {
            format!(
                "create private storage-DVM disk from {}",
                config.boot_disk_image.display()
            )
        })?;
        fs::set_permissions(&disk, std::fs::Permissions::from_mode(0o600))?;
        let aperture = run_dir.join("dvm-block.ivshmem");
        create_dvm_block_aperture(&aperture, &disk)?;
        (
            Some(aperture),
            Some(run_dir.join("dvm-block-doorbell.sock")),
            Some(disk),
        )
    } else {
        (None, None, None)
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
    for log in [
        &debugcon_log,
        &rustos_serial_log,
        &dvm_serial_log,
        &rustos_stderr_log,
        &dvm_stderr_log,
    ] {
        prepare_runtime_log(log, !options.dry_run)?;
    }
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
        _display_backing_dir: display_backing_dir,
        gui_dvm_surfaces,
        gui_dvm_pixels,
        dvm_display_doorbell,
        dvm_network_shmem,
        dvm_block_aperture,
        dvm_block_doorbell,
        dvm_block_disk,
    })
}

fn prepare_runtime_log(path: &Path, truncate_existing: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    if truncate_existing {
        options.truncate(true);
    }
    options
        .open(path)
        .with_context(|| format!("prepare KVM runtime log {}", path.display()))?;
    Ok(())
}

fn create_dma_pinnable_display_directory() -> Result<TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("rustos-kvm-display-")
        .tempdir_in("/dev/shm")
        .context("create private tmpfs DVM display-backing directory")?;
    fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let directory_fd = std::fs::File::open(directory.path())?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(directory_fd.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("inspect KVM DVM display-backing filesystem");
    }
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    if unsafe { filesystem.assume_init() }.f_type as u64 != TMPFS_MAGIC {
        bail!("/dev/shm is not tmpfs; refusing unproven VFIO display backings");
    }
    Ok(directory)
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

fn create_dvm_block_aperture(path: &Path, disk: &Path) -> Result<()> {
    for candidate in [path, disk] {
        if candidate.to_string_lossy().contains(',') {
            bail!(
                "KVM storage-DVM path contains an unsupported QEMU option separator: {}",
                candidate.display()
            );
        }
    }
    let disk_bytes = fs::metadata(disk)
        .with_context(|| format!("inspect private storage-DVM disk {}", disk.display()))?
        .len();
    if disk_bytes == 0 || !disk_bytes.is_multiple_of(512) {
        bail!("private storage-DVM disk must be non-empty and 512-byte aligned");
    }
    let header = DvmBlockHeader::new(
        1,
        disk_bytes / 512,
        512,
        512,
        DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA,
    );
    if !header.is_valid() {
        bail!("refusing to create invalid fixed DVM block header");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create DVM block aperture {}", path.display()))?;
    file.set_len(DVM_BLOCK_APERTURE_BYTES)
        .with_context(|| format!("size DVM block aperture {}", path.display()))?;
    file.write_all(&header.encode())
        .with_context(|| format!("write DVM block header {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync DVM block aperture {}", path.display()))?;
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
    let atlas_header = DvmGpuAtlasPoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        header,
        DVM_GPU_ATLAS_WIDTH,
        DVM_GPU_ATLAS_HEIGHT,
    )
    .ok_or_else(|| anyhow::anyhow!("refusing to create invalid GPU atlas-pool header"))?;
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
    file.seek(SeekFrom::Start(DVM_GPU_ATLAS_POOL_HEADER_OFFSET as u64))
        .with_context(|| format!("seek DVM GPU atlas header {}", path.display()))?;
    file.write_all(&atlas_header.encode())
        .with_context(|| format!("initialize DVM GPU atlas header {}", path.display()))?;
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

#[derive(Clone, Copy, Debug, Default)]
struct DvmInputCounters {
    producer: u64,
    consumer: u64,
    flags: u32,
}

fn dvm_input_counters(path: &Path) -> Result<DvmInputCounters> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open DVM shared input {}", path.display()))?;
    let mut bytes = [0_u8; DvmInputRingHeader::encoded_len()];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read DVM shared input header {}", path.display()))?;
    let header = DvmInputRingHeader::decode(&bytes)
        .context("DVM shared input header changed or became invalid during smoke")?;
    Ok(DvmInputCounters {
        producer: header.producer,
        consumer: header.consumer,
        flags: header.flags,
    })
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

fn verify_dvm_block_ready(layout: &KvmLayout) -> Result<()> {
    let aperture = layout
        .dvm_block_aperture
        .as_deref()
        .context("storage-DVM proof lost its block aperture")?;
    let disk = layout
        .dvm_block_disk
        .as_deref()
        .context("storage-DVM proof lost its private backing disk")?;
    let mut file = std::fs::File::open(aperture)
        .with_context(|| format!("open live DVM block aperture {}", aperture.display()))?;
    let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read live DVM block header {}", aperture.display()))?;
    let header = DvmBlockHeader::decode(&bytes)
        .context("live DVM block aperture contains an invalid header")?;
    let ready = DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
    if header.flags & ready != ready {
        bail!(
            "DVM block peers did not both publish readiness flags={:#x}",
            header.flags
        );
    }
    let disk_bytes = fs::metadata(disk)
        .with_context(|| format!("inspect live storage-DVM disk {}", disk.display()))?
        .len();
    if header.generation != 1
        || disk_bytes == 0
        || !disk_bytes.is_multiple_of(512)
        || header.capacity_sectors != disk_bytes / 512
        || header.logical_block_size != 512
        || header.physical_block_size != 512
    {
        bail!("live DVM block geometry diverged from the private backing disk");
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
    )?;
    replace_private_registry_anchor(
        runtime_disk,
        UISERVER_PROFILE_REGISTRY_PATHS,
        WAYCLICK_PROFILE_DISABLED,
        WAYCLICK_PROFILE_ENABLED,
        "WayClick frame profile",
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

fn canonical_pci_bdf(value: &str) -> Result<String> {
    let (domain, rest) = value
        .split_once(':')
        .with_context(|| format!("invalid PCI BDF {value:?}"))?;
    let (bus, device_function) = rest
        .split_once(':')
        .with_context(|| format!("invalid PCI BDF {value:?}"))?;
    let (device, function) = device_function
        .split_once('.')
        .with_context(|| format!("invalid PCI BDF {value:?}"))?;
    let domain = u16::from_str_radix(domain, 16)
        .with_context(|| format!("invalid PCI domain in {value:?}"))?;
    let bus =
        u8::from_str_radix(bus, 16).with_context(|| format!("invalid PCI bus in {value:?}"))?;
    let device = u8::from_str_radix(device, 16)
        .with_context(|| format!("invalid PCI device in {value:?}"))?;
    let function = u8::from_str_radix(function, 16)
        .with_context(|| format!("invalid PCI function in {value:?}"))?;
    if device > 0x1f || function > 7 {
        bail!("PCI BDF is outside device/function bounds: {value}");
    }
    let canonical = format!("{domain:04x}:{bus:02x}:{device:02x}.{function:x}");
    if canonical != value {
        bail!("PCI BDF must be canonical lowercase {canonical}, got {value:?}");
    }
    Ok(canonical)
}

fn require_direct_rw_character_device(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
        bail!(
            "{label} must be a direct character device: {}",
            path.display()
        );
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {label} {} read/write", path.display()))?;
    Ok(())
}

fn vfio_device_cdev_path(device: &Path) -> Result<PathBuf> {
    let mut names = std::fs::read_dir(device.join("vfio-dev"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    if names.len() != 1 {
        bail!(
            "physical AMD VFIO device must expose exactly one vfio-dev cdev, found {}",
            names.len()
        );
    }
    let name = names
        .pop()
        .and_then(|name| name.into_string().ok())
        .context("physical AMD vfio-dev name is not UTF-8")?;
    let id = name
        .strip_prefix("vfio")
        .context("physical AMD vfio-dev name lacks vfio prefix")?
        .parse::<u32>()
        .context("physical AMD vfio-dev name has a non-numeric ID")?;
    if name != format!("vfio{id}") {
        bail!("physical AMD vfio-dev name is not canonical: {name}");
    }
    Ok(Path::new("/dev/vfio/devices").join(name))
}

fn physical_memlock_soft_limit() -> Result<Option<u64>> {
    let limits = fs::read_to_string("/proc/self/limits")?;
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max locked memory"))
        .context("/proc/self/limits has no Max locked memory row")?;
    let value = line
        .split_whitespace()
        .nth(3)
        .context("Max locked memory row has no soft limit")?;
    if value == "unlimited" {
        return Ok(None);
    }
    Ok(Some(
        value
            .parse()
            .context("parse Max locked memory soft limit")?,
    ))
}

fn validate_lab_amd_vfct(path: &Path, owner: u32) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect lab AMD VFCT {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "lab AMD VFCT must be an owner-private non-symlink file: {}",
            path.display()
        );
    }
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize lab AMD VFCT {}", path.display()))?;
    let label = canonical.to_string_lossy();
    if label.contains([',', '\n', '\r']) {
        bail!("lab AMD VFCT path is not representable as a QEMU property");
    }
    let table = fs::read(&canonical)?;
    if table.len() < ACPI_VFCT_HEADER_BYTES
        || table.len() > ACPI_VFCT_MAX_BYTES
        || table.get(0..4) != Some(b"VFCT")
    {
        bail!("lab AMD VFCT header or bounded size is invalid");
    }
    let table_length =
        u32::from_le_bytes(table[4..8].try_into().expect("validated VFCT length field")) as usize;
    if table_length != table.len()
        || table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0
    {
        bail!("lab AMD VFCT length or ACPI checksum is invalid");
    }
    let image_header = u32::from_le_bytes(
        table[ACPI_VFCT_VBIOS_OFFSET..ACPI_VFCT_VBIOS_OFFSET + 4]
            .try_into()
            .expect("validated VFCT image offset"),
    ) as usize;
    let image_start = image_header
        .checked_add(ACPI_VFCT_IMAGE_HEADER_BYTES)
        .context("lab AMD VFCT image offset overflow")?;
    if image_header < ACPI_VFCT_HEADER_BYTES || image_start > table.len() {
        bail!("lab AMD VFCT image header is out of bounds");
    }
    let field_u32 = |offset: usize| -> Result<u32> {
        let bytes = table
            .get(offset..offset + 4)
            .context("truncated lab AMD VFCT field")?;
        Ok(u32::from_le_bytes(
            bytes.try_into().expect("four-byte VFCT field"),
        ))
    };
    let field_u16 = |offset: usize| -> Result<u16> {
        let bytes = table
            .get(offset..offset + 2)
            .context("truncated lab AMD VFCT identity")?;
        Ok(u16::from_le_bytes(
            bytes.try_into().expect("two-byte VFCT field"),
        ))
    };
    let image_length = field_u32(image_header + ACPI_VFCT_IMAGE_LENGTH_OFFSET)? as usize;
    let image_end = image_start
        .checked_add(image_length)
        .context("lab AMD VFCT image length overflow")?;
    if field_u32(image_header)? != 0
        || field_u32(image_header + 4)? != 8
        || field_u32(image_header + 8)? != 0
        || field_u16(image_header + 12)? != 0x1002
        || field_u16(image_header + 14)? != 0x1900
        || image_length < 0x4a
        || image_end > table.len()
        || table.get(image_start..image_start + 2) != Some(&[0x55, 0xaa])
    {
        bail!("lab AMD VFCT is not the exact relocated 1002:1900 guest 00:08.0 image");
    }
    let atom_header = usize::from(field_u16(image_start + 0x48)?);
    let atom = image_start
        .checked_add(atom_header)
        .and_then(|offset| offset.checked_add(4))
        .context("lab AMD VBIOS ATOM pointer overflow")?;
    if table
        .get(atom..atom + 4)
        .is_none_or(|magic| magic != b"ATOM" && magic != b"MOTA")
    {
        bail!("lab AMD VFCT VBIOS lacks an ATOM header");
    }
    Ok(canonical)
}

fn physical_gpu_profile(vendor: &str, device: &str) -> Option<PhysicalGpuProfile> {
    PHYSICAL_GPU_PROFILES
        .iter()
        .copied()
        .find(|profile| profile.vendor == vendor && profile.device == device)
}

fn selected_physical_gpu_profile(options: &SmokeOptions) -> Result<PhysicalGpuProfile> {
    let bdf = canonical_pci_bdf(
        options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU BDF is missing")?,
    )?;
    let device = Path::new("/sys/bus/pci/devices").join(&bdf);
    let vendor = fs::read_to_string(device.join("vendor"))?;
    let device_id = fs::read_to_string(device.join("device"))?;
    physical_gpu_profile(vendor.trim(), device_id.trim()).with_context(|| {
        format!(
            "physical GPU {:04x}:{:04x} has no certified profile",
            u16::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).unwrap_or(0),
            u16::from_str_radix(device_id.trim().trim_start_matches("0x"), 16).unwrap_or(0)
        )
    })
}

fn gpu_evidence_expectation(options: &SmokeOptions) -> Result<GpuEvidenceExpectation> {
    if options.physical_gpu_bdf.is_none() {
        return Ok(VIRTUAL_GPU_EVIDENCE);
    }
    let profile = selected_physical_gpu_profile(options)?;
    Ok(GpuEvidenceExpectation {
        drm_driver: profile.drm_driver,
        backend_class: profile.backend_class,
    })
}

fn claim_physical_gpu_launch(layout: &KvmLayout, options: &SmokeOptions) -> Result<()> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let boot_id = boot_id.trim();
    if boot_id.len() != 36
        || !boot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        bail!("host boot ID is malformed; refusing physical GPU launch");
    }
    let bdf = canonical_pci_bdf(
        options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU BDF is missing")?,
    )?;
    let profile = selected_physical_gpu_profile(options)?;
    claim_physical_gpu_launch_in(&layout.run_dir, boot_id, profile, &bdf)
}

fn claim_physical_gpu_launch_in(
    run_dir: &Path,
    boot_id: &str,
    profile: PhysicalGpuProfile,
    bdf: &str,
) -> Result<()> {
    let claim = run_dir.join(format!("physical-gpu-launch-{boot_id}"));
    match fs::create_dir(&claim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "physical GPU launch already attempted during host boot {boot_id}; reset methods are disabled, so cold-boot the host before another assignment"
            )
        }
        Err(error) => return Err(error).context("create physical GPU single-launch claim"),
    }
    fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o700))?;
    let evidence = format!(
        "PHYSICAL_GPU_LAUNCH_CLAIM_SCHEMA=1\nBOOT_ID={boot_id}\nPROFILE={}\nBDF={bdf}\nRESET_RECOVERY=cold-boot-required\n",
        profile.id
    );
    let evidence_path = claim.join("claim.env");
    fs::write(&evidence_path, evidence)?;
    fs::set_permissions(&evidence_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn validate_physical_gpu_inputs(options: &SmokeOptions) -> Result<()> {
    let bdf = canonical_pci_bdf(
        options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU BDF is missing")?,
    )?;
    let profile = selected_physical_gpu_profile(options)?;
    let device = Path::new("/sys/bus/pci/devices").join(&bdf);
    let vendor = fs::read_to_string(device.join("vendor"))?;
    let device_id = fs::read_to_string(device.join("device"))?;
    let driver = std::fs::canonicalize(device.join("driver"))?;
    if driver.file_name() != Some(OsStr::new("vfio-pci")) {
        bail!(
            "physical GPU profile {} must be pre-bound to vfio-pci: vendor={} device={} driver={}",
            profile.id,
            vendor.trim(),
            device_id.trim(),
            driver.display()
        );
    }
    let group = std::fs::canonicalize(device.join("iommu_group"))?;
    let group_id = group
        .file_name()
        .and_then(OsStr::to_str)
        .context("physical GPU IOMMU group has no numeric name")?;
    group_id
        .parse::<u32>()
        .context("physical GPU IOMMU group name is not numeric")?;
    let mut members = std::fs::read_dir(group.join("devices"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    members.sort();
    if members != [bdf.clone()] {
        bail!(
            "physical GPU lab target must be the sole IOMMU-group member: {}",
            members.join(",")
        );
    }
    let reset_methods = fs::read_to_string(device.join("reset_method"))?;
    if !reset_methods.trim().is_empty() {
        bail!(
            "physical GPU lab mode requires reset_method disabled so QEMU cannot bus-reset outside group: {}",
            reset_methods.trim()
        );
    }
    if fs::read_to_string("/sys/module/vfio_pci/parameters/disable_idle_d3")?.trim() != "Y" {
        bail!("physical GPU lab mode requires vfio-pci disable_idle_d3=Y");
    }
    let mut config = std::fs::OpenOptions::new()
        .read(true)
        .open(device.join("config"))?;
    config.seek(SeekFrom::Start(4))?;
    let mut command = [0_u8; 2];
    config.read_exact(&mut command)?;
    if u16::from_le_bytes(command) & 0x4 != 0 {
        bail!("physical GPU lab target still has PCI bus mastering enabled");
    }
    require_direct_rw_character_device(Path::new("/dev/iommu"), "IOMMUFD")?;
    let vfio_cdev = vfio_device_cdev_path(&device)?;
    require_direct_rw_character_device(&vfio_cdev, "VFIO device cdev")?;
    let soft_memlock = physical_memlock_soft_limit()?;
    if soft_memlock.is_some_and(|bytes| bytes < PHYSICAL_GPU_REQUIRED_MEMLOCK) {
        if options.dry_run {
            eprintln!(
                "xtask: physical GPU dry-run warning: inherited memlock is below 4 GiB; the real command will fail before QEMU"
            );
        } else {
            bail!(
                "physical GPU QEMU requires inherited memlock >= 4 GiB (observed {} bytes)",
                soft_memlock.unwrap_or(0)
            );
        }
    }
    let owner = std::fs::metadata("/proc/self")?.uid();
    match profile.firmware_kind {
        PhysicalGpuFirmwareKind::AmdVfct => {
            validate_lab_amd_vfct(
                options
                    .physical_gpu_firmware
                    .as_deref()
                    .context("AMD physical GPU profile requires a VFCT")?,
                owner,
            )?;
        }
    }
    let boot_vga = fs::read_to_string(device.join("boot_vga"))?;
    if !matches!(boot_vga.trim(), "0" | "1") {
        bail!("physical AMD boot_vga state is malformed");
    }
    eprintln!(
        "xtask: NON-COMMERCIAL physical GPU lab mode profile={} target={bdf} group={group_id} boot_vga={} binding/reset are operator-owned",
        profile.id,
        boot_vga.trim()
    );
    Ok(())
}

fn require_host_render_node() -> Result<PathBuf> {
    let mut amdgpu_nodes = Vec::new();
    for entry in std::fs::read_dir("/dev/dri").context("missing host DRM device directory")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("renderD") {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
            bail!(
                "{} must be a direct character-device render node",
                path.display()
            );
        }
        let sysfs = Path::new("/sys/class/drm").join(name).join("device");
        let vendor = std::fs::read_to_string(sysfs.join("vendor"))
            .with_context(|| format!("missing vendor identity for {}", path.display()))?;
        let driver = std::fs::canonicalize(sysfs.join("driver"))
            .with_context(|| format!("missing driver identity for {}", path.display()))?;
        if vendor.trim() == "0x1002" && driver.file_name() == Some(OsStr::new("amdgpu")) {
            amdgpu_nodes.push(path);
        }
    }
    amdgpu_nodes.sort();
    let render_node = match amdgpu_nodes.as_slice() {
        [render_node] => render_node.clone(),
        [] => bail!("KVM virgl requires exactly one AMDGPU render node; found none"),
        nodes => bail!(
            "KVM virgl requires exactly one AMDGPU render node; found {}",
            nodes
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_node)
        .with_context(|| {
            format!(
                "KVM virgl requires read/write access to {}",
                render_node.display()
            )
        })?;
    Ok(render_node)
}

fn mesa_dri_prime_for_render_node(render_node: &Path) -> Result<String> {
    let name = render_node
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.starts_with("renderD"))
        .with_context(|| {
            format!(
                "validated host render node has an invalid name: {}",
                render_node.display()
            )
        })?;
    let device = std::fs::canonicalize(Path::new("/sys/class/drm").join(name).join("device"))
        .with_context(|| format!("resolve PCI identity for {}", render_node.display()))?;
    let bdf = device
        .file_name()
        .and_then(OsStr::to_str)
        .context("host render node sysfs target has no PCI BDF")?;
    mesa_dri_prime_for_pci_bdf(bdf)
}

fn mesa_dri_prime_for_pci_bdf(bdf: &str) -> Result<String> {
    let bdf = canonical_pci_bdf(bdf)?;
    Ok(format!(
        "pci-{}",
        bdf.chars()
            .map(|character| match character {
                ':' | '.' => '_',
                other => other,
            })
            .collect::<String>()
    ))
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
        // The input ivshmem broker deliberately tears down the whole fixed
        // topology when either peer disconnects. Keep peer 1 alive across
        // bounded vsock setup failures; otherwise a DVM that becomes ready
        // just after the first five-second accept deadline can never reconnect.
        let mut sink = loop {
            match InputRingSink::connect(&input_doorbell, &input_ring, Duration::from_secs(1)) {
                Ok(sink) => break sink,
                Err(error) => {
                    eprintln!(
                        "xtask: interactive DVM input transport not ready; retrying: {error:#}"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
            }
        };
        loop {
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

fn start_dvm_block_doorbell(layout: &KvmLayout) -> Result<Option<IvshmemDoorbellServer>> {
    let (Some(aperture), Some(doorbell)) = (
        layout.dvm_block_aperture.as_deref(),
        layout.dvm_block_doorbell.as_deref(),
    ) else {
        return Ok(None);
    };
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(aperture)
        .with_context(|| format!("open DVM block aperture {}", aperture.display()))?;
    Ok(Some(IvshmemDoorbellServer::start_single_vector(
        doorbell, &backing,
    )?))
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

// The paired launch keeps both guest, transport, display, and relay-gate
// ownership arguments explicit at the orchestration boundary.
#[allow(clippy::too_many_arguments)]
fn spawn_guests(
    qemu: &Path,
    config: &Config,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
    options: &SmokeOptions,
    guest_display: GuestDisplay,
    host_render_node: Option<&Path>,
    display_doorbell: Option<&IvshmemDoorbellServer>,
    block_doorbell: Option<&IvshmemDoorbellServer>,
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
    if let Some(doorbell) = layout.dvm_block_doorbell.as_deref() {
        append_dvm_block_doorbell(&mut rustos_command, doorbell);
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

    if let Some(display_doorbell) = display_doorbell
        && let Err(error) = display_doorbell.wait_for_peer_count(1, DVM_DISPLAY_FIRST_PEER_TIMEOUT)
    {
        let mut rustos = rustos;
        stop_guest(&mut rustos);
        return Err(error).context("RustOS did not claim ivshmem peer ID 0 before DVM launch");
    }
    if let Some(block_doorbell) = block_doorbell
        && let Err(error) = block_doorbell.wait_for_peer_count(1, DVM_BLOCK_FIRST_PEER_TIMEOUT)
    {
        let mut rustos = rustos;
        stop_guest(&mut rustos);
        return Err(error)
            .context("RustOS did not claim block ivshmem peer ID 0 before DVM launch");
    }

    let mut dvm_command = Command::new(qemu);
    let dvm_append = if options.exercise_input {
        "console=ttyS0 preempt=full rustos.dvm.input-selftest=1"
    } else {
        "console=ttyS0 preempt=full"
    };
    let dvm_display = qemu_display_backend(guest_display, host_render_node)?;
    if guest_display == GuestDisplay::DvmGtk {
        let render_node =
            host_render_node.context("GTK display lost its validated host render node")?;
        let dri_prime = mesa_dri_prime_for_render_node(render_node)?;
        eprintln!(
            "xtask: KVM GTK renderer pinned node={} DRI_PRIME={dri_prime}",
            render_node.display()
        );
        dvm_command.env("DRI_PRIME", dri_prime);
    }
    dvm_command
        .arg("-name")
        .arg("rustos-linux-dvm-kvm")
        .args([
            "-machine",
            dvm_machine(),
            "-cpu",
            "host",
            "-m",
            DVM_GUEST_MEMORY,
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
            &dvm_display,
            "-vga",
            "none",
            "-no-reboot",
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
        ));
    append_dvm_network_device(&mut dvm_command, guest_display);
    if !append_dvm_virtual_gpu(&mut dvm_command, guest_display) {
        let bdf = options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU display selected without a BDF")?;
        let profile = selected_physical_gpu_profile(options)?;
        let firmware = std::fs::canonicalize(
            options
                .physical_gpu_firmware
                .as_deref()
                .context("physical GPU display selected without profile firmware")?,
        )?;
        append_physical_gpu(&mut dvm_command, profile, bdf, &firmware);
    }
    append_dvm_input_devices(&mut dvm_command, guest_display);
    dvm_command
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
    if let Some(doorbell) = layout.dvm_block_doorbell.as_deref() {
        append_dvm_block_doorbell(&mut dvm_command, doorbell);
        append_dvm_virtual_storage(
            &mut dvm_command,
            layout
                .dvm_block_disk
                .as_deref()
                .context("DVM block aperture exists without a private backing disk")?,
        );
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

fn append_dvm_block_doorbell(command: &mut Command, socket_path: &Path) {
    command
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-block-doorbell,path={}",
            socket_path.display(),
        ))
        .arg("-device")
        .arg("ivshmem-doorbell,vectors=1,chardev=dvm-block-doorbell");
}

fn append_dvm_virtual_storage(command: &mut Command, disk: &Path) {
    command
        .arg("-drive")
        .arg(format!(
            "file={},format=raw,if=none,id=dvm-storage-disk,cache=none,aio=threads",
            disk.display()
        ))
        .arg("-device")
        // q35 already owns exactly one ICH9 AHCI controller. Attach the
        // private namespace to that controller instead of adding a second
        // NVMe controller, which would correctly fail the storage DVM's
        // exact-single-controller admission.
        .arg("ide-hd,drive=dvm-storage-disk,bus=ide.0,unit=0,id=dvm-storage-disk-device");
}

fn append_dvm_display_pixels(command: &mut Command, path: &Path, read_only: bool) {
    let mut backend = format!(
        "memory-backend-file,id=dvm-display-pixels,mem-path={},size={},share=on",
        path.display(),
        DVM_DISPLAY_REGION_BYTES
    );
    if read_only {
        backend.push_str(",readonly=on,rom=on");
    } else {
        // Fault every tmpfs page before the VFIO device is attached in the
        // second QEMU process. IOMMUFD must never discover an unpinnable or
        // unpopulated source aperture only after device activation.
        backend.push_str(",prealloc=on");
    }
    command
        .arg("-object")
        .arg(backend)
        .arg("-device")
        .arg(format!(
            "virtio-pmem-pci,id=dvm-display-pmem,memdev=dvm-display-pixels,memaddr={DVM_DISPLAY_PIXEL_PHYS_ADDR}"
        ));
}

/// Add an already-bound device from the sealed physical-GPU profile registry.
///
/// QEMU 11.0 exposes each mmap-able VFIO PCI BAR through the kernel's
/// VFIO_DEVICE_FEATURE_DMA_BUF API before IOMMUFD maps it. Keep BAR mmap enabled:
/// `x-no-mmap=on` bypasses that API, falls back to slow MMIO, and recreates the
/// unsupported PCI-BAR mapping that caused QEMU 10.2.1 to abort before DVM boot.
fn append_physical_gpu(
    command: &mut Command,
    profile: PhysicalGpuProfile,
    bdf: &str,
    firmware: &Path,
) {
    command.args(["-object", "iommufd,id=iommufd0"]);
    match profile.firmware_kind {
        PhysicalGpuFirmwareKind::AmdVfct => {
            command
                .arg("-acpitable")
                .arg(format!("file={}", firmware.display()));
        }
    }
    command
        .args(["-trace", "enable=vfio_listener_region_add_ram"])
        .args(["-trace", "enable=iommufd_backend_map_dma"])
        .args(["-trace", "enable=iommufd_backend_map_file_dma"])
        .args(["-trace", "enable=vfio_region_dmabuf"])
        .arg("-device")
        .arg(format!(
            "vfio-pci,host={bdf},iommufd=iommufd0,addr={},rombar=0",
            profile.guest_address
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
    let gpu_evidence = gpu_evidence_expectation(options)?;
    loop {
        check_guest_running(rustos, "RustOS", &layout.rustos_stderr_log)?;
        check_guest_running(dvm, "Linux DVM", &layout.dvm_stderr_log)?;
        let rustos_log = fs::read_to_string(&layout.debugcon_log)?;
        let dvm_log = fs::read_to_string(&layout.dvm_serial_log)?;
        if options.gui_dvm_surfaces
            && let Some(failure) = dvm_display_failure(&dvm_log, options.physical_gpu_bdf.is_some())
        {
            bail!("Linux DVM display relay failed before readiness: {failure}");
        }
        let rustos_ready = options
            .expected_markers
            .iter()
            .all(|marker| rustos_log.contains(marker));
        let dvm_ready = options
            .expected_dvm_markers
            .iter()
            .all(|marker| dvm_log.contains(marker));
        let dvm_gpu_ready = dvm_gpu_compositor_ready(&dvm_log, gpu_evidence);
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
        let ui_render_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            uiserver_profile_meets_fps(&rustos_log, minimum, options.ui_proof_windows)
        });
        let ui_input_ready = options.min_ui_fps.is_none_or(|minimum| {
            uiserver_profile_input_pipeline_healthy(
                &rustos_log,
                options.ui_proof_windows,
                Some(minimum),
            )
        });
        let wayclick_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            wayclick_profile_meets_fps(&rustos_log, minimum, options.ui_proof_windows)
        });
        let ui_runtime_ready = !runtime_stall_or_crash_observed(&rustos_log);
        let dvm_relay_fps_ready = options.min_ui_fps.is_none_or(|minimum| {
            !options.gui_dvm_surfaces
                || dvm_display_relay_meets_fps(&dvm_log, minimum, options.ui_proof_windows)
        });
        let dvm_runtime_ready = !runtime_stall_or_crash_observed(&dvm_log);
        let ui_fps_ready = ui_render_fps_ready
            && ui_input_ready
            && wayclick_fps_ready
            && ui_runtime_ready
            && dvm_relay_fps_ready
            && dvm_runtime_ready;
        let dvm_display_ready = !options.gui_dvm_surfaces
            || (dvm_display_provider_ready(&rustos_log)
                && dvm_display_relay_ready(&dvm_log, options.physical_gpu_bdf.is_some()));
        let physical_gpu_frames_ready = options
            .physical_gpu_bdf
            .as_ref()
            .is_none_or(|_| dvm_physical_frames_ready(&dvm_log));
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
            && dvm_ready
            && dvm_gpu_ready
            && ui_fps_ready
            && dvm_display_ready
            && physical_gpu_frames_ready
            && dvm_network_ready
            && dvm_network_traffic_ready
            && let Some(control_ready) = control_ready
        {
            return Ok(control_ready);
        }
        if Instant::now() >= deadline {
            let input = dvm_input_counters(&layout.dvm_input_ring)?;
            let wayclick_observed = wayclick_profile_observation(&rustos_log);
            let missing_rustos = options
                .expected_markers
                .iter()
                .filter(|marker| !rustos_log.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let missing_dvm = options
                .expected_dvm_markers
                .iter()
                .filter(|marker| !dvm_log.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            bail!(
                "KVM parallel boot did not reach readiness within {:?}; RustOS missing={:?}; Linux-DVM missing={:?}; dvm-gpu-ready={}; ui-fps-ready={} (render={} input={} wayclick={} rustos-runtime={} dvm-relay={} dvm-runtime={}); wayclick-observed={:?}; dvm-display-ready={}; physical-gpu-frames-ready={}; dvm-network-ready={}; dvm-network-traffic-ready={}; host-input-relay-pending={}; input-ring={}/{} flags={:#x}; network-ring={:?}; inspect {}, {}, {}, and {}",
                options.timeout,
                missing_rustos,
                missing_dvm,
                dvm_gpu_ready,
                ui_fps_ready,
                ui_render_fps_ready,
                ui_input_ready,
                wayclick_fps_ready,
                ui_runtime_ready,
                dvm_relay_fps_ready,
                dvm_runtime_ready,
                wayclick_observed,
                dvm_display_ready,
                physical_gpu_frames_ready,
                dvm_network_ready,
                dvm_network_traffic_ready,
                control_ready.is_none(),
                input.producer,
                input.consumer,
                input.flags,
                dvm_network,
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
                layout.rustos_stderr_log.display(),
                layout.dvm_stderr_log.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn dvm_gpu_compositor_ready(log: &str, expected: GpuEvidenceExpectation) -> bool {
    let failure_after_ready = [
        "rustos-dvm-gpu: context lost",
        "rustos-dvm-gpu: executor unavailable",
        "rustos-dvm-gpu: pipeline prime failed",
        "rustos-dvm-gpu: proof failed",
        "rustos-dvm-gpu: evidence publish failed",
        "rustos-dvm-gpu: contract negative selftest failed",
    ];
    let mut ready = false;
    let mut health_sequence = 0_u64;
    for line in log.lines() {
        if failure_after_ready
            .iter()
            .any(|marker| line.contains(marker))
        {
            ready = false;
            health_sequence = 0;
            continue;
        }
        if let Some((_, fields)) = line.split_once("rustos-dvm-gpu: health ") {
            let sequence = log_u64(fields, "sequence");
            let completion = log_u64(fields, "completion_us");
            if sequence.is_some_and(|value| value == health_sequence + 1)
                && completion.is_some_and(|value| value > 0 && value <= 16_667)
                && fields
                    .split_whitespace()
                    .any(|field| field == "acquire-fence=1")
            {
                health_sequence += 1;
            } else {
                ready = false;
                health_sequence = 0;
            }
            continue;
        }
        let Some((_, fields)) = line.split_once(DVM_GPU_COMPOSITOR_MARKER) else {
            continue;
        };
        let frames = log_u64(fields, "frames");
        let prime = log_u64(fields, "prime_us");
        let fps = log_u64(fields, "fps_milli");
        let average = log_u64(fields, "avg_us");
        let maximum = log_u64(fields, "max_us");
        let wall_maximum = log_u64(fields, "wall_max_us");
        let frame_hash_a = fields
            .split_whitespace()
            .find_map(|field| field.strip_prefix("frame_hash_a="))
            .filter(|value| value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|value| u64::from_str_radix(value, 16).ok());
        let frame_hash_b = fields
            .split_whitespace()
            .find_map(|field| field.strip_prefix("frame_hash_b="))
            .filter(|value| value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|value| u64::from_str_radix(value, 16).ok());
        ready = log_text_field_is(fields, "driver", expected.drm_driver)
            && log_text_field_is(fields, "backend-class", expected.backend_class)
            && log_text_field_is(fields, "certification", "registered")
            && fields.split_whitespace().any(|field| {
                field
                    .strip_prefix("renderer=")
                    .is_some_and(|value| !value.is_empty())
            })
            && (expected.backend_class != "virtual-staged"
                || fields
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("renderer="))
                    .is_some_and(|renderer| renderer.to_ascii_lowercase().contains("virgl")))
            && fields.split_whitespace().any(|field| field == "commands=3")
            && fields
                .split_whitespace()
                .any(|field| field == "gpu-fence=1")
            && fields
                .split_whitespace()
                .any(|field| field == "acquire-fence=1")
            && fields.split_whitespace().any(|field| field == "negative=5")
            && fields.split_whitespace().any(|field| field == "software=0")
            && fields
                .split_whitespace()
                .any(|field| field == "scheduler=rr")
            && fields.split_whitespace().any(|field| field == "priority=8")
            && fields
                .split_whitespace()
                .any(|field| field == "rttime-soft-us=50000")
            && fields
                .split_whitespace()
                .any(|field| field == "rttime-hard-us=100000")
            && fields
                .split_whitespace()
                .any(|field| field == "rttime-hard-action=terminate")
            && fields
                .split_whitespace()
                .any(|field| field == "scheduler-restored=normal")
            && fields
                .split_whitespace()
                .any(|field| field == "performance-target=1")
            && fields
                .split_whitespace()
                .any(|field| field == "scope-public-abi=0")
            && fields
                .split_whitespace()
                .any(|field| field == "scope-ui-connected=0")
            && fields
                .split_whitespace()
                .any(|field| field == "scope-scanout=0")
            && frames.is_some_and(|value| value >= 120)
            && prime.is_some_and(|value| value > 0 && value <= DVM_GPU_PIPELINE_PRIME_TIMEOUT_US)
            && fps.is_some_and(|value| value >= 60_000)
            && maximum.is_some_and(|value| value <= 16_667)
            && wall_maximum.is_some_and(|value| value > 0 && value <= 16_667)
            && average.zip(maximum).is_some_and(|(avg, max)| avg <= max)
            && frame_hash_a
                .zip(frame_hash_b)
                .is_some_and(|(left, right)| left != 0 && right != 0 && left != right)
            && fields
                .split_whitespace()
                .any(|field| field == "hash-stable=1")
            && fields
                .split_whitespace()
                .any(|field| field == "hash-dynamic=1");
        health_sequence = 0;
    }
    ready && health_sequence >= DVM_GPU_HEALTH_SAMPLES
}

fn dvm_display_failure(log: &str, physical_gpu: bool) -> Option<String> {
    if let Some(line) = log
        .lines()
        .find(|line| line.contains("rustos-dvm-display: gpu-compositor offline"))
    {
        return Some(format!(
            "Linux DVM GPU compositor went offline during readiness detail={}",
            line.trim()
        ));
    }
    if let Some(line) = log
        .lines()
        .rev()
        .find(|line| line.contains("rustos-dvm-display: GPU KMS setup unavailable stage="))
    {
        return Some(line.trim().to_owned());
    }
    for marker in [
        "rustos-dvm-gpu: pipeline prime evidence unavailable",
        "rustos-dvm-gpu: evidence publish failed",
    ] {
        if let Some(line) = log.lines().find(|line| line.contains(marker)) {
            return Some(format!(
                "Linux DVM GPU evidence publication failed detail={}",
                line.trim()
            ));
        }
    }
    if physical_gpu && log.contains("PSP create ring failed") {
        return Some(
            "physical GPU kernel probe failed stage=device-security-processor; the assigned device did not enter a reusable post-reset state"
                .to_owned(),
        );
    }
    if physical_gpu {
        for marker in [
            "Fatal error during GPU init",
            "probe with driver ",
            "rustos-dvm-gpu: executor unavailable",
        ] {
            if let Some(line) = log.lines().find(|line| line.contains(marker)) {
                return Some(format!(
                    "physical GPU kernel probe failed stage=driver-init detail={}",
                    line.trim()
                ));
            }
        }
    }
    None
}

fn dvm_physical_frames_ready(log: &str) -> bool {
    let mut frame_count = 0_usize;
    let mut last_sequence = None;
    let mut last_submit = None;
    for line in log.lines() {
        let Some((_, fields)) = line.split_once("rustos-dvm-display: gpu-frame ") else {
            continue;
        };
        let sequence = log_u64(fields, "sequence");
        let submit = log_u64(fields, "submit");
        let output = log_u64(fields, "output");
        let render_us = log_u64(fields, "render_us");
        let contract_ok = fields
            .split_whitespace()
            .any(|field| field == "source-path=dmabuf")
            && fields
                .split_whitespace()
                .any(|field| field == "zero-copy=1")
            && fields
                .split_whitespace()
                .any(|field| field == "gpu-fence=1")
            && fields
                .split_whitespace()
                .any(|field| field == "present-fence=1");
        let Some((sequence, submit, output, render_us)) =
            sequence.zip(submit).zip(output).zip(render_us).map(
                |(((sequence, submit), output), render_us)| (sequence, submit, output, render_us),
            )
        else {
            return false;
        };
        if !contract_ok
            || sequence == 0
            || submit == 0
            || output >= 3
            || render_us == 0
            || render_us > 16_667
            || last_sequence.is_some_and(|prior| sequence != prior + 1)
            || last_submit.is_some_and(|prior| submit != prior + 1)
        {
            return false;
        }
        last_sequence = Some(sequence);
        last_submit = Some(submit);
        frame_count += 1;
    }
    frame_count >= PHYSICAL_GPU_SMOKE_MIN_FRAMES
}

/// The kernel's bootstrap trace intentionally does not promise runtime
/// debugcon delivery. The userspace display-info ABI is the authoritative
/// observation: the runner's fixed ivshmem header must emerge unchanged as the
/// active primary display provider.
fn dvm_display_provider_ready(log: &str) -> bool {
    let expected_stride = DvmGuiSurfacePoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
    )
    .stride_bytes;
    log.lines().any(|line| {
        let Some((_, fields)) = line.split_once("uiserver: display_get_info ") else {
            return false;
        };
        uiserver_display_field_is(fields, "width", DVM_DISPLAY_WIDTH)
            && uiserver_display_field_is(fields, "height", DVM_DISPLAY_HEIGHT)
            && uiserver_display_field_is(fields, "stride", expected_stride)
            && uiserver_display_field_is(fields, "bpp", 4)
            && uiserver_display_field_is(fields, "fmt", 1)
            // A DVM scanout is still the active primary provider. Requiring
            // both provenance bits prevents the smoke from accepting either a
            // generic primary framebuffer or a non-primary DVM aperture.
            && fields
                .split_whitespace()
                .any(|field| field == "flags=0xe")
    })
}

fn dvm_display_relay_ready(log: &str, physical_amdgpu: bool) -> bool {
    let expected_stride = DvmGuiSurfacePoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
    )
    .stride_bytes;
    let active = log.lines().any(|line| {
        let has_interrupt = line
            .split_whitespace()
            .find_map(|field| field.strip_prefix("irq_count="))
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|count| count > 0);
        let transport = if physical_amdgpu {
            line.contains("source-path=dmabuf")
                && line.contains("zero-copy=1")
                && line.contains("staged-damage-copy=0")
        } else {
            line.contains("source-path=staged-copy")
                && line.contains("zero-copy=0")
                && line.contains("staged-damage-copy=1")
        };
        line.contains("rustos-dvm-display: active")
            && line.contains(&format!("width={DVM_DISPLAY_WIDTH}"))
            && line.contains(&format!("height={DVM_DISPLAY_HEIGHT}"))
            && line.contains(&format!("stride={expected_stride}"))
            && line.contains("event=ivshmem-msix-uio")
            && has_interrupt
            && line.contains("format=BGRA8888")
            && transport
            && line.contains("gpu-composition=1")
            && line.contains("explicit-fence=1")
            && line.contains("scanout_buffers=3")
            && line.contains("cpu-final-compose=0")
    });
    active
        && log.lines().any(|line| {
            line.contains(
                "rustos-dvm-display: host confirmed peer readiness event=ivshmem-msix-uio",
            )
        })
        && log.lines().any(|line| {
            line.contains(
                "rustos-dvm-display: scheduler admitted policy=rr priority=9 rttime_soft_us=50000 rttime_hard_us=100000 rttime_hard_action=terminate",
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
    let mut last_block_error = None;
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
        let block_ready = if rustos_log.contains(RUSTOS_DVM_BLOCK_MARKER)
            && rustos_log.contains(RUSTOS_DVM_BLOCK_E2E_MARKER)
            && dvm_log.contains(DVM_BLOCK_READY_MARKER)
        {
            match verify_dvm_block_ready(layout) {
                Ok(()) => true,
                Err(error) => {
                    last_block_error = Some(error.to_string());
                    false
                }
            }
        } else {
            false
        };
        if dvm_display_provider_ready(&rustos_log)
            && dvm_display_relay_ready(&dvm_log, false)
            && surface_ready
            && block_ready
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "interactive display/storage was not proven ready within {:?}; surface={}; block={}; inspect {} and {}",
                INTERACTIVE_DISPLAY_READY_TIMEOUT,
                last_surface_error.unwrap_or_else(|| "no valid PRESENT yet".to_owned()),
                last_block_error.unwrap_or_else(|| "no exact block readiness yet".to_owned()),
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
    if !dvm_display_provider_ready(&rustos_log) || !dvm_display_relay_ready(&dvm_log, false) {
        bail!(
            "interactive KVM DVM acceptance lacks the active atomic GUI-DVM display contract; inspect {} and {}",
            layout.debugcon_log.display(),
            layout.dvm_serial_log.display(),
        );
    }
    if !rustos_log.contains(RUSTOS_DVM_BLOCK_MARKER)
        || !rustos_log.contains(RUSTOS_DVM_BLOCK_E2E_MARKER)
        || !dvm_log.contains(DVM_BLOCK_READY_MARKER)
    {
        bail!("interactive KVM DVM acceptance lacks the exact block transport readiness contract");
    }
    verify_dvm_block_ready(layout)?;
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
        let pageflip_completions = fields.split_whitespace().find_map(|field| {
            field
                .strip_prefix("pageflip_completions=")
                .and_then(|value| value.parse::<u64>().ok())
        });
        let frame_hz_milli = fields.split_whitespace().find_map(|field| {
            field
                .strip_prefix("frame_hz_milli=")
                .and_then(|value| value.parse::<u64>().ok())
        });
        let relay_cpu_copy_us = log_u64(fields, "relay_cpu_copy_us_avg");
        let atomic_commit_us = log_u64(fields, "atomic_commit_us_avg");
        let gpu_render_us_avg = log_u64(fields, "gpu_render_us_avg");
        let gpu_render_us_max = log_u64(fields, "gpu_render_us_max");
        let gpu_fence_completions = log_u64(fields, "gpu_fence_completions");
        let present_fence_completions = log_u64(fields, "present_fence_completions");
        let Some((
            pageflip_completions,
            frame_hz_milli,
            relay_cpu_copy_us,
            atomic_commit_us,
            gpu_render_us_avg,
            gpu_render_us_max,
            gpu_fence_completions,
            present_fence_completions,
        )) = pageflip_completions
            .zip(frame_hz_milli)
            .zip(relay_cpu_copy_us.zip(atomic_commit_us))
            .zip(gpu_render_us_avg.zip(gpu_render_us_max))
            .zip(gpu_fence_completions.zip(present_fence_completions))
            .map(
                |(
                    (((submissions, hz), (copy, commit)), (gpu_avg, gpu_max)),
                    (gpu_fences, present_fences),
                )| {
                    (
                        submissions,
                        hz,
                        copy,
                        commit,
                        gpu_avg,
                        gpu_max,
                        gpu_fences,
                        present_fences,
                    )
                },
            )
        else {
            continue;
        };
        if pageflip_completions == 0
            || frame_hz_milli < required_milli
            || relay_cpu_copy_us != 0
            || atomic_commit_us > MAX_DVM_DISPLAY_RELAY_US
            || gpu_render_us_avg == 0
            || gpu_render_us_avg > MAX_DVM_DISPLAY_RELAY_US
            || gpu_render_us_max == 0
            || gpu_render_us_max > MAX_DVM_GPU_RENDER_US
            || gpu_fence_completions != pageflip_completions
            || present_fence_completions != pageflip_completions
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

fn log_text_field_is(fields: &str, name: &str, expected: &str) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            == Some(expected)
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
    frame_hz_milli: u64,
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
        frame_hz_milli: log_u64(fields, "frame_hz_milli")?,
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

fn uiserver_profile_input_pipeline_healthy(
    log: &str,
    required_windows: usize,
    minimum_fps: Option<u32>,
) -> bool {
    let required_frame_hz_milli = minimum_fps.map(|fps| u64::from(fps).saturating_mul(1_000));
    let mut windows = Vec::new();
    for window in log.lines().filter_map(parse_ui_profile_input_window) {
        if required_frame_hz_milli.is_some_and(|minimum| window.frame_hz_milli < minimum)
            || window.input_events < MIN_UI_FPS_INPUT_EVENTS
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

fn wayclick_profile_meets_fps(log: &str, minimum_fps: u32, required_windows: usize) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    let mut consecutive = 0_usize;
    for fields in log.lines().filter_map(|line| {
        line.split_once("wayclick profile: ")
            .map(|(_, fields)| fields)
    }) {
        let Some(commit_hz) = log_u64(fields, "commit_hz_milli") else {
            consecutive = 0;
            continue;
        };
        let Some(callback_hz) = log_u64(fields, "callback_hz_milli") else {
            consecutive = 0;
            continue;
        };
        let Some(commits) = log_u64(fields, "commits") else {
            consecutive = 0;
            continue;
        };
        let Some(callbacks) = log_u64(fields, "callbacks") else {
            consecutive = 0;
            continue;
        };
        let Some(releases) = log_u64(fields, "buffer_releases") else {
            consecutive = 0;
            continue;
        };
        let Some(max_gap_ms) = log_u64(fields, "max_callback_gap_ms") else {
            consecutive = 0;
            continue;
        };
        let balanced = commits.abs_diff(callbacks) <= 2 && callbacks.abs_diff(releases) <= 2;
        if commit_hz < required_milli
            || callback_hz < required_milli
            || commits == 0
            || callbacks == 0
            || releases == 0
            || max_gap_ms > MAX_UI_INPUT_GAP_MS
            || !balanced
        {
            consecutive = 0;
            continue;
        }
        consecutive = consecutive.saturating_add(1);
        if consecutive >= required_windows {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WayclickProfileObservation {
    windows: usize,
    commit_hz_milli_min: u64,
    commit_hz_milli_max: u64,
    callback_hz_milli_min: u64,
    callback_hz_milli_max: u64,
    max_callback_gap_ms: u64,
    max_redraw_ms: u64,
}

fn wayclick_profile_observation(log: &str) -> Option<WayclickProfileObservation> {
    let mut observation = WayclickProfileObservation {
        windows: 0,
        commit_hz_milli_min: u64::MAX,
        commit_hz_milli_max: 0,
        callback_hz_milli_min: u64::MAX,
        callback_hz_milli_max: 0,
        max_callback_gap_ms: 0,
        max_redraw_ms: 0,
    };
    for fields in log.lines().filter_map(|line| {
        line.split_once("wayclick profile: ")
            .map(|(_, fields)| fields)
    }) {
        let Some(commit_hz) = log_u64(fields, "commit_hz_milli") else {
            continue;
        };
        let Some(callback_hz) = log_u64(fields, "callback_hz_milli") else {
            continue;
        };
        let Some(max_gap_ms) = log_u64(fields, "max_callback_gap_ms") else {
            continue;
        };
        observation.windows = observation.windows.saturating_add(1);
        observation.commit_hz_milli_min = observation.commit_hz_milli_min.min(commit_hz);
        observation.commit_hz_milli_max = observation.commit_hz_milli_max.max(commit_hz);
        observation.callback_hz_milli_min = observation.callback_hz_milli_min.min(callback_hz);
        observation.callback_hz_milli_max = observation.callback_hz_milli_max.max(callback_hz);
        observation.max_callback_gap_ms = observation.max_callback_gap_ms.max(max_gap_ms);
        observation.max_redraw_ms = observation
            .max_redraw_ms
            .max(log_u64(fields, "max_redraw_ms").unwrap_or(0));
    }
    (observation.windows != 0).then_some(observation)
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
    if !uiserver_profile_input_pipeline_healthy(&log, options.ui_proof_windows, Some(minimum_fps)) {
        bail!(
            "KVM UI proof found no single consecutive window set satisfying both FPS and input loss/backlog/gap/cursor requirements; inspect {}",
            layout.debugcon_log.display(),
        );
    }
    if !wayclick_profile_meets_fps(&log, minimum_fps, options.ui_proof_windows) {
        bail!(
            "KVM WayClick FPS proof found no consecutive commit/frame-callback/release window set at or above {} FPS; observed={:?}; inspect {}",
            minimum_fps,
            wayclick_profile_observation(&log),
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
        DEFAULT_UI_FPS_ACTIVE_WINDOWS, DVM_BLOCK_READY_MARKER, DVM_CONTROL_AUTHENTICATION,
        DVM_CONTROL_CAPABILITIES, DVM_CONTROL_PROTOCOL, DVM_CONTROL_STATE, DVM_CONTROL_TRANSPORT,
        DVM_DISPLAY_REGION_BYTES, DVM_GPU_COMPOSITOR_MARKER, DVM_KEYBOARD_INGRESS_MARKER,
        DVM_POINTER_INGRESS_MARKER, DvmNetworkCounters, GuestDisplay, PHYSICAL_GPU_PROFILES,
        RUSTOS_BOOT_MARKER, RUSTOS_DVM_BLOCK_E2E_MARKER, RUSTOS_DVM_BLOCK_MARKER,
        RUSTOS_GPU_SCENE_COMPILER_MARKER, RUSTOS_INIT_IDENTITY_MARKER,
        RUSTOS_POST_INIT_PROVENANCE_MARKER, VIRTUAL_GPU_EVIDENCE, WayclickProfileObservation,
        append_dvm_display_pixels, append_dvm_input_devices, append_dvm_network_device,
        append_dvm_virtual_gpu, append_physical_gpu, claim_physical_gpu_launch_in,
        dvm_display_failure, dvm_display_provider_ready, dvm_display_relay_meets_fps,
        dvm_display_relay_ready, dvm_gpu_compositor_ready, dvm_gpu_device, dvm_machine,
        dvm_physical_frames_ready, dvm_pointer_device, is_sha256, mesa_dri_prime_for_pci_bdf,
        parse_dvm_control_contract_text, parse_manifest_text, parse_smoke_options,
        physical_gpu_profile, prepare_runtime_log, qemu_display_backend,
        runtime_stall_or_crash_observed, select_smoke_guest_display,
        uiserver_has_interactive_slow_loop, uiserver_idle_ticks_healthy,
        uiserver_profile_input_pipeline_healthy, uiserver_profile_meets_fps,
        validate_manifest_values, vfio_device_cdev_path, wayclick_profile_meets_fps,
        wayclick_profile_observation,
    };
    use std::{fs, path::Path, process::Command};

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
    fn smoke_readiness_budget_starts_only_after_both_guests_spawn() {
        let source = include_str!("kvm.rs");
        let spawn = source
            .find("let (mut rustos, mut dvm) = spawn_guests(")
            .expect("parallel guest spawn");
        let deadline = source
            .find("let deadline = Instant::now() + options.timeout;")
            .expect("readiness deadline");
        assert!(spawn < deadline);
    }

    #[test]
    fn dvm_display_mode_requires_the_observed_display_contract() {
        let options = parse_smoke_options(vec!["--gui-dvm-surfaces".into()].into_iter()).unwrap();
        assert!(options.gui_dvm_surfaces);
        assert!(dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=7168 bpp=4 fmt=1 flags=0xe gen=1"
        ));
        assert!(!dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=7168 bpp=4 fmt=1 flags=0x6 gen=1"
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
                .contains(&RUSTOS_DVM_BLOCK_E2E_MARKER.to_owned())
        );
        assert!(
            options
                .expected_dvm_markers
                .contains(&DVM_BLOCK_READY_MARKER.to_owned())
        );
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
    fn ui_profile_gate_rejects_tremble_loss_and_stalls() {
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
            "../../../driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c"
        );
        assert!(source.contains("UI_SET_KEYBIT, BTN_LEFT"));
        assert!(source.contains("UI_SET_ABSBIT, ABS_X"));
        assert!(source.contains("UI_SET_ABSBIT, ABS_Y"));
        assert!(source.contains("selftest->motion_phase == 0U"));
        assert!(source.contains("#define INPUT_SELFTEST_CYCLES 4000U"));
        assert!(source.contains("#define INPUT_SELFTEST_LEG_CYCLES 64U"));
        assert!(source.contains("#define INPUT_SELFTEST_POLL_MS 10"));
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
            "../../../driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c"
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
            "../../../driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c"
        );
        let init = include_str!(
            "../../../driver-domains/linux/board/overlay/etc/init.d/S48rustos-dvm-net"
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
            "../../../driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-gpu-probe.c"
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
            "schema=8\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-xz\ndata-plane=hostd-input-ring-msix\ncontrol-plane=agent-v1-control\ncontrol-protocol=agent-v1\ncontrol-state=control\ncontrol-transport=kvm-vsock\ncontrol-authentication=dvm-agent-hmac-sha256-v1\ncontrol-capabilities=health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream\ncontrol-contract-sha256={hash}\nbuildroot_version=2026.05\nlinux_version=6.12.94\nnvidia-open-version=580.173.02\nnvidia-open-sha256=8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b\nnvidia-open-redistribute=no\ndisplay-kernel-modules=i915,xe,amdgpu,nvidia-drm\nmodule-signing-enforced=yes\nmodule-signing-cert-sha256={hash}\nkernel_sha256={hash}\nrootfs_sha256={hash}\nconfig_sha256={hash}\nkernel-config-sha256={hash}\nsources_lock_sha256={hash}\n"
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
            "../../../driver-domains/linux/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"
        );
        let invalid = contract_source.replace(
            "CONTROL_CAPABILITIES=health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream",
            "CONTROL_CAPABILITIES=health,network-rx",
        );
        assert!(parse_dvm_control_contract_text(&invalid, "invalid contract").is_err());
    }
}
