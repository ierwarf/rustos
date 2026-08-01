use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc,
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use driver_domain_protocol::{
    DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_FEATURE_FUA,
    DVM_BLOCK_FLAG_DVM_READY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_HEADER_RECORD_BYTES,
    DVM_GPU_ATLAS_POOL_HEADER_OFFSET, DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET,
    DVM_GUI_SURFACE_SLOT_COUNT, DVM_INPUT_RING_APERTURE_BYTES, DVM_NET_APERTURE_BYTES,
    DvmBlockHeader, DvmGpuAtlasPoolHeader, DvmGuiSurfaceMessage, DvmGuiSurfacePoolHeader,
    DvmInputRingHeader, DvmNetHeader,
};
use fatfs::Seek as FatSeek;
use fatfs::Write as FatWrite;
use fs_err as fs;
use rustos_driver_domain_host::{
    ControlContract as HostControlContract, ControlSecret, HostControlListener, InputRingSink,
    IvshmemDoorbellServer, ProbeResult,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::Result;
use crate::config::Config;
use crate::util::{resolve_command_path, run_command};

const DVM_KERNEL: &str = "rustos-linux-dvm-x86_64.bzImage";
const DVM_ROOTFS: &str = "rustos-linux-dvm-x86_64.rootfs.cpio.zst";
const DVM_CONFIG: &str = "rustos-linux-dvm-x86_64.config";
const DVM_KERNEL_CONFIG: &str = "rustos-linux-dvm-x86_64.kernel.config";
const DVM_MODULE_SIGNING_CERT: &str = "rustos-linux-dvm-x86_64.module-signing.x509";
const DVM_SOURCES_LOCK: &str = "rustos-linux-dvm-x86_64.sources.lock";
const DVM_CONTROL_ARTIFACT: &str = "rustos-linux-dvm-x86_64.control.env";
const DVM_DEV_OUTPUT_MARKER: &str = "out/buildroot-output/.rustos-dvm-dev-output-v1";
const DVM_MANIFEST: &str = "rustos-linux-dvm-x86_64.manifest";
const DVM_MANIFEST_SCHEMA: &str = "9";
const DVM_CONTROL_CONTRACT: &str = "board/overlay/usr/share/rustos-dvm/control-plane-v1.env";
const DVM_CONTROL_PROTOCOL: &str = "agent-v1";
const DVM_CONTROL_STATE: &str = "control";
const DVM_CONTROL_TRANSPORT: &str = "kvm-vsock";
const DVM_CONTROL_AUTHENTICATION: &str = "dvm-agent-hmac-sha256-v1";
const DVM_CONTROL_CAPABILITIES: &str =
    "health,device-inventory,driver-inventory,display-evidence-v2,block-evidence-v1,input-stream";
const RUSTOS_BOOT_MARKER: &str = "rootd: core services ready, spawning initd via loaderd";
const RUSTOS_INIT_IDENTITY_MARKER: &str = "initd: identity endpoint registered";
const RUSTOS_BOOT_MILESTONE: &str = "name=product-root-core-ready";
const RUSTOS_INIT_IDENTITY_MILESTONE: &str = "name=product-init-identity-ready";
const RUSTOS_POST_INIT_PROVENANCE_MARKER: &str =
    "rootd: post-init deferred-spawn provenance verified";
const RUSTOS_GPU_SCENE_COMPILER_MARKER: &str =
    "uiserver: gpu-scene compiler ready contract=3 public-abi=0 dvm-submit=1";
const RUSTOS_GPU_ACTIVE_MARKER: &str = "uiserver: gpu-compositor active contract=3";
const WAYCLICK_FIRST_FRAME_MARKER: &str = "wayclick: first frame presented";
const DVM_KEYBOARD_INGRESS_MARKER: &str = "inputd: DVM keyboard ingress observed";
const DVM_POINTER_INGRESS_MARKER: &str = "inputd: DVM pointer ingress observed";
const DVM_GPU_COMPOSITOR_MARKER: &str = "rustos-dvm-gpu: ready contract=1";
const DVM_GPU_LIVE_MARKER: &str = "rustos-dvm-display: gpu-compositor primed contract=3";
const DVM_BOOTSTRAP_FRAME_MARKER: &str = "bootstrap=local-nonblack";
const RUSTOS_DVM_BLOCK_MARKER: &str = "dvm-block: transport installed generation=1";
const RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER: &str = "dvm-block: first completion observed";
const RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MILESTONE: &str = "name=dvm-block-first-completion";
const RUSTOS_DVM_BLOCK_E2E_MARKER: &str = "storaged: dvm-block e2e flush completed generation=1";
const RUSTOS_DVM_BLOCK_E2E_MILESTONE: &str = "name=product-storage-ready";
const RUSTOS_DVM_BLOCK_FLUSH_FAULT_MARKER: &str =
    "dvm-block: injected device fault operation=block.flush generation=1";
const GUI_DVM_OFFLINE_MARKER: &str = "gui-dvm: peer offline lease revoked";
const GUI_DVM_REBOUND_MARKER: &str = "gui-dvm: peer ready lease rebound";
// The higher-half trace is emitted before the structured debugcon sink becomes
// durable on every OVMF path. `clocksource-ready` is the earliest milestone
// guaranteed to survive in the per-process capture and therefore the first
// admissible marker for a fresh-guest recovery epoch.
const RUSTOS_REBOOT_ENTRY_MARKER: &str = "name=clocksource-ready";
const DVM_BLOCK_READY_MARKER: &str = "rustos-dvm-block: ready abi=2 generation=";
const DVM_GPU_PIPELINE_PRIME_TIMEOUT_US: u64 = 500_000;
const DVM_GPU_HEALTH_SAMPLES: u64 = 3;
const PHYSICAL_GPU_SMOKE_MIN_FRAMES: usize = 4;
const DEFAULT_UI_FPS_ACTIVE_WINDOWS: usize = 3;
// Long UI acceptance runs must be able to prove one full minute of
// consecutive one-second samples. Keep the bound finite so malformed CLI
// input cannot turn the smoke runner into an unbounded soak.
const MAX_UI_FPS_ACTIVE_WINDOWS: usize = 60;
// The end-to-end cursor contract is 60 accepted motion updates per second.
// Require at least 55 in every measured one-second window (over 90%) so a
// single timer boundary cannot fail an otherwise continuous 60 Hz stream.
const MIN_UI_FPS_INPUT_EVENTS: u64 = 55;
const MIN_UI_FPS_CURSOR_MOVES: u64 = 50;
const MAX_UI_INPUT_GAP_MS: u64 = 50;
const MIN_UI_CURSOR_SPAN: u64 = 96;
// Completing the direct DMA-BUF atomic commit must leave meaningful headroom
// inside a 16.67 ms 60 Hz frame.  The relay never copies pixel payloads.
const MAX_DVM_DISPLAY_RELAY_US: u64 = 12_000;
const MAX_DVM_GPU_RENDER_US: u64 = 16_667;
const DVM_DISPLAY_WIDTH: u32 = 1600;
const DVM_DISPLAY_HEIGHT: u32 = 900;
const DVM_DISPLAY_REGION_BYTES: u64 = 128 * 1024 * 1024;
const DVM_GPU_ATLAS_WIDTH: u32 = 2048;
const DVM_GPU_ATLAS_HEIGHT: u32 = 2048;
// Keep the KVM proof topology identical to the supervised physical-display
// DVM. Mesa virgl plus the AMD radeonsi/LLVM runtime, firmware, the compressed
// initrd, XZ workspace, and unpacked ramfs coexist during early boot. Keep a
// measured two-GiB floor so GPU enablement cannot turn memory pressure into a
// nondeterministic 30-second readiness failure.
const DVM_GUEST_MEMORY: &str = "2048M,maxmem=3G,slots=2";
// QEMU maps the shared pixel backend as cacheable device memory at this
// reserved, 2 MiB-aligned guest-physical address in both guests. The ivshmem
// BAR carries only bounded control records and MSI-X doorbells.
const DVM_DISPLAY_PIXEL_PHYS_ADDR: u64 = 0x1_0000_0000;
const DVM_DISPLAY_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const DVM_BLOCK_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
const INTERACTIVE_IDLE_TICKS: usize = 3;
const DVM_INPUT_FIRST_PEER_TIMEOUT: Duration = Duration::from_secs(5);
/// Authentication remains a five-second setup gate, but a real RustOS input
/// consumer appears only after the policy services and uiserver are running.
/// Keep that distinct boot dependency bounded without falsely admitting the
/// transport-only MSI-X state.
// This is a guest policy-publication deadline, not the host acceptance-soak
// duration. Keep it at the product's finite 30-second service ceiling even
// when the host runner collects a longer sequence of performance samples.
const DVM_INPUT_POLICY_READY_TIMEOUT: Duration = Duration::from_secs(30);
// The RustOS MSI-X receive substrate deliberately rejects x2APIC until an
// interrupt-remapping implementation can supply a non-truncated destination
// ID. The KVM validation topology therefore pins the guest to xAPIC instead of
// weakening the kernel with a guessed x2APIC message format.
// RustOS admits TSC as a clocksource only when CPUID advertises invariant TSC.
// QEMU masks `invtsc` by default because it constrains live migration, even on
// a constant/nonstop-TSC KVM host. This local validation topology is not live
// migrated, so expose the host guarantee explicitly; HPET remains the guest's
// independent calibration/watchdog reference.
const RUSTOS_DVM_KVM_CPU: &str = "host,-x2apic,+invtsc";
const DVM_NET_REGION_BYTES: u64 = DVM_NET_APERTURE_BYTES;
// Private acceptance state must not rewrite signed early-system registries:
// vfsd intentionally serves those immutable bootstrap copies before the DVM
// volume. The KVM harness instead adds one unsigned, diagnostics-only contract
// to its private disk; runtimed accepts only these fixed boolean fields.
const PRIVATE_ACCEPTANCE_CONTRACT_PATH: &str = "system/registry/system/kvm-acceptance-v1.env";
const NETPROBE_QEMU_REACHABLE_MARKER: &str = "netprobe: qemu gateway reachable";
const MIN_DVM_GUEST_CID: u32 = 3;
const VHOST_VSOCK_DEVICE: &str = "/dev/vhost-vsock";
// Readiness plus a 60-window performance proof needs headroom for boot. This
// is only the host runner's hard process deadline; guest service waits retain
// their narrower class-specific bounds.
const MAX_SMOKE_TIMEOUT: u64 = 120;
const PHYSICAL_GPU_REQUIRED_MEMLOCK: u64 = 4 * 1024 * 1024 * 1024;
const ACPI_VFCT_HEADER_BYTES: usize = 0x4c;
const ACPI_VFCT_VBIOS_OFFSET: usize = 0x34;
const ACPI_VFCT_IMAGE_HEADER_BYTES: usize = 28;
const ACPI_VFCT_IMAGE_LENGTH_OFFSET: usize = 24;
const ACPI_VFCT_MAX_BYTES: usize = 4 * 1024 * 1024;

fn rustos_marker_present(log: &str, marker: &str) -> bool {
    log.contains(marker)
        || marker == RUSTOS_BOOT_MARKER
            && (log.contains(RUSTOS_BOOT_MILESTONE)
                // The bounded milestone/debugcon channel may drop one
                // contended record. Init identity is a strict causal successor:
                // rootd cannot spawn initd before the core-ready transition.
                || log.contains(RUSTOS_INIT_IDENTITY_MILESTONE))
        || marker == RUSTOS_INIT_IDENTITY_MARKER && log.contains(RUSTOS_INIT_IDENTITY_MILESTONE)
        || marker == RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER
            && log.contains(RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MILESTONE)
        || marker == RUSTOS_DVM_BLOCK_E2E_MARKER && log.contains(RUSTOS_DVM_BLOCK_E2E_MILESTONE)
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn storage_acceptance_accepts_the_reliable_kernel_milestones() {
        let log = "name=dvm-block-first-completion\nname=product-storage-ready";
        assert!(rustos_marker_present(
            log,
            RUSTOS_DVM_BLOCK_FIRST_COMPLETION_MARKER
        ));
        assert!(rustos_marker_present(log, RUSTOS_DVM_BLOCK_E2E_MARKER));
    }
}

fn smp_runtime_missing_markers(log: &str, rustos_vcpus: u8) -> Vec<String> {
    let mut missing = Vec::new();
    for logical_cpu in 0..rustos_vcpus {
        for name in [
            "smp-cpu-online",
            "smp-cpu-idle-enter",
            "smp-cpu-first-clockevent",
            "smp-cpu-first-user-dispatch",
        ] {
            let marker = format!("name={name} arg0=0x{logical_cpu:x}");
            if !log.contains(marker.as_str()) {
                missing.push(marker);
            }
        }
        if rustos_vcpus > 1 {
            let marker = format!("name=smp-cpu-first-reschedule-ipi arg0=0x{logical_cpu:x}");
            if !log.contains(marker.as_str()) {
                missing.push(marker);
            }
        }
    }
    missing
}

include!("kvm/options.rs");
include!("kvm/manifest.rs");
include!("kvm/help.rs");
include!("kvm/layout.rs");
include!("kvm/guest.rs");
include!("kvm/evidence.rs");
include!("kvm/tests.rs");
