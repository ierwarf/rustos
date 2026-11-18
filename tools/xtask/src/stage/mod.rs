use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use boot_protocol::{
    EARLY_SYSTEM_ENTRY_BYTES, EARLY_SYSTEM_HEADER_BYTES, EARLY_SYSTEM_MAX_ENTRIES,
    EARLY_SYSTEM_PAYLOAD_ALIGNMENT, EarlySystemEntry, EarlySystemHeader,
};
use fatfs::Seek as FatSeek;
use fatfs::Write as FatWrite;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::Result;
use crate::config::Config;
use crate::package_manifest::{
    BuilderKind, DesktopEntrySpec, DesktopLaunchMode, InstallLayout, PackageManifest, StartupMode,
    load_manifests,
};
use crate::util::{
    command_in_path, copy_or_unpack_firmware, copy_tree_files, copy_with_parent,
    remove_dir_if_exists, remove_file_if_exists, run_command,
};

mod browser_runtime;
mod library_cache;
mod runtime_libs;

const APPLICATIONS_DIR: &str = "usr/share/applications";
const EARLY_SYSTEM_IMAGE_PATH: &str = "system/boot/early-system.img";
const EARLY_SYSTEM_IMAGE_SIGNATURE_PATH: &str = "system/boot/early-system.img.sig";
const EARLY_SYSTEM_BOOTSTRAP_PATHS: &[&str] = &[
    "etc/ld.so.cache",
    // Boot cache policy and expected payload hashes must be authenticated,
    // not replaced by the storage provider supplying the library bytes.
    "etc/pagerd/library-cache.conf",
    "lib/x86_64-linux-gnu/libc.so.6",
    "lib/x86_64-linux-gnu/libgcc_s.so.1",
    "lib64/ld-linux-x86-64.so.2",
    // The qualification contract remains a private DVM-volume input, but its
    // executable is trusted evidence code. Keep the exact ELF inside the
    // bootloader-authenticated early-system closure so a driver domain cannot
    // substitute a program that merely fabricates valid phase syscalls.
    "apps/smpqual/smpqual.elf",
    "services/devmgrd/devmgrd.elf",
    "services/initd/initd.elf",
    "services/inputd/inputd.elf",
    "services/loaderd/loaderd.elf",
    "services/pagerd/pagerd.elf",
    "services/netd/netd.elf",
    "services/procd/procd.elf",
    "services/rootd/rootd.elf",
    "services/runtimed/runtimed.elf",
    "services/storaged/storaged.elf",
    "services/syscalld/syscalld.elf",
    // The compositor is a core UI bootstrap dependency, not an ordinary
    // desktop application. Keeping its signed executable in the immutable
    // closure removes DVM-volume cold-read latency without giving ring0 any
    // storage-controller or display-provider authority.
    "services/uiserver/uiserver.elf",
    "services/vfsd/vfsd.elf",
    "system/registry/compat/windows-system-dlls.txt",
    "system/registry/system/desktop-programs.tsv",
    "system/registry/system/linux-runtime-access.tsv",
    "system/registry/system/runtime-env.tsv",
    "system/registry/system/runtime-launch-programs.tsv",
    "system/registry/system/startup-programs.tsv",
];
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
    "/usr/share/fonts",
    "/etc/fonts",
    "/etc/ssl",
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
/// Environment for the init scope (services started by initd).
const DEFAULT_INIT_ENV: &[(&str, &str)] = &[
    ("PATH", "/bin:/usr/bin:/usr/local/bin"),
    ("HOME", "/home/user"),
    ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ("WAYLAND_DISPLAY", "wayland-0"),
];
/// Single source of truth for the default environment of every
/// runtimed-launched application. Staged into `runtime-env.tsv`; runtimed's
/// spawn path merges it under per-catalog-entry `env` (entry wins). HOME and
/// the XDG dirs live on the writable `/data` partition; `/tmp` and
/// `/run/user/1000` are tmpfs-class paths owned by vfsd.
const DEFAULT_RUNTIME_ENV: &[(&str, &str)] = &[
    ("PATH", "/bin:/usr/bin:/usr/local/bin"),
    ("HOME", "/data/home"),
    ("XDG_CONFIG_HOME", "/data/config"),
    ("XDG_CACHE_HOME", "/data/cache"),
    ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ("TMPDIR", "/tmp"),
    ("LANG", "C.UTF-8"),
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
    let manifests = load_manifests(&config.root_dir)?;

    remove_dir_if_exists(&config.image_dir)?;
    cleanup_stale_build_paths(config)?;

    stage_image_asset_overlay(&config.image_asset_overlay_dir, &config.image_dir)?;
    let runtime_baseline = runtime_libs::RuntimeBaseline::capture(&config.image_dir)?;
    apply_configured_autostart_policy(config)?;

    for manifest in &manifests {
        stage_manifest(config, manifest)?;
    }
    // The kernel bootstrap allowlist spawns rootd and its five boot services
    // without loaderd, so their stamps are proven here instead of at spawn.
    crate::native_abi::verify_staged_native_elfs(config, &manifests)?;
    let prebuilt_elfs = runtime_libs::stage_prebuilt_app_runtime_libraries(config, &manifests)?;
    stage_boot_manager(config)?;
    stage_nucleus_signature(config)?;

    let amdgpu_image_firmware_dir = config.amdgpu_image_firmware_dir();
    fs::create_dir_all(&amdgpu_image_firmware_dir)?;
    for basename in &config.amdgpu_required_firmware_basenames {
        let dst = amdgpu_image_firmware_dir.join(basename);
        copy_or_unpack_firmware(&config.amdgpu_firmware_dir, basename, &dst)?;
    }

    write_application_desktop_files(config, &manifests)?;
    write_desktop_registry(config, &manifests)?;
    write_runtime_launch_registry(config, &manifests)?;
    write_startup_registry(config, &manifests)?;
    write_windows_dll_registry(config, &manifests)?;
    write_linux_runtime_access_registry(config)?;
    write_runtime_env_registry(config)?;
    for note in browser_runtime::stage_browser_runtime_files(config)? {
        eprintln!("stage: {note}");
    }
    generate_dynamic_linker_cache(&config.image_dir)?;
    materialize_image_file_aliases(&config.image_dir)?;
    runtime_baseline.verify(&config.image_dir)?;
    let library_uses = runtime_libs::verify_relocations(&config.image_dir, &prebuilt_elfs)?;
    library_cache::stage_plan(&config.image_dir, &library_uses)?;
    write_early_system_image(config)?;
    write_boot_disk_image(config)?;
    Ok(())
}

fn write_early_system_image(config: &Config) -> Result<()> {
    let signing_key =
        crate::storage_epoch::load_or_create_signing_key(&config.storage_epoch_signing_key)?;
    let image = build_early_system_image(
        &config.image_dir,
        EARLY_SYSTEM_BOOTSTRAP_PATHS,
        signing_key.verifying_key().to_bytes(),
    )?;
    let work_dir = config.build_dir.join("early-system");
    fs::create_dir_all(&work_dir)?;
    let image_path = work_dir.join("early-system.img");
    let signature_path = work_dir.join("early-system.img.sig");
    fs::write(&image_path, image)?;
    sign_detached_for_grub(config, &image_path, &signature_path)?;

    let staged_image = config.image_dir.join(EARLY_SYSTEM_IMAGE_PATH);
    let staged_signature = config.image_dir.join(EARLY_SYSTEM_IMAGE_SIGNATURE_PATH);
    if let Some(parent) = staged_image.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&image_path, staged_image)?;
    fs::copy(&signature_path, staged_signature)?;
    Ok(())
}

fn build_early_system_image(
    image_dir: &Path,
    paths: &[&str],
    storage_epoch_verifying_key: [u8; 32],
) -> Result<Vec<u8>> {
    if paths.is_empty() || paths.len() > EARLY_SYSTEM_MAX_ENTRIES as usize {
        bail!("early-system allowlist count is outside the fixed ABI bound");
    }
    let mut paths = paths.to_vec();
    paths.sort_unstable();
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("early-system allowlist contains a duplicate path");
    }

    let table_end = EARLY_SYSTEM_HEADER_BYTES
        .checked_add(
            paths
                .len()
                .checked_mul(EARLY_SYSTEM_ENTRY_BYTES)
                .context("early-system table size overflow")?,
        )
        .context("early-system table size overflow")?;
    let payload_offset = align_early_system_offset(table_end as u64)?;
    let mut payload_cursor = payload_offset;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let source = image_dir.join(path);
        let bytes = fs::read(&source)
            .with_context(|| format!("missing early-system bootstrap file {}", source.display()))?;
        if bytes.is_empty() {
            bail!("early-system bootstrap file is empty: {path}");
        }
        payload_cursor = align_early_system_offset(payload_cursor)?;
        let len = u64::try_from(bytes.len()).context("early-system file is too large")?;
        let end = payload_cursor
            .checked_add(len)
            .context("early-system payload size overflow")?;
        let sha256: [u8; 32] = Sha256::digest(&bytes).into();
        let entry = EarlySystemEntry::new(path.as_bytes(), payload_cursor, len, sha256)
            .with_context(|| format!("invalid early-system path or range: {path}"))?;
        files.push((entry, bytes));
        payload_cursor = end;
    }

    let header = EarlySystemHeader::new(
        u32::try_from(files.len()).context("early-system entry count overflow")?,
        payload_offset,
        payload_cursor,
        storage_epoch_verifying_key,
    )
    .context("early-system header violates the fixed ABI")?;
    let image_len =
        usize::try_from(header.total_bytes).context("early-system image does not fit usize")?;
    let mut image = vec![0_u8; image_len];
    image[..EARLY_SYSTEM_HEADER_BYTES].copy_from_slice(
        &header
            .encode()
            .context("early-system header encoding failed")?,
    );
    for (index, (entry, bytes)) in files.into_iter().enumerate() {
        let record_start = EARLY_SYSTEM_HEADER_BYTES + index * EARLY_SYSTEM_ENTRY_BYTES;
        let record_end = record_start + EARLY_SYSTEM_ENTRY_BYTES;
        image[record_start..record_end].copy_from_slice(
            &entry
                .encode(header)
                .context("early-system entry encoding failed")?,
        );
        let payload_start = usize::try_from(entry.payload_offset)
            .context("early-system payload offset overflow")?;
        let payload_end = payload_start
            .checked_add(bytes.len())
            .context("early-system payload end overflow")?;
        image[payload_start..payload_end].copy_from_slice(&bytes);
    }
    Ok(image)
}

fn align_early_system_offset(value: u64) -> Result<u64> {
    value
        .checked_add(EARLY_SYSTEM_PAYLOAD_ALIGNMENT - 1)
        .map(|value| value / EARLY_SYSTEM_PAYLOAD_ALIGNMENT * EARLY_SYSTEM_PAYLOAD_ALIGNMENT)
        .context("early-system alignment overflow")
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
    {
        let root = fs.root_dir();
        for file in &files {
            ensure_fat_parent_dirs(&root, &file.relative)?;
            let mut dst = root.create_file(file.relative.as_str())?;
            dst.truncate()?;
            copy_host_file_to_fat(&file.source, &mut dst)?;
            dst.flush()?;
        }
    }
    fs.unmount()?;
    verify_boot_disk_image_contract(&config.boot_disk_image, &files)?;
    Ok(())
}

fn verify_boot_disk_image_contract(boot_disk_image: &Path, files: &[ImageFile]) -> Result<()> {
    let image =
        fatfs::StdIoWrapper::new(fs::File::open(boot_disk_image).with_context(|| {
            format!("failed to reopen boot disk {}", boot_disk_image.display())
        })?);
    let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new())?;
    let root = fs.root_dir();
    for file in files {
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
        let mut staged = root
            .open_file(file.relative.as_str())
            .with_context(|| format!("missing staged boot-disk file {}", file.relative))?;
        let mut actual = Vec::new();
        staged.read_to_end(&mut actual)?;
        if actual != expected {
            bail!("boot-disk payload mismatch for {}", file.relative);
        }
    }
    drop(root);
    fs.unmount()?;
    Ok(())
}

fn gpt_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn write_gpt_entry(
    buf: &mut [u8],
    type_guid: [u8; 16],
    unique_guid: [u8; 16],
    start_lba: u64,
    end_lba: u64,
    name: &str,
) {
    buf[0..16].copy_from_slice(&type_guid);
    buf[16..32].copy_from_slice(&unique_guid);
    buf[32..40].copy_from_slice(&start_lba.to_le_bytes());
    buf[40..48].copy_from_slice(&end_lba.to_le_bytes());
    buf[48..56].copy_from_slice(&0_u64.to_le_bytes());
    for (idx, code_unit) in name.encode_utf16().take(36).enumerate() {
        let off = 56 + idx * 2;
        buf[off..off + 2].copy_from_slice(&code_unit.to_le_bytes());
    }
}

#[allow(clippy::too_many_arguments)]
fn build_gpt_header(
    my_lba: u64,
    alternate_lba: u64,
    first_usable_lba: u64,
    last_usable_lba: u64,
    disk_guid: [u8; 16],
    entry_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entry_array_crc32: u32,
) -> [u8; 512] {
    let mut header = [0_u8; 512];
    header[0..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    header[12..16].copy_from_slice(&92_u32.to_le_bytes());
    header[20..24].copy_from_slice(&0_u32.to_le_bytes());
    header[24..32].copy_from_slice(&my_lba.to_le_bytes());
    header[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
    header[40..48].copy_from_slice(&first_usable_lba.to_le_bytes());
    header[48..56].copy_from_slice(&last_usable_lba.to_le_bytes());
    header[56..72].copy_from_slice(&disk_guid);
    header[72..80].copy_from_slice(&entry_lba.to_le_bytes());
    header[80..84].copy_from_slice(&entry_count.to_le_bytes());
    header[84..88].copy_from_slice(&entry_size.to_le_bytes());
    header[88..92].copy_from_slice(&entry_array_crc32.to_le_bytes());
    let header_crc = gpt_crc32(&header[..92]);
    header[16..20].copy_from_slice(&header_crc.to_le_bytes());
    header
}

struct PartitionFileSlice<'a> {
    file: &'a mut std::fs::File,
    base_offset: u64,
    len: u64,
    pos: u64,
}

impl<'a> PartitionFileSlice<'a> {
    fn new(file: &'a mut std::fs::File, base_offset: u64, len: u64) -> Self {
        Self {
            file,
            base_offset,
            len,
            pos: 0,
        }
    }
}

impl std::io::Read for PartitionFileSlice<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Seek;
        if self.pos >= self.len {
            return Ok(0);
        }
        let max_read = usize::try_from(self.len - self.pos).unwrap_or(usize::MAX);
        let to_read = buf.len().min(max_read);
        self.file.seek(std::io::SeekFrom::Start(self.base_offset + self.pos))?;
        let n = self.file.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Write for PartitionFileSlice<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use std::io::Seek;
        if self.pos >= self.len {
            return Ok(0);
        }
        let max_write = usize::try_from(self.len - self.pos).unwrap_or(usize::MAX);
        let to_write = buf.len().min(max_write);
        self.file.seek(std::io::SeekFrom::Start(self.base_offset + self.pos))?;
        let n = self.file.write(&buf[..to_write])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl std::io::Seek for PartitionFileSlice<'_> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let next = match pos {
            std::io::SeekFrom::Start(offset) => i128::from(offset),
            std::io::SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
            std::io::SeekFrom::Current(offset) => i128::from(self.pos) + i128::from(offset),
        };
        if next < 0 || next > i128::from(self.len) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek out of partition bounds",
            ));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

/// Read partition 2 of a previously written boot+data GPT image. Refuses any
/// image that is not exactly the layout this writer produces, so a wrong file
/// is never reinterpreted as user data.
fn read_previous_data_partition(previous: &Path, data_size_bytes: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek};
    const SECTOR: u64 = 512;
    let mut file = fs::File::open(previous)
        .with_context(|| format!("open existing data disk {}", previous.display()))?;
    let mut header = [0_u8; 512];
    file.seek(std::io::SeekFrom::Start(SECTOR))?;
    file.read_exact(&mut header)
        .context("existing data disk is too small to hold a GPT header")?;
    if &header[0..8] != b"EFI PART" {
        bail!("existing data disk {} has no GPT header", previous.display());
    }
    let header_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if !(92..=512).contains(&header_len) {
        bail!("existing data disk has an invalid GPT header size");
    }
    let stored_crc = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let mut crc_input = header[..header_len].to_vec();
    crc_input[16..20].fill(0);
    if gpt_crc32(&crc_input) != stored_crc {
        bail!("existing data disk has a corrupt GPT header CRC");
    }
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let entry_count = u32::from_le_bytes(header[80..84].try_into().unwrap());
    let entry_bytes = u32::from_le_bytes(header[84..88].try_into().unwrap());
    let entries_crc = u32::from_le_bytes(header[88..92].try_into().unwrap());
    let array_len = u64::from(entry_count) * u64::from(entry_bytes);
    if entry_bytes < 128 || entry_count < 2 || array_len > 1 << 20 {
        bail!("existing data disk has an unsupported GPT entry array");
    }
    let mut entries = vec![0_u8; array_len as usize];
    file.seek(std::io::SeekFrom::Start(entries_lba * SECTOR))?;
    file.read_exact(&mut entries)?;
    if gpt_crc32(&entries) != entries_crc {
        bail!("existing data disk has a corrupt GPT entry array CRC");
    }
    let entry = &entries[entry_bytes as usize..entry_bytes as usize + 128];
    let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
    let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
    let name: Vec<u16> = entry[56..128]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    if String::from_utf16_lossy(&name) != "RUSTOSDATA" {
        bail!("existing data disk partition 2 is not RUSTOSDATA");
    }
    let bytes = last
        .checked_sub(first)
        .and_then(|blocks| blocks.checked_add(1))
        .and_then(|blocks| blocks.checked_mul(SECTOR))
        .context("existing data partition bounds overflow")?;
    if bytes != data_size_bytes {
        bail!(
            "existing data partition is {bytes} bytes, expected {data_size_bytes}; refusing to resize user data"
        );
    }
    let mut data = vec![0_u8; bytes as usize];
    file.seek(std::io::SeekFrom::Start(first * SECTOR))?;
    file.read_exact(&mut data)
        .context("existing data disk is shorter than its partition table")?;
    if data[510..512] != [0x55, 0xAA] {
        bail!("existing data partition has no FAT boot signature");
    }
    Ok(data)
}

/// Like [`write_gpt_boot_plus_writable_data_image`], but a `previous` image's
/// data partition is carried over byte-for-byte instead of being formatted.
/// The boot partition is always rewritten from `boot_image`. The output is
/// built at a sibling temp path and renamed into place, so a failure at any
/// point leaves an existing `output_gpt_image` (and `previous`) untouched.
pub(crate) fn write_gpt_boot_plus_data_image(
    boot_image: &Path,
    output_gpt_image: &Path,
    data_size_bytes: u64,
    previous: Option<&Path>,
) -> Result<()> {
    let previous_data = previous
        .map(|path| read_previous_data_partition(path, data_size_bytes))
        .transpose()?;
    let parent = output_gpt_image.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    write_gpt_image_impl(boot_image, temp.as_file_mut(), data_size_bytes, previous_data.as_deref())?;
    temp.persist(output_gpt_image)
        .with_context(|| format!("publish GPT image {}", output_gpt_image.display()))?;
    Ok(())
}

fn write_gpt_image_impl(
    boot_image: &Path,
    mut out: &mut std::fs::File,
    data_size_bytes: u64,
    previous_data: Option<&[u8]>,
) -> Result<()> {
    use std::io::{Seek, Write};

    const SECTOR_BYTES: u64 = 512;
    const GPT_ENTRY_COUNT: u32 = 128;
    const GPT_ENTRY_BYTES: u32 = 128;
    const GPT_ARRAY_SECTORS: u64 = 32;
    const FIRST_USABLE_LBA: u64 = 2 + GPT_ARRAY_SECTORS;
    const EFI_SYSTEM_PART_GUID: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ];
    const BASIC_DATA_PART_GUID: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    let boot_bytes = fs::metadata(boot_image)
        .with_context(|| format!("inspect boot image {}", boot_image.display()))?
        .len();
    if boot_bytes == 0 || !boot_bytes.is_multiple_of(SECTOR_BYTES) {
        bail!("boot image must be non-empty and 512-byte aligned");
    }
    if data_size_bytes == 0 || !data_size_bytes.is_multiple_of(SECTOR_BYTES) {
        bail!("writable data partition size must be non-empty and 512-byte aligned");
    }

    let boot_blocks = boot_bytes / SECTOR_BYTES;
    let data_blocks = data_size_bytes / SECTOR_BYTES;
    let part1_start = FIRST_USABLE_LBA;
    let part1_end = part1_start
        .checked_add(boot_blocks)
        .and_then(|end| end.checked_sub(1))
        .context("boot partition LBA overflow")?;
    let part2_start = part1_end
        .checked_add(1)
        .context("data partition start LBA overflow")?;
    let part2_end = part2_start
        .checked_add(data_blocks)
        .and_then(|end| end.checked_sub(1))
        .context("data partition end LBA overflow")?;
    let last_usable_lba = part2_end;
    let backup_array_lba = last_usable_lba + 1;
    let backup_header_lba = backup_array_lba + GPT_ARRAY_SECTORS;
    let total_blocks = backup_header_lba + 1;
    let total_bytes = total_blocks
        .checked_mul(SECTOR_BYTES)
        .context("GPT disk byte length overflow")?;

    out.set_len(total_bytes)?;

    out.seek(std::io::SeekFrom::Start(part1_start * SECTOR_BYTES))?;
    let mut boot_in = fs::File::open(boot_image)?;
    let copied = std::io::copy(&mut boot_in, &mut out)?;
    if copied != boot_bytes {
        bail!("short copy from boot image {}", boot_image.display());
    }

    if let Some(data) = previous_data {
        out.seek(std::io::SeekFrom::Start(part2_start * SECTOR_BYTES))?;
        out.write_all(data)?;
    } else {
        let slice = PartitionFileSlice::new(&mut out, part2_start * SECTOR_BYTES, data_size_bytes);
        let mut fat_io = fatfs::StdIoWrapper::new(slice);
        fatfs::format_volume(
            &mut fat_io,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .bytes_per_sector(512)
                .volume_label(*b"RUSTOSDATA ")
                .volume_id(0x5255_5344),
        )
        .context("format writable FAT32 data partition")?;
    }

    let mut entries = vec![0_u8; (GPT_ENTRY_COUNT * GPT_ENTRY_BYTES) as usize];
    write_gpt_entry(
        &mut entries[0..128],
        EFI_SYSTEM_PART_GUID,
        [0x11; 16],
        part1_start,
        part1_end,
        "RUSTOSBOOT",
    );
    write_gpt_entry(
        &mut entries[128..256],
        BASIC_DATA_PART_GUID,
        [0x22; 16],
        part2_start,
        part2_end,
        "RUSTOSDATA",
    );
    let entry_array_crc = gpt_crc32(&entries);

    let mut mbr = [0_u8; 512];
    mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    mbr[446 + 4] = 0xEE;
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[446 + 8..446 + 12].copy_from_slice(&1_u32.to_le_bytes());
    let mbr_sectors = u32::try_from(total_blocks - 1).unwrap_or(u32::MAX);
    mbr[446 + 12..446 + 16].copy_from_slice(&mbr_sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let disk_guid = [0x52; 16];
    let primary_header = build_gpt_header(
        1,
        backup_header_lba,
        FIRST_USABLE_LBA,
        last_usable_lba,
        disk_guid,
        2,
        GPT_ENTRY_COUNT,
        GPT_ENTRY_BYTES,
        entry_array_crc,
    );
    let backup_header = build_gpt_header(
        backup_header_lba,
        1,
        FIRST_USABLE_LBA,
        last_usable_lba,
        disk_guid,
        backup_array_lba,
        GPT_ENTRY_COUNT,
        GPT_ENTRY_BYTES,
        entry_array_crc,
    );

    out.seek(std::io::SeekFrom::Start(0))?;
    out.write_all(&mbr)?;
    out.write_all(&primary_header)?;
    out.write_all(&entries)?;

    out.seek(std::io::SeekFrom::Start(backup_array_lba * SECTOR_BYTES))?;
    out.write_all(&entries)?;
    out.write_all(&backup_header)?;
    out.sync_all()?;
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
    let mut files = Vec::new();
    for entry in WalkDir::new(image_dir) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            bail!("unmaterialized image alias would be lost on FAT: {}", entry.path().display());
        }
        if !entry.file_type().is_file() { continue; }
        let path = entry.into_path();
        let relative = path.strip_prefix(image_dir)?.components()
            .map(|component| component.as_os_str().to_str().context("non-UTF8 image file name"))
            .collect::<Result<Vec<_>>>()?.join("/");
        let len = fs::metadata(&path)
            .map_err(|err| anyhow!("failed to stat image file {}: {err}", path.display()))?.len();
        files.push(ImageFile { source: path, relative, len });
    }
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

fn write_application_desktop_files(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let applications_dir = config.image_dir.join(APPLICATIONS_DIR);
    fs::create_dir_all(&applications_dir)?;

    for manifest in manifests {
        if !manifest.artifact_path(config).exists()
            || !runtime_launch_enabled_for_manifest(config, manifest)
        {
            continue;
        }

        for (index, entry) in manifest.desktop.entries.iter().enumerate() {
            let desktop_file_id = desktop_file_id(manifest, index);
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
                argv = desktop_value(&join_registry_list(&entry.args)?)?,
                env = desktop_value(&join_registry_list(&entry.env)?)?,
            );
            fs::write(applications_dir.join(&desktop_file_id), content)?;
        }
    }

    Ok(())
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
                    .map(|arg| registry_list_item(arg))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let env = if entry.env.is_empty() {
                String::new()
            } else {
                entry
                    .env
                    .iter()
                    .map(|item| registry_list_item(item))
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
                    .map(|arg| registry_list_item(arg))
                    .collect::<Result<Vec<_>>>()?
                    .join("|")
            };
            let env = if entry.env.is_empty() {
                String::new()
            } else {
                entry
                    .env
                    .iter()
                    .map(|item| registry_list_item(item))
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
            if !name.to_ascii_lowercase().ends_with(".dll") {
                continue;
            }
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

/// An element of a `|`-joined registry list. A literal `|` would split it into
/// two elements when the registry is parsed.
fn registry_list_item(value: &str) -> Result<String> {
    if value.contains('|') {
        bail!("registry list value contains unsupported separator: {value:?}");
    }
    registry_value(value)
}

fn join_registry_list(items: &[String]) -> Result<String> {
    Ok(items
        .iter()
        .map(|item| registry_list_item(item))
        .collect::<Result<Vec<_>>>()?
        .join("|"))
}

fn registry_deps(deps: &[String]) -> Result<String> {
    deps.iter()
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

fn cleanup_stale_build_paths(config: &Config) -> Result<()> {
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

include!("finalization.rs");

#[cfg(test)]
mod tests {
    use super::{
        EARLY_SYSTEM_BOOTSTRAP_PATHS, ImageFile, build_early_system_image, ensure_fat_parent_dirs,
        registry_deps, verify_boot_disk_image_contract, collect_image_files,
        materialize_image_file_aliases, write_boot_disk_image, copy_tree_files, Config,
    };
    use boot_protocol::{
        EARLY_SYSTEM_ENTRY_BYTES, EARLY_SYSTEM_HEADER_BYTES, EarlySystemEntry, EarlySystemHeader,
    };
    use fatfs::{Seek as _, Write as _};
    use std::fs::{self, OpenOptions};
    use std::path::Path;

    #[test]
    fn imported_chrome_is_visible_without_any_boot_launch_entry() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::from_env().unwrap();
        config.image_dir = temp.path().join("image");
        config.artifact_dir = temp.path().join("artifacts");
        let manifest: crate::package_manifest::PackageManifest = toml::from_str(
            include_str!("../../../../apps/chrome/RUSTOS.package.toml.example"),
        ).unwrap();
        let artifact = manifest.artifact_path(&config);
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(artifact.join("chrome"), b"browser fixture").unwrap();
        super::stage_manifest(&config, &manifest).unwrap();
        let manifests = [manifest];
        super::write_application_desktop_files(&config, &manifests).unwrap();
        super::write_desktop_registry(&config, &manifests).unwrap();
        super::write_runtime_launch_registry(&config, &manifests).unwrap();
        super::write_startup_registry(&config, &manifests).unwrap();
        let desktop = std::fs::read_to_string(config.image_dir.join(super::DESKTOP_REGISTRY_PATH)).unwrap();
        for field in ["display_name=Google Chrome", "startup=none", "no_display=0", "autostart_enabled=0", "image=apps/chrome/chrome"] {
            assert!(desktop.contains(field), "{desktop}");
        }
        let entry = std::fs::read_to_string(config.image_dir.join(super::APPLICATIONS_DIR).join("chrome.desktop")).unwrap();
        assert!(entry.contains("X-RustOS-Startup=none"));
        assert!(entry.contains("NoDisplay=false"));
        for registry in [super::STARTUP_REGISTRY_PATH, super::RUNTIME_LAUNCH_REGISTRY_PATH] {
            assert!(std::fs::read_to_string(config.image_dir.join(registry)).unwrap().trim().is_empty());
        }
        assert!(!config.image_dir.join("etc/xdg/autostart/chrome.desktop").exists());
        let mut disabled_fuzzer = manifests[0].clone();
        disabled_fuzzer.id = super::ABI_FUZZ_PACKAGE_ID.into();
        config.project.fuzzing.enabled = false;
        super::write_application_desktop_files(&config, &[disabled_fuzzer]).unwrap();
        assert!(!config.image_dir.join(super::APPLICATIONS_DIR).join("abifuzz.desktop").exists());
        assert_eq!(std::fs::read(config.image_dir.join("apps/chrome/chrome")).unwrap(), b"browser fixture");
    }

    #[test]
    fn registry_deps_join_package_ids_with_commas() {
        let deps = vec!["runtimed".to_string(), "sessiond".to_string()];

        assert_eq!(registry_deps(&deps).unwrap(), "runtimed,sessiond");
    }

    #[test]
    fn early_system_image_is_deterministic_sorted_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("services/a")).unwrap();
        std::fs::create_dir_all(root.join("services/b")).unwrap();
        std::fs::write(root.join("services/a/a.elf"), b"alpha").unwrap();
        std::fs::write(root.join("services/b/b.elf"), b"beta").unwrap();

        let image =
            build_early_system_image(root, &["services/b/b.elf", "services/a/a.elf"], [0x5a; 32])
                .unwrap();
        let header = EarlySystemHeader::decode(&image).unwrap();
        assert_eq!(header.entry_count, 2);
        assert_eq!(header.total_bytes as usize, image.len());
        let first = EarlySystemEntry::decode(
            &image[EARLY_SYSTEM_HEADER_BYTES..EARLY_SYSTEM_HEADER_BYTES + EARLY_SYSTEM_ENTRY_BYTES],
            header,
        )
        .unwrap();
        assert_eq!(first.path_bytes(), Some(b"services/a/a.elf".as_slice()));
        let start = first.payload_offset as usize;
        let end = start + first.payload_len as usize;
        assert_eq!(&image[start..end], b"alpha");
    }

    #[test]
    fn early_system_allowlist_contains_the_minimal_dynamic_runtime_closure() {
        for required in [
            "apps/smpqual/smpqual.elf",
            "etc/ld.so.cache",
            "lib64/ld-linux-x86-64.so.2",
            "lib/x86_64-linux-gnu/libc.so.6",
            "lib/x86_64-linux-gnu/libgcc_s.so.1",
            "services/uiserver/uiserver.elf",
        ] {
            assert!(
                EARLY_SYSTEM_BOOTSTRAP_PATHS.contains(&required),
                "missing early-system dynamic runtime dependency {required}"
            );
        }
    }

    #[test]
    fn registry_list_items_reject_the_list_separator() {
        assert_eq!(
            super::join_registry_list(&["a".into(), "b=c".into()]).unwrap(),
            "a|b=c"
        );
        assert!(super::join_registry_list(&["a|b".into()]).is_err());
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
    fn registry_deps_rejects_embedded_commas() {
        let deps = vec!["runtimed,sessiond".to_string()];

        assert!(registry_deps(&deps).is_err());
    }

    #[test]
    fn image_aliases_are_materialized_and_survive_fat_serialization() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let image = temp.path().join("image");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("libfoo.so.1.0"), b"library payload").unwrap();
        symlink("libfoo.so.1.0", source.join("libfoo.so.1")).unwrap();
        copy_tree_files(&source, &image).unwrap();
        assert!(fs::symlink_metadata(source.join("libfoo.so.1")).unwrap().is_symlink());
        assert!(!fs::symlink_metadata(image.join("libfoo.so.1")).unwrap().is_symlink());
        // Model the additional alias introduced after copying by ldconfig.
        symlink("libfoo.so.1.0", image.join("libfoo.so")).unwrap();
        assert!(collect_image_files(&image).is_err(), "never silently omit a verified alias");
        materialize_image_file_aliases(&image).unwrap();
        let files = collect_image_files(&image).unwrap();
        assert_eq!(files.iter().map(|file| file.relative.as_str()).collect::<Vec<_>>(),
            ["libfoo.so", "libfoo.so.1", "libfoo.so.1.0"]);
        let mut config = Config::from_env().unwrap();
        config.image_dir = image;
        config.boot_disk_image = temp.path().join("alias-fat.img");
        // This reopens the actual FAT volume and compares every alias payload.
        write_boot_disk_image(&config).unwrap();
    }

    #[test]
    fn source_and_final_image_reject_escaping_dangling_and_directory_aliases() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("directory")).unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"not an image file").unwrap();
        let alias = source.join("alias");
        for target in [outside.as_path(), Path::new("missing"), Path::new("directory")] {
            symlink(target, &alias).unwrap();
            assert!(copy_tree_files(&source, &temp.path().join("dest")).is_err());
            assert!(materialize_image_file_aliases(&source).is_err());
            fs::remove_file(&alias).unwrap();
        }
    }

    #[test]
    fn boot_disk_contract_reopens_every_staged_file() {
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
        {
            let root = fs.root_dir();
            let relative = "services/rootd/rootd.elf";
            ensure_fat_parent_dirs(&root, relative).unwrap();
            let mut staged = root.create_file(relative).unwrap();
            staged.write_all(payload).unwrap();
            staged.flush().unwrap();
        }
        fs.unmount().unwrap();

        verify_boot_disk_image_contract(
            &boot_disk,
            &[ImageFile {
                source,
                relative: "services/rootd/rootd.elf".to_string(),
                len: payload.len() as u64,
            }],
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn data_disk_carry_over_preserves_partial_sibling_and_symlink_target() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let boot = temp.path().join("boot.img");
        fs::write(&boot, vec![0u8; 512]).unwrap();
        let output = temp.path().join("data.img");
        let sibling = temp.path().join("data.img.partial");
        let target = temp.path().join("target");
        fs::write(&target, b"target").unwrap();
        fs::write(&sibling, b"sibling").unwrap();
        super::write_gpt_boot_plus_data_image(&boot, &output, 64 * 1024 * 1024, None).unwrap();
        assert_eq!(fs::read(&sibling).unwrap(), b"sibling");
        fs::remove_file(&sibling).unwrap();
        symlink(&target, &sibling).unwrap();
        super::write_gpt_boot_plus_data_image(&boot, &output, 64 * 1024 * 1024, Some(&output)).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"target");
        assert!(fs::symlink_metadata(&sibling).unwrap().file_type().is_symlink());
    }

    #[test]
    fn gpt_boot_plus_writable_data_image_exposes_boot_and_rw_fat32_partitions() {
        use storage_core::FsVolume;

        let temp = tempfile::tempdir().unwrap();
        let boot_disk = temp.path().join("rustos-boot.img");
        let payload = b"rootd bootstrap payload";
        {
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
            {
                let root = fs.root_dir();
                ensure_fat_parent_dirs(&root, "services/rootd/rootd.elf").unwrap();
                let mut staged = root.create_file("services/rootd/rootd.elf").unwrap();
                staged.write_all(payload).unwrap();
                staged.flush().unwrap();
            }
            fs.unmount().unwrap();
        }

        let gpt_disk = temp.path().join("rustos-gpt.img");
        super::write_gpt_boot_plus_data_image(&boot_disk, &gpt_disk, 36 * 1024 * 1024, None)
            .unwrap();

        let disk_bytes = std::fs::read(&gpt_disk).unwrap();
        let mut dev = storage_core::MemBlockDevice::from_bytes(512, disk_bytes.clone()).unwrap();
        let partitions = storage_core::detect_partitions(&mut dev).unwrap();
        assert_eq!(partitions.len(), 2);
        assert!(partitions[0].start_lba + partitions[0].block_count <= partitions[1].start_lba);

        {
            let boot_vol = storage_fat::FatVolume::from_partition(
                dev,
                partitions[0].start_lba,
                partitions[0].block_count,
            )
            .unwrap();
            let actual = boot_vol.read_file_to_vec("services/rootd/rootd.elf").unwrap();
            assert_eq!(actual, payload);
            boot_vol.unmount().unwrap();
        }

        {
            let rw_dev = storage_core::MemBlockDevice::from_bytes(512, disk_bytes).unwrap();
            let mut data_vol = storage_fat::FatVolume::from_partition(
                rw_dev,
                partitions[1].start_lba,
                partitions[1].block_count,
            )
            .unwrap();
            assert!(data_vol.read_dir("/").unwrap().is_empty());
            {
                let mut file = data_vol.create_file("/hello.txt").unwrap();
                file.flush().unwrap();
            }
            let (written, new_len) =
                FsVolume::write_at(&data_vol, "/hello.txt", 0, b"rustos rw").unwrap();
            assert_eq!(written, 9);
            assert_eq!(new_len, 9);
            FsVolume::flush(&data_vol).unwrap();
            assert_eq!(
                data_vol.read_file_to_vec("/hello.txt").unwrap(),
                b"rustos rw"
            );
            data_vol.unmount().unwrap();
        }
    }

    #[test]
    fn data_disk_carry_over_keeps_partition_two_and_refreshes_boot() {
        let temp = tempfile::tempdir().unwrap();
        let boot_a = temp.path().join("boot-a.img");
        let boot_b = temp.path().join("boot-b.img");
        std::fs::write(&boot_a, vec![0xA1_u8; 2 * 1024 * 1024]).unwrap();
        std::fs::write(&boot_b, vec![0xB2_u8; 4 * 1024 * 1024]).unwrap();
        let disk = temp.path().join("data.img");
        let data_bytes = 36 * 1024 * 1024;
        super::write_gpt_boot_plus_data_image(&boot_a, &disk, data_bytes, None).unwrap();
        // Partition 2 start LBA lives in GPT entry 1 (second 128-byte entry).
        let part2 = |bytes: &[u8]| {
            let at = 2 * 512 + 128 + 32;
            u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) * 512
        };
        let mut image = std::fs::read(&disk).unwrap();
        let start = part2(&image) as usize;
        image[start + 8192..start + 8192 + 9].copy_from_slice(b"user-data");
        std::fs::write(&disk, &image).unwrap();

        // A larger boot image moves partition 2; the data must follow it.
        super::write_gpt_boot_plus_data_image(&boot_b, &disk, data_bytes, Some(&disk)).unwrap();
        let after = std::fs::read(&disk).unwrap();
        let new_start = part2(&after) as usize;
        assert!(new_start > start);
        assert_eq!(&after[new_start + 8192..new_start + 8192 + 9], b"user-data");
        assert_eq!(after[34 * 512], 0xB2, "boot partition is refreshed");
        assert!(!temp.path().join("data.img.partial").exists());

        // Wrong-sized or foreign files are refused and left untouched.
        assert!(super::write_gpt_boot_plus_data_image(&boot_b, &disk, data_bytes + 512, Some(&disk)).is_err());
        assert_eq!(std::fs::read(&disk).unwrap(), after);
        let foreign = temp.path().join("foreign.img");
        std::fs::write(&foreign, vec![7_u8; 64 * 1024]).unwrap();
        assert!(super::write_gpt_boot_plus_data_image(&boot_b, &foreign, data_bytes, Some(&foreign)).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), vec![7_u8; 64 * 1024]);
    }
}
