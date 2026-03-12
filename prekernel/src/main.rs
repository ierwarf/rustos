#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

extern crate alloc;

mod load;
mod runtime;

pub(crate) use load::elf_loader;
pub(crate) use runtime::{debug, heap};

#[path = "../../kernel/src/storage/fat.rs"]
mod fat;

use boot_protocol::BootInfo;
use core::fmt;
use core::panic::PanicInfo;
use fatfs::{Seek, SeekFrom};
use x86_64::instructions::{hlt, interrupts};

const KERNEL_PATH: &str = "kernel.elf";
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    debug::println!();
    debug::println!("[PREKERNEL PANIC]");
    debug::println!("message: {}", info.message());
    if let Some(location) = info.location() {
        debug::println!(
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
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

    let (entry_point, segment_count) = match elf_loader::load_kernel_elf(&mut kernel_file, kernel_size) {
        Ok(loaded) => loaded,
        Err(reason) => fatal(format_args!("failed to load {}: {}", KERNEL_PATH, reason)),
    };
    drop(kernel_file);
    if let Err(err) = volume.close() {
        fatal(format_args!("failed to close boot volume: {:?}", err));
    }

    debug::println!(
        "prekernel: kernel ELF loaded, entry={:#x}, segments={}",
        entry_point,
        segment_count
    );
    debug::println!("prekernel: jumping to kernel");

    unsafe {
        let kernel_entry: extern "sysv64" fn(*const BootInfo) -> ! = core::mem::transmute(entry_point);
        kernel_entry(boot_info_ptr);
    }
}

fn fatal(args: fmt::Arguments<'_>) -> ! {
    debug::println!();
    debug::println!("[PREKERNEL FATAL]");
    debug::println!("{}", args);
    loop {
        hlt();
    }
}
