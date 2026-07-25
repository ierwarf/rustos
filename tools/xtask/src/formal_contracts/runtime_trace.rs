use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::Serialize;

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
}

#[derive(Serialize)]
struct RuntimeEvent<'a> {
    schema: &'static str,
    run_id: u64,
    topology: &'a str,
    sequence: usize,
    model: &'a str,
    action: &'a str,
    outcome: &'static str,
    elapsed_ms: u64,
}

pub(crate) fn record_kvm_runtime_trace(
    root: &Path,
    observation: KvmRuntimeObservation,
) -> Result<()> {
    let topology = if observation.storage_only {
        "storage-dvm"
    } else {
        "qemu-commercial"
    };
    let run_id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let mut actions = vec![("rootd-bootstrap/RootdBootstrap", "CoreServicesReady")];
    if observation.input {
        actions.push((
            "input-ingestion-worker/InputIngestionWorker",
            "AuthenticatedRelayReady",
        ));
    }
    if observation.storage {
        actions.push((
            "dvm-block-startup/DvmBlockStartup",
            "GenerationBoundDataPlaneProven",
        ));
    }
    if observation.display {
        actions.push((
            "dvm-display-readiness/DvmDisplayReadiness",
            "GenerationBoundSurfaceReady",
        ));
    }
    if observation.network {
        actions.push((
            "network-payload-session/NetworkPayloadSession",
            "AuthenticatedRoundTrip",
        ));
    }
    if observation.ui_budget {
        actions.push(("ui-frame-budget/UiFrameBudget", "FrameBudgetSatisfied"));
    }
    let artifact_dir = root.join("build/formal/runtime-traces");
    fs::create_dir_all(&artifact_dir)?;
    let trace = artifact_dir.join("kvm-p0.jsonl");
    let mut output = Vec::new();
    for (sequence, (model, action)) in actions.into_iter().enumerate() {
        let event = RuntimeEvent {
            schema: "rustos-formal-runtime-event-v1",
            run_id,
            topology,
            sequence,
            model,
            action,
            outcome: "success",
            elapsed_ms: observation.elapsed_ms,
        };
        serde_json::to_writer(&mut output, &event)?;
        output.push(b'\n');
    }
    fs::write(&trace, output)?;
    let checker = root.join("formal/check-kvm-runtime-trace.py");
    let summary = artifact_dir.join("kvm-p0-summary.json");
    let status = Command::new("python3")
        .arg(checker)
        .arg(&trace)
        .args(["--summary"])
        .arg(&summary)
        .status()
        .context("replay KVM P0 runtime trace")?;
    if !status.success() {
        bail!("KVM P0 runtime trace did not conform to the registered model actions");
    }
    Ok(())
}
