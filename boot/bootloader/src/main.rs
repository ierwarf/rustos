#![cfg_attr(all(not(test), rustos_boot_image), no_std)]
#![cfg_attr(all(not(test), rustos_boot_image), no_main)]

#[cfg(all(not(test), rustos_boot_image))]
extern crate alloc;

#[cfg(all(not(test), rustos_boot_image))]
mod boot;
#[cfg(all(not(test), rustos_boot_image))]
mod platform;
#[cfg(all(not(test), rustos_boot_image))]
mod runtime;
#[cfg(all(not(test), rustos_boot_image))]
#[path = "../../../settings.rs"]
mod settings;

#[cfg(all(not(test), rustos_boot_image))]
pub(crate) use boot::{boot_info, error};
#[cfg(all(not(test), rustos_boot_image))]
pub(crate) use platform::{debug, gui, random};

#[cfg(all(not(test), rustos_boot_image))]
use crate::error::BootError;
#[cfg(all(not(test), rustos_boot_image))]
use uefi::prelude::*;

#[cfg(all(not(test), rustos_boot_image))]
#[entry]
fn main() -> Status {
    use crate::boot::boot_kernel;
    use raw_cpuid::CpuId;

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

    match boot_kernel() {
        Ok(()) => Status::SUCCESS,
        Err(err) => report_boot_error(err),
    }
}

#[cfg(any(test, not(rustos_boot_image)))]
fn main() {}

#[cfg(all(not(test), rustos_boot_image))]
fn report_boot_error(err: BootError) -> Status {
    debug::println!("bootloader: error: {:?}", err);
    match err {
        BootError::InvalidElf(reason) => {
            uefi::println!("boot error: {} ({reason})", err.summary());
        }
        BootError::InvalidBootInfo(reason) => {
            uefi::println!("boot error: {} ({reason})", err.summary());
        }
        BootError::GraphicsMode(reason) => {
            uefi::println!("boot error: {} ({reason})", err.summary());
        }
        _ => uefi::println!("boot error: {} ({:?})", err.summary(), err),
    }
    err.status()
}
