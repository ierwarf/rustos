#![no_std]
#![no_main]

extern crate alloc;

mod boot;
mod platform;
mod runtime;

pub(crate) use boot::{boot_info, error};
pub(crate) use platform::{debug, gui, random};

use crate::boot::boot_prekernel;
use crate::error::BootError;
use raw_cpuid::CpuId;
use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if let Err(err) = uefi::helpers::init() {
        return err.status();
    }

    uefi::println!("rustos bootloader started");
    debug::println!("bootloader: start");

    let cpuid = CpuId::new();

    if let Some(topology) = cpuid.get_extended_topology_info() {
        for level in topology {
            uefi::println!(
                "Level: {:?}, CPUs: {}",
                level.level_type(),
                level.processors()
            );
        }
    }

    match boot_prekernel() {
        Ok(()) => Status::SUCCESS,
        Err(err) => report_boot_error(err),
    }
}

fn report_boot_error(err: BootError) -> Status {
    debug::println!("bootloader: error: {:?}", err);
    match err {
        BootError::InvalidElf(reason) => {
            uefi::println!("boot error: {} ({reason})", err.summary());
        }
        BootError::GraphicsMode(reason) => {
            uefi::println!("boot error: {} ({reason})", err.summary());
        }
        _ => uefi::println!("boot error: {} ({:?})", err.summary(), err),
    }
    err.status()
}
