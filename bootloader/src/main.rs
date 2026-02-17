#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::proto::media::file::{File, FileMode, FileAttribute};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{MemoryType, AllocateType};
use core::slice;
use xmas_elf::ElfFile;

#[entry]
fn main(handle: Handle, mut st: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut st).unwrap();

    uefi_services::println!("Bootloader started...");

    match load_and_execute_kernel(handle, &st) {
        Ok(_) => Status::SUCCESS,
        Err(e) => {
            uefi_services::println!("Error loading kernel: {:?}", e);
            Status::LOAD_ERROR
        }
    }
}

fn load_and_execute_kernel(_handle: Handle, st: &SystemTable<Boot>) -> uefi::Result<()> {
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

    // 커널 로드 메모리 할당
    let pages = (file_size + 0xfff) / 0x1000;
    let kernel_buffer = bs.allocate_pages(
        AllocateType::AnyPages,
        MemoryType::LOADER_DATA,
        pages,
    )?;

    // 파일 읽기
    let kernel_data = unsafe {
        slice::from_raw_parts_mut(kernel_buffer as *mut u8, file_size)
    };
    kernel_file.read(kernel_data)?;

    uefi_services::println!("Kernel loaded into memory");

    // ELF 파일 파싱
    let elf = ElfFile::new(kernel_data).map_err(|_| uefi::Status::LOAD_ERROR)?;

    uefi_services::println!("ELF file parsed");

    // 프로그램 헤더 처리 (LOAD 세그먼트 로드)
    for header in elf.program_iter() {
        if header.get_type() != Ok(xmas_elf::program::Type::Load) {
            continue;
        }

        let vaddr = header.virtual_addr() as usize;
        let memsz = header.mem_size() as usize;
        let filesz = header.file_size() as usize;

        uefi_services::println!("Loading segment at {:#x}, size {:#x}", vaddr, memsz);

        // 메모리 할당
        let seg_pages = (memsz + 0xfff) / 0x1000;
        let _ = bs.allocate_pages(
            AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            seg_pages,
        )?;

        // 파일 내용 복사
        let src_offset = header.offset() as usize;
        let src = &kernel_data[src_offset..src_offset + filesz];
        let dst = unsafe { slice::from_raw_parts_mut(vaddr as *mut u8, memsz) };
        dst[..filesz].copy_from_slice(src);
        // 나머지는 0으로 초기화
        for i in filesz..memsz {
            dst[i] = 0;
        }
    }

    uefi_services::println!("Kernel segments loaded");

    // 엔트리 포인트 가져오기
    let entry_point = elf.header.pt2.entry_point() as usize;
    uefi_services::println!("Entry point: {:#x}", entry_point);

    uefi_services::println!("Jumping to kernel...");

    // 커널로 점프
    unsafe {
        let kernel_entry: fn() = core::mem::transmute(entry_point);
        kernel_entry();
    }

    Ok(())
}
