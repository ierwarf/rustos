#![no_std]

extern crate alloc;

use alloc::format;

#[macro_export]
macro_rules! executive_debug_println {
    () => {{
        nucleus_core::debug::println_newline();
    }};
    ($($arg:tt)*) => {{
        nucleus_core::debug::println_fmt(format_args!($($arg)*));
    }};
}

#[allow(unused_imports, unused_macros)]
pub mod debug {
    pub use crate::executive_debug_println as println;
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
