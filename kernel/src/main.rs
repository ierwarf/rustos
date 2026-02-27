#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

mod asmtools;
mod debug;
mod gdt;
mod gui;
mod heap;
mod idt;
mod multitask;
mod paging;
mod pic;
mod pit;
mod rtc;

extern crate alloc;

use embedded_graphics::pixelcolor::Rgb888;
use x86_64::instructions::interrupts;

use crate::multitask::Thread;

const RECT_SIZE: u32 = 300;
const RECT_DELAY_MS: u64 = 4;

fn init(boot_info_ptr: *const gui::BootInfo) {
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

    multitask::init(1.0);
    interrupts::enable();
    debug::println!("Multitask initialized.");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const gui::BootInfo) -> ! {
    init(boot_info_ptr);

    let threads = [
        Thread::new(gui_update, 90),
        Thread::new(gui2, 44),
        Thread::new(gui3, 55),
    ];
    for thread in &threads {
        thread.start();
    }

    loop {
        core::hint::spin_loop();
    }
}

fn gui2(_id: u16) {
    animate_rect(0, 0, |value| Rgb888::new(value, 0, 0));
}

fn gui3(_id: u16) {
    animate_rect(300, 300, |value| Rgb888::new(0, value, 0));
}

fn animate_rect(x: i64, y: i64, color: fn(u8) -> Rgb888) {
    loop {
        use gui::GOP_SCREEN;

        for value in (0..=255).chain((0..=255).rev()) {
            GOP_SCREEN
                .lock()
                .fill_rect(x, y, RECT_SIZE, RECT_SIZE, color(value), 255);
            rtc::sleep(RECT_DELAY_MS);
        }
    }
}

fn gui_update(_id: u16) {
    let fps = 10;
    loop {
        rtc::sleep(1000 / fps);
        gui::GOP_SCREEN.lock().refresh();
    }
}
