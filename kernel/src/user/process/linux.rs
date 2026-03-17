use alloc::vec;
use alloc::vec::Vec;
use core::cmp;
use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;
use xmas_elf::ElfFile;
use xmas_elf::dynamic::Tag as DynamicTag;
use xmas_elf::header::{Class, Data, Machine, Type as ElfType};
use xmas_elf::program::{ProgramHeader, SegmentData, Type as ProgramType};

use crate::debug;
use crate::fat;
use crate::paging::{self, ProcessAddressSpace};
use crate::user::abi::UserAbi;
use crate::user::linux::{
    self as linux_abi, LinuxInitialTlsInfo, LinuxProcessImageInfo, LinuxProcessLaunch,
};

use super::{
    LoadedProcessImage, LoadedProcessRuntime, MAX_LOAD_SEGMENTS, PAGE_SIZE, ProcessLoadError,
    align_down, align_up, page_ranges_overlap,
};

const ELF_DYN_LOAD_BASE: u64 = paging::USER_SPACE_BASE + 0x0040_0000;
const ELF_INTERP_LOAD_BASE: u64 = paging::USER_SPACE_BASE + 0x0200_0000;
const ELF_RELOC_X86_64_RELATIVE: u32 = 8;
const ELF_RELOC_X86_64_IRELATIVE: u32 = 37;
const LINUX_STACK_RANDOM_BYTES: usize = 16;
const LINUX_STACK_CLOCK_TICKS: u64 = 100;
const INITIAL_TLS_TCB_ALIGN: u64 = 16;
const INITIAL_TLS_TCB_SIZE: u64 = 64;
const INITIAL_TLS_DTV_SIZE: u64 = 32;

#[derive(Clone, Copy)]
enum ElfImageType {
    Executable,
    StaticPie,
}

#[derive(Clone, Copy)]
struct ElfDynamicRelocationInfo {
    rela_address: u64,
    rela_size: u64,
    rela_entry_size: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Rela64 {
    offset: u64,
    info: u64,
    addend: i64,
}

struct SegmentLoadInfo {
    addr: u64,
    end: u64,
    page_base: u64,
    page_end: u64,
    file_offset: usize,
    file_end: usize,
}

#[derive(Clone, Copy)]
struct MappedElfImage {
    entry: VirtAddr,
    load_bias: u64,
    max_loaded_end: u64,
}

#[derive(Clone, Copy)]
struct ElfTlsTemplateInfo {
    template_addr: u64,
    template_size: u64,
    mem_size: u64,
    align: u64,
}

pub(super) fn load_elf(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    let elf = ElfFile::new(image).map_err(ProcessLoadError::InvalidElf)?;
    let elf_image_type = validate_elf_header(&elf)?;
    let interpreter_path = elf_interpreter_path(image, &elf)?;
    let load_bias = choose_elf_load_bias(&elf, elf_image_type, ELF_DYN_LOAD_BASE)?;
    let mut address_space = ProcessAddressSpace::new()?;

    let mut loaded_segments = 0usize;
    let mut mapped_page_ranges = [(0_u64, 0_u64); MAX_LOAD_SEGMENTS];
    let main_image = map_elf_image(
        &elf,
        image,
        elf_image_type,
        load_bias,
        &mut address_space,
        &mut mapped_page_ranges,
        &mut loaded_segments,
        interpreter_path.is_none(),
    )?;

    let (entry, interpreter_base, max_loaded_end) = if let Some(interpreter_path) = interpreter_path
    {
        let interpreter_image = fat::read_file_to_vec(interpreter_path)
            .map_err(|error| make_interpreter_load_error(interpreter_path, error))?;
        let interpreter_elf =
            ElfFile::new(interpreter_image.as_slice()).map_err(ProcessLoadError::InvalidElf)?;
        let interpreter_type = validate_elf_header(&interpreter_elf)?;
        ensure_no_elf_interpreter(&interpreter_elf)?;
        let interpreter_load_bias =
            choose_elf_load_bias(&interpreter_elf, interpreter_type, ELF_INTERP_LOAD_BASE)?;
        let interpreter = map_elf_image(
            &interpreter_elf,
            interpreter_image.as_slice(),
            interpreter_type,
            interpreter_load_bias,
            &mut address_space,
            &mut mapped_page_ranges,
            &mut loaded_segments,
            false,
        )?;
        (
            interpreter.entry,
            interpreter.load_bias,
            main_image.max_loaded_end.max(interpreter.max_loaded_end),
        )
    } else {
        (main_image.entry, 0, main_image.max_loaded_end)
    };

    let linux_image = build_linux_process_image(
        &elf,
        image,
        main_image.load_bias,
        max_loaded_end,
        main_image.entry.as_u64(),
        interpreter_base,
    )?;

    Ok(LoadedProcessImage {
        abi: UserAbi::Linux,
        address_space,
        entry,
        runtime: LoadedProcessRuntime::Linux(linux_image),
    })
}

fn validate_elf_header(elf: &ElfFile<'_>) -> Result<ElfImageType, ProcessLoadError> {
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

    match elf.header.pt2.type_().as_type() {
        ElfType::Executable => Ok(ElfImageType::Executable),
        ElfType::SharedObject => Ok(ElfImageType::StaticPie),
        _ => Err(ProcessLoadError::InvalidElf(
            "ELF type is not executable or static PIE",
        )),
    }
}

fn ensure_no_elf_interpreter(elf: &ElfFile<'_>) -> Result<(), ProcessLoadError> {
    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type == ProgramType::Interp {
            return Err(ProcessLoadError::InvalidElf(
                "nested ELF interpreters are not supported",
            ));
        }
    }

    Ok(())
}

fn choose_elf_load_bias(
    elf: &ElfFile<'_>,
    image_type: ElfImageType,
    preferred_base: u64,
) -> Result<u64, ProcessLoadError> {
    match image_type {
        ElfImageType::Executable => Ok(0),
        ElfImageType::StaticPie => {
            let mut min_load_addr = u64::MAX;
            for ph in elf.program_iter() {
                let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
                if ph_type != ProgramType::Load || ph.mem_size() == 0 {
                    continue;
                }
                min_load_addr = min_load_addr.min(align_down(ph.virtual_addr(), PAGE_SIZE));
            }

            if min_load_addr == u64::MAX {
                return Err(ProcessLoadError::InvalidElf(
                    "ELF does not contain PT_LOAD segments",
                ));
            }

            let load_bias =
                preferred_base
                    .checked_sub(min_load_addr)
                    .ok_or(ProcessLoadError::InvalidElf(
                        "static PIE load bias underflow",
                    ))?;
            debug::println!(
                "process load_elf: static pie min_load={:#x} load_bias={:#x}",
                min_load_addr,
                load_bias,
            );
            Ok(load_bias)
        }
    }
}

fn validate_entry_point(elf: &ElfFile<'_>, load_bias: u64) -> Result<VirtAddr, ProcessLoadError> {
    let entry = elf
        .header
        .pt2
        .entry_point()
        .checked_add(load_bias)
        .ok_or(ProcessLoadError::InvalidElf("entry point address overflow"))?;
    if !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&entry) {
        debug::println!(
            "process load_elf: entry out of range raw={:#x} load_bias={:#x} entry={:#x} user=[{:#x}, {:#x})",
            elf.header.pt2.entry_point(),
            load_bias,
            entry,
            paging::USER_SPACE_BASE,
            paging::USER_SPACE_END_EXCLUSIVE,
        );
        return Err(ProcessLoadError::InvalidElf(
            "entry point is outside the supported user range",
        ));
    }
    debug::println!(
        "process load_elf: entry raw={:#x} final={:#x}",
        elf.header.pt2.entry_point(),
        entry,
    );
    Ok(VirtAddr::new(entry))
}

fn elf_interpreter_path<'a>(
    image: &'a [u8],
    elf: &ElfFile<'_>,
) -> Result<Option<&'a str>, ProcessLoadError> {
    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type != ProgramType::Interp {
            continue;
        }

        let offset = usize::try_from(ph.offset())
            .map_err(|_| ProcessLoadError::InvalidElf("PT_INTERP file offset is out of range"))?;
        let size = usize::try_from(ph.file_size())
            .map_err(|_| ProcessLoadError::InvalidElf("PT_INTERP size is out of range"))?;
        let bytes = image
            .get(
                offset
                    ..offset
                        .checked_add(size)
                        .ok_or(ProcessLoadError::InvalidElf("PT_INTERP bounds overflow"))?,
            )
            .ok_or(ProcessLoadError::InvalidElf("PT_INTERP is truncated"))?;
        let nul = bytes
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(ProcessLoadError::InvalidElf(
                "PT_INTERP path is not terminated",
            ))?;
        let path = core::str::from_utf8(&bytes[..nul])
            .map_err(|_| ProcessLoadError::InvalidElf("PT_INTERP path is not valid UTF-8"))?;
        return Ok(Some(path));
    }

    Ok(None)
}

fn map_elf_image(
    elf: &ElfFile<'_>,
    image: &[u8],
    image_type: ElfImageType,
    load_bias: u64,
    address_space: &mut ProcessAddressSpace,
    mapped_page_ranges: &mut [(u64, u64); MAX_LOAD_SEGMENTS],
    loaded_segments: &mut usize,
    apply_kernel_relocations: bool,
) -> Result<MappedElfImage, ProcessLoadError> {
    let entry = validate_entry_point(elf, load_bias)?;
    let mut executable_entry_covered = false;
    let mut max_loaded_end = 0_u64;
    let mut image_loaded_segments = 0usize;

    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type != ProgramType::Load {
            continue;
        }
        if ph.mem_size() == 0 && ph.file_size() == 0 {
            continue;
        }

        if *loaded_segments >= MAX_LOAD_SEGMENTS {
            return Err(ProcessLoadError::InvalidElf("too many PT_LOAD segments"));
        }

        validate_segment_policy(&ph)?;
        let segment = validated_segment_bounds(image, &ph, load_bias)?;
        if page_ranges_overlap(
            segment.page_base,
            segment.page_end,
            &mapped_page_ranges[..*loaded_segments],
        ) {
            return Err(ProcessLoadError::InvalidElf("PT_LOAD page ranges overlap"));
        }
        mapped_page_ranges[*loaded_segments] = (segment.page_base, segment.page_end);
        max_loaded_end = max_loaded_end.max(segment.end);

        if ph.flags().is_execute() && (segment.addr..segment.end).contains(&entry.as_u64()) {
            executable_entry_covered = true;
        }

        let page_count = ((segment.page_end - segment.page_base) / PAGE_SIZE) as usize;
        let page_flags = segment_page_flags(&ph);
        debug::println!(
            "process load_elf: map segment {} base={:#x} end={:#x} pages={} file={} mem={}",
            *loaded_segments,
            segment.page_base,
            segment.page_end,
            page_count,
            ph.file_size(),
            ph.mem_size(),
        );
        address_space.map_zeroed_user_pages_at(
            VirtAddr::new(segment.page_base),
            page_count,
            page_flags,
        )?;
        debug::println!("process load_elf: segment {} mapped", *loaded_segments);
        address_space.initialize_user_bytes(
            VirtAddr::new(segment.addr),
            &image[segment.file_offset..segment.file_end],
        )?;
        debug::println!("process load_elf: segment {} initialized", *loaded_segments);
        *loaded_segments += 1;
        image_loaded_segments += 1;
    }

    if image_loaded_segments == 0 {
        return Err(ProcessLoadError::InvalidElf(
            "ELF does not contain PT_LOAD segments",
        ));
    }
    if !executable_entry_covered {
        return Err(ProcessLoadError::InvalidElf(
            "entry point is not inside an executable PT_LOAD segment",
        ));
    }

    if apply_kernel_relocations && matches!(image_type, ElfImageType::StaticPie) {
        apply_elf_dynamic_relocations(elf, image, address_space, load_bias)?;
    }

    Ok(MappedElfImage {
        entry,
        load_bias,
        max_loaded_end,
    })
}

fn validated_segment_bounds(
    image: &[u8],
    ph: &ProgramHeader<'_>,
    load_bias: u64,
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

    let addr = ph
        .virtual_addr()
        .checked_add(load_bias)
        .ok_or(ProcessLoadError::InvalidElf("segment address overflow"))?;
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

fn build_linux_process_image(
    elf: &ElfFile<'_>,
    image: &[u8],
    load_bias: u64,
    max_loaded_end: u64,
    entry: u64,
    interpreter_base: u64,
) -> Result<LinuxProcessImageInfo, ProcessLoadError> {
    let program_headers = program_header_table_addr(elf, load_bias)?;
    // Dynamic ELF relies on the userspace interpreter to build the initial thread
    // pointer and module TLS layout. For direct ELF entry we provide a minimal
    // kernel-side static TLS/TCB so runtimes have a valid initial FS base.
    let initial_tls = if interpreter_base == 0 {
        elf_initial_tls_info(elf, image, load_bias, max_loaded_end)?
    } else {
        None
    };
    let brk_start = if let Some(tls) = initial_tls {
        tls.mapping_base
            .checked_add(tls.mapping_size)
            .ok_or(ProcessLoadError::InvalidElf(
                "initial brk calculation overflow",
            ))?
    } else {
        align_up(max_loaded_end, PAGE_SIZE).ok_or(ProcessLoadError::InvalidElf(
            "initial brk calculation overflow",
        ))?
    };

    Ok(LinuxProcessImageInfo {
        entry,
        interpreter_base,
        program_headers,
        program_header_entry_size: elf.header.pt2.ph_entry_size() as u64,
        program_header_count: elf.header.pt2.ph_count() as u64,
        brk_start,
        initial_tls,
    })
}

fn elf_initial_tls_info(
    elf: &ElfFile<'_>,
    image: &[u8],
    load_bias: u64,
    max_loaded_end: u64,
) -> Result<Option<LinuxInitialTlsInfo>, ProcessLoadError> {
    let Some(template) = elf_tls_template_info(elf, image, load_bias)? else {
        return Ok(None);
    };

    elf_initial_tls_info_from_template(template, max_loaded_end)
        .map(Some)
        .ok_or(ProcessLoadError::InvalidElf(
            "PT_TLS layout calculation overflow",
        ))
}

fn elf_initial_tls_info_from_template(
    template: ElfTlsTemplateInfo,
    max_loaded_end: u64,
) -> Option<LinuxInitialTlsInfo> {
    let mapping_base = align_up(max_loaded_end, PAGE_SIZE)?;
    let tls_align = template.align.max(1);
    let tls_block_size = align_up(template.mem_size, tls_align)?;
    let thread_pointer_align = cmp::max(tls_align, INITIAL_TLS_TCB_ALIGN);
    let tls_end_hint = mapping_base.checked_add(tls_block_size)?;
    let thread_pointer = align_up(tls_end_hint, thread_pointer_align)?;
    let tls_block_base = thread_pointer.checked_sub(tls_block_size)?;
    let tcb_base = thread_pointer;
    let dtv_base = tcb_base.checked_add(INITIAL_TLS_TCB_SIZE)?;
    let mapping_end_hint = dtv_base.checked_add(INITIAL_TLS_DTV_SIZE)?;
    let mapping_end = align_up(mapping_end_hint, PAGE_SIZE)?;
    if mapping_end > paging::USER_SPACE_END_EXCLUSIVE {
        return None;
    }

    Some(LinuxInitialTlsInfo {
        template_addr: template.template_addr,
        template_size: template.template_size,
        mem_size: template.mem_size,
        align: tls_align,
        mapping_base,
        mapping_size: mapping_end - mapping_base,
        tls_block_base,
        thread_pointer,
        tcb_base,
        dtv_base,
    })
}

fn elf_tls_template_info(
    elf: &ElfFile<'_>,
    image: &[u8],
    load_bias: u64,
) -> Result<Option<ElfTlsTemplateInfo>, ProcessLoadError> {
    let mut tls_template = None;

    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type != ProgramType::Tls {
            continue;
        }
        if tls_template.is_some() {
            return Err(ProcessLoadError::InvalidElf(
                "multiple PT_TLS segments are not supported",
            ));
        }

        let template_size = ph.file_size();
        let mem_size = ph.mem_size();
        if mem_size == 0 {
            continue;
        }
        if template_size > mem_size {
            return Err(ProcessLoadError::InvalidElf(
                "PT_TLS file size exceeds memory size",
            ));
        }

        let align = ph.align().max(1);
        if !align.is_power_of_two() {
            return Err(ProcessLoadError::InvalidElf(
                "PT_TLS alignment must be zero, one, or a power of two",
            ));
        }

        let template_addr =
            ph.virtual_addr()
                .checked_add(load_bias)
                .ok_or(ProcessLoadError::InvalidElf(
                    "PT_TLS template address overflow",
                ))?;
        let template_end =
            template_addr
                .checked_add(template_size)
                .ok_or(ProcessLoadError::InvalidElf(
                    "PT_TLS template bounds overflow",
                ))?;
        if template_end > paging::USER_SPACE_END_EXCLUSIVE {
            return Err(ProcessLoadError::InvalidElf(
                "PT_TLS template is outside the supported user range",
            ));
        }
        if template_size != 0 {
            let _ = elf_file_slice_from_virtual_address(
                image,
                elf,
                ph.virtual_addr(),
                usize::try_from(template_size).map_err(|_| {
                    ProcessLoadError::InvalidElf("PT_TLS template size is out of range")
                })?,
            )?;
        }

        tls_template = Some(ElfTlsTemplateInfo {
            template_addr,
            template_size,
            mem_size,
            align,
        });
    }

    Ok(tls_template)
}

fn program_header_table_addr(elf: &ElfFile<'_>, load_bias: u64) -> Result<u64, ProcessLoadError> {
    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type == ProgramType::Phdr {
            return ph
                .virtual_addr()
                .checked_add(load_bias)
                .ok_or(ProcessLoadError::InvalidElf(
                    "program header address overflow",
                ));
        }
    }

    let ph_offset = elf.header.pt2.ph_offset();
    let ph_size = (elf.header.pt2.ph_entry_size() as u64)
        .checked_mul(elf.header.pt2.ph_count() as u64)
        .ok_or(ProcessLoadError::InvalidElf(
            "program header table size overflow",
        ))?;
    let ph_end = ph_offset
        .checked_add(ph_size)
        .ok_or(ProcessLoadError::InvalidElf(
            "program header table bounds overflow",
        ))?;

    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type != ProgramType::Load || ph.file_size() == 0 {
            continue;
        }

        let file_start = ph.offset();
        let file_end = file_start
            .checked_add(ph.file_size())
            .ok_or(ProcessLoadError::InvalidElf("PT_LOAD file bounds overflow"))?;
        if ph_offset < file_start || ph_end > file_end {
            continue;
        }

        let table_delta = ph_offset - file_start;
        return ph
            .virtual_addr()
            .checked_add(table_delta)
            .and_then(|value| value.checked_add(load_bias))
            .ok_or(ProcessLoadError::InvalidElf(
                "program header table address overflow",
            ));
    }

    Err(ProcessLoadError::InvalidElf(
        "program header table is not mapped by PT_LOAD",
    ))
}

fn apply_elf_dynamic_relocations(
    elf: &ElfFile<'_>,
    image: &[u8],
    address_space: &ProcessAddressSpace,
    load_bias: u64,
) -> Result<(), ProcessLoadError> {
    let Some(relocations) = parse_elf_dynamic_relocations(elf)? else {
        return Ok(());
    };

    if relocations.rela_entry_size != core::mem::size_of::<Rela64>() as u64 {
        return Err(ProcessLoadError::InvalidElf(
            "ELF RELA entry size is not supported",
        ));
    }

    let rela_size = usize::try_from(relocations.rela_size)
        .map_err(|_| ProcessLoadError::InvalidElf("ELF RELA size is out of range"))?;
    let rela_bytes =
        elf_file_slice_from_virtual_address(image, elf, relocations.rela_address, rela_size)?;
    if rela_bytes.len() % core::mem::size_of::<Rela64>() != 0 {
        return Err(ProcessLoadError::InvalidElf(
            "ELF RELA table size is not aligned",
        ));
    }

    for chunk in rela_bytes.chunks_exact(core::mem::size_of::<Rela64>()) {
        let relocation = read_rela64(chunk);
        let relocation_type = relocation.info as u32;
        if relocation_type == ELF_RELOC_X86_64_IRELATIVE {
            return Err(ProcessLoadError::InvalidElf(
                "IFUNC relocations are not supported for static PIE",
            ));
        }
        if relocation_type != ELF_RELOC_X86_64_RELATIVE {
            return Err(ProcessLoadError::InvalidElf(
                "unsupported static PIE relocation type",
            ));
        }
        if (relocation.info >> 32) != 0 {
            return Err(ProcessLoadError::InvalidElf(
                "static PIE relocation unexpectedly references a symbol",
            ));
        }

        let target_addr =
            relocation
                .offset
                .checked_add(load_bias)
                .ok_or(ProcessLoadError::InvalidElf(
                    "relocation target address overflow",
                ))?;
        let relocated = add_signed_u64(load_bias, relocation.addend)
            .ok_or(ProcessLoadError::InvalidElf("relocation value overflow"))?;
        address_space
            .initialize_user_bytes(VirtAddr::new(target_addr), &relocated.to_le_bytes())?;
    }

    Ok(())
}

fn parse_elf_dynamic_relocations(
    elf: &ElfFile<'_>,
) -> Result<Option<ElfDynamicRelocationInfo>, ProcessLoadError> {
    let Some(dynamic_segment) = elf.program_iter().find(|ph| {
        ph.get_type()
            .map(|kind| kind == ProgramType::Dynamic)
            .unwrap_or(false)
    }) else {
        return Ok(None);
    };

    let mut info = ElfDynamicRelocationInfo {
        rela_address: 0,
        rela_size: 0,
        rela_entry_size: 0,
    };

    let entries = match dynamic_segment
        .get_data(elf)
        .map_err(ProcessLoadError::InvalidElf)?
    {
        SegmentData::Dynamic64(entries) => entries,
        _ => {
            return Err(ProcessLoadError::InvalidElf(
                "dynamic segment is not 64-bit",
            ));
        }
    };

    for entry in entries {
        match entry.get_tag().map_err(ProcessLoadError::InvalidElf)? {
            DynamicTag::Rela => {
                info.rela_address = entry.get_ptr().map_err(ProcessLoadError::InvalidElf)?;
            }
            DynamicTag::RelaSize => {
                info.rela_size = entry.get_val().map_err(ProcessLoadError::InvalidElf)?;
            }
            DynamicTag::RelaEnt => {
                info.rela_entry_size = entry.get_val().map_err(ProcessLoadError::InvalidElf)?;
            }
            DynamicTag::Null => break,
            _ => {}
        }
    }

    if info.rela_size == 0 {
        return Ok(None);
    }
    if info.rela_address == 0 || info.rela_entry_size == 0 {
        return Err(ProcessLoadError::InvalidElf(
            "dynamic relocation metadata is incomplete",
        ));
    }

    Ok(Some(info))
}

fn elf_file_slice_from_virtual_address<'a>(
    image: &'a [u8],
    elf: &ElfFile<'_>,
    virtual_address: u64,
    len: usize,
) -> Result<&'a [u8], ProcessLoadError> {
    let end = virtual_address
        .checked_add(len as u64)
        .ok_or(ProcessLoadError::InvalidElf(
            "ELF virtual slice bounds overflow",
        ))?;

    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(ProcessLoadError::InvalidElf)?;
        if ph_type != ProgramType::Load || ph.file_size() == 0 {
            continue;
        }

        let segment_start = ph.virtual_addr();
        let segment_end =
            segment_start
                .checked_add(ph.file_size())
                .ok_or(ProcessLoadError::InvalidElf(
                    "ELF PT_LOAD file-backed range overflow",
                ))?;
        if virtual_address < segment_start || end > segment_end {
            continue;
        }

        let delta = usize::try_from(virtual_address - segment_start)
            .map_err(|_| ProcessLoadError::InvalidElf("ELF virtual slice is out of range"))?;
        let file_offset = usize::try_from(ph.offset())
            .map_err(|_| ProcessLoadError::InvalidElf("ELF file offset is out of range"))?;
        let start = file_offset
            .checked_add(delta)
            .ok_or(ProcessLoadError::InvalidElf(
                "ELF file slice offset overflow",
            ))?;
        let end = start.checked_add(len).ok_or(ProcessLoadError::InvalidElf(
            "ELF file slice bounds overflow",
        ))?;
        return image
            .get(start..end)
            .ok_or(ProcessLoadError::InvalidElf("ELF file slice is truncated"));
    }

    Err(ProcessLoadError::InvalidElf(
        "ELF virtual slice is not covered by a PT_LOAD segment",
    ))
}

fn read_rela64(bytes: &[u8]) -> Rela64 {
    debug_assert_eq!(bytes.len(), core::mem::size_of::<Rela64>());
    Rela64 {
        offset: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        info: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        addend: i64::from_le_bytes(bytes[16..24].try_into().unwrap()),
    }
}

fn add_signed_u64(base: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        base.checked_add(delta as u64)
    } else {
        base.checked_sub(delta.unsigned_abs())
    }
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

pub(super) fn initialize_linux_initial_tls(
    address_space: &mut ProcessAddressSpace,
    image: LinuxProcessImageInfo,
) -> Result<(), ProcessLoadError> {
    let Some(tls) = image.initial_tls else {
        return Ok(());
    };

    address_space.map_zeroed_user_bytes_at(
        VirtAddr::new(tls.mapping_base),
        usize::try_from(tls.mapping_size)
            .map_err(|_| ProcessLoadError::InvalidElf("PT_TLS mapping size is out of range"))?,
        PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
    )?;

    if tls.template_size != 0 {
        let mut template = vec![
            0_u8;
            usize::try_from(tls.template_size).map_err(|_| {
                ProcessLoadError::InvalidElf("PT_TLS template size is out of range")
            })?
        ];
        address_space.copy_from_user(VirtAddr::new(tls.template_addr), &mut template)?;
        address_space.initialize_user_bytes(VirtAddr::new(tls.tls_block_base), &template)?;
    }

    let mut tcb_head = [0_u8; INITIAL_TLS_TCB_SIZE as usize];
    tcb_head[0..8].copy_from_slice(&tls.thread_pointer.to_le_bytes());
    tcb_head[8..16].copy_from_slice(&tls.dtv_base.to_le_bytes());
    tcb_head[16..24].copy_from_slice(&tls.thread_pointer.to_le_bytes());
    address_space.initialize_user_bytes(VirtAddr::new(tls.tcb_base), &tcb_head)?;

    Ok(())
}

pub(super) fn initialize_linux_user_stack(
    address_space: &ProcessAddressSpace,
    stack_end: VirtAddr,
    image: LinuxProcessImageInfo,
    launch: LinuxProcessLaunch<'_>,
) -> Result<VirtAddr, ProcessLoadError> {
    let aligned_top = align_down(stack_end.as_u64(), 16);
    let mut cursor = aligned_top;
    let mut random_bytes = [0_u8; LINUX_STACK_RANDOM_BYTES];
    crate::random::Random::new().fill_bytes(&mut random_bytes);
    let random_addr = push_stack_bytes(
        address_space,
        &mut cursor,
        &random_bytes,
        16,
        "linux AT_RANDOM placement overflow",
    )?;

    let exec_path = launch.exec_path;
    let execfn_addr = if exec_path.is_empty() {
        0
    } else {
        push_stack_c_string(
            address_space,
            &mut cursor,
            exec_path,
            "linux execfn placement overflow",
        )?
    };

    let env_ptrs = push_stack_c_string_list(
        address_space,
        &mut cursor,
        launch.env,
        "linux env string placement overflow",
    )?;

    let argv_storage;
    let argv_values = if launch.argv.is_empty() {
        if exec_path.is_empty() {
            &[][..]
        } else {
            argv_storage = vec![exec_path];
            &argv_storage[..]
        }
    } else {
        launch.argv
    };
    let argv_ptrs = push_stack_c_string_list(
        address_space,
        &mut cursor,
        argv_values,
        "linux argv string placement overflow",
    )?;

    let auxv = [
        linux_abi::AT_PHDR,
        image.program_headers,
        linux_abi::AT_PHENT,
        image.program_header_entry_size,
        linux_abi::AT_PHNUM,
        image.program_header_count,
        linux_abi::AT_PAGESZ,
        PAGE_SIZE,
        linux_abi::AT_BASE,
        image.interpreter_base,
        linux_abi::AT_FLAGS,
        0,
        linux_abi::AT_ENTRY,
        image.entry,
        linux_abi::AT_UID,
        0,
        linux_abi::AT_EUID,
        0,
        linux_abi::AT_GID,
        0,
        linux_abi::AT_EGID,
        0,
        linux_abi::AT_CLKTCK,
        LINUX_STACK_CLOCK_TICKS,
        linux_abi::AT_SECURE,
        0,
        linux_abi::AT_RANDOM,
        random_addr,
        linux_abi::AT_HWCAP2,
        0,
        linux_abi::AT_EXECFN,
        execfn_addr,
        linux_abi::AT_NULL,
        0,
    ];

    let mut stack_words = Vec::with_capacity(2 + argv_ptrs.len() + env_ptrs.len() + auxv.len());
    stack_words.push(argv_ptrs.len() as u64);
    stack_words.extend_from_slice(&argv_ptrs);
    stack_words.push(0);
    stack_words.extend_from_slice(&env_ptrs);
    stack_words.push(0);
    stack_words.extend_from_slice(&auxv);

    let stack_bytes_len = stack_words
        .len()
        .checked_mul(core::mem::size_of::<u64>())
        .ok_or(ProcessLoadError::InvalidElf(
            "linux user stack size overflow",
        ))? as u64;
    let stack_start = align_down(
        cursor
            .checked_sub(stack_bytes_len)
            .ok_or(ProcessLoadError::InvalidElf(
                "linux user stack calculation underflow",
            ))?,
        16,
    );
    let mut stack_bytes = Vec::with_capacity(stack_words.len() * core::mem::size_of::<u64>());
    for word in stack_words {
        stack_bytes.extend_from_slice(&word.to_le_bytes());
    }

    address_space.initialize_user_bytes(VirtAddr::new(stack_start), &stack_bytes)?;
    Ok(VirtAddr::new(stack_start))
}

fn push_stack_c_string_list(
    address_space: &ProcessAddressSpace,
    cursor: &mut u64,
    values: &[&str],
    overflow_reason: &'static str,
) -> Result<Vec<u64>, ProcessLoadError> {
    let mut pointers = Vec::with_capacity(values.len());
    for value in values.iter().rev() {
        pointers.push(push_stack_c_string(
            address_space,
            cursor,
            value,
            overflow_reason,
        )?);
    }
    pointers.reverse();
    Ok(pointers)
}

fn push_stack_c_string(
    address_space: &ProcessAddressSpace,
    cursor: &mut u64,
    value: &str,
    overflow_reason: &'static str,
) -> Result<u64, ProcessLoadError> {
    let mut bytes = Vec::with_capacity(value.len() + 1);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    push_stack_bytes(address_space, cursor, &bytes, 1, overflow_reason)
}

fn push_stack_bytes(
    address_space: &ProcessAddressSpace,
    cursor: &mut u64,
    bytes: &[u8],
    align: u64,
    overflow_reason: &'static str,
) -> Result<u64, ProcessLoadError> {
    let next = cursor
        .checked_sub(bytes.len() as u64)
        .ok_or(ProcessLoadError::InvalidElf(overflow_reason))?;
    let aligned = align_down(next, align.max(1));
    address_space.initialize_user_bytes(VirtAddr::new(aligned), bytes)?;
    *cursor = aligned;
    Ok(aligned)
}

fn make_interpreter_load_error(
    path: &str,
    error: fatfs::Error<fat::DiskIoError>,
) -> ProcessLoadError {
    let mut stored_path = [0_u8; 128];
    let path_bytes = path.as_bytes();
    let path_len = path_bytes.len().min(stored_path.len());
    stored_path[..path_len].copy_from_slice(&path_bytes[..path_len]);

    ProcessLoadError::InterpreterLoad {
        path: stored_path,
        path_len,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ELF_DYN_LOAD_BASE, ElfTlsTemplateInfo, INITIAL_TLS_TCB_ALIGN, INITIAL_TLS_TCB_SIZE,
        elf_initial_tls_info_from_template,
    };
    use crate::paging::USER_SPACE_BASE;

    #[test]
    fn initial_tls_layout_places_tls_before_thread_pointer() {
        let template = ElfTlsTemplateInfo {
            template_addr: ELF_DYN_LOAD_BASE + 0x2000,
            template_size: 24,
            mem_size: 48,
            align: 32,
        };
        let tls = elf_initial_tls_info_from_template(template, USER_SPACE_BASE + 0x12345)
            .expect("tls layout");
        assert_eq!(tls.thread_pointer, tls.tcb_base);
        assert_eq!(tls.thread_pointer & (INITIAL_TLS_TCB_ALIGN - 1), 0);
        assert_eq!(tls.tls_block_base & (template.align - 1), 0);
        assert!(tls.tls_block_base >= tls.mapping_base);
        assert_eq!(tls.thread_pointer - tls.tls_block_base, 64);
        assert_eq!(tls.dtv_base, tls.tcb_base + INITIAL_TLS_TCB_SIZE);
        assert_eq!(tls.mapping_size % 4096, 0);
    }
}
