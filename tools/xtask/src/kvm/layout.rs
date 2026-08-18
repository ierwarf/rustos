// SPDX-License-Identifier: MIT

fn prepare_layout(config: &Config, options: &SmokeOptions) -> Result<KvmLayout> {
    if !config.boot_disk_image.is_file() {
        bail!(
            "missing RustOS boot disk {}; run `cargo xtask build` first",
            config.boot_disk_image.display()
        );
    }
    if !config.ovmf_path.is_file() {
        bail!(
            "missing pinned OVMF firmware {}",
            config.ovmf_path.display()
        );
    }

    let run_dir = config.build_dir.join("kvm");
    fs::create_dir_all(&run_dir)?;
    fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "restrict KVM runtime directory permissions {}",
            run_dir.display()
        )
    })?;
    // Normal interactive KVM sessions do not alter the boot image. QEMU's
    // snapshot mode below protects it from guest writes, avoiding a full disk
    // copy on every F5. Only proof options that patch private boot content
    // receive a per-run image.
    let needs_private_contracts = options.min_ui_fps.is_some()
        || options.exercise_network
        || options.smp_ring3_qualification
        || options.ipcbench_probe.is_some();
    let runtime_disk = if needs_private_contracts {
        let runtime_disk = run_dir.join("rustos-kvm.img");
        fs::copy(&config.boot_disk_image, &runtime_disk).with_context(|| {
            format!(
                "failed to create KVM runtime disk from {}",
                config.boot_disk_image.display()
            )
        })?;
        runtime_disk
    } else {
        config.boot_disk_image.clone()
    };
    if needs_private_contracts {
        write_private_kvm_contracts(
            &runtime_disk,
            options
                .min_ui_fps
                .is_some()
                .then_some((options.min_ui_fps.is_some(), options.exercise_network)),
            options
                .smp_ring3_qualification
                .then_some(options.rustos_vcpus),
            options.ipcbench_probe.as_deref(),
        )?;
    }
    let display_backing_dir = if options.gui_dvm_surfaces {
        Some(create_dma_pinnable_display_directory()?)
    } else {
        None
    };
    let gui_dvm_surfaces = if let Some(directory) = display_backing_dir.as_ref() {
        // A physical VFIO device causes QEMU to map every guest RAM section
        // into its IOMMUFD IOAS. Keep the writable ivshmem BAR on tmpfs: a
        // regular build-directory file cannot be write-pinned by MAP_FILE.
        let path = directory.path().join("dvm-display.ivshmem");
        create_gui_dvm_surfaces(&path)?;
        Some(path)
    } else {
        None
    };
    let gui_dvm_pixels = if let Some(directory) = display_backing_dir.as_ref() {
        let path = directory.path().join("dvm-display.pmem");
        create_gui_dvm_surfaces(&path)?;
        Some(path)
    } else {
        None
    };
    let dvm_display_doorbell = gui_dvm_surfaces
        .as_ref()
        .map(|_| run_dir.join("dvm-display-doorbell.sock"));
    let dvm_network_shmem = if options.dvm_network_shmem {
        let path = run_dir.join("dvm-network.ivshmem");
        create_dvm_network_shmem(&path)?;
        Some(path)
    } else {
        None
    };
    let (dvm_block_aperture, dvm_block_doorbell, dvm_block_disk) = if options.dvm_block_shmem {
        let disk = run_dir.join("dvm-block-disk.img");
        // The storage DVM becomes the authoritative VFS backing store as soon
        // as the block relay is admitted. Copy the already-private runtime
        // image so KVM-only registry overrides are visible through both the
        // bootstrap disk and the post-handoff DVM path. Copying the pristine
        // build image here silently discarded profiling/network overrides and
        // made their acceptance gates observe a different filesystem view.
        fs::copy(&runtime_disk, &disk).with_context(|| {
            format!(
                "create private storage-DVM disk from {}",
                runtime_disk.display()
            )
        })?;
        fs::set_permissions(&disk, std::fs::Permissions::from_mode(0o600))?;
        sync_private_dvm_block_snapshot(&disk, &run_dir)?;
        let aperture = run_dir.join("dvm-block.ivshmem");
        create_dvm_block_aperture(&aperture, &disk, &config.storage_epoch_signing_key)?;
        (
            Some(aperture),
            Some(run_dir.join("dvm-block-doorbell.sock")),
            Some(disk),
        )
    } else {
        (None, None, None)
    };

    let debugcon_log = run_dir.join("rustos-debugcon.log");
    let rustos_serial_log = run_dir.join("rustos-serial.log");
    let dvm_serial_log = run_dir.join("linux-dvm-serial.log");
    let rustos_stderr_log = run_dir.join("rustos-qemu.stderr.log");
    let dvm_stderr_log = run_dir.join("linux-dvm-qemu.stderr.log");
    let dvm_input_ring = run_dir.join("dvm-input.ivshmem");
    let dvm_input_doorbell = run_dir.join("dvm-input-doorbell.sock");
    let rustos_monitor = run_dir.join("rustos-monitor.sock");
    if rustos_monitor.exists() {
        fs::remove_file(&rustos_monitor)?;
    }
    let dvm_control_secret = run_dir.join("linux-dvm-control.secret");
    let control_secret = ControlSecret::random()?;
    fs::write(&dvm_control_secret, control_secret.as_hex())?;
    fs::set_permissions(&dvm_control_secret, std::fs::Permissions::from_mode(0o600))?;
    for log in [
        &debugcon_log,
        &rustos_serial_log,
        &dvm_serial_log,
        &rustos_stderr_log,
        &dvm_stderr_log,
    ] {
        prepare_runtime_log(log, !options.dry_run)?;
    }
    create_dvm_input_ring(&dvm_input_ring)?;

    Ok(KvmLayout {
        run_dir,
        guest_cid: guest_cid_for_process(std::process::id()),
        runtime_disk,
        debugcon_log,
        rustos_serial_log,
        dvm_serial_log,
        rustos_stderr_log,
        dvm_stderr_log,
        dvm_input_ring,
        dvm_input_doorbell,
        rustos_monitor,
        dvm_control_secret,
        _display_backing_dir: display_backing_dir,
        gui_dvm_surfaces,
        gui_dvm_pixels,
        dvm_display_doorbell,
        dvm_network_shmem,
        dvm_block_aperture,
        dvm_block_doorbell,
        dvm_block_disk,
    })
}

fn prepare_runtime_log(path: &Path, truncate_existing: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    if truncate_existing {
        options.truncate(true);
    }
    options
        .open(path)
        .with_context(|| format!("prepare KVM runtime log {}", path.display()))?;
    Ok(())
}

fn create_dma_pinnable_display_directory() -> Result<TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("rustos-kvm-display-")
        .tempdir_in("/dev/shm")
        .context("create private tmpfs DVM display-backing directory")?;
    fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let directory_fd = std::fs::File::open(directory.path())?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(directory_fd.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("inspect KVM DVM display-backing filesystem");
    }
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    if unsafe { filesystem.assume_init() }.f_type as u64 != TMPFS_MAGIC {
        bail!("/dev/shm is not tmpfs; refusing unproven VFIO display backings");
    }
    Ok(directory)
}

fn create_dvm_input_ring(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM input-ring path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmInputRingHeader::new(DVM_INPUT_RING_APERTURE_BYTES, 1);
    if !header.is_valid() {
        bail!("refusing to create invalid fixed DVM input-ring header");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create DVM input-ring backing {}", path.display()))?;
    file.set_len(DVM_INPUT_RING_APERTURE_BYTES)
        .with_context(|| format!("size DVM input-ring backing {}", path.display()))?;
    file.write_all(&header.encode())
        .with_context(|| format!("write DVM input-ring header {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync DVM input-ring backing {}", path.display()))?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn create_dvm_network_shmem(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM shared-network path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmNetHeader::new(DVM_NET_REGION_BYTES, 1);
    if !header.is_valid() {
        bail!("refusing to create invalid DVM shared-network header");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    file.set_len(DVM_NET_REGION_BYTES)?;
    file.write_all(&header.encode())?;
    file.sync_all()?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

include!("layout/block_transport.rs");

/// Allocate the production GUI-DVM three-surface pool for the KVM topology.
/// The L0 runner creates every slot and control record before either guest
/// starts. Neither guest gets to select an address, slot count, or queue size.
fn create_gui_dvm_surfaces(path: &Path) -> Result<()> {
    if path.to_string_lossy().contains(',') {
        bail!(
            "KVM shared-display path contains an unsupported QEMU option separator: {}",
            path.display()
        );
    }
    let header = DvmGuiSurfacePoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        DVM_DISPLAY_WIDTH,
        DVM_DISPLAY_HEIGHT,
    );
    if !header.is_valid() {
        bail!("refusing to create invalid GUI-DVM surface-pool header");
    }
    let atlas_header = DvmGpuAtlasPoolHeader::new(
        DVM_DISPLAY_REGION_BYTES,
        header,
        DVM_GPU_ATLAS_WIDTH,
        DVM_GPU_ATLAS_HEIGHT,
    )
    .ok_or_else(|| anyhow::anyhow!("refusing to create invalid GPU atlas-pool header"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create DVM shared display {}", path.display()))?;
    file.set_len(DVM_DISPLAY_REGION_BYTES)
        .with_context(|| format!("size DVM shared display {}", path.display()))?;
    file.write_all(&header.encode())
        .with_context(|| format!("initialize DVM shared display {}", path.display()))?;
    file.seek(SeekFrom::Start(DVM_GPU_ATLAS_POOL_HEADER_OFFSET as u64))
        .with_context(|| format!("seek DVM GPU atlas header {}", path.display()))?;
    file.write_all(&atlas_header.encode())
        .with_context(|| format!("initialize DVM GPU atlas header {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush DVM shared display {}", path.display()))?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict DVM shared display permissions {}", path.display()))?;
    Ok(())
}

/// A successful GUI-DVM smoke must prove a valid host `PRESENT` record and
/// pixels in exactly the slot named by that record. This rejects a host-only
/// pool, a stale record, and pixels written outside the fixed slot capability.
fn verify_dvm_display_surface(control_path: &Path, pixel_path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(control_path)
        .with_context(|| format!("open DVM display control {}", control_path.display()))?;
    let mut encoded = [0_u8; DvmGuiSurfacePoolHeader::encoded_len()];
    file.read_exact(&mut encoded)
        .with_context(|| format!("read DVM display control header {}", control_path.display()))?;
    let header = DvmGuiSurfacePoolHeader::decode(&encoded)
        .context("GUI-DVM surface-pool header changed or became invalid during smoke")?;
    if header.region_bytes != DVM_DISPLAY_REGION_BYTES
        || header.width != DVM_DISPLAY_WIDTH
        || header.height != DVM_DISPLAY_HEIGHT
    {
        bail!(
            "GUI-DVM surface-pool header differs from launch contract: region={} width={} height={}",
            header.region_bytes,
            header.width,
            header.height
        );
    }
    let mut newest = None;
    for slot in 0..DVM_GUI_SURFACE_SLOT_COUNT {
        let offset = u64::try_from(DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET)?
            .checked_add(u64::from(slot) * DvmGuiSurfaceMessage::encoded_len() as u64)
            .context("GUI-DVM host record offset overflow")?;
        file.seek(SeekFrom::Start(offset))?;
        let mut record = [0_u8; DvmGuiSurfaceMessage::encoded_len()];
        file.read_exact(&mut record)?;
        let Some(message) = DvmGuiSurfaceMessage::decode(&record) else {
            continue;
        };
        if !message.is_valid_for_dimensions(header.width, header.height)
            || !matches!(
                message.kind,
                driver_domain_protocol::DvmGuiSurfaceMessageKind::Present
            )
            || message.slot != slot
        {
            bail!(
                "GUI-DVM host record {} is malformed or exceeds its capability",
                slot
            );
        }
        if newest
            .is_none_or(|existing: DvmGuiSurfaceMessage| existing.generation < message.generation)
        {
            newest = Some(message);
        }
    }
    let message = newest.context("GUI-DVM surface pool contains no host PRESENT record")?;
    let slot_offset = header
        .slot_offset(message.slot)
        .context("GUI-DVM PRESENT names an out-of-range slot")?;
    let mut pixel_file = std::fs::File::open(pixel_path)
        .with_context(|| format!("open DVM cacheable pixel pool {}", pixel_path.display()))?;
    pixel_file.seek(SeekFrom::Start(slot_offset))?;
    let mut remaining = header.slot_bytes;
    let mut block = [0_u8; 4096];
    let mut wrote_pixels = false;
    while remaining > 0 {
        let bytes = usize::try_from(remaining.min(block.len() as u64))?;
        pixel_file.read_exact(&mut block[..bytes])?;
        if block[..bytes].iter().any(|byte| *byte != 0) {
            wrote_pixels = true;
            break;
        }
        remaining -= bytes as u64;
    }
    if !wrote_pixels {
        bail!("GUI-DVM provider published a slot but RustOS wrote no pixels into it");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct DvmNetworkCounters {
    tx_producer: u32,
    tx_consumer: u32,
    rx_producer: u32,
    rx_consumer: u32,
    dvm_ready: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct DvmInputCounters {
    producer: u64,
    consumer: u64,
    flags: u32,
}

fn dvm_input_counters(path: &Path) -> Result<DvmInputCounters> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open DVM shared input {}", path.display()))?;
    let mut bytes = [0_u8; DvmInputRingHeader::encoded_len()];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read DVM shared input header {}", path.display()))?;
    let header = DvmInputRingHeader::decode(&bytes)
        .context("DVM shared input header changed or became invalid during smoke")?;
    Ok(DvmInputCounters {
        producer: header.producer,
        consumer: header.consumer,
        flags: header.flags,
    })
}

impl DvmNetworkCounters {
    fn is_valid(self, slots: u32) -> bool {
        self.tx_producer.wrapping_sub(self.tx_consumer) <= slots
            && self.rx_producer.wrapping_sub(self.rx_consumer) <= slots
    }

    fn round_trip_observed(self) -> bool {
        self.tx_producer != 0
            && self.tx_consumer != 0
            && self.rx_producer != 0
            && self.rx_consumer != 0
    }
}

fn dvm_network_counters(path: &Path) -> Result<DvmNetworkCounters> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open DVM shared network {}", path.display()))?;
    let mut bytes = [0_u8; DvmNetHeader::encoded_len()];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read DVM shared network header {}", path.display()))?;
    let header = DvmNetHeader::decode(&bytes)
        .context("DVM shared network header changed or became invalid during smoke")?;
    let counters = DvmNetworkCounters {
        tx_producer: u32::from_le_bytes(bytes[40..44].try_into().expect("fixed counter offset")),
        tx_consumer: u32::from_le_bytes(bytes[44..48].try_into().expect("fixed counter offset")),
        rx_producer: u32::from_le_bytes(bytes[48..52].try_into().expect("fixed counter offset")),
        rx_consumer: u32::from_le_bytes(bytes[52..56].try_into().expect("fixed counter offset")),
        dvm_ready: header.dvm_ready(),
    };
    if !counters.is_valid(header.slot_count) {
        bail!(
            "DVM shared network counters violate bounded-ring invariant: tx={}/{} rx={}/{} slots={}",
            counters.tx_producer,
            counters.tx_consumer,
            counters.rx_producer,
            counters.rx_consumer,
            header.slot_count,
        );
    }
    Ok(counters)
}

fn verify_dvm_network_round_trip(path: &Path) -> Result<()> {
    let counters = dvm_network_counters(path)?;
    if !counters.round_trip_observed() {
        bail!(
            "DVM network exercise did not show bidirectional ring consumption: tx={}/{} rx={}/{}",
            counters.tx_producer,
            counters.tx_consumer,
            counters.rx_producer,
            counters.rx_consumer,
        );
    }
    Ok(())
}

fn dvm_block_header_matches_ready_generation(
    header: DvmBlockHeader,
    expected_generation: u64,
) -> bool {
    let ready = DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY | DVM_BLOCK_FLAG_READ_ONLY;
    header.flags & ready == ready && header.generation == expected_generation
}

fn verify_dvm_block_ready(layout: &KvmLayout) -> Result<()> {
    verify_dvm_block_ready_generation(layout, 1)
}

fn verify_dvm_block_ready_generation(layout: &KvmLayout, expected_generation: u64) -> Result<()> {
    let aperture = layout
        .dvm_block_aperture
        .as_deref()
        .context("storage-DVM proof lost its block aperture")?;
    let disk = layout
        .dvm_block_disk
        .as_deref()
        .context("storage-DVM proof lost its private backing disk")?;
    let mut file = std::fs::File::open(aperture)
        .with_context(|| format!("open live DVM block aperture {}", aperture.display()))?;
    let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read live DVM block header {}", aperture.display()))?;
    let header = DvmBlockHeader::decode(&bytes)
        .context("live DVM block aperture contains an invalid header")?;
    if !dvm_block_header_matches_ready_generation(header, expected_generation) {
        bail!(
            "DVM block peers did not publish expected read-only dual readiness generation={} actual_generation={} flags={:#x}",
            expected_generation,
            header.generation,
            header.flags,
        );
    }
    let disk_bytes = fs::metadata(disk)
        .with_context(|| format!("inspect live storage-DVM disk {}", disk.display()))?
        .len();
    if disk_bytes == 0
        || !disk_bytes.is_multiple_of(u64::from(DVM_BLOCK_MEDIA_BLOCK_BYTES))
        || header.capacity_sectors != disk_bytes / 512
        || header.logical_block_size != DVM_BLOCK_MEDIA_BLOCK_BYTES
        || header.physical_block_size != DVM_BLOCK_MEDIA_BLOCK_BYTES
    {
        bail!("live DVM block geometry diverged from the private backing disk");
    }
    Ok(())
}

fn render_private_acceptance_contract(ui_profile: bool, network_exercise: bool) -> String {
    format!(
        "contract=rustos-kvm-acceptance-v1\nui_profile={}\nnetwork_exercise={}\n",
        u8::from(ui_profile),
        u8::from(network_exercise),
    )
}

fn render_smp_ring3_qualification_contract(workers: u8) -> String {
    format!(
        "contract=rustos-kvm-smp-qualification-v1\nworkers={workers}\nwork_units={SMP_QUALIFICATION_WORK_UNITS}\ndeadline_ms={SMP_QUALIFICATION_DEADLINE_MS}\n"
    )
}

fn render_ipcbench_probe_contract(probe: &str) -> String {
    format!("contract=rustos-ipcbench-probe-v1\nprobe={probe}\n")
}

/// Write every per-run KVM contract through one mounted private FAT image.
/// Acceptance-v1 stays verbatim; Ring3 uses a disjoint path and proof mode.
fn write_private_kvm_contracts(
    runtime_disk: &Path,
    acceptance: Option<(bool, bool)>,
    smp_ring3_workers: Option<u8>,
    ipcbench_probe: Option<&str>,
) -> Result<()> {
    let disk = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(runtime_disk)
        .with_context(|| format!("open private KVM disk {}", runtime_disk.display()))?;
    let mut image = fatfs::StdIoWrapper::new(disk);
    image.seek(fatfs::SeekFrom::Start(0))?;
    let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new())?;
    {
        let root = fs.root_dir();
        if let Some((ui_profile, network_exercise)) = acceptance {
            let mut contract = root
                .create_file(PRIVATE_ACCEPTANCE_CONTRACT_PATH)
                .context("create private KVM acceptance contract file")?;
            contract.truncate()?;
            let contents = render_private_acceptance_contract(ui_profile, network_exercise);
            FatWrite::write_all(&mut contract, contents.as_bytes())?;
            FatWrite::flush(&mut contract)?;
        }
        if let Some(workers) = smp_ring3_workers {
            let mut contract = root
                .create_file(PRIVATE_SMP_QUALIFICATION_CONTRACT_PATH)
                .with_context(|| {
                    format!(
                        "create private KVM SMP qualification contract {PRIVATE_SMP_QUALIFICATION_CONTRACT_PATH}"
                    )
                })?;
            contract.truncate()?;
            let contents = render_smp_ring3_qualification_contract(workers);
            FatWrite::write_all(&mut contract, contents.as_bytes())?;
            FatWrite::flush(&mut contract)?;
        }
        if let Some(probe) = ipcbench_probe {
            let mut contract = root
                .create_file(PRIVATE_IPCBENCH_PROBE_CONTRACT_PATH)
                .context("create private KVM ipcbench probe contract file")?;
            contract.truncate()?;
            let contents = render_ipcbench_probe_contract(probe);
            FatWrite::write_all(&mut contract, contents.as_bytes())?;
            FatWrite::flush(&mut contract)?;
        }
    }
    fs.unmount()?;
    Ok(())
}

fn read_private_smp_ring3_qualification_contract(runtime_disk: &Path) -> Result<Vec<u8>> {
    read_private_kvm_file(runtime_disk, PRIVATE_SMP_QUALIFICATION_CONTRACT_PATH)
}

fn read_private_early_system_image(runtime_disk: &Path) -> Result<Vec<u8>> {
    read_private_kvm_file(runtime_disk, "system/boot/early-system.img")
}

fn read_private_kvm_file(runtime_disk: &Path, path: &str) -> Result<Vec<u8>> {
    let disk = std::fs::OpenOptions::new()
        .read(true)
        .open(runtime_disk)
        .with_context(|| format!("open private KVM disk {}", runtime_disk.display()))?;
    let mut image = fatfs::StdIoWrapper::new(disk);
    image.seek(fatfs::SeekFrom::Start(0))?;
    let fs = fatfs::FileSystem::new(image, fatfs::FsOptions::new())?;
    let mut contents = Vec::new();
    {
        let mut contract = fs
            .root_dir()
            .open_file(path)
            .with_context(|| format!("open private KVM file {path}"))?;
        contract.read_to_end(&mut contents)?;
    }
    fs.unmount()?;
    Ok(contents)
}

fn require_qemu(config: &Config) -> Result<PathBuf> {
    resolve_command_path(&config.kvm_qemu_bin).with_context(|| {
        format!(
            "missing KVM QEMU command {}; install qemu-system-x86 or set KVM_QEMU_BIN",
            Path::new(&config.kvm_qemu_bin).display()
        )
    })
}

fn canonical_pci_bdf(value: &str) -> Result<String> {
    let (domain, rest) = value
        .split_once(':')
        .with_context(|| format!("invalid PCI BDF {value:?}"))?;
    let (bus, device_function) = rest
        .split_once(':')
        .with_context(|| format!("invalid PCI BDF {value:?}"))?;
    let (device, function) = device_function
        .split_once('.')
        .with_context(|| format!("invalid PCI BDF {value:?}"))?;
    let domain = u16::from_str_radix(domain, 16)
        .with_context(|| format!("invalid PCI domain in {value:?}"))?;
    let bus =
        u8::from_str_radix(bus, 16).with_context(|| format!("invalid PCI bus in {value:?}"))?;
    let device = u8::from_str_radix(device, 16)
        .with_context(|| format!("invalid PCI device in {value:?}"))?;
    let function = u8::from_str_radix(function, 16)
        .with_context(|| format!("invalid PCI function in {value:?}"))?;
    if device > 0x1f || function > 7 {
        bail!("PCI BDF is outside device/function bounds: {value}");
    }
    let canonical = format!("{domain:04x}:{bus:02x}:{device:02x}.{function:x}");
    if canonical != value {
        bail!("PCI BDF must be canonical lowercase {canonical}, got {value:?}");
    }
    Ok(canonical)
}

fn require_direct_rw_character_device(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
        bail!(
            "{label} must be a direct character device: {}",
            path.display()
        );
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {label} {} read/write", path.display()))?;
    Ok(())
}

fn vfio_device_cdev_path(device: &Path) -> Result<PathBuf> {
    let mut names = std::fs::read_dir(device.join("vfio-dev"))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    if names.len() != 1 {
        bail!(
            "physical AMD VFIO device must expose exactly one vfio-dev cdev, found {}",
            names.len()
        );
    }
    let name = names
        .pop()
        .and_then(|name| name.into_string().ok())
        .context("physical AMD vfio-dev name is not UTF-8")?;
    let id = name
        .strip_prefix("vfio")
        .context("physical AMD vfio-dev name lacks vfio prefix")?
        .parse::<u32>()
        .context("physical AMD vfio-dev name has a non-numeric ID")?;
    if name != format!("vfio{id}") {
        bail!("physical AMD vfio-dev name is not canonical: {name}");
    }
    Ok(Path::new("/dev/vfio/devices").join(name))
}

fn physical_memlock_soft_limit() -> Result<Option<u64>> {
    let limits = fs::read_to_string("/proc/self/limits")?;
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max locked memory"))
        .context("/proc/self/limits has no Max locked memory row")?;
    let value = line
        .split_whitespace()
        .nth(3)
        .context("Max locked memory row has no soft limit")?;
    if value == "unlimited" {
        return Ok(None);
    }
    Ok(Some(
        value
            .parse()
            .context("parse Max locked memory soft limit")?,
    ))
}

fn validate_lab_amd_vfct(path: &Path, owner: u32) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect lab AMD VFCT {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "lab AMD VFCT must be an owner-private non-symlink file: {}",
            path.display()
        );
    }
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize lab AMD VFCT {}", path.display()))?;
    let label = canonical.to_string_lossy();
    if label.contains([',', '\n', '\r']) {
        bail!("lab AMD VFCT path is not representable as a QEMU property");
    }
    let table = fs::read(&canonical)?;
    if table.len() < ACPI_VFCT_HEADER_BYTES
        || table.len() > ACPI_VFCT_MAX_BYTES
        || table.get(0..4) != Some(b"VFCT")
    {
        bail!("lab AMD VFCT header or bounded size is invalid");
    }
    let table_length =
        u32::from_le_bytes(table[4..8].try_into().expect("validated VFCT length field")) as usize;
    if table_length != table.len()
        || table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0
    {
        bail!("lab AMD VFCT length or ACPI checksum is invalid");
    }
    let image_header = u32::from_le_bytes(
        table[ACPI_VFCT_VBIOS_OFFSET..ACPI_VFCT_VBIOS_OFFSET + 4]
            .try_into()
            .expect("validated VFCT image offset"),
    ) as usize;
    let image_start = image_header
        .checked_add(ACPI_VFCT_IMAGE_HEADER_BYTES)
        .context("lab AMD VFCT image offset overflow")?;
    if image_header < ACPI_VFCT_HEADER_BYTES || image_start > table.len() {
        bail!("lab AMD VFCT image header is out of bounds");
    }
    let field_u32 = |offset: usize| -> Result<u32> {
        let bytes = table
            .get(offset..offset + 4)
            .context("truncated lab AMD VFCT field")?;
        Ok(u32::from_le_bytes(
            bytes.try_into().expect("four-byte VFCT field"),
        ))
    };
    let field_u16 = |offset: usize| -> Result<u16> {
        let bytes = table
            .get(offset..offset + 2)
            .context("truncated lab AMD VFCT identity")?;
        Ok(u16::from_le_bytes(
            bytes.try_into().expect("two-byte VFCT field"),
        ))
    };
    let image_length = field_u32(image_header + ACPI_VFCT_IMAGE_LENGTH_OFFSET)? as usize;
    let image_end = image_start
        .checked_add(image_length)
        .context("lab AMD VFCT image length overflow")?;
    if field_u32(image_header)? != 0
        || field_u32(image_header + 4)? != 8
        || field_u32(image_header + 8)? != 0
        || field_u16(image_header + 12)? != 0x1002
        || field_u16(image_header + 14)? != 0x1900
        || image_length < 0x4a
        || image_end > table.len()
        || table.get(image_start..image_start + 2) != Some(&[0x55, 0xaa])
    {
        bail!("lab AMD VFCT is not the exact relocated 1002:1900 guest 00:08.0 image");
    }
    let atom_header = usize::from(field_u16(image_start + 0x48)?);
    let atom = image_start
        .checked_add(atom_header)
        .and_then(|offset| offset.checked_add(4))
        .context("lab AMD VBIOS ATOM pointer overflow")?;
    if table
        .get(atom..atom + 4)
        .is_none_or(|magic| magic != b"ATOM" && magic != b"MOTA")
    {
        bail!("lab AMD VFCT VBIOS lacks an ATOM header");
    }
    Ok(canonical)
}

fn physical_gpu_profile(vendor: &str, device: &str) -> Option<PhysicalGpuProfile> {
    PHYSICAL_GPU_PROFILES
        .iter()
        .copied()
        .find(|profile| profile.vendor == vendor && profile.device == device)
}

fn selected_physical_gpu_profile(options: &SmokeOptions) -> Result<PhysicalGpuProfile> {
    let bdf = canonical_pci_bdf(
        options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU BDF is missing")?,
    )?;
    let device = Path::new("/sys/bus/pci/devices").join(&bdf);
    let vendor = fs::read_to_string(device.join("vendor"))?;
    let device_id = fs::read_to_string(device.join("device"))?;
    physical_gpu_profile(vendor.trim(), device_id.trim()).with_context(|| {
        format!(
            "physical GPU {:04x}:{:04x} has no certified profile",
            u16::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).unwrap_or(0),
            u16::from_str_radix(device_id.trim().trim_start_matches("0x"), 16).unwrap_or(0)
        )
    })
}

fn gpu_evidence_expectation(options: &SmokeOptions) -> Result<GpuEvidenceExpectation> {
    if options.physical_gpu_bdf.is_none() {
        return Ok(VIRTUAL_GPU_EVIDENCE);
    }
    let profile = selected_physical_gpu_profile(options)?;
    Ok(GpuEvidenceExpectation {
        drm_driver: profile.drm_driver,
        backend_class: profile.backend_class,
    })
}

fn claim_physical_gpu_launch(layout: &KvmLayout, options: &SmokeOptions) -> Result<()> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let boot_id = boot_id.trim();
    if boot_id.len() != 36
        || !boot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        bail!("host boot ID is malformed; refusing physical GPU launch");
    }
    let bdf = canonical_pci_bdf(
        options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU BDF is missing")?,
    )?;
    let profile = selected_physical_gpu_profile(options)?;
    claim_physical_gpu_launch_in(&layout.run_dir, boot_id, profile, &bdf)
}

fn claim_physical_gpu_launch_in(
    run_dir: &Path,
    boot_id: &str,
    profile: PhysicalGpuProfile,
    bdf: &str,
) -> Result<()> {
    let claim = run_dir.join(format!("physical-gpu-launch-{boot_id}"));
    match fs::create_dir(&claim) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "physical GPU launch already attempted during host boot {boot_id}; reset methods are disabled, so cold-boot the host before another assignment"
            )
        }
        Err(error) => return Err(error).context("create physical GPU single-launch claim"),
    }
    fs::set_permissions(&claim, std::fs::Permissions::from_mode(0o700))?;
    let evidence = format!(
        "PHYSICAL_GPU_LAUNCH_CLAIM_SCHEMA=1\nBOOT_ID={boot_id}\nPROFILE={}\nBDF={bdf}\nRESET_RECOVERY=cold-boot-required\n",
        profile.id
    );
    let evidence_path = claim.join("claim.env");
    fs::write(&evidence_path, evidence)?;
    fs::set_permissions(&evidence_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn validate_physical_gpu_inputs(options: &SmokeOptions) -> Result<()> {
    let bdf = canonical_pci_bdf(
        options
            .physical_gpu_bdf
            .as_deref()
            .context("physical GPU BDF is missing")?,
    )?;
    let profile = selected_physical_gpu_profile(options)?;
    let device = Path::new("/sys/bus/pci/devices").join(&bdf);
    let vendor = fs::read_to_string(device.join("vendor"))?;
    let device_id = fs::read_to_string(device.join("device"))?;
    let driver = std::fs::canonicalize(device.join("driver"))?;
    if driver.file_name() != Some(OsStr::new("vfio-pci")) {
        bail!(
            "physical GPU profile {} must be pre-bound to vfio-pci: vendor={} device={} driver={}",
            profile.id,
            vendor.trim(),
            device_id.trim(),
            driver.display()
        );
    }
    let group = std::fs::canonicalize(device.join("iommu_group"))?;
    let group_id = group
        .file_name()
        .and_then(OsStr::to_str)
        .context("physical GPU IOMMU group has no numeric name")?;
    group_id
        .parse::<u32>()
        .context("physical GPU IOMMU group name is not numeric")?;
    let mut members = std::fs::read_dir(group.join("devices"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    members.sort();
    if members != [bdf.clone()] {
        bail!(
            "physical GPU lab target must be the sole IOMMU-group member: {}",
            members.join(",")
        );
    }
    let reset_methods = fs::read_to_string(device.join("reset_method"))?;
    if !reset_methods.trim().is_empty() {
        bail!(
            "physical GPU lab mode requires reset_method disabled so QEMU cannot bus-reset outside group: {}",
            reset_methods.trim()
        );
    }
    if fs::read_to_string("/sys/module/vfio_pci/parameters/disable_idle_d3")?.trim() != "Y" {
        bail!("physical GPU lab mode requires vfio-pci disable_idle_d3=Y");
    }
    let mut config = std::fs::OpenOptions::new()
        .read(true)
        .open(device.join("config"))?;
    config.seek(SeekFrom::Start(4))?;
    let mut command = [0_u8; 2];
    config.read_exact(&mut command)?;
    if u16::from_le_bytes(command) & 0x4 != 0 {
        bail!("physical GPU lab target still has PCI bus mastering enabled");
    }
    require_direct_rw_character_device(Path::new("/dev/iommu"), "IOMMUFD")?;
    let vfio_cdev = vfio_device_cdev_path(&device)?;
    require_direct_rw_character_device(&vfio_cdev, "VFIO device cdev")?;
    let soft_memlock = physical_memlock_soft_limit()?;
    if soft_memlock.is_some_and(|bytes| bytes < PHYSICAL_GPU_REQUIRED_MEMLOCK) {
        if options.dry_run {
            eprintln!(
                "xtask: physical GPU dry-run warning: inherited memlock is below 4 GiB; the real command will fail before QEMU"
            );
        } else {
            bail!(
                "physical GPU QEMU requires inherited memlock >= 4 GiB (observed {} bytes)",
                soft_memlock.unwrap_or(0)
            );
        }
    }
    let owner = std::fs::metadata("/proc/self")?.uid();
    match profile.firmware_kind {
        PhysicalGpuFirmwareKind::AmdVfct => {
            validate_lab_amd_vfct(
                options
                    .physical_gpu_firmware
                    .as_deref()
                    .context("AMD physical GPU profile requires a VFCT")?,
                owner,
            )?;
        }
    }
    let boot_vga = fs::read_to_string(device.join("boot_vga"))?;
    if !matches!(boot_vga.trim(), "0" | "1") {
        bail!("physical AMD boot_vga state is malformed");
    }
    eprintln!(
        "xtask: NON-COMMERCIAL physical GPU lab mode profile={} target={bdf} group={group_id} boot_vga={} binding/reset are operator-owned",
        profile.id,
        boot_vga.trim()
    );
    Ok(())
}

fn require_host_render_node() -> Result<PathBuf> {
    let mut amdgpu_nodes = Vec::new();
    for entry in std::fs::read_dir("/dev/dri").context("missing host DRM device directory")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("renderD") {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
            bail!(
                "{} must be a direct character-device render node",
                path.display()
            );
        }
        let sysfs = Path::new("/sys/class/drm").join(name).join("device");
        let vendor = std::fs::read_to_string(sysfs.join("vendor"))
            .with_context(|| format!("missing vendor identity for {}", path.display()))?;
        let driver = std::fs::canonicalize(sysfs.join("driver"))
            .with_context(|| format!("missing driver identity for {}", path.display()))?;
        if vendor.trim() == "0x1002" && driver.file_name() == Some(OsStr::new("amdgpu")) {
            amdgpu_nodes.push(path);
        }
    }
    amdgpu_nodes.sort();
    let render_node = match amdgpu_nodes.as_slice() {
        [render_node] => render_node.clone(),
        [] => bail!("KVM virgl requires exactly one AMDGPU render node; found none"),
        nodes => bail!(
            "KVM virgl requires exactly one AMDGPU render node; found {}",
            nodes
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_node)
        .with_context(|| {
            format!(
                "KVM virgl requires read/write access to {}",
                render_node.display()
            )
        })?;
    Ok(render_node)
}

fn mesa_dri_prime_for_render_node(render_node: &Path) -> Result<String> {
    let name = render_node
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.starts_with("renderD"))
        .with_context(|| {
            format!(
                "validated host render node has an invalid name: {}",
                render_node.display()
            )
        })?;
    let device = std::fs::canonicalize(Path::new("/sys/class/drm").join(name).join("device"))
        .with_context(|| format!("resolve PCI identity for {}", render_node.display()))?;
    let bdf = device
        .file_name()
        .and_then(OsStr::to_str)
        .context("host render node sysfs target has no PCI BDF")?;
    mesa_dri_prime_for_pci_bdf(bdf)
}

fn mesa_dri_prime_for_pci_bdf(bdf: &str) -> Result<String> {
    let bdf = canonical_pci_bdf(bdf)?;
    Ok(format!(
        "pci-{}",
        bdf.chars()
            .map(|character| match character {
                ':' | '.' => '_',
                other => other,
            })
            .collect::<String>()
    ))
}

fn require_vhost_vsock() -> Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(VHOST_VSOCK_DEVICE)
        .with_context(|| {
            format!(
                "KVM DVM control requires read/write access to {VHOST_VSOCK_DEVICE}; grant the launch user access before running kvm-smoke"
            )
        })?;
    Ok(())
}

fn start_dvm_input_relay(
    config: &Config,
    timeout: Duration,
    guest_cid: u32,
    input_doorbell: PathBuf,
    input_ring: PathBuf,
    control_secret_path: PathBuf,
    gate: Arc<AtomicBool>,
) -> Result<Receiver<Result<ProbeResult>>> {
    let contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    let contract = HostControlContract::from_env_file(&contract_path)?;
    let control_secret = ControlSecret::from_hex_file(&control_secret_path)?;
    let listener = HostControlListener::bind(guest_cid, contract, control_secret)?;
    // Preserve both the one-time readiness proof and a terminal relay error.
    // A one-slot channel can otherwise drop an error that races immediately
    // after readiness, turning a broken input stream into an unrelated timeout.
    let (sender, receiver) = mpsc::sync_channel(2);
    thread::spawn(move || {
        let sender_ready = sender.clone();
        let result = (|| {
            wait_for_input_relay_gate(&gate, timeout)?;
            let mut sink = InputRingSink::connect(&input_doorbell, &input_ring, timeout)?;
            listener.relay_input_once_with_ready(
                timeout,
                DVM_INPUT_POLICY_READY_TIMEOUT,
                &mut sink,
                |probe| {
                    sender_ready
                        .send(Ok(probe.clone()))
                        .context("report Linux DVM input relay readiness")
                },
            )
        })();
        if let Err(error) = result {
            let _ = sender.try_send(Err(error));
        }
    });
    Ok(receiver)
}

fn start_dvm_input_relay_unbounded(
    config: &Config,
    guest_cid: u32,
    input_doorbell: PathBuf,
    input_ring: PathBuf,
    control_secret_path: PathBuf,
    gate: Arc<AtomicBool>,
) -> Result<()> {
    let contract_path = dvm_dir(config).join(DVM_CONTROL_CONTRACT);
    let contract = HostControlContract::from_env_file(&contract_path)?;
    let control_secret = ControlSecret::from_hex_file(&control_secret_path)?;
    let listener = HostControlListener::bind(guest_cid, contract, control_secret)?;
    thread::spawn(move || {
        while !gate.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        // The input ivshmem broker deliberately tears down the whole fixed
        // topology when either peer disconnects. Keep peer 1 alive across
        // bounded vsock setup failures; otherwise a DVM that becomes ready
        // just after the first ten-second accept deadline can never reconnect.
        let mut sink = loop {
            match InputRingSink::connect(&input_doorbell, &input_ring, Duration::from_secs(1)) {
                Ok(sink) => break sink,
                Err(error) => {
                    eprintln!(
                        "xtask: interactive DVM input transport not ready; retrying: {error:#}"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
            }
        };
        loop {
            if let Err(error) = listener.relay_input_once_unbounded(&mut sink) {
                eprintln!("xtask: interactive DVM input relay disconnected: {error:#}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    });
    Ok(())
}

fn wait_for_input_relay_gate(gate: &AtomicBool, timeout: Duration) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("input relay gate deadline overflow")?;
    while !gate.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            bail!("RustOS did not claim the fixed input ivshmem peer before deadline");
        }
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// The doorbell server owns the backing FD for the entire paired launch. It is
/// started before either QEMU process; `spawn_guests` then observes the RustOS
/// connection as peer 0 before it starts the GUI DVM, which becomes peer 1.
/// The device contract never lets either guest select an ID.
fn start_dvm_display_doorbell(layout: &KvmLayout) -> Result<Option<IvshmemDoorbellServer>> {
    let (Some(shared_display), Some(doorbell)) = (
        layout.gui_dvm_surfaces.as_deref(),
        layout.dvm_display_doorbell.as_deref(),
    ) else {
        return Ok(None);
    };
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(shared_display)
        .with_context(|| format!("open DVM display backing {}", shared_display.display()))?;
    Ok(Some(IvshmemDoorbellServer::start(doorbell, &backing)?))
}

fn start_dvm_block_doorbell(layout: &KvmLayout) -> Result<Option<IvshmemDoorbellServer>> {
    let (Some(aperture), Some(doorbell)) = (
        layout.dvm_block_aperture.as_deref(),
        layout.dvm_block_doorbell.as_deref(),
    ) else {
        return Ok(None);
    };
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(aperture)
        .with_context(|| format!("open DVM block aperture {}", aperture.display()))?;
    Ok(Some(IvshmemDoorbellServer::start_single_vector(
        doorbell, &backing,
    )?))
}

/// The L0 input producer is the second fixed ivshmem peer. It is started
/// before RustOS, but not connected until `spawn_guests` proves that RustOS
/// claimed peer 0. The DVM itself never receives this aperture or a doorbell.
fn start_dvm_input_doorbell(layout: &KvmLayout) -> Result<IvshmemDoorbellServer> {
    let backing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&layout.dvm_input_ring)
        .with_context(|| {
            format!(
                "open DVM input-ring backing {}",
                layout.dvm_input_ring.display()
            )
        })?;
    IvshmemDoorbellServer::start_input(&layout.dvm_input_doorbell, &backing)
}
