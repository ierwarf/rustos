//! Bounded consistency interval for one complete VMA range rewrite.
//! The process writer already owns serialization; fault readers never spin.

use super::{MAX_PUBLICATION_SEQUENCE, PagerVmaError, ProcessHandle};
use crate::multitask::slot_dir::AtomicDir;
use core::sync::atomic::{AtomicU64, Ordering};

// Per-slot sequences protect payloads, not a range rewrite's temporary empty
// publications or movement between slots. Never reset this counter on reuse:
// exact process/MM identity is still checked separately and no scan may ABA.
pub(super) static REWRITE_EPOCHS: AtomicDir<AtomicU64> = AtomicDir::new();

/// Publishes the epoch cells covering `index` before that slot can exist.
pub(in crate::multitask) fn ensure_slot_storage(index: usize) -> bool {
    REWRITE_EPOCHS.ensure(index, || AtomicU64::new(0))
}

/// Only a consistency interval, never VMA, process or frame authority. The
/// caller already owns this process's writer lock; fault readers never take it.
pub(super) struct RangeRewrite {
    sequence: &'static AtomicU64,
    before: u64,
}
impl RangeRewrite {
    pub(super) fn begin(handle: ProcessHandle) -> Result<Self, PagerVmaError> {
        let sequence = REWRITE_EPOCHS
            .get(handle.index())
            .ok_or(PagerVmaError::Stale)?;
        let before = sequence.load(Ordering::Relaxed);
        if before & 1 != 0 || before > MAX_PUBLICATION_SEQUENCE {
            return Err(PagerVmaError::Pressure);
        }
        // ORDERING: invalidate the whole scan before the first slot withdrawal
        // and installer drain. No IRQ-off reader spins on this odd interval.
        sequence.store(before + 1, Ordering::Release);
        core::sync::atomic::fence(Ordering::SeqCst);
        Ok(Self { sequence, before })
    }
}
impl Drop for RangeRewrite {
    fn drop(&mut self) {
        // ORDERING: commit only after every replacement (or original rollback)
        // publication. Checked headroom at begin forbids wrap and ABA.
        self.sequence.store(self.before + 2, Ordering::Release);
    }
}

pub(super) fn read_rewrite_epoch<R>(
    handle: ProcessHandle,
    scan: impl FnOnce() -> Result<R, PagerVmaError>,
) -> Result<R, PagerVmaError> {
    let sequence = REWRITE_EPOCHS
        .get(handle.index())
        .ok_or(PagerVmaError::Stale)?;
    // ORDERING: acquire the writer's completed set before any slot read.
    let before = sequence.load(Ordering::Acquire);
    if before & 1 != 0 {
        return Err(PagerVmaError::Unstable);
    }
    let result = scan();
    // ORDERING: retain ALL slot reads, including negative extent filters and
    // permission errors, before the final whole-rewrite sequence observation.
    core::sync::atomic::fence(Ordering::Acquire);
    if sequence.load(Ordering::Relaxed) != before {
        Err(PagerVmaError::Unstable)
    } else {
        result
    }
}
