#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

pub mod allocator;
pub mod ipc;
pub mod pager;
pub mod provider_client;
pub mod syscall;

use core::ptr;
use core::slice;

use rustos_user_abi::syscall::{AT_RUSTOS_BOOTSTRAP_HEAP_BASE, AT_RUSTOS_BOOTSTRAP_HEAP_LEN};

// Linux x86_64 auxv terminator code. Defined here to avoid pulling in linux-raw-sys.
const AT_NULL: u64 = 0;

/// Emit the allocation-free, GOT-free static-PIE relocation leaf. It runs
/// before any Rust code can dereference a vtable, static slice, or GOT slot.
/// Service images use a zero link base and only RELA/RELATIVE relocations;
/// unsupported formats fail closed. Reapplying an already-relocated bootstrap
/// image is harmless because RELA carries the original addend.
#[macro_export]
macro_rules! relocation_trampoline {
    () => {
        ::core::arch::global_asm!(
            ".hidden __rustos_svc_relocate",
            ".global __rustos_svc_relocate",
            ".type __rustos_svc_relocate, @function",
            "__rustos_svc_relocate:",
            "    endbr64",
            "    lea rdi, [rip + __ehdr_start]",
            "    lea r8, [rip + _DYNAMIC]",
            "    xor r9d, r9d",
            "    xor r10d, r10d",
            "    mov r11d, 24",
            "2:",
            "    mov rax, [r8]",
            "    test rax, rax",
            "    jz 6f",
            "    cmp rax, 7", // DT_RELA
            "    jne 3f",
            "    mov r9, [r8 + 8]",
            "3:",
            "    cmp rax, 8", // DT_RELASZ
            "    jne 4f",
            "    mov r10, [r8 + 8]",
            "4:",
            "    cmp rax, 9", // DT_RELAENT
            "    jne 5f",
            "    mov r11, [r8 + 8]",
            "5:",
            "    cmp rax, 35", // DT_RELRSZ: packed relocations unsupported
            "    je 9f",
            "    add r8, 16",
            "    jmp 2b",
            "6:",
            "    cmp r11, 24",
            "    jne 9f",
            "    test r10, r10",
            "    jz 8f",
            "    test r9, r9",
            "    jz 9f",
            "    add r9, rdi",
            "7:",
            "    cmp r10, 24",
            "    jb 9f",
            "    cmp qword ptr [r9 + 8], 8", // R_X86_64_RELATIVE, symbol=0
            "    jne 9f",
            "    mov rax, [r9]",
            "    add rax, rdi",
            "    mov rcx, [r9 + 16]",
            "    add rcx, rdi",
            "    mov [rax], rcx",
            "    add r9, 24",
            "    sub r10, 24",
            "    jnz 7b",
            "8:",
            "    ret",
            "9:",
            "    mov eax, 231", // exit_group(127), no policy/runtime needed
            "    mov edi, 127",
            "    syscall",
            "    ud2",
            ".size __rustos_svc_relocate, .-__rustos_svc_relocate",
        );
    };
}

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
        $crate::relocation_trampoline!();
        ::core::arch::global_asm!(
            ".global _start",
            ".type _start, @function",
            "_start:",
            "    endbr64",
            "    xor rbp, rbp",
            "    call __rustos_svc_relocate",
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
        #[cfg(target_arch = "x86_64")]
        if len >= 32 {
            // Exact byte bounds, no SIMD state or alignment requirement. The
            // C ABI enters with DF clear; STOS does not change flags. Assembly
            // also prevents LLVM from lowering this implementation to itself.
            core::arch::asm!(
                "rep stosb",
                inout("rdi") dest => _,
                inout("rcx") len => _,
                in("al") byte,
                options(nostack, preserves_flags),
            );
            return dest;
        }
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
        #[cfg(target_arch = "x86_64")]
        if len >= 32 {
            // Same exact-bound/DF contract as memset. Keep the small byte path
            // below: short IPC envelopes should not pay REP's startup cost.
            core::arch::asm!(
                "rep movsb",
                inout("rdi") dest => _,
                inout("rsi") src => _,
                inout("rcx") len => _,
                options(nostack, preserves_flags),
            );
            return dest;
        }
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
