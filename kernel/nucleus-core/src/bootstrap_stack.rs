//! One-shot aligned stack used for the higher-half bootstrap handoff.
//!
//! - **Owner:** Kernel entry owns this memory until the one-way high-half call.
//! - **Boundary:** Only the compiled size/alignment and the private static
//!   address may form the stack top.
//! - **Lifecycle:** Zero-initialize, derive one aligned top, enter once, and
//!   retain forever because the bootstrap call never returns.
//! - **Concurrency:** BSP-only before interrupts; no AP or user context can
//!   observe the stack.
//! - **Failure:** Arithmetic and layout are compile-time bounded.
//! - **Forbidden:** No reuse, heap backing, runtime resize, AP access, or
//!   publication outside the entry module.
//! - **Evidence:** `kernel-memory-protection` and `service-bootstrap`.

use core::cell::UnsafeCell;

const BOOTSTRAP_STACK_SIZE: usize = 2 * 1024 * 1024;

#[repr(align(16))]
struct BootstrapStack {
    // LAYOUT: Entry assembly uses the aligned object as raw stack storage and
    // never performs a Rust field read.
    #[allow(dead_code)]
    bytes: [u8; BOOTSTRAP_STACK_SIZE],
}

struct BootstrapStackMemory(UnsafeCell<BootstrapStack>);

// SAFETY: Bootstrap is BSP-only and the stack is used exactly once before
// interrupts or any secondary execution context can observe it.
unsafe impl Sync for BootstrapStackMemory {}

static BOOTSTRAP_STACK: BootstrapStackMemory =
    BootstrapStackMemory(UnsafeCell::new(BootstrapStack {
        bytes: [0; BOOTSTRAP_STACK_SIZE],
    }));

pub fn top() -> u64 {
    let base = BOOTSTRAP_STACK.0.get() as *const BootstrapStack as u64;
    base + BOOTSTRAP_STACK_SIZE as u64
}
