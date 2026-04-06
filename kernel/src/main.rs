#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

mod arch;
mod debug;
mod driver;
mod generated_registry;
mod input;
mod io;
mod memory;
mod multitask;
#[path = "../../settings.rs"]
mod settings;
mod storage;
mod system;
#[cfg(test)]
mod test_support;
mod usb;
mod user;
mod util;
mod vfs;

extern crate alloc;

use core::cell::UnsafeCell;

#[cfg(not(test))]
use crate::arch::{acpi, asmtools, gdt, idt, pic, rtc, simd};
#[cfg(not(test))]
use crate::io::{console, gui, tty};
#[cfg(not(test))]
use crate::memory::{heap, paging, phys};
#[cfg(not(test))]
use crate::storage::boot_volume;
#[cfg(not(test))]
use crate::user::{demo, syscall};
#[cfg(not(test))]
use crate::util::random;
#[cfg(not(test))]
use boot_protocol::BootInfo;
#[cfg(not(test))]
use boot_protocol::BootVolumeTransport;
#[cfg(not(test))]
use driver_abi::DriverClass;

#[cfg(not(test))]
const BOOTSTRAP_STACK_SIZE: usize = 2 * 1024 * 1024;

#[cfg(not(test))]
#[repr(align(16))]
struct BootstrapStack {
    bytes: [u8; BOOTSTRAP_STACK_SIZE],
}

#[cfg(not(test))]
struct BootstrapStackMemory(UnsafeCell<BootstrapStack>);

#[cfg(not(test))]
unsafe impl Sync for BootstrapStackMemory {}

#[cfg(not(test))]
static BOOTSTRAP_STACK: BootstrapStackMemory =
    BootstrapStackMemory(UnsafeCell::new(BootstrapStack {
        bytes: [0; BOOTSTRAP_STACK_SIZE],
    }));

#[cfg(not(test))]
fn announce_ready(_name: &str, console_line: &[u8]) {
    debug::println!("{_name} initialized.");
    console::write(console_line);
}

#[cfg(not(test))]
fn init(boot_info_ptr: *const BootInfo) {
    let _boot_info = unsafe { &*boot_info_ptr };
    gui::init(boot_info_ptr);
    debug::println!(
        "RUST OS loaded in higher half: rip={:#x}",
        asmtools::current_rip()
    );
    debug::println!("GDT loaded.");
    debug::println!("IDT loaded.");
    debug::println!("Paging initialized.");
    phys::init(boot_info_ptr);
    debug::println!(
        "Physical memory initialized: usable={} KiB free={} KiB",
        phys::usable_bytes() / 1024,
        phys::free_bytes() / 1024
    );
    heap::init_heap();
    debug::println!("Heap initialized.");
    simd::init();
    debug::println!("SIMD initialized ({}).", simd::mode_name());

    boot_volume::init_boot_info(boot_info_ptr);
    let transport_hint =
        boot_volume::boot_volume_transport_hint().unwrap_or(BootVolumeTransport::Unknown);
    match boot_volume::boot_volume_identity() {
        Some(_identity) => debug::println!(
            "boot volume identity: transport={:?} serial={:#010x} start_lba={} sectors={}",
            _identity.transport(),
            _identity.fat_volume_id,
            _identity.volume_start_lba,
            _identity.volume_sector_count
        ),
        None if transport_hint != BootVolumeTransport::Unknown => debug::println!(
            "boot volume identity: unavailable, transport hint={:?}",
            transport_hint
        ),
        None => debug::println!("boot volume identity: unavailable"),
    }
    crate::storage::block::register_boot_volume_opener();
    crate::storage::block::init();
    for _descriptor in crate::storage::block::descriptors() {
        debug::println!(
            "storage descriptor: id={} path={} transport={:?} readonly={} block_size={} start_block={} blocks={}",
            _descriptor.id,
            _descriptor.path,
            _descriptor.transport,
            _descriptor.readonly,
            _descriptor.logical_block_size,
            _descriptor.start_block,
            _descriptor.block_count
        );
    }
    driver::linux::init_cpu_local_symbols();
    debug::println!(
        "Linux compat CPU-local symbols initialized: current_task_off={:#x} stack_guard_off={:#x}",
        syscall::linux_compat_current_task_offset(),
        syscall::linux_compat_stack_guard_offset()
    );

    debug::println!("init stage: vfs::init begin");
    vfs::init();
    debug::println!("init stage: vfs::init done");
    debug::println!("init stage: driver registry load begin");
    let _registered_driver_count =
        generated_registry::register_loadable_drivers().unwrap_or_else(|error| panic!("{error}"));
    debug::println!(
        "init stage: driver registry load done (registered={})",
        _registered_driver_count
    );
    announce_ready("GUI", b"GUI initialized.\r\n");
    announce_ready("Heap", b"Heap initialized.\r\n");

    acpi::init(boot_info_ptr);
    announce_ready("ACPI", b"ACPI initialized.\r\n");

    pic::init();
    announce_ready("PIC", b"PIC initialized.\r\n");

    usb::init();
    announce_ready("USB", b"USB initialized.\r\n");

    input::init();
    announce_ready("Input", b"Input initialized.\r\n");

    console::init();
    announce_ready("Console", b"Console initialized.\r\n");

    tty::init();
    rtc::init();
    announce_ready("RTC", b"RTC initialized.\r\n");
    driver::initialize_loadable_modules_for_class(DriverClass::Display);
    announce_ready(
        "Display Drivers",
        b"Display driver modules initialized.\r\n",
    );

    paging::smoke_test();

    random::init(boot_info_ptr);
    announce_ready("Random", b"Random initialized.\r\n");

    syscall::init();
    announce_ready("Syscall", b"Syscall initialized.\r\n");
}

#[cfg(not(test))]
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
    paging::init(boot_info_ptr);
    debug::boot_trace::println_fmt(format_args!("kernel: paging ready"));

    unsafe {
        asmtools::enter_higher_half(
            paging::higher_half_addr(kernel_main_high as *const () as usize as u64),
            boot_info_ptr as u64,
        );
    }
}

#[cfg(not(test))]
extern "C" fn kernel_main_high(boot_info_ptr: *const BootInfo) -> ! {
    let bootstrap_stack_top = {
        let base = BOOTSTRAP_STACK.0.get() as *const BootstrapStack as u64;
        paging::higher_half_addr(base) + BOOTSTRAP_STACK_SIZE as u64
    };
    unsafe {
        asmtools::call_with_stack(
            paging::higher_half_addr(kernel_main_bootstrap as *const () as usize as u64),
            boot_info_ptr as u64,
            bootstrap_stack_top,
        );
    }
}

#[cfg(not(test))]
extern "C" fn kernel_main_bootstrap(boot_info_ptr: *const BootInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    debug::boot_trace::println_fmt(format_args!("kernel: higher half entry"));
    init(boot_info_ptr);
    system::bootstrap_desktop_runtime(demo::bootstrap);
    multitask::init();
    announce_ready("Multitask", b"Multitask initialized.\r\n");
    system::run_service_loop()
}
