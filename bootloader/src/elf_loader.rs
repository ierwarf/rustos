use core::convert::TryFrom;
use core::ptr;

use uefi::boot::{self, AllocateType, MemoryType};
use xmas_elf::header::{Class, Data, Machine, Type as ElfType};
use xmas_elf::program::{ProgramHeader, Type as ProgramType};
use xmas_elf::ElfFile;

use crate::error::BootError;

const PAGE_SIZE: usize = 0x1000;

pub fn load_kernel_elf(kernel_image: &[u8]) -> Result<(usize, usize), BootError> {
    let elf = ElfFile::new(kernel_image).map_err(BootError::InvalidElf)?;
    validate_elf_header(&elf)?;

    let mut loaded_segments = 0usize;
    for ph in elf.program_iter() {
        let ph_type = ph.get_type().map_err(BootError::InvalidElf)?;
        if ph_type != ProgramType::Load {
            continue;
        }

        load_segment(kernel_image, &ph)?;
        loaded_segments += 1;
    }

    let entry_point = usize::try_from(elf.header.pt2.entry_point())
        .map_err(|_| BootError::InvalidElf("entry point out of range"))?;

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
        return Err(BootError::InvalidElf("ELF type is not executable/shared object"));
    }

    Ok(())
}

fn load_segment(kernel_image: &[u8], ph: &ProgramHeader<'_>) -> Result<(), BootError> {
    let file_size = usize::try_from(ph.file_size())
        .map_err(|_| BootError::InvalidElf("segment file size out of range"))?;
    let mem_size = usize::try_from(ph.mem_size())
        .map_err(|_| BootError::InvalidElf("segment memory size out of range"))?;
    let file_offset =
        usize::try_from(ph.offset()).map_err(|_| BootError::InvalidElf("segment offset out of range"))?;

    if file_size > mem_size {
        return Err(BootError::InvalidElf("segment file size exceeds memory size"));
    }
    if mem_size == 0 {
        return Ok(());
    }

    let file_end = file_offset
        .checked_add(file_size)
        .ok_or(BootError::InvalidElf("segment file bounds overflow"))?;
    if file_end > kernel_image.len() {
        return Err(BootError::InvalidElf("segment file range is outside ELF image"));
    }

    let segment_addr = segment_addr(ph)?;
    let segment_end = segment_addr
        .checked_add(mem_size)
        .ok_or(BootError::InvalidElf("segment address overflow"))?;

    let page_base = align_down(segment_addr, PAGE_SIZE);
    let page_end =
        align_up(segment_end, PAGE_SIZE).ok_or(BootError::InvalidElf("segment end alignment overflow"))?;
    let page_count = (page_end - page_base) / PAGE_SIZE;

    let segment_memory = boot::allocate_pages(
        AllocateType::Address(page_base as u64),
        MemoryType::LOADER_DATA,
        page_count,
    )
    .map_err(|err| BootError::SegmentAlloc(err.status()))?;

    let page_offset = segment_addr - page_base;
    let segment_dest = unsafe { segment_memory.as_ptr().add(page_offset) };

    unsafe {
        ptr::write_bytes(segment_memory.as_ptr(), 0, page_count * PAGE_SIZE);
        ptr::copy_nonoverlapping(kernel_image.as_ptr().add(file_offset), segment_dest, file_size);
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
