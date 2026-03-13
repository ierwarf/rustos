pub(crate) mod boot_info;
pub(crate) mod elf_loader;
pub(crate) mod error;
mod file_cache;

use alloc::vec::Vec;

use uefi::boot;
use uefi::fs::{Error as FsError, FileSystem};
use uefi::prelude::*;

use self::boot_info::BootInfo;
use self::elf_loader::load_elf_image;
use self::error::BootError;
use crate::debug;
use crate::gui;

const PREKERNEL_CANDIDATE_PATHS: [(&str, &uefi::CStr16); 4] = [
    ("\\prekernel.elf", cstr16!("\\prekernel.elf")),
    ("prekernel.elf", cstr16!("prekernel.elf")),
    (
        "\\EFI\\BOOT\\prekernel.elf",
        cstr16!("\\EFI\\BOOT\\prekernel.elf"),
    ),
    (
        "EFI\\BOOT\\prekernel.elf",
        cstr16!("EFI\\BOOT\\prekernel.elf"),
    ),
];

pub fn boot_prekernel() -> Result<(), BootError> {
    debug::println!("bootloader: reading prekernel image");
    let prekernel_image = read_stage_image()?;
    debug::println!(
        "bootloader: prekernel image loaded, {} bytes",
        prekernel_image.len()
    );
    let (entry_point, segment_count) = load_elf_image(&prekernel_image)?;
    debug::println!(
        "bootloader: ELF loaded, entry={:#x}, segments={}",
        entry_point,
        segment_count
    );
    if segment_count == 0 {
        return Err(BootError::InvalidElf("no PT_LOAD segments"));
    }
    let boot_files = file_cache::snapshot_boot_volume()?;
    let mut boot_info = gui::prepare_boot_info()?;
    boot_info.boot_files = boot_files;
    let boot_info_ptr = gui::allocate_boot_info(boot_info)?;
    debug::println!(
        "bootloader: boot info prepared, fb={:#x}, back={:#x}",
        boot_info.framebuffer.addr,
        boot_info.framebuffer.back_buffer_addr
    );

    uefi::println!("prekernel entry point: {entry_point:#x}");
    uefi::println!("prekernel loaded segments: {segment_count}");
    uefi::println!(
        "framebuffer: {}x{} stride={} base={:#x} back={:#x}",
        boot_info.framebuffer.width,
        boot_info.framebuffer.height,
        boot_info.framebuffer.stride,
        boot_info.framebuffer.addr,
        boot_info.framebuffer.back_buffer_addr
    );
    uefi::println!("exiting boot services");
    debug::println!("bootloader: exiting boot services");

    exit_boot_services_and_jump(entry_point, boot_info_ptr)
}

fn read_stage_image() -> Result<Vec<u8>, BootError> {
    let sfs = boot::get_image_file_system(boot::image_handle())
        .map_err(|err| BootError::OpenFileSystem(err.status()))?;

    let mut fs = FileSystem::new(sfs);
    for (display_path, path) in PREKERNEL_CANDIDATE_PATHS {
        match fs.read(path) {
            Ok(stage_image) => {
                debug::println!("bootloader: found prekernel at {}", display_path);
                uefi::println!(
                    "prekernel image found: {display_path} ({} bytes)",
                    stage_image.len()
                );
                return Ok(stage_image);
            }
            Err(err) => {
                let status = fs_error_status(&err);
                if status != Status::NOT_FOUND {
                    return Err(BootError::ReadStage(status));
                }
            }
        }
    }

    uefi::println!("prekernel image not found; tried:");
    debug::println!("bootloader: prekernel image not found");
    for (display_path, _) in PREKERNEL_CANDIDATE_PATHS {
        uefi::println!("  - {display_path}");
    }

    Err(BootError::ReadStage(Status::NOT_FOUND))
}

fn fs_error_status(err: &FsError) -> Status {
    match err {
        FsError::Io(io) => io.uefi_error.status(),
        FsError::Path(_) | FsError::Utf8Encoding(_) => Status::LOAD_ERROR,
    }
}

fn exit_boot_services_and_jump(entry_point: usize, boot_info_ptr: *const BootInfo) -> ! {
    unsafe {
        let _memory_map = boot::exit_boot_services(None);
        let kernel_entry: extern "sysv64" fn(*const BootInfo) -> ! =
            core::mem::transmute(entry_point);
        kernel_entry(boot_info_ptr)
    }
}
