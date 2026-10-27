//! Owner: exact Running CPU return and Blocked wake queue publication.
//! Boundary: queue custody only; original caller retains physical bank/frame and IRQ scope.
//! Lifecycle: Running -> Local/Blocked or terminal refusal; Blocked -> Local wake CAS.
//! Concurrency: wait/wake change run intent in the same word; CAS loss retries without resurrection.
//! Failure: terminal/foreign custody refuses; inserted provisional membership rolls back exactly.
//! Forbidden: owner word or published bitmap as CPU/stack/MM execution or reuse authority.
//! Evidence: actual owner/queue/frame-generation operations; enclosing dispatch serializer remains.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::multitask::scheduler) enum ReturnPublication {
    Local,
    Blocked { was_runnable: bool },
    Terminal,
}

/// Publishes only the outgoing queue state. Caller retains its original exact
/// CPU bank and complete outgoing frame; this never grants new execution.
pub(in crate::multitask::scheduler) fn publish_return(
    slot: usize,
    cpu: usize,
    weight: u32,
    allow_local: bool,
) -> ReturnPublication {
    publish_return_inner(slot, cpu, weight, allow_local, |_| {})
}

fn publish_return_inner(
    slot: usize,
    cpu: usize,
    weight: u32,
    allow_local: bool,
    mut before_cas: impl FnMut(usize),
) -> ReturnPublication {
    validate_cpu(cpu);
    // A blocked (or refused) return changes no queue membership, so it needs no queue
    // leaf. The leaf used to order this return's frame-generation rebind against a
    // remote wake's rebind; binding the generation BEFORE the owner CAS orders them
    // instead: while the slot is still Running no wake writes its frame token (a wake
    // of a Running slot only raises its run intent in the owner word), and once the CAS
    // makes Blocked visible this return writes nothing more. A CAS lost to a raced wake
    // leaves a token naming a generation no owner word carries, which no reader accepts
    // and the next bind overwrites.
    loop {
        let observed = owner(slot);
        if observed.state.is_terminal() {
            return ReturnPublication::Terminal;
        }
        assert!(
            observed.state == RunOwnerState::Running && observed.cpu == Some(cpu),
            "CPU return lacks original Running custody slot={slot} owner={observed:?} cpu={cpu}"
        );
        if observed.runnable && allow_local {
            break;
        }
        let next = observed.next_preserving_wait(RunOwnerState::Blocked, None);
        frame_publication::bind_local_owner_generation(slot, next.generation);
        before_cas(slot);
        if OWNER_WORDS[slot].compare_exchange(observed, next).is_ok() {
            return ReturnPublication::Blocked {
                was_runnable: observed.runnable,
            };
        }
    }
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    loop {
        let observed = owner(slot);
        // The published bitmap and load describe queue membership. Only a local
        // insert (or its rollback) changes it, so the refusal and blocked returns
        // below leave the publication, and its two locked sequence bumps, alone.
        if observed.state.is_terminal() {
            return ReturnPublication::Terminal;
        }
        assert!(
            observed.state == RunOwnerState::Running && observed.cpu == Some(cpu),
            "CPU return lacks original Running custody slot={slot} owner={observed:?} cpu={cpu}"
        );
        let local = observed.runnable && allow_local;
        let next = if local {
            rq.insert(slot, weight);
            observed.next(RunOwnerState::Local, Some(cpu))
        } else {
            observed.next_preserving_wait(RunOwnerState::Blocked, None)
        };
        before_cas(slot);
        if OWNER_WORDS[slot].compare_exchange(observed, next).is_ok() {
            // The complete outgoing frame also belongs to a blocked return.
            // Original Current/Transition still excludes reads until RSP ACK;
            // afterwards a typed wake must observe THIS committed generation.
            // The queue leaf excludes a concurrent wake's later rebind here.
            frame_publication::bind_local_owner_generation(slot, next.generation);
            if local {
                RUN_QUEUES[cpu].publish_load(&rq);
            }
            return if local {
                ReturnPublication::Local
            } else {
                ReturnPublication::Blocked {
                    was_runnable: observed.runnable,
                }
            };
        }
        // A raced wake changes run intent, so retry the SAME original Running
        // CPU rather than force its earlier blocked decision. Terminal always
        // wins. No provisional queue entry survives a failed owner CAS.
        if local {
            rq.remove(slot, weight);
            RUN_QUEUES[cpu].publish_load(&rq);
        }
    }
}

pub(in crate::multitask::scheduler) fn publish_blocked_wake(
    slot: usize,
    cpu: usize,
    weight: u32,
) -> RemoteWakeOutcome {
    publish_blocked_wake_inner(slot, cpu, weight, |_| {})
}
fn publish_blocked_wake_inner(
    slot: usize,
    cpu: usize,
    weight: u32,
    mut before_cas: impl FnMut(usize),
) -> RemoteWakeOutcome {
    validate_cpu(cpu);
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    loop {
        let observed = owner(slot);
        if observed.state.is_terminal() || observed.state == RunOwnerState::Dormant {
            return RemoteWakeOutcome::Rejected;
        }
        if super::ipc_handoff::local_wake_owner_is_already_owned(observed.state) {
            return RemoteWakeOutcome::AlreadyOwned { cpu: observed.cpu };
        }
        if observed.state != RunOwnerState::Blocked {
            return RemoteWakeOutcome::Rejected;
        }
        rq.insert(slot, weight);
        let next = observed.next(RunOwnerState::Local, Some(cpu));
        before_cas(slot);
        if OWNER_WORDS[slot].compare_exchange(observed, next).is_ok() {
            frame_publication::bind_local_owner_generation(slot, next.generation);
            RUN_QUEUES[cpu].publish_load(&rq);
            return RemoteWakeOutcome::Published { cpu, notify: false };
        }
        rq.remove(slot, weight);
        RUN_QUEUES[cpu].publish_load(&rq);
    }
}

/// Native queue-only input for a retained physical Current-bank wake witness.
/// Models the earlier claim, never supplies CPU/MM/stack execution permission.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn test_running_claim(slot: usize, cpu: usize, weight: u32) {
    let mut rq = RUN_QUEUES[cpu].inner.lock();
    let observed = owner(slot);
    assert_eq!(observed.state, RunOwnerState::Local);
    assert_eq!(observed.cpu, Some(cpu));
    OWNER_WORDS[slot]
        .compare_exchange(observed, observed.next(RunOwnerState::Running, Some(cpu)))
        .unwrap();
    rq.remove(slot, weight);
    RUN_QUEUES[cpu].publish_load(&rq);
}

/// Native earlier claim input followed by actual queue return, not MM/CPU grant.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn test_blocked_return(slot: usize, cpu: usize, weight: u32) {
    test_running_claim(slot, cpu, weight);
    set_runnable(slot, false);
    assert_eq!(
        publish_return(slot, cpu, weight, true),
        ReturnPublication::Blocked {
            was_runnable: false
        }
    );
}

#[cfg(test)]
mod tests;
