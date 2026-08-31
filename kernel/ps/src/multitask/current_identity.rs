//! Lock-free identity publication for the task running on each CPU.
//!
//! Syscall entry asks "who am I?" far more often than the scheduler ever
//! dispatches. The per-caller acquisition census measured 167k of these
//! queries per second against roughly 4k dispatches, and every one of them
//! took the exclusive global scheduler lock to read fields belonging to the
//! task already running on the asking CPU. Linux answers the same question
//! from `current`, FreeBSD from `curthread`, and Zircon from
//! `Thread::Current::Get()` — a published cell, never the run-queue lock.
//!
//! Each task slot owns a seqlock-protected record. Writes happen only under
//! the scheduler lock, so there is never more than one writer per slot; reads
//! run with interrupts masked, so the asking CPU's current slot cannot change
//! underneath the observation. A reader that catches a writer mid-update, or
//! that finds a slot which was never published, returns `None` and its caller
//! falls back to the locked query.
//!
//! Publication is not optional. A write site that mutates identity without
//! republishing leaves a stale record, which is a correctness fault and not
//! merely a slow path, so `matches_authority` re-derives each slot from the
//! authoritative scheduler tables while the lock is held and reports any
//! divergence rather than trusting the write sites to be complete by
//! inspection.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};

use super::MAX_SCHEDULER_TASKS;
use super::process_table::ProcessHandle;
use crate::io::session::ConsoleSessionHandle;
use crate::user::abi::UserAbi;

const FLAG_PUBLISHED: u32 = 1 << 0;
const FLAG_TASK_ID: u32 = 1 << 1;
const FLAG_USER_MODE: u32 = 1 << 2;
const FLAG_PROCESS: u32 = 1 << 3;
const FLAG_PROCESS_ID: u32 = 1 << 4;
const FLAG_PAGER_CHARGE: u32 = 1 << 5;
const ABI_SHIFT: u32 = 6;
const ABI_MASK: u32 = 0b11 << ABI_SHIFT;
const ABI_NONE: u32 = 0;
const ABI_LINUX: u32 = 1;
const ABI_WINDOWS: u32 = 2;

/// Immutable scheduling authority copied into the slot publication record.
/// A faulting task resolves IPC donation first, then reads the selected owner's
/// stamp without taking the scheduler lock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PagerChargeStamp {
    pub(super) context_slot: u64,
    pub(super) context_generation: u64,
    pub(super) scheduling_domain: u64,
    pub(super) policy_epoch: u64,
    pub(super) period_ns: u64,
}

/// The identity fields a running task can be asked for without the scheduler
/// lock. Everything here is bound when the slot is installed and changes only
/// through the publication sites, never through ordinary task execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct TaskIdentity {
    pub(super) task_id: Option<u64>,
    pub(super) user_mode: bool,
    pub(super) abi: Option<UserAbi>,
    pub(super) process_handle: Option<ProcessHandle>,
    /// Immutable PID bound to this task slot. This is diagnostic identity
    /// only; the scheduler's slot/current-owner state remains authoritative.
    pub(super) process_id: Option<u64>,
    pub(super) console_session: ConsoleSessionHandle,
    pub(super) pager_charge: Option<PagerChargeStamp>,
}

impl TaskIdentity {
    /// The binding shape `current_user_process_binding` returns: user tasks
    /// only, and only once every field it needs is present.
    pub(super) fn user_binding(
        &self,
    ) -> Option<(u64, UserAbi, ProcessHandle, ConsoleSessionHandle)> {
        if !self.user_mode {
            return None;
        }
        Some((
            self.task_id?,
            self.abi?,
            self.process_handle?,
            self.console_session,
        ))
    }

    /// Returns a definitive log identity when this record is complete.
    ///
    /// The outer `None` means a user record is incomplete and callers must
    /// use the locked authority. `Some(None)` is a complete kernel-task
    /// record, which conclusively has no user log identity.
    pub(super) fn complete_user_log_ids(&self) -> Option<Option<(u64, u64)>> {
        if !self.user_mode {
            return Some(None);
        }
        Some(Some((self.process_id?, self.task_id?)))
    }
}

struct PublishedIdentity {
    /// Even when stable, odd while a writer is between the two halves of an
    /// update. Readers that observe an odd or changed value take the lock.
    version: AtomicU64,
    task_id: AtomicU64,
    /// `index << 32 | generation`, meaningful only under `FLAG_PROCESS`.
    process: AtomicU64,
    /// Meaningful only under `FLAG_PROCESS_ID`.
    process_id: AtomicU64,
    console: AtomicU64,
    pager_context_slot: AtomicU64,
    pager_context_generation: AtomicU64,
    pager_scheduling_domain: AtomicU64,
    pager_policy_epoch: AtomicU64,
    pager_period_ns: AtomicU64,
    flags: AtomicU32,
}

impl PublishedIdentity {
    const fn empty() -> Self {
        Self {
            version: AtomicU64::new(0),
            task_id: AtomicU64::new(0),
            process: AtomicU64::new(0),
            process_id: AtomicU64::new(0),
            console: AtomicU64::new(0),
            pager_context_slot: AtomicU64::new(0),
            pager_context_generation: AtomicU64::new(0),
            pager_scheduling_domain: AtomicU64::new(0),
            pager_policy_epoch: AtomicU64::new(0),
            pager_period_ns: AtomicU64::new(0),
            flags: AtomicU32::new(0),
        }
    }
}

#[expect(
    clippy::declare_interior_mutable_const,
    reason = "array initializer for per-slot atomics"
)]
const EMPTY_IDENTITY: PublishedIdentity = PublishedIdentity::empty();

static IDENTITIES: [PublishedIdentity; MAX_SCHEDULER_TASKS] = [EMPTY_IDENTITY; MAX_SCHEDULER_TASKS];

/// Conservative per-slot "this thread may have a pending signal" hint, the
/// same role `TIF_SIGPENDING` plays in Linux: syscall return checks it on every
/// exit and must not pay for the run-queue lock to learn that nothing is
/// pending.
///
/// The hint is allowed to read `true` when nothing is pending, which costs one
/// locked recheck. It can never read `false` while something is pending: the
/// only site that raises a pending signal and the only site that lowers this
/// hint both run under the scheduler lock, so they are serialized against each
/// other, and the raise stores the hint after the state it describes.
static SIGNAL_PENDING: [AtomicBool; MAX_SCHEDULER_TASKS] =
    [const { AtomicBool::new(false) }; MAX_SCHEDULER_TASKS];

/// Raises the hint for `slot`. Callers must hold the scheduler lock and must
/// already have written the pending state this advertises.
pub(super) fn raise_signal_pending(slot: usize) {
    if let Some(flag) = SIGNAL_PENDING.get(slot) {
        // ORDERING: Release publishes the pending-signal state written before
        // this call to any reader that observes the raised hint.
        flag.store(true, Ordering::Release);
    }
}

/// Re-derives the hint for `slot` from authoritative state. Callers must hold
/// the scheduler lock, which is what excludes a concurrent raise.
pub(super) fn sync_signal_pending(slot: usize, pending: bool) {
    if let Some(flag) = SIGNAL_PENDING.get(slot) {
        flag.store(pending, Ordering::Release);
    }
}

/// Reads the hint for `slot`. An unknown slot reports `true` so the caller
/// falls back to the authoritative locked query.
pub(super) fn signal_pending(slot: usize) -> bool {
    SIGNAL_PENDING
        .get(slot)
        // ORDERING: Acquire pairs with the raising Release store.
        .is_none_or(|flag| flag.load(Ordering::Acquire))
}

fn encode_process(handle: Option<ProcessHandle>) -> (u64, u32) {
    match handle {
        Some(handle) => {
            let index = u64::try_from(handle.index()).expect("process index exceeds u64");
            ((index << 32) | u64::from(handle.generation()), FLAG_PROCESS)
        }
        None => (0, 0),
    }
}

fn decode_process(word: u64, flags: u32) -> Option<ProcessHandle> {
    if flags & FLAG_PROCESS == 0 {
        return None;
    }
    let index = usize::try_from(word >> 32).expect("published process index exceeds usize");
    let generation = u32::try_from(word & u64::from(u32::MAX)).expect("generation exceeds u32");
    Some(ProcessHandle::new(index, generation))
}

fn encode_abi(abi: Option<UserAbi>) -> u32 {
    let code = match abi {
        None => ABI_NONE,
        Some(UserAbi::Linux) => ABI_LINUX,
        Some(UserAbi::Windows) => ABI_WINDOWS,
    };
    code << ABI_SHIFT
}

fn decode_abi(flags: u32) -> Option<UserAbi> {
    match (flags & ABI_MASK) >> ABI_SHIFT {
        ABI_LINUX => Some(UserAbi::Linux),
        ABI_WINDOWS => Some(UserAbi::Windows),
        _ => None,
    }
}

/// Publishes `identity` for `slot`. Callers must hold the scheduler lock, which
/// is what makes the single-writer requirement of the seqlock hold.
pub(super) fn publish(slot: usize, identity: TaskIdentity) {
    let Some(cell) = IDENTITIES.get(slot) else {
        return;
    };
    // ORDERING: Relaxed is sufficient because the scheduler lock already
    // excludes every other writer of this slot.
    let version = cell.version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "task identity: concurrent publication for slot {slot}"
    );
    let (process, process_flag) = encode_process(identity.process_handle);
    let mut flags = FLAG_PUBLISHED | process_flag | encode_abi(identity.abi);
    if identity.user_mode {
        flags |= FLAG_USER_MODE;
    }
    if identity.task_id.is_some() {
        flags |= FLAG_TASK_ID;
    }
    if identity.process_id.is_some() {
        flags |= FLAG_PROCESS_ID;
    }
    if identity.pager_charge.is_some() {
        flags |= FLAG_PAGER_CHARGE;
    }

    // ORDERING: the odd version must be visible before any field changes, or a
    // reader could load a half-updated record and still see a stable version.
    cell.version
        .store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    cell.task_id
        .store(identity.task_id.unwrap_or(0), Ordering::Relaxed);
    cell.process.store(process, Ordering::Relaxed);
    cell.process_id
        .store(identity.process_id.unwrap_or(0), Ordering::Relaxed);
    cell.console
        .store(identity.console_session.raw(), Ordering::Relaxed);
    let pager_charge = identity.pager_charge.unwrap_or_default();
    cell.pager_context_slot
        .store(pager_charge.context_slot, Ordering::Relaxed);
    cell.pager_context_generation
        .store(pager_charge.context_generation, Ordering::Relaxed);
    cell.pager_scheduling_domain
        .store(pager_charge.scheduling_domain, Ordering::Relaxed);
    cell.pager_policy_epoch
        .store(pager_charge.policy_epoch, Ordering::Relaxed);
    cell.pager_period_ns
        .store(pager_charge.period_ns, Ordering::Relaxed);
    cell.flags.store(flags, Ordering::Relaxed);
    // ORDERING: Release publishes every field store above to any reader that
    // observes this even version.
    cell.version
        .store(version.wrapping_add(2), Ordering::Release);
}

/// Clears `slot` so readers fall back to the locked query until it is bound
/// again. Used when a slot is retired rather than rebound.
pub(super) fn clear(slot: usize) {
    let Some(cell) = IDENTITIES.get(slot) else {
        return;
    };
    let version = cell.version.load(Ordering::Relaxed);
    assert_eq!(
        version & 1,
        0,
        "task identity: concurrent retirement for slot {slot}"
    );
    cell.version
        .store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    // Clearing every scalar while the record is odd makes a stale PID/task
    // pair unrepresentable even to future maintenance code that adds flags.
    cell.task_id.store(0, Ordering::Relaxed);
    cell.process.store(0, Ordering::Relaxed);
    cell.process_id.store(0, Ordering::Relaxed);
    cell.console.store(0, Ordering::Relaxed);
    cell.pager_context_slot.store(0, Ordering::Relaxed);
    cell.pager_context_generation.store(0, Ordering::Relaxed);
    cell.pager_scheduling_domain.store(0, Ordering::Relaxed);
    cell.pager_policy_epoch.store(0, Ordering::Relaxed);
    cell.pager_period_ns.store(0, Ordering::Relaxed);
    cell.flags.store(0, Ordering::Relaxed);
    cell.version
        .store(version.wrapping_add(2), Ordering::Release);
}

/// Reads the published identity for `slot`, or `None` when the slot was never
/// published or a writer is mid-update.
pub(super) fn read(slot: usize) -> Option<TaskIdentity> {
    let cell = IDENTITIES.get(slot)?;
    // ORDERING: Acquire pairs with the publishing Release store and orders the
    // field loads below after this version observation.
    let before = cell.version.load(Ordering::Acquire);
    if before & 1 != 0 {
        return None;
    }
    let flags = cell.flags.load(Ordering::Relaxed);
    let task_id = cell.task_id.load(Ordering::Relaxed);
    let process = cell.process.load(Ordering::Relaxed);
    let process_id = cell.process_id.load(Ordering::Relaxed);
    let console = cell.console.load(Ordering::Relaxed);
    let pager_context_slot = cell.pager_context_slot.load(Ordering::Relaxed);
    let pager_context_generation = cell.pager_context_generation.load(Ordering::Relaxed);
    let pager_scheduling_domain = cell.pager_scheduling_domain.load(Ordering::Relaxed);
    let pager_policy_epoch = cell.pager_policy_epoch.load(Ordering::Relaxed);
    let pager_period_ns = cell.pager_period_ns.load(Ordering::Relaxed);
    // ORDERING: this fence keeps the field loads above from being reordered
    // after the version re-read that validates them.
    fence(Ordering::Acquire);
    if cell.version.load(Ordering::Relaxed) != before || flags & FLAG_PUBLISHED == 0 {
        return None;
    }
    Some(TaskIdentity {
        task_id: (flags & FLAG_TASK_ID != 0).then_some(task_id),
        user_mode: flags & FLAG_USER_MODE != 0,
        abi: decode_abi(flags),
        process_handle: decode_process(process, flags),
        process_id: (flags & FLAG_PROCESS_ID != 0).then_some(process_id),
        console_session: ConsoleSessionHandle::from_raw(console),
        pager_charge: (flags & FLAG_PAGER_CHARGE != 0).then_some(PagerChargeStamp {
            context_slot: pager_context_slot,
            context_generation: pager_context_generation,
            scheduling_domain: pager_scheduling_domain,
            policy_epoch: pager_policy_epoch,
            period_ns: pager_period_ns,
        }),
    })
}

/// Compares the published record for `slot` against the authority the
/// scheduler holds, returning `false` on divergence. The caller must hold the
/// scheduler lock so that `authority` is a stable observation.
pub(super) fn matches_authority(slot: usize, authority: Option<TaskIdentity>) -> bool {
    match (read(slot), authority) {
        (Some(published), Some(authority)) => published == authority,
        (None, None) => true,
        // A cleared slot that the scheduler still binds is a missed
        // publication; a published slot the scheduler no longer binds is a
        // missed retirement. Both are faults.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_identity_round_trips_and_clears() {
        let slot = 3;
        let identity = TaskIdentity {
            task_id: Some(41),
            user_mode: true,
            abi: Some(UserAbi::Linux),
            process_handle: Some(ProcessHandle::new(7, 9)),
            process_id: Some(23),
            console_session: ConsoleSessionHandle::from_raw(5),
            pager_charge: Some(PagerChargeStamp {
                context_slot: 4,
                context_generation: 42,
                scheduling_domain: 43,
                policy_epoch: 47,
                period_ns: 53,
            }),
        };
        publish(slot, identity);
        assert_eq!(read(slot), Some(identity));
        assert_eq!(
            read(slot).and_then(|read| read.complete_user_log_ids()),
            Some(Some((23, 41)))
        );
        assert_eq!(
            read(slot).and_then(|read| read.user_binding()),
            Some((
                41,
                UserAbi::Linux,
                ProcessHandle::new(7, 9),
                ConsoleSessionHandle::from_raw(5)
            ))
        );
        clear(slot);
        assert_eq!(read(slot), None);
        let cleared = &IDENTITIES[slot];
        assert_eq!(cleared.task_id.load(Ordering::Relaxed), 0);
        assert_eq!(cleared.process_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn kernel_task_publishes_no_user_binding() {
        let slot = 4;
        publish(
            slot,
            TaskIdentity {
                task_id: Some(12),
                user_mode: false,
                abi: None,
                process_handle: None,
                process_id: None,
                console_session: ConsoleSessionHandle::SYSTEM,
                pager_charge: None,
            },
        );
        let published = read(slot).expect("published identity");
        assert_eq!(published.task_id, Some(12));
        assert_eq!(published.user_binding(), None);
        clear(slot);
    }

    #[test]
    fn incomplete_user_or_odd_publication_requires_locked_log_lookup() {
        let slot = 5;
        clear(slot);
        assert_eq!(
            read(slot).and_then(|identity| identity.complete_user_log_ids()),
            None
        );

        let incomplete = TaskIdentity {
            task_id: Some(73),
            user_mode: true,
            abi: Some(UserAbi::Linux),
            process_handle: Some(ProcessHandle::new(2, 4)),
            process_id: None,
            console_session: ConsoleSessionHandle::from_raw(7),
            pager_charge: None,
        };
        publish(slot, incomplete);
        assert_eq!(
            read(slot).and_then(|identity| identity.complete_user_log_ids()),
            None
        );

        let complete = TaskIdentity {
            process_id: Some(31),
            ..incomplete
        };
        publish(slot, complete);
        let cell = &IDENTITIES[slot];
        let stable_version = cell.version.load(Ordering::Relaxed);
        cell.version
            .store(stable_version.wrapping_add(1), Ordering::Relaxed);
        assert_eq!(
            read(slot).and_then(|identity| identity.complete_user_log_ids()),
            None
        );
        cell.version.store(stable_version, Ordering::Relaxed);
        clear(slot);
    }

    #[test]
    fn unpublished_slot_reads_as_absent() {
        assert_eq!(read(MAX_SCHEDULER_TASKS - 1), None);
        assert_eq!(read(MAX_SCHEDULER_TASKS), None);
    }
}
