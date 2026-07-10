use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::Result;
use crate::config::Config;
use crate::util::{create_temp_dir, env_string, read_trimmed, resolve_command_path};

const DEFAULT_QEMU_DISPLAY_DEVICE_ID: &str = "rustos_video0";

struct RunOptions {
    profile: String,
    accel: String,
    usb_input: bool,
    usb_input_device: UsbInputDevice,
    debugcon: DebugconMode,
    qemu_log: QemuLogMode,
    timeout: Option<Duration>,
    expect_markers: Vec<String>,
    summarize_log: bool,
    vfio_force: bool,
    auto_phoenix3_passthrough: bool,
    network: bool,
    vfio_hosts: Vec<String>,
    fault_rules: Vec<String>,
    qemu_user_args: Vec<String>,
}

impl RunOptions {
    fn from_env() -> Self {
        Self {
            profile: env_string("RUSTOS_QEMU_PROFILE").unwrap_or_else(|| String::from("default")),
            accel: env_string("RUSTOS_QEMU_ACCEL").unwrap_or_default(),
            usb_input: false,
            usb_input_device: env_string("RUSTOS_QEMU_USB_INPUT_DEVICE")
                .as_deref()
                .and_then(UsbInputDevice::parse)
                .unwrap_or_default(),
            debugcon: DebugconMode::File,
            qemu_log: QemuLogMode::None,
            timeout: None,
            expect_markers: Vec::new(),
            summarize_log: false,
            vfio_force: false,
            auto_phoenix3_passthrough: false,
            network: !matches!(
                env_string("RUSTOS_QEMU_NETWORK").as_deref(),
                Some("0" | "false" | "False" | "off" | "Off" | "no" | "No")
            ),
            vfio_hosts: Vec::new(),
            fault_rules: env_string("RUSTOS_FAULTS")
                .map(|value| {
                    value
                        .split(';')
                        .map(str::trim)
                        .filter(|rule| !rule.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            qemu_user_args: Vec::new(),
        }
    }
}

struct RunSession {
    temp_dir: PathBuf,
    debugcon_tail: Option<Child>,
}

struct PreparedRun {
    qemu_bin: PathBuf,
    session: RunSession,
    profile_args: Vec<OsString>,
    vfio_args: Vec<OsString>,
    gpu_args: Vec<OsString>,
    usb_args: Vec<OsString>,
    usb_input_device: UsbInputDevice,
    network_args: Vec<OsString>,
    display_args: Vec<OsString>,
    debugcon_args: Vec<OsString>,
    qemu_log_args: Vec<OsString>,
    fault_args: Vec<OsString>,
    qemu_user_args: Vec<String>,
    debugcon_log: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum QemuBootDisk {
    Ahci,
    Nvme,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum UsbInputDevice {
    #[default]
    Tablet,
    Mouse,
}

impl UsbInputDevice {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "tablet" | "usb-tablet" | "usbtablet" | "absolute" | "abs" => Some(Self::Tablet),
            "mouse" | "usb-mouse" | "usbmouse" | "relative" | "rel" => Some(Self::Mouse),
            _ => None,
        }
    }

    fn qemu_device_arg(self) -> String {
        match self {
            Self::Tablet => format!(
                "usb-tablet,bus=xhci.0,id=usbtablet,display={DEFAULT_QEMU_DISPLAY_DEVICE_ID}"
            ),
            Self::Mouse => String::from("usb-mouse,bus=xhci.0,id=usbmouse"),
        }
    }

    fn is_absolute(self) -> bool {
        matches!(self, Self::Tablet)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Tablet => "usbtablet",
            Self::Mouse => "usbmouse",
        }
    }
}

#[derive(Clone, Copy)]
struct QemuProfileSpec {
    boot_disk: QemuBootDisk,
    memory: &'static str,
    cpu_when_kvm: Option<&'static str>,
    cpu_other: Option<&'static str>,
    smp: Option<&'static str>,
    rtc: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugconMode {
    File,
    Stdio,
    Null,
}

impl DebugconMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "file" => Some(Self::File),
            "stdio" => Some(Self::Stdio),
            "null" | "off" => Some(Self::Null),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QemuLogMode {
    None,
    Interrupt,
}

impl QemuLogMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" | "null" | "off" => Some(Self::None),
            "int" | "interrupt" => Some(Self::Interrupt),
            _ => None,
        }
    }
}

impl RunSession {
    fn create() -> Result<Self> {
        let temp_dir = create_temp_dir("rustos-qemu")?;
        Ok(Self {
            temp_dir,
            debugcon_tail: None,
        })
    }
}

impl Drop for RunSession {
    fn drop(&mut self) {
        if let Some(mut tail) = self.debugcon_tail.take() {
            let _ = tail.kill();
            let _ = tail.wait();
        }
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

const PROBE_BAD_MARKERS: [&str; 5] = [
    "[PANIC]",
    "Unhandled exception:",
    "framebuffer present rejected",
    "xhci event: type=0",
    "stale display surface detected, rebuilding",
];
const PROBE_BOOT_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);
const PROBE_QMP_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_STEP_DELAY: Duration = Duration::from_millis(25);
const PROBE_STRESS_DURATION_DEFAULT: Duration = Duration::from_secs(20);
const PROBE_HEARTBEAT_STALL_DEFAULT: Duration = Duration::from_secs(15);
const PROBE_INPUT_START_DEFAULT: Duration = Duration::from_secs(10);
const PROBE_INPUT_STALL_DEFAULT: Duration = Duration::from_millis(2500);
const QEMU_WAIT_POLL: Duration = Duration::from_millis(100);
const FAULT_FW_CFG_NAME: &str = "opt/rustos/fault-injection";

const RUN_SUMMARY_MARKERS: &[&str] = &[
    "[PANIC]",
    "[KERNEL FATAL]",
    "Unhandled exception",
    "panic",
    "fatal",
    "error",
    "failed",
    "timeout",
    "exception",
    "boot volume",
    "boot-volume-handle",
    "storage:",
    "ahci-16m-boundary",
    "ahci",
    "nvme",
    "driver loader:",
    "driver module",
    "module-init",
    "module loaded",
    ".ko",
    "linux compat:",
    "virtio",
    "virtio_net",
    "virtio-net",
    "virtio-net-driver",
    "virtio-net-init",
    "virtio-net-tcp",
    "__register_virtio_driver",
    "netdev",
    "netdev-register",
    "netdev-carrier",
    "carrier",
    "AF_INET",
    "af-inet",
    "TCP connect",
    "HTTP",
    "google",
    "init bootstrap",
    "init-bootstrap",
    "initd",
    "runtimed",
    "sessiond",
    "uiserver",
    "DisplayUnavailable",
    "first present",
    "wayland",
];

const COMMERCIAL_MAX_READY_MARKERS: &[&str] = &[
    "rootd: core services ready, spawning initd via loaderd",
    "rootd: initd spawned",
    "devmgrd: device policy endpoint registered",
    "inputd: input policy endpoint registered",
    "runtimed: session policy endpoint registered",
    "uiserver: wayland compositor ready on /run/user/1000/wayland-0",
    "uiserver: ui ready notified",
    "launched wayclick.desktop",
    "storaged: storage policy endpoint registered",
];

#[derive(Clone, Copy)]
enum ScenarioCommand {
    Run,
    ProbeDisplay,
}

struct QemuScenario {
    name: &'static str,
    description: &'static str,
    command: ScenarioCommand,
    args: fn(u64) -> Vec<String>,
}

impl QemuScenario {
    fn command_name(&self) -> &'static str {
        match self.command {
            ScenarioCommand::Run => "run",
            ScenarioCommand::ProbeDisplay => "probe-display",
        }
    }
}

fn qemu_scenarios() -> Vec<QemuScenario> {
    vec![
        QemuScenario {
            name: "boot-smoke",
            description: "headless AHCI boot with debug summary and no network device",
            command: ScenarioCommand::Run,
            args: |timeout| {
                scenario_args(
                    timeout,
                    &["--no-network"],
                    &["--no-reboot", "-display", "none"],
                )
            },
        },
        QemuScenario {
            name: "display-probe",
            description: "headless display probe with screendump/non-black validation",
            command: ScenarioCommand::ProbeDisplay,
            args: |timeout| scenario_args(timeout, &["--no-network"], &["--no-reboot"]),
        },
        QemuScenario {
            name: "usb-input-display",
            description: "display probe with USB keyboard/tablet input attached",
            command: ScenarioCommand::ProbeDisplay,
            args: |timeout| {
                scenario_args(timeout, &["--usb-input", "--no-network"], &["--no-reboot"])
            },
        },
        QemuScenario {
            name: "nvme-smoke",
            description: "headless NVMe boot profile smoke test",
            command: ScenarioCommand::Run,
            args: |timeout| {
                scenario_args(
                    timeout,
                    &["--profile", "nvme", "--no-network"],
                    &["--no-reboot", "-display", "none"],
                )
            },
        },
    ]
}

fn scenario_args(timeout: u64, xtask_args: &[&str], qemu_args: &[&str]) -> Vec<String> {
    let mut args = vec![
        "--debugcon".to_string(),
        "file".to_string(),
        "--timeout".to_string(),
        timeout.to_string(),
        "--summarize-log".to_string(),
    ];
    args.extend(xtask_args.iter().map(|arg| arg.to_string()));
    if !qemu_args.is_empty() {
        args.push("--".to_string());
        args.extend(qemu_args.iter().map(|arg| arg.to_string()));
    }
    args
}

pub(crate) fn run_qemu_command<I>(config: &Config, mut args: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    let Some(options) = parse_run_options(&mut args, print_run_help)? else {
        return Ok(());
    };
    run_qemu_with_options(config, options)
}

pub(crate) fn probe_display_command<I>(config: &Config, mut args: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    let Some(mut options) = parse_run_options(&mut args, print_probe_display_help)? else {
        return Ok(());
    };
    append_default_arg_pair(&mut options.qemu_user_args, "-display", "none");
    run_display_probe(config, options)
}

pub(crate) fn debug_qemu_command<I>(config: &Config, mut args: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    let Some(mut options) = parse_run_options(&mut args, print_run_help)? else {
        return Ok(());
    };
    append_unique_string(&mut options.qemu_user_args, String::from("-s"));
    append_unique_string(&mut options.qemu_user_args, String::from("-S"));
    write_gdb_helper(config)?;
    run_qemu_with_options(config, options)
}

pub(crate) fn scenarios_command(
    config: &Config,
    list: bool,
    selected: Vec<String>,
    dry_run: bool,
    timeout_secs: u64,
) -> Result<()> {
    if timeout_secs == 0 {
        bail!("--timeout must be greater than zero");
    }
    let scenarios = qemu_scenarios();
    if list {
        for scenario in scenarios {
            println!("{}: {}", scenario.name, scenario.description);
        }
        return Ok(());
    }

    let selected = if selected.is_empty() {
        scenarios
            .iter()
            .map(|scenario| scenario.name)
            .collect::<Vec<_>>()
    } else {
        selected.iter().map(String::as_str).collect::<Vec<_>>()
    };

    for name in selected {
        let Some(scenario) = scenarios.iter().find(|candidate| candidate.name == name) else {
            bail!("unknown QEMU scenario: {name}");
        };
        let args = (scenario.args)(timeout_secs);
        if dry_run {
            println!("cargo xtask {} {}", scenario.command_name(), args.join(" "));
            continue;
        }
        println!("xtask: running QEMU scenario {}", scenario.name);
        match scenario.command {
            ScenarioCommand::Run => run_qemu_command(config, args.into_iter())?,
            ScenarioCommand::ProbeDisplay => probe_display_command(config, args.into_iter())?,
        }
    }
    Ok(())
}

fn parse_run_options<I>(args: &mut I, help: fn()) -> Result<Option<RunOptions>>
where
    I: Iterator<Item = String>,
{
    let mut options = RunOptions::from_env();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => {
                options.qemu_user_args.extend(args);
                break;
            }
            "-profile" | "--profile" => {
                options.profile = next_required_arg(args, arg.as_str())?;
            }
            "-accel-profile" | "--accel-profile" => {
                options.accel = next_required_arg(args, arg.as_str())?;
            }
            "--usb-input" => options.usb_input = true,
            "--usb-input-device" => {
                let value = next_required_arg(args, "--usb-input-device")?;
                options.usb_input_device = UsbInputDevice::parse(value.as_str())
                    .with_context(|| format!("invalid --usb-input-device value: {value}"))?;
                options.usb_input = true;
            }
            "--usb-mouse" => {
                options.usb_input = true;
                options.usb_input_device = UsbInputDevice::Mouse;
            }
            "--debugcon" => {
                let value = next_required_arg(args, "--debugcon")?;
                options.debugcon = DebugconMode::parse(value.as_str())
                    .with_context(|| format!("invalid --debugcon value: {value}"))?;
            }
            "--qemu-log" => {
                let value = next_required_arg(args, "--qemu-log")?;
                options.qemu_log = QemuLogMode::parse(value.as_str())
                    .with_context(|| format!("invalid --qemu-log value: {value}"))?;
            }
            "--timeout" => {
                let value = next_required_arg(args, "--timeout")?;
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| anyhow!("invalid --timeout seconds: {value}"))?;
                if seconds == 0 {
                    bail!("--timeout must be greater than zero");
                }
                options.timeout = Some(Duration::from_secs(seconds));
            }
            "--expect" => {
                let marker = next_required_arg(args, "--expect")?;
                append_unique_string(&mut options.expect_markers, marker);
            }
            "--commercial-max-ready" => {
                for marker in COMMERCIAL_MAX_READY_MARKERS {
                    append_unique_string(&mut options.expect_markers, (*marker).to_owned());
                }
                options.summarize_log = true;
                if options.timeout.is_none() {
                    options.timeout = Some(Duration::from_secs(35));
                }
            }
            "--fault" => {
                let rule = next_required_arg(args, "--fault")?;
                rustos_fault_injection::parse_rule(&rule)
                    .map_err(|err| anyhow!("invalid --fault rule {rule:?}: {err}"))?;
                append_unique_string(&mut options.fault_rules, rule);
            }
            "--summarize-log" => options.summarize_log = true,
            "--vfio-pci" => {
                let host = next_required_arg(args, "--vfio-pci")?;
                append_unique_string(&mut options.vfio_hosts, host);
            }
            "--phoenix3-passthrough" => options.auto_phoenix3_passthrough = true,
            "--vfio-force" => options.vfio_force = true,
            "--no-network" => options.network = false,
            "-h" | "--help" => {
                help();
                return Ok(None);
            }
            _ => options.qemu_user_args.push(arg),
        }
    }

    Ok(Some(options))
}

fn run_qemu_with_options(config: &Config, options: RunOptions) -> Result<()> {
    let timeout = options.timeout;
    let expect_markers = options.expect_markers.clone();
    let summarize_log = options.summarize_log;
    let prepared = prepare_run(config, options)?;

    println!(
        "\n====================================\nStarting QEMU...\n====================================\n"
    );

    let mut command = base_qemu_command(config, &prepared.qemu_bin);
    append_qemu_args(
        &mut command,
        &prepared.profile_args,
        &prepared.vfio_args,
        &prepared.gpu_args,
        &prepared.usb_args,
        &prepared.network_args,
        &prepared.display_args,
        &prepared.debugcon_args,
        &prepared.qemu_log_args,
        &prepared.fault_args,
        &prepared.qemu_user_args,
    );

    if timeout.is_some() || !expect_markers.is_empty() {
        return run_qemu_supervised(
            &mut command,
            timeout,
            expect_markers.as_slice(),
            summarize_log,
            prepared.debugcon_log.as_deref(),
        );
    }

    let status = command.status()?;

    println!(
        "\n====================================\nQEMU exited with code {}\n====================================\n",
        status
            .code()
            .map(|code: i32| code.to_string())
            .unwrap_or_else(|| String::from("signal"))
    );

    if summarize_log {
        summarize_debugcon_log(prepared.debugcon_log.as_deref())?;
    }

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("QEMU exited with status {status}"))
    }
}

fn run_qemu_supervised(
    command: &mut Command,
    timeout: Option<Duration>,
    expect_markers: &[String],
    summarize_log: bool,
    debugcon_log: Option<&Path>,
) -> Result<()> {
    if !expect_markers.is_empty() && debugcon_log.is_none() {
        bail!("--expect requires --debugcon file");
    }

    let started = Instant::now();
    let deadline = timeout.map(|timeout| started + timeout);
    let mut child = command.spawn()?;
    loop {
        if !expect_markers.is_empty()
            && expected_markers_satisfied(debugcon_log, expect_markers)?.is_empty()
        {
            terminate_qemu_child(&mut child)?;
            println!(
                "\n====================================\nQEMU stopped after expected marker(s): {}\n====================================\n",
                expect_markers.join(" | ")
            );
            if summarize_log {
                summarize_debugcon_log(debugcon_log)?;
            }
            return Ok(());
        }

        if let Some(status) = child.try_wait()? {
            println!(
                "\n====================================\nQEMU exited with code {}\n====================================\n",
                status
                    .code()
                    .map(|code: i32| code.to_string())
                    .unwrap_or_else(|| String::from("signal"))
            );
            if summarize_log {
                summarize_debugcon_log(debugcon_log)?;
            }
            if !status.success() {
                bail!("QEMU exited with status {status}");
            }
            let missing = expected_markers_satisfied(debugcon_log, expect_markers)?;
            if !missing.is_empty() {
                bail!(
                    "QEMU exited before expected marker(s): {}",
                    missing.join(" | ")
                );
            }
            return Ok(());
        }

        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            terminate_qemu_child(&mut child)?;
            println!(
                "\n====================================\nQEMU stopped after timeout {:?}\n====================================\n",
                timeout.unwrap_or_default()
            );
            if summarize_log {
                summarize_debugcon_log(debugcon_log)?;
            }
            let missing = expected_markers_satisfied(debugcon_log, expect_markers)?;
            if !missing.is_empty() {
                bail!(
                    "timed out waiting for expected marker(s): {}",
                    missing.join(" | ")
                );
            }
            return Ok(());
        }

        thread::sleep(QEMU_WAIT_POLL);
    }
}

fn terminate_qemu_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait()?;
    Ok(())
}

fn expected_markers_satisfied(
    debugcon_log: Option<&Path>,
    expect_markers: &[String],
) -> Result<Vec<String>> {
    if expect_markers.is_empty() {
        return Ok(Vec::new());
    }
    let Some(debugcon_log) = debugcon_log else {
        return Ok(expect_markers.to_vec());
    };
    let contents = fs::read_to_string(debugcon_log).unwrap_or_default();
    Ok(expect_markers
        .iter()
        .filter(|marker| !contents.contains(marker.as_str()))
        .cloned()
        .collect())
}

fn summarize_debugcon_log(debugcon_log: Option<&Path>) -> Result<()> {
    let Some(debugcon_log) = debugcon_log else {
        println!("debugcon summary skipped: debugcon file output is disabled.");
        return Ok(());
    };
    let contents = fs::read_to_string(debugcon_log).unwrap_or_default();
    let mut selected = Vec::<&str>::new();
    for line in contents.lines() {
        if RUN_SUMMARY_MARKERS
            .iter()
            .any(|marker| line_contains_case_insensitive(line, marker))
        {
            selected.push(line);
        }
    }

    const MAX_SUMMARY_LINES: usize = 80;
    let start = selected.len().saturating_sub(MAX_SUMMARY_LINES);
    println!(
        "\n====================================\nDebugcon summary: {}\n====================================",
        debugcon_log.display()
    );
    if selected.is_empty() {
        println!("No high-signal debug markers matched.");
        return Ok(());
    }
    if start != 0 {
        println!("... omitted {} earlier matching line(s) ...", start);
    }
    for line in &selected[start..] {
        println!("{line}");
    }
    Ok(())
}

fn line_contains_case_insensitive(line: &str, marker: &str) -> bool {
    if line.contains(marker) {
        return true;
    }
    let lower_line = line.to_ascii_lowercase();
    lower_line.contains(marker.to_ascii_lowercase().as_str())
}

fn write_gdb_helper(config: &Config) -> Result<()> {
    fs::create_dir_all(&config.logs_dir)?;
    let script_path = config.logs_dir.join("rustos-debug.gdb");
    let script = format!(
        "\
set pagination off
set confirm off
set breakpoint pending on
set architecture i386:x86-64
file {}
target remote :1234

echo Connected to RustOS debug target.\n
echo Nucleus symbols: {}\n
echo Module symbols can be loaded from /dev/debug0 snapshots once the guest is up.\n

break handle_kernel_panic
break fatal_init_bootstrap_load
break fatal_init_bootstrap_spawn
break load_module_image_explicit
break register_virtio_driver
break register_linux_netdev
break set_linux_netdev_carrier
break note_virtio_net_driver_registered
break ensure_initialized
break connect_inet_socket
break connect_tcp
break bootstrap_init_process

echo RustOS breakpoint candidates installed. Missing symbols remain pending.\n
",
        config.artifact_nucleus_elf_path().display(),
        config.artifact_nucleus_elf_path().display(),
    );
    fs::write(&script_path, script)?;
    println!("gdb helper written to {}", script_path.display());
    Ok(())
}

fn ensure_qemu_prerequisites(config: &Config) -> Result<()> {
    if !config.image_dir.is_dir() {
        bail!(
            "missing build image directory: {} (run `cargo xtask build` first)",
            config.image_dir.display()
        );
    }

    if !config.boot_disk_image.is_file() {
        bail!(
            "missing boot disk image: {} (run `cargo xtask build` or `cargo xtask stage` first)",
            config.boot_disk_image.display()
        );
    }

    let boot_efi = config.boot_efi_path();
    if !boot_efi.is_file() {
        bail!(
            "missing staged boot manager image: {} (run `cargo xtask build` first)",
            boot_efi.display()
        );
    }

    if !config.ovmf_path.is_file() {
        bail!(
            "missing OVMF firmware image: {}",
            config.ovmf_path.display()
        );
    }

    Ok(())
}

fn prepare_run(config: &Config, options: RunOptions) -> Result<PreparedRun> {
    ensure_qemu_prerequisites(config)?;
    let qemu_bin = resolve_command_path(&config.qemu_bin).with_context(|| {
        format!(
            "missing QEMU binary: {}",
            Path::new(&config.qemu_bin).display()
        )
    })?;

    let mut session = RunSession::create()?;
    let mut profile_args = build_run_profile_args(&options, &config.boot_disk_image)?;
    let mut vfio_hosts = options.vfio_hosts.clone();
    if options.auto_phoenix3_passthrough {
        for bdf in detect_phoenix3_devices()? {
            append_unique_string(&mut vfio_hosts, bdf);
        }
    }

    let vfio_args = configure_vfio_args(&mut profile_args, &vfio_hosts, options.vfio_force)?;
    let use_kvm = options.accel == "kvm";
    let gpu_args = configure_default_gpu_args(config, &options.qemu_user_args, &vfio_hosts);
    let use_low_latency_input = options.usb_input || options.accel == "kvm";
    let usb_args = configure_usb_args(
        use_low_latency_input,
        options.usb_input_device,
        &options.qemu_user_args,
    );
    let network_args = configure_network_args(options.network, &options.qemu_user_args);
    let display_args = configure_display_args(&options.qemu_user_args, use_kvm);
    let (debugcon_args, debugcon_log) = configure_debugcon(config, &mut session, options.debugcon)?;
    let qemu_log_args = configure_qemu_log(config, options.qemu_log)?;
    let fault_args = configure_fault_args(config, &options.fault_rules)?;

    Ok(PreparedRun {
        qemu_bin,
        session,
        profile_args,
        vfio_args,
        gpu_args,
        usb_args,
        usb_input_device: options.usb_input_device,
        network_args,
        display_args,
        debugcon_args,
        qemu_log_args,
        fault_args,
        qemu_user_args: options.qemu_user_args,
        debugcon_log,
    })
}

pub(crate) fn print_run_help() {
    println!(
        "\
usage: cargo xtask run [options] [-- qemu args...]

options:
  -profile, --profile <name>         qemu profile (default, g14, nvme)
  -accel-profile, --accel-profile <name>
                                     accelerator profile; use \"kvm\" for host CPU
  --usb-input                        attach qemu-xhci + usb-kbd + usb-tablet
                                     for USB HID testing
  --usb-input-device <tablet|mouse>  choose USB HID pointer for --usb-input
  --usb-mouse                        shorthand for --usb-input-device mouse
  --no-network                       do not attach default usernet + virtio-net-pci
  --debugcon <file|stdio|null>       route debugcon to file, terminal, or disable it
  --qemu-log <int|null>              write QEMU trace log to logs/qemu_interrupt.log or disable it
  --timeout <seconds>                stop QEMU after this many seconds; no default timeout
  --expect <marker>                  stop successfully once all repeated expected markers appear
  --commercial-max-ready             expect rootd/initd/services/Wayland/wayclick/storaged readiness
  --fault <location=action>          pass a validated fault-injection rule through QEMU fw_cfg
  --summarize-log                    print high-signal debugcon lines after QEMU stops
  --vfio-pci <0000:bb:dd.f>          attach a vfio-pci host device to qemu (repeatable)
  --phoenix3-passthrough             auto-attach host Phoenix3 VGA function and same-slot audio
  --vfio-force                       allow devices that currently drive an active host display
  -h, --help                         show this help
"
    );
}

fn print_probe_display_help() {
    println!(
        "\
usage: cargo xtask probe-display [options] [-- qemu args...]

options:
  -profile, --profile <name>         qemu profile (default, g14, nvme)
  -accel-profile, --accel-profile <name>
                                     accelerator profile; use \"kvm\" for host CPU
  --usb-input                        attach qemu-xhci + usb-kbd + usb-tablet
                                     for USB HID testing
  --usb-input-device <tablet|mouse>  choose USB HID pointer for --usb-input
  --usb-mouse                        shorthand for --usb-input-device mouse
  --no-network                       do not attach default usernet + virtio-net-pci
  --debugcon <file|stdio|null>       route debugcon to file, terminal, or disable it
  --qemu-log <int|null>              write QEMU trace log to logs/qemu_interrupt.log or disable it
  --timeout <seconds>                stop QEMU after this many seconds; no default timeout
  --expect <marker>                  stop successfully once all repeated expected markers appear
  --fault <location=action>          pass a validated fault-injection rule through QEMU fw_cfg
  --summarize-log                    print high-signal debugcon lines after QEMU stops
  --vfio-pci <0000:bb:dd.f>          attach a vfio-pci host device to qemu (repeatable)
  --phoenix3-passthrough             auto-attach host Phoenix3 VGA function and same-slot audio
  --vfio-force                       allow devices that currently drive an active host display
  -h, --help                         show this help

probe-display always forces headless mode and validates screendump geometry after
injecting mouse movement/click stress through QMP. Set RUSTOS_PROBE_KEY_TEXT to
inject focused keyboard text before the mouse stress phase.
"
    );
}

fn next_required_arg<I>(args: &mut I, option: &str) -> Result<String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .filter(|value| !value.is_empty())
        .with_context(|| anyhow!("missing value for {option}"))
}

fn append_unique_string(items: &mut Vec<String>, candidate: String) {
    if candidate.is_empty() || items.iter().any(|existing| existing == &candidate) {
        return;
    }
    items.push(candidate);
}

fn append_default_arg_pair(args: &mut Vec<String>, option: &str, value: &str) {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == option {
            return;
        }
        if arg.starts_with(option) {
            return;
        }
        if arg == value {
            return;
        }
        if arg == "--" {
            break;
        }
        let _ = iter.next();
    }

    args.push(option.to_string());
    args.push(value.to_string());
}

fn build_run_profile_args(options: &RunOptions, boot_image: &Path) -> Result<Vec<OsString>> {
    let spec = qemu_profile_spec(options.profile.as_str())?;
    let mut args = Vec::new();
    append_boot_disk_args(&mut args, boot_image, spec.boot_disk);
    args.push(OsString::from("-m"));
    args.push(OsString::from(spec.memory));
    args.push(OsString::from("-machine"));
    let use_low_latency_input = options.usb_input || options.accel == "kvm";
    if options.accel == "kvm" {
        args.push(machine_option(
            "q35,accel=kvm,vmport=off",
            use_low_latency_input,
        ));
    } else {
        args.push(machine_option("q35,vmport=off", use_low_latency_input));
    }
    if let Some(cpu) = if options.accel == "kvm" {
        spec.cpu_when_kvm
    } else {
        spec.cpu_other
    } {
        args.push(OsString::from("-cpu"));
        args.push(OsString::from(cpu));
    }
    if let Some(smp) = spec.smp {
        args.push(OsString::from("-smp"));
        args.push(OsString::from(smp));
    }
    if let Some(rtc) = spec.rtc {
        args.push(OsString::from("-rtc"));
        args.push(OsString::from(rtc));
    }
    Ok(args)
}

fn qemu_profile_spec(profile: &str) -> Result<QemuProfileSpec> {
    match profile {
        "default" => Ok(QemuProfileSpec {
            boot_disk: QemuBootDisk::Ahci,
            memory: "4G",
            cpu_when_kvm: Some("host"),
            cpu_other: None,
            smp: Some("4,sockets=1,cores=4,threads=1"),
            rtc: Some("base=localtime,clock=host"),
        }),
        "g14" => Ok(QemuProfileSpec {
            boot_disk: QemuBootDisk::Ahci,
            memory: "8G",
            cpu_when_kvm: Some("host"),
            cpu_other: Some("EPYC-v4"),
            smp: Some("8,sockets=1,cores=8,threads=1"),
            rtc: Some("base=localtime,clock=host"),
        }),
        "nvme" => Ok(QemuProfileSpec {
            boot_disk: QemuBootDisk::Nvme,
            memory: "4G",
            cpu_when_kvm: Some("host"),
            cpu_other: None,
            smp: Some("4,sockets=1,cores=4,threads=1"),
            rtc: Some("base=localtime,clock=host"),
        }),
        _ => Err(anyhow!("unknown qemu profile: {profile}")),
    }
}

fn append_boot_disk_args(args: &mut Vec<OsString>, boot_image: &Path, boot_disk: QemuBootDisk) {
    if matches!(boot_disk, QemuBootDisk::Ahci) {
        args.push(OsString::from("-device"));
        args.push(OsString::from("ich9-ahci,id=ahci"));
    }
    args.push(OsString::from("-drive"));
    args.push(OsString::from(format!(
        "id=bootdisk,if=none,file={},format=raw,snapshot=on",
        boot_image.display()
    )));
    args.push(OsString::from("-device"));
    args.push(match boot_disk {
        QemuBootDisk::Ahci => OsString::from("ide-hd,drive=bootdisk,bus=ahci.0"),
        QemuBootDisk::Nvme => OsString::from("nvme,serial=RUSTOSNVME01,drive=bootdisk"),
    });
}

fn machine_option(base: &str, disable_i8042: bool) -> OsString {
    if disable_i8042 {
        OsString::from(format!("{base},i8042=off"))
    } else {
        OsString::from(base)
    }
}

fn profile_has_machine_arg(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "-machine")
}

fn configure_vfio_args(
    profile_args: &mut Vec<OsString>,
    vfio_hosts: &[String],
    vfio_force: bool,
) -> Result<Vec<OsString>> {
    if vfio_hosts.is_empty() {
        return Ok(Vec::new());
    }

    ensure_vfio_available()?;
    if !profile_has_machine_arg(profile_args) {
        profile_args.insert(0, OsString::from("q35"));
        profile_args.insert(0, OsString::from("-machine"));
    }

    let mut args = Vec::new();
    let mut first_gpu = true;
    for bdf in vfio_hosts {
        validate_vfio_device(bdf, vfio_force)?;
        let class_code = read_trimmed(Path::new("/sys/bus/pci/devices").join(bdf).join("class"))?;
        if class_code.starts_with("0x03") {
            if first_gpu {
                args.push(OsString::from("-display"));
                args.push(OsString::from("none"));
                args.push(OsString::from("-vga"));
                args.push(OsString::from("none"));
                args.push(OsString::from("-device"));
                args.push(OsString::from(format!(
                    "vfio-pci,host={bdf},multifunction=on,x-vga=on"
                )));
                first_gpu = false;
            } else {
                args.push(OsString::from("-device"));
                args.push(OsString::from(format!("vfio-pci,host={bdf}")));
            }
        } else {
            args.push(OsString::from("-device"));
            args.push(OsString::from(format!("vfio-pci,host={bdf}")));
        }
    }
    Ok(args)
}

fn ensure_vfio_available() -> Result<()> {
    if Path::new("/dev/vfio/vfio").exists() || Path::new("/dev/vfio").exists() {
        Ok(())
    } else {
        Err(anyhow!("VFIO is not available: /dev/vfio is missing."))
    }
}

fn validate_vfio_device(bdf: &str, vfio_force: bool) -> Result<()> {
    let devpath = Path::new("/sys/bus/pci/devices").join(bdf);
    if !devpath.is_dir() {
        bail!("VFIO host device not found: {bdf}");
    }

    let driver_link = devpath.join("driver");
    if !driver_link.exists() {
        bail!("VFIO host device has no bound driver: {bdf}");
    }

    let driver_path = fs::canonicalize(&driver_link)?;
    let driver_name = driver_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>");
    if driver_name != "vfio-pci" {
        bail!("VFIO host device is not bound to vfio-pci: {bdf} (current: {driver_name})");
    }

    let iommu_group_link = devpath.join("iommu_group");
    if iommu_group_link.exists() {
        let iommu_group = fs::canonicalize(&iommu_group_link)?;
        let group_name = iommu_group
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("failed to resolve IOMMU group for {bdf}"))?;
        if !Path::new("/dev/vfio").join(group_name).exists() {
            bail!("VFIO IOMMU group device is missing: /dev/vfio/{group_name} for {bdf}");
        }
    }

    if !vfio_force && device_drives_active_host_display(bdf)? {
        bail!(
            "Refusing to passthrough active host display device {bdf}. Use --vfio-force only after moving the host off this GPU."
        );
    }

    Ok(())
}

fn detect_phoenix3_devices() -> Result<Vec<String>> {
    let pci_root = Path::new("/sys/bus/pci/devices");
    for entry in fs::read_dir(pci_root)? {
        let entry = entry?;
        let path = entry.path();
        if read_trimmed(path.join("vendor")).ok().as_deref() != Some("0x1002") {
            continue;
        }
        if read_trimmed(path.join("device")).ok().as_deref() != Some("0x1900") {
            continue;
        }

        let bdf = entry.file_name().to_string_lossy().into_owned();
        let slot_prefix = bdf
            .rsplit_once('.')
            .map(|(prefix, _)| prefix)
            .unwrap_or(&bdf)
            .to_string();
        let mut devices = vec![bdf.clone()];

        for sibling in fs::read_dir(pci_root)? {
            let sibling = sibling?;
            let sibling_name = sibling.file_name().to_string_lossy().into_owned();
            if sibling_name == bdf || !sibling_name.starts_with(&(slot_prefix.clone() + ".")) {
                continue;
            }
            let class = read_trimmed(sibling.path().join("class")).unwrap_or_default();
            if class == "0x040300" {
                devices.push(sibling_name);
            }
        }

        return Ok(devices);
    }

    Err(anyhow!("Phoenix3 (1002:1900) host GPU not found."))
}

fn device_drives_active_host_display(bdf: &str) -> Result<bool> {
    let drm_root = Path::new("/sys/class/drm");
    if !drm_root.is_dir() {
        return Ok(false);
    }

    for entry in fs::read_dir(drm_root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || !name.contains('-') {
            continue;
        }
        let device_link = path.join("device");
        if !device_link.exists() {
            continue;
        }
        let device_path = fs::canonicalize(&device_link)?;
        let Some(device_name) = device_path
            .file_name()
            .and_then(|file_name| file_name.to_str())
        else {
            continue;
        };
        if device_name != bdf {
            continue;
        }

        let enabled = read_trimmed(path.join("enabled")).unwrap_or_default();
        if enabled == "enabled" {
            return Ok(true);
        }
        let status = read_trimmed(path.join("status")).unwrap_or_default();
        if status == "connected" {
            return Ok(true);
        }
    }

    Ok(false)
}

fn configure_debugcon(
    config: &Config,
    session: &mut RunSession,
    mode: DebugconMode,
) -> Result<(Vec<OsString>, Option<PathBuf>)> {
    match mode {
        DebugconMode::Null => {
            return Ok((
                vec![OsString::from("-debugcon"), OsString::from("null")],
                None,
            ));
        }
        DebugconMode::Stdio => {
            return Ok((
                vec![OsString::from("-debugcon"), OsString::from("stdio")],
                None,
            ));
        }
        DebugconMode::File => {}
    }

    fs::create_dir_all(&config.logs_dir)?;
    let debugcon_log = config.logs_dir.join("debugcon.log");
    fs::File::create(&debugcon_log)?;
    if let Some(tail_bin) = resolve_command_path(OsStr::new("tail")) {
        let child = Command::new(tail_bin)
            .arg("-f")
            .arg(&debugcon_log)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        session.debugcon_tail = Some(child);
    }

    println!(
        "debugcon redirected to {} because stdio is already in use.",
        debugcon_log.display()
    );

    Ok((
        vec![
            OsString::from("-debugcon"),
            OsString::from(format!("file:{}", debugcon_log.display())),
        ],
        Some(debugcon_log),
    ))
}

fn configure_qemu_log(config: &Config, mode: QemuLogMode) -> Result<Vec<OsString>> {
    match mode {
        QemuLogMode::None => {
            let interrupt_log = config.logs_dir.join("qemu_interrupt.log");
            match fs::remove_file(&interrupt_log) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            Ok(Vec::new())
        }
        QemuLogMode::Interrupt => {
            fs::create_dir_all(&config.logs_dir)?;
            let interrupt_log = config.logs_dir.join("qemu_interrupt.log");
            fs::File::create(&interrupt_log)?;
            Ok(vec![
                OsString::from("-d"),
                OsString::from("int"),
                OsString::from("-D"),
                interrupt_log.into_os_string(),
            ])
        }
    }
}

fn configure_fault_args(config: &Config, cli_rules: &[String]) -> Result<Vec<OsString>> {
    let mut rules = Vec::new();
    if config.project.fault_injection.enabled {
        rules.extend(config.project.fault_injection.rules.iter().cloned());
    }
    rules.extend(cli_rules.iter().cloned());
    if rules.is_empty() {
        return Ok(Vec::new());
    }

    let spec = rules.join(";");
    rustos_fault_injection::parse_rules(&spec)
        .map_err(|err| anyhow!("invalid fault injection rule set: {err}"))?;
    println!("fault injection fw_cfg {}={}", FAULT_FW_CFG_NAME, spec);
    Ok(vec![
        OsString::from("-fw_cfg"),
        OsString::from(format!("name={FAULT_FW_CFG_NAME},string={spec}")),
    ])
}

fn configure_display_args(qemu_user_args: &[String], _use_kvm: bool) -> Vec<OsString> {
    if env::var_os("DISPLAY").is_none() && env::var_os("WAYLAND_DISPLAY").is_none() {
        return Vec::new();
    }

    let mut expect_display_target = false;
    for arg in qemu_user_args {
        if expect_display_target {
            return Vec::new();
        }

        match arg.as_str() {
            "-display" => expect_display_target = true,
            "-nographic" | "-curses" | "-vnc" | "-spice" => return Vec::new(),
            value if value.starts_with("-display") => return Vec::new(),
            value if value.starts_with("-vnc") => return Vec::new(),
            value if value.starts_with("-spice") => return Vec::new(),
            _ => {}
        }
    }

    if expect_display_target {
        return Vec::new();
    }

    vec![
        OsString::from("-display"),
        OsString::from("gtk,gl=off,grab-on-hover=on,show-cursor=off,zoom-to-fit=on"),
    ]
}

fn configure_default_gpu_args(
    config: &Config,
    qemu_user_args: &[String],
    vfio_hosts: &[String],
) -> Vec<OsString> {
    if !vfio_hosts.is_empty() || qemu_user_overrides_gpu(qemu_user_args) {
        return Vec::new();
    }

    let display = &config.project.qemu.display;
    vec![
        OsString::from("-vga"),
        OsString::from("none"),
        OsString::from("-device"),
        OsString::from(format!(
            "virtio-vga,id={},xres={},yres={},max_outputs=1,edid=off",
            DEFAULT_QEMU_DISPLAY_DEVICE_ID, display.width, display.height
        )),
    ]
}

fn qemu_user_overrides_gpu(qemu_user_args: &[String]) -> bool {
    let mut expect_vga_target = false;
    let mut expect_device_target = false;
    for arg in qemu_user_args {
        if expect_vga_target {
            return true;
        }
        if expect_device_target {
            expect_device_target = false;
            if qemu_device_arg_is_graphics(arg) {
                return true;
            }
            continue;
        }

        match arg.as_str() {
            "-vga" => expect_vga_target = true,
            "-device" => expect_device_target = true,
            value if value.starts_with("-vga") => return true,
            value if value.starts_with("-device") && qemu_device_arg_is_graphics(value) => {
                return true;
            }
            _ => {}
        }
    }

    expect_vga_target
}

fn qemu_device_arg_is_graphics(arg: &str) -> bool {
    arg.contains("virtio-gpu")
        || arg.contains("virtio-vga")
        || arg.contains("vfio-pci")
        || arg.contains("VGA")
        || arg.contains("bochs-display")
        || arg.contains("ramfb")
}

fn configure_usb_args(
    enable_usb_input: bool,
    input_device: UsbInputDevice,
    qemu_user_args: &[String],
) -> Vec<OsString> {
    if !enable_usb_input {
        return Vec::new();
    }

    if qemu_user_args.iter().any(|arg| {
        arg.contains("qemu-xhci")
            || arg.contains("usb-kbd")
            || arg.contains("usb-tablet")
            || arg.contains("usb-mouse")
            || arg == "-usb"
    }) {
        return Vec::new();
    }

    vec![
        OsString::from("-device"),
        OsString::from("qemu-xhci,id=xhci"),
        OsString::from("-device"),
        OsString::from("usb-kbd,bus=xhci.0,id=usbkbd"),
        OsString::from("-device"),
        OsString::from(input_device.qemu_device_arg()),
    ]
}

fn configure_network_args(enable_network: bool, qemu_user_args: &[String]) -> Vec<OsString> {
    if !enable_network || qemu_user_overrides_network(qemu_user_args) {
        return Vec::new();
    }

    vec![
        OsString::from("-netdev"),
        OsString::from(
            "user,id=net0,net=10.0.2.0/24,host=10.0.2.2,hostfwd=tcp::10080-:8080,hostfwd=udp::10080-:8080",
        ),
        OsString::from("-device"),
        OsString::from("virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56"),
    ]
}

fn qemu_user_overrides_network(qemu_user_args: &[String]) -> bool {
    let mut expect_device_target = false;
    for arg in qemu_user_args {
        if expect_device_target {
            expect_device_target = false;
            if qemu_device_arg_is_network(arg) {
                return true;
            }
            continue;
        }

        match arg.as_str() {
            "-netdev" | "-nic" | "-net" => return true,
            "-device" => expect_device_target = true,
            value
                if value.starts_with("-netdev")
                    || value.starts_with("-nic")
                    || value.starts_with("-net") =>
            {
                return true;
            }
            value if value.starts_with("-device") && qemu_device_arg_is_network(value) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn qemu_device_arg_is_network(arg: &str) -> bool {
    arg.contains("virtio-net")
        || arg.contains("e1000")
        || arg.contains("rtl8139")
        || arg.contains("vmxnet")
}

fn base_qemu_command(config: &Config, qemu_bin: &Path) -> Command {
    let mut command = Command::new(qemu_bin);
    command.current_dir(&config.root_dir);
    // QEMU's vvfat backend creates temporary files via the host tmpdir. Some
    // sandboxed runs expose /var/tmp read-only, so force the QEMU child onto
    // /tmp where the workspace tooling is allowed to create sockets/files.
    command.env("TMPDIR", "/tmp");
    command.arg("-bios").arg(&config.ovmf_path);
    command
}

#[allow(clippy::too_many_arguments)]
fn append_qemu_args(
    command: &mut Command,
    profile_args: &[OsString],
    vfio_args: &[OsString],
    gpu_args: &[OsString],
    usb_args: &[OsString],
    network_args: &[OsString],
    display_args: &[OsString],
    debugcon_args: &[OsString],
    qemu_log_args: &[OsString],
    fault_args: &[OsString],
    qemu_user_args: &[String],
) {
    command.args(profile_args);
    command.args(vfio_args);
    command.args(gpu_args);
    command.args(usb_args);
    command.args(network_args);
    command.args(display_args);
    command.args(debugcon_args);
    command.args(qemu_log_args);
    command.args(fault_args);
    command.args(qemu_user_args);
}

fn run_display_probe(config: &Config, options: RunOptions) -> Result<()> {
    let probe_expect_markers = options.expect_markers.clone();
    let probe_key_text = env_string("RUSTOS_PROBE_KEY_TEXT");
    let prepared = prepare_run(config, options)?;
    fs::create_dir_all(&config.logs_dir)?;

    let qmp_socket = prepared.session.temp_dir.join("qmp.sock");
    let mut qmp_args = Vec::new();
    qmp_args.push(OsString::from("-qmp"));
    qmp_args.push(OsString::from(format!(
        "unix:{},server=on,wait=off",
        qmp_socket.display()
    )));

    println!(
        "\n====================================\nStarting headless display probe...\n====================================\n"
    );

    let mut command = base_qemu_command(config, &prepared.qemu_bin);
    append_qemu_args(
        &mut command,
        &prepared.profile_args,
        &prepared.vfio_args,
        &prepared.gpu_args,
        &prepared.usb_args,
        &prepared.network_args,
        &prepared.display_args,
        &prepared.debugcon_args,
        &prepared.qemu_log_args,
        &prepared.fault_args,
        &prepared.qemu_user_args,
    );
    command.args(&qmp_args);
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    let mut child = command.spawn()?;
    let debugcon_log = config.logs_dir.join("debugcon.log");
    let probe_result = (|| -> Result<()> {
        let mut qmp = connect_qmp(&qmp_socket, PROBE_QMP_TIMEOUT)?;
        wait_for_boot_marker_with_qmp(
            &debugcon_log,
            &[
                "userspace display active",
                "userspace_display=true",
                "uiserver: wayland compositor ready",
                "uiserver: init surface meta",
            ],
            probe_duration_env("RUSTOS_PROBE_BOOT_TIMEOUT_MS", PROBE_BOOT_TIMEOUT_DEFAULT),
            &mut qmp,
        )?;
        wait_for_boot_marker_with_qmp(
            &debugcon_log,
            &[
                "uiserver: desktop background ready",
                "uiserver: first present done",
                "uiserver: slow full present",
            ],
            PROBE_QMP_TIMEOUT,
            &mut qmp,
        )?;
        for marker in &probe_expect_markers {
            wait_for_boot_marker_with_qmp(
                &debugcon_log,
                &[marker.as_str()],
                PROBE_QMP_TIMEOUT,
                &mut qmp,
            )?;
        }
        if let Some(text) = probe_key_text.as_deref() {
            qmp_hmp_type_text(&mut qmp, text)?;
        }
        thread::sleep(Duration::from_millis(500));

        let baseline_dump = prepared.session.temp_dir.join("probe-baseline.ppm");
        qmp_screendump(&mut qmp, &baseline_dump)?;
        let baseline = read_ppm_summary(&baseline_dump)?;
        println!(
            "probe baseline color avg_rgb=({},{},{}) dominant={}",
            baseline.avg_r,
            baseline.avg_g,
            baseline.avg_b,
            baseline.dominant_channel()
        );

        if baseline.width == 0
            || baseline.height == 0
            || baseline.width > 8192
            || baseline.height > 8192
        {
            bail!(
                "probe baseline geometry is invalid: {}x{}",
                baseline.width,
                baseline.height
            );
        }
        validate_ppm_visible("baseline", baseline)?;

        if let Ok(info) = qmp_hmp_capture(&mut qmp, "info mice", Instant::now() + PROBE_QMP_TIMEOUT)
        {
            println!("probe info mice:\n{info}");
        }

        if !prepared.usb_args.is_empty() {
            let service_ready_timeout =
                probe_duration_env("RUSTOS_PROBE_BOOT_TIMEOUT_MS", PROBE_BOOT_TIMEOUT_DEFAULT);
            wait_for_boot_marker_with_qmp(
                &debugcon_log,
                &["uiserver: input pointer surface published"],
                service_ready_timeout,
                &mut qmp,
            )?;
            wait_for_boot_marker_with_qmp(
                &debugcon_log,
                &["storaged: storage policy endpoint registered"],
                service_ready_timeout,
                &mut qmp,
            )?;
        }

        run_probe_mouse_stress(
            &mut qmp,
            &debugcon_log,
            if prepared.usb_args.is_empty() {
                None
            } else {
                Some(prepared.usb_input_device)
            },
            (!prepared.usb_args.is_empty()).then_some(DEFAULT_QEMU_DISPLAY_DEVICE_ID),
            Some((baseline.width, baseline.height)),
        )?;
        if !prepared.usb_args.is_empty() {
            wait_for_boot_marker_with_qmp(
                &debugcon_log,
                &["uiserver: pointer moved"],
                PROBE_QMP_TIMEOUT,
                &mut qmp,
            )?;
        }

        let stressed_dump = prepared.session.temp_dir.join("probe-stressed.ppm");
        qmp_screendump(&mut qmp, &stressed_dump)?;
        let stressed = read_ppm_summary(&stressed_dump)?;
        println!(
            "probe stressed color avg_rgb=({},{},{}) dominant={}",
            stressed.avg_r,
            stressed.avg_g,
            stressed.avg_b,
            stressed.dominant_channel()
        );
        if stressed.dimensions() != baseline.dimensions() {
            bail!(
                "display geometry changed during probe: baseline={}x{}, stressed={}x{}",
                baseline.width,
                baseline.height,
                stressed.width,
                stressed.height
            );
        }
        validate_ppm_visible("stressed", stressed)?;

        let debugcon = fs::read_to_string(&debugcon_log).unwrap_or_default();
        for marker in PROBE_BAD_MARKERS {
            if debugcon.contains(marker) {
                bail!("probe detected bad marker in debugcon log: {marker}");
            }
        }

        qmp_quit(&mut qmp)?;
        Ok(())
    })();

    let status = wait_for_child_or_kill(&mut child)?;
    probe_result?;
    if !status.success() {
        bail!("QEMU exited with status {status}");
    }

    println!(
        "\n====================================\nDisplay probe passed\n====================================\n"
    );
    Ok(())
}

fn connect_qmp(path: &Path, timeout: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                let mut qmp = stream;
                let _ = read_qmp_message(&mut qmp, deadline)?;
                qmp_execute(&mut qmp, r#"{"execute":"qmp_capabilities"}"#, deadline)?;
                return Ok(qmp);
            }
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => bail!("failed to connect to QMP socket: {err}"),
        }
    }
}

fn wait_for_boot_marker(log_path: &Path, markers: &[&str], timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let contents = fs::read_to_string(log_path).unwrap_or_default();
        if markers.iter().any(|marker| contents.contains(marker)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for boot markers: {}",
                markers.join(" | ")
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_boot_marker_with_qmp(
    log_path: &Path,
    markers: &[&str],
    timeout: Duration,
    qmp: &mut UnixStream,
) -> Result<()> {
    match wait_for_boot_marker(log_path, markers, timeout) {
        Ok(()) => Ok(()),
        Err(err) => bail!("{err}; {}", probe_boot_timeout_context(log_path, qmp)),
    }
}

fn probe_boot_timeout_context(log_path: &Path, qmp: &mut UnixStream) -> String {
    let debugcon_bytes = fs::metadata(log_path)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let qmp_status = qmp_hmp_capture(qmp, "info status", Instant::now() + PROBE_QMP_TIMEOUT)
        .map(|status| compact_probe_text(&status))
        .unwrap_or_else(|err| format!("qmp info status failed: {err}"));
    let qmp_mice = qmp_hmp_capture(qmp, "info mice", Instant::now() + PROBE_QMP_TIMEOUT)
        .map(|mice| compact_probe_text(&mice))
        .unwrap_or_else(|err| format!("qmp info mice failed: {err}"));
    let dump_path = log_path.with_file_name("probe-boot-timeout.ppm");
    let screen = match qmp_screendump(qmp, &dump_path).and_then(|()| read_ppm_summary(&dump_path)) {
        Ok(summary) => format!(
            "{}x{} avg_rgb=({},{},{}) dominant={}",
            summary.width,
            summary.height,
            summary.avg_r,
            summary.avg_g,
            summary.avg_b,
            summary.dominant_channel()
        ),
        Err(err) => format!("screendump failed: {err}"),
    };
    format!(
        "debugcon_bytes={} debugcon_path={} qmp_status={} qmp_mice={} screen={}",
        debugcon_bytes,
        log_path.display(),
        qmp_status,
        qmp_mice,
        screen
    )
}

fn compact_probe_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn run_probe_mouse_stress(
    qmp: &mut UnixStream,
    debugcon_log: &Path,
    input_device: Option<UsbInputDevice>,
    input_route_device: Option<&str>,
    display_size: Option<(u32, u32)>,
) -> Result<()> {
    let stress_duration =
        probe_duration_env("RUSTOS_PROBE_STRESS_MS", PROBE_STRESS_DURATION_DEFAULT);
    let heartbeat_stall = probe_duration_env(
        "RUSTOS_PROBE_HEARTBEAT_STALL_MS",
        PROBE_HEARTBEAT_STALL_DEFAULT,
    );
    let input_stall = probe_duration_env("RUSTOS_PROBE_INPUT_STALL_MS", PROBE_INPUT_STALL_DEFAULT);
    let input_start = probe_duration_env("RUSTOS_PROBE_INPUT_START_MS", PROBE_INPUT_START_DEFAULT);
    let step_delay = probe_duration_env("RUSTOS_PROBE_STEP_MS", PROBE_STEP_DELAY);
    let mut started_at = Instant::now();
    let mut end = started_at + stress_duration;
    let mut step = 0usize;
    let mut log_tracker = ProbeLogTracker::new(debugcon_log);
    log_tracker.refresh();
    let mut last_heartbeat_second = log_tracker.latest_kernel_second;
    let mut last_liveness_epoch =
        (log_tracker.liveness_epoch > 0).then_some(log_tracker.liveness_epoch);
    let mut heartbeat_seen = last_liveness_epoch.is_some();
    let mut last_heartbeat_at = Instant::now();
    let mut input_progress = log_tracker.input_progress;
    let mut last_hid_progress_at = Instant::now();
    let mut last_read_progress_at = Instant::now();
    let mut last_ui_progress_at = Instant::now();
    let mut first_ui_progress_at = None::<Instant>;
    let mut x = 0x4000_i32;
    let mut y = 0x3000_i32;
    let mut dx = 1400_i32;
    let mut dy = 900_i32;
    let mut last_sent_x = x as u16;
    let mut last_sent_y = y as u16;
    let mut last_sent_dx = 0_i16;
    let mut last_sent_dy = 0_i16;
    let mut last_sent_at = Instant::now();
    let mut max_qmp_send = Duration::ZERO;
    let mut slow_qmp_sends = 0usize;
    let mut changed_sends = 0usize;
    let mut last_probe_notice = Instant::now();
    const ABS_MIN: i32 = 0x0800;
    const ABS_MAX: i32 = 0x77ff;

    if input_device.is_some() && !input_start.is_zero() {
        thread::sleep(input_start);
        log_tracker.refresh();
        input_progress = log_tracker.input_progress;
        last_heartbeat_second = log_tracker.latest_kernel_second;
        last_liveness_epoch =
            (log_tracker.liveness_epoch > 0).then_some(log_tracker.liveness_epoch);
        heartbeat_seen = last_liveness_epoch.is_some();
        let now = Instant::now();
        last_heartbeat_at = now;
        last_hid_progress_at = now;
        last_read_progress_at = now;
        last_ui_progress_at = now;
        first_ui_progress_at = None;
        last_sent_at = now;
        last_probe_notice = now;
        started_at = now;
        end = started_at + stress_duration;
    }

    while Instant::now() < end {
        log_tracker.refresh();
        if let Some(marker) = log_tracker.bad_marker {
            bail!("probe detected bad marker in debugcon log: {marker}");
        }

        if log_tracker.liveness_epoch > 0 {
            heartbeat_seen = true;
            if last_liveness_epoch != Some(log_tracker.liveness_epoch) {
                last_liveness_epoch = Some(log_tracker.liveness_epoch);
                last_heartbeat_second = log_tracker.latest_kernel_second;
                last_heartbeat_at = Instant::now();
            }
        }

        if heartbeat_seen
            && Instant::now().saturating_duration_since(last_heartbeat_at) > heartbeat_stall
        {
            bail!(
                "probe detected stalled kernel/service heartbeat after {:?} (last kernel second={})",
                heartbeat_stall,
                last_heartbeat_second
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| String::from("none"))
            );
        }

        if input_device.is_some() {
            let next_progress = log_tracker.input_progress;
            let now = Instant::now();
            let xhci_advanced = next_progress.xhci_events > input_progress.xhci_events;
            let hid_advanced =
                next_progress.hid_pointer_reports > input_progress.hid_pointer_reports;
            let read_advanced = next_progress.input_read_events > input_progress.input_read_events;
            let ui_advanced = next_progress.ui_input_events > input_progress.ui_input_events;
            let ui_caught_up = input_device.is_some_and(UsbInputDevice::is_absolute)
                && display_size.is_some_and(|(width, height)| {
                    next_progress.cursor_matches_abs(last_sent_x, last_sent_y, width, height)
                });
            if first_ui_progress_at.is_none() && ui_advanced {
                first_ui_progress_at = Some(now);
                last_ui_progress_at = now;
            }
            if hid_advanced {
                last_hid_progress_at = now;
            }
            if xhci_advanced && !hid_advanced {
                last_hid_progress_at = now;
            }
            if read_advanced {
                last_read_progress_at = now;
            }
            if ui_advanced {
                last_ui_progress_at = now;
            }
            if ui_caught_up {
                first_ui_progress_at.get_or_insert(now);
                last_ui_progress_at = now;
            }
            if next_progress.ui_input_last_age_ms.is_some_and(|age_ms| {
                u128::from(age_ms) <= input_stall.as_millis()
                    && next_progress.ui_input_delivered_events
                        >= input_progress.ui_input_delivered_events
            }) {
                first_ui_progress_at.get_or_insert(now);
                last_ui_progress_at = now;
            }
            input_progress = next_progress;
            if now.saturating_duration_since(last_probe_notice) >= Duration::from_secs(2) {
                println!(
                    "probe input send progress steps={} last_abs={},{} last_rel={},{} changed_sends={} qmp_last_age_ms={} qmp_max_ms={} qmp_slow={} xhci_events={} xhci_success={} xhci_short={} xhci_empty={} xhci_reports={} hid_reports={} read_events={} ui_events={} ui_reader_age_ms={:?} ui_pending={:?} ui_raw_events={} ui_delivered_events={}",
                    step,
                    last_sent_x,
                    last_sent_y,
                    last_sent_dx,
                    last_sent_dy,
                    changed_sends,
                    now.saturating_duration_since(last_sent_at).as_millis(),
                    max_qmp_send.as_millis(),
                    slow_qmp_sends,
                    input_progress.xhci_events,
                    input_progress.xhci_success,
                    input_progress.xhci_short,
                    input_progress.xhci_empty,
                    input_progress.xhci_reports,
                    input_progress.hid_pointer_reports,
                    input_progress.input_read_events,
                    input_progress.ui_input_events,
                    input_progress.ui_input_last_age_ms,
                    input_progress.ui_input_pending,
                    input_progress.ui_input_raw_events,
                    input_progress.ui_input_delivered_events,
                );
                last_probe_notice = now;
            }
            let input_start_expired = now.saturating_duration_since(started_at) > input_start;
            let ui_progress_seen = first_ui_progress_at.is_some();
            if input_start_expired && !ui_progress_seen {
                let mice = qmp_probe_mice_status(qmp);
                bail!(
                    "probe UI input progress never started: elapsed_ms={} sent_steps={} last_abs={},{} last_rel={},{} qmp_max_ms={} qmp_slow={} changed_sends={} xhci_events={} xhci_success={} xhci_short={} xhci_empty={} xhci_reports={} hid_reports={} read_events={} ui_events={} ui_reader_age_ms={:?} ui_pending={:?} ui_raw_events={} ui_delivered_events={} mice={}",
                    now.saturating_duration_since(started_at).as_millis(),
                    step,
                    last_sent_x,
                    last_sent_y,
                    last_sent_dx,
                    last_sent_dy,
                    max_qmp_send.as_millis(),
                    slow_qmp_sends,
                    changed_sends,
                    input_progress.xhci_events,
                    input_progress.xhci_success,
                    input_progress.xhci_short,
                    input_progress.xhci_empty,
                    input_progress.xhci_reports,
                    input_progress.hid_pointer_reports,
                    input_progress.input_read_events,
                    input_progress.ui_input_events,
                    input_progress.ui_input_last_age_ms,
                    input_progress.ui_input_pending,
                    input_progress.ui_input_raw_events,
                    input_progress.ui_input_delivered_events,
                    mice
                );
            }
            let ui_currently_caught_up = input_device.is_some_and(UsbInputDevice::is_absolute)
                && display_size.is_some_and(|(width, height)| {
                    input_progress.cursor_matches_abs(last_sent_x, last_sent_y, width, height)
                });
            if ui_currently_caught_up {
                last_ui_progress_at = now;
            }
            let stall_stage = if now.saturating_duration_since(last_hid_progress_at) > input_stall {
                "hid-ingress"
            } else if now.saturating_duration_since(last_read_progress_at) > input_stall {
                "inputd-read"
            } else if input_progress
                .ui_input_last_age_ms
                .is_none_or(|age_ms| u128::from(age_ms) > input_stall.as_millis())
            {
                "uiserver-input-reader"
            } else {
                "cursor-present"
            };
            if ui_progress_seen
                && !ui_currently_caught_up
                && now.saturating_duration_since(last_ui_progress_at) > input_stall
            {
                let mice = qmp_probe_mice_status(qmp);
                bail!(
                    "probe UI input progress stalled: stage={} sent_steps={} qmp_last_age_ms={} hid_age_ms={} read_age_ms={} ui_age_ms={} last_abs={},{} last_rel={},{} cursor={:?},{:?} qmp_max_ms={} qmp_slow={} changed_sends={} xhci_events={} xhci_success={} xhci_short={} xhci_empty={} xhci_reports={} hid_reports={} read_events={} ui_events={} ui_reader_age_ms={:?} ui_pending={:?} ui_raw_events={} ui_delivered_events={} mice={}",
                    stall_stage,
                    step,
                    now.saturating_duration_since(last_sent_at).as_millis(),
                    now.saturating_duration_since(last_hid_progress_at)
                        .as_millis(),
                    now.saturating_duration_since(last_read_progress_at)
                        .as_millis(),
                    now.saturating_duration_since(last_ui_progress_at)
                        .as_millis(),
                    last_sent_x,
                    last_sent_y,
                    last_sent_dx,
                    last_sent_dy,
                    input_progress.cursor_x,
                    input_progress.cursor_y,
                    max_qmp_send.as_millis(),
                    slow_qmp_sends,
                    changed_sends,
                    input_progress.xhci_events,
                    input_progress.xhci_success,
                    input_progress.xhci_short,
                    input_progress.xhci_empty,
                    input_progress.xhci_reports,
                    input_progress.hid_pointer_reports,
                    input_progress.input_read_events,
                    input_progress.ui_input_events,
                    input_progress.ui_input_last_age_ms,
                    input_progress.ui_input_pending,
                    input_progress.ui_input_raw_events,
                    input_progress.ui_input_delivered_events,
                    mice
                );
            }
        }

        x += dx;
        y += dy;
        if !(ABS_MIN..=ABS_MAX).contains(&x) {
            dx = -dx;
            x = x.clamp(ABS_MIN, ABS_MAX);
        }
        if !(ABS_MIN..=ABS_MAX).contains(&y) {
            dy = -dy;
            y = y.clamp(ABS_MIN, ABS_MAX);
        }

        if input_device.is_some_and(UsbInputDevice::is_absolute) {
            let prev_x = last_sent_x;
            let prev_y = last_sent_y;
            let send_started = Instant::now();
            qmp_input_send_pointer_abs(
                qmp,
                input_route_device,
                x as u16,
                y as u16,
                None,
                Instant::now() + PROBE_QMP_TIMEOUT,
            )?;
            let elapsed = send_started.elapsed();
            max_qmp_send = max_qmp_send.max(elapsed);
            if elapsed > step_delay.saturating_mul(2).max(Duration::from_millis(50)) {
                slow_qmp_sends = slow_qmp_sends.saturating_add(1);
            }
            last_sent_x = x as u16;
            last_sent_y = y as u16;
            last_sent_dx = 0;
            last_sent_dy = 0;
            last_sent_at = Instant::now();
            if last_sent_x != prev_x || last_sent_y != prev_y {
                changed_sends = changed_sends.saturating_add(1);
            }
        } else {
            let rel_dx = (dx / 175).clamp(-32, 32) as i16;
            let rel_dy = (dy / 175).clamp(-32, 32) as i16;
            let send_started = Instant::now();
            qmp_input_send_pointer_rel(
                qmp,
                input_route_device,
                rel_dx,
                rel_dy,
                None,
                Instant::now() + PROBE_QMP_TIMEOUT,
            )?;
            let elapsed = send_started.elapsed();
            max_qmp_send = max_qmp_send.max(elapsed);
            if elapsed > step_delay.saturating_mul(2).max(Duration::from_millis(50)) {
                slow_qmp_sends = slow_qmp_sends.saturating_add(1);
            }
            last_sent_dx = rel_dx;
            last_sent_dy = rel_dy;
            last_sent_at = Instant::now();
            if rel_dx != 0 || rel_dy != 0 {
                changed_sends = changed_sends.saturating_add(1);
            }
        }
        thread::sleep(step_delay);
        step += 1;
    }
    if input_device.is_some_and(UsbInputDevice::is_absolute) {
        qmp_input_send_pointer_abs(
            qmp,
            input_route_device,
            x as u16,
            y as u16,
            None,
            Instant::now() + PROBE_QMP_TIMEOUT,
        )?;
    } else {
        qmp_input_send_pointer_rel(
            qmp,
            input_route_device,
            0,
            0,
            None,
            Instant::now() + PROBE_QMP_TIMEOUT,
        )?;
    }
    thread::sleep(Duration::from_millis(200));
    println!(
        "probe mouse stress sent steps={} elapsed_ms={} step_ms={} device={}",
        step,
        started_at.elapsed().as_millis(),
        step_delay.as_millis(),
        input_device
            .map(UsbInputDevice::label)
            .unwrap_or("relative"),
    );
    Ok(())
}

fn qmp_probe_mice_status(qmp: &mut UnixStream) -> String {
    let hmp = match qmp_hmp_capture(qmp, "info mice", Instant::now() + PROBE_QMP_TIMEOUT) {
        Ok(status) => collapse_probe_status(&status, 512),
        Err(error) => format!("unavailable:{error}"),
    };
    let query = match qmp_query_mice(qmp, Instant::now() + PROBE_QMP_TIMEOUT) {
        Ok(status) => collapse_probe_status(&status, 512),
        Err(error) => format!("unavailable:{error}"),
    };
    format!("info_mice={hmp} query_mice={query}")
}

fn qmp_query_mice(qmp: &mut UnixStream, deadline: Instant) -> Result<String> {
    qmp.write_all(b"{\"execute\":\"query-mice\"}\n")?;
    wait_for_qmp_return_message(qmp, deadline)
}

fn collapse_probe_status(status: &str, max_len: usize) -> String {
    let mut compact = status.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() > max_len {
        compact.truncate(max_len);
        compact.push_str("...");
    }
    compact
}

fn qmp_input_send_pointer_abs(
    qmp: &mut UnixStream,
    device: Option<&str>,
    x: u16,
    y: u16,
    button_down: Option<bool>,
    deadline: Instant,
) -> Result<()> {
    let mut events = format!(
        r#"{{"type":"abs","data":{{"axis":"x","value":{x}}}}},{{"type":"abs","data":{{"axis":"y","value":{y}}}}}"#
    );
    if let Some(down) = button_down {
        events.push_str(&format!(
            r#",{{"type":"btn","data":{{"down":{},"button":"left"}}}}"#,
            if down { "true" } else { "false" }
        ));
    }

    let command = qmp_input_send_event_command(device, events.as_str());
    qmp_execute(qmp, &command, deadline)
}

fn qmp_input_send_pointer_rel(
    qmp: &mut UnixStream,
    device: Option<&str>,
    dx: i16,
    dy: i16,
    button_down: Option<bool>,
    deadline: Instant,
) -> Result<()> {
    let mut events = format!(
        r#"{{"type":"rel","data":{{"axis":"x","value":{dx}}}}},{{"type":"rel","data":{{"axis":"y","value":{dy}}}}}"#
    );
    if let Some(down) = button_down {
        events.push_str(&format!(
            r#",{{"type":"btn","data":{{"down":{},"button":"left"}}}}"#,
            if down { "true" } else { "false" }
        ));
    }

    let command = qmp_input_send_event_command(device, events.as_str());
    qmp_execute(qmp, &command, deadline)
}

#[derive(Clone, Copy, Debug, Default)]
struct ProbeInputProgress {
    xhci_events: u64,
    xhci_success: u64,
    xhci_short: u64,
    xhci_empty: u64,
    xhci_reports: u64,
    hid_pointer_reports: u64,
    input_read_events: u64,
    ui_input_events: u64,
    ui_input_last_age_ms: Option<u64>,
    ui_input_pending: Option<u64>,
    ui_input_raw_events: u64,
    ui_input_delivered_events: u64,
    cursor_x: Option<i32>,
    cursor_y: Option<i32>,
}

impl ProbeInputProgress {
    fn cursor_matches_abs(&self, abs_x: u16, abs_y: u16, width: u32, height: u32) -> bool {
        let Some(cursor_x) = self.cursor_x else {
            return false;
        };
        let Some(cursor_y) = self.cursor_y else {
            return false;
        };
        if width == 0 || height == 0 {
            return false;
        }
        let max_x = width.saturating_sub(1);
        let max_y = height.saturating_sub(1);
        let expected_x = ((u64::from(abs_x) * u64::from(max_x)) / 0x7fff) as i32;
        let expected_y = ((u64::from(abs_y) * u64::from(max_y)) / 0x7fff) as i32;
        cursor_x.abs_diff(expected_x) <= 3 && cursor_y.abs_diff(expected_y) <= 3
    }

    fn observe_line(&mut self, line: &str) {
        if line.contains("usb input diag") {
            self.xhci_events = self
                .xhci_events
                .saturating_add(parse_u64_metric(line, "xhci_delta=").unwrap_or(0));
            self.xhci_success = self
                .xhci_success
                .saturating_add(parse_u64_metric(line, "xhci_success_delta=").unwrap_or(0));
            self.xhci_short = self
                .xhci_short
                .saturating_add(parse_u64_metric(line, "xhci_short_delta=").unwrap_or(0));
            self.xhci_empty = self
                .xhci_empty
                .saturating_add(parse_u64_metric(line, "xhci_empty_delta=").unwrap_or(0));
            self.xhci_reports = self
                .xhci_reports
                .saturating_add(parse_u64_metric(line, "xhci_reports_delta=").unwrap_or(0));
            self.hid_pointer_reports = self
                .hid_pointer_reports
                .saturating_add(parse_u64_metric(line, "hid_ptr_delta=").unwrap_or(0));
            self.input_read_events = self
                .input_read_events
                .saturating_add(parse_u64_metric(line, "read_events_delta=").unwrap_or(0));
        }
        if line.contains("uiserver: update tick") {
            self.ui_input_events = self
                .ui_input_events
                .saturating_add(parse_u64_metric(line, "input_loop_events=").unwrap_or(0));
            self.ui_input_last_age_ms = parse_u64_metric(line, "input_last_age_ms=");
            self.ui_input_pending = parse_u64_metric(line, "input_pending=");
            if let Some(raw_events) = parse_u64_metric(line, "input_raw_events=") {
                self.ui_input_raw_events = raw_events;
            }
            if let Some(delivered_events) = parse_u64_metric(line, "input_events=") {
                self.ui_input_delivered_events = delivered_events;
            }
            if let Some((x, y)) = parse_i32_pair_metric(line, "cursor=") {
                self.cursor_x = Some(x);
                self.cursor_y = Some(y);
            }
        } else if line.contains("uiserver: input pulse") {
            self.ui_input_events = self
                .ui_input_events
                .saturating_add(parse_u64_metric(line, "events=").unwrap_or(0));
            if let Some(raw_events) = parse_u64_metric(line, "input_raw_events=") {
                self.ui_input_raw_events = raw_events;
            }
            if let Some(delivered_events) = parse_u64_metric(line, "input_events=") {
                self.ui_input_delivered_events = delivered_events;
            }
            if let Some((x, y)) = parse_i32_pair_metric(line, "cursor=") {
                self.cursor_x = Some(x);
                self.cursor_y = Some(y);
            }
        }
    }
}

struct ProbeLogTracker {
    path: PathBuf,
    offset: u64,
    pending: String,
    bad_marker: Option<&'static str>,
    liveness_epoch: u64,
    latest_kernel_second: Option<u64>,
    input_progress: ProbeInputProgress,
}

impl ProbeLogTracker {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            offset: 0,
            pending: String::new(),
            bad_marker: None,
            liveness_epoch: 0,
            latest_kernel_second: None,
            input_progress: ProbeInputProgress::default(),
        }
    }

    fn refresh(&mut self) {
        let Ok(mut file) = fs::File::open(&self.path) else {
            return;
        };
        let Ok(metadata) = file.metadata() else {
            return;
        };
        let len = metadata.len();
        if len < self.offset {
            self.offset = 0;
            self.pending.clear();
            self.bad_marker = None;
            self.liveness_epoch = 0;
            self.latest_kernel_second = None;
            self.input_progress = ProbeInputProgress::default();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return;
        }
        self.offset = len;
        if bytes.is_empty() {
            return;
        }

        let chunk = String::from_utf8_lossy(&bytes);
        self.observe_chunk(&chunk);
    }

    fn observe_chunk(&mut self, chunk: &str) {
        self.pending.push_str(chunk);
        while let Some(newline) = self.pending.find('\n') {
            let mut line = self.pending.drain(..=newline).collect::<String>();
            if line.ends_with('\n') {
                line.pop();
            }
            if line.ends_with('\r') {
                line.pop();
            }
            self.observe_line(&line);
        }
        if self.pending.len() > 64 * 1024 {
            let line = std::mem::take(&mut self.pending);
            self.observe_line(&line);
        }
    }

    fn observe_line(&mut self, line: &str) {
        if self.bad_marker.is_none() {
            self.bad_marker = PROBE_BAD_MARKERS
                .into_iter()
                .find(|marker| line.contains(marker));
        }
        if let Some(second) = parse_kernel_alive_second(line) {
            self.liveness_epoch = self.liveness_epoch.saturating_add(1);
            self.latest_kernel_second = Some(second);
        } else if line.contains("uiserver: heartbeat") {
            self.liveness_epoch = self.liveness_epoch.saturating_add(1);
        }
        self.input_progress.observe_line(line);
    }
}

fn parse_u64_metric(line: &str, key: &str) -> Option<u64> {
    let value = line.split(key).nth(1)?.split_whitespace().next()?;
    value.trim_end_matches(',').parse().ok()
}

fn parse_i32_pair_metric(line: &str, key: &str) -> Option<(i32, i32)> {
    let value = line.split(key).nth(1)?.split_whitespace().next()?;
    let value = value.trim_end_matches(',');
    let (x, y) = value.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

fn qmp_input_send_event_command(device: Option<&str>, events: &str) -> String {
    let device = device
        .map(|name| format!(r#""device":"{name}","#))
        .unwrap_or_default();
    format!(
        r#"{{"execute":"input-send-event","arguments":{{{}"events":[{}]}}}}"#,
        device, events
    )
}

fn parse_kernel_alive_second(line: &str) -> Option<u64> {
    if let Some(rest) = line.strip_prefix("[heartbeat] ") {
        return parse_kernel_alive_second(rest);
    }

    if let Some(index) = line.find("msg=\"second=") {
        return parse_kernel_alive_second(&line[index + "msg=\"".len()..]);
    }

    if let Some(rest) = line.strip_prefix("kernel alive: second=") {
        return rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok());
    }

    if let Some(rest) = line.strip_prefix("second=") {
        return rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok());
    }

    None
}

fn probe_duration_env(name: &str, default: Duration) -> Duration {
    env_string(name)
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

fn qmp_screendump(qmp: &mut UnixStream, path: &Path) -> Result<()> {
    let command = format!(
        r#"{{"execute":"screendump","arguments":{{"filename":"{}"}}}}"#,
        json_escape(path.to_string_lossy().as_ref())
    );
    qmp_execute(qmp, &command, Instant::now() + PROBE_QMP_TIMEOUT)?;
    Ok(())
}

fn qmp_quit(qmp: &mut UnixStream) -> Result<()> {
    qmp_execute(
        qmp,
        r#"{"execute":"quit"}"#,
        Instant::now() + Duration::from_secs(2),
    )?;
    Ok(())
}

fn qmp_hmp_capture(qmp: &mut UnixStream, command_line: &str, deadline: Instant) -> Result<String> {
    let command = format!(
        r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{}"}}}}"#,
        json_escape(command_line)
    );
    qmp.write_all(command.as_bytes())?;
    qmp.write_all(b"\n")?;
    wait_for_qmp_return_message(qmp, deadline)
}

fn qmp_hmp_send_key(qmp: &mut UnixStream, key: &str) -> Result<()> {
    let command_line = format!("sendkey {key}");
    let _ = qmp_hmp_capture(qmp, &command_line, Instant::now() + PROBE_QMP_TIMEOUT)?;
    thread::sleep(Duration::from_millis(25));
    Ok(())
}

fn qmp_hmp_type_text(qmp: &mut UnixStream, text: &str) -> Result<()> {
    for ch in text.chars() {
        let Some(key) = hmp_key_for_char(ch) else {
            bail!("RUSTOS_PROBE_KEY_TEXT contains unsupported character: {ch:?}");
        };
        qmp_hmp_send_key(qmp, key)?;
    }
    thread::sleep(Duration::from_millis(500));
    Ok(())
}

fn hmp_key_for_char(ch: char) -> Option<&'static str> {
    match ch {
        'a'..='z' | '0'..='9' => Some(match ch {
            'a' => "a",
            'b' => "b",
            'c' => "c",
            'd' => "d",
            'e' => "e",
            'f' => "f",
            'g' => "g",
            'h' => "h",
            'i' => "i",
            'j' => "j",
            'k' => "k",
            'l' => "l",
            'm' => "m",
            'n' => "n",
            'o' => "o",
            'p' => "p",
            'q' => "q",
            'r' => "r",
            's' => "s",
            't' => "t",
            'u' => "u",
            'v' => "v",
            'w' => "w",
            'x' => "x",
            'y' => "y",
            'z' => "z",
            '0' => "0",
            '1' => "1",
            '2' => "2",
            '3' => "3",
            '4' => "4",
            '5' => "5",
            '6' => "6",
            '7' => "7",
            '8' => "8",
            '9' => "9",
            _ => return None,
        }),
        'A' => Some("shift-a"),
        'B' => Some("shift-b"),
        'C' => Some("shift-c"),
        'D' => Some("shift-d"),
        'E' => Some("shift-e"),
        'F' => Some("shift-f"),
        'G' => Some("shift-g"),
        'H' => Some("shift-h"),
        'I' => Some("shift-i"),
        'J' => Some("shift-j"),
        'K' => Some("shift-k"),
        'L' => Some("shift-l"),
        'M' => Some("shift-m"),
        'N' => Some("shift-n"),
        'O' => Some("shift-o"),
        'P' => Some("shift-p"),
        'Q' => Some("shift-q"),
        'R' => Some("shift-r"),
        'S' => Some("shift-s"),
        'T' => Some("shift-t"),
        'U' => Some("shift-u"),
        'V' => Some("shift-v"),
        'W' => Some("shift-w"),
        'X' => Some("shift-x"),
        'Y' => Some("shift-y"),
        'Z' => Some("shift-z"),
        ' ' => Some("spc"),
        '\n' | '\r' => Some("ret"),
        '\t' => Some("tab"),
        '-' => Some("minus"),
        '_' => Some("shift-minus"),
        '.' => Some("dot"),
        ',' => Some("comma"),
        '/' => Some("slash"),
        '\\' => Some("backslash"),
        _ => None,
    }
}

fn qmp_execute(qmp: &mut UnixStream, command: &str, deadline: Instant) -> Result<()> {
    qmp.write_all(command.as_bytes())?;
    qmp.write_all(b"\n")?;
    let _ = wait_for_qmp_return_message(qmp, deadline)?;
    Ok(())
}

fn wait_for_qmp_return_message(qmp: &mut UnixStream, deadline: Instant) -> Result<String> {
    loop {
        let message = read_qmp_message(qmp, deadline)?;
        if message.contains("\"return\"") {
            return Ok(message);
        }
        if message.contains("\"error\"") {
            bail!("QMP command failed: {message}");
        }
    }
}

fn read_qmp_message(qmp: &mut UnixStream, deadline: Instant) -> Result<String> {
    let mut buffer = Vec::new();
    loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for QMP response");
        }

        let mut byte = [0_u8; 1];
        match qmp.read(&mut byte) {
            Ok(0) => {
                if !buffer.is_empty() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Ok(_) => {
                buffer.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => bail!("failed to read QMP response: {err}"),
        }
    }

    let text = String::from_utf8_lossy(&buffer).trim().to_string();
    if text.is_empty() {
        read_qmp_message(qmp, deadline)
    } else {
        Ok(text)
    }
}

#[derive(Clone, Copy)]
struct PpmSummary {
    width: u32,
    height: u32,
    non_black_pixels: u64,
    avg_r: u8,
    avg_g: u8,
    avg_b: u8,
}

impl PpmSummary {
    fn dimensions(self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn pixel_count(self) -> u64 {
        u64::from(self.width).saturating_mul(u64::from(self.height))
    }

    fn dominant_channel(self) -> &'static str {
        if self.avg_r >= self.avg_g && self.avg_r >= self.avg_b {
            "red"
        } else if self.avg_g >= self.avg_b {
            "green"
        } else {
            "blue"
        }
    }
}

fn validate_ppm_visible(stage: &str, summary: PpmSummary) -> Result<()> {
    let min_visible_pixels = (summary.pixel_count() / 1000).max(64);
    if summary.non_black_pixels < min_visible_pixels {
        bail!(
            "display probe captured an effectively black {stage} frame: non_black_pixels={} min_visible_pixels={} geometry={}x{}",
            summary.non_black_pixels,
            min_visible_pixels,
            summary.width,
            summary.height
        );
    }
    Ok(())
}

fn read_ppm_summary(path: &Path) -> Result<PpmSummary> {
    let data = fs::read(path)?;
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < data.len() && tokens.len() < 4 {
        while i < data.len() && data[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        if data[i] == b'#' {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < data.len() && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        tokens.push(String::from_utf8_lossy(&data[start..i]).into_owned());
    }

    if tokens.len() < 4 || tokens[0] != "P6" {
        bail!("invalid screendump header: {}", path.display());
    }

    let width = tokens[1]
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid screendump width: {}", tokens[1]))?;
    let height = tokens[2]
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid screendump height: {}", tokens[2]))?;
    let max_value = tokens[3]
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid screendump max value: {}", tokens[3]))?;
    if max_value != 255 {
        bail!("unsupported screendump max value: {max_value}");
    }
    if i < data.len() && data[i].is_ascii_whitespace() {
        i += 1;
    }
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| anyhow!("screendump dimensions overflow: {}x{}", width, height))?;
    let pixel_end = i
        .checked_add(expected_len)
        .ok_or_else(|| anyhow!("screendump pixel length overflow: {}", path.display()))?;
    let pixels = data
        .get(i..pixel_end)
        .ok_or_else(|| anyhow!("truncated screendump pixel data: {}", path.display()))?;
    let mut non_black_pixels = 0u64;
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;
    for pixel in pixels.chunks_exact(3) {
        let r = u64::from(pixel[0]);
        let g = u64::from(pixel[1]);
        let b = u64::from(pixel[2]);
        if r != 0 || g != 0 || b != 0 {
            non_black_pixels += 1;
        }
        sum_r += r;
        sum_g += g;
        sum_b += b;
    }
    let pixel_count = u64::from(width).saturating_mul(u64::from(height)).max(1);

    Ok(PpmSummary {
        width,
        height,
        non_black_pixels,
        avg_r: (sum_r / pixel_count) as u8,
        avg_g: (sum_g / pixel_count) as u8,
        avg_b: (sum_b / pixel_count) as u8,
    })
}

fn json_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn wait_for_child_or_kill(child: &mut Child) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            return Ok(child.wait()?);
        }
        thread::sleep(Duration::from_millis(50));
    }
}
