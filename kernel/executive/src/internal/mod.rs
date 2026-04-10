use alloc::format;

use boot_protocol::{BootInfo, BootVolumeTransport};
use diag_abi::{DiagLevel, DiagProvider};
use driver_abi::DriverClass;

use crate::compat_api;
use crate::debug;
use crate::hal_api;
use crate::io_manager_api;
use crate::io_manager_api::BootstrapPhase;
use crate::io_manager_api::api::bootstrap_phase;
use crate::mm_api;
use crate::ps_api;
use crate::user::console_host::{self, ConsoleProgramSpec};
use crate::util::random;

const INITD_EXEC_PATH: &str = "services/initd/initd.elf";

fn emit_flow(level: DiagLevel, event_id: u16, message: &str) {
    debug::emit_text(DiagProvider::Service, level, event_id, 0, 0, message);
}

fn flow_info(event_id: u16, message: &str) {
    emit_flow(DiagLevel::Info, event_id, message);
}

fn flow_debug(event_id: u16, message: &str) {
    emit_flow(DiagLevel::Debug, event_id, message);
}

fn announce_ready(name: &str, console_line: &[u8]) {
    flow_info(20, format!("{name} initialized").as_str());
    debug::println!("{name} initialized.");
    io_manager_api::api::write_console(console_line);
}

mod fatal;
mod tasks;

pub mod boot;
