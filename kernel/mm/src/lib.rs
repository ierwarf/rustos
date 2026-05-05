#![no_std]

extern crate alloc;

pub(crate) use kernel_lowlevel as lowlevel;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;

    #[cfg(all(rustos_debug_print_enabled, rustos_log_memory_info))]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::info!(memory, $($arg)*);
        }};
    }

    #[cfg(not(all(rustos_debug_print_enabled, rustos_log_memory_info)))]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{}};
    }

    pub(crate) use println;
}

pub use nucleus_core::settings;

#[path = "memory/mod.rs"]
pub mod memory;

pub mod api;
