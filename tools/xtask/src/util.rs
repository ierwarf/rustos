use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use fs_err as fs;
use walkdir::WalkDir;

use crate::Result;

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

pub(crate) fn path_label(path: &Path) -> String {
    path.display().to_string()
}

pub(crate) fn copy_with_parent(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst
        .parent()
        .with_context(|| format!("destination has no parent: {}", dst.display()))?;
    fs::create_dir_all(parent)?;
    fs::copy(src, dst)?;
    Ok(())
}

pub(crate) fn output_is_fresh(output: &Path, inputs: &[PathBuf]) -> Result<bool> {
    let output_time = match fs::metadata(output) {
        Ok(metadata) if metadata.is_file() => metadata.modified()?,
        Ok(_) => return Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    for input in inputs {
        let input_time = fs::metadata(input)?.modified()?;
        if input_time > output_time {
            return Ok(false);
        }
    }

    Ok(true)
}

pub(crate) fn outputs_are_fresh(outputs: &[PathBuf], inputs: &[PathBuf]) -> Result<bool> {
    for output in outputs {
        if !output_is_fresh(output, inputs)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn copy_tree_files(src_root: &Path, dst_root: &Path) -> Result<()> {
    if !src_root.is_dir() {
        return Ok(());
    }

    let mut files = WalkDir::new(src_root)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    files.sort_by_key(|entry| entry.path().to_owned());
    for entry in files {
        if entry.file_type().is_file() {
            let relative = entry.path().strip_prefix(src_root)?;
            copy_with_parent(entry.path(), &dst_root.join(relative))?;
        }
    }

    Ok(())
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
        bail!("missing AMDGPU firmware blob: {}(.zst)", src_bin.display());
    }

    let parent = dst
        .parent()
        .with_context(|| format!("firmware destination has no parent: {}", dst.display()))?;
    fs::create_dir_all(parent)?;

    let unpacker = command_in_path("zstd")
        .map(|_| OsString::from("zstd"))
        .or_else(|| command_in_path("zstdcat").map(|_| OsString::from("zstdcat")))
        .with_context(|| format!("missing zstd/zstdcat to unpack {}", src_zst.display()))?;

    let status = if unpacker == OsStr::new("zstd") {
        Command::new(&unpacker)
            .arg("-dc")
            .arg(&src_zst)
            .stdout(File::create(dst)?)
            .status()?
    } else {
        Command::new(&unpacker)
            .arg(&src_zst)
            .stdout(File::create(dst)?)
            .status()?
    };

    if !status.success() {
        bail!("failed to unpack {}", src_zst.display());
    }

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

pub(crate) fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    Ok(tempfile::Builder::new().prefix(prefix).tempdir()?.keep())
}

pub(crate) fn run_command(command: &mut Command) -> Result<()> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        eprint!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    bail!(
        "command failed with status {}: {:?}",
        output.status,
        command
    )
}
