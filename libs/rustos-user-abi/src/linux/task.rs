//! Linux task, execution, futex, and affinity syscall numbers.
//!
//! The values are generated from the pinned Linux UAPI dependency and are
//! re-exported by `linux`; this module owns no policy or mutable state.

use linux_raw_sys::general as linux;

pub const SYS_GETTID: u64 = linux::__NR_gettid as u64;
pub const SYS_FUTEX: u64 = linux::__NR_futex as u64;
pub const SYS_EXECVE: u64 = linux::__NR_execve as u64;
pub const SYS_SCHED_SETAFFINITY: u64 = linux::__NR_sched_setaffinity as u64;
pub const SYS_SCHED_GETAFFINITY: u64 = linux::__NR_sched_getaffinity as u64;
pub const SYS_SET_TID_ADDRESS: u64 = linux::__NR_set_tid_address as u64;
