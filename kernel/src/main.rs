#![feature(abi_x86_interrupt)]
#![no_std]
#![no_main]

mod debug;
mod gdt;
mod handlers;
mod idt;
mod pic;
mod pit;

use core::arch::asm;

fn init() {
    debug::println("RUST OS loaded.");

    gdt::init();
    debug::println("GDT loaded.");

    idt::init();
    debug::println("IDT loaded.");

    pic::init();
    debug::println("PIC initialized.");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    init();

    let a = 7 / zero();

    loop {
        core::hint::spin_loop();
    }
}

fn zero() -> i32 {
    0
}
