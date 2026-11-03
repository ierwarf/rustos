//! Volatile `/run` directory namespace owned by vfsd.
//!
//! The boot volume is read-only (`persistent_mutation_status`), so a runtime
//! directory cannot be created in FAT. `/run` is instead a vfsd-owned,
//! bounded, memory-backed namespace: `mkdir` under it records a real
//! directory that `stat`, `getdents` and later `mkdir`/`open` resolution
//! observe, and it vanishes when vfsd restarts (like a tmpfs). Creation stays
//! admitted only for the paths `mkdir_policy` names; every other path remains
//! `EROFS` instead of a success that creates nothing.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{mkdir_policy, ENOENT};

const EEXIST: i32 = 17;
const ENOSPC: i32 = 28;

/// Hard bound on runtime directories; `mkdir_policy` admits at most
/// `/run/user` plus one directory per effective uid, so this only bounds a
/// uid-churning caller.
pub const RUN_DIRECTORIES_MAX: usize = 64;

#[derive(Debug, Default)]
pub struct RunDirectories {
    /// Absolute paths below `/run`; `/run` itself always exists and is not stored.
    created: BTreeSet<String>,
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}

impl RunDirectories {
    pub const fn new() -> Self {
        Self {
            created: BTreeSet::new(),
        }
    }

    /// Whether `path` is `/run` or a created runtime directory.
    pub fn contains(&self, path: &str) -> bool {
        path == "/run" || self.created.contains(path)
    }

    /// POSIX `mkdir` of a canonical absolute path inside the runtime tree.
    /// Returns 0 or a positive errno. A path outside the admitted set keeps
    /// the read-only answer, an existing path is `EEXIST`, and a missing
    /// parent is `ENOENT` so `create_dir_all`-style callers create parents
    /// first.
    pub fn mkdir(&mut self, path: &str, euid: u32) -> i32 {
        let admitted = mkdir_policy(path, euid);
        if admitted != 0 {
            return admitted;
        }
        if self.contains(path) {
            return EEXIST;
        }
        if !self.contains(parent_of(path)) {
            return ENOENT;
        }
        if self.created.len() >= RUN_DIRECTORIES_MAX {
            return ENOSPC;
        }
        self.created.insert(String::from(path));
        0
    }

    /// Names of the runtime directories directly inside `dir`.
    pub fn children(&self, dir: &str) -> Vec<&str> {
        self.created
            .iter()
            .filter(|path| parent_of(path) == dir)
            .map(|path| &path[dir.len() + usize::from(dir != "/")..])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EROFS;

    #[test]
    fn mkdir_really_creates_and_is_observable() {
        let mut dirs = RunDirectories::new();
        assert!(!dirs.contains("/run/user"));
        assert_eq!(dirs.mkdir("/run/user", 1000), 0);
        assert!(dirs.contains("/run/user"));
        assert_eq!(dirs.mkdir("/run/user", 1000), EEXIST);
        assert_eq!(dirs.mkdir("/run/user/1000", 1000), 0);
        assert!(dirs.contains("/run/user/1000"));
        assert_eq!(dirs.children("/run"), ["user"]);
        assert_eq!(dirs.children("/run/user"), ["1000"]);
        assert!(dirs.children("/run/user/1000").is_empty());
    }

    #[test]
    fn missing_parent_is_enoent_not_silent_success() {
        let mut dirs = RunDirectories::new();
        assert_eq!(dirs.mkdir("/run/user/1000", 1000), ENOENT);
        assert!(!dirs.contains("/run/user/1000"));
    }

    #[test]
    fn run_itself_exists_and_foreign_paths_stay_read_only() {
        let mut dirs = RunDirectories::new();
        assert_eq!(dirs.mkdir("/run", 0), EEXIST);
        assert_eq!(dirs.mkdir("/var/lib/rustos", 0), EROFS);
        assert_eq!(dirs.mkdir("/run/other", 0), EROFS);
        assert_eq!(dirs.mkdir("/run/user/1001", 1000), EROFS);
        assert!(!dirs.contains("/run/other"));
    }

    #[test]
    fn creation_is_bounded() {
        let mut dirs = RunDirectories::new();
        assert_eq!(dirs.mkdir("/run/user", 0), 0);
        for euid in 0..(RUN_DIRECTORIES_MAX as u32 - 1) {
            let path = alloc::format!("/run/user/{euid}");
            assert_eq!(dirs.mkdir(&path, euid), 0, "{path}");
        }
        assert_eq!(dirs.mkdir("/run/user/99999", 99999), ENOSPC);
    }
}
