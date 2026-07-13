// RING3-MIGRATION-REFERENCE START: DVM display transport substrate.
// Provider policy remains in driverd/uiserver. Ring0 only recognizes the
// exact ivshmem PCI function, validates a fixed host-created header, and maps
// pixels as a framebuffer. It never accepts DVM pointers, command streams, or
// variable-length metadata.
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence};

use driver_abi::{
    DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER, DisplayFramebufferRegistration, DisplayPixelFormat,
};
use driver_domain_protocol::{DVM_DISPLAY_HEADER_BYTES, DVM_DISPLAY_RECORD_BYTES, DvmDisplayHeader};

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const IVSHMEM_SHARED_MEMORY_BAR: usize = 2;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SHARED_HEADER_ADDR: AtomicUsize = AtomicUsize::new(0);
static FRAME_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Install the host-created DVM display aperture when present. Absence is the
/// normal non-KVM path and deliberately produces no provider or fallback.
pub(crate) fn try_install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    let Some(device) = find_ivshmem_device() else {
        return false;
    };
    let Some(resource) = device.resource(IVSHMEM_SHARED_MEMORY_BAR) else {
        warn_rejected("missing-shared-bar");
        return false;
    };
    if resource.is_io || resource.size < u64::from(DVM_DISPLAY_HEADER_BYTES) {
        warn_rejected("invalid-shared-bar");
        return false;
    }
    let Ok(resource_len) = usize::try_from(resource.size) else {
        warn_rejected("shared-bar-too-large");
        return false;
    };
    let mapped = crate::driver::mmio::map(resource.start, resource_len, true).cast::<u8>();
    if mapped.is_null() {
        warn_rejected("shared-bar-map-failed");
        return false;
    }
    let Some(header) = read_header(mapped) else {
        warn_rejected("invalid-header");
        return false;
    };
    if header.generation & 1 != 0 {
        warn_rejected("unstable-initial-generation");
        return false;
    }
    if !header_fits_resource(header, resource.size) {
        warn_rejected("header-outside-resource");
        return false;
    }
    let frame = unsafe { mapped.add(DVM_DISPLAY_HEADER_BYTES as usize) };
    let registration = DisplayFramebufferRegistration {
        addr: frame as u64,
        size: header.frame_bytes,
        back_buffer_addr: 0,
        back_buffer_size: 0,
        width: header.width,
        height: header.height,
        stride: header.stride_bytes / 4,
        pixel_format: DisplayPixelFormat::Bgr as u32,
        bytes_per_pixel: 4,
        flags: DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER,
        reserved: [0; 2],
    };
    if unsafe { crate::io::gui::register_driver_framebuffer(&registration) } != 0 {
        warn_rejected("provider-rejected");
        return false;
    }
    SHARED_HEADER_ADDR.store(mapped as usize, Ordering::Release);
    FRAME_GENERATION.store(header.generation, Ordering::Release);
    INSTALLED.store(true, Ordering::Release);
    crate::debug::info!(
        display,
        "dvm-display: shared provider published width={} height={} stride={} generation={}",
        header.width,
        header.height,
        header.stride_bytes,
        header.generation,
    );
    true
}

/// Retry the optional DVM provider at the first real present boundary. Early
/// boot may precede complete PCI publication; this call deliberately happens
/// before the framebuffer backend lock is taken, because installation replaces
/// that backend through `register_driver_framebuffer`.
pub(crate) fn ensure_installed_before_present() {
    if !INSTALLED.load(Ordering::Acquire) {
        let _ = try_install();
    }
}

/// Return whether this present operation must publish a DVM display seqlock
/// transition. The framebuffer backend serializes callers, so one generation
/// counter covers the active primary provider.
pub(crate) fn begin_frame() -> bool {
    if SHARED_HEADER_ADDR.load(Ordering::Acquire) == 0 {
        return false;
    }
    let current = FRAME_GENERATION.load(Ordering::Relaxed);
    let mut writing = if current & 1 == 0 {
        current.wrapping_add(1)
    } else {
        current.wrapping_add(2)
    };
    if writing == 0 {
        writing = 1;
    }
    write_generation(writing);
    FRAME_GENERATION.store(writing, Ordering::Release);
    fence(Ordering::SeqCst);
    true
}

/// Finish a frame publication even when the local present failed. Leaving an
/// odd generation live would otherwise make a DVM consumer wait forever.
pub(crate) fn finish_frame(started: bool) {
    if !started {
        return;
    }
    fence(Ordering::SeqCst);
    let current = FRAME_GENERATION.load(Ordering::Relaxed);
    let mut complete = if current & 1 == 0 {
        current
    } else {
        current.wrapping_add(1)
    };
    if complete == 0 {
        complete = 2;
    }
    write_generation(complete);
    FRAME_GENERATION.store(complete, Ordering::Release);
}

/// Stop publishing when another provider replaces the DVM aperture.
pub(crate) fn on_framebuffer_installed(framebuffer_addr: u64) {
    let header_addr = SHARED_HEADER_ADDR.load(Ordering::Acquire);
    if header_addr != 0 && framebuffer_addr != header_addr as u64 + u64::from(DVM_DISPLAY_HEADER_BYTES) {
        SHARED_HEADER_ADDR.store(0, Ordering::Release);
        FRAME_GENERATION.store(0, Ordering::Release);
    }
}

fn write_generation(generation: u64) {
    let header = SHARED_HEADER_ADDR.load(Ordering::Acquire);
    if header == 0 {
        return;
    }
    // The ivshmem BAR is page-aligned, so the fixed offset is naturally
    // aligned for an x86_64 volatile u64 store. The wire value is little-endian.
    unsafe {
        (header as *mut u8)
            .add(56)
            .cast::<u64>()
            .write_volatile(generation.to_le());
    }
}

fn find_ivshmem_device() -> Option<crate::arch::pci::PciDevice> {
    let mut found = None;
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() == IVSHMEM_VENDOR_ID && device.device_id() == IVSHMEM_DEVICE_ID {
            found = Some(device);
            true
        } else {
            false
        }
    });
    found
}

fn read_header(mapped: *const u8) -> Option<DvmDisplayHeader> {
    let mut bytes = [0_u8; DVM_DISPLAY_RECORD_BYTES];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { mapped.add(index).read_volatile() };
    }
    DvmDisplayHeader::decode(&bytes)
}

fn header_fits_resource(header: DvmDisplayHeader, resource_len: u64) -> bool {
    header.region_bytes <= resource_len
        && header.frame_bytes <= header.region_bytes.saturating_sub(u64::from(DVM_DISPLAY_HEADER_BYTES))
}

fn warn_rejected(reason: &str) {
    crate::debug::warn!(display, "dvm-display: shared provider rejected reason={}", reason);
}

#[cfg(test)]
mod tests {
    use driver_domain_protocol::{DVM_DISPLAY_HEADER_BYTES, DvmDisplayHeader};

    use super::header_fits_resource;

    #[test]
    fn header_must_stay_inside_the_mapped_bar() {
        let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
        assert!(header_fits_resource(header, 8 * 1024 * 1024));
        assert!(!header_fits_resource(header, u64::from(DVM_DISPLAY_HEADER_BYTES)));
    }
}
// RING3-MIGRATION-REFERENCE END: DVM display transport substrate.
