#![no_std]

extern crate alloc;

pub mod api;

#[path = "internal/input_core.rs"]
pub mod input_core;
#[path = "internal/vfs_core.rs"]
pub mod vfs_core;
