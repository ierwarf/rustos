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
        arm(slot, TOKEN);
        // A typed refusal for another object is not this wait's refusal.
        assert!(!record_in(&scope, ID, Some((runqueue::wait::REASON_PAGER_FAULT, TOKEN + 1))));
        assert!(record_in(&scope, ID, Some((runqueue::wait::REASON_PAGER_FAULT, TOKEN))));
        // One token per task; a newer refusal replaces it.
        assert!(record_in(&scope, ID, None));
        assert_eq!(pending_on(&scope), 1);
        runqueue::wait::clear_arm(slot);
        drain(&scope);
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
