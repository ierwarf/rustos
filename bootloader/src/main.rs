#![no_std]
#![no_main]

extern crate alloc;

#[cfg(not(test))]
mod alloc_panic;
mod boot;
mod elf_loader;
mod error;

use crate::boot::boot_kernel;
use crate::error::BootError;
use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if let Err(err) = uefi::helpers::init() {
        return err.status();
    }

    uefi::println!("rustos bootloader started");

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
        _ => uefi::println!("boot error: {:?}", err),
    }
    err.status()
}
