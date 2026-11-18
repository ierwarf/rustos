//! `openat` flag semantics that need no new `vfsd` operation.
//!
//! - **Owner:** `kernel-compat` validates the flag word the way Linux's
//!   `build_open_flags` does; `vfsd` still resolves the path and owns symlink
//!   and permission policy.
//! - **Boundary:** sanitize the untrusted flag word before forwarding it.
//! - **Lifecycle:** the admitted status flags attach to the new description.
//! - **Concurrency:** the service resolves the final link during open.
//! - **Failure:** malformed combinations fail before descriptor publication.
//! - **Forbidden:** grant data access merely because an O_PATH fd is live.
//! - **Evidence:** flag admission and descriptor-rights tests.
//! - **Unknown bits:** `openat` (unlike `openat2`) ignores bits it does not
//!   define, so they are cleared, not refused.
//! - **`O_PATH`:** only `O_DIRECTORY`, `O_NOFOLLOW` and `O_CLOEXEC` may
//!   accompany it (`EINVAL` otherwise). The flag travels to `vfsd` and is
//!   recorded in the descriptor's status flags, where it downgrades the
//!   descriptor's rights to transfer-only, so write, truncate and map are
//!   refused with `EBADF`.
//! - **`O_NOFOLLOW`:** passed to `vfsd` unchanged; honouring it (`ELOOP` on a
//!   final symlink) is `vfsd`'s resolution policy.
//! - **`O_TMPFILE`:** validated, then `EOPNOTSUPP`: an unnamed inode needs a
//!   `vfsd` operation that does not exist yet.
use super::*;

/// Every `open` flag Linux defines on x86-64 (`VALID_OPEN_FLAGS`).
const VALID_OPEN_FLAGS: u64 = linux_abi::O_ACCMODE
    | linux_abi::O_CREAT
    | linux_abi::O_EXCL
    | linux_abi::O_NOCTTY
    | linux_abi::O_TRUNC
    | linux_abi::O_APPEND
    | linux_abi::O_NONBLOCK
    | O_DSYNC
    | O_FASYNC
    | linux_abi::O_DIRECT
    | O_LARGEFILE
    | linux_abi::O_DIRECTORY
    | linux_abi::O_NOFOLLOW
    | O_NOATIME
    | linux_abi::O_CLOEXEC
    | O_SYNC_BIT
    | linux_abi::O_PATH
    | O_TMPFILE_BIT;

const O_DSYNC: u64 = 0o10000;
const O_FASYNC: u64 = 0o20000;
const O_LARGEFILE: u64 = 0o100000;
const O_NOATIME: u64 = 0o1000000;
/// The `__O_SYNC` bit; `O_SYNC` is `__O_SYNC | O_DSYNC`.
const O_SYNC_BIT: u64 = 0o4000000;
/// The `__O_TMPFILE` bit; `O_TMPFILE` is `__O_TMPFILE | O_DIRECTORY`.
const O_TMPFILE_BIT: u64 = linux_abi::O_TMPFILE & !linux_abi::O_DIRECTORY;
const O_PATH_COMPANIONS: u64 =
    linux_abi::O_DIRECTORY | linux_abi::O_NOFOLLOW | linux_abi::O_PATH | linux_abi::O_CLOEXEC;

/// The flag word to hand to `vfsd`, or the errno Linux would return.
pub(super) fn effective_open_flags(flags: u64) -> Result<u64, i64> {
    let flags = flags & VALID_OPEN_FLAGS;
    if flags & O_TMPFILE_BIT != 0 {
        if flags & linux_abi::O_DIRECTORY == 0 {
            return Err(LINUX_EINVAL);
        }
        if flags & linux_abi::O_ACCMODE == linux_abi::O_RDONLY {
            return Err(LINUX_EINVAL);
        }
    }
    if flags & linux_abi::O_PATH != 0 && flags & !O_PATH_COMPANIONS != 0 {
        return Err(LINUX_EINVAL);
    }
    if flags & O_TMPFILE_BIT != 0 {
        return Err(LINUX_EOPNOTSUPP);
    }
    Ok(flags)
}

pub(super) fn syscall_linux_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64) -> u64 {
    match effective_open_flags(flags) {
        Ok(flags) => syscall_linux_vfs_openat(dirfd, path_ptr, flags, mode),
        Err(errno) => linux_errno(errno),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_bits_are_ignored_like_openat() {
        let unknown = 1_u64 << 40;
        assert_eq!(effective_open_flags(linux_abi::O_RDWR | unknown), Ok(linux_abi::O_RDWR));
        assert_eq!(effective_open_flags(unknown | 0o200000000), Ok(0));
    }

    #[test]
    fn ordinary_flags_pass_through_unchanged() {
        let flags = linux_abi::O_RDWR
            | linux_abi::O_CREAT
            | linux_abi::O_EXCL
            | linux_abi::O_TRUNC
            | linux_abi::O_CLOEXEC
            | linux_abi::O_NOFOLLOW
            | linux_abi::O_NONBLOCK
            | O_LARGEFILE
            | O_SYNC_BIT
            | O_DSYNC;
        assert_eq!(effective_open_flags(flags), Ok(flags));
    }

    #[test]
    fn o_path_accepts_only_its_companions() {
        let ok = linux_abi::O_PATH | linux_abi::O_DIRECTORY | linux_abi::O_NOFOLLOW | linux_abi::O_CLOEXEC;
        assert_eq!(effective_open_flags(ok), Ok(ok));
        assert_eq!(effective_open_flags(linux_abi::O_PATH), Ok(linux_abi::O_PATH));
        assert_eq!(effective_open_flags(linux_abi::O_PATH | linux_abi::O_RDWR), Err(LINUX_EINVAL));
        assert_eq!(effective_open_flags(linux_abi::O_PATH | linux_abi::O_WRONLY), Err(LINUX_EINVAL));
        assert_eq!(effective_open_flags(linux_abi::O_PATH | linux_abi::O_CREAT), Err(LINUX_EINVAL));
        assert_eq!(effective_open_flags(linux_abi::O_PATH | linux_abi::O_TRUNC), Err(LINUX_EINVAL));
    }

    #[test]
    fn o_tmpfile_is_validated_then_unsupported() {
        let tmp = linux_abi::O_TMPFILE;
        assert_eq!(effective_open_flags(tmp | linux_abi::O_RDWR), Err(LINUX_EOPNOTSUPP));
        assert_eq!(effective_open_flags(tmp | linux_abi::O_WRONLY), Err(LINUX_EOPNOTSUPP));
        // read-only tmpfile and a lone __O_TMPFILE bit are malformed.
        assert_eq!(effective_open_flags(tmp), Err(LINUX_EINVAL));
        assert_eq!(effective_open_flags(O_TMPFILE_BIT | linux_abi::O_RDWR), Err(LINUX_EINVAL));
        // O_DIRECTORY alone is the ordinary directory open.
        assert_eq!(
            effective_open_flags(linux_abi::O_DIRECTORY),
            Ok(linux_abi::O_DIRECTORY)
        );
    }

    #[test]
    fn flag_constants_match_the_uapi() {
        assert_eq!(O_TMPFILE_BIT, 0o20000000);
        assert_eq!(linux_abi::O_TMPFILE, 0o20200000);
        assert_eq!(linux_abi::O_PATH, 0o10000000);
        assert_eq!(linux_abi::O_NOFOLLOW, 0o400000);
        assert_eq!(linux_abi::O_CLOEXEC, 0o2000000);
        assert_eq!(linux_abi::O_DIRECT, 0o40000);
    }
}
