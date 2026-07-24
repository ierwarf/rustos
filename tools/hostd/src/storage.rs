use std::fmt::Write as _;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use driver_domain_protocol::{
    DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_DISCARD, DVM_BLOCK_FEATURE_FLUSH,
    DVM_BLOCK_FEATURE_FUA, DVM_BLOCK_FEATURE_WRITE_ZEROES, DVM_BLOCK_FEATURE_WRITEBACK,
    DVM_BLOCK_FLAG_READ_ONLY, DVM_BLOCK_HEADER_RECORD_BYTES, DvmBlockHeader,
};
use ed25519_dalek::{Signer, SigningKey};
use rustos_driver_domain_host::{
    PhysicalStoragePolicy, ValidatedLease, validate_physical_storage_assignment,
};
use sha2::{Digest, Sha256};

const BLKROGET: libc::c_ulong = 0x125e;
const BLKFLSBUF: libc::c_ulong = 0x1261;
const BLKSSZGET: libc::c_ulong = 0x1268;
const BLKPBSZGET: libc::c_ulong = 0x127b;
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
const ZERO_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageHandoffEvidence {
    pub controller_bdf: String,
    pub block_name: String,
    pub aperture_path: PathBuf,
    pub header: DvmBlockHeader,
}

pub struct StorageHandoffGuard {
    device: File,
    aperture: File,
    evidence: StorageHandoffEvidence,
    committed: bool,
}

impl StorageHandoffGuard {
    pub fn evidence(&self) -> &StorageHandoffEvidence {
        &self.evidence
    }

    pub fn commit(mut self) -> StorageHandoffEvidence {
        self.committed = true;
        self.evidence.clone()
    }
}

impl Drop for StorageHandoffGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut revoked = self.evidence.header;
        revoked.flags = 0;
        revoked.request_producer = 0;
        revoked.request_consumer = 0;
        revoked.completion_producer = 0;
        revoked.completion_consumer = 0;
        let _ = self.aperture.write_all_at(&revoked.encode(), 0);
        let _ = self.aperture.sync_all();
        let _ = unsafe { libc::flock(self.device.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn prepare_storage_handoff(
    lease: &ValidatedLease,
    sysfs_root: &Path,
    device_root: &Path,
    proc_root: &Path,
    policy: &PhysicalStoragePolicy,
    aperture_path: &Path,
    epoch_signing_key: &SigningKey,
) -> Result<StorageHandoffGuard> {
    let controller_bdf = validate_physical_storage_assignment(lease, sysfs_root, policy)?;
    let block_name = discover_whole_block_device(sysfs_root, &controller_bdf)?;
    validate_block_device_idle(sysfs_root, proc_root, device_root, &block_name)?;
    let node = device_root.join(&block_name);
    let device = open_exclusive_block_device(&node)?;
    let geometry = read_block_geometry(&device, sysfs_root, &block_name)?;
    flush_block_device_bounded(&device, Duration::from_millis(policy.handoff_timeout_ms()))?;
    let (aperture, aperture_path, generation) = prepare_aperture(aperture_path)?;
    let mut header = DvmBlockHeader::new(
        generation,
        geometry.capacity_sectors,
        geometry.logical_block_size,
        geometry.physical_block_size,
        geometry.features,
    );
    if geometry.read_only {
        header.flags |= DVM_BLOCK_FLAG_READ_ONLY;
    }
    header = header.with_epoch_signature(
        epoch_signing_key
            .sign(&header.epoch_signing_bytes())
            .to_bytes(),
    );
    if !header.is_valid() {
        bail!("derived physical storage geometry does not satisfy the block transport ABI");
    }
    initialize_aperture(&aperture, header)?;
    Ok(StorageHandoffGuard {
        device,
        aperture,
        evidence: StorageHandoffEvidence {
            controller_bdf,
            block_name,
            aperture_path,
            header,
        },
        committed: false,
    })
}

pub fn load_storage_epoch_signing_key(path: &Path) -> Result<SigningKey> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open storage epoch signing key {}", path.display()))?;
    let metadata = file
        .metadata()
        .context("inspect opened storage epoch signing key")?;
    if !metadata.file_type().is_file() {
        bail!("storage epoch signing key must be a regular non-symlink file");
    }
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() != 32
    {
        bail!(
            "storage epoch signing key must be caller-owned, mode 0600 or stricter, and exactly 32 bytes"
        );
    }
    let mut seed = [0_u8; 32];
    file.read_exact_at(&mut seed, 0)?;
    if seed.iter().all(|byte| *byte == 0) {
        bail!("storage epoch signing key must not be the all-zero seed");
    }
    let key = SigningKey::from_bytes(&seed);
    seed.fill(0);
    Ok(key)
}

pub fn storage_epoch_verifying_key_sha256(key: &SigningKey) -> String {
    let digest = Sha256::digest(key.verifying_key().as_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

pub fn revoke_storage_aperture(path: &Path, expected_generation: u64) -> Result<()> {
    let owner = unsafe { libc::geteuid() };
    let (file, canonical) = open_private_aperture(path, false)?;
    let header = read_aperture_header(&file)?.ok_or_else(|| {
        anyhow!(
            "storage aperture {} has no valid live header",
            canonical.display()
        )
    })?;
    if header.generation != expected_generation {
        bail!(
            "storage aperture generation changed expected={} actual={}",
            expected_generation,
            header.generation
        );
    }
    let metadata = file.metadata()?;
    if metadata.uid() != owner {
        bail!("storage aperture owner changed before revocation");
    }
    let mut revoked = header;
    revoked.flags = 0;
    revoked.request_producer = 0;
    revoked.request_consumer = 0;
    revoked.completion_producer = 0;
    revoked.completion_consumer = 0;
    file.write_all_at(&revoked.encode(), 0)?;
    file.sync_all()
        .context("persist storage aperture revocation")
}

pub fn inspect_storage_aperture(path: &Path, expected_generation: u64) -> Result<DvmBlockHeader> {
    let (file, canonical) = open_private_aperture(path, false)?;
    let header = read_aperture_header(&file)?.ok_or_else(|| {
        anyhow!(
            "storage aperture {} has no valid header",
            canonical.display()
        )
    })?;
    if header.generation != expected_generation {
        bail!(
            "storage aperture generation changed expected={} actual={}",
            expected_generation,
            header.generation
        );
    }
    Ok(header)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BlockGeometry {
    capacity_sectors: u64,
    logical_block_size: u32,
    physical_block_size: u32,
    features: u64,
    read_only: bool,
}

fn discover_whole_block_device(sysfs_root: &Path, controller_bdf: &str) -> Result<String> {
    let controller = fs::canonicalize(sysfs_root.join("bus/pci/devices").join(controller_bdf))
        .with_context(|| format!("canonicalize storage controller {controller_bdf}"))?;
    let mut found = Vec::new();
    let class = sysfs_root.join("class/block");
    for entry in
        fs::read_dir(&class).with_context(|| format!("read block inventory {}", class.display()))?
    {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("non-UTF-8 block device name"))?;
        if name.is_empty()
            || name.contains('/')
            || name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("dm-")
            || entry.path().join("partition").exists()
        {
            continue;
        }
        let device = match fs::canonicalize(entry.path().join("device")) {
            Ok(device) => device,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("resolve block-device controller ancestry"),
        };
        if device.starts_with(&controller) {
            found.push(name);
        }
    }
    found.sort();
    match found.as_slice() {
        [name] => Ok(name.clone()),
        [] => bail!("storage controller {controller_bdf} exposes no whole block device"),
        _ => bail!(
            "storage controller {controller_bdf} exposes ambiguous whole block devices: {}",
            found.join(",")
        ),
    }
}

fn validate_block_device_idle(
    sysfs_root: &Path,
    proc_root: &Path,
    device_root: &Path,
    block_name: &str,
) -> Result<()> {
    let members = block_family_members(sysfs_root, device_root, block_name)?;
    let mountinfo = fs::read_to_string(proc_root.join("self/mountinfo"))
        .context("read host mountinfo before storage handoff")?;
    if mountinfo.lines().any(|line| {
        line.split_ascii_whitespace()
            .nth(2)
            .is_some_and(|value| members.iter().any(|member| member.device_number == value))
    }) {
        bail!("block device {block_name} or one of its partitions is mounted by the L0 host");
    }
    let swaps = fs::read_to_string(proc_root.join("swaps"))
        .context("read host swap inventory before storage handoff")?;
    for line in swaps.lines().skip(1) {
        let Some(path) = line.split_ascii_whitespace().next() else {
            continue;
        };
        if fs::canonicalize(path)
            .is_ok_and(|candidate| members.iter().any(|member| member.node == candidate))
        {
            bail!("block device {block_name} or one of its partitions backs active L0 swap");
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct BlockFamilyMember {
    name: String,
    device_number: String,
    node: PathBuf,
}

fn block_family_members(
    sysfs_root: &Path,
    device_root: &Path,
    block_name: &str,
) -> Result<Vec<BlockFamilyMember>> {
    let class = sysfs_root.join("class/block");
    let whole = class.join(block_name);
    let whole_target = fs::canonicalize(&whole)
        .with_context(|| format!("canonicalize whole block device {block_name}"))?;
    let mut members = Vec::new();
    for entry in
        fs::read_dir(&class).with_context(|| format!("read block inventory {}", class.display()))?
    {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("non-UTF-8 block device name"))?;
        if name.is_empty() || name.contains('/') {
            bail!("invalid block device name in sysfs inventory");
        }
        let target = fs::canonicalize(entry.path())
            .with_context(|| format!("canonicalize block member {name}"))?;
        let is_whole = name == block_name && target == whole_target;
        let is_partition =
            entry.path().join("partition").exists() && target.starts_with(&whole_target);
        if !is_whole && !is_partition {
            continue;
        }
        let device_number = read_trimmed(&entry.path().join("dev"))?;
        validate_device_number(&device_number)?;
        let holders = entry.path().join("holders");
        if fs::read_dir(&holders)
            .with_context(|| format!("read holders for {name}"))?
            .next()
            .transpose()?
            .is_some()
        {
            bail!("block device member {name} has active holders");
        }
        let node = fs::canonicalize(device_root.join(&name))
            .with_context(|| format!("canonicalize block device node {name}"))?;
        members.push(BlockFamilyMember {
            name,
            device_number,
            node,
        });
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    if members
        .first()
        .is_none_or(|member| member.name != block_name)
    {
        bail!("whole block device {block_name} disappeared during idle validation");
    }
    for (index, member) in members.iter().enumerate() {
        if members[index + 1..]
            .iter()
            .any(|candidate| candidate.device_number == member.device_number)
        {
            bail!("block family contains duplicate device numbers");
        }
    }
    Ok(members)
}

fn open_exclusive_block_device(path: &Path) -> Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_EXCL)
        .open(path)
        .with_context(|| format!("open exclusive physical block device {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_block_device() {
        bail!(
            "physical storage node {} is not a block device",
            path.display()
        );
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error()).context("lock physical block device");
    }
    Ok(file)
}

fn read_block_geometry(file: &File, sysfs_root: &Path, name: &str) -> Result<BlockGeometry> {
    let mut bytes = 0_u64;
    let mut logical = 0_u32;
    let mut physical = 0_u32;
    let mut read_only = 0_i32;
    for (request, value, label) in [
        (
            BLKGETSIZE64,
            (&mut bytes as *mut u64).cast::<libc::c_void>(),
            "BLKGETSIZE64",
        ),
        (
            BLKSSZGET,
            (&mut logical as *mut u32).cast::<libc::c_void>(),
            "BLKSSZGET",
        ),
        (
            BLKPBSZGET,
            (&mut physical as *mut u32).cast::<libc::c_void>(),
            "BLKPBSZGET",
        ),
        (
            BLKROGET,
            (&mut read_only as *mut i32).cast::<libc::c_void>(),
            "BLKROGET",
        ),
    ] {
        if unsafe { libc::ioctl(file.as_raw_fd(), request, value) } != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("{label} for {name}"));
        }
    }
    if read_only != 0 {
        bail!("physical storage handoff requires a writable whole block device");
    }
    if bytes == 0 || !bytes.is_multiple_of(512) {
        bail!("invalid physical storage byte capacity");
    }
    let class = sysfs_root.join("class/block").join(name);
    let sysfs_sectors = read_u64(&class.join("size"))?;
    let sysfs_logical = read_u32(&class.join("queue/logical_block_size"))?;
    let sysfs_physical = read_u32(&class.join("queue/physical_block_size"))?;
    if bytes / 512 != sysfs_sectors || logical != sysfs_logical || physical != sysfs_physical {
        bail!("block ioctl geometry changed relative to the admitted sysfs snapshot");
    }
    let mut features = DVM_BLOCK_FEATURE_FLUSH;
    if read_optional_u64(&class.join("queue/discard_max_bytes"))?.unwrap_or(0) != 0 {
        features |= DVM_BLOCK_FEATURE_DISCARD;
    }
    if read_optional_u64(&class.join("queue/write_zeroes_max_bytes"))?.unwrap_or(0) != 0 {
        features |= DVM_BLOCK_FEATURE_WRITE_ZEROES;
    }
    if read_optional_u64(&class.join("queue/fua"))?.unwrap_or(0) == 1 {
        features |= DVM_BLOCK_FEATURE_FUA;
    }
    if fs::read_to_string(class.join("queue/write_cache"))
        .is_ok_and(|value| value.trim().starts_with("write back"))
    {
        features |= DVM_BLOCK_FEATURE_WRITEBACK;
    }
    Ok(BlockGeometry {
        capacity_sectors: bytes / 512,
        logical_block_size: logical,
        physical_block_size: physical,
        features,
        read_only: false,
    })
}

fn flush_block_device_bounded(file: &File, timeout: Duration) -> Result<()> {
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error()).context("fork bounded storage flush");
    }
    if child == 0 {
        let status = unsafe {
            if libc::fsync(file.as_raw_fd()) == 0
                && libc::ioctl(file.as_raw_fd(), BLKFLSBUF, 0) == 0
            {
                0
            } else {
                1
            }
        };
        unsafe { libc::_exit(status) };
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("storage handoff deadline overflow"))?;
    loop {
        let mut status = 0_i32;
        let waited = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if waited == child {
            if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                return Ok(());
            }
            bail!("physical storage FLUSH/BLKFLSBUF failed");
        }
        if waited < 0 {
            return Err(io::Error::last_os_error()).context("wait for storage flush");
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, std::ptr::null_mut(), 0);
            }
            bail!("physical storage flush exceeded signed handoff deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn prepare_aperture(path: &Path) -> Result<(File, PathBuf, u64)> {
    let (file, canonical) = open_private_aperture(path, true)?;
    let generation = match read_aperture_header(&file)? {
        Some(header)
            if header.flags == 0
                && header.request_producer == 0
                && header.request_consumer == 0
                && header.completion_producer == 0
                && header.completion_consumer == 0 =>
        {
            header
                .generation
                .checked_add(1)
                .ok_or_else(|| anyhow!("storage aperture generation exhausted"))?
        }
        Some(_) => bail!("storage aperture retains an active or dirty transport epoch"),
        None => {
            let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
            file.read_exact_at(&mut bytes, 0)?;
            if bytes.iter().any(|byte| *byte != 0) {
                bail!("storage aperture contains an unrecognized nonzero header");
            }
            1
        }
    };
    Ok((file, canonical, generation))
}

fn open_private_aperture(path: &Path, create: bool) -> Result<(File, PathBuf)> {
    let owner = unsafe { libc::geteuid() };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("storage aperture has no parent directory"))?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("canonicalize storage aperture parent {}", parent.display()))?;
    let metadata = fs::symlink_metadata(&parent)?;
    if !metadata.file_type().is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        bail!("unsafe storage aperture parent {}", parent.display());
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("storage aperture has no file name"))?;
    let canonical_candidate = parent.join(name);
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = if create {
        match options.create_new(true).open(&canonical_candidate) {
            Ok(file) => {
                file.set_len(DVM_BLOCK_APERTURE_BYTES)?;
                file.sync_all()?;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                options.create_new(false);
                options.open(&canonical_candidate)?
            }
            Err(error) => return Err(error).context("create private storage aperture"),
        }
    } else {
        options.open(&canonical_candidate)?
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.len() != DVM_BLOCK_APERTURE_BYTES
    {
        bail!("unsafe or incorrectly sized storage aperture");
    }
    Ok((file, canonical_candidate))
}

fn initialize_aperture(file: &File, header: DvmBlockHeader) -> Result<()> {
    let zeros = [0_u8; ZERO_CHUNK_BYTES];
    let mut offset = 0_u64;
    while offset < DVM_BLOCK_APERTURE_BYTES {
        let bytes = usize::try_from((DVM_BLOCK_APERTURE_BYTES - offset).min(zeros.len() as u64))
            .expect("bounded aperture chunk fits usize");
        file.write_all_at(&zeros[..bytes], offset)?;
        offset += bytes as u64;
    }
    file.write_all_at(&header.encode(), 0)?;
    file.sync_all()
        .context("persist initialized storage DVM aperture")
}

fn read_aperture_header(file: &File) -> Result<Option<DvmBlockHeader>> {
    let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(DvmBlockHeader::decode(&bytes))
}

fn read_trimmed(path: &Path) -> Result<String> {
    let value = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(value.trim().to_owned())
}

fn read_u64(path: &Path) -> Result<u64> {
    read_trimmed(path)?
        .parse()
        .with_context(|| format!("parse {}", path.display()))
}

fn read_u32(path: &Path) -> Result<u32> {
    read_trimmed(path)?
        .parse()
        .with_context(|| format!("parse {}", path.display()))
}

fn read_optional_u64(path: &Path) -> Result<Option<u64>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(
            value
                .trim()
                .parse()
                .with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn validate_device_number(value: &str) -> Result<()> {
    let Some((major, minor)) = value.split_once(':') else {
        bail!("invalid block device number");
    };
    if major.is_empty()
        || minor.is_empty()
        || !major.bytes().all(|byte| byte.is_ascii_digit())
        || !minor.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("invalid block device number");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        initialize_aperture, prepare_aperture, revoke_storage_aperture, validate_block_device_idle,
    };
    use driver_domain_protocol::{
        DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_HEADER_RECORD_BYTES,
        DvmBlockHeader,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use std::fs;
    use std::os::unix::fs::{FileExt, PermissionsExt, symlink};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn signed_header(generation: u64) -> DvmBlockHeader {
        let key = SigningKey::from_bytes(&[0x42; 32]);
        let header = DvmBlockHeader::new(generation, 4096, 512, 4096, DVM_BLOCK_FEATURE_FLUSH);
        header.with_epoch_signature(key.sign(&header.epoch_signing_bytes()).to_bytes())
    }

    #[test]
    fn aperture_epochs_are_clean_monotonic_and_revocable() {
        let root = std::env::temp_dir().join(format!(
            "rustos-hostd-storage-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("block.ivshmem");
        let (file, canonical, generation) = prepare_aperture(&path).unwrap();
        assert_eq!(generation, 1);
        let header = signed_header(1);
        initialize_aperture(&file, header).unwrap();
        drop(file);

        let (file, _, generation) = prepare_aperture(&path).unwrap();
        assert_eq!(generation, 2);
        let next = signed_header(2);
        initialize_aperture(&file, next).unwrap();
        drop(file);
        revoke_storage_aperture(&canonical, 2).unwrap();
        let file = fs::OpenOptions::new().read(true).open(&canonical).unwrap();
        let mut record = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
        file.read_exact_at(&mut record, 0).unwrap();
        let revoked = DvmBlockHeader::decode(&record).unwrap();
        assert_eq!(revoked.generation, 2);
        assert_eq!(revoked.flags, 0);
        assert_eq!(file.metadata().unwrap().len(), DVM_BLOCK_APERTURE_BYTES);
        drop(file);
        let (_, _, generation) = prepare_aperture(&canonical).unwrap();
        assert_eq!(generation, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idle_validation_covers_every_partition_of_the_whole_device() {
        let root = std::env::temp_dir().join(format!(
            "rustos-hostd-storage-family-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let class = root.join("sys/class/block");
        let whole = root.join("sys/devices/pci0000:00/0000:00:01.0/block/nvme0n1");
        let partition = whole.join("nvme0n1p1");
        let devices = root.join("dev");
        let proc_root = root.join("proc");
        fs::create_dir_all(&class).unwrap();
        fs::create_dir_all(whole.join("holders")).unwrap();
        fs::create_dir_all(partition.join("holders")).unwrap();
        fs::create_dir_all(&devices).unwrap();
        fs::create_dir_all(proc_root.join("self")).unwrap();
        fs::write(whole.join("dev"), "259:0\n").unwrap();
        fs::write(partition.join("dev"), "259:1\n").unwrap();
        fs::write(partition.join("partition"), "1\n").unwrap();
        fs::write(devices.join("nvme0n1"), "").unwrap();
        fs::write(devices.join("nvme0n1p1"), "").unwrap();
        symlink(&whole, class.join("nvme0n1")).unwrap();
        symlink(&partition, class.join("nvme0n1p1")).unwrap();
        fs::write(
            proc_root.join("swaps"),
            "Filename Type Size Used Priority\n",
        )
        .unwrap();
        fs::write(
            proc_root.join("self/mountinfo"),
            "36 25 259:1 / /mnt rw,relatime - ext4 /dev/nvme0n1p1 rw\n",
        )
        .unwrap();

        assert!(
            validate_block_device_idle(&root.join("sys"), &proc_root, &devices, "nvme0n1")
                .unwrap_err()
                .to_string()
                .contains("partitions is mounted")
        );
        fs::write(proc_root.join("self/mountinfo"), "").unwrap();
        validate_block_device_idle(&root.join("sys"), &proc_root, &devices, "nvme0n1").unwrap();

        fs::write(partition.join("holders/dm-0"), "").unwrap();
        assert!(
            validate_block_device_idle(&root.join("sys"), &proc_root, &devices, "nvme0n1")
                .unwrap_err()
                .to_string()
                .contains("active holders")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
