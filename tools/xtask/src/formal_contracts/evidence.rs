use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::ContractRegistry;
use crate::Result;

const DEFAULT_GRUB_DEV_KEY: &str = "RustOS Dev GRUB <rustos-dev-grub@example.invalid>";

pub(crate) struct ValidatedSmpLaunchEvidence {
    _private: (),
}

pub(crate) fn validate_smp_launch_evidence(
    root: &Path,
    profile_name: &str,
) -> Result<ValidatedSmpLaunchEvidence> {
    let registry = ContractRegistry::load(root)?;
    registry.validate(root)?;
    registry.check_generated_doc(root)?;
    let profile = registry
        .manifest
        .profiles
        .get(profile_name)
        .context("formal registry lacks the SMP launch evidence profile")?;
    let source_tree_sha256 = source_tree_hash(root)?;
    let verification_run = root.join(format!("build/formal/verification-run/{profile_name}.json"));
    validate_verification_run(root, profile_name, &source_tree_sha256, &verification_run)?;
    let generated_unix = evidence_mtime(verification_run)?;
    let expires_unix = generated_unix
        .checked_add(profile.evidence_max_age_hours.saturating_mul(3600))
        .context("SMP launch evidence expiry overflow")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if now > expires_unix {
        bail!(
            "SMP launch evidence profile {profile_name} expired at {expires_unix}; rerun its verification command"
        );
    }
    Ok(ValidatedSmpLaunchEvidence { _private: () })
}

/// The verification command that seals one profile against the current tree.
fn verification_command(profile_name: &str) -> Result<Vec<&'static str>> {
    match profile_name {
        "smp-iteration" => Ok(vec!["bash", "formal/verify-smp-iteration.sh"]),
        "pr" => Ok(vec!["bash", "formal/verify-all.sh", "--profile", "pr"]),
        other => bail!("formal profile {other} has no registered verification command"),
    }
}

/// Seal the profile for this tree before refusing to launch.
///
/// The gate exists so a multicore boot cannot run ahead of the models that
/// admit its topology - not to punish a developer for forgetting a command.
/// When the seal is merely absent, expired, or bound to an older tree, the old
/// failure said "rerun its verification command"; running that exact command
/// and re-checking is strictly the same gate, because admission still comes
/// only from the second validation over the freshly written evidence. A
/// verification that fails, or that leaves the profile unsealed, still refuses
/// the launch.
pub(crate) fn ensure_smp_launch_evidence(root: &Path, profile_name: &str) -> Result<()> {
    let stale = match validate_smp_launch_evidence(root, profile_name) {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };
    let command = verification_command(profile_name)?;
    let printable = command.join(" ");
    println!(
        "xtask: formal profile {profile_name} is not sealed for this tree ({stale:#}); running `{printable}`"
    );
    let status = Command::new(command[0])
        .args(&command[1..])
        .current_dir(root)
        .status()
        .with_context(|| format!("run formal verification `{printable}`"))?;
    if !status.success() {
        bail!("formal verification `{printable}` failed; refusing the KVM launch");
    }
    validate_smp_launch_evidence(root, profile_name).with_context(|| {
        format!("formal profile {profile_name} is still unsealed after `{printable}`")
    })?;
    println!("xtask: formal profile {profile_name} sealed by `{printable}`");
    Ok(())
}

#[cfg(test)]
pub(crate) fn validated_smp_launch_evidence_for_tests() -> ValidatedSmpLaunchEvidence {
    ValidatedSmpLaunchEvidence { _private: () }
}

#[derive(Serialize)]
struct EvidenceManifest {
    schema: &'static str,
    profile: String,
    topology: String,
    generated_unix: u64,
    expires_unix: u64,
    git_commit: String,
    dirty: bool,
    source_tree_sha256: String,
    contract_registry_sha256: String,
    signer_fingerprint: Option<String>,
    artifacts: Vec<EvidenceArtifact>,
    required_runtime_models: Vec<String>,
    metadata: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct EvidenceArtifact {
    path: String,
    sha256: String,
    bytes: u64,
}

pub(super) fn write_evidence(
    root: &Path,
    profile: &str,
    topology: &str,
    allow_dirty: bool,
    sign: bool,
) -> Result<()> {
    let registry = ContractRegistry::load(root)?;
    registry.validate(root)?;
    registry.check_generated_doc(root)?;
    let profile_contract = registry
        .manifest
        .profiles
        .get(profile)
        .with_context(|| format!("unknown formal evidence profile {profile}"))?;
    let topology_contract = registry
        .manifest
        .topologies
        .get(topology)
        .with_context(|| format!("unknown formal evidence topology {topology}"))?;
    validate_runtime_trace(
        root,
        topology,
        topology_contract.runtime_scenario.as_str(),
        &topology_contract.required_runtime_models,
    )?;
    let git_commit = git_output(root, &["rev-parse", "HEAD"])?;
    let dirty = !git_output(root, &["status", "--porcelain=v1"])?.is_empty();
    if dirty && !allow_dirty {
        bail!(
            "refusing release evidence for a dirty worktree; pass --allow-dirty for development evidence"
        );
    }
    let generated_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let source_tree_sha256 = source_tree_hash(root)?;
    let contract_registry_sha256 = hash_files(
        root,
        [
            Path::new("formal/contracts.toml"),
            registry.manifest.models.as_path(),
            registry.manifest.model_bindings.as_path(),
            registry.manifest.flows.as_path(),
            registry.manifest.scenarios.as_path(),
            registry.manifest.witnesses.as_path(),
            registry.manifest.generated_doc.as_path(),
        ],
    )?;
    let mut verification_paths = profile_contract
        .required_evidence
        .iter()
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    let verification_run = root.join(format!("build/formal/verification-run/{profile}.json"));
    validate_verification_run(root, profile, &source_tree_sha256, &verification_run)?;
    verification_paths.push(verification_run);
    for path in &verification_paths {
        validate_passed_evidence(path)?;
    }
    for model in registry.models.values() {
        let path = root.join(format!(
            "build/formal/tlc/{profile}/{}/summary.json",
            model.name.replace('/', "__")
        ));
        validate_tlc_evidence(root, model.name.as_str(), &path)?;
        verification_paths.push(path);
    }
    let mut artifact_paths = verification_paths.clone();
    for relative in &topology_contract.required_artifacts {
        let path = root.join(relative);
        if !path.is_file() {
            bail!(
                "topology {topology} requires missing binary artifact {}",
                relative.display()
            );
        }
        artifact_paths.push(path);
    }
    artifact_paths.sort();
    artifact_paths.dedup();
    let oldest_evidence_unix = verification_paths
        .iter()
        .cloned()
        .map(evidence_mtime)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min()
        .context("evidence profile contains no verification artifacts")?;
    let expires_unix = oldest_evidence_unix
        .checked_add(profile_contract.evidence_max_age_hours.saturating_mul(3600))
        .context("formal evidence expiry overflow")?;
    if generated_unix > expires_unix {
        bail!("oldest verification evidence expired at {expires_unix}; rerun profile {profile}");
    }
    let artifacts = artifact_paths
        .iter()
        .map(|path| evidence_artifact(root, path))
        .collect::<Result<Vec<_>>>()?;

    let signer_fingerprint = if sign {
        Some(signing_fingerprint(root)?)
    } else {
        None
    };
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "claim".to_owned(),
        if dirty {
            "development-evidence".to_owned()
        } else {
            "clean-source-evidence".to_owned()
        },
    );
    let manifest = EvidenceManifest {
        schema: "rustos-commercial-evidence-v1",
        profile: profile.to_owned(),
        topology: topology.to_owned(),
        generated_unix,
        expires_unix,
        git_commit,
        dirty,
        source_tree_sha256,
        contract_registry_sha256,
        signer_fingerprint,
        artifacts,
        required_runtime_models: topology_contract.required_runtime_models.clone(),
        metadata,
    };
    let artifact_dir = root.join("build/formal/evidence");
    fs::create_dir_all(&artifact_dir)?;
    let manifest_path = artifact_dir.join(format!("{profile}-{topology}.json"));
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    fs::write(&manifest_path, [&bytes[..], b"\n"].concat())?;
    if sign {
        sign_manifest(root, &manifest_path)?;
    }
    println!(
        "xtask: formal evidence written path={} signed={sign} expires_unix={expires_unix}",
        manifest_path.display()
    );
    Ok(())
}

fn validate_runtime_trace(
    root: &Path,
    topology: &str,
    scenario: &str,
    required_models: &[String],
) -> Result<()> {
    let summary_path = root.join("build/formal/runtime-traces/kvm-p0-summary.json");
    let summary: serde_json::Value = serde_json::from_slice(
        &fs::read(&summary_path)
            .with_context(|| format!("read required {}", summary_path.display()))?,
    )?;
    if summary.get("status").and_then(serde_json::Value::as_str) != Some("passed") {
        bail!("KVM runtime trace summary is not passed");
    }
    if summary.get("schema").and_then(serde_json::Value::as_str)
        != Some("rustos-kvm-formal-trace-evidence-v5")
    {
        bail!("KVM runtime trace summary uses a stale evidence schema");
    }
    let source_hash = summary
        .get("source_tree_sha256")
        .and_then(serde_json::Value::as_str)
        .context("KVM runtime trace summary lacks its source-tree hash")?;
    if source_hash != source_tree_hash(root)? {
        bail!("KVM runtime trace was not recorded from the current source tree");
    }
    for (field, relative) in [
        ("rustos_boot_image_sha256", "build/rustos-boot.img"),
        (
            "dvm_manifest_sha256",
            "driver-domains/linux/out/artifacts/rustos-linux-dvm-x86_64.manifest",
        ),
    ] {
        let recorded = summary
            .get(field)
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("KVM runtime trace summary lacks {field}"))?;
        if recorded != hash_file(&root.join(relative))? {
            bail!("KVM runtime trace is stale for launched artifact {relative}");
        }
    }
    if summary.get("topology").and_then(serde_json::Value::as_str) != Some(topology) {
        bail!("KVM runtime trace topology does not match evidence topology {topology}");
    }
    if summary.get("scenario").and_then(serde_json::Value::as_str) != Some(scenario) {
        bail!("KVM runtime trace scenario does not match evidence topology {topology}");
    }
    let trace_path = root.join("build/formal/runtime-traces/kvm-p0.jsonl");
    let trace_hash = summary
        .get("trace_sha256")
        .and_then(serde_json::Value::as_str)
        .context("KVM runtime trace summary lacks its trace hash")?;
    if trace_hash != hash_file(&trace_path)? {
        bail!("KVM runtime trace summary does not bind the current trace");
    }
    let scenario_path = root.join("formal/product-scenarios.tsv");
    let scenario_hash = summary
        .get("scenario_registry_sha256")
        .and_then(serde_json::Value::as_str)
        .context("KVM runtime trace summary lacks its scenario registry hash")?;
    if scenario_hash != hash_file(&scenario_path)? {
        bail!("KVM runtime trace summary is stale for the product scenario registry");
    }
    let models = summary
        .get("models")
        .and_then(serde_json::Value::as_array)
        .context("KVM runtime trace summary has no model set")?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let missing = required_models
        .iter()
        .filter(|model| !models.contains(model.as_str()))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "KVM runtime trace lacks topology-required models: {}",
            missing
                .iter()
                .map(|model| model.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn evidence_artifact(root: &Path, path: &Path) -> Result<EvidenceArtifact> {
    let metadata = fs::metadata(path)?;
    let relative = path.strip_prefix(root).unwrap_or(path);
    Ok(EvidenceArtifact {
        path: relative.to_string_lossy().into_owned(),
        sha256: hash_file(path)?,
        bytes: metadata.len(),
    })
}

fn validate_passed_evidence(path: &Path) -> Result<()> {
    let bytes = fs::read(path)
        .with_context(|| format!("read required formal evidence {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse required formal evidence {}", path.display()))?;
    if value.get("status").and_then(serde_json::Value::as_str) != Some("passed") {
        bail!("required formal evidence is not passed: {}", path.display());
    }
    Ok(())
}

fn validate_verification_run(
    root: &Path,
    profile: &str,
    source_tree_sha256: &str,
    path: &Path,
) -> Result<()> {
    validate_passed_evidence(path)?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let observed_schema = value.get("schema").and_then(serde_json::Value::as_str);
    let observed_profile = value.get("profile").and_then(serde_json::Value::as_str);
    let observed_source = value
        .get("source_tree_sha256")
        .and_then(serde_json::Value::as_str);
    if observed_schema != Some("rustos-formal-verification-run-v1")
        || observed_profile != Some(profile)
        || observed_source != Some(source_tree_sha256)
    {
        bail!(
            "formal verification run binding mismatch: schema={observed_schema:?} \
             profile={observed_profile:?} expected_profile={profile:?} \
             source={observed_source:?} expected_source={source_tree_sha256}"
        );
    }
    let artifacts = value
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .context("formal verification run lacks its artifact set")?;
    if artifacts.is_empty() {
        bail!("formal verification run has an empty artifact set");
    }
    let mut observed = std::collections::BTreeSet::new();
    for artifact in artifacts {
        let relative = artifact
            .get("path")
            .and_then(serde_json::Value::as_str)
            .context("formal verification run artifact lacks path")?;
        if !observed.insert(relative) {
            bail!("formal verification run repeats artifact {relative}");
        }
        let recorded = artifact
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .context("formal verification run artifact lacks hash")?;
        if recorded != hash_file(&root.join(relative))? {
            bail!("formal verification run artifact is stale: {relative}");
        }
    }
    Ok(())
}

fn validate_tlc_evidence(root: &Path, model: &str, path: &Path) -> Result<()> {
    validate_passed_evidence(path)?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    if value.get("model").and_then(serde_json::Value::as_str) != Some(model) {
        bail!("TLC evidence model identity mismatch: {}", path.display());
    }
    let inputs = value
        .get("inputs")
        .and_then(serde_json::Value::as_object)
        .with_context(|| format!("TLC evidence lacks input hashes: {}", path.display()))?;
    let spec = root.join(format!("formal/{model}.tla"));
    let config = root.join(format!("formal/{model}.cfg"));
    for (field, source) in [("spec_sha256", spec), ("config_sha256", config)] {
        let recorded = inputs
            .get(field)
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("TLC evidence lacks {field}: {}", path.display()))?;
        if recorded != hash_file(&source)? {
            bail!(
                "TLC evidence is stale for {} input {}",
                model,
                source.display()
            );
        }
    }
    Ok(())
}

fn evidence_mtime(path: PathBuf) -> Result<u64> {
    Ok(fs::metadata(&path)
        .with_context(|| format!("stat evidence {}", path.display()))?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs())
}

/// Paths the verification-run binding deliberately does not cover.
///
/// The binding says a sealed result corresponds to this exact tree. A file no
/// lane reads is not an input to that result, so hashing it does not strengthen
/// the claim -- it only makes the seal stale for an edit that cannot change any
/// answer, and the binding is a precondition for `cargo xtask bench`. The list
/// lives in `formal/binding-exempt-paths.txt`, which is itself tracked and
/// therefore inside this hash, and `formal/check-binding-exemptions.py` proves
/// no file under `formal/` or `tools/` mentions an exempt path.
fn binding_exempt_paths(root: &Path) -> Result<Vec<String>> {
    let list = root.join("formal/binding-exempt-paths.txt");
    let Ok(text) = fs::read_to_string(&list) else {
        // An absent list is an empty list: the binding then covers everything,
        // which is the conservative direction.
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

pub(super) fn source_tree_hash(root: &Path) -> Result<String> {
    let exempt = binding_exempt_paths(root)?;
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .context("list source tree for evidence")?;
    if !output.status.success() {
        bail!("git ls-files failed while hashing source tree");
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(str::to_owned)
                .context("source tree contains a non-UTF-8 path")
        })
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths.retain(|relative| !exempt.iter().any(|entry| entry == relative));
    let mut hasher = Sha256::new();
    for relative in paths {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(
            fs::read(root.join(&relative)).with_context(|| format!("hash source {relative}"))?,
        );
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_files<'a>(root: &Path, paths: impl IntoIterator<Item = &'a Path>) -> Result<String> {
    let mut hasher = Sha256::new();
    for relative in paths {
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(fs::read(root.join(relative))?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn signing_material(root: &Path) -> (OsString, PathBuf, String) {
    let gpg = std::env::var_os("GPG").unwrap_or_else(|| OsString::from("gpg"));
    let home = std::env::var_os("RUSTOS_GPG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("build/dev-grub-gpg"));
    let key = std::env::var("RUSTOS_GRUB_SIGNING_KEY")
        .unwrap_or_else(|_| DEFAULT_GRUB_DEV_KEY.to_owned());
    (gpg, home, key)
}

fn signing_fingerprint(root: &Path) -> Result<String> {
    let (gpg, home, key) = signing_material(root);
    let output = Command::new(gpg)
        .args(["--homedir"])
        .arg(home)
        .args([
            "--batch",
            "--with-colons",
            "--fingerprint",
            "--list-secret-keys",
        ])
        .arg(key)
        .output()
        .context("query formal evidence signing key")?;
    if !output.status.success() {
        bail!("formal evidence signing key is unavailable; build the signed RustOS image first");
    }
    String::from_utf8(output.stdout)?
        .lines()
        .find_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            (fields.first() == Some(&"fpr"))
                .then(|| fields.get(9).copied())
                .flatten()
                .map(str::to_owned)
        })
        .context("formal evidence signing key has no fingerprint")
}

fn sign_manifest(root: &Path, manifest: &Path) -> Result<()> {
    let (gpg, home, key) = signing_material(root);
    let signature = manifest.with_extension("json.sig");
    if signature.exists() {
        fs::remove_file(&signature)?;
    }
    let status = Command::new(&gpg)
        .arg("--homedir")
        .arg(&home)
        .args([
            "--batch",
            "--yes",
            "--pinentry-mode",
            "loopback",
            "--local-user",
        ])
        .arg(&key)
        .args(["--detach-sign", "--output"])
        .arg(&signature)
        .arg(manifest)
        .status()
        .context("sign formal evidence manifest")?;
    if !status.success() {
        bail!("formal evidence manifest signing failed");
    }
    let verify = Command::new(gpg)
        .arg("--homedir")
        .arg(home)
        .args(["--batch", "--verify"])
        .arg(&signature)
        .arg(manifest)
        .status()
        .context("verify formal evidence manifest signature")?;
    if !verify.success() {
        bail!("formal evidence signature readback failed");
    }
    Ok(())
}
