use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Result;
use crate::config::Config;
use crate::layering::validate_workspace_layering;
use crate::package_manifest::{
    BuilderKind, PackageManifest, load_default_manifests, required_manifest,
};
use crate::stage;
use crate::util::{
    command_in_path, copy_with_parent, create_temp_dir, output_is_fresh, outputs_are_fresh,
    remove_dir_if_exists, remove_file_if_exists, run_command,
};

mod cargo;

use cargo::{apply_kernel_cargo_env, run_cargo_kernel_check, run_cargo_kernel_rustc};

const DEFAULT_GRUB_DEV_KEY: &str = "RustOS Dev GRUB <rustos-dev-grub@example.invalid>";

struct GrubSigningMaterial {
    gpg_home: PathBuf,
    pubkey: PathBuf,
    signing_key: String,
}

pub(crate) fn build(config: &Config) -> Result<()> {
    validate_workspace_layering(&config.root_dir)?;
    let manifests = load_default_manifests(&config.root_dir)?;
    ensure_targets(config)?;
    let winsys_root = required_manifest(&manifests, "winsys")?
        .package_root
        .clone();
    validate_winsys_export_contracts(winsys_root.as_path())?;
    build_nucleus(config)?;
    build_efi(config)?;
    build_manifests_matching(config, &manifests, |manifest| {
        matches!(
            manifest.build.builder,
            BuilderKind::CargoKernelBinary | BuilderKind::MingwCExe
        )
    })?;
    build_manifests_matching(config, &manifests, |manifest| {
        matches!(manifest.build.builder, BuilderKind::WinsysDllBundle)
    })?;
    build_manifests_matching(config, &manifests, |manifest| {
        matches!(
            manifest.build.builder,
            BuilderKind::CDemo | BuilderKind::ModuleImage | BuilderKind::ExternalCopy
        )
    })?;
    stage::stage(config)?;
    Ok(())
}

pub(crate) fn check(config: &Config) -> Result<()> {
    validate_workspace_layering(&config.root_dir)?;
    let manifests = load_default_manifests(&config.root_dir)?;
    ensure_targets(config)?;

    run_cargo_kernel_check(config, &config.nucleus_package)?;
    check_nucleus_multiboot2_if_present(config)?;
    check_os_target_manifests(config, &manifests)?;
    check_host_workspace(config, &manifests)?;

    Ok(())
}

fn check_os_target_manifests(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut checked = BTreeSet::<String>::new();
    for manifest in manifests {
        let Some(package) = manifest.build.package.as_deref() else {
            continue;
        };
        if !checked.insert(package.to_owned()) {
            continue;
        }
        match manifest.build.builder {
            BuilderKind::CargoKernelBinary => check_cargo_os_binary(config, package)?,
            BuilderKind::ModuleImage => run_cargo_kernel_check(config, package)?,
            _ => {}
        }
    }

    Ok(())
}

fn check_cargo_os_binary(config: &Config, package: &str) -> Result<()> {
    let mut command = Command::new(&config.cargo);
    command
        .arg("check")
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target)
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    run_command(&mut command)
}

fn check_host_workspace(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let mut workspace_check = Command::new(&config.cargo);
    workspace_check
        .arg("check")
        .arg("--workspace")
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    for package in host_workspace_excludes(config, manifests) {
        workspace_check.arg("--exclude").arg(package);
    }
    run_command(&mut workspace_check)?;

    Ok(())
}

fn host_workspace_excludes(config: &Config, manifests: &[PackageManifest]) -> BTreeSet<String> {
    let mut excludes = BTreeSet::new();
    excludes.insert(config.nucleus_package.clone());
    for manifest in manifests {
        if matches!(
            manifest.build.builder,
            BuilderKind::CargoKernelBinary | BuilderKind::ModuleImage | BuilderKind::KernelRustc
        ) && let Some(package) = manifest.build.package.as_deref()
        {
            excludes.insert(package.to_owned());
        }
    }
    excludes
}

pub(crate) fn clean(config: &Config) -> Result<()> {
    let mut clean_target = Command::new(&config.cargo);
    clean_target
        .arg("clean")
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    run_command(&mut clean_target)?;

    let mut clean_manifest = Command::new(&config.cargo);
    clean_manifest
        .arg("clean")
        .arg("--manifest-path")
        .arg(&config.workspace_manifest)
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    run_command(&mut clean_manifest)?;
    remove_dir_if_exists(&config.build_dir)?;
    Ok(())
}

pub(crate) fn ensure_targets(config: &Config) -> Result<()> {
    let mut command = Command::new(&config.rustup);
    command.arg("target").arg("add").arg(&config.kernel_target);
    run_command(&mut command)
}

pub(crate) fn build_efi(config: &Config) -> Result<()> {
    let signing = ensure_grub_signing_material(config)?;

    sign_nucleus_with(config, &signing)?;

    let artifact = config.artifact_boot_efi_path();
    remove_file_if_exists(&artifact)?;
    let parent = artifact
        .parent()
        .with_context(|| format!("boot artifact has no parent: {}", artifact.display()))?;
    fs::create_dir_all(parent)?;

    let temp_dir = create_temp_dir("rustos-grub")?;
    let grub_cfg = temp_dir.join("grub.cfg");
    fs::write(
        &grub_cfg,
        "set check_signatures=enforce\nload_video\nset gfxmode=auto\nset gfxpayload=keep\nterminal_output gfxterm\nsearch --file --set=root /nucleus.elf\nmultiboot2 ($root)/nucleus.elf\nboot\n",
    )?;
    let grub_cfg_signature = temp_dir.join("grub.cfg.sig");
    sign_detached(config, &signing, &grub_cfg, &grub_cfg_signature)?;

    let mut command = Command::new(&config.grub_mkstandalone);
    command
        .arg("-O")
        .arg("x86_64-efi")
        .arg("-o")
        .arg(&artifact)
        .arg("--pubkey")
        .arg(&signing.pubkey)
        .arg("--modules")
        .arg("memdisk tar normal pgp gcry_rsa gcry_sha256 gcry_sha512 fat part_msdos part_gpt search search_fs_file ls multiboot2 all_video video video_fb efi_gop efi_uga gfxterm")
        .arg("--install-modules")
        .arg(config.rustos_grub_modules.as_deref().unwrap_or(
            "normal multiboot2 part_msdos part_gpt fat search search_fs_file ls all_video video video_fb efi_gop efi_uga gfxterm gcry_rsa gcry_sha256 gcry_sha512 pgp memdisk tar",
        ))
        .arg(format!("/boot/grub/grub.cfg={}", grub_cfg.display()))
        .arg(format!(
            "/boot/grub/grub.cfg.sig={}",
            grub_cfg_signature.display()
        ));
    if let Some(sbat) = config.rustos_grub_sbat.as_ref() {
        command.arg("--sbat").arg(sbat);
    }
    run_command(&mut command)
}

pub(crate) fn build_nucleus(config: &Config) -> Result<()> {
    run_cargo_kernel_rustc(config, &config.nucleus_package, &config.nucleus_rustc_args)?;
    let artifact = config.artifact_nucleus_elf_path();
    let source = config.nucleus_source_path();
    if !output_is_fresh(&artifact, std::slice::from_ref(&source))? {
        copy_with_parent(&source, &artifact)?;
    }
    check_nucleus_multiboot2(config)?;
    refresh_nucleus_signature_after_build(config)
}

fn check_nucleus_multiboot2_if_present(config: &Config) -> Result<()> {
    if config.artifact_nucleus_elf_path().is_file() {
        check_nucleus_multiboot2(config)?;
    }
    Ok(())
}

fn check_nucleus_multiboot2(config: &Config) -> Result<()> {
    let artifact = config.artifact_nucleus_elf_path();
    let status = Command::new(&config.grub_file)
        .arg("--is-x86-multiboot2")
        .arg(&artifact)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "nucleus artifact is not Multiboot2-compliant: {}",
            artifact.display()
        ))
    }
}

pub(crate) fn sign_nucleus(config: &Config) -> Result<()> {
    let signing = ensure_grub_signing_material(config)?;
    sign_nucleus_with(config, &signing)
}

fn sign_nucleus_with(config: &Config, signing: &GrubSigningMaterial) -> Result<()> {
    let nucleus = config.artifact_nucleus_elf_path();
    if !nucleus.is_file() {
        bail!("missing nucleus artifact: {}", nucleus.display());
    }
    sign_detached(
        config,
        signing,
        &nucleus,
        &config.artifact_nucleus_signature_path(),
    )
}

fn refresh_nucleus_signature_after_build(config: &Config) -> Result<()> {
    let nucleus = config.artifact_nucleus_elf_path();
    let signature = config.artifact_nucleus_signature_path();
    if config.rustos_grub_signing_key.is_some() {
        if !output_is_fresh(&signature, &[nucleus])? {
            sign_nucleus(config)?;
        }
    } else if signature.is_file() && !output_is_fresh(&signature, &[nucleus])? {
        remove_file_if_exists(&signature)?;
        eprintln!(
            "xtask: warning: removed stale nucleus signature; set RUSTOS_GRUB_SIGNING_KEY before staging a signed boot image"
        );
    }
    Ok(())
}

fn sign_detached(
    config: &Config,
    signing: &GrubSigningMaterial,
    input: &Path,
    signature: &Path,
) -> Result<()> {
    remove_file_if_exists(signature)?;

    let mut command = Command::new(&config.gpg);
    command
        .arg("--homedir")
        .arg(&signing.gpg_home)
        .arg("--batch")
        .arg("--yes")
        .arg("--pinentry-mode")
        .arg("loopback")
        .arg("--local-user")
        .arg(&signing.signing_key)
        .arg("--detach-sign")
        .arg("--output")
        .arg(signature)
        .arg(input);
    run_command(&mut command)
}

fn ensure_grub_signing_material(config: &Config) -> Result<GrubSigningMaterial> {
    let gpg_home = config
        .rustos_gpg_home
        .clone()
        .unwrap_or_else(|| config.build_dir.join("dev-grub-gpg"));
    let pubkey = config
        .rustos_grub_pubkey
        .clone()
        .unwrap_or_else(|| config.build_dir.join("dev-grub.pub"));
    let signing_key = config
        .rustos_grub_signing_key
        .clone()
        .unwrap_or_else(|| String::from(DEFAULT_GRUB_DEV_KEY));

    fs::create_dir_all(&gpg_home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gpg_home, std::fs::Permissions::from_mode(0o700))?;
    }

    if !gpg_secret_key_exists(config, &gpg_home, &signing_key)? {
        generate_grub_dev_key(config, &gpg_home, &signing_key)?;
    }

    export_grub_pubkey(config, &gpg_home, &signing_key, &pubkey)?;
    Ok(GrubSigningMaterial {
        gpg_home,
        pubkey,
        signing_key,
    })
}

fn gpg_secret_key_exists(config: &Config, gpg_home: &Path, signing_key: &str) -> Result<bool> {
    let status = Command::new(&config.gpg)
        .arg("--homedir")
        .arg(gpg_home)
        .arg("--batch")
        .arg("--list-secret-keys")
        .arg(signing_key)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

fn generate_grub_dev_key(config: &Config, gpg_home: &Path, signing_key: &str) -> Result<()> {
    let mut command = Command::new(&config.gpg);
    command
        .arg("--homedir")
        .arg(gpg_home)
        .arg("--batch")
        .arg("--passphrase")
        .arg("")
        .arg("--pinentry-mode")
        .arg("loopback")
        .arg("--quick-gen-key")
        .arg(signing_key)
        .arg("rsa2048")
        .arg("sign")
        .arg("0");
    run_command(&mut command).with_context(|| {
        format!(
            "failed to generate GRUB development signing key in {}",
            gpg_home.display()
        )
    })
}

fn export_grub_pubkey(
    config: &Config,
    gpg_home: &Path,
    signing_key: &str,
    pubkey: &Path,
) -> Result<()> {
    if let Some(parent) = pubkey.parent() {
        fs::create_dir_all(parent)?;
    }
    let output = Command::new(&config.gpg)
        .arg("--homedir")
        .arg(gpg_home)
        .arg("--batch")
        .arg("--export")
        .arg(signing_key)
        .output()?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.is_empty() {
            eprint!("{stdout}");
        }
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
        bail!(
            "failed to export GRUB public key with status {}",
            output.status
        );
    }
    fs::write(pubkey, output.stdout)?;
    Ok(())
}

pub(crate) fn build_user(config: &Config) -> Result<()> {
    let manifests = load_default_manifests(&config.root_dir)?;
    build_manifests_matching(config, &manifests, |manifest| {
        matches!(
            manifest.build.builder,
            BuilderKind::CargoKernelBinary | BuilderKind::MingwCExe
        )
    })
}

pub(crate) fn build_console_demo(config: &Config) -> Result<()> {
    let manifests = load_default_manifests(&config.root_dir)?;
    build_manifests_matching(config, &manifests, |manifest| {
        matches!(manifest.build.builder, BuilderKind::CDemo)
    })
}

fn build_windows_system_dlls(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let artifact_dir = manifest.artifact_path(config);
    let winsys_dir = &manifest.package_root;

    fs::create_dir_all(&artifact_dir)?;
    remove_dir_if_exists(&artifact_dir.join(".importlibs"))?;
    let import_lib_dir = config.build_dir.join("intermediates/winsys-importlibs");
    fs::create_dir_all(&import_lib_dir)?;

    for (index, spec) in winsys_dll_specs().iter().enumerate() {
        let mut sources = winsys_c_sources(&winsys_dir.join(spec.dir))?;
        let exports = winsys_dir.join(spec.dir).join("exports.def");
        let output = artifact_dir.join(spec.file_name);
        let import_lib = import_lib_dir.join(format!("{}.a", spec.file_name));
        for shared_source in spec.shared_sources {
            sources.push(winsys_dir.join(shared_source));
        }
        let mut inputs = sources.clone();
        inputs.push(exports.clone());
        inputs.push(manifest.manifest_path.clone());
        inputs.extend(winsys_headers(&winsys_dir.join(spec.dir))?);
        inputs.extend(winsys_headers(&winsys_dir.join("common"))?);
        if winsys_dll_needs_ntdll(spec.file_name) {
            inputs.push(import_lib_dir.join("ntdll.dll.a"));
        }

        if outputs_are_fresh(&[output.clone(), import_lib.clone()], &inputs)? {
            continue;
        }

        remove_file_if_exists(&output)?;
        remove_file_if_exists(&import_lib)?;

        let image_base = 0x7000_0000_u64 + (index as u64) * 0x0010_0000_u64;
        let mut command = Command::new(&config.mingw_cc);
        command
            .arg("-shared")
            .arg("-nostdlib")
            .arg("-ffreestanding")
            .arg("-fno-builtin")
            .arg("-fno-stack-protector")
            .arg("-I")
            .arg(winsys_dir.join("common"))
            .arg("-Wl,--entry,DllMain")
            .arg(format!("-Wl,--image-base=0x{image_base:x}"))
            .arg(format!("-Wl,--out-implib={}", import_lib.display()))
            .arg("-o")
            .arg(&output);
        for source in &sources {
            command.arg(source);
        }
        command.arg(&exports);
        if winsys_dll_needs_ntdll(spec.file_name) {
            command.arg(import_lib_dir.join("ntdll.dll.a"));
        }
        run_command(&mut command)?;
    }

    Ok(())
}

fn winsys_headers(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("h") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn winsys_dll_needs_ntdll(file_name: &str) -> bool {
    matches!(
        file_name,
        "kernelbase.dll"
            | "msvcrt.dll"
            | "ucrtbase.dll"
            | "vcruntime140.dll"
            | "vcruntime140_1.dll"
    )
}

fn winsys_c_sources(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut sources = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("c") {
            sources.push(path);
        }
    }
    sources.sort();
    if sources.is_empty() {
        bail!("no C sources found in {}", dir.display());
    }
    Ok(sources)
}

pub(crate) fn build_driver_modules(config: &Config) -> Result<()> {
    let manifests = load_default_manifests(&config.root_dir)?;
    build_manifests_matching(config, &manifests, |manifest| {
        matches!(
            manifest.build.builder,
            BuilderKind::ModuleImage | BuilderKind::ExternalCopy
        )
    })
}

fn build_manifests_matching(
    config: &Config,
    manifests: &[PackageManifest],
    filter: impl Fn(&PackageManifest) -> bool,
) -> Result<()> {
    for manifest in manifests.iter().filter(|manifest| filter(manifest)) {
        build_manifest(config, manifest)?;
    }
    Ok(())
}

fn build_manifest(config: &Config, manifest: &PackageManifest) -> Result<()> {
    match manifest.build.builder {
        BuilderKind::BootloaderUefi => build_efi(config),
        BuilderKind::KernelRustc => build_nucleus(config),
        BuilderKind::CargoKernelBinary => build_cargo_kernel_binary(config, manifest),
        BuilderKind::MingwCExe => build_mingw_c_exe(config, manifest),
        BuilderKind::CDemo => build_c_demo_manifest(config, manifest),
        BuilderKind::ModuleImage => build_module_image_manifest(config, manifest),
        BuilderKind::WinsysDllBundle => build_windows_system_dlls(config, manifest),
        BuilderKind::ExternalCopy => build_external_copy_manifest(config, manifest),
    }
}

fn build_cargo_kernel_binary(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let linkage = manifest
        .build
        .linkage
        .as_deref()
        .unwrap_or(config.user_elf_linkage.as_str());

    match linkage {
        "dynamic" => build_cargo_kernel_binary_dynamic(config, manifest),
        "static-pie" => build_cargo_kernel_binary_static_pie(config, manifest),
        other => bail!(
            "package {} has unsupported build.linkage={:?}",
            manifest.id,
            other
        ),
    }
}

fn build_cargo_kernel_binary_dynamic(config: &Config, manifest: &PackageManifest) -> Result<()> {
    if config.user_elf_linkage != "dynamic" {
        bail!(
            "Rust std userspace currently supports only USER_ELF_LINKAGE=dynamic, got {}",
            config.user_elf_linkage
        );
    }

    let package = manifest
        .build
        .package
        .as_deref()
        .with_context(|| format!("package {} missing build.package", manifest.id))?;
    let mut command = Command::new(&config.cargo);
    command
        .arg("build")
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target)
        .arg("--release")
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    run_command(&mut command)?;

    let binary = config
        .cargo_target_dir
        .join(format!("{}/release/{package}", config.kernel_target));
    let artifact = manifest.artifact_path(config);
    if output_is_fresh(&artifact, &[binary.clone()])? {
        return Ok(());
    }
    copy_with_parent(&binary, &artifact)
}

/// Build a `no_std` static-PIE service binary that has no `PT_INTERP` and no
/// libc dependency. Used by foundation policy services (syscalld, vfsd) so
/// that bringing them up does not require the dynamic Linux runtime they are
/// supposed to provide — the seL4 root-task pattern. See
/// [`libs/rustos-svc-runtime`](../../../../libs/rustos-svc-runtime/src/lib.rs).
fn build_cargo_kernel_binary_static_pie(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let package = manifest
        .build
        .package
        .as_deref()
        .with_context(|| format!("package {} missing build.package", manifest.id))?;

    // Link our own _start (provided by rustos-svc-runtime via the `entry!`
    // macro) and produce a static PIE with no interpreter. `-no-pie` is NOT
    // used — we want PIE so the kernel can choose a load bias.
    let rustflags = concat!(
        "-C target-feature=+crt-static ",
        "-C panic=abort ",
        "-C relocation-model=pic ",
        "-C link-arg=-nostartfiles ",
        "-C link-arg=-static-pie ",
        "-C link-arg=-Wl,--no-dynamic-linker"
    );

    let mut command = Command::new(&config.cargo);
    command
        .arg("build")
        .args(&config.kernel_cargo_zflags)
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target)
        .arg("--release")
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir)
        .env("RUSTFLAGS", rustflags);
    run_command(&mut command)?;

    let binary = config
        .cargo_target_dir
        .join(format!("{}/release/{package}", config.kernel_target));
    let artifact = manifest.artifact_path(config);
    if output_is_fresh(&artifact, &[binary.clone()])? {
        return Ok(());
    }
    copy_with_parent(&binary, &artifact)
}

fn build_mingw_c_exe(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let source = manifest
        .resolved_source_path()
        .with_context(|| format!("package {} missing build.source", manifest.id))?;
    let output = manifest.artifact_path(config);
    let parent = output.parent().with_context(|| {
        format!(
            "Windows executable path has no parent: {}",
            output.display()
        )
    })?;
    fs::create_dir_all(parent)?;

    if output_is_fresh(&output, &[source.clone(), manifest.manifest_path.clone()])? {
        return Ok(());
    }

    remove_file_if_exists(&output)?;

    run_command(
        Command::new(&config.mingw_cc)
            .arg("-Os")
            .arg("-s")
            .arg("-Wl,--image-base,0x400000")
            .arg("-o")
            .arg(&output)
            .arg(&source),
    )?;
    if manifest.id == "userdemo2" {
        let winsys_root = required_manifest(&load_default_manifests(&config.root_dir)?, "winsys")?
            .package_root
            .clone();
        audit_userdemo2_imports_for_path(config, winsys_root.as_path(), &output)?;
    }
    Ok(())
}

fn build_c_demo_manifest(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let source = manifest
        .resolved_source_path()
        .with_context(|| format!("package {} missing build.source", manifest.id))?;
    let extra_args = manifest
        .build
        .extra_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let output = manifest.artifact_path(config);
    if output_is_fresh(&output, &[source.clone(), manifest.manifest_path.clone()])? {
        return Ok(());
    }
    build_c_demo(&config.cc, &source, &output, &extra_args)
}

fn build_module_image_manifest(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let package = manifest
        .build
        .package
        .as_deref()
        .with_context(|| format!("package {} missing build.package", manifest.id))?;
    let crate_name = manifest
        .build
        .crate_name
        .as_deref()
        .with_context(|| format!("package {} missing build.crate_name", manifest.id))?;
    let dependency_crates = manifest
        .build
        .dependency_crates
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    build_rust_module_image(
        config,
        package,
        crate_name,
        &manifest.artifact_path(config),
        &dependency_crates,
        &[manifest.manifest_path.clone()],
    )
}

fn build_external_copy_manifest(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let source = resolve_external_copy_source(config, manifest);
    let artifact = manifest.artifact_path(config);
    if let Some(source) = source.filter(|path| path.is_file()) {
        if manifest.build.source_env.is_none() {
            let inputs = vec![source.clone(), manifest.manifest_path.clone()];
            if output_is_fresh(&artifact, &inputs)? {
                return Ok(());
            }
        }
        remove_file_if_exists(&artifact)?;
        copy_external_copy_source(&source, &artifact)?;
    } else if !manifest.build.optional {
        bail!(
            "required external package {} is missing source file",
            manifest.id
        );
    } else {
        eprintln!(
            "xtask: warning: optional vendor module not found: {}",
            manifest.id
        );
    }
    Ok(())
}

fn copy_external_copy_source(source: &Path, artifact: &Path) -> Result<()> {
    if source.extension().and_then(|ext| ext.to_str()) != Some("zst") {
        return copy_with_parent(source, artifact);
    }
    let parent = artifact
        .parent()
        .with_context(|| format!("artifact destination has no parent: {}", artifact.display()))?;
    fs::create_dir_all(parent)?;
    let zstd = command_in_path("zstd").context("missing zstd to unpack external .zst artifact")?;
    let output = File::create(artifact)
        .with_context(|| format!("failed to create {}", artifact.display()))?;
    let status = Command::new(zstd)
        .arg("-dc")
        .arg(source)
        .stdout(Stdio::from(output))
        .status()
        .with_context(|| format!("failed to run zstd for {}", source.display()))?;
    if !status.success() {
        bail!("failed to unpack {}", source.display());
    }
    Ok(())
}

fn resolve_external_copy_source(config: &Config, manifest: &PackageManifest) -> Option<PathBuf> {
    manifest
        .build
        .source_env
        .as_deref()
        .and_then(crate::util::env_path)
        .or_else(|| {
            manifest
                .build
                .source
                .as_ref()
                .map(|path| config.root_dir.join(path))
        })
}

fn build_rust_module_image(
    config: &Config,
    package: &str,
    crate_name: &str,
    output: &Path,
    dependency_crates: &[&str],
    extra_inputs: &[PathBuf],
) -> Result<()> {
    let output_parent = output
        .parent()
        .with_context(|| format!("module artifact path has no parent: {}", output.display()))?;
    fs::create_dir_all(output_parent)?;

    let deps_dir = config.kernel_release_deps_dir();

    let mut command = Command::new(&config.cargo);
    apply_kernel_cargo_env(config, &mut command);
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
    let mut archive_paths = vec![self_archive.clone()];
    for dependency in &archives {
        archive_paths.push(find_latest_rlib_artifact(
            &deps_dir,
            &format!("lib{dependency}-"),
        )?);
    }
    let mut freshness_inputs = archive_paths.clone();
    freshness_inputs.extend(extra_inputs.iter().cloned());
    if output_is_fresh(output, &freshness_inputs)? {
        return Ok(());
    }

    remove_file_if_exists(output)?;
    let ar_bin = command_in_path("llvm-ar")
        .or_else(|| command_in_path("ar"))
        .context("missing llvm-ar/ar for module archive extraction")?;
    let temp_dir = create_temp_dir("rustos-module-link")?;
    let mut link_inputs = Vec::new();
    extract_archive_objects(
        &ar_bin,
        &self_archive,
        &temp_dir.join(crate_name),
        &mut link_inputs,
    )?;

    for (dependency, archive) in archives.iter().zip(archive_paths.iter().skip(1)) {
        extract_archive_objects(
            &ar_bin,
            archive,
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

fn build_c_demo(
    cc: &std::ffi::OsString,
    source: &Path,
    output: &Path,
    extra_args: &[&str],
) -> Result<()> {
    let parent = output
        .parent()
        .with_context(|| format!("demo artifact path has no parent: {}", output.display()))?;
    fs::create_dir_all(parent)?;

    let mut command = Command::new(cc);
    command.arg(source);
    for arg in extra_args {
        command.arg(arg);
    }
    command.arg("-o").arg(output);
    run_command(&mut command)
}

fn audit_userdemo2_imports_for_path(
    config: &Config,
    winsys_root: &Path,
    executable: &Path,
) -> Result<()> {
    let output = Command::new(&config.objdump)
        .arg("-p")
        .arg(executable)
        .output()?;
    if !output.status.success() {
        bail!("objdump failed for {}", executable.display());
    }

    fs::write(config.userdemo2_import_audit_log_path(), &output.stdout)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let imports = parse_objdump_imports(text.as_ref());
    validate_imports_exported_by_winsys(winsys_root, &imports)?;
    Ok(())
}

fn validate_imports_exported_by_winsys(
    winsys_root: &Path,
    imports: &[(String, String)],
) -> Result<()> {
    let exports_by_dll = winsys_import_contracts(winsys_root)?;

    let mut missing = Vec::new();
    for (dll, symbol) in imports {
        let Some(exports) = exports_by_dll.get(dll.as_str()) else {
            missing.push((dll.clone(), symbol.clone()));
            continue;
        };
        if !exports
            .iter()
            .any(|export| export.name.eq_ignore_ascii_case(symbol))
        {
            missing.push((dll.clone(), symbol.clone()));
        }
    }

    if !missing.is_empty() {
        let details = missing
            .into_iter()
            .map(|(dll, symbol)| format!("{dll}!{symbol}"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("winsys export contract mismatch: {}", details);
    }

    Ok(())
}

fn parse_objdump_imports(text: &str) -> Vec<(String, String)> {
    let mut imports = Vec::new();
    let mut current_dll = None::<String>;
    let mut in_import_tables = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "The Import Tables (interpreted .idata section contents)" {
            in_import_tables = true;
            current_dll = None;
            continue;
        }
        if !in_import_tables {
            continue;
        }
        if trimmed.starts_with("The ")
            || trimmed.starts_with("PE File Base Relocations")
            || trimmed.starts_with("Dump of ")
        {
            break;
        }
        if let Some(name) = trimmed.strip_prefix("DLL Name: ") {
            current_dll = Some(name.to_ascii_lowercase());
            continue;
        }

        let Some(dll) = current_dll.as_ref() else {
            continue;
        };
        if trimmed.is_empty()
            || trimmed.starts_with("vma:")
            || trimmed.starts_with("The Function Table")
            || trimmed.starts_with("Entry ")
        {
            continue;
        }
        if !trimmed
            .bytes()
            .next()
            .map(|byte| byte.is_ascii_hexdigit())
            .unwrap_or(false)
        {
            continue;
        }

        let parts = trimmed.split_whitespace().collect::<Vec<_>>();
        if let Some(symbol) = parts.last().copied() {
            if symbol.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            imports.push((dll.clone(), String::from(symbol)));
        }
    }

    imports
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
        .with_context(|| anyhow!("module rlib artifact not found under {}", dir.display()))
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

struct WinsysDllSpec {
    dir: &'static str,
    file_name: &'static str,
    shared_sources: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DefExport {
    name: String,
    forward_dll: Option<String>,
    forward_symbol: Option<String>,
}

const fn winsys_dll_specs() -> &'static [WinsysDllSpec] {
    &[
        WinsysDllSpec {
            dir: "ntdll",
            file_name: "ntdll.dll",
            shared_sources: &[],
        },
        WinsysDllSpec {
            dir: "kernelbase",
            file_name: "kernelbase.dll",
            shared_sources: &[],
        },
        WinsysDllSpec {
            dir: "kernel32",
            file_name: "kernel32.dll",
            shared_sources: &[],
        },
        WinsysDllSpec {
            dir: "msvcrt",
            file_name: "msvcrt.dll",
            shared_sources: &["common/msvcrt_impl.c"],
        },
        WinsysDllSpec {
            dir: "ucrtbase",
            file_name: "ucrtbase.dll",
            shared_sources: &["common/msvcrt_impl.c", "common/ucrtbase_impl.c"],
        },
        WinsysDllSpec {
            dir: "vcruntime140",
            file_name: "vcruntime140.dll",
            shared_sources: &["common/vcruntime_impl.c"],
        },
        WinsysDllSpec {
            dir: "vcruntime140_1",
            file_name: "vcruntime140_1.dll",
            shared_sources: &["common/vcruntime_impl.c"],
        },
    ]
}

fn winsys_import_contracts(winsys_root: &Path) -> Result<BTreeMap<String, Vec<DefExport>>> {
    let mut exports_by_dll = BTreeMap::<String, Vec<DefExport>>::new();
    for spec in winsys_dll_specs() {
        let exports_path = winsys_root.join(spec.dir).join("exports.def");
        exports_by_dll.insert(
            spec.file_name.to_ascii_lowercase(),
            parse_def_exports(&exports_path)?,
        );
    }

    for (alias, target) in winsys_import_aliases() {
        let target = target.to_ascii_lowercase();
        let Some(target_exports) = exports_by_dll.get(target.as_str()).cloned() else {
            bail!(
                "winsys import alias error: {} maps to missing DLL {}",
                alias,
                target
            );
        };
        exports_by_dll.insert((*alias).to_ascii_lowercase(), target_exports);
    }

    Ok(exports_by_dll)
}

const fn winsys_import_aliases() -> &'static [(&'static str, &'static str)] {
    &[
        ("api-ms-win-core-console-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-errorhandling-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-file-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-handle-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-heap-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-libraryloader-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-libraryloader-l1-2-0.dll", "kernel32.dll"),
        ("api-ms-win-core-memory-l1-1-0.dll", "kernel32.dll"),
        (
            "api-ms-win-core-processenvironment-l1-1-0.dll",
            "kernel32.dll",
        ),
        ("api-ms-win-core-processthreads-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-string-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-synch-l1-1-0.dll", "kernel32.dll"),
        ("api-ms-win-core-synch-l1-2-0.dll", "kernel32.dll"),
        ("api-ms-win-crt-convert-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-environment-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-heap-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-locale-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-math-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-stdio-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-string-l1-1-0.dll", "ucrtbase.dll"),
        ("api-ms-win-crt-utility-l1-1-0.dll", "ucrtbase.dll"),
    ]
}

fn validate_winsys_export_contracts(winsys_root: &Path) -> Result<()> {
    let exports_by_dll = winsys_import_contracts(winsys_root)?;

    for (dll_name, exports) in &exports_by_dll {
        if winsys_import_aliases()
            .iter()
            .any(|(alias, _)| dll_name.eq_ignore_ascii_case(alias))
        {
            continue;
        }
        for export in exports {
            let (Some(target_dll), Some(target_symbol)) =
                (&export.forward_dll, &export.forward_symbol)
            else {
                continue;
            };
            let Some(target_exports) = exports_by_dll.get(target_dll) else {
                bail!(
                    "winsys export contract error: {}!{} forwards to missing DLL {}",
                    dll_name,
                    export.name,
                    target_dll
                );
            };
            if !target_exports
                .iter()
                .any(|candidate| candidate.name.eq_ignore_ascii_case(target_symbol))
            {
                bail!(
                    "winsys export contract error: {}!{} forwards to missing export {}!{}",
                    dll_name,
                    export.name,
                    target_dll,
                    target_symbol
                );
            }
        }
    }

    Ok(())
}

fn parse_def_exports(path: &Path) -> Result<Vec<DefExport>> {
    let text = fs::read_to_string(path)?;
    let mut in_exports = false;
    let mut exports = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("EXPORTS") {
            in_exports = true;
            continue;
        }
        if !in_exports || trimmed.to_ascii_uppercase().starts_with("LIBRARY ") {
            continue;
        }

        let token = trimmed
            .split_whitespace()
            .next()
            .with_context(|| format!("invalid DEF export line in {}: {trimmed}", path.display()))?;
        let (name, forward_dll, forward_symbol) =
            if let Some((name, target)) = token.split_once('=') {
                let (dll, symbol) = target.rsplit_once('.').with_context(|| {
                    format!(
                        "invalid DEF forwarder target in {}: {}",
                        path.display(),
                        trimmed
                    )
                })?;
                (
                    String::from(name),
                    Some(canonical_forward_dll_name(dll)),
                    Some(String::from(symbol)),
                )
            } else {
                (String::from(token), None, None)
            };
        exports.push(DefExport {
            name,
            forward_dll,
            forward_symbol,
        });
    }

    Ok(exports)
}

fn canonical_forward_dll_name(name: &str) -> String {
    if name.to_ascii_lowercase().ends_with(".dll") {
        name.to_ascii_lowercase()
    } else {
        format!("{}.dll", name.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_forward_dll_name, parse_def_exports, parse_objdump_imports};
    use fs_err as fs;

    #[test]
    fn parse_objdump_imports_stops_after_import_tables() {
        let text = r#"
The Import Tables (interpreted .idata section contents)
 vma:            Hint    Time      Forward  DLL       First
                 Table   Stamp     Chain    Name      Thunk
 0000d000	0000d040 00000000 00000000 0000d62c 0000d1c8

	DLL Name: KERNEL32.dll
	vma:  Hint/Ord Member-Name Bound-To
	d350	  281  DeleteCriticalSection
	d368	  317  EnterCriticalSection

 0000d014	0000d0b0 00000000 00000000 0000d6c4 0000d238

	DLL Name: msvcrt.dll
	vma:  Hint/Ord Member-Name Bound-To
	d568	  953  fprintf
	d572	  955  fputc

The Function Table (interpreted .pdata section contents)
vma:			BeginAddress	 EndAddress	  UnwindData
 000000000040a000:	0000000000401000 0000000000401001 000000000040b000
"#;

        let imports = parse_objdump_imports(text);
        assert_eq!(
            imports,
            vec![
                (
                    String::from("kernel32.dll"),
                    String::from("DeleteCriticalSection"),
                ),
                (
                    String::from("kernel32.dll"),
                    String::from("EnterCriticalSection"),
                ),
                (String::from("msvcrt.dll"), String::from("fprintf")),
                (String::from("msvcrt.dll"), String::from("fputc")),
            ]
        );
    }

    #[test]
    fn parse_objdump_imports_ignores_text_before_import_tables() {
        let text = r#"
There is an import table in .idata at 0x40d000
 000000000040a000:	0000000000401000 0000000000401001 000000000040b000
The Import Tables (interpreted .idata section contents)

	DLL Name: KERNEL32.dll
	vma:  Hint/Ord Member-Name Bound-To
	d40c	 1407  Sleep
"#;

        let imports = parse_objdump_imports(text);
        assert_eq!(
            imports,
            vec![(String::from("kernel32.dll"), String::from("Sleep"))]
        );
    }

    #[test]
    fn parse_def_exports_recognizes_forwarders() {
        let temp_dir =
            std::env::temp_dir().join(format!("rustos-xtask-def-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("exports.def");
        fs::write(
            &path,
            "LIBRARY kernel32.dll\nEXPORTS\n    GetProcAddress=KERNELBASE.GetProcAddress\n    Sleep\n",
        )
        .unwrap();

        let exports = parse_def_exports(&path).unwrap();
        assert_eq!(exports.len(), 2);
        assert_eq!(exports[0].name, "GetProcAddress");
        assert_eq!(exports[0].forward_dll.as_deref(), Some("kernelbase.dll"));
        assert_eq!(exports[0].forward_symbol.as_deref(), Some("GetProcAddress"));
        assert_eq!(exports[1].name, "Sleep");
        assert_eq!(exports[1].forward_dll, None);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn canonical_forward_dll_name_adds_missing_suffix() {
        assert_eq!(canonical_forward_dll_name("KERNELBASE"), "kernelbase.dll");
        assert_eq!(
            canonical_forward_dll_name("vcruntime140.dll"),
            "vcruntime140.dll"
        );
    }
}
