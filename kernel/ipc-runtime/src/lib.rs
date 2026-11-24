#![no_std]

#[cfg(all(feature = "host-test", rustos_boot_image))]
compile_error!("kernel-ipc-runtime host-test must never be enabled for the RustOS target");

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod api;

#[path = "ipc/mod.rs"]
pub(crate) mod ipc;

#[cfg(not(test))]
pub(crate) use kernel_mm::api as memory;
