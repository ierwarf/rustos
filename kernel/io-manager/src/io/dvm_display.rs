//! DVM display aperture, immutable snapshot, damage, and provider substrate.
//!
//! - **Owner:** `kernel-io-manager` owns fixed shared-memory mechanics;
//!   `uiserver` owns composition and the Linux DVM owns device presentation.
//! - **Boundary:** Shared headers, slot state, geometry, pitch, damage,
//!   generations, and completions are untrusted.
//! - **Lifecycle:** Transactionally install matched apertures/vectors, publish
//!   complete immutable snapshots, release exact slots, revoke provider, and
//!   re-prime a new epoch.
//! - **Concurrency:** IRQ callbacks mark pending only; copy/publish ordering and
//!   provider replacement are serialized in normal context.
//! - **Failure:** Invalid topology/damage/history, timeout, stale completion,
//!   restart, and detach race retain the previous valid front or revoke.
//! - **Forbidden:** No user pointer, partial snapshot publication, CPU-render
//!   success fallback, untracked vector, or stale slot authority.
//! - **Evidence:** `dvm-display-ingress` and `gpu-frame-lifecycle`.
// RING3-MIGRATION-REFERENCE START: GUI-DVM display transport substrate.
// Policy stays in uiserver and the GUI DVM. Ring0 only maps the exact
// host-created three-slot pool, maps one slot-scoped staging capability to
// uiserver, commits bounded command metadata, and handles the two fixed MSI-X
// leaf notifications without composing or interpreting a guest command stream.
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

use crate::transport_types::{
    DISPLAY_FRAMEBUFFER_FLAG_DVM_SCANOUT, DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER,
    DisplayFramebufferRegistration, DisplayPixelFormat,
};
use driver_domain_protocol::{
    DVM_GPU_ATLAS_COMMAND_SLOT_BYTES, DVM_GPU_ATLAS_CONTEXT_EPOCH_OFFSET,
    DVM_GPU_ATLAS_CONTEXT_ID_OFFSET, DVM_GPU_ATLAS_DAMAGE_BYTES, DVM_GPU_ATLAS_POOL_HEADER_OFFSET,
    DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET, DVM_GPU_ATLAS_PRIME_FENCE_OFFSET,
    DVM_GPU_ATLAS_SUBMIT_BYTES, DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF,
    DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY, DVM_GPU_PRIME_COMPLETION_BYTES,
    DVM_GPU_RENDER_HEADER_BYTES, DVM_GPU_RENDER_MAX_BATCH_BYTES, DVM_GPU_RENDER_MAX_IN_FLIGHT,
    DVM_GPU_RENDER_SOURCE_BYTES, DVM_GUI_SURFACE_POOL_DVM_RECORD_OFFSET,
    DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET, DVM_GUI_SURFACE_POOL_HEADER_BYTES,
    DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET,
    DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET,
    DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, DVM_GUI_SURFACE_SLOT_COUNT, DvmDisplayDamage,
    DvmGpuAtlasCompletion, DvmGpuAtlasDamage, DvmGpuAtlasPoolHeader, DvmGpuAtlasSubmit,
    DvmGpuPrimeCompletion, DvmGpuPrimeCompletionStatus, DvmGpuRenderBatchHeader,
    DvmGpuRenderSource, DvmGuiSurfaceMessage, DvmGuiSurfaceMessageKind, DvmGuiSurfacePoolHeader,
    dvm_gpu_atlas_completion_ack_offset, dvm_gpu_atlas_completion_offset,
    dvm_gpu_atlas_completion_sequence_offset, dvm_gpu_atlas_damage_is_valid,
    dvm_gpu_atlas_invitation_offset,
};

mod app_buffer;
pub use app_buffer::AppBufferTransportError;
pub(crate) use app_buffer::{
    aperture_phys as app_buffer_aperture_phys, begin as begin_app_buffer,
    read_range as app_buffer_read_range,
    scrub_range as app_buffer_scrub_range, take as take_app_buffer,
};
mod installation;
mod peer;

use installation::*;
use peer::*;
pub(crate) use peer::scanout_region_contains;

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const IVSHMEM_REGISTERS_BAR: usize = 0;
const IVSHMEM_SHARED_MEMORY_BAR: usize = 2;
const IVSHMEM_DOORBELL_OFFSET: usize = 12;
const GUI_DVM_PIXEL_REGION_BYTES: u64 = driver_domain_protocol::DVM_PIXEL_APERTURE_BYTES;

/// Guest-physical base of the GUI/atlas virtio-pmem. The hypervisor places it in
/// its device-memory region right after guest RAM, so it is derived from the RAM
/// top (the same rule the host launcher applies) rather than assumed.
fn gui_dvm_pixel_region_phys_addr() -> Option<u64> {
    static BASE: AtomicU64 = AtomicU64::new(0);
    // ORDERING: Acquire pairs with the Release store below; the value is a pure
    // function of the boot memory map, so a racing duplicate store is identical.
    let cached = BASE.load(Ordering::Acquire);
    if cached != 0 {
        return Some(cached);
    }
    let base = driver_domain_protocol::dvm_pmem_base(kernel_mm::api::phys::boot_ram_top()?)?;
    // ORDERING: Release publishes the derived base to Acquire readers above.
    BASE.store(base, Ordering::Release);
    Some(base)
}
const GUI_DVM_PEER_ID: u32 = 1;
const GUI_DVM_CONTROL_VECTOR_INDEX: usize = 0;
const GUI_DVM_OFFLINE_VECTOR_INDEX: usize = 1;
const GUI_DVM_MSIX_VECTOR_COUNT: u16 = 2;
const GUI_SLOT_COUNT: usize = DVM_GUI_SURFACE_SLOT_COUNT as usize;
const GUI_SLOT_FREE: u8 = 0;
const GUI_SLOT_WRITING: u8 = 1;
const GUI_SLOT_READY: u8 = 2;
const GPU_SLOT_FREE: u8 = 0;
const GPU_SLOT_WRITING: u8 = 1;
const GPU_SLOT_SUBMITTED: u8 = 2;
const GPU_CONTEXT_ID: u32 = 1;
const GPU_INITIAL_CONTEXT_EPOCH: u32 = 1;
const GPU_PRIME_FENCE_VALUE: u64 = 1;
const MSIX_ENTRY_BYTES: usize = 16;
const MSIX_ENTRY_ADDRESS_LOW_OFFSET: usize = 0;
const MSIX_ENTRY_ADDRESS_HIGH_OFFSET: usize = 4;
const MSIX_ENTRY_DATA_OFFSET: usize = 8;
const MSIX_ENTRY_VECTOR_CONTROL_OFFSET: usize = 12;
const MSIX_ENTRY_VECTOR_MASKED: u32 = 1;

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// A present-path retry and early initialization can overlap. Serialize them
/// so permanent GUI-DVM MSI vectors and provider registration stay one-shot.
static INSTALLING: AtomicBool = AtomicBool::new(false);
/// MSI vectors are permanently reserved once allocated.  A valid pool may
/// appear later, but after exact MSI-X setup (or provider publication) fails,
/// retrying the same topology leaks vectors and can never restore a coherent
/// provider.  Reject that boot deterministically instead.
static INSTALL_REJECTED: AtomicBool = AtomicBool::new(false);
/// The first PCI probe can precede the GUI-DVM peer attaching to the private
/// ivshmem broker. A consumer-triggered retry is bounded and does not create a
/// timer/polling fallback: after this budget is exhausted the display remains
/// unavailable until an explicit new provider lifecycle is introduced.
static INSTALL_RETRY_BUDGET: AtomicU8 = AtomicU8::new(8);
static TRANSPORT_REVOKED: AtomicBool = AtomicBool::new(false);
static SHARED_HEADER_ADDR: AtomicUsize = AtomicUsize::new(0);
static SHARED_PIXEL_ADDR: AtomicUsize = AtomicUsize::new(0);
static SHARED_DOORBELL_ADDR: AtomicUsize = AtomicUsize::new(0);
static POOL_WIDTH: AtomicU32 = AtomicU32::new(0);
static POOL_HEIGHT: AtomicU32 = AtomicU32::new(0);
static POOL_STRIDE_BYTES: AtomicU32 = AtomicU32::new(0);
static POOL_SLOT_BYTES: AtomicU64 = AtomicU64::new(0);
static GPU_COMMAND_OFFSET: AtomicU64 = AtomicU64::new(0);
static GPU_ATLAS_OFFSET: AtomicU64 = AtomicU64::new(0);
static GPU_ATLAS_SLOT_BYTES: AtomicU64 = AtomicU64::new(0);
static GPU_ATLAS_WIDTH: AtomicU32 = AtomicU32::new(0);
static GPU_ATLAS_HEIGHT: AtomicU32 = AtomicU32::new(0);
static GPU_ATLAS_STRIDE_BYTES: AtomicU32 = AtomicU32::new(0);
/// A slot mapping is installed once for the provider lifetime. Repeated
/// compositor surface creation must not grow the MMIO mapper refcount.
static GPU_ATLAS_SLOT_MAP_ADDR: [AtomicUsize; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
static GPU_NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static GPU_CONTEXT_EPOCH: AtomicU32 = AtomicU32::new(GPU_INITIAL_CONTEXT_EPOCH);
static GPU_PRIME_DURATION_NS: AtomicU64 = AtomicU64::new(0);
static GPU_SUBMIT_FLAGS: AtomicU32 = AtomicU32::new(0);
static GPU_SESSION_SUBMISSIONS: AtomicU64 = AtomicU64::new(0);
static GPU_SUBMIT_LOCK: AtomicBool = AtomicBool::new(false);
static GPU_LIFECYCLE: crate::transport_lifecycle::TransportLifecycle =
    crate::transport_lifecycle::TransportLifecycle::detached();
static GPU_EPOCH_RESET_PENDING: AtomicBool = AtomicBool::new(false);
static GPU_SLOT_STATE: [AtomicU8; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU8::new(GPU_SLOT_FREE) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
static GPU_SLOT_GENERATION: [AtomicU64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU64::new(0) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
static GPU_SLOT_SEQUENCE: [AtomicU64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU64::new(0) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
static GPU_SLOT_CONTEXT_ID: [AtomicU32; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU32::new(0) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
static GPU_SLOT_CONTEXT_EPOCH: [AtomicU32; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU32::new(0) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
static GPU_SLOT_SUBMIT_VALUE: [AtomicU64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU64::new(0) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
/// Completion polls that found the slot still pending since its submission.
/// The doorbell is the only DVM wakeup for a submitted slot and an
/// ivshmem interrupt that is coalesced or lands before the DVM handler is
/// armed is never repeated, so a pending poll periodically re-rings it. The
/// DVM consumes invitations idempotently by sequence.
static GPU_SLOT_PENDING_POLLS: [AtomicU32; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [const { AtomicU32::new(0) }; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
const GPU_PENDING_REDOORBELL_POLLS: u32 = 64;
static PUBLISHED_GENERATION: AtomicU64 = AtomicU64::new(0);
static SLOT_STATE: [AtomicU8; GUI_SLOT_COUNT] = [
    AtomicU8::new(GUI_SLOT_FREE),
    AtomicU8::new(GUI_SLOT_FREE),
    AtomicU8::new(GUI_SLOT_FREE),
];
static SLOT_GENERATION: [AtomicU64; GUI_SLOT_COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// Pixel contents survive a validated RELEASE even though release authority
/// does not.  Retaining this separate generation lets a later host writer
/// patch only the declared damage when it reclaims the exact immediately
/// preceding snapshot.  `SLOT_GENERATION` remains the live capability token
/// and is still cleared on release.
static SLOT_CONTENT_GENERATION: [AtomicU64; GUI_SLOT_COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// A different shared-surface backing may contain unrelated pixels at the
/// same geometry.  Such a source always forces a complete snapshot before
/// incremental reuse resumes.
static LAST_SOURCE_PTR: AtomicUsize = AtomicUsize::new(0);
static DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);
/// IRQ callbacks only set these pending bits. All control-record validation and
/// state transitions run at the next normal present boundary.
static GUI_DVM_CONTROL_IRQ_PENDING: AtomicBool = AtomicBool::new(false);
static GUI_DVM_OFFLINE_IRQ_PENDING: AtomicBool = AtomicBool::new(false);
/// Single-consumer admission for the DVM return record.
static GUI_DVM_CONTROL_DRAIN: AtomicBool = AtomicBool::new(false);
static GUI_DVM_PEER_READY: AtomicBool = AtomicBool::new(false);
/// Last invitation whose rebind was reported. Diagnostic only.
static GUI_DVM_LOGGED_INVITATION: AtomicU64 = AtomicU64::new(0);
static GUI_DVM_EXPECTED_INVITATION: AtomicU64 = AtomicU64::new(0);
static GUI_DVM_ACKED_CONTROL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct MappedGuiDvmPool {
    device: crate::arch::pci::PciDevice,
    control: *mut u8,
    pixels: *mut u8,
    doorbell: *mut u8,
    header: DvmGuiSurfacePoolHeader,
    atlas_header: DvmGpuAtlasPoolHeader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GpuAtlasInfo {
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub slot_count: u32,
    pub context_id: u32,
    pub context_epoch: u32,
    pub prime_fence_value: u64,
    pub prime_duration_ns: u64,
    pub submit_flags: u32,
}

/// The display ABI must distinguish a transient saturated pool from a
/// transport failure.  Treating both as `ENODEV` tears down uiserver during a
/// normal burst before the DVM has returned its first slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DvmPresentOutcome {
    Presented,
    Backpressured,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DvmGpuSubmitOutcome {
    Submitted,
    Backpressured,
    Unavailable,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DvmGpuCompletionOutcome {
    Completed,
    Pending,
    Unavailable,
    Invalid,
}

struct GpuSubmitGuard;

impl GpuSubmitGuard {
    fn try_acquire() -> Option<Self> {
        GPU_SUBMIT_LOCK
            // ORDERING: try-lock for the single GPU submitter. The AcqRel CAS acquires the
            // previous holder's Release unlock below, so that holder's writes to the shared
            // command slot and GPU_SLOT_* words are visible; the failure Acquire only
            // reports Busy to the caller.
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for GpuSubmitGuard {
    fn drop(&mut self) {
        // ORDERING: Release unlock publishes everything the submitter wrote (slot
        // state, session counter, shared command bytes) to the next holder's AcqRel CAS.
        GPU_SUBMIT_LOCK.store(false, Ordering::Release);
    }
}

/// Install only the host-created V3 three-slot GUI-DVM pool. The retired
/// single-aperture V2 header is deliberately not recognized, so it cannot be
/// selected as a fallback by PCI enumeration order or an incomplete launch.
pub(crate) fn try_install() -> bool {
    // ORDERING: Acquire loads pair with the Release stores that end installation
    // (INSTALLED, after every aperture/geometry word is stored) and with the Release
    // stores in revoke_transport/on_framebuffer_installed (TRANSPORT_REVOKED).
    if INSTALLED.load(Ordering::Acquire) {
        return !TRANSPORT_REVOKED.load(Ordering::Acquire);
    }
    // ORDERING: sticky refusal bit set with Release in reject_install; Acquire only
    // makes the refusal visible, no other data depends on it.
    if INSTALL_REJECTED.load(Ordering::Acquire) {
        return false;
    }
    if INSTALLING
        // ORDERING: AcqRel CAS is the one-installer admission. Acquire sees the previous
        // installer's Release store of INSTALLING=false (and so its INSTALL_REJECTED and
        // static writes); the failure Acquire only makes this caller bail out.
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let installed = try_install_serialized();
    // ORDERING: Release ends the exclusive install transaction and publishes every
    // write try_install_serialized made to the next admitted installer.
    INSTALLING.store(false, Ordering::Release);
    installed
}

/// Early boot can precede PCI publication. Retry only the explicit V3 pool;
/// absence remains unavailable and never selects an in-kernel or V2 provider.
pub(crate) fn ensure_installed_before_present() {
    // ORDERING: Acquire loads are a retry gate only; try_install re-checks both under
    // the INSTALLING admission, which is the authority.
    if !INSTALLED.load(Ordering::Acquire) && !INSTALL_REJECTED.load(Ordering::Acquire) {
        let retry =
            // ORDERING: bounded retry counter. No data is published through it; the AcqRel
            // is stronger than needed and the INSTALLING CAS in try_install is what
            // serializes installers.
            INSTALL_RETRY_BUDGET.fetch_update(Ordering::AcqRel, Ordering::Acquire, |budget| {
                budget.checked_sub(1)
            });
        if retry.is_ok() {
            let _ = try_install();
        }
    }
}

pub(crate) fn gpu_atlas_info() -> Option<GpuAtlasInfo> {
    drain_dvm_control();
    gpu_atlas_info_snapshot()
}

/// Read the already-published atlas contract without transport discovery or
/// MMIO mutation. Display ioctls use this after their explicit preflight so
/// the per-process handle-table critical section never nests the sleepable
/// MMIO registry.
pub(crate) fn gpu_atlas_info_snapshot() -> Option<GpuAtlasInfo> {
    // ORDERING: Acquire loads pair with the install-time Release stores of INSTALLED,
    // the atlas geometry words and the prime fields (GPU_PRIME_DURATION_NS and
    // GPU_SUBMIT_FLAGS stored before GUI_DVM_PEER_READY in drain_dvm_control). A
    // provider replacement only zeroes words, which the checks below reject.
    if !INSTALLED.load(Ordering::Acquire)
        // ORDERING: Acquire pairs with the Release stores of TRANSPORT_REVOKED (revoke_transport,
        // on_framebuffer_installed) and of GUI_DVM_PEER_READY (drain_dvm_control).
        || TRANSPORT_REVOKED.load(Ordering::Acquire)
        || !GUI_DVM_PEER_READY.load(Ordering::Acquire)
    {
        return None;
    }
    let info = GpuAtlasInfo {
        // ORDERING: Acquire pairs with the install-time Release stores of the atlas geometry;
        // a provider replacement only zeroes these words and the != 0 checks below reject it.
        width: GPU_ATLAS_WIDTH.load(Ordering::Acquire),
        height: GPU_ATLAS_HEIGHT.load(Ordering::Acquire),
        stride_bytes: GPU_ATLAS_STRIDE_BYTES.load(Ordering::Acquire),
        slot_count: DVM_GPU_RENDER_MAX_IN_FLIGHT,
        context_id: GPU_CONTEXT_ID,
        // ORDERING: epoch, prime duration and submit flags pair with the Release stores in service_gpu_epoch_reset and drain_dvm_control.
        context_epoch: GPU_CONTEXT_EPOCH.load(Ordering::Acquire),
        prime_fence_value: GPU_PRIME_FENCE_VALUE,
        prime_duration_ns: GPU_PRIME_DURATION_NS.load(Ordering::Acquire),
        submit_flags: GPU_SUBMIT_FLAGS.load(Ordering::Acquire),
    };
    (info.width != 0
        && info.height != 0
        && info.stride_bytes >= info.width.saturating_mul(4)
        // ORDERING: Acquire pairs with the install-time Release stores of the command/atlas offsets;
        // zero (provider replaced) fails the contract check.
        && GPU_COMMAND_OFFSET.load(Ordering::Acquire) != 0
        && GPU_ATLAS_OFFSET.load(Ordering::Acquire) != 0
        && GPU_ATLAS_SLOT_BYTES.load(Ordering::Acquire) != 0
        && info.context_epoch != 0
        && info.prime_duration_ns != 0
        && matches!(
            info.submit_flags,
            DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY | DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
        ))
    .then_some(info)
}

pub(crate) fn gpu_atlas_slot_mapping(slot: u32) -> Option<(u64, *mut u8, usize)> {
    let info = gpu_atlas_info()?;
    if slot >= info.slot_count {
        return None;
    }
    // ORDERING: Acquire pairs with the install-time Release stores of the atlas slot
    // size/offset (cleared to zero on provider replacement); the geometry is
    // re-bounded against GUI_DVM_PIXEL_REGION_BYTES below.
    let slot_bytes = GPU_ATLAS_SLOT_BYTES.load(Ordering::Acquire);
    let slot_offset = slot_bytes
        .checked_mul(u64::from(slot))
        // ORDERING: Acquire pairs with the install-time Release store of the atlas offset.
        .and_then(|offset| GPU_ATLAS_OFFSET.load(Ordering::Acquire).checked_add(offset))?;
    let phys_start = gui_dvm_pixel_region_phys_addr()?.checked_add(slot_offset)?;
    let len = usize::try_from(slot_bytes).ok()?;
    if len == 0
        || !phys_start.is_multiple_of(4096)
        || !slot_bytes.is_multiple_of(4096)
        || slot_offset
            .checked_add(slot_bytes)
            .is_none_or(|end| end > GUI_DVM_PIXEL_REGION_BYTES)
    {
        return None;
    }
    // ORDERING: Acquire pairs with the AcqRel publication CAS below, so a non-zero
    // cached address implies the mmio mapper already registered that mapping.
    let cached = GPU_ATLAS_SLOT_MAP_ADDR[slot as usize].load(Ordering::Acquire);
    if cached != 0 {
        return Some((phys_start, cached as *mut u8, len));
    }
    let mapping = crate::driver::mmio::map_write_combining(phys_start, len).cast::<u8>();
    if mapping.is_null() {
        return None;
    }
    match GPU_ATLAS_SLOT_MAP_ADDR[slot as usize].compare_exchange(
        0,
        mapping as usize,
        // ORDERING: AcqRel publishes the new slot mapping; the failure Acquire observes
        // the winner's mapping so this caller can unmap its duplicate and use the
        // winner's address.
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => Some((phys_start, mapping, len)),
        Err(winner) => {
            crate::driver::mmio::unmap(mapping.cast());
            Some((phys_start, winner as *mut u8, len))
        }
    }
}

/// Publish one immutable private-compositor atlas snapshot and its bounded
/// command batch. The DVM sees only the host-created slot; neither the user
/// pointer nor a GPU address crosses the domain boundary.
pub(crate) fn try_submit_gpu_atlas(
    surface_token: u64,
    binding_slot: u32,
    width: u32,
    height: u32,
    stride_bytes: u32,
    damage: &[DvmGpuAtlasDamage],
    batch: &[u8],
) -> DvmGpuSubmitOutcome {
    service_gpu_epoch_reset();
    // ORDERING: Acquire gate loads pair with the Release stores of INSTALLED and
    // TRANSPORT_REVOKED; the second TRANSPORT_REVOKED/PEER_READY checks below
    // re-read after drain_dvm_control may have revoked or un-readied the peer.
    if !INSTALLED.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmGpuSubmitOutcome::Unavailable;
    }
    drain_dvm_control();
    // ORDERING: Acquire re-reads revocation and peer readiness after drain_dvm_control; they pair with the Release stores in revoke_transport/drain_dvm_control.
    if TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmGpuSubmitOutcome::Unavailable;
    }
    if !GUI_DVM_PEER_READY.load(Ordering::Acquire) {
        return DvmGpuSubmitOutcome::Backpressured;
    }
    // ORDERING: Acquire selects the installed context epoch before claiming
    // any shared slot; the claim recheck closes concurrent drain/reset.
    let expected_epoch = u64::from(GPU_CONTEXT_EPOCH.load(Ordering::Acquire));
    let Some(lifecycle_claim) = GPU_LIFECYCLE.try_claim(expected_epoch) else {
        return DvmGpuSubmitOutcome::Unavailable;
    };
    let Some(_submit_guard) = GpuSubmitGuard::try_acquire() else {
        return DvmGpuSubmitOutcome::Backpressured;
    };
    let slot = binding_slot as usize;
    // ORDERING: Acquire loads of session counter and atlas geometry are read under
    // the exclusive GpuSubmitGuard and pair with their install/Release stores; the
    // counter is only mutated by the lock holder (and reset by drain after PEER_READY
    // is cleared).
    let initial = GPU_SESSION_SUBMISSIONS.load(Ordering::Acquire) == 0;
    if slot >= GPU_SLOT_STATE.len()
        || surface_token == 0
        // ORDERING: Acquire loads of the atlas geometry pair with the install-time Release stores;
        // the submit must match the geometry the DVM was primed with.
        || width != GPU_ATLAS_WIDTH.load(Ordering::Acquire)
        || height != GPU_ATLAS_HEIGHT.load(Ordering::Acquire)
        || stride_bytes != GPU_ATLAS_STRIDE_BYTES.load(Ordering::Acquire)
        || !dvm_gpu_atlas_damage_is_valid(damage, width, height, initial)
        || batch.len() < DVM_GPU_RENDER_HEADER_BYTES + DVM_GPU_RENDER_SOURCE_BYTES
        || batch.len() > DVM_GPU_RENDER_MAX_BATCH_BYTES
    {
        return DvmGpuSubmitOutcome::Invalid;
    }
    if GPU_SLOT_STATE[slot]
        .compare_exchange(
            GPU_SLOT_FREE,
            GPU_SLOT_WRITING,
            // ORDERING: the AcqRel CAS FREE -> WRITING claims the slot; Acquire pairs with the
            // Release FREE stores (abandon paths and completion retirement), so the prior
            // owner's clears happen-before this submitter's writes. Failure Acquire only
            // reports Backpressured.
            Ordering::AcqRel,
            // ORDERING: failure Acquire only reports Backpressured; the claim is not taken.
            Ordering::Acquire,
        )
        .is_err()
    {
        return DvmGpuSubmitOutcome::Backpressured;
    }

    let parsed = parse_gpu_batch(
        batch,
        surface_token,
        binding_slot,
        width,
        height,
        stride_bytes,
    );
    let Some((header, source)) = parsed else {
        // ORDERING: Release abandons the WRITING claim; nothing was published (state
        // never reached SUBMITTED), and this orders this submitter's earlier writes
        // before the next claimant's AcqRel CAS.
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    };
    if header.context_id != GPU_CONTEXT_ID
        // ORDERING: Acquire pairs with the Release store of the context epoch in
        // service_gpu_epoch_reset/install; a mismatch with the batch header is Invalid.
        || header.context_epoch != GPU_CONTEXT_EPOCH.load(Ordering::Acquire)
    {
        // ORDERING: Release abandon of the WRITING claim; see the first abandon above.
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    }
    let Some(sequence) = next_gpu_sequence() else {
        // ORDERING: Release abandons the claim. The Acquire load only reads the last
        // acknowledged control sequence to echo into the revoke record.
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-submit-sequence-exhausted", control_sequence);
        return DvmGpuSubmitOutcome::Unavailable;
    };
    let Some(next_session_submissions) = GPU_SESSION_SUBMISSIONS
        // ORDERING: Acquire read of the session counter under the submit guard; the
        // value is checked for overflow before it is stored back with Release.
        .load(Ordering::Acquire)
        .checked_add(1)
    else {
        // ORDERING: Release abandons the claim; the Acquire load is the revoke-record echo
        // of the acknowledged control sequence (Release-stored by drain/revoke).
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-session-submit-counter-exhausted", control_sequence);
        return DvmGpuSubmitOutcome::Unavailable;
    };
    // ORDERING: Acquire pairs with the Release store of the prime-proven submit flags
    // (drain_dvm_control); they select the staged/direct path and are validated.
    let submit_flags = GPU_SUBMIT_FLAGS.load(Ordering::Acquire);
    if !matches!(
        submit_flags,
        DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY | DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
    ) {
        // ORDERING: Release abandons the claim (unsupported submit flags).
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Unavailable;
    }
    let submit = DvmGpuAtlasSubmit {
        slot: binding_slot,
        batch_bytes: batch.len() as u32,
        generation: source.generation,
        sequence,
        context_epoch: header.context_epoch,
        flags: submit_flags,
        content_epoch: source.content_epoch,
        damage_count: damage.len() as u32,
    };
    if !submit.matches_batch(header, source) || !publish_gpu_batch(slot, damage, batch, submit) {
        // ORDERING: Release abandons the claim; the shared command bytes already written
        // are never consumed because no invitation was written.
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    }

    // ORDERING: these Release stores write the slot's capability words (generation,
    // sequence, context id/epoch, submit value) while the state is still WRITING.
    // They are consumed by query_gpu_atlas_completion only after it Acquire-loads
    // SLOT_STATE==SUBMITTED, which the AcqRel CAS below publishes after them.
    GPU_SLOT_GENERATION[slot].store(source.generation, Ordering::Release);
    // ORDERING: Release stores of the slot capability words while the state is still WRITING;
    // they become visible to a completion query through the SUBMITTED publication CAS below.
    GPU_SLOT_SEQUENCE[slot].store(sequence, Ordering::Release);
    GPU_SLOT_CONTEXT_ID[slot].store(header.context_id, Ordering::Release);
    GPU_SLOT_CONTEXT_EPOCH[slot].store(header.context_epoch, Ordering::Release);
    // ORDERING: last capability word; Release like the stores above, published by the SUBMITTED CAS.
    GPU_SLOT_SUBMIT_VALUE[slot].store(header.submit_value, Ordering::Release);
    // ORDERING: the AcqRel CAS is the final slot publication and follows all
    // payload/context Release stores under the still-live epoch claim.
    // ORDERING: the AcqRel CAS is the final slot publication and follows all
    // payload/context Release stores under the still-live epoch claim.
    if !lifecycle_claim.validate_current()
        || lifecycle_claim.epoch() != u64::from(header.context_epoch)
        || GPU_SLOT_STATE[slot]
            .compare_exchange(
                GPU_SLOT_WRITING,
                GPU_SLOT_SUBMITTED,
                // ORDERING: AcqRel publishes SUBMITTED after the Release stores above; failure Acquire
                // observes a reset that already freed the slot.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        // ORDERING: Release abandons the claim when the epoch moved or the publication
        // CAS failed because a reset already freed the slot.
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Unavailable;
    }
    let Some(invitation_offset) = dvm_gpu_atlas_invitation_offset(binding_slot) else {
        // ORDERING: Release abandons the claim after SUBMITTED was published but the
        // invitation offset could not be formed (unreachable: slot < MAX_IN_FLIGHT).
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    };
    write_u64(invitation_offset, sequence);
    GPU_SLOT_PENDING_POLLS[slot].store(0, Ordering::Relaxed);
    // ORDERING: Release store under the submit guard; the next guard holder reads
    // it after its AcqRel CAS on the lock, so Release here is belt and braces.
    GPU_SESSION_SUBMISSIONS.store(next_session_submissions, Ordering::Release);
    // ORDERING: SeqCst is a full barrier ordering the invitation and command-slot
    // stores (volatile/plain shared-RAM writes, invisible to Release/Acquire pairs)
    // ahead of the doorbell write in signal_gpu_dvm that makes the DVM read them.
    fence(Ordering::SeqCst);
    signal_gpu_dvm();
    DvmGpuSubmitOutcome::Submitted
}

pub(crate) fn query_gpu_atlas_completion(
    binding_slot: u32,
    result: &mut [u8; driver_domain_protocol::DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES],
) -> DvmGpuCompletionOutcome {
    service_gpu_epoch_reset();
    // ORDERING: Acquire gate loads, as in try_submit_gpu_atlas.
    if !INSTALLED.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmGpuCompletionOutcome::Unavailable;
    }
    drain_dvm_control();
    // ORDERING: Acquire re-reads revocation after drain_dvm_control; pairs with the Release store in revoke_transport.
    if TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmGpuCompletionOutcome::Unavailable;
    }
    // ORDERING: Acquire binds completion inspection to the active epoch before
    // any shared completion bytes are accepted.
    let expected_epoch = u64::from(GPU_CONTEXT_EPOCH.load(Ordering::Acquire));
    let Some(lifecycle_claim) = GPU_LIFECYCLE.try_claim(expected_epoch) else {
        return DvmGpuCompletionOutcome::Unavailable;
    };
    let slot = binding_slot as usize;
    if slot >= GPU_SLOT_STATE.len() {
        return DvmGpuCompletionOutcome::Invalid;
    }
    // ORDERING: Acquire pairs with the AcqRel CAS that publishes SUBMITTED, so the
    // slot's capability words stored before it are visible below.
    match GPU_SLOT_STATE[slot].load(Ordering::Acquire) {
        GPU_SLOT_SUBMITTED => {}
        // FREE here means the caller still holds a ticket for a submission
        // that no longer exists: a DVM offline epoch reset (`reset_gpu_slots`)
        // wiped it while the transport stayed installed and re-activated.
        // Reporting Pending would make the caller wait forever for a
        // completion that can never arrive (uiserver then times out with
        // errno 110 / stalls). Unavailable (ENODEV) tells it the provider was
        // lost so it re-primes a fresh context.
        GPU_SLOT_FREE => return DvmGpuCompletionOutcome::Unavailable,
        _ => return DvmGpuCompletionOutcome::Pending,
    }
    // ORDERING: Acquire pairs with the Release store of the slot sequence at submit.
    let expected_sequence = GPU_SLOT_SEQUENCE[slot].load(Ordering::Acquire);
    let Some(sequence_offset) = dvm_gpu_atlas_completion_sequence_offset(binding_slot) else {
        return DvmGpuCompletionOutcome::Invalid;
    };
    let observed_sequence = read_u64(sequence_offset);
    if observed_sequence == 0 || observed_sequence < expected_sequence {
        let polls = GPU_SLOT_PENDING_POLLS[slot].fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if polls % GPU_PENDING_REDOORBELL_POLLS == 0 {
            signal_gpu_dvm();
        }
        return DvmGpuCompletionOutcome::Pending;
    }
    if observed_sequence > expected_sequence {
        // ORDERING: Acquire read of the acknowledged control sequence, used only to
        // label the revoke record written to the host.
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-completion-sequence-ahead", control_sequence);
        return DvmGpuCompletionOutcome::Unavailable;
    }
    let Some(completion_offset) = dvm_gpu_atlas_completion_offset(binding_slot) else {
        return DvmGpuCompletionOutcome::Invalid;
    };
    let mut bytes = [0_u8; driver_domain_protocol::DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES];
    if read_bytes(completion_offset, &mut bytes).is_none() {
        return DvmGpuCompletionOutcome::Unavailable;
    }
    // ORDERING: the DVM stores the completion body before its sequence word, which
    // this path read first via read_u64. This Acquire fence follows the body read
    // and keeps the validation loads and the ack below from moving above it; the
    // sequence -> body ordering itself rests on x86 TSO (loads are not reordered
    // with older loads) and on volatile reads not being reordered by the compiler.
    // ORDERING: Acquire fence, see the note above: sequence -> body ordering relies on x86 TSO plus volatile program order.
    fence(Ordering::Acquire);
    let Some(completion) = DvmGpuAtlasCompletion::decode(&bytes) else {
        log_malformed_gpu_completion(binding_slot, expected_sequence, &bytes);
        // ORDERING: Acquire echo of the acknowledged control sequence for the revoke
        // record; the malformed completion itself was decoded from the local copy.
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-completion-malformed", control_sequence);
        return DvmGpuCompletionOutcome::Unavailable;
    };
    if completion.slot != binding_slot
        // ORDERING: these Acquire loads pair with the Release stores of the slot
        // capability words at submit; the untrusted completion must match all of
        // them, otherwise the transport is revoked (mismatch is never repaired).
        || completion.generation != GPU_SLOT_GENERATION[slot].load(Ordering::Acquire)
        || completion.sequence != expected_sequence
        // ORDERING: Acquire loads pair with the Release stores of the slot capability words at submit;
        // all must match the untrusted completion or the transport is revoked.
        || completion.render.context_id != GPU_SLOT_CONTEXT_ID[slot].load(Ordering::Acquire)
        || completion.render.context_epoch != GPU_SLOT_CONTEXT_EPOCH[slot].load(Ordering::Acquire)
        || completion.render.submit_value != GPU_SLOT_SUBMIT_VALUE[slot].load(Ordering::Acquire)
    {
        // ORDERING: Acquire echo of the acknowledged control sequence for the revoke record.
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-completion-capability-mismatch", control_sequence);
        return DvmGpuCompletionOutcome::Unavailable;
    }
    // ORDERING: Acquire rechecks the slot's context epoch after shared record
    // validation and before completion acknowledgement.
    if !lifecycle_claim.validate_current()
        || lifecycle_claim.epoch()
            != u64::from(GPU_SLOT_CONTEXT_EPOCH[slot].load(Ordering::Acquire))
    {
        return DvmGpuCompletionOutcome::Unavailable;
    }
    let Some(ack_offset) = dvm_gpu_atlas_completion_ack_offset(binding_slot) else {
        return DvmGpuCompletionOutcome::Invalid;
    };
    // Retirement is single-owner: the surface handle is process-wide, so two
    // threads can query one slot concurrently. Without an exclusive claim the
    // loser would run the clear below after the winner freed the slot and a
    // new submission republished it, wiping that submission's capability words
    // and freeing a slot the DVM still owns. Claiming SUBMITTED -> WRITING
    // makes submitters see the slot busy and other queriers see it pending.
    // ORDERING: AcqRel claims the slot and, on success, observes every Release
    // store a racing submit made before its SUBMITTED publication.
    let claimed = GPU_SLOT_STATE[slot].compare_exchange(
        GPU_SLOT_SUBMITTED,
        GPU_SLOT_WRITING,
        // ORDERING: AcqRel claim; Acquire on failure observes the owner.
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    if claimed.is_err() {
        return DvmGpuCompletionOutcome::Pending;
    }
    // ORDERING: Acquire pairs with the submit's Release sequence store.
    if GPU_SLOT_SEQUENCE[slot].load(Ordering::Acquire) != expected_sequence {
        // The slot was retired and resubmitted between the first state load and
        // this claim; the bytes validated above belong to the old submission.
        // ORDERING: AcqRel hands the claim back; a failed CAS means a reset
        // freed the slot and it must not be resurrected.
        let _ = GPU_SLOT_STATE[slot].compare_exchange(
            GPU_SLOT_WRITING,
            GPU_SLOT_SUBMITTED,
            // ORDERING: AcqRel hand-back; failure only observes a reset.
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        return DvmGpuCompletionOutcome::Pending;
    }
    write_u64(ack_offset, expected_sequence);
    // ORDERING: retirement is under the exclusive SUBMITTED -> WRITING claim. The
    // capability words are cleared first and the Release FREE store last, so a
    // later submitter's AcqRel CAS on FREE sees all of them cleared and cannot have
    // its own freshly stored words wiped by a stale retirer.
    GPU_SLOT_GENERATION[slot].store(0, Ordering::Release);
    // ORDERING: capability words are cleared while this retirer holds the WRITING claim, so no
    // submitter can have stored new words; Release orders them before the FREE store below.
    GPU_SLOT_SEQUENCE[slot].store(0, Ordering::Release);
    GPU_SLOT_CONTEXT_ID[slot].store(0, Ordering::Release);
    GPU_SLOT_CONTEXT_EPOCH[slot].store(0, Ordering::Release);
    // ORDERING: last cleared capability word; the Release FREE store follows.
    GPU_SLOT_SUBMIT_VALUE[slot].store(0, Ordering::Release);
    // ORDERING: Release FREE is last; pairs with the next submitter's AcqRel claim CAS on FREE.
    GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
    *result = bytes;
    DvmGpuCompletionOutcome::Completed
}

/// Names the raw fields of a completion that failed `decode` before the
/// transport is revoked, so a real render failure (non-Completed status) is
/// distinguishable from wire corruption. Offsets follow
/// `DvmGpuAtlasCompletion::encode`: render completion at 64, status at +24.
fn log_malformed_gpu_completion(
    slot: u32,
    expected_sequence: u64,
    bytes: &[u8; driver_domain_protocol::DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES],
) {
    let u32_at = |offset: usize| {
        u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    };
    let u64_at = |offset: usize| {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&bytes[offset..offset + 8]);
        u64::from_le_bytes(raw)
    };
    crate::debug::warn!(
        display,
        "gui-dvm: malformed gpu completion slot={} expected_seq={} magic_ok={} seq={} gen={} flags={:#x} render_status={} ctx={}/{} submit={} completion={} release={} present_value={}",
        slot,
        expected_sequence,
        bytes[0..8] == driver_domain_protocol::DVM_GPU_ATLAS_COMPLETION_MAGIC,
        u64_at(32),
        u64_at(24),
        u32_at(20),
        u32_at(64 + 24),
        u32_at(64 + 16),
        u32_at(64 + 20),
        u64_at(64 + 32),
        u64_at(64 + 40),
        u64_at(64 + 56),
        u64_at(128 + 40),
    );
}

fn parse_gpu_batch(
    batch: &[u8],
    surface_token: u64,
    binding_slot: u32,
    width: u32,
    height: u32,
    stride_bytes: u32,
) -> Option<(DvmGpuRenderBatchHeader, DvmGpuRenderSource)> {
    let header_bytes: [u8; DVM_GPU_RENDER_HEADER_BYTES] =
        batch.get(..DVM_GPU_RENDER_HEADER_BYTES)?.try_into().ok()?;
    let header = DvmGpuRenderBatchHeader::decode(&header_bytes)?;
    if header.source_count != 1 || header.encoded_batch_len()? != batch.len() {
        return None;
    }
    let source_end = DVM_GPU_RENDER_HEADER_BYTES.checked_add(DVM_GPU_RENDER_SOURCE_BYTES)?;
    let source_bytes: [u8; DVM_GPU_RENDER_SOURCE_BYTES] = batch
        .get(DVM_GPU_RENDER_HEADER_BYTES..source_end)?
        .try_into()
        .ok()?;
    let source = DvmGpuRenderSource::decode(&source_bytes)?;
    (source.token == surface_token
        && source.binding_slot == binding_slot
        && source.width == width
        && source.height == height
        && source.stride_bytes == stride_bytes)
        .then_some((header, source))
}

fn next_gpu_sequence() -> Option<u64> {
    // ORDERING: monotonic sequence allocator; callers hold the GpuSubmitGuard, so the CAS only
    // needs atomicity for uniqueness. Acquire/AcqRel are stronger than needed and publish no data.
    let mut current = GPU_NEXT_SEQUENCE.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1)?;
        match GPU_NEXT_SEQUENCE.compare_exchange_weak(
            current,
            next,
            // ORDERING: AcqRel CAS (stronger than needed, see above); failure Acquire just reloads.
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(next),
            Err(observed) => current = observed,
        }
    }
}

fn publish_gpu_batch(
    slot: usize,
    damage: &[DvmGpuAtlasDamage],
    batch: &[u8],
    submit: DvmGpuAtlasSubmit,
) -> bool {
    // ORDERING: Acquire loads pair with the install-time Release stores of the pool/atlas addresses and
    // offsets. Zero (replaced provider) is rejected below and all offsets are bounded by the fixed region size.
    let pixels = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    let atlas_base = GPU_ATLAS_OFFSET.load(Ordering::Acquire);
    let atlas_slot_bytes = GPU_ATLAS_SLOT_BYTES.load(Ordering::Acquire);
    // ORDERING: Acquire pairs with the install-time Release store of the command offset.
    let command_base = GPU_COMMAND_OFFSET.load(Ordering::Acquire);
    let Some(atlas_offset) = atlas_slot_bytes
        .checked_mul(slot as u64)
        .and_then(|offset| atlas_base.checked_add(offset))
    else {
        return false;
    };
    let Some(command_offset) = DVM_GPU_ATLAS_COMMAND_SLOT_BYTES
        .checked_mul(slot as u64)
        .and_then(|offset| command_base.checked_add(offset))
    else {
        return false;
    };
    let Some(damage_offset) = command_offset.checked_add(DVM_GPU_ATLAS_SUBMIT_BYTES as u64) else {
        return false;
    };
    let Some(damage_bytes) = (damage.len() as u64).checked_mul(DVM_GPU_ATLAS_DAMAGE_BYTES as u64)
    else {
        return false;
    };
    let Some(batch_offset) = damage_offset.checked_add(damage_bytes) else {
        return false;
    };
    let Some(batch_end) = batch_offset.checked_add(batch.len() as u64) else {
        return false;
    };
    let Some(command_end) = command_offset.checked_add(DVM_GPU_ATLAS_COMMAND_SLOT_BYTES) else {
        return false;
    };
    let region_bytes = GUI_DVM_PIXEL_REGION_BYTES;
    if pixels == 0
        || atlas_slot_bytes == 0
        || atlas_offset
            .checked_add(atlas_slot_bytes)
            .is_none_or(|end| end > region_bytes)
        || batch_end > region_bytes
        || batch_end > command_end
    {
        return false;
    }
    // `pixels` is the higher-half mapping of the reserved GUI_DVM_PIXEL_REGION_BYTES
    // pixel region, never unmapped after install; every offset below uses checked
    // arithmetic and batch_end <= command_end <= region bounds was verified above.
    // SAFETY: the pointers stay inside that region; this submitter owns the slot's
    // command area (state WRITING via its CAS) and the sources are distinct kernel buffers.
    unsafe {
        for (index, rect) in damage.iter().copied().enumerate() {
            let encoded = rect.encode();
            ptr::copy_nonoverlapping(
                encoded.as_ptr(),
                (pixels as *mut u8)
                    .add(damage_offset as usize + index * DVM_GPU_ATLAS_DAMAGE_BYTES),
                encoded.len(),
            );
        }
        ptr::copy_nonoverlapping(
            batch.as_ptr(),
            (pixels as *mut u8).add(batch_offset as usize),
            batch.len(),
        );
        let encoded = submit.encode();
        ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            (pixels as *mut u8).add(command_offset as usize),
            encoded.len(),
        );
    }
    // ORDERING: SeqCst fence orders the plain command/damage/batch copies ahead of the later SUBMITTED publication and invitation write.
    fence(Ordering::SeqCst);
    true
}

fn signal_gpu_dvm() {
    // ORDERING: Acquire loads pair with the Release store of the doorbell address at install and the
    // Release store of PEER_READY in drain_dvm_control; the doorbell is only rung for a ready peer.
    let doorbell = SHARED_DOORBELL_ADDR.load(Ordering::Acquire);
    if doorbell == 0 || !GUI_DVM_PEER_READY.load(Ordering::Acquire) {
        return;
    }
    // ORDERING: SeqCst fence orders every earlier shared-memory store ahead of the doorbell MMIO write below.
    fence(Ordering::SeqCst);
    let value = (GUI_DVM_PEER_ID << 16).to_le();
    // SAFETY: `doorbell` is the non-zero BAR0 register mapping published at install (never unmapped);
    // find_ivshmem_gui_pool checked registers.size covers the 4-byte write at IVSHMEM_DOORBELL_OFFSET,
    // and the mapping base is page aligned so the u32 access is aligned MMIO.
    unsafe {
        (doorbell as *mut u8)
            .add(IVSHMEM_DOORBELL_OFFSET)
            .cast::<u32>()
            .write_volatile(value);
    }
}

/// Copy a complete frame into one host-owned free slot.  A missing GUI-DVM
/// transport is unavailable: normal presentation never selects a weaker
/// framebuffer provider.
pub(crate) fn try_publish_full(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
) -> DvmPresentOutcome {
    publish_frame(
        src_ptr,
        width,
        height,
        stride_bytes,
        DvmDisplayDamage::full(),
    )
}

pub(crate) fn try_publish_rect(
    frame: crate::io::gui::KernelBgraFrame,
    rect: crate::io::gui::GuiDamageRect,
) -> DvmPresentOutcome {
    publish_frame(
        frame.src_ptr,
        frame.width,
        frame.height,
        frame.stride_bytes,
        DvmDisplayDamage::rect(
            rect.x as u32,
            rect.y as u32,
            rect.width as u32,
            rect.height as u32,
        ),
    )
}

fn publish_frame(
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    damage: DvmDisplayDamage,
) -> DvmPresentOutcome {
    // ORDERING: Acquire pairs with the Release store of INSTALLED that ends installation.
    if !INSTALLED.load(Ordering::Acquire) {
        return DvmPresentOutcome::Unavailable;
    }
    drain_dvm_control();
    // ORDERING: Acquire re-reads revocation after the control drain, then loads the single-writer
    // generation published by the AcqRel CAS below and the acknowledged control sequence (echoed into revoke records).
    if TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmPresentOutcome::Unavailable;
    }
    // ORDERING: Acquire; PUBLISHED_GENERATION has a single host writer (see the CAS below).
    let preferred_generation = PUBLISHED_GENERATION.load(Ordering::Acquire);
    let Some(generation) = preferred_generation.checked_add(2) else {
        // ORDERING: Acquire pairs with the Release stores of the acknowledged sequence in drain_dvm_control/revoke_transport.
        let sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("host-generation-exhausted", sequence);
        return DvmPresentOutcome::Unavailable;
    };
    let Some(slot) = reserve_free_slot(preferred_generation) else {
        // A relay can restart after all three slots were committed.  There is
        // then no new frame to create an invitation, so a bounded producer
        // must re-advertise the newest extant READY slot exactly once after an
        // offline transition.  This is recovery of the fixed pool, not a
        // polling or alternate-provider fallback.
        reinvite_newest_ready_slot();
        DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
        return DvmPresentOutcome::Backpressured;
    };
    let copied = copy_into_slot(
        slot,
        src_ptr,
        width,
        height,
        stride_bytes,
        damage,
        preferred_generation,
    );
    if !copied {
        // ORDERING: Release abandons the reserved slot (content generation cleared first); pairs with the AcqRel claim CAS in reserve_free_slot.
        SLOT_CONTENT_GENERATION[slot].store(0, Ordering::Release);
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        return DvmPresentOutcome::Unavailable;
    }
    let message = DvmGuiSurfaceMessage::present(slot as u32, generation, damage);
    if !message.is_valid_for_dimensions(
        // ORDERING: Acquire loads pair with the install-time Release stores of the pool dimensions.
        POOL_WIDTH.load(Ordering::Acquire),
        POOL_HEIGHT.load(Ordering::Acquire),
    ) {
        // ORDERING: Release abandons the reserved slot, as above.
        SLOT_CONTENT_GENERATION[slot].store(0, Ordering::Release);
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        warn_rejected("host-present-record-invalid");
        return DvmPresentOutcome::Unavailable;
    }
    if PUBLISHED_GENERATION
        .compare_exchange(
            preferred_generation,
            generation,
            // ORDERING: AcqRel CAS detects a second host writer: only the writer whose preferred generation
            // is still current succeeds; failure Acquire observes the winner.
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        // The display backend contract permits one host snapshot writer. If
        // that invariant is ever violated, this slot may have been patched
        // from a non-predecessor base and must never be published or reused.
        // ORDERING: Release abandons a slot patched from a non-predecessor base; clearing the
        // content generation first prevents reuse as an incremental base. Acquire reads the echo sequence.
        SLOT_CONTENT_GENERATION[slot].store(0, Ordering::Release);
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        let sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("concurrent-host-writer", sequence);
        return DvmPresentOutcome::Unavailable;
    }
    write_host_record(slot, message);
    // ORDERING: Release stores publish the slot, ending with READY. The DVM-visible host record was
    // written (SeqCst fence in write_bytes) before them; drain_dvm_control Acquire-loads READY then the generation.
    SLOT_CONTENT_GENERATION[slot].store(generation, Ordering::Release);
    LAST_SOURCE_PTR.store(src_ptr as usize, Ordering::Release);
    SLOT_GENERATION[slot].store(generation, Ordering::Release);
    // ORDERING: READY is the slot's final Release publication (see the group above).
    SLOT_STATE[slot].store(GUI_SLOT_READY, Ordering::Release);
    signal_gui_dvm(generation);
    DvmPresentOutcome::Presented
}

fn reserve_free_slot(preferred_generation: u64) -> Option<usize> {
    if preferred_generation != 0 {
        for (slot, state) in SLOT_STATE.iter().enumerate() {
            // ORDERING: Acquire reads the retained content generation; pairs with the Release store at publish.
            if SLOT_CONTENT_GENERATION[slot].load(Ordering::Acquire) == preferred_generation
                && state
                    .compare_exchange(
                        GUI_SLOT_FREE,
                        GUI_SLOT_WRITING,
                        // ORDERING: AcqRel claim FREE -> WRITING; Acquire pairs with the Release FREE store that freed the slot
                        // (drain_dvm_control release or abandon), so earlier DVM-release bookkeeping is visible.
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                return Some(slot);
            }
        }
    }
    for (slot, state) in SLOT_STATE.iter().enumerate() {
        if state
            .compare_exchange(
                GUI_SLOT_FREE,
                GUI_SLOT_WRITING,
                // ORDERING: AcqRel claim of any free slot, same pairing as above.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            return Some(slot);
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SnapshotCopyPlan {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    incremental: bool,
}

fn snapshot_copy_plan(
    damage: DvmDisplayDamage,
    width: usize,
    height: usize,
    slot_content_generation: u64,
    previous_generation: u64,
    source_matches: bool,
) -> Option<SnapshotCopyPlan> {
    let damage_bounds = damage_bounds(damage, width, height)?;
    let incremental = damage.flags != driver_domain_protocol::DVM_DISPLAY_DAMAGE_FULL
        && source_matches
        && previous_generation != 0
        && slot_content_generation == previous_generation;
    let (x, y, copy_width, copy_height) = if incremental {
        damage_bounds
    } else {
        (0, 0, width, height)
    };
    Some(SnapshotCopyPlan {
        x,
        y,
        width: copy_width,
        height: copy_height,
        incremental,
    })
}

fn contiguous_snapshot_copy_len(
    plan: SnapshotCopyPlan,
    surface_width: usize,
    stride_bytes: usize,
) -> Option<usize> {
    if plan.x != 0 || plan.width != surface_width {
        return None;
    }
    plan.height.checked_mul(stride_bytes)
}

fn copy_into_slot(
    slot: usize,
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    damage: DvmDisplayDamage,
    previous_generation: u64,
) -> bool {
    if src_ptr.is_null()
        // ORDERING: Acquire loads pair with the install-time Release stores of the pool geometry.
        || width != POOL_WIDTH.load(Ordering::Acquire) as usize
        || height != POOL_HEIGHT.load(Ordering::Acquire) as usize
        || stride_bytes != POOL_STRIDE_BYTES.load(Ordering::Acquire) as usize
        || !damage.is_valid_for_dimensions(width as u32, height as u32)
    {
        warn_rejected("frame-geometry-or-damage-invalid");
        return false;
    }
    let Some(slot_offset) = slot_offset(slot) else {
        warn_rejected("slot-offset-invalid");
        return false;
    };
    // ORDERING: Acquire pairs with the Release store of LAST_SOURCE_PTR at publish; it only selects incremental vs full copy.
    let source_matches = LAST_SOURCE_PTR.load(Ordering::Acquire) == src_ptr as usize;
    let Some(plan) = snapshot_copy_plan(
        damage,
        width,
        height,
        // ORDERING: Acquire pairs with the Release store of the slot content generation at publish or clear.
        SLOT_CONTENT_GENERATION[slot].load(Ordering::Acquire),
        previous_generation,
        source_matches,
    ) else {
        warn_rejected("damage-bounds-invalid");
        return false;
    };
    // Every published slot is a self-contained immutable frame snapshot.
    // A partial copy is valid only when this free slot still contains the
    // exact immediately preceding snapshot from the same source mapping.
    // Stale/uninitialized slots and replacement surfaces always receive a
    // complete copy, so damage never creates a dependency on unknown bytes.
    let SnapshotCopyPlan {
        x,
        y,
        width: copy_width,
        height: copy_height,
        incremental: _,
    } = plan;
    let bytes_per_pixel = 4_usize;
    let Some(row_bytes) = copy_width.checked_mul(bytes_per_pixel) else {
        return false;
    };
    let Some(row_offset) = y.checked_mul(stride_bytes).and_then(|offset| {
        x.checked_mul(bytes_per_pixel)
            .and_then(|x_offset| offset.checked_add(x_offset))
    }) else {
        return false;
    };
    let Some(last_row) = copy_height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(stride_bytes))
    else {
        return false;
    };
    let Some(end) = row_offset
        .checked_add(last_row)
        .and_then(|offset| offset.checked_add(row_bytes))
    else {
        return false;
    };
    let contiguous_copy_len = contiguous_snapshot_copy_len(plan, width, stride_bytes);
    let Some(copy_end) = contiguous_copy_len
        .map(|copy_len| row_offset.checked_add(copy_len))
        .unwrap_or(Some(end))
    else {
        return false;
    };
    // ORDERING: Acquire pairs with the install-time Release store of the slot size.
    if copy_end > POOL_SLOT_BYTES.load(Ordering::Acquire) as usize {
        warn_rejected("copy-exceeds-slot");
        return false;
    }
    // ORDERING: Acquire pairs with the install-time Release store of the pixel mapping address.
    let pixels = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    if pixels == 0 {
        return false;
    }
    // SAFETY: `source` stays inside the caller's frame (KernelBgraFrame contract, geometry equal to the
    // pool geometry checked above, extent bounded by `end`). The destination lies inside this slot:
    // copy_end <= POOL_SLOT_BYTES and slot_offset() is bounded by header_fits_resource at install. The slot is
    // exclusively WRITING-owned by this publisher, and the reserved pool is outside the allocator, so no overlap.
    unsafe {
        let source = src_ptr.add(row_offset);
        let destination = (pixels as *mut u8).add(slot_offset + row_offset);
        if let Some(copy_len) = contiguous_copy_len {
            // Full-width snapshots are contiguous in both mappings. One bulk
            // copy avoids a syscall-scale memcpy setup for every scanline,
            // which is especially costly on the first cold QEMU frame.
            ptr::copy_nonoverlapping(source, destination, copy_len);
        } else {
            let mut source = source;
            let mut destination = destination;
            for _ in 0..copy_height {
                // The pixel pool is reserved WB memory; the ivshmem BAR carries
                // control records only and is never used as a bulk-copy target.
                ptr::copy_nonoverlapping(source, destination, row_bytes);
                source = source.add(stride_bytes);
                destination = destination.add(stride_bytes);
            }
        }
    }
    // ORDERING: SeqCst fence orders the pixel copy ahead of the slot publication and invitation the DVM acts on.
    fence(Ordering::SeqCst);
    true
}

fn damage_bounds(
    damage: DvmDisplayDamage,
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if damage.flags == driver_domain_protocol::DVM_DISPLAY_DAMAGE_FULL {
        return Some((0, 0, width, height));
    }
    let x = damage.x as usize;
    let y = damage.y as usize;
    let rect_width = damage.width as usize;
    let rect_height = damage.height as usize;
    (rect_width != 0
        && rect_height != 0
        && x.checked_add(rect_width)? <= width
        && y.checked_add(rect_height)? <= height)
        .then_some((x, y, rect_width, rect_height))
}

/// A replacement provider must not leave an installed GUI-DVM pool reachable.
pub(crate) fn on_framebuffer_installed(framebuffer_addr: u64) {
    // ORDERING: Acquire pairs with the install-time Release store of the pixel mapping address.
    let pixel_addr = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    let Some(first_slot) = slot_offset(0) else {
        return;
    };
    if pixel_addr != 0 && framebuffer_addr != pixel_addr as u64 + first_slot as u64 {
        // ORDERING: teardown. Release stores zero the addresses and geometry before INSTALLED/REVOKED flip, so
        // a reader that Acquires either flag sees zeros and bails. Mappings are never unmapped, so a pointer
        // loaded earlier by a racing reader stays valid memory.
        SHARED_HEADER_ADDR.store(0, Ordering::Release);
        SHARED_PIXEL_ADDR.store(0, Ordering::Release);
        // ORDERING: Release zeroing; see the teardown note above.
        SHARED_DOORBELL_ADDR.store(0, Ordering::Release);
        // ORDERING: Release zeroing; see the teardown note above.
        GPU_COMMAND_OFFSET.store(0, Ordering::Release);
        // ORDERING: continuation of the same teardown sequence, ordered after the zeroed addresses.
        GPU_ATLAS_OFFSET.store(0, Ordering::Release);
        GPU_ATLAS_SLOT_BYTES.store(0, Ordering::Release);
        GPU_ATLAS_WIDTH.store(0, Ordering::Release);
        GPU_ATLAS_HEIGHT.store(0, Ordering::Release);
        // ORDERING: INSTALLED=false then TRANSPORT_REVOKED=true; Release makes the zeroed words visible first.
        GPU_ATLAS_STRIDE_BYTES.store(0, Ordering::Release);
        INSTALLED.store(false, Ordering::Release);
        TRANSPORT_REVOKED.store(true, Ordering::Release);
        // `reset_gpu_slots` asserts that no submit/query claim is in flight, and
        // a concurrent ioctl may legitimately hold one while the provider is
        // replaced. Skip the reset then instead of panicking: both flags above
        // already fail every later submit/query, and a reinstall reinitializes
        // every slot, so no stale slot state can survive.
        if GPU_LIFECYCLE.in_flight() == 0 {
            reset_gpu_slots();
        }
    }
}

/// MSI-X leaf callbacks: allocation, logging, composition, and protocol
/// parsing are forbidden here. The next normal present drains the fixed record.
fn gui_dvm_control_interrupt(_vector: u8) {
    // ORDERING: IRQ leaf: Release sets the pending bit; drain_dvm_control consumes it with an AcqRel swap in normal context.
    GUI_DVM_CONTROL_IRQ_PENDING.store(true, Ordering::Release);
}

fn gui_dvm_offline_interrupt(_vector: u8) {
    // ORDERING: IRQ leaf: Release sets the offline pending bit; consumed by the AcqRel swap in drain_dvm_control.
    GUI_DVM_OFFLINE_IRQ_PENDING.store(true, Ordering::Release);
}

#[cfg(test)]
#[path = "display_transport_tests.rs"]
mod tests;
// RING3-MIGRATION-REFERENCE END: GUI-DVM display transport substrate.
