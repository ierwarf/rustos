use core::ffi::c_char;
use core::sync::atomic::{AtomicU64, Ordering};

use super::linux::compat::LinuxCompatPciDev;
use crate::sync::KernelSpinLock as Mutex;
use driver_abi::DriverBus;
use rustos_user_abi::syscall::{
    LinuxDriverSymbolEventWire, DRIVER_SYMBOL_EVENT_CONTEXT_DEVICE_MODEL,
    DRIVER_SYMBOL_EVENT_CONTEXT_PCI_CONFIG, DRIVER_SYMBOL_EVENT_CONTEXT_PCI_RESOURCE,
    DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT, DRIVER_SYMBOL_EVENT_CONTEXT_RESOURCE_LEASE,
    DRIVER_SYMBOL_EVENT_CONTEXT_WORKQUEUE_TIMER, DRIVER_SYMBOL_EVENT_FLAG_DROPPED_BEFORE,
    DRIVER_SYMBOL_EVENT_MODULE_CAPACITY, DRIVER_SYMBOL_EVENT_SCOPE_DEVICE_MODEL,
    DRIVER_SYMBOL_EVENT_SCOPE_DMA, DRIVER_SYMBOL_EVENT_SCOPE_DRM, DRIVER_SYMBOL_EVENT_SCOPE_HID,
    DRIVER_SYMBOL_EVENT_SCOPE_IRQ, DRIVER_SYMBOL_EVENT_SCOPE_MMIO,
    DRIVER_SYMBOL_EVENT_SCOPE_NETDEV, DRIVER_SYMBOL_EVENT_SCOPE_PCI, DRIVER_SYMBOL_EVENT_SCOPE_USB,
    DRIVER_SYMBOL_EVENT_SCOPE_VIRTIO, DRIVER_SYMBOL_EVENT_SCOPE_WORKQUEUE,
    DRIVER_SYMBOL_EVENT_SYMBOL_CAPACITY,
};

const EVENT_QUEUE_CAPACITY: usize = 64;

struct SymbolEventQueue {
    entries: [Option<LinuxDriverSymbolEventWire>; EVENT_QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

impl SymbolEventQueue {
    const fn new() -> Self {
        Self {
            entries: [None; EVENT_QUEUE_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push_back(&mut self, event: LinuxDriverSymbolEventWire) -> bool {
        if self.len == self.entries.len() {
            return false;
        }
        let tail = (self.head + self.len) % self.entries.len();
        self.entries[tail] = Some(event);
        self.len += 1;
        true
    }

    fn pop_front(&mut self) -> Option<LinuxDriverSymbolEventWire> {
        if self.len == 0 {
            return None;
        }
        let event = self.entries[self.head].take();
        self.head = (self.head + 1) % self.entries.len();
        self.len -= 1;
        event
    }
}

static SYMBOL_EVENTS: Mutex<SymbolEventQueue> = Mutex::new(SymbolEventQueue::new());
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn record_pci_probe_init_symbol(symbol: &'static str, module: *const c_char) {
    record_event(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT,
        DRIVER_SYMBOL_EVENT_SCOPE_PCI,
        DriverBus::Pci as u32,
        0,
        0,
        0,
        module,
    );
}

pub(crate) fn record_pci_resource_symbol(
    symbol: &'static str,
    dev: *mut LinuxCompatPciDev,
    arg1: u64,
    arg2: u64,
) {
    record_event(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PCI_RESOURCE,
        DRIVER_SYMBOL_EVENT_SCOPE_PCI,
        DriverBus::Pci as u32,
        pci_device_key(dev),
        arg1,
        arg2,
        core::ptr::null(),
    );
}

pub(crate) fn record_pci_config_symbol(
    symbol: &'static str,
    dev: *mut LinuxCompatPciDev,
    offset: i32,
    value_and_width: u64,
) {
    record_event(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PCI_CONFIG,
        DRIVER_SYMBOL_EVENT_SCOPE_PCI,
        DriverBus::Pci as u32,
        pci_device_key(dev),
        offset as u32 as u64,
        value_and_width,
        core::ptr::null(),
    );
}

pub(crate) fn record_usb_probe_init_symbol(symbol: &'static str, driver: usize, arg1: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT,
        DRIVER_SYMBOL_EVENT_SCOPE_USB,
        driver as u64,
        arg1,
        0,
    );
}

pub(crate) fn record_hid_probe_init_symbol(symbol: &'static str, driver: usize, arg1: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT,
        DRIVER_SYMBOL_EVENT_SCOPE_HID,
        driver as u64,
        arg1,
        0,
    );
}

pub(crate) fn record_device_model_symbol(symbol: &'static str, subject: usize, arg1: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_DEVICE_MODEL,
        DRIVER_SYMBOL_EVENT_SCOPE_DEVICE_MODEL,
        subject as u64,
        arg1,
        0,
    );
}

pub(crate) fn record_virtio_probe_init_symbol(symbol: &'static str, driver: usize, arg1: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT,
        DRIVER_SYMBOL_EVENT_SCOPE_VIRTIO,
        driver as u64,
        arg1,
        0,
    );
}

pub(crate) fn record_drm_probe_init_symbol(symbol: &'static str, dev: usize, flags: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT,
        DRIVER_SYMBOL_EVENT_SCOPE_DRM,
        dev as u64,
        flags,
        0,
    );
}

pub(crate) fn record_netdev_symbol(symbol: &'static str, dev: usize, arg1: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_DEVICE_MODEL,
        DRIVER_SYMBOL_EVENT_SCOPE_NETDEV,
        dev as u64,
        arg1,
        0,
    );
}

pub(crate) fn record_dma_symbol(symbol: &'static str, dev: usize, arg1: u64, arg2: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_RESOURCE_LEASE,
        DRIVER_SYMBOL_EVENT_SCOPE_DMA,
        dev as u64,
        arg1,
        arg2,
    );
}

pub(crate) fn record_mmio_symbol(symbol: &'static str, subject: usize, arg1: u64, arg2: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_RESOURCE_LEASE,
        DRIVER_SYMBOL_EVENT_SCOPE_MMIO,
        subject as u64,
        arg1,
        arg2,
    );
}

pub(crate) fn record_irq_symbol(symbol: &'static str, subject: usize, irq: u32, flags: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_RESOURCE_LEASE,
        DRIVER_SYMBOL_EVENT_SCOPE_IRQ,
        subject as u64,
        u64::from(irq),
        flags,
    );
}

pub(crate) fn record_workqueue_timer_symbol(symbol: &'static str, subject: usize, arg1: u64) {
    record_linux_slowpath_symbol(
        symbol,
        DRIVER_SYMBOL_EVENT_CONTEXT_WORKQUEUE_TIMER,
        DRIVER_SYMBOL_EVENT_SCOPE_WORKQUEUE,
        subject as u64,
        arg1,
        0,
    );
}

pub(crate) fn record_linux_slowpath_symbol(
    symbol: &'static str,
    context: u16,
    scope: u16,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) {
    record_event(
        symbol,
        context,
        scope,
        0,
        arg0,
        arg1,
        arg2,
        core::ptr::null(),
    );
}

fn record_event(
    symbol: &'static str,
    context: u16,
    scope: u16,
    bus: u32,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    module: *const c_char,
) {
    let mut event = LinuxDriverSymbolEventWire {
        sequence: NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        class: 0,
        bus,
        context,
        scope,
        arg0,
        arg1,
        arg2,
        ..LinuxDriverSymbolEventWire::default()
    };
    event.symbol_len = copy_symbol(symbol, &mut event.symbol);
    event.module_len = unsafe { copy_c_string(module, &mut event.module) };
    let dropped = DROPPED_EVENTS.swap(0, Ordering::AcqRel);
    if dropped != 0 {
        event.flags |= DRIVER_SYMBOL_EVENT_FLAG_DROPPED_BEFORE;
        event.dropped_before = dropped;
    }

    let mut queue = SYMBOL_EVENTS.lock();
    if !queue.push_back(event) {
        DROPPED_EVENTS.fetch_add(1 + dropped, Ordering::AcqRel);
    }
}

fn pci_device_key(dev: *mut LinuxCompatPciDev) -> u64 {
    if dev.is_null() {
        return 0;
    }
    let dev = unsafe { &*dev };
    (u64::from(dev.vendor) << 48) | (u64::from(dev.device) << 32) | u64::from(dev.devfn)
}

pub fn drain_linux_symbol_event() -> Option<LinuxDriverSymbolEventWire> {
    SYMBOL_EVENTS.lock().pop_front()
}

fn copy_symbol(src: &str, dest: &mut [u8; DRIVER_SYMBOL_EVENT_SYMBOL_CAPACITY]) -> u16 {
    let bytes = src.as_bytes();
    let len = core::cmp::min(bytes.len(), dest.len());
    dest[..len].copy_from_slice(&bytes[..len]);
    len as u16
}

unsafe fn copy_c_string(
    src: *const c_char,
    dest: &mut [u8; DRIVER_SYMBOL_EVENT_MODULE_CAPACITY],
) -> u16 {
    if src.is_null() {
        return 0;
    }
    let mut len = 0usize;
    while len < dest.len() {
        let byte = unsafe { *src.add(len) as u8 };
        if byte == 0 {
            break;
        }
        dest[len] = byte;
        len += 1;
    }
    len as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_recorded_pci_probe_init_symbol() {
        let _guard = crate::test_support::exclusive_test();
        while drain_linux_symbol_event().is_some() {}

        record_pci_probe_init_symbol("__pci_register_driver", core::ptr::null());
        let event = drain_linux_symbol_event().expect("symbol event");
        assert_eq!(event.context, DRIVER_SYMBOL_EVENT_CONTEXT_PROBE_INIT);
        assert_eq!(event.scope, DRIVER_SYMBOL_EVENT_SCOPE_PCI);
        assert_eq!(
            &event.symbol[..event.symbol_len as usize],
            b"__pci_register_driver"
        );
        assert!(drain_linux_symbol_event().is_none());
    }

    #[test]
    fn preserves_pci_resource_and_config_payloads() {
        let _guard = crate::test_support::exclusive_test();
        while drain_linux_symbol_event().is_some() {}

        record_pci_resource_symbol("pci_iomap", core::ptr::null_mut(), 0x1000, 0x2000);
        record_pci_config_symbol(
            "pci_write_config_dword",
            core::ptr::null_mut(),
            0x10,
            0x0400_0001,
        );

        let resource = drain_linux_symbol_event().expect("resource event");
        assert_eq!(resource.context, DRIVER_SYMBOL_EVENT_CONTEXT_PCI_RESOURCE);
        assert_eq!(
            &resource.symbol[..resource.symbol_len as usize],
            b"pci_iomap"
        );
        assert_eq!(resource.arg1, 0x1000);
        assert_eq!(resource.arg2, 0x2000);

        let config = drain_linux_symbol_event().expect("config event");
        assert_eq!(config.context, DRIVER_SYMBOL_EVENT_CONTEXT_PCI_CONFIG);
        assert_eq!(
            &config.symbol[..config.symbol_len as usize],
            b"pci_write_config_dword"
        );
        assert_eq!(config.arg1, 0x10);
        assert_eq!(config.arg2, 0x0400_0001);
    }

    #[test]
    fn records_generic_slowpath_symbol() {
        let _guard = crate::test_support::exclusive_test();
        while drain_linux_symbol_event().is_some() {}

        record_linux_slowpath_symbol("usb_register_driver", 4, 2, 0x10, 0x20, 0x30);
        let event = drain_linux_symbol_event().expect("symbol event");
        assert_eq!(event.context, 4);
        assert_eq!(event.scope, 2);
        assert_eq!(event.bus, 0);
        assert_eq!(event.arg0, 0x10);
        assert_eq!(event.arg1, 0x20);
        assert_eq!(event.arg2, 0x30);
        assert_eq!(
            &event.symbol[..event.symbol_len as usize],
            b"usb_register_driver"
        );
    }
}
