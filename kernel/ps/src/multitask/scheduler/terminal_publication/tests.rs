//! Owner: actual terminal pipeline/election/consumer witnesses, not Scheduler projections.
//! Boundary: retained physical root/thread backing, original process turn and cleanup rows.
//! Lifecycle: birth -> owned withdrawal/effects -> consumers -> explicit native ACK/reclaim.
//! Concurrency: canonical fixture namespace; preexisting real wake loans force deferral.
//! Failure: busy process election or invalid wake retains original terminal authority.
//! Forbidden: native Current/topology or injected external ACK as hardware completion grant.
//! Evidence: shared root/thread/activation/terminal/Linux engines, not joint guard deletion.
use super::*;
const ID: u64 = 81600;
pub(in crate::multitask::scheduler) fn release_published(slot: usize, id: u64, handle: process_table::ProcessHandle) {
    release_with_ack(slot, id, handle, false);
}
pub(in crate::multitask::scheduler) fn release_after_kernel_ack(slot: usize, id: u64, handle: process_table::ProcessHandle) {
    assert!(!retirement_cleanup::kernel_pending(slot));
    release_with_ack(slot, id, handle, true);
}
fn release_with_ack(slot: usize, id: u64, handle: process_table::ProcessHandle, kernel_done: bool) {
    let context = stack_resources::context::read(slot).unwrap();
    // Inject only external consumer ACKs, AFTER the physical Current bank drops.
    // Actual producer/process turn, row seals, resource/MM/funding reclaim run.
    assert_eq!(
        retirement_cleanup::test_consumer_publication(slot),
        (true, !kernel_done)
    );
    let cleanup = retirement_cleanup::claim_next().unwrap();
    assert_eq!(cleanup.task_id, id);
    assert!(retirement_cleanup::complete(cleanup));
    if !kernel_done { retirement_cleanup::ack_kernel_fixture(slot); }
    let mut reset = retirement_cleanup::reuse::begin(slot, Some(id)).unwrap();
    assert!(scheduling_runtime::close_for_reclaim(
        slot,
        context.scheduling_context.identity()
    ));
    let mut reuse = stack_resources::reuse::begin_retired(slot, id).unwrap();
    // Withdraw original birth/activation hints while exact physical reuse
    // custody is retained; never reset a neighbour fixture's global policy.
    dispatch_policy::remove_reclaimed_slot(slot);
    reset.mark_effects();
    reset.reset();
    runqueue::release_retired(slot);
    scheduling_runtime::release(slot, context.scheduling_context.identity());
    stack_resources::context::clear(slot);
    linux_thread_state::clear_reclaimed(slot);
    runqueue::simd_tls::reset_simd_state(slot);
    stack_resources::task_start::clear(slot);
    frame_publication::clear(slot);
    let storage = reuse.take_storage();
    if !kernel_done { process_table::detach_task(handle).unwrap(); }
    drop(storage);
    reset.finish();
    reuse.finish();
}
fn root(current: bool, f: impl FnOnce(usize, process_table::ProcessHandle)) {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.linux_thread_state = Some(LinuxThreadState::default());
    root_creation::tests::exec_root_with_release(ID, current, boot, f, release_published);
}

#[test]
pub(in crate::multitask::scheduler) fn actual_terminal_process_donations_retire_by_original_members_without_catalog_rescan() {
    use super::super::{donation_publication, thread_creation};
    let siblings = core::cell::Cell::new([MAX_TASK; 3]);
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.linux_thread_state = Some(LinuxThreadState::default());
    root_creation::tests::exec_root_with_release(ID, true, boot, |leader, _| {
        // Earlier loader VA input is a native model; actual activation still
        // validates the born frame instead of accepting host-code addresses.
        let sibling_boot = || {
            let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
            boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
            boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
            boot
        };
        let b = thread_creation::tests::active_sibling(leader, ID + 1, sibling_boot());
        let c = thread_creation::tests::active_sibling(leader, ID + 2, sibling_boot());
        let d = thread_creation::tests::active_sibling(leader, ID + 3, sibling_boot());
        siblings.set([b, c, d]);
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        scope.mask_irqs();
        assert!(donation_publication::reserve_call_with(&scope, ID).donation_reserved);
        assert!(donation_publication::bind_with(&scope, 91, ID, ID + 1, false));
        assert!(donation_publication::reserve_call_with(&scope, ID + 2).donation_reserved);
        assert!(donation_publication::bind_with(&scope, 92, ID + 2, ID + 3, false));
        drop(scope);
        let stop = terminal_quarantine::begin(b, Some(ID + 1), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: same explicit native entry; no external consumer ACK here.
        unsafe { guarded(stop, &|_| 1); }
        assert_eq!(donation_ledger::borrowed_context_owner_slot(b), None);
        assert_eq!(donation_ledger::borrowed_context_charge_token(d), Some((c, 92)));
        assert!(!retirement_cleanup::test_cleanup_payload(b).unwrap().process_terminal);
        for (slot, id) in [(d, ID + 3), (c, ID + 2), (leader, ID)] {
            let stop = terminal_quarantine::begin(slot, Some(id), TaskRetireReason::Exited, false).unwrap();
            // SAFETY: original physical member seals and actual terminal census.
            unsafe { guarded(stop, &|_| 1); }
            assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
        }
        assert_eq!(donation_ledger::live_len(), 0);
        assert!(retirement_cleanup::test_cleanup_payload(leader).unwrap().process_terminal);
        // All stopped members retain original descriptors/backing until ACK;
        // no withdrawn-context scan supplies process-wide cancellation authority.
        for &slot in &[b, c, d] {
            assert!(stack_resources::context::read(slot).is_some());
            assert!(!retirement_cleanup::test_cleanup_payload(slot).unwrap().process_terminal);
        }
    }, |leader, id, handle| {
        // Native external ACKs only AFTER the real physical Current bank drops.
        release_published(leader, id, handle);
        for (index, slot) in siblings.get().into_iter().enumerate() {
            release_published(slot, ID + index as u64 + 1, handle);
        }
    });
}

#[test]
fn actual_terminal_frozen_outgoing_defers_same_producer_before_detach_or_consumers() {
    root(true, |slot, handle| {
        crate::multitask::cpu_local::dispatch_turn::with_original_current_model(slot, |turn| {
            assert!(turn.outgoing_image().is_some());
            let owner = terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, true).unwrap();
            // SAFETY: the real original modeled masked Current turn is retained.
            unsafe { guarded(owner, &|_| 1); }
            assert!(retirement_cleanup::deferred(slot).is_some());
            assert_eq!(retirement_cleanup::test_consumer_publication(slot), (false, false));
            assert_eq!(stack_resources::context::read(slot).unwrap().process_handle, Some(handle));
        });
        assert!(!outgoing_image::frozen(slot));
        // Native turn drop, not a hardware RSP ACK. Retry the SAME producer.
        // SAFETY: the outer fixture still retains original modeled own-CPU
        // custody; the first attempt retained this exact terminal producer.
        unsafe { drain_guarded(&|_| 1); }
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
        assert_eq!(stack_resources::context::read(slot).unwrap().process_handle, None);
    });
}
#[test]
fn actual_terminal_effects_publish_both_consumers_last_and_retain_current_backing_until_ack() {
    root(true, |slot, handle| {
        let rsp = runqueue::saved_rsp(slot);
        let before = stack_resources::context::read(slot).unwrap();
        let owner =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (false, false)
        );
        // SAFETY: explicit modeled entry/Current with actual physical bank.
        unsafe {
            guarded(owner, &|_| 1);
        }
        assert!(identity::lifecycle(slot).retired);
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (true, true)
        );
        assert_eq!(retirement_cleanup::deferred(slot), None);
        assert_eq!(
            retirement_cleanup::reason(slot),
            Some(TaskRetireReason::Exited)
        );
        assert_eq!(
            stack_resources::context::read(slot).unwrap().process_handle,
            Some(handle)
        );
        assert_eq!(
            stack_resources::context::read(slot)
                .unwrap()
                .scheduling_context
                .identity(),
            before.scheduling_context.identity()
        );
        assert_eq!(runqueue::saved_rsp(slot), rsp);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        let cleanup = retirement_cleanup::test_cleanup_payload(slot).unwrap();
        assert_eq!(cleanup.task_id, ID);
        assert_eq!(cleanup.process_id, ID);
        assert!(cleanup.process_terminal);
    });
}

#[test]
fn actual_terminal_preexisting_wake_borrower_defers_original_reason_before_signal_and_consumer_effects()
 {
    root(true, |slot, handle| {
        let loan = wake_publication::Binding::capture(slot, ID).unwrap();
        let owner =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, true).unwrap();
        // SAFETY: modeled entry, actual source loan must defer even own Current.
        unsafe {
            guarded(owner, &|_| 1);
        }
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (false, false)
        );
        assert_eq!(
            retirement_cleanup::deferred(slot),
            Some(TaskRetireReason::Exited)
        );
        assert!(
            stack_resources::context::read(slot)
                .unwrap()
                .process_handle
                .is_some()
        );
        assert!(wake_publication::Binding::capture(slot, ID).is_none());
        let claim = terminal_quarantine::claim(slot, Some(ID)).unwrap();
        assert!(claim.detach());
        assert_eq!(claim.reason(), TaskRetireReason::Exited);
        drop(claim);
        drop(loan);
        // SAFETY: same modeled entry/physical Current bank; original borrower drained.
        unsafe {
            drain_guarded(&|_| 1);
        }
        assert_eq!(retirement_cleanup::deferred(slot), None);
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (true, true)
        );
        assert_eq!(
            stack_resources::context::read(slot).unwrap().process_handle,
            None
        );
        assert_eq!(identity::read(slot).unwrap().process_handle, None);
        assert_eq!(
            retirement_cleanup::test_kernel_detach_payload(slot),
            Some(Some(handle))
        );
    });
}

#[test]
fn actual_terminal_sigchld_transfers_exact_sibling_events_to_leader_with_mask_and_consumed_current_frame()
 {
    let bit =
        crate::user::sysops::linux::linux_signal_bit(rustos_user_abi::linux::SIGCHLD).unwrap();
    for mask in [0, bit] {
        root(true, |leader, handle| {
            let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
            boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
            boot.stack_pointer =
                x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
            boot.linux_thread_state = Some(LinuxThreadState::default());
            let sibling = thread_creation::tests::active_sibling(leader, ID + 1, boot);
            linux_thread_state::test_update_exact(sibling, ID + 1, |state| {
                state.pending_sigchld_events = 7;
                state.pending_signals |= bit;
                state.clear_child_tid = 0x111;
                state.robust_list_head = 0x222;
                state.robust_list_len = 24;
            });
            linux_thread_state::test_update_exact(leader, ID, |state| state.signal_mask = mask);
            let saved = runqueue::saved_rsp(leader);
            runqueue::set_saved_rsp(leader, 1);
            runqueue::wait::set_armed(leader, true);
            runqueue::wait::set_blocked(leader, true);
            let owner =
                terminal_quarantine::begin(sibling, Some(ID + 1), TaskRetireReason::Exited, false)
                    .unwrap();
            // SAFETY: modeled native entry; target original Current bank retained.
            unsafe {
                guarded(owner, &|_| 1);
            }
            let source = linux_thread_state::test_exec_state(sibling, ID + 1).unwrap();
            assert_eq!(source.pending_sigchld_events, 0);
            assert_eq!(source.pending_signals & bit, 0);
            let target = linux_thread_state::test_exec_state(leader, ID).unwrap();
            assert_eq!(target.pending_sigchld_events, 7);
            assert_eq!(target.pending_signals & bit, bit);
            assert_eq!(target.signal_mask, mask);
            assert!(identity::signal_pending(leader));
            assert_eq!(runqueue::saved_rsp(leader), 1);
            assert_eq!(runqueue::wait::armed(leader), mask != 0);
            assert_eq!(runqueue::wait::blocked(leader), mask != 0);
            let cleanup = retirement_cleanup::test_cleanup_payload(sibling).unwrap();
            assert!(!cleanup.process_terminal);
            assert_eq!(cleanup.clear_child_tid, 0x111);
            assert_eq!(cleanup.robust_list_head, 0x222);
            assert_eq!(cleanup.robust_list_len, 24);
            runqueue::set_saved_rsp(leader, saved);
            release_published(sibling, ID + 1, handle);
            let owner =
                terminal_quarantine::begin(leader, Some(ID), TaskRetireReason::Exited, false)
                    .unwrap();
            // SAFETY: same modeled entry and retained original leader Current bank.
            unsafe {
                guarded(owner, &|_| 1);
            }
            assert!(
                retirement_cleanup::test_cleanup_payload(leader)
                    .unwrap()
                    .process_terminal
            );
        });
    }
}

#[test]
fn actual_invalid_wake_transfers_same_producer_after_reader_release_and_publishes_terminal_cleanup()
{
    root(false, |slot, _| {
        // This root deliberately uses the byte-only native fixture addresses,
        // which the unchanged actual user-return validator must reject.
        let rsp = runqueue::saved_rsp(slot);
        // SAFETY: original modeled entry and actual nonexecuting frame loans.
        assert!(unsafe { wake_publication::guarded(slot, ID, &|_| 1) });
        assert_eq!(stack_resources::frame_readers::test_count(slot), 0);
        assert_eq!(identity::execution::count(slot), 0);
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (true, true)
        );
        assert!(matches!(retirement_cleanup::reason(slot),
            Some(TaskRetireReason::CorruptedContext { saved_rsp, reason: "user return frame points outside user space", .. }) if saved_rsp == rsp));
    });
}

#[test]
fn actual_terminal_busy_process_election_defers_without_spending_original_detach_then_retries() {
    root(false, |slot, handle| {
        let turn =
            crate::multitask::process_placement::terminal::Turn::begin(handle, (slot, ID)).unwrap();
        let owner =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, true).unwrap();
        // SAFETY: modeled native entry; actual competing election is retained.
        unsafe {
            guarded(owner, &|_| 1);
        }
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (false, false)
        );
        assert_eq!(
            retirement_cleanup::deferred(slot),
            Some(TaskRetireReason::Exited)
        );
        assert_eq!(
            stack_resources::context::read(slot).unwrap().process_handle,
            Some(handle)
        );
        drop(turn); // untouched original process turn rolls back, no fake ACK
        // SAFETY: same modeled entry; actual competing process turn has drained.
        unsafe {
            drain_guarded(&|_| 1);
        }
        assert_eq!(
            retirement_cleanup::test_consumer_publication(slot),
            (true, true)
        );
        assert_eq!(
            stack_resources::context::read(slot).unwrap().process_handle,
            None
        );
    });
}
