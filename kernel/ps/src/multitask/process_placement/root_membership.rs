//! Owner: exact root-constructor process membership through actual slot publication ACK.
//! Boundary: existing ProcessPlacement55 and real process pin; no directory/CPU/MM grant.
//! Lifecycle: sealed process reservation -> pinned visibility -> Registered -> Slot ACK.
//! Concurrency: non-yielding constructor loan; census/terminal/change wait for joins to drain.
//! Failure: unpublished rollback exact; newly-visible or registered abandon keeps poisoned pin.
//! Forbidden: visible root prefix, copied UID or Blocked observation alone as constructor ACK.
//! Evidence: actual constructor receipt and row witnesses; hardware/user continuation separate.
use super::*;
use crate::multitask::scheduler::Publication;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Record {
    member: Member,
    generation: u64,
    nonce: u64,
    registered: bool,
}
/// Negative constructor exclusion only, never a grant from metadata absence.
/// Physical UID/context/MM and actual owner CAS still qualify independently.
/// Read under Identity51/Resources53 -> Placement55; root ACK takes no lower leaf.
pub(in crate::multitask) fn pending(handle: ProcessHandle) -> bool {
    cell(handle).is_none_or(|cell| cell.with(|s| s.root.is_some()))
}

pub(in crate::multitask) struct RootJoin {
    handle: ProcessHandle,
    key: Record,
    mask: u64,
    process: Option<process_table::pin_owner::ProcessObjectPin>,
    spawn_key: Option<process_table::spawn::RootSpawnLoan>,
    live: bool,
}
impl RootJoin {
    pub(in crate::multitask) fn begin(
        handle: ProcessHandle,
        member: Member,
        generation: u64,
        mask: u64,
    ) -> Option<Self> {
        if member.0 >= super::super::MAX_SCHEDULER_TASKS || generation == 0 || mask == 0 {
            return None;
        }
        let process = process_table::pin_owner::pin(handle)?;
        Self::claim(handle, member, generation, mask, Some(process))
    }
    /// Claim the checked process ledger nonce before fallible materialization
    /// or PID publication, then seal the exact reserved Class54 row outside55.
    pub(in crate::multitask) fn prepare(
        reservation: process_table::SpawnReservation,
        member: Member,
        generation: u64,
        mask: u64,
    ) -> Option<Self> {
        if member.0 >= super::super::MAX_SCHEDULER_TASKS || generation == 0 || mask == 0 {
            return None;
        }
        let mut join = Self::claim(reservation.process_handle(), member, generation, mask, None)?;
        join.spawn_key = Some(process_table::spawn::seal_root_spawn(
            reservation,
            member,
            generation,
            join.key.nonce,
        )?);
        Some(join)
    }
    fn claim(
        handle: ProcessHandle,
        member: Member,
        generation: u64,
        mask: u64,
        process: Option<process_table::pin_owner::ProcessObjectPin>,
    ) -> Option<Self> {
        let key = cell(handle)?.with(|s| {
            if s.handle.is_some_and(|h| h != handle)
                || s.phase != 0
                || s.joins != 0
                || s.root.is_some()
                || s.terminal.is_some()
                || s.exit.is_some()
                || s.members.iter().any(Option::is_some)
            {
                return None;
            }
            let nonce = s.root_nonce.checked_add(1)?;
            let key = Record {
                member,
                generation,
                nonce,
                registered: false,
            };
            s.root_nonce = nonce;
            s.root = Some(key);
            s.handle = Some(handle);
            s.mask = mask;
            s.joins = 1;
            Some(key)
        })?;
        Some(Self {
            handle,
            key,
            mask,
            process,
            spawn_key: None,
            live: true,
        })
    }
    /// The actual reserved table row and preclaimed root nonce, not a copied
    /// spawn receipt, consume publication. Pin acquisition precedes visibility.
    pub(in crate::multitask) fn publish(
        &mut self,
        parent: Option<u64>,
        state: crate::user::process_state::UserProcessState,
    ) -> Option<ProcessHandle> {
        if !self.live || self.process.is_some() || self.key.registered {
            return None;
        }
        let loan = self.spawn_key.as_mut()?;
        let process = process_table::spawn::publish_root_spawn(loan, parent, state)?;
        assert_eq!(
            process.handle(),
            self.handle,
            "root process changed original generation"
        );
        self.process = Some(process);
        Some(self.handle)
    }
    pub(in crate::multitask) fn cancel(self) -> Option<process_table::SpawnReservation> {
        if self.process.is_some() || self.key.registered {
            return None;
        }
        let reservation = self.spawn_key.as_ref()?.reservation();
        drop(self); // atomic Class54 transaction+seal cancellation precedes Class55 rollback
        Some(reservation)
    }

    fn exact(&self, s: &State) -> bool {
        self.live
            && s.handle == Some(self.handle)
            && s.root == Some(self.key)
            && s.root_nonce == self.key.nonce
            && s.mask == self.mask
            && s.joins == 1
    }
    pub(in crate::multitask) fn register(&mut self) -> Option<()> {
        if self.process.is_none() {
            return None;
        }
        cell(self.handle)?.with(|s| {
            if !self.exact(s)
                || self.key.registered
                || s.phase != 0
                || s.terminal.is_some()
                || s.exit.is_some_and(|e| !e.permits_captured_join())
            {
                return None;
            }
            s.insert(self.key.member)?;
            self.key.registered = true;
            s.root = Some(self.key);
            Some(())
        })
    }
    pub(in crate::multitask) fn finish(mut self, publication: Publication) {
        assert!(
            publication.matches(self.key.member, self.key.generation),
            "root Join lacks actual ticket publication receipt"
        );
        cell(self.handle).expect("root process row lost").with(|s| {
            assert!(
                self.exact(s) && self.key.registered && s.members.contains(&Some(self.key.member)),
                "root Join ACK lost exact registered constructor"
            );
            s.root = None;
            s.joins = 0;
        });
        self.live = false;
        // ProcessObjectPin drops outside ProcessPlacement55.
    }
}
impl Drop for RootJoin {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let mut retained = self.process.take();
        if retained.is_none()
            && let Some(loan) = self.spawn_key.as_mut()
        {
            assert!(
                process_table::spawn::release_root_spawn(loan),
                "unpublished root rollback lost actual process seal"
            );
        }
        // A newly visible process cannot be made ownerless by dropping its
        // unregistered producer. No Class54 leaf crosses ledger reset/poison.
        let visible_birth = self.spawn_key.is_some() && retained.is_some();
        cell(self.handle).expect("root process row lost").with(|s| {
            assert!(self.exact(s), "root Join rollback lost constructor");
            if self.key.registered || visible_birth {
                assert!(s.root_process.is_none());
                s.root_process = retained.take();
                s.phase = 2;
            } else {
                s.root = None;
                s.joins = 0;
                s.release_empty();
            }
        });
        drop(retained);
    }
}

/// Native quarantine recovery only. Not a constructor/reuse ACK or production reopening.
#[cfg(test)]
pub(in crate::multitask) fn recover_fixture(handle: ProcessHandle) {
    let retained = cell(handle).unwrap().with(|s| {
        assert!(s.handle == Some(handle) && s.phase == 2 && s.joins == 1);
        assert!(s.root.is_some() && s.terminal.is_none() && s.exit.is_none());
        s.root = None;
        s.joins = 0;
        s.phase = 0;
        s.members = [None; MAX];
        s.stopped = [false; MAX];
        s.exit_detached = [false; MAX];
        let pin = s
            .root_process
            .take()
            .expect("registered root poison lost actual process pin");
        s.release_empty();
        pin
    });
    drop(retained);
}

/// Native checked-epoch injection; called only with a quiescent empty row.
#[cfg(test)]
pub(in crate::multitask) fn set_nonce_fixture(handle: ProcessHandle, nonce: u64) -> u64 {
    cell(handle).unwrap().with(|s| {
        assert!(s.root.is_none() && s.root_process.is_none() && s.joins == 0 && s.phase == 0);
        assert!(s.members.iter().all(Option::is_none) && s.exit.is_none() && s.terminal.is_none());
        core::mem::replace(&mut s.root_nonce, nonce)
    })
}
