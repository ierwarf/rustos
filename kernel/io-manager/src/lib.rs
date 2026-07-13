#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub(crate) use kernel_hal::api::arch;
pub(crate) use kernel_ipc_runtime::api as ipc;
pub(crate) use kernel_mm::api as memory;
pub(crate) use kernel_ps::api as multitask;
pub(crate) use kernel_ps::api as user;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;

    #[cfg(all(rustos_debug_print_enabled, rustos_log_debug_info, not(test)))]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::info!(debug, $($arg)*);
        }};
    }

    #[cfg(test)]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{
            let _ = core::format_args!($($arg)*);
        }};
    }

    #[cfg(all(not(test), not(all(rustos_debug_print_enabled, rustos_log_debug_info))))]
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

#[path = "network/mod.rs"]
pub(crate) mod network;

#[path = "io/mod.rs"]
pub mod io;
#[path = "storage/mod.rs"]
pub mod storage;
pub(crate) mod sync;
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn exclusive_test() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().expect("test lock poisoned")
    }
}
