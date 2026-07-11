use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use fs_err as fs;

use crate::Result;
use crate::config::Config;
use crate::util::{resolve_command_path, run_command};

const DVM_KERNEL: &str = "rustos-linux-dvm-x86_64.bzImage";
const DVM_ROOTFS: &str = "rustos-linux-dvm-x86_64.rootfs.cpio.xz";
const DVM_CONFIG: &str = "rustos-linux-dvm-x86_64.config";
const DVM_MANIFEST: &str = "rustos-linux-dvm-x86_64.manifest";
const DVM_MANIFEST_SCHEMA: &str = "2";
const DVM_CONTROL_CONTRACT: &str = "board/overlay/usr/share/rustos-dvm/control-plane-v1.env";
const DVM_CONTROL_PROTOCOL: &str = "agent-v1";
const DVM_CONTROL_STATE: &str = "pretransport";
const DVM_CONTROL_TRANSPORT: &str = "kvm-vsock-pending";
const DVM_CONTROL_AUTHENTICATION: &str = "kvm-host-bound-pending";
const DVM_CONTROL_CAPABILITIES: &str = "health,device-inventory";
const RUSTOS_BOOT_MARKER: &str = "rootd: core services ready, spawning initd via loaderd";
const DVM_READY_MARKER: &str = "rustos-dvm-agent: ready protocol=agent-v1 state=pretransport";
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
}

#[derive(Debug)]
struct SmokeOptions {
    dry_run: bool,
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
    let layout = prepare_layout(config)?;

    if options.dry_run {
        println!(
            "xtask: KVM smoke inputs prepared in {}",
            layout.run_dir.display()
        );
        return Ok(());
    }

    let (mut rustos, mut dvm) = spawn_guests(&qemu, config, &artifacts, &layout)?;
    let result = wait_for_parallel_boot(&mut rustos, &mut dvm, &layout, &options);
    stop_guest(&mut rustos);
    stop_guest(&mut dvm);
    result?;
    println!(
        "xtask: parallel KVM boot passed (RustOS + Linux DVM); control={} is pre-transport and does not assert a RustOS-to-DVM data plane",
        artifacts.control.control_plane(),
    );
    Ok(())
}

pub(crate) fn print_kvm_smoke_help() {
    println!(
        "\
usage: cargo xtask kvm-smoke [options]

Boots the Linux DVM and RustOS concurrently with QEMU/KVM. This is an isolated
parallel-boot check, not a production driver-domain launch: the current DVM
control contract is pre-transport.

options:
  --timeout <seconds>  wait for readiness markers (1..={MAX_SMOKE_TIMEOUT}, default 30)
  --expect <marker>    require an additional RustOS debugcon marker (repeatable)
  --dry-run            validate inputs and prepare KVM log paths without launching QEMU
  -h, --help           show this help

The default proof requires RustOS to reach init handoff and Linux DVM to report
`rustos-dvm-agent` ready. Both guests are launched as independent KVM processes;
no vsock control channel or device data plane is implied.
"
    );
}

fn parse_smoke_options<I>(mut args: I) -> Result<SmokeOptions>
where
    I: Iterator<Item = String>,
{
    let mut options = SmokeOptions {
        dry_run: false,
        timeout: Duration::from_secs(MAX_SMOKE_TIMEOUT),
        expected_markers: vec![RUSTOS_BOOT_MARKER.to_owned()],
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => options.dry_run = true,
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
    require_manifest_value(values, "data-plane", "dvm-local-virtio")?;
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
    validate_pretransport_control_contract(&control)?;
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
    validate_pretransport_control_contract(&control)?;
    Ok(control)
}

fn validate_pretransport_control_contract(control: &DvmControlContract) -> Result<()> {
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

fn prepare_layout(config: &Config) -> Result<KvmLayout> {
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
    let runtime_disk = run_dir.join("rustos-kvm.img");
    fs::copy(&config.boot_disk_image, &runtime_disk).with_context(|| {
        format!(
            "failed to create KVM runtime disk from {}",
            config.boot_disk_image.display()
        )
    })?;

    let debugcon_log = run_dir.join("rustos-debugcon.log");
    let rustos_serial_log = run_dir.join("rustos-serial.log");
    let dvm_serial_log = run_dir.join("linux-dvm-serial.log");
    let rustos_stderr_log = run_dir.join("rustos-qemu.stderr.log");
    let dvm_stderr_log = run_dir.join("linux-dvm-qemu.stderr.log");
    fs::write(&debugcon_log, "")?;
    fs::write(&rustos_serial_log, "")?;
    fs::write(&dvm_serial_log, "")?;
    fs::write(&rustos_stderr_log, "")?;
    fs::write(&dvm_stderr_log, "")?;

    Ok(KvmLayout {
        run_dir,
        runtime_disk,
        debugcon_log,
        rustos_serial_log,
        dvm_serial_log,
        rustos_stderr_log,
        dvm_stderr_log,
    })
}

fn require_qemu(config: &Config) -> Result<PathBuf> {
    resolve_command_path(&config.kvm_qemu_bin).with_context(|| {
        format!(
            "missing KVM QEMU command {}; install qemu-system-x86 or set KVM_QEMU_BIN",
            Path::new(&config.kvm_qemu_bin).display()
        )
    })
}

fn spawn_guests(
    qemu: &Path,
    config: &Config,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
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
        .args(["-display", "none", "-no-reboot", "-no-shutdown"])
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
        .args(["-serial", "chardev:serial"]);
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
            "console=ttyS0",
            "-display",
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
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&layout.dvm_stderr_log)?));
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
) -> Result<()> {
    let deadline = Instant::now() + options.timeout;
    loop {
        check_guest_running(rustos, "RustOS", &layout.rustos_stderr_log)?;
        check_guest_running(dvm, "Linux DVM", &layout.dvm_stderr_log)?;
        let rustos_log = fs::read_to_string(&layout.debugcon_log)?;
        let dvm_log = fs::read_to_string(&layout.dvm_serial_log)?;
        let rustos_ready = options
            .expected_markers
            .iter()
            .all(|marker| rustos_log.contains(marker));
        let dvm_ready = dvm_log.contains(DVM_READY_MARKER);
        if rustos_ready && dvm_ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let missing_rustos = options
                .expected_markers
                .iter()
                .filter(|marker| !rustos_log.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let missing_dvm = (!dvm_ready).then_some(DVM_READY_MARKER);
            bail!(
                "KVM parallel boot did not reach readiness within {:?}; RustOS missing={:?}; Linux DVM missing={:?}; inspect {}, {}, {}, and {}",
                options.timeout,
                missing_rustos,
                missing_dvm,
                layout.debugcon_log.display(),
                layout.dvm_serial_log.display(),
                layout.rustos_stderr_log.display(),
                layout.dvm_stderr_log.display(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
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
        DVM_CONTROL_STATE, DVM_CONTROL_TRANSPORT, RUSTOS_BOOT_MARKER, is_sha256,
        parse_dvm_control_contract_text, parse_manifest_text, parse_smoke_options,
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
    fn sha256_shape_is_strict() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(!is_sha256(&"g".repeat(64)));
        assert!(!is_sha256("abc"));
    }

    #[test]
    fn dvm_pretransport_contract_and_manifest_are_bound() {
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
            "schema=2\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-xz\ndata-plane=dvm-local-virtio\ncontrol-plane=agent-v1-pretransport\ncontrol-protocol=agent-v1\ncontrol-state=pretransport\ncontrol-transport=kvm-vsock-pending\ncontrol-authentication=kvm-host-bound-pending\ncontrol-capabilities=health,device-inventory\ncontrol-contract-sha256={hash}\nkernel_sha256={hash}\nrootfs_sha256={hash}\nconfig_sha256={hash}\nsources_lock_sha256={hash}\n"
        );
        let values = parse_manifest_text(&manifest, "manifest").unwrap();
        assert_eq!(validate_manifest_values(&values).unwrap(), contract);
    }

    #[test]
    fn dvm_pretransport_contract_rejects_data_plane_capability() {
        let contract_source = include_str!(
            "../../../driver-domains/linux/board/overlay/usr/share/rustos-dvm/control-plane-v1.env"
        );
        let invalid = contract_source.replace(
            "CONTROL_CAPABILITIES=health,device-inventory",
            "CONTROL_CAPABILITIES=health,network-rx",
        );
        assert!(parse_dvm_control_contract_text(&invalid, "invalid contract").is_err());
    }
}
