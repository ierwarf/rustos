use std::ffi::OsString;
use std::path::PathBuf;

use crate::Result;
use crate::util::{default_root_dir, env_os, env_path, env_string, split_whitespace_owned};

mod project;

pub(crate) use project::{ProjectConfig, effective_config_toml, load_project_config};

pub(crate) struct Config {
    pub(crate) project: ProjectConfig,
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
    pub(crate) grub_mkstandalone: OsString,
    pub(crate) grub_file: OsString,
    pub(crate) gpg: OsString,
    pub(crate) kernel_target: String,
    pub(crate) nucleus_package: String,
    pub(crate) user_elf_package: String,
    pub(crate) user_elf_linkage: String,
    pub(crate) kernel_cargo_zflags: Vec<String>,
    pub(crate) nucleus_rustc_args: Vec<String>,
    pub(crate) rustos_grub_pubkey: Option<PathBuf>,
    pub(crate) rustos_grub_signing_key: Option<String>,
    pub(crate) rustos_gpg_home: Option<PathBuf>,
    pub(crate) rustos_grub_sbat: Option<PathBuf>,
    pub(crate) rustos_grub_modules: Option<String>,
    pub(crate) build_dir: PathBuf,
    pub(crate) artifact_dir: PathBuf,
    pub(crate) logs_dir: PathBuf,
    pub(crate) image_dir: PathBuf,
    pub(crate) boot_disk_image: PathBuf,
    pub(crate) image_asset_overlay_dir: PathBuf,
    pub(crate) amdgpu_firmware_dir: PathBuf,
    pub(crate) amdgpu_required_firmware_basenames: Vec<String>,
    pub(crate) ovmf_path: PathBuf,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self> {
        let root_dir = env_path("ROOT_DIR").unwrap_or_else(default_root_dir);
        let project = load_project_config(&root_dir)?;
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
        let grub_mkstandalone =
            env_os("GRUB_MKSTANDALONE").unwrap_or_else(|| OsString::from("grub-mkstandalone"));
        let grub_file = env_os("GRUB_FILE").unwrap_or_else(|| OsString::from("grub-file"));
        let gpg = env_os("GPG").unwrap_or_else(|| OsString::from("gpg"));

        let kernel_target =
            env_string("KERNEL_TARGET").unwrap_or_else(|| String::from("x86_64-unknown-linux-gnu"));
        let nucleus_package = env_string("NUCLEUS_PACKAGE")
            .or_else(|| env_string("KERNEL_PACKAGE"))
            .unwrap_or_else(|| String::from("nucleus"));
        let user_elf_package =
            env_string("USER_ELF_PACKAGE").unwrap_or_else(|| String::from("uiserver"));
        let user_elf_linkage =
            env_string("USER_ELF_LINKAGE").unwrap_or_else(|| String::from("dynamic"));

        let assets_dir = env_path("ASSETS_DIR").unwrap_or_else(|| root_dir.join("assets"));
        let build_dir = env_path("BUILD_DIR").unwrap_or_else(|| root_dir.join("build"));
        let logs_dir = env_path("LOGS_DIR").unwrap_or_else(|| root_dir.join("logs"));
        let artifact_dir = env_path("ARTIFACT_DIR").unwrap_or_else(|| build_dir.join("artifacts"));
        let image_dir = env_path("IMAGE_DIR").unwrap_or_else(|| build_dir.join("image"));
        let boot_disk_image =
            env_path("BOOT_DISK_IMAGE").unwrap_or_else(|| build_dir.join("rustos-boot.img"));
        let vendor_dir = env_path("VENDOR_DIR").unwrap_or_else(|| root_dir.join("vendor"));

        let kernel_cargo_zflags = env_string("KERNEL_CARGO_ZFLAGS")
            .map(|value| split_whitespace_owned(&value))
            .unwrap_or_else(|| {
                split_whitespace_owned(
                    "-Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem",
                )
            });
        let nucleus_rustc_args = env_string("NUCLEUS_RUSTC_ARGS")
            .or_else(|| env_string("KERNEL_RUSTC_ARGS"))
            .map(|value| split_whitespace_owned(&value))
            .unwrap_or_else(|| {
                let linker_script = root_dir.join("kernel/linker-multiboot2.ld");
                split_whitespace_owned(&format!(
                    "-C no-redzone -C relocation-model=static -C link-arg=-nostartfiles -C link-arg=-no-pie -C link-arg=-static -C link-arg=-Wl,-T,{} -C link-arg=-Wl,-e,_start",
                    linker_script.display()
                ))
            });
        let rustos_grub_pubkey = env_path("RUSTOS_GRUB_PUBKEY");
        let rustos_grub_signing_key = env_string("RUSTOS_GRUB_SIGNING_KEY");
        let rustos_gpg_home = env_path("RUSTOS_GPG_HOME");
        let rustos_grub_sbat = env_path("RUSTOS_GRUB_SBAT");
        let rustos_grub_modules = env_string("RUSTOS_GRUB_MODULES");
        Ok(Self {
            project,
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
            grub_mkstandalone,
            grub_file,
            gpg,
            kernel_target,
            nucleus_package,
            user_elf_package,
            user_elf_linkage,
            kernel_cargo_zflags,
            nucleus_rustc_args,
            rustos_grub_pubkey,
            rustos_grub_signing_key,
            rustos_gpg_home,
            rustos_grub_sbat,
            rustos_grub_modules,
            build_dir,
            artifact_dir,
            logs_dir,
            image_dir,
            boot_disk_image,
        })
    }

    pub(crate) fn boot_efi_path(&self) -> PathBuf {
        self.image_dir.join("EFI/BOOT/BOOTX64.EFI")
    }

    pub(crate) fn artifact_boot_efi_path(&self) -> PathBuf {
        self.artifact_dir.join("EFI/BOOT/BOOTX64.EFI")
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

    pub(crate) fn artifact_nucleus_signature_path(&self) -> PathBuf {
        self.artifact_dir.join("nucleus.elf.sig")
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

pub(crate) fn check(config: &Config) -> Result<()> {
    let _ = effective_config_toml(&config.project);
    eprintln!("xtask: config ok ({})", config.project.source.label());
    Ok(())
}

pub(crate) fn show(config: &Config) -> Result<()> {
    print!("{}", effective_config_toml(&config.project));
    Ok(())
}
