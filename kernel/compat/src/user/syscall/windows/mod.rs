mod api;
mod dispatch;

pub use api::Api;
pub(crate) use dispatch::dispatch_syscall;
