use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use super::*;
use lazy_static::lazy_static;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::syscall::{
    LIFECYCLE_DRAIN_MAX_EVENTS, LIFECYCLE_EVENT_EXIT, LifecycleDrainBrokerArgs, LifecycleEventWire,
    VFS_IPC_OP_PREAD64, VFS_IPC_PAYLOAD_CAPACITY,
};

pub(crate) fn call_remote_vfs_read_bytes(
    remote_id: u64,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, i64> {
    let mut bytes = Vec::new();
    while bytes.len() < len {
        let chunk_len = (len - bytes.len()).min(VFS_IPC_PAYLOAD_CAPACITY);
        let mut request = new_vfs_request(VFS_IPC_OP_PREAD64);
        request.remote_id = remote_id;
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
    static ref ROOTD_LIFECYCLE_EVENTS: TrackedSpinLock<LifecycleEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(LifecycleEventQueue::new());
    static ref PROCD_LIFECYCLE_EVENTS: TrackedSpinLock<LifecycleEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(LifecycleEventQueue::new());
    static ref SYSCALLD_LIFECYCLE_EVENTS: TrackedSpinLock<LifecycleEventQueue, { LockClass::CompatLifecycle as u8 }> =
        TrackedSpinLock::new(LifecycleEventQueue::new());
}
static ROOTD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static PROCD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static SYSCALLD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static ROOTD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static PROCD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static SYSCALLD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

struct LifecycleEventQueue {
    events: [LifecycleEventWire; LIFECYCLE_DRAIN_MAX_EVENTS],
    len: usize,
}

impl LifecycleEventQueue {
    fn new() -> Self {
        Self {
            events: [LifecycleEventWire::default(); LIFECYCLE_DRAIN_MAX_EVENTS],
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
}

impl LifecycleConsumer {
    fn events(
        self,
    ) -> &'static TrackedSpinLock<LifecycleEventQueue, { LockClass::CompatLifecycle as u8 }> {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_EVENTS,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_EVENTS,
            Self::LinuxSyscallPolicy => &SYSCALLD_LIFECYCLE_EVENTS,
        }
    }

    fn overflowed(self) -> &'static AtomicBool {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_EVENTS_OVERFLOWED,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_EVENTS_OVERFLOWED,
            Self::LinuxSyscallPolicy => &SYSCALLD_LIFECYCLE_EVENTS_OVERFLOWED,
        }
    }

    fn drain_in_progress(self) -> &'static AtomicBool {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_DRAIN_IN_PROGRESS,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_DRAIN_IN_PROGRESS,
            Self::LinuxSyscallPolicy => &SYSCALLD_LIFECYCLE_DRAIN_IN_PROGRESS,
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
    let count = snapshot.len();
    let out_count = count as u32;
    if out_count != 0 {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                snapshot.as_ptr().cast::<u8>(),
                count * core::mem::size_of::<LifecycleEventWire>(),
            )
        };
        usermem::write_current_user_bytes(args.out_events_ptr, bytes)
            .map_err(address_space_error_to_linux_errno)?;
    }
    usermem::write_current_user_bytes(args.out_count_ptr, &out_count.to_le_bytes())
        .map_err(address_space_error_to_linux_errno)?;
    let mut events = queue.lock();
    if lifecycle_overflow_requires_rebase(consumer, &mut events, overflowed) {
        return Err(LINUX_EOVERFLOW);
    }
    debug_assert!(events.len() >= count);
    events.drain_prefix(count);
    Ok(out_count as u64)
}

fn lifecycle_overflow_requires_rebase(
    consumer: LifecycleConsumer,
    events: &mut LifecycleEventQueue,
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
        let mut events = LifecycleEventQueue::new();
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
        let mut events = LifecycleEventQueue::new();
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
    fn lifecycle_fanout_consumers_drain_independently() {
        let event = LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 7,
            ..LifecycleEventWire::default()
        };
        let mut rootd = LifecycleEventQueue::new();
        let mut procd = LifecycleEventQueue::new();
        let mut syscalld = LifecycleEventQueue::new();
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
}
