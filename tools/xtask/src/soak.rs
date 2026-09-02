//! Repeated-boot reproduction lane for defects that are rare per run.
//!
//! - **Owner:** this lane owns only repetition and the failure summary; `bench`
//!   owns booting, parsing, and every measurement claim it prints.
//! - **Boundary:** a guest log is untrusted text; a panic is reported as the
//!   exact line the guest wrote, never paraphrased.
//! - **Lifecycle:** run the ordinary bench lane N times, keep going past a
//!   failure, then name every failed run and why.
//! - **Failure:** the lane fails when any run failed. A run that never booted
//!   and a run that panicked are both failures and are distinguished by
//!   whether its archived log names a panic.
//! - **Forbidden:** no measurement claim may be derived here. An SMP defect
//!   that needs twenty boots to appear is a reproduction question, and the
//!   per-run tables `bench` prints remain the only measurement surface.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;

/// One run's outcome, as much as the archived log can say.
struct RunOutcome {
    index: usize,
    failure: Option<String>,
}

/// Repeats the bench lane and reports which runs failed.
///
/// A defect that appears in roughly one boot of ten cannot be confirmed fixed
/// by one boot, and hand-rolling this loop loses the failing run's log to the
/// next run's truncation. `bench` archives every run, so this only has to keep
/// going and then say which archive to read.
pub(crate) fn soak(
    config: &Config,
    runs: usize,
    rustos_vcpus: u8,
    isolate_probe: Option<&str>,
) -> Result<()> {
    let history = config.build_dir.join("kvm/debugcon-history");
    let mut outcomes = Vec::with_capacity(runs);
    for index in 1..=runs {
        let before = newest_archive(&history);
        let result = crate::bench::bench(config, None, None, rustos_vcpus, isolate_probe);
        let failure = result.err().map(|error| {
            let archived = newest_archive(&history).filter(|path| Some(path) != before.as_ref());
            match archived.as_deref().and_then(guest_panic) {
                Some(panic) => format!("{panic} [{}]", display_path(archived.as_deref())),
                None => format!("{error:#} [{}]", display_path(archived.as_deref())),
            }
        });
        if let Some(reason) = failure.as_deref() {
            eprintln!("xtask: soak run {index}/{runs} failed: {reason}");
        }
        outcomes.push(RunOutcome { index, failure });
    }

    let failed: Vec<&RunOutcome> = outcomes
        .iter()
        .filter(|outcome| outcome.failure.is_some())
        .collect();
    println!(
        "xtask: soak complete runs={runs} failed={} vcpus={rustos_vcpus}",
        failed.len()
    );
    for outcome in &failed {
        println!(
            "  run {}: {}",
            outcome.index,
            outcome.failure.as_deref().unwrap_or("unknown")
        );
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} of {runs} soak runs failed", failed.len())
    }
}

fn display_path(path: Option<&Path>) -> String {
    path.map_or_else(
        || "no archived log".to_owned(),
        |path| path.display().to_string(),
    )
}

/// The most recently archived run log, which sorts last because the archive
/// name starts with the millisecond stamp it was written at.
pub(crate) fn newest_archive(history: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(history)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect();
    entries.sort();
    entries.pop()
}

/// The guest's own panic line, verbatim, when the archived log holds one.
pub(crate) fn guest_panic(archive: &Path) -> Option<String> {
    let log = fs::read_to_string(archive).ok()?;
    let mut location = None;
    for line in log.lines() {
        if let Some(rest) = line.split_once("location: ").map(|(_, rest)| rest) {
            location = Some(rest.trim().to_owned());
        }
        if let Some((_, message)) = line.split_once("message: ") {
            return Some(match location {
                Some(at) => format!("{at}: {}", message.trim()),
                None => message.trim().to_owned(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    /// The lane's whole reason to exist is naming why a rare run failed, so
    /// the extraction is pinned against the exact debugcon shape a guest panic
    /// actually wrote -- the one this lane was built to chase.
    #[test]
    fn a_guest_panic_is_reported_with_its_location_and_message() {
        let mut archive = tempfile::NamedTempFile::new().expect("archive");
        writeln!(
            archive,
            "seq=1 msg=\"ordinary boot line\"\n\
             !PANIC-SITE:00000fdd:kernel/ps/src/multitask/scheduler.rs\n\
             location: kernel/ps/src/multitask/scheduler.rs:4061:9\n\
             message: scheduler fallback task lost local rq custody slot=48 cpu=3\n\
             [NESTED PANIC]"
        )
        .expect("write archive");
        assert_eq!(
            super::guest_panic(archive.path()).as_deref(),
            Some(
                "kernel/ps/src/multitask/scheduler.rs:4061:9: scheduler fallback task lost local rq custody slot=48 cpu=3"
            )
        );
    }

    /// A run that never booted has no panic to name, and must not be reported
    /// as one; the launch error is the honest answer there.
    #[test]
    fn a_clean_log_reports_no_guest_panic() {
        let mut archive = tempfile::NamedTempFile::new().expect("archive");
        writeln!(archive, "seq=1 msg=\"ipcbench: end\"").expect("write archive");
        assert_eq!(super::guest_panic(archive.path()), None);
    }
}
