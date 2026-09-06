//! One command for the local verification pipeline.
//!
//! - **Owner:** this module owns the *order* of the local gate and the exact
//!   invocation each stage needs. Every stage still belongs to the module that
//!   implements it; nothing is reimplemented here.
//! - **Boundary:** stage output is captured and shown only for the stage that
//!   failed, with a bounded diagnostic tail; a passing run is a handful of
//!   lines rather than several thousand.
//! - **Lifecycle:** stages run in dependency order and stop at the first
//!   failure, because every later stage would be describing a tree that does
//!   not build.
//! - **Failure:** a failing stage prints its first relevant diagnostic, at most
//!   120 trailing lines, and its exact command line. The complete captured
//!   output is retained at `build/verify/<stage>.log` for follow-up.
//! - **Forbidden:** no stage may be skipped silently, and the formal seal must
//!   be the last thing before a boot lane - a seal binds the source tree, so
//!   anything that edits tracked files after it invalidates it.
//! - **Evidence:** `docs/ai/commands.md`.

use std::ffi::OsStr;
use std::fs;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// Why this exists at all.
///
/// The stages below were six separate commands, each rediscovered from
/// `docs/ai/commands.md` on every session, each run with its own redirect to
/// its own log file, and each dumping its full output into the terminal on
/// success. Two mistakes came out of that arrangement often enough to be worth
/// naming:
///
/// - **Running the boot lane against a stale seal.** The seal binds the source
///   tree hash, so any edit after `verify-all.sh` invalidates it, and the
///   resulting `formal verification run binding mismatch` reads exactly like a
///   boot failure. Ordering the stages here makes that unrepresentable.
/// - **Testing packages the wrong way.** `rootd` is a freestanding binary
///   whose default `cargo test` harness segfaults; its real tests need
///   `--features host-test`. That knowledge lived nowhere and cost a
///   `cargo test --workspace` false alarm.
pub(crate) fn verify(
    config: &Config,
    gate: bool,
    repeat: usize,
    rustos_vcpus: u8,
    min_ui_fps: u32,
) -> Result<()> {
    let started = Instant::now();

    run_stage("check", || crate::build::check(config, false))?;
    run_stage("build", || crate::build::build(config, false))?;

    // The freestanding service binaries are excluded from the sweep and tested
    // through the feature that gives them a native harness. `--workspace`
    // alone builds their linker entrypoint into a libtest binary, which
    // segfaults before a single test reports.
    run_captured(
        config,
        "host-tests",
        &config.cargo,
        &["test", "--workspace", "--exclude", "rootd"],
    )?;
    run_captured(
        config,
        "host-tests-freestanding",
        &config.cargo,
        &["test", "-p", "rootd", "--features", "host-test"],
    )?;

    run_captured(config, "formal-selftest", "bash", &["formal/selftest.sh"])?;

    // Last, and deliberately last: this seals the source tree, and every stage
    // above may edit generated evidence that the seal covers.
    run_captured(
        config,
        "formal-seal",
        "bash",
        &["formal/verify-all.sh", "--profile", "pr"],
    )?;

    if gate {
        let vcpus = rustos_vcpus.to_string();
        let fps = min_ui_fps.to_string();
        let runs = repeat.to_string();
        // Through the ordinary smoke entry, so the boot lane's own auto-seal,
        // launch lock, and per-run evidence archiving all still apply.
        crate::kvm::kvm_smoke_command(
            config,
            [
                "--rustos-vcpus".to_owned(),
                vcpus,
                "--min-ui-fps".to_owned(),
                fps,
                "--dvm-network-shmem".to_owned(),
                "--timeout".to_owned(),
                "120".to_owned(),
                "--repeat".to_owned(),
                runs,
            ]
            .into_iter(),
        )
        .context("verify stage `kvm-gate` failed")?;
        println!("xtask: verify kvm-gate passed");
    }

    println!(
        "xtask: verify passed elapsed_seconds={}",
        started.elapsed().as_secs()
    );
    Ok(())
}

fn run_stage<F>(name: &str, stage: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let started = Instant::now();
    stage().with_context(|| format!("verify stage `{name}` failed"))?;
    println!(
        "xtask: verify {name} passed elapsed_seconds={}",
        started.elapsed().as_secs()
    );
    Ok(())
}

const FAILURE_TAIL_LINES: usize = 120;

/// Runs one stage as a child process, showing only bounded failure context.
fn run_captured(
    config: &Config,
    name: &str,
    program: impl AsRef<OsStr>,
    args: &[&str],
) -> Result<()> {
    let started = Instant::now();
    let program = program.as_ref();
    let output = Command::new(program)
        .args(args)
        .current_dir(&config.root_dir)
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir)
        .output()
        .with_context(|| format!("run verify stage `{name}`: {}", args.join(" ")))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let command = rerun_command(config, program, args);
        let log_dir = config.build_dir.join("verify");
        fs::create_dir_all(&log_dir)
            .with_context(|| format!("create verify log directory `{}`", log_dir.display()))?;
        let log_path = log_dir.join(format!("{}.log", stage_log_stem(name)));
        fs::write(&log_path, captured_output(&stdout, &stderr))
            .with_context(|| format!("write failed verify stage log `{}`", log_path.display()))?;

        eprintln!("xtask: verify {name} failed status={}", output.status);
        eprintln!("rerun: {command}");
        if let Some(diagnostic) =
            first_relevant_diagnostic(&stderr).or_else(|| first_relevant_diagnostic(&stdout))
        {
            eprintln!("first diagnostic: {diagnostic}");
        } else {
            eprintln!("first diagnostic: <none matched; inspect the bounded tail>");
        }
        let tail = failure_tail(&stdout, &stderr);
        eprintln!("last {} lines:", tail.len());
        for line in tail {
            eprintln!("{line}");
        }
        eprintln!("full log: {}", log_path.display());
        bail!("verify stage `{name}` failed; rerun: {command}");
    }
    println!(
        "xtask: verify {name} passed elapsed_seconds={}",
        started.elapsed().as_secs()
    );
    Ok(())
}

fn captured_output(stdout: &str, stderr: &str) -> String {
    format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
}

fn failure_tail(stdout: &str, stderr: &str) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("--- stdout ---".to_owned());
    lines.extend(stdout.lines().map(str::to_owned));
    lines.push("--- stderr ---".to_owned());
    lines.extend(stderr.lines().map(str::to_owned));
    let start = lines.len().saturating_sub(FAILURE_TAIL_LINES);
    lines[start..].to_vec()
}

fn first_relevant_diagnostic(output: &str) -> Option<&str> {
    output.lines().map(str::trim).find(|line| {
        if line.is_empty() {
            return false;
        }
        let lower = line.to_ascii_lowercase();
        ["error", "panic", "failed", "failure", "fatal", "assertion"]
            .iter()
            .any(|marker| lower.contains(marker))
    })
}

fn rerun_command(config: &Config, program: &OsStr, args: &[&str]) -> String {
    let mut command_parts = Vec::with_capacity(args.len() + 1);
    command_parts.push(shell_quote(&program.to_string_lossy()));
    command_parts.extend(args.iter().map(|arg| shell_quote(arg)));
    format!(
        "cd {} && CARGO_TARGET_DIR={} {}",
        shell_quote(&config.root_dir.display().to_string()),
        shell_quote(&config.cargo_target_dir.display().to_string()),
        command_parts.join(" ")
    )
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=+".contains(&byte))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn stage_log_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if stem.is_empty() {
        "stage".to_owned()
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::{FAILURE_TAIL_LINES, failure_tail, first_relevant_diagnostic, stage_log_stem};

    #[test]
    fn failure_tail_is_bounded_to_the_declared_limit() {
        let stdout = (0..200)
            .map(|line| format!("stdout-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = failure_tail(&stdout, "stderr-final");

        assert_eq!(tail.len(), FAILURE_TAIL_LINES);
        assert_eq!(tail.last().map(String::as_str), Some("stderr-final"));
        assert!(tail.iter().any(|line| line == "stdout-199"));
        assert!(!tail.iter().any(|line| line == "stdout-0"));
    }

    #[test]
    fn first_diagnostic_finds_error_markers() {
        assert_eq!(
            first_relevant_diagnostic("progress\nthread panicked: bad state\n"),
            Some("thread panicked: bad state")
        );
        assert_eq!(first_relevant_diagnostic("progress\ncomplete\n"), None);
    }

    #[test]
    fn stage_log_stem_cannot_escape_the_verify_directory() {
        assert_eq!(stage_log_stem("formal/seal"), "formal-seal");
        assert_eq!(stage_log_stem(""), "stage");
    }
}
