use std::collections::VecDeque;
use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use keyboard_core::{KeyAction, KeyboardDriver, KeyboardEvent};
use rustos_user_abi::syscall::{
    identity_is_exact_sender, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, InputIngestBrokerArgs, InputIngressWire, InputPointerPacketWire,
    InputPointerPositionWire, InputStatsBrokerArgs, InputStatsWire, InputdIpcRequest,
    InputdIpcResponse, InputdPointerSurfaceRequest, InputdReadResponse,
    COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY, COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST, COMMERCIAL_MAX_INPUTD_OP_INPUT_READER,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS, COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_INPUTD, INPUTD_ACCESS_EVDEV, INPUTD_ACCESS_NATIVE,
    INPUTD_INGEST_MAX_EVENTS, INPUTD_INGRESS_FLAG_DVM_SOURCE, INPUTD_INGRESS_FLAG_RESET_STATE,
    INPUTD_INGRESS_KIND_DVM_LINUX_KEY, INPUTD_INGRESS_KIND_POINTER_PACKET,
    INPUTD_INGRESS_KIND_POINTER_POSITION, INPUTD_IPC_ABI_VERSION, INPUTD_IPC_OP_AUTHORIZE_READ,
    INPUTD_IPC_OP_DRAIN_INGEST, INPUTD_IPC_OP_PING, INPUTD_IPC_OP_READ,
    INPUTD_IPC_OP_SET_POINTER_SURFACE, INPUTD_IPC_OP_STATS, INPUTD_READ_FLAG_NONBLOCK,
    INPUTD_READ_PAYLOAD_CAPACITY, IPC_MAX_INLINE_BYTES, IPC_SERVICE_INPUTD, IPC_SERVICE_UISERVER,
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_INPUT_INGEST_BROKER, SYS_RUSTOS_INPUT_STATS_BROKER,
    SYS_RUSTOS_INPUT_WAIT_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REPLY,
};
#[cfg(not(test))]
use rustos_user_abi::syscall::{
    WaitSetSignalBrokerArgs, SYS_RUSTOS_WAITSET_SIGNAL_BROKER, WAITSET_ABI_VERSION,
    WAITSET_GLOBAL_OBJECT_ID, WAITSET_PROVIDER_INPUTD,
};

// The DVM ingestion worker waits on the MSI-X-published ring and transfers
// bounded batches into this user-space queue independently of app reads. App
// requests still own reader authorization and event serialization; no polling
// client is allowed to become the liveness dependency for transport progress.
const INPUTD_QUEUE_MAX_EVENTS: usize = 4096;
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

type SharedInputQueue = Arc<Mutex<InputQueue>>;

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
    queue.lock().unwrap_or_else(|_| {
        debug_line("inputd: input queue synchronization failed");
        std::process::exit(134);
    })
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
            self.advance_readiness_generation(true);
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
            self.advance_readiness_generation(false);
        }
        event
    }

    fn advance_readiness_generation(&mut self, publish: bool) {
        self.readiness_generation = self
            .readiness_generation
            .checked_add(1)
            .expect("inputd readiness generation exhausted");
        if publish {
            publish_readiness_generation(self.readiness_generation);
        }
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
        apply_dvm_ingress_wire, ingest_batch_needs_immediate_retry, validate_commercial_request,
        DvmIngressLogState, InputQueue, INPUTD_INGEST_MAX_EVENTS,
    };
    use rustos_user_abi::syscall::{
        CommercialMaxProtocolRequest, InputIngressWire, InputPointerPacketWire,
        InputPointerPositionWire, InputdIpcRequest, COMMERCIAL_MAX_INPUTD_OP_INPUT_READER,
        COMMERCIAL_MAX_PROTOCOL_INPUTD, INPUTD_ACCESS_NATIVE, INPUTD_INGRESS_FLAG_DVM_SOURCE,
        INPUTD_INGRESS_FLAG_RESET_STATE, INPUTD_INGRESS_KIND_DVM_LINUX_KEY,
    };

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
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_INPUTD, endpoint as u64);
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("inputd: input policy endpoint registered");
    serve(endpoint as u64);
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
            let mut ingest_scratch = vec![InputIngressWire::default(); INPUTD_INGEST_MAX_EVENTS];
            let mut retry_without_wait = false;
            loop {
                if !retry_without_wait {
                    if let Err(errno) = wait_for_dvm_ingress() {
                        debug_line(&format!("inputd: DVM ingestion wait failed errno={errno}"));
                        std::process::exit(134);
                    }
                }
                let drained = {
                    let mut queue = lock_input_queue(&queue);
                    match drain_ingest(&mut queue, &mut ingest_scratch) {
                        Ok(count) => count,
                        Err(errno) => {
                            debug_line(&format!(
                                "inputd: DVM ingestion drain failed errno={errno}"
                            ));
                            std::process::exit(134);
                        }
                    }
                };
                log_dvm_ingress_observations(&queue, &log_state);
                retry_without_wait = ingest_batch_needs_immediate_retry(drained);
                if retry_without_wait {
                    // A hostile or recovering producer cannot retain the
                    // interactive class across unbounded batches. Continue
                    // after the yield without waiting for another MSI-X edge:
                    // a full batch is proof that backlog may remain, and L0
                    // intentionally rings only on empty-to-nonempty.
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

fn serve(endpoint: u64) {
    let queue = Arc::new(Mutex::new(InputQueue::default()));
    let dvm_ingress_log_state = Arc::new(DvmIngressLogState::default());
    start_dvm_ingestion_worker(Arc::clone(&queue), Arc::clone(&dvm_ingress_log_state));
    // This buffer is reused by request handlers. Do not allocate a new 96 KiB
    // wire batch for every client read.
    let mut ingest_scratch = vec![InputIngressWire::default(); INPUTD_INGEST_MAX_EVENTS];
    loop {
        let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
        let mut reply_cap = 0_u64;
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            request.as_mut_ptr() as u64,
            request.len() as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        );
        if received <= 0 {
            continue;
        }
        let request_size = received as usize;
        if request_size == size_of::<CommercialMaxProtocolRequest>() {
            let request = read_unaligned::<CommercialMaxProtocolRequest>(&request);
            let reply = {
                let mut queue = lock_input_queue(&queue);
                reply_commercial_request(
                    reply_cap,
                    &request,
                    sender_pid,
                    sender_tid,
                    &mut queue,
                    &mut ingest_scratch,
                )
            };
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
            }
            log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
            continue;
        }
        if request_size == size_of::<InputdPointerSurfaceRequest>() {
            debug_line("inputd: pointer surface request received");
            let request = read_unaligned::<InputdPointerSurfaceRequest>(&request);
            let mut response = InputdIpcResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdIpcResponse::default()
            };
            response.status = {
                let mut queue = lock_input_queue(&queue);
                if rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_UISERVER, sender_pid)
                    < 0
                {
                    libc::EACCES
                } else {
                    dispatch_pointer_surface_request(&request, &mut queue)
                }
            };
            response.approved_len = (response.status == 0) as u64;
            debug_line("inputd: pointer surface state applied");
            let reply = syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const InputdIpcResponse) as u64,
                size_of::<InputdIpcResponse>() as u64,
            );
            debug_line("inputd: pointer surface reply returned");
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
            }
            log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
            continue;
        }
        if request_size != size_of::<InputdIpcRequest>() {
            continue;
        }
        let request = read_unaligned::<InputdIpcRequest>(&request);
        let reply = if request.op == INPUTD_IPC_OP_READ {
            let mut response = InputdReadResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdReadResponse::default()
            };
            response.status = match validate(received as usize, &request) {
                Ok(())
                    if !identity_is_exact_sender(
                        request.pid,
                        request.tid,
                        sender_pid,
                        sender_tid,
                    ) =>
                {
                    libc::EACCES
                }
                Ok(()) => {
                    let mut queue = lock_input_queue(&queue);
                    dispatch_read(&request, &mut response, &mut queue, &mut ingest_scratch)
                }
                Err(errno) => errno,
            };
            syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const InputdReadResponse) as u64,
                size_of::<InputdReadResponse>() as u64,
            )
        } else {
            let mut response = InputdIpcResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdIpcResponse::default()
            };
            response.status = match validate(received as usize, &request) {
                Ok(())
                    if !identity_is_exact_sender(
                        request.pid,
                        request.tid,
                        sender_pid,
                        sender_tid,
                    ) =>
                {
                    libc::EACCES
                }
                Ok(()) => {
                    let mut queue = lock_input_queue(&queue);
                    dispatch(&request, &mut response, &mut queue, &mut ingest_scratch)
                }
                Err(errno) => errno,
            };
            syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const InputdIpcResponse) as u64,
                size_of::<InputdIpcResponse>() as u64,
            )
        };
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
        }
        log_dvm_ingress_observations(&queue, &dvm_ingress_log_state);
    }
}

fn log_dvm_ingress_observations(queue: &SharedInputQueue, state: &DvmIngressLogState) {
    let (dvm_keyboard, dvm_pointer) = lock_input_queue(queue).take_dvm_ingress_observations();
    let (log_keyboard, log_pointer) = state.claim(dvm_keyboard, dvm_pointer);
    if log_keyboard {
        debug_line("inputd: DVM keyboard ingress observed");
    }
    if log_pointer {
        debug_line("inputd: DVM pointer ingress observed");
    }
}

fn reply_commercial_request(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
    queue: &mut InputQueue,
    ingest_scratch: &mut [InputIngressWire],
) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = if !request.subject_is_exact_sender(sender_pid, sender_tid) {
        libc::EACCES
    } else {
        validate_commercial_request(request)
            .and_then(|_| {
                dispatch_commercial_request(request, &mut response, queue, ingest_scratch)
            })
            .err()
            .unwrap_or(0)
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn dispatch(
    request: &InputdIpcRequest,
    response: &mut InputdIpcResponse,
    queue: &mut InputQueue,
    ingest_scratch: &mut [InputIngressWire],
) -> i32 {
    match request.op {
        INPUTD_IPC_OP_PING => {
            response.approved_len = request.requested_len;
            0
        }
        INPUTD_IPC_OP_STATS => {
            // `poll(2)` obtains readiness through this operation. Refresh the
            // DVM-backed ingress before answering so an input-ring record that woke a
            // kernel poll waiter is visible to the same readiness recheck.
            //
            // Keep this transfer request-driven. An idle background turn must
            // not remove ingress after a reader arms a ring0 poll wait, because
            // it cannot complete that wait itself.
            match drain_ingest(queue, ingest_scratch).and_then(|_| fetch_stats(queue)) {
                Ok(stats) => {
                    response.stats = stats;
                    0
                }
                Err(errno) => errno,
            }
        }
        INPUTD_IPC_OP_AUTHORIZE_READ => authorize_read(request, response, queue),
        INPUTD_IPC_OP_DRAIN_INGEST => match drain_ingest(queue, ingest_scratch) {
            Ok(count) => {
                response.approved_len = count as u64;
                match fetch_stats(queue) {
                    Ok(stats) => response.stats = stats,
                    Err(errno) => return errno,
                }
                0
            }
            Err(errno) => errno,
        },
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
    ingest_scratch: &mut [InputIngressWire],
) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    let Some(approved_len) = consume_read_authorization(queue, request) else {
        return libc::EACCES;
    };
    if let Err(errno) = drain_ingest(queue, ingest_scratch) {
        return errno;
    }
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

fn drain_ingest(queue: &mut InputQueue, events: &mut [InputIngressWire]) -> Result<usize, i32> {
    if events.len() != INPUTD_INGEST_MAX_EVENTS {
        return Err(libc::EINVAL);
    }
    let mut count = 0_u32;
    let args = InputIngestBrokerArgs {
        abi_version: INPUTD_IPC_ABI_VERSION,
        reserved0: 0,
        reserved1: 0,
        out_events_ptr: events.as_mut_ptr() as u64,
        out_capacity: events.len() as u32,
        reserved2: 0,
        out_count_ptr: (&mut count as *mut u32) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_INPUT_INGEST_BROKER,
        (&args as *const InputIngestBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    let count = (count as usize).min(events.len());
    for wire in events.iter().take(count) {
        apply_dvm_ingress_wire(queue, wire);
    }
    Ok(count)
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

fn publish_readiness_generation(generation: u64) {
    #[cfg(test)]
    let _ = generation;
    #[cfg(not(test))]
    {
        let args = WaitSetSignalBrokerArgs {
            abi_version: WAITSET_ABI_VERSION,
            provider: WAITSET_PROVIDER_INPUTD,
            flags: 0,
            object_id: WAITSET_GLOBAL_OBJECT_ID,
            generation,
            reserved0: 0,
        };
        let result = syscall1(
            SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
            (&args as *const WaitSetSignalBrokerArgs) as u64,
        );
        if result < 0 {
            debug_line("inputd: readiness generation publication failed");
            std::process::exit(134);
        }
    }
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
    queue: &mut InputQueue,
    ingest_scratch: &mut [InputIngressWire],
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST => {
            response.value0 = drain_ingest(queue, ingest_scratch)? as u64;
            write_stats_payload(queue, response)
        }
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
        COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST
        | COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE
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
        INPUTD_IPC_OP_DRAIN_INGEST => Ok(()),
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
        COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST => 1 << 0,
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

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3, arg4, arg5) as i64 }
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
