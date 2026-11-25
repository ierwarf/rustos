// SPDX-License-Identifier: MIT
//! Permission checks for credentials attested on VFS requests.
use storage_core::FsMetadata;

#[derive(Clone, Copy)]
pub struct Credentials { pub uid: u32, pub gid: u32 }

pub fn permits(fs: FsMetadata, cred: Credentials, requested: u32) -> bool {
    if cred.uid == 0 {
        return requested & 1 == 0 || fs.kind == storage_core::FsNodeKind::Directory || fs.mode & 0o111 != 0;
    }
    let bits = if cred.uid == fs.uid { fs.mode >> 6 } else if cred.gid == fs.gid { fs.mode >> 3 } else { fs.mode };
    bits & requested == requested
}

/// `utimensat(2)` mask that only touches times to "now": every time that is
/// set is `UTIME_NOW` (the other may be `UTIME_OMIT`), and no creation time or
/// DOS attribute is requested. Linux treats this as `ATTR_TOUCH`, so a writer
/// who is not the owner is allowed; any explicit timestamp needs ownership.
/// Bits: 1 atime, 2 mtime, 4 crtime, 8 DOS attrs, 16 atime NOW, 32 mtime NOW.
pub const fn utimes_touch_only(mask: u64) -> bool {
    mask & 0b1100 == 0
        && mask & 0b0011 != 0
        && (mask & 1 == 0 || mask & 16 != 0)
        && (mask & 2 == 0 || mask & 32 != 0)
}

pub fn owner(fs: FsMetadata, cred: Credentials) -> bool { cred.uid == 0 || cred.uid == fs.uid }
/// Both NOW permits a writable non-owner; any explicit or mixed request does not.
pub fn set_times_allowed(fs: FsMetadata, cred: Credentials, both_now: bool) -> bool {
    owner(fs, cred) || (both_now && permits(fs, cred, 2))
}
/// Caller identity that `chown(2)` and hard-link protection judge. The kernel
/// keeps no supplementary groups, so group membership is the real or
/// effective primary group.
#[derive(Clone, Copy)]
pub struct OwnerCredentials { pub euid: u32, pub egid: u32, pub gid: u32 }

/// Linux `chown_ok`/`chgrp_ok`: root (`CAP_CHOWN`) may set any owner and
/// group; the file's owner may only keep the owner and move the group to one
/// it belongs to (or leave it). `None` is "unchanged".
pub fn chown_allowed(
    owner_uid: u32,
    owner_gid: u32,
    cred: OwnerCredentials,
    new_uid: Option<u32>,
    new_gid: Option<u32>,
) -> bool {
    if cred.euid == 0 {
        return true;
    }
    if let Some(uid) = new_uid {
        if !(cred.euid == owner_uid && uid == owner_uid) {
            return false;
        }
    }
    if let Some(gid) = new_gid {
        let member = gid == cred.egid || gid == cred.gid || gid == owner_gid;
        if !(cred.euid == owner_uid && member) {
            return false;
        }
    }
    true
}

/// Linux `protected_hardlinks`: a caller that does not own the file may link
/// only a plain regular file without set-id bits that it can read and write.
pub fn hardlink_source_allowed(fs: FsMetadata, cred: Credentials) -> bool {
    if cred.uid == 0 || cred.uid == fs.uid {
        return true;
    }
    let plain = fs.kind == storage_core::FsNodeKind::File
        && fs.mode & 0o4000 == 0
        && !(fs.mode & 0o2000 != 0 && fs.mode & 0o010 != 0);
    plain && permits(fs, cred, 6)
}

pub fn sticky_allows(dir: FsMetadata, victim: FsMetadata, cred: Credentials) -> bool {
    dir.mode & 0o1000 == 0 || owner(dir, cred) || cred.uid == victim.uid
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage_core::{FsNodeKind, FsTimestamp};
    fn meta(mode: u32, uid: u32, kind: FsNodeKind) -> FsMetadata {
        FsMetadata { node_id: 1, kind, len: 0, allocation_size: 0, nlink: 1, mode, uid, gid: uid, dos_attrs: 0, atime: FsTimestamp { secs: 0, nanos: 0 }, mtime: FsTimestamp { secs: 0, nanos: 0 }, ctime: FsTimestamp { secs: 0, nanos: 0 }, crtime: FsTimestamp { secs: 0, nanos: 0 } }
    }
    #[test]
    fn cross_uid_and_sticky_policy() {
        let a = Credentials { uid: 1000, gid: 1000 }; let b = Credentials { uid: 1001, gid: 1001 }; let root = Credentials { uid: 0, gid: 0 };
        let profile = meta(0o700, a.uid, FsNodeKind::Directory);
        let file = meta(0o600, a.uid, FsNodeKind::File);
        let tmp = meta(0o1777, 0, FsNodeKind::Directory);
        assert!(!permits(profile, b, 1)); assert!(!permits(file, b, 6)); assert!(!owner(file, b));
        assert!(permits(file, a, 6)); assert!(sticky_allows(tmp, file, a)); assert!(sticky_allows(tmp, file, root)); assert!(!sticky_allows(tmp, file, b));
    }
    #[test]
    fn timestamp_now_allows_writers_but_explicit_times_require_owner() {
        let fs = meta(0o622, 1000, FsNodeKind::File);
        let writer = Credentials { uid: 1001, gid: 1001 };
        let owner_cred = Credentials { uid: 1000, gid: 1000 };
        let root = Credentials { uid: 0, gid: 0 };
        assert!(set_times_allowed(fs, writer, true));
        assert!(!set_times_allowed(fs, writer, false));
        assert!(!set_times_allowed(meta(0o600, 1000, FsNodeKind::File), writer, true));
        assert!(set_times_allowed(fs, owner_cred, false));
        assert!(set_times_allowed(fs, root, false));
    }
    #[test]
    fn touch_only_covers_now_with_omit_but_not_explicit_times() {
        // (NOW, NOW), (NOW, OMIT), (OMIT, NOW) are touches; any explicit time,
        // creation time or DOS attribute needs the owner.
        assert!(utimes_touch_only(1 | 2 | 16 | 32));
        assert!(utimes_touch_only(1 | 16));
        assert!(utimes_touch_only(2 | 32));
        assert!(!utimes_touch_only(0));
        assert!(!utimes_touch_only(1));
        assert!(!utimes_touch_only(1 | 2 | 16));
        assert!(!utimes_touch_only(1 | 16 | 4));
        assert!(!utimes_touch_only(2 | 32 | 8));
        let fs = meta(0o622, 1000, FsNodeKind::File);
        let writer = Credentials { uid: 1001, gid: 1001 };
        assert!(set_times_allowed(fs, writer, utimes_touch_only(1 | 16)));
    }
    #[test]
    fn runtime_directory_accepts_delegated_socket_owner() {
        let runtime = meta(0o700, 1000, FsNodeKind::Directory);
        let delegated = Credentials { uid: 1000, gid: 1000 };
        let stranger = Credentials { uid: 1001, gid: 1001 };
        assert!(permits(runtime, delegated, 3));
        assert!(!permits(runtime, stranger, 3));
        let socket = meta(0o755, delegated.uid, FsNodeKind::Socket);
        assert!(owner(socket, delegated));
        assert!(!owner(socket, stranger));
    }


    #[test]
    fn chown_is_root_only_for_owner_and_owner_only_for_group() {
        let user = OwnerCredentials { euid: 1000, egid: 100, gid: 100 };
        let root = OwnerCredentials { euid: 0, egid: 0, gid: 0 };
        assert!(chown_allowed(1000, 100, root, Some(5), Some(6)));
        // The owner cannot give the file away, but may keep it.
        assert!(!chown_allowed(1000, 100, user, Some(5), None));
        assert!(chown_allowed(1000, 100, user, Some(1000), None));
        // Group: own group yes, a foreign one no, the file's current one yes.
        assert!(chown_allowed(1000, 100, user, None, Some(100)));
        assert!(!chown_allowed(1000, 100, user, None, Some(7)));
        assert!(chown_allowed(1000, 7, user, None, Some(7)));
        // A non-owner may change nothing, not even toward its own group.
        assert!(!chown_allowed(2000, 100, user, None, Some(100)));
        assert!(!chown_allowed(2000, 100, user, Some(1000), None));
        assert!(chown_allowed(2000, 100, user, None, None));
        // Real gid counts as membership.
        let real = OwnerCredentials { euid: 1000, egid: 100, gid: 200 };
        assert!(chown_allowed(1000, 5, real, None, Some(200)));
    }

    #[test]
    fn protected_hardlinks_demand_rw_on_plain_files_of_others() {
        let user = Credentials { uid: 1000, gid: 1000 };
        assert!(hardlink_source_allowed(meta(0o600, 1000, FsNodeKind::File), user));
        assert!(hardlink_source_allowed(meta(0o000, 0, FsNodeKind::File), Credentials { uid: 0, gid: 0 }));
        assert!(hardlink_source_allowed(meta(0o666, 0, FsNodeKind::File), user));
        assert!(!hardlink_source_allowed(meta(0o644, 0, FsNodeKind::File), user));
        assert!(!hardlink_source_allowed(meta(0o4777, 0, FsNodeKind::File), user));
        assert!(!hardlink_source_allowed(meta(0o2777, 0, FsNodeKind::File), user));
        assert!(!hardlink_source_allowed(meta(0o777, 0, FsNodeKind::Symlink), user));
    }
}

/// renameat2(2): unknown bits and `RENAME_NOREPLACE|RENAME_EXCHANGE` together
/// are `EINVAL`; `RENAME_EXCHANGE` alone is unsupported by this backend.
pub fn renameat2_flags_errno(flags: u64) -> Option<i32> {
    let noreplace = 1;
    let exchange = 2;
    if flags & !(noreplace | exchange) != 0 || flags & (noreplace | exchange) == (noreplace | exchange) {
        return Some(22);
    }
    if flags & exchange != 0 {
        return Some(95);
    }
    None
}

#[cfg(test)]
mod renameat2_flag_tests {
    use super::*;

    #[test]
    fn exchange_with_noreplace_is_einval_not_eopnotsupp() {
        let (n, x) = (1_u64, 2_u64);
        assert_eq!(renameat2_flags_errno(0), None);
        assert_eq!(renameat2_flags_errno(n), None);
        assert_eq!(renameat2_flags_errno(x), Some(95));
        assert_eq!(renameat2_flags_errno(n | x), Some(22));
        assert_eq!(renameat2_flags_errno(1 << 2), Some(22));
    }
}
