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
const ABI_SHIFT: u32 = 4;
const ABI_MASK: u32 = 0b11 << ABI_SHIFT;
const ABI_NONE: u32 = 0;
const ABI_LINUX: u32 = 1;
const ABI_WINDOWS: u32 = 2;

/// The identity fields a running task can be asked for without the scheduler
/// lock. Everything here is bound when the slot is installed and changes only
/// through the publication sites, never through ordinary task execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct TaskIdentity {
    pub(super) task_id: Option<u64>,
    pub(super) user_mode: bool,
    pub(super) abi: Option<UserAbi>,
    pub(super) process_handle: Option<ProcessHandle>,
    pub(super) console_session: ConsoleSessionHandle,
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
}

struct PublishedIdentity {
    /// Even when stable, odd while a writer is between the two halves of an
    /// update. Readers that observe an odd or changed value take the lock.
    version: AtomicU64,
    task_id: AtomicU64,
    /// `index << 32 | generation`, meaningful only under `FLAG_PROCESS`.
    process: AtomicU64,
    console: AtomicU64,
    flags: AtomicU32,
}

impl PublishedIdentity {
    const fn empty() -> Self {
        Self {
            version: AtomicU64::new(0),
            task_id: AtomicU64::new(0),
            process: AtomicU64::new(0),
            console: AtomicU64::new(0),
            flags: AtomicU32::new(0),
        }
    }
}

#[expect(
    clippy::declare_interior_mutable_const,
    reason = "array initializer for per-slot atomics"
)]
const EMPTY_IDENTITY: PublishedIdentity = PublishedIdentity::empty();

static IDENTITIES: [PublishedIdentity; MAX_SCHEDULER_TASKS] =
    [EMPTY_IDENTITY; MAX_SCHEDULER_TASKS];

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

    // ORDERING: the odd version must be visible before any field changes, or a
    // reader could load a half-updated record and still see a stable version.
    cell.version.store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    cell.task_id
        .store(identity.task_id.unwrap_or(0), Ordering::Relaxed);
    cell.process.store(process, Ordering::Relaxed);
    cell.console
        .store(identity.console_session.raw(), Ordering::Relaxed);
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
    cell.version.store(version.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
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
    let console = cell.console.load(Ordering::Relaxed);
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
        console_session: ConsoleSessionHandle::from_raw(console),
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
            console_session: ConsoleSessionHandle::from_raw(5),
        };
        publish(slot, identity);
        assert_eq!(read(slot), Some(identity));
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
                console_session: ConsoleSessionHandle::SYSTEM,
            },
        );
        let published = read(slot).expect("published identity");
        assert_eq!(published.task_id, Some(12));
        assert_eq!(published.user_binding(), None);
        clear(slot);
    }

    #[test]
    fn unpublished_slot_reads_as_absent() {
        assert_eq!(read(MAX_SCHEDULER_TASKS - 1), None);
        assert_eq!(read(MAX_SCHEDULER_TASKS), None);
    }
}
