//! Input transport consumer, validation, session policy, and readiness owner.
//!
//! - **Owner:** `inputd` owns DVM record validation, translation, session
//!   authority, policy queue, and readiness generation.
//! - **Boundary:** Transport records, session changes, reader authorization,
//!   and cross-service lifecycle replies are untrusted.
//! - **Lifecycle:** Acquire exact consumer lease, ingest bounded batches,
//!   validate/translate, publish generation, authorize/read, reset/revoke, and
//!   withdraw on owner exit.
//! - **Concurrency:** Ingestion uses atomic arm/recheck and bounded queue turns;
//!   local locks are released before netd/session authority calls.
//! - **Failure:** Malformed sequence/checksum/input, queue pressure, timeout,
//!   consumer exit, session reset, and transport revoke preserve edge events
//!   or fail explicitly.
//! - **Forbidden:** No ring0 decode, polling, lossy key/button coalescing,
//!   foreign reader, or native-device fallback.
//! - **Evidence:** `input-delivery-lifecycle`, `dvm-input-ingress`, and
//!   `waitset`.
use std::collections::VecDeque;
use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

mod dvm_protocol;
mod dvm_session_sync;
mod service_loop;

use keyboard_core::{KeyAction, KeyboardDriver, KeyboardEvent};
use rustos_user_abi::syscall::{
    identity_is_exact_sender, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, InputDvmRecordWire, InputIngestBrokerArgs, InputIngressWire,
    InputPointerPacketWire, InputPointerPositionWire, InputStatsBrokerArgs, InputStatsWire,
    InputdIpcRequest, InputdIpcResponse, InputdPointerSurfaceRequest, InputdReadResponse,
    NetdIpcRequest, NetdIpcResponse, COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE, COMMERCIAL_MAX_INPUTD_OP_INPUT_READER,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS, COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_INPUTD, INPUTD_ACCESS_EVDEV, INPUTD_ACCESS_NATIVE,
    INPUTD_INGEST_MAX_EVENTS, INPUTD_INGRESS_FLAG_DVM_SOURCE, INPUTD_INGRESS_FLAG_RESET_STATE,
    INPUTD_INGRESS_KIND_DVM_LINUX_KEY, INPUTD_INGRESS_KIND_POINTER_PACKET,
    INPUTD_INGRESS_KIND_POINTER_POSITION, INPUTD_IPC_ABI_VERSION, INPUTD_IPC_OP_AUTHORIZE_READ,
    INPUTD_IPC_OP_PING, INPUTD_IPC_OP_READ, INPUTD_IPC_OP_SET_POINTER_SURFACE, INPUTD_IPC_OP_STATS,
    INPUTD_READ_FLAG_NONBLOCK, INPUTD_READ_PAYLOAD_CAPACITY, IPC_MAX_INLINE_BYTES,
    IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IPC_SERVICE_UISERVER, NETD_DVM_SESSION_GRANT,
    NETD_DVM_SESSION_REVOKE, NETD_IPC_ABI_VERSION, NETD_IPC_OP_DVM_SESSION,
    NETD_IPC_REQUEST_HEADER_SIZE, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_INPUT_INGEST_BROKER,
    SYS_RUSTOS_INPUT_STATS_BROKER, SYS_RUSTOS_INPUT_WAIT_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
};
#[cfg(not(test))]
use rustos_user_abi::syscall::{
    WaitSetSignalBrokerArgs, SYS_RUSTOS_WAITSET_SIGNAL_BROKER, WAITSET_ABI_VERSION,
    WAITSET_INPUT_EVDEV_OBJECT_ID, WAITSET_INPUT_NATIVE_OBJECT_ID, WAITSET_PROVIDER_INPUTD,
    WAITSET_SIGNAL_FLAG_READY,
};

// The DVM ingestion worker waits on the MSI-X-published ring and transfers
// bounded batches into this user-space queue independently of app reads. App
// requests still own reader authorization and event serialization; no polling
// client is allowed to become the liveness dependency for transport progress.
const INPUTD_QUEUE_MAX_EVENTS: usize = 4096;
static REPLY_FAILURE_DIAGNOSTICS: rustos_svc_runtime::ipc::ReplyFailureDiagnostics =
    rustos_svc_runtime::ipc::ReplyFailureDiagnostics::new();
const INPUTD_MAX_NATIVE_READ_BYTES: u64 = input_evdev::MAX_NATIVE_READ_BYTES as u64;
const INPUTD_MAX_EVDEV_READ_BYTES: u64 = input_evdev::MAX_EVDEV_READ_BYTES as u64;
#[derive(Clone, Copy, Debug, Default)]
struct PointerSurface {
    width: u32,
    height: u32,
    generation: u64,
}

impl PointerSurface {
    fn configured(self) -> bool {
        self.width > 0 && self.height > 0
    }
}

struct InputQueue {
    events: VecDeque<input_evdev::InputEvent>,
    dvm_keyboard: KeyboardDriver,
    dvm_keyboard_observed: bool,
    dvm_pointer_observed: bool,
    pointer_surface: PointerSurface,
    dropped_discrete: u64,
    dropped_lossy: u64,
    dvm_pointer_buttons: u8,
    published_pointer_buttons: u8,
    dvm_pointer_position: Option<(i32, i32)>,
    read_authorizations: VecDeque<InputReadAuthorization>,
    readiness_generation: u64,
}

impl Default for InputQueue {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            dvm_keyboard: KeyboardDriver::default(),
            dvm_keyboard_observed: false,
            dvm_pointer_observed: false,
            pointer_surface: PointerSurface::default(),
            dropped_discrete: 0,
            dropped_lossy: 0,
            dvm_pointer_buttons: 0,
            published_pointer_buttons: 0,
            dvm_pointer_position: None,
            read_authorizations: VecDeque::new(),
            readiness_generation: 1,
        }
    }
}

struct SharedInputQueueState {
    queue: Mutex<InputQueue>,
    handoff: Mutex<InputQueueHandoff>,
    handoff_changed: Condvar,
}

#[derive(Default)]
struct InputQueueHandoff {
    ingestion_waiting: bool,
}

impl SharedInputQueueState {
    fn new() -> Self {
        Self {
            queue: Mutex::new(InputQueue::default()),
            handoff: Mutex::new(InputQueueHandoff::default()),
            handoff_changed: Condvar::new(),
        }
    }
}

type SharedInputQueue = Arc<SharedInputQueueState>;

#[derive(Default)]
struct DvmIngressLogState {
    keyboard: AtomicBool,
    pointer: AtomicBool,
}

impl DvmIngressLogState {
    fn claim(&self, keyboard: bool, pointer: bool) -> (bool, bool) {
        (
            keyboard && !self.keyboard.swap(true, Ordering::AcqRel),
            pointer && !self.pointer.swap(true, Ordering::AcqRel),
        )
    }
}

fn lock_input_queue(queue: &SharedInputQueue) -> MutexGuard<'_, InputQueue> {
    loop {
        let mut handoff = queue.handoff.lock().unwrap_or_else(|_| {
            debug_line("inputd: input queue handoff synchronization failed");
            std::process::exit(134);
        });
        while handoff.ingestion_waiting {
            handoff = queue.handoff_changed.wait(handoff).unwrap_or_else(|_| {
                debug_line("inputd: input queue handoff wait failed");
                std::process::exit(134);
            });
        }
        drop(handoff);
        let guard = queue.queue.lock().unwrap_or_else(|_| {
            debug_line("inputd: input queue synchronization failed");
            std::process::exit(134);
        });
        let handoff = queue.handoff.lock().unwrap_or_else(|_| {
            debug_line("inputd: input queue handoff synchronization failed");
            std::process::exit(134);
        });
        if !handoff.ingestion_waiting {
            return guard;
        }
        drop(handoff);
        drop(guard);
    }
}

fn lock_input_queue_for_ingestion(queue: &SharedInputQueue) -> MutexGuard<'_, InputQueue> {
    {
        let mut handoff = queue.handoff.lock().unwrap_or_else(|_| {
            debug_line("inputd: input queue handoff synchronization failed");
            std::process::exit(134);
        });
        handoff.ingestion_waiting = true;
    }
    // Block behind the exact current owner. Readers that observe this handoff
    // sleep on a condition variable rather than remaining runnable at the
    // service scheduling class, so the designated ingestion worker always gets
    // the turn needed to acquire the queue and return ring credit.
    let guard = queue.queue.lock().unwrap_or_else(|_| {
        debug_line("inputd: input queue synchronization failed");
        std::process::exit(134);
    });
    {
        let mut handoff = queue.handoff.lock().unwrap_or_else(|_| {
            debug_line("inputd: input queue handoff synchronization failed");
            std::process::exit(134);
        });
        handoff.ingestion_waiting = false;
        queue.handoff_changed.notify_all();
    }
    guard
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputReadAuthorization {
    pid: u64,
    tid: u64,
    fd: u64,
    access: u16,
    approved_len: u64,
}

impl InputQueue {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn push(&mut self, event: input_evdev::InputEvent) {
        if let Some(back) = self.events.back_mut() {
            if can_coalesce(*back, event) {
                merge_coalesced_event(back, event);
                return;
            }
        }
        if !self.make_room_for(event) {
            return;
        }
        let was_empty = self.events.is_empty();
        self.events.push_back(event);
        if was_empty {
            self.advance_readiness_generation();
        }
    }

    fn make_room_for(&mut self, event: input_evdev::InputEvent) -> bool {
        if self.events.len() < INPUTD_QUEUE_MAX_EVENTS {
            return true;
        }
        if self.drop_oldest_lossy_event() {
            return true;
        }
        if is_lossy_pointer_event(event) {
            self.dropped_lossy = self.dropped_lossy.saturating_add(1);
            return false;
        }
        let _ = self.events.pop_front();
        self.dropped_discrete = self.dropped_discrete.saturating_add(1);
        true
    }

    fn drop_oldest_lossy_event(&mut self) -> bool {
        let Some(index) = self
            .events
            .iter()
            .position(|event| is_lossy_pointer_event(*event))
        else {
            return false;
        };
        let _ = self.events.remove(index);
        self.dropped_lossy = self.dropped_lossy.saturating_add(1);
        true
    }

    fn pop_front(&mut self) -> Option<input_evdev::InputEvent> {
        let event = self.events.pop_front();
        if event.is_some() && self.events.is_empty() {
            self.advance_readiness_generation();
        }
        event
    }

    /// Publish every readiness transition, in both directions.
    ///
    /// Only the arrival edge used to be published, so ring0 knew when the
    /// queue filled but never when it drained, and could not answer a
    /// readiness question from what it already held - it had to ask over IPC.
    /// A queue that goes empty is a readiness fact of exactly the same kind as
    /// one that goes non-empty, and publishing both is what lets the waitset
    /// answer locally.
    fn advance_readiness_generation(&mut self) {
        self.readiness_generation = self
            .readiness_generation
            .checked_add(1)
            .expect("inputd readiness generation exhausted");
        publish_readiness(self.readiness_generation, !self.events.is_empty());
    }

    fn set_pointer_surface(&mut self, width: u32, height: u32, generation: u64) -> Result<(), i32> {
        if width == 0 || height == 0 {
            return Err(libc::EINVAL);
        }
        if self.pointer_surface.configured() && generation < self.pointer_surface.generation {
            return Err(libc::ESTALE);
        }
        self.pointer_surface = PointerSurface {
            width,
            height,
            generation,
        };
        Ok(())
    }

    /// Translate a Linux `EV_KEY` value from the DVM only in ring 3.  This
    /// keeps layout, modifiers, lock state, and printable text in ring 3
    /// instead of leaking Linux codes into apps.
    fn push_dvm_linux_key(&mut self, linux_code: u32, action: u16) {
        let Some(code) = input_evdev::linux_key_code_to_rustos(linux_code) else {
            return;
        };
        let released = match action {
            input_evdev::INPUT_ACTION_PRESSED | input_evdev::INPUT_ACTION_REPEATED => false,
            input_evdev::INPUT_ACTION_RELEASED => true,
            _ => return,
        };
        self.dvm_keyboard_observed = true;
        self.dvm_keyboard.inject_key_transition(code, released);
        while let Some(event) = self.dvm_keyboard.pop_event() {
            self.push_keyboard_driver_event(event);
        }
    }

    fn reset_dvm_input(&mut self) {
        self.dvm_keyboard.reset_provider_state();
        while let Some(event) = self.dvm_keyboard.pop_event() {
            self.push_keyboard_driver_event(event);
        }
        self.dvm_pointer_buttons = 0;
        self.dvm_pointer_position = None;
        self.publish_pointer_button_edges();
    }

    fn push_pointer_motion_and_scroll(&mut self, packet: InputPointerPacketWire) {
        if packet.dx != 0 || packet.dy != 0 {
            self.push(input_evdev::InputEvent {
                kind: input_evdev::INPUT_KIND_POINTER_MOTION,
                action: input_evdev::INPUT_ACTION_NONE,
                code: 0,
                value0: packet.dx as i32,
                value1: packet.dy as i32,
                modifiers: 0,
                text: 0,
            });
        }
        if packet.wheel_vertical != 0 || packet.wheel_horizontal != 0 {
            self.push(input_evdev::InputEvent {
                kind: input_evdev::INPUT_KIND_POINTER_SCROLL,
                action: input_evdev::INPUT_ACTION_NONE,
                code: 0,
                value0: packet.wheel_vertical as i32,
                value1: packet.wheel_horizontal as i32,
                modifiers: 0,
                text: 0,
            });
        }
    }

    fn push_dvm_pointer_packet(&mut self, packet: InputPointerPacketWire) {
        if packet.dx != 0
            || packet.dy != 0
            || packet.wheel_vertical != 0
            || packet.wheel_horizontal != 0
            || packet.buttons != self.dvm_pointer_buttons
        {
            self.dvm_pointer_observed = true;
        }
        self.push_pointer_motion_and_scroll(packet);
        self.dvm_pointer_buttons = packet.buttons;
        self.publish_pointer_button_edges();
    }

    fn push_dvm_pointer_position(&mut self, position: InputPointerPositionWire) {
        if position.x < 0 || position.y < 0 {
            return;
        }
        if self.pointer_surface.configured()
            && (position.x as u32 >= self.pointer_surface.width
                || position.y as u32 >= self.pointer_surface.height)
        {
            return;
        }
        let coordinates = (position.x, position.y);
        let changed = self.dvm_pointer_position != Some(coordinates);
        if changed {
            self.push(input_evdev::InputEvent {
                kind: input_evdev::INPUT_KIND_POINTER_POSITION,
                action: input_evdev::INPUT_ACTION_NONE,
                code: 0,
                value0: position.x,
                value1: position.y,
                modifiers: 0,
                text: 0,
            });
            self.dvm_pointer_position = Some(coordinates);
        }
        if position.wheel_vertical != 0 || position.wheel_horizontal != 0 {
            self.push(input_evdev::InputEvent {
                kind: input_evdev::INPUT_KIND_POINTER_SCROLL,
                action: input_evdev::INPUT_ACTION_NONE,
                code: 0,
                value0: position.wheel_vertical as i32,
                value1: position.wheel_horizontal as i32,
                modifiers: 0,
                text: 0,
            });
        }
        if changed
            || position.wheel_vertical != 0
            || position.wheel_horizontal != 0
            || position.buttons != self.dvm_pointer_buttons
        {
            self.dvm_pointer_observed = true;
        }
        self.dvm_pointer_buttons = position.buttons;
        self.publish_pointer_button_edges();
    }

    /// Return the first accepted DVM keyboard/pointer observations since the
    /// previous poll. These markers are intentionally one-shot diagnostics:
    /// they prove the end-to-end ingress route without turning normal input
    /// into a per-event logging channel.
    fn take_dvm_ingress_observations(&mut self) -> (bool, bool) {
        let keyboard = std::mem::take(&mut self.dvm_keyboard_observed);
        let pointer = std::mem::take(&mut self.dvm_pointer_observed);
        (keyboard, pointer)
    }

    fn push_keyboard_driver_event(&mut self, event: KeyboardEvent) {
        self.push(input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_KEYBOARD,
            action: keyboard_action_to_inputd(event.action),
            code: event.code as u32,
            value0: 0,
            value1: 0,
            modifiers: event.modifiers.bits() as u32,
            text: event.text.unwrap_or(0) as u32,
        });
    }

    fn publish_pointer_button_edges(&mut self) {
        let current = self.dvm_pointer_buttons;
        let previous = self.published_pointer_buttons;
        self.published_pointer_buttons = current;
        self.push_pointer_button_edge(previous, current, input_evdev::POINTER_BUTTON_LEFT as u8);
        self.push_pointer_button_edge(previous, current, input_evdev::POINTER_BUTTON_RIGHT as u8);
        self.push_pointer_button_edge(previous, current, input_evdev::POINTER_BUTTON_MIDDLE as u8);
        self.push_pointer_button_edge(previous, current, input_evdev::POINTER_BUTTON_X1 as u8);
        self.push_pointer_button_edge(previous, current, input_evdev::POINTER_BUTTON_X2 as u8);
    }

    fn push_pointer_button_edge(&mut self, previous: u8, current: u8, button_mask: u8) {
        let was_pressed = previous & button_mask != 0;
        let is_pressed = current & button_mask != 0;
        if was_pressed == is_pressed {
            return;
        }
        self.push(input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_POINTER_BUTTON,
            action: if is_pressed {
                input_evdev::INPUT_ACTION_PRESSED
            } else {
                input_evdev::INPUT_ACTION_RELEASED
            },
            code: button_mask as u32,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: 0,
        });
    }
}

fn can_coalesce(existing: input_evdev::InputEvent, next: input_evdev::InputEvent) -> bool {
    existing.kind == next.kind
        && existing.kind != input_evdev::INPUT_KIND_KEYBOARD
        && existing.kind != input_evdev::INPUT_KIND_POINTER_BUTTON
        && existing.action == next.action
        && existing.code == next.code
        && existing.modifiers == next.modifiers
        && existing.text == next.text
}

fn is_lossy_pointer_event(event: input_evdev::InputEvent) -> bool {
    matches!(
        event.kind,
        input_evdev::INPUT_KIND_POINTER_MOTION | input_evdev::INPUT_KIND_POINTER_POSITION
    )
}

fn merge_coalesced_event(existing: &mut input_evdev::InputEvent, next: input_evdev::InputEvent) {
    if existing.kind == input_evdev::INPUT_KIND_POINTER_POSITION {
        existing.value0 = next.value0;
        existing.value1 = next.value1;
        return;
    }
    existing.value0 = existing.value0.saturating_add(next.value0);
    existing.value1 = existing.value1.saturating_add(next.value1);
}

fn keyboard_action_to_inputd(action: KeyAction) -> u16 {
    match action {
        KeyAction::Pressed => input_evdev::INPUT_ACTION_PRESSED,
        KeyAction::Released => input_evdev::INPUT_ACTION_RELEASED,
        KeyAction::Repeated => input_evdev::INPUT_ACTION_REPEATED,
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{
        apply_dvm_ingress_wire, ingest_batch_needs_immediate_retry, lock_input_queue,
        lock_input_queue_for_ingestion, validate_commercial_request, DvmIngressLogState,
        InputQueue, SharedInputQueueState, INPUTD_INGEST_MAX_EVENTS,
    };
    use crate::dvm_session_sync;
    use rustos_user_abi::syscall::{
        CommercialMaxProtocolRequest, InputIngressWire, InputPointerPacketWire,
        InputPointerPositionWire, InputdIpcRequest, COMMERCIAL_MAX_INPUTD_OP_INPUT_READER,
        COMMERCIAL_MAX_PROTOCOL_INPUTD, INPUTD_ACCESS_NATIVE, INPUTD_INGRESS_FLAG_DVM_SOURCE,
        INPUTD_INGRESS_FLAG_RESET_STATE, INPUTD_INGRESS_KIND_DVM_LINUX_KEY,
    };
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    #[test]
    fn netd_dvm_session_request_uses_one_nonzero_transaction_end() {
        let deadline = rustos_user_abi::deadline::AbsoluteDeadline::after(2_000_000, 80_000_000);
        let (request, waiter_ms) = super::netd_dvm_session_request_with_deadline(
            7,
            rustos_user_abi::syscall::NETD_DVM_SESSION_GRANT,
            41,
            43,
            deadline,
            42_000_000,
        )
        .expect("deadline still has budget");

        assert_eq!(request.deadline_ns, deadline.end_ns());
        assert_ne!(request.deadline_ns, 0);
        assert_eq!(waiter_ms, 40);
        let (_, retry_waiter_ms) = super::netd_dvm_session_request_with_deadline(
            7,
            rustos_user_abi::syscall::NETD_DVM_SESSION_GRANT,
            41,
            43,
            deadline,
            81_999_999,
        )
        .expect("the final nanosecond rounds up to one bounded millisecond");
        assert_eq!(retry_waiter_ms, 1);
        assert_eq!(
            super::netd_dvm_session_request_with_deadline(
                7,
                rustos_user_abi::syscall::NETD_DVM_SESSION_GRANT,
                41,
                43,
                deadline,
                deadline.end_ns(),
            ),
            Err(libc::ETIMEDOUT)
        );

        let transaction = rustos_user_abi::deadline::AbsoluteDeadline::after(0, 5_000_000_000);
        let (request, call_cap_ms) = super::netd_dvm_session_request_with_deadline(
            7,
            rustos_user_abi::syscall::NETD_DVM_SESSION_GRANT,
            41,
            43,
            transaction,
            0,
        )
        .expect("five-second transaction is live");
        assert_eq!(request.deadline_ns, transaction.end_ns());
        assert_eq!(call_cap_ms, dvm_session_sync::CALL_DEADLINE_MS);
        assert!(
            u64::from(call_cap_ms).saturating_mul(rustos_user_abi::deadline::NANOS_PER_MILLI)
                < transaction.end_ns(),
            "interactive call cap must not replace the immutable transaction end"
        );
    }

    fn pointer_motion(dx: i32, dy: i32) -> input_evdev::InputEvent {
        input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_POINTER_MOTION,
            action: input_evdev::INPUT_ACTION_NONE,
            code: 0,
            value0: dx,
            value1: dy,
            modifiers: 0,
            text: 0,
        }
    }

    #[test]
    fn ingestion_handoff_prevents_hot_reader_mutex_barging() {
        let queue = Arc::new(SharedInputQueueState::new());
        let held = queue.queue.lock().unwrap();
        let (tx, rx) = mpsc::channel();

        let ingestion_queue = Arc::clone(&queue);
        let ingestion_tx = tx.clone();
        let ingestion = std::thread::spawn(move || {
            let _guard = lock_input_queue_for_ingestion(&ingestion_queue);
            ingestion_tx.send("ingestion").unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !queue.handoff.lock().unwrap().ingestion_waiting {
            assert!(
                Instant::now() < deadline,
                "ingestion did not declare handoff"
            );
            std::thread::yield_now();
        }

        let reader_queue = Arc::clone(&queue);
        let reader = std::thread::spawn(move || {
            let _guard = lock_input_queue(&reader_queue);
            tx.send("reader").unwrap();
        });
        drop(held);

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "ingestion"
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "reader");
        ingestion.join().unwrap();
        reader.join().unwrap();
    }

    fn pointer_position(x: i32, y: i32) -> input_evdev::InputEvent {
        input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_POINTER_POSITION,
            action: input_evdev::INPUT_ACTION_NONE,
            code: 0,
            value0: x,
            value1: y,
            modifiers: 0,
            text: 0,
        }
    }

    #[test]
    fn readiness_generation_closes_empty_queue_lost_wake_window() {
        let mut queue = InputQueue::default();
        let initial = queue.readiness_generation;
        queue.push(pointer_motion(1, 0));
        assert_eq!(queue.readiness_generation, initial + 1);
        queue.push(pointer_motion(2, 0));
        assert_eq!(queue.readiness_generation, initial + 1);
        assert!(queue.pop_front().is_some());
        assert_eq!(queue.readiness_generation, initial + 2);
    }

    fn pointer_packet(buttons: u8, dx: i16, dy: i16) -> InputPointerPacketWire {
        InputPointerPacketWire {
            buttons,
            reserved0: [0; 3],
            dx,
            dy,
            wheel_vertical: 0,
            wheel_horizontal: 0,
        }
    }

    #[test]
    fn inputd_queue_coalesces_pointer_motion_samples() {
        let mut queue = InputQueue::default();
        queue.push(pointer_motion(3, -2));
        queue.push(pointer_motion(4, 6));

        assert_eq!(queue.len(), 1);
        let event = queue.pop_front().unwrap();
        assert_eq!(event.value0, 7);
        assert_eq!(event.value1, 4);
    }

    #[test]
    fn inputd_queue_keeps_latest_pointer_position_sample() {
        let mut queue = InputQueue::default();
        queue.push(pointer_position(10, 20));
        queue.push(pointer_position(30, 40));

        assert_eq!(queue.len(), 1);
        let event = queue.pop_front().unwrap();
        assert_eq!(event.value0, 30);
        assert_eq!(event.value1, 40);
    }

    #[test]
    fn authenticated_absolute_pointer_duplicates_are_idempotent() {
        let mut queue = InputQueue::default();
        queue.set_pointer_surface(1600, 900, 1).unwrap();
        let position = InputPointerPositionWire {
            x: 800,
            y: 450,
            ..InputPointerPositionWire::default()
        };
        queue.push_dvm_pointer_position(position);
        queue.push_dvm_pointer_position(position);

        assert_eq!(queue.len(), 1);
        let event = queue.pop_front().unwrap();
        assert_eq!(event.kind, input_evdev::INPUT_KIND_POINTER_POSITION);
        assert_eq!((event.value0, event.value1), (800, 450));

        queue.push_dvm_pointer_position(InputPointerPositionWire {
            x: 1600,
            y: 450,
            ..position
        });
        assert!(queue.is_empty());
    }

    #[test]
    fn inputd_queue_preserves_pointer_button_edges_between_positions() {
        let mut queue = InputQueue::default();
        queue.push(pointer_position(10, 20));
        queue.push_dvm_pointer_packet(pointer_packet(1, 0, 0));
        queue.push(pointer_position(30, 40));

        assert_eq!(queue.len(), 3);
        assert_eq!(
            queue.pop_front().unwrap().kind,
            input_evdev::INPUT_KIND_POINTER_POSITION
        );
        let button = queue.pop_front().unwrap();
        assert_eq!(button.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(button.action, input_evdev::INPUT_ACTION_PRESSED);
        let position = queue.pop_front().unwrap();
        assert_eq!(position.kind, input_evdev::INPUT_KIND_POINTER_POSITION);
        assert_eq!(position.value0, 30);
        assert_eq!(position.value1, 40);
    }

    #[test]
    fn inputd_queue_overflow_drops_lossy_pointer_samples_before_button_edges() {
        let mut queue = InputQueue::default();
        for index in 0..super::INPUTD_QUEUE_MAX_EVENTS {
            queue
                .events
                .push_back(pointer_position(index as i32, index as i32));
        }

        queue.push_dvm_pointer_packet(pointer_packet(1, 0, 0));

        assert_eq!(queue.len(), super::INPUTD_QUEUE_MAX_EVENTS);
        assert_eq!(queue.dropped_lossy, 1);
        assert_eq!(queue.dropped_discrete, 0);
        assert!(queue.events.iter().any(|event| {
            event.kind == input_evdev::INPUT_KIND_POINTER_BUTTON
                && event.action == input_evdev::INPUT_ACTION_PRESSED
        }));
    }

    #[test]
    fn inputd_queue_owns_pointer_button_edges_from_raw_reports() {
        let mut queue = InputQueue::default();
        queue.push_dvm_pointer_packet(pointer_packet(1, 7, -3));

        assert_eq!(queue.len(), 2);
        let motion = queue.pop_front().unwrap();
        assert_eq!(motion.kind, input_evdev::INPUT_KIND_POINTER_MOTION);
        assert_eq!(motion.value0, 7);
        assert_eq!(motion.value1, -3);
        let button = queue.pop_front().unwrap();
        assert_eq!(button.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(button.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(button.code, input_evdev::POINTER_BUTTON_LEFT);

        queue.push_dvm_pointer_packet(pointer_packet(0, 0, 0));
        let release = queue.pop_front().unwrap();
        assert_eq!(release.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(release.action, input_evdev::INPUT_ACTION_RELEASED);
        assert_eq!(release.code, input_evdev::POINTER_BUTTON_LEFT);
    }

    #[test]
    fn inputd_dvm_reset_releases_dvm_keyboard_and_pointer_state() {
        let mut queue = InputQueue::default();
        queue.push_dvm_linux_key(29, input_evdev::INPUT_ACTION_PRESSED);
        queue.push_dvm_linux_key(30, input_evdev::INPUT_ACTION_PRESSED);
        let _ = queue.pop_front();
        let _ = queue.pop_front();
        queue.push_dvm_pointer_packet(pointer_packet(1, 0, 0));
        let press = queue.pop_front().unwrap();
        assert_eq!(press.action, input_evdev::INPUT_ACTION_PRESSED);

        queue.reset_dvm_input();
        let ctrl_release = queue.pop_front().unwrap();
        assert_eq!(ctrl_release.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(ctrl_release.action, input_evdev::INPUT_ACTION_RELEASED);
        assert_eq!(ctrl_release.modifiers, 0);
        let key_release = queue.pop_front().unwrap();
        assert_eq!(key_release.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(key_release.action, input_evdev::INPUT_ACTION_RELEASED);
        let pointer_release = queue.pop_front().unwrap();
        assert_eq!(pointer_release.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(pointer_release.action, input_evdev::INPUT_ACTION_RELEASED);
        assert!(queue.is_empty());
    }

    #[test]
    fn inputd_accepts_the_dvm_reset_barrier_and_rejects_unknown_flags() {
        let mut queue = InputQueue::default();
        queue.push_dvm_linux_key(29, input_evdev::INPUT_ACTION_PRESSED);
        let _ = queue.pop_front();

        let mut reset = InputIngressWire {
            kind: INPUTD_INGRESS_KIND_DVM_LINUX_KEY,
            access: INPUTD_ACCESS_NATIVE,
            flags: INPUTD_INGRESS_FLAG_DVM_SOURCE | INPUTD_INGRESS_FLAG_RESET_STATE,
            ..InputIngressWire::default()
        };
        apply_dvm_ingress_wire(&mut queue, &reset);
        assert_eq!(
            queue.pop_front().unwrap().action,
            input_evdev::INPUT_ACTION_RELEASED
        );

        reset.flags |= 1 << 15;
        apply_dvm_ingress_wire(&mut queue, &reset);
        assert!(queue.is_empty());
    }

    #[test]
    fn dvm_linux_evdev_key_preserves_text_and_modifier_policy() {
        let mut queue = InputQueue::default();
        // Linux KEY_LEFTSHIFT then KEY_A: only inputd translates the Linux
        // codes and computes the printable native event.
        queue.push_dvm_linux_key(42, input_evdev::INPUT_ACTION_PRESSED);
        queue.push_dvm_linux_key(30, input_evdev::INPUT_ACTION_PRESSED);

        let shift = queue.pop_front().unwrap();
        assert_eq!(shift.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(shift.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_ne!(shift.modifiers, 0);

        let a = queue.pop_front().unwrap();
        assert_eq!(a.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(a.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(a.text, b'A' as u32);
        assert_eq!(queue.take_dvm_ingress_observations(), (true, false));
        assert_eq!(queue.take_dvm_ingress_observations(), (false, false));
    }

    #[test]
    fn dvm_pointer_observation_requires_a_real_packet_change() {
        let mut queue = InputQueue::default();
        queue.push_dvm_pointer_packet(pointer_packet(0, 0, 0));
        assert_eq!(queue.take_dvm_ingress_observations(), (false, false));

        queue.push_dvm_pointer_packet(pointer_packet(1, 3, -2));
        assert_eq!(queue.take_dvm_ingress_observations(), (false, true));
    }

    #[test]
    fn dvm_ingress_lifecycle_markers_are_claimed_once_across_workers() {
        let state = DvmIngressLogState::default();
        assert_eq!(state.claim(true, false), (true, false));
        assert_eq!(state.claim(true, true), (false, true));
        assert_eq!(state.claim(true, true), (false, false));
    }

    #[test]
    fn full_dvm_ingest_batch_retries_without_requiring_another_irq() {
        assert!(!ingest_batch_needs_immediate_retry(0));
        assert!(!ingest_batch_needs_immediate_retry(
            INPUTD_INGEST_MAX_EVENTS - 1
        ));
        assert!(ingest_batch_needs_immediate_retry(INPUTD_INGEST_MAX_EVENTS));
    }

    #[test]
    fn inputd_read_authorization_is_bound_to_pid_tid_fd_and_access() {
        let mut queue = InputQueue::default();
        queue
            .read_authorizations
            .push_back(super::InputReadAuthorization {
                pid: 10,
                tid: 11,
                fd: 3,
                access: rustos_user_abi::syscall::INPUTD_ACCESS_EVDEV,
                approved_len: 64,
            });

        let wrong_fd = InputdIpcRequest {
            pid: 10,
            tid: 11,
            fd: 4,
            access: rustos_user_abi::syscall::INPUTD_ACCESS_EVDEV,
            ..InputdIpcRequest::default()
        };
        assert_eq!(
            super::consume_read_authorization(&mut queue, &wrong_fd),
            None
        );

        let matching = InputdIpcRequest { fd: 3, ..wrong_fd };
        assert_eq!(
            super::consume_read_authorization(&mut queue, &matching),
            Some(64)
        );
        assert_eq!(
            super::consume_read_authorization(&mut queue, &matching),
            None
        );
    }

    #[test]
    fn commercial_reader_rejects_noncanonical_access_values() {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_INPUTD;
        request.header.op = COMMERCIAL_MAX_INPUTD_OP_INPUT_READER;
        request.arg0 = u64::from(INPUTD_ACCESS_NATIVE);
        assert_eq!(validate_commercial_request(&request), Ok(()));

        request.arg0 |= 1_u64 << 32;
        assert_eq!(validate_commercial_request(&request), Err(libc::EINVAL));
    }
}

fn main() {
    observability_client::info!("inputd", service, "service started");
    service_loop::run();
}

/// Runs at the input-service priority but has no policy authority beyond the
/// existing inputd capability. It sleeps on the MSI-X-associated wait broker,
/// drains at most one ABI-bounded batch, and yields before a recovery backlog
/// can take another batch. This is the QNX-style interrupt-to-service handoff:
/// device progress is independent of any application's read cadence.
fn start_dvm_ingestion_worker(queue: SharedInputQueue, log_state: Arc<DvmIngressLogState>) {
    let result = thread::Builder::new()
        .name(String::from("inputd-dvm-ingress"))
        .spawn(move || {
            let mut ingest_scratch = vec![InputDvmRecordWire::default(); INPUTD_INGEST_MAX_EVENTS];
            let mut pending_events = Vec::with_capacity(INPUTD_INGEST_MAX_EVENTS);
            let mut decoder = dvm_protocol::DvmDecoder::default();
            let mut retry_without_wait = false;
            let mut total_drained = 0_u64;
            let mut next_progress_report = 1_u64;
            let mut nonempty_batches = 0_u64;
            loop {
                if !retry_without_wait {
                    if let Err(errno) = wait_for_dvm_ingress() {
                        debug_line(&format!("inputd: DVM ingestion wait failed errno={errno}"));
                        std::process::exit(134);
                    }
                }
                // The consumer cursor is the only acknowledgement L0 sees and it
                // advances once per broker call, so the credit window is
                // governed by this whole turn rather than by the copy. Split
                // the turn so the dominant phase is a reading, not a guess.
                let turn_started_ns = dvm_session_sync::monotonic_nanos();
                let drained = match drain_transport(&mut ingest_scratch) {
                    Ok(count) => count,
                    Err(errno) => {
                        debug_line(&format!("inputd: DVM ingestion drain failed errno={errno}"));
                        std::process::exit(134);
                    }
                };
                let drain_done_ns = dvm_session_sync::monotonic_nanos();
                let mut outcomes = ingest_scratch[..drained]
                    .iter()
                    .map(|record| decoder.consume(record))
                    .collect::<Vec<_>>();
                let decode_done_ns = dvm_session_sync::monotonic_nanos();
                total_drained = total_drained.saturating_add(drained as u64);
                if drained != 0 {
                    nonempty_batches = nonempty_batches.saturating_add(1);
                }
                let report_progress = drained != 0
                    && (nonempty_batches == 1 || total_drained >= next_progress_report); // First proof, then interval-bound to avoid debugcon boot serialization.
                if report_progress {
                    debug_line(&format!(
                        "inputd: DVM transport progress records={total_drained} batch={drained} batch_seq={nonempty_batches} stage=decoded"
                    ));
                }
                // Start the sync clock after the stage line, not before it.
                // Reporting happens on one turn in 256, and a debugcon line is
                // a port write per byte; leaving it inside the window made the
                // sampled turn the only expensive one and charged its cost to
                // the phase under investigation. The stage lines still bracket
                // the sync so a hang leaves the same evidence it always did.
                let sync_started_ns = dvm_session_sync::monotonic_nanos();
                let session_sync_deadline = rustos_user_abi::deadline::AbsoluteDeadline::after(
                    sync_started_ns,
                    dvm_session_sync::TIMEOUT_NS,
                );
                let mut session_sync_attempts = 0_u32;
                let observations = loop {
                    match dvm_session_sync::apply(
                        &queue,
                        outcomes.as_mut_slice(),
                        &mut pending_events,
                        session_sync_deadline,
                        notify_netd_dvm_session,
                    ) {
                        Ok(observations) => break observations,
                        Err(errno) => {
                            session_sync_attempts = session_sync_attempts.saturating_add(1);
                            if session_sync_attempts == 1 || session_sync_attempts.is_power_of_two() {
                                debug_line(&format!(
                                    "inputd: DVM session authority sync retry errno={errno} attempt={session_sync_attempts}"
                                ));
                            }
                            let Ok(backoff_ns) = session_sync_deadline.retry_backoff_ns(
                                dvm_session_sync::monotonic_nanos(),
                                dvm_session_sync::retry_backoff_for_attempt(session_sync_attempts),
                            ) else {
                                debug_line(&format!(
                                    "inputd: DVM session authority sync timed out errno={errno} attempts={session_sync_attempts}"
                                ));
                                std::process::exit(134);
                            };
                            // SESSION-CUSTODY: keep this decoded, bounded batch
                            // and its decoder epoch private until netd admits
                            // every ordered revoke/grant. Draining later ring
                            // records or resetting the decoder here would lose
                            // the sole authenticated SESSION_START and silently
                            // discard all subsequent input from the live epoch.
                            thread::sleep(Duration::from_nanos(backoff_ns));
                        }
                    }
                };
                let sync_done_ns = report_progress
                    .then(dvm_session_sync::monotonic_nanos)
                    .unwrap_or(sync_started_ns);
                if report_progress {
                    debug_line(&format!(
                        "inputd: DVM transport progress records={total_drained} batch={drained} batch_seq={nonempty_batches} stage=published"
                    ));
                    if total_drained >= next_progress_report {
                        next_progress_report = total_drained.saturating_add(256);
                    }
                    let drain_ns = drain_done_ns.saturating_sub(turn_started_ns);
                    let decode_ns = decode_done_ns.saturating_sub(drain_done_ns);
                    let sync_ns = sync_done_ns.saturating_sub(sync_started_ns);
                    // The three phases sum to the turn. Wall time from the wake
                    // to here does not, because the stage lines above sit
                    // between them and are not work the other 255 turns do.
                    debug_line(&format!(
                        "inputd: DVM turn split records={total_drained} batch={drained} drain_us={} decode_us={} sync_us={} turn_us={}",
                        drain_ns / 1_000,
                        decode_ns / 1_000,
                        sync_ns / 1_000,
                        drain_ns.saturating_add(decode_ns).saturating_add(sync_ns) / 1_000,
                    ));
                }
                log_dvm_ingress_observation_flags(&log_state, observations);
                retry_without_wait = ingest_batch_needs_immediate_retry(drained);
                if retry_without_wait {
                    // A hostile or recovering producer cannot retain the
                    // interactive class across unbounded batches. Continue
                    // after the yield without waiting for another MSI-X edge:
                    // a full batch is proof that backlog may remain. L0 rings
                    // every committed record to close the stale-cursor wake
                    // race, but this worker does not depend on a later record
                    // arriving to finish an already-admitted batch.
                    thread::yield_now();
                }
            }
        });
    if result.is_err() {
        debug_line("inputd: DVM ingestion worker spawn failed");
        std::process::exit(134);
    }
}

fn ingest_batch_needs_immediate_retry(drained: usize) -> bool {
    drained == INPUTD_INGEST_MAX_EVENTS
}

fn wait_for_dvm_ingress() -> Result<(), i32> {
    let result = syscall0(SYS_RUSTOS_INPUT_WAIT_BROKER);
    if result < 0 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

fn log_dvm_ingress_observations(queue: &SharedInputQueue, state: &DvmIngressLogState) {
    let observations = lock_input_queue(queue).take_dvm_ingress_observations();
    log_dvm_ingress_observation_flags(state, observations);
}

fn log_dvm_ingress_observation_flags(
    state: &DvmIngressLogState,
    (dvm_keyboard, dvm_pointer): (bool, bool),
) {
    let (log_keyboard, log_pointer) = state.claim(dvm_keyboard, dvm_pointer);
    if log_keyboard {
        debug_line("inputd: DVM keyboard ingress observed");
    }
    if log_pointer {
        debug_line("inputd: DVM pointer ingress observed");
    }
}

fn dispatch(
    request: &InputdIpcRequest,
    response: &mut InputdIpcResponse,
    queue: &mut InputQueue,
) -> i32 {
    match request.op {
        INPUTD_IPC_OP_PING => {
            response.approved_len = request.requested_len;
            0
        }
        INPUTD_IPC_OP_STATS => match fetch_stats(queue) {
            Ok(stats) => {
                response.stats = stats;
                0
            }
            Err(errno) => errno,
        },
        INPUTD_IPC_OP_AUTHORIZE_READ => authorize_read(request, response, queue),
        _ => libc::EINVAL,
    }
}

fn dispatch_pointer_surface_request(
    request: &InputdPointerSurfaceRequest,
    queue: &mut InputQueue,
) -> i32 {
    if request.version != INPUTD_IPC_ABI_VERSION
        || request.op != INPUTD_IPC_OP_SET_POINTER_SURFACE
        || request.flags != 0
        || request.reserved0 != 0
    {
        return libc::EINVAL;
    }
    queue
        .set_pointer_surface(request.width, request.height, request.generation)
        .err()
        .unwrap_or(0)
}

fn dispatch_read(
    request: &InputdIpcRequest,
    response: &mut InputdReadResponse,
    queue: &mut InputQueue,
) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    let Some(approved_len) = consume_read_authorization(queue, request) else {
        return libc::EACCES;
    };
    let requested = request
        .requested_len
        .min(approved_len)
        .min(INPUTD_READ_PAYLOAD_CAPACITY as u64) as usize;
    let status = match request.access {
        INPUTD_ACCESS_NATIVE => fill_native_payload(queue, &mut response.payload, requested),
        INPUTD_ACCESS_EVDEV => fill_evdev_payload(queue, &mut response.payload, requested),
        _ => return libc::EINVAL,
    };
    let Ok(len) = status else {
        return status.err().unwrap_or(libc::EINVAL);
    };
    response.payload_len = len as u32;
    match fetch_stats(queue) {
        Ok(stats) => response.stats = stats,
        Err(errno) => return errno,
    }
    0
}

fn drain_transport(records: &mut [InputDvmRecordWire]) -> Result<usize, i32> {
    if records.len() != INPUTD_INGEST_MAX_EVENTS {
        debug_line(&format!(
            "inputd: invalid ingest scratch length={} expected={}",
            records.len(),
            INPUTD_INGEST_MAX_EVENTS
        ));
        return Err(libc::EINVAL);
    }
    let mut count = 0_u32;
    let args = InputIngestBrokerArgs {
        abi_version: INPUTD_IPC_ABI_VERSION,
        reserved0: 0,
        reserved1: 0,
        out_records_ptr: records.as_mut_ptr() as u64,
        out_capacity: records.len() as u32,
        reserved2: 0,
        out_count_ptr: (&mut count as *mut u32) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_INPUT_INGEST_BROKER,
        (&args as *const InputIngestBrokerArgs) as u64,
    );
    if result < 0 {
        let errno = last_errno();
        debug_line(&format!(
            "inputd: ingest broker syscall rejected raw={result} errno={errno}"
        ));
        return Err(errno);
    }
    Ok((count as usize).min(records.len()))
}

fn netd_dvm_session_request_with_deadline(
    epoch: u32,
    action: u64,
    pid: u64,
    tid: u64,
    deadline: rustos_user_abi::deadline::AbsoluteDeadline,
    now_ns: u64,
) -> Result<(NetdIpcRequest, u64), i32> {
    // Preserve the immutable transaction end on the wire. The bounded IPC
    // syscall receives only this attempt's remaining interactive-control
    // budget; kernel and netd enforce the effective end as
    // `min(transaction_end, admission_time + class_cap)`.
    let timeout_ms = deadline
        .child_timeout_ms(now_ns, dvm_session_sync::CALL_DEADLINE_MS)
        .map_err(|_| libc::ETIMEDOUT)?;
    Ok((
        NetdIpcRequest {
            version: NETD_IPC_ABI_VERSION,
            op: NETD_IPC_OP_DVM_SESSION,
            pid,
            tid,
            arg0: u64::from(epoch),
            arg1: action,
            deadline_ns: deadline.end_ns(),
            ..NetdIpcRequest::default()
        },
        timeout_ms,
    ))
}

fn notify_netd_dvm_session(
    epoch: u32,
    action: u64,
    deadline: rustos_user_abi::deadline::AbsoluteDeadline,
) -> Result<(), i32> {
    if epoch == 0 || !matches!(action, NETD_DVM_SESSION_GRANT | NETD_DVM_SESSION_REVOKE) {
        return Err(libc::EINVAL);
    }
    let endpoint = rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_NETD);
    if endpoint < 0 {
        let errno = (-endpoint).try_into().unwrap_or(libc::EIO);
        debug_line(&format!(
            "inputd: netd DVM session lookup failed errno={errno}"
        ));
        return Err(errno);
    }
    let pid = syscall0(rustos_user_abi::linux::SYS_GETPID);
    let tid = syscall0(rustos_user_abi::linux::SYS_GETTID);
    if pid <= 0 || tid <= 0 {
        debug_line(&format!(
            "inputd: netd DVM session identity failed pid={pid} tid={tid}"
        ));
        return Err(libc::ESRCH);
    }
    let (request, timeout_ms) = netd_dvm_session_request_with_deadline(
        epoch,
        action,
        pid as u64,
        tid as u64,
        deadline,
        dvm_session_sync::monotonic_nanos(),
    )?;
    let mut response = NetdIpcResponse::default();
    let received = unsafe {
        rustos_svc_runtime::ipc::call_bounded(
            endpoint as u64,
            (&request as *const NetdIpcRequest).cast(),
            NETD_IPC_REQUEST_HEADER_SIZE,
            (&mut response as *mut NetdIpcResponse).cast(),
            size_of::<NetdIpcResponse>(),
            timeout_ms,
        )
    };
    if received < 0 {
        let errno = (-received).try_into().unwrap_or(libc::EIO);
        debug_line(&format!(
            "inputd: netd DVM session call failed errno={errno}"
        ));
        return Err(errno);
    }
    if received as usize != rustos_user_abi::syscall::NETD_IPC_RESPONSE_HEADER_SIZE
        || response.version != NETD_IPC_ABI_VERSION
        || response.op != NETD_IPC_OP_DVM_SESSION
    {
        debug_line(&format!(
            "inputd: netd DVM session response malformed received={received} version={} op={}",
            response.version, response.op
        ));
        return Err(libc::EPROTO);
    }
    if response.status != 0 {
        debug_line(&format!(
            "inputd: netd DVM session rejected status={} epoch={epoch} action={action}",
            response.status
        ));
        return Err(response.status);
    }
    Ok(())
}

fn apply_dvm_ingress_wire(queue: &mut InputQueue, wire: &InputIngressWire) {
    match wire.kind {
        INPUTD_INGRESS_KIND_DVM_LINUX_KEY
            if wire.access == INPUTD_ACCESS_NATIVE
                && (wire.flags == INPUTD_INGRESS_FLAG_DVM_SOURCE
                    || wire.flags
                        == (INPUTD_INGRESS_FLAG_DVM_SOURCE | INPUTD_INGRESS_FLAG_RESET_STATE)) =>
        {
            if wire.flags & INPUTD_INGRESS_FLAG_RESET_STATE != 0 {
                queue.reset_dvm_input();
                return;
            }
            queue.push_dvm_linux_key(wire.keyboard.code, wire.keyboard.action);
        }
        INPUTD_INGRESS_KIND_POINTER_PACKET
            if wire.access == INPUTD_ACCESS_NATIVE
                && wire.flags == INPUTD_INGRESS_FLAG_DVM_SOURCE =>
        {
            queue.push_dvm_pointer_packet(wire.pointer_packet);
        }
        INPUTD_INGRESS_KIND_POINTER_POSITION
            if wire.access == INPUTD_ACCESS_NATIVE
                && wire.flags == INPUTD_INGRESS_FLAG_DVM_SOURCE =>
        {
            queue.push_dvm_pointer_position(wire.pointer_position);
        }
        _ => {}
    }
}

fn fill_native_payload(
    queue: &mut InputQueue,
    payload: &mut [u8; INPUTD_READ_PAYLOAD_CAPACITY],
    requested: usize,
) -> Result<usize, i32> {
    let event_size = size_of::<input_evdev::InputEvent>();
    let capacity = (requested / event_size).min(queue.len());
    if capacity == 0 {
        return Ok(0);
    }
    let mut written = 0usize;
    for _ in 0..capacity {
        let Some(event) = queue.pop_front() else {
            break;
        };
        let bytes = unsafe {
            slice::from_raw_parts(
                (&event as *const input_evdev::InputEvent).cast::<u8>(),
                event_size,
            )
        };
        payload[written..written + event_size].copy_from_slice(bytes);
        written += event_size;
    }
    Ok(written)
}

fn fill_evdev_payload(
    queue: &mut InputQueue,
    payload: &mut [u8; INPUTD_READ_PAYLOAD_CAPACITY],
    requested: usize,
) -> Result<usize, i32> {
    let event_size = size_of::<input_evdev::LinuxInputEvent>();
    let output_capacity = requested / event_size;
    if output_capacity < input_evdev::MAX_EVDEV_EVENTS_PER_INPUT_EVENT || queue.is_empty() {
        return Ok(0);
    }
    let mut input = Vec::new();
    let max_input = (output_capacity / input_evdev::MAX_EVDEV_EVENTS_PER_INPUT_EVENT)
        .min(queue.len())
        .min(input_evdev::MAX_INPUT_EVENTS_PER_READ);
    for _ in 0..max_input {
        if let Some(event) = queue.pop_front() {
            input.push(event);
        }
    }
    let mut output = vec![input_evdev::LinuxInputEvent::default(); output_capacity];
    let written_events = input_evdev::translate_input_events_to_evdev(&input, &mut output)
        .map_err(|_| libc::EINVAL)?;
    let bytes_len = written_events * event_size;
    let bytes = unsafe { slice::from_raw_parts(output.as_ptr().cast::<u8>(), bytes_len) };
    payload[..bytes_len].copy_from_slice(bytes);
    Ok(bytes_len)
}

fn authorize_read(
    request: &InputdIpcRequest,
    response: &mut InputdIpcResponse,
    queue: &mut InputQueue,
) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    let max_len = match request.access {
        INPUTD_ACCESS_NATIVE => INPUTD_MAX_NATIVE_READ_BYTES,
        INPUTD_ACCESS_EVDEV => INPUTD_MAX_EVDEV_READ_BYTES,
        _ => return libc::EINVAL,
    };
    response.approved_len = request.requested_len.min(max_len);
    queue.read_authorizations.push_back(InputReadAuthorization {
        pid: request.pid,
        tid: request.tid,
        fd: request.fd,
        access: request.access,
        approved_len: response.approved_len,
    });
    match fetch_stats(queue) {
        Ok(stats) => response.stats = stats,
        Err(errno) => return errno,
    }
    0
}

fn consume_read_authorization(queue: &mut InputQueue, request: &InputdIpcRequest) -> Option<u64> {
    let index = queue.read_authorizations.iter().position(|auth| {
        auth.pid == request.pid
            && auth.tid == request.tid
            && auth.fd == request.fd
            && auth.access == request.access
    })?;
    queue
        .read_authorizations
        .remove(index)
        .map(|auth| auth.approved_len)
}

fn fetch_stats(queue: &InputQueue) -> Result<InputStatsWire, i32> {
    let mut stats = InputStatsWire::default();
    let args = InputStatsBrokerArgs {
        abi_version: INPUTD_IPC_ABI_VERSION,
        reserved0: 0,
        reserved1: 0,
        out_stats_ptr: (&mut stats as *mut InputStatsWire) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_INPUT_STATS_BROKER,
        (&args as *const InputStatsBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    stats.queued = stats.queued.saturating_add(queue.len() as u64);
    stats.dropped_discrete = stats
        .dropped_discrete
        .saturating_add(queue.dropped_discrete);
    stats.dropped_lossy = stats.dropped_lossy.saturating_add(queue.dropped_lossy);
    stats.readiness_generation = queue.readiness_generation;
    Ok(stats)
}

fn publish_readiness(generation: u64, ready: bool) {
    #[cfg(test)]
    let _ = (generation, ready);
    #[cfg(not(test))]
    {
        let flags = if ready { WAITSET_SIGNAL_FLAG_READY } else { 0 };
        for object_id in [
            WAITSET_INPUT_NATIVE_OBJECT_ID,
            WAITSET_INPUT_EVDEV_OBJECT_ID,
        ] {
            let args = WaitSetSignalBrokerArgs {
                abi_version: WAITSET_ABI_VERSION,
                provider: WAITSET_PROVIDER_INPUTD,
                flags,
                object_id,
                generation,
                reserved0: 0,
            };
            let result = syscall1(
                SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
                (&args as *const WaitSetSignalBrokerArgs) as u64,
            );
            if result < 0 {
                debug_line("inputd: readiness publication failed");
                std::process::exit(134);
            }
        }
    }
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
    queue: &mut InputQueue,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_INPUTD_OP_INPUT_READER => {
            response.value0 = match request.arg0 {
                value if value == u64::from(INPUTD_ACCESS_NATIVE) => {
                    request.arg1.min(INPUTD_MAX_NATIVE_READ_BYTES)
                }
                value if value == u64::from(INPUTD_ACCESS_EVDEV) => {
                    request.arg1.min(INPUTD_MAX_EVDEV_READ_BYTES)
                }
                _ => return Err(libc::EINVAL),
            };
            response.capability = input_capability("reader", request.header.op);
            write_stats_payload(queue, response)
        }
        COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE => {
            response.value0 = input_evdev::MAX_EVDEV_EVENTS_PER_INPUT_EVENT as u64;
            response.value1 = input_evdev::MAX_INPUT_EVENTS_PER_READ as u64;
            response.capability = input_capability("evdev", request.header.op);
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY => {
            response.descriptor_count = 1;
            response.descriptors[0] = input_descriptor("keyboard-layout", request.header.op);
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY => {
            response.value0 = INPUTD_QUEUE_MAX_EVENTS as u64;
            response.value1 = queue.dropped_lossy;
            response.descriptor_count = 1;
            response.descriptors[0] = input_descriptor("lossy-drop-oldest", request.header.op);
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS => write_stats_payload(queue, response),
        COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY => {
            response.value0 = (u64::from(queue.pointer_surface.width) << 32)
                | u64::from(queue.pointer_surface.height);
            response.value1 = queue.pointer_surface.generation;
            response.descriptor_count = 1;
            response.descriptors[0] = input_descriptor("pointer-surface-policy", request.header.op);
            response.capability = input_capability("pointer-surface", request.header.op);
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if !request.has_valid_envelope() || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_INPUTD {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_INPUTD_OP_INPUT_READER
            if matches!(
                request.arg0,
                value if value == u64::from(INPUTD_ACCESS_NATIVE)
                    || value == u64::from(INPUTD_ACCESS_EVDEV)
            ) =>
        {
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE
        | COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS
        | COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn write_stats_payload(
    queue: &InputQueue,
    response: &mut CommercialMaxProtocolResponse,
) -> Result<(), i32> {
    let stats = fetch_stats(queue)?;
    response.value0 = response.value0.max(stats.queued);
    response.value1 = stats.dropped_lossy;
    response.payload_len = write_payload_struct(&stats, &mut response.payload);
    response.descriptor_count = 1;
    response.descriptors[0] = input_descriptor("stats", COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS);
    Ok(())
}

fn validate(received: usize, request: &InputdIpcRequest) -> Result<(), i32> {
    if received != size_of::<InputdIpcRequest>()
        || request.version != INPUTD_IPC_ABI_VERSION
        || request.flags & !INPUTD_READ_FLAG_NONBLOCK != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        INPUTD_IPC_OP_PING => Ok(()),
        INPUTD_IPC_OP_STATS => Ok(()),
        INPUTD_IPC_OP_AUTHORIZE_READ => Ok(()),
        INPUTD_IPC_OP_READ => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn input_descriptor(label: &str, op: u16) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_INPUTD,
        op,
        service_id: IPC_SERVICE_INPUTD,
        capability_mask: input_capability_mask(op),
        value0: INPUTD_QUEUE_MAX_EVENTS as u64,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(
        label.as_bytes(),
        &mut descriptor.name,
        &mut descriptor.name_len,
    );
    descriptor
}

fn input_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: op as u64,
        service_id: IPC_SERVICE_INPUTD,
        capability_mask: input_capability_mask(op),
        rights_mask: input_capability_mask(op),
        generation: INPUTD_QUEUE_MAX_EVENTS as u64,
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(
        label.as_bytes(),
        &mut capability.label,
        &mut capability.label_len,
    );
    capability
}

fn input_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_INPUTD_OP_INPUT_READER => 1 << 1,
        COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE => 1 << 2,
        COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY => 1 << 3,
        COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY => 1 << 4,
        COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS => 1 << 5,
        COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY => 1 << 10,
        _ => 0,
    }
}

fn copy_label(src: &[u8], dest: &mut [u8], len: &mut u16) {
    let count = src.len().min(dest.len());
    dest[..count].copy_from_slice(&src[..count]);
    *len = count as u16;
}

fn write_payload_struct<T>(value: &T, dest: &mut [u8]) -> u32 {
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    let count = bytes.len().min(dest.len());
    dest[..count].copy_from_slice(&bytes[..count]);
    count as u32
}

fn read_unaligned<T: Copy>(buffer: &[u8]) -> T {
    assert!(buffer.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(buffer.as_ptr().cast::<T>()) }
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        line.as_ptr() as u64,
        (len + 1) as u64,
    );
}
