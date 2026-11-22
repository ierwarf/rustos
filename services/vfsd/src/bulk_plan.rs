//! Sizing rules for vfsd's parallel storaged read lanes (`bulk_lanes`).
//!
//! Pure arithmetic kept in the library so it is unit-tested; the lanes
//! themselves run in the service binary.

/// Reads below this stay on the calling thread: a lane wake-up costs more than
/// the few calls it would overlap.
pub const PARALLEL_MIN_BYTES: usize = 2 * 1024 * 1024;
/// Whether a sequential read leaves the helpers reading the next range.
pub const LOOKAHEAD_ENABLED: bool = true;
/// Largest read that is looked ahead, and the size of the lookahead buffer.
pub const PREFETCH_BYTES: usize = 4 * 1024 * 1024;

/// Size of the lookahead read that follows a read ending at block `end`: the
/// same length, shortened to the device end and to whole blocks, never below
/// `PARALLEL_MIN_BYTES`. `None` skips the lookahead.
pub fn lookahead_bytes(
    end: u64,
    len: usize,
    block_size: usize,
    device_blocks: u64,
) -> Option<usize> {
    if len > PREFETCH_BYTES || block_size == 0 {
        return None;
    }
    let remaining = device_blocks.checked_sub(end)?;
    let bytes = usize::try_from(remaining.saturating_mul(block_size as u64)).unwrap_or(usize::MAX);
    let bytes = bytes.min(len) / block_size * block_size;
    (bytes >= PARALLEL_MIN_BYTES).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 512;

    #[test]
    fn lookahead_repeats_the_read_size_and_stops_at_the_device_end() {
        let len = 4 * 1024 * 1024;
        let blocks = (len / BS) as u64;
        assert_eq!(lookahead_bytes(blocks, len, BS, blocks * 10), Some(len));
        // Half of the next read remains: shortened, still a whole parallel read.
        assert_eq!(
            lookahead_bytes(blocks, len, BS, blocks + blocks / 2),
            Some(len / 2)
        );
        // Less than the parallel threshold remains: not worth waking the lanes.
        assert_eq!(lookahead_bytes(blocks, len, BS, blocks + 1024), None);
        assert_eq!(lookahead_bytes(blocks, len, BS, blocks), None);
        // Past the end never underflows.
        assert_eq!(lookahead_bytes(blocks + 1, len, BS, blocks), None);
    }

    #[test]
    fn lookahead_is_bounded_by_its_private_buffer() {
        assert_eq!(
            lookahead_bytes(0, PREFETCH_BYTES + BS, BS, u64::MAX / 2),
            None
        );
        assert_eq!(
            lookahead_bytes(0, PREFETCH_BYTES, BS, u64::MAX / 2),
            Some(PREFETCH_BYTES)
        );
        assert_eq!(lookahead_bytes(0, PREFETCH_BYTES, 0, 1 << 20), None);
    }

    #[test]
    fn lookahead_rounds_down_to_whole_blocks() {
        let end = 8;
        // 2 MiB + 100 bytes remain: only whole blocks, and it is still >= minimum.
        let device_blocks = end + (PARALLEL_MIN_BYTES / BS) as u64 + 1;
        assert_eq!(
            lookahead_bytes(end, PREFETCH_BYTES, BS, device_blocks),
            Some(PARALLEL_MIN_BYTES + BS)
        );
    }
}
