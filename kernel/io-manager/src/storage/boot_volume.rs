// Ring0 bootstrap storage is deliberately limited to one immutable,
// bootloader-authenticated early-system image. Normal files and every mutable
// block operation belong to vfsd/storaged and the storage DVM.
use alloc::vec::Vec;
use boot_protocol::{
    BootInfo, BootVolumeIdentity, BootVolumeTransport, EARLY_SYSTEM_ENTRY_BYTES,
    EARLY_SYSTEM_HEADER_BYTES, EARLY_SYSTEM_PAYLOAD_ALIGNMENT, EarlySystemEntry, EarlySystemHeader,
    FramebufferInfo, valid_early_system_path,
};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use sha2::{Digest, Sha256};

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());
static BOOTSTRAP_PHASE: AtomicU8 = AtomicU8::new(BootstrapPhase::EarlyBootstrap as u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapImageError {
    NotFound,
    Unavailable,
    Invalid,
}

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

/// Boot-volume identity is diagnostic input only. Ring0 never opens the
/// controller or volume named by it.
pub fn boot_volume_identity() -> Option<BootVolumeIdentity> {
    let identity = boot_info()?.boot_volume;
    identity.is_present().then_some(identity)
}

pub fn boot_volume_transport_hint() -> Option<BootVolumeTransport> {
    Some(boot_info()?.boot_volume.transport())
}

/// Returns the L0 storage-epoch verifying key carried by the signed immutable
/// early-system header. A storage DVM can read the shared block aperture but
/// cannot replace this key or mint a successor transport epoch.
pub fn storage_epoch_verifying_key() -> Result<[u8; 32], BootstrapImageError> {
    let image = early_system_image_bytes()?.ok_or(BootstrapImageError::Unavailable)?;
    let header = EarlySystemHeader::decode(image).ok_or(BootstrapImageError::Invalid)?;
    Ok(header.storage_epoch_verifying_key)
}

pub fn read_file_to_vec(path: &str) -> Result<Vec<u8>, BootstrapImageError> {
    let Some(image) = early_system_image_bytes()? else {
        return Err(BootstrapImageError::Unavailable);
    };
    early_system_payload(image, path)?
        .map(Vec::from)
        .ok_or(BootstrapImageError::NotFound)
}

pub fn file_len(path: &str) -> Result<u64, BootstrapImageError> {
    let Some(image) = early_system_image_bytes()? else {
        return Err(BootstrapImageError::Unavailable);
    };
    let payload = early_system_payload(image, path)?.ok_or(BootstrapImageError::NotFound)?;
    u64::try_from(payload.len()).map_err(|_| BootstrapImageError::Invalid)
}

/// Reads only an admitted early-system entry. `Ok(None)` means that the
/// immutable bootstrap image does not own this path, so callers may ask vfsd;
/// it never means that physical disk fallback is permitted.
pub fn read_file_range(
    path: &str,
    file_offset: u64,
    dest: &mut [u8],
) -> Result<Option<usize>, BootstrapImageError> {
    let Some(image) = early_system_image_bytes()? else {
        return Ok(None);
    };
    let Some(payload) = early_system_payload(image, path)? else {
        return Ok(None);
    };
    let offset = usize::try_from(file_offset).map_err(|_| BootstrapImageError::Invalid)?;
    if offset >= payload.len() || dest.is_empty() {
        return Ok(Some(0));
    }
    let count = dest.len().min(payload.len() - offset);
    dest[..count].copy_from_slice(&payload[offset..offset + count]);
    Ok(Some(count))
}

fn normalized_bootstrap_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    (!path.is_empty() && !path.contains("..") && valid_early_system_path(path.as_bytes()))
        .then_some(path)
}

fn early_system_image_bytes() -> Result<Option<&'static [u8]>, BootstrapImageError> {
    let Some(info) = boot_info() else {
        return Ok(None);
    };
    let image = info.early_system_image;
    if !image.is_present() {
        return Ok(None);
    }
    image.validate().map_err(|_| BootstrapImageError::Invalid)?;
    let len = usize::try_from(image.len).map_err(|_| BootstrapImageError::Invalid)?;
    // SAFETY: BootInfo admission validates the immutable bootloader module
    // range, and boot memory retains it for the entire bootstrap lifetime.
    Ok(Some(unsafe {
        core::slice::from_raw_parts(image.ptr as *const u8, len)
    }))
}

fn early_system_payload<'a>(
    bytes: &'a [u8],
    path: &str,
) -> Result<Option<&'a [u8]>, BootstrapImageError> {
    let Some(path) = normalized_bootstrap_path(path) else {
        return Ok(None);
    };
    let header = EarlySystemHeader::decode(bytes).ok_or(BootstrapImageError::Invalid)?;
    if usize::try_from(header.total_bytes).ok() != Some(bytes.len()) {
        return Err(BootstrapImageError::Invalid);
    }

    let entry_count =
        usize::try_from(header.entry_count).map_err(|_| BootstrapImageError::Invalid)?;
    let mut previous: Option<EarlySystemEntry> = None;
    let mut previous_payload_end = header.payload_offset;
    let mut found = None;
    for index in 0..entry_count {
        let start = EARLY_SYSTEM_HEADER_BYTES
            .checked_add(
                index
                    .checked_mul(EARLY_SYSTEM_ENTRY_BYTES)
                    .ok_or(BootstrapImageError::Invalid)?,
            )
            .ok_or(BootstrapImageError::Invalid)?;
        let end = start
            .checked_add(EARLY_SYSTEM_ENTRY_BYTES)
            .ok_or(BootstrapImageError::Invalid)?;
        let record = bytes.get(start..end).ok_or(BootstrapImageError::Invalid)?;
        let entry = EarlySystemEntry::decode(record, header).ok_or(BootstrapImageError::Invalid)?;
        let entry_path = entry.path_bytes().ok_or(BootstrapImageError::Invalid)?;
        if previous
            .as_ref()
            .and_then(EarlySystemEntry::path_bytes)
            .is_some_and(|previous_path| previous_path >= entry_path)
            || !entry
                .payload_offset
                .is_multiple_of(EARLY_SYSTEM_PAYLOAD_ALIGNMENT)
            || entry.payload_offset < previous_payload_end
        {
            return Err(BootstrapImageError::Invalid);
        }
        let payload_end = entry
            .payload_offset
            .checked_add(entry.payload_len)
            .ok_or(BootstrapImageError::Invalid)?;
        if entry_path == path.as_bytes() {
            found = Some((entry.payload_offset, payload_end, entry.sha256));
        }
        previous_payload_end = payload_end;
        previous = Some(entry);
    }
    if previous_payload_end != header.total_bytes {
        return Err(BootstrapImageError::Invalid);
    }

    let Some((start, end, expected_digest)) = found else {
        return Ok(None);
    };
    let start = usize::try_from(start).map_err(|_| BootstrapImageError::Invalid)?;
    let end = usize::try_from(end).map_err(|_| BootstrapImageError::Invalid)?;
    let payload = bytes.get(start..end).ok_or(BootstrapImageError::Invalid)?;
    let digest: [u8; 32] = Sha256::digest(payload).into();
    if digest != expected_digest {
        return Err(BootstrapImageError::Invalid);
    }
    Ok(Some(payload))
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }.ok()
}

#[cfg(test)]
mod tests {
    use super::{BootstrapImageError, early_system_payload};
    use alloc::vec;
    use boot_protocol::{
        EARLY_SYSTEM_ENTRY_BYTES, EARLY_SYSTEM_HEADER_BYTES, EarlySystemEntry, EarlySystemHeader,
    };
    use sha2::{Digest, Sha256};

    #[test]
    fn early_system_lookup_verifies_exact_path_and_payload_digest() {
        let payload = b"rootd early payload";
        let header =
            EarlySystemHeader::new(1, 4096, 4096 + payload.len() as u64, [0x5a; 32]).unwrap();
        let digest: [u8; 32] = Sha256::digest(payload).into();
        let entry = EarlySystemEntry::new(
            b"services/rootd/rootd.elf",
            4096,
            payload.len() as u64,
            digest,
        )
        .unwrap();
        let mut image = vec![0_u8; header.total_bytes as usize];
        image[..EARLY_SYSTEM_HEADER_BYTES].copy_from_slice(&header.encode().unwrap());
        image[EARLY_SYSTEM_HEADER_BYTES..EARLY_SYSTEM_HEADER_BYTES + EARLY_SYSTEM_ENTRY_BYTES]
            .copy_from_slice(&entry.encode(header).unwrap());
        image[4096..].copy_from_slice(payload);

        assert_eq!(
            early_system_payload(&image, "/services/rootd/rootd.elf").unwrap(),
            Some(payload.as_slice())
        );
        assert_eq!(early_system_payload(&image, "/services/missing"), Ok(None));
        image[4096] ^= 0xff;
        assert_eq!(
            early_system_payload(&image, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
    }
}
