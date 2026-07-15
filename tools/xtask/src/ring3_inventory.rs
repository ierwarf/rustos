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
                "compat-ring0-exception"
                    | "bootstrap-ring0-exception"
                    | "hot-path-ring0-exception"
                    | "hardware-probe-ring0-exception"
                    | "syscall-decode-ring0-exception"
                    | "capability-broker-ring0-exception"
                    | "input-ingress-ring0-exception"
                    | "scheduler-thread-substrate-exception"
                    | "memfd-kernel-substrate-exception"
                    | "signal-frame-substrate-exception"
                    | "display-present-substrate-exception"
                    | "dvm-transport-substrate-exception"
                    | "bootstrap-device-route-exception"
                    | "vfs-bootstrap-hotpath-exception"
                    | "vfs-socket-fd-substrate-exception"
                    | "abi-substrate-reference"
                    | "ring3-owner-reference"
                    | "already-migrated-reference"
                    | "compat-slowpath-ring3"
                    | "pager-slowpath-ring3"
                    | "process-slowpath-ring3"
            )
        })
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let service_driver_host = entries
        .iter()
        .filter(|entry| entry.lane == "service-driver-host")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let compat_slowpath_ring3 = entries
        .iter()
        .filter(|entry| entry.lane == "compat-slowpath-ring3")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let pager_slowpath_ring3 = entries
        .iter()
        .filter(|entry| entry.lane == "pager-slowpath-ring3")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let process_slowpath_ring3 = entries
        .iter()
        .filter(|entry| entry.lane == "process-slowpath-ring3")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let cleanup_debt = entries
        .iter()
        .filter(|entry| entry.lane == "legacy-native-removal")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let active = total.saturating_sub(excluded);
    let migration_candidate = active.saturating_sub(cleanup_debt);

    println!("ring3 migration inventory");
    println!("total_marked_loc={total}");
    println!("reference_loc={reference}");
    println!("commented_out_loc={commented_out}");
    println!("excluded_exception_loc={excluded}");
    println!("service_driver_host_loc={service_driver_host}");
    println!("compat_slowpath_ring3_loc={compat_slowpath_ring3}");
    println!("pager_slowpath_ring3_loc={pager_slowpath_ring3}");
    println!("process_slowpath_ring3_loc={process_slowpath_ring3}");
    println!("cleanup_debt_loc={cleanup_debt}");
    println!("migration_candidate_loc={migration_candidate}");
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
    if path.ends_with("/user/compat_slowpath_migration.rs") {
        "syscalld-procd-vfsd-netd-devmgrd"
    } else if path.ends_with("/memory/pager_slowpath_migration.rs") {
        "pagerd-syscalld-loaderd"
    } else if path.ends_with("/user/process_slowpath_migration.rs") {
        "procd-syscalld-loaderd"
    } else if path.contains("/storage/ahci.rs") || path.contains("/storage/nvme.rs") {
        "storaged"
    } else if path.contains("services/vfsd/") || path.contains("/sysops/stat.rs") {
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
        "service-driver"
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
    } else if path.ends_with("/io/dvm_network.rs") {
        "netd"
    } else if path.ends_with("/io/dvm_display.rs") {
        "uiserver"
    } else if path.contains("/input/") {
        "inputd"
    } else if path.contains("/io/tty.rs")
        || path.contains("/io/session.rs")
        || path.contains("/io/console.rs")
    {
        "sessiond"
    } else if path.contains("/io/gui") {
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
    if path.ends_with("/user/compat_slowpath_migration.rs") {
        "compat-slowpath-ring3"
    } else if path.ends_with("/memory/pager_slowpath_migration.rs") {
        "pager-slowpath-ring3"
    } else if path.ends_with("/user/process_slowpath_migration.rs") {
        "process-slowpath-ring3"
    } else if path.ends_with("/compat/src/user/process/linux.rs")
        || path.ends_with("/compat/src/user/console_host.rs")
        || path.contains("/memory_ops.rs")
        || path.contains("/block_broker_ops.rs")
        || path.contains("/storage/ahci.rs")
        || path.contains("/storage/nvme.rs")
        || path.contains("/storage/boot_volume.rs")
        || path.ends_with("/storage/block.rs")
        || path.contains("/storage/block/boot.rs")
        || path.contains("/storage/block/io.rs")
        || path.contains("/io/console.rs")
        || path.contains("/io/tty.rs")
    {
        "bootstrap-ring0-exception"
    } else if path.contains("/service_ops/ipc_helpers.rs") {
        "bootstrap-device-route-exception"
    } else if path.contains("/sysops/device.rs") || path.contains("/io/device/display.rs") {
        "hot-path-ring0-exception"
    } else if path.ends_with("/driver/mod.rs") {
        "hardware-probe-ring0-exception"
    } else if path.contains("/syscall/windows/") {
        "syscall-decode-ring0-exception"
    } else if path.contains("/lifecycle_broker_ops.rs")
        || path.contains("/proc_broker_ops.rs")
        || path.contains("/device_broker_ops.rs")
        || path.contains("/driver_broker_ops.rs")
        || path.contains("/ipc_ops.rs")
        || path.ends_with("/broker_ops.rs")
    {
        "capability-broker-ring0-exception"
    } else if path.contains("/service_ops/vfs_socket.rs") {
        "vfs-socket-fd-substrate-exception"
    } else if path.contains("/service_ops/vfs_meta.rs") {
        "vfs-bootstrap-hotpath-exception"
    } else if path.contains("/service_ops/futex_thread.rs")
        || path.contains("/service_ops/process_time.rs")
    {
        "scheduler-thread-substrate-exception"
    } else if path.contains("/input_core.rs")
        || path.contains("/input/event_queue.rs")
        || path.ends_with("/input/dvm_frames.rs")
        || path.ends_with("/input/mod.rs")
        || path.ends_with("/driver/input.rs")
        || path.contains("/input_broker_ops.rs")
        || path.contains("/input/dispatcher.rs")
        || path.contains("/input/keyboard.rs")
        || path.contains("/usb/runtime.rs")
    {
        "input-ingress-ring0-exception"
    } else if path.contains("/memfd.rs") {
        "memfd-kernel-substrate-exception"
    } else if path.ends_with("/io/dvm_network.rs") || path.ends_with("/io/dvm_display.rs") {
        "dvm-transport-substrate-exception"
    } else if path.contains("/io/gui") {
        "display-present-substrate-exception"
    } else if path.contains("/usb/xhci.rs")
        || path.contains("/usb/core.rs")
        || path.contains("/usb/manager.rs")
        || path.ends_with("/usb/mod.rs")
    {
        "usb-runtime-substrate-exception"
    } else if path.ends_with("/compat/src/user/mod.rs")
        || path.ends_with("/compat/src/user/sysops/mod.rs")
    {
        "abi-substrate-reference"
    } else if path.contains("/syscall/linux/support.rs") {
        "signal-frame-substrate-exception"
    } else if path.contains("services/vfsd/") {
        "ring3-owner-reference"
    } else if path.contains("/sysops/stat.rs")
        || path.ends_with("/ps/src/user/linux.rs")
        || path.ends_with("/compat/src/user/process/mod.rs")
        || path.contains("/socket.rs")
        || path.contains("/epoll.rs")
        || path.contains("/io/session.rs")
    {
        "already-migrated-reference"
    } else if path.contains("/process/")
        || path.contains("/proc_broker_ops.rs")
        || path.contains("/ipc_ops.rs")
    {
        "abi-first-large"
    } else if path.contains("/usb/")
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
    if path.ends_with("/user/compat_slowpath_migration.rs") {
        "move residual Linux and Win32 slow-path policy into owner services"
    } else if path.ends_with("/memory/pager_slowpath_migration.rs") {
        "move VMA, backing, and accounting policy into pagerd and syscalld"
    } else if path.ends_with("/user/process_slowpath_migration.rs") {
        "move process hierarchy, wait, credentials, and signal policy into procd"
    } else if path.ends_with("/io/dvm_network.rs") {
        "keep bounded DVM Ethernet transport; netd owns network policy"
    } else if path.ends_with("/io/dvm_display.rs") {
        "keep bounded DVM framebuffer transport; uiserver owns presentation policy"
    } else if path.contains("/input/") || path.contains("/input_broker_ops.rs") {
        "keep authenticated DVM ingress bounded; inputd owns decode and coalescing"
    } else if path.contains("/storage/") || path.contains("/block_broker_ops.rs") {
        "keep physical boot transport narrow; storaged and vfsd own storage policy"
    } else if path.contains("/io/gui") || path.contains("/io/device/display.rs") {
        "keep present hot path narrow; uiserver and devmgrd own display policy"
    } else if path.contains("/syscall/windows/") {
        "keep ABI decode narrow; syscalld and loaderd own Win32 policy"
    } else if path.contains("/broker_ops.rs") || path.contains("/ipc_ops.rs") {
        "keep capability-gated transport narrow; rootd owns admission policy"
    } else {
        "replace marked policy with a service-owned protocol before removing the marker"
    }
}

#[cfg(test)]
mod tests {
    use super::lane_for_path;
    use std::path::Path;

    #[test]
    fn dvm_frames_are_classified_as_bounded_input_substrate() {
        let path = Path::new("kernel/io-manager/src/input/dvm_frames.rs");

        assert_eq!(lane_for_path(path), "input-ingress-ring0-exception");
    }
}
