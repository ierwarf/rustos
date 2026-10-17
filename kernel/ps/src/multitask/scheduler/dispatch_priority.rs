//! First-turn publication versus a complete own-CPU dispatch choice.
//!
//! - **Owner:** original non-Copy CpuDispatchTurn owns Choice until Current publication.
//! - **Boundary:** per-CPU policy publication, not task/frame/MM or funding permission.
//! - **Lifecycle:** vacant -> choice -> actual Current publication -> vacant; abandoned
//!   claimed choice poisons instead of acknowledging a switch that did not happen.
//! - **Concurrency:** original masked CPU scope, acquire/release phase; no raw leaf
//!   is retained while waiting, validating frames, claiming, or installing registers.
//! - **Failure:** poisoned CPU fails closed; publication cannot pass an admitted choice.
//! - **Forbidden:** scalar generation/slot lookup as choice lifetime, global serializer,
//!   phase release before actual Current publication, consuming an incomplete FIFO head.
//! - **Evidence:** physical turn witnesses and independent phase contention witnesses.
use core::{marker::PhantomData, sync::atomic::{AtomicU8, Ordering}};
use nucleus_core::util::lockdep::{DispatchPreemption, MAX_TRACKED_CPUS};

const VACANT: u8 = 0;
const CHOOSING: u8 = 1;
const PUBLISHING: u8 = 2;
const POISONED: u8 = 3;
static PHASES: [Phase; MAX_TRACKED_CPUS] = [const { Phase::new() }; MAX_TRACKED_CPUS];

struct Phase(AtomicU8);
impl Phase {
    const fn new() -> Self { Self(AtomicU8::new(VACANT)) }
    fn admit(&self, desired: u8) {
        loop {
            // ORDERING: acquire the preceding publication's release before
            // observation; failed admission never grants priority custody.
            match self.0.compare_exchange(VACANT, desired, Ordering::Acquire, Ordering::Acquire) {
                Ok(_) => return,
                Err(POISONED) => panic!("priority publication reached an abandoned CPU choice"),
                Err(_) => core::hint::spin_loop(),
            }
        }
    }
}

/// Stored only in the original turn, never reconstructed from policy observations.
#[must_use]
pub(in crate::multitask) struct Choice {
    phase: &'static Phase,
    live: bool,
    affinity: PhantomData<*mut ()>,
}
impl Choice {
    pub(in crate::multitask) fn begin(turn: &crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn<'_>) -> Self {
        let (cpu, _) = turn.original_current().expect("priority choice lacks original Current");
        assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0,
            "priority choice waits inside a raw leaf");
        let phase = &PHASES[cpu];
        phase.admit(CHOOSING);
        Self { phase, live: true, affinity: PhantomData }
    }
    pub(in crate::multitask) fn is_live(&self) -> bool {
        // ORDERING: qualify this original still-owned choice against the phase
        // it exclusively acquired, not a task/frame lifetime observation.
        self.live && self.phase.0.load(Ordering::Acquire) == CHOOSING
    }
    /// Called only AFTER the original turn's actual Current/Transition publication.
    pub(in crate::multitask) fn published(mut self) {
        assert!(self.is_live(), "priority choice lost original phase");
        // ORDERING: actual Current/Transition is visible before the next
        // priority producer acquires the phase; this is not an RSP ACK.
        self.phase.0.store(VACANT, Ordering::Release);
        self.live = false;
    }
}
impl Drop for Choice {
    fn drop(&mut self) {
        if self.live {
            // No successful CPU publication was observed; fail closed, even when
            // abandoned before claim. Native reset is explicitly a fixture ACK.
            // ORDERING: later admission observes irreversible abandonment,
            // never a fictitious successful Current/Transition publication.
            self.phase.0.store(POISONED, Ordering::Release);
        }
    }
}

/// Bounded FIFO publication only. The caller retains its original Cohort/frame loans.
#[must_use]
pub(super) struct Publication<'scope, 'irq> {
    phase: &'static Phase,
    // Borrow the ORIGINAL non-Copy CPU owner through the final publication,
    // not just its scalar masked/CPU observations at admission.
    scope: &'scope DispatchPreemption<'irq>,
    affinity: PhantomData<*mut ()>,
}
impl<'scope, 'irq> Publication<'scope, 'irq> {
    pub(super) fn begin(scope: &'scope DispatchPreemption<'irq>, target: usize) -> Self {
        assert!(scope.is_masked(), "priority publisher lacks original masked CPU scope");
        assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0,
            "priority publisher waits inside a raw leaf");
        let phase = PHASES.get(target).expect("priority target exceeds CPU capacity");
        // ORDERING: observe an admitted original same-CPU choice before
        // attempting publication, refusing self-wait under masked exclusion.
        assert!(scope.cpu() != target || phase.0.load(Ordering::Acquire) != CHOOSING,
            "priority publisher overlaps its own original dispatch choice");
        phase.admit(PUBLISHING);
        Self { phase, scope, affinity: PhantomData }
    }
}
impl Drop for Publication<'_, '_> {
    fn drop(&mut self) {
        assert!(self.scope.is_masked(), "priority publication lost its original CPU owner");
        assert_eq!(nucleus_core::util::lockdep::held_spin_lock_depth(), 0,
            "priority publication released before its raw leaf");
        // ORDERING: complete FIFO/count/pending effects precede the next choice.
        self.phase.0.store(VACANT, Ordering::Release);
    }
}

#[cfg(test)]
pub(in crate::multitask) fn reset_model(cpu: usize) {
    // Native fixture recovery only; never production abandonment recovery.
    // ORDERING: fixture cleanup publishes its earlier modeled ACK, not a
    // production recovery path for a poisoned or live choice.
    PHASES[cpu].0.store(VACANT, Ordering::Release);
}

#[cfg(test)]
mod tests;
