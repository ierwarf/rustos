use std::ffi::OsString;
use std::path::PathBuf;

use crate::Result;
use crate::util::{default_root_dir, env_os, env_path, env_string, split_whitespace_owned};

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
    pub(crate) nucleus_package: String,
    pub(crate) prekernel_package: String,
    pub(crate) user_elf_package: String,
    pub(crate) user_elf_linkage: String,
    pub(crate) kernel_cargo_zflags: Vec<String>,
    pub(crate) prekernel_rustc_args: Vec<String>,
    pub(crate) nucleus_rustc_args: Vec<String>,
    pub(crate) build_dir: PathBuf,
    pub(crate) artifact_dir: PathBuf,
    pub(crate) logs_dir: PathBuf,
    pub(crate) image_dir: PathBuf,
    pub(crate) image_asset_overlay_dir: PathBuf,
    pub(crate) amdgpu_firmware_dir: PathBuf,
    pub(crate) amdgpu_required_firmware_basenames: Vec<String>,
    pub(crate) ovmf_path: PathBuf,
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
        let nucleus_package = env_string("NUCLEUS_PACKAGE")
            .or_else(|| env_string("KERNEL_PACKAGE"))
            .unwrap_or_else(|| String::from("nucleus"));
        let prekernel_package =
            env_string("PREKERNEL_PACKAGE").unwrap_or_else(|| String::from("prekernel"));
        let user_elf_package =
            env_string("USER_ELF_PACKAGE").unwrap_or_else(|| String::from("uiserver"));
        let user_elf_linkage =
            env_string("USER_ELF_LINKAGE").unwrap_or_else(|| String::from("dynamic"));

        let assets_dir = env_path("ASSETS_DIR").unwrap_or_else(|| root_dir.join("assets"));
        let build_dir = env_path("BUILD_DIR").unwrap_or_else(|| root_dir.join("build"));
        let logs_dir = env_path("LOGS_DIR").unwrap_or_else(|| root_dir.join("logs"));
        let artifact_dir = env_path("ARTIFACT_DIR").unwrap_or_else(|| build_dir.join("artifacts"));
        let image_dir = env_path("IMAGE_DIR").unwrap_or_else(|| build_dir.join("image"));
        let vendor_dir = env_path("VENDOR_DIR").unwrap_or_else(|| root_dir.join("vendor"));

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
        let nucleus_rustc_args = env_string("NUCLEUS_RUSTC_ARGS")
            .or_else(|| env_string("KERNEL_RUSTC_ARGS"))
            .map(|value| split_whitespace_owned(&value))
            .unwrap_or_else(|| {
                split_whitespace_owned(
                    "-C no-redzone -C relocation-model=pic -C link-arg=-nostartfiles -C link-arg=-shared -C link-arg=-static -C link-arg=-Wl,-Bsymbolic -C link-arg=-Wl,-e,_start",
                )
            });
        Ok(Self {
            image_asset_overlay_dir: env_path("IMAGE_ASSET_OVERLAY_DIR")
                .unwrap_or_else(|| assets_dir.join("image")),
            amdgpu_firmware_dir: env_path("AMDGPU_FIRMWARE_DIR")
                .unwrap_or_else(|| PathBuf::from("/lib/firmware/amdgpu")),
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
            ovmf_path: env_path("OVMF_PATH").unwrap_or_else(|| {
                let new_path = vendor_dir.join("firmware/ovmf/OVMF.fd");
                if new_path.is_file() {
                    new_path
                } else {
                    vendor_dir.join("ovmf/OVMF.fd")
                }
            }),
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
            nucleus_package,
            prekernel_package,
            user_elf_package,
            user_elf_linkage,
            kernel_cargo_zflags,
            prekernel_rustc_args,
            nucleus_rustc_args,
            build_dir,
            artifact_dir,
            logs_dir,
            image_dir,
        })
    }

    pub(crate) fn boot_efi_path(&self) -> PathBuf {
        self.image_dir.join("EFI/BOOT/BOOTX64.EFI")
    }

    pub(crate) fn bootloader_source_efi_path(&self) -> PathBuf {
        self.cargo_target_dir.join(format!(
            "{}/release/{}.efi",
            self.target, self.bootloader_package
        ))
    }

    pub(crate) fn artifact_boot_efi_path(&self) -> PathBuf {
        self.artifact_dir.join("EFI/BOOT/BOOTX64.EFI")
    }

    pub(crate) fn prekernel_source_path(&self) -> PathBuf {
        self.cargo_target_dir.join(format!(
            "{}/release/{}",
            self.kernel_target, self.prekernel_package
        ))
    }

    pub(crate) fn artifact_prekernel_elf_path(&self) -> PathBuf {
        self.artifact_dir.join("prekernel.elf")
    }

    pub(crate) fn nucleus_source_path(&self) -> PathBuf {
        self.cargo_target_dir.join(format!(
            "{}/release/{}",
            self.kernel_target, self.nucleus_package
        ))
    }

    pub(crate) fn artifact_nucleus_elf_path(&self) -> PathBuf {
        self.artifact_dir.join("nucleus.elf")
    }

    pub(crate) fn amdgpu_image_firmware_dir(&self) -> PathBuf {
        self.image_dir.join("system/firmware/amdgpu")
    }

    pub(crate) fn userdemo2_import_audit_log_path(&self) -> PathBuf {
        self.logs_dir.join("userdemo2-imports.txt")
    }

    pub(crate) fn kernel_release_deps_dir(&self) -> PathBuf {
        self.cargo_target_dir
            .join(format!("{}/release/deps", self.kernel_target))
    }
}
