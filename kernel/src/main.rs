//! BSP kernel entry, one-way higher-half bootstrap, and SMP handoff.
//!
//! - **Owner:** The kernel entry owns architectural admission ordering until
//!   `kernel-executive` assumes the initialized substrate.
//! - **Boundary:** Bootloader registers, `BootInfo`, bootstrap memory, and
//!   firmware interrupt state are untrusted until their owning subsystem admits
//!   them.
//! - **Lifecycle:** Enter with interrupts disabled, establish descriptor and
//!   memory state, cross once to the higher half, initialize owners in dependency
//!   order, start scheduling, and never return.
//! - **Concurrency:** Early bootstrap is BSP-only and interrupts remain
//!   disabled until the scheduler, handlers, and admitted AP-private state are
//!   ready; later execution follows the SMP contract.
//! - **Failure:** Invalid boot metadata, allocator failure, or subsystem
//!   initialization failure terminates through the bounded panic path.
//! - **Forbidden:** No policy decision, AP startup, inherited interrupt enable,
//!   mutable success cache, or continuation after partial initialization.
//! - **Evidence:** `kernel-memory-protection`, `physical-frame-lifecycle`,
//!   `entropy-boundary`, and `service-bootstrap`.

#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]
#![cfg_attr(all(not(test), rustos_boot_image), no_std)]
#![cfg_attr(all(not(test), rustos_boot_image), no_main)]

#[cfg(all(not(test), rustos_boot_image))]
use boot_protocol::BootInfo;
#[cfg(all(not(test), rustos_boot_image))]
use core::alloc::Layout;
#[cfg(all(not(test), rustos_boot_image))]
use core::arch::global_asm;
#[cfg(all(not(test), rustos_boot_image))]
use kernel_executive::boot;
#[cfg(all(not(test), rustos_boot_image))]
use kernel_hal::api as hal_api;
#[cfg(all(not(test), rustos_boot_image))]
use kernel_mm::api as mm_api;
#[cfg(all(not(test), rustos_boot_image))]
use nucleus_core::debug;

#[cfg(all(not(test), rustos_boot_image))]
#[global_allocator]
static KERNEL_ALLOCATOR: mm_api::KernelAllocator = mm_api::KernelAllocator;

#[cfg(all(not(test), rustos_boot_image))]
global_asm!(
    include_str!("../nucleus-core/src/multiboot2_entry.S"),
    options(att_syntax)
);

#[cfg(any(test, not(rustos_boot_image)))]
fn main() {}

#[cfg(all(not(test), rustos_boot_image))]
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    mm_api::handle_alloc_error(layout)
}

#[cfg(all(not(test), rustos_boot_image))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    boot::handle_kernel_panic(info)
}

#[cfg(all(not(test), rustos_boot_image))]
#[unsafe(no_mangle)]
pub extern "C" fn multiboot2_entry64(magic: u32, mbi_addr: u32) -> ! {
    let boot_info_ptr = nucleus_core::multiboot2::build_boot_info(magic, mbi_addr);
    kernel_start_from_boot_info(boot_info_ptr)
}

#[cfg(all(not(test), rustos_boot_image))]
fn kernel_start_from_boot_info(boot_info_ptr: *const BootInfo) -> ! {
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

#[cfg(all(not(test), rustos_boot_image))]
extern "C" fn kernel_main_high(boot_info_ptr: *const BootInfo) -> ! {
    let bootstrap_stack_top = nucleus_core::bootstrap_stack::top();
    unsafe {
        hal_api::call_with_stack(
            mm_api::higher_half_addr(kernel_main_bootstrap as *const () as usize as u64),
            boot_info_ptr as u64,
            bootstrap_stack_top,
        );
    }
}

#[cfg(all(not(test), rustos_boot_image))]
extern "C" fn kernel_main_bootstrap(boot_info_ptr: *const BootInfo) -> ! {
    // SAFETY: `rustos_entry` passes the unchanged boot-protocol pointer after
    // switching to the aligned bootstrap stack.
    unsafe { boot::kernel_main_bootstrap(boot_info_ptr) }
}
