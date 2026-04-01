use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Config;
use crate::Result;

pub(crate) const PACKAGE_MANIFEST_NAME: &str = "RUSTOS.package.toml";
pub(crate) const DEFAULT_PROFILE: &str = "default";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PackageKind {
    Boot,
    Kernel,
    Driver,
    SystemApp,
    SystemLib,
    CompatKernel,
    CompatUser,
    Sample,
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
    pub(crate) output_name: Option<String>,
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
    #[serde(default = "default_profiles")]
    pub(crate) profiles: Vec<String>,
    pub(crate) build: BuildSpec,
    pub(crate) install: InstallSpec,
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
        if manifest.id.trim().is_empty() {
            return Err(format!("package id is empty in {}", manifest.manifest_path.display()).into());
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
        if let Some(previous) = install_paths.insert(&manifest.install.path, &manifest.manifest_path)
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
                if manifest.build.package.as_deref().unwrap_or_default().is_empty() {
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

        if manifest.autoload.is_some() && manifest.kind != PackageKind::Driver {
            return Err(format!(
                "package {} defines autoload metadata but is not a driver",
                manifest.id
            )
            .into());
        }
    }

    Ok(())
}
