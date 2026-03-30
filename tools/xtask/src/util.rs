use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use xshell::{cmd, Shell};

use crate::{config::Config, Result};

pub(crate) fn default_root_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask crate must live under tools/xtask beneath the workspace root")
        .to_path_buf()
}

pub(crate) fn env_path(key: &str) -> Option<PathBuf> {
    env_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn env_os(key: &str) -> Option<OsString> {
    env::var_os(key).filter(|value| !value.is_empty())
}

pub(crate) fn env_string(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

pub(crate) fn split_whitespace_owned(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_owned).collect()
}

pub(crate) fn run_cargo_kernel_rustc(
    config: &Config,
    package: &str,
    rustc_args: &[String],
) -> Result<()> {
    let sh = shell()?;
    let cargo = &config.cargo;
    let manifest = &config.workspace_manifest;
    let zflags = &config.kernel_cargo_zflags;
    let target = &config.kernel_target;
    cmd!(
        sh,
        "{cargo} rustc --manifest-path {manifest} {zflags...} -p {package} --target {target} --release -- {rustc_args...}"
    )
    .env("CARGO_TARGET_DIR", &config.cargo_target_dir)
    .run()?;
    Ok(())
}

pub(crate) fn run_cargo_kernel_check(config: &Config, package: &str) -> Result<()> {
    let sh = shell()?;
    let cargo = &config.cargo;
    let manifest = &config.workspace_manifest;
    let zflags = &config.kernel_cargo_zflags;
    let target = &config.kernel_target;
    cmd!(
        sh,
        "{cargo} check --manifest-path {manifest} {zflags...} -p {package} --target {target}"
    )
    .env("CARGO_TARGET_DIR", &config.cargo_target_dir)
    .run()?;
    Ok(())
}

pub(crate) fn copy_with_parent(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| format!("destination has no parent: {}", dst.display()))?;
    fs::create_dir_all(parent)?;
    fs::copy(src, dst)?;
    Ok(())
}

pub(crate) fn maybe_copy_host_runtime(
    src: &Option<PathBuf>,
    dst: &Path,
    boot_entry: &str,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    if let Some(src) = src.as_ref().filter(|path| path.is_file()) {
        copy_with_parent(src, dst)?;
        push_boot_entry_unique(boot_entries, boot_entry);
    }
    Ok(())
}

pub(crate) fn maybe_copy_dual_host_runtime(
    src: &Option<PathBuf>,
    primary_dst: &Path,
    fallback_dst: &Path,
    primary_boot_entry: &str,
    fallback_boot_entry: &str,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    if let Some(src) = src.as_ref().filter(|path| path.is_file()) {
        copy_with_parent(src, primary_dst)?;
        copy_with_parent(src, fallback_dst)?;
        push_boot_entry_unique(boot_entries, primary_boot_entry);
        push_boot_entry_unique(boot_entries, fallback_boot_entry);
    }
    Ok(())
}

pub(crate) fn maybe_copy_optional_file(
    src: &Path,
    dst: &Path,
    boot_entry: &str,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    if src.is_file() {
        copy_with_parent(src, dst)?;
        push_boot_entry_unique(boot_entries, boot_entry);
    }
    Ok(())
}

pub(crate) fn push_boot_entry_unique(entries: &mut Vec<String>, entry: &str) {
    if !entries.iter().any(|existing| existing == entry) {
        entries.push(String::from(entry));
    }
}

pub(crate) fn copy_or_unpack_firmware(
    firmware_dir: &Path,
    basename: &str,
    dst: &Path,
) -> Result<()> {
    let src_bin = firmware_dir.join(basename);
    if src_bin.is_file() {
        return copy_with_parent(&src_bin, dst);
    }

    let src_zst = firmware_dir.join(format!("{basename}.zst"));
    if !src_zst.is_file() {
        return Err(format!("missing AMDGPU firmware blob: {}(.zst)", src_bin.display()).into());
    }

    let parent = dst
        .parent()
        .ok_or_else(|| format!("firmware destination has no parent: {}", dst.display()))?;
    fs::create_dir_all(parent)?;

    let unpacker = command_in_path("zstd")
        .map(|_| OsString::from("zstd"))
        .or_else(|| command_in_path("zstdcat").map(|_| OsString::from("zstdcat")))
        .ok_or_else(|| format!("missing zstd/zstdcat to unpack {}", src_zst.display()))?;

    let status = if unpacker == OsStr::new("zstd") {
        Command::new(&unpacker)
            .arg("-dc")
            .arg(&src_zst)
            .stdout(fs::File::create(dst)?)
            .status()?
    } else {
        Command::new(&unpacker)
            .arg(&src_zst)
            .stdout(fs::File::create(dst)?)
            .status()?
    };

    if !status.success() {
        return Err(format!("failed to unpack {}", src_zst.display()).into());
    }

    Ok(())
}

pub(crate) fn write_boot_file_list(path: &Path, entries: &[String]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("boot file list path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut content = String::new();
    for entry in entries {
        content.push_str(entry);
        content.push_str("\r\n");
    }
    fs::write(path, content)?;
    Ok(())
}

pub(crate) fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn compiler_print_file_name(cc: &OsStr, file_name: &str) -> Option<PathBuf> {
    let output = Command::new(cc)
        .arg(format!("-print-file-name={file_name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let candidate = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if candidate.is_empty() || candidate == file_name {
        return None;
    }

    let path = PathBuf::from(candidate);
    path.is_file().then_some(path)
}

pub(crate) fn command_in_path(name: &str) -> Option<PathBuf> {
    command_in_path_os(OsStr::new(name))
}

pub(crate) fn command_in_path_os(name: &OsStr) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn resolve_command_path(command: &OsStr) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file().then_some(path.to_path_buf());
    }
    command_in_path_os(command)
}

pub(crate) fn read_trimmed(path: impl AsRef<Path>) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

pub(crate) fn shell() -> Result<Shell> {
    Ok(Shell::new()?)
}

pub(crate) fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let base = env::temp_dir();
    for attempt in 0..64u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!(
            "{prefix}.{}.{}.{}",
            std::process::id(),
            nanos,
            attempt
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(format!(
        "failed to create temporary directory under {}",
        base.display()
    )
    .into())
}

pub(crate) fn run_command(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("command failed with status {status}: {:?}", command).into());
    }
    Ok(())
}
