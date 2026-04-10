use core::cmp::min;
use core::convert::TryFrom;
use core::ptr;

use object::elf::{
    self as objelf, FileHeader64 as RawElfHeader, ProgramHeader64 as RawProgramHeader,
};
use object::{FileKind, LittleEndian};
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::proto::media::file::RegularFile;

use crate::error::BootError;

const ELF64_HEADER_SIZE: usize = 64;
const ELF64_PROGRAM_HEADER_SIZE: usize = 56;
const MAX_PROGRAM_HEADERS: usize = 32;
const PAGE_SIZE: usize = 0x1000;
const MIN_KERNEL_LOAD_ADDR: usize = 0x0020_0000;
const MAX_KERNEL_LOAD_END_EXCLUSIVE: usize = 512 * 1024 * 1024 * 1024;
const LOAD_CHUNK_SIZE: usize = 4096;
const ELF64_DYNAMIC_ENTRY_SIZE: usize = 16;
const ELF64_RELA_ENTRY_SIZE: usize = 24;
const MAX_DYNAMIC_RELOCATIONS: usize = 262144;
const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const R_X86_64_RELATIVE: u32 = 8;
const ELF_ENDIAN: LittleEndian = LittleEndian;

pub(crate) struct UefiKernelFile {
    file: RegularFile,
}

impl UefiKernelFile {
    pub(crate) const fn new(file: RegularFile) -> Self {
        Self { file }
    }

    fn seek(&mut self, offset: u64, error_message: &'static str) -> Result<(), BootError> {
        self.file.set_position(offset).map_err(|err| {
            let _ = error_message;
            BootError::ReadKernel(err.status())
        })
    }

    fn read_exact(
        &mut self,
        mut buffer: &mut [u8],
        error_message: &'static str,
    ) -> Result<(), BootError> {
        while !buffer.is_empty() {
            let read = self.file.read(buffer).map_err(|err| {
                let _ = error_message;
                BootError::ReadKernel(err.status())
            })?;
            if read == 0 {
                return Err(BootError::InvalidElf(error_message));
            }
            buffer = &mut buffer[read..];
        }
        Ok(())
    }
}

pub(crate) fn load_kernel_elf(
    reader: &mut UefiKernelFile,
    file_len: u64,
    physical_slide: usize,
) -> Result<(usize, usize, usize, usize, usize), BootError> {
    let mut elf_header = [0_u8; ELF64_HEADER_SIZE];
    read_exact_at(reader, 0, &mut elf_header, "failed to read ELF header")?;

    let header = parse_elf_header(&elf_header)?;
    validate_elf_header(&elf_header, &header, file_len)?;

    let ph_table_size = header
        .program_header_count
        .checked_mul(header.program_header_size)
        .ok_or(BootError::InvalidElf("program header table size overflow"))?;
    let mut program_headers = [0_u8; ELF64_PROGRAM_HEADER_SIZE * MAX_PROGRAM_HEADERS];
    read_exact_at(
        reader,
        header.program_header_offset,
        &mut program_headers[..ph_table_size],
        "failed to read program header table",
    )?;

    let program_headers = parse_program_headers(
        &program_headers[..ph_table_size],
        header.program_header_count,
    )?;
    let load_footprint =
        kernel_load_footprint(&header, &program_headers[..header.program_header_count])?;
    let allocation = allocate_kernel_image(&header, &load_footprint, physical_slide)?;
    let load_bias = allocation.load_bias;
    let entry_point = relocate_image_addr(&header, header.entry_point, load_bias)?;
    validate_kernel_entry(entry_point)?;

    let mut loaded_segments = 0usize;
    let mut executable_entry_covered = false;
    let mut loaded_ranges = [(0usize, 0usize); MAX_PROGRAM_HEADERS];
    let mut loaded_range_count = 0usize;
    let mut dynamic_segment = None;

    for ph in program_headers.iter() {
        if ph.ty == objelf::PT_DYNAMIC {
            if dynamic_segment.is_some() {
                return Err(BootError::InvalidElf(
                    "ELF contains multiple PT_DYNAMIC segments",
                ));
            }
            dynamic_segment = Some(dynamic_segment_info(&header, ph, load_bias)?);
            continue;
        }
        if ph.ty != objelf::PT_LOAD {
            continue;
        }

        let segment = validated_segment_bounds(&header, ph, file_len, load_bias)?;
        reject_overlapping_segment(
            segment.addr,
            segment.end,
            &loaded_ranges[..loaded_range_count],
        )?;
        loaded_ranges[loaded_range_count] = (segment.addr, segment.end);
        loaded_range_count += 1;

        if ph.flags & objelf::PF_X != 0 && (segment.addr..segment.end).contains(&entry_point) {
            executable_entry_covered = true;
        }

        load_segment(reader, &segment)?;
        loaded_segments += 1;
    }

    if loaded_segments == 0 {
        return Err(BootError::InvalidElf("no PT_LOAD segments"));
    }
    if !executable_entry_covered {
        return Err(BootError::InvalidElf(
            "entry point is not inside an executable PT_LOAD segment",
        ));
    }
    if header.elf_type == objelf::ET_DYN {
        let dynamic_segment =
            dynamic_segment.ok_or(BootError::InvalidElf("ET_DYN image is missing PT_DYNAMIC"))?;
        apply_dynamic_relocations(
            &header,
            &dynamic_segment,
            load_bias,
            &loaded_ranges[..loaded_range_count],
        )?;
    }

    Ok((
        entry_point,
        loaded_segments,
        load_bias,
        allocation.phys_start,
        allocation.size,
    ))
}

#[derive(Clone, Copy)]
struct ElfHeader {
    elf_type: u16,
    entry_point: u64,
    program_header_offset: u64,
    program_header_size: usize,
    program_header_count: usize,
}

#[derive(Clone, Copy, Default)]
struct ProgramHeader {
    ty: u32,
    flags: u32,
    offset: u64,
    virtual_addr: u64,
    physical_addr: u64,
    file_size: u64,
    mem_size: u64,
    align: u64,
}

#[derive(Clone, Copy, Debug)]
struct SegmentLoadInfo {
    addr: usize,
    end: usize,
    page_base: usize,
    page_end: usize,
    file_offset: u64,
    file_size: usize,
}

#[derive(Clone, Copy, Debug)]
struct DynamicSegmentInfo {
    addr: usize,
    size: usize,
}

#[derive(Clone, Copy, Debug)]
struct DynamicRelocationTable {
    addr: usize,
    size: usize,
}

#[derive(Clone, Copy)]
struct KernelLoadFootprint {
    start: usize,
    end: usize,
    alignment: usize,
}

#[derive(Clone, Copy)]
struct KernelLoadAllocation {
    phys_start: usize,
    size: usize,
    load_bias: usize,
}

fn parse_elf_header(header: &[u8; ELF64_HEADER_SIZE]) -> Result<ElfHeader, BootError> {
    let header = read_raw_elf_header(header);
    Ok(ElfHeader {
        elf_type: header.e_type.get(ELF_ENDIAN),
        entry_point: header.e_entry.get(ELF_ENDIAN),
        program_header_offset: header.e_phoff.get(ELF_ENDIAN),
        program_header_size: usize::from(header.e_phentsize.get(ELF_ENDIAN)),
        program_header_count: usize::from(header.e_phnum.get(ELF_ENDIAN)),
    })
}

fn validate_elf_header(
    raw_header: &[u8; ELF64_HEADER_SIZE],
    header: &ElfHeader,
    file_len: u64,
) -> Result<(), BootError> {
    let raw = read_raw_elf_header(raw_header);
    match FileKind::parse(&raw_header[..16])
        .map_err(|_| BootError::InvalidElf("invalid ELF image"))?
    {
        FileKind::Elf64 => {}
        _ => return Err(BootError::InvalidElf("ELF image is not 64-bit")),
    }
    if raw.e_ident.magic != objelf::ELFMAG {
        return Err(BootError::InvalidElf("invalid ELF magic"));
    }
    if raw.e_ident.class != objelf::ELFCLASS64 {
        return Err(BootError::InvalidElf("ELF class is not 64-bit"));
    }
    if raw.e_ident.data != objelf::ELFDATA2LSB {
        return Err(BootError::InvalidElf("ELF endianness is not little-endian"));
    }
    if raw.e_ident.version != objelf::EV_CURRENT {
        return Err(BootError::InvalidElf("ELF ident version is invalid"));
    }
    if raw.e_version.get(ELF_ENDIAN) != objelf::EV_CURRENT as u32 {
        return Err(BootError::InvalidElf("ELF version is invalid"));
    }
    if !matches!(header.elf_type, objelf::ET_EXEC | objelf::ET_DYN) {
        return Err(BootError::InvalidElf(
            "ELF type is not executable/shared object",
        ));
    }
    if raw.e_machine.get(ELF_ENDIAN) != objelf::EM_X86_64 {
        return Err(BootError::InvalidElf("ELF machine is not x86_64"));
    }
    if usize::from(raw.e_ehsize.get(ELF_ENDIAN)) != ELF64_HEADER_SIZE {
        return Err(BootError::InvalidElf("ELF header size is invalid"));
    }
    if header.program_header_size != ELF64_PROGRAM_HEADER_SIZE {
        return Err(BootError::InvalidElf("ELF program header size is invalid"));
    }
    if header.program_header_count == 0 {
        return Err(BootError::InvalidElf("ELF has no program headers"));
    }
    if header.program_header_count > MAX_PROGRAM_HEADERS {
        return Err(BootError::InvalidElf("too many program headers"));
    }

    let ph_table_size = u64::try_from(
        header
            .program_header_count
            .checked_mul(header.program_header_size)
            .ok_or(BootError::InvalidElf("program header table size overflow"))?,
    )
    .map_err(|_| BootError::InvalidElf("program header table size out of range"))?;
    let ph_table_end = header
        .program_header_offset
        .checked_add(ph_table_size)
        .ok_or(BootError::InvalidElf(
            "program header table bounds overflow",
        ))?;
    if ph_table_end > file_len {
        return Err(BootError::InvalidElf(
            "program header table is outside ELF image",
        ));
    }

    Ok(())
}

fn parse_program_headers(
    bytes: &[u8],
    count: usize,
) -> Result<[ProgramHeader; MAX_PROGRAM_HEADERS], BootError> {
    if bytes.len() != count * ELF64_PROGRAM_HEADER_SIZE {
        return Err(BootError::InvalidElf("invalid program header size"));
    }

    let mut parsed = [ProgramHeader::default(); MAX_PROGRAM_HEADERS];
    for (index, slot) in parsed.iter_mut().take(count).enumerate() {
        let offset = index
            .checked_mul(ELF64_PROGRAM_HEADER_SIZE)
            .ok_or(BootError::InvalidElf("program header offset overflow"))?;
        let raw = read_raw_program_header(
            bytes[offset..offset + ELF64_PROGRAM_HEADER_SIZE]
                .try_into()
                .map_err(|_| BootError::InvalidElf("invalid program header size"))?,
        );
        *slot = ProgramHeader {
            ty: raw.p_type.get(ELF_ENDIAN),
            flags: raw.p_flags.get(ELF_ENDIAN),
            offset: raw.p_offset.get(ELF_ENDIAN),
            virtual_addr: raw.p_vaddr.get(ELF_ENDIAN),
            physical_addr: raw.p_paddr.get(ELF_ENDIAN),
            file_size: raw.p_filesz.get(ELF_ENDIAN),
            mem_size: raw.p_memsz.get(ELF_ENDIAN),
            align: raw.p_align.get(ELF_ENDIAN),
        };
    }

    Ok(parsed)
}

fn load_segment(reader: &mut UefiKernelFile, segment: &SegmentLoadInfo) -> Result<(), BootError> {
    unsafe {
        ptr::write_bytes(
            segment.page_base as *mut u8,
            0,
            segment.page_end - segment.page_base,
        );
    }

    if segment.file_size == 0 {
        return Ok(());
    }

    reader.seek(segment.file_offset, "failed to seek to PT_LOAD bytes")?;

    let mut remaining = segment.file_size;
    let mut destination = segment.addr as *mut u8;
    let mut chunk = [0_u8; LOAD_CHUNK_SIZE];

    while remaining != 0 {
        let chunk_len = min(remaining, chunk.len());
        reader.read_exact(&mut chunk[..chunk_len], "failed to read PT_LOAD bytes")?;
        unsafe {
            ptr::copy_nonoverlapping(chunk.as_ptr(), destination, chunk_len);
            destination = destination.add(chunk_len);
        }
        remaining -= chunk_len;
    }

    Ok(())
}

fn validate_kernel_entry(entry_point: usize) -> Result<(), BootError> {
    if !(MIN_KERNEL_LOAD_ADDR..MAX_KERNEL_LOAD_END_EXCLUSIVE).contains(&entry_point) {
        return Err(BootError::InvalidElf(
            "entry point is outside the supported kernel load range",
        ));
    }
    Ok(())
}

fn validated_segment_bounds(
    header: &ElfHeader,
    ph: &ProgramHeader,
    file_len: u64,
    load_bias: usize,
) -> Result<SegmentLoadInfo, BootError> {
    let file_size = usize::try_from(ph.file_size)
        .map_err(|_| BootError::InvalidElf("segment file size out of range"))?;
    let mem_size = usize::try_from(ph.mem_size)
        .map_err(|_| BootError::InvalidElf("segment memory size out of range"))?;
    if mem_size == 0 {
        return Err(BootError::InvalidElf(
            "PT_LOAD segment has zero memory size",
        ));
    }
    if file_size > mem_size {
        return Err(BootError::InvalidElf(
            "segment file size exceeds memory size",
        ));
    }

    let file_end = ph
        .offset
        .checked_add(ph.file_size)
        .ok_or(BootError::InvalidElf("segment file bounds overflow"))?;
    if file_end > file_len {
        return Err(BootError::InvalidElf(
            "segment file range is outside ELF image",
        ));
    }

    let addr = segment_addr(header, ph, load_bias)?;
    if addr < MIN_KERNEL_LOAD_ADDR {
        return Err(BootError::InvalidElf(
            "segment address is below minimum kernel load address",
        ));
    }

    let end = addr
        .checked_add(mem_size)
        .ok_or(BootError::InvalidElf("segment address overflow"))?;
    if end > MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err(BootError::InvalidElf(
            "segment address exceeds maximum kernel load range",
        ));
    }

    let page_base = align_down(addr, PAGE_SIZE);
    let page_end =
        align_up(end, PAGE_SIZE).ok_or(BootError::InvalidElf("segment end alignment overflow"))?;

    Ok(SegmentLoadInfo {
        addr,
        end,
        page_base,
        page_end,
        file_offset: ph.offset,
        file_size,
    })
}

fn dynamic_segment_info(
    header: &ElfHeader,
    ph: &ProgramHeader,
    load_bias: usize,
) -> Result<DynamicSegmentInfo, BootError> {
    let size = usize::try_from(ph.mem_size)
        .map_err(|_| BootError::InvalidElf("dynamic segment size is out of range"))?;
    if size == 0 {
        return Err(BootError::InvalidElf("PT_DYNAMIC segment has zero size"));
    }
    if size % ELF64_DYNAMIC_ENTRY_SIZE != 0 {
        return Err(BootError::InvalidElf(
            "PT_DYNAMIC segment size is not entry aligned",
        ));
    }
    let addr = relocate_image_addr(header, ph.virtual_addr, load_bias)?;
    let end = addr
        .checked_add(size)
        .ok_or(BootError::InvalidElf("dynamic segment address overflow"))?;
    if addr < MIN_KERNEL_LOAD_ADDR || end > MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err(BootError::InvalidElf(
            "dynamic segment is outside the supported kernel load range",
        ));
    }
    Ok(DynamicSegmentInfo { addr, size })
}

fn reject_overlapping_segment(
    segment_addr: usize,
    segment_end: usize,
    existing_ranges: &[(usize, usize)],
) -> Result<(), BootError> {
    for &(other_start, other_end) in existing_ranges {
        if segment_addr < other_end && other_start < segment_end {
            return Err(BootError::InvalidElf("PT_LOAD segments overlap"));
        }
    }
    Ok(())
}

fn segment_addr(
    header: &ElfHeader,
    ph: &ProgramHeader,
    load_bias: usize,
) -> Result<usize, BootError> {
    if header.elf_type == objelf::ET_DYN {
        return relocate_image_addr(header, ph.virtual_addr, load_bias);
    }

    let physical_addr = usize::try_from(ph.physical_addr)
        .map_err(|_| BootError::InvalidElf("segment physical address out of range"))?;
    if physical_addr != 0 {
        return Ok(physical_addr);
    }

    usize::try_from(ph.virtual_addr)
        .map_err(|_| BootError::InvalidElf("segment virtual address out of range"))
}

fn kernel_load_footprint(
    header: &ElfHeader,
    program_headers: &[ProgramHeader],
) -> Result<KernelLoadFootprint, BootError> {
    let mut start = usize::MAX;
    let mut end = 0usize;
    let mut alignment = PAGE_SIZE;
    let mut load_count = 0usize;

    for ph in program_headers {
        if ph.ty != objelf::PT_LOAD {
            continue;
        }
        if ph.mem_size == 0 {
            return Err(BootError::InvalidElf(
                "PT_LOAD segment has zero memory size",
            ));
        }

        let segment_alignment = usize::try_from(ph.align)
            .map_err(|_| BootError::InvalidElf("segment alignment out of range"))?;
        if segment_alignment != 0 {
            if !segment_alignment.is_power_of_two() {
                return Err(BootError::InvalidElf(
                    "segment alignment is not a power of two",
                ));
            }
            alignment = alignment.max(segment_alignment);
        }

        let raw_addr = if header.elf_type == objelf::ET_DYN {
            usize::try_from(ph.virtual_addr)
                .map_err(|_| BootError::InvalidElf("segment virtual address out of range"))?
        } else {
            let physical_addr = usize::try_from(ph.physical_addr)
                .map_err(|_| BootError::InvalidElf("segment physical address out of range"))?;
            if physical_addr != 0 {
                physical_addr
            } else {
                usize::try_from(ph.virtual_addr)
                    .map_err(|_| BootError::InvalidElf("segment virtual address out of range"))?
            }
        };
        let mem_size = usize::try_from(ph.mem_size)
            .map_err(|_| BootError::InvalidElf("segment memory size out of range"))?;
        let segment_end = raw_addr
            .checked_add(mem_size)
            .ok_or(BootError::InvalidElf("segment address overflow"))?;
        start = start.min(align_down(raw_addr, PAGE_SIZE));
        end = end.max(
            align_up(segment_end, PAGE_SIZE)
                .ok_or(BootError::InvalidElf("segment end alignment overflow"))?,
        );
        load_count += 1;
    }

    if load_count == 0 {
        return Err(BootError::InvalidElf("no PT_LOAD segments"));
    }
    if end <= start {
        return Err(BootError::InvalidElf("kernel load footprint is empty"));
    }

    Ok(KernelLoadFootprint {
        start,
        end,
        alignment,
    })
}

fn allocate_kernel_image(
    header: &ElfHeader,
    footprint: &KernelLoadFootprint,
    physical_slide: usize,
) -> Result<KernelLoadAllocation, BootError> {
    let size = footprint
        .end
        .checked_sub(footprint.start)
        .ok_or(BootError::InvalidElf("kernel load footprint underflow"))?;
    let page_count = size / PAGE_SIZE;
    if page_count == 0 {
        return Err(BootError::InvalidElf("kernel load footprint has no pages"));
    }

    let phys_start = if header.elf_type == objelf::ET_DYN {
        allocate_dynamic_kernel_image(page_count, footprint.alignment, physical_slide)?
    } else {
        if footprint.start < MIN_KERNEL_LOAD_ADDR {
            return Err(BootError::InvalidElf(
                "fixed kernel load address is below the supported minimum",
            ));
        }
        let ptr = boot::allocate_pages(
            AllocateType::Address(footprint.start as u64),
            MemoryType::LOADER_DATA,
            page_count,
        )
        .map_err(|err| BootError::SegmentAlloc(err.status()))?;
        ptr.as_ptr() as usize
    };

    if phys_start < MIN_KERNEL_LOAD_ADDR {
        return Err(BootError::InvalidElf(
            "allocated kernel load address is below the supported minimum",
        ));
    }
    let phys_end = phys_start.checked_add(size).ok_or(BootError::InvalidElf(
        "allocated kernel image range overflow",
    ))?;
    if phys_end > MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err(BootError::InvalidElf(
            "allocated kernel image exceeds maximum kernel load range",
        ));
    }

    let load_bias = if header.elf_type == objelf::ET_DYN {
        phys_start
            .checked_sub(footprint.start)
            .ok_or(BootError::InvalidElf("kernel load bias underflow"))?
    } else {
        0
    };

    Ok(KernelLoadAllocation {
        phys_start,
        size,
        load_bias,
    })
}

fn allocate_dynamic_kernel_image(
    page_count: usize,
    alignment: usize,
    physical_slide: usize,
) -> Result<usize, BootError> {
    if alignment <= PAGE_SIZE {
        let ptr = boot::allocate_pages(
            AllocateType::MaxAddress((MAX_KERNEL_LOAD_END_EXCLUSIVE - 1) as u64),
            MemoryType::LOADER_DATA,
            page_count,
        )
        .map_err(|err| BootError::SegmentAlloc(err.status()))?;
        return Ok(ptr.as_ptr() as usize);
    }

    let extra_pages = alignment
        .checked_div(PAGE_SIZE)
        .and_then(|pages| pages.checked_sub(1))
        .ok_or(BootError::InvalidElf("kernel load alignment is invalid"))?;
    let alloc_pages = page_count
        .checked_add(extra_pages)
        .ok_or(BootError::InvalidElf(
            "kernel allocation page count overflow",
        ))?;
    let mut max_addr = MAX_KERNEL_LOAD_END_EXCLUSIVE - 1;
    let jitter = align_down(physical_slide, PAGE_SIZE);
    if jitter != 0 {
        max_addr = max_addr.saturating_sub(jitter);
        if max_addr < MIN_KERNEL_LOAD_ADDR {
            max_addr = MAX_KERNEL_LOAD_END_EXCLUSIVE - 1;
        }
    }
    let ptr = boot::allocate_pages(
        AllocateType::MaxAddress(max_addr as u64),
        MemoryType::LOADER_DATA,
        alloc_pages,
    )
    .map_err(|err| BootError::SegmentAlloc(err.status()))?;
    let base = ptr.as_ptr() as usize;
    let aligned = align_up(base, alignment).ok_or(BootError::InvalidElf(
        "kernel allocation alignment overflow",
    ))?;
    if aligned
        .checked_add(page_count * PAGE_SIZE)
        .ok_or(BootError::InvalidElf("aligned kernel allocation overflow"))?
        > base + alloc_pages * PAGE_SIZE
    {
        return Err(BootError::InvalidElf(
            "aligned kernel allocation is outside reserved range",
        ));
    }
    Ok(aligned)
}

fn relocate_image_addr(
    header: &ElfHeader,
    raw_addr: u64,
    load_bias: usize,
) -> Result<usize, BootError> {
    let addr = usize::try_from(raw_addr)
        .map_err(|_| BootError::InvalidElf("image address is out of range"))?;
    if header.elf_type == objelf::ET_DYN {
        return load_bias.checked_add(addr).ok_or(BootError::InvalidElf(
            "image address overflow after relocation",
        ));
    }
    Ok(addr)
}

fn apply_dynamic_relocations(
    header: &ElfHeader,
    dynamic_segment: &DynamicSegmentInfo,
    load_bias: usize,
    loaded_ranges: &[(usize, usize)],
) -> Result<(), BootError> {
    let dynamic_end = dynamic_segment
        .addr
        .checked_add(dynamic_segment.size)
        .ok_or(BootError::InvalidElf("dynamic segment address overflow"))?;
    require_range_covered(
        dynamic_segment.addr,
        dynamic_end,
        loaded_ranges,
        "dynamic segment is not inside a PT_LOAD segment",
    )?;

    let relocations = parse_dynamic_relocation_table(header, dynamic_segment, load_bias)?;
    if relocations.size == 0 {
        return Ok(());
    }
    let rela_end = relocations
        .addr
        .checked_add(relocations.size)
        .ok_or(BootError::InvalidElf("Rela table address overflow"))?;
    require_range_covered(
        relocations.addr,
        rela_end,
        loaded_ranges,
        "dynamic relocation table is not inside a PT_LOAD segment",
    )?;

    let rela_count = relocations.size / ELF64_RELA_ENTRY_SIZE;
    for index in 0..rela_count {
        let entry_addr = relocations
            .addr
            .checked_add(index * ELF64_RELA_ENTRY_SIZE)
            .ok_or(BootError::InvalidElf("Rela entry address overflow"))?;
        let offset = read_u64_from_memory(entry_addr);
        let info = read_u64_from_memory(entry_addr + 8);
        let addend = read_i64_from_memory(entry_addr + 16);
        if (info as u32) != R_X86_64_RELATIVE {
            return Err(BootError::InvalidElf("unsupported dynamic relocation type"));
        }

        let target = relocate_image_addr(header, offset, load_bias)?;
        let target_end = target
            .checked_add(core::mem::size_of::<u64>())
            .ok_or(BootError::InvalidElf("relocation target overflow"))?;
        require_range_covered(
            target,
            target_end,
            loaded_ranges,
            "dynamic relocation target is not inside a PT_LOAD segment",
        )?;

        let relocated_value = relative_relocation_value(header, load_bias, addend)?;
        write_u64_to_memory(target, relocated_value);
    }

    Ok(())
}

fn parse_dynamic_relocation_table(
    header: &ElfHeader,
    dynamic_segment: &DynamicSegmentInfo,
    load_bias: usize,
) -> Result<DynamicRelocationTable, BootError> {
    let mut rela_addr = None;
    let mut rela_size = None;
    let mut rela_ent = None;

    let entry_count = dynamic_segment.size / ELF64_DYNAMIC_ENTRY_SIZE;
    for index in 0..entry_count {
        let entry_addr = dynamic_segment
            .addr
            .checked_add(index * ELF64_DYNAMIC_ENTRY_SIZE)
            .ok_or(BootError::InvalidElf("dynamic entry address overflow"))?;
        let tag = read_i64_from_memory(entry_addr);
        let value = read_u64_from_memory(entry_addr + 8);

        match tag {
            DT_NULL => break,
            DT_RELA => {
                rela_addr = Some(relocate_image_addr(header, value, load_bias)?);
            }
            DT_RELASZ => {
                rela_size = Some(usize::try_from(value).map_err(|_| {
                    BootError::InvalidElf("dynamic relocation table size is out of range")
                })?);
            }
            DT_RELAENT => {
                rela_ent = Some(usize::try_from(value).map_err(|_| {
                    BootError::InvalidElf("dynamic relocation entry size is out of range")
                })?);
            }
            _ => {}
        }
    }

    let rela_addr = match (rela_addr, rela_size) {
        (Some(addr), Some(size)) if size != 0 => {
            if let Some(entry_size) = rela_ent {
                if entry_size != ELF64_RELA_ENTRY_SIZE {
                    return Err(BootError::InvalidElf("unsupported Rela entry size"));
                }
            }
            if size % ELF64_RELA_ENTRY_SIZE != 0 {
                return Err(BootError::InvalidElf(
                    "dynamic relocation table size is not aligned",
                ));
            }
            addr
        }
        _ => return Ok(DynamicRelocationTable { addr: 0, size: 0 }),
    };
    let rela_size = rela_size.unwrap_or(0);
    if rela_size / ELF64_RELA_ENTRY_SIZE > MAX_DYNAMIC_RELOCATIONS {
        return Err(BootError::InvalidElf(
            "dynamic relocation table exceeds hard cap",
        ));
    }
    Ok(DynamicRelocationTable {
        addr: rela_addr,
        size: rela_size,
    })
}

fn require_range_covered(
    start: usize,
    end: usize,
    loaded_ranges: &[(usize, usize)],
    error_message: &'static str,
) -> Result<(), BootError> {
    for &(range_start, range_end) in loaded_ranges {
        if start >= range_start && end <= range_end {
            return Ok(());
        }
    }
    Err(BootError::InvalidElf(error_message))
}

fn relative_relocation_value(
    header: &ElfHeader,
    load_bias: usize,
    addend: i64,
) -> Result<u64, BootError> {
    let base = if header.elf_type == objelf::ET_DYN {
        load_bias as i128
    } else {
        0
    };
    let value = base + i128::from(addend);
    if !(0..=u64::MAX as i128).contains(&value) {
        return Err(BootError::InvalidElf("relocation value is out of range"));
    }
    Ok(value as u64)
}

fn read_i64_from_memory(addr: usize) -> i64 {
    unsafe { ptr::read_unaligned(addr as *const i64) }
}

fn read_u64_from_memory(addr: usize) -> u64 {
    unsafe { ptr::read_unaligned(addr as *const u64) }
}

fn write_u64_to_memory(addr: usize, value: u64) {
    unsafe { ptr::write_unaligned(addr as *mut u64, value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header(elf_type: u16) -> ElfHeader {
        ElfHeader {
            elf_type,
            entry_point: MIN_KERNEL_LOAD_ADDR as u64,
            program_header_offset: ELF64_HEADER_SIZE as u64,
            program_header_size: ELF64_PROGRAM_HEADER_SIZE,
            program_header_count: 1,
        }
    }

    #[test]
    fn dynamic_segment_rejects_misaligned_size() {
        let header = valid_header(objelf::ET_DYN);
        let ph = ProgramHeader {
            ty: objelf::PT_DYNAMIC,
            flags: 0,
            offset: 0,
            virtual_addr: 0x2000,
            physical_addr: 0,
            file_size: 0,
            mem_size: (ELF64_DYNAMIC_ENTRY_SIZE + 1) as u64,
            align: 8,
        };

        let err = dynamic_segment_info(&header, &ph, 0x40_0000).expect_err("misaligned PT_DYNAMIC");
        assert!(matches!(
            err,
            BootError::InvalidElf("PT_DYNAMIC segment size is not entry aligned")
        ));
    }

    #[test]
    fn dynamic_relocation_table_rejects_hard_cap_overflow() {
        let header = valid_header(objelf::ET_DYN);
        let mut dynamic = [0u8; ELF64_DYNAMIC_ENTRY_SIZE * 3];
        dynamic[0..8].copy_from_slice(&DT_RELA.to_le_bytes());
        dynamic[8..16].copy_from_slice(&(0x1000u64).to_le_bytes());
        dynamic[16..24].copy_from_slice(&DT_RELASZ.to_le_bytes());
        dynamic[24..32].copy_from_slice(
            &(((MAX_DYNAMIC_RELOCATIONS + 1) * ELF64_RELA_ENTRY_SIZE) as u64).to_le_bytes(),
        );
        dynamic[32..40].copy_from_slice(&DT_RELAENT.to_le_bytes());
        dynamic[40..48].copy_from_slice(&(ELF64_RELA_ENTRY_SIZE as u64).to_le_bytes());

        let dynamic_segment = DynamicSegmentInfo {
            addr: dynamic.as_ptr() as usize,
            size: dynamic.len(),
        };

        let err = parse_dynamic_relocation_table(&header, &dynamic_segment, 0)
            .expect_err("oversized relocation table must fail");
        assert!(matches!(
            err,
            BootError::InvalidElf("dynamic relocation table exceeds hard cap")
        ));
    }
}

fn align_down(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|aligned| align_down(aligned, align))
}

fn read_exact_at(
    reader: &mut UefiKernelFile,
    offset: u64,
    buffer: &mut [u8],
    error_message: &'static str,
) -> Result<(), BootError> {
    reader.seek(offset, error_message)?;
    reader.read_exact(buffer, error_message)
}

fn read_raw_elf_header(bytes: &[u8; ELF64_HEADER_SIZE]) -> RawElfHeader<LittleEndian> {
    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const RawElfHeader<LittleEndian>) }
}

fn read_raw_program_header(
    bytes: &[u8; ELF64_PROGRAM_HEADER_SIZE],
) -> RawProgramHeader<LittleEndian> {
    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const RawProgramHeader<LittleEndian>) }
}
