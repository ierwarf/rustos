//! Host-owned control plane for RustOS Linux driver domains.
//!
//! The L0 host is the only authority that binds a KVM guest identity to a
//! DVM image/control contract plus the narrow L0-owned event relay.
//!
//! A driver domain is never allowed to address RustOS directly.  It reports a
//! versioned, allowlisted event to L0 over vsock; L0 validates it and writes a
//! fixed binary frame to RustOS's dedicated virtual transport.  High bandwidth
//! devices (network, block, GPU) need their own paravirtual backends and must
//! not be tunneled through this control relay.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

/// The DVM control listener port is derived from the owner-private per-launch
/// secret.  It is an endpoint capability, not a stable service discovery port:
/// an ordinary process sharing the DVM CID cannot hold the listener's setup
/// slot unless it can first read that root-only launch secret.
pub const CONTROL_PORT_FLOOR: u32 = 49_152;
pub const MAX_CONTROL_FRAME: usize = 4 * 1024;
pub const LINUX_DVM_ROLE: &str = "linux-driver-domain";
pub const RUSTOS_INPUT_FRAME_BYTES: usize = 32;

const RUSTOS_INPUT_MAGIC: [u8; 4] = *b"RDI1";
const RUSTOS_INPUT_VERSION: u8 = 2;
const RUSTOS_INPUT_KIND_SESSION_START: u8 = 0;
const RUSTOS_INPUT_KIND_KEY: u8 = 1;
const RUSTOS_INPUT_KIND_POINTER: u8 = 2;
const RUSTOS_INPUT_KIND_SESSION_END: u8 = 3;
const LINUX_EVDEV_KEY_MAX: u16 = 0x02ff;
const RUSTOS_POINTER_BUTTON_MASK: u8 = 0x1f;
const INPUT_RELAY_MAX_SEQUENCE: u32 = u32::MAX - 1024;
// RDI2 is carried by a bounded 115200-bps COM2 transport. The DVM coalesces
// relative pointer samples to 125Hz, and L0 still enforces the resulting
// physical transport budget against a compromised or buggy DVM.
const INPUT_RELAY_MAX_FRAMES_PER_SECOND: u32 = 256;
const INPUT_RELAY_MAX_KEYS_PER_SECOND: u32 = INPUT_RELAY_MAX_FRAMES_PER_SECOND;
const CONTROL_AUTHENTICATION: &str = "dvm-agent-hmac-sha256-v1";
const CONTROL_PROOF_CONTEXT: &[u8] = b"rustos-dvm-control-hmac-v1\0";
const CONTROL_SECRET_BYTES: usize = 32;

const VMADDR_CID_ANY: u32 = u32::MAX;
const AF_VSOCK: libc::c_int = 40;
const AF_ALG: libc::c_int = 38;
const SOL_ALG: libc::c_int = 279;
const ALG_SET_KEY: libc::c_int = 1;

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

#[repr(C)]
#[derive(Clone, Copy)]
struct SockaddrAlg {
    family: libc::sa_family_t,
    algorithm_type: [u8; 14],
    feature: u32,
    mask: u32,
    name: [u8; 64],
}

/// Per-DVM secret used only to derive the private control endpoint and prove a
/// fresh challenge on that narrow channel. It is intentionally not a release
/// authorization or a RustOS transport credential.
pub struct ControlSecret {
    bytes: [u8; CONTROL_SECRET_BYTES],
}

impl std::fmt::Debug for ControlSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ControlSecret(<redacted>)")
    }
}

impl ControlSecret {
    pub fn from_hex_file(path: &Path) -> Result<Self> {
        let mut source = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| {
                format!(
                    "open DVM control secret {} without following symlinks",
                    path.display()
                )
            })?;
        let metadata = source
            .metadata()
            .with_context(|| format!("read DVM control secret metadata {}", path.display()))?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            bail!(
                "DVM control secret {} must be a regular file owned by the current user with mode 0600 or stricter",
                path.display()
            );
        }
        let mut encoded = String::new();
        source
            .read_to_string(&mut encoded)
            .with_context(|| format!("read DVM control secret {}", path.display()))?;
        Self::from_hex(encoded.trim_end_matches(['\n', '\r']))
    }

    pub fn from_bytes(bytes: [u8; CONTROL_SECRET_BYTES]) -> Result<Self> {
        if bytes.iter().all(|byte| *byte == 0) {
            bail!("DVM control secret must not be all zero");
        }
        Ok(Self { bytes })
    }

    pub fn random() -> Result<Self> {
        let mut bytes = [0_u8; CONTROL_SECRET_BYTES];
        fs::File::open("/dev/urandom")
            .context("open /dev/urandom for DVM control secret")?
            .read_exact(&mut bytes)
            .context("read DVM control secret from /dev/urandom")?;
        Self::from_bytes(bytes)
    }

    pub fn as_hex(&self) -> String {
        encode_hex(&self.bytes)
    }

    /// Return the per-launch host-vsock endpoint capability.
    ///
    /// The mapping deliberately uses only a nonzero private-port range, and
    /// must remain byte-for-byte aligned with `rustos-dvm-agent.c`.  It is not
    /// an authorization credential: the HMAC challenge still authenticates a
    /// peer that reaches this endpoint.
    pub fn control_port(&self) -> u32 {
        let entropy = u32::from_be_bytes(self.bytes[..4].try_into().expect("control secret"));
        let span = u32::MAX - CONTROL_PORT_FLOOR + 1;
        CONTROL_PORT_FLOOR + entropy % span
    }

    fn from_hex(source: &str) -> Result<Self> {
        if source.len() != CONTROL_SECRET_BYTES * 2
            || !source.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("DVM control secret must be exactly 64 hexadecimal characters");
        }
        let mut bytes = [0_u8; CONTROL_SECRET_BYTES];
        for (index, chunk) in source.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_value(chunk[0])? << 4) | hex_value(chunk[1])?;
        }
        Self::from_bytes(bytes)
    }
}

impl Drop for ControlSecret {
    fn drop(&mut self) {
        self.bytes.fill(0);
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
            || self.authentication != CONTROL_AUTHENTICATION
        {
            bail!("unsupported DVM control contract {label}");
        }
        if self.capabilities
            != [
                "health",
                "device-inventory",
                "driver-inventory",
                "input-stream",
            ]
        {
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

/// Device classes that may be exported by a driver domain.  Adding a class to
/// the policy does not create a data plane: it only names the owner and forces
/// a future transport implementation to be explicit.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeviceClass {
    Input,
    Network,
    Block,
    Display,
}

impl DeviceClass {
    pub const fn policy_key(self) -> &'static str {
        match self {
            Self::Input => "INPUT_TRANSPORT",
            Self::Network => "NETWORK_TRANSPORT",
            Self::Block => "BLOCK_TRANSPORT",
            Self::Display => "DISPLAY_TRANSPORT",
        }
    }
}

/// Explicit DVM-to-RustOS transport selection for one device class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceTransport {
    Disabled,
    /// Fixed, sequenced RDI2 input frames over the private RustOS COM2 device.
    Rdi2Com2,
}

impl DeviceTransport {
    fn parse(value: &str, key: &str, label: &str) -> Result<Self> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "rdi2-com2" if key == DeviceClass::Input.policy_key() => Ok(Self::Rdi2Com2),
            _ => bail!("unsupported {label} {key} transport {value:?}"),
        }
    }
}

/// Immutable L0 policy that joins a validated launch plan to exactly one
/// transport per class.  It deliberately rejects a generic "proxy" value:
/// NIC, block, and display need distinct bounded protocols rather than an
/// accidental extension of the input relay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverDomainPolicy {
    pub domain_id: String,
    transports: BTreeMap<DeviceClass, DeviceTransport>,
}

impl DriverDomainPolicy {
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read driver-domain policy {}", path.display()))?;
        Self::parse(&source, &path.display().to_string())
    }

    pub fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const REQUIRED: [&str; 6] = [
            "DRIVER_DOMAIN_POLICY_SCHEMA",
            "DOMAIN_ID",
            "INPUT_TRANSPORT",
            "NETWORK_TRANSPORT",
            "BLOCK_TRANSPORT",
            "DISPLAY_TRANSPORT",
        ];
        if values.len() != REQUIRED.len()
            || values.keys().any(|key| !REQUIRED.contains(&key.as_str()))
        {
            bail!("unsupported key in {label}");
        }
        if launch_plan_value(&values, "DRIVER_DOMAIN_POLICY_SCHEMA", label)? != "1" {
            bail!("unsupported {label} schema");
        }
        let domain_id = launch_plan_value(&values, "DOMAIN_ID", label)?.to_owned();
        validate_domain_id(&domain_id, label)?;
        let mut transports = BTreeMap::new();
        for class in [
            DeviceClass::Input,
            DeviceClass::Network,
            DeviceClass::Block,
            DeviceClass::Display,
        ] {
            let key = class.policy_key();
            transports.insert(
                class,
                DeviceTransport::parse(launch_plan_value(&values, key, label)?, key, label)?,
            );
        }
        Ok(Self {
            domain_id,
            transports,
        })
    }

    pub fn validate_for_lease(&self, lease: &ValidatedLease) -> Result<()> {
        if self.domain_id != lease.domain_id {
            bail!(
                "driver-domain policy domain={} does not match launch plan domain={}",
                self.domain_id,
                lease.domain_id
            );
        }
        Ok(())
    }

    pub fn transport_for(&self, class: DeviceClass) -> DeviceTransport {
        self.transports
            .get(&class)
            .copied()
            .expect("driver-domain policy has every fixed class")
    }
}

/// Immutable inventory of every hardware-backed driver domain permitted on
/// one L0 host.  Individual launch plans are useful for local preflight, but
/// cannot by themselves prove that a second plan does not reuse a CID, an
/// IOMMU group, or one PCI function.  A signed release binds this complete
/// fleet policy before any member may receive VFIO ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverDomainFleetPolicy {
    members: BTreeMap<String, DriverDomainFleetMember>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DriverDomainFleetMember {
    dvm_guest_cid: u32,
    iommu_group: u32,
    pci_bdfs: BTreeSet<String>,
}

impl DriverDomainFleetPolicy {
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read driver-domain fleet policy {}", path.display()))?;
        Self::parse(&source, &path.display().to_string())
    }

    /// The compact member encoding is deliberately not a generic nested
    /// language: `domain@cid@iommu-group@bdf+bdf;...`.  The release signature
    /// covers its exact bytes, while this parser makes every identity explicit
    /// before a member can be activated.
    pub fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const REQUIRED: [&str; 2] = ["DRIVER_DOMAIN_FLEET_POLICY_SCHEMA", "FLEET_MEMBERS"];
        if values.len() != REQUIRED.len()
            || values.keys().any(|key| !REQUIRED.contains(&key.as_str()))
        {
            bail!("unsupported key in {label}");
        }
        if launch_plan_value(&values, "DRIVER_DOMAIN_FLEET_POLICY_SCHEMA", label)? != "1" {
            bail!("unsupported {label} schema");
        }

        let mut members = BTreeMap::new();
        let mut cids = BTreeSet::new();
        let mut groups = BTreeSet::new();
        let mut bdfs = BTreeSet::new();
        let raw_members = launch_plan_value(&values, "FLEET_MEMBERS", label)?;
        for raw_member in raw_members.split(';') {
            let fields = raw_member.split('@').collect::<Vec<_>>();
            if fields.len() != 4 {
                bail!("invalid {label} FLEET_MEMBERS entry");
            }
            let domain_id = fields[0];
            validate_domain_id(domain_id, label)?;
            let dvm_guest_cid = fields[1]
                .parse::<u32>()
                .context("invalid FLEET_MEMBERS DVM guest CID")?;
            if dvm_guest_cid <= 2 {
                bail!("invalid {label} FLEET_MEMBERS DVM guest CID");
            }
            let iommu_group = fields[2]
                .parse::<u32>()
                .context("invalid FLEET_MEMBERS IOMMU group")?;
            let pci_bdfs = parse_fleet_member_pci_bdfs(fields[3], label)?;
            if !cids.insert(dvm_guest_cid) {
                bail!("duplicate DVM guest CID {dvm_guest_cid} in {label}");
            }
            if !groups.insert(iommu_group) {
                bail!("duplicate IOMMU group {iommu_group} in {label}");
            }
            for bdf in &pci_bdfs {
                if !bdfs.insert(bdf.clone()) {
                    bail!("duplicate PCI BDF {bdf:?} in {label}");
                }
            }
            if members
                .insert(
                    domain_id.to_owned(),
                    DriverDomainFleetMember {
                        dvm_guest_cid,
                        iommu_group,
                        pci_bdfs,
                    },
                )
                .is_some()
            {
                bail!("duplicate driver domain {domain_id:?} in {label}");
            }
        }
        if members.is_empty() {
            bail!("empty {label} FLEET_MEMBERS");
        }
        Ok(Self { members })
    }

    /// Validates that a preflighted plan is exactly one fleet member.  This is
    /// intentionally equality rather than subset matching: a plan cannot add
    /// a function to a member or silently split an IOMMU group.
    pub fn validate_for_lease(&self, lease: &ValidatedLease) -> Result<()> {
        let member = self.members.get(&lease.domain_id).ok_or_else(|| {
            anyhow!(
                "driver domain {} is absent from fleet policy",
                lease.domain_id
            )
        })?;
        if member.dvm_guest_cid != lease.dvm_guest_cid
            || member.iommu_group != lease.iommu_group
            || member.pci_bdfs != lease.pci_bdfs.iter().cloned().collect::<BTreeSet<_>>()
        {
            bail!(
                "fleet policy member {} does not exactly match the validated launch plan",
                lease.domain_id
            );
        }
        Ok(())
    }
}

/// Signed release authorization for an irreversible VFIO device handoff.
///
/// A launch plan is only a topology preflight input. Before `hostd acquire
/// --activate` may detach a real device, release engineering must sign this
/// authorization and bind the exact DVM artifact manifest and per-domain
/// transport policy to the complete IOMMU group. The signature is verified by
/// `hostd` with a pinned release keyring; this type keeps that payload strict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAuthorization {
    domain_id: String,
    dvm_guest_cid: u32,
    iommu_group: u32,
    assigned_pci_bdfs: BTreeSet<String>,
    dvm_artifact_manifest_sha256: String,
    device_policy_sha256: String,
    fleet_policy_sha256: String,
    not_before_unix: u64,
    not_after_unix: u64,
}

impl ReleaseAuthorization {
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read release authorization {}", path.display()))?;
        Self::parse(&source, &path.display().to_string())
    }

    pub fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const REQUIRED: [&str; 10] = [
            "RELEASE_AUTHORIZATION_SCHEMA",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "ASSIGNED_PCI_BDFS",
            "DVM_ARTIFACT_MANIFEST_SHA256",
            "DEVICE_POLICY_SHA256",
            "FLEET_POLICY_SHA256",
            "NOT_BEFORE_UNIX",
            "NOT_AFTER_UNIX",
        ];
        if values.len() != REQUIRED.len()
            || values.keys().any(|key| !REQUIRED.contains(&key.as_str()))
        {
            bail!("unsupported key in {label}");
        }
        if launch_plan_value(&values, "RELEASE_AUTHORIZATION_SCHEMA", label)? != "1" {
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
        let dvm_artifact_manifest_sha256 = parse_sha256(
            launch_plan_value(&values, "DVM_ARTIFACT_MANIFEST_SHA256", label)?,
            label,
        )?;
        let device_policy_sha256 = parse_sha256(
            launch_plan_value(&values, "DEVICE_POLICY_SHA256", label)?,
            label,
        )?;
        let fleet_policy_sha256 = parse_sha256(
            launch_plan_value(&values, "FLEET_POLICY_SHA256", label)?,
            label,
        )?;
        let not_before_unix = launch_plan_value(&values, "NOT_BEFORE_UNIX", label)?
            .parse::<u64>()
            .context("invalid NOT_BEFORE_UNIX")?;
        let not_after_unix = launch_plan_value(&values, "NOT_AFTER_UNIX", label)?
            .parse::<u64>()
            .context("invalid NOT_AFTER_UNIX")?;
        if not_before_unix == 0 || not_after_unix <= not_before_unix {
            bail!("invalid release authorization validity window in {label}");
        }
        Ok(Self {
            domain_id,
            dvm_guest_cid,
            iommu_group,
            assigned_pci_bdfs,
            dvm_artifact_manifest_sha256,
            device_policy_sha256,
            fleet_policy_sha256,
            not_before_unix,
            not_after_unix,
        })
    }

    pub fn validate_for_lease(&self, lease: &ValidatedLease, now_unix: u64) -> Result<()> {
        if self.domain_id != lease.domain_id
            || self.dvm_guest_cid != lease.dvm_guest_cid
            || self.iommu_group != lease.iommu_group
            || self.assigned_pci_bdfs.iter().cloned().collect::<Vec<_>>() != lease.pci_bdfs
        {
            bail!("release authorization does not match the validated launch plan");
        }
        if now_unix < self.not_before_unix || now_unix > self.not_after_unix {
            bail!("release authorization is outside its validity window");
        }
        Ok(())
    }

    pub fn dvm_artifact_manifest_sha256(&self) -> &str {
        &self.dvm_artifact_manifest_sha256
    }

    pub fn device_policy_sha256(&self) -> &str {
        &self.device_policy_sha256
    }

    pub fn fleet_policy_sha256(&self) -> &str {
        &self.fleet_policy_sha256
    }

    pub fn not_after_unix(&self) -> u64 {
        self.not_after_unix
    }

    pub fn into_vfio_release_binding(
        &self,
        release_manifest_sha256: &str,
        authorized_at_unix: u64,
    ) -> Result<VfioReleaseBinding> {
        if authorized_at_unix < self.not_before_unix || authorized_at_unix > self.not_after_unix {
            bail!("VFIO release binding was created outside authorization validity window");
        }
        VfioReleaseBinding::new(
            release_manifest_sha256,
            &self.dvm_artifact_manifest_sha256,
            &self.device_policy_sha256,
            &self.fleet_policy_sha256,
            authorized_at_unix,
            self.not_after_unix,
        )
    }
}

/// Evidence retained with a durable VFIO lease after a release authorization
/// was verified. Recovery may restore a legacy record, but no new bind may be
/// prepared or activated without this exact immutable evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VfioReleaseBinding {
    release_manifest_sha256: String,
    dvm_artifact_manifest_sha256: String,
    device_policy_sha256: String,
    fleet_policy_sha256: Option<String>,
    authorized_at_unix: u64,
    authorization_not_after_unix: u64,
}

impl VfioReleaseBinding {
    pub fn new(
        release_manifest_sha256: &str,
        dvm_artifact_manifest_sha256: &str,
        device_policy_sha256: &str,
        fleet_policy_sha256: &str,
        authorized_at_unix: u64,
        authorization_not_after_unix: u64,
    ) -> Result<Self> {
        if authorized_at_unix == 0 || authorization_not_after_unix < authorized_at_unix {
            bail!("invalid VFIO release binding validity window");
        }
        Ok(Self {
            release_manifest_sha256: parse_sha256(release_manifest_sha256, "VFIO release binding")?,
            dvm_artifact_manifest_sha256: parse_sha256(
                dvm_artifact_manifest_sha256,
                "VFIO release binding",
            )?,
            device_policy_sha256: parse_sha256(device_policy_sha256, "VFIO release binding")?,
            fleet_policy_sha256: Some(parse_sha256(fleet_policy_sha256, "VFIO release binding")?),
            authorized_at_unix,
            authorization_not_after_unix,
        })
    }

    /// V2 records predate fleet binding.  They remain restorable so a host
    /// never loses recovery for an already-bound group, but are ineligible for
    /// every future prepare/bind/active mutation.
    fn legacy_v2(
        release_manifest_sha256: &str,
        dvm_artifact_manifest_sha256: &str,
        device_policy_sha256: &str,
        authorized_at_unix: u64,
        authorization_not_after_unix: u64,
    ) -> Result<Self> {
        if authorized_at_unix == 0 || authorization_not_after_unix < authorized_at_unix {
            bail!("invalid VFIO release binding validity window");
        }
        Ok(Self {
            release_manifest_sha256: parse_sha256(release_manifest_sha256, "VFIO release binding")?,
            dvm_artifact_manifest_sha256: parse_sha256(
                dvm_artifact_manifest_sha256,
                "VFIO release binding",
            )?,
            device_policy_sha256: parse_sha256(device_policy_sha256, "VFIO release binding")?,
            fleet_policy_sha256: None,
            authorized_at_unix,
            authorization_not_after_unix,
        })
    }

    fn validate_at(&self, now_unix: u64) -> Result<()> {
        if self.fleet_policy_sha256.is_none() {
            bail!("VFIO release authorization lacks fleet-policy evidence");
        }
        if now_unix < self.authorized_at_unix || now_unix > self.authorization_not_after_unix {
            bail!("VFIO release authorization is outside its validity window");
        }
        Ok(())
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
    release_binding: Option<VfioReleaseBinding>,
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
        release_binding: Option<VfioReleaseBinding>,
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
            release_binding,
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
        if let Some(binding) = &self.release_binding {
            if let Some(fleet_policy_sha256) = &binding.fleet_policy_sha256 {
                return format!(
                    "VFIO_LEASE_SCHEMA=3\nLEASE_STATE={state}\nDOMAIN_ID={}\nDVM_GUEST_CID={}\nIOMMU_GROUP={}\nORIGINAL_DRIVERS={drivers}\nORIGINAL_DRIVER_OVERRIDES={overrides}\nRELEASE_MANIFEST_SHA256={}\nDVM_ARTIFACT_MANIFEST_SHA256={}\nDEVICE_POLICY_SHA256={}\nFLEET_POLICY_SHA256={}\nAUTHORIZED_AT_UNIX={}\nAUTHORIZATION_NOT_AFTER_UNIX={}\n",
                    self.domain_id,
                    self.dvm_guest_cid,
                    self.iommu_group,
                    binding.release_manifest_sha256,
                    binding.dvm_artifact_manifest_sha256,
                    binding.device_policy_sha256,
                    fleet_policy_sha256,
                    binding.authorized_at_unix,
                    binding.authorization_not_after_unix,
                );
            }
            return format!(
                "VFIO_LEASE_SCHEMA=2\nLEASE_STATE={state}\nDOMAIN_ID={}\nDVM_GUEST_CID={}\nIOMMU_GROUP={}\nORIGINAL_DRIVERS={drivers}\nORIGINAL_DRIVER_OVERRIDES={overrides}\nRELEASE_MANIFEST_SHA256={}\nDVM_ARTIFACT_MANIFEST_SHA256={}\nDEVICE_POLICY_SHA256={}\nAUTHORIZED_AT_UNIX={}\nAUTHORIZATION_NOT_AFTER_UNIX={}\n",
                self.domain_id,
                self.dvm_guest_cid,
                self.iommu_group,
                binding.release_manifest_sha256,
                binding.dvm_artifact_manifest_sha256,
                binding.device_policy_sha256,
                binding.authorized_at_unix,
                binding.authorization_not_after_unix,
            );
        }
        format!(
            "VFIO_LEASE_SCHEMA=1\nLEASE_STATE={state}\nDOMAIN_ID={}\nDVM_GUEST_CID={}\nIOMMU_GROUP={}\nORIGINAL_DRIVERS={drivers}\nORIGINAL_DRIVER_OVERRIDES={overrides}\n",
            self.domain_id, self.dvm_guest_cid, self.iommu_group
        )
    }

    fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const V1_REQUIRED: [&str; 7] = [
            "VFIO_LEASE_SCHEMA",
            "LEASE_STATE",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "ORIGINAL_DRIVERS",
            "ORIGINAL_DRIVER_OVERRIDES",
        ];
        const V2_REQUIRED: [&str; 12] = [
            "VFIO_LEASE_SCHEMA",
            "LEASE_STATE",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "ORIGINAL_DRIVERS",
            "ORIGINAL_DRIVER_OVERRIDES",
            "RELEASE_MANIFEST_SHA256",
            "DVM_ARTIFACT_MANIFEST_SHA256",
            "DEVICE_POLICY_SHA256",
            "AUTHORIZED_AT_UNIX",
            "AUTHORIZATION_NOT_AFTER_UNIX",
        ];
        const V3_REQUIRED: [&str; 13] = [
            "VFIO_LEASE_SCHEMA",
            "LEASE_STATE",
            "DOMAIN_ID",
            "DVM_GUEST_CID",
            "IOMMU_GROUP",
            "ORIGINAL_DRIVERS",
            "ORIGINAL_DRIVER_OVERRIDES",
            "RELEASE_MANIFEST_SHA256",
            "DVM_ARTIFACT_MANIFEST_SHA256",
            "DEVICE_POLICY_SHA256",
            "FLEET_POLICY_SHA256",
            "AUTHORIZED_AT_UNIX",
            "AUTHORIZATION_NOT_AFTER_UNIX",
        ];
        let schema = launch_plan_value(&values, "VFIO_LEASE_SCHEMA", label)?;
        let release_binding = match schema {
            "1" if values.len() == V1_REQUIRED.len()
                && !values
                    .keys()
                    .any(|key| !V1_REQUIRED.contains(&key.as_str())) =>
            {
                None
            }
            "2" if values.len() == V2_REQUIRED.len()
                && !values
                    .keys()
                    .any(|key| !V2_REQUIRED.contains(&key.as_str())) =>
            {
                Some(VfioReleaseBinding::legacy_v2(
                    launch_plan_value(&values, "RELEASE_MANIFEST_SHA256", label)?,
                    launch_plan_value(&values, "DVM_ARTIFACT_MANIFEST_SHA256", label)?,
                    launch_plan_value(&values, "DEVICE_POLICY_SHA256", label)?,
                    launch_plan_value(&values, "AUTHORIZED_AT_UNIX", label)?
                        .parse::<u64>()
                        .context("invalid AUTHORIZED_AT_UNIX")?,
                    launch_plan_value(&values, "AUTHORIZATION_NOT_AFTER_UNIX", label)?
                        .parse::<u64>()
                        .context("invalid AUTHORIZATION_NOT_AFTER_UNIX")?,
                )?)
            }
            "3" if values.len() == V3_REQUIRED.len()
                && !values
                    .keys()
                    .any(|key| !V3_REQUIRED.contains(&key.as_str())) =>
            {
                Some(VfioReleaseBinding::new(
                    launch_plan_value(&values, "RELEASE_MANIFEST_SHA256", label)?,
                    launch_plan_value(&values, "DVM_ARTIFACT_MANIFEST_SHA256", label)?,
                    launch_plan_value(&values, "DEVICE_POLICY_SHA256", label)?,
                    launch_plan_value(&values, "FLEET_POLICY_SHA256", label)?,
                    launch_plan_value(&values, "AUTHORIZED_AT_UNIX", label)?
                        .parse::<u64>()
                        .context("invalid AUTHORIZED_AT_UNIX")?,
                    launch_plan_value(&values, "AUTHORIZATION_NOT_AFTER_UNIX", label)?
                        .parse::<u64>()
                        .context("invalid AUTHORIZATION_NOT_AFTER_UNIX")?,
                )?)
            }
            _ => bail!("unsupported key or schema in {label}"),
        };
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
            release_binding,
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

    pub fn create_prepared(&self, record: &VfioLeaseRecord, now_unix: u64) -> Result<()> {
        if record.state != VfioLeaseState::Prepared {
            bail!("only a prepared VFIO lease can be created");
        }
        record
            .release_binding
            .as_ref()
            .ok_or_else(|| {
                anyhow!("prepared VFIO lease lacks signed release authorization evidence")
            })?
            .validate_at(now_unix)?;
        self.ensure_private_root()?;
        write_new_private(&self.path_for(&record.domain_id)?, &record.to_env())?;
        sync_directory(&self.root)
    }

    pub fn mark_active(&self, record: &mut VfioLeaseRecord, now_unix: u64) -> Result<()> {
        if record.state != VfioLeaseState::Prepared {
            bail!("only a prepared VFIO lease can become active");
        }
        record
            .release_binding
            .as_ref()
            .ok_or_else(|| {
                anyhow!("active VFIO lease lacks signed release authorization evidence")
            })?
            .validate_at(now_unix)?;
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

pub fn inspect_vfio_lease(
    lease: &ValidatedLease,
    ops: &impl VfioOps,
    release_binding: VfioReleaseBinding,
) -> Result<VfioLeaseRecord> {
    inspect_vfio_lease_inner(lease, ops, Some(release_binding))
}

/// Read-only driver snapshot for an `acquire` dry run. The returned record is
/// intentionally ineligible for `create_prepared` or `acquire_vfio_lease`.
pub fn inspect_vfio_lease_preflight(
    lease: &ValidatedLease,
    ops: &impl VfioOps,
) -> Result<VfioLeaseRecord> {
    inspect_vfio_lease_inner(lease, ops, None)
}

fn inspect_vfio_lease_inner(
    lease: &ValidatedLease,
    ops: &impl VfioOps,
    release_binding: Option<VfioReleaseBinding>,
) -> Result<VfioLeaseRecord> {
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
    VfioLeaseRecord::from_validated_lease(
        lease,
        original_drivers,
        original_driver_overrides,
        release_binding,
    )
}

/// Acquire the complete validated IOMMU group. On every failure this function
/// attempts a reverse-order rollback to the record's original host drivers.
pub fn acquire_vfio_lease(
    record: &VfioLeaseRecord,
    ops: &mut impl VfioOps,
    now_unix: u64,
) -> Result<()> {
    if record.state != VfioLeaseState::Prepared {
        bail!("VFIO acquire requires a prepared lease");
    }
    record
        .release_binding
        .as_ref()
        .ok_or_else(|| anyhow!("VFIO acquire requires signed release authorization evidence"))?
        .validate_at(now_unix)?;
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
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
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

fn parse_sha256(value: &str, label: &str) -> Result<String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 value in {label}");
    }
    Ok(value.to_ascii_lowercase())
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

fn parse_fleet_member_pci_bdfs(value: &str, label: &str) -> Result<BTreeSet<String>> {
    let mut bdfs = BTreeSet::new();
    for bdf in value.split('+') {
        validate_pci_bdf(bdf, label)?;
        if !bdfs.insert(bdf.to_owned()) {
            bail!("duplicate PCI BDF {bdf:?} in {label} FLEET_MEMBERS entry");
        }
    }
    if bdfs.is_empty() {
        bail!("empty PCI BDF list in {label} FLEET_MEMBERS entry");
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
    pub driver_inventory: DvmDriverInventory,
}

/// DVM-local driver binding snapshot. These values prove only that the Linux
/// domain owns its virtual devices; they are deliberately not a claim that a
/// high-bandwidth RustOS data plane exists yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmDriverInventory {
    pub virtio_net_bound: bool,
    pub virtio_gpu_bound: bool,
}

/// A fixed, checksummed L0-to-RustOS input frame.
///
/// This is a RustOS virtual-device transport frame, not a public application
/// ABI. The session frame establishes a new host relay epoch; all input frames
/// are accepted only once and only in that epoch by the RustOS transport
/// receiver. Payloads are fixed-width so a DVM never controls a length or a
/// native input ABI structure in RustOS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustosInputFrame {
    bytes: [u8; RUSTOS_INPUT_FRAME_BYTES],
}

impl RustosInputFrame {
    pub fn session_start(epoch: u32) -> Result<Self> {
        if epoch == 0 {
            bail!("RustOS input relay epoch must be nonzero");
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_SESSION_START, epoch, 0);
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn session_end(epoch: u32, sequence: u32) -> Result<Self> {
        if epoch == 0 || sequence == 0 {
            bail!("RustOS input relay session end requires nonzero epoch and sequence");
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_SESSION_END, epoch, sequence);
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn linux_evdev_key(epoch: u32, sequence: u32, code: u16, value: u8) -> Result<Self> {
        if epoch == 0 || sequence == 0 {
            bail!("RustOS input relay key frame requires nonzero epoch and sequence");
        }
        if code == 0 || code > LINUX_EVDEV_KEY_MAX || value > 2 {
            bail!("invalid Linux evdev key frame code={code} value={value}");
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_KEY, epoch, sequence);
        frame.bytes[16..18].copy_from_slice(&code.to_be_bytes());
        frame.bytes[18] = value;
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn linux_evdev_pointer(
        epoch: u32,
        sequence: u32,
        dx: i16,
        dy: i16,
        wheel_vertical: i16,
        wheel_horizontal: i16,
        buttons: u8,
    ) -> Result<Self> {
        if epoch == 0 || sequence == 0 {
            bail!("RustOS pointer frame requires nonzero epoch and sequence");
        }
        if buttons & !RUSTOS_POINTER_BUTTON_MASK != 0 {
            bail!("invalid RustOS pointer buttons {buttons:#x}");
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_POINTER, epoch, sequence);
        frame.bytes[16..18].copy_from_slice(&dx.to_be_bytes());
        frame.bytes[18..20].copy_from_slice(&dy.to_be_bytes());
        frame.bytes[20..22].copy_from_slice(&wheel_vertical.to_be_bytes());
        frame.bytes[22..24].copy_from_slice(&wheel_horizontal.to_be_bytes());
        frame.bytes[24] = buttons;
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn as_bytes(&self) -> &[u8; RUSTOS_INPUT_FRAME_BYTES] {
        &self.bytes
    }

    fn new(kind: u8, epoch: u32, sequence: u32) -> Self {
        let mut bytes = [0_u8; RUSTOS_INPUT_FRAME_BYTES];
        bytes[..4].copy_from_slice(&RUSTOS_INPUT_MAGIC);
        bytes[4] = RUSTOS_INPUT_VERSION;
        bytes[5] = kind;
        bytes[8..12].copy_from_slice(&epoch.to_be_bytes());
        bytes[12..16].copy_from_slice(&sequence.to_be_bytes());
        Self { bytes }
    }

    fn finish_checksum(&mut self) {
        let checksum = crc32(&self.bytes[..28]).to_be_bytes();
        self.bytes[28..32].copy_from_slice(&checksum);
    }
}

/// Destination controlled by L0 for sanitized DVM input frames.
pub trait RustosInputSink {
    fn send_input_frame(&mut self, frame: &RustosInputFrame) -> Result<()>;
}

/// QEMU's private, dedicated serial socket used by the current low-rate input
/// transport.  The socket is intentionally distinct from QMP and from the
/// debug/console serial channel.
#[derive(Debug)]
pub struct UnixInputSink {
    stream: UnixStream,
}

impl UnixInputSink {
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self> {
        let started = std::time::Instant::now();
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => {
                    stream
                        .set_write_timeout(Some(timeout.min(Duration::from_secs(1))))
                        .with_context(|| {
                            format!("set RustOS input socket timeout {}", path.display())
                        })?;
                    return Ok(Self { stream });
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::AddrNotAvailable
                    ) && started.elapsed() < timeout =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("connect RustOS input socket {}", path.display())
                    });
                }
            }
            if started.elapsed() >= timeout {
                bail!(
                    "timed out waiting for RustOS input socket {}",
                    path.display()
                );
            }
        }
    }
}

impl RustosInputSink for UnixInputSink {
    fn send_input_frame(&mut self, frame: &RustosInputFrame) -> Result<()> {
        self.stream
            .write_all(frame.as_bytes())
            .and_then(|()| self.stream.flush())
            .context("write sanitized frame to RustOS virtual input transport")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputRelayResult {
    pub probe: ProbeResult,
    pub forwarded_events: u64,
}

/// L0-side CPU and serial-budget guard. It intentionally rejects an abusive
/// DVM stream instead of allowing an unbounded input producer to starve the
/// broker or fill RustOS's bounded ingress queue. The total permits the
/// 125Hz coalesced pointer stream plus normal keyboard use without overrunning
/// the serial transport.
struct InputRelayRate {
    window_started: Instant,
    total_frames: u32,
    key_frames: u32,
}

impl InputRelayRate {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            total_frames: 0,
            key_frames: 0,
        }
    }

    fn admit(&mut self, event: LinuxEvdevInputEvent) -> Result<()> {
        if self.window_started.elapsed() >= Duration::from_secs(1) {
            self.window_started = Instant::now();
            self.total_frames = 0;
            self.key_frames = 0;
        }
        if self.total_frames >= INPUT_RELAY_MAX_FRAMES_PER_SECOND {
            bail!("Linux DVM input stream exceeds L0 frame rate budget");
        }
        if matches!(event, LinuxEvdevInputEvent::Key(_))
            && self.key_frames >= INPUT_RELAY_MAX_KEYS_PER_SECOND
        {
            bail!("Linux DVM input stream exceeds L0 keyboard rate budget");
        }
        self.total_frames += 1;
        if matches!(event, LinuxEvdevInputEvent::Key(_)) {
            self.key_frames += 1;
        }
        Ok(())
    }
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
    control_secret: ControlSecret,
}

impl HostControlListener {
    pub fn bind(
        expected_dvm_cid: u32,
        contract: ControlContract,
        control_secret: ControlSecret,
    ) -> Result<Self> {
        if expected_dvm_cid <= 2 {
            bail!("invalid DVM vsock identity cid={expected_dvm_cid}");
        }
        // Keep endpoint selection at the secret-owning L0 boundary. Exposing a
        // caller-selected public port here would let a future caller silently
        // undo the same-CID setup-slot isolation.
        let port = control_secret.control_port();
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
            control_secret,
        })
    }

    pub fn probe_once(&self, timeout: Duration) -> Result<ProbeResult> {
        let mut connection = self.accept_authenticated(Some(timeout))?;
        self.probe_connection(&mut connection)
    }

    /// Relay Linux evdev keyboard and pointer events from one authenticated
    /// DVM into a host-owned RustOS virtual input endpoint.
    ///
    /// The DVM chooses neither the destination nor the wire format.  L0 sends
    /// a fresh session frame, validates every event before assigning its own
    /// monotonic sequence number, and fails closed on malformed stream data.
    pub fn relay_input_once(
        &self,
        timeout: Duration,
        sink: &mut impl RustosInputSink,
    ) -> Result<InputRelayResult> {
        self.relay_input_once_inner(Some(timeout), sink, |_| Ok(()))
    }

    /// Like [`Self::relay_input_once`], but reports the verified DVM endpoint
    /// after the RustOS relay session is installed and before waiting for the
    /// first human input event.  KVM smoke uses this to prove endpoint setup
    /// without reintroducing synthetic QMP input.
    pub fn relay_input_once_with_ready(
        &self,
        timeout: Duration,
        sink: &mut impl RustosInputSink,
        on_ready: impl FnOnce(&ProbeResult) -> Result<()>,
    ) -> Result<InputRelayResult> {
        self.relay_input_once_inner(Some(timeout), sink, on_ready)
    }

    /// Relay one DVM input session with a bounded authentication/setup stage
    /// and no deadline for a healthy, idle input stream. This is for a
    /// developer-owned interactive VM session; smoke tests must use the
    /// caller-selected bounded methods above. A same-CID peer that never sends
    /// a HELLO or proof therefore cannot hold the listener forever. A
    /// disconnect still emits input cleanup and returns an error to the
    /// session supervisor for reconnect handling.
    pub fn relay_input_once_unbounded(
        &self,
        sink: &mut impl RustosInputSink,
    ) -> Result<InputRelayResult> {
        self.relay_input_once_inner(Some(Duration::from_secs(5)), sink, |_| Ok(()))
    }

    fn relay_input_once_inner(
        &self,
        setup_timeout: Option<Duration>,
        sink: &mut impl RustosInputSink,
        on_ready: impl FnOnce(&ProbeResult) -> Result<()>,
    ) -> Result<InputRelayResult> {
        let mut connection = self.accept_authenticated(setup_timeout)?;
        let probe = self.probe_connection(&mut connection)?;
        let ready = request(&mut connection, 4, "input-stream")?;
        if !has_exact_fields(
            &ready,
            &["id", "op", "status", "format", "keyboard", "pointer"],
        ) || ready.get("status") != Some(&"ready".to_owned())
            || ready.get("format") != Some(&"linux-evdev-v2".to_owned())
            || !valid_evdev_endpoint(ready.get("keyboard"))
            || !valid_evdev_endpoint(ready.get("pointer"))
        {
            bail!("Linux DVM did not acknowledge input stream readiness");
        }

        let epoch = new_input_epoch();
        // This remains an input relay: it sends no Ethernet payload. In the
        // current combined-DVM profile, its L0-authenticated start/end markers
        // also bound the lifetime of RustOS's independent fixed network ring.
        // A later network-only domain must receive a distinct authenticated
        // lifecycle signal rather than inheriting this input-DVM epoch.
        sink.send_input_frame(&RustosInputFrame::session_start(epoch)?)?;
        // Endpoint setup has a deadline, but a live input device may stay idle
        // indefinitely. A disconnect is still reported as a relay failure after
        // L0 emits releases and a session-end marker to prevent stuck input.
        configure_socket_timeout(&connection, None)?;
        if let Err(error) = on_ready(&probe) {
            let _ = sink.send_input_frame(&RustosInputFrame::session_end(epoch, 1)?);
            return Err(error);
        }
        let mut next_sequence = 1_u32;
        let mut forwarded_events = 0_u64;
        let mut pressed_keys = BTreeSet::new();
        let mut pointer_buttons = 0_u8;
        let mut rate = InputRelayRate::new();
        let relay_result = loop {
            if next_sequence > INPUT_RELAY_MAX_SEQUENCE {
                break Err(anyhow!(
                    "RustOS input relay sequence budget exhausted; reconnect DVM"
                ));
            }
            let message = match read_message(&mut connection).and_then(|raw| parse_message(&raw)) {
                Ok(message) => message,
                Err(error) => break Err(error),
            };
            let event = match parse_linux_evdev_input_event(&message, 4, "input-stream") {
                Ok(event) => event,
                Err(error) => break Err(error),
            };
            if let Err(error) = rate.admit(event) {
                break Err(error);
            }
            let frame = match event {
                LinuxEvdevInputEvent::Key(event) => {
                    if event.value == 0 {
                        pressed_keys.remove(&event.code);
                    } else if event.value == 1 {
                        pressed_keys.insert(event.code);
                    }
                    RustosInputFrame::linux_evdev_key(epoch, next_sequence, event.code, event.value)
                }
                LinuxEvdevInputEvent::Pointer(event) => {
                    pointer_buttons = event.buttons;
                    RustosInputFrame::linux_evdev_pointer(
                        epoch,
                        next_sequence,
                        event.dx,
                        event.dy,
                        event.wheel_vertical,
                        event.wheel_horizontal,
                        event.buttons,
                    )
                }
            };
            let frame = match frame.and_then(|frame| {
                sink.send_input_frame(&frame)?;
                Ok(frame)
            }) {
                Ok(frame) => frame,
                Err(error) => break Err(error),
            };
            let _ = frame;
            forwarded_events = forwarded_events.saturating_add(1);
            next_sequence += 1;
        };

        let cleanup = send_input_cleanup(
            sink,
            epoch,
            &mut next_sequence,
            &pressed_keys,
            pointer_buttons,
        );
        match (relay_result, cleanup) {
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(error).context(format!(
                "RustOS input relay cleanup failed: {cleanup_error:#}"
            )),
            (Ok(()), _) => unreachable!("input relay is a continuous stream"),
        }
    }

    fn accept_authenticated(&self, timeout: Option<Duration>) -> Result<std::fs::File> {
        let timeout_ms = timeout
            .map(|duration| duration.as_millis().min(i32::MAX as u128) as libc::c_int)
            .unwrap_or(-1);
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
        authenticate_control_peer(&mut connection, &hello, &self.control_secret)?;
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
        let drivers = request(connection, 3, "driver-inventory")?;
        if !has_exact_fields(
            &drivers,
            &["id", "op", "status", "virtio-net", "virtio-gpu"],
        ) || drivers.get("status") != Some(&"ok".to_owned())
        {
            bail!("Linux DVM driver inventory probe was not ready");
        }
        let driver_inventory = DvmDriverInventory {
            virtio_net_bound: parse_driver_inventory_state(&drivers, "virtio-net")?,
            virtio_gpu_bound: parse_driver_inventory_state(&drivers, "virtio-gpu")?,
        };
        Ok(ProbeResult {
            peer_cid: self.expected_dvm_cid,
            inventory_count,
            driver_inventory,
        })
    }
}

fn parse_driver_inventory_state(fields: &BTreeMap<String, String>, key: &str) -> Result<bool> {
    match fields.get(key).map(String::as_str) {
        Some("bound") => Ok(true),
        Some("missing") => Ok(false),
        Some(value) => bail!("invalid Linux DVM driver inventory state {key}={value:?}"),
        None => bail!("Linux DVM driver inventory omitted {key}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinuxEvdevKeyEvent {
    code: u16,
    value: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinuxEvdevPointerEvent {
    dx: i16,
    dy: i16,
    wheel_vertical: i16,
    wheel_horizontal: i16,
    buttons: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxEvdevInputEvent {
    Key(LinuxEvdevKeyEvent),
    Pointer(LinuxEvdevPointerEvent),
}

fn parse_linux_evdev_input_event(
    message: &Message,
    id: u32,
    operation: &str,
) -> Result<LinuxEvdevInputEvent> {
    if message.kind != "EVENT"
        || message.fields.get("id") != Some(&id.to_string())
        || message.fields.get("op") != Some(&operation.to_owned())
    {
        bail!("invalid Linux DVM input stream event");
    }
    match message.fields.get("type").map(String::as_str) {
        Some("key") => parse_linux_evdev_key_event(message),
        Some("pointer") => parse_linux_evdev_pointer_event(message),
        _ => bail!("rejected unknown Linux DVM input event type"),
    }
}

fn parse_linux_evdev_key_event(message: &Message) -> Result<LinuxEvdevInputEvent> {
    if message.kind != "EVENT"
        || !has_exact_fields(&message.fields, &["id", "op", "type", "code", "value"])
        || message.fields.get("type") != Some(&"key".to_owned())
    {
        bail!("invalid Linux DVM input stream event");
    }
    let code = message
        .fields
        .get("code")
        .ok_or_else(|| anyhow!("Linux DVM input event omitted key code"))?
        .parse::<u16>()
        .context("invalid Linux DVM input key code")?;
    let value = message
        .fields
        .get("value")
        .ok_or_else(|| anyhow!("Linux DVM input event omitted key value"))?
        .parse::<u8>()
        .context("invalid Linux DVM input key value")?;
    if code == 0 || code > LINUX_EVDEV_KEY_MAX || value > 2 {
        bail!("rejected out-of-range Linux DVM key event code={code} value={value}");
    }
    Ok(LinuxEvdevInputEvent::Key(LinuxEvdevKeyEvent {
        code,
        value,
    }))
}

fn parse_linux_evdev_pointer_event(message: &Message) -> Result<LinuxEvdevInputEvent> {
    if !has_exact_fields(
        &message.fields,
        &[
            "id", "op", "type", "dx", "dy", "wheel-v", "wheel-h", "buttons",
        ],
    ) {
        bail!("invalid Linux DVM pointer event fields");
    }
    let parse_axis = |field: &str| -> Result<i16> {
        message
            .fields
            .get(field)
            .ok_or_else(|| anyhow!("Linux DVM pointer event omitted {field}"))?
            .parse::<i16>()
            .with_context(|| format!("invalid Linux DVM pointer {field}"))
    };
    let buttons = message
        .fields
        .get("buttons")
        .ok_or_else(|| anyhow!("Linux DVM pointer event omitted buttons"))?
        .parse::<u8>()
        .context("invalid Linux DVM pointer buttons")?;
    if buttons & !RUSTOS_POINTER_BUTTON_MASK != 0 {
        bail!("rejected Linux DVM pointer buttons {buttons:#x}");
    }
    Ok(LinuxEvdevInputEvent::Pointer(LinuxEvdevPointerEvent {
        dx: parse_axis("dx")?,
        dy: parse_axis("dy")?,
        wheel_vertical: parse_axis("wheel-v")?,
        wheel_horizontal: parse_axis("wheel-h")?,
        buttons,
    }))
}

fn valid_evdev_endpoint(value: Option<&String>) -> bool {
    value.is_some_and(|value| {
        value.strip_prefix("event").is_some_and(|index| {
            !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn send_input_cleanup(
    sink: &mut impl RustosInputSink,
    epoch: u32,
    next_sequence: &mut u32,
    pressed_keys: &BTreeSet<u16>,
    pointer_buttons: u8,
) -> Result<()> {
    for &code in pressed_keys {
        send_input_frame_with_sequence(sink, next_sequence, |sequence| {
            RustosInputFrame::linux_evdev_key(epoch, sequence, code, 0)
        })?;
    }
    if pointer_buttons != 0 {
        send_input_frame_with_sequence(sink, next_sequence, |sequence| {
            RustosInputFrame::linux_evdev_pointer(epoch, sequence, 0, 0, 0, 0, 0)
        })?;
    }
    send_input_frame_with_sequence(sink, next_sequence, |sequence| {
        RustosInputFrame::session_end(epoch, sequence)
    })
}

fn send_input_frame_with_sequence(
    sink: &mut impl RustosInputSink,
    next_sequence: &mut u32,
    make_frame: impl FnOnce(u32) -> Result<RustosInputFrame>,
) -> Result<()> {
    if *next_sequence == 0 || *next_sequence > u32::MAX - 1 {
        bail!("RustOS input relay has no sequence reserved for cleanup");
    }
    let frame = make_frame(*next_sequence)?;
    sink.send_input_frame(&frame)?;
    *next_sequence += 1;
    Ok(())
}

fn new_input_epoch() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ u64::from(std::process::id()).rotate_left(17);
    let epoch = (mixed ^ (mixed >> 32)) as u32;
    epoch.max(1)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg() & 0xedb8_8320;
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
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

fn configure_socket_timeout(connection: &std::fs::File, timeout: Option<Duration>) -> Result<()> {
    let timeout = timeout.unwrap_or_default();
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

fn authenticate_control_peer(
    connection: &mut std::fs::File,
    hello: &str,
    control_secret: &ControlSecret,
) -> Result<()> {
    let mut nonce = [0_u8; CONTROL_SECRET_BYTES];
    fs::File::open("/dev/urandom")
        .context("open /dev/urandom for DVM control challenge")?
        .read_exact(&mut nonce)
        .context("read DVM control challenge")?;
    write_message(
        connection,
        &format!("CHALLENGE\nnonce={}", encode_hex(&nonce)),
    )?;
    let proof = parse_message(&read_message(connection)?)?;
    if proof.kind != "PROOF" || !has_exact_fields(&proof.fields, &["mac"]) {
        bail!("rejected Linux DVM control proof shape");
    }
    let supplied = decode_hex_exact(
        proof
            .fields
            .get("mac")
            .ok_or_else(|| anyhow!("DVM control proof omitted mac"))?,
        CONTROL_SECRET_BYTES,
    )?;
    let expected = control_proof(control_secret, &nonce, hello)?;
    if !constant_time_eq(&supplied, &expected) {
        bail!("rejected Linux DVM control proof");
    }
    Ok(())
}

fn control_proof(
    control_secret: &ControlSecret,
    nonce: &[u8; CONTROL_SECRET_BYTES],
    hello: &str,
) -> Result<[u8; CONTROL_SECRET_BYTES]> {
    let mut transcript =
        Vec::with_capacity(CONTROL_PROOF_CONTEXT.len() + nonce.len() + hello.len());
    transcript.extend_from_slice(CONTROL_PROOF_CONTEXT);
    transcript.extend_from_slice(nonce);
    transcript.extend_from_slice(hello.as_bytes());
    hmac_sha256(&control_secret.bytes, &transcript)
}

fn hmac_sha256(
    key: &[u8; CONTROL_SECRET_BYTES],
    message: &[u8],
) -> Result<[u8; CONTROL_SECRET_BYTES]> {
    let fd = unsafe { libc::socket(AF_ALG, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create AF_ALG HMAC socket");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut address = SockaddrAlg {
        family: AF_ALG as libc::sa_family_t,
        algorithm_type: [0; 14],
        feature: 0,
        mask: 0,
        name: [0; 64],
    };
    copy_c_string(&mut address.algorithm_type, b"hash")?;
    copy_c_string(&mut address.name, b"hmac(sha256)")?;
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            size_of::<SockaddrAlg>() as libc::socklen_t,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("bind AF_ALG HMAC socket");
    }
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            SOL_ALG,
            ALG_SET_KEY,
            key.as_ptr().cast(),
            key.len() as libc::socklen_t,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("set AF_ALG HMAC key");
    }
    let operation = unsafe {
        libc::accept4(
            fd.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if operation < 0 {
        return Err(std::io::Error::last_os_error()).context("accept AF_ALG HMAC operation");
    }
    let mut operation = unsafe { std::fs::File::from_raw_fd(operation) };
    operation
        .write_all(message)
        .context("write AF_ALG HMAC transcript")?;
    let mut digest = [0_u8; CONTROL_SECRET_BYTES];
    operation
        .read_exact(&mut digest)
        .context("read AF_ALG HMAC digest")?;
    Ok(digest)
}

fn copy_c_string(destination: &mut [u8], value: &[u8]) -> Result<()> {
    if value.is_empty() || value.len() >= destination.len() || value.contains(&0) {
        bail!("invalid AF_ALG algorithm name");
    }
    destination[..value.len()].copy_from_slice(value);
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex_exact(source: &str, expected_bytes: usize) -> Result<Vec<u8>> {
    if source.len() != expected_bytes * 2 || !source.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid hexadecimal DVM control proof");
    }
    source
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| Ok((hex_value(chunk[0])? << 4) | hex_value(chunk[1])?))
        .collect()
}

fn hex_value(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => bail!("invalid hexadecimal digit"),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut different = 0_u8;
    for (left, right) in left.iter().zip(right) {
        different |= left ^ right;
    }
    different == 0
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
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        CONTROL_PORT_FLOOR, CONTROL_SECRET_BYTES, ControlContract, ControlSecret, DeviceClass,
        DeviceTransport, DriverDomainFleetPolicy, DriverDomainPolicy, FileLeaseStore,
        InputRelayRate, IommuTopology, LaunchPlan, LinuxEvdevInputEvent, LinuxEvdevKeyEvent,
        ReleaseAuthorization, RustosInputFrame, ValidatedLease, VfioLeaseRecord, VfioLeaseState,
        VfioOps, VfioReleaseBinding, acquire_vfio_lease, control_proof, inspect_vfio_lease,
        inspect_vfio_lease_preflight, parse_linux_evdev_input_event, parse_message,
        restore_vfio_lease, validate_hello,
    };
    use anyhow::Result;

    const VALID: &str = "CONTROL_SCHEMA=1\nCONTROL_PROTOCOL=agent-v1\nCONTROL_STATE=control\nCONTROL_TRANSPORT=kvm-vsock\nCONTROL_AUTHENTICATION=dvm-agent-hmac-sha256-v1\nCONTROL_CAPABILITIES=health,device-inventory,driver-inventory,input-stream\n";

    #[test]
    fn control_contract_is_strict() {
        let contract = ControlContract::parse(VALID, "test").unwrap();
        assert_eq!(
            contract.capabilities,
            [
                "health",
                "device-inventory",
                "driver-inventory",
                "input-stream"
            ]
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
        let hello = "HELLO\nrole=linux-driver-domain\nprotocol=agent-v1\nstate=control\ntransport=kvm-vsock\nauthentication=dvm-agent-hmac-sha256-v1\ncapabilities=health,device-inventory,driver-inventory,input-stream";
        assert!(validate_hello(hello, &contract).is_ok());
        assert!(
            validate_hello(
                &hello.replace(
                    "health,device-inventory,driver-inventory,input-stream",
                    "health"
                ),
                &contract
            )
            .is_err()
        );
        assert!(validate_hello(&format!("{hello}\nextra=unexpected"), &contract).is_err());
    }

    #[test]
    fn control_secret_and_proof_bind_each_session() {
        assert!(ControlSecret::from_bytes([0; CONTROL_SECRET_BYTES]).is_err());
        let secret = ControlSecret::from_bytes([0x5a; CONTROL_SECRET_BYTES]).unwrap();
        let hello = "HELLO\nrole=linux-driver-domain\nprotocol=agent-v1\nstate=control\ntransport=kvm-vsock\nauthentication=dvm-agent-hmac-sha256-v1\ncapabilities=health,device-inventory,driver-inventory,input-stream";
        let first = control_proof(&secret, &[1; CONTROL_SECRET_BYTES], hello).unwrap();
        assert_eq!(
            first,
            control_proof(&secret, &[1; CONTROL_SECRET_BYTES], hello).unwrap()
        );
        assert_ne!(
            first,
            control_proof(&secret, &[2; CONTROL_SECRET_BYTES], hello).unwrap()
        );
        assert_ne!(
            first,
            control_proof(
                &secret,
                &[1; CONTROL_SECRET_BYTES],
                &format!("{hello}\nextra=x")
            )
            .unwrap()
        );
    }

    #[test]
    fn control_endpoint_is_a_secret_derived_private_port() {
        let mut low_entropy = [0_u8; CONTROL_SECRET_BYTES];
        low_entropy[CONTROL_SECRET_BYTES - 1] = 1;
        let low = ControlSecret::from_bytes(low_entropy).unwrap();
        assert_eq!(low.control_port(), CONTROL_PORT_FLOOR);

        let high = ControlSecret::from_bytes([0xff; CONTROL_SECRET_BYTES]).unwrap();
        assert!(high.control_port() >= CONTROL_PORT_FLOOR);
        assert_ne!(low.control_port(), high.control_port());
        assert_eq!(high.control_port(), high.control_port());
    }

    #[test]
    fn control_secret_file_is_owner_private_and_not_a_symlink() {
        let sysfs = TestSysfs::new(&[]);
        fs::create_dir_all(sysfs.path()).unwrap();
        let secret_path = sysfs.path().join("dvm-control-secret");
        let secret = ControlSecret::from_bytes([0x5a; CONTROL_SECRET_BYTES]).unwrap();
        fs::write(&secret_path, format!("{}\n", secret.as_hex())).unwrap();
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            ControlSecret::from_hex_file(&secret_path).unwrap().as_hex(),
            secret.as_hex()
        );

        let symlink_path = sysfs.path().join("dvm-control-secret-link");
        symlink(&secret_path, &symlink_path).unwrap();
        assert!(ControlSecret::from_hex_file(&symlink_path).is_err());

        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(ControlSecret::from_hex_file(&secret_path).is_err());
    }

    #[test]
    fn control_messages_reject_duplicate_fields() {
        assert!(
            parse_message("HELLO\nrole=linux-driver-domain\nrole=linux-driver-domain").is_err()
        );
    }

    #[test]
    fn input_stream_requires_bounded_exact_key_and_pointer_events() {
        let event =
            parse_message("EVENT\nid=3\nop=input-stream\ntype=key\ncode=30\nvalue=1").unwrap();
        assert_eq!(
            parse_linux_evdev_input_event(&event, 3, "input-stream").unwrap(),
            super::LinuxEvdevInputEvent::Key(super::LinuxEvdevKeyEvent { code: 30, value: 1 })
        );
        let malformed =
            parse_message("EVENT\nid=3\nop=input-stream\ntype=key\ncode=768\nvalue=1").unwrap();
        assert!(parse_linux_evdev_input_event(&malformed, 3, "input-stream").is_err());
        let pointer = parse_message(
            "EVENT\nid=3\nop=input-stream\ntype=pointer\ndx=-4\ndy=2\nwheel-v=1\nwheel-h=0\nbuttons=3",
        )
        .unwrap();
        assert!(parse_linux_evdev_input_event(&pointer, 3, "input-stream").is_ok());
        let invalid_buttons = parse_message(
            "EVENT\nid=3\nop=input-stream\ntype=pointer\ndx=0\ndy=0\nwheel-v=0\nwheel-h=0\nbuttons=32",
        )
        .unwrap();
        assert!(parse_linux_evdev_input_event(&invalid_buttons, 3, "input-stream").is_err());
    }

    #[test]
    fn input_relay_frames_are_bounded_and_distinct_by_sequence() {
        let session = RustosInputFrame::session_start(7).unwrap();
        let first = RustosInputFrame::linux_evdev_key(7, 1, 30, 1).unwrap();
        let second = RustosInputFrame::linux_evdev_key(7, 2, 30, 0).unwrap();
        let pointer = RustosInputFrame::linux_evdev_pointer(7, 3, -4, 2, 1, 0, 3).unwrap();
        let end = RustosInputFrame::session_end(7, 4).unwrap();
        assert_ne!(session.as_bytes(), first.as_bytes());
        assert_ne!(first.as_bytes(), second.as_bytes());
        assert_ne!(second.as_bytes(), pointer.as_bytes());
        assert_ne!(pointer.as_bytes(), end.as_bytes());
        assert!(RustosInputFrame::linux_evdev_key(7, 1, 0, 1).is_err());
        assert!(RustosInputFrame::linux_evdev_key(7, 1, 30, 3).is_err());
        assert!(RustosInputFrame::linux_evdev_pointer(7, 3, 0, 0, 0, 0, 0x20).is_err());
    }

    #[test]
    fn input_relay_rate_guard_rejects_keyboard_floods() {
        let mut rate = InputRelayRate::new();
        let key = LinuxEvdevInputEvent::Key(LinuxEvdevKeyEvent { code: 30, value: 1 });
        for _ in 0..super::INPUT_RELAY_MAX_KEYS_PER_SECOND {
            assert!(rate.admit(key).is_ok());
        }
        assert!(rate.admit(key).is_err());
    }

    #[test]
    fn driver_domain_policy_names_one_explicit_transport_per_class() {
        let policy = DriverDomainPolicy::parse(
            "DRIVER_DOMAIN_POLICY_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nINPUT_TRANSPORT=rdi2-com2\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=disabled\n",
            "test",
        )
        .unwrap();
        assert_eq!(
            policy.transport_for(DeviceClass::Input),
            DeviceTransport::Rdi2Com2
        );
        assert_eq!(
            policy.transport_for(DeviceClass::Network),
            DeviceTransport::Disabled
        );
        assert!(DriverDomainPolicy::parse(
            "DRIVER_DOMAIN_POLICY_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nINPUT_TRANSPORT=rdi2-com2\nNETWORK_TRANSPORT=rdi2-com2\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=disabled\n",
            "test",
        )
        .is_err());
    }

    #[test]
    fn fleet_policy_requires_disjoint_domain_cid_group_and_pci_authority() {
        let policy = DriverDomainFleetPolicy::parse(
            "DRIVER_DOMAIN_FLEET_POLICY_SCHEMA=1\nFLEET_MEMBERS=linux-dvm-net0@4@17@0000:04:00.0+0000:04:00.1;linux-dvm-net1@5@18@0000:05:00.0\n",
            "test",
        )
        .unwrap();
        let first = ValidatedLease {
            domain_id: "linux-dvm-net0".to_owned(),
            dvm_guest_cid: 4,
            iommu_group: 17,
            pci_bdfs: vec!["0000:04:00.0".to_owned(), "0000:04:00.1".to_owned()],
        };
        policy.validate_for_lease(&first).unwrap();

        let wrong_cid = ValidatedLease {
            dvm_guest_cid: 5,
            ..first.clone()
        };
        assert!(policy.validate_for_lease(&wrong_cid).is_err());
        assert!(DriverDomainFleetPolicy::parse(
            "DRIVER_DOMAIN_FLEET_POLICY_SCHEMA=1\nFLEET_MEMBERS=linux-dvm-net0@4@17@0000:04:00.0;linux-dvm-net1@4@18@0000:05:00.0\n",
            "test",
        )
        .is_err());
        assert!(DriverDomainFleetPolicy::parse(
            "DRIVER_DOMAIN_FLEET_POLICY_SCHEMA=1\nFLEET_MEMBERS=linux-dvm-net0@4@17@0000:04:00.0;linux-dvm-net1@5@17@0000:05:00.0\n",
            "test",
        )
        .is_err());
        assert!(DriverDomainFleetPolicy::parse(
            "DRIVER_DOMAIN_FLEET_POLICY_SCHEMA=1\nFLEET_MEMBERS=linux-dvm-net0@4@17@0000:04:00.0;linux-dvm-net1@5@18@0000:04:00.0\n",
            "test",
        )
        .is_err());
    }

    #[test]
    fn release_authorization_binds_artifacts_policy_and_complete_iommu_group() {
        let lease = ValidatedLease {
            domain_id: "linux-dvm-net0".to_owned(),
            dvm_guest_cid: 4,
            iommu_group: 17,
            pci_bdfs: vec!["0000:04:00.0".to_owned(), "0000:04:00.1".to_owned()],
        };
        let digest = "a".repeat(64);
        let authorization = ReleaseAuthorization::parse(
            &format!(
                "RELEASE_AUTHORIZATION_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=17\nASSIGNED_PCI_BDFS=0000:04:00.0,0000:04:00.1\nDVM_ARTIFACT_MANIFEST_SHA256={digest}\nDEVICE_POLICY_SHA256={digest}\nFLEET_POLICY_SHA256={digest}\nNOT_BEFORE_UNIX=100\nNOT_AFTER_UNIX=200\n"
            ),
            "test",
        )
        .unwrap();
        authorization.validate_for_lease(&lease, 150).unwrap();
        assert_eq!(authorization.dvm_artifact_manifest_sha256(), digest);
        assert!(authorization.validate_for_lease(&lease, 99).is_err());
        assert!(authorization.validate_for_lease(&lease, 201).is_err());

        let wrong_group = ReleaseAuthorization::parse(
            &format!(
                "RELEASE_AUTHORIZATION_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=18\nASSIGNED_PCI_BDFS=0000:04:00.0,0000:04:00.1\nDVM_ARTIFACT_MANIFEST_SHA256={digest}\nDEVICE_POLICY_SHA256={digest}\nFLEET_POLICY_SHA256={digest}\nNOT_BEFORE_UNIX=100\nNOT_AFTER_UNIX=200\n"
            ),
            "test",
        )
        .unwrap();
        assert!(wrong_group.validate_for_lease(&lease, 150).is_err());
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
        let record = inspect_vfio_lease(&lease, &ops, release_binding()).unwrap();
        acquire_vfio_lease(&record, &mut ops, 150).unwrap();
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
        let record = inspect_vfio_lease(&lease, &ops, release_binding()).unwrap();
        assert!(acquire_vfio_lease(&record, &mut ops, 150).is_err());
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
    fn unsigned_vfio_preflight_cannot_persist_or_bind() {
        let root = TestSysfs::new(&[]);
        let lease = validated_lease(&["0000:02:00.0"]);
        let mut ops = FakeVfioOps::with_drivers(&[("0000:02:00.0", Some("first-driver"))]);
        let record = inspect_vfio_lease_preflight(&lease, &ops).unwrap();
        let store = FileLeaseStore::new(root.path().join("leases"));
        assert!(store.create_prepared(&record, 150).is_err());
        assert!(acquire_vfio_lease(&record, &mut ops, 150).is_err());
        assert_eq!(
            ops.current_driver("0000:02:00.0").unwrap().as_deref(),
            Some("first-driver")
        );
    }

    #[test]
    fn expired_release_binding_cannot_prepare_or_bind() {
        let root = TestSysfs::new(&[]);
        let lease = validated_lease(&["0000:02:00.0"]);
        let mut ops = FakeVfioOps::with_drivers(&[("0000:02:00.0", Some("first-driver"))]);
        let record = inspect_vfio_lease(&lease, &ops, release_binding()).unwrap();
        let store = FileLeaseStore::new(root.path().join("leases"));
        assert!(store.create_prepared(&record, 201).is_err());
        assert!(acquire_vfio_lease(&record, &mut ops, 201).is_err());
        assert_eq!(
            ops.current_driver("0000:02:00.0").unwrap().as_deref(),
            Some("first-driver")
        );
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
            release_binding: Some(release_binding()),
        };
        store.create_prepared(&record, 150).unwrap();
        assert_eq!(
            store.load("linux-dvm-net0").unwrap().state,
            VfioLeaseState::Prepared
        );
        store.mark_active(&mut record, 150).unwrap();
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

    fn release_binding() -> VfioReleaseBinding {
        let digest = "a".repeat(64);
        VfioReleaseBinding::new(&digest, &digest, &digest, &digest, 100, 200).unwrap()
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
