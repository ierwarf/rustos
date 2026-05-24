use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp;
use core::convert::TryFrom;
use core::ptr;

use object::elf::{self as objelf, FileHeader64 as RawElfHeader};
use object::read::FileKind;
use object::LittleEndian;
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;
use xmas_elf::dynamic::Tag as DynamicTag;
use xmas_elf::program::{ProgramHeader, SegmentData, Type as ProgramType};
use xmas_elf::ElfFile;

use crate::memory::paging::{self, ProcessAddressSpace};
use crate::multitask::UserStackState;
use crate::user::abi::UserAbi;
use crate::user::handles::VfsFileHandle;
use crate::user::linux::{
    self as linux_abi, LinuxImageMapping, LinuxImageMappingPathKind, LinuxInitialTlsInfo,
    LinuxMemoryMapState, LinuxProcessImageInfo, LinuxProcessLaunch, LinuxRuntimeProfile, LinuxVma,
    LinuxVmaFlags, LinuxVmaName,
};
use crate::user::process_state::ProcessSecurityContext;
use crate::vfs;

use super::{
    align_down, align_up, page_ranges_overlap, LoadedProcessImage, LoadedProcessRuntime,
    ProcessLoadError, MAX_LOAD_SEGMENTS, PAGE_SIZE,
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
const ELF64_HEADER_SIZE: usize = 64;
const ELF64_PROGRAM_HEADER_SIZE: u64 = 56;
const ELF64_DYNAMIC_ENTRY_SIZE: usize = 16;
const ELF_ENDIAN: LittleEndian = LittleEndian;
const LINUX_AUX_PLATFORM: &str = "x86_64";
const LINUX_AUX_HWCAP: u64 = 0;
const LINUX_AUX_HWCAP2: u64 = 0;

// RING3-MIGRATION-REFERENCE START: loaderd/procd/syscalld should own Linux ELF
// image policy, interpreter/runtime search, segment validation, mapping
// manifests, dynamic relocation policy, runtime profile construction, and
// initial memory-map metadata. Ring0 should keep only address-space commit,
// page mutation, TLS install, and bootstrap register/stack materialization.
#[derive(Clone, Copy)]
enum ElfImageType {
    Executable,
    StaticPie,
}

#[derive(Clone, Copy)]
struct ElfHeaderInfo {
    image_type: ElfImageType,
    entry_point: u64,
    program_header_offset: u64,
    program_header_entry_size: u64,
    program_header_count: u64,
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
    page_file_offset: usize,
    page_file_end: usize,
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

#[inline(never)]
pub(super) fn load_elf(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
    validate_elf_kind(image)?;
    let header = validate_elf_header(image)?;
    let elf = ElfFile::new(image).map_err(ProcessLoadError::InvalidElf)?;
    let interpreter_path = elf_interpreter_path(image, &elf)?;
    let runtime_search_paths = elf_runtime_search_paths(image, &elf)?;
    let load_bias = choose_elf_load_bias(&elf, header.image_type, ELF_DYN_LOAD_BASE)?;
    let mut address_space = ProcessAddressSpace::new()?;
    let mut image_mappings = Vec::new();

    let mut loaded_segments = 0usize;
    let mut mapped_page_ranges = [(0_u64, 0_u64); MAX_LOAD_SEGMENTS];
    let main_image = map_elf_image(
        &elf,
        &header,
        image,
        header.image_type,
        load_bias,
        LinuxImageMappingPathKind::Executable,
        &mut address_space,
        &mut mapped_page_ranges,
        &mut loaded_segments,
        &mut image_mappings,
        interpreter_path.is_none(),
    )?;

    let interpreter_path_owned = interpreter_path.clone();
    let (entry, interpreter_base, max_loaded_end) =
        if let Some(interpreter_path) = interpreter_path.as_deref() {
            let interpreter_image = load_interpreter_image(interpreter_path)?;
            validate_elf_kind(interpreter_image.as_slice())?;
            let interpreter_header = validate_elf_header(interpreter_image.as_slice())?;
            let interpreter_elf =
                ElfFile::new(interpreter_image.as_slice()).map_err(ProcessLoadError::InvalidElf)?;
            ensure_no_elf_interpreter(&interpreter_elf)?;
            let interpreter_load_bias = choose_elf_load_bias(
                &interpreter_elf,
                interpreter_header.image_type,
                ELF_INTERP_LOAD_BASE,
            )?;
            let interpreter = map_elf_image(
                &interpreter_elf,
                &interpreter_header,
                interpreter_image.as_slice(),
                interpreter_header.image_type,
                interpreter_load_bias,
                LinuxImageMappingPathKind::Interpreter,
                &mut address_space,
                &mut mapped_page_ranges,
                &mut loaded_segments,
                &mut image_mappings,
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

    let mut linux_image = build_linux_process_image(
        &elf,
        &header,
        image,
        main_image.load_bias,
        max_loaded_end,
        main_image.entry.as_u64(),
        interpreter_base,
        interpreter_path_owned,
        image_mappings,
        runtime_search_paths,
    )?;

    if linux_image.interpreter_path.is_none() {
        reserve_bootstrap_heap(&mut address_space, &mut linux_image)?;
    }

    Ok(LoadedProcessImage {
        abi: UserAbi::Linux,
        address_space,
        entry,
        runtime: LoadedProcessRuntime::Linux(linux_image),
    })
}

fn validate_elf_kind(image: &[u8]) -> Result<(), ProcessLoadError> {
    match FileKind::parse(image).map_err(|_| ProcessLoadError::InvalidElf("invalid ELF image"))? {
        FileKind::Elf64 => Ok(()),
        _ => Err(ProcessLoadError::InvalidElf("ELF image is not 64-bit")),
    }
}

fn validate_elf_header(image: &[u8]) -> Result<ElfHeaderInfo, ProcessLoadError> {
    let header = read_raw_elf_header(image)?;
    if header.e_ident.magic != objelf::ELFMAG {
        return Err(ProcessLoadError::InvalidElf("invalid ELF magic"));
    }
    if header.e_ident.class != objelf::ELFCLASS64 {
        return Err(ProcessLoadError::InvalidElf("ELF class is not 64-bit"));
    }
    if header.e_ident.data != objelf::ELFDATA2LSB {
        return Err(ProcessLoadError::InvalidElf(
            "ELF endianness is not little-endian",
        ));
    }
    if header.e_ident.version != objelf::EV_CURRENT {
        return Err(ProcessLoadError::InvalidElf("ELF ident version is invalid"));
    }
    if header.e_version.get(ELF_ENDIAN) != objelf::EV_CURRENT as u32 {
        return Err(ProcessLoadError::InvalidElf("ELF version is invalid"));
    }
    if header.e_machine.get(ELF_ENDIAN) != objelf::EM_X86_64 {
        return Err(ProcessLoadError::InvalidElf("ELF machine is not x86_64"));
    }
    if usize::from(header.e_ehsize.get(ELF_ENDIAN)) != ELF64_HEADER_SIZE {
        return Err(ProcessLoadError::InvalidElf("ELF header size is invalid"));
    }
    if u64::from(header.e_phentsize.get(ELF_ENDIAN)) != ELF64_PROGRAM_HEADER_SIZE {
        return Err(ProcessLoadError::InvalidElf(
            "ELF program header size is invalid",
        ));
    }

    let image_type = match header.e_type.get(ELF_ENDIAN) {
        objelf::ET_EXEC => ElfImageType::Executable,
        objelf::ET_DYN => ElfImageType::StaticPie,
        _ => Err(ProcessLoadError::InvalidElf(
            "ELF type is not executable or static PIE",
        ))?,
    };

    Ok(ElfHeaderInfo {
        image_type,
        entry_point: header.e_entry.get(ELF_ENDIAN),
        program_header_offset: header.e_phoff.get(ELF_ENDIAN),
        program_header_entry_size: u64::from(header.e_phentsize.get(ELF_ENDIAN)),
        program_header_count: u64::from(header.e_phnum.get(ELF_ENDIAN)),
    })
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
            Ok(load_bias)
        }
    }
}

fn validate_entry_point(
    header: &ElfHeaderInfo,
    load_bias: u64,
) -> Result<VirtAddr, ProcessLoadError> {
    let entry = header
        .entry_point
        .checked_add(load_bias)
        .ok_or(ProcessLoadError::InvalidElf("entry point address overflow"))?;
    if !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&entry) {
        return Err(ProcessLoadError::InvalidElf(
            "entry point is outside the supported user range",
        ));
    }
    Ok(VirtAddr::new(entry))
}

fn elf_interpreter_path(
    image: &[u8],
    elf: &ElfFile<'_>,
) -> Result<Option<String>, ProcessLoadError> {
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
        let raw_path = core::str::from_utf8(&bytes[..nul])
            .map_err(|_| ProcessLoadError::InvalidElf("PT_INTERP path is not valid UTF-8"))?;
        let normalized = vfs::normalize_kernel_path(raw_path)
            .map_err(|_| ProcessLoadError::InvalidElf("PT_INTERP path is invalid"))?;
        return Ok(Some(normalized));
    }

    Ok(None)
}

fn map_elf_image(
    elf: &ElfFile<'_>,
    header: &ElfHeaderInfo,
    image: &[u8],
    image_type: ElfImageType,
    load_bias: u64,
    path_kind: LinuxImageMappingPathKind,
    address_space: &mut ProcessAddressSpace,
    mapped_page_ranges: &mut [(u64, u64); MAX_LOAD_SEGMENTS],
    loaded_segments: &mut usize,
    image_mappings: &mut Vec<LinuxImageMapping>,
    apply_kernel_relocations: bool,
) -> Result<MappedElfImage, ProcessLoadError> {
    let entry = validate_entry_point(header, load_bias)?;
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
        address_space.map_zeroed_user_pages_at(
            VirtAddr::new(segment.page_base),
            page_count,
            page_flags,
        )?;
        if segment.page_file_offset != segment.page_file_end {
            address_space.initialize_user_bytes(
                VirtAddr::new(segment.page_base),
                &image[segment.page_file_offset..segment.page_file_end],
            )?;
        }
        address_space.ensure_user_region_mapped(VirtAddr::new(segment.page_base), page_count)?;
        image_mappings.push(LinuxImageMapping {
            start: segment.page_base,
            end: segment.page_end,
            offset: align_down(ph.offset(), PAGE_SIZE),
            flags: LinuxVmaFlags::new(
                ph.flags().is_read(),
                ph.flags().is_write(),
                ph.flags().is_execute(),
                true,
            ),
            path_kind,
        });
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
    let (page_file_offset, page_file_end) = segment_page_file_copy_range(file_offset, file_size)?;

    Ok(SegmentLoadInfo {
        addr,
        end,
        page_base,
        page_end,
        page_file_offset,
        page_file_end,
    })
}

fn segment_page_file_copy_range(
    file_offset: usize,
    file_size: usize,
) -> Result<(usize, usize), ProcessLoadError> {
    let page_file_offset = usize::try_from(align_down(file_offset as u64, PAGE_SIZE))
        .map_err(|_| ProcessLoadError::InvalidElf("segment page file offset out of range"))?;
    let page_file_end = if file_size == 0 {
        page_file_offset
    } else {
        file_offset
            .checked_add(file_size)
            .ok_or(ProcessLoadError::InvalidElf(
                "segment page-backed file bounds overflow",
            ))?
    };
    Ok((page_file_offset, page_file_end))
}

fn build_linux_process_image(
    elf: &ElfFile<'_>,
    header: &ElfHeaderInfo,
    image: &[u8],
    load_bias: u64,
    max_loaded_end: u64,
    entry: u64,
    interpreter_base: u64,
    interpreter_path: Option<String>,
    image_mappings: Vec<LinuxImageMapping>,
    runtime_search_paths: Vec<String>,
) -> Result<LinuxProcessImageInfo, ProcessLoadError> {
    let program_headers = program_header_table_addr(elf, header, load_bias)?;
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
        interpreter_path,
        program_headers,
        program_header_entry_size: header.program_header_entry_size,
        program_header_count: header.program_header_count,
        brk_start,
        bootstrap_heap_base: 0,
        bootstrap_heap_len: 0,
        initial_tls,
        image_mappings,
        runtime_search_paths,
    })
}

/// Pre-map a fixed bootstrap heap region for static-PIE policy services so
/// `rustos-svc-runtime` can hand the address out to its bump allocator before
/// any of the dynamic Linux runtime (and thus syscalld/vfsd) is available.
/// Modeled on the seL4 BootInfo pattern where the kernel hands the root task
/// pre-existing memory caps. Idempotent on failure: errors propagate as
/// `ProcessLoadError` and leave the address space unchanged.
fn reserve_bootstrap_heap(
    address_space: &mut ProcessAddressSpace,
    image: &mut LinuxProcessImageInfo,
) -> Result<(), ProcessLoadError> {
    use rustos_user_abi::syscall::RUSTOS_BOOTSTRAP_HEAP_DEFAULT_LEN;
    const HEAP_GAP: u64 = 16 * 1024 * 1024;
    const PAGE_SIZE_U64: u64 = 4096;

    let heap_len = RUSTOS_BOOTSTRAP_HEAP_DEFAULT_LEN;
    let heap_base = align_up(image.brk_start.saturating_add(HEAP_GAP), PAGE_SIZE_U64)
        .ok_or(ProcessLoadError::InvalidElf("bootstrap heap base overflow"))?;
    if heap_base.checked_add(heap_len).is_none() {
        return Err(ProcessLoadError::InvalidElf(
            "bootstrap heap range overflow",
        ));
    }
    let page_count = (heap_len / PAGE_SIZE_U64) as usize;
    let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    address_space
        .map_zeroed_user_pages_at(VirtAddr::new(heap_base), page_count, flags)
        .map_err(|_| ProcessLoadError::InvalidElf("bootstrap heap mapping failed"))?;

    image.bootstrap_heap_base = heap_base;
    image.bootstrap_heap_len = heap_len;
    Ok(())
}

pub(super) fn build_runtime_profile(
    image: &LinuxProcessImageInfo,
    launch: LinuxProcessLaunch<'_>,
) -> LinuxRuntimeProfile {
    let mut profile = LinuxRuntimeProfile::new();

    for entry in &image.runtime_search_paths {
        for path in expand_runtime_search_entry(entry.as_str(), launch.exec_path) {
            profile.allow_loader_search_dir(path.as_str());
            if runtime_access_dir_allowed(path.as_str(), launch.exec_path) {
                profile.allow_kernel_runtime_access_dir(path.as_str());
            }
        }
    }

    if let Some(value) = linux_env_value(launch.env, "LD_LIBRARY_PATH") {
        for entry in value.split(':') {
            for path in expand_runtime_search_entry(entry, launch.exec_path) {
                profile.allow_loader_search_dir(path.as_str());
            }
        }
    }

    profile
}

pub(super) fn build_initial_memory_map(
    image: &LinuxProcessImageInfo,
    exec_path: &str,
    user_stack: Option<UserStackState>,
) -> LinuxMemoryMapState {
    let mut maps = LinuxMemoryMapState::new();

    for mapping in &image.image_mappings {
        let name = match mapping.path_kind {
            LinuxImageMappingPathKind::None => LinuxVmaName::None,
            LinuxImageMappingPathKind::Executable if exec_path.is_empty() => LinuxVmaName::None,
            LinuxImageMappingPathKind::Executable => LinuxVmaName::Path(String::from(exec_path)),
            LinuxImageMappingPathKind::Interpreter => image
                .interpreter_path
                .as_ref()
                .map(|path| LinuxVmaName::Path(path.clone()))
                .unwrap_or(LinuxVmaName::None),
        };
        let area = LinuxVma::new(
            mapping.start,
            mapping.end,
            mapping.offset,
            mapping.flags,
            name,
        )
        .expect("initial ELF VMA bounds are invalid");
        maps.insert_area(area)
            .expect("initial ELF VMA ranges unexpectedly overlap");
    }

    if let Some(tls) = image.initial_tls {
        let tls_end = tls.mapping_base.saturating_add(tls.mapping_size);
        if let Some(area) = LinuxVma::new(
            tls.mapping_base,
            tls_end,
            0,
            LinuxVmaFlags::private_anon(true, true, false),
            LinuxVmaName::None,
        ) {
            maps.insert_area(area)
                .expect("initial TLS mapping overlaps an existing VMA");
        }
    }

    if let Some(stack) = user_stack {
        if let Some(area) = LinuxVma::new(
            stack.reserve_start,
            stack.reserve_end,
            0,
            LinuxVmaFlags::private_anon(true, true, false),
            LinuxVmaName::Label("[stack]"),
        ) {
            maps.insert_area(area)
                .expect("initial stack mapping overlaps an existing VMA");
        }
    }

    maps
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

fn program_header_table_addr(
    elf: &ElfFile<'_>,
    header: &ElfHeaderInfo,
    load_bias: u64,
) -> Result<u64, ProcessLoadError> {
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

    let ph_offset = header.program_header_offset;
    let ph_size = header
        .program_header_entry_size
        .checked_mul(header.program_header_count)
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

fn runtime_access_dir_allowed(path: &str, exec_path: &str) -> bool {
    is_under_trusted_runtime_root(path) || is_exec_private_runtime_dir(path, exec_path)
}

fn is_under_trusted_runtime_root(path: &str) -> bool {
    ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
        .iter()
        .any(|root| path == *root || path_is_under_directory(path, root))
}

fn is_exec_private_runtime_dir(path: &str, exec_path: &str) -> bool {
    let Some(exec_dir) = exec_path.rsplit_once('/').map(|(dir, _)| dir) else {
        return false;
    };
    path == exec_dir || path_is_under_directory(path, exec_dir)
}

fn path_is_under_directory(path: &str, directory: &str) -> bool {
    if directory == "/" {
        return path.starts_with('/') && path.len() > 1;
    }

    path.strip_prefix(directory)
        .map(|suffix| suffix.starts_with('/'))
        .unwrap_or(false)
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

fn elf_runtime_search_paths(
    image: &[u8],
    elf: &ElfFile<'_>,
) -> Result<Vec<String>, ProcessLoadError> {
    let Some(dynamic_segment) = elf.program_iter().find(|ph| {
        ph.get_type()
            .map(|kind| kind == ProgramType::Dynamic)
            .unwrap_or(false)
    }) else {
        return Ok(Vec::new());
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

    let mut strtab_addr = 0_u64;
    let mut strtab_size = 0_u64;
    let mut rpath_offset = None;
    let mut runpath_offset = None;
    for entry in entries {
        match entry.get_tag().map_err(ProcessLoadError::InvalidElf)? {
            DynamicTag::StrTab => {
                strtab_addr = entry.get_ptr().map_err(ProcessLoadError::InvalidElf)?;
            }
            DynamicTag::StrSize => {
                strtab_size = entry.get_val().map_err(ProcessLoadError::InvalidElf)?;
            }
            DynamicTag::RPath => {
                rpath_offset = Some(entry.get_val().map_err(ProcessLoadError::InvalidElf)?);
            }
            DynamicTag::RunPath => {
                runpath_offset = Some(entry.get_val().map_err(ProcessLoadError::InvalidElf)?);
            }
            DynamicTag::Null => break,
            _ => {}
        }
    }

    let Some(search_offset) = runpath_offset.or(rpath_offset) else {
        return Ok(Vec::new());
    };
    if strtab_addr == 0 || strtab_size == 0 {
        return Err(ProcessLoadError::InvalidElf(
            "dynamic string table metadata is incomplete",
        ));
    }

    let strtab = elf_file_slice_from_virtual_address(
        image,
        elf,
        strtab_addr,
        usize::try_from(strtab_size)
            .map_err(|_| ProcessLoadError::InvalidElf("dynamic string table is too large"))?,
    )?;
    let encoded = elf_dynamic_string(
        strtab,
        usize::try_from(search_offset)
            .map_err(|_| ProcessLoadError::InvalidElf("runtime search path offset is invalid"))?,
    )?;

    let mut paths = Vec::new();
    for entry in encoded.split(':') {
        let entry = entry.trim();
        if entry.is_empty() || paths.iter().any(|current| current == entry) {
            continue;
        }
        paths.push(entry.to_string());
    }
    Ok(paths)
}

fn elf_dynamic_string<'a>(strtab: &'a [u8], offset: usize) -> Result<&'a str, ProcessLoadError> {
    let bytes = strtab.get(offset..).ok_or(ProcessLoadError::InvalidElf(
        "runtime search path offset is outside the string table",
    ))?;
    let Some(len) = bytes.iter().position(|&byte| byte == 0) else {
        return Err(ProcessLoadError::InvalidElf(
            "dynamic string table entry is not terminated",
        ));
    };
    core::str::from_utf8(&bytes[..len])
        .map_err(|_| ProcessLoadError::InvalidElf("dynamic string table entry is not valid UTF-8"))
}

fn linux_env_value<'a>(env: &'a [&'a str], key: &str) -> Option<&'a str> {
    env.iter().find_map(|entry| {
        let (name, value) = entry.split_once('=')?;
        if name == key {
            Some(value)
        } else {
            None
        }
    })
}

fn expand_runtime_search_entry(entry: &str, exec_path: &str) -> Vec<String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Vec::new();
    }

    let origin = runtime_search_origin(exec_path);
    let expanded = entry.replace("${ORIGIN}", origin.as_str());
    let expanded = expanded.replace("$ORIGIN", origin.as_str());
    let Some(path) = normalize_absolute_runtime_path(expanded.as_str()) else {
        return Vec::new();
    };
    vec![path]
}

fn runtime_search_origin(exec_path: &str) -> String {
    if exec_path == "/" || exec_path.is_empty() {
        return String::from("/");
    }
    match exec_path.rsplit_once('/') {
        Some(("", _)) => String::from("/"),
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        _ => String::from("/"),
    }
}

fn normalize_absolute_runtime_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }

    let mut components = Vec::new();
    for component in trimmed.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        components.push(component);
    }

    let mut normalized = String::from("/");
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Some(normalized)
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

fn read_raw_elf_header(image: &[u8]) -> Result<RawElfHeader<LittleEndian>, ProcessLoadError> {
    let bytes: &[u8; ELF64_HEADER_SIZE] = image
        .get(..ELF64_HEADER_SIZE)
        .ok_or(ProcessLoadError::InvalidElf("ELF header is truncated"))?
        .try_into()
        .map_err(|_| ProcessLoadError::InvalidElf("ELF header is truncated"))?;
    Ok(unsafe { ptr::read_unaligned(bytes.as_ptr() as *const RawElfHeader<LittleEndian>) })
}
// RING3-MIGRATION-REFERENCE END: loaderd/procd/syscalld-owned Linux ELF load policy.

pub(super) fn initialize_linux_initial_tls(
    address_space: &mut ProcessAddressSpace,
    image: &LinuxProcessImageInfo,
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
    image: &LinuxProcessImageInfo,
    launch: LinuxProcessLaunch<'_>,
    security: ProcessSecurityContext,
) -> Result<VirtAddr, ProcessLoadError> {
    // RING3-MIGRATION-REFERENCE START: procd/syscalld should own Linux initial
    // stack and auxv policy, including argv/env layout, credential aux entries,
    // hwcap/defaults, and RustOS-private aux values. Ring0 keeps the final
    // current-address-space byte writes needed to materialize the prepared
    // bootstrap image.
    let aligned_top = align_down(stack_end.as_u64(), 16);
    let mut cursor = aligned_top;
    let mut random_bytes = [0_u8; LINUX_STACK_RANDOM_BYTES];
    nucleus_core::util::random::Random::new().fill_bytes(&mut random_bytes);
    let random_addr = push_stack_bytes(
        address_space,
        &mut cursor,
        &random_bytes,
        16,
        "linux AT_RANDOM placement overflow",
    )?;

    let exec_path = launch.exec_path;
    let platform_addr = push_stack_c_string(
        address_space,
        &mut cursor,
        LINUX_AUX_PLATFORM,
        "linux platform string placement overflow",
    )?;
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

    let argv_ptrs = push_stack_c_string_list(
        address_space,
        &mut cursor,
        launch.argv,
        "linux argv string placement overflow",
    )?;

    let stack_words = build_linux_initial_stack_words(
        image,
        &argv_ptrs,
        &env_ptrs,
        random_addr,
        execfn_addr,
        platform_addr,
        security,
    );
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

fn build_linux_initial_stack_words(
    image: &LinuxProcessImageInfo,
    argv_ptrs: &[u64],
    env_ptrs: &[u64],
    random_addr: u64,
    execfn_addr: u64,
    platform_addr: u64,
    security: ProcessSecurityContext,
) -> Vec<u64> {
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
        security.uid() as u64,
        linux_abi::AT_EUID,
        security.euid() as u64,
        linux_abi::AT_GID,
        security.gid() as u64,
        linux_abi::AT_EGID,
        security.egid() as u64,
        linux_abi::AT_CLKTCK,
        LINUX_STACK_CLOCK_TICKS,
        linux_abi::AT_SECURE,
        0,
        linux_abi::AT_RANDOM,
        random_addr,
        linux_abi::AT_PLATFORM,
        platform_addr,
        linux_abi::AT_HWCAP,
        LINUX_AUX_HWCAP,
        linux_abi::AT_HWCAP2,
        LINUX_AUX_HWCAP2,
        linux_abi::AT_EXECFN,
        execfn_addr,
        rustos_user_abi::syscall::AT_RUSTOS_BOOTSTRAP_HEAP_BASE,
        image.bootstrap_heap_base,
        rustos_user_abi::syscall::AT_RUSTOS_BOOTSTRAP_HEAP_LEN,
        image.bootstrap_heap_len,
        linux_abi::AT_NULL,
        0,
    ];

    let mut stack_words = Vec::with_capacity(2 + argv_ptrs.len() + env_ptrs.len() + auxv.len());
    stack_words.push(argv_ptrs.len() as u64);
    stack_words.extend_from_slice(argv_ptrs);
    stack_words.push(0);
    stack_words.extend_from_slice(env_ptrs);
    stack_words.push(0);
    stack_words.extend_from_slice(&auxv);
    stack_words
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
// RING3-MIGRATION-REFERENCE END: procd/syscalld-owned Linux initial stack policy.

fn make_interpreter_load_error(path: &str, error: vfs::VfsError) -> ProcessLoadError {
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

fn load_interpreter_image(path: &str) -> Result<Vec<u8>, ProcessLoadError> {
    vfs::read_path_to_vec_for_kernel(path).map_err(|error| make_interpreter_load_error(path, error))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{
        build_linux_initial_stack_words, build_runtime_profile, elf_initial_tls_info_from_template,
        expand_runtime_search_entry, segment_page_file_copy_range, ElfTlsTemplateInfo,
        ELF_DYN_LOAD_BASE, INITIAL_TLS_TCB_ALIGN, INITIAL_TLS_TCB_SIZE, LINUX_AUX_HWCAP,
        LINUX_AUX_HWCAP2,
    };
    use crate::memory::paging::USER_SPACE_BASE;
    use crate::user::linux as linux_abi;
    use crate::user::linux::LinuxProcessImageInfo;
    use crate::user::process_state::ProcessSecurityContext;

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

    #[test]
    fn linux_initial_stack_words_preserve_empty_argv() {
        let image = LinuxProcessImageInfo {
            entry: 0x401000,
            interpreter_base: 0x7f000000,
            interpreter_path: None,
            program_headers: 0x400040,
            program_header_entry_size: 56,
            program_header_count: 9,
            brk_start: 0x500000,
            bootstrap_heap_base: 0,
            bootstrap_heap_len: 0,
            initial_tls: None,
            image_mappings: Vec::new(),
            runtime_search_paths: Vec::new(),
        };
        let env_ptrs = [0x9000_u64];
        let words = build_linux_initial_stack_words(
            &image,
            &[],
            &env_ptrs,
            0x7000,
            0x8000,
            0x8100,
            ProcessSecurityContext::new(false),
        );

        assert_eq!(words[0], 0);
        assert_eq!(words[1], 0);
        assert_eq!(words[2], env_ptrs[0]);
        assert_eq!(words[3], 0);

        let auxv = &words[4..];
        let execfn_index = auxv
            .chunks_exact(2)
            .position(|pair| pair[0] == linux_abi::AT_EXECFN)
            .expect("AT_EXECFN present");
        assert_eq!(auxv[execfn_index * 2 + 1], 0x8000);

        let platform_index = auxv
            .chunks_exact(2)
            .position(|pair| pair[0] == linux_abi::AT_PLATFORM)
            .expect("AT_PLATFORM present");
        assert_eq!(auxv[platform_index * 2 + 1], 0x8100);

        let hwcap_index = auxv
            .chunks_exact(2)
            .position(|pair| pair[0] == linux_abi::AT_HWCAP)
            .expect("AT_HWCAP present");
        assert_eq!(auxv[hwcap_index * 2 + 1], LINUX_AUX_HWCAP);

        let hwcap2_index = auxv
            .chunks_exact(2)
            .position(|pair| pair[0] == linux_abi::AT_HWCAP2)
            .expect("AT_HWCAP2 present");
        assert_eq!(auxv[hwcap2_index * 2 + 1], LINUX_AUX_HWCAP2);

        let uid_index = auxv
            .chunks_exact(2)
            .position(|pair| pair[0] == linux_abi::AT_UID)
            .expect("AT_UID present");
        assert_eq!(auxv[uid_index * 2 + 1], 1000);

        let egid_index = auxv
            .chunks_exact(2)
            .position(|pair| pair[0] == linux_abi::AT_EGID)
            .expect("AT_EGID present");
        assert_eq!(auxv[egid_index * 2 + 1], 1000);
    }

    #[test]
    fn segment_page_copy_range_preserves_prefix_without_copying_bss_tail() {
        assert_eq!(
            segment_page_file_copy_range(0x123, 0x2000).expect("copy range"),
            (0x0, 0x2123)
        );
        assert_eq!(
            segment_page_file_copy_range(0x1123, 0).expect("empty copy range"),
            (0x1000, 0x1000)
        );
    }

    #[test]
    fn runtime_search_entries_expand_origin_and_ignore_relative_paths() {
        assert_eq!(
            expand_runtime_search_entry("$ORIGIN/../lib", "/services/uiserver/uiserver.elf"),
            vec![String::from("/services/lib")]
        );
        assert!(expand_runtime_search_entry("relative/lib", "/system/app.elf").is_empty());
    }

    #[test]
    fn runtime_profile_uses_runpath_and_ld_library_path() {
        let image = LinuxProcessImageInfo {
            entry: 0,
            interpreter_base: 0,
            interpreter_path: None,
            program_headers: 0,
            program_header_entry_size: 0,
            program_header_count: 0,
            brk_start: 0,
            bootstrap_heap_base: 0,
            bootstrap_heap_len: 0,
            initial_tls: None,
            image_mappings: Vec::new(),
            runtime_search_paths: vec![String::from("$ORIGIN/../lib")],
        };

        let profile = build_runtime_profile(
            &image,
            linux_abi::LinuxProcessLaunch {
                exec_path: "/services/uiserver/uiserver.elf",
                argv: &[],
                env: &["LD_LIBRARY_PATH=/opt/rustos/lib:/tmp/relative"],
            },
        );

        assert_eq!(
            profile.loader_search_dirs(),
            &[
                String::from("/services/lib"),
                String::from("/opt/rustos/lib"),
                String::from("/tmp/relative"),
            ]
        );
        assert!(profile.kernel_runtime_access_dirs().is_empty());
    }

    #[test]
    fn runtime_profile_only_grants_kernel_access_to_trusted_or_private_dirs() {
        let image = LinuxProcessImageInfo {
            entry: 0,
            interpreter_base: 0,
            interpreter_path: None,
            program_headers: 0,
            program_header_entry_size: 0,
            program_header_count: 0,
            brk_start: 0,
            bootstrap_heap_base: 0,
            bootstrap_heap_len: 0,
            initial_tls: None,
            image_mappings: Vec::new(),
            runtime_search_paths: vec![
                String::from("$ORIGIN/lib"),
                String::from("$ORIGIN/../lib"),
                String::from("/usr/lib/custom"),
            ],
        };

        let profile = build_runtime_profile(
            &image,
            linux_abi::LinuxProcessLaunch {
                exec_path: "/services/uiserver/uiserver.elf",
                argv: &[],
                env: &["LD_LIBRARY_PATH=/etc:/opt/private"],
            },
        );

        assert_eq!(
            profile.loader_search_dirs(),
            &[
                String::from("/services/uiserver/lib"),
                String::from("/services/lib"),
                String::from("/usr/lib/custom"),
                String::from("/etc"),
                String::from("/opt/private"),
            ]
        );
        assert_eq!(
            profile.kernel_runtime_access_dirs(),
            &[
                String::from("/services/uiserver/lib"),
                String::from("/usr/lib/custom"),
            ]
        );
    }
}
