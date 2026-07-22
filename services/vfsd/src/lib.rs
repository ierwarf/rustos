#![no_std]

extern crate alloc;

use alloc::format;
use rustos_user_abi::syscall::{VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_WRITE};

pub const ENOENT: i32 = 2;
pub const EROFS: i32 = 30;

/// Persistent mutation remains unavailable until a journal/recovery protocol
/// is implemented. Keeping this decision in the testable policy library makes
/// the service dispatch and the formal admission model share one source gate.
pub const fn persistent_mutation_status(op: u16) -> Option<i32> {
    match op {
        VFS_IPC_OP_WRITE | VFS_IPC_OP_FTRUNCATE => Some(EROFS),
        _ => None,
    }
}

pub fn mkdir_policy(path: &str, euid: u32) -> i32 {
    let run_user_path = format!("/run/user/{euid}");
    if path == "/run" || path == "/run/user" || path == run_user_path.as_str() {
        0
    } else {
        EROFS
    }
}

pub fn unlink_policy(path: &str) -> i32 {
    if path.starts_with("/run/") {
        ENOENT
    } else {
        EROFS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_mutation_admission_remains_read_only() {
        assert_eq!(persistent_mutation_status(VFS_IPC_OP_WRITE), Some(EROFS));
        assert_eq!(
            persistent_mutation_status(VFS_IPC_OP_FTRUNCATE),
            Some(EROFS)
        );
        assert_eq!(persistent_mutation_status(0xffff), None);
        assert_eq!(mkdir_policy("/var/lib/rustos", 0), EROFS);
        assert_eq!(unlink_policy("/var/lib/rustos/state"), EROFS);
        assert_eq!(mkdir_policy("/run/user/1000", 1000), 0);
        assert_eq!(unlink_policy("/run/user/1000/socket"), ENOENT);
    }
}
