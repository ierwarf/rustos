// RING3-MIGRATION-REFERENCE START: decode exception: syscalld owns Win32
// syscall routing policy. Ring0 keeps module dispatch substrate.
mod api;
mod dispatch;
mod fs_object;
mod net_socket;
mod nt_dispatch;
mod process_thread;
mod registry;
mod sync;

pub use api::Api;
pub(crate) use dispatch::dispatch_syscall;
// RING3-MIGRATION-REFERENCE END: syscalld-owned Win32 routing decode exception.
