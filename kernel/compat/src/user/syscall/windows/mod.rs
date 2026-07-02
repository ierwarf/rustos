// RING3-MIGRATION-REFERENCE START: decode exception: syscalld owns Win32
// syscall routing policy. Ring0 keeps module dispatch substrate.
mod api;
mod dispatch;

pub use api::Api;
pub(crate) use dispatch::dispatch_syscall;
// RING3-MIGRATION-REFERENCE END: syscalld-owned Win32 routing decode exception.
