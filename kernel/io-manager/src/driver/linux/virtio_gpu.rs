// RING3-MIGRATION-REFERENCE START: Linux .ko virtio-gpu substrate exception.
// driverd owns provider selection; uiserver owns presentation policy. Ring0
// keeps PCI/MMIO/DMA virtio-gpu 2D command execution and provider publish.
#![allow(dead_code)]

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use driver_abi::{
    DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER, DisplayFramebufferRegistration, DisplayPixelFormat,
};

use crate::sync::KernelWaitLock;

const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL: u16 = 0x1010;
const PCI_DEVICE_VIRTIO_MODERN_BASE: u16 = 0x1040;
const PCI_DEVICE_VIRTIO_MODERN_END: u16 = 0x107f;
const VIRTIO_DEVICE_ID_GPU: u32 = 16;

const PCI_STATUS_OFFSET: u8 = 0x06;
const PCI_CAPABILITY_LIST: u16 = 1 << 4;
const PCI_CAPABILITY_POINTER_OFFSET: u8 = 0x34;
const PCI_CAP_ID_VENDOR: u8 = 0x09;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

const COMMON_DEVICE_FEATURE_SELECT: usize = 0;
const COMMON_DEVICE_FEATURE: usize = 4;
const COMMON_DRIVER_FEATURE_SELECT: usize = 8;
const COMMON_DRIVER_FEATURE: usize = 12;
const COMMON_NUM_QUEUES: usize = 18;
const COMMON_DEVICE_STATUS: usize = 20;
const COMMON_QUEUE_SELECT: usize = 22;
const COMMON_QUEUE_SIZE: usize = 24;
const COMMON_QUEUE_ENABLE: usize = 28;
const COMMON_QUEUE_NOTIFY_OFF: usize = 30;
const COMMON_QUEUE_DESC: usize = 32;
const COMMON_QUEUE_DRIVER: usize = 40;
const COMMON_QUEUE_DEVICE: usize = 48;

const QUEUE_SIZE: u16 = 8;
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

const PRIMARY_RESOURCE_ID: u32 = 1;
const PRIMARY_SCANOUT_ID: u32 = 0;
const MAX_DIMENSION: u32 = 7680;
const DEFAULT_WIDTH: u32 = 1600;
const DEFAULT_HEIGHT: u32 = 900;
const INIT_COMMAND_POLL_BUDGET: usize = 2_000_000;
const PRESENT_COMMAND_POLL_BUDGET: usize = 128;
const COMMAND_DMA_LEN: usize = 512;

static PROVIDER_READY: AtomicBool = AtomicBool::new(false);
static PROVIDER_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static FLUSH_BUSY: AtomicBool = AtomicBool::new(false);
static FLUSH_FAILURES: AtomicU32 = AtomicU32::new(0);
static STATE: KernelWaitLock<Option<VirtioGpuState>> = KernelWaitLock::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScanoutFlushRect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VirtioGpuError {
    AlreadyReady,
    NoDevice,
    MissingCommonConfig,
    MissingNotifyConfig,
    InvalidCapability,
    MmioMapFailed,
    QueueUnavailable,
    DmaAllocationFailed,
    DeviceRejectedFeatures,
    CommandFailed,
    ProviderRejected,
    GeometryInvalid,
}

struct VirtioGpuState {
    _pci: crate::arch::pci::PciDevice,
    common: MmioRegion,
    notify: MmioRegion,
    notify_multiplier: u32,
    queue_notify_off: u16,
    queue: VirtQueue,
    framebuffer_cpu: usize,
    framebuffer_dma: u64,
    framebuffer_len: usize,
    width: u32,
    height: u32,
    stride_pixels: u32,
    command_request: DmaBlock,
    command_response: DmaBlock,
}

unsafe impl Send for VirtioGpuState {}

struct MmioRegion {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for MmioRegion {}

struct DmaBlock {
    cpu: *mut u8,
    dma: u64,
    len: usize,
}

unsafe impl Send for DmaBlock {}

impl Drop for DmaBlock {
    fn drop(&mut self) {
        if self.cpu.is_null() {
            return;
        }
        crate::driver::dma::free_coherent(
            core::ptr::null_mut(),
            self.cpu.cast::<c_void>(),
            self.dma,
        );
    }
}

struct VirtQueue {
    desc: DmaBlock,
    avail: DmaBlock,
    used: DmaBlock,
    next_avail: u16,
    last_used: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuCtrlHdr {
    type_: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuDisplayOne {
    rect: GpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuRespDisplayInfo {
    hdr: GpuCtrlHdr,
    pmodes: [GpuDisplayOne; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuResourceCreate2d {
    hdr: GpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuResourceAttachBacking {
    hdr: GpuCtrlHdr,
    resource_id: u32,
    nr_entries: u32,
    entry: GpuMemEntry,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuSetScanout {
    hdr: GpuCtrlHdr,
    rect: GpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuTransferToHost2d {
    hdr: GpuCtrlHdr,
    rect: GpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GpuResourceFlush {
    hdr: GpuCtrlHdr,
    rect: GpuRect,
    resource_id: u32,
    padding: u32,
}

pub(crate) fn provider_ready() -> bool {
    PROVIDER_READY.load(Ordering::Acquire)
}

pub(crate) fn ensure_primary_provider() -> Result<(), VirtioGpuError> {
    if provider_ready() {
        return Ok(());
    }
    if PROVIDER_ATTEMPTED.swap(true, Ordering::AcqRel) {
        return Err(VirtioGpuError::NoDevice);
    }
    let state = initialize_primary_provider()?;
    *STATE.lock() = Some(state);
    PROVIDER_READY.store(true, Ordering::Release);
    Ok(())
}

pub(crate) fn flush_primary_scanout() -> bool {
    flush_primary_scanout_rect(None)
}

pub(crate) fn flush_primary_scanout_rect(rect: Option<ScanoutFlushRect>) -> bool {
    if !provider_ready() {
        return true;
    }
    if FLUSH_BUSY.swap(true, Ordering::AcqRel) {
        return true;
    }
    let result = {
        let Some(mut guard) = STATE.try_lock() else {
            FLUSH_BUSY.store(false, Ordering::Release);
            return true;
        };
        let Some(state) = guard.as_mut() else {
            FLUSH_BUSY.store(false, Ordering::Release);
            return false;
        };
        state.flush(rect)
    };
    FLUSH_BUSY.store(false, Ordering::Release);
    match result {
        Ok(()) => true,
        Err(err) => {
            if FLUSH_FAILURES.fetch_add(1, Ordering::Relaxed) < 8 {
                crate::debug::warn!(display, "virtio-gpu: primary flush failed: {:?}", err);
            }
            true
        }
    }
}

fn initialize_primary_provider() -> Result<VirtioGpuState, VirtioGpuError> {
    let pci = find_virtio_gpu_device().ok_or(VirtioGpuError::NoDevice)?;
    pci.enable_memory_bus_master();
    let caps = discover_caps(pci)?;
    crate::driver::dma::set_mask_and_coherent(core::ptr::null_mut(), u64::MAX);

    let common = map_cap(pci, caps.common).ok_or(VirtioGpuError::MmioMapFailed)?;
    let notify = map_cap(pci, caps.notify).ok_or(VirtioGpuError::MmioMapFailed)?;
    let notify_multiplier = caps.notify_multiplier.max(2);

    reset_device(&common);
    write_common_u8(&common, COMMON_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
    write_common_u8(
        &common,
        COMMON_DEVICE_STATUS,
        VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
    );
    write_common_u32(&common, COMMON_DEVICE_FEATURE_SELECT, 0);
    let _features0 = read_common_u32(&common, COMMON_DEVICE_FEATURE);
    write_common_u32(&common, COMMON_DRIVER_FEATURE_SELECT, 0);
    write_common_u32(&common, COMMON_DRIVER_FEATURE, 0);
    write_common_u32(&common, COMMON_DEVICE_FEATURE_SELECT, 1);
    let _features1 = read_common_u32(&common, COMMON_DEVICE_FEATURE);
    write_common_u32(&common, COMMON_DRIVER_FEATURE_SELECT, 1);
    write_common_u32(&common, COMMON_DRIVER_FEATURE, 0);
    write_common_u8(
        &common,
        COMMON_DEVICE_STATUS,
        VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
    );
    if read_common_u8(&common, COMMON_DEVICE_STATUS) & VIRTIO_STATUS_FEATURES_OK == 0 {
        return Err(VirtioGpuError::DeviceRejectedFeatures);
    }

    let mut queue = setup_queue(&common, 0)?;
    let queue_notify_off = read_common_u16(&common, COMMON_QUEUE_NOTIFY_OFF);
    let command_request = alloc_dma(COMMAND_DMA_LEN)?;
    let command_response = alloc_dma(COMMAND_DMA_LEN)?;
    write_common_u8(
        &common,
        COMMON_DEVICE_STATUS,
        VIRTIO_STATUS_ACKNOWLEDGE
            | VIRTIO_STATUS_DRIVER
            | VIRTIO_STATUS_FEATURES_OK
            | VIRTIO_STATUS_DRIVER_OK,
    );

    let mut state = VirtioGpuState {
        _pci: pci,
        common,
        notify,
        notify_multiplier,
        queue_notify_off,
        queue,
        framebuffer_cpu: 0,
        framebuffer_dma: 0,
        framebuffer_len: 0,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        stride_pixels: DEFAULT_WIDTH,
        command_request,
        command_response,
    };
    let (width, height) = preferred_geometry(state.display_geometry().ok());
    state.create_primary_resource(width, height)?;
    state.publish_provider()?;
    crate::debug::info!(
        display,
        "virtio-gpu: primary provider published width={} height={} stride={}",
        state.width,
        state.height,
        state.stride_pixels
    );
    // `debug::info!(display, ...)` can be filtered from the early debugcon
    // stream. Keep one deterministic, bounded readiness marker so the KVM
    // hardware-profile gate proves provider publication rather than merely a
    // successful `.ko` load.
    crate::debug::println!(
        "virtio-gpu: primary provider published width={} height={} stride={}",
        state.width,
        state.height,
        state.stride_pixels
    );
    Ok(state)
}

fn find_virtio_gpu_device() -> Option<crate::arch::pci::PciDevice> {
    let mut found = None;
    crate::arch::pci::visit_devices(|pci| {
        if pci.vendor_id() != PCI_VENDOR_VIRTIO {
            return false;
        }
        if virtio_device_type(pci.device_id()) != Some(VIRTIO_DEVICE_ID_GPU) {
            return false;
        }
        found = Some(pci);
        true
    });
    found
}

fn virtio_device_type(device_id: u16) -> Option<u32> {
    match device_id {
        PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL => Some(VIRTIO_DEVICE_ID_GPU),
        PCI_DEVICE_VIRTIO_MODERN_BASE..=PCI_DEVICE_VIRTIO_MODERN_END => {
            Some(u32::from(device_id - PCI_DEVICE_VIRTIO_MODERN_BASE))
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct VirtioPciCap {
    bar: u8,
    offset: u32,
    length: u32,
}

#[derive(Clone, Copy)]
struct VirtioGpuCaps {
    common: VirtioPciCap,
    notify: VirtioPciCap,
    notify_multiplier: u32,
}

fn discover_caps(pci: crate::arch::pci::PciDevice) -> Result<VirtioGpuCaps, VirtioGpuError> {
    if pci.read_u16(PCI_STATUS_OFFSET) & PCI_CAPABILITY_LIST == 0 {
        return Err(VirtioGpuError::InvalidCapability);
    }
    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut cap = pci.read_u8(PCI_CAPABILITY_POINTER_OFFSET) & !0x3;
    let mut guard = 0;
    while cap >= 0x40 && guard < 48 {
        guard += 1;
        let cap_id = pci.read_u8(cap);
        let next = pci.read_u8(cap.wrapping_add(1)) & !0x3;
        if cap_id == PCI_CAP_ID_VENDOR {
            let cfg_type = pci.read_u8(cap.wrapping_add(3));
            let bar = pci.read_u8(cap.wrapping_add(4));
            let offset = pci.read_u32(cap.wrapping_add(8));
            let length = pci.read_u32(cap.wrapping_add(12));
            let virtio_cap = VirtioPciCap {
                bar,
                offset,
                length,
            };
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG => common = Some(virtio_cap),
                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                    notify = Some(virtio_cap);
                    notify_multiplier = pci.read_u32(cap.wrapping_add(16));
                }
                _ => {}
            }
        }
        if next == 0 || next == cap {
            break;
        }
        cap = next;
    }
    Ok(VirtioGpuCaps {
        common: common.ok_or(VirtioGpuError::MissingCommonConfig)?,
        notify: notify.ok_or(VirtioGpuError::MissingNotifyConfig)?,
        notify_multiplier,
    })
}

fn map_cap(pci: crate::arch::pci::PciDevice, cap: VirtioPciCap) -> Option<MmioRegion> {
    let resource = pci.resource(cap.bar as usize)?;
    if resource.is_io || cap.length == 0 {
        return None;
    }
    let start = resource.start.checked_add(cap.offset as u64)?;
    let len = cap.length as usize;
    let ptr = crate::driver::mmio::map(start, len, false).cast::<u8>();
    if ptr.is_null() {
        return None;
    }
    Some(MmioRegion { ptr, len })
}

fn setup_queue(common: &MmioRegion, index: u16) -> Result<VirtQueue, VirtioGpuError> {
    write_common_u16(common, COMMON_QUEUE_SELECT, index);
    let device_queue_size = read_common_u16(common, COMMON_QUEUE_SIZE);
    if device_queue_size < 2 {
        return Err(VirtioGpuError::QueueUnavailable);
    }
    let queue_size = device_queue_size.min(QUEUE_SIZE);
    let desc_len = queue_size as usize * size_of::<VirtqDesc>();
    let avail_len = 6 + queue_size as usize * size_of::<u16>();
    let used_len = 6 + queue_size as usize * size_of::<VirtqUsedElem>();
    let desc = alloc_dma(desc_len)?;
    let avail = alloc_dma(avail_len)?;
    let used = alloc_dma(used_len)?;
    write_common_u64(common, COMMON_QUEUE_DESC, desc.dma);
    write_common_u64(common, COMMON_QUEUE_DRIVER, avail.dma);
    write_common_u64(common, COMMON_QUEUE_DEVICE, used.dma);
    write_common_u16(common, COMMON_QUEUE_SIZE, queue_size);
    write_common_u16(common, COMMON_QUEUE_ENABLE, 1);
    Ok(VirtQueue {
        desc,
        avail,
        used,
        next_avail: 0,
        last_used: 0,
    })
}

impl VirtioGpuState {
    fn display_geometry(&mut self) -> Result<(u32, u32), VirtioGpuError> {
        let request = GpuCtrlHdr {
            type_: VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            ..GpuCtrlHdr::default()
        };
        let mut response = GpuRespDisplayInfo {
            hdr: GpuCtrlHdr::default(),
            pmodes: [GpuDisplayOne {
                rect: GpuRect::default(),
                enabled: 0,
                flags: 0,
            }; 16],
        };
        self.command(&request, &mut response, INIT_COMMAND_POLL_BUDGET)?;
        if response.hdr.type_ != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            return Err(VirtioGpuError::CommandFailed);
        }
        for mode in response.pmodes {
            if mode.enabled != 0 && valid_geometry(mode.rect.width, mode.rect.height) {
                return Ok((mode.rect.width, mode.rect.height));
            }
        }
        Ok((DEFAULT_WIDTH, DEFAULT_HEIGHT))
    }

    fn create_primary_resource(&mut self, width: u32, height: u32) -> Result<(), VirtioGpuError> {
        if !valid_geometry(width, height) {
            return Err(VirtioGpuError::GeometryInvalid);
        }
        let stride_pixels = width;
        let len = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(VirtioGpuError::GeometryInvalid)? as usize;
        let framebuffer = alloc_dma(len)?;
        unsafe {
            ptr::write_bytes(framebuffer.cpu, 0, framebuffer.len);
        }

        let create = GpuResourceCreate2d {
            hdr: GpuCtrlHdr {
                type_: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                ..GpuCtrlHdr::default()
            },
            resource_id: PRIMARY_RESOURCE_ID,
            format: VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            width,
            height,
        };
        self.command_ok(&create, INIT_COMMAND_POLL_BUDGET)?;

        let attach = GpuResourceAttachBacking {
            hdr: GpuCtrlHdr {
                type_: VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                ..GpuCtrlHdr::default()
            },
            resource_id: PRIMARY_RESOURCE_ID,
            nr_entries: 1,
            entry: GpuMemEntry {
                addr: framebuffer.dma,
                length: len as u32,
                padding: 0,
            },
        };
        self.command_ok(&attach, INIT_COMMAND_POLL_BUDGET)?;

        let rect = GpuRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let set_scanout = GpuSetScanout {
            hdr: GpuCtrlHdr {
                type_: VIRTIO_GPU_CMD_SET_SCANOUT,
                ..GpuCtrlHdr::default()
            },
            rect,
            scanout_id: PRIMARY_SCANOUT_ID,
            resource_id: PRIMARY_RESOURCE_ID,
        };
        self.command_ok(&set_scanout, INIT_COMMAND_POLL_BUDGET)?;

        self.framebuffer_cpu = framebuffer.cpu as usize;
        self.framebuffer_dma = framebuffer.dma;
        self.framebuffer_len = framebuffer.len;
        self.width = width;
        self.height = height;
        self.stride_pixels = stride_pixels;
        self.flush_with_budget(INIT_COMMAND_POLL_BUDGET)?;
        core::mem::forget(framebuffer);
        Ok(())
    }

    fn publish_provider(&self) -> Result<(), VirtioGpuError> {
        let registration = DisplayFramebufferRegistration {
            addr: self.framebuffer_cpu as u64,
            size: self.framebuffer_len as u64,
            back_buffer_addr: 0,
            back_buffer_size: 0,
            width: self.width,
            height: self.height,
            stride: self.stride_pixels,
            pixel_format: DisplayPixelFormat::Bgr as u32,
            bytes_per_pixel: 4,
            flags: DISPLAY_FRAMEBUFFER_FLAG_PRIMARY_PROVIDER,
            reserved: [0; 2],
        };
        let status = unsafe { crate::io::gui::register_driver_framebuffer(&registration) };
        if status != 0 {
            return Err(VirtioGpuError::ProviderRejected);
        }
        Ok(())
    }

    fn flush(&mut self, rect: Option<ScanoutFlushRect>) -> Result<(), VirtioGpuError> {
        self.flush_rect_with_budget(
            rect.unwrap_or(ScanoutFlushRect {
                x: 0,
                y: 0,
                width: self.width as usize,
                height: self.height as usize,
            }),
            PRESENT_COMMAND_POLL_BUDGET,
        )
    }

    fn flush_with_budget(&mut self, poll_budget: usize) -> Result<(), VirtioGpuError> {
        self.flush_rect_with_budget(
            ScanoutFlushRect {
                x: 0,
                y: 0,
                width: self.width as usize,
                height: self.height as usize,
            },
            poll_budget,
        )
    }

    fn flush_rect_with_budget(
        &mut self,
        rect: ScanoutFlushRect,
        poll_budget: usize,
    ) -> Result<(), VirtioGpuError> {
        if self.framebuffer_len == 0 {
            return Ok(());
        }
        let Some(rect) = clamp_flush_rect(rect, self.width, self.height) else {
            return Ok(());
        };
        let row = rect
            .y
            .checked_mul(self.stride_pixels as usize)
            .ok_or(VirtioGpuError::GeometryInvalid)?;
        let pixels = row
            .checked_add(rect.x)
            .ok_or(VirtioGpuError::GeometryInvalid)?;
        let offset = pixels
            .checked_mul(4)
            .ok_or(VirtioGpuError::GeometryInvalid)?;
        let gpu_rect = GpuRect {
            x: rect.x as u32,
            y: rect.y as u32,
            width: rect.width as u32,
            height: rect.height as u32,
        };
        let transfer = GpuTransferToHost2d {
            hdr: GpuCtrlHdr {
                type_: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                ..GpuCtrlHdr::default()
            },
            rect: gpu_rect,
            offset: offset as u64,
            resource_id: PRIMARY_RESOURCE_ID,
            padding: 0,
        };
        self.command_ok(&transfer, poll_budget)?;
        let flush = GpuResourceFlush {
            hdr: GpuCtrlHdr {
                type_: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                ..GpuCtrlHdr::default()
            },
            rect: gpu_rect,
            resource_id: PRIMARY_RESOURCE_ID,
            padding: 0,
        };
        self.command_ok(&flush, poll_budget)
    }

    fn command_ok<T>(&mut self, request: &T, poll_budget: usize) -> Result<(), VirtioGpuError> {
        let mut response = GpuCtrlHdr::default();
        self.command(request, &mut response, poll_budget)?;
        if response.type_ == VIRTIO_GPU_RESP_OK_NODATA {
            Ok(())
        } else {
            Err(VirtioGpuError::CommandFailed)
        }
    }

    fn command<T, R>(
        &mut self,
        request: &T,
        response: &mut R,
        poll_budget: usize,
    ) -> Result<(), VirtioGpuError> {
        if nucleus_core::util::fault_injection::should_fail("virtio-gpu.control.submit") {
            return Err(VirtioGpuError::CommandFailed);
        }
        let request_len = size_of::<T>();
        let response_len = size_of::<R>();
        if request_len > self.command_request.len || response_len > self.command_response.len {
            return Err(VirtioGpuError::CommandFailed);
        }
        self.retire_outstanding_or_busy(poll_budget)?;
        unsafe {
            ptr::copy_nonoverlapping(
                (request as *const T).cast::<u8>(),
                self.command_request.cpu,
                request_len,
            );
            ptr::write_bytes(self.command_response.cpu, 0, response_len);
        }
        self.submit_two_desc(request_len, response_len, poll_budget)?;
        unsafe {
            ptr::copy_nonoverlapping(
                self.command_response.cpu,
                (response as *mut R).cast::<u8>(),
                response_len,
            );
        }
        Ok(())
    }

    fn submit_two_desc(
        &mut self,
        request_len: usize,
        response_len: usize,
        poll_budget: usize,
    ) -> Result<(), VirtioGpuError> {
        unsafe {
            let desc = self.queue.desc.cpu.cast::<VirtqDesc>();
            ptr::write(
                desc,
                VirtqDesc {
                    addr: self.command_request.dma,
                    len: request_len as u32,
                    flags: VIRTQ_DESC_F_NEXT,
                    next: 1,
                },
            );
            ptr::write(
                desc.add(1),
                VirtqDesc {
                    addr: self.command_response.dma,
                    len: response_len as u32,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );

            let avail = self.queue.avail.cpu;
            let ring = avail.add(4).cast::<u16>();
            ptr::write_volatile(
                ring.add((self.queue.next_avail as usize) % QUEUE_SIZE as usize),
                0,
            );
            self.queue.next_avail = self.queue.next_avail.wrapping_add(1);
            ptr::write_volatile(avail.add(2).cast::<u16>(), self.queue.next_avail);
        }
        self.notify_queue();
        self.poll_used(poll_budget)
    }

    fn retire_outstanding_or_busy(&mut self, poll_budget: usize) -> Result<(), VirtioGpuError> {
        if self.queue.last_used == self.queue.next_avail {
            return Ok(());
        }
        self.poll_used(poll_budget)
    }

    fn notify_queue(&self) {
        let offset =
            (self.queue_notify_off as usize).saturating_mul(self.notify_multiplier as usize);
        if offset + size_of::<u16>() > self.notify.len {
            return;
        }
        unsafe {
            ptr::write_volatile(self.notify.ptr.add(offset).cast::<u16>(), 0);
        }
    }

    fn poll_used(&mut self, poll_budget: usize) -> Result<(), VirtioGpuError> {
        for iteration in 0..poll_budget {
            let used_idx = unsafe { ptr::read_volatile(self.queue.used.cpu.add(2).cast::<u16>()) };
            if used_idx != self.queue.last_used {
                self.queue.last_used = self.queue.last_used.wrapping_add(1);
                return Ok(());
            }
            if iteration & 0xff == 0xff {
                crate::multitask::cond_resched();
            }
            core::hint::spin_loop();
        }
        Err(VirtioGpuError::CommandFailed)
    }
}

fn valid_geometry(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION
}

fn preferred_geometry(device_geometry: Option<(u32, u32)>) -> (u32, u32) {
    let Some((device_width, device_height)) = device_geometry else {
        return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
    };
    if !valid_geometry(device_width, device_height) {
        return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
    }
    let preferred_area = DEFAULT_WIDTH.saturating_mul(DEFAULT_HEIGHT);
    let device_area = device_width.saturating_mul(device_height);
    if device_area >= preferred_area {
        (device_width, device_height)
    } else {
        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }
}

fn clamp_flush_rect(rect: ScanoutFlushRect, width: u32, height: u32) -> Option<ScanoutFlushRect> {
    let x0 = rect.x.min(width as usize);
    let y0 = rect.y.min(height as usize);
    let x1 = rect.x.saturating_add(rect.width).min(width as usize);
    let y1 = rect.y.saturating_add(rect.height).min(height as usize);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(ScanoutFlushRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

fn alloc_dma(len: usize) -> Result<DmaBlock, VirtioGpuError> {
    let len = len.max(8);
    let mut dma = 0_u64;
    let cpu = crate::driver::dma::alloc_coherent(core::ptr::null_mut(), len, &mut dma);
    if cpu.is_null() || dma == crate::driver::dma::DMA_MAPPING_ERROR {
        return Err(VirtioGpuError::DmaAllocationFailed);
    }
    Ok(DmaBlock {
        cpu: cpu.cast::<u8>(),
        dma,
        len,
    })
}

fn reset_device(common: &MmioRegion) {
    write_common_u8(common, COMMON_DEVICE_STATUS, 0);
    for _ in 0..10_000 {
        if read_common_u8(common, COMMON_DEVICE_STATUS) == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn read_common_u8(common: &MmioRegion, offset: usize) -> u8 {
    if offset >= common.len {
        return 0;
    }
    unsafe { ptr::read_volatile(common.ptr.add(offset)) }
}

fn read_common_u16(common: &MmioRegion, offset: usize) -> u16 {
    if offset + size_of::<u16>() > common.len {
        return 0;
    }
    unsafe { ptr::read_volatile(common.ptr.add(offset).cast::<u16>()) }
}

fn read_common_u32(common: &MmioRegion, offset: usize) -> u32 {
    if offset + size_of::<u32>() > common.len {
        return 0;
    }
    unsafe { ptr::read_volatile(common.ptr.add(offset).cast::<u32>()) }
}

fn write_common_u8(common: &MmioRegion, offset: usize, value: u8) {
    if offset >= common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset), value) }
}

fn write_common_u16(common: &MmioRegion, offset: usize, value: u16) {
    if offset + size_of::<u16>() > common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset).cast::<u16>(), value) }
}

fn write_common_u32(common: &MmioRegion, offset: usize, value: u32) {
    if offset + size_of::<u32>() > common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset).cast::<u32>(), value) }
}

fn write_common_u64(common: &MmioRegion, offset: usize, value: u64) {
    if offset + size_of::<u64>() > common.len {
        return;
    }
    unsafe { ptr::write_volatile(common.ptr.add(offset).cast::<u64>(), value) }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko virtio-gpu substrate exception.
