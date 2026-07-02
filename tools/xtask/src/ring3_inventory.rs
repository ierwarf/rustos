use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;

const MARKER_START: &str = "RING3-MIGRATION-REFERENCE START";
const MARKER_END: &str = "RING3-MIGRATION-REFERENCE END";
const COMMENTED_OUT_START: &str = "RING3-MIGRATION-COMMENTED-OUT START";
const COMMENTED_OUT_END: &str = "RING3-MIGRATION-COMMENTED-OUT END";

struct InventoryEntry {
    path: PathBuf,
    marked_loc: usize,
    reference_loc: usize,
    commented_out_loc: usize,
    owner: &'static str,
    lane: &'static str,
    action: &'static str,
}

pub(crate) fn print_inventory(config: &Config) -> Result<()> {
    let mut entries = Vec::new();
    collect_entries(
        &config.root_dir.join("kernel"),
        &config.root_dir,
        &mut entries,
    )?;
    collect_entries(
        &config.root_dir.join("services"),
        &config.root_dir,
        &mut entries,
    )?;
    entries.sort_by(|left, right| {
        right
            .marked_loc
            .cmp(&left.marked_loc)
            .then_with(|| left.path.cmp(&right.path))
    });

    let total = entries.iter().map(|entry| entry.marked_loc).sum::<usize>();
    let reference = entries
        .iter()
        .map(|entry| entry.reference_loc)
        .sum::<usize>();
    let commented_out = entries
        .iter()
        .map(|entry| entry.commented_out_loc)
        .sum::<usize>();
    let excluded = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.lane,
                "compat-ring0-exception" | "ring3-owner-reference" | "already-migrated-reference"
            )
        })
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let service_driver_host = entries
        .iter()
        .filter(|entry| entry.lane == "service-driver-host")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let active = total.saturating_sub(excluded);

    println!("ring3 migration inventory");
    println!("total_marked_loc={total}");
    println!("reference_loc={reference}");
    println!("commented_out_loc={commented_out}");
    println!("excluded_exception_loc={excluded}");
    println!("service_driver_host_loc={service_driver_host}");
    println!("active_batch_marked_loc={active}");
    println!();
    println!("loc\treference\tcommented_out\tlane\towner\taction\tpath");
    for entry in entries {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            entry.marked_loc,
            entry.reference_loc,
            entry.commented_out_loc,
            entry.lane,
            entry.owner,
            entry.action,
            entry.path.display()
        );
    }
    Ok(())
}

fn collect_entries(dir: &Path, root_dir: &Path, entries: &mut Vec<InventoryEntry>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for item in fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        let file_name = item.file_name();
        let file_name = file_name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                file_name.as_ref(),
                "target" | "build" | "vendor" | "logs" | ".git"
            ) {
                continue;
            }
            collect_entries(&path, root_dir, entries)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let content = fs::read_to_string(&path)?;
        if !content.contains(MARKER_START) && !content.contains(COMMENTED_OUT_START) {
            continue;
        }
        let relative = path.strip_prefix(root_dir).unwrap_or(path.as_path());
        let reference_loc = marked_source_loc(&content, MARKER_START, MARKER_END);
        let commented_out_loc = marked_source_loc(&content, COMMENTED_OUT_START, COMMENTED_OUT_END);
        entries.push(InventoryEntry {
            path: relative.to_path_buf(),
            marked_loc: reference_loc + commented_out_loc,
            reference_loc,
            commented_out_loc,
            owner: owner_for_path(relative),
            lane: lane_for_path(relative),
            action: action_for_path(relative),
        });
    }
    Ok(())
}

fn marked_source_loc(content: &str, marker_start: &str, marker_end: &str) -> usize {
    let mut in_marker = false;
    let mut total = 0usize;
    for line in content.lines() {
        if line.contains(marker_start) {
            in_marker = true;
            continue;
        }
        if line.contains(marker_end) {
            in_marker = false;
            continue;
        }
        if !in_marker {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        total += 1;
    }
    total
}

fn owner_for_path(path: &Path) -> &'static str {
    let path = path.to_string_lossy();
    if path.contains("/usb/xhci.rs") {
        "usbdrv-driverd"
    } else if path.contains("/usb/core.rs") {
        "driverd-devmgrd-inputd"
    } else if path.contains("/storage/ahci.rs") {
        "storaged-driverd"
    } else if path.contains("/storage/nvme.rs") {
        "storaged-driverd"
    } else if path.contains("/driver/linux/")
        || path.contains("/driver/devres.rs")
        || path.contains("/driver/export.rs")
        || path.contains("/driver/kernel_api.rs")
        || path.contains("/driver/module_registry.rs")
    {
        "linux-ko-compat"
    } else if path.contains("services/vfsd/") {
        "vfsd"
    } else if path.contains("/sysops/stat.rs") {
        "vfsd"
    } else if path.ends_with("/ps/src/user/linux.rs") {
        "syscalld-loaderd-procd"
    } else if path.contains("/service_ops/vfs_meta.rs") {
        "vfsd"
    } else if path.contains("/service_ops/vfs_socket.rs")
        || path.contains("/service_ops/poll_epoll.rs")
        || path.contains("/epoll.rs")
    {
        "vfsd-netd"
    } else if path.contains("/service_ops/futex_thread.rs")
        || path.contains("/service_ops/process_time.rs")
    {
        "procd-syscalld"
    } else if path.contains("/service_ops/ipc_helpers.rs") {
        "rootd-loaderd-procd-vfsd"
    } else if path.contains("/block_broker_ops.rs") {
        "vfsd-storaged"
    } else if path.contains("/storage_broker_ops.rs") {
        "storaged"
    } else if path.contains("/device_broker_ops.rs") {
        "devmgrd"
    } else if path.contains("/driver_broker_ops.rs") || path.ends_with("/driver/mod.rs") {
        "driverd"
    } else if path.contains("/input_broker_ops.rs")
        || path.contains("/input_core.rs")
        || path.ends_with("/driver/input.rs")
    {
        "inputd"
    } else if path.contains("/broker_ops.rs") || path.contains("/lifecycle_broker_ops.rs") {
        "rootd-capability"
    } else if path.contains("/offload_ops.rs") {
        "syscalld"
    } else if path.ends_with("/compat/src/user/mod.rs") {
        "syscalld-loaderd-procd-vfsd-netd"
    } else if path.ends_with("/compat/src/user/syscall/mod.rs")
        || path.contains("/syscall/linux/support.rs")
    {
        "syscalld-procd"
    } else if path.contains("/syscall/windows/") {
        "syscalld-loaderd"
    } else if path.ends_with("/compat/src/user/sysops/mod.rs") {
        "vfsd-devmgrd-sessiond"
    } else if path.contains("/memfd.rs") {
        "pagerd-procd"
    } else if path.contains("/sysops/console.rs") {
        "sessiond-runtimed"
    } else if path.contains("/console_host.rs") {
        "loaderd-sessiond"
    } else if path.contains("/usb/") || path.contains("/input/") || path.contains("/serio.rs") {
        "inputd"
    } else if path.contains("/io/tty.rs")
        || path.contains("/io/session.rs")
        || path.contains("/io/console.rs")
    {
        "sessiond"
    } else if path.contains("/io/gui") || path.contains("/virtio_gpu.rs") {
        "uiserver"
    } else if path.contains("/storage/") {
        "storaged"
    } else if path.contains("/socket.rs") || path.contains("/net_broker_ops.rs") {
        "netd"
    } else if path.contains("/process/") || path.contains("/proc_broker_ops.rs") {
        "loaderd-procd"
    } else if path.contains("/memory_ops.rs")
        || path.contains("/mm_broker_ops.rs")
        || path.contains("/syscalld_ops.rs")
        || path.contains("/win32/memory.rs")
    {
        "syscalld-pagerd"
    } else if path.contains("/ipc_ops.rs") {
        "rootd-capability"
    } else if path.contains("/sysops/file.rs") {
        "vfsd-pagerd"
    } else if path.contains("/sysops/device.rs") || path.contains("/io/device/") {
        "devmgrd"
    } else if path.contains("/ahci.rs") {
        "storaged"
    } else {
        "unclassified"
    }
}

fn lane_for_path(path: &Path) -> &'static str {
    let path = path.to_string_lossy();
    if path.contains("/driver/linux/")
        || path.contains("/driver/devres.rs")
        || path.contains("/driver/export.rs")
        || path.contains("/driver/kernel_api.rs")
        || path.contains("/driver/module_registry.rs")
    {
        "compat-ring0-exception"
    } else if path.contains("services/vfsd/") {
        "ring3-owner-reference"
    } else if path.contains("/sysops/stat.rs") || path.ends_with("/ps/src/user/linux.rs") {
        "already-migrated-reference"
    } else if path.contains("/usb/xhci.rs")
        || path.contains("/storage/ahci.rs")
        || path.contains("/storage/nvme.rs")
    {
        "service-driver-host"
    } else if path.contains("/process/")
        || path.contains("/proc_broker_ops.rs")
        || path.contains("/ipc_ops.rs")
        || path.contains("/virtio_gpu.rs")
    {
        "abi-first-large"
    } else if path.contains("/usb/")
        || path.contains("/serio.rs")
        || path.contains("/input/")
        || path.contains("/io/gui")
        || path.contains("/io/tty.rs")
        || path.contains("/io/session.rs")
        || path.contains("/io/console.rs")
        || path.contains("/storage/")
        || path.contains("/socket.rs")
        || path.contains("/net_broker_ops.rs")
    {
        "service-shrink"
    } else {
        "policy-bridge"
    }
}

fn action_for_path(path: &Path) -> &'static str {
    let path = path.to_string_lossy();
    if path.contains("/usb/xhci.rs")
        || path.contains("/storage/ahci.rs")
        || path.contains("/storage/nvme.rs")
    {
        "migrate non-.ko service-driver to ring3 host, or document explicit privileged substrate exception"
    } else if path.contains("/driver/linux/")
        || path.contains("/driver/devres.rs")
        || path.contains("/driver/export.rs")
        || path.contains("/driver/kernel_api.rs")
        || path.contains("/driver/module_registry.rs")
    {
        "explicit Linux .ko ring0 compatibility substrate exception; do not migrate .ko execution to ring3"
    } else if path.contains("services/vfsd/") {
        "already ring3 service owner; marker restored for historical audit only"
    } else if path.contains("/sysops/stat.rs") || path.ends_with("/ps/src/user/linux.rs") {
        "already migrated/shared ABI reference; marker restored for historical audit only"
    } else if path.contains("/usb/core.rs") {
        "move USB interface admission/provider policy into driverd/devmgrd/inputd, keep compat callback substrate"
    } else if path.contains("/service_ops/vfs_meta.rs")
        || path.contains("/sysops/file.rs")
        || path.contains("/sysops/stat.rs")
    {
        "move VFS metadata/path policy into vfsd, keep user-copy/fd substrate"
    } else if path.contains("/service_ops/vfs_socket.rs")
        || path.contains("/service_ops/poll_epoll.rs")
        || path.contains("/epoll.rs")
    {
        "move VFS/socket readiness policy into vfsd/netd, keep fd/user-copy substrate"
    } else if path.contains("/service_ops/futex_thread.rs")
        || path.contains("/service_ops/process_time.rs")
    {
        "move thread/futex/time policy into procd/syscalld, keep scheduler substrate"
    } else if path.contains("/service_ops/ipc_helpers.rs") {
        "move direct service routing policy into owner services, keep IPC copy substrate"
    } else if path.contains("/block_broker_ops.rs") || path.contains("/storage_broker_ops.rs") {
        "move boot block/storage descriptor policy into storaged/vfsd, keep gated physical substrate"
    } else if path.contains("/device_broker_ops.rs")
        || path.contains("/sysops/device.rs")
        || path.contains("/io/device/")
    {
        "move device namespace/right policy into devmgrd, keep native device substrate"
    } else if path.contains("/driver_broker_ops.rs") || path.ends_with("/driver/mod.rs") {
        "move driver/provider policy into driverd, keep .ko/DMA/MMIO/IRQ substrate"
    } else if path.contains("/input_broker_ops.rs")
        || path.contains("/input_core.rs")
        || path.ends_with("/driver/input.rs")
    {
        "move input ingress/coalescing policy into inputd, keep hardware callback substrate"
    } else if path.contains("/broker_ops.rs") || path.contains("/lifecycle_broker_ops.rs") {
        "move broker/lifecycle capability policy into rootd/procd, keep kernel event substrate"
    } else if path.contains("/offload_ops.rs") || path.contains("/syscalld_ops.rs") {
        "move Linux syscall offload policy into syscalld, keep user-copy substrate"
    } else if path.ends_with("/compat/src/user/mod.rs") {
        "move compat module policy into owner services, keep ring0 ABI routing substrate"
    } else if path.ends_with("/compat/src/user/syscall/mod.rs")
        || path.contains("/syscall/linux/support.rs")
    {
        "move syscall support policy into syscalld/procd, keep syscall entry substrate"
    } else if path.contains("/syscall/windows/") {
        "move Win32 syscall policy into syscalld/loaderd, keep syscall decode substrate"
    } else if path.ends_with("/compat/src/user/sysops/mod.rs") {
        "move sysop namespace policy into vfsd/devmgrd/sessiond, keep module routing substrate"
    } else if path.contains("/memfd.rs") {
        "move memfd lifecycle policy into pagerd/procd, keep frame backing substrate"
    } else if path.contains("/sysops/console.rs") || path.contains("/console_host.rs") {
        "move console host/sysop policy into loaderd/sessiond/runtimed"
    } else if path.contains("/usb/runtime.rs") || path.contains("/usb/synthetic.rs") {
        "move HID parse/state policy into inputd, keep USB callback source"
    } else if path.contains("/serio.rs") || path.contains("/input/i8042.rs") {
        "move legacy input routing into inputd service-driver path"
    } else if path.contains("/io/gui") || path.contains("/virtio_gpu.rs") {
        "move provider/display policy into uiserver or service-driver path"
    } else if path.contains("/storage/") {
        "move post-bootstrap storage policy into storaged, keep raw block broker"
    } else if path.contains("/process/") || path.contains("/proc_broker_ops.rs") {
        "move cold image/process policy into loaderd/procd before deleting ring0 parser branches"
    } else if path.contains("/ipc_ops.rs") {
        "move namespace/capability policy behind rootd capability protocol"
    } else {
        "replace marked policy with service-owned protocol and then remove marker"
    }
}
