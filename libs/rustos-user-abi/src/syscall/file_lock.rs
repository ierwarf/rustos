//! `VFS_IPC_OP_FILE_LOCK`: advisory file locks (`fcntl` record/OFD locks and
//! `flock`) as one small versioned wire struct.
//!
//! Owner of the policy: vfsd (`services/vfsd/src/locks.rs`). Ring0 compat only
//! translates `struct flock` / `flock(2)` into this wire and back. The request
//! header carries `remote_id` (the open description) and the kernel-stamped
//! `pid`/`tid`; the wire never carries a trusted identity (`pid` is reply
//! only and must be `0` in a request).
//!
//! Blocking is not a vfsd feature: vfsd is a single receiver and must never
//! wait. `SETLKW` is a *non-blocking attempt that registers the waiter*: on
//! conflict the status is `EAGAIN` (retry), or `EDEADLK` when the wait would
//! close a cycle of POSIX waiters. The compat layer retries with a bounded
//! sleep, honouring signals, and sends `CANCEL_WAIT` when it gives up.

/// Additive VFS IPC op; `VFS_IPC_ABI_VERSION` is unchanged.
pub const VFS_IPC_OP_FILE_LOCK: u16 = 38;

pub const VFS_FILE_LOCK_WIRE_VERSION: u16 = 1;

/// Report the first lock that would block the described one (reply carries it).
pub const VFS_FILE_LOCK_CMD_GETLK: u16 = 1;
/// Take/convert/release the lock; a conflict is `EAGAIN`.
pub const VFS_FILE_LOCK_CMD_SETLK: u16 = 2;
/// As `SETLK`, and on conflict register the caller thread as a waiter so
/// vfsd can detect POSIX deadlocks (`EDEADLK`). The caller retries.
pub const VFS_FILE_LOCK_CMD_SETLKW: u16 = 3;
/// Forget the waiter registered for the request's `tid` (interrupted wait).
pub const VFS_FILE_LOCK_CMD_CANCEL_WAIT: u16 = 4;
/// Release every POSIX lock the request's `pid` holds on the file of
/// `remote_id`. Sent by the kernel when the process closes any fd of the file
/// (POSIX close semantics) before the fd's description reference is dropped.
pub const VFS_FILE_LOCK_CMD_RELEASE_FILE: u16 = 5;

/// Process-owned record lock (`F_SETLK`...): owner is the request `pid`.
pub const VFS_FILE_LOCK_OWNER_POSIX: u16 = 1;
/// Open-description-owned record lock (`F_OFD_*`): owner is `remote_id`.
pub const VFS_FILE_LOCK_OWNER_OFD: u16 = 2;
/// `flock(2)`: whole-file lock owned by `remote_id`, independent of the
/// record-lock family.
pub const VFS_FILE_LOCK_OWNER_FLOCK: u16 = 3;

pub const VFS_FILE_LOCK_TYPE_RDLCK: u16 = 0;
pub const VFS_FILE_LOCK_TYPE_WRLCK: u16 = 1;
pub const VFS_FILE_LOCK_TYPE_UNLCK: u16 = 2;

pub const VFS_FILE_LOCK_WHENCE_SET: u16 = 0;
pub const VFS_FILE_LOCK_WHENCE_CUR: u16 = 1;
pub const VFS_FILE_LOCK_WHENCE_END: u16 = 2;

/// Fixed-width little-endian payload of `VFS_IPC_OP_FILE_LOCK` (request and
/// reply). Layout (48 bytes):
///
/// ```text
///  0 version u16   2 cmd u16      4 owner u16     6 lock_type u16
///  8 whence u16   10 flags u16 (0) 12 reserved u32 (0)
/// 16 start i64    24 len i64      32 pid i64 (reply)  40 reserved u64 (0)
/// ```
///
/// `start`/`len` are the user's `l_start`/`l_len` unresolved: vfsd owns the
/// description's offset and the file size that `whence` needs. A `GETLK`
/// reply rewrites them as `SEEK_SET`/absolute start/length (`0` = to EOF) and
/// sets `pid` (`-1` for an OFD conflict).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfsFileLockWire {
    pub version: u16,
    pub cmd: u16,
    pub owner: u16,
    pub lock_type: u16,
    pub whence: u16,
    pub flags: u16,
    pub start: i64,
    pub len: i64,
    pub pid: i64,
}

impl VfsFileLockWire {
    pub const WIRE_BYTES: usize = 48;

    pub fn to_bytes(self) -> [u8; Self::WIRE_BYTES] {
        let mut out = [0_u8; Self::WIRE_BYTES];
        out[0..2].copy_from_slice(&self.version.to_le_bytes());
        out[2..4].copy_from_slice(&self.cmd.to_le_bytes());
        out[4..6].copy_from_slice(&self.owner.to_le_bytes());
        out[6..8].copy_from_slice(&self.lock_type.to_le_bytes());
        out[8..10].copy_from_slice(&self.whence.to_le_bytes());
        out[10..12].copy_from_slice(&self.flags.to_le_bytes());
        out[16..24].copy_from_slice(&self.start.to_le_bytes());
        out[24..32].copy_from_slice(&self.len.to_le_bytes());
        out[32..40].copy_from_slice(&self.pid.to_le_bytes());
        out
    }

    /// Decodes exactly `WIRE_BYTES`; the reserved fields must be zero so a
    /// later version can assign them without ambiguity. Field *values* are
    /// checked by [`Self::is_well_formed`].
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::WIRE_BYTES
            || bytes[12..16] != [0; 4]
            || bytes[40..48] != [0; 8]
        {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let i64_at = |o: usize| {
            let mut b = [0_u8; 8];
            b.copy_from_slice(&bytes[o..o + 8]);
            i64::from_le_bytes(b)
        };
        Some(Self {
            version: u16_at(0),
            cmd: u16_at(2),
            owner: u16_at(4),
            lock_type: u16_at(6),
            whence: u16_at(8),
            flags: u16_at(10),
            start: i64_at(16),
            len: i64_at(24),
            pid: i64_at(32),
        })
    }

    /// Version, enumerations and request-only invariants (`flags == 0`).
    pub fn is_well_formed(&self) -> bool {
        self.version == VFS_FILE_LOCK_WIRE_VERSION
            && matches!(
                self.cmd,
                VFS_FILE_LOCK_CMD_GETLK
                    | VFS_FILE_LOCK_CMD_SETLK
                    | VFS_FILE_LOCK_CMD_SETLKW
                    | VFS_FILE_LOCK_CMD_CANCEL_WAIT
                    | VFS_FILE_LOCK_CMD_RELEASE_FILE
            )
            && matches!(
                self.owner,
                VFS_FILE_LOCK_OWNER_POSIX | VFS_FILE_LOCK_OWNER_OFD | VFS_FILE_LOCK_OWNER_FLOCK
            )
            && self.lock_type <= VFS_FILE_LOCK_TYPE_UNLCK
            && self.whence <= VFS_FILE_LOCK_WHENCE_END
            && self.flags == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscall::{VFS_IPC_PAYLOAD_CAPACITY, VFS_IPC_REQUEST_PAYLOAD_CAPACITY};

    fn sample() -> VfsFileLockWire {
        VfsFileLockWire {
            version: VFS_FILE_LOCK_WIRE_VERSION,
            cmd: VFS_FILE_LOCK_CMD_SETLKW,
            owner: VFS_FILE_LOCK_OWNER_OFD,
            lock_type: VFS_FILE_LOCK_TYPE_WRLCK,
            whence: VFS_FILE_LOCK_WHENCE_END,
            flags: 0,
            start: -4096,
            len: 0x1_0000_0000,
            pid: -1,
        }
    }

    #[test]
    fn wire_layout_is_fixed_and_round_trips() {
        assert_eq!(VfsFileLockWire::WIRE_BYTES, 48);
        assert!(VfsFileLockWire::WIRE_BYTES <= VFS_IPC_REQUEST_PAYLOAD_CAPACITY);
        assert!(VfsFileLockWire::WIRE_BYTES <= VFS_IPC_PAYLOAD_CAPACITY);
        let wire = sample();
        let bytes = wire.to_bytes();
        assert_eq!(&bytes[0..2], &1_u16.to_le_bytes());
        assert_eq!(&bytes[2..4], &3_u16.to_le_bytes());
        assert_eq!(&bytes[16..24], &(-4096_i64).to_le_bytes());
        assert_eq!(&bytes[24..32], &0x1_0000_0000_i64.to_le_bytes());
        assert_eq!(&bytes[32..40], &(-1_i64).to_le_bytes());
        assert_eq!(&bytes[12..16], &[0; 4]);
        assert_eq!(&bytes[40..48], &[0; 8]);
        assert_eq!(VfsFileLockWire::from_bytes(&bytes), Some(wire));
        assert!(wire.is_well_formed());
    }

    #[test]
    fn decoding_rejects_wrong_length_and_nonzero_reserved() {
        let bytes = sample().to_bytes();
        assert_eq!(VfsFileLockWire::from_bytes(&bytes[..47]), None);
        let mut long = [0_u8; 49];
        long[..48].copy_from_slice(&bytes);
        assert_eq!(VfsFileLockWire::from_bytes(&long), None);
        for reserved in [12_usize, 15, 40, 47] {
            let mut dirty = bytes;
            dirty[reserved] = 1;
            assert_eq!(VfsFileLockWire::from_bytes(&dirty), None, "byte {reserved}");
        }
    }

    #[test]
    fn well_formed_checks_every_enumeration() {
        let ok = sample();
        for bad in [
            VfsFileLockWire { version: 2, ..ok },
            VfsFileLockWire { cmd: 0, ..ok },
            VfsFileLockWire { cmd: 6, ..ok },
            VfsFileLockWire { owner: 0, ..ok },
            VfsFileLockWire { owner: 4, ..ok },
            VfsFileLockWire { lock_type: 3, ..ok },
            VfsFileLockWire { whence: 3, ..ok },
            VfsFileLockWire { flags: 1, ..ok },
        ] {
            assert!(!bad.is_well_formed(), "{bad:?}");
        }
    }

    #[test]
    fn op_id_is_the_next_free_vfs_op() {
        assert_eq!(VFS_IPC_OP_FILE_LOCK, 38);
        assert!(VFS_IPC_OP_FILE_LOCK > crate::syscall::VFS_IPC_OP_FALLOCATE);
    }
}
