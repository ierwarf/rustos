#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

mod debug;
mod gdt;
mod gui;
mod heap;
mod idt;
mod multitask;
mod pic;
mod pit;
mod rtc;

extern crate alloc;

use alloc::boxed::Box;
use multitask::Thread;
use x86_64::instructions::interrupts;

fn work1(_id: u16) {
    loop {
        debug::println!("0");
        rtc::sleep(1000);
    }
}

fn work2(_id: u16) {
    loop {
        debug::println!("1");
        rtc::sleep(1000);
    }
}

fn work3(_id: u16) {
    loop {
        debug::println!("2");
        rtc::sleep(1000);
    }
}

fn init(boot_info_ptr: *const gui::BootInfo) {
    debug::println!("RUST OS loaded.");

    let gui_ready = if let Err(reason) = gui::init(boot_info_ptr) {
        debug::println!("GUI init failed: {}", reason);
        false
    } else {
        debug::println!("GUI metadata loaded.");
        true
    };

    gdt::init();
    debug::println!("GDT loaded.");

    idt::init();
    debug::println!("IDT loaded.");

    pic::init();
    debug::println!("PIC initialized.");
    rtc::init();
    debug::println!("RTC initialized.");

    heap::init_heap();
    debug::println!("Heap initialized.");

    if gui_ready {
        if let Err(reason) = gui::enable_double_buffer() {
            debug::println!("GUI double buffer failed: {}", reason);
        }
        gui::render_boot_screen();
        if gui::is_double_buffer_enabled() {
            debug::println!("GUI initialized (double buffer).");
        } else {
            debug::println!("GUI initialized (single buffer).");
        }
    }

    let x = Box::new("Complete");
    debug::println!("Heap alloc test: {}", *x);

    multitask::init(0.1);
    interrupts::enable();
    debug::println!("Multitask initialized.");

    let th1 = Thread::new(work1, 1);
    th1.start();
    let th2 = Thread::new(work2, 2);
    th2.start();
    let th3 = Thread::new(work3, 3);
    th3.start();
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const gui::BootInfo) -> ! {
    init(boot_info_ptr);

    loop {
        core::hint::spin_loop();
    }
}
