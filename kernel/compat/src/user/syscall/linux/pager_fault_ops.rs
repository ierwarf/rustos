//! Owner: pagerd owns anonymous-memory policy; compat owns syscall admission.
//! Boundary: versioned wait/reply wires cross one exact live pager endpoint.
//! Lifecycle: register, block, dispatch, bind donation, then reply or cancel.
//! Concurrency: endpoint generation and worker task bind each one-shot token.
//! Failure: malformed, stale, foreign, or unbound operations fail closed.
//! Forbidden: generic pager IPC, wildcard workers, and policy in ring0.
//! Evidence: PagerFaultSlotLifecycle plus exact rendezvous syscall tests.

use super::*;

use crate::multitask as ps_api;
use rustos_user_abi::pager::{PagerEndpointCapabilityWire, VM_PROT_READ};
use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_PAGER_POLICY, IPC_SERVICE_PAGERD, PAGER_FAULT_RENDEZVOUS_ABI_VERSION,
    RustosPagerFaultReplyArgs, RustosPagerFaultWaitArgs, SYS_RUSTOS_PAGER_FAULT_REPLY,
    SYS_RUSTOS_PAGER_FAULT_WAIT,
};

pub(super) const fn is_pager_fault_syscall(number: u64) -> bool {
    matches!(
        number,
        SYS_RUSTOS_PAGER_FAULT_WAIT | SYS_RUSTOS_PAGER_FAULT_REPLY
    )
}

pub(super) fn dispatch_pager_fault_syscall(number: u64, args: u64) -> u64 {
    match number {
        SYS_RUSTOS_PAGER_FAULT_WAIT => syscall_linux_rustos_pager_fault_wait(args),
        SYS_RUSTOS_PAGER_FAULT_REPLY => syscall_linux_rustos_pager_fault_reply(args),
        _ => linux_errno(LINUX_ENOSYS),
    }
}

#[inline]
fn pager_rendezvous_errno(error: ps_api::PagerFaultSlotError) -> u64 {
    match error {
        ps_api::PagerFaultSlotError::Malformed => linux_errno(LINUX_EINVAL),
        ps_api::PagerFaultSlotError::Pressure => linux_errno(LINUX_EBUSY),
        ps_api::PagerFaultSlotError::Stale | ps_api::PagerFaultSlotError::Transition => {
            linux_errno(LINUX_EAGAIN)
        }
        ps_api::PagerFaultSlotError::Authority => linux_errno(LINUX_EPERM),
    }
}

/// Reconstructs the single live pagerd endpoint identity that was stamped into
/// anonymous VMAs at admission. The service capability authorizes this
/// privileged receive, while this exact identity prevents a waiter for any
/// other endpoint from consuming a fault dispatch.
fn current_pager_endpoint() -> Option<PagerEndpointCapabilityWire> {
    let identity = ipc_ops::service_endpoint(IPC_SERVICE_PAGERD)?.identity()?;
    let endpoint = PagerEndpointCapabilityWire {
        slot: identity.slot(),
        generation: identity.generation(),
        rights: u64::from(VM_PROT_READ),
    };
    endpoint.has_authority().then_some(endpoint)
}

#[inline]
fn write_dispatch(
    reservation: ps_api::PagerFaultReservation,
    pager_task_id: u64,
    dispatch_out: u64,
) -> u64 {
    let dispatch = crate::pager::dispatch_wire(reservation);
    if let Err(error) = usermem::write_current_user_struct(dispatch_out, &dispatch) {
        // A dispatch whose pagerd output buffer is no longer writable must
        // complete as a deny. Leaving the exact fault owner blocked would
        // turn a service-local memory error into an unbounded kernel wait.
        let _ = crate::pager::reject_rendezvous_dispatch(reservation, pager_task_id);
        return linux_errno(address_space_error_to_linux_errno(error));
    }

    // The fixed fault slot is the admission proof. Once pagerd is executing in
    // ordinary syscall context, bind the exact worker directly to the fault
    // owner's effective scheduling context; no second donor reservation can
    // alias an unrelated in-flight IPC edge.
    let donor_task_id = reservation.request.task_id;
    if !ps_api::inherit_pager_fault_priority(reservation.token, donor_task_id, pager_task_id) {
        let _ = crate::pager::reject_rendezvous_dispatch(reservation, pager_task_id);
        return linux_errno(LINUX_EBUSY);
    }
    0
}

fn wait_for_dispatch(
    pager_task_id: u64,
    endpoint: PagerEndpointCapabilityWire,
    dispatch_out: u64,
) -> u64 {
    if let Some(reservation) = ps_api::take_next_pager_fault_for_rendezvous(pager_task_id, endpoint)
    {
        return write_dispatch(reservation, pager_task_id, dispatch_out);
    }
    if !ps_api::arm_block_current_task_on_pager_service() {
        return linux_errno(LINUX_EBUSY);
    }
    if !ps_api::register_pager_fault_waiter(pager_task_id, endpoint) {
        let _ = ps_api::cancel_block_current_task();
        return linux_errno(LINUX_EBUSY);
    }

    // Close the register-before-sleep race. A fault that publishes here either
    // consumes this waiter and wins the scheduler wake, or is claimed below
    // before this worker gives up the CPU.
    if let Some(reservation) = ps_api::take_next_pager_fault_for_rendezvous(pager_task_id, endpoint)
    {
        let _ = ps_api::unregister_pager_fault_waiter(pager_task_id);
        let _ = ps_api::cancel_block_current_task();
        return write_dispatch(reservation, pager_task_id, dispatch_out);
    }

    // The same close for the pager's *other* arrival source. A control request
    // published before this wait armed cannot wake it - there was nothing
    // parked to wake - so the pager would sleep on a fault that may never come
    // while a caller spent its whole reply deadline. Re-checking the published
    // edge after arming is what makes both sources race-free.
    if ipc_ops::take_pending_pager_control_request() {
        let _ = ps_api::unregister_pager_fault_waiter(pager_task_id);
        let _ = ps_api::cancel_block_current_task();
        return linux_errno(LINUX_EAGAIN);
    }

    let committed = ps_api::commit_block_current_task_and_yield();
    let _ = ps_api::unregister_pager_fault_waiter(pager_task_id);
    if committed.is_none() {
        return linux_errno(LINUX_EINVAL);
    }
    if let Some(reservation) = ps_api::take_next_pager_fault_for_rendezvous(pager_task_id, endpoint)
    {
        return write_dispatch(reservation, pager_task_id, dispatch_out);
    }

    // Generic service IPC may have woken pagerd while it was parked here. The
    // caller must return to its normal endpoint loop to process that request.
    linux_errno(LINUX_EAGAIN)
}

/// Wait for one exact fixed pager-fault dispatch. Pagerd registers its task in
/// a bounded atomic table only after its scheduler wait is armed; exception
/// ingress can therefore wake or hand off to it without generic IPC state.
pub(super) fn syscall_linux_rustos_pager_fault_wait(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PAGER_POLICY) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosPagerFaultWaitArgs>(args_ptr) {
        Ok(args) => args,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if args.abi_version != PAGER_FAULT_RENDEZVOUS_ABI_VERSION
        || args.flags != 0
        || args.reserved0 != 0
        || args.dispatch_out == 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(pager_task_id) = ps_api::current_task_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let Some(endpoint) = current_pager_endpoint() else {
        return linux_errno(LINUX_ENODEV);
    };
    wait_for_dispatch(pager_task_id, endpoint, args.dispatch_out)
}

/// Complete one worker-bound pager-fault dispatch. Mapping remains in the
/// kernel's opaque-frame primitive, while VMA and access policy have already
/// been decided by pagerd when it formed this reply.
pub(super) fn syscall_linux_rustos_pager_fault_reply(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PAGER_POLICY) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<RustosPagerFaultReplyArgs>(args_ptr) {
        Ok(args) => args,
        Err(error) => return linux_errno(address_space_error_to_linux_errno(error)),
    };
    if args.abi_version != PAGER_FAULT_RENDEZVOUS_ABI_VERSION
        || args.flags != 0
        || args.reserved0 != 0
    {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(pager_task_id) = ps_api::current_task_id() else {
        return linux_errno(LINUX_EINVAL);
    };
    let reservation = match ps_api::pager_fault_snapshot(args.reply.fault_token) {
        Ok(reservation) => reservation,
        Err(error) => return pager_rendezvous_errno(error),
    };
    match crate::pager::adopt_rendezvous_reply(reservation, args.reply, pager_task_id) {
        Ok(()) => 0,
        Err(error) => pager_rendezvous_errno(error),
    }
}
