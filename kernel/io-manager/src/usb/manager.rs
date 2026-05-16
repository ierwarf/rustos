use alloc::vec::Vec;

use crate::sync::KernelSpinLock as Mutex;

use super::emulation;
#[allow(unused_imports)]
use super::host::{UsbHostControllerInfo, controller_kind_name};
use super::xhci;

const USB_SERVICE_ROUNDS: usize = 2;

struct UsbRuntimeState {
    controllers: Vec<UsbHostControllerInfo>,
    runtime_initialized: bool,
}

impl UsbRuntimeState {
    const fn new() -> Self {
        Self {
            controllers: Vec::new(),
            runtime_initialized: false,
        }
    }
}

static USB_STATE: Mutex<UsbRuntimeState> = Mutex::new(UsbRuntimeState::new());

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn init() {
    let mut state = USB_STATE.lock();
    if state.runtime_initialized {
        return;
    }

    let controllers = scan_host_controllers();
    let xhci_count = xhci::initialize(&controllers);
    state.controllers = controllers;
    state.runtime_initialized = true;
    crate::debug::println!(
        "usb: in-kernel runtime initialized controllers={} xhci={}",
        state.controllers.len(),
        xhci_count
    );
}

pub(crate) fn service_pending() -> usize {
    let initialized = USB_STATE.lock().runtime_initialized;
    if !initialized {
        return 0;
    }

    let mut total = 0;
    for _ in 0..USB_SERVICE_ROUNDS {
        let host_work = xhci::service_pending();
        let emulation_work = emulation::service_pending();
        let round_work = host_work + emulation_work;
        total += round_work;
        if round_work == 0 {
            break;
        }
    }
    total
}

pub(crate) fn host_controllers_available() -> bool {
    let state = USB_STATE.lock();
    state.runtime_initialized && !state.controllers.is_empty()
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn scan_host_controllers() -> Vec<UsbHostControllerInfo> {
    let mut controllers = Vec::new();

    crate::arch::pci::visit_usb_controllers(|address, kind| {
        let bar0 = address.resource(0);
        let irq_line = address.interrupt_line();
        if kind == crate::arch::pci::UsbHostControllerKind::Xhci {
            address.enable_memory_bus_master();
        }
        controllers.push(UsbHostControllerInfo {
            address,
            kind,
            bar0,
            irq_line,
        });
        false
    });

    if controllers.is_empty() {
        crate::debug::println!("usb: no host controllers detected");
    } else {
        for controller in controllers.iter() {
            let bdf = controller.address;
            let bar0_base = controller.bar0.map(|resource| resource.start).unwrap_or(0);
            let bar0_size = controller.bar0.map(|resource| resource.size).unwrap_or(0);
            crate::debug::println!(
                "usb host controller: kind={} bdf={:02x}:{:02x}.{} vendor={:04x} device={:04x} irq={} bar0={:#x}/len={:#x}",
                controller_kind_name(controller.kind),
                bdf.bus,
                bdf.device,
                bdf.function,
                bdf.vendor_id(),
                bdf.device_id(),
                controller.irq_line,
                bar0_base,
                bar0_size,
            );
        }
    }

    controllers
}
