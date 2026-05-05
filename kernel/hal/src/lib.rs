#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub(crate) use kernel_lowlevel as lowlevel;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;

    #[cfg(all(rustos_debug_print_enabled, rustos_log_boot_info))]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::info!(boot, $($arg)*);
        }};
    }

    #[cfg(not(all(rustos_debug_print_enabled, rustos_log_boot_info)))]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{}};
    }

    pub(crate) use println;
}

mod arch;
mod hooks;
mod interrupt_stubs;

pub mod api;
