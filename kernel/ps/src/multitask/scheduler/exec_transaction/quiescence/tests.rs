//! Owner: actual immutable exec census/sibling terminal pipeline witnesses.
//! Boundary: actual root/thread/bank loans; native entry and consumer ACK are modeled.
//! Lifecycle: physical birth -> intent/census -> exact stop/drain/detach -> cancellation/reclaim.
//! Concurrency: original native fixture namespace; preexisting wake loans force real deferral.
//! Failure: foreign mode or caller stack refuses before sibling effects; retired is not drain.
//! Forbidden: catalog injection, scalar Current or external fixture ACK as hardware/refinement proof.
//! Evidence: all-build quiescence engine plus actual terminal producer, process turn and payloads.
use super::*;
pub(in crate::multitask::scheduler) mod legacy_witnesses;
const ID: u64 = 81800;
fn check(intent: &identity::exec_intent::Intent, census: &ExecMembership) -> Option<bool> {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    preemption.mask_irqs();
    quiesce(&preemption, intent, census, &|_| 1)
}
fn boot() -> UserTaskBootstrap {
    let mut b = root_creation::tests::bootstrap(UserAbi::Linux);
    b.entry = VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x440000);
    b.stack_pointer = VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x780000);
    b.linux_thread_state = Some(LinuxThreadState::default());
    b
}
fn fixture(f: impl FnOnce(usize, usize, ProcessHandle)) {
    root_creation::tests::exec_root(ID, true, boot(), |target, handle| {
        let sibling = thread_creation::tests::active_sibling(target, ID + 1, boot());
        f(target, sibling, handle);
    });
}
fn cancel(intent: identity::exec_intent::Intent, census: ExecMembership) {
    census.finish_for_test();
    intent.cancel_with(
        nucleus_core::util::lockdep::DispatchPreemption::enter_model(0),
        process_table::cancel_exec_owned,
    );
}
#[test]
pub(in crate::multitask::scheduler) fn actual_exec_census_retired_borrower_defers_same_detach_then_retries_to_both_consumer_publications()
 {
    fixture(|target, sibling, handle| {
        let borrower = wake_publication::Binding::capture(sibling, ID + 1).unwrap();
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, true).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        // SAFETY: original fixture entry/serializer and actual root Current bank retained.
        assert_eq!(check(&intent, &census), Some(false));
        assert!(identity::lifecycle(sibling).retired);
        assert_eq!(
            retirement_cleanup::deferred(sibling),
            Some(TaskRetireReason::Exited)
        );
        assert_eq!(
            retirement_cleanup::test_consumer_publication(sibling),
            (false, false)
        );
        assert_eq!(
            stack_resources::context::read(sibling)
                .unwrap()
                .process_handle,
            Some(handle)
        );
        assert!(stack_resources::reuse::begin_retired(sibling, ID + 1).is_none());
        // Repeat on ALREADY Retired with the same original borrower, never treat
        // the lifecycle publication as an executing/MM/consumer ACK.
        // SAFETY: same modeled entry, original physical bank/borrower still retained.
        assert_eq!(check(&intent, &census), Some(false));
        drop(borrower);
        // SAFETY: same entry; only the original borrower has now released its loan.
        assert_eq!(check(&intent, &census), Some(true));
        assert_eq!(retirement_cleanup::deferred(sibling), None);
        assert_eq!(
            retirement_cleanup::test_consumer_publication(sibling),
            (true, true)
        );
        assert_eq!(
            stack_resources::context::read(sibling)
                .unwrap()
                .process_handle,
            None
        );
        assert_eq!(
            retirement_cleanup::test_kernel_detach_payload(sibling),
            Some(Some(handle))
        );
        let cleanup = retirement_cleanup::test_cleanup_payload(sibling).unwrap();
        assert_eq!(cleanup.task_id, ID + 1);
        assert!(!cleanup.process_terminal);
        // Already published original detached member is never elected/stopped twice.
        // SAFETY: unchanged modeled entry and original intent/census.
        assert_eq!(check(&intent, &census), Some(true));
        assert_eq!(
            retirement_cleanup::test_cleanup_payload(sibling).unwrap(),
            cleanup
        );
        cancel(intent, census);
        terminal_publication::tests::release_published(sibling, ID + 1, handle);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_exec_census_cannot_retire_its_live_caller_before_spending_any_sibling_producer() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, false).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let caller = cpu_local::install_test_current_owner(0, sibling);
        // SAFETY: modeled caller is a negative input only; no executing grant is claimed.
        assert_eq!(check(&intent, &census), None);
        assert!(!identity::lifecycle(sibling).retired);
        assert_eq!(retirement_cleanup::reason(sibling), None);
        assert_eq!(
            retirement_cleanup::test_consumer_publication(sibling),
            (false, false)
        );
        drop(caller);
        cancel(intent, census);
        thread_creation::tests::release_born(sibling, ID + 1, handle);
    });
}
#[test]
fn actual_exec_census_foreign_mode_refuses_without_queue_stop_or_terminal_payload() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, false).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let before = runqueue::owner(sibling);
        // SAFETY: original entry/root bank retained; deliberately foreign mode must refuse.
        assert_eq!(check(&intent, &census), None);
        assert_eq!(runqueue::owner(sibling), before);
        assert!(!identity::lifecycle(sibling).retired);
        assert_eq!(retirement_cleanup::reason(sibling), None);
        cancel(intent, census);
        thread_creation::tests::release_born(sibling, ID + 1, handle);
    });
}

#[test]
fn actual_exec_census_unmasked_or_foreign_cpu_scope_refuses_before_original_sibling_effects() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, true).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let owner = runqueue::owner(sibling);
        let unmasked = nucleus_core::util::lockdep::DispatchPreemption::enter_model_with_enabled_irqs(0);
        assert!(!unmasked.is_masked());
        assert_eq!(quiesce(&unmasked, &intent, &census, &|_| 1), None);
        assert_eq!(runqueue::owner(sibling), owner);
        assert!(!identity::lifecycle(sibling).retired);
        assert_eq!(retirement_cleanup::reason(sibling), None);
        drop(unmasked);
        let mut foreign = nucleus_core::util::lockdep::DispatchPreemption::enter_model(1);
        foreign.mask_irqs();
        assert_eq!(quiesce(&foreign, &intent, &census, &|_| 1), None);
        assert_eq!(runqueue::owner(sibling), owner);
        assert!(!identity::lifecycle(sibling).retired);
        assert_eq!(retirement_cleanup::test_consumer_publication(sibling), (false, false));
        drop(foreign);
        cancel(intent, census);
        thread_creation::tests::release_born(sibling, ID + 1, handle);
    });
}

#[test]
fn actual_exec_census_busy_terminal_election_is_not_quiescent_even_with_zero_execution_refs() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, true).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let election =
            crate::multitask::process_placement::terminal::Turn::begin(handle, (sibling, ID + 1))
                .unwrap();
        assert_eq!(identity::execution::count(sibling), 0);
        // SAFETY: original fixture entry/serializer and actual root Current bank retained.
        assert_eq!(check(&intent, &census), Some(false));
        assert!(identity::lifecycle(sibling).retired);
        assert_eq!(identity::execution::count(sibling), 0);
        assert_eq!(
            retirement_cleanup::test_consumer_publication(sibling),
            (false, false)
        );
        assert_eq!(
            stack_resources::context::read(sibling)
                .unwrap()
                .process_handle,
            Some(handle)
        );
        drop(election);
        // SAFETY: unchanged entry; original untouched election rolled back, not a forged ACK.
        assert_eq!(check(&intent, &census), Some(true));
        assert_eq!(
            retirement_cleanup::test_kernel_detach_payload(sibling),
            Some(Some(handle))
        );
        cancel(intent, census);
        terminal_publication::tests::release_published(sibling, ID + 1, handle);
    });
}
