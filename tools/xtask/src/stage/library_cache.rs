//! Signed boot-prewarm hints from actual final-loader selections.
//! This is host/Ring3 policy, never kernel file/cache policy.
use anyhow::{Context, Result, ensure};
use fs_err as fs;
use rustos_library_cache_policy::{
    Admission, MANIFEST_PATH, MANIFEST_VERSION, Plan, Policy, Selection, is_common_runtime,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug)]
pub(super) struct LibraryUse {
    pub application: PathBuf,
    pub path: PathBuf,
    pub requested: String,
}

#[derive(Default)]
struct Consumers {
    applications: BTreeSet<PathBuf>,
    common: bool,
}

struct Candidate {
    path: PathBuf,
    bytes: u64,
    applications: u32,
    common: bool,
}

// All ELF helpers within one /apps/NAME (or /opt/NAME) installation count as
// one potential application, not many measured runtime users. Unknown layouts
// use the complete root name rather than guessing which binaries belong together.
fn application_group(app: &Path) -> PathBuf {
    let parts: Vec<_> = app
        .components()
        .filter_map(|p| match p {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if parts.len() >= 2 && (parts[0] == "apps" || parts[0] == "opt") {
        Path::new("/").join(parts[0]).join(parts[1])
    } else {
        app.to_path_buf()
    }
}

fn system_library(path: &Path) -> bool {
    ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
        .iter()
        .any(|dir| path.starts_with(dir))
}

pub(super) fn stage_plan(image: &Path, uses: &[LibraryUse]) -> Result<()> {
    let policy = Policy::DEFAULT;
    let mut by_path: BTreeMap<PathBuf, Consumers> = BTreeMap::new();
    for usage in uses {
        if !system_library(&usage.path) {
            continue;
        }
        let entry = by_path.entry(usage.path.clone()).or_default();
        entry
            .applications
            .insert(application_group(&usage.application));
        entry.common |= is_common_runtime(&usage.requested);
    }
    let mut candidates = Vec::new();
    for (path, consumers) in by_path {
        let source = super::runtime_libs::image_path(image, &path)?;
        let metadata = fs::metadata(&source)?;
        ensure!(
            metadata.is_file(),
            "prewarm input is not a file: {}",
            path.display()
        );
        candidates.push(Candidate {
            path,
            bytes: metadata.len(),
            applications: consumers.applications.len().try_into()?,
            common: consumers.common,
        });
    }
    candidates.sort_by(|a, b| {
        b.common
            .cmp(&a.common)
            .then(b.applications.cmp(&a.applications))
            .then(a.bytes.cmp(&b.bytes))
            .then(a.path.cmp(&b.path))
    });
    let mut selection =
        Selection::new(policy).map_err(|e| anyhow::anyhow!("invalid prewarm policy: {e:?}"))?;
    let mut text = format!(
        "{MANIFEST_VERSION}\t{}\t{}\t{}\t{}\t{}\n",
        policy.budget_bytes,
        policy.max_file_bytes,
        policy.max_entries,
        policy.min_applications,
        policy.read_ahead_pages
    );
    for candidate in candidates {
        if !matches!(
            selection.consider(candidate.bytes, candidate.applications, candidate.common),
            Admission::Warm { .. }
        ) {
            continue;
        }
        let path = candidate.path.to_str().context("non-UTF8 prewarm path")?;
        ensure!(
            rustos_library_cache_policy::valid_path(path),
            "invalid prewarm path {path:?}"
        );
        let bytes = fs::read(super::runtime_libs::image_path(image, &candidate.path)?)?;
        ensure!(
            u64::try_from(bytes.len())? == candidate.bytes,
            "prewarm input changed during staging"
        );
        let digest = Sha256::digest(&bytes);
        write!(text, "{}\t", candidate.bytes)?;
        for byte in digest {
            write!(text, "{byte:02x}")?;
        }
        writeln!(text, "\t{path}")?;
    }
    let plan =
        Plan::parse(&text).map_err(|e| anyhow::anyhow!("invalid generated prewarm plan: {e:?}"))?;
    ensure!(
        plan.entry_count() == selection.selected_entries()
            && plan.resident_bytes() == selection.selected_bytes(),
        "prewarm producer/consumer disagree"
    );
    let output = super::runtime_libs::image_path(image, Path::new(MANIFEST_PATH))?;
    if let Ok(metadata) = fs::symlink_metadata(&output) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "prewarm manifest must not be a symlink"
        );
    }
    fs::create_dir_all(output.parent().context("prewarm manifest parent")?)?;
    fs::write(output, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn usage(app: &str, path: &str, requested: &str) -> LibraryUse {
        LibraryUse {
            application: app.into(),
            path: path.into(),
            requested: requested.into(),
        }
    }
    fn file(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    #[test]
    fn prewarms_common_and_cross_app_small_but_not_rare_large_or_private() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["libc.so.6", "libshared.so", "librare.so"] {
            file(tmp.path(), &format!("/usr/lib/{name}"), b"library");
        }
        file(tmp.path(), "/usr/lib/libhuge.so", &vec![0; 5 * 1024 * 1024]);
        file(tmp.path(), "/apps/a/libprivate.so", b"private");
        let uses = [
            usage("/apps/a/main", "/usr/lib/libc.so.6", "libc.so.6"),
            usage("/apps/a/main", "/usr/lib/libshared.so", "libshared.so"),
            usage("/apps/b/helper", "/usr/lib/libshared.so", "libshared.so"),
            usage("/apps/a/main", "/usr/lib/librare.so", "librare.so"),
            usage("/apps/a/helper", "/usr/lib/librare.so", "librare.so"),
            usage("/apps/a/main", "/usr/lib/libhuge.so", "libc.so.6"),
            usage("/apps/a/main", "/apps/a/libprivate.so", "libc.so.6"),
        ];
        stage_plan(tmp.path(), &uses).unwrap();
        let text =
            fs::read_to_string(tmp.path().join(MANIFEST_PATH.trim_start_matches('/'))).unwrap();
        let plan = Plan::parse(&text).unwrap();
        let paths: Vec<_> = plan.entries().map(|e| e.unwrap().path).collect();
        assert_eq!(paths, ["/usr/lib/libc.so.6", "/usr/lib/libshared.so"]);
        assert_eq!(plan.resident_bytes(), 8192);
        let digest: [u8; 32] = Sha256::digest(b"library").into();
        assert!(plan.entries().all(|e| e.unwrap().sha256 == digest));
        stage_plan(tmp.path(), &uses).unwrap();
        assert_eq!(
            text,
            fs::read_to_string(tmp.path().join(MANIFEST_PATH.trim_start_matches('/'))).unwrap()
        );
    }
    #[test]
    fn helpers_in_one_installation_are_not_independent_popularity() {
        assert_eq!(
            application_group(Path::new("/apps/chrome/swiftshader/lib.so")),
            Path::new("/apps/chrome")
        );
        assert_eq!(
            application_group(Path::new("/opt/test/helper")),
            Path::new("/opt/test")
        );
        assert_eq!(
            application_group(Path::new("/unknown/test/helper")),
            Path::new("/unknown/test/helper")
        );
        assert!(!system_library(Path::new("/usr/library/evil.so")));
    }
    #[test]
    fn empty_plan_is_explicit_and_output_cannot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        stage_plan(tmp.path(), &[]).unwrap();
        let path = tmp.path().join(MANIFEST_PATH.trim_start_matches('/'));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(Plan::parse(&text).unwrap().entry_count(), 0);
        fs::remove_file(&path).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        assert!(stage_plan(tmp.path(), &[]).is_err());
        assert_eq!(fs::metadata(outside.path()).unwrap().len(), 0);
    }
}
