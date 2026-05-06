#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod debug;
#[cfg(any(test, all(not(test), rustos_boot_image)))]
pub mod multiboot2;
pub mod settings;
#[path = "user_abi.rs"]
pub mod user_abi;
#[path = "util/mod.rs"]
pub mod util;
