//! Linux ABI for advisory file locks: `flock(2)` and the `fcntl(2)` record-lock
//! commands with `struct flock`.
//!
//! Policy (ranges, owners, conflicts, deadlock detection) lives in vfsd
//! (`services/vfsd/src/locks.rs`); ring0 only marshals. Values come from
//! `linux-raw-sys`, never retyped.
use super::linux;

pub const SYS_FLOCK: u64 = linux::__NR_flock as u64;

pub const F_GETLK: u64 = linux::F_GETLK as u64;
pub const F_SETLK: u64 = linux::F_SETLK as u64;
pub const F_SETLKW: u64 = linux::F_SETLKW as u64;
/// The `*64` commands are not defined by the x86_64 UAPI (`linux-raw-sys`
/// omits them); RustOS accepts them as aliases of the plain commands because
/// `struct flock` is already 64-bit.
pub const F_GETLK64: u64 = 12;
pub const F_SETLK64: u64 = 13;
pub const F_SETLKW64: u64 = 14;
pub const F_OFD_GETLK: u64 = linux::F_OFD_GETLK as u64;
pub const F_OFD_SETLK: u64 = linux::F_OFD_SETLK as u64;
pub const F_OFD_SETLKW: u64 = linux::F_OFD_SETLKW as u64;

pub const F_RDLCK: i16 = linux::F_RDLCK as i16;
pub const F_WRLCK: i16 = linux::F_WRLCK as i16;
pub const F_UNLCK: i16 = linux::F_UNLCK as i16;

pub const LOCK_SH: u64 = linux::LOCK_SH as u64;
pub const LOCK_EX: u64 = linux::LOCK_EX as u64;
pub const LOCK_NB: u64 = linux::LOCK_NB as u64;
pub const LOCK_UN: u64 = linux::LOCK_UN as u64;

/// x86_64 `struct flock` (32 bytes, explicit padding so it can be copied as
/// bytes without exposing uninitialized memory).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxFlock {
    pub l_type: i16,
    pub l_whence: i16,
    pub pad0: u32,
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: i32,
    pub pad1: u32,
}

impl LinuxFlock {
    pub const BYTES: usize = 32;

    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0_u8; Self::BYTES];
        out[0..2].copy_from_slice(&self.l_type.to_le_bytes());
        out[2..4].copy_from_slice(&self.l_whence.to_le_bytes());
        out[8..16].copy_from_slice(&self.l_start.to_le_bytes());
        out[16..24].copy_from_slice(&self.l_len.to_le_bytes());
        out[24..28].copy_from_slice(&self.l_pid.to_le_bytes());
        out
    }
}

const _: () = assert!(core::mem::size_of::<LinuxFlock>() == LinuxFlock::BYTES);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flock_numbers_match_x86_64_linux() {
        assert_eq!(SYS_FLOCK, 73);
        assert_eq!((F_GETLK, F_SETLK, F_SETLKW), (5, 6, 7));
        assert_eq!((F_GETLK64, F_SETLK64, F_SETLKW64), (12, 13, 14));
        assert_eq!((F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW), (36, 37, 38));
        assert_eq!((F_RDLCK, F_WRLCK, F_UNLCK), (0, 1, 2));
        assert_eq!((LOCK_SH, LOCK_EX, LOCK_NB, LOCK_UN), (1, 2, 4, 8));
    }

    #[test]
    fn flock_struct_layout_is_the_x86_64_one() {
        assert_eq!(core::mem::size_of::<LinuxFlock>(), 32);
        assert_eq!(core::mem::offset_of!(LinuxFlock, l_start), 8);
        assert_eq!(core::mem::offset_of!(LinuxFlock, l_len), 16);
        assert_eq!(core::mem::offset_of!(LinuxFlock, l_pid), 24);
        let bytes = LinuxFlock {
            l_type: F_WRLCK,
            l_whence: 2,
            l_start: -5,
            l_len: 9,
            l_pid: 77,
            ..LinuxFlock::default()
        }
        .to_bytes();
        assert_eq!(&bytes[0..2], &1_i16.to_le_bytes());
        assert_eq!(&bytes[8..16], &(-5_i64).to_le_bytes());
        assert_eq!(&bytes[24..28], &77_i32.to_le_bytes());
        assert_eq!(&bytes[28..32], &[0; 4]);
    }
}
