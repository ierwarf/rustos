//! Bounded diagnostics for one-shot IPC reply rejection.
//!
//! Rejection is a correctness signal, but debugcon is synchronous under KVM.
//! Preserve exact early evidence and cumulative later evidence without letting
//! a timeout storm consume the CPU needed by the reply owner to recover.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kernel_ipc_runtime::api::IpcError;

const MAX_REPLY_REJECTION_SUMMARIES_PER_SECOND: u8 = 1;
const EARLY_REPLY_REJECTION_SAMPLES: usize = 4;
static IPC_REPLY_REJECTION_COUNT: AtomicUsize = AtomicUsize::new(0);
static IPC_REPLY_REJECTION_RATE_STATE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Reply ids whose caller abandoned an already-delivered call.
///
/// A rejected reply reports `InvalidHandle` whether the service used a bogus
/// capability or its caller simply gave up waiting, and those are opposite
/// diagnoses: one is a broken service, the other is a service that cannot keep
/// pace with the deadline its callers are setting. A measured run produced
/// ninety-eight rejections that all read `reason=1` and said nothing about
/// which. The cancellation path knows, so it leaves the id here for the
/// rejection to find - a small ring, because only the recent ones can still be
/// in flight.
const ABANDONED_REPLY_RING: usize = 32;
static ABANDONED_REPLIES: [AtomicU64; ABANDONED_REPLY_RING] =
    [const { AtomicU64::new(0) }; ABANDONED_REPLY_RING];
static ABANDONED_REPLY_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Reason code for a reply rejected because its caller abandoned the call.
const REPLY_REJECTION_REASON_ABANDONED: u64 = 7;

pub(super) fn note_reply_abandoned_in_flight(reply: u64) {
    if reply == 0 {
        return;
    }
    // ORDERING: Relaxed is exact; the ring owns diagnostics only and a racing
    // writer can at worst overwrite an entry that was about to be consumed.
    let index = ABANDONED_REPLY_CURSOR.fetch_add(1, Ordering::Relaxed) % ABANDONED_REPLY_RING;
    ABANDONED_REPLIES[index].store(reply, Ordering::Relaxed);
}

/// Consumes the record if this reply was abandoned, so a later genuine
/// `InvalidHandle` on a recycled id is not misattributed.
fn take_reply_abandonment(reply: u64) -> bool {
    if reply == 0 {
        return false;
    }
    ABANDONED_REPLIES.iter().any(|slot| {
        // ORDERING: Relaxed is exact for a diagnostic claim; the compare
        // exchange is what makes the record single-use.
        slot.compare_exchange(reply, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    })
}

pub(super) fn record_ipc_reply_rejection(reply: u64, receiver_process_id: u64, err: IpcError) {
    let reason = match err {
        IpcError::InvalidHandle if take_reply_abandonment(reply) => {
            REPLY_REJECTION_REASON_ABANDONED
        }
        IpcError::InvalidHandle => 1_u64,
        IpcError::PermissionDenied => 2,
        IpcError::PeerClosed => 3,
        IpcError::BufferTooSmall => 4,
        IpcError::InvalidArgument => 5,
        IpcError::NoMemory => 6,
    };
    // ORDERING: this counter owns diagnostics only; AcqRel keeps overflow
    // saturation and the later published cumulative total in one order.
    let total = IPC_REPLY_REJECTION_COUNT
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map_or(usize::MAX, |previous| previous + 1);
    let owner_and_reason = ((receiver_process_id & 0xffff_ffff) << 32) | reason;
    if total <= EARLY_REPLY_REJECTION_SAMPLES {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "ipc-reply-rejected",
            reply,
            owner_and_reason,
        );
        return;
    }
    let window = crate::arch::rtc::ticks() / crate::arch::rtc::ticks_per_second().max(1);
    if diagnostic_rate_limit_permit(
        &IPC_REPLY_REJECTION_RATE_STATE,
        window,
        MAX_REPLY_REJECTION_SUMMARIES_PER_SECOND,
    ) {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "ipc-reply-rejected-summary",
            total as u64,
            owner_and_reason,
        );
    }
}

pub(in crate::user::syscall::linux) fn diagnostic_rate_limit_permit(
    state: &AtomicU64,
    window: u64,
    limit: u8,
) -> bool {
    if limit == 0 {
        return false;
    }
    const COUNT_BITS: u32 = 8;
    const COUNT_MASK: u64 = (1_u64 << COUNT_BITS) - 1;
    let window = window.min(u64::MAX >> COUNT_BITS);
    loop {
        // ORDERING: the packed diagnostic window has no payload; Relaxed reads
        // plus the successful AcqRel compare-exchange provide exact admission.
        let previous = state.load(Ordering::Relaxed);
        let previous_window = previous >> COUNT_BITS;
        let previous_count = (previous & COUNT_MASK) as u8;
        let next = if previous_window != window {
            (window << COUNT_BITS) | 1
        } else {
            if previous_count >= limit {
                return false;
            }
            previous + 1
        };
        // ORDERING: only this CAS publishes a permit in the packed window.
        if state
            .compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rate_limit_is_exact_per_time_window() {
        let state = AtomicU64::new(u64::MAX);
        for _ in 0..4 {
            assert!(diagnostic_rate_limit_permit(&state, 7, 4));
        }
        assert!(!diagnostic_rate_limit_permit(&state, 7, 4));
        assert!(diagnostic_rate_limit_permit(&state, 8, 4));
        assert!(!diagnostic_rate_limit_permit(&state, 8, 0));
    }

    #[test]
    fn reply_rejection_summary_is_bounded_to_one_per_second() {
        let state = AtomicU64::new(u64::MAX);
        assert!(diagnostic_rate_limit_permit(
            &state,
            11,
            MAX_REPLY_REJECTION_SUMMARIES_PER_SECOND,
        ));
        assert!(!diagnostic_rate_limit_permit(
            &state,
            11,
            MAX_REPLY_REJECTION_SUMMARIES_PER_SECOND,
        ));
        assert!(diagnostic_rate_limit_permit(
            &state,
            12,
            MAX_REPLY_REJECTION_SUMMARIES_PER_SECOND,
        ));
    }
}
