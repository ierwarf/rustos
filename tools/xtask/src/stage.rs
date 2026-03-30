use std::fs;
use std::path::{Path, PathBuf};

use xshell::cmd;

use crate::config::Config;
use crate::package_manifest::{
    load_manifests, BuilderKind, DesktopLaunchMode, InstallLayout, PackageManifest,
    DEFAULT_PROFILE,
};
use crate::util::{
    copy_or_unpack_firmware, copy_with_parent, maybe_copy_dual_host_runtime,
    maybe_copy_host_runtime, push_boot_entry_unique, remove_dir_if_exists, remove_file_if_exists,
    shell, write_boot_file_list,
};
use crate::Result;

const DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";
const DESKTOP_REGISTRY_PATH: &str = "system/registry/system/desktop-programs.tsv";
const WINDOWS_DLL_REGISTRY_PATH: &str = "system/registry/compat/windows-system-dlls.txt";
const LEGACY_BOOT_FILE_LIST_PATH: &str = "BOOTFILES.TXT";

pub(crate) fn stage(config: &Config) -> Result<()> {
    let manifests = selected_manifests(config)?;

    remove_dir_if_exists(&config.image_dir)?;
    cleanup_legacy_build_layout(config)?;

    let mut boot_entries = Vec::new();
    stage_image_asset_overlay(
        &config.image_asset_overlay_dir,
        &config.image_dir,
        &mut boot_entries,
    )?;

    for manifest in &manifests {
        stage_manifest(config, manifest, &mut boot_entries)?;
    }

    fs::create_dir_all(&config.amdgpu_image_firmware_dir)?;
    for basename in &config.amdgpu_required_firmware_basenames {
        let dst = config.amdgpu_image_firmware_dir.join(basename);
        copy_or_unpack_firmware(&config.amdgpu_firmware_dir, basename, &dst)?;
        push_boot_entry_unique(
            &mut boot_entries,
            &format!("system/firmware/amdgpu/{basename}"),
        );
    }

    maybe_copy_host_runtime(
        &config.glibc_interpreter_source,
        &config.glibc_interpreter_dest,
        "lib64/ld-linux-x86-64.so.2",
        &mut boot_entries,
    )?;
    maybe_copy_dual_host_runtime(
        &config.glibc_libc_source,
        &config.glibc_libc_primary_dest,
        &config.glibc_libc_fallback_dest,
        "lib/x86_64-linux-gnu/libc.so.6",
        "lib64/libc.so.6",
        &mut boot_entries,
    )?;
    maybe_copy_dual_host_runtime(
        &config.glibc_libgcc_source,
        &config.glibc_libgcc_primary_dest,
        &config.glibc_libgcc_fallback_dest,
        "lib/x86_64-linux-gnu/libgcc_s.so.1",
        "lib64/libgcc_s.so.1",
        &mut boot_entries,
    )?;

    if let Some(ldconfig) = config.ldconfig.as_ref() {
        if config.glibc_libc_primary_dest.is_file() {
            let sh = shell()?;
            let ldso_cache_parent = config
                .glibc_ldso_cache_dest
                .parent()
                .ok_or("ld.so.cache destination has no parent")?;
            fs::create_dir_all(ldso_cache_parent)?;
            let ldso_conf = config.image_dir.join("etc/ld.so.conf");
            fs::write(&ldso_conf, "include /etc/ld.so.conf.d/*.conf\n")?;
            fs::create_dir_all(config.image_dir.join("etc/ld.so.conf.d"))?;
            let root = &config.image_dir;
            cmd!(sh, "{ldconfig} -r {root} -C /etc/ld.so.cache").run()?;
            push_boot_entry_unique(&mut boot_entries, "etc/ld.so.cache");
        }
    }

    fs::create_dir_all(
        config
            .glibc_ldso_preload_dest
            .parent()
            .ok_or("ld.so.preload destination has no parent")?,
    )?;
    fs::write(&config.glibc_ldso_preload_dest, [])?;
    push_boot_entry_unique(&mut boot_entries, "etc/ld.so.preload");

    write_driver_registry(config, &manifests)?;
    write_desktop_registry(config, &manifests)?;
    write_windows_dll_registry(config, &manifests)?;
    push_boot_entry_unique(&mut boot_entries, DRIVER_REGISTRY_PATH);
    push_boot_entry_unique(&mut boot_entries, DESKTOP_REGISTRY_PATH);
    push_boot_entry_unique(&mut boot_entries, WINDOWS_DLL_REGISTRY_PATH);

    boot_entries.sort();
    if boot_entries.is_empty() {
        remove_file_if_exists(&config.boot_file_list)?;
        remove_file_if_exists(&config.image_dir.join(LEGACY_BOOT_FILE_LIST_PATH))?;
    } else {
        write_boot_file_list(&config.boot_file_list, &boot_entries)?;
        write_boot_file_list(&config.image_dir.join(LEGACY_BOOT_FILE_LIST_PATH), &boot_entries)?;
    }
    Ok(())
}

fn selected_manifests(config: &Config) -> Result<Vec<PackageManifest>> {
    Ok(load_manifests(&config.root_dir)?
        .into_iter()
        .filter(|manifest| manifest.profile_enabled(DEFAULT_PROFILE))
        .collect())
}

fn stage_manifest(
    config: &Config,
    manifest: &PackageManifest,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    let artifact = manifest.artifact_path(config);
    let image = manifest.image_path(config);

    match manifest.install.layout {
        InstallLayout::File => {
            if artifact.is_file() {
                copy_with_parent(&artifact, &image)?;
                if manifest.boot.preload {
                    push_boot_entry_unique(boot_entries, &manifest.install.path);
                }
                return Ok(());
            }
        }
        InstallLayout::Directory => {
            if artifact.is_dir() {
                stage_directory_manifest(&artifact, &image, &manifest.install.path, manifest, boot_entries)?;
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

fn stage_directory_manifest(
    src_root: &Path,
    dst_root: &Path,
    install_prefix: &str,
    manifest: &PackageManifest,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    let mut stack = vec![(src_root.to_path_buf(), dst_root.to_path_buf())];

    while let Some((src_dir, dst_dir)) = stack.pop() {
        let mut entries = fs::read_dir(&src_dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries.into_iter().rev() {
            let src_path = entry.path();
            let dst_path = dst_dir.join(entry.file_name());
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push((src_path, dst_path));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            copy_with_parent(&src_path, &dst_path)?;
            if manifest.boot.preload {
                let relative = dst_path
                    .strip_prefix(dst_root)
                    .map_err(|_| format!("failed to relativize {}", dst_path.display()))?;
                let boot_entry = path_join_unix(install_prefix, relative)?;
                push_boot_entry_unique(boot_entries, &boot_entry);
            }
        }
    }

    Ok(())
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

fn write_desktop_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests {
        if !manifest.artifact_path(config).exists() {
            continue;
        }
        for entry in &manifest.desktop.entries {
            let image = entry.image.as_deref().unwrap_or(&manifest.install.path);
            let exec = entry.exec.as_deref().unwrap_or(image);
            let args = if entry.args.is_empty() {
                String::new()
            } else {
                entry.args
                    .iter()
                    .map(|arg| registry_value(arg))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let env = if entry.env.is_empty() {
                String::new()
            } else {
                entry.env
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
            lines.push(format!(
                "display_name={}\timage={}\texec={}\tweight={}\tlogical_admin={}\tconsole_hosted={}\tlaunch={}\targs={}\tenv={}",
                registry_value(&entry.display_name)?,
                registry_value(image)?,
                registry_value(exec)?,
                entry.weight_micros,
                if entry.logical_admin { 1 } else { 0 },
                if entry.console_hosted { 1 } else { 0 },
                launch,
                args,
                env,
            ));
        }
    }
    write_registry_lines(config.image_dir.join(DESKTOP_REGISTRY_PATH), &lines)
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
        let mut entries = fs::read_dir(&artifact_dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
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
    for path in [
        config.build_dir.join("EFI"),
        config.build_dir.join("etc"),
        config.build_dir.join("lib"),
        config.build_dir.join("lib64"),
        config.build_dir.join("linux"),
        config.build_dir.join("SYSTEM"),
    ] {
        remove_dir_if_exists(&path)?;
    }

    for path in [
        config.boot_file_list.clone(),
        config.image_dir.join(LEGACY_BOOT_FILE_LIST_PATH),
        config.image_dir.join("startup.nsh"),
        config.build_dir.join("kernel.elf"),
        config.build_dir.join("prekernel.elf"),
        config.build_dir.join("UISERVER.ELF"),
        config.build_dir.join("UISERVER.EXE"),
        config.build_dir.join("SHELL.ELF"),
        config.build_dir.join("EXECSMOKE.ELF"),
        config.build_dir.join("USERDEMO.ELF"),
        config.build_dir.join("USERDEMO.EXE"),
        config.build_dir.join("NvVars"),
        config.build_dir.join("artifacts/boot/BOOTX64.EFI"),
        config.build_dir.join("background.jpg"),
        config.build_dir.join("sonic.gif"),
        config.root_dir.join("debugcon.log"),
        config.root_dir.join("qemu_interrupt.log"),
    ] {
        remove_file_if_exists(&path)?;
    }

    Ok(())
}

fn stage_image_asset_overlay(
    src_root: &Path,
    dst_root: &Path,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    if !src_root.is_dir() {
        return Ok(());
    }

    stage_image_asset_overlay_recursive(src_root, src_root, dst_root, boot_entries)
}

fn stage_image_asset_overlay_recursive(
    src_root: &Path,
    current_dir: &Path,
    dst_root: &Path,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(current_dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let src_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            stage_image_asset_overlay_recursive(src_root, &src_path, dst_root, boot_entries)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let relative = src_path
            .strip_prefix(src_root)
            .map_err(|_| format!("asset path escaped overlay root: {}", src_path.display()))?;
        let dst_path = dst_root.join(relative);
        copy_with_parent(&src_path, &dst_path)?;

        let boot_entry = relative
            .to_str()
            .ok_or_else(|| format!("non-utf8 asset path is unsupported: {}", relative.display()))?
            .replace('\\', "/");
        push_boot_entry_unique(boot_entries, &boot_entry);
    }

    Ok(())
}
