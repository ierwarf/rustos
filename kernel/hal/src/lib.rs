#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub(crate) use kernel_lowlevel as lowlevel;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;

    #[cfg(rustos_debug_print_enabled)]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::println_fmt(format_args!($($arg)*));
        }};
    }

    #[cfg(not(rustos_debug_print_enabled))]
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
