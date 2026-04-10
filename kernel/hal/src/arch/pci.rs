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
const REVISION_OFFSET: u8 = 0x08;
const HEADER_TYPE_OFFSET: u8 = 0x0e;
const CLASS_CODE_OFFSET: u8 = 0x0b;
const SUBCLASS_OFFSET: u8 = 0x0a;
const PROG_IF_OFFSET: u8 = 0x09;
const SUBSYSTEM_VENDOR_OFFSET: u8 = 0x2c;
const SUBSYSTEM_DEVICE_OFFSET: u8 = 0x2e;
const INTERRUPT_LINE_OFFSET: u8 = 0x3c;
const INTERRUPT_PIN_OFFSET: u8 = 0x3d;
const BAR0_OFFSET: u8 = 0x10;

const COMMAND_IO_SPACE: u16 = 1 << 0;
const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;

const HEADER_TYPE_MASK: u8 = 0x7f;
const HEADER_TYPE_NORMAL: u8 = 0x00;
const HEADER_TYPE_BRIDGE: u8 = 0x01;
const HEADER_TYPE_CARDBUS: u8 = 0x02;

const PCI_STD_NUM_BARS: usize = 6;
const PCI_BRIDGE_NUM_BARS: usize = 2;

const PCI_BAR_IO_SPACE: u32 = 1 << 0;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0x6;
const PCI_BAR_MEM_TYPE_64: u32 = 0x4;
const PCI_BAR_PREFETCH: u32 = 0x8;
const PCI_BAR_IO_ADDRESS_MASK: u32 = !0x3;
const PCI_BAR_MEM_ADDRESS_MASK: u32 = !0xf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciResource {
    pub start: u64,
    pub size: u64,
    pub is_io: bool,
    pub prefetchable: bool,
    pub is_64bit: bool,
}

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

    pub fn subsystem_vendor_id(self) -> u16 {
        if self.header_type() == HEADER_TYPE_NORMAL {
            self.read_u16(SUBSYSTEM_VENDOR_OFFSET)
        } else {
            0
        }
    }

    pub fn subsystem_device_id(self) -> u16 {
        if self.header_type() == HEADER_TYPE_NORMAL {
            self.read_u16(SUBSYSTEM_DEVICE_OFFSET)
        } else {
            0
        }
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

    pub fn class(self) -> u32 {
        ((self.class_code() as u32) << 16) | ((self.subclass() as u32) << 8) | self.prog_if() as u32
    }

    pub fn revision(self) -> u8 {
        self.read_u8(REVISION_OFFSET)
    }

    pub fn header_type(self) -> u8 {
        self.read_u8(HEADER_TYPE_OFFSET) & HEADER_TYPE_MASK
    }

    pub fn interrupt_line(self) -> u8 {
        self.read_u8(INTERRUPT_LINE_OFFSET)
    }

    pub fn interrupt_pin(self) -> u8 {
        self.read_u8(INTERRUPT_PIN_OFFSET)
    }

    pub fn devfn(self) -> u8 {
        (self.device << 3) | self.function
    }

    pub fn config_size(self) -> i32 {
        if crate::arch::acpi::pci_config_address(
            self.segment,
            self.bus,
            self.device,
            self.function,
            0x100,
        )
        .is_some()
        {
            4096
        } else {
            256
        }
    }

    pub fn is_present(self) -> bool {
        self.vendor_id() != 0xffff
    }

    pub fn enable_memory_bus_master(self) {
        self.update_command_bits(COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER, 0);
    }

    pub fn standard_bar_count(self) -> usize {
        match self.header_type() {
            HEADER_TYPE_NORMAL => PCI_STD_NUM_BARS,
            HEADER_TYPE_BRIDGE => PCI_BRIDGE_NUM_BARS,
            HEADER_TYPE_CARDBUS => 1,
            _ => 0,
        }
    }

    pub fn resource(self, bar_index: usize) -> Option<PciResource> {
        if bar_index >= self.standard_bar_count() {
            return None;
        }

        let bar_offset = BAR0_OFFSET + (bar_index as u8) * 4;
        let original_command = self.read_u16(COMMAND_OFFSET);
        let decoding_bits = original_command & (COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE);
        if decoding_bits != 0 {
            self.write_u16(
                COMMAND_OFFSET,
                original_command & !(COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE),
            );
        }

        let resource = self.read_resource_snapshot(bar_index, bar_offset);

        if decoding_bits != 0 {
            self.write_u16(COMMAND_OFFSET, original_command);
        }
        resource
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
        if let Some(addr) = crate::arch::acpi::pci_config_address(
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

    pub fn write_u8(self, offset: u8, value: u8) {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x3) * 8) as u32;
        let mask = !(0xff_u32 << shift);
        let current = self.read_u32(aligned);
        let next = (current & mask) | ((value as u32) << shift);
        self.write_u32(aligned, next);
    }

    pub fn write_u16(self, offset: u8, value: u16) {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x2) * 8) as u32;
        let mask = !(0xffff_u32 << shift);
        let current = self.read_u32(aligned);
        let next = (current & mask) | ((value as u32) << shift);
        self.write_u32(aligned, next);
    }

    pub fn update_command_bits(self, set_bits: u16, clear_bits: u16) -> u16 {
        let next = (self.read_u16(COMMAND_OFFSET) | set_bits) & !clear_bits;
        self.write_u16(COMMAND_OFFSET, next);
        next
    }

    pub fn write_u32(self, offset: u8, value: u32) {
        if let Some(addr) = crate::arch::acpi::pci_config_address(
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

    fn read_resource_snapshot(self, bar_index: usize, bar_offset: u8) -> Option<PciResource> {
        let original_low = self.read_u32(bar_offset);
        self.write_u32(bar_offset, u32::MAX);
        let mask_low = self.read_u32(bar_offset);
        let format_low = original_low | mask_low;

        if (format_low & PCI_BAR_IO_SPACE) != 0 {
            self.write_u32(bar_offset, original_low);
            return decode_io_resource(original_low, mask_low);
        }

        let is_64bit = (format_low & PCI_BAR_MEM_TYPE_MASK) == PCI_BAR_MEM_TYPE_64;
        if is_64bit && bar_index + 1 >= self.standard_bar_count() {
            self.write_u32(bar_offset, original_low);
            return None;
        }

        let original_high = if is_64bit {
            self.read_u32(bar_offset + 4)
        } else {
            0
        };

        let mask_high = if is_64bit {
            self.write_u32(bar_offset + 4, u32::MAX);
            self.read_u32(bar_offset + 4)
        } else {
            0
        };

        if is_64bit {
            self.write_u32(bar_offset + 4, original_high);
        }
        self.write_u32(bar_offset, original_low);

        decode_mem_resource(original_low, original_high, mask_low, mask_high, is_64bit)
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

fn usb_host_controller_kind(prog_if: u8) -> UsbHostControllerKind {
    match prog_if {
        PROG_IF_UHCI => UsbHostControllerKind::Uhci,
        PROG_IF_OHCI => UsbHostControllerKind::Ohci,
        PROG_IF_EHCI => UsbHostControllerKind::Ehci,
        PROG_IF_XHCI => UsbHostControllerKind::Xhci,
        other => UsbHostControllerKind::Unknown(other),
    }
}

pub fn visit_devices(mut visit: impl FnMut(PciDevice) -> bool) {
    crate::arch::acpi::for_each_pci_bus_region(|segment, start_bus, end_bus| {
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

                    if visit(pci) {
                        return true;
                    }
                }
            }
        }

        false
    });
}

pub fn visit_usb_controllers(mut visit: impl FnMut(PciDevice, UsbHostControllerKind) -> bool) {
    visit_devices(|pci| {
        if pci.class_code() == CLASS_SERIAL_BUS && pci.subclass() == SUBCLASS_USB {
            return visit(pci, usb_host_controller_kind(pci.prog_if()));
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

fn decode_io_resource(original_low: u32, mask_low: u32) -> Option<PciResource> {
    let mask = (mask_low & PCI_BAR_IO_ADDRESS_MASK) as u64;
    if mask == 0 {
        return None;
    }

    let size = (!mask).wrapping_add(1) & 0xffff_ffff;
    if size == 0 {
        return None;
    }

    Some(PciResource {
        start: (original_low & PCI_BAR_IO_ADDRESS_MASK) as u64,
        size,
        is_io: true,
        prefetchable: false,
        is_64bit: false,
    })
}

fn decode_mem_resource(
    original_low: u32,
    original_high: u32,
    mask_low: u32,
    mask_high: u32,
    is_64bit: bool,
) -> Option<PciResource> {
    let low_mask = (mask_low & PCI_BAR_MEM_ADDRESS_MASK) as u64;
    let mut high_mask = if is_64bit { mask_high as u64 } else { 0 };

    // Some firmware/QEMU combinations report an all-zero upper size probe for 64-bit BARs
    // even when the assigned BAR lives above 4GiB. Treat that as "all upper address bits are
    // implemented" so the computed size is driven by the meaningful low dword mask.
    if is_64bit && high_mask == 0 && original_high != 0 && low_mask != 0 {
        high_mask = u32::MAX as u64;
    }

    let mask = if is_64bit {
        (high_mask << 32) | low_mask
    } else {
        low_mask
    };
    if mask == 0 {
        return None;
    }

    let size = if is_64bit {
        (!mask).wrapping_add(1)
    } else {
        ((!mask & 0xffff_ffff).wrapping_add(1)) & 0xffff_ffff
    };
    if size == 0 {
        return None;
    }

    let start = if is_64bit {
        ((original_high as u64) << 32) | ((original_low & PCI_BAR_MEM_ADDRESS_MASK) as u64)
    } else {
        (original_low & PCI_BAR_MEM_ADDRESS_MASK) as u64
    };

    Some(PciResource {
        start,
        size,
        is_io: false,
        prefetchable: (original_low & PCI_BAR_PREFETCH) != 0,
        is_64bit,
    })
}
