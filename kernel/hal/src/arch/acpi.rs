//! Bounded ACPI topology admission for kernel hardware discovery.
//!
//! - **Owner:** `kernel-hal` owns checksummed firmware-table decoding.
//! - **Boundary:** Firmware addresses, lengths, signatures, and resource
//!   descriptors are untrusted until complete-table admission succeeds.
//! - **Lifecycle:** Tables are staged, validated atomically, then published as
//!   immutable topology; partial tables never mutate live state.
//! - **Concurrency:** Admission is boot-serialized on the BSP.
//! - **Failure:** Malformed optional topology is rejected with an explicit
//!   bounded fallback only where the architecture contract permits it.
//! - **Forbidden:** No unchecked firmware pointer, partial MCFG publication,
//!   or fabricated timer topology.
//! - **Evidence:** `acpi-firmware-admission` and
//!   `monotonic-deadline-lifecycle`.
use boot_protocol::BootInfo;
use spin::Mutex;

const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;
const SDT_HEADER_LEN: usize = 36;
const MCFG_HEADER_LEN: usize = 44;
const MCFG_ENTRY_LEN: usize = 16;
const HPET_TABLE_LEN: usize = 56;
const ACPI_ADDRESS_SPACE_SYSTEM_MEMORY: u8 = 0;
const ACPI_GAS_ACCESS_QWORD: u8 = 4;
const MAX_MCFG_REGIONS: usize = 8;
const MAX_RSDP_BYTES: usize = 4096;
const MAX_ACPI_SDT_BYTES: usize = 1024 * 1024;
const PCI_ECAM_BUS_BYTES: u64 = 1 << 20;
const IDENTITY_MAPPED_PHYS_LIMIT: u64 = 512 * 1024 * 1024 * 1024;

static ACPI_STATE: Mutex<AcpiState> = Mutex::new(AcpiState::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciConfigRegion {
    pub base_address: u64,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

impl PciConfigRegion {
    const fn empty() -> Self {
        Self {
            base_address: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 0,
        }
    }

    fn contains(self, segment: u16, bus: u8) -> bool {
        self.base_address != 0
            && self.segment == segment
            && bus >= self.start_bus
            && bus <= self.end_bus
    }

    fn config_address(self, bus: u8, device: u8, function: u8, offset: usize) -> Option<u64> {
        if self.base_address == 0
            || bus < self.start_bus
            || bus > self.end_bus
            || device >= 32
            || function >= 8
            || offset >= 4096
        {
            return None;
        }
        let bus_offset = u64::from(bus.checked_sub(self.start_bus)?).checked_mul(1 << 20)?;
        let device_offset = u64::from(device).checked_mul(1 << 15)?;
        let function_offset = u64::from(function).checked_mul(1 << 12)?;
        let address = self
            .base_address
            .checked_add(bus_offset)?
            .checked_add(device_offset)?
            .checked_add(function_offset)?
            .checked_add(offset as u64)?;
        (address < IDENTITY_MAPPED_PHYS_LIMIT).then_some(address)
    }

    fn overlaps(self, other: Self) -> bool {
        self.segment == other.segment
            && self.start_bus <= other.end_bus
            && other.start_bus <= self.end_bus
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcpiState {
    rsdp_addr: u64,
    hpet_address: u64,
    region_count: usize,
    regions: [PciConfigRegion; MAX_MCFG_REGIONS],
}

impl AcpiState {
    const fn new() -> Self {
        Self {
            rsdp_addr: 0,
            hpet_address: 0,
            region_count: 0,
            regions: [PciConfigRegion::empty(); MAX_MCFG_REGIONS],
        }
    }

    fn reset(&mut self, rsdp_addr: u64) {
        self.rsdp_addr = rsdp_addr;
        self.hpet_address = 0;
        self.region_count = 0;
        self.regions = [PciConfigRegion::empty(); MAX_MCFG_REGIONS];
    }

    fn push_region(&mut self, region: PciConfigRegion) -> bool {
        if self.region_count >= self.regions.len()
            || self.regions[..self.region_count]
                .iter()
                .any(|current| current.overlaps(region))
        {
            return false;
        }

        self.regions[self.region_count] = region;
        self.region_count += 1;
        true
    }
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let Some(boot_info) = boot_info_from_ptr(boot_info_ptr) else {
        return;
    };

    let mut state = ACPI_STATE.lock();
    state.reset(boot_info.acpi_rsdp_addr);

    if state.rsdp_addr == 0 {
        crate::debug::println!("ACPI RSDP unavailable.");
        return;
    }

    let rsdp_addr = state.rsdp_addr;
    if load_mcfg_regions(rsdp_addr, &mut state) {
        crate::debug::println!(
            "ACPI MCFG loaded from {:#x}: {} region(s).",
            state.rsdp_addr,
            state.region_count,
        );
    } else {
        crate::debug::println!("ACPI MCFG unavailable; falling back to legacy PCI config access.");
    }
    state.hpet_address = load_hpet_address(rsdp_addr).unwrap_or(0);
    if state.hpet_address != 0 {
        crate::debug::println!("ACPI HPET available at {:#x}.", state.hpet_address);
    } else {
        crate::debug::println!("ACPI HPET unavailable.");
    }
}

pub fn hpet_address() -> Option<u64> {
    let address = ACPI_STATE.lock().hpet_address;
    (address != 0).then_some(address)
}

pub fn pci_config_address(
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
    offset: usize,
) -> Option<u64> {
    if device >= 32 || function >= 8 || offset >= 4096 {
        return None;
    }

    let state = ACPI_STATE.lock();
    state.regions[..state.region_count]
        .iter()
        .copied()
        .find(|region: &PciConfigRegion| region.contains(segment, bus))
        .and_then(|region| region.config_address(bus, device, function, offset))
}

pub fn for_each_pci_bus_region(mut visit: impl FnMut(u16, u8, u8) -> bool) {
    let (region_count, regions) = {
        let state = ACPI_STATE.lock();
        (state.region_count, state.regions)
    };

    if region_count == 0 {
        let _ = visit(0, 0, u8::MAX);
        return;
    }

    for region in regions[..region_count].iter().copied() {
        if visit(region.segment, region.start_bus, region.end_bus) {
            return;
        }
    }
}

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> Option<&'static BootInfo> {
    unsafe { BootInfo::from_ptr(boot_info_ptr) }.ok()
}

fn load_mcfg_regions(rsdp_addr: u64, state: &mut AcpiState) -> bool {
    let Some((root_addr, entry_size)) = root_sdt_from_rsdp(rsdp_addr) else {
        return false;
    };
    let Some(root_table) = sdt_bytes(root_addr) else {
        return false;
    };

    let Some(entries) = root_sdt_entries(root_table, entry_size) else {
        return false;
    };
    let mut index = 0;
    let mut loaded = false;
    while index + entry_size <= entries.len() {
        let table_addr = if entry_size == 8 {
            le_u64(&entries[index..index + 8])
        } else {
            le_u32(&entries[index..index + 4]) as u64
        };
        if table_addr == 0 {
            return false;
        }

        if let Some(table) = sdt_bytes(table_addr)
            && &table[..4] == b"MCFG"
        {
            loaded |= parse_mcfg_table(table, state);
            break;
        }

        index += entry_size;
    }

    loaded
}

fn load_hpet_address(rsdp_addr: u64) -> Option<u64> {
    let (root_addr, entry_size) = root_sdt_from_rsdp(rsdp_addr)?;
    let root_table = sdt_bytes(root_addr)?;
    let entries = root_sdt_entries(root_table, entry_size)?;
    let mut index = 0;
    while index + entry_size <= entries.len() {
        let table_addr = if entry_size == 8 {
            le_u64(&entries[index..index + 8])
        } else {
            le_u32(&entries[index..index + 4]) as u64
        };
        if table_addr == 0 {
            return None;
        }
        if let Some(table) = sdt_bytes(table_addr)
            && &table[..4] == b"HPET"
        {
            return parse_hpet_table(table);
        }
        index += entry_size;
    }
    None
}

fn parse_hpet_table(table: &[u8]) -> Option<u64> {
    // ACPI header (36) + Event Timer Block ID (4) precede the GAS.
    const GAS_OFFSET: usize = 40;
    if table.len() < HPET_TABLE_LEN || table[GAS_OFFSET] != ACPI_ADDRESS_SPACE_SYSTEM_MEMORY {
        return None;
    }
    // Some firmware leaves GAS.RegisterBitWidth as zero/unspecified. Accept
    // that encoding and let the HPET capability register provide the
    // authoritative 64-bit check; reject only an explicit sub-64-bit width.
    if table[GAS_OFFSET + 1] != 0 && table[GAS_OFFSET + 1] < 64 {
        return None;
    }
    if table[GAS_OFFSET + 2] != 0 || !matches!(table[GAS_OFFSET + 3], 0 | ACPI_GAS_ACCESS_QWORD) {
        return None;
    }
    let address = le_u64(&table[GAS_OFFSET + 4..GAS_OFFSET + 12]);
    let end = address.checked_add(1024)?;
    (address != 0 && address.is_multiple_of(1024) && end <= IDENTITY_MAPPED_PHYS_LIMIT)
        .then_some(address)
}

fn root_sdt_from_rsdp(rsdp_addr: u64) -> Option<(u64, usize)> {
    let rsdp_v1 = phys_bytes(rsdp_addr, RSDP_V1_LEN)?;
    if &rsdp_v1[..8] != b"RSD PTR " || !checksum_ok(rsdp_v1) {
        return None;
    }

    let revision = rsdp_v1[15];
    let rsdt_addr = le_u32(&rsdp_v1[16..20]) as u64;
    if revision < 2 {
        return (rsdt_addr != 0).then_some((rsdt_addr, 4));
    }

    let rsdp_v2 = phys_bytes(rsdp_addr, RSDP_V2_LEN)?;
    let length = le_u32(&rsdp_v2[20..24]) as usize;
    if !(RSDP_V2_LEN..=MAX_RSDP_BYTES).contains(&length) {
        return None;
    }
    let rsdp_full = phys_bytes(rsdp_addr, length)?;
    if !checksum_ok(rsdp_full) {
        return None;
    }

    let xsdt_addr = le_u64(&rsdp_full[24..32]);
    if xsdt_addr != 0 {
        Some((xsdt_addr, 8))
    } else if rsdt_addr != 0 {
        Some((rsdt_addr, 4))
    } else {
        None
    }
}

fn sdt_bytes(addr: u64) -> Option<&'static [u8]> {
    let header = phys_bytes(addr, SDT_HEADER_LEN)?;
    let length = le_u32(&header[4..8]) as usize;
    if !(SDT_HEADER_LEN..=MAX_ACPI_SDT_BYTES).contains(&length) {
        return None;
    }

    let table = phys_bytes(addr, length)?;
    checksum_ok(table).then_some(table)
}

fn root_sdt_entries(table: &[u8], entry_size: usize) -> Option<&[u8]> {
    let signature = match entry_size {
        4 => b"RSDT",
        8 => b"XSDT",
        _ => return None,
    };
    if table.len() < SDT_HEADER_LEN
        || &table[..4] != signature
        || !(table.len() - SDT_HEADER_LEN).is_multiple_of(entry_size)
    {
        return None;
    }
    Some(&table[SDT_HEADER_LEN..])
}

fn parse_mcfg_table(table: &[u8], state: &mut AcpiState) -> bool {
    if table.len() < MCFG_HEADER_LEN
        || !(table.len() - MCFG_HEADER_LEN).is_multiple_of(MCFG_ENTRY_LEN)
    {
        return false;
    }

    let entry_count = (table.len() - MCFG_HEADER_LEN) / MCFG_ENTRY_LEN;
    if entry_count == 0 || entry_count > state.regions.len().saturating_sub(state.region_count) {
        return false;
    }

    let mut staged = *state;
    let mut index = MCFG_HEADER_LEN;
    while index < table.len() {
        let base_address = le_u64(&table[index..index + 8]);
        let segment = le_u16(&table[index + 8..index + 10]);
        let start_bus = table[index + 10];
        let end_bus = table[index + 11];
        let Some(region) = validated_mcfg_region(base_address, segment, start_bus, end_bus) else {
            return false;
        };
        if !staged.push_region(region) {
            return false;
        }
        index += MCFG_ENTRY_LEN;
    }

    *state = staged;
    true
}

fn validated_mcfg_region(
    base_address: u64,
    segment: u16,
    start_bus: u8,
    end_bus: u8,
) -> Option<PciConfigRegion> {
    if base_address == 0 || !base_address.is_multiple_of(PCI_ECAM_BUS_BYTES) || start_bus > end_bus
    {
        return None;
    }
    let bus_count = u64::from(end_bus - start_bus) + 1;
    let region_bytes = bus_count.checked_mul(PCI_ECAM_BUS_BYTES)?;
    let end = base_address.checked_add(region_bytes)?;
    if end > IDENTITY_MAPPED_PHYS_LIMIT {
        return None;
    }
    Some(PciConfigRegion {
        base_address,
        segment,
        start_bus,
        end_bus,
    })
}

fn phys_bytes(addr: u64, len: usize) -> Option<&'static [u8]> {
    if addr == 0 || len == 0 {
        return None;
    }

    let len = u64::try_from(len).ok()?;
    let end = addr.checked_add(len)?;
    if end > IDENTITY_MAPPED_PHYS_LIMIT {
        return None;
    }

    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) })
}

fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        ACPI_GAS_ACCESS_QWORD, AcpiState, HPET_TABLE_LEN, MCFG_ENTRY_LEN, MCFG_HEADER_LEN,
        PCI_ECAM_BUS_BYTES, PciConfigRegion, parse_hpet_table, parse_mcfg_table, root_sdt_entries,
        validated_mcfg_region,
    };

    fn one_mcfg_entry(
        base_address: u64,
        segment: u16,
        start_bus: u8,
        end_bus: u8,
    ) -> [u8; MCFG_HEADER_LEN + MCFG_ENTRY_LEN] {
        let mut table = [0_u8; MCFG_HEADER_LEN + MCFG_ENTRY_LEN];
        table[MCFG_HEADER_LEN..MCFG_HEADER_LEN + 8].copy_from_slice(&base_address.to_le_bytes());
        table[MCFG_HEADER_LEN + 8..MCFG_HEADER_LEN + 10].copy_from_slice(&segment.to_le_bytes());
        table[MCFG_HEADER_LEN + 10] = start_bus;
        table[MCFG_HEADER_LEN + 11] = end_bus;
        table
    }

    #[test]
    fn root_sdt_requires_exact_signature_width_and_entry_alignment() {
        let mut xsdt = [0_u8; 44];
        xsdt[..4].copy_from_slice(b"XSDT");
        assert_eq!(root_sdt_entries(&xsdt, 8).map(<[u8]>::len), Some(8));
        assert!(root_sdt_entries(&xsdt, 4).is_none());
        assert!(root_sdt_entries(&xsdt[..43], 8).is_none());
        assert!(root_sdt_entries(&xsdt, 7).is_none());
    }

    #[test]
    fn mcfg_admission_is_atomic_bounded_aligned_and_nonoverlapping() {
        let mut state = AcpiState::new();
        let valid = one_mcfg_entry(0x8000_0000, 0, 0, 31);
        assert!(parse_mcfg_table(&valid, &mut state));
        let admitted = state;

        let overlap = one_mcfg_entry(0x9000_0000, 0, 16, 63);
        assert!(!parse_mcfg_table(&overlap, &mut state));
        assert_eq!(state, admitted);

        let unaligned = one_mcfg_entry(0x8000_1000, 1, 0, 0);
        assert!(!parse_mcfg_table(&unaligned, &mut state));
        assert_eq!(state, admitted);

        let mut trailing = [0_u8; MCFG_HEADER_LEN + MCFG_ENTRY_LEN + 1];
        trailing[..valid.len()].copy_from_slice(&valid);
        assert!(!parse_mcfg_table(&trailing, &mut AcpiState::new()));
    }

    #[test]
    fn ecam_region_range_and_config_address_are_checked_end_to_end() {
        let region =
            validated_mcfg_region(0x8000_0000, 0, 4, 5).expect("bounded ECAM region admitted");
        assert_eq!(
            region.config_address(5, 31, 7, 4095),
            Some(0x8000_0000 + PCI_ECAM_BUS_BYTES + (31 << 15) + (7 << 12) + 4095)
        );
        assert!(region.config_address(6, 0, 0, 0).is_none());
        assert!(region.config_address(5, 32, 0, 0).is_none());
        assert!(region.config_address(5, 0, 8, 0).is_none());
        assert!(region.config_address(5, 0, 0, 4096).is_none());

        assert!(validated_mcfg_region(1, 0, 0, 0).is_none());
        assert!(validated_mcfg_region(PCI_ECAM_BUS_BYTES, 0, 2, 1).is_none());
        assert!(
            PciConfigRegion::empty()
                .config_address(0, 0, 0, 0)
                .is_none()
        );
    }

    #[test]
    fn hpet_gas_requires_memory_qword_zero_offset_and_aligned_range() {
        let mut table = [0_u8; HPET_TABLE_LEN];
        table[40] = 0;
        table[41] = 64;
        table[42] = 0;
        table[43] = ACPI_GAS_ACCESS_QWORD;
        table[44..52].copy_from_slice(&0xfed0_0000_u64.to_le_bytes());
        assert_eq!(parse_hpet_table(&table), Some(0xfed0_0000));

        table[42] = 1;
        assert!(parse_hpet_table(&table).is_none());
        table[42] = 0;
        table[43] = 3;
        assert!(parse_hpet_table(&table).is_none());
        table[43] = ACPI_GAS_ACCESS_QWORD;
        table[44..52].copy_from_slice(&0xfed0_0001_u64.to_le_bytes());
        assert!(parse_hpet_table(&table).is_none());
    }
}
