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
//! - **Forbidden:** No unchecked firmware pointer, partial MCFG/MADT
//!   publication, raw-APIC-ID array indexing, or fabricated timer topology.
//! - **Evidence:** `acpi-firmware-admission`, `cpu-topology-admission`, and
//!   `monotonic-deadline-lifecycle`.
use boot_protocol::BootInfo;
use spin::Mutex;

const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;
const SDT_HEADER_LEN: usize = 36;
const MCFG_HEADER_LEN: usize = 44;
const MCFG_ENTRY_LEN: usize = 16;
const HPET_TABLE_LEN: usize = 56;
const MADT_HEADER_LEN: usize = 44;
const MADT_LOCAL_APIC: u8 = 0;
const MADT_LOCAL_APIC_ADDRESS_OVERRIDE: u8 = 5;
const MADT_LOCAL_X2APIC: u8 = 9;
const MADT_PROCESSOR_ENABLED: u32 = 1;
const MADT_PROCESSOR_ONLINE_CAPABLE: u32 = 2;
const ACPI_ADDRESS_SPACE_SYSTEM_MEMORY: u8 = 0;
const ACPI_GAS_ACCESS_QWORD: u8 = 4;
const MAX_MCFG_REGIONS: usize = 8;
const MAX_RSDP_BYTES: usize = 4096;
const MAX_ACPI_SDT_BYTES: usize = 1024 * 1024;
pub const MAX_SUPPORTED_CPUS: usize = 8;
const PCI_ECAM_BUS_BYTES: u64 = 1 << 20;
const IDENTITY_MAPPED_PHYS_LIMIT: u64 = 512 * 1024 * 1024 * 1024;

static ACPI_STATE: Mutex<AcpiState> = Mutex::new(AcpiState::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuDescriptor {
    pub logical_index: u8,
    pub firmware_uid: u32,
    pub apic_id: u32,
    pub uses_x2apic_id: bool,
}

impl CpuDescriptor {
    const fn empty() -> Self {
        Self {
            logical_index: u8::MAX,
            firmware_uid: u32::MAX,
            apic_id: u32::MAX,
            uses_x2apic_id: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuTopology {
    local_apic_address: u64,
    cpu_count: usize,
    cpus: [CpuDescriptor; MAX_SUPPORTED_CPUS],
}

impl CpuTopology {
    const fn empty() -> Self {
        Self {
            local_apic_address: 0,
            cpu_count: 0,
            cpus: [CpuDescriptor::empty(); MAX_SUPPORTED_CPUS],
        }
    }

    pub fn local_apic_address(self) -> u64 {
        self.local_apic_address
    }

    pub fn cpu_count(self) -> usize {
        self.cpu_count
    }

    pub fn cpus(&self) -> &[CpuDescriptor] {
        &self.cpus[..self.cpu_count]
    }

    fn normalize_bsp_first(mut self, bsp_apic_id: u32) -> Option<Self> {
        let bsp_position = self.cpus[..self.cpu_count]
            .iter()
            .position(|cpu| cpu.apic_id == bsp_apic_id)?;
        self.cpus.swap(0, bsp_position);
        for (logical_index, cpu) in self.cpus[..self.cpu_count].iter_mut().enumerate() {
            cpu.logical_index =
                u8::try_from(logical_index).expect("admitted CPU count exceeds logical index");
        }
        Some(self)
    }

    fn push_cpu(&mut self, firmware_uid: u32, apic_id: u32, uses_x2apic_id: bool) -> bool {
        if self.cpu_count >= self.cpus.len()
            || self.cpus[..self.cpu_count]
                .iter()
                .any(|cpu| cpu.firmware_uid == firmware_uid || cpu.apic_id == apic_id)
        {
            return false;
        }
        let Ok(logical_index) = u8::try_from(self.cpu_count) else {
            return false;
        };
        self.cpus[self.cpu_count] = CpuDescriptor {
            logical_index,
            firmware_uid,
            apic_id,
            uses_x2apic_id,
        };
        self.cpu_count += 1;
        true
    }
}

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
    cpu_topology: CpuTopology,
    region_count: usize,
    regions: [PciConfigRegion; MAX_MCFG_REGIONS],
}

impl AcpiState {
    const fn new() -> Self {
        Self {
            rsdp_addr: 0,
            hpet_address: 0,
            cpu_topology: CpuTopology::empty(),
            region_count: 0,
            regions: [PciConfigRegion::empty(); MAX_MCFG_REGIONS],
        }
    }

    fn reset(&mut self, rsdp_addr: u64) {
        self.rsdp_addr = rsdp_addr;
        self.hpet_address = 0;
        self.cpu_topology = CpuTopology::empty();
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
    if let Some(topology) = load_cpu_topology(rsdp_addr).and_then(|topology| {
        topology.normalize_bsp_first(nucleus_core::util::lockdep::hardware_apic_id())
    }) {
        state.cpu_topology = topology;
        crate::debug::println!(
            "ACPI MADT admitted: {} logical CPU(s), local APIC at {:#x}.",
            topology.cpu_count(),
            topology.local_apic_address(),
        );
    } else {
        crate::debug::println!("ACPI MADT CPU topology unavailable or unsupported.");
    }
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

pub fn cpu_topology() -> Option<CpuTopology> {
    let topology = ACPI_STATE.lock().cpu_topology;
    (topology.cpu_count != 0).then_some(topology)
}

#[cfg(test)]
pub(super) fn test_topology(cpus: &[(u32, u32, bool)]) -> CpuTopology {
    let mut topology = CpuTopology::empty();
    topology.local_apic_address = 0xfee0_0000;
    for &(firmware_uid, apic_id, uses_x2apic_id) in cpus {
        assert!(topology.push_cpu(firmware_uid, apic_id, uses_x2apic_id));
    }
    topology
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

fn load_cpu_topology(rsdp_addr: u64) -> Option<CpuTopology> {
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
            && &table[..4] == b"APIC"
        {
            return parse_madt_table(table);
        }
        index += entry_size;
    }
    None
}

fn parse_madt_table(table: &[u8]) -> Option<CpuTopology> {
    if table.len() < MADT_HEADER_LEN || &table[..4] != b"APIC" {
        return None;
    }
    let mut topology = CpuTopology::empty();
    topology.local_apic_address = u64::from(le_u32(&table[36..40]));
    if le_u32(&table[40..44]) & !1 != 0 {
        return None;
    }

    let mut address_overridden = false;
    let mut index = MADT_HEADER_LEN;
    while index < table.len() {
        let header_end = index.checked_add(2)?;
        if header_end > table.len() {
            return None;
        }
        let entry_type = table[index];
        let entry_len = usize::from(table[index + 1]);
        if entry_len < 2 {
            return None;
        }
        let entry_end = index.checked_add(entry_len)?;
        if entry_end > table.len() {
            return None;
        }
        let entry = &table[index..entry_end];
        match entry_type {
            MADT_LOCAL_APIC => {
                if entry_len != 8 {
                    return None;
                }
                let flags = le_u32(&entry[4..8]);
                if flags & !(MADT_PROCESSOR_ENABLED | MADT_PROCESSOR_ONLINE_CAPABLE) != 0 {
                    return None;
                }
                if flags & MADT_PROCESSOR_ONLINE_CAPABLE != 0 && flags & MADT_PROCESSOR_ENABLED == 0
                {
                    // Fixed-CPU commercial topology does not admit hot-add-only CPUs.
                    return None;
                }
                if flags & MADT_PROCESSOR_ENABLED != 0
                    && !topology.push_cpu(u32::from(entry[2]), u32::from(entry[3]), false)
                {
                    return None;
                }
            }
            MADT_LOCAL_X2APIC => {
                if entry_len != 16 || le_u16(&entry[2..4]) != 0 {
                    return None;
                }
                let flags = le_u32(&entry[8..12]);
                if flags & !(MADT_PROCESSOR_ENABLED | MADT_PROCESSOR_ONLINE_CAPABLE) != 0 {
                    return None;
                }
                if flags & MADT_PROCESSOR_ONLINE_CAPABLE != 0 && flags & MADT_PROCESSOR_ENABLED == 0
                {
                    return None;
                }
                if flags & MADT_PROCESSOR_ENABLED != 0
                    && !topology.push_cpu(le_u32(&entry[12..16]), le_u32(&entry[4..8]), true)
                {
                    return None;
                }
            }
            MADT_LOCAL_APIC_ADDRESS_OVERRIDE => {
                if entry_len != 12 || le_u16(&entry[2..4]) != 0 || address_overridden {
                    return None;
                }
                topology.local_apic_address = le_u64(&entry[4..12]);
                address_overridden = true;
            }
            _ => {}
        }
        index = entry_end;
    }

    let apic_end = topology.local_apic_address.checked_add(4096)?;
    (topology.cpu_count != 0
        && topology.local_apic_address != 0
        && topology.local_apic_address.is_multiple_of(4096)
        && apic_end <= IDENTITY_MAPPED_PHYS_LIMIT)
        .then_some(topology)
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
    use alloc::{vec, vec::Vec};

    use super::{
        ACPI_GAS_ACCESS_QWORD, AcpiState, HPET_TABLE_LEN, MADT_HEADER_LEN, MAX_SUPPORTED_CPUS,
        MCFG_ENTRY_LEN, MCFG_HEADER_LEN, PCI_ECAM_BUS_BYTES, PciConfigRegion, parse_hpet_table,
        parse_madt_table, parse_mcfg_table, root_sdt_entries, validated_mcfg_region,
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

    fn madt_header(extra_bytes: usize) -> Vec<u8> {
        let mut table = vec![0_u8; MADT_HEADER_LEN + extra_bytes];
        let table_len = table.len() as u32;
        table[..4].copy_from_slice(b"APIC");
        table[4..8].copy_from_slice(&table_len.to_le_bytes());
        table[36..40].copy_from_slice(&0xfee0_0000_u32.to_le_bytes());
        table[40..44].copy_from_slice(&1_u32.to_le_bytes());
        table
    }

    #[test]
    fn madt_cpu_topology_is_dense_unique_bounded_and_atomic() {
        let mut table = madt_header(24);
        table[44..52].copy_from_slice(&[0, 8, 7, 3, 1, 0, 0, 0]);
        table[52] = 9;
        table[53] = 16;
        table[56..60].copy_from_slice(&0x1234_u32.to_le_bytes());
        table[60..64].copy_from_slice(&1_u32.to_le_bytes());
        table[64..68].copy_from_slice(&42_u32.to_le_bytes());

        let topology = parse_madt_table(&table).expect("valid mixed MADT topology");
        assert_eq!(topology.local_apic_address(), 0xfee0_0000);
        assert_eq!(topology.cpu_count(), 2);
        assert_eq!(topology.cpus()[0].logical_index, 0);
        assert_eq!(topology.cpus()[0].firmware_uid, 7);
        assert_eq!(topology.cpus()[0].apic_id, 3);
        assert!(!topology.cpus()[0].uses_x2apic_id);
        assert_eq!(topology.cpus()[1].logical_index, 1);
        assert_eq!(topology.cpus()[1].firmware_uid, 42);
        assert_eq!(topology.cpus()[1].apic_id, 0x1234);
        assert!(topology.cpus()[1].uses_x2apic_id);

        let mut duplicate = table;
        duplicate[64..68].copy_from_slice(&7_u32.to_le_bytes());
        assert!(parse_madt_table(&duplicate).is_none());

        let mut too_many = madt_header((MAX_SUPPORTED_CPUS + 1) * 8);
        for cpu in 0..=MAX_SUPPORTED_CPUS {
            let offset = MADT_HEADER_LEN + cpu * 8;
            too_many[offset..offset + 8].copy_from_slice(&[0, 8, cpu as u8, cpu as u8, 1, 0, 0, 0]);
        }
        assert!(parse_madt_table(&too_many).is_none());
    }

    #[test]
    fn madt_normalizes_the_executing_bsp_to_logical_cpu_zero() {
        let mut table = madt_header(24);
        table[44..52].copy_from_slice(&[0, 8, 7, 3, 1, 0, 0, 0]);
        table[52] = 9;
        table[53] = 16;
        table[56..60].copy_from_slice(&0x1234_u32.to_le_bytes());
        table[60..64].copy_from_slice(&1_u32.to_le_bytes());
        table[64..68].copy_from_slice(&42_u32.to_le_bytes());

        let topology = parse_madt_table(&table)
            .and_then(|topology| topology.normalize_bsp_first(0x1234))
            .expect("BSP must be present in the admitted topology");
        assert_eq!(topology.cpus()[0].apic_id, 0x1234);
        assert_eq!(topology.cpus()[0].logical_index, 0);
        assert_eq!(topology.cpus()[1].apic_id, 3);
        assert_eq!(topology.cpus()[1].logical_index, 1);
        assert!(
            parse_madt_table(&table)
                .and_then(|topology| topology.normalize_bsp_first(0x99))
                .is_none()
        );
    }

    #[test]
    fn madt_rejects_truncation_hot_add_only_and_bad_apic_override() {
        let mut truncated = madt_header(3);
        truncated[44..47].copy_from_slice(&[0, 8, 0]);
        assert!(parse_madt_table(&truncated).is_none());

        let mut hot_add_only = madt_header(8);
        hot_add_only[44..52].copy_from_slice(&[0, 8, 0, 0, 2, 0, 0, 0]);
        assert!(parse_madt_table(&hot_add_only).is_none());

        let mut bad_override = madt_header(20);
        bad_override[44..52].copy_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
        bad_override[52] = 5;
        bad_override[53] = 12;
        bad_override[56..64].copy_from_slice(&0xfee0_0001_u64.to_le_bytes());
        assert!(parse_madt_table(&bad_override).is_none());
    }
}
