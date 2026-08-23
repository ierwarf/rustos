//! Scheduler-derived synchronous-call admission.
//!
//! This boundary samples scheduling authority before entering the IPC object
//! locks and packages only the neutral reply-custody token for transport.

use super::*;

pub(super) struct CallSchedulingAdmission {
    pub(super) priority: EndpointCallPriority,
    pub(super) scheduling_context: kernel_ipc_runtime::api::ReplySchedulingContextCustody,
    pub(super) donation_required: bool,
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
    let donation_required = true;
    Ok(CallSchedulingAdmission {
        priority,
        scheduling_context,
        donation_required,
    })
}
