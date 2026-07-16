#![no_std]

//! Format-neutral executable mapping admission shared by the Linux ELF and
//! Windows PE64 loader paths.
//!
//! Format ownership remains in `loaderd`, but every byte-derived ELF load
//! segment, PE64 relocation block, and PE64 import table crosses this crate
//! before loaderd may construct broker mappings.  Keeping the bounded parsers
//! here makes the source-level admission proof exercise the same code as the
//! production loader rather than a second test-only parser.

pub const IMAGE_REGION_READ: u8 = 1 << 0;
pub const IMAGE_REGION_WRITE: u8 = 1 << 1;
pub const IMAGE_REGION_EXECUTE: u8 = 1 << 2;
pub const IMAGE_REGION_KNOWN_FLAGS: u8 =
    IMAGE_REGION_READ | IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE;

pub const ELF64_PROGRAM_HEADER_SIZE: usize = 56;
pub const ELF64_HEADER_SIZE: usize = 64;
pub const ELF64_MAX_PROGRAM_HEADERS: usize = 128;
pub const ELF64_PT_LOAD: u32 = 1;
pub const ELF64_PT_INTERP: u32 = 3;
pub const ELF64_PT_SHLIB: u32 = 5;
pub const ELF64_PT_PHDR: u32 = 6;
pub const ELF64_ET_EXEC: u16 = 2;
pub const ELF64_ET_DYN: u16 = 3;
pub const ELF64_EM_X86_64: u16 = 62;
pub const ELF64_PF_X: u32 = 1;
pub const ELF64_PF_W: u32 = 2;
pub const ELF64_PF_R: u32 = 4;
pub const ELF64_KNOWN_PF: u32 = ELF64_PF_X | ELF64_PF_W | ELF64_PF_R;

pub const PE64_RELOC_ABSOLUTE: u16 = 0;
pub const PE64_RELOC_DIR64: u16 = 10;
pub const PE64_FILE_RELOCS_STRIPPED: u16 = 0x0001;
pub const PE64_IMPORT_DESCRIPTOR_BYTES: usize = 20;
pub const PE64_IMPORT_THUNK_BYTES: usize = 8;
pub const PE64_ORDINAL_FLAG: u64 = 1 << 63;
pub const PE64_ORDINAL_RESERVED_MASK: u64 = 0x7fff_ffff_ffff_0000;
pub const PE64_NAME_RESERVED_MASK: u64 = 0x7fff_ffff_8000_0000;
pub const PE64_DOS_HEADER_SIZE: usize = 64;
pub const PE64_FILE_HEADER_SIZE: usize = 24;
pub const PE64_SECTION_HEADER_SIZE: usize = 40;
pub const PE64_MAX_SECTIONS: usize = 128;
pub const PE64_MACHINE_AMD64: u16 = 0x8664;
pub const PE64_OPTIONAL_MAGIC: u16 = 0x20b;
pub const PE64_FILE_DLL: u16 = 0x2000;
pub const PE64_SCN_EXECUTE: u32 = 0x2000_0000;
pub const PE64_SCN_READ: u32 = 0x4000_0000;
pub const PE64_SCN_WRITE: u32 = 0x8000_0000;
pub const PE64_KNOWN_MEMORY_FLAGS: u32 = PE64_SCN_EXECUTE | PE64_SCN_READ | PE64_SCN_WRITE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageRegion {
    pub start: u64,
    pub len: u64,
    pub flags: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageAdmissionError {
    EmptyAddressWindow,
    EmptyImage,
    ZeroLengthRegion,
    AddressOverflow,
    AddressOutOfRange,
    UnknownRegionFlags,
    WritableExecutableRegion,
    OverlappingRegions,
    MissingEntryPoint,
    EntryPointOutsideExecutableRegion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ByteAdmissionError {
    Truncated,
    InvalidValue,
    AddressOverflow,
    AddressOutOfRange,
    WritableExecutableRegion,
    MissingRelocations,
    RelocationsStripped,
    UnsupportedRelocation,
    MissingTerminator,
    TooManyImports,
    OverlappingRegions,
    MissingEntryPoint,
    EntryPointOutsideExecutableRegion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pe64ImportSummary {
    pub descriptors: usize,
    pub imports: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elf64ImageSummary {
    pub load_bias: u64,
    pub entry: u64,
    pub program_headers: u16,
    pub load_regions: u16,
    pub has_interpreter: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pe64DataDirectory {
    pub rva: u32,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pe64SectionSummary {
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pe64ImageSummary {
    pub preferred_base: u64,
    pub load_base: u64,
    pub image_size: u64,
    pub headers_size: u64,
    pub entry_point: u64,
    pub is_dll: bool,
    pub section_count: u16,
    pub sections: [Pe64SectionSummary; PE64_MAX_SECTIONS],
    pub directories: [Pe64DataDirectory; 16],
}

fn admit_pe64_section(
    raw: &[u8],
    load_base: u64,
    minimum_section_rva: u64,
    section_alignment: u64,
    file_alignment: u64,
    image_size: u64,
) -> Result<(Pe64SectionSummary, Option<ImageRegion>), ByteAdmissionError> {
    let section = Pe64SectionSummary {
        virtual_size: read_u32(raw, 8)?,
        virtual_address: read_u32(raw, 12)?,
        raw_size: read_u32(raw, 16)?,
        raw_offset: read_u32(raw, 20)?,
        characteristics: read_u32(raw, 36)?,
    };
    let section_size = u64::from(section.virtual_size.max(section.raw_size));
    if section_size == 0 {
        return Ok((section, None));
    }
    let virtual_address = u64::from(section.virtual_address);
    let raw_size = u64::from(section.raw_size);
    let raw_offset = u64::from(section.raw_offset);
    if virtual_address < minimum_section_rva
        || virtual_address % section_alignment != 0
        || (raw_size != 0 && (raw_offset % file_alignment != 0 || raw_size % file_alignment != 0))
        || section.characteristics & PE64_KNOWN_MEMORY_FLAGS == 0
        || section.characteristics & (PE64_SCN_WRITE | PE64_SCN_EXECUTE)
            == (PE64_SCN_WRITE | PE64_SCN_EXECUTE)
    {
        return Err(ByteAdmissionError::InvalidValue);
    }
    virtual_address
        .checked_add(section_size)
        .filter(|end| *end <= image_size)
        .ok_or(ByteAdmissionError::AddressOutOfRange)?;
    raw_offset
        .checked_add(raw_size)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let start = load_base
        .checked_add(virtual_address)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let mut flags = 0;
    if section.characteristics & PE64_SCN_READ != 0 {
        flags |= IMAGE_REGION_READ;
    }
    if section.characteristics & PE64_SCN_WRITE != 0 {
        flags |= IMAGE_REGION_WRITE;
    }
    if section.characteristics & PE64_SCN_EXECUTE != 0 {
        flags |= IMAGE_REGION_EXECUTE;
    }
    Ok((
        section,
        Some(ImageRegion {
            start,
            len: section_size,
            flags,
        }),
    ))
}

/// Validate all PE32+ byte tables that select image layout. The caller reads
/// section payloads with exact-length I/O, materializes them into an isolated
/// zeroed snapshot, then applies the relocation/import validators below.
// Keep the independently bounded PE tables and address window visible at the
// admission boundary; grouping them would obscure which raw input was proved.
#[allow(clippy::too_many_arguments)]
pub fn admit_pe64_image_headers(
    dos_header: &[u8; PE64_DOS_HEADER_SIZE],
    file_header: &[u8; PE64_FILE_HEADER_SIZE],
    optional_header: &[u8],
    section_headers: &[u8],
    load_base: u64,
    address_start: u64,
    address_end_exclusive: u64,
    max_image_bytes: u64,
    require_dll: bool,
) -> Result<Pe64ImageSummary, ByteAdmissionError> {
    if dos_header[0..2] != *b"MZ" || file_header[0..4] != *b"PE\0\0" {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let pe_offset = read_u32(dos_header, 0x3c)? as u64;
    if pe_offset < PE64_DOS_HEADER_SIZE as u64 {
        return Err(ByteAdmissionError::InvalidValue);
    }
    if read_u16(file_header, 4)? != PE64_MACHINE_AMD64 {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let section_count = read_u16(file_header, 6)? as usize;
    let optional_size = read_u16(file_header, 20)? as usize;
    let characteristics = read_u16(file_header, 22)?;
    if section_count == 0
        || section_count > PE64_MAX_SECTIONS
        || optional_size != optional_header.len()
        || optional_size < 112
        || (characteristics & PE64_FILE_DLL != 0) != require_dll
    {
        return Err(ByteAdmissionError::InvalidValue);
    }
    if section_headers.len()
        != section_count
            .checked_mul(PE64_SECTION_HEADER_SIZE)
            .ok_or(ByteAdmissionError::AddressOverflow)?
        || read_u16(optional_header, 0)? != PE64_OPTIONAL_MAGIC
    {
        return Err(ByteAdmissionError::Truncated);
    }

    let entry_rva = read_u32(optional_header, 16)?;
    let preferred_base = read_u64(optional_header, 24)?;
    let section_alignment = read_u32(optional_header, 32)? as u64;
    let file_alignment = read_u32(optional_header, 36)? as u64;
    let image_size = read_u32(optional_header, 56)? as u64;
    let headers_size = read_u32(optional_header, 60)? as u64;
    if section_alignment < 4096
        || !section_alignment.is_power_of_two()
        || !(512..=65_536).contains(&file_alignment)
        || !file_alignment.is_power_of_two()
        || file_alignment > section_alignment
        || image_size == 0
        || headers_size == 0
        || image_size > max_image_bytes
        || headers_size > image_size
        || !image_size.is_multiple_of(section_alignment)
        || !headers_size.is_multiple_of(file_alignment)
    {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let mapped_image_size = align_up_4k(image_size)?;
    let image_end = load_base
        .checked_add(mapped_image_size)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    if address_start >= address_end_exclusive
        || load_base < address_start
        || image_end > address_end_exclusive
    {
        return Err(ByteAdmissionError::AddressOutOfRange);
    }

    let directory_count = read_u32(optional_header, 108)? as usize;
    if directory_count > 16
        || optional_header.len()
            < 112usize
                .checked_add(
                    directory_count
                        .checked_mul(8)
                        .ok_or(ByteAdmissionError::AddressOverflow)?,
                )
                .ok_or(ByteAdmissionError::AddressOverflow)?
    {
        return Err(ByteAdmissionError::Truncated);
    }
    let mut directories = [Pe64DataDirectory::default(); 16];
    for (index, directory) in directories.iter_mut().take(directory_count).enumerate() {
        let offset = 112 + index * 8;
        directory.rva = read_u32(optional_header, offset)?;
        directory.size = read_u32(optional_header, offset + 4)?;
        if (directory.rva == 0) != (directory.size == 0) {
            return Err(ByteAdmissionError::InvalidValue);
        }
        if matches!(index, 0 | 1 | 5) && directory.rva != 0 {
            let end = u64::from(directory.rva)
                .checked_add(u64::from(directory.size))
                .ok_or(ByteAdmissionError::AddressOverflow)?;
            if end > image_size {
                return Err(ByteAdmissionError::AddressOutOfRange);
            }
        }
    }

    let minimum_section_rva = align_to(headers_size, section_alignment)?;
    let mut sections = [Pe64SectionSummary::default(); PE64_MAX_SECTIONS];
    let mut regions = [ImageRegion {
        start: 0,
        len: 0,
        flags: 0,
    }; PE64_MAX_SECTIONS];
    let mut region_count = 0usize;
    for (index, raw) in section_headers
        .chunks_exact(PE64_SECTION_HEADER_SIZE)
        .enumerate()
    {
        let (section, region) = admit_pe64_section(
            raw,
            load_base,
            minimum_section_rva,
            section_alignment,
            file_alignment,
            image_size,
        )?;
        sections[index] = section;
        if let Some(region) = region {
            regions[region_count] = region;
            region_count += 1;
        }
    }
    let entry_point = if entry_rva == 0 {
        0
    } else {
        load_base
            .checked_add(u64::from(entry_rva))
            .ok_or(ByteAdmissionError::AddressOverflow)?
    };
    admit_image(
        entry_point,
        &regions[..region_count],
        address_start,
        address_end_exclusive,
        require_dll,
    )
    .map_err(byte_error_from_image_error)?;
    Ok(Pe64ImageSummary {
        preferred_base,
        load_base,
        image_size: mapped_image_size,
        headers_size: align_up_4k(headers_size)?,
        entry_point,
        is_dll: require_dll,
        section_count: section_count as u16,
        sections,
        directories,
    })
}

fn align_to(value: u64, alignment: u64) -> Result<u64, ByteAdmissionError> {
    value
        .checked_add(alignment - 1)
        .map(|aligned| aligned & !(alignment - 1))
        .ok_or(ByteAdmissionError::AddressOverflow)
}

/// Validate the complete ELF64 header and program-header byte table used by
/// loaderd. File-backed payload reads remain exact-length operations in the
/// process broker, so a truncated `p_offset..p_filesz` range fails before
/// commit rather than becoming a zero-filled tail.
pub fn admit_elf64_image(
    header: &[u8; ELF64_HEADER_SIZE],
    phdrs: &[u8],
    dyn_load_offset: u64,
    address_start: u64,
    address_end_exclusive: u64,
) -> Result<Elf64ImageSummary, ByteAdmissionError> {
    if header[0..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 || header[6] != 1 {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let image_type = read_u16(header, 16)?;
    if !matches!(image_type, ELF64_ET_EXEC | ELF64_ET_DYN)
        || read_u16(header, 18)? != ELF64_EM_X86_64
        || read_u32(header, 20)? != 1
        || read_u16(header, 52)? as usize != ELF64_HEADER_SIZE
        || read_u16(header, 54)? as usize != ELF64_PROGRAM_HEADER_SIZE
    {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let phnum = read_u16(header, 56)? as usize;
    if phnum == 0 || phnum > ELF64_MAX_PROGRAM_HEADERS {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let table_len = phnum
        .checked_mul(ELF64_PROGRAM_HEADER_SIZE)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    if phdrs.len() != table_len {
        return Err(ByteAdmissionError::Truncated);
    }
    read_u64(header, 32)?
        .checked_add(table_len as u64)
        .ok_or(ByteAdmissionError::AddressOverflow)?;

    let mut minimum_load_page = u64::MAX;
    for ph in phdrs.chunks_exact(ELF64_PROGRAM_HEADER_SIZE) {
        if read_u32(ph, 0)? == ELF64_PT_LOAD && read_u64(ph, 40)? != 0 {
            minimum_load_page = minimum_load_page.min(read_u64(ph, 16)? & !0xfff);
        }
    }
    if minimum_load_page == u64::MAX {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let load_bias = if image_type == ELF64_ET_EXEC {
        0
    } else {
        address_start
            .checked_add(dyn_load_offset)
            .and_then(|base| base.checked_sub(minimum_load_page))
            .ok_or(ByteAdmissionError::AddressOverflow)?
    };

    let mut regions = [ImageRegion {
        start: 0,
        len: 0,
        flags: 0,
    }; ELF64_MAX_PROGRAM_HEADERS];
    let mut mapped_regions = [ImageRegion {
        start: 0,
        len: 0,
        flags: 0,
    }; ELF64_MAX_PROGRAM_HEADERS];
    let mut region_count = 0usize;
    let mut saw_load = false;
    let mut saw_interp = false;
    let mut saw_phdr = false;
    let mut previous_load_vaddr = 0_u64;
    for ph in phdrs.chunks_exact(ELF64_PROGRAM_HEADER_SIZE) {
        let kind = read_u32(ph, 0)?;
        let flags = read_u32(ph, 4)?;
        if flags & !ELF64_KNOWN_PF != 0 {
            return Err(ByteAdmissionError::InvalidValue);
        }
        match kind {
            ELF64_PT_LOAD => {
                let vaddr = read_u64(ph, 16)?;
                if saw_load && vaddr < previous_load_vaddr {
                    return Err(ByteAdmissionError::InvalidValue);
                }
                previous_load_vaddr = vaddr;
                saw_load = true;
                let region = admit_elf64_load_segment(
                    ph.try_into().map_err(|_| ByteAdmissionError::Truncated)?,
                    load_bias,
                    address_start,
                    address_end_exclusive,
                )?;
                let page_delta = vaddr & 0xfff;
                mapped_regions[region_count] = ImageRegion {
                    start: (vaddr & !0xfff)
                        .checked_add(load_bias)
                        .ok_or(ByteAdmissionError::AddressOverflow)?,
                    len: align_up_4k(
                        page_delta
                            .checked_add(read_u64(ph, 40)?)
                            .ok_or(ByteAdmissionError::AddressOverflow)?,
                    )?,
                    flags: region.flags,
                };
                regions[region_count] = region;
                region_count += 1;
            }
            ELF64_PT_INTERP => {
                if saw_interp || saw_load {
                    return Err(ByteAdmissionError::InvalidValue);
                }
                saw_interp = true;
            }
            ELF64_PT_PHDR => {
                if saw_phdr || saw_load || read_u64(ph, 40)? != table_len as u64 {
                    return Err(ByteAdmissionError::InvalidValue);
                }
                saw_phdr = true;
            }
            ELF64_PT_SHLIB => return Err(ByteAdmissionError::InvalidValue),
            _ => {}
        }
    }
    let entry = read_u64(header, 24)?
        .checked_add(load_bias)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    admit_image(
        entry,
        &regions[..region_count],
        address_start,
        address_end_exclusive,
        false,
    )
    .map_err(byte_error_from_image_error)?;
    reject_overlapping_regions(&mapped_regions[..region_count])?;
    Ok(Elf64ImageSummary {
        load_bias,
        entry,
        program_headers: phnum as u16,
        load_regions: region_count as u16,
        has_interpreter: saw_interp,
    })
}

fn reject_overlapping_regions(regions: &[ImageRegion]) -> Result<(), ByteAdmissionError> {
    for (index, region) in regions.iter().enumerate() {
        let end = region
            .start
            .checked_add(region.len)
            .ok_or(ByteAdmissionError::AddressOverflow)?;
        for previous in &regions[..index] {
            let previous_end = previous
                .start
                .checked_add(previous.len)
                .ok_or(ByteAdmissionError::AddressOverflow)?;
            if region.start < previous_end && previous.start < end {
                return Err(ByteAdmissionError::OverlappingRegions);
            }
        }
    }
    Ok(())
}

fn byte_error_from_image_error(error: ImageAdmissionError) -> ByteAdmissionError {
    match error {
        ImageAdmissionError::AddressOverflow => ByteAdmissionError::AddressOverflow,
        ImageAdmissionError::AddressOutOfRange | ImageAdmissionError::EmptyAddressWindow => {
            ByteAdmissionError::AddressOutOfRange
        }
        ImageAdmissionError::WritableExecutableRegion => {
            ByteAdmissionError::WritableExecutableRegion
        }
        ImageAdmissionError::OverlappingRegions => ByteAdmissionError::OverlappingRegions,
        ImageAdmissionError::MissingEntryPoint => ByteAdmissionError::MissingEntryPoint,
        ImageAdmissionError::EntryPointOutsideExecutableRegion => {
            ByteAdmissionError::EntryPointOutsideExecutableRegion
        }
        _ => ByteAdmissionError::InvalidValue,
    }
}

/// Parse and validate one raw ELF64 `PT_LOAD` program header.
///
/// The caller still owns ELF header and program-header-table validation.  This
/// function owns every byte field which can affect a mapping: file/memory
/// sizes, alignment, address arithmetic, page rounding, W^X and the admitted
/// user window.
pub fn admit_elf64_load_segment(
    ph: &[u8; ELF64_PROGRAM_HEADER_SIZE],
    load_bias: u64,
    address_start: u64,
    address_end_exclusive: u64,
) -> Result<ImageRegion, ByteAdmissionError> {
    if read_u32(ph, 0)? != ELF64_PT_LOAD {
        return Err(ByteAdmissionError::InvalidValue);
    }
    let flags = read_u32(ph, 4)?;
    if flags & !ELF64_KNOWN_PF != 0 {
        return Err(ByteAdmissionError::InvalidValue);
    }
    if flags & (ELF64_PF_W | ELF64_PF_X) == (ELF64_PF_W | ELF64_PF_X) {
        return Err(ByteAdmissionError::WritableExecutableRegion);
    }

    let offset = read_u64(ph, 8)?;
    let vaddr = read_u64(ph, 16)?;
    let file_size = read_u64(ph, 32)?;
    let mem_size = read_u64(ph, 40)?;
    let align = read_u64(ph, 48)?;
    if mem_size == 0 || file_size > mem_size {
        return Err(ByteAdmissionError::InvalidValue);
    }
    if align > 1 && (!align.is_power_of_two() || offset % align != vaddr % align) {
        return Err(ByteAdmissionError::InvalidValue);
    }
    offset
        .checked_add(file_size)
        .ok_or(ByteAdmissionError::AddressOverflow)?;

    let page_delta = vaddr & 0xfff;
    let mapped_start = (vaddr & !0xfff)
        .checked_add(load_bias)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let mapped_len = align_up_4k(
        page_delta
            .checked_add(mem_size)
            .ok_or(ByteAdmissionError::AddressOverflow)?,
    )?;
    let mapped_end = mapped_start
        .checked_add(mapped_len)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    if address_start >= address_end_exclusive
        || mapped_start < address_start
        || mapped_end > address_end_exclusive
    {
        return Err(ByteAdmissionError::AddressOutOfRange);
    }

    let start = vaddr
        .checked_add(load_bias)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let mut region_flags = 0;
    if flags & ELF64_PF_R != 0 {
        region_flags |= IMAGE_REGION_READ;
    }
    if flags & ELF64_PF_W != 0 {
        region_flags |= IMAGE_REGION_WRITE;
    }
    if flags & ELF64_PF_X != 0 {
        region_flags |= IMAGE_REGION_EXECUTE;
    }
    Ok(ImageRegion {
        start,
        len: mem_size,
        flags: region_flags,
    })
}

/// Validate and apply the PE32+ base-relocation table to an isolated image
/// snapshot.  No byte reaches a process mapping until this function succeeds.
pub fn apply_pe64_base_relocations(
    image: &mut [u8],
    preferred_base: u64,
    load_base: u64,
    reloc_rva: u32,
    reloc_size: u32,
    characteristics: u16,
) -> Result<usize, ByteAdmissionError> {
    let relocated = preferred_base != load_base;
    if characteristics & PE64_FILE_RELOCS_STRIPPED != 0 && relocated {
        return Err(ByteAdmissionError::RelocationsStripped);
    }
    if reloc_rva == 0 || reloc_size == 0 {
        return if reloc_rva == 0 && reloc_size == 0 && !relocated {
            Ok(0)
        } else if reloc_rva == 0 && reloc_size == 0 {
            Err(ByteAdmissionError::MissingRelocations)
        } else {
            Err(ByteAdmissionError::InvalidValue)
        };
    }

    let reloc_start = reloc_rva as usize;
    let reloc_end = reloc_start
        .checked_add(reloc_size as usize)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    if reloc_end > image.len() {
        return Err(ByteAdmissionError::Truncated);
    }

    let mut cursor = reloc_start;
    let mut patched_count = 0usize;
    while cursor < reloc_end {
        let header_end = cursor
            .checked_add(8)
            .ok_or(ByteAdmissionError::AddressOverflow)?;
        if header_end > reloc_end {
            return Err(ByteAdmissionError::Truncated);
        }
        let page_rva = read_u32(image, cursor)? as u64;
        if page_rva & 0xfff != 0 {
            return Err(ByteAdmissionError::InvalidValue);
        }
        let block_size = read_u32(image, cursor + 4)? as usize;
        if block_size < 8 || !block_size.is_multiple_of(2) {
            return Err(ByteAdmissionError::InvalidValue);
        }
        let block_end = cursor
            .checked_add(block_size)
            .ok_or(ByteAdmissionError::AddressOverflow)?;
        if block_end > reloc_end {
            return Err(ByteAdmissionError::Truncated);
        }

        let mut entry_offset = cursor + 8;
        while entry_offset < block_end {
            let entry = read_u16(image, entry_offset)?;
            match entry >> 12 {
                PE64_RELOC_ABSOLUTE => {}
                PE64_RELOC_DIR64 => {
                    let target_rva = page_rva
                        .checked_add(u64::from(entry & 0x0fff))
                        .ok_or(ByteAdmissionError::AddressOverflow)?;
                    let target = usize::try_from(target_rva)
                        .map_err(|_| ByteAdmissionError::AddressOverflow)?;
                    let target_end = target
                        .checked_add(8)
                        .ok_or(ByteAdmissionError::AddressOverflow)?;
                    if target_end > image.len() {
                        return Err(ByteAdmissionError::AddressOutOfRange);
                    }
                    if relocated {
                        let old = read_u64(image, target)?;
                        let patched = if load_base >= preferred_base {
                            old.checked_add(load_base - preferred_base)
                        } else {
                            old.checked_sub(preferred_base - load_base)
                        }
                        .ok_or(ByteAdmissionError::AddressOverflow)?;
                        image[target..target_end].copy_from_slice(&patched.to_le_bytes());
                        patched_count = patched_count
                            .checked_add(1)
                            .ok_or(ByteAdmissionError::AddressOverflow)?;
                    }
                }
                _ => return Err(ByteAdmissionError::UnsupportedRelocation),
            }
            entry_offset += 2;
        }
        cursor = block_end;
    }
    Ok(patched_count)
}

/// Validate the complete bounded PE32+ import descriptor and thunk tables.
/// Names must be non-empty NUL-terminated ASCII and reserved thunk bits must
/// be zero as required by the PE32+ format.
pub fn validate_pe64_import_table(
    image: &[u8],
    import_rva: u32,
    import_size: u32,
    max_imports: usize,
) -> Result<Pe64ImportSummary, ByteAdmissionError> {
    if import_rva == 0 || import_size == 0 {
        return if import_rva == 0 && import_size == 0 {
            Ok(Pe64ImportSummary {
                descriptors: 0,
                imports: 0,
            })
        } else {
            Err(ByteAdmissionError::InvalidValue)
        };
    }
    if max_imports == 0 {
        return Err(ByteAdmissionError::TooManyImports);
    }
    let mut descriptor = import_rva as usize;
    let limit = descriptor
        .checked_add(import_size as usize)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    if limit > image.len() {
        return Err(ByteAdmissionError::Truncated);
    }

    let mut descriptors = 0usize;
    let mut imports = 0usize;
    let mut terminated = false;
    while descriptor
        .checked_add(PE64_IMPORT_DESCRIPTOR_BYTES)
        .is_some_and(|end| end <= limit)
    {
        let original_first_thunk = read_u32(image, descriptor)?;
        let name_rva = read_u32(image, descriptor + 12)?;
        let first_thunk = read_u32(image, descriptor + 16)?;
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            terminated = true;
            break;
        }
        if name_rva == 0 || first_thunk == 0 {
            return Err(ByteAdmissionError::InvalidValue);
        }
        validate_ascii_c_string(image, name_rva, false)?;
        descriptors = descriptors
            .checked_add(1)
            .ok_or(ByteAdmissionError::AddressOverflow)?;

        let mut lookup_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let mut write_rva = first_thunk;
        loop {
            let lookup_offset = lookup_rva as usize;
            let entry = read_u64(image, lookup_offset)?;
            if entry == 0 {
                break;
            }
            read_u64(image, write_rva as usize)?;
            imports = imports
                .checked_add(1)
                .ok_or(ByteAdmissionError::AddressOverflow)?;
            if imports > max_imports {
                return Err(ByteAdmissionError::TooManyImports);
            }
            if entry & PE64_ORDINAL_FLAG != 0 {
                if entry & PE64_ORDINAL_RESERVED_MASK != 0 || entry & 0xffff == 0 {
                    return Err(ByteAdmissionError::InvalidValue);
                }
            } else {
                if entry & PE64_NAME_RESERVED_MASK != 0 {
                    return Err(ByteAdmissionError::InvalidValue);
                }
                let name_rva =
                    u32::try_from(entry).map_err(|_| ByteAdmissionError::AddressOverflow)?;
                let string_rva = name_rva
                    .checked_add(2)
                    .ok_or(ByteAdmissionError::AddressOverflow)?;
                validate_ascii_c_string(image, string_rva, false)?;
            }
            lookup_rva = lookup_rva
                .checked_add(PE64_IMPORT_THUNK_BYTES as u32)
                .ok_or(ByteAdmissionError::AddressOverflow)?;
            write_rva = write_rva
                .checked_add(PE64_IMPORT_THUNK_BYTES as u32)
                .ok_or(ByteAdmissionError::AddressOverflow)?;
        }
        descriptor += PE64_IMPORT_DESCRIPTOR_BYTES;
    }
    if !terminated {
        return Err(ByteAdmissionError::MissingTerminator);
    }
    Ok(Pe64ImportSummary {
        descriptors,
        imports,
    })
}

fn validate_ascii_c_string(
    bytes: &[u8],
    start: u32,
    allow_empty: bool,
) -> Result<(), ByteAdmissionError> {
    let start = start as usize;
    if start >= bytes.len() {
        return Err(ByteAdmissionError::Truncated);
    }
    let mut cursor = start;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == 0 {
            return if allow_empty || cursor != start {
                Ok(())
            } else {
                Err(ByteAdmissionError::InvalidValue)
            };
        }
        if !byte.is_ascii() || byte.is_ascii_control() {
            return Err(ByteAdmissionError::InvalidValue);
        }
        cursor += 1;
    }
    Err(ByteAdmissionError::MissingTerminator)
}

fn align_up_4k(value: u64) -> Result<u64, ByteAdmissionError> {
    value
        .checked_add(4095)
        .map(|aligned| aligned & !4095)
        .ok_or(ByteAdmissionError::AddressOverflow)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ByteAdmissionError> {
    let end = offset
        .checked_add(2)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let raw: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(ByteAdmissionError::Truncated)?
        .try_into()
        .map_err(|_| ByteAdmissionError::Truncated)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ByteAdmissionError> {
    let end = offset
        .checked_add(4)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(ByteAdmissionError::Truncated)?
        .try_into()
        .map_err(|_| ByteAdmissionError::Truncated)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ByteAdmissionError> {
    let end = offset
        .checked_add(8)
        .ok_or(ByteAdmissionError::AddressOverflow)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(ByteAdmissionError::Truncated)?
        .try_into()
        .map_err(|_| ByteAdmissionError::Truncated)?;
    Ok(u64::from_le_bytes(raw))
}

/// Admit one already-parsed executable image plan.
///
/// `allow_zero_entry` is reserved for PE DLLs whose optional entry point may
/// be absent. Main ELF and PE images must always provide an executable entry.
pub fn admit_image(
    entry_point: u64,
    regions: &[ImageRegion],
    address_start: u64,
    address_end_exclusive: u64,
    allow_zero_entry: bool,
) -> Result<(), ImageAdmissionError> {
    if address_start >= address_end_exclusive {
        return Err(ImageAdmissionError::EmptyAddressWindow);
    }
    if regions.is_empty() {
        return Err(ImageAdmissionError::EmptyImage);
    }

    let mut entry_is_executable = false;
    for (index, region) in regions.iter().copied().enumerate() {
        if region.len == 0 {
            return Err(ImageAdmissionError::ZeroLengthRegion);
        }
        if region.flags & !IMAGE_REGION_KNOWN_FLAGS != 0 {
            return Err(ImageAdmissionError::UnknownRegionFlags);
        }
        if region.flags & (IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE)
            == (IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE)
        {
            return Err(ImageAdmissionError::WritableExecutableRegion);
        }

        let region_end = region
            .start
            .checked_add(region.len)
            .ok_or(ImageAdmissionError::AddressOverflow)?;
        if region.start < address_start || region_end > address_end_exclusive {
            return Err(ImageAdmissionError::AddressOutOfRange);
        }

        if region.flags & IMAGE_REGION_EXECUTE != 0
            && (region.start..region_end).contains(&entry_point)
        {
            entry_is_executable = true;
        }

        for previous in &regions[..index] {
            let previous_end = previous
                .start
                .checked_add(previous.len)
                .ok_or(ImageAdmissionError::AddressOverflow)?;
            if region.start < previous_end && previous.start < region_end {
                return Err(ImageAdmissionError::OverlappingRegions);
            }
        }
    }

    if entry_point == 0 && allow_zero_entry {
        return Ok(());
    }
    if entry_point == 0 {
        return Err(ImageAdmissionError::MissingEntryPoint);
    }
    if !entry_is_executable {
        return Err(ImageAdmissionError::EntryPointOutsideExecutableRegion);
    }
    Ok(())
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    #[kani::unwind(3)]
    fn accepted_entry_is_bounded_and_executable() {
        let region = ImageRegion {
            start: kani::any(),
            len: kani::any(),
            flags: kani::any(),
        };
        let entry = kani::any();
        if admit_image(entry, &[region], 0x1000, 0x9000, false).is_ok() {
            let end = region.start.checked_add(region.len).unwrap();
            assert!(region.start >= 0x1000);
            assert!(end <= 0x9000);
            assert!(region.flags & IMAGE_REGION_EXECUTE != 0);
            assert!(region.flags & IMAGE_REGION_WRITE == 0);
            assert!((region.start..end).contains(&entry));
        }
    }

    #[kani::proof]
    fn accepted_elf_segment_is_bounded_and_wx_exclusive() {
        let ph: [u8; ELF64_PROGRAM_HEADER_SIZE] = kani::any();
        if let Ok(region) = admit_elf64_load_segment(&ph, 0x2000, 0x1000, 0x20_000) {
            let end = region.start.checked_add(region.len).unwrap();
            assert!(region.start >= 0x1000);
            assert!(end <= 0x20_000);
            assert!(
                region.flags & (IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE)
                    != (IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE)
            );
        }
    }

    #[kani::proof]
    fn rebased_pe_without_relocations_is_rejected() {
        let mut image: [u8; 8] = kani::any();
        assert_eq!(
            apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, 0, 0, 0),
            Err(ByteAdmissionError::MissingRelocations)
        );
    }

    #[kani::proof]
    #[kani::unwind(3)]
    fn accepted_pe_relocation_entry_has_a_bounded_exact_effect() {
        let old_value: u64 = kani::any();
        let relocation_kind: u8 = kani::any();
        let entry = u16::from(relocation_kind) << 12;
        let mut image = [0_u8; 48];
        image[0..8].copy_from_slice(&old_value.to_le_bytes());
        image[32..36].copy_from_slice(&0_u32.to_le_bytes());
        image[36..40].copy_from_slice(&10_u32.to_le_bytes());
        image[40..42].copy_from_slice(&entry.to_le_bytes());

        if let Ok(patched) = apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, 32, 10, 0) {
            let decoded_kind = relocation_kind & 0x0f;
            assert!(
                decoded_kind == PE64_RELOC_ABSOLUTE as u8 || decoded_kind == PE64_RELOC_DIR64 as u8
            );
            if decoded_kind == PE64_RELOC_DIR64 as u8 {
                assert_eq!(patched, 1);
                assert_eq!(read_u64(&image, 0), Ok(old_value + 0x1000));
            } else {
                assert_eq!(patched, 0);
                assert_eq!(read_u64(&image, 0), Ok(old_value));
            }
        }
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn accepted_pe_import_thunk_has_valid_identity_and_bound() {
        let entry: u64 = kani::any();
        kani::assume(entry != 0);
        let mut image = [0_u8; 96];
        image[8..12].copy_from_slice(&48_u32.to_le_bytes());
        image[20..24].copy_from_slice(&72_u32.to_le_bytes());
        image[24..28].copy_from_slice(&48_u32.to_le_bytes());
        image[48..56].copy_from_slice(&entry.to_le_bytes());
        image[72..74].copy_from_slice(b"d\0");
        image[82..84].copy_from_slice(b"f\0");

        if let Ok(summary) = validate_pe64_import_table(&image, 8, 40, 1) {
            assert_eq!(summary.descriptors, 1);
            assert_eq!(summary.imports, 1);
            if entry & PE64_ORDINAL_FLAG != 0 {
                assert_eq!(entry & PE64_ORDINAL_RESERVED_MASK, 0);
                assert_ne!(entry & 0xffff, 0);
            } else {
                assert_eq!(entry & PE64_NAME_RESERVED_MASK, 0);
                assert!(u32::try_from(entry).is_ok());
                let string_rva = (entry as u32).checked_add(2).unwrap();
                assert!((string_rva as usize) < image.len());
                assert!(validate_ascii_c_string(&image, string_rva, false).is_ok());
            }
        }
    }

    #[kani::proof]
    fn little_endian_byte_fields_are_decoded_exactly() {
        let bytes: [u8; 8] = kani::any();
        assert_eq!(
            read_u16(&bytes, 0),
            Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
        );
        assert_eq!(
            read_u32(&bytes, 0),
            Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        );
        assert_eq!(read_u64(&bytes, 0), Ok(u64::from_le_bytes(bytes)));
    }

    #[kani::proof]
    fn pe_section_u32_fields_are_decoded_exactly() {
        let virtual_size: u32 = kani::any();
        let virtual_address: u32 = kani::any();
        let raw_size: u32 = kani::any();
        let raw_offset: u32 = kani::any();
        let characteristics: u32 = kani::any();
        let mut section = [0_u8; PE64_SECTION_HEADER_SIZE];
        section[8..12].copy_from_slice(&virtual_size.to_le_bytes());
        section[12..16].copy_from_slice(&virtual_address.to_le_bytes());
        section[16..20].copy_from_slice(&raw_size.to_le_bytes());
        section[20..24].copy_from_slice(&raw_offset.to_le_bytes());
        section[36..40].copy_from_slice(&characteristics.to_le_bytes());

        assert_eq!(read_u32(&section, 8), Ok(virtual_size));
        assert_eq!(read_u32(&section, 12), Ok(virtual_address));
        assert_eq!(read_u32(&section, 16), Ok(raw_size));
        assert_eq!(read_u32(&section, 20), Ok(raw_offset));
        assert_eq!(read_u32(&section, 36), Ok(characteristics));
    }

    #[kani::proof]
    fn accepted_pe_section_is_bounded_and_wx_exclusive() {
        let section: [u8; PE64_SECTION_HEADER_SIZE] = kani::any();
        if let Ok((summary, Some(region))) =
            admit_pe64_section(&section, 0x400000, 0x1000, 0x1000, 0x200, 0x20_000)
        {
            assert_ne!(
                summary.characteristics & (PE64_SCN_WRITE | PE64_SCN_EXECUTE),
                PE64_SCN_WRITE | PE64_SCN_EXECUTE
            );
            assert_ne!(
                region.flags & (IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE),
                IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE
            );
            assert!(region.start >= 0x401000);
            assert!(region.start.checked_add(region.len).unwrap() <= 0x420000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 0x1000;
    const END: u64 = 0x9000;

    fn rx(start: u64, len: u64) -> ImageRegion {
        ImageRegion {
            start,
            len,
            flags: IMAGE_REGION_READ | IMAGE_REGION_EXECUTE,
        }
    }

    #[test]
    fn accepts_entry_in_bounded_executable_region() {
        assert_eq!(
            admit_image(0x2100, &[rx(0x2000, 0x1000)], START, END, false),
            Ok(())
        );
    }

    #[test]
    fn rejects_entry_in_non_executable_region() {
        let regions = [
            rx(0x2000, 0x1000),
            ImageRegion {
                start: 0x4000,
                len: 0x1000,
                flags: IMAGE_REGION_READ | IMAGE_REGION_WRITE,
            },
        ];
        assert_eq!(
            admit_image(0x4100, &regions, START, END, false),
            Err(ImageAdmissionError::EntryPointOutsideExecutableRegion)
        );
    }

    #[test]
    fn rejects_writable_executable_region() {
        let region = ImageRegion {
            start: 0x2000,
            len: 0x1000,
            flags: IMAGE_REGION_WRITE | IMAGE_REGION_EXECUTE,
        };
        assert_eq!(
            admit_image(0x2100, &[region], START, END, false),
            Err(ImageAdmissionError::WritableExecutableRegion)
        );
    }

    #[test]
    fn rejects_overlapping_regions() {
        let regions = [rx(0x2000, 0x1800), rx(0x3000, 0x1000)];
        assert_eq!(
            admit_image(0x2100, &regions, START, END, false),
            Err(ImageAdmissionError::OverlappingRegions)
        );
    }

    #[test]
    fn rejects_out_of_range_and_overflowing_regions() {
        assert_eq!(
            admit_image(0x2000, &[rx(0x0800, 0x1000)], START, END, false),
            Err(ImageAdmissionError::AddressOutOfRange)
        );
        assert_eq!(
            admit_image(u64::MAX, &[rx(u64::MAX - 1, 4)], START, u64::MAX, false),
            Err(ImageAdmissionError::AddressOverflow)
        );
    }

    #[test]
    fn zero_entry_is_only_valid_for_entryless_library() {
        let data = ImageRegion {
            start: 0x2000,
            len: 0x1000,
            flags: IMAGE_REGION_READ,
        };
        assert_eq!(admit_image(0, &[data], START, END, true), Ok(()));
        assert_eq!(
            admit_image(0, &[data], START, END, false),
            Err(ImageAdmissionError::MissingEntryPoint)
        );
    }

    fn elf_load_header(flags: u32) -> [u8; ELF64_PROGRAM_HEADER_SIZE] {
        let mut ph = [0_u8; ELF64_PROGRAM_HEADER_SIZE];
        ph[0..4].copy_from_slice(&ELF64_PT_LOAD.to_le_bytes());
        ph[4..8].copy_from_slice(&flags.to_le_bytes());
        ph[8..16].copy_from_slice(&0x1000_u64.to_le_bytes());
        ph[16..24].copy_from_slice(&0x2000_u64.to_le_bytes());
        ph[32..40].copy_from_slice(&0x800_u64.to_le_bytes());
        ph[40..48].copy_from_slice(&0x1000_u64.to_le_bytes());
        ph[48..56].copy_from_slice(&0x1000_u64.to_le_bytes());
        ph
    }

    #[test]
    fn raw_elf_load_segment_enforces_file_alignment_bounds_and_wx() {
        let ph = elf_load_header(ELF64_PF_R | ELF64_PF_X);
        assert_eq!(
            admit_elf64_load_segment(&ph, 0x1000, 0x1000, 0x10_000),
            Ok(ImageRegion {
                start: 0x3000,
                len: 0x1000,
                flags: IMAGE_REGION_READ | IMAGE_REGION_EXECUTE,
            })
        );

        let wx = elf_load_header(ELF64_PF_W | ELF64_PF_X);
        assert_eq!(
            admit_elf64_load_segment(&wx, 0x1000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::WritableExecutableRegion)
        );

        let mut truncated_in_file = ph;
        truncated_in_file[32..40].copy_from_slice(&0x2000_u64.to_le_bytes());
        assert_eq!(
            admit_elf64_load_segment(&truncated_in_file, 0x1000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::InvalidValue)
        );

        let mut misaligned = ph;
        misaligned[8..16].copy_from_slice(&0x1001_u64.to_le_bytes());
        assert_eq!(
            admit_elf64_load_segment(&misaligned, 0x1000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::InvalidValue)
        );
    }

    fn elf64_image_bytes() -> ([u8; ELF64_HEADER_SIZE], [u8; ELF64_PROGRAM_HEADER_SIZE]) {
        let mut header = [0_u8; ELF64_HEADER_SIZE];
        header[0..4].copy_from_slice(b"\x7fELF");
        header[4] = 2;
        header[5] = 1;
        header[6] = 1;
        header[16..18].copy_from_slice(&ELF64_ET_EXEC.to_le_bytes());
        header[18..20].copy_from_slice(&ELF64_EM_X86_64.to_le_bytes());
        header[20..24].copy_from_slice(&1_u32.to_le_bytes());
        header[24..32].copy_from_slice(&0x2100_u64.to_le_bytes());
        header[32..40].copy_from_slice(&(ELF64_HEADER_SIZE as u64).to_le_bytes());
        header[52..54].copy_from_slice(&(ELF64_HEADER_SIZE as u16).to_le_bytes());
        header[54..56].copy_from_slice(&(ELF64_PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        header[56..58].copy_from_slice(&1_u16.to_le_bytes());

        let mut ph = [0_u8; ELF64_PROGRAM_HEADER_SIZE];
        ph[0..4].copy_from_slice(&ELF64_PT_LOAD.to_le_bytes());
        ph[4..8].copy_from_slice(&(ELF64_PF_R | ELF64_PF_X).to_le_bytes());
        ph[16..24].copy_from_slice(&0x2000_u64.to_le_bytes());
        ph[32..40].copy_from_slice(&0x100_u64.to_le_bytes());
        ph[40..48].copy_from_slice(&0x1000_u64.to_le_bytes());
        ph[48..56].copy_from_slice(&0x1000_u64.to_le_bytes());
        (header, ph)
    }

    #[test]
    fn complete_elf64_header_and_program_table_share_the_admission_gate() {
        let (header, ph) = elf64_image_bytes();
        assert_eq!(
            admit_elf64_image(&header, &ph, 0x4000, 0x1000, 0x10_000),
            Ok(Elf64ImageSummary {
                load_bias: 0,
                entry: 0x2100,
                program_headers: 1,
                load_regions: 1,
                has_interpreter: false,
            })
        );

        let mut bad_magic = header;
        bad_magic[0] = 0;
        assert_eq!(
            admit_elf64_image(&bad_magic, &ph, 0x4000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::InvalidValue)
        );
        assert_eq!(
            admit_elf64_image(&header, &ph[..55], 0x4000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::Truncated)
        );

        let mut outside_entry = header;
        outside_entry[24..32].copy_from_slice(&0x4000_u64.to_le_bytes());
        assert_eq!(
            admit_elf64_image(&outside_entry, &ph, 0x4000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::EntryPointOutsideExecutableRegion)
        );
    }

    #[test]
    fn elf64_rejects_distinct_segments_that_alias_one_page() {
        let (mut header, mut first) = elf64_image_bytes();
        header[56..58].copy_from_slice(&2_u16.to_le_bytes());
        first[40..48].copy_from_slice(&0x1800_u64.to_le_bytes());
        let mut second = elf_load_header(ELF64_PF_R | ELF64_PF_W);
        second[8..16].copy_from_slice(&0x1800_u64.to_le_bytes());
        second[16..24].copy_from_slice(&0x3800_u64.to_le_bytes());
        second[32..40].copy_from_slice(&0x100_u64.to_le_bytes());
        second[40..48].copy_from_slice(&0x800_u64.to_le_bytes());
        let mut table = [0_u8; ELF64_PROGRAM_HEADER_SIZE * 2];
        table[..ELF64_PROGRAM_HEADER_SIZE].copy_from_slice(&first);
        table[ELF64_PROGRAM_HEADER_SIZE..].copy_from_slice(&second);

        assert_eq!(
            admit_elf64_image(&header, &table, 0x4000, 0x1000, 0x10_000),
            Err(ByteAdmissionError::OverlappingRegions)
        );
    }

    fn pe64_header_bytes() -> (
        [u8; PE64_DOS_HEADER_SIZE],
        [u8; PE64_FILE_HEADER_SIZE],
        [u8; 240],
        [u8; PE64_SECTION_HEADER_SIZE],
    ) {
        let mut dos = [0_u8; PE64_DOS_HEADER_SIZE];
        dos[0..2].copy_from_slice(b"MZ");
        dos[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());

        let mut file = [0_u8; PE64_FILE_HEADER_SIZE];
        file[0..4].copy_from_slice(b"PE\0\0");
        file[4..6].copy_from_slice(&PE64_MACHINE_AMD64.to_le_bytes());
        file[6..8].copy_from_slice(&1_u16.to_le_bytes());
        file[20..22].copy_from_slice(&(240_u16).to_le_bytes());

        let mut optional = [0_u8; 240];
        optional[0..2].copy_from_slice(&PE64_OPTIONAL_MAGIC.to_le_bytes());
        optional[16..20].copy_from_slice(&0x1000_u32.to_le_bytes());
        optional[24..32].copy_from_slice(&0x400000_u64.to_le_bytes());
        optional[32..36].copy_from_slice(&0x1000_u32.to_le_bytes());
        optional[36..40].copy_from_slice(&0x200_u32.to_le_bytes());
        optional[56..60].copy_from_slice(&0x2000_u32.to_le_bytes());
        optional[60..64].copy_from_slice(&0x200_u32.to_le_bytes());
        optional[108..112].copy_from_slice(&16_u32.to_le_bytes());

        let mut section = [0_u8; PE64_SECTION_HEADER_SIZE];
        section[0..5].copy_from_slice(b".text");
        section[8..12].copy_from_slice(&0x1000_u32.to_le_bytes());
        section[12..16].copy_from_slice(&0x1000_u32.to_le_bytes());
        section[16..20].copy_from_slice(&0x200_u32.to_le_bytes());
        section[20..24].copy_from_slice(&0x200_u32.to_le_bytes());
        section[36..40].copy_from_slice(&(PE64_SCN_READ | PE64_SCN_EXECUTE).to_le_bytes());
        (dos, file, optional, section)
    }

    #[test]
    fn complete_pe64_headers_and_sections_share_the_admission_gate() {
        let (dos, file, optional, section) = pe64_header_bytes();
        let admitted = admit_pe64_image_headers(
            &dos,
            &file,
            &optional,
            &section,
            0x400000,
            0x1000,
            0x800000,
            128 * 1024 * 1024,
            false,
        )
        .unwrap();
        assert_eq!(admitted.entry_point, 0x401000);
        assert_eq!(admitted.section_count, 1);
        assert_eq!(admitted.sections[0].virtual_address, 0x1000);

        let mut bad_directory_pair = optional;
        bad_directory_pair[112..116].copy_from_slice(&0x1000_u32.to_le_bytes());
        assert_eq!(
            admit_pe64_image_headers(
                &dos,
                &file,
                &bad_directory_pair,
                &section,
                0x400000,
                0x1000,
                0x800000,
                128 * 1024 * 1024,
                false,
            ),
            Err(ByteAdmissionError::InvalidValue)
        );

        let mut writable_executable = section;
        writable_executable[36..40]
            .copy_from_slice(&(PE64_SCN_READ | PE64_SCN_WRITE | PE64_SCN_EXECUTE).to_le_bytes());
        assert_eq!(
            admit_pe64_image_headers(
                &dos,
                &file,
                &optional,
                &writable_executable,
                0x400000,
                0x1000,
                0x800000,
                128 * 1024 * 1024,
                false,
            ),
            Err(ByteAdmissionError::InvalidValue)
        );
    }

    #[test]
    fn pe64_relocation_requires_a_table_when_the_image_is_rebased() {
        let mut image = [0_u8; 256];
        assert_eq!(
            apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, 0, 0, 0),
            Err(ByteAdmissionError::MissingRelocations)
        );
        assert_eq!(
            apply_pe64_base_relocations(
                &mut image,
                0x1000,
                0x2000,
                0,
                0,
                PE64_FILE_RELOCS_STRIPPED,
            ),
            Err(ByteAdmissionError::RelocationsStripped)
        );
    }

    #[test]
    fn pe64_dir64_relocation_is_bounded_and_exact() {
        let mut image = [0_u8; 256];
        image[0x20..0x24].copy_from_slice(&0_u32.to_le_bytes());
        image[0x24..0x28].copy_from_slice(&10_u32.to_le_bytes());
        image[0x28..0x2a].copy_from_slice(&((PE64_RELOC_DIR64 << 12) | 0x80).to_le_bytes());
        image[0x80..0x88].copy_from_slice(&0x3000_u64.to_le_bytes());

        assert_eq!(
            apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, 0x20, 10, 0),
            Ok(1)
        );
        assert_eq!(
            u64::from_le_bytes(image[0x80..0x88].try_into().unwrap()),
            0x4000
        );

        image[0x28..0x2a].copy_from_slice(&(3_u16 << 12).to_le_bytes());
        assert_eq!(
            apply_pe64_base_relocations(&mut image, 0x1000, 0x2000, 0x20, 10, 0),
            Err(ByteAdmissionError::UnsupportedRelocation)
        );
    }

    fn valid_import_image() -> [u8; 256] {
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
    fn pe64_import_table_requires_bounded_ascii_and_a_null_descriptor() {
        let image = valid_import_image();
        assert_eq!(
            validate_pe64_import_table(&image, 0x20, 40, 8),
            Ok(Pe64ImportSummary {
                descriptors: 1,
                imports: 1,
            })
        );
        assert_eq!(
            validate_pe64_import_table(&image, 0x20, 20, 8),
            Err(ByteAdmissionError::MissingTerminator)
        );
        assert_eq!(
            validate_pe64_import_table(&image, 0x20, 40, 0),
            Err(ByteAdmissionError::TooManyImports)
        );
    }

    #[test]
    fn pe64_import_thunks_reject_reserved_bits_and_unbounded_names() {
        let mut image = valid_import_image();
        image[0xa0..0xa8].copy_from_slice(&(1_u64 << 40).to_le_bytes());
        assert_eq!(
            validate_pe64_import_table(&image, 0x20, 40, 8),
            Err(ByteAdmissionError::InvalidValue)
        );

        let mut ordinal = valid_import_image();
        ordinal[0xa0..0xa8].copy_from_slice(&(PE64_ORDINAL_FLAG | (1_u64 << 20) | 7).to_le_bytes());
        assert_eq!(
            validate_pe64_import_table(&ordinal, 0x20, 40, 8),
            Err(ByteAdmissionError::InvalidValue)
        );
    }
}
