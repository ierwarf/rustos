use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use super::*;
use lazy_static::lazy_static;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    LIFECYCLE_DRAIN_MAX_EVENTS, LIFECYCLE_EVENT_EXIT, LIFECYCLE_EVENT_FORK,
    LIFECYCLE_EVENT_THREAD_EXIT, LIFECYCLE_EVENT_THREAD_EXIT_LOST, LifecycleDrainBrokerArgs,
    LifecycleEventWire,
    VFS_IPC_OP_FILE_IDENTITY, VFS_IPC_OP_PREAD64, VFS_IPC_PAYLOAD_CAPACITY, VfsFileIdentityWire,
};

/// Asks vfsd which backing file one open description names. The reply is the
/// only identity authority for a library binding; it is decoded from exactly
/// one `VfsFileIdentityWire` payload and rejected when malformed.
/// Asks vfsd which file `remote_id` names, on behalf of its owning process.
///
/// The mapping broker runs in syscalld's context, so the ambient request
/// identity (pid and tid, which vfsd checks against the IPC sender) is
/// syscalld's, while vfsd attributes the open description to the client that
/// opened it and answers ESTALE for any other pid. The thread identity stays
/// the sender's; only the pid names the client. vfsd accepts this split only for
/// this read-only operation and only from the live syscalld owner.
pub(crate) fn call_remote_vfs_file_identity(
    remote_id: u64,
    client_pid: u64,
) -> Result<VfsFileIdentityWire, i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_FILE_IDENTITY);
    request.remote_id = remote_id;
    request.pid = client_pid;
    let response = call_vfs_ipc_request(&request)?;
    ensure_vfs_status(&response)?;
    decode_file_identity(&response.payload[..], response.payload_len as usize)
}

fn decode_file_identity(payload: &[u8], len: usize) -> Result<VfsFileIdentityWire, i64> {
    let bytes = payload.get(..len).ok_or(LINUX_EINVAL)?;
    let wire = VfsFileIdentityWire::from_bytes(bytes).ok_or(LINUX_EINVAL)?;
    if !wire.is_canonical() {
        return Err(LINUX_EINVAL);
    }
    Ok(wire)
}

#[cfg(test)]
mod file_identity_tests {
    use super::*;

    fn bytes(wire: VfsFileIdentityWire) -> [u8; 56] {
        wire.to_bytes()
    }
    fn good() -> VfsFileIdentityWire {
        VfsFileIdentityWire { volume: 1, object: 2, generation: 3, storage_epoch: 4, mount_generation: 5, len: 6, flags: 1 }
    }

    #[test]
    fn identity_reply_must_be_exactly_one_canonical_wire() {
        let payload = bytes(good());
        assert_eq!(decode_file_identity(&payload, 56), Ok(good()));
        assert_eq!(decode_file_identity(&payload, 55), Err(LINUX_EINVAL));
        assert_eq!(decode_file_identity(&payload, 57), Err(LINUX_EINVAL));
        assert_eq!(decode_file_identity(&payload[..40], 56), Err(LINUX_EINVAL));
        let zero_len = bytes(VfsFileIdentityWire { len: 0, ..good() });
        assert_eq!(decode_file_identity(&zero_len, 56), Err(LINUX_EINVAL));
        let unknown_flag = bytes(VfsFileIdentityWire { flags: 1 << 9, ..good() });
        assert_eq!(decode_file_identity(&unknown_flag, 56), Err(LINUX_EINVAL));
    }
}

/// Reads `len` bytes of `remote_id` at `offset` on behalf of `client_pid`, the
/// process that owns the open description. The mapping broker runs in
/// syscalld's context, so without naming the client vfsd answers ESTALE (the
/// description is funded to the client, not to syscalld); see
/// `call_remote_vfs_file_identity` for the identity split.
pub(crate) fn call_remote_vfs_read_bytes(
    remote_id: u64,
    client_pid: u64,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, i64> {
    let mut bytes = Vec::new();
    while bytes.len() < len {
        let chunk_len = (len - bytes.len()).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.remote_id = remote_id;
        request.pid = client_pid;
        request.arg0 = offset.saturating_add(bytes.len() as u64);
        request.arg1 = chunk_len as u64;
        let response = call_vfs_ipc_request(&request).inspect_err(|errno| {
            nucleus_core::debug::write_debugcon_only_line(
                alloc::format!(
                    "proc-commit: remote read rejected stage=vfs-transport remote_id={remote_id} offset={} len={chunk_len} errno={errno}",
                    request.arg0,
                )
                .as_bytes(),
            );
        })?;
        ensure_vfs_status(&response).inspect_err(|errno| {
            nucleus_core::debug::write_debugcon_only_line(
                alloc::format!(
                    "proc-commit: remote read rejected stage=vfs-status remote_id={remote_id} offset={} len={chunk_len} errno={errno}",
                    request.arg0,
                )
                .as_bytes(),
            );
        })?;
        let read = response.payload_len as usize;
        if read > chunk_len || read > response.payload.len() {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&response.payload[..read]);
        if read < chunk_len {
            break;
        }
    }
    Ok(bytes)
}

lazy_static! {
    static ref ROOTD_LIFECYCLE_EVENTS: TrackedSpinLock<ProcessEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(ProcessEventQueue::new());
    static ref PROCD_LIFECYCLE_EVENTS: TrackedSpinLock<ProcessEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(ProcessEventQueue::new());
    static ref SYSCALLD_LIFECYCLE_EVENTS: TrackedSpinLock<ProcessEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(ProcessEventQueue::new());
    static ref VFSD_LIFECYCLE_EVENTS: TrackedSpinLock<ProcessEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(ProcessEventQueue::new());
    /// procd's per-thread exit evidence. Thread exit is orders of magnitude more
    /// frequent than process exit, so it must not share the process queue: a
    /// full process queue makes procd forget every process's signal policy.
    static ref PROCD_THREAD_EXIT_EVENTS: TrackedSpinLock<ThreadExitQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(ThreadExitQueue::new());
}
/// Set (under the thread-exit queue lock) when a thread exit could not be
/// queued. The next drain reports it as `LIFECYCLE_EVENT_THREAD_EXIT_LOST`
/// after the queued exits, so procd can reclaim what it can prove stale.
static PROCD_THREAD_EXIT_LOST: AtomicBool = AtomicBool::new(false);
/// Thread exits buffered for procd between two drains. procd drains before
/// every request, so this only bounds a burst of exits with no procd traffic.
const THREAD_EXIT_QUEUE_CAPACITY: usize = 256;
type ProcessEventQueue = LifecycleEventQueue<LIFECYCLE_DRAIN_MAX_EVENTS>;
type ThreadExitQueue = LifecycleEventQueue<THREAD_EXIT_QUEUE_CAPACITY>;
static ROOTD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static PROCD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static SYSCALLD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static VFSD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static ROOTD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static PROCD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static SYSCALLD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static VFSD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

struct LifecycleEventQueue<const CAPACITY: usize> {
    events: [LifecycleEventWire; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> LifecycleEventQueue<CAPACITY> {
    fn new() -> Self {
        Self {
            events: [LifecycleEventWire::default(); CAPACITY],
            len: 0,
        }
    }

    fn push(&mut self, event: LifecycleEventWire) -> bool {
        let Some(slot) = self.events.get_mut(self.len) else {
            return false;
        };
        *slot = event;
        self.len += 1;
        true
    }

    fn len(&self) -> usize {
        self.len
    }

    fn prefix(&self, count: usize) -> &[LifecycleEventWire] {
        &self.events[..count.min(self.len)]
    }

    fn drain_prefix(&mut self, count: usize) {
        let count = count.min(self.len);
        self.events.copy_within(count..self.len, 0);
        self.len -= count;
        self.events[self.len..].fill(LifecycleEventWire::default());
    }

    fn clear(&mut self) {
        self.events[..self.len].fill(LifecycleEventWire::default());
        self.len = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleConsumer {
    RootSupervisor,
    ProcessPolicy,
    LinuxSyscallPolicy,
    /// vfsd prunes and inherits its per-pid cwd from exit and fork evidence.
    VfsPolicy,
}

impl LifecycleConsumer {
    fn events(
        self,
    ) -> &'static TrackedSpinLock<ProcessEventQueue, { LockClass::CompatLifecycle as u8 }> {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_EVENTS,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_EVENTS,
            Self::LinuxSyscallPolicy => &SYSCALLD_LIFECYCLE_EVENTS,
            Self::VfsPolicy => &VFSD_LIFECYCLE_EVENTS,
        }
    }

    fn overflowed(self) -> &'static AtomicBool {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_EVENTS_OVERFLOWED,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_EVENTS_OVERFLOWED,
            Self::LinuxSyscallPolicy => &SYSCALLD_LIFECYCLE_EVENTS_OVERFLOWED,
            Self::VfsPolicy => &VFSD_LIFECYCLE_EVENTS_OVERFLOWED,
        }
    }

    fn drain_in_progress(self) -> &'static AtomicBool {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_DRAIN_IN_PROGRESS,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_DRAIN_IN_PROGRESS,
            Self::LinuxSyscallPolicy => &SYSCALLD_LIFECYCLE_DRAIN_IN_PROGRESS,
            Self::VfsPolicy => &VFSD_LIFECYCLE_DRAIN_IN_PROGRESS,
        }
    }
}

struct LifecycleDrainClaim {
    state: &'static AtomicBool,
}

impl LifecycleDrainClaim {
    fn acquire(consumer: LifecycleConsumer) -> Result<Self, i64> {
        let state = consumer.drain_in_progress();
        state
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self { state })
            .map_err(|_| LINUX_EBUSY)
    }
}

impl Drop for LifecycleDrainClaim {
    fn drop(&mut self) {
        self.state.store(false, Ordering::Release);
    }
}

pub(crate) fn record_process_exit(pid: u64, parent_pid: u64, exit_status: i32) {
    let event = LifecycleEventWire {
        event: LIFECYCLE_EVENT_EXIT,
        pid,
        parent_pid,
        exit_status,
        ..LifecycleEventWire::default()
    };
    for consumer in [
        LifecycleConsumer::RootSupervisor,
        LifecycleConsumer::ProcessPolicy,
        LifecycleConsumer::LinuxSyscallPolicy,
        LifecycleConsumer::VfsPolicy,
    ] {
        let mut events = consumer.events().lock();
        if !events.push(event) {
            // Each consumer owns independent evidence. Rootd treats overflow
            // as terminal; policy services discard all cached per-process
            // state after observing their private overflow and then resume
            // from an empty queue.
            consumer.overflowed().store(true, Ordering::Release);
        }
    }
}

/// Publish a non-last thread's exit to procd so it can free that thread's
/// signal state without waiting for the whole process to exit. A full queue is
/// remembered as a sticky loss marker instead of an overflow: only the lost
/// thread-exit evidence is affected, never the process-lifecycle queue.
pub(crate) fn record_thread_exit(pid: u64, tid: u64) {
    let event = LifecycleEventWire {
        event: LIFECYCLE_EVENT_THREAD_EXIT,
        pid,
        tid,
        ..LifecycleEventWire::default()
    };
    let mut events = PROCD_THREAD_EXIT_EVENTS.lock();
    if !events.push(event) {
        // The flag is only ever cleared by the drain, which takes this lock.
        PROCD_THREAD_EXIT_LOST.store(true, Ordering::Release);
    }
}

/// Appends as many queued thread exits as fit in `room`, then the loss marker
/// once every queued exit has been taken. Returns how many queue entries were
/// taken and whether the marker was consumed (and so must be restored if the
/// copy-out fails).
fn take_thread_exit_snapshot(
    events: &mut ThreadExitQueue,
    lost: &AtomicBool,
    room: usize,
    out: &mut Vec<LifecycleEventWire>,
) -> (usize, bool) {
    let taken = room.min(events.len());
    out.extend_from_slice(events.prefix(taken));
    let drained_all = taken == events.len();
    if drained_all && room > taken && lost.swap(false, Ordering::AcqRel) {
        out.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_THREAD_EXIT_LOST,
            ..LifecycleEventWire::default()
        });
        return (taken, true);
    }
    (taken, false)
}

/// Publish replacement-MM evidence before the blocked exec caller resumes.
/// Syscalld must discard its old image's VMA/guard cache, but keep credentials.
pub(crate) fn record_process_exec(pid: u64) {
    let consumer = LifecycleConsumer::LinuxSyscallPolicy;
    let event = LifecycleEventWire {
        event: rustos_user_abi::syscall::LIFECYCLE_EVENT_EXEC,
        pid,
        ..LifecycleEventWire::default()
    };
    if !consumer.events().lock().push(event) {
        consumer.overflowed().store(true, Ordering::Release);
    }
}

/// Publish a new child process to vfsd before the child can run. `pid` is the
/// child and `parent_pid` the forking process, so vfsd can copy the parent's
/// cwd into the child's entry before the child's first namespace request. The
/// queue is FIFO and drained before each vfsd request, which orders this
/// event after any earlier exit of a reused pid and before the child's first
/// request; the parent's later `chdir` cannot overtake it.
pub(crate) fn record_process_fork(pid: u64, parent_pid: u64) {
    let consumer = LifecycleConsumer::VfsPolicy;
    let event = LifecycleEventWire {
        event: LIFECYCLE_EVENT_FORK,
        pid,
        parent_pid,
        ..LifecycleEventWire::default()
    };
    if !consumer.events().lock().push(event) {
        consumer.overflowed().store(true, Ordering::Release);
    }
}

/// Withdraw a fork event whose child never became runnable, so vfsd drops the
/// cwd it copied for a pid that no exit event will ever name.
pub(crate) fn record_process_fork_abandoned(pid: u64) {
    let consumer = LifecycleConsumer::VfsPolicy;
    let event = LifecycleEventWire {
        event: LIFECYCLE_EVENT_EXIT,
        pid,
        ..LifecycleEventWire::default()
    };
    if !consumer.events().lock().push(event) {
        consumer.overflowed().store(true, Ordering::Release);
    }
}

pub(super) fn drain_lifecycle_events(
    args: &LifecycleDrainBrokerArgs,
    consumer: LifecycleConsumer,
) -> Result<u64, i64> {
    if args.out_capacity as usize > LIFECYCLE_DRAIN_MAX_EVENTS || args.out_count_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let capacity = args.out_capacity as usize;
    if capacity != 0 && args.out_events_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let _claim = LifecycleDrainClaim::acquire(consumer)?;
    let queue = consumer.events();
    let overflowed = consumer.overflowed();
    // Never access pageable user memory while holding the lifecycle spinlock.
    // The single-consumer claim keeps another drain from changing the prefix;
    // producers may only append to it.
    let mut snapshot = Vec::with_capacity(capacity);
    {
        let mut events = queue.lock();
        if lifecycle_overflow_requires_rebase(consumer, &mut events, overflowed) {
            return Err(LINUX_EOVERFLOW);
        }
        let count = capacity.min(events.len());
        snapshot.extend_from_slice(events.prefix(count));
    }
    let process_count = snapshot.len();
    // procd's thread exits ride behind its process events. The two queues are
    // never locked together, so each keeps its own producer-visible order.
    let (thread_count, thread_loss_taken) = if consumer == LifecycleConsumer::ProcessPolicy {
        take_thread_exit_snapshot(
            &mut PROCD_THREAD_EXIT_EVENTS.lock(),
            &PROCD_THREAD_EXIT_LOST,
            capacity - process_count,
            &mut snapshot,
        )
    } else {
        (0, false)
    };
    let restore_thread_loss = |taken: bool| {
        if taken {
            PROCD_THREAD_EXIT_LOST.store(true, Ordering::Release);
        }
    };
    let count = snapshot.len();
    let out_count = count as u32;
    if out_count != 0 {
        if let Err(error) = usermem::write_current_user_slice(args.out_events_ptr, &snapshot) {
            restore_thread_loss(thread_loss_taken);
            return Err(address_space_error_to_linux_errno(error));
        }
    }
    if let Err(error) = usermem::write_current_user_bytes(args.out_count_ptr, &out_count.to_le_bytes())
    {
        restore_thread_loss(thread_loss_taken);
        return Err(address_space_error_to_linux_errno(error));
    }
    if thread_count != 0 {
        PROCD_THREAD_EXIT_EVENTS.lock().drain_prefix(thread_count);
    }
    let mut events = queue.lock();
    if lifecycle_overflow_requires_rebase(consumer, &mut events, overflowed) {
        return Err(LINUX_EOVERFLOW);
    }
    debug_assert!(events.len() >= process_count);
    events.drain_prefix(process_count);
    Ok(out_count as u64)
}

fn lifecycle_overflow_requires_rebase(
    consumer: LifecycleConsumer,
    events: &mut ProcessEventQueue,
    overflowed: &AtomicBool,
) -> bool {
    if !overflowed.load(Ordering::Acquire) {
        return false;
    }
    if consumer != LifecycleConsumer::RootSupervisor {
        // procd and syscalld treat EOVERFLOW as a command to forget every
        // cached per-process policy. Clearing only that consumer's private
        // queue and sticky bit under the same lock gives it a safe empty
        // baseline without weakening rootd's fatal evidence rule.
        events.clear();
        overflowed.store(false, Ordering::Release);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_lifecycle_queue_rejects_loss_instead_of_dropping_oldest_exit() {
        let mut events = ProcessEventQueue::new();
        for pid in 1..=LIFECYCLE_DRAIN_MAX_EVENTS as u64 {
            assert!(events.push(LifecycleEventWire {
                event: LIFECYCLE_EVENT_EXIT,
                pid,
                ..LifecycleEventWire::default()
            }));
        }
        assert!(!events.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: u64::MAX,
            ..LifecycleEventWire::default()
        }));
        assert_eq!(events.len(), LIFECYCLE_DRAIN_MAX_EVENTS);
        assert_eq!(events.prefix(1)[0].pid, 1);
    }

    #[test]
    fn lifecycle_drain_snapshot_preserves_events_appended_during_copyout() {
        let mut events = ProcessEventQueue::new();
        assert!(events.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 1,
            ..LifecycleEventWire::default()
        }));
        assert!(events.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 2,
            ..LifecycleEventWire::default()
        }));
        let snapshot = events.prefix(1).to_vec();
        assert!(events.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 3,
            ..LifecycleEventWire::default()
        }));

        events.drain_prefix(snapshot.len());

        assert_eq!(events.len(), 2);
        assert_eq!(events.prefix(2)[0].pid, 2);
        assert_eq!(events.prefix(2)[1].pid, 3);
    }

    #[test]
    fn vfsd_overflow_rebases_its_private_queue_only() {
        let mut vfsd = ProcessEventQueue::new();
        assert!(vfsd.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_FORK,
            pid: 9,
            parent_pid: 8,
            ..LifecycleEventWire::default()
        }));
        let overflow = AtomicBool::new(true);
        assert!(lifecycle_overflow_requires_rebase(
            LifecycleConsumer::VfsPolicy,
            &mut vfsd,
            &overflow,
        ));
        assert_eq!(vfsd.len(), 0);
        assert!(!overflow.load(Ordering::Acquire));
    }

    #[test]
    fn lifecycle_fanout_consumers_drain_independently() {
        let event = LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 7,
            ..LifecycleEventWire::default()
        };
        let mut rootd = ProcessEventQueue::new();
        let mut procd = ProcessEventQueue::new();
        let mut syscalld = ProcessEventQueue::new();
        assert!(rootd.push(event));
        assert!(procd.push(event));
        assert!(syscalld.push(event));

        rootd.drain_prefix(1);

        assert_eq!(rootd.len(), 0);
        assert_eq!(procd.len(), 1);
        assert_eq!(procd.prefix(1)[0].pid, 7);
        assert_eq!(syscalld.len(), 1);
        assert_eq!(syscalld.prefix(1)[0].pid, 7);

        let procd_overflow = AtomicBool::new(true);
        assert!(lifecycle_overflow_requires_rebase(
            LifecycleConsumer::ProcessPolicy,
            &mut procd,
            &procd_overflow,
        ));
        assert_eq!(procd.len(), 0);
        assert!(!procd_overflow.load(Ordering::Acquire));

        let syscalld_overflow = AtomicBool::new(true);
        assert!(lifecycle_overflow_requires_rebase(
            LifecycleConsumer::LinuxSyscallPolicy,
            &mut syscalld,
            &syscalld_overflow,
        ));
        assert_eq!(syscalld.len(), 0);
        assert!(!syscalld_overflow.load(Ordering::Acquire));

        let rootd_overflow = AtomicBool::new(true);
        assert!(rootd.push(event));
        assert!(lifecycle_overflow_requires_rebase(
            LifecycleConsumer::RootSupervisor,
            &mut rootd,
            &rootd_overflow,
        ));
        assert_eq!(rootd.len(), 1);
        assert!(rootd_overflow.load(Ordering::Acquire));
    }

    fn thread_exit(tid: u64) -> LifecycleEventWire {
        LifecycleEventWire {
            event: LIFECYCLE_EVENT_THREAD_EXIT,
            pid: 5,
            tid,
            ..LifecycleEventWire::default()
        }
    }

    #[test]
    fn thread_exit_snapshot_takes_queued_exits_then_loss_marker() {
        let mut events = ThreadExitQueue::new();
        let lost = AtomicBool::new(false);
        for tid in 1..=THREAD_EXIT_QUEUE_CAPACITY as u64 {
            assert!(events.push(thread_exit(tid)));
        }
        // The producer observed a full queue and recorded a loss.
        assert!(!events.push(thread_exit(u64::MAX)));
        lost.store(true, Ordering::Release);

        let mut out = Vec::new();
        let (taken, loss) =
            take_thread_exit_snapshot(&mut events, &lost, LIFECYCLE_DRAIN_MAX_EVENTS, &mut out);
        assert_eq!((taken, loss), (LIFECYCLE_DRAIN_MAX_EVENTS, false));
        assert_eq!(out.len(), LIFECYCLE_DRAIN_MAX_EVENTS);
        assert!(lost.load(Ordering::Acquire), "marker waits for the backlog");
        events.drain_prefix(taken);

        // Drain the remaining backlog in batches; the marker comes last.
        loop {
            out.clear();
            let (taken, loss) =
                take_thread_exit_snapshot(&mut events, &lost, LIFECYCLE_DRAIN_MAX_EVENTS, &mut out);
            events.drain_prefix(taken);
            if loss {
                assert_eq!(out.last().unwrap().event, LIFECYCLE_EVENT_THREAD_EXIT_LOST);
                assert_eq!(events.len(), 0);
                break;
            }
            assert!(events.len() != 0 || taken == out.len());
        }
        assert!(!lost.load(Ordering::Acquire));
    }

    #[test]
    fn thread_exit_snapshot_marker_needs_room_and_survives_restore() {
        let mut events = ThreadExitQueue::new();
        let lost = AtomicBool::new(true);
        assert!(events.push(thread_exit(9)));
        let mut out = Vec::new();
        // Exactly one slot: the exit fits, the marker must wait.
        let (taken, loss) = take_thread_exit_snapshot(&mut events, &lost, 1, &mut out);
        assert_eq!((taken, loss), (1, false));
        assert!(lost.load(Ordering::Acquire));
        events.drain_prefix(taken);
        // Room for the marker: it is consumed and the caller can restore it.
        out.clear();
        let (taken, loss) = take_thread_exit_snapshot(&mut events, &lost, 1, &mut out);
        assert_eq!((taken, loss), (0, true));
        assert_eq!(out[0].event, LIFECYCLE_EVENT_THREAD_EXIT_LOST);
        assert!(!lost.load(Ordering::Acquire));
    }

    #[test]
    fn lifecycle_event_wire_layout_is_stable() {
        assert_eq!(core::mem::size_of::<LifecycleEventWire>(), 32);
        assert_eq!(core::mem::offset_of!(LifecycleEventWire, tid), 24);
    }
}
