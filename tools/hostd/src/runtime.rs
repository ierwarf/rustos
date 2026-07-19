use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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
    validate_reset_scope_assignment, validate_vfio_bind_dma_quiescence,
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
const ACPI_VFCT_HEADER_BYTES: usize = 0x4c;
const ACPI_VFCT_VBIOS_OFFSET: usize = 0x34;
const ACPI_VFCT_LIB1_OFFSET: usize = 0x38;
// AMD's VFCT_IMAGE_HEADER ends with two ULONG fields: Revision and
// ImageLength.  Keep the offsets explicit because treating Revision as the
// length can turn a valid firmware table into a one-byte image.
const ACPI_VFCT_IMAGE_HEADER_BYTES: usize = 28;
const ACPI_VFCT_IMAGE_REVISION_OFFSET: usize = 20;
const ACPI_VFCT_IMAGE_LENGTH_OFFSET: usize = 24;
const ACPI_VFCT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const AMDGPU_GUEST_PCI_BUS: u32 = 0;
const AMDGPU_GUEST_PCI_DEVICE: u32 = 8;
const AMDGPU_GUEST_PCI_FUNCTION: u32 = 0;
const AMDGPU_QEMU_PCI_ADDRESS: &str = "08.0";
// The release initramfs expands to about 453 MiB. A 1 GiB guest leaves only a
// marginal half-RAM root tmpfs after kernel and device reservations and was
// observed to fail part-way through unpacking. Keep the physical supervisor
// aligned with the independently exercised xtask DVM profile.
const DVM_GUEST_MEMORY: &str = "2048M";
const DVM_REQUIRED_MEMLOCK_BYTES: libc::rlim_t = 4 * 1024 * 1024 * 1024;
// Must match RUSTOS_GUI_PIXEL_REGION_BYTES in the signed DVM exporter and the
// V3 three-slot atlas ABI. The former 32 MiB supervisor value could not hold
// the admitted 128 MiB source pool.
const DVM_PIXEL_BYTES: u64 = 128 * 1024 * 1024;
const DVM_PIXEL_PHYS: u64 = 0x1_0000_0000;
const GUEST_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const TERMINATE_GRACE: Duration = Duration::from_secs(5);
const QMP_IO_TIMEOUT: Duration = Duration::from_secs(3);
const QMP_CONNECT_RETRY: Duration = Duration::from_millis(50);
const QMP_MAX_MESSAGE_BYTES: u64 = 64 * 1024;
const QMP_MAX_MESSAGES: usize = 32;
const QMP_CAPABILITIES_ID: &str = "rustos-capabilities";
const QMP_POWERDOWN_ID: &str = "rustos-powerdown";
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopKind {
    CleanExit,
    Forced,
}

#[derive(Debug)]
struct StopResult {
    status: ExitStatus,
    kind: StopKind,
    qmp_failure: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmdVbiosArtifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmdVfctArtifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size: usize,
}

struct ValidatedAmdVfct {
    table: Vec<u8>,
    image: Vec<u8>,
    image_header_offset: usize,
}

struct LocatedAmdVbios {
    image: Vec<u8>,
    header_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AmdPciIdentity {
    bus: u32,
    device: u32,
    function: u32,
    vendor: u16,
    device_id: u16,
    subsystem_vendor: u16,
    subsystem_device: u16,
}

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
            != "health,device-inventory,driver-inventory,display-evidence-v2,input-stream"
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

fn read_le_u16(bytes: &[u8], offset: usize, label: &str) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow!("truncated {label}"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_le_u32(bytes: &[u8], offset: usize, label: &str) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("truncated {label}"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_pci_hex_u16(path: &Path, field: &str, bdf: &str) -> Result<u16> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read PCI {field} for AMD VBIOS target {bdf}"))?;
    let value = source
        .trim()
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("PCI {field} for {bdf} lacks 0x prefix"))?;
    u16::from_str_radix(value, 16).with_context(|| format!("parse PCI {field} for {bdf}"))
}

fn amd_pci_identity(sysfs_root: &Path, bdf: &str) -> Result<AmdPciIdentity> {
    let bytes = bdf.as_bytes();
    if bytes.len() != 12 || bytes[4] != b':' || bytes[7] != b':' || bytes[10] != b'.' {
        bail!("invalid AMD display PCI BDF {bdf:?}");
    }
    let bus = u32::from_str_radix(&bdf[5..7], 16)
        .with_context(|| format!("parse AMD display PCI bus in {bdf}"))?;
    let device = u32::from_str_radix(&bdf[8..10], 16)
        .with_context(|| format!("parse AMD display PCI device in {bdf}"))?;
    let function = u32::from_str_radix(&bdf[11..12], 16)
        .with_context(|| format!("parse AMD display PCI function in {bdf}"))?;
    let device_path = sysfs_root.join("bus/pci/devices").join(bdf);
    Ok(AmdPciIdentity {
        bus,
        device,
        function,
        vendor: read_pci_hex_u16(&device_path.join("vendor"), "vendor", bdf)?,
        device_id: read_pci_hex_u16(&device_path.join("device"), "device", bdf)?,
        subsystem_vendor: read_pci_hex_u16(
            &device_path.join("subsystem_vendor"),
            "subsystem vendor",
            bdf,
        )?,
        subsystem_device: read_pci_hex_u16(
            &device_path.join("subsystem_device"),
            "subsystem device",
            bdf,
        )?,
    })
}

fn validate_atom_vbios(image: &[u8]) -> Result<()> {
    if image.len() < 0x4a || image[0..2] != [0x55, 0xaa] {
        bail!("VFCT AMD VBIOS lacks the 0x55aa signature");
    }
    let header = usize::from(read_le_u16(image, 0x48, "AMD VBIOS header pointer")?);
    let signature_offset = header
        .checked_add(4)
        .ok_or_else(|| anyhow!("AMD VBIOS header pointer overflow"))?;
    let signature = image
        .get(signature_offset..signature_offset + 4)
        .ok_or_else(|| anyhow!("AMD VBIOS ATOM header is truncated"))?;
    if signature != b"ATOM" && signature != b"MOTA" {
        bail!("VFCT AMD VBIOS lacks an ATOM firmware header");
    }
    Ok(())
}

fn locate_amdgpu_vfct_image(table: &[u8], target: AmdPciIdentity) -> Result<LocatedAmdVbios> {
    if table.len() < ACPI_VFCT_HEADER_BYTES || table.get(0..4) != Some(b"VFCT") {
        bail!("ACPI VFCT table header is missing or truncated");
    }
    let table_length = usize::try_from(read_le_u32(table, 4, "ACPI VFCT length")?)
        .context("ACPI VFCT length does not fit usize")?;
    if table_length != table.len() {
        bail!(
            "ACPI VFCT length mismatch: header={table_length} actual={}",
            table.len()
        );
    }
    if table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        bail!("ACPI VFCT checksum is invalid");
    }
    let mut offset = usize::try_from(read_le_u32(
        table,
        ACPI_VFCT_VBIOS_OFFSET,
        "ACPI VFCT VBIOS offset",
    )?)
    .context("ACPI VFCT VBIOS offset does not fit usize")?;
    let lib1_offset = usize::try_from(read_le_u32(
        table,
        ACPI_VFCT_LIB1_OFFSET,
        "ACPI VFCT library offset",
    )?)
    .context("ACPI VFCT library offset does not fit usize")?;
    let image_end = if lib1_offset == 0 {
        table.len()
    } else {
        lib1_offset
    };
    if offset < ACPI_VFCT_HEADER_BYTES || offset >= image_end || image_end > table.len() {
        bail!("ACPI VFCT VBIOS image range is invalid");
    }

    let mut matched = None;
    while offset < image_end {
        // Firmware may pad the final image to the ACPI table length.  Admit
        // only an entirely zero tail; nonzero trailing bytes must still parse
        // as a complete VFCT_IMAGE_HEADER plus image or fail closed.
        if table[offset..image_end].iter().all(|byte| *byte == 0) {
            offset = image_end;
            break;
        }
        let header_end = offset
            .checked_add(ACPI_VFCT_IMAGE_HEADER_BYTES)
            .ok_or_else(|| anyhow!("ACPI VFCT image header overflow"))?;
        if header_end > image_end {
            bail!("ACPI VFCT image header is truncated");
        }
        let bus = read_le_u32(table, offset, "VFCT image PCI bus")?;
        let device = read_le_u32(table, offset + 4, "VFCT image PCI device")?;
        let function = read_le_u32(table, offset + 8, "VFCT image PCI function")?;
        let vendor = read_le_u16(table, offset + 12, "VFCT image PCI vendor")?;
        let device_id = read_le_u16(table, offset + 14, "VFCT image PCI device ID")?;
        let subsystem_vendor = read_le_u16(table, offset + 16, "VFCT image subsystem vendor")?;
        let subsystem_device = read_le_u16(table, offset + 18, "VFCT image subsystem device")?;
        let _revision = read_le_u32(
            table,
            offset + ACPI_VFCT_IMAGE_REVISION_OFFSET,
            "VFCT image revision",
        )?;
        let image_length = usize::try_from(read_le_u32(
            table,
            offset + ACPI_VFCT_IMAGE_LENGTH_OFFSET,
            "VFCT image length",
        )?)
        .context("VFCT image length does not fit usize")?;
        let next = header_end
            .checked_add(image_length)
            .ok_or_else(|| anyhow!("ACPI VFCT image length overflow"))?;
        if image_length == 0 || next > image_end {
            bail!("ACPI VFCT image is empty or truncated");
        }
        // Some firmware, including the GA403UM VFCT, leaves both subsystem
        // fields zero.  That is an absent identity, not a wildcard per field:
        // accept only the all-zero pair, retain exact BDF/vendor/device
        // matching, and still reject more than one matching image.  A partial
        // zero or a populated mismatch remains fail-closed.
        let subsystem_matches = (subsystem_vendor == 0 && subsystem_device == 0)
            || (subsystem_vendor == target.subsystem_vendor
                && subsystem_device == target.subsystem_device);
        if bus == target.bus
            && device == target.device
            && function == target.function
            && vendor == target.vendor
            && device_id == target.device_id
            && subsystem_matches
        {
            if matched.is_some() {
                bail!("ACPI VFCT contains duplicate images for the AMD display target");
            }
            let image = table[header_end..next].to_vec();
            validate_atom_vbios(&image)?;
            matched = Some(LocatedAmdVbios {
                image,
                header_offset: offset,
            });
        }
        offset = next;
    }
    if offset != image_end {
        bail!("ACPI VFCT VBIOS image range has trailing partial data");
    }
    matched.ok_or_else(|| anyhow!("ACPI VFCT contains no exact image for the AMD display target"))
}

#[cfg(test)]
fn extract_amdgpu_vfct_image(table: &[u8], target: AmdPciIdentity) -> Result<Vec<u8>> {
    Ok(locate_amdgpu_vfct_image(table, target)?.image)
}

fn read_validated_amdgpu_vfct(
    vfct_path: &Path,
    sysfs_root: &Path,
    display_bdf: &str,
) -> Result<ValidatedAmdVfct> {
    let metadata = fs::symlink_metadata(vfct_path)
        .with_context(|| format!("inspect ACPI VFCT source {}", vfct_path.display()))?;
    if !metadata.file_type().is_file() || metadata.mode() & 0o022 != 0 {
        bail!("ACPI VFCT source must be a non-symlink file that is not group/world writable");
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(vfct_path)
        .with_context(|| format!("open ACPI VFCT source {}", vfct_path.display()))?;
    let mut table = Vec::new();
    Read::by_ref(&mut file)
        .take(ACPI_VFCT_MAX_BYTES + 1)
        .read_to_end(&mut table)
        .with_context(|| format!("read ACPI VFCT source {}", vfct_path.display()))?;
    if u64::try_from(table.len()).unwrap_or(u64::MAX) > ACPI_VFCT_MAX_BYTES {
        bail!("ACPI VFCT source exceeds the bounded 4 MiB limit");
    }
    let located = locate_amdgpu_vfct_image(&table, amd_pci_identity(sysfs_root, display_bdf)?)?;
    Ok(ValidatedAmdVfct {
        table,
        image: located.image,
        image_header_offset: located.header_offset,
    })
}

pub fn export_amdgpu_vbios(
    vfct_path: &Path,
    sysfs_root: &Path,
    display_bdf: &str,
    output: &Path,
) -> Result<AmdVbiosArtifact> {
    let image = read_validated_amdgpu_vfct(vfct_path, sysfs_root, display_bdf)?.image;
    let sha256 = format!("{:x}", Sha256::digest(&image));
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("AMD VBIOS output {} has no parent", output.display()))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(output)
        .with_context(|| format!("create private AMD VBIOS output {}", output.display()))?;
    if let Err(error) = (|| -> Result<()> {
        file.write_all(&image)?;
        file.sync_all()?;
        sync_directory(parent)?;
        Ok(())
    })() {
        drop(file);
        let _ = fs::remove_file(output);
        return Err(error).context("persist private AMD VBIOS snapshot");
    }
    let path = fs::canonicalize(output)
        .with_context(|| format!("canonicalize AMD VBIOS output {}", output.display()))?;
    Ok(AmdVbiosArtifact {
        path,
        sha256,
        size: image.len(),
    })
}

fn validate_amdgpu_vfct_source(sysfs_root: &Path, display_bdf: &str) -> Result<()> {
    read_validated_amdgpu_vfct(
        &sysfs_root.join("firmware/acpi/tables/VFCT"),
        sysfs_root,
        display_bdf,
    )?;
    Ok(())
}

pub fn export_amdgpu_guest_vfct(
    vfct_path: &Path,
    sysfs_root: &Path,
    display_bdf: &str,
    output: &Path,
) -> Result<AmdVfctArtifact> {
    let mut validated = read_validated_amdgpu_vfct(vfct_path, sysfs_root, display_bdf)?;
    let header = validated.image_header_offset;
    validated.table[header..header + 4].copy_from_slice(&AMDGPU_GUEST_PCI_BUS.to_le_bytes());
    validated.table[header + 4..header + 8].copy_from_slice(&AMDGPU_GUEST_PCI_DEVICE.to_le_bytes());
    validated.table[header + 8..header + 12]
        .copy_from_slice(&AMDGPU_GUEST_PCI_FUNCTION.to_le_bytes());
    // The host identity is validated before relocation.  QEMU then pins the
    // device at this one guest BDF, so only those three routing fields may
    // change; the VBIOS payload and PCI identity must remain byte-for-byte.
    validated.table[9] = 0;
    let checksum = validated
        .table
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    validated.table[9] = 0_u8.wrapping_sub(checksum);
    let guest_target = AmdPciIdentity {
        bus: AMDGPU_GUEST_PCI_BUS,
        device: AMDGPU_GUEST_PCI_DEVICE,
        function: AMDGPU_GUEST_PCI_FUNCTION,
        ..amd_pci_identity(sysfs_root, display_bdf)?
    };
    let relocated = locate_amdgpu_vfct_image(&validated.table, guest_target)?;
    if relocated.header_offset != header || relocated.image != validated.image {
        bail!("relocated AMD VFCT changed the validated VBIOS payload");
    }
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("AMD VFCT output {} has no parent", output.display()))?;
    let sha256 = format!("{:x}", Sha256::digest(&validated.table));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(output)
        .with_context(|| format!("create private AMD VFCT snapshot {}", output.display()))?;
    if let Err(error) = (|| -> Result<()> {
        file.write_all(&validated.table)?;
        file.sync_all()?;
        sync_directory(parent)?;
        Ok(())
    })() {
        drop(file);
        let _ = fs::remove_file(output);
        return Err(error).context("persist private AMD VFCT snapshot");
    }
    Ok(AmdVfctArtifact {
        path: fs::canonicalize(output)
            .with_context(|| format!("canonicalize AMD VFCT snapshot {}", output.display()))?,
        sha256,
        size: validated.table.len(),
    })
}

fn prepare_amdgpu_vfct(
    sysfs_root: &Path,
    display_bdf: &str,
    runtime_dir: &Path,
) -> Result<AmdVfctArtifact> {
    export_amdgpu_guest_vfct(
        &sysfs_root.join("firmware/acpi/tables/VFCT"),
        sysfs_root,
        display_bdf,
        &runtime_dir.join("amdgpu-vfct.bin"),
    )
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
    let display_bdf = validate_physical_display_identity(lease, sysfs_root, physical_display)?;
    if physical_display.driver() == "amdgpu" {
        validate_amdgpu_vfct_source(sysfs_root, &display_bdf)?;
    }
    validate_reset_scope_assignment(lease, sysfs_root)?;
    validate_vfio_bind_dma_quiescence(sysfs_root)?;
    let (_, qemu_sha256) = verify_qemu(qemu)?;
    if qemu_sha256 != policy.qemu_sha256() {
        bail!("physical runtime QEMU digest does not match the signed device policy");
    }
    verify_artifacts(artifact_manifest)?;
    verify_qemu_memlock_budget()?;
    probe_iommufd()?;
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
    ensure_private_socket(config.display_doorbell)?;
    let (qemu, qemu_sha256) = verify_qemu(config.qemu)?;
    if qemu_sha256 != policy.qemu_sha256() {
        bail!("production QEMU digest does not match the signed device policy");
    }
    verify_qemu_memlock_budget()?;
    probe_iommufd()?;
    let artifacts = verify_artifacts(&artifact_manifest)?;
    prepare_dma_pinnable_pixel_file(config.display_pixels)?;
    let contract = artifacts.control.clone();
    let secret = ControlSecret::random()?;
    let secret_path = store.write_secret(&lease.domain_id, &secret)?;
    let listener = HostControlListener::bind(lease.dvm_guest_cid, contract, secret)?;
    let runtime_dir = store.prepare_domain_dir(&lease.domain_id)?;
    let amd_vfct = if physical_display.driver() == "amdgpu" {
        match prepare_amdgpu_vfct(sysfs_root, &display_bdf, &runtime_dir) {
            Ok(artifact) => Some(artifact),
            Err(error) => {
                let _ = store.remove(&lease.domain_id);
                return Err(error).context("prepare exact host AMD VFCT before device reset");
            }
        }
    } else {
        None
    };
    let qmp_path = match prepare_qmp_socket(&runtime_dir) {
        Ok(path) => path,
        Err(error) => {
            let _ = store.remove(&lease.domain_id);
            return Err(error).context("prepare private QMP shutdown endpoint");
        }
    };

    let mut ops = SysfsVfioOps::new(sysfs_root);
    reset_vfio_group(lease, &mut ops)?;
    install_signal_handlers()?;
    TERMINATE_REQUESTED.store(false, Ordering::Release);
    let (mut child, mut launch_gate) = match spawn_qemu_gated(
        &qemu,
        lease,
        &artifacts,
        &display_bdf,
        amd_vfct.as_ref(),
        &secret_path,
        config.display_doorbell,
        config.display_pixels,
        &runtime_dir,
        &qmp_path,
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
            &qmp_path,
        )?;
        require_successful_child_exit(status)
    })();
    let stop_error = if run_result.is_err() {
        stop_child(&mut child, &qmp_path).err()
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
            let qmp_path = store.domain_dir(&lease.domain_id)?.join("qmp.sock");
            terminate_recovered_process(record.pid, record.process_start_ticks, &qmp_path)?;
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
    display_bdf: &str,
    amd_vfct: Option<&AmdVfctArtifact>,
    secret: &Path,
    display_doorbell: &Path,
    display_pixels: &Path,
    runtime_dir: &Path,
    qmp_path: &Path,
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
    let qmp_argument = qmp_server_argument(qmp_path)?;
    let (parent_gate, child_gate) = UnixStream::pair().context("create DVM launch gate")?;
    let parent_gate_fd = parent_gate.as_raw_fd();
    let child_gate_fd = child_gate.as_raw_fd();
    command
        .arg("-name")
        .arg(format!("rustos-dvm-{}", lease.domain_id))
        .args([
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-m",
            DVM_GUEST_MEMORY,
            "-smp",
            "2",
        ])
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
        .arg("-qmp")
        .arg(qmp_argument)
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
    if let Some(vfct) = amd_vfct {
        let path = vfct
            .path
            .to_str()
            .ok_or_else(|| anyhow!("AMD VFCT path is not UTF-8"))?;
        if path.contains([',', '\n', '\r']) {
            bail!("AMD VFCT path cannot be represented as a QEMU ACPI table property");
        }
        if vfct.size < ACPI_VFCT_HEADER_BYTES || vfct.sha256.len() != 64 {
            bail!("private AMD VFCT snapshot metadata is invalid");
        }
        command.arg("-acpitable").arg(format!("file={path}"));
    }
    for bdf in lease.original_drivers.keys() {
        let mut device = format!("vfio-pci,host={bdf},iommufd=iommufd0");
        if bdf == display_bdf && amd_vfct.is_some() {
            device.push_str(",addr=");
            device.push_str(AMDGPU_QEMU_PCI_ADDRESS);
        }
        command.arg("-device").arg(device);
    }
    // The child cannot exec QEMU (and therefore cannot open VFIO) until the
    // parent has fsync'd the exact PID/start-time runtime record. If hostd
    // crashes first, closing the peer makes pre_exec fail closed.
    unsafe {
        command.pre_exec(move || {
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(signal, libc::SIG_DFL);
            }
            libc::umask(0o077);
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
    qmp_path: &Path,
) -> Result<ExitStatus> {
    let mut next_health = Instant::now() + HEALTH_INTERVAL;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if TERMINATE_REQUESTED.load(Ordering::Acquire) {
            record.state = RuntimeState::Stopping;
            store.write_record(record)?;
            let stopped = stop_child(child, qmp_path)?;
            if stopped.kind != StopKind::CleanExit {
                bail!(
                    "DVM required forced termination after bounded ACPI shutdown: {}",
                    stopped
                        .qmp_failure
                        .as_deref()
                        .unwrap_or("unknown QMP failure")
                );
            }
            return Ok(stopped.status);
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

fn prepare_qmp_socket(runtime_dir: &Path) -> Result<PathBuf> {
    let owner = unsafe { libc::geteuid() };
    ensure_trusted_directory(runtime_dir, owner, true)?;
    let path = runtime_dir.join("qmp.sock");
    qmp_server_argument(&path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.file_type().is_socket()
                && metadata.uid() == owner
                && metadata.mode() & 0o077 == 0 =>
        {
            fs::remove_file(&path)
                .with_context(|| format!("remove stale private QMP socket {}", path.display()))?;
            sync_directory(runtime_dir)?;
        }
        Ok(_) => bail!(
            "refusing non-private or non-socket QMP endpoint {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect QMP socket {}", path.display()));
        }
    }
    Ok(path)
}

fn qmp_server_argument(path: &Path) -> Result<String> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() > 107 {
        bail!("QMP Unix socket path must fit Linux sockaddr_un");
    }
    if bytes
        .iter()
        .any(|byte| matches!(*byte, b',' | b'\n' | b'\r' | 0))
    {
        bail!("QMP Unix socket path cannot be represented as a QEMU property");
    }
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("QMP Unix socket path is not UTF-8"))?;
    Ok(format!("unix:{path},server=on,wait=off"))
}

fn connect_private_qmp(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let owner = unsafe { libc::geteuid() };
    ensure_trusted_parent(path, owner)?;
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket()
                    || metadata.uid() != owner
                    || metadata.mode() & 0o077 != 0
                {
                    bail!(
                        "QMP endpoint {} must be an owner-private Unix socket",
                        path.display()
                    );
                }
                match UnixStream::connect(path) {
                    Ok(stream) => return Ok(stream),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("connect QMP socket {}", path.display()));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect QMP socket {}", path.display()));
            }
        }
        if Instant::now() >= deadline {
            bail!("private QMP socket {} did not become ready", path.display());
        }
        std::thread::sleep(QMP_CONNECT_RETRY);
    }
}

fn read_qmp_message(reader: &mut impl BufRead) -> Result<serde_json::Value> {
    let mut message = Vec::new();
    let count = (&mut *reader)
        .take(QMP_MAX_MESSAGE_BYTES + 1)
        .read_until(b'\n', &mut message)
        .context("read bounded QMP message")?;
    if count == 0 {
        bail!("QMP endpoint closed before a complete response");
    }
    if u64::try_from(count).unwrap_or(u64::MAX) > QMP_MAX_MESSAGE_BYTES {
        bail!("QMP message exceeds 64 KiB bound");
    }
    if !message.ends_with(b"\r\n") {
        bail!("QMP message is not CRLF terminated");
    }
    message.truncate(message.len() - 2);
    let value: serde_json::Value =
        serde_json::from_slice(&message).context("parse QMP JSON object")?;
    if !value.is_object() {
        bail!("QMP message must be a JSON object");
    }
    Ok(value)
}

fn write_qmp_command(stream: &mut UnixStream, execute: &str, id: &str) -> Result<()> {
    let command = serde_json::json!({"execute": execute, "id": id});
    serde_json::to_writer(&mut *stream, &command).context("encode QMP command")?;
    stream.write_all(b"\r\n")?;
    stream.flush().context("flush QMP command")
}

fn require_qmp_response(reader: &mut impl BufRead, expected_id: &str) -> Result<()> {
    for _ in 0..QMP_MAX_MESSAGES {
        let response = read_qmp_message(reader)?;
        if response.get("event").is_some() {
            continue;
        }
        if response.get("id").and_then(serde_json::Value::as_str) != Some(expected_id) {
            bail!("QMP returned an unexpected response id");
        }
        if let Some(error) = response.get("error") {
            bail!("QMP command {expected_id} failed: {error}");
        }
        if response.get("return").is_none() {
            bail!("QMP command {expected_id} response has no return member");
        }
        return Ok(());
    }
    bail!("QMP command {expected_id} exceeded the event/response bound")
}

fn request_guest_powerdown(qmp_path: &Path) -> Result<()> {
    let deadline = Instant::now() + QMP_IO_TIMEOUT;
    let mut stream = connect_private_qmp(qmp_path, deadline)?;
    stream.set_read_timeout(Some(QMP_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(QMP_IO_TIMEOUT))?;
    let reader_stream = stream.try_clone().context("clone QMP stream reader")?;
    let mut reader = BufReader::new(reader_stream);
    let greeting = read_qmp_message(&mut reader).context("read QMP server greeting")?;
    if !greeting
        .get("QMP")
        .is_some_and(serde_json::Value::is_object)
    {
        bail!("QMP server greeting lacks the QMP object");
    }
    write_qmp_command(&mut stream, "qmp_capabilities", QMP_CAPABILITIES_ID)?;
    require_qmp_response(&mut reader, QMP_CAPABILITIES_ID)?;
    write_qmp_command(&mut stream, "system_powerdown", QMP_POWERDOWN_ID)?;
    require_qmp_response(&mut reader, QMP_POWERDOWN_ID)?;
    Ok(())
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(SUPERVISOR_TICK);
    }
}

fn stop_child(child: &mut Child, qmp_path: &Path) -> Result<StopResult> {
    if let Some(status) = child.try_wait()? {
        return Ok(StopResult {
            status,
            kind: StopKind::CleanExit,
            qmp_failure: None,
        });
    }
    let qmp_failure = match request_guest_powerdown(qmp_path) {
        Ok(()) => {
            if let Some(status) = wait_child_exit(child, GUEST_SHUTDOWN_GRACE)? {
                return Ok(StopResult {
                    status,
                    kind: StopKind::CleanExit,
                    qmp_failure: None,
                });
            }
            Some("QMP system_powerdown was accepted but QEMU did not exit in 10 seconds".to_owned())
        }
        Err(error) => Some(format!("{error:#}")),
    };
    terminate_process(child.id())?;
    if let Some(status) = wait_child_exit(child, TERMINATE_GRACE)? {
        return Ok(StopResult {
            status,
            kind: StopKind::Forced,
            qmp_failure,
        });
    }
    child.kill().context("SIGKILL supervised DVM")?;
    let status = child.wait().context("reap supervised DVM")?;
    Ok(StopResult {
        status,
        kind: StopKind::Forced,
        qmp_failure,
    })
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

fn terminate_recovered_process(pid: u32, expected_start_ticks: u64, qmp_path: &Path) -> Result<()> {
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
    if request_guest_powerdown(qmp_path).is_ok() && pidfd.wait_exited(GUEST_SHUTDOWN_GRACE)? {
        return Ok(());
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

fn memlock_budget_is_adequate(limit: libc::rlim_t) -> bool {
    limit == libc::RLIM_INFINITY || limit >= DVM_REQUIRED_MEMLOCK_BYTES
}

fn verify_qemu_memlock_budget() -> Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read QEMU memlock resource limit");
    }
    if !memlock_budget_is_adequate(limit.rlim_cur) {
        bail!(
            "physical DVM requires RLIMIT_MEMLOCK soft limit >= {} bytes (observed {}) before VFIO mutation",
            DVM_REQUIRED_MEMLOCK_BYTES,
            limit.rlim_cur
        );
    }
    Ok(())
}

/// Exercise only the userspace IOMMUFD ABI. This does not bind, open, enable,
/// or reset a VFIO device.
pub fn probe_iommufd() -> Result<()> {
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

fn prepare_dma_pinnable_pixel_file(path: &Path) -> Result<()> {
    ensure_trusted_parent(path, unsafe { libc::geteuid() })?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open private DVM pixel file {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() != DVM_PIXEL_BYTES
    {
        bail!(
            "DVM pixel file {} must be owner-private, non-symlink, and exactly {} bytes",
            path.display(),
            DVM_PIXEL_BYTES,
        );
    }
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("inspect DVM pixel filesystem {}", path.display()));
    }
    let filesystem = unsafe { filesystem.assume_init() };
    let filesystem_type = filesystem.f_type as u64;
    if !dma_pinnable_filesystem_type(filesystem_type) {
        bail!(
            "DVM pixel file {} must use DMA-pinnable tmpfs or hugetlbfs, got filesystem type {filesystem_type:#x}",
            path.display()
        );
    }
    let length =
        libc::off_t::try_from(DVM_PIXEL_BYTES).expect("bounded DVM pixel aperture fits off_t");
    if unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, length) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("preallocate DVM pixel backing {}", path.display()));
    }
    file.sync_data()
        .with_context(|| format!("sync DVM pixel backing {}", path.display()))?;
    Ok(())
}

fn dma_pinnable_filesystem_type(filesystem_type: u64) -> bool {
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    const HUGETLBFS_MAGIC: u64 = 0x9584_58f6;
    matches!(filesystem_type, TMPFS_MAGIC | HUGETLBFS_MAGIC)
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
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::mpsc;

    fn vfct_fixture(target: AmdPciIdentity) -> Vec<u8> {
        let image_len = 0x80_usize;
        let image_offset = ACPI_VFCT_HEADER_BYTES;
        let image_start = image_offset + ACPI_VFCT_IMAGE_HEADER_BYTES;
        let mut table = vec![0_u8; image_start + image_len];
        let table_len = table.len() as u32;
        table[0..4].copy_from_slice(b"VFCT");
        table[4..8].copy_from_slice(&table_len.to_le_bytes());
        table[ACPI_VFCT_VBIOS_OFFSET..ACPI_VFCT_VBIOS_OFFSET + 4]
            .copy_from_slice(&(image_offset as u32).to_le_bytes());
        table[image_offset..image_offset + 4].copy_from_slice(&target.bus.to_le_bytes());
        table[image_offset + 4..image_offset + 8].copy_from_slice(&target.device.to_le_bytes());
        table[image_offset + 8..image_offset + 12].copy_from_slice(&target.function.to_le_bytes());
        table[image_offset + 12..image_offset + 14].copy_from_slice(&target.vendor.to_le_bytes());
        table[image_offset + 14..image_offset + 16]
            .copy_from_slice(&target.device_id.to_le_bytes());
        table[image_offset + 16..image_offset + 18]
            .copy_from_slice(&target.subsystem_vendor.to_le_bytes());
        table[image_offset + 18..image_offset + 20]
            .copy_from_slice(&target.subsystem_device.to_le_bytes());
        table[image_offset + ACPI_VFCT_IMAGE_REVISION_OFFSET
            ..image_offset + ACPI_VFCT_IMAGE_REVISION_OFFSET + 4]
            .copy_from_slice(&1_u32.to_le_bytes());
        table[image_offset + ACPI_VFCT_IMAGE_LENGTH_OFFSET
            ..image_offset + ACPI_VFCT_IMAGE_LENGTH_OFFSET + 4]
            .copy_from_slice(&(image_len as u32).to_le_bytes());
        table[image_start] = 0x55;
        table[image_start + 1] = 0xaa;
        table[image_start + 0x48..image_start + 0x4a].copy_from_slice(&0x60_u16.to_le_bytes());
        table[image_start + 0x64..image_start + 0x68].copy_from_slice(b"ATOM");
        let checksum = table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        table[9] = table[9].wrapping_sub(checksum);
        table
    }

    fn amd_target() -> AmdPciIdentity {
        AmdPciIdentity {
            bus: 0x65,
            device: 0,
            function: 0,
            vendor: 0x1002,
            device_id: 0x1900,
            subsystem_vendor: 0x1043,
            subsystem_device: 0x3a48,
        }
    }

    #[test]
    fn iommufd_probe_matches_linux_uapi_layout() {
        assert_eq!(DVM_GUEST_MEMORY, "2048M");
        assert_eq!(DVM_REQUIRED_MEMLOCK_BYTES, 4_294_967_296);
        assert!(!memlock_budget_is_adequate(8 * 1024 * 1024));
        assert!(memlock_budget_is_adequate(DVM_REQUIRED_MEMLOCK_BYTES));
        assert!(memlock_budget_is_adequate(libc::RLIM_INFINITY));
        assert_eq!(std::mem::size_of::<IommuIoasAlloc>(), 12);
        assert_eq!(std::mem::size_of::<IommuDestroy>(), 8);
        assert_eq!(IOMMU_IOAS_ALLOC, 0x3b81);
        assert_eq!(IOMMU_DESTROY, 0x3b80);
    }

    #[test]
    fn vfct_extraction_binds_exact_amd_identity_and_atom_image() {
        let target = amd_target();
        let table = vfct_fixture(target);
        let image = extract_amdgpu_vfct_image(&table, target).unwrap();
        assert_eq!(image.len(), 0x80);
        assert_eq!(&image[0..2], &[0x55, 0xaa]);
        assert_eq!(&image[0x64..0x68], b"ATOM");

        let located = locate_amdgpu_vfct_image(&table, target).unwrap();
        let mut relocated = table.clone();
        let header = located.header_offset;
        relocated[header..header + 4].copy_from_slice(&AMDGPU_GUEST_PCI_BUS.to_le_bytes());
        relocated[header + 4..header + 8].copy_from_slice(&AMDGPU_GUEST_PCI_DEVICE.to_le_bytes());
        relocated[header + 8..header + 12]
            .copy_from_slice(&AMDGPU_GUEST_PCI_FUNCTION.to_le_bytes());
        relocated[9] = 0;
        let checksum = relocated
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        relocated[9] = 0_u8.wrapping_sub(checksum);
        let guest = AmdPciIdentity {
            bus: AMDGPU_GUEST_PCI_BUS,
            device: AMDGPU_GUEST_PCI_DEVICE,
            function: AMDGPU_GUEST_PCI_FUNCTION,
            ..target
        };
        assert_eq!(
            locate_amdgpu_vfct_image(&relocated, guest).unwrap().image,
            located.image
        );
        assert!(locate_amdgpu_vfct_image(&relocated, target).is_err());

        let mut wrong = target;
        wrong.subsystem_device ^= 1;
        assert!(extract_amdgpu_vfct_image(&table, wrong).is_err());

        let mut subsystem_unspecified = vfct_fixture(target);
        subsystem_unspecified[ACPI_VFCT_HEADER_BYTES + 16..ACPI_VFCT_HEADER_BYTES + 20].fill(0);
        let checksum = subsystem_unspecified
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        subsystem_unspecified[9] = subsystem_unspecified[9].wrapping_sub(checksum);
        assert_eq!(
            extract_amdgpu_vfct_image(&subsystem_unspecified, wrong)
                .unwrap()
                .len(),
            0x80
        );

        let mut partial_subsystem = vfct_fixture(target);
        partial_subsystem[ACPI_VFCT_HEADER_BYTES + 18..ACPI_VFCT_HEADER_BYTES + 20].fill(0);
        let checksum = partial_subsystem
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        partial_subsystem[9] = partial_subsystem[9].wrapping_sub(checksum);
        assert!(extract_amdgpu_vfct_image(&partial_subsystem, target).is_err());
    }

    #[test]
    fn vfct_extraction_rejects_checksum_bounds_and_invalid_atom() {
        let target = amd_target();
        let mut bad_checksum = vfct_fixture(target);
        bad_checksum[20] ^= 1;
        assert!(extract_amdgpu_vfct_image(&bad_checksum, target).is_err());

        let mut bad_length = vfct_fixture(target);
        let image_header = ACPI_VFCT_HEADER_BYTES;
        bad_length[image_header + ACPI_VFCT_IMAGE_LENGTH_OFFSET
            ..image_header + ACPI_VFCT_IMAGE_LENGTH_OFFSET + 4]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        let checksum = bad_length
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        bad_length[9] = bad_length[9].wrapping_sub(checksum);
        assert!(extract_amdgpu_vfct_image(&bad_length, target).is_err());

        let mut bad_atom = vfct_fixture(target);
        let image_start = ACPI_VFCT_HEADER_BYTES + ACPI_VFCT_IMAGE_HEADER_BYTES;
        bad_atom[image_start] = 0;
        let checksum = bad_atom
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        bad_atom[9] = bad_atom[9].wrapping_sub(checksum);
        assert!(extract_amdgpu_vfct_image(&bad_atom, target).is_err());

        let mut padded = vfct_fixture(target);
        padded.extend_from_slice(&[0_u8; ACPI_VFCT_IMAGE_HEADER_BYTES]);
        let padded_len = padded.len() as u32;
        padded[4..8].copy_from_slice(&padded_len.to_le_bytes());
        let checksum = padded
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        padded[9] = padded[9].wrapping_sub(checksum);
        assert!(extract_amdgpu_vfct_image(&padded, target).is_ok());

        let tail = padded.len() - 1;
        padded[tail] = 1;
        let checksum = padded
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        padded[9] = padded[9].wrapping_sub(checksum);
        assert!(extract_amdgpu_vfct_image(&padded, target).is_err());
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

    #[test]
    fn physical_pixel_backing_contract_is_exact_and_dma_pinnable() {
        assert_eq!(DVM_PIXEL_BYTES, 128 * 1024 * 1024);
        assert!(dma_pinnable_filesystem_type(0x0102_1994));
        assert!(dma_pinnable_filesystem_type(0x9584_58f6));
        assert!(!dma_pinnable_filesystem_type(0xef53));
    }

    #[test]
    fn qmp_powerdown_negotiates_capabilities_before_shutdown() {
        let owner = unsafe { libc::geteuid() };
        let current = std::env::current_dir().unwrap();
        let trusted_parent = current
            .ancestors()
            .find(|candidate| {
                ensure_trusted_directory(candidate, owner, false).is_ok()
                    && fs::metadata(candidate).is_ok_and(|metadata| metadata.mode() & 0o200 != 0)
            })
            .unwrap();
        let root = trusted_parent.join(format!(
            ".hostd-qmp-test-{}-{}",
            std::process::id(),
            PRIVATE_REPLACE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("qmp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"{\"capabilities\":[],\"QMP\":{\"version\":{}}}\r\n")
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut commands = Vec::new();
            for (expected_execute, expected_id) in [
                ("qmp_capabilities", QMP_CAPABILITIES_ID),
                ("system_powerdown", QMP_POWERDOWN_ID),
            ] {
                let request = read_qmp_message(&mut reader).unwrap();
                assert_eq!(
                    request.get("execute").and_then(serde_json::Value::as_str),
                    Some(expected_execute)
                );
                assert_eq!(
                    request.get("id").and_then(serde_json::Value::as_str),
                    Some(expected_id)
                );
                commands.push(expected_execute.to_owned());
                let response = serde_json::json!({"return": {}, "id": expected_id});
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\r\n").unwrap();
                stream.flush().unwrap();
            }
            sender.send(commands).unwrap();
        });

        request_guest_powerdown(&socket).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            ["qmp_capabilities", "system_powerdown"]
        );
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn qmp_path_rejects_property_injection_and_oversize() {
        assert!(qmp_server_argument(Path::new("/tmp/qmp,bad.sock")).is_err());
        let oversized = format!("/{}", "q".repeat(108));
        assert!(qmp_server_argument(Path::new(&oversized)).is_err());
        assert_eq!(
            qmp_server_argument(Path::new("/run/rustos/qmp.sock")).unwrap(),
            "unix:/run/rustos/qmp.sock,server=on,wait=off"
        );
    }
}
