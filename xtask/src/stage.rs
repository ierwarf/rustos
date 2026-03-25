use std::fs;
use std::path::Path;

use xshell::cmd;

use crate::Result;
use crate::config::Config;
use crate::util::{
    copy_or_unpack_firmware, copy_with_parent, maybe_copy_dual_host_runtime,
    maybe_copy_host_runtime, maybe_copy_optional_file, push_boot_entry_unique,
    remove_dir_if_exists, remove_file_if_exists, shell, write_boot_file_list,
};

pub(crate) fn stage(config: &Config) -> Result<()> {
    remove_dir_if_exists(&config.image_dir)?;
    cleanup_legacy_build_layout(config)?;

    let mut boot_entries = Vec::new();
    stage_image_asset_overlay(
        &config.image_asset_overlay_dir,
        &config.image_dir,
        &mut boot_entries,
    )?;

    fs::create_dir_all(&config.efi_boot_dir)?;
    copy_with_parent(&config.artifact_boot_efi, &config.boot_efi)?;
    copy_with_parent(&config.artifact_prekernel_elf, &config.prekernel_elf)?;
    copy_with_parent(&config.artifact_kernel_elf, &config.kernel_elf)?;
    copy_with_parent(&config.artifact_user_elf, &config.image_user_elf)?;
    copy_with_parent(&config.artifact_win_user_exe, &config.image_win_user_exe)?;
    copy_with_parent(
        &config.artifact_printf_demo_elf,
        &config.image_printf_demo_elf,
    )?;
    copy_with_parent(&config.artifact_bootfb_ko, &config.image_bootfb_ko)?;
    copy_with_parent(&config.artifact_amdgpu_ko, &config.image_amdgpu_ko)?;

    fs::create_dir_all(&config.amdgpu_image_firmware_dir)?;

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
            fs::create_dir_all(
                config
                    .glibc_ldso_cache_dest
                    .parent()
                    .ok_or("ld.so.cache destination has no parent")?,
            )?;
            let root = &config.image_dir;
            cmd!(sh, "{ldconfig} -r {root} -C /etc/ld.so.cache").run()?;
            push_boot_entry_unique(&mut boot_entries, "etc/ld.so.cache");
        }
    }

    push_boot_entry_unique(&mut boot_entries, "system/apps/uiserver/uiserver.elf");
    push_boot_entry_unique(&mut boot_entries, "system/apps/uiserver/uiserver.exe");
    push_boot_entry_unique(&mut boot_entries, "system/apps/printfdemo/printfdemo.elf");
    push_boot_entry_unique(&mut boot_entries, "system/drivers/display/bootfb.ko");
    push_boot_entry_unique(&mut boot_entries, "system/drivers/display/amdgpu.ko");

    for basename in &config.amdgpu_required_firmware_basenames {
        let dst = config.amdgpu_image_firmware_dir.join(basename);
        copy_or_unpack_firmware(&config.amdgpu_firmware_dir, basename, &dst)?;
        push_boot_entry_unique(
            &mut boot_entries,
            &format!("system/firmware/amdgpu/{basename}"),
        );
    }

    maybe_copy_optional_file(
        &config.vendor_psmouse_ko,
        &config.image_psmouse_ko,
        "system/drivers/input/psmouse.ko",
        &mut boot_entries,
    )?;
    maybe_copy_optional_file(
        &config.vendor_hid_ko,
        &config.image_hid_ko,
        "system/drivers/input/hid.ko",
        &mut boot_entries,
    )?;
    maybe_copy_optional_file(
        &config.vendor_hid_generic_ko,
        &config.image_hid_generic_ko,
        "system/drivers/input/hid-generic.ko",
        &mut boot_entries,
    )?;
    maybe_copy_optional_file(
        &config.vendor_usbhid_ko,
        &config.image_usbhid_ko,
        "system/drivers/input/usbhid.ko",
        &mut boot_entries,
    )?;

    fs::create_dir_all(
        config
            .glibc_ldso_preload_dest
            .parent()
            .ok_or("ld.so.preload destination has no parent")?,
    )?;
    fs::write(&config.glibc_ldso_preload_dest, [])?;
    push_boot_entry_unique(&mut boot_entries, "etc/ld.so.preload");

    if boot_entries.is_empty() {
        remove_file_if_exists(&config.boot_file_list)?;
    } else {
        write_boot_file_list(&config.boot_file_list, &boot_entries)?;
    }
    Ok(())
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
        config.image_dir.join("startup.nsh"),
        config.build_dir.join("kernel.elf"),
        config.build_dir.join("prekernel.elf"),
        config.build_dir.join("UISERVER.ELF"),
        config.build_dir.join("UISERVER.EXE"),
        config.build_dir.join("PRINTFDEMO.ELF"),
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
