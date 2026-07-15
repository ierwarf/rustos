use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::{Parser, Subcommand};
use rustos_driver_domain_host::{
    ControlContract, ControlSecret, DeviceClass, DeviceTransport, DriverDomainFleetPolicy,
    DriverDomainPolicy, FileLeaseStore, HostControlListener, InputRingSink, IommuTopology,
    LaunchPlan, ReleaseAuthorization, SysfsVfioOps, ValidatedLease, VfioOps, VfioReleaseBinding,
    acquire_vfio_lease, inspect_vfio_lease, inspect_vfio_lease_preflight, restore_vfio_lease,
};

#[derive(Parser)]
#[command(
    name = "rustos-hostd",
    about = "RustOS L0 Linux driver-domain control broker"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List host IOMMU groups without changing a device binding.
    Discover {
        #[arg(long, default_value = "/sys")]
        sysfs_root: PathBuf,
    },
    /// Validate exact IOMMU-group ownership without unbinding or assigning a device.
    Preflight {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "/sys")]
        sysfs_root: PathBuf,
    },
    /// Preflight the lease, then accept one bounded host-to-DVM control probe.
    Probe {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "/sys")]
        sysfs_root: PathBuf,
        #[arg(long)]
        control_contract: PathBuf,
        /// Owner-private 256-bit control secret, provisioned to this DVM at launch.
        #[arg(long)]
        control_secret: PathBuf,
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    /// Relay the authenticated DVM's Linux evdev stream into RustOS's
    /// dedicated virtual input transport. This is not QMP and does not give
    /// the DVM access to any RustOS management or filesystem interface.
    RelayInput {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "/sys")]
        sysfs_root: PathBuf,
        #[arg(long)]
        control_contract: PathBuf,
        /// Owner-private 256-bit control secret, provisioned to this DVM at launch.
        #[arg(long)]
        control_secret: PathBuf,
        /// Immutable L0 policy that enables the input transport for this exact domain.
        #[arg(long)]
        device_policy: PathBuf,
        #[arg(long)]
        /// Launch-owned fixed 128 KiB input-ring backing, mapped only by L0 and RustOS.
        #[arg(long)]
        input_ring: PathBuf,
        /// Launch-owned ivshmem-doorbell socket after RustOS has claimed peer 0.
        #[arg(long)]
        input_doorbell: PathBuf,
        /// Maximum time to establish the DVM and RustOS relay endpoints.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Exit after the first relay failure. Intended only for focused diagnostics.
        #[arg(long)]
        once: bool,
    },
    /// Show or explicitly acquire a complete validated IOMMU group for vfio-pci.
    Acquire {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "/sys")]
        sysfs_root: PathBuf,
        #[arg(long, default_value = "/run/rustos-hostd/leases")]
        state_root: PathBuf,
        /// Perform driver override, unbind, and vfio-pci bind after durable state is written.
        #[arg(long)]
        activate: bool,
        /// Signed release authorization binding this exact IOMMU group to DVM artifacts.
        #[arg(long)]
        release_manifest: Option<PathBuf>,
        /// Detached OpenPGP signature for `--release-manifest`.
        #[arg(long)]
        release_signature: Option<PathBuf>,
        /// Pinned release keyring accepted by gpgv.
        #[arg(long)]
        release_keyring: Option<PathBuf>,
        /// Hash-bound Linux DVM artifact manifest named by the release authorization.
        #[arg(long)]
        dvm_artifact_manifest: Option<PathBuf>,
        /// Hash-bound immutable driver-domain policy named by the release authorization.
        #[arg(long)]
        device_policy: Option<PathBuf>,
        /// Hash-bound fleet policy that proves no other domain reuses this CID, IOMMU group, or PCI function.
        #[arg(long)]
        fleet_policy: Option<PathBuf>,
    },
    /// Show or explicitly restore the original host drivers from a durable VFIO lease.
    Release {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value = "/sys")]
        sysfs_root: PathBuf,
        #[arg(long, default_value = "/run/rustos-hostd/leases")]
        state_root: PathBuf,
        /// Restore original drivers and remove the durable lease only after success.
        #[arg(long)]
        activate: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Discover { sysfs_root } => {
            let topology = IommuTopology::discover(&sysfs_root)?;
            for (group, bdfs) in topology.groups() {
                println!(
                    "iommu_group={group} pci_bdfs={}",
                    bdfs.iter().cloned().collect::<Vec<_>>().join(",")
                );
            }
        }
        Command::Preflight { plan, sysfs_root } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            println!(
                "rustos-hostd: lease preflight passed domain={} cid={} iommu_group={} pci_bdfs={}",
                lease.domain_id,
                lease.dvm_guest_cid,
                lease.iommu_group,
                lease.pci_bdfs.join(","),
            );
        }
        Command::Probe {
            plan,
            sysfs_root,
            control_contract,
            control_secret,
            timeout_secs,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let contract = ControlContract::from_env_file(&control_contract)?;
            let control_secret = ControlSecret::from_hex_file(&control_secret)?;
            let listener =
                HostControlListener::bind(lease.dvm_guest_cid, contract, control_secret)?;
            let result = listener.probe_once(Duration::from_secs(timeout_secs))?;
            println!(
                "rustos-hostd: DVM control verified domain={} cid={} iommu_group={} inventory_count={} virtio_net={} virtio_gpu={}",
                lease.domain_id,
                result.peer_cid,
                lease.iommu_group,
                result.inventory_count,
                result.driver_inventory.virtio_net_bound,
                result.driver_inventory.virtio_gpu_bound,
            );
        }
        Command::RelayInput {
            plan,
            sysfs_root,
            control_contract,
            control_secret,
            device_policy,
            input_ring,
            input_doorbell,
            timeout_secs,
            once,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let policy = DriverDomainPolicy::from_env_file(&device_policy)?;
            policy.validate_for_lease(&lease)?;
            if policy.transport_for(DeviceClass::Input) != DeviceTransport::InputRingMsix {
                anyhow::bail!(
                    "driver-domain policy does not enable input-ring-msix transport for {}",
                    lease.domain_id
                );
            }
            let contract = ControlContract::from_env_file(&control_contract)?;
            let control_secret = ControlSecret::from_hex_file(&control_secret)?;
            let timeout = Duration::from_secs(timeout_secs.max(1));
            let listener =
                HostControlListener::bind(lease.dvm_guest_cid, contract, control_secret)?;
            loop {
                let mut sink = match InputRingSink::connect(&input_doorbell, &input_ring, timeout) {
                    Ok(sink) => sink,
                    Err(error) if once => return Err(error),
                    Err(error) => {
                        eprintln!(
                            "rustos-hostd: fixed RustOS input ring unavailable domain={} reason={error:#}; retrying",
                            lease.domain_id
                        );
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                match listener.relay_input_once(timeout, &mut sink) {
                    Ok(result) => println!(
                        "rustos-hostd: DVM input relay ended domain={} cid={} iommu_group={} inventory_count={} virtio_net={} virtio_gpu={} forwarded_events={}",
                        lease.domain_id,
                        result.probe.peer_cid,
                        lease.iommu_group,
                        result.probe.inventory_count,
                        result.probe.driver_inventory.virtio_net_bound,
                        result.probe.driver_inventory.virtio_gpu_bound,
                        result.forwarded_events,
                    ),
                    Err(error) if once => return Err(error),
                    Err(error) => eprintln!(
                        "rustos-hostd: input relay reset domain={} cid={} reason={error:#}; retrying",
                        lease.domain_id, lease.dvm_guest_cid
                    ),
                }
                if once {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        Command::Acquire {
            plan,
            sysfs_root,
            state_root,
            activate,
            release_manifest,
            release_signature,
            release_keyring,
            dvm_artifact_manifest,
            device_policy,
            fleet_policy,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let mut ops = SysfsVfioOps::new(&sysfs_root);
            let release_binding = if activate {
                Some(verify_release_authorization(
                    &lease,
                    required_path(release_manifest, "--release-manifest")?,
                    required_path(release_signature, "--release-signature")?,
                    required_path(release_keyring, "--release-keyring")?,
                    required_path(dvm_artifact_manifest, "--dvm-artifact-manifest")?,
                    required_path(device_policy, "--device-policy")?,
                    required_path(fleet_policy, "--fleet-policy")?,
                )?)
            } else {
                None
            };
            let mut record = match release_binding {
                Some(binding) => inspect_vfio_lease(&lease, &ops, binding)?,
                None => inspect_vfio_lease_preflight(&lease, &ops)?,
            };
            println!(
                "rustos-hostd: VFIO {} domain={} cid={} iommu_group={} pci_bdfs={} original_drivers={:?} vfio_pci_loaded={}",
                if activate { "acquire" } else { "dry-run" },
                lease.domain_id,
                lease.dvm_guest_cid,
                lease.iommu_group,
                lease.pci_bdfs.join(","),
                record.original_drivers,
                ops.vfio_driver_present()?,
            );
            if !activate {
                return Ok(());
            }
            let store = FileLeaseStore::new(state_root);
            store.create_prepared(&record, current_unix_time()?)?;
            if let Err(error) = acquire_vfio_lease(&record, &mut ops, current_unix_time()?) {
                anyhow::bail!(
                    "VFIO acquire failed; prepared lease retained for explicit recovery with release --activate: {error:#}"
                );
            }
            if let Err(error) = store.mark_active(&mut record, current_unix_time()?) {
                let restore_error = restore_vfio_lease(&record, &mut ops).err();
                if let Some(restore_error) = restore_error {
                    anyhow::bail!(
                        "VFIO lease activation record failed: {error:#}; rollback failed: {restore_error:#}"
                    );
                }
                anyhow::bail!(
                    "VFIO device binding was rolled back because lease activation record failed: {error:#}"
                );
            }
            println!(
                "rustos-hostd: VFIO lease active domain={} iommu_group={}",
                record.domain_id, record.iommu_group
            );
        }
        Command::Release {
            plan,
            sysfs_root,
            state_root,
            activate,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let store = FileLeaseStore::new(state_root);
            let record = store.load(&lease.domain_id)?;
            validate_record_matches_lease(&record, &lease)?;
            println!(
                "rustos-hostd: VFIO {} release domain={} iommu_group={} pci_bdfs={}",
                if activate { "activate" } else { "dry-run" },
                record.domain_id,
                record.iommu_group,
                lease.pci_bdfs.join(","),
            );
            if !activate {
                return Ok(());
            }
            let mut ops = SysfsVfioOps::new(sysfs_root);
            restore_vfio_lease(&record, &mut ops)?;
            store.remove(&record.domain_id)?;
            println!(
                "rustos-hostd: VFIO lease released domain={} iommu_group={}",
                record.domain_id, record.iommu_group
            );
        }
    }
    Ok(())
}

fn validate_plan(
    plan_path: &std::path::Path,
    sysfs_root: &std::path::Path,
) -> Result<rustos_driver_domain_host::ValidatedLease> {
    let plan = LaunchPlan::from_env_file(plan_path)?;
    let topology = IommuTopology::discover(sysfs_root)?;
    plan.validate_topology(&topology)
}

fn validate_record_matches_lease(
    record: &rustos_driver_domain_host::VfioLeaseRecord,
    lease: &ValidatedLease,
) -> Result<()> {
    if record.domain_id != lease.domain_id
        || record.dvm_guest_cid != lease.dvm_guest_cid
        || record.iommu_group != lease.iommu_group
        || record.original_drivers.keys().cloned().collect::<Vec<_>>() != lease.pci_bdfs
    {
        anyhow::bail!("durable VFIO lease does not match the current validated launch plan");
    }
    Ok(())
}

fn required_path(path: Option<PathBuf>, flag: &str) -> Result<PathBuf> {
    path.ok_or_else(|| anyhow::anyhow!("{flag} is required with --activate"))
}

fn verify_release_authorization(
    lease: &ValidatedLease,
    release_manifest: PathBuf,
    release_signature: PathBuf,
    release_keyring: PathBuf,
    dvm_artifact_manifest: PathBuf,
    device_policy: PathBuf,
    fleet_policy: PathBuf,
) -> Result<VfioReleaseBinding> {
    let status = ProcessCommand::new("gpgv")
        .arg("--keyring")
        .arg(&release_keyring)
        .arg(&release_signature)
        .arg(&release_manifest)
        .status()
        .map_err(|error| anyhow::anyhow!("start gpgv for release authorization: {error}"))?;
    if !status.success() {
        anyhow::bail!("release authorization signature verification failed");
    }
    let authorization = ReleaseAuthorization::from_env_file(&release_manifest)?;
    let now_unix = current_unix_time()?;
    authorization.validate_for_lease(lease, now_unix)?;
    let release_manifest_sha256 = sha256_file(&release_manifest)?;
    verify_sha256_file(
        &dvm_artifact_manifest,
        authorization.dvm_artifact_manifest_sha256(),
    )?;
    verify_sha256_file(&device_policy, authorization.device_policy_sha256())?;
    verify_sha256_file(&fleet_policy, authorization.fleet_policy_sha256())?;
    DriverDomainFleetPolicy::from_env_file(&fleet_policy)?.validate_for_lease(lease)?;
    authorization.into_vfio_release_binding(&release_manifest_sha256, now_unix)
}

fn current_unix_time() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow::anyhow!("system clock before Unix epoch: {error}"))
        .map(|duration| duration.as_secs())
}

fn verify_sha256_file(path: &std::path::Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected {
        anyhow::bail!(
            "release authorization hash mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn sha256_file(path: &std::path::Path) -> Result<String> {
    let output = ProcessCommand::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|error| anyhow::anyhow!("hash {}: {error}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!("sha256sum failed for {}", path.display());
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow::anyhow!("sha256sum produced no digest for {}", path.display()))?;
    if actual.len() != 64 || !actual.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!(
            "sha256sum produced an invalid digest for {}",
            path.display()
        );
    }
    Ok(actual)
}
