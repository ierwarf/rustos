//! Persistent physical-device reservation and process serialization.
use anyhow::{Context, Result, bail};
use rustos_driver_domain_host::ValidatedLease;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/var/lib/rustos-hostd";
pub const LEASES: &str = "/var/lib/rustos-hostd/leases";
pub const DOMAINS: &str = "/var/lib/rustos-hostd/domains";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Reserved,
    Mutating,
    Reset,
    Complete,
}
impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Mutating => "mutating",
            Self::Reset => "reset",
            Self::Complete => "complete",
        }
    }
}

pub struct Authority {
    root: PathBuf,
}
pub struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn validate_ancestors(path: &Path, test_root: bool) -> Result<()> {
    #[cfg(not(test))]
    if test_root {
        bail!("injected authority roots are test-only");
    }
    if test_root {
        return Ok(());
    }
    if unsafe { libc::geteuid() } != 0 {
        bail!("physical authority requires root");
    }
    if fs::canonicalize(path)? != path {
        bail!("physical authority path must be canonical");
    }
    for ancestor in path.ancestors() {
        let m = fs::symlink_metadata(ancestor)?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            bail!("unsafe physical authority ancestor {}", ancestor.display());
        }
    }
    Ok(())
}

fn validate_filesystem(path: &Path, test_root: bool) -> Result<()> {
    let file = File::open(path)?;
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut info) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("inspect physical authority filesystem");
    }
    // Explicitly allow local disk filesystems only; overlay and unknown types fail closed.
    if !test_root && !matches!(info.f_type as u64, 0xef53 | 0x5846_5342 | 0x9123_683e) {
        bail!("physical authority filesystem is not an admitted persistent filesystem");
    }
    Ok(())
}

impl Authority {
    pub fn provision(root: &Path) -> Result<Self> {
        Self::provision_checked(root, false)
    }
    fn provision_checked(root: &Path, test_root: bool) -> Result<Self> {
        let parent = root.parent().context("authority has no parent")?;
        validate_ancestors(parent, test_root)?;
        fs::create_dir(root).context("authority already exists or cannot be provisioned")?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        validate_filesystem(root, test_root)?;
        for name in ["quarantine", "locks", "leases", "domains"] {
            fs::create_dir(root.join(name))?;
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700))?;
        }
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("authority"))?;
        marker.write_all(b"RUSTOS_HOSTD_AUTHORITY=1\n")?;
        marker.sync_all()?;
        File::open(root)?.sync_all()?;
        File::open(parent)?.sync_all()?;
        Self::open_checked(root, test_root)
    }
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_checked(root, false)
    }
    fn open_checked(root: &Path, test_root: bool) -> Result<Self> {
        validate_ancestors(root, test_root)?;
        validate_filesystem(root, test_root)?;
        let root_metadata = fs::symlink_metadata(root)?;
        if !root_metadata.is_dir()
            || root_metadata.uid() != unsafe { libc::geteuid() }
            || root_metadata.mode() & 0o077 != 0
        {
            bail!("unsafe physical authority root");
        }
        let marker_path = root.join("authority");
        let metadata = fs::symlink_metadata(&marker_path)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.dev() != root_metadata.dev()
            || fs::read(&marker_path)? != b"RUSTOS_HOSTD_AUTHORITY=1\n"
        {
            bail!("invalid or unsafe physical authority marker");
        }
        for name in ["quarantine", "locks", "leases", "domains"] {
            let metadata = fs::symlink_metadata(root.join(name))?;
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || metadata.dev() != root_metadata.dev()
            {
                bail!("missing or unsafe physical authority directory {name}");
            }
        }
        Ok(Self {
            root: root.to_owned(),
        })
    }
    pub fn lock(&self, name: &str) -> Result<Lock> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            bail!("invalid authority lock name");
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.root.join("locks").join(name))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error()).context("physical authority is busy");
        }
        Ok(Lock(file))
    }
    fn path(&self, domain: &str) -> Result<PathBuf> {
        if domain.is_empty()
            || !domain
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            bail!("invalid quarantine domain");
        }
        Ok(self.root.join("quarantine").join(format!("{domain}.env")))
    }
    fn read(&self, path: &Path) -> Result<(String, u32, BTreeSet<String>, Phase)> {
        let m = fs::symlink_metadata(path)?;
        if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
            bail!("unsafe quarantine record");
        }
        let s = fs::read_to_string(path)?;
        let mut lines = s.lines();
        if lines.next() != Some("QUARANTINE_SCHEMA=1") {
            bail!("invalid quarantine schema");
        }
        let domain = lines
            .next()
            .and_then(|s| s.strip_prefix("DOMAIN="))
            .context("quarantine domain")?
            .to_owned();
        self.path(&domain)?;
        let group = lines
            .next()
            .and_then(|s| s.strip_prefix("IOMMU_GROUP="))
            .context("quarantine group")?
            .parse()?;
        let bdfs: BTreeSet<String> = lines
            .next()
            .and_then(|s| s.strip_prefix("PCI_BDFS="))
            .context("quarantine devices")?
            .split(',')
            .map(str::to_owned)
            .collect();
        if bdfs.is_empty()
            || bdfs.iter().any(|s| {
                s.len() != 12
                    || !s
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.')
            })
        {
            bail!("invalid quarantine devices");
        }
        let phase = match lines.next() {
            Some("STATE=reserved") => Phase::Reserved,
            Some("STATE=mutating") => Phase::Mutating,
            Some("STATE=reset") => Phase::Reset,
            Some("STATE=complete") => Phase::Complete,
            _ => bail!("invalid quarantine state"),
        };
        if lines.next().is_some() {
            bail!("invalid quarantine record");
        }
        Ok((domain, group, bdfs, phase))
    }
    pub fn reserve(&self, lease: &ValidatedLease) -> Result<()> {
        self.reserve_with_sync(lease, |file| file.sync_all())
    }
    fn reserve_with_sync(
        &self,
        lease: &ValidatedLease,
        sync: impl Fn(&File) -> std::io::Result<()>,
    ) -> Result<()> {
        let requested: BTreeSet<String> = lease.pci_bdfs.iter().cloned().collect();
        for entry in fs::read_dir(self.root.join("quarantine"))? {
            let (domain, group, devices, _) = self.read(&entry?.path())?;
            if domain == lease.domain_id
                || group == lease.iommu_group
                || !requested.is_disjoint(&devices)
            {
                bail!(
                    "physical device quarantined by domain {domain}; recover it before reassignment"
                );
            }
        }
        let path = self.path(&lease.domain_id)?;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        write!(
            f,
            "QUARANTINE_SCHEMA=1\nDOMAIN={}\nIOMMU_GROUP={}\nPCI_BDFS={}\nSTATE=reserved\n",
            lease.domain_id,
            lease.iommu_group,
            lease.pci_bdfs.join(",")
        )?;
        sync(&f)?;
        sync(&File::open(self.root.join("quarantine"))?)?;
        Ok(())
    }
    pub fn require(&self, lease: &ValidatedLease) -> Result<()> {
        let (domain, group, bdfs, _) = self.read(&self.path(&lease.domain_id)?)?;
        if domain != lease.domain_id
            || group != lease.iommu_group
            || bdfs != lease.pci_bdfs.iter().cloned().collect()
        {
            bail!("quarantine identity mismatch");
        }
        Ok(())
    }
    pub fn phase(&self, lease: &ValidatedLease) -> Result<Phase> {
        self.require(lease)?;
        Ok(self.read(&self.path(&lease.domain_id)?)?.3)
    }
    pub fn advance(&self, lease: &ValidatedLease, expected: Phase, next: Phase) -> Result<()> {
        if self.phase(lease)? != expected {
            bail!("physical authority phase changed");
        }
        let valid = matches!(
            (expected, next),
            (Phase::Reserved, Phase::Mutating | Phase::Complete)
                | (Phase::Mutating, Phase::Reset)
                | (Phase::Reset, Phase::Complete)
        );
        if !valid {
            bail!("invalid physical authority phase transition");
        }
        let path = self.path(&lease.domain_id)?;
        let temporary = self
            .root
            .join("quarantine")
            .join(format!(".{}.mutating", lease.domain_id));
        if let Ok(metadata) = fs::symlink_metadata(&temporary) {
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                bail!("unsafe interrupted authority update");
            }
            fs::remove_file(&temporary)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        write!(
            file,
            "QUARANTINE_SCHEMA=1\nDOMAIN={}\nIOMMU_GROUP={}\nPCI_BDFS={}\nSTATE={}\n",
            lease.domain_id,
            lease.iommu_group,
            lease.pci_bdfs.join(","),
            next.as_str()
        )?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        File::open(self.root.join("quarantine"))?.sync_all()?;
        Ok(())
    }
    pub fn mark_mutating(&self, lease: &ValidatedLease) -> Result<()> {
        self.advance(lease, Phase::Reserved, Phase::Mutating)
    }
    pub fn finish_complete(
        &self,
        lease: &ValidatedLease,
        remove_runtime: impl FnOnce() -> Result<()>,
        remove_lease: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.finish_complete_with_hook(lease, remove_runtime, remove_lease, |_| Ok(()))
    }
    fn finish_complete_with_hook(
        &self,
        lease: &ValidatedLease,
        remove_runtime: impl FnOnce() -> Result<()>,
        remove_lease: impl FnOnce() -> Result<()>,
        hook: impl Fn(u8) -> Result<()>,
    ) -> Result<()> {
        if self.phase(lease)? != Phase::Complete {
            bail!("recovery is not complete");
        }
        hook(0)?;
        remove_runtime()?;
        hook(1)?;
        remove_lease()?;
        hook(2)?;
        let _inventory_lock = self.lock("inventory")?;
        hook(3)?;
        self.remove(lease)
    }
    pub fn remove(&self, lease: &ValidatedLease) -> Result<()> {
        self.require(lease)?;
        let temporary = self
            .root
            .join("quarantine")
            .join(format!(".{}.mutating", lease.domain_id));
        match fs::symlink_metadata(&temporary) {
            Ok(metadata) => {
                if !metadata.is_file()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o077 != 0
                {
                    bail!("unsafe interrupted authority update");
                }
                fs::remove_file(temporary)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::remove_file(self.path(&lease.domain_id)?)?;
        File::open(self.root.join("quarantine"))?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "hostd-quarantine-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn lease(domain: &str, group: u32, bdf: &str) -> ValidatedLease {
        ValidatedLease {
            domain_id: domain.into(),
            dvm_guest_cid: 4,
            iommu_group: group,
            pci_bdfs: vec![bdf.into()],
        }
    }
    #[test]
    fn reservation_survives_reopen_and_blocks_group_and_device_overlap() {
        let path = root();
        let a = Authority::provision_checked(&path, true).unwrap();
        let original = lease("gpu", 7, "0000:01:00.0");
        a.reserve(&original).unwrap();
        drop(a);
        let a = Authority::open_checked(&path, true).unwrap();
        assert!(a.reserve(&lease("other", 8, "0000:01:00.0")).is_err());
        assert!(a.reserve(&lease("other", 7, "0000:02:00.0")).is_err());
        assert!(a.remove(&lease("gpu", 8, "0000:01:00.0")).is_err());
        a.require(&original).unwrap();
        a.remove(&original).unwrap();
        a.reserve(&lease("other", 8, "0000:01:00.0")).unwrap();
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn provisioning_is_explicit_and_corruption_fails_closed() {
        let path = root();
        assert!(Authority::open_checked(&path, true).is_err());
        let authority = Authority::provision_checked(&path, true).unwrap();
        assert!(Authority::provision_checked(&path, true).is_err());
        let device = lease("gpu", 7, "0000:01:00.0");
        authority.reserve(&device).unwrap();
        assert_eq!(authority.phase(&device).unwrap(), Phase::Reserved);
        authority.mark_mutating(&device).unwrap();
        drop(authority);
        let authority = Authority::open_checked(&path, true).unwrap();
        assert_eq!(authority.phase(&device).unwrap(), Phase::Mutating);
        assert!(authority.mark_mutating(&device).is_err());
        drop(authority);
        fs::write(path.join("authority"), "bad").unwrap();
        assert!(Authority::open_checked(&path, true).is_err());
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn complete_tombstone_recovers_every_cleanup_interruption() {
        for failure in 0..4 {
            let path = root();
            let authority = Authority::provision_checked(&path, true).unwrap();
            let original = lease("gpu", 7, "0000:01:00.0");
            authority.reserve(&original).unwrap();
            authority
                .advance(&original, Phase::Reserved, Phase::Mutating)
                .unwrap();
            authority
                .advance(&original, Phase::Mutating, Phase::Reset)
                .unwrap();
            authority
                .advance(&original, Phase::Reset, Phase::Complete)
                .unwrap();
            let runtime = path.join("runtime-test");
            let lease_file = path.join("lease-test");
            fs::write(&runtime, "runtime").unwrap();
            fs::write(&lease_file, "lease").unwrap();
            let remove = |p: &Path| -> Result<()> {
                match fs::remove_file(p) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            };
            assert!(
                authority
                    .finish_complete_with_hook(
                        &original,
                        || remove(&runtime),
                        || remove(&lease_file),
                        |step| {
                            if step == failure {
                                bail!("injected interruption");
                            }
                            Ok(())
                        }
                    )
                    .is_err()
            );
            drop(authority);
            let authority = Authority::open_checked(&path, true).unwrap();
            assert_eq!(authority.phase(&original).unwrap(), Phase::Complete);
            assert!(
                authority
                    .reserve(&lease("other", 8, "0000:01:00.0"))
                    .is_err()
            );
            authority
                .finish_complete(&original, || remove(&runtime), || remove(&lease_file))
                .unwrap();
            authority
                .reserve(&lease("other", 8, "0000:01:00.0"))
                .unwrap();
            fs::remove_dir_all(path).unwrap();
        }
    }
    #[test]
    fn failed_reservation_sync_prevents_reassignment() {
        let path = root();
        let authority = Authority::provision_checked(&path, true).unwrap();
        let original = lease("gpu", 7, "0000:01:00.0");
        assert!(
            authority
                .reserve_with_sync(&original, |_| Err(std::io::Error::other(
                    "injected fsync failure"
                )))
                .is_err()
        );
        drop(authority);
        let authority = Authority::open_checked(&path, true).unwrap();
        assert!(
            authority
                .reserve(&lease("other", 8, "0000:01:00.0"))
                .is_err()
        );
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn invalid_reservation_and_contended_lock_fail_closed() {
        let path = root();
        let a = Authority::provision_checked(&path, true).unwrap();
        let lock = a.lock("inventory").unwrap();
        assert!(a.lock("inventory").is_err());
        drop(lock);
        fs::write(path.join("quarantine/bad.env"), "bad").unwrap();
        assert!(a.reserve(&lease("gpu", 7, "0000:01:00.0")).is_err());
        fs::remove_dir_all(path).unwrap();
    }
}
