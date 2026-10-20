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
const GUI_DVM_PIXEL_REGION_PHYS_ADDR: u64 = 0x1_0000_0000;
const GUI_DVM_PIXEL_REGION_BYTES: u64 = 128 * 1024 * 1024;
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
static GPU_SLOT_STATE: [AtomicU8; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] = [
    AtomicU8::new(GPU_SLOT_FREE),
    AtomicU8::new(GPU_SLOT_FREE),
    AtomicU8::new(GPU_SLOT_FREE),
];
static GPU_SLOT_GENERATION: [AtomicU64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static GPU_SLOT_SEQUENCE: [AtomicU64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static GPU_SLOT_CONTEXT_ID: [AtomicU32; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static GPU_SLOT_CONTEXT_EPOCH: [AtomicU32; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static GPU_SLOT_SUBMIT_VALUE: [AtomicU64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
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
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for GpuSubmitGuard {
    fn drop(&mut self) {
        GPU_SUBMIT_LOCK.store(false, Ordering::Release);
    }
}

/// Install only the host-created V3 three-slot GUI-DVM pool. The retired
/// single-aperture V2 header is deliberately not recognized, so it cannot be
/// selected as a fallback by PCI enumeration order or an incomplete launch.
pub(crate) fn try_install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return !TRANSPORT_REVOKED.load(Ordering::Acquire);
    }
    if INSTALL_REJECTED.load(Ordering::Acquire) {
        return false;
    }
    if INSTALLING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let installed = try_install_serialized();
    INSTALLING.store(false, Ordering::Release);
    installed
}

/// Early boot can precede PCI publication. Retry only the explicit V3 pool;
/// absence remains unavailable and never selects an in-kernel or V2 provider.
pub(crate) fn ensure_installed_before_present() {
    if !INSTALLED.load(Ordering::Acquire) && !INSTALL_REJECTED.load(Ordering::Acquire) {
        let retry =
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
    if !INSTALLED.load(Ordering::Acquire)
        || TRANSPORT_REVOKED.load(Ordering::Acquire)
        || !GUI_DVM_PEER_READY.load(Ordering::Acquire)
    {
        return None;
    }
    let info = GpuAtlasInfo {
        width: GPU_ATLAS_WIDTH.load(Ordering::Acquire),
        height: GPU_ATLAS_HEIGHT.load(Ordering::Acquire),
        stride_bytes: GPU_ATLAS_STRIDE_BYTES.load(Ordering::Acquire),
        slot_count: DVM_GPU_RENDER_MAX_IN_FLIGHT,
        context_id: GPU_CONTEXT_ID,
        context_epoch: GPU_CONTEXT_EPOCH.load(Ordering::Acquire),
        prime_fence_value: GPU_PRIME_FENCE_VALUE,
        prime_duration_ns: GPU_PRIME_DURATION_NS.load(Ordering::Acquire),
        submit_flags: GPU_SUBMIT_FLAGS.load(Ordering::Acquire),
    };
    (info.width != 0
        && info.height != 0
        && info.stride_bytes >= info.width.saturating_mul(4)
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
    let slot_bytes = GPU_ATLAS_SLOT_BYTES.load(Ordering::Acquire);
    let slot_offset = slot_bytes
        .checked_mul(u64::from(slot))
        .and_then(|offset| GPU_ATLAS_OFFSET.load(Ordering::Acquire).checked_add(offset))?;
    let phys_start = GUI_DVM_PIXEL_REGION_PHYS_ADDR.checked_add(slot_offset)?;
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
    if !INSTALLED.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmGpuSubmitOutcome::Unavailable;
    }
    drain_dvm_control();
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
    let initial = GPU_SESSION_SUBMISSIONS.load(Ordering::Acquire) == 0;
    if slot >= GPU_SLOT_STATE.len()
        || surface_token == 0
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
            Ordering::AcqRel,
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
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    };
    if header.context_id != GPU_CONTEXT_ID
        || header.context_epoch != GPU_CONTEXT_EPOCH.load(Ordering::Acquire)
    {
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    }
    let Some(sequence) = next_gpu_sequence() else {
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-submit-sequence-exhausted", control_sequence);
        return DvmGpuSubmitOutcome::Unavailable;
    };
    let Some(next_session_submissions) = GPU_SESSION_SUBMISSIONS
        .load(Ordering::Acquire)
        .checked_add(1)
    else {
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-session-submit-counter-exhausted", control_sequence);
        return DvmGpuSubmitOutcome::Unavailable;
    };
    let submit_flags = GPU_SUBMIT_FLAGS.load(Ordering::Acquire);
    if !matches!(
        submit_flags,
        DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY | DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
    ) {
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
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    }

    GPU_SLOT_GENERATION[slot].store(source.generation, Ordering::Release);
    GPU_SLOT_SEQUENCE[slot].store(sequence, Ordering::Release);
    GPU_SLOT_CONTEXT_ID[slot].store(header.context_id, Ordering::Release);
    GPU_SLOT_CONTEXT_EPOCH[slot].store(header.context_epoch, Ordering::Release);
    GPU_SLOT_SUBMIT_VALUE[slot].store(header.submit_value, Ordering::Release);
    // ORDERING: the AcqRel CAS is the final slot publication and follows all
    // payload/context Release stores under the still-live epoch claim.
    if !lifecycle_claim.validate_current()
        || lifecycle_claim.epoch() != u64::from(header.context_epoch)
        || GPU_SLOT_STATE[slot]
            .compare_exchange(
                GPU_SLOT_WRITING,
                GPU_SLOT_SUBMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Unavailable;
    }
    let Some(invitation_offset) = dvm_gpu_atlas_invitation_offset(binding_slot) else {
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        return DvmGpuSubmitOutcome::Invalid;
    };
    write_u64(invitation_offset, sequence);
    GPU_SESSION_SUBMISSIONS.store(next_session_submissions, Ordering::Release);
    fence(Ordering::SeqCst);
    signal_gpu_dvm();
    DvmGpuSubmitOutcome::Submitted
}

pub(crate) fn query_gpu_atlas_completion(
    binding_slot: u32,
    result: &mut [u8; driver_domain_protocol::DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES],
) -> DvmGpuCompletionOutcome {
    service_gpu_epoch_reset();
    if !INSTALLED.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmGpuCompletionOutcome::Unavailable;
    }
    drain_dvm_control();
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
    if GPU_SLOT_STATE[slot].load(Ordering::Acquire) != GPU_SLOT_SUBMITTED {
        return DvmGpuCompletionOutcome::Pending;
    }
    let expected_sequence = GPU_SLOT_SEQUENCE[slot].load(Ordering::Acquire);
    let Some(sequence_offset) = dvm_gpu_atlas_completion_sequence_offset(binding_slot) else {
        return DvmGpuCompletionOutcome::Invalid;
    };
    let observed_sequence = read_u64(sequence_offset);
    if observed_sequence == 0 || observed_sequence < expected_sequence {
        return DvmGpuCompletionOutcome::Pending;
    }
    if observed_sequence > expected_sequence {
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
    fence(Ordering::Acquire);
    let Some(completion) = DvmGpuAtlasCompletion::decode(&bytes) else {
        let control_sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("gpu-completion-malformed", control_sequence);
        return DvmGpuCompletionOutcome::Unavailable;
    };
    if completion.slot != binding_slot
        || completion.generation != GPU_SLOT_GENERATION[slot].load(Ordering::Acquire)
        || completion.sequence != expected_sequence
        || completion.render.context_id != GPU_SLOT_CONTEXT_ID[slot].load(Ordering::Acquire)
        || completion.render.context_epoch != GPU_SLOT_CONTEXT_EPOCH[slot].load(Ordering::Acquire)
        || completion.render.submit_value != GPU_SLOT_SUBMIT_VALUE[slot].load(Ordering::Acquire)
    {
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
    write_u64(ack_offset, expected_sequence);
    GPU_SLOT_GENERATION[slot].store(0, Ordering::Release);
    GPU_SLOT_SEQUENCE[slot].store(0, Ordering::Release);
    GPU_SLOT_CONTEXT_ID[slot].store(0, Ordering::Release);
    GPU_SLOT_CONTEXT_EPOCH[slot].store(0, Ordering::Release);
    GPU_SLOT_SUBMIT_VALUE[slot].store(0, Ordering::Release);
    GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
    *result = bytes;
    DvmGpuCompletionOutcome::Completed
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
    let mut current = GPU_NEXT_SEQUENCE.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1)?;
        match GPU_NEXT_SEQUENCE.compare_exchange_weak(
            current,
            next,
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
    let pixels = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    let atlas_base = GPU_ATLAS_OFFSET.load(Ordering::Acquire);
    let atlas_slot_bytes = GPU_ATLAS_SLOT_BYTES.load(Ordering::Acquire);
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
    fence(Ordering::SeqCst);
    true
}

fn signal_gpu_dvm() {
    let doorbell = SHARED_DOORBELL_ADDR.load(Ordering::Acquire);
    if doorbell == 0 || !GUI_DVM_PEER_READY.load(Ordering::Acquire) {
        return;
    }
    fence(Ordering::SeqCst);
    let value = (GUI_DVM_PEER_ID << 16).to_le();
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
    if !INSTALLED.load(Ordering::Acquire) {
        return DvmPresentOutcome::Unavailable;
    }
    drain_dvm_control();
    if TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return DvmPresentOutcome::Unavailable;
    }
    let preferred_generation = PUBLISHED_GENERATION.load(Ordering::Acquire);
    let Some(generation) = preferred_generation.checked_add(2) else {
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
        SLOT_CONTENT_GENERATION[slot].store(0, Ordering::Release);
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        return DvmPresentOutcome::Unavailable;
    }
    let message = DvmGuiSurfaceMessage::present(slot as u32, generation, damage);
    if !message.is_valid_for_dimensions(
        POOL_WIDTH.load(Ordering::Acquire),
        POOL_HEIGHT.load(Ordering::Acquire),
    ) {
        SLOT_CONTENT_GENERATION[slot].store(0, Ordering::Release);
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        warn_rejected("host-present-record-invalid");
        return DvmPresentOutcome::Unavailable;
    }
    if PUBLISHED_GENERATION
        .compare_exchange(
            preferred_generation,
            generation,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        // The display backend contract permits one host snapshot writer. If
        // that invariant is ever violated, this slot may have been patched
        // from a non-predecessor base and must never be published or reused.
        SLOT_CONTENT_GENERATION[slot].store(0, Ordering::Release);
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        let sequence = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
        revoke_transport("concurrent-host-writer", sequence);
        return DvmPresentOutcome::Unavailable;
    }
    write_host_record(slot, message);
    SLOT_CONTENT_GENERATION[slot].store(generation, Ordering::Release);
    LAST_SOURCE_PTR.store(src_ptr as usize, Ordering::Release);
    SLOT_GENERATION[slot].store(generation, Ordering::Release);
    SLOT_STATE[slot].store(GUI_SLOT_READY, Ordering::Release);
    signal_gui_dvm(generation);
    DvmPresentOutcome::Presented
}

fn reserve_free_slot(preferred_generation: u64) -> Option<usize> {
    if preferred_generation != 0 {
        for (slot, state) in SLOT_STATE.iter().enumerate() {
            if SLOT_CONTENT_GENERATION[slot].load(Ordering::Acquire) == preferred_generation
                && state
                    .compare_exchange(
                        GUI_SLOT_FREE,
                        GUI_SLOT_WRITING,
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
    let source_matches = LAST_SOURCE_PTR.load(Ordering::Acquire) == src_ptr as usize;
    let Some(plan) = snapshot_copy_plan(
        damage,
        width,
        height,
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
    if copy_end > POOL_SLOT_BYTES.load(Ordering::Acquire) as usize {
        warn_rejected("copy-exceeds-slot");
        return false;
    }
    let pixels = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    if pixels == 0 {
        return false;
    }
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
    let pixel_addr = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    let Some(first_slot) = slot_offset(0) else {
        return;
    };
    if pixel_addr != 0 && framebuffer_addr != pixel_addr as u64 + first_slot as u64 {
        SHARED_HEADER_ADDR.store(0, Ordering::Release);
        SHARED_PIXEL_ADDR.store(0, Ordering::Release);
        SHARED_DOORBELL_ADDR.store(0, Ordering::Release);
        GPU_COMMAND_OFFSET.store(0, Ordering::Release);
        GPU_ATLAS_OFFSET.store(0, Ordering::Release);
        GPU_ATLAS_SLOT_BYTES.store(0, Ordering::Release);
        GPU_ATLAS_WIDTH.store(0, Ordering::Release);
        GPU_ATLAS_HEIGHT.store(0, Ordering::Release);
        GPU_ATLAS_STRIDE_BYTES.store(0, Ordering::Release);
        reset_gpu_slots();
        INSTALLED.store(false, Ordering::Release);
        TRANSPORT_REVOKED.store(true, Ordering::Release);
    }
}

/// MSI-X leaf callbacks: allocation, logging, composition, and protocol
/// parsing are forbidden here. The next normal present drains the fixed record.
fn gui_dvm_control_interrupt(_vector: u8) {
    GUI_DVM_CONTROL_IRQ_PENDING.store(true, Ordering::Release);
}

fn gui_dvm_offline_interrupt(_vector: u8) {
    GUI_DVM_OFFLINE_IRQ_PENDING.store(true, Ordering::Release);
}

#[cfg(test)]
#[path = "display_transport_tests.rs"]
mod tests;
// RING3-MIGRATION-REFERENCE END: GUI-DVM display transport substrate.
