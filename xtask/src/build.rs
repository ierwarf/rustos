use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xshell::cmd;

use crate::Result;
use crate::config::Config;
use crate::stage;
use crate::util::{
    command_in_path, copy_with_parent, create_temp_dir, remove_dir_if_exists,
    remove_file_if_exists, run_cargo_kernel_check, run_cargo_kernel_rustc, run_command, shell,
};

pub(crate) fn build(config: &Config) -> Result<()> {
    ensure_targets(config)?;
    build_efi(config)?;
    build_prekernel(config)?;
    build_kernel(config)?;
    build_user(config)?;
    build_console_demo(config)?;
    build_driver_modules(config)?;
    stage::stage(config)?;

    println!("UEFI image ready: {}", config.boot_efi.display());
    println!("Prekernel ELF ready: {}", config.prekernel_elf.display());
    println!("Kernel ELF ready: {}", config.kernel_elf.display());
    println!("User ELF ready: {}", config.image_user_elf.display());
    println!("User EXE ready: {}", config.image_win_user_exe.display());
    println!(
        "Console demo ELF ready: {}",
        config.image_printf_demo_elf.display()
    );
    println!(
        "Boot framebuffer driver module ready: {}",
        config.image_bootfb_ko.display()
    );
    println!(
        "AMDGPU driver module ready: {}",
        config.image_amdgpu_ko.display()
    );
    print_vendor_module_status(
        "PS/2 mouse driver module",
        &config.vendor_psmouse_ko,
        &config.image_psmouse_ko,
    );
    print_vendor_module_status(
        "HID core module",
        &config.vendor_hid_ko,
        &config.image_hid_ko,
    );
    print_vendor_module_status(
        "HID generic module",
        &config.vendor_hid_generic_ko,
        &config.image_hid_generic_ko,
    );
    print_vendor_module_status(
        "USB HID module",
        &config.vendor_usbhid_ko,
        &config.image_usbhid_ko,
    );
    if config.boot_file_list.is_file() {
        println!(
            "Boot file manifest ready: {}",
            config.boot_file_list.display()
        );
    }

    Ok(())
}

pub(crate) fn check(config: &Config) -> Result<()> {
    ensure_targets(config)?;
    let sh = shell()?;
    let cargo = &config.cargo;

    let package = &config.bootloader_package;
    let target = &config.target;
    cmd!(sh, "{cargo} check -p {package} --target {target}").run()?;

    run_cargo_kernel_check(config, &config.prekernel_package)?;
    run_cargo_kernel_check(config, &config.kernel_package)?;

    let package = &config.user_elf_package;
    let target = &config.kernel_target;
    cmd!(sh, "{cargo} check -p {package} --target {target}").run()?;

    cmd!(sh, "{cargo} check --workspace").run()?;

    Ok(())
}

pub(crate) fn clean(config: &Config) -> Result<()> {
    let sh = shell()?;
    let cargo = &config.cargo;
    cmd!(sh, "{cargo} clean").run()?;
    remove_dir_if_exists(&config.build_dir)?;
    Ok(())
}

pub(crate) fn ensure_targets(config: &Config) -> Result<()> {
    let sh = shell()?;
    let rustup = &config.rustup;
    let target = &config.target;
    cmd!(sh, "{rustup} target add {target}").run()?;
    let target = &config.kernel_target;
    cmd!(sh, "{rustup} target add {target}").run()?;
    Ok(())
}

pub(crate) fn build_efi(config: &Config) -> Result<()> {
    let sh = shell()?;
    let cargo = &config.cargo;
    let package = &config.bootloader_package;
    let target = &config.target;
    cmd!(sh, "{cargo} build -p {package} --target {target} --release").run()?;
    remove_file_if_exists(&config.build_dir.join("artifacts/boot/BOOTX64.EFI"))?;
    copy_with_parent(&config.source_efi, &config.artifact_boot_efi)
}

pub(crate) fn build_prekernel(config: &Config) -> Result<()> {
    run_cargo_kernel_rustc(
        config,
        &config.prekernel_package,
        &config.prekernel_rustc_args,
    )?;
    copy_with_parent(&config.prekernel_source, &config.artifact_prekernel_elf)
}

pub(crate) fn build_kernel(config: &Config) -> Result<()> {
    run_cargo_kernel_rustc(config, &config.kernel_package, &config.kernel_rustc_args)?;
    copy_with_parent(&config.kernel_source, &config.artifact_kernel_elf)
}

pub(crate) fn build_user(config: &Config) -> Result<()> {
    if config.user_elf_linkage != "dynamic" {
        return Err(format!(
            "Rust std UISERVER currently supports only USER_ELF_LINKAGE=dynamic, got {}",
            config.user_elf_linkage
        )
        .into());
    }

    fs::create_dir_all(&config.user_build_dir)?;
    let sh = shell()?;
    let cargo = &config.cargo;
    let package = &config.user_elf_package;
    let target = &config.kernel_target;
    cmd!(sh, "{cargo} build -p {package} --target {target} --release").run()?;

    copy_with_parent(&config.user_binary, &config.user_source)?;

    let win_object_parent = config.win_user_object.parent().ok_or_else(|| {
        format!(
            "Windows object path has no parent: {}",
            config.win_user_object.display()
        )
    })?;
    fs::create_dir_all(win_object_parent)?;

    run_command(
        Command::new(&config.nasm)
            .arg("-f")
            .arg("win64")
            .arg("-o")
            .arg(&config.win_user_object)
            .arg(&config.win_user_asm_source),
    )?;

    run_command(
        Command::new(&config.ld)
            .arg("-nostdlib")
            .arg("-s")
            .arg("-m")
            .arg("i386pep")
            .arg("-e")
            .arg("main")
            .arg("--image-base")
            .arg("0x400000")
            .arg("-o")
            .arg(&config.win_user_source)
            .arg(&config.win_user_object),
    )?;

    copy_with_parent(&config.user_source, &config.artifact_user_elf)?;
    copy_with_parent(&config.win_user_source, &config.artifact_win_user_exe)?;
    Ok(())
}

pub(crate) fn build_console_demo(config: &Config) -> Result<()> {
    let parent = config.artifact_printf_demo_elf.parent().ok_or_else(|| {
        format!(
            "printf demo artifact path has no parent: {}",
            config.artifact_printf_demo_elf.display()
        )
    })?;
    fs::create_dir_all(parent)?;

    run_command(
        Command::new(&config.cc)
            .arg(&config.printf_demo_source)
            .arg("-o")
            .arg(&config.artifact_printf_demo_elf),
    )
}

pub(crate) fn build_driver_modules(config: &Config) -> Result<()> {
    build_rust_module_image(
        config,
        "rustos-bootfb-driver",
        "rustos_bootfb_driver",
        &config.artifact_bootfb_ko,
        &["driver_module_runtime", "driver_abi"],
    )?;
    build_rust_module_image(
        config,
        "rustos-amdgpu-driver",
        "rustos_amdgpu_driver",
        &config.artifact_amdgpu_ko,
        &["driver_module_runtime", "driver_abi"],
    )?;

    for vendor_path in [
        &config.vendor_psmouse_ko,
        &config.vendor_hid_ko,
        &config.vendor_hid_generic_ko,
        &config.vendor_usbhid_ko,
    ] {
        if !vendor_path.is_file() {
            eprintln!(
                "xtask: warning: optional vendor module not found: {}",
                vendor_path.display()
            );
        }
    }

    Ok(())
}

fn build_rust_module_image(
    config: &Config,
    package: &str,
    crate_name: &str,
    output: &Path,
    dependency_crates: &[&str],
) -> Result<()> {
    let output_parent = output
        .parent()
        .ok_or_else(|| format!("module artifact path has no parent: {}", output.display()))?;
    fs::create_dir_all(output_parent)?;
    remove_file_if_exists(output)?;

    let deps_dir = config.kernel_release_deps_dir();

    let mut command = Command::new(&config.cargo);
    command.arg("rustc");
    for flag in &config.kernel_cargo_zflags {
        command.arg(flag);
    }
    command
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target)
        .arg("--release")
        .arg("--lib")
        .arg("--features")
        .arg("module-image")
        .arg("--")
        .arg("--emit=obj")
        .arg("-C")
        .arg("panic=abort")
        .arg("-C")
        .arg("no-redzone")
        .arg("-C")
        .arg("relocation-model=pic");
    run_command(&mut command)?;

    let ar_bin = command_in_path("llvm-ar")
        .or_else(|| command_in_path("ar"))
        .ok_or("missing llvm-ar/ar for module archive extraction")?;
    let temp_dir = create_temp_dir("rustos-module-link")?;
    let mut link_inputs = Vec::new();
    let mut archives = dependency_crates
        .iter()
        .map(|dependency| (*dependency).to_owned())
        .collect::<Vec<_>>();
    for builtin in ["core", "alloc", "compiler_builtins"] {
        if !archives.iter().any(|existing| existing == builtin) {
            archives.push(String::from(builtin));
        }
    }

    let self_archive = find_latest_rlib_artifact(&deps_dir, &format!("lib{crate_name}-"))?;
    extract_archive_objects(
        &ar_bin,
        &self_archive,
        &temp_dir.join(crate_name),
        &mut link_inputs,
    )?;

    for dependency in &archives {
        let archive = find_latest_rlib_artifact(&deps_dir, &format!("lib{dependency}-"))?;
        extract_archive_objects(
            &ar_bin,
            &archive,
            &temp_dir.join(dependency),
            &mut link_inputs,
        )?;
    }

    let result = if link_inputs.len() == 1 {
        copy_with_parent(&link_inputs[0], output)
    } else {
        let mut command = Command::new(&config.ld);
        command.arg("-r").arg("-o").arg(output);
        for input in &link_inputs {
            command.arg(input);
        }
        run_command(&mut command)
    };

    let _ = remove_dir_if_exists(&temp_dir);
    result
}

fn find_latest_rlib_artifact(dir: &Path, prefix: &str) -> Result<PathBuf> {
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("rlib")
            || !file_name.starts_with(prefix)
        {
            continue;
        }

        let modified = entry.metadata()?.modified()?;
        match &latest {
            Some((best_time, _)) if &modified <= best_time => {}
            _ => latest = Some((modified, path)),
        }
    }

    latest
        .map(|(_, path)| path)
        .ok_or_else(|| format!("module rlib artifact not found under {}", dir.display()).into())
}

fn collect_object_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(dir)?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("o"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn extract_archive_objects(
    ar_bin: &Path,
    archive: &Path,
    extract_dir: &Path,
    link_inputs: &mut Vec<PathBuf>,
) -> Result<()> {
    fs::create_dir_all(extract_dir)?;
    run_command(
        Command::new(ar_bin)
            .current_dir(extract_dir)
            .arg("x")
            .arg(archive),
    )?;
    link_inputs.extend(collect_object_files(extract_dir)?);
    Ok(())
}

fn print_vendor_module_status(label: &str, src: &Path, dst: &Path) {
    if src.is_file() {
        println!("{label} ready: {}", dst.display());
    } else {
        println!("{label} skipped: {}", src.display());
    }
}
