#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

#[cfg(not(test))]
use core::alloc::Layout;
#[cfg(not(test))]
use core::cell::UnsafeCell;

#[cfg(not(test))]
use boot_protocol::BootInfo;
#[cfg(not(test))]
use kernel_base::debug;
#[cfg(not(test))]
use kernel_executive::boot;
#[cfg(not(test))]
use kernel_hal::api as hal_api;
#[cfg(not(test))]
use kernel_mm::api as mm_api;

#[cfg(not(test))]
#[global_allocator]
static KERNEL_ALLOCATOR: mm_api::KernelAllocator = mm_api::KernelAllocator;

#[cfg(not(test))]
const BOOTSTRAP_STACK_SIZE: usize = 2 * 1024 * 1024;

#[cfg(not(test))]
#[repr(align(16))]
struct BootstrapStack {
    #[allow(dead_code)]
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

#[cfg(test)]
fn main() {}

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    mm_api::handle_alloc_error(layout)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    kernel_base::debug::panic::handle_kernel_panic(info)
}

#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info_ptr: *const BootInfo) -> ! {
    // Do not inherit interrupt state from firmware/bootloader.
    // The kernel enables interrupts only after the scheduler and handlers are ready.
    hal_api::disable_interrupts();

    debug::boot_trace::init(boot_info_ptr);
    debug::boot_trace::println_fmt(format_args!("kernel: _start"));
    hal_api::init_gdt();
    debug::boot_trace::println_fmt(format_args!("kernel: GDT ready"));
    hal_api::init_idt();
    debug::boot_trace::println_fmt(format_args!("kernel: IDT ready"));
    mm_api::init_paging(boot_info_ptr);
    debug::boot_trace::println_fmt(format_args!("kernel: paging ready"));

    unsafe {
        hal_api::enter_higher_half(
            mm_api::higher_half_addr(kernel_main_high as *const () as usize as u64),
            boot_info_ptr as u64,
        );
    }
}

#[cfg(not(test))]
extern "C" fn kernel_main_high(boot_info_ptr: *const BootInfo) -> ! {
    let bootstrap_stack_top = {
        let base = BOOTSTRAP_STACK.0.get() as *const BootstrapStack as u64;
        mm_api::higher_half_addr(base) + BOOTSTRAP_STACK_SIZE as u64
    };
    unsafe {
        hal_api::call_with_stack(
            mm_api::higher_half_addr(kernel_main_bootstrap as *const () as usize as u64),
            boot_info_ptr as u64,
            bootstrap_stack_top,
        );
    }
}

#[cfg(not(test))]
extern "C" fn kernel_main_bootstrap(boot_info_ptr: *const BootInfo) -> ! {
    boot::kernel_main_bootstrap(boot_info_ptr)
}
