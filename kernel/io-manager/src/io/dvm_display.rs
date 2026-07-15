// RING3-MIGRATION-REFERENCE START: GUI-DVM display transport substrate.
// Policy stays in uiserver and the GUI DVM. Ring0 only maps the exact
// host-created three-slot pool, copies a bounded frame into a host-owned slot,
// and handles the two fixed MSI-X leaf notifications without composing or
// interpreting a guest command stream.
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

use driver_abi::{
    DISPLAY_FRAMEBUFFER_FLAG_DVM_SCANOUT, DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER,
    DisplayFramebufferRegistration, DisplayPixelFormat,
};
use driver_domain_protocol::{
    DVM_GUI_SURFACE_POOL_DVM_RECORD_OFFSET, DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET,
    DVM_GUI_SURFACE_POOL_HEADER_BYTES, DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET,
    DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET, DVM_GUI_SURFACE_POOL_INVITATION_OFFSET,
    DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET, DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET,
    DVM_GUI_SURFACE_SLOT_COUNT, DvmDisplayDamage, DvmGuiSurfaceMessage, DvmGuiSurfaceMessageKind,
    DvmGuiSurfacePoolHeader,
};

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const IVSHMEM_REGISTERS_BAR: usize = 0;
const IVSHMEM_SHARED_MEMORY_BAR: usize = 2;
const IVSHMEM_DOORBELL_OFFSET: usize = 12;
const GUI_DVM_PIXEL_REGION_PHYS_ADDR: u64 = 0x1_0000_0000;
const GUI_DVM_PIXEL_REGION_BYTES: u64 = 32 * 1024 * 1024;
const GUI_DVM_PEER_ID: u32 = 1;
const GUI_DVM_CONTROL_VECTOR_INDEX: usize = 0;
const GUI_DVM_OFFLINE_VECTOR_INDEX: usize = 1;
const GUI_DVM_MSIX_VECTOR_COUNT: u16 = 2;
const GUI_SLOT_COUNT: usize = DVM_GUI_SURFACE_SLOT_COUNT as usize;
const GUI_SLOT_FREE: u8 = 0;
const GUI_SLOT_WRITING: u8 = 1;
const GUI_SLOT_READY: u8 = 2;
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
static PUBLISHED_GENERATION: AtomicU64 = AtomicU64::new(0);
static SLOT_STATE: [AtomicU8; GUI_SLOT_COUNT] = [
    AtomicU8::new(GUI_SLOT_FREE),
    AtomicU8::new(GUI_SLOT_FREE),
    AtomicU8::new(GUI_SLOT_FREE),
];
static SLOT_GENERATION: [AtomicU64; GUI_SLOT_COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);
/// IRQ callbacks only set these pending bits. All control-record validation and
/// state transitions run at the next normal present boundary.
static GUI_DVM_CONTROL_IRQ_PENDING: AtomicBool = AtomicBool::new(false);
static GUI_DVM_OFFLINE_IRQ_PENDING: AtomicBool = AtomicBool::new(false);
static GUI_DVM_PEER_READY: AtomicBool = AtomicBool::new(false);
static GUI_DVM_EXPECTED_INVITATION: AtomicU64 = AtomicU64::new(0);
static GUI_DVM_ACKED_CONTROL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct MappedGuiDvmPool {
    device: crate::arch::pci::PciDevice,
    control: *mut u8,
    pixels: *mut u8,
    doorbell: *mut u8,
    header: DvmGuiSurfacePoolHeader,
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

fn try_install_serialized() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return !TRANSPORT_REVOKED.load(Ordering::Acquire);
    }
    if INSTALL_REJECTED.load(Ordering::Acquire) {
        return false;
    }
    let Some(pool) = find_ivshmem_gui_pool() else {
        crate::debug::warn!(
            display,
            "gui-dvm: no valid three-slot ivshmem pool discovered"
        );
        return false;
    };
    if !arm_gui_dvm_interrupts(pool.device) {
        release_gui_pool(pool);
        reject_install("msix-control-interrupt-unavailable");
        return false;
    }
    let Some(first_slot) = pool.header.slot_offset(0) else {
        release_gui_pool(pool);
        reject_install("slot-zero-out-of-range");
        return false;
    };
    let frame = unsafe { pool.pixels.add(first_slot as usize) };
    let registration = DisplayFramebufferRegistration {
        addr: frame as u64,
        size: pool.header.slot_bytes,
        back_buffer_addr: 0,
        back_buffer_size: 0,
        width: pool.header.width,
        height: pool.header.height,
        stride: pool.header.stride_bytes / 4,
        pixel_format: DisplayPixelFormat::Bgr as u32,
        bytes_per_pixel: 4,
        // Provenance can only lower trust. A GUI-DVM scanout never attests a
        // prompt, even when its transport is live and all control records pass.
        flags: DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER | DISPLAY_FRAMEBUFFER_FLAG_DVM_SCANOUT,
        reserved: [0; 2],
    };
    if unsafe { crate::io::gui::register_driver_framebuffer(&registration) } != 0 {
        release_gui_pool(pool);
        reject_install("provider-rejected");
        return false;
    }

    SHARED_HEADER_ADDR.store(pool.control as usize, Ordering::Release);
    SHARED_PIXEL_ADDR.store(pool.pixels as usize, Ordering::Release);
    SHARED_DOORBELL_ADDR.store(pool.doorbell as usize, Ordering::Release);
    POOL_WIDTH.store(pool.header.width, Ordering::Release);
    POOL_HEIGHT.store(pool.header.height, Ordering::Release);
    POOL_STRIDE_BYTES.store(pool.header.stride_bytes, Ordering::Release);
    POOL_SLOT_BYTES.store(pool.header.slot_bytes, Ordering::Release);
    PUBLISHED_GENERATION.store(0, Ordering::Release);
    DROPPED_FRAMES.store(0, Ordering::Release);
    GUI_DVM_CONTROL_IRQ_PENDING.store(false, Ordering::Release);
    GUI_DVM_OFFLINE_IRQ_PENDING.store(false, Ordering::Release);
    GUI_DVM_PEER_READY.store(false, Ordering::Release);
    GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
    GUI_DVM_ACKED_CONTROL_SEQUENCE.store(0, Ordering::Release);
    TRANSPORT_REVOKED.store(false, Ordering::Release);
    for state in &SLOT_STATE {
        state.store(GUI_SLOT_FREE, Ordering::Release);
    }
    for generation in &SLOT_GENERATION {
        generation.store(0, Ordering::Release);
    }
    write_u64(DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, 0);
    INSTALLED.store(true, Ordering::Release);
    crate::debug::info!(
        display,
        "gui-dvm: cacheable-pixel provider published width={} height={} stride={} slot_bytes={} pixel_phys={:#x}",
        pool.header.width,
        pool.header.height,
        pool.header.stride_bytes,
        pool.header.slot_bytes,
        GUI_DVM_PIXEL_REGION_PHYS_ADDR,
    );
    true
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
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
) -> DvmPresentOutcome {
    publish_frame(
        src_ptr,
        width,
        height,
        stride_bytes,
        DvmDisplayDamage::rect(x, y, rect_width, rect_height),
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
    let Some(slot) = reserve_free_slot() else {
        // A relay can restart after all three slots were committed.  There is
        // then no new frame to create an invitation, so a bounded producer
        // must re-advertise the newest extant READY slot exactly once after an
        // offline transition.  This is recovery of the fixed pool, not a
        // polling or alternate-provider fallback.
        reinvite_newest_ready_slot();
        DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
        return DvmPresentOutcome::Backpressured;
    };
    let generation = next_even_generation();
    let copied = copy_into_slot(slot, src_ptr, width, height, stride_bytes, damage);
    if !copied {
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        return DvmPresentOutcome::Unavailable;
    }
    let message = DvmGuiSurfaceMessage::present(slot as u32, generation, damage);
    if !message.is_valid_for_dimensions(
        POOL_WIDTH.load(Ordering::Acquire),
        POOL_HEIGHT.load(Ordering::Acquire),
    ) {
        SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
        warn_rejected("host-present-record-invalid");
        return DvmPresentOutcome::Unavailable;
    }
    write_host_record(slot, message);
    SLOT_GENERATION[slot].store(generation, Ordering::Release);
    SLOT_STATE[slot].store(GUI_SLOT_READY, Ordering::Release);
    signal_gui_dvm(generation);
    DvmPresentOutcome::Presented
}

fn reserve_free_slot() -> Option<usize> {
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

fn next_even_generation() -> u64 {
    loop {
        let current = PUBLISHED_GENERATION.load(Ordering::Acquire);
        let mut next = current.wrapping_add(2);
        if next == 0 {
            next = 2;
        }
        if PUBLISHED_GENERATION
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return next;
        }
    }
}

fn copy_into_slot(
    slot: usize,
    src_ptr: *const u8,
    width: usize,
    height: usize,
    stride_bytes: usize,
    damage: DvmDisplayDamage,
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
    if damage_bounds(damage, width, height).is_none() {
        warn_rejected("damage-bounds-invalid");
        return false;
    }
    // Every published slot is a self-contained immutable frame snapshot.
    // Damage remains scheduling metadata for the consumer, never a license
    // to depend on the previous contents of a recycled capability slot.
    let (x, y, copy_width, copy_height) = (0_usize, 0_usize, width, height);
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
    if end > POOL_SLOT_BYTES.load(Ordering::Acquire) as usize {
        warn_rejected("copy-exceeds-slot");
        return false;
    }
    let pixels = SHARED_PIXEL_ADDR.load(Ordering::Acquire);
    if pixels == 0 {
        return false;
    }
    unsafe {
        let mut source = src_ptr.add(row_offset);
        let mut destination = (pixels as *mut u8).add(slot_offset + row_offset);
        for _ in 0..copy_height {
            // The pixel pool is reserved WB memory; the ivshmem BAR carries
            // control records only and is never used as a bulk-copy target.
            ptr::copy_nonoverlapping(source, destination, row_bytes);
            source = source.add(stride_bytes);
            destination = destination.add(stride_bytes);
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

/// Handle the DVM's one fixed return record. A guest cannot free a writing
/// slot, invent a generation, or advance the sequence more than once without
/// a host acknowledgement. Unsupported focus changes fail closed because the
/// current single-domain input broker has no multi-domain focus authority.
fn drain_dvm_control() {
    if GUI_DVM_OFFLINE_IRQ_PENDING.swap(false, Ordering::AcqRel) {
        GUI_DVM_PEER_READY.store(false, Ordering::Release);
        GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
        // A future relay must not mistake an old confirmation for approval of
        // a reused generation after DVM restart.
        write_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, 0);
    }
    if !GUI_DVM_CONTROL_IRQ_PENDING.swap(false, Ordering::AcqRel) {
        return;
    }
    let expected = GUI_DVM_EXPECTED_INVITATION.load(Ordering::Acquire);
    let ready_ack = read_u64(DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET);
    if expected != 0 && expected == ready_ack {
        GUI_DVM_PEER_READY.store(true, Ordering::Release);
        write_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, expected);
        // The first host present can legitimately precede Linux-DVM boot. In
        // that order its original doorbell is unobservable, so confirmation
        // is also the single replay-safe wakeup that lets the DVM consume the
        // already committed slot. `GUI_DVM_PEER_READY` is set first, therefore
        // `signal_gui_dvm` cannot rewrite the accepted invitation.
        signal_gui_dvm(expected);
    }
    let sequence = read_u64(DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET);
    let acknowledged = GUI_DVM_ACKED_CONTROL_SEQUENCE.load(Ordering::Acquire);
    if sequence == 0 || sequence <= acknowledged {
        return;
    }
    let Some(message) = read_dvm_record() else {
        revoke_transport("malformed-dvm-control-record", sequence);
        return;
    };
    let valid_release = matches!(message.kind, DvmGuiSurfaceMessageKind::Release)
        && message.is_valid_for_dimensions(
            POOL_WIDTH.load(Ordering::Acquire),
            POOL_HEIGHT.load(Ordering::Acquire),
        )
        && usize::try_from(message.slot).ok().is_some_and(|slot| {
            slot < GUI_SLOT_COUNT
                && SLOT_STATE[slot].load(Ordering::Acquire) == GUI_SLOT_READY
                && SLOT_GENERATION[slot].load(Ordering::Acquire) == message.generation
        });
    if !valid_release {
        revoke_transport("unauthorized-dvm-slot-release", sequence);
        return;
    }
    let slot = message.slot as usize;
    SLOT_GENERATION[slot].store(0, Ordering::Release);
    SLOT_STATE[slot].store(GUI_SLOT_FREE, Ordering::Release);
    GUI_DVM_ACKED_CONTROL_SEQUENCE.store(sequence, Ordering::Release);
    write_u64(DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, sequence);
}

fn revoke_transport(reason: &str, sequence: u64) {
    TRANSPORT_REVOKED.store(true, Ordering::Release);
    GUI_DVM_PEER_READY.store(false, Ordering::Release);
    GUI_DVM_EXPECTED_INVITATION.store(0, Ordering::Release);
    GUI_DVM_ACKED_CONTROL_SEQUENCE.store(sequence, Ordering::Release);
    write_u64(DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, sequence);
    crate::debug::warn!(display, "gui-dvm: transport revoked reason={}", reason);
}

fn signal_gui_dvm(generation: u64) {
    let doorbell = SHARED_DOORBELL_ADDR.load(Ordering::Acquire);
    if doorbell == 0 || generation == 0 || generation & 1 != 0 {
        return;
    }
    if !GUI_DVM_PEER_READY.load(Ordering::Acquire) {
        write_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, generation);
        GUI_DVM_EXPECTED_INVITATION.store(generation, Ordering::Release);
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

fn reinvite_newest_ready_slot() {
    if GUI_DVM_PEER_READY.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return;
    }
    let mut newest = 0_u64;
    for slot in 0..GUI_SLOT_COUNT {
        if SLOT_STATE[slot].load(Ordering::Acquire) == GUI_SLOT_READY {
            newest = newest.max(SLOT_GENERATION[slot].load(Ordering::Acquire));
        }
    }
    if newest == 0 || newest & 1 != 0 {
        return;
    }
    if GUI_DVM_EXPECTED_INVITATION.load(Ordering::Acquire) != newest {
        signal_gui_dvm(newest);
    }
}

fn write_host_record(slot: usize, message: DvmGuiSurfaceMessage) {
    let Some(offset) = DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET
        .checked_add(slot.saturating_mul(DvmGuiSurfaceMessage::encoded_len()))
    else {
        return;
    };
    write_bytes(offset, &message.encode());
}

fn read_dvm_record() -> Option<DvmGuiSurfaceMessage> {
    let mut bytes = [0_u8; DvmGuiSurfaceMessage::encoded_len()];
    read_bytes(DVM_GUI_SURFACE_POOL_DVM_RECORD_OFFSET, &mut bytes)?;
    DvmGuiSurfaceMessage::decode(&bytes)
}

fn write_u64(offset: usize, value: u64) {
    write_bytes(offset, &value.to_le_bytes());
}

fn read_u64(offset: usize) -> u64 {
    let mut bytes = [0_u8; size_of::<u64>()];
    if read_bytes(offset, &mut bytes).is_none() {
        return 0;
    }
    u64::from_le_bytes(bytes)
}

fn write_bytes(offset: usize, bytes: &[u8]) {
    let header = SHARED_HEADER_ADDR.load(Ordering::Acquire);
    if header == 0 {
        return;
    }
    unsafe {
        for (index, byte) in bytes.iter().enumerate() {
            (header as *mut u8)
                .add(offset + index)
                .write_volatile(*byte);
        }
    }
    fence(Ordering::SeqCst);
}

fn read_bytes(offset: usize, bytes: &mut [u8]) -> Option<()> {
    let header = SHARED_HEADER_ADDR.load(Ordering::Acquire);
    if header == 0 {
        return None;
    }
    unsafe {
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (header as *const u8).add(offset + index).read_volatile();
        }
    }
    Some(())
}

fn slot_offset(slot: usize) -> Option<usize> {
    if slot >= GUI_SLOT_COUNT {
        return None;
    }
    let slot_bytes = usize::try_from(POOL_SLOT_BYTES.load(Ordering::Acquire)).ok()?;
    (DVM_GUI_SURFACE_POOL_HEADER_BYTES as usize).checked_add(slot.checked_mul(slot_bytes)?)
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

/// Program exactly two host receive vectors: control/ready and offline. The
/// device cannot turn them into a generic guest-selected interrupt allocator.
fn arm_gui_dvm_interrupts(device: crate::arch::pci::PciDevice) -> bool {
    let Some(capability) = device.msix_capability() else {
        return false;
    };
    if capability.table_entries() != GUI_DVM_MSIX_VECTOR_COUNT {
        return false;
    }
    let Some(table_resource) = capability.table_resource(device) else {
        return false;
    };
    let Ok(table_len) = usize::try_from(table_resource.size) else {
        return false;
    };
    let Ok(table_offset) = usize::try_from(capability.table_offset()) else {
        return false;
    };
    if table_offset
        .checked_add(MSIX_ENTRY_BYTES * GUI_DVM_MSIX_VECTOR_COUNT as usize)
        .is_none_or(|end| end > table_len)
    {
        return false;
    }
    capability.set_function_masked(device, true);
    capability.set_enabled(device, false);
    let Some(control_vector) = crate::arch::msi::allocate_vector() else {
        return false;
    };
    if !crate::arch::msi::register_handler(control_vector, gui_dvm_control_interrupt) {
        return false;
    }
    let Some(offline_vector) = crate::arch::msi::allocate_vector() else {
        return false;
    };
    if !crate::arch::msi::register_handler(offline_vector, gui_dvm_offline_interrupt) {
        return false;
    }
    let Some(control_message) = crate::arch::msi::message_for(control_vector) else {
        return false;
    };
    let Some(offline_message) = crate::arch::msi::message_for(offline_vector) else {
        return false;
    };
    let table = crate::driver::mmio::map(table_resource.start, table_len, false).cast::<u8>();
    if table.is_null() {
        return false;
    }
    unsafe {
        program_msix_entry(table.add(table_offset), control_message);
        program_msix_entry(
            table.add(table_offset + GUI_DVM_OFFLINE_VECTOR_INDEX * MSIX_ENTRY_BYTES),
            offline_message,
        );
        fence(Ordering::SeqCst);
        unmask_msix_entry(
            table.add(table_offset + GUI_DVM_CONTROL_VECTOR_INDEX * MSIX_ENTRY_BYTES),
        );
        unmask_msix_entry(
            table.add(table_offset + GUI_DVM_OFFLINE_VECTOR_INDEX * MSIX_ENTRY_BYTES),
        );
    }
    fence(Ordering::SeqCst);
    capability.set_enabled(device, true);
    capability.set_function_masked(device, false);
    true
}

unsafe fn program_msix_entry(entry: *mut u8, message: crate::arch::msi::MsiMessage) {
    unsafe {
        entry
            .add(MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(MSIX_ENTRY_VECTOR_MASKED.to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_LOW_OFFSET)
            .cast::<u32>()
            .write_volatile((message.address as u32).to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_HIGH_OFFSET)
            .cast::<u32>()
            .write_volatile(((message.address >> 32) as u32).to_le());
        entry
            .add(MSIX_ENTRY_DATA_OFFSET)
            .cast::<u32>()
            .write_volatile(message.data.to_le());
    }
}

unsafe fn unmask_msix_entry(entry: *mut u8) {
    unsafe {
        entry
            .add(MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(0);
    }
}

fn find_ivshmem_gui_pool() -> Option<MappedGuiDvmPool> {
    let mut found = None;
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() != IVSHMEM_VENDOR_ID || device.device_id() != IVSHMEM_DEVICE_ID {
            return false;
        }
        let Some(resource) = device.resource(IVSHMEM_SHARED_MEMORY_BAR) else {
            return false;
        };
        let Some(registers) = device.resource(IVSHMEM_REGISTERS_BAR) else {
            return false;
        };
        if resource.is_io || resource.size < u64::from(DVM_GUI_SURFACE_POOL_HEADER_BYTES) {
            return false;
        }
        if registers.is_io
            || registers.size
                < u64::try_from(IVSHMEM_DOORBELL_OFFSET + size_of::<u32>()).unwrap_or(u64::MAX)
        {
            return false;
        }
        let control = crate::driver::mmio::map(
            resource.start,
            DVM_GUI_SURFACE_POOL_HEADER_BYTES as usize,
            false,
        )
        .cast::<u8>();
        if control.is_null() {
            return false;
        }
        let Ok(registers_len) = usize::try_from(registers.size) else {
            release_gui_mappings(control, core::ptr::null_mut());
            return false;
        };
        // BAR0 contains ivshmem control/doorbell registers, not framebuffer
        // memory. Both BAR0 and the BAR2 control header require uncached MMIO;
        // pixels live in the separately reserved cacheable memory device.
        let doorbell = crate::driver::mmio::map(registers.start, registers_len, false).cast::<u8>();
        if doorbell.is_null() {
            release_gui_mappings(control, doorbell);
            return false;
        }
        let Some(header) = read_pool_header(control) else {
            release_gui_mappings(control, doorbell);
            return false;
        };
        if !header_fits_resource(header, GUI_DVM_PIXEL_REGION_BYTES) {
            release_gui_mappings(control, doorbell);
            return false;
        }
        let pixels = crate::memory::higher_half_addr(GUI_DVM_PIXEL_REGION_PHYS_ADDR) as *mut u8;
        let Some(pixel_header) = read_pool_header(pixels) else {
            release_gui_mappings(control, doorbell);
            return false;
        };
        if pixel_header != header {
            release_gui_mappings(control, doorbell);
            return false;
        }
        found = Some(MappedGuiDvmPool {
            device,
            control,
            pixels,
            doorbell,
            header,
        });
        true
    });
    found
}

fn read_pool_header(mapped: *const u8) -> Option<DvmGuiSurfacePoolHeader> {
    let mut bytes = [0_u8; DvmGuiSurfacePoolHeader::encoded_len()];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { mapped.add(index).read_volatile() };
    }
    DvmGuiSurfacePoolHeader::decode(&bytes)
}

fn header_fits_resource(header: DvmGuiSurfacePoolHeader, resource_len: u64) -> bool {
    if header.region_bytes > resource_len {
        return false;
    }
    let Some(last_slot) = header.slot_offset(DVM_GUI_SURFACE_SLOT_COUNT - 1) else {
        return false;
    };
    last_slot
        .checked_add(header.slot_bytes)
        .is_some_and(|end| end <= header.region_bytes)
}

fn warn_rejected(reason: &str) {
    crate::debug::warn!(
        display,
        "gui-dvm: shared provider rejected reason={}",
        reason
    );
}

fn reject_install(reason: &str) {
    INSTALL_REJECTED.store(true, Ordering::Release);
    warn_rejected(reason);
}

fn release_gui_pool(pool: MappedGuiDvmPool) {
    release_gui_mappings(pool.control, pool.doorbell);
}

fn release_gui_mappings(mapped: *mut u8, doorbell: *mut u8) {
    if !doorbell.is_null() {
        crate::driver::mmio::unmap(doorbell.cast());
    }
    if !mapped.is_null() {
        crate::driver::mmio::unmap(mapped.cast());
    }
}

#[cfg(test)]
mod tests {
    use driver_domain_protocol::DvmGuiSurfacePoolHeader;

    use super::{DvmPresentOutcome, damage_bounds, header_fits_resource, try_publish_full};

    #[test]
    fn pool_header_must_cover_all_three_slots() {
        let header = DvmGuiSurfacePoolHeader::new(32 * 1024 * 1024, 1600, 900);
        assert!(header_fits_resource(header, 32 * 1024 * 1024));
        assert!(!header_fits_resource(header, header.region_bytes - 1));
    }

    #[test]
    fn damage_bounds_reject_overflow_and_accept_full_frame() {
        assert_eq!(
            damage_bounds(driver_domain_protocol::DvmDisplayDamage::full(), 1600, 900),
            Some((0, 0, 1600, 900))
        );
        assert_eq!(
            damage_bounds(
                driver_domain_protocol::DvmDisplayDamage::rect(1599, 899, 2, 1),
                1600,
                900
            ),
            None
        );
    }

    #[test]
    fn missing_gui_dvm_is_unavailable_not_a_fallback_provider() {
        assert_eq!(
            try_publish_full(core::ptr::null(), 1, 1, 4),
            DvmPresentOutcome::Unavailable
        );
    }
}
// RING3-MIGRATION-REFERENCE END: GUI-DVM display transport substrate.
