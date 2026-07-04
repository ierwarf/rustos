use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use walkdir::{DirEntry, WalkDir};

use crate::Result;
use crate::config::Config;

pub(crate) const PACKAGE_MANIFEST_NAME: &str = "RUSTOS.package.toml";
pub(crate) const DEFAULT_PROFILE: &str = "default";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PackageKind {
    Boot,
    Kernel,
    #[serde(alias = "driver")]
    BridgeDriver,
    UserDriver,
    #[serde(alias = "system-app")]
    Service,
    #[serde(alias = "sample")]
    App,
    #[serde(alias = "compat-user")]
    #[serde(alias = "compat-kernel")]
    Compat,
}

impl PackageKind {
    pub(crate) fn is_driver(&self) -> bool {
        matches!(self, Self::BridgeDriver | Self::UserDriver)
    }

    pub(crate) fn default_execution_domain(&self) -> ExecutionDomain {
        match self {
            Self::Boot | Self::Kernel | Self::BridgeDriver => ExecutionDomain::Kernel,
            Self::UserDriver | Self::Service | Self::App | Self::Compat => ExecutionDomain::User,
        }
    }
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
    BootloaderUefi,
    KernelRustc,
    CargoKernelBinary,
    MingwCExe,
    CDemo,
    ModuleImage,
    WinsysDllBundle,
    ExternalCopy,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct BuildSpec {
    pub(crate) builder: BuilderKind,
    #[serde(default)]
    pub(crate) package: Option<String>,
    #[serde(default)]
    pub(crate) crate_name: Option<String>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) source_env: Option<String>,
    #[serde(default)]
    pub(crate) dependency_crates: Vec<String>,
    #[serde(default)]
    pub(crate) extra_args: Vec<String>,
    #[serde(default)]
    pub(crate) optional: bool,
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
pub(crate) struct InstallSpec {
    pub(crate) path: String,
    #[serde(default = "default_install_layout")]
    pub(crate) layout: InstallLayout,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub(crate) struct BootSpec {
    #[serde(default)]
    pub(crate) preload: bool,
    // Parsed today to keep the package schema stable while boot policy migration catches up.
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) required: bool,
    // Parsed today to keep manifest-driven boot ordering forward-compatible.
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) priority: i32,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AutoloadSpec {
    pub(crate) name: String,
    pub(crate) class: String,
    pub(crate) bus: String,
    #[serde(default = "default_autoload_enabled")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) priority: i32,
    #[serde(default)]
    pub(crate) when: Option<String>,
    #[serde(default)]
    pub(crate) aliases: Vec<String>,
    #[serde(default)]
    pub(crate) deps: Vec<String>,
    #[serde(default)]
    pub(crate) softdeps: Vec<String>,
    #[serde(default)]
    pub(crate) linux_driver_names: Vec<String>,
    #[serde(default)]
    pub(crate) provider_group: Option<String>,
    #[serde(default)]
    pub(crate) fallback_only: bool,
}

fn default_autoload_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DesktopEntrySpec {
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) image: Option<String>,
    #[serde(default)]
    pub(crate) exec: Option<String>,
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
pub(crate) struct DesktopSection {
    #[serde(default)]
    pub(crate) entries: Vec<DesktopEntrySpec>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PackageManifest {
    pub(crate) id: String,
    pub(crate) kind: PackageKind,
    #[serde(default)]
    pub(crate) execution_domain: Option<ExecutionDomain>,
    #[serde(default = "default_profiles")]
    pub(crate) profiles: Vec<String>,
    pub(crate) build: BuildSpec,
    pub(crate) install: InstallSpec,
    #[serde(default = "default_startup_mode")]
    pub(crate) startup: StartupMode,
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) boot: BootSpec,
    #[serde(default)]
    pub(crate) autoload: Option<AutoloadSpec>,
    #[serde(default)]
    pub(crate) desktop: DesktopSection,
    // Runtime dependency enforcement is planned, but the current orchestrator only records it.
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) runtime_deps: Vec<String>,
    #[serde(skip)]
    pub(crate) manifest_path: PathBuf,
    #[serde(skip)]
    pub(crate) package_root: PathBuf,
}

fn default_profiles() -> Vec<String> {
    vec![String::from(DEFAULT_PROFILE)]
}

impl PackageManifest {
    pub(crate) fn execution_domain(&self) -> ExecutionDomain {
        self.execution_domain
            .unwrap_or_else(|| self.kind.default_execution_domain())
    }

    pub(crate) fn artifact_path(&self, config: &Config) -> PathBuf {
        config.artifact_dir.join(&self.install.path)
    }

    pub(crate) fn image_path(&self, config: &Config) -> PathBuf {
        config.image_dir.join(&self.install.path)
    }

    pub(crate) fn profile_enabled(&self, profile: &str) -> bool {
        self.profiles.iter().any(|candidate| candidate == profile)
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
    manifests.sort_by(|lhs, rhs| lhs.id.cmp(&rhs.id));
    Ok(manifests)
}

pub(crate) fn validate_manifest_text_for_testinfra(text: &str) -> Result<()> {
    let _: PackageManifest =
        toml::from_str(text).map_err(|err| anyhow!("invalid package manifest: {err}"))?;
    Ok(())
}

pub(crate) fn load_profile_manifests(
    root_dir: &Path,
    profile: &str,
) -> Result<Vec<PackageManifest>> {
    let manifests = load_manifests(root_dir)?;
    let known_ids = manifests
        .iter()
        .map(|manifest| manifest.id.clone())
        .collect::<BTreeSet<_>>();
    let profile_manifests = manifests
        .into_iter()
        .filter(|manifest| manifest.profile_enabled(profile))
        .collect::<Vec<_>>();
    validate_profile_runtime_deps(&profile_manifests, profile, &known_ids)?;
    Ok(profile_manifests)
}

pub(crate) fn load_default_manifests(root_dir: &Path) -> Result<Vec<PackageManifest>> {
    load_profile_manifests(root_dir, DEFAULT_PROFILE)
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
        let execution_domain = manifest.execution_domain();
        if manifest.id.trim().is_empty() {
            bail!(
                "package id is empty in {}",
                manifest.manifest_path.display()
            );
        }
        if !ids.insert(manifest.id.as_str()) {
            bail!("duplicate package id: {}", manifest.id);
        }
        if manifest.profiles.is_empty() {
            bail!(
                "package {} has no build profiles in {}",
                manifest.id,
                manifest.manifest_path.display()
            );
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
            BuilderKind::BootloaderUefi
            | BuilderKind::KernelRustc
            | BuilderKind::CargoKernelBinary
            | BuilderKind::ModuleImage => {
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
            BuilderKind::ExternalCopy => {
                if manifest.build.source.is_none() && manifest.build.source_env.is_none() {
                    bail!(
                        "package {} must define build.source or build.source_env",
                        manifest.id
                    );
                }
            }
        }

        if manifest.autoload.is_some() && !manifest.kind.is_driver() {
            bail!(
                "package {} defines autoload metadata but is not a driver",
                manifest.id
            );
        }

        match manifest.kind {
            PackageKind::Boot | PackageKind::Kernel | PackageKind::BridgeDriver
                if execution_domain != ExecutionDomain::Kernel =>
            {
                bail!(
                    "package {} must use execution_domain = \"kernel\"",
                    manifest.id
                );
            }
            PackageKind::UserDriver
            | PackageKind::Service
            | PackageKind::App
            | PackageKind::Compat
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

fn validate_profile_runtime_deps(
    manifests: &[PackageManifest],
    profile: &str,
    known_ids: &BTreeSet<String>,
) -> Result<()> {
    let active_ids = manifests
        .iter()
        .map(|manifest| manifest.id.as_str())
        .collect::<BTreeSet<_>>();
    let manifest_by_id = manifests
        .iter()
        .map(|manifest| (manifest.id.as_str(), manifest))
        .collect::<BTreeMap<_, _>>();

    for manifest in manifests {
        for dep in &manifest.runtime_deps {
            if dep == &manifest.id {
                bail!("package {} runtime_deps includes itself", manifest.id);
            }
            if !known_ids.contains(dep.as_str()) {
                bail!(
                    "package {} runtime_deps includes missing package {}",
                    manifest.id,
                    dep
                );
            }
            if !active_ids.contains(dep.as_str()) {
                bail!(
                    "package {} runtime_deps includes package {} outside profile {}",
                    manifest.id,
                    dep,
                    profile
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
        PackageKind::Boot => "boot/",
        PackageKind::Kernel => "kernel/",
        PackageKind::BridgeDriver => "drivers/bridges/",
        PackageKind::UserDriver => "drivers/user/",
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
        PackageKind::BridgeDriver => {
            if !path.starts_with("system/drivers/") || !path.ends_with(".ko") {
                bail!(
                    "bridge driver {} must install to system/drivers/*.ko, got {}",
                    manifest.id,
                    path
                );
            }
        }
        PackageKind::UserDriver => {
            if !path.starts_with("drivers/user/") {
                bail!(
                    "user driver {} must install under drivers/user/, got {}",
                    manifest.id,
                    path
                );
            }
        }
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
        PackageKind::Boot | PackageKind::Kernel => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, profiles: &[&str], runtime_deps: &[&str]) -> PackageManifest {
        PackageManifest {
            id: id.to_string(),
            kind: PackageKind::Service,
            execution_domain: Some(ExecutionDomain::User),
            profiles: profiles.iter().map(|profile| profile.to_string()).collect(),
            build: BuildSpec {
                builder: BuilderKind::CargoKernelBinary,
                package: Some(id.to_string()),
                crate_name: None,
                source: None,
                source_env: None,
                dependency_crates: Vec::new(),
                extra_args: Vec::new(),
                optional: false,
                linkage: None,
            },
            install: InstallSpec {
                path: format!("services/{id}/{id}.elf"),
                layout: InstallLayout::File,
            },
            startup: StartupMode::None,
            boot: BootSpec::default(),
            autoload: None,
            desktop: DesktopSection::default(),
            runtime_deps: runtime_deps.iter().map(|dep| dep.to_string()).collect(),
            manifest_path: PathBuf::from(format!("services/{id}/RUSTOS.package.toml")),
            package_root: PathBuf::from(format!("services/{id}")),
        }
    }

    fn validate_profile(
        manifests: &[PackageManifest],
        profile: &str,
    ) -> std::result::Result<(), String> {
        let known_ids = manifests
            .iter()
            .map(|manifest| manifest.id.clone())
            .collect::<BTreeSet<_>>();
        let profile_manifests = manifests
            .iter()
            .filter(|manifest| manifest.profile_enabled(profile))
            .cloned()
            .collect::<Vec<_>>();
        validate_profile_runtime_deps(&profile_manifests, profile, &known_ids)
            .map_err(|err| err.to_string())
    }

    #[test]
    fn accepts_valid_runtime_dependency_graph() {
        let manifests = vec![
            manifest("runtimed", &["default"], &[]),
            manifest("uiserver", &["default"], &["runtimed"]),
        ];

        validate_profile(&manifests, "default").expect("valid runtime deps");
    }

    #[test]
    fn rejects_missing_runtime_dependency_package() {
        let manifests = vec![manifest("uiserver", &["default"], &["runtimed"])];

        let err = validate_profile(&manifests, "default").expect_err("missing dep");
        assert!(err.contains("uiserver runtime_deps includes missing package runtimed"));
    }

    #[test]
    fn rejects_self_runtime_dependency() {
        let manifests = vec![manifest("uiserver", &["default"], &["uiserver"])];

        let err = validate_profile(&manifests, "default").expect_err("self dep");
        assert!(err.contains("uiserver runtime_deps includes itself"));
    }

    #[test]
    fn rejects_runtime_dependency_cycle() {
        let manifests = vec![
            manifest("runtimed", &["default"], &["uiserver"]),
            manifest("uiserver", &["default"], &["runtimed"]),
        ];

        let err = validate_profile(&manifests, "default").expect_err("cycle");
        assert!(err.contains("runtime_deps cycle detected"));
        assert!(err.contains("runtimed"));
        assert!(err.contains("uiserver"));
    }

    #[test]
    fn rejects_runtime_dependency_outside_selected_profile() {
        let manifests = vec![
            manifest("storaged", &["legacy-services"], &[]),
            manifest("uiserver", &["default"], &["storaged"]),
        ];

        let err = validate_profile(&manifests, "default").expect_err("profile dep");
        assert!(
            err.contains("uiserver runtime_deps includes package storaged outside profile default")
        );
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
}
