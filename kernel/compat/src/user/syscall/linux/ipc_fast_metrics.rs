//! Allocation-free counters for fixed-frame IPC admission and fallback causes.
//!
//! Counter IDs are a stable diagnostic wire consumed by `xtask bench`.

#[cfg(rustos_ipc_phase_profile)]
use super::ipc_ops::debug;
use super::ipc_ops::multitask;
#[cfg(rustos_ipc_phase_profile)]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(rustos_ipc_phase_profile)]
use kernel_ps::multitask::drain_fast_ipc_eligibility_rejections;

#[cfg(rustos_ipc_phase_profile)]
const IPC_FAST_COUNTER_COUNT: usize = 23;
#[cfg(rustos_ipc_phase_profile)]
const IPC_FAST_ELIGIBILITY_COUNTER_START: usize = IPC_FAST_COUNTER_COUNT;
#[cfg(rustos_ipc_phase_profile)]
static IPC_FAST_COUNTERS: [AtomicU64; IPC_FAST_COUNTER_COUNT] =
    [const { AtomicU64::new(0) }; IPC_FAST_COUNTER_COUNT];

#[repr(usize)]
#[derive(Clone, Copy)]
// DIAGNOSTIC: Rejection-only slots are compiled out with the IPC profile.
#[cfg_attr(not(rustos_ipc_phase_profile), allow(dead_code))]
pub(super) enum IpcFastCounter {
    AdmissionAttempt = 0,
    AdmissionFallback = 1,
    ReservationPublished = 2,
    HandoffCommitted = 3,
    HandoffRejected = 4,
    ReceiverTaken = 5,
    ReplyPublished = 6,
    CallerResponse = 7,
    CallerTerminalError = 8,
    CallerDeadline = 9,
    FusedReplyPublished = 10,
    Rollback = 11,
    FallbackShape = 12,
    FallbackNoFrame = 13,
    FallbackDeadlineArm = 14,
    FallbackScheduler = 15,
    CallerMmRejected = 16,
    HandoffRejectSender = 17,
    HandoffRejectReceiver = 18,
    HandoffRejectDonation = 19,
    HandoffRejectEligibility = 20,
    HandoffRejectCustody = 21,
    HandoffRejectOrdering = 22,
}

#[inline]
pub(super) fn note_fast_ipc(counter: IpcFastCounter) {
    #[cfg(rustos_ipc_phase_profile)]
    IPC_FAST_COUNTERS[counter as usize].fetch_add(1, Ordering::Relaxed);
    #[cfg(not(rustos_ipc_phase_profile))]
    let _ = counter;
}

#[inline]
pub(super) fn note_fast_ipc_handoff_rejection(outcome: multitask::FastIpcCallHandoffOutcome) {
    #[cfg(not(rustos_ipc_phase_profile))]
    {
        let _ = outcome;
        return;
    }
    #[cfg(rustos_ipc_phase_profile)]
    {
        let counter = match outcome {
            multitask::FastIpcCallHandoffOutcome::SenderMismatch => {
                IpcFastCounter::HandoffRejectSender
            }
            multitask::FastIpcCallHandoffOutcome::ReceiverMismatch => {
                IpcFastCounter::HandoffRejectReceiver
            }
            multitask::FastIpcCallHandoffOutcome::DonationUnavailable => {
                IpcFastCounter::HandoffRejectDonation
            }
            multitask::FastIpcCallHandoffOutcome::EligibilityUnavailable => {
                IpcFastCounter::HandoffRejectEligibility
            }
            multitask::FastIpcCallHandoffOutcome::DirectCustodyUnavailable => {
                IpcFastCounter::HandoffRejectCustody
            }
            multitask::FastIpcCallHandoffOutcome::OrderingUnavailable => {
                IpcFastCounter::HandoffRejectOrdering
            }
            multitask::FastIpcCallHandoffOutcome::CommittedSameCpu
            | multitask::FastIpcCallHandoffOutcome::CommittedCrossCpu => return,
        };
        note_fast_ipc(counter);
    }
}

// DIAGNOSTIC: Housekeeping calls this only in an IPC diagnostic image.
#[cfg_attr(not(rustos_ipc_phase_profile), allow(dead_code))]
pub(crate) fn drain_fast_ipc_counters() -> usize {
    #[cfg(not(rustos_ipc_phase_profile))]
    {
        return 0;
    }
    #[cfg(rustos_ipc_phase_profile)]
    {
        let mut emitted = 0;
        for (reason, counter) in IPC_FAST_COUNTERS.iter().enumerate() {
            let count = counter.swap(0, Ordering::Relaxed);
            if count == 0 {
                continue;
            }
            debug::record_milestone(
                debug::LogCategory::Compat,
                "ipc-fastpath-counter",
                reason as u64,
                count,
            );
            emitted += 1;
        }
        for (reason, count) in drain_fast_ipc_eligibility_rejections()
            .into_iter()
            .enumerate()
        {
            if count == 0 {
                continue;
            }
            debug::record_milestone(
                debug::LogCategory::Compat,
                "ipc-fastpath-counter",
                (IPC_FAST_ELIGIBILITY_COUNTER_START + reason) as u64,
                count,
            );
            emitted += 1;
        }
        emitted
    }
}
