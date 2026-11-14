//! Host-authorized app-buffer mailbox. The payload is never an authority: the
//! retained kernel capability supplies identity and range. Publication leases,
//! service policy and timeout cancellation belong to the caller.
//!
//! - **Owner:** io-manager owns the one-slot request/response mailbox shared
//!   with the display provider; the broker in kernel-compat owns policy.
//! - **Boundary:** responses come from a provider and are untrusted; a response
//!   is matched to its request by the retained capability and exact epoch, and
//!   a payload field never selects a range.
//! - **Lifecycle:** a request is published once, answered at most once, and
//!   retired by the caller on completion, timeout or provider restart.
//! - **Concurrency:** the mailbox mutex is taken before the app-buffer pool and
//!   is never held across a wait or a callback.
//! - **Failure:** a stale, duplicate or mismatched response changes nothing and
//!   leaves the request for the caller's timeout path.
//! - **Forbidden:** treating payload bytes as identity, blocking while holding
//!   the mailbox, and completing a request the caller already cancelled.
//! - **Evidence:** `formal/app-buffer-custody`, host tests in this module, and
//!   `kvm-smoke --app-dmabuf-probe`.
use super::*;
use driver_domain_protocol::{
    DVM_APP_BUFFER_APERTURE_BYTES, DVM_APP_BUFFER_PAGE_BYTES, DVM_APP_BUFFER_PHYS_ADDR,
    DVM_APP_BUFFER_REQUEST_EXPORT, DVM_APP_BUFFER_REQUEST_KIND_OFFSET,
    DVM_APP_BUFFER_REQUEST_RECORD_OFFSET, DVM_APP_BUFFER_REQUEST_REVOKE,
    DVM_APP_BUFFER_REQUEST_SEQUENCE_OFFSET, DVM_APP_BUFFER_RESPONSE_SEQUENCE_OFFSET,
    DVM_APP_BUFFER_RESPONSE_STATUS_OFFSET, DvmAppBuffer,
};
use kernel_ps::api::handles::{BufferCapability, app_buffer_pool};
use spin::Mutex;

const VIRTIO_PMEM_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_PMEM_DEVICE_ID: u16 = 0x105b;
static APERTURE_VERIFIED: AtomicBool = AtomicBool::new(false);

// The atlas pixel pool ends exactly where the app aperture begins.
const _: () = assert!(
    GUI_DVM_PIXEL_REGION_PHYS_ADDR + GUI_DVM_PIXEL_REGION_BYTES == DVM_APP_BUFFER_PHYS_ADDR
);

/// Authenticate the fixed app aperture before any physical address derived from
/// it can reach a user page table. Both conditions are required: the physical
/// allocator must not own any frame of the range (so a user mapping can never
/// alias RAM), and the KVM topology's virtio-pmem function must be present.
/// The result is cached only on success; the constants are never taken from a
/// provider or a client. Reading the pmem function's own start/size registers
/// is not implemented, so a mismatched host layout is not detected beyond RAM
/// aliasing and is unverified until a boot run.
pub fn verify_aperture() -> bool {
    if APERTURE_VERIFIED.load(Ordering::Acquire) {
        return true;
    }
    if driver_domain_protocol::DVM_APP_BUFFER_PHYS_ADDR % DVM_APP_BUFFER_PAGE_BYTES != 0
        || kernel_mm::api::phys::range_overlaps_boot_usable(
            DVM_APP_BUFFER_PHYS_ADDR,
            DVM_APP_BUFFER_APERTURE_BYTES,
        )
    {
        return false;
    }
    let mut present = false;
    crate::arch::pci::visit_devices(|device| {
        present = device.vendor_id() == VIRTIO_PMEM_VENDOR_ID
            && device.device_id() == VIRTIO_PMEM_DEVICE_ID;
        present
    });
    if present {
        APERTURE_VERIFIED.store(true, Ordering::Release);
    }
    present
}

/// Physical base for a verified aperture offset range, or `None`.
pub fn aperture_phys(offset: u64, len: u64) -> Option<u64> {
    if !verify_aperture() || len == 0 {
        return None;
    }
    let end = offset.checked_add(len)?;
    (end <= DVM_APP_BUFFER_APERTURE_BYTES).then_some(DVM_APP_BUFFER_PHYS_ADDR + offset)
}

/// Bounded copy out of the verified aperture. The caller holds a mapping hold
/// on the owning capability, so the range cannot be scrubbed or reused.
pub fn read_range(offset: u64, dst: &mut [u8]) -> bool {
    let Some(phys) = aperture_phys(offset, dst.len() as u64) else {
        return false;
    };
    let src = crate::memory::higher_half_addr(phys) as *const u8;
    fence(Ordering::Acquire);
    // SAFETY: verified reserved WB aperture; `dst` is a distinct kernel buffer.
    unsafe { ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len()) };
    true
}

/// Kernel zeroing of a quarantined aperture range, used only after custody
/// proves provider retirement, no capability references and no mappings.
/// The caller supplies a range obtained from `reclaimable_range`.
pub fn scrub_range(offset: u64, len: u64) -> bool {
    let Some(phys) = aperture_phys(offset, len) else {
        return false;
    };
    let base = crate::memory::higher_half_addr(phys) as *mut u8;
    let mut done = 0u64;
    while done < len {
        let chunk = (len - done).min(1 << 20) as usize;
        // SAFETY: the verified aperture is reserved WB memory outside the
        // allocator, and this range is exclusively quarantined by custody.
        unsafe { ptr::write_bytes(base.add(done as usize), 0, chunk) };
        done += chunk as u64;
    }
    fence(Ordering::SeqCst);
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppBufferTransportError {
    Unavailable,
    Busy,
    Invalid,
    Stale,
    Exhausted,
    Provider(i64),
}

struct Pending {
    sequence: u64,
    kind: u64,
    capability: BufferCapability,
}

struct Mailbox {
    sequence: u64,
    pending: Option<Pending>,
}

static MAILBOX: Mutex<Mailbox> = Mutex::new(Mailbox {
    sequence: 0,
    pending: None,
});

/// The caller holds the exact live service-publication lease until this
/// operation has published. Geometry is policy-selected; range and identity
/// must match the independently retained kernel capability.
pub fn begin(
    capability: &BufferCapability,
    record: DvmAppBuffer,
    revoke: bool,
) -> Result<u64, AppBufferTransportError> {
    let mut mailbox = MAILBOX.lock();
    if mailbox.pending.is_some() {
        return Err(AppBufferTransportError::Busy);
    }
    if !INSTALLED.load(Ordering::Acquire)
        || TRANSPORT_REVOKED.load(Ordering::Acquire)
        || !GUI_DVM_PEER_READY.load(Ordering::Acquire)
    {
        return Err(AppBufferTransportError::Unavailable);
    }
    if !verify_aperture() {
        return Err(AppBufferTransportError::Unavailable);
    }
    let id = capability.identity();
    if id.owner.generation != record.provider_generation || id.allocation != record.allocation {
        return Err(AppBufferTransportError::Invalid);
    }
    let mut pool = app_buffer_pool();
    let range = if revoke { pool.range_including_revoked(id) } else { pool.range(id) }
        .map_err(|_| AppBufferTransportError::Stale)?;
    if range != (record.offset, record.length) {
        return Err(AppBufferTransportError::Invalid);
    }
    let bytes = record.encode().ok_or(AppBufferTransportError::Invalid)?;
    if revoke {
        pool.revoke(id)
            .map_err(|_| AppBufferTransportError::Stale)?;
    }
    drop(pool);
    let sequence = mailbox
        .sequence
        .checked_add(1)
        .ok_or(AppBufferTransportError::Exhausted)?;
    let kind = if revoke {
        DVM_APP_BUFFER_REQUEST_REVOKE
    } else {
        DVM_APP_BUFFER_REQUEST_EXPORT
    };
    mailbox.pending = Some(Pending {
        sequence,
        kind,
        capability: capability.clone(),
    });
    mailbox.sequence = sequence;
    write_bytes(DVM_APP_BUFFER_REQUEST_RECORD_OFFSET, &bytes);
    write_u64(DVM_APP_BUFFER_REQUEST_KIND_OFFSET, kind);
    write_u64(DVM_APP_BUFFER_REQUEST_SEQUENCE_OFFSET, sequence);
    signal_gui_dvm(GUI_DVM_EXPECTED_INVITATION.load(Ordering::Acquire));
    // Proof markers for `kvm-smoke --app-dmabuf-probe`; the range is the
    // kernel's own record, never provider or client text.
    // Only the milestone channel reaches debugcon in the product build; the
    // range is packed as (offset pages << 32) | length pages.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Display,
        if revoke { "app-dmabuf-revoke-published" } else { "app-dmabuf-export-published" },
        sequence,
        ((record.offset / 4096) << 32) | (record.length / 4096),
    );
    Ok(sequence)
}

/// A missing, stale or failed response never manufactures provider retirement.
/// The retained capability cannot disappear while a response is outstanding.
pub fn take(sequence: u64) -> Result<Option<BufferCapability>, AppBufferTransportError> {
    let mut mailbox = MAILBOX.lock();
    let pending = mailbox
        .pending
        .as_ref()
        .ok_or(AppBufferTransportError::Stale)?;
    if pending.sequence != sequence {
        return Err(AppBufferTransportError::Stale);
    }
    if !GUI_DVM_PEER_READY.load(Ordering::Acquire) || TRANSPORT_REVOKED.load(Ordering::Acquire) {
        return Err(AppBufferTransportError::Unavailable);
    }
    if read_u64(DVM_APP_BUFFER_RESPONSE_SEQUENCE_OFFSET) != sequence {
        return Ok(None);
    }
    fence(Ordering::Acquire);
    let status = read_u64(DVM_APP_BUFFER_RESPONSE_STATUS_OFFSET) as i64;
    let pending = mailbox.pending.take().expect("pending mailbox transaction");
    crate::debug::record_milestone(
        crate::debug::LogCategory::Display,
        if pending.kind == DVM_APP_BUFFER_REQUEST_REVOKE {
            "app-dmabuf-revoke-ack"
        } else {
            "app-dmabuf-export-ack"
        },
        sequence,
        status as u64,
    );
    if status != 0 {
        return Err(AppBufferTransportError::Provider(status));
    }
    if pending.kind == DVM_APP_BUFFER_REQUEST_REVOKE {
        // The provider acknowledges only after its final dma-buf reference has
        // disappeared. Host mapping/TLB/capability custody still gates reuse.
        let mut pool = app_buffer_pool();
        pool.provider_retired(pending.capability.identity())
            .map_err(|_| AppBufferTransportError::Stale)?;
    }
    Ok(Some(pending.capability))
}
