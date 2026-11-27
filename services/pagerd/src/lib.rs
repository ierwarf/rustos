#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

mod page_cache;
pub mod library_custody;
pub mod library_warm;

pub use page_cache::{
    CacheAdmission, CacheError, CachePage, PageCache, PageCacheKey, PAGER_MAX_CACHE_ENTRIES,
};

use rustos_user_abi::pager::{
    apply_region_edit, pager_fault_token_generation, pager_fault_token_slot,
    PagerFaultDispatchWire, PagerFaultReplyWire, PagerFaultRequestWire, PagerRangeEdit,
    PagerRegionEdit, PagerVmRegionWire, PAGER_ACTION_DENY, PAGER_ACTION_MAP_ZEROED,
    PAGER_FAULT_ABI_VERSION, PAGER_MAX_FAULT_SLOTS, PAGER_MAX_TRACKED_REGIONS,
    PAGER_PRESSURE_REGION_SPLIT_NO_SLOT, PAGER_PRESSURE_REGION_TABLE_FULL,
    PAGER_PRESSURE_UNSPECIFIED, VM_ACCESS_EXECUTE, VM_ACCESS_READ, VM_ACCESS_WRITE, VM_FAULT_COW,
    VM_FAULT_PRESENT, VM_FAULT_PROTECTION, VM_OBJECT_ANONYMOUS, VM_PROT_EXECUTE, VM_PROT_READ,
    VM_PROT_WRITE,
};

/// Live regions this pager tracks, sized by the shared ABI against ring0's
/// per-process VMA ceiling so one process cannot wedge the others.
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
    /// A bounded table refused, carrying the ABI code for *which* table.
    ///
    /// One undifferentiated `Pressure` made a full region table, an empty
    /// fault-frame reserve and a full grant table read identically in the log,
    /// so every occurrence cost a fresh investigation of all three.
    Pressure(u16),
    EpochExhausted,
}

impl PagerFaultError {
    /// The ABI pressure code this error carries, or `UNSPECIFIED`.
    pub const fn pressure_code(self) -> u16 {
        match self {
            Self::Pressure(code) => code,
            _ => PAGER_PRESSURE_UNSPECIFIED,
        }
    }
}

pub struct PagerState {
    epoch: u64,
    /// Per-process B-tree of tracked regions keyed by `start`, with
    /// non-overlapping spans per process. Provides O(log P + log M) lookup,
    /// admission, and range edits, O(log P) process invalidation, and zero
    /// upfront allocation until the first region is admitted.
    processes: BTreeMap<u64, BTreeMap<u64, PagerVmRegionWire>>,
    tracked_count: usize,
    /// Highest fault-token generation accepted for each ring0 slot. Index is
    /// the token's one-based slot minus one; zero means the slot is unused.
    accepted_generations: [u64; PAGER_MAX_CONSUMED_TOKENS],
}

impl PagerState {
    pub const fn new(epoch: u64) -> Self {
        Self {
            epoch,
            processes: BTreeMap::new(),
            tracked_count: 0,
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
        if let Some(proc_regions) = self.processes.get(&region.process_handle) {
            if let Some((_, prev)) = proc_regions.range(..=region.start).next_back() {
                if prev.end > region.start {
                    return Err(PagerFaultError::Stale);
                }
            }
            if let Some((_, next)) = proc_regions.range(region.start..).next() {
                if region.end > next.start {
                    return Err(PagerFaultError::Stale);
                }
            }
        }
        if self.tracked_count >= PAGER_MAX_REGIONS {
            return Err(PagerFaultError::Pressure(PAGER_PRESSURE_REGION_TABLE_FULL));
        }
        self.processes
            .entry(region.process_handle)
            .or_default()
            .insert(region.start, region);
        self.tracked_count += 1;
        Ok(())
    }

    /// Regions this pager currently tracks, across every process.
    pub fn tracked_regions(&self) -> usize {
        self.tracked_count
    }

    /// Free region slots. A split needs one of these for its second fragment.
    pub fn free_region_slots(&self) -> usize {
        PAGER_MAX_REGIONS - self.tracked_count
    }

    /// Processes currently holding at least one tracked region (0 until first admission).
    pub fn tracked_processes(&self) -> usize {
        self.processes.len()
    }

    /// Applies one ring0-stamped range edit to every region it overlaps,
    /// using the shared ABI rule rather than a private one.
    ///
    /// # Why this is two-phase
    ///
    /// A `munmap` in the middle of a region splits it, so the result can need
    /// one more slot than the input. Computing every outcome first means a
    /// table that cannot hold the result refuses the whole edit and keeps the
    /// original region, instead of applying the edit halfway and losing a
    /// range the process can still touch.
    ///
    /// # Why refusing is safe, and dropping is not
    ///
    /// Ring0's VMA table is the authority for whether a mapping exists; this
    /// table is policy for how it is backed. A region that outlives its ring0
    /// VMA is inert - no fault can reach it, because ring0 rejects the fault
    /// before it ever dispatches. A region that is missing while ring0 still
    /// has its VMA kills the faulting thread. So under pressure this replica
    /// keeps *more* than ring0, never less, and asks the caller to retry; the
    /// broker's parked-release reconciliation is exactly that retry.
    fn apply_edit(
        &mut self,
        process_handle: u64,
        process_generation: u64,
        edit: PagerRangeEdit,
    ) -> Result<usize, PagerFaultError> {
        if !edit.is_canonical() || process_handle == 0 || process_generation == 0 {
            return Err(PagerFaultError::Malformed);
        }
        let Some(proc_regions) = self.processes.get(&process_handle) else {
            return Ok(0);
        };
        let mut matched: Vec<PagerVmRegionWire> = Vec::new();
        let mut additional = 0_usize;

        let mut inspect = |region: PagerVmRegionWire| -> Result<(), PagerFaultError> {
            if region.process_generation != process_generation {
                return Ok(());
            }
            let outcome = apply_region_edit(region, edit);
            if outcome.is_rejection() {
                return Err(match outcome {
                    PagerRegionEdit::Denied => PagerFaultError::Stale,
                    _ => PagerFaultError::Malformed,
                });
            }
            additional += outcome.additional_pager_slots();
            matched
                .try_reserve(1)
                .map_err(|_| PagerFaultError::Pressure(PAGER_PRESSURE_REGION_SPLIT_NO_SLOT))?;
            matched.push(region);
            Ok(())
        };

        // At most one region starting strictly before `edit.start` can extend
        // past `edit.start` because per-process regions never overlap.
        if let Some((_, &prev)) = proc_regions.range(..edit.start).next_back() {
            if prev.end > edit.start {
                inspect(prev)?;
            }
        }
        for (_, &region) in proc_regions.range(edit.start..edit.end) {
            inspect(region)?;
        }

        let edited = matched.len();
        if edited == 0 {
            return Ok(0);
        }
        if additional > self.free_region_slots() {
            return Err(PagerFaultError::Pressure(
                PAGER_PRESSURE_REGION_SPLIT_NO_SLOT,
            ));
        }

        let proc_regions = self
            .processes
            .get_mut(&process_handle)
            .expect("process map checked above");
        for region in matched {
            let removed = proc_regions.remove(&region.start);
            debug_assert!(removed.is_some());
            self.tracked_count -= 1;
            let (fragments, len) = apply_region_edit(region, edit).pager_fragments();
            for fragment in &fragments[..len] {
                proc_regions.insert(fragment.start, *fragment);
                self.tracked_count += 1;
            }
        }
        if proc_regions.is_empty() {
            self.processes.remove(&process_handle);
        }
        Ok(edited)
    }

    /// Releases tracking for one exact ring0-stamped range.
    ///
    /// Ring0 frees its own VMA slot on unmap and then sends this. Without it a
    /// dead region would live forever: it would refuse to re-admit the same
    /// range as an overlap, and the fixed table would fill and start refusing
    /// every admission, which downgrades demand paging to eager mapping.
    ///
    /// A partial release **trims or splits**; it does not drop the whole
    /// region. `munmap(2)` in the middle of a mapping leaves two smaller
    /// mappings on either side, and ring0's VMA table already preserves them,
    /// so dropping them here made the two replicas disagree and turned the
    /// next fault in a surviving remainder into a dead thread.
    pub fn release_range(
        &mut self,
        release: rustos_user_abi::pager::PagerReleaseRangeWire,
    ) -> Result<usize, PagerFaultError> {
        if !release.is_canonical() {
            return Err(PagerFaultError::Malformed);
        }
        self.apply_edit(
            release.process_handle,
            release.process_generation,
            release.edit(),
        )
    }

    /// Narrows tracked protection over one exact ring0-stamped range.
    ///
    /// `region.prot` is what this pager answers a fault with, so a protection
    /// change ring0 applied but never published here would let the pager grant
    /// rights the process no longer has. Attenuation only: an edit that widens
    /// any right the region does not already hold is refused.
    pub fn protect_range(
        &mut self,
        protect: rustos_user_abi::pager::PagerProtectRangeWire,
    ) -> Result<usize, PagerFaultError> {
        if !protect.is_canonical() {
            return Err(PagerFaultError::Malformed);
        }
        self.apply_edit(
            protect.process_handle,
            protect.process_generation,
            protect.edit(),
        )
    }

    pub fn invalidate_process(&mut self, process_handle: u64) -> usize {
        let removed = self
            .processes
            .remove(&process_handle)
            .map_or(0, |regions| regions.len());
        self.tracked_count -= removed;
        removed
    }

    pub fn restart(&mut self) -> Result<u64, PagerFaultError> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .filter(|epoch| *epoch != 0)
            .ok_or(PagerFaultError::EpochExhausted)?;
        self.processes.clear();
        self.tracked_count = 0;
        self.accepted_generations.fill(0);
        Ok(self.epoch)
    }

    /// Resolves one zero-fill dispatch received through the fixed rendezvous.
    ///
    /// Ring0 answers anonymous first touch itself, so no live dispatch reaches
    /// this today (see `docs/ai/pager-protocol-contract.md` §0). It stays as
    /// the worked reference for the rendezvous reply - token freshness, region
    /// matching, the deny classes, and the run-length answer - which the page
    /// cache's pager-backed resolution is built on top of, and which the tests
    /// below exercise directly.
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
            .processes
            .get(&request.process_handle)
            .and_then(|proc_regions| {
                proc_regions
                    .range(..=request.virtual_address)
                    .next_back()
                    .map(|(_, &region)| region)
            })
            .filter(|region| fault_matches_region(request, *region))
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
        // Fault-around policy. Ring0 offers what the VMA and its reserve can
        // support; this decides how much of that to take. Anonymous first
        // touch is overwhelmingly sequential, so taking the whole offer
        // amortizes one rendezvous round trip over the run. Every page past
        // the first is best effort in ring0, so asking for more can cost
        // throughput but never correctness.
        reply.map_run_pages = u64::from(dispatch.map_run_pages_offered);
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
/// Fault resolution and every region-table edit (backing, release, protect) are
/// ring0 operations: ring0 owns the VMA table and the pager's region table is
/// only its replica, so a user process must never be able to admit, drop or
/// re-protect a region for itself or for a pid it names in the payload. A live
/// kernel task is stamped `(0, exact_nonzero_tid)`; `(0, 0)` is an unavailable
/// sender, never kernel authority. The wire subject must name that same task.
/// The region's target process remains in its separate typed payload. Generic
/// fault resolution still has no transport authority: main rejects it in favor
/// of the worker-bound fixed rendezvous. Every other operation retains the
/// ordinary, nonzero exact-user-subject rule.
pub fn request_sender_is_authorized(
    request: &rustos_user_abi::syscall::CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> bool {
    use rustos_user_abi::syscall::{
        COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT, COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE,
        COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT, COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT,
    };
    if matches!(
        request.header.op,
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE
            | COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT
            | COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT
            | COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT
    ) {
        sender_pid == 0 && sender_tid != 0 && (request.header.subject_pid, request.header.subject_tid) == (0, sender_tid)
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
        // A denial maps nothing, but the wire still has to be canonical, and
        // one is the only length that is always valid.
        map_run_pages: 1,
        ..PagerFaultReplyWire::default()
    }
}

#[cfg(test)]
mod sender_auth_tests {
    use super::request_sender_is_authorized;
    use rustos_user_abi::syscall::{
        CommercialMaxProtocolRequest, COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT,
        COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY,
        COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT, COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT,
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
        let request = request(COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, 0, 67);
        assert!(request_sender_is_authorized(&request, 0, 67));
        assert!(!request_sender_is_authorized(&request, 0, 0));
    }

    #[test]
    fn fault_resolve_rejects_user_sender_even_when_subject_is_spoofed_to_match() {
        let request = request(COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, 41, 43);
        assert!(!request_sender_is_authorized(&request, 41, 43));
    }

    #[test]
    fn region_table_edits_are_ring0_only_even_with_a_matching_user_subject() {
        for op in [
            COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT,
            COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT,
            COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT,
        ] {
            let user = request(op, 47, 53);
            assert!(!request_sender_is_authorized(&user, 47, 53));
            let kernel = request(op, 0, 67);
            assert!(request_sender_is_authorized(&kernel, 0, 67));
            assert!(!request_sender_is_authorized(&kernel, 47, 53));
        }
    }

    #[test]
    fn kernel_pager_requests_reject_stale_or_mismatched_transport_and_wire_subjects() {
        for op in [
            COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE,
            COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT,
            COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT,
            COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT,
        ] {
            let kernel = request(op, 0, 67);
            for (sender_pid, sender_tid) in [(0, 0), (0, 68), (47, 67), (47, 0)] {
                assert!(!request_sender_is_authorized(&kernel, sender_pid, sender_tid),
                    "op {op} cannot turn an unavailable or different sender into its kernel subject");
            }
            for (subject_pid, subject_tid) in [(0, 0), (0, 68), (47, 67), (47, 53)] {
                assert!(!request_sender_is_authorized(&request(op, subject_pid, subject_tid), 0, 67),
                    "op {op} must bind the wire subject to the attested kernel task");
            }
            let absent = request(op, 0, 0);
            assert!(!request_sender_is_authorized(&absent, 0, 0),
                "matching zero placeholders are not an authenticated kernel sender");
        }
    }

    #[test]
    fn other_operations_retain_exact_user_subject_rule() {
        let request = request(COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY, 47, 53);
        assert!(request_sender_is_authorized(&request, 47, 53));
        assert!(!request_sender_is_authorized(&request, 0, 0));
        assert!(!request_sender_is_authorized(&request, 0, 67));
        assert!(!request_sender_is_authorized(&request, 47, 54));
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
            commit_state: rustos_user_abi::pager::VM_COMMIT_COMMITTED,
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
            map_run_pages_offered: 1,
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

    fn protect_of(
        region: PagerVmRegionWire,
        start: u64,
        end: u64,
        prot: u32,
    ) -> rustos_user_abi::pager::PagerProtectRangeWire {
        rustos_user_abi::pager::PagerProtectRangeWire {
            version: PAGER_FAULT_ABI_VERSION,
            reserved0: 0,
            prot,
            process_handle: region.process_handle,
            process_generation: region.process_generation,
            start,
            end,
            reserved1: [0; 2],
        }
    }

    fn release_span(
        region: PagerVmRegionWire,
        start: u64,
        end: u64,
    ) -> rustos_user_abi::pager::PagerReleaseRangeWire {
        rustos_user_abi::pager::PagerReleaseRangeWire {
            start,
            end,
            ..release_of(region)
        }
    }

    fn at(region: PagerVmRegionWire, address: u64, token_value: u64) -> PagerFaultRequestWire {
        PagerFaultRequestWire {
            virtual_address: address,
            object_offset: region.object_offset + (address - region.start),
            ..request(region, token_value)
        }
    }

    /// Whole tracked span set, sorted, so a test compares the map rather than
    /// one region at a time.
    fn spans(pager: &PagerState) -> ([(u64, u64); 8], usize) {
        let mut spans = [(0_u64, 0_u64); 8];
        let mut len = 0;
        for proc_regions in pager.processes.values() {
            for region in proc_regions.values() {
                assert!(len < spans.len(), "span set is larger than this fixture");
                spans[len] = (region.start, region.end);
                len += 1;
            }
        }
        spans[..len].sort_unstable();
        (spans, len)
    }

    /// The defect this whole rule exists for. Ring0's VMA table preserves the
    /// left and right remainders of an interior `munmap`; this replica used to
    /// delete every overlapping region, so the next fault in a surviving
    /// remainder matched nothing here and killed the thread.
    #[test]
    fn an_interior_release_keeps_both_remainders_and_they_still_fault() {
        let mut pager = PagerState::new(2);
        let region = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x18_000,
            ..region(2)
        };
        pager.admit_region(region).unwrap();
        assert_eq!(
            pager.release_range(release_span(region, 0x12_000, 0x14_000)),
            Ok(1)
        );

        let (spans, len) = spans(&pager);
        assert_eq!(&spans[..len], &[(0x10_000, 0x12_000), (0x14_000, 0x18_000)]);

        // Both remainders still resolve; the hole does not.
        for (address, slot) in [(0x10_000_u64, 1_u64), (0x14_000, 2), (0x17_000, 3)] {
            let reply = pager
                .resolve_anonymous_first_touch(dispatch(
                    at(region, address, token(slot, 101)),
                    53,
                    region.prot,
                ))
                .unwrap_or_else(|error| panic!("{address:#x} must still be managed: {error:?}"));
            assert_eq!(reply.action, PAGER_ACTION_MAP_ZEROED);
        }
        assert_eq!(
            pager.resolve_anonymous_first_touch(dispatch(
                at(region, 0x12_000, token(9, 101)),
                53,
                region.prot
            )),
            Err(PagerFaultError::NotManaged)
        );
    }

    #[test]
    fn head_and_tail_releases_trim_in_place_without_consuming_a_slot() {
        let mut pager = PagerState::new(2);
        let region = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x18_000,
            ..region(2)
        };
        pager.admit_region(region).unwrap();
        let free_before = pager.free_region_slots();
        assert_eq!(
            pager.release_range(release_span(region, 0x10_000, 0x12_000)),
            Ok(1)
        );
        assert_eq!(pager.free_region_slots(), free_before);
        assert_eq!(
            pager.release_range(release_span(region, 0x16_000, 0x18_000)),
            Ok(1)
        );
        let (spans, len) = spans(&pager);
        assert_eq!(&spans[..len], &[(0x12_000, 0x16_000)]);
        // The freed head is admissible again; the surviving middle is not.
        let head = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x12_000,
            ..region
        };
        assert!(pager.admit_region(head).is_ok());
        assert_eq!(pager.admit_region(region), Err(PagerFaultError::Stale));
    }

    /// A release that spans several regions can only trim the two it ends
    /// inside, so it never needs a free slot however many it crosses.
    #[test]
    fn a_release_crossing_many_regions_never_grows_the_table() {
        let mut pager = PagerState::new(2);
        let base = region(2);
        for index in 0..4_u64 {
            let start = 0x10_000 + index * 0x4000;
            pager
                .admit_region(PagerVmRegionWire {
                    start,
                    end: start + 0x4000,
                    ..base
                })
                .unwrap();
        }
        let before = pager.tracked_regions();
        assert_eq!(before, 4);
        assert_eq!(
            pager.release_range(release_span(base, 0x12_000, 0x1e_000)),
            Ok(4)
        );
        assert!(pager.tracked_regions() <= before);
        let (spans, len) = spans(&pager);
        assert_eq!(&spans[..len], &[(0x10_000, 0x12_000), (0x1e_000, 0x20_000)]);
    }

    /// Under table pressure a split must refuse rather than drop the region.
    /// Keeping more than ring0 is inert - ring0's VMA check gates every fault
    /// before pagerd sees it - while keeping less kills a live mapping.
    #[test]
    fn a_split_that_cannot_fit_refuses_and_keeps_the_region_whole() {
        let mut pager = PagerState::new(2);
        let base = region(2);
        for index in 0..PAGER_MAX_REGIONS as u64 {
            let start = 0x10_000 + index * 0x8000;
            pager
                .admit_region(PagerVmRegionWire {
                    start,
                    end: start + 0x8000,
                    ..base
                })
                .unwrap();
        }
        assert_eq!(pager.free_region_slots(), 0);
        let error = pager
            .release_range(release_span(base, 0x12_000, 0x14_000))
            .unwrap_err();
        assert_eq!(
            error,
            PagerFaultError::Pressure(PAGER_PRESSURE_REGION_SPLIT_NO_SLOT)
        );
        assert_eq!(
            error.pressure_code(),
            PAGER_PRESSURE_REGION_SPLIT_NO_SLOT,
            "the log must name which table refused"
        );
        // The region survives intact, so a fault anywhere in it still resolves.
        assert_eq!(pager.tracked_regions(), PAGER_MAX_REGIONS);
        let whole = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x18_000,
            ..base
        };
        assert!(pager
            .resolve_anonymous_first_touch(dispatch(
                at(whole, 0x16_000, token(4, 103)),
                53,
                whole.prot
            ))
            .is_ok());

        // Once one slot frees, the parked retry succeeds.
        let last = 0x10_000 + (PAGER_MAX_REGIONS as u64 - 1) * 0x8000;
        assert_eq!(
            pager.release_range(release_span(base, last, last + 0x8000)),
            Ok(1)
        );
        assert_eq!(
            pager.release_range(release_span(base, 0x12_000, 0x14_000)),
            Ok(1)
        );
    }

    #[test]
    fn region_storage_starts_empty_and_scales_to_linux_vma_parity_in_log_time() {
        let mut pager = PagerState::new(2);
        assert_eq!(pager.tracked_regions(), 0);
        assert_eq!(pager.tracked_processes(), 0);
        let base = region(2);
        // Insert 65,536 regions in descending order (worst case for flat arrays)
        // for one process, plus a second process to verify per-process isolation.
        let count = rustos_user_abi::pager::PAGER_MAX_VMAS_PER_PROCESS as u64;
        for index in (0..count).rev() {
            let start = 0x10_000 + index * 0x8000;
            pager
                .admit_region(PagerVmRegionWire {
                    start,
                    end: start + 0x4000,
                    ..base
                })
                .unwrap();
        }
        let other = PagerVmRegionWire {
            process_handle: base.process_handle + 1,
            start: 0x20_000,
            end: 0x28_000,
            ..base
        };
        pager.admit_region(other).unwrap();
        assert_eq!(pager.tracked_regions(), count as usize + 1);
        assert_eq!(pager.tracked_processes(), 2);

        // Interior split in the middle of 65,536 regions and O(log N) lookup.
        let mid_start = 0x10_000 + (count / 2) * 0x8000;
        assert_eq!(
            pager.release_range(release_span(base, mid_start + 0x1000, mid_start + 0x2000)),
            Ok(1)
        );
        assert_eq!(pager.tracked_regions(), count as usize + 2);
        let mid_region = PagerVmRegionWire {
            start: mid_start,
            end: mid_start + 0x4000,
            ..base
        };
        assert!(pager
            .resolve_anonymous_first_touch(dispatch(
                at(mid_region, mid_start + 0x2000, token(1, 300)),
                53,
                base.prot
            ))
            .is_ok());
        assert_eq!(
            pager.resolve_anonymous_first_touch(dispatch(
                at(mid_region, mid_start + 0x1000, token(2, 300)),
                53,
                base.prot
            )),
            Err(PagerFaultError::NotManaged)
        );

        // O(log P) process invalidation removes only the exited process and frees its map.
        assert_eq!(
            pager.invalidate_process(base.process_handle),
            count as usize + 1
        );
        assert_eq!(pager.tracked_regions(), 1);
        assert_eq!(pager.tracked_processes(), 1);
        assert_eq!(pager.invalidate_process(other.process_handle), 1);
        assert_eq!(pager.tracked_regions(), 0);
        assert_eq!(pager.tracked_processes(), 0);
    }

    /// `mprotect` on part of a region splits it on ring0's side. Without the
    /// matching notification this replica keeps the original protection and
    /// answers a fault in the narrowed span with rights the process no longer
    /// has, because `reply.frame_rights` comes from `region.prot`.
    #[test]
    fn an_interior_protect_narrows_only_the_edited_span() {
        let mut pager = PagerState::new(2);
        let region = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x18_000,
            ..region(2)
        };
        pager.admit_region(region).unwrap();
        assert_eq!(
            pager.protect_range(protect_of(region, 0x12_000, 0x14_000, VM_PROT_READ)),
            Ok(1)
        );
        assert_eq!(pager.tracked_regions(), 3);

        // A write fault in the narrowed span is denied for illegal access...
        let denied = pager
            .resolve_anonymous_first_touch(dispatch(
                at(region, 0x12_000, token(5, 107)),
                53,
                region.prot,
            ))
            .unwrap();
        assert_eq!(denied.action, PAGER_ACTION_DENY);
        assert_eq!(denied.disposition, PAGER_DISPOSITION_ILLEGAL_ACCESS);
        // ...while the untouched tail still grants its original rights.
        let granted = pager
            .resolve_anonymous_first_touch(dispatch(
                at(region, 0x16_000, token(6, 107)),
                53,
                region.prot,
            ))
            .unwrap();
        assert_eq!(granted.action, PAGER_ACTION_MAP_ZEROED);
        assert_eq!(granted.frame_rights, region.prot);
    }

    #[test]
    fn range_compaction_preserves_foreign_generations_and_neighboring_processes() {
        let base = region(2);
        for prot in [VM_PROT_READ, 0] {
            let mut pager = PagerState::new(2);
            pager.admit_region(PagerVmRegionWire { process_handle: 12, ..base }).unwrap();
            for index in 0..4_u64 {
                let start = 0x10_000 + index * 0x4000;
                pager.admit_region(PagerVmRegionWire {
                    start,
                    end: start + 0x4000,
                    process_generation: if index == 1 { 18 } else { base.process_generation },
                    ..base
                }).unwrap();
            }
            pager.admit_region(PagerVmRegionWire { process_handle: 14, ..base }).unwrap();
            let result = if prot != 0 {
                pager.protect_range(protect_of(base, 0x12_000, 0x1e_000, prot))
            } else {
                pager.release_range(release_span(base, 0x12_000, 0x1e_000))
            };
            assert_eq!(result, Ok(3));
            let actual: Vec<_> = pager
                .processes
                .values()
                .flat_map(|proc_regions| proc_regions.values())
                .map(|r| {
                    (
                        r.process_handle,
                        r.process_generation,
                        r.start,
                        r.end,
                        r.prot,
                        r.object_offset,
                    )
                })
                .collect();
            let mut expected = alloc::vec![
                (12, 17, 0x4000, 0x8000, base.prot, 0),
                (13, 17, 0x10_000, 0x12_000, base.prot, 0),
            ];
            if prot != 0 {
                expected.push((13, 17, 0x12_000, 0x14_000, prot, 0x2000));
            }
            expected.push((13, 18, 0x14_000, 0x18_000, base.prot, 0));
            if prot != 0 {
                expected.push((13, 17, 0x18_000, 0x1c_000, prot, 0));
                expected.push((13, 17, 0x1c_000, 0x1e_000, prot, 0));
            }
            expected.push((13, 17, 0x1e_000, 0x20_000, base.prot, 0x2000));
            expected.push((14, 17, 0x4000, 0x8000, base.prot, 0));
            assert_eq!(actual, expected);
            assert_eq!(pager.tracked_regions(), expected.len());
        }
    }

    #[test]
    fn a_protect_that_widens_rights_is_refused_without_touching_the_table() {
        let mut pager = PagerState::new(2);
        // Delegated objects stay attenuation-only. (A ring0-owned private
        // anonymous object has no delegated ceiling and may be reprotected.)
        let mut region = region(2);
        region.object.object_type = rustos_user_abi::pager::VM_OBJECT_FILE_PRIVATE;
        region.object.backing_service = 2;
        region.prot = VM_PROT_READ;
        region.object.rights = VM_PROT_READ;
        pager.admit_region(region).unwrap();
        assert_eq!(
            pager.protect_range(protect_of(
                region,
                region.start,
                region.end,
                VM_PROT_READ | VM_PROT_WRITE
            )),
            Err(PagerFaultError::Stale)
        );
        let (spans, len) = spans(&pager);
        assert_eq!(&spans[..len], &[(region.start, region.end)]);
    }

    /// Fault-around policy: take the whole run ring0 offers, and never more.
    ///
    /// Ring0 has already clipped the offer to the VMA's own extent and to
    /// `PAGER_FAULT_RUN_PAGES_MAX`, so exceeding it would be asking ring0 to
    /// map outside the authority this fault carries. Asking for less would
    /// leave the round trip unamortized for no gain: anonymous first touch is
    /// overwhelmingly sequential.
    #[test]
    fn fault_around_takes_the_offered_run_and_never_exceeds_it() {
        let mut pager = PagerState::new(2);
        let region = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x20_000,
            ..region(2)
        };
        pager.admit_region(region).unwrap();
        for (offered, slot) in [(1_u32, 1_u64), (4, 2), (16, 3)] {
            let mut fault = dispatch(at(region, region.start, token(slot, 200)), 53, region.prot);
            fault.map_run_pages_offered = offered;
            let reply = pager.resolve_anonymous_first_touch(fault).unwrap();
            assert_eq!(reply.action, PAGER_ACTION_MAP_ZEROED);
            assert_eq!(reply.map_run_pages, u64::from(offered));
            assert!(reply.map_run_pages <= u64::from(fault.map_run_pages_offered));
            assert!(reply.is_canonical_zeroed_for(fault));
        }
    }

    /// A denial still has to be a canonical wire, and it maps nothing.
    #[test]
    fn a_denied_fault_carries_a_canonical_run_length() {
        let mut pager = PagerState::new(2);
        let region = region(2);
        pager.admit_region(region).unwrap();
        let protection = PagerFaultRequestWire {
            fault_flags: VM_FAULT_PROTECTION,
            ..request(region, token(7, 211))
        };
        let reply = pager
            .resolve_anonymous_first_touch(dispatch(protection, 97, region.prot))
            .unwrap();
        assert_eq!(reply.action, PAGER_ACTION_DENY);
        assert_eq!(reply.map_run_pages, 1);
        assert!(reply.is_canonical_for(protection));
    }

    /// Sustained partial unmaps must not leak slots. Each cycle splits and
    /// then releases both remainders, returning the table to its start size.
    #[test]
    fn repeated_split_and_release_cycles_reclaim_every_slot() {
        let mut pager = PagerState::new(2);
        let base = region(2);
        let whole = PagerVmRegionWire {
            start: 0x10_000,
            end: 0x18_000,
            ..base
        };
        for round in 0..64 {
            assert!(
                pager.admit_region(whole).is_ok(),
                "round {round} must be admissible"
            );
            assert_eq!(
                pager.release_range(release_span(whole, 0x12_000, 0x14_000)),
                Ok(1)
            );
            assert_eq!(pager.tracked_regions(), 2, "round {round}");
            assert_eq!(
                pager.release_range(release_span(whole, 0x10_000, 0x18_000)),
                Ok(2)
            );
            assert_eq!(pager.tracked_regions(), 0, "round {round}");
        }
    }
}
