use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::Result;

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
                    | "usb-runtime-substrate-exception"
                    | "display-present-substrate-exception"
                    | "dvm-transport-substrate-exception"
                    | "bootstrap-device-route-exception"
                    | "vfs-bootstrap-hotpath-exception"
                    | "vfs-socket-fd-substrate-exception"
                    | "abi-substrate-reference"
                    | "ring3-owner-reference"
                    | "already-migrated-reference"
                    | "ko-slowpath-ring3"
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
    let ko_slowpath_ring3 = entries
        .iter()
        .filter(|entry| entry.lane == "ko-slowpath-ring3")
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
    println!("ko_slowpath_ring3_loc={ko_slowpath_ring3}");
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
    if path.ends_with("/driver/ko_slowpath_migration.rs") {
        "driverd-linux-ko-compat"
    } else if path.ends_with("/user/compat_slowpath_migration.rs") {
        "syscalld-procd-vfsd-netd-devmgrd"
    } else if path.ends_with("/memory/pager_slowpath_migration.rs") {
        "pagerd-syscalld-loaderd"
    } else if path.ends_with("/user/process_slowpath_migration.rs") {
        "procd-syscalld-loaderd"
    } else if path.contains("/usb/xhci.rs") {
        "driverd-devmgrd-inputd"
    } else if path.contains("/driver/serio.rs") {
        "linux-ko-compat"
    } else if path.contains("/input/i8042.rs") {
        "inputd"
    } else if path.contains("/usb/core.rs") {
        "driverd-devmgrd-inputd"
    } else if path.contains("/storage/ahci.rs") || path.contains("/storage/nvme.rs") {
        "storaged-driverd"
    } else if path.contains("/driver/linux/")
        || path.contains("/driver/devres.rs")
        || path.contains("/driver/export.rs")
        || path.contains("/driver/kernel_api.rs")
        || path.contains("/driver/loader.rs")
        || path.contains("/driver/module_registry.rs")
        || path.ends_with("/driver/pci.rs")
    {
        "linux-ko-compat"
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
    } else if path.ends_with("/io/dvm_network.rs") {
        "netd"
    } else if path.ends_with("/io/dvm_display.rs") {
        "uiserver"
    } else if path.contains("/usb/") || path.contains("/input/") || path.contains("/serio.rs") {
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
    if path.ends_with("/driver/ko_slowpath_migration.rs") {
        "ko-slowpath-ring3"
    } else if path.ends_with("/user/compat_slowpath_migration.rs") {
        "compat-slowpath-ring3"
    } else if path.ends_with("/memory/pager_slowpath_migration.rs") {
        "pager-slowpath-ring3"
    } else if path.ends_with("/user/process_slowpath_migration.rs") {
        "process-slowpath-ring3"
    } else if path.contains("/driver/linux/")
        || path.contains("/driver/devres.rs")
        || path.contains("/driver/export.rs")
        || path.contains("/driver/kernel_api.rs")
        || path.contains("/driver/loader.rs")
        || path.contains("/driver/module_registry.rs")
        || path.ends_with("/driver/pci.rs")
        || path.contains("/usb/emulation.rs")
    {
        "compat-ring0-exception"
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
    } else if path.contains("/input/i8042.rs")
        || path.contains("/input_core.rs")
        || path.contains("/input/event_queue.rs")
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
    } else if path.contains("/driver/serio.rs") {
        "compat-ring0-exception"
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
    if path.ends_with("/driver/ko_slowpath_migration.rs") {
        "migrate sleepable Linux .ko init/probe/resource policy into driverd while ring0 remains the privileged broker"
    } else if path.ends_with("/user/compat_slowpath_migration.rs") {
        "move residual Linux/Win32 syscall slow-path policy into owner services while ring0 keeps ABI decode and brokers"
    } else if path.ends_with("/memory/pager_slowpath_migration.rs") {
        "move VMA/backing/accounting policy into pagerd/syscalld while ring0 keeps page-table/frame substrate"
    } else if path.ends_with("/user/process_slowpath_migration.rs") {
        "move process hierarchy/wait/credentials/signal policy into procd/syscalld while ring0 keeps scheduler substrate"
    } else if path.contains("/usb/xhci.rs") {
        "RustOS native xHCI MMIO/DMA/IRQ transfer substrate exception; provider/device/input policy is driverd/devmgrd/inputd-owned"
    } else if path.contains("/driver/serio.rs") {
        "explicit Linux .ko serio compatibility bus substrate exception; do not migrate .ko execution to ring3"
    } else if path.contains("/input/i8042.rs") {
        "i8042 raw input ingress substrate exception; keyboard/mouse policy is inputd-owned"
    } else if path.contains("/driver/linux/")
        || path.contains("/driver/devres.rs")
        || path.contains("/driver/export.rs")
        || path.contains("/driver/kernel_api.rs")
        || path.contains("/driver/loader.rs")
        || path.contains("/driver/module_registry.rs")
        || path.ends_with("/driver/pci.rs")
        || path.contains("/usb/emulation.rs")
    {
        "explicit Linux .ko ring0 compatibility substrate exception; do not migrate .ko execution to ring3"
    } else if path.ends_with("/compat/src/user/process/linux.rs") {
        "pre-loaderd bootstrap Linux ELF substrate exception; normal ELF policy is loaderd/procd-owned"
    } else if path.ends_with("/compat/src/user/console_host.rs") {
        "pre-loaderd bootstrap console-host substrate exception; normal launch policy is loaderd/sessiond-owned"
    } else if path.contains("/memory_ops.rs") {
        "pre-syscalld bootstrap memory substrate exception; post-bootstrap mmap/brk policy is syscalld-owned"
    } else if path.contains("/block_broker_ops.rs") {
        "physical boot-block substrate exception; boot extent policy is storaged/vfsd-owned"
    } else if path.contains("/storage/ahci.rs") {
        "built-in AHCI bootstrap transport exception; post-bootstrap storage/provider policy is storaged/driverd-owned"
    } else if path.contains("/storage/nvme.rs") {
        "built-in NVMe bootstrap transport exception; post-bootstrap storage/provider policy is storaged/driverd-owned"
    } else if path.contains("/storage/boot_volume.rs") {
        "boot-volume bootstrap substrate exception; normal rootfs policy is vfsd/storaged-owned"
    } else if path.ends_with("/storage/block.rs")
        || path.contains("/storage/block/boot.rs")
        || path.contains("/storage/block/io.rs")
    {
        "physical boot-block substrate exception; post-bootstrap block policy is storaged/pagerd-owned"
    } else if path.contains("/io/console.rs") {
        "bootstrap console buffer substrate exception; console presentation policy is sessiond/runtimed-owned"
    } else if path.contains("/io/tty.rs") {
        "bootstrap TTY buffer substrate exception; normal line discipline and session routing are sessiond/runtimed-owned"
    } else if path.contains("/service_ops/ipc_helpers.rs") {
        "fixed bootstrap spawn and explicit device-route substrate exception; rootd/loaderd/vfsd/devmgrd own policy"
    } else if path.contains("/sysops/device.rs") {
        "hot display-present ioctl substrate exception; policy-sensitive ioctls route through devmgrd/sessiond"
    } else if path.contains("/io/device/display.rs") {
        "hot display ioctl execution substrate exception; admission and routing policy is devmgrd/uiserver-owned"
    } else if path.ends_with("/driver/mod.rs") {
        "privileged hardware-probe substrate exception; driver/provider policy is driverd-owned"
    } else if path.contains("/syscall/windows/") {
        "Win32 syscall decode substrate exception; syscall policy is syscalld/loaderd-owned"
    } else if path.contains("/lifecycle_broker_ops.rs") {
        "capability-gated lifecycle event substrate exception; restart policy is procd/rootd-owned"
    } else if path.contains("/proc_broker_ops.rs") {
        "capability-gated process prepare substrate exception; admission policy is procd-owned"
    } else if path.contains("/device_broker_ops.rs") {
        "capability-gated device/session ioctl substrate exception; policy is devmgrd/sessiond-owned"
    } else if path.contains("/ipc_ops.rs") {
        "service endpoint registry and capability-gate substrate exception; rootd owns capability lease policy"
    } else if path.contains("/service_ops/vfs_socket.rs") {
        "VFS/socket fd-table and user-copy substrate exception; file/socket policy is vfsd/netd-owned"
    } else if path.contains("/service_ops/vfs_meta.rs") {
        "bootstrap stat and hot ioctl route substrate exception; policy is vfsd/devmgrd/sessiond-owned"
    } else if path.contains("/service_ops/futex_thread.rs")
        || path.contains("/service_ops/process_time.rs")
    {
        "scheduler/thread substrate exception; futex, clone, and time admission policy is procd/syscalld-owned"
    } else if path.contains("/input/i8042.rs")
        || path.contains("/input_core.rs")
        || path.contains("/input/event_queue.rs")
        || path.ends_with("/input/mod.rs")
        || path.ends_with("/driver/input.rs")
        || path.contains("/input_broker_ops.rs")
        || path.contains("/input/dispatcher.rs")
        || path.contains("/input/keyboard.rs")
        || path.contains("/usb/runtime.rs")
    {
        "bounded input ingress substrate exception; read/coalescing/evdev policy is inputd-owned"
    } else if path.contains("/memfd.rs") {
        "memfd fd/frame/page-table substrate exception; creation and mapping admission policy is syscalld/pagerd-owned"
    } else if path.ends_with("/io/dvm_network.rs") {
        "fixed DVM Ethernet transport substrate exception; netd owns socket/TCP policy and L0 owns liveness/revocation policy"
    } else if path.ends_with("/io/dvm_display.rs") {
        "fixed DVM framebuffer transport substrate exception; uiserver owns mode, damage, and presentation policy"
    } else if path.contains("/io/gui") {
        "display present/framebuffer substrate exception; mode, damage, and presentation policy is uiserver-owned"
    } else if path.contains("/usb/xhci.rs")
        || path.contains("/usb/core.rs")
        || path.contains("/usb/manager.rs")
        || path.ends_with("/usb/mod.rs")
    {
        "USB native/compat runtime substrate exception; provider/device/input policy is driverd/devmgrd/inputd-owned"
    } else if path.ends_with("/compat/src/user/mod.rs")
        || path.ends_with("/compat/src/user/sysops/mod.rs")
    {
        "ring0 ABI module substrate; compat policy is owned by services"
    } else if path.contains("/syscall/linux/support.rs") {
        "Linux signal frame substrate exception; pending-signal selection and disposition policy is procd-owned"
    } else if path.contains("services/vfsd/") {
        "already ring3 service owner; marker restored for historical audit only"
    } else if path.contains("/sysops/stat.rs")
        || path.ends_with("/ps/src/user/linux.rs")
        || path.ends_with("/compat/src/user/process/mod.rs")
        || path.contains("/socket.rs")
        || path.contains("/epoll.rs")
        || path.contains("/io/session.rs")
    {
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
    } else if path.ends_with("/compat/src/user/sysops/mod.rs") {
        "move sysop namespace policy into vfsd/devmgrd/sessiond, keep module routing substrate"
    } else if path.contains("/sysops/console.rs") || path.contains("/console_host.rs") {
        "move console host/sysop policy into loaderd/sessiond/runtimed"
    } else if path.contains("/usb/runtime.rs") || path.contains("/usb/synthetic.rs") {
        "move HID parse/state policy into inputd, keep USB callback source"
    } else if path.contains("/serio.rs") || path.contains("/input/i8042.rs") {
        "move legacy input routing into inputd service-driver path"
    } else if path.contains("/io/gui") {
        "move provider/display policy into uiserver or service-driver path"
    } else if path.contains("/storage/") {
        "move post-bootstrap storage policy into storaged, keep raw block broker"
    } else if path.contains("/process/") || path.contains("/proc_broker_ops.rs") {
        "move cold image/process policy into loaderd/procd before deleting ring0 parser branches"
    } else {
        "replace marked policy with service-owned protocol and then remove marker"
    }
}
