use driver_abi::{
    PointerPacket, POINTER_BUTTON_LEFT as POINTER_BUTTON_MASK_LEFT,
    POINTER_BUTTON_MIDDLE as POINTER_BUTTON_MASK_MIDDLE,
    POINTER_BUTTON_RIGHT as POINTER_BUTTON_MASK_RIGHT, POINTER_BUTTON_X1 as POINTER_BUTTON_MASK_X1,
    POINTER_BUTTON_X2 as POINTER_BUTTON_MASK_X2,
};
use spin::{Mutex, MutexGuard};
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use super::keyboard::{KeyAction, KeyboardEvent};
use crate::user::abi::device::{
    InputEvent, INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED,
    INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON,
    INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL,
    POINTER_BUTTON_LEFT,
};
use crate::util::ring::RingBuffer;

const INPUT_EVENT_QUEUE_CAPACITY: usize = 2048;
const INPUT_EVENT_LOSSY_RESERVE: usize = 64;
const INPUT_EVENT_DROP_LOG_INTERVAL: u64 = 256;
const INPUT_POINTER_POSITION_OVERWRITE_LOG_INTERVAL: u64 = 256;

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

pub(crate) struct InputEventQueueState {
    queued: RingBuffer<InputEvent, INPUT_EVENT_QUEUE_CAPACITY>,
    pending_coalesced: Option<PendingEvent>,
    pending_pointer_position: Option<PendingEvent>,
    next_pending_sequence: u64,
    dropped_discrete_events: u64,
    dropped_lossy_events: u64,
    overwritten_pointer_positions: u64,
}

impl InputEventQueueState {
    const fn new() -> Self {
        Self {
            queued: RingBuffer::new(),
            pending_coalesced: None,
            pending_pointer_position: None,
            next_pending_sequence: 0,
            dropped_discrete_events: 0,
            dropped_lossy_events: 0,
            overwritten_pointer_positions: 0,
        }
    }

    pub(crate) fn can_accept_keyboard_event(&self, event: KeyboardEvent) -> bool {
        match event.action {
            KeyAction::Repeated => self.queued.remaining_capacity() > INPUT_EVENT_LOSSY_RESERVE,
            KeyAction::Pressed | KeyAction::Released => self.queued.remaining_capacity() != 0,
        }
    }

    pub(crate) fn push_keyboard_event(&mut self, event: KeyboardEvent) -> bool {
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

    pub(crate) fn note_keyboard_drop(&mut self, event: KeyboardEvent) {
        self.record_keyboard_drop(event.action);
    }

    fn push_pointer_motion(&mut self, dx: i16, dy: i16) {
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

    fn push_pointer_position(&mut self, x: u32, y: u32) {
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
                self.maybe_log_queue_health();
            }
            return;
        }

        self.pending_pointer_position = Some(self.new_pending_event(event));
    }

    fn push_pointer_button(&mut self, code: u32, pressed: bool) {
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

    fn push_pointer_scroll(&mut self, vertical: i16, horizontal: i16) {
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

    fn read_input_events(&mut self, dest: &mut [InputEvent]) -> usize {
        let mut count = self.queued.pop_into(dest);
        while count < dest.len() {
            let Some(event) = self.take_oldest_pending_event() else {
                break;
            };
            dest[count] = event;
            count += 1;
        }
        count
    }

    fn has_pending_events(&self) -> bool {
        !self.queued.is_empty()
            || self.pending_coalesced.is_some()
            || self.pending_pointer_position.is_some()
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
        if self.queued.remaining_capacity() <= INPUT_EVENT_LOSSY_RESERVE {
            self.record_discrete_drop();
            return false;
        }
        debug_assert!(self.queued.push(event));
        true
    }

    fn push_critical_discrete_event(&mut self, event: InputEvent) -> bool {
        if self.queued.push(event) {
            return true;
        }
        self.record_discrete_drop();
        false
    }

    fn submit_pointer_packet(&mut self, packet: PointerPacket, previous_buttons: u8) -> bool {
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

    fn submit_pointer_absolute(
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

        if self.queued.remaining_capacity() <= minimum_remaining_capacity {
            return false;
        }
        if !self.queued.push(event) {
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
        self.maybe_log_queue_health();
    }

    fn record_lossy_drop(&mut self) {
        self.dropped_lossy_events = self.dropped_lossy_events.saturating_add(1);
        self.maybe_log_queue_health();
    }

    fn maybe_log_queue_health(&self) {
        let dropped_total = self
            .dropped_discrete_events
            .saturating_add(self.dropped_lossy_events);
        let should_log_drop =
            dropped_total != 0 && dropped_total.is_multiple_of(INPUT_EVENT_DROP_LOG_INTERVAL);
        let should_log_overwrite = self.overwritten_pointer_positions != 0
            && self
                .overwritten_pointer_positions
                .is_multiple_of(INPUT_POINTER_POSITION_OVERWRITE_LOG_INTERVAL);

        if !should_log_drop && !should_log_overwrite {
            return;
        }

        crate::debug::println!(
            "input queue health: dropped_discrete={} dropped_lossy={} overwritten_pointer_position={} queued={} pending_coalesced={} pending_pointer_position={}",
            self.dropped_discrete_events,
            self.dropped_lossy_events,
            self.overwritten_pointer_positions,
            self.queued.len(),
            self.pending_coalesced.is_some(),
            self.pending_pointer_position.is_some()
        );
    }
}

static INPUT_EVENTS: Mutex<InputEventQueueState> = Mutex::new(InputEventQueueState::new());

pub(crate) fn push_keyboard_event(event: KeyboardEvent) {
    with_event_queue(|events| events.push_keyboard_event(event));
}

pub(crate) fn push_pointer_motion(dx: i16, dy: i16) {
    with_event_queue(|events| events.push_pointer_motion(dx, dy));
}

pub(crate) fn push_pointer_position(x: u32, y: u32) {
    with_event_queue(|events| events.push_pointer_position(x, y));
}

pub(crate) fn push_pointer_button(code: u32, pressed: bool) {
    with_event_queue(|events| events.push_pointer_button(code, pressed));
}

pub(crate) fn push_pointer_scroll(vertical: i16, horizontal: i16) {
    with_event_queue(|events| events.push_pointer_scroll(vertical, horizontal));
}

pub(crate) fn push_pointer_button_left(pressed: bool) {
    push_pointer_button(POINTER_BUTTON_LEFT, pressed);
}

pub(crate) fn submit_pointer_packet(packet: PointerPacket, previous_buttons: u8) -> bool {
    with_event_queue(|events| events.submit_pointer_packet(packet, previous_buttons))
}

pub(crate) fn submit_pointer_absolute(
    x: u32,
    y: u32,
    buttons: u8,
    wheel_vertical: i16,
    previous_buttons: u8,
) -> bool {
    with_event_queue(|events| {
        events.submit_pointer_absolute(x, y, buttons, wheel_vertical, previous_buttons)
    })
}

pub(crate) fn read_input_events(dest: &mut [InputEvent]) -> usize {
    with_event_queue(|events| events.read_input_events(dest))
}

pub(crate) fn has_pending_input_events() -> bool {
    with_event_queue(|events| events.has_pending_events())
}

pub(crate) fn lock_event_queue() -> MutexGuard<'static, InputEventQueueState> {
    INPUT_EVENTS.lock()
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *INPUT_EVENTS.lock() = InputEventQueueState::new();
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

fn with_event_queue<R>(f: impl FnOnce(&mut InputEventQueueState) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut INPUT_EVENTS.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut INPUT_EVENTS.lock()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        has_pending_input_events, push_pointer_button_left, push_pointer_motion,
        push_pointer_position, push_pointer_scroll, read_input_events, reset_for_tests,
        INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_KIND_POINTER_BUTTON,
        INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL,
        POINTER_BUTTON_LEFT,
    };

    fn isolated() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::exclusive_test()
    }

    #[test]
    fn coalesces_consecutive_pointer_motion() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_motion(3, -2);
        push_pointer_motion(4, 6);

        let mut events = [crate::user::abi::device::InputEvent::default(); 2];
        let read = read_input_events(&mut events);

        assert_eq!(read, 1);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].action, INPUT_ACTION_NONE);
        assert_eq!(events[0].value0, 7);
        assert_eq!(events[0].value1, 4);
    }

    #[test]
    fn button_flushes_pending_motion_in_order() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_motion(9, 5);
        push_pointer_button_left(true);

        let mut events = [crate::user::abi::device::InputEvent::default(); 2];
        let read = read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].value0, 9);
        assert_eq!(events[0].value1, 5);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].action, INPUT_ACTION_PRESSED);
        assert_eq!(events[1].code, POINTER_BUTTON_LEFT);
    }

    #[test]
    fn pending_motion_marks_queue_readable() {
        let _guard = isolated();
        reset_for_tests();
        assert!(!has_pending_input_events());
        push_pointer_motion(1, 1);
        assert!(has_pending_input_events());
    }

    #[test]
    fn different_lossy_kinds_preserve_order() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_motion(1, 2);
        push_pointer_scroll(3, 4);

        let mut events = [crate::user::abi::device::InputEvent::default(); 2];
        let read = read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_SCROLL);
        assert_eq!(events[1].value0, 3);
        assert_eq!(events[1].value1, 4);
    }

    #[test]
    fn full_queue_drops_new_discrete_event() {
        let _guard = isolated();
        reset_for_tests();
        for code in 0..super::INPUT_EVENT_QUEUE_CAPACITY as u32 {
            super::push_pointer_button(code, true);
        }
        super::push_pointer_button(u32::MAX, true);

        let mut events =
            [crate::user::abi::device::InputEvent::default(); super::INPUT_EVENT_QUEUE_CAPACITY];
        let read = read_input_events(&mut events);

        assert_eq!(read, super::INPUT_EVENT_QUEUE_CAPACITY);
        assert_eq!(events[0].code, 0);
        assert_eq!(
            events[super::INPUT_EVENT_QUEUE_CAPACITY - 1].code,
            super::INPUT_EVENT_QUEUE_CAPACITY as u32 - 1
        );
    }

    #[test]
    fn critical_button_drops_pending_motion_to_keep_release_slot() {
        let _guard = isolated();
        reset_for_tests();
        for code in 0..(super::INPUT_EVENT_QUEUE_CAPACITY as u32 - 1) {
            super::push_pointer_button(code, true);
        }
        push_pointer_motion(7, 9);
        super::push_pointer_button(u32::MAX, true);

        let mut events =
            [crate::user::abi::device::InputEvent::default(); super::INPUT_EVENT_QUEUE_CAPACITY];
        let read = read_input_events(&mut events);

        assert_eq!(read, super::INPUT_EVENT_QUEUE_CAPACITY);
        assert_eq!(
            events[super::INPUT_EVENT_QUEUE_CAPACITY - 1].kind,
            INPUT_KIND_POINTER_BUTTON
        );
        assert_eq!(events[super::INPUT_EVENT_QUEUE_CAPACITY - 1].code, u32::MAX);
    }

    #[test]
    fn coalesces_pointer_position_to_latest_sample() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_position(100, 120);
        push_pointer_position(240, 260);

        let mut events = [crate::user::abi::device::InputEvent::default(); 2];
        let read = read_input_events(&mut events);

        assert_eq!(read, 1);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[0].value0, 240);
        assert_eq!(events[0].value1, 260);
    }

    #[test]
    fn pointer_button_flushes_pending_absolute_position_in_order() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_position(320, 240);
        push_pointer_button_left(true);

        let mut events = [crate::user::abi::device::InputEvent::default(); 2];
        let read = read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[0].value0, 320);
        assert_eq!(events[0].value1, 240);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_BUTTON);
        assert_eq!(events[1].action, INPUT_ACTION_PRESSED);
    }

    #[test]
    fn pointer_position_preserves_order_against_later_scroll() {
        let _guard = isolated();
        reset_for_tests();
        push_pointer_position(100, 200);
        push_pointer_scroll(3, 0);

        let mut events = [crate::user::abi::device::InputEvent::default(); 2];
        let read = read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_SCROLL);
    }
}
