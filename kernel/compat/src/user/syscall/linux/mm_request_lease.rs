//! Kernel-issued lease binding one live address-space request to its MM.
//!
//! - **Owner:** Compat. `syscalld` decides placement and protection, but the
//!   authority to mutate a process is the kernel's own record that *this exact
//!   caller task is still blocked in this exact request* against the process and
//!   MM generations it had when it asked.
//! - **Boundary:** `syscalld` echoes the ticket it received. The echo is
//!   untrusted: it is honoured only if the ticket names a live lease whose pid is
//!   the broker's `target_pid`. A guessed, stale, cross-process, or zero ticket
//!   confers nothing.
//! - **Lifecycle:** the caller opens a lease before the IPC call and closes it
//!   when the call returns for any reason (reply, timeout, interruption). Close
//!   first retires the ticket so no new apply can start, then waits for appliers
//!   that already passed validation. After close returns the request has no
//!   further effect: a timed-out `mmap` cannot land late, and a late `munmap`
//!   cannot hit the image `exec` installed meanwhile.
//! - **Concurrency:** lock-free slots. An apply increments `busy`, then
//!   re-reads the ticket; close stores a non-matching sentinel before waiting
//!   for `busy` to reach zero, so either the applier sees the retirement or
//!   close sees the applier.
//! - **Failure:** table exhaustion refuses the request (`EAGAIN`) instead of
//!   issuing an unbound ticket. Nothing here retries or falls back to by-PID.
//! - **Forbidden:** PID-only authority, tickets that outlive the call, and any
//!   wait on IPC or the process-state lock while holding a slot claim.
//! - **Evidence:** the tests below; a timed-out request's effects are *not*
//!   rolled back (the outcome of a timed-out call is unknown, as for any IPC).

use alloc::boxed::Box;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Slots per dynamically allocated lease chunk.
const LEASE_CHUNK_LEN: usize = 64;
/// Maximum number of lease chunks (262,144 concurrent in-flight requests).
const MAX_LEASE_CHUNKS: usize = 4096;
const LEASE_INDEX_BITS: u32 = 20;
const LEASE_INDEX_MASK: u64 = (1_u64 << LEASE_INDEX_BITS) - 1;
/// Fairness throttle: most concurrent in-flight mm requests one pid may hold so
/// a single runaway process cannot exhaust all chunks before reclaiming others.
const MAX_LEASES_PER_PID: usize = 256;
const FREE: u64 = 0;
/// A slot being filled or drained. Never equals a real ticket.
const BUSY_SENTINEL: u64 = u64::MAX;

/// Exact incarnation a request is bound to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MmTarget {
    pub(super) pid: u64,
    pub(super) process_generation: u32,
    pub(super) mm_generation: u32,
}

impl MmTarget {
    /// The exact incarnation, for the process-state primitives that revalidate
    /// it under their own lock.
    pub(super) fn expected(self) -> crate::multitask::ExpectedMm {
        crate::multitask::ExpectedMm {
            process_generation: self.process_generation,
            mm_generation: self.mm_generation,
        }
    }
}

struct Slot {
    ticket: AtomicU64,
    pid: AtomicU64,
    generations: AtomicU64,
    /// Appliers between validation and completion. Balanced: every increment is
    /// matched by a decrement, including failed validations.
    appliers: AtomicU32,
}

impl Slot {
    const fn new() -> Self {
        Self {
            ticket: AtomicU64::new(FREE),
            pid: AtomicU64::new(0),
            generations: AtomicU64::new(0),
            appliers: AtomicU32::new(0),
        }
    }
}

struct LeaseChunk {
    slots: [Slot; LEASE_CHUNK_LEN],
    claimed_mask: AtomicU64,
}

impl LeaseChunk {
    const fn new() -> Self {
        Self {
            slots: [const { Slot::new() }; LEASE_CHUNK_LEN],
            claimed_mask: AtomicU64::new(0),
        }
    }
}

static LEASE_CHUNKS: [AtomicPtr<LeaseChunk>; MAX_LEASE_CHUNKS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_LEASE_CHUNKS];
static LEASE_CHUNK_COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

const _: () = assert!(
    MAX_LEASE_CHUNKS * LEASE_CHUNK_LEN <= (1 << LEASE_INDEX_BITS)
        && LEASE_CHUNK_LEN == 64
        && BUSY_SENTINEL == u64::MAX
);

#[inline]
fn lease_chunk_ref(chunk_idx: usize) -> Option<&'static LeaseChunk> {
    // ORDERING: Acquire pairs with AcqRel CAS chunk publication in `try_open_in`.
    let ptr = LEASE_CHUNKS.get(chunk_idx)?.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: published lease chunks remain allocated for `'static`.
    Some(unsafe { &*ptr })
}

#[inline]
fn lease_slot_ref(index: usize) -> Option<(&'static LeaseChunk, &'static Slot, usize)> {
    let chunk = lease_chunk_ref(index >> 6)?;
    let offset = index & 63;
    Some((chunk, &chunk.slots[offset], offset))
}

fn claim_mask_set(chunk: &LeaseChunk, offset: usize) {
    // ORDERING: SeqCst so the claim bit is in the single total order with the SeqCst
    // pid store and the holders_of_pid scan (store-then-scan, Dekker style): of two
    // same-pid openers at least one observes the other's claim.
    chunk.claimed_mask.fetch_or(1_u64 << offset, Ordering::SeqCst);
}

fn claim_mask_clear(chunk: &LeaseChunk, offset: usize) {
    // ORDERING: Release only needs to order this slot's earlier accesses before the
    // bit drops; a stale-set bit is harmless because holders_of_pid also requires a
    // non-FREE ticket, and the slot is freed only after this clear.
    chunk.claimed_mask.fetch_and(!(1_u64 << offset), Ordering::Release);
}

enum OpenRefusal {
    PerPidLimit,
    TableFull,
}

fn pack(process_generation: u32, mm_generation: u32) -> u64 {
    (u64::from(process_generation) << 32) | u64::from(mm_generation)
}

/// Open lease held by the caller across its `syscalld` call.
pub(super) struct RequestLease {
    index: usize,
    ticket: u64,
}

impl RequestLease {
    /// Opens a lease for `target`; `None` when every slot is in flight.
    ///
    /// A caller whose task was destroyed while blocked never runs its `Drop`,
    /// so a full table first reclaims leases whose process/MM incarnation is no
    /// longer live (their tickets already authorize nothing, because every
    /// effect revalidates the generations) and then retries once.
    pub(super) fn open(target: MmTarget) -> Option<Self> {
        Self::open_with(target, |dead| {
            crate::multitask::with_process_state_by_generation_mut(
                dead.pid,
                dead.process_generation,
                dead.mm_generation,
                |_| (),
            )
            .is_some()
        })
    }

    fn open_with(target: MmTarget, is_live: impl Fn(MmTarget) -> bool) -> Option<Self> {
        Self::open_with_chunks(target, is_live, MAX_LEASE_CHUNKS)
    }

    fn open_with_chunks(
        target: MmTarget,
        is_live: impl Fn(MmTarget) -> bool,
        max_chunks: usize,
    ) -> Option<Self> {
        match Self::try_open_in(target, max_chunks) {
            Ok(lease) => Some(lease),
            Err(OpenRefusal::PerPidLimit) => {
                reclaim_dead(&is_live, Some(target.pid), max_chunks);
                Self::try_open_in(target, max_chunks).ok()
            }
            Err(OpenRefusal::TableFull) => {
                reclaim_dead(&is_live, None, max_chunks);
                Self::try_open_in(target, max_chunks).ok()
            }
        }
    }

    fn claim_opened_slot(
        target: MmTarget,
        chunk: &LeaseChunk,
        slot: &Slot,
        offset: usize,
        index: usize,
        max_chunks: usize,
    ) -> Result<Self, OpenRefusal> {
        let ticket = loop {
            // Encode `index` in the low bits so `begin_apply` is O(1).
            let seq = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
            let candidate = (seq << LEASE_INDEX_BITS) | (index as u64);
            if candidate != FREE && candidate != BUSY_SENTINEL {
                break candidate;
            }
        };
        // ORDERING: mark bit and announce pid (SeqCst) while the slot is
        // held by the sentinel, then scan claimed slots. Two same-pid
        // openers each store then scan, so at least one sees the other.
        claim_mask_set(chunk, offset);
        slot.pid.store(target.pid, Ordering::SeqCst);
        if holders_of_pid(target.pid, index, max_chunks) >= MAX_LEASES_PER_PID {
            claim_mask_clear(chunk, offset);
            // ORDERING: Release returns the slot to FREE after the claim bit was cleared;
            // pairs with the Acquire CAS in try_open_in.
            slot.ticket.store(FREE, Ordering::Release);
            return Err(OpenRefusal::PerPidLimit);
        }
        slot.generations.store(
            pack(target.process_generation, target.mm_generation),
            Ordering::Relaxed,
        );
        // ORDERING: Release publishes pid/generations with the ticket.
        slot.ticket.store(ticket, Ordering::Release);
        Ok(Self { index, ticket })
    }

    fn try_open_in(target: MmTarget, max_chunks: usize) -> Result<Self, OpenRefusal> {
        let limit = max_chunks.min(MAX_LEASE_CHUNKS);
        let mut chunk_idx = 0usize;
        while chunk_idx < limit {
            let Some(chunk) = lease_chunk_ref(chunk_idx) else {
                let mut fresh = Box::new(LeaseChunk::new());
                // Reserve slot 0 while unshared before publishing the chunk pointer.
                fresh.slots[0].ticket = AtomicU64::new(BUSY_SENTINEL);
                let raw = Box::into_raw(fresh);
                // ORDERING: AcqRel publishes initialized chunk; Acquire reads concurrent winner.
                match LEASE_CHUNKS[chunk_idx].compare_exchange(
                    ptr::null_mut(),
                    raw,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // ORDERING: SeqCst publishes chunk count into total order before pid check.
                        LEASE_CHUNK_COUNT.fetch_max(chunk_idx + 1, Ordering::SeqCst);
                        // SAFETY: published chunk remains live for `'static`.
                        let chunk = unsafe { &*raw };
                        let index = chunk_idx << 6;
                        return Self::claim_opened_slot(
                            target,
                            chunk,
                            &chunk.slots[0],
                            0,
                            index,
                            limit,
                        );
                    }
                    Err(_) => {
                        // SAFETY: lost publication race before anyone could observe `raw`.
                        drop(unsafe { Box::from_raw(raw) });
                        // ORDERING: SeqCst matches winning publisher bound update.
                        LEASE_CHUNK_COUNT.fetch_max(chunk_idx + 1, Ordering::SeqCst);
                        continue;
                    }
                }
            };
            for (offset, slot) in chunk.slots.iter().enumerate() {
                if slot
                    .ticket
                    // ORDERING: Acquire on the winning CAS pairs with the Release FREE store of the
                    // previous owner (drain_and_free), so its slot writes cannot overlap ours; a
                    // failed CAS reads nothing, hence Relaxed.
                    .compare_exchange(FREE, BUSY_SENTINEL, Ordering::Acquire, Ordering::Relaxed)
                    .is_err()
                {
                    continue;
                }
                let index = (chunk_idx << 6) | offset;
                return Self::claim_opened_slot(target, chunk, slot, offset, index, limit);
            }
            chunk_idx += 1;
        }
        Err(OpenRefusal::TableFull)
    }

    pub(super) fn ticket(&self) -> u64 {
        self.ticket
    }
}

/// Slots other than `skip` that are claimed (live, filling or draining) and
/// name `pid`. The pid is read before the ticket so a slot whose claimant has
/// already announced `pid` is seen as claimed.
fn holders_of_pid(pid: u64, skip: usize, max_chunks: usize) -> usize {
    let mut count = 0_usize;
    // ORDERING: Acquire reads published chunk count paired with SeqCst publication.
    let chunks = LEASE_CHUNK_COUNT.load(Ordering::Acquire).min(max_chunks);
    for chunk_idx in 0..chunks {
        let Some(chunk) = lease_chunk_ref(chunk_idx) else {
            continue;
        };
        // ORDERING: SeqCst load of the claim mask completes the store-then-scan pattern
        // documented in claim_opened_slot: the opener's SeqCst bit/pid stores precede this scan.
        let mut bits = chunk.claimed_mask.load(Ordering::SeqCst);
        while bits != 0 {
            let offset = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let index = (chunk_idx << 6) | offset;
            if index != skip
                // ORDERING: SeqCst pid load is the second half of the store-then-scan in
                // claim_opened_slot; it must be ordered after the claim-bit load above.
                && chunk.slots[offset].pid.load(Ordering::SeqCst) == pid
                // ORDERING: SeqCst so a slot whose claimant already announced its pid is seen
                // as claimed in the same total order (see claim_opened_slot).
                && chunk.slots[offset].ticket.load(Ordering::SeqCst) != FREE
            {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
impl RequestLease {
    /// Step one of close: no apply may begin after this returns. False when
    /// the lease was already reclaimed (its incarnation died first).
    fn retire(&self) -> bool {
        let (_, slot, _) = lease_slot_ref(self.index).expect("live lease chunk");
        retire(slot, self.ticket)
    }

    /// Appliers that validated before retirement and have not finished.
    fn active_appliers(&self) -> u32 {
        let (_, slot, _) = lease_slot_ref(self.index).expect("live lease chunk");
        // ORDERING: test-only observation of the applier count; SeqCst matches the
        // protocol's own accesses to this counter.
        slot.appliers.load(Ordering::SeqCst)
    }
}

/// Retires exactly `ticket` in `slot` so no apply may begin afterwards. False
/// when the slot no longer holds that ticket (reclaimed or recycled).
fn retire(slot: &Slot, ticket: u64) -> bool {
    slot.ticket
        // ORDERING: SeqCst on both outcomes: the retirement store must be ordered
        // before drain_and_free's SeqCst load of appliers, pairing with begin_apply's
        // fetch_add then ticket re-read, so either the applier sees the sentinel or
        // close sees the applier.
        .compare_exchange(ticket, BUSY_SENTINEL, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Waits for appliers that validated before retirement, then frees the slot.
/// The slot must already be retired by the caller (it holds the sentinel).
///
/// Appliers run a bounded kernel mutation (no IPC, no waits on this lease), so
/// the wait terminates. Callers must hold no IPC or process-state lock: an
/// applier may need either to finish. `yield_cpu` lets the applier's CPU make
/// progress in normal syscall context instead of burning this one.
fn drain_and_free(chunk: &LeaseChunk, offset: usize, slot: &Slot, yield_cpu: bool) {
    // ORDERING: SeqCst load of appliers after retire's SeqCst CAS: it cannot miss an
    // applier whose fetch_add preceded that applier's ticket re-read (Dekker).
    while slot.appliers.load(Ordering::SeqCst) != 0 {
        if yield_cpu {
            #[cfg(not(test))]
            crate::multitask::yield_now();
            #[cfg(test)]
            core::hint::spin_loop();
        } else {
            core::hint::spin_loop();
        }
    }
    claim_mask_clear(chunk, offset);
    // ORDERING: Release publishes the drained, unclaimed slot as FREE; pairs with
    // the Acquire CAS in try_open_in, and orders the preceding claim_mask_clear.
    slot.ticket.store(FREE, Ordering::Release);
}

/// Retire `ticket` and, if this call won the retirement, drain and free the
/// slot. Returns false (touching nothing) when the slot was not ours to retire.
fn retire_and_drain(
    chunk: &LeaseChunk,
    offset: usize,
    slot: &Slot,
    ticket: u64,
    yield_cpu: bool,
) -> bool {
    if !retire(slot, ticket) {
        return false;
    }
    drain_and_free(chunk, offset, slot, yield_cpu);
    true
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        // Retire first: appliers arriving from here on fail validation, then
        // drain those that validated earlier. A false result means the lease
        // was reclaimed after this owner's incarnation died; the slot belongs
        // to someone else now and is not ours to retire or free.
        let (chunk, slot, offset) = lease_slot_ref(self.index).expect("live lease chunk");
        retire_and_drain(chunk, offset, slot, self.ticket, true);
    }
}

/// Frees leases whose owner can no longer run (its incarnation is gone).
fn reclaim_dead(is_live: &impl Fn(MmTarget) -> bool, pid_filter: Option<u64>, max_chunks: usize) {
    // ORDERING: Acquire reads published chunk count paired with SeqCst publication.
    let chunks = LEASE_CHUNK_COUNT.load(Ordering::Acquire).min(max_chunks);
    for chunk_idx in 0..chunks {
        let Some(chunk) = lease_chunk_ref(chunk_idx) else {
            continue;
        };
        // ORDERING: Acquire pairs with the SeqCst/Release claim-bit updates, so the slot's
        // ticket we examine below is at least as new as the bit we observed.
        let mut bits = chunk.claimed_mask.load(Ordering::Acquire);
        while bits != 0 {
            let offset = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let slot = &chunk.slots[offset];
            // ORDERING: Acquire pairs with the Release ticket store in claim_opened_slot, making the
            // slot's pid/generations (Relaxed reads below) visible. A slot recycled
            // meanwhile has another ticket, so retire_and_drain's exact-ticket CAS fails.
            let ticket = slot.ticket.load(Ordering::Acquire);
            if ticket == FREE || ticket == BUSY_SENTINEL {
                continue;
            }
            let pid = slot.pid.load(Ordering::Relaxed);
            if let Some(target_pid) = pid_filter
                && pid != target_pid
            {
                continue;
            }
            let generations = slot.generations.load(Ordering::Relaxed);
            let target = MmTarget {
                pid,
                process_generation: (generations >> 32) as u32,
                mm_generation: generations as u32,
            };
            if is_live(target) {
                continue;
            }
            // Retire only the lease observed above: a slot recycled meanwhile
            // carries a different ticket and is left alone.
            retire_and_drain(chunk, offset, slot, ticket, false);
        }
    }
}

/// Held for the duration of one broker effect; proves the lease was live when
/// the effect began and keeps the caller's close from completing until it ends.
pub(super) struct ApplyGuard {
    index: usize,
    target: MmTarget,
}

impl ApplyGuard {
    pub(super) fn target(&self) -> MmTarget {
        self.target
    }
}

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        let (_, slot, _) = lease_slot_ref(self.index).expect("live lease chunk");
        // ORDERING: SeqCst decrement pairs with drain_and_free's SeqCst load; it also
        // releases the effect's memory writes to the closer that frees the slot.
        slot.appliers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why a broker operation was refused before touching anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LeaseRefusal {
    /// No live request owns this ticket (never issued, retired, or zero).
    NoRequest,
    /// The ticket is live but belongs to a different process.
    WrongProcess,
}

/// Validates `ticket` for `target_pid` and begins one effect.
pub(super) fn begin_apply(ticket: u64, target_pid: u64) -> Result<ApplyGuard, LeaseRefusal> {
    if ticket == FREE || ticket == BUSY_SENTINEL {
        return Err(LeaseRefusal::NoRequest);
    }
    let index = (ticket & LEASE_INDEX_MASK) as usize;
    let Some((_, slot, _)) = lease_slot_ref(index) else {
        return Err(LeaseRefusal::NoRequest);
    };
    // ORDERING: Acquire pairs with try_open's Release ticket store, so pid and
    // generations are visible; a retired or recycled ticket fails the compare.
    if slot.ticket.load(Ordering::Acquire) != ticket {
        return Err(LeaseRefusal::NoRequest);
    }
    // ORDERING: SeqCst announcement of the applier before the ticket re-read below;
    // pairs with retire's SeqCst CAS and drain_and_free's SeqCst load.
    slot.appliers.fetch_add(1, Ordering::SeqCst);
    // Re-read after announcing: close stores the sentinel before waiting on
    // `appliers`, so either this read sees the retirement or close sees us.
    // ORDERING: SeqCst re-read: either it sees retire's sentinel or the closer's
    // appliers load sees our increment (no applier escapes close).
    if slot.ticket.load(Ordering::SeqCst) != ticket {
        // ORDERING: SeqCst undo of the announce on refusal; keeps the counter balanced.
        slot.appliers.fetch_sub(1, Ordering::SeqCst);
        return Err(LeaseRefusal::NoRequest);
    }
    let generations = slot.generations.load(Ordering::Relaxed);
    let guard = ApplyGuard {
        index,
        target: MmTarget {
            pid: slot.pid.load(Ordering::Relaxed),
            process_generation: (generations >> 32) as u32,
            mm_generation: generations as u32,
        },
    };
    if guard.target.pid != target_pid {
        return Err(LeaseRefusal::WrongProcess);
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is global; the exhaustion test must not starve the others.
    static TABLE_GUARD: spin::Mutex<()> = spin::Mutex::new(());

    fn target(pid: u64) -> MmTarget {
        MmTarget {
            pid,
            process_generation: 3,
            mm_generation: 5,
        }
    }

    #[test]
    fn a_live_ticket_binds_exactly_its_process_and_generations() {
        let _table = TABLE_GUARD.lock();
        let lease = RequestLease::open(target(9_001)).unwrap();
        let apply = begin_apply(lease.ticket(), 9_001).unwrap();
        assert_eq!(apply.target(), target(9_001));
        drop(apply);
        assert_eq!(
            begin_apply(lease.ticket(), 9_002).err(),
            Some(LeaseRefusal::WrongProcess)
        );
    }

    #[test]
    fn zero_sentinel_and_unissued_tickets_confer_nothing() {
        let _table = TABLE_GUARD.lock();
        let _lease = RequestLease::open(target(9_003)).unwrap();
        for ticket in [0, u64::MAX, u64::MAX - 7] {
            assert_eq!(
                begin_apply(ticket, 9_003).err(),
                Some(LeaseRefusal::NoRequest)
            );
        }
    }

    #[test]
    fn a_closed_request_cannot_apply_late() {
        let _table = TABLE_GUARD.lock();
        let lease = RequestLease::open(target(9_004)).unwrap();
        let ticket = lease.ticket();
        drop(lease);
        assert_eq!(
            begin_apply(ticket, 9_004).err(),
            Some(LeaseRefusal::NoRequest)
        );
    }

    #[test]
    fn retirement_refuses_new_effects_while_an_began_effect_still_blocks_close() {
        let _table = TABLE_GUARD.lock();
        let lease = RequestLease::open(target(9_005)).unwrap();
        let ticket = lease.ticket();
        let apply = begin_apply(ticket, 9_005).unwrap();
        assert!(lease.retire());
        // A late effect is refused, but the one that began keeps close waiting:
        // the caller must not observe the end of its call mid-effect.
        assert_eq!(
            begin_apply(ticket, 9_005).err(),
            Some(LeaseRefusal::NoRequest)
        );
        assert_eq!(lease.active_appliers(), 1);
        drop(apply);
        assert_eq!(lease.active_appliers(), 0);
        // Finish the close by hand: `retire` already ran, so Drop must not.
        // ORDERING: test-only manual completion of close (retire already ran); Release
        // matches drain_and_free.
        let (_, slot, _) = lease_slot_ref(lease.index).expect("live lease chunk");
        slot.ticket.store(FREE, Ordering::Release);
        core::mem::forget(lease);
    }

    #[test]
    fn slots_are_reused_with_fresh_tickets_and_full_table_is_reported() {
        const TEST_CHUNKS: usize = 4;
        const TEST_SLOTS: usize = TEST_CHUNKS * LEASE_CHUNK_LEN;
        let _table = TABLE_GUARD.lock();
        let first = RequestLease::open(target(9_006)).unwrap();
        let first_ticket = first.ticket();
        drop(first);
        let second = RequestLease::open(target(9_007)).unwrap();
        assert_ne!(second.ticket(), first_ticket);
        assert_eq!(
            begin_apply(first_ticket, 9_006).err(),
            Some(LeaseRefusal::NoRequest)
        );
        drop(second);
        // Growth past the legacy 256-slot limit succeeds dynamically across chunks.
        let mut wide = alloc::vec::Vec::new();
        for idx in 0..400_usize {
            wide.push(
                RequestLease::open_with(target(8_000 + (idx / MAX_LEASES_PER_PID) as u64), |_| true)
                    .expect("chunk table grows past 256 concurrent leases"),
            );
        }
        drop(wide);
        // Bounded chunk exhaustion refuses; it never yields an unbound ticket.
        let mut held = alloc::vec::Vec::new();
        // Spread over pids so the per-pid cap does not end the fill early.
        while let Some(lease) = RequestLease::open_with_chunks(
            target(9_300 + (held.len() / MAX_LEASES_PER_PID) as u64),
            |_| true,
            TEST_CHUNKS,
        ) {
            held.push(lease);
            assert!(held.len() <= TEST_SLOTS);
        }
        assert_eq!(held.len(), TEST_SLOTS);
        assert!(RequestLease::open_with_chunks(target(9_999), |_| true, TEST_CHUNKS).is_none());
    }

    #[test]
    fn a_full_table_reclaims_only_leases_of_dead_incarnations() {
        const TEST_CHUNKS: usize = 4;
        let _table = TABLE_GUARD.lock();
        let mut held = alloc::vec::Vec::new();
        let mut pid = 9_100;
        while let Some(lease) = RequestLease::open_with_chunks(target(pid), |_| true, TEST_CHUNKS) {
            held.push((pid, lease));
            pid += 1;
        }
        let (victim_pid, victim) = held.pop().unwrap();
        let victim_ticket = victim.ticket();
        // The victim's task died blocked: its Drop never ran (modelled by
        // leaking it; the slot is reclaimed through the liveness predicate).
        core::mem::forget(victim);
        let fresh = RequestLease::open_with_chunks(
            target(9_999),
            |t| t.pid != victim_pid,
            TEST_CHUNKS,
        )
        .expect("a dead owner's slot is reclaimed instead of leaking forever");
        assert_ne!(fresh.ticket(), victim_ticket);
        assert_eq!(
            begin_apply(victim_ticket, victim_pid).err(),
            Some(LeaseRefusal::NoRequest),
            "the dead owner's ticket authorizes nothing after reclamation"
        );
        // Live owners keep their leases; a reclaimed owner that does run Drop
        // later must not disturb the slot's new tenant.
        let (live_pid, live) = &held[0];
        assert!(begin_apply(live.ticket(), *live_pid).is_ok());
        assert!(begin_apply(fresh.ticket(), 9_999).is_ok());
    }

    #[test]
    fn one_pid_at_its_cap_does_not_block_another_and_close_frees_it() {
        let _table = TABLE_GUARD.lock();
        let mut held = alloc::vec::Vec::new();
        for _ in 0..MAX_LEASES_PER_PID {
            held.push(RequestLease::open_with(target(9_200), |_| true).unwrap());
        }
        assert!(RequestLease::open_with(target(9_200), |_| true).is_none());
        let other = RequestLease::open_with(target(9_201), |_| true)
            .expect("another pid is not starved by a capped pid");
        drop(other);
        held.pop();
        let again = RequestLease::open_with(target(9_200), |_| true)
            .expect("closing a lease frees the pid's cap");
        assert!(begin_apply(again.ticket(), 9_200).is_ok());
    }

    #[test]
    fn a_capped_pid_reclaims_its_dead_incarnation_leases() {
        let _table = TABLE_GUARD.lock();
        let mut held = alloc::vec::Vec::new();
        for _ in 0..MAX_LEASES_PER_PID {
            held.push(RequestLease::open_with(target(9_210), |_| true).unwrap());
        }
        let stale: alloc::vec::Vec<u64> = held.iter().map(RequestLease::ticket).collect();
        // The incarnation died (execed/exited) with callers blocked: their
        // Drop never ran, so the cap must not leak them forever.
        for lease in held {
            core::mem::forget(lease);
        }
        let fresh = RequestLease::open_with(
            MmTarget {
                pid: 9_210,
                process_generation: 4,
                mm_generation: 6,
            },
            |t| t.process_generation != 3,
        )
        .expect("dead-incarnation leases are reclaimed under the per-pid cap");
        for ticket in stale {
            assert_eq!(
                begin_apply(ticket, 9_210).err(),
                Some(LeaseRefusal::NoRequest)
            );
        }
        assert!(begin_apply(fresh.ticket(), 9_210).is_ok());
    }
}
