//! Owner: actual wake binding/frame/queue witnesses with original physical roots.
//! Boundary: UID/backing admission and retained Current bank, not native catalog arrays.
//! Lifecycle: capture -> token or reader -> queue -> original loan release.
//! Concurrency: canonical native namespace; original entry/Current/topology are modeled.
//! Failure: foreign UID or terminal seal refuses before wait/queue effects.
//! Forbidden: consumed frame reads or native Current as hardware execution grant.
//! Evidence: actual shared wake/resources/terminal rows, not joint dispatch return proof.
use super::*;
const ID: u64 = 81500;
fn check(id: u64) -> bool {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    preemption.mask_irqs();
    wake(&preemption, id, &|_| 1)
}
fn root(current: bool, f: impl FnOnce(usize)) {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    // Native root fixture's byte-only addresses are not user return addresses.
    // Exercise the unchanged production address-range validator with valid ABI.
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    root_creation::tests::exec_root(ID, current, boot, |slot, _| f(slot));
}

#[test]
fn actual_reply_wake_retains_original_binding_through_fifo_and_rejects_replaced_generation() {
    root(false, |slot| {
        sync_handoff::remove_slot_all_cpus(slot);
        let fifo_len = sync_handoff::model_len(0);
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        scope.mask_irqs();
        let refs = identity::execution::count(slot);
        let binding = Binding::resolve(ID).unwrap();
        // SAFETY: explicit modeled original scope survives through FIFO publication.
        let binding = unsafe { retained_wake(binding, &|_| 1) }.ok().unwrap();
        assert_eq!(identity::execution::count(slot), refs + 1);
        let token =
            ReplyWakeHandoff::from_owner(binding.slot(), binding.id(), runqueue::owner(slot))
                .unwrap();
        assert!(sync_handoff::enqueue_reply_wake(token));
        assert!(sync_handoff::pending(0));
        assert_eq!(sync_handoff::model_len(0), fifo_len + 1);
        let stop =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert!(Binding::capture(slot, ID).is_none());
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        assert_eq!(identity::execution::count(slot), refs + 1);
        drop(stop);
        // Actual queue custody changes generation; urgency token may not revive it.
        runqueue::test_blocked_return(slot, 0, runqueue::weight::value(slot));
        assert!(!sync_handoff::enqueue_reply_wake(token));
        assert_eq!(binding.id(), ID);
        drop(binding);
        assert_eq!(identity::execution::count(slot), refs);
        sync_handoff::remove_slot_all_cpus(slot);
    });
}

#[test]
fn actual_reply_wake_scope_and_foreign_identity_refuse_without_wait_or_fifo_effects() {
    root(false, |slot| {
        // Model the earlier exact wait, retaining all original UID/loan checks.
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_ENDPOINT_REPLY, 601);
        sync_handoff::remove_slot_all_cpus(slot);
        let fifo_len = sync_handoff::model_len(0);
        let owner = runqueue::owner(slot);
        let refs = identity::execution::count(slot);
        let unmasked =
            nucleus_core::util::lockdep::DispatchPreemption::enter_model_with_enabled_irqs(0);
        assert!(!reply_with(&unmasked, 601, ID, &|_| 1));
        assert_eq!(runqueue::owner(slot), owner);
        drop(unmasked);
        let foreign = nucleus_core::util::lockdep::DispatchPreemption::enter_model(1);
        assert!(!reply_with(&foreign, 601, ID, &|_| 1));
        assert_eq!(runqueue::owner(slot), owner);
        drop(foreign);
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        scope.mask_irqs();
        task_directory::record(ID + 1, slot);
        assert!(!reply_with(&scope, 601, ID + 1, &|_| 1));
        task_directory::forget(ID + 1);
        let stop =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert!(!reply_with(&scope, 601, ID, &|_| 1));
        assert_eq!(runqueue::owner(slot), owner);
        assert_eq!(identity::execution::count(slot), refs);
        assert_eq!(sync_handoff::model_len(0), fifo_len);
        drop(stop);
        assert!(reply_with(&scope, 601, ID, &|_| 1));
        assert_eq!(identity::execution::count(slot), refs);
        assert!(sync_handoff::pending(0));
        assert_eq!(sync_handoff::model_len(0), fifo_len + 1);
        sync_handoff::remove_slot_all_cpus(slot);
    });
}

#[test]
fn actual_wake_current_ignores_consumed_frame_and_retains_original_bank_and_uid() {
    root(true, |slot| {
        let saved = runqueue::saved_rsp(slot);
        // A consumed saved frame is ordinary stack bytes, never wake authority.
        runqueue::set_saved_rsp(slot, 1);
        runqueue::wait::set_blocked(slot, true);
        runqueue::wait::set_armed(slot, true);
        runqueue::wait::set_reason(slot, 1, 0);
        runqueue::wait::set_blocked_since_ticks(slot, 77);
        runqueue::wait::set_ready_since_ticks(slot, 88);
        let refs = identity::execution::count(slot);
        // SAFETY: explicit modeled native entry; actual Current bank retained.
        assert!(check(ID));
        assert_eq!(runqueue::saved_rsp(slot), 1);
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert!(runqueue::published_runnable_count(0) > 0);
        assert_eq!(identity::execution::count(slot), refs);
        assert_eq!(runqueue::wait::ready_since_ticks(slot), 0);
        assert_eq!(runqueue::wait::blocked_since_ticks(slot), 0);
        assert_eq!(runqueue::wait::reason(slot), (0, 0));
        assert!(!runqueue::wait::blocked(slot));
        assert!(!runqueue::wait::armed(slot));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(!identity::lifecycle(slot).retired);
        runqueue::set_saved_rsp(slot, saved);
    });
}

#[test]
fn actual_wake_blocked_return_publishes_queue_while_original_current_bank_retains_consumed_frame() {
    root(true, |slot| {
        let saved = runqueue::saved_rsp(slot);
        // Model the earlier queue claim, then actual outgoing Blocked return.
        // Physical bank is original; MM/register return is not inferred.
        runqueue::test_blocked_return(slot, 0, runqueue::weight::value(slot));
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Blocked
        );
        runqueue::set_saved_rsp(slot, 1);
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_GENERIC, 0);
        let refs = identity::execution::count(slot);
        // SAFETY: modeled native entry/IRQ with original retained Current bank.
        assert!(check(ID));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert!(runqueue::published_runnable_count(0) > 0);
        assert_eq!(runqueue::saved_rsp(slot), 1);
        assert_eq!(identity::execution::count(slot), refs);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(!runqueue::wait::blocked(slot));
        assert!(!runqueue::wait::armed(slot));
        assert_eq!(runqueue::wait::reason(slot).0, runqueue::wait::REASON_NONE);
        runqueue::set_saved_rsp(slot, saved);
    });
}

#[test]
fn actual_wake_foreign_uid_and_original_terminal_reservation_refuse_before_effects() {
    root(true, |slot| {
        let owner = runqueue::owner(slot);
        let refs = identity::execution::count(slot);
        // SAFETY: explicit modeled native entry, no hardware permission claim.
        assert!(!check(ID + 1));
        assert_eq!(identity::execution::count(slot), refs);
        assert_eq!(runqueue::owner(slot), owner);
        let stop =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert!(Binding::capture(slot, ID).is_none());
        // SAFETY: same modeled native entry; exact terminal reservation retained.
        assert!(!check(ID));
        assert_eq!(runqueue::owner(slot), owner);
        assert_eq!(identity::execution::count(slot), refs);
        drop(stop); // untouched exact reservation rollback
        assert!(Binding::capture(slot, ID).is_some());
    });
}

#[test]
fn actual_wake_unmasked_or_foreign_cpu_scope_refuses_before_binding_and_wait_effects() {
    root(true, |slot| {
        runqueue::wait::set_blocked(slot, true);
        runqueue::wait::set_armed(slot, true);
        let owner = runqueue::owner(slot);
        let refs = identity::execution::count(slot);
        let unmasked =
            nucleus_core::util::lockdep::DispatchPreemption::enter_model_with_enabled_irqs(0);
        assert!(!unmasked.is_masked());
        assert!(!wake(&unmasked, ID, &|_| 1));
        assert!(runqueue::wait::blocked(slot));
        assert!(runqueue::wait::armed(slot));
        assert_eq!(runqueue::owner(slot), owner);
        assert_eq!(identity::execution::count(slot), refs);
        drop(unmasked);
        let mut foreign = nucleus_core::util::lockdep::DispatchPreemption::enter_model(1);
        foreign.mask_irqs();
        assert!(!wake(&foreign, ID, &|_| 1));
        assert!(runqueue::wait::blocked(slot));
        assert!(runqueue::wait::armed(slot));
        assert_eq!(runqueue::owner(slot), owner);
        assert_eq!(identity::execution::count(slot), refs);
        drop(foreign);
    });
}

#[test]
fn actual_wake_stale_directory_hint_is_only_candidate_and_never_uid_authority() {
    root(false, |slot| {
        let refs = identity::execution::count(slot);
        task_directory::record(ID, ROOT_TASK_SLOT);
        assert!(check(ID));
        assert!(runqueue::owner(slot).runnable);
        assert_eq!(identity::execution::count(slot), refs);
        task_directory::record(ID + 1, slot);
        assert!(!check(ID + 1));
        assert_eq!(identity::execution::count(slot), refs);
        task_directory::forget(ID + 1);
        task_directory::forget(ID);
    });
}

#[test]
fn actual_wake_binding_pins_before_stop_and_excludes_reuse_until_original_drop() {
    root(true, |slot| {
        let refs = identity::execution::count(slot);
        let loan = Binding::capture(slot, ID).unwrap();
        assert_eq!(loan.pin.slot(), slot);
        assert_eq!(loan.pin.id(), ID);
        assert_eq!(identity::execution::count(slot), refs + 1);
        let stop =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert!(Binding::capture(slot, ID).is_none());
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        assert_eq!(
            stack_resources::task_start::read(slot).map(|s| s.id),
            Some(ID)
        );
        drop(loan);
        assert_eq!(identity::execution::count(slot), refs);
        drop(stop);
    });
}

#[test]
fn actual_generic_deadline_wake_admits_original_kernel_wait_without_user_reclassification() {
    root(true, |_slot| {
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        scope.mask_irqs();
        let kernel = 121;
        let kernel_id = stack_resources::task_start::read(kernel).unwrap().id;
        runqueue::wait::set_blocked(kernel, true);
        runqueue::wait::set_reason(kernel, runqueue::wait::REASON_GENERIC, 0);
        let before = runqueue::owner(kernel);
        let queued = super::super::wake_retry::pending_on(&scope);
        assert!(!wake_user_in(&scope, kernel_id, &|_| 1));
        assert_eq!(runqueue::owner(kernel), before);
        // A user-qualified refusal of a kernel wait records no retry: the
        // retry path must never widen user-wake authority to kernel waits.
        assert_eq!(super::super::wake_retry::pending_on(&scope), queued);
        // SAME born kernel allocation/identity/frame, not a manufactured user
        // identity or permission to skip frame/queue/physical-bank validation.
        assert!(wake(&scope, kernel_id, &|_| 1));
        assert!(!runqueue::wait::blocked(kernel));
        assert_eq!(stack_resources::task_start::read(kernel).unwrap().id, kernel_id);
        assert!(!stack_resources::context::read(kernel).unwrap().user_mode);
    });
}

#[test]
fn actual_user_qualified_wake_preserves_kernel_wait_and_wakes_exact_original_user() {
    root(true, |slot| {
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        scope.mask_irqs();
        let kernel = 121;
        let kernel_id = stack_resources::task_start::read(kernel).unwrap().id;
        runqueue::wait::set_blocked(kernel, true);
        let kernel_owner = runqueue::owner(kernel);
        assert!(!wake_user_in(&scope, kernel_id, &|_| 1));
        assert!(runqueue::wait::blocked(kernel));
        assert_eq!(runqueue::owner(kernel), kernel_owner);
        runqueue::wait::clear(kernel);
        assert!(!wake_user_in(&scope, ID + 1, &|_| 1));
        // Native earlier Blocked return, with SAME original physical bank. A
        // consumed frame is not re-read or converted into a second grant.
        runqueue::test_blocked_return(slot, 0, runqueue::weight::value(slot));
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_GENERIC, 0);
        let rsp = runqueue::saved_rsp(slot);
        runqueue::set_saved_rsp(slot, 1);
        let refs = identity::execution::count(slot);
        task_directory::forget(ID);
        assert!(wake_user_in(&scope, ID, &|_| 1));
        assert!(!runqueue::wait::blocked(slot));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert_eq!(runqueue::saved_rsp(slot), 1);
        assert_eq!(identity::execution::count(slot), refs);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        runqueue::set_saved_rsp(slot, rsp);
    });
}

#[test]
fn actual_wake_nonexecuting_validates_real_frame_and_publishes_queue_without_catalog() {
    root(false, |slot| {
        let refs = identity::execution::count(slot);
        let rsp = runqueue::saved_rsp(slot);
        // SAFETY: explicit modeled native entry, actual physical reader protocol.
        assert!(check(ID));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert!(runqueue::owner(slot).runnable);
        assert_eq!(identity::execution::count(slot), refs);
        assert_eq!(runqueue::saved_rsp(slot), rsp);
        assert!(!identity::lifecycle(slot).retired);
    });
}
