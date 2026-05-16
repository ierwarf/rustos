use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use crate::sync::KernelSpinLock as Mutex;
use boot_protocol::{
    BootPixelFormat, FramebufferInfo, MAX_BOOT_FRAMEBUFFER_HEIGHT, MAX_BOOT_FRAMEBUFFER_WIDTH,
};
use x86_64::instructions::interrupts;

const PCI_VENDOR_VIRTIO: u16 = 0x1af4;
const PCI_DEVICE_VIRTIO_GPU_MODERN: u16 = 0x1050;
const PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL: u16 = 0x1010;
const PCI_CAP_STATUS: u8 = 0x06;
const PCI_CAP_POINTER: u8 = 0x34;
const PCI_STATUS_CAP_LIST: u16 = 1 << 4;
const PCI_CAP_ID_VENDOR: u8 = 0x09;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_FAILED: u8 = 128;
const VIRTIO_F_VERSION_1: u32 = 32;

const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE: usize = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
const COMMON_DRIVER_FEATURE: usize = 0x0c;
const COMMON_NUM_QUEUES: usize = 0x12;
const COMMON_DEVICE_STATUS: usize = 0x14;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_SIZE: usize = 0x18;
const COMMON_QUEUE_ENABLE: usize = 0x1c;
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1e;
const COMMON_QUEUE_DESC: usize = 0x20;
const COMMON_QUEUE_DRIVER: usize = 0x28;
const COMMON_QUEUE_DEVICE: usize = 0x30;

const CONTROL_QUEUE_INDEX: u16 = 0;
const QUEUE_SIZE: u16 = 8;
const QUEUE_MEM_SIZE: usize = virtio_drivers::PAGE_SIZE;
const COMMAND_MEM_SIZE: usize = 4096;
const COMMAND_REQUEST_OFFSET: usize = 0;
const COMMAND_RESPONSE_OFFSET: usize = 2048;
const COMMAND_RESPONSE_SIZE: usize = 1024;

const FRAMEBUFFER_WIDTH_DEFAULT: u32 = 1600;
const FRAMEBUFFER_HEIGHT_DEFAULT: u32 = 900;
const MIN_USABLE_SCANOUT_WIDTH: u32 = 1024;
const MIN_USABLE_SCANOUT_HEIGHT: u32 = 600;
const MAX_SCANOUT_WIDTH: u32 = MAX_BOOT_FRAMEBUFFER_WIDTH;
const MAX_SCANOUT_HEIGHT: u32 = MAX_BOOT_FRAMEBUFFER_HEIGHT;
const BYTES_PER_PIXEL: u32 = 4;
const RESOURCE_ID_PRIMARY: u32 = 1;

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

const HDR_SIZE: usize = 24;
const RECT_SIZE: usize = 16;
const DISPLAY_INFO_SCANOUT_COUNT: usize = 16;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const COMMAND_COMPLETION_TIMEOUT_MS: u64 = 250;
const COMMAND_COMPLETION_IRQ_OFF_SPINS: usize = 50_000;
const COMMAND_COMPLETION_BOOT_SPINS: usize = 1_000_000;
const COMMAND_TIMEOUT_BACKOFF_BASE_MS: u64 = 100;
const COMMAND_TIMEOUT_BACKOFF_MAX_MS: u64 = 1_000;

static DISPLAY: Mutex<Option<VirtioGpuDisplay>> = Mutex::new(None);
static DISPLAY_INITIALIZING: AtomicBool = AtomicBool::new(false);
static DISPLAY_NEEDS_FULL_FLUSH: AtomicBool = AtomicBool::new(false);
static PENDING_FLUSH: Mutex<PendingFlush> = Mutex::new(PendingFlush::empty());

#[derive(Clone, Copy)]
struct FlushRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl FlushRect {
    fn union(self, other: Self) -> Self {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self
            .x
            .saturating_add(self.width)
            .max(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .max(other.y.saturating_add(other.height));
        Self {
            x: x0,
            y: y0,
            width: x1.saturating_sub(x0),
            height: y1.saturating_sub(y0),
        }
    }
}

#[derive(Clone, Copy)]
enum FlushRequest {
    Full,
    Rect(FlushRect),
}

struct PendingFlush {
    full: bool,
    rect: Option<FlushRect>,
}

impl PendingFlush {
    const fn empty() -> Self {
        Self {
            full: false,
            rect: None,
        }
    }

    fn request_full(&mut self) {
        self.full = true;
        self.rect = None;
    }

    fn add_rect(&mut self, rect: FlushRect) {
        if self.full || rect.width == 0 || rect.height == 0 {
            return;
        }
        self.rect = Some(match self.rect {
            Some(existing) => existing.union(rect),
            None => rect,
        });
    }

    fn take_request(&mut self) -> Option<FlushRequest> {
        if self.full {
            self.full = false;
            self.rect = None;
            return Some(FlushRequest::Full);
        }
        self.rect.take().map(FlushRequest::Rect)
    }

    fn requeue(&mut self, request: FlushRequest) {
        match request {
            FlushRequest::Full => self.request_full(),
            FlushRequest::Rect(rect) => self.add_rect(rect),
        }
    }
}

#[derive(Clone, Copy)]
struct VirtioGpuPciCaps {
    common: MmioRegion,
    notify: MmioRegion,
    notify_multiplier: u32,
    device_config: Option<MmioRegion>,
}

#[derive(Clone, Copy)]
struct MmioRegion {
    bar: usize,
    offset: u64,
    length: usize,
}

struct VirtioGpuDisplay {
    _common: *mut u8,
    notify: *mut u8,
    _device_config: *mut u8,
    notify_offset: usize,
    queue: VirtQueue,
    command_cpu: *mut u8,
    command_dma: u64,
    framebuffer_cpu: *mut u8,
    _framebuffer_dma: u64,
    framebuffer_size: usize,
    width: u32,
    height: u32,
    stride_pixels: u32,
    flush_backoff_until_tick: u64,
    flush_timeout_count: u32,
    needs_full_flush: bool,
}

unsafe impl Send for VirtioGpuDisplay {}

struct VirtQueue {
    queue_size: u16,
    notify_off: u16,
    mem_cpu: *mut u8,
    _mem_dma: u64,
    desc_offset: usize,
    avail_offset: usize,
    used_offset: usize,
    avail_idx: u16,
    used_idx: u16,
}

impl VirtioGpuDisplay {
    fn flush_full_frame(&mut self) -> bool {
        self.flush_region(0, 0, self.width, self.height, true)
    }

    fn flush_rect(&mut self, x: u32, y: u32, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 || x >= self.width || y >= self.height {
            return true;
        }
        let width = width.min(self.width - x);
        let height = height.min(self.height - y);
        if self.needs_full_flush {
            return self.flush_full_frame();
        }
        self.flush_region(x, y, width, height, false)
    }

    fn flush_region(&mut self, x: u32, y: u32, width: u32, height: u32, full_frame: bool) -> bool {
        if self.flush_backoff_active() {
            self.needs_full_flush = true;
            return false;
        }
        if !self.transfer_to_host_2d(x, y, width, height) {
            self.needs_full_flush = true;
            return false;
        }
        if !self.resource_flush(x, y, width, height) {
            self.needs_full_flush = true;
            return false;
        }
        if full_frame {
            self.needs_full_flush = false;
        }
        true
    }

    fn transfer_to_host_2d(&mut self, x: u32, y: u32, width: u32, height: u32) -> bool {
        let offset = self.framebuffer_offset(x, y);
        self.command(
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
            56,
            VIRTIO_GPU_RESP_OK_NODATA,
            |buf, _, _| {
                write_rect(buf, HDR_SIZE, x, y, width, height);
                write_u64(buf, HDR_SIZE + RECT_SIZE, offset);
                write_u32(buf, HDR_SIZE + RECT_SIZE + 8, RESOURCE_ID_PRIMARY);
                write_u32(buf, HDR_SIZE + RECT_SIZE + 12, 0);
            },
        )
    }

    fn resource_flush(&mut self, x: u32, y: u32, width: u32, height: u32) -> bool {
        self.command(
            VIRTIO_GPU_CMD_RESOURCE_FLUSH,
            48,
            VIRTIO_GPU_RESP_OK_NODATA,
            |buf, _, _| {
                write_rect(buf, HDR_SIZE, x, y, width, height);
                write_u32(buf, HDR_SIZE + RECT_SIZE, RESOURCE_ID_PRIMARY);
                write_u32(buf, HDR_SIZE + RECT_SIZE + 4, 0);
            },
        )
    }

    fn framebuffer_offset(&self, x: u32, y: u32) -> u64 {
        u64::from(y)
            .saturating_mul(u64::from(self.stride_pixels))
            .saturating_add(u64::from(x))
            .saturating_mul(u64::from(BYTES_PER_PIXEL))
    }

    fn command(
        &mut self,
        ty: u32,
        request_len: usize,
        expected_response: u32,
        fill: impl FnOnce(&mut [u8], u32, u32),
    ) -> bool {
        let request = unsafe {
            core::slice::from_raw_parts_mut(
                self.command_cpu.add(COMMAND_REQUEST_OFFSET),
                request_len,
            )
        };
        request.fill(0);
        write_u32(request, 0, ty);
        fill(request, self.width, self.height);

        let response = unsafe {
            core::slice::from_raw_parts_mut(
                self.command_cpu.add(COMMAND_RESPONSE_OFFSET),
                COMMAND_RESPONSE_SIZE,
            )
        };
        response.fill(0);

        if !self.submit_control(request_len, COMMAND_RESPONSE_SIZE) {
            trace_native("virtio-gpu-native-command-timeout", u64::from(ty), 0);
            self.record_command_timeout(ty);
            return false;
        }

        let actual_response = read_u32(response, 0);
        let matched = actual_response == expected_response;
        if matched {
            self.record_command_success();
        } else {
            trace_native(
                "virtio-gpu-native-command-bad-response",
                u64::from(ty),
                u64::from(actual_response),
            );
            self.record_command_timeout(ty);
        }
        matched
    }

    fn submit_control(&mut self, request_len: usize, response_len: usize) -> bool {
        let desc = unsafe { self.queue.mem_cpu.add(self.queue.desc_offset) };
        let avail = unsafe { self.queue.mem_cpu.add(self.queue.avail_offset) };
        let used = unsafe { self.queue.mem_cpu.add(self.queue.used_offset) };
        let head = 0u16;

        unsafe {
            write_desc(
                desc,
                0,
                self.command_dma + COMMAND_REQUEST_OFFSET as u64,
                request_len as u32,
                DESC_F_NEXT,
                1,
            );
            write_desc(
                desc,
                1,
                self.command_dma + COMMAND_RESPONSE_OFFSET as u64,
                response_len as u32,
                DESC_F_WRITE,
                0,
            );

            ptr::write_volatile(
                avail
                    .add(4 + ((self.queue.avail_idx as usize % self.queue.queue_size as usize) * 2))
                    as *mut u16,
                head,
            );
            self.queue.avail_idx = self.queue.avail_idx.wrapping_add(1);
            compiler_fence(Ordering::SeqCst);
            ptr::write_volatile(avail.add(2) as *mut u16, self.queue.avail_idx);
            compiler_fence(Ordering::SeqCst);
            ptr::write_volatile(
                self.notify.add(self.queue_notify_offset()) as *mut u16,
                CONTROL_QUEUE_INDEX,
            );
        }

        if let Some(next_used) = wait_for_queue_used(used, self.queue.used_idx) {
            let expected_next_used = self.queue.used_idx.wrapping_add(1);
            compiler_fence(Ordering::SeqCst);
            let used_index = self.queue.used_idx as usize % self.queue.queue_size as usize;
            let used_elem = unsafe { used.add(4 + used_index * 8) };
            let used_id = unsafe { ptr::read_volatile(used_elem as *const u32) };
            let used_len = unsafe { ptr::read_volatile(used_elem.add(4) as *const u32) };
            self.queue.used_idx = next_used;
            if next_used != expected_next_used
                || used_id != u32::from(head)
                || used_len > response_len as u32
            {
                trace_native(
                    "virtio-gpu-native-used-ring-invalid",
                    u64::from(used_id),
                    u64::from(used_len),
                );
                return false;
            }
            return true;
        }

        false
    }

    fn queue_notify_offset(&self) -> usize {
        self.notify_offset
    }

    fn flush_backoff_active(&self) -> bool {
        let until = self.flush_backoff_until_tick;
        if until == 0 {
            return false;
        }
        let now = crate::arch::rtc::ticks();
        now != 0 && now < until
    }

    fn record_command_success(&mut self) {
        self.flush_timeout_count = 0;
        self.flush_backoff_until_tick = 0;
    }

    fn record_command_timeout(&mut self, ty: u32) {
        self.flush_timeout_count = self.flush_timeout_count.saturating_add(1).min(8);
        let shift = self.flush_timeout_count.saturating_sub(1).min(3);
        let backoff_ms = COMMAND_TIMEOUT_BACKOFF_BASE_MS
            .saturating_mul(1_u64 << shift)
            .min(COMMAND_TIMEOUT_BACKOFF_MAX_MS);
        let now = crate::arch::rtc::ticks();
        if now != 0 {
            self.flush_backoff_until_tick =
                now.saturating_add(milliseconds_to_ticks(backoff_ms).max(1));
        }
        if self.flush_timeout_count <= 4 {
            crate::debug::println!(
                "virtio-gpu native: command timeout ty={:#x} count={} backoff_ms={}",
                ty,
                self.flush_timeout_count,
                backoff_ms
            );
        }
    }
}

fn wait_for_queue_used(used: *mut u8, current_used_idx: u16) -> Option<u16> {
    if !interrupts::are_enabled() {
        for _ in 0..COMMAND_COMPLETION_IRQ_OFF_SPINS {
            let next_used = unsafe { ptr::read_volatile(used.add(2) as *const u16) };
            if next_used != current_used_idx {
                return Some(next_used);
            }
            core::hint::spin_loop();
        }
        trace_native("virtio-gpu-native-command-irq-off-deferred", 0, 0);
        return None;
    }

    let start_ticks = crate::arch::rtc::ticks();
    if start_ticks != 0 {
        let timeout_ticks = milliseconds_to_ticks(COMMAND_COMPLETION_TIMEOUT_MS).max(1);
        let deadline = start_ticks.saturating_add(timeout_ticks);
        loop {
            let next_used = unsafe { ptr::read_volatile(used.add(2) as *const u16) };
            if next_used != current_used_idx {
                return Some(next_used);
            }
            if crate::arch::rtc::ticks() >= deadline {
                return None;
            }
            crate::arch::rtc::sleep(1);
        }
    }

    for _ in 0..COMMAND_COMPLETION_BOOT_SPINS {
        let next_used = unsafe { ptr::read_volatile(used.add(2) as *const u16) };
        if next_used != current_used_idx {
            return Some(next_used);
        }
        core::hint::spin_loop();
    }
    None
}

fn milliseconds_to_ticks(milliseconds: u64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    milliseconds
        .saturating_mul(ticks_per_second)
        .saturating_add(999)
        / 1000
}

unsafe impl Send for VirtQueue {}

pub(crate) fn try_enable_primary_display() -> bool {
    if DISPLAY.lock().is_some() {
        return true;
    }

    if DISPLAY_INITIALIZING.swap(true, Ordering::AcqRel) {
        return DISPLAY.lock().is_some();
    }

    let enabled = try_enable_primary_display_once();
    DISPLAY_INITIALIZING.store(false, Ordering::Release);
    enabled
}

fn try_enable_primary_display_once() -> bool {
    if DISPLAY.lock().is_some() {
        return true;
    }
    trace_native("virtio-gpu-native-probe", 0, 0);

    let Some(mut controller) = probe_and_init() else {
        return false;
    };

    if !controller.flush_full_frame() {
        trace_native("virtio-gpu-native-initial-flush-failed", 0, 0);
        crate::debug::println!("virtio-gpu native: initial flush failed");
        return false;
    }

    let framebuffer = FramebufferInfo {
        addr: controller.framebuffer_cpu as u64,
        size: controller.framebuffer_size as u64,
        back_buffer_addr: 0,
        back_buffer_size: 0,
        width: controller.width,
        height: controller.height,
        stride: controller.stride_pixels,
        pixel_format: BootPixelFormat::Bgr,
        bytes_per_pixel: BYTES_PER_PIXEL as u8,
        _reserved: [0; 3],
    };

    if !crate::io::gui::install_native_driver_framebuffer(framebuffer) {
        trace_native("virtio-gpu-native-register-failed", 0, 0);
        crate::debug::println!("virtio-gpu native: framebuffer registration failed");
        return false;
    }

    trace_native(
        "virtio-gpu-native-registered",
        u64::from(controller.width),
        u64::from(controller.height),
    );
    crate::debug::println!(
        "virtio-gpu native: display registered {}x{} stride={} fb={:#x} size={:#x}",
        controller.width,
        controller.height,
        controller.stride_pixels,
        controller.framebuffer_cpu as usize,
        controller.framebuffer_size
    );
    let mut display = DISPLAY.lock();
    if display.is_some() {
        return true;
    }
    *display = Some(controller);
    true
}

pub(crate) fn flush_primary() -> bool {
    if !interrupts::are_enabled() {
        DISPLAY_NEEDS_FULL_FLUSH.store(true, Ordering::Release);
        trace_native("virtio-gpu-native-flush-irq-off-deferred", 0, 0);
        return false;
    }
    let Some(mut display) = DISPLAY.try_lock() else {
        DISPLAY_NEEDS_FULL_FLUSH.store(true, Ordering::Release);
        return false;
    };
    let Some(display) = display.as_mut() else {
        return true;
    };
    if DISPLAY_NEEDS_FULL_FLUSH.swap(false, Ordering::AcqRel) {
        display.needs_full_flush = true;
    }
    display.flush_full_frame()
}

pub(crate) fn flush_primary_rect(x: u32, y: u32, width: u32, height: u32) -> bool {
    if !interrupts::are_enabled() {
        DISPLAY_NEEDS_FULL_FLUSH.store(true, Ordering::Release);
        trace_native("virtio-gpu-native-flush-rect-irq-off-deferred", 0, 0);
        return false;
    }
    let Some(mut display) = DISPLAY.try_lock() else {
        DISPLAY_NEEDS_FULL_FLUSH.store(true, Ordering::Release);
        return false;
    };
    let Some(display) = display.as_mut() else {
        return true;
    };
    if DISPLAY_NEEDS_FULL_FLUSH.swap(false, Ordering::AcqRel) {
        display.needs_full_flush = true;
    }
    display.flush_rect(x, y, width, height)
}

pub(crate) fn queue_primary_flush() -> bool {
    PENDING_FLUSH.lock().request_full();
    true
}

pub(crate) fn queue_primary_flush_rect(x: u32, y: u32, width: u32, height: u32) -> bool {
    if width == 0 || height == 0 {
        return true;
    }
    PENDING_FLUSH.lock().add_rect(FlushRect {
        x,
        y,
        width,
        height,
    });
    true
}

pub(crate) fn service_pending() -> usize {
    let request = {
        let mut pending = PENDING_FLUSH.lock();
        pending.take_request()
    }
    .or_else(|| {
        DISPLAY_NEEDS_FULL_FLUSH
            .load(Ordering::Acquire)
            .then_some(FlushRequest::Full)
    });
    let Some(request) = request else {
        return 0;
    };

    let flushed = match request {
        FlushRequest::Full => flush_primary(),
        FlushRequest::Rect(rect) => flush_primary_rect(rect.x, rect.y, rect.width, rect.height),
    };
    if !flushed {
        PENDING_FLUSH.lock().requeue(request);
    }
    1
}

fn probe_and_init() -> Option<VirtioGpuDisplay> {
    let mut found = None;
    crate::arch::pci::visit_devices(|pci| {
        if pci.vendor_id() == PCI_VENDOR_VIRTIO && pci.class_code() == 0x03 {
            crate::debug::println!(
                "virtio-gpu native: candidate {:02x}:{:02x}.{} vendor={:04x} device={:04x} class={:06x}",
                pci.bus,
                pci.device,
                pci.function,
                pci.vendor_id(),
                pci.device_id(),
                pci.class()
            );
        }
        if pci.vendor_id() == PCI_VENDOR_VIRTIO && pci_device_is_virtio_gpu(pci.device_id()) {
            trace_native(
                "virtio-gpu-native-candidate",
                pci_bdf_key(pci),
                u64::from(pci.device_id()),
            );
            found = Some(pci);
            return true;
        }
        false
    });
    let Some(pci) = found else {
        trace_native("virtio-gpu-native-no-device", 0, 0);
        crate::debug::println!("virtio-gpu native: no PCI virtio display device found");
        return None;
    };

    let Some(caps) = parse_virtio_pci_caps(pci) else {
        trace_native("virtio-gpu-native-no-modern-caps", pci_bdf_key(pci), 0);
        crate::debug::println!("virtio-gpu native: modern PCI capabilities not found");
        return None;
    };
    let Some(common) = map_region(pci, caps.common, false) else {
        trace_native("virtio-gpu-native-common-map-failed", pci_bdf_key(pci), 0);
        crate::debug::println!("virtio-gpu native: common config map failed");
        return None;
    };
    let Some(notify) = map_region(pci, caps.notify, false) else {
        trace_native("virtio-gpu-native-notify-map-failed", pci_bdf_key(pci), 0);
        crate::debug::println!("virtio-gpu native: notify config map failed");
        return None;
    };
    let device_config = caps
        .device_config
        .and_then(|region| map_region(pci, region, false))
        .unwrap_or(core::ptr::null_mut());

    pci.enable_memory_bus_master();

    unsafe {
        write_common_u8(common, COMMON_DEVICE_STATUS, 0);
        write_common_u8(
            common,
            COMMON_DEVICE_STATUS,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
        );
        if negotiate_features(common).is_none() {
            trace_native("virtio-gpu-native-feature-failed", pci_bdf_key(pci), 0);
            crate::debug::println!("virtio-gpu native: feature negotiation failed");
            return None;
        }
    }

    let Some(queue) = setup_control_queue(common) else {
        trace_native("virtio-gpu-native-queue-failed", pci_bdf_key(pci), 0);
        crate::debug::println!("virtio-gpu native: control queue setup failed");
        return None;
    };
    let Some(notify_offset) =
        checked_queue_notify_offset(queue.notify_off, caps.notify_multiplier, caps.notify.length)
    else {
        trace_native(
            "virtio-gpu-native-notify-offset-invalid",
            pci_bdf_key(pci),
            0,
        );
        crate::debug::println!("virtio-gpu native: notify offset outside mapped capability");
        unsafe {
            mark_driver_failed(common);
        }
        return None;
    };
    let Some((command_cpu, command_dma)) = alloc_dma(COMMAND_MEM_SIZE) else {
        trace_native(
            "virtio-gpu-native-command-alloc-failed",
            pci_bdf_key(pci),
            0,
        );
        crate::debug::println!("virtio-gpu native: command buffer allocation failed");
        return None;
    };

    let mut controller = VirtioGpuDisplay {
        _common: common,
        notify,
        _device_config: device_config,
        notify_offset,
        queue,
        command_cpu,
        command_dma,
        framebuffer_cpu: core::ptr::null_mut(),
        _framebuffer_dma: 0,
        framebuffer_size: 0,
        width: FRAMEBUFFER_WIDTH_DEFAULT,
        height: FRAMEBUFFER_HEIGHT_DEFAULT,
        stride_pixels: FRAMEBUFFER_WIDTH_DEFAULT,
        flush_backoff_until_tick: 0,
        flush_timeout_count: 0,
        needs_full_flush: false,
    };

    unsafe {
        mark_driver_ok(common);
    }
    trace_native("virtio-gpu-native-driver-ok", pci_bdf_key(pci), 0);

    let (width, height) = query_display_info(&mut controller)
        .and_then(usable_scanout_size)
        .unwrap_or_else(default_scanout_size);
    let stride_pixels = width;
    let Some(framebuffer_size) = framebuffer_size_bytes(width, height) else {
        trace_native(
            "virtio-gpu-native-geometry-rejected",
            u64::from(width),
            u64::from(height),
        );
        crate::debug::println!("virtio-gpu native: framebuffer geometry rejected");
        unsafe {
            mark_driver_failed(common);
        }
        return None;
    };
    let Some((framebuffer_cpu, framebuffer_dma)) = alloc_dma(framebuffer_size) else {
        trace_native(
            "virtio-gpu-native-framebuffer-alloc-failed",
            framebuffer_size as u64,
            0,
        );
        crate::debug::println!("virtio-gpu native: framebuffer allocation failed");
        unsafe {
            mark_driver_failed(common);
        }
        return None;
    };
    mark_framebuffer_write_combine(framebuffer_dma, framebuffer_size);
    unsafe {
        ptr::write_bytes(framebuffer_cpu, 0, framebuffer_size);
    }

    controller.framebuffer_cpu = framebuffer_cpu;
    controller._framebuffer_dma = framebuffer_dma;
    controller.framebuffer_size = framebuffer_size;
    controller.width = width;
    controller.height = height;
    controller.stride_pixels = stride_pixels;

    if !create_resource_2d(&mut controller) {
        trace_native("virtio-gpu-native-create-resource-failed", 0, 0);
        crate::debug::println!("virtio-gpu native: RESOURCE_CREATE_2D failed");
        unsafe {
            mark_driver_failed(common);
        }
        return None;
    }
    if !attach_backing(&mut controller, framebuffer_dma) {
        trace_native("virtio-gpu-native-attach-backing-failed", 0, 0);
        crate::debug::println!("virtio-gpu native: RESOURCE_ATTACH_BACKING failed");
        unsafe {
            mark_driver_failed(common);
        }
        return None;
    }
    if !set_scanout(&mut controller) {
        trace_native("virtio-gpu-native-set-scanout-failed", 0, 0);
        crate::debug::println!("virtio-gpu native: SET_SCANOUT failed");
        unsafe {
            mark_driver_failed(common);
        }
        return None;
    }

    Some(controller)
}

fn pci_device_is_virtio_gpu(device_id: u16) -> bool {
    matches!(
        device_id,
        PCI_DEVICE_VIRTIO_GPU_MODERN | PCI_DEVICE_VIRTIO_GPU_TRANSITIONAL
    )
}

fn checked_queue_notify_offset(
    notify_off: u16,
    notify_multiplier: u32,
    notify_region_len: usize,
) -> Option<usize> {
    let offset = (notify_off as usize).checked_mul(notify_multiplier as usize)?;
    let end = offset.checked_add(core::mem::size_of::<u16>())?;
    (end <= notify_region_len).then_some(offset)
}

fn trace_native(name: &'static str, arg0: u64, arg1: u64) {
    crate::debug::record_milestone(crate::debug::LogCategory::Display, name, arg0, arg1);
}

fn pci_bdf_key(pci: crate::arch::pci::PciDevice) -> u64 {
    (u64::from(pci.bus) << 16) | (u64::from(pci.device) << 8) | u64::from(pci.function)
}

fn create_resource_2d(controller: &mut VirtioGpuDisplay) -> bool {
    controller.command(
        VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
        40,
        VIRTIO_GPU_RESP_OK_NODATA,
        |buf, width, height| {
            write_u32(buf, HDR_SIZE, RESOURCE_ID_PRIMARY);
            write_u32(buf, HDR_SIZE + 4, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM);
            write_u32(buf, HDR_SIZE + 8, width);
            write_u32(buf, HDR_SIZE + 12, height);
        },
    )
}

fn attach_backing(controller: &mut VirtioGpuDisplay, framebuffer_dma: u64) -> bool {
    let framebuffer_size = controller.framebuffer_size as u32;
    controller.command(
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
        48,
        VIRTIO_GPU_RESP_OK_NODATA,
        |buf, _, _| {
            write_u32(buf, HDR_SIZE, RESOURCE_ID_PRIMARY);
            write_u32(buf, HDR_SIZE + 4, 1);
            write_u64(buf, HDR_SIZE + 8, framebuffer_dma);
            write_u32(buf, HDR_SIZE + 16, framebuffer_size);
            write_u32(buf, HDR_SIZE + 20, 0);
        },
    )
}

fn set_scanout(controller: &mut VirtioGpuDisplay) -> bool {
    controller.command(
        VIRTIO_GPU_CMD_SET_SCANOUT,
        48,
        VIRTIO_GPU_RESP_OK_NODATA,
        |buf, width, height| {
            write_rect(buf, HDR_SIZE, 0, 0, width, height);
            write_u32(buf, HDR_SIZE + RECT_SIZE, 0);
            write_u32(buf, HDR_SIZE + RECT_SIZE + 4, RESOURCE_ID_PRIMARY);
        },
    )
}

fn query_display_info(controller: &mut VirtioGpuDisplay) -> Option<(u32, u32)> {
    if !controller.command(
        VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
        HDR_SIZE,
        VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
        |_, _, _| {},
    ) {
        return None;
    }

    let response = unsafe {
        core::slice::from_raw_parts(
            controller.command_cpu.add(COMMAND_RESPONSE_OFFSET),
            COMMAND_RESPONSE_SIZE,
        )
    };
    for index in 0..DISPLAY_INFO_SCANOUT_COUNT {
        let base = HDR_SIZE + index * 24;
        let width = read_u32(response, base + 8);
        let height = read_u32(response, base + 12);
        let enabled = read_u32(response, base + 16);
        if enabled != 0 && width != 0 && height != 0 {
            return Some((width, height));
        }
    }

    None
}

fn usable_scanout_size((width, height): (u32, u32)) -> Option<(u32, u32)> {
    if !scanout_size_is_supported(width, height) {
        crate::debug::println!(
            "virtio-gpu native: rejecting unsupported host scanout {}x{}",
            width,
            height
        );
        return None;
    }

    if scanout_size_matches_boot_framebuffer(width, height) {
        let fallback = default_scanout_size();
        if fallback != (width, height) {
            crate::debug::println!(
                "virtio-gpu native: overriding inherited boot scanout {}x{} with {}x{}",
                width,
                height,
                fallback.0,
                fallback.1
            );
            return Some(fallback);
        }
    }

    if width < MIN_USABLE_SCANOUT_WIDTH || height < MIN_USABLE_SCANOUT_HEIGHT {
        let fallback = default_scanout_size();
        crate::debug::println!(
            "virtio-gpu native: overriding small host scanout {}x{} with {}x{}",
            width,
            height,
            fallback.0,
            fallback.1
        );
        return Some(fallback);
    }

    Some((width, height))
}

fn default_scanout_size() -> (u32, u32) {
    (FRAMEBUFFER_WIDTH_DEFAULT, FRAMEBUFFER_HEIGHT_DEFAULT)
}

fn scanout_size_matches_boot_framebuffer(width: u32, height: u32) -> bool {
    if let Some(framebuffer) = crate::storage::boot_volume::boot_framebuffer_info() {
        return framebuffer.validate().is_ok()
            && framebuffer.width as u32 == width
            && framebuffer.height as u32 == height;
    }
    false
}

fn scanout_size_is_supported(width: u32, height: u32) -> bool {
    width != 0 && height != 0 && width <= MAX_SCANOUT_WIDTH && height <= MAX_SCANOUT_HEIGHT
}

fn framebuffer_size_bytes(width: u32, height: u32) -> Option<usize> {
    if !scanout_size_is_supported(width, height) {
        return None;
    }
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(BYTES_PER_PIXEL as usize)
}

fn setup_control_queue(common: *mut u8) -> Option<VirtQueue> {
    unsafe {
        let num_queues = read_common_u16(common, COMMON_NUM_QUEUES);
        if num_queues == 0 {
            return None;
        }
        write_common_u16(common, COMMON_QUEUE_SELECT, CONTROL_QUEUE_INDEX);
        let max_size = read_common_u16(common, COMMON_QUEUE_SIZE);
        if max_size < 2 {
            return None;
        }
        let queue_size = QUEUE_SIZE.min(max_size);
        let notify_off = read_common_u16(common, COMMON_QUEUE_NOTIFY_OFF);
        let (mem_cpu, mem_dma) = alloc_dma(QUEUE_MEM_SIZE)?;
        ptr::write_bytes(mem_cpu, 0, QUEUE_MEM_SIZE);

        let desc_offset = 0;
        let avail_offset = align_up(queue_size as usize * 16, 2);
        let used_offset = align_up(avail_offset + 4 + queue_size as usize * 2, 4);

        write_common_u16(common, COMMON_QUEUE_SIZE, queue_size);
        write_common_u64(common, COMMON_QUEUE_DESC, mem_dma + desc_offset as u64);
        write_common_u64(common, COMMON_QUEUE_DRIVER, mem_dma + avail_offset as u64);
        write_common_u64(common, COMMON_QUEUE_DEVICE, mem_dma + used_offset as u64);
        write_common_u16(common, COMMON_QUEUE_ENABLE, 1);

        Some(VirtQueue {
            queue_size,
            notify_off,
            mem_cpu,
            _mem_dma: mem_dma,
            desc_offset,
            avail_offset,
            used_offset,
            avail_idx: 0,
            used_idx: 0,
        })
    }
}

unsafe fn negotiate_features(common: *mut u8) -> Option<()> {
    unsafe {
        write_common_u32(common, COMMON_DEVICE_FEATURE_SELECT, 1);
        let high = read_common_u32(common, COMMON_DEVICE_FEATURE);
        if (high & (1 << (VIRTIO_F_VERSION_1 - 32))) == 0 {
            return None;
        }

        write_common_u32(common, COMMON_DRIVER_FEATURE_SELECT, 0);
        write_common_u32(common, COMMON_DRIVER_FEATURE, 0);
        write_common_u32(common, COMMON_DRIVER_FEATURE_SELECT, 1);
        write_common_u32(
            common,
            COMMON_DRIVER_FEATURE,
            1 << (VIRTIO_F_VERSION_1 - 32),
        );

        let status = read_common_u8(common, COMMON_DEVICE_STATUS) | VIRTIO_STATUS_FEATURES_OK;
        write_common_u8(common, COMMON_DEVICE_STATUS, status);
        if (read_common_u8(common, COMMON_DEVICE_STATUS) & VIRTIO_STATUS_FEATURES_OK) == 0 {
            return None;
        }
        Some(())
    }
}

unsafe fn mark_driver_ok(common: *mut u8) {
    unsafe {
        let status = read_common_u8(common, COMMON_DEVICE_STATUS) | VIRTIO_STATUS_DRIVER_OK;
        write_common_u8(common, COMMON_DEVICE_STATUS, status);
    }
}

unsafe fn mark_driver_failed(common: *mut u8) {
    unsafe {
        let status = read_common_u8(common, COMMON_DEVICE_STATUS) | VIRTIO_STATUS_FAILED;
        write_common_u8(common, COMMON_DEVICE_STATUS, status);
    }
}

fn parse_virtio_pci_caps(pci: crate::arch::pci::PciDevice) -> Option<VirtioGpuPciCaps> {
    if (pci.read_u16(PCI_CAP_STATUS) & PCI_STATUS_CAP_LIST) == 0 {
        return None;
    }

    let mut common = None;
    let mut notify = None;
    let mut notify_multiplier = 0;
    let mut device_config = None;
    let mut cap = pci.read_u8(PCI_CAP_POINTER) & !0x3;
    let mut guard = 0;

    while cap != 0 && guard < 32 {
        guard += 1;
        if pci.read_u8(cap) != PCI_CAP_ID_VENDOR {
            cap = pci.read_u8(cap + 1) & !0x3;
            continue;
        }

        let cfg_type = pci.read_u8(cap + 3);
        let bar = pci.read_u8(cap + 4) as usize;
        let offset = pci.read_u32(cap + 8) as u64;
        let length = pci.read_u32(cap + 12) as usize;
        let region = MmioRegion {
            bar,
            offset,
            length,
        };

        match cfg_type {
            VIRTIO_PCI_CAP_COMMON_CFG => common = Some(region),
            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                notify = Some(region);
                notify_multiplier = pci.read_u32(cap + 16);
            }
            VIRTIO_PCI_CAP_DEVICE_CFG => device_config = Some(region),
            _ => {}
        }

        cap = pci.read_u8(cap + 1) & !0x3;
    }

    Some(VirtioGpuPciCaps {
        common: common?,
        notify: notify?,
        notify_multiplier,
        device_config,
    })
}

fn map_region(
    pci: crate::arch::pci::PciDevice,
    region: MmioRegion,
    write_combine: bool,
) -> Option<*mut u8> {
    let resource = pci.resource(region.bar)?;
    if resource.is_io || region.length == 0 {
        return None;
    }
    let size = region.length.max(4);
    let end = region.offset.checked_add(size as u64)?;
    if end > resource.size {
        return None;
    }
    let ptr = crate::driver::mmio::map(
        resource.start.checked_add(region.offset)?,
        size,
        write_combine,
    );
    (!ptr.is_null()).then_some(ptr.cast())
}

fn alloc_dma(size: usize) -> Option<(*mut u8, u64)> {
    let mut dma = 0u64;
    let ptr = crate::driver::dma::alloc_coherent(core::ptr::null_mut(), size, &mut dma);
    (!ptr.is_null()).then_some((ptr.cast(), dma))
}

fn mark_framebuffer_write_combine(phys_addr: u64, size: usize) {
    let _ = crate::memory::paging::update_direct_map_range_flags(
        phys_addr,
        size,
        crate::memory::paging::WRITE_COMBINE_BIT,
        x86_64::structures::paging::PageTableFlags::empty(),
    );
}

unsafe fn write_desc(base: *mut u8, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
    unsafe {
        let offset = index * 16;
        ptr::write_volatile(base.add(offset) as *mut u64, addr);
        ptr::write_volatile(base.add(offset + 8) as *mut u32, len);
        ptr::write_volatile(base.add(offset + 12) as *mut u16, flags);
        ptr::write_volatile(base.add(offset + 14) as *mut u16, next);
    }
}

fn write_rect(buf: &mut [u8], offset: usize, x: u32, y: u32, width: u32, height: u32) {
    write_u32(buf, offset, x);
    write_u32(buf, offset + 4, y);
    write_u32(buf, offset + 8, width);
    write_u32(buf, offset + 12, height);
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

unsafe fn read_common_u8(base: *mut u8, offset: usize) -> u8 {
    unsafe { ptr::read_volatile(base.add(offset) as *const u8) }
}

unsafe fn write_common_u8(base: *mut u8, offset: usize, value: u8) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u8, value) }
}

unsafe fn read_common_u16(base: *mut u8, offset: usize) -> u16 {
    unsafe { ptr::read_volatile(base.add(offset) as *const u16) }
}

unsafe fn write_common_u16(base: *mut u8, offset: usize, value: u16) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u16, value) }
}

unsafe fn read_common_u32(base: *mut u8, offset: usize) -> u32 {
    unsafe { ptr::read_volatile(base.add(offset) as *const u32) }
}

unsafe fn write_common_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u32, value) }
}

unsafe fn write_common_u64(base: *mut u8, offset: usize, value: u64) {
    unsafe { ptr::write_volatile(base.add(offset) as *mut u64, value) }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
