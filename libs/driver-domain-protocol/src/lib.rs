#![no_std]

//! Fixed, host-initialized memory contracts for isolated driver domains.
//!
//! This crate deliberately describes only byte layouts and validation. Device
//! policy stays in ring3/L0; the kernel may map a known transport only after
//! the header has passed these bounded checks.

#[cfg(all(feature = "std", not(kani)))]
extern crate std;

mod display;
mod gui;
mod gpu;
mod block;
mod input;
mod net;

pub use display::*;
pub use gui::*;
pub use gpu::*;
pub use block::*;
pub use input::*;
pub use net::*;

#[cfg(kani)]
mod verification;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod block_transport_tests;
