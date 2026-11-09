//! Protocol-level tests for the destructive PCI BAR sizing transaction.
//!
//! These replace the retired `pci-bar-discovery` TLA+ model. That model was a
//! nine-state straight line over Boolean flags; its five invariants are
//! properties of the *access sequence*, which TLC could only assert about the
//! model. Here the production `probe_bar_transaction` runs against a simulated
//! PCI function that checks each rule at the instant of the access:
//!
//! - `ProbeRequiresDecodeDisabled`: an all-ones size probe is written only
//!   while the I/O and memory decode bits are clear;
//! - `HighProbeRequiresRestoredLow`: the upper dword of a 64-bit BAR is probed
//!   only after the lower dword holds its original value again;
//! - `DecodeRequiresRestoredPair` / `EveryTerminalRestoresHardware`: when the
//!   transaction returns, every BAR dword and the command register equal their
//!   original values, whether a resource was published or rejected;
//! - `PublishedResourceIsValid`: a published resource has a power-of-two size
//!   and the original, size-aligned base;
//! - the lowest implemented mask bit defines the size, including for devices
//!   that implement fewer than 64 address bits and for a device that reports
//!   a nonsensical upper mask while its lower dword is still all ones;
//! - no write lands outside the header's standard BAR dwords.
//!
//! Each simulation is deterministic and the domain is enumerated rather than
//! sampled, so a failing combination is reproducible from its printed
//! description.

use super::{
    BAR0_OFFSET, BarConfig, COMMAND_BUS_MASTER, COMMAND_IO_SPACE, COMMAND_MEMORY_SPACE,
    COMMAND_OFFSET, PCI_BAR_IO_SPACE, PCI_BAR_MEM_TYPE_64, PCI_BAR_PREFETCH, PciResource,
    probe_bar_transaction,
};
use alloc::{format, string::String, vec, vec::Vec};

/// What one BAR dword (or dword pair) of the simulated function implements.
#[derive(Clone, Copy, Debug)]
enum Bar {
    Unimplemented,
    Io { size: u64, base: u32 },
    Mem32 { size: u64, base: u32, prefetch: bool },
    Mem64 { size: u64, base: u64, address_bits: u32, prefetch: bool },
}

struct Function {
    command: u16,
    original_command: u16,
    /// Header registers by dword index (offset / 4).
    registers: [u32; 16],
    original: [u32; 16],
    /// Writable-bit mask and read-only bits per BAR dword.
    writable: [u32; 16],
    fixed: [u32; 16],
    bar_dwords: usize,
    /// Report a bogus all-ones upper mask while the paired lower dword is not
    /// at its original value (observed on virtual devices).
    transient_upper_quirk: bool,
    /// Index of the lower dword of the 64-bit BAR, when one is present.
    mem64_low: Option<usize>,
    violations: Vec<String>,
}

impl Function {
    fn new(index: usize, bar: Bar, bar_dwords: usize, command: u16, quirk: bool) -> Self {
        let mut function = Function {
            command,
            original_command: command,
            registers: [0; 16],
            original: [0; 16],
            writable: [0; 16],
            fixed: [0; 16],
            bar_dwords,
            transient_upper_quirk: quirk,
            mem64_low: None,
            violations: Vec::new(),
        };
        let dword = BAR0_OFFSET as usize / 4 + index;
        match bar {
            Bar::Unimplemented => {}
            Bar::Io { size, base } => {
                function.fixed[dword] = PCI_BAR_IO_SPACE;
                function.writable[dword] = !(size as u32 - 1) & !0x3;
                function.registers[dword] = (base & function.writable[dword]) | PCI_BAR_IO_SPACE;
            }
            Bar::Mem32 { size, base, prefetch } => {
                let fixed = if prefetch { PCI_BAR_PREFETCH } else { 0 };
                function.fixed[dword] = fixed;
                function.writable[dword] = !(size as u32 - 1) & !0xf;
                function.registers[dword] = (base & function.writable[dword]) | fixed;
            }
            Bar::Mem64 { size, base, address_bits, prefetch } => {
                let fixed = PCI_BAR_MEM_TYPE_64 | if prefetch { PCI_BAR_PREFETCH } else { 0 };
                function.fixed[dword] = fixed;
                let address_mask = if address_bits == 64 { u64::MAX } else { (1u64 << address_bits) - 1 };
                let mask = !(size - 1) & address_mask & !0xf;
                function.writable[dword] = mask as u32;
                function.writable[dword + 1] = (mask >> 32) as u32;
                function.registers[dword] = ((base & mask) as u32) | fixed;
                function.registers[dword + 1] = ((base & mask) >> 32) as u32;
                function.mem64_low = Some(dword);
            }
        }
        function.original = function.registers;
        function
    }

    fn bar_window(&self) -> core::ops::Range<usize> {
        let first = BAR0_OFFSET as usize / 4;
        first..first + self.bar_dwords
    }

    fn decoding(&self) -> bool {
        self.command & (COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE) != 0
    }
}

impl BarConfig for Function {
    fn read16(&mut self, offset: u8) -> u16 {
        assert_eq!(offset, COMMAND_OFFSET, "only the command register is read as a word");
        self.command
    }

    fn write16(&mut self, offset: u8, value: u16) {
        assert_eq!(offset, COMMAND_OFFSET, "only the command register is written as a word");
        self.command = value;
    }

    fn read32(&mut self, offset: u8) -> u32 {
        let dword = usize::from(offset / 4);
        if !self.bar_window().contains(&dword) && dword != self.bar_window().end {
            self.violations.push(format!("read outside the BAR window at {offset:#x}"));
        }
        // A real function that is decoding while its BAR holds a size mask
        // would claim cycles at that bogus address; the read itself is fine.
        if self.transient_upper_quirk {
            if let Some(low) = self.mem64_low {
                if dword == low + 1 && self.registers[low] != self.original[low] {
                    return u32::MAX;
                }
            }
        }
        self.registers[dword]
    }

    fn write32(&mut self, offset: u8, value: u32) {
        let dword = usize::from(offset / 4);
        if !self.bar_window().contains(&dword) {
            self.violations.push(format!("write outside the BAR window at {offset:#x}"));
            return;
        }
        if value == u32::MAX && self.decoding() {
            self.violations
                .push(format!("all-ones probe of dword {dword} while decode is enabled"));
        }
        if let Some(low) = self.mem64_low {
            if dword == low + 1 && value == u32::MAX && self.registers[low] != self.original[low] {
                self.violations
                    .push("upper dword probed while the lower dword is not restored".into());
            }
        }
        self.registers[dword] = (value & self.writable[dword]) | self.fixed[dword];
    }

    fn bar_dwords(&mut self) -> usize {
        self.bar_dwords
    }
}

fn expected(bar: Bar, index: usize, bar_dwords: usize) -> Option<PciResource> {
    match bar {
        Bar::Unimplemented => None,
        Bar::Io { size, base } => Some(PciResource {
            start: u64::from(base & !(size as u32 - 1) & !0x3),
            size,
            is_io: true,
            prefetchable: false,
            is_64bit: false,
        }),
        Bar::Mem32 { size, base, prefetch } => Some(PciResource {
            start: u64::from(base & !(size as u32 - 1) & !0xf),
            size,
            is_io: false,
            prefetchable: prefetch,
            is_64bit: false,
        }),
        Bar::Mem64 { size, base, address_bits, prefetch } => {
            // A 64-bit BAR needs its upper dword inside the header.
            if index + 1 >= bar_dwords {
                return None;
            }
            let address_mask = if address_bits == 64 { u64::MAX } else { (1u64 << address_bits) - 1 };
            Some(PciResource {
                start: base & !(size - 1) & address_mask & !0xf,
                size,
                is_io: false,
                prefetchable: prefetch,
                is_64bit: true,
            })
        }
    }
}

fn bars() -> Vec<Bar> {
    let mut bars = vec![Bar::Unimplemented];
    for shift in [2u32, 4, 8, 12, 16] {
        for base in [0u32, 0xc000, 0x1_0000] {
            bars.push(Bar::Io { size: 1 << shift, base });
        }
    }
    for shift in [4u32, 12, 20, 23, 31] {
        for prefetch in [false, true] {
            for base in [0u32, 0x8000_0000, 0xfee0_0000] {
                bars.push(Bar::Mem32 { size: 1 << shift, base, prefetch });
            }
        }
    }
    for address_bits in [32u32, 36, 40, 48, 64] {
        for shift in [4u32, 12, 23, 31] {
            if shift >= address_bits {
                continue;
            }
            for prefetch in [false, true] {
                for base in [0u64, 0x8000_0000, 0x38_0000_0000 << 4, 0x1_0000_0000, 0xffff_8000_0000] {
                    bars.push(Bar::Mem64 { size: 1 << shift, base, address_bits, prefetch });
                }
            }
        }
    }
    bars
}

fn run(index: usize, bar: Bar, bar_dwords: usize, command: u16, quirk: bool) {
    let description = format!(
        "index={index} bar={bar:?} bar_dwords={bar_dwords} command={command:#06x} quirk={quirk}"
    );
    let mut function = Function::new(index, bar, bar_dwords, command, quirk);
    let resource = probe_bar_transaction(&mut function, index, index as u8);
    assert!(function.violations.is_empty(), "{description}: {:?}", function.violations);
    assert_eq!(function.command, function.original_command, "{description}: command restored");
    assert_eq!(function.registers, function.original, "{description}: BAR dwords restored");
    assert_eq!(resource, expected(bar, index, bar_dwords), "{description}");
    if let Some(resource) = resource {
        assert!(resource.size.is_power_of_two(), "{description}");
        assert_eq!(resource.start % resource.size, 0, "{description}: base aligned to size");
    }
}

#[test]
fn bar_sizing_obeys_the_probe_protocol_for_every_device_shape() {
    let commands = [
        0x0000,
        COMMAND_IO_SPACE,
        COMMAND_MEMORY_SPACE,
        COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE,
        COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER,
        0x0407, // I/O + memory + bus master + interrupt disable
    ];
    let mut cases = 0usize;
    for bar in bars() {
        for index in 0..6usize {
            for bar_dwords in [6usize, 2] {
                // `PciDevice::resource` refuses an index at or beyond the header
                // type's BAR count before the transaction starts, so the
                // transaction's domain is `index < bar_dwords`.
                if index >= bar_dwords {
                    continue;
                }
                for command in commands {
                    for quirk in [false, true] {
                        run(index, bar, bar_dwords, command, quirk);
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(cases > 20_000, "domain too small: {cases}");
}

#[test]
fn a_64bit_bar_in_the_last_dword_is_rejected_without_touching_the_next_register() {
    for command in [0, COMMAND_MEMORY_SPACE] {
        let bar = Bar::Mem64 { size: 1 << 20, base: 0x8000_0000, address_bits: 64, prefetch: false };
        let mut function = Function::new(5, bar, 6, command, false);
        assert_eq!(probe_bar_transaction(&mut function, 5, 5), None);
        assert!(function.violations.is_empty(), "{:?}", function.violations);
        assert_eq!(function.registers, function.original);
        assert_eq!(function.command, command);
    }
}
