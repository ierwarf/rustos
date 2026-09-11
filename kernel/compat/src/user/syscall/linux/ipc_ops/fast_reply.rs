//! Deferred fast-reply settlement fused with the receiver's block transition.

use kernel_ipc_runtime::api::{KernelEndpointHandle, ReplyCompletion};

use crate::multitask;

pub(super) struct DeferredFastReply {
    pending: Option<(u64, ReplyCompletion)>,
}

impl DeferredFastReply {
    const fn new(reply: u64, completion: ReplyCompletion) -> Self {
        Self {
            pending: Some((reply, completion)),
        }
    }

    pub(super) fn settle_without_blocking(&mut self) {
        let Some((reply, completion)) = self.pending.take() else {
            return;
        };
        if multitask::complete_fast_ipc_reply_wake_handoff_with_custody(reply, completion) {
            multitask::request_deferred_reschedule();
        }
    }

    pub(super) fn commit_block_and_yield(&mut self) -> Option<bool> {
        let (reply, completion) = self
            .pending
            .take()
            .expect("deferred fast reply consumed more than once");
        let committed =
            multitask::commit_block_current_task_with_fast_reply_and_yield(reply, completion);
        if committed != Some(true)
            && multitask::complete_fast_ipc_reply_wake_handoff_with_custody(reply, completion)
        {
            multitask::request_deferred_reschedule();
        }
        committed
    }
}

impl Drop for DeferredFastReply {
    fn drop(&mut self) {
        self.settle_without_blocking();
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::user::syscall::linux) fn recv_with_sender_blocking_prepared_after_fast_reply(
    endpoint: KernelEndpointHandle,
    task_id: u64,
    retained_mm: &multitask::RetainedCurrentUserAddressSpace,
    request_ptr: u64,
    request_capacity: usize,
    reply_cap_ptr: u64,
    sender_pid_ptr: u64,
    sender_tid_ptr: u64,
    reply: u64,
    completion: ReplyCompletion,
) -> Result<(usize, bool), (i64, bool)> {
    super::recv_with_sender_blocking_prepared_inner(
        endpoint,
        task_id,
        retained_mm,
        request_ptr,
        request_capacity,
        reply_cap_ptr,
        sender_pid_ptr,
        sender_tid_ptr,
        None,
        Some(DeferredFastReply::new(reply, completion)),
    )
}
