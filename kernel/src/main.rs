#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

mod arch;
mod debug;
mod input;
mod io;
mod memory;
mod multitask;
mod storage;
mod user;
mod util;

pub(crate) use arch::{acpi, asmtools, gdt, idt, pic, pit, rtc};
pub(crate) use input::keyboard;
pub(crate) use io::{console, gui, jpeg, tty};
pub(crate) use memory::{heap, paging};
pub(crate) use storage::fat;
pub(crate) use user::{demo, process, syscall, win32};
pub(crate) use util::{random, ring};

extern crate alloc;

use boot_protocol::BootInfo;

fn announce_ready(name: &str, console_line: &[u8]) {
    debug::println!("{name} initialized.");
    gui::write_console(console_line);
}

fn init(boot_info_ptr: *const BootInfo) {
    debug::println!(
        "RUST OS loaded in higher half: rip={:#x}",
        asmtools::current_rip()
    );
    debug::println!("GDT loaded.");
    debug::println!("IDT loaded.");
    debug::println!("Paging initialized.");

    gui::init(boot_info_ptr);
    fat::init_boot_info(boot_info_ptr);
    heap::init_heap();
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

    paging::smoke_test();

    random::init(boot_info_ptr);
    announce_ready("Random", b"Random initialized.\r\n");

    multitask::init();
    input::start_worker();
    console::start_worker();
    announce_ready("Multitask", b"Multitask initialized.\r\n");

    syscall::init();
    announce_ready("Syscall", b"Syscall initialized.\r\n");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
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
    debug::boot_trace::println_fmt(format_args!("kernel: higher half entry"));
    init(boot_info_ptr);
    demo::run()
}
