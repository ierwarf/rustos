use core::ptr;

use x86_64::instructions::port::Port;

const CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const CONFIG_DATA_PORT: u16 = 0x0cfc;

const CLASS_SERIAL_BUS: u8 = 0x0c;
const SUBCLASS_USB: u8 = 0x03;
const PROG_IF_UHCI: u8 = 0x00;
const PROG_IF_OHCI: u8 = 0x10;
const PROG_IF_EHCI: u8 = 0x20;
const PROG_IF_XHCI: u8 = 0x30;

const COMMAND_OFFSET: u8 = 0x04;
const HEADER_TYPE_OFFSET: u8 = 0x0e;
const CLASS_CODE_OFFSET: u8 = 0x0b;
const SUBCLASS_OFFSET: u8 = 0x0a;
const PROG_IF_OFFSET: u8 = 0x09;
const BAR0_OFFSET: u8 = 0x10;

const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDevice {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciDevice {
    pub fn vendor_id(self) -> u16 {
        self.read_u16(0x00)
    }

    pub fn device_id(self) -> u16 {
        self.read_u16(0x02)
    }

    pub fn class_code(self) -> u8 {
        self.read_u8(CLASS_CODE_OFFSET)
    }

    pub fn subclass(self) -> u8 {
        self.read_u8(SUBCLASS_OFFSET)
    }

    pub fn prog_if(self) -> u8 {
        self.read_u8(PROG_IF_OFFSET)
    }

    pub fn is_present(self) -> bool {
        self.vendor_id() != 0xffff
    }

    pub fn enable_memory_bus_master(self) {
        let command = self.read_u16(COMMAND_OFFSET) | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER;
        self.write_u16(COMMAND_OFFSET, command);
    }

    pub fn bar0(self) -> Option<u64> {
        let low = self.read_u32(BAR0_OFFSET);
        if low == 0 || (low & 1) != 0 {
            return None;
        }

        let bar_type = (low >> 1) & 0x3;
        let addr_low = (low & !0xf) as u64;
        if bar_type == 0x2 {
            let high = self.read_u32(BAR0_OFFSET + 4) as u64;
            Some((high << 32) | addr_low)
        } else {
            Some(addr_low)
        }
    }

    pub fn read_u8(self, offset: u8) -> u8 {
        let shift = ((offset & 0x3) * 8) as u32;
        ((self.read_u32(offset & !0x3) >> shift) & 0xff) as u8
    }

    pub fn read_u16(self, offset: u8) -> u16 {
        let shift = ((offset & 0x2) * 8) as u32;
        ((self.read_u32(offset & !0x3) >> shift) & 0xffff) as u16
    }

    pub fn read_u32(self, offset: u8) -> u32 {
        if let Some(addr) = crate::acpi::pci_config_address(
            self.segment,
            self.bus,
            self.device,
            self.function,
            offset as usize,
        ) {
            return unsafe { ptr::read_volatile(addr as *const u32) };
        }

        unsafe {
            let mut address_port = Port::<u32>::new(CONFIG_ADDRESS_PORT);
            let mut data_port = Port::<u32>::new(CONFIG_DATA_PORT);
            address_port.write(config_address(self.bus, self.device, self.function, offset));
            data_port.read()
        }
    }

    pub fn write_u16(self, offset: u8, value: u16) {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x2) * 8) as u32;
        let mask = !(0xffff_u32 << shift);
        let current = self.read_u32(aligned);
        let next = (current & mask) | ((value as u32) << shift);
        self.write_u32(aligned, next);
    }

    pub fn write_u32(self, offset: u8, value: u32) {
        if let Some(addr) = crate::acpi::pci_config_address(
            self.segment,
            self.bus,
            self.device,
            self.function,
            offset as usize,
        ) {
            unsafe {
                ptr::write_volatile(addr as *mut u32, value);
            }
            return;
        }

        unsafe {
            let mut address_port = Port::<u32>::new(CONFIG_ADDRESS_PORT);
            let mut data_port = Port::<u32>::new(CONFIG_DATA_PORT);
            address_port.write(config_address(self.bus, self.device, self.function, offset));
            data_port.write(value);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbHostControllerKind {
    Uhci,
    Ohci,
    Ehci,
    Xhci,
    Unknown(u8),
}

impl UsbHostControllerKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Uhci => "UHCI",
            Self::Ohci => "OHCI",
            Self::Ehci => "EHCI",
            Self::Xhci => "xHCI",
            Self::Unknown(_) => "USB",
        }
    }
}

fn usb_host_controller_kind(prog_if: u8) -> UsbHostControllerKind {
    match prog_if {
        PROG_IF_UHCI => UsbHostControllerKind::Uhci,
        PROG_IF_OHCI => UsbHostControllerKind::Ohci,
        PROG_IF_EHCI => UsbHostControllerKind::Ehci,
        PROG_IF_XHCI => UsbHostControllerKind::Xhci,
        other => UsbHostControllerKind::Unknown(other),
    }
}

pub fn visit_usb_controllers(mut visit: impl FnMut(PciDevice, UsbHostControllerKind) -> bool) {
    crate::acpi::for_each_pci_bus_region(|segment, start_bus, end_bus| {
        for bus in start_bus..=end_bus {
            for device in 0..32 {
                let function0 = PciDevice {
                    segment,
                    bus,
                    device,
                    function: 0,
                };
                if !function0.is_present() {
                    continue;
                }

                let header_type = function0.read_u8(HEADER_TYPE_OFFSET);
                let function_count = if (header_type & 0x80) != 0 { 8 } else { 1 };
                for function in 0..function_count {
                    let pci = PciDevice {
                        segment,
                        bus,
                        device,
                        function,
                    };
                    if !pci.is_present() {
                        continue;
                    }

                    if pci.class_code() == CLASS_SERIAL_BUS && pci.subclass() == SUBCLASS_USB {
                        let kind = usb_host_controller_kind(pci.prog_if());
                        if visit(pci, kind) {
                            return true;
                        }
                    }
                }
            }
        }

        false
    });
}

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xfc)
}
