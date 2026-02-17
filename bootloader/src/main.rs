#![no_std]
#![no_main]

use core::slice;
use uefi::prelude::*;
use uefi::proto::media::file::{File, FileAttribute, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, MemoryType};
use xmas_elf::program::Type as ProgramType;
use xmas_elf::ElfFile;

const PAGE_SIZE: usize = 0x1000;

#[entry]
fn main(_handle: Handle, mut st: SystemTable<Boot>) -> Status {
    if let Err(e) = uefi_services::init(&mut st) {
        return e.status();
    }

    uefi_services::println!("Bootloader started...");

    match load_and_execute_kernel(st) {
        Ok(_) => Status::SUCCESS,
        Err(e) => {
            uefi_services::println!("Error loading kernel: {:?}", e);
            Status::LOAD_ERROR
        }
    }

}
fn load_and_execute_kernel(st: SystemTable<Boot>) -> uefi::Result<()> {
    let entry_point = load_kernel_elf(&st)?;
    uefi_services::println!("Kernel entry point: {:#x}", entry_point);
    uefi_services::println!("Exiting boot services...");

    let (_runtime_st, _memory_map) = st.exit_boot_services(MemoryType::LOADER_DATA);

    // After exit_boot_services, UEFI boot services and logger are no longer usable.
    unsafe {
        let kernel_entry: extern "sysv64" fn() -> ! = core::mem::transmute(entry_point);
        kernel_entry();
    }
}

fn load_kernel_elf(st: &SystemTable<Boot>) -> uefi::Result<usize> {
    uefi_services::println!("Loading kernel.elf...");

    let bs = st.boot_services();

    // SimpleFileSystem 프로토콜 가져오기
    let fs_handle = bs.get_handle_for_protocol::<SimpleFileSystem>()?;
    let mut fs = bs.open_protocol_exclusive::<SimpleFileSystem>(fs_handle)?;

    // 루트 디렉토리 열기
    let mut root = fs.open_volume()?;

    // kernel.elf 파일 열기
    let kernel_file = root.open(
        cstr16!("kernel.elf"),
        FileMode::Read,
        FileAttribute::empty(),
    )?;

    let mut kernel_file = kernel_file.into_regular_file().ok_or(uefi::Status::LOAD_ERROR)?;

    // 파일 크기 확인
    let mut info_buf: [u8; 256] = [0; 256];
    let file_info = kernel_file.get_info::<uefi::proto::media::file::FileInfo>(&mut info_buf)
        .map_err(|_| uefi::Status::LOAD_ERROR)?;
    let file_size = file_info.file_size() as usize;

    uefi_services::println!("Kernel size: {} bytes", file_size);
    if file_size == 0 {
        return Err(uefi::Status::LOAD_ERROR.into());
    }

    // 커널 로드 메모리 할당
    let pages = pages_for(file_size).ok_or(uefi::Status::LOAD_ERROR)?;
    let kernel_buffer = bs.allocate_pages(
        AllocateType::AnyPages,
        MemoryType::LOADER_DATA,
        pages,
    )?;

    // 파일 읽기
    let kernel_data = unsafe {
        slice::from_raw_parts_mut(kernel_buffer as *mut u8, file_size)
    };
    let read_size = kernel_file.read(kernel_data)?;
    if read_size != file_size {
        return Err(uefi::Status::LOAD_ERROR.into());
    }

    uefi_services::println!("Kernel loaded into memory");

    // ELF 파일 파싱
    let elf = ElfFile::new(kernel_data).map_err(|_| uefi::Status::LOAD_ERROR)?;

    uefi_services::println!("ELF file parsed");

    // 프로그램 헤더 처리 (LOAD 세그먼트 로드)
    for header in elf.program_iter() {
        if header.get_type() != Ok(ProgramType::Load) {
            continue;
        }

        let vaddr = header.virtual_addr() as usize;
        let memsz = header.mem_size() as usize;
        let filesz = header.file_size() as usize;
        let src_offset = header.offset() as usize;

        if memsz == 0 {
            continue;
        }
        if filesz > memsz {
            return Err(uefi::Status::LOAD_ERROR.into());
        }
        let src_end = src_offset
            .checked_add(filesz)
            .ok_or(uefi::Status::LOAD_ERROR)?;
        if src_end > kernel_data.len() {
            return Err(uefi::Status::LOAD_ERROR.into());
        }

        uefi_services::println!("Loading segment at {:#x}, size {:#x}", vaddr, memsz);

        // ELF p_vaddr 기준으로 페이지를 직접 할당해야 실제 점프 주소와 일치한다.
        let page_offset = vaddr & (PAGE_SIZE - 1);
        let seg_base = vaddr
            .checked_sub(page_offset)
            .ok_or(uefi::Status::LOAD_ERROR)?;
        let alloc_len = memsz
            .checked_add(page_offset)
            .ok_or(uefi::Status::LOAD_ERROR)?;
        let seg_pages = pages_for(alloc_len).ok_or(uefi::Status::LOAD_ERROR)?;
        let _actual = bs.allocate_pages(
            AllocateType::Address(seg_base as u64),
            MemoryType::LOADER_DATA,
            seg_pages,
        )?;

        // 파일 내용 복사
        let src = &kernel_data[src_offset..src_end];
        let dst = unsafe { slice::from_raw_parts_mut(vaddr as *mut u8, memsz) };
        dst[..filesz].copy_from_slice(src);
        dst[filesz..].fill(0);
    }

    uefi_services::println!("Kernel segments loaded");

    // 엔트리 포인트 가져오기
    let entry_point = elf.header.pt2.entry_point() as usize;
    Ok(entry_point)
}

fn pages_for(size: usize) -> Option<usize> {
    size.checked_add(PAGE_SIZE - 1).map(|n| n / PAGE_SIZE)
}
