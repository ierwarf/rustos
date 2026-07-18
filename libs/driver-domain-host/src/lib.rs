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

pub mod ivshmem;
pub use ivshmem::{IvshmemDoorbellServer, IvshmemInputProducer};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
pub use driver_domain_protocol::{
    DVM_INPUT_RING_APERTURE_BYTES, DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
    DVM_INPUT_RING_FLAG_RUSTOS_READY, DVM_INPUT_RING_HEADER_BYTES, DVM_INPUT_RING_PRODUCER_OFFSET,
    DVM_INPUT_RING_RECORD_BYTES, DVM_INPUT_RING_SLOT_COUNT, DvmInputFrameError, DvmInputRingHeader,
    LINUX_EVDEV_KEY_MAX, RUSTOS_INPUT_FRAME_BYTES, RUSTOS_POINTER_BUTTON_MASK,
    RUSTOS_POINTER_POSITION_MAX_X, RUSTOS_POINTER_POSITION_MAX_Y, RustosInputFrame,
};

/// The DVM control listener port is derived from the owner-private per-launch
/// secret.  It is an endpoint capability, not a stable service discovery port:
/// an ordinary process sharing the DVM CID cannot hold the listener's setup
/// slot unless it can first read that root-only launch secret.
pub const CONTROL_PORT_FLOOR: u32 = 49_152;
pub const MAX_CONTROL_FRAME: usize = 4 * 1024;
pub const LINUX_DVM_ROLE: &str = "linux-driver-domain";

const INPUT_RELAY_MAX_SEQUENCE: u32 = u32::MAX - 1024;
const INPUT_STREAM_REQUEST_ID: u32 = 5;
// The Linux DVM relay coalesces relative pointer samples to 125Hz; L0 still
// enforces the resulting physical transport budget against a compromised or
// buggy DVM before it commits the fixed shared-memory input ring.
const INPUT_RELAY_MAX_FRAMES_PER_SECOND: u32 = 256;
// Every live hostd process allocates a relay epoch once. The fixed input ring
// carries that epoch plus a per-epoch frame sequence, so time/PID derived
// values are not sufficient: two rapid reconnects can collide. Stop before
// wrapping rather than silently reusing an authenticated epoch.
static NEXT_INPUT_EPOCH: AtomicU32 = AtomicU32::new(1);
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
            if line != raw_line {
                bail!("invalid {label} line {raw_line:?}");
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid {label} line {line:?}"))?;
            if key.is_empty()
                || value.is_empty()
                || key.contains(char::is_whitespace)
                || value.contains(char::is_whitespace)
                || values.insert(key, value).is_some()
            {
                bail!("invalid or duplicate {label} key {key:?}");
            }
        }

        const KEYS: [&str; 6] = [
            "CONTROL_SCHEMA",
            "CONTROL_PROTOCOL",
            "CONTROL_STATE",
            "CONTROL_TRANSPORT",
            "CONTROL_AUTHENTICATION",
            "CONTROL_CAPABILITIES",
        ];
        if values.len() != KEYS.len() || values.keys().any(|key| !KEYS.contains(key)) {
            bail!("unsupported {label} key set");
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
                "display-evidence-v1",
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
    /// Fixed RDI3 records in the host-owned input ring plus one MSI-X wake.
    InputRingMsix,
    /// Read-only RustOS snapshot slots exported by the GUI DVM as DMA-BUFs
    /// and imported by the DVM-owned DRM/KMS device for direct scanout.
    DisplayDmaBufKms,
}

impl DeviceTransport {
    fn parse(value: &str, key: &str, label: &str) -> Result<Self> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "input-ring-msix" if key == DeviceClass::Input.policy_key() => Ok(Self::InputRingMsix),
            "display-dmabuf-kms" if key == DeviceClass::Display.policy_key() => {
                Ok(Self::DisplayDmaBufKms)
            }
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
    qemu_sha256: String,
    transports: BTreeMap<DeviceClass, DeviceTransport>,
    physical_display: Option<PhysicalDisplayPolicy>,
}

/// Signed admission and runtime-evidence thresholds for the enabled physical
/// AMD display-DVM topology.  These values live in the device policy rather
/// than CLI flags so an operator cannot weaken the release gate after signing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalDisplayPolicy {
    driver: String,
    pci_vendor: u16,
    pci_device: u16,
    min_frame_hz_milli: u64,
    max_pageflip_latency_us: u64,
    max_atomic_commit_us: u64,
    max_sample_age_ms: u64,
    required_consecutive_samples: u32,
}

impl PhysicalDisplayPolicy {
    pub fn driver(&self) -> &str {
        &self.driver
    }

    pub fn pci_vendor(&self) -> u16 {
        self.pci_vendor
    }

    pub fn pci_device(&self) -> u16 {
        self.pci_device
    }

    pub fn required_consecutive_samples(&self) -> u32 {
        self.required_consecutive_samples
    }

    pub fn validate_evidence(&self, evidence: &DvmDisplayEvidence) -> Result<()> {
        if evidence.driver != self.driver
            || evidence.pci_vendor != self.pci_vendor
            || evidence.pci_device != self.pci_device
        {
            bail!(
                "DVM display evidence identity {} {:04x}:{:04x} does not match signed policy {} {:04x}:{:04x}",
                evidence.driver,
                evidence.pci_vendor,
                evidence.pci_device,
                self.driver,
                self.pci_vendor,
                self.pci_device
            );
        }
        if !evidence.direct_scanout || evidence.cpu_copy_us_avg != 0 {
            bail!("DVM display evidence does not prove zero-copy direct scanout");
        }
        if evidence.connector_id == 0 || evidence.mode_width == 0 || evidence.mode_height == 0 {
            bail!("DVM display evidence omitted the active physical mode");
        }
        if evidence.sample_age_ms > self.max_sample_age_ms {
            bail!(
                "DVM display evidence is stale age_ms={} max={}",
                evidence.sample_age_ms,
                self.max_sample_age_ms
            );
        }
        if evidence.window_ns < 500_000_000 || evidence.window_ns > 2_500_000_000 {
            bail!(
                "DVM display evidence window is outside the bounded sampling contract: {}ns",
                evidence.window_ns
            );
        }
        let computed_frame_hz_milli = evidence
            .pageflip_completions
            .checked_mul(1_000_000_000_000)
            .ok_or_else(|| anyhow!("DVM display evidence frame-rate calculation overflow"))?
            / evidence.window_ns;
        if computed_frame_hz_milli != evidence.frame_hz_milli {
            bail!(
                "DVM display evidence frame rate is inconsistent reported={} computed={}",
                evidence.frame_hz_milli,
                computed_frame_hz_milli
            );
        }
        if evidence.frame_hz_milli < self.min_frame_hz_milli {
            bail!(
                "DVM physical page-flip rate {}mHz is below signed minimum {}mHz",
                evidence.frame_hz_milli,
                self.min_frame_hz_milli
            );
        }
        if evidence.pageflip_latency_us_avg > self.max_pageflip_latency_us
            || evidence.pageflip_latency_us_max > self.max_pageflip_latency_us
            || evidence.pageflip_latency_us_avg == 0
            || evidence.pageflip_latency_us_max < evidence.pageflip_latency_us_avg
        {
            bail!(
                "DVM physical page-flip latency avg={}us max={}us exceeds signed maximum {}us",
                evidence.pageflip_latency_us_avg,
                evidence.pageflip_latency_us_max,
                self.max_pageflip_latency_us
            );
        }
        if evidence.atomic_commit_us_avg == 0
            || evidence.atomic_commit_us_avg > self.max_atomic_commit_us
        {
            bail!(
                "DVM atomic commit latency {}us exceeds signed maximum {}us",
                evidence.atomic_commit_us_avg,
                self.max_atomic_commit_us
            );
        }
        Ok(())
    }
}

impl DriverDomainPolicy {
    pub fn from_env_file(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("read driver-domain policy {}", path.display()))?;
        Self::parse(&source, &path.display().to_string())
    }

    pub fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
        const REQUIRED_V2: [&str; 7] = [
            "DRIVER_DOMAIN_POLICY_SCHEMA",
            "DOMAIN_ID",
            "QEMU_SHA256",
            "INPUT_TRANSPORT",
            "NETWORK_TRANSPORT",
            "BLOCK_TRANSPORT",
            "DISPLAY_TRANSPORT",
        ];
        const REQUIRED_V3: [&str; 15] = [
            "DRIVER_DOMAIN_POLICY_SCHEMA",
            "DOMAIN_ID",
            "QEMU_SHA256",
            "INPUT_TRANSPORT",
            "NETWORK_TRANSPORT",
            "BLOCK_TRANSPORT",
            "DISPLAY_TRANSPORT",
            "DISPLAY_DRIVER",
            "DISPLAY_PCI_VENDOR",
            "DISPLAY_PCI_DEVICE",
            "DISPLAY_MIN_FRAME_HZ_MILLI",
            "DISPLAY_MAX_PAGEFLIP_LATENCY_US",
            "DISPLAY_MAX_ATOMIC_COMMIT_US",
            "DISPLAY_MAX_SAMPLE_AGE_MS",
            "DISPLAY_REQUIRED_CONSECUTIVE_SAMPLES",
        ];
        let schema = launch_plan_value(&values, "DRIVER_DOMAIN_POLICY_SCHEMA", label)?;
        let required = match schema {
            "2" => &REQUIRED_V2[..],
            "3" => &REQUIRED_V3[..],
            _ => bail!("unsupported {label} schema"),
        };
        if values.len() != required.len()
            || values.keys().any(|key| !required.contains(&key.as_str()))
        {
            bail!("unsupported key in {label}");
        }
        let domain_id = launch_plan_value(&values, "DOMAIN_ID", label)?.to_owned();
        validate_domain_id(&domain_id, label)?;
        let qemu_sha256 = parse_sha256(launch_plan_value(&values, "QEMU_SHA256", label)?, label)?;
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
        let physical_display = if schema == "3" {
            if transports.get(&DeviceClass::Display) != Some(&DeviceTransport::DisplayDmaBufKms)
                || transports.get(&DeviceClass::Network) != Some(&DeviceTransport::Disabled)
                || transports.get(&DeviceClass::Block) != Some(&DeviceTransport::Disabled)
            {
                bail!("{label} schema 3 is reserved for the physical display-only DVM");
            }
            let driver = launch_plan_value(&values, "DISPLAY_DRIVER", label)?.to_owned();
            if driver != "amdgpu" {
                bail!("{label} physical display driver must be amdgpu");
            }
            let pci_vendor = parse_pci_id(
                launch_plan_value(&values, "DISPLAY_PCI_VENDOR", label)?,
                "DISPLAY_PCI_VENDOR",
                label,
            )?;
            let pci_device = parse_pci_id(
                launch_plan_value(&values, "DISPLAY_PCI_DEVICE", label)?,
                "DISPLAY_PCI_DEVICE",
                label,
            )?;
            if pci_vendor != 0x1002 {
                bail!("{label} physical display vendor must be AMD 1002");
            }
            let min_frame_hz_milli =
                parse_bounded_policy_u64(&values, "DISPLAY_MIN_FRAME_HZ_MILLI", 1, 240_000, label)?;
            let max_pageflip_latency_us = parse_bounded_policy_u64(
                &values,
                "DISPLAY_MAX_PAGEFLIP_LATENCY_US",
                1,
                1_000_000,
                label,
            )?;
            let max_atomic_commit_us = parse_bounded_policy_u64(
                &values,
                "DISPLAY_MAX_ATOMIC_COMMIT_US",
                1,
                1_000_000,
                label,
            )?;
            let max_sample_age_ms =
                parse_bounded_policy_u64(&values, "DISPLAY_MAX_SAMPLE_AGE_MS", 1, 10_000, label)?;
            let required_consecutive_samples = u32::try_from(parse_bounded_policy_u64(
                &values,
                "DISPLAY_REQUIRED_CONSECUTIVE_SAMPLES",
                2,
                30,
                label,
            )?)
            .expect("bounded display sample count fits u32");
            Some(PhysicalDisplayPolicy {
                driver,
                pci_vendor,
                pci_device,
                min_frame_hz_milli,
                max_pageflip_latency_us,
                max_atomic_commit_us,
                max_sample_age_ms,
                required_consecutive_samples,
            })
        } else {
            None
        };
        Ok(Self {
            domain_id,
            qemu_sha256,
            transports,
            physical_display,
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

    pub fn qemu_sha256(&self) -> &str {
        &self.qemu_sha256
    }

    pub fn physical_display(&self) -> Option<&PhysicalDisplayPolicy> {
        self.physical_display.as_ref()
    }
}

fn parse_pci_id(value: &str, key: &str, label: &str) -> Result<u16> {
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid {label} {key}");
    }
    u16::from_str_radix(value, 16).with_context(|| format!("invalid {label} {key}"))
}

fn parse_bounded_policy_u64(
    values: &BTreeMap<String, String>,
    key: &str,
    minimum: u64,
    maximum: u64,
    label: &str,
) -> Result<u64> {
    let value = launch_plan_value(values, key, label)?
        .parse::<u64>()
        .with_context(|| format!("invalid {label} {key}"))?;
    if !(minimum..=maximum).contains(&value) {
        bail!("{label} {key} is outside {minimum}..={maximum}");
    }
    Ok(value)
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
/// was verified. Every durable record carries this exact immutable evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VfioReleaseBinding {
    release_manifest_sha256: String,
    dvm_artifact_manifest_sha256: String,
    device_policy_sha256: String,
    fleet_policy_sha256: String,
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
            fleet_policy_sha256: parse_sha256(fleet_policy_sha256, "VFIO release binding")?,
            authorized_at_unix,
            authorization_not_after_unix,
        })
    }

    fn validate_at(&self, now_unix: u64) -> Result<()> {
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

/// Refuse a new VFIO assignment that would detach the L0 boot display or a
/// DRM device with a physically connected connector.  The launch plan's
/// `HOST_PROTECTED_PCI_BDFS` remains an operator-supplied deny-list, but it is
/// not authoritative evidence that a display is idle: that fact must be
/// derived from the same live sysfs snapshot used for assignment.
pub fn validate_host_display_assignment(lease: &ValidatedLease, sysfs_root: &Path) -> Result<()> {
    for bdf in &lease.pci_bdfs {
        let device = sysfs_root.join("bus/pci/devices").join(bdf);
        let metadata =
            fs::metadata(&device).with_context(|| format!("inspect host PCI function {bdf}"))?;
        if !metadata.is_dir() {
            bail!("host PCI function {bdf} is not a sysfs directory");
        }

        let boot_vga = device.join("boot_vga");
        match fs::read_to_string(&boot_vga) {
            Ok(value) => match value.trim() {
                "0" => {}
                "1" => bail!("refusing VFIO assignment of L0 boot display {bdf}"),
                other => bail!("invalid boot_vga value {other:?} for PCI function {bdf}"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("read boot_vga for PCI function {bdf}"));
            }
        }

        let drm = device.join("drm");
        let cards = match fs::read_dir(&drm) {
            Ok(cards) => cards,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read DRM devices for PCI function {bdf}"));
            }
        };
        for card in cards {
            let card = card?;
            if !card.file_type()?.is_dir() {
                continue;
            }
            for connector in fs::read_dir(card.path())
                .with_context(|| format!("read DRM connectors for PCI function {bdf}"))?
            {
                let connector = connector?;
                let status = connector.path().join("status");
                let value = match fs::read_to_string(&status) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("read DRM connector status for PCI function {bdf}")
                        });
                    }
                };
                match value.trim() {
                    "connected" => {
                        bail!(
                            "refusing VFIO assignment of L0 display {bdf} with connected DRM connector {}",
                            connector.file_name().to_string_lossy()
                        );
                    }
                    "disconnected" | "unknown" => {}
                    other => bail!("invalid DRM connector status {other:?} for PCI function {bdf}"),
                }
            }
        }
    }
    Ok(())
}

/// Bind the physical display release to one exact AMDGPU identity before any
/// VFIO mutation.  The complete group may contain an AMD audio function, but
/// a second display function or a network/storage function is out of scope and
/// therefore rejected rather than inheriting display-DVM authority.
pub fn validate_physical_display_assignment(
    lease: &ValidatedLease,
    sysfs_root: &Path,
    policy: &PhysicalDisplayPolicy,
) -> Result<()> {
    let bdf = validate_physical_display_identity(lease, sysfs_root, policy)?;
    let device = sysfs_root.join("bus/pci/devices").join(&bdf);
    let driver_path = fs::canonicalize(device.join("driver"))
        .with_context(|| format!("resolve current driver for physical display {bdf}"))?;
    let driver = driver_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid current driver for physical display {bdf}"))?;
    if driver != policy.driver {
        bail!(
            "physical display {bdf} is bound to {driver:?}, expected {:?}",
            policy.driver
        );
    }
    Ok(())
}

/// Recheck the signed PCI identity even after the function is bound to
/// `vfio-pci`.  This closes same-BDF device replacement between acquire and
/// supervise without requiring the original host driver to remain bound.
pub fn validate_physical_display_identity(
    lease: &ValidatedLease,
    sysfs_root: &Path,
    policy: &PhysicalDisplayPolicy,
) -> Result<String> {
    let mut display_bdf = None;
    for bdf in &lease.pci_bdfs {
        let device = sysfs_root.join("bus/pci/devices").join(bdf);
        let vendor = read_sysfs_pci_hex(&device.join("vendor"), 0xffff, "vendor", bdf)? as u16;
        let device_id = read_sysfs_pci_hex(&device.join("device"), 0xffff, "device", bdf)? as u16;
        let class = read_sysfs_pci_hex(&device.join("class"), 0xff_ffff, "class", bdf)?;
        let base_class = class >> 16;
        if matches!(base_class, 0x01 | 0x02) {
            bail!(
                "physical display IOMMU group contains excluded storage/network function {bdf} class={class:06x}"
            );
        }
        if base_class != 0x03 {
            if vendor != policy.pci_vendor || class >> 8 != 0x0403 {
                bail!(
                    "physical display IOMMU group contains unsupported companion function {bdf} vendor={vendor:04x} class={class:06x}"
                );
            }
            continue;
        }
        if display_bdf.replace(bdf.as_str()).is_some() {
            bail!("physical display IOMMU group contains more than one display function");
        }
        if vendor != policy.pci_vendor || device_id != policy.pci_device {
            bail!(
                "physical display {bdf} identity {vendor:04x}:{device_id:04x} does not match signed policy {:04x}:{:04x}",
                policy.pci_vendor,
                policy.pci_device
            );
        }
    }
    display_bdf.map(str::to_owned).ok_or_else(|| {
        anyhow!("physical display IOMMU group contains no display-class PCI function")
    })
}

fn read_sysfs_pci_hex(path: &Path, maximum: u32, field: &str, bdf: &str) -> Result<u32> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read PCI {field} for physical display assignment {bdf}"))?;
    let value = source
        .trim()
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("PCI {field} for {bdf} is missing 0x prefix"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid PCI {field} for {bdf}");
    }
    let parsed =
        u32::from_str_radix(value, 16).with_context(|| format!("invalid PCI {field} for {bdf}"))?;
    if parsed > maximum {
        bail!("out-of-range PCI {field} for {bdf}");
    }
    Ok(parsed)
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
    pub fn dvm_artifact_manifest_sha256(&self) -> Result<&str> {
        self.release_binding
            .as_ref()
            .map(|binding| binding.dvm_artifact_manifest_sha256.as_str())
            .ok_or_else(|| anyhow!("VFIO lease lacks DVM artifact binding"))
    }

    pub fn device_policy_sha256(&self) -> Result<&str> {
        self.release_binding
            .as_ref()
            .map(|binding| binding.device_policy_sha256.as_str())
            .ok_or_else(|| anyhow!("VFIO lease lacks device-policy binding"))
    }

    pub fn release_manifest_sha256(&self) -> Result<&str> {
        self.release_binding
            .as_ref()
            .map(|binding| binding.release_manifest_sha256.as_str())
            .ok_or_else(|| anyhow!("VFIO lease lacks release-manifest binding"))
    }

    pub fn authorization_valid_at(&self, now_unix: u64) -> Result<()> {
        self.release_binding
            .as_ref()
            .ok_or_else(|| anyhow!("VFIO lease lacks signed release authorization evidence"))?
            .validate_at(now_unix)
    }

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

    fn to_env(&self) -> Result<String> {
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
        let binding = self.release_binding.as_ref().ok_or_else(|| {
            anyhow!("durable VFIO lease lacks signed release authorization evidence")
        })?;
        Ok(format!(
            "VFIO_LEASE_SCHEMA=3\nLEASE_STATE={state}\nDOMAIN_ID={}\nDVM_GUEST_CID={}\nIOMMU_GROUP={}\nORIGINAL_DRIVERS={drivers}\nORIGINAL_DRIVER_OVERRIDES={overrides}\nRELEASE_MANIFEST_SHA256={}\nDVM_ARTIFACT_MANIFEST_SHA256={}\nDEVICE_POLICY_SHA256={}\nFLEET_POLICY_SHA256={}\nAUTHORIZED_AT_UNIX={}\nAUTHORIZATION_NOT_AFTER_UNIX={}\n",
            self.domain_id,
            self.dvm_guest_cid,
            self.iommu_group,
            binding.release_manifest_sha256,
            binding.dvm_artifact_manifest_sha256,
            binding.device_policy_sha256,
            binding.fleet_policy_sha256,
            binding.authorized_at_unix,
            binding.authorization_not_after_unix,
        ))
    }

    fn parse(source: &str, label: &str) -> Result<Self> {
        let values = parse_launch_plan_values(source, label)?;
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
        let encoded = record.to_env()?;
        write_new_private(&self.path_for(&record.domain_id)?, &encoded)?;
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
        let encoded = match record.to_env() {
            Ok(encoded) => encoded,
            Err(error) => {
                record.state = VfioLeaseState::Prepared;
                return Err(error);
            }
        };
        if let Err(error) = replace_private(&path, &encoded) {
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
    /// Reset one function after it is VFIO-bound and before assignment, and
    /// again after the guest has stopped but before the original driver is
    /// restored. Absence of the reset attribute is a failed commercial gate.
    fn reset_device(&mut self, bdf: &str) -> Result<()>;
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

    fn reset_device(&mut self, bdf: &str) -> Result<()> {
        validate_pci_bdf(bdf, "VFIO reset")?;
        let reset = self.device_path(bdf).join("reset");
        let metadata = fs::symlink_metadata(&reset)
            .with_context(|| format!("inspect PCI reset attribute for {bdf}"))?;
        if !metadata.file_type().is_file() {
            bail!("PCI reset attribute for {bdf} is not a regular sysfs file");
        }
        fs::write(&reset, b"1\n").with_context(|| format!("reset PCI device {bdf}"))
    }
}

/// Reset every function in deterministic order. A partial reset never grants
/// launch authority: callers must restore the complete durable lease instead.
pub fn reset_vfio_group(record: &VfioLeaseRecord, ops: &mut impl VfioOps) -> Result<()> {
    if record.state != VfioLeaseState::Active {
        bail!("VFIO group reset requires an active durable lease");
    }
    let mut failures = Vec::new();
    for bdf in record.original_drivers.keys() {
        let result = (|| {
            if ops.current_driver(bdf)?.as_deref() != Some("vfio-pci") {
                bail!("PCI device {bdf} is not VFIO-bound at reset");
            }
            ops.reset_device(bdf)
        })();
        if let Err(error) = result {
            failures.push(format!("{bdf}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("VFIO group reset incomplete: {}", failures.join("; "))
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
    if record.state == VfioLeaseState::Active {
        // Never hand a device that may retain guest-programmed queues back to
        // a host driver. If reset fails, leave it quarantined on vfio-pci and
        // retain the durable lease for explicit recovery.
        reset_vfio_group(record, ops)?;
    }
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
            if needs_rebind && let Some(driver) = original_driver {
                ops.bind_driver(bdf, driver)?;
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
    pub display_evidence: Option<DvmDisplayEvidence>,
}

/// DVM-local driver binding snapshot. These values prove only that the Linux
/// domain owns its virtual devices; they are deliberately not a claim that a
/// high-bandwidth RustOS data plane exists yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmDriverInventory {
    pub virtio_net_bound: bool,
    pub virtio_gpu_bound: bool,
    pub display_driver_bound: bool,
    pub display_relay_ready: bool,
}

/// One fresh, relay-produced physical page-flip sample returned through the
/// launch-authenticated DVM control channel.  Absence is valid for non-display
/// and virtual KVM probes, but a physical schema-3 policy never accepts it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DvmDisplayEvidence {
    pub sample_sequence: u64,
    pub sample_age_ms: u64,
    pub driver: String,
    pub pci_vendor: u16,
    pub pci_device: u16,
    pub guest_pci_bdf: String,
    pub connector_id: u32,
    pub mode_width: u32,
    pub mode_height: u32,
    pub direct_scanout: bool,
    pub window_ns: u64,
    pub frame_hz_milli: u64,
    pub pageflip_completions: u64,
    pub cpu_copy_us_avg: u64,
    pub pageflip_latency_us_avg: u64,
    pub pageflip_latency_us_max: u64,
    pub atomic_commit_us_avg: u64,
}

/// Destination controlled by L0 for sanitized DVM input frames.
pub trait RustosInputSink {
    /// Wait until RustOS has validated the exact shared ring and armed its one
    /// MSI-X receiver. This is boot admission, never a data-plane poll.
    fn wait_for_receiver_ready(&mut self, timeout: Duration) -> Result<()>;

    fn send_input_frame(&mut self, frame: &RustosInputFrame) -> Result<()>;

    /// Cleanup frames have reserved L0 queue capacity, so a DVM cannot fill
    /// normal traffic capacity and suppress its own releases/session end.
    fn send_input_cleanup_frame(&mut self, frame: &RustosInputFrame) -> Result<()> {
        self.send_input_frame(frame)
    }

    /// Flush every L0-admitted frame before reporting a terminal relay result.
    fn finish_input_relay(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Host producer for the one-way fixed ivshmem input ring. It maps the
/// launch-owned backing file itself, commits a complete validated L0 frame,
/// then signals RustOS's one MSI-X eventfd only for an empty-to-nonempty
/// transition (or authenticated cleanup). No serial queue, QMP path,
/// guest-selected descriptor, or data-plane retry is retained.
pub struct InputRingSink {
    producer: IvshmemInputProducer,
    mapped: *mut u8,
    mapped_len: usize,
    generation: u64,
    producer_cursor: u64,
}

unsafe impl Send for InputRingSink {}

impl InputRingSink {
    pub fn connect(doorbell: &Path, backing: &Path, timeout: Duration) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(backing)
            .with_context(|| format!("open input-ring backing {}", backing.display()))?;
        let mapped_len = usize::try_from(file.metadata().context("stat input-ring backing")?.len())
            .context("input-ring backing length does not fit usize")?;
        if mapped_len != DVM_INPUT_RING_APERTURE_BYTES as usize {
            bail!("input-ring backing has unexpected length {mapped_len}");
        }
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mapped_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        }
        .cast::<u8>();
        if mapped == libc::MAP_FAILED.cast::<u8>() {
            return Err(std::io::Error::last_os_error()).context("map input-ring backing");
        }
        let header = read_input_ring_header(mapped)?;
        if header.region_bytes != DVM_INPUT_RING_APERTURE_BYTES {
            unsafe { libc::munmap(mapped.cast::<libc::c_void>(), mapped_len) };
            bail!("input-ring backing did not contain the required fixed header");
        }
        let producer = match IvshmemInputProducer::connect(doorbell, timeout) {
            Ok(producer) => producer,
            Err(error) => {
                unsafe { libc::munmap(mapped.cast::<libc::c_void>(), mapped_len) };
                return Err(error);
            }
        };
        Ok(Self {
            producer,
            mapped,
            mapped_len,
            generation: header.generation,
            producer_cursor: header.producer,
        })
    }

    fn header(&self) -> Result<DvmInputRingHeader> {
        let header = read_input_ring_header(self.mapped)?;
        std::sync::atomic::fence(Ordering::Acquire);
        Ok(header)
    }

    fn write_frame(&mut self, frame: &RustosInputFrame, cleanup: bool) -> Result<()> {
        let header = self.header()?;
        if header.generation != self.generation || header.producer != self.producer_cursor {
            bail!("input-ring producer lifecycle changed while L0 relay was live");
        }
        let required_ready =
            DVM_INPUT_RING_FLAG_RUSTOS_READY | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY;
        if header.flags & required_ready != required_ready {
            bail!("RustOS revoked input-ring transport or policy-consumer readiness");
        }
        let outstanding = header.producer.saturating_sub(header.consumer);
        let cleanup_reserve = u64::from(LINUX_EVDEV_KEY_MAX) + 2;
        let normal_limit = u64::from(DVM_INPUT_RING_SLOT_COUNT).saturating_sub(cleanup_reserve);
        let limit = if cleanup {
            u64::from(DVM_INPUT_RING_SLOT_COUNT)
        } else {
            normal_limit
        };
        if outstanding >= limit {
            bail!(
                "fixed input-ring is saturated outstanding={outstanding} limit={limit} cleanup={cleanup}"
            );
        }
        let offset = usize::try_from(DvmInputRingHeader::record_offset(header.producer))
            .context("input-ring record offset does not fit usize")?;
        let end = offset
            .checked_add(DVM_INPUT_RING_RECORD_BYTES)
            .filter(|end| *end <= self.mapped_len)
            .ok_or_else(|| anyhow!("input-ring record is outside launch-owned aperture"))?;
        for (index, byte) in frame.as_bytes().iter().enumerate() {
            unsafe { self.mapped.add(offset + index).write_volatile(*byte) };
        }
        debug_assert_eq!(end - offset, RUSTOS_INPUT_FRAME_BYTES);
        std::sync::atomic::fence(Ordering::Release);
        let next = header
            .producer
            .checked_add(1)
            .ok_or_else(|| anyhow!("input-ring producer wrapped"))?;
        unsafe {
            self.mapped
                .add(DVM_INPUT_RING_PRODUCER_OFFSET)
                .cast::<u64>()
                .write_volatile(next.to_le());
        }
        self.producer_cursor = next;
        // The inputd worker rechecks producer/consumer after it arms its
        // dedicated wait slot, so one edge for an empty-to-nonempty transition
        // is sufficient. Ringing for every pointer frame forces avoidable
        // MSI-X exits and priority handoffs that compete with presentation.
        // Cleanup remains urgent even behind normal data, because releases
        // must not wait for an already-coalesced producer batch to empty.
        if input_doorbell_needed(outstanding, cleanup) {
            self.producer.notify_rustos()?;
        }
        Ok(())
    }
}

fn input_doorbell_needed(outstanding: u64, cleanup: bool) -> bool {
    cleanup || outstanding == 0
}

impl Drop for InputRingSink {
    fn drop(&mut self) {
        if !self.mapped.is_null() {
            unsafe { libc::munmap(self.mapped.cast::<libc::c_void>(), self.mapped_len) };
        }
    }
}

impl RustosInputSink for InputRingSink {
    fn wait_for_receiver_ready(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let header = self.header()?;
            if header.generation == self.generation
                && header.flags & DVM_INPUT_RING_FLAG_RUSTOS_READY != 0
                && header.flags & DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY != 0
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "RustOS did not arm both the fixed input-ring vector and a live policy consumer before deadline"
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn send_input_frame(&mut self, frame: &RustosInputFrame) -> Result<()> {
        self.write_frame(frame, false)
    }

    fn send_input_cleanup_frame(&mut self, frame: &RustosInputFrame) -> Result<()> {
        self.write_frame(frame, true)
    }
}

fn read_input_ring_header(mapped: *const u8) -> Result<DvmInputRingHeader> {
    let mut bytes = [0_u8; DvmInputRingHeader::encoded_len()];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { mapped.add(index).read_volatile() };
    }
    DvmInputRingHeader::decode(&bytes).ok_or_else(|| anyhow!("invalid fixed input-ring header"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputRelayResult {
    pub probe: ProbeResult,
    pub forwarded_events: u64,
}

/// L0-side admission guard. It rejects an abusive DVM stream before it can
/// consume the fixed ring's cleanup reserve or starve inputd's bounded broker.
struct InputRelayRate {
    window_started: Instant,
    last_frame_at: Option<Instant>,
    max_inter_frame_gap: Duration,
    total_frames: u32,
    key_frames: u32,
}

impl InputRelayRate {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            last_frame_at: None,
            max_inter_frame_gap: Duration::ZERO,
            total_frames: 0,
            key_frames: 0,
        }
    }

    fn admit(&mut self, event: LinuxEvdevInputEvent) -> Result<()> {
        let now = Instant::now();
        if let Some(previous) = self.last_frame_at {
            self.max_inter_frame_gap = self
                .max_inter_frame_gap
                .max(now.saturating_duration_since(previous));
        }
        self.last_frame_at = Some(now);
        if now.saturating_duration_since(self.window_started) >= Duration::from_secs(1) {
            if std::env::var_os("RUSTOS_DVM_INPUT_PROFILE").as_deref()
                == Some(std::ffi::OsStr::new("1"))
            {
                eprintln!(
                    "rustos-dvm-input-relay: frames={} max_gap_us={}",
                    self.total_frames,
                    self.max_inter_frame_gap.as_micros()
                );
            }
            self.window_started = now;
            self.max_inter_frame_gap = Duration::ZERO;
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
        self.relay_input_once_inner(Some(timeout), timeout, sink, |_| Ok(()))
    }

    /// Like [`Self::relay_input_once`], but reports the verified DVM endpoint
    /// after the RustOS relay session is installed and before waiting for the
    /// first human input event.  KVM smoke uses this to prove endpoint setup
    /// without reintroducing synthetic QMP input.
    pub fn relay_input_once_with_ready(
        &self,
        setup_timeout: Duration,
        receiver_timeout: Duration,
        sink: &mut impl RustosInputSink,
        on_ready: impl FnOnce(&ProbeResult) -> Result<()>,
    ) -> Result<InputRelayResult> {
        self.relay_input_once_inner(Some(setup_timeout), receiver_timeout, sink, on_ready)
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
        self.relay_input_once_inner(
            Some(Duration::from_secs(5)),
            Duration::from_secs(30),
            sink,
            |_| Ok(()),
        )
    }

    fn relay_input_once_inner(
        &self,
        setup_timeout: Option<Duration>,
        receiver_timeout: Duration,
        sink: &mut impl RustosInputSink,
        on_ready: impl FnOnce(&ProbeResult) -> Result<()>,
    ) -> Result<InputRelayResult> {
        let mut connection = self.accept_authenticated(setup_timeout)?;
        let probe = self.probe_connection(&mut connection)?;
        // RustOS must have validated the fixed ring and armed its wake vector
        // before L0 asks the DVM agent to open the input stream. In the KVM
        // self-test that request arms uinput immediately, so this admission
        // prevents a boot-time producer from racing an uninstalled consumer.
        sink.wait_for_receiver_ready(receiver_timeout)?;
        let ready = request(&mut connection, INPUT_STREAM_REQUEST_ID, "input-stream")?;
        if !has_exact_fields(
            &ready,
            &["id", "op", "status", "format", "keyboard", "pointer"],
        ) || ready.get("status") != Some(&"ready".to_owned())
            || ready.get("format") != Some(&"linux-evdev-v3".to_owned())
            || !valid_evdev_endpoint(ready.get("keyboard"))
            || !valid_evdev_endpoint(ready.get("pointer"))
        {
            bail!("Linux DVM did not acknowledge input stream readiness");
        }

        let epoch = new_input_epoch()?;
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
            let _ = sink.send_input_cleanup_frame(&RustosInputFrame::session_end(epoch, 1)?);
            let _ = sink.finish_input_relay();
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
            let event = match parse_linux_evdev_input_event(
                &message,
                INPUT_STREAM_REQUEST_ID,
                "input-stream",
            ) {
                Ok(event) => event,
                Err(error) => break Err(error),
            };
            if let Err(error) = rate.admit(event) {
                break Err(error);
            }
            let frame: Result<RustosInputFrame> = match event {
                LinuxEvdevInputEvent::Key(event) => {
                    if event.value == 0 {
                        pressed_keys.remove(&event.code);
                    } else if event.value == 1 {
                        pressed_keys.insert(event.code);
                    }
                    RustosInputFrame::linux_evdev_key(epoch, next_sequence, event.code, event.value)
                        .map_err(Into::into)
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
                    .map_err(Into::into)
                }
                LinuxEvdevInputEvent::PointerPosition(event) => {
                    pointer_buttons = event.buttons;
                    RustosInputFrame::linux_evdev_pointer_position(
                        epoch,
                        next_sequence,
                        event.x,
                        event.y,
                        event.wheel_vertical,
                        event.wheel_horizontal,
                        event.buttons,
                    )
                    .map_err(Into::into)
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
        )
        .and_then(|()| sink.finish_input_relay());
        match (relay_result, cleanup) {
            (Err(error), Ok(())) => Err(error).context(format!(
                "RustOS input relay failed after forwarding {forwarded_events} events"
            )),
            (Err(error), Err(cleanup_error)) => Err(error).context(format!(
                "RustOS input relay cleanup failed after forwarding {forwarded_events} events: {cleanup_error:#}"
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
            &[
                "id",
                "op",
                "status",
                "virtio-net",
                "virtio-gpu",
                "display-driver",
                "display-relay",
            ],
        ) || drivers.get("status") != Some(&"ok".to_owned())
        {
            bail!("Linux DVM driver inventory probe was not ready");
        }
        let driver_inventory = DvmDriverInventory {
            virtio_net_bound: parse_driver_inventory_state(&drivers, "virtio-net")?,
            virtio_gpu_bound: parse_driver_inventory_state(&drivers, "virtio-gpu")?,
            display_driver_bound: parse_driver_inventory_state(&drivers, "display-driver")?,
            display_relay_ready: parse_ready_inventory_state(&drivers, "display-relay")?,
        };
        let display = request(connection, 4, "display-evidence-v1")?;
        let display_evidence = parse_display_evidence(&display)?;
        Ok(ProbeResult {
            peer_cid: self.expected_dvm_cid,
            inventory_count,
            driver_inventory,
            display_evidence,
        })
    }
}

fn parse_display_evidence(fields: &BTreeMap<String, String>) -> Result<Option<DvmDisplayEvidence>> {
    const REQUIRED: [&str; 20] = [
        "id",
        "op",
        "status",
        "sample-sequence",
        "sample-age-ms",
        "driver",
        "pci-vendor",
        "pci-device",
        "guest-pci-bdf",
        "connector-id",
        "mode-width",
        "mode-height",
        "direct-scanout",
        "window-ns",
        "frame-hz-milli",
        "pageflip-completions",
        "cpu-copy-us-avg",
        "pageflip-latency-us-avg",
        "pageflip-latency-us-max",
        "atomic-commit-us-avg",
    ];
    if !has_exact_fields(fields, &REQUIRED)
        || fields.get("op") != Some(&"display-evidence-v1".to_owned())
    {
        bail!("invalid Linux DVM display evidence response");
    }
    match fields.get("status").map(String::as_str) {
        Some("unavailable") => {
            for key in [
                "sample-sequence",
                "sample-age-ms",
                "connector-id",
                "mode-width",
                "mode-height",
                "window-ns",
                "frame-hz-milli",
                "pageflip-completions",
                "cpu-copy-us-avg",
                "pageflip-latency-us-avg",
                "pageflip-latency-us-max",
                "atomic-commit-us-avg",
            ] {
                if fields.get(key).map(String::as_str) != Some("0") {
                    bail!("nonzero field in unavailable Linux DVM display evidence");
                }
            }
            if fields.get("driver").map(String::as_str) != Some("missing")
                || fields.get("pci-vendor").map(String::as_str) != Some("0000")
                || fields.get("pci-device").map(String::as_str) != Some("0000")
                || fields.get("guest-pci-bdf").map(String::as_str) != Some("none")
                || fields.get("direct-scanout").map(String::as_str) != Some("no")
            {
                bail!("invalid unavailable Linux DVM display evidence sentinel");
            }
            Ok(None)
        }
        Some("ok") => {
            let driver = fields
                .get("driver")
                .ok_or_else(|| anyhow!("Linux DVM display evidence omitted driver"))?
                .to_owned();
            validate_driver_name(&driver, "Linux DVM display evidence")?;
            let pci_vendor = parse_pci_id_field(fields, "pci-vendor")?;
            let pci_device = parse_pci_id_field(fields, "pci-device")?;
            let guest_pci_bdf = fields
                .get("guest-pci-bdf")
                .ok_or_else(|| anyhow!("Linux DVM display evidence omitted guest PCI BDF"))?
                .to_owned();
            validate_pci_bdf(&guest_pci_bdf, "Linux DVM display evidence")?;
            let evidence = DvmDisplayEvidence {
                sample_sequence: parse_display_u64(fields, "sample-sequence")?,
                sample_age_ms: parse_display_u64(fields, "sample-age-ms")?,
                driver,
                pci_vendor,
                pci_device,
                guest_pci_bdf,
                connector_id: u32::try_from(parse_display_u64(fields, "connector-id")?)
                    .context("Linux DVM display connector ID overflow")?,
                mode_width: u32::try_from(parse_display_u64(fields, "mode-width")?)
                    .context("Linux DVM display width overflow")?,
                mode_height: u32::try_from(parse_display_u64(fields, "mode-height")?)
                    .context("Linux DVM display height overflow")?,
                direct_scanout: match fields.get("direct-scanout").map(String::as_str) {
                    Some("yes") => true,
                    Some("no") => false,
                    _ => bail!("invalid Linux DVM direct-scanout evidence"),
                },
                window_ns: parse_display_u64(fields, "window-ns")?,
                frame_hz_milli: parse_display_u64(fields, "frame-hz-milli")?,
                pageflip_completions: parse_display_u64(fields, "pageflip-completions")?,
                cpu_copy_us_avg: parse_display_u64(fields, "cpu-copy-us-avg")?,
                pageflip_latency_us_avg: parse_display_u64(fields, "pageflip-latency-us-avg")?,
                pageflip_latency_us_max: parse_display_u64(fields, "pageflip-latency-us-max")?,
                atomic_commit_us_avg: parse_display_u64(fields, "atomic-commit-us-avg")?,
            };
            if evidence.sample_sequence == 0 || evidence.pageflip_completions == 0 {
                bail!("Linux DVM display evidence lacks a completed physical page flip");
            }
            Ok(Some(evidence))
        }
        _ => bail!("invalid Linux DVM display evidence status"),
    }
}

fn parse_pci_id_field(fields: &BTreeMap<String, String>, key: &str) -> Result<u16> {
    let value = fields
        .get(key)
        .ok_or_else(|| anyhow!("Linux DVM display evidence omitted {key}"))?;
    if value.len() != 4
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid Linux DVM display evidence {key}");
    }
    u16::from_str_radix(value, 16).with_context(|| format!("invalid display evidence {key}"))
}

fn parse_display_u64(fields: &BTreeMap<String, String>, key: &str) -> Result<u64> {
    fields
        .get(key)
        .ok_or_else(|| anyhow!("Linux DVM display evidence omitted {key}"))?
        .parse::<u64>()
        .with_context(|| format!("invalid Linux DVM display evidence {key}"))
}

fn parse_driver_inventory_state(fields: &BTreeMap<String, String>, key: &str) -> Result<bool> {
    match fields.get(key).map(String::as_str) {
        Some("bound") => Ok(true),
        Some("missing") => Ok(false),
        Some(value) => bail!("invalid Linux DVM driver inventory state {key}={value:?}"),
        None => bail!("Linux DVM driver inventory omitted {key}"),
    }
}

fn parse_ready_inventory_state(fields: &BTreeMap<String, String>, key: &str) -> Result<bool> {
    match fields.get(key).map(String::as_str) {
        Some("ready") => Ok(true),
        Some("missing") => Ok(false),
        Some(value) => bail!("invalid Linux DVM readiness inventory state {key}={value:?}"),
        None => bail!("Linux DVM readiness inventory omitted {key}"),
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
struct LinuxEvdevPointerPositionEvent {
    x: u16,
    y: u16,
    wheel_vertical: i16,
    wheel_horizontal: i16,
    buttons: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxEvdevInputEvent {
    Key(LinuxEvdevKeyEvent),
    Pointer(LinuxEvdevPointerEvent),
    PointerPosition(LinuxEvdevPointerPositionEvent),
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
        Some("pointer-position") => parse_linux_evdev_pointer_position_event(message),
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

fn parse_linux_evdev_pointer_position_event(message: &Message) -> Result<LinuxEvdevInputEvent> {
    if !has_exact_fields(
        &message.fields,
        &[
            "id", "op", "type", "x", "y", "wheel-v", "wheel-h", "buttons",
        ],
    ) {
        bail!("invalid Linux DVM absolute pointer event fields");
    }
    let parse_axis = |field: &str| -> Result<i16> {
        message
            .fields
            .get(field)
            .ok_or_else(|| anyhow!("Linux DVM absolute pointer event omitted {field}"))?
            .parse::<i16>()
            .with_context(|| format!("invalid Linux DVM absolute pointer {field}"))
    };
    let x = message
        .fields
        .get("x")
        .ok_or_else(|| anyhow!("Linux DVM absolute pointer event omitted x"))?
        .parse::<u16>()
        .context("invalid Linux DVM absolute pointer x")?;
    let y = message
        .fields
        .get("y")
        .ok_or_else(|| anyhow!("Linux DVM absolute pointer event omitted y"))?
        .parse::<u16>()
        .context("invalid Linux DVM absolute pointer y")?;
    let buttons = message
        .fields
        .get("buttons")
        .ok_or_else(|| anyhow!("Linux DVM absolute pointer event omitted buttons"))?
        .parse::<u8>()
        .context("invalid Linux DVM absolute pointer buttons")?;
    if x > RUSTOS_POINTER_POSITION_MAX_X
        || y > RUSTOS_POINTER_POSITION_MAX_Y
        || buttons & !RUSTOS_POINTER_BUTTON_MASK != 0
    {
        bail!("rejected out-of-range Linux DVM absolute pointer event");
    }
    Ok(LinuxEvdevInputEvent::PointerPosition(
        LinuxEvdevPointerPositionEvent {
            x,
            y,
            wheel_vertical: parse_axis("wheel-v")?,
            wheel_horizontal: parse_axis("wheel-h")?,
            buttons,
        },
    ))
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
        send_input_cleanup_frame_with_sequence(sink, next_sequence, |sequence| {
            RustosInputFrame::linux_evdev_key(epoch, sequence, code, 0)
        })?;
    }
    if pointer_buttons != 0 {
        send_input_cleanup_frame_with_sequence(sink, next_sequence, |sequence| {
            RustosInputFrame::linux_evdev_pointer(epoch, sequence, 0, 0, 0, 0, 0)
        })?;
    }
    send_input_cleanup_frame_with_sequence(sink, next_sequence, |sequence| {
        RustosInputFrame::session_end(epoch, sequence)
    })
}

fn send_input_cleanup_frame_with_sequence(
    sink: &mut impl RustosInputSink,
    next_sequence: &mut u32,
    make_frame: impl FnOnce(u32) -> core::result::Result<RustosInputFrame, DvmInputFrameError>,
) -> Result<()> {
    if *next_sequence == 0 || *next_sequence > u32::MAX - 1 {
        bail!("RustOS input relay has no sequence reserved for cleanup");
    }
    let frame = make_frame(*next_sequence)?;
    sink.send_input_cleanup_frame(&frame)?;
    *next_sequence += 1;
    Ok(())
}

fn new_input_epoch() -> Result<u32> {
    allocate_input_epoch(&NEXT_INPUT_EPOCH)
}

fn allocate_input_epoch(next: &AtomicU32) -> Result<u32> {
    loop {
        let epoch = next.load(Ordering::Relaxed);
        if epoch == 0 || epoch == u32::MAX {
            bail!("RustOS input relay epoch space exhausted; restart hostd before reconnecting");
        }
        if next
            .compare_exchange_weak(epoch, epoch + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(epoch);
        }
    }
}

fn required(values: &BTreeMap<&str, &str>, key: &str, expected: &str, label: &str) -> Result<()> {
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
    use std::sync::atomic::AtomicU32;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        CONTROL_PORT_FLOOR, CONTROL_SECRET_BYTES, ControlContract, ControlSecret, DeviceClass,
        DeviceTransport, DriverDomainFleetPolicy, DriverDomainPolicy, FileLeaseStore,
        INPUT_STREAM_REQUEST_ID, InputRelayRate, IommuTopology, LaunchPlan, LinuxEvdevInputEvent,
        LinuxEvdevKeyEvent, ReleaseAuthorization, RustosInputFrame, ValidatedLease,
        VfioLeaseRecord, VfioLeaseState, VfioOps, VfioReleaseBinding, acquire_vfio_lease,
        allocate_input_epoch, control_proof, inspect_vfio_lease, inspect_vfio_lease_preflight,
        parse_display_evidence, parse_linux_evdev_input_event, parse_message, reset_vfio_group,
        restore_vfio_lease, validate_hello, validate_host_display_assignment,
        validate_physical_display_assignment,
    };
    use anyhow::Result;

    const VALID: &str = "CONTROL_SCHEMA=1\nCONTROL_PROTOCOL=agent-v1\nCONTROL_STATE=control\nCONTROL_TRANSPORT=kvm-vsock\nCONTROL_AUTHENTICATION=dvm-agent-hmac-sha256-v1\nCONTROL_CAPABILITIES=health,device-inventory,driver-inventory,display-evidence-v1,input-stream\n";

    #[test]
    fn relay_epochs_are_monotonic_and_fail_closed_before_reuse() {
        let next = AtomicU32::new(41);
        assert_eq!(allocate_input_epoch(&next).unwrap(), 41);
        assert_eq!(allocate_input_epoch(&next).unwrap(), 42);

        let exhausted = AtomicU32::new(u32::MAX);
        assert!(allocate_input_epoch(&exhausted).is_err());
    }

    #[test]
    fn control_contract_is_strict() {
        let contract = ControlContract::parse(VALID, "test").unwrap();
        assert_eq!(
            contract.capabilities,
            [
                "health",
                "device-inventory",
                "driver-inventory",
                "display-evidence-v1",
                "input-stream"
            ]
        );
        assert!(ControlContract::parse(&VALID.replace("control", "pretransport"), "test").is_err());
        assert!(
            ControlContract::parse(&VALID.replace("device-inventory", "network-rx"), "test")
                .is_err()
        );
        assert!(ControlContract::parse(&format!("{VALID}CONTROL_DEBUG=yes\n"), "test").is_err());
        assert!(
            ControlContract::parse(&VALID.replace("CONTROL_STATE", " CONTROL_STATE"), "test")
                .is_err()
        );
    }

    #[test]
    fn hello_is_bound_to_the_exact_control_contract() {
        let contract = ControlContract::parse(VALID, "test").unwrap();
        let hello = "HELLO\nrole=linux-driver-domain\nprotocol=agent-v1\nstate=control\ntransport=kvm-vsock\nauthentication=dvm-agent-hmac-sha256-v1\ncapabilities=health,device-inventory,driver-inventory,display-evidence-v1,input-stream";
        assert!(validate_hello(hello, &contract).is_ok());
        assert!(
            validate_hello(
                &hello.replace(
                    "health,device-inventory,driver-inventory,display-evidence-v1,input-stream",
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
        let hello = "HELLO\nrole=linux-driver-domain\nprotocol=agent-v1\nstate=control\ntransport=kvm-vsock\nauthentication=dvm-agent-hmac-sha256-v1\ncapabilities=health,device-inventory,driver-inventory,display-evidence-v1,input-stream";
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
    fn display_evidence_is_exact_fresh_and_zero_copy() {
        let policy = DriverDomainPolicy::parse(
            &format!("DRIVER_DOMAIN_POLICY_SCHEMA=3\nDOMAIN_ID=linux-dvm-gpu0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=display-dmabuf-kms\nDISPLAY_DRIVER=amdgpu\nDISPLAY_PCI_VENDOR=1002\nDISPLAY_PCI_DEVICE=1900\nDISPLAY_MIN_FRAME_HZ_MILLI=59000\nDISPLAY_MAX_PAGEFLIP_LATENCY_US=25000\nDISPLAY_MAX_ATOMIC_COMMIT_US=2000\nDISPLAY_MAX_SAMPLE_AGE_MS=2000\nDISPLAY_REQUIRED_CONSECUTIVE_SAMPLES=5\n", "a".repeat(64)),
            "evidence-policy",
        )
        .unwrap();
        let message = parse_message(
            "RESPONSE\nid=4\nop=display-evidence-v1\nstatus=ok\nsample-sequence=7\nsample-age-ms=20\ndriver=amdgpu\npci-vendor=1002\npci-device=1900\nguest-pci-bdf=0000:01:00.0\nconnector-id=78\nmode-width=1920\nmode-height=1080\ndirect-scanout=yes\nwindow-ns=1000000000\nframe-hz-milli=60000\npageflip-completions=60\ncpu-copy-us-avg=0\npageflip-latency-us-avg=16500\npageflip-latency-us-max=17100\natomic-commit-us-avg=40",
        )
        .unwrap();
        let evidence = parse_display_evidence(&message.fields).unwrap().unwrap();
        assert_eq!(evidence.driver, "amdgpu");
        assert_eq!(evidence.pci_vendor, 0x1002);
        assert!(evidence.direct_scanout);
        policy
            .physical_display()
            .unwrap()
            .validate_evidence(&evidence)
            .unwrap();

        let copied = parse_message(
            "RESPONSE\nid=4\nop=display-evidence-v1\nstatus=ok\nsample-sequence=7\nsample-age-ms=20\ndriver=amdgpu\npci-vendor=1002\npci-device=1900\nguest-pci-bdf=0000:01:00.0\nconnector-id=78\nmode-width=1920\nmode-height=1080\ndirect-scanout=yes\nwindow-ns=1000000000\nframe-hz-milli=60000\npageflip-completions=60\ncpu-copy-us-avg=1\npageflip-latency-us-avg=16500\npageflip-latency-us-max=17100\natomic-commit-us-avg=40",
        )
        .unwrap();
        let evidence = parse_display_evidence(&copied.fields).unwrap().unwrap();
        assert!(
            policy
                .physical_display()
                .unwrap()
                .validate_evidence(&evidence)
                .is_err()
        );

        let inconsistent = parse_message(
            "RESPONSE\nid=4\nop=display-evidence-v1\nstatus=ok\nsample-sequence=8\nsample-age-ms=20\ndriver=amdgpu\npci-vendor=1002\npci-device=1900\nguest-pci-bdf=0000:01:00.0\nconnector-id=78\nmode-width=1920\nmode-height=1080\ndirect-scanout=yes\nwindow-ns=1000000000\nframe-hz-milli=60000\npageflip-completions=59\ncpu-copy-us-avg=0\npageflip-latency-us-avg=16500\npageflip-latency-us-max=17100\natomic-commit-us-avg=40",
        )
        .unwrap();
        let evidence = parse_display_evidence(&inconsistent.fields)
            .unwrap()
            .unwrap();
        assert!(
            policy
                .physical_display()
                .unwrap()
                .validate_evidence(&evidence)
                .is_err()
        );

        let unavailable = parse_message(
            "RESPONSE\nid=4\nop=display-evidence-v1\nstatus=unavailable\nsample-sequence=0\nsample-age-ms=0\ndriver=missing\npci-vendor=0000\npci-device=0000\nguest-pci-bdf=none\nconnector-id=0\nmode-width=0\nmode-height=0\ndirect-scanout=no\nwindow-ns=0\nframe-hz-milli=0\npageflip-completions=0\ncpu-copy-us-avg=0\npageflip-latency-us-avg=0\npageflip-latency-us-max=0\natomic-commit-us-avg=0",
        )
        .unwrap();
        assert!(
            parse_display_evidence(&unavailable.fields)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn input_stream_requires_bounded_exact_key_and_pointer_events() {
        let event =
            parse_message("EVENT\nid=5\nop=input-stream\ntype=key\ncode=30\nvalue=1").unwrap();
        assert_eq!(
            parse_linux_evdev_input_event(&event, INPUT_STREAM_REQUEST_ID, "input-stream").unwrap(),
            super::LinuxEvdevInputEvent::Key(super::LinuxEvdevKeyEvent { code: 30, value: 1 })
        );
        let malformed =
            parse_message("EVENT\nid=5\nop=input-stream\ntype=key\ncode=768\nvalue=1").unwrap();
        assert!(
            parse_linux_evdev_input_event(&malformed, INPUT_STREAM_REQUEST_ID, "input-stream")
                .is_err()
        );
        let pointer = parse_message(
            "EVENT\nid=5\nop=input-stream\ntype=pointer\ndx=-4\ndy=2\nwheel-v=1\nwheel-h=0\nbuttons=3",
        )
        .unwrap();
        assert!(
            parse_linux_evdev_input_event(&pointer, INPUT_STREAM_REQUEST_ID, "input-stream")
                .is_ok()
        );
        let invalid_buttons = parse_message(
            "EVENT\nid=5\nop=input-stream\ntype=pointer\ndx=0\ndy=0\nwheel-v=0\nwheel-h=0\nbuttons=32",
        )
        .unwrap();
        assert!(
            parse_linux_evdev_input_event(
                &invalid_buttons,
                INPUT_STREAM_REQUEST_ID,
                "input-stream"
            )
            .is_err()
        );
        let position = parse_message(
            "EVENT\nid=5\nop=input-stream\ntype=pointer-position\nx=800\ny=450\nwheel-v=0\nwheel-h=0\nbuttons=0",
        )
        .unwrap();
        assert!(
            parse_linux_evdev_input_event(&position, INPUT_STREAM_REQUEST_ID, "input-stream")
                .is_ok()
        );
        let invalid_position = parse_message(
            "EVENT\nid=5\nop=input-stream\ntype=pointer-position\nx=1600\ny=450\nwheel-v=0\nwheel-h=0\nbuttons=0",
        )
        .unwrap();
        assert!(
            parse_linux_evdev_input_event(
                &invalid_position,
                INPUT_STREAM_REQUEST_ID,
                "input-stream"
            )
            .is_err()
        );
    }

    #[test]
    fn input_relay_frames_are_bounded_and_distinct_by_sequence() {
        let session = RustosInputFrame::session_start(7).unwrap();
        let first = RustosInputFrame::linux_evdev_key(7, 1, 30, 1).unwrap();
        let second = RustosInputFrame::linux_evdev_key(7, 2, 30, 0).unwrap();
        let pointer = RustosInputFrame::linux_evdev_pointer(7, 3, -4, 2, 1, 0, 3).unwrap();
        let position =
            RustosInputFrame::linux_evdev_pointer_position(7, 4, 800, 450, 0, 0, 0).unwrap();
        let end = RustosInputFrame::session_end(7, 5).unwrap();
        assert_ne!(session.as_bytes(), first.as_bytes());
        assert_ne!(first.as_bytes(), second.as_bytes());
        assert_ne!(second.as_bytes(), pointer.as_bytes());
        assert_ne!(pointer.as_bytes(), position.as_bytes());
        assert_ne!(position.as_bytes(), end.as_bytes());
        assert!(RustosInputFrame::linux_evdev_key(7, 1, 0, 1).is_err());
        assert!(RustosInputFrame::linux_evdev_key(7, 1, 30, 3).is_err());
        assert!(RustosInputFrame::linux_evdev_pointer(7, 3, 0, 0, 0, 0, 0x20).is_err());
        assert!(RustosInputFrame::linux_evdev_pointer_position(7, 4, 1600, 450, 0, 0, 0).is_err());
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
    fn input_ring_doorbell_is_edge_triggered_but_cleanup_is_urgent() {
        assert!(super::input_doorbell_needed(0, false));
        assert!(!super::input_doorbell_needed(1, false));
        assert!(super::input_doorbell_needed(1, true));
    }

    #[test]
    fn driver_domain_policy_names_one_explicit_transport_per_class() {
        let policy = DriverDomainPolicy::parse(
            &format!("DRIVER_DOMAIN_POLICY_SCHEMA=2\nDOMAIN_ID=linux-dvm-net0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=disabled\n", "a".repeat(64)),
            "test",
        )
        .unwrap();
        assert_eq!(
            policy.transport_for(DeviceClass::Input),
            DeviceTransport::InputRingMsix
        );
        assert_eq!(
            policy.transport_for(DeviceClass::Network),
            DeviceTransport::Disabled
        );
        assert!(DriverDomainPolicy::parse(
            &format!("DRIVER_DOMAIN_POLICY_SCHEMA=2\nDOMAIN_ID=linux-dvm-net0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=input-ring-msix\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=disabled\n", "a".repeat(64)),
            "test",
        )
        .is_err());
        let display = DriverDomainPolicy::parse(
            &format!("DRIVER_DOMAIN_POLICY_SCHEMA=3\nDOMAIN_ID=linux-dvm-gpu0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=display-dmabuf-kms\nDISPLAY_DRIVER=amdgpu\nDISPLAY_PCI_VENDOR=1002\nDISPLAY_PCI_DEVICE=1900\nDISPLAY_MIN_FRAME_HZ_MILLI=59000\nDISPLAY_MAX_PAGEFLIP_LATENCY_US=25000\nDISPLAY_MAX_ATOMIC_COMMIT_US=2000\nDISPLAY_MAX_SAMPLE_AGE_MS=2000\nDISPLAY_REQUIRED_CONSECUTIVE_SAMPLES=5\n", "a".repeat(64)),
            "display-test",
        )
        .unwrap();
        assert_eq!(
            display.transport_for(DeviceClass::Display),
            DeviceTransport::DisplayDmaBufKms
        );
        assert_eq!(display.physical_display().unwrap().driver(), "amdgpu");
        assert!(DriverDomainPolicy::parse(
            &format!("DRIVER_DOMAIN_POLICY_SCHEMA=3\nDOMAIN_ID=linux-dvm-gpu0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=display-dmabuf-kms\nDISPLAY_DRIVER=amdgpu\nDISPLAY_PCI_VENDOR=10de\nDISPLAY_PCI_DEVICE=1900\nDISPLAY_MIN_FRAME_HZ_MILLI=59000\nDISPLAY_MAX_PAGEFLIP_LATENCY_US=25000\nDISPLAY_MAX_ATOMIC_COMMIT_US=2000\nDISPLAY_MAX_SAMPLE_AGE_MS=2000\nDISPLAY_REQUIRED_CONSECUTIVE_SAMPLES=5\n", "a".repeat(64)),
            "wrong-vendor",
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
    fn vfio_assignment_rejects_live_host_displays() {
        let sysfs = TestSysfs::new(&[(18, &["0000:65:00.0"])]);
        let lease = ValidatedLease {
            domain_id: "linux-dvm-gpu0".to_owned(),
            dvm_guest_cid: 4,
            iommu_group: 18,
            pci_bdfs: vec!["0000:65:00.0".to_owned()],
        };

        sysfs.write_pci_attr("0000:65:00.0", "boot_vga", "1\n");
        assert!(validate_host_display_assignment(&lease, sysfs.path()).is_err());

        sysfs.write_pci_attr("0000:65:00.0", "boot_vga", "0\n");
        sysfs.write_connector_status("0000:65:00.0", "card0-eDP-1", "connected\n");
        assert!(validate_host_display_assignment(&lease, sysfs.path()).is_err());

        sysfs.write_connector_status("0000:65:00.0", "card0-eDP-1", "disconnected\n");
        validate_host_display_assignment(&lease, sysfs.path()).unwrap();
    }

    #[test]
    fn physical_display_assignment_is_bound_to_exact_amdgpu_identity() {
        let sysfs = TestSysfs::new(&[(18, &["0000:65:00.0", "0000:65:00.1"])]);
        let lease = ValidatedLease {
            domain_id: "linux-dvm-gpu0".to_owned(),
            dvm_guest_cid: 4,
            iommu_group: 18,
            pci_bdfs: vec!["0000:65:00.0".to_owned(), "0000:65:00.1".to_owned()],
        };
        let policy = DriverDomainPolicy::parse(
            &format!("DRIVER_DOMAIN_POLICY_SCHEMA=3\nDOMAIN_ID=linux-dvm-gpu0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=display-dmabuf-kms\nDISPLAY_DRIVER=amdgpu\nDISPLAY_PCI_VENDOR=1002\nDISPLAY_PCI_DEVICE=1900\nDISPLAY_MIN_FRAME_HZ_MILLI=59000\nDISPLAY_MAX_PAGEFLIP_LATENCY_US=25000\nDISPLAY_MAX_ATOMIC_COMMIT_US=2000\nDISPLAY_MAX_SAMPLE_AGE_MS=2000\nDISPLAY_REQUIRED_CONSECUTIVE_SAMPLES=5\n", "a".repeat(64)),
            "physical-test",
        )
        .unwrap();
        let display = policy.physical_display().unwrap();
        sysfs.write_pci_attr("0000:65:00.0", "vendor", "0x1002\n");
        sysfs.write_pci_attr("0000:65:00.0", "device", "0x1900\n");
        sysfs.write_pci_attr("0000:65:00.0", "class", "0x030000\n");
        sysfs.bind_driver("0000:65:00.0", "amdgpu");
        sysfs.write_pci_attr("0000:65:00.1", "vendor", "0x1002\n");
        sysfs.write_pci_attr("0000:65:00.1", "device", "0x1640\n");
        sysfs.write_pci_attr("0000:65:00.1", "class", "0x040300\n");
        validate_physical_display_assignment(&lease, sysfs.path(), display).unwrap();

        sysfs.write_pci_attr("0000:65:00.1", "class", "0x0c0330\n");
        assert!(validate_physical_display_assignment(&lease, sysfs.path(), display).is_err());
        sysfs.write_pci_attr("0000:65:00.1", "class", "0x040300\n");
        sysfs.write_pci_attr("0000:65:00.0", "device", "0xffff\n");
        assert!(validate_physical_display_assignment(&lease, sysfs.path(), display).is_err());
        sysfs.write_pci_attr("0000:65:00.0", "device", "0x1900\n");
        sysfs.bind_driver("0000:65:00.0", "vfio-pci");
        assert!(validate_physical_display_assignment(&lease, sysfs.path(), display).is_err());
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
        let mut record = inspect_vfio_lease(&lease, &ops, release_binding()).unwrap();
        acquire_vfio_lease(&record, &mut ops, 150).unwrap();
        reset_vfio_group(&record, &mut ops).unwrap_err();
        record.state = VfioLeaseState::Active;
        reset_vfio_group(&record, &mut ops).unwrap();
        assert_eq!(ops.reset_bdfs, ["0000:02:00.0", "0000:02:00.1"]);
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

    #[test]
    fn durable_vfio_leases_reject_retired_schemas() {
        let v1 = "VFIO_LEASE_SCHEMA=1\nLEASE_STATE=prepared\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=15\nORIGINAL_DRIVERS=0000:02:00.0@rtsx-pci\nORIGINAL_DRIVER_OVERRIDES=0000:02:00.0@none\n";
        let digest = "a".repeat(64);
        let v2 = format!(
            "VFIO_LEASE_SCHEMA=2\nLEASE_STATE=prepared\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=15\nORIGINAL_DRIVERS=0000:02:00.0@rtsx-pci\nORIGINAL_DRIVER_OVERRIDES=0000:02:00.0@none\nRELEASE_MANIFEST_SHA256={digest}\nDVM_ARTIFACT_MANIFEST_SHA256={digest}\nDEVICE_POLICY_SHA256={digest}\nAUTHORIZED_AT_UNIX=100\nAUTHORIZATION_NOT_AFTER_UNIX=200\n"
        );
        assert!(VfioLeaseRecord::parse(v1, "retired-v1").is_err());
        assert!(VfioLeaseRecord::parse(&v2, "retired-v2").is_err());
    }

    #[test]
    fn preflight_snapshot_cannot_be_serialized_as_a_durable_lease() {
        let lease = validated_lease(&["0000:02:00.0"]);
        let ops = FakeVfioOps::with_drivers(&[("0000:02:00.0", Some("first-driver"))]);
        let record = inspect_vfio_lease_preflight(&lease, &ops).unwrap();
        assert!(record.to_env().is_err());
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
        fail_reset: Option<String>,
        reset_bdfs: Vec<String>,
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
                fail_reset: None,
                reset_bdfs: Vec::new(),
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

        fn reset_device(&mut self, bdf: &str) -> Result<(), anyhow::Error> {
            if self.fail_reset.as_deref() == Some(bdf) {
                anyhow::bail!("injected fake reset failure for {bdf}");
            }
            if self.current_driver(bdf)?.as_deref() != Some("vfio-pci") {
                anyhow::bail!("fake reset requires vfio-pci for {bdf}");
            }
            self.reset_bdfs.push(bdf.to_owned());
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
                    fs::create_dir_all(root.join("bus/pci/devices").join(bdf)).unwrap();
                }
            }
            Self { root }
        }

        fn write_pci_attr(&self, bdf: &str, name: &str, value: &str) {
            fs::write(
                self.root.join("bus/pci/devices").join(bdf).join(name),
                value,
            )
            .unwrap();
        }

        fn write_connector_status(&self, bdf: &str, connector: &str, value: &str) {
            let connector = self
                .root
                .join("bus/pci/devices")
                .join(bdf)
                .join("drm/card0")
                .join(connector);
            fs::create_dir_all(&connector).unwrap();
            fs::write(connector.join("status"), value).unwrap();
        }

        fn bind_driver(&self, bdf: &str, driver: &str) {
            let driver_root = self.root.join("bus/pci/drivers").join(driver);
            fs::create_dir_all(&driver_root).unwrap();
            let link = self.root.join("bus/pci/devices").join(bdf).join("driver");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(driver_root, link).unwrap();
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
