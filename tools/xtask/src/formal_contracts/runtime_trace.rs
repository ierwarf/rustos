use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::Serialize;

use super::registry::{ContractRegistry, ProductScenarioStep};
use crate::Result;

#[derive(Clone, Copy, Debug)]
pub(crate) struct KvmRuntimeObservation {
    pub elapsed_ms: u64,
    pub storage: bool,
    pub input: bool,
    pub display: bool,
    pub network: bool,
    pub ui_budget: bool,
    pub storage_only: bool,
    /// Whether a step that lands after its absolute deadline fails the run.
    ///
    /// The interactive lane is operator-owned and, by its own documented
    /// contract, does not terminate on a boot-to-UI deadline: a developer
    /// pausing at a breakpoint or a cold host cache is not a product
    /// regression. It still records every observed timestamp, so the
    /// measurement stays available; only the acceptance lanes enforce it.
    pub enforce_deadlines: bool,
}

#[derive(Serialize)]
struct RuntimeEvent<'a> {
    schema: &'static str,
    run_id: u64,
    topology: &'a str,
    scenario: &'a str,
    sequence: usize,
    step: &'a str,
    flow: &'a str,
    transition: &'a str,
    model: &'a str,
    log: &'a str,
    marker: &'a str,
    requires: &'a [String],
    outcome: &'static str,
    guest_ts_us: u64,
    elapsed_ms: u64,
    deadline_ms: u64,
    source_line: usize,
    host_run_elapsed_ms: u64,
    network_exercised: bool,
    fps_proof: bool,
    source_tree_sha256: &'a str,
    rustos_boot_image_sha256: &'a str,
    dvm_manifest_sha256: &'a str,
}

pub(crate) fn record_kvm_runtime_trace(
    root: &Path,
    observation: KvmRuntimeObservation,
    rustos_log_path: &Path,
    dvm_log_path: &Path,
) -> Result<()> {
    let topology = if observation.storage_only {
        "storage-dvm"
    } else if observation.storage && observation.display {
        "qemu-commercial"
    } else {
        "qemu-control"
    };
    if topology == "qemu-commercial" && !observation.input {
        bail!("commercial KVM runtime evidence lacks the authenticated input topology");
    }

    let registry = ContractRegistry::load(root)?;
    registry.validate(root)?;
    let source_tree_sha256 = super::evidence::source_tree_hash(root)?;
    let rustos_boot_image_sha256 = super::evidence::hash_file(&root.join("build/rustos-boot.img"))
        .context("hash launched RustOS boot image for runtime evidence")?;
    let dvm_manifest_sha256 = super::evidence::hash_file(
        &root.join("driver-domains/linux/out/artifacts/rustos-linux-dvm-x86_64.manifest"),
    )
    .context("hash verified Linux DVM manifest for runtime evidence")?;
    let topology_contract = registry
        .manifest
        .topologies
        .get(topology)
        .with_context(|| format!("runtime trace uses unregistered topology {topology}"))?;
    let mut steps = registry
        .scenarios
        .iter()
        .filter(|step| {
            step.topology == topology && step.scenario == topology_contract.runtime_scenario
        })
        .collect::<Vec<_>>();
    steps.sort_by_key(|step| step.sequence);
    if steps.is_empty() {
        bail!(
            "runtime trace topology {topology} has no scenario {}",
            topology_contract.runtime_scenario
        );
    }

    let rustos_log = fs::read_to_string(rustos_log_path)
        .with_context(|| format!("read {}", rustos_log_path.display()))?;
    let dvm_log = fs::read_to_string(dvm_log_path)
        .with_context(|| format!("read {}", dvm_log_path.display()))?;
    let rustos_lines = rustos_log.lines().collect::<Vec<_>>();
    let dvm_lines = dvm_log.lines().collect::<Vec<_>>();
    let mut observed_guest_ts_us = std::collections::BTreeMap::<&str, u64>::new();

    let run_id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let artifact_dir = root.join("build/formal/runtime-traces");
    fs::create_dir_all(&artifact_dir)?;
    // A lane that does not enforce deadlines must not overwrite the acceptance
    // artifact: `run-runtime-traces.sh` replays that file strictly, so one
    // relaxed interactive session would otherwise fail every later seal.
    let trace = artifact_dir.join(if observation.enforce_deadlines {
        "kvm-p0.jsonl"
    } else {
        "kvm-p0-interactive.jsonl"
    });
    let mut output = Vec::new();
    for step in steps {
        let (line_number, guest_ts_us) = match step.log.as_str() {
            "rustos" => find_structured_step(&rustos_lines, step)?,
            "dvm" => find_structured_step(&dvm_lines, step)?,
            other => bail!(
                "runtime trace step {} uses unsupported log {other}",
                step.step
            ),
        };
        for required in step.requires.iter().filter(|required| *required != "START") {
            let required_ts_us =
                observed_guest_ts_us
                    .get(required.as_str())
                    .with_context(|| {
                        format!(
                            "runtime trace step {} requires unobserved predecessor {required}",
                            step.step
                        )
                    })?;
            if guest_ts_us < *required_ts_us {
                bail!(
                    "runtime trace dependency moved backwards at {}: {} < {} us from {required}",
                    step.step,
                    guest_ts_us,
                    required_ts_us
                );
            }
        }
        let elapsed_ms = guest_ts_us.saturating_add(999) / 1_000;
        if elapsed_ms > step.deadline_ms {
            if observation.enforce_deadlines {
                bail!(
                    "runtime trace step {} missed its absolute deadline: {} > {} ms",
                    step.step,
                    elapsed_ms,
                    step.deadline_ms
                );
            }
            // Recorded, not enforced: the operator-owned lane reports the
            // overshoot so it stays visible without failing the session.
            println!(
                "xtask: runtime trace step {} landed after its absolute deadline: {} > {} ms (not enforced on the interactive lane)",
                step.step, elapsed_ms, step.deadline_ms
            );
        }
        observed_guest_ts_us.insert(step.step.as_str(), guest_ts_us);
        let event = RuntimeEvent {
            schema: "rustos-formal-runtime-event-v4",
            run_id,
            topology,
            scenario: step.scenario.as_str(),
            sequence: step.sequence,
            step: step.step.as_str(),
            flow: step.flow.as_str(),
            transition: step.transition.as_str(),
            model: step.model.as_str(),
            log: step.log.as_str(),
            marker: step.marker.as_str(),
            requires: &step.requires,
            outcome: "success",
            guest_ts_us,
            elapsed_ms,
            deadline_ms: step.deadline_ms,
            source_line: line_number,
            host_run_elapsed_ms: observation.elapsed_ms,
            network_exercised: observation.network,
            fps_proof: observation.ui_budget,
            source_tree_sha256: &source_tree_sha256,
            rustos_boot_image_sha256: &rustos_boot_image_sha256,
            dvm_manifest_sha256: &dvm_manifest_sha256,
        };
        serde_json::to_writer(&mut output, &event)?;
        output.push(b'\n');
    }
    fs::write(&trace, output)?;

    let checker = root.join("formal/check-kvm-runtime-trace.py");
    let summary = artifact_dir.join(if observation.enforce_deadlines {
        "kvm-p0-summary.json"
    } else {
        "kvm-p0-interactive-summary.json"
    });
    let mut command = Command::new("python3");
    command.arg(checker).arg(&trace);
    if !observation.enforce_deadlines {
        // The replay must apply the same rule the recorder just did, or the
        // steps it let through would be rejected here and the trace would be
        // shorter than its scenario.
        command.arg("--deadlines-advisory");
    }
    let status = command
        .args(["--registry"])
        .arg(root.join("formal/product-scenarios.tsv"))
        .args(["--root"])
        .arg(root)
        .args(["--topology", topology, "--summary"])
        .arg(&summary)
        .status()
        .context("replay KVM P0 runtime trace")?;
    if !status.success() {
        bail!("KVM P0 runtime trace did not conform to its product scenario");
    }
    Ok(())
}

fn find_structured_step(lines: &[&str], step: &ProductScenarioStep) -> Result<(usize, u64)> {
    for (offset, line) in lines.iter().enumerate() {
        if !line.contains(step.marker.as_str()) {
            continue;
        }
        let guest_ts_us = structured_timestamp_us(line).with_context(|| {
            format!(
                "runtime marker {} for step {} lacks a kernel-stamped ts_us field",
                step.marker, step.step
            )
        })?;
        return Ok((offset + 1, guest_ts_us));
    }
    bail!(
        "runtime trace is missing scenario marker {} for step {}",
        step.marker,
        step.step
    )
}

fn structured_timestamp_us(line: &str) -> Option<u64> {
    line.split_ascii_whitespace().find_map(|field| {
        field
            .strip_prefix("ts_us=")
            .and_then(|value| value.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::structured_timestamp_us;

    #[test]
    fn product_trace_accepts_only_kernel_timestamped_records() {
        assert_eq!(
            structured_timestamp_us(
                "seq=7 ts_us=4999123 tick=9 lvl=info msg=\"name=product-first-frame\""
            ),
            Some(4_999_123)
        );
        assert_eq!(
            structured_timestamp_us("wayclick: first frame presented"),
            None
        );
    }
}
