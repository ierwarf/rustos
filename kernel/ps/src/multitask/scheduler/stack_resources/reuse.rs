//! Owner: exact producer-last physical slot vacancy, separate from group exit policy.
//! Boundary: closes slot reuse through metadata reset and owned allocation detach.
//! Lifecycle: Closing -> Detached -> Complete; abandonment poisons occupancy.
//! Concurrency: Identity51 -> Resources53 seals UID/checked nonce before effects.
//! Failure: refusal does not spend nonce; old/duplicate receipts cannot reopen reuse.
//! Forbidden: absence of context/storage is not reset ACK; no external work in leaves.
//! Evidence: real resource/identity owner tests; not full terminal/MM/hardware refinement.
use super::*;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Closing,
    Detached,
    Poisoned,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Key {
    id: Option<u64>,
    nonce: u64,
    phase: Phase,
}
impl Key {
    pub(super) fn nonce(self) -> u64 { self.nonce }
}
pub(in crate::multitask::scheduler) struct Reuse {
    slot: usize,
    key: Key,
    live: bool,
}
#[cfg(test)]
pub(in crate::multitask::scheduler) fn begin(slot: usize, id: Option<u64>) -> Option<Reuse> {
    begin_qualified(slot, id, false)
}

pub(in crate::multitask::scheduler) fn begin_retired(slot: usize, id: u64) -> Option<Reuse> {
    begin_qualified(slot, Some(id), true)
}

/// Native recovery of an explicitly abandoned fully detached fixture only.
/// Never authorizes external completion or serves as a production reopen route.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn recover_detached_fixture(slot: usize, id: u64) {
    let key = RESOURCES[slot].with(|r| {
        let mut key = r.reclaim.expect("fixture lost poisoned resource seal");
        assert_eq!(key.id, Some(id));
        assert_eq!(key.phase, Phase::Poisoned);
        assert!(
            r.context.is_none()
                && r.start.is_none()
                && r.binding.is_none()
                && r.stack.is_none()
                && r.mm_pin.is_none()
                && r.process_pin.is_none()
                && r.reservation.is_none()
                && !r.frame_writing
                // ORDERING: Acquire pairs with lock-free reader release.
                && RESOURCES[slot].readers.load(Ordering::Acquire) == 0
        );
        key.phase = Phase::Detached;
        r.reclaim = Some(key);
        key
    });
    Reuse {
        slot,
        key,
        live: true,
    }
    .finish();
}

fn begin_qualified(slot: usize, id: Option<u64>, retired: bool) -> Option<Reuse> {
    let cell = RESOURCES.get(slot)?;
    let key = crate::multitask::current_identity::with_reclaim_binding_qualified(
        slot,
        id,
        retired,
        || {
            cell.with(|r| {
            if r.reclaim.is_some()
                || r.reservation.is_some()
                || r.frame_writing
                // ORDERING: Acquire pairs with lock-free reader release.
                || cell.readers.load(Ordering::Acquire) != 0
                // ORDERING: acquire observes pin release under the exact resource/identity admission turn.
                || cell.pins.load(Ordering::Acquire) != 0
                || r.binding.map(|(uid, _)| uid) != id
                || r.start.map(|start| start.id) != id
                || (retired && (r.context.is_none() || r.start.is_none()
                    || r.context.and_then(|c| c.process_handle).is_some_and(crate::multitask::process_placement::root_membership::pending)))
                || r.context.is_some_and(|context| {
                    id.is_none_or(|id| !context.scheduling_context.is_bound_to(id))
                })
                || (id.is_none()
                    && (r.context.is_some()
                        || r.stack.is_some()
                        || r.mm_pin.is_some()
                        || r.process_pin.is_some()))
            {
                return None;
            }
            let nonce = r.reservation_generation.checked_add(1)?;
            let admission = ReclaimAdmission {
                member: Membership { resources: r, slot },
                nonce,
            };
            crate::multitask::process_placement::reclaim::reserve(&admission)?;
            let key = Key {
                id,
                nonce,
                phase: Phase::Closing,
            };
            r.reservation_generation = nonce;
            r.reclaim = Some(key);
            r.closing = true;
            // ORDERING: close observation follows exact producer-last gate installation.
            cell.closing_notice.store(true, Ordering::Release);
            Some(key)
        })
        },
    )??;
    Some(Reuse {
        slot,
        key,
        live: true,
    })
}
impl Reuse {
    pub(in crate::multitask::scheduler) fn take_storage(&mut self) -> Option<Vec<u8>> {
        assert!(
            self.live && self.key.phase == Phase::Closing,
            "reclaim detachment lost exact producer"
        );
        let stack = super::take_storage_with_reservation(self.slot, None, Some(self.key));
        RESOURCES[self.slot].with(|r| {
            assert_eq!(r.reclaim, Some(self.key));
            self.key.phase = Phase::Detached;
            r.reclaim = Some(self.key);
        });
        stack
    }
    pub(in crate::multitask::scheduler) fn finish(mut self) {
        assert!(
            self.live && self.key.phase == Phase::Detached,
            "reclaim ACK preceded owned detachment"
        );
        crate::multitask::current_identity::clear_reclaimed(self.slot, self.key.id, || {
            RESOURCES[self.slot].with(|r| {
                r.reclaim == Some(self.key)
                    && r.context.is_none()
                    && r.start.is_none()
                    && r.binding.is_none()
                    && r.stack.is_none()
                    && r.mm_pin.is_none()
                    && r.process_pin.is_none()
                    && r.reservation.is_none()
                    && !r.frame_writing
                    // ORDERING: Acquire pairs with lock-free reader release.
                    && RESOURCES[self.slot].readers.load(Ordering::Acquire) == 0
            })
        })
        .expect("reclaim identity clear lacks exact drain receipt");
        crate::multitask::current_identity::with_reclaim_binding(self.slot, None, || {
            RESOURCES[self.slot].with(|r| {
                assert_eq!(r.reclaim, Some(self.key), "foreign reclaim ACK");
                assert!(
                    r.context.is_none()
                        && r.start.is_none()
                        && r.binding.is_none()
                        && r.stack.is_none()
                        && r.mm_pin.is_none()
                        && r.process_pin.is_none()
                        && r.reservation.is_none()
                        && !r.frame_writing
                        // ORDERING: Acquire pairs with lock-free reader release.
                        && RESOURCES[self.slot].readers.load(Ordering::Acquire) == 0
                        // ORDERING: acquire observes pin release under the exact resource/identity admission turn.
                        && RESOURCES[self.slot].pins.load(Ordering::Acquire) == 0,
                    "reclaim ACK preceded complete metadata drain"
                );
                r.reclaim = None;
                r.closing = false;
                // ORDERING: release reopen only after all exact metadata/binding reset effects.
                RESOURCES[self.slot]
                    .closing_notice
                    .store(false, Ordering::Release);
            })
        })
        .expect("reclaim ACK preceded exact identity drain");
        self.live = false;
    }
}
impl Drop for Reuse {
    fn drop(&mut self) {
        if self.live {
            RESOURCES[self.slot].with(|r| {
                if r.reclaim == Some(self.key) {
                    r.reclaim.as_mut().unwrap().phase = Phase::Poisoned;
                    r.closing = true;
                    // ORDERING: poisoned occupancy retains close observation for abandoned effects.
                    RESOURCES[self.slot]
                        .closing_notice
                        .store(true, Ordering::Release);
                }
            });
        }
    }
}
#[cfg(test)]
mod tests;
#[cfg(test)]
mod census_tests;
