// RING3-MIGRATION-REFERENCE START: usb-runtime-substrate exception:
// usbdrv/driverd own USB runtime initialization and controller provider policy.
// Ring0 keeps native host-controller discovery, xHCI enable gating, and service
// hooks until the service-driver host can drive leased hardware resources.
use alloc::vec::Vec;

use crate::sync::KernelSpinLock as Mutex;

use super::emulation;
#[allow(unused_imports)]
use super::host::{UsbHostControllerInfo, controller_kind_name};
use super::xhci;

const USB_SERVICE_ROUNDS: usize = 2;
const ENABLE_NATIVE_XHCI: bool = true;

struct UsbRuntimeState {
    controllers: Vec<UsbHostControllerInfo>,
    runtime_initializing: bool,
    runtime_initialized: bool,
}

impl UsbRuntimeState {
    const fn new() -> Self {
        Self {
            controllers: Vec::new(),
            runtime_initializing: false,
            runtime_initialized: false,
        }
    }
}

static USB_STATE: Mutex<UsbRuntimeState> = Mutex::new(UsbRuntimeState::new());

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn init() {
    {
        let mut state = USB_STATE.lock();
        if state.runtime_initialized || state.runtime_initializing {
            return;
        }
        state.runtime_initializing = true;
    }

    let controllers = scan_host_controllers();
    let xhci_count = if ENABLE_NATIVE_XHCI {
        xhci::initialize(&controllers)
    } else {
        0
    };

    let mut state = USB_STATE.lock();
    state.controllers = controllers;
    state.runtime_initializing = false;
    state.runtime_initialized = true;
    crate::debug::println!(
        "usb: in-kernel runtime initialized controllers={} xhci={} native_xhci={}",
        state.controllers.len(),
        xhci_count,
        if ENABLE_NATIVE_XHCI { 1 } else { 0 }
    );
}

pub(crate) fn service_pending() -> usize {
    let initialized = USB_STATE
        .try_lock()
        .map(|state| state.runtime_initialized)
        .unwrap_or(false);
    if !initialized {
        return 0;
    }

    let mut total = 0;
    for _ in 0..USB_SERVICE_ROUNDS {
        let host_work = if ENABLE_NATIVE_XHCI {
            xhci::service_pending()
        } else {
            0
        };
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
    USB_STATE
        .try_lock()
        .map(|state| state.runtime_initialized && !state.controllers.is_empty())
        .unwrap_or(false)
}

pub(crate) fn uses_polled_input_completion() -> bool {
    let initialized = USB_STATE
        .try_lock()
        .map(|state| state.runtime_initialized)
        .unwrap_or(false);
    initialized && ENABLE_NATIVE_XHCI && xhci::has_active_input_polling()
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
// RING3-MIGRATION-REFERENCE END: USB manager substrate exception.
