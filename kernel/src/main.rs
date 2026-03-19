#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

mod arch;
mod debug;
mod driver;
mod input;
mod io;
mod memory;
mod multitask;
mod storage;
mod system;
mod user;
mod util;
mod vfs;

pub(crate) use arch::{acpi, asmtools, gdt, idt, pic, pit, rtc, simd};
pub(crate) use driver_abi::{DriverBus, DriverClass};
pub(crate) use input::keyboard;
pub(crate) use io::{console, gui, session, tty};
pub(crate) use memory::{heap, paging};
pub(crate) use storage::fat;
pub(crate) use user::syscall::windows as win32;
pub(crate) use user::{demo, process, syscall};
pub(crate) use util::{random, ring};

extern crate alloc;

use boot_protocol::BootInfo;

const AMDGPU_DRIVER_NAME: &str = "amdgpu";
const AMDGPU_DRIVER_MODULE_PATH: &str = "system/drivers/display/amdgpu.ko";

fn announce_ready(name: &str, console_line: &[u8]) {
    debug::println!("{name} initialized.");
    console::write(console_line);
}

fn init(boot_info_ptr: *const BootInfo) {
    debug::println!(
        "RUST OS loaded in higher half: rip={:#x}",
        asmtools::current_rip()
    );
    debug::println!("GDT loaded.");
    debug::println!("IDT loaded.");
    debug::println!("Paging initialized.");
    simd::init();
    debug::println!("SIMD initialized ({}).", simd::mode_name());

    gui::init(boot_info_ptr);
    fat::init_boot_info(boot_info_ptr);
    vfs::init();
    heap::init_heap();
    driver::register_kernel_builtin("uefi-gop", DriverClass::Display, DriverBus::Platform);
    driver::register_loadable_elf(
        AMDGPU_DRIVER_NAME,
        DriverClass::Display,
        DriverBus::Pci,
        AMDGPU_DRIVER_MODULE_PATH,
    );
    announce_ready("GUI", b"GUI initialized.\r\n");
    announce_ready("Heap", b"Heap initialized.\r\n");

    acpi::init(boot_info_ptr);
    announce_ready("ACPI", b"ACPI initialized.\r\n");

    pic::init();
    announce_ready("PIC", b"PIC initialized.\r\n");

    input::init();
    announce_ready("Input", b"Input initialized.\r\n");

    console::init();
    announce_ready("Console", b"Console initialized.\r\n");

    tty::init();
    rtc::init();
    announce_ready("RTC", b"RTC initialized.\r\n");
    driver::initialize_loadable_modules_for_class(DriverClass::Display);
    announce_ready("Display Drivers", b"Display driver modules initialized.\r\n");

    paging::smoke_test();

    random::init(boot_info_ptr);
    announce_ready("Random", b"Random initialized.\r\n");

    syscall::init();
    announce_ready("Syscall", b"Syscall initialized.\r\n");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
    // Do not inherit interrupt state from firmware/bootloader.
    // The kernel enables interrupts only after the scheduler and handlers are ready.
    x86_64::instructions::interrupts::disable();

    debug::boot_trace::init(boot_info_ptr);
    debug::boot_trace::println_fmt(format_args!("kernel: _start"));
    gdt::init();
    debug::boot_trace::println_fmt(format_args!("kernel: GDT ready"));
    idt::init();
    debug::boot_trace::println_fmt(format_args!("kernel: IDT ready"));
    paging::init();
    debug::boot_trace::println_fmt(format_args!("kernel: paging ready"));

    unsafe {
        asmtools::enter_higher_half(
            paging::higher_half_addr(kernel_main_high as *const () as usize as u64),
            boot_info_ptr as u64,
        );
    }
}

extern "C" fn kernel_main_high(boot_info_ptr: *const BootInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    debug::boot_trace::println_fmt(format_args!("kernel: higher half entry"));
    init(boot_info_ptr);
    system::bootstrap_desktop_runtime(demo::bootstrap);
    multitask::init();
    announce_ready("Multitask", b"Multitask initialized.\r\n");
    system::run_service_loop()
}
