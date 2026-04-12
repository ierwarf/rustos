pub(crate) mod boot_info;
pub(crate) mod elf_loader;
pub(crate) mod error;
mod file_cache;

use core::ptr;

use uefi::boot;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use uefi::prelude::*;
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode};

use self::boot_info::{BootInfo, BootMemoryKind, BootMemoryRegion, NucleusImageInfo};
use self::elf_loader::{load_kernel_elf, UefiKernelFile};
use self::error::BootError;
use crate::debug;
use crate::gui;
#[cfg(rustos_kernel_physical_kaslr_enabled)]
use crate::settings;

const PAGE_SIZE: usize = 4096;
const BOOT_MEMORY_MAP_STORAGE_PAGES: usize = 32;

const NUCLEUS_CANDIDATE_PATHS: [(&str, &uefi::CStr16); 4] = [
    ("\\nucleus.elf", cstr16!("\\nucleus.elf")),
    ("nucleus.elf", cstr16!("nucleus.elf")),
    (
        "\\EFI\\BOOT\\nucleus.elf",
        cstr16!("\\EFI\\BOOT\\nucleus.elf"),
    ),
    ("EFI\\BOOT\\nucleus.elf", cstr16!("EFI\\BOOT\\nucleus.elf")),
];

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub fn boot_kernel() -> Result<(), BootError> {
    debug::println!("bootloader: locating nucleus image");
    let mut boot_info = gui::prepare_boot_info()?;
    let (nucleus_path, mut nucleus_file, nucleus_size) = open_kernel_file()?;
    debug::println!(
        "bootloader: nucleus image found at {}, {} bytes",
        nucleus_path,
        nucleus_size
    );
    let kernel_physical_slide = choose_kernel_physical_slide(boot_info.rng_seed);
    debug::println!(
        "bootloader: kernel physical slide(raw)={:#x}",
        kernel_physical_slide
    );
    let (entry_point, segment_count, load_bias, kernel_phys_start, kernel_size_bytes) =
        load_kernel_elf(&mut nucleus_file, nucleus_size, kernel_physical_slide)?;
    let applied_slide = load_bias.saturating_sub(0x0020_0000);
    boot_info.nucleus_image = NucleusImageInfo {
        phys_start: kernel_phys_start as u64,
        size: kernel_size_bytes as u64,
        load_bias: load_bias as u64,
        entry_point: entry_point as u64,
    };
    debug::println!(
        "bootloader: kernel ELF loaded, entry={:#x}, segments={}, load_bias={:#x}, image=[{:#x}, {:#x}), applied_slide={:#x}",
        entry_point,
        segment_count,
        load_bias,
        kernel_phys_start,
        kernel_phys_start.saturating_add(kernel_size_bytes),
        applied_slide
    );
    if segment_count == 0 {
        return Err(BootError::InvalidElf("no PT_LOAD segments"));
    }
    boot_info.boot_volume = match file_cache::extract_boot_volume_identity() {
        Ok(identity) => identity,
        Err(err) => {
            debug::println!(
                "bootloader: boot volume identity unavailable: {:?}; continuing with runtime probe",
                err
            );
            boot_info::BootVolumeIdentity::empty()
        }
    };

    let boot_info_ptr = gui::allocate_boot_info(boot_info)?;
    let boot_memory_map_storage = allocate_boot_memory_map_storage()?;
    debug::println!(
        "bootloader: boot info prepared, fb={:#x}, back={:#x}",
        boot_info.framebuffer.addr,
        boot_info.framebuffer.back_buffer_addr
    );

    uefi::println!("kernel entry point: {entry_point:#x}");
    uefi::println!("kernel loaded segments: {segment_count}");
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

fn open_kernel_file() -> Result<(&'static str, UefiKernelFile, u64), BootError> {
    let mut sfs = boot::get_image_file_system(boot::image_handle())
        .map_err(|err| BootError::OpenFileSystem(err.status()))?;
    let mut root = sfs
        .open_volume()
        .map_err(|err| BootError::OpenFileSystem(err.status()))?;

    for (display_path, path) in NUCLEUS_CANDIDATE_PATHS {
        let handle = match root.open(path, FileMode::Read, FileAttribute::empty()) {
            Ok(handle) => handle,
            Err(err) if err.status() == Status::NOT_FOUND => continue,
            Err(err) => return Err(BootError::ReadKernel(err.status())),
        };
        let mut file = handle
            .into_regular_file()
            .ok_or(BootError::ReadKernel(Status::LOAD_ERROR))?;
        let info = file
            .get_boxed_info::<FileInfo>()
            .map_err(|err| BootError::ReadKernel(err.status()))?;
        debug::println!("bootloader: found nucleus at {}", display_path);
        return Ok((display_path, UefiKernelFile::new(file), info.file_size()));
    }

    uefi::println!("nucleus image not found; tried:");
    debug::println!("bootloader: nucleus image not found");
    for (display_path, _) in NUCLEUS_CANDIDATE_PATHS {
        uefi::println!("  - {display_path}");
    }
    Err(BootError::ReadKernel(Status::NOT_FOUND))
}

#[cfg(rustos_kernel_physical_kaslr_enabled)]
fn choose_kernel_physical_slide(seed: [u8; 32]) -> usize {
    if !boot_protocol::rng_seed_usable(seed) {
        return 0;
    }

    let mut value = 0_u64;
    for chunk in seed.chunks_exact(8) {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        value ^= u64::from_le_bytes(word);
        value = splitmix64(value);
    }
    let max_slide = u64::try_from(settings::MAX_KERNEL_PHYSICAL_KASLR_SLIDE.max(0)).unwrap_or(0);
    if max_slide == 0 {
        0
    } else {
        (value % (max_slide + 1)) as usize
    }
}

#[cfg(not(rustos_kernel_physical_kaslr_enabled))]
fn choose_kernel_physical_slide(_seed: [u8; 32]) -> usize {
    0
}

#[cfg(rustos_kernel_physical_kaslr_enabled)]
fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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
        if let Err(error) = boot_info.validate() {
            debug::println!(
                "bootloader: refusing to jump with invalid boot info: {}",
                error.as_str()
            );
            loop {
                core::hint::spin_loop();
            }
        }
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
