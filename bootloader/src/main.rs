#![no_std]
#![no_main]

extern crate alloc;

#[cfg(not(test))]
mod alloc_panic;
mod boot;
mod elf_loader;
mod error;
mod gui;

use crate::boot::boot_kernel;
use crate::error::BootError;
use raw_cpuid::CpuId;
use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if let Err(err) = uefi::helpers::init() {
        return err.status();
    }

    uefi::println!("rustos bootloader started");

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

    match boot_kernel() {
        Ok(()) => Status::SUCCESS,
        Err(err) => report_boot_error(err),
    }
}

fn report_boot_error(err: BootError) -> Status {
    match err {
        BootError::InvalidElf(reason) => {
            uefi::println!("boot error: invalid ELF ({reason})");
        }
        BootError::GraphicsMode(reason) => {
            uefi::println!("boot error: unsupported graphics mode ({reason})");
        }
        _ => uefi::println!("boot error: {:?}", err),
    }
    err.status()
}
