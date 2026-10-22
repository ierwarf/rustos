//! Owner: exact dynamic kernel/user-root production and reserved user-thread storage.
//! Boundary: a slot ticket is not boot/AP idle, activation, policy or CPU permission.
//! Lifecycle: reserve before bytes; blocked publication precedes producer ACK.
//! Concurrency: Resources53 ticket excludes foreign creators/claims/teardown;
//! directory serialization still owns the enclosing process/lifecycle transaction.
//! Failure: pre-publication cancellation drains storage; partial commit poisons.
//! Forbidden: allocation/byte initialization/destruction inside resource leaves.
//! Evidence: real ticket/native witnesses, not complete creation or return proof.
use super::{
    STACK_CANARY_WORD, TASK_STACK_GUARD_BYTES, TASK_STACK_SIZE, runqueue,
    stack_resources::admission::{self, Ticket},
};
use alloc::vec::Vec;
use core::mem;
pub(super) fn prepare_storage(ticket: &Ticket) -> Option<()> {
    assert!(
        ticket.is_current(ticket.slot(), ticket.task_id()),
        "foreign user storage reservation"
    );
    let mut storage = Vec::new();
    storage.try_reserve_exact(TASK_STACK_SIZE).ok()?;
    storage.resize(TASK_STACK_SIZE, 0);
    ticket.install_storage(storage);
    ticket.initialize_storage(|storage| {
        storage.fill(0);
        for word in storage[..TASK_STACK_GUARD_BYTES].chunks_exact_mut(mem::size_of::<u64>()) {
            word.copy_from_slice(&STACK_CANARY_WORD.to_ne_bytes());
        }
    });
    Some(())
}
pub(super) fn reserve(slot: usize, id: u64) -> Option<Ticket> {
    let ticket = admission::reserve(slot, id)?;
    if prepare_storage(&ticket).is_none() {
        cancel_unpublished(ticket);
        return None;
    }
    Some(ticket)
}
pub(super) fn cancel_unpublished(ticket: Ticket) {
    assert!(
        ticket.begin_cancel(),
        "root cancellation lacks unpublished producer"
    );
    let storage = ticket.take_storage();
    drop(storage); // detach under Resources53, cross-owner destruction outside it
    ticket.finish_cancel();
}
/// Complete root publication carries both actual slot producer and process Join.
/// Neither visible metadata nor a bare membership row can finish this capsule.
pub(super) struct RootPublication {
    ticket: Ticket,
    join: crate::multitask::process_placement::root_membership::RootJoin,
    handle: crate::multitask::process_table::ProcessHandle,
}
impl RootPublication {
    #[cfg(test)]
    pub(super) fn begin(
        handle: crate::multitask::process_table::ProcessHandle,
        ticket: Ticket,
        mask: u64,
    ) -> Result<Self, Ticket> {
        if !ticket.is_reserved_current() {
            return Err(ticket);
        }
        let Some(join) = crate::multitask::process_placement::root_membership::RootJoin::begin(
            handle,
            (ticket.slot(), ticket.task_id()),
            ticket.generation(),
            mask,
        ) else {
            return Err(ticket);
        };
        Ok(Self {
            ticket,
            join,
            handle,
        })
    }
    pub(super) fn prepare(
        reservation: crate::multitask::process_table::SpawnReservation,
        ticket: Ticket,
        mask: u64,
    ) -> Result<Self, Ticket> {
        if !ticket.is_reserved_current() {
            return Err(ticket);
        }
        let Some(join) = crate::multitask::process_placement::root_membership::RootJoin::prepare(
            reservation,
            (ticket.slot(), ticket.task_id()),
            ticket.generation(),
            mask,
        ) else {
            return Err(ticket);
        };
        Ok(Self {
            ticket,
            join,
            handle: reservation.process_handle(),
        })
    }
    pub(super) fn publish_process(
        &mut self,
        parent: Option<u64>,
        state: crate::user::process_state::UserProcessState,
    ) -> Option<crate::multitask::process_table::ProcessHandle> {
        self.join.publish(parent, state)
    }
    pub(super) fn cancel(self) {
        let Self { ticket, join, .. } = self;
        let _cancelled = join
            .cancel()
            .expect("visible root cannot be unpublished rollback");
        // RootJoin atomically cancelled its process transaction and seal before
        // returning. A copied receipt cannot race this physical slot rollback.
        cancel_unpublished(ticket);
    }

    pub(super) fn ticket(&self) -> &Ticket {
        &self.ticket
    }
    pub(super) fn begin_commit(&self) {
        self.ticket.begin_commit();
    }
    pub(super) fn install_context(&self, context: super::TaskContext) {
        self.ticket.install_context(context);
    }
    pub(super) fn publish_start(&self, start: super::TaskStart) {
        self.ticket.publish_start(start);
    }
    #[cfg(test)]
    pub(super) fn finish(self) {
        let _ = self.finish_inner(None);
    }
    /// Acquire physical child custody before releasing both Slot and process
    /// ACKs; the caller retains this real loan through wake/notification.
    pub(super) fn finish_retaining(
        self,
        view: super::PublishedFrameView,
    ) -> super::stack_resources::StackPin {
        self.finish_inner(Some(view))
            .expect("root ACK lost publication backing")
    }
    fn finish_inner(
        mut self,
        view: Option<super::PublishedFrameView>,
    ) -> Option<super::stack_resources::StackPin> {
        let slot = self.ticket.slot();
        let id = self.ticket.task_id();
        assert!(
            self.ticket.is_committing(),
            "root membership ACK lacks actual committing ticket"
        );
        let context =
            super::stack_resources::context::read(slot).expect("root membership lacks context");
        assert!(
            context.user_mode
                && context.process_handle == Some(self.handle)
                && context.scheduling_context.is_bound_to(id),
            "root membership changed constructor process"
        );
        self.join
            .register()
            .expect("root membership lost exact constructor Join");
        runqueue::admit_blocked(slot);
        let backing = view.map(|view| {
            super::frame_publication::bind_local_owner_generation(
                slot,
                runqueue::owner(slot).generation,
            );
            self.ticket.retain_publication(view)
        });
        let publication = self.ticket.finish_commit();
        self.join.finish(publication);
        backing
    }
}

#[cfg(test)]
pub(super) fn admit_blocked(ticket: Ticket) -> Result<(), Ticket> {
    if !ticket.is_committing() {
        return Err(ticket);
    }
    runqueue::admit_blocked(ticket.slot());
    ticket.finish_commit();
    Ok(())
}
#[cfg(test)]
mod tests;
