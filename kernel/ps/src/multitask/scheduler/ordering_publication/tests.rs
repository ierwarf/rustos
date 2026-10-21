//! Boundary: actual born Current/recipient and production ordering owners.
//! Lifecycle: original banks/UID/interval span effects; terminal consumers ACK last.
//! Concurrency: native CPU/IRQ, manifest class and earlier wait inputs are modeled.
//! Failure: wrong UID, blocked target, absent Current and idle donors refuse rights.
//! Forbidden: no scalar hint grants execution, funding, wake or backing lifetime.
//! Evidence: actual FIFO/floor/consumer witnesses, not full joint or hardware proof.
use super::*;
use crate::multitask::current_identity as identity;
const PARENT: u64 = super::super::current_terminal::tests::ID;
const CHILD: u64 = PARENT + 1;
fn family(f: impl FnOnce(usize, usize, &DispatchPreemption<'_>)) {
    super::super::current_terminal::tests::family(true, |root, _| {
        let child = super::super::current_terminal::tests::sibling(root);
        dispatch_policy::remove_reclaimed_slot(root);
        dispatch_policy::remove_reclaimed_slot(child);
        super::super::current_terminal::tests::with_current(root, |_, scope| {
            f(root, child, scope);
        });
        let producer = terminal_quarantine::begin(child, Some(CHILD), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: isolated born fixture; parent Current scope has drained.
        unsafe { terminal_publication::guarded(producer, &runqueue_policy::online_affinity_mask); }
        Some(child)
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_original_ordering_preserves_spawn_fifo_and_current_floor_without_funding_transfer() {
    family(|root, child, scope| {
        let kernel_weight = runqueue::weight::value(121);
        runqueue::weight::replace(121, SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD);
        runqueue::raise_vruntime_floor(root, 4 * IPC_DONATION_BONUS_NS);
        runqueue::raise_vruntime_floor(child, 8 * IPC_DONATION_BONUS_NS);
        let stamp = current_identity::read_dispatch(child).unwrap().pager_charge;
        assert!(target_in(scope, CHILD));
        let hint = dispatch_policy::lock(0).next_pick_hint.unwrap();
        assert_eq!((hint.slot, hint.task_id), (child, CHILD));
        assert!(runqueue::vruntime(child) <= runqueue::vruntime(root).saturating_sub(IPC_DONATION_BONUS_NS));
        assert_eq!(identity::read_dispatch(child).unwrap().pager_charge, stamp);
        runqueue::lower_vruntime_ceiling(child, 1_000);
        assert!(target_in(scope, CHILD));
        assert!(runqueue::vruntime(child) <= 1_000, "latency floor never raises vruntime");
        // Distinguish caller and strict-band floors: a max() mutation must fail,
        // not be masked by the outer min against the receiver's current value.
        let old_weight = runqueue::weight::value(root);
        runqueue::weight::replace(root, SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD);
        runqueue::raise_vruntime_floor(root, 20_000_000);
        runqueue::raise_vruntime_floor(child, 100_000_000);
        assert!(target_in(scope, CHILD));
        assert_eq!(runqueue::vruntime(child), 18_000_000);
        let self_before = runqueue::vruntime(root);
        assert!(target_in(scope, PARENT));
        assert_eq!(runqueue::vruntime(root), self_before);
        runqueue::set_idle_slot(child, true);
        let idle_before = runqueue::vruntime(child);
        assert!(!target_in(scope, CHILD));
        assert_eq!(runqueue::vruntime(child), idle_before);
        runqueue::set_idle_slot(child, false);
        // Native earlier idle-classification input on the SAME physical Current.
        // Idle callers may order work, never donate a caller latency floor.
        runqueue::set_idle_slot(root, true);
        runqueue::raise_vruntime_floor(child, 100_000_000);
        assert!(target_in(scope, CHILD));
        assert_eq!(runqueue::vruntime(child), 100_000_000);
        runqueue::set_idle_slot(root, false);
        runqueue::weight::replace(root, old_weight);
        assert!(spawn_in(scope, CHILD));
        runqueue::raise_vruntime_floor(child, 100_000_000);
        assert!(!spawn_in(scope, CHILD));
        assert_eq!(runqueue::vruntime(child), 100_000_000, "deduplicated spawn cannot donate twice");
        assert!(spawn_in(scope, PARENT));
        let mut policy = dispatch_policy::lock(0);
        assert_eq!(policy.spawn_pick_hints.pop().unwrap().task_id, CHILD);
        assert_eq!(policy.spawn_pick_hints.pop().unwrap().task_id, PARENT);
        assert!(policy.spawn_pick_hints.is_empty());
        drop(policy);
        assert!(stack_resources::cpu_leases::owns_current(0, root));
        assert!(identity::execution::banks::owns_current(0, root));
        assert!(current_terminal::OriginalCurrent::borrow(scope).is_some());
        runqueue::weight::replace(121, kernel_weight);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_latency_and_process_ordering_use_exact_live_original_targets_without_waking() {
    family(|root, child, scope| {
        task_directory::forget(CHILD);
        assert!(latency_in(scope, CHILD));
        assert!(latency_in(scope, PARENT));
        assert!(latency_in(scope, CHILD));
        assert_eq!(dispatch_policy::lock(0).latency_pick_hint_len, 2);
        let kernel_id = stack_resources::task_start::read(121).unwrap().id;
        let kernel_weight = runqueue::weight::value(121);
        // The root fixture's kernel is unclassified User by default. Supply
        // an explicit native manifest-class input to the actual class owner.
        runqueue::weight::replace(121, SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD);
        assert!(!latency_in(scope, kernel_id), "System work cannot spend User exception capacity");
        runqueue::weight::replace(121, kernel_weight);
        dispatch_policy::store_dispatch_streaks(0, 0, MAX_CONSECUTIVE_LATENCY_HANDOFFS);
        assert_eq!(dispatcher::test_take_latency(0), None);
        assert_eq!(dispatch_policy::lock(0).latency_pick_hint_len, 2);
        dispatch_policy::store_dispatch_streaks(0, 0, 0);
        assert_eq!(dispatcher::test_take_latency(0), Some(child));
        assert_eq!(dispatch_policy::lock(0).latency_pick_hint_len, 1,
            "a successful consumed head spends exactly one queue entry");
        assert_eq!(dispatch_policy::lock(0).latency_pick_hint_head, 1,
            "a consumed entry advances the actual FIFO cursor");
        assert_eq!(dispatcher::test_take_latency(0), None); // running parent is not a Local grant
        assert_eq!(dispatch_policy::lock(0).latency_pick_hint_len, 0);
        // Earlier FIFO input is only a hint: stale UID must be discarded by
        // the actual generic consumer before class/Local candidate selection.
        let child_weight = runqueue::weight::value(child);
        // Explicit native manifest-class input isolates UID qualification from
        // fixture-only unrelated kernel publication gaps in the System scan.
        runqueue::weight::replace(child, SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD);
        dispatch_policy::lock(0).next_pick_hint = Some(dispatch_policy::LatencyPickHint {
            slot: child, task_id: CHILD,
        });
        assert!(!dispatcher::test_hint_is_empty(0, root), "valid exact hint must reach the candidate boundary before stale UID test");
        dispatch_policy::lock(0).next_pick_hint = Some(dispatch_policy::LatencyPickHint {
            slot: child, task_id: CHILD + 1,
        });
        assert!(dispatcher::test_hint_is_empty(0, root));
        runqueue::weight::replace(child, child_weight);
        let target = Target::capture(CHILD).unwrap();
        assert_eq!(target.binding.slot(), child);
        assert_eq!(identity::execution::count(child), 1);
        assert!(stack_resources::reuse::begin_retired(child, CHILD).is_none());
        drop(target);
        // Process worker comparison keeps its original winning binding, not
        // a scalar min-slot lookup. Pick the lower-vruntime child.
        runqueue::raise_vruntime_floor(root, 8 * IPC_DONATION_BONUS_NS);
        let selected = process_in(scope, PARENT).unwrap();
        assert_eq!(selected, CHILD);
        assert!(sync_handoff::pending(0));
        let record = sync_handoff::take_next_ready(0, false, |_| true).unwrap();
        assert_eq!((record.slot(), record.task_id()), (child, CHILD));
        // Native earlier wait-publication input. No Running/Current token is
        // manufactured and this ordering producer is forbidden to clear it.
        runqueue::wait::set_blocked(child, true);
        let owner = runqueue::owner(child);
        assert!(Target::capture(CHILD).is_none(), "original admission refuses blocked ordering before publication");
        assert!(!target_in(scope, CHILD));
        assert!(!latency_in(scope, CHILD));
        assert!(!spawn_in(scope, CHILD));
        assert_eq!(runqueue::owner(child), owner);
        assert!(runqueue::wait::blocked(child));
        runqueue::wait::clear(child);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_fifo_qualification_releases_ordering_leaf_before_original_binding_and_replenishment() {
    family(|root, child, scope| {
        runqueue::raise_vruntime_floor(root, 8 * IPC_DONATION_BONUS_NS);
        assert_eq!(process_in(scope, PARENT), Some(CHILD));
        let mut original = None;
        let taken = sync_handoff::take_next_ready(0, false, |record| {
            assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0,
                "FIFO cannot retain a raw leaf across original identity/frame qualification");
            let target = Target::capture(CHILD).unwrap();
            assert_eq!(target.binding.slot(), child);
            assert_eq!((record.slot(), record.task_id()), (child, CHILD));
            assert!(!sync_handoff::pending(0), "empty was observed under the leaf");
            assert!(sync_handoff::enqueue(0, record));
            original = Some(record);
            true
        }).unwrap();
        assert_eq!(taken, original.unwrap(), "the consumed exact record survives qualification");
        assert!(sync_handoff::pending(0), "a callback's later producer cannot be cleared by an earlier consumer");
        let mut callbacks = 0;
        assert!(sync_handoff::take_next_ready(0, false, |record| {
            assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0);
            callbacks += 1;
            assert!(sync_handoff::enqueue(0, record));
            false
        }).is_none());
        assert_eq!(callbacks, MAX_TASK, "continuous producers cannot make one dispatch drain unbounded");
        assert!(sync_handoff::pending(0));
        assert_eq!(sync_handoff::take_next_ready(0, false, |_| true), Some(taken));
        assert!(!sync_handoff::pending(0));
    });
}
#[test]
fn actual_handoff_checks_competitors_only_at_the_budget_limit() {
    family(|_, child, _| {
        assert!(sync_handoff::take_next_ready(0, false, |_| false).is_none());
        sync_handoff::record_dispatch(0, false, 0);
        let record = sync_handoff::SyncHandoffRecord::new(child, CHILD);
        let calls = core::cell::Cell::new(0);
        let competitor = || {
            calls.set(calls.get() + 1);
            assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0);
            true
        };
        assert!(sync_handoff::enqueue(0, record));
        assert_eq!(sync_handoff::take_next_ready_with(0, competitor, |_| true), Some(record));
        assert_eq!(calls.get(), 0, "an unused competitor scan must stay lazy");
        sync_handoff::record_dispatch(0, true, 1);
        sync_handoff::record_dispatch(0, true, 1 + u64::from(MAX_SYNC_HANDOFF_CHAIN_TICKS));
        assert!(sync_handoff::enqueue(0, record));
        assert_eq!(sync_handoff::take_next_ready_with(0, competitor, |_| true), None);
        assert_eq!(calls.get(), 1);
        assert!(sync_handoff::pending(0), "fairness refusal cannot consume the FIFO");
        assert_eq!(sync_handoff::take_next_ready_with(0, || false, |_| true), Some(record),
            "without a fair competitor, the exhausted chain can still progress");
        sync_handoff::record_dispatch(0, false, 0);
    });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_missing_current_cannot_supply_latency_floor_and_foreign_uid_cannot_publish() {
    family(|_, child, scope| {
        runqueue::raise_vruntime_floor(child, 8 * IPC_DONATION_BONUS_NS);
        let before = runqueue::vruntime(child);
        let missing = identity::execution::banks::isolate(0);
        // Hint is just ordering; absence of donor custody must NEVER cause a
        // scalar Current fallback or fabricate a latency/funding donation.
        assert!(target_in(scope, CHILD));
        assert_eq!(runqueue::vruntime(child), before);
        assert!(!target_in(scope, CHILD + 100));
        assert!(dispatch_policy::lock(0).next_pick_hint.is_none());
        assert!(!latency_in(scope, CHILD + 100));
        drop(missing);
    });
}
