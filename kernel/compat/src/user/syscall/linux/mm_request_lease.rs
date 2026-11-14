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

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Concurrent in-flight address-space requests. A request exists only while its
/// caller is blocked on `syscalld`, so this bounds blocked callers, and a full
/// table is reported to the caller as `EAGAIN` rather than served unbound.
const LEASE_SLOTS: usize = 256;
/// Most leases one pid may hold at once. A process has at most
/// `MAX_THREADS_PER_PROCESS` (128) threads, each holding one lease for one
/// syscall, so without a cap a few processes with a slow `syscalld` could fill
/// the whole table and turn every other process's request into `EAGAIN`. Over
/// the cap, `open` refuses exactly like a full table; 32 lets a thread-heavy
/// process keep many mm requests in flight while eight such processes still
/// fit the table. This bound is admission
/// policy outside the verified lease protocol (`formal/mm-request-lease` models
/// one slot and does not model exhaustion); it never affects a granted ticket.
const MAX_LEASES_PER_PID: usize = 32;
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

static SLOTS: [Slot; LEASE_SLOTS] = [const { Slot::new() }; LEASE_SLOTS];
static CLAIMED_MASK: [AtomicU64; LEASE_SLOTS / 64] = [const { AtomicU64::new(0) }; LEASE_SLOTS / 64];
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

const _: () = assert!(LEASE_SLOTS == 256 && BUSY_SENTINEL == u64::MAX);

fn claim_mask_set(index: usize) {
    CLAIMED_MASK[index >> 6].fetch_or(1_u64 << (index & 63), Ordering::SeqCst);
}

fn claim_mask_clear(index: usize) {
    CLAIMED_MASK[index >> 6].fetch_and(!(1_u64 << (index & 63)), Ordering::Release);
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
        match Self::try_open(target) {
            Ok(lease) => Some(lease),
            Err(OpenRefusal::PerPidLimit) => {
                reclaim_dead(&is_live, Some(target.pid));
                Self::try_open(target).ok()
            }
            Err(OpenRefusal::TableFull) => {
                reclaim_dead(&is_live, None);
                Self::try_open(target).ok()
            }
        }
    }

    fn try_open(target: MmTarget) -> Result<Self, OpenRefusal> {
        for (index, slot) in SLOTS.iter().enumerate() {
            if slot
                .ticket
                .compare_exchange(FREE, BUSY_SENTINEL, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let ticket = loop {
                // Encode `index` in the low 8 bits so `begin_apply` is O(1).
                let seq = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
                let candidate = (seq << 8) | (index as u64);
                if candidate != FREE && candidate != BUSY_SENTINEL {
                    break candidate;
                }
            };
            // ORDERING: mark bit and announce pid (SeqCst) while the slot is
            // held by the sentinel, then scan claimed slots. Two same-pid
            // openers each store then scan, so at least one sees the other.
            claim_mask_set(index);
            slot.pid.store(target.pid, Ordering::SeqCst);
            if holders_of_pid(target.pid, index) >= MAX_LEASES_PER_PID {
                claim_mask_clear(index);
                slot.ticket.store(FREE, Ordering::Release);
                return Err(OpenRefusal::PerPidLimit);
            }
            slot.generations.store(
                pack(target.process_generation, target.mm_generation),
                Ordering::Relaxed,
            );
            // ORDERING: Release publishes pid/generations with the ticket.
            slot.ticket.store(ticket, Ordering::Release);
            return Ok(Self { index, ticket });
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
fn holders_of_pid(pid: u64, skip: usize) -> usize {
    let mut count = 0_usize;
    for (word_idx, word_atom) in CLAIMED_MASK.iter().enumerate() {
        let mut bits = word_atom.load(Ordering::SeqCst);
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let index = (word_idx << 6) | bit;
            if index != skip
                && SLOTS[index].pid.load(Ordering::SeqCst) == pid
                && SLOTS[index].ticket.load(Ordering::SeqCst) != FREE
            {
                count += 1;
            }
        }
    }
    count
}

impl RequestLease {
    /// Step one of close: no apply may begin after this returns. False when
    /// the lease was already reclaimed (its incarnation died first).
    fn retire(&self) -> bool {
        retire(&SLOTS[self.index], self.ticket)
    }

    /// Appliers that validated before retirement and have not finished.
    fn active_appliers(&self) -> u32 {
        SLOTS[self.index].appliers.load(Ordering::SeqCst)
    }
}

/// Retires exactly `ticket` in `slot` so no apply may begin afterwards. False
/// when the slot no longer holds that ticket (reclaimed or recycled).
fn retire(slot: &Slot, ticket: u64) -> bool {
    slot.ticket
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
fn drain_and_free(index: usize, slot: &Slot, yield_cpu: bool) {
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
    claim_mask_clear(index);
    slot.ticket.store(FREE, Ordering::Release);
}

/// Retire `ticket` and, if this call won the retirement, drain and free the
/// slot. Returns false (touching nothing) when the slot was not ours to retire.
fn retire_and_drain(index: usize, slot: &Slot, ticket: u64, yield_cpu: bool) -> bool {
    if !retire(slot, ticket) {
        return false;
    }
    drain_and_free(index, slot, yield_cpu);
    true
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        // Retire first: appliers arriving from here on fail validation, then
        // drain those that validated earlier. A false result means the lease
        // was reclaimed after this owner's incarnation died; the slot belongs
        // to someone else now and is not ours to retire or free.
        retire_and_drain(self.index, &SLOTS[self.index], self.ticket, true);
    }
}

/// Frees leases whose owner can no longer run (its incarnation is gone).
fn reclaim_dead(is_live: &impl Fn(MmTarget) -> bool, pid_filter: Option<u64>) {
    for (word_idx, word_atom) in CLAIMED_MASK.iter().enumerate() {
        let mut bits = word_atom.load(Ordering::Acquire);
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let index = (word_idx << 6) | bit;
            let slot = &SLOTS[index];
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
            retire_and_drain(index, slot, ticket, false);
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
        SLOTS[self.index].appliers.fetch_sub(1, Ordering::SeqCst);
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
    let index = (ticket as usize) & (LEASE_SLOTS - 1);
    let slot = &SLOTS[index];
    if slot.ticket.load(Ordering::Acquire) != ticket {
        return Err(LeaseRefusal::NoRequest);
    }
    slot.appliers.fetch_add(1, Ordering::SeqCst);
    // Re-read after announcing: close stores the sentinel before waiting on
    // `appliers`, so either this read sees the retirement or close sees us.
    if slot.ticket.load(Ordering::SeqCst) != ticket {
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
        SLOTS[lease.index].ticket.store(FREE, Ordering::Release);
        core::mem::forget(lease);
    }

    #[test]
    fn slots_are_reused_with_fresh_tickets_and_full_table_is_reported() {
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
        // Exhaustion refuses; it never yields an unbound ticket.
        let mut held = alloc::vec::Vec::new();
        // Spread over pids so the per-pid cap does not end the fill early.
        while let Some(lease) = RequestLease::open_with(
            target(9_300 + (held.len() / MAX_LEASES_PER_PID) as u64),
            |_| true,
        ) {
            held.push(lease);
            assert!(held.len() <= LEASE_SLOTS);
        }
        assert_eq!(held.len(), LEASE_SLOTS);
        assert!(RequestLease::open_with(target(9_999), |_| true).is_none());
    }

    #[test]
    fn a_full_table_reclaims_only_leases_of_dead_incarnations() {
        let _table = TABLE_GUARD.lock();
        let mut held = alloc::vec::Vec::new();
        let mut pid = 9_100;
        while let Some(lease) = RequestLease::open_with(target(pid), |_| true) {
            held.push((pid, lease));
            pid += 1;
        }
        let (victim_pid, victim) = held.pop().unwrap();
        let victim_ticket = victim.ticket();
        // The victim's task died blocked: its Drop never ran (modelled by
        // leaking it; the slot is reclaimed through the liveness predicate).
        core::mem::forget(victim);
        let fresh = RequestLease::open_with(target(9_999), |t| t.pid != victim_pid)
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
