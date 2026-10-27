//! Scheduler-derived synchronous-call admission.
//!
//! This boundary samples scheduling authority before entering the IPC object
//! locks and packages only the neutral reply-custody token for transport.

use super::*;

pub(super) struct CallSchedulingAdmission {
    pub(super) priority: EndpointCallPriority,
    pub(super) scheduling_context: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
}

/// Read-only transport race discriminator after the original scheduler wake
/// returned. This never grants funding, a generic wake, or execution custody.
pub(super) fn refused_wake_can_keep_delivered_reply(
    reply: KernelReplyHandle, caller: u64, donation_admitted: bool,
) -> bool {
    kernel_ipc_runtime::api::endpoint_reply_custody_returned(reply, caller)
        || (donation_admitted
            && (kernel_ipc_runtime::api::endpoint::received_request_retains_custody(reply, caller)
                // A fast frame its receiver already took or answered: the
                // request is executing (or done) under that receiver, and
                // cancelling it lost a service launch at eight vCPUs.
                || kernel_ipc_runtime::api::endpoint::fast_request_owned_by_receiver(reply, caller)
                // Still queued with admitted custody: exactly the state an
                // enqueue with no parked receiver leaves, served by the next
                // receive. Cancelling it raced that receive and killed a
                // service launch at eight vCPUs.
                || kernel_ipc_runtime::api::endpoint::queued_request_retains_custody(reply, caller)))
}

/// Bounded retries of a cold call handoff refused with `ReceiverMismatch`.
pub(super) const RECEIVER_REFUSAL_RETRIES: u32 = 16;

/// `ReceiverMismatch` is returned before any scheduler effect. While the exact
/// receiver still holds its receive phase on this endpoint, the refusal is a
/// transient binding window (for example the receiver is between its receive
/// arm and block commit on another CPU), so retry the SAME handoff a bounded
/// number of times instead of failing the caller's request with ENOSPC.
pub(super) fn retry_transient_receiver_refusal<T>(
    mut commit: impl FnMut() -> Result<T, multitask::FastIpcCallHandoffOutcome>,
    mut receiver_still_waiting: impl FnMut() -> bool,
) -> Result<T, multitask::FastIpcCallHandoffOutcome> {
    let mut attempt = 0;
    loop {
        match commit() {
            Err(multitask::FastIpcCallHandoffOutcome::ReceiverMismatch)
                if attempt < RECEIVER_REFUSAL_RETRIES && receiver_still_waiting() =>
            {
                attempt += 1;
                for _ in 0..(32_u32 << attempt.min(8)) {
                    core::hint::spin_loop();
                }
            }
            result => return result,
        }
    }
}

pub(super) fn reserve(task_id: u64) -> Result<CallSchedulingAdmission, i64> {
    let admission = multitask::reserve_ipc_call_donation(task_id);
    let Some(scheduling_context) = admission
        .scheduling_context
        .zip(admission.scheduling_context_owner_task_id)
        .and_then(|(identity, owner_task_id)| {
            kernel_ipc_runtime::api::ReplySchedulingContextCustody::new_with_owner(
                identity,
                owner_task_id,
                task_id,
            )
        })
    else {
        if admission.donation_reserved {
            let _ = multitask::cancel_ipc_priority_reservation(task_id);
        }
        return Err(LINUX_EINVAL);
    };
    if !admission.donation_reserved {
        // A synchronous call may not publish a reply without bounded custody
        // for its exact scheduling context. Losing this edge would let a
        // passive server execute outside the caller/domain budget.
        return Err(LINUX_ENOSPC);
    }
    let priority = if admission.system_class {
        EndpointCallPriority::System
    } else {
        EndpointCallPriority::Ordinary
    };
    Ok(CallSchedulingAdmission {
        priority,
        scheduling_context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_receiver_refusal_retries_only_while_the_receiver_still_waits() {
        use multitask::FastIpcCallHandoffOutcome as Refusal;
        let mut calls = 0;
        let result = retry_transient_receiver_refusal(|| {
            calls += 1;
            if calls < 3 { Err(Refusal::ReceiverMismatch) } else { Ok(calls) }
        }, || true);
        assert_eq!(result, Ok(3), "a transient window must not fail the call");

        let mut calls = 0;
        let result: Result<(), _> = retry_transient_receiver_refusal(|| {
            calls += 1;
            Err(Refusal::ReceiverMismatch)
        }, || false);
        assert_eq!((result, calls), (Err(Refusal::ReceiverMismatch), 1),
            "a receiver that left its receive phase is not retried");

        let mut calls = 0;
        let result: Result<(), _> = retry_transient_receiver_refusal(|| {
            calls += 1;
            Err(Refusal::DonationUnavailable)
        }, || true);
        assert_eq!((result, calls), (Err(Refusal::DonationUnavailable), 1),
            "only the effect-free receiver refusal is retried");

        let mut calls = 0;
        let result: Result<(), _> = retry_transient_receiver_refusal(|| {
            calls += 1;
            Err(Refusal::ReceiverMismatch)
        }, || true);
        assert_eq!((result, calls), (Err(Refusal::ReceiverMismatch), RECEIVER_REFUSAL_RETRIES + 1),
            "retries are bounded");
    }

    #[test]
    fn actual_received_call_survives_a_refused_sender_wake_without_certifying_queued_or_uncustodied_requests() {
        use kernel_ipc_runtime::api::{self as ipc, endpoint};
        use kernel_object::api::{ObjectIdentity, ObjectKind, ObjectOwner};
        const CALLER: u64 = 953901;
        const RECEIVER: u64 = 953902;
        let endpoint = endpoint::create_for_process(RECEIVER).unwrap();
        // Earlier SC admission is an explicit native input; this witness owns
        // actual transport objects, NOT a scheduler/execution grant or IRQ model.
        let identity = ObjectIdentity::new(ObjectOwner::Ps, ObjectKind::SchedulingContext, 3, CALLER).unwrap();
        let custody = ipc::ReplySchedulingContextCustody::new(identity, CALLER).unwrap();
        assert!(!ipc::add_endpoint_receiver_waiter_with_capacity(endpoint, RECEIVER, 16).unwrap());
        let (reply, candidate) = endpoint::enqueue_call_with_handles_priority_and_custody(
            endpoint, CALLER, b"request", &[], ipc::EndpointCallPriority::Ordinary, custody).unwrap();
        assert_eq!(candidate, Some(RECEIVER));
        // Still queued with admitted custody is the no-parked-receiver state:
        // the next receive serves it. Cancelling it here raced that receive
        // and killed a service launch at eight vCPUs.
        assert!(refused_wake_can_keep_delivered_reply(reply, CALLER, true),
            "a queued request with admitted custody waits for the next receive");
        assert!(!refused_wake_can_keep_delivered_reply(reply, CALLER, false),
            "without admitted donation a queued request is still cancelled");
        assert!(matches!(endpoint::receive_or_wait(endpoint, RECEIVER, 16, 0, false).unwrap(),
            endpoint::EndpointReceive::Queued((actual, _, _, caller)) if actual == reply && caller == CALLER));
        let delivered = refused_wake_can_keep_delivered_reply(reply, CALLER, true);
        assert!(!refused_wake_can_keep_delivered_reply(reply, CALLER, false));
        assert!(!refused_wake_can_keep_delivered_reply(reply, CALLER + 1, true));
        let completion = ipc::complete_endpoint_reply_for_process_with_custody(reply, RECEIVER, b"ok").unwrap();
        assert_eq!(completion.scheduling_context, Some(custody));
        assert!(refused_wake_can_keep_delivered_reply(reply, CALLER, false),
            "the independently completed reply remains a terminal race proof");
        assert!(matches!(endpoint::take_response_detailed(reply, 0).unwrap(),
            ipc::EndpointResponseTake::Response(_)));
        assert!(!refused_wake_can_keep_delivered_reply(reply, CALLER, true));
        let (uncustodied, _) = endpoint::enqueue_call(endpoint, CALLER, b"no-custody").unwrap();
        assert!(matches!(endpoint::receive_or_wait(endpoint, RECEIVER, 16, 0, false).unwrap(),
            endpoint::EndpointReceive::Queued(_)));
        assert!(!refused_wake_can_keep_delivered_reply(uncustodied, CALLER, true));
        ipc::cancel_endpoint_call_with_transfers(uncustodied, CALLER).unwrap();
        let _ = ipc::fail_endpoints_owned_by_process(RECEIVER, ipc::IpcError::PeerClosed);
        assert!(delivered, "actual request consumption must not be cancelled as a failed wake");
    }
}
