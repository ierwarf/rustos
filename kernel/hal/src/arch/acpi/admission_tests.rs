//! Exhaustive small-domain equivalence tests for ACPI firmware admission.
//!
//! These replace the retired `acpi-table-admission` and
//! `cpu-topology-admission` TLA+ models. Both were straight-line decision
//! machines over free Boolean flags ("lengths valid", "identities unique"),
//! so TLC could only check the models' own predicates. Here the same
//! predicates are stated once, independently, from the firmware formats and
//! the models' invariants, and the production parsers must agree with them
//! on every table in a bounded domain, in both directions: a table that
//! satisfies the predicate must be admitted and a table that violates it must
//! not be. Atomicity (`PublicationIsAtomic`, `EcamRequiresAtomicAdmission`) is
//! checked on the parser's state: a rejected table leaves it byte-identical.
//!
//! The oracle uses `i128`/`u128` arithmetic so it cannot share a wrap bug with
//! the `u64` production code.

use alloc::{vec, vec::Vec};

use super::{
    ACPI_GAS_ACCESS_QWORD, AcpiState, HPET_TABLE_LEN, IDENTITY_MAPPED_PHYS_LIMIT,
    MADT_HEADER_LEN, MAX_MCFG_REGIONS, MAX_SUPPORTED_CPUS, MCFG_ENTRY_LEN, MCFG_HEADER_LEN,
    PCI_ECAM_BUS_BYTES, PciConfigRegion, SDT_HEADER_LEN, checksum_ok, parse_hpet_table,
    parse_madt_table, parse_mcfg_table, root_sdt_entries,
};

const LIMIT: u128 = IDENTITY_MAPPED_PHYS_LIMIT as u128;
const ECAM_BUS: u128 = PCI_ECAM_BUS_BYTES as u128;

// ---------------------------------------------------------------------------
// Root SDT.
// ---------------------------------------------------------------------------

#[test]
fn root_sdt_admission_agrees_with_signature_width_and_entry_alignment() {
    let signatures: [&[u8; 4]; 4] = [b"RSDT", b"XSDT", b"FACP", b"RSD\0"];
    let mut admitted = 0usize;
    for entry_size in [0usize, 1, 4, 7, 8, 16] {
        for signature in signatures {
            for length in 0..=(SDT_HEADER_LEN + 3 * 8 + 1) {
                let mut table = vec![0_u8; length];
                if length >= 4 {
                    table[..4].copy_from_slice(signature);
                }
                let expected = match entry_size {
                    4 => signature == b"RSDT",
                    8 => signature == b"XSDT",
                    _ => false,
                } && length >= SDT_HEADER_LEN
                    && (length - SDT_HEADER_LEN) % entry_size == 0;
                let actual = root_sdt_entries(&table, entry_size);
                assert_eq!(
                    actual.is_some(),
                    expected,
                    "entry_size={entry_size} signature={signature:?} length={length}"
                );
                if let Some(entries) = actual {
                    assert_eq!(entries.len(), length - SDT_HEADER_LEN);
                    admitted += 1;
                }
            }
        }
    }
    assert!(admitted > 10);
}

#[test]
fn acpi_checksum_is_the_byte_sum_modulo_256() {
    let values = [0_u8, 1, 0x7f, 0x80, 0xff];
    for a in values {
        for b in values {
            for c in values {
                for d in values {
                    let bytes = [a, b, c, d];
                    let sum = bytes.iter().map(|byte| u32::from(*byte)).sum::<u32>();
                    assert_eq!(checksum_ok(&bytes), sum % 256 == 0, "{bytes:?}");
                }
            }
        }
    }
    assert!(checksum_ok(&[]), "an empty table sums to zero; lengths are bounded elsewhere");
}

// ---------------------------------------------------------------------------
// MCFG / ECAM.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Entry {
    base: u64,
    segment: u16,
    start_bus: u8,
    end_bus: u8,
}

fn region_is_valid(entry: Entry) -> bool {
    let bus_count = u128::from(entry.end_bus.saturating_sub(entry.start_bus)) + 1;
    entry.base != 0
        && u128::from(entry.base) % ECAM_BUS == 0
        && entry.start_bus <= entry.end_bus
        && u128::from(entry.base) + bus_count * ECAM_BUS <= LIMIT
}

fn overlaps(left: Entry, right: Entry) -> bool {
    left.segment == right.segment
        && left.start_bus <= right.end_bus
        && right.start_bus <= left.end_bus
}

fn mcfg_table(entries: &[Entry], trailing: usize) -> Vec<u8> {
    let mut table = vec![0_u8; MCFG_HEADER_LEN + entries.len() * MCFG_ENTRY_LEN + trailing];
    for (index, entry) in entries.iter().enumerate() {
        let at = MCFG_HEADER_LEN + index * MCFG_ENTRY_LEN;
        table[at..at + 8].copy_from_slice(&entry.base.to_le_bytes());
        table[at + 8..at + 10].copy_from_slice(&entry.segment.to_le_bytes());
        table[at + 10] = entry.start_bus;
        table[at + 11] = entry.end_bus;
    }
    table
}

fn published(state: &AcpiState) -> Vec<PciConfigRegion> {
    state.regions[..state.region_count].to_vec()
}

#[test]
fn mcfg_admission_agrees_with_the_ecam_predicate_and_is_atomic() {
    let bases = [
        0_u64,
        0x8000_0000,
        0x8000_1000, // not bus-aligned
        0x9000_0000,
        IDENTITY_MAPPED_PHYS_LIMIT - 0x10_0000,
        IDENTITY_MAPPED_PHYS_LIMIT,
        u64::MAX - 0xf_ffff,
    ];
    let buses = [(0_u8, 0_u8), (0, 31), (16, 63), (5, 4), (0, 255)];
    let mut candidates = Vec::new();
    for base in bases {
        for segment in [0_u16, 1] {
            for (start_bus, end_bus) in buses {
                candidates.push(Entry { base, segment, start_bus, end_bus });
            }
        }
    }
    let preexisting = [
        None,
        Some(Entry { base: 0xa000_0000, segment: 0, start_bus: 0, end_bus: 15 }),
    ];
    let (mut admitted, mut rejected) = (0usize, 0usize);
    let mut check = |entries: &[Entry], trailing: usize| {
        for before in preexisting {
            let mut state = AcpiState::new();
            if let Some(entry) = before {
                let table = mcfg_table(&[entry], 0);
                assert!(parse_mcfg_table(&table, &mut state));
            }
            let snapshot = state;
            let table = mcfg_table(entries, trailing);

            let existing: Vec<Entry> = before.into_iter().collect();
            let fits = entries.len() >= 1 && entries.len() <= MAX_MCFG_REGIONS - existing.len();
            let mut expected = trailing == 0 && fits && entries.iter().all(|e| region_is_valid(*e));
            if expected {
                for (i, left) in entries.iter().enumerate() {
                    if existing.iter().any(|e| overlaps(*left, *e))
                        || entries[i + 1..].iter().any(|right| overlaps(*left, *right))
                    {
                        expected = false;
                    }
                }
            }
            let actual = parse_mcfg_table(&table, &mut state);
            assert_eq!(
                actual, expected,
                "entries={entries:x?} trailing={trailing} before={before:x?}"
            );
            if actual {
                admitted += 1;
                let regions = published(&state);
                assert_eq!(regions.len(), existing.len() + entries.len());
                for (region, entry) in regions[existing.len()..].iter().zip(entries) {
                    assert_eq!(
                        *region,
                        PciConfigRegion {
                            base_address: entry.base,
                            segment: entry.segment,
                            start_bus: entry.start_bus,
                            end_bus: entry.end_bus,
                        }
                    );
                }
            } else {
                rejected += 1;
                assert_eq!(state, snapshot, "a rejected MCFG must publish nothing");
            }
        }
    };
    check(&[], 0);
    check(&[], 3);
    for one in &candidates {
        check(&[*one], 0);
        check(&[*one], 1);
        check(&[*one], 15);
    }
    for left in &candidates {
        for right in &candidates {
            check(&[*left, *right], 0);
        }
    }
    // Three entries over a reduced alphabet: the atomic-rollback case where
    // only the last entry is bad.
    let reduced = [
        Entry { base: 0x8000_0000, segment: 0, start_bus: 0, end_bus: 15 },
        Entry { base: 0x9000_0000, segment: 0, start_bus: 16, end_bus: 31 },
        Entry { base: 0x9000_0000, segment: 0, start_bus: 8, end_bus: 20 },
        Entry { base: 0xa000_0000, segment: 1, start_bus: 0, end_bus: 255 },
        Entry { base: 0x8000_1000, segment: 1, start_bus: 0, end_bus: 0 },
    ];
    for a in reduced {
        for b in reduced {
            for c in reduced {
                check(&[a, b, c], 0);
            }
        }
    }
    assert!(admitted > 100, "the domain must admit nontrivial tables: {admitted}");
    assert!(rejected > 1_000, "the domain must refuse nontrivial tables: {rejected}");
}

#[test]
fn mcfg_region_count_is_bounded_by_the_published_capacity() {
    let distinct = |count: usize| -> Vec<Entry> {
        (0..count)
            .map(|index| Entry {
                base: 0x8000_0000,
                segment: index as u16,
                start_bus: 0,
                end_bus: 0,
            })
            .collect()
    };
    let mut state = AcpiState::new();
    assert!(parse_mcfg_table(&mcfg_table(&distinct(MAX_MCFG_REGIONS), 0), &mut state));
    assert_eq!(state.region_count, MAX_MCFG_REGIONS);

    let mut state = AcpiState::new();
    let snapshot = state;
    assert!(!parse_mcfg_table(&mcfg_table(&distinct(MAX_MCFG_REGIONS + 1), 0), &mut state));
    assert_eq!(state, snapshot);

    // A full table admits nothing further, however valid.
    let mut full = AcpiState::new();
    assert!(parse_mcfg_table(&mcfg_table(&distinct(MAX_MCFG_REGIONS), 0), &mut full));
    let snapshot = full;
    let extra = Entry { base: 0xa000_0000, segment: 99, start_bus: 0, end_bus: 0 };
    assert!(!parse_mcfg_table(&mcfg_table(&[extra], 0), &mut full));
    assert_eq!(full, snapshot);
}

// ---------------------------------------------------------------------------
// HPET generic address structure.
// ---------------------------------------------------------------------------

#[test]
fn hpet_admission_agrees_with_the_exact_memory_gas_predicate() {
    let spaces = [0_u8, 1, 2, 0x7f, 0xff];
    let widths = [0_u8, 1, 32, 63, 64, 128, 255];
    let bit_offsets = [0_u8, 1, 64];
    let accesses = [0_u8, 1, 3, ACPI_GAS_ACCESS_QWORD, 5, 255];
    let addresses = [
        0_u64,
        0xfed0_0000,
        0xfed0_0400,
        0xfed0_0001,
        0xfed0_0200,
        IDENTITY_MAPPED_PHYS_LIMIT - 1024,
        IDENTITY_MAPPED_PHYS_LIMIT - 512,
        IDENTITY_MAPPED_PHYS_LIMIT,
        u64::MAX - 1023,
        u64::MAX,
    ];
    let (mut admitted, mut rejected) = (0usize, 0usize);
    for length in [HPET_TABLE_LEN - 1, HPET_TABLE_LEN, HPET_TABLE_LEN + 1] {
        for space in spaces {
            for width in widths {
                for bit_offset in bit_offsets {
                    for access in accesses {
                        for address in addresses {
                            let mut table = vec![0_u8; length];
                            if length > 40 {
                                table[40] = space;
                            }
                            if length > 43 {
                                table[41] = width;
                                table[42] = bit_offset;
                                table[43] = access;
                            }
                            if length >= 52 {
                                table[44..52].copy_from_slice(&address.to_le_bytes());
                            }
                            let expected = length >= HPET_TABLE_LEN
                                && space == 0
                                && (width == 0 || width >= 64)
                                && bit_offset == 0
                                && (access == 0 || access == ACPI_GAS_ACCESS_QWORD)
                                && address != 0
                                && u128::from(address) % 1024 == 0
                                && u128::from(address) + 1024 <= LIMIT;
                            let actual = parse_hpet_table(&table);
                            assert_eq!(
                                actual.is_some(),
                                expected,
                                "len={length} space={space} width={width} offset={bit_offset} access={access} address={address:#x}"
                            );
                            if actual.is_some() {
                                assert_eq!(actual, Some(address));
                                admitted += 1;
                            } else {
                                rejected += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(admitted > 10 && rejected > 1_000, "{admitted}/{rejected}");
}

// ---------------------------------------------------------------------------
// MADT / CPU topology.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Madt {
    Lapic { uid: u8, apic: u8, flags: u32 },
    X2apic { uid: u32, apic: u32, flags: u32 },
    Override { address: u64, reserved: u16 },
    Unknown { kind: u8, len: u8 },
    ShortLapic,
    ShortX2apic,
    LongX2apic,
    ZeroLength,
    OneLength,
}

impl Madt {
    fn bytes(self) -> Vec<u8> {
        match self {
            Madt::Lapic { uid, apic, flags } => {
                let mut b = vec![0, 8, uid, apic, 0, 0, 0, 0];
                b[4..8].copy_from_slice(&flags.to_le_bytes());
                b
            }
            Madt::X2apic { uid, apic, flags } => {
                let mut b = vec![0_u8; 16];
                b[0] = 9;
                b[1] = 16;
                b[4..8].copy_from_slice(&apic.to_le_bytes());
                b[8..12].copy_from_slice(&flags.to_le_bytes());
                b[12..16].copy_from_slice(&uid.to_le_bytes());
                b
            }
            Madt::Override { address, reserved } => {
                let mut b = vec![0_u8; 12];
                b[0] = 5;
                b[1] = 12;
                b[2..4].copy_from_slice(&reserved.to_le_bytes());
                b[4..12].copy_from_slice(&address.to_le_bytes());
                b
            }
            Madt::Unknown { kind, len } => {
                let mut b = vec![0_u8; usize::from(len)];
                b[0] = kind;
                b[1] = len;
                b
            }
            Madt::ShortLapic => vec![0, 7, 0, 0, 1, 0, 0],
            Madt::ShortX2apic => vec![9, 12, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            Madt::LongX2apic => {
                let mut b = vec![0_u8; 20];
                b[0] = 9;
                b[1] = 20;
                b[8] = 1;
                b
            }
            Madt::ZeroLength => vec![0x7f, 0],
            // Skipping a one-byte entry would land on a well-formed entry
            // (type 1, length 2), so only the length guard rejects it.
            Madt::OneLength => vec![0x7f, 1, 2],
        }
    }
}

fn madt_table(header_address: u32, entries: &[Madt]) -> Vec<u8> {
    let mut table = vec![0_u8; MADT_HEADER_LEN];
    table[..4].copy_from_slice(b"APIC");
    table[36..40].copy_from_slice(&header_address.to_le_bytes());
    table[40..44].copy_from_slice(&1_u32.to_le_bytes());
    for entry in entries {
        table.extend_from_slice(&entry.bytes());
    }
    let length = table.len() as u32;
    table[4..8].copy_from_slice(&length.to_le_bytes());
    table
}

/// Independent statement of `CpuTopologyAdmission` for tables that stay under
/// the fixed CPU capacity: the expected `(uid, apic, x2apic)` list and local
/// APIC address, or `None` when the model's guard fails.
fn expected_topology(header_address: u32, entries: &[Madt]) -> Option<(u64, Vec<(u32, u32, bool)>)> {
    let mut address = u64::from(header_address);
    let mut overridden = false;
    let mut cpus: Vec<(u32, u32, bool)> = Vec::new();
    for entry in entries {
        match *entry {
            Madt::ShortLapic
            | Madt::ShortX2apic
            | Madt::LongX2apic
            | Madt::ZeroLength
            | Madt::OneLength => return None,
            Madt::Unknown { .. } => {}
            Madt::Override { address: new, reserved } => {
                if reserved != 0 || overridden {
                    return None;
                }
                overridden = true;
                address = new;
            }
            Madt::Lapic { uid, apic, flags } => {
                if flags & 1 != 0 {
                    let (uid, apic) = (u32::from(uid), u32::from(apic));
                    if cpus.iter().any(|c| c.0 == uid || c.1 == apic) {
                        return None;
                    }
                    cpus.push((uid, apic, false));
                }
            }
            Madt::X2apic { uid, apic, flags } => {
                if flags & 1 != 0 {
                    if cpus.iter().any(|c| c.0 == uid || c.1 == apic) {
                        return None;
                    }
                    cpus.push((uid, apic, true));
                }
            }
        }
    }
    let aligned = address % 4096 == 0;
    let bounded = u128::from(address) + 4096 <= LIMIT;
    (!cpus.is_empty() && address != 0 && aligned && bounded).then_some((address, cpus))
}

#[test]
fn madt_admission_agrees_with_the_topology_predicate_and_is_atomic() {
    let mut candidates = vec![
        Madt::Unknown { kind: 0x7f, len: 2 },
        Madt::Unknown { kind: 1, len: 12 },
        Madt::ShortLapic,
        Madt::ShortX2apic,
        Madt::LongX2apic,
        Madt::ZeroLength,
        Madt::OneLength,
    ];
    for uid in [0_u8, 1] {
        for apic in [0_u8, 1] {
            for flags in [0_u32, 1, 2, 0x8000_0001] {
                candidates.push(Madt::Lapic { uid, apic, flags });
            }
        }
    }
    for uid in [0_u32, 1, 0x100] {
        for apic in [0_u32, 1, 0x100] {
            for flags in [0_u32, 1] {
                candidates.push(Madt::X2apic { uid, apic, flags });
            }
        }
    }
    for address in [0xfee0_0000_u64, 0xfee0_0001, 0, IDENTITY_MAPPED_PHYS_LIMIT] {
        for reserved in [0_u16, 1] {
            candidates.push(Madt::Override { address, reserved });
        }
    }
    let (mut admitted, mut rejected) = (0usize, 0usize);
    let mut check = |header: u32, entries: &[Madt]| {
        let table = madt_table(header, entries);
        let expected = expected_topology(header, entries);
        let actual = parse_madt_table(&table);
        assert_eq!(
            actual.is_some(),
            expected.is_some(),
            "header={header:#x} entries={entries:x?}: {actual:?}"
        );
        let (Some(topology), Some((address, cpus))) = (actual, expected) else {
            rejected += 1;
            return;
        };
        admitted += 1;
        assert_eq!(topology.local_apic_address(), address);
        assert_eq!(topology.cpu_count(), cpus.len());
        for (logical, (cpu, (uid, apic, x2apic))) in topology.cpus().iter().zip(&cpus).enumerate() {
            assert_eq!(usize::from(cpu.logical_index), logical, "dense logical indexes");
            assert_eq!((cpu.firmware_uid, cpu.apic_id, cpu.uses_x2apic_id), (*uid, *apic, *x2apic));
        }
        // BSP normalization: any admitted CPU can be moved to logical zero,
        // indexes stay dense and the set is unchanged; an absent BSP fails.
        for (position, (_, apic, _)) in cpus.iter().enumerate() {
            let normalized = topology.normalize_bsp_first(*apic).expect("admitted BSP");
            assert_eq!(normalized.cpus()[0].apic_id, *apic);
            assert_eq!(normalized.cpu_count(), cpus.len());
            for (logical, cpu) in normalized.cpus().iter().enumerate() {
                assert_eq!(usize::from(cpu.logical_index), logical);
            }
            let mut after: Vec<u32> = normalized.cpus().iter().map(|c| c.apic_id).collect();
            let mut before: Vec<u32> = cpus.iter().map(|c| c.1).collect();
            after.sort_unstable();
            before.sort_unstable();
            assert_eq!(after, before, "position {position}: same CPU set");
        }
        assert!(topology.normalize_bsp_first(0xdead_beef).is_none());
    };
    for header in [0xfee0_0000_u32, 0] {
        check(header, &[]);
        for one in &candidates {
            check(header, &[*one]);
        }
        for left in &candidates {
            for right in &candidates {
                check(header, &[*left, *right]);
            }
        }
    }
    // Three-entry sequences over a reduced alphabet reach the interactions
    // between duplicate detection, the single address override and skipped
    // entries.
    let reduced = [
        Madt::Lapic { uid: 0, apic: 0, flags: 1 },
        Madt::Lapic { uid: 1, apic: 1, flags: 1 },
        Madt::Lapic { uid: 1, apic: 0, flags: 1 },
        Madt::Lapic { uid: 2, apic: 2, flags: 2 },
        Madt::X2apic { uid: 0x100, apic: 0x100, flags: 1 },
        Madt::X2apic { uid: 5, apic: 1, flags: 1 },
        Madt::Override { address: 0xfee0_0000, reserved: 0 },
        Madt::Override { address: 0xfee0_1000, reserved: 0 },
        Madt::Unknown { kind: 0x7f, len: 2 },
    ];
    for a in reduced {
        for b in reduced {
            for c in reduced {
                check(0xfee0_0000, &[a, b, c]);
            }
        }
    }
    assert!(admitted > 100 && rejected > 1_000, "{admitted}/{rejected}");
}

#[test]
fn madt_over_capacity_keeps_the_first_cpus_in_firmware_order() {
    for extra in 0..=3_usize {
        let count = MAX_SUPPORTED_CPUS + extra;
        let entries: Vec<Madt> = (0..count)
            .map(|index| Madt::Lapic { uid: index as u8, apic: index as u8, flags: 1 })
            .collect();
        let topology = parse_madt_table(&madt_table(0xfee0_0000, &entries))
            .expect("over-capacity firmware boots with the first CPUs");
        assert_eq!(topology.cpu_count(), count.min(MAX_SUPPORTED_CPUS));
        for (logical, cpu) in topology.cpus().iter().enumerate() {
            assert_eq!(usize::from(cpu.logical_index), logical);
            assert_eq!(usize::from(cpu.apic_id as u8), logical, "firmware order is kept");
        }
    }
}
