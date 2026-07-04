#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod api;

#[path = "ipc/mod.rs"]
pub mod ipc;
#[path = "ipc_core.rs"]
pub mod ipc_core;

#[cfg(not(test))]
pub(crate) use kernel_mm::api as memory;
