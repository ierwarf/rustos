pub(crate) mod boot_info;
pub(crate) mod elf_loader;
pub(crate) mod error;
mod file_cache;

use alloc::vec::Vec;
use core::ptr;

use uefi::boot;
use uefi::fs::{Error as FsError, FileSystem};
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use uefi::prelude::*;

use self::boot_info::{BootInfo, BootMemoryKind, BootMemoryRegion};
use self::elf_loader::load_elf_image;
use self::error::BootError;
use crate::debug;
use crate::gui;

const PAGE_SIZE: usize = 4096;
const BOOT_MEMORY_MAP_STORAGE_PAGES: usize = 32;

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
    let boot_memory_map_storage = allocate_boot_memory_map_storage()?;
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

    exit_boot_services_and_jump(entry_point, boot_info_ptr, boot_memory_map_storage)
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

fn allocate_boot_memory_map_storage() -> Result<*mut BootMemoryRegion, BootError> {
    let ptr = boot::allocate_pages(
        boot::AllocateType::AnyPages,
        boot::MemoryType::LOADER_DATA,
        BOOT_MEMORY_MAP_STORAGE_PAGES,
    )
    .map_err(|err| BootError::BootMemoryMapAlloc(err.status()))?;
    unsafe {
        ptr::write_bytes(ptr.as_ptr(), 0, BOOT_MEMORY_MAP_STORAGE_PAGES * PAGE_SIZE);
    }
    Ok(ptr.as_ptr().cast())
}

fn exit_boot_services_and_jump(
    entry_point: usize,
    boot_info_ptr: *const BootInfo,
    boot_memory_map_storage: *mut BootMemoryRegion,
) -> ! {
    unsafe {
        let memory_map = boot::exit_boot_services(None);
        let boot_info = &mut *boot_info_ptr.cast_mut();
        let memory_map_entry_capacity =
            (BOOT_MEMORY_MAP_STORAGE_PAGES * PAGE_SIZE) / core::mem::size_of::<BootMemoryRegion>();
        let entry_count = populate_boot_memory_map(
            boot_info,
            &memory_map,
            boot_memory_map_storage,
            memory_map_entry_capacity,
        );
        boot_info.memory_map.entries_ptr = boot_memory_map_storage as u64;
        boot_info.memory_map.entry_count = entry_count as u32;
        debug::println!(
            "bootloader: memory map summarized: regions={}",
            boot_info.memory_map.entry_count
        );
        let kernel_entry: extern "sysv64" fn(*const BootInfo) -> ! =
            core::mem::transmute(entry_point);
        kernel_entry(boot_info_ptr)
    }
}

fn populate_boot_memory_map(
    boot_info: &BootInfo,
    memory_map: &MemoryMapOwned,
    output: *mut BootMemoryRegion,
    capacity: usize,
) -> usize {
    if output.is_null() || capacity == 0 {
        return 0;
    }

    let output = unsafe { core::slice::from_raw_parts_mut(output, capacity) };
    let mut count = 0usize;

    for descriptor in memory_map.entries() {
        if descriptor.page_count == 0 {
            continue;
        }

        let region = BootMemoryRegion {
            phys_start: descriptor.phys_start,
            page_count: descriptor.page_count,
            kind: descriptor_kind(boot_info, descriptor),
            _reserved0: 0,
        };

        if let Some(previous) = output.get_mut(count.saturating_sub(1)) {
            let previous_end =
                previous.phys_start + previous.page_count.saturating_mul(PAGE_SIZE as u64);
            if previous.kind == region.kind && previous_end == region.phys_start {
                previous.page_count = previous.page_count.saturating_add(region.page_count);
                continue;
            }
        }

        if count >= output.len() {
            break;
        }

        output[count] = region;
        count += 1;
    }

    count
}

fn descriptor_kind(
    boot_info: &BootInfo,
    descriptor: &uefi::mem::memory_map::MemoryDescriptor,
) -> BootMemoryKind {
    if descriptor_overlaps_framebuffer(
        descriptor,
        boot_info.framebuffer.addr,
        boot_info.framebuffer.size,
    ) || descriptor_overlaps_framebuffer(
        descriptor,
        boot_info.framebuffer.back_buffer_addr,
        boot_info.framebuffer.back_buffer_size,
    ) {
        return BootMemoryKind::Framebuffer;
    }

    match descriptor.ty {
        MemoryType::CONVENTIONAL => BootMemoryKind::Usable,
        MemoryType::ACPI_RECLAIM => BootMemoryKind::AcpiReclaim,
        MemoryType::ACPI_NON_VOLATILE => BootMemoryKind::AcpiNvs,
        MemoryType::MMIO | MemoryType::MMIO_PORT_SPACE => BootMemoryKind::Mmio,
        _ => BootMemoryKind::Reserved,
    }
}

fn descriptor_overlaps_framebuffer(
    descriptor: &uefi::mem::memory_map::MemoryDescriptor,
    base: u64,
    size: u64,
) -> bool {
    if base == 0 || size == 0 || descriptor.page_count == 0 {
        return false;
    }

    let desc_start = descriptor.phys_start;
    let desc_end =
        desc_start.saturating_add(descriptor.page_count.saturating_mul(PAGE_SIZE as u64));
    let range_end = base.saturating_add(size);
    desc_start < range_end && base < desc_end
}
