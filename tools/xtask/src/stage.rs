use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;
use crate::config::Config;
use crate::package_manifest::{
    BuilderKind, DesktopLaunchMode, InstallLayout, PackageManifest, StartupMode,
    load_default_manifests,
};
use crate::util::{
    command_in_path, copy_or_unpack_firmware, copy_tree_files, copy_with_parent,
    remove_dir_if_exists, remove_file_if_exists, run_command,
};

const APPLICATIONS_DIR: &str = "usr/share/applications";
const DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";
const DESKTOP_REGISTRY_PATH: &str = "system/registry/system/desktop-programs.tsv";
const RUNTIME_LAUNCH_REGISTRY_PATH: &str = "system/registry/system/runtime-launch-programs.tsv";
const STARTUP_REGISTRY_PATH: &str = "system/registry/system/startup-programs.tsv";
const WINDOWS_DLL_REGISTRY_PATH: &str = "system/registry/compat/windows-system-dlls.txt";
const LD_SO_CONF_PATH: &str = "etc/ld.so.conf";
const LEGACY_BUILD_DIRS: &[&str] = &["EFI", "etc", "lib", "lib64", "linux", "SYSTEM"];
const LEGACY_BUILD_FILES: &[&str] = &[
    "kernel.elf",
    "nucleus.elf",
    "prekernel.elf",
    "UISERVER.ELF",
    "UISERVER.EXE",
    "SHELL.ELF",
    "EXECSMOKE.ELF",
    "USERDEMO.ELF",
    "USERDEMO.EXE",
    "NvVars",
    "artifacts/boot/BOOTX64.EFI",
    "background.jpg",
    "sonic.gif",
];
const LEGACY_ROOT_FILES: &[&str] = &["debugcon.log", "qemu_interrupt.log"];

pub(crate) fn stage(config: &Config) -> Result<()> {
    let manifests = load_default_manifests(&config.root_dir)?;

    remove_dir_if_exists(&config.image_dir)?;
    cleanup_legacy_build_layout(config)?;

    stage_image_asset_overlay(&config.image_asset_overlay_dir, &config.image_dir)?;

    for manifest in &manifests {
        stage_manifest(config, manifest)?;
    }

    let amdgpu_image_firmware_dir = config.amdgpu_image_firmware_dir();
    fs::create_dir_all(&amdgpu_image_firmware_dir)?;
    for basename in &config.amdgpu_required_firmware_basenames {
        let dst = amdgpu_image_firmware_dir.join(basename);
        copy_or_unpack_firmware(&config.amdgpu_firmware_dir, basename, &dst)?;
    }

    write_driver_registry(config, &manifests)?;
    write_application_desktop_files(config, &manifests)?;
    write_desktop_registry(config, &manifests)?;
    write_runtime_launch_registry(config, &manifests)?;
    write_startup_registry(config, &manifests)?;
    write_windows_dll_registry(config, &manifests)?;
    generate_dynamic_linker_cache(&config.image_dir)?;
    Ok(())
}

fn stage_manifest(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let artifact = manifest.artifact_path(config);
    let image = manifest.image_path(config);

    match manifest.install.layout {
        InstallLayout::File => {
            if artifact.is_file() {
                copy_with_parent(&artifact, &image)?;
                return Ok(());
            }
        }
        InstallLayout::Directory => {
            if artifact.is_dir() {
                stage_directory_manifest(&artifact, &image)?;
                return Ok(());
            }
        }
    }

    if manifest.build.optional {
        remove_file_if_exists(&image)?;
        remove_dir_if_exists(&image)?;
        return Ok(());
    }

    Err(format!(
        "missing staged artifact for package {} at {}",
        manifest.id,
        artifact.display()
    )
    .into())
}

fn stage_directory_manifest(src_root: &Path, dst_root: &Path) -> Result<()> {
    copy_tree_files(src_root, dst_root)
}

fn write_driver_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests {
        let Some(autoload) = manifest.autoload.as_ref() else {
            continue;
        };
        if !manifest.artifact_path(config).is_file() {
            continue;
        }
        lines.push(format!(
            "name={}\tclass={}\tbus={}\tpriority={}\tpath={}\twhen={}",
            registry_value(&autoload.name)?,
            registry_value(&autoload.class)?,
            registry_value(&autoload.bus)?,
            autoload.priority,
            registry_value(&manifest.install.path)?,
            registry_value(autoload.when.as_deref().unwrap_or("vfs-ready"))?,
        ));
    }
    write_registry_lines(config.image_dir.join(DRIVER_REGISTRY_PATH), &lines)
}

fn write_application_desktop_files(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let applications_dir = config.image_dir.join(APPLICATIONS_DIR);
    fs::create_dir_all(&applications_dir)?;

    for manifest in manifests {
        if !manifest.artifact_path(config).exists() {
            continue;
        }

        for (index, entry) in manifest.desktop.entries.iter().enumerate() {
            let desktop_file_id = desktop_file_id(manifest, index);
            if !should_generate_application_desktop(config, manifest, &desktop_file_id) {
                continue;
            }
            let exec = entry.exec.as_deref().unwrap_or(&manifest.install.path);
            let argv = if entry.args.is_empty() {
                vec![exec.to_string()]
            } else {
                entry.args.clone()
            };
            let content = format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={exec_line}\nTerminal={terminal}\nOnlyShowIn=RustOS;\nNoDisplay={no_display}\nX-RustOS-DesktopId={desktop_id}\nX-RustOS-Startup={startup}\nX-RustOS-WeightMicros={weight}\nX-RustOS-LogicalAdmin={logical_admin}\nX-RustOS-ConsoleHosted={console_hosted}\nX-RustOS-Argv={argv}\nX-RustOS-Env={env}\n",
                name = desktop_value(&entry.display_name)?,
                exec_line = desktop_exec_line(&argv)?,
                terminal = desktop_bool(entry.console_hosted),
                no_display =
                    desktop_bool(manifest.kind == crate::package_manifest::PackageKind::Service),
                desktop_id = desktop_value(&desktop_file_id)?,
                startup = desktop_startup_mode(manifest.startup),
                weight = entry.weight_micros,
                logical_admin = desktop_bool(entry.logical_admin),
                console_hosted = desktop_bool(entry.console_hosted),
                argv = desktop_value(&entry.args.join("|"))?,
                env = desktop_value(&entry.env.join("|"))?,
            );
            fs::write(applications_dir.join(&desktop_file_id), content)?;
        }
    }

    Ok(())
}

fn should_generate_application_desktop(
    config: &Config,
    manifest: &PackageManifest,
    desktop_file_id: &str,
) -> bool {
    if !matches!(manifest.startup, StartupMode::None) {
        return true;
    }
    config
        .image_dir
        .join("etc/xdg/autostart")
        .join(desktop_file_id)
        .is_file()
}

fn write_desktop_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests {
        if !manifest.artifact_path(config).exists() {
            continue;
        }
        for (index, entry) in manifest.desktop.entries.iter().enumerate() {
            let desktop_id = desktop_file_id(manifest, index);
            let image = entry.image.as_deref().unwrap_or(&manifest.install.path);
            let exec = entry.exec.as_deref().unwrap_or(image);
            let args = if entry.args.is_empty() {
                String::new()
            } else {
                entry
                    .args
                    .iter()
                    .map(|arg| registry_value(arg))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let env = if entry.env.is_empty() {
                String::new()
            } else {
                entry
                    .env
                    .iter()
                    .map(|item| registry_value(item))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let launch = match entry.launch {
                DesktopLaunchMode::None => "none",
                DesktopLaunchMode::NewSession => "new-session",
                DesktopLaunchMode::AllSessions => "all-sessions",
            };
            let startup = desktop_startup_mode(manifest.startup);
            let no_display = manifest.kind == crate::package_manifest::PackageKind::Service;
            let autostart_enabled = desktop_autostart_enabled(config, &desktop_id)?;
            lines.push(format!(
                "desktop_id={}\tstartup={}\tdisplay_name={}\timage={}\texec={}\tweight={}\tlogical_admin={}\tconsole_hosted={}\tterminal={}\thidden=0\tno_display={}\tautostart_enabled={}\tlaunch={}\targs={}\tenv={}",
                registry_value(&desktop_id)?,
                startup,
                registry_value(&entry.display_name)?,
                registry_value(image)?,
                registry_value(exec)?,
                entry.weight_micros,
                if entry.logical_admin { 1 } else { 0 },
                if entry.console_hosted { 1 } else { 0 },
                if entry.console_hosted { 1 } else { 0 },
                if no_display { 1 } else { 0 },
                if autostart_enabled { 1 } else { 0 },
                launch,
                args,
                env,
            ));
        }
    }
    write_registry_lines(config.image_dir.join(DESKTOP_REGISTRY_PATH), &lines)
}

fn write_runtime_launch_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests {
        if !manifest.artifact_path(config).exists() {
            continue;
        }
        for (index, entry) in manifest.desktop.entries.iter().enumerate() {
            let desktop_id = desktop_file_id(manifest, index);
            let autostart_enabled = desktop_autostart_enabled(config, &desktop_id)?;
            let startup_mode = manifest.startup;
            if matches!(startup_mode, StartupMode::None) && !autostart_enabled {
                continue;
            }

            let image = entry.image.as_deref().unwrap_or(&manifest.install.path);
            let exec = entry.exec.as_deref().unwrap_or(image);
            let args = if entry.args.is_empty() {
                String::new()
            } else {
                entry
                    .args
                    .iter()
                    .map(|arg| registry_value(arg))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let env = if entry.env.is_empty() {
                String::new()
            } else {
                entry
                    .env
                    .iter()
                    .map(|item| registry_value(item))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let launch = match entry.launch {
                DesktopLaunchMode::None => "none",
                DesktopLaunchMode::NewSession => "new-session",
                DesktopLaunchMode::AllSessions => "all-sessions",
            };
            let no_display = manifest.kind == crate::package_manifest::PackageKind::Service;
            lines.push(format!(
                "desktop_id={}\tstartup={}\tdisplay_name={}\timage={}\texec={}\tweight={}\tlogical_admin={}\tconsole_hosted={}\tterminal={}\thidden=0\tno_display={}\tautostart_enabled={}\tlaunch={}\targs={}\tenv={}",
                registry_value(&desktop_id)?,
                desktop_startup_mode(startup_mode),
                registry_value(&entry.display_name)?,
                registry_value(image)?,
                registry_value(exec)?,
                entry.weight_micros,
                if entry.logical_admin { 1 } else { 0 },
                if entry.console_hosted { 1 } else { 0 },
                if entry.console_hosted { 1 } else { 0 },
                if no_display { 1 } else { 0 },
                if autostart_enabled { 1 } else { 0 },
                launch,
                args,
                env,
            ));
        }
    }

    write_registry_lines(config.image_dir.join(RUNTIME_LAUNCH_REGISTRY_PATH), &lines)
}

fn desktop_file_id(manifest: &PackageManifest, index: usize) -> String {
    if manifest.desktop.entries.len() <= 1 {
        return format!("{}.desktop", manifest.id);
    }
    format!("{}-{}.desktop", manifest.id, index + 1)
}

fn desktop_exec_line(argv: &[String]) -> Result<String> {
    let mut tokens = Vec::with_capacity(argv.len());
    for arg in argv {
        let value = desktop_value(arg)?;
        if value.contains(char::is_whitespace) {
            tokens.push(format!("\"{}\"", value.replace('"', "\\\"")));
        } else {
            tokens.push(value);
        }
    }
    Ok(tokens.join(" "))
}

fn desktop_startup_mode(mode: StartupMode) -> &'static str {
    match mode {
        StartupMode::None => "none",
        StartupMode::Init => "init",
        StartupMode::Session => "session",
        StartupMode::Desktop => "desktop",
    }
}

fn desktop_bool(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn desktop_autostart_enabled(config: &Config, desktop_file_id: &str) -> Result<bool> {
    let path = config
        .image_dir
        .join("etc/xdg/autostart")
        .join(desktop_file_id);
    if !path.is_file() {
        return Ok(false);
    }

    let contents = fs::read_to_string(&path)?;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "X-GNOME-Autostart-enabled" {
            return Ok(matches!(value.trim(), "1" | "true" | "True" | "yes" | "Yes"));
        }
    }

    Ok(true)
}

fn desktop_value(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        return Err(format!("desktop value contains unsupported newline: {value:?}").into());
    }
    Ok(value.to_owned())
}

fn write_startup_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests {
        if matches!(manifest.startup, StartupMode::None) || !manifest.artifact_path(config).exists()
        {
            continue;
        }
        let entry = manifest.desktop.entries.first().ok_or_else(|| {
            format!(
                "package {} has startup mode but no desktop entries",
                manifest.id
            )
        })?;
        let image = entry.image.as_deref().unwrap_or(&manifest.install.path);
        let exec = entry.exec.as_deref().unwrap_or(image);
        let launch = match entry.launch {
            DesktopLaunchMode::None => "none",
            DesktopLaunchMode::NewSession => "new-session",
            DesktopLaunchMode::AllSessions => "all-sessions",
        };
        let mode = match manifest.startup {
            StartupMode::None => "none",
            StartupMode::Init => "init",
            StartupMode::Session => "session",
            StartupMode::Desktop => "desktop",
        };
        let desktop_id = desktop_file_id(manifest, 0);
        lines.push(format!(
            "desktop_id={}\tmode={}\tdisplay_name={}\texec={}\tlaunch={}",
            registry_value(&desktop_id)?,
            mode,
            registry_value(&entry.display_name)?,
            registry_value(exec)?,
            launch,
        ));
    }
    write_registry_lines(config.image_dir.join(STARTUP_REGISTRY_PATH), &lines)
}

fn write_windows_dll_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests
        .iter()
        .filter(|manifest| manifest.build.builder == BuilderKind::WinsysDllBundle)
    {
        let artifact_dir = manifest.artifact_path(config);
        if !artifact_dir.is_dir() {
            continue;
        }
        let mut entries =
            fs::read_dir(&artifact_dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| format!("non-utf8 DLL name in {}", artifact_dir.display()))?;
            lines.push(path_join_unix(&manifest.install.path, Path::new(&name))?);
        }
    }
    write_registry_lines(config.image_dir.join(WINDOWS_DLL_REGISTRY_PATH), &lines)
}

fn write_registry_lines(path: PathBuf, lines: &[String]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("registry destination has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut content = String::new();
    for line in lines {
        content.push_str(line);
        content.push('\n');
    }
    fs::write(path, content)?;
    Ok(())
}

fn registry_value(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') || value.contains('\t') {
        return Err(format!("registry value contains unsupported whitespace: {value:?}").into());
    }
    Ok(value.to_owned())
}

fn path_join_unix(prefix: &str, suffix: &Path) -> Result<String> {
    let suffix = suffix
        .to_str()
        .ok_or_else(|| format!("non-utf8 relative path: {}", suffix.display()))?
        .replace('\\', "/");
    Ok(format!("{prefix}/{suffix}"))
}

fn cleanup_legacy_build_layout(config: &Config) -> Result<()> {
    for relative in LEGACY_BUILD_DIRS {
        remove_dir_if_exists(&config.build_dir.join(relative))?;
    }

    remove_file_if_exists(&config.image_dir.join("startup.nsh"))?;

    for relative in LEGACY_BUILD_FILES {
        remove_file_if_exists(&config.build_dir.join(relative))?;
    }

    for relative in LEGACY_ROOT_FILES {
        remove_file_if_exists(&config.root_dir.join(relative))?;
    }

    Ok(())
}

fn stage_image_asset_overlay(src_root: &Path, dst_root: &Path) -> Result<()> {
    copy_tree_files(src_root, dst_root)
}

fn generate_dynamic_linker_cache(image_dir: &Path) -> Result<()> {
    let ld_so_conf = image_dir.join(LD_SO_CONF_PATH);
    if !ld_so_conf.is_file() {
        return Ok(());
    }

    let Some(ldconfig) = command_in_path("ldconfig") else {
        return Err(format!(
            "missing ldconfig required to generate dynamic linker cache from {}",
            ld_so_conf.display()
        )
        .into());
    };

    run_command(Command::new(ldconfig).arg("-r").arg(image_dir))
}
