#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

extern crate alloc;

mod load;
mod runtime;
mod settings;

pub(crate) use load::elf_loader;
pub(crate) use runtime::{debug, heap, panic_screen};

use boot_random as random;
use boot_storage as fat;

use boot_protocol::BootInfo;
use core::fmt;
use core::panic::PanicInfo;
use fatfs::{Seek, SeekFrom};
use x86_64::instructions::{hlt, interrupts};

const KERNEL_PATH: &str = "kernel.elf";

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

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
    interrupts::disable();
    heap::init_heap();

    debug::println!("prekernel: start");
    if boot_info_ptr.is_null() {
        fatal(format_args!("boot info pointer is null"));
    }
    panic_screen::init(boot_info_ptr);
    fat::init_boot_info(boot_info_ptr);
    random::init(boot_info_ptr);
    panic_screen::println_fmt(format_args!("prekernel: start"));

    let volume = match fat::BootVolume::open() {
        Ok(volume) => volume,
        Err(err) => fatal(format_args!("failed to open boot volume: {:?}", err)),
    };
    let mut kernel_file = match volume.open_file(KERNEL_PATH) {
        Ok(file) => file,
        Err(err) => fatal(format_args!("failed to open {}: {:?}", KERNEL_PATH, err)),
    };

    let kernel_size = match kernel_file.seek(SeekFrom::End(0)) {
        Ok(size) => size,
        Err(err) => fatal(format_args!("failed to stat {}: {:?}", KERNEL_PATH, err)),
    };
    if let Err(err) = kernel_file.seek(SeekFrom::Start(0)) {
        fatal(format_args!("failed to rewind {}: {:?}", KERNEL_PATH, err));
    }
    debug::println!("prekernel: kernel image found, {} bytes", kernel_size);
    panic_screen::println_fmt(format_args!(
        "prekernel: kernel image found, {} bytes",
        kernel_size
    ));

    #[cfg(rustos_kernel_physical_kaslr_enabled)]
    let kernel_physical_slide =
        random::Random::new().randint(0, settings::MAX_KERNEL_PHYSICAL_KASLR_SLIDE + 1) as usize;
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
        match elf_loader::load_kernel_elf(&mut kernel_file, kernel_size, kernel_physical_slide) {
            Ok(loaded) => loaded,
            Err(reason) => fatal(format_args!("failed to load {}: {}", KERNEL_PATH, reason)),
        };
    drop(kernel_file);
    if let Err(err) = volume.close() {
        fatal(format_args!("failed to close boot volume: {:?}", err));
    }

    let applied_slide = load_bias.saturating_sub(0x0020_0000);
    debug::println!(
        "prekernel: kernel ELF loaded, entry={:#x}, segments={}, load_bias={:#x}, applied_slide={:#x}",
        entry_point,
        segment_count,
        load_bias,
        applied_slide
    );
    debug::println!("prekernel: jumping to kernel");
    panic_screen::println_fmt(format_args!(
        "prekernel: kernel ELF loaded, entry={:#x}, segments={}, load_bias={:#x}, applied_slide={:#x}",
        entry_point, segment_count, load_bias, applied_slide
    ));
    panic_screen::println_fmt(format_args!("prekernel: jumping to kernel"));

    unsafe {
        let kernel_entry: extern "sysv64" fn(*const BootInfo) -> ! =
            core::mem::transmute(entry_point);
        kernel_entry(boot_info_ptr);
    }
}

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
