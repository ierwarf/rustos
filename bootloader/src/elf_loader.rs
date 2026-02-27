use core::convert::TryFrom;
use core::ptr;

use uefi::boot::{self, AllocateType, MemoryType};
use xmas_elf::header::{Class, Data, Machine, Type as ElfType};
use xmas_elf::program::{ProgramHeader, Type as ProgramType};
use xmas_elf::ElfFile;

use crate::error::BootError;

const PAGE_SIZE: usize = 0x1000;
const MIN_KERNEL_LOAD_ADDR: usize = 0x0010_0000; // 1 MiB
const MAX_KERNEL_LOAD_END_EXCLUSIVE: usize = 512 * 1024 * 1024 * 1024; // 512 GiB

pub fn load_kernel_elf(kernel_image: &[u8]) -> Result<(usize, usize), BootError> {
    let elf = ElfFile::new(kernel_image).map_err(BootError::InvalidElf)?;
    validate_elf_header(&elf)?;
    let entry_point = usize::try_from(elf.header.pt2.entry_point())
        .map_err(|_| BootError::InvalidElf("entry point out of range"))?;
    validate_kernel_entry(entry_point)?;

    let mut loaded_segments = 0usize;
    let mut executable_entry_covered = false;
    let mut loaded_ranges: [(usize, usize); 32] = [(0, 0); 32];
    let mut loaded_range_count = 0usize;

    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(BootError::InvalidElf)?;
        if ph_type != ProgramType::Load {
            continue;
        }

        let (segment_addr, segment_end) = validated_segment_bounds(&ph)?;
        if loaded_range_count >= loaded_ranges.len() {
            return Err(BootError::InvalidElf("too many PT_LOAD segments"));
        }
        reject_overlapping_segment(
            segment_addr,
            segment_end,
            &loaded_ranges[..loaded_range_count],
        )?;
        loaded_ranges[loaded_range_count] = (segment_addr, segment_end);
        loaded_range_count += 1;

        if ph.flags().is_execute() && (segment_addr..segment_end).contains(&entry_point) {
            executable_entry_covered = true;
        }

        load_segment(kernel_image, &ph)?;
        loaded_segments += 1;
    }

    if !executable_entry_covered {
        return Err(BootError::InvalidElf(
            "entry point is not inside an executable PT_LOAD segment",
        ));
    }

    Ok((entry_point, loaded_segments))
}

fn validate_elf_header(elf: &ElfFile<'_>) -> Result<(), BootError> {
    if elf.header.pt1.class() != Class::SixtyFour {
        return Err(BootError::InvalidElf("ELF class is not 64-bit"));
    }
    if elf.header.pt1.data() != Data::LittleEndian {
        return Err(BootError::InvalidElf("ELF endianness is not little-endian"));
    }
    if elf.header.pt2.machine().as_machine() != Machine::X86_64 {
        return Err(BootError::InvalidElf("ELF machine is not x86_64"));
    }

    let elf_type = elf.header.pt2.type_().as_type();
    if !matches!(elf_type, ElfType::Executable | ElfType::SharedObject) {
        return Err(BootError::InvalidElf(
            "ELF type is not executable/shared object",
        ));
    }

    Ok(())
}

fn load_segment(kernel_image: &[u8], ph: &ProgramHeader<'_>) -> Result<(), BootError> {
    let file_size = usize::try_from(ph.file_size())
        .map_err(|_| BootError::InvalidElf("segment file size out of range"))?;
    let mem_size = usize::try_from(ph.mem_size())
        .map_err(|_| BootError::InvalidElf("segment memory size out of range"))?;
    let file_offset = usize::try_from(ph.offset())
        .map_err(|_| BootError::InvalidElf("segment offset out of range"))?;

    if file_size > mem_size {
        return Err(BootError::InvalidElf(
            "segment file size exceeds memory size",
        ));
    }
    if mem_size == 0 {
        return Ok(());
    }

    let file_end = file_offset
        .checked_add(file_size)
        .ok_or(BootError::InvalidElf("segment file bounds overflow"))?;
    if file_end > kernel_image.len() {
        return Err(BootError::InvalidElf(
            "segment file range is outside ELF image",
        ));
    }

    let (segment_addr, segment_end) = validated_segment_bounds(ph)?;

    let page_base = align_down(segment_addr, PAGE_SIZE);
    let page_end = align_up(segment_end, PAGE_SIZE)
        .ok_or(BootError::InvalidElf("segment end alignment overflow"))?;
    let page_count = (page_end - page_base) / PAGE_SIZE;

    let segment_memory = boot::allocate_pages(
        AllocateType::Address(page_base as u64),
        MemoryType::LOADER_DATA,
        page_count,
    )
    .map_err(|err| {
        let status = err.status();
        uefi::println!(
            "segment alloc failed: status={:?} range={:#x}..{:#x} pages={}",
            status,
            page_base,
            page_end,
            page_count
        );
        BootError::SegmentAlloc(status)
    })?;

    let page_offset = segment_addr - page_base;
    let segment_dest = unsafe { segment_memory.as_ptr().add(page_offset) };

    unsafe {
        ptr::write_bytes(segment_memory.as_ptr(), 0, page_count * PAGE_SIZE);
        ptr::copy_nonoverlapping(
            kernel_image.as_ptr().add(file_offset),
            segment_dest,
            file_size,
        );
    }

    Ok(())
}

fn validate_kernel_entry(entry_point: usize) -> Result<(), BootError> {
    if entry_point < MIN_KERNEL_LOAD_ADDR {
        return Err(BootError::InvalidElf(
            "entry point is below minimum load address",
        ));
    }
    if entry_point >= MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err(BootError::InvalidElf(
            "entry point is above maximum load address",
        ));
    }
    Ok(())
}

fn validated_segment_bounds(ph: &ProgramHeader<'_>) -> Result<(usize, usize), BootError> {
    let mem_size = usize::try_from(ph.mem_size())
        .map_err(|_| BootError::InvalidElf("segment memory size out of range"))?;
    if mem_size == 0 {
        return Err(BootError::InvalidElf(
            "PT_LOAD segment has zero memory size",
        ));
    }

    let segment_addr = segment_addr(ph)?;
    if segment_addr < MIN_KERNEL_LOAD_ADDR {
        return Err(BootError::InvalidElf(
            "segment address is below minimum load address",
        ));
    }

    let segment_end = segment_addr
        .checked_add(mem_size)
        .ok_or(BootError::InvalidElf("segment address overflow"))?;
    if segment_end > MAX_KERNEL_LOAD_END_EXCLUSIVE {
        return Err(BootError::InvalidElf(
            "segment address exceeds maximum load range",
        ));
    }

    Ok((segment_addr, segment_end))
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

fn segment_addr(ph: &ProgramHeader<'_>) -> Result<usize, BootError> {
    let physical_addr = usize::try_from(ph.physical_addr())
        .map_err(|_| BootError::InvalidElf("segment physical address out of range"))?;
    if physical_addr != 0 {
        return Ok(physical_addr);
    }

    usize::try_from(ph.virtual_addr())
        .map_err(|_| BootError::InvalidElf("segment virtual address out of range"))
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
