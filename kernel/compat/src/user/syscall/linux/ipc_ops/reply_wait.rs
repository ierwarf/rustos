//! The synchronous reply wait for a `rustos_ipc_call`.
//!
//! - **Owner:** `kernel-compat` owns the caller side of a synchronous call;
//!   the endpoint queue and the wake itself belong to the IPC runtime and the
//!   scheduler.
//! - **Boundary:** the deadline, the handle capacity, and the reply handle all
//!   arrive from an untrusted caller and are bounded before the wait arms.
//! - **Lifecycle:** poll the endpoint, arm the block and the deadline waiter,
//!   re-poll, then block; every exit path disarms the deadline waiter.
//! - **Concurrency:** the post-arm re-poll is the race fix. A reply that lands
//!   between the first take and the block arm must not park the caller.
//! - **Failure:** an expired deadline, a retired caller, or a revoked reply
//!   handle returns an errno rather than parking the task.
//! - **Forbidden:** no wait that can outlive its deadline, and no exit that
//!   leaves a deadline waiter armed.
//! - **Evidence:** `synchronous-ipc-handoff`, `ipc-reply-deadline`.
//!
//! Split out of `ipc_ops.rs` under that file's registered split plan. The wait
//! is a self-contained phase of the call: it neither publishes services nor
//! touches grant state, so it carries none of the anchors the rest of the file
//! does.

use super::*;

pub(super) fn wait_for_reply_with_deadline(
    reply: KernelReplyHandle,
    handle_capacity: usize,
    deadline_tick: u64,
) -> Result<(Vec<u8>, Vec<KernelTransferredHandle>), i64> {
    let caller_task_id = multitask::current_task_id().ok_or(LINUX_EINVAL)?;
    let mut cycle_mark = phase_mark();
    loop {
        // Sample expiry on both sides of the queue take. A reply cannot win
        // if expiry was already visible or becomes visible during the take.
        let expired_before_take = reply_deadline_expired(deadline_tick);
        cycle_mark = charge_phase(IpcCallPhase::WaitDeadlineSample, cycle_mark);
        if !rustos_user_abi::deadline::reply_observation_allows_publication(
            expired_before_take,
            false,
        ) {
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::DeadlineBeforeArm);
            return Err(LINUX_ETIMEDOUT);
        }
        let take = take_endpoint_response_for_wait(reply, handle_capacity);
        cycle_mark = charge_phase(IpcCallPhase::WaitTake, cycle_mark);
        match take {
            Ok(Some(response)) => {
                if !rustos_user_abi::deadline::reply_observation_allows_publication(
                    expired_before_take,
                    reply_deadline_expired(deadline_tick),
                ) {
                    disarm_reply_deadline_waiter(caller_task_id);
                    drop_transfer_descriptors(response.1.as_slice());
                    return Err(LINUX_ETIMEDOUT);
                }
                disarm_reply_deadline_waiter(caller_task_id);
                let _ = charge_phase(IpcCallPhase::WaitDisarm, cycle_mark);
                return Ok(response);
            }
            Ok(None) => {}
            Err(errno) => {
                disarm_reply_deadline_waiter(caller_task_id);
                if errno == LINUX_EOVERFLOW {
                    // A byte-only caller cannot own a response descriptor.
                    // Cancel while the reply remains live so the normal
                    // transfer result reaches deferred provider cleanup.
                    cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::TransferCapacity);
                }
                record_ipc_reply_wait_failure(reply, caller_task_id, errno);
                return Err(errno);
            }
        }
        cycle_mark = phase_mark();
        if !multitask::arm_block_current_task() {
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::InvalidArm);
            return Err(LINUX_EINVAL);
        }
        if !arm_reply_deadline_waiter(caller_task_id, deadline_tick) {
            let _ = multitask::cancel_block_current_task();
            multitask::yield_now();
            continue;
        }
        cycle_mark = charge_phase(IpcCallPhase::WaitArm, cycle_mark);
        // Re-poll after arming. If the replier completed the response between our
        // first take and arming, the wake_task call landed before arm_block, so
        // wake_armed would be set but no further wake arrives; re-checking the
        // queue here picks up that response without sleeping.
        let expired_before_take = reply_deadline_expired(deadline_tick);
        cycle_mark = charge_phase(IpcCallPhase::WaitDeadlineSample, cycle_mark);
        if !rustos_user_abi::deadline::reply_observation_allows_publication(
            expired_before_take,
            false,
        ) {
            disarm_reply_deadline_waiter(caller_task_id);
            let _ = multitask::wake_task(caller_task_id);
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::DeadlineAfterArm);
            return Err(LINUX_ETIMEDOUT);
        }
        let take = take_endpoint_response_for_wait(reply, handle_capacity);
        cycle_mark = charge_phase(IpcCallPhase::WaitTake, cycle_mark);
        match take {
            Ok(Some(response)) => {
                if !rustos_user_abi::deadline::reply_observation_allows_publication(
                    expired_before_take,
                    reply_deadline_expired(deadline_tick),
                ) {
                    disarm_reply_deadline_waiter(caller_task_id);
                    let _ = multitask::cancel_block_current_task();
                    drop_transfer_descriptors(response.1.as_slice());
                    return Err(LINUX_ETIMEDOUT);
                }
                disarm_reply_deadline_waiter(caller_task_id);
                let _ = multitask::cancel_block_current_task();
                let _ = charge_phase(IpcCallPhase::WaitDisarm, cycle_mark);
                return Ok(response);
            }
            Ok(None) => {}
            Err(errno) => {
                disarm_reply_deadline_waiter(caller_task_id);
                let _ = multitask::cancel_block_current_task();
                if errno == LINUX_EOVERFLOW {
                    // See the pre-arm case: capacity rejection is terminal
                    // for this caller and must retire a prepared reply handle.
                    cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::TransferCapacity);
                }
                record_ipc_reply_wait_failure(reply, caller_task_id, errno);
                return Err(errno);
            }
        }
        if reply_deadline_expired(deadline_tick) {
            disarm_reply_deadline_waiter(caller_task_id);
            let _ = multitask::wake_task(caller_task_id);
            cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::DeadlineAfterArm);
            return Err(LINUX_ETIMEDOUT);
        }
        cycle_mark = phase_mark();
        let committed = multitask::commit_block_current_task_and_yield();
        cycle_mark = charge_phase(IpcCallPhase::WaitBlocked, cycle_mark);
        match committed {
            Some(true) => {
                disarm_reply_deadline_waiter(caller_task_id);
                cycle_mark =
                    charge_phase(IpcCallPhase::WaitDisarm, cycle_mark);
            }
            Some(false) => {
                disarm_reply_deadline_waiter(caller_task_id);
                cycle_mark =
                    charge_phase(IpcCallPhase::WaitDisarm, cycle_mark);
                continue;
            }
            None => {
                disarm_reply_deadline_waiter(caller_task_id);
                cancel_reply_wait(reply, caller_task_id, ReplyCancelReason::InvalidCommit);
                return Err(LINUX_EINVAL);
            }
        }
    }
}

type EndpointWaitResponse = (Vec<u8>, Vec<KernelTransferredHandle>);

fn take_endpoint_response_for_wait(
    reply: KernelReplyHandle,
    handle_capacity: usize,
) -> Result<Option<EndpointWaitResponse>, i64> {
    match kernel_ipc_runtime::api::take_endpoint_response_detailed(reply, handle_capacity) {
        Ok(kernel_ipc_runtime::api::EndpointResponseTake::Pending) => Ok(None),
        Ok(kernel_ipc_runtime::api::EndpointResponseTake::Response(response)) => {
            let _ = multitask::release_ipc_priority(reply.raw());
            Ok(Some(response))
        }
        Ok(kernel_ipc_runtime::api::EndpointResponseTake::Error {
            error,
            discarded_request_handles,
        }) => {
            let _ = multitask::release_ipc_priority(reply.raw());
            drop_transfer_descriptors(discarded_request_handles.as_slice());
            Err(ipc_error_to_linux_errno(error))
        }
        Err(err) => Err(ipc_error_to_linux_errno(err)),
    }
}
