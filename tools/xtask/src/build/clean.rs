//! Reclaiming build and formal scratch from the tree.
//!
//! Split out of `build/mod.rs` to keep that module within its registered
//! `formal/rust-large-files.tsv` budget. Nothing here participates in a build;
//! it only deletes working state that a lane rebuilds on demand.

use fs_err as fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::Result;
use crate::config::Config;
use crate::util::{remove_dir_if_exists, remove_file_if_exists, run_command};

/// Formal scratch that any lane rebuilds on demand, relative to the repo root.
///
/// Deliberately none of these is evidence. A lane's logs, summaries, detached
/// signatures, and proof index stay where the sealing code expects to find
/// them; what is listed here is compiler and tool working state that happens
/// to sit in the same tree and outweighs the evidence by three orders of
/// magnitude.
const FORMAL_SCRATCH: &[&str] = &[
    "build/formal/abi-differential/wine-prefix",
    "build/formal/shuttle/target",
    "formal/loom-proof-kernel/target",
    "formal/shuttle-proof-kernel/target",
    "formal/fuzz/target",
];

/// The mutation lane shards one cargo target tree per worker, so their names
/// are only known by prefix.
const FORMAL_MUTATION_SHARD_DIR: &str = "build/formal/implementation-mutations";

fn path_size(path: &Path) -> Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += path_size(&entry?.path())?;
    }
    Ok(total)
}

fn modified_age(path: &Path) -> Result<Duration> {
    let modified = fs::symlink_metadata(path)?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO))
}

/// Remove `path`, or measure it when `dry_run`, returning the bytes involved.
fn reclaim(path: &Path, dry_run: bool) -> Result<u64> {
    let size = path_size(path)?;
    if size == 0 && !path.exists() {
        return Ok(0);
    }
    if !dry_run {
        if fs::symlink_metadata(path)?.is_dir() {
            remove_dir_if_exists(path)?;
        } else {
            remove_file_if_exists(path)?;
        }
    }
    Ok(size)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Collect the `incremental` and `deps` directories anywhere under the target
/// tree. Profiles sit at the top level for the host target and one level
/// deeper under an explicit target triple, so neither depth can be assumed.
fn artifact_dirs(root: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        match path.file_name().and_then(|name| name.to_str()) {
            Some("incremental" | "deps") => found.push(path),
            // `incremental` and `deps` are leaves for this purpose; their
            // contents are the units being aged, not more directories to scan.
            _ => artifact_dirs(&path, found)?,
        }
    }
    Ok(())
}

fn clean_stale(config: &Config, days: u32, dry_run: bool) -> Result<u64> {
    let cutoff = Duration::from_secs(u64::from(days) * 24 * 60 * 60);
    let mut dirs = Vec::new();
    artifact_dirs(&config.cargo_target_dir, &mut dirs)?;

    let mut reclaimed = 0;
    let mut entries = 0;
    for dir in dirs {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            // A build writes its outputs and leaves them; anything Cargo would
            // still consult has been touched by the last build that consulted
            // it. Beyond the cutoff the fingerprint has moved and the artifact
            // is unreachable.
            if modified_age(&path)? <= cutoff {
                continue;
            }
            reclaimed += reclaim(&path, dry_run)?;
            entries += 1;
        }
    }
    println!(
        "clean tier=stale days={days} entries={entries} reclaimed={} dry_run={dry_run}",
        human_bytes(reclaimed)
    );
    Ok(reclaimed)
}

fn clean_scratch(config: &Config, dry_run: bool) -> Result<u64> {
    let mut reclaimed = 0;
    let mut entries = 0;
    for relative in FORMAL_SCRATCH {
        let path = config.root_dir.join(relative);
        let size = reclaim(&path, dry_run)?;
        if size > 0 {
            reclaimed += size;
            entries += 1;
        }
    }

    let shards = config.root_dir.join(FORMAL_MUTATION_SHARD_DIR);
    if shards.is_dir() {
        for entry in fs::read_dir(&shards)? {
            let path = entry?.path();
            let is_shard_target = path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == "target" || name.starts_with("target-"));
            if !is_shard_target {
                continue;
            }
            reclaimed += reclaim(&path, dry_run)?;
            entries += 1;
        }
    }

    println!(
        "clean tier=scratch entries={entries} reclaimed={} dry_run={dry_run}",
        human_bytes(reclaimed)
    );
    Ok(reclaimed)
}

fn clean_all(config: &Config, dry_run: bool) -> Result<u64> {
    let reclaimed = path_size(&config.cargo_target_dir)?
        + path_size(&config.build_dir)?
        + path_size(&config.logs_dir)?;
    if dry_run {
        println!(
            "clean tier=all reclaimed={} dry_run=true",
            human_bytes(reclaimed)
        );
        return Ok(reclaimed);
    }

    let mut clean_target = Command::new(&config.cargo);
    clean_target
        .arg("clean")
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    run_command(&mut clean_target)?;

    let mut clean_manifest = Command::new(&config.cargo);
    clean_manifest
        .arg("clean")
        .arg("--manifest-path")
        .arg(&config.workspace_manifest)
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    run_command(&mut clean_manifest)?;
    remove_dir_if_exists(&config.build_dir)?;
    remove_dir_if_exists(&config.logs_dir)?;
    println!(
        "clean tier=all reclaimed={} dry_run=false",
        human_bytes(reclaimed)
    );
    Ok(reclaimed)
}

pub(crate) fn clean(
    config: &Config,
    stale: Option<u32>,
    scratch: bool,
    dry_run: bool,
) -> Result<()> {
    // No tier selected keeps the original meaning of a bare `clean`: wipe
    // everything. Selecting any tier means the caller asked for that tier and
    // nothing else, so the full wipe must not also run.
    if stale.is_none() && !scratch {
        clean_all(config, dry_run)?;
        return Ok(());
    }

    let mut reclaimed = 0;
    if let Some(days) = stale {
        reclaimed += clean_stale(config, days, dry_run)?;
    }
    if scratch {
        reclaimed += clean_scratch(config, dry_run)?;
    }
    println!(
        "clean total reclaimed={} dry_run={dry_run}",
        human_bytes(reclaimed)
    );
    Ok(())
}
