use alloc::vec::Vec;
use embedded_graphics::pixelcolor::Rgb888;
use core::{convert::TryFrom, fmt};
use fatfs::{Read, Seek, SeekFrom};
use x86_64::instructions::{hlt, interrupts};

use crate::debug;
use crate::{fat, gui, process};

const USER_DEMO_EXE_PATH: &str = "USERDEMO.EXE";
const USER_DEMO_ELF_PATH: &str = "USERDEMO.ELF";
const USER_DEMO_WEIGHT_MICROS: u64 = 50;

pub fn run() -> ! {
    gui::GOP_SCREEN.lock().fill(Rgb888::new(0, 0, 0));

    let (userdemo_path, userdemo_image) = match read_preferred_user_demo() {
        Ok(value) => value,
        Err(err) => fatal(format_args!(
            "failed to read {} or {} from boot volume: {:?}",
            USER_DEMO_EXE_PATH,
            USER_DEMO_ELF_PATH,
            err
        )),
    };
    let spawned = match process::spawn_process(&userdemo_image, USER_DEMO_WEIGHT_MICROS, 0, 0) {
        Ok(spawned) => spawned,
        Err(err) => {
            err.log_debug_details();
            fatal(format_args!(
                "failed to spawn {}: {} ({:?})",
                userdemo_path,
                err.summary(),
                err
            ))
        }
    };

    debug::println!(
        "Ring3 process spawned: pid={} entry={:#x} weight={}us path={}",
        spawned.pid,
        spawned.entry.as_u64(),
        USER_DEMO_WEIGHT_MICROS,
        userdemo_path
    );

    interrupts::enable();
    loop {
        hlt();
    }
}

fn read_boot_file(path: &str) -> core::result::Result<Vec<u8>, fatfs::Error<fat::DiskIoError>> {
    let volume = fat::BootVolume::open()?;
    let result = {
        let mut file = volume.open_file(path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        let capacity =
            usize::try_from(file_len).map_err(|_| fatfs::Error::Io(fat::DiskIoError::InvalidInput))?;
        file.seek(SeekFrom::Start(0))?;

        let mut bytes = Vec::with_capacity(capacity);
        let mut chunk = [0_u8; 4096];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }

        Ok(bytes)
    };

    match (result, volume.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

fn read_preferred_user_demo(
) -> core::result::Result<(&'static str, Vec<u8>), fatfs::Error<fat::DiskIoError>> {
    match read_boot_file(USER_DEMO_EXE_PATH) {
        Ok(image) => Ok((USER_DEMO_EXE_PATH, image)),
        Err(_) => read_boot_file(USER_DEMO_ELF_PATH).map(|image| (USER_DEMO_ELF_PATH, image)),
    }
}

fn fatal(args: fmt::Arguments<'_>) -> ! {
    debug::println!();
    debug::println!("[KERNEL FATAL]");
    debug::println!("{}", args);
    interrupts::disable();
    loop {
        hlt();
    }
}
