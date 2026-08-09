//! Transactional PCI configuration and BAR resource discovery.
//!
//! - **Owner:** `kernel-hal` owns privileged PCI config-space mechanism.
//! - **Boundary:** Device-reported BAR masks, widths, and capabilities are
//!   untrusted hardware input.
//! - **Lifecycle:** One boot enumeration disables decode, probes, and restores
//!   every touched register, publishing one admitted resource or no resource
//!   per BAR. Sealing that pass removes the destructive path entirely, so a
//!   driver can never disturb a function another driver already owns.
//! - **Concurrency:** `CONFIG_STATE` serializes every configuration
//!   transaction. Both the legacy `0xcf8`/`0xcfc` address/data pair and the
//!   size probe itself are multi-access sequences, so two CPUs enumerating
//!   concurrently would otherwise publish one CPU's size mask as the other
//!   CPU's base address. Configuration access from interrupt context is
//!   forbidden because this lock does not mask local interrupts.
//! - **Failure:** Overflow, malformed masks, unsupported layouts, restore
//!   mismatch, and an exhausted snapshot table reject the resource without
//!   leaving decode state altered.
//! - **Forbidden:** No truncated BAR, guest-selected address, partial
//!   command-register restore, or repeated size probe of a BAR a driver has
//!   already claimed and mapped.
//! - **Evidence:** `pci-resource-discovery`.
use core::ptr;

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
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

/// Upper bound on distinct BARs the boot enumeration may record.
///
/// The pass sizes every standard BAR of every present function, so the bound
/// is a whole-topology one: the fixed guest presents well under a dozen
/// functions of at most six BARs each. Exhaustion means an unsupported
/// topology and fails the remaining resources closed rather than leaving them
/// to a later, unsealed probe.
const SIZED_BAR_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciResource {
    pub start: u64,
    pub size: u64,
    pub is_io: bool,
    pub prefetchable: bool,
    pub is_64bit: bool,
}

/// One BAR whose destructive size probe has already been executed. The
/// admitted resource is remembered together with the rejection, so a malformed
/// BAR is never reprobed either.
#[derive(Clone, Copy)]
struct SizedBar {
    config_key: u32,
    bar_index: u8,
    resource: Option<PciResource>,
}

struct ConfigState {
    sized_bars: [Option<SizedBar>; SIZED_BAR_CAPACITY],
    sized_bar_count: usize,
    enumeration_sealed: bool,
    attached: [Option<AttachedFunction>; ATTACHED_FUNCTION_CAPACITY],
}

impl ConfigState {
    const fn new() -> Self {
        Self {
            sized_bars: [None; SIZED_BAR_CAPACITY],
            sized_bar_count: 0,
            enumeration_sealed: false,
            attached: [None; ATTACHED_FUNCTION_CAPACITY],
        }
    }

    /// Admit one claim on a function, or refuse it.
    ///
    /// An exclusive claim excludes everything. A plain owner excludes every
    /// later owner. Only a claim that declared shared ownership admits further
    /// shared owners, which is the QNX rule that shared access must be
    /// declared by the first owner rather than assumed by the second.
    fn attach(&mut self, config_key: u32, mode: PciAttachMode) -> bool {
        if let Some(existing) = self
            .attached
            .iter_mut()
            .flatten()
            .find(|entry| entry.config_key == config_key)
        {
            if mode != PciAttachMode::SharedOwner
                || existing.mode != PciAttachMode::SharedOwner
                || existing.owners == u32::MAX
            {
                return false;
            }
            existing.owners += 1;
            return true;
        }
        let Some(slot) = self.attached.iter_mut().find(|entry| entry.is_none()) else {
            return false;
        };
        *slot = Some(AttachedFunction {
            config_key,
            mode,
            owners: 1,
        });
        true
    }

    fn detach(&mut self, config_key: u32) {
        for entry in self.attached.iter_mut() {
            let Some(attached) = entry else { continue };
            if attached.config_key != config_key {
                continue;
            }
            attached.owners -= 1;
            if attached.owners == 0 {
                *entry = None;
            }
            return;
        }
    }

    fn is_attached(&self, config_key: u32) -> bool {
        self.attached
            .iter()
            .flatten()
            .any(|entry| entry.config_key == config_key)
    }

    /// After the boot enumeration, a size probe is no longer an admissible
    /// operation on any function: every driver reads the snapshot instead.
    const fn probe_is_admissible(&self) -> bool {
        !self.enumeration_sealed && !self.is_full()
    }

    /// `Some(resource)` means this BAR was already sized; the inner `Option`
    /// carries the admitted resource or the remembered rejection.
    fn snapshot(&self, config_key: u32, bar_index: u8) -> Option<Option<PciResource>> {
        self.sized_bars[..self.sized_bar_count]
            .iter()
            .flatten()
            .find(|entry| entry.config_key == config_key && entry.bar_index == bar_index)
            .map(|entry| entry.resource)
    }

    const fn is_full(&self) -> bool {
        self.sized_bar_count >= SIZED_BAR_CAPACITY
    }

    fn seal(&mut self, config_key: u32, bar_index: u8, resource: Option<PciResource>) {
        if self.is_full() {
            return;
        }
        self.sized_bars[self.sized_bar_count] = Some(SizedBar {
            config_key,
            bar_index,
            resource,
        });
        self.sized_bar_count += 1;
    }
}

static CONFIG_STATE: TrackedSpinLock<ConfigState, { LockClass::PciConfigTransaction as u8 }> =
    TrackedSpinLock::new(ConfigState::new());

/// Upper bound on simultaneously attached functions. The fixed guest topology
/// attaches one function per DVM transport; exhaustion refuses the attach
/// rather than letting an unowned driver write configuration space.
const ATTACHED_FUNCTION_CAPACITY: usize = 32;

/// How a driver claims a function, following the QNX pci-server contract.
///
/// QNX requires attachment before any configuration or write operation on a
/// device, permits exactly one owner unless shared ownership is declared on
/// the first owning attach, and lets an exclusive attach forbid every later
/// one. RustOS needs the same three states: each DVM transport owns its
/// ivshmem function outright, and nothing else may reprogram it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciAttachMode {
    /// No other attach is admitted while this one lives.
    Exclusive,
    /// Sole owner, but a later owner is admitted only if this attach declared
    /// shared ownership.
    Owner,
    /// Owner that declares shared ownership up front.
    SharedOwner,
}

#[derive(Clone, Copy)]
struct AttachedFunction {
    config_key: u32,
    mode: PciAttachMode,
    owners: u32,
}

/// Proof that the holder owns one configuration function.
///
/// Configuration writes are reachable only through methods that take this
/// handle, so "a driver reprogrammed a function another driver owns" is not
/// an ordering rule to remember but a value the caller cannot fabricate.
/// Dropping it releases the claim.
pub struct PciAttach {
    device: PciDevice,
}

impl PciAttach {
    pub const fn device(&self) -> PciDevice {
        self.device
    }

    /// Keep this claim for the rest of the boot.
    ///
    /// A transport that has published its device owns it for the kernel's
    /// lifetime; releasing the claim at the end of the install transaction
    /// would reopen the function to a later driver's configuration writes.
    pub fn retain_permanent(self) {
        core::mem::forget(self);
    }

    /// Enable memory decode and bus mastering on the owned function.
    ///
    /// Reachable only from an attach because it is a configuration write: an
    /// unowned caller could otherwise re-enable decode on a function whose
    /// owner had deliberately quiesced it.
    pub fn enable_memory_bus_master(&self) {
        self.device.enable_memory_bus_master();
    }
}

/// Whether any driver currently claims this function.
pub fn function_is_attached(device: PciDevice) -> bool {
    CONFIG_STATE.lock().is_attached(device.config_key())
}

impl Drop for PciAttach {
    fn drop(&mut self) {
        CONFIG_STATE.lock().detach(self.device.config_key());
    }
}

/// Claim one function for exclusive or owning access.
///
/// Returns `None` when the function is already claimed incompatibly, which is
/// the observable form of the conflict that was previously invisible.
pub fn attach(device: PciDevice, mode: PciAttachMode) -> Option<PciAttach> {
    CONFIG_STATE
        .lock()
        .attach(device.config_key(), mode)
        .then_some(PciAttach { device })
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
    pub fn set_function_masked(self, attach: &PciAttach, masked: bool) {
        let device = attach.device();
        let mut control = device.read_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET);
        if masked {
            control |= PCI_MSIX_CONTROL_FUNCTION_MASK;
        } else {
            control &= !PCI_MSIX_CONTROL_FUNCTION_MASK;
        }
        device.write_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET, control);
        let observed = device.read_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET);
        assert_eq!(
            observed & PCI_MSIX_CONTROL_FUNCTION_MASK != 0,
            masked,
            "PCI MSI-X invariant: function-mask write did not complete"
        );
    }

    /// Enable MSI-X only after a driver has populated and unmasked an owned
    /// table entry. Callers must never enable it as a legacy-IRQ fallback.
    pub fn set_enabled(self, attach: &PciAttach, enabled: bool) {
        let device = attach.device();
        let mut control = device.read_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET);
        if enabled {
            control |= PCI_MSIX_CONTROL_ENABLE;
        } else {
            control &= !PCI_MSIX_CONTROL_ENABLE;
        }
        device.write_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET, control);
        let observed = device.read_u16(self.config_offset + PCI_MSIX_CONTROL_OFFSET);
        assert_eq!(
            observed & PCI_MSIX_CONTROL_ENABLE != 0,
            enabled,
            "PCI MSI-X invariant: enable write did not complete"
        );
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

    fn enable_memory_bus_master(self) {
        self.update_command_bits(COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER, 0);
    }

    pub fn standard_bar_count(self) -> usize {
        let _transaction = CONFIG_STATE.lock();
        self.standard_bar_count_raw()
    }

    fn standard_bar_count_raw(self) -> usize {
        match self.read_u8_raw(HEADER_TYPE_OFFSET) & HEADER_TYPE_MASK {
            HEADER_TYPE_NORMAL => PCI_STD_NUM_BARS,
            HEADER_TYPE_BRIDGE => PCI_BRIDGE_NUM_BARS,
            HEADER_TYPE_CARDBUS => 1,
            _ => 0,
        }
    }

    /// Resolve one BAR from the boot enumeration's snapshot.
    ///
    /// Sizing is destructive: it disables the function's decode and writes all
    /// ones into the BAR. Repeating it after a driver has mapped the aperture
    /// makes every concurrent read of that aperture observe an undecoded zero,
    /// which is indistinguishable from a peer that erased the shared region.
    /// Linux sizes every BAR once in `pci_read_bases()` during enumeration and
    /// serves `dev->resource[]` thereafter; this is the same contract, and
    /// after [`enumerate_and_seal_resources`] the probe cannot run at all.
    pub fn resource(self, bar_index: usize) -> Option<PciResource> {
        let bar_slot = u8::try_from(bar_index).ok()?;
        let config_key = self.config_key();
        let mut state = CONFIG_STATE.lock();
        if let Some(snapshot) = state.snapshot(config_key, bar_slot) {
            return snapshot;
        }
        if !state.probe_is_admissible() || bar_index >= self.standard_bar_count_raw() {
            return None;
        }
        let resource = self.probe_bar_locked(bar_index, bar_slot);
        state.seal(config_key, bar_slot, resource);
        resource
    }

    /// Record that a BAR index carries no independent resource, without
    /// probing it. Used for the upper dword of a 64-bit BAR.
    fn seal_absent_bar(self, bar_index: usize) {
        let Ok(bar_slot) = u8::try_from(bar_index) else {
            return;
        };
        let config_key = self.config_key();
        let mut state = CONFIG_STATE.lock();
        if state.snapshot(config_key, bar_slot).is_none() {
            state.seal(config_key, bar_slot, None);
        }
    }

    /// Run the destructive size probe with the configuration transaction held.
    fn probe_bar_locked(self, bar_index: usize, bar_slot: u8) -> Option<PciResource> {
        let bar_offset = BAR0_OFFSET + bar_slot * 4;
        let original_command = self.read_u16_raw(COMMAND_OFFSET);
        let decoding_bits = original_command & (COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE);
        if decoding_bits != 0 {
            self.write_u16_raw(
                COMMAND_OFFSET,
                original_command & !(COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE),
            );
        }

        let resource = self.read_resource_snapshot(bar_index, bar_offset);

        if decoding_bits != 0 {
            self.write_u16_raw(COMMAND_OFFSET, original_command);
        }
        resource
    }

    /// Dense identity of one configuration function, used to key the sealed
    /// BAR snapshots.
    const fn config_key(self) -> u32 {
        ((self.segment as u32) << 16)
            | ((self.bus as u32) << 8)
            | ((self.device as u32) << 3)
            | (self.function as u32)
    }

    pub fn read_u8(self, offset: u8) -> u8 {
        let _transaction = CONFIG_STATE.lock();
        self.read_u8_raw(offset)
    }

    pub fn read_u16(self, offset: u8) -> u16 {
        let _transaction = CONFIG_STATE.lock();
        self.read_u16_raw(offset)
    }

    pub fn read_u32(self, offset: u8) -> u32 {
        let _transaction = CONFIG_STATE.lock();
        self.read_u32_raw(offset)
    }

    fn read_u8_raw(self, offset: u8) -> u8 {
        let shift = ((offset & 0x3) * 8) as u32;
        ((self.read_u32_raw(offset & !0x3) >> shift) & 0xff) as u8
    }

    fn read_u16_raw(self, offset: u8) -> u16 {
        let shift = ((offset & 0x2) * 8) as u32;
        ((self.read_u32_raw(offset & !0x3) >> shift) & 0xffff) as u16
    }

    fn read_u32_raw(self, offset: u8) -> u32 {
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


    fn write_u16(self, offset: u8, value: u16) {
        let _transaction = CONFIG_STATE.lock();
        self.write_u16_raw(offset, value);
    }

    /// Read-modify-write the command register as one transaction. A split
    /// read and write would let a concurrent enumerator restore a stale
    /// command word and leave the function permanently undecoded.
    fn update_command_bits(self, set_bits: u16, clear_bits: u16) -> u16 {
        let _transaction = CONFIG_STATE.lock();
        let next = (self.read_u16_raw(COMMAND_OFFSET) | set_bits) & !clear_bits;
        self.write_u16_raw(COMMAND_OFFSET, next);
        next
    }



    fn write_u16_raw(self, offset: u8, value: u16) {
        let aligned = offset & !0x3;
        let shift = ((offset & 0x2) * 8) as u32;
        let mask = !(0xffff_u32 << shift);
        let current = self.read_u32_raw(aligned);
        let next = (current & mask) | ((value as u32) << shift);
        self.write_u32_raw(aligned, next);
    }

    fn write_u32_raw(self, offset: u8, value: u32) {
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

    /// Size one BAR while the caller holds the configuration transaction and
    /// has already disabled the function's decode.
    fn read_resource_snapshot(self, bar_index: usize, bar_offset: u8) -> Option<PciResource> {
        let original_low = self.read_u32_raw(bar_offset);
        self.write_u32_raw(bar_offset, u32::MAX);
        let mask_low = self.read_u32_raw(bar_offset);
        // Restore the low half before touching the high half. Linux sizes the
        // standard BAR dwords independently while decode is disabled; leaving
        // all ones in the low half while probing a 64-bit partner can make a
        // virtual device expose a transient, nonsensical upper mask.
        self.write_u32_raw(bar_offset, original_low);

        if (original_low & PCI_BAR_IO_SPACE) != 0 {
            return decode_io_resource(original_low, mask_low);
        }

        let is_64bit = (original_low & PCI_BAR_MEM_TYPE_MASK) == PCI_BAR_MEM_TYPE_64;
        if is_64bit && bar_index + 1 >= self.standard_bar_count_raw() {
            return None;
        }

        let original_high = if is_64bit {
            self.read_u32_raw(bar_offset + 4)
        } else {
            0
        };

        let mask_high = if is_64bit {
            self.write_u32_raw(bar_offset + 4, u32::MAX);
            self.read_u32_raw(bar_offset + 4)
        } else {
            0
        };

        if is_64bit {
            self.write_u32_raw(bar_offset + 4, original_high);
        }

        decode_mem_resource(original_low, original_high, mask_low, mask_high, is_64bit)
    }
}

/// Size every function's BARs once, then forbid any further size probe.
///
/// This is the boot enumeration Linux performs in `pci_read_bases()`: the one
/// point in the system's life at which writing all ones into a BAR is safe,
/// because no driver has claimed a function, mapped an aperture, or armed an
/// interrupt yet. Sealing afterwards turns "a driver must not disturb another
/// driver's live device" from a convention into a structural property: the
/// destructive path no longer exists for anyone to reach.
///
/// Callers must run this after ACPI publishes the PCI bus regions and before
/// the first driver probe. Returns the number of BARs recorded.
pub fn enumerate_and_seal_resources() -> usize {
    let mut recorded = 0usize;
    visit_devices(|device| {
        let bar_count = device.standard_bar_count();
        let mut bar_index = 0;
        while bar_index < bar_count {
            let resource = device.resource(bar_index);
            if resource.is_some() {
                recorded += 1;
            }
            // A 64-bit BAR owns the next dword. Linux advances past it during
            // enumeration; decoding it as an independent BAR would publish the
            // upper half of an address as a base of its own.
            if resource.is_some_and(|resource| resource.is_64bit) {
                device.seal_absent_bar(bar_index + 1);
                bar_index += 2;
            } else {
                bar_index += 1;
            }
        }
        false
    });
    let mut state = CONFIG_STATE.lock();
    state.enumeration_sealed = true;
    recorded
}

/// Whether the boot enumeration has closed the size-probe path.
pub fn resource_enumeration_is_sealed() -> bool {
    CONFIG_STATE.lock().enumeration_sealed
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
        ATTACHED_FUNCTION_CAPACITY, ConfigState, PCI_BAR_MEM_TYPE_64, PCI_MSIX_TABLE_BIR_MASK,
        PCI_MSIX_TABLE_OFFSET_MASK, PciAttachMode, PciDevice, PciResource, SIZED_BAR_CAPACITY,
        decode_mem_resource,
    };

    const ADMITTED: PciResource = PciResource {
        start: 0x3800_0000_0000,
        size: 0x0080_0000,
        is_io: false,
        prefetchable: true,
        is_64bit: true,
    };

    fn function(segment: u16, bus: u8, device: u8, function: u8) -> u32 {
        PciDevice {
            segment,
            bus,
            device,
            function,
        }
        .config_key()
    }

    #[test]
    fn config_key_separates_every_function_of_the_same_device() {
        let keys = [
            function(0, 0, 6, 0),
            function(0, 0, 6, 1),
            function(0, 0, 7, 0),
            function(0, 1, 6, 0),
            function(1, 0, 6, 0),
        ];
        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(key));
        }
    }

    #[test]
    fn a_sealed_bar_is_never_probed_again_and_remembers_its_rejection() {
        let mut state = ConfigState::new();
        let block = function(0, 0, 6, 0);
        let display = function(0, 0, 7, 0);
        assert!(state.snapshot(block, 2).is_none());

        state.seal(block, 2, Some(ADMITTED));
        state.seal(block, 0, None);

        assert_eq!(state.snapshot(block, 2), Some(Some(ADMITTED)));
        // A rejection is a completed probe: repeating it would disable a live
        // function's decode a second time.
        assert_eq!(state.snapshot(block, 0), Some(None));
        assert!(state.snapshot(block, 1).is_none());
        assert!(state.snapshot(display, 2).is_none());
    }

    #[test]
    fn an_exhausted_snapshot_table_stops_admitting_new_probes() {
        let mut state = ConfigState::new();
        for slot in 0..SIZED_BAR_CAPACITY {
            assert!(state.probe_is_admissible());
            state.seal(slot as u32, 0, Some(ADMITTED));
        }
        assert!(state.is_full());
        assert!(!state.probe_is_admissible());

        state.seal(u32::MAX, 0, Some(ADMITTED));
        assert!(state.snapshot(u32::MAX, 0).is_none());
        assert_eq!(state.snapshot(0, 0), Some(Some(ADMITTED)));
    }

    #[test]
    fn an_exclusive_claim_excludes_every_later_claim_until_it_is_released() {
        let mut state = ConfigState::new();
        let block = function(0, 0, 6, 0);
        let display = function(0, 0, 7, 0);
        assert!(state.attach(block, PciAttachMode::Exclusive));
        assert!(state.is_attached(block));

        assert!(!state.attach(block, PciAttachMode::Exclusive));
        assert!(!state.attach(block, PciAttachMode::Owner));
        assert!(!state.attach(block, PciAttachMode::SharedOwner));
        // A different function is unaffected.
        assert!(state.attach(display, PciAttachMode::Owner));

        state.detach(block);
        assert!(!state.is_attached(block));
        assert!(state.attach(block, PciAttachMode::Owner));
    }

    #[test]
    fn shared_ownership_must_be_declared_by_the_first_owner() {
        let mut state = ConfigState::new();
        let sole = function(0, 0, 1, 0);
        let shared = function(0, 0, 2, 0);

        // A plain owner refuses a second owner, exactly as QNX refuses a later
        // OWNER attach when MULTI was not set on the first one.
        assert!(state.attach(sole, PciAttachMode::Owner));
        assert!(!state.attach(sole, PciAttachMode::SharedOwner));

        assert!(state.attach(shared, PciAttachMode::SharedOwner));
        assert!(state.attach(shared, PciAttachMode::SharedOwner));
        // An exclusive claim can never join an existing owner.
        assert!(!state.attach(shared, PciAttachMode::Exclusive));

        // The claim survives until the last shared owner releases it.
        state.detach(shared);
        assert!(state.is_attached(shared));
        state.detach(shared);
        assert!(!state.is_attached(shared));
        assert!(state.attach(shared, PciAttachMode::Exclusive));
    }

    #[test]
    fn an_exhausted_attach_table_refuses_the_claim_rather_than_admitting_it() {
        let mut state = ConfigState::new();
        for slot in 0..ATTACHED_FUNCTION_CAPACITY {
            assert!(state.attach(slot as u32, PciAttachMode::Exclusive));
        }
        assert!(!state.attach(u32::MAX, PciAttachMode::Exclusive));
        assert!(!state.is_attached(u32::MAX));
    }

    #[test]
    fn sealing_the_enumeration_closes_the_probe_path_but_not_the_snapshot() {
        let mut state = ConfigState::new();
        let block = function(0, 0, 6, 0);
        state.seal(block, 2, Some(ADMITTED));
        assert!(state.probe_is_admissible());

        state.enumeration_sealed = true;
        // A driver scanning after the seal still resolves what enumeration
        // recorded, and can no longer reach the destructive probe for anything
        // it did not record - including a function it does not own.
        assert_eq!(state.snapshot(block, 2), Some(Some(ADMITTED)));
        assert!(state.snapshot(block, 1).is_none());
        assert!(!state.probe_is_admissible());
    }

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
