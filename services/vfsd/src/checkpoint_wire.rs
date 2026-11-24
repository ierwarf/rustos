//! Pure cache-range encodings owned by vfsd.
//!
//! These live in the library target because the `vfsd` binary is
//! `no_std`/`no_main` and its test harness is disabled; a `#[test]` kept beside
//! the service loop would never execute.

/// Copies the cached file bytes at `start..` into `dest`; an offset at or past
/// EOF (or beyond `usize`) copies nothing.
pub fn copy_file_cache_range(bytes: &[u8], start: u64, dest: &mut [u8]) -> usize {
    let Ok(start) = usize::try_from(start) else {
        return 0;
    };
    let Some(source) = bytes.get(start..) else {
        return 0;
    };
    let count = source.len().min(dest.len());
    dest[..count].copy_from_slice(&source[..count]);
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_file_ranges_are_exact_and_eof_bounded() {
        let bytes = b"0123456789";
        let mut out = [0xaa; 4];
        assert_eq!(copy_file_cache_range(bytes, 3, &mut out), 4);
        assert_eq!(&out, b"3456");
        assert_eq!(copy_file_cache_range(bytes, 9, &mut out), 1);
        assert_eq!(out[0], b'9');
        assert_eq!(copy_file_cache_range(bytes, 10, &mut out), 0);
        assert_eq!(copy_file_cache_range(bytes, u64::MAX, &mut out), 0);
    }
}
