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

pub(crate) use arch::{asmtools, gdt, idt, pic, pit, rtc};
pub(crate) use input::keyboard;
pub(crate) use io::{console, gui, tty};
pub(crate) use memory::{heap, paging};
pub(crate) use storage::fat;
pub(crate) use user::{demo, process, syscall, win32};
pub(crate) use util::{random, ring};

extern crate alloc;

use boot_protocol::BootInfo;

fn init(boot_info_ptr: *const BootInfo) {
    debug::println!(
        "RUST OS loaded in higher half: rip={:#x}",
        asmtools::current_rip()
    );
    debug::println!("GDT loaded.");
    debug::println!("IDT loaded.");
    debug::println!("Paging initialized.");

    gui::init(boot_info_ptr);
    debug::println!("GUI Initialized.");

    pic::init();
    debug::println!("PIC initialized.");

    keyboard::init();
    debug::println!("Keyboard initialized.");

    console::init();
    debug::println!("Console initialized.");

    tty::init();
    debug::println!("TTY initialized.");

    rtc::init();
    debug::println!("RTC initialized.");

    heap::init_heap();
    debug::println!("Heap initialized.");

    paging::smoke_test();

    random::init(boot_info_ptr);
    debug::println!("Random initialized.");

    multitask::init();
    debug::println!("Multitask initialized.");

    syscall::init();
    debug::println!("Syscall initialized.");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
    gdt::init();
    idt::init();
    paging::init();

    unsafe {
        asmtools::enter_higher_half(
            paging::higher_half_addr(kernel_main_high as *const () as usize as u64),
            boot_info_ptr as u64,
        );
    }
}

extern "C" fn kernel_main_high(boot_info_ptr: *const BootInfo) -> ! {
    init(boot_info_ptr);
    demo::run()
}
