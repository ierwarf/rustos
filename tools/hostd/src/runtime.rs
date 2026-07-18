use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rustos_driver_domain_host::{
    ControlContract, ControlSecret, DeviceClass, DeviceTransport, DriverDomainPolicy,
    HostControlListener, PhysicalDisplayPolicy, SysfsVfioOps, ValidatedLease, VfioLeaseRecord,
    VfioLeaseState, reset_vfio_group, restore_vfio_lease, validate_host_display_assignment,
    validate_physical_display_assignment, validate_physical_display_identity,
};
use sha2::{Digest, Sha256};

const DVM_MANIFEST_SCHEMA: &str = "8";
const DVM_KERNEL: &str = "rustos-linux-dvm-x86_64.bzImage";
const DVM_ROOTFS: &str = "rustos-linux-dvm-x86_64.rootfs.cpio.xz";
const DVM_CONFIG: &str = "rustos-linux-dvm-x86_64.config";
const DVM_KERNEL_CONFIG: &str = "rustos-linux-dvm-x86_64.kernel.config";
const DVM_MODULE_SIGNING_CERT: &str = "rustos-linux-dvm-x86_64.module-signing.x509";
const DVM_SOURCES_LOCK: &str = "rustos-linux-dvm-x86_64.sources.lock";
const DVM_CONTROL_CONTRACT: &str = "rustos-linux-dvm-x86_64.control.env";
const DVM_PIXEL_BYTES: u64 = 32 * 1024 * 1024;
const DVM_PIXEL_PHYS: u64 = 0x1_0000_0000;
const TERMINATE_GRACE: Duration = Duration::from_secs(5);
const SUPERVISOR_TICK: Duration = Duration::from_millis(100);
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const IOMMU_MODE: &str = "iommufd-vfio-nonidentity";
const IOMMUFD_IOCTL_TYPE: libc::c_ulong = b';' as libc::c_ulong;
const IOMMUFD_CMD_DESTROY: libc::c_ulong = 0x80;
const IOMMUFD_CMD_IOAS_ALLOC: libc::c_ulong = 0x81;
const IOMMU_DESTROY: libc::c_ulong = (IOMMUFD_IOCTL_TYPE << 8) | IOMMUFD_CMD_DESTROY;
const IOMMU_IOAS_ALLOC: libc::c_ulong = (IOMMUFD_IOCTL_TYPE << 8) | IOMMUFD_CMD_IOAS_ALLOC;

#[repr(C)]
struct IommuIoasAlloc {
    size: u32,
    flags: u32,
    out_ioas_id: u32,
}

#[repr(C)]
struct IommuDestroy {
    size: u32,
    id: u32,
}

static TERMINATE_REQUESTED: AtomicBool = AtomicBool::new(false);
static PRIVATE_REPLACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

extern "C" fn request_termination(_signal: libc::c_int) {
    TERMINATE_REQUESTED.store(true, Ordering::Release);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeState {
    Starting,
    Ready,
    Stopping,
}

impl RuntimeState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "starting" => Ok(Self::Starting),
            "ready" => Ok(Self::Ready),
            "stopping" => Ok(Self::Stopping),
            _ => bail!("invalid DVM runtime state {value:?}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRecord {
    pub state: RuntimeState,
    pub domain_id: String,
    pub dvm_guest_cid: u32,
    pub iommu_group: u32,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub launched_at_unix: u64,
    pub qemu_sha256: String,
    pub artifact_manifest_sha256: String,
    pub device_policy_sha256: String,
    pub release_manifest_sha256: String,
}

impl RuntimeRecord {
    fn encode(&self) -> String {
        format!(
            "DVM_RUNTIME_SCHEMA=2\nRUNTIME_STATE={}\nDOMAIN_ID={}\nDVM_GUEST_CID={}\nIOMMU_GROUP={}\nIOMMU_MODE={IOMMU_MODE}\nPID={}\nPROCESS_START_TICKS={}\nLAUNCHED_AT_UNIX={}\nQEMU_SHA256={}\nDVM_ARTIFACT_MANIFEST_SHA256={}\nDEVICE_POLICY_SHA256={}\nRELEASE_MANIFEST_SHA256={}\n",
            self.state.as_str(),
            self.domain_id,
            self.dvm_guest_cid,
            self.iommu_group,
            self.pid,
            self.process_start_ticks,
            self.launched_at_unix,
            self.qemu_sha256,
            self.artifact_manifest_sha256,
            self.device_policy_sha256,
            self.release_manifest_sha256,
        )
    }

    fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_env(source, label)?;
        const REQUIRED: [&str; 13] = [
            "DVM_RUNTIME_SCHEMA",
            "RUNTIME_STATE",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "IOMMU_MODE",
            "PID",
            "PROCESS_START_TICKS",
            "LAUNCHED_AT_UNIX",
            "QEMU_SHA256",
            "DVM_ARTIFACT_MANIFEST_SHA256",
            "DEVICE_POLICY_SHA256",
            "RELEASE_MANIFEST_SHA256",
        ];
        if values.len() != REQUIRED.len()
            || values.keys().any(|key| !REQUIRED.contains(&key.as_str()))
            || value(&values, "DVM_RUNTIME_SCHEMA", label)? != "2"
            || value(&values, "IOMMU_MODE", label)? != IOMMU_MODE
        {
            bail!("unsupported DVM runtime record {label}");
        }
        let domain_id = value(&values, "DOMAIN_ID", label)?.to_owned();
        validate_domain_id(&domain_id)?;
        let dvm_guest_cid = value(&values, "DVM_GUEST_CID", label)?.parse()?;
        if dvm_guest_cid <= 2 {
            bail!("invalid DVM runtime CID");
        }
        let pid = value(&values, "PID", label)?.parse()?;
        let process_start_ticks = value(&values, "PROCESS_START_TICKS", label)?.parse()?;
        if pid == 0 || process_start_ticks == 0 {
            bail!("invalid DVM runtime process identity");
        }
        Ok(Self {
            state: RuntimeState::parse(value(&values, "RUNTIME_STATE", label)?)?,
            domain_id,
            dvm_guest_cid,
            iommu_group: value(&values, "IOMMU_GROUP", label)?.parse()?,
            pid,
            process_start_ticks,
            launched_at_unix: value(&values, "LAUNCHED_AT_UNIX", label)?.parse()?,
            qemu_sha256: parse_sha256(value(&values, "QEMU_SHA256", label)?)?,
            artifact_manifest_sha256: parse_sha256(value(
                &values,
                "DVM_ARTIFACT_MANIFEST_SHA256",
                label,
            )?)?,
            device_policy_sha256: parse_sha256(value(&values, "DEVICE_POLICY_SHA256", label)?)?,
            release_manifest_sha256: parse_sha256(value(
                &values,
                "RELEASE_MANIFEST_SHA256",
                label,
            )?)?,
        })
    }

    pub fn matches(&self, lease: &VfioLeaseRecord) -> bool {
        self.domain_id == lease.domain_id
            && self.dvm_guest_cid == lease.dvm_guest_cid
            && self.iommu_group == lease.iommu_group
            && lease
                .dvm_artifact_manifest_sha256()
                .is_ok_and(|digest| digest == self.artifact_manifest_sha256)
            && lease
                .device_policy_sha256()
                .is_ok_and(|digest| digest == self.device_policy_sha256)
            && lease
                .release_manifest_sha256()
                .is_ok_and(|digest| digest == self.release_manifest_sha256)
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn domain_dir(&self, domain_id: &str) -> Result<PathBuf> {
        validate_domain_id(domain_id)?;
        Ok(self.root.join(domain_id))
    }

    pub fn prepare_domain_dir(&self, domain_id: &str) -> Result<PathBuf> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create DVM runtime root {}", self.root.display()))?;
        let root_metadata = fs::symlink_metadata(&self.root)?;
        if !root_metadata.file_type().is_dir() || root_metadata.uid() != unsafe { libc::geteuid() }
        {
            bail!("unsafe DVM runtime root {}", self.root.display());
        }
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        ensure_trusted_directory(&self.root, unsafe { libc::geteuid() }, true)?;
        let directory = self.domain_dir(domain_id)?;
        fs::create_dir_all(&directory)
            .with_context(|| format!("create DVM runtime directory {}", directory.display()))?;
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("unsafe DVM runtime directory {}", directory.display());
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        ensure_trusted_directory(&directory, unsafe { libc::geteuid() }, true)?;
        sync_directory(&directory)?;
        Ok(directory)
    }

    pub fn write_secret(&self, domain_id: &str, secret: &ControlSecret) -> Result<PathBuf> {
        let directory = self.prepare_domain_dir(domain_id)?;
        let path = directory.join("control-secret.hex");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("create DVM control secret {}", path.display()))?;
        file.write_all(secret.as_hex().as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        sync_directory(&directory)?;
        Ok(path)
    }

    pub fn write_record(&self, record: &RuntimeRecord) -> Result<()> {
        let directory = self.prepare_domain_dir(&record.domain_id)?;
        replace_private(&directory.join("runtime.env"), &record.encode())?;
        sync_directory(&directory)
    }

    pub fn load(&self, domain_id: &str) -> Result<RuntimeRecord> {
        let directory = self.domain_dir(domain_id)?;
        ensure_trusted_directory(&self.root, unsafe { libc::geteuid() }, true)?;
        ensure_trusted_directory(&directory, unsafe { libc::geteuid() }, true)?;
        let path = directory.join("runtime.env");
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect DVM runtime record {}", path.display()))?;
        if !metadata.file_type().is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            bail!("unsafe DVM runtime record {}", path.display());
        }
        let source = fs::read_to_string(&path)?;
        RuntimeRecord::parse(&source, &path.display().to_string())
    }

    pub fn remove(&self, domain_id: &str) -> Result<()> {
        let directory = self.domain_dir(domain_id)?;
        match fs::symlink_metadata(&self.root) {
            Ok(_) => ensure_trusted_directory(&self.root, unsafe { libc::geteuid() }, true)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect DVM runtime root"),
        }
        match fs::symlink_metadata(&directory) {
            Ok(_) => ensure_trusted_directory(&directory, unsafe { libc::geteuid() }, true)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("inspect DVM runtime directory"),
        }
        match fs::remove_dir_all(&directory) {
            Ok(()) => sync_directory(&self.root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("remove DVM runtime directory {}", directory.display())),
        }
    }
}

#[derive(Debug)]
pub struct VerifiedArtifacts {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub control_contract: PathBuf,
    pub control: ControlContract,
}

pub fn verify_artifacts(manifest: &Path) -> Result<VerifiedArtifacts> {
    let owner = unsafe { libc::geteuid() };
    let manifest = trusted_canonical_regular_file(manifest, owner)?;
    let source = fs::read_to_string(&manifest)
        .with_context(|| format!("read DVM artifact manifest {}", manifest.display()))?;
    let values = parse_manifest(&source, &manifest.display().to_string())?;
    const REQUIRED: [&str; 24] = [
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
        "kernel_sha256",
        "rootfs_sha256",
        "config_sha256",
        "kernel-config-sha256",
        "nvidia-open-version",
        "nvidia-open-sha256",
        "nvidia-open-redistribute",
        "display-kernel-modules",
        "module-signing-enforced",
        "module-signing-cert-sha256",
    ];
    // sources_lock_sha256 is also mandatory but kept separate so the exact
    // schema count is explicit and accidental extra keys still fail closed.
    if values.len() != REQUIRED.len() + 1
        || values
            .keys()
            .any(|key| !REQUIRED.contains(&key.as_str()) && key.as_str() != "sources_lock_sha256")
        || manifest_value(&values, "schema")? != DVM_MANIFEST_SCHEMA
        || manifest_value(&values, "id")? != "rustos-linux-dvm-x86_64"
        || manifest_value(&values, "architecture")? != "x86_64"
        || manifest_value(&values, "boot")? != "linux-bzimage+cpio-xz"
        || manifest_value(&values, "data-plane")? != "hostd-input-ring-msix"
        || manifest_value(&values, "control-plane")? != "agent-v1-control"
        || manifest_value(&values, "control-protocol")? != "agent-v1"
        || manifest_value(&values, "control-state")? != "control"
        || manifest_value(&values, "control-transport")? != "kvm-vsock"
        || manifest_value(&values, "control-authentication")? != "dvm-agent-hmac-sha256-v1"
        || manifest_value(&values, "control-capabilities")?
            != "health,device-inventory,driver-inventory,display-evidence-v1,input-stream"
        || manifest_value(&values, "buildroot_version")? != "2026.05"
        || manifest_value(&values, "linux_version")? != "6.12.94"
        || manifest_value(&values, "nvidia-open-version")? != "580.173.02"
        || manifest_value(&values, "nvidia-open-sha256")?
            != "8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b"
        || manifest_value(&values, "nvidia-open-redistribute")? != "no"
        || manifest_value(&values, "display-kernel-modules")? != "i915,xe,amdgpu,nvidia-drm"
        || manifest_value(&values, "module-signing-enforced")? != "yes"
    {
        bail!("unsupported DVM artifact manifest {}", manifest.display());
    }
    for key in [
        "control-contract-sha256",
        "kernel_sha256",
        "rootfs_sha256",
        "config_sha256",
        "kernel-config-sha256",
        "sources_lock_sha256",
        "nvidia-open-sha256",
        "module-signing-cert-sha256",
    ] {
        parse_sha256(manifest_value(&values, key)?)?;
    }
    let directory = manifest
        .parent()
        .ok_or_else(|| anyhow!("DVM artifact manifest has no parent directory"))?;
    let kernel = trusted_canonical_regular_file(&directory.join(DVM_KERNEL), owner)?;
    let rootfs = trusted_canonical_regular_file(&directory.join(DVM_ROOTFS), owner)?;
    let module_signing_cert =
        trusted_canonical_regular_file(&directory.join(DVM_MODULE_SIGNING_CERT), owner)?;
    let buildroot_config = trusted_canonical_regular_file(&directory.join(DVM_CONFIG), owner)?;
    let kernel_config = trusted_canonical_regular_file(&directory.join(DVM_KERNEL_CONFIG), owner)?;
    let sources_lock = trusted_canonical_regular_file(&directory.join(DVM_SOURCES_LOCK), owner)?;
    let control_contract =
        trusted_canonical_regular_file(&directory.join(DVM_CONTROL_CONTRACT), owner)?;
    verify_sha256_file(&kernel, manifest_value(&values, "kernel_sha256")?)?;
    verify_sha256_file(&rootfs, manifest_value(&values, "rootfs_sha256")?)?;
    verify_sha256_file(
        &module_signing_cert,
        manifest_value(&values, "module-signing-cert-sha256")?,
    )?;
    verify_sha256_file(&buildroot_config, manifest_value(&values, "config_sha256")?)?;
    verify_sha256_file(
        &kernel_config,
        manifest_value(&values, "kernel-config-sha256")?,
    )?;
    validate_signed_module_kernel_config(&kernel_config)?;
    verify_sha256_file(
        &sources_lock,
        manifest_value(&values, "sources_lock_sha256")?,
    )?;
    verify_sha256_file(
        &control_contract,
        manifest_value(&values, "control-contract-sha256")?,
    )?;
    let control = ControlContract::from_env_file(&control_contract)?;
    if manifest_value(&values, "control-protocol")? != control.protocol
        || manifest_value(&values, "control-state")? != control.state
        || manifest_value(&values, "control-transport")? != control.transport
        || manifest_value(&values, "control-authentication")? != control.authentication
        || manifest_value(&values, "control-capabilities")? != control.capabilities.join(",")
        || manifest_value(&values, "control-plane")?
            != format!("{}-{}", control.protocol, control.state)
    {
        bail!(
            "DVM manifest and packaged control contract disagree in {}",
            manifest.display()
        );
    }
    Ok(VerifiedArtifacts {
        kernel,
        rootfs,
        control_contract,
        control,
    })
}

/// Complete every reversible physical-runtime admission check before an
/// authorized IOMMU group is detached from its host drivers.
pub fn preflight_physical_runtime_inputs(
    lease: &ValidatedLease,
    sysfs_root: &Path,
    qemu: &Path,
    artifact_manifest: &Path,
    device_policy: &Path,
) -> Result<()> {
    let owner = unsafe { libc::geteuid() };
    let device_policy = trusted_canonical_regular_file(device_policy, owner)?;
    let policy = DriverDomainPolicy::from_env_file(&device_policy)?;
    policy.validate_for_lease(lease)?;
    if policy.transport_for(DeviceClass::Display) != DeviceTransport::DisplayDmaBufKms
        || policy.transport_for(DeviceClass::Network) != DeviceTransport::Disabled
        || policy.transport_for(DeviceClass::Block) != DeviceTransport::Disabled
    {
        bail!("physical runtime preflight requires display-dmabuf-kms with network/block disabled");
    }
    let physical_display = policy
        .physical_display()
        .ok_or_else(|| anyhow!("physical runtime preflight requires device policy schema 3"))?;
    validate_host_display_assignment(lease, sysfs_root)?;
    validate_physical_display_assignment(lease, sysfs_root, physical_display)?;
    let (_, qemu_sha256) = verify_qemu(qemu)?;
    if qemu_sha256 != policy.qemu_sha256() {
        bail!("physical runtime QEMU digest does not match the signed device policy");
    }
    verify_artifacts(artifact_manifest)?;
    verify_iommufd()?;
    Ok(())
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

pub struct SuperviseConfig<'a> {
    pub qemu: &'a Path,
    pub artifact_manifest: &'a Path,
    pub device_policy: &'a Path,
    pub display_doorbell: &'a Path,
    pub display_pixels: &'a Path,
    pub setup_timeout: Duration,
}

pub fn supervise_domain(
    lease: &mut VfioLeaseRecord,
    sysfs_root: &Path,
    store: &RuntimeStore,
    config: SuperviseConfig<'_>,
) -> Result<ExitStatus> {
    if lease.state != VfioLeaseState::Active {
        bail!("DVM supervision requires an active VFIO lease");
    }
    lease.authorization_valid_at(current_unix_time()?)?;
    let owner = unsafe { libc::geteuid() };
    let artifact_manifest = trusted_canonical_regular_file(config.artifact_manifest, owner)?;
    let device_policy = trusted_canonical_regular_file(config.device_policy, owner)?;
    verify_sha256_file(&artifact_manifest, lease.dvm_artifact_manifest_sha256()?)?;
    verify_sha256_file(&device_policy, lease.device_policy_sha256()?)?;
    let policy = DriverDomainPolicy::from_env_file(&device_policy)?;
    let validated = ValidatedLease {
        domain_id: lease.domain_id.clone(),
        dvm_guest_cid: lease.dvm_guest_cid,
        iommu_group: lease.iommu_group,
        pci_bdfs: lease.original_drivers.keys().cloned().collect(),
    };
    policy.validate_for_lease(&validated)?;
    if policy.transport_for(DeviceClass::Display) != DeviceTransport::DisplayDmaBufKms
        || policy.transport_for(DeviceClass::Network) != DeviceTransport::Disabled
        || policy.transport_for(DeviceClass::Block) != DeviceTransport::Disabled
    {
        bail!("supervised physical DVM requires display-dmabuf-kms with network/block disabled");
    }
    let physical_display = policy
        .physical_display()
        .ok_or_else(|| anyhow!("supervised physical DVM requires device policy schema 3"))?
        .clone();
    let display_bdf =
        validate_physical_display_identity(&validated, sysfs_root, &physical_display)?;
    if lease
        .original_drivers
        .get(&display_bdf)
        .and_then(Option::as_deref)
        != Some(physical_display.driver())
    {
        bail!("durable VFIO lease does not retain the signed physical display driver");
    }
    ensure_private_regular_file(config.display_pixels)?;
    ensure_private_socket(config.display_doorbell)?;
    let (qemu, qemu_sha256) = verify_qemu(config.qemu)?;
    if qemu_sha256 != policy.qemu_sha256() {
        bail!("production QEMU digest does not match the signed device policy");
    }
    verify_iommufd()?;
    let artifacts = verify_artifacts(&artifact_manifest)?;
    let contract = artifacts.control.clone();
    let secret = ControlSecret::random()?;
    let secret_path = store.write_secret(&lease.domain_id, &secret)?;
    let listener = HostControlListener::bind(lease.dvm_guest_cid, contract, secret)?;

    let mut ops = SysfsVfioOps::new(sysfs_root);
    reset_vfio_group(lease, &mut ops)?;
    install_signal_handlers()?;
    TERMINATE_REQUESTED.store(false, Ordering::Release);
    let runtime_dir = store.prepare_domain_dir(&lease.domain_id)?;
    let (mut child, mut launch_gate) = match spawn_qemu_gated(
        &qemu,
        lease,
        &artifacts,
        &secret_path,
        config.display_doorbell,
        config.display_pixels,
        &runtime_dir,
    ) {
        Ok(child) => child,
        Err(start_error) => {
            let restore = restore_vfio_lease(lease, &mut ops);
            let _ = store.remove(&lease.domain_id);
            return match restore {
                Ok(()) => Err(start_error),
                Err(restore_error) => Err(start_error).context(format!(
                    "DVM start failed and VFIO recovery failed: {restore_error:#}"
                )),
            };
        }
    };
    let run_result = (|| {
        let process_start_ticks = wait_for_process_start(child.id(), config.setup_timeout)?;
        let mut record = RuntimeRecord {
            state: RuntimeState::Starting,
            domain_id: lease.domain_id.clone(),
            dvm_guest_cid: lease.dvm_guest_cid,
            iommu_group: lease.iommu_group,
            pid: child.id(),
            process_start_ticks,
            launched_at_unix: current_unix_time()?,
            qemu_sha256,
            artifact_manifest_sha256: lease.dvm_artifact_manifest_sha256()?.to_owned(),
            device_policy_sha256: lease.device_policy_sha256()?.to_owned(),
            release_manifest_sha256: lease.release_manifest_sha256()?.to_owned(),
        };
        store.write_record(&record)?;
        launch_gate
            .write_all(&[1])
            .context("release durably recorded DVM launch gate")?;
        drop(launch_gate);
        let display_sample_sequence =
            wait_for_display_readiness(&listener, config.setup_timeout, &physical_display)?;
        record.state = RuntimeState::Ready;
        store.write_record(&record)?;
        let status = monitor_child(
            &mut child,
            &mut record,
            store,
            &listener,
            &physical_display,
            display_sample_sequence,
        )?;
        require_successful_child_exit(status)
    })();
    let stop_error = if run_result.is_err() {
        stop_child(&mut child).err()
    } else {
        None
    };
    let restore_result = restore_vfio_lease(lease, &mut ops);
    if restore_result.is_ok() {
        store.remove(&lease.domain_id)?;
    }
    match (run_result, stop_error, restore_result) {
        (Ok(status), None, Ok(())) => Ok(status),
        (Ok(status), _, Err(restore_error)) => Err(restore_error).context(format!(
            "DVM exited with {status}, but VFIO reset/restore failed; runtime record retained"
        )),
        (Err(run_error), None, Ok(())) => Err(run_error),
        (Err(run_error), Some(stop_error), Ok(())) => Err(run_error).context(format!(
            "DVM supervision failed and bounded stop also failed: {stop_error:#}"
        )),
        (Err(run_error), stop_error, Err(restore_error)) => Err(run_error).context(format!(
            "DVM supervision failed; stop={}; VFIO recovery failed: {restore_error:#}",
            stop_error
                .map(|error| format!("failed: {error:#}"))
                .unwrap_or_else(|| "complete".to_owned())
        )),
        (Ok(_), Some(_), Ok(())) => unreachable!("successful run has no stop error"),
    }
}

fn require_successful_child_exit(status: ExitStatus) -> Result<ExitStatus> {
    if !status.success() {
        bail!("supervised DVM exited unsuccessfully: {status}");
    }
    Ok(status)
}

pub fn recover_domain(
    lease: &VfioLeaseRecord,
    sysfs_root: &Path,
    store: &RuntimeStore,
) -> Result<()> {
    if lease.state != VfioLeaseState::Active {
        bail!("DVM recovery requires an active VFIO lease");
    }
    match store.load(&lease.domain_id) {
        Ok(record) => {
            if !record.matches(lease) {
                bail!("DVM runtime record does not match durable VFIO lease");
            }
            terminate_recovered_process(record.pid, record.process_start_ticks)?;
        }
        Err(error) if is_missing_runtime(&error) => {}
        Err(error) => return Err(error),
    }
    let mut ops = SysfsVfioOps::new(sysfs_root);
    restore_vfio_lease(lease, &mut ops)?;
    store.remove(&lease.domain_id)
}

fn spawn_qemu_gated(
    qemu: &Path,
    lease: &VfioLeaseRecord,
    artifacts: &VerifiedArtifacts,
    secret: &Path,
    display_doorbell: &Path,
    display_pixels: &Path,
    runtime_dir: &Path,
) -> Result<(Child, UnixStream)> {
    let serial = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(runtime_dir.join("serial.log"))?;
    let stderr = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(runtime_dir.join("qemu.stderr.log"))?;
    let mut command = Command::new(qemu);
    let (parent_gate, child_gate) = UnixStream::pair().context("create DVM launch gate")?;
    let parent_gate_fd = parent_gate.as_raw_fd();
    let child_gate_fd = child_gate.as_raw_fd();
    command
        .arg("-name")
        .arg(format!("rustos-dvm-{}", lease.domain_id))
        .args(["-machine", "q35,accel=kvm", "-cpu", "host", "-m", "1024M", "-smp", "2"])
        .args(["-object", "iommufd,id=iommufd0"])
        .arg("-kernel")
        .arg(&artifacts.kernel)
        .arg("-initrd")
        .arg(&artifacts.rootfs)
        .args([
            "-append",
            "console=ttyS0 preempt=full",
            "-display",
            "none",
            "-vga",
            "none",
            "-no-reboot",
            "-nodefaults",
        ])
        .arg("-serial")
        .arg("stdio")
        .arg("-device")
        .arg(format!("vhost-vsock-pci,guest-cid={}", lease.dvm_guest_cid))
        .arg("-fw_cfg")
        .arg(format!(
            "name=opt/rustos/dvm-control-secret,file={}",
            secret.display()
        ))
        .arg("-chardev")
        .arg(format!(
            "socket,id=dvm-display-doorbell,path={}",
            display_doorbell.display()
        ))
        .args([
            "-device",
            "ivshmem-doorbell,vectors=2,chardev=dvm-display-doorbell",
        ])
        .arg("-object")
        .arg(format!(
            "memory-backend-file,id=dvm-display-pixels,mem-path={},size={DVM_PIXEL_BYTES},share=on,readonly=on,rom=on",
            display_pixels.display()
        ))
        .arg("-device")
        .arg(format!(
            "virtio-pmem-pci,id=dvm-display-pmem,memdev=dvm-display-pixels,memaddr={DVM_PIXEL_PHYS}"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::from(serial))
        .stderr(Stdio::from(stderr));
    for bdf in lease.original_drivers.keys() {
        command
            .arg("-device")
            .arg(format!("vfio-pci,host={bdf},iommufd=iommufd0"));
    }
    // The child cannot exec QEMU (and therefore cannot open VFIO) until the
    // parent has fsync'd the exact PID/start-time runtime record. If hostd
    // crashes first, closing the peer makes pre_exec fail closed.
    unsafe {
        command.pre_exec(move || {
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(signal, libc::SIG_DFL);
            }
            libc::close(parent_gate_fd);
            let mut byte = 0_u8;
            loop {
                let read = libc::read(child_gate_fd, (&mut byte as *mut u8).cast(), 1);
                if read == 1 {
                    libc::close(child_gate_fd);
                    return Ok(());
                }
                if read == 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EPIPE));
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(error);
                }
            }
        });
    }
    let child = command
        .spawn()
        .context("start gated supervised Linux DVM")?;
    drop(child_gate);
    Ok((child, parent_gate))
}

fn monitor_child(
    child: &mut Child,
    record: &mut RuntimeRecord,
    store: &RuntimeStore,
    listener: &HostControlListener,
    display_policy: &PhysicalDisplayPolicy,
    mut display_sample_sequence: u64,
) -> Result<ExitStatus> {
    let mut next_health = Instant::now() + HEALTH_INTERVAL;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if TERMINATE_REQUESTED.load(Ordering::Acquire) {
            record.state = RuntimeState::Stopping;
            store.write_record(record)?;
            stop_child(child)?;
            return child.wait().context("wait for stopped DVM");
        }
        if Instant::now() >= next_health {
            let probe = listener
                .probe_once(HEALTH_TIMEOUT)
                .context("DVM lost authenticated health")?;
            if !probe.driver_inventory.display_driver_bound
                || !probe.driver_inventory.display_relay_ready
            {
                bail!("DVM lost direct display driver/relay readiness");
            }
            let evidence = probe
                .display_evidence
                .as_ref()
                .ok_or_else(|| anyhow!("DVM lost physical display evidence"))?;
            display_policy.validate_evidence(evidence)?;
            if evidence.sample_sequence <= display_sample_sequence {
                bail!(
                    "DVM physical display evidence stopped advancing sequence={} previous={}",
                    evidence.sample_sequence,
                    display_sample_sequence
                );
            }
            display_sample_sequence = evidence.sample_sequence;
            next_health = Instant::now() + HEALTH_INTERVAL;
        }
        std::thread::sleep(SUPERVISOR_TICK);
    }
}

fn wait_for_display_readiness(
    listener: &HostControlListener,
    timeout: Duration,
    display_policy: &PhysicalDisplayPolicy,
) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    let mut last_reason = "no authenticated DVM probe".to_owned();
    let mut last_sequence = 0_u64;
    let mut consecutive_samples = 0_u32;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt_timeout = remaining.min(HEALTH_TIMEOUT);
        match listener.probe_once(attempt_timeout) {
            Ok(probe)
                if probe.driver_inventory.display_driver_bound
                    && probe.driver_inventory.display_relay_ready =>
            {
                match probe.display_evidence.as_ref() {
                    Some(evidence) => match display_policy.validate_evidence(evidence) {
                        Ok(()) if evidence.sample_sequence == last_sequence => {}
                        Ok(()) => {
                            consecutive_samples = if last_sequence == 0
                                || last_sequence.checked_add(1) == Some(evidence.sample_sequence)
                            {
                                consecutive_samples + 1
                            } else {
                                1
                            };
                            last_sequence = evidence.sample_sequence;
                            if consecutive_samples >= display_policy.required_consecutive_samples()
                            {
                                return Ok(last_sequence);
                            }
                        }
                        Err(error) => {
                            consecutive_samples = 0;
                            last_reason = format!("{error:#}");
                        }
                    },
                    None => {
                        consecutive_samples = 0;
                        last_reason = "physical display evidence unavailable".to_owned();
                    }
                }
            }
            Ok(_) => {
                consecutive_samples = 0;
                last_reason = "display driver or direct relay not ready".to_owned();
            }
            Err(error) => {
                consecutive_samples = 0;
                last_reason = format!("{error:#}");
            }
        }
        std::thread::sleep(SUPERVISOR_TICK);
    }
    bail!(
        "DVM failed authenticated physical-display readiness after {consecutive_samples} consecutive samples: {last_reason}"
    )
}

fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    terminate_process(child.id())?;
    let deadline = Instant::now() + TERMINATE_GRACE;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(SUPERVISOR_TICK);
    }
    child.kill().context("SIGKILL supervised DVM")?;
    child.wait().context("reap supervised DVM")?;
    Ok(())
}

fn install_signal_handlers() -> Result<()> {
    let handler = request_termination as *const () as usize;
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        if unsafe { libc::signal(signal, handler) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error()).context("install DVM supervisor signal");
        }
    }
    Ok(())
}

fn terminate_process(pid: u32) -> Result<()> {
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context(format!("SIGTERM DVM pid {pid}"));
        }
    }
    Ok(())
}

fn terminate_recovered_process(pid: u32, expected_start_ticks: u64) -> Result<()> {
    match process_start_ticks(pid) {
        Ok(actual) if actual != expected_start_ticks => return Ok(()),
        Ok(_) => {}
        Err(error) if is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error).context("inspect recorded DVM process identity"),
    }
    let Some(pidfd) = PidFd::open(pid)? else {
        return Ok(());
    };
    // Close the check/open race: if the numeric PID changed identity before
    // pidfd_open, the descriptor may refer to an unrelated process. Never
    // signal it unless the post-open /proc identity still matches the record.
    match process_start_ticks(pid) {
        Ok(actual) if actual != expected_start_ticks => return Ok(()),
        Ok(_) => {}
        Err(error) if is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error).context("revalidate recorded DVM process identity"),
    }
    pidfd.send_signal(libc::SIGTERM)?;
    if pidfd.wait_exited(TERMINATE_GRACE)? {
        return Ok(());
    }
    pidfd.send_signal(libc::SIGKILL)?;
    if pidfd.wait_exited(TERMINATE_GRACE)? {
        return Ok(());
    }
    bail!("DVM pid {pid} remained live after bounded pidfd SIGKILL")
}

struct PidFd(OwnedFd);

impl PidFd {
    fn open(pid: u32) -> Result<Option<Self>> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("pidfd_open recorded DVM process");
        }
        Ok(Some(Self(unsafe { OwnedFd::from_raw_fd(fd as i32) })))
    }

    fn send_signal(&self, signal: libc::c_int) -> Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context("pidfd_send_signal recorded DVM process");
            }
        }
        Ok(())
    }

    fn wait_exited(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if result > 0 {
                return Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0);
            }
            if result == 0 {
                return Ok(false);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error).context("poll recorded DVM pidfd");
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}

fn wait_for_process_start(pid: u32, timeout: Duration) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    loop {
        match process_start_ticks(pid) {
            Ok(start) => return Ok(start),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("DVM process identity did not appear"),
        }
    }
}

fn process_start_ticks(pid: u32) -> Result<u64> {
    let source = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = source
        .rfind(')')
        .ok_or_else(|| anyhow!("malformed /proc/{pid}/stat"))?;
    let fields = source[end + 1..].split_whitespace().collect::<Vec<_>>();
    fields
        .get(19)
        .ok_or_else(|| anyhow!("missing /proc/{pid}/stat starttime"))?
        .parse()
        .context("invalid process starttime")
}

fn verify_qemu(path: &Path) -> Result<(PathBuf, String)> {
    let canonical = trusted_canonical_regular_file(path, 0)?;
    let digest = sha256_file(&canonical)?;
    Ok((canonical, digest))
}

fn verify_iommufd() -> Result<()> {
    let path = Path::new("/dev/iommu");
    let metadata = fs::symlink_metadata(path).context("inspect /dev/iommu")?;
    if !metadata.file_type().is_char_device() {
        bail!("/dev/iommu is not a character device");
    }
    let device = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .context("open /dev/iommu for non-identity DVM mapping")?;
    let mut allocation = IommuIoasAlloc {
        size: u32::try_from(std::mem::size_of::<IommuIoasAlloc>())
            .expect("IOMMUFD allocation ABI size fits u32"),
        flags: 0,
        out_ioas_id: 0,
    };
    if unsafe {
        libc::ioctl(
            device.as_raw_fd(),
            IOMMU_IOAS_ALLOC,
            &mut allocation as *mut IommuIoasAlloc,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error())
            .context("allocate empty IOMMUFD IO address space");
    }
    if allocation.out_ioas_id == 0 {
        bail!("IOMMUFD returned invalid zero IOAS object ID");
    }
    let mut destroy = IommuDestroy {
        size: u32::try_from(std::mem::size_of::<IommuDestroy>())
            .expect("IOMMUFD destroy ABI size fits u32"),
        id: allocation.out_ioas_id,
    };
    if unsafe {
        libc::ioctl(
            device.as_raw_fd(),
            IOMMU_DESTROY,
            &mut destroy as *mut IommuDestroy,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error()).context("destroy IOMMUFD IO address space");
    }
    Ok(())
}

fn ensure_private_regular_file(path: &Path) -> Result<()> {
    ensure_trusted_parent(path, unsafe { libc::geteuid() })?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect private DVM file {}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "DVM file {} must be owner-private and non-symlink",
            path.display()
        );
    }
    Ok(())
}

fn ensure_private_socket(path: &Path) -> Result<()> {
    ensure_trusted_parent(path, unsafe { libc::geteuid() })?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect private DVM socket {}", path.display()))?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "DVM socket {} must be owner-private and non-symlink",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn trusted_canonical_regular_file(path: &Path, owner: u32) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize trusted file {}", path.display()))?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if !metadata.file_type().is_file() || metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
        bail!(
            "trusted file {} must be owned by uid {owner} and not group/world writable",
            canonical.display()
        );
    }
    ensure_trusted_parent(&canonical, owner)?;
    Ok(canonical)
}

fn ensure_trusted_parent(path: &Path, owner: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("trusted path {} has no parent", path.display()))?;
    ensure_trusted_directory(parent, owner, false)
}

fn ensure_trusted_directory(path: &Path, owner: u32, private: bool) -> Result<()> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize trusted directory {}", path.display()))?;
    for directory in canonical.ancestors() {
        let metadata = fs::symlink_metadata(directory)?;
        let allowed_owner = metadata.uid() == 0 || metadata.uid() == owner;
        let unsafe_mode = if directory == canonical && private {
            metadata.mode() & 0o077 != 0
        } else {
            metadata.mode() & 0o022 != 0
        };
        if !metadata.file_type().is_dir() || !allowed_owner || unsafe_mode {
            bail!(
                "untrusted directory {} in path {}",
                directory.display(),
                canonical.display()
            );
        }
    }
    Ok(())
}

fn parse_manifest(source: &str, label: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for (index, raw) in source.lines().enumerate() {
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid manifest {label}:{}", index + 1))?;
        if key.is_empty()
            || value.is_empty()
            || key.contains(char::is_whitespace)
            || values.insert(key.to_owned(), value.to_owned()).is_some()
        {
            bail!("invalid manifest {label}:{}", index + 1);
        }
    }
    Ok(values)
}

fn manifest_value<'a>(values: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing DVM manifest key {key}"))
}

fn parse_env(source: &str, label: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for raw in source.lines() {
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid {label} line"))?;
        if key.is_empty()
            || value.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || values.insert(key.to_owned(), value.to_owned()).is_some()
        {
            bail!("invalid {label} key {key:?}");
        }
    }
    Ok(values)
}

fn value<'a>(values: &'a BTreeMap<String, String>, key: &str, label: &str) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing {label} key {key}"))
}

fn parse_sha256(value: &str) -> Result<String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest");
    }
    Ok(value.to_ascii_lowercase())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_sha256_file(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected.to_ascii_lowercase() {
        bail!("SHA-256 mismatch for {}", path.display());
    }
    Ok(())
}

fn replace_private(path: &Path, contents: &str) -> Result<()> {
    ensure_trusted_parent(path, unsafe { libc::geteuid() })?;
    for _ in 0..16 {
        let sequence = PRIVATE_REPLACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create private replacement file"),
        };
        let result = (|| {
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }
    bail!("could not allocate a private replacement file")
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn validate_domain_id(domain_id: &str) -> Result<()> {
    if domain_id.is_empty()
        || domain_id.len() > 64
        || !domain_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid DVM runtime domain id");
    }
    Ok(())
}

fn current_unix_time() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn is_missing_runtime(error: &anyhow::Error) -> bool {
    is_not_found(error)
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn iommufd_probe_matches_linux_uapi_layout() {
        assert_eq!(std::mem::size_of::<IommuIoasAlloc>(), 12);
        assert_eq!(std::mem::size_of::<IommuDestroy>(), 8);
        assert_eq!(IOMMU_IOAS_ALLOC, 0x3b81);
        assert_eq!(IOMMU_DESTROY, 0x3b80);
    }

    #[test]
    fn runtime_record_rejects_pid_reuse_inputs_and_unknown_keys() {
        let digest = "a".repeat(64);
        let valid = format!(
            "DVM_RUNTIME_SCHEMA=2\nRUNTIME_STATE=ready\nDOMAIN_ID=linux-dvm-gpu0\nDVM_GUEST_CID=4\nIOMMU_GROUP=17\nIOMMU_MODE={IOMMU_MODE}\nPID=42\nPROCESS_START_TICKS=99\nLAUNCHED_AT_UNIX=100\nQEMU_SHA256={digest}\nDVM_ARTIFACT_MANIFEST_SHA256={digest}\nDEVICE_POLICY_SHA256={digest}\nRELEASE_MANIFEST_SHA256={digest}\n"
        );
        assert!(RuntimeRecord::parse(&valid, "test").is_ok());
        assert!(RuntimeRecord::parse(&valid.replace("PID=42", "PID=0"), "test").is_err());
        assert!(RuntimeRecord::parse(&(valid + "EXTRA=1\n"), "test").is_err());
    }

    #[test]
    fn trusted_file_rejects_mutable_file_and_ancestor() {
        let owner = unsafe { libc::geteuid() };
        let current = std::env::current_dir().unwrap();
        let trusted_test_parent = current
            .ancestors()
            .find(|candidate| {
                ensure_trusted_directory(candidate, owner, false).is_ok()
                    && fs::metadata(candidate).is_ok_and(|metadata| metadata.mode() & 0o200 != 0)
            })
            .unwrap();
        let root = trusted_test_parent.join(format!(
            ".hostd-path-test-{}-{}",
            std::process::id(),
            PRIVATE_REPLACE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let file = root.join("artifact");
        fs::write(&file, b"bound artifact").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            trusted_canonical_regular_file(&file, owner).unwrap(),
            file.canonicalize().unwrap()
        );

        fs::set_permissions(&file, fs::Permissions::from_mode(0o620)).unwrap();
        assert!(trusted_canonical_regular_file(&file, owner).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o720)).unwrap();
        assert!(trusted_canonical_regular_file(&file, owner).is_err());

        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pidfd_binds_the_current_process_without_reporting_exit() {
        let pidfd = PidFd::open(std::process::id()).unwrap().unwrap();
        assert!(!pidfd.wait_exited(Duration::ZERO).unwrap());
    }

    #[test]
    fn supervised_child_exit_must_be_successful() {
        assert!(require_successful_child_exit(ExitStatus::from_raw(0)).is_ok());
        assert!(require_successful_child_exit(ExitStatus::from_raw(7 << 8)).is_err());
        assert!(require_successful_child_exit(ExitStatus::from_raw(libc::SIGKILL)).is_err());
    }
}
