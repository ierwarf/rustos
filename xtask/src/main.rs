use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(err) = run() {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("stage") => {
            if let Some(arg) = args.next() {
                return Err(format!("unexpected argument for stage: {arg}").into());
            }
            stage()
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("unknown xtask subcommand: {other}").into()),
    }
}

fn print_help() {
    println!(
        "\
usage: cargo xtask <command>

commands:
  stage     populate build/image from build/artifacts and host runtime assets
"
    );
}

fn stage() -> Result<()> {
    let config = StageConfig::from_env()?;

    remove_dir_if_exists(&config.image_dir)?;
    for path in [
        &config.build_dir.join("EFI"),
        &config.build_dir.join("etc"),
        &config.build_dir.join("lib"),
        &config.build_dir.join("lib64"),
        &config.build_dir.join("linux"),
    ] {
        remove_dir_if_exists(path)?;
    }

    for path in [
        &config.boot_file_list,
        &config.startup_nsh,
        &config.build_dir.join("kernel.elf"),
        &config.build_dir.join("prekernel.elf"),
        &config.build_dir.join("UISERVER.ELF"),
        &config.build_dir.join("UISERVER.EXE"),
        &config.build_dir.join("PRINTFDEMO.ELF"),
    ] {
        remove_file_if_exists(path)?;
    }

    fs::create_dir_all(&config.efi_boot_dir)?;
    copy_with_parent(&config.artifact_boot_efi, &config.boot_efi)?;
    copy_with_parent(&config.artifact_prekernel_elf, &config.prekernel_elf)?;
    copy_with_parent(&config.artifact_kernel_elf, &config.kernel_elf)?;
    copy_with_parent(&config.artifact_user_elf, &config.image_user_elf)?;
    copy_with_parent(&config.artifact_win_user_exe, &config.image_win_user_exe)?;
    copy_with_parent(&config.artifact_printf_demo_elf, &config.image_printf_demo_elf)?;
    copy_with_parent(&config.artifact_amdgpu_ko, &config.image_amdgpu_ko)?;
    copy_with_parent(&config.artifact_psmouse_ko, &config.image_psmouse_ko)?;
    fs::create_dir_all(&config.amdgpu_image_firmware_dir)?;

    let mut boot_entries = Vec::new();

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
            fs::create_dir_all(
                config
                    .glibc_ldso_cache_dest
                    .parent()
                    .ok_or("ld.so.cache destination has no parent")?,
            )?;
            run_command(
                Command::new(ldconfig)
                    .arg("-r")
                    .arg(&config.image_dir)
                    .arg("-C")
                    .arg("/etc/ld.so.cache"),
            )?;
            boot_entries.push(String::from("etc/ld.so.cache"));
        }
    }

    boot_entries.push(String::from("system/apps/uiserver/uiserver.elf"));
    boot_entries.push(String::from("system/apps/uiserver/uiserver.exe"));
    boot_entries.push(String::from("system/apps/printfdemo/printfdemo.elf"));
    boot_entries.push(String::from("system/drivers/display/amdgpu.ko"));

    for basename in &config.amdgpu_required_firmware_basenames {
        let dst = config.amdgpu_image_firmware_dir.join(basename);
        copy_or_unpack_firmware(&config.amdgpu_firmware_dir, basename, &dst)?;
        boot_entries.push(format!("system/firmware/amdgpu/{basename}"));
    }

    boot_entries.push(String::from("system/drivers/input/psmouse.ko"));

    fs::create_dir_all(
        config
            .glibc_ldso_preload_dest
            .parent()
            .ok_or("ld.so.preload destination has no parent")?,
    )?;
    fs::write(&config.glibc_ldso_preload_dest, [])?;
    boot_entries.push(String::from("etc/ld.so.preload"));

    if boot_entries.is_empty() {
        remove_file_if_exists(&config.boot_file_list)?;
    } else {
        write_boot_file_list(&config.boot_file_list, &boot_entries)?;
        copy_with_parent(
            &config.boot_file_list,
            &config.efi_boot_dir.join("BOOTFILES.TXT"),
        )?;
    }

    fs::write(&config.startup_nsh, "\\EFI\\BOOT\\BOOTX64.EFI\r\n")?;
    Ok(())
}

struct StageConfig {
    build_dir: PathBuf,
    image_dir: PathBuf,
    efi_boot_dir: PathBuf,
    boot_efi: PathBuf,
    artifact_boot_efi: PathBuf,
    artifact_prekernel_elf: PathBuf,
    prekernel_elf: PathBuf,
    artifact_kernel_elf: PathBuf,
    kernel_elf: PathBuf,
    artifact_user_elf: PathBuf,
    image_user_elf: PathBuf,
    artifact_win_user_exe: PathBuf,
    image_win_user_exe: PathBuf,
    artifact_printf_demo_elf: PathBuf,
    image_printf_demo_elf: PathBuf,
    artifact_amdgpu_ko: PathBuf,
    image_amdgpu_ko: PathBuf,
    artifact_psmouse_ko: PathBuf,
    image_psmouse_ko: PathBuf,
    amdgpu_firmware_dir: PathBuf,
    amdgpu_image_firmware_dir: PathBuf,
    amdgpu_required_firmware_basenames: Vec<String>,
    startup_nsh: PathBuf,
    boot_file_list: PathBuf,
    glibc_interpreter_source: Option<PathBuf>,
    glibc_libc_source: Option<PathBuf>,
    glibc_libgcc_source: Option<PathBuf>,
    glibc_interpreter_dest: PathBuf,
    glibc_libc_primary_dest: PathBuf,
    glibc_libc_fallback_dest: PathBuf,
    glibc_libgcc_primary_dest: PathBuf,
    glibc_libgcc_fallback_dest: PathBuf,
    glibc_ldso_cache_dest: PathBuf,
    glibc_ldso_preload_dest: PathBuf,
    ldconfig: Option<OsString>,
}

impl StageConfig {
    fn from_env() -> Result<Self> {
        let root_dir = env_path("ROOT_DIR").unwrap_or_else(default_root_dir);
        let build_dir = env_path("BUILD_DIR").unwrap_or_else(|| root_dir.join("build"));
        let artifact_dir = env_path("ARTIFACT_DIR").unwrap_or_else(|| build_dir.join("artifacts"));
        let image_dir = env_path("IMAGE_DIR").unwrap_or_else(|| build_dir.join("image"));
        let efi_boot_dir =
            env_path("EFI_BOOT_DIR").unwrap_or_else(|| image_dir.join("EFI/BOOT"));

        let cc = env::var_os("CC").unwrap_or_else(|| OsString::from("gcc"));
        let ldconfig = env::var_os("LDCONFIG").filter(|value| !value.is_empty()).or_else(|| {
            command_in_path("ldconfig")
                .map(PathBuf::into_os_string)
                .filter(|value| !value.is_empty())
        });

        Ok(Self {
            boot_efi: env_path("BOOT_EFI").unwrap_or_else(|| efi_boot_dir.join("BOOTX64.EFI")),
            artifact_boot_efi: env_path("ARTIFACT_BOOT_EFI")
                .unwrap_or_else(|| artifact_dir.join("boot/BOOTX64.EFI")),
            artifact_prekernel_elf: env_path("ARTIFACT_PREKERNEL_ELF")
                .unwrap_or_else(|| artifact_dir.join("boot/prekernel.elf")),
            prekernel_elf: env_path("PREKERNEL_ELF")
                .unwrap_or_else(|| image_dir.join("prekernel.elf")),
            artifact_kernel_elf: env_path("ARTIFACT_KERNEL_ELF")
                .unwrap_or_else(|| artifact_dir.join("kernel/kernel.elf")),
            kernel_elf: env_path("KERNEL_ELF").unwrap_or_else(|| image_dir.join("kernel.elf")),
            artifact_user_elf: env_path("ARTIFACT_USER_ELF")
                .unwrap_or_else(|| artifact_dir.join("system/apps/uiserver/uiserver.elf")),
            image_user_elf: env_path("IMAGE_USER_ELF")
                .unwrap_or_else(|| image_dir.join("system/apps/uiserver/uiserver.elf")),
            artifact_win_user_exe: env_path("ARTIFACT_WIN_USER_EXE")
                .unwrap_or_else(|| artifact_dir.join("system/apps/uiserver/uiserver.exe")),
            image_win_user_exe: env_path("IMAGE_WIN_USER_EXE")
                .unwrap_or_else(|| image_dir.join("system/apps/uiserver/uiserver.exe")),
            artifact_printf_demo_elf: env_path("ARTIFACT_PRINTF_DEMO_ELF")
                .unwrap_or_else(|| artifact_dir.join("system/apps/printfdemo/printfdemo.elf")),
            image_printf_demo_elf: env_path("IMAGE_PRINTF_DEMO_ELF")
                .unwrap_or_else(|| image_dir.join("system/apps/printfdemo/printfdemo.elf")),
            artifact_amdgpu_ko: env_path("ARTIFACT_AMDGPU_KO")
                .unwrap_or_else(|| artifact_dir.join("system/drivers/display/amdgpu.ko")),
            image_amdgpu_ko: env_path("IMAGE_AMDGPU_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/display/amdgpu.ko")),
            artifact_psmouse_ko: env_path("ARTIFACT_PSMOUSE_KO")
                .unwrap_or_else(|| artifact_dir.join("system/drivers/input/psmouse.ko")),
            image_psmouse_ko: env_path("IMAGE_PSMOUSE_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/input/psmouse.ko")),
            amdgpu_firmware_dir: env_path("AMDGPU_FIRMWARE_DIR")
                .unwrap_or_else(|| PathBuf::from("/lib/firmware/amdgpu")),
            amdgpu_image_firmware_dir: env_path("AMDGPU_IMAGE_FIRMWARE_DIR")
                .unwrap_or_else(|| image_dir.join("system/firmware/amdgpu")),
            amdgpu_required_firmware_basenames: env::var("AMDGPU_REQUIRED_FIRMWARE_BASENAMES")
                .ok()
                .map(|value| {
                    value
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|items| !items.is_empty())
                .unwrap_or_else(|| {
                    vec![
                        String::from("dcn_3_1_4_dmcub.bin"),
                        String::from("psp_13_0_10_sos.bin"),
                        String::from("psp_13_0_10_ta.bin"),
                        String::from("smu_13_0_10.bin"),
                    ]
                }),
            startup_nsh: env_path("STARTUP_NSH")
                .unwrap_or_else(|| image_dir.join("startup.nsh")),
            boot_file_list: env_path("BOOT_FILE_LIST")
                .unwrap_or_else(|| image_dir.join("BOOTFILES.TXT")),
            glibc_interpreter_source: env_path("GLIBC_INTERPRETER_SOURCE")
                .or_else(|| compiler_print_file_name(&cc, "ld-linux-x86-64.so.2")),
            glibc_libc_source: env_path("GLIBC_LIBC_SOURCE")
                .or_else(|| compiler_print_file_name(&cc, "libc.so.6")),
            glibc_libgcc_source: env_path("GLIBC_LIBGCC_SOURCE")
                .or_else(|| compiler_print_file_name(&cc, "libgcc_s.so.1")),
            glibc_interpreter_dest: env_path("GLIBC_INTERPRETER_DEST")
                .unwrap_or_else(|| image_dir.join("lib64/ld-linux-x86-64.so.2")),
            glibc_libc_primary_dest: env_path("GLIBC_LIBC_PRIMARY_DEST")
                .unwrap_or_else(|| image_dir.join("lib/x86_64-linux-gnu/libc.so.6")),
            glibc_libc_fallback_dest: env_path("GLIBC_LIBC_FALLBACK_DEST")
                .unwrap_or_else(|| image_dir.join("lib64/libc.so.6")),
            glibc_libgcc_primary_dest: env_path("GLIBC_LIBGCC_PRIMARY_DEST")
                .unwrap_or_else(|| image_dir.join("lib/x86_64-linux-gnu/libgcc_s.so.1")),
            glibc_libgcc_fallback_dest: env_path("GLIBC_LIBGCC_FALLBACK_DEST")
                .unwrap_or_else(|| image_dir.join("lib64/libgcc_s.so.1")),
            glibc_ldso_cache_dest: env_path("GLIBC_LDSO_CACHE_DEST")
                .unwrap_or_else(|| image_dir.join("etc/ld.so.cache")),
            glibc_ldso_preload_dest: env_path("GLIBC_LDSO_PRELOAD_DEST")
                .unwrap_or_else(|| image_dir.join("etc/ld.so.preload")),
            build_dir,
            image_dir,
            efi_boot_dir,
            ldconfig,
        })
    }
}

fn default_root_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate must live under workspace root")
        .to_path_buf()
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn copy_with_parent(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| format!("destination has no parent: {}", dst.display()))?;
    fs::create_dir_all(parent)?;
    fs::copy(src, dst)?;
    Ok(())
}

fn maybe_copy_host_runtime(
    src: &Option<PathBuf>,
    dst: &Path,
    boot_entry: &str,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    if let Some(src) = src.as_ref().filter(|path| path.is_file()) {
        copy_with_parent(src, dst)?;
        boot_entries.push(String::from(boot_entry));
    }
    Ok(())
}

fn maybe_copy_dual_host_runtime(
    src: &Option<PathBuf>,
    primary_dst: &Path,
    fallback_dst: &Path,
    primary_boot_entry: &str,
    fallback_boot_entry: &str,
    boot_entries: &mut Vec<String>,
) -> Result<()> {
    if let Some(src) = src.as_ref().filter(|path| path.is_file()) {
        copy_with_parent(src, primary_dst)?;
        copy_with_parent(src, fallback_dst)?;
        boot_entries.push(String::from(primary_boot_entry));
        boot_entries.push(String::from(fallback_boot_entry));
    }
    Ok(())
}

fn copy_or_unpack_firmware(firmware_dir: &Path, basename: &str, dst: &Path) -> Result<()> {
    let src_bin = firmware_dir.join(basename);
    if src_bin.is_file() {
        return copy_with_parent(&src_bin, dst);
    }

    let src_zst = firmware_dir.join(format!("{basename}.zst"));
    if !src_zst.is_file() {
        return Err(format!(
            "missing AMDGPU firmware blob: {}(.zst)",
            src_bin.display()
        )
        .into());
    }

    let parent = dst
        .parent()
        .ok_or_else(|| format!("firmware destination has no parent: {}", dst.display()))?;
    fs::create_dir_all(parent)?;

    let unpacker = command_in_path("zstd")
        .map(|_| OsString::from("zstd"))
        .or_else(|| command_in_path("zstdcat").map(|_| OsString::from("zstdcat")))
        .ok_or_else(|| format!("missing zstd/zstdcat to unpack {}", src_zst.display()))?;

    let status = if unpacker == OsStr::new("zstd") {
        Command::new(&unpacker)
            .arg("-dc")
            .arg(&src_zst)
            .stdout(fs::File::create(dst)?)
            .status()?
    } else {
        Command::new(&unpacker)
            .arg(&src_zst)
            .stdout(fs::File::create(dst)?)
            .status()?
    };

    if !status.success() {
        return Err(format!("failed to unpack {}", src_zst.display()).into());
    }

    Ok(())
}

fn write_boot_file_list(path: &Path, entries: &[String]) -> Result<()> {
    let mut content = String::new();
    for entry in entries {
        content.push_str(entry);
        content.push_str("\r\n");
    }
    fs::write(path, content)?;
    Ok(())
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn compiler_print_file_name(cc: &OsStr, file_name: &str) -> Option<PathBuf> {
    let output = Command::new(cc)
        .arg(format!("-print-file-name={file_name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let candidate = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if candidate.is_empty() || candidate == file_name {
        return None;
    }

    let path = PathBuf::from(candidate);
    path.is_file().then_some(path)
}

fn command_in_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn run_command(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("command failed with status {status}: {:?}", command).into());
    }
    Ok(())
}
