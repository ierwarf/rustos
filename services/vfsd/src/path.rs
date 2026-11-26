//! Namespace path normalization shared by every vfsd request handler.
//!
//! Kept in the library target so its tests execute: the `vfsd` binary has no
//! test harness.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_user_abi::syscall::VFS_IPC_PATH_CAPACITY;

use crate::EINVAL;

/// Raw components for permission-checked symlink traversal. The ordinary path
/// is borrowed, and each symlink retains one owned target instead of allocating
/// one String per component. Only unresolved suffixes need a stack entry.
/// Dot components are deliberately returned unchanged: the namespace owner
/// checks directory search permission before interpreting them.
pub struct SymlinkComponents<'a> {
    current: Cow<'a, str>,
    offset: usize,
    suffixes: Vec<(Cow<'a, str>, usize)>,
}

impl<'a> SymlinkComponents<'a> {
    pub fn new(path: &'a str) -> Self {
        Self {
            current: Cow::Borrowed(path),
            offset: path.len() - path.trim_start_matches('/').len(),
            suffixes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.offset == self.current.len() && self.suffixes.is_empty()
    }

    pub fn next_component(&mut self) -> Option<&str> {
        while self.offset == self.current.len() {
            let (path, offset) = self.suffixes.pop()?;
            self.current = path;
            self.offset = offset;
        }
        let start = self.offset;
        let end = self.current[start..].find('/').map_or(self.current.len(), |n| start + n);
        let remaining = &self.current[end..];
        self.offset = end + remaining.len() - remaining.trim_start_matches('/').len();
        Some(&self.current[start..end])
    }

    /// Substitute a symlink target before the still-unresolved suffix. The
    /// caller owns absolute-target root selection and the expansion limit.
    pub fn prepend_target(&mut self, target: String) {
        let offset = target.len() - target.trim_start_matches('/').len();
        let previous = core::mem::replace(&mut self.current, Cow::Owned(target));
        if self.offset != previous.len() {
            self.suffixes.push((previous, self.offset));
        }
        self.offset = offset;
    }
}

/// Resolves `path` against `base_path` into a canonical absolute path with no
/// `.`/`..`/empty components. Refuses NUL and `\` and any result longer than
/// `VFS_IPC_PATH_CAPACITY`.
pub fn normalize_absolute_path(base_path: &str, path: &str) -> Result<String, i32> {
    // `\` is a separator inside the FAT layer (`normalize_fat_path`) but not
    // here, so `a\..\b` would escape this normalizer's `..` handling. FAT
    // names cannot contain it, so refusing it loses no valid path.
    if path.as_bytes().iter().any(|&b| b == 0 || b == b'\\') {
        return Err(EINVAL);
    }
    if path.starts_with('/') {
        if path == "/" {
            return Ok(String::from("/"));
        }
        if path.len() <= VFS_IPC_PATH_CAPACITY
            && path[1..]
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != "..")
        {
            return Ok(String::from(path));
        }
    }
    let capacity = base_path
        .len()
        .saturating_add(path.len())
        .saturating_add(1)
        .min(VFS_IPC_PATH_CAPACITY + 1);
    let mut normalized = String::with_capacity(capacity);
    normalized.push('/');
    if !path.starts_with('/') {
        for component in base_path.split('/') {
            normalize_component(&mut normalized, component);
        }
    }
    for component in path.split('/') {
        normalize_component(&mut normalized, component);
    }
    check_path_capacity(normalized.len())?;
    Ok(normalized)
}

/// VFS wire capacity counts UTF-8 bytes and excludes the terminator.
pub fn check_path_capacity(len: usize) -> Result<(), i32> {
    if len > VFS_IPC_PATH_CAPACITY {
        Err(crate::ENAMETOOLONG)
    } else {
        Ok(())
    }
}

fn normalize_component(normalized: &mut String, component: &str) {
    if component.is_empty() || component == "." {
        return;
    }
    if component == ".." {
        if normalized.len() <= 1 {
            return;
        }
        if let Some(index) = normalized.rfind('/') {
            normalized.truncate(index.max(1));
        }
        return;
    }
    if normalized.len() > 1 {
        normalized.push('/');
    }
    normalized.push_str(component);
}

/// Identity of a path, stable across ASCII case: the boot volume is FAT, whose
/// lookup is case-insensitive (`/BIN/Ls` and `/bin/ls` are one file), so the
/// inode and checkpoint content identity must not tell them apart. 8.3 short
/// aliases still hash as their own spelling; see the vfsd contract in
/// `docs/ai/abi/vfs-storage.md`.
pub fn path_inode(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash.max(1)
}

/// Case-exact variant of `path_inode` for `CasePolicy::Exact` filesystems (e.g. ext4).
pub fn path_inode_exact(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash.max(1)
}

pub fn path_inode_for_policy(bytes: &[u8], case_policy: storage_core::fs::CasePolicy) -> u64 {
    match case_policy {
        storage_core::fs::CasePolicy::Exact => path_inode_exact(bytes),
        storage_core::fs::CasePolicy::AsciiInsensitive
        | storage_core::fs::CasePolicy::UnicodePreservingInsensitive => path_inode(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_components_borrow_plain_paths_and_preserve_dot_order() {
        let mut components = SymlinkComponents::new("//a/./../한글///");
        assert!(matches!(components.current, Cow::Borrowed(_)));
        for expected in ["a", ".", "..", "한글"] {
            assert_eq!(components.next_component(), Some(expected));
        }
        assert!(components.is_empty());
        assert_eq!(components.next_component(), None);
        assert_eq!(components.suffixes.capacity(), 0);
    }

    #[test]
    fn nested_symlink_targets_resume_suffixes_in_order() {
        let mut components = SymlinkComponents::new("/link/tail");
        assert_eq!(components.next_component(), Some("link"));
        components.prepend_target(String::from("../other/end"));
        assert_eq!(components.next_component(), Some(".."));
        assert_eq!(components.next_component(), Some("other"));
        components.prepend_target(String::from("//root///"));
        assert_eq!(components.next_component(), Some("root"));
        assert!(!components.is_empty());
        assert_eq!(components.next_component(), Some("end"));
        assert_eq!(components.next_component(), Some("tail"));
        components.prepend_target(String::from("///"));
        assert!(components.is_empty());
        assert_eq!(components.next_component(), None);
    }

    #[test]
    fn slash_only_target_keeps_the_original_suffix_pending() {
        let mut components = SymlinkComponents::new("link/child");
        assert_eq!(components.next_component(), Some("link"));
        components.prepend_target(String::from("/"));
        assert!(!components.is_empty());
        assert_eq!(components.next_component(), Some("child"));
        assert!(components.is_empty());
    }

    #[test]
    fn case_spellings_of_one_fat_path_share_inode() {
        assert_eq!(path_inode(b"/BIN/Ls"), path_inode(b"/bin/ls"));
        assert_ne!(path_inode(b"/bin/ls"), path_inode(b"/bin/lt"));
        assert_ne!(path_inode(b""), 0);
    }

    #[test]
    fn path_inode_for_policy_separates_case_for_exact_mounts() {
        use storage_core::fs::CasePolicy;
        assert_eq!(
            path_inode_for_policy(b"/data/Foo", CasePolicy::AsciiInsensitive),
            path_inode_for_policy(b"/data/foo", CasePolicy::AsciiInsensitive),
        );
        assert_ne!(
            path_inode_for_policy(b"/data/Foo", CasePolicy::Exact),
            path_inode_for_policy(b"/data/foo", CasePolicy::Exact),
        );
    }

    #[test]
    fn normalizes_cwd_relative_paths() {
        assert_eq!(
            normalize_absolute_path("/usr/lib", "../bin/app").unwrap(),
            "/usr/bin/app"
        );
        assert_eq!(
            normalize_absolute_path("/a", "/b/./c//d/..").unwrap(),
            "/b/c"
        );
        assert_eq!(normalize_absolute_path("/", "../../x").unwrap(), "/x");
    }

    #[test]
    fn backslash_and_nul_cannot_smuggle_dotdot_past_the_normalizer() {
        assert_eq!(normalize_absolute_path("/", "a\\..\\b"), Err(EINVAL));
        assert_eq!(normalize_absolute_path("/", "a\0b"), Err(EINVAL));
    }

    #[test]
    fn overlong_results_are_refused() {
        let long = "a/".repeat(VFS_IPC_PATH_CAPACITY);
        assert_eq!(normalize_absolute_path("/", &long), Err(crate::ENAMETOOLONG));
        let base = alloc::format!("/{}", "a".repeat(VFS_IPC_PATH_CAPACITY - 3));
        assert_eq!(normalize_absolute_path(&base, "b").unwrap().len(), VFS_IPC_PATH_CAPACITY);
        assert_eq!(normalize_absolute_path(&base, "bc"), Err(crate::ENAMETOOLONG));
    }

    #[test]
    fn absolute_path_ignores_the_base_and_keeps_dot_names() {
        assert_eq!(
            normalize_absolute_path("/ignored", "/usr/lib/.hidden").unwrap(),
            "/usr/lib/.hidden"
        );
    }
}
