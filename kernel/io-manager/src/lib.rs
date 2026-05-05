#![no_std]

extern crate alloc;

pub(crate) use kernel_hal::api::arch;
pub(crate) use kernel_ipc_runtime::api as ipc;
pub(crate) use kernel_mm::api as memory;
pub(crate) use kernel_ps::api as multitask;
pub(crate) use kernel_ps::api as user;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;

    #[cfg(all(rustos_debug_print_enabled, rustos_log_debug_info))]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::info!(debug, $($arg)*);
        }};
    }

    #[cfg(not(all(rustos_debug_print_enabled, rustos_log_debug_info)))]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{}};
    }

    pub(crate) use println;
}

pub mod api;

#[path = "driver/mod.rs"]
pub mod driver;
#[path = "input/mod.rs"]
pub mod input;
#[path = "input_core.rs"]
pub mod input_core;
#[path = "io/mod.rs"]
pub mod io;
#[path = "network/mod.rs"]
pub mod network;
#[path = "storage/mod.rs"]
pub mod storage;
#[path = "usb/mod.rs"]
pub mod usb;
#[path = "vfs/mod.rs"]
pub mod vfs;
#[path = "vfs_core.rs"]
pub mod vfs_core;
