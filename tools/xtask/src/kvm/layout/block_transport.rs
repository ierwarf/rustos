/// Capacity, in 512-byte protocol sectors, of a private storage-DVM disk.
///
/// The image must be non-empty and a whole number of the declared logical
/// blocks (`DVM_STORAGE_LOGICAL_BLOCK_BYTES`): QEMU would otherwise round the
/// capacity the guest sees, and the relay would reject the signed header.
fn dvm_storage_capacity_sectors(disk_bytes: u64) -> Result<u64> {
    if disk_bytes == 0 || !disk_bytes.is_multiple_of(u64::from(DVM_STORAGE_LOGICAL_BLOCK_BYTES)) {
        bail!(
            "private storage-DVM disk must be non-empty and a whole number of {DVM_STORAGE_LOGICAL_BLOCK_BYTES}-byte logical blocks"
        );
    }
    Ok(disk_bytes / 512)
}

/// True when a live header describes exactly the declared storage geometry over
/// a backing disk of `disk_bytes`.
fn dvm_storage_header_geometry_matches(header: &DvmBlockHeader, disk_bytes: u64) -> bool {
    dvm_storage_capacity_sectors(disk_bytes)
        .is_ok_and(|sectors| header.capacity_sectors == sectors)
        && header.logical_block_size == DVM_STORAGE_LOGICAL_BLOCK_BYTES
        && header.physical_block_size == DVM_STORAGE_PHYSICAL_BLOCK_BYTES
}

pub(crate) fn dvm_writable_block_header(
    generation: u64,
    capacity_sectors: u64,
    logical_block_size: u32,
    physical_block_size: u32,
    features: u64,
) -> DvmBlockHeader {
    let mut header = DvmBlockHeader::new(
        generation,
        capacity_sectors,
        logical_block_size,
        physical_block_size,
        features | DVM_BLOCK_FEATURE_FLUSH | driver_domain_protocol::DVM_BLOCK_FEATURE_FUA,
    );
    header.flags = 0;
    header
}

fn write_signed_dvm_block_aperture(
    path: &Path,
    disk: &Path,
    signing_key_path: &Path,
    unsigned_header: DvmBlockHeader,
) -> Result<()> {
    for candidate in [path, disk] {
        if candidate.to_string_lossy().contains(',') {
            bail!(
                "KVM storage-DVM path contains an unsupported QEMU option separator: {}",
                candidate.display()
            );
        }
    }
    let signing_key = crate::storage_epoch::load_signing_key(signing_key_path)?;
    let header = crate::storage_epoch::sign_epoch(&signing_key, unsigned_header);
    if !header.is_valid() {
        bail!("refusing to create invalid fixed DVM block header");
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).read(true).write(true);
    // Private from creation: set_permissions below would leave a umask-wide
    // window for a newly created aperture.
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("create DVM block aperture {}", path.display()))?;
    file.set_len(DVM_BLOCK_APERTURE_BYTES)
        .with_context(|| format!("size DVM block aperture {}", path.display()))?;
    file.write_all(&header.encode())
        .with_context(|| format!("write DVM block header {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync DVM block aperture {}", path.display()))?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub(crate) fn create_writable_dvm_block_aperture(
    path: &Path,
    disk: &Path,
    signing_key_path: &Path,
) -> Result<()> {
    let disk_bytes = fs::metadata(disk)
        .with_context(|| format!("inspect private storage-DVM disk {}", disk.display()))?
        .len();
    let capacity_sectors = dvm_storage_capacity_sectors(disk_bytes)
        .context("private writable storage-DVM disk geometry")?;
    write_signed_dvm_block_aperture(
        path,
        disk,
        signing_key_path,
        dvm_writable_block_header(
            1,
            capacity_sectors,
            DVM_STORAGE_LOGICAL_BLOCK_BYTES,
            DVM_STORAGE_PHYSICAL_BLOCK_BYTES,
            DVM_BLOCK_FEATURE_FLUSH | driver_domain_protocol::DVM_BLOCK_FEATURE_FUA,
        ),
    )
}

fn dvm_read_only_block_header(
    generation: u64,
    capacity_sectors: u64,
    logical_block_size: u32,
    physical_block_size: u32,
    features: u64,
) -> DvmBlockHeader {
    let mut header = DvmBlockHeader::new(
        generation,
        capacity_sectors,
        logical_block_size,
        physical_block_size,
        features,
    );
    // The signed transport contract must describe the actual QEMU block
    // device mode (`readonly=on` surfaces as the virtio-blk read-only feature
    // and Linux `BLKROGET`). The Linux relay rejects a read-only device paired
    // with a writable header, and a writable device paired with a read-only
    // header, before it publishes DVM readiness; it never opens a read-only
    // transport's device writable.
    header.flags = DVM_BLOCK_FLAG_READ_ONLY;
    header
}

fn sync_private_dvm_block_snapshot(disk: &Path, directory: &Path) -> Result<()> {
    std::fs::File::open(disk)
        .with_context(|| format!("open private storage-DVM snapshot {}", disk.display()))?
        .sync_all()
        .with_context(|| format!("sync private storage-DVM snapshot {}", disk.display()))?;
    std::fs::File::open(directory)
        .with_context(|| {
            format!(
                "open private storage-DVM snapshot directory {}",
                directory.display()
            )
        })?
        .sync_all()
        .with_context(|| {
            format!(
                "sync private storage-DVM snapshot directory {}",
                directory.display()
            )
        })?;
    Ok(())
}

fn create_dvm_block_aperture(path: &Path, disk: &Path, signing_key_path: &Path) -> Result<()> {
    let disk_bytes = fs::metadata(disk)
        .with_context(|| format!("inspect private storage-DVM disk {}", disk.display()))?
        .len();
    let capacity_sectors = dvm_storage_capacity_sectors(disk_bytes)
        .context("private storage-DVM disk geometry")?;
    write_signed_dvm_block_aperture(
        path,
        disk,
        signing_key_path,
        dvm_read_only_block_header(
            1,
            capacity_sectors,
            DVM_STORAGE_LOGICAL_BLOCK_BYTES,
            DVM_STORAGE_PHYSICAL_BLOCK_BYTES,
            DVM_BLOCK_MEDIA_FEATURES,
        ),
    )
}

/// Publish a new L0-authorized block transport epoch without replacing the
/// backing file while RustOS still maps it.
///
/// The caller must first prove that the DVM peer has exited. RustOS may retain
/// the mapping, but it will reject the changed generation, revoke every
/// predecessor request, and admit this zero-cursor epoch only after verifying
/// its signature. This is the storage equivalent of check-revoke-rebind; it
/// never teaches either guest to accept the predecessor's mutable ring state.
fn rotate_dvm_block_epoch(path: &Path, disk: &Path, signing_key_path: &Path) -> Result<u64> {
    let disk_bytes = fs::metadata(disk)
        .with_context(|| format!("inspect private storage-DVM disk {}", disk.display()))?
        .len();
    dvm_storage_capacity_sectors(disk_bytes).context("private storage-DVM disk geometry")?;
    let signing_key = crate::storage_epoch::load_signing_key(signing_key_path)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open live DVM block aperture {}", path.display()))?;
    let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
    file.read_exact(&mut bytes)
        .with_context(|| format!("read live DVM block header {}", path.display()))?;
    let predecessor = DvmBlockHeader::decode(&bytes)
        .context("live DVM block aperture contains an invalid predecessor header")?;
    let expected_signature =
        crate::storage_epoch::sign_epoch(&signing_key, predecessor.with_epoch_signature([0; 64]))
            .epoch_signature;
    if predecessor.epoch_signature != expected_signature {
        bail!("refusing to rotate a DVM block epoch not authorized by this L0");
    }
    if predecessor.flags & DVM_BLOCK_FLAG_READ_ONLY == 0 {
        bail!("refusing to rotate a writable DVM block transport epoch");
    }
    if !dvm_storage_header_geometry_matches(&predecessor, disk_bytes) {
        bail!("live DVM block epoch geometry diverged from its private backing disk");
    }
    let generation = predecessor
        .generation
        .checked_add(1)
        .context("DVM block transport generation exhausted")?;
    let successor = crate::storage_epoch::sign_epoch(
        &signing_key,
        dvm_read_only_block_header(
            generation,
            predecessor.capacity_sectors,
            predecessor.logical_block_size,
            predecessor.physical_block_size,
            predecessor.features,
        ),
    );
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&successor.encode())
        .with_context(|| format!("publish successor DVM block header {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync successor DVM block header {}", path.display()))?;
    Ok(generation)
}
