use anyhow::{Context, anyhow, bail};
use fs_err as fs;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::Result;
use crate::config::Config;
use crate::layering::validate_workspace_layering;
use crate::package_manifest::{BuilderKind, PackageManifest, load_manifests, required_manifest};
use crate::stage;
use crate::util::{
    copy_with_parent, create_temp_dir, output_is_fresh, outputs_are_fresh, remove_dir_if_exists,
    remove_file_if_exists, run_command, sync_directory,
};

mod cargo;
mod clean;
mod nucleus_audit;

pub(crate) use clean::clean;

use cargo::{apply_owned_target_rustflags, run_cargo_kernel_check, run_cargo_kernel_rustc};
use nucleus_audit::{
    audit_simd_custody, check_nucleus_multiboot2, check_nucleus_multiboot2_if_present,
    prepare_self_relocating_multiboot_elf,
};

const DEFAULT_GRUB_DEV_KEY: &str = "RustOS Dev GRUB <rustos-dev-grub@example.invalid>";

struct GrubSigningMaterial {
    gpg_home: PathBuf,
    pubkey: PathBuf,
    signing_key: String,
}

pub(crate) fn build(config: &Config, show_timings: bool) -> Result<()> {
    let mut timings = StepTimings::new(show_timings);
    validate_workspace_layering(&config.root_dir)?;
    timings.mark("layering");
    crate::abi_drift::check_abi_drift(&config.root_dir)?;
    timings.mark("abi-drift");
    crate::native_abi::check_native_abi_stamps(config)?;
    timings.mark("native-abi-stamps");
    let manifests = load_manifests(&config.root_dir)?;
    timings.mark("manifests");
    validate_dvm_gpu_contract(config)?;
    timings.mark("dvm-gpu-contract");
    ensure_targets(config)?;
    timings.mark("targets");
    build_nucleus(config)?;
    timings.mark("nucleus");
    build_efi(config)?;
    timings.mark("efi");
    build_userspace_manifests(config, &manifests)?;
    timings.mark("userspace");
    stage::stage(config)?;
    timings.mark("stage");
    timings.report("build");
    Ok(())
}

fn validate_dvm_gpu_contract(config: &Config) -> Result<()> {
    let source_root = config
        .root_dir
        .join("driver-domains/linux/package/rustos-dvm-display/src");
    let agent_path = config
        .root_dir
        .join("driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c");
    let relay_path = source_root.join("rustos-dvm-display.c");
    let probe_path = source_root.join("rustos-dvm-gpu-probe.c");
    let module_path = source_root.join("rustos_dvm_ivshmem_uio.c");
    let runtime_path = source_root.join("rustos-dvm-gpu-runtime.c");
    let runtime_header_path = source_root.join("rustos-dvm-gpu-runtime.h");
    let backend_header_path = source_root.join("rustos-dvm-gpu-backends.h");
    let relay = fs::read_to_string(&relay_path)
        .with_context(|| format!("read DVM GPU relay contract {}", relay_path.display()))?;
    let agent = fs::read_to_string(&agent_path)
        .with_context(|| format!("read DVM agent display contract {}", agent_path.display()))?;
    let module = fs::read_to_string(&module_path)
        .with_context(|| format!("read DVM GPU module contract {}", module_path.display()))?;
    let probe = fs::read_to_string(&probe_path)
        .with_context(|| format!("read DVM GPU proof contract {}", probe_path.display()))?;
    let backend_header = fs::read_to_string(&backend_header_path).with_context(|| {
        format!(
            "read DVM GPU backend registry {}",
            backend_header_path.display()
        )
    })?;
    let runtime = fs::read_to_string(&runtime_path)
        .with_context(|| format!("read DVM GPU runtime contract {}", runtime_path.display()))?;
    let runtime_header = fs::read_to_string(&runtime_header_path).with_context(|| {
        format!(
            "read DVM GPU runtime header contract {}",
            runtime_header_path.display()
        )
    })?;
    for token in [
        "\"DISPLAY_RELAY_SCHEMA=2\\n\"",
        "\"STATE=ready\\n\"",
        "\"MODE=gpu-compositor-staged-copy\\n\"",
        "\"ZERO_COPY=0\\n\"",
        "\"GPU_COMPOSITION=1\\n\"",
        "\"EXPLICIT_FENCE=1\\n\";",
    ] {
        require_contract_token(&relay, &relay_path, token)?;
        require_contract_token(&agent, &agent_path, token)?;
    }
    let version = driver_domain_protocol::DVM_GPU_ATLAS_TRANSPORT_VERSION;
    require_contract_token(
        &relay,
        &relay_path,
        &format!("#define GPU_ATLAS_VERSION {version}U"),
    )?;
    require_contract_token(
        &module,
        &module_path,
        &format!("#define RUSTOS_GPU_ATLAS_VERSION {version}"),
    )?;
    let prime_version = driver_domain_protocol::DVM_GPU_PRIME_COMPLETION_VERSION;
    require_contract_token(
        &relay,
        &relay_path,
        &format!("#define GPU_PRIME_COMPLETION_VERSION {prime_version}U"),
    )?;
    require_contract_token(
        &module,
        &module_path,
        &format!("#define RUSTOS_GPU_PRIME_COMPLETION_VERSION {prime_version}"),
    )?;
    for (relay_name, module_name, value) in [
        (
            "GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY",
            "RUSTOS_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY",
            driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
        ),
        (
            "GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF",
            "RUSTOS_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF",
            driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF,
        ),
    ] {
        require_contract_token(
            &relay,
            &relay_path,
            &format!("#define {relay_name} {value}U"),
        )?;
        require_contract_token(
            &module,
            &module_path,
            &format!("#define {module_name} {value}"),
        )?;
    }
    for token in [
        "{\"virtio_gpu\", RUSTOS_GPU_BACKEND_VIRTUAL_STAGED,",
        "{\"amdgpu\", RUSTOS_GPU_BACKEND_PHYSICAL_DIRECT,",
        "#define RUSTOS_GPU_SOURCE_STAGED_COPY 1U",
        "#define RUSTOS_GPU_SOURCE_DIRECT_DMABUF 2U",
    ] {
        require_contract_token(&backend_header, &backend_header_path, token)?;
    }
    for token in [
        "#define GPU_PRIME_EVIDENCE_MAX_BYTES 1024U",
        "#define GPU_EVIDENCE_MAX_BYTES 2048U",
        "char evidence[GPU_PRIME_EVIDENCE_MAX_BYTES];",
        "char evidence[GPU_EVIDENCE_MAX_BYTES];",
    ] {
        require_contract_token(&probe, &probe_path, token)?;
    }
    for (source, path) in [(&relay, &relay_path), (&probe, &probe_path)] {
        require_contract_token(source, path, "#include \"rustos-dvm-gpu-backends.h\"")?;
    }
    for (name, magic) in [
        (
            "GPU_RENDER_COMPLETION_MAGIC",
            driver_domain_protocol::DVM_GPU_RENDER_COMPLETION_MAGIC,
        ),
        (
            "GPU_PRIME_COMPLETION_MAGIC",
            driver_domain_protocol::DVM_GPU_PRIME_COMPLETION_MAGIC,
        ),
        (
            "GPU_PRESENT_COMPLETION_MAGIC",
            driver_domain_protocol::DVM_GPU_PRESENT_COMPLETION_MAGIC,
        ),
    ] {
        let magic = core::str::from_utf8(&magic)
            .map_err(|_| anyhow!("Rust DVM GPU magic {name} is not ASCII"))?;
        require_contract_token(&relay, &relay_path, &format!("#define {name} \"{magic}\""))?;
        let module_name = format!("rustos_{}", name.to_ascii_lowercase());
        require_contract_token(
            &module,
            &module_path,
            &format!("static const u8 {module_name}[] = \"{magic}\";"),
        )?;
    }
    for (name, value) in [
        (
            "RUSTOS_GPU_ATLAS_PRIME_COMPLETION_OFFSET",
            driver_domain_protocol::DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET,
        ),
        (
            "RUSTOS_GPU_ATLAS_CONTEXT_ID_OFFSET",
            driver_domain_protocol::DVM_GPU_ATLAS_CONTEXT_ID_OFFSET,
        ),
        (
            "RUSTOS_GPU_ATLAS_CONTEXT_EPOCH_OFFSET",
            driver_domain_protocol::DVM_GPU_ATLAS_CONTEXT_EPOCH_OFFSET,
        ),
        (
            "RUSTOS_GPU_ATLAS_PRIME_FENCE_OFFSET",
            driver_domain_protocol::DVM_GPU_ATLAS_PRIME_FENCE_OFFSET,
        ),
    ] {
        require_contract_token(&module, &module_path, &format!("#define {name} {value}"))?;
        let relay_name = name.strip_prefix("RUSTOS_").unwrap_or(name);
        require_contract_token(
            &relay,
            &relay_path,
            &format!("#define {relay_name} {value}U"),
        )?;
    }
    for token in [
        "#define GPU_PROOF_RR_PRIORITY 8",
        "#define GPU_PROOF_RTTIME_SOFT_US 50000U",
        "#define GPU_PROOF_RTTIME_HARD_US 100000U",
        "\"PROOF_RTTIME_HARD_ACTION=terminate\\n\"",
        "\"PROOF_SCHEDULER_RESTORED=normal\\n\"",
    ] {
        require_contract_token(&probe, &probe_path, token)?;
    }
    for (contents, path) in [
        (&relay, &relay_path),
        (&probe, &probe_path),
        (&agent, &agent_path),
    ] {
        require_contract_token(
            contents,
            path,
            "if (guard->saved_policy != SCHED_OTHER || guard->saved_param.sched_priority != 0) {",
        )?;
        require_contract_token(
            contents,
            path,
            "observed_rttime.rlim_cur != guard->saved_rttime.rlim_cur ||",
        )?;
    }
    require_contract_token(
        &relay,
        &relay_path,
        "return scheduler.fatal ? DISPLAY_SERVE_FATAL : DISPLAY_SERVE_RETRY;",
    )?;
    for token in [
        "#define RUSTOS_DVM_DISPLAY_OWNER_NAME \"display-owner.lock\"",
        "#define RUSTOS_DVM_DISPLAY_READY_CANDIDATE \".display-ready.next\"",
        "#define RUSTOS_DVM_DMABUF_DEVICE \"/dev/rustos-dvm-display-dmabuf\"",
        "owner_fd = claim_display_process_owner();",
        "candidate_crtc = select_connector_crtc(fd, resources, connector);",
        "exporter = open(RUSTOS_DVM_DMABUF_DEVICE,",
        "if (rustos_gpu_runtime_import_dmabuf_sources(",
        "acquire_fence_fd = acquire_gpu_source_fence(&display, &submission);",
        "const int dmabuf_sources = rustos_gpu_runtime_uses_dmabuf_sources(runtime);",
        "pollfds[2].fd = frame->in_fence_fd;",
        "\"MODE=gpu-compositor-dmabuf-source\\n\"",
        "\"ATOMIC_KMS_SCANOUT=1\\n\"",
        "\"STAGED_DAMAGE_COPY=0\\n\"",
        "renameat(directory_fd, RUSTOS_DVM_DISPLAY_READY_CANDIDATE, directory_fd,",
    ] {
        require_contract_token(&relay, &relay_path, token)?;
    }
    require_contract_token(
        &runtime,
        &runtime_path,
        "runtime->egl_display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL,",
    )?;
    for token in [
        "!extension_present(egl_extensions, \"EGL_KHR_wait_sync\") ||",
        "result = runtime->wait_sync(runtime->egl_display, sync, 0);",
        "runtime->stage = \"gpu-prime-internal-source\";",
    ] {
        require_contract_token(&runtime, &runtime_path, token)?;
    }
    require_contract_token(
        &runtime,
        &runtime_path,
        "(runtime->atlas_generation != 0U && generation != runtime->atlas_generation) ||",
    )?;
    require_contract_token(
        &runtime,
        &runtime_path,
        "runtime->atlas_generation = generation;",
    )?;
    require_contract_token(
        &runtime,
        &runtime_path,
        "sequence <= runtime->last_sequence ||",
    )?;
    require_contract_token(
        &module,
        &module_path,
        "if (direction != DMA_TO_DEVICE && direction != DMA_BIDIRECTIONAL)",
    )?;
    require_contract_token(
        &module,
        &module_path,
        "if (!dev_is_dma_coherent(attachment->dev))",
    )?;
    for token in [
        "#define RUSTOS_DVM_DMABUF_IOCTL_ACQUIRE _IOW('R', 0x42, struct rustos_dvm_acquire_request)",
        "!atomic_read(&state->relay_ready) ||",
        "dma_rmb();",
        "sync_file = sync_file_create(fence);",
    ] {
        require_contract_token(&module, &module_path, token)?;
    }
    require_contract_token(
        &agent,
        &agent_path,
        "die(\"input scheduler restore failed\");",
    )?;
    for token in [
        "#define READY_OWNER_NAME \"agent-owner.lock\"",
        "#define READY_CANDIDATE_NAME \".ready.next\"",
        "\"MODE=gpu-compositor-dmabuf-source\\n\"",
        "\"ATOMIC_KMS_SCANOUT=1\\n\"",
        "\"STAGED_DAMAGE_COPY=0\\n\"",
        "flock(guard->singleton_fd, LOCK_EX | LOCK_NB) != 0) {",
        "renameat(directory_fd, READY_CANDIDATE_NAME, directory_fd, \"ready\") != 0) {",
        "return local_health(&contract) ? EXIT_SUCCESS : EXIT_FAILURE;",
    ] {
        require_contract_token(&agent, &agent_path, token)?;
    }
    for (contents, path, token) in [
        (&relay, &relay_path, "rustos_gpu_runtime_render_legacy"),
        (&relay, &relay_path, "open_kms_display"),
        (&runtime, &runtime_path, "rustos_gpu_runtime_render_legacy"),
        (
            &runtime_header,
            &runtime_header_path,
            "rustos_gpu_runtime_render_legacy",
        ),
        (&relay, &relay_path, "MODE=dmabuf-direct-scanout"),
        (&agent, &agent_path, "MODE=dmabuf-direct-scanout"),
        (&agent, &agent_path, "access(READY_FILE"),
        (&relay, &relay_path, "ftruncate(fd, 0)"),
        (&relay, &relay_path, "unlink(RUSTOS_DVM_DISPLAY_READY_LOCK)"),
    ] {
        reject_contract_token(contents, path, token)?;
    }
    Ok(())
}

fn require_contract_token(contents: &str, path: &Path, token: &str) -> Result<()> {
    if !contents.lines().any(|line| line.trim() == token) {
        bail!(
            "DVM GPU wire contract drift in {}: expected `{}`",
            path.display(),
            token
        );
    }
    Ok(())
}

fn reject_contract_token(contents: &str, path: &Path, token: &str) -> Result<()> {
    if contents.contains(token) {
        bail!(
            "retired DVM display path returned in {}: found `{}`",
            path.display(),
            token
        );
    }
    Ok(())
}

pub(crate) fn check(config: &Config, show_timings: bool) -> Result<()> {
    let mut timings = StepTimings::new(show_timings);
    validate_workspace_layering(&config.root_dir)?;
    timings.mark("layering");
    let manifests = load_manifests(&config.root_dir)?;
    timings.mark("manifests");
    crate::native_abi::check_native_abi_stamps(config)?;
    timings.mark("native-abi-stamps");
    validate_dvm_gpu_contract(config)?;
    timings.mark("dvm-gpu-contract");
    ensure_targets(config)?;
    timings.mark("targets");

    run_cargo_kernel_check(config, &config.nucleus_package)?;
    timings.mark("nucleus");
    check_nucleus_multiboot2_if_present(config)?;
    timings.mark("multiboot2");
    check_os_target_manifests(config, &manifests)?;
    timings.mark("os-targets");
    check_host_workspace(config, &manifests)?;
    timings.mark("host-workspace");

    timings.report("check");
    Ok(())
}

struct StepTimings {
    enabled: bool,
    started: Instant,
    previous: Instant,
    steps: Vec<(&'static str, Duration)>,
}

impl StepTimings {
    fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            started: now,
            previous: now,
            steps: Vec::new(),
        }
    }

    fn mark(&mut self, name: &'static str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        self.steps.push((name, now.duration_since(self.previous)));
        self.previous = now;
    }

    fn report(&self, operation: &str) {
        if !self.enabled {
            return;
        }
        for (name, elapsed) in &self.steps {
            eprintln!(
                "xtask: timing {operation}.{name}={:.3}s",
                elapsed.as_secs_f64()
            );
        }
        eprintln!(
            "xtask: timing {operation}.total={:.3}s",
            self.started.elapsed().as_secs_f64()
        );
    }
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
        if manifest.build.builder == BuilderKind::CargoKernelBinary {
            check_cargo_os_binary(config, package)?;
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
            BuilderKind::CargoKernelBinary | BuilderKind::KernelRustc
        ) && let Some(package) = manifest.build.package.as_deref()
        {
            excludes.insert(package.to_owned());
        }
    }
    excludes
}

fn ensure_targets(config: &Config) -> Result<()> {
    let mut command = Command::new(&config.rustup);
    command.arg("target").arg("add").arg(&config.kernel_target);
    run_command(&mut command)
}

fn build_efi(config: &Config) -> Result<()> {
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
        "# The standalone EFI image contains these preload modules. Load them before\n# enabling detached-signature enforcement, because Ubuntu GRUB does not ship\n# detached signatures for individual embedded modules.\ninsmod serial\nserial --speed=115200\nterminal_input serial\nterminal_output serial console\ninsmod search\ninsmod search_fs_file\ninsmod multiboot2\nset check_signatures=enforce\n# RustOS owns graphical output after the nucleus starts. Keep GRUB on the\n# firmware text/serial consoles because some OVMF GOP implementations reject\n# gfxterm's automatic mode selection.\nsearch --file --set=root /nucleus.elf\nmultiboot2 ($root)/nucleus.elf\nmodule2 ($root)/system/boot/early-system.img rustos-early-system\nboot\n",
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
        .arg("memdisk tar normal serial pgp gcry_rsa gcry_sha256 gcry_sha512 fat part_msdos part_gpt search search_fs_file ls multiboot2")
        .arg("--install-modules")
        .arg(config.rustos_grub_modules.as_deref().unwrap_or(
            "normal serial multiboot2 part_msdos part_gpt fat search search_fs_file ls gcry_rsa gcry_sha256 gcry_sha512 pgp memdisk tar",
        ))
        .arg(format!("/boot/grub/grub.cfg={}", grub_cfg.display()))
        .arg(format!(
            "/boot/grub/grub.cfg.sig={}",
            grub_cfg_signature.display()
        ));
    if let Some(sbat) = config.rustos_grub_sbat.as_ref() {
        command.arg("--sbat").arg(sbat);
    }
    let result = run_command(&mut command);
    // The directory only carries the embedded grub.cfg inputs; never leak one
    // per build.
    let _ = remove_dir_if_exists(&temp_dir);
    result
}

pub(crate) fn build_nucleus(config: &Config) -> Result<()> {
    run_cargo_kernel_rustc(config, &config.nucleus_package, &config.nucleus_rustc_args)?;
    let artifact = config.artifact_nucleus_elf_path();
    let source = config.nucleus_source_path();
    if !output_is_fresh(&artifact, std::slice::from_ref(&source))? {
        copy_with_parent(&source, &artifact)?;
    }
    prepare_self_relocating_multiboot_elf(&artifact)?;
    check_nucleus_multiboot2(config)?;
    audit_simd_custody(config)?;
    refresh_nucleus_signature_after_build(config)
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
    if !output_is_fresh(&signature, &[nucleus])? {
        // `sign_nucleus` creates the local development signing material when a
        // release key was not supplied. A fresh nucleus must never leave a
        // stale or missing signature for a later stage invocation.
        sign_nucleus(config)?;
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
        // Only a "Name <mail>" user id can name a development key to create. A
        // key id or fingerprint names an existing key; generating a new key
        // whose user id is that string would silently sign with an unrelated
        // key instead of the one the caller asked for.
        if !signing_key.contains('<') {
            bail!(
                "GRUB signing key {signing_key} has no secret key in {}; refusing to generate a key for a key id or fingerprint",
                gpg_home.display()
            );
        }
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
    let manifests = load_manifests(&config.root_dir)?;
    ensure_targets(config)?;
    build_userspace_manifests(config, &manifests)
}

fn build_userspace_manifests(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    let winsys_root = required_manifest(manifests, "winsys")?.package_root.clone();
    validate_winsys_export_contracts(winsys_root.as_path())?;
    build_manifests_matching(config, manifests, |manifest| {
        matches!(
            manifest.build.builder,
            BuilderKind::CargoKernelBinary | BuilderKind::MingwCExe
        )
    })?;
    build_manifests_matching(config, manifests, |manifest| {
        matches!(manifest.build.builder, BuilderKind::WinsysDllBundle)
    })?;
    build_manifests_matching(config, manifests, |manifest| {
        matches!(manifest.build.builder, BuilderKind::AssetBundle | BuilderKind::PrebuiltApp)
    })?;
    build_manifests_matching(config, manifests, |manifest| {
        matches!(manifest.build.builder, BuilderKind::CDemo)
    })
}

const WINE_CORE_DLL_TARGETS: &[(&str, &str)] = &[
    ("apisetschema.dll", "dlls/apisetschema/x86_64-windows/apisetschema.dll"),
    ("ntdll.dll", "dlls/ntdll/x86_64-windows/ntdll.dll"),
    ("kernelbase.dll", "dlls/kernelbase/x86_64-windows/kernelbase.dll"),
    ("kernel32.dll", "dlls/kernel32/x86_64-windows/kernel32.dll"),
    ("msvcrt.dll", "dlls/msvcrt/x86_64-windows/msvcrt.dll"),
    ("ucrtbase.dll", "dlls/ucrtbase/x86_64-windows/ucrtbase.dll"),
    ("vcruntime140.dll", "dlls/vcruntime140/x86_64-windows/vcruntime140.dll"),
    ("vcruntime140_1.dll", "dlls/vcruntime140_1/x86_64-windows/vcruntime140_1.dll"),
    ("nsi.dll", "dlls/nsi/x86_64-windows/nsi.dll"),
    ("ws2_32.dll", "dlls/ws2_32/x86_64-windows/ws2_32.dll"),
];

const WINE_NLS_FILES: &[&str] = &[
    "locale.nls",
    "l_intl.nls",
    "c_037.nls",
    "c_437.nls",
    "c_1252.nls",
    "c_20127.nls",
    "c_28591.nls",
    "c_949.nls",
    "normnfc.nls",
    "normnfd.nls",
    "normnfkc.nls",
    "normnfkd.nls",
    "normidna.nls",
    "sortdefault.nls",
];

fn collect_wine_dll_inputs(
    config: &Config,
    manifest: &PackageManifest,
    wine_source_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut inputs = vec![
        manifest.manifest_path.clone(),
        config.root_dir.join("tools/xtask/src/build/mod.rs"),
        wine_source_dir.join("RUSTOS-PATCHES.md"),
        wine_source_dir.join("VERSION"),
        wine_source_dir.join("configure"),
        wine_source_dir.join("include/wine/asm.h"),
        wine_source_dir.join("include/winternl.h"),
    ];
    for dll_dir in [
        "apisetschema",
        "ntdll",
        "kernelbase",
        "kernel32",
        "msvcrt",
        "ucrtbase",
        "vcruntime140",
        "vcruntime140_1",
        "nsi",
        "ws2_32",
    ] {
        let dir = wine_source_dir.join("dlls").join(dll_dir);
        if dir.is_dir() {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.is_file()
                    && matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("c" | "h" | "spec" | "s" | "in")
                    )
                {
                    inputs.push(path);
                }
            }
        }
    }
    for nls in WINE_NLS_FILES {
        inputs.push(wine_source_dir.join("nls").join(nls));
    }
    inputs.sort();
    Ok(inputs)
}

fn build_windows_system_dlls(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let wine_source_dir = config.root_dir.join("vendor/wine");
    if !wine_source_dir.join("configure").is_file() {
        bail!(
            "missing Wine 11.22 source tree at {}",
            wine_source_dir.display()
        );
    }

    let artifact_dir = manifest.artifact_path(config);
    let mut expected_outputs = Vec::new();
    let mut expected_names = BTreeSet::new();
    for (file_name, _) in WINE_CORE_DLL_TARGETS {
        expected_outputs.push(artifact_dir.join(file_name));
        expected_names.insert((*file_name).to_string());
    }
    for nls in WINE_NLS_FILES {
        expected_outputs.push(artifact_dir.join(nls));
        expected_names.insert((*nls).to_string());
    }

    let inputs = collect_wine_dll_inputs(config, manifest, &wine_source_dir)?;
    let has_stale_entries = artifact_dir.is_dir()
        && fs::read_dir(&artifact_dir)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .any(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map(|name| !expected_names.contains(&name))
                    .unwrap_or(true)
            });

    if !has_stale_entries && outputs_are_fresh(&expected_outputs, &inputs)? {
        return Ok(());
    }

    let wine_build_dir = config.build_dir.join("wine");
    if !wine_build_dir.join("Makefile").is_file() {
        fs::create_dir_all(&wine_build_dir)?;
        let mut configure_cmd = Command::new(wine_source_dir.join("configure"));
        configure_cmd
            .current_dir(&wine_build_dir)
            .arg("--enable-archs=x86_64")
            .args([
                "--without-x",
                "--without-alsa",
                "--without-cups",
                "--without-dbus",
                "--without-gphoto",
                "--without-gssapi",
                "--without-gstreamer",
                "--without-krb5",
                "--without-netapi",
                "--without-opencl",
                "--without-opengl",
                "--without-osmesa",
                "--without-oss",
                "--without-pcap",
                "--without-pulse",
                "--without-sane",
                "--without-sdl",
                "--without-udev",
                "--without-unwind",
                "--without-usb",
                "--without-v4l2",
                "--without-vulkan",
            ])
            .arg("x86_64_CFLAGS=-D__RUSTOS__ -O2");
        run_command(&mut configure_cmd)?;
    }

    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut tooldeps_cmd = Command::new("make");
    tooldeps_cmd
        .current_dir(&wine_build_dir)
        .arg(format!("-j{jobs}"))
        .arg("__tooldeps__");
    run_command(&mut tooldeps_cmd)?;

    let mut dlls_cmd = Command::new("make");
    dlls_cmd
        .current_dir(&wine_build_dir)
        .arg(format!("-j{jobs}"))
        .arg("x86_64_CFLAGS=-D__RUSTOS__ -O2");
    for (_, rel_target) in WINE_CORE_DLL_TARGETS {
        dlls_cmd.arg(rel_target);
    }
    run_command(&mut dlls_cmd)?;

    remove_dir_if_exists(&artifact_dir)?;
    fs::create_dir_all(&artifact_dir)?;
    let strip_bin = crate::util::env_os("MINGW_STRIP")
        .unwrap_or_else(|| std::ffi::OsString::from("x86_64-w64-mingw32-strip"));
    for (file_name, rel_target) in WINE_CORE_DLL_TARGETS {
        let built = wine_build_dir.join(rel_target);
        if !built.is_file() {
            bail!("missing built Wine PE64 DLL {}", built.display());
        }
        let staged = artifact_dir.join(file_name);
        let mut strip_cmd = Command::new(&strip_bin);
        strip_cmd
            .arg("--strip-unneeded")
            .arg("-o")
            .arg(&staged)
            .arg(&built);
        run_command(&mut strip_cmd)?;
    }
    for nls in WINE_NLS_FILES {
        let src = wine_source_dir.join("nls").join(nls);
        let dst = artifact_dir.join(nls);
        fs::copy(&src, &dst)?;
    }

    Ok(())
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
        BuilderKind::KernelRustc => build_nucleus(config),
        BuilderKind::CargoKernelBinary => build_cargo_kernel_binary(config, manifest),
        BuilderKind::MingwCExe => build_mingw_c_exe(config, manifest),
        BuilderKind::CDemo => build_c_demo_manifest(config, manifest),
        BuilderKind::WinsysDllBundle => build_windows_system_dlls(config, manifest),
        BuilderKind::AssetBundle | BuilderKind::PrebuiltApp => build_directory_bundle(config, manifest),
    }
}

/// Mirror a data or prebuilt-app package's `build.source` into its artifact
/// directory. Only files whose contents changed are written, so an unchanged
/// bundle does not look newer to later freshness checks, and files removed
/// from the source are removed from the artifact.
fn build_directory_bundle(config: &Config, manifest: &PackageManifest) -> Result<()> {
    let source_root = manifest
        .resolved_source_path()
        .with_context(|| format!("bundle package {} has no build.source", manifest.id))?;
    if !source_root.is_dir() {
        bail!(
            "bundle package {} source is not a directory: {}",
            manifest.id,
            source_root.display()
        );
    }
    if manifest.build.builder == BuilderKind::PrebuiltApp {
        let prefix = format!("{}/", manifest.install.path);
        for entry in &manifest.desktop.entries {
            let image = entry.image.as_deref().context("prebuilt-app has no desktop image")?;
            let relative = image.strip_prefix(&prefix).context("prebuilt-app image outside bundle")?;
            if !source_root.join(relative).is_file() {
                bail!("prebuilt-app {} missing executable: {}", manifest.id, image);
            }
        }
    }
    let artifact_dir = manifest.artifact_path(config);
    fs::create_dir_all(&artifact_dir)?;
    sync_directory(&source_root, &artifact_dir)
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
        // Prebuilt std is not CET-instrumented. Rebuild the entire Rust
        // dependency closure, not just each final userspace crate.
        .args([
            "-Z",
            "build-std=std,panic_abort",
            "-Z",
            "build-std-features=compiler-builtins-mem",
        ])
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target)
        .arg("--release")
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    apply_owned_target_rustflags(
        &mut command,
        "-Z cf-protection=branch -C jump-tables=n -C panic=abort",
    );
    run_command(&mut command)?;

    let binary = config
        .cargo_target_dir
        .join(format!("{}/release/{package}", config.kernel_target));
    let artifact = manifest.artifact_path(config);
    if output_is_fresh(&artifact, std::slice::from_ref(&binary))? {
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
        "-Z cf-protection=branch -C jump-tables=n ",
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
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
    apply_owned_target_rustflags(&mut command, rustflags);
    run_command(&mut command)?;

    let binary = config
        .cargo_target_dir
        .join(format!("{}/release/{package}", config.kernel_target));
    let artifact = manifest.artifact_path(config);
    if output_is_fresh(&artifact, std::slice::from_ref(&binary))? {
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

    if output_is_fresh(
        &output,
        &[
            source.clone(),
            manifest.manifest_path.clone(),
            config.root_dir.join("tools/xtask/src/build/mod.rs"),
        ],
    )? {
        return Ok(());
    }

    remove_file_if_exists(&output)?;

    run_command(
        Command::new(&config.mingw_cc)
            .arg("-fcf-protection=branch")
            .arg("-fno-jump-tables")
            .arg("-Os")
            .arg("-s")
            .arg("-Wl,--image-base,0x400000")
            .args(&manifest.build.extra_args)
            .arg("-o")
            .arg(&output)
            .arg(&source),
    )?;
    if manifest.id == "userdemo2" {
        let winsys_root = required_manifest(&load_manifests(&config.root_dir)?, "winsys")?
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
    let sources = c_demo_sources(
        source,
        &manifest.package_root,
        &manifest.build.extra_sources,
    )?;
    let inputs = c_demo_inputs(
        &sources,
        &manifest.manifest_path,
        &config.root_dir.join("tools/xtask/src/build/mod.rs"),
    );
    if output_is_fresh(&output, &inputs)? {
        return Ok(());
    }
    build_c_demo(&config.cc, &sources, &output, &extra_args)
}

fn c_demo_sources(source: PathBuf, package_root: &Path, extra: &[String]) -> Result<Vec<PathBuf>> {
    let mut sources = vec![source];
    for name in extra {
        if name.is_empty()
            || !Path::new(name)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            bail!("C package extra source is not a package-relative file: {name}");
        }
        let path = package_root.join(name);
        if sources.contains(&path) {
            bail!("duplicate C package translation unit: {}", path.display());
        }
        sources.push(path);
    }
    Ok(sources)
}

fn c_demo_inputs(sources: &[PathBuf], manifest: &Path, build_owner: &Path) -> Vec<PathBuf> {
    sources
        .iter()
        .cloned()
        .chain([manifest.to_path_buf(), build_owner.to_path_buf()])
        .collect()
}

fn build_c_demo(
    cc: &std::ffi::OsString,
    sources: &[PathBuf],
    output: &Path,
    extra_args: &[&str],
) -> Result<()> {
    let parent = output
        .parent()
        .with_context(|| format!("demo artifact path has no parent: {}", output.display()))?;
    fs::create_dir_all(parent)?;

    let mut command = Command::new(cc);
    command.args(sources);
    for arg in extra_args {
        command.arg(arg);
    }
    command
        .args(["-fcf-protection=branch", "-fno-jump-tables"])
        .arg("-o")
        .arg(output);
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

    let audit_path = config.userdemo2_import_audit_log_path();
    let audit_parent = audit_path
        .parent()
        .with_context(|| format!("import audit path has no parent: {}", audit_path.display()))?;
    fs::create_dir_all(audit_parent)?;
    fs::write(&audit_path, &output.stdout)?;
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

struct WinsysDllSpec {
    dir: &'static str,
    file_name: &'static str,
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
        },
        WinsysDllSpec {
            dir: "kernelbase",
            file_name: "kernelbase.dll",
        },
        WinsysDllSpec {
            dir: "kernel32",
            file_name: "kernel32.dll",
        },
        WinsysDllSpec {
            dir: "msvcrt",
            file_name: "msvcrt.dll",
        },
        WinsysDllSpec {
            dir: "ucrtbase",
            file_name: "ucrtbase.dll",
        },
        WinsysDllSpec {
            dir: "vcruntime140",
            file_name: "vcruntime140.dll",
        },
        WinsysDllSpec {
            dir: "vcruntime140_1",
            file_name: "vcruntime140_1.dll",
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
        ("api-ms-win-crt-private-l1-1-0.dll", "ucrtbase.dll"),
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
mod tests;
