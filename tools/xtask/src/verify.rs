//! One command for the local verification pipeline.
//!
//! - **Owner:** this module owns the *order* of the local gate and the exact
//!   invocation each stage needs. Every stage still belongs to the module that
//!   implements it; nothing is reimplemented here.
//! - **Boundary:** stage output is captured and shown only for the stage that
//!   failed, so a passing run is a handful of lines rather than several
//!   thousand.
//! - **Lifecycle:** stages run in dependency order and stop at the first
//!   failure, because every later stage would be describing a tree that does
//!   not build.
//! - **Failure:** a failing stage prints its own captured output and its exact
//!   command line, so it can be rerun in isolation.
//! - **Forbidden:** no stage may be skipped silently, and the formal seal must
//!   be the last thing before a boot lane - a seal binds the source tree, so
//!   anything that edits tracked files after it invalidates it.
//! - **Evidence:** `docs/ai/commands.md`.

use std::ffi::OsStr;
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

/// Runs one stage as a child process, showing its output only if it failed.
fn run_captured(
    config: &Config,
    name: &str,
    program: impl AsRef<OsStr>,
    args: &[&str],
) -> Result<()> {
    let started = Instant::now();
    let output = Command::new(program)
        .args(args)
        .current_dir(&config.root_dir)
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir)
        .output()
        .with_context(|| format!("run verify stage `{name}`: {}", args.join(" ")))?;
    if !output.status.success() {
        // The failing stage's own output is the debugging context, so print all
        // of it, then name the command so it can be rerun on its own.
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        bail!("verify stage `{name}` failed: {}", args.join(" "));
    }
    println!(
        "xtask: verify {name} passed elapsed_seconds={}",
        started.elapsed().as_secs()
    );
    Ok(())
}
