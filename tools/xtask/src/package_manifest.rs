use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use walkdir::{DirEntry, WalkDir};

use crate::Result;
use crate::config::Config;

pub(crate) const PACKAGE_MANIFEST_NAME: &str = "RUSTOS.package.toml";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PackageKind {
    Kernel,
    Service,
    App,
    Compat,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ExecutionDomain {
    Kernel,
    User,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StartupMode {
    None,
    Init,
    Session,
    Desktop,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum InstallLayout {
    File,
    Directory,
}

fn default_install_layout() -> InstallLayout {
    InstallLayout::File
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DesktopLaunchMode {
    None,
    NewSession,
    AllSessions,
}

fn default_desktop_launch_mode() -> DesktopLaunchMode {
    DesktopLaunchMode::None
}

fn default_startup_mode() -> StartupMode {
    StartupMode::None
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BuilderKind {
    KernelRustc,
    CargoKernelBinary,
    MingwCExe,
    CDemo,
    WinsysDllBundle,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildSpec {
    pub(crate) builder: BuilderKind,
    #[serde(default)]
    pub(crate) package: Option<String>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) extra_args: Vec<String>,
    /// Per-package linkage override. Currently understood values:
    /// - `None` / `Some("dynamic")`: link as a dynamic Linux ELF (default).
    /// - `Some("static-pie")`: link as a static PIE with no `PT_INTERP`. Used
    ///   by [seL4-style root tasks](https://docs.sel4.systems) such as
    ///   syscalld and vfsd that must run before the dynamic Linux runtime
    ///   (and the policies it depends on) are available.
    #[serde(default)]
    pub(crate) linkage: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallSpec {
    pub(crate) path: String,
    #[serde(default = "default_install_layout")]
    pub(crate) layout: InstallLayout,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopEntrySpec {
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) image: Option<String>,
    #[serde(default)]
    pub(crate) exec: Option<String>,
    #[serde(default)]
    pub(crate) no_display: bool,
    pub(crate) weight_micros: u64,
    #[serde(default)]
    pub(crate) logical_admin: bool,
    #[serde(default = "default_console_hosted")]
    pub(crate) console_hosted: bool,
    #[serde(default = "default_desktop_launch_mode")]
    pub(crate) launch: DesktopLaunchMode,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) env: Vec<String>,
}

fn default_console_hosted() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopSection {
    #[serde(default)]
    pub(crate) entries: Vec<DesktopEntrySpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageManifest {
    pub(crate) id: String,
    pub(crate) kind: PackageKind,
    pub(crate) execution_domain: ExecutionDomain,
    pub(crate) build: BuildSpec,
    pub(crate) install: InstallSpec,
    #[serde(default = "default_startup_mode")]
    pub(crate) startup: StartupMode,
    #[serde(default)]
    pub(crate) desktop: DesktopSection,
    #[serde(default)]
    pub(crate) runtime_deps: Vec<String>,
    #[serde(skip)]
    pub(crate) manifest_path: PathBuf,
    #[serde(skip)]
    pub(crate) package_root: PathBuf,
}

impl PackageManifest {
    pub(crate) fn artifact_path(&self, config: &Config) -> PathBuf {
        config.artifact_dir.join(&self.install.path)
    }

    pub(crate) fn image_path(&self, config: &Config) -> PathBuf {
        config.image_dir.join(&self.install.path)
    }

    pub(crate) fn resolved_source_path(&self) -> Option<PathBuf> {
        self.build
            .source
            .as_ref()
            .map(|source| self.package_root.join(source))
    }
}

pub(crate) fn load_manifests(root_dir: &Path) -> Result<Vec<PackageManifest>> {
    let mut manifest_paths = Vec::new();
    scan_manifest_paths(root_dir, &mut manifest_paths)?;
    manifest_paths.sort();

    let mut manifests = Vec::with_capacity(manifest_paths.len());
    for manifest_path in manifest_paths {
        let text = fs::read_to_string(&manifest_path)?;
        let mut manifest: PackageManifest = toml::from_str(&text)
            .map_err(|err| anyhow!("failed to parse {}: {err}", manifest_path.display()))?;
        manifest.package_root = manifest_path
            .parent()
            .with_context(|| format!("manifest has no parent: {}", manifest_path.display()))?
            .to_path_buf();
        manifest.manifest_path = manifest_path;
        manifests.push(manifest);
    }

    validate_manifests(&manifests)?;
    validate_runtime_deps(&manifests)?;
    manifests.sort_by(|lhs, rhs| lhs.id.cmp(&rhs.id));
    Ok(manifests)
}

pub(crate) fn validate_manifest_text_for_testinfra(text: &str) -> Result<()> {
    let _: PackageManifest =
        toml::from_str(text).map_err(|err| anyhow!("invalid package manifest: {err}"))?;
    Ok(())
}

pub(crate) fn required_manifest<'a>(
    manifests: &'a [PackageManifest],
    id: &str,
) -> Result<&'a PackageManifest> {
    manifests
        .iter()
        .find(|manifest| manifest.id == id)
        .with_context(|| anyhow!("missing package manifest: {id}"))
}

fn scan_manifest_paths(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    out.extend(
        WalkDir::new(dir)
            .into_iter()
            .filter_entry(|entry| !is_skipped_manifest_scan_dir(entry))
            .filter_map(|entry| match entry {
                Ok(entry)
                    if entry.file_type().is_file()
                        && entry.file_name().to_str() == Some(PACKAGE_MANIFEST_NAME) =>
                {
                    Some(Ok(entry.into_path()))
                }
                Ok(_) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );
    Ok(())
}

fn is_skipped_manifest_scan_dir(entry: &DirEntry) -> bool {
    entry.file_type().is_dir()
        && matches!(
            entry.file_name().to_str(),
            Some(".git" | "target" | "build")
        )
}

fn validate_manifests(manifests: &[PackageManifest]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut install_paths = BTreeMap::<&str, &Path>::new();

    for manifest in manifests {
        let execution_domain = manifest.execution_domain;
        if manifest.id.trim().is_empty() {
            bail!(
                "package id is empty in {}",
                manifest.manifest_path.display()
            );
        }
        if !ids.insert(manifest.id.as_str()) {
            bail!("duplicate package id: {}", manifest.id);
        }
        if !is_normal_relative_install_path(&manifest.install.path) {
            bail!(
                "package {} has invalid install path {}",
                manifest.id,
                manifest.install.path
            );
        }
        if let Some(previous) =
            install_paths.insert(&manifest.install.path, &manifest.manifest_path)
        {
            bail!(
                "install path collision for {} between {} and {}",
                manifest.install.path,
                previous.display(),
                manifest.manifest_path.display()
            );
        }

        match manifest.build.builder {
            BuilderKind::KernelRustc | BuilderKind::CargoKernelBinary => {
                if manifest
                    .build
                    .package
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
                {
                    bail!(
                        "package {} missing build.package in {}",
                        manifest.id,
                        manifest.manifest_path.display()
                    );
                }
            }
            BuilderKind::CDemo | BuilderKind::MingwCExe => {
                if manifest.resolved_source_path().is_none() {
                    bail!(
                        "package {} missing build.source in {}",
                        manifest.id,
                        manifest.manifest_path.display()
                    );
                }
            }
            BuilderKind::WinsysDllBundle => {
                if manifest.install.layout != InstallLayout::Directory {
                    bail!(
                        "package {} must use install.layout = \"directory\"",
                        manifest.id
                    );
                }
            }
        }

        match manifest.kind {
            PackageKind::Kernel if execution_domain != ExecutionDomain::Kernel => {
                bail!(
                    "package {} must use execution_domain = \"kernel\"",
                    manifest.id
                );
            }
            PackageKind::Service | PackageKind::App | PackageKind::Compat
                if execution_domain != ExecutionDomain::User =>
            {
                bail!(
                    "package {} must use execution_domain = \"user\"",
                    manifest.id
                );
            }
            _ => {}
        }

        validate_manifest_location(manifest)?;
        validate_install_taxonomy(manifest)?;

        if matches!(
            manifest.startup,
            StartupMode::Init | StartupMode::Session | StartupMode::Desktop
        ) && manifest.desktop.entries.is_empty()
        {
            bail!(
                "package {} uses startup mode {:?} but defines no desktop entries",
                manifest.id,
                manifest.startup
            );
        }
    }

    Ok(())
}

fn validate_runtime_deps(manifests: &[PackageManifest]) -> Result<()> {
    let manifest_by_id = manifests
        .iter()
        .map(|manifest| (manifest.id.as_str(), manifest))
        .collect::<BTreeMap<_, _>>();

    for manifest in manifests {
        for dep in &manifest.runtime_deps {
            if dep == &manifest.id {
                bail!("package {} runtime_deps includes itself", manifest.id);
            }
            if !manifest_by_id.contains_key(dep.as_str()) {
                bail!(
                    "package {} runtime_deps includes missing package {}",
                    manifest.id,
                    dep
                );
            }
        }
    }

    let mut state = BTreeMap::<&str, VisitState>::new();
    let mut stack = Vec::<&str>::new();
    for manifest in manifests {
        detect_runtime_dep_cycle(
            manifest.id.as_str(),
            &manifest_by_id,
            &mut state,
            &mut stack,
        )?;
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisitState {
    Visiting,
    Visited,
}

fn detect_runtime_dep_cycle<'a>(
    id: &'a str,
    manifest_by_id: &BTreeMap<&'a str, &'a PackageManifest>,
    state: &mut BTreeMap<&'a str, VisitState>,
    stack: &mut Vec<&'a str>,
) -> Result<()> {
    match state.get(id).copied() {
        Some(VisitState::Visited) => return Ok(()),
        Some(VisitState::Visiting) => {
            let start = stack
                .iter()
                .position(|candidate| *candidate == id)
                .unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(id);
            bail!("runtime_deps cycle detected: {}", cycle.join(" -> "));
        }
        None => {}
    }

    state.insert(id, VisitState::Visiting);
    stack.push(id);

    let manifest = manifest_by_id
        .get(id)
        .with_context(|| format!("runtime dependency graph missing package {id}"))?;
    for dep in &manifest.runtime_deps {
        detect_runtime_dep_cycle(dep.as_str(), manifest_by_id, state, stack)?;
    }

    stack.pop();
    state.insert(id, VisitState::Visited);
    Ok(())
}

fn validate_manifest_location(manifest: &PackageManifest) -> Result<()> {
    let relative = manifest.manifest_path.to_string_lossy().replace('\\', "/");

    let expected_root = match manifest.kind {
        PackageKind::Kernel => "kernel/",
        PackageKind::Service => "services/",
        PackageKind::App => "apps/",
        PackageKind::Compat => "compat/",
    };

    if relative.contains(&format!("/{expected_root}")) || relative.starts_with(expected_root) {
        return Ok(());
    }

    Err(anyhow!(
        "package {} has kind {:?} but manifest is outside {}: {}",
        manifest.id,
        manifest.kind,
        expected_root,
        manifest.manifest_path.display()
    ))
}

fn is_normal_relative_install_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_install_taxonomy(manifest: &PackageManifest) -> Result<()> {
    let path = manifest.install.path.as_str();

    match manifest.kind {
        PackageKind::Service => {
            if !path.starts_with("services/") {
                bail!(
                    "service {} must install under services/, got {}",
                    manifest.id,
                    path
                );
            }
        }
        PackageKind::App => {
            if !path.starts_with("apps/") {
                bail!("app {} must install under apps/, got {}", manifest.id, path);
            }
        }
        PackageKind::Compat => {
            if !path.starts_with("compat/") {
                bail!(
                    "compat package {} must install under compat/, got {}",
                    manifest.id,
                    path
                );
            }
        }
        PackageKind::Kernel => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, runtime_deps: &[&str]) -> PackageManifest {
        PackageManifest {
            id: id.to_string(),
            kind: PackageKind::Service,
            execution_domain: ExecutionDomain::User,
            build: BuildSpec {
                builder: BuilderKind::CargoKernelBinary,
                package: Some(id.to_string()),
                source: None,
                extra_args: Vec::new(),
                linkage: None,
            },
            install: InstallSpec {
                path: format!("services/{id}/{id}.elf"),
                layout: InstallLayout::File,
            },
            startup: StartupMode::None,
            desktop: DesktopSection::default(),
            runtime_deps: runtime_deps.iter().map(|dep| dep.to_string()).collect(),
            manifest_path: PathBuf::from(format!("services/{id}/RUSTOS.package.toml")),
            package_root: PathBuf::from(format!("services/{id}")),
        }
    }

    fn validate_deps(manifests: &[PackageManifest]) -> std::result::Result<(), String> {
        validate_runtime_deps(manifests).map_err(|err| err.to_string())
    }

    #[test]
    fn accepts_valid_runtime_dependency_graph() {
        let manifests = vec![
            manifest("runtimed", &[]),
            manifest("uiserver", &["runtimed"]),
        ];

        validate_deps(&manifests).expect("valid runtime deps");
    }

    #[test]
    fn rejects_missing_runtime_dependency_package() {
        let manifests = vec![manifest("uiserver", &["runtimed"])];

        let err = validate_deps(&manifests).expect_err("missing dep");
        assert!(err.contains("uiserver runtime_deps includes missing package runtimed"));
    }

    #[test]
    fn rejects_self_runtime_dependency() {
        let manifests = vec![manifest("uiserver", &["uiserver"])];

        let err = validate_deps(&manifests).expect_err("self dep");
        assert!(err.contains("uiserver runtime_deps includes itself"));
    }

    #[test]
    fn rejects_runtime_dependency_cycle() {
        let manifests = vec![
            manifest("runtimed", &["uiserver"]),
            manifest("uiserver", &["runtimed"]),
        ];

        let err = validate_deps(&manifests).expect_err("cycle");
        assert!(err.contains("runtime_deps cycle detected"));
        assert!(err.contains("runtimed"));
        assert!(err.contains("uiserver"));
    }

    #[test]
    fn install_paths_must_be_normal_relative_components() {
        assert!(is_normal_relative_install_path("services/initd/initd.elf"));
        assert!(!is_normal_relative_install_path(""));
        assert!(!is_normal_relative_install_path(
            "/services/initd/initd.elf"
        ));
        assert!(!is_normal_relative_install_path(
            "services/../initd/initd.elf"
        ));
        assert!(!is_normal_relative_install_path(
            "./services/initd/initd.elf"
        ));
    }

    #[test]
    fn rejects_retired_boot_metadata() {
        let err = validate_manifest_text_for_testinfra(
            "id = \"example\"\nkind = \"service\"\nexecution_domain = \"user\"\n[build]\nbuilder = \"cargo-kernel-binary\"\n[install]\npath = \"services/example/example.elf\"\n[boot]\npreload = true\nrequired = false\npriority = -1\n",
        )
        .expect_err("retired boot metadata must not be silently ignored");

        assert!(err.to_string().contains("unknown field `boot`"));
    }

    #[test]
    fn rejects_unknown_nested_metadata() {
        let err = validate_manifest_text_for_testinfra(
            "id = \"example\"\nkind = \"service\"\nexecution_domain = \"user\"\n[build]\nbuilder = \"cargo-kernel-binary\"\nunknown = true\n[install]\npath = \"services/example/example.elf\"\n",
        )
        .expect_err("unknown build metadata must not be silently ignored");

        assert!(err.to_string().contains("unknown field `unknown`"));
    }

    #[test]
    fn parses_top_level_startup_and_desktop_visibility() {
        let manifest: PackageManifest = toml::from_str(
            "id = \"example\"\nkind = \"app\"\nexecution_domain = \"user\"\nstartup = \"session\"\n[build]\nbuilder = \"cargo-kernel-binary\"\n[install]\npath = \"apps/example/example.elf\"\n[[desktop.entries]]\ndisplay_name = \"example\"\nweight_micros = 100\nno_display = true\n",
        )
        .expect("valid top-level startup metadata");

        assert_eq!(manifest.startup, StartupMode::Session);
        assert!(manifest.desktop.entries[0].no_display);
    }
}
