use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerOwner {
    Boot,
    Kernel,
    Services,
    Apps,
    DriversBridges,
    DriversLibs,
    Libs,
    Compat,
    Tools,
    Tests,
    Other,
}

pub(crate) fn validate_workspace_layering(root_dir: &Path) -> Result<()> {
    let mut manifests = Vec::new();
    scan_cargo_manifests(root_dir, &mut manifests)?;

    let mut violations = Vec::new();
    for manifest in manifests {
        if manifest == root_dir.join("Cargo.toml") {
            continue;
        }
        let owner = classify_owner(root_dir, &manifest);
        if !matches!(
            owner,
            LayerOwner::Kernel
                | LayerOwner::Services
                | LayerOwner::Apps
                | LayerOwner::DriversBridges
        ) {
            continue;
        }

        let text = fs::read_to_string(&manifest)?;
        let doc: Value = toml::from_str(&text).map_err(|err| {
            format!(
                "failed to parse cargo manifest {}: {err}",
                manifest.display()
            )
        })?;
        let manifest_dir = manifest
            .parent()
            .ok_or_else(|| format!("cargo manifest has no parent: {}", manifest.display()))?;

        let mut deps = Vec::new();
        collect_path_dependencies(&doc, manifest_dir, &mut deps);

        for dep in deps {
            let dep_owner = classify_owner(root_dir, &dep);
            if !dependency_allowed(owner, dep_owner) {
                violations.push(format!(
                    "{} depends on disallowed layer {:?} via {}",
                    manifest.display(),
                    dep_owner,
                    dep.display()
                ));
            }
        }
    }

    if violations.is_empty() {
        return Ok(());
    }

    Err(format!(
        "workspace layering check failed:\n{}",
        violations.join("\n")
    )
    .into())
}

fn scan_cargo_manifests(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if matches!(name, ".git" | "target" | "build" | "logs") {
                continue;
            }
            scan_cargo_manifests(&path, out)?;
            continue;
        }
        if file_type.is_file()
            && path.file_name().and_then(|value| value.to_str()) == Some("Cargo.toml")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_path_dependencies(doc: &Value, manifest_dir: &Path, out: &mut Vec<PathBuf>) {
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(table) = doc.get(key).and_then(Value::as_table) else {
            continue;
        };
        for dep in table.values() {
            let Some(path) = dep
                .as_table()
                .and_then(|table| table.get("path"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            out.push(normalize_path(&manifest_dir.join(path)));
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn classify_owner(root_dir: &Path, path: &Path) -> LayerOwner {
    let relative = path.strip_prefix(root_dir).unwrap_or(path);
    let relative = relative.to_string_lossy().replace('\\', "/");

    if relative.starts_with("kernel/") {
        LayerOwner::Kernel
    } else if relative.starts_with("services/") {
        LayerOwner::Services
    } else if relative.starts_with("apps/") {
        LayerOwner::Apps
    } else if relative.starts_with("drivers/bridges/") {
        LayerOwner::DriversBridges
    } else if relative.starts_with("drivers/libs/") {
        LayerOwner::DriversLibs
    } else if relative.starts_with("libs/") {
        LayerOwner::Libs
    } else if relative.starts_with("compat/") {
        LayerOwner::Compat
    } else if relative.starts_with("boot/") {
        LayerOwner::Boot
    } else if relative.starts_with("tools/") {
        LayerOwner::Tools
    } else if relative.starts_with("tests/") {
        LayerOwner::Tests
    } else {
        LayerOwner::Other
    }
}

fn dependency_allowed(owner: LayerOwner, dep_owner: LayerOwner) -> bool {
    match owner {
        LayerOwner::Kernel => matches!(
            dep_owner,
            LayerOwner::Boot | LayerOwner::Libs | LayerOwner::DriversLibs
        ),
        LayerOwner::Services => matches!(
            dep_owner,
            LayerOwner::Libs | LayerOwner::Compat | LayerOwner::DriversLibs
        ),
        LayerOwner::Apps => matches!(dep_owner, LayerOwner::Libs | LayerOwner::Compat),
        LayerOwner::DriversBridges => matches!(
            dep_owner,
            LayerOwner::Kernel | LayerOwner::Libs | LayerOwner::DriversLibs
        ),
        _ => true,
    }
}
