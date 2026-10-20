use rustos_svc_runtime::{memcpy, memmove, memset};
use std::hint::black_box;
use std::time::Instant;

fn pattern(index: usize) -> u8 {
    index.wrapping_mul(37).wrapping_add(index / 251) as u8
}

#[test]
fn copies_exact_unaligned_spans_without_changing_guards() {
    let lengths = (0..=257).chain([511, 512, 1023, 4095, 4096, 4097, 65535, 65536]);
    for len in lengths {
        for source_offset in [0, 1, 7, 15] {
            let source: Vec<_> = (0..len + 32).map(pattern).collect();
            for dest_offset in [0, 1, 7, 15] {
                let mut dest = vec![0xa5; len + 32];
                let start = unsafe { dest.as_mut_ptr().add(dest_offset) };
                let result =
                    unsafe { black_box(memcpy)(start, source.as_ptr().add(source_offset), len) };
                assert_eq!(result, start);
                for (index, &byte) in dest.iter().enumerate() {
                    let expected = if (dest_offset..dest_offset + len).contains(&index) {
                        pattern(source_offset + index - dest_offset)
                    } else {
                        0xa5
                    };
                    assert_eq!(
                        byte, expected,
                        "len={len} src={source_offset} dst={dest_offset} index={index}"
                    );
                }
            }
            assert!(source
                .iter()
                .enumerate()
                .all(|(index, &byte)| byte == pattern(index)));
        }
    }
}

#[test]
fn fills_exact_spans_and_truncates_the_fill_value() {
    for len in (0..=257).chain([511, 512, 4095, 4096, 4097, 65536]) {
        for offset in [0, 1, 7, 15] {
            for value in [0, 0x5a, 0x1ab, -1] {
                let mut dest = vec![0xa5; len + 32];
                let start = unsafe { dest.as_mut_ptr().add(offset) };
                assert_eq!(unsafe { black_box(memset)(start, value, len) }, start);
                for (index, &byte) in dest.iter().enumerate() {
                    let expected = if (offset..offset + len).contains(&index) {
                        value as u8
                    } else {
                        0xa5
                    };
                    assert_eq!(
                        byte, expected,
                        "len={len} offset={offset} value={value} index={index}"
                    );
                }
            }
        }
    }
}

#[test]
fn moves_preserve_both_overlap_directions_and_identical_spans() {
    for len in [0, 1, 7, 16, 31, 64, 255, 4096] {
        for source in [0, 1, 7, 16, 32] {
            for dest in [0, 1, 7, 16, 32] {
                let mut bytes: Vec<_> = (0..len + 64).map(pattern).collect();
                let start = unsafe { bytes.as_mut_ptr().add(dest) };
                let result = unsafe { black_box(memmove)(start, bytes.as_ptr().add(source), len) };
                assert_eq!(result, start);
                for (index, &byte) in bytes.iter().enumerate() {
                    let expected = if (dest..dest + len).contains(&index) {
                        pattern(source + index - dest)
                    } else {
                        pattern(index)
                    };
                    assert_eq!(
                        byte, expected,
                        "len={len} src={source} dst={dest} index={index}"
                    );
                }
            }
        }
    }
}

#[test]
fn empty_operations_return_the_destination_without_touching_memory() {
    let source = 0x23;
    let mut byte = 0x5a_u8;
    let dest = &mut byte as *mut u8;
    unsafe {
        assert_eq!(black_box(memcpy)(dest, &source, 0), dest);
        assert_eq!(black_box(memmove)(dest, &source, 0), dest);
        assert_eq!(black_box(memset)(dest, 0, 0), dest);
    }
    assert_eq!(byte, 0x5a);
    assert_eq!(source, 0x23);
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn bulk_operations_do_not_access_either_guard_page() {
    use rustos_svc_runtime::syscall::{mmap_anonymous, syscall2, syscall3};
    // Native Linux x86-64 test-only syscall numbers. The runtime deliberately
    // does not enable the optional Linux compatibility-state ABI dependency.
    const SYS_MPROTECT: u64 = 10;
    const SYS_MUNMAP: u64 = 11;
    const PAGE: usize = 4096;
    struct GuardedPage(*mut u8);
    impl GuardedPage {
        fn new() -> Self {
            let mapping = unsafe { mmap_anonymous(3 * PAGE) };
            assert!(mapping > 0, "mmap failed: {mapping}");
            let page = Self(mapping as *mut u8);
            for offset in [0, 2 * PAGE] {
                assert_eq!(
                    unsafe { syscall3(SYS_MPROTECT, page.0.add(offset) as u64, PAGE as u64, 0) },
                    0
                );
            }
            page
        }
        fn bytes(&self) -> *mut u8 {
            unsafe { self.0.add(PAGE) }
        }
    }
    impl Drop for GuardedPage {
        fn drop(&mut self) {
            unsafe { syscall2(SYS_MUNMAP, self.0 as u64, (3 * PAGE) as u64) };
        }
    }

    let source = GuardedPage::new();
    let dest = GuardedPage::new();
    for index in 0..PAGE {
        unsafe { source.bytes().add(index).write(pattern(index)) };
    }
    // Source is also read-only: a direction/register error cannot modify it.
    assert_eq!(
        unsafe { syscall3(SYS_MPROTECT, source.bytes() as u64, PAGE as u64, 1) },
        0
    );
    for len in [0, 1, 7, 31, 32, 33, 64, 511, PAGE] {
        for source_offset in [0, PAGE - len] {
            for dest_offset in [0, PAGE - len] {
                unsafe {
                    let start = dest.bytes().add(dest_offset);
                    black_box(memcpy)(start, source.bytes().add(source_offset), len);
                    for index in 0..len {
                        assert_eq!(start.add(index).read(), pattern(source_offset + index));
                    }
                    black_box(memset)(start, 0x5a, len);
                    for index in 0..len {
                        assert_eq!(start.add(index).read(), 0x5a);
                    }
                }
            }
        }
    }
}

// Native instruction-cost evidence only, not guest IPC or shipping acceptance.
// Run with --no-default-features --release -- --ignored --nocapture.
#[test]
#[ignore]
fn memory_primitives_native_benchmark() {
    for len in [8, 64, 512, 4096, 65536, 1048576] {
        let source: Vec<_> = (0..len + 32).map(pattern).collect();
        let mut dest = vec![0u8; len + 32];
        let iterations = (4 * 1024 * 1024 / len).clamp(2, 16384);
        for operation in ["memcpy", "memset"] {
            let mut samples = Vec::new();
            let copy = black_box(memcpy);
            let fill = black_box(memset);
            for sample in 0..35 {
                let start = Instant::now();
                for _ in 0..iterations {
                    unsafe {
                        if operation == "memcpy" {
                            copy(
                                black_box(dest.as_mut_ptr().add(1)),
                                black_box(source.as_ptr().add(7)),
                                black_box(len),
                            );
                        } else {
                            fill(
                                black_box(dest.as_mut_ptr().add(1)),
                                black_box(0x5a),
                                black_box(len),
                            );
                        }
                    }
                }
                let ns = start.elapsed().as_nanos() as f64 / iterations as f64;
                if sample >= 4 {
                    samples.push(ns);
                }
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "memory-native op={operation} len={len} p50_ns={:.2} p95_ns={:.2}",
                samples[15], samples[29]
            );
            black_box(&dest);
        }
    }
}
