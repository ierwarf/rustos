use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use fatfs::Seek as FatSeek;
use fatfs::Write as FatWrite;
use walkdir::WalkDir;

use crate::Result;
use crate::config::Config;
use crate::package_manifest::{
    BuilderKind, DesktopEntrySpec, DesktopLaunchMode, InstallLayout, PackageManifest, StartupMode,
    load_default_manifests,
};
use crate::util::{
    command_in_path, copy_or_unpack_firmware, copy_tree_files, copy_with_parent,
    remove_dir_if_exists, remove_file_if_exists, run_command,
};

const APPLICATIONS_DIR: &str = "usr/share/applications";
const DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";
const ROOT_FILE_EXTENTS_REGISTRY_PATH: &str = "system/registry/kernel/root-file-extents.tsv";
const ROOT_FILE_EXTENTS_REGISTRY_SIGNATURE_PATH: &str =
    "system/registry/kernel/root-file-extents.tsv.sig";
const DESKTOP_REGISTRY_PATH: &str = "system/registry/system/desktop-programs.tsv";
const RUNTIME_LAUNCH_REGISTRY_PATH: &str = "system/registry/system/runtime-launch-programs.tsv";
const STARTUP_REGISTRY_PATH: &str = "system/registry/system/startup-programs.tsv";
const LINUX_RUNTIME_ACCESS_REGISTRY_PATH: &str = "system/registry/system/linux-runtime-access.tsv";
const RUNTIME_ENV_REGISTRY_PATH: &str = "system/registry/system/runtime-env.tsv";
const WINDOWS_DLL_REGISTRY_PATH: &str = "system/registry/compat/windows-system-dlls.txt";
const LD_SO_CONF_PATH: &str = "etc/ld.so.conf";
const DEFAULT_RUNTIME_LIBRARY_DIRS: &[&str] = &["/lib", "/lib64", "/usr/lib", "/usr/lib64"];
const DEFAULT_RUNTIME_LINKER_FILES: &[&str] =
    &["/etc/ld.so.cache", "/etc/ld.so.preload", "/etc/ld.so.conf"];
const DEFAULT_RUNTIME_LINKER_INCLUDE_DIR: &str = "/etc/ld.so.conf.d";
const DEFAULT_RUNTIME_ASSET_DIRS: &[&str] = &[
    "/usr/lib/locale",
    "/usr/share/locale",
    "/usr/lib/gconv",
    "/usr/share/zoneinfo",
];
const DEFAULT_RUNTIME_ASSET_FILES: &[&str] = &[
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/localtime",
    "/system/registry/system/desktop-programs.tsv",
    "/system/registry/system/startup-programs.tsv",
    "/system/registry/system/runtime-launch-programs.tsv",
];
const DEFAULT_INIT_ENV: &[(&str, &str)] = &[
    ("PATH", "/bin:/usr/bin:/usr/local/bin"),
    ("HOME", "/home/user"),
    ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ("WAYLAND_DISPLAY", "wayland-0"),
];
const DEFAULT_RUNTIME_ENV: &[(&str, &str)] = &[
    ("PATH", "/bin:/usr/bin:/usr/local/bin"),
    ("HOME", "/home/user"),
    ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ("WAYLAND_DISPLAY", "wayland-0"),
    ("XDG_SESSION_TYPE", "wayland"),
    ("XDG_CURRENT_DESKTOP", "RustOS"),
];
const STALE_BUILD_DIRS: &[&str] = &["EFI", "etc", "lib", "lib64", "linux", "SYSTEM"];
const STALE_BUILD_FILES: &[&str] = &[
    "kernel.elf",
    "nucleus.elf",
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
const STALE_ROOT_FILES: &[&str] = &["debugcon.log"];
const ABI_FUZZ_PACKAGE_ID: &str = "abifuzz";
const ABI_FUZZ_DESKTOP_ID: &str = "abifuzz.desktop";
const DEFAULT_GRUB_DEV_KEY: &str = "RustOS Dev GRUB <rustos-dev-grub@example.invalid>";

pub(crate) fn stage(config: &Config) -> Result<()> {
    let manifests = load_default_manifests(&config.root_dir)?;

    remove_dir_if_exists(&config.image_dir)?;
    cleanup_legacy_build_layout(config)?;

    stage_image_asset_overlay(&config.image_asset_overlay_dir, &config.image_dir)?;
    apply_configured_autostart_policy(config)?;

    for manifest in &manifests {
        stage_manifest(config, manifest)?;
    }
    stage_boot_manager(config)?;
    stage_nucleus_signature(config)?;

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
    write_linux_runtime_access_registry(config)?;
    write_runtime_env_registry(config)?;
    generate_dynamic_linker_cache(&config.image_dir)?;
    write_boot_disk_image(config)?;
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

    Err(anyhow!(
        "missing staged artifact for package {} at {}",
        manifest.id,
        artifact.display()
    ))
}

fn stage_directory_manifest(src_root: &Path, dst_root: &Path) -> Result<()> {
    copy_tree_files(src_root, dst_root)
}

fn stage_boot_manager(config: &Config) -> Result<()> {
    let artifact = config.artifact_boot_efi_path();
    if artifact.is_file() {
        copy_with_parent(&artifact, &config.boot_efi_path())?;
    }
    Ok(())
}

fn stage_nucleus_signature(config: &Config) -> Result<()> {
    let nucleus = config.artifact_nucleus_elf_path();
    let signature = config.artifact_nucleus_signature_path();
    if !nucleus.is_file() {
        return Ok(());
    }

    if !signature.is_file() {
        bail!(
            "missing nucleus signature for {}; run cargo xtask build-efi or set RUSTOS_GRUB_SIGNING_KEY before cargo xtask build-kernel",
            nucleus.display()
        );
    }

    ensure_signature_is_fresh(&nucleus, &signature)?;
    copy_with_parent(&signature, &config.image_dir.join("nucleus.elf.sig"))?;
    Ok(())
}

fn ensure_signature_is_fresh(input: &Path, signature: &Path) -> Result<()> {
    let input_modified = fs::metadata(input)?.modified()?;
    let signature_modified = fs::metadata(signature)?.modified()?;
    if signature_modified < input_modified {
        bail!(
            "stale nucleus signature: {} is older than {}; run cargo xtask build-kernel or cargo xtask build-efi",
            signature.display(),
            input.display()
        );
    }

    Ok(())
}

fn write_boot_disk_image(config: &Config) -> Result<()> {
    let files = collect_image_files(&config.image_dir)?;
    let payload_bytes = files
        .iter()
        .map(|file| file.len)
        .try_fold(0_u64, |acc, len| {
            acc.checked_add(len).context("image payload is too large")
        })?;
    let image_bytes = boot_disk_image_len(payload_bytes);
    if let Some(parent) = config.boot_disk_image.parent() {
        fs::create_dir_all(parent)?;
    }

    let image = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&config.boot_disk_image)?;
    image.set_len(image_bytes)?;
    let mut image = fatfs::StdIoWrapper::new(image);
    fatfs::format_volume(
        &mut image,
        fatfs::FormatVolumeOptions::new()
            .bytes_per_sector(512)
            .bytes_per_cluster(32 * 1024)
            .volume_label(*b"RUSTOS     ")
            .volume_id(0x5255_5354),
    )?;
    image.seek(fatfs::SeekFrom::Start(0))?;
    let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new())?;
    let mut extent_entries = Vec::new();
    let extent_manifest;
    {
        let root = fs.root_dir();
        for file in &files {
            ensure_fat_parent_dirs(&root, &file.relative)?;
            let mut dst = root.create_file(file.relative.as_str())?;
            dst.truncate()?;
            copy_host_file_to_fat(&file.source, &mut dst)?;
            dst.flush()?;
            extent_entries.push(BootDiskExtentEntry {
                path: format!("/{}", file.relative),
                len: file.len,
                extents: collect_fat_file_extents(&mut dst)?,
            });
        }
        extent_manifest = write_root_file_extents_registry(&root, &extent_entries)?;
        write_root_file_extents_signature(config, &root, extent_manifest.as_bytes())?;
    }
    fs.unmount()?;
    verify_boot_disk_image_contract(
        &config.boot_disk_image,
        &files,
        &extent_entries,
        extent_manifest.as_str(),
    )?;
    Ok(())
}

struct BootDiskExtentEntry {
    path: String,
    len: u64,
    extents: Vec<BootDiskFileExtent>,
}

struct BootDiskFileExtent {
    offset: u64,
    len: u64,
}

fn verify_boot_disk_image_contract(
    boot_disk_image: &Path,
    files: &[ImageFile],
    extent_entries: &[BootDiskExtentEntry],
    expected_manifest: &str,
) -> Result<()> {
    if files.len() != extent_entries.len() {
        bail!(
            "boot extent contract entry count mismatch: files={} extents={}",
            files.len(),
            extent_entries.len()
        );
    }

    let mut raw_disk = fs::File::open(boot_disk_image)
        .with_context(|| format!("failed to reopen boot disk {}", boot_disk_image.display()))?;
    for (file, entry) in files.iter().zip(extent_entries) {
        let expected_path = format!("/{}", file.relative);
        if entry.path != expected_path || entry.len != file.len {
            bail!(
                "boot extent contract metadata mismatch for {}",
                file.source.display()
            );
        }

        let expected = fs::read(&file.source)
            .with_context(|| format!("failed to reread staged source {}", file.source.display()))?;
        let expected_len = usize::try_from(file.len)
            .context("staged file length does not fit host address space")?;
        if expected.len() != expected_len {
            bail!(
                "staged source changed while writing boot disk: {}",
                file.source.display()
            );
        }

        let mut actual = Vec::with_capacity(expected_len);
        for extent in &entry.extents {
            let extent_len = usize::try_from(extent.len)
                .context("boot extent length does not fit host address space")?;
            let next_len = actual
                .len()
                .checked_add(extent_len)
                .context("boot extent length overflow")?;
            if next_len > expected_len {
                bail!("boot extents exceed staged file length for {}", entry.path);
            }
            raw_disk
                .seek(SeekFrom::Start(extent.offset))
                .with_context(|| format!("failed to seek boot extent for {}", entry.path))?;
            let start = actual.len();
            actual.resize(next_len, 0);
            raw_disk
                .read_exact(&mut actual[start..])
                .with_context(|| format!("failed to read boot extent for {}", entry.path))?;
        }
        if actual != expected {
            bail!("boot extent payload mismatch for {}", entry.path);
        }
    }

    let image =
        fatfs::StdIoWrapper::new(fs::File::open(boot_disk_image).with_context(|| {
            format!("failed to reopen boot disk {}", boot_disk_image.display())
        })?);
    let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new())?;
    let actual_manifest = {
        let root = fs.root_dir();
        let mut manifest_file = root.open_file(ROOT_FILE_EXTENTS_REGISTRY_PATH)?;
        let mut bytes = Vec::new();
        manifest_file.read_to_end(&mut bytes)?;
        bytes
    };
    fs.unmount()?;
    if actual_manifest != expected_manifest.as_bytes() {
        bail!("boot extent manifest payload mismatch after image write");
    }
    Ok(())
}

fn collect_fat_file_extents<D: fatfs::ReadWriteSeek>(
    file: &mut fatfs::File<'_, D, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
) -> Result<Vec<BootDiskFileExtent>>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let mut extents: Vec<BootDiskFileExtent> = Vec::new();
    for extent in file.extents() {
        let extent = extent?;
        let len = u64::from(extent.size);
        if len == 0 {
            continue;
        }
        if let Some(last) = extents.last_mut()
            && last.offset.saturating_add(last.len) == extent.offset
        {
            last.len = last.len.saturating_add(len);
            continue;
        }
        extents.push(BootDiskFileExtent {
            offset: extent.offset,
            len,
        });
    }
    Ok(extents)
}

fn write_root_file_extents_registry<D: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, D, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
    entries: &[BootDiskExtentEntry],
) -> Result<String>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    ensure_fat_parent_dirs(root, ROOT_FILE_EXTENTS_REGISTRY_PATH)?;
    if root.open_file(ROOT_FILE_EXTENTS_REGISTRY_PATH).is_ok() {
        root.remove(ROOT_FILE_EXTENTS_REGISTRY_PATH)?;
    }

    let mut content = String::new();
    for entry in entries {
        content.push_str("path=");
        content.push_str(registry_value(entry.path.as_str())?.as_str());
        content.push_str("\tlen=");
        content.push_str(entry.len.to_string().as_str());
        content.push_str("\textents=");
        for (index, extent) in entry.extents.iter().enumerate() {
            if index != 0 {
                content.push(',');
            }
            content.push_str(extent.offset.to_string().as_str());
            content.push(':');
            content.push_str(extent.len.to_string().as_str());
        }
        content.push('\n');
    }

    let mut file = root.create_file(ROOT_FILE_EXTENTS_REGISTRY_PATH)?;
    file.truncate()?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    Ok(content)
}

fn write_root_file_extents_signature<D: fatfs::ReadWriteSeek>(
    config: &Config,
    root: &fatfs::Dir<'_, D, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
    manifest: &[u8],
) -> Result<()>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let work_dir = config.build_dir.join("boot-extent-manifest");
    fs::create_dir_all(&work_dir)?;
    let manifest_path = work_dir.join("root-file-extents.tsv");
    let signature_path = work_dir.join("root-file-extents.tsv.sig");
    fs::write(&manifest_path, manifest)?;
    sign_detached_for_grub(config, &manifest_path, &signature_path)?;

    ensure_fat_parent_dirs(root, ROOT_FILE_EXTENTS_REGISTRY_SIGNATURE_PATH)?;
    if root
        .open_file(ROOT_FILE_EXTENTS_REGISTRY_SIGNATURE_PATH)
        .is_ok()
    {
        root.remove(ROOT_FILE_EXTENTS_REGISTRY_SIGNATURE_PATH)?;
    }
    let signature = fs::read(&signature_path)?;
    let mut file = root.create_file(ROOT_FILE_EXTENTS_REGISTRY_SIGNATURE_PATH)?;
    file.truncate()?;
    file.write_all(&signature)?;
    file.flush()?;
    Ok(())
}

fn sign_detached_for_grub(config: &Config, input: &Path, signature: &Path) -> Result<()> {
    remove_file_if_exists(signature)?;
    let gpg_home = config
        .rustos_gpg_home
        .clone()
        .unwrap_or_else(|| config.build_dir.join("dev-grub-gpg"));
    let signing_key = config
        .rustos_grub_signing_key
        .clone()
        .unwrap_or_else(|| String::from(DEFAULT_GRUB_DEV_KEY));
    let mut command = Command::new(&config.gpg);
    command
        .arg("--homedir")
        .arg(gpg_home)
        .arg("--batch")
        .arg("--yes")
        .arg("--pinentry-mode")
        .arg("loopback")
        .arg("--local-user")
        .arg(signing_key)
        .arg("--detach-sign")
        .arg("--output")
        .arg(signature)
        .arg(input);
    run_command(&mut command)
}

struct ImageFile {
    source: PathBuf,
    relative: String,
    len: u64,
}

fn collect_image_files(image_dir: &Path) -> Result<Vec<ImageFile>> {
    let mut files = WalkDir::new(image_dir)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry)),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
        .map(|entry| {
            let path = entry?.into_path();
            let relative = path
                .strip_prefix(image_dir)?
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let len = fs::metadata(&path)
                .map_err(|err| anyhow!("failed to stat image file {}: {err}", path.display()))?
                .len();
            Ok(ImageFile {
                source: path,
                relative,
                len,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    files.sort_by(|lhs, rhs| lhs.relative.cmp(&rhs.relative));
    Ok(files)
}

fn boot_disk_image_len(payload_bytes: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    const MIN_IMAGE_BYTES: u64 = 128 * MIB;
    let requested = payload_bytes.saturating_mul(2).saturating_add(64 * MIB);
    requested.max(MIN_IMAGE_BYTES).div_ceil(MIB) * MIB
}

fn ensure_fat_parent_dirs<D: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, D, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
    path: &str,
) -> Result<()>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let Some((parent, _file_name)) = path.rsplit_once('/') else {
        return Ok(());
    };
    let mut current = String::new();
    for component in parent.split('/') {
        if component.is_empty() {
            continue;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        if root.open_dir(current.as_str()).is_ok() {
            continue;
        }
        root.create_dir(current.as_str())?;
    }
    Ok(())
}

fn copy_host_file_to_fat<D: fatfs::ReadWriteSeek>(
    src: &Path,
    dst: &mut fatfs::File<'_, D, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
) -> Result<()>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let mut src = fs::File::open(src)?;
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = src.read(&mut buf)?;
        if read == 0 {
            break;
        }
        dst.write_all(&buf[..read])?;
    }
    Ok(())
}

fn write_driver_registry(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut lines = Vec::new();
    for manifest in manifests {
        let Some(autoload) = manifest.autoload.as_ref() else {
            continue;
        };
        if !autoload.enabled {
            continue;
        }
        if !manifest.artifact_path(config).is_file() {
            continue;
        }
        let aliases = registry_list(&autoload.aliases)?;
        let deps = registry_list(&autoload.deps)?;
        let softdeps = registry_list(&autoload.softdeps)?;
        let linux_driver_names = if autoload.linux_driver_names.is_empty() {
            registry_list(std::slice::from_ref(&autoload.name))?
        } else {
            registry_list(&autoload.linux_driver_names)?
        };
        lines.push(format!(
            "name={}\tclass={}\tbus={}\tpriority={}\tpath={}\twhen={}\taliases={}\tdeps={}\tsoftdeps={}\tlinux_driver_names={}\tprovider_group={}\tfallback_only={}",
            registry_value(&autoload.name)?,
            registry_value(&autoload.class)?,
            registry_value(&autoload.bus)?,
            autoload.priority,
            registry_value(&manifest.install.path)?,
            registry_value(autoload.when.as_deref().unwrap_or("vfs-ready"))?,
            aliases,
            deps,
            softdeps,
            linux_driver_names,
            registry_value(autoload.provider_group.as_deref().unwrap_or(""))?,
            if autoload.fallback_only { 1 } else { 0 },
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
            let deps = registry_deps(&manifest.runtime_deps)?;
            let argv = if entry.args.is_empty() {
                vec![exec.to_string()]
            } else {
                entry.args.clone()
            };
            let content = format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={exec_line}\nTerminal={terminal}\nOnlyShowIn=RustOS;\nNoDisplay={no_display}\nX-RustOS-DesktopId={desktop_id}\nX-RustOS-PackageId={package_id}\nX-RustOS-Startup={startup}\nX-RustOS-Deps={deps}\nX-RustOS-WeightMicros={weight}\nX-RustOS-LogicalAdmin={logical_admin}\nX-RustOS-ConsoleHosted={console_hosted}\nX-RustOS-Argv={argv}\nX-RustOS-Env={env}\n",
                name = desktop_value(&entry.display_name)?,
                exec_line = desktop_exec_line(&argv)?,
                terminal = desktop_bool(entry.console_hosted),
                no_display = desktop_bool(
                    manifest.kind == crate::package_manifest::PackageKind::Service
                        || entry.no_display,
                ),
                desktop_id = desktop_value(&desktop_file_id)?,
                package_id = desktop_value(&manifest.id)?,
                startup = desktop_startup_mode(manifest.startup),
                deps = desktop_value(&deps)?,
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
        if !runtime_launch_enabled_for_manifest(config, manifest) {
            continue;
        }
        if !manifest.artifact_path(config).exists() {
            continue;
        }
        for (index, entry) in manifest.desktop.entries.iter().enumerate() {
            let desktop_id = desktop_file_id(manifest, index);
            let image = entry.image.as_deref().unwrap_or(&manifest.install.path);
            let exec = entry.exec.as_deref().unwrap_or(image);
            let args = runtime_launch_args(config, manifest, entry, exec);
            let args = if args.is_empty() {
                String::new()
            } else {
                args.iter()
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
            let no_display =
                manifest.kind == crate::package_manifest::PackageKind::Service || entry.no_display;
            let autostart_enabled = desktop_autostart_enabled(config, &desktop_id)?;
            let deps = registry_deps(&manifest.runtime_deps)?;
            lines.push(format!(
                "desktop_id={}\tpackage_id={}\tstartup={}\tdisplay_name={}\timage={}\texec={}\tweight={}\tlogical_admin={}\tconsole_hosted={}\tterminal={}\thidden=0\tno_display={}\tautostart_enabled={}\tlaunch={}\tdeps={}\targs={}\tenv={}",
                registry_value(&desktop_id)?,
                registry_value(&manifest.id)?,
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
                deps,
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
        if !runtime_launch_enabled_for_manifest(config, manifest) {
            continue;
        }
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
            let args = runtime_launch_args(config, manifest, entry, exec);
            let args = if args.is_empty() {
                String::new()
            } else {
                args.iter()
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
            let no_display =
                manifest.kind == crate::package_manifest::PackageKind::Service || entry.no_display;
            let deps = registry_deps(&manifest.runtime_deps)?;
            lines.push(format!(
                "desktop_id={}\tpackage_id={}\tstartup={}\tdisplay_name={}\timage={}\texec={}\tweight={}\tlogical_admin={}\tconsole_hosted={}\tterminal={}\thidden=0\tno_display={}\tautostart_enabled={}\tlaunch={}\tdeps={}\targs={}\tenv={}",
                registry_value(&desktop_id)?,
                registry_value(&manifest.id)?,
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
                deps,
                args,
                env,
            ));
        }
    }

    write_registry_lines(config.image_dir.join(RUNTIME_LAUNCH_REGISTRY_PATH), &lines)
}

fn apply_configured_autostart_policy(config: &Config) -> Result<()> {
    if config.project.fuzzing.enabled {
        return Ok(());
    }
    remove_file_if_exists(
        &config
            .image_dir
            .join("etc/xdg/autostart")
            .join(ABI_FUZZ_DESKTOP_ID),
    )
}

fn runtime_launch_enabled_for_manifest(config: &Config, manifest: &PackageManifest) -> bool {
    manifest.id != ABI_FUZZ_PACKAGE_ID || config.project.fuzzing.enabled
}

fn runtime_launch_args(
    config: &Config,
    manifest: &PackageManifest,
    entry: &DesktopEntrySpec,
    exec: &str,
) -> Vec<String> {
    let mut args = entry.args.clone();
    if manifest.id == ABI_FUZZ_PACKAGE_ID {
        if args.is_empty() {
            args.push(exec.to_string());
        }
        if config.project.fuzzing.startup_delay_ms > 0 {
            args.push(format!(
                "--delay-ms={}",
                config.project.fuzzing.startup_delay_ms
            ));
        }
        if config.project.fuzzing.fd_transfer_stress {
            args.push(String::from("--fd-transfer-stress"));
        }
    }
    args
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
            return Ok(matches!(
                value.trim(),
                "1" | "true" | "True" | "yes" | "Yes"
            ));
        }
    }

    Ok(true)
}

fn desktop_value(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        bail!("desktop value contains unsupported newline: {value:?}");
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
        let entry = manifest.desktop.entries.first().with_context(|| {
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
        let deps = registry_deps(&manifest.runtime_deps)?;
        lines.push(format!(
            "desktop_id={}\tpackage_id={}\tmode={}\tdisplay_name={}\texec={}\tlaunch={}\tdeps={}",
            registry_value(&desktop_id)?,
            registry_value(&manifest.id)?,
            mode,
            registry_value(&entry.display_name)?,
            registry_value(exec)?,
            launch,
            deps,
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
                .map_err(|_| anyhow!("non-utf8 DLL name in {}", artifact_dir.display()))?;
            lines.push(path_join_unix(&manifest.install.path, Path::new(&name))?);
        }
    }
    write_registry_lines(config.image_dir.join(WINDOWS_DLL_REGISTRY_PATH), &lines)
}

fn write_linux_runtime_access_registry(config: &Config) -> Result<()> {
    let mut policy = RuntimeAccessRegistry::default();
    for dir in DEFAULT_RUNTIME_LIBRARY_DIRS {
        policy.allow_dir(dir);
    }
    for file in DEFAULT_RUNTIME_LINKER_FILES {
        policy.allow_file(file);
    }
    policy.allow_dir(DEFAULT_RUNTIME_LINKER_INCLUDE_DIR);
    for dir in DEFAULT_RUNTIME_ASSET_DIRS {
        policy.allow_dir(dir);
    }
    for file in DEFAULT_RUNTIME_ASSET_FILES {
        policy.allow_file(file);
    }

    let mut visited = Vec::new();
    load_runtime_linker_config_file(
        &config.image_dir,
        "/etc/ld.so.conf",
        &mut policy,
        &mut visited,
    );

    let mut lines = Vec::new();
    for dir in policy.dirs {
        lines.push(format!("kind=dir\tpath={}", registry_value(&dir)?));
    }
    for file in policy.files {
        lines.push(format!("kind=file\tpath={}", registry_value(&file)?));
    }
    write_registry_lines(
        config.image_dir.join(LINUX_RUNTIME_ACCESS_REGISTRY_PATH),
        &lines,
    )
}

fn write_runtime_env_registry(config: &Config) -> Result<()> {
    let mut lines = Vec::new();
    push_runtime_env_scope(&mut lines, "init", DEFAULT_INIT_ENV)?;
    push_runtime_env_scope(&mut lines, "runtime", DEFAULT_RUNTIME_ENV)?;
    write_registry_lines(config.image_dir.join(RUNTIME_ENV_REGISTRY_PATH), &lines)
}

fn push_runtime_env_scope(
    lines: &mut Vec<String>,
    scope: &str,
    entries: &[(&str, &str)],
) -> Result<()> {
    for (key, value) in entries {
        lines.push(format!(
            "scope={}\tkey={}\tvalue={}",
            registry_value(scope)?,
            registry_value(key)?,
            registry_value(value)?,
        ));
    }
    Ok(())
}

fn write_registry_lines(path: PathBuf, lines: &[String]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("registry destination has no parent: {}", path.display()))?;
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
        bail!("registry value contains unsupported whitespace: {value:?}");
    }
    Ok(value.to_owned())
}

fn registry_deps(deps: &[String]) -> Result<String> {
    registry_list(deps)
}

fn registry_list(values: &[String]) -> Result<String> {
    values
        .iter()
        .map(|value| {
            if value.contains(',') {
                bail!("registry list value contains unsupported separator: {value:?}");
            }
            registry_value(value)
        })
        .collect::<Result<Vec<_>>>()
        .map(|values| values.join(","))
}

#[derive(Default)]
struct RuntimeAccessRegistry {
    dirs: Vec<String>,
    files: Vec<String>,
}

impl RuntimeAccessRegistry {
    fn allow_dir(&mut self, path: &str) {
        let Some(path) = normalize_runtime_access_path(path) else {
            return;
        };
        push_unique_string(&mut self.dirs, path);
    }

    fn allow_file(&mut self, path: &str) {
        let Some(path) = normalize_runtime_access_path(path) else {
            return;
        };
        push_unique_string(&mut self.files, path);
    }
}

fn load_runtime_linker_config_file(
    image_dir: &Path,
    path: &str,
    policy: &mut RuntimeAccessRegistry,
    visited: &mut Vec<String>,
) {
    let Some(path) = normalize_runtime_access_path(path) else {
        return;
    };
    if visited.iter().any(|current| current == &path) {
        return;
    }
    visited.push(path.clone());
    policy.allow_file(path.as_str());

    let host_path = image_path_for_absolute(image_dir, path.as_str());
    let Ok(text) = fs::read_to_string(&host_path) else {
        return;
    };
    for raw_line in text.lines() {
        let line = strip_runtime_config_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(include_spec) = parse_runtime_config_include(line) {
            load_runtime_linker_config_include(image_dir, include_spec, policy, visited);
            continue;
        }
        policy.allow_dir(line);
    }
}

fn load_runtime_linker_config_include(
    image_dir: &Path,
    include_spec: &str,
    policy: &mut RuntimeAccessRegistry,
    visited: &mut Vec<String>,
) {
    let Some(include_path) = normalize_runtime_access_path(include_spec) else {
        return;
    };
    if !include_path.contains('*') {
        load_runtime_linker_config_file(image_dir, include_path.as_str(), policy, visited);
        return;
    }

    let (dir, pattern) = split_runtime_include_path(include_path.as_str());
    let host_dir = image_path_for_absolute(image_dir, dir);
    let Ok(entries) = fs::read_dir(&host_dir) else {
        return;
    };
    let mut names = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    names.sort();
    for name in names {
        if !runtime_config_pattern_matches(pattern, name.as_str()) {
            continue;
        }
        let child = if dir == "/" {
            format!("/{name}")
        } else {
            format!("{dir}/{name}")
        };
        load_runtime_linker_config_file(image_dir, child.as_str(), policy, visited);
    }
}

fn image_path_for_absolute(image_dir: &Path, path: &str) -> PathBuf {
    image_dir.join(path.trim_start_matches('/'))
}

fn normalize_runtime_access_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return None;
    }

    let mut components = Vec::new();
    for component in trimmed.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        components.push(component);
    }

    let mut normalized = String::from("/");
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Some(normalized)
}

fn split_runtime_include_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("/", path))
}

fn strip_runtime_config_comment(line: &str) -> &str {
    line.split_once('#')
        .map(|(before, _)| before)
        .unwrap_or(line)
}

fn parse_runtime_config_include(line: &str) -> Option<&str> {
    let mut parts = line.split_whitespace();
    let directive = parts.next()?;
    if directive != "include" {
        return None;
    }
    parts.next()
}

fn runtime_config_pattern_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star = None;
    let mut retry_t = 0usize;

    while t < text.len() {
        if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
            continue;
        }
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry_t = t;
            continue;
        }
        if let Some(star_index) = star {
            p = star_index + 1;
            retry_t += 1;
            t = retry_t;
            continue;
        }
        return false;
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn push_unique_string(dest: &mut Vec<String>, value: String) {
    if dest.iter().any(|current| current == &value) {
        return;
    }
    dest.push(value);
}

fn path_join_unix(prefix: &str, suffix: &Path) -> Result<String> {
    let suffix = suffix
        .to_str()
        .with_context(|| format!("non-utf8 relative path: {}", suffix.display()))?
        .replace('\\', "/");
    Ok(format!("{prefix}/{suffix}"))
}

fn cleanup_legacy_build_layout(config: &Config) -> Result<()> {
    // Keep staging cleanup bounded to known stale paths; a broad build tree walk
    // makes `cargo xtask stage` slower and can remove still-valid artifacts.
    for relative in STALE_BUILD_DIRS {
        remove_dir_if_exists(&config.build_dir.join(relative))?;
    }

    remove_file_if_exists(&config.image_dir.join("startup.nsh"))?;

    for relative in STALE_BUILD_FILES {
        remove_file_if_exists(&config.build_dir.join(relative))?;
    }

    for relative in STALE_ROOT_FILES {
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
        bail!(
            "missing ldconfig required to generate dynamic linker cache from {}",
            ld_so_conf.display()
        );
    };

    run_command(Command::new(ldconfig).arg("-r").arg(image_dir))
}

#[cfg(test)]
mod tests {
    use super::{
        BootDiskExtentEntry, ImageFile, collect_fat_file_extents, ensure_fat_parent_dirs,
        registry_deps, registry_list, verify_boot_disk_image_contract,
        write_root_file_extents_registry,
    };
    use fatfs::{Seek as _, Write as _};
    use std::fs::OpenOptions;

    #[test]
    fn registry_deps_join_package_ids_with_commas() {
        let deps = vec!["runtimed".to_string(), "sessiond".to_string()];

        assert_eq!(registry_deps(&deps).unwrap(), "runtimed,sessiond");
    }

    #[test]
    fn registry_deps_allow_empty_dependency_list() {
        let deps = Vec::new();

        assert_eq!(registry_deps(&deps).unwrap(), "");
    }

    #[test]
    fn registry_deps_reject_registry_whitespace() {
        let deps = vec!["bad\tdep".to_string()];

        assert!(registry_deps(&deps).is_err());
    }

    #[test]
    fn registry_list_join_driver_metadata_with_commas() {
        let aliases = vec![
            "virtio:d00000010v*".to_string(),
            "platform:bootfb".to_string(),
        ];

        assert_eq!(
            registry_list(&aliases).unwrap(),
            "virtio:d00000010v*,platform:bootfb"
        );
    }

    #[test]
    fn registry_list_rejects_embedded_commas() {
        let aliases = vec!["virtio:d00000010v*,platform:bootfb".to_string()];

        assert!(registry_list(&aliases).is_err());
    }

    #[test]
    fn boot_disk_extent_contract_rechecks_raw_payload_and_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("rootd.elf");
        let payload = b"rootd bootstrap payload";
        std::fs::write(&source, payload).unwrap();

        let boot_disk = temp.path().join("rustos-boot.img");
        let image_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&boot_disk)
            .unwrap();
        image_file.set_len(2 * 1024 * 1024).unwrap();
        let mut image = fatfs::StdIoWrapper::new(image_file);
        fatfs::format_volume(&mut image, fatfs::FormatVolumeOptions::new()).unwrap();
        image.seek(fatfs::SeekFrom::Start(0)).unwrap();
        let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new()).unwrap();
        let (entry, manifest) = {
            let root = fs.root_dir();
            let relative = "services/rootd/rootd.elf";
            ensure_fat_parent_dirs(&root, relative).unwrap();
            let mut staged = root.create_file(relative).unwrap();
            staged.write_all(payload).unwrap();
            staged.flush().unwrap();
            let entry = BootDiskExtentEntry {
                path: format!("/{relative}"),
                len: payload.len() as u64,
                extents: collect_fat_file_extents(&mut staged).unwrap(),
            };
            let manifest =
                write_root_file_extents_registry(&root, std::slice::from_ref(&entry)).unwrap();
            (entry, manifest)
        };
        fs.unmount().unwrap();

        verify_boot_disk_image_contract(
            &boot_disk,
            &[ImageFile {
                source,
                relative: "services/rootd/rootd.elf".to_string(),
                len: payload.len() as u64,
            }],
            &[entry],
            &manifest,
        )
        .unwrap();
    }
}
