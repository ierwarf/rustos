use core::cmp::min;
use core::convert::TryFrom;
use core::ptr;

use fatfs::{IoBase, Read, Seek, SeekFrom};

use crate::fat::DiskIoError;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELF_CLASS_64: u8 = 2;
const ELF_DATA_LITTLE_ENDIAN: u8 = 1;
const ELF_TYPE_EXEC: u16 = 2;
const ELF_TYPE_DYN: u16 = 3;
const ELF_MACHINE_X86_64: u16 = 62;
const ELF64_HEADER_SIZE: usize = 64;
const ELF64_PROGRAM_HEADER_SIZE: usize = 56;
const ELF_PT_LOAD: u32 = 1;
const ELF_PT_DYNAMIC: u32 = 2;
const ELF_PF_X: u32 = 1;
const MAX_PROGRAM_HEADERS: usize = 32;
const PAGE_SIZE: usize = 0x1000;
const MIN_KERNEL_LOAD_ADDR: usize = 0x0020_0000;
const MAX_KERNEL_LOAD_END_EXCLUSIVE: usize = 512 * 1024 * 1024 * 1024;
const LOAD_CHUNK_SIZE: usize = 4096;
const KERNEL_PIE_LOAD_BIAS: usize = MIN_KERNEL_LOAD_ADDR;
const MAX_KERNEL_PHYSICAL_KASLR_SLIDE: usize = 0x0020_0000;
const ELF64_DYNAMIC_ENTRY_SIZE: usize = 16;
const ELF64_RELA_ENTRY_SIZE: usize = 24;
const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const R_X86_64_RELATIVE: u32 = 8;

pub fn load_kernel_elf<R>(
    reader: &mut R,
    file_len: u64,
    physical_slide: usize,
) -> Result<(usize, usize, usize), &'static str>
where
    R: Read + Seek + IoBase<Error = fatfs::Error<DiskIoError>>,
{
    let mut elf_header = [0_u8; ELF64_HEADER_SIZE];
    read_exact_at(reader, 0, &mut elf_header, "failed to read ELF header")?;

    let header = parse_elf_header(&elf_header)?;
    validate_elf_header(&elf_header, &header, file_len)?;

    let ph_table_size = header
        .program_header_count
        .checked_mul(header.program_header_size)
        .ok_or("program header table size overflow")?;
    let mut program_headers = [0_u8; ELF64_PROGRAM_HEADER_SIZE * MAX_PROGRAM_HEADERS];
    read_exact_at(
        reader,
        header.program_header_offset,
        &mut program_headers[..ph_table_size],
        "failed to read program header table",
    )?;

    let load_alignment = required_load_alignment(&program_headers[..ph_table_size], &header)?;
    let load_bias = kernel_load_bias(&header, physical_slide, load_alignment)?;
    let entry_point = relocate_image_addr(&header, header.entry_point, load_bias)?;
    validate_kernel_entry(entry_point)?;

    let mut loaded_segments = 0usize;
    let mut executable_entry_covered = false;
    let mut loaded_ranges = [(0usize, 0usize); MAX_PROGRAM_HEADERS];
    let mut loaded_range_count = 0usize;
    let mut dynamic_segment = None;

    for index in 0..header.program_header_count {
        let offset = index * ELF64_PROGRAM_HEADER_SIZE;
        let ph =
            parse_program_header(&program_headers[offset..offset + ELF64_PROGRAM_HEADER_SIZE])?;
        if ph.ty == ELF_PT_DYNAMIC {
            dynamic_segment = Some(dynamic_segment_info(&header, &ph, load_bias)?);
            continue;
        }
        if ph.ty != ELF_PT_LOAD {
            continue;
        }

        let segment = validated_segment_bounds(&header, &ph, file_len, load_bias)?;
        reject_overlapping_segment(
            segment.addr,
            segment.end,
            &loaded_ranges[..loaded_range_count],
        )?;
        loaded_ranges[loaded_range_count] = (segment.addr, segment.end);
        loaded_range_count += 1;

        if ph.flags & ELF_PF_X != 0 && (segment.addr..segment.end).contains(&entry_point) {
            executable_entry_covered = true;
        }

        load_segment(reader, &segment)?;
        loaded_segments += 1;
    }

    if loaded_segments == 0 {
        return Err("no PT_LOAD segments");
    }
    if !executable_entry_covered {
        return Err("entry point is not inside an executable PT_LOAD segment");
    }
    if header.elf_type == ELF_TYPE_DYN {
        let dynamic_segment = dynamic_segment.ok_or("ET_DYN image is missing PT_DYNAMIC")?;
        apply_dynamic_relocations(
            &header,
            &dynamic_segment,
            load_bias,
            &loaded_ranges[..loaded_range_count],
        )?;
    }

    Ok((entry_point, loaded_segments, load_bias))
}

#[derive(Clone, Copy)]
struct ElfHeader {
    elf_type: u16,
    entry_point: u64,
    program_header_offset: u64,
    program_header_size: usize,
    program_header_count: usize,
}

#[derive(Clone, Copy)]
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

#[derive(Clone, Copy)]
struct DynamicSegmentInfo {
    addr: usize,
    size: usize,
}

#[derive(Clone, Copy)]
struct DynamicRelocationTable {
    addr: usize,
    size: usize,
}

fn parse_elf_header(header: &[u8; ELF64_HEADER_SIZE]) -> Result<ElfHeader, &'static str> {
    if header[..4] != ELF_MAGIC {
        return Err("invalid ELF magic");
    }

    Ok(ElfHeader {
        elf_type: read_u16(header, 16),
        entry_point: read_u64(header, 24),
        program_header_offset: read_u64(header, 32),
        program_header_size: usize::from(read_u16(header, 54)),
        program_header_count: usize::from(read_u16(header, 56)),
    })
}

fn validate_elf_header(
    raw_header: &[u8; ELF64_HEADER_SIZE],
    header: &ElfHeader,
    file_len: u64,
) -> Result<(), &'static str> {
    if raw_header[4] != ELF_CLASS_64 {
        return Err("ELF class is not 64-bit");
    }
    if raw_header[5] != ELF_DATA_LITTLE_ENDIAN {
        return Err("ELF endianness is not little-endian");
    }

    if !matches!(header.elf_type, ELF_TYPE_EXEC | ELF_TYPE_DYN) {
        return Err("ELF type is not executable/shared object");
    }
    if read_u16(raw_header, 18) != ELF_MACHINE_X86_64 {
        return Err("ELF machine is not x86_64");
    }
    if usize::from(read_u16(raw_header, 52)) != ELF64_HEADER_SIZE {
        return Err("ELF header size is invalid");
    }
    if header.program_header_size != ELF64_PROGRAM_HEADER_SIZE {
        return Err("ELF program header size is invalid");
    }
    if header.program_header_count == 0 {
        return Err("ELF has no program headers");
    }
    if header.program_header_count > MAX_PROGRAM_HEADERS {
        return Err("too many program headers");
    }

    let ph_table_size = u64::try_from(
        header
            .program_header_count
            .checked_mul(header.program_header_size)
            .ok_or("program header table size overflow")?,
    )
    .map_err(|_| "program header table size out of range")?;
    let ph_table_end = header
        .program_header_offset
        .checked_add(ph_table_size)
        .ok_or("program header table bounds overflow")?;
    if ph_table_end > file_len {
        return Err("program header table is outside ELF image");
    }

    Ok(())
}

fn parse_program_header(bytes: &[u8]) -> Result<ProgramHeader, &'static str> {
    if bytes.len() != ELF64_PROGRAM_HEADER_SIZE {
        return Err("invalid program header size");
    }

    Ok(ProgramHeader {
        ty: read_u32(bytes, 0),
        flags: read_u32(bytes, 4),
        offset: read_u64(bytes, 8),
        virtual_addr: read_u64(bytes, 16),
        physical_addr: read_u64(bytes, 24),
        file_size: read_u64(bytes, 32),
        mem_size: read_u64(bytes, 40),
        align: read_u64(bytes, 48),
    })
}

fn load_segment<R>(reader: &mut R, segment: &SegmentLoadInfo) -> Result<(), &'static str>
where
    R: Read + Seek + IoBase<Error = fatfs::Error<DiskIoError>>,
{
    unsafe {
        // The destination range was validated to lie in the supported kernel load window.
        ptr::write_bytes(
            segment.page_base as *mut u8,
            0,
            segment.page_end - segment.page_base,
        );
    }

    if segment.file_size == 0 {
        return Ok(());
    }

    reader
        .seek(SeekFrom::Start(segment.file_offset))
        .map_err(|_| "failed to seek to PT_LOAD bytes")?;

    let mut remaining = segment.file_size;
    let mut destination = segment.addr as *mut u8;
    let mut chunk = [0_u8; LOAD_CHUNK_SIZE];

    while remaining != 0 {
        let chunk_len = min(remaining, chunk.len());
        read_exact(
            reader,
            &mut chunk[..chunk_len],
            "failed to read PT_LOAD bytes",
        )?;
        unsafe {
            // `destination` stays within the validated PT_LOAD output range.
            ptr::copy_nonoverlapping(chunk.as_ptr(), destination, chunk_len);
            destination = destination.add(chunk_len);
        }
        remaining -= chunk_len;
    }

    Ok(())
}

fn validate_kernel_entry(entry_point: usize) -> Result<(), &'static str> {
    if !(MIN_KERNEL_LOAD_ADDR..MAX_KERNEL_LOAD_END_EXCLUSIVE).contains(&entry_point) {
        return Err("entry point is outside the supported kernel load range");
    }
    Ok(())
}

fn validated_segment_bounds(
    header: &ElfHeader,
    ph: &ProgramHeader,
    file_len: u64,
    load_bias: usize,
) -> Result<SegmentLoadInfo, &'static str> {
    let file_size = usize::try_from(ph.file_size).map_err(|_| "segment file size out of range")?;
    let mem_size = usize::try_from(ph.mem_size).map_err(|_| "segment memory size out of range")?;
    if mem_size == 0 {
        return Err("PT_LOAD segment has zero memory size");
    }
    if file_size > mem_size {
        return Err("segment file size exceeds memory size");
    }

    let file_end = ph
        .offset
        .checked_add(ph.file_size)
        .ok_or("segment file bounds overflow")?;
    if file_end > file_len {
        return Err("segment file range is outside ELF image");
    }

    let addr = segment_addr(header, ph, load_bias)?;
    if addr < MIN_KERNEL_LOAD_ADDR {
        return Err("segment address is below minimum kernel load address");
    }

    let end = addr
        .checked_add(mem_size)
        .ok_or("segment address overflow")?;
    if end > MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err("segment address exceeds maximum kernel load range");
    }

    let page_base = align_down(addr, PAGE_SIZE);
    let page_end = align_up(end, PAGE_SIZE).ok_or("segment end alignment overflow")?;

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
) -> Result<DynamicSegmentInfo, &'static str> {
    let size = usize::try_from(ph.mem_size).map_err(|_| "dynamic segment size is out of range")?;
    if size == 0 {
        return Err("PT_DYNAMIC segment has zero size");
    }
    let addr = relocate_image_addr(header, ph.virtual_addr, load_bias)?;
    let end = addr
        .checked_add(size)
        .ok_or("dynamic segment address overflow")?;
    if addr < MIN_KERNEL_LOAD_ADDR || end > MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err("dynamic segment is outside the supported kernel load range");
    }

    Ok(DynamicSegmentInfo { addr, size })
}

fn reject_overlapping_segment(
    segment_addr: usize,
    segment_end: usize,
    existing_ranges: &[(usize, usize)],
) -> Result<(), &'static str> {
    for &(other_start, other_end) in existing_ranges {
        if segment_addr < other_end && other_start < segment_end {
            return Err("PT_LOAD segments overlap");
        }
    }
    Ok(())
}

fn segment_addr(
    header: &ElfHeader,
    ph: &ProgramHeader,
    load_bias: usize,
) -> Result<usize, &'static str> {
    if header.elf_type == ELF_TYPE_DYN {
        return relocate_image_addr(header, ph.virtual_addr, load_bias);
    }

    let physical_addr =
        usize::try_from(ph.physical_addr).map_err(|_| "segment physical address out of range")?;
    if physical_addr != 0 {
        return Ok(physical_addr);
    }

    usize::try_from(ph.virtual_addr).map_err(|_| "segment virtual address out of range")
}

fn kernel_load_bias(
    header: &ElfHeader,
    physical_slide: usize,
    load_alignment: usize,
) -> Result<usize, &'static str> {
    if header.elf_type == ELF_TYPE_DYN {
        if physical_slide > MAX_KERNEL_PHYSICAL_KASLR_SLIDE {
            return Err("kernel physical slide exceeds the supported KASLR window");
        }

        let applied_slide = align_down(physical_slide, load_alignment);
        KERNEL_PIE_LOAD_BIAS
            .checked_add(applied_slide)
            .ok_or("kernel load bias overflow")
    } else {
        Ok(0)
    }
}

fn required_load_alignment(
    program_headers: &[u8],
    header: &ElfHeader,
) -> Result<usize, &'static str> {
    let mut required_alignment = PAGE_SIZE;

    for index in 0..header.program_header_count {
        let offset = index
            .checked_mul(ELF64_PROGRAM_HEADER_SIZE)
            .ok_or("program header offset overflow")?;
        let ph =
            parse_program_header(&program_headers[offset..offset + ELF64_PROGRAM_HEADER_SIZE])?;
        if ph.ty != ELF_PT_LOAD {
            continue;
        }

        let segment_alignment =
            usize::try_from(ph.align).map_err(|_| "segment alignment out of range")?;
        if segment_alignment == 0 {
            continue;
        }
        if !segment_alignment.is_power_of_two() {
            return Err("segment alignment is not a power of two");
        }
        required_alignment = required_alignment.max(segment_alignment);
    }

    Ok(required_alignment)
}

fn relocate_image_addr(
    header: &ElfHeader,
    raw_addr: u64,
    load_bias: usize,
) -> Result<usize, &'static str> {
    let addr = usize::try_from(raw_addr).map_err(|_| "image address is out of range")?;
    if header.elf_type == ELF_TYPE_DYN {
        load_bias
            .checked_add(addr)
            .ok_or("image address overflow after relocation")
    } else {
        Ok(addr)
    }
}

fn apply_dynamic_relocations(
    header: &ElfHeader,
    dynamic_segment: &DynamicSegmentInfo,
    load_bias: usize,
    loaded_ranges: &[(usize, usize)],
) -> Result<(), &'static str> {
    let dynamic_end = dynamic_segment
        .addr
        .checked_add(dynamic_segment.size)
        .ok_or("dynamic segment address overflow")?;
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
        .ok_or("Rela table address overflow")?;
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
            .ok_or("Rela entry address overflow")?;
        let offset = read_u64_from_memory(entry_addr);
        let info = read_u64_from_memory(entry_addr + 8);
        let addend = read_i64_from_memory(entry_addr + 16);

        let relocation_type = info as u32;
        if relocation_type != R_X86_64_RELATIVE {
            return Err("unsupported dynamic relocation type");
        }

        let target = relocate_image_addr(header, offset, load_bias)?;
        let target_end = target
            .checked_add(core::mem::size_of::<u64>())
            .ok_or("relocation target overflow")?;
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
) -> Result<DynamicRelocationTable, &'static str> {
    let mut rela_addr = None;
    let mut rela_size = None;
    let mut rela_ent = None;

    let entry_count = dynamic_segment.size / ELF64_DYNAMIC_ENTRY_SIZE;
    for index in 0..entry_count {
        let entry_addr = dynamic_segment
            .addr
            .checked_add(index * ELF64_DYNAMIC_ENTRY_SIZE)
            .ok_or("dynamic entry address overflow")?;
        let tag = read_i64_from_memory(entry_addr);
        let value = read_u64_from_memory(entry_addr + 8);

        match tag {
            DT_NULL => break,
            DT_RELA => {
                rela_addr = Some(relocate_image_addr(header, value, load_bias)?);
            }
            DT_RELASZ => {
                rela_size = Some(
                    usize::try_from(value)
                        .map_err(|_| "dynamic relocation table size is out of range")?,
                );
            }
            DT_RELAENT => {
                rela_ent = Some(
                    usize::try_from(value)
                        .map_err(|_| "dynamic relocation entry size is out of range")?,
                );
            }
            _ => {}
        }
    }

    let rela_addr = match (rela_addr, rela_size) {
        (Some(addr), Some(size)) if size != 0 => {
            if let Some(entry_size) = rela_ent {
                if entry_size != ELF64_RELA_ENTRY_SIZE {
                    return Err("unsupported Rela entry size");
                }
            }
            if size % ELF64_RELA_ENTRY_SIZE != 0 {
                return Err("dynamic relocation table size is not aligned");
            }
            addr
        }
        _ => return Ok(DynamicRelocationTable { addr: 0, size: 0 }),
    };
    let rela_size = rela_size.unwrap_or(0);

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
) -> Result<(), &'static str> {
    for &(range_start, range_end) in loaded_ranges {
        if start >= range_start && end <= range_end {
            return Ok(());
        }
    }

    Err(error_message)
}

fn relative_relocation_value(
    header: &ElfHeader,
    load_bias: usize,
    addend: i64,
) -> Result<u64, &'static str> {
    let base = if header.elf_type == ELF_TYPE_DYN {
        load_bias as i128
    } else {
        0
    };
    let value = base + i128::from(addend);
    if !(0..=u64::MAX as i128).contains(&value) {
        return Err("relocation value is out of range");
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

fn read_exact_at<R>(
    reader: &mut R,
    offset: u64,
    buffer: &mut [u8],
    error_message: &'static str,
) -> Result<(), &'static str>
where
    R: Read + Seek + IoBase<Error = fatfs::Error<DiskIoError>>,
{
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|_| error_message)?;
    read_exact(reader, buffer, error_message)
}

fn read_exact<R>(
    reader: &mut R,
    mut buffer: &mut [u8],
    error_message: &'static str,
) -> Result<(), &'static str>
where
    R: Read + IoBase<Error = fatfs::Error<DiskIoError>>,
{
    while !buffer.is_empty() {
        let read = reader.read(buffer).map_err(|_| error_message)?;
        if read == 0 {
            return Err(error_message);
        }
        buffer = &mut buffer[read..];
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let chunk = &bytes[offset..offset + 2];
    u16::from_le_bytes([chunk[0], chunk[1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let chunk = &bytes[offset..offset + 4];
    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let chunk = &bytes[offset..offset + 8];
    u64::from_le_bytes([
        chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header(elf_type: u16) -> [u8; ELF64_HEADER_SIZE] {
        let mut header = [0_u8; ELF64_HEADER_SIZE];
        header[..4].copy_from_slice(&ELF_MAGIC);
        header[4] = ELF_CLASS_64;
        header[5] = ELF_DATA_LITTLE_ENDIAN;
        header[16..18].copy_from_slice(&elf_type.to_le_bytes());
        header[18..20].copy_from_slice(&ELF_MACHINE_X86_64.to_le_bytes());
        header[24..32].copy_from_slice(&(MIN_KERNEL_LOAD_ADDR as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(ELF64_HEADER_SIZE as u64).to_le_bytes());
        header[52..54].copy_from_slice(&(ELF64_HEADER_SIZE as u16).to_le_bytes());
        header[54..56].copy_from_slice(&(ELF64_PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        header[56..58].copy_from_slice(&(1_u16).to_le_bytes());
        header
    }

    fn valid_program_header() -> ProgramHeader {
        ProgramHeader {
            ty: ELF_PT_LOAD,
            flags: ELF_PF_X,
            offset: 0x1000,
            virtual_addr: MIN_KERNEL_LOAD_ADDR as u64,
            physical_addr: MIN_KERNEL_LOAD_ADDR as u64,
            file_size: 0x200,
            mem_size: 0x400,
            align: PAGE_SIZE as u64,
        }
    }

    #[test]
    fn validate_elf_header_accepts_minimal_valid_header() {
        let header_bytes = valid_header(ELF_TYPE_EXEC);
        let header = parse_elf_header(&header_bytes).expect("header should parse");

        assert_eq!(header.entry_point, MIN_KERNEL_LOAD_ADDR as u64);
        validate_elf_header(
            &header_bytes,
            &header,
            ELF64_HEADER_SIZE as u64 + ELF64_PROGRAM_HEADER_SIZE as u64,
        )
        .expect("header should validate");
    }

    #[test]
    fn validate_elf_header_rejects_short_program_table() {
        let header_bytes = valid_header(ELF_TYPE_EXEC);
        let header = parse_elf_header(&header_bytes).expect("header should parse");

        let err = validate_elf_header(&header_bytes, &header, ELF64_HEADER_SIZE as u64)
            .expect_err("short file should fail");
        assert_eq!(err, "program header table is outside ELF image");
    }

    #[test]
    fn validated_segment_bounds_accepts_supported_range() {
        let header_bytes = valid_header(ELF_TYPE_EXEC);
        let header = parse_elf_header(&header_bytes).expect("header should parse");
        let segment = validated_segment_bounds(&header, &valid_program_header(), 0x4000, 0)
            .expect("segment should validate");

        assert_eq!(segment.addr, MIN_KERNEL_LOAD_ADDR);
        assert_eq!(segment.end, MIN_KERNEL_LOAD_ADDR + 0x400);
        assert_eq!(segment.page_base, MIN_KERNEL_LOAD_ADDR);
        assert_eq!(segment.page_end, MIN_KERNEL_LOAD_ADDR + PAGE_SIZE);
    }

    #[test]
    fn validated_segment_bounds_rejects_low_address() {
        let header_bytes = valid_header(ELF_TYPE_EXEC);
        let elf_header = parse_elf_header(&header_bytes).expect("header should parse");
        let mut program_header = valid_program_header();
        program_header.physical_addr = (MIN_KERNEL_LOAD_ADDR - 0x1000) as u64;

        let err = validated_segment_bounds(&elf_header, &program_header, 0x4000, 0)
            .expect_err("low segment should fail");
        assert_eq!(err, "segment address is below minimum kernel load address");
    }

    #[test]
    fn dyn_entry_is_relocated_by_kernel_bias() {
        let mut header_bytes = valid_header(ELF_TYPE_DYN);
        header_bytes[24..32].copy_from_slice(&(0x8c2f0_u64).to_le_bytes());
        let header = parse_elf_header(&header_bytes).expect("header should parse");

        let load_bias =
            kernel_load_bias(&header, 0x1234, PAGE_SIZE).expect("load bias should compute");
        let entry = relocate_image_addr(&header, header.entry_point, load_bias)
            .expect("entry relocation should succeed");
        assert_eq!(entry, KERNEL_PIE_LOAD_BIAS + 0x1000 + 0x8c2f0);
    }

    #[test]
    fn reject_overlapping_segment_detects_collision() {
        let err = reject_overlapping_segment(0x3000, 0x5000, &[(0x2000, 0x4000)])
            .expect_err("overlap should fail");
        assert_eq!(err, "PT_LOAD segments overlap");
    }
}
