use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::path::{Path, PathBuf};

use toml::Value;

use crate::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerOwner {
    Boot,
    Core,
    KernelNucleusCore,
    KernelLowlevel,
    KernelMonolith,
    KernelHal,
    KernelMm,
    KernelObject,
    KernelIpcRuntime,
    KernelPs,
    KernelIoManager,
    KernelCompat,
    KernelExecutive,
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
        if !is_validated_owner(owner) {
            continue;
        }

        let text = fs::read_to_string(&manifest)?;
        let doc: Value = toml::from_str(&text).map_err(|err| {
            anyhow!(
                "failed to parse cargo manifest {}: {err}",
                manifest.display()
            )
        })?;
        let manifest_dir = manifest
            .parent()
            .with_context(|| format!("cargo manifest has no parent: {}", manifest.display()))?;

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
        validate_kernel_source_boundaries(root_dir)?;
        return Ok(());
    }

    Err(anyhow!(
        "workspace layering check failed:\n{}",
        violations.join("\n")
    ))
}

fn is_validated_owner(owner: LayerOwner) -> bool {
    matches!(
        owner,
        LayerOwner::KernelNucleusCore
            | LayerOwner::KernelLowlevel
            | LayerOwner::KernelMonolith
            | LayerOwner::KernelHal
            | LayerOwner::KernelMm
            | LayerOwner::KernelObject
            | LayerOwner::KernelIpcRuntime
            | LayerOwner::KernelPs
            | LayerOwner::KernelIoManager
            | LayerOwner::KernelCompat
            | LayerOwner::KernelExecutive
            | LayerOwner::Services
            | LayerOwner::Apps
            | LayerOwner::DriversBridges
            | LayerOwner::DriversLibs
            | LayerOwner::Libs
            | LayerOwner::Compat
            | LayerOwner::Boot
            | LayerOwner::Tests
    )
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

    if relative == "kernel/nucleus-core" || relative.starts_with("kernel/nucleus-core/") {
        LayerOwner::KernelNucleusCore
    } else if relative == "kernel/lowlevel" || relative.starts_with("kernel/lowlevel/") {
        LayerOwner::KernelLowlevel
    } else if relative == "kernel/hal" || relative.starts_with("kernel/hal/") {
        LayerOwner::KernelHal
    } else if relative == "kernel/mm" || relative.starts_with("kernel/mm/") {
        LayerOwner::KernelMm
    } else if relative == "kernel/object" || relative.starts_with("kernel/object/") {
        LayerOwner::KernelObject
    } else if relative == "kernel/ipc-runtime" || relative.starts_with("kernel/ipc-runtime/") {
        LayerOwner::KernelIpcRuntime
    } else if relative == "kernel/ps" || relative.starts_with("kernel/ps/") {
        LayerOwner::KernelPs
    } else if relative == "kernel/io-manager" || relative.starts_with("kernel/io-manager/") {
        LayerOwner::KernelIoManager
    } else if relative == "kernel/compat" || relative.starts_with("kernel/compat/") {
        LayerOwner::KernelCompat
    } else if relative == "kernel/executive" || relative.starts_with("kernel/executive/") {
        LayerOwner::KernelExecutive
    } else if relative == "kernel"
        || relative == "kernel/Cargo.toml"
        || relative.starts_with("kernel/src/")
    {
        LayerOwner::KernelMonolith
    } else if relative.starts_with("core/") {
        LayerOwner::Core
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
        LayerOwner::KernelNucleusCore => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::KernelLowlevel
        ),
        LayerOwner::KernelLowlevel => matches!(
            dep_owner,
            LayerOwner::Boot | LayerOwner::Core | LayerOwner::Libs | LayerOwner::DriversLibs
        ),
        LayerOwner::KernelMonolith => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelLowlevel
                | LayerOwner::KernelHal
                | LayerOwner::KernelMm
                | LayerOwner::KernelExecutive
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
        ),
        LayerOwner::KernelHal => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelLowlevel
        ),
        LayerOwner::KernelMm => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelLowlevel
                | LayerOwner::KernelHal
        ),
        LayerOwner::KernelObject => matches!(
            dep_owner,
            LayerOwner::Core | LayerOwner::Libs | LayerOwner::KernelNucleusCore
        ),
        LayerOwner::KernelIpcRuntime => matches!(
            dep_owner,
            LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelObject
                | LayerOwner::KernelMm
        ),
        LayerOwner::KernelPs => matches!(
            dep_owner,
            LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelObject
                | LayerOwner::KernelLowlevel
                | LayerOwner::KernelHal
                | LayerOwner::KernelMm
                | LayerOwner::KernelIpcRuntime
        ),
        LayerOwner::KernelIoManager => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelObject
                | LayerOwner::KernelHal
                | LayerOwner::KernelMm
                | LayerOwner::KernelIpcRuntime
                | LayerOwner::KernelPs
        ),
        LayerOwner::KernelCompat => matches!(
            dep_owner,
            LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelObject
                | LayerOwner::KernelHal
                | LayerOwner::KernelMm
                | LayerOwner::KernelIpcRuntime
                | LayerOwner::KernelPs
                | LayerOwner::KernelIoManager
        ),
        LayerOwner::KernelExecutive => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelHal
                | LayerOwner::KernelObject
                | LayerOwner::KernelMm
                | LayerOwner::KernelPs
                | LayerOwner::KernelIoManager
                | LayerOwner::KernelCompat
        ),
        LayerOwner::Services => matches!(
            dep_owner,
            LayerOwner::Libs | LayerOwner::Compat | LayerOwner::DriversLibs
        ),
        LayerOwner::Apps => matches!(dep_owner, LayerOwner::Libs | LayerOwner::Compat),
        LayerOwner::DriversBridges => matches!(
            dep_owner,
            LayerOwner::KernelMonolith
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
        ),
        LayerOwner::DriversLibs => matches!(
            dep_owner,
            LayerOwner::Boot | LayerOwner::Core | LayerOwner::Libs | LayerOwner::DriversLibs
        ),
        LayerOwner::Libs => matches!(
            dep_owner,
            LayerOwner::Boot | LayerOwner::Core | LayerOwner::Libs
        ),
        LayerOwner::Compat => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::Compat
        ),
        LayerOwner::Boot => matches!(
            dep_owner,
            LayerOwner::Boot | LayerOwner::Core | LayerOwner::Libs | LayerOwner::DriversLibs
        ),
        LayerOwner::Tests => matches!(
            dep_owner,
            LayerOwner::Boot
                | LayerOwner::Core
                | LayerOwner::Libs
                | LayerOwner::DriversLibs
                | LayerOwner::Compat
                | LayerOwner::KernelNucleusCore
                | LayerOwner::KernelLowlevel
                | LayerOwner::KernelMonolith
                | LayerOwner::KernelHal
                | LayerOwner::KernelMm
                | LayerOwner::KernelObject
                | LayerOwner::KernelIpcRuntime
                | LayerOwner::KernelPs
                | LayerOwner::KernelIoManager
                | LayerOwner::KernelCompat
                | LayerOwner::KernelExecutive
        ),
        _ => true,
    }
}

fn validate_kernel_source_boundaries(root_dir: &Path) -> Result<()> {
    if root_dir.join("core").exists() {
        bail!("top-level core/ must be removed after kernel directory rebase");
    }
    if root_dir.join("kernel/src/lib.rs").exists() {
        bail!("kernel/src/lib.rs must be removed after nucleus bin split");
    }
    if root_dir.join("kernel/src/system.rs").exists() {
        bail!("kernel/src/system.rs must be removed after executive split");
    }
    if root_dir.join("kernel/src/kernel_host/mod.rs").exists() {
        bail!("kernel/src/kernel_host must be removed after single-kernel reunification");
    }
    if root_dir.join("kernel/hosts").exists() {
        bail!("kernel/hosts must be removed after single-kernel reunification");
    }
    if root_dir.join("core/kernel-host-runtime").exists() {
        bail!("core/kernel-host-runtime must be removed after single-kernel reunification");
    }
    for shim in [
        "kernel/src/hal_api.rs",
        "kernel/src/mm_api.rs",
        "kernel/src/object_api.rs",
        "kernel/src/ipc_runtime_api.rs",
        "kernel/src/ps_api.rs",
        "kernel/src/io_manager_api.rs",
        "kernel/src/compat_api.rs",
    ] {
        if root_dir.join(shim).exists() {
            bail!("{shim} must be removed once kernel uses manager crates directly");
        }
    }
    if root_dir.join("kernel/base").exists() {
        bail!("kernel/base must be removed after ownership distribution");
    }

    let main_rs = fs::read_to_string(root_dir.join("kernel/src/main.rs"))?;
    if main_rs.contains("nucleus::") {
        bail!("kernel/src/main.rs must not depend on the nucleus library facade");
    }
    if main_rs.contains("kernel_host::") || main_rs.contains("crate::kernel_host") {
        bail!("kernel/src/main.rs must not reference kernel_host runtime glue");
    }
    if main_rs.contains("system::bootstrap_kernel_hosts")
        || main_rs.contains("system::finalize_kernel_initialization")
        || main_rs.contains("system::run_nucleus_loop")
    {
        bail!("kernel/src/main.rs must enter via executive boot facade");
    }

    let kernel_src_entries =
        fs::read_dir(root_dir.join("kernel/src"))?.collect::<std::result::Result<Vec<_>, _>>()?;
    let unexpected_kernel_src = kernel_src_entries
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (name.as_ref() != "main.rs").then_some(name.into_owned())
        })
        .collect::<Vec<_>>();
    if !unexpected_kernel_src.is_empty() {
        bail!(
            "kernel/src must be entry-only; unexpected paths remain: {}",
            unexpected_kernel_src.join(", ")
        );
    }

    let executive_lib = fs::read_to_string(root_dir.join("kernel/executive/src/lib.rs"))?;
    if executive_lib.contains("kernel_host::") || executive_lib.contains("crate::kernel_host") {
        bail!("kernel executive must not reference kernel_host runtime glue");
    }
    if executive_lib.contains("bootstrap_kernel_hosts")
        || executive_lib.contains("validate_named_host_barrier")
        || executive_lib.contains("load_staged_hosts")
    {
        bail!("kernel executive must not include host bootstrap flow");
    }
    assert_source_not_contains_any(
        &executive_lib,
        "kernel/executive/src/lib.rs",
        &[
            "use crate::arch::",
            "use crate::driver;",
            "use crate::input;",
            "use crate::io::console;",
            "use crate::io::session::",
            "use crate::io::{gui, tty};",
            "use crate::memory::",
            "use crate::multitask;",
            "use crate::usb;",
            "use crate::user::syscall;",
        ],
    )?;

    let executive_boot = fs::read_to_string(root_dir.join("kernel/executive/src/boot.rs"))?;
    assert_source_not_contains_any(
        &executive_boot,
        "kernel/executive/src/boot.rs",
        &[
            "crate::storage::boot_volume::init_boot_info",
            "crate::storage::block::",
            "crate::vfs::",
            "crate::driver::initialize_loadable_modules_for_class",
            "crate::driver::linux::runtime::service_compat_pending",
            "crate::input::dispatcher::service_pending",
            "crate::io::console::",
            "crate::multitask::",
            "crate::user::syscall::",
            "crate::memory::paging::smoke_test",
            "crate::arch::asmtools::current_rip",
        ],
    )?;

    let executive_tasks = fs::read_to_string(root_dir.join("kernel/executive/src/tasks.rs"))?;
    assert_source_not_contains_any(
        &executive_tasks,
        "kernel/executive/src/tasks.rs",
        &["multitask::yield_now"],
    )?;

    assert_source_not_contains_any(
        &executive_lib,
        "kernel/executive/src/lib.rs",
        &[
            concat!("kernel_base", "::user::console_host::"),
            concat!("kernel_base", "::debug::"),
            "pub mod bootstrap_fs",
        ],
    )?;

    let io_manager_api = fs::read_to_string(root_dir.join("kernel/io-manager/src/api.rs"))?;
    assert_source_not_contains_any(
        &io_manager_api,
        "kernel/io-manager/src/api.rs",
        &[concat!(
            "pub use ",
            "kernel_base",
            "::storage::boot_volume::BootstrapPhase;"
        )],
    )?;
    assert_source_not_contains_any(
        &io_manager_api,
        "kernel/io-manager/src/api.rs",
        &[concat!("kernel_base", "::")],
    )?;
    assert_source_not_contains_any(
        &io_manager_api,
        "kernel/io-manager/src/api.rs",
        &[concat!("kernel_base", "::vfs_core::")],
    )?;
    assert_source_not_contains_any(
        &io_manager_api,
        "kernel/io-manager/src/api.rs",
        &[concat!("kernel_base", "::bootstrap_fs::")],
    )?;

    let ipc_runtime_lib = fs::read_to_string(root_dir.join("kernel/ipc-runtime/src/lib.rs"))?;
    assert_source_not_contains_any(
        &ipc_runtime_lib,
        "kernel/ipc-runtime/src/lib.rs",
        &[concat!("pub use ", "kernel_base", "::ipc_core;")],
    )?;

    let compat_console_host =
        fs::read_to_string(root_dir.join("kernel/compat/src/user/console_host.rs"))?;
    assert_source_not_contains_any(
        &compat_console_host,
        "kernel/compat/src/user/console_host.rs",
        &["bootstrap_fs::"],
    )?;

    let compat_api = fs::read_to_string(root_dir.join("kernel/compat/src/api.rs"))?;
    assert_source_not_contains_any(
        &compat_api,
        "kernel/compat/src/api.rs",
        &[concat!("kernel_base", "::")],
    )?;

    let compat_user_mod = fs::read_to_string(root_dir.join("kernel/compat/src/user/mod.rs"))?;
    assert_source_not_contains_any(
        &compat_user_mod,
        "kernel/compat/src/user/mod.rs",
        &[
            "#[cfg(rustos_building_kernel_compat)]",
            "#[cfg(not(rustos_building_kernel_compat))]",
        ],
    )?;

    let hal_lib = fs::read_to_string(root_dir.join("kernel/hal/src/lib.rs"))?;
    assert_source_not_contains_any(
        &hal_lib,
        "kernel/hal/src/lib.rs",
        &[
            concat!("kernel_base", "::debug::"),
            "pub use kernel_mm::",
            "pub use kernel_ps::",
            concat!("pub use ", "kernel_base", "::"),
            "pub mod debug",
            "pub use kernel_lowlevel as lowlevel",
        ],
    )?;

    let hal_idt_mod = fs::read_to_string(root_dir.join("kernel/hal/src/arch/idt/mod.rs"))?;
    assert_source_not_contains_any(
        &hal_idt_mod,
        "kernel/hal/src/arch/idt/mod.rs",
        &["kernel_mm::", "kernel_ps::"],
    )?;

    let hal_idt_handlers =
        fs::read_to_string(root_dir.join("kernel/hal/src/arch/idt/handlers.rs"))?;
    assert_source_not_contains_any(
        &hal_idt_handlers,
        "kernel/hal/src/arch/idt/handlers.rs",
        &["kernel_mm::", "kernel_ps::"],
    )?;

    let hal_rtc = fs::read_to_string(root_dir.join("kernel/hal/src/arch/rtc.rs"))?;
    assert_source_not_contains_any(&hal_rtc, "kernel/hal/src/arch/rtc.rs", &["kernel_ps::"])?;

    for retired_shadow_file in [
        "kernel/compat/src/user/abi.rs",
        "kernel/compat/src/user/epoll.rs",
        "kernel/compat/src/user/handles.rs",
        "kernel/compat/src/user/linux.rs",
        "kernel/compat/src/user/memfd.rs",
        "kernel/compat/src/user/process_state.rs",
        "kernel/compat/src/user/socket.rs",
    ] {
        if root_dir.join(retired_shadow_file).exists() {
            bail!("{retired_shadow_file} should stay retired; use kernel_ps::api re-exports");
        }
    }

    for shared_util_file in [
        "kernel/io-manager/src/io/console.rs",
        "kernel/compat/src/user/process/linux.rs",
    ] {
        let source = fs::read_to_string(root_dir.join(shared_util_file))?;
        assert_source_not_contains_any(
            &source,
            shared_util_file,
            &[
                "crate::util::random::",
                "crate::util::ring::",
                concat!("kernel_base", "::util::"),
            ],
        )?;
    }

    for api_file in [
        "kernel/hal/src/api.rs",
        "kernel/mm/src/api.rs",
        "kernel/object/src/api.rs",
        "kernel/ipc-runtime/src/api.rs",
        "kernel/ps/src/api.rs",
        "kernel/io-manager/src/api.rs",
        "kernel/compat/src/api.rs",
    ] {
        let source = fs::read_to_string(root_dir.join(api_file))?;
        assert_source_not_contains_any(&source, api_file, &["pub mod api {"])?;
    }

    for manager_dir in [
        "kernel/hal/src",
        "kernel/mm/src",
        "kernel/object/src",
        "kernel/ipc-runtime/src",
        "kernel/ps/src",
        "kernel/io-manager/src",
        "kernel/compat/src",
        "kernel/executive/src",
    ] {
        assert_no_cross_crate_path_imports(&root_dir.join(manager_dir), root_dir)?;
    }

    assert_max_lines(root_dir, "kernel/src/main.rs", 120)?;
    assert_max_lines(root_dir, "kernel/executive/src/lib.rs", 180)?;
    assert_max_lines(root_dir, "kernel/compat/src/user/syscall/linux.rs", 600)?;
    assert_max_lines(root_dir, "kernel/io-manager/src/storage/block.rs", 500)?;
    assert_max_lines(root_dir, "kernel/ps/src/multitask/mod.rs", 500)?;

    for manager in [
        "kernel/hal/src/lib.rs",
        "kernel/mm/src/lib.rs",
        "kernel/object/src/lib.rs",
        "kernel/ipc-runtime/src/lib.rs",
        "kernel/ps/src/lib.rs",
        "kernel/io-manager/src/lib.rs",
        "kernel/compat/src/lib.rs",
        "kernel/executive/src/lib.rs",
    ] {
        let source = fs::read_to_string(root_dir.join(manager))?;
        if source.contains("pub use nucleus::") {
            bail!("{manager} still re-exports the nucleus crate facade");
        }
    }

    let mm_lib = fs::read_to_string(root_dir.join("kernel/mm/src/lib.rs"))?;
    assert_source_not_contains_any(
        &mm_lib,
        "kernel/mm/src/lib.rs",
        &[
            concat!("kernel_base", "::debug::"),
            concat!("pub use ", "kernel_base", "::"),
            "pub mod debug",
            "pub use kernel_lowlevel as lowlevel",
        ],
    )?;

    let ps_lib = fs::read_to_string(root_dir.join("kernel/ps/src/lib.rs"))?;
    assert_source_not_contains_any(
        &ps_lib,
        "kernel/ps/src/lib.rs",
        &[
            concat!("kernel_base", "::debug::"),
            concat!("pub use ", "kernel_base", "::"),
            "pub mod debug",
            "pub use kernel_lowlevel as lowlevel",
            "pub use kernel_mm::memory",
            concat!("pub use ", "kernel_base", "::multitask"),
        ],
    )?;

    let kernel_main = fs::read_to_string(root_dir.join("kernel/src/main.rs"))?;
    assert_source_not_contains_any(
        &kernel_main,
        "kernel/src/main.rs",
        &[
            concat!("kernel_base", "::debug::"),
            concat!("use ", "kernel_base", "::debug;"),
        ],
    )?;

    let workspace_cargo = fs::read_to_string(root_dir.join("Cargo.toml"))?;
    if workspace_cargo.contains("core/kernel-host-runtime")
        || workspace_cargo.contains("kernel/hosts/")
    {
        bail!("workspace must not include kernel host crates or kernel-host-runtime");
    }

    let kernel_cargo = fs::read_to_string(root_dir.join("kernel/Cargo.toml"))?;
    if kernel_cargo.contains("kernel-base =") {
        bail!("kernel/Cargo.toml must not depend on kernel-base");
    }

    Ok(())
}

fn assert_max_lines(root_dir: &Path, relative_path: &str, max_lines: usize) -> Result<()> {
    let source = fs::read_to_string(root_dir.join(relative_path))?;
    let line_count = source.lines().count();
    if line_count > max_lines {
        bail!("{relative_path} exceeds size gate: {line_count} > {max_lines}");
    }
    Ok(())
}

fn assert_source_not_contains_any(
    source: &str,
    relative_path: &str,
    patterns: &[&str],
) -> Result<()> {
    for pattern in patterns {
        if source.contains(pattern) {
            bail!(
                "{relative_path} must use manager facade APIs instead of direct reference `{pattern}`"
            );
        }
    }
    Ok(())
}

fn assert_no_cross_crate_path_imports(dir: &Path, root_dir: &Path) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            assert_no_cross_crate_path_imports(&path, root_dir)?;
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }

        let source = fs::read_to_string(&path)?;
        if source.contains("#[path = \"../../") || source.contains("#[path = \"../../../") {
            let relative = path
                .strip_prefix(root_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            bail!("{relative} must not pull source from another crate via #[path = ...]");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LayerOwner, classify_owner, dependency_allowed};
    use std::path::Path;

    #[test]
    fn classifies_kernel_internal_crates_before_kernel_monolith() {
        let root = Path::new("/repo");
        assert_eq!(
            classify_owner(root, Path::new("/repo/kernel/nucleus-core/Cargo.toml")),
            LayerOwner::KernelNucleusCore
        );
        assert_eq!(
            classify_owner(root, Path::new("/repo/kernel/hal/Cargo.toml")),
            LayerOwner::KernelHal
        );
        assert_eq!(
            classify_owner(root, Path::new("/repo/kernel/src/main.rs")),
            LayerOwner::KernelMonolith
        );
    }

    #[test]
    fn executive_depends_on_lower_kernel_layers() {
        assert!(dependency_allowed(
            LayerOwner::KernelExecutive,
            LayerOwner::KernelCompat
        ));
        assert!(!dependency_allowed(
            LayerOwner::KernelHal,
            LayerOwner::KernelExecutive
        ));
    }

    #[test]
    fn shared_libraries_cannot_depend_on_product_implementation_layers() {
        for disallowed in [
            LayerOwner::KernelMonolith,
            LayerOwner::KernelHal,
            LayerOwner::Services,
            LayerOwner::Apps,
            LayerOwner::DriversBridges,
        ] {
            assert!(!dependency_allowed(LayerOwner::Libs, disallowed));
        }
        assert!(dependency_allowed(LayerOwner::Libs, LayerOwner::Libs));
        assert!(dependency_allowed(LayerOwner::Libs, LayerOwner::Boot));
    }

    #[test]
    fn driver_libraries_cannot_depend_on_bridge_driver_implementations() {
        assert!(!dependency_allowed(
            LayerOwner::DriversLibs,
            LayerOwner::DriversBridges
        ));
        assert!(dependency_allowed(
            LayerOwner::DriversLibs,
            LayerOwner::Libs
        ));
        assert!(dependency_allowed(
            LayerOwner::DriversLibs,
            LayerOwner::DriversLibs
        ));
    }
}
