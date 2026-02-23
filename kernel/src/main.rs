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
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::set_general_handler;
use x86_64::structures::idt::InterruptStackFrame;

static TICK: AtomicU64 = AtomicU64::new(0);

fn init() {
    debug::println!("RUST OS loaded.");

    gdt::init();
    debug::println!("GDT loaded.");

    idt::init();
    debug::println!("IDT loaded.");

    pic::init();
    debug::println!("PIC initialized.");

    pic::enable_irq(0);
    pit::start(0, 1);
}

fn ticktick(_stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    let tick = TICK.fetch_add(1, Ordering::Relaxed) + 1;

    debug::println!("Tick: {}", tick);

    unsafe {
        use x86_64::instructions::port::Port;

        let mut pic_cmd = Port::<u8>::new(0x20);
        pic_cmd.write(0x20); // EOI
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    init();

    loop {
        core::hint::spin_loop();
    }
}
