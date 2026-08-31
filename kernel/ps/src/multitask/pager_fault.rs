//! Allocation-free, generation-bound custody for pager-managed user faults.
//!
//! - **Owner:** `kernel-ps` owns fixed fault-token custody and the scheduler
//!   owns the corresponding task block/wake transition; pagerd owns policy.
//! - **Boundary:** Exception entry supplies a fully stamped request and VMA
//!   endpoint capability. This module publishes neither process-state nor
//!   physical-frame authority to pagerd.
//! - **Lifecycle:** Reserve `FaultPending`, commit `BlockedOnPager`, claim one
//!   reply, then consume or cancel before the generation-bound slot is reused.
//! - **Concurrency:** Every slot is all-atomic. Release publication and paired
//!   Acquire snapshots make a writer/reply/cancel race select exactly one
//!   terminal authority without taking `ProcessStateLock`.
//! - **Failure:** Malformed requests, absent endpoint authority, pressure,
//!   stale generation, and invalid transitions fail closed without a wait.
//! - **Forbidden:** No allocator, `Vec`, service lookup, pathname lookup,
//!   `ProcessStateLock`, physical address, or reusable token in fault entry.
//! - **Evidence:** Focused unit tests plus `pager-fault-slot-lifecycle` TLC,
//!   spec mutations, and implementation mutations.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::pager::{
    PAGER_FAULT_ABI_VERSION, PagerEndpointCapabilityWire, PagerFaultRequestWire,
    PagerObjectIdentityWire,
};

/// The bounded number of simultaneous user faults that may wait on pagerd.
///
/// This is deliberately independent from endpoint queue capacity: a full
/// pager queue must fail the newly faulting task closed, rather than leave an
/// unbounded amount of task and frame authority resident in ring0.
// The token shape is ABI, not a kernel-private detail: pagerd needs it to keep
// exact per-slot replay state, so both sides read one definition.
pub const MAX_PAGER_FAULT_SLOTS: usize = rustos_user_abi::pager::PAGER_MAX_FAULT_SLOTS;

const SLOT_BITS: u32 = rustos_user_abi::pager::PAGER_FAULT_TOKEN_SLOT_BITS;
const SLOT_MASK: u64 = rustos_user_abi::pager::PAGER_FAULT_TOKEN_SLOT_MASK;
const MAX_SLOT_GENERATION: u64 = rustos_user_abi::pager::PAGER_MAX_FAULT_TOKEN_GENERATION;

const SLOT_FREE: u64 = 0;
const SLOT_INITIALIZING: u64 = 1;
const SLOT_FAULT_PENDING: u64 = 2;
const SLOT_BLOCKED_ON_PAGER: u64 = 3;
const SLOT_REPLY_CLAIMED: u64 = 4;
const SLOT_DISPATCHED_TO_PAGER: u64 = 5;
const SLOT_CANCEL_CLAIMED: u64 = 6;

/// The observable custody state of one pager fault token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerFaultState {
    FaultPending,
    BlockedOnPager,
    DispatchedToPager,
    ReplyClaimed,
    CancelClaimed,
}

/// Why a pager-fault slot operation was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PagerFaultSlotError {
    /// The request or endpoint cannot become an exact pager authority.
    Malformed,
    /// All fixed fault slots are live, or a slot generation is exhausted.
    Pressure,
    /// The token belongs to an old slot generation or was already consumed.
    Stale,
    /// The requested transition does not match the current custody state.
    Transition,
}

/// A copied, exact pager request plus the endpoint authority it is bound to.
///
/// `request.fault_token` is an opaque one-shot token.  `endpoint` is retained
/// separately because it is not part of the wire request and must be checked
/// against the VMA's pre-published endpoint before a kernel IPC is attempted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagerFaultReservation {
    pub token: u64,
    pub state: PagerFaultState,
    pub request: PagerFaultRequestWire,
    pub endpoint: PagerEndpointCapabilityWire,
    pub zeroed_frame_capability: u64,
    pub granted_frame_rights: u32,
    pub dispatch_reply_handle: u64,
}

struct PagerFaultSlot {
    /// State is the publication word for all following Relaxed fields.
    state: AtomicU64,
    generation: AtomicU64,
    request_header: AtomicU64,
    process_handle: AtomicU64,
    process_generation: AtomicU64,
    task_id: AtomicU64,
    task_generation: AtomicU64,
    mm_generation: AtomicU64,
    vma_generation: AtomicU64,
    virtual_address: AtomicU64,
    object_offset: AtomicU64,
    deadline_ns: AtomicU64,
    scheduling_domain: AtomicU64,
    charge_token: AtomicU64,
    object_header: AtomicU64,
    backing_service: AtomicU64,
    object_slot: AtomicU64,
    object_generation: AtomicU64,
    pager_epoch: AtomicU64,
    backing_generation: AtomicU64,
    endpoint_slot: AtomicU64,
    endpoint_generation: AtomicU64,
    endpoint_rights: AtomicU64,
    zeroed_frame_capability: AtomicU64,
    granted_frame_rights: AtomicU64,
    dispatch_reply_handle: AtomicU64,
}

impl PagerFaultSlot {
    const fn empty() -> Self {
        Self {
            state: AtomicU64::new(SLOT_FREE),
            generation: AtomicU64::new(0),
            request_header: AtomicU64::new(0),
            process_handle: AtomicU64::new(0),
            process_generation: AtomicU64::new(0),
            task_id: AtomicU64::new(0),
            task_generation: AtomicU64::new(0),
            mm_generation: AtomicU64::new(0),
            vma_generation: AtomicU64::new(0),
            virtual_address: AtomicU64::new(0),
            object_offset: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(0),
            scheduling_domain: AtomicU64::new(0),
            charge_token: AtomicU64::new(0),
            object_header: AtomicU64::new(0),
            backing_service: AtomicU64::new(0),
            object_slot: AtomicU64::new(0),
            object_generation: AtomicU64::new(0),
            pager_epoch: AtomicU64::new(0),
            backing_generation: AtomicU64::new(0),
            endpoint_slot: AtomicU64::new(0),
            endpoint_generation: AtomicU64::new(0),
            endpoint_rights: AtomicU64::new(0),
            zeroed_frame_capability: AtomicU64::new(0),
            granted_frame_rights: AtomicU64::new(0),
            dispatch_reply_handle: AtomicU64::new(0),
        }
    }

    fn write_request(
        &self,
        request: PagerFaultRequestWire,
        endpoint: PagerEndpointCapabilityWire,
        zeroed_frame_capability: u64,
        granted_frame_rights: u32,
    ) {
        self.request_header.store(
            u64::from(request.version)
                | (u64::from(request.access) << 16)
                | (u64::from(request.fault_flags) << 32),
            Ordering::Relaxed,
        );
        self.process_handle
            .store(request.process_handle, Ordering::Relaxed);
        self.process_generation
            .store(request.process_generation, Ordering::Relaxed);
        self.task_id.store(request.task_id, Ordering::Relaxed);
        self.task_generation
            .store(request.task_generation, Ordering::Relaxed);
        self.mm_generation
            .store(request.mm_generation, Ordering::Relaxed);
        self.vma_generation
            .store(request.vma_generation, Ordering::Relaxed);
        self.virtual_address
            .store(request.virtual_address, Ordering::Relaxed);
        self.object_offset
            .store(request.object_offset, Ordering::Relaxed);
        self.deadline_ns
            .store(request.deadline_ns, Ordering::Relaxed);
        self.scheduling_domain
            .store(request.scheduling_domain, Ordering::Relaxed);
        self.charge_token
            .store(request.charge_token, Ordering::Relaxed);
        self.object_header.store(
            u64::from(request.object.object_type) | (u64::from(request.object.rights) << 32),
            Ordering::Relaxed,
        );
        self.backing_service
            .store(request.object.backing_service, Ordering::Relaxed);
        self.object_slot
            .store(request.object.slot, Ordering::Relaxed);
        self.object_generation
            .store(request.object.generation, Ordering::Relaxed);
        self.pager_epoch
            .store(request.object.pager_epoch, Ordering::Relaxed);
        self.backing_generation
            .store(request.object.backing_generation, Ordering::Relaxed);
        self.endpoint_slot.store(endpoint.slot, Ordering::Relaxed);
        self.endpoint_generation
            .store(endpoint.generation, Ordering::Relaxed);
        self.endpoint_rights
            .store(endpoint.rights, Ordering::Relaxed);
        self.zeroed_frame_capability
            .store(zeroed_frame_capability, Ordering::Relaxed);
        self.granted_frame_rights
            .store(u64::from(granted_frame_rights), Ordering::Relaxed);
    }

    fn read_reservation(&self, token: u64, state: PagerFaultState) -> PagerFaultReservation {
        let request_header = self.request_header.load(Ordering::Relaxed);
        let object_header = self.object_header.load(Ordering::Relaxed);
        PagerFaultReservation {
            token,
            state,
            request: PagerFaultRequestWire {
                version: request_header as u16,
                access: (request_header >> 16) as u16,
                fault_flags: (request_header >> 32) as u16,
                reserved0: 0,
                fault_token: token,
                process_handle: self.process_handle.load(Ordering::Relaxed),
                process_generation: self.process_generation.load(Ordering::Relaxed),
                task_id: self.task_id.load(Ordering::Relaxed),
                task_generation: self.task_generation.load(Ordering::Relaxed),
                mm_generation: self.mm_generation.load(Ordering::Relaxed),
                vma_generation: self.vma_generation.load(Ordering::Relaxed),
                virtual_address: self.virtual_address.load(Ordering::Relaxed),
                object_offset: self.object_offset.load(Ordering::Relaxed),
                deadline_ns: self.deadline_ns.load(Ordering::Relaxed),
                scheduling_domain: self.scheduling_domain.load(Ordering::Relaxed),
                charge_token: self.charge_token.load(Ordering::Relaxed),
                object: PagerObjectIdentityWire {
                    object_type: object_header as u16,
                    reserved0: 0,
                    rights: (object_header >> 32) as u32,
                    backing_service: self.backing_service.load(Ordering::Relaxed),
                    slot: self.object_slot.load(Ordering::Relaxed),
                    generation: self.object_generation.load(Ordering::Relaxed),
                    pager_epoch: self.pager_epoch.load(Ordering::Relaxed),
                    backing_generation: self.backing_generation.load(Ordering::Relaxed),
                },
                reserved1: [0; 2],
            },
            endpoint: PagerEndpointCapabilityWire {
                slot: self.endpoint_slot.load(Ordering::Relaxed),
                generation: self.endpoint_generation.load(Ordering::Relaxed),
                rights: self.endpoint_rights.load(Ordering::Relaxed),
            },
            zeroed_frame_capability: self.zeroed_frame_capability.load(Ordering::Relaxed),
            granted_frame_rights: self.granted_frame_rights.load(Ordering::Relaxed) as u32,
            dispatch_reply_handle: self.dispatch_reply_handle.load(Ordering::Relaxed),
        }
    }

    fn clear(&self) {
        self.request_header.store(0, Ordering::Relaxed);
        self.process_handle.store(0, Ordering::Relaxed);
        self.process_generation.store(0, Ordering::Relaxed);
        self.task_id.store(0, Ordering::Relaxed);
        self.task_generation.store(0, Ordering::Relaxed);
        self.mm_generation.store(0, Ordering::Relaxed);
        self.vma_generation.store(0, Ordering::Relaxed);
        self.virtual_address.store(0, Ordering::Relaxed);
        self.object_offset.store(0, Ordering::Relaxed);
        self.deadline_ns.store(0, Ordering::Relaxed);
        self.scheduling_domain.store(0, Ordering::Relaxed);
        self.charge_token.store(0, Ordering::Relaxed);
        self.object_header.store(0, Ordering::Relaxed);
        self.backing_service.store(0, Ordering::Relaxed);
        self.object_slot.store(0, Ordering::Relaxed);
        self.object_generation.store(0, Ordering::Relaxed);
        self.pager_epoch.store(0, Ordering::Relaxed);
        self.backing_generation.store(0, Ordering::Relaxed);
        self.endpoint_slot.store(0, Ordering::Relaxed);
        self.endpoint_generation.store(0, Ordering::Relaxed);
        self.endpoint_rights.store(0, Ordering::Relaxed);
        self.zeroed_frame_capability.store(0, Ordering::Relaxed);
        self.granted_frame_rights.store(0, Ordering::Relaxed);
        self.dispatch_reply_handle.store(0, Ordering::Relaxed);
    }
}

struct PagerFaultTable {
    slots: [PagerFaultSlot; MAX_PAGER_FAULT_SLOTS],
}

impl PagerFaultTable {
    const fn new() -> Self {
        Self {
            slots: [const { PagerFaultSlot::empty() }; MAX_PAGER_FAULT_SLOTS],
        }
    }

    fn reserve(
        &self,
        request: PagerFaultRequestWire,
        endpoint: PagerEndpointCapabilityWire,
    ) -> Result<PagerFaultReservation, PagerFaultSlotError> {
        self.reserve_with_dispatch_grant(request, endpoint, false, |_, _| Ok((0, 0)))
    }

    /// Constructs token-bound dispatch authority before FaultPending becomes
    /// observable. The callback runs only after the canonical token exists,
    /// while the slot is still private in Initializing.
    fn reserve_with_dispatch_grant<F>(
        &self,
        mut request: PagerFaultRequestWire,
        endpoint: PagerEndpointCapabilityWire,
        require_dispatch_grant: bool,
        grant_factory: F,
    ) -> Result<PagerFaultReservation, PagerFaultSlotError>
    where
        F: FnOnce(u64, &PagerFaultRequestWire) -> Result<(u64, u32), PagerFaultSlotError>,
    {
        if request.fault_token != 0
            || request.version != PAGER_FAULT_ABI_VERSION
            || !endpoint.has_authority()
        {
            return Err(PagerFaultSlotError::Malformed);
        }

        let Some((index, slot)) = self.slots.iter().enumerate().find(|(_, slot)| {
            // ORDERING: Acquire on success pairs with the Release store that
            // freed this slot, so the winning claimant observes the previous
            // reservation's teardown before it writes any payload of its own.
            slot.state
                .compare_exchange(
                    SLOT_FREE,
                    SLOT_INITIALIZING,
                    // ORDERING: see above; Acquire claims the freed slot.
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
        }) else {
            return Err(PagerFaultSlotError::Pressure);
        };

        let previous_generation = slot.generation.load(Ordering::Relaxed);
        let Some(generation) = previous_generation.checked_add(1) else {
            // ORDERING: Release publishes the abandoned reservation, so the
            // next Acquire claimant cannot observe this half-initialised slot.
            slot.state.store(SLOT_FREE, Ordering::Release);
            return Err(PagerFaultSlotError::Pressure);
        };
        if generation > MAX_SLOT_GENERATION {
            // ORDERING: same Release rollback edge; an exhausted generation
            // must free the slot without publishing a usable token.
            slot.state.store(SLOT_FREE, Ordering::Release);
            return Err(PagerFaultSlotError::Pressure);
        }
        let slot_part = u64::try_from(index + 1).expect("pager fault slot index fits in u64");
        let token = (generation << SLOT_BITS) | slot_part;
        request.fault_token = token;
        if !request.is_canonical() {
            // ORDERING: a malformed request is rejected before publication;
            // Release frees the slot without exposing its stamped token.
            slot.state.store(SLOT_FREE, Ordering::Release);
            return Err(PagerFaultSlotError::Malformed);
        }

        let (zeroed_frame_capability, granted_frame_rights) = match grant_factory(token, &request) {
            Ok(grant) => grant,
            Err(error) => {
                // ORDERING: the grant factory failed, so Release returns the
                // slot before any frame authority becomes observable.
                slot.state.store(SLOT_FREE, Ordering::Release);
                return Err(error);
            }
        };
        if require_dispatch_grant && (zeroed_frame_capability == 0 || granted_frame_rights == 0) {
            // ORDERING: a dispatch reservation without a real grant is void;
            // Release frees the slot before a dispatcher could observe it.
            slot.state.store(SLOT_FREE, Ordering::Release);
            return Err(PagerFaultSlotError::Malformed);
        }

        slot.generation.store(generation, Ordering::Relaxed);
        slot.write_request(
            request,
            endpoint,
            zeroed_frame_capability,
            granted_frame_rights,
        );
        // ORDERING: the single Release publishes request, endpoint, and exact
        // token-bound frame grant as one indivisible dispatch reservation.
        slot.state.store(SLOT_FAULT_PENDING, Ordering::Release);
        Ok(PagerFaultReservation {
            token,
            state: PagerFaultState::FaultPending,
            request,
            endpoint,
            zeroed_frame_capability,
            granted_frame_rights,
            dispatch_reply_handle: 0,
        })
    }

    fn snapshot(&self, token: u64) -> Result<PagerFaultReservation, PagerFaultSlotError> {
        let (slot, generation) = self.slot_for_token(token)?;
        // ORDERING: this Acquire pairs with the reservation Release before
        // loading Relaxed payload fields; no reply authority exists for an
        // initializing or freed slot.
        let before = slot.state.load(Ordering::Acquire);
        let state = decode_state(before).ok_or(PagerFaultSlotError::Stale)?;
        if slot.generation.load(Ordering::Relaxed) != generation {
            return Err(PagerFaultSlotError::Stale);
        }
        let reservation = slot.read_reservation(token, state);
        // ORDERING: matching this second Acquire observation proves that a
        // consume/cancel transition did not invalidate the Relaxed fields while
        // this snapshot was assembled.
        let after = slot.state.load(Ordering::Acquire);
        (before == after)
            .then_some(reservation)
            .ok_or(PagerFaultSlotError::Stale)
    }

    fn mark_blocked(&self, token: u64) -> Result<(), PagerFaultSlotError> {
        let (slot, generation) = self.slot_for_token(token)?;
        // ORDERING: acquire the slot generation before accepting a blocked
        // transition, so a recycled index cannot turn an old fault token into
        // authority over a new request.
        if slot.generation.load(Ordering::Acquire) != generation {
            return Err(PagerFaultSlotError::Stale);
        }
        // ORDERING: the fused scheduler handoff invokes this AcqRel transition
        // after it has accepted the exact reply wait and before it releases the
        // CPU. A reply claimant therefore cannot observe BlockedOnPager first.
        // ORDERING: this AcqRel CAS is the one transition that publishes the
        // scheduler-coupled pager-blocked authority to a reply claimant.
        slot.state
            .compare_exchange(
                SLOT_FAULT_PENDING,
                SLOT_BLOCKED_ON_PAGER,
                // ORDERING: AcqRel commits the scheduler-coupled blocked
                // authority before either the faulting task or pagerd runs.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|state| state_error(state))
    }

    /// Claims one exact blocked fault for normal-time delivery to pagerd.
    ///
    /// Exception entry never calls this function. The dispatcher observes the
    /// complete request only after the faulting task has committed its typed
    /// wait, and a reply must later prove this dispatch claim before mapping.
    fn take_next_dispatchable(&self) -> Option<PagerFaultReservation> {
        for (index, slot) in self.slots.iter().enumerate() {
            // ORDERING: Acquire observes the completed blocked transition and
            // its preceding fault request publication before this dispatcher
            // may attempt to take ownership of the token.
            if slot.state.load(Ordering::Acquire) != SLOT_BLOCKED_ON_PAGER {
                continue;
            }
            // ORDERING: Acquire rejects a slot whose generation was recycled
            // while this bounded scan advanced from another table entry.
            let generation = slot.generation.load(Ordering::Acquire);
            if generation == 0 {
                continue;
            }
            if slot
                .state
                .compare_exchange(
                    SLOT_BLOCKED_ON_PAGER,
                    SLOT_DISPATCHED_TO_PAGER,
                    // ORDERING: AcqRel transfers the fully published request
                    // from the blocked fault owner to exactly one dispatcher.
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            let slot_part = u64::try_from(index + 1).ok()?;
            let token = (generation << SLOT_BITS) | slot_part;
            return self.snapshot(token).ok();
        }
        None
    }

    fn bind_dispatch_reply(
        &self,
        token: u64,
        reply_handle: u64,
    ) -> Result<(), PagerFaultSlotError> {
        if reply_handle == 0 {
            return Err(PagerFaultSlotError::Malformed);
        }
        let (slot, generation) = self.slot_for_token(token)?;
        // ORDERING: Acquire rejects a recycled slot before this binding can
        // attach a reply handle to a newer reservation's token.
        if slot.generation.load(Ordering::Acquire) != generation {
            return Err(PagerFaultSlotError::Stale);
        }
        // ORDERING: Acquire observes the dispatcher's published transition, so
        // only a slot already handed to the pager accepts a reply handle.
        if slot.state.load(Ordering::Acquire) != SLOT_DISPATCHED_TO_PAGER {
            return Err(state_error(slot.state.load(Ordering::Acquire)));
        }
        // ORDERING: Release publishes the handle to the response poller, and
        // the Acquire failure edge keeps a losing racer from double-binding.
        slot.dispatch_reply_handle
            .compare_exchange(0, reply_handle, Ordering::Release, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| PagerFaultSlotError::Transition)
    }

    fn next_dispatched_with_reply(&self) -> Option<PagerFaultReservation> {
        for (index, slot) in self.slots.iter().enumerate() {
            // ORDERING: both Acquire loads pair with the dispatcher's Release
            // publications, so a slot is only polled once its state and reply
            // handle are jointly visible; a half-published slot is skipped.
            if slot.state.load(Ordering::Acquire) != SLOT_DISPATCHED_TO_PAGER
                || slot.dispatch_reply_handle.load(Ordering::Acquire) == 0
            {
                continue;
            }
            // ORDERING: Acquire pins the generation this scan will encode into
            // the token, so a concurrent recycle cannot alias a stale token.
            let generation = slot.generation.load(Ordering::Acquire);
            if generation == 0 {
                continue;
            }
            let slot_part = u64::try_from(index + 1).ok()?;
            let token = (generation << SLOT_BITS) | slot_part;
            if let Ok(reservation) = self.snapshot(token) {
                return Some(reservation);
            }
        }
        None
    }

    fn claim_reply(&self, token: u64) -> Result<PagerFaultReservation, PagerFaultSlotError> {
        let (slot, generation) = self.slot_for_token(token)?;
        // ORDERING: this acquire rejects a response for an earlier slot
        // generation before it can race the one-shot reply claimant.
        if slot.generation.load(Ordering::Acquire) != generation {
            return Err(PagerFaultSlotError::Stale);
        }
        // ORDERING: AcqRel elects exactly one reply/cancel winner. A stale
        // pager response can therefore never obtain mapping authority twice.
        // ORDERING: this AcqRel CAS lets only one exact blocked token become
        // ReplyClaimed while every losing response observes the terminal state.
        slot.state
            .compare_exchange(
                SLOT_DISPATCHED_TO_PAGER,
                SLOT_REPLY_CLAIMED,
                // ORDERING: AcqRel elects one reply claimant for this exact
                // token before any mapping transaction can begin.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(state_error)?;
        self.snapshot(token)
    }

    /// Claims cancellation before any wake or frame revocation is published.
    ///
    /// The claim is deliberately non-terminal: payload identity remains live
    /// while the exact task wake and any associated authority rollback run.
    /// Only the claimant may subsequently release `CancelClaimed` to `Free`.
    fn claim_cancellation(
        &self,
        token: u64,
        expected: PagerFaultState,
    ) -> Result<PagerFaultReservation, PagerFaultSlotError> {
        if !matches!(
            expected,
            PagerFaultState::FaultPending
                | PagerFaultState::BlockedOnPager
                | PagerFaultState::DispatchedToPager
        ) {
            return Err(PagerFaultSlotError::Transition);
        }
        let (slot, generation) = self.slot_for_token(token)?;
        // ORDERING: Acquire rejects a recycled slot, so a stale token cannot
        // drive the terminal transition of a newer reservation.
        if slot.generation.load(Ordering::Acquire) != generation {
            return Err(PagerFaultSlotError::Stale);
        }
        slot.state
            .compare_exchange(
                encode_state(expected),
                SLOT_CANCEL_CLAIMED,
                // ORDERING: AcqRel elects cancellation against dispatch and
                // reply claim before any wake can expose the terminal result.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(state_error)?;
        self.snapshot(token)
    }

    fn release(&self, token: u64, expected: PagerFaultState) -> Result<(), PagerFaultSlotError> {
        let (slot, generation) = self.slot_for_token(token)?;
        // ORDERING: this acquire rejects stale cancel/consume authority before
        // the terminal state CAS clears the slot for a future generation.
        if slot.generation.load(Ordering::Acquire) != generation {
            return Err(PagerFaultSlotError::Stale);
        }
        let expected = encode_state(expected);
        // ORDERING: this AcqRel CAS elects the consume/cancel winner before
        // clear() removes payload authority and releases the next generation.
        slot.state
            .compare_exchange(
                expected,
                SLOT_INITIALIZING,
                // ORDERING: AcqRel elects a terminal consume/cancel winner
                // before the Relaxed payload clear below.
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(state_error)?;
        slot.clear();
        // ORDERING: clear every Relaxed payload field before Free is released;
        // the next generation's reservation cannot inherit a prior fault's
        // endpoint, request, or frame authority.
        slot.state.store(SLOT_FREE, Ordering::Release);
        Ok(())
    }

    fn slot_for_token(&self, token: u64) -> Result<(&PagerFaultSlot, u64), PagerFaultSlotError> {
        let slot_part = token & SLOT_MASK;
        let generation = token >> SLOT_BITS;
        if token == 0 || slot_part == 0 || generation == 0 {
            return Err(PagerFaultSlotError::Stale);
        }
        let index = usize::try_from(slot_part - 1).map_err(|_| PagerFaultSlotError::Stale)?;
        let slot = self.slots.get(index).ok_or(PagerFaultSlotError::Stale)?;
        Ok((slot, generation))
    }
}

fn decode_state(raw: u64) -> Option<PagerFaultState> {
    match raw {
        SLOT_FAULT_PENDING => Some(PagerFaultState::FaultPending),
        SLOT_BLOCKED_ON_PAGER => Some(PagerFaultState::BlockedOnPager),
        SLOT_DISPATCHED_TO_PAGER => Some(PagerFaultState::DispatchedToPager),
        SLOT_REPLY_CLAIMED => Some(PagerFaultState::ReplyClaimed),
        SLOT_CANCEL_CLAIMED => Some(PagerFaultState::CancelClaimed),
        _ => None,
    }
}

const fn encode_state(state: PagerFaultState) -> u64 {
    match state {
        PagerFaultState::FaultPending => SLOT_FAULT_PENDING,
        PagerFaultState::BlockedOnPager => SLOT_BLOCKED_ON_PAGER,
        PagerFaultState::DispatchedToPager => SLOT_DISPATCHED_TO_PAGER,
        PagerFaultState::ReplyClaimed => SLOT_REPLY_CLAIMED,
        PagerFaultState::CancelClaimed => SLOT_CANCEL_CLAIMED,
    }
}

fn state_error(state: u64) -> PagerFaultSlotError {
    if state == SLOT_FREE || state == SLOT_INITIALIZING {
        PagerFaultSlotError::Stale
    } else {
        PagerFaultSlotError::Transition
    }
}

static PAGER_FAULTS: PagerFaultTable = PagerFaultTable::new();

/// Reserve one fixed, generation-bound pager-fault slot without allocating.
pub fn reserve_pager_fault(
    request: PagerFaultRequestWire,
    endpoint: PagerEndpointCapabilityWire,
) -> Result<PagerFaultReservation, PagerFaultSlotError> {
    PAGER_FAULTS.reserve(request, endpoint)
}

/// Reserve one fault together with the exact pre-zeroed frame grant that its
/// eventual pager dispatch may return. The grant callback executes before the
/// fault becomes visible to cancellation, dispatch, or reply paths.
pub fn reserve_pager_fault_with_dispatch_grant<F>(
    request: PagerFaultRequestWire,
    endpoint: PagerEndpointCapabilityWire,
    grant_factory: F,
) -> Result<PagerFaultReservation, PagerFaultSlotError>
where
    F: FnOnce(u64, &PagerFaultRequestWire) -> Result<(u64, u32), PagerFaultSlotError>,
{
    PAGER_FAULTS.reserve_with_dispatch_grant(request, endpoint, true, grant_factory)
}

/// Snapshot a still-live pager fault token for diagnostics or reply validation.
pub fn pager_fault_snapshot(token: u64) -> Result<PagerFaultReservation, PagerFaultSlotError> {
    PAGER_FAULTS.snapshot(token)
}

/// Commit the `FaultPending -> BlockedOnPager` ownership transition.
pub fn mark_pager_fault_blocked(token: u64) -> Result<(), PagerFaultSlotError> {
    PAGER_FAULTS.mark_blocked(token)
}

/// Returns one normal-time pager dispatch claim, if a faulting task has
/// already committed its exact `PagerFault(token)` wait.
pub fn take_next_pager_fault_for_dispatch() -> Option<PagerFaultReservation> {
    PAGER_FAULTS.take_next_dispatchable()
}

pub fn bind_pager_fault_dispatch_reply(
    token: u64,
    reply_handle: u64,
) -> Result<(), PagerFaultSlotError> {
    PAGER_FAULTS.bind_dispatch_reply(token, reply_handle)
}

pub fn next_dispatched_pager_fault_response() -> Option<PagerFaultReservation> {
    PAGER_FAULTS.next_dispatched_with_reply()
}

/// Claim the reply authority exactly once before a PTE transaction begins.
pub fn claim_pager_fault_reply(token: u64) -> Result<PagerFaultReservation, PagerFaultSlotError> {
    PAGER_FAULTS.claim_reply(token)
}

/// Consume a reply claimant after its mapping transaction commits or rolls back.
pub fn consume_pager_fault_reply(token: u64) -> Result<(), PagerFaultSlotError> {
    PAGER_FAULTS.release(token, PagerFaultState::ReplyClaimed)
}

/// Cancel a fault whose IPC, deadline, unmap, exec, or exit path won the race.
///
/// Cancellation claims slot custody before waking the exact task. A reply that
/// already claimed the slot therefore prevents an early resume, while a block
/// commit racing this claim observes either the wake or `CancelClaimed` and
/// performs its local owner-word rollback.
pub fn cancel_pager_fault(token: u64, state: PagerFaultState) -> Result<(), PagerFaultSlotError> {
    let claimed = PAGER_FAULTS.claim_cancellation(token, state)?;
    let _ = super::current::wake_task(claimed.request.task_id);
    PAGER_FAULTS.release(token, PagerFaultState::CancelClaimed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::pager::{
        PAGER_PAGE_BYTES, VM_ACCESS_READ, VM_OBJECT_ANONYMOUS, VM_PROT_READ,
    };

    fn request() -> PagerFaultRequestWire {
        PagerFaultRequestWire {
            version: PAGER_FAULT_ABI_VERSION,
            access: VM_ACCESS_READ,
            fault_flags: 0,
            reserved0: 0,
            fault_token: 0,
            process_handle: 7,
            process_generation: 11,
            task_id: 13,
            task_generation: 17,
            mm_generation: 19,
            vma_generation: 23,
            virtual_address: 0x4000,
            object_offset: 0,
            deadline_ns: 29,
            scheduling_domain: 31,
            charge_token: 37,
            object: PagerObjectIdentityWire {
                object_type: VM_OBJECT_ANONYMOUS,
                reserved0: 0,
                rights: VM_PROT_READ,
                backing_service: 0,
                slot: 41,
                generation: 43,
                pager_epoch: 47,
                backing_generation: 53,
            },
            reserved1: [0; 2],
        }
    }

    fn endpoint() -> PagerEndpointCapabilityWire {
        PagerEndpointCapabilityWire {
            slot: 59,
            generation: 61,
            rights: 1,
        }
    }

    #[test]
    fn fault_slot_is_exact_and_consumed_once() {
        let table = PagerFaultTable::new();
        let reservation = table
            .reserve_with_dispatch_grant(request(), endpoint(), true, |token, request| {
                assert_ne!(token, 0);
                assert_eq!(request.fault_token, token);
                Ok((0xfeed, VM_PROT_READ))
            })
            .expect("reserve fault");
        assert_eq!(reservation.zeroed_frame_capability, 0xfeed);
        assert_eq!(reservation.granted_frame_rights, VM_PROT_READ);
        assert!(reservation.request.is_canonical());
        assert_eq!(reservation.state, PagerFaultState::FaultPending);
        assert_eq!(reservation.request.virtual_address % PAGER_PAGE_BYTES, 0);
        assert_eq!(
            table
                .snapshot(reservation.token)
                .expect("snapshot")
                .endpoint,
            endpoint()
        );

        table.mark_blocked(reservation.token).expect("block fault");
        let dispatched = table
            .take_next_dispatchable()
            .expect("dispatch exact blocked fault");
        assert_eq!(dispatched.token, reservation.token);
        assert_eq!(dispatched.state, PagerFaultState::DispatchedToPager);
        assert_eq!(dispatched.dispatch_reply_handle, 0);
        table
            .bind_dispatch_reply(reservation.token, 0xbeef)
            .expect("bind exact IPC reply");
        assert_eq!(
            table
                .next_dispatched_with_reply()
                .expect("poll dispatched response")
                .dispatch_reply_handle,
            0xbeef
        );
        let claimed = table.claim_reply(reservation.token).expect("claim reply");
        assert_eq!(claimed.state, PagerFaultState::ReplyClaimed);
        assert_eq!(claimed.request, reservation.request);
        table
            .release(reservation.token, PagerFaultState::ReplyClaimed)
            .expect("consume fault");
        assert_eq!(
            table.snapshot(reservation.token),
            Err(PagerFaultSlotError::Stale)
        );
    }

    #[test]
    fn reused_slot_rejects_old_fault_token() {
        let table = PagerFaultTable::new();
        let old = table.reserve(request(), endpoint()).expect("first reserve");
        table
            .release(old.token, PagerFaultState::FaultPending)
            .expect("cancel first");
        let current = table
            .reserve(request(), endpoint())
            .expect("second reserve");
        assert_ne!(current.token, old.token);
        assert_eq!(
            table.mark_blocked(old.token),
            Err(PagerFaultSlotError::Stale)
        );
        assert!(table.mark_blocked(current.token).is_ok());
    }

    #[test]
    fn cancellation_wins_over_late_reply_claim() {
        let table = PagerFaultTable::new();
        let reservation = table.reserve(request(), endpoint()).expect("reserve fault");
        table.mark_blocked(reservation.token).expect("block fault");
        let cancelled = table
            .claim_cancellation(reservation.token, PagerFaultState::BlockedOnPager)
            .expect("claim cancellation");
        assert_eq!(cancelled.state, PagerFaultState::CancelClaimed);
        assert_eq!(
            table.claim_reply(reservation.token),
            Err(PagerFaultSlotError::Transition)
        );
        table
            .release(reservation.token, PagerFaultState::CancelClaimed)
            .expect("consume cancellation");
        assert_eq!(
            table.claim_reply(reservation.token),
            Err(PagerFaultSlotError::Stale)
        );
    }

    #[test]
    fn malformed_template_never_publishes_authority() {
        let table = PagerFaultTable::new();
        let mut malformed = request();
        malformed.virtual_address = 0x4001;
        assert_eq!(
            table.reserve(malformed, endpoint()),
            Err(PagerFaultSlotError::Malformed)
        );
        let mut no_authority = endpoint();
        no_authority.rights = 0;
        assert_eq!(
            table.reserve(request(), no_authority),
            Err(PagerFaultSlotError::Malformed)
        );
    }

    #[test]
    fn reply_cannot_claim_a_task_that_is_not_blocked_on_pager() {
        let table = PagerFaultTable::new();
        let reservation = table.reserve(request(), endpoint()).expect("reserve fault");
        assert_eq!(
            table.claim_reply(reservation.token),
            Err(PagerFaultSlotError::Transition)
        );
    }

    #[test]
    fn dispatch_never_takes_a_slot_before_its_task_has_blocked() {
        let table = PagerFaultTable::new();
        let reservation = table.reserve(request(), endpoint()).expect("reserve fault");
        // Still FaultPending: the faulting task has not yet committed its
        // block, so publishing this request to the pager would let a reply
        // race a task that is still running on its own stack.
        assert!(table.take_next_dispatchable().is_none());
        table
            .mark_blocked(reservation.token)
            .expect("commit the block");
        let dispatched = table
            .take_next_dispatchable()
            .expect("a blocked fault is dispatchable");
        assert_eq!(dispatched.token, reservation.token);
        assert_eq!(dispatched.state, PagerFaultState::DispatchedToPager);
        // Exactly one dispatcher may own a request.
        assert!(table.take_next_dispatchable().is_none());
    }
}
