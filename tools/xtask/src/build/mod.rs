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
    remove_file_if_exists, run_command,
};

mod cargo;

use cargo::{run_cargo_kernel_check, run_cargo_kernel_rustc};

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
    remove_dir_if_exists(&config.logs_dir)?;
    Ok(())
}

fn ensure_targets(config: &Config) -> Result<()> {
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
        "# The standalone EFI image contains these preload modules. Load them before\n# enabling detached-signature enforcement, because Ubuntu GRUB does not ship\n# detached signatures for individual embedded modules.\ninsmod serial\nserial --speed=115200\nterminal_input serial\nterminal_output serial console\ninsmod search\ninsmod search_fs_file\ninsmod multiboot2\nset check_signatures=enforce\n# RustOS owns graphical output after the nucleus starts. Keep GRUB on the\n# firmware text/serial consoles because some OVMF GOP implementations reject\n# gfxterm's automatic mode selection.\nsearch --file --set=root /nucleus.elf\nmultiboot2 ($root)/nucleus.elf\nmodule2 ($root)/system/registry/kernel/root-file-extents.tsv rustos-root-extents\nboot\n",
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
    if output_is_fresh(&output, &[source.clone(), manifest.manifest_path.clone()])? {
        return Ok(());
    }
    build_c_demo(&config.cc, &source, &output, &extra_args)
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
    use super::{
        canonical_forward_dll_name, parse_def_exports, parse_objdump_imports, reject_contract_token,
    };
    use fs_err as fs;
    use std::path::Path;

    #[test]
    fn retired_dvm_display_path_guard_fails_closed() {
        let path = Path::new("rustos-dvm-display.c");
        assert!(reject_contract_token("active GPU path", path, "render_legacy").is_ok());
        let error = reject_contract_token("render_legacy", path, "render_legacy")
            .expect_err("retired renderer must be rejected");
        assert!(
            error
                .to_string()
                .contains("retired DVM display path returned")
        );
    }

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
