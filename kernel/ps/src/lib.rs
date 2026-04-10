#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub use kernel_lowlevel as lowlevel;

pub use kernel_base::arch;
#[allow(unused_imports, unused_macros)]
pub mod debug {
    pub use kernel_base::debug::*;
    pub(crate) use trace_loc_macro::trace_loc;

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

pub use kernel_base::driver;
pub use kernel_base::io;
pub use kernel_base::memory;
pub use kernel_base::multitask;
pub use kernel_base::user;

pub mod api;
