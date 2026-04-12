#![feature(alloc_error_handler)]
#![cfg_attr(all(not(test), rustos_boot_image), no_std)]
#![cfg_attr(all(not(test), rustos_boot_image), no_main)]

#[cfg(all(not(test), rustos_boot_image))]
extern crate alloc;

#[cfg(all(not(test), rustos_boot_image))]
mod load;
#[cfg(all(not(test), rustos_boot_image))]
mod runtime;
#[cfg(all(not(test), rustos_boot_image))]
#[path = "../../../settings.rs"]
mod settings;
#[cfg(all(not(test), rustos_boot_image))]
mod storage;

#[cfg(all(not(test), rustos_boot_image))]
pub(crate) use load::elf_loader;
#[cfg(all(not(test), rustos_boot_image))]
pub(crate) use runtime::{debug, heap, panic_screen};

#[cfg(all(not(test), rustos_boot_image))]
use boot_random as random;

#[cfg(all(not(test), rustos_boot_image))]
use boot_protocol::BootInfo;
#[cfg(all(not(test), rustos_boot_image))]
use core::fmt;
#[cfg(all(not(test), rustos_boot_image))]
use core::panic::PanicInfo;
#[cfg(all(not(test), rustos_boot_image))]
use fatfs::{Seek, SeekFrom};
#[cfg(all(not(test), rustos_boot_image))]
use x86_64::instructions::{hlt, interrupts};

#[cfg(all(not(test), rustos_boot_image))]
const NUCLEUS_PATH: &str = "nucleus.elf";

#[cfg(all(not(test), rustos_boot_image))]
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    panic_screen::reset();
    debug::println!();
    debug::println!("[PREKERNEL PANIC]");
    debug::println!("message: {}", info.message());
    panic_screen::println_fmt(format_args!(""));
    panic_screen::println_fmt(format_args!("[PREKERNEL PANIC]"));
    panic_screen::println_fmt(format_args!("message: {}", info.message()));
    if let Some(location) = info.location() {
        debug::println!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
        panic_screen::println_fmt(format_args!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ));
    } else {
        panic_screen::println_fmt(format_args!("location: <unknown>"));
    }
    loop {
        hlt();
    }
}

#[cfg(all(not(test), rustos_boot_image))]
#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
    interrupts::disable();
    heap::init_heap();

    debug::install_boot_diag(boot_info_ptr);
    let boot_info = match unsafe { BootInfo::from_ptr(boot_info_ptr) } {
        Ok(boot_info) => boot_info,
        Err(error) => fatal(format_args!("{}", error.as_str())),
    };
    debug::println!("prekernel: start");
    panic_screen::init(boot_info_ptr);
    random::init(boot_info_ptr);
    panic_screen::println_fmt(format_args!("prekernel: start"));

    let volume = match storage::open_boot_volume(boot_info.boot_volume) {
        Ok(volume) => volume,
        Err(err) => fatal(format_args!("failed to open boot volume: {}", err)),
    };
    let mut nucleus_file = match volume.open_file(NUCLEUS_PATH) {
        Ok(file) => file,
        Err(err) => fatal(format_args!("failed to open {}: {:?}", NUCLEUS_PATH, err)),
    };

    let nucleus_size = match nucleus_file.seek(SeekFrom::End(0)) {
        Ok(size) => size,
        Err(err) => fatal(format_args!("failed to stat {}: {:?}", NUCLEUS_PATH, err)),
    };
    if let Err(err) = nucleus_file.seek(SeekFrom::Start(0)) {
        fatal(format_args!("failed to rewind {}: {:?}", NUCLEUS_PATH, err));
    }
    debug::println!("prekernel: nucleus image found, {} bytes", nucleus_size);
    panic_screen::println_fmt(format_args!(
        "prekernel: nucleus image found, {} bytes",
        nucleus_size
    ));

    #[cfg(rustos_kernel_physical_kaslr_enabled)]
    let kernel_physical_slide = if boot_protocol::rng_seed_usable(boot_info.rng_seed) {
        random::Random::new().randint(0, settings::MAX_KERNEL_PHYSICAL_KASLR_SLIDE + 1) as usize
    } else {
        0
    };
    #[cfg(not(rustos_kernel_physical_kaslr_enabled))]
    let kernel_physical_slide = 0;
    debug::println!(
        "prekernel: kernel physical slide(raw)={:#x}",
        kernel_physical_slide
    );
    panic_screen::println_fmt(format_args!(
        "prekernel: kernel physical slide(raw)={:#x}",
        kernel_physical_slide
    ));

    let (entry_point, segment_count, load_bias) =
        match elf_loader::load_kernel_elf(&mut nucleus_file, nucleus_size, kernel_physical_slide) {
            Ok(loaded) => loaded,
            Err(reason) => fatal(format_args!("failed to load {}: {}", NUCLEUS_PATH, reason)),
        };
    drop(nucleus_file);
    if let Err(err) = volume.unmount() {
        fatal(format_args!("failed to close boot volume: {:?}", err));
    }

    let applied_slide = load_bias.saturating_sub(0x0020_0000);
    debug::println!(
        "prekernel: nucleus ELF loaded, entry={:#x}, segments={}, load_bias={:#x}, applied_slide={:#x}",
        entry_point,
        segment_count,
        load_bias,
        applied_slide
    );
    debug::println!("prekernel: jumping to nucleus");
    panic_screen::println_fmt(format_args!(
        "prekernel: nucleus ELF loaded, entry={:#x}, segments={}, load_bias={:#x}, applied_slide={:#x}",
        entry_point, segment_count, load_bias, applied_slide
    ));
    panic_screen::println_fmt(format_args!("prekernel: jumping to nucleus"));

    unsafe {
        let nucleus_entry: extern "sysv64" fn(*const BootInfo) -> ! =
            core::mem::transmute(entry_point);
        nucleus_entry(boot_info_ptr);
    }
}

#[cfg(any(test, not(rustos_boot_image)))]
fn main() {}

#[cfg(all(not(test), rustos_boot_image))]
fn fatal(args: fmt::Arguments<'_>) -> ! {
    panic_screen::reset();
    debug::println!();
    debug::println!("[PREKERNEL FATAL]");
    debug::println!("{}", args);
    panic_screen::println_fmt(format_args!(""));
    panic_screen::println_fmt(format_args!("[PREKERNEL FATAL]"));
    panic_screen::println_fmt(args);
    loop {
        hlt();
    }
}
