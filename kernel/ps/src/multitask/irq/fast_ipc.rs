//! Encoding of fast-IPC outcomes in the software-schedule trap frame.

use super::SavedContext;
use crate::multitask::scheduler::FastIpcCallHandoffOutcome;

pub(super) fn stamp_fast_ipc_call_outcome(
    context_ptr: *mut SavedContext,
    outcome: FastIpcCallHandoffOutcome,
) -> *mut SavedContext {
    let code = match outcome {
        FastIpcCallHandoffOutcome::CommittedSameCpu => 0,
        FastIpcCallHandoffOutcome::CommittedCrossCpu => 1,
        FastIpcCallHandoffOutcome::SenderMismatch => 2,
        FastIpcCallHandoffOutcome::ReceiverMismatch => 3,
        FastIpcCallHandoffOutcome::DonationUnavailable => 4,
        FastIpcCallHandoffOutcome::EligibilityUnavailable => 5,
        FastIpcCallHandoffOutcome::DirectCustodyUnavailable => 6,
        FastIpcCallHandoffOutcome::OrderingUnavailable => 7,
    };
    // SAFETY: only the software interrupt callback mutates its owned outgoing
    // frame, and RAX is the declared result channel consumed after this exact
    // task continuation is restored.
    unsafe { (*context_ptr).rax = code };
    context_ptr
}
