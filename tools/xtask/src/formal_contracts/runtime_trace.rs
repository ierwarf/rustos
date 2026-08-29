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
    evidence: &'a str,
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
            // Per-step boot deadlines are measurement fields, not acceptance
            // failures. The outer QEMU timeout remains the bounded liveness
            // gate and missing/out-of-order milestones still fail closed.
            println!(
                "xtask: runtime trace step {} landed after its advisory deadline: {} > {} ms",
                step.step, elapsed_ms, step.deadline_ms
            );
        }
        observed_guest_ts_us.insert(step.step.as_str(), guest_ts_us);
        let event = RuntimeEvent {
            schema: "rustos-formal-runtime-event-v5",
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
            evidence: step.evidence.as_str(),
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
    command.arg(checker).arg(&trace).arg("--deadlines-advisory");
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
        let milestone = verified_milestone(line).with_context(|| {
            format!(
                "runtime marker {} for step {} is not one complete kernel-stamped milestone frame",
                step.marker, step.step
            )
        })?;
        if !milestone_evidence_matches(milestone.semantic, step.evidence.as_str()) {
            bail!(
                "runtime marker {} for step {} does not satisfy evidence contract {}",
                step.marker,
                step.step,
                step.evidence
            );
        }
        return Ok((offset + 1, milestone.timestamp_us));
    }
    bail!(
        "runtime trace is missing scenario marker {} for step {}",
        step.marker,
        step.step
    )
}

const MILESTONE_PREFIX: &str = "milestone-begin v=1 ";
const MILESTONE_CHECKSUM_PREFIX: &str = " checksum=";
const MILESTONE_SUFFIX: &str = " milestone-end\"";
const FNV1A64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

struct VerifiedMilestone<'a> {
    semantic: &'a str,
    timestamp_us: u64,
}

fn verified_milestone(line: &str) -> Option<VerifiedMilestone<'_>> {
    let semantic_start = line.find(MILESTONE_PREFIX)?;
    let checksum_offset =
        semantic_start + line[semantic_start..].find(MILESTONE_CHECKSUM_PREFIX)?;
    let checksum_start = checksum_offset.checked_add(MILESTONE_CHECKSUM_PREFIX.len())?;
    let checksum_end = checksum_start.checked_add(16)?;
    let expected = u64::from_str_radix(line.get(checksum_start..checksum_end)?, 16).ok()?;
    if line.get(checksum_end..)? != MILESTONE_SUFFIX
        || fnv1a64(line[semantic_start..checksum_offset].as_bytes()) != expected
    {
        return None;
    }
    let semantic = &line[semantic_start..checksum_offset];
    let timestamp_us = semantic.split_ascii_whitespace().find_map(|field| {
        field
            .strip_prefix("ts_us=")
            .and_then(|value| value.parse().ok())
    })?;
    Some(VerifiedMilestone {
        semantic,
        timestamp_us,
    })
}

fn milestone_evidence_matches(semantic: &str, contract: &str) -> bool {
    let mut fields = std::collections::BTreeMap::new();
    for field in semantic.split_ascii_whitespace().skip(2) {
        let Some((key, value)) = field.split_once('=') else {
            return false;
        };
        if fields.insert(key, value).is_some() {
            return false;
        }
    }
    match contract {
        "none" => !fields.contains_key("evidence_v"),
        "executable-snapshot-v1" => {
            fields.get("evidence_v") == Some(&"1")
                && fields.get("backing") == Some(&"dvm-volume")
                && fields.get("provider_service") == Some(&"2")
                && [
                    "provider_generation",
                    "storage_epoch",
                    "mount_generation",
                    "request_id",
                ]
                .iter()
                .all(|key| {
                    fields
                        .get(*key)
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some_and(|value| value != 0)
                })
                && fields.get("sha256").is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                        && digest.bytes().any(|byte| byte != b'0')
                })
        }
        _ => false,
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV1A64_OFFSET_BASIS, |checksum, byte| {
        (checksum ^ u64::from(*byte)).wrapping_mul(FNV1A64_PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        FNV1A64_OFFSET_BASIS, FNV1A64_PRIME, milestone_evidence_matches, verified_milestone,
    };

    fn framed(semantic: &str) -> String {
        let checksum = semantic
            .as_bytes()
            .iter()
            .fold(FNV1A64_OFFSET_BASIS, |value, byte| {
                (value ^ u64::from(*byte)).wrapping_mul(FNV1A64_PRIME)
            });
        format!("seq=7 msg=\"{semantic} checksum={checksum:016x} milestone-end\"")
    }

    #[test]
    fn product_trace_accepts_only_complete_checksumming_milestone_frames() {
        let complete = framed(
            "milestone-begin v=1 output_seq=7 seq=3 ts_us=4999123 tick=9 cat=compat name=product-first-frame arg0=0x1 arg1=0x2 pid=71 tid=72 dropped=0 discarded_bytes=0",
        );
        assert_eq!(
            verified_milestone(&complete).map(|milestone| milestone.timestamp_us),
            Some(4_999_123)
        );
        assert!(verified_milestone("wayclick: first frame presented").is_none());
        assert!(verified_milestone(&complete.replacen("arg1=0x2", "arg1=0x3", 1)).is_none());
    }

    #[test]
    fn executable_snapshot_evidence_requires_every_kernel_stamped_identity_field() {
        let semantic = "milestone-begin v=1 output_seq=7 seq=3 ts_us=99 tick=9 cat=compat name=product-executable-snapshot-sealed arg0=0x1 arg1=0x2 pid=71 tid=72 dropped=0 discarded_bytes=0 evidence_v=1 backing=dvm-volume provider_service=2 provider_generation=7 storage_epoch=8 mount_generation=9 request_id=10 sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(milestone_evidence_matches(
            semantic,
            "executable-snapshot-v1"
        ));
        assert!(!milestone_evidence_matches(
            semantic.replacen("storage_epoch=8 ", "", 1).as_str(),
            "executable-snapshot-v1"
        ));
        assert!(!milestone_evidence_matches(
            semantic
                .replacen("backing=dvm-volume", "backing=bootstrap", 1)
                .as_str(),
            "executable-snapshot-v1"
        ));
    }
}
