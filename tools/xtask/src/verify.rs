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
//!   output is retained at `build/verify/<stage>.log` for follow-up. Passing
//!   stages replace their logs too, so stale failures never describe a pass.
//! - **Forbidden:** no stage may be skipped silently, and the formal seal must
//!   be the last thing before a boot lane - a seal binds the source tree, so
//!   anything that edits tracked files after it invalidates it.
//! - **Evidence:** `docs/ai/commands.md`.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
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
/// - **Testing packages the wrong way.** `rootd` and `procd` are freestanding
///   binaries whose default `cargo test` harnesses segfault; their tests need
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

    let xtask = std::env::current_exe().context("locate xtask for verify stages")?;
    run_captured(config, "check", &xtask, &["check"])?;
    run_captured(config, "build", &xtask, &["build"])?;

    // Both services' build scripts need their native feature to omit the
    // freestanding linker entrypoint. Enable these within the workspace sweep
    // instead of testing the same dependency closure a second time.
    run_captured(config, "host-tests", &config.cargo, HOST_TEST_ARGS)?;

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

const HOST_TEST_ARGS: &[&str] = &[
    "test",
    "--workspace",
    "--features",
    "rootd/host-test,procd/host-test",
];

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
    let log_dir = config.build_dir.join("verify");
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("create verify log directory `{}`", log_dir.display()))?;
    let log_path = log_dir.join(format!("{}.log", stage_log_stem(name)));
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&config.root_dir)
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    let status = capture_command(&mut command, &log_path)
        .with_context(|| format!("run verify stage `{name}`: {}", args.join(" ")))?;
    if !status.success() {
        let command = rerun_command(config, program, args);
        let (diagnostic, tail) = failure_context(&log_path)?;

        eprintln!("xtask: verify {name} failed status={}", status);
        eprintln!("rerun: {command}");
        if let Some(diagnostic) = diagnostic {
            eprintln!("first diagnostic: {diagnostic}");
        } else {
            eprintln!("first diagnostic: <none matched; inspect the bounded tail>");
        }
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

// Write directly to a shared file description: output remains ordered and a
// verbose compiler/proof lane cannot accumulate its full log in xtask's heap.
fn capture_command(command: &mut Command, log_path: &Path) -> Result<ExitStatus> {
    let log = File::create(log_path)
        .with_context(|| format!("create verify stage log `{}`", log_path.display()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .status()
        .with_context(|| format!("execute {command:?}"))
}

fn failure_context(log_path: &Path) -> Result<(Option<String>, VecDeque<String>)> {
    let mut diagnostic = None;
    let mut tail = VecDeque::with_capacity(FAILURE_TAIL_LINES);
    for line in BufReader::new(File::open(log_path)?).split(b'\n') {
        let line = line?;
        let line = String::from_utf8_lossy(&line).into_owned();
        if diagnostic.is_none() {
            diagnostic = first_relevant_diagnostic(&line).map(str::to_owned);
        }
        if tail.len() == FAILURE_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    Ok((diagnostic, tail))
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
    if !value.is_empty()
        && value
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
    use super::*;

    #[test]
    fn failure_context_is_bounded_and_keeps_the_first_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stage.log");
        let output = (0..200)
            .map(|line| format!("progress-{line}\n"))
            .collect::<String>();
        fs::write(&path, format!("error: first failure\n{output}final line\n")).unwrap();
        let (diagnostic, tail) = failure_context(&path).unwrap();

        assert_eq!(diagnostic.as_deref(), Some("error: first failure"));
        assert_eq!(tail.len(), FAILURE_TAIL_LINES);
        assert_eq!(tail.back().map(String::as_str), Some("final line"));
        assert!(tail.iter().any(|line| line == "progress-199"));
        assert!(!tail.iter().any(|line| line == "progress-0"));
    }

    #[test]
    fn capture_command_retains_both_streams_on_success_and_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stage.log");
        for code in [0, 7] {
            let mut command = Command::new("bash");
            command.args([
                "-c",
                &format!("printf 'out\n'; printf 'error: err\n' >&2; exit {code}"),
            ]);
            let status = capture_command(&mut command, &path).unwrap();
            assert_eq!(status.code(), Some(code));
            assert_eq!(fs::read_to_string(&path).unwrap(), "out\nerror: err\n");
        }
    }

    #[test]
    fn failure_context_accepts_non_utf8_tool_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stage.log");
        fs::write(&path, b"error: \xff\n").unwrap();
        let (diagnostic, tail) = failure_context(&path).unwrap();
        assert_eq!(diagnostic.as_deref(), Some("error: \u{fffd}"));
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn host_tests_enable_each_freestanding_native_harness() {
        assert!(HOST_TEST_ARGS.contains(&"--workspace"));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let features = HOST_TEST_ARGS[HOST_TEST_ARGS
            .iter()
            .position(|arg| *arg == "--features")
            .unwrap()
            + 1];
        for package in ["rootd", "procd"] {
            let text =
                fs::read_to_string(root.join(format!("services/{package}/Cargo.toml"))).unwrap();
            let manifest: toml::Value = toml::from_str(&text).unwrap();
            assert!(manifest["features"].get("host-test").is_some());
            assert!(
                features
                    .split(',')
                    .any(|feature| feature == format!("{package}/host-test"))
            );
        }
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
    fn shell_quote_preserves_empty_arguments_and_metacharacters() {
        assert_eq!(shell_quote(""), "''");
        for value in ["", "simple/path", "a'b", "with spaces", "$(exit 7)"] {
            let output = Command::new("bash")
                .args(["-c", &format!("printf '%s' {}", shell_quote(value))])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, value.as_bytes());
        }
    }

    #[test]
    fn stage_log_stem_cannot_escape_the_verify_directory() {
        assert_eq!(stage_log_stem("formal/seal"), "formal-seal");
        assert_eq!(stage_log_stem(""), "stage");
    }
}
