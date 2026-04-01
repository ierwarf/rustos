use std::ffi::OsString;
use std::path::PathBuf;

use crate::util::{
    command_in_path, compiler_print_file_name, default_root_dir, env_os, env_path, env_string,
    split_whitespace_owned,
};
use crate::Result;

pub(crate) struct Config {
    pub(crate) root_dir: PathBuf,
    pub(crate) workspace_manifest: PathBuf,
    pub(crate) cargo_target_dir: PathBuf,
    pub(crate) cargo: OsString,
    pub(crate) rustup: OsString,
    pub(crate) cc: OsString,
    pub(crate) ld: OsString,
    pub(crate) mingw_cc: OsString,
    pub(crate) objdump: OsString,
    pub(crate) qemu_bin: OsString,
    pub(crate) target: String,
    pub(crate) kernel_target: String,
    pub(crate) bootloader_package: String,
    pub(crate) kernel_package: String,
    pub(crate) prekernel_package: String,
    pub(crate) user_elf_package: String,
    pub(crate) user_elf_linkage: String,
    pub(crate) kernel_cargo_zflags: Vec<String>,
    pub(crate) prekernel_rustc_args: Vec<String>,
    pub(crate) kernel_rustc_args: Vec<String>,
    pub(crate) build_dir: PathBuf,
    pub(crate) artifact_dir: PathBuf,
    pub(crate) logs_dir: PathBuf,
    pub(crate) image_dir: PathBuf,
    pub(crate) image_asset_overlay_dir: PathBuf,
    pub(crate) boot_efi: PathBuf,
    pub(crate) source_efi: PathBuf,
    pub(crate) artifact_boot_efi: PathBuf,
    pub(crate) prekernel_source: PathBuf,
    pub(crate) artifact_prekernel_elf: PathBuf,
    pub(crate) prekernel_elf: PathBuf,
    pub(crate) kernel_source: PathBuf,
    pub(crate) artifact_kernel_elf: PathBuf,
    pub(crate) kernel_elf: PathBuf,
    pub(crate) user_build_dir: PathBuf,
    pub(crate) image_user_elf: PathBuf,
    pub(crate) userdemo2_exe: PathBuf,
    pub(crate) userdemo2_import_audit_log: PathBuf,
    pub(crate) image_userdemo2_exe: PathBuf,
    pub(crate) winsys_dir: PathBuf,
    pub(crate) artifact_winsys_dir: PathBuf,
    pub(crate) image_winsys_dir: PathBuf,
    pub(crate) image_shell_elf: PathBuf,
    pub(crate) image_bootfb_ko: PathBuf,
    pub(crate) image_amdgpu_ko: PathBuf,
    pub(crate) vendor_psmouse_ko: PathBuf,
    pub(crate) image_psmouse_ko: PathBuf,
    pub(crate) vendor_hid_ko: PathBuf,
    pub(crate) image_hid_ko: PathBuf,
    pub(crate) vendor_hid_generic_ko: PathBuf,
    pub(crate) image_hid_generic_ko: PathBuf,
    pub(crate) vendor_usbhid_ko: PathBuf,
    pub(crate) image_usbhid_ko: PathBuf,
    pub(crate) amdgpu_firmware_dir: PathBuf,
    pub(crate) amdgpu_image_firmware_dir: PathBuf,
    pub(crate) amdgpu_required_firmware_basenames: Vec<String>,
    pub(crate) glibc_interpreter_source: Option<PathBuf>,
    pub(crate) glibc_libc_source: Option<PathBuf>,
    pub(crate) glibc_libgcc_source: Option<PathBuf>,
    pub(crate) glibc_interpreter_dest: PathBuf,
    pub(crate) glibc_libc_primary_dest: PathBuf,
    pub(crate) glibc_libc_fallback_dest: PathBuf,
    pub(crate) glibc_libgcc_primary_dest: PathBuf,
    pub(crate) glibc_libgcc_fallback_dest: PathBuf,
    pub(crate) glibc_ldso_cache_dest: PathBuf,
    pub(crate) glibc_ldso_preload_dest: PathBuf,
    pub(crate) ovmf_path: PathBuf,
    pub(crate) ldconfig: Option<OsString>,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self> {
        let root_dir = env_path("ROOT_DIR").unwrap_or_else(default_root_dir);
        let workspace_manifest = env_path("WORKSPACE_MANIFEST")
            .or_else(|| env_path("KERNEL_GROUND_MANIFEST"))
            .unwrap_or_else(|| root_dir.join("Cargo.toml"));
        let cargo_target_dir =
            env_path("CARGO_TARGET_DIR").unwrap_or_else(|| root_dir.join("target"));

        let cargo = env_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let rustup = env_os("RUSTUP").unwrap_or_else(|| OsString::from("rustup"));
        let cc = env_os("CC").unwrap_or_else(|| OsString::from("gcc"));
        let ld = env_os("LD").unwrap_or_else(|| OsString::from("ld"));
        let mingw_cc =
            env_os("MINGW_CC").unwrap_or_else(|| OsString::from("x86_64-w64-mingw32-gcc"));
        let objdump = env_os("OBJDUMP").unwrap_or_else(|| OsString::from("objdump"));
        let qemu_bin = env_os("QEMU_BIN").unwrap_or_else(|| OsString::from("qemu-system-x86_64"));

        let target = env_string("TARGET").unwrap_or_else(|| String::from("x86_64-unknown-uefi"));
        let kernel_target =
            env_string("KERNEL_TARGET").unwrap_or_else(|| String::from("x86_64-unknown-linux-gnu"));
        let bootloader_package =
            env_string("BOOTLOADER_PACKAGE").unwrap_or_else(|| String::from("bootloader"));
        let kernel_package = env_string("KERNEL_PACKAGE").unwrap_or_else(|| String::from("kernel"));
        let prekernel_package =
            env_string("PREKERNEL_PACKAGE").unwrap_or_else(|| String::from("prekernel"));
        let user_elf_package =
            env_string("USER_ELF_PACKAGE").unwrap_or_else(|| String::from("uiserver"));
        let user_elf_linkage =
            env_string("USER_ELF_LINKAGE").unwrap_or_else(|| String::from("dynamic"));

        let assets_dir = env_path("ASSETS_DIR").unwrap_or_else(|| root_dir.join("assets"));
        let build_dir = env_path("BUILD_DIR").unwrap_or_else(|| root_dir.join("build"));
        let logs_dir = env_path("LOGS_DIR").unwrap_or_else(|| build_dir.join("logs"));
        let vendor_dir = env_path("VENDOR_DIR").unwrap_or_else(|| root_dir.join("vendor"));
        let artifact_dir = env_path("ARTIFACT_DIR").unwrap_or_else(|| build_dir.join("artifacts"));
        let image_dir = env_path("IMAGE_DIR").unwrap_or_else(|| build_dir.join("image"));
        let efi_boot_dir = env_path("EFI_BOOT_DIR").unwrap_or_else(|| image_dir.join("EFI/BOOT"));
        let user_build_dir =
            env_path("USER_BUILD_DIR").unwrap_or_else(|| root_dir.join("target/uiserver"));
        let vendor_input_modules_dir = env_path("VENDOR_INPUT_MODULES_DIR")
            .unwrap_or_else(|| vendor_dir.join("modules/input"));

        let kernel_cargo_zflags = env_string("KERNEL_CARGO_ZFLAGS")
            .map(|value| split_whitespace_owned(&value))
            .unwrap_or_else(|| {
                split_whitespace_owned(
                    "-Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem",
                )
            });
        let prekernel_rustc_args = env_string("PREKERNEL_RUSTC_ARGS")
            .map(|value| split_whitespace_owned(&value))
            .unwrap_or_else(|| {
                split_whitespace_owned(
                    "-C no-redzone -C link-arg=-nostartfiles -C link-arg=-no-pie -C link-arg=-static -C link-arg=-Wl,--image-base=0x100000",
                )
            });
        let kernel_rustc_args = env_string("KERNEL_RUSTC_ARGS")
            .map(|value| split_whitespace_owned(&value))
            .unwrap_or_else(|| {
                split_whitespace_owned(
                    "-C no-redzone -C relocation-model=pic -C link-arg=-nostartfiles -C link-arg=-shared -C link-arg=-static -C link-arg=-Wl,-Bsymbolic -C link-arg=-Wl,-e,_start",
                )
            });

        let ldconfig = env_os("LDCONFIG")
            .filter(|value| !value.is_empty())
            .or_else(|| {
                command_in_path("ldconfig")
                    .map(PathBuf::into_os_string)
                    .filter(|value| !value.is_empty())
            });

        Ok(Self {
            image_asset_overlay_dir: env_path("IMAGE_ASSET_OVERLAY_DIR")
                .unwrap_or_else(|| assets_dir.join("image")),
            boot_efi: env_path("BOOT_EFI").unwrap_or_else(|| efi_boot_dir.join("BOOTX64.EFI")),
            source_efi: env_path("SOURCE_EFI").unwrap_or_else(|| {
                cargo_target_dir.join(format!("{target}/release/{bootloader_package}.efi"))
            }),
            artifact_boot_efi: env_path("ARTIFACT_BOOT_EFI")
                .unwrap_or_else(|| artifact_dir.join("EFI/BOOT/BOOTX64.EFI")),
            prekernel_source: env_path("PREKERNEL_SOURCE").unwrap_or_else(|| {
                cargo_target_dir.join(format!("{kernel_target}/release/{prekernel_package}"))
            }),
            artifact_prekernel_elf: env_path("ARTIFACT_PREKERNEL_ELF")
                .unwrap_or_else(|| artifact_dir.join("prekernel.elf")),
            prekernel_elf: env_path("PREKERNEL_ELF")
                .unwrap_or_else(|| image_dir.join("prekernel.elf")),
            kernel_source: env_path("KERNEL_SOURCE").unwrap_or_else(|| {
                cargo_target_dir.join(format!("{kernel_target}/release/{kernel_package}"))
            }),
            artifact_kernel_elf: env_path("ARTIFACT_KERNEL_ELF")
                .unwrap_or_else(|| artifact_dir.join("kernel.elf")),
            kernel_elf: env_path("KERNEL_ELF").unwrap_or_else(|| image_dir.join("kernel.elf")),
            image_user_elf: env_path("IMAGE_USER_ELF")
                .unwrap_or_else(|| image_dir.join("system/packages/uiserver/uiserver.elf")),
            userdemo2_exe: env_path("USERDEMO2_EXE")
                .unwrap_or_else(|| user_build_dir.join("USERDEMO2.EXE")),
            userdemo2_import_audit_log: env_path("USERDEMO2_IMPORT_AUDIT_LOG")
                .unwrap_or_else(|| logs_dir.join("userdemo2-imports.txt")),
            image_userdemo2_exe: env_path("IMAGE_USERDEMO2_EXE")
                .unwrap_or_else(|| image_dir.join("samples/windows/userdemo2/userdemo2.exe")),
            winsys_dir: env_path("WINSYS_DIR")
                .unwrap_or_else(|| root_dir.join("compat/windows/user/winsys")),
            artifact_winsys_dir: env_path("ARTIFACT_WINSYS_DIR")
                .unwrap_or_else(|| artifact_dir.join("compat/windows/System32")),
            image_winsys_dir: env_path("IMAGE_WINSYS_DIR")
                .unwrap_or_else(|| image_dir.join("compat/windows/System32")),
            image_shell_elf: env_path("IMAGE_SHELL_ELF")
                .unwrap_or_else(|| image_dir.join("samples/shell/shell.elf")),
            image_bootfb_ko: env_path("IMAGE_BOOTFB_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/display/bootfb.ko")),
            image_amdgpu_ko: env_path("IMAGE_AMDGPU_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/display/amdgpu.ko")),
            vendor_psmouse_ko: env_path("VENDOR_PSMOUSE_KO")
                .unwrap_or_else(|| vendor_input_modules_dir.join("psmouse.ko")),
            image_psmouse_ko: env_path("IMAGE_PSMOUSE_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/input/psmouse.ko")),
            vendor_hid_ko: env_path("VENDOR_HID_KO")
                .unwrap_or_else(|| vendor_input_modules_dir.join("hid.ko")),
            image_hid_ko: env_path("IMAGE_HID_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/input/hid.ko")),
            vendor_hid_generic_ko: env_path("VENDOR_HID_GENERIC_KO")
                .unwrap_or_else(|| vendor_input_modules_dir.join("hid-generic.ko")),
            image_hid_generic_ko: env_path("IMAGE_HID_GENERIC_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/input/hid-generic.ko")),
            vendor_usbhid_ko: env_path("VENDOR_USBHID_KO")
                .unwrap_or_else(|| vendor_input_modules_dir.join("usbhid.ko")),
            image_usbhid_ko: env_path("IMAGE_USBHID_KO")
                .unwrap_or_else(|| image_dir.join("system/drivers/input/usbhid.ko")),
            amdgpu_firmware_dir: env_path("AMDGPU_FIRMWARE_DIR")
                .unwrap_or_else(|| PathBuf::from("/lib/firmware/amdgpu")),
            amdgpu_image_firmware_dir: env_path("AMDGPU_IMAGE_FIRMWARE_DIR")
                .unwrap_or_else(|| image_dir.join("system/firmware/amdgpu")),
            amdgpu_required_firmware_basenames: env_string("AMDGPU_REQUIRED_FIRMWARE_BASENAMES")
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
            ovmf_path: env_path("OVMF_PATH").unwrap_or_else(|| vendor_dir.join("ovmf/OVMF.fd")),
            root_dir,
            workspace_manifest,
            cargo_target_dir,
            cargo,
            rustup,
            cc,
            ld,
            mingw_cc,
            objdump,
            qemu_bin,
            target,
            kernel_target,
            bootloader_package,
            kernel_package,
            prekernel_package,
            user_elf_package,
            user_elf_linkage,
            kernel_cargo_zflags,
            prekernel_rustc_args,
            kernel_rustc_args,
            build_dir,
            artifact_dir,
            logs_dir,
            image_dir,
            user_build_dir,
            ldconfig,
        })
    }
    pub(crate) fn kernel_release_deps_dir(&self) -> PathBuf {
        self.cargo_target_dir
            .join(format!("{}/release/deps", self.kernel_target))
    }
}
