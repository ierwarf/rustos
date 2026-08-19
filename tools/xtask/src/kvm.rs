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
    DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_FLAG_DVM_READY,
    DVM_BLOCK_FLAG_READ_ONLY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_HEADER_RECORD_BYTES,
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
const DVM_BLOCK_MEDIA_BLOCK_BYTES: u32 = 2048;
const DVM_BLOCK_MEDIA_FEATURES: u64 = DVM_BLOCK_FEATURE_FLUSH;
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
const RUSTOS_DVM_BLOCK_E2E_MARKER: &str =
    "storaged: dvm-block e2e media barrier completed generation=1";
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
// This private, per-run file deliberately complements rather than mutates the
// existing KVM acceptance contract.  `runtimed` admits it only in the KVM
// topology and uses the immutable fields to select the bounded Ring3 SMP
// qualification workload.
const PRIVATE_SMP_QUALIFICATION_CONTRACT_PATH: &str =
    "system/registry/system/kvm-smp-qualification-v1.env";
// Isolates the in-kernel `ipc-call-phase-*` / `usermem-phase-*` counters to
// one `ipcbench` probe per boot: they are process-wide, so any other probe
// sharing the boot would charge the same window and make its phase totals
// undivideable by one round trip. `ipcbench` reads this directly, the same
// way `uiserver` reads the acceptance contract above, so no service mediates
// it.
const PRIVATE_IPCBENCH_PROBE_CONTRACT_PATH: &str = "system/registry/system/ipcbench-probe-v1.env";
const SMP_QUALIFICATION_WORK_UNITS: u64 = 1_000_000;
const SMP_QUALIFICATION_DEADLINE_MS: u64 = 5_000;
const SMP_QUALIFICATION_DEADLINE_US: u64 = SMP_QUALIFICATION_DEADLINE_MS * 1_000;
const SMP_QUALIFICATION_WORK_BITS: u32 = 24;
const SMP_QUALIFICATION_WORK_MASK: u64 = (1_u64 << SMP_QUALIFICATION_WORK_BITS) - 1;
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

const MILESTONE_FRAME_PREFIX: &str = "milestone-begin v=1 ";
const MILESTONE_CHECKSUM_PREFIX: &str = " checksum=";
const MILESTONE_FRAME_SUFFIX: &str = " milestone-end\"";
const MILESTONE_FNV1A64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const MILESTONE_FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SmpRuntimeEvent {
    source_line: usize,
    output_seq: u64,
    milestone_seq: u64,
    guest_ts_us: u64,
    guest_tick: u64,
    category: String,
    event: String,
    logical_cpu: u8,
    event_arg1: u64,
    process_id: Option<u64>,
    thread_id: Option<u64>,
    milestones_dropped: u64,
    debug_bytes_discarded: u64,
    frame_checksum: u64,
}

/// A complete milestone record whose outer debugcon envelope and inner
/// self-framed payload agree exactly.  This is intentionally generic: each
/// consumer applies its own narrow category/name/argument contract after the
/// transport record has been integrity-checked against byte interleaving and
/// partial writes. The FNV checksum is framing evidence, not authentication.
#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedMilestoneFrame {
    source_line: usize,
    output_seq: u64,
    milestone_seq: u64,
    guest_ts_us: u64,
    guest_tick: u64,
    category: String,
    event: String,
    event_arg0: u64,
    event_arg1: u64,
    process_id: Option<u64>,
    thread_id: Option<u64>,
    milestones_dropped: u64,
    debug_bytes_discarded: u64,
    frame_checksum: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SmpQualificationEvent {
    source_line: usize,
    output_seq: u64,
    milestone_seq: u64,
    guest_ts_us: u64,
    guest_tick: u64,
    phase: String,
    worker_id: u32,
    observed_cpu: u32,
    binding_id: u64,
    work_units: u64,
    process_id: u64,
    thread_id: u64,
    milestones_dropped: u64,
    debug_bytes_discarded: u64,
    frame_checksum: u64,
}

fn milestone_frame_checksum(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(MILESTONE_FNV1A64_OFFSET_BASIS, |checksum, byte| {
            (checksum ^ u64::from(*byte)).wrapping_mul(MILESTONE_FNV1A64_PRIME)
        })
}

fn parse_decimal_field(field: &str, name: &str) -> Option<u64> {
    field.strip_prefix(name)?.parse().ok()
}

fn parse_hex_field(field: &str, name: &str) -> Option<u64> {
    u64::from_str_radix(field.strip_prefix(name)?.strip_prefix("0x")?, 16).ok()
}

fn parse_optional_decimal_field(field: &str, name: &str) -> Option<Option<u64>> {
    let value = field.strip_prefix(name)?;
    if value == "-" {
        Some(None)
    } else {
        value.parse().ok().map(Some)
    }
}

/// Verify the complete canonical milestone transport envelope.  A substring
/// from a byte-interleaved debugcon line is evidence loss, never permission to
/// infer a missing lifecycle edge.
fn parse_verified_milestone_frame(
    line: &str,
    source_line: usize,
) -> Option<VerifiedMilestoneFrame> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let (outer, framed) = line.split_once(" msg=\"")?;
    if !framed.starts_with(MILESTONE_FRAME_PREFIX) || !framed.ends_with(MILESTONE_FRAME_SUFFIX) {
        return None;
    }

    let checksum_offset = framed.rfind(MILESTONE_CHECKSUM_PREFIX)?;
    let semantic = &framed[..checksum_offset];
    let checksum_start = checksum_offset + MILESTONE_CHECKSUM_PREFIX.len();
    let checksum_end = framed.len().checked_sub(MILESTONE_FRAME_SUFFIX.len())?;
    let checksum_text = framed.get(checksum_start..checksum_end)?;
    if checksum_text.len() != 16
        || !checksum_text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let frame_checksum = u64::from_str_radix(checksum_text, 16).ok()?;
    if milestone_frame_checksum(semantic.as_bytes()) != frame_checksum {
        return None;
    }

    let outer = outer.split_ascii_whitespace().collect::<Vec<_>>();
    if outer.len() != 9
        || outer[3] != "lvl=info"
        || outer[5] != "mod=nucleus_core::debug"
        || outer[6] != "line=0"
    {
        return None;
    }
    let outer_seq = parse_decimal_field(outer[0], "seq=")?;
    let outer_ts_us = parse_decimal_field(outer[1], "ts_us=")?;
    let outer_tick = parse_decimal_field(outer[2], "tick=")?;
    let outer_category = outer[4].strip_prefix("cat=")?;
    let outer_process_id = parse_optional_decimal_field(outer[7], "pid=")?;
    let outer_thread_id = parse_optional_decimal_field(outer[8], "tid=")?;

    let inner = semantic.split_ascii_whitespace().collect::<Vec<_>>();
    if inner.len() != 14 || inner[0] != "milestone-begin" || inner[1] != "v=1" {
        return None;
    }
    let output_seq = parse_decimal_field(inner[2], "output_seq=")?;
    let milestone_seq = parse_decimal_field(inner[3], "seq=")?;
    let guest_ts_us = parse_decimal_field(inner[4], "ts_us=")?;
    let guest_tick = parse_decimal_field(inner[5], "tick=")?;
    let category = inner[6].strip_prefix("cat=")?;
    let event = inner[7].strip_prefix("name=")?;
    let event_arg0 = parse_hex_field(inner[8], "arg0=")?;
    let event_arg1 = parse_hex_field(inner[9], "arg1=")?;
    let process_id = parse_optional_decimal_field(inner[10], "pid=")?;
    let thread_id = parse_optional_decimal_field(inner[11], "tid=")?;
    let milestones_dropped = parse_decimal_field(inner[12], "dropped=")?;
    let debug_bytes_discarded = parse_decimal_field(inner[13], "discarded_bytes=")?;

    if output_seq == 0
        || milestone_seq == 0
        || output_seq != outer_seq
        || guest_ts_us != outer_ts_us
        || guest_tick != outer_tick
        || category != outer_category
        || process_id != outer_process_id
        || thread_id != outer_thread_id
    {
        return None;
    }

    Some(VerifiedMilestoneFrame {
        source_line,
        output_seq,
        milestone_seq,
        guest_ts_us,
        guest_tick,
        category: category.to_owned(),
        event: event.to_owned(),
        event_arg0,
        event_arg1,
        process_id,
        thread_id,
        milestones_dropped,
        debug_bytes_discarded,
        frame_checksum,
    })
}

/// Accept one scheduler milestone only after its generic transport frame is
/// complete and only when it is one of the exact scheduler lifecycle events.
fn parse_verified_smp_runtime_event(line: &str, source_line: usize) -> Option<SmpRuntimeEvent> {
    let frame = parse_verified_milestone_frame(line, source_line)?;
    let expected_category = match frame.event.as_str() {
        "smp-cpu-online" => "boot",
        "smp-cpu-idle-enter"
        | "smp-cpu-first-clockevent"
        | "smp-cpu-first-user-dispatch"
        | "smp-cpu-first-reschedule-ipi" => "sched",
        _ => return None,
    };
    if frame.category != expected_category {
        return None;
    }
    let logical_cpu = u8::try_from(frame.event_arg0).ok()?;

    Some(SmpRuntimeEvent {
        source_line: frame.source_line,
        output_seq: frame.output_seq,
        milestone_seq: frame.milestone_seq,
        guest_ts_us: frame.guest_ts_us,
        guest_tick: frame.guest_tick,
        category: frame.category,
        event: frame.event,
        logical_cpu,
        event_arg1: frame.event_arg1,
        process_id: frame.process_id,
        thread_id: frame.thread_id,
        milestones_dropped: frame.milestones_dropped,
        debug_bytes_discarded: frame.debug_bytes_discarded,
        frame_checksum: frame.frame_checksum,
    })
}

fn verified_smp_runtime_events(log: &str, rustos_vcpus: u8) -> Vec<SmpRuntimeEvent> {
    log.lines()
        .enumerate()
        .filter_map(|(line, record)| parse_verified_smp_runtime_event(record, line + 1))
        .filter(|event| event.logical_cpu < rustos_vcpus)
        .collect()
}

fn parse_verified_smp_qualification_event(
    line: &str,
    source_line: usize,
) -> Option<SmpQualificationEvent> {
    let frame = parse_verified_milestone_frame(line, source_line)?;
    if frame.category != "compat"
        || !matches!(
            frame.event.as_str(),
            "smp-qualification-ready"
                | "smp-qualification-start"
                | "smp-qualification-finish"
                | "smp-qualification-complete"
        )
    {
        return None;
    }
    let process_id = frame.process_id?;
    let thread_id = frame.thread_id?;
    let binding_id = frame.event_arg1 >> SMP_QUALIFICATION_WORK_BITS;
    let work_units = frame.event_arg1 & SMP_QUALIFICATION_WORK_MASK;
    Some(SmpQualificationEvent {
        source_line: frame.source_line,
        output_seq: frame.output_seq,
        milestone_seq: frame.milestone_seq,
        guest_ts_us: frame.guest_ts_us,
        guest_tick: frame.guest_tick,
        phase: frame.event,
        worker_id: frame.event_arg0 as u32,
        observed_cpu: (frame.event_arg0 >> 32) as u32,
        binding_id,
        work_units,
        process_id,
        thread_id,
        milestones_dropped: frame.milestones_dropped,
        debug_bytes_discarded: frame.debug_bytes_discarded,
        frame_checksum: frame.frame_checksum,
    })
}

fn verified_smp_qualification_events(log: &str) -> Vec<SmpQualificationEvent> {
    log.lines()
        .enumerate()
        .filter_map(|(line, record)| parse_verified_smp_qualification_event(record, line + 1))
        .collect()
}

fn qualification_phase_sequence_is_strict(ready: u64, start: u64, finish: u64) -> bool {
    ready < start && start < finish
}

fn validate_smp_ring3_qualification_events(
    events: &[SmpQualificationEvent],
    workers: u8,
) -> Result<()> {
    if !matches!(workers, 1 | 2 | 4 | 8) {
        bail!("SMP Ring3 qualification requires a 1, 2, 4, or 8 worker topology");
    }
    let expected_events = usize::from(workers) * 3 + 1;
    if events.len() != expected_events {
        bail!(
            "SMP Ring3 qualification requires exactly {expected_events} verified events, observed {}",
            events.len()
        );
    }
    if !events.windows(2).all(|window| {
        window[0].source_line < window[1].source_line && window[0].output_seq < window[1].output_seq
    }) {
        bail!("SMP Ring3 qualification has replayed or reordered verified frames");
    }
    if events
        .iter()
        .map(|event| event.milestone_seq)
        .collect::<BTreeSet<_>>()
        .len()
        != events.len()
    {
        bail!("SMP Ring3 qualification reuses a kernel milestone sequence");
    }
    if events.iter().any(|event| {
        event.milestones_dropped != 0
            || event.debug_bytes_discarded != 0
            || event.work_units != SMP_QUALIFICATION_WORK_UNITS
    }) {
        bail!("SMP Ring3 qualification has loss counters or work units outside its contract");
    }
    let binding_ids = events
        .iter()
        .map(|event| event.binding_id)
        .collect::<BTreeSet<_>>();
    if binding_ids.len() != 1 || binding_ids.first() == Some(&0) {
        bail!("SMP Ring3 qualification requires one nonzero kernel binding ID");
    }

    let process_ids = events
        .iter()
        .map(|event| event.process_id)
        .collect::<BTreeSet<_>>();
    if process_ids.len() != 1 || process_ids.first() == Some(&0) {
        bail!("SMP Ring3 qualification requires exactly one nonzero process identity");
    }
    let worker_ids = events
        .iter()
        .map(|event| event.worker_id)
        .collect::<BTreeSet<_>>();
    let observed_cpus = events
        .iter()
        .map(|event| event.observed_cpu)
        .collect::<BTreeSet<_>>();
    let expected_workers = (0..u32::from(workers)).collect::<BTreeSet<_>>();
    if worker_ids != expected_workers || observed_cpus != expected_workers {
        bail!(
            "SMP Ring3 qualification workers and observed CPUs are not exact topology bijections"
        );
    }

    let earliest_start = events
        .iter()
        .filter(|event| event.phase == "smp-qualification-start")
        .min_by_key(|event| event.output_seq)
        .context("SMP Ring3 qualification is missing a start event")?;
    let latest_ready = events
        .iter()
        .filter(|event| event.phase == "smp-qualification-ready")
        .max_by_key(|event| event.output_seq)
        .context("SMP Ring3 qualification is missing a ready event")?;
    if latest_ready.output_seq >= earliest_start.output_seq {
        bail!("SMP Ring3 qualification started before every worker was ready");
    }
    let earliest_ready_ts = events
        .iter()
        .filter(|event| event.phase == "smp-qualification-ready")
        .map(|event| event.guest_ts_us)
        .min()
        .context("SMP Ring3 qualification is missing a ready timestamp")?;
    let terminal_events = events
        .iter()
        .filter(|event| event.phase == "smp-qualification-complete")
        .collect::<Vec<_>>();
    if terminal_events.len() != 1 {
        bail!("SMP Ring3 qualification requires one terminal completion record");
    }
    let terminal = terminal_events[0];
    if terminal.worker_id != 0 || terminal.observed_cpu != 0 {
        bail!("SMP Ring3 qualification terminal record must belong to worker zero on CPU zero");
    }
    let latest_finish = events
        .iter()
        .filter(|event| event.phase == "smp-qualification-finish")
        .max_by_key(|event| event.output_seq)
        .context("SMP Ring3 qualification is missing a finish event")?;
    if terminal.output_seq <= latest_finish.output_seq {
        bail!("SMP Ring3 qualification completed before every worker finish was published");
    }
    if terminal.guest_ts_us.saturating_sub(earliest_ready_ts) > SMP_QUALIFICATION_DEADLINE_US {
        bail!("SMP Ring3 qualification exceeded its one immutable observed deadline window");
    }
    let mut worker_phases = BTreeMap::new();
    for event in events {
        if event.observed_cpu != event.worker_id {
            bail!("SMP Ring3 qualification worker CPU does not match its assigned worker ID");
        }
        if event.phase == "smp-qualification-complete" {
            continue;
        }
        let phase = worker_phases
            .entry(event.worker_id)
            .or_insert_with(BTreeMap::new);
        if phase.insert(event.phase.as_str(), event).is_some() {
            bail!("SMP Ring3 qualification contains a duplicate worker phase");
        }
    }
    for worker_id in expected_workers {
        let phases = worker_phases
            .get(&worker_id)
            .context("SMP Ring3 qualification is missing a worker")?;
        if phases.len() != 3 {
            bail!("SMP Ring3 qualification worker is missing a lifecycle phase");
        }
        let ready = *phases
            .get("smp-qualification-ready")
            .context("SMP Ring3 qualification is missing ready")?;
        let start = *phases
            .get("smp-qualification-start")
            .context("SMP Ring3 qualification is missing start")?;
        let finish = *phases
            .get("smp-qualification-finish")
            .context("SMP Ring3 qualification is missing finish")?;
        if ready.process_id != start.process_id
            || ready.process_id != finish.process_id
            || ready.thread_id != start.thread_id
            || ready.thread_id != finish.thread_id
            || ready.observed_cpu != start.observed_cpu
            || ready.observed_cpu != finish.observed_cpu
            || ready.work_units != start.work_units
            || ready.work_units != finish.work_units
        {
            bail!("SMP Ring3 qualification worker identity or work changed across phases");
        }
        if ready.thread_id == 0 {
            bail!("SMP Ring3 qualification worker thread identity must be nonzero");
        }
        if !qualification_phase_sequence_is_strict(
            ready.output_seq,
            start.output_seq,
            finish.output_seq,
        ) || ready.guest_ts_us > earliest_start.guest_ts_us
            || ready.guest_ts_us > start.guest_ts_us
            || start.guest_ts_us > finish.guest_ts_us
            || finish.guest_ts_us - start.guest_ts_us > SMP_QUALIFICATION_DEADLINE_US
        {
            bail!("SMP Ring3 qualification worker phase order or deadline is invalid");
        }
    }
    let worker_zero = worker_phases
        .get(&0)
        .context("SMP Ring3 qualification is missing worker zero")?;
    let worker_zero_finish = worker_zero
        .get("smp-qualification-finish")
        .context("SMP Ring3 qualification is missing worker zero finish")?;
    if terminal.process_id != worker_zero_finish.process_id
        || terminal.thread_id != worker_zero_finish.thread_id
        || terminal.work_units != worker_zero_finish.work_units
    {
        bail!("SMP Ring3 qualification terminal record changed worker-zero identity or work");
    }
    let thread_ids = events
        .iter()
        .map(|event| event.thread_id)
        .collect::<BTreeSet<_>>();
    if thread_ids.len() != usize::from(workers) || thread_ids.contains(&0) {
        bail!("SMP Ring3 qualification requires one distinct nonzero thread identity per worker");
    }
    Ok(())
}

fn smp_ring3_qualification_is_complete(log: &str, workers: u8) -> bool {
    validate_smp_ring3_qualification_events(&verified_smp_qualification_events(log), workers)
        .is_ok()
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
    let events = verified_smp_runtime_events(log, rustos_vcpus);
    let mut missing = Vec::new();
    for logical_cpu in 0..rustos_vcpus {
        let online_generation = events
            .iter()
            .find(|event| {
                event.logical_cpu == logical_cpu
                    && event.event == "smp-cpu-online"
                    && event.event_arg1 != 0
            })
            .map(|event| event.event_arg1);
        for name in [
            "smp-cpu-online",
            "smp-cpu-idle-enter",
            "smp-cpu-first-clockevent",
            "smp-cpu-first-user-dispatch",
        ] {
            let marker = format!("name={name} arg0=0x{logical_cpu:x}");
            let present = events.iter().any(|event| {
                event.logical_cpu == logical_cpu
                    && event.event == name
                    && match name {
                        "smp-cpu-online" => online_generation == Some(event.event_arg1),
                        "smp-cpu-idle-enter" => online_generation == Some(event.event_arg1),
                        _ => event.event_arg1 == 1,
                    }
            });
            if !present {
                missing.push(marker);
            }
        }
        if rustos_vcpus > 1 {
            let marker = format!("name=smp-cpu-first-reschedule-ipi arg0=0x{logical_cpu:x}");
            if !events.iter().any(|event| {
                event.logical_cpu == logical_cpu
                    && event.event == "smp-cpu-first-reschedule-ipi"
                    && event.event_arg1 == 1
            }) {
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
include!("kvm/user_debug_records.rs");
include!("kvm/tests.rs");
