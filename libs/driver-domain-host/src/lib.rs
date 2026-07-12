//! Host-owned control plane for RustOS Linux driver domains.
//!
//! The L0 host is the only authority that binds a KVM guest identity to a
//! DVM image/control contract.  This crate intentionally exposes only a
//! bounded health and inventory probe; it is not a guest-to-guest data plane.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

pub const DEFAULT_CONTROL_PORT: u32 = 40_500;
pub const MAX_CONTROL_FRAME: usize = 4 * 1024;
pub const LINUX_DVM_ROLE: &str = "linux-driver-domain";

const VMADDR_CID_ANY: u32 = u32::MAX;
const AF_VSOCK: libc::c_int = 40;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrVm {
    family: libc::sa_family_t,
    reserved: u16,
    port: u32,
    cid: u32,
    zero: [u8; 4],
}

impl SockaddrVm {
    const fn new(cid: u32, port: u32) -> Self {
        Self {
            family: AF_VSOCK as libc::sa_family_t,
            reserved: 0,
            port,
            cid,
            zero: [0; 4],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlContract {
    pub protocol: String,
    pub state: String,
    pub transport: String,
    pub authentication: String,
    pub capabilities: Vec<String>,
}

impl ControlContract {
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read DVM control contract {}", path.display()))?;
        Self::parse(&source, &path.display().to_string())
    }

    pub fn parse(source: &str, label: &str) -> Result<Self> {
        let mut values = BTreeMap::new();
        for raw_line in source.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid {label} line {line:?}"))?;
            if key.is_empty() || value.is_empty() || values.insert(key, value).is_some() {
                bail!("invalid or duplicate {label} key {key:?}");
            }
        }

        required(&values, "CONTROL_SCHEMA", "1", label)?;
        let protocol = required_value(&values, "CONTROL_PROTOCOL", label)?.to_owned();
        let state = required_value(&values, "CONTROL_STATE", label)?.to_owned();
        let transport = required_value(&values, "CONTROL_TRANSPORT", label)?.to_owned();
        let authentication = required_value(&values, "CONTROL_AUTHENTICATION", label)?.to_owned();
        let capabilities = required_value(&values, "CONTROL_CAPABILITIES", label)?
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let contract = Self {
            protocol,
            state,
            transport,
            authentication,
            capabilities,
        };
        contract.validate(label)?;
        Ok(contract)
    }

    fn validate(&self, label: &str) -> Result<()> {
        if self.protocol != "agent-v1"
            || self.state != "control"
            || self.transport != "kvm-vsock"
            || self.authentication != "kvm-host-bound"
        {
            bail!("unsupported DVM control contract {label}");
        }
        if self.capabilities != ["health", "device-inventory", "keyboard-events"] {
            bail!("unsupported DVM capabilities in {label}");
        }
        Ok(())
    }
}

/// Immutable L0 input that names exactly one DVM and one complete IOMMU group.
///
/// This is a preflight contract, not an authorization to bind VFIO or reset a
/// device. A future signed release manifest must bind this plan to a DVM image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPlan {
    pub domain_id: String,
    pub dvm_guest_cid: u32,
    pub iommu_group: u32,
    pub assigned_pci_bdfs: BTreeSet<String>,
    pub host_protected_pci_bdfs: BTreeSet<String>,
}

impl LaunchPlan {
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read hostd launch plan {}", path.display()))?;
        Self::parse(&source, &path.display().to_string())
    }

    pub fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const REQUIRED: [&str; 6] = [
            "LAUNCH_PLAN_SCHEMA",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "ASSIGNED_PCI_BDFS",
            "HOST_PROTECTED_PCI_BDFS",
        ];
        if values.len() != REQUIRED.len()
            || values.keys().any(|key| !REQUIRED.contains(&key.as_str()))
        {
            bail!("unsupported key in {label}");
        }
        if launch_plan_value(&values, "LAUNCH_PLAN_SCHEMA", label)? != "1" {
            bail!("unsupported {label} schema");
        }
        let domain_id = launch_plan_value(&values, "DOMAIN_ID", label)?.to_owned();
        validate_domain_id(&domain_id, label)?;
        let dvm_guest_cid = launch_plan_value(&values, "DVM_GUEST_CID", label)?
            .parse::<u32>()
            .context("invalid DVM_GUEST_CID")?;
        if dvm_guest_cid <= 2 {
            bail!("invalid {label} DVM_GUEST_CID");
        }
        let iommu_group = launch_plan_value(&values, "IOMMU_GROUP", label)?
            .parse::<u32>()
            .context("invalid IOMMU_GROUP")?;
        let assigned_pci_bdfs = parse_pci_bdf_list(
            launch_plan_value(&values, "ASSIGNED_PCI_BDFS", label)?,
            false,
            label,
        )?;
        let host_protected_pci_bdfs = parse_pci_bdf_list(
            launch_plan_value(&values, "HOST_PROTECTED_PCI_BDFS", label)?,
            true,
            label,
        )?;
        Ok(Self {
            domain_id,
            dvm_guest_cid,
            iommu_group,
            assigned_pci_bdfs,
            host_protected_pci_bdfs,
        })
    }

    pub fn validate_topology(&self, topology: &IommuTopology) -> Result<ValidatedLease> {
        let actual = topology
            .groups
            .get(&self.iommu_group)
            .ok_or_else(|| anyhow!("IOMMU group {} is absent", self.iommu_group))?;
        if actual != &self.assigned_pci_bdfs {
            bail!(
                "IOMMU group {} does not exactly match ASSIGNED_PCI_BDFS; plan={:?} host={:?}",
                self.iommu_group,
                self.assigned_pci_bdfs,
                actual
            );
        }
        let protected = actual
            .intersection(&self.host_protected_pci_bdfs)
            .next()
            .cloned();
        if let Some(bdf) = protected {
            bail!(
                "IOMMU group {} contains host-protected PCI function {bdf}",
                self.iommu_group
            );
        }
        Ok(ValidatedLease {
            domain_id: self.domain_id.clone(),
            dvm_guest_cid: self.dvm_guest_cid,
            iommu_group: self.iommu_group,
            pci_bdfs: actual.iter().cloned().collect(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedLease {
    pub domain_id: String,
    pub dvm_guest_cid: u32,
    pub iommu_group: u32,
    pub pci_bdfs: Vec<String>,
}

/// Snapshot of host IOMMU ownership groups beneath a configurable sysfs root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IommuTopology {
    groups: BTreeMap<u32, BTreeSet<String>>,
}

impl IommuTopology {
    pub fn discover(sysfs_root: &Path) -> Result<Self> {
        let root = sysfs_root.join("kernel/iommu_groups");
        let entries = fs::read_dir(&root)
            .with_context(|| format!("read host IOMMU groups {}", root.display()))?;
        let mut groups = BTreeMap::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let group_name = entry.file_name();
            let group_name = group_name
                .to_str()
                .ok_or_else(|| anyhow!("non-UTF-8 IOMMU group name"))?;
            let group = group_name
                .parse::<u32>()
                .with_context(|| format!("invalid IOMMU group {group_name:?}"))?;
            let devices = fs::read_dir(entry.path().join("devices"))
                .with_context(|| format!("read host IOMMU group {group} devices"))?;
            let mut bdfs = BTreeSet::new();
            for device in devices {
                let bdf = device?.file_name();
                let bdf = bdf
                    .to_str()
                    .ok_or_else(|| anyhow!("non-UTF-8 PCI BDF in IOMMU group {group}"))?;
                validate_pci_bdf(bdf, "host IOMMU topology")?;
                if !bdfs.insert(bdf.to_owned()) {
                    bail!("duplicate PCI BDF {bdf} in IOMMU group {group}");
                }
            }
            if bdfs.is_empty() || groups.insert(group, bdfs).is_some() {
                bail!("invalid or duplicate host IOMMU group {group}");
            }
        }
        if groups.is_empty() {
            bail!("no host IOMMU groups found under {}", root.display());
        }
        Ok(Self { groups })
    }

    pub fn groups(&self) -> impl Iterator<Item = (u32, &BTreeSet<String>)> {
        self.groups.iter().map(|(group, bdfs)| (*group, bdfs))
    }
}

/// Durable recovery record for one whole-IOMMU-group VFIO handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VfioLeaseRecord {
    pub state: VfioLeaseState,
    pub domain_id: String,
    pub dvm_guest_cid: u32,
    pub iommu_group: u32,
    pub original_drivers: BTreeMap<String, Option<String>>,
    /// Empty means that the kernel reported no override (`(null)`).
    pub original_driver_overrides: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfioLeaseState {
    Prepared,
    Active,
}

impl VfioLeaseRecord {
    fn from_validated_lease(
        lease: &ValidatedLease,
        original_drivers: BTreeMap<String, Option<String>>,
        original_driver_overrides: BTreeMap<String, String>,
    ) -> Result<Self> {
        let expected = lease.pci_bdfs.iter().cloned().collect::<BTreeSet<_>>();
        if original_drivers.keys().cloned().collect::<BTreeSet<_>>() != expected {
            bail!("VFIO lease original-driver snapshot does not match the validated IOMMU group");
        }
        if original_driver_overrides
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected
        {
            bail!("VFIO lease driver-override snapshot does not match the validated IOMMU group");
        }
        Ok(Self {
            state: VfioLeaseState::Prepared,
            domain_id: lease.domain_id.clone(),
            dvm_guest_cid: lease.dvm_guest_cid,
            iommu_group: lease.iommu_group,
            original_drivers,
            original_driver_overrides,
        })
    }

    fn to_env(&self) -> String {
        let state = match self.state {
            VfioLeaseState::Prepared => "prepared",
            VfioLeaseState::Active => "active",
        };
        let drivers = self
            .original_drivers
            .iter()
            .map(|(bdf, driver)| format!("{bdf}@{}", driver.as_deref().unwrap_or("none")))
            .collect::<Vec<_>>()
            .join(",");
        let overrides = self
            .original_driver_overrides
            .iter()
            .map(|(bdf, driver)| {
                format!("{bdf}@{}", if driver.is_empty() { "none" } else { driver })
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "VFIO_LEASE_SCHEMA=1\nLEASE_STATE={state}\nDOMAIN_ID={}\nDVM_GUEST_CID={}\nIOMMU_GROUP={}\nORIGINAL_DRIVERS={drivers}\nORIGINAL_DRIVER_OVERRIDES={overrides}\n",
            self.domain_id, self.dvm_guest_cid, self.iommu_group
        )
    }

    fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const REQUIRED: [&str; 7] = [
            "VFIO_LEASE_SCHEMA",
            "LEASE_STATE",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "ORIGINAL_DRIVERS",
            "ORIGINAL_DRIVER_OVERRIDES",
        ];
        if values.len() != REQUIRED.len()
            || values.keys().any(|key| !REQUIRED.contains(&key.as_str()))
        {
            bail!("unsupported key in {label}");
        }
        if launch_plan_value(&values, "VFIO_LEASE_SCHEMA", label)? != "1" {
            bail!("unsupported {label} schema");
        }
        let state = match launch_plan_value(&values, "LEASE_STATE", label)? {
            "prepared" => VfioLeaseState::Prepared,
            "active" => VfioLeaseState::Active,
            _ => bail!("invalid {label} LEASE_STATE"),
        };
        let domain_id = launch_plan_value(&values, "DOMAIN_ID", label)?.to_owned();
        validate_domain_id(&domain_id, label)?;
        let dvm_guest_cid = launch_plan_value(&values, "DVM_GUEST_CID", label)?
            .parse::<u32>()
            .context("invalid DVM_GUEST_CID")?;
        if dvm_guest_cid <= 2 {
            bail!("invalid {label} DVM_GUEST_CID");
        }
        let iommu_group = launch_plan_value(&values, "IOMMU_GROUP", label)?
            .parse::<u32>()
            .context("invalid IOMMU_GROUP")?;
        let mut original_drivers = BTreeMap::new();
        for entry in launch_plan_value(&values, "ORIGINAL_DRIVERS", label)?.split(',') {
            let (bdf, driver) = entry
                .split_once('@')
                .ok_or_else(|| anyhow!("invalid {label} ORIGINAL_DRIVERS entry"))?;
            validate_pci_bdf(bdf, label)?;
            let driver = if driver == "none" {
                None
            } else {
                validate_driver_name(driver, label)?;
                Some(driver.to_owned())
            };
            if original_drivers.insert(bdf.to_owned(), driver).is_some() {
                bail!("duplicate PCI BDF {bdf:?} in {label} ORIGINAL_DRIVERS");
            }
        }
        if original_drivers.is_empty() {
            bail!("empty {label} ORIGINAL_DRIVERS");
        }
        let mut original_driver_overrides = BTreeMap::new();
        for entry in launch_plan_value(&values, "ORIGINAL_DRIVER_OVERRIDES", label)?.split(',') {
            let (bdf, driver) = entry
                .split_once('@')
                .ok_or_else(|| anyhow!("invalid {label} ORIGINAL_DRIVER_OVERRIDES entry"))?;
            validate_pci_bdf(bdf, label)?;
            let driver = if driver == "none" {
                String::new()
            } else {
                validate_driver_name(driver, label)?;
                driver.to_owned()
            };
            if original_driver_overrides
                .insert(bdf.to_owned(), driver)
                .is_some()
            {
                bail!("duplicate PCI BDF {bdf:?} in {label} ORIGINAL_DRIVER_OVERRIDES");
            }
        }
        if original_driver_overrides.is_empty()
            || original_driver_overrides.keys().collect::<BTreeSet<_>>()
                != original_drivers.keys().collect::<BTreeSet<_>>()
        {
            bail!("{label} driver-override snapshot does not match original drivers");
        }
        Ok(Self {
            state,
            domain_id,
            dvm_guest_cid,
            iommu_group,
            original_drivers,
            original_driver_overrides,
        })
    }
}

/// Persistent root-owned state store. A `prepared` record is intentionally
/// durable before the first sysfs driver mutation, so a crash cannot orphan a
/// device without its original-driver recovery information.
#[derive(Clone, Debug)]
pub struct FileLeaseStore {
    root: std::path::PathBuf,
}

impl FileLeaseStore {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn create_prepared(&self, record: &VfioLeaseRecord) -> Result<()> {
        if record.state != VfioLeaseState::Prepared {
            bail!("only a prepared VFIO lease can be created");
        }
        self.ensure_private_root()?;
        write_new_private(&self.path_for(&record.domain_id)?, &record.to_env())?;
        sync_directory(&self.root)
    }

    pub fn mark_active(&self, record: &mut VfioLeaseRecord) -> Result<()> {
        if record.state != VfioLeaseState::Prepared {
            bail!("only a prepared VFIO lease can become active");
        }
        self.ensure_private_root()?;
        let path = self.path_for(&record.domain_id)?;
        if !path.is_file() {
            bail!("missing prepared VFIO lease {}", path.display());
        }
        record.state = VfioLeaseState::Active;
        if let Err(error) = replace_private(&path, &record.to_env()) {
            record.state = VfioLeaseState::Prepared;
            return Err(error);
        }
        sync_directory(&self.root)?;
        Ok(())
    }

    pub fn load(&self, domain_id: &str) -> Result<VfioLeaseRecord> {
        validate_domain_id(domain_id, "VFIO lease lookup")?;
        self.ensure_private_root()?;
        let path = self.path_for(domain_id)?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect VFIO lease {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("VFIO lease {} is not a regular file", path.display());
        }
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            bail!(
                "VFIO lease {} has unsafe ownership or permissions",
                path.display()
            );
        }
        let source = fs::read_to_string(&path)
            .with_context(|| format!("read VFIO lease {}", path.display()))?;
        VfioLeaseRecord::parse(&source, &path.display().to_string())
    }

    pub fn remove(&self, domain_id: &str) -> Result<()> {
        self.ensure_private_root()?;
        let path = self.path_for(domain_id)?;
        fs::remove_file(&path).with_context(|| format!("remove VFIO lease {}", path.display()))?;
        sync_directory(&self.root)
    }

    fn path_for(&self, domain_id: &str) -> Result<std::path::PathBuf> {
        validate_domain_id(domain_id, "VFIO lease")?;
        Ok(self.root.join(format!("{domain_id}.env")))
    }

    fn ensure_private_root(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create VFIO lease directory {}", self.root.display()))?;
        let metadata = fs::symlink_metadata(&self.root)
            .with_context(|| format!("inspect VFIO lease directory {}", self.root.display()))?;
        if !metadata.file_type().is_dir() {
            bail!("VFIO lease path {} is not a directory", self.root.display());
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            bail!(
                "VFIO lease directory {} has an unexpected owner",
                self.root.display()
            );
        }
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict VFIO lease directory {}", self.root.display()))?;
        sync_directory(&self.root)
    }
}

pub trait VfioOps {
    fn vfio_driver_present(&self) -> Result<bool>;
    fn current_driver(&self, bdf: &str) -> Result<Option<String>>;
    /// Returns an empty string when sysfs reports no driver override.
    fn current_driver_override(&self, bdf: &str) -> Result<String>;
    fn set_driver_override(&mut self, bdf: &str, driver: &str) -> Result<()>;
    fn clear_driver_override(&mut self, bdf: &str) -> Result<()>;
    fn unbind_driver(&mut self, bdf: &str, driver: &str) -> Result<()>;
    fn bind_driver(&mut self, bdf: &str, driver: &str) -> Result<()>;
}

/// Real L0 sysfs implementation. Constructing this object is read-only;
/// mutations occur only through `acquire_vfio_lease`/`restore_vfio_lease`.
#[derive(Clone, Debug)]
pub struct SysfsVfioOps {
    sysfs_root: std::path::PathBuf,
}

impl SysfsVfioOps {
    pub fn new(sysfs_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            sysfs_root: sysfs_root.into(),
        }
    }

    fn device_path(&self, bdf: &str) -> std::path::PathBuf {
        self.sysfs_root.join("bus/pci/devices").join(bdf)
    }

    fn driver_path(&self, driver: &str) -> std::path::PathBuf {
        self.sysfs_root.join("bus/pci/drivers").join(driver)
    }

    fn write_device_attr(&self, bdf: &str, attribute: &str, value: &str) -> Result<()> {
        fs::write(self.device_path(bdf).join(attribute), value)
            .with_context(|| format!("write PCI {bdf} {attribute}"))
    }

    fn write_driver_attr(&self, driver: &str, attribute: &str, bdf: &str) -> Result<()> {
        validate_driver_name(driver, "sysfs driver")?;
        fs::write(self.driver_path(driver).join(attribute), format!("{bdf}\n"))
            .with_context(|| format!("write PCI driver {driver} {attribute} for {bdf}"))
    }
}

impl VfioOps for SysfsVfioOps {
    fn vfio_driver_present(&self) -> Result<bool> {
        Ok(self.driver_path("vfio-pci").is_dir())
    }

    fn current_driver(&self, bdf: &str) -> Result<Option<String>> {
        validate_pci_bdf(bdf, "sysfs query")?;
        match fs::read_link(self.device_path(bdf).join("driver")) {
            Ok(path) => {
                let driver = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("invalid PCI driver symlink for {bdf}"))?;
                validate_driver_name(driver, "sysfs query")?;
                Ok(Some(driver.to_owned()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read PCI driver for {bdf}")),
        }
    }

    fn current_driver_override(&self, bdf: &str) -> Result<String> {
        validate_pci_bdf(bdf, "sysfs query")?;
        let value = fs::read_to_string(self.device_path(bdf).join("driver_override"))
            .with_context(|| format!("read PCI driver_override for {bdf}"))?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value == "(null)" {
            return Ok(String::new());
        }
        validate_driver_name(value, "sysfs driver_override")?;
        Ok(value.to_owned())
    }

    fn set_driver_override(&mut self, bdf: &str, driver: &str) -> Result<()> {
        validate_driver_name(driver, "driver override")?;
        self.write_device_attr(bdf, "driver_override", &format!("{driver}\n"))
    }

    fn clear_driver_override(&mut self, bdf: &str) -> Result<()> {
        self.write_device_attr(bdf, "driver_override", "\n")
    }

    fn unbind_driver(&mut self, bdf: &str, driver: &str) -> Result<()> {
        self.write_driver_attr(driver, "unbind", bdf)
    }

    fn bind_driver(&mut self, bdf: &str, driver: &str) -> Result<()> {
        self.write_driver_attr(driver, "bind", bdf)
    }
}

pub fn inspect_vfio_lease(lease: &ValidatedLease, ops: &impl VfioOps) -> Result<VfioLeaseRecord> {
    let mut original_drivers = BTreeMap::new();
    let mut original_driver_overrides = BTreeMap::new();
    for bdf in &lease.pci_bdfs {
        let driver = ops.current_driver(bdf)?;
        if driver.as_deref() == Some("vfio-pci") {
            bail!("refusing to adopt already-VFIO-bound PCI device {bdf} without a durable lease");
        }
        original_drivers.insert(bdf.clone(), driver);
        original_driver_overrides.insert(bdf.clone(), ops.current_driver_override(bdf)?);
    }
    VfioLeaseRecord::from_validated_lease(lease, original_drivers, original_driver_overrides)
}

/// Acquire the complete validated IOMMU group. On every failure this function
/// attempts a reverse-order rollback to the record's original host drivers.
pub fn acquire_vfio_lease(record: &VfioLeaseRecord, ops: &mut impl VfioOps) -> Result<()> {
    if record.state != VfioLeaseState::Prepared {
        bail!("VFIO acquire requires a prepared lease");
    }
    if !ops.vfio_driver_present()? {
        bail!("vfio-pci is not loaded on L0");
    }
    let mut touched = Vec::new();
    for (bdf, original_driver) in &record.original_drivers {
        let current = ops.current_driver(bdf)?;
        if current.as_deref() != original_driver.as_deref() {
            let rollback = rollback_vfio_lease(record, ops, &touched);
            return match rollback {
                Ok(()) => Err(anyhow!("PCI driver changed before VFIO acquire for {bdf}")),
                Err(rollback_error) => Err(anyhow!(
                    "PCI driver changed before VFIO acquire for {bdf}; rollback failed: {rollback_error:#}"
                )),
            };
        }
        touched.push(bdf.as_str());
        let result = (|| {
            ops.set_driver_override(bdf, "vfio-pci")?;
            if let Some(driver) = original_driver {
                ops.unbind_driver(bdf, driver)?;
            }
            ops.bind_driver(bdf, "vfio-pci")?;
            if ops.current_driver(bdf)?.as_deref() != Some("vfio-pci") {
                bail!("vfio-pci did not bind {bdf}");
            }
            Ok(())
        })();
        if let Err(error) = result {
            let rollback = rollback_vfio_lease(record, ops, &touched);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow!(
                    "VFIO acquire failed: {error:#}; rollback failed: {rollback_error:#}"
                )),
            };
        }
    }
    Ok(())
}

/// Restore every device from either a prepared or active durable lease.
pub fn restore_vfio_lease(record: &VfioLeaseRecord, ops: &mut impl VfioOps) -> Result<()> {
    let bdfs = record
        .original_drivers
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    rollback_vfio_lease(record, ops, &bdfs)
}

fn rollback_vfio_lease(
    record: &VfioLeaseRecord,
    ops: &mut impl VfioOps,
    touched: &[&str],
) -> Result<()> {
    let mut failures = Vec::new();
    for bdf in touched.iter().rev() {
        let original_driver = record
            .original_drivers
            .get(*bdf)
            .ok_or_else(|| anyhow!("rollback missing original driver for {bdf}"))?;
        let original_override = record
            .original_driver_overrides
            .get(*bdf)
            .ok_or_else(|| anyhow!("rollback missing original driver_override for {bdf}"))?;
        let result = (|| {
            let needs_rebind = match ops.current_driver(bdf)?.as_deref() {
                Some("vfio-pci") => {
                    ops.unbind_driver(bdf, "vfio-pci")?;
                    true
                }
                Some(driver) if Some(driver) == original_driver.as_deref() => false,
                Some(driver) => bail!("unexpected PCI driver {driver} while restoring {bdf}"),
                None => original_driver.is_some(),
            };
            // Clear the VFIO override before explicitly re-binding the original
            // driver: an inherited nonmatching override would block that bind.
            ops.clear_driver_override(bdf)?;
            if needs_rebind {
                if let Some(driver) = original_driver {
                    ops.bind_driver(bdf, driver)?;
                }
            }
            if ops.current_driver(bdf)? != *original_driver {
                bail!("failed to restore original driver for {bdf}");
            }
            if !original_override.is_empty() {
                ops.set_driver_override(bdf, original_override)?;
            }
            if ops.current_driver_override(bdf)? != *original_override {
                bail!("failed to restore original driver_override for {bdf}");
            }
            Ok(())
        })();
        if let Err(error) = result {
            failures.push(format!("{bdf}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("VFIO rollback incomplete: {}", failures.join("; "));
    }
}

fn write_new_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create VFIO lease {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn replace_private(path: &Path, contents: &str) -> Result<()> {
    let temporary = path.with_extension("next");
    write_new_private(&temporary, contents)?;
    fs::rename(&temporary, path).with_context(|| format!("activate VFIO lease {}", path.display()))
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("open VFIO lease directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync VFIO lease directory {}", path.display()))
}

fn parse_launch_plan_values(source: &str, label: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid {label} line {line:?}"))?;
        if key.is_empty()
            || value.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
            || values.insert(key.to_owned(), value.to_owned()).is_some()
        {
            bail!("invalid or duplicate {label} key {key:?}");
        }
    }
    Ok(values)
}

fn launch_plan_value<'a>(
    values: &'a BTreeMap<String, String>,
    key: &str,
    label: &str,
) -> Result<&'a str> {
    values
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing {label} key {key}"))
}

fn parse_pci_bdf_list(value: &str, allow_none: bool, label: &str) -> Result<BTreeSet<String>> {
    if allow_none && value == "none" {
        return Ok(BTreeSet::new());
    }
    let mut bdfs = BTreeSet::new();
    for bdf in value.split(',') {
        validate_pci_bdf(bdf, label)?;
        if !bdfs.insert(bdf.to_owned()) {
            bail!("duplicate PCI BDF {bdf:?} in {label}");
        }
    }
    if bdfs.is_empty() {
        bail!("empty PCI BDF list in {label}");
    }
    Ok(bdfs)
}

fn validate_pci_bdf(bdf: &str, label: &str) -> Result<()> {
    let bytes = bdf.as_bytes();
    let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    if bytes.len() != 12
        || bytes[4] != b':'
        || bytes[7] != b':'
        || bytes[10] != b'.'
        || ![0, 1, 2, 3, 5, 6, 8, 9, 11]
            .into_iter()
            .all(|index| hex(bytes[index]))
    {
        bail!("invalid PCI BDF {bdf:?} in {label}");
    }
    Ok(())
}

fn validate_domain_id(domain_id: &str, label: &str) -> Result<()> {
    if domain_id.is_empty()
        || domain_id.len() > 64
        || !domain_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid {label} DOMAIN_ID");
    }
    Ok(())
}

fn validate_driver_name(driver: &str, label: &str) -> Result<()> {
    if driver.is_empty()
        || driver.len() > 64
        || !driver.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        bail!("invalid PCI driver {driver:?} in {label}");
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeResult {
    pub peer_cid: u32,
    pub inventory_count: u32,
}

/// One Linux evdev key press reported by the launch-bound DVM.
///
/// This is deliberately a bounded control-plane event, not a general device
/// data plane. The first KVM smoke accepts only the exact key it injected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardEvent {
    pub linux_key_code: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardProbeResult {
    pub probe: ProbeResult,
    pub event: KeyboardEvent,
}

/// A listener bound by L0 before a DVM is started.
///
/// An accepted source CID is necessary but not sufficient identity: the hello
/// frame must also match the launch-bound DVM contract.
#[derive(Debug)]
pub struct HostControlListener {
    fd: OwnedFd,
    expected_dvm_cid: u32,
    contract: ControlContract,
}

impl HostControlListener {
    pub fn bind(expected_dvm_cid: u32, port: u32, contract: ControlContract) -> Result<Self> {
        if expected_dvm_cid <= 2 || port == 0 {
            bail!("invalid DVM vsock identity cid={expected_dvm_cid} port={port}");
        }
        let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("create host AF_VSOCK listener");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = SockaddrVm::new(VMADDR_CID_ANY, port);
        let bind_result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };
        if bind_result != 0 {
            return Err(std::io::Error::last_os_error()).context("bind host AF_VSOCK listener");
        }
        if unsafe { libc::listen(fd.as_raw_fd(), 4) } != 0 {
            return Err(std::io::Error::last_os_error()).context("listen on host AF_VSOCK");
        }
        Ok(Self {
            fd,
            expected_dvm_cid,
            contract,
        })
    }

    pub fn probe_once(&self, timeout: Duration) -> Result<ProbeResult> {
        let mut connection = self.accept_authenticated(timeout)?;
        self.probe_connection(&mut connection)
    }

    /// Request one Linux evdev key press after the caller has injected the
    /// matching test key into the DVM. The callback runs only after the
    /// authenticated DVM has accepted the bounded request.
    pub fn keyboard_probe_once<F>(
        &self,
        timeout: Duration,
        inject_test_key: F,
    ) -> Result<KeyboardProbeResult>
    where
        F: FnOnce() -> Result<()>,
    {
        let mut connection = self.accept_authenticated(timeout)?;
        let probe = self.probe_connection(&mut connection)?;
        let event = request_after_ready(&mut connection, 3, "keyboard-event", inject_test_key)?;
        if !has_exact_fields(&event, &["id", "op", "status", "type", "code", "value"])
            || event.get("status") != Some(&"ok".to_owned())
            || event.get("type") != Some(&"key".to_owned())
            || event.get("value") != Some(&"1".to_owned())
        {
            bail!("Linux DVM keyboard probe returned an invalid key event");
        }
        let linux_key_code = event
            .get("code")
            .ok_or_else(|| anyhow!("Linux DVM keyboard probe omitted key code"))?
            .parse::<u16>()
            .context("invalid Linux DVM keyboard key code")?;
        if linux_key_code == 0 {
            bail!("Linux DVM keyboard probe reported reserved key code zero");
        }
        Ok(KeyboardProbeResult {
            probe,
            event: KeyboardEvent { linux_key_code },
        })
    }

    fn accept_authenticated(&self, timeout: Duration) -> Result<std::fs::File> {
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready == 0 {
            bail!("timed out waiting for Linux DVM vsock control connection");
        }
        if ready < 0 {
            return Err(std::io::Error::last_os_error()).context("poll host AF_VSOCK listener");
        }
        let mut peer = SockaddrVm::new(0, 0);
        let mut peer_len = size_of::<SockaddrVm>() as libc::socklen_t;
        let connection = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                (&raw mut peer).cast::<libc::sockaddr>(),
                &raw mut peer_len,
                libc::SOCK_CLOEXEC,
            )
        };
        if connection < 0 {
            return Err(std::io::Error::last_os_error())
                .context("accept Linux DVM vsock connection");
        }
        let mut connection = unsafe { std::fs::File::from_raw_fd(connection) };
        configure_socket_timeout(&connection, timeout)?;
        if peer.family != AF_VSOCK as libc::sa_family_t || peer.cid != self.expected_dvm_cid {
            bail!(
                "rejected DVM vsock peer cid={} expected={}",
                peer.cid,
                self.expected_dvm_cid
            );
        }
        let hello = read_message(&mut connection)?;
        validate_hello(&hello, &self.contract)?;
        write_message(&mut connection, &welcome_message(&self.contract))?;
        Ok(connection)
    }

    fn probe_connection(&self, connection: &mut std::fs::File) -> Result<ProbeResult> {
        let health = request(connection, 1, "health")?;
        if !has_exact_fields(&health, &["id", "op", "status", "value"])
            || health.get("status") != Some(&"ok".to_owned())
            || health.get("value") != Some(&"ready".to_owned())
        {
            bail!("Linux DVM health probe was not ready");
        }
        let inventory = request(connection, 2, "device-inventory")?;
        if !has_exact_fields(&inventory, &["id", "op", "status", "count"])
            || inventory.get("status") != Some(&"ok".to_owned())
        {
            bail!("Linux DVM inventory probe was not ready");
        }
        let inventory_count = inventory
            .get("count")
            .ok_or_else(|| anyhow!("Linux DVM inventory response omitted count"))?
            .parse::<u32>()
            .context("invalid Linux DVM inventory count")?;
        Ok(ProbeResult {
            peer_cid: self.expected_dvm_cid,
            inventory_count,
        })
    }
}

fn required<'a>(
    values: &'a BTreeMap<&str, &str>,
    key: &str,
    expected: &str,
    label: &str,
) -> Result<()> {
    if required_value(values, key, label)? != expected {
        bail!("unsupported {label} {key}");
    }
    Ok(())
}

fn required_value<'a>(values: &'a BTreeMap<&str, &str>, key: &str, label: &str) -> Result<&'a str> {
    values
        .get(key)
        .copied()
        .ok_or_else(|| anyhow!("missing {label} key {key}"))
}

fn configure_socket_timeout(connection: &std::fs::File, timeout: Duration) -> Result<()> {
    let timeout = libc::timeval {
        tv_sec: timeout.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
        tv_usec: timeout.subsec_micros().into(),
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        let result = unsafe {
            libc::setsockopt(
                connection.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&raw const timeout).cast(),
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("configure DVM control socket timeout");
        }
    }
    Ok(())
}

fn request(
    connection: &mut std::fs::File,
    id: u32,
    operation: &str,
) -> Result<BTreeMap<String, String>> {
    write_message(connection, &format!("REQUEST\nid={id}\nop={operation}"))?;
    let response = parse_message(&read_message(connection)?)?;
    if response.kind != "RESPONSE"
        || response.fields.get("id") != Some(&id.to_string())
        || response.fields.get("op") != Some(&operation.to_owned())
    {
        bail!("mismatched DVM control response");
    }
    Ok(response.fields)
}

fn request_after_ready<F>(
    connection: &mut std::fs::File,
    id: u32,
    operation: &str,
    inject_test_key: F,
) -> Result<BTreeMap<String, String>>
where
    F: FnOnce() -> Result<()>,
{
    write_message(connection, &format!("REQUEST\nid={id}\nop={operation}"))?;
    let ready = parse_message(&read_message(connection)?)?;
    validate_keyboard_ready(&ready, id, operation)?;
    inject_test_key()?;
    let response = parse_message(&read_message(connection)?)?;
    if response.kind != "RESPONSE"
        || response.fields.get("id") != Some(&id.to_string())
        || response.fields.get("op") != Some(&operation.to_owned())
    {
        bail!("mismatched DVM control response");
    }
    Ok(response.fields)
}

fn validate_keyboard_ready(message: &Message, id: u32, operation: &str) -> Result<()> {
    if message.kind != "READY"
        || !has_exact_fields(&message.fields, &["id", "op", "status"])
        || message.fields.get("id") != Some(&id.to_string())
        || message.fields.get("op") != Some(&operation.to_owned())
        || message.fields.get("status") != Some(&"ready".to_owned())
    {
        bail!(
            "Linux DVM did not acknowledge keyboard event readiness: kind={} fields={:?}",
            message.kind,
            message.fields
        );
    }
    Ok(())
}

fn welcome_message(contract: &ControlContract) -> String {
    format!(
        "WELCOME\nprotocol={}\ncapabilities={}",
        contract.protocol,
        contract.capabilities.join(",")
    )
}

fn has_exact_fields(fields: &BTreeMap<String, String>, expected: &[&str]) -> bool {
    fields.len() == expected.len() && expected.iter().all(|key| fields.contains_key(*key))
}

fn validate_hello(source: &str, contract: &ControlContract) -> Result<()> {
    let hello = parse_message(source)?;
    if hello.kind != "HELLO"
        || !has_exact_fields(
            &hello.fields,
            &[
                "role",
                "protocol",
                "state",
                "transport",
                "authentication",
                "capabilities",
            ],
        )
        || hello.fields.get("role") != Some(&LINUX_DVM_ROLE.to_owned())
        || hello.fields.get("protocol") != Some(&contract.protocol)
        || hello.fields.get("state") != Some(&contract.state)
        || hello.fields.get("transport") != Some(&contract.transport)
        || hello.fields.get("authentication") != Some(&contract.authentication)
        || hello.fields.get("capabilities") != Some(&contract.capabilities.join(","))
    {
        bail!("rejected Linux DVM control hello");
    }
    Ok(())
}

struct Message {
    kind: String,
    fields: BTreeMap<String, String>,
}

fn parse_message(source: &str) -> Result<Message> {
    let mut lines = source.lines();
    let kind = lines
        .next()
        .filter(|kind| {
            !kind.is_empty()
                && kind
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        })
        .ok_or_else(|| anyhow!("missing or invalid control message type"))?
        .to_owned();
    let mut fields = BTreeMap::new();
    for line in lines {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid control message field"))?;
        if key.is_empty()
            || value.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || fields.insert(key.to_owned(), value.to_owned()).is_some()
        {
            bail!("invalid or duplicate control message field");
        }
    }
    Ok(Message { kind, fields })
}

fn read_message(connection: &mut std::fs::File) -> Result<String> {
    let mut length = [0_u8; 4];
    connection
        .read_exact(&mut length)
        .context("read DVM control frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_CONTROL_FRAME {
        bail!("invalid DVM control frame length {length}");
    }
    let mut payload = vec![0_u8; length];
    connection
        .read_exact(&mut payload)
        .context("read DVM control frame payload")?;
    String::from_utf8(payload).context("DVM control frame is not UTF-8")
}

fn write_message(connection: &mut std::fs::File, message: &str) -> Result<()> {
    if message.is_empty() || message.len() > MAX_CONTROL_FRAME {
        bail!("invalid outbound DVM control frame length");
    }
    connection
        .write_all(&(message.len() as u32).to_be_bytes())
        .and_then(|()| connection.write_all(message.as_bytes()))
        .and_then(|()| connection.flush())
        .context("write DVM control frame")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        ControlContract, FileLeaseStore, IommuTopology, LaunchPlan, ValidatedLease,
        VfioLeaseRecord, VfioLeaseState, VfioOps, acquire_vfio_lease, inspect_vfio_lease,
        parse_message, restore_vfio_lease, validate_hello, validate_keyboard_ready,
    };
    use anyhow::Result;

    const VALID: &str = "CONTROL_SCHEMA=1\nCONTROL_PROTOCOL=agent-v1\nCONTROL_STATE=control\nCONTROL_TRANSPORT=kvm-vsock\nCONTROL_AUTHENTICATION=kvm-host-bound\nCONTROL_CAPABILITIES=health,device-inventory,keyboard-events\n";

    #[test]
    fn control_contract_is_strict() {
        let contract = ControlContract::parse(VALID, "test").unwrap();
        assert_eq!(
            contract.capabilities,
            ["health", "device-inventory", "keyboard-events"]
        );
        assert!(ControlContract::parse(&VALID.replace("control", "pretransport"), "test").is_err());
        assert!(
            ControlContract::parse(&VALID.replace("device-inventory", "network-rx"), "test")
                .is_err()
        );
    }

    #[test]
    fn hello_is_bound_to_the_exact_control_contract() {
        let contract = ControlContract::parse(VALID, "test").unwrap();
        let hello = "HELLO\nrole=linux-driver-domain\nprotocol=agent-v1\nstate=control\ntransport=kvm-vsock\nauthentication=kvm-host-bound\ncapabilities=health,device-inventory,keyboard-events";
        assert!(validate_hello(hello, &contract).is_ok());
        assert!(
            validate_hello(
                &hello.replace("health,device-inventory,keyboard-events", "health"),
                &contract
            )
            .is_err()
        );
        assert!(validate_hello(&format!("{hello}\nextra=unexpected"), &contract).is_err());
    }

    #[test]
    fn control_messages_reject_duplicate_fields() {
        assert!(
            parse_message("HELLO\nrole=linux-driver-domain\nrole=linux-driver-domain").is_err()
        );
    }

    #[test]
    fn keyboard_probe_requires_exact_ready_acknowledgement() {
        let ready = parse_message("READY\nid=3\nop=keyboard-event\nstatus=ready").unwrap();
        assert!(validate_keyboard_ready(&ready, 3, "keyboard-event").is_ok());

        let wrong_status =
            parse_message("READY\nid=3\nop=keyboard-event\nstatus=accepted").unwrap();
        assert!(validate_keyboard_ready(&wrong_status, 3, "keyboard-event").is_err());

        let extra =
            parse_message("READY\nid=3\nop=keyboard-event\nstatus=ready\nextra=no").unwrap();
        assert!(validate_keyboard_ready(&extra, 3, "keyboard-event").is_err());
    }

    #[test]
    fn launch_plan_requires_the_complete_iommu_group() {
        let sysfs = TestSysfs::new(&[(17, &["0000:04:00.0", "0000:04:00.1"])]);
        let plan = LaunchPlan::parse(
            "LAUNCH_PLAN_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=17\nASSIGNED_PCI_BDFS=0000:04:00.0,0000:04:00.1\nHOST_PROTECTED_PCI_BDFS=none\n",
            "test",
        )
        .unwrap();
        let topology = IommuTopology::discover(sysfs.path()).unwrap();
        let lease = plan.validate_topology(&topology).unwrap();
        assert_eq!(lease.iommu_group, 17);
        assert_eq!(lease.pci_bdfs, ["0000:04:00.0", "0000:04:00.1"]);

        let incomplete = LaunchPlan::parse(
            "LAUNCH_PLAN_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=17\nASSIGNED_PCI_BDFS=0000:04:00.0\nHOST_PROTECTED_PCI_BDFS=none\n",
            "test",
        )
        .unwrap();
        assert!(incomplete.validate_topology(&topology).is_err());
    }

    #[test]
    fn launch_plan_rejects_host_protected_device() {
        let sysfs = TestSysfs::new(&[(4, &["0000:08:00.0"])]);
        let plan = LaunchPlan::parse(
            "LAUNCH_PLAN_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=4\nASSIGNED_PCI_BDFS=0000:08:00.0\nHOST_PROTECTED_PCI_BDFS=0000:08:00.0\n",
            "test",
        )
        .unwrap();
        let topology = IommuTopology::discover(sysfs.path()).unwrap();
        assert!(plan.validate_topology(&topology).is_err());
    }

    #[test]
    fn vfio_acquire_and_restore_are_transactional() {
        let lease = validated_lease(&["0000:02:00.0", "0000:02:00.1"]);
        let mut ops = FakeVfioOps::with_drivers(&[
            ("0000:02:00.0", Some("first-driver")),
            ("0000:02:00.1", Some("second-driver")),
        ]);
        ops.overrides
            .insert("0000:02:00.0".to_owned(), "other-driver".to_owned());
        let record = inspect_vfio_lease(&lease, &ops).unwrap();
        acquire_vfio_lease(&record, &mut ops).unwrap();
        assert_eq!(
            ops.current_driver("0000:02:00.0").unwrap().as_deref(),
            Some("vfio-pci")
        );
        assert_eq!(
            ops.current_driver("0000:02:00.1").unwrap().as_deref(),
            Some("vfio-pci")
        );
        restore_vfio_lease(&record, &mut ops).unwrap();
        assert_eq!(
            ops.current_driver("0000:02:00.0").unwrap().as_deref(),
            Some("first-driver")
        );
        assert_eq!(
            ops.current_driver("0000:02:00.1").unwrap().as_deref(),
            Some("second-driver")
        );
        assert_eq!(
            ops.current_driver_override("0000:02:00.0").unwrap(),
            "other-driver"
        );
        assert_eq!(ops.current_driver_override("0000:02:00.1").unwrap(), "");
    }

    #[test]
    fn vfio_acquire_failure_restores_every_touched_device() {
        let lease = validated_lease(&["0000:02:00.0", "0000:02:00.1"]);
        let mut ops = FakeVfioOps::with_drivers(&[
            ("0000:02:00.0", Some("first-driver")),
            ("0000:02:00.1", Some("second-driver")),
        ]);
        ops.fail_bind = Some(("0000:02:00.1".to_owned(), "vfio-pci".to_owned()));
        let record = inspect_vfio_lease(&lease, &ops).unwrap();
        assert!(acquire_vfio_lease(&record, &mut ops).is_err());
        assert_eq!(
            ops.current_driver("0000:02:00.0").unwrap().as_deref(),
            Some("first-driver")
        );
        assert_eq!(
            ops.current_driver("0000:02:00.1").unwrap().as_deref(),
            Some("second-driver")
        );
        assert!(ops.overrides.is_empty());
    }

    #[test]
    fn prepared_lease_is_durable_before_activation() {
        let root = TestSysfs::new(&[]);
        let store = FileLeaseStore::new(root.path().join("leases"));
        let mut record = VfioLeaseRecord {
            state: VfioLeaseState::Prepared,
            domain_id: "linux-dvm-net0".to_owned(),
            dvm_guest_cid: 4,
            iommu_group: 15,
            original_drivers: BTreeMap::from([(
                "0000:02:00.0".to_owned(),
                Some("rtsx-pci".to_owned()),
            )]),
            original_driver_overrides: BTreeMap::from([("0000:02:00.0".to_owned(), String::new())]),
        };
        store.create_prepared(&record).unwrap();
        assert_eq!(
            store.load("linux-dvm-net0").unwrap().state,
            VfioLeaseState::Prepared
        );
        store.mark_active(&mut record).unwrap();
        assert_eq!(
            store.load("linux-dvm-net0").unwrap().state,
            VfioLeaseState::Active
        );
        store.remove("linux-dvm-net0").unwrap();
    }

    fn validated_lease(bdfs: &[&str]) -> ValidatedLease {
        ValidatedLease {
            domain_id: "linux-dvm-net0".to_owned(),
            dvm_guest_cid: 4,
            iommu_group: 15,
            pci_bdfs: bdfs.iter().map(|bdf| (*bdf).to_owned()).collect(),
        }
    }

    struct FakeVfioOps {
        vfio_present: bool,
        drivers: BTreeMap<String, Option<String>>,
        overrides: BTreeMap<String, String>,
        fail_bind: Option<(String, String)>,
    }

    impl FakeVfioOps {
        fn with_drivers(drivers: &[(&str, Option<&str>)]) -> Self {
            Self {
                vfio_present: true,
                drivers: drivers
                    .iter()
                    .map(|(bdf, driver)| ((*bdf).to_owned(), driver.map(str::to_owned)))
                    .collect(),
                overrides: BTreeMap::new(),
                fail_bind: None,
            }
        }
    }

    impl VfioOps for FakeVfioOps {
        fn vfio_driver_present(&self) -> Result<bool, anyhow::Error> {
            Ok(self.vfio_present)
        }

        fn current_driver(&self, bdf: &str) -> Result<Option<String>, anyhow::Error> {
            self.drivers
                .get(bdf)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown fake PCI device {bdf}"))
        }

        fn current_driver_override(&self, bdf: &str) -> Result<String, anyhow::Error> {
            if !self.drivers.contains_key(bdf) {
                anyhow::bail!("unknown fake PCI device {bdf}");
            }
            Ok(self.overrides.get(bdf).cloned().unwrap_or_default())
        }

        fn set_driver_override(&mut self, bdf: &str, driver: &str) -> Result<(), anyhow::Error> {
            self.overrides.insert(bdf.to_owned(), driver.to_owned());
            Ok(())
        }

        fn clear_driver_override(&mut self, bdf: &str) -> Result<(), anyhow::Error> {
            self.overrides.remove(bdf);
            Ok(())
        }

        fn unbind_driver(&mut self, bdf: &str, driver: &str) -> Result<(), anyhow::Error> {
            if self.current_driver(bdf)?.as_deref() != Some(driver) {
                anyhow::bail!("fake driver mismatch while unbinding {bdf}");
            }
            self.drivers.insert(bdf.to_owned(), None);
            Ok(())
        }

        fn bind_driver(&mut self, bdf: &str, driver: &str) -> Result<(), anyhow::Error> {
            if self
                .fail_bind
                .as_ref()
                .is_some_and(|(fail_bdf, fail_driver)| fail_bdf == bdf && fail_driver == driver)
            {
                anyhow::bail!("injected fake bind failure for {bdf}");
            }
            if self.current_driver(bdf)?.is_some() {
                anyhow::bail!("fake PCI device {bdf} is already bound");
            }
            self.drivers.insert(bdf.to_owned(), Some(driver.to_owned()));
            Ok(())
        }
    }

    struct TestSysfs {
        root: PathBuf,
    }

    impl TestSysfs {
        fn new(groups: &[(u32, &[&str])]) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "rustos-driver-domain-host-{}-{nonce}",
                std::process::id()
            ));
            for (group, bdfs) in groups {
                let devices = root.join(format!("kernel/iommu_groups/{group}/devices"));
                fs::create_dir_all(&devices).unwrap();
                for bdf in *bdfs {
                    fs::create_dir(devices.join(bdf)).unwrap();
                }
            }
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TestSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
