use core::arch::{
    asm,
    x86_64::{__cpuid, __cpuid_count},
};
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr;

use boot_protocol::{
    BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootInfo, BootMemoryKind, BootMemoryMap, BootMemoryRegion,
    BootPixelFormat, BootVolumeIdentity, EarlySystemImage, FramebufferInfo, NucleusImageInfo,
    rng_seed_usable,
};

const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36d7_6289;
const PAGE_SIZE: u64 = 4096;
const MAX_BOOT_MEMORY_REGIONS: usize = 256;
const CPUID_RDRAND: u32 = 1 << 30;
const CPUID_RDSEED: u32 = 1 << 18;
const HARDWARE_RNG_RETRIES: usize = 128;

const TAG_END: u32 = 0;
const TAG_MODULE: u32 = 3;
const TAG_MMAP: u32 = 6;
const TAG_FRAMEBUFFER: u32 = 8;
const TAG_ACPI_OLD: u32 = 14;
const TAG_ACPI_NEW: u32 = 15;
const TAG_EFI_MMAP: u32 = 17;
const TAG_LOAD_BASE_ADDR: u32 = 21;
const EARLY_SYSTEM_MODULE_CMDLINE: &[u8] = b"rustos-early-system";

#[repr(C)]
struct RawTag {
    ty: u32,
    size: u32,
}

#[repr(C, packed)]
struct RawMmapEntry {
    addr: u64,
    len: u64,
    ty: u32,
    zero: u32,
}

#[repr(C, packed)]
struct RawFramebufferTag {
    ty: u32,
    size: u32,
    addr: u64,
    pitch: u32,
    width: u32,
    height: u32,
    bpp: u8,
    framebuffer_type: u8,
    reserved: u16,
    red_position: u8,
    red_mask_size: u8,
    green_position: u8,
    green_mask_size: u8,
    blue_position: u8,
    blue_mask_size: u8,
}

#[repr(C, packed)]
struct RawLoadBaseTag {
    ty: u32,
    size: u32,
    load_base_addr: u32,
}

#[repr(C, packed)]
struct RawModuleTag {
    ty: u32,
    size: u32,
    mod_start: u32,
    mod_end: u32,
}

struct BootInfoStorage(UnsafeCell<BootInfo>);
struct MemoryMapStorage(UnsafeCell<[BootMemoryRegion; MAX_BOOT_MEMORY_REGIONS]>);

unsafe impl Sync for BootInfoStorage {}
unsafe impl Sync for MemoryMapStorage {}

static BOOT_INFO_STORAGE: BootInfoStorage = BootInfoStorage(UnsafeCell::new(empty_boot_info()));
static MEMORY_MAP_STORAGE: MemoryMapStorage = MemoryMapStorage(UnsafeCell::new(
    [empty_memory_region(); MAX_BOOT_MEMORY_REGIONS],
));

pub fn build_boot_info(magic: u32, mbi_addr: u32) -> *const BootInfo {
    if magic != MULTIBOOT2_BOOTLOADER_MAGIC || mbi_addr == 0 {
        fatal();
    }

    let tags = unsafe { Tags::new(mbi_addr as usize) };
    let framebuffer = tags.framebuffer().unwrap_or_else(FramebufferInfo::empty);
    let entry_count = tags.write_memory_map(unsafe { &mut *MEMORY_MAP_STORAGE.0.get() });
    if entry_count == 0 {
        fatal();
    }

    let load_base = tags.load_base_addr().unwrap_or(0x20_0000) as u64;
    let image_end = unsafe { kernel_image_end() };
    let image_size = image_end.saturating_sub(load_base).max(PAGE_SIZE);

    let Some(rng_seed) = hardware_rng_seed() else {
        fatal();
    };
    let boot_info = BootInfo {
        magic: BOOT_INFO_MAGIC,
        version: BOOT_INFO_VERSION,
        _reserved0: 0,
        rng_seed,
        acpi_rsdp_addr: tags.acpi_rsdp_addr().unwrap_or(0),
        boot_volume: BootVolumeIdentity::empty(),
        framebuffer,
        nucleus_image: NucleusImageInfo {
            phys_start: load_base,
            size: image_size,
            load_bias: load_base,
            entry_point: multiboot2_entry64_addr(),
        },
        memory_map: BootMemoryMap {
            entries_ptr: unsafe { (*MEMORY_MAP_STORAGE.0.get()).as_ptr() as u64 },
            entry_count: entry_count as u32,
            _reserved0: 0,
        },
        _reserved_storage_bootstrap: [0; 2],
        early_system_image: tags
            .early_system_image()
            .unwrap_or_else(EarlySystemImage::empty),
    };

    if boot_info.validate().is_err() {
        fatal();
    }

    unsafe {
        ptr::write(BOOT_INFO_STORAGE.0.get(), boot_info);
        BOOT_INFO_STORAGE.0.get().cast_const()
    }
}

fn hardware_rng_seed() -> Option<[u8; 32]> {
    let max_leaf = __cpuid(0).eax;
    if max_leaf < 1 {
        return None;
    }
    let rdrand = __cpuid(1).ecx & CPUID_RDRAND != 0;
    let rdseed = max_leaf >= 7 && __cpuid_count(7, 0).ebx & CPUID_RDSEED != 0;
    if !rdrand && !rdseed {
        return None;
    }

    let mut seed = [0_u8; 32];
    for chunk in seed.chunks_exact_mut(size_of::<u64>()) {
        let word = (rdseed.then(rdseed64).flatten()).or_else(|| rdrand.then(rdrand64).flatten())?;
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    rng_seed_usable(seed).then_some(seed)
}

fn rdseed64() -> Option<u64> {
    for _ in 0..HARDWARE_RNG_RETRIES {
        let mut value = 0_u64;
        let mut success = 0_u8;
        unsafe {
            asm!(
                "rdseed {value}",
                "setc {success}",
                value = out(reg) value,
                success = out(reg_byte) success,
                options(nomem, nostack),
            );
        }
        if success != 0 {
            return Some(value);
        }
        core::hint::spin_loop();
    }
    None
}

fn rdrand64() -> Option<u64> {
    for _ in 0..HARDWARE_RNG_RETRIES {
        let mut value = 0_u64;
        let mut success = 0_u8;
        unsafe {
            asm!(
                "rdrand {value}",
                "setc {success}",
                value = out(reg) value,
                success = out(reg_byte) success,
                options(nomem, nostack),
            );
        }
        if success != 0 {
            return Some(value);
        }
        core::hint::spin_loop();
    }
    None
}

struct Tags {
    base: usize,
    total_size: usize,
}

impl Tags {
    unsafe fn new(addr: usize) -> Self {
        let total_size = unsafe { ptr::read_unaligned(addr as *const u32) } as usize;
        if total_size < 16 {
            fatal();
        }
        Self {
            base: addr,
            total_size,
        }
    }

    fn framebuffer(&self) -> Option<FramebufferInfo> {
        let tag = self.find_tag(TAG_FRAMEBUFFER)?;
        if (tag.size as usize) < size_of::<RawFramebufferTag>() - 6 * size_of::<u8>()
            || tag.size as usize > self.total_size
        {
            return None;
        }
        let raw = unsafe { ptr::read_unaligned(tag as *const RawTag as *const RawFramebufferTag) };
        if raw.framebuffer_type != 1 || raw.bpp != 24 && raw.bpp != 32 {
            return None;
        }

        let bytes_per_pixel = (raw.bpp / 8).max(1);
        let pixel_format = match (raw.red_position, raw.blue_position) {
            (0, 16) => BootPixelFormat::Rgb,
            (16, 0) => BootPixelFormat::Bgr,
            _ => BootPixelFormat::Bitmask,
        };
        Some(FramebufferInfo {
            addr: raw.addr,
            size: (raw.pitch as u64).saturating_mul(raw.height as u64),
            back_buffer_addr: 0,
            back_buffer_size: 0,
            width: raw.width,
            height: raw.height,
            stride: raw.pitch / u32::from(bytes_per_pixel),
            pixel_format,
            bytes_per_pixel,
            _reserved: [0; 3],
        })
    }

    fn write_memory_map(&self, output: &mut [BootMemoryRegion]) -> usize {
        if let Some(count) = self.write_multiboot_memory_map(output) {
            return count;
        }
        self.write_efi_memory_map(output).unwrap_or(0)
    }

    fn write_multiboot_memory_map(&self, output: &mut [BootMemoryRegion]) -> Option<usize> {
        let tag = self.find_tag(TAG_MMAP)?;
        if tag.size < 16 {
            return None;
        }
        let entry_size = unsafe {
            ptr::read_unaligned((tag as *const RawTag).cast::<u8>().add(8).cast::<u32>())
        };
        if (entry_size as usize) < size_of::<RawMmapEntry>() {
            return None;
        }
        let mut count = 0usize;
        let mut cursor = tag as *const RawTag as usize + 16;
        let end = tag as *const RawTag as usize + tag.size as usize;
        while cursor + entry_size as usize <= end && count < output.len() {
            let raw = unsafe { ptr::read_unaligned(cursor as *const RawMmapEntry) };
            if let Some(region) = memory_region_from_range(raw.addr, raw.len, raw.ty) {
                append_region(output, &mut count, region);
            }
            cursor += entry_size as usize;
        }
        Some(count)
    }

    fn write_efi_memory_map(&self, output: &mut [BootMemoryRegion]) -> Option<usize> {
        let tag = self.find_tag(TAG_EFI_MMAP)?;
        if tag.size < 24 {
            return None;
        }
        let header = tag as *const RawTag as usize;
        let descr_size = unsafe { ptr::read_unaligned((header + 12) as *const u32) } as usize;
        if descr_size < 32 {
            return None;
        }

        let mut count = 0usize;
        let mut cursor = header + 24;
        let end = header + tag.size as usize;
        while cursor + descr_size <= end && count < output.len() {
            let ty = unsafe { ptr::read_unaligned(cursor as *const u32) };
            let phys_start = unsafe { ptr::read_unaligned((cursor + 8) as *const u64) };
            let page_count = unsafe { ptr::read_unaligned((cursor + 24) as *const u64) };
            if page_count != 0 {
                append_region(
                    output,
                    &mut count,
                    BootMemoryRegion {
                        phys_start,
                        page_count,
                        kind: efi_memory_kind(ty),
                        _reserved0: 0,
                    },
                );
            }
            cursor += descr_size;
        }
        Some(count)
    }

    fn acpi_rsdp_addr(&self) -> Option<u64> {
        self.find_tag(TAG_ACPI_NEW)
            .or_else(|| self.find_tag(TAG_ACPI_OLD))
            .map(|tag| unsafe { (tag as *const RawTag).cast::<u8>().add(8) as u64 })
    }

    fn load_base_addr(&self) -> Option<u32> {
        let tag = self.find_tag(TAG_LOAD_BASE_ADDR)?;
        if tag.size as usize >= size_of::<RawLoadBaseTag>() {
            Some(
                unsafe { ptr::read_unaligned(tag as *const RawTag as *const RawLoadBaseTag) }
                    .load_base_addr,
            )
        } else {
            None
        }
    }

    fn early_system_image(&self) -> Option<EarlySystemImage> {
        let (ptr, len) = self.module_range_by_cmdline(EARLY_SYSTEM_MODULE_CMDLINE)?;
        Some(EarlySystemImage { ptr, len })
    }

    fn module_range_by_cmdline(&self, expected_cmdline: &[u8]) -> Option<(u64, u64)> {
        let mut cursor = self.base + 8;
        let end = self.base.checked_add(self.total_size)?;
        let mut found = None;
        while cursor + size_of::<RawTag>() <= end {
            let tag = unsafe { &*(cursor as *const RawTag) };
            if tag.ty == TAG_END {
                break;
            }
            if tag.size < size_of::<RawTag>() as u32 || cursor + tag.size as usize > end {
                return None;
            }
            if tag.ty == TAG_MODULE
                && let Some(range) = self.module_range_from_tag(tag, expected_cmdline)
            {
                if found.is_some() {
                    return None;
                }
                found = Some(range);
            }
            cursor = align_up(cursor + tag.size as usize, 8)?;
        }
        found
    }

    fn module_range_from_tag(&self, tag: &RawTag, expected_cmdline: &[u8]) -> Option<(u64, u64)> {
        if (tag.size as usize) < size_of::<RawModuleTag>() + expected_cmdline.len() {
            return None;
        }
        let raw = unsafe { ptr::read_unaligned(tag as *const RawTag as *const RawModuleTag) };
        let start = u64::from(raw.mod_start);
        let end = u64::from(raw.mod_end);
        if end <= start {
            return None;
        }
        let cmdline_start = tag as *const RawTag as usize + size_of::<RawModuleTag>();
        let cmdline_len = tag.size as usize - size_of::<RawModuleTag>();
        let cmdline =
            unsafe { core::slice::from_raw_parts(cmdline_start as *const u8, cmdline_len) };
        let cmdline = cmdline.split(|byte| *byte == 0).next().unwrap_or(cmdline);
        if cmdline != expected_cmdline {
            return None;
        }
        Some((start, end - start))
    }

    fn find_tag(&self, ty: u32) -> Option<&RawTag> {
        let mut cursor = self.base + 8;
        let end = self.base.checked_add(self.total_size)?;
        while cursor + size_of::<RawTag>() <= end {
            let tag = unsafe { &*(cursor as *const RawTag) };
            if tag.ty == TAG_END {
                break;
            }
            if tag.size < size_of::<RawTag>() as u32 || cursor + tag.size as usize > end {
                return None;
            }
            if tag.ty == ty {
                return Some(tag);
            }
            cursor = align_up(cursor + tag.size as usize, 8)?;
        }
        None
    }
}

fn memory_region_from_range(phys_start: u64, len: u64, ty: u32) -> Option<BootMemoryRegion> {
    if len == 0 {
        return None;
    }
    let aligned_start = align_up_u64(phys_start, PAGE_SIZE)?;
    let end = phys_start.checked_add(len)?;
    let aligned_end = end / PAGE_SIZE * PAGE_SIZE;
    if aligned_end <= aligned_start {
        return None;
    }
    Some(BootMemoryRegion {
        phys_start: aligned_start,
        page_count: (aligned_end - aligned_start) / PAGE_SIZE,
        kind: match ty {
            1 => BootMemoryKind::Usable,
            3 => BootMemoryKind::AcpiReclaim,
            4 => BootMemoryKind::AcpiNvs,
            _ => BootMemoryKind::Reserved,
        },
        _reserved0: 0,
    })
}

fn append_region(output: &mut [BootMemoryRegion], count: &mut usize, region: BootMemoryRegion) {
    if region.page_count == 0 || *count >= output.len() {
        return;
    }
    if let Some(previous) = count.checked_sub(1).and_then(|index| output.get_mut(index)) {
        let previous_end = previous
            .phys_start
            .saturating_add(previous.page_count.saturating_mul(PAGE_SIZE));
        if previous.kind == region.kind && previous_end == region.phys_start {
            previous.page_count = previous.page_count.saturating_add(region.page_count);
            return;
        }
    }
    output[*count] = region;
    *count += 1;
}

fn efi_memory_kind(ty: u32) -> BootMemoryKind {
    match ty {
        7 => BootMemoryKind::Usable,
        9 => BootMemoryKind::AcpiReclaim,
        10 => BootMemoryKind::AcpiNvs,
        11 | 12 => BootMemoryKind::Mmio,
        _ => BootMemoryKind::Reserved,
    }
}

const fn empty_memory_region() -> BootMemoryRegion {
    BootMemoryRegion {
        phys_start: 0,
        page_count: 0,
        kind: BootMemoryKind::Reserved,
        _reserved0: 0,
    }
}

const fn empty_boot_info() -> BootInfo {
    BootInfo {
        magic: 0,
        version: 0,
        _reserved0: 0,
        rng_seed: [0; 32],
        acpi_rsdp_addr: 0,
        boot_volume: BootVolumeIdentity::empty(),
        framebuffer: FramebufferInfo::empty(),
        nucleus_image: NucleusImageInfo::empty(),
        memory_map: BootMemoryMap::empty(),
        _reserved_storage_bootstrap: [0; 2],
        early_system_image: EarlySystemImage::empty(),
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

fn align_up_u64(value: u64, align: u64) -> Option<u64> {
    value
        .checked_add(align - 1)
        .map(|value| value / align * align)
}

fn multiboot2_entry64_addr() -> u64 {
    unsafe extern "C" {
        fn multiboot2_entry64(magic: u32, mbi_addr: u32) -> !;
    }
    multiboot2_entry64 as *const () as usize as u64
}

unsafe fn kernel_image_end() -> u64 {
    unsafe extern "C" {
        static _end: u8;
    }
    unsafe { &_end as *const u8 as u64 }
}

fn fatal() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_multiboot_memory_region_to_pages() {
        let region = memory_region_from_range(0x1003, 0x5000, 1).expect("region");
        assert_eq!(region.phys_start, 0x2000);
        assert_eq!(region.page_count, 4);
        assert_eq!(region.kind, BootMemoryKind::Usable);
    }

    #[test]
    fn maps_efi_memory_types() {
        assert_eq!(efi_memory_kind(7), BootMemoryKind::Usable);
        assert_eq!(efi_memory_kind(9), BootMemoryKind::AcpiReclaim);
        assert_eq!(efi_memory_kind(10), BootMemoryKind::AcpiNvs);
        assert_eq!(efi_memory_kind(11), BootMemoryKind::Mmio);
    }

    #[test]
    fn appends_adjacent_regions() {
        let mut regions = [empty_memory_region(); 4];
        let mut count = 0;
        append_region(
            &mut regions,
            &mut count,
            BootMemoryRegion {
                phys_start: 0x1000,
                page_count: 1,
                kind: BootMemoryKind::Usable,
                _reserved0: 0,
            },
        );
        append_region(
            &mut regions,
            &mut count,
            BootMemoryRegion {
                phys_start: 0x2000,
                page_count: 2,
                kind: BootMemoryKind::Usable,
                _reserved0: 0,
            },
        );
        assert_eq!(count, 1);
        assert_eq!(regions[0].page_count, 3);
    }

    #[test]
    fn parses_rgb_and_bgr_framebuffers() {
        let rgb = tags_with_framebuffer(0, 16);
        let rgb_tags = unsafe { Tags::new(rgb.as_ptr() as usize) };
        let rgb_info = rgb_tags.framebuffer().expect("rgb framebuffer");
        assert_eq!(rgb_info.pixel_format, BootPixelFormat::Rgb);
        assert_eq!(rgb_info.bytes_per_pixel, 4);
        assert_eq!(rgb_info.stride, 800);
        assert_eq!(rgb_info.back_buffer_addr, 0);

        let bgr = tags_with_framebuffer(16, 0);
        let bgr_tags = unsafe { Tags::new(bgr.as_ptr() as usize) };
        assert_eq!(
            bgr_tags
                .framebuffer()
                .expect("bgr framebuffer")
                .pixel_format,
            BootPixelFormat::Bgr
        );
    }

    #[test]
    fn prefers_new_acpi_rsdp_over_old() {
        let mut mbi = mbi_header();
        push_acpi_tag(&mut mbi, TAG_ACPI_OLD, b"OLD");
        push_acpi_tag(&mut mbi, TAG_ACPI_NEW, b"NEW");
        finish_mbi(&mut mbi);

        let tags = unsafe { Tags::new(mbi.as_ptr() as usize) };
        let addr = tags.acpi_rsdp_addr().expect("acpi rsdp");
        let offset = addr as usize - mbi.as_ptr() as usize;
        assert_eq!(&mbi[offset..offset + 3], b"NEW");
    }

    #[test]
    fn handles_missing_optional_tags() {
        let mbi = tags_with_framebuffer(16, 0);
        let tags = unsafe { Tags::new(mbi.as_ptr() as usize) };
        let mut regions = [empty_memory_region(); 4];

        assert_eq!(tags.acpi_rsdp_addr(), None);
        assert_eq!(tags.load_base_addr(), None);
        assert_eq!(tags.write_memory_map(&mut regions), 0);
    }

    #[test]
    fn finds_one_early_system_module_and_rejects_duplicates() {
        let mut mbi = mbi_header();
        push_module_tag(&mut mbi, 0x6000, 0x7000, EARLY_SYSTEM_MODULE_CMDLINE);
        finish_mbi(&mut mbi);
        let tags = unsafe { Tags::new(mbi.as_ptr() as usize) };
        assert_eq!(
            tags.early_system_image(),
            Some(EarlySystemImage {
                ptr: 0x6000,
                len: 0x1000
            })
        );

        let mut duplicate = mbi_header();
        push_module_tag(&mut duplicate, 0x6000, 0x7000, EARLY_SYSTEM_MODULE_CMDLINE);
        push_module_tag(&mut duplicate, 0x8000, 0x9000, EARLY_SYSTEM_MODULE_CMDLINE);
        finish_mbi(&mut duplicate);
        let tags = unsafe { Tags::new(duplicate.as_ptr() as usize) };
        assert_eq!(tags.early_system_image(), None);
    }

    fn tags_with_framebuffer(red_position: u8, blue_position: u8) -> Vec<u8> {
        let mut mbi = mbi_header();
        push_framebuffer_tag(&mut mbi, red_position, blue_position);
        finish_mbi(&mut mbi);
        mbi
    }

    fn mbi_header() -> Vec<u8> {
        let mut mbi = Vec::new();
        push_u32(&mut mbi, 0);
        push_u32(&mut mbi, 0);
        mbi
    }

    fn finish_mbi(mbi: &mut Vec<u8>) {
        align_mbi(mbi);
        push_u32(mbi, TAG_END);
        push_u32(mbi, 8);
        let size = mbi.len() as u32;
        mbi[0..4].copy_from_slice(&size.to_le_bytes());
    }

    fn push_framebuffer_tag(mbi: &mut Vec<u8>, red_position: u8, blue_position: u8) {
        align_mbi(mbi);
        push_u32(mbi, TAG_FRAMEBUFFER);
        push_u32(mbi, size_of::<RawFramebufferTag>() as u32);
        push_u64(mbi, 0x8000_0000);
        push_u32(mbi, 800 * 4);
        push_u32(mbi, 800);
        push_u32(mbi, 600);
        mbi.push(32);
        mbi.push(1);
        mbi.extend_from_slice(&0u16.to_le_bytes());
        mbi.push(red_position);
        mbi.push(8);
        mbi.push(8);
        mbi.push(8);
        mbi.push(blue_position);
        mbi.push(8);
    }

    fn push_acpi_tag(mbi: &mut Vec<u8>, ty: u32, bytes: &[u8]) {
        align_mbi(mbi);
        push_u32(mbi, ty);
        push_u32(mbi, (8 + bytes.len()) as u32);
        mbi.extend_from_slice(bytes);
    }

    fn push_module_tag(mbi: &mut Vec<u8>, start: u32, end: u32, cmdline: &[u8]) {
        align_mbi(mbi);
        push_u32(mbi, TAG_MODULE);
        push_u32(mbi, (size_of::<RawModuleTag>() + cmdline.len() + 1) as u32);
        push_u32(mbi, start);
        push_u32(mbi, end);
        mbi.extend_from_slice(cmdline);
        mbi.push(0);
    }

    fn align_mbi(mbi: &mut Vec<u8>) {
        while !mbi.len().is_multiple_of(8) {
            mbi.push(0);
        }
    }

    fn push_u32(mbi: &mut Vec<u8>, value: u32) {
        mbi.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u64(mbi: &mut Vec<u8>, value: u64) {
        mbi.extend_from_slice(&value.to_le_bytes());
    }
}
