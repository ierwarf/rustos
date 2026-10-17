//! Exact prepublication thread-slot custody, independent of Scheduler storage.
//! No resource leaf crosses allocation, process attachment or initialization.
use super::{Geometry, RESOURCES, Resources, StackPin};
use crate::multitask::scheduler::PublishedFrameView;
use core::sync::atomic::Ordering;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Reserved,
    Committing,
    Canceling,
    Poisoned,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Reservation {
    task_id: u64,
    generation: u64,
    phase: Phase,
}
/// Non-Copy evidence of the actual exact identity/resource final publication.
/// Only Ticket::finish_commit constructs it, after its real reservation release.
pub(in crate::multitask) struct Publication {
    slot: usize,
    task_id: u64,
    generation: u64,
}
impl Publication {
    pub(in crate::multitask) fn matches(&self, member: (usize, u64), generation: u64) -> bool {
        (self.slot, self.task_id) == member && self.generation == generation
    }
}

pub(in crate::multitask::scheduler) struct Ticket {
    slot: usize,
    task_id: u64,
    generation: u64,
    live: bool,
}
fn matches(resources: &Resources, ticket: &Ticket, phase: Phase) -> bool {
    resources.reservation
        == Some(Reservation {
            task_id: ticket.task_id,
            generation: ticket.generation,
            phase,
        })
}
/// Isolated Local/Direct descriptor witness only, never a production ACK.
#[cfg(test)]
pub(super) fn set_phase_for_test(slot: usize, id: u64, phase: Option<Phase>) {
    RESOURCES[slot].with(|resources| {
        assert!(
            resources
                .reservation
                .is_none_or(|r| r.task_id == id && r.generation == 43)
        );
        resources.reservation = phase.map(|phase| Reservation {
            task_id: id,
            generation: 43,
            phase,
        });
    });
}

pub(in crate::multitask::scheduler) fn is_reserved(slot: usize) -> bool {
    RESOURCES
        .get(slot)
        .is_none_or(|cell| cell.with(|resources| resources.reservation.is_some()))
}

pub(in crate::multitask) fn wake_allowed(slot: usize) -> bool {
    !is_reserved(slot)
        && super::context::read(slot)
            .and_then(|c| c.process_handle)
            .is_none_or(|handle| {
                !crate::multitask::process_placement::root_membership::pending(handle)
            })
}
pub(in crate::multitask::scheduler) fn reserve(slot: usize, task_id: u64) -> Option<Ticket> {
    let cell = RESOURCES.get(slot)?;
    cell.with(|resources| {
        if resources.reservation.is_some()
            || resources.reclaim.is_some()
            || resources.closing
            || resources.mm_pin.is_some()
            || resources.frame_readers != 0
            || resources.context.is_some()
            || resources.process_pin.is_some()
            || resources.start.is_some()
            || resources.stack.is_some()
            || resources.binding.is_some()
            || resources.frame_writing
            || crate::multitask::current_identity::execution::count(slot) != 0
            || crate::multitask::current_identity::exec_intent::busy(slot)
            || cell.pins.load(Ordering::Acquire) != 0
        {
            return None;
        }
        let generation = resources.reservation_generation.checked_add(1)?;
        resources.reservation_generation = generation;
        resources.reservation = Some(Reservation {
            task_id,
            generation,
            phase: Phase::Reserved,
        });
        Some(Ticket {
            slot,
            task_id,
            generation,
            live: true,
        })
    })
}
impl Ticket {
    pub(in crate::multitask::scheduler) fn slot(&self) -> usize {
        self.slot
    }
    pub(in crate::multitask::scheduler) fn is_committing(&self) -> bool {
        self.live
            && RESOURCES[self.slot].with(|resources| matches(resources, self, Phase::Committing))
    }

    pub(in crate::multitask::scheduler) fn is_reserved_current(&self) -> bool {
        self.live && RESOURCES[self.slot].with(|r| matches(r, self, Phase::Reserved))
    }

    pub(in crate::multitask::scheduler) fn task_id(&self) -> u64 {
        self.task_id
    }

    pub(in crate::multitask::scheduler) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::multitask::scheduler) fn is_current(&self, slot: usize, task_id: u64) -> bool {
        self.live
            && self.slot == slot
            && self.task_id == task_id
            && RESOURCES[self.slot].with(|resources| matches(resources, self, Phase::Reserved))
    }

    #[cfg(test)]
    pub(super) fn set_phase_for_test(&self, phase: Phase) {
        RESOURCES[self.slot].with(|r| {
            let reservation = r
                .reservation
                .as_mut()
                .expect("native recovery lost producer");
            assert_eq!(reservation.task_id, self.task_id);
            assert_eq!(reservation.generation, self.generation);
            reservation.phase = phase;
        });
    }

    /// Called only under Resources53; the borrowed producer cannot be spent
    /// while its lexical constructor writer is active.
    pub(super) fn admits_image(&self, slot: usize, resources: &Resources) -> bool {
        self.live
            && self.slot == slot
            && (matches(resources, self, Phase::Reserved)
                || matches(resources, self, Phase::Committing))
    }

    pub(in crate::multitask::scheduler) fn initialize_storage(&self, f: impl FnOnce(&mut [u8])) {
        super::frame_access::initialize_reserved_storage(self.slot, self, f);
    }

    pub(in crate::multitask::scheduler) fn with_image<R>(
        &self,
        f: impl FnOnce(&super::frame_access::FrameWriter) -> R,
    ) -> R {
        super::frame_access::with_reserved_image(self.slot, self, f)
    }
    pub(in crate::multitask::scheduler) fn install_storage(&self, storage: alloc::vec::Vec<u8>) {
        super::install_storage_with_reservation(
            self.slot,
            storage,
            Some(Reservation {
                task_id: self.task_id,
                generation: self.generation,
                phase: Phase::Reserved,
            }),
        );
    }
    pub(in crate::multitask::scheduler) fn install_context(
        &self,
        context: super::super::TaskContext,
    ) {
        assert!(
            context.scheduling_context.is_bound_to(self.task_id),
            "foreign reserved context identity"
        );
        super::context::install_with_reservation(
            self.slot,
            context,
            Some(Reservation {
                task_id: self.task_id,
                generation: self.generation,
                phase: Phase::Committing,
            }),
        );
    }
    pub(in crate::multitask::scheduler) fn publish_start(&self, start: super::super::TaskStart) {
        assert_eq!(
            start.id, self.task_id,
            "foreign reserved descriptor identity"
        );
        super::task_start::publish_with_reservation(
            self.slot,
            start,
            Some(Reservation {
                task_id: self.task_id,
                generation: self.generation,
                phase: Phase::Committing,
            }),
        );
    }

    fn transition(&self, from: Phase, to: Phase) -> bool {
        RESOURCES[self.slot].with(|resources| {
            if !self.live || !matches(resources, self, from) {
                return false;
            }
            resources.reservation.as_mut().unwrap().phase = to;
            true
        })
    }
    pub(in crate::multitask::scheduler) fn begin_commit(&self) {
        assert!(
            self.transition(Phase::Reserved, Phase::Committing),
            "foreign thread reservation commit"
        );
    }
    pub(in crate::multitask::scheduler) fn begin_cancel(&self) -> bool {
        self.transition(Phase::Reserved, Phase::Canceling)
    }
    pub(in crate::multitask::scheduler) fn take_storage(&self) -> Option<alloc::vec::Vec<u8>> {
        super::take_storage_with_reservation(
            self.slot,
            Some(Reservation {
                task_id: self.task_id,
                generation: self.generation,
                phase: Phase::Canceling,
            }),
            None,
        )
    }
    /// Transfer physical backing before Slot ACK. This is not continuation,
    /// dispatch, membership or retirement permission; consumers still qualify
    /// the exact published UID and queue state while this loan prevents reuse.
    pub(in crate::multitask::scheduler) fn retain_publication(
        &self,
        view: PublishedFrameView,
    ) -> StackPin {
        RESOURCES[self.slot].with(|resources| {
            assert!(
                self.live && matches(resources, self, Phase::Committing),
                "publication backing lacks exact committing producer"
            );
            assert!(
                !resources.closing
                    && !resources.frame_writing
                    && resources
                        .start
                        .is_some_and(|start| start.id == self.task_id)
                    && resources.binding == Some((self.task_id, Geometry::from_view(view)))
                    && resources.context.is_some_and(|context| context
                        .scheduling_context
                        .is_bound_to(self.task_id))
                    && resources.mm_pin.as_ref().is_none_or(|pin| pin.is_open()),
                "publication backing lacks complete original physical binding"
            );
            // ORDERING: the exact producer/resource leaf serializes increments;
            // acquire/release retains backing before reservation removal admits readers.
            RESOURCES[self.slot]
                .pins
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_add(1)
                })
                .expect("publication bank pin overflow");
        });
        StackPin { slot: self.slot }
    }

    pub(in crate::multitask::scheduler) fn finish_commit(mut self) -> Publication {
        assert_eq!(
            super::super::runqueue::owner(self.slot).state,
            super::super::runqueue::RunOwnerState::Blocked,
            "thread reservation released before blocked admission"
        );
        // Identity51 -> Resources53 joins the exact UID to the one release.
        // Actual claims and cold binding acquisition share these owners.
        crate::multitask::current_identity::with_exact_publication(self.slot, self.task_id, || {
            RESOURCES[self.slot].with(|resources| {
                assert!(
                    matches(resources, &self, Phase::Committing),
                    "foreign thread reservation publication"
                );
                assert!(
                    resources.context.is_some_and(|context| context
                        .scheduling_context
                        .is_bound_to(self.task_id))
                        && resources
                            .start
                            .is_some_and(|start| start.id == self.task_id)
                        && resources.binding.is_some_and(|(id, _)| id == self.task_id)
                        && !resources.frame_writing
                        && !resources.closing,
                    "incomplete thread reservation publication"
                );
                resources.reservation = None;
            });
        })
        .expect("thread reservation released before exact identity publication");
        self.live = false;
        Publication {
            slot: self.slot,
            task_id: self.task_id,
            generation: self.generation,
        }
    }
    pub(in crate::multitask::scheduler) fn finish_cancel(mut self) {
        RESOURCES[self.slot].with(|resources| {
            assert!(
                matches(resources, &self, Phase::Canceling),
                "foreign thread reservation cancellation"
            );
            assert!(
                resources.context.is_none()
                    && resources.process_pin.is_none()
                    && resources.start.is_none()
                    && resources.binding.is_none()
                    && resources.stack.is_none()
                    && !resources.frame_writing
                    && RESOURCES[self.slot].pins.load(Ordering::Acquire) == 0,
                "thread reservation canceled before release"
            );
            resources.reservation = None;
        });
        self.live = false;
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        RESOURCES[self.slot].with(|resources| {
            // Abandonment is not rollback. Never reopen a partially initialized
            // slot; a stale ticket also cannot poison its replacement owner.
            if let Some(reservation) = resources.reservation.as_mut() {
                if (reservation.task_id, reservation.generation) == (self.task_id, self.generation)
                {
                    reservation.phase = Phase::Poisoned;
                }
            }
        });
    }
}
#[cfg(test)]
mod tests;
