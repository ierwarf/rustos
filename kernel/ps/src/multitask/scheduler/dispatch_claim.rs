//! Owner: exact selected frame, funding and queue-to-CPU preparation transaction.
//! Boundary: borrows the original masked CpuDispatchTurn; allocation is not a slot hint.
//! Lifecycle: pin/read/recheck -> original funding -> atomic frame/owner claim -> CPU banks.
//! Concurrency: final frame epoch check shares the real resource leaf with writer/reader exclusion.
//! Failure: stale identity/frame/queue or refused funding drops the original pins without staging.
//! Forbidden: naked generation grants, rereading donation after claim, or replacing an active interval.
//! Evidence: actual frame epoch, UID, physical allocation, claim CAS and prepared funding; not full IRQ restore.
use super::{
    context_validation, current_identity, frame_publication, runqueue, scheduling_context,
    stack_resources,
};
use crate::multitask::cpu_local::dispatch_turn::CpuDispatchTurn;

#[derive(Clone, Copy)]
pub(super) enum FrameCustody {
    Local,
    Direct,
    Current,
}

pub(super) enum FrameCapture<'a> {
    Ready(DispatchFrame<'a>),
    Stale,
    Invalid(&'static str),
}

pub(super) struct DispatchFrame<'a> {
    turn: &'a CpuDispatchTurn<'static>,
    cpu: usize,
    id: u64,
    token: frame_publication::StableFrameToken,
    custody: FrameCustody,
    return_to_user: bool,
    // Original selection observation, qualified only under the ledger leaf
    // with this non-Copy frame/allocation and original funding, never a pin.
    class_sequence: u64,
    pin: stack_resources::StackPin,
}

pub(super) struct FundedFrame<'a> {
    frame: DispatchFrame<'a>,
    funding: scheduling_context::dispatch::DispatchFunding,
    previous: super::return_architecture::ArchitectureState,
}

impl<'a> DispatchFrame<'a> {
    pub(super) fn capture(
        turn: &'a CpuDispatchTurn<'static>,
        slot: usize,
        expected_id: u64,
        generation: u64,
        rsp: usize,
        custody: FrameCustody,
    ) -> FrameCapture<'a> {
        Self::capture_selected(turn, slot, expected_id, generation, rsp, custody,
            super::donation_ledger::class_publication_sequence())
    }

    pub(super) fn capture_selected(
        turn: &'a CpuDispatchTurn<'static>, slot: usize, expected_id: u64,
        generation: u64, rsp: usize, custody: FrameCustody, class_sequence: u64,
    ) -> FrameCapture<'a> {
        let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::ClaimCapture);
        let Some((cpu, original)) = turn.original_current() else {
            return FrameCapture::Stale;
        };
        if matches!(custody, FrameCustody::Current) && slot != original {
            return FrameCapture::Stale;
        }
        let token = match custody {
            FrameCustody::Local => frame_publication::local_frame_token(slot, cpu),
            FrameCustody::Direct => frame_publication::direct_handoff_frame_token(slot, cpu),
            FrameCustody::Current => frame_publication::current_local_frame_token(slot, cpu),
        };
        let Some(token) = token else {
            return FrameCapture::Stale;
        };
        if !rustos_scheduler_frame_gate::selected_local_frame_identity(
            generation, token.owner_generation, rsp, token.saved_rsp,
        ) {
            return FrameCapture::Stale;
        }
        let Some(id) = current_identity::read_dispatch(slot).and_then(|p| p.task_id) else {
            return FrameCapture::Stale;
        };
        if id != expected_id {
            return FrameCapture::Stale;
        }
        let Some(pin) = frame_publication::pin_for_identity(token, id) else {
            return FrameCapture::Stale;
        };
        // SAFETY: the borrowed original turn excludes local IRQ/preemption;
        // this real allocation pin precedes reader admission and any byte read.
        let Some(reader) = (unsafe { stack_resources::frame_readers::acquire(&pin, cpu) }) else {
            return FrameCapture::Stale;
        };
        let mut view = token.view;
        if let Some(intact) = stack_resources::owned_canary_intact(slot) {
            view.stack_canary_intact &= intact;
        }
        let validation = context_validation::validate_published_saved_context(slot, rsp, view);
        // SAFETY: the exact allocation pin and actual FrameReader still retain
        // immutable bytes; successful header validation proves this RSP's bounds.
        let return_to_user = validation.is_ok() && unsafe {
            (*(rsp as *const super::SavedContext)).cs == crate::arch::gdt::user_code_selector().0 as u64
        };
        let exact = Self::token_current(token, cpu, custody)
            && current_identity::read_dispatch(slot).is_some_and(|p| p.task_id == Some(id));
        drop(reader);
        if !exact {
            return FrameCapture::Stale;
        }
        if let Err(reason) = validation {
            return FrameCapture::Invalid(reason);
        }
        FrameCapture::Ready(Self {
            turn,
            cpu,
            id,
            token,
            custody,
            return_to_user,
            class_sequence,
            pin,
        })
    }

    fn token_current(
        token: frame_publication::StableFrameToken,
        cpu: usize,
        custody: FrameCustody,
    ) -> bool {
        match custody {
            FrameCustody::Local => frame_publication::token_is_current(token, cpu),
            FrameCustody::Direct => frame_publication::direct_handoff_token_is_current(token, cpu),
            FrameCustody::Current => frame_publication::current_local_token_is_current(token, cpu),
        }
    }

    /// This check is invoked INSIDE the resource/identity/placement claim leaf,
    /// immediately before the exact owner CAS. A same-generation frame writer
    /// between byte validation and claim therefore refuses, not dispatches.
    pub(super) fn is_current(&self) -> bool {
        self.turn.original_current().is_some_and(|(cpu, original)| {
            cpu == self.cpu
                && (!matches!(self.custody, FrameCustody::Current) || original == self.token.slot)
        }) && Self::token_current(self.token, self.cpu, self.custody)
            && current_identity::read_dispatch(self.token.slot)
                .is_some_and(|p| p.task_id == Some(self.id))
    }

    pub(super) fn prepare(self) -> Option<FundedFrame<'a>> {
        let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::ClaimFunding);
        if !self.is_current() {
            return None;
        }
        let previous = super::return_architecture::ArchitectureState::original(self.turn)?;
        let funding = scheduling_context::dispatch::prepare_grant(self.token.slot, self.cpu)?;
        if funding.task_id != self.id || !self.is_current() {
            return None;
        }
        Some(FundedFrame {
            frame: self,
            funding,
            previous,
        })
    }

    /// rq40 -> original class45 -> original Context48/Domain49 -> exact
    /// Terminal/Identity/Resources/Placement claim. No raw leaf escapes this
    /// callback; retained non-Copy grants are published only AFTER it returns.
    pub(super) fn with_admission<T>(
        &self, funding: Option<&super::scheduling_runtime::FundingPin>,
        claim: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        // Fixed idle progress is not a class winner or donation receiver.
        // Its SAME physical frame/UID/placement claim remains mandatory, but
        // unrelated class writers cannot turn an incomplete scan into panic.
        if runqueue::is_idle_slot(self.token.slot) {
            if funding.is_some() { return None; }
            return claim();
        }
        super::donation_ledger::with_class_claim(self.class_sequence, || {
            match funding {
                Some(pin) => pin.with_dispatch_admission(self.cpu,
                    crate::arch::clock::monotonic_nanos(), claim),
                None => claim(),
            }
        })
    }

    pub(super) fn slot(&self) -> usize {
        self.token.slot
    }
    pub(super) fn generation(&self) -> u64 {
        self.token.owner_generation
    }
    pub(super) fn cpu(&self) -> usize {
        self.cpu
    }
}

impl FundedFrame<'_> {
    /// No fallible work follows the successful CAS. Original allocation and
    /// funding move into execution under THIS turn before claim returns. No
    /// later scalar Scheduler lookup can open or replace the interval.
    pub(super) fn claim(self) -> bool {
        let _phase = super::runtime_profile::phase_scope(super::SchedulerPhase::ClaimCommit);
        let slot = self.frame.slot();
        let cpu = self.frame.cpu();
        if !runqueue::claim_validated_dispatch(&self.frame, runqueue::weight::value(slot),
            self.funding.funding.as_ref()) {
            return false;
        }
        stack_resources::cpu_leases::prepare(cpu, self.frame.pin);
        self.frame.turn.retain_claimed_return(super::return_architecture::ClaimedReturn {
            id: self.frame.id,
            token: self.frame.token,
            running_generation: self.frame.token.owner_generation.checked_add(1).expect("claim generation exhausted"),
            return_to_user: self.frame.return_to_user,
            previous: self.previous,
        });
        super::execution_interval::open_claimed(self.frame.turn, self.funding.funding);
        true
    }
}

/// Progress uses the same real frame/funding/placement transaction as selected
/// work, never a discarded validation boolean or an unbudgeted-current escape.
pub(super) fn claim_fallback(turn: &CpuDispatchTurn<'static>, idle_slot: usize, class_sequence: u64) -> usize {
    let (cpu, original) = turn
        .original_current()
        .expect("fallback lost original CPU bank");
    if let Some(token) = frame_publication::current_local_frame_token(original, cpu) {
        if let FrameCapture::Ready(frame) = DispatchFrame::capture_selected(
            turn,
            original,
            current_identity::read_dispatch(original)
                .and_then(|p| p.task_id)
                .expect("current fallback lost identity"),
            token.owner_generation,
            token.saved_rsp,
            FrameCustody::Current,
            class_sequence,
        ) {
            if frame.prepare().is_some_and(FundedFrame::claim) {
                return original;
            }
        }
    }
    assert_ne!(
        idle_slot, original,
        "idle fallback lost its own original custody"
    );
    assert!(
        runqueue::is_idle_slot(idle_slot),
        "fallback target is not fixed CPU idle"
    );
    let token = frame_publication::local_frame_token(idle_slot, cpu)
        .expect("idle fallback lost exact published frame");
    let FrameCapture::Ready(frame) = DispatchFrame::capture(
        turn,
        idle_slot,
        current_identity::read_dispatch(idle_slot)
            .and_then(|p| p.task_id)
            .expect("idle fallback lost identity"),
        token.owner_generation,
        token.saved_rsp,
        FrameCustody::Local,
    ) else {
        panic!("idle fallback frame is unavailable or corrupted");
    };
    let funded = frame
        .prepare()
        .expect("fixed idle fallback funding is unavailable");
    assert!(
        funded.funding.funding.is_none(),
        "fixed idle acquired budget authority"
    );
    assert!(
        funded.claim(),
        "idle fallback lost exact frame/placement/queue custody"
    );
    super::runqueue_policy::latch_fallback_idle_dispatch();
    idle_slot
}

#[cfg(test)]
pub(super) mod tests;
