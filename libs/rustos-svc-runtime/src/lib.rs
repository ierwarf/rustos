#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

pub mod allocator;
pub mod ipc;
pub mod syscall;

use core::ptr;
use core::slice;

use rustos_user_abi::syscall::{AT_RUSTOS_BOOTSTRAP_HEAP_BASE, AT_RUSTOS_BOOTSTRAP_HEAP_LEN};

// Linux x86_64 auxv terminator code. Defined here to avoid pulling in linux-raw-sys.
const AT_NULL: u64 = 0;

/// Declare the entry point of a static-PIE RustOS service.
///
/// Generates `_start` (raw assembly trampoline) plus the
/// `__rustos_svc_rt_start` Rust shim that parses the initial stack, brings up
/// the bootstrap heap allocator, and finally invokes `$main`.
///
/// The macro must be expanded in the binary crate so that `_start` ends up in
/// the final ELF (rlib symbols are otherwise stripped).
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        ::core::arch::global_asm!(
            ".global _start",
            ".type _start, @function",
            "_start:",
            "    xor rbp, rbp",
            "    mov rdi, rsp",
            "    and rsp, -16",
            "    call __rustos_svc_rt_start",
            "    ud2",
        );

        #[no_mangle]
        pub unsafe extern "C" fn __rustos_svc_rt_start(stack_ptr: *const u64) -> ! {
            let main_fn: fn() = $main;
            $crate::__runtime_entry(stack_ptr, main_fn)
        }
    };
}

/// Internal trampoline invoked by the `entry!` macro. Pub-but-hidden so the
/// macro can resolve the path; not part of the stable API.
#[doc(hidden)]
pub unsafe fn __runtime_entry(stack_ptr: *const u64, main: fn()) -> ! {
    let bootstrap = parse_initial_stack(stack_ptr);
    allocator::init(bootstrap);
    main();
    syscall::exit_group(0)
}

#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, len: usize) -> *mut u8 {
    if !dest.is_null() && len != 0 {
        let byte = value as u8;
        let mut index = 0;
        while index < len {
            ptr::write_volatile(dest.add(index), byte);
            index += 1;
        }
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    if !dest.is_null() && !src.is_null() && len != 0 {
        let mut index = 0;
        while index < len {
            let byte = ptr::read_volatile(src.add(index));
            ptr::write_volatile(dest.add(index), byte);
            index += 1;
        }
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    if !dest.is_null() && !src.is_null() && len != 0 {
        if (dest as usize) <= (src as usize) {
            let mut index = 0;
            while index < len {
                let byte = ptr::read_volatile(src.add(index));
                ptr::write_volatile(dest.add(index), byte);
                index += 1;
            }
        } else {
            let mut index = len;
            while index != 0 {
                index -= 1;
                let byte = ptr::read_volatile(src.add(index));
                ptr::write_volatile(dest.add(index), byte);
            }
        }
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(lhs: *const u8, rhs: *const u8, len: usize) -> i32 {
    if lhs.is_null() || rhs.is_null() || len == 0 {
        return 0;
    }
    let lhs = slice::from_raw_parts(lhs, len);
    let rhs = slice::from_raw_parts(rhs, len);
    for (left, right) in lhs.iter().zip(rhs.iter()) {
        if left != right {
            return (*left as i32) - (*right as i32);
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    syscall::exit_group(101)
}

/// Parsed view of the bootstrap heap auxv pair handed to the service by the
/// kernel. `base == 0` means the kernel did not pre-map a heap (e.g. a normal
/// dynamic-linked service that happened to depend on this crate); the
/// allocator falls back to lazy `mmap` in that case.
#[derive(Clone, Copy, Debug, Default)]
pub struct BootstrapHeap {
    pub base: usize,
    pub len: usize,
}

unsafe fn parse_initial_stack(stack_ptr: *const u64) -> BootstrapHeap {
    // SystemV AMD64 ABI initial stack layout (per Linux):
    //   [rsp]      = argc
    //   [rsp+8..]  = argv[0..argc], NULL
    //                envp[..], NULL
    //                auxv[..], { AT_NULL, 0 }
    let argc = ptr::read(stack_ptr) as usize;
    let mut cursor = stack_ptr.add(1 + argc + 1);
    // Skip envp until NULL.
    while ptr::read(cursor) != 0 {
        cursor = cursor.add(1);
    }
    cursor = cursor.add(1);

    let mut bootstrap = BootstrapHeap::default();
    loop {
        let key = ptr::read(cursor);
        let value = ptr::read(cursor.add(1));
        if key == AT_NULL {
            break;
        }
        if key == AT_RUSTOS_BOOTSTRAP_HEAP_BASE {
            bootstrap.base = value as usize;
        } else if key == AT_RUSTOS_BOOTSTRAP_HEAP_LEN {
            bootstrap.len = value as usize;
        }
        cursor = cursor.add(2);
    }
    bootstrap
}
