// Evidence for `kvm-smoke --mmap-cost-probe`: measures what the CURRENT path
// costs when a process maps a large, immutable, file-backed MAP_PRIVATE range
// that is NOT in the boot library cache. Observation only: the guest side is
// the `mode=mmapcost` branch of `apps/libmapprobe`, armed by a private contract
// and reading the same free-page diagnostic as the other library probe modes.
// Results and interpretation: docs/ai/pager/library-cache.md, "Measured
// baseline: large non-warmed file mappings".
//
// The data files are written into the per-run copy of the boot FAT volume, the
// same volume (and the same `/apps/...` namespace and vfsd read path) that real
// large application files such as the Chrome bundle live on. They are never
// listed in `/etc/pagerd/library-cache.conf`, so they take the eager-copy path.

/// Sizes measured by default, in MiB.
const MMAP_COST_DEFAULT_SIZES_MIB: [u64; 4] = [4, 16, 64, 128];
/// The guest keeps one 8-byte slot per page; 1 GiB is its static bound.
const MMAP_COST_MAX_SIZE_MIB: u64 = 1024;
const MMAP_COST_VOLUME_DIR: &str = "apps/libmapprobe";
const MMAP_COST_DONE_MARKER: &str = "mmap-cost-probe: done files=";
const MMAP_COST_SAMPLE_MARKER: &str = "mmap-cost-probe: size=";
const MMAP_COST_SEED_BASE: u64 = 0x6d6d_6170_636f_7374;

#[derive(Clone, Debug, PartialEq, Eq)]
struct MmapCostFile {
    size: u64,
    seed: u64,
    sha256: String,
}

fn mmap_cost_seed(size_bytes: u64) -> u64 {
    MMAP_COST_SEED_BASE ^ size_bytes
}

/// Must equal `expected_word` in `apps/libmapprobe/libmapprobe.c`.
fn mmap_cost_word(seed: u64, index: u64) -> u64 {
    let mut z = seed.wrapping_add(index.wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn mmap_cost_volume_path(size_mib: u64) -> String {
    format!("{MMAP_COST_VOLUME_DIR}/mmapcost-{size_mib}m.bin")
}

/// Streams the deterministic content into `sink` and returns its sha256.
fn mmap_cost_stream(size: u64, seed: u64, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut chunk = vec![0_u8; 1 << 20];
    let mut done = 0_u64;
    while done < size {
        let len = (size - done).min(chunk.len() as u64) as usize;
        for (word, bytes) in chunk[..len].chunks_exact_mut(8).enumerate() {
            bytes.copy_from_slice(&mmap_cost_word(seed, done / 8 + word as u64).to_le_bytes());
        }
        hasher.update(&chunk[..len]);
        sink(&chunk[..len])?;
        done += len as u64;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn parse_mmap_cost_sizes(value: &str) -> Result<Vec<u64>> {
    let mut sizes = Vec::new();
    for part in value.split(',') {
        let mib: u64 = part
            .trim()
            .parse()
            .with_context(|| format!("--mmap-cost-sizes-mib: `{part}` is not a number of MiB"))?;
        if mib == 0 || mib > MMAP_COST_MAX_SIZE_MIB || sizes.contains(&mib) {
            bail!("--mmap-cost-sizes-mib: sizes must be distinct and within 1..={MMAP_COST_MAX_SIZE_MIB} MiB");
        }
        sizes.push(mib);
    }
    Ok(sizes)
}

/// Contract body: the `libmapprobe` header, the mode line, then one row per file
/// (`file=<guest path> <bytes> <seed hex> <sha256>`).
fn render_mmap_cost_contract(sizes_mib: &[u64], files: &[MmapCostFile]) -> String {
    let mut body = String::from("contract=rustos-libmapprobe-v1\nmode=mmapcost\n");
    for (mib, file) in sizes_mib.iter().zip(files) {
        body.push_str(&format!(
            "file=/{} {} {:x} {}\n",
            mmap_cost_volume_path(*mib),
            file.size,
            file.seed,
            file.sha256
        ));
    }
    body
}

/// Writes the data files into the per-run boot FAT image, then the contract.
fn write_mmap_cost_probe_files<D: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, D, fatfs::DefaultTimeProvider, fatfs::LossyOemCpConverter>,
    sizes_mib: &[u64],
) -> Result<String>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let mut files = Vec::new();
    for &mib in sizes_mib {
        let size = mib * 1024 * 1024;
        let seed = mmap_cost_seed(size);
        let path = mmap_cost_volume_path(mib);
        let mut dst = root
            .create_file(&path)
            .with_context(|| format!("create mmap-cost data file {path}"))?;
        dst.truncate()?;
        let sha256 = mmap_cost_stream(size, seed, |bytes| {
            FatWrite::write_all(&mut dst, bytes).with_context(|| format!("write {path}"))
        })?;
        FatWrite::flush(&mut dst)?;
        files.push(MmapCostFile { size, seed, sha256 });
    }
    Ok(render_mmap_cost_contract(sizes_mib, &files))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MmapCostSample {
    size: u64,
    mmap_us: u64,
    sparse_us: u64,
    sparse_pages: u64,
    full_us: u64,
    fork_us: i64,
    unmap_us: u64,
    sha_us: u64,
    free_before: u64,
    free_after_mmap: u64,
    free_after_sparse: u64,
    free_after_full: u64,
    free_fork_live: u64,
    free_after_unmap: u64,
    leaves_mmap: i64,
}

#[derive(Debug, PartialEq, Eq)]
enum MmapCostRecord {
    Sample(MmapCostSample),
    /// The system refused or lost the size: `(size, stage, errno)`.
    Failed(u64, String, i64),
}

fn mmap_cost_kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
}

fn mmap_cost_u64(line: &str, key: &str) -> Option<u64> {
    mmap_cost_kv(line, key)?.parse().ok()
}

fn parse_mmap_cost_records(log: &str) -> Vec<MmapCostRecord> {
    let mut records = Vec::new();
    for line in log.lines() {
        let Some(at) = line.find(MMAP_COST_SAMPLE_MARKER) else {
            continue;
        };
        let line = &line[at..];
        let Some(size) = mmap_cost_u64(line, "size") else {
            continue;
        };
        if let Some(stage) = mmap_cost_kv(line, "stage").filter(|_| line.contains(" FAILED ")) {
            let errno = mmap_cost_kv(line, "errno").and_then(|v| v.parse().ok()).unwrap_or(-1);
            records.push(MmapCostRecord::Failed(size, stage.to_owned(), errno));
            continue;
        }
        let field = |key: &str| mmap_cost_u64(line, key);
        let sample = (|| {
            Some(MmapCostSample {
                size,
                mmap_us: field("mmap_us")?,
                sparse_us: field("sparse_us")?,
                sparse_pages: field("sparse_pages")?,
                full_us: field("full_us")?,
                fork_us: mmap_cost_kv(line, "fork_us")?.parse().ok()?,
                unmap_us: field("unmap_us")?,
                sha_us: field("sha_us")?,
                free_before: field("free_before")?,
                free_after_mmap: field("free_after_mmap")?,
                free_after_sparse: field("free_after_sparse")?,
                free_after_full: field("free_after_full")?,
                free_fork_live: field("free_fork_live")?,
                free_after_unmap: field("free_after_unmap")?,
                leaves_mmap: mmap_cost_kv(line, "leaves_mmap")?.parse().ok()?,
            })
        })();
        if let Some(sample) = sample.filter(|_| line.contains("sha_ok=1") && line.contains("verify_ok=1")) {
            records.push(MmapCostRecord::Sample(sample));
        }
    }
    records
}

/// Requires a clean end (`done`), no probe FAIL line, and one record per size.
fn validate_mmap_cost_log(log: &str, sizes_mib: &[u64]) -> Result<Vec<MmapCostRecord>> {
    if let Some(failure) = library_map_probe_failure(log) {
        bail!("mmap-cost probe reported a failure: {failure}");
    }
    if !log.contains(MMAP_COST_DONE_MARKER) {
        bail!("mmap-cost probe never logged `{MMAP_COST_DONE_MARKER}`");
    }
    let records = parse_mmap_cost_records(log);
    for &mib in sizes_mib {
        let size = mib * 1024 * 1024;
        let found = records.iter().any(|record| match record {
            MmapCostRecord::Sample(sample) => sample.size == size,
            MmapCostRecord::Failed(failed, ..) => *failed == size,
        });
        if !found {
            bail!("mmap-cost probe produced no record for {mib} MiB");
        }
    }
    Ok(records)
}

fn render_mmap_cost_table(records: &[MmapCostRecord]) -> String {
    let mut out = String::from(
        "size_MiB mmap_ms sparse_us(pages) full_ms fork_us unmap_ms | free pages: before  mmap  sparse  full  fork-live  unmap | resident_after_mmap_MiB\n",
    );
    for record in records {
        match record {
            MmapCostRecord::Sample(s) => {
                let resident_mib =
                    (s.free_before as i64 - s.free_after_mmap as i64) * 4096 / (1024 * 1024);
                out.push_str(&format!(
                    "{:>8} {:>7.1} {:>7}({:>3}) {:>7.2} {:>7} {:>8.1} | {:>18} {:>6} {:>6} {:>5} {:>9} {:>6} | {:>6}\n",
                    s.size / (1024 * 1024),
                    s.mmap_us as f64 / 1000.0,
                    s.sparse_us,
                    s.sparse_pages,
                    s.full_us as f64 / 1000.0,
                    s.fork_us,
                    s.unmap_us as f64 / 1000.0,
                    s.free_before,
                    s.free_after_mmap,
                    s.free_after_sparse,
                    s.free_after_full,
                    s.free_fork_live,
                    s.free_after_unmap,
                    resident_mib,
                ));
            }
            MmapCostRecord::Failed(size, stage, errno) => out.push_str(&format!(
                "{:>8} FAILED at stage {stage}, errno {errno}\n",
                size / (1024 * 1024)
            )),
        }
    }
    out
}

#[cfg(test)]
mod mmap_cost_probe_tests {
    use super::*;

    #[test]
    fn word_generator_is_stable_and_streams_match() {
        // Pinned so the guest generator in libmapprobe.c cannot drift silently.
        assert_eq!(mmap_cost_word(0, 0), 0xe220_a839_7b1d_cdaf);
        let mut bytes = Vec::new();
        let sha = mmap_cost_stream(8192, 7, |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(bytes.len(), 8192);
        assert_eq!(&bytes[8..16], &mmap_cost_word(7, 1).to_le_bytes());
        assert_eq!(sha.len(), 64);
    }

    #[test]
    fn sizes_are_validated() {
        assert_eq!(parse_mmap_cost_sizes("4,16").unwrap(), vec![4, 16]);
        assert!(parse_mmap_cost_sizes("4,4").is_err());
        assert!(parse_mmap_cost_sizes("0").is_err());
        assert!(parse_mmap_cost_sizes("2048").is_err());
        assert!(parse_mmap_cost_sizes("x").is_err());
    }

    #[test]
    fn contract_names_each_file_with_path_seed_and_digest() {
        let files = vec![MmapCostFile { size: 4 << 20, seed: 0xab, sha256: "f".repeat(64) }];
        let body = render_mmap_cost_contract(&[4], &files);
        assert!(body.starts_with("contract=rustos-libmapprobe-v1\nmode=mmapcost\n"));
        assert!(body.contains(&format!("file=/apps/libmapprobe/mmapcost-4m.bin 4194304 ab {}\n", "f".repeat(64))));
    }

    const LINE: &str = "mmap-cost-probe: size=4194304 mmap_us=100 sparse_us=5 sparse_pages=10 full_us=50 fork_us=70 unmap_us=9 sha_us=1000 free_before=500000 free_after_mmap=498976 free_after_sparse=498976 free_after_full=498976 free_fork_live=498900 free_after_unmap=500000 leaves_mmap=1024 sha_ok=1 verify_ok=1";

    #[test]
    fn log_records_parse_and_validate() {
        let log = format!(
            "[x] {LINE}\n[x] mmap-cost-probe: size=8388608 FAILED stage=mmap errno=12\n[x] mmap-cost-probe: done files=2\n"
        );
        let records = validate_mmap_cost_log(&log, &[4, 8]).unwrap();
        assert_eq!(records.len(), 2);
        let MmapCostRecord::Sample(sample) = &records[0] else { panic!() };
        assert_eq!((sample.size, sample.free_before - sample.free_after_mmap), (4194304, 1024));
        assert_eq!(records[1], MmapCostRecord::Failed(8388608, "mmap".into(), 12));
        assert!(render_mmap_cost_table(&records).contains("FAILED at stage mmap"));
        assert!(validate_mmap_cost_log(&log, &[4, 16]).is_err());
        assert!(validate_mmap_cost_log(&format!("[x] {LINE}\n"), &[4]).is_err());
        let bad = format!("{log}library-map-probe: FAIL sparse page content 1 2 errno=0\n");
        assert!(validate_mmap_cost_log(&bad, &[4, 8]).is_err());
    }

    #[test]
    fn unverified_samples_are_dropped() {
        let line = LINE.replace("verify_ok=1", "verify_ok=0");
        assert!(parse_mmap_cost_records(&line).is_empty());
    }
}
