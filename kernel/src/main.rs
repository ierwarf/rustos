#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![no_std]
#![no_main]

mod asmbly;
mod debug;
mod gdt;
mod handlers;
mod heap;
mod idt;
mod multitask;
mod pic;
mod pit;

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::AtomicU64;
use multitask::Thread;

static TICK: AtomicU64 = AtomicU64::new(0);

fn work1(id: u16) {
    loop {
        debug::println!("0");
    }
}

fn work2(id: u16) {
    loop {
        debug::println!("1");
    }
}

fn init() {
    debug::println!("RUST OS loaded.");

    gdt::init();
    debug::println!("GDT loaded.");

    idt::init();
    debug::println!("IDT loaded.");

    pic::init();
    debug::println!("PIC initialized.");

    heap::init_heap();
    debug::println!("Heap initialized.");

    let x = Box::new("Complete");
    debug::println!("Heap alloc test: {}", *x);

    multitask::init();
    debug::println!("Multitask initialized.");

    let th1 = Thread::new(work1, 1);
    th1.start();
    let th2 = Thread::new(work2, 2);
    th2.start();

    multitask::start_scheduler(1);

    debug::println!("aaa");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    init();

    loop {
        core::hint::spin_loop();
    }
}
