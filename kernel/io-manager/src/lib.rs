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

pub(crate) mod network {
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum LinuxNetdevTransport {
        Unknown,
        Pci,
    }

    static CURRENT_TRANSPORT: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn note_virtio_net_driver_registered() {}

    pub(crate) fn current_linux_netdev_transport() -> LinuxNetdevTransport {
        match CURRENT_TRANSPORT.load(Ordering::Acquire) {
            1 => LinuxNetdevTransport::Pci,
            _ => LinuxNetdevTransport::Unknown,
        }
    }

    pub(crate) fn set_current_linux_netdev_transport(transport: LinuxNetdevTransport) {
        let value = match transport {
            LinuxNetdevTransport::Unknown => 0,
            LinuxNetdevTransport::Pci => 1,
        };
        CURRENT_TRANSPORT.store(value, Ordering::Release);
    }

    pub(crate) fn register_linux_netdev(_dev: usize, _transport: LinuxNetdevTransport) -> i32 {
        0
    }

    pub(crate) fn unregister_linux_netdev(_dev: usize) {}

    pub(crate) fn allocate_linux_netdev(_dev: usize, _sizeof_priv: usize, _txqs: u32, _rxqs: u32) {}

    pub(crate) fn free_linux_netdev(_dev: usize) {}

    pub(crate) fn set_linux_netdev_carrier(_dev: usize, _carrier: bool) {}
}

#[path = "io/mod.rs"]
pub mod io;
#[path = "storage/mod.rs"]
pub mod storage;
pub(crate) mod sync;
#[path = "usb/mod.rs"]
pub mod usb;
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn exclusive_test() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().expect("test lock poisoned")
    }
}
