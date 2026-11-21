use std::collections::BTreeMap;

use rustos_user_abi::desktop_session::{
    launch_credentials, RuntimeDir, DESKTOP_SESSION_UID, WAYLAND_SOCKET_NAME,
};

use super::{
    ensure_dirs_with, ensure_owned_private_dir, probe_writable, resolve_launch_env, LaunchHost,
};
use crate::spawn::build_exec_env_with_defaults as build;

/// Scripted host: records every call and fails the directories it is told to.
#[derive(Default)]
struct FakeHost {
    uid: u32,
    fail: BTreeMap<String, i32>,
    calls: Vec<String>,
    logs: Vec<String>,
}

impl FakeHost {
    fn for_uid(uid: u32) -> Self {
        Self { uid, ..Self::default() }
    }

    fn failing(mut self, dir: &str, errno: i32) -> Self {
        self.fail.insert(dir.to_owned(), errno);
        self
    }

    fn call(&mut self, kind: &str, dir: &str) -> std::io::Result<()> {
        self.calls.push(format!("{kind}:{dir}"));
        match self.fail.get(dir) {
            Some(errno) => Err(std::io::Error::from_raw_os_error(*errno)),
            None => Ok(()),
        }
    }
}

impl LaunchHost for FakeHost {
    fn uid(&self) -> u32 {
        self.uid
    }
    fn ensure_persistent_dir(&mut self, dir: &str) -> std::io::Result<()> {
        self.call("persistent", dir)
    }
    fn ensure_private_dir(&mut self, dir: &str) -> std::io::Result<()> {
        self.call("private", dir)
    }
    fn ensure_session_dir(&mut self, dir: &str) -> std::io::Result<()> {
        self.call("session", dir)
    }
    fn ensure_temp_dir(&mut self, dir: &str) -> std::io::Result<()> {
        self.call("temp", dir)
    }
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_owned());
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// The shape of the staged `runtime` scope: templates only.
fn staged_defaults() -> Vec<String> {
    strings(&[
        "PATH=/bin:/usr/bin:/usr/local/bin",
        "HOME={profile_root}/home",
        "XDG_CONFIG_HOME={profile_root}/config",
        "XDG_CACHE_HOME={profile_root}/cache",
        "XDG_RUNTIME_DIR={runtime_dir}",
        "TMPDIR={temp_dir}",
        "LANG=C.UTF-8",
        "WAYLAND_DISPLAY={wayland_display}",
        "XDG_SESSION_TYPE=wayland",
        "XDG_CURRENT_DESKTOP=RustOS",
    ])
}

fn values(env: Vec<std::ffi::CString>) -> Vec<String> {
    env.into_iter().map(|item| item.into_string().unwrap()).collect()
}

#[test]
fn persistent_profile_is_kept_when_the_data_volume_works() {
    let mut host = FakeHost::for_uid(1000);
    let env = values(build(&[], &staged_defaults(), &mut host).unwrap());
    assert!(env.contains(&String::from("HOME=/data/home")));
    assert!(env.contains(&String::from("XDG_CONFIG_HOME=/data/config")));
    assert!(env.contains(&String::from("XDG_CACHE_HOME=/data/cache")));
    assert!(env.contains(&String::from("XDG_RUNTIME_DIR=/run/user/1000")));
    assert!(env.contains(&String::from("TMPDIR=/tmp")));
    assert!(env.contains(&String::from("WAYLAND_DISPLAY=wayland-0")));
    assert!(env.contains(&String::from("LANG=C.UTF-8")));
    assert_eq!(
        host.calls,
        [
            "persistent:/data/home",
            "persistent:/data/config",
            "persistent:/data/cache",
            "session:/run/user/1000",
            "temp:/tmp",
        ]
    );
    assert_eq!(
        host.logs,
        ["runtimed: profile root=/data reason=persistent-volume-writable uid=1000"]
    );
}

#[test]
fn unusable_data_volume_moves_the_whole_profile_to_a_verified_tmpfs_root() {
    let mut host = FakeHost::for_uid(1000)
        .failing("/data/home", libc::EROFS)
        .failing("/data/config", libc::EROFS)
        .failing("/data/cache", libc::EROFS);
    let env = values(build(&[], &staged_defaults(), &mut host).unwrap());
    for expected in [
        "HOME=/tmp/rustos-profile-1000/home",
        "XDG_CONFIG_HOME=/tmp/rustos-profile-1000/config",
        "XDG_CACHE_HOME=/tmp/rustos-profile-1000/cache",
    ] {
        assert!(env.contains(&String::from(expected)), "{expected}");
    }
    assert!(!env.iter().any(|item| item.contains("/data")), "{env:?}");
    let private: Vec<_> = host.calls.iter().filter(|c| c.starts_with("private:")).collect();
    assert_eq!(
        private,
        [
            "private:/tmp/rustos-profile-1000",
            "private:/tmp/rustos-profile-1000/home",
            "private:/tmp/rustos-profile-1000/config",
            "private:/tmp/rustos-profile-1000/cache",
        ]
    );
    assert_eq!(
        host.logs,
        [
            "runtimed: profile root=/tmp/rustos-profile-1000 reason=persistent-unusable(\
             /data/home:errno=30,/data/config:errno=30,/data/cache:errno=30) uid=1000"
        ]
    );
}

#[test]
fn one_unusable_persistent_directory_moves_the_whole_profile() {
    // A split profile (some keys on /data, some on /tmp) would make HOME and
    // XDG_CACHE_HOME disagree about durability; the root is chosen once.
    let mut host = FakeHost::for_uid(1000).failing("/data/cache", libc::ENOSPC);
    let env = values(build(&[], &staged_defaults(), &mut host).unwrap());
    assert!(env.contains(&String::from("HOME=/tmp/rustos-profile-1000/home")));
    assert!(env.contains(&String::from("XDG_CACHE_HOME=/tmp/rustos-profile-1000/cache")));
}

#[test]
fn no_usable_root_fails_the_launch_with_the_real_errno() {
    let mut host = FakeHost::for_uid(1000)
        .failing("/data/home", libc::ENOENT)
        .failing("/data/config", libc::ENOENT)
        .failing("/data/cache", libc::ENOENT)
        .failing("/tmp/rustos-profile-1000", libc::EROFS);
    assert_eq!(build(&[], &staged_defaults(), &mut host).unwrap_err(), libc::EROFS);
    assert!(host.logs[0].contains("profile root unavailable"), "{:?}", host.logs);
    assert!(host.logs[0].contains("fallback=/tmp/rustos-profile-1000 errno=30"));
    assert!(
        !host.calls.iter().any(|c| c.starts_with("session:") || c.starts_with("temp:")),
        "nothing else is prepared once the launch is doomed"
    );

    // A fallback leaf failing after its root succeeded is equally fatal.
    let mut host = FakeHost::for_uid(1000)
        .failing("/data/home", libc::ENODEV)
        .failing("/tmp/rustos-profile-1000/config", libc::ENOSPC);
    assert_eq!(build(&[], &staged_defaults(), &mut host).unwrap_err(), libc::ENOSPC);
}

#[test]
fn runtime_dir_and_socket_derive_from_the_launch_credentials() {
    for logical_admin in [false, true] {
        let (uid, _) = launch_credentials(logical_admin);
        let mut host = FakeHost::for_uid(uid);
        let env = values(build(&[], &staged_defaults(), &mut host).unwrap());
        assert!(env.contains(&format!("XDG_RUNTIME_DIR={}", RuntimeDir(uid))), "{env:?}");
        assert!(host.calls.contains(&format!("session:{}", RuntimeDir(uid))));
        assert!(env.contains(&format!("WAYLAND_DISPLAY={WAYLAND_SOCKET_NAME}")));
    }
}

#[test]
fn runtime_dir_failure_fails_the_launch() {
    let mut host = FakeHost::for_uid(1000).failing("/run/user/1000", libc::EACCES);
    assert_eq!(build(&[], &staged_defaults(), &mut host).unwrap_err(), libc::EACCES);
    assert!(host.logs.iter().any(|line| line.contains("runtime dir unavailable")));
}

#[test]
fn compositor_clients_get_the_directory_uiserver_binds_in() {
    // uiserver runs as the desktop identity (it derives `/run/user/<own uid>`);
    // graphical apps are non-admin launches resolved here to the same value.
    assert!(
        include_str!("../../uiserver/RUSTOS.package.toml").contains("logical_admin = false"),
        "uiserver and its clients must share the desktop identity"
    );
    let (uid, _) = launch_credentials(false);
    assert_eq!(uid, DESKTOP_SESSION_UID);
    let mut host = FakeHost::for_uid(uid);
    let env = values(build(&[], &staged_defaults(), &mut host).unwrap());
    let runtime_dir = RuntimeDir(uid).to_string();
    assert!(env.contains(&format!("XDG_RUNTIME_DIR={runtime_dir}")));
    assert!(env.contains(&format!("WAYLAND_DISPLAY={WAYLAND_SOCKET_NAME}")));
}

#[test]
fn entry_values_win_keep_order_and_shadowed_defaults_establish_nothing() {
    let mut host = FakeHost::for_uid(1000);
    let entry = strings(&["PATH=/custom/bin", "HOME={runtime_dir}", "XDG_CONFIG_HOME=/elsewhere/c",
        "XDG_CACHE_HOME=/elsewhere/k"]);
    let env = values(build(&entry, &staged_defaults(), &mut host).unwrap());
    assert_eq!(
        &env[..4],
        [
            "PATH=/custom/bin",
            "HOME=/run/user/1000",
            "XDG_CONFIG_HOME=/elsewhere/c",
            "XDG_CACHE_HOME=/elsewhere/k"
        ]
    );
    assert!(!env.contains(&String::from("PATH=/bin:/usr/bin:/usr/local/bin")));
    // Every profile-rooted default was shadowed, so no root was chosen, and
    // the explicit literal directories were ensured instead.
    assert!(host.logs.is_empty(), "{:?}", host.logs);
    assert!(!host.calls.contains(&String::from("persistent:/data/home")));
    assert!(host.calls.contains(&String::from("persistent:/elsewhere/c")));
    assert!(host.calls.contains(&String::from("persistent:/elsewhere/k")));
    // No duplicate keys survive the merge.
    let mut keys: Vec<_> = env.iter().map(|item| item.split('=').next().unwrap()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), env.len());
}

#[test]
fn explicit_literal_profile_directory_that_cannot_exist_fails_the_launch() {
    let mut host = FakeHost::for_uid(1000).failing("/elsewhere", libc::EROFS);
    let entry = strings(&["HOME=/elsewhere"]);
    assert_eq!(build(&entry, &staged_defaults(), &mut host).unwrap_err(), libc::EROFS);
    assert!(host.logs.iter().any(|line| line.contains("launch dir unavailable path=/elsewhere")));
}

#[test]
fn unknown_placeholders_are_hard_errors_in_defaults_and_entries() {
    let mut host = FakeHost::for_uid(1000);
    assert_eq!(
        build(&[], &strings(&["XDG_RUNTIME_DIR=/run/user/{uid}"]), &mut host).unwrap_err(),
        libc::EINVAL
    );
    assert_eq!(
        build(&strings(&["HOME={nope}"]), &[], &mut host).unwrap_err(),
        libc::EINVAL
    );
    assert_eq!(build(&strings(&["A=b}"]), &[], &mut host).unwrap_err(), libc::EINVAL);
    assert!(host.calls.is_empty(), "nothing is created for a rejected environment");
}

#[test]
fn empty_entry_gets_every_default_and_literal_only_env_touches_nothing() {
    let mut host = FakeHost::for_uid(1000);
    let env = values(build(&[], &strings(&["LANG=C.UTF-8", "PATH=/bin"]), &mut host).unwrap());
    assert_eq!(env, ["LANG=C.UTF-8", "PATH=/bin"]);
    assert!(host.calls.is_empty() && host.logs.is_empty());
}

#[test]
fn resolver_never_expands_profile_root_without_a_selected_root() {
    // `resolve_launch_env` derives the root from the templates themselves, so a
    // profile-rooted value always selects (and ensures) a root first.
    let mut host = FakeHost::for_uid(7);
    let env = resolve_launch_env(&["CACHE={profile_root}/a/b"], 0, &mut host).unwrap();
    assert_eq!(env[0].to_str().unwrap(), "CACHE=/data/a/b");
    assert_eq!(host.calls, ["persistent:/data/a/b"]);
}

#[test]
fn nested_fallback_suffix_verifies_every_component() {
    let mut host = FakeHost::for_uid(7).failing("/data/a/b", libc::EROFS);
    let env = resolve_launch_env(&["CACHE={profile_root}/a/b"], 0, &mut host).unwrap();
    assert_eq!(env[0].to_str().unwrap(), "CACHE=/tmp/rustos-profile-7/a/b");
    let private: Vec<_> = host.calls.iter().filter(|c| c.starts_with("private:")).collect();
    assert_eq!(
        private,
        [
            "private:/tmp/rustos-profile-7",
            "private:/tmp/rustos-profile-7/a",
            "private:/tmp/rustos-profile-7/a/b",
        ]
    );
}

#[test]
fn ensure_dirs_reports_each_failure_and_keeps_going() {
    let mut seen = Vec::new();
    let failures = ensure_dirs_with(&["/data/home", "/data/config", "/data/cache"], |dir: &str| {
        seen.push(dir.to_owned());
        if dir == "/data/config" {
            Err(std::io::Error::from_raw_os_error(libc::ENOENT))
        } else {
            Ok(())
        }
    });
    assert_eq!(seen.len(), 3);
    assert_eq!(failures, [(String::from("/data/config"), libc::ENOENT)]);
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("runtimed-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn own_identity() -> (u32, u32) {
    // SAFETY: both calls only read the process's own identity.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

#[test]
fn private_dir_is_created_owned_verified_and_idempotent() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch_dir("private");
    let dir = root.join("profile");
    let path = dir.to_str().unwrap();
    let (uid, gid) = own_identity();
    ensure_owned_private_dir(path, uid, gid).expect("created");
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    ensure_owned_private_dir(path, uid, gid).expect("existing own directory is accepted");
    probe_writable(path).expect("writable");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "probe file is removed");

    // Another identity is never accepted for an existing entry.
    assert_eq!(
        ensure_owned_private_dir(path, uid.wrapping_add(1), gid).unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    // A directory other users can write is not private.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        ensure_owned_private_dir(path, uid, gid).unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn private_dir_rejects_symlinks_and_missing_parents() {
    let root = scratch_dir("private-link");
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let (uid, gid) = own_identity();
    assert_eq!(
        ensure_owned_private_dir(link.to_str().unwrap(), uid, gid).unwrap_err().raw_os_error(),
        Some(libc::ENOTDIR)
    );
    let orphan = root.join("missing").join("leaf");
    assert_eq!(
        ensure_owned_private_dir(orphan.to_str().unwrap(), uid, gid).unwrap_err().raw_os_error(),
        Some(libc::ENOENT)
    );
    assert!(!root.join("missing").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn writable_probe_needs_a_real_directory() {
    let root = scratch_dir("probe");
    assert!(probe_writable(root.to_str().unwrap()).is_ok());
    let leftover = root.join(format!(".rustos-launch-probe-{}", std::process::id()));
    std::fs::write(&leftover, b"stale").unwrap();
    assert!(probe_writable(root.to_str().unwrap()).is_ok());
    assert!(probe_writable(root.join("absent").to_str().unwrap()).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
