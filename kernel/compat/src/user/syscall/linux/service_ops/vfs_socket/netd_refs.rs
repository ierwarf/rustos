//! Socket descriptor references and lossless final-release custody.
//!
//! The initial reply publication reserves one fixed slot before ownership
//! leaves netd. Dup/fork/SCM_RIGHTS/epoll pins share its local reference count;
//! only the final release sends CLOSE. The SAME slot survives close, a lost
//! reply, and exact ACK, so neither cleanup nor a committed mutation needs a
//! second fallible queue admission. Provider identity is always epoch-bound.

use super::*;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use nucleus_core::util::ring::RingBuffer;
use rustos_user_abi::syscall::{NETD_IPC_OP_REF_ACK_BATCH, NETD_REF_ACK_BATCH_MAX, NetdRefAckWire};

// Includes live descriptions and all pending/in-flight final releases. This
// exceeds netd's 8192 netlink + 1024 INET live socket ceilings without creating
// an unbounded post-close allocation. Admission fails before reply binding.
const SOCKET_CUSTODY_CAPACITY: usize = 16 * 1024;
const CLOSES_PER_TURN: usize = 8;
const QUEUE_SCAN_PER_CLAIM: usize = 32;
const NO_SLOT: u16 = u16::MAX;
// All calls in one turn share this end. A failed exact idempotent close/ACK
// rotates and backs off; it cannot occupy this executor for a fresh 5 s lease.
const MAINTENANCE_TURN_MS: u64 =
    rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase { Empty, Live, Close, Closing, Ack, Acking }

#[derive(Clone, Copy)]
struct Slot {
    token: u64,
    epoch: u64,
    operation: u64,
    references: u64,
    retry_after_ns: u64,
    failures: u8,
    phase: Phase,
    next: u16,
}

impl Slot {
    const EMPTY: Self = Self {
        token: 0, epoch: 0, operation: 0, references: 0,
        retry_after_ns: 0, failures: 0, phase: Phase::Empty, next: NO_SLOT,
    };
}

#[derive(Clone, Copy)]
struct Work {
    index: u16,
    token: u64,
    epoch: u64,
    operation: u64,
    phase: Phase,
}

impl Work {
    const EMPTY: Self = Self {
        index: 0, token: 0, epoch: 0, operation: 0, phase: Phase::Empty,
    };

    fn acknowledgement(self) -> NetdRefAckWire {
        NetdRefAckWire {
            operation_hi: self.epoch,
            operation_lo: self.operation,
            socket_token: self.token,
            op: SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
            ..NetdRefAckWire::default()
        }
    }
}

struct Registry<const N: usize> {
    slots: [Slot; N],
    buckets: [u16; N],
    free_head: u16,
    next_unused: usize,
    len: usize,
    closes: RingBuffer<u16, N>,
    acks: RingBuffer<u16, N>,
}

impl<const N: usize> Registry<N> {
    const fn new() -> Self {
        assert!(N.is_power_of_two() && N <= u16::MAX as usize);
        Self {
            slots: [Slot::EMPTY; N], buckets: [NO_SLOT; N], free_head: NO_SLOT,
            next_unused: 0, len: 0, closes: RingBuffer::new(), acks: RingBuffer::new(),
        }
    }

    fn bucket(token: u64) -> usize {
        let mut mixed = token;
        mixed ^= mixed >> 33;
        mixed = mixed.wrapping_mul(0xff51_afd7_ed55_8ccd);
        mixed ^= mixed >> 33;
        mixed as usize & (N - 1)
    }

    fn find(&self, token: u64) -> Option<usize> {
        let mut index = self.buckets[Self::bucket(token)];
        while index != NO_SLOT {
            let slot = self.slots[index as usize];
            if slot.token == token { return Some(index as usize); }
            index = slot.next;
        }
        None
    }

    fn reserve(&mut self, token: u64, epoch: u64, operation: u64) -> Result<(), i64> {
        if token == 0 || epoch == 0 || operation == 0 { return Err(LINUX_EINVAL); }
        // Old handles must never alias an equal token minted by a replacement
        // provider. Keep the old epoch's slot until its last descriptor closes.
        if self.find(token).is_some() { return Err(LINUX_EEXIST); }
        if self.len == N { return Err(LINUX_ENOSPC); }
        let index = if self.free_head != NO_SLOT {
            let index = self.free_head as usize;
            self.free_head = self.slots[index].next;
            index
        } else {
            let index = self.next_unused;
            assert!(index < N, "occupied + free slots account for fixed storage");
            self.next_unused += 1;
            index
        };
        let bucket = Self::bucket(token);
        self.slots[index] = Slot {
            token, epoch, operation, references: 1, phase: Phase::Live,
            next: self.buckets[bucket], ..Slot::EMPTY
        };
        self.buckets[bucket] = index as u16;
        self.len += 1;
        Ok(())
    }

    fn forget(&mut self, index: usize) {
        // Stable custody slots never move: queue and in-flight owners retain
        // these indices. Unlink from the separate hash chain immediately, so
        // closed-token churn cannot accumulate tombstones under this leaf.
        let bucket = Self::bucket(self.slots[index].token);
        let next = self.slots[index].next;
        if self.buckets[bucket] == index as u16 {
            self.buckets[bucket] = next;
        } else {
            let mut previous = self.buckets[bucket];
            loop {
                assert!(previous != NO_SLOT, "occupied slot belongs to its hash chain");
                if self.slots[previous as usize].next == index as u16 {
                    self.slots[previous as usize].next = next;
                    break;
                }
                previous = self.slots[previous as usize].next;
            }
        }
        self.slots[index] = Slot { next: self.free_head, ..Slot::EMPTY };
        self.free_head = index as u16;
        self.len -= 1;
    }

    fn cancel_unpublished(&mut self, token: u64, epoch: u64) {
        let index = self.find(token).expect("unpublished socket retains its reserved slot");
        let slot = self.slots[index];
        assert!(slot.epoch == epoch && slot.phase == Phase::Live && slot.references == 1,
            "only the initial unbound socket reference can be cancelled");
        self.forget(index);
    }

    fn acquire(&mut self, token: u64) -> Result<(), i64> {
        let index = self.find(token).ok_or(LINUX_EBADF)?;
        let slot = &mut self.slots[index];
        if slot.phase != Phase::Live || slot.references == 0 { return Err(LINUX_EBADF); }
        slot.references = slot.references.checked_add(1).ok_or(LINUX_EOVERFLOW)?;
        Ok(())
    }

    fn release(&mut self, token: u64) -> Result<bool, i64> {
        let index = self.find(token).ok_or(LINUX_EBADF)?;
        let slot = &mut self.slots[index];
        if slot.phase != Phase::Live || slot.references == 0 { return Err(LINUX_EBADF); }
        slot.references -= 1;
        if slot.references != 0 { return Ok(false); }
        slot.phase = Phase::Close;
        // Every queue member owns one distinct occupied slot; in-flight and
        // live slots count against the same N, so this cannot run out of room.
        assert!(self.closes.push(index as u16), "reserved socket final-close capacity");
        Ok(true)
    }

    fn queue(&mut self, phase: Phase) -> &mut RingBuffer<u16, N> {
        match phase {
            Phase::Close => &mut self.closes,
            Phase::Ack => &mut self.acks,
            _ => unreachable!("only queued phases own a FIFO"),
        }
    }

    fn claim(&mut self, phase: Phase, now_ns: u64, epoch: Option<u64>) -> Option<Work> {
        let scanned = self.queue(phase).len().min(QUEUE_SCAN_PER_CLAIM);
        for _ in 0..scanned {
            let index = self.queue(phase).pop().expect("bounded queue snapshot");
            let slot = &mut self.slots[index as usize];
            assert!(slot.phase == phase, "queued socket phase must match its lane");
            if slot.retry_after_ns > now_ns || epoch.is_some_and(|epoch| epoch != slot.epoch) {
                assert!(self.queue(phase).push(index), "rotation keeps its reserved slot");
                continue;
            }
            slot.phase = if phase == Phase::Close { Phase::Closing } else { Phase::Acking };
            return Some(Work {
                index, token: slot.token, epoch: slot.epoch,
                operation: slot.operation, phase: slot.phase,
            });
        }
        None
    }

    fn owned_slot(&mut self, work: Work) -> &mut Slot {
        let slot = &mut self.slots[work.index as usize];
        assert!(slot.token == work.token && slot.epoch == work.epoch
            && slot.operation == work.operation && slot.phase == work.phase,
            "exact in-flight socket custody cannot be replaced or acknowledged twice");
        slot
    }

    fn confirmed_close(&mut self, work: Work) {
        assert!(work.phase == Phase::Closing);
        let slot = self.owned_slot(work);
        slot.phase = Phase::Ack;
        slot.failures = 0;
        slot.retry_after_ns = 0;
        assert!(self.acks.push(work.index), "CLOSE replaces itself by ACK in reserved custody");
    }

    fn complete(&mut self, work: Work) {
        let _ = self.owned_slot(work);
        self.forget(work.index as usize);
    }

    fn retry(&mut self, work: Work, now_ns: u64) {
        let slot = self.owned_slot(work);
        slot.failures = slot.failures.saturating_add(1);
        slot.retry_after_ns = now_ns.saturating_add(retry_delay_ns(slot.failures));
        let phase = if work.phase == Phase::Closing { Phase::Close } else { Phase::Ack };
        slot.phase = phase;
        assert!(self.queue(phase).push(work.index), "failed work retains capacity at the tail");
    }
}

const fn retry_delay_ns(failures: u8) -> u64 {
    let shift = if failures > 8 { 8 } else { failures };
    (4_u64 << shift) * 1_000_000
}

static REFERENCES: TrackedSpinLock<Registry<SOCKET_CUSTODY_CAPACITY>,
    { LockClass::NetdDeferredRef as u8 }> = TrackedSpinLock::new(Registry::new());
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);
static DRAINING: AtomicBool = AtomicBool::new(false);

pub(in crate::user::syscall::linux) fn reserve_initial(token: u64, epoch: u64) -> Result<(), i64> {
    // Uniqueness only; the registry lock publishes the descriptor lifetime.
    let operation = NEXT_OPERATION.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
        |next| next.checked_add(1)).map_err(|_| LINUX_EOVERFLOW)?;
    REFERENCES.lock().reserve(token, epoch, operation)
}

pub(in crate::user::syscall::linux) fn cancel_initial(token: u64, epoch: u64) {
    REFERENCES.lock().cancel_unpublished(token, epoch);
}

pub(super) fn acquire(token: u64) -> Result<(), i64> {
    REFERENCES.lock().acquire(token)
}

pub(super) fn release(token: u64) {
    let final_release = REFERENCES.lock().release(token);
    match final_release {
        Ok(true) => {
            purge_kernel_epoll_object(WAITSET_PROVIDER_NETD, token);
            multitask::kick_service_cleanup();
        }
        Ok(false) => {}
        Err(_) => panic!("socket reference release lost its initial publication custody"),
    }
}

struct DrainClaim;
impl Drop for DrainClaim {
    fn drop(&mut self) {
        // Release publishes this drainer's queue transitions to its successor.
        DRAINING.store(false, Ordering::Release);
    }
}

fn request(op: u16, task: u64, deadline_ns: u64) -> alloc::boxed::Box<NetdIpcRequest> {
    // `service` already authenticated this exact kernel executor. No process
    // credential lookup or entropy generation belongs in a maintenance frame.
    let mut request = alloc_box_zeroed::<NetdIpcRequest>();
    request.version = NETD_IPC_ABI_VERSION;
    request.op = op;
    request.pid = 0;
    request.tid = task;
    request.deadline_ns = deadline_ns;
    request
}

pub(super) fn service() -> usize {
    // Provider cleanup runs only on a real kernel task. Missing/stale user
    // state cannot manufacture kernel authority or a self-IPC cleanup turn.
    let Some(task) = multitask::current_task_id() else { return 0; };
    if kernel_ps::api::snapshot::ipc_sender_ids_for_task(task) != Some((0, task)) {
        return 0;
    }
    // Acquire pairs with DrainClaim's Release; a losing caller touches no work.
    if DRAINING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return 0;
    }
    let _claim = DrainClaim;
    let deadline_ns = vfs_meta::netd_deadline_after_ms(vfs_meta::netd_monotonic_nanos(), MAINTENANCE_TURN_MS);
    let mut completed = service_acks(task, deadline_ns);
    for _ in 0..CLOSES_PER_TURN {
        let Some(timeout) = vfs_meta::netd_deadline_remaining_ms(deadline_ns) else { break; };
        let now = vfs_meta::netd_monotonic_nanos();
        let Some(work) = REFERENCES.lock().claim(Phase::Close, now, None)
        else { break; };
        let mut request = request(SYSCALL_OFFLOAD_OP_LINUX_CLOSE, task, deadline_ns);
        request.socket_token = work.token;
        request.operation_hi = work.epoch;
        request.operation_lo = work.operation;
        let result = ipc_helpers::call_netd_ref_maintenance(&request, work.epoch, timeout);
        let now = vfs_meta::netd_monotonic_nanos();
        let mut registry = REFERENCES.lock();
        match &result {
            Ok(response) if matches!(response.status.unsigned_abs() as i64, 0 | LINUX_EBADF)
                && response.payload_len == 0 =>
            {
                registry.confirmed_close(work);
                completed += 1;
            }
            // Exact provider retirement invalidates its socket and replay log;
            // never send old tokens to the replacement provider.
            Err(LINUX_ESTALE) => { registry.complete(work); completed += 1; }
            _ => registry.retry(work, now),
        }
    }
    completed.saturating_add(service_acks(task, deadline_ns))
}

fn service_acks(task: u64, deadline_ns: u64) -> usize {
    let Some(timeout) = vfs_meta::netd_deadline_remaining_ms(deadline_ns) else { return 0; };
    let mut batch = [Work::EMPTY; NETD_REF_ACK_BATCH_MAX];
    let now = vfs_meta::netd_monotonic_nanos();
    let count = {
        let mut registry = REFERENCES.lock();
        let mut count = 0;
        let mut epoch = None;
        for target in &mut batch {
            let Some(work) = registry.claim(Phase::Ack, now, epoch) else { break; };
            epoch = Some(work.epoch);
            *target = work;
            count += 1;
        }
        count
    };
    if count == 0 { return 0; }
    let mut request = request(NETD_IPC_OP_REF_ACK_BATCH, task, deadline_ns);
    request.arg0 = count as u64;
    for (index, work) in batch[..count].iter().enumerate() {
        let ack = work.acknowledgement();
        let start = index * core::mem::size_of::<NetdRefAckWire>();
        request.payload[start..start + core::mem::size_of::<NetdRefAckWire>()]
            .copy_from_slice(as_bytes(&ack));
    }
    request.payload_len = (count * core::mem::size_of::<NetdRefAckWire>()) as u32;
    let result = ipc_helpers::call_netd_ref_maintenance(&request, batch[0].epoch, timeout);
    let now = vfs_meta::netd_monotonic_nanos();
    let mut registry = REFERENCES.lock();
    let mut completed = 0;
    for (index, work) in batch[..count].iter().copied().enumerate() {
        let acknowledged = match &result {
            Ok(response) if response.status == 0 && response.value == count as u64
                && response.payload_len as usize == count * core::mem::size_of::<i32>() =>
            {
                let start = index * core::mem::size_of::<i32>();
                i32::from_le_bytes(response.payload[start..start + 4].try_into().unwrap()) == 0
            }
            Err(LINUX_ESTALE) => true,
            _ => false,
        };
        if acknowledged { registry.complete(work); completed += 1; }
        else { registry.retry(work, now); }
    }
    completed
}

#[cfg(test)]
mod tests;
