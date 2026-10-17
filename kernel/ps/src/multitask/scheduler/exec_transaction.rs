//! Image custody spans process staging, scheduler/CR3 publication and process
//! finalization. No resource leaf lock spans any of those operations. Existing
//! sibling quiescence and HAL inactive-root/TLB retirement remain mandatory.
use super::*;
use crate::multitask::process_table::ExecReservation;
use core::cell::{Cell, RefCell};

/// Copied candidate, never image permission. Intent::begin joins exact identity
/// and live resource binding before granting an exec producer.
pub(in crate::multitask) fn peer_exec_candidate(
    pid: u64,
    tid: u64,
) -> Option<(usize, ProcessHandle)> {
    if pid == 0 || pid != tid {
        return None;
    }
    (1..MAX_TASK).find_map(|slot| {
        let context = stack_resources::context::read(slot)?;
        if !context.user_mode
            || context.user_abi != Some(UserAbi::Linux)
            || context.process_id != Some(pid)
            || stack_resources::task_start::read(slot)?.id != tid
        {
            return None;
        }
        Some((slot, context.process_handle?))
    })
}
/// Own-CPU publication scope, not a directory transaction or a MM hardware ACK.
pub(in crate::multitask) fn claim_exec_candidate(
    target: Option<(usize, u64, u64)>,
    reservation: ExecReservation,
    linux: bool,
) -> Option<(super::super::current_identity::exec_intent::Intent, bool)> {
    #[cfg(not(test))]
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    #[cfg(test)]
    let scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    #[cfg(not(test))]
    scope.mask_irqs();
    let slot = match target {
        Some((slot, _, _)) => slot,
        None => super::super::cpu_local::current_cpu_task_slot()?,
    };
    let context = stack_resources::context::read(slot)?;
    let start = stack_resources::task_start::read(slot)?;
    let (pid, tid) = match target {
        Some((_, pid, tid)) => (pid, tid),
        None => (context.process_id?, start.id),
    };
    if !context.user_mode
        || context.process_handle != Some(reservation.process_handle())
        || context.process_id != Some(pid)
        || start.id != tid
        || pid != tid
        || (linux && context.user_abi != Some(UserAbi::Linux))
    {
        return None;
    }
    let current = super::super::cpu_local::current_cpu_task_slot() == Some(slot);
    let intent =
        super::super::current_identity::exec_intent::begin(slot, pid, reservation, current)?;
    drop(scope);
    Some((intent, current))
}

pub(in crate::multitask::scheduler) mod publication;
pub(in crate::multitask) mod quiescence;

pub(in crate::multitask) struct ExecImageLease<'a> {
    intent: Option<&'a super::super::current_identity::exec_intent::Image<'a>>,
    census: Option<&'a super::super::process_placement::exec_membership::ExecMembership>,
    writer: &'a stack_resources::frame_access::FrameWriter,
    slot: usize,
    task_id: u64,
    reservation: ExecReservation,
    root: u64,
    current: bool,
    published: Cell<bool>,
    pending_mm: RefCell<Option<crate::multitask::process_table::MmExecutionPin>>,
    pending_activation: RefCell<Option<crate::multitask::process_table::MmActivationPin>>,
    pending_placement: RefCell<Option<super::PlacementGrant>>,
    #[cfg(test)]
    _fixture_mm: ProcessAddressSpace,
}
pub(in crate::multitask) struct ExecPublication {
    intent: Option<super::super::current_identity::exec_intent::Credential>,
    reservation: ExecReservation,
    root: u64,
}

/// Process-owner fixture only: no native instruction or CPU-publication claim.
#[cfg(test)]
pub(in crate::multitask) fn test_publication(
    reservation: ExecReservation,
    root: u64,
) -> ExecPublication {
    ExecPublication {
        intent: None,
        reservation,
        root,
    }
}
impl ExecPublication {
    pub(in crate::multitask) fn intent(
        &self,
    ) -> Option<super::super::current_identity::exec_intent::Credential> {
        self.intent
    }
    pub(in crate::multitask) fn assert_matches(&self, reservation: ExecReservation, root: u64) {
        assert_eq!(
            self.reservation, reservation,
            "foreign exec publication transaction"
        );
        assert_eq!(self.root, root, "foreign exec publication root");
        #[cfg(not(test))]
        assert!(
            self.intent.is_some(),
            "exec publication lacks cold intent custody"
        );
        if let Some(intent) = self.intent {
            assert!(
                intent.published(reservation),
                "exec finalization lost published cold intent"
            );
        }
    }
}
impl ExecImageLease<'_> {
    pub(in crate::multitask) fn bind_staged_mm(
        &self,
        staged: &crate::multitask::process_table::StagedExec,
    ) {
        assert!(!self.published.get(), "MM bound after exec publication");
        let pin = staged.pin_execution_mm(self.reservation, self.root);
        assert!(
            pin.matches_exec(self.reservation, self.root),
            "staged MM credential mismatch"
        );
        let activation = pin
            .try_activate()
            .expect("staged MM activation closed before exec hardware");
        *self.pending_mm.borrow_mut() = Some(pin);
        *self.pending_activation.borrow_mut() = Some(activation);
    }

    fn matches(
        &self,
        slot: usize,
        task_id: u64,
        handle: ProcessHandle,
        root: u64,
        current: bool,
    ) -> bool {
        (
            self.slot,
            self.task_id,
            self.reservation.process_handle(),
            self.root,
            self.current,
        ) == (slot, task_id, handle, root, current)
            && !self.published.get()
            && self
                .intent
                .is_none_or(|intent| intent.credential().admits_image(slot, task_id))
            && self
                .pending_mm
                .borrow()
                .as_ref()
                .is_some_and(|pin| pin.matches_exec(self.reservation, self.root))
    }
    #[cfg(test)]
    pub(in crate::multitask::scheduler) fn publication_for_mm_test(
        &self,
        handle: ProcessHandle,
    ) -> ExecPublication {
        self.issue_publication(handle, false)
    }
    #[cfg(test)]
    pub(in crate::multitask::scheduler) fn publication_for_cpu_test(
        &self,
        handle: ProcessHandle,
    ) -> ExecPublication {
        self.publication(handle)
    }
    fn publication(&self, handle: ProcessHandle) -> ExecPublication {
        self.issue_publication(handle, true)
    }
    fn issue_publication(&self, handle: ProcessHandle, park_current: bool) -> ExecPublication {
        assert_eq!(
            handle,
            self.reservation.process_handle(),
            "exec image changed process generation"
        );
        self.writer.exec_stack_top(self.slot, self.task_id);
        #[cfg(not(test))]
        self.census
            .expect("exec publication lacks frozen membership")
            .assert_image(
                self.intent
                    .expect("exec publication lacks intent image")
                    .credential(),
            );
        assert_eq!(
            runqueue::address_space::root(self.slot),
            self.root,
            "exec image root was not published"
        );
        #[cfg(rustos_boot_image)]
        if self.current {
            let (frame, _) = x86_64::registers::control::Cr3::read();
            assert_eq!(
                frame.start_address().as_u64(),
                self.root,
                "self exec CR3 was not installed"
            );
        }
        assert!(!self.published.replace(true), "exec image published twice");
        let incoming = self
            .pending_mm
            .borrow()
            .as_ref()
            .expect("exec publication lacks staged MM execution custody")
            .try_clone()
            .expect("staged MM execution owner closed");
        stack_resources::context::replace_exec_mm(
            self.slot,
            self.task_id,
            self.reservation,
            self.root,
            incoming,
        );
        if self.current && park_current {
            let activation = self
                .pending_activation
                .borrow_mut()
                .take()
                .expect("self exec publication lacks staged MM activation");
            let credential =
                stack_resources::activation::bind_exec(self.slot, self.task_id, activation)
                    .expect("self exec activation lost its exact resource MM binding");
            let cpu = nucleus_core::util::lockdep::current_cpu_index();
            super::super::cpu_local::retain_mm_activation(cpu, credential);
            let placement = self.pending_placement.borrow_mut().take();
            #[cfg(not(test))]
            assert!(
                placement.is_some(),
                "self exec lost staged placement permission"
            );
            if let Some(mut placement) = placement {
                placement.rebind_exec(self.writer);
                super::super::cpu_local::retain_placement(cpu, placement);
            }
        }
        if let Some(intent) = self.intent {
            intent.publication(self.reservation);
        }
        ExecPublication {
            intent: self.intent.map(|intent| intent.credential()),
            reservation: self.reservation,
            root: self.root,
        }
    }
}

/// Production completion retains exact frozen creation/membership census through
/// FrameWriter release, intent completion and staged/finalized process publication.
pub(in crate::multitask) fn with_census_image(
    intent: &super::super::current_identity::exec_intent::Intent,
    census: &super::super::process_placement::exec_membership::ExecMembership,
    reservation: ExecReservation,
    root: u64,
    current: bool,
    f: impl FnOnce(&ExecImageLease<'_>) -> Option<super::super::process_table::ExecFinalize>,
) -> Option<Option<super::super::process_table::ExecFinalize>> {
    if !intent.mode_matches(current) {
        return None;
    }
    census.assert_matches(intent);
    with_image_inner(
        intent.slot(),
        intent.task_id(),
        reservation,
        root,
        current,
        Some(intent),
        Some(census),
        f,
        |result| {
            if let Some(finalized) = result {
                intent.complete(finalized);
            }
        },
    )
}

/// Complete only after FrameWriter Drop but before caller IRQ restoration, so
/// a self exec cannot be descheduled behind its own closed admission gate.
#[cfg(test)]
pub(in crate::multitask) fn with_completed_intent_image(
    intent: &super::super::current_identity::exec_intent::Intent,
    reservation: ExecReservation,
    root: u64,
    current: bool,
    f: impl FnOnce(&ExecImageLease<'_>) -> Option<super::super::process_table::ExecFinalize>,
) -> Option<Option<super::super::process_table::ExecFinalize>> {
    if !intent.mode_matches(current) {
        return None;
    }
    with_image_inner(
        intent.slot(),
        intent.task_id(),
        reservation,
        root,
        current,
        Some(intent),
        None,
        f,
        |result| {
            if let Some(finalized) = result {
                intent.complete(finalized);
            }
        },
    )
}

#[cfg(test)]
pub(in crate::multitask) fn with_intent_image<R>(
    intent: &super::super::current_identity::exec_intent::Intent,
    reservation: ExecReservation,
    root: u64,
    current: bool,
    f: impl FnOnce(&ExecImageLease<'_>) -> R,
) -> Option<R> {
    if !intent.mode_matches(current) {
        return None;
    }
    with_image_inner(
        intent.slot(),
        intent.task_id(),
        reservation,
        root,
        current,
        Some(intent),
        None,
        f,
        |_| {},
    )
}
#[cfg(test)]
pub(in crate::multitask) fn with_image<R>(
    slot: usize,
    task_id: u64,
    reservation: ExecReservation,
    root: u64,
    current: bool,
    f: impl FnOnce(&ExecImageLease<'_>) -> R,
) -> Option<R> {
    with_image_inner(
        slot,
        task_id,
        reservation,
        root,
        current,
        None,
        None,
        f,
        |_| {},
    )
}

/// Callback includes staging and finalization, not merely scheduler mutation.
/// Refusal occurs before the callback can replace process state. Current must
/// be this CPU's exact task; stopped-peer quiescence remains externally owned.
fn with_image_inner<R>(
    slot: usize,
    task_id: u64,
    reservation: ExecReservation,
    root: u64,
    current: bool,
    intent: Option<&super::super::current_identity::exec_intent::Intent>,
    census: Option<&super::super::process_placement::exec_membership::ExecMembership>,
    f: impl FnOnce(&ExecImageLease<'_>) -> R,
    after_image: impl FnOnce(&R),
) -> Option<R> {
    // Refused inputs (including the unused new MM) are destroyed only after
    // restoring the caller's IRQ state, not inside the admission leaf/scope.
    #[cfg(not(test))]
    if intent.is_none() || census.is_none() {
        return None;
    }
    let mut callback = Some(f);
    let mut completion = Some(after_image);
    #[allow(unused_mut)] // host branch invokes FnMut directly; boot consumes it
    let mut body = || {
        let context = stack_resources::context::read(slot)?;
        let start = stack_resources::task_start::read(slot)?;
        if !context.user_mode
            || context.process_handle != Some(reservation.process_handle())
            || context.process_id != Some(task_id)
            || start.id != task_id
        {
            return None;
        }
        if current && super::super::cpu_local::current_cpu_task_slot() != Some(slot) {
            return None;
        }
        if !current && super::super::cpu_local::task_execution_owner(slot).is_some() {
            return None;
        }
        let invoke = |writer: &stack_resources::frame_access::FrameWriter| {
            // Close copied admission's gap with resource writer acquisition.
            let bound = stack_resources::context::read(slot)?;
            let bound_start = stack_resources::task_start::read(slot)?;
            if !bound.user_mode
                || bound.process_handle != Some(reservation.process_handle())
                || bound.process_id != Some(task_id)
                || bound_start.id != task_id
            {
                return None;
            }
            #[cfg(test)]
            let fixture_mm = ProcessAddressSpace::modeled_root_for_host(root);
            #[cfg(test)]
            let initial_mm = Some(crate::multitask::process_table::test_execution_mm(
                reservation,
                &fixture_mm,
            ));
            #[cfg(not(test))]
            let initial_mm: Option<crate::multitask::process_table::MmExecutionPin> = None;
            let initial_activation = initial_mm
                .as_ref()
                .map(|pin| pin.try_activate().expect("initial exec MM closed"));
            let cpu = nucleus_core::util::lockdep::current_cpu_index();
            let placement = if current {
                super::runqueue::affinity_payload::capture_current(slot, cpu)
            } else {
                None
            };
            #[cfg(not(test))]
            if current && placement.is_none() {
                return None;
            }
            let intent_image = match intent {
                Some(intent) => Some(intent.image(reservation)?),
                None => None,
            };
            if let Some(census) = census {
                census.assert_image(intent_image.as_ref()?.credential());
            }
            let lease = ExecImageLease {
                intent: intent_image.as_ref(),
                census,
                writer,
                slot,
                task_id,
                reservation,
                root,
                current,
                published: Cell::new(false),
                pending_mm: RefCell::new(initial_mm),
                pending_activation: RefCell::new(initial_activation),
                pending_placement: RefCell::new(placement),
                #[cfg(test)]
                _fixture_mm: fixture_mm,
            };
            let outcome = callback.take().expect("exec image callback consumed twice")(&lease);
            let published = lease.published.get();
            drop(lease);
            if let Some(intent_image) = intent_image {
                intent_image.finish(published);
            }
            Some(outcome)
        };
        let outcome = if let Some(intent) = intent {
            if !intent.matches(slot, reservation) || intent.task_id() != task_id {
                return None;
            }
            stack_resources::frame_access::with_owned_exec(
                slot,
                task_id,
                current,
                intent.credential(),
                invoke,
            )
            .flatten()
        } else if current {
            stack_resources::frame_access::with_current_exec(slot, task_id, invoke).flatten()
        } else {
            stack_resources::frame_access::with_exec(slot, task_id, invoke).flatten()
        };
        if let Some(ref outcome) = outcome {
            completion.take().expect("image completion invoked twice")(outcome);
        }
        outcome
    };
    #[cfg(not(test))]
    return x86_64::instructions::interrupts::without_interrupts(body);
    #[cfg(test)]
    body()
}

impl ExecImageLease<'_> {
    /// Immutable actor authentication before opening its non-yielding CPU turn.
    pub(in crate::multitask) fn cpu_actor(&self, cpu: usize) -> Option<usize> {
        let actor = super::super::cpu_local::current_cpu_task_slot()?;
        (cpu == nucleus_core::util::lockdep::current_cpu_index()
            && stack_resources::cpu_leases::owns_current(cpu, actor)
            && self.current == (actor == self.slot)
            && self.matches(
                self.slot,
                self.task_id,
                self.reservation.process_handle(),
                self.root,
                self.current
            ))
        .then_some(actor)
    }

    pub(in crate::multitask) fn finish_cpu_actor(&self, cpu: usize, actor: usize) -> bool {
        assert_eq!(
            super::super::cpu_local::current_cpu_task_slot(),
            Some(actor),
            "exec image changed original Current"
        );
        assert!(stack_resources::cpu_leases::owns_current(cpu, actor));
        assert_eq!(self.current, actor == self.slot);
        self.writer.exec_stack_top(self.slot, self.task_id);
        runqueue::is_idle_slot(actor)
    }

    pub(in crate::multitask) fn commit_image(
        &self,
        bootstrap: &mut UserTaskBootstrap,
        by_pid: Option<(u64, u64)>,
    ) -> Option<ExecPublication> {
        #[cfg(not(test))]
        let turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
        #[cfg(test)]
        let turn = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        super::super::cpu_local::with_exec_image_turn(turn, self, |turn| {
            publication::commit(self, turn, bootstrap, by_pid)
        })
        .flatten()
    }
}
#[cfg(test)]
mod tests;
