pub(crate) use super::*;

pub mod futex_thread;
pub mod ipc_helpers;
pub(in crate::user::syscall::linux) mod local_fd;
mod local_memfd_io;
pub mod poll_epoll;
pub mod proc_identity;
pub mod process_time;
pub mod vfs_meta;
pub mod vfs_socket;

pub use futex_thread::*;
pub use ipc_helpers::*;
pub use local_fd::{
    syscall_linux_eventfd, syscall_linux_eventfd2, syscall_linux_pipe, syscall_linux_pipe2,
};
pub use poll_epoll::*;
pub use process_time::*;
pub use vfs_meta::*;
pub use vfs_socket::*;

pub(super) fn service_deferred_handle_maintenance() -> usize {
    vfs_socket::service_deferred_netd_refs()
        .saturating_add(ipc_helpers::service_deferred_vfs_mutations())
}
