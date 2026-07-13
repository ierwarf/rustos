use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use fatfs::Seek as FatSeek;
use fatfs::Write as FatWrite;
use fs_err as fs;
use driver_domain_protocol::{
    DVM_DISPLAY_HEADER_BYTES, DVM_DISPLAY_INITIAL_GENERATION, DVM_NET_APERTURE_BYTES,
    DvmDisplayHeader, DvmNetHeader,
};
use rustos_driver_domain_host::{
    ControlContract as HostControlContract, DEFAULT_CONTROL_PORT, HostControlListener, ProbeResult,
    UnixInputSink,
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
const DVM_CONTROL_AUTHENTICATION: &str = "kvm-host-bound";
const DVM_CONTROL_CAPABILITIES: &str =
    "health,device-inventory,driver-inventory,input-stream";
const RUSTOS_BOOT_MARKER: &str = "rootd: core services ready, spawning initd via loaderd";
const DVM_KEYBOARD_INGRESS_MARKER: &str = "inputd: DVM keyboard ingress observed";
const DVM_POINTER_INGRESS_MARKER: &str = "inputd: DVM pointer ingress observed";
const DVM_DISPLAY_WIDTH: u32 = 1600;
const DVM_DISPLAY_HEIGHT: u32 = 900;
const DVM_DISPLAY_REGION_BYTES: u64 = 8 * 1024 * 1024;
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
    rustos_input_socket: PathBuf,
    dvm_display_shmem: Option<PathBuf>,
    dvm_network_shmem: Option<PathBuf>,
}

#[derive(Debug)]
struct SmokeOptions {
    dry_run: bool,
    exercise_input: bool,
    exercise_network: bool,
    dvm_display_shmem: bool,
    dvm_network_shmem: bool,
    min_ui_fps: Option<u32>,
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
    let control_relay =
        start_dvm_input_relay(config, options.timeout, layout.rustos_input_socket.clone())?;
    let (mut rustos, mut dvm) = spawn_guests(
        &qemu,
        config,
        &artifacts,
        &layout,
        &options,
        GuestDisplay::Headless,
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
    if let Some(shared_display) = layout.dvm_display_shmem.as_deref() {
        verify_dvm_display_surface(shared_display)?;
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
        if probe.driver_inventory.virtio_net_bound { "bound" } else { "missing" },
        if probe.driver_inventory.virtio_gpu_bound { "bound" } else { "missing" },
    );
    Ok(())
}

/// Start the normal KVM driver-domain topology as an interactive session.
/// Unlike `kvm-smoke`, this has no readiness deadline or success criteria: it
/// remains alive until the user closes the DVM QEMU window or interrupts it.
pub(crate) fn kvm_run_command(config: &Config) -> Result<()> {
    let artifacts = verify_dvm_artifacts(config)?;
    let qemu = require_qemu(config)?;
    let options = SmokeOptions {
        dry_run: false,
        exercise_input: false,
        exercise_network: false,
        dvm_display_shmem: true,
        dvm_network_shmem: true,
        min_ui_fps: None,
        timeout: Duration::ZERO,
        expected_markers: Vec::new(),
    };
    let layout = prepare_layout(config, &options)?;
    require_vhost_vsock()?;
    start_dvm_input_relay_unbounded(config, layout.rustos_input_socket.clone())?;
    let (mut rustos, mut dvm) = spawn_guests(
        &qemu,
        config,
        &artifacts,
        &layout,
        &options,
        GuestDisplay::DvmGtk,
    )?;

    println!(
        "xtask: interactive KVM DVM session is running; use the Linux DVM QEMU window for display/input and close it or press Ctrl-C to stop"
    );
    let status = dvm.wait().context("wait for Linux DVM QEMU session")?;
    stop_guest(&mut rustos);
    if status.success() {
        Ok(())
    } else {
        bail!("interactive Linux DVM QEMU session exited with {status}")
    }
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
  --min-ui-fps <fps>   enable the private KVM-only UI profiler and require a
                       profile window at or above the requested integer FPS
  --dvm-display-shmem  replace RustOS's native virtio-gpu test device with a
                       host-initialized ivshmem framebuffer owned by the DVM path
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

fn parse_smoke_options<I>(mut args: I) -> Result<SmokeOptions>
where
    I: Iterator<Item = String>,
{
    let mut options = SmokeOptions {
        dry_run: false,
        exercise_input: false,
        exercise_network: false,
        dvm_display_shmem: false,
        dvm_network_shmem: false,
        min_ui_fps: None,
        timeout: Duration::from_secs(MAX_SMOKE_TIMEOUT),
        expected_markers: vec![RUSTOS_BOOT_MARKER.to_owned()],
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => options.dry_run = true,
            "--exercise-input" => options.exercise_input = true,
            "--exercise-network" => options.exercise_network = true,
            "--dvm-display-shmem" => options.dvm_display_shmem = true,
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
    require_manifest_value(values, "data-plane", "hostd-rdi2-input")?;
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
    let runtime_disk = run_dir.join("rustos-kvm.img");
    fs::copy(&config.boot_disk_image, &runtime_disk).with_context(|| {
        format!(
            "failed to create KVM runtime disk from {}",
            config.boot_disk_image.display()
        )
    })?;
    if options.min_ui_fps.is_some() {
        enable_private_ui_profile(&runtime_disk)?;
    }
    if options.exercise_network {
        enable_private_network_exercise(&runtime_disk)?;
    }
    let dvm_display_shmem = if options.dvm_display_shmem {
        let path = run_dir.join("dvm-display.ivshmem");
        create_dvm_display_shmem(&path)?;
        Some(path)
    } else {
        None
    };
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
    let rustos_input_socket = run_dir.join("rustos-dvm-input.sock");
    fs::write(&debugcon_log, "")?;
    fs::write(&rustos_serial_log, "")?;
    fs::write(&dvm_serial_log, "")?;
    fs::write(&rustos_stderr_log, "")?;
    fs::write(&dvm_stderr_log, "")?;
    match fs::remove_file(&rustos_input_socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "remove stale RustOS DVM input socket {}",
                    rustos_input_socket.display()
                )
            });
        }
    }

    Ok(KvmLayout {
        run_dir,
        runtime_disk,
        debugcon_log,
        rustos_serial_log,
        dvm_serial_log,
        rustos_stderr_log,
        dvm_stderr_log,
        rustos_input_socket,
        dvm_display_shmem,
        dvm_network_shmem,
    })
}

fn create_dvm_network_shmem(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!("KVM shared-network path contains an unsupported QEMU option separator: {}", path.display());
    }
    let header = DvmNetHeader::new(DVM_NET_REGION_BYTES, 1);
    if !header.is_valid() {
        bail!("refusing to create invalid DVM shared-network header");
    }
    let mut file = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(path)?;
    file.set_len(DVM_NET_REGION_BYTES)?;
    file.write_all(&header.encode())?;
    file.sync_all()?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Allocate the only shared display object used by the KVM smoke topology.
///
/// The runner owns the shape and initial contents before either guest starts.
/// Guests receive the same plain ivshmem aperture, but RustOS accepts only the
/// fixed header and never treats its contents as a command stream.
fn create_dvm_display_shmem(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM shared-display path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmDisplayHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
        DVM_DISPLAY_INITIAL_GENERATION,
    );
    if !header.is_valid() {
        bail!("refusing to create invalid DVM shared-display header");
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
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "restrict DVM shared display permissions {}",
            path.display()
        )
    })?;
    Ok(())
}

/// A successful shared-display smoke must show both the unchanged contract and
/// an actual RustOS framebuffer write. This detects a host-only aperture that
/// was mapped but never made it into the kernel display provider.
fn verify_dvm_display_surface(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open DVM shared display {}", path.display()))?;
    let mut encoded = [0_u8; DvmDisplayHeader::encoded_len()];
    file.read_exact(&mut encoded)
        .with_context(|| format!("read DVM shared display header {}", path.display()))?;
    let header = DvmDisplayHeader::decode(&encoded)
        .context("DVM shared display header changed or became invalid during smoke")?;
    if header.region_bytes != DVM_DISPLAY_REGION_BYTES
        || header.width != DVM_DISPLAY_WIDTH
        || header.height != DVM_DISPLAY_HEIGHT
    {
        bail!(
            "DVM shared display header differs from launch contract: region={} width={} height={}",
            header.region_bytes,
            header.width,
            header.height
        );
    }
    file.seek(SeekFrom::Start(u64::from(DVM_DISPLAY_HEADER_BYTES)))?;
    let mut remaining = header.frame_bytes;
    let mut block = [0_u8; 4096];
    let mut wrote_pixels = false;
    while remaining > 0 {
        let bytes = usize::try_from(remaining.min(block.len() as u64))?;
        file.read_exact(&mut block[..bytes])?;
        if block[..bytes].iter().any(|byte| *byte != 0) {
            wrote_pixels = true;
            break;
        }
        remaining -= bytes as u64;
    }
    if !wrote_pixels {
        bail!("DVM shared display provider published but RustOS wrote no framebuffer pixels");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct DvmNetworkCounters {
    tx_producer: u32,
    tx_consumer: u32,
    rx_producer: u32,
    rx_consumer: u32,
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
    rustos_input_socket: PathBuf,
) -> Result<Receiver<Result<ProbeResult>>> {
    let contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    let contract = HostControlContract::from_env_file(&contract_path)?;
    let listener = HostControlListener::bind(DVM_GUEST_CID, DEFAULT_CONTROL_PORT, contract)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let sender_ready = sender.clone();
        let result = (|| {
            let mut sink = UnixInputSink::connect(&rustos_input_socket, timeout)?;
            listener.relay_input_once_with_ready(timeout, &mut sink, |probe| {
                sender_ready
                    .send(Ok(probe.clone()))
                    .context("report Linux DVM input relay readiness")
            })
        })();
        if let Err(error) = result {
            let _ = sender.try_send(Err(error));
        }
    });
    Ok(receiver)
}

fn start_dvm_input_relay_unbounded(config: &Config, rustos_input_socket: PathBuf) -> Result<()> {
    let contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    let contract = HostControlContract::from_env_file(&contract_path)?;
    let listener = HostControlListener::bind(DVM_GUEST_CID, DEFAULT_CONTROL_PORT, contract)?;
    thread::spawn(move || {
        loop {
            let Ok(mut sink) = UnixInputSink::connect(&rustos_input_socket, Duration::from_secs(1))
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

fn spawn_guests(
    qemu: &Path,
    config: &Config,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
    options: &SmokeOptions,
    guest_display: GuestDisplay,
) -> Result<(Child, Child)> {
    let mut rustos_command = Command::new(qemu);
    rustos_command
        .arg("-name")
        .arg("rustos-kvm")
        .args([
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-m",
            "2048",
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
        ])
        .arg("-chardev")
        .arg(format!(
            "file,id=debugcon,path={}",
            layout.debugcon_log.display()
        ))
        .arg("-device")
        .arg("isa-debugcon,iobase=0xe9,chardev=debugcon")
        .arg("-chardev")
        .arg(format!(
            "file,id=serial,path={}",
            layout.rustos_serial_log.display()
        ))
        .args(["-serial", "chardev:serial"])
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-input,path={},server=on,wait=off",
            layout.rustos_input_socket.display()
        ))
        .arg("-device")
        .arg("isa-serial,chardev=dvm-input,index=1");
    if let Some(shared_display) = layout.dvm_display_shmem.as_deref() {
        append_dvm_display_ivshmem(&mut rustos_command, shared_display);
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

    let mut dvm_command = Command::new(qemu);
    let dvm_append = if options.exercise_input {
        "console=ttyS0 rustos.dvm.input-selftest=1"
    } else {
        "console=ttyS0"
    };
    let dvm_display = match guest_display {
        GuestDisplay::Headless => "none",
        // The GTK backend defaults to `zoom-to-fit=on` for virtio-GPU.  That
        // makes its initial small host window a guest resize request, so the
        // DVM KMS relay selects 640x480 and needlessly downscales RustOS's
        // fixed 1600x900 transport.  Keep the guest mode authoritative for
        // the interactive topology; GTK will resize its window to the DVM
        // scanout instead of feeding its bootstrap size back into the guest.
        GuestDisplay::DvmGtk => "gtk,show-tabs=off,zoom-to-fit=off",
    };
    dvm_command
        .arg("-name")
        .arg("rustos-linux-dvm-kvm")
        .args([
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-m",
            "512",
            "-smp",
            "1",
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
            "file,id=serial,path={}",
            layout.dvm_serial_log.display()
        ))
        .args(["-serial", "chardev:serial"])
        .arg("-device")
        .arg(format!("vhost-vsock-pci,guest-cid={DVM_GUEST_CID}"))
        .arg("-device")
        .arg("virtio-keyboard-pci,id=dvm-keyboard")
        .arg("-device")
        .arg("virtio-mouse-pci,id=dvm-pointer")
        .arg("-netdev")
        .arg("user,id=dvm-net")
        .arg("-device")
        .arg("virtio-net-pci,netdev=dvm-net,id=dvm-virtio-net,mac=52:54:00:12:34:56")
        .arg("-device")
        .arg(format!(
            // The DVM display transport is deliberately fixed at 1600x900.
            // GTK's resize-aware EDID starts at its tiny bootstrap window and
            // can otherwise replace that mode with 640x480 before the Linux
            // DRM relay starts.  Disable EDID for this private fixed-mode
            // appliance so QEMU's explicit xres/yres are authoritative.
            "virtio-gpu-pci,id=dvm-virtio-gpu,xres={},yres={},edid=off",
            DVM_DISPLAY_WIDTH, DVM_DISPLAY_HEIGHT
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&layout.dvm_stderr_log)?));
    if let Some(shared_display) = layout.dvm_display_shmem.as_deref() {
        append_dvm_display_ivshmem(&mut dvm_command, shared_display);
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

fn append_dvm_display_ivshmem(command: &mut Command, path: &Path) {
    command
        .arg("-object")
        .arg(format!(
            "memory-backend-file,id=dvm-display-shm,mem-path={},size={},share=on",
            path.display(),
            DVM_DISPLAY_REGION_BYTES
        ))
        .arg("-device")
        .arg("ivshmem-plain,memdev=dvm-display-shm");
}

fn append_dvm_network_ivshmem(command: &mut Command, path: &Path) {
    command
        .arg("-object")
        .arg(format!(
            "memory-backend-file,id=dvm-network-shm,mem-path={},size={},share=on",
            path.display(), DVM_NET_REGION_BYTES
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
        if control_ready.is_none() {
            match control_relay.try_recv() {
                Ok(Ok(probe)) => {
                    control_ready = Some(probe);
                }
                Ok(Err(error)) => {
                    bail!("Linux DVM input relay failed before readiness: {error:#}");
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!("Linux DVM host input relay terminated without a readiness result")
                }
            }
        }
        let ui_fps_ready = options
            .min_ui_fps
            .is_none_or(|minimum| uiserver_profile_meets_fps(&rustos_log, minimum));
        let dvm_display_ready =
            !options.dvm_display_shmem
                || (dvm_display_provider_ready(&rustos_log) && dvm_display_relay_ready(&dvm_log));
        let dvm_network_ready =
            !options.dvm_network_shmem || dvm_network_relay_ready(&dvm_log);
        let dvm_network_traffic_ready = if options.exercise_network {
            let shared_network = layout
                .dvm_network_shmem
                .as_deref()
                .context("network exercise lost its shared DVM network aperture")?;
            dvm_network_counters(shared_network)?.round_trip_observed()
                && rustos_log.contains(NETPROBE_QEMU_REACHABLE_MARKER)
        } else {
            true
        };
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
                "KVM parallel boot did not reach readiness within {:?}; RustOS missing={:?}; dvm-display-ready={}; dvm-network-ready={}; dvm-network-traffic-ready={}; host-input-relay-pending={}; inspect {}, {}, {}, and {}",
                options.timeout,
                missing_rustos,
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
            && fields.split_whitespace().any(|field| field == "flags=0x2")
    })
}

fn dvm_display_relay_ready(log: &str) -> bool {
    log.lines().any(|line| {
        line.contains("rustos-dvm-display: active")
            && line.contains(&format!("width={DVM_DISPLAY_WIDTH}"))
            && line.contains(&format!("height={DVM_DISPLAY_HEIGHT}"))
            && line.contains(&format!("stride={}", DVM_DISPLAY_WIDTH * 4))
            && line.contains("format=BGRA8888 double-buffered")
    })
}

fn dvm_network_relay_ready(log: &str) -> bool {
    log.lines().any(|line| {
        line.contains("rustos-dvm-net: active")
            && line.contains("interface=eth0")
            && line.contains("mtu=1514")
            && line.contains("slots=64")
    })
}

fn uiserver_display_field_is(fields: &str, name: &str, expected: u32) -> bool {
    fields.split_whitespace().any(|field| {
        field
            .split_once('=')
            .is_some_and(|(key, value)| key == name && value == expected.to_string())
    })
}

fn uiserver_profile_meets_fps(log: &str, minimum_fps: u32) -> bool {
    let required_milli = u64::from(minimum_fps).saturating_mul(1_000);
    log.lines().any(|line| {
        // Service logs normally carry an observability prefix, while early
        // debugcon output may be bare. The KVM gate accepts either form but
        // still requires the exact profile payload.
        line.split_once("uiserver profile: ")
            .map(|(_, profile)| profile)
            .and_then(|fields| {
                fields.split_whitespace().find_map(|field| {
                    field
                        .strip_prefix("frame_hz_milli=")
                        .and_then(|value| value.parse::<u64>().ok())
                })
            })
            .is_some_and(|rate| rate >= required_milli)
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
        DVM_CONTROL_AUTHENTICATION, DVM_CONTROL_CAPABILITIES, DVM_CONTROL_PROTOCOL,
        DVM_CONTROL_STATE, DVM_CONTROL_TRANSPORT, DVM_KEYBOARD_INGRESS_MARKER,
        DVM_POINTER_INGRESS_MARKER, RUSTOS_BOOT_MARKER, DvmNetworkCounters,
        dvm_display_provider_ready,
        dvm_display_relay_ready, dvm_network_relay_ready, is_sha256,
        parse_dvm_control_contract_text, parse_manifest_text, parse_smoke_options,
        uiserver_profile_meets_fps, validate_manifest_values,
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
        assert!(options
            .expected_markers
            .contains(&DVM_KEYBOARD_INGRESS_MARKER.to_owned()));
        assert!(options
            .expected_markers
            .contains(&DVM_POINTER_INGRESS_MARKER.to_owned()));
    }

    #[test]
    fn dvm_display_mode_requires_the_observed_display_contract() {
        let options = parse_smoke_options(vec!["--dvm-display-shmem".into()].into_iter()).unwrap();
        assert!(options.dvm_display_shmem);
        assert!(dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=6400 bpp=4 fmt=1 flags=0x2 gen=1"
        ));
        assert!(!dvm_display_provider_ready(
            "[INFO ] service=uiserver uiserver: display_get_info attempt=1 width=1600 height=900 stride=6400 bpp=4 fmt=1 flags=0x3 gen=1"
        ));
        assert!(dvm_display_relay_ready(
            "rustos-dvm-display: active width=1600 height=900 stride=6400 format=BGRA8888 double-buffered"
        ));
        assert!(dvm_network_relay_ready(
            "rustos-dvm-net: active interface=eth0 mtu=1514 slots=64"
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
        };
        assert!(observed.is_valid(64));
        assert!(observed.round_trip_observed());
        assert!(!DvmNetworkCounters {
            tx_producer: 65,
            tx_consumer: 0,
            rx_producer: 0,
            rx_consumer: 0,
        }
        .is_valid(64));
    }

    #[test]
    fn ui_fps_option_is_bounded_and_profile_rate_is_strict() {
        let options = parse_smoke_options(
            vec!["--min-ui-fps".into(), "20".into()].into_iter(),
        )
        .unwrap();
        assert_eq!(options.min_ui_fps, Some(20));
        assert!(parse_smoke_options(vec!["--min-ui-fps".into(), "241".into()].into_iter())
            .is_err());
        assert!(uiserver_profile_meets_fps(
            "[INFO ] service=uiserver uiserver profile: elapsed_ms=1000 frame_hz_milli=20000 full=0 part=20",
            20,
        ));
        assert!(!uiserver_profile_meets_fps(
            "uiserver profile: elapsed_ms=1000 frame_hz_milli=19999 full=0 part=19",
            20,
        ));
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
            "schema=4\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-xz\ndata-plane=hostd-rdi2-input\ncontrol-plane=agent-v1-control\ncontrol-protocol=agent-v1\ncontrol-state=control\ncontrol-transport=kvm-vsock\ncontrol-authentication=kvm-host-bound\ncontrol-capabilities=health,device-inventory,driver-inventory,input-stream\ncontrol-contract-sha256={hash}\nkernel_sha256={hash}\nrootfs_sha256={hash}\nconfig_sha256={hash}\nsources_lock_sha256={hash}\n"
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
