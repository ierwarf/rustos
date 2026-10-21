//! Kernel `memcpy`/`memset`, replacing the weak `compiler_builtins` copies.
//!
//! - **Owner:** the nucleus image; every Rust move/copy the compiler lowers to
//!   a libcall lands here, including the IPC envelope and scheduler record
//!   copies on every round trip.
//! - **Boundary:** plain loads/stores (LLVM may pair them into SSE `movups`,
//!   which the nucleus SIMD audit permits: data movement, no `MXCSR` effect)
//!   and `rep movsb`/`rep stosb`.
//! - **Failure:** the builtin form ran three `rep` instructions per call (head
//!   bytes, quadwords, tail bytes) whatever the length. Most kernel copies are
//!   short, so each paid three microcoded start-ups; guest `perf` of the
//!   eight-vCPU IPC benchmark put it among the hottest kernel symbols.
//! - **Shape:** up to 64 bytes uses overlapping unaligned word moves (every
//!   load is issued before any store, so the result is exact for any length in
//!   the bucket); longer spans use one `rep` (ERMS/FSRM-friendly, and still
//!   correct on CPUs without them). No loop exists for LLVM to re-idiomize
//!   into a recursive libcall.

use core::ptr::{read_unaligned as load, write_unaligned as store};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    // SAFETY: the C contract: both ranges are valid for `len` bytes and do not
    // overlap. Every access below stays inside `[0, len)`.
    unsafe {
        if len <= 16 {
            if len >= 8 {
                let (a, b) = (
                    load(src.cast::<u64>()),
                    load(src.add(len - 8).cast::<u64>()),
                );
                store(dest.cast::<u64>(), a);
                store(dest.add(len - 8).cast::<u64>(), b);
            } else if len >= 4 {
                let (a, b) = (
                    load(src.cast::<u32>()),
                    load(src.add(len - 4).cast::<u32>()),
                );
                store(dest.cast::<u32>(), a);
                store(dest.add(len - 4).cast::<u32>(), b);
            } else if len != 0 {
                let (a, b, c) = (*src, *src.add(len / 2), *src.add(len - 1));
                *dest = a;
                *dest.add(len / 2) = b;
                *dest.add(len - 1) = c;
            }
        } else if len <= 32 {
            let a = load(src.cast::<u64>());
            let b = load(src.add(8).cast::<u64>());
            let c = load(src.add(len - 16).cast::<u64>());
            let d = load(src.add(len - 8).cast::<u64>());
            store(dest.cast::<u64>(), a);
            store(dest.add(8).cast::<u64>(), b);
            store(dest.add(len - 16).cast::<u64>(), c);
            store(dest.add(len - 8).cast::<u64>(), d);
        } else if len <= 64 {
            let head = [
                load(src.cast::<u64>()),
                load(src.add(8).cast::<u64>()),
                load(src.add(16).cast::<u64>()),
                load(src.add(24).cast::<u64>()),
            ];
            let tail = [
                load(src.add(len - 32).cast::<u64>()),
                load(src.add(len - 24).cast::<u64>()),
                load(src.add(len - 16).cast::<u64>()),
                load(src.add(len - 8).cast::<u64>()),
            ];
            store(dest.cast::<u64>(), head[0]);
            store(dest.add(8).cast::<u64>(), head[1]);
            store(dest.add(16).cast::<u64>(), head[2]);
            store(dest.add(24).cast::<u64>(), head[3]);
            store(dest.add(len - 32).cast::<u64>(), tail[0]);
            store(dest.add(len - 24).cast::<u64>(), tail[1]);
            store(dest.add(len - 16).cast::<u64>(), tail[2]);
            store(dest.add(len - 8).cast::<u64>(), tail[3]);
        } else {
            // The SysV ABI enters with DF clear; MOVS does not change flags.
            core::arch::asm!(
                "rep movsb",
                inout("rdi") dest => _,
                inout("rsi") src => _,
                inout("rcx") len => _,
                options(nostack, preserves_flags),
            );
        }
    }
    dest
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, len: usize) -> *mut u8 {
    let byte = value as u8;
    let word = u64::from(byte) * 0x0101_0101_0101_0101;
    // SAFETY: the C contract: `dest` is valid for `len` bytes. Every store
    // below stays inside `[0, len)`.
    unsafe {
        if len <= 16 {
            if len >= 8 {
                store(dest.cast::<u64>(), word);
                store(dest.add(len - 8).cast::<u64>(), word);
            } else if len >= 4 {
                store(dest.cast::<u32>(), word as u32);
                store(dest.add(len - 4).cast::<u32>(), word as u32);
            } else if len != 0 {
                *dest = byte;
                *dest.add(len / 2) = byte;
                *dest.add(len - 1) = byte;
            }
        } else if len <= 32 {
            store(dest.cast::<u64>(), word);
            store(dest.add(8).cast::<u64>(), word);
            store(dest.add(len - 16).cast::<u64>(), word);
            store(dest.add(len - 8).cast::<u64>(), word);
        } else {
            core::arch::asm!(
                "rep stosb",
                inout("rdi") dest => _,
                inout("rcx") len => _,
                in("al") byte,
                options(nostack, preserves_flags),
            );
        }
    }
    dest
}
