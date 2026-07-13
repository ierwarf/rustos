#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub(crate) use kernel_hal::api::arch;
pub(crate) use kernel_ipc_runtime::api as ipc;
pub(crate) use kernel_lowlevel as lowlevel;
pub(crate) use kernel_mm::api as memory;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;
    pub(crate) use trace_loc_macro::trace_loc;

    #[cfg(all(rustos_debug_print_enabled, rustos_log_sched_info))]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::info!(sched, $($arg)*);
        }};
    }

    #[cfg(not(all(rustos_debug_print_enabled, rustos_log_sched_info)))]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{}};
    }

    pub(crate) use println;
}

pub(crate) mod io {
    pub(crate) mod device {
        pub(crate) use kernel_object::api::device::{DeviceAccessKind, DeviceHandle, DeviceId};
    }

    pub(crate) mod session {
        pub(crate) use kernel_object::api::session::ConsoleSessionHandle;
    }
}

pub mod api;
pub mod user;

#[path = "multitask/mod.rs"]
pub mod multitask;
