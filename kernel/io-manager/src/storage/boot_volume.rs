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
use spin::Mutex;

static BOOT_INFO_PTR: AtomicPtr<BootInfo> = AtomicPtr::new(ptr::null_mut());
static BOOTSTRAP_PHASE: AtomicU8 = AtomicU8::new(BootstrapPhase::EarlyBootstrap as u8);
const VERIFIED_PAYLOAD_CACHE_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VerifiedPayloadKey {
    image_ptr: usize,
    image_len: usize,
    payload_offset: usize,
    payload_len: usize,
    expected_digest: [u8; 32],
}

/// Successful digest admission is reusable because the bootloader module is
/// immutable for the entire boot. The cache is deliberately bounded: a
/// malformed image cannot turn path probes into unbounded ring0 allocation.
static VERIFIED_PAYLOADS: Mutex<
    heapless::Vec<VerifiedPayloadKey, VERIFIED_PAYLOAD_CACHE_CAPACITY>,
> = Mutex::new(heapless::Vec::new());

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
    VERIFIED_PAYLOADS.lock().clear();
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
    cached_early_system_payload(image, path)?
        .map(Vec::from)
        .ok_or(BootstrapImageError::NotFound)
}

pub fn file_len(path: &str) -> Result<u64, BootstrapImageError> {
    let Some(image) = early_system_image_bytes()? else {
        return Err(BootstrapImageError::Unavailable);
    };
    let payload = cached_early_system_payload(image, path)?.ok_or(BootstrapImageError::NotFound)?;
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
    let Some(payload) = cached_early_system_payload(image, path)? else {
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

/// Returns one fully admitted immutable early-system payload. The returned
/// slice remains valid for the bootstrap lifetime and has already passed the
/// exact entry digest check. Callers that need multiple ranges must retain
/// this slice for the operation instead of revalidating the complete payload
/// for every chunk.
pub fn verified_file_bytes(path: &str) -> Result<Option<&'static [u8]>, BootstrapImageError> {
    let Some(image) = early_system_image_bytes()? else {
        return Ok(None);
    };
    cached_early_system_payload(image, path)
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

#[cfg(test)]
fn early_system_payload<'a>(
    bytes: &'a [u8],
    path: &str,
) -> Result<Option<&'a [u8]>, BootstrapImageError> {
    let Some((payload, expected_digest, _payload_offset)) =
        early_system_payload_candidate(bytes, path)?
    else {
        return Ok(None);
    };
    verify_payload_digest(payload, expected_digest)?;
    Ok(Some(payload))
}

fn cached_early_system_payload(
    bytes: &'static [u8],
    path: &str,
) -> Result<Option<&'static [u8]>, BootstrapImageError> {
    let Some((payload, expected_digest, payload_offset)) =
        early_system_payload_candidate(bytes, path)?
    else {
        return Ok(None);
    };
    let key = VerifiedPayloadKey {
        image_ptr: bytes.as_ptr() as usize,
        image_len: bytes.len(),
        payload_offset,
        payload_len: payload.len(),
        expected_digest,
    };
    if VERIFIED_PAYLOADS.lock().contains(&key) {
        return Ok(Some(payload));
    }

    verify_payload_digest(payload, expected_digest)?;
    let mut verified = VERIFIED_PAYLOADS.lock();
    if !verified.contains(&key) {
        let _ = verified.push(key);
    }
    Ok(Some(payload))
}

fn verify_payload_digest(
    payload: &[u8],
    expected_digest: [u8; 32],
) -> Result<(), BootstrapImageError> {
    let digest: [u8; 32] = Sha256::digest(payload).into();
    if digest != expected_digest {
        return Err(BootstrapImageError::Invalid);
    }
    Ok(())
}

type EarlySystemPayloadCandidate<'a> = (&'a [u8], [u8; 32], usize);

fn early_system_payload_candidate<'a>(
    bytes: &'a [u8],
    path: &str,
) -> Result<Option<EarlySystemPayloadCandidate<'a>>, BootstrapImageError> {
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
    Ok(Some((payload, expected_digest, start)))
}

fn boot_info() -> Option<&'static BootInfo> {
    let boot_info_ptr = BOOT_INFO_PTR.load(Ordering::Acquire);
    unsafe { BootInfo::from_ptr(boot_info_ptr.cast_const()) }.ok()
}

#[cfg(test)]
mod tests {
    use super::{BootstrapImageError, cached_early_system_payload, early_system_payload};
    use alloc::{vec, vec::Vec};
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

    // -----------------------------------------------------------------------
    // Content-integrity properties of the signed early-system image.
    //
    // These replace the retired `filesystem-content-integrity` TLA+ model, whose
    // `VerifiedContentMatchesManifest`, `BadContentNeverVerifies` and
    // `TerminalReadHasExactEvidenceShape` invariants were stated over two
    // symbolic digests. Here the production table walk and SHA-256 admission
    // run over every single-byte corruption of a real image.
    // -----------------------------------------------------------------------

    const ROOTD: &[u8] = b"services/rootd/rootd.elf";
    const LOADERD: &[u8] = b"services/loaderd/loaderd.elf";

    struct Spec<'a> {
        path: &'a [u8],
        payload: &'a [u8],
        offset: u64,
        digest: Option<[u8; 32]>,
    }

    fn spec<'a>(path: &'a [u8], payload: &'a [u8], offset: u64) -> Spec<'a> {
        Spec {
            path,
            payload,
            offset,
            digest: None,
        }
    }

    /// Build an image whose table lists `files` in the given order.
    fn image(files: &[Spec<'_>], tail_slack: u64) -> Vec<u8> {
        let end = files
            .iter()
            .map(|f| f.offset + f.payload.len() as u64)
            .max()
            .unwrap();
        let header =
            EarlySystemHeader::new(files.len() as u32, 4096, end + tail_slack, [0x5a; 32]).unwrap();
        let mut bytes = vec![0_u8; header.total_bytes as usize];
        bytes[..EARLY_SYSTEM_HEADER_BYTES].copy_from_slice(&header.encode().unwrap());
        for (index, file) in files.iter().enumerate() {
            let digest = file
                .digest
                .unwrap_or_else(|| Sha256::digest(file.payload).into());
            let entry =
                EarlySystemEntry::new(file.path, file.offset, file.payload.len() as u64, digest)
                    .unwrap();
            let at = EARLY_SYSTEM_HEADER_BYTES + index * EARLY_SYSTEM_ENTRY_BYTES;
            bytes[at..at + EARLY_SYSTEM_ENTRY_BYTES]
                .copy_from_slice(&entry.encode(header).unwrap());
            bytes[file.offset as usize..file.offset as usize + file.payload.len()]
                .copy_from_slice(file.payload);
        }
        bytes
    }

    fn two_file_image() -> Vec<u8> {
        // Paths are sorted: loaderd < rootd.
        image(
            &[
                spec(LOADERD, b"loaderd payload bytes", 4096),
                spec(ROOTD, b"rootd early payload", 8192),
            ],
            0,
        )
    }

    fn lookup(bytes: &[u8], path: &str) -> Result<Option<Vec<u8>>, BootstrapImageError> {
        early_system_payload(bytes, path).map(|found| found.map(<[u8]>::to_vec))
    }

    #[test]
    fn a_corrupted_image_never_returns_content_other_than_the_signed_payload() {
        let pristine = two_file_image();
        let expected = [
            (
                "/services/loaderd/loaderd.elf",
                b"loaderd payload bytes".as_slice(),
            ),
            (
                "/services/rootd/rootd.elf",
                b"rootd early payload".as_slice(),
            ),
        ];
        for (path, payload) in expected {
            assert_eq!(lookup(&pristine, path).unwrap().as_deref(), Some(payload));
        }
        let (mut admitted_unchanged, mut refused) = (0usize, 0usize);
        for position in 0..pristine.len() {
            for value in [0x00_u8, 0x01, 0x7f, 0x80, 0xff] {
                if pristine[position] == value {
                    continue;
                }
                let mut corrupted = pristine.clone();
                corrupted[position] = value;
                for (path, payload) in expected {
                    match lookup(&corrupted, path) {
                        // Alignment padding is outside every digest, so a
                        // corruption there leaves the verified content exact.
                        Ok(Some(found)) => {
                            assert_eq!(found, payload, "byte {position:#x}={value:#x} path {path}");
                            admitted_unchanged += 1;
                        }
                        Ok(None) | Err(_) => refused += 1,
                    }
                }
                assert_eq!(
                    lookup(&corrupted, "/services/missing")
                        .map(|v| v.is_some())
                        .unwrap_or(false),
                    false
                );
            }
        }
        assert!(
            admitted_unchanged > 0 && refused > 0,
            "{admitted_unchanged}/{refused}"
        );
    }

    #[test]
    fn every_truncation_of_the_image_is_refused() {
        let pristine = two_file_image();
        for len in 0..pristine.len() {
            assert_eq!(
                lookup(&pristine[..len], "/services/rootd/rootd.elf"),
                Err(BootstrapImageError::Invalid),
                "len {len}"
            );
        }
        let mut longer = pristine.clone();
        longer.push(0);
        assert_eq!(
            lookup(&longer, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
    }

    #[test]
    fn a_table_that_is_not_canonical_is_refused_even_when_every_digest_is_valid() {
        let payload = b"shared payload";
        // Two entries naming the same payload range overlap.
        let overlapping = image(
            &[spec(LOADERD, payload, 4096), spec(ROOTD, payload, 4096)],
            0,
        );
        assert_eq!(
            lookup(&overlapping, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
        // The same path twice (and any unsorted order) breaks the strict order.
        let duplicate = image(&[spec(ROOTD, payload, 4096), spec(ROOTD, payload, 8192)], 0);
        assert_eq!(
            lookup(&duplicate, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
        let unsorted = image(
            &[spec(ROOTD, payload, 4096), spec(LOADERD, payload, 8192)],
            0,
        );
        assert_eq!(
            lookup(&unsorted, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
        // Bytes after the last payload are not covered by any digest.
        let slack = image(&[spec(ROOTD, payload, 4096)], 7);
        assert_eq!(
            lookup(&slack, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
        // A payload that does not start on the declared alignment.
        let misaligned = image(&[spec(ROOTD, payload, 4096 + 8)], 0);
        assert_eq!(
            lookup(&misaligned, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
        // A digest that does not match the payload is refused.
        let wrong = image(
            &[Spec {
                digest: Some([0x11; 32]),
                ..spec(ROOTD, payload, 4096)
            }],
            0,
        );
        assert_eq!(
            lookup(&wrong, "/services/rootd/rootd.elf"),
            Err(BootstrapImageError::Invalid)
        );
    }

    #[test]
    fn a_path_outside_the_bootstrap_namespace_resolves_to_nothing() {
        let pristine = two_file_image();
        for path in [
            "",
            "/",
            "//",
            "/services/../services/rootd/rootd.elf",
            "/services/rootd/",
            "/missing",
        ] {
            assert_eq!(lookup(&pristine, path), Ok(None), "{path:?}");
        }
    }

    #[test]
    fn the_verified_payload_cache_is_bound_to_the_exact_image() {
        // A leaked copy stands in for the immutable bootloader module.
        let good: &'static [u8] = vec_leak(two_file_image());
        let mut bad_copy = two_file_image();
        let at = 8192;
        bad_copy[at] ^= 0xff;
        let bad: &'static [u8] = vec_leak(bad_copy);
        let path = "/services/rootd/rootd.elf";
        assert!(cached_early_system_payload(good, path).unwrap().is_some());
        // The corrupted image has the same offsets, lengths and table digest.
        // Only its address differs; a cache that ignored the address would
        // return the unverified bytes.
        assert_eq!(
            cached_early_system_payload(bad, path),
            Err(BootstrapImageError::Invalid)
        );
        assert!(cached_early_system_payload(good, path).unwrap().is_some());
    }

    fn vec_leak(bytes: Vec<u8>) -> &'static [u8] {
        alloc::boxed::Box::leak(bytes.into_boxed_slice())
    }
}
