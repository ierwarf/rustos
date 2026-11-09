//! Exhaustive small-domain equivalence tests for the executable-image gate.
//!
//! These replace the retired `dual-abi-image-admission` and
//! `dual-abi-byte-parser` TLA+ models. Those models were a three-state
//! validator machine over free Boolean flags, so TLC could only check the
//! model's own predicate. Here the same predicate is written once, directly
//! from the model's `ValidImagePlan` / `ValidBytes` definitions, as an
//! independent oracle, and the production functions are required to agree
//! with it for every input in a bounded domain that is larger than the TLC
//! configuration (three regions instead of two, zero-length and overflowing
//! extents, unknown flag bits, and a wrapped address window). Agreement is
//! required in both directions: an accepted plan must satisfy the predicate
//! and a plan that satisfies it must be accepted, so a missing guard and an
//! over-strict guard are both visible.
//!
//! The oracle uses `i128` arithmetic so that it cannot share an overflow bug
//! with the `u64` production code. The mutation registry
//! (`formal/implementation-mutations.tsv`) names the witness that kills each
//! guard removal.

use super::*;
use std::{vec, vec::Vec};

/// Independent statement of the common ELF/PE plan gate (`ValidImagePlan`).
fn plan_is_valid(
    entry: u64,
    regions: &[ImageRegion],
    window_start: u64,
    window_end: u64,
    allow_zero_entry: bool,
) -> bool {
    let (window_start, window_end) = (i128::from(window_start), i128::from(window_end));
    if window_start >= window_end || regions.is_empty() {
        return false;
    }
    let extent = |r: &ImageRegion| (i128::from(r.start), i128::from(r.start) + i128::from(r.len));
    for r in regions {
        let (start, end) = extent(r);
        let writable = r.flags & IMAGE_REGION_WRITE != 0;
        let executable = r.flags & IMAGE_REGION_EXECUTE != 0;
        if r.len == 0
            || r.flags & !IMAGE_REGION_KNOWN_FLAGS != 0
            || (writable && executable)
            || start < window_start
            || end > window_end
        {
            return false;
        }
    }
    for (i, left) in regions.iter().enumerate() {
        for right in &regions[i + 1..] {
            let (ls, le) = extent(left);
            let (rs, re) = extent(right);
            if ls < re && rs < le {
                return false;
            }
        }
    }
    if entry == 0 {
        return allow_zero_entry;
    }
    regions.iter().any(|r| {
        let (start, end) = extent(r);
        r.flags & IMAGE_REGION_EXECUTE != 0 && start <= i128::from(entry) && i128::from(entry) < end
    })
}

fn region(start: u64, len: u64, flags: u8) -> ImageRegion {
    ImageRegion { start, len, flags }
}

/// Visit every combination of one element from each of `slots` lists.
fn for_each_choice<T: Copy>(choices: &[T], arity: usize, visit: &mut impl FnMut(&[T])) {
    let mut index = vec![0usize; arity];
    let mut current = vec![choices[0]; arity];
    loop {
        for (slot, i) in current.iter_mut().zip(&index) {
            *slot = choices[*i];
        }
        visit(&current);
        let mut position = 0;
        loop {
            if position == arity {
                return;
            }
            index[position] += 1;
            if index[position] < choices.len() {
                break;
            }
            index[position] = 0;
            position += 1;
        }
    }
}

#[test]
fn admit_image_agrees_with_the_plan_predicate_for_zero_to_two_regions() {
    const BIG: u64 = u64::MAX;
    let starts = [0, 1, 2, 3, 4, BIG - 1, BIG];
    let lens = [0, 1, 2, 3, BIG];
    let flags = [
        0,
        IMAGE_REGION_READ,
        IMAGE_REGION_READ | IMAGE_REGION_EXECUTE,
        IMAGE_REGION_READ | IMAGE_REGION_WRITE,
        IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE,
        0x80,
    ];
    let mut candidates = Vec::new();
    for start in starts {
        for len in lens {
            for flag in flags {
                candidates.push(region(start, len, flag));
            }
        }
    }
    let entries = [0, 1, 2, 3, 4, 5, BIG];
    let windows = [(1, 5), (2, 4), (5, 1), (3, 3), (0, BIG)];
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut check = |regions: &[ImageRegion]| {
        for entry in entries {
            for (ws, we) in windows {
                for allow_zero in [false, true] {
                    let expected = plan_is_valid(entry, regions, ws, we, allow_zero);
                    let actual = admit_image(entry, regions, ws, we, allow_zero).is_ok();
                    assert_eq!(
                        actual, expected,
                        "entry={entry} regions={regions:?} window={ws}..{we} allow_zero={allow_zero}"
                    );
                    if actual {
                        accepted += 1;
                    } else {
                        rejected += 1;
                    }
                }
            }
        }
    };
    check(&[]);
    for one in &candidates {
        check(&[*one]);
    }
    for left in &candidates {
        for right in &candidates {
            check(&[*left, *right]);
        }
    }
    assert!(accepted > 1_000, "the domain must admit nontrivial plans");
    assert!(rejected > 100_000, "the domain must refuse nontrivial plans");
}

#[test]
fn admit_image_agrees_with_the_plan_predicate_for_three_regions() {
    let starts = [0, 1, 2, 3, 4];
    let lens = [0, 1, 2, 3];
    let flags = [
        IMAGE_REGION_READ,
        IMAGE_REGION_READ | IMAGE_REGION_EXECUTE,
        IMAGE_REGION_READ | IMAGE_REGION_WRITE,
        IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE,
    ];
    let mut candidates = Vec::new();
    for start in starts {
        for len in lens {
            for flag in flags {
                candidates.push(region(start, len, flag));
            }
        }
    }
    let mut accepted = 0usize;
    for_each_choice(&candidates, 3, &mut |regions| {
        for entry in [0, 1, 2, 3, 4, 6] {
            for allow_zero in [false, true] {
                let expected = plan_is_valid(entry, regions, 1, 5, allow_zero);
                let actual = admit_image(entry, regions, 1, 5, allow_zero).is_ok();
                assert_eq!(actual, expected, "entry={entry} regions={regions:?}");
                accepted += usize::from(actual);
            }
        }
    });
    assert!(accepted > 100, "the domain must admit nontrivial plans");
}

// ---------------------------------------------------------------------------
// ELF64 header and program-header bytes.
// ---------------------------------------------------------------------------

const ELF_WINDOW: [(u64, u64); 2] = [(0x1000, 0x9000), (0x2000, 0x4000)];

fn elf_header(entry: u64, phnum: u16) -> [u8; ELF64_HEADER_SIZE] {
    let mut header = [0_u8; ELF64_HEADER_SIZE];
    header[0..4].copy_from_slice(b"\x7fELF");
    header[4] = 2;
    header[5] = 1;
    header[6] = 1;
    header[16..18].copy_from_slice(&ELF64_ET_EXEC.to_le_bytes());
    header[18..20].copy_from_slice(&ELF64_EM_X86_64.to_le_bytes());
    header[20..24].copy_from_slice(&1_u32.to_le_bytes());
    header[24..32].copy_from_slice(&entry.to_le_bytes());
    header[32..40].copy_from_slice(&(ELF64_HEADER_SIZE as u64).to_le_bytes());
    header[52..54].copy_from_slice(&(ELF64_HEADER_SIZE as u16).to_le_bytes());
    header[54..56].copy_from_slice(&(ELF64_PROGRAM_HEADER_SIZE as u16).to_le_bytes());
    header[56..58].copy_from_slice(&phnum.to_le_bytes());
    header
}

fn elf_load(flags: u32, vaddr: u64, mem_size: u64) -> [u8; ELF64_PROGRAM_HEADER_SIZE] {
    let mut ph = [0_u8; ELF64_PROGRAM_HEADER_SIZE];
    ph[0..4].copy_from_slice(&ELF64_PT_LOAD.to_le_bytes());
    ph[4..8].copy_from_slice(&flags.to_le_bytes());
    // offset == vaddr keeps `offset % align == vaddr % align`.
    ph[8..16].copy_from_slice(&vaddr.to_le_bytes());
    ph[16..24].copy_from_slice(&vaddr.to_le_bytes());
    ph[32..40].copy_from_slice(&mem_size.min(0x100).to_le_bytes());
    ph[40..48].copy_from_slice(&mem_size.to_le_bytes());
    ph[48..56].copy_from_slice(&0x1000_u64.to_le_bytes());
    ph
}

/// The ELF64 `ValidBytes` predicate for a table of `PT_LOAD` headers, derived
/// from the ELF gABI and the model: page-rounded mappings are inside the user
/// window, no mapping is W+X, mappings do not share a page, load addresses are
/// ordered, and a non-zero entry lies in an executable segment.
fn elf_exec_is_valid(
    loads: &[(u32, u64, u64)],
    entry: u64,
    window_start: u64,
    window_end: u64,
) -> bool {
    let page = 0x1000_i128;
    let mut previous_vaddr = 0_i128;
    let mut regions = Vec::new();
    for (index, (flags, vaddr, mem)) in loads.iter().copied().enumerate() {
        let vaddr = i128::from(vaddr);
        let mem = i128::from(mem);
        if flags & !ELF64_KNOWN_PF != 0
            || flags & (ELF64_PF_W | ELF64_PF_X) == (ELF64_PF_W | ELF64_PF_X)
            || mem == 0
            || (index > 0 && vaddr < previous_vaddr)
        {
            return false;
        }
        previous_vaddr = vaddr;
        let mapped_start = vaddr & !(page - 1);
        let mapped_end = mapped_start + ((vaddr & (page - 1)) + mem + page - 1) / page * page;
        if mapped_start < i128::from(window_start) || mapped_end > i128::from(window_end) {
            return false;
        }
        regions.push((vaddr, vaddr + mem, mapped_start, mapped_end, flags));
    }
    for (i, left) in regions.iter().enumerate() {
        for right in &regions[i + 1..] {
            if left.2 < right.3 && right.2 < left.3 {
                return false;
            }
        }
    }
    entry != 0
        && regions.iter().any(|r| {
            r.4 & ELF64_PF_X != 0 && r.0 <= i128::from(entry) && i128::from(entry) < r.1
        })
}

#[test]
fn elf64_exec_images_agree_with_the_byte_predicate_for_two_load_segments() {
    let flag_sets = [
        ELF64_PF_R | ELF64_PF_X,
        ELF64_PF_R | ELF64_PF_W,
        ELF64_PF_W | ELF64_PF_X,
        ELF64_PF_R,
        0x8,
    ];
    let vaddrs = [0x0, 0x1000, 0x1800, 0x2000, 0x3000, 0x3ff0, 0x8000, 0xf000];
    let sizes = [0, 0x100, 0x1000, 0x1800, 0x7000];
    let mut segments = Vec::new();
    for flags in flag_sets {
        for vaddr in vaddrs {
            for size in sizes {
                segments.push((flags, vaddr, size));
            }
        }
    }
    let entries = [0, 0x1000, 0x1100, 0x2000, 0x2800, 0x3000, 0x8000, 0xf000];
    let (mut accepted, mut rejected) = (0usize, 0usize);
    for &left in &segments {
        for &right in &segments {
            let loads = [left, right];
            let mut table = Vec::new();
            for (flags, vaddr, size) in loads {
                table.extend_from_slice(&elf_load(flags, vaddr, size));
            }
            for entry in entries {
                let header = elf_header(entry, 2);
                for (window_start, window_end) in ELF_WINDOW {
                    let expected = elf_exec_is_valid(&loads, entry, window_start, window_end);
                    let actual =
                        admit_elf64_image(&header, &table, 0, window_start, window_end);
                    assert_eq!(
                        actual.is_ok(),
                        expected,
                        "loads={loads:x?} entry={entry:#x} window={window_start:#x}..{window_end:#x}: {actual:?}"
                    );
                    if expected {
                        accepted += 1;
                    } else {
                        rejected += 1;
                    }
                }
            }
        }
    }
    assert!(accepted > 200, "the domain must admit nontrivial images");
    assert!(rejected > 100_000, "the domain must refuse nontrivial images");
}

#[test]
fn elf64_every_byte_mutation_is_rejected_or_stays_inside_the_window() {
    let header = elf_header(0x2100, 1);
    let ph = elf_load(ELF64_PF_R | ELF64_PF_X, 0x2000, 0x1000);
    let values = [0x00, 0x01, 0x02, 0x7f, 0x80, 0xfe, 0xff];
    let (window_start, window_end) = (0x1000, 0x10_000);
    let baseline = admit_elf64_image(&header, &ph, 0x4000, window_start, window_end)
        .expect("fixture is admissible");
    assert_eq!(baseline.entry, 0x2100);
    let mut accepted = 0usize;
    let mut check = |header: &[u8; ELF64_HEADER_SIZE], ph: &[u8; ELF64_PROGRAM_HEADER_SIZE]| {
        if let Ok(summary) = admit_elf64_image(header, ph, 0x4000, window_start, window_end) {
            accepted += 1;
            let entry = u64::from_le_bytes(header[24..32].try_into().unwrap());
            let vaddr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
            let mem = u64::from_le_bytes(ph[40..48].try_into().unwrap());
            let flags = u32::from_le_bytes(ph[4..8].try_into().unwrap());
            assert_eq!(summary.load_regions, 1);
            assert!(flags & ELF64_PF_X != 0 && flags & ELF64_PF_W == 0, "{flags:#x}");
            // ET_EXEC (type 2) has no bias; ET_DYN is rebased into the window.
            assert!(summary.entry >= window_start && summary.entry < window_end);
            assert!(vaddr.saturating_add(mem) <= window_end || summary.load_bias != 0);
            assert!(entry != 0);
        }
    };
    for position in 0..ELF64_HEADER_SIZE {
        for value in values {
            let mut mutated = header;
            mutated[position] = value;
            check(&mutated, &ph);
        }
    }
    for position in 0..ELF64_PROGRAM_HEADER_SIZE {
        for value in values {
            let mut mutated = ph;
            mutated[position] = value;
            check(&header, &mutated);
        }
    }
    assert!(accepted > 0, "some mutations must remain admissible");
}

#[test]
fn elf64_every_truncation_of_the_program_table_is_rejected() {
    let header = elf_header(0x2100, 2);
    let mut table = Vec::new();
    table.extend_from_slice(&elf_load(ELF64_PF_R | ELF64_PF_X, 0x2000, 0x1000));
    table.extend_from_slice(&elf_load(ELF64_PF_R, 0x4000, 0x1000));
    assert!(admit_elf64_image(&header, &table, 0, 0x1000, 0x9000).is_ok());
    for len in 0..table.len() {
        assert_eq!(
            admit_elf64_image(&header, &table[..len], 0, 0x1000, 0x9000),
            Err(ByteAdmissionError::Truncated),
            "len={len}"
        );
    }
    let mut longer = table.clone();
    longer.push(0);
    assert_eq!(
        admit_elf64_image(&header, &longer, 0, 0x1000, 0x9000),
        Err(ByteAdmissionError::Truncated)
    );
}

// ---------------------------------------------------------------------------
// PE32+ headers and section table.
// ---------------------------------------------------------------------------

const PE_LOAD_BASE: u64 = 0x40_0000;
const PE_IMAGE_SIZE: u32 = 0x5000;
const PE_WINDOW: (u64, u64) = (0x1000, 0x80_0000);

#[derive(Clone)]
struct PeFixture {
    dos: [u8; PE64_DOS_HEADER_SIZE],
    file: [u8; PE64_FILE_HEADER_SIZE],
    optional: [u8; 240],
    sections: Vec<u8>,
}

fn pe_section(chars: u32, virtual_address: u32, virtual_size: u32) -> [u8; PE64_SECTION_HEADER_SIZE] {
    let mut section = [0_u8; PE64_SECTION_HEADER_SIZE];
    section[0..5].copy_from_slice(b".text");
    section[8..12].copy_from_slice(&virtual_size.to_le_bytes());
    section[12..16].copy_from_slice(&virtual_address.to_le_bytes());
    // raw_size == 0 leaves file alignment unconstrained: this domain studies
    // the memory layout, not the file layout.
    section[36..40].copy_from_slice(&chars.to_le_bytes());
    section
}

fn pe_fixture(entry_rva: u32, sections: &[[u8; PE64_SECTION_HEADER_SIZE]]) -> PeFixture {
    let mut dos = [0_u8; PE64_DOS_HEADER_SIZE];
    dos[0..2].copy_from_slice(b"MZ");
    dos[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());

    let mut file = [0_u8; PE64_FILE_HEADER_SIZE];
    file[0..4].copy_from_slice(b"PE\0\0");
    file[4..6].copy_from_slice(&PE64_MACHINE_AMD64.to_le_bytes());
    file[6..8].copy_from_slice(&(sections.len() as u16).to_le_bytes());
    file[20..22].copy_from_slice(&240_u16.to_le_bytes());

    let mut optional = [0_u8; 240];
    optional[0..2].copy_from_slice(&PE64_OPTIONAL_MAGIC.to_le_bytes());
    optional[16..20].copy_from_slice(&entry_rva.to_le_bytes());
    optional[24..32].copy_from_slice(&PE_LOAD_BASE.to_le_bytes());
    optional[32..36].copy_from_slice(&0x1000_u32.to_le_bytes());
    optional[36..40].copy_from_slice(&0x200_u32.to_le_bytes());
    optional[56..60].copy_from_slice(&PE_IMAGE_SIZE.to_le_bytes());
    optional[60..64].copy_from_slice(&0x200_u32.to_le_bytes());
    optional[108..112].copy_from_slice(&16_u32.to_le_bytes());

    PeFixture {
        dos,
        file,
        optional,
        sections: sections.concat(),
    }
}

fn admit_pe(fixture: &PeFixture, require_dll: bool) -> Result<Pe64ImageSummary, ByteAdmissionError> {
    admit_pe64_image_headers(
        &fixture.dos,
        &fixture.file,
        &fixture.optional,
        &fixture.sections,
        PE_LOAD_BASE,
        PE_WINDOW.0,
        PE_WINDOW.1,
        128 * 1024 * 1024,
        require_dll,
    )
}

/// Optional-header geometry the layout predicate depends on.
#[derive(Clone, Copy)]
struct PeGeometry {
    image_size: u32,
    headers_size: u32,
    section_alignment: u32,
}

/// The unmutated fixture geometry.
const PE_GEOMETRY: PeGeometry = PeGeometry {
    image_size: PE_IMAGE_SIZE,
    headers_size: 0x200,
    section_alignment: 0x1000,
};

/// PE64 `ValidBytes` for a memory layout: every non-empty section is
/// section-aligned, past the headers, inside the declared image, has a known
/// and non-W+X permission set, sections do not overlap, and the entry is zero
/// only for a DLL and otherwise inside an executable section.
fn pe_layout_is_valid(
    sections: &[(u32, u32, u32)],
    entry_rva: u32,
    require_dll: bool,
    geometry: PeGeometry,
) -> bool {
    let image = i128::from(geometry.image_size);
    let alignment = i128::from(geometry.section_alignment);
    let minimum_rva = (i128::from(geometry.headers_size) + alignment - 1) / alignment * alignment;
    let mut extents = Vec::new();
    for (chars, va, size) in sections.iter().copied() {
        if size == 0 {
            continue;
        }
        let (va, size) = (i128::from(va), i128::from(size));
        let known = chars & PE64_KNOWN_MEMORY_FLAGS;
        if va < minimum_rva
            || va % alignment != 0
            || known == 0
            || chars & (PE64_SCN_WRITE | PE64_SCN_EXECUTE) == (PE64_SCN_WRITE | PE64_SCN_EXECUTE)
            || va + size > image
        {
            return false;
        }
        extents.push((va, va + size, chars & PE64_SCN_EXECUTE != 0));
    }
    if extents.is_empty() {
        return false;
    }
    for (i, left) in extents.iter().enumerate() {
        for right in &extents[i + 1..] {
            if left.0 < right.1 && right.0 < left.1 {
                return false;
            }
        }
    }
    if entry_rva == 0 {
        return require_dll;
    }
    extents
        .iter()
        .any(|e| e.2 && e.0 <= i128::from(entry_rva) && i128::from(entry_rva) < e.1)
}

#[test]
fn pe64_images_agree_with_the_layout_predicate_for_two_sections() {
    let chars = [
        PE64_SCN_READ,
        PE64_SCN_READ | PE64_SCN_EXECUTE,
        PE64_SCN_READ | PE64_SCN_WRITE,
        PE64_SCN_WRITE | PE64_SCN_EXECUTE,
        PE64_SCN_READ | PE64_SCN_WRITE | PE64_SCN_EXECUTE,
        0x0000_0020, // code flag only: no read/write/execute memory permission
    ];
    let vas = [0x0, 0x0800, 0x1000, 0x1800, 0x2000, 0x3000, 0x4000, 0x5000];
    let sizes = [0, 0x800, 0x1000, 0x1800, 0x4000];
    let mut candidates = Vec::new();
    for c in chars {
        for va in vas {
            for size in sizes {
                candidates.push((c, va, size));
            }
        }
    }
    let entries = [0, 0x800, 0x1000, 0x1800, 0x2000, 0x3000, 0x4000, 0x5000];
    let (mut accepted, mut rejected) = (0usize, 0usize);
    for &left in &candidates {
        for &right in &candidates {
            let layout = [left, right];
            let raw = [
                pe_section(left.0, left.1, left.2),
                pe_section(right.0, right.1, right.2),
            ];
            for entry in entries {
                let fixture = pe_fixture(entry, &raw);
                // `file_is_dll` is the header bit; `require_dll` is the
                // loader's expectation. A mismatch must never be admitted.
                for (file_is_dll, require_dll) in
                    [(false, false), (true, true), (true, false), (false, true)]
                {
                    let mut fixture = fixture.clone();
                    if file_is_dll {
                        fixture.file[22..24].copy_from_slice(&PE64_FILE_DLL.to_le_bytes());
                    }
                    let expected = file_is_dll == require_dll
                        && pe_layout_is_valid(&layout, entry, require_dll, PE_GEOMETRY);
                    let actual = admit_pe(&fixture, require_dll);
                    assert_eq!(
                        actual.is_ok(),
                        expected,
                        "sections={layout:x?} entry={entry:#x} file_dll={file_is_dll} require_dll={require_dll}: {actual:?}"
                    );
                    if expected {
                        accepted += 1;
                    } else {
                        rejected += 1;
                    }
                }
            }
        }
    }
    assert!(accepted > 500, "the domain must admit nontrivial images");
    assert!(rejected > 100_000, "the domain must refuse nontrivial images");
}

#[test]
fn pe64_every_byte_mutation_is_rejected_or_yields_a_valid_layout() {
    let raw = [
        pe_section(PE64_SCN_READ | PE64_SCN_EXECUTE, 0x1000, 0x1000),
        pe_section(PE64_SCN_READ | PE64_SCN_WRITE, 0x2000, 0x1000),
    ];
    let fixture = pe_fixture(0x1000, &raw);
    assert!(admit_pe(&fixture, false).is_ok());
    let values = [0x00, 0x01, 0x02, 0x7f, 0x80, 0xfe, 0xff];
    let mut accepted = 0usize;
    let mut check = |fixture: &PeFixture| {
        let dll = u16::from_le_bytes(fixture.file[22..24].try_into().unwrap()) & PE64_FILE_DLL != 0;
        let Ok(summary) = admit_pe(fixture, dll) else {
            return;
        };
        accepted += 1;
        let entry_rva = u32::from_le_bytes(fixture.optional[16..20].try_into().unwrap());
        let layout: Vec<(u32, u32, u32)> = summary.sections[..usize::from(summary.section_count)]
            .iter()
            .map(|s| (s.characteristics, s.virtual_address, s.virtual_size.max(s.raw_size)))
            .collect();
        // Geometry comes from the mutated bytes: the oracle must judge the
        // layout against the image the header actually declared.
        let word = |offset: usize| {
            u32::from_le_bytes(fixture.optional[offset..offset + 4].try_into().unwrap())
        };
        let geometry = PeGeometry {
            image_size: word(56),
            headers_size: word(60),
            section_alignment: word(32),
        };
        assert!(
            pe_layout_is_valid(&layout, entry_rva, dll, geometry),
            "admitted an invalid layout {layout:x?} entry={entry_rva:#x} dll={dll}"
        );
        assert_eq!(summary.load_base, PE_LOAD_BASE);
        assert!(summary.image_size >= u64::from(geometry.image_size));
        assert_eq!(summary.is_dll, dll);
    };
    for position in 0..PE64_DOS_HEADER_SIZE {
        for value in values {
            let mut mutated = fixture.clone();
            mutated.dos[position] = value;
            check(&mutated);
        }
    }
    for position in 0..PE64_FILE_HEADER_SIZE {
        for value in values {
            let mut mutated = fixture.clone();
            mutated.file[position] = value;
            check(&mutated);
        }
    }
    for position in 0..fixture.optional.len() {
        for value in values {
            let mut mutated = fixture.clone();
            mutated.optional[position] = value;
            check(&mutated);
        }
    }
    for position in 0..fixture.sections.len() {
        for value in values {
            let mut mutated = fixture.clone();
            mutated.sections[position] = value;
            check(&mutated);
        }
    }
    assert!(accepted > 0, "some mutations must remain admissible");
}

#[test]
fn pe64_every_truncated_or_extended_table_is_rejected() {
    let raw = [
        pe_section(PE64_SCN_READ | PE64_SCN_EXECUTE, 0x1000, 0x1000),
        pe_section(PE64_SCN_READ, 0x2000, 0x1000),
    ];
    let fixture = pe_fixture(0x1000, &raw);
    assert!(admit_pe(&fixture, false).is_ok());
    for len in 0..fixture.sections.len() {
        let result = admit_pe64_image_headers(
            &fixture.dos,
            &fixture.file,
            &fixture.optional,
            &fixture.sections[..len],
            PE_LOAD_BASE,
            PE_WINDOW.0,
            PE_WINDOW.1,
            128 * 1024 * 1024,
            false,
        );
        assert!(result.is_err(), "section table of {len} bytes was admitted");
    }
    let mut longer = fixture.sections.clone();
    longer.push(0);
    assert!(
        admit_pe64_image_headers(
            &fixture.dos,
            &fixture.file,
            &fixture.optional,
            &longer,
            PE_LOAD_BASE,
            PE_WINDOW.0,
            PE_WINDOW.1,
            128 * 1024 * 1024,
            false,
        )
        .is_err()
    );
    // A slice longer than the size the file header declares is inconsistent
    // input, not trailing slack.
    let mut longer_optional = fixture.optional.to_vec();
    longer_optional.push(0);
    assert!(
        admit_pe64_image_headers(
            &fixture.dos,
            &fixture.file,
            &longer_optional,
            &fixture.sections,
            PE_LOAD_BASE,
            PE_WINDOW.0,
            PE_WINDOW.1,
            128 * 1024 * 1024,
            false,
        )
        .is_err()
    );
    for len in 0..fixture.optional.len() {
        let result = admit_pe64_image_headers(
            &fixture.dos,
            &fixture.file,
            &fixture.optional[..len],
            &fixture.sections,
            PE_LOAD_BASE,
            PE_WINDOW.0,
            PE_WINDOW.1,
            128 * 1024 * 1024,
            false,
        );
        assert!(result.is_err(), "optional header of {len} bytes was admitted");
    }
}

// ---------------------------------------------------------------------------
// Relocation and import tables: panic-freedom and write confinement.
// ---------------------------------------------------------------------------

const RELOC_IMAGE: usize = 256;
const RELOC_TABLE: usize = 0x20;

fn reloc_image() -> [u8; RELOC_IMAGE] {
    let mut image = [0_u8; RELOC_IMAGE];
    image[RELOC_TABLE..RELOC_TABLE + 4].copy_from_slice(&0_u32.to_le_bytes());
    image[RELOC_TABLE + 4..RELOC_TABLE + 8].copy_from_slice(&12_u32.to_le_bytes());
    image[RELOC_TABLE + 8..RELOC_TABLE + 10]
        .copy_from_slice(&((PE64_RELOC_DIR64 << 12) | 0x80).to_le_bytes());
    image[RELOC_TABLE + 10..RELOC_TABLE + 12]
        .copy_from_slice(&((PE64_RELOC_DIR64 << 12) | 0xa0).to_le_bytes());
    image[0x80..0x88].copy_from_slice(&0x3000_u64.to_le_bytes());
    image[0xa0..0xa8].copy_from_slice(&0x3100_u64.to_le_bytes());
    image
}

#[test]
fn pe64_relocation_never_writes_outside_the_declared_dir64_targets() {
    let baseline = reloc_image();
    let mut image = baseline;
    assert_eq!(
        apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, RELOC_TABLE as u32, 12, 0),
        Ok(2)
    );
    let values = [0x00, 0x01, 0x02, 0x7f, 0x80, 0xa0, 0xfe, 0xff];
    for position in 0..RELOC_IMAGE {
        for value in values {
            let mut original = baseline;
            original[position] = value;
            let allowed = dir64_targets(&original);
            for load_base in [0x1000_u64, 0x2000, 0x800] {
                let mut patched = original;
                // Both outcomes must keep every write inside a DIR64 target
                // that the (mutated) table itself declares.
                let _ = apply_pe64_base_relocations(
                    &mut patched,
                    0x1000,
                    load_base,
                    RELOC_TABLE as u32,
                    12,
                    0,
                );
                for (index, (before, after)) in original.iter().zip(&patched).enumerate() {
                    assert!(
                        before == after || allowed.iter().any(|t| (*t..*t + 8).contains(&index)),
                        "byte {index:#x} changed outside {allowed:x?} (mutated byte {position:#x}={value:#x}, base {load_base:#x})"
                    );
                }
            }
        }
    }
}

#[test]
fn pe64_relocation_block_with_an_unaligned_page_rva_is_invalid() {
    let mut image = reloc_image();
    image[RELOC_TABLE..RELOC_TABLE + 4].copy_from_slice(&0x10_u32.to_le_bytes());
    assert_eq!(
        apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, RELOC_TABLE as u32, 12, 0),
        Err(ByteAdmissionError::InvalidValue)
    );
}

/// Independent decode of the single relocation block in `reloc_image`.
fn dir64_targets(image: &[u8; RELOC_IMAGE]) -> Vec<usize> {
    let page = u32::from_le_bytes(image[RELOC_TABLE..RELOC_TABLE + 4].try_into().unwrap()) as usize;
    let mut targets = Vec::new();
    for entry_offset in [RELOC_TABLE + 8, RELOC_TABLE + 10] {
        let entry = u16::from_le_bytes(image[entry_offset..entry_offset + 2].try_into().unwrap());
        if entry >> 12 == PE64_RELOC_DIR64 {
            targets.push(page + usize::from(entry & 0x0fff));
        }
    }
    targets
}

fn import_image() -> [u8; 256] {
    let mut image = [0_u8; 256];
    image[0x20..0x24].copy_from_slice(&0xa0_u32.to_le_bytes());
    image[0x2c..0x30].copy_from_slice(&0x80_u32.to_le_bytes());
    image[0x30..0x34].copy_from_slice(&0xb0_u32.to_le_bytes());
    image[0x80..0x89].copy_from_slice(b"test.dll\0");
    image[0xa0..0xa8].copy_from_slice(&0xc0_u64.to_le_bytes());
    image[0xc2..0xc7].copy_from_slice(b"Func\0");
    image
}

#[test]
fn pe64_import_table_admission_is_total_and_bounded_under_every_byte_mutation() {
    let baseline = import_image();
    assert_eq!(
        validate_pe64_import_table(&baseline, 0x20, 0x28, 8),
        Ok(Pe64ImportSummary {
            descriptors: 1,
            imports: 1
        })
    );
    let values = [0x00, 0x01, 0x2e, 0x41, 0x7f, 0x80, 0xff];
    let mut accepted = 0usize;
    for position in 0..baseline.len() {
        for value in values {
            let mut image = baseline;
            image[position] = value;
            for (rva, size) in [(0x20, 0x28), (0x20, 0x14), (0x20, 0xe0), (0xf0, 0x28)] {
                // A bounded image never makes the validator index out of range
                // and the import budget is honoured.
                if let Ok(summary) = validate_pe64_import_table(&image, rva, size, 2) {
                    accepted += 1;
                    assert!(summary.imports <= 2);
                    assert!(summary.descriptors <= size as usize / PE64_IMPORT_DESCRIPTOR_BYTES);
                }
            }
        }
    }
    assert!(accepted > 0, "some mutations must remain admissible");
}
