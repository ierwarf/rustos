//! Per-launch environment resolution: the session profile.
//!
//! - **Owner:** `runtimed` decides every runtime fact a launch environment
//!   names. The staged `runtime-env.tsv` and catalog `env` carry only templates
//!   (`session-profile` grammar); the image builder never embeds a uid, a
//!   profile path or a volume assumption.
//! - **Facts resolved here:** the uid/gid the launch runs as
//!   (`launch_credentials`), the profile root (persistent `/data` volume when it
//!   is mounted and writable, else a verified per-uid directory on the TmpFs),
//!   the per-uid runtime directory and the Wayland socket name shared with
//!   uiserver through `rustos_user_abi::desktop_session`.
//! - **Directories:** every directory a resolved value names (profile
//!   directories, runtime dir) is created or verified before the environment is
//!   returned. A directory that could not be created fails the launch with the
//!   real errno instead of starting the app with a path that cannot exist.
//! - **Forbidden:** passing an unresolved placeholder through, reporting
//!   success for a directory that was not created, silently keeping a dead
//!   `HOME`.

use std::ffi::CString;

use rustos_user_abi::desktop_session::{
    launch_credentials, RuntimeDir, PERSISTENT_DATA_DIR, TEMP_DIR, WAYLAND_SOCKET_NAME,
};
use session_profile::{Placeholder, Template};

/// Keys whose explicit literal absolute directory runtimed also ensures.
const PROFILE_DIR_KEYS: [&str; 3] = ["HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"];

/// Everything launch resolution does to the outside world, so policy is
/// testable without a filesystem.
pub(super) trait LaunchHost {
    fn uid(&self) -> u32;
    /// Persistent volume directory: `mkdir -p`, then proven writable.
    fn ensure_persistent_dir(&mut self, dir: &str) -> std::io::Result<()>;
    /// TmpFs directory created 0700 for the launch identity (parent must
    /// exist), or an existing one verified as owned and private, then proven
    /// writable.
    fn ensure_private_dir(&mut self, dir: &str) -> std::io::Result<()>;
    /// The launch uid's runtime directory: created or verified as owned and
    /// private (writability is the owning identity's mode bits).
    fn ensure_session_dir(&mut self, dir: &str) -> std::io::Result<()>;
    /// The shared temporary directory must exist as a directory.
    fn ensure_temp_dir(&mut self, dir: &str) -> std::io::Result<()>;
    /// Ensure a session D-Bus listener exists inside `dir` for the launch identity.
    fn ensure_session_bus(&mut self, _dir: &str) -> std::io::Result<()> {
        Ok(())
    }
    fn log(&mut self, line: &str);
}

fn errno_of(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

/// `mkdir -p` for each directory through `create`; every failure is returned
/// with its path and the remaining directories are still attempted.
pub(super) fn ensure_dirs_with(
    dirs: &[&str],
    mut create: impl FnMut(&str) -> std::io::Result<()>,
) -> Vec<(String, i32)> {
    let mut failures = Vec::new();
    for dir in dirs {
        if let Err(error) = create(dir) {
            failures.push(((*dir).to_owned(), errno_of(&error)));
        }
    }
    failures
}

/// Chooses the profile root and makes the directories `suffixes` name under it
/// usable. The persistent root is kept whenever every named directory works
/// there; otherwise the whole profile moves to `<tmp>/rustos-profile-<uid>`.
/// The decision and its reason are logged once. When not even the fallback can
/// be prepared the real errno is returned.
fn select_profile_root(host: &mut impl LaunchHost, suffixes: &[&str]) -> Result<String, i32> {
    let uid = host.uid();
    let mut wanted: Vec<String> = Vec::new();
    for suffix in suffixes {
        let dir = format!("{PERSISTENT_DATA_DIR}{suffix}");
        if !wanted.contains(&dir) {
            wanted.push(dir);
        }
    }
    let refs: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let failures = ensure_dirs_with(&refs, |dir| host.ensure_persistent_dir(dir));
    if failures.is_empty() {
        host.log(&format!(
            "runtimed: profile root={PERSISTENT_DATA_DIR} reason=persistent-volume-writable uid={uid}"
        ));
        return Ok(String::from(PERSISTENT_DATA_DIR));
    }
    let root = format!("{TEMP_DIR}/rustos-profile-{uid}");
    let reasons = failures
        .iter()
        .map(|(dir, errno)| format!("{dir}:errno={errno}"))
        .collect::<Vec<_>>()
        .join(",");
    let prepared = host.ensure_private_dir(&root).map_err(|e| (root.clone(), e)).and_then(|()| {
        for suffix in suffixes {
            // Every component below the root, so a nested suffix still has
            // every parent verified (never `mkdir -p` through an unchecked path).
            let mut dir = root.clone();
            for part in suffix.split('/').filter(|part| !part.is_empty()) {
                dir = format!("{dir}/{part}");
                host.ensure_private_dir(&dir).map_err(|e| (dir.clone(), e))?;
            }
        }
        Ok(())
    });
    match prepared {
        Ok(()) => {
            host.log(&format!(
                "runtimed: profile root={root} reason=persistent-unusable({reasons}) uid={uid}"
            ));
            Ok(root)
        }
        Err((path, error)) => {
            let errno = errno_of(&error);
            host.log(&format!(
                "runtimed: profile root unavailable persistent=({reasons}) fallback={path} errno={errno} uid={uid}"
            ));
            Err(errno)
        }
    }
}

/// Resolves the merged launch environment. `items` is `KEY=template` in launch
/// order; the first `explicit` entries came from the catalog entry (their value
/// won over the registry default). Every template is parsed first (an unknown
/// placeholder is `EINVAL`), then only the facts the surviving values actually
/// name are established, their directories are ensured, and the values expand.
pub(super) fn resolve_launch_env(
    items: &[&str],
    explicit: usize,
    host: &mut impl LaunchHost,
) -> Result<Vec<CString>, i32> {
    let mut parsed: Vec<(&str, &str, Template)> = Vec::with_capacity(items.len());
    for item in items {
        let (key, value) = item.split_once('=').ok_or(libc::EINVAL)?;
        match Template::parse(value) {
            Ok(template) => parsed.push((key, value, template)),
            Err(error) => {
                host.log(&format!("runtimed: launch env {key} rejected: {error}"));
                return Err(libc::EINVAL);
            }
        }
    }
    let uid = host.uid();

    let mut suffixes: Vec<&str> = Vec::new();
    for (_, _, template) in &parsed {
        if let Some(suffix) = template.profile_suffix() {
            if !suffixes.contains(&suffix) {
                suffixes.push(suffix);
            }
        }
    }
    let profile_root = if suffixes.is_empty() {
        None
    } else {
        Some(select_profile_root(host, &suffixes)?)
    };

    let runtime_dir = RuntimeDir(uid).to_string();
    if parsed.iter().any(|(_, _, template)| template.uses(Placeholder::RuntimeDir)) {
        if let Err(error) = host.ensure_session_dir(&runtime_dir) {
            let errno = errno_of(&error);
            host.log(&format!(
                "runtimed: runtime dir unavailable path={runtime_dir} errno={errno} uid={uid}"
            ));
            return Err(errno);
        }
        // The session bus is optional for launched programs (toolkits degrade
        // when it is absent), so a bind failure is reported but never blocks
        // the launch.
        if let Err(error) = host.ensure_session_bus(&runtime_dir) {
            let errno = errno_of(&error);
            host.log(&format!(
                "runtimed: session bus unavailable path={runtime_dir}/bus errno={errno} uid={uid}"
            ));
        }
    }
    if parsed.iter().any(|(_, _, template)| template.uses(Placeholder::TempDir)) {
        if let Err(error) = host.ensure_temp_dir(TEMP_DIR) {
            let errno = errno_of(&error);
            host.log(&format!("runtimed: temp dir unavailable path={TEMP_DIR} errno={errno}"));
            return Err(errno);
        }
    }

    // An explicit literal profile directory from the catalog entry is the
    // manifest author's choice, but it must still exist before the app starts.
    for (key, value, template) in parsed.iter().take(explicit) {
        if !PROFILE_DIR_KEYS.contains(key) || !template.is_literal() || !value.starts_with('/') {
            continue;
        }
        if let Err(error) = host.ensure_persistent_dir(value) {
            let errno = errno_of(&error);
            host.log(&format!("runtimed: launch dir unavailable path={value} errno={errno}"));
            return Err(errno);
        }
    }

    let mut env = Vec::with_capacity(parsed.len());
    for (key, _, template) in &parsed {
        let value = template
            .expand(|placeholder| {
                Ok::<_, i32>(match placeholder {
                    Placeholder::ProfileRoot => profile_root.clone().ok_or(libc::EINVAL)?,
                    Placeholder::RuntimeDir => runtime_dir.clone(),
                    Placeholder::TempDir => String::from(TEMP_DIR),
                    Placeholder::WaylandDisplay => String::from(WAYLAND_SOCKET_NAME),
                })
            })?;
        env.push(CString::new(format!("{key}={value}")).map_err(|_| libc::EINVAL)?);
    }
    Ok(env)
}

/// The real filesystem, acting for one launch identity.
pub(super) struct SystemLaunchHost {
    uid: u32,
    gid: u32,
    probed_persistent_writable: bool,
}

impl SystemLaunchHost {
    pub(super) fn for_launch(logical_admin: bool) -> Self {
        let (uid, gid) = launch_credentials(logical_admin);
        Self {
            uid,
            gid,
            probed_persistent_writable: false,
        }
    }
}

impl LaunchHost for SystemLaunchHost {
    fn uid(&self) -> u32 {
        self.uid
    }

    fn ensure_persistent_dir(&mut self, dir: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        // Probe writability once per launch when all subdirectories sit on the
        // same persistent volume instead of creating+unlinking a probe file
        // for every suffix (`/data/home`, `/data/config`, `/data/cache`).
        if !self.probed_persistent_writable {
            probe_writable(dir)?;
            if dir.starts_with(PERSISTENT_DATA_DIR) {
                self.probed_persistent_writable = true;
            }
        }
        Ok(())
    }

    fn ensure_private_dir(&mut self, dir: &str) -> std::io::Result<()> {
        ensure_owned_private_dir(dir, self.uid, self.gid)?;
        probe_writable(dir)
    }

    fn ensure_session_dir(&mut self, dir: &str) -> std::io::Result<()> {
        ensure_owned_private_dir(dir, self.uid, self.gid)
    }

    fn ensure_session_bus(&mut self, dir: &str) -> std::io::Result<()> {
        super::dbus::ensure_session_bus(dir, self.uid, self.gid)
    }

    fn ensure_temp_dir(&mut self, dir: &str) -> std::io::Result<()> {
        if std::fs::metadata(dir)?.is_dir() {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(libc::ENOTDIR))
        }
    }

    fn log(&mut self, line: &str) {
        super::debug_line(line);
    }
}

/// Creates and removes a probe file in `dir`. The probe is the only check that
/// sees a read-only or full volume; removal is best effort because a leftover
/// empty marker does not change whether the directory is writable.
pub(super) fn probe_writable(dir: &str) -> std::io::Result<()> {
    let probe = format!("{dir}/.rustos-launch-probe-{}", std::process::id());
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&probe)?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Creates `dir` (one level; the parent must exist) 0700 and hands it to
/// `uid:gid` (runtimed is logical admin, so a plain `mkdir` would leave a
/// root-owned directory the app cannot use), or accepts an existing entry only
/// when it is a real directory owned by that identity that no other user can
/// write. `/tmp` and `/run/user` are shared namespaces, so an existing entry is
/// never trusted by name.
pub(super) fn ensure_owned_private_dir(dir: &str, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{chown, DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {
            let handed_over = chown(dir, Some(uid), Some(gid))
                .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)));
            if let Err(error) = handed_over {
                // Never leave a directory the launch identity cannot use behind.
                let _ = std::fs::remove_dir(dir);
                return Err(error);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    if meta.uid() != uid || meta.mode() & 0o700 != 0o700 || meta.mode() & 0o022 != 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EACCES));
    }
    Ok(())
}

#[cfg(test)]
#[path = "launch_profile_tests.rs"]
mod tests;
