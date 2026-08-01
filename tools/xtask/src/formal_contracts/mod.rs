mod evidence;
mod profiles;
mod registry;
mod runtime_trace;

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::Result;
use crate::cli::FormalContractsCommand;

#[cfg(test)]
pub(crate) use evidence::validated_smp_launch_evidence_for_tests;
pub(crate) use evidence::{ValidatedSmpLaunchEvidence, validate_smp_launch_evidence};
pub(crate) use registry::{ContractImpact, ContractRegistry};
pub(crate) use runtime_trace::{KvmRuntimeObservation, record_kvm_runtime_trace};

pub(crate) fn run(root: &Path, command: &FormalContractsCommand) -> Result<()> {
    match command {
        FormalContractsCommand::Check => {
            let registry = ContractRegistry::load(root)?;
            registry.validate(root)?;
            registry.check_generated_doc(root)?;
            println!(
                "xtask: formal contracts passed models={} flows={} transitions={} witnesses={}",
                registry.models.len(),
                registry.flow_count(),
                registry.transitions.len(),
                registry.witnesses.len()
            );
            Ok(())
        }
        FormalContractsCommand::Generate => {
            let registry = ContractRegistry::load(root)?;
            registry.validate(root)?;
            registry.write_generated_doc(root)?;
            println!(
                "xtask: generated {}",
                registry.manifest.generated_doc.display()
            );
            Ok(())
        }
        FormalContractsCommand::Impact { base, paths } => {
            let registry = ContractRegistry::load(root)?;
            registry.validate(root)?;
            if base.is_some() && !paths.is_empty() {
                anyhow::bail!("formal impact accepts either --base or explicit paths, not both");
            }
            let paths = if let Some(base) = base {
                changed_paths_since(root, base)?
            } else if paths.is_empty() {
                changed_worktree_paths(root)?
            } else {
                paths.clone()
            };
            let impact = registry.impact(&paths);
            print_impact(&impact);
            if !impact.unmapped_high_risk.is_empty() {
                anyhow::bail!(
                    "high-risk changed paths lack a formal contract mapping: {}",
                    impact
                        .unmapped_high_risk
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(())
        }
        FormalContractsCommand::Evidence {
            profile,
            topology,
            allow_dirty,
            sign,
        } => evidence::write_evidence(root, profile, topology, *allow_dirty, *sign),
    }
}

pub(crate) fn load_impact(root: &Path, paths: &[PathBuf]) -> Result<ContractImpact> {
    let registry = ContractRegistry::load(root)?;
    registry.validate(root)?;
    Ok(registry.impact(paths))
}

fn print_impact(impact: &ContractImpact) {
    println!("formal-impact-models={}", impact.models.len());
    for model in &impact.models {
        println!("  bash formal/run-tlc.sh {model}");
    }
    for witness in &impact.witnesses {
        println!(
            "  cargo test -q -p {} {} -- --exact",
            witness.package, witness.test
        );
    }
    for path in &impact.unmapped_high_risk {
        println!("unmapped-high-risk={}", path.display());
    }
}

fn changed_worktree_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .context("query changed paths for formal impact")?;
    if !output.status.success() {
        anyhow::bail!("git status failed while computing formal impact");
    }
    let mut paths = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.len() < 4 {
            continue;
        }
        let raw = std::str::from_utf8(&record[3..])
            .context("formal impact does not accept non-UTF-8 paths")?;
        let path = raw
            .rsplit_once(" -> ")
            .map_or(raw, |(_, destination)| destination);
        paths.push(PathBuf::from(path));
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn changed_paths_since(root: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let revision = format!("{base}^{{commit}}");
    let resolved = std::process::Command::new("git")
        .args(["rev-parse", "--verify", &revision])
        .current_dir(root)
        .output()
        .context("resolve formal impact base revision")?;
    if !resolved.status.success() {
        anyhow::bail!("formal impact base is not a commit in this checkout: {base}");
    }
    let base_commit = String::from_utf8(resolved.stdout)?.trim().to_owned();
    let range = format!("{base_commit}...HEAD");
    let output = std::process::Command::new("git")
        .args([
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACMR",
            &range,
            "--",
        ])
        .current_dir(root)
        .output()
        .context("query committed paths for formal impact")?;
    if !output.status.success() {
        anyhow::bail!("git diff failed while computing formal impact");
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(PathBuf::from)
                .context("formal impact does not accept non-UTF-8 paths")
        })
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}
