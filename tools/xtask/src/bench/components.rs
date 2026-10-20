//! UI-deferred diagnostic component collection, NOT the shipping IPC max gate.
//! Completed component windows survive a later timeout. An unfinished window
//! never contributes samples, and a partial suite is never reported complete.
use super::{
    BEGIN_MARKER, BenchResult, END_MARKER, ParsedRun, TSC_PREFIX, parse_log, payload_lines,
};
use crate::{Result, config::Config};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[path = "../../../../apps/ipcbench/src/component_contract.rs"]
mod contract;

struct Window {
    name: String,
    run: ParsedRun,
}
struct Transcript {
    windows: Vec<Window>,
    ended: bool,
    pending: Option<String>,
}

fn diagnostic_complete(
    transcript: Option<&Transcript>,
    fresh: bool,
    same_source: bool,
    same_images: bool,
    faults: bool,
    launch_acceptable: bool,
) -> bool {
    transcript.is_some_and(|t| {
        t.ended && t.pending.is_none() && t.windows.len() == contract::PROBES.len()
    }) && fresh
        && same_source
        && same_images
        && !faults
        && launch_acceptable
}

fn valid_stats(result: &BenchResult) -> bool {
    result.min > 0
        && result.min <= result.p50
        && result.p50 <= result.p99
        && result.p99 <= result.max
        && (result.min..=result.max).contains(&result.mean)
}

fn validate_window(name: &str, run: &ParsedRun) -> Result<()> {
    let expected = contract::PROBES
        .iter()
        .find(|&&(probe, _)| probe == name)
        .context("unknown component window")?
        .1;
    ensure!(
        run.skipped.is_empty(),
        "component {name} skipped an operation"
    );
    let primary = run
        .results
        .iter()
        .find(|r| r.name == name)
        .context("component omitted its primary result")?;
    ensure!(
        primary.iters == expected && valid_stats(primary),
        "component {name} has wrong sample count/statistics"
    );
    let splits = contract::split_names(name);
    ensure!(
        run.results.len() == 1 + splits.len(),
        "component {name} has unrelated/missing results"
    );
    for split in splits {
        let result = run
            .results
            .iter()
            .find(|r| r.name == *split)
            .context("component omitted paired server stamps")?;
        ensure!(
            result.iters > 0 && result.iters <= expected && valid_stats(result),
            "invalid paired server stamps"
        );
    }
    Ok(())
}

fn parse_components(log: &str) -> Result<Transcript> {
    let mut windows = Vec::new();
    let mut pending: Option<(String, String)> = None;
    let mut started = false;
    let mut ended = false;
    let mut tsc = None;
    for line in payload_lines(log) {
        if line == BEGIN_MARKER {
            ensure!(!started && !ended, "duplicate component suite begin");
            started = true;
        } else if let Some(value) = line.strip_prefix(TSC_PREFIX) {
            ensure!(
                started && !ended && tsc.is_none() && pending.is_none(),
                "out-of-order component clock"
            );
            let value = value.parse::<u64>().context("malformed component clock")?;
            ensure!(value > 0, "zero component clock");
            tsc = Some(value);
        } else if let Some(name) = line.strip_prefix(contract::BEGIN) {
            ensure!(
                started && !ended && tsc.is_some() && pending.is_none(),
                "overlapping/unbound component begin"
            );
            ensure!(
                contract::PROBES
                    .get(windows.len())
                    .is_some_and(|&(expected, _)| expected == name),
                "unknown, duplicate, or out-of-order component {name}"
            );
            pending = Some((name.to_owned(), String::new()));
        } else if let Some(name) = line.strip_prefix(contract::END) {
            let (expected, body) = pending.take().context("unmatched component end")?;
            ensure!(name == expected, "component end changed its name");
            // Reuse the strict result parser only AFTER observing a real closed
            // component. These in-memory delimiters do not invent a suite end
            // or make partial data eligible for the shipping acceptance path.
            let run = parse_log(&format!(
                "{BEGIN_MARKER}\n{TSC_PREFIX}{}\n{body}{END_MARKER}\n",
                tsc.unwrap()
            ))?;
            validate_window(name, &run)?;
            windows.push(Window {
                name: expected,
                run,
            });
        } else if line == END_MARKER {
            ensure!(
                started && !ended && tsc.is_some() && pending.is_none(),
                "invalid component suite end"
            );
            ended = true;
        } else if let Some((_, body)) = pending.as_mut() {
            body.push_str(line);
            body.push('\n');
        } else {
            ensure!(
                !line.starts_with(super::RESULT_PREFIX) && !line.starts_with(super::SKIP_PREFIX),
                "component result outside a closed measurement bracket"
            );
        }
    }
    Ok(Transcript {
        windows,
        ended,
        pending: pending.map(|(name, _)| name),
    })
}

fn runtime_fault(log: &str) -> bool {
    [
        "!PANIC-SITE:",
        "[PANIC]",
        "name=linux-user-fault ",
        "panicked at ",
        "fatal service endpoint not ready",
    ]
    .iter()
    .any(|marker| log.contains(marker))
}

fn diagnostic_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .to_string()
            .starts_with("KVM parallel boot did not reach readiness within ")
    })
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn result_json(result: &BenchResult) -> Value {
    json!({"name": result.name, "iterations": result.iters, "min": result.min,
        "p50": result.p50, "p99": result.p99, "max": result.max, "mean": result.mean})
}

pub(crate) fn bench_components(config: &Config, vcpus: u8, timeout: u64) -> Result<()> {
    ensure!(
        (1..=120).contains(&timeout),
        "component diagnostic timeout must be 1..=120 seconds"
    );
    ensure!(
        [1, 2, 4, 8].contains(&vcpus),
        "components require 1/2/4/8 vCPUs"
    );
    crate::build::build(config, false)?;
    let source = crate::formal_contracts::source_tree_hash(&config.root_dir)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = config.build_dir.join("bench-components");
    fs::create_dir_all(&root)?;
    let directory = root.join(format!("vcpu-{vcpus}-{nonce}"));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let kernel = config.artifact_nucleus_elf_path();
    let app = config
        .build_dir
        .join("artifacts/apps/ipcbench/ipcbench.elf");
    let kernel_hash = sha256(&fs::read(&kernel)?);
    let app_hash = sha256(&fs::read(&app)?);
    fs::copy(&kernel, directory.join("nucleus.elf"))?;
    fs::copy(&app, directory.join("ipcbench.elf"))?;
    // Keep the ordinary devices and userspace launch/policy path. UI failures
    // are not debugged here. Only this diagnostic collector can retain closed
    // component data after a later readiness timeout; `bench` stays strict.
    let started = SystemTime::now();
    let args = [
        "--gui-dvm-surfaces",
        "--dvm-network-shmem",
        "--dvm-block-shmem",
        "--timeout",
        &timeout.to_string(),
        "--rustos-vcpus",
        &vcpus.to_string(),
        "--ipcbench-probe",
        contract::FILTER,
        "--expect",
        END_MARKER,
    ]
    .map(str::to_owned);
    let launch = crate::kvm::kvm_benchmark_command(config, args.into_iter());
    let path = config.build_dir.join("kvm/rustos-debugcon.log");
    let fresh = path
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|mtime| mtime >= started);
    let raw = if fresh {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    write_private(&directory.join("guest-debugcon.log"), raw.as_bytes())?;
    let log = crate::kvm::rejoin_user_debug_records(&raw);
    let parsed = parse_components(&log);
    let same_source = source == crate::formal_contracts::source_tree_hash(&config.root_dir)?;
    let same_images = kernel_hash == sha256(&fs::read(&kernel)?)
        && app_hash == sha256(&fs::read(&app)?)
        && kernel_hash == sha256(&fs::read(config.build_dir.join("image/nucleus.elf"))?)
        && app_hash
            == sha256(&fs::read(
                config.build_dir.join("image/apps/ipcbench/ipcbench.elf"),
            )?);
    let faults = runtime_fault(&log);
    let launch_acceptable_for_diagnostic =
        launch.as_ref().map_or_else(diagnostic_timeout, |_| true);
    let complete = diagnostic_complete(
        parsed.as_ref().ok(),
        fresh,
        same_source,
        same_images,
        faults,
        launch_acceptable_for_diagnostic,
    );
    let mut windows = Vec::new();
    println!(
        "\nDIAGNOSTIC COMPONENTS vcpus={vcpus}; raw invariant-TSC ticks; placement=unconstrained"
    );
    println!(
        "{:<40} {:>8} {:>10} {:>10} {:>10} {:>12}",
        "component", "iters", "min", "p50", "p99", "max"
    );
    if let Ok(transcript) = &parsed {
        for window in &transcript.windows {
            let primary = window
                .run
                .results
                .iter()
                .find(|r| r.name == window.name)
                .unwrap();
            println!(
                "{:<40} {:>8} {:>10} {:>10} {:>10} {:>12}",
                primary.name, primary.iters, primary.min, primary.p50, primary.p99, primary.max
            );
            windows.push(json!({"name": window.name, "tsc_khz": window.run.tsc_khz,
                "results": window.run.results.iter().map(result_json).collect::<Vec<_>>(),
                "system_wide_phase_observations": window.run.phases.iter().map(|p| json!({
                    "name":p.name,"cycles":p.cycles.to_string(),"samples":p.samples.to_string()})).collect::<Vec<_>>(),
                "system_wide_counters": window.run.counters.iter().map(|c| json!({
                    "name":c.name,"units":c.units.to_string(),"operations":c.operations.to_string()})).collect::<Vec<_>>() }));
        }
    }
    let disassembly = json!({
        "kernel": super::component_disassembly::capture(&directory.join("nucleus.elf"), &directory.join("kernel-disassembly"), true)?,
        "app": super::component_disassembly::capture(&directory.join("ipcbench.elf"), &directory.join("app-disassembly"), false)?,
    });
    let project = &config.project;
    let report = json!({"schema":"rustos-component-diagnostic-v1", "diagnostic_only":true,
        "shipping_acceptance":false,"rustos_vcpus":vcpus,"placement":"unconstrained",
        "diagnostic_complete":complete,"full_topology_accepted":launch.is_ok(),
        "launch_error":launch.as_ref().err().map(|e| format!("{e:#}")),
        "parse_error":parsed.as_ref().err().map(|e| format!("{e:#}")),
        "unfinished_component":parsed.as_ref().ok().and_then(|t| t.pending.as_ref()),
        "suite_end_observed":parsed.as_ref().is_ok_and(|t| t.ended),
        "source_tree_sha256":source,"same_source":same_source,"fresh_log":fresh,
        "kernel_sha256":kernel_hash,"ipcbench_sha256":app_hash,"same_linked_and_staged_images":same_images,
        "runtime_fault_observed":faults,"warmups":2000,"timeout_seconds":timeout,
        "profiles":{"ipc":project.ipc_telemetry.phase_profile,"scheduler":project.scheduler_telemetry.phase_profile,
            "lock":project.lock_telemetry.phase_profile,"syscall":project.syscall_telemetry.phase_profile,
            "usermem":project.usermem_telemetry.phase_profile},
        "components":windows,"disassembly":disassembly});
    write_private(
        &directory.join("report.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "diagnostic_complete={complete} full_topology_accepted={} shipping_acceptance=false report={}",
        launch.is_ok(),
        directory.join("report.json").display()
    );
    ensure!(
        complete,
        "component diagnostic incomplete/invalid; archived data only; UI investigation deferred"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(name: &str, count: u64) -> String {
        format!(
            "ipcbench: result name={name} iters={count} min=80 p50=120 p90=160 p99=200 max=400 mean=140 min_ns=20 p50_ns=30 mean_ns=35\n"
        )
    }
    fn transcript() -> String {
        let mut s = format!("{BEGIN_MARKER}\n{TSC_PREFIX}4000000\n");
        for &(name, count) in contract::PROBES {
            s.push_str(&format!("{}{name}\n{}", contract::BEGIN, row(name, count)));
            for split in contract::split_names(name) {
                s.push_str(&row(split, count));
            }
            s.push_str(&format!("{}{name}\n", contract::END));
        }
        s.push_str(&format!("{END_MARKER}\n"));
        s
    }
    #[test]
    fn components_require_closed_exact_semantic_windows_and_full_sample_counts() {
        let valid = transcript();
        let parsed = parse_components(&valid).unwrap();
        assert_eq!(parsed.windows.len(), contract::PROBES.len());
        assert!(parsed.ended);
        for bad in [
            valid.replacen("iters=50000", "iters=0", 1),
            valid.replacen("iters=50000", "iters=49999", 1),
            valid.replacen("min=80", "min=0", 1),
            valid.replacen("min=80", "min=800", 1),
            valid.replacen("max=400", "max=40", 1),
            valid.replacen(
                "component begin name=vmexit_cpuid",
                "component begin name=unknown",
                1,
            ),
            valid.replacen(
                "component end name=vmexit_cpuid",
                "component end name=tsc_overhead",
                1,
            ),
            valid.replacen(
                "ipcbench: result name=vmexit_cpuid",
                "ipcbench: skip name=vmexit_cpuid",
                1,
            ),
        ] {
            assert!(parse_components(&bad).is_err());
        }
        let partial = valid
            .split("ipcbench: component end name=tsc_overhead")
            .next()
            .unwrap();
        let parsed = parse_components(partial).unwrap();
        assert_eq!(parsed.windows.len(), 1);
        assert!(!parsed.ended);
        assert_eq!(parsed.pending.as_deref(), Some("tsc_overhead"));
    }
    #[test]
    fn diagnostic_completion_requires_actual_end_all_windows_and_exact_provenance() {
        let mut parsed = parse_components(&transcript()).unwrap();
        assert!(diagnostic_complete(
            Some(&parsed),
            true,
            true,
            true,
            false,
            true
        ));
        assert!(!diagnostic_complete(None, true, true, true, false, true));
        for flags in [
            (false, true, true, false, true),
            (true, false, true, false, true),
            (true, true, false, false, true),
            (true, true, true, true, true),
            (true, true, true, false, false),
        ] {
            assert!(!diagnostic_complete(
                Some(&parsed),
                flags.0,
                flags.1,
                flags.2,
                flags.3,
                flags.4
            ));
        }
        parsed.ended = false;
        assert!(!diagnostic_complete(
            Some(&parsed),
            true,
            true,
            true,
            false,
            true
        ));
        parsed.ended = true;
        parsed.pending = Some("unfinished".to_owned());
        assert!(!diagnostic_complete(
            Some(&parsed),
            true,
            true,
            true,
            false,
            true
        ));
        parsed.pending = None;
        parsed.windows.pop();
        assert!(!diagnostic_complete(
            Some(&parsed),
            true,
            true,
            true,
            false,
            true
        ));
    }
    #[test]
    fn components_reject_duplicate_out_of_order_and_unbracketed_records() {
        let valid = transcript();
        for bad in [
            format!("{BEGIN_MARKER}\n{valid}"),
            format!("{valid}{END_MARKER}\n"),
            format!("{valid}{}", row("null_syscall_getpid", 10000)),
            valid.replacen(TSC_PREFIX, &format!("{TSC_PREFIX}4000000\n{TSC_PREFIX}"), 1),
            valid.replacen(
                "component end name=vmexit_cpuid",
                "component begin name=tsc_overhead",
                1,
            ),
            valid.replacen(
                "component begin name=tsc_overhead",
                "component begin name=vmexit_cpuid",
                1,
            ),
        ] {
            assert!(
                parse_components(&bad).is_err(),
                "accepted malformed transcript: {bad}"
            );
        }
        let partial = valid.strip_suffix(&format!("{END_MARKER}\n")).unwrap();
        let parsed = parse_components(partial).unwrap();
        assert_eq!(parsed.windows.len(), contract::PROBES.len());
        assert!(!diagnostic_complete(
            Some(&parsed),
            true,
            true,
            true,
            false,
            true
        ));
        let empty = parse_components("unrelated startup records").unwrap();
        assert!(empty.windows.is_empty());
        assert!(!diagnostic_complete(
            Some(&empty),
            true,
            true,
            true,
            false,
            true
        ));
    }
    #[test]
    fn components_do_not_treat_crashes_or_generic_launch_errors_as_ui_timeouts() {
        assert!(diagnostic_timeout(&anyhow::anyhow!(
            "KVM parallel boot did not reach readiness within 30s; missing UI"
        )));
        for error in [
            "QEMU guest exited",
            "build/sign failure",
            "some unrelated timeout",
        ] {
            assert!(!diagnostic_timeout(&anyhow::anyhow!(error.to_owned())));
        }
        for error in [
            "[PANIC]",
            "!PANIC-SITE:x",
            "name=linux-user-fault ",
            "fatal service endpoint not ready",
        ] {
            assert!(runtime_fault(error));
        }
        assert!(!runtime_fault("uiserver: panic hook installed"));
    }
}
