//! Linux ABI constants for the kernel-local anonymous fd objects:
//! `pipe(2)`/`pipe2(2)` and `eventfd(2)`/`eventfd2(2)`.
//!
//! Kept in their own file so the object contract (`docs/ai/abi/local-fd.md`)
//! has one definition site. Values come from `linux-raw-sys`, never retyped.
use super::linux;

pub const SYS_PIPE: u64 = linux::__NR_pipe as u64;
pub const SYS_PIPE2: u64 = linux::__NR_pipe2 as u64;
pub const SYS_EVENTFD: u64 = linux::__NR_eventfd as u64;
pub const SYS_EVENTFD2: u64 = linux::__NR_eventfd2 as u64;

pub const EFD_SEMAPHORE: u64 = linux::EFD_SEMAPHORE as u64;
pub const EFD_CLOEXEC: u64 = linux::EFD_CLOEXEC as u64;
pub const EFD_NONBLOCK: u64 = linux::EFD_NONBLOCK as u64;

/// `O_DIRECT` on `pipe2` selects packet mode, which RustOS does not implement;
/// it is rejected with `EINVAL` rather than silently ignored.
pub const O_DIRECT: u64 = linux::O_DIRECT as u64;
pub const F_SETPIPE_SZ: u64 = linux::F_SETPIPE_SZ as u64;
pub const F_GETPIPE_SZ: u64 = linux::F_GETPIPE_SZ as u64;
/// Writes of at most this many bytes are atomic with respect to other writers.
pub const PIPE_BUF: usize = linux::PIPE_BUF as usize;
pub const SIGPIPE: u64 = linux::SIGPIPE as u64;
pub const S_IFIFO: u32 = linux::S_IFIFO;
pub const POLLRDNORM: u32 = linux::POLLRDNORM;
pub const POLLWRNORM: u32 = linux::POLLWRNORM;
