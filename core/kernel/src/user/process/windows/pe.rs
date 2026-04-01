use alloc::vec::Vec;

use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::memory::paging::{self, ProcessAddressSpace};

use super::super::{align_down, align_up, page_ranges_overlap, ProcessLoadError, PAGE_SIZE};

const PE_DEFAULT_LOAD_BASE: u64 = paging::USER_SPACE_BASE + 0x0040_0000;
const PE_MACHINE_AMD64: u16 = 0x8664;
const PE_MAGIC_PE32_PLUS: u16 = 0x20b;
pub(super) const PE_DIRECTORY_IMPORT: usize = 1;
pub(super) const PE_DIRECTORY_EXPORT: usize = 0;
pub(super) const PE_DIRECTORY_BASERELOC: usize = 5;
const PE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
const PE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
const PE_FILE_DLL: u16 = 0x2000;
const PE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const PE_SCN_MEM_READ: u32 = 0x4000_0000;
const PE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const PE_REL_BASED_ABSOLUTE: u16 = 0;
const PE_REL_BASED_DIR64: u16 = 10;

#[derive(Clone, Copy, Debug)]
pub(super) struct PeSection {
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PeDataDirectory {
    pub rva: u32,
    pub size: u32,
}

#[derive(Clone, Debug)]
pub(super) struct PeImage {
    pub entry_rva: u32,
    pub preferred_base: u64,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub relocs_stripped: bool,
    pub is_dll: bool,
    pub directories: [PeDataDirectory; 16],
    pub sections: Vec<PeSection>,
}

pub(super) fn parse_pe_image(image: &[u8]) -> Result<PeImage, ProcessLoadError> {
    if image.len() < 0x40 {
        return Err(ProcessLoadError::InvalidPe("PE image is too small"));
    }

    let pe_offset = read_u32(image, 0x3c)? as usize;
    let file_header_offset = pe_offset
        .checked_add(4)
        .ok_or(ProcessLoadError::InvalidPe("PE header offset overflow"))?;
    if read_bytes(image, pe_offset, 4)? != b"PE\0\0" {
        return Err(ProcessLoadError::InvalidPe("missing PE signature"));
    }

    let machine = read_u16(image, file_header_offset)?;
    if machine != PE_MACHINE_AMD64 {
        return Err(ProcessLoadError::InvalidPe("PE machine is not x86_64"));
    }

    let section_count = read_u16(image, file_header_offset + 2)? as usize;
    if section_count == 0 {
        return Err(ProcessLoadError::InvalidPe("PE has no sections"));
    }

    let characteristics = read_u16(image, file_header_offset + 18)?;
    if characteristics & PE_FILE_EXECUTABLE_IMAGE == 0 {
        return Err(ProcessLoadError::InvalidPe("PE is not executable"));
    }

    let optional_header_size = read_u16(image, file_header_offset + 16)? as usize;
    let optional_header_offset = file_header_offset + 20;
    let optional_header = read_bytes(image, optional_header_offset, optional_header_size)?;
    if read_u16(optional_header, 0)? != PE_MAGIC_PE32_PLUS {
        return Err(ProcessLoadError::InvalidPe(
            "PE optional header is not PE32+",
        ));
    }

    let entry_rva = read_u32(optional_header, 16)?;
    let preferred_base = read_u64(optional_header, 24)?;
    let section_alignment = read_u32(optional_header, 32)?;
    let file_alignment = read_u32(optional_header, 36)?;
    let size_of_image = read_u32(optional_header, 56)?;
    let size_of_headers = read_u32(optional_header, 60)?;
    let number_of_rva_and_sizes = read_u32(optional_header, 108)? as usize;

    if section_alignment < PAGE_SIZE as u32 || !section_alignment.is_power_of_two() {
        return Err(ProcessLoadError::InvalidPe(
            "PE section alignment must be page-sized and a power of two",
        ));
    }
    if file_alignment == 0 || !file_alignment.is_power_of_two() {
        return Err(ProcessLoadError::InvalidPe(
            "PE file alignment must be a power of two",
        ));
    }
    if size_of_image == 0 || size_of_headers == 0 {
        return Err(ProcessLoadError::InvalidPe("PE image has invalid size"));
    }
    if size_of_headers > size_of_image {
        return Err(ProcessLoadError::InvalidPe("PE headers exceed image size"));
    }

    let mut directories = [PeDataDirectory { rva: 0, size: 0 }; 16];
    let directory_count = number_of_rva_and_sizes.min(directories.len());
    for (index, entry) in directories.iter_mut().enumerate().take(directory_count) {
        let base = 112 + index * 8;
        if base + 8 > optional_header.len() {
            return Err(ProcessLoadError::InvalidPe(
                "PE data directories are truncated",
            ));
        }
        *entry = PeDataDirectory {
            rva: read_u32(optional_header, base)?,
            size: read_u32(optional_header, base + 4)?,
        };
    }

    let section_table_offset = optional_header_offset
        .checked_add(optional_header_size)
        .ok_or(ProcessLoadError::InvalidPe(
            "PE section table offset overflow",
        ))?;
    let mut sections = Vec::with_capacity(section_count);
    for index in 0..section_count {
        let offset = section_table_offset
            .checked_add(index * 40)
            .ok_or(ProcessLoadError::InvalidPe("PE section table overflow"))?;
        let _name = read_bytes(image, offset, 8)?;
        let virtual_size = read_u32(image, offset + 8)?;
        let virtual_address = read_u32(image, offset + 12)?;
        let raw_size = read_u32(image, offset + 16)?;
        let raw_offset = read_u32(image, offset + 20)?;
        let characteristics = read_u32(image, offset + 36)?;

        if virtual_size == 0 && raw_size == 0 {
            continue;
        }
        if virtual_address == 0 || virtual_address % section_alignment != 0 {
            return Err(ProcessLoadError::InvalidPe(
                "PE section virtual address is invalid",
            ));
        }
        if raw_size != 0 {
            let raw_end = raw_offset
                .checked_add(raw_size)
                .ok_or(ProcessLoadError::InvalidPe("PE section raw range overflow"))?;
            if raw_end as usize > image.len() {
                return Err(ProcessLoadError::InvalidPe(
                    "PE section raw range is outside the image",
                ));
            }
        }
        sections.push(PeSection {
            virtual_address,
            virtual_size,
            raw_offset,
            raw_size,
            characteristics,
        });
    }

    if sections.is_empty() {
        return Err(ProcessLoadError::InvalidPe("PE has no loadable sections"));
    }

    Ok(PeImage {
        entry_rva,
        preferred_base,
        size_of_image,
        size_of_headers,
        relocs_stripped: characteristics & PE_FILE_RELOCS_STRIPPED != 0,
        is_dll: characteristics & PE_FILE_DLL != 0,
        directories,
        sections,
    })
}

pub(super) fn choose_pe_load_base(pe: &PeImage) -> Result<u64, ProcessLoadError> {
    let size = align_up(pe.size_of_image as u64, PAGE_SIZE).ok_or(ProcessLoadError::InvalidPe(
        "PE image size alignment overflow",
    ))?;

    if is_range_within_user_space(pe.preferred_base, size) {
        return Ok(pe.preferred_base);
    }
    if is_range_within_user_space(PE_DEFAULT_LOAD_BASE, size) {
        return Ok(PE_DEFAULT_LOAD_BASE);
    }

    Err(ProcessLoadError::InvalidPe(
        "PE image does not fit in the supported user range",
    ))
}

pub(super) fn validate_pe_entry_point(pe: &PeImage) -> Result<(), ProcessLoadError> {
    if pe.entry_rva == 0 {
        return Err(ProcessLoadError::InvalidPe("PE entry point is missing"));
    }

    for section in &pe.sections {
        let span = section.virtual_size.max(section.raw_size);
        let Some(section_end) = section.virtual_address.checked_add(span) else {
            return Err(ProcessLoadError::InvalidPe("PE section range overflow"));
        };
        if pe.entry_rva >= section.virtual_address && pe.entry_rva < section_end {
            if (section.characteristics & PE_SCN_MEM_EXECUTE) == 0 {
                return Err(ProcessLoadError::InvalidPe(
                    "PE entry point is not inside an executable section",
                ));
            }
            return Ok(());
        }
    }

    Err(ProcessLoadError::InvalidPe(
        "PE entry point does not fall inside a section",
    ))
}

pub(super) fn map_pe_headers(
    image: &[u8],
    pe: &PeImage,
    address_space: &mut ProcessAddressSpace,
    load_base: u64,
    mapped_ranges: &mut Vec<(u64, u64)>,
) -> Result<(), ProcessLoadError> {
    let header_len = pe.size_of_headers.min(image.len() as u32) as usize;
    let page_base = load_base;
    let page_end = align_up(
        load_base
            .checked_add(header_len as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE header mapping overflow"))?,
        PAGE_SIZE,
    )
    .ok_or(ProcessLoadError::InvalidPe(
        "PE header page alignment overflow",
    ))?;
    if page_ranges_overlap(page_base, page_end, mapped_ranges) {
        return Err(ProcessLoadError::InvalidPe(
            "load image page ranges overlap",
        ));
    }

    let page_count = ((page_end - page_base) / PAGE_SIZE) as usize;
    address_space.map_zeroed_user_pages_at(
        VirtAddr::new(page_base),
        page_count,
        PageTableFlags::NO_EXECUTE,
    )?;
    address_space.initialize_user_bytes(VirtAddr::new(load_base), &image[..header_len])?;
    mapped_ranges.push((page_base, page_end));
    Ok(())
}

pub(super) fn map_pe_sections(
    image: &[u8],
    pe: &PeImage,
    address_space: &mut ProcessAddressSpace,
    load_base: u64,
    mapped_ranges: &mut Vec<(u64, u64)>,
) -> Result<(), ProcessLoadError> {
    for section in &pe.sections {
        let section_size = section.virtual_size.max(section.raw_size);
        let start = load_base
            .checked_add(section.virtual_address as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE section base overflow"))?;
        let page_base = align_down(start, PAGE_SIZE);
        let page_end = align_up(
            start
                .checked_add(section_size as u64)
                .ok_or(ProcessLoadError::InvalidPe("PE section end overflow"))?,
            PAGE_SIZE,
        )
        .ok_or(ProcessLoadError::InvalidPe(
            "PE section page alignment overflow",
        ))?;
        if page_ranges_overlap(page_base, page_end, mapped_ranges) {
            return Err(ProcessLoadError::InvalidPe(
                "load image page ranges overlap",
            ));
        }

        let page_count = ((page_end - page_base) / PAGE_SIZE) as usize;
        address_space.map_zeroed_user_pages_at(
            VirtAddr::new(page_base),
            page_count,
            pe_section_page_flags(*section)?,
        )?;

        if section.raw_size != 0 {
            let raw_offset = section.raw_offset as usize;
            let raw_end = raw_offset + section.raw_size as usize;
            address_space
                .initialize_user_bytes(VirtAddr::new(start), &image[raw_offset..raw_end])?;
        }

        mapped_ranges.push((page_base, page_end));
    }

    Ok(())
}

pub(super) fn apply_pe_relocations(
    image: &[u8],
    pe: &PeImage,
    address_space: &ProcessAddressSpace,
    load_base: u64,
) -> Result<(), ProcessLoadError> {
    let delta = load_base.wrapping_sub(pe.preferred_base);
    if delta == 0 {
        return Ok(());
    }

    let reloc_dir = pe.directories[PE_DIRECTORY_BASERELOC];
    if reloc_dir.rva == 0 || reloc_dir.size == 0 {
        if pe.relocs_stripped {
            return Err(ProcessLoadError::InvalidPe(
                "PE image requires relocation but has no base relocations",
            ));
        }
        return Ok(());
    }

    let reloc_offset = rva_to_file_offset(pe, reloc_dir.rva, image.len() as u32)?;
    let reloc_end =
        reloc_offset
            .checked_add(reloc_dir.size as usize)
            .ok_or(ProcessLoadError::InvalidPe(
                "PE relocation directory overflow",
            ))?;
    if reloc_end > image.len() {
        return Err(ProcessLoadError::InvalidPe(
            "PE relocation directory is truncated",
        ));
    }

    let mut cursor = reloc_offset;
    while cursor < reloc_end {
        let block_rva = read_u32(image, cursor)?;
        let block_size = read_u32(image, cursor + 4)? as usize;
        if block_size < 8 || cursor + block_size > reloc_end {
            return Err(ProcessLoadError::InvalidPe("invalid PE relocation block"));
        }

        let entry_count = (block_size - 8) / 2;
        let entries_offset = cursor + 8;
        for index in 0..entry_count {
            let entry = read_u16(image, entries_offset + index * 2)?;
            let reloc_type = entry >> 12;
            let offset = entry & 0x0fff;
            if reloc_type == PE_REL_BASED_ABSOLUTE {
                continue;
            }
            if reloc_type != PE_REL_BASED_DIR64 {
                return Err(ProcessLoadError::InvalidPe(
                    "unsupported PE relocation type",
                ));
            }

            let target_rva = block_rva
                .checked_add(offset as u32)
                .ok_or(ProcessLoadError::InvalidPe("PE relocation target overflow"))?;
            let target_addr =
                load_base
                    .checked_add(target_rva as u64)
                    .ok_or(ProcessLoadError::InvalidPe(
                        "PE relocation address overflow",
                    ))?;
            let value = read_user_u64(address_space, target_addr)?;
            let relocated = value.wrapping_add(delta);
            address_space
                .initialize_user_bytes(VirtAddr::new(target_addr), &relocated.to_le_bytes())?;
        }

        cursor += block_size;
    }

    Ok(())
}

pub(super) fn read_import_name_at_rva<'a>(
    image: &'a [u8],
    pe: &PeImage,
    hint_name_rva: u32,
) -> Result<&'a [u8], ProcessLoadError> {
    let string_offset = rva_to_file_offset(pe, hint_name_rva, image.len() as u32)?
        .checked_add(2)
        .ok_or(ProcessLoadError::InvalidPe("PE import name overflow"))?;
    read_c_string(image, string_offset)
}

pub(super) fn read_c_string_at_rva<'a>(
    image: &'a [u8],
    pe: &PeImage,
    rva: u32,
) -> Result<&'a [u8], ProcessLoadError> {
    let offset = rva_to_file_offset(pe, rva, image.len() as u32)?;
    read_c_string(image, offset)
}

pub(super) fn read_c_string(image: &[u8], offset: usize) -> Result<&[u8], ProcessLoadError> {
    let bytes = image
        .get(offset..)
        .ok_or(ProcessLoadError::InvalidPe("PE string is truncated"))?;
    let Some(end) = bytes.iter().position(|&byte| byte == 0) else {
        return Err(ProcessLoadError::InvalidPe("PE string is not terminated"));
    };
    Ok(&bytes[..end])
}

pub(super) fn rva_to_file_offset(
    pe: &PeImage,
    rva: u32,
    image_len: u32,
) -> Result<usize, ProcessLoadError> {
    if rva < pe.size_of_headers {
        return usize::try_from(rva)
            .map_err(|_| ProcessLoadError::InvalidPe("PE header offset out of range"));
    }

    for section in &pe.sections {
        let span = section.virtual_size.max(section.raw_size);
        if rva >= section.virtual_address && rva < section.virtual_address.saturating_add(span) {
            let within = rva - section.virtual_address;
            if within >= section.raw_size {
                return Err(ProcessLoadError::InvalidPe(
                    "PE RVA points into zero-filled section data",
                ));
            }
            let offset = section
                .raw_offset
                .checked_add(within)
                .ok_or(ProcessLoadError::InvalidPe("PE RVA offset overflow"))?;
            if offset >= image_len {
                return Err(ProcessLoadError::InvalidPe("PE RVA is outside the image"));
            }
            return usize::try_from(offset)
                .map_err(|_| ProcessLoadError::InvalidPe("PE RVA offset out of range"));
        }
    }

    Err(ProcessLoadError::InvalidPe(
        "PE RVA does not map to file data",
    ))
}

pub(super) fn read_bytes<'a>(
    bytes: &'a [u8],
    offset: usize,
    len: usize,
) -> Result<&'a [u8], ProcessLoadError> {
    let end = offset
        .checked_add(len)
        .ok_or(ProcessLoadError::InvalidPe("PE file offset overflow"))?;
    bytes
        .get(offset..end)
        .ok_or(ProcessLoadError::InvalidPe("PE file is truncated"))
}

pub(super) fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ProcessLoadError> {
    let raw = read_bytes(bytes, offset, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

pub(super) fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ProcessLoadError> {
    let raw = read_bytes(bytes, offset, 4)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

pub(super) fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ProcessLoadError> {
    let raw = read_bytes(bytes, offset, 8)?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

fn pe_section_page_flags(section: PeSection) -> Result<PageTableFlags, ProcessLoadError> {
    let executable = (section.characteristics & PE_SCN_MEM_EXECUTE) != 0;
    let writable = (section.characteristics & PE_SCN_MEM_WRITE) != 0;
    let readable = (section.characteristics & PE_SCN_MEM_READ) != 0;

    if writable && executable {
        return Err(ProcessLoadError::InvalidPe(
            "writable executable PE sections are not supported",
        ));
    }
    if !readable && !writable && !executable {
        return Err(ProcessLoadError::InvalidPe(
            "PE section has no access permissions",
        ));
    }

    let mut flags = PageTableFlags::empty();
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }
    if !executable {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    Ok(flags)
}

fn read_user_u64(address_space: &ProcessAddressSpace, addr: u64) -> Result<u64, ProcessLoadError> {
    let mut bytes = [0_u8; 8];
    address_space.copy_from_user(VirtAddr::new(addr), &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn is_range_within_user_space(start: u64, len: u64) -> bool {
    start >= paging::USER_SPACE_BASE
        && start
            .checked_add(len)
            .map(|end| end <= paging::USER_SPACE_END_EXCLUSIVE)
            .unwrap_or(false)
}

#[cfg(test)]
pub(super) fn missing_base_relocations_are_fatal(pe: &PeImage, load_base: u64) -> bool {
    load_base.wrapping_sub(pe.preferred_base) != 0 && pe.relocs_stripped
}

#[cfg(test)]
mod tests {
    use super::{missing_base_relocations_are_fatal, PeDataDirectory, PeImage};

    #[test]
    fn relocationless_pe_without_relocs_stripped_flag_is_accepted() {
        let pe = PeImage {
            entry_rva: 0x1000,
            preferred_base: 0x0040_0000,
            size_of_image: 0x2000,
            size_of_headers: 0x400,
            relocs_stripped: false,
            is_dll: false,
            directories: [PeDataDirectory { rva: 0, size: 0 }; 16],
            sections: alloc::vec::Vec::new(),
        };
        assert!(!missing_base_relocations_are_fatal(
            &pe,
            crate::memory::paging::USER_SPACE_BASE + 0x0040_0000,
        ));
    }

    #[test]
    fn relocationless_fixed_pe_is_rejected() {
        let pe = PeImage {
            entry_rva: 0x1000,
            preferred_base: 0x0040_0000,
            size_of_image: 0x2000,
            size_of_headers: 0x400,
            relocs_stripped: true,
            is_dll: false,
            directories: [PeDataDirectory { rva: 0, size: 0 }; 16],
            sections: alloc::vec::Vec::new(),
        };
        assert!(missing_base_relocations_are_fatal(
            &pe,
            crate::memory::paging::USER_SPACE_BASE + 0x0040_0000,
        ));
    }
}
