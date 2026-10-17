//! Owner: exec census versus actual physical resource reclaim admission.
//! Boundary: Class55 membership fencing, NOT a fresh execution/MM/allocation loan.
//! Lifecycle: Resource53 checked nonce -> member reservation -> physical unregister.
//! Concurrency: Identity51 -> Resource53 -> ProcessPlacement55; never reverse.
//! Failure: census-first refuses reclaim without nonce/close; reclaim-first refuses census.
//! Forbidden: scalar slot/UID/context grants, callbacks, allocation or lower leaves under55.
//! Evidence: actual born resource/census witnesses; not complete terminal or hardware proof.
use super::*;
use crate::multitask::scheduler::stack_resources::{Membership, ReclaimAdmission};

pub(in crate::multitask) fn reserve(admission: &ReclaimAdmission<'_>) -> Option<()> {
    let Some((handle, slot, id)) = admission.member().binding() else {
        // Kernel/unpublished resources never appear in a process exec census.
        return Some(());
    };
    cell(handle)?.with(|state| {
        if state.handle != Some(handle) {
            return Some(());
        }
        let Some(index) = state
            .members
            .iter()
            .position(|&row| row == Some((slot, id)))
        else {
            // Logical removal outside EXEC already excluded this UID from every new census.
            return Some(());
        };
        if state.phase == 4 || state.terminal.is_some() || state.reclaiming[index].is_some() {
            return None;
        }
        state.reclaiming[index] = Some(admission.nonce());
        Some(())
    })
}

/// Compatibility close probe owns no reservation. The real Reuse producer MUST
/// still reserve in the same Class55 turn that excludes census publication.
pub(in crate::multitask) fn busy(member: &Membership<'_>) -> bool {
    let Some((handle, slot, id)) = member.binding() else {
        return false;
    };
    cell(handle).is_some_and(|cell| {
        cell.with(|state| {
            state.handle == Some(handle)
                && (state.phase == 4 || state.terminal.is_some())
                && state.members.contains(&Some((slot, id)))
        })
    })
}

/// Called inside the ORIGINAL resource leaf before clearing context/process/MM.
/// Logical unregister cannot grant physical destruction of a frozen census row.
pub(in crate::multitask) fn unregister(member: &Membership<'_>) {
    let Some((handle, slot, id)) = member.binding() else {
        return;
    };
    let Some(cell) = cell(handle) else {
        return;
    };
    cell.with(|state| {
        if state.handle != Some(handle) {
            return;
        }
        if let Some(index) = state
            .members
            .iter()
            .position(|&row| row == Some((slot, id)))
        {
            assert!(
                state.phase != 4 && state.terminal.is_none(),
                "physical detach with frozen exec/terminal census custody"
            );
            assert_eq!(
                state.reclaiming[index],
                member.reclaim_nonce(),
                "physical detach lacks original resource reclaim nonce"
            );
            state.unregister_member(index);
        }
        state.release_empty();
    });
}
