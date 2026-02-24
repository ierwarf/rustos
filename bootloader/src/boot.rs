use alloc::vec::Vec;

use uefi::boot;
use uefi::fs::{Error as FsError, FileSystem};
use uefi::prelude::*;

use crate::elf_loader::load_kernel_elf;
use crate::error::BootError;
use crate::gui;

const KERNEL_CANDIDATE_PATHS: [&uefi::CStr16; 2] = [cstr16!("\\kernel.elf"), cstr16!("kernel.elf")];

pub fn boot_kernel() -> Result<(), BootError> {
    let kernel_image = read_kernel_image()?;
    let (entry_point, segment_count) = load_kernel_elf(&kernel_image)?;
    if segment_count == 0 {
        return Err(BootError::InvalidElf("no PT_LOAD segments"));
    }
    let boot_info = gui::prepare_boot_info()?;
    let boot_info_ptr = gui::allocate_boot_info(boot_info)?;

    uefi::println!("kernel entry point: {entry_point:#x}");
    uefi::println!("loaded segments: {segment_count}");
    uefi::println!(
        "framebuffer: {}x{} stride={} base={:#x} back={:#x}",
        boot_info.framebuffer.width,
        boot_info.framebuffer.height,
        boot_info.framebuffer.stride,
        boot_info.framebuffer.addr,
        boot_info.framebuffer.back_buffer_addr
    );
    uefi::println!("exiting boot services");

    exit_boot_services_and_jump(entry_point, boot_info_ptr)
}

fn read_kernel_image() -> Result<Vec<u8>, BootError> {
    let sfs = boot::get_image_file_system(boot::image_handle())
        .map_err(|err| BootError::OpenFileSystem(err.status()))?;

    let mut fs = FileSystem::new(sfs);
    for path in KERNEL_CANDIDATE_PATHS {
        match fs.read(path) {
            Ok(kernel_image) => return Ok(kernel_image),
            Err(err) => {
                let status = fs_error_status(&err);
                if status != Status::NOT_FOUND {
                    return Err(BootError::ReadKernel(status));
                }
            }
        }
    }

    Err(BootError::ReadKernel(Status::NOT_FOUND))
}

fn fs_error_status(err: &FsError) -> Status {
    match err {
        FsError::Io(io) => io.uefi_error.status(),
        FsError::Path(_) | FsError::Utf8Encoding(_) => Status::LOAD_ERROR,
    }
}

fn exit_boot_services_and_jump(entry_point: usize, boot_info_ptr: *const gui::BootInfo) -> ! {
    unsafe {
        let _memory_map = boot::exit_boot_services(None);
        let kernel_entry: extern "sysv64" fn(*const gui::BootInfo) -> ! =
            core::mem::transmute(entry_point);
        kernel_entry(boot_info_ptr);
    }
}
