use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::Result;
use crate::config::Config;
use crate::util::{create_temp_dir, env_string, read_trimmed, resolve_command_path};

struct RunOptions {
    profile: String,
    accel: String,
    usb_input: bool,
    debugcon: DebugconMode,
    qemu_log: QemuLogMode,
    vfio_force: bool,
    auto_phoenix3_passthrough: bool,
    vfio_hosts: Vec<String>,
    qemu_user_args: Vec<String>,
}

impl RunOptions {
    fn from_env() -> Self {
        Self {
            profile: env_string("RUSTOS_QEMU_PROFILE").unwrap_or_else(|| String::from("default")),
            accel: env_string("RUSTOS_QEMU_ACCEL").unwrap_or_default(),
            usb_input: false,
            debugcon: DebugconMode::File,
            qemu_log: QemuLogMode::None,
            vfio_force: false,
            auto_phoenix3_passthrough: false,
            vfio_hosts: Vec::new(),
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
    usb_args: Vec<OsString>,
    display_args: Vec<OsString>,
    debugcon_args: Vec<OsString>,
    qemu_log_args: Vec<OsString>,
    qemu_user_args: Vec<String>,
}

#[derive(Clone, Copy)]
enum QemuBootDisk {
    Ahci,
    Nvme,
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
const PROBE_HEARTBEAT_STALL_DEFAULT: Duration = Duration::from_millis(2500);

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
            "--debugcon" => {
                let value = next_required_arg(args, "--debugcon")?;
                options.debugcon = DebugconMode::parse(value.as_str())
                    .ok_or_else(|| format!("invalid --debugcon value: {value}"))?;
            }
            "--qemu-log" => {
                let value = next_required_arg(args, "--qemu-log")?;
                options.qemu_log = QemuLogMode::parse(value.as_str())
                    .ok_or_else(|| format!("invalid --qemu-log value: {value}"))?;
            }
            "--vfio-pci" => {
                let host = next_required_arg(args, "--vfio-pci")?;
                append_unique_string(&mut options.vfio_hosts, host);
            }
            "--phoenix3-passthrough" => options.auto_phoenix3_passthrough = true,
            "--vfio-force" => options.vfio_force = true,
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
    let prepared = prepare_run(config, options)?;

    println!(
        "\n====================================\nStarting QEMU...\n====================================\n"
    );

    let mut command = base_qemu_command(config, &prepared.qemu_bin);
    append_qemu_args(
        &mut command,
        &prepared.profile_args,
        &prepared.vfio_args,
        &prepared.usb_args,
        &prepared.display_args,
        &prepared.debugcon_args,
        &prepared.qemu_log_args,
        &prepared.qemu_user_args,
    );

    let status = command.status()?;

    println!(
        "\n====================================\nQEMU exited with code {}\n====================================\n",
        status
            .code()
            .map(|code: i32| code.to_string())
            .unwrap_or_else(|| String::from("signal"))
    );

    if status.success() {
        Ok(())
    } else {
        Err(format!("QEMU exited with status {status}").into())
    }
}

fn ensure_qemu_prerequisites(config: &Config) -> Result<()> {
    if !config.image_dir.is_dir() {
        return Err(format!(
            "missing build image directory: {} (run `cargo xtask build` first)",
            config.image_dir.display()
        )
        .into());
    }

    let boot_efi = config.boot_efi_path();
    if !boot_efi.is_file() {
        return Err(format!(
            "missing staged bootloader image: {} (run `cargo xtask build` first)",
            boot_efi.display()
        )
        .into());
    }

    if !config.ovmf_path.is_file() {
        return Err(format!(
            "missing OVMF firmware image: {}",
            config.ovmf_path.display()
        )
        .into());
    }

    Ok(())
}

fn prepare_run(config: &Config, options: RunOptions) -> Result<PreparedRun> {
    ensure_qemu_prerequisites(config)?;
    let qemu_bin = resolve_command_path(&config.qemu_bin).ok_or_else(|| {
        format!(
            "missing QEMU binary: {}",
            Path::new(&config.qemu_bin).display()
        )
    })?;

    let mut session = RunSession::create()?;
    let mut profile_args = build_run_profile_args(&options, &config.image_dir)?;
    let mut vfio_hosts = options.vfio_hosts.clone();
    if options.auto_phoenix3_passthrough {
        for bdf in detect_phoenix3_devices()? {
            append_unique_string(&mut vfio_hosts, bdf);
        }
    }

    let vfio_args = configure_vfio_args(&mut profile_args, &vfio_hosts, options.vfio_force)?;
    let usb_args = configure_usb_args(options.usb_input, &options.qemu_user_args);
    let display_args = configure_display_args(&options.qemu_user_args);
    let debugcon_args = configure_debugcon(config, &mut session, options.debugcon)?;
    let qemu_log_args = configure_qemu_log(config, options.qemu_log)?;

    Ok(PreparedRun {
        qemu_bin,
        session,
        profile_args,
        vfio_args,
        usb_args,
        display_args,
        debugcon_args,
        qemu_log_args,
        qemu_user_args: options.qemu_user_args,
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
  --usb-input                        attach qemu-xhci + usb-kbd + usb-tablet for USB HID testing
  --debugcon <file|stdio|null>       route debugcon to file, terminal, or disable it
  --qemu-log <int|null>              write QEMU trace log to logs/qemu_interrupt.log or disable it
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
  --usb-input                        attach qemu-xhci + usb-kbd + usb-tablet for USB HID testing
  --debugcon <file|stdio|null>       route debugcon to file, terminal, or disable it
  --qemu-log <int|null>              write QEMU trace log to logs/qemu_interrupt.log or disable it
  --vfio-pci <0000:bb:dd.f>          attach a vfio-pci host device to qemu (repeatable)
  --phoenix3-passthrough             auto-attach host Phoenix3 VGA function and same-slot audio
  --vfio-force                       allow devices that currently drive an active host display
  -h, --help                         show this help

probe-display always forces headless mode and validates screendump geometry after
injecting mouse movement/click stress through QMP.
"
    );
}

fn next_required_arg<I>(args: &mut I, option: &str) -> Result<String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing value for {option}").into())
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

fn build_run_profile_args(options: &RunOptions, boot_dir: &Path) -> Result<Vec<OsString>> {
    let spec = qemu_profile_spec(options.profile.as_str())?;
    let mut args = Vec::new();
    append_boot_disk_args(&mut args, boot_dir, spec.boot_disk);
    args.push(OsString::from("-m"));
    args.push(OsString::from(spec.memory));
    args.push(OsString::from("-machine"));
    if options.accel == "kvm" {
        args.push(machine_option("q35,accel=kvm", options.usb_input));
    } else {
        args.push(machine_option("q35", options.usb_input));
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
            memory: "2G",
            cpu_when_kvm: Some("host"),
            cpu_other: None,
            smp: None,
            rtc: None,
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
            memory: "2G",
            cpu_when_kvm: Some("host"),
            cpu_other: None,
            smp: None,
            rtc: None,
        }),
        _ => Err(format!("unknown qemu profile: {profile}").into()),
    }
}

fn append_boot_disk_args(args: &mut Vec<OsString>, boot_dir: &Path, boot_disk: QemuBootDisk) {
    if matches!(boot_disk, QemuBootDisk::Ahci) {
        args.push(OsString::from("-device"));
        args.push(OsString::from("ich9-ahci,id=ahci"));
    }
    args.push(OsString::from("-drive"));
    args.push(OsString::from(format!(
        "id=bootdisk,if=none,file=fat:rw:{},format=raw",
        boot_dir.display()
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
        Err(String::from("VFIO is not available: /dev/vfio is missing.").into())
    }
}

fn validate_vfio_device(bdf: &str, vfio_force: bool) -> Result<()> {
    let devpath = Path::new("/sys/bus/pci/devices").join(bdf);
    if !devpath.is_dir() {
        return Err(format!("VFIO host device not found: {bdf}").into());
    }

    let driver_link = devpath.join("driver");
    if !driver_link.exists() {
        return Err(format!("VFIO host device has no bound driver: {bdf}").into());
    }

    let driver_path = fs::canonicalize(&driver_link)?;
    let driver_name = driver_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>");
    if driver_name != "vfio-pci" {
        return Err(format!(
            "VFIO host device is not bound to vfio-pci: {bdf} (current: {driver_name})"
        )
        .into());
    }

    let iommu_group_link = devpath.join("iommu_group");
    if iommu_group_link.exists() {
        let iommu_group = fs::canonicalize(&iommu_group_link)?;
        let group_name = iommu_group
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("failed to resolve IOMMU group for {bdf}"))?;
        if !Path::new("/dev/vfio").join(group_name).exists() {
            return Err(format!(
                "VFIO IOMMU group device is missing: /dev/vfio/{group_name} for {bdf}"
            )
            .into());
        }
    }

    if !vfio_force && device_drives_active_host_display(bdf)? {
        return Err(format!(
            "Refusing to passthrough active host display device {bdf}. Use --vfio-force only after moving the host off this GPU."
        )
        .into());
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

    Err(String::from("Phoenix3 (1002:1900) host GPU not found.").into())
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
) -> Result<Vec<OsString>> {
    match mode {
        DebugconMode::Null => {
            return Ok(vec![OsString::from("-debugcon"), OsString::from("null")]);
        }
        DebugconMode::Stdio => {
            return Ok(vec![OsString::from("-debugcon"), OsString::from("stdio")]);
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

    Ok(vec![
        OsString::from("-debugcon"),
        OsString::from(format!("file:{}", debugcon_log.display())),
    ])
}

fn configure_qemu_log(config: &Config, mode: QemuLogMode) -> Result<Vec<OsString>> {
    match mode {
        QemuLogMode::None => Ok(Vec::new()),
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

fn configure_display_args(qemu_user_args: &[String]) -> Vec<OsString> {
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
        OsString::from("gtk,gl=off,grab-on-hover=on"),
    ]
}

fn configure_usb_args(enable_usb_input: bool, qemu_user_args: &[String]) -> Vec<OsString> {
    if !enable_usb_input {
        return Vec::new();
    }

    if qemu_user_args.iter().any(|arg| {
        arg.contains("qemu-xhci")
            || arg.contains("usb-kbd")
            || arg.contains("usb-tablet")
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
        OsString::from("usb-tablet,bus=xhci.0,id=usbtablet"),
    ]
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

fn append_qemu_args(
    command: &mut Command,
    profile_args: &[OsString],
    vfio_args: &[OsString],
    usb_args: &[OsString],
    display_args: &[OsString],
    debugcon_args: &[OsString],
    qemu_log_args: &[OsString],
    qemu_user_args: &[String],
) {
    command.args(profile_args);
    command.args(vfio_args);
    command.args(usb_args);
    command.args(display_args);
    command.args(debugcon_args);
    command.args(qemu_log_args);
    command.args(qemu_user_args);
}

fn run_display_probe(config: &Config, options: RunOptions) -> Result<()> {
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
        &prepared.usb_args,
        &prepared.display_args,
        &prepared.debugcon_args,
        &prepared.qemu_log_args,
        &prepared.qemu_user_args,
    );
    command.args(&qmp_args);
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    let mut child = command.spawn()?;
    let debugcon_log = config.logs_dir.join("debugcon.log");
    let probe_result = (|| -> Result<()> {
        let mut qmp = connect_qmp(&qmp_socket, PROBE_QMP_TIMEOUT)?;
        wait_for_boot_marker(
            &debugcon_log,
            "userspace display active",
            probe_duration_env("RUSTOS_PROBE_BOOT_TIMEOUT_MS", PROBE_BOOT_TIMEOUT_DEFAULT),
        )?;

        let baseline_dump = prepared.session.temp_dir.join("probe-baseline.ppm");
        qmp_screendump(&mut qmp, &baseline_dump)?;
        let baseline = read_ppm_dimensions(&baseline_dump)?;

        if baseline.0 == 0 || baseline.1 == 0 || baseline.0 > 8192 || baseline.1 > 8192 {
            return Err(format!(
                "probe baseline geometry is invalid: {}x{}",
                baseline.0, baseline.1
            )
            .into());
        }

        run_probe_mouse_stress(&mut qmp, &debugcon_log)?;

        let stressed_dump = prepared.session.temp_dir.join("probe-stressed.ppm");
        qmp_screendump(&mut qmp, &stressed_dump)?;
        let stressed = read_ppm_dimensions(&stressed_dump)?;
        if stressed != baseline {
            return Err(format!(
                "display geometry changed during probe: baseline={}x{}, stressed={}x{}",
                baseline.0, baseline.1, stressed.0, stressed.1
            )
            .into());
        }

        let debugcon = fs::read_to_string(&debugcon_log).unwrap_or_default();
        for marker in PROBE_BAD_MARKERS {
            if debugcon.contains(marker) {
                return Err(format!("probe detected bad marker in debugcon log: {marker}").into());
            }
        }

        qmp_quit(&mut qmp)?;
        Ok(())
    })();

    let status = wait_for_child_or_kill(&mut child)?;
    if let Err(err) = probe_result {
        return Err(err);
    }
    if !status.success() {
        return Err(format!("QEMU exited with status {status}").into());
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
            Err(err) if Instant::now() < deadline => {
                if err.kind() != ErrorKind::NotFound && err.kind() != ErrorKind::ConnectionRefused {
                    thread::sleep(Duration::from_millis(50));
                } else {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            Err(err) => return Err(format!("failed to connect to QMP socket: {err}").into()),
        }
    }
}

fn wait_for_boot_marker(log_path: &Path, marker: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let contents = fs::read_to_string(log_path).unwrap_or_default();
        if contents.contains(marker) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for boot marker: {marker}").into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn run_probe_mouse_stress(qmp: &mut UnixStream, debugcon_log: &Path) -> Result<()> {
    let stress_duration =
        probe_duration_env("RUSTOS_PROBE_STRESS_MS", PROBE_STRESS_DURATION_DEFAULT);
    let heartbeat_stall = probe_duration_env(
        "RUSTOS_PROBE_HEARTBEAT_STALL_MS",
        PROBE_HEARTBEAT_STALL_DEFAULT,
    );
    let end = Instant::now() + stress_duration;
    let mut step = 0usize;
    let mut last_heartbeat_second = latest_kernel_alive_second(debugcon_log);
    let mut last_heartbeat_at = Instant::now();
    let mut x = 0x4000_i32;
    let mut y = 0x3000_i32;
    let mut dx = 1400_i32;
    let mut dy = 900_i32;
    const ABS_MIN: i32 = 0;
    const ABS_MAX: i32 = 0x7fff;

    while Instant::now() < end {
        let log = fs::read_to_string(debugcon_log).unwrap_or_default();
        for marker in PROBE_BAD_MARKERS {
            if log.contains(marker) {
                return Err(format!("probe detected bad marker in debugcon log: {marker}").into());
            }
        }

        if let Some(second) = latest_kernel_alive_second(debugcon_log) {
            if last_heartbeat_second != Some(second) {
                last_heartbeat_second = Some(second);
                last_heartbeat_at = Instant::now();
            }
        }

        if Instant::now().saturating_duration_since(last_heartbeat_at) > heartbeat_stall {
            return Err(format!(
                "probe detected stalled kernel heartbeat after {:?} (last second={})",
                heartbeat_stall,
                last_heartbeat_second
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| String::from("none"))
            )
            .into());
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

        let button = if step % 16 == 0 {
            Some(true)
        } else if step % 16 == 8 {
            Some(false)
        } else {
            None
        };
        qmp_input_send_pointer_abs(
            qmp,
            x as u16,
            y as u16,
            button,
            Instant::now() + PROBE_QMP_TIMEOUT,
        )?;
        thread::sleep(PROBE_STEP_DELAY);
        step += 1;
    }
    qmp_input_send_pointer_abs(
        qmp,
        x as u16,
        y as u16,
        Some(false),
        Instant::now() + PROBE_QMP_TIMEOUT,
    )?;
    thread::sleep(Duration::from_millis(200));
    Ok(())
}

fn qmp_input_send_pointer_abs(
    qmp: &mut UnixStream,
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
    let command = format!(
        r#"{{"execute":"input-send-event","arguments":{{"events":[{}]}}}}"#,
        events
    );
    qmp_execute(qmp, &command, deadline)
}

fn latest_kernel_alive_second(log_path: &Path) -> Option<u64> {
    let log = fs::read_to_string(log_path).ok()?;
    log.lines()
        .rev()
        .find_map(|line| line.strip_prefix("kernel alive: second="))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
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

fn qmp_hmp(qmp: &mut UnixStream, command_line: &str, deadline: Instant) -> Result<()> {
    let command = format!(
        r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{}"}}}}"#,
        json_escape(command_line)
    );
    qmp_execute(qmp, &command, deadline)
}

fn qmp_execute(qmp: &mut UnixStream, command: &str, deadline: Instant) -> Result<()> {
    qmp.write_all(command.as_bytes())?;
    qmp.write_all(b"\n")?;
    wait_for_qmp_return(qmp, deadline)?;
    Ok(())
}

fn wait_for_qmp_return(qmp: &mut UnixStream, deadline: Instant) -> Result<()> {
    loop {
        let message = read_qmp_message(qmp, deadline)?;
        if message.contains("\"return\"") {
            return Ok(());
        }
        if message.contains("\"error\"") {
            return Err(format!("QMP command failed: {message}").into());
        }
    }
}

fn read_qmp_message(qmp: &mut UnixStream, deadline: Instant) -> Result<String> {
    let mut buffer = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err(String::from("timed out waiting for QMP response").into());
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
            Err(err) => return Err(format!("failed to read QMP response: {err}").into()),
        }
    }

    let text = String::from_utf8_lossy(&buffer).trim().to_string();
    if text.is_empty() {
        read_qmp_message(qmp, deadline)
    } else {
        Ok(text)
    }
}

fn read_ppm_dimensions(path: &Path) -> Result<(u32, u32)> {
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
        return Err(format!("invalid screendump header: {}", path.display()).into());
    }

    let width = tokens[1]
        .parse::<u32>()
        .map_err(|_| format!("invalid screendump width: {}", tokens[1]))?;
    let height = tokens[2]
        .parse::<u32>()
        .map_err(|_| format!("invalid screendump height: {}", tokens[2]))?;
    Ok((width, height))
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
