//! Bound observation of the synchronous handoff chain, split from the FIFO owner.
//!
//! - **Owner:** `sync_handoff` owns the per-CPU streak; this reads it.
//! - **Concurrency:** only that CPU's masked scheduler transaction writes the
//!   streak, so the same transaction reads it without ordering.
//! - **Evidence:** the bounded-chain witnesses in `sync_handoff` and the frame-loan
//!   shortcut admission in `irq_entry_tests::switch`.

use super::{MAX_SYNC_HANDOFF_CHAIN_TICKS, SYNC_HANDOFF_BUDGET};

/// Whether this CPU's bounded synchronous chain has used its ticks. Selection
/// refuses a further handoff only while a fair competitor is also ready.
pub(in crate::multitask::scheduler) fn chain_exhausted(cpu: usize) -> bool {
    SYNC_HANDOFF_BUDGET.get(cpu).is_some_and(|budget| {
        // ORDERING: Relaxed; only this CPU's masked scheduler transaction writes it.
        budget.streak.load(core::sync::atomic::Ordering::Relaxed) >= MAX_SYNC_HANDOFF_CHAIN_TICKS
    })
}
