#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub use kernel_base::memory;
pub use kernel_base::user;

pub mod api;

mod ipc_shim {
    pub use kernel_base::ipc::*;
}

pub(crate) use ipc_shim as ipc;
