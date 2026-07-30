#![no_std]

extern crate alloc;

pub(crate) use kernel_hal::api::arch;
pub(crate) use kernel_ipc_runtime::api as ipc;
pub(crate) use kernel_mm::api as memory;
pub(crate) use kernel_ps::api as multitask;

#[allow(unused_imports, unused_macros)]
pub(crate) mod debug {
    pub(crate) use nucleus_core::debug::*;
    pub(crate) use trace_loc_macro::trace_loc;

    #[cfg(all(rustos_debug_print_enabled, rustos_log_compat_info))]
    macro_rules! println {
        () => {{
            nucleus_core::debug::println_newline();
        }};
        ($($arg:tt)*) => {{
            nucleus_core::debug::info!(compat, $($arg)*);
        }};
    }

    #[cfg(not(all(rustos_debug_print_enabled, rustos_log_compat_info)))]
    macro_rules! println {
        () => {{}};
        ($($arg:tt)*) => {{}};
    }

    pub(crate) use println;
}

pub(crate) mod io {
    pub(crate) mod device {
        pub(crate) use kernel_io_manager::api::device::DeviceError;
        pub(crate) use kernel_object::api::device::{DeviceHandle, DeviceId};

        pub fn ioctl_from_user(
            handle: DeviceHandle,
            process_id: u64,
            process_state: &mut kernel_ps::api::process_state::UserProcessState,
            request: u64,
            arg: u64,
        ) -> Result<u64, DeviceError> {
            kernel_io_manager::api::device::ioctl_from_user(
                handle,
                process_id,
                process_state,
                request,
                arg,
            )
        }
    }

    pub(crate) mod session {
        pub(crate) use kernel_object::api::session::ConsoleSessionHandle;
    }

    pub(crate) mod tty {
        pub(crate) use kernel_object::api::session::ConsoleSessionHandle;

        pub fn write_to_session(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
            kernel_io_manager::api::tty::write_to_session(session, bytes)
        }

        pub fn read_input_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
            kernel_io_manager::api::tty::read_input_for_session(session, dest)
        }

        pub fn read_input_blocking_for_session(
            session: ConsoleSessionHandle,
            dest: &mut [u8],
        ) -> usize {
            kernel_io_manager::api::tty::read_input_blocking_for_session(session, dest)
        }
    }
}

pub(crate) mod storage {
    pub(crate) mod boot_volume {
        pub fn userspace_runtime_active() -> bool {
            kernel_io_manager::api::boot::userspace_ready()
        }
    }
}

pub(crate) mod vfs {
    pub use kernel_io_manager::api::VfsError;

    pub fn normalize_kernel_path(path: &str) -> Result<alloc::string::String, VfsError> {
        kernel_io_manager::api::vfs::normalize_kernel_path(path)
    }

    pub fn read_path_to_vec_for_kernel(
        absolute_path: &str,
    ) -> Result<alloc::vec::Vec<u8>, VfsError> {
        kernel_io_manager::api::vfs::read_path_to_vec_for_kernel(absolute_path)
    }
}

pub(crate) mod vfs_core {
    pub fn path_inode(path: &[u8]) -> u64 {
        kernel_io_manager::api::vfs::path_inode(path)
    }
}

pub mod user;

pub mod api;
