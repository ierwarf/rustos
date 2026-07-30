//! Transactional PCI configuration and BAR resource discovery.
//!
//! - **Owner:** `kernel-hal` owns privileged PCI config-space mechanism.
//! - **Boundary:** Device-reported BAR masks, widths, and capabilities are
//!   untrusted hardware input.
//! - **Lifecycle:** Disable decode, probe, restore every touched register, then
//!   publish one admitted resource or no resource.
//! - **Concurrency:** BSP discovery is serialized; runtime mutations require
//!   an explicit owner lease.
//! - **Failure:** Overflow, malformed masks, unsupported layouts, and restore
//!   mismatch reject the resource without leaving decode state altered.
//! - **Forbidden:** No truncated BAR, guest-selected address, or partial
//!   command-register restore.
//! - **Evidence:** `pci-resource-discovery`.
use core::ptr;

use x86_64::instructions::port::Port;

const CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const CONFIG_DATA_PORT: u16 = 0x0cfc;

const COMMAND_OFFSET: u8 = 0x04;
const STATUS_OFFSET: u8 = 0x06;
const REVISION_OFFSET: u8 = 0x08;
const HEADER_TYPE_OFFSET: u8 = 0x0e;
const CLASS_CODE_OFFSET: u8 = 0x0b;
const SUBCLASS_OFFSET: u8 = 0x0a;
const PROG_IF_OFFSET: u8 = 0x09;
const SECONDARY_BUS_OFFSET: u8 = 0x19;
const SUBSYSTEM_VENDOR_OFFSET: u8 = 0x2c;
const SUBSYSTEM_DEVICE_OFFSET: u8 = 0x2e;
const INTERRUPT_LINE_OFFSET: u8 = 0x3c;
const INTERRUPT_PIN_OFFSET: u8 = 0x3d;
const BAR0_OFFSET: u8 = 0x10;
const CAPABILITIES_POINTER_OFFSET: u8 = 0x34;

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
const PCI_STATUS_CAPABILITIES_LIST: u16 = 1 << 4;
const PCI_CAP_ID_MSIX: u8 = 0x11;
const PCI_CAP_NEXT_OFFSET: u8 = 1;
const PCI_MSIX_CONTROL_OFFSET: u8 = 2;
const PCI_MSIX_TABLE_OFFSET: u8 = 4;
const PCI_MSIX_CAPABILITY_BYTES: usize = 12;
const PCI_MSIX_TABLE_BIR_MASK: u32 = 0x7;
const PCI_MSIX_TABLE_OFFSET_MASK: u32 = !PCI_MSIX_TABLE_BIR_MASK;
const PCI_MSIX_TABLE_SIZE_MASK: u16 = 0x07ff;
const PCI_MSIX_CONTROL_FUNCTION_MASK: u16 = 1 << 14;
const PCI_MSIX_CONTROL_ENABLE: u16 = 1 << 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciResource {
    pub start: u64,
    pub size: u64,
    pub is_io: bool,
    pub prefetchable: bool,
    pub is_64bit: bool,
}

/// One PCI MSI-X capability. The table BAR/offset is device-provided but
/// remains bounded by `table_resource()` before a driver can map it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsixCapability {
    config_offset: u8,
    table_bar: usize,
    table_offset: u64,
    table_entries: u16,
}

impl MsixCapability {
    pub const fn table_bar(self) -> usize {
        self.table_bar
    }

    pub const fn table_offset(self) -> u64 {
        self.table_offset
    }

    pub const fn table_entries(self) -> u16 {
        self.table_entries
    }

    /// Resolve the one table BAR while checking that every table entry fits.
    pub fn table_resource(self, device: PciDevice) -> Option<PciResource> {
        let resource = device.resource(self.table_bar)?;
        if resource.is_io {
            return None;
        }
        let table_bytes = u64::from(self.table_entries).checked_mul(16)?;
        let end = self.table_offset.checked_add(table_bytes)?;
        (end <= resource.size).then_some(resource)
    }

    /// Mask the entire function before table programming. The device owner
    /// must unmask the selected table entry before enabling the function.
    pub fn set_function_masked(self, device: PciDevice, masked: bool) {
        let mut control = device.read_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET);
        if masked {
            control |= PCI_MSIX_CONTROL_FUNCTION_MASK;
        } else {
            control &= !PCI_MSIX_CONTROL_FUNCTION_MASK;
        }
        device.write_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET, control);
    }

    /// Enable MSI-X only after a driver has populated and unmasked an owned
    /// table entry. Callers must never enable it as a legacy-IRQ fallback.
    pub fn set_enabled(self, device: PciDevice, enabled: bool) {
        let mut control = device.read_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET);
        if enabled {
            control |= PCI_MSIX_CONTROL_ENABLE;
        } else {
            control &= !PCI_MSIX_CONTROL_ENABLE;
        }
        device.write_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET, control);
    }
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

    /// Find the MSI-X capability using the conventional PCI capability list.
    /// Extended capabilities are intentionally not searched: MSI-X is a
    /// standard capability, and accepting an arbitrary extended structure
    /// would weaken this fixed transport substrate.
    pub fn msix_capability(self) -> Option<MsixCapability> {
        if self.header_type() != HEADER_TYPE_NORMAL
            || self.read_u16(STATUS_OFFSET) & PCI_STATUS_CAPABILITIES_LIST == 0
        {
            return None;
        }
        let mut offset = self.read_u8(CAPABILITIES_POINTER_OFFSET);
        for _ in 0..48 {
            if offset < 0x40
                || offset & 0x3 != 0
                || usize::from(offset)
                    .checked_add(PCI_MSIX_CAPABILITY_BYTES)
                    .is_none_or(|end| end > self.config_size() as usize)
            {
                return None;
            }
            let capability_id = self.read_u8(offset);
            let next = self.read_u8(offset + PCI_CAP_NEXT_OFFSET);
            if capability_id == PCI_CAP_ID_MSIX {
                let control = self.read_u16(offset + PCI_MSIX_CONTROL_OFFSET);
                let table = self.read_u32(offset + PCI_MSIX_TABLE_OFFSET);
                let table_bar = (table & PCI_MSIX_TABLE_BIR_MASK) as usize;
                if table_bar >= self.standard_bar_count() {
                    return None;
                }
                return Some(MsixCapability {
                    config_offset: offset,
                    table_bar,
                    table_offset: u64::from(table & PCI_MSIX_TABLE_OFFSET_MASK),
                    table_entries: (control & PCI_MSIX_TABLE_SIZE_MASK) + 1,
                });
            }
            if next == 0 {
                return None;
            }
            offset = next;
        }
        None
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
        // Restore the low half before touching the high half. Linux sizes the
        // standard BAR dwords independently while decode is disabled; leaving
        // all ones in the low half while probing a 64-bit partner can make a
        // virtual device expose a transient, nonsensical upper mask.
        self.write_u32(bar_offset, original_low);

        if (original_low & PCI_BAR_IO_SPACE) != 0 {
            return decode_io_resource(original_low, mask_low);
        }

        let is_64bit = (original_low & PCI_BAR_MEM_TYPE_MASK) == PCI_BAR_MEM_TYPE_64;
        if is_64bit && bar_index + 1 >= self.standard_bar_count() {
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

        decode_mem_resource(original_low, original_high, mask_low, mask_high, is_64bit)
    }
}

pub fn visit_devices(mut visit: impl FnMut(PciDevice) -> bool) {
    crate::arch::acpi::for_each_pci_bus_region(|segment, start_bus, end_bus| {
        let mut seen = [false; 256];
        let mut queue = [0u8; 256];
        let mut head = 0usize;
        let mut tail = 0usize;

        if start_bus <= end_bus {
            seen[start_bus as usize] = true;
            queue[tail] = start_bus;
            tail += 1;
        }

        while head < tail {
            let bus = queue[head];
            head += 1;

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
                    if pci.header_type() == HEADER_TYPE_BRIDGE {
                        let secondary_bus = pci.read_u8(SECONDARY_BUS_OFFSET);
                        if secondary_bus >= start_bus
                            && secondary_bus <= end_bus
                            && !seen[secondary_bus as usize]
                        {
                            seen[secondary_bus as usize] = true;
                            queue[tail] = secondary_bus;
                            tail += 1;
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
    let high_mask = if is_64bit { mask_high as u64 } else { 0 };

    let mask = if is_64bit {
        (high_mask << 32) | low_mask
    } else {
        low_mask
    };
    if mask == 0 {
        return None;
    }

    // The least significant implemented address bit is the BAR alignment and
    // therefore its size. This remains correct when a 64-bit BAR implements
    // fewer than 64 address bits and legitimately returns zero in upper mask
    // bits; two's-complement inversion incorrectly turns that into a huge BAR.
    let size = mask & mask.wrapping_neg();
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

#[cfg(test)]
mod tests {
    use super::{
        PCI_BAR_MEM_TYPE_64, PCI_MSIX_TABLE_BIR_MASK, PCI_MSIX_TABLE_OFFSET_MASK,
        decode_mem_resource,
    };

    #[test]
    fn msix_table_word_preserves_bar_and_page_aligned_offset() {
        let word = 0x0012_3003_u32;
        assert_eq!(word & PCI_MSIX_TABLE_BIR_MASK, 3);
        assert_eq!(word & PCI_MSIX_TABLE_OFFSET_MASK, 0x0012_3000);
    }

    #[test]
    fn mem64_bar_size_uses_the_lowest_implemented_mask_bit() {
        let low_only = decode_mem_resource(
            0x8000_0000 | PCI_BAR_MEM_TYPE_64,
            0,
            0xff80_0000 | PCI_BAR_MEM_TYPE_64,
            0,
            true,
        )
        .unwrap();
        assert_eq!(low_only.start, 0x8000_0000);
        assert_eq!(low_only.size, 8 * 1024 * 1024);
        assert!(low_only.is_64bit);

        let full_width = decode_mem_resource(
            PCI_BAR_MEM_TYPE_64,
            1,
            0xff80_0000 | PCI_BAR_MEM_TYPE_64,
            u32::MAX,
            true,
        )
        .unwrap();
        assert_eq!(full_width.start, 1_u64 << 32);
        assert_eq!(full_width.size, 8 * 1024 * 1024);
    }
}
