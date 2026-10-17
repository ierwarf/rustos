//! Owner: constructor ACK and original one-shot initial CPU admission.
//! Boundary: hidden descriptor publication is separate from runtime claim/wake.
//! Lifecycle: Dormant -> Blocked ACK; original image alone joins initial Running.
//! Concurrency: exact owner CAS and image borrow; no owner leaf crosses work.
//! Failure: duplicate, foreign CPU/UID, reservation or typed wait fails closed.
//! Forbidden: Blocked ACK and copied owner generations never grant execution.
//! Evidence: actual born initial descriptor/interval/bank witnesses; HAL modeled.
use super::{OWNER_WORDS, RunOwnerState, frame_publication, owner, validate_cpu, wait};

pub(in crate::multitask::scheduler) fn admit_blocked(slot: usize) {
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Dormant,
        "scheduler slot admitted twice"
    );
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Blocked, None))
        .unwrap_or_else(|observed| panic!("scheduler slot admission raced observed={observed:?}"));
}

/// The original one-shot CPU image, not a queue hint or copied generation,
/// admits a fully materialized hidden kernel after producer Blocked ACK.
pub(in crate::multitask::scheduler) fn admit_initial(
    image: &super::super::bootstrap::InitialCpu<'_>,
) {
    image.assert_materializing();
    let slot = image.slot();
    let cpu = image.cpu();
    validate_cpu(cpu);
    assert_eq!(
        super::super::current_identity::read_dispatch(slot).and_then(|p| p.task_id),
        Some(image.id()),
        "initial Running admission lost original UID"
    );
    assert!(!super::super::stack_resources::admission::is_reserved(slot));
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Blocked,
        "initial image spent twice"
    );
    assert!(
        !wait::armed(slot) && wait::reason(slot) == (wait::REASON_NONE, 0),
        "initial image acquired a typed wait intent"
    );
    let running = owner.next(RunOwnerState::Running, Some(cpu));
    OWNER_WORDS[slot]
        .compare_exchange(owner, running)
        .expect("initial image Running publication raced");
    frame_publication::bind_local_owner_generation(slot, running.generation);
}

pub(in crate::multitask::scheduler) fn admit_running(slot: usize, cpu: usize) {
    validate_cpu(cpu);
    let owner = owner(slot);
    assert_eq!(
        owner.state,
        RunOwnerState::Dormant,
        "scheduler slot admitted twice"
    );
    OWNER_WORDS[slot]
        .compare_exchange(owner, owner.next(RunOwnerState::Running, Some(cpu)))
        .unwrap_or_else(|observed| {
            panic!("scheduler running admission raced observed={observed:?}")
        });
}
