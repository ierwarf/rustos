use super::*;
use crate::multitask::current_identity::{self as identity, LifecycleGate, TaskIdentity};
const SLOT: usize = 126;
const CHILD: usize = 127;
const ID: u64 = 126009;
fn fixture(f: impl FnOnce(ProcessHandle)) {
    let _serial = crate::multitask::scheduler::execution_test_serial_guard();
    reset_for_tests();
    identity::clear(SLOT);
    identity::clear(CHILD);
    let h = ProcessHandle::new(31, 42);
    // This catalog projection has no production spawn to grow the side cell.
    assert!(ensure_slot_storage(h.index()));
    identity::publish_metadata(
        SLOT,
        Some(TaskIdentity {
            task_id: Some(ID),
            user_mode: true,
            abi: Some(UserAbi::Windows),
            process_handle: Some(h),
            process_id: Some(7000),
            console_session: crate::io::session::ConsoleSessionHandle::from_raw(0),
            pager_charge: None,
        }),
    );
    register_root(h, SLOT, ID, 3).unwrap();
    f(h);
    unregister(h, SLOT, ID);
    unregister(h, CHILD, ID + 1);
    identity::clear(SLOT);
    identity::clear(CHILD);
    reset_for_tests();
}
#[test]
fn creation_grant_freezes_process_mask_and_registers_child_before_writer_can_capture_membership() {
    fixture(|h| {
        // Real constructors retain the original CPU unit through process joins.
        // The ProcessPlacement leaf masks IRQs, not a nested dispatch owner.
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        scope.mask_irqs();
        let cpu = scope.cpu();
        assert_eq!(nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(cpu), 1);
        let join = Join::begin(h, (SLOT, ID), UserAbi::Windows).unwrap();
        assert_eq!(identity::execution::count(SLOT), 1);
        assert_eq!(join.mask(), 3);
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        join.register(CHILD, ID + 1).unwrap();
        assert!(join.register(CHILD, ID + 2).is_none());
        drop(join);
        let change = Change::begin(h, (SLOT, ID)).unwrap();
        assert_eq!(change.members(), &[(SLOT, ID), (CHILD, ID + 1)]);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_none());
        assert_eq!(identity::execution::count(SLOT), 0);
        drop(change);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_some());
        assert_eq!(nucleus_core::util::lockdep::DispatchPreemption::model_depth_on(cpu), 1);
        drop(scope);
    });
}
#[test]
fn committed_query_stays_old_during_partial_masks_and_joins_open_only_after_published_unseal() {
    fixture(|h| {
        let mut change = Change::begin(h, (SLOT, ID)).unwrap();
        let mut cohort = identity::cohort::Cohort::begin(change.members()).unwrap();
        assert_eq!(mask(h), Some(3));
        assert!(
            identity::execution::with_claim(SLOT, |_| panic!(
                "claim escaped unpublished process mask"
            ))
            .is_none()
        );
        change.begin_effects();
        cohort.begin_effects();
        assert_eq!(mask(h), Some(3));
        change.publish(4, &mut cohort);
        assert_eq!(mask(h), Some(4));
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_none());
        assert!(identity::execution::with_claim(SLOT, |_| ()).is_some());
        cohort.finish_published();
        change.finish();
        let join = Join::begin(h, (SLOT, ID), UserAbi::Windows).unwrap();
        assert_eq!(join.mask(), 4);
    });
}

#[test]
fn job_control_and_affinity_credentials_cannot_spend_each_others_original_process_turn() {
    fixture(|h| {
        let mut change = Change::begin(h, (SLOT, ID)).unwrap();
        change.begin_effects();
        let mut job = identity::cohort::Cohort::begin_job_control(change.members()).unwrap();
        job.begin_effects();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            change.publish(4, &mut job))).is_err());
        assert_eq!(mask(h), Some(3));
        job.complete();
        let mut affinity = identity::cohort::Cohort::begin(change.members()).unwrap();
        affinity.begin_effects();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            change.publish_job_control(&mut affinity))).is_err());
        assert_eq!(mask(h), Some(3));
        affinity.complete();
        let mut job = identity::cohort::Cohort::begin_job_control(change.members()).unwrap();
        job.begin_effects();
        change.publish_job_control(&mut job);
        job.finish_published();
        change.finish();
        assert_eq!(mask(h), Some(3));
    });
}

#[test]
fn activation_credential_cannot_publish_process_mask_or_spend_pending_affinity_transaction() {
    fixture(|h| {
        let mut change = Change::begin(h, (SLOT, ID)).unwrap();
        change.begin_effects();
        identity::update_live_lifecycle(SLOT, ID, identity::LifecycleGate::Suspended, true);
        let mut wrong = identity::cohort::Cohort::begin_activation(change.members()).unwrap();
        wrong.begin_effects();
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            change.publish(4, &mut wrong)
        }));
        assert!(refused.is_err());
        assert_eq!(mask(h), Some(3));
        wrong.complete(); // isolated credential fixture, no activation effects
        let mut cohort = identity::cohort::Cohort::begin(change.members()).unwrap();
        cohort.begin_effects();
        change.publish(4, &mut cohort);
        cohort.finish_published();
        change.finish();
        assert_eq!(mask(h), Some(4));
    });
}

#[test]
fn process_membership_and_mask_survive_same_uid_exec_abi_change_without_old_abi_creation_permission()
 {
    fixture(|h| {
        let mut meta = identity::read(SLOT).unwrap();
        meta.abi = Some(UserAbi::Linux);
        identity::publish_metadata(SLOT, Some(meta));
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_none());
        let join = Join::begin(h, (SLOT, ID), UserAbi::Linux).unwrap();
        assert_eq!(join.mask(), 3);
        drop(join);
        meta.abi = Some(UserAbi::Windows);
        identity::publish_metadata(SLOT, Some(meta));
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Linux).is_none());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_some());
        assert_eq!(mask(h), Some(3));
    });
}
#[test]
fn process_and_creator_generations_abi_terminal_and_duplicate_members_refuse_without_spending_join()
{
    fixture(|h| {
        let stale = ProcessHandle::new(h.index(), h.generation() + 1);
        assert!(mask(stale).is_none());
        assert!(Change::begin(stale, (SLOT, ID)).is_none());
        assert!(Join::begin(stale, (SLOT, ID), UserAbi::Windows).is_none());
        assert!(Join::begin(h, (SLOT, ID + 1), UserAbi::Windows).is_none());
        let mut meta = identity::read(SLOT).unwrap();
        meta.abi = Some(UserAbi::Linux);
        identity::publish_metadata(SLOT, Some(meta));
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_none());
        meta.abi = Some(UserAbi::Windows);
        identity::publish_metadata(SLOT, Some(meta));
        identity::update_live_lifecycle(SLOT, ID, LifecycleGate::Retired, true);
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_none());
        assert_eq!(identity::execution::count(SLOT), 0);
        identity::update_live_lifecycle(SLOT, ID, LifecycleGate::Retired, false);
        assert!(register_root(h, SLOT, ID, 3).is_none());
    });
}
#[test]
fn exhausted_transaction_and_partial_failure_never_reopen_or_alias_next_process_generation() {
    fixture(|h| {
        cell(h).unwrap().with(|state| state.nonce = u64::MAX);
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_some());
        cell(h).unwrap().with(|state| state.nonce = 19); // explicit isolated host fixture only
        let mut change = Change::begin(h, (SLOT, ID)).unwrap();
        change.begin_effects();
        drop(change);
        assert!(mask(h).is_none());
        assert!(Change::begin(h, (SLOT, ID)).is_none());
        assert!(Join::begin(h, (SLOT, ID), UserAbi::Windows).is_none());
        unregister(h, SLOT, ID);
        assert!(register_root(ProcessHandle::new(h.index(), 43), SLOT, ID + 1, 7).is_none());
        cell(h).unwrap().with(|state| {
            state.phase = 0;
            state.release_empty();
        }); // not production recovery
    });
}
