use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rustos_driver_domain_host::{
    ControlContract, DEFAULT_CONTROL_PORT, DeviceClass, DeviceTransport, DriverDomainPolicy,
    FileLeaseStore, HostControlListener, IommuTopology, LaunchPlan, SysfsVfioOps, UnixInputSink,
    ValidatedLease, VfioOps, acquire_vfio_lease, inspect_vfio_lease, restore_vfio_lease,
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
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        port: u32,
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
        /// Immutable L0 policy that enables the input transport for this exact domain.
        #[arg(long)]
        device_policy: PathBuf,
        #[arg(long)]
        rustos_input_socket: PathBuf,
        #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
        port: u32,
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
        /// Permit an unsigned lab-only bind. Production release-manifest authorization is not implemented.
        #[arg(long, requires = "activate")]
        allow_unsigned_test_bind: bool,
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
            port,
            timeout_secs,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let contract = ControlContract::from_env_file(&control_contract)?;
            let listener = HostControlListener::bind(lease.dvm_guest_cid, port, contract)?;
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
            device_policy,
            rustos_input_socket,
            port,
            timeout_secs,
            once,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let policy = DriverDomainPolicy::from_env_file(&device_policy)?;
            policy.validate_for_lease(&lease)?;
            if policy.transport_for(DeviceClass::Input) != DeviceTransport::Rdi2Com2 {
                anyhow::bail!(
                    "driver-domain policy does not enable rdi2-com2 input transport for {}",
                    lease.domain_id
                );
            }
            let contract = ControlContract::from_env_file(&control_contract)?;
            let timeout = Duration::from_secs(timeout_secs.max(1));
            let listener = HostControlListener::bind(lease.dvm_guest_cid, port, contract)?;
            loop {
                let mut sink = match UnixInputSink::connect(&rustos_input_socket, timeout) {
                    Ok(sink) => sink,
                    Err(error) if once => return Err(error),
                    Err(error) => {
                        eprintln!(
                            "rustos-hostd: RustOS input endpoint unavailable domain={} reason={error:#}; retrying",
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
            allow_unsigned_test_bind,
        } => {
            let lease = validate_plan(&plan, &sysfs_root)?;
            let mut ops = SysfsVfioOps::new(&sysfs_root);
            let mut record = inspect_vfio_lease(&lease, &ops)?;
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
            if !allow_unsigned_test_bind {
                anyhow::bail!(
                    "refusing VFIO acquire without --allow-unsigned-test-bind; production release-manifest authorization is not implemented"
                );
            }
            let store = FileLeaseStore::new(state_root);
            store.create_prepared(&record)?;
            if let Err(error) = acquire_vfio_lease(&record, &mut ops) {
                anyhow::bail!(
                    "VFIO acquire failed; prepared lease retained for explicit recovery with release --activate: {error:#}"
                );
            }
            if let Err(error) = store.mark_active(&mut record) {
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
