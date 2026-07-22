use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use lazy_static::lazy_static;
use rustos_user_abi::syscall::{
    LIFECYCLE_DRAIN_MAX_EVENTS, LIFECYCLE_EVENT_EXIT, LifecycleDrainBrokerArgs, LifecycleEventWire,
    VFS_IPC_OP_PREAD64, VFS_IPC_PAYLOAD_CAPACITY,
};
use spin::Mutex;

use super::*;

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
        let response = call_vfs_ipc_request(&request)?;
        ensure_vfs_status(&response)?;
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
    static ref ROOTD_LIFECYCLE_EVENTS: Mutex<Vec<LifecycleEventWire>> = Mutex::new(Vec::new());
    static ref PROCD_LIFECYCLE_EVENTS: Mutex<Vec<LifecycleEventWire>> = Mutex::new(Vec::new());
}
static ROOTD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static PROCD_LIFECYCLE_EVENTS_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static ROOTD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static PROCD_LIFECYCLE_DRAIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleConsumer {
    RootSupervisor,
    ProcessPolicy,
}

impl LifecycleConsumer {
    fn events(self) -> &'static Mutex<Vec<LifecycleEventWire>> {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_EVENTS,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_EVENTS,
        }
    }

    fn overflowed(self) -> &'static AtomicBool {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_EVENTS_OVERFLOWED,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_EVENTS_OVERFLOWED,
        }
    }

    fn drain_in_progress(self) -> &'static AtomicBool {
        match self {
            Self::RootSupervisor => &ROOTD_LIFECYCLE_DRAIN_IN_PROGRESS,
            Self::ProcessPolicy => &PROCD_LIFECYCLE_DRAIN_IN_PROGRESS,
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

fn push_lifecycle_event(events: &mut Vec<LifecycleEventWire>, event: LifecycleEventWire) -> bool {
    if events.len() >= LIFECYCLE_DRAIN_MAX_EVENTS {
        return false;
    }
    events.push(event);
    true
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
    ] {
        let mut events = consumer.events().lock();
        if !push_lifecycle_event(&mut events, event) {
            // Each consumer owns independent evidence. Rootd treats overflow
            // as terminal; procd discards all cached policy after observing
            // its overflow and then resumes from an empty queue.
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
    let snapshot = {
        let mut events = queue.lock();
        if lifecycle_overflow_requires_rebase(consumer, &mut events, overflowed) {
            return Err(LINUX_EOVERFLOW);
        }
        let count = capacity.min(events.len());
        events[..count].to_vec()
    };
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
    events.drain(..count);
    Ok(out_count as u64)
}

fn lifecycle_overflow_requires_rebase(
    consumer: LifecycleConsumer,
    events: &mut Vec<LifecycleEventWire>,
    overflowed: &AtomicBool,
) -> bool {
    if !overflowed.load(Ordering::Acquire) {
        return false;
    }
    if consumer == LifecycleConsumer::ProcessPolicy {
        // procd treats EOVERFLOW as a command to forget every cached policy.
        // Clearing this consumer's private queue and sticky bit under the same
        // lock gives it a safe empty baseline without weakening rootd's fatal
        // evidence rule.
        events.clear();
        overflowed.store(false, Ordering::Release);
    }
    true
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn full_lifecycle_queue_rejects_loss_instead_of_dropping_oldest_exit() {
        let mut events = Vec::new();
        for pid in 1..=LIFECYCLE_DRAIN_MAX_EVENTS as u64 {
            assert!(push_lifecycle_event(
                &mut events,
                LifecycleEventWire {
                    event: LIFECYCLE_EVENT_EXIT,
                    pid,
                    ..LifecycleEventWire::default()
                }
            ));
        }
        assert!(!push_lifecycle_event(
            &mut events,
            LifecycleEventWire {
                event: LIFECYCLE_EVENT_EXIT,
                pid: u64::MAX,
                ..LifecycleEventWire::default()
            }
        ));
        assert_eq!(events.len(), LIFECYCLE_DRAIN_MAX_EVENTS);
        assert_eq!(events[0].pid, 1);
    }

    #[test]
    fn lifecycle_drain_snapshot_preserves_events_appended_during_copyout() {
        let mut events = vec![
            LifecycleEventWire {
                event: LIFECYCLE_EVENT_EXIT,
                pid: 1,
                ..LifecycleEventWire::default()
            },
            LifecycleEventWire {
                event: LIFECYCLE_EVENT_EXIT,
                pid: 2,
                ..LifecycleEventWire::default()
            },
        ];
        let snapshot = events[..1].to_vec();
        events.push(LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 3,
            ..LifecycleEventWire::default()
        });

        events.drain(..snapshot.len());

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].pid, 2);
        assert_eq!(events[1].pid, 3);
    }

    #[test]
    fn lifecycle_fanout_consumers_drain_independently() {
        let event = LifecycleEventWire {
            event: LIFECYCLE_EVENT_EXIT,
            pid: 7,
            ..LifecycleEventWire::default()
        };
        let mut rootd = Vec::new();
        let mut procd = Vec::new();
        assert!(push_lifecycle_event(&mut rootd, event));
        assert!(push_lifecycle_event(&mut procd, event));

        rootd.drain(..1);

        assert!(rootd.is_empty());
        assert_eq!(procd.len(), 1);
        assert_eq!(procd[0].pid, 7);

        let procd_overflow = AtomicBool::new(true);
        assert!(lifecycle_overflow_requires_rebase(
            LifecycleConsumer::ProcessPolicy,
            &mut procd,
            &procd_overflow,
        ));
        assert!(procd.is_empty());
        assert!(!procd_overflow.load(Ordering::Acquire));

        let rootd_overflow = AtomicBool::new(true);
        rootd.push(event);
        assert!(lifecycle_overflow_requires_rebase(
            LifecycleConsumer::RootSupervisor,
            &mut rootd,
            &rootd_overflow,
        ));
        assert_eq!(rootd.len(), 1);
        assert!(rootd_overflow.load(Ordering::Acquire));
    }
}
