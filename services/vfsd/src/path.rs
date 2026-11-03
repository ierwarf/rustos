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

#[cfg(test)]
mod tests {
    use super::*;

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
