use boot_protocol::BootInfo;
use spin::Mutex;

const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;
const SDT_HEADER_LEN: usize = 36;
const MCFG_HEADER_LEN: usize = 44;
const MCFG_ENTRY_LEN: usize = 16;
const HPET_TABLE_LEN: usize = 56;
const ACPI_ADDRESS_SPACE_SYSTEM_MEMORY: u8 = 0;
const MAX_MCFG_REGIONS: usize = 8;
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
}

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

    fn push_region(&mut self, region: PciConfigRegion) {
        if self.region_count >= self.regions.len() {
            return;
        }

        self.regions[self.region_count] = region;
        self.region_count += 1;
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
        .map(|region| {
            region.base_address
                + (((bus - region.start_bus) as u64) << 20)
                + ((device as u64) << 15)
                + ((function as u64) << 12)
                + offset as u64
        })
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

    let entries = &root_table[SDT_HEADER_LEN..];
    let mut index = 0;
    let mut loaded = false;
    while index + entry_size <= entries.len() {
        let table_addr = if entry_size == 8 {
            le_u64(&entries[index..index + 8])
        } else {
            le_u32(&entries[index..index + 4]) as u64
        };

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
    let entries = &root_table[SDT_HEADER_LEN..];
    let mut index = 0;
    while index + entry_size <= entries.len() {
        let table_addr = if entry_size == 8 {
            le_u64(&entries[index..index + 8])
        } else {
            le_u32(&entries[index..index + 4]) as u64
        };
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
    let address = le_u64(&table[GAS_OFFSET + 4..GAS_OFFSET + 12]);
    let end = address.checked_add(1024)?;
    (address != 0 && end <= IDENTITY_MAPPED_PHYS_LIMIT).then_some(address)
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
    if length < RSDP_V2_LEN {
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
    if length < SDT_HEADER_LEN {
        return None;
    }

    let table = phys_bytes(addr, length)?;
    checksum_ok(table).then_some(table)
}

fn parse_mcfg_table(table: &[u8], state: &mut AcpiState) -> bool {
    if table.len() < MCFG_HEADER_LEN {
        return false;
    }

    let mut index = MCFG_HEADER_LEN;
    while index + MCFG_ENTRY_LEN <= table.len() {
        let base_address = le_u64(&table[index..index + 8]);
        let segment = le_u16(&table[index + 8..index + 10]);
        let start_bus = table[index + 10];
        let end_bus = table[index + 11];

        if base_address != 0 && start_bus <= end_bus {
            state.push_region(PciConfigRegion {
                base_address,
                segment,
                start_bus,
                end_bus,
            });
        }

        index += MCFG_ENTRY_LEN;
    }

    state.region_count != 0
}

fn phys_bytes(addr: u64, len: usize) -> Option<&'static [u8]> {
    if addr == 0 || len == 0 {
        return None;
    }

    let end = addr.checked_add(len as u64)?;
    if end > IDENTITY_MAPPED_PHYS_LIMIT {
        return None;
    }

    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len) })
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
