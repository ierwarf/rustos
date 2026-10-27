//! Native witnesses: a recorded refusal retries only its exact wait instance.
use super::*;
use crate::multitask::scheduler::{root_creation, runqueue};
use crate::user::abi::UserAbi;

const ID: u64 = 81900;
const TOKEN: u64 = 905;

fn root(f: impl FnOnce(usize)) {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    root_creation::tests::exec_root(ID, true, boot, |slot, _| f(slot));
}

fn scope() -> DispatchPreemption<'static> {
    let mut scope = DispatchPreemption::enter_model(0);
    scope.mask_irqs();
    scope
}

fn arm(slot: usize, token: u64) {
    assert!(runqueue::wait::try_publish_arm(slot, runqueue::wait::REASON_PAGER_FAULT, token));
}

fn drain(scope: &DispatchPreemption<'_>) {
    let _ = with_queue(scope, Queue::take);
}

#[test]
fn refusal_is_recorded_only_for_a_published_matching_wait() {
    root(|slot| {
        let scope = scope();
        drain(&scope);
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        // Not waiting: nothing to retry.
        assert!(!record_in(&scope, ID, None));
        assert!(!pending(scope.cpu()));
        arm(slot, TOKEN);
        // A typed refusal for another object is not this wait's refusal.
        assert!(!record_in(&scope, ID, Some((runqueue::wait::REASON_PAGER_FAULT, TOKEN + 1))));
        assert!(record_in(&scope, ID, Some((runqueue::wait::REASON_PAGER_FAULT, TOKEN))));
        // One token per task; a newer refusal replaces it.
        assert!(record_in(&scope, ID, None));
        assert_eq!(pending_on(&scope), 1);
        assert!(pending(scope.cpu()));
        runqueue::wait::clear_arm(slot);
        drain(&scope);
        assert!(!pending(scope.cpu()));
    });
}

#[test]
fn retry_wakes_the_same_wait_and_then_forgets_it() {
    root(|slot| {
        let scope = scope();
        drain(&scope);
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        arm(slot, TOKEN);
        assert!(record_in(&scope, ID, None));
        assert_eq!(service(&scope), 1);
        // The exact wait was woken: its arm and reason are withdrawn.
        assert_eq!(runqueue::wait::wait_instance(slot), WaitInstance::Idle);
        assert!(!runqueue::wait::armed(slot));
        assert_eq!(pending_on(&scope), 0);
    });
}

#[test]
fn retry_never_wakes_a_later_wait_of_the_same_task() {
    root(|slot| {
        let scope = scope();
        drain(&scope);
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        arm(slot, TOKEN);
        assert!(record_in(&scope, ID, None));
        // The task finished that wait and armed a new one on the same object:
        // the epoch advanced, so the stale token must not wake it.
        runqueue::wait::clear_arm(slot);
        arm(slot, TOKEN);
        assert_eq!(service(&scope), 1);
        assert!(runqueue::wait::armed(slot));
        assert!(matches!(runqueue::wait::wait_instance(slot), WaitInstance::Waiting { .. }));
        assert_eq!(pending_on(&scope), 0);
        runqueue::wait::clear_arm(slot);
    });
}

#[test]
fn retry_cannot_capture_a_new_same_object_epoch_after_its_observation() {
    root(|slot| {
        let scope = scope();
        drain(&scope);
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        arm(slot, TOKEN);
        let WaitInstance::Waiting { serial, kind, id } = wait_instance(slot) else {
            panic!("original wait missing");
        };
        let token = Token { task_id: ID, slot, serial, kind, id, expires_tick: u64::MAX };
        let mut newer = None;
        let result = retry_after_observing(&scope, token, &|_| 1, || {
            runqueue::wait::clear_arm(slot);
            arm(slot, TOKEN);
            newer = Some(wait_instance(slot));
        });
        assert_eq!(result, Retry::Keep, "stale observation must fail atomic epoch capture");
        assert_eq!(wait_instance(slot), newer.unwrap());
        assert!(runqueue::wait::armed(slot));
        assert_eq!(retry(&scope, token, &|_| 1), Retry::Done);
        runqueue::wait::clear_arm(slot);
    });
}

#[test]
fn retry_only_work_defeats_idle_return_until_the_last_token_is_resolved() {
    root(|slot| nucleus_core::util::lockdep::with_native_cpu(7, || {
        let cpu = 7;
        let mut scope = DispatchPreemption::enter_model_with_enabled_irqs(cpu);
        scope.mask_irqs();
        drain(&scope);
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        arm(slot, TOKEN);
        assert!(!runqueue::local_dispatch_work_pending(cpu));
        assert!(!crate::multitask::scheduler::local_dispatch_work_pending(cpu));
        assert!(record_in(&scope, ID, None));
        assert!(crate::multitask::scheduler::local_dispatch_work_pending(cpu));
        let binding = wake_publication::Binding::capture(slot, ID).unwrap();
        let phase = runqueue::wait::TypedWake::capture(&binding,
            runqueue::wait::REASON_PAGER_FAULT, TOKEN).unwrap();
        // A different producer still owns the pending wait; this entry cannot
        // complete it. Every later idle tick must continue to see retry work.
        assert_eq!(service_with(&scope, &|_| 1), 0);
        assert!(pending(cpu));
        assert!(crate::multitask::scheduler::local_dispatch_work_pending(cpu));
        drop(phase);
        drop(binding);
        assert_eq!(service_with(&scope, &|_| 1), 1);
        assert!(!pending(cpu));
        assert!(!crate::multitask::scheduler::local_dispatch_work_pending(cpu));
    }));
}

#[test]
fn retry_drops_a_token_whose_slot_names_another_task() {
    root(|slot| {
        let scope = scope();
        drain(&scope);
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        arm(slot, TOKEN);
        let WaitInstance::Waiting { serial, kind, id } = runqueue::wait::wait_instance(slot) else {
            panic!("armed wait not observable");
        };
        let stale = Token { task_id: ID + 1, slot, serial, kind, id, expires_tick: u64::MAX };
        assert!(with_queue(&scope, |queue| queue.insert(stale)));
        assert_eq!(service(&scope), 1);
        // The real task's wait is untouched.
        assert!(runqueue::wait::armed(slot));
        runqueue::wait::clear_arm(slot);
    });
}
