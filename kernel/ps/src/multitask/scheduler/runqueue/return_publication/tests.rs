//! Owner: actual queue return/wake CAS arbitration witnesses.
//! Boundary: native owner-word projections, not physical CPU return or register acceptance.
//! Lifecycle: provisional Local publication retries wait/wake and rolls back terminal loss.
//! Concurrency: one deterministic competing actual owner operation at the CAS boundary.
//! Failure: terminal wins without resurrection; neighbour membership remains unchanged.
//! Forbidden: these fixtures as stack/MM/IRQ or guard-deletion authority.
//! Evidence: original queue, owner, published membership and frame-generation effects.
use super::*;

struct Fixture {
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let guard = super::super::tests::test_serial_guard();
        reset_before_publication();
        Self { _serial: guard }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        reset_before_publication();
    }
}
const SLOT: usize = 65;
const NEIGHBOUR: usize = 66;
const CPU: usize = 0;
const WEIGHT: u32 = 100;
fn neighbour() {
    admit_blocked(NEIGHBOUR);
    assert_eq!(
        publish_blocked_wake(NEIGHBOUR, CPU, WEIGHT),
        RemoteWakeOutcome::Published {
            cpu: CPU,
            notify: false
        }
    );
}
fn contains(slot: usize) -> bool {
    let word = slot / 64;
    let bit = slot % 64;
    // ORDERING: inspect the exact published queue membership after its Release,
    // not the native all-slots candidate scan or any execution permission.
    RUN_QUEUES[CPU].published_runnable[word].load(Ordering::Acquire) & (1_u64 << bit) != 0
}

#[test]
fn actual_return_raced_wake_recomputes_run_intent_and_publishes_once() {
    let _fixture = Fixture::new();
    neighbour();
    admit_running(SLOT, CPU);
    set_runnable(SLOT, false);
    let mut raced = false;
    let result = publish_return_inner(SLOT, CPU, WEIGHT, true, |slot| {
        if !raced {
            raced = true;
            wake_wait(slot);
        }
    });
    assert_eq!(result, ReturnPublication::Local);
    assert!(contains(SLOT) && contains(NEIGHBOUR));
    assert_eq!(published_runnable_count(CPU), 2);
    assert_eq!(owner(SLOT).state, RunOwnerState::Local);
    assert!(owner(SLOT).runnable);
    assert_eq!(
        RUN_QUEUES[CPU].inner.lock().runnable_weight,
        2 * u64::from(WEIGHT)
    );
}

#[test]
fn actual_return_raced_wait_rolls_back_only_its_provisional_membership() {
    let _fixture = Fixture::new();
    neighbour();
    admit_running(SLOT, CPU);
    let mut raced = false;
    let result = publish_return_inner(SLOT, CPU, WEIGHT, true, |slot| {
        if !raced {
            raced = true;
            set_runnable(slot, false);
        }
    });
    assert_eq!(
        result,
        ReturnPublication::Blocked {
            was_runnable: false
        }
    );
    assert!(!contains(SLOT) && contains(NEIGHBOUR));
    assert_eq!(owner(SLOT).state, RunOwnerState::Blocked);
    assert!(!owner(SLOT).runnable);
    assert_eq!(
        RUN_QUEUES[CPU].inner.lock().runnable_weight,
        u64::from(WEIGHT)
    );
}

#[test]
fn actual_return_terminal_wins_without_resurrection_or_neighbour_withdrawal() {
    let _fixture = Fixture::new();
    neighbour();
    admit_running(SLOT, CPU);
    let mut raced = false;
    let result = publish_return_inner(SLOT, CPU, WEIGHT, true, |slot| {
        // Original observed owner is Running: retirement does not reacquire
        // this CPU queue leaf. Provisional membership belongs to this turn.
        if !raced {
            raced = true;
            retire(slot, WEIGHT);
        }
    });
    assert_eq!(result, ReturnPublication::Terminal);
    assert_eq!(owner(SLOT).state, RunOwnerState::Retired);
    assert!(!contains(SLOT) && contains(NEIGHBOUR));
    assert_eq!(
        RUN_QUEUES[CPU].inner.lock().runnable_weight,
        u64::from(WEIGHT)
    );
}

#[test]
fn actual_blocked_wake_terminal_wins_and_rolls_back_same_queue_turn() {
    let _fixture = Fixture::new();
    neighbour();
    admit_blocked(SLOT);
    let mut raced = false;
    let result = publish_blocked_wake_inner(SLOT, CPU, WEIGHT, |slot| {
        if !raced {
            raced = true;
            retire(slot, WEIGHT);
        }
    });
    assert_eq!(result, RemoteWakeOutcome::Rejected);
    assert_eq!(owner(SLOT).state, RunOwnerState::Retired);
    assert!(!contains(SLOT) && contains(NEIGHBOUR));
    assert_eq!(
        RUN_QUEUES[CPU].inner.lock().runnable_weight,
        u64::from(WEIGHT)
    );
}
