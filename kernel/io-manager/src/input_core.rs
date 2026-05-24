//! In-kernel input event queue.
//!
//! Owns the bounded ingress ring used by hardware callbacks. Reader queue
//! coalescing, evdev translation, and drop policy live in `inputd` and the
//! shared `input-evdev` crate so ring0 stays a thin report source.

pub use driver_abi::PointerPacket;
use heapless::Deque as HeaplessDeque;
pub use input_evdev::{
    linux_key_code_to_rustos, pointer_button_to_linux, rustos_key_code_to_linux,
    translate_input_events_to_evdev, translate_input_to_evdev, EvdevTranslateError, InputEvent,
    LinuxInputEvent, LinuxInputTimeval, INPUT_ACTION_NONE, INPUT_ACTION_PRESSED,
    INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON,
    INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL,
    MAX_EVDEV_EVENTS_PER_INPUT_EVENT, MAX_EVDEV_EVENTS_PER_READ, MAX_EVDEV_READ_BYTES,
    MAX_INPUT_EVENTS_PER_READ, MAX_NATIVE_READ_BYTES, POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE,
    POINTER_BUTTON_RIGHT, POINTER_BUTTON_X1, POINTER_BUTTON_X2,
};

const INPUT_EVENT_QUEUE_CAPACITY: usize = 2048;

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

pub struct InputEventQueueState {
    queued: HeaplessDeque<InputEvent, INPUT_EVENT_QUEUE_CAPACITY>,
    dropped_discrete_events: u64,
    dropped_lossy_events: u64,
}

impl InputEventQueueState {
    pub const fn new() -> Self {
        Self {
            queued: HeaplessDeque::new(),
            dropped_discrete_events: 0,
            dropped_lossy_events: 0,
        }
    }

    pub fn push_pointer_motion(&mut self, dx: i16, dy: i16) {
        if dx == 0 && dy == 0 {
            return;
        }

        self.push_lossy_event(InputEvent {
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
        self.push_lossy_event(InputEvent {
            kind: INPUT_KIND_POINTER_POSITION,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: x as i32,
            value1: y as i32,
            modifiers: 0,
            text: 0,
        });
    }

    pub fn push_pointer_button(&mut self, code: u32, pressed: bool) {
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

        self.push_lossy_event(InputEvent {
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

    pub fn read_input_events(&mut self, dest: &mut [InputEvent]) -> usize {
        pop_events_into(&mut self.queued, dest)
    }

    pub fn has_pending_events(&self) -> bool {
        !self.queued.is_empty()
    }

    pub fn snapshot(&self) -> InputEventQueueSnapshot {
        InputEventQueueSnapshot {
            queued: self.queued.len(),
            pending_coalesced: false,
            pending_pointer_position: false,
            dropped_discrete: self.dropped_discrete_events,
            dropped_lossy: self.dropped_lossy_events,
            overwritten_pointer_positions: 0,
        }
    }

    fn push_lossy_event(&mut self, event: InputEvent) -> bool {
        if self.queued.push_back(event).is_ok() {
            true
        } else {
            self.record_lossy_drop();
            false
        }
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
#[cfg(test)]
mod tests {
    use super::{
        InputEventQueueState, PointerPacket, INPUT_ACTION_PRESSED, INPUT_KIND_POINTER_BUTTON,
        INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, POINTER_BUTTON_LEFT,
    };

    #[test]
    fn queues_consecutive_pointer_motion_as_ingress_reports() {
        let mut queue = InputEventQueueState::new();
        queue.push_pointer_motion(3, -2);
        queue.push_pointer_motion(4, 6);

        let mut events = [super::InputEvent::default(); 2];
        let read = queue.read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].value0, 3);
        assert_eq!(events[0].value1, -2);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[1].value0, 4);
        assert_eq!(events[1].value1, 6);
    }

    #[test]
    fn pointer_positions_remain_raw_ingress_reports() {
        let mut queue = InputEventQueueState::new();
        queue.push_pointer_position(10, 20);
        queue.push_pointer_position(30, 40);

        let mut events = [super::InputEvent::default(); 2];
        let read = queue.read_input_events(&mut events);

        assert_eq!(read, 2);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[0].value0, 10);
        assert_eq!(events[0].value1, 20);
        assert_eq!(events[1].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[1].value0, 30);
        assert_eq!(events[1].value1, 40);
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
}
