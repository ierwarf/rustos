#![feature(abi_x86_interrupt)]
#![no_std]
#![no_main]

mod debug;
mod descriptor;

use core::arch::asm;
use x86_64::instructions::port::Port;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    debug::print("RUST OS sucess.");

    descriptor::idt::init();
    debug::print("IDT loaded");

    x86_64::instructions::interrupts::int3();

    loop {
        core::hint::spin_loop();
    }
}
