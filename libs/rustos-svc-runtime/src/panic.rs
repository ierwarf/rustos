use core::panic::PanicInfo;

use rustos_user_abi::syscall::SYS_RUSTOS_DEBUG_PRINT;

use crate::syscall;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let msg = b"rustos-svc-runtime: service panic\n";
    unsafe {
        syscall::syscall2(SYS_RUSTOS_DEBUG_PRINT, msg.as_ptr() as u64, msg.len() as u64);
    }
    if let Some(location) = info.location() {
        let file = location.file().as_bytes();
        unsafe {
            syscall::syscall2(SYS_RUSTOS_DEBUG_PRINT, file.as_ptr() as u64, file.len() as u64);
            syscall::syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
        }
    }
    syscall::exit_group(101)
}
