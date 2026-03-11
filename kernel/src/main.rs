#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

mod asmtools;
mod debug;
mod fat;
mod gdt;
mod gui;
mod heap;
mod idt;
mod multitask;
mod paging;
mod pic;
mod pit;
mod random;
mod rtc;

extern crate alloc;

use embedded_graphics::pixelcolor::Rgb888;
use random::Random;
use x86_64::instructions::interrupts;

use crate::multitask::Thread;
use boot_protocol::BootInfo;

const RECT_SIZE: u32 = 300;
const RECT_DELAY_MS: u64 = 4;

fn init(boot_info_ptr: *const BootInfo) {
    debug::println!("RUST OS loaded.");

    gdt::init();
    debug::println!("GDT loaded.");

    idt::init();
    debug::println!("IDT loaded.");

    paging::init();
    debug::println!("Paging initialized.");

    gui::init(boot_info_ptr);
    debug::println!("GUI Initialized.");

    pic::init();
    debug::println!("PIC initialized.");

    rtc::init();
    debug::println!("RTC initialized.");

    heap::init_heap();
    debug::println!("Heap initialized.");

    random::init(boot_info_ptr);
    debug::println!("Random initialized.");

    multitask::init(0.1);
    interrupts::enable();
    debug::println!("Multitask initialized.");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
    init(boot_info_ptr);

    debug::println!("asdf");

    let th1 = Thread::new(gui_update, 10);
    th1.start();

    gui::GOP_SCREEN.lock().fill(Rgb888::new(0, 0, 0));

    loop {
        core::hint::spin_loop();
    }
}

fn gui_update(_id: u16) {
    let fps = 10;
    loop {
        rtc::sleep(1000 / fps);
        gui::GOP_SCREEN.lock().refresh();
    }
}
