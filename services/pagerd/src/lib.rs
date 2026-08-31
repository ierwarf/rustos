#![no_std]

mod page_cache;

pub use page_cache::{
    CacheAdmission, CacheError, CachePage, PageCache, PageCacheKey, PAGER_MAX_CACHE_ENTRIES,
};

use rustos_user_abi::pager::{
    pager_fault_token_generation, pager_fault_token_slot, PagerFaultDispatchWire,
    PagerFaultReplyWire, PagerFaultRequestWire, PagerVmRegionWire, PAGER_ACTION_DENY,
    PAGER_ACTION_MAP_ZEROED, PAGER_FAULT_ABI_VERSION, PAGER_MAX_FAULT_SLOTS,
    PAGER_MAX_TRACKED_REGIONS, VM_ACCESS_EXECUTE, VM_ACCESS_READ, VM_ACCESS_WRITE, VM_FAULT_COW,
    VM_FAULT_PRESENT, VM_FAULT_PROTECTION, VM_OBJECT_ANONYMOUS, VM_PROT_EXECUTE, VM_PROT_READ,
    VM_PROT_WRITE,
};

/// Live regions this pager tracks, sized by the shared ABI against ring0's
/// per-process VMA capacity so one process cannot wedge the others.
pub const PAGER_MAX_REGIONS: usize = PAGER_MAX_TRACKED_REGIONS;
/// Exact one-shot replay state, one entry per ring0 fault slot.
///
/// This replaces an append-only list of consumed tokens. That list could only
/// grow, so once it filled every later fault was refused with `Pressure` and
/// the faulting task never resumed. Fault tokens carry a strictly increasing
/// per-slot generation, so remembering the highest generation accepted for
/// each slot is both exact and fixed-size.
pub const PAGER_MAX_CONSUMED_TOKENS: usize = PAGER_MAX_FAULT_SLOTS;
pub const PAGER_DISPOSITION_ILLEGAL_ACCESS: u64 = 1;
pub const PAGER_DISPOSITION_NOT_DEMAND: u64 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerFaultError {
    Malformed,
    Stale,
    NotManaged,
    Pressure,
    EpochExhausted,
}

pub struct PagerState {
    epoch: u64,
    regions: [Option<PagerVmRegionWire>; PAGER_MAX_REGIONS],
    /// Highest fault-token generation accepted for each ring0 slot. Index is
    /// the token's one-based slot minus one; zero means the slot is unused.
    accepted_generations: [u64; PAGER_MAX_CONSUMED_TOKENS],
}

impl PagerState {
    pub const fn new(epoch: u64) -> Self {
        Self {
            epoch,
            regions: [None; PAGER_MAX_REGIONS],
            accepted_generations: [0; PAGER_MAX_CONSUMED_TOKENS],
        }
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn admit_region(&mut self, region: PagerVmRegionWire) -> Result<(), PagerFaultError> {
        if !region.is_canonical() || region.object.pager_epoch != self.epoch {
            return Err(PagerFaultError::Malformed);
        }
        if self.regions.iter().flatten().any(|current| {
            current.process_handle == region.process_handle
                && current.start < region.end
                && region.start < current.end
        }) {
            return Err(PagerFaultError::Stale);
        }
        let slot = self
            .regions
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(PagerFaultError::Pressure)?;
        *slot = Some(region);
        Ok(())
    }

    /// Releases tracking for one exact ring0-stamped range.
    ///
    /// Ring0 frees its own VMA slot on unmap and then sends this. Without it a
    /// dead region would live forever: it would refuse to re-admit the same
    /// range as an overlap, and the fixed table would fill and start refusing
    /// every admission, which downgrades demand paging to eager mapping.
    pub fn release_range(
        &mut self,
        release: rustos_user_abi::pager::PagerReleaseRangeWire,
    ) -> Result<usize, PagerFaultError> {
        if !release.is_canonical() {
            return Err(PagerFaultError::Malformed);
        }
        let mut released = 0;
        for slot in &mut self.regions {
            let matches = slot.is_some_and(|region| {
                region.process_handle == release.process_handle
                    && region.process_generation == release.process_generation
                    && region.start < release.end
                    && release.start < region.end
            });
            if matches {
                *slot = None;
                released += 1;
            }
        }
        Ok(released)
    }

    pub fn invalidate_process(&mut self, process_handle: u64) -> usize {
        let mut removed = 0;
        for slot in &mut self.regions {
            if slot.is_some_and(|region| region.process_handle == process_handle) {
                *slot = None;
                removed += 1;
            }
        }
        removed
    }

    pub fn restart(&mut self) -> Result<u64, PagerFaultError> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .filter(|epoch| *epoch != 0)
            .ok_or(PagerFaultError::EpochExhausted)?;
        self.regions.fill(None);
        self.accepted_generations.fill(0);
        Ok(self.epoch)
    }

    pub fn resolve_anonymous_first_touch(
        &mut self,
        dispatch: PagerFaultDispatchWire,
    ) -> Result<PagerFaultReplyWire, PagerFaultError> {
        if !dispatch.is_canonical() {
            return Err(PagerFaultError::Malformed);
        }
        let request = dispatch.request;
        if request.object.pager_epoch != self.epoch || !self.token_is_fresh(request.fault_token) {
            return Err(PagerFaultError::Stale);
        }
        let region = self
            .regions
            .iter()
            .flatten()
            .find(|region| fault_matches_region(request, **region))
            .copied()
            .ok_or(PagerFaultError::NotManaged)?;

        let mut reply = reply_envelope(request);
        if request.fault_flags & (VM_FAULT_PRESENT | VM_FAULT_PROTECTION | VM_FAULT_COW) != 0 {
            reply.action = PAGER_ACTION_DENY;
            reply.disposition = PAGER_DISPOSITION_NOT_DEMAND;
            self.consume(request.fault_token)?;
            return Ok(reply);
        }
        if !access_is_allowed(request.access, region.prot) {
            reply.action = PAGER_ACTION_DENY;
            reply.disposition = PAGER_DISPOSITION_ILLEGAL_ACCESS;
            self.consume(request.fault_token)?;
            return Ok(reply);
        }
        if region.object.object_type != VM_OBJECT_ANONYMOUS {
            return Err(PagerFaultError::NotManaged);
        }
        reply.action = PAGER_ACTION_MAP_ZEROED;
        reply.frame_rights = region.prot;
        reply.frame_capability = dispatch.zeroed_frame_capability;
        if !reply.is_canonical_zeroed_for(dispatch) {
            return Err(PagerFaultError::Malformed);
        }
        self.consume(request.fault_token)?;
        Ok(reply)
    }

    /// True when `token` names a live ring0 slot whose generation is newer
    /// than anything this pager has already answered for that slot.
    fn token_is_fresh(&self, token: u64) -> bool {
        match decode_token(token) {
            Some((index, generation)) => self
                .accepted_generations
                .get(index)
                .is_some_and(|accepted| generation > *accepted),
            None => false,
        }
    }

    /// Records one fault token as consumed, keyed by its ring0 slot.
    ///
    /// Generations increase strictly per slot, so keeping the highest one
    /// accepted gives exact one-shot semantics in fixed memory. The previous
    /// append-only token list could only grow; once full, every later fault
    /// was refused with `Pressure` and the faulting task never resumed.
    fn consume(&mut self, token: u64) -> Result<(), PagerFaultError> {
        let (index, generation) = decode_token(token).ok_or(PagerFaultError::Malformed)?;
        let accepted = self
            .accepted_generations
            .get_mut(index)
            .ok_or(PagerFaultError::Malformed)?;
        if generation <= *accepted {
            return Err(PagerFaultError::Stale);
        }
        *accepted = generation;
        Ok(())
    }
}

/// Splits a ring0 fault token into its zero-based slot index and generation.
///
/// Returns `None` for any token ring0 could not have minted, so a malformed
/// value can never alias a live slot's replay state.
fn decode_token(token: u64) -> Option<(usize, u64)> {
    let slot = pager_fault_token_slot(token)?;
    let generation = pager_fault_token_generation(token)?;
    Some((slot - 1, generation))
}

fn fault_matches_region(request: PagerFaultRequestWire, region: PagerVmRegionWire) -> bool {
    region.contains(request.virtual_address)
        && region.process_handle == request.process_handle
        && region.process_generation == request.process_generation
        && region.mm_generation == request.mm_generation
        && region.vma_generation == request.vma_generation
        && region.object == request.object
}

const fn access_is_allowed(access: u16, prot: u32) -> bool {
    match access {
        VM_ACCESS_READ => prot & VM_PROT_READ != 0,
        VM_ACCESS_WRITE => prot & VM_PROT_WRITE != 0,
        VM_ACCESS_EXECUTE => prot & VM_PROT_EXECUTE != 0,
        _ => false,
    }
}

/// Authenticates pager requests against the sender identity stamped by ring0.
///
/// Fault resolution is itself dispatched by ring0, whose receive-side identity
/// is exactly `(0, 0)`. User-originated operations retain the ordinary,
/// nonzero exact-subject rule.
pub fn request_sender_is_authorized(
    request: &rustos_user_abi::syscall::CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> bool {
    if request.header.op == rustos_user_abi::syscall::COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE {
        sender_pid == 0 && sender_tid == 0
    } else {
        request.subject_is_exact_sender(sender_pid, sender_tid)
    }
}

fn reply_envelope(request: PagerFaultRequestWire) -> PagerFaultReplyWire {
    PagerFaultReplyWire {
        version: PAGER_FAULT_ABI_VERSION,
        fault_token: request.fault_token,
        process_generation: request.process_generation,
        task_generation: request.task_generation,
        mm_generation: request.mm_generation,
        vma_generation: request.vma_generation,
        pager_epoch: request.object.pager_epoch,
        ..PagerFaultReplyWire::default()
    }
}

#[cfg(test)]
mod sender_auth_tests {
    use super::request_sender_is_authorized;
    use rustos_user_abi::syscall::{
        CommercialMaxProtocolRequest, COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT,
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE,
    };

    fn request(op: u16, subject_pid: u64, subject_tid: u64) -> CommercialMaxProtocolRequest {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.op = op;
        request.header.subject_pid = subject_pid;
        request.header.subject_tid = subject_tid;
        request
    }

    #[test]
    fn fault_resolve_requires_kernel_sender_identity() {
        let request = request(COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, 0, 0);
        assert!(request_sender_is_authorized(&request, 0, 0));
    }

    #[test]
    fn fault_resolve_rejects_user_sender_even_when_subject_is_spoofed_to_match() {
        let request = request(COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, 41, 43);
        assert!(!request_sender_is_authorized(&request, 41, 43));
    }

    #[test]
    fn backing_object_retains_exact_user_subject_rule() {
        let request = request(COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, 47, 53);
        assert!(request_sender_is_authorized(&request, 47, 53));
        assert!(!request_sender_is_authorized(&request, 0, 0));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::pager::{
        PagerEndpointCapabilityWire, PagerFaultDispatchWire, PagerObjectIdentityWire,
        PAGER_FAULT_TOKEN_SLOT_BITS, VM_OBJECT_ANONYMOUS, VM_SHARING_PRIVATE,
    };

    fn region(epoch: u64) -> PagerVmRegionWire {
        PagerVmRegionWire {
            start: 0x4000,
            end: 0x8000,
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_ANONYMOUS,
                backing_service: 0,
                rights: VM_PROT_READ | VM_PROT_WRITE,
                slot: 3,
                generation: 5,
                pager_epoch: epoch,
                backing_generation: 7,
                ..PagerObjectIdentityWire::default()
            },
            prot: VM_PROT_READ | VM_PROT_WRITE,
            sharing: VM_SHARING_PRIVATE,
            vma_generation: 11,
            process_handle: 13,
            process_generation: 17,
            mm_generation: 19,
            fault_endpoint: PagerEndpointCapabilityWire {
                slot: 23,
                generation: epoch,
                rights: 1,
            },
            ..PagerVmRegionWire::default()
        }
    }

    /// Builds a token in the exact shape ring0 mints, so tests cannot pass
    /// with values the kernel could never produce.
    fn token(slot: u64, generation: u64) -> u64 {
        (generation << PAGER_FAULT_TOKEN_SLOT_BITS) | slot
    }

    fn request(region: PagerVmRegionWire, token: u64) -> PagerFaultRequestWire {
        PagerFaultRequestWire {
            version: PAGER_FAULT_ABI_VERSION,
            access: VM_ACCESS_WRITE,
            fault_token: token,
            process_handle: region.process_handle,
            process_generation: region.process_generation,
            task_id: 29,
            task_generation: 31,
            mm_generation: region.mm_generation,
            vma_generation: region.vma_generation,
            virtual_address: region.start,
            deadline_ns: 37,
            scheduling_domain: 41,
            charge_token: 43,
            object: region.object,
            ..PagerFaultRequestWire::default()
        }
    }

    fn dispatch(
        request: PagerFaultRequestWire,
        zeroed_frame_capability: u64,
        granted_frame_rights: u32,
    ) -> PagerFaultDispatchWire {
        PagerFaultDispatchWire {
            request,
            zeroed_frame_capability,
            granted_frame_rights,
            ..PagerFaultDispatchWire::default()
        }
    }

    #[test]
    fn anonymous_first_touch_is_zero_fill_and_one_shot() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        let request = request(region, token(1, 47));
        let fault = dispatch(request, 53, region.prot);
        let reply = pager.resolve_anonymous_first_touch(fault).unwrap();
        assert_eq!(reply.action, PAGER_ACTION_MAP_ZEROED);
        assert!(reply.is_canonical_zeroed_for(fault));
        assert_eq!(
            pager.resolve_anonymous_first_touch(dispatch(request, 59, region.prot)),
            Err(PagerFaultError::Stale)
        );
    }

    #[test]
    fn fault_resolution_is_not_capped_by_consumed_token_capacity() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        // A long-running system cycles the fixed ring0 slot space many times
        // over. The previous append-only consumed-token list stopped serving
        // faults for good once it filled, which stalled every faulting task.
        let mut resolved = 0_usize;
        for generation in 1..=8_u64 {
            for slot in 1..=PAGER_MAX_FAULT_SLOTS as u64 {
                let fault = dispatch(request(region, token(slot, generation)), 53, region.prot);
                assert_eq!(
                    pager
                        .resolve_anonymous_first_touch(fault)
                        .expect("slot reuse must keep resolving")
                        .action,
                    PAGER_ACTION_MAP_ZEROED
                );
                resolved += 1;
            }
        }
        assert_eq!(resolved, 8 * PAGER_MAX_FAULT_SLOTS);
    }

    #[test]
    fn same_slot_rejects_replayed_and_older_generations() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        let resolve = |pager: &mut PagerState, slot, generation| {
            pager.resolve_anonymous_first_touch(dispatch(
                request(region, token(slot, generation)),
                53,
                region.prot,
            ))
        };
        assert!(resolve(&mut pager, 7, 5).is_ok());
        // Replay of the exact token, and any older generation on that slot,
        // must both fail closed; a newer generation is still admitted.
        assert_eq!(resolve(&mut pager, 7, 5), Err(PagerFaultError::Stale));
        assert_eq!(resolve(&mut pager, 7, 4), Err(PagerFaultError::Stale));
        assert!(resolve(&mut pager, 7, 6).is_ok());
        // A different slot keeps its own independent generation ladder.
        assert!(resolve(&mut pager, 8, 1).is_ok());
    }

    #[test]
    fn malformed_tokens_never_alias_a_live_slot() {
        assert_eq!(decode_token(0), None);
        // Generation zero: ring0 never mints it.
        assert_eq!(decode_token(5), None);
        // Slot zero, and a slot past the fixed table.
        assert_eq!(decode_token(token(0, 3)), None);
        assert_eq!(
            decode_token(token(PAGER_MAX_FAULT_SLOTS as u64 + 1, 3)),
            None
        );
        assert_eq!(decode_token(token(1, 3)), Some((0, 3)));
    }

    fn release_of(region: PagerVmRegionWire) -> rustos_user_abi::pager::PagerReleaseRangeWire {
        rustos_user_abi::pager::PagerReleaseRangeWire {
            version: PAGER_FAULT_ABI_VERSION,
            reserved0: [0; 3],
            process_handle: region.process_handle,
            process_generation: region.process_generation,
            start: region.start,
            end: region.end,
            reserved1: [0; 2],
        }
    }

    #[test]
    fn releasing_a_range_frees_its_slot_and_allows_re_admission() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        // Re-admitting a still-tracked range is an overlap, not a refresh.
        assert_eq!(pager.admit_region(region), Err(PagerFaultError::Stale));
        assert_eq!(pager.release_range(release_of(region)), Ok(1));
        // Once ring0 says the range died, the same range is admissible again.
        assert!(pager.admit_region(region).is_ok());
        // Releasing a range nobody tracks is a no-op, not an error.
        assert_eq!(pager.release_range(release_of(region)), Ok(1));
        assert_eq!(pager.release_range(release_of(region)), Ok(0));
    }

    #[test]
    fn admission_capacity_is_reclaimed_rather_than_leaked() {
        let mut pager = PagerState::new(2);
        let base = region(2);
        // Cycle far more ranges than the table holds. Without a release path
        // the table filled permanently and every later admission was refused,
        // which silently downgraded demand paging to eager mapping.
        for round in 0..4 {
            for slot in 0..PAGER_MAX_REGIONS as u64 {
                let start = 0x4000 + (slot + 1) * 0x10_000;
                let region = PagerVmRegionWire {
                    start,
                    end: start + 0x4000,
                    ..base
                };
                assert!(
                    pager.admit_region(region).is_ok(),
                    "round {round} slot {slot} must be admissible"
                );
                assert_eq!(pager.release_range(release_of(region)), Ok(1));
            }
        }
    }

    #[test]
    fn release_rejects_a_malformed_or_foreign_range() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        let mut malformed = release_of(region);
        malformed.version = 0;
        assert_eq!(
            pager.release_range(malformed),
            Err(PagerFaultError::Malformed)
        );
        // A different process generation must not free this process's region.
        let mut foreign = release_of(region);
        foreign.process_generation = region.process_generation + 1;
        assert_eq!(pager.release_range(foreign), Ok(0));
        assert_eq!(pager.admit_region(region), Err(PagerFaultError::Stale));
    }

    #[test]
    fn exec_unmap_and_exit_invalidation_reject_stale_faults() {
        let mut pager = PagerState::new(3);
        let region = region(3);
        pager.admit_region(region).unwrap();
        assert_eq!(pager.invalidate_process(region.process_handle), 1);
        assert_eq!(
            pager.resolve_anonymous_first_touch(dispatch(
                request(region, token(2, 61)),
                67,
                region.prot
            )),
            Err(PagerFaultError::NotManaged)
        );
    }

    #[test]
    fn restart_advances_epoch_and_old_reply_authority_dies() {
        let mut pager = PagerState::new(5);
        let old = region(5);
        pager.admit_region(old).unwrap();
        let old_request = request(old, token(4, 71));
        assert_eq!(pager.restart(), Ok(6));
        assert_eq!(
            pager.resolve_anonymous_first_touch(dispatch(old_request, 73, old.prot)),
            Err(PagerFaultError::Stale)
        );
        let new = region(6);
        pager.admit_region(new).unwrap();
        assert_eq!(
            pager
                .resolve_anonymous_first_touch(dispatch(request(new, token(5, 79)), 83, new.prot))
                .unwrap()
                .action,
            PAGER_ACTION_MAP_ZEROED
        );
    }

    #[test]
    fn permission_and_protection_faults_have_explicit_abi_dispositions() {
        let mut pager = PagerState::new(7);
        let region = region(7);
        pager.admit_region(region).unwrap();
        let protection = PagerFaultRequestWire {
            fault_flags: VM_FAULT_PROTECTION,
            ..request(region, token(3, 89))
        };
        let reply = pager
            .resolve_anonymous_first_touch(dispatch(protection, 97, region.prot))
            .unwrap();
        assert_eq!(reply.action, PAGER_ACTION_DENY);
        assert_eq!(reply.disposition, PAGER_DISPOSITION_NOT_DEMAND);
        assert!(reply.is_canonical_for(protection));
    }
}
