//! Namespace path normalization shared by every vfsd request handler.
//!
//! Kept in the library target so its tests execute: the `vfsd` binary has no
//! test harness.

use alloc::string::String;

use rustos_user_abi::syscall::VFS_IPC_PATH_CAPACITY;

use crate::EINVAL;

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
    if normalized.len() > VFS_IPC_PATH_CAPACITY {
        return Err(EINVAL);
    }
    Ok(normalized)
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

/// Key under which path-addressed storage caches file their entries. The FAT
/// engine matches names ASCII-case-insensitively, so every case spelling of one
/// path must reach one entry instead of caching the same bytes, listing or
/// sealed executable image once per spelling.
pub fn fat_cache_key(path: &str) -> String {
    path.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_spellings_of_one_fat_path_share_inode_and_cache_key() {
        assert_eq!(path_inode(b"/BIN/Ls"), path_inode(b"/bin/ls"));
        assert_eq!(fat_cache_key("/BIN/Ls"), fat_cache_key("/bin/ls"));
        assert_ne!(path_inode(b"/bin/ls"), path_inode(b"/bin/lt"));
        assert_ne!(path_inode(b""), 0);
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
        assert_eq!(normalize_absolute_path("/", &long), Err(EINVAL));
    }
}
