// RING3-MIGRATION-REFERENCE START: input-ingress exception: inputd owns input
// queue coalescing, drop policy, and evdev/native read policy. Ring0 keeps
// bounded hardware ingress buffers and stats substrate.
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::KernelSpinLock as Mutex;
use driver_abi::PointerPacket;
use heapless::Deque as HeaplessDeque;
use rustos_user_abi::syscall::{
    INPUTD_ACCESS_NATIVE, INPUTD_INGRESS_FLAG_DVM_SOURCE, INPUTD_INGRESS_FLAG_RESET_STATE,
    INPUTD_INGRESS_KIND_DVM_LINUX_KEY, INPUTD_INGRESS_KIND_POINTER_PACKET, InputIngressWire,
    InputKeyboardEventWire, InputPointerPacketWire,
};
#[cfg(not(test))]
use x86_64::instructions::interrupts;

static POINTER_PACKET_SUBMIT_COUNT: AtomicU64 = AtomicU64::new(0);
static INPUT_READ_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
static INPUT_READ_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);
static INPUT_INGRESS_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
const INPUT_INGRESS_QUEUE_CAPACITY: usize = 2048;
const INPUT_WAITERS_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Default)]
pub struct InputEventQueueDebugSnapshot {
    pub pointer_packet_submits: u64,
    pub read_calls: u64,
    pub read_events: u64,
    pub lock_active: u64,
    pub lock_last_seq: u64,
    pub queued: usize,
    pub pending_coalesced: bool,
    pub pending_pointer_position: bool,
    pub dropped_discrete: u64,
    pub dropped_lossy: u64,
}

static INPUT_INGRESS: Mutex<HeaplessDeque<InputIngressWire, INPUT_INGRESS_QUEUE_CAPACITY>> =
    Mutex::new(HeaplessDeque::new());
static INPUT_WAITERS: Mutex<[Option<u64>; INPUT_WAITERS_CAPACITY]> =
    Mutex::new([None; INPUT_WAITERS_CAPACITY]);

/// Queue a Linux `EV_KEY` transition only after the L0 DVM relay has checked
/// its source, framing, sequence, and code range.  `inputd` owns Linux-keymap
/// translation and modifier/text state; ring0 only carries the bounded event.
pub(crate) fn submit_dvm_linux_key(action: u16, linux_key_code: u16) -> bool {
    let mut ingress = InputIngressWire::default();
    ingress.kind = INPUTD_INGRESS_KIND_DVM_LINUX_KEY;
    ingress.access = INPUTD_ACCESS_NATIVE;
    // Preserve the authenticated relay provenance all the way to inputd.
    // The service rejects untagged Linux-key ingress so native producers
    // cannot impersonate the DVM channel.
    ingress.flags = INPUTD_INGRESS_FLAG_DVM_SOURCE;
    ingress.keyboard = InputKeyboardEventWire {
        action,
        reserved0: 0,
        code: linux_key_code as u32,
        modifiers: 0,
        text: 0,
    };
    push_ingress(ingress)
}

/// Clear DVM-owned keyboard and pointer state after an authenticated relay
/// session ends. The host emits releases first; this is a fail-safe state
/// reset so a reconnect cannot inherit modifiers or button state.
pub(crate) fn submit_dvm_input_reset() -> bool {
    let mut ingress = InputIngressWire::default();
    ingress.kind = INPUTD_INGRESS_KIND_DVM_LINUX_KEY;
    ingress.access = INPUTD_ACCESS_NATIVE;
    ingress.flags = INPUTD_INGRESS_FLAG_RESET_STATE | INPUTD_INGRESS_FLAG_DVM_SOURCE;
    // A reset is a revocation barrier, not another input sample.  Purge any
    // queued frames from the retired DVM session so an adversarial or wedged
    // producer cannot make old key presses overtake its disconnect cleanup.
    push_dvm_reset_ingress(ingress)
}

/// Queue a normalized pointer packet from the authenticated DVM relay. The
/// source flag proves that ring-3 inputd received an authenticated DVM record.
pub(crate) fn submit_dvm_pointer_packet(packet: PointerPacket) -> bool {
    POINTER_PACKET_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut ingress = InputIngressWire::default();
    ingress.kind = INPUTD_INGRESS_KIND_POINTER_PACKET;
    ingress.access = INPUTD_ACCESS_NATIVE;
    ingress.flags = INPUTD_INGRESS_FLAG_DVM_SOURCE;
    ingress.pointer_packet = InputPointerPacketWire {
        buttons: packet.buttons,
        reserved0: [0; 3],
        dx: packet.dx,
        dy: packet.dy,
        wheel_vertical: packet.wheel_vertical,
        wheel_horizontal: packet.wheel_horizontal,
    };
    push_ingress(ingress)
}

pub(crate) fn has_pending_input_events() -> bool {
    has_pending_ingress()
}

pub(crate) fn arm_input_waiter(task_id: u64) -> bool {
    with_input_waiters(|waiters| {
        if waiters.iter().any(|waiter| *waiter == Some(task_id)) {
            return true;
        }
        for waiter in waiters.iter_mut() {
            if waiter.is_none() {
                *waiter = Some(task_id);
                return true;
            }
        }
        false
    })
}

pub(crate) fn disarm_input_waiter(task_id: u64) {
    with_input_waiters(|waiters| {
        for waiter in waiters.iter_mut() {
            if *waiter == Some(task_id) {
                *waiter = None;
            }
        }
    });
}

pub fn debug_snapshot() -> InputEventQueueDebugSnapshot {
    let ingress_queued = INPUT_INGRESS.lock().len();
    InputEventQueueDebugSnapshot {
        pointer_packet_submits: POINTER_PACKET_SUBMIT_COUNT.load(Ordering::Relaxed),
        read_calls: INPUT_READ_CALL_COUNT.load(Ordering::Relaxed),
        read_events: INPUT_READ_EVENT_COUNT.load(Ordering::Relaxed),
        lock_active: 0,
        lock_last_seq: 0,
        queued: ingress_queued,
        pending_coalesced: false,
        pending_pointer_position: false,
        dropped_discrete: 0,
        dropped_lossy: INPUT_INGRESS_DROP_COUNT.load(Ordering::Relaxed),
    }
}

pub(crate) fn drain_ingress(dest: &mut [InputIngressWire]) -> usize {
    let count = with_ingress_queue(|ingress| {
        let mut count = 0;
        for slot in dest.iter_mut() {
            let Some(wire) = ingress.pop_front() else {
                break;
            };
            *slot = wire;
            count += 1;
        }
        count
    });
    INPUT_READ_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    INPUT_READ_EVENT_COUNT.fetch_add(count as u64, Ordering::Relaxed);
    count
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    // InputIngressWire carries a full bounded payload. Reconstructing all
    // 2,048 slots as a temporary places the complete queue on the host test
    // thread's stack and can overflow it before the test reaches the queue.
    // Clear the static queue in place instead; production never uses this
    // helper and retains the same fixed allocation.
    INPUT_INGRESS.lock().clear();
    *INPUT_WAITERS.lock() = [None; INPUT_WAITERS_CAPACITY];
    POINTER_PACKET_SUBMIT_COUNT.store(0, Ordering::Relaxed);
    INPUT_READ_CALL_COUNT.store(0, Ordering::Relaxed);
    INPUT_READ_EVENT_COUNT.store(0, Ordering::Relaxed);
    INPUT_INGRESS_DROP_COUNT.store(0, Ordering::Relaxed);
}

fn push_ingress(wire: InputIngressWire) -> bool {
    let pushed = with_ingress_queue(|ingress| {
        if ingress.push_back(wire).is_ok() {
            true
        } else {
            INPUT_INGRESS_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            false
        }
    });
    if pushed {
        wake_input_waiters();
    }
    pushed
}

fn push_dvm_reset_ingress(wire: InputIngressWire) -> bool {
    let discarded = with_ingress_queue(|ingress| replace_pending_with_reset(ingress, wire));
    if discarded != 0 {
        INPUT_INGRESS_DROP_COUNT.fetch_add(discarded as u64, Ordering::Relaxed);
    }
    wake_input_waiters();
    true
}

fn replace_pending_with_reset<const CAPACITY: usize>(
    ingress: &mut HeaplessDeque<InputIngressWire, CAPACITY>,
    reset: InputIngressWire,
) -> usize {
    let discarded = ingress.len();
    ingress.clear();
    debug_assert!(ingress.push_back(reset).is_ok());
    discarded
}

fn has_pending_ingress() -> bool {
    with_ingress_queue(|ingress| !ingress.is_empty())
}

fn with_ingress_queue<R>(
    f: impl FnOnce(&mut HeaplessDeque<InputIngressWire, INPUT_INGRESS_QUEUE_CAPACITY>) -> R,
) -> R {
    #[cfg(test)]
    {
        f(&mut INPUT_INGRESS.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut INPUT_INGRESS.lock()))
    }
}

fn wake_input_waiters() {
    let mut task_ids = [0_u64; INPUT_WAITERS_CAPACITY];
    let count = with_input_waiters(|waiters| {
        let mut count = 0;
        for waiter in waiters.iter_mut() {
            if let Some(task_id) = waiter.take() {
                task_ids[count] = task_id;
                count += 1;
            }
        }
        count
    });
    for task_id in task_ids.iter().take(count).copied() {
        let _ = crate::multitask::wake_task(task_id);
    }
}

fn with_input_waiters<R>(f: impl FnOnce(&mut [Option<u64>; INPUT_WAITERS_CAPACITY]) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut INPUT_WAITERS.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut INPUT_WAITERS.lock()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        debug_snapshot, drain_ingress, replace_pending_with_reset, reset_for_tests,
        submit_dvm_pointer_packet,
    };
    use driver_abi::{POINTER_BUTTON_LEFT as POINTER_PACKET_LEFT, PointerPacket};
    use heapless::Deque as HeaplessDeque;
    use rustos_user_abi::syscall::{
        INPUTD_INGRESS_FLAG_DVM_SOURCE, INPUTD_INGRESS_FLAG_RESET_STATE,
        INPUTD_INGRESS_KIND_DVM_LINUX_KEY, INPUTD_INGRESS_KIND_POINTER_PACKET, InputIngressWire,
    };

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    #[test]
    fn pointer_packet_reaches_ingress_queue() {
        let _guard = isolated();
        reset_for_tests();

        assert!(submit_dvm_pointer_packet(PointerPacket {
            buttons: POINTER_PACKET_LEFT,
            dx: 5,
            dy: -2,
            wheel_vertical: 0,
            wheel_horizontal: 0,
            reserved0: 0,
            reserved1: 0,
            reserved2: 0,
        }));

        let mut ingress = [InputIngressWire::default(); 2];
        assert_eq!(drain_ingress(&mut ingress), 1);
        assert_eq!(ingress[0].kind, INPUTD_INGRESS_KIND_POINTER_PACKET);
        assert_eq!(ingress[0].pointer_packet.buttons, POINTER_PACKET_LEFT);
        assert_eq!(ingress[0].pointer_packet.dx, 5);
        assert_eq!(ingress[0].pointer_packet.dy, -2);
    }

    #[test]
    fn debug_snapshot_reflects_ingress_state() {
        let _guard = isolated();
        reset_for_tests();
        assert!(submit_dvm_pointer_packet(PointerPacket {
            buttons: 0,
            dx: 1,
            dy: 1,
            wheel_vertical: 0,
            wheel_horizontal: 0,
            reserved0: 0,
            reserved1: 0,
            reserved2: 0,
        }));

        let snapshot = debug_snapshot();
        assert!(!snapshot.pending_coalesced);
        assert_eq!(snapshot.queued, 1);
    }

    #[test]
    fn dvm_reset_is_a_priority_revocation_barrier() {
        let mut ingress = HeaplessDeque::<InputIngressWire, 4>::new();
        assert!(ingress.push_back(InputIngressWire::default()).is_ok());
        assert!(ingress.push_back(InputIngressWire::default()).is_ok());
        let mut reset = InputIngressWire::default();
        reset.kind = INPUTD_INGRESS_KIND_DVM_LINUX_KEY;
        reset.flags = INPUTD_INGRESS_FLAG_DVM_SOURCE | INPUTD_INGRESS_FLAG_RESET_STATE;
        assert_eq!(replace_pending_with_reset(&mut ingress, reset), 2);
        assert_eq!(ingress.len(), 1);
        let queued = ingress.pop_front().unwrap();
        assert_eq!(queued.kind, INPUTD_INGRESS_KIND_DVM_LINUX_KEY);
        assert_eq!(
            queued.flags,
            INPUTD_INGRESS_FLAG_DVM_SOURCE | INPUTD_INGRESS_FLAG_RESET_STATE
        );
    }
}
// RING3-MIGRATION-REFERENCE END: inputd-owned input event queue ingress exception.
