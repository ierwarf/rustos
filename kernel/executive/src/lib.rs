#![no_std]

extern crate alloc;

use alloc::format;

pub(crate) mod debug {
    pub use nucleus_core::debug::*;
}

pub(crate) fn flow_info(event_id: u16, _message: &str) {
    let _ = event_id;
    debug::info!(service, "{}", _message);
}

pub(crate) fn flow_debug(event_id: u16, _message: &str) {
    let _ = event_id;
    debug::debug!(service, "{}", _message);
}

pub(crate) fn announce_ready(name: &str, console_line: &[u8]) {
    flow_info(20, format!("{name} initialized").as_str());
    crate::io_services::console_write(console_line);
}

mod hal_hooks;

mod fatal;
mod io_services;
mod tasks;

pub mod boot;
