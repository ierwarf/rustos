//! Transactional GUI-DVM aperture and MSI-X installation.
//!
//! - **Owner:** kernel-io-manager owns fixed mappings and vector claims;
//!   uiserver and the DVM retain composition and presentation policy.
//! - **Boundary:** PCI resources, shared headers and geometry are untrusted.
//! - **Lifecycle:** Claim vectors, validate/map apertures, initialize context,
//!   invite the peer, publish installation, then retain vector ownership.
//! - **Concurrency:** The parent's exclusive install claim encloses all steps.
//! - **Failure:** Every prepublication refusal releases mappings and claims.
//! - **Forbidden:** No alternate pool, CPU-frame proof or early publication.
//! - **Evidence:** `dvm-display-ingress` and ordered installation source checks.

use super::*;

pub(super) fn try_install_serialized() -> bool {
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
    let Some(interrupt_install) = arm_gui_dvm_interrupts(pool.device) else {
        release_gui_pool(pool);
        reject_install("msix-control-interrupt-unavailable");
        return false;
    };
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
    GPU_COMMAND_OFFSET.store(pool.atlas_header.command_offset, Ordering::Release);
    GPU_ATLAS_OFFSET.store(pool.atlas_header.atlas_offset, Ordering::Release);
    GPU_ATLAS_SLOT_BYTES.store(pool.atlas_header.atlas_slot_bytes, Ordering::Release);
    GPU_ATLAS_WIDTH.store(pool.atlas_header.atlas_width, Ordering::Release);
    GPU_ATLAS_HEIGHT.store(pool.atlas_header.atlas_height, Ordering::Release);
    GPU_ATLAS_STRIDE_BYTES.store(pool.atlas_header.atlas_stride_bytes, Ordering::Release);
    GPU_NEXT_SEQUENCE.store(0, Ordering::Release);
    GPU_CONTEXT_EPOCH.store(GPU_INITIAL_CONTEXT_EPOCH, Ordering::Release);
    GPU_PRIME_DURATION_NS.store(0, Ordering::Release);
    GPU_SUBMIT_FLAGS.store(0, Ordering::Release);
    GPU_SESSION_SUBMISSIONS.store(0, Ordering::Release);
    GPU_SUBMIT_LOCK.store(false, Ordering::Release);
    for slot in 0..DVM_GPU_RENDER_MAX_IN_FLIGHT as usize {
        GPU_SLOT_STATE[slot].store(GPU_SLOT_FREE, Ordering::Release);
        GPU_SLOT_GENERATION[slot].store(0, Ordering::Release);
        GPU_SLOT_SEQUENCE[slot].store(0, Ordering::Release);
        GPU_SLOT_CONTEXT_ID[slot].store(0, Ordering::Release);
        GPU_SLOT_CONTEXT_EPOCH[slot].store(0, Ordering::Release);
        GPU_SLOT_SUBMIT_VALUE[slot].store(0, Ordering::Release);
        if let Some(offset) = dvm_gpu_atlas_invitation_offset(slot as u32) {
            write_u64(offset, 0);
        }
        if let Some(offset) = dvm_gpu_atlas_completion_sequence_offset(slot as u32) {
            write_u64(offset, 0);
        }
        if let Some(offset) = dvm_gpu_atlas_completion_ack_offset(slot as u32) {
            write_u64(offset, 0);
        }
    }
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
    for generation in &SLOT_CONTENT_GENERATION {
        generation.store(0, Ordering::Release);
    }
    LAST_SOURCE_PTR.store(0, Ordering::Release);
    write_u64(DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_INVITATION_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET, 0);
    write_u64(DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET, 0);
    write_u32(DVM_GPU_ATLAS_CONTEXT_ID_OFFSET, GPU_CONTEXT_ID);
    write_u32(
        DVM_GPU_ATLAS_CONTEXT_EPOCH_OFFSET,
        GPU_INITIAL_CONTEXT_EPOCH,
    );
    write_u64(DVM_GPU_ATLAS_PRIME_FENCE_OFFSET, GPU_PRIME_FENCE_VALUE);
    write_bytes(
        DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET,
        &[0; DVM_GPU_PRIME_COMPLETION_BYTES],
    );
    assert!(
        GPU_LIFECYCLE.activate(u64::from(GPU_INITIAL_CONTEXT_EPOCH)),
        "gui-dvm lifecycle activation failed during install"
    );
    // Prime is control-only: mandatory GPU UI cannot publish a CPU frame to
    // obtain the very provider proof it needs before its first submission.
    // Advertise under the exclusive install transaction BEFORE publication;
    // a GET_INFO check/use pair could overwrite a newer frame invitation.
    signal_gui_dvm(2);
    // ORDERING: Release makes the complete context and initial invitation
    // visible before any present/discovery consumer can use this transport.
    INSTALLED.store(true, Ordering::Release);
    interrupt_install.retain_permanent();
    crate::debug::info!(
        display,
        "gui-dvm: cacheable-pixel provider published width={} height={} stride={} slot_bytes={} gpu_atlas={}x{} gpu_stride={} pixel_phys={:#x}",
        pool.header.width,
        pool.header.height,
        pool.header.stride_bytes,
        pool.header.slot_bytes,
        pool.atlas_header.atlas_width,
        pool.atlas_header.atlas_height,
        pool.atlas_header.atlas_stride_bytes,
        GUI_DVM_PIXEL_REGION_PHYS_ADDR,
    );
    true
}

/// Program exactly two host receive vectors: control/ready and offline. The
/// device cannot turn them into a generic guest-selected interrupt allocator.
struct GuiInterruptInstall {
    capability: crate::arch::pci::MsixCapability,
    /// Exclusive claim on the function, held for the transaction's whole life
    /// so no other driver can reprogram the interrupts this one armed.
    attach: Option<crate::arch::pci::PciAttach>,
    control: Option<crate::arch::msi::CommittedMsiVector>,
    offline: Option<crate::arch::msi::CommittedMsiVector>,
}

impl GuiInterruptInstall {
    fn retain_permanent(mut self) {
        self.attach
            .take()
            .expect("GUI DVM interrupt transaction lost its device claim")
            .retain_permanent();
        self.control
            .take()
            .expect("GUI DVM interrupt transaction lost control vector")
            .retain_permanent();
        self.offline
            .take()
            .expect("GUI DVM interrupt transaction lost offline vector")
            .retain_permanent();
    }
}

impl Drop for GuiInterruptInstall {
    fn drop(&mut self) {
        if let (Some(attach), true) = (
            self.attach.as_ref(),
            self.control.is_some() || self.offline.is_some(),
        ) {
            self.capability.set_function_masked(attach, true);
            self.capability.set_enabled(attach, false);
            drop(self.offline.take());
            drop(self.control.take());
        }
    }
}

pub(super) fn arm_gui_dvm_interrupts(device: crate::arch::pci::PciDevice) -> Option<GuiInterruptInstall> {
    // Claim the function before the first configuration write; this transport
    // owns its ivshmem function outright for the rest of the boot.
    let attach = crate::arch::pci::attach(device, crate::arch::pci::PciAttachMode::Exclusive)?;
    let Some(capability) = device.msix_capability() else {
        return None;
    };
    if capability.table_entries() != GUI_DVM_MSIX_VECTOR_COUNT {
        return None;
    }
    let Some(table_resource) = capability.table_resource(device) else {
        return None;
    };
    let Ok(table_len) = usize::try_from(table_resource.size) else {
        return None;
    };
    let Ok(table_offset) = usize::try_from(capability.table_offset()) else {
        return None;
    };
    if table_offset
        .checked_add(MSIX_ENTRY_BYTES * GUI_DVM_MSIX_VECTOR_COUNT as usize)
        .is_none_or(|end| end > table_len)
    {
        return None;
    }
    capability.set_function_masked(&attach, true);
    capability.set_enabled(&attach, false);
    let Some(mut control_lease) = crate::arch::msi::MsiVectorLease::allocate() else {
        return None;
    };
    if !control_lease.register_handler(gui_dvm_control_interrupt) {
        return None;
    }
    let Some(mut offline_lease) = crate::arch::msi::MsiVectorLease::allocate() else {
        return None;
    };
    if !offline_lease.register_handler(gui_dvm_offline_interrupt) {
        return None;
    }
    let Some(control_message) = control_lease.message() else {
        return None;
    };
    let Some(offline_message) = offline_lease.message() else {
        return None;
    };
    let table = crate::driver::mmio::map_uncached(table_resource.start, table_len).cast::<u8>();
    if table.is_null() {
        return None;
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
    capability.set_enabled(&attach, true);
    capability.set_function_masked(&attach, false);
    crate::driver::mmio::unmap(table.cast());
    Some(GuiInterruptInstall {
        capability,
        attach: Some(attach),
        control: Some(control_lease.commit()),
        offline: Some(offline_lease.commit()),
    })
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

pub(super) fn find_ivshmem_gui_pool() -> Option<MappedGuiDvmPool> {
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
        let control = crate::driver::mmio::map_uncached(
            resource.start,
            DVM_GUI_SURFACE_POOL_HEADER_BYTES as usize,
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
        let doorbell =
            crate::driver::mmio::map_uncached(registers.start, registers_len).cast::<u8>();
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
        let Some(atlas_header) = read_atlas_pool_header(control) else {
            release_gui_mappings(control, doorbell);
            return false;
        };
        let Some(pixel_atlas_header) = read_atlas_pool_header(pixels) else {
            release_gui_mappings(control, doorbell);
            return false;
        };
        if pixel_atlas_header != atlas_header
            || !atlas_header_fits_resource(atlas_header, GUI_DVM_PIXEL_REGION_BYTES)
        {
            release_gui_mappings(control, doorbell);
            return false;
        }
        found = Some(MappedGuiDvmPool {
            device,
            control,
            pixels,
            doorbell,
            header,
            atlas_header,
        });
        true
    });
    found
}

pub(super) fn read_atlas_pool_header(mapped: *const u8) -> Option<DvmGpuAtlasPoolHeader> {
    let mut bytes = [0_u8; DvmGpuAtlasPoolHeader::encoded_len()];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe {
            mapped
                .add(DVM_GPU_ATLAS_POOL_HEADER_OFFSET + index)
                .read_volatile()
        };
    }
    DvmGpuAtlasPoolHeader::decode(&bytes)
}

pub(super) fn read_pool_header(mapped: *const u8) -> Option<DvmGuiSurfacePoolHeader> {
    let mut bytes = [0_u8; DvmGuiSurfacePoolHeader::encoded_len()];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { mapped.add(index).read_volatile() };
    }
    DvmGuiSurfacePoolHeader::decode(&bytes)
}

pub(super) fn header_fits_resource(header: DvmGuiSurfacePoolHeader, resource_len: u64) -> bool {
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

pub(super) fn atlas_header_fits_resource(header: DvmGpuAtlasPoolHeader, resource_len: u64) -> bool {
    header.region_bytes <= resource_len
        && header
            .atlas_slot_offset(DVM_GPU_RENDER_MAX_IN_FLIGHT - 1)
            .and_then(|offset| offset.checked_add(header.atlas_slot_bytes))
            .is_some_and(|end| end <= header.region_bytes)
}

pub(super) fn warn_rejected(reason: &str) {
    crate::debug::warn!(
        display,
        "gui-dvm: shared provider rejected reason={}",
        reason
    );
}

pub(super) fn reject_install(reason: &str) {
    INSTALL_REJECTED.store(true, Ordering::Release);
    warn_rejected(reason);
}

pub(super) fn release_gui_pool(pool: MappedGuiDvmPool) {
    release_gui_mappings(pool.control, pool.doorbell);
}

pub(super) fn release_gui_mappings(mapped: *mut u8, doorbell: *mut u8) {
    if !doorbell.is_null() {
        crate::driver::mmio::unmap(doorbell.cast());
    }
    if !mapped.is_null() {
        crate::driver::mmio::unmap(mapped.cast());
    }
}

