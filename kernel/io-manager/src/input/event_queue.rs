use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) use crate::input_core::InputEventQueueState;
use crate::sync::{KernelSpinGuard as MutexGuard, KernelSpinLock as Mutex};
use driver_abi::PointerPacket;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

// `crate::user::abi::device::InputEvent` and `crate::input_core::InputEvent`
// are both aliases of `rustos_user_abi::ui::UiInputEvent` via the shared
// `input-evdev` crate, so callers can pass either name through the queue
// without a layout cast.
use crate::user::abi::device::InputEvent;

static POINTER_PACKET_SUBMIT_COUNT: AtomicU64 = AtomicU64::new(0);
static POINTER_ABSOLUTE_SUBMIT_COUNT: AtomicU64 = AtomicU64::new(0);
static INPUT_READ_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
static INPUT_READ_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);
static EVENT_QUEUE_LOCK_ACTIVE: AtomicU64 = AtomicU64::new(0);
static EVENT_QUEUE_LOCK_LAST_SEQ: AtomicU64 = AtomicU64::new(0);
static EVENT_QUEUE_LOCK_NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default)]
pub struct InputEventQueueDebugSnapshot {
    pub pointer_packet_submits: u64,
    pub pointer_absolute_submits: u64,
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

// RING3-MIGRATION-REFERENCE START: inputd should own input queue policy,
// overflow/coalescing state, readers, and observability. Ring0 should keep only
// bounded hardware-report ingress, wakeup, and user-copy/broker primitives.
static INPUT_EVENTS: Mutex<InputEventQueueState> = Mutex::new(InputEventQueueState::new());

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn push_pointer_motion(dx: i16, dy: i16) {
    with_event_queue(|events| events.push_pointer_motion(dx, dy));
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn push_pointer_position(x: u32, y: u32) {
    with_event_queue(|events| events.push_pointer_position(x, y));
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn push_pointer_button(code: u32, pressed: bool) {
    with_event_queue(|events| events.push_pointer_button(code, pressed));
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn push_pointer_scroll(vertical: i16, horizontal: i16) {
    with_event_queue(|events| events.push_pointer_scroll(vertical, horizontal));
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn push_pointer_button_left(pressed: bool) {
    push_pointer_button(crate::input_core::POINTER_BUTTON_LEFT, pressed);
}

pub(crate) fn submit_pointer_packet(packet: PointerPacket, previous_buttons: u8) -> bool {
    POINTER_PACKET_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
    with_event_queue(|events| events.submit_pointer_packet(packet, previous_buttons))
}

pub(crate) fn submit_pointer_absolute(
    x: u32,
    y: u32,
    buttons: u8,
    wheel_vertical: i16,
    previous_buttons: u8,
) -> bool {
    POINTER_ABSOLUTE_SUBMIT_COUNT.fetch_add(1, Ordering::Relaxed);
    with_event_queue(|events| {
        events.submit_pointer_absolute(x, y, buttons, wheel_vertical, previous_buttons)
    })
}

pub(crate) fn read_input_events(dest: &mut [InputEvent]) -> usize {
    let count = with_event_queue(|events| events.read_input_events(dest));
    INPUT_READ_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    INPUT_READ_EVENT_COUNT.fetch_add(count as u64, Ordering::Relaxed);
    count
}

pub(crate) fn has_pending_input_events() -> bool {
    with_event_queue(|events| events.has_pending_events())
}

pub(crate) fn lock_event_queue() -> MutexGuard<'static, InputEventQueueState> {
    INPUT_EVENTS.lock()
}

pub fn debug_snapshot() -> InputEventQueueDebugSnapshot {
    let snapshot = INPUT_EVENTS
        .try_lock()
        .map(|events| events.snapshot())
        .unwrap_or_default();
    InputEventQueueDebugSnapshot {
        pointer_packet_submits: POINTER_PACKET_SUBMIT_COUNT.load(Ordering::Relaxed),
        pointer_absolute_submits: POINTER_ABSOLUTE_SUBMIT_COUNT.load(Ordering::Relaxed),
        read_calls: INPUT_READ_CALL_COUNT.load(Ordering::Relaxed),
        read_events: INPUT_READ_EVENT_COUNT.load(Ordering::Relaxed),
        lock_active: EVENT_QUEUE_LOCK_ACTIVE.load(Ordering::Acquire),
        lock_last_seq: EVENT_QUEUE_LOCK_LAST_SEQ.load(Ordering::Acquire),
        queued: snapshot.queued,
        pending_coalesced: snapshot.pending_coalesced,
        pending_pointer_position: snapshot.pending_pointer_position,
        dropped_discrete: snapshot.dropped_discrete,
        dropped_lossy: snapshot.dropped_lossy,
    }
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *INPUT_EVENTS.lock() = InputEventQueueState::new();
    POINTER_PACKET_SUBMIT_COUNT.store(0, Ordering::Relaxed);
    POINTER_ABSOLUTE_SUBMIT_COUNT.store(0, Ordering::Relaxed);
    INPUT_READ_CALL_COUNT.store(0, Ordering::Relaxed);
    INPUT_READ_EVENT_COUNT.store(0, Ordering::Relaxed);
    EVENT_QUEUE_LOCK_ACTIVE.store(0, Ordering::Relaxed);
    EVENT_QUEUE_LOCK_LAST_SEQ.store(0, Ordering::Relaxed);
    EVENT_QUEUE_LOCK_NEXT_SEQ.store(1, Ordering::Relaxed);
}

fn with_event_queue<R>(f: impl FnOnce(&mut InputEventQueueState) -> R) -> R {
    let seq = EVENT_QUEUE_LOCK_NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    EVENT_QUEUE_LOCK_LAST_SEQ.store(seq, Ordering::Release);
    EVENT_QUEUE_LOCK_ACTIVE.fetch_add(1, Ordering::AcqRel);

    #[cfg(test)]
    {
        let result = f(&mut INPUT_EVENTS.lock());
        EVENT_QUEUE_LOCK_LAST_SEQ.store(seq, Ordering::Release);
        EVENT_QUEUE_LOCK_ACTIVE.fetch_sub(1, Ordering::AcqRel);
        result
    }

    #[cfg(not(test))]
    {
        let result = interrupts::without_interrupts(|| f(&mut INPUT_EVENTS.lock()));
        EVENT_QUEUE_LOCK_LAST_SEQ.store(seq, Ordering::Release);
        EVENT_QUEUE_LOCK_ACTIVE.fetch_sub(1, Ordering::AcqRel);
        result
    }
}
// RING3-MIGRATION-REFERENCE END: inputd-owned input queue policy.

#[cfg(test)]
mod tests {
    use super::{
        debug_snapshot, push_pointer_button_left, push_pointer_motion, read_input_events,
        reset_for_tests,
    };
    use crate::user::abi::device::{
        INPUT_ACTION_PRESSED, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION, InputEvent,
    };

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    #[test]
    fn wrapper_reads_motion_and_button_events() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_motion(5, -2);
        push_pointer_button_left(true);

        let mut events = [InputEvent::default(); 2];
        let read = read_input_events(&mut events);
        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].value0, 5);
        assert_eq!(events[0].value1, -2);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].action, INPUT_ACTION_PRESSED);
    }

    #[test]
    fn debug_snapshot_reflects_queue_state() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_motion(1, 1);

        let snapshot = debug_snapshot();
        assert!(snapshot.pending_coalesced);
        assert_eq!(snapshot.queued, 0);
    }
}
