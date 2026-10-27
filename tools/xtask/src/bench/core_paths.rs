//! Core-path suite acceptance: closed vocabulary, exact complete sample counts.
//! A skipped/short probe is missing evidence, never a fast operation.

use super::{HARDWARE_ANCHOR, ParsedRun};
use std::collections::BTreeMap;
pub(super) type Placements = BTreeMap<String, BTreeMap<u32, u64>>;

pub(super) fn record_placement(placements: &mut Placements, line: &str) -> Result<()> {
    let name = line
        .split_ascii_whitespace()
        .find_map(|part| part.strip_prefix("name="));
    ensure!(
        matches!(name, Some("wake_to_run_futex" | "smp_balance_futex"))
            && line.split_ascii_whitespace().count() == 3,
        "invalid placement identity"
    );
    let apic = super::field(line, "apic=").and_then(|value| u32::try_from(value).ok());
    let count = super::field(line, "samples=").filter(|count| *count > 0);
    ensure!(
        apic.is_some() && count.is_some(),
        "invalid placement observation"
    );
    let records = placements.entry(name.unwrap().to_owned()).or_default();
    ensure!(
        records.insert(apic.unwrap(), count.unwrap()).is_none(),
        "duplicate placement APIC"
    );
    Ok(())
}

pub(super) fn validate_placements(
    placements: &Placements,
    results: &[super::BenchResult],
) -> Result<()> {
    for (name, records) in placements {
        let total: u128 = records.values().map(|count| u128::from(*count)).sum();
        ensure!(
            results
                .iter()
                .any(|row| row.name == *name && u128::from(row.iters) == total),
            "placement sample count differs from complete timed workload {name}"
        );
    }
    Ok(())
}
use anyhow::{Result, ensure};

#[path = "../../../../apps/ipcbench/src/core_contract.rs"]
pub(super) mod contract;

pub(super) fn validate(run: &ParsedRun, suite: &str, linux: bool) -> Result<()> {
    ensure!(contract::is_suite(suite), "unknown core-path suite {suite}");
    ensure!(
        run.skipped.is_empty(),
        "core-path suite has skipped operations: {:?}",
        run.skipped
    );
    let require = |name: &str, iters: u64| -> Result<()> {
        let rows: Vec<_> = run.results.iter().filter(|row| row.name == name).collect();
        ensure!(
            rows.len() == 1 && rows[0].iters == iters,
            "core-path suite requires {name} with exactly {iters} samples"
        );
        Ok(())
    };
    require(HARDWARE_ANCHOR, 50_000)?;
    if suite == contract::STATE {
        require("state_pristine_null", contract::ITERS as u64)?;
        for phase in std::iter::once("before").chain(contract::STATE_TRANSITIONS.iter().copied()) {
            require(&format!("state_{phase}_null"), contract::ITERS as u64)?;
            if !linux {
                require(&format!("state_{phase}_ipc"), 20_000)?;
                for leg in ["to_recv", "body", "to_return"] {
                    require(&format!("state_{phase}_ipc_{leg}"), 20_000)?;
                }
            }
        }
        for kind in if linux {
            &["null"][..]
        } else {
            &["null", "ipc"][..]
        } {
            let baseline = run
                .results
                .iter()
                .find(|row| {
                    row.name
                        == if *kind == "null" {
                            "state_pristine_null"
                        } else {
                            "state_before_ipc"
                        }
                })
                .unwrap();
            ensure!(baseline.p50 > 0, "zero state baseline");
            for phase in
                std::iter::once("before").chain(contract::STATE_TRANSITIONS.iter().copied())
            {
                let row = run
                    .results
                    .iter()
                    .find(|row| row.name == format!("state_{phase}_{kind}"))
                    .unwrap();
                ensure!(
                    u128::from(row.p50) * 2 <= u128::from(baseline.p50) * 3,
                    "persistent state tax: {} p50={} before={} (>1.5x)",
                    row.name,
                    row.p50,
                    baseline.p50
                );
            }
        }
    } else {
        if suite == contract::SCHEDULER {
            ensure!(
                ["wake_to_run_futex", "smp_balance_futex"]
                    .iter()
                    .all(|name| run.placements.contains_key(*name)),
                "scheduler suite is missing observed APIC distributions"
            );
        }
        for &name in contract::probes(suite).expect("closed suite") {
            let iters = if name == "cow_first_write" {
                128
            } else if suite == contract::SCHEDULER {
                match name {
                    "sched_yield" => 50_000,
                    "smp_balance_futex" => 2048,
                    _ => contract::ITERS as u64,
                }
            } else if suite == contract::LIFECYCLE {
                match name {
                    "fork_exec_exit_wait" | "exec_replace_single_thread" => 32,
                    "thread_clone_exit_join" | "fork_cow_private_write" => 64,
                    _ => 128,
                }
            } else if suite == contract::SOCKET_BUFFERED {
                contract::DATA_ITERS as u64
            } else if suite == contract::DATA {
                if name.ends_with("_1b") {
                    contract::ITERS as u64
                } else {
                    contract::DATA_ITERS as u64
                }
            } else {
                contract::ITERS as u64
            };
            require(name, iters)?;
        }
    }
    Ok(())
}

pub(super) fn render_quality(run: &ParsedRun) -> String {
    let mut table = String::from(
        "\ncore-path tail quality (raw; <=1.5 required, failures are not filtered):\n",
    );
    for row in &run.results {
        if row.name == HARDWARE_ANCHOR || row.name.starts_with("ipc_split_") {
            continue;
        }
        let pass = row.p50 > 0 && u128::from(row.p99) * 2 <= u128::from(row.p50) * 3;
        table.push_str(&format!(
            "  {} p99/p50={:.3} {}\n",
            row.name,
            row.p99 as f64 / row.p50.max(1) as f64,
            if pass { "PASS" } else { "FAIL" }
        ));
    }
    for (name, records) in &run.placements {
        table.push_str(&format!(
            "  {name} post-wake APIC observations (not an atomic stamp+CPU pair): {records:?}\n"
        ));
    }
    table
}

pub(super) struct Capture {
    directory: std::path::PathBuf,
    source: String,
    app: String,
    kernel: String,
    ui: String,
    suite: String,
    vcpus: u8,
}

fn file_hash(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

impl Capture {
    pub(super) fn begin(config: &crate::config::Config, suite: &str, vcpus: u8) -> Result<Self> {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            time::{SystemTime, UNIX_EPOCH},
        };
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = config
            .build_dir
            .join("bench-core")
            .join(format!("{suite}-vcpu-{vcpus}-{nonce}"));
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let source = crate::formal_contracts::source_tree_hash(&config.root_dir)?;
        let app = app_hash(config)?;
        let kernel = file_hash(&config.artifact_nucleus_elf_path())?;
        let ui_path = config
            .build_dir
            .join("artifacts/services/uiserver/uiserver.elf");
        let ui = file_hash(&ui_path)?;
        fs::copy(&ui_path, directory.join("uiserver.elf"))?;
        fs::copy(
            config
                .build_dir
                .join("artifacts/apps/ipcbench/ipcbench.elf"),
            directory.join("ipcbench.elf"),
        )?;
        fs::copy(
            config.artifact_nucleus_elf_path(),
            directory.join("nucleus.elf"),
        )?;
        Ok(Self {
            directory,
            source,
            app,
            kernel,
            ui,
            suite: suite.to_owned(),
            vcpus,
        })
    }

    pub(super) fn finish(
        &self,
        config: &crate::config::Config,
        log: &std::path::Path,
        launch_ok: bool,
    ) -> Result<()> {
        use std::fs;
        fs::copy(log, self.directory.join("guest.log"))?;
        let unchanged = self.source == crate::formal_contracts::source_tree_hash(&config.root_dir)?
            && self.app == app_hash(config)?
            && self.kernel == file_hash(&config.artifact_nucleus_elf_path())?
            && self.ui
                == file_hash(
                    &config
                        .build_dir
                        .join("artifacts/services/uiserver/uiserver.elf"),
                )?;
        let staged = self.app == file_hash(&config.image_dir.join("apps/ipcbench/ipcbench.elf"))?
            && self.kernel == file_hash(&config.image_dir.join("nucleus.elf"))?
            && self.ui == file_hash(&config.image_dir.join("services/uiserver/uiserver.elf"))?;
        let dvm_log = config.build_dir.join("kvm/linux-dvm-serial.log");
        let dvm_log_hash = if dvm_log.exists() {
            fs::copy(&dvm_log, self.directory.join("linux-dvm-serial.log"))?;
            Some(file_hash(&dvm_log)?)
        } else {
            None
        };
        let project = &config.project;
        let revision = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"]).current_dir(&config.root_dir).output()?;
        ensure!(revision.status.success(), "read benchmark source revision");
        let revision = String::from_utf8(revision.stdout)?;
        let report = serde_json::json!({"v":1, "suite":self.suite, "vcpus":self.vcpus,
            "source_revision":revision.trim(),
            "source_tree_hash":self.source, "ipcbench_sha256":self.app, "nucleus_sha256":self.kernel,
            "uiserver_sha256":self.ui, "dvm_serial_sha256":dvm_log_hash,
            "launch_ok":launch_ok, "source_and_elf_unchanged":unchanged, "staged_elf_match":staged,
            "warmup_counts":{"primitive":contract::WARMUP,"bulk":contract::DATA_WARMUP},
            "host_boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
            "profiles":{"ipc":project.ipc_telemetry.phase_profile, "scheduler":project.scheduler_telemetry.phase_profile,
                "lock":project.lock_telemetry.phase_profile, "syscall":project.syscall_telemetry.phase_profile,
                "usermem":project.usermem_telemetry.phase_profile, "lifecycle":project.lifecycle_telemetry.phase_profile}});
        fs::write(
            self.directory.join("metadata.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!("core-path archive={}", self.directory.display());
        ensure!(
            unchanged && staged,
            "core-path source or linked/staged ELF changed during measurement"
        );
        Ok(())
    }
}

fn app_hash(config: &crate::config::Config) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(std::fs::read(
            config
                .build_dir
                .join("artifacts/apps/ipcbench/ipcbench.elf")
        )?)
    ))
}

pub(crate) fn linux_guest(config: &crate::config::Config, suite: &str, vcpus: u8) -> Result<()> {
    use anyhow::Context;
    use std::{fs, process::Command};
    ensure!(
        contract::is_suite(suite),
        "Linux baseline requires a core_* suite"
    );
    ensure!(
        [1, 8].contains(&vcpus),
        "Linux core baseline supports 1 or 8 vCPUs"
    );
    crate::build::build(config, false)?;
    let source = crate::formal_contracts::source_tree_hash(&config.root_dir)?;
    let hash = app_hash(config)?;
    let output = Command::new("python3")
        .arg(config.root_dir.join("tools/bench/run-linux-guest.py"))
        .args(["--binary"])
        .arg(
            config
                .build_dir
                .join("artifacts/apps/ipcbench/ipcbench.elf"),
        )
        .args(["--output"])
        .arg(config.build_dir.join("bench-linux"))
        .args(["--vcpus", &vcpus.to_string(), "--suite", suite])
        .output()
        .context("run same-host Linux guest collector")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    ensure!(
        output.status.success(),
        "Linux collector failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = stdout
        .lines()
        .find_map(|line| line.strip_prefix("linux-baseline log="))
        .context("Linux collector omitted its private log path")?;
    let path = std::path::Path::new(log);
    let raw = fs::read_to_string(path)?;
    let run = super::parse_log(&raw)?;
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(
        path.parent()
            .context("Linux log parent")?
            .join("metadata.json"),
    )?)?;
    validate_linux_completion(&metadata, &raw)?;
    println!(
        "{}{}{}",
        super::render(run.tsc_khz, &run.results, &run.skipped),
        run.tails.render(),
        render_quality(&run)
    );
    ensure!(
        source == crate::formal_contracts::source_tree_hash(&config.root_dir)?
            && hash == app_hash(config)?,
        "source or ELF changed during Linux measurement"
    );
    validate(&run, suite, true)?;
    fs::write(
        path.parent()
            .context("Linux log parent")?
            .join("source.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"source_tree_hash": source, "ipcbench_sha256": hash, "collection_complete":true}),
        )?,
    )?;
    println!("Linux core-path suite collection COMPLETE (no RustOS IPC emulation)");
    Ok(())
}

fn validate_linux_completion(metadata: &serde_json::Value, raw: &str) -> Result<()> {
    ensure!(
        metadata["exit_code"].as_i64() == Some(0)
            && metadata
                .get("timeout")
                .is_none_or(|value| value.as_bool() == Some(false)),
        "Linux baseline runner failed or timed out"
    );
    let lines: Vec<_> = raw.lines().map(str::trim).collect();
    let completions: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == "linux-baseline: child-complete v=1")
        .collect();
    ensure!(
        completions.len() == 1
            && !lines
                .iter()
                .any(|line| line.starts_with("linux-baseline: FAIL"))
            && lines
                .iter()
                .position(|line| *line == super::END_MARKER)
                .is_some_and(|end| end < completions[0].0),
        "Linux baseline lacks exact successful PID1 completion after the suite"
    );
    Ok(())
}

pub(super) fn compare_linux(
    config: &crate::config::Config,
    run: &ParsedRun,
    suite: &str,
    vcpus: u8,
    path: &std::path::Path,
) -> Result<String> {
    use anyhow::Context;
    use std::fs;
    let raw = fs::read_to_string(path)?;
    let linux = super::parse_log(&raw)?;
    validate(&linux, suite, true)?;
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(
        path.parent()
            .context("Linux baseline parent")?
            .join("metadata.json"),
    )?)?;
    validate_linux_completion(&metadata, &raw)?;
    let seal: serde_json::Value = serde_json::from_slice(&fs::read(
        path.parent()
            .context("Linux baseline parent")?
            .join("source.json"),
    )?)?;
    ensure!(
        seal["collection_complete"].as_bool() == Some(true)
            && seal["ipcbench_sha256"] == metadata["ipcbench_sha256"],
        "Linux archive did not complete source/ELF-bound collection"
    );
    ensure!(
        metadata["host_boot_id"].as_str()
            == Some(fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()),
        "Linux baseline is not from this host boot"
    );
    ensure!(
        metadata["vcpus"].as_u64() == Some(u64::from(vcpus))
            && metadata["suite"].as_str() == Some(suite),
        "Linux baseline topology or suite mismatch"
    );
    ensure!(
        metadata["ipcbench_sha256"].as_str() == Some(app_hash(config)?.as_str()),
        "Linux baseline used a different ipcbench ELF"
    );
    let anchor = |data: &ParsedRun| {
        data.results
            .iter()
            .find(|row| row.name == HARDWARE_ANCHOR)
            .unwrap()
            .p50
    };
    let (a, b) = (anchor(run), anchor(&linux));
    ensure!(
        a > 0 && b > 0 && u128::from(a.abs_diff(b)) * 100 <= u128::from(b) * 3,
        "CPUID p50 anchor drift exceeds 3% (RustOS={a}, Linux={b}); no ratio claim"
    );
    let mut table = String::from(
        "\nRustOS / same-host Linux guest (raw ratios, no normalization):\n  probe                                   p50      p99     mean\n",
    );
    for row in &run.results {
        if row.name == HARDWARE_ANCHOR {
            continue;
        }
        if let Some(base) = linux.results.iter().find(|base| base.name == row.name) {
            ensure!(
                base.p50 > 0 && base.p99 > 0 && base.mean > 0,
                "zero Linux divisor"
            );
            table.push_str(&format!(
                "  {:<36} {:>7.2}x {:>7.2}x {:>7.2}x\n",
                row.name,
                row.p50 as f64 / base.p50 as f64,
                row.p99 as f64 / base.p99 as f64,
                row.mean as f64 / base.mean as f64
            ));
        }
    }
    table.push_str("A single ratio table is not same-session ABAB optimization evidence.\n");
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn log() -> String {
        let mut log = String::from("ipcbench: begin\nipcbench: tsc_khz=4000000\n");
        for (&name, iters) in std::iter::once((&HARDWARE_ANCHOR, 50_000)).chain(
            contract::SYSCALL_PROBES
                .iter()
                .map(|name| (name, contract::ITERS)),
        ) {
            log.push_str(&format!("ipcbench: result name={name} iters={iters} min=40 p50=100 p99=150 max=500 mean=110 min_ns=10 p50_ns=25\n"));
        }
        log.push_str("ipcbench: end\n");
        log
    }

    #[test]
    fn complete_suite_is_required_not_just_an_anchor_or_one_cheap_row() {
        let original = log();
        let run = super::super::parse_log(&original).unwrap();
        validate(&run, contract::SYSCALL, false).unwrap();
        for broken in [
            original.replace("name=syscall_getuid", "name=wrong_operation"),
            original.replace("iters=2000", "iters=1999"),
            original.replace(
                "ipcbench: end",
                "ipcbench: skip name=syscall_stat reason=ENOSYS\nipcbench: end",
            ),
            original.replace("name=vmexit_cpuid", "name=wrong_anchor"),
        ] {
            assert!(
                validate(
                    &super::super::parse_log(&broken).unwrap(),
                    contract::SYSCALL,
                    false
                )
                .is_err()
            );
        }
    }

    #[test]
    fn buffered_socket_suite_requires_both_complete_samples() {
        let mut log = String::from("ipcbench: begin\nipcbench: tsc_khz=4000000\nipcbench: result name=vmexit_cpuid iters=50000 min=40 p50=100 p99=150 max=500 mean=110 min_ns=10 p50_ns=25\n");
        for name in contract::SOCKET_BUFFERED_PROBES {
            log.push_str(&format!("ipcbench: result name={name} iters=128 min=40 p50=100 p99=150 max=500 mean=110 min_ns=10 p50_ns=25\n"));
        }
        log.push_str("ipcbench: end\n");
        validate(&super::super::parse_log(&log).unwrap(), contract::SOCKET_BUFFERED, false).unwrap();
        for broken in [
            log.replace("iters=128", "iters=127"),
            log.replace("socket_buffered_read_32k", "wrong_probe"),
        ] {
            assert!(validate(
                &super::super::parse_log(&broken).unwrap(),
                contract::SOCKET_BUFFERED,
                false,
            )
            .is_err());
        }
    }

    #[test]
    fn state_tax_gate_checks_every_transition_and_both_null_and_real_ipc() {
        let mut log = String::from(
            "ipcbench: begin\nipcbench: tsc_khz=4000000\nipcbench: result name=vmexit_cpuid iters=50000 min=40 p50=100 p99=200 max=1000 mean=110 min_ns=10 p50_ns=25\n",
        );
        log.push_str("ipcbench: result name=state_pristine_null iters=2000 min=40 p50=480 p99=960 max=100000 mean=480 min_ns=10 p50_ns=25\n");
        for phase in std::iter::once("before").chain(contract::STATE_TRANSITIONS.iter().copied()) {
            for (kind, iters, p50) in [("null", 2000, 480), ("ipc", 20000, 10000)] {
                log.push_str(&format!("ipcbench: result name=state_{phase}_{kind} iters={iters} min=40 p50={p50} p99={} max=100000 mean={p50} min_ns=10 p50_ns=25\n", p50 * 2));
                if kind == "ipc" {
                    for leg in ["to_recv", "body", "to_return"] {
                        log.push_str(&format!("ipcbench: result name=state_{phase}_ipc_{leg} iters=20000 min=40 p50=100 p99=200 max=100000 mean=100 min_ns=10 p50_ns=25\n"));
                    }
                }
            }
        }
        log.push_str("ipcbench: end\n");
        validate(
            &super::super::parse_log(&log).unwrap(),
            contract::STATE,
            false,
        )
        .unwrap();
        let inflated_baseline = log.replace(
            "name=state_before_null iters=2000 min=40 p50=480",
            "name=state_before_null iters=2000 min=40 p50=721",
        );
        assert!(
            validate(
                &super::super::parse_log(&inflated_baseline).unwrap(),
                contract::STATE,
                false
            )
            .is_err()
        );
        let partial = log.replace(
            "name=state_exec_ipc_body iters=20000",
            "name=state_exec_ipc_body iters=1",
        );
        assert!(
            validate(
                &super::super::parse_log(&partial).unwrap(),
                contract::STATE,
                false
            )
            .is_err()
        );
        for phase in contract::STATE_TRANSITIONS {
            for (kind, iters, before) in [("null", 2000, 480), ("ipc", 20000, 10000)] {
                let original =
                    format!("name=state_{phase}_{kind} iters={iters} min=40 p50={before}");
                let changed = |p50| {
                    log.replace(
                        &original,
                        &format!("name=state_{phase}_{kind} iters={iters} min=40 p50={p50}"),
                    )
                };
                validate(
                    &super::super::parse_log(&changed(before * 3 / 2)).unwrap(),
                    contract::STATE,
                    false,
                )
                .unwrap();
                assert!(
                    validate(
                        &super::super::parse_log(&changed(before * 3 / 2 + 1)).unwrap(),
                        contract::STATE,
                        false
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn complete_samples_do_not_rescue_a_failed_linux_guest() {
        let raw = format!("{}linux-baseline: child-complete v=1\n", log());
        let valid = serde_json::json!({"exit_code":0});
        validate_linux_completion(&valid, &raw).unwrap();
        for metadata in [
            serde_json::json!({}),
            serde_json::json!({"exit_code":1}),
            serde_json::json!({"exit_code":0,"timeout":true}),
            serde_json::json!({"exit_code":0,"timeout":0}),
        ] {
            assert!(validate_linux_completion(&metadata, &raw).is_err());
        }
        for broken in [
            log(),
            format!("{raw}linux-baseline: child-complete v=1\n"),
            format!("{raw}linux-baseline: FAIL child-status=1\n"),
            format!("linux-baseline: child-complete v=1\n{}", log()),
        ] {
            assert!(validate_linux_completion(&valid, &broken).is_err());
        }
    }

    #[test]
    fn placement_counts_are_unique_bounded_and_bound_to_the_timed_samples() {
        let mut placement = Placements::default();
        record_placement(&mut placement, "name=wake_to_run_futex apic=3 samples=2000").unwrap();
        assert!(
            record_placement(&mut placement, "name=wake_to_run_futex apic=3 samples=1").is_err()
        );
        for line in [
            "name=wrong apic=0 samples=1",
            "name=smp_balance_futex apic=4294967296 samples=1",
            "name=smp_balance_futex apic=0 samples=0",
            "name=smp_balance_futex apic=0 samples=1 samples=2",
        ] {
            assert!(record_placement(&mut Placements::default(), line).is_err());
        }
        // Failed duplicate insertion is not admitted as a valid transcript.
        let mut placement = Placements::default();
        record_placement(&mut placement, "name=syscall_getuid apic=0 samples=2000").unwrap_err();
        let run = super::super::parse_log(&log()).unwrap();
        record_placement(&mut placement, "name=wake_to_run_futex apic=0 samples=2000").unwrap();
        assert!(validate_placements(&placement, &run.results).is_err());
    }

    #[test]
    fn tail_boundary_is_inclusive_without_changing_any_raw_result() {
        let log = log();
        let run = super::super::parse_log(&log).unwrap();
        assert!(!render_quality(&run).contains("FAIL"));
        let run = super::super::parse_log(&log.replace("p99=150", "p99=151")).unwrap();
        assert!(render_quality(&run).contains("FAIL"));
        assert_eq!(run.results[1].p99, 151);
        assert_eq!(run.results[1].max, 500);
    }
}
