use alloc::vec::Vec;

use driver_abi::DriverBus;
use spin::Mutex;

use super::emulation;
use super::host::UsbHostControllerInfo;
use super::xhci;
const USB_SERVICE_ROUNDS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsbSubsystemStage {
    Uninitialized,
    ControllersScanned,
    WaitingForUserspaceDisplay,
    ModulesLoaded,
}

struct UsbSubsystemState {
    stage: UsbSubsystemStage,
    controllers: Vec<UsbHostControllerInfo>,
}

impl UsbSubsystemState {
    const fn new() -> Self {
        Self {
            stage: UsbSubsystemStage::Uninitialized,
            controllers: Vec::new(),
        }
    }
}

static USB_STATE: Mutex<UsbSubsystemState> = Mutex::new(UsbSubsystemState::new());

pub(crate) fn init() {
    emulation::prepare();
    let found_xhci = scan_host_controllers();

    if found_xhci {
        let controllers = host_controllers();
        let initialized = xhci::initialize(&controllers);
        crate::debug::println!("usb: xHCI controllers initialized={}", initialized);
        crate::debug::println!(
            "usb: xHCI present, deferring USB input modules until userspace display"
        );
        USB_STATE.lock().stage = UsbSubsystemStage::WaitingForUserspaceDisplay;
    } else {
        crate::debug::println!("usb: no xHCI controller found, USB modules left validated");
    }
}

pub(crate) fn service_pending() -> usize {
    let mut total = maybe_initialize_usb_input_modules();
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

fn maybe_initialize_usb_input_modules() -> usize {
    let should_initialize = {
        let mut state = USB_STATE.lock();
        if state.stage != UsbSubsystemStage::WaitingForUserspaceDisplay
            || !crate::io::gui::is_userspace_display_active()
        {
            false
        } else {
            state.stage = UsbSubsystemStage::ModulesLoaded;
            true
        }
    };

    if !should_initialize {
        return 0;
    }

    crate::debug::println!("usb: userspace display active, loading USB input modules");
    crate::driver::initialize_loadable_modules_for_bus(DriverBus::Usb);
    1
}

pub(crate) fn host_controllers() -> Vec<UsbHostControllerInfo> {
    USB_STATE.lock().controllers.clone()
}

fn scan_host_controllers() -> bool {
    let mut controllers = Vec::new();
    let mut found_xhci = false;

    crate::arch::pci::visit_usb_controllers(|address, kind| {
        let bar0 = address.resource(0);
        let irq_line = address.interrupt_line();
        if kind == crate::arch::pci::UsbHostControllerKind::Xhci {
            address.enable_memory_bus_master();
            found_xhci = true;
        }
        controllers.push(UsbHostControllerInfo {
            address,
            kind,
            bar0,
            irq_line,
        });
        false
    });

    let mut state = USB_STATE.lock();
    state.controllers = controllers;
    state.stage = UsbSubsystemStage::ControllersScanned;

    if state.controllers.is_empty() {
        crate::debug::println!("usb: no host controllers detected");
    } else {
        for controller in state.controllers.iter() {
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

    found_xhci
}
