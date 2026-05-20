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
            process_state: &mut kernel_ps::api::process_state::UserProcessState,
            request: u64,
            arg: u64,
        ) -> Result<u64, DeviceError> {
            kernel_io_manager::api::device::ioctl_from_user(
                handle.into(),
                process_state,
                request,
                arg,
            )
        }

        pub mod input {
            pub fn has_pending_events() -> bool {
                kernel_io_manager::api::device::input::has_pending_events()
            }
        }
    }

    pub(crate) mod session {
        pub(crate) use kernel_object::api::session::ConsoleSessionHandle;
    }

    pub(crate) mod tty {
        pub(crate) use kernel_io_manager::api::tty::LinuxTermios;
        pub(crate) use kernel_object::api::session::ConsoleSessionHandle;

        pub fn has_pending_input_for_session(session: ConsoleSessionHandle) -> bool {
            kernel_io_manager::api::tty::has_pending_input_for_session(session.into())
        }

        pub fn pending_input_len_for_session(session: ConsoleSessionHandle) -> usize {
            kernel_io_manager::api::tty::pending_input_len_for_session(session.into())
        }

        pub fn termios_for_session(session: ConsoleSessionHandle) -> LinuxTermios {
            kernel_io_manager::api::tty::termios_for_session(session.into())
        }

        pub fn set_termios_for_session(
            session: ConsoleSessionHandle,
            termios: LinuxTermios,
            flush_input: bool,
        ) {
            kernel_io_manager::api::tty::set_termios_for_session(
                session.into(),
                termios,
                flush_input,
            );
        }

        pub fn write_to_session(session: ConsoleSessionHandle, bytes: &[u8]) -> usize {
            kernel_io_manager::api::tty::write_to_session(session.into(), bytes)
        }

        pub fn read_input_for_session(session: ConsoleSessionHandle, dest: &mut [u8]) -> usize {
            kernel_io_manager::api::tty::read_input_for_session(session.into(), dest)
        }

        pub fn read_input_blocking_for_session(
            session: ConsoleSessionHandle,
            dest: &mut [u8],
        ) -> usize {
            kernel_io_manager::api::tty::read_input_blocking_for_session(session.into(), dest)
        }
    }
}

pub(crate) mod storage {
    pub(crate) mod boot_volume {
        pub fn read_file_to_vec(
            path: &str,
        ) -> Result<alloc::vec::Vec<u8>, kernel_io_manager::api::VfsError> {
            kernel_io_manager::api::vfs::read_path_to_vec_for_kernel(path)
        }

        pub fn userspace_runtime_active() -> bool {
            kernel_io_manager::api::boot::userspace_ready()
        }
    }
}

pub(crate) mod vfs {
    pub use kernel_io_manager::api::{MountError, VfsError, VfsMetadata, VfsNodeKind};

    pub fn normalize_kernel_path(path: &str) -> Result<alloc::string::String, VfsError> {
        kernel_io_manager::api::vfs::normalize_kernel_path(path)
    }

    pub fn mount_for_current_process(
        source_path: &str,
        target_path: &str,
        filesystem_type: &str,
        flags: u64,
        options: Option<&str>,
    ) -> Result<(), MountError> {
        kernel_io_manager::api::vfs::mount_for_current_process(
            source_path,
            target_path,
            filesystem_type,
            flags,
            options,
        )
    }

    pub fn umount_for_current_process(target_path: &str) -> Result<(), MountError> {
        kernel_io_manager::api::vfs::umount_for_current_process(target_path)
    }

    pub fn open_path_for_current_process(
        absolute_path: &str,
        flags: u64,
        mode: u64,
    ) -> Result<u64, VfsError> {
        kernel_io_manager::api::vfs::open_path_for_current_process(absolute_path, flags, mode)
    }

    pub fn metadata_for_current_process_path(absolute_path: &str) -> Result<VfsMetadata, VfsError> {
        kernel_io_manager::api::vfs::metadata_for_current_process_path(absolute_path)
    }

    pub fn check_access_for_user_process(
        absolute_path: &str,
        mode: u64,
        abi: crate::user::abi::UserAbi,
        process_state: &mut crate::user::process_state::UserProcessState,
    ) -> Result<(), VfsError> {
        kernel_io_manager::api::vfs::check_access_for_user_process(
            absolute_path,
            mode,
            abi,
            process_state,
        )
    }

    pub fn read_path_to_vec_for_kernel(
        absolute_path: &str,
    ) -> Result<alloc::vec::Vec<u8>, VfsError> {
        kernel_io_manager::api::vfs::read_path_to_vec_for_kernel(absolute_path)
    }

    pub fn readlink_for_current_process(
        absolute_path: &str,
    ) -> Result<alloc::string::String, VfsError> {
        kernel_io_manager::api::vfs::readlink_for_current_process(absolute_path)
    }
}

pub(crate) mod vfs_core {
    pub fn path_inode(path: &[u8]) -> u64 {
        kernel_io_manager::api::vfs::path_inode(path)
    }
}

pub mod user;

pub mod api;
