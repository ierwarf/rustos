#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub use kernel_lowlevel as lowlevel;

#[allow(unused_imports, unused_macros)]
pub mod debug {
    pub use kernel_base::debug::*;

    #[cfg(rustos_debug_print_enabled)]
    macro_rules! println {
        () => {{
            kernel_base::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            kernel_base::debug::println_fmt(format_args!($($arg)*));
        }};
    }

    #[cfg(not(rustos_debug_print_enabled))]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{}};
    }

    pub(crate) use println;
}

pub use kernel_base::arch;
pub use kernel_base::driver;
pub use kernel_base::input;
pub use kernel_base::io;
pub use kernel_base::memory;
pub use kernel_base::multitask;
pub use kernel_base::usb;
pub use kernel_base::user;

pub mod api;
