//! In-kernel input event queue.
//!
//! Owns the bounded ring used by hardware callbacks plus the coalescing/drop
//! policy that decides what reaches readers. evdev translation and the key/
//! pointer code maps now live in the shared `input-evdev` crate so the kernel
//! and `inputd` share the same bit-stable tables.

pub use driver_abi::PointerPacket;
use heapless::Deque as HeaplessDeque;
pub use input_evdev::{
    EvdevTranslateError, INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED,
    INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON,
    INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL, InputEvent,
    LinuxInputEvent, LinuxInputTimeval, MAX_EVDEV_EVENTS_PER_INPUT_EVENT,
    MAX_EVDEV_EVENTS_PER_READ, MAX_EVDEV_READ_BYTES, MAX_INPUT_EVENTS_PER_READ,
    MAX_NATIVE_READ_BYTES, POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT,
    POINTER_BUTTON_X1, POINTER_BUTTON_X2, linux_key_code_to_rustos, pointer_button_to_linux,
    rustos_key_code_to_linux, translate_input_events_to_evdev, translate_input_to_evdev,
};
pub use keyboard_core::{KeyAction, KeyCode, KeyboardEvent, Modifiers};

const INPUT_EVENT_QUEUE_CAPACITY: usize = 2048;
const INPUT_EVENT_LOSSY_RESERVE: usize = 64;

const POINTER_BUTTON_MASK_LEFT: u8 = driver_abi::POINTER_BUTTON_LEFT;
const POINTER_BUTTON_MASK_RIGHT: u8 = driver_abi::POINTER_BUTTON_RIGHT;
const POINTER_BUTTON_MASK_MIDDLE: u8 = driver_abi::POINTER_BUTTON_MIDDLE;
const POINTER_BUTTON_MASK_X1: u8 = driver_abi::POINTER_BUTTON_X1;
const POINTER_BUTTON_MASK_X2: u8 = driver_abi::POINTER_BUTTON_X2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerIngressState {
    buttons: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputEventQueueSnapshot {
    pub queued: usize,
    pub pending_coalesced: bool,
    pub pending_pointer_position: bool,
    pub dropped_discrete: u64,
    pub dropped_lossy: u64,
    pub overwritten_pointer_positions: u64,
}

// RING3-MIGRATION-REFERENCE START: inputd should own the bounded input queue,
// coalescing logic, overflow/drop policy, and reader-visible counters. Ring0
// should keep only the IRQ-side ingress primitive (a bounded shared ring) plus
// the user-copy/broker primitives consumers need. evdev translation and key/
// button code maps already moved to the shared `input-evdev` crate.
#[derive(Clone, Copy)]
struct PendingEvent {
    event: InputEvent,
    sequence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Coalesced,
    PointerPosition,
}

pub struct InputEventQueueState {
    queued: HeaplessDeque<InputEvent, INPUT_EVENT_QUEUE_CAPACITY>,
    pending_coalesced: Option<PendingEvent>,
    pending_pointer_position: Option<PendingEvent>,
    next_pending_sequence: u64,
    dropped_discrete_events: u64,
    dropped_lossy_events: u64,
    overwritten_pointer_positions: u64,
}

impl InputEventQueueState {
    pub const fn new() -> Self {
        Self {
            queued: HeaplessDeque::new(),
            pending_coalesced: None,
            pending_pointer_position: None,
            next_pending_sequence: 0,
            dropped_discrete_events: 0,
            dropped_lossy_events: 0,
            overwritten_pointer_positions: 0,
        }
    }

    pub fn can_accept_keyboard_event(&self, event: KeyboardEvent) -> bool {
        match event.action {
            KeyAction::Repeated => {
                queue_remaining_capacity(&self.queued) > INPUT_EVENT_LOSSY_RESERVE
            }
            KeyAction::Pressed | KeyAction::Released => queue_remaining_capacity(&self.queued) != 0,
        }
    }

    pub fn push_keyboard_event(&mut self, event: KeyboardEvent) -> bool {
        let accepted = self.can_accept_keyboard_event(event);
        let action = event.action;
        let input_event = InputEvent {
            kind: INPUT_KIND_KEYBOARD,
            action: map_key_action(event.action),
            code: event.code as u32,
            value0: 0,
            value1: 0,
            modifiers: event.modifiers.bits() as u32,
            text: event.text.unwrap_or(0) as u32,
        };

        if !accepted {
            self.record_keyboard_drop(action);
            return false;
        }

        if matches!(action, KeyAction::Repeated) {
            return self.push_noncritical_discrete_event(input_event);
        }

        self.push_critical_discrete_event(input_event)
    }

    pub fn note_keyboard_drop(&mut self, event: KeyboardEvent) {
        self.record_keyboard_drop(event.action);
    }

    pub fn push_pointer_motion(&mut self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }

        self.push_coalescible_event(InputEvent {
            kind: INPUT_KIND_POINTER_MOTION,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: dx as i32,
            value1: dy as i32,
            modifiers: 0,
            text: 0,
        });
    }

    pub fn push_pointer_position(&mut self, x: u32, y: u32) {
        let event = InputEvent {
            kind: INPUT_KIND_POINTER_POSITION,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: x as i32,
            value1: y as i32,
            modifiers: 0,
            text: 0,
        };

        if let Some(pending) = &mut self.pending_pointer_position {
            if pending.event.value0 != event.value0 || pending.event.value1 != event.value1 {
                pending.event = event;
                self.overwritten_pointer_positions =
                    self.overwritten_pointer_positions.saturating_add(1);
            }
            return;
        }

        self.pending_pointer_position = Some(self.new_pending_event(event));
    }

    pub fn push_pointer_button(&mut self, code: u32, pressed: bool) {
        self.drain_pending_before_critical_discrete();
        self.push_critical_discrete_event(InputEvent {
            kind: INPUT_KIND_POINTER_BUTTON,
            action: if pressed {
                INPUT_ACTION_PRESSED
            } else {
                INPUT_ACTION_RELEASED
            },
            code,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: 0,
        });
    }

    pub fn push_pointer_scroll(&mut self, vertical: i16, horizontal: i16) {
        if vertical == 0 && horizontal == 0 {
            return;
        }

        self.push_coalescible_event(InputEvent {
            kind: INPUT_KIND_POINTER_SCROLL,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: vertical as i32,
            value1: horizontal as i32,
            modifiers: 0,
            text: 0,
        });
    }

    pub fn submit_pointer_packet(&mut self, packet: PointerPacket, previous_buttons: u8) -> bool {
        let mut changed = false;

        if packet.dx != 0 || packet.dy != 0 {
            self.push_pointer_motion(packet.dx, packet.dy);
            changed = true;
        }
        if packet.wheel_vertical != 0 || packet.wheel_horizontal != 0 {
            self.push_pointer_scroll(packet.wheel_vertical, packet.wheel_horizontal);
            changed = true;
        }

        changed |= self.push_pointer_button_edges(previous_buttons, packet.buttons);
        changed
    }

    pub fn submit_pointer_absolute(
        &mut self,
        x: u32,
        y: u32,
        buttons: u8,
        wheel_vertical: i16,
        previous_buttons: u8,
    ) -> bool {
        self.push_pointer_position(x, y);
        let mut changed = true;

        if wheel_vertical != 0 {
            self.push_pointer_scroll(wheel_vertical, 0);
        }

        changed |= self.push_pointer_button_edges(previous_buttons, buttons);
        changed
    }

    pub fn read_input_events(&mut self, dest: &mut [InputEvent]) -> usize {
        let mut count = pop_events_into(&mut self.queued, dest);
        while count < dest.len() {
            let Some(event) = self.take_oldest_pending_event() else {
                break;
            };
            dest[count] = event;
            count += 1;
        }
        count
    }

    pub fn has_pending_events(&self) -> bool {
        !self.queued.is_empty()
            || self.pending_coalesced.is_some()
            || self.pending_pointer_position.is_some()
    }

    pub fn snapshot(&self) -> InputEventQueueSnapshot {
        InputEventQueueSnapshot {
            queued: self.queued.len(),
            pending_coalesced: self.pending_coalesced.is_some(),
            pending_pointer_position: self.pending_pointer_position.is_some(),
            dropped_discrete: self.dropped_discrete_events,
            dropped_lossy: self.dropped_lossy_events,
            overwritten_pointer_positions: self.overwritten_pointer_positions,
        }
    }

    fn push_coalescible_event(&mut self, event: InputEvent) {
        if let Some(pending) = &mut self.pending_coalesced {
            if can_coalesce(pending.event, event) {
                merge_coalesced_event(&mut pending.event, event);
                return;
            }
        }

        self.make_room_for_pending_coalesced_slot();
        if self.pending_coalesced.is_none() {
            self.pending_coalesced = Some(self.new_pending_event(event));
        } else {
            self.record_lossy_drop();
        }
    }

    fn push_noncritical_discrete_event(&mut self, event: InputEvent) -> bool {
        if queue_remaining_capacity(&self.queued) <= INPUT_EVENT_LOSSY_RESERVE {
            self.record_discrete_drop();
            return false;
        }
        self.queued.push_back(event).is_ok()
    }

    fn push_critical_discrete_event(&mut self, event: InputEvent) -> bool {
        if self.queued.push_back(event).is_ok() {
            return true;
        }
        self.record_discrete_drop();
        false
    }

    fn push_pointer_button_edges(&mut self, previous: u8, current: u8) -> bool {
        let mut changed = false;
        changed |= self.emit_button_edge(previous, current, POINTER_BUTTON_MASK_LEFT);
        changed |= self.emit_button_edge(previous, current, POINTER_BUTTON_MASK_RIGHT);
        changed |= self.emit_button_edge(previous, current, POINTER_BUTTON_MASK_MIDDLE);
        changed |= self.emit_button_edge(previous, current, POINTER_BUTTON_MASK_X1);
        changed |= self.emit_button_edge(previous, current, POINTER_BUTTON_MASK_X2);
        changed
    }

    fn emit_button_edge(&mut self, previous: u8, current: u8, button_mask: u8) -> bool {
        let was_pressed = previous & button_mask != 0;
        let is_pressed = current & button_mask != 0;
        if was_pressed == is_pressed {
            return false;
        }

        self.push_pointer_button(button_mask as u32, is_pressed);
        true
    }

    fn make_room_for_pending_coalesced_slot(&mut self) {
        while self.pending_coalesced.is_some() {
            let Some(kind) = self.oldest_pending_kind() else {
                break;
            };
            if self.try_queue_pending_kind(kind, INPUT_EVENT_LOSSY_RESERVE) {
                continue;
            }
            self.drop_pending_kind(kind);
        }
    }

    fn drain_pending_before_critical_discrete(&mut self) {
        while let Some(kind) = self.oldest_pending_kind() {
            if self.try_queue_pending_kind(kind, 1) {
                continue;
            }
            self.drop_pending_kind(kind);
        }
    }

    fn try_queue_pending_kind(
        &mut self,
        kind: PendingKind,
        minimum_remaining_capacity: usize,
    ) -> bool {
        let event = match self.pending_event(kind) {
            Some(event) => event.event,
            None => return true,
        };

        if queue_remaining_capacity(&self.queued) <= minimum_remaining_capacity {
            return false;
        }
        if self.queued.push_back(event).is_err() {
            return false;
        }

        self.clear_pending_kind(kind);
        true
    }

    fn oldest_pending_kind(&self) -> Option<PendingKind> {
        match (self.pending_coalesced, self.pending_pointer_position) {
            (Some(coalesced), Some(position)) => Some(if coalesced.sequence <= position.sequence {
                PendingKind::Coalesced
            } else {
                PendingKind::PointerPosition
            }),
            (Some(_), None) => Some(PendingKind::Coalesced),
            (None, Some(_)) => Some(PendingKind::PointerPosition),
            (None, None) => None,
        }
    }

    fn take_oldest_pending_event(&mut self) -> Option<InputEvent> {
        let kind = self.oldest_pending_kind()?;
        let event = self.pending_event(kind)?.event;
        self.clear_pending_kind(kind);
        Some(event)
    }

    fn pending_event(&self, kind: PendingKind) -> Option<PendingEvent> {
        match kind {
            PendingKind::Coalesced => self.pending_coalesced,
            PendingKind::PointerPosition => self.pending_pointer_position,
        }
    }

    fn clear_pending_kind(&mut self, kind: PendingKind) {
        match kind {
            PendingKind::Coalesced => self.pending_coalesced = None,
            PendingKind::PointerPosition => self.pending_pointer_position = None,
        }
    }

    fn drop_pending_kind(&mut self, kind: PendingKind) {
        self.clear_pending_kind(kind);
        self.record_lossy_drop();
    }

    fn new_pending_event(&mut self, event: InputEvent) -> PendingEvent {
        let pending = PendingEvent {
            event,
            sequence: self.next_pending_sequence,
        };
        self.next_pending_sequence = self.next_pending_sequence.wrapping_add(1);
        pending
    }

    fn record_keyboard_drop(&mut self, action: KeyAction) {
        if matches!(action, KeyAction::Repeated) {
            self.record_lossy_drop();
        } else {
            self.record_discrete_drop();
        }
    }

    fn record_discrete_drop(&mut self) {
        self.dropped_discrete_events = self.dropped_discrete_events.saturating_add(1);
    }

    fn record_lossy_drop(&mut self) {
        self.dropped_lossy_events = self.dropped_lossy_events.saturating_add(1);
    }
}

impl Default for InputEventQueueState {
    fn default() -> Self {
        Self::new()
    }
}

impl PointerIngressState {
    pub const fn new() -> Self {
        Self { buttons: 0 }
    }

    pub fn reset(&mut self) {
        self.buttons = 0;
    }

    pub const fn buttons(&self) -> u8 {
        self.buttons
    }

    pub fn submit_pointer_packet(
        &mut self,
        queue: &mut InputEventQueueState,
        packet: PointerPacket,
    ) -> bool {
        let previous = self.buttons;
        self.buttons = packet.buttons;
        queue.submit_pointer_packet(packet, previous)
    }

    pub fn submit_pointer_absolute(
        &mut self,
        queue: &mut InputEventQueueState,
        x: u32,
        y: u32,
        buttons: u8,
        wheel_vertical: i16,
    ) -> bool {
        let previous = self.buttons;
        self.buttons = buttons;
        queue.submit_pointer_absolute(x, y, buttons, wheel_vertical, previous)
    }
}

fn map_key_action(action: KeyAction) -> u16 {
    match action {
        KeyAction::Pressed => INPUT_ACTION_PRESSED,
        KeyAction::Released => INPUT_ACTION_RELEASED,
        KeyAction::Repeated => INPUT_ACTION_REPEATED,
    }
}

fn can_coalesce(existing: InputEvent, next: InputEvent) -> bool {
    existing.kind == next.kind
        && existing.kind != INPUT_KIND_KEYBOARD
        && existing.kind != INPUT_KIND_POINTER_BUTTON
        && existing.action == next.action
        && existing.code == next.code
        && existing.modifiers == next.modifiers
        && existing.text == next.text
}

fn merge_coalesced_event(existing: &mut InputEvent, next: InputEvent) {
    if existing.kind == INPUT_KIND_POINTER_POSITION {
        existing.value0 = next.value0;
        existing.value1 = next.value1;
        return;
    }

    existing.value0 = existing.value0.saturating_add(next.value0);
    existing.value1 = existing.value1.saturating_add(next.value1);
}

fn queue_remaining_capacity<T, const CAPACITY: usize>(queue: &HeaplessDeque<T, CAPACITY>) -> usize {
    CAPACITY - queue.len()
}

fn pop_events_into(
    queue: &mut HeaplessDeque<InputEvent, INPUT_EVENT_QUEUE_CAPACITY>,
    dest: &mut [InputEvent],
) -> usize {
    let mut count = 0;
    for slot in dest.iter_mut() {
        let Some(value) = queue.pop_front() else {
            break;
        };
        *slot = value;
        count += 1;
    }
    count
}
// RING3-MIGRATION-REFERENCE END: inputd-owned input queue policy.

#[cfg(test)]
mod tests {
    use super::{
        INPUT_ACTION_PRESSED, INPUT_EVENT_LOSSY_RESERVE, INPUT_EVENT_QUEUE_CAPACITY,
        INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION,
        InputEventQueueState, KeyboardEvent, Modifiers, POINTER_BUTTON_LEFT, PointerPacket,
    };
    use keyboard_core::{KeyAction, KeyCode};

    fn key_event() -> KeyboardEvent {
        KeyboardEvent {
            code: KeyCode::A,
            action: KeyAction::Pressed,
            modifiers: Modifiers::empty(),
            text: Some(b'a'),
        }
    }

    #[test]
    fn coalesces_consecutive_pointer_motion() {
        let mut queue = InputEventQueueState::new();
        queue.push_pointer_motion(3, -2);
        queue.push_pointer_motion(4, 6);

        let mut events = [super::InputEvent::default(); 2];
        let read = queue.read_input_events(&mut events);

        assert_eq!(read, 1);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].value0, 7);
        assert_eq!(events[0].value1, 4);
    }

    #[test]
    fn latest_pointer_position_wins() {
        let mut queue = InputEventQueueState::new();
        queue.push_pointer_position(10, 20);
        queue.push_pointer_position(30, 40);

        let mut events = [super::InputEvent::default(); 1];
        let read = queue.read_input_events(&mut events);

        assert_eq!(read, 1);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[0].value0, 30);
        assert_eq!(events[0].value1, 40);
    }

    #[test]
    fn button_flushes_pending_motion() {
        let mut queue = InputEventQueueState::new();
        queue.push_pointer_motion(9, 5);
        queue.push_pointer_button(POINTER_BUTTON_LEFT, true);

        let mut events = [super::InputEvent::default(); 2];
        let read = queue.read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].action, INPUT_ACTION_PRESSED);
        assert_eq!(events[1].code, POINTER_BUTTON_LEFT);
    }

    #[test]
    fn pointer_packets_generate_motion_and_button_edges() {
        let mut queue = InputEventQueueState::new();
        assert!(queue.submit_pointer_packet(
            PointerPacket {
                buttons: driver_abi::POINTER_BUTTON_LEFT,
                dx: 7,
                dy: -3,
                wheel_vertical: 0,
                wheel_horizontal: 0,
                reserved0: 0,
                reserved1: 0,
                reserved2: 0,
            },
            0,
        ));

        let mut events = [super::InputEvent::default(); 4];
        let read = queue.read_input_events(&mut events);
        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].code, POINTER_BUTTON_LEFT);
    }

    #[test]
    fn keyboard_repeat_is_lossy_near_capacity() {
        let mut queue = InputEventQueueState::new();
        for code in 0..(INPUT_EVENT_QUEUE_CAPACITY as u32 - INPUT_EVENT_LOSSY_RESERVE as u32) {
            queue.push_pointer_button(code, true);
        }
        let mut repeated = key_event();
        repeated.action = KeyAction::Repeated;
        assert!(!queue.push_keyboard_event(repeated));
    }
}
