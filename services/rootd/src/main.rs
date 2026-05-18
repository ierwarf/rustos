#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

use rustos_user_abi::syscall::{
    IPC_SERVICE_LINUX_SYSCALLD, IPC_SERVICE_LOADERD, IPC_SERVICE_PROCD, IPC_SERVICE_VFSD,
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, SYS_RUSTOS_SPAWN_EXEC,
};

const SYS_SCHED_YIELD: u64 = 24;
const SYS_EXIT_GROUP: u64 = 231;
const SPAWN_FLAG_LOGICAL_ADMIN: u64 = 1;
// Bootstrap IPC hosts sit on hot syscall/loader/VFS paths. Give them a modest
// Linux nice-like service boost so dynamic-linker and driver bursts do not
// leave runnable servers behind for hundreds of milliseconds.
const CORE_SERVICE_WEIGHT_MICROS: u64 = 4_000;
const INITD_WEIGHT_MICROS: u64 = 2_000;

const SYSCALLD_EXEC: &[u8] = b"services/syscalld/syscalld.elf\0";
const VFSD_EXEC: &[u8] = b"services/vfsd/vfsd.elf\0";
const LOADERD_EXEC: &[u8] = b"services/loaderd/loaderd.elf\0";
const PROCD_EXEC: &[u8] = b"services/procd/procd.elf\0";
const INITD_EXEC: &[u8] = b"services/initd/initd.elf\0";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    debug_line(b"rootd: bootstrap enter\n");

    // Start the core hosts first, then hand off to initd immediately so the
    // remaining bootstrap work can overlap with their own initialization.
    // Initd already gates the services it needs before it launches them, so
    // rootd does not need to serialize the entire bootstrap on readiness.
    spawn_core_service_without_wait(SYSCALLD_EXEC, IPC_SERVICE_LINUX_SYSCALLD);
    spawn_core_service_without_wait(VFSD_EXEC, IPC_SERVICE_VFSD);
    spawn_core_service_without_wait(LOADERD_EXEC, IPC_SERVICE_LOADERD);
    spawn_core_service_without_wait(PROCD_EXEC, IPC_SERVICE_PROCD);

    debug_line(b"rootd: core services spawned, spawning initd\n");
    loop {
        match spawn_exec(INITD_EXEC, INITD_WEIGHT_MICROS) {
            Ok(_) => break,
            Err(_) => yield_now(),
        }
    }

    debug_line(b"rootd: initd spawned\n");
    exit_group(0);
    loop {
        yield_now();
    }
}

fn spawn_core_service_without_wait(path: &'static [u8], service_id: u64) {
    if service_ready(service_id) {
        return;
    }

    loop {
        match spawn_exec(path, CORE_SERVICE_WEIGHT_MICROS) {
            Ok(_) => break,
            Err(_) => yield_now(),
        }
    }
}

fn spawn_exec(path: &'static [u8], weight_micros: u64) -> Result<u64, i64> {
    let argv = [path.as_ptr(), core::ptr::null()];
    let result = syscall6(
        SYS_RUSTOS_SPAWN_EXEC,
        path.as_ptr() as u64,
        argv.as_ptr() as u64,
        0,
        SPAWN_FLAG_LOGICAL_ADMIN,
        0,
        weight_micros,
    );
    if result < 0 {
        Err(-result)
    } else {
        Ok(result as u64)
    }
}

fn service_ready(service_id: u64) -> bool {
    syscall1(SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT, service_id) > 0
}

fn debug_line(bytes: &[u8]) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    );
}

fn yield_now() {
    let _ = syscall0(SYS_SCHED_YIELD);
}

fn exit_group(status: i32) {
    let _ = syscall1(SYS_EXIT_GROUP, status as u64);
}

fn syscall0(number: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    let result: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            in("r8") arg4,
            in("r9") arg5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    debug_line(b"rootd: panic\n");
    loop {
        yield_now();
    }
}
