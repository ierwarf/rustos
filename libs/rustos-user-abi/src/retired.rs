//! Published identifiers that were retired and must never be reused.
//!
//! A retired number stays reserved forever: a new syscall takes a fresh number
//! instead. `cargo xtask check` (tools/xtask/src/abi_drift.rs) rejects any
//! `SYS_RUSTOS_*` declaration that reuses one of these, and docs may mention a
//! retired number only because it is listed here.

/// Native syscall numbers that were removed. See the comments next to
/// `0x5255_0020` and `0x5255_0037` in `syscall.rs` for why each went away.
pub const RETIRED_NATIVE_SYSCALLS: &[u64] = &[0x5255_0020, 0x5255_0021, 0x5255_0022, 0x5255_0037];
