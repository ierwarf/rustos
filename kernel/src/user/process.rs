use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;
use xmas_elf::ElfFile;
use xmas_elf::header::{Class, Data, Machine, Type as ElfType};
use xmas_elf::program::{ProgramHeader, Type as ProgramType};

use crate::debug;
use crate::multitask;
use crate::paging::{self, AddressSpaceError, ProcessAddressSpace};
use crate::win32;

const PAGE_SIZE: u64 = 4096;
const MAX_LOAD_SEGMENTS: usize = 32;
const USER_STACK_GUARD_PAGES: usize = 1;
const USER_STACK_PAGES: usize = 8;
const USER_STACK_TOP_EXCLUSIVE: u64 = paging::USER_SPACE_END_EXCLUSIVE;
const PE_DEFAULT_LOAD_BASE: u64 = paging::USER_SPACE_BASE + 0x0040_0000;

const PE_MACHINE_AMD64: u16 = 0x8664;
const PE_MAGIC_PE32_PLUS: u16 = 0x20b;
const PE_DIRECTORY_IMPORT: usize = 1;
const PE_DIRECTORY_BASERELOC: usize = 5;
const PE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
const PE_FILE_DLL: u16 = 0x2000;
const PE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const PE_SCN_MEM_READ: u32 = 0x4000_0000;
const PE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const PE_REL_BASED_ABSOLUTE: u16 = 0;
const PE_REL_BASED_DIR64: u16 = 10;

#[derive(Debug)]
pub enum ProcessLoadError {
    InvalidElf(&'static str),
    InvalidPe(&'static str),
    AddressSpace(AddressSpaceError),
    UnsupportedImport {
        dll: [u8; 32],
        dll_len: usize,
        function: [u8; 64],
        function_len: usize,
    },
    Spawn(multitask::SpawnTaskError),
}

impl From<AddressSpaceError> for ProcessLoadError {
    fn from(value: AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<multitask::SpawnTaskError> for ProcessLoadError {
    fn from(value: multitask::SpawnTaskError) -> Self {
        Self::Spawn(value)
    }
}

impl ProcessLoadError {
    pub fn summary(&self) -> &'static str {
        match self {
            Self::InvalidElf(reason) => reason,
            Self::InvalidPe(reason) => reason,
            Self::AddressSpace(AddressSpaceError::ZeroSizedAllocation) => {
                "zero-sized user allocation"
            }
            Self::AddressSpace(AddressSpaceError::AddressOverflow) => {
                "user address calculation overflow"
            }
            Self::AddressSpace(AddressSpaceError::AddressOutOfRange) => {
                "user address outside supported range"
            }
            Self::AddressSpace(AddressSpaceError::AddressNotPageAligned) => {
                "user address is not page aligned"
            }
            Self::AddressSpace(AddressSpaceError::AlreadyMapped) => {
                "user page range is already mapped"
            }
            Self::AddressSpace(AddressSpaceError::NotMapped) => "user page range is not mapped",
            Self::AddressSpace(AddressSpaceError::ProtectionViolation) => {
                "user page access permissions are invalid"
            }
            Self::AddressSpace(AddressSpaceError::HugePageConflict) => {
                "user mapping conflicts with huge page"
            }
            Self::AddressSpace(AddressSpaceError::OutOfFrames) => {
                "process frame allocator is exhausted"
            }
            Self::UnsupportedImport { .. } => "PE import is not supported yet",
            Self::Spawn(err) => err.summary(),
        }
    }

    pub fn log_debug_details(&self) {
        let Self::UnsupportedImport {
            dll,
            dll_len,
            function,
            function_len,
        } = self
        else {
            return;
        };

        let dll_name = core::str::from_utf8(&dll[..*dll_len]).unwrap_or("<non-utf8>");
        let function_name =
            core::str::from_utf8(&function[..*function_len]).unwrap_or("<non-utf8>");
        debug::println!("unsupported PE import: {}!{}", dll_name, function_name,);
    }
}

pub struct LoadedProcessImage {
    pub address_space: ProcessAddressSpace,
    pub entry: VirtAddr,
}

pub struct SpawnedProcess {
    pub pid: u64,
    pub entry: VirtAddr,
}

pub fn load_elf(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    let elf = ElfFile::new(image).map_err(ProcessLoadError::InvalidElf)?;
    validate_elf_header(&elf)?;

    let entry = validate_entry_point(&elf)?;
    let mut address_space = ProcessAddressSpace::new()?;

    let mut loaded_segments = 0usize;
    let mut executable_entry_covered = false;
    let mut mapped_page_ranges = [(0_u64, 0_u64); MAX_LOAD_SEGMENTS];

    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type != ProgramType::Load {
            continue;
        }
        if ph.mem_size() == 0 && ph.file_size() == 0 {
            continue;
        }

        if loaded_segments >= MAX_LOAD_SEGMENTS {
            return Err(ProcessLoadError::InvalidElf("too many PT_LOAD segments"));
        }

        validate_segment_policy(&ph)?;
        let segment = validated_segment_bounds(image, &ph)?;
        if page_ranges_overlap(
            segment.page_base,
            segment.page_end,
            &mapped_page_ranges[..loaded_segments],
        ) {
            return Err(ProcessLoadError::InvalidElf("PT_LOAD page ranges overlap"));
        }
        mapped_page_ranges[loaded_segments] = (segment.page_base, segment.page_end);

        if ph.flags().is_execute() && (segment.addr..segment.end).contains(&entry.as_u64()) {
            executable_entry_covered = true;
        }

        let page_count = ((segment.page_end - segment.page_base) / PAGE_SIZE) as usize;
        let page_flags = segment_page_flags(&ph);
        address_space.map_zeroed_user_pages_at(
            VirtAddr::new(segment.page_base),
            page_count,
            page_flags,
        )?;
        address_space.initialize_user_bytes(
            VirtAddr::new(segment.addr),
            &image[segment.file_offset..segment.file_end],
        )?;
        loaded_segments += 1;
    }

    if loaded_segments == 0 {
        return Err(ProcessLoadError::InvalidElf(
            "ELF does not contain PT_LOAD segments",
        ));
    }
    if !executable_entry_covered {
        return Err(ProcessLoadError::InvalidElf(
            "entry point is not inside an executable PT_LOAD segment",
        ));
    }

    Ok(LoadedProcessImage {
        address_space,
        entry,
    })
}

pub fn load_image(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    if image.starts_with(b"\x7FELF") {
        return load_elf(image);
    }
    if image.starts_with(b"MZ") {
        return load_pe(image);
    }

    Err(ProcessLoadError::InvalidPe(
        "unknown executable image format",
    ))
}

#[derive(Clone, Copy)]
struct PeSection {
    virtual_address: u32,
    virtual_size: u32,
    raw_offset: u32,
    raw_size: u32,
    characteristics: u32,
}

#[derive(Clone, Copy)]
struct PeDataDirectory {
    rva: u32,
    size: u32,
}

struct PeImage {
    entry_rva: u32,
    preferred_base: u64,
    size_of_image: u32,
    size_of_headers: u32,
    directories: [PeDataDirectory; 16],
    sections: Vec<PeSection>,
}

#[derive(Clone, Copy)]
struct ResolvedImport {
    first_thunk_rva: u32,
    api: win32::Api,
}

fn load_pe(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    let pe = parse_pe_image(image)?;
    validate_pe_entry_point(&pe)?;
    let load_base = choose_pe_load_base(&pe)?;
    let entry = VirtAddr::new(
        load_base
            .checked_add(pe.entry_rva as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE entry point overflow"))?,
    );

    let mut address_space = ProcessAddressSpace::new()?;
    let mut mapped_ranges = Vec::with_capacity(pe.sections.len() + 2);

    map_pe_headers(
        image,
        &pe,
        &mut address_space,
        load_base,
        &mut mapped_ranges,
    )?;
    map_pe_sections(
        image,
        &pe,
        &mut address_space,
        load_base,
        &mut mapped_ranges,
    )?;
    apply_pe_relocations(image, &pe, &address_space, load_base)?;
    resolve_pe_imports(image, &pe, &mut address_space, load_base)?;

    Ok(LoadedProcessImage {
        address_space,
        entry,
    })
}

fn parse_pe_image(image: &[u8]) -> Result<PeImage, ProcessLoadError> {
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
    if characteristics & PE_FILE_DLL != 0 {
        return Err(ProcessLoadError::InvalidPe("DLL images are not supported"));
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
        directories,
        sections,
    })
}

fn choose_pe_load_base(pe: &PeImage) -> Result<u64, ProcessLoadError> {
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

fn validate_pe_entry_point(pe: &PeImage) -> Result<(), ProcessLoadError> {
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

fn map_pe_headers(
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

fn map_pe_sections(
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

fn apply_pe_relocations(
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
        return Err(ProcessLoadError::InvalidPe(
            "PE image requires relocation but has no base relocations",
        ));
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

fn resolve_pe_imports(
    image: &[u8],
    pe: &PeImage,
    address_space: &mut ProcessAddressSpace,
    load_base: u64,
) -> Result<(), ProcessLoadError> {
    let imports = collect_pe_imports(image, pe)?;
    if imports.is_empty() {
        return Ok(());
    }

    let thunk_bytes = imports
        .len()
        .checked_mul(win32::import_thunk_len())
        .ok_or(ProcessLoadError::InvalidPe("PE thunk table size overflow"))?;
    let thunk_pages = usize::try_from(
        align_up(thunk_bytes as u64, PAGE_SIZE).ok_or(ProcessLoadError::InvalidPe(
            "PE thunk page alignment overflow",
        ))? / PAGE_SIZE,
    )
    .map_err(|_| ProcessLoadError::InvalidPe("PE thunk page count overflow"))?;
    let thunk_base = align_up(
        load_base
            .checked_add(pe.size_of_image as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE thunk base overflow"))?,
        PAGE_SIZE,
    )
    .ok_or(ProcessLoadError::InvalidPe(
        "PE thunk base alignment overflow",
    ))?;

    ensure_unmapped_user_pages(
        address_space,
        VirtAddr::new(thunk_base),
        thunk_pages,
        "PE import thunk page address overflow",
        "PE import thunk pages overlap an existing mapping",
    )?;
    address_space.map_zeroed_user_pages_at(
        VirtAddr::new(thunk_base),
        thunk_pages,
        PageTableFlags::empty(),
    )?;

    let mut thunk_buffer = vec![0_u8; thunk_pages * PAGE_SIZE as usize];
    for (index, import) in imports.iter().enumerate() {
        let offset = index * win32::import_thunk_len();
        win32::encode_import_thunk(import.api, &mut thunk_buffer[offset..]);
        let thunk_addr = thunk_base
            .checked_add(offset as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE thunk address overflow"))?;
        let iat_addr = load_base
            .checked_add(import.first_thunk_rva as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE IAT address overflow"))?;
        address_space.initialize_user_bytes(VirtAddr::new(iat_addr), &thunk_addr.to_le_bytes())?;
    }

    address_space.initialize_user_bytes(VirtAddr::new(thunk_base), &thunk_buffer)?;
    Ok(())
}

fn collect_pe_imports(image: &[u8], pe: &PeImage) -> Result<Vec<ResolvedImport>, ProcessLoadError> {
    let import_dir = pe.directories[PE_DIRECTORY_IMPORT];
    if import_dir.rva == 0 || import_dir.size == 0 {
        return Ok(Vec::new());
    }

    let mut imports = Vec::new();
    let mut descriptor_offset = rva_to_file_offset(pe, import_dir.rva, image.len() as u32)?;
    let descriptor_limit = descriptor_offset
        .checked_add(import_dir.size as usize)
        .ok_or(ProcessLoadError::InvalidPe("PE import directory overflow"))?;
    if descriptor_limit > image.len() {
        return Err(ProcessLoadError::InvalidPe(
            "PE import directory is truncated",
        ));
    }

    while descriptor_offset + 20 <= descriptor_limit {
        let original_first_thunk = read_u32(image, descriptor_offset)?;
        let _timestamp = read_u32(image, descriptor_offset + 4)?;
        let _forwarder_chain = read_u32(image, descriptor_offset + 8)?;
        let name_rva = read_u32(image, descriptor_offset + 12)?;
        let first_thunk = read_u32(image, descriptor_offset + 16)?;
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            break;
        }

        let dll_name = read_c_string_at_rva(image, pe, name_rva)?;
        let mut thunk_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let mut first_thunk_rva = first_thunk;

        loop {
            let thunk_offset = rva_to_file_offset(pe, thunk_rva, image.len() as u32)?;
            let entry = read_u64(image, thunk_offset)?;
            if entry == 0 {
                break;
            }
            if (entry >> 63) != 0 {
                return Err(ProcessLoadError::InvalidPe(
                    "ordinal PE imports are not supported",
                ));
            }

            let name_rva = (entry & 0x7fff_ffff) as u32;
            let import_name = read_import_name_at_rva(image, pe, name_rva)?;
            let Some(api) = win32::resolve_import(dll_name, import_name) else {
                return Err(make_unsupported_import_error(dll_name, import_name));
            };

            imports.push(ResolvedImport {
                first_thunk_rva,
                api,
            });
            thunk_rva = thunk_rva
                .checked_add(8)
                .ok_or(ProcessLoadError::InvalidPe("PE import thunk overflow"))?;
            first_thunk_rva = first_thunk_rva
                .checked_add(8)
                .ok_or(ProcessLoadError::InvalidPe("PE import thunk overflow"))?;
        }

        descriptor_offset += 20;
    }

    Ok(imports)
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

fn read_import_name_at_rva<'a>(
    image: &'a [u8],
    pe: &PeImage,
    hint_name_rva: u32,
) -> Result<&'a [u8], ProcessLoadError> {
    let string_offset = rva_to_file_offset(pe, hint_name_rva, image.len() as u32)?
        .checked_add(2)
        .ok_or(ProcessLoadError::InvalidPe("PE import name overflow"))?;
    read_c_string(image, string_offset)
}

fn read_c_string_at_rva<'a>(
    image: &'a [u8],
    pe: &PeImage,
    rva: u32,
) -> Result<&'a [u8], ProcessLoadError> {
    let offset = rva_to_file_offset(pe, rva, image.len() as u32)?;
    read_c_string(image, offset)
}

fn read_c_string(image: &[u8], offset: usize) -> Result<&[u8], ProcessLoadError> {
    let bytes = image
        .get(offset..)
        .ok_or(ProcessLoadError::InvalidPe("PE string is truncated"))?;
    let Some(end) = bytes.iter().position(|&byte| byte == 0) else {
        return Err(ProcessLoadError::InvalidPe("PE string is not terminated"));
    };
    Ok(&bytes[..end])
}

fn rva_to_file_offset(pe: &PeImage, rva: u32, image_len: u32) -> Result<usize, ProcessLoadError> {
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

fn read_user_u64(address_space: &ProcessAddressSpace, addr: u64) -> Result<u64, ProcessLoadError> {
    let mut bytes = [0_u8; 8];
    address_space.copy_from_user(VirtAddr::new(addr), &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_bytes<'a>(
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

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ProcessLoadError> {
    let raw = read_bytes(bytes, offset, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ProcessLoadError> {
    let raw = read_bytes(bytes, offset, 4)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ProcessLoadError> {
    let raw = read_bytes(bytes, offset, 8)?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

fn is_range_within_user_space(start: u64, len: u64) -> bool {
    start >= paging::USER_SPACE_BASE
        && start
            .checked_add(len)
            .map(|end| end <= paging::USER_SPACE_END_EXCLUSIVE)
            .unwrap_or(false)
}

fn make_unsupported_import_error(dll_name: &[u8], function_name: &[u8]) -> ProcessLoadError {
    let mut dll = [0_u8; 32];
    let dll_len = dll_name.len().min(dll.len());
    dll[..dll_len].copy_from_slice(&dll_name[..dll_len]);

    let mut function = [0_u8; 64];
    let function_len = function_name.len().min(function.len());
    function[..function_len].copy_from_slice(&function_name[..function_len]);

    ProcessLoadError::UnsupportedImport {
        dll,
        dll_len,
        function,
        function_len,
    }
}

pub fn spawn_process(
    image: &[u8],
    weight_micros: u64,
    arg0: u64,
    arg1: u64,
) -> Result<SpawnedProcess, ProcessLoadError> {
    let mut loaded = load_image(image)?;

    let stack_span_pages = USER_STACK_GUARD_PAGES + USER_STACK_PAGES;
    let guard_start = VirtAddr::new(USER_STACK_TOP_EXCLUSIVE - stack_span_pages as u64 * PAGE_SIZE);
    ensure_unmapped_user_pages(
        &loaded.address_space,
        guard_start,
        USER_STACK_GUARD_PAGES,
        "user stack guard page address overflow",
        "user stack guard page overlaps an existing mapping",
    )?;

    let stack_start =
        VirtAddr::new(guard_start.as_u64() + USER_STACK_GUARD_PAGES as u64 * PAGE_SIZE);
    let stack_region = loaded.address_space.map_zeroed_user_pages_at(
        stack_start,
        USER_STACK_PAGES,
        PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
    )?;
    let user_stack_top = initial_user_stack_top(stack_region.end())?;

    let pid = multitask::spawn_user_process(
        loaded.address_space,
        loaded.entry,
        user_stack_top,
        weight_micros,
        arg0,
        arg1,
    )?;

    Ok(SpawnedProcess {
        pid,
        entry: loaded.entry,
    })
}

struct SegmentLoadInfo {
    addr: u64,
    end: u64,
    page_base: u64,
    page_end: u64,
    file_offset: usize,
    file_end: usize,
}

fn validate_elf_header(elf: &ElfFile<'_>) -> Result<(), ProcessLoadError> {
    if elf.header.pt1.class() != Class::SixtyFour {
        return Err(ProcessLoadError::InvalidElf("ELF class is not 64-bit"));
    }
    if elf.header.pt1.data() != Data::LittleEndian {
        return Err(ProcessLoadError::InvalidElf(
            "ELF endianness is not little-endian",
        ));
    }
    if elf.header.pt2.machine().as_machine() != Machine::X86_64 {
        return Err(ProcessLoadError::InvalidElf("ELF machine is not x86_64"));
    }
    if elf.header.pt2.type_().as_type() != ElfType::Executable {
        return Err(ProcessLoadError::InvalidElf("ELF type is not executable"));
    }
    Ok(())
}

fn validate_entry_point(elf: &ElfFile<'_>) -> Result<VirtAddr, ProcessLoadError> {
    let entry = elf.header.pt2.entry_point();
    if !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&entry) {
        return Err(ProcessLoadError::InvalidElf(
            "entry point is outside the supported user range",
        ));
    }
    Ok(VirtAddr::new(entry))
}

fn validated_segment_bounds(
    image: &[u8],
    ph: &ProgramHeader<'_>,
) -> Result<SegmentLoadInfo, ProcessLoadError> {
    validate_segment_alignment(ph)?;

    let file_size = usize::try_from(ph.file_size())
        .map_err(|_| ProcessLoadError::InvalidElf("segment file size out of range"))?;
    let mem_size = usize::try_from(ph.mem_size())
        .map_err(|_| ProcessLoadError::InvalidElf("segment memory size out of range"))?;
    let file_offset = usize::try_from(ph.offset())
        .map_err(|_| ProcessLoadError::InvalidElf("segment file offset out of range"))?;

    if mem_size == 0 {
        return Err(ProcessLoadError::InvalidElf(
            "PT_LOAD segment has zero memory size",
        ));
    }
    if file_size > mem_size {
        return Err(ProcessLoadError::InvalidElf(
            "segment file size exceeds memory size",
        ));
    }

    let file_end = file_offset
        .checked_add(file_size)
        .ok_or(ProcessLoadError::InvalidElf("segment file bounds overflow"))?;
    if file_end > image.len() {
        return Err(ProcessLoadError::InvalidElf(
            "segment file range is outside ELF image",
        ));
    }

    let addr = ph.virtual_addr();
    if !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&addr) {
        return Err(ProcessLoadError::InvalidElf(
            "segment address is outside the supported user range",
        ));
    }

    let end = addr
        .checked_add(mem_size as u64)
        .ok_or(ProcessLoadError::InvalidElf("segment address overflow"))?;
    if end > paging::USER_SPACE_END_EXCLUSIVE {
        return Err(ProcessLoadError::InvalidElf(
            "segment address exceeds the supported user range",
        ));
    }

    let page_base = align_down(addr, PAGE_SIZE);
    let page_end = align_up(end, PAGE_SIZE).ok_or(ProcessLoadError::InvalidElf(
        "segment page alignment overflow",
    ))?;

    Ok(SegmentLoadInfo {
        addr,
        end,
        page_base,
        page_end,
        file_offset,
        file_end,
    })
}

fn page_ranges_overlap(page_base: u64, page_end: u64, existing_ranges: &[(u64, u64)]) -> bool {
    for &(other_start, other_end) in existing_ranges {
        if page_base < other_end && other_start < page_end {
            return true;
        }
    }
    false
}

fn validate_segment_policy(ph: &ProgramHeader<'_>) -> Result<(), ProcessLoadError> {
    if ph.flags().is_write() && ph.flags().is_execute() {
        return Err(ProcessLoadError::InvalidElf(
            "writable executable PT_LOAD segment is not allowed",
        ));
    }

    Ok(())
}

fn validate_segment_alignment(ph: &ProgramHeader<'_>) -> Result<(), ProcessLoadError> {
    let align = ph.align();
    if align > 1 && !align.is_power_of_two() {
        return Err(ProcessLoadError::InvalidElf(
            "PT_LOAD alignment must be zero, one, or a power of two",
        ));
    }

    if align > 1 && ((ph.virtual_addr() ^ ph.offset()) & (align - 1)) != 0 {
        return Err(ProcessLoadError::InvalidElf(
            "PT_LOAD virtual address and file offset are misaligned",
        ));
    }

    Ok(())
}

fn segment_page_flags(ph: &ProgramHeader<'_>) -> PageTableFlags {
    let mut flags = PageTableFlags::empty();
    if ph.flags().is_write() {
        flags |= PageTableFlags::WRITABLE;
    }
    if !ph.flags().is_execute() {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

fn ensure_unmapped_user_pages(
    address_space: &ProcessAddressSpace,
    start: VirtAddr,
    page_count: usize,
    overflow_reason: &'static str,
    overlap_reason: &'static str,
) -> Result<(), ProcessLoadError> {
    for page_index in 0..page_count {
        let page_addr = start
            .as_u64()
            .checked_add(page_index as u64 * PAGE_SIZE)
            .ok_or(ProcessLoadError::InvalidPe(overflow_reason))?;
        if address_space
            .translate_user(VirtAddr::new(page_addr))
            .is_some()
        {
            return Err(ProcessLoadError::InvalidPe(overlap_reason));
        }
    }

    Ok(())
}

fn initial_user_stack_top(stack_end: VirtAddr) -> Result<VirtAddr, ProcessLoadError> {
    let aligned_top = align_down(stack_end.as_u64(), 16);
    let user_stack_top = aligned_top
        .checked_sub(8)
        .ok_or(ProcessLoadError::InvalidElf(
            "user stack top calculation underflow",
        ))?;
    Ok(VirtAddr::new(user_stack_top))
}

fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| align_down(aligned, align))
}
