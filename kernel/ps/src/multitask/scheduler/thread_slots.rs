//! Transactional user-thread scheduler-slot reservation.
//!
//! Clone allocates stack/process membership first, publishes no runnable task
//! until all faultable userspace copyout has committed, and can cancel the
//! exact reservation without leaving a partially visible scheduler slot.

use super::*;

/// Physical MM capture is not commit permission: close admission precedes bytes.
pub(in crate::multitask::scheduler) fn capture_commit_mm(
    mm: Option<&crate::multitask::process_table::MmExecutionPin>,
) -> Option<Option<crate::multitask::process_table::MmActivationPin>> {
    match mm {
        Some(mm) => Some(Some(mm.try_activate()?)),
        None => {
            #[cfg(not(test))]
            {
                None
            }
            #[cfg(test)]
            {
                Some(None)
            }
        }
    }
}

/// Captured publication grant: binding membership plus old-MM close exclusion.
/// Slot Ticket remains a separate producer; never a CPU or generic image grant.
pub(super) struct CommitGrant {
    key: (usize, u64, ProcessHandle, usize, u64, u64, Option<u64>),
    join: super::super::process_placement::Join,
    _activation: crate::multitask::process_table::MmActivationPin,
}
impl CommitGrant {
    pub(super) fn capture_ticket(
        ticket: &stack_resources::admission::Ticket,
        handle: ProcessHandle,
        parent: (usize, u64),
        root: u64,
        abi: UserAbi,
        mm: &crate::multitask::process_table::MmExecutionPin,
    ) -> Option<Self> {
        if !ticket.is_reserved_current() || !mm.matches(handle, root) {
            return None;
        }
        let join = super::super::process_placement::Join::begin(handle, parent, abi)?;
        let activation = capture_commit_mm(Some(mm))??;
        let pid = stack_resources::context::read(parent.0)?.process_id?;
        let current_mm = process_table::pin_process_mm(handle, pid)?;
        if !activation.matches(&current_mm) {
            return None;
        }
        Some(Self {
            key: (
                ticket.slot(),
                ticket.task_id(),
                handle,
                parent.0,
                parent.1,
                root,
                Some(ticket.generation()),
            ),
            join,
            _activation: activation,
        })
    }
    pub(super) fn mask(&self) -> u64 {
        self.join.mask()
    }
    pub(super) fn register(&self, ticket: &stack_resources::admission::Ticket) {
        assert!(
            self.matches_ticket(ticket),
            "membership lost exact publication ticket"
        );
        self.join.register(ticket.slot(), ticket.task_id())
            .expect("thread publication lost captured process membership");
    }

    pub(super) fn matches_ticket(&self, ticket: &stack_resources::admission::Ticket) -> bool {
        self.key.0 == ticket.slot()
            && self.key.1 == ticket.task_id()
            && self.key.6 == Some(ticket.generation())
            && (ticket.is_reserved_current() || ticket.is_committing())
    }
}


#[cfg(test)]
mod tests;
