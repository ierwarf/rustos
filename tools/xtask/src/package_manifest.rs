use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

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
    PrekernelRustc,
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
    #[serde(default)]
    pub(crate) priority: i32,
    #[serde(default)]
    pub(crate) when: Option<String>,
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
            .map_err(|err| format!("failed to parse {}: {err}", manifest_path.display()))?;
        manifest.package_root = manifest_path
            .parent()
            .ok_or_else(|| format!("manifest has no parent: {}", manifest_path.display()))?
            .to_path_buf();
        manifest.manifest_path = manifest_path;
        manifests.push(manifest);
    }

    validate_manifests(&manifests)?;
    manifests.sort_by(|lhs, rhs| lhs.id.cmp(&rhs.id));
    Ok(manifests)
}

pub(crate) fn load_profile_manifests(
    root_dir: &Path,
    profile: &str,
) -> Result<Vec<PackageManifest>> {
    Ok(load_manifests(root_dir)?
        .into_iter()
        .filter(|manifest| manifest.profile_enabled(profile))
        .collect())
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
        .ok_or_else(|| format!("missing package manifest: {id}").into())
}

fn scan_manifest_paths(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if matches!(name, ".git" | "target" | "build") {
                continue;
            }
            scan_manifest_paths(&path, out)?;
            continue;
        }
        if file_type.is_file()
            && path.file_name().and_then(|value| value.to_str()) == Some(PACKAGE_MANIFEST_NAME)
        {
            out.push(path);
        }
    }
    Ok(())
}

fn validate_manifests(manifests: &[PackageManifest]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut install_paths = BTreeMap::<&str, &Path>::new();

    for manifest in manifests {
        let execution_domain = manifest.execution_domain();
        if manifest.id.trim().is_empty() {
            return Err(format!(
                "package id is empty in {}",
                manifest.manifest_path.display()
            )
            .into());
        }
        if !ids.insert(manifest.id.as_str()) {
            return Err(format!("duplicate package id: {}", manifest.id).into());
        }
        if manifest.profiles.is_empty() {
            return Err(format!(
                "package {} has no build profiles in {}",
                manifest.id,
                manifest.manifest_path.display()
            )
            .into());
        }
        if manifest.install.path.starts_with('/') || manifest.install.path.is_empty() {
            return Err(format!(
                "package {} has invalid install path {}",
                manifest.id, manifest.install.path
            )
            .into());
        }
        if let Some(previous) =
            install_paths.insert(&manifest.install.path, &manifest.manifest_path)
        {
            return Err(format!(
                "install path collision for {} between {} and {}",
                manifest.install.path,
                previous.display(),
                manifest.manifest_path.display()
            )
            .into());
        }

        match manifest.build.builder {
            BuilderKind::BootloaderUefi
            | BuilderKind::PrekernelRustc
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
                    return Err(format!(
                        "package {} missing build.package in {}",
                        manifest.id,
                        manifest.manifest_path.display()
                    )
                    .into());
                }
            }
            BuilderKind::CDemo | BuilderKind::MingwCExe => {
                if manifest.resolved_source_path().is_none() {
                    return Err(format!(
                        "package {} missing build.source in {}",
                        manifest.id,
                        manifest.manifest_path.display()
                    )
                    .into());
                }
            }
            BuilderKind::WinsysDllBundle => {
                if manifest.install.layout != InstallLayout::Directory {
                    return Err(format!(
                        "package {} must use install.layout = \"directory\"",
                        manifest.id
                    )
                    .into());
                }
            }
            BuilderKind::ExternalCopy => {
                if manifest.build.source.is_none() && manifest.build.source_env.is_none() {
                    return Err(format!(
                        "package {} must define build.source or build.source_env",
                        manifest.id
                    )
                    .into());
                }
            }
        }

        if manifest.autoload.is_some() && !manifest.kind.is_driver() {
            return Err(format!(
                "package {} defines autoload metadata but is not a driver",
                manifest.id
            )
            .into());
        }

        match manifest.kind {
            PackageKind::Boot | PackageKind::Kernel | PackageKind::BridgeDriver
                if execution_domain != ExecutionDomain::Kernel =>
            {
                return Err(format!(
                    "package {} must use execution_domain = \"kernel\"",
                    manifest.id
                )
                .into());
            }
            PackageKind::UserDriver
            | PackageKind::Service
            | PackageKind::App
            | PackageKind::Compat
                if execution_domain != ExecutionDomain::User =>
            {
                return Err(format!(
                    "package {} must use execution_domain = \"user\"",
                    manifest.id
                )
                .into());
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
            return Err(format!(
                "package {} uses startup mode {:?} but defines no desktop entries",
                manifest.id, manifest.startup
            )
            .into());
        }
    }

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

    Err(format!(
        "package {} has kind {:?} but manifest is outside {}: {}",
        manifest.id,
        manifest.kind,
        expected_root,
        manifest.manifest_path.display()
    )
    .into())
}

fn validate_install_taxonomy(manifest: &PackageManifest) -> Result<()> {
    let path = manifest.install.path.as_str();

    match manifest.kind {
        PackageKind::BridgeDriver => {
            if !path.starts_with("system/drivers/") || !path.ends_with(".ko") {
                return Err(format!(
                    "bridge driver {} must install to system/drivers/*.ko, got {}",
                    manifest.id, path
                )
                .into());
            }
        }
        PackageKind::UserDriver => {
            if !path.starts_with("drivers/user/") {
                return Err(format!(
                    "user driver {} must install under drivers/user/, got {}",
                    manifest.id, path
                )
                .into());
            }
        }
        PackageKind::Service => {
            if !path.starts_with("services/") {
                return Err(format!(
                    "service {} must install under services/, got {}",
                    manifest.id, path
                )
                .into());
            }
        }
        PackageKind::App => {
            if !path.starts_with("apps/") {
                return Err(
                    format!("app {} must install under apps/, got {}", manifest.id, path).into(),
                );
            }
        }
        PackageKind::Compat => {
            if !path.starts_with("compat/") {
                return Err(format!(
                    "compat package {} must install under compat/, got {}",
                    manifest.id, path
                )
                .into());
            }
        }
        PackageKind::Boot | PackageKind::Kernel => {}
    }

    Ok(())
}
