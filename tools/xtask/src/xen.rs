use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use fs_err as fs;

use crate::Result;
use crate::config::Config;
use crate::util::{resolve_command_path, run_command};

const DVM_NAME: &str = "rustos-linux-dvm";
const RUSTOS_NAME: &str = "rustos-hvm";
const DVM_KERNEL: &str = "rustos-linux-dvm-x86_64.bzImage";
const DVM_ROOTFS: &str = "rustos-linux-dvm-x86_64.rootfs.cpio.xz";
const DVM_CONFIG: &str = "rustos-linux-dvm-x86_64.config";
const DVM_MANIFEST: &str = "rustos-linux-dvm-x86_64.manifest";
const DVM_MANIFEST_SCHEMA: &str = "2";
const DVM_CONTROL_CONTRACT: &str = "board/overlay/usr/share/rustos-dvm/control-plane-v1.env";
const DVM_CONTROL_PROTOCOL: &str = "agent-v1";
const DVM_CONTROL_STATE: &str = "pretransport";
const DVM_CONTROL_TRANSPORT: &str = "xen-vchan-pending";
const DVM_CONTROL_AUTHENTICATION: &str = "l0-domain-bound-pending";
const DVM_CONTROL_CAPABILITIES: &str = "health,device-inventory";
const HVM_BOOT_DISK_VDEV: &str = "hda";
const HVM_BOOT_DISK_TYPE: &str = "ahci";
const HVM_XEN_DISCOVERY_MARKER: &str = "xen-hvm: discovery ready";
const HVM_XEN_HYPERCALL_MARKER: &str = "xen-hvm: hypercall page ready";
const HVM_PREINTERACTION_MARKER: &str = "rootd: core services ready, spawning initd via loaderd";
const SMOKE_SETTLE: Duration = Duration::from_secs(3);
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
struct XenLayout {
    run_dir: PathBuf,
    dvm_config: PathBuf,
    rustos_config: PathBuf,
    debugcon_log: PathBuf,
}

#[derive(Debug)]
struct SmokeOptions {
    dry_run: bool,
    timeout: Duration,
    expected_markers: Vec<String>,
}

pub(crate) fn run_xen_command<I>(config: &Config, mut args: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    if let Some(arg) = args.next() {
        bail!(
            "cargo xtask run is the production Xen path and accepts no compatibility arguments; use `cargo xtask xen-smoke --help` for an isolated lifecycle smoke ({arg:?} was supplied)"
        );
    }

    let artifacts = verify_dvm_artifacts(config)?;
    bail!(
        "refusing production Xen launch: the Linux DVM control contract is {} (transport={}, authentication={}); Xen vchan/grant/event endpoints and the authenticated RustOS-to-DVM control channel are not implemented. Use `cargo xtask xen-smoke` only for parallel domain-lifecycle diagnosis.",
        artifacts.control.control_plane(),
        artifacts.control.transport,
        artifacts.control.authentication,
    );
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

pub(crate) fn xen_smoke_command<I>(config: &Config, args: I) -> Result<()>
where
    I: Iterator<Item = String>,
{
    let args = args.collect::<Vec<_>>();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        print_xen_smoke_help();
        return Ok(());
    }
    let options = parse_smoke_options(args.into_iter())?;
    let artifacts = verify_dvm_artifacts(config)?;
    let layout = prepare_layout(config, &artifacts)?;

    if options.dry_run {
        println!(
            "xtask: Xen smoke configuration written to {}",
            layout.run_dir.display()
        );
        println!("xtask: Linux DVM config: {}", layout.dvm_config.display());
        println!(
            "xtask: RustOS HVM config: {}",
            layout.rustos_config.display()
        );
        return Ok(());
    }

    let xl = require_xl(config)?;
    ensure_xen_control_domain(&xl)?;
    ensure_domain_absent(&xl, DVM_NAME)?;
    ensure_domain_absent(&xl, RUSTOS_NAME)?;

    create_domains_in_parallel(
        &xl,
        &layout.dvm_config,
        DVM_NAME,
        &layout.rustos_config,
        RUSTOS_NAME,
    )?;
    if let Err(err) = wait_for_active_domain(&xl, DVM_NAME, SMOKE_SETTLE) {
        eprintln!("xtask: Linux DVM did not remain active; preserving Xen state for inspection");
        return Err(err);
    }
    if let Err(err) = wait_for_active_domain(&xl, RUSTOS_NAME, SMOKE_SETTLE) {
        eprintln!("xtask: RustOS HVM did not remain active; preserving Xen state for inspection");
        return Err(err);
    }

    wait_for_markers(&layout.debugcon_log, &options)?;
    println!(
        "xtask: parallel Xen lifecycle smoke passed (DVM={} HVM={}); control={} is pre-transport and does not assert a RustOS-to-DVM data plane",
        DVM_NAME,
        RUSTOS_NAME,
        artifacts.control.control_plane(),
    );
    Ok(())
}

pub(crate) fn print_xen_smoke_help() {
    println!(
        "\
usage: cargo xtask xen-smoke [options]

Creates the Linux DVM and RustOS HVM concurrently through the active Xen control
domain. This is intentionally an isolated parallel-lifecycle check, not a
production driver-domain launch: the current DVM contract is pre-transport.

options:
  --timeout <seconds>  wait for expected debugcon markers (1..={MAX_SMOKE_TIMEOUT}, default 30)
  --expect <marker>    require an additional RustOS HVM debugcon marker (repeatable)
  --dry-run            validate inputs and write xl configs without invoking xl
  -h, --help           show this help

The default proof requires the RustOS marker
`rootd: core services ready, spawning initd via loaderd`; a merely-created or
paused HVM never passes. Both domain lifecycles are checked independently before
this marker is accepted. No Xen vchan/grant/event or device data plane is implied.
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
        expected_markers: vec![
            HVM_XEN_DISCOVERY_MARKER.to_owned(),
            HVM_XEN_HYPERCALL_MARKER.to_owned(),
            HVM_PREINTERACTION_MARKER.to_owned(),
        ],
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
            unknown => bail!("unknown Xen smoke option: {unknown}"),
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

fn prepare_layout(config: &Config, artifacts: &DvmArtifacts) -> Result<XenLayout> {
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

    let run_dir = config.build_dir.join("xen");
    fs::create_dir_all(&run_dir)?;
    let runtime_disk = run_dir.join("rustos-hvm.img");
    fs::copy(&config.boot_disk_image, &runtime_disk).with_context(|| {
        format!(
            "failed to create Xen runtime disk from {}",
            config.boot_disk_image.display()
        )
    })?;

    let dvm_config = run_dir.join("linux-dvm.cfg");
    let rustos_config = run_dir.join("rustos-hvm.cfg");
    let debugcon_log = run_dir.join("rustos-debugcon.log");
    fs::write(&debugcon_log, "")?;
    fs::write(&dvm_config, render_dvm_config(artifacts)?)?;
    fs::write(
        &rustos_config,
        render_rustos_config(config, &runtime_disk, &debugcon_log)?,
    )?;

    Ok(XenLayout {
        run_dir,
        dvm_config,
        rustos_config,
        debugcon_log,
    })
}

fn render_dvm_config(artifacts: &DvmArtifacts) -> Result<String> {
    Ok(format!(
        "name = {name}\ntype = 'pv'\nmemory = 384\nvcpus = 1\nkernel = {kernel}\nramdisk = {rootfs}\nextra = 'console=hvc0'\non_poweroff = 'destroy'\non_reboot = 'destroy'\non_crash = 'destroy'\n",
        name = xl_string(DVM_NAME)?,
        kernel = xl_path(&artifacts.kernel)?,
        rootfs = xl_path(&artifacts.rootfs)?,
    ))
}

fn render_rustos_config(
    config: &Config,
    runtime_disk: &Path,
    debugcon_log: &Path,
) -> Result<String> {
    let disk_spec = render_hvm_boot_disk_spec(runtime_disk);
    let debugcon = format!("file:{}", debugcon_log.display());
    let mut device_model_args = vec![String::from("'-debugcon'"), xl_string(&debugcon)?];
    if config.project.fault_injection.enabled {
        let payload = config.project.fault_injection.rules.join(";");
        if !payload.is_empty() {
            device_model_args.push(String::from("'-fw_cfg'"));
            device_model_args.push(xl_string(&format!(
                "name=opt/rustos/fault-injection,string={payload}"
            ))?);
        }
    }
    Ok(format!(
        "name = {name}\ntype = 'hvm'\nmemory = 2048\nvcpus = 2\nfirmware = {firmware}\ndevice_model_version = 'qemu-xen'\nhdtype = '{hdtype}'\ndisk = [{disk}]\nvnc = 0\nserial = {serial}\ndevice_model_args_hvm = [{device_model_args}]\non_poweroff = 'destroy'\non_reboot = 'destroy'\non_crash = 'destroy'\n",
        name = xl_string(RUSTOS_NAME)?,
        firmware = xl_path(&config.ovmf_path)?,
        hdtype = HVM_BOOT_DISK_TYPE,
        disk = xl_string(&disk_spec)?,
        serial = xl_string(&format!(
            "file:{}",
            config.build_dir.join("xen/rustos-serial.log").display()
        ))?,
        device_model_args = device_model_args.join(", "),
    ))
}

fn render_hvm_boot_disk_spec(runtime_disk: &Path) -> String {
    format!(
        "format=raw, vdev={HVM_BOOT_DISK_VDEV}, access=rw, target={}",
        runtime_disk.display()
    )
}

fn xl_path(path: &Path) -> Result<String> {
    xl_string(
        path.to_str()
            .with_context(|| format!("non-UTF-8 Xen path is not supported: {}", path.display()))?,
    )
}

fn xl_string(value: &str) -> Result<String> {
    if value.is_empty() || value.contains(['\n', '\r', '\0']) {
        bail!("Xen config value must be non-empty and single-line");
    }
    Ok(format!(
        "'{}'",
        value.replace('\\', "\\\\").replace('\'', "\\'")
    ))
}

fn require_xl(config: &Config) -> Result<PathBuf> {
    resolve_command_path(&config.xen_xl_bin).with_context(|| {
        format!(
            "missing Xen xl toolstack command {}; run from an active Xen control domain",
            Path::new(&config.xen_xl_bin).display()
        )
    })
}

fn ensure_xen_control_domain(xl: &Path) -> Result<()> {
    let output = Command::new(xl).arg("info").output()?;
    if output.status.success() {
        return Ok(());
    }
    show_command_output(&output);
    bail!("`xl info` failed; Xen control-domain privileges are required for xen-smoke");
}

fn ensure_domain_absent(xl: &Path, name: &str) -> Result<()> {
    if domain_state(xl, name)?.is_some() {
        bail!(
            "Xen domain {name:?} already exists; inspect it or destroy it explicitly before this smoke run"
        );
    }
    Ok(())
}

fn create_domain(xl: &Path, config: &Path, name: &str) -> Result<()> {
    let output = Command::new(xl).arg("create").arg(config).output()?;
    if output.status.success() {
        return Ok(());
    }
    show_command_output(&output);
    bail!(
        "failed to create Xen domain {name:?} from {}",
        config.display()
    );
}

fn create_domains_in_parallel(
    xl: &Path,
    first_config: &Path,
    first_name: &str,
    second_config: &Path,
    second_name: &str,
) -> Result<()> {
    let first_xl = xl.to_path_buf();
    let first_config = first_config.to_path_buf();
    let first_name = first_name.to_owned();
    let first_label = first_name.clone();
    let first = thread::spawn(move || create_domain(&first_xl, &first_config, &first_name));

    let second_xl = xl.to_path_buf();
    let second_config = second_config.to_path_buf();
    let second_name = second_name.to_owned();
    let second_label = second_name.clone();
    let second = thread::spawn(move || create_domain(&second_xl, &second_config, &second_name));

    let first_result = first
        .join()
        .map_err(|_| anyhow!("parallel Xen create thread panicked for {first_label:?}"))?;
    let second_result = second
        .join()
        .map_err(|_| anyhow!("parallel Xen create thread panicked for {second_label:?}"))?;
    match (first_result, second_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(first), Ok(())) => Err(first.context("first parallel Xen domain creation failed")),
        (Ok(()), Err(second)) => Err(second.context("second parallel Xen domain creation failed")),
        (Err(first), Err(second)) => {
            bail!("both parallel Xen domain creations failed; first={first:#}; second={second:#}")
        }
    }
}

fn wait_for_active_domain(xl: &Path, name: &str, wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let state = match domain_state(xl, name)? {
            Some(state) if state.contains('r') || state.contains('b') => state,
            Some(state) => bail!("Xen domain {name:?} became inactive with state {state:?}"),
            None => bail!("Xen domain {name:?} disappeared before becoming active"),
        };
        if Instant::now() >= deadline {
            println!("xtask: Xen domain {name} remained active with state {state:?}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn domain_state(xl: &Path, name: &str) -> Result<Option<String>> {
    let output = Command::new(xl).arg("list").arg(name).output()?;
    if !output.status.success() {
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if diagnostic.contains("does not exist") || diagnostic.contains("not found") {
            return Ok(None);
        }
        show_command_output(&output);
        bail!("`xl list {name}` failed while checking Xen domain state");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().skip(1) {
        let mut columns = line.split_whitespace();
        if columns.next() == Some(name) {
            let _id = columns.next();
            let _memory = columns.next();
            let _vcpus = columns.next();
            return columns
                .next()
                .map(str::to_owned)
                .context("xl list returned a domain row without state")
                .map(Some);
        }
    }
    Ok(None)
}

fn wait_for_markers(debugcon_log: &Path, options: &SmokeOptions) -> Result<()> {
    if options.expected_markers.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + options.timeout;
    loop {
        let text = match fs::read_to_string(debugcon_log) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        if options
            .expected_markers
            .iter()
            .all(|marker| text.contains(marker))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let missing = options
                .expected_markers
                .iter()
                .filter(|marker| !text.contains(marker.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            bail!(
                "RustOS HVM did not produce expected debugcon marker(s) within {:?}: {}; inspect {}",
                options.timeout,
                missing.join(", "),
                debugcon_log.display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn show_command_output(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        eprint!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DVM_CONTROL_AUTHENTICATION, DVM_CONTROL_CAPABILITIES, DVM_CONTROL_PROTOCOL,
        DVM_CONTROL_STATE, DVM_CONTROL_TRANSPORT, HVM_BOOT_DISK_TYPE, HVM_BOOT_DISK_VDEV,
        HVM_PREINTERACTION_MARKER, HVM_XEN_DISCOVERY_MARKER, HVM_XEN_HYPERCALL_MARKER, is_sha256,
        parse_dvm_control_contract_text, parse_manifest_text, parse_smoke_options,
        validate_manifest_values, xl_string,
    };
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn xen_strings_escape_python_quotes() {
        assert_eq!(xl_string("a'b\\c").unwrap(), "'a\\'b\\\\c'");
    }

    #[test]
    fn smoke_timeout_is_bounded() {
        let options =
            parse_smoke_options(vec!["--timeout".into(), "30".into()].into_iter()).unwrap();
        assert_eq!(options.timeout.as_secs(), 30);
        assert_eq!(
            options.expected_markers,
            vec![
                HVM_XEN_DISCOVERY_MARKER.to_owned(),
                HVM_XEN_HYPERCALL_MARKER.to_owned(),
                HVM_PREINTERACTION_MARKER.to_owned(),
            ]
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
    fn hvm_boot_storage_contract_uses_emulated_ahci_not_xen_pv_block() {
        assert_eq!(HVM_BOOT_DISK_TYPE, "ahci");
        assert_eq!(HVM_BOOT_DISK_VDEV, "hda");
        assert_ne!(HVM_BOOT_DISK_VDEV, "xvda");
        assert_eq!(
            super::render_hvm_boot_disk_spec(Path::new("/var/lib/rustos.img")),
            "format=raw, vdev=hda, access=rw, target=/var/lib/rustos.img"
        );
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
            "schema=2\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-xz\ndata-plane=dvm-local-virtio\ncontrol-plane=agent-v1-pretransport\ncontrol-protocol=agent-v1\ncontrol-state=pretransport\ncontrol-transport=xen-vchan-pending\ncontrol-authentication=l0-domain-bound-pending\ncontrol-capabilities=health,device-inventory\ncontrol-contract-sha256={hash}\nkernel_sha256={hash}\nrootfs_sha256={hash}\nconfig_sha256={hash}\nsources_lock_sha256={hash}\n"
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

    #[cfg(unix)]
    #[test]
    fn parallel_domain_create_does_not_serially_wait_for_each_xl_invocation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let xl = temp.path().join("xl");
        std::fs::write(&xl, "#!/bin/sh\nsleep 1\n").unwrap();
        std::fs::set_permissions(&xl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let first_config = temp.path().join("dvm.cfg");
        let second_config = temp.path().join("hvm.cfg");
        std::fs::write(&first_config, "").unwrap();
        std::fs::write(&second_config, "").unwrap();

        let started = Instant::now();
        super::create_domains_in_parallel(
            &xl,
            &first_config,
            "dvm-test",
            &second_config,
            "hvm-test",
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1_800),
            "parallel create took {:?}",
            started.elapsed()
        );
    }
}
