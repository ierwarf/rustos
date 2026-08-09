//! SMP Ring3 qualification success-record schema, provenance, and archival.
//!
//! Owner: xtask KVM qualification evidence. This private module seals a
//! pre-launch artifact snapshot, then publishes one non-overwriting v5 record
//! and its exact debugcon, DVM serial, and injected contract bytes. The parent
//! retains launch admission and source-conformance anchors. Evidence:
//! `kvm::write_kvm_success_summary` and the `smp-ring3-qualification` gate.

use super::{
    Config, DvmArtifacts, KvmFailureLog, KvmLayout, SmokeOptions, SmpQualificationEvent,
    SmpRuntimeEvent, read_private_early_system_image, read_private_smp_ring3_qualification_contract,
    render_smp_ring3_qualification_contract,
};
use anyhow::{Context, Result, bail};
use boot_protocol::{
    EARLY_SYSTEM_ENTRY_BYTES, EARLY_SYSTEM_HEADER_BYTES, EarlySystemEntry, EarlySystemHeader,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize)]
pub(super) struct KvmSuccessArtifact {
    pub(super) path: String,
    pub(super) bytes: u64,
    pub(super) sha256: String,
    pub(super) modified_unix_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct SmpEvidenceRun {
    pub(super) cohort: Option<String>,
    pub(super) run_id: String,
    pub(super) started_unix_ms: u64,
}

#[derive(Clone, Debug)]
pub(super) struct KvmLaunchEvidenceSnapshot {
    pub(super) run: SmpEvidenceRun,
    pub(super) source_tree_sha256: String,
    pub(super) formal_verification: KvmSuccessArtifact,
    pub(super) rustos_boot_image: KvmSuccessArtifact,
    pub(super) rustos_runtime_image: KvmSuccessArtifact,
    pub(super) dvm_attached_block_disk: Option<KvmSuccessArtifact>,
    pub(super) smpqual_early_system_executable: Option<KvmSuccessArtifact>,
    pub(super) dvm_kernel: KvmSuccessArtifact,
    pub(super) dvm_rootfs: KvmSuccessArtifact,
    pub(super) private_smp_qualification_contract: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
pub(super) struct KvmSuccessSummary {
    pub(super) schema: &'static str,
    pub(super) predecessor_schema: &'static str,
    pub(super) status: &'static str,
    pub(super) smp_evidence_cohort: Option<String>,
    pub(super) run_id: String,
    pub(super) started_unix_ms: u64,
    pub(super) rustos_vcpus: u8,
    pub(super) boot_elapsed_ms: u64,
    pub(super) formal_profile: &'static str,
    pub(super) source_tree_sha256: String,
    pub(super) formal_verification: KvmSuccessArtifact,
    pub(super) rustos_boot_image: KvmSuccessArtifact,
    pub(super) rustos_runtime_image: KvmSuccessArtifact,
    pub(super) dvm_attached_block_disk: Option<KvmSuccessArtifact>,
    pub(super) smpqual_early_system_executable: Option<KvmSuccessArtifact>,
    pub(super) dvm_kernel: KvmSuccessArtifact,
    pub(super) dvm_rootfs: KvmSuccessArtifact,
    pub(super) rustos_log: KvmFailureLog,
    pub(super) dvm_log: KvmFailureLog,
    pub(super) rustos_debugcon_archive: KvmSuccessArtifact,
    pub(super) dvm_serial_archive: KvmSuccessArtifact,
    pub(super) private_smp_qualification_contract_archive: Option<KvmSuccessArtifact>,
    pub(super) required_rustos_markers: usize,
    pub(super) required_dvm_markers: usize,
    pub(super) smp_runtime_markers: usize,
    pub(super) smp_runtime_events: Vec<SmpRuntimeEvent>,
    pub(super) smp_ring3_qualification_events: Option<Vec<SmpQualificationEvent>>,
    pub(super) ui_minimum_fps: Option<u32>,
    pub(super) ui_proof_windows: usize,
}

pub(super) struct SmpEvidenceArchive {
    root: PathBuf,
    directory: PathBuf,
    stem: String,
}

impl SmpEvidenceRun {
    pub(super) fn new(cohort: Option<&str>) -> Result<Self> {
        if let Some(cohort) = cohort {
            validate_identifier(cohort, "SMP evidence cohort")?;
        }
        let mut random = [0_u8; 16];
        fs::File::open("/dev/urandom")
            .context("open /dev/urandom for SMP evidence run ID")?
            .read_exact(&mut random)
            .context("read SMP evidence run ID")?;
        let run_id = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read SMP evidence start timestamp")?
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        Ok(Self {
            cohort: cohort.map(str::to_owned),
            run_id,
            started_unix_ms,
        })
    }
}

pub(super) fn capture_kvm_launch_evidence(
    config: &Config,
    artifacts: &DvmArtifacts,
    layout: &KvmLayout,
    options: &SmokeOptions,
) -> Result<Option<KvmLaunchEvidenceSnapshot>> {
    if !options.smp_iteration {
        return Ok(None);
    }
    crate::formal_contracts::validate_smp_launch_evidence(&config.root_dir, "smp-iteration")?;
    let formal_path = config
        .root_dir
        .join("build/formal/verification-run/smp-iteration.json");
    let formal_bytes = fs::read(&formal_path)
        .with_context(|| format!("read SMP formal evidence {}", formal_path.display()))?;
    let formal: serde_json::Value = serde_json::from_slice(&formal_bytes)?;
    let source_tree_sha256 = formal
        .get("source_tree_sha256")
        .and_then(serde_json::Value::as_str)
        .context("SMP formal evidence lacks source_tree_sha256")?
        .to_owned();
    let private_smp_qualification_contract = if options.smp_ring3_qualification {
        let actual = read_private_smp_ring3_qualification_contract(&layout.runtime_disk)?;
        let expected = render_smp_ring3_qualification_contract(options.rustos_vcpus).into_bytes();
        if actual != expected {
            bail!("private SMP qualification contract diverged before QEMU launch");
        }
        Some(actual)
    } else {
        None
    };
    let rustos_runtime_image = binary_artifact(&config.root_dir, &layout.runtime_disk)?;
    let dvm_attached_block_disk = capture_dvm_attached_block_disk(
        &config.root_dir,
        &rustos_runtime_image,
        layout.dvm_block_disk.as_deref(),
        options.smp_ring3_qualification,
    )?;
    let smpqual_early_system_executable = options
        .smp_ring3_qualification
        .then(|| capture_smpqual_early_system_executable(&layout.runtime_disk))
        .transpose()?;
    let snapshot = KvmLaunchEvidenceSnapshot {
        run: SmpEvidenceRun::new(options.smp_evidence_cohort.as_deref())?,
        source_tree_sha256,
        formal_verification: binary_artifact(&config.root_dir, &formal_path)?,
        rustos_boot_image: binary_artifact(&config.root_dir, &config.boot_disk_image)?,
        rustos_runtime_image,
        dvm_attached_block_disk,
        smpqual_early_system_executable,
        dvm_kernel: binary_artifact(&config.root_dir, &artifacts.kernel)?,
        dvm_rootfs: binary_artifact(&config.root_dir, &artifacts.rootfs)?,
        private_smp_qualification_contract,
    };
    // The DVM's private disk is copied only after all per-run contracts have
    // been injected. Re-read the source and exact DVM-attached copy here,
    // immediately before QEMU spawn, so evidence cannot name the source while
    // the DVM receives different bytes.
    verify_prelaunch_snapshot(&config.root_dir, &snapshot)?;
    Ok(Some(snapshot))
}

impl SmpEvidenceArchive {
    pub(super) fn new(
        root: &Path,
        run_dir: &Path,
        run: &SmpEvidenceRun,
        rustos_vcpus: u8,
    ) -> Result<Self> {
        validate_identifier(&run.run_id, "SMP evidence run ID")?;
        let cohort_dir = match run.cohort.as_deref() {
            Some(cohort) => {
                validate_identifier(cohort, "SMP evidence cohort")?;
                format!("cohort-{cohort}")
            }
            None => "unqualified".to_owned(),
        };
        let directory = run_dir.join("smp-evidence").join(cohort_dir);
        fs::create_dir_all(&directory)
            .with_context(|| format!("create SMP evidence archive {}", directory.display()))?;
        let stem = format!("vcpu-{rustos_vcpus}-run-{}", run.run_id);
        Ok(Self {
            root: root.to_path_buf(),
            directory,
            stem,
        })
    }

    pub(super) fn artifact_for_bytes(&self, suffix: &str, bytes: &[u8]) -> KvmSuccessArtifact {
        let path = self.path_for(suffix);
        KvmSuccessArtifact {
            path: path
                .strip_prefix(&self.root)
                .unwrap_or(&path)
                .display()
                .to_string(),
            bytes: bytes.len().try_into().unwrap_or(u64::MAX),
            sha256: sha256(bytes),
            modified_unix_ms: None,
        }
    }

    fn path_for(&self, suffix: &str) -> PathBuf {
        self.directory.join(format!("{}.{}", self.stem, suffix))
    }
}

pub(super) fn binary_artifact(root: &Path, path: &Path) -> Result<KvmSuccessArtifact> {
    let bytes =
        fs::read(path).with_context(|| format!("read evidence artifact {}", path.display()))?;
    let modified_unix_ms = fs::metadata(path)
        .with_context(|| format!("stat evidence artifact {}", path.display()))?
        .modified()
        .with_context(|| format!("read evidence artifact timestamp {}", path.display()))?
        .duration_since(UNIX_EPOCH)
        .context("evidence artifact timestamp predates Unix epoch")?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    Ok(KvmSuccessArtifact {
        path: path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string(),
        bytes: bytes.len().try_into().unwrap_or(u64::MAX),
        sha256: sha256(&bytes),
        modified_unix_ms: Some(modified_unix_ms),
    })
}

pub(super) fn verify_prelaunch_snapshot(
    root: &Path,
    snapshot: &KvmLaunchEvidenceSnapshot,
) -> Result<()> {
    for artifact in [
        &snapshot.formal_verification,
        &snapshot.rustos_boot_image,
        &snapshot.rustos_runtime_image,
        &snapshot.dvm_kernel,
        &snapshot.dvm_rootfs,
    ] {
        verify_artifact_unchanged(root, artifact)?;
    }
    if let Some(dvm_attached_block_disk) = snapshot.dvm_attached_block_disk.as_ref() {
        verify_artifact_unchanged(root, dvm_attached_block_disk)?;
        verify_dvm_attached_block_disk_matches_runtime(snapshot)?;
    }
    if let Some(smpqual_early_system_executable) = snapshot.smpqual_early_system_executable.as_ref()
    {
        let runtime_disk = root.join(&snapshot.rustos_runtime_image.path);
        let observed = capture_smpqual_early_system_executable(&runtime_disk)?;
        if observed.bytes != smpqual_early_system_executable.bytes
            || observed.sha256 != smpqual_early_system_executable.sha256
        {
            bail!("pre-launch SMP qualification executable drifted inside the early-system image");
        }
    }
    Ok(())
}

fn capture_smpqual_early_system_executable(runtime_disk: &Path) -> Result<KvmSuccessArtifact> {
    const EARLY_SYSTEM_SMPQUAL_PATH: &str = "apps/smpqual/smpqual.elf";
    let image = read_private_early_system_image(runtime_disk)?;
    let header = EarlySystemHeader::decode(&image)
        .context("private early-system image has an invalid header")?;
    if usize::try_from(header.total_bytes).ok() != Some(image.len()) {
        bail!("private early-system image length diverged from its header");
    }
    let entry_count = usize::try_from(header.entry_count)
        .context("private early-system entry count does not fit usize")?;
    let mut previous_path: Option<Vec<u8>> = None;
    for index in 0..entry_count {
        let start = EARLY_SYSTEM_HEADER_BYTES
            .checked_add(
                index
                    .checked_mul(EARLY_SYSTEM_ENTRY_BYTES)
                    .context("private early-system table offset overflow")?,
            )
            .context("private early-system table offset overflow")?;
        let end = start
            .checked_add(EARLY_SYSTEM_ENTRY_BYTES)
            .context("private early-system table extent overflow")?;
        let entry = EarlySystemEntry::decode(
            image
                .get(start..end)
                .context("private early-system entry is outside the image")?,
            header,
        )
        .context("private early-system entry is invalid")?;
        let path = entry
            .path_bytes()
            .context("private early-system entry path is invalid")?;
        if previous_path
            .as_deref()
            .is_some_and(|previous| previous >= path)
        {
            bail!("private early-system entries are not strictly ordered");
        }
        previous_path = Some(path.to_vec());
        if path != EARLY_SYSTEM_SMPQUAL_PATH.as_bytes() {
            continue;
        }
        let start = usize::try_from(entry.payload_offset)
            .context("SMP qualification executable offset does not fit usize")?;
        let end = entry
            .payload_offset
            .checked_add(entry.payload_len)
            .and_then(|end| usize::try_from(end).ok())
            .context("SMP qualification executable extent is invalid")?;
        let payload = image
            .get(start..end)
            .context("SMP qualification executable is outside the early-system image")?;
        let observed_sha256 = sha256(payload);
        let expected_sha256 = entry
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if observed_sha256 != expected_sha256 {
            bail!("SMP qualification executable diverged from its early-system digest");
        }
        return Ok(KvmSuccessArtifact {
            path: "system/boot/early-system.img#apps/smpqual/smpqual.elf".to_owned(),
            bytes: entry.payload_len,
            sha256: observed_sha256,
            modified_unix_ms: None,
        });
    }
    bail!("private early-system image lacks the SMP qualification executable")
}

fn capture_dvm_attached_block_disk(
    root: &Path,
    rustos_runtime_image: &KvmSuccessArtifact,
    dvm_block_disk: Option<&Path>,
    required_for_qualification: bool,
) -> Result<Option<KvmSuccessArtifact>> {
    let Some(dvm_block_disk) = dvm_block_disk else {
        if required_for_qualification {
            bail!("SMP Ring3 qualification lacks the DVM-attached block disk");
        }
        return Ok(None);
    };
    let dvm_attached_block_disk = binary_artifact(root, dvm_block_disk)?;
    if rustos_runtime_image.bytes != dvm_attached_block_disk.bytes
        || rustos_runtime_image.sha256 != dvm_attached_block_disk.sha256
    {
        bail!(
            "DVM-attached block disk diverged from the private RustOS runtime image before QEMU launch"
        );
    }
    Ok(Some(dvm_attached_block_disk))
}

fn verify_dvm_attached_block_disk_matches_runtime(
    snapshot: &KvmLaunchEvidenceSnapshot,
) -> Result<()> {
    let dvm_attached_block_disk = snapshot
        .dvm_attached_block_disk
        .as_ref()
        .context("SMP snapshot lost the DVM-attached block disk")?;
    if snapshot.rustos_runtime_image.bytes != dvm_attached_block_disk.bytes
        || snapshot.rustos_runtime_image.sha256 != dvm_attached_block_disk.sha256
    {
        bail!(
            "pre-launch DVM-attached block disk no longer matches the private RustOS runtime image"
        );
    }
    Ok(())
}

pub(super) fn publish_success_summary(
    summary: &KvmSuccessSummary,
    archive: &SmpEvidenceArchive,
    snapshot: &KvmLaunchEvidenceSnapshot,
    rustos_debugcon: &[u8],
    dvm_serial: &[u8],
) -> Result<PathBuf> {
    let mut encoded = serde_json::to_vec_pretty(summary)?;
    encoded.push(b'\n');
    let checksum = format!("{}  {}.json\n", sha256(&encoded), archive.stem);
    let mut staged = vec![
        StagedArtifact::new(archive.path_for("rustos-debugcon.log"), rustos_debugcon),
        StagedArtifact::new(archive.path_for("linux-dvm-serial.log"), dvm_serial),
    ];
    if let Some(contract) = snapshot.private_smp_qualification_contract.as_deref() {
        staged.push(StagedArtifact::new(
            archive.path_for("smp-qualification.env"),
            contract,
        ));
    }
    staged.push(StagedArtifact::new(archive.path_for("json"), &encoded));
    staged.push(StagedArtifact::new(
        archive.path_for("json.sha256"),
        checksum.as_bytes(),
    ));

    for artifact in &mut staged {
        artifact.stage()?;
    }
    // This is deliberately the final operation before any artifact becomes
    // visible. A builder or other host writer cannot make a post-launch image
    // appear to be the immutable bytes that QEMU actually started from.
    verify_prelaunch_snapshot(&archive.root, snapshot)?;
    for artifact in &mut staged {
        artifact.publish()?;
    }
    Ok(archive.path_for("json"))
}

struct StagedArtifact<'a> {
    final_path: PathBuf,
    temporary_path: PathBuf,
    bytes: &'a [u8],
}

impl<'a> StagedArtifact<'a> {
    fn new(final_path: PathBuf, bytes: &'a [u8]) -> Self {
        let temporary_path = final_path.with_extension(format!(
            "{}.tmp",
            final_path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("artifact")
        ));
        Self {
            final_path,
            temporary_path,
            bytes,
        }
    }

    fn stage(&mut self) -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.temporary_path)
            .with_context(|| {
                format!(
                    "create private SMP evidence temporary {}",
                    self.temporary_path.display()
                )
            })?;
        file.write_all(self.bytes).with_context(|| {
            format!(
                "write private SMP evidence temporary {}",
                self.temporary_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "sync private SMP evidence temporary {}",
                self.temporary_path.display()
            )
        })?;
        Ok(())
    }

    fn publish(&mut self) -> Result<()> {
        fs::hard_link(&self.temporary_path, &self.final_path).with_context(|| {
            format!(
                "publish non-overwriting SMP evidence {}",
                self.final_path.display()
            )
        })?;
        fs::remove_file(&self.temporary_path).with_context(|| {
            format!(
                "remove published SMP evidence temporary {}",
                self.temporary_path.display()
            )
        })?;
        Ok(())
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("{label} must be exactly 32 lowercase hexadecimal characters");
    }
    Ok(())
}

fn verify_artifact_unchanged(root: &Path, expected: &KvmSuccessArtifact) -> Result<()> {
    let path = root.join(&expected.path);
    let observed = binary_artifact(root, &path)?;
    if observed.bytes != expected.bytes
        || observed.sha256 != expected.sha256
        || observed.modified_unix_ms != expected.modified_unix_ms
    {
        bail!(
            "pre-launch KVM evidence artifact drifted before publication: {}",
            expected.path
        );
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_run(cohort: Option<&str>, run_id: &str) -> SmpEvidenceRun {
        SmpEvidenceRun {
            cohort: cohort.map(str::to_owned),
            run_id: run_id.to_owned(),
            started_unix_ms: 1,
        }
    }

    #[test]
    fn archive_paths_bind_same_cohort_to_distinct_vcpu_runs() {
        let root = tempfile::tempdir().unwrap();
        let cohort = "0123456789abcdef0123456789abcdef";
        let one = SmpEvidenceArchive::new(
            root.path(),
            root.path(),
            &fixed_run(Some(cohort), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            1,
        )
        .unwrap();
        let two = SmpEvidenceArchive::new(
            root.path(),
            root.path(),
            &fixed_run(Some(cohort), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            2,
        )
        .unwrap();
        assert_eq!(one.directory, two.directory);
        assert_ne!(one.path_for("json"), two.path_for("json"));
    }

    #[test]
    fn archive_publish_never_replaces_an_existing_artifact() {
        let root = tempfile::tempdir().unwrap();
        let final_path = root.path().join("evidence.json");
        fs::write(&final_path, b"first").unwrap();
        let mut staged = StagedArtifact::new(final_path.clone(), b"second");
        staged.stage().unwrap();
        assert!(staged.publish().is_err());
        assert_eq!(fs::read(&final_path).unwrap(), b"first");
        fs::remove_file(staged.temporary_path).unwrap();
    }

    fn snapshot_for_paths(root: &Path, paths: &[PathBuf]) -> KvmLaunchEvidenceSnapshot {
        KvmLaunchEvidenceSnapshot {
            run: fixed_run(None, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            source_tree_sha256: "source".to_owned(),
            formal_verification: binary_artifact(root, &paths[0]).unwrap(),
            rustos_boot_image: binary_artifact(root, &paths[1]).unwrap(),
            rustos_runtime_image: binary_artifact(root, &paths[2]).unwrap(),
            dvm_attached_block_disk: Some(binary_artifact(root, &paths[3]).unwrap()),
            smpqual_early_system_executable: None,
            dvm_kernel: binary_artifact(root, &paths[4]).unwrap(),
            dvm_rootfs: binary_artifact(root, &paths[5]).unwrap(),
            private_smp_qualification_contract: None,
        }
    }

    fn summary_for_snapshot(snapshot: &KvmLaunchEvidenceSnapshot) -> KvmSuccessSummary {
        let empty_log = || KvmFailureLog {
            path: "test.log".to_owned(),
            bytes: 0,
            sha256: sha256(b""),
            latest_guest_ts_us: None,
        };
        KvmSuccessSummary {
            schema: "rustos-kvm-smp-correctness-evidence-v6",
            predecessor_schema: "rustos-kvm-smp-correctness-evidence-v5",
            status: "passed",
            smp_evidence_cohort: snapshot.run.cohort.clone(),
            run_id: snapshot.run.run_id.clone(),
            started_unix_ms: snapshot.run.started_unix_ms,
            rustos_vcpus: 1,
            boot_elapsed_ms: 1,
            formal_profile: "smp-iteration",
            source_tree_sha256: snapshot.source_tree_sha256.clone(),
            formal_verification: snapshot.formal_verification.clone(),
            rustos_boot_image: snapshot.rustos_boot_image.clone(),
            rustos_runtime_image: snapshot.rustos_runtime_image.clone(),
            dvm_attached_block_disk: snapshot.dvm_attached_block_disk.clone(),
            smpqual_early_system_executable: snapshot.smpqual_early_system_executable.clone(),
            dvm_kernel: snapshot.dvm_kernel.clone(),
            dvm_rootfs: snapshot.dvm_rootfs.clone(),
            rustos_log: empty_log(),
            dvm_log: empty_log(),
            rustos_debugcon_archive: KvmSuccessArtifact {
                path: "test-rustos.log".to_owned(),
                bytes: 0,
                sha256: sha256(b""),
                modified_unix_ms: None,
            },
            dvm_serial_archive: KvmSuccessArtifact {
                path: "test-dvm.log".to_owned(),
                bytes: 0,
                sha256: sha256(b""),
                modified_unix_ms: None,
            },
            private_smp_qualification_contract_archive: None,
            required_rustos_markers: 0,
            required_dvm_markers: 0,
            smp_runtime_markers: 0,
            smp_runtime_events: Vec::new(),
            smp_ring3_qualification_events: None,
            ui_minimum_fps: None,
            ui_proof_windows: 0,
        }
    }

    #[test]
    fn artifact_drift_is_rejected_before_success_publication() {
        let root = tempfile::tempdir().unwrap();
        let paths = ["formal", "boot", "runtime", "dvm-disk", "kernel", "rootfs"]
            .into_iter()
            .map(|name| root.path().join(name))
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(path, b"before").unwrap();
        }
        for path in &paths {
            let snapshot = snapshot_for_paths(root.path(), &paths);
            assert!(verify_prelaunch_snapshot(root.path(), &snapshot).is_ok());
            fs::write(path, b"after").unwrap();
            assert!(
                verify_prelaunch_snapshot(root.path(), &snapshot).is_err(),
                "{} drift must reject publication",
                path.display()
            );
            fs::write(path, b"before").unwrap();
        }
    }

    #[test]
    fn attached_dvm_copy_tamper_after_runtime_snapshot_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let paths = ["formal", "boot", "runtime", "dvm-disk", "kernel", "rootfs"]
            .into_iter()
            .map(|name| root.path().join(name))
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(path, b"before").unwrap();
        }
        let snapshot = snapshot_for_paths(root.path(), &paths);
        fs::write(&paths[3], b"tampered-dvm-copy").unwrap();

        assert!(verify_prelaunch_snapshot(root.path(), &snapshot).is_err());
    }

    #[test]
    fn attached_dvm_copy_mismatch_is_rejected_before_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let dvm_copy = root.path().join("dvm-copy");
        fs::write(&runtime, b"runtime").unwrap();
        fs::write(&dvm_copy, b"different").unwrap();
        let runtime_artifact = binary_artifact(root.path(), &runtime).unwrap();

        assert!(capture_dvm_attached_block_disk(
            root.path(),
            &runtime_artifact,
            Some(&dvm_copy),
            true,
        )
        .is_err());
    }

    #[test]
    fn success_publication_rechecks_the_prelaunch_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let paths = ["formal", "boot", "runtime", "dvm-disk", "kernel", "rootfs"]
            .into_iter()
            .map(|name| root.path().join(name))
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(path, b"before").unwrap();
        }
        let snapshot = snapshot_for_paths(root.path(), &paths);
        let archive = SmpEvidenceArchive::new(root.path(), root.path(), &snapshot.run, 1).unwrap();
        fs::write(&paths[2], b"after").unwrap();

        assert!(
            publish_success_summary(
                &summary_for_snapshot(&snapshot),
                &archive,
                &snapshot,
                b"rustos",
                b"dvm",
            )
            .is_err()
        );
        assert!(!archive.path_for("json").exists());
    }

    #[test]
    fn archive_keeps_exact_logs_contract_summary_and_checksum() {
        let root = tempfile::tempdir().unwrap();
        let paths = ["formal", "boot", "runtime", "dvm-disk", "kernel", "rootfs"]
            .into_iter()
            .map(|name| root.path().join(name))
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(path, b"pristine").unwrap();
        }
        let mut snapshot = snapshot_for_paths(root.path(), &paths);
        let cohort = "0123456789abcdef0123456789abcdef";
        snapshot.run = fixed_run(Some(cohort), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        snapshot.private_smp_qualification_contract = Some(b"exact-contract\n".to_vec());
        let archive = SmpEvidenceArchive::new(root.path(), root.path(), &snapshot.run, 2).unwrap();

        let published = publish_success_summary(
            &summary_for_snapshot(&snapshot),
            &archive,
            &snapshot,
            b"debugcon\n",
            b"dvm-serial\n",
        )
        .unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(published).unwrap()).unwrap();
        assert_eq!(
            summary
                .get("smp_evidence_cohort")
                .and_then(serde_json::Value::as_str),
            Some(cohort)
        );
        assert_eq!(
            summary.get("run_id").and_then(serde_json::Value::as_str),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            summary.get("schema").and_then(serde_json::Value::as_str),
            Some("rustos-kvm-smp-correctness-evidence-v6")
        );
        assert_eq!(
            summary
                .get("predecessor_schema")
                .and_then(serde_json::Value::as_str),
            Some("rustos-kvm-smp-correctness-evidence-v5")
        );
        assert_eq!(
            summary
                .get("dvm_attached_block_disk")
                .and_then(|artifact| artifact.get("sha256"))
                .and_then(serde_json::Value::as_str),
            snapshot
                .dvm_attached_block_disk
                .as_ref()
                .map(|artifact| artifact.sha256.as_str())
        );
        assert_eq!(
            summary
                .get("dvm_attached_block_disk")
                .and_then(|artifact| artifact.get("path"))
                .and_then(serde_json::Value::as_str),
            snapshot
                .dvm_attached_block_disk
                .as_ref()
                .map(|artifact| artifact.path.as_str())
        );
        assert_eq!(
            fs::read(archive.path_for("rustos-debugcon.log")).unwrap(),
            b"debugcon\n"
        );
        assert_eq!(
            fs::read(archive.path_for("linux-dvm-serial.log")).unwrap(),
            b"dvm-serial\n"
        );
        assert_eq!(
            fs::read(archive.path_for("smp-qualification.env")).unwrap(),
            b"exact-contract\n"
        );
        assert!(archive.path_for("json.sha256").is_file());
    }
}
