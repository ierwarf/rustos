use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;

const MARKER_START: &str = "RING3-MIGRATION-REFERENCE START";
const MARKER_END: &str = "RING3-MIGRATION-REFERENCE END";

struct InventoryEntry {
    path: PathBuf,
    marked_loc: usize,
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
    let excluded = entries
        .iter()
        .filter(|entry| entry.lane == "exclude-ko-eval")
        .map(|entry| entry.marked_loc)
        .sum::<usize>();
    let active = total.saturating_sub(excluded);

    println!("ring3 migration inventory");
    println!("total_marked_loc={total}");
    println!("excluded_xhci_nvme_loc={excluded}");
    println!("active_batch_marked_loc={active}");
    println!();
    println!("loc\tlane\towner\taction\tpath");
    for entry in entries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            entry.marked_loc,
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
        if !content.contains(MARKER_START) {
            continue;
        }
        let relative = path.strip_prefix(root_dir).unwrap_or(path.as_path());
        entries.push(InventoryEntry {
            path: relative.to_path_buf(),
            marked_loc: marked_source_loc(&content),
            owner: owner_for_path(relative),
            lane: lane_for_path(relative),
            action: action_for_path(relative),
        });
    }
    Ok(())
}

fn marked_source_loc(content: &str) -> usize {
    let mut in_marker = false;
    let mut total = 0usize;
    for line in content.lines() {
        if line.contains(MARKER_START) {
            in_marker = true;
            continue;
        }
        if line.contains(MARKER_END) {
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
        "usb-ko-eval"
    } else if path.contains("/storage/nvme.rs") {
        "storage-ko-eval"
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
    if path.contains("/usb/xhci.rs") || path.contains("/storage/nvme.rs") {
        "exclude-ko-eval"
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
    if path.contains("/usb/xhci.rs") || path.contains("/storage/nvme.rs") {
        "hold for .ko replacement decision"
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
