//! Boot-time warm-up of the shared immutable-library cache.
//!
//! - **Owner:** pagerd decides *which* files are cached (the manifest) and
//!   verifies their bytes; ring0 owns frames and the object registry; vfsd
//!   attests file identity and is the only byte source.
//! - **Boundary:** the manifest and every vfsd reply are untrusted. A file is
//!   published only if vfsd attests an immutable-volume identity whose length
//!   matches the manifest row and the streamed bytes hash to the row digest.
//! - **Lifecycle:** create, supply pages while hashing, then seal; any refusal
//!   retires the unsealed object, so nothing partial is ever visible.
//! - **Failure:** a refused row is skipped and counted; an unreadable manifest
//!   is returned so the caller can retry when vfsd is ready.
//! - **Forbidden:** path authority for guests, unverified publication, and
//!   any frame or mapping decision.
use rustos_library_cache_policy::{Entry, Plan, MAX_MANIFEST_BYTES, MANIFEST_PATH};
use rustos_user_abi::syscall::{LibraryIdentityWire, VfsFileIdentityWire};
use sha2::{Digest, Sha256};

pub const PAGE: usize = 4096;
const ENOMEM: i64 = -12;
const EEXIST: i64 = -17;

/// One bounded vfsd read. `identity` is meaningful only when `attested`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadChunk {
    pub len: usize,
    pub file_len: u64,
    pub attested: bool,
    pub identity: VfsFileIdentityWire,
}

pub trait WarmHost {
    /// Read at most `out.len()` bytes at a page-aligned `offset`.
    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<ReadChunk, i32>;
    fn create(&mut self, identity: LibraryIdentityWire) -> Result<u64, i64>;
    fn supply(&mut self, object: u64, page: u64, bytes: &[u8]) -> Result<(), i64>;
    fn seal(&mut self, object: u64) -> Result<(), i64>;
    fn retire(&mut self, object: u64);
    /// A sealed object the pager now holds custody of: `rank` is its position
    /// in the manifest's selection order (lower is more popular), `pages` its
    /// reserved size. Called once per object this run sealed.
    fn published(&mut self, _rank: u16, _object: u64, _pages: u64) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmError {
    /// The manifest could not be read or parsed; retry only for `Host`.
    Host(i32),
    Manifest,
}

/// Why one manifest row was not published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Skip {
    pub stage: Stage,
    pub code: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Read,
    NotAttested,
    LengthMismatch,
    NotImmutable,
    NotCanonical,
    Create,
    Supply,
    StreamShape,
    StreamRead,
    IdentityChanged,
    DigestMismatch,
    Seal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WarmReport {
    pub sealed: u16,
    pub refused: u16,
    /// Rows whose identical object was already published (an earlier,
    /// retried run or another publisher): warm, not refused.
    pub present: u16,
    /// Stage and errno of the first refused row (`None` when nothing refused),
    /// so a warm that seals nothing can name its cause once.
    pub first_refusal: Option<Skip>,
    /// The kernel byte budget ended the run early.
    pub budget_exhausted: bool,
}

pub fn warm<H: WarmHost>(
    host: &mut H,
    manifest: &mut [u8],
    chunk: &mut [u8],
) -> Result<WarmReport, WarmError> {
    assert!(!chunk.is_empty() && chunk.len() % PAGE == 0);
    let len = read_manifest(host, manifest, chunk)?;
    let text = core::str::from_utf8(&manifest[..len]).map_err(|_| WarmError::Manifest)?;
    let plan = Plan::parse(text).map_err(|_| WarmError::Manifest)?;
    let mut report = WarmReport::default();
    for (rank, entry) in plan.entries().enumerate() {
        let entry = entry.map_err(|_| WarmError::Manifest)?;
        let rank = u16::try_from(rank).unwrap_or(u16::MAX);
        match warm_entry(host, &entry, rank, chunk) {
            Ok(()) => report.sealed += 1,
            Err(Refusal::Budget) => {
                report.refused += 1;
                report.first_refusal.get_or_insert(Skip { stage: Stage::Supply, code: ENOMEM });
                report.budget_exhausted = true;
                break;
            }
            Err(Refusal::Present) => report.present += 1,
            Err(Refusal::NotReady(errno)) => return Err(WarmError::Host(errno)),
            Err(Refusal::Skip(skip)) => {
                report.refused += 1;
                report.first_refusal.get_or_insert(skip);
            }
        }
    }
    Ok(report)
}

fn read_manifest<H: WarmHost>(
    host: &mut H,
    manifest: &mut [u8],
    chunk: &mut [u8],
) -> Result<usize, WarmError> {
    let mut offset = 0_u64;
    let mut total = None;
    loop {
        let got = host.read(MANIFEST_PATH, offset, chunk).map_err(WarmError::Host)?;
        if got.len > chunk.len() {
            return Err(WarmError::Manifest);
        }
        let total = *total.get_or_insert(got.file_len);
        if got.file_len != total || total > MAX_MANIFEST_BYTES as u64 || total as usize > manifest.len()
        {
            return Err(WarmError::Manifest);
        }
        let start = offset as usize;
        if got.len > 0 {
            manifest[start..start + got.len].copy_from_slice(&chunk[..got.len]);
        }
        offset += got.len as u64;
        if offset == total {
            return Ok(total as usize);
        }
        if got.len == 0 || got.len % PAGE != 0 {
            return Err(WarmError::Manifest);
        }
    }
}

enum Refusal {
    Skip(Skip),
    /// The row's identical object already exists.
    Present,
    /// The volume holding the file is not serving yet; the whole run retries.
    NotReady(i32),
    Budget,
}

/// EAGAIN, ENODEV, ENOSYS: vfsd's answer while the boot volume's storage
/// service is not up. The same classification vfsd applies to its own
/// storage calls; any other errno is a property of the row.
pub const fn is_transient_not_ready(errno: i32) -> bool {
    matches!(errno, 11 | 19 | 38)
}

fn skip(stage: Stage, code: i64) -> Refusal {
    Refusal::Skip(Skip { stage, code })
}

fn warm_entry<H: WarmHost>(
    host: &mut H,
    entry: &Entry<'_>,
    rank: u16,
    chunk: &mut [u8],
) -> Result<(), Refusal> {
    let first = host.read(entry.path, 0, chunk).map_err(|errno| {
        if is_transient_not_ready(errno) {
            Refusal::NotReady(errno)
        } else {
            skip(Stage::Read, i64::from(errno))
        }
    })?;
    if !first.attested {
        return Err(skip(Stage::NotAttested, 0));
    }
    if first.identity.len != entry.bytes || first.file_len != entry.bytes {
        return Err(skip(Stage::LengthMismatch, first.file_len as i64));
    }
    if !first.identity.is_immutable() {
        return Err(skip(Stage::NotImmutable, 0));
    }
    let identity = LibraryIdentityWire { file: first.identity, content_hash: entry.sha256 };
    if !identity.is_canonical() {
        return Err(skip(Stage::NotCanonical, 0));
    }
    let object = match host.create(identity) {
        Ok(object) => object,
        Err(ENOMEM) => return Err(Refusal::Budget),
        Err(EEXIST) => return Err(Refusal::Present),
        Err(errno) => return Err(skip(Stage::Create, errno)),
    };
    match stream(host, object, entry, first, chunk) {
        Ok(()) => match host.seal(object) {
            Ok(()) => {
                host.published(rank, object, identity.page_count());
                Ok(())
            }
            Err(errno) => {
                host.retire(object);
                Err(skip(Stage::Seal, errno))
            }
        },
        Err(refusal) => {
            host.retire(object);
            Err(refusal)
        }
    }
}

fn stream<H: WarmHost>(
    host: &mut H,
    object: u64,
    entry: &Entry<'_>,
    first: ReadChunk,
    chunk: &mut [u8],
) -> Result<(), Refusal> {
    let mut digest = Sha256::new();
    let mut offset = 0_u64;
    let mut got = first;
    loop {
        let len = got.len;
        // A short non-final chunk would misalign the next page-aligned read.
        if len == 0 || len > chunk.len() || (len % PAGE != 0 && offset + len as u64 != entry.bytes)
        {
            return Err(skip(Stage::StreamShape, len as i64));
        }
        digest.update(&chunk[..len]);
        for (index, piece) in chunk[..len].chunks(PAGE).enumerate() {
            let page = offset / PAGE as u64 + index as u64;
            match host.supply(object, page, piece) {
                Ok(()) => {}
                Err(ENOMEM) => return Err(Refusal::Budget),
                Err(errno) => return Err(skip(Stage::Supply, errno)),
            }
        }
        offset += len as u64;
        if offset == entry.bytes {
            break;
        }
        got = host
            .read(entry.path, offset, chunk)
            .map_err(|errno| skip(Stage::StreamRead, i64::from(errno)))?;
        // The file must not change identity between chunks.
        if !got.attested || got.identity != first.identity || got.file_len != entry.bytes {
            return Err(skip(Stage::IdentityChanged, 0));
        }
    }
    let computed: [u8; 32] = digest.finalize().into();
    if computed == entry.sha256 { Ok(()) } else { Err(skip(Stage::DigestMismatch, 0)) }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeMap;
    use std::string::String;
    use std::vec::Vec;

    #[derive(Default)]
    struct Fake {
        files: BTreeMap<String, (Vec<u8>, bool)>,
        objects: BTreeMap<u64, (Vec<Vec<u8>>, bool, bool)>,
        next: u64,
        budget_pages: usize,
        flip_after_first: bool,
        reads: usize,
        library_read_error: Option<i32>,
        published: Vec<(u16, u64, u64)>,
    }

    impl WarmHost for Fake {
        fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<ReadChunk, i32> {
            self.reads += 1;
            if let Some(errno) = self.library_read_error.filter(|_| path != MANIFEST_PATH) {
                return Err(errno);
            }
            let (data, attested) = self.files.get(path).ok_or(2)?;
            let start = (offset as usize).min(data.len());
            let n = out.len().min(data.len() - start);
            out[..n].copy_from_slice(&data[start..start + n]);
            let mut generation = 1;
            if self.flip_after_first && offset > 0 {
                generation = 2;
            }
            Ok(ReadChunk {
                len: n,
                file_len: data.len() as u64,
                attested: *attested,
                identity: VfsFileIdentityWire {
                    volume: 1,
                    object: 7,
                    generation,
                    storage_epoch: 1,
                    mount_generation: 1,
                    len: data.len() as u64,
                    flags: 1,
                },
            })
        }
        fn create(&mut self, _: LibraryIdentityWire) -> Result<u64, i64> {
            self.next += 1;
            self.objects.insert(self.next, (Vec::new(), false, false));
            Ok(self.next)
        }
        fn supply(&mut self, object: u64, page: u64, bytes: &[u8]) -> Result<(), i64> {
            if self.budget_pages == 0 {
                return Err(ENOMEM);
            }
            self.budget_pages -= 1;
            let entry = self.objects.get_mut(&object).unwrap();
            assert_eq!(entry.0.len() as u64, page, "pages arrive in order");
            entry.0.push(bytes.to_vec());
            Ok(())
        }
        fn seal(&mut self, object: u64) -> Result<(), i64> {
            self.objects.get_mut(&object).unwrap().1 = true;
            Ok(())
        }
        fn retire(&mut self, object: u64) {
            self.objects.get_mut(&object).unwrap().2 = true;
        }
        fn published(&mut self, rank: u16, object: u64, pages: u64) {
            self.published.push((rank, object, pages));
        }
    }

    fn hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| std::format!("{b:02x}")).collect()
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn manifest(rows: &[(&str, &[u8])]) -> String {
        let mut text = String::from("rustos-library-cache-v1\t1048576\t524288\t8\t1\t4\n");
        for (path, data) in rows {
            text.push_str(&std::format!("{}\t{}\t{}\n", data.len(), hex(data), path));
        }
        text
    }

    fn run(fake: &mut Fake) -> Result<WarmReport, WarmError> {
        let mut manifest = std::vec![0_u8; MAX_MANIFEST_BYTES];
        let mut chunk = std::vec![0_u8; 8 * PAGE];
        warm(fake, &mut manifest, &mut chunk)
    }

    fn host_with(rows: &[(&str, Vec<u8>)]) -> Fake {
        let borrowed: Vec<(&str, &[u8])> = rows.iter().map(|(p, d)| (*p, d.as_slice())).collect();
        let mut fake = Fake { budget_pages: 1000, ..Fake::default() };
        fake.files.insert(MANIFEST_PATH.into(), (manifest(&borrowed).into_bytes(), false));
        for (path, data) in rows {
            fake.files.insert((*path).into(), (data.clone(), true));
        }
        fake
    }

    #[test]
    fn only_sealed_objects_are_published_to_custody_with_their_manifest_rank() {
        let first = pattern(2 * PAGE + 1, 1);
        let mut bad = pattern(PAGE, 2);
        let third = pattern(PAGE, 3);
        let mut fake = host_with(&[
            ("/lib/a.so", first),
            ("/lib/b.so", bad.clone()),
            ("/lib/c.so", third),
        ]);
        // The second file's bytes no longer match the manifest digest.
        bad[0] ^= 1;
        fake.files.insert("/lib/b.so".into(), (bad, true));
        let report = run(&mut fake).unwrap();
        assert_eq!((report.sealed, report.refused), (2, 1));
        // Ranks follow manifest order; the refused row (rank 1) is absent.
        let ranks: Vec<(u16, u64)> = fake.published.iter().map(|(r, _, p)| (*r, *p)).collect();
        assert_eq!(ranks, std::vec![(0, 3), (2, 1)]);
    }

    #[test]
    fn warms_multi_chunk_file_with_exact_tail_and_seals_once() {
        let data = pattern(9 * PAGE + 123, 3);
        let mut fake = host_with(&[("/lib/a.so", data.clone())]);
        let report = run(&mut fake).unwrap();
        assert_eq!(report, WarmReport { sealed: 1, ..WarmReport::default() });
        let (pages, sealed, retired) = &fake.objects[&1];
        assert!(*sealed && !*retired);
        assert_eq!(pages.len(), 10);
        assert_eq!(pages.last().unwrap().len(), 123);
        assert_eq!(pages.concat(), data);
    }

    #[test]
    fn digest_mismatch_retires_the_unsealed_object() {
        let good = pattern(2 * PAGE, 1);
        let mut fake = host_with(&[("/lib/a.so", good)]);
        // Same length, different bytes than the manifest digest.
        fake.files.get_mut("/lib/a.so").unwrap().0[5] ^= 1;
        let report = run(&mut fake).unwrap();
        assert_eq!(report.sealed, 0);
        assert_eq!(report.refused, 1);
        let (_, sealed, retired) = &fake.objects[&1];
        assert!(!*sealed && *retired);
    }

    #[test]
    fn unattested_or_wrong_length_files_never_create_an_object() {
        let data = pattern(PAGE, 9);
        let mut fake = host_with(&[("/lib/a.so", data.clone()), ("/lib/b.so", pattern(PAGE, 8))]);
        fake.files.get_mut("/lib/a.so").unwrap().1 = false;
        fake.files.get_mut("/lib/b.so").unwrap().0.push(0);
        let report = run(&mut fake).unwrap();
        assert_eq!(report.sealed, 0);
        assert_eq!(report.refused, 2);
        assert_eq!(report.first_refusal.map(|skip| skip.stage), Some(Stage::NotAttested));
        assert!(fake.objects.is_empty());
    }

    #[test]
    fn identity_change_between_chunks_is_refused() {
        let mut fake = host_with(&[("/lib/a.so", pattern(9 * PAGE, 4))]);
        fake.flip_after_first = true;
        let report = run(&mut fake).unwrap();
        assert_eq!(report.sealed, 0);
        assert_eq!(report.refused, 1);
        assert_eq!(report.first_refusal.map(|skip| skip.stage), Some(Stage::IdentityChanged));
        assert!(fake.objects[&1].2 && !fake.objects[&1].1);
    }

    #[test]
    fn kernel_budget_stops_the_run_and_leaves_nothing_sealed() {
        let mut fake = host_with(&[
            ("/lib/a.so", pattern(3 * PAGE, 1)),
            ("/lib/b.so", pattern(PAGE, 2)),
        ]);
        fake.budget_pages = 2;
        let report = run(&mut fake).unwrap();
        assert!(report.budget_exhausted);
        assert_eq!(report.sealed, 0);
        assert_eq!(fake.objects.len(), 1, "the second file is not attempted");
        assert!(fake.objects[&1].2);
    }

    #[test]
    fn library_read_before_the_volume_serves_retries_the_whole_run() {
        for errno in [11, 19, 38] {
            let mut fake = host_with(&[("/lib/a.so", pattern(PAGE, 1))]);
            fake.library_read_error = Some(errno);
            assert_eq!(run(&mut fake), Err(WarmError::Host(errno)));
            assert!(fake.objects.is_empty());
        }
        // A permanent per-row errno is a refusal that names its stage.
        let mut fake = host_with(&[("/lib/a.so", pattern(PAGE, 1))]);
        fake.library_read_error = Some(2);
        let report = run(&mut fake).unwrap();
        assert_eq!(report.refused, 1);
        assert_eq!(report.first_refusal, Some(Skip { stage: Stage::Read, code: 2 }));
    }

    #[test]
    fn missing_manifest_is_a_retryable_host_error_and_bad_manifest_is_not() {
        let mut fake = Fake::default();
        assert_eq!(run(&mut fake), Err(WarmError::Host(2)));
        fake.files.insert(MANIFEST_PATH.into(), (b"garbage\n".to_vec(), false));
        assert_eq!(run(&mut fake), Err(WarmError::Manifest));
    }
}
