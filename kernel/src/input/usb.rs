use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::cmp::min;
use core::hint::spin_loop;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicU8, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::arch::pci::{self, PciDevice, UsbHostControllerKind};
use crate::keyboard::{self, KeyCode};

const USB_POLL_INTERVAL_TICKS: u8 = 2;
const XHCI_POLL_SPINS: usize = 5_000_000;
const XHCI_RING_TRBS: usize = 256;
const USB_REPORT_LEN: usize = 8;
const MAX_CONTROL_BUFFER: usize = 512;

const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCSPARAMS2: usize = 0x08;
const CAP_HCCPARAMS1: usize = 0x10;
const CAP_DBOFF: usize = 0x14;
const CAP_RTSOFF: usize = 0x18;

const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_PAGESIZE: usize = 0x08;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTSC_BASE: usize = 0x400;
const OP_PORTSC_STRIDE: usize = 0x10;

const RT_INTR0: usize = 0x20;
const INTR_ERSTSZ: usize = 0x08;
const INTR_ERSTBA: usize = 0x10;
const INTR_ERDP: usize = 0x18;

const USBCMD_RUN_STOP: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;

const USBSTS_HCHALTED: u32 = 1 << 0;
const USBSTS_CNR: u32 = 1 << 11;

const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR: u32 = 1 << 4;
const PORTSC_PP: u32 = 1 << 9;
const PORTSC_SPEED_SHIFT: u32 = 10;
const PORTSC_SPEED_MASK: u32 = 0x0f << PORTSC_SPEED_SHIFT;
const PORTSC_CHANGE_BITS: u32 =
    (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22) | (1 << 23);

const TRB_CYCLE: u32 = 1 << 0;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_TYPE_SHIFT: u32 = 10;
const TRB_TYPE_MASK: u32 = 0x3f << TRB_TYPE_SHIFT;
const TRB_SETUP_TRANSFER_TYPE_SHIFT: u32 = 16;
const TRB_DATA_DIR_IN: u32 = 1 << 16;
const TRB_STATUS_DIR_IN: u32 = 1 << 16;
const TRB_SLOT_ID_SHIFT: u32 = 24;
const TRB_ENDPOINT_ID_SHIFT: u32 = 16;

const TRB_TYPE_NORMAL: u32 = 1;
const TRB_TYPE_SETUP_STAGE: u32 = 2;
const TRB_TYPE_DATA_STAGE: u32 = 3;
const TRB_TYPE_STATUS_STAGE: u32 = 4;
const TRB_TYPE_LINK: u32 = 6;
const TRB_TYPE_ENABLE_SLOT: u32 = 9;
const TRB_TYPE_DISABLE_SLOT: u32 = 10;
const TRB_TYPE_ADDRESS_DEVICE: u32 = 11;
const TRB_TYPE_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_TYPE_EVALUATE_CONTEXT: u32 = 13;
const TRB_TYPE_TRANSFER_EVENT: u32 = 32;
const TRB_TYPE_COMMAND_COMPLETION: u32 = 33;
const TRB_TYPE_PORT_STATUS_CHANGE: u32 = 34;

const COMPLETION_SUCCESS: u8 = 1;
const COMPLETION_SHORT_PACKET: u8 = 13;

const EP_TYPE_CONTROL: u32 = 4;
const EP_TYPE_INTERRUPT_IN: u32 = 7;

const REQ_GET_DESCRIPTOR: u8 = 0x06;
const REQ_SET_CONFIGURATION: u8 = 0x09;
const HID_REQ_SET_IDLE: u8 = 0x0a;
const HID_REQ_SET_PROTOCOL: u8 = 0x0b;

const DESC_DEVICE: u8 = 0x01;
const DESC_CONFIGURATION: u8 = 0x02;
const DESC_INTERFACE: u8 = 0x04;
const DESC_ENDPOINT: u8 = 0x05;

const HID_CLASS: u8 = 0x03;
const HID_SUBCLASS_BOOT: u8 = 0x01;
const HID_PROTOCOL_KEYBOARD: u8 = 0x01;
const ENDPOINT_ATTR_INTERRUPT: u8 = 0x03;

static USB_KEYBOARD: Mutex<Option<UsbKeyboardDriver>> = Mutex::new(None);
static USB_POLL_TICKS: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UsbKeyboardInfo {
    pub controller: PciDevice,
    pub port_id: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UsbKeyboardInitResult {
    Ready(UsbKeyboardInfo),
    Unavailable(&'static str),
}

pub fn init() -> UsbKeyboardInitResult {
    interrupts::without_interrupts(|| {
        let mut guard = USB_KEYBOARD.lock();
        match UsbKeyboardDriver::new() {
            Ok(driver) => {
                let info = driver.info();
                *guard = Some(driver);
                UsbKeyboardInitResult::Ready(info)
            }
            Err(err) => {
                *guard = None;
                UsbKeyboardInitResult::Unavailable(err)
            }
        }
    })
}

pub fn poll_fallback() {
    let ticks = USB_POLL_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks < USB_POLL_INTERVAL_TICKS {
        return;
    }

    USB_POLL_TICKS.store(0, Ordering::Relaxed);
    poll();
}

pub fn poll() {
    interrupts::without_interrupts(|| {
        let mut guard = USB_KEYBOARD.lock();
        if let Some(driver) = guard.as_mut() {
            driver.poll();
        }
    });
}

struct UsbKeyboardDriver {
    controller: XhciController,
    slot_id: u8,
    port_id: u8,
    interrupt_dci: u8,
    report_buffer: DmaBlock,
    last_report: [u8; USB_REPORT_LEN],
    transfer_armed: bool,
}

struct ProbeSummary {
    found_usb_controller: bool,
    found_xhci_controller: bool,
    found_legacy_usb_controller: bool,
    found_keyboardless_controller: bool,
    last_xhci_error: &'static str,
}

impl ProbeSummary {
    const fn new() -> Self {
        Self {
            found_usb_controller: false,
            found_xhci_controller: false,
            found_legacy_usb_controller: false,
            found_keyboardless_controller: false,
            last_xhci_error: "no xHCI controller",
        }
    }

    fn record_controller(&mut self, kind: UsbHostControllerKind) {
        self.found_usb_controller = true;
        match kind {
            UsbHostControllerKind::Xhci => self.found_xhci_controller = true,
            UsbHostControllerKind::Uhci
            | UsbHostControllerKind::Ohci
            | UsbHostControllerKind::Ehci => self.found_legacy_usb_controller = true,
            UsbHostControllerKind::Unknown(_) => {}
        }
    }

    fn record_keyboardless_xhci(&mut self) {
        self.found_keyboardless_controller = true;
    }

    fn record_xhci_error(&mut self, err: &'static str) {
        self.last_xhci_error = err;
    }

    fn error(self) -> &'static str {
        if self.found_keyboardless_controller {
            return "no HID boot keyboard found on any xHCI controller";
        }
        if self.found_xhci_controller {
            return self.last_xhci_error;
        }
        if self.found_legacy_usb_controller {
            return "found only legacy USB controllers (UHCI/OHCI/EHCI); non-xHCI USB keyboard support is not implemented";
        }
        if self.found_usb_controller {
            return "found only unsupported USB host controllers";
        }
        "no USB host controller"
    }
}

impl UsbKeyboardDriver {
    fn new() -> Result<Self, &'static str> {
        let mut summary = ProbeSummary::new();
        let mut driver = None;

        pci::visit_usb_controllers(|pci, kind| {
            summary.record_controller(kind);
            match kind {
                UsbHostControllerKind::Xhci => match Self::new_on_controller(pci) {
                    Ok(Some(candidate)) => {
                        driver = Some(candidate);
                        true
                    }
                    Ok(None) => {
                        summary.record_keyboardless_xhci();
                        crate::debug::println!(
                            "xHCI {:04x}:{:04x} on {:04x}:{:02x}:{:02x}.{} has no HID boot keyboard.",
                            pci.vendor_id(),
                            pci.device_id(),
                            pci.segment,
                            pci.bus,
                            pci.device,
                            pci.function,
                        );
                        false
                    }
                    Err(err) => {
                        summary.record_xhci_error(err);
                        crate::debug::println!(
                            "xHCI {:04x}:{:04x} on {:04x}:{:02x}:{:02x}.{} probe failed: {}",
                            pci.vendor_id(),
                            pci.device_id(),
                            pci.segment,
                            pci.bus,
                            pci.device,
                            pci.function,
                            err,
                        );
                        false
                    }
                },
                other => {
                    crate::debug::println!(
                        "{} {:04x}:{:04x} on {:04x}:{:02x}:{:02x}.{} is present but unsupported by the current USB keyboard backend.",
                        other.name(),
                        pci.vendor_id(),
                        pci.device_id(),
                        pci.segment,
                        pci.bus,
                        pci.device,
                        pci.function,
                    );
                    false
                }
            }
        });

        if let Some(driver) = driver {
            return Ok(driver);
        }

        Err(summary.error())
    }

    fn new_on_controller(pci: PciDevice) -> Result<Option<Self>, &'static str> {
        let mut controller = XhciController::new(pci)?;
        let report_buffer = DmaBlock::new(USB_REPORT_LEN, 64)?;
        let control_buffer = DmaBlock::new(MAX_CONTROL_BUFFER, 64)?;

        for port_id in 1..=controller.max_ports {
            if let Some(device) =
                Self::try_enumerate_keyboard(&mut controller, port_id, &control_buffer)?
            {
                let mut driver = Self {
                    controller,
                    slot_id: device.slot_id,
                    port_id,
                    interrupt_dci: device.interrupt_dci,
                    report_buffer,
                    last_report: [0; USB_REPORT_LEN],
                    transfer_armed: false,
                };
                driver.arm_interrupt_transfer()?;
                return Ok(Some(driver));
            }
        }

        Ok(None)
    }

    fn info(&self) -> UsbKeyboardInfo {
        UsbKeyboardInfo {
            controller: self.controller.pci,
            port_id: self.port_id,
        }
    }

    fn try_enumerate_keyboard(
        controller: &mut XhciController,
        port_id: u8,
        control_buffer: &DmaBlock,
    ) -> Result<Option<UsbKeyboardDevice>, &'static str> {
        let portsc = controller.read_portsc(port_id);
        if portsc & PORTSC_CCS == 0 {
            return Ok(None);
        }

        controller.reset_port(port_id)?;
        let speed = controller.port_speed(port_id);
        if speed == 0 {
            return Ok(None);
        }

        let slot_id = controller.enable_slot()?;
        if controller.address_device(slot_id, port_id, speed)? == 0 {
            controller.disable_slot(slot_id)?;
            return Ok(None);
        }

        let initial_device = controller.get_descriptor(
            slot_id,
            control_buffer,
            0x80,
            REQ_GET_DESCRIPTOR,
            ((DESC_DEVICE as u16) << 8) | 0,
            0,
            8,
        )?;
        let max_packet = match speed {
            4.. => {
                let exp = *initial_device.get(7).ok_or("short device descriptor")?;
                1_u16 << min(exp, 15)
            }
            _ => *initial_device.get(7).ok_or("short device descriptor")? as u16,
        };
        controller.update_ep0_max_packet(slot_id, port_id, speed, max_packet)?;

        let config_header = controller.get_descriptor(
            slot_id,
            control_buffer,
            0x80,
            REQ_GET_DESCRIPTOR,
            ((DESC_CONFIGURATION as u16) << 8) | 0,
            0,
            9,
        )?;
        if config_header.len() < 9 {
            controller.disable_slot(slot_id)?;
            return Ok(None);
        }
        let total_length = u16::from_le_bytes([config_header[2], config_header[3]]) as usize;
        if total_length == 0 || total_length > MAX_CONTROL_BUFFER {
            controller.disable_slot(slot_id)?;
            return Ok(None);
        }

        let config = controller.get_descriptor(
            slot_id,
            control_buffer,
            0x80,
            REQ_GET_DESCRIPTOR,
            ((DESC_CONFIGURATION as u16) << 8) | 0,
            0,
            total_length as u16,
        )?;
        let Some(descriptor) = parse_boot_keyboard_interface(&config) else {
            controller.disable_slot(slot_id)?;
            return Ok(None);
        };

        controller.configure_interrupt_endpoint(slot_id, port_id, speed, &descriptor)?;
        controller.control_no_data(
            slot_id,
            0x00,
            REQ_SET_CONFIGURATION,
            descriptor.configuration_value as u16,
            0,
        )?;
        let _ = controller.control_no_data(
            slot_id,
            0x21,
            HID_REQ_SET_PROTOCOL,
            0,
            descriptor.interface_number as u16,
        );
        let _ = controller.control_no_data(
            slot_id,
            0x21,
            HID_REQ_SET_IDLE,
            0,
            descriptor.interface_number as u16,
        );

        Ok(Some(UsbKeyboardDevice {
            slot_id,
            interrupt_dci: endpoint_dci(descriptor.endpoint_address),
        }))
    }

    fn arm_interrupt_transfer(&mut self) -> Result<(), &'static str> {
        let trb = Trb::normal(self.report_buffer.phys(), USB_REPORT_LEN as u32, true);
        self.controller
            .queue_transfer(self.slot_id, self.interrupt_dci, trb);
        self.transfer_armed = true;
        Ok(())
    }

    fn poll(&mut self) {
        while let Some(event) = self.controller.next_event() {
            match event.trb_type() {
                TRB_TYPE_TRANSFER_EVENT => self.handle_transfer_event(event),
                TRB_TYPE_PORT_STATUS_CHANGE => self.controller.clear_port_changes(self.port_id),
                _ => {}
            }
        }
    }

    fn handle_transfer_event(&mut self, event: Trb) {
        if event.slot_id() != self.slot_id || event.endpoint_id() != self.interrupt_dci {
            return;
        }

        self.transfer_armed = false;
        let completion = event.completion_code();
        if matches!(completion, COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET) {
            let report = self.report_buffer.as_slice::<u8>(USB_REPORT_LEN);
            let mut snapshot = [0_u8; USB_REPORT_LEN];
            snapshot.copy_from_slice(report);
            self.apply_report(snapshot);
        } else {
            crate::debug::println!(
                "USB keyboard transfer event failed: code={}, slot={}, ep={}",
                completion,
                event.slot_id(),
                event.endpoint_id(),
            );
        }

        let _ = self.arm_interrupt_transfer();
    }

    fn apply_report(&mut self, report: [u8; USB_REPORT_LEN]) {
        let Some(current) = decode_boot_report(&report) else {
            return;
        };
        let previous = decode_boot_report(&self.last_report).unwrap_or_default();

        for code in previous.iter().copied() {
            if !current.contains(&code) {
                keyboard::inject_key_transition(code, true);
            }
        }
        for code in current.iter().copied() {
            if !previous.contains(&code) {
                keyboard::inject_key_transition(code, false);
            }
        }

        self.last_report = report;
    }
}

struct UsbKeyboardDevice {
    slot_id: u8,
    interrupt_dci: u8,
}

#[derive(Clone, Copy)]
struct BootKeyboardDescriptor {
    configuration_value: u8,
    interface_number: u8,
    endpoint_address: u8,
    max_packet_size: u16,
    interval: u8,
}

struct XhciController {
    pci: PciDevice,
    mmio_base: u64,
    op_base: u64,
    rt_base: u64,
    db_base: u64,
    max_slots: u8,
    max_ports: u8,
    ctx_size: usize,
    cmd_ring: ProducerRing,
    event_ring: EventRing,
    dcbaa: DmaBlock,
    scratchpad_array: Option<DmaBlock>,
    scratchpads: Vec<DmaBlock>,
    device_context: DmaBlock,
    input_context: DmaBlock,
    ep0_ring: ProducerRing,
    interrupt_ring: ProducerRing,
}

impl XhciController {
    fn new(pci: PciDevice) -> Result<Self, &'static str> {
        let mmio_phys = pci.bar0().ok_or("xHCI BAR0 unavailable")?;
        // Many laptops expose xHCI MMIO above the identity-mapped window, so
        // route all controller accesses through the kernel MMIO window.
        let mmio_base = crate::paging::mmio_addr(mmio_phys).ok_or("xHCI BAR0 is not mapped")?;
        pci.enable_memory_bus_master();

        let cap_length = unsafe { ptr::read_volatile(mmio_base as *const u8) } as usize;
        let hcsparams1 = mmio_read32(mmio_base, CAP_HCSPARAMS1);
        let hcsparams2 = mmio_read32(mmio_base, CAP_HCSPARAMS2);
        let hccparams1 = mmio_read32(mmio_base, CAP_HCCPARAMS1);
        let max_slots = (hcsparams1 & 0xff) as u8;
        let max_ports = ((hcsparams1 >> 24) & 0xff) as u8;
        let ctx_size = if (hccparams1 & (1 << 2)) != 0 { 64 } else { 32 };

        let mut controller = Self {
            pci,
            mmio_base,
            op_base: mmio_base + cap_length as u64,
            rt_base: mmio_base + (mmio_read32(mmio_base, CAP_RTSOFF) & !0x1f) as u64,
            db_base: mmio_base + (mmio_read32(mmio_base, CAP_DBOFF) & !0x3) as u64,
            max_slots,
            max_ports,
            ctx_size,
            cmd_ring: ProducerRing::new(XHCI_RING_TRBS)?,
            event_ring: EventRing::new(XHCI_RING_TRBS)?,
            dcbaa: DmaBlock::new((max_slots as usize + 1) * core::mem::size_of::<u64>(), 64)?,
            scratchpad_array: None,
            scratchpads: Vec::new(),
            device_context: DmaBlock::new(ctx_size * 32, 64)?,
            input_context: DmaBlock::new(ctx_size * 33, 64)?,
            ep0_ring: ProducerRing::new(XHCI_RING_TRBS)?,
            interrupt_ring: ProducerRing::new(XHCI_RING_TRBS)?,
        };

        controller.take_ownership(hccparams1);
        controller.stop_controller()?;
        controller.reset_controller()?;
        if controller.read_op32(OP_PAGESIZE) & 1 == 0 {
            return Err("xHCI does not support 4 KiB pages");
        }

        controller.setup_scratchpads(hcsparams2)?;
        controller.write_op64(OP_DCBAAP, controller.dcbaa.phys());
        controller.write_op64(OP_CRCR, controller.cmd_ring.base_phys() | 1);
        controller.write_intr32(INTR_ERSTSZ, 1);
        controller.write_intr64(INTR_ERSTBA, controller.event_ring.erst.phys());
        controller.event_ring.update_erdp(controller.rt_base);
        controller.write_op32(OP_CONFIG, controller.max_slots as u32);
        controller.run_controller()?;
        Ok(controller)
    }

    fn take_ownership(&self, hccparams1: u32) {
        let mut offset = (((hccparams1 >> 16) & 0xffff) as usize) * 4;
        for _ in 0..64 {
            if offset == 0 {
                return;
            }

            let cap = mmio_read32(self.mmio_base, offset);
            let cap_id = (cap & 0xff) as u8;
            let next = ((cap >> 8) & 0xff) as usize * 4;
            if cap_id == 1 {
                let mut legacy = mmio_read32(self.mmio_base, offset);
                if legacy & (1 << 16) != 0 {
                    legacy |= 1 << 24;
                    mmio_write32(self.mmio_base, offset, legacy);
                    let _ = wait_until(|| mmio_read32(self.mmio_base, offset) & (1 << 16) == 0);
                }
                mmio_write32(self.mmio_base, offset + 4, 0);
                return;
            }

            offset = next;
        }
    }

    fn setup_scratchpads(&mut self, hcsparams2: u32) -> Result<(), &'static str> {
        let count = (((hcsparams2 >> 27) & 0x1f) << 5) | ((hcsparams2 >> 21) & 0x1f);
        if count == 0 {
            return Ok(());
        }

        let array = DmaBlock::new(count as usize * core::mem::size_of::<u64>(), 64)?;
        let entries = array.as_mut_slice::<u64>(count as usize);
        for entry in entries.iter_mut() {
            *entry = 0;
        }

        for index in 0..count as usize {
            let buffer = DmaBlock::new(4096, 4096)?;
            entries[index] = buffer.phys();
            self.scratchpads.push(buffer);
        }

        self.dcbaa.as_mut_slice::<u64>(self.max_slots as usize + 1)[0] = array.phys();
        self.scratchpad_array = Some(array);
        Ok(())
    }

    fn stop_controller(&self) -> Result<(), &'static str> {
        let command = self.read_op32(OP_USBCMD) & !USBCMD_RUN_STOP;
        self.write_op32(OP_USBCMD, command);
        wait_until(|| self.read_op32(OP_USBSTS) & USBSTS_HCHALTED != 0).ok_or("xHCI failed to halt")
    }

    fn reset_controller(&self) -> Result<(), &'static str> {
        self.write_op32(OP_USBCMD, self.read_op32(OP_USBCMD) | USBCMD_HCRST);
        wait_until(|| self.read_op32(OP_USBCMD) & USBCMD_HCRST == 0)
            .ok_or("xHCI reset timed out")?;
        wait_until(|| self.read_op32(OP_USBSTS) & USBSTS_CNR == 0)
            .ok_or("xHCI controller never became ready")
    }

    fn run_controller(&self) -> Result<(), &'static str> {
        self.write_op32(OP_USBCMD, self.read_op32(OP_USBCMD) | USBCMD_RUN_STOP);
        wait_until(|| self.read_op32(OP_USBSTS) & USBSTS_HCHALTED == 0)
            .ok_or("xHCI failed to start")
    }

    fn enable_slot(&mut self) -> Result<u8, &'static str> {
        let completion = self.issue_command(Trb::command(TRB_TYPE_ENABLE_SLOT, 0, 0))?;
        if completion.completion_code() != COMPLETION_SUCCESS {
            return Err("Enable Slot failed");
        }
        Ok(completion.slot_id())
    }

    fn disable_slot(&mut self, slot_id: u8) -> Result<(), &'static str> {
        let completion = self.issue_command(Trb::command(
            TRB_TYPE_DISABLE_SLOT,
            0,
            (slot_id as u32) << TRB_SLOT_ID_SHIFT,
        ))?;
        if completion.completion_code() != COMPLETION_SUCCESS {
            return Err("Disable Slot failed");
        }
        Ok(())
    }

    fn address_device(&mut self, slot_id: u8, port_id: u8, speed: u8) -> Result<u16, &'static str> {
        self.device_context.clear();
        self.input_context.clear();
        self.dcbaa.as_mut_slice::<u64>(self.max_slots as usize + 1)[slot_id as usize] =
            self.device_context.phys();

        let input = self
            .input_context
            .as_mut_slice::<u32>(self.input_context.size / 4);
        input[0] = 0;
        input[1] = (1 << 0) | (1 << 1);

        let slot_base = self.ctx_size / 4;
        input[slot_base] = ((speed as u32) << 20) | (1 << 27);
        input[slot_base + 1] = (port_id as u32) << 16;

        let ep_base = (1 + 1) * (self.ctx_size / 4);
        let ep0_phys = self.ep0_ring.base_phys();
        input[ep_base + 1] =
            (3 << 1) | (EP_TYPE_CONTROL << 3) | ((default_ep0_packet_size(speed) as u32) << 16);
        input[ep_base + 2] = (ep0_phys as u32) | 1;
        input[ep_base + 3] = (ep0_phys >> 32) as u32;
        input[ep_base + 4] = 8;

        let completion = self.issue_command(Trb::command(
            TRB_TYPE_ADDRESS_DEVICE,
            self.input_context.phys(),
            (slot_id as u32) << TRB_SLOT_ID_SHIFT,
        ))?;
        if completion.completion_code() != COMPLETION_SUCCESS {
            return Err("Address Device failed");
        }

        Ok(default_ep0_packet_size(speed))
    }

    fn update_ep0_max_packet(
        &mut self,
        slot_id: u8,
        port_id: u8,
        speed: u8,
        max_packet: u16,
    ) -> Result<(), &'static str> {
        self.input_context.clear();
        let input = self
            .input_context
            .as_mut_slice::<u32>(self.input_context.size / 4);
        input[0] = 0;
        input[1] = 1 << 1;

        let slot_base = self.ctx_size / 4;
        input[slot_base] = ((speed as u32) << 20) | (1 << 27);
        input[slot_base + 1] = (port_id as u32) << 16;

        let ep_base = (1 + 1) * (self.ctx_size / 4);
        let ep0_phys = self.ep0_ring.base_phys();
        input[ep_base + 1] = (3 << 1) | (EP_TYPE_CONTROL << 3) | ((max_packet as u32) << 16);
        input[ep_base + 2] = (ep0_phys as u32) | 1;
        input[ep_base + 3] = (ep0_phys >> 32) as u32;
        input[ep_base + 4] = 8;

        let completion = self.issue_command(Trb::command(
            TRB_TYPE_EVALUATE_CONTEXT,
            self.input_context.phys(),
            (slot_id as u32) << TRB_SLOT_ID_SHIFT,
        ))?;
        if completion.completion_code() != COMPLETION_SUCCESS {
            return Err("Evaluate Context failed");
        }

        Ok(())
    }

    fn configure_interrupt_endpoint(
        &mut self,
        slot_id: u8,
        port_id: u8,
        speed: u8,
        descriptor: &BootKeyboardDescriptor,
    ) -> Result<(), &'static str> {
        self.input_context.clear();
        unsafe {
            ptr::copy_nonoverlapping(
                self.device_context.ptr.as_ptr(),
                self.input_context.ptr.as_ptr().add(self.ctx_size),
                self.ctx_size,
            );
        }

        let dci = endpoint_dci(descriptor.endpoint_address);
        let input = self
            .input_context
            .as_mut_slice::<u32>(self.input_context.size / 4);
        input[0] = 0;
        input[1] = (1 << 0) | (1 << dci);

        let slot_base = self.ctx_size / 4;
        input[slot_base] = ((speed as u32) << 20) | ((dci as u32) << 27);
        input[slot_base + 1] = (port_id as u32) << 16;

        let ep_base = (1 + dci as usize) * (self.ctx_size / 4);
        let ring_phys = self.interrupt_ring.base_phys();
        input[ep_base] = interval_value(speed, descriptor.interval) << 16;
        input[ep_base + 1] =
            (3 << 1) | (EP_TYPE_INTERRUPT_IN << 3) | ((descriptor.max_packet_size as u32) << 16);
        input[ep_base + 2] = (ring_phys as u32) | 1;
        input[ep_base + 3] = (ring_phys >> 32) as u32;
        input[ep_base + 4] =
            descriptor.max_packet_size as u32 | ((descriptor.max_packet_size as u32) << 16);

        let completion = self.issue_command(Trb::command(
            TRB_TYPE_CONFIGURE_ENDPOINT,
            self.input_context.phys(),
            (slot_id as u32) << TRB_SLOT_ID_SHIFT,
        ))?;
        if completion.completion_code() != COMPLETION_SUCCESS {
            return Err("Configure Endpoint failed");
        }

        Ok(())
    }

    fn get_descriptor<'a>(
        &mut self,
        slot_id: u8,
        buffer: &'a DmaBlock,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Result<&'a [u8], &'static str> {
        let target = buffer.as_mut_slice::<u8>(length as usize);
        for byte in target.iter_mut() {
            *byte = 0;
        }

        self.control_transfer(slot_id, request_type, request, value, index, target, true)?;
        Ok(target)
    }

    fn control_no_data(
        &mut self,
        slot_id: u8,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
    ) -> Result<(), &'static str> {
        self.control_transfer(slot_id, request_type, request, value, index, &mut [], false)
    }

    fn control_transfer(
        &mut self,
        slot_id: u8,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buffer: &mut [u8],
        direction_in: bool,
    ) -> Result<(), &'static str> {
        let setup = setup_packet(request_type, request, value, index, buffer.len() as u16);
        self.ep0_ring.enqueue(Trb::setup(
            u64::from_le_bytes(setup),
            buffer.is_empty(),
            direction_in,
        ));

        if !buffer.is_empty() {
            self.ep0_ring.enqueue(Trb::data(
                virt_to_phys(buffer.as_mut_ptr() as u64),
                buffer.len() as u32,
                direction_in,
            ));
        }

        let status_phys = self.ep0_ring.enqueue(Trb::status(!direction_in));
        self.ring_doorbell(slot_id, 1);

        let completion = self.wait_for_transfer(slot_id, 1, status_phys)?;
        if !matches!(
            completion.completion_code(),
            COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET
        ) {
            return Err("control transfer failed");
        }

        Ok(())
    }

    fn queue_transfer(&mut self, slot_id: u8, endpoint_id: u8, trb: Trb) {
        self.interrupt_ring.enqueue(trb);
        self.ring_doorbell(slot_id, endpoint_id);
    }

    fn issue_command(&mut self, trb: Trb) -> Result<Trb, &'static str> {
        let phys = self.cmd_ring.enqueue(trb);
        self.ring_doorbell(0, 0);
        self.wait_for_command(phys)
    }

    fn wait_for_command(&mut self, trb_phys: u64) -> Result<Trb, &'static str> {
        for _ in 0..XHCI_POLL_SPINS {
            if let Some(event) = self.next_event() {
                match event.trb_type() {
                    TRB_TYPE_COMMAND_COMPLETION if event.parameter == trb_phys => return Ok(event),
                    TRB_TYPE_TRANSFER_EVENT => {}
                    TRB_TYPE_PORT_STATUS_CHANGE => {
                        self.clear_port_changes(self.port_from_event(event))
                    }
                    _ => {}
                }
            }
            spin_loop();
        }

        Err("timed out waiting for xHCI command completion")
    }

    fn wait_for_transfer(
        &mut self,
        slot_id: u8,
        endpoint_id: u8,
        trb_phys: u64,
    ) -> Result<Trb, &'static str> {
        for _ in 0..XHCI_POLL_SPINS {
            if let Some(event) = self.next_event() {
                match event.trb_type() {
                    TRB_TYPE_TRANSFER_EVENT
                        if event.parameter == trb_phys
                            && event.slot_id() == slot_id
                            && event.endpoint_id() == endpoint_id =>
                    {
                        return Ok(event);
                    }
                    TRB_TYPE_PORT_STATUS_CHANGE => {
                        self.clear_port_changes(self.port_from_event(event))
                    }
                    _ => {}
                }
            }
            spin_loop();
        }

        Err("timed out waiting for xHCI transfer completion")
    }

    fn next_event(&mut self) -> Option<Trb> {
        self.event_ring.next(self.rt_base)
    }

    fn reset_port(&self, port_id: u8) -> Result<(), &'static str> {
        self.clear_port_changes(port_id);
        let mut portsc = self.read_portsc(port_id);
        portsc |= PORTSC_PP;
        self.write_portsc(port_id, portsc | PORTSC_CHANGE_BITS);
        if port_requires_reset(self.port_speed(port_id)) {
            self.write_portsc(port_id, portsc | PORTSC_PR | PORTSC_CHANGE_BITS);
            wait_until(|| {
                let current = self.read_portsc(port_id);
                current & PORTSC_PR == 0 && current & PORTSC_PED != 0
            })
            .ok_or("xHCI USB2 port reset timed out")?;
        } else if portsc & PORTSC_PED == 0 {
            wait_until(|| self.read_portsc(port_id) & PORTSC_PED != 0)
                .ok_or("xHCI port never reached enabled state")?;
        }
        self.clear_port_changes(port_id);
        Ok(())
    }

    fn clear_port_changes(&self, port_id: u8) {
        if port_id == 0 || port_id > self.max_ports {
            return;
        }
        let current = self.read_portsc(port_id);
        self.write_portsc(port_id, current | PORTSC_CHANGE_BITS);
    }

    fn port_from_event(&self, event: Trb) -> u8 {
        (((event.parameter >> 24) & 0xff) as u8).max(1)
    }

    fn port_speed(&self, port_id: u8) -> u8 {
        ((self.read_portsc(port_id) & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8
    }

    fn read_portsc(&self, port_id: u8) -> u32 {
        self.read_op32(OP_PORTSC_BASE + (port_id as usize - 1) * OP_PORTSC_STRIDE)
    }

    fn write_portsc(&self, port_id: u8, value: u32) {
        self.write_op32(
            OP_PORTSC_BASE + (port_id as usize - 1) * OP_PORTSC_STRIDE,
            value,
        );
    }

    fn ring_doorbell(&self, slot_id: u8, target: u8) {
        mmio_write32(self.db_base, slot_id as usize * 4, target as u32);
    }

    fn read_op32(&self, offset: usize) -> u32 {
        mmio_read32(self.op_base, offset)
    }

    fn write_op32(&self, offset: usize, value: u32) {
        mmio_write32(self.op_base, offset, value);
    }

    fn write_op64(&self, offset: usize, value: u64) {
        mmio_write64(self.op_base, offset, value);
    }

    fn write_intr32(&self, offset: usize, value: u32) {
        mmio_write32(self.rt_base + RT_INTR0 as u64, offset, value);
    }

    fn write_intr64(&self, offset: usize, value: u64) {
        mmio_write64(self.rt_base + RT_INTR0 as u64, offset, value);
    }
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn command(kind: u32, parameter: u64, control: u32) -> Self {
        Self {
            parameter,
            status: 0,
            control: control | (kind << TRB_TYPE_SHIFT),
        }
    }

    fn setup(packet: u64, no_data_stage: bool, direction_in: bool) -> Self {
        let transfer_type = if no_data_stage {
            0
        } else if direction_in {
            3
        } else {
            2
        };
        Self {
            parameter: packet,
            status: 8,
            control: TRB_IDT
                | (TRB_TYPE_SETUP_STAGE << TRB_TYPE_SHIFT)
                | (transfer_type << TRB_SETUP_TRANSFER_TYPE_SHIFT),
        }
    }

    fn data(buffer: u64, length: u32, direction_in: bool) -> Self {
        Self {
            parameter: buffer,
            status: length,
            control: (TRB_TYPE_DATA_STAGE << TRB_TYPE_SHIFT)
                | if direction_in { TRB_DATA_DIR_IN } else { 0 },
        }
    }

    fn status(direction_in: bool) -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: TRB_IOC
                | (TRB_TYPE_STATUS_STAGE << TRB_TYPE_SHIFT)
                | if direction_in { TRB_STATUS_DIR_IN } else { 0 },
        }
    }

    fn normal(buffer: u64, length: u32, interrupt_on_completion: bool) -> Self {
        Self {
            parameter: buffer,
            status: length,
            control: (TRB_TYPE_NORMAL << TRB_TYPE_SHIFT)
                | if interrupt_on_completion { TRB_IOC } else { 0 },
        }
    }

    fn link(target: u64, cycle: bool) -> Self {
        Self {
            parameter: target,
            status: 0,
            control: (TRB_TYPE_LINK << TRB_TYPE_SHIFT)
                | (1 << 1)
                | if cycle { TRB_CYCLE } else { 0 },
        }
    }

    fn trb_type(self) -> u32 {
        (self.control & TRB_TYPE_MASK) >> TRB_TYPE_SHIFT
    }

    fn completion_code(self) -> u8 {
        ((self.status >> 24) & 0xff) as u8
    }

    fn slot_id(self) -> u8 {
        ((self.control >> TRB_SLOT_ID_SHIFT) & 0xff) as u8
    }

    fn endpoint_id(self) -> u8 {
        ((self.control >> TRB_ENDPOINT_ID_SHIFT) & 0x1f) as u8
    }

    fn cycle(self) -> bool {
        (self.control & TRB_CYCLE) != 0
    }
}

struct ProducerRing {
    block: DmaBlock,
    trb_count: usize,
    enqueue_index: usize,
    cycle: bool,
}

impl ProducerRing {
    fn new(trb_count: usize) -> Result<Self, &'static str> {
        let mut ring = Self {
            block: DmaBlock::new(trb_count * core::mem::size_of::<Trb>(), 64)?,
            trb_count,
            enqueue_index: 0,
            cycle: true,
        };
        ring.update_link();
        Ok(ring)
    }

    fn base_phys(&self) -> u64 {
        self.block.phys()
    }

    fn enqueue(&mut self, mut trb: Trb) -> u64 {
        if self.cycle {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }

        let index = self.enqueue_index;
        self.trbs_mut()[index] = trb;
        let phys = self.base_phys() + (index * core::mem::size_of::<Trb>()) as u64;

        self.enqueue_index += 1;
        if self.enqueue_index == self.trb_count - 1 {
            self.update_link();
            self.enqueue_index = 0;
            self.cycle = !self.cycle;
        }

        phys
    }

    fn update_link(&mut self) {
        let link_index = self.trb_count - 1;
        self.trbs_mut()[link_index] = Trb::link(self.base_phys(), self.cycle);
    }

    fn trbs_mut(&mut self) -> &mut [Trb] {
        self.block.as_mut_slice::<Trb>(self.trb_count)
    }
}

#[repr(C, align(16))]
struct EventRingSegmentTableEntry {
    segment_base: u64,
    segment_size: u16,
    _reserved0: u16,
    _reserved1: u32,
}

struct EventRing {
    block: DmaBlock,
    erst: DmaBlock,
    trb_count: usize,
    dequeue_index: usize,
    cycle: bool,
}

impl EventRing {
    fn new(trb_count: usize) -> Result<Self, &'static str> {
        let block = DmaBlock::new(trb_count * core::mem::size_of::<Trb>(), 64)?;
        let erst = DmaBlock::new(core::mem::size_of::<EventRingSegmentTableEntry>(), 64)?;
        let mut ring = Self {
            block,
            erst,
            trb_count,
            dequeue_index: 0,
            cycle: true,
        };
        let entry = ring.erst.as_mut::<EventRingSegmentTableEntry>();
        *entry = EventRingSegmentTableEntry {
            segment_base: ring.block.phys(),
            segment_size: trb_count as u16,
            _reserved0: 0,
            _reserved1: 0,
        };
        Ok(ring)
    }

    fn next(&mut self, runtime_base: u64) -> Option<Trb> {
        let trb = self.block.as_slice::<Trb>(self.trb_count)[self.dequeue_index];
        if trb.cycle() != self.cycle {
            return None;
        }

        self.dequeue_index += 1;
        if self.dequeue_index == self.trb_count {
            self.dequeue_index = 0;
            self.cycle = !self.cycle;
        }
        self.update_erdp(runtime_base);
        Some(trb)
    }

    fn update_erdp(&self, runtime_base: u64) {
        let erdp = self.block.phys() + (self.dequeue_index * core::mem::size_of::<Trb>()) as u64;
        mmio_write64(runtime_base + RT_INTR0 as u64, INTR_ERDP, erdp | (1 << 3));
    }
}

struct DmaBlock {
    ptr: NonNull<u8>,
    size: usize,
}

unsafe impl Send for DmaBlock {}

impl DmaBlock {
    fn new(size: usize, align: usize) -> Result<Self, &'static str> {
        let layout = Layout::from_size_align(size.max(1), align).map_err(|_| "bad DMA layout")?;
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or("DMA allocation failed")?;
        Ok(Self { ptr, size })
    }

    fn phys(&self) -> u64 {
        virt_to_phys(self.ptr.as_ptr() as u64)
    }

    fn clear(&self) {
        unsafe {
            ptr::write_bytes(self.ptr.as_ptr(), 0, self.size);
        }
    }

    fn as_mut<T>(&mut self) -> &mut T {
        unsafe { &mut *(self.ptr.as_ptr() as *mut T) }
    }

    fn as_slice<T>(&self, len: usize) -> &[T] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr() as *const T, len) }
    }

    fn as_mut_slice<T>(&self, len: usize) -> &mut [T] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut T, len) }
    }
}

fn parse_boot_keyboard_interface(config: &[u8]) -> Option<BootKeyboardDescriptor> {
    let mut configuration_value = 0;
    let mut current_interface = None;
    let mut index = 0;

    while index + 2 <= config.len() {
        let len = config[index] as usize;
        if len == 0 || index + len > config.len() {
            break;
        }

        match config[index + 1] {
            DESC_CONFIGURATION if len >= 6 => configuration_value = config[index + 5],
            DESC_INTERFACE if len >= 9 => {
                let interface_number = config[index + 2];
                let alternate_setting = config[index + 3];
                let class = config[index + 5];
                let subclass = config[index + 6];
                let protocol = config[index + 7];
                current_interface = (alternate_setting == 0
                    && class == HID_CLASS
                    && subclass == HID_SUBCLASS_BOOT
                    && protocol == HID_PROTOCOL_KEYBOARD)
                    .then_some(interface_number);
            }
            DESC_ENDPOINT if len >= 7 => {
                if let Some(interface_number) = current_interface {
                    let endpoint_address = config[index + 2];
                    let attributes = config[index + 3] & 0x3;
                    if endpoint_address & 0x80 != 0 && attributes == ENDPOINT_ATTR_INTERRUPT {
                        return Some(BootKeyboardDescriptor {
                            configuration_value,
                            interface_number,
                            endpoint_address,
                            max_packet_size: u16::from_le_bytes([
                                config[index + 4],
                                config[index + 5],
                            ]) & 0x07ff,
                            interval: config[index + 6],
                        });
                    }
                }
            }
            _ => {}
        }

        index += len;
    }

    None
}

fn decode_boot_report(report: &[u8; USB_REPORT_LEN]) -> Option<Vec<KeyCode>> {
    // HID boot keyboards report rollover/error states with reserved usages.
    // Ignore those reports so we do not synthesize mass key releases.
    if report[2..]
        .iter()
        .any(|usage| matches!(*usage, 0x01..=0x03))
    {
        return None;
    }

    let mut keys = Vec::new();

    for (bit, code) in modifier_keys().iter().copied().enumerate() {
        if report[0] & (1 << bit) != 0 {
            let Some(code) = code else {
                continue;
            };
            keys.push(code);
        }
    }

    for usage in report[2..].iter().copied().filter(|usage| *usage != 0) {
        if let Some(code) = usb_usage_to_key_code(usage) {
            if !keys.contains(&code) {
                keys.push(code);
            }
        }
    }

    Some(keys)
}

fn modifier_keys() -> [Option<KeyCode>; 8] {
    [
        Some(KeyCode::LeftCtrl),
        Some(KeyCode::LeftShift),
        Some(KeyCode::LeftAlt),
        Some(KeyCode::LeftMeta),
        Some(KeyCode::RightCtrl),
        Some(KeyCode::RightShift),
        Some(KeyCode::RightAlt),
        Some(KeyCode::RightMeta),
    ]
}

fn usb_usage_to_key_code(usage: u8) -> Option<KeyCode> {
    Some(match usage {
        0x04 => KeyCode::A,
        0x05 => KeyCode::B,
        0x06 => KeyCode::C,
        0x07 => KeyCode::D,
        0x08 => KeyCode::E,
        0x09 => KeyCode::F,
        0x0a => KeyCode::G,
        0x0b => KeyCode::H,
        0x0c => KeyCode::I,
        0x0d => KeyCode::J,
        0x0e => KeyCode::K,
        0x0f => KeyCode::L,
        0x10 => KeyCode::M,
        0x11 => KeyCode::N,
        0x12 => KeyCode::O,
        0x13 => KeyCode::P,
        0x14 => KeyCode::Q,
        0x15 => KeyCode::R,
        0x16 => KeyCode::S,
        0x17 => KeyCode::T,
        0x18 => KeyCode::U,
        0x19 => KeyCode::V,
        0x1a => KeyCode::W,
        0x1b => KeyCode::X,
        0x1c => KeyCode::Y,
        0x1d => KeyCode::Z,
        0x1e => KeyCode::Digit1,
        0x1f => KeyCode::Digit2,
        0x20 => KeyCode::Digit3,
        0x21 => KeyCode::Digit4,
        0x22 => KeyCode::Digit5,
        0x23 => KeyCode::Digit6,
        0x24 => KeyCode::Digit7,
        0x25 => KeyCode::Digit8,
        0x26 => KeyCode::Digit9,
        0x27 => KeyCode::Digit0,
        0x28 => KeyCode::Enter,
        0x29 => KeyCode::Escape,
        0x2a => KeyCode::Backspace,
        0x2b => KeyCode::Tab,
        0x2c => KeyCode::Space,
        0x2d => KeyCode::Minus,
        0x2e => KeyCode::Equal,
        0x2f => KeyCode::LeftBracket,
        0x30 => KeyCode::RightBracket,
        0x31 => KeyCode::Backslash,
        0x33 => KeyCode::Semicolon,
        0x34 => KeyCode::Apostrophe,
        0x35 => KeyCode::Grave,
        0x36 => KeyCode::Comma,
        0x37 => KeyCode::Dot,
        0x38 => KeyCode::Slash,
        0x39 => KeyCode::CapsLock,
        0x3a => KeyCode::F1,
        0x3b => KeyCode::F2,
        0x3c => KeyCode::F3,
        0x3d => KeyCode::F4,
        0x3e => KeyCode::F5,
        0x3f => KeyCode::F6,
        0x40 => KeyCode::F7,
        0x41 => KeyCode::F8,
        0x42 => KeyCode::F9,
        0x43 => KeyCode::F10,
        0x44 => KeyCode::F11,
        0x45 => KeyCode::F12,
        0x46 => KeyCode::PrintScreen,
        0x47 => KeyCode::ScrollLock,
        0x48 => KeyCode::Pause,
        0x49 => KeyCode::Insert,
        0x4a => KeyCode::Home,
        0x4b => KeyCode::PageUp,
        0x4c => KeyCode::Delete,
        0x4d => KeyCode::End,
        0x4e => KeyCode::PageDown,
        0x4f => KeyCode::ArrowRight,
        0x50 => KeyCode::ArrowLeft,
        0x51 => KeyCode::ArrowDown,
        0x52 => KeyCode::ArrowUp,
        0x53 => KeyCode::NumLock,
        0x54 => KeyCode::NumpadSlash,
        0x55 => KeyCode::NumpadStar,
        0x56 => KeyCode::NumpadMinus,
        0x57 => KeyCode::NumpadPlus,
        0x58 => KeyCode::NumpadEnter,
        0x59 => KeyCode::Numpad1,
        0x5a => KeyCode::Numpad2,
        0x5b => KeyCode::Numpad3,
        0x5c => KeyCode::Numpad4,
        0x5d => KeyCode::Numpad5,
        0x5e => KeyCode::Numpad6,
        0x5f => KeyCode::Numpad7,
        0x60 => KeyCode::Numpad8,
        0x61 => KeyCode::Numpad9,
        0x62 => KeyCode::Numpad0,
        0x63 => KeyCode::NumpadDot,
        0x65 => KeyCode::Menu,
        _ => return None,
    })
}

fn port_requires_reset(speed: u8) -> bool {
    speed < 4
}

fn endpoint_dci(endpoint_address: u8) -> u8 {
    let endpoint_number = endpoint_address & 0x0f;
    let direction_in = endpoint_address & 0x80 != 0;
    if endpoint_number == 0 {
        1
    } else {
        endpoint_number * 2 + if direction_in { 1 } else { 0 }
    }
}

fn default_ep0_packet_size(speed: u8) -> u16 {
    match speed {
        3 => 64,
        4.. => 512,
        _ => 8,
    }
}

fn interval_value(speed: u8, interval: u8) -> u32 {
    match speed {
        1 | 2 => interval.saturating_add(3) as u32,
        _ => interval.saturating_sub(1) as u32,
    }
}

fn setup_packet(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
    [
        request_type,
        request,
        value as u8,
        (value >> 8) as u8,
        index as u8,
        (index >> 8) as u8,
        length as u8,
        (length >> 8) as u8,
    ]
}

fn wait_until<F: Fn() -> bool>(condition: F) -> Option<()> {
    for _ in 0..XHCI_POLL_SPINS {
        if condition() {
            return Some(());
        }
        spin_loop();
    }
    None
}

fn mmio_read32(base: u64, offset: usize) -> u32 {
    unsafe { ptr::read_volatile((base + offset as u64) as *const u32) }
}

fn mmio_write32(base: u64, offset: usize, value: u32) {
    unsafe {
        ptr::write_volatile((base + offset as u64) as *mut u32, value);
    }
}

fn mmio_write64(base: u64, offset: usize, value: u64) {
    unsafe {
        ptr::write_volatile((base + offset as u64) as *mut u64, value);
    }
}

fn virt_to_phys(addr: u64) -> u64 {
    if addr >= crate::paging::KERNEL_VIRT_OFFSET {
        addr - crate::paging::KERNEL_VIRT_OFFSET
    } else {
        addr
    }
}
