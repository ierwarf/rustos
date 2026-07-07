use core::panic::PanicInfo;

use alloc::vec::Vec;
use rustos_user_abi::syscall::SYS_RUSTOS_DEBUG_PRINT;

use crate::syscall;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let msg = b"rustos-svc-runtime: service panic\n";
    unsafe {
        syscall::syscall2(
            SYS_RUSTOS_DEBUG_PRINT,
            msg.as_ptr() as u64,
            msg.len() as u64,
        );
    }
    if let Some(location) = info.location() {
        let mut file = Vec::from(location.file().as_bytes());
        file.push(b'\n');
        unsafe {
            syscall::syscall2(
                SYS_RUSTOS_DEBUG_PRINT,
                file.as_ptr() as u64,
                file.len() as u64,
            );
        }
    }
    syscall::exit_group(101)
}
