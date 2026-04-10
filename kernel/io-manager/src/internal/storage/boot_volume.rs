#![cfg_attr(not(test), allow(dead_code))]

use alloc::boxed::Box;
use alloc::vec::Vec;
use boot_protocol::{BootInfo, BootVolumeIdentity, BootVolumeTransport, FramebufferInfo};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use storage_core::BlockDevice;

use crate::storage::fat::{self, DiskIoError};

pub use crate::storage::fat::{BootVolumeDirEntry, BootVolumeMetadata};

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());
static mut BOOT_BLOCK_DEVICE_OPENER: Option<BootBlockDeviceOpener> = None;
static mut PHYSICAL_BOOT_BLOCK_DEVICE_OPENER: Option<PhysicalBootBlockDeviceOpener> = None;
static BOOTSTRAP_PHASE: AtomicU8 = AtomicU8::new(BootstrapPhase::EarlyBootstrap as u8);

pub type BootBlockDeviceOpener =
    fn() -> core::result::Result<Box<dyn BlockDevice>, fatfs::Error<DiskIoError>>;
pub type PhysicalBootBlockDeviceOpener =
    fn(BootVolumeIdentity) -> core::result::Result<Box<dyn BlockDevice>, fatfs::Error<DiskIoError>>;

type BootVolumeFs = fat::MountedFatVolume<Box<dyn BlockDevice>>;
type BootVolumeFileInner<'a> = storage_fat::FatFile<'a, Box<dyn BlockDevice>>;
const BOOT_VOLUME_READ_CHUNK_CAP: usize = 4 * 1024;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapPhase {
    EarlyBootstrap = 0,
    CoreHostsLaunching = 1,
    KernelVfsReady = 2,
    UserspaceReady = 3,
}

impl BootstrapPhase {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::CoreHostsLaunching,
            2 => Self::KernelVfsReady,
            3 => Self::UserspaceReady,
            _ => Self::EarlyBootstrap,
        }
    }
}

fn should_trace_boot_path(path: &str) -> bool {
    path.contains("services/")
        || path.starts_with("lib/")
        || path.starts_with("/lib/")
        || path.starts_with("lib64/")
        || path.starts_with("/lib64/")
}

fn ensure_bootstrap_fs_access(path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
    if kernel_vfs_runtime_active() {
        crate::debug::println!(
            "bootstrap fs: rejected late direct access path={} phase={:?}",
            path,
            bootstrap_phase()
        );
        return Err(fatfs::Error::Io(DiskIoError::Unsupported));
    }
    Ok(())
}

pub struct BootVolume {
    fs: BootVolumeFs,
}

pub struct BootVolumeFile<'a>(BootVolumeFileInner<'a>);

pub struct PhysicalBootVolume {
    fs: BootVolumeFs,
}

pub struct PhysicalBootVolumeFile<'a>(BootVolumeFileInner<'a>);

pub fn init_boot_info(boot_info_ptr: *const BootInfo) {
    BOOT_INFO_PTR.store(boot_info_ptr.cast_mut(), Ordering::Release);
}

pub fn bootstrap_phase() -> BootstrapPhase {
    BootstrapPhase::from_raw(BOOTSTRAP_PHASE.load(Ordering::Acquire))
}

pub fn kernel_vfs_runtime_active() -> bool {
    matches!(
        bootstrap_phase(),
        BootstrapPhase::KernelVfsReady | BootstrapPhase::UserspaceReady
    )
}

pub fn userspace_runtime_active() -> bool {
    bootstrap_phase() == BootstrapPhase::UserspaceReady
}

fn set_bootstrap_phase(phase: BootstrapPhase) {
    BOOTSTRAP_PHASE.store(phase as u8, Ordering::Release);
    crate::debug::println!("bootstrap phase -> {:?}", phase);
}

pub fn enter_kernel_vfs_runtime() {
    set_bootstrap_phase(BootstrapPhase::KernelVfsReady);
}

pub fn enter_userspace_runtime() {
    set_bootstrap_phase(BootstrapPhase::UserspaceReady);
}

pub fn boot_framebuffer_info() -> Option<FramebufferInfo> {
    boot_info().map(|info| info.framebuffer)
}

pub fn boot_volume_identity() -> Option<BootVolumeIdentity> {
    let identity = boot_info()?.boot_volume;
    identity.is_present().then_some(identity)
}

pub fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
    Some(boot_info()?.boot_volume.transport())
}

pub fn set_boot_block_device_opener(opener: BootBlockDeviceOpener) {
    unsafe {
        BOOT_BLOCK_DEVICE_OPENER = Some(opener);
    }
}

pub fn set_physical_boot_block_device_opener(opener: PhysicalBootBlockDeviceOpener) {
    unsafe {
        PHYSICAL_BOOT_BLOCK_DEVICE_OPENER = Some(opener);
    }
}

impl IoBase for BootVolumeFile<'_> {
    type Error = fatfs::Error<DiskIoError>;
}

impl Read for BootVolumeFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.0.read(buf)
    }
}

impl Seek for BootVolumeFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, fatfs::Error<DiskIoError>> {
        self.0.seek(pos)
    }
}

impl IoBase for PhysicalBootVolumeFile<'_> {
    type Error = fatfs::Error<DiskIoError>;
}

impl Read for PhysicalBootVolumeFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.0.read(buf)
    }
}

impl Write for PhysicalBootVolumeFile<'_> {
    fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.0.flush()
    }
}

impl Seek for PhysicalBootVolumeFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, fatfs::Error<DiskIoError>> {
        self.0.seek(pos)
    }
}

impl PhysicalBootVolumeFile<'_> {
    pub fn truncate(&mut self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.0.truncate()
    }
}

impl BootVolume {
    pub fn open() -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        let opener =
            unsafe { BOOT_BLOCK_DEVICE_OPENER }.ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let device = opener()?;
        let fs = fat::open_volume(device)?;
        Ok(Self { fs })
    }

    pub fn open_file(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        self.fs.open_file(path).map(BootVolumeFile)
    }

    pub fn metadata(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
        self.fs.metadata(path)
    }

    pub fn read_dir(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<BootVolumeDirEntry>, fatfs::Error<DiskIoError>> {
        self.fs.read_dir(path)
    }

    pub fn read_file_to_vec(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
        if should_trace_boot_path(path) {
            crate::debug::println!("boot volume: read_file_to_vec enter path={}", path);
        }
        self.fs.read_file_to_vec(path)
    }

    pub fn read_file_into(
        &self,
        path: &str,
        dest: &mut [u8],
    ) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        if should_trace_boot_path(path) {
            crate::debug::println!(
                "boot volume: read_file_into enter path={} len={}",
                path,
                dest.len()
            );
        }
        let trace = should_trace_boot_path(path);
        if trace {
            crate::debug::println!("boot volume: read_file_into open_file begin path={}", path);
        }
        let mut file = self.fs.open_file(path)?;
        if trace {
            crate::debug::println!("boot volume: read_file_into open_file done path={}", path);
        }
        let mut done = 0usize;
        while done < dest.len() {
            if trace && done == 0 {
                crate::debug::println!(
                    "boot volume: read_file_into first read begin path={} remaining={}",
                    path,
                    dest.len() - done
                );
            }
            let remaining = dest.len() - done;
            let chunk_len = remaining.min(BOOT_VOLUME_READ_CHUNK_CAP);
            let count = match file.read(&mut dest[done..done + chunk_len]) {
                Ok(count) => count,
                Err(err) => {
                    if trace {
                        crate::debug::println!(
                            "boot volume: read_file_into read error path={} offset={} chunk_len={} err={:?}",
                            path,
                            done,
                            chunk_len,
                            err
                        );
                    }
                    return Err(err);
                }
            };
            if trace && done == 0 {
                crate::debug::println!(
                    "boot volume: read_file_into first read done path={} count={}",
                    path,
                    count
                );
            } else if trace && done < (BOOT_VOLUME_READ_CHUNK_CAP * 4) {
                crate::debug::println!(
                    "boot volume: read_file_into chunk done path={} offset={} count={}",
                    path,
                    done,
                    count
                );
            }
            if count == 0 {
                break;
            }
            done += count;
        }
        if trace {
            crate::debug::println!(
                "boot volume: read_file_into exit path={} ok={} read={}",
                path,
                true,
                done
            );
        }
        Ok(done)
    }

    pub fn close(self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.unmount()
    }
}

impl PhysicalBootVolume {
    pub fn open(
        identity: BootVolumeIdentity,
    ) -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        if identity.validate().is_err() || !identity.is_present() {
            return Err(fatfs::Error::Io(DiskIoError::NotPresent));
        }
        let opener = unsafe { PHYSICAL_BOOT_BLOCK_DEVICE_OPENER }
            .ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        let device = opener(identity)?;
        let fs = fat::open_volume(device)?;
        Ok(Self { fs })
    }

    pub fn open_current() -> core::result::Result<Self, fatfs::Error<DiskIoError>> {
        let identity = boot_volume_identity().ok_or(fatfs::Error::Io(DiskIoError::NotPresent))?;
        Self::open(identity)
    }

    pub fn open_or_create_truncated_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        let mut file = self.create_file(path)?;
        file.seek(SeekFrom::Start(0))?;
        file.truncate()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }

    pub fn open_or_create_append_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        let mut file = self.create_file(path)?;
        file.seek(SeekFrom::End(0))?;
        Ok(file)
    }

    pub fn open_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        self.fs.open_file(path).map(PhysicalBootVolumeFile)
    }

    pub fn create_file(
        &self,
        path: &str,
    ) -> core::result::Result<PhysicalBootVolumeFile<'_>, fatfs::Error<DiskIoError>> {
        self.fs.create_file(path).map(PhysicalBootVolumeFile)
    }

    pub fn metadata(
        &self,
        path: &str,
    ) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
        self.fs.metadata(path)
    }

    pub fn read_dir(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<BootVolumeDirEntry>, fatfs::Error<DiskIoError>> {
        self.fs.read_dir(path)
    }

    pub fn create_dir(&self, path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.create_dir(path)
    }

    pub fn remove_file(&self, path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.remove_file(path)
    }

    pub fn remove_dir(&self, path: &str) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.remove_dir(path)
    }

    pub fn rename(
        &self,
        src: &str,
        dst: &str,
    ) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.rename(src, dst)
    }

    pub fn read_file_to_vec(
        &self,
        path: &str,
    ) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
        self.fs.read_file_to_vec(path)
    }

    pub fn read_file_into(
        &self,
        path: &str,
        dest: &mut [u8],
    ) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
        self.fs.read_file_into(path, dest)
    }

    pub fn append_bytes(
        &self,
        path: &str,
        bytes: &[u8],
        flush: bool,
    ) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        let mut file = self.open_or_create_append_file(path)?;
        let mut written = 0usize;
        while written < bytes.len() {
            let count = file.write(&bytes[written..])?;
            if count == 0 {
                return Err(fatfs::Error::Io(DiskIoError::WriteZero));
            }
            written += count;
        }
        if flush {
            file.flush()?;
        }
        Ok(())
    }

    pub fn close(self) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
        self.fs.unmount()
    }
}

pub fn open_physical_boot_volume(
    identity: BootVolumeIdentity,
) -> core::result::Result<PhysicalBootVolume, fatfs::Error<DiskIoError>> {
    PhysicalBootVolume::open(identity)
}

pub fn open_current_physical_boot_volume()
-> core::result::Result<PhysicalBootVolume, fatfs::Error<DiskIoError>> {
    PhysicalBootVolume::open_current()
}

pub fn read_bootstrap_file_to_vec(
    path: &str,
) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!("boot volume helper: read_file_to_vec begin path={}", path);
    }
    ensure_bootstrap_fs_access(path)?;
    with_open_boot_volume(|volume| volume.read_file_to_vec(path))
}

pub fn read_file_to_vec(path: &str) -> core::result::Result<Vec<u8>, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!(
            "boot volume helper: runtime read_file_to_vec begin path={}",
            path
        );
    }
    with_open_boot_volume(|volume| volume.read_file_to_vec(path))
}

pub fn read_file_into(
    path: &str,
    dest: &mut [u8],
) -> core::result::Result<usize, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!(
            "boot volume helper: read_file_into begin path={} len={}",
            path,
            dest.len()
        );
    }
    with_open_boot_volume(|volume| volume.read_file_into(path, dest))
}

pub fn metadata(path: &str) -> core::result::Result<BootVolumeMetadata, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!("boot volume helper: metadata begin path={}", path);
    }
    with_open_boot_volume(|volume| volume.metadata(path))
}

pub fn read_dir(
    path: &str,
) -> core::result::Result<Vec<BootVolumeDirEntry>, fatfs::Error<DiskIoError>> {
    if should_trace_boot_path(path) {
        crate::debug::println!("boot volume helper: read_dir begin path={}", path);
    }
    with_open_boot_volume(|volume| volume.read_dir(path))
}

fn with_open_boot_volume<T>(
    f: impl FnOnce(&BootVolume) -> core::result::Result<T, fatfs::Error<DiskIoError>>,
) -> core::result::Result<T, fatfs::Error<DiskIoError>> {
    crate::debug::println!("boot volume helper: open begin");
    let volume = BootVolume::open()?;
    crate::debug::println!("boot volume helper: open done");
    let result = f(&volume);
    crate::debug::println!("boot volume helper: callback done ok={}", result.is_ok());
    let close_result = volume.close();
    crate::debug::println!("boot volume helper: close done ok={}", close_result.is_ok());
    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }.ok()
}
