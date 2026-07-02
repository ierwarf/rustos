use std::collections::VecDeque;
use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use keyboard_core::{KeyAction, KeyboardDriver, ScanCodeSet};
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, InputHidKeyboardReportWire,
    InputHidPointerReportWire, InputHidPolicyWire, InputIngestBrokerArgs, InputIngressWire,
    InputKeyboardEventWire, InputPointerAbsoluteWire, InputPointerPacketWire, InputStatsBrokerArgs,
    InputStatsWire, InputdIpcRequest, InputdIpcResponse, InputdReadResponse,
    COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY, COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE,
    COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY, COMMERCIAL_MAX_INPUTD_OP_I8042_COMMAND_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST, COMMERCIAL_MAX_INPUTD_OP_INPUT_READER,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS, COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_PS2_PACKET_POLICY, COMMERCIAL_MAX_INPUTD_OP_SERIO_BUS_POLICY,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_INPUTD, INPUTD_ACCESS_EVDEV,
    INPUTD_ACCESS_NATIVE, INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY, INPUTD_HID_POLICY_KIND_KEYBOARD,
    INPUTD_HID_POLICY_KIND_POINTER, INPUTD_HID_POLICY_KIND_UNKNOWN,
    INPUTD_HID_POLICY_REPORT_CAPACITY, INPUTD_INGEST_MAX_EVENTS, INPUTD_INGRESS_FLAG_RESET_STATE,
    INPUTD_INGRESS_KIND_EVENT, INPUTD_INGRESS_KIND_HID_KEYBOARD_REPORT,
    INPUTD_INGRESS_KIND_HID_POINTER_REPORT, INPUTD_INGRESS_KIND_HID_RAW_REPORT,
    INPUTD_INGRESS_KIND_KEYBOARD, INPUTD_INGRESS_KIND_POINTER_ABSOLUTE,
    INPUTD_INGRESS_KIND_POINTER_PACKET, INPUTD_INGRESS_KIND_PS2_MOUSE_BYTE,
    INPUTD_INGRESS_KIND_PS2_SCANCODE, INPUTD_IPC_ABI_VERSION, INPUTD_IPC_OP_AUTHORIZE_READ,
    INPUTD_IPC_OP_DRAIN_INGEST, INPUTD_IPC_OP_PING, INPUTD_IPC_OP_READ, INPUTD_IPC_OP_STATS,
    INPUTD_READ_FLAG_NONBLOCK, INPUTD_READ_PAYLOAD_CAPACITY, IPC_MAX_INLINE_BYTES,
    IPC_SERVICE_INPUTD, SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_INPUT_INGEST_BROKER,
    SYS_RUSTOS_INPUT_STATS_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const INPUTD_QUEUE_MAX_EVENTS: usize = 4096;
const INPUTD_MAX_NATIVE_READ_BYTES: u64 = input_evdev::MAX_NATIVE_READ_BYTES as u64;
const INPUTD_MAX_EVDEV_READ_BYTES: u64 = input_evdev::MAX_EVDEV_READ_BYTES as u64;
const DEFAULT_POINTER_WIDTH: i32 = 1600;
const DEFAULT_POINTER_HEIGHT: i32 = 900;
const PS2_MOUSE_STATUS_LEFT: u8 = 1 << 0;
const PS2_MOUSE_STATUS_RIGHT: u8 = 1 << 1;
const PS2_MOUSE_STATUS_MIDDLE: u8 = 1 << 2;
const PS2_MOUSE_STATUS_ALWAYS_ONE: u8 = 1 << 3;
const PS2_MOUSE_STATUS_X_OVERFLOW: u8 = 1 << 6;
const PS2_MOUSE_STATUS_Y_OVERFLOW: u8 = 1 << 7;
static INPUT_DELIVERY_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct HidKeyboardState {
    source_id: u64,
    modifiers: u8,
    keys: [u8; 16],
    key_count: usize,
}

#[derive(Default)]
struct InputQueue {
    events: VecDeque<input_evdev::InputEvent>,
    hid_keyboards: Vec<HidKeyboardState>,
    ps2_keyboard: KeyboardDriver,
    ps2_mouse: Ps2MousePacketState,
    dropped_lossy: u64,
    pointer_buttons: u8,
}

#[derive(Default)]
struct Ps2MousePacketState {
    bytes: [u8; 3],
    len: usize,
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
        if self.events.len() >= INPUTD_QUEUE_MAX_EVENTS {
            let _ = self.events.pop_front();
            self.dropped_lossy = self.dropped_lossy.saturating_add(1);
        }
        self.events.push_back(event);
    }

    fn pop_front(&mut self) -> Option<input_evdev::InputEvent> {
        self.events.pop_front()
    }

    fn push_keyboard_event(&mut self, event: InputKeyboardEventWire) {
        self.push(input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_KEYBOARD,
            action: event.action,
            code: event.code,
            value0: 0,
            value1: 0,
            modifiers: event.modifiers,
            text: event.text,
        });
    }

    fn push_ps2_scancode(&mut self, scancode: u8, translated: bool) {
        let scan_set = if translated {
            ScanCodeSet::Set1
        } else {
            ScanCodeSet::Set2
        };
        self.ps2_keyboard.set_scan_code_set(scan_set);
        self.ps2_keyboard.feed_scancode(scancode);
        while let Some(event) = self.ps2_keyboard.pop_event() {
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
    }

    fn push_ps2_mouse_byte(&mut self, byte: u8) {
        let packet = match self.ps2_mouse.len {
            0 => {
                if byte & PS2_MOUSE_STATUS_ALWAYS_ONE == 0 {
                    return;
                }
                self.ps2_mouse.bytes[0] = byte;
                self.ps2_mouse.len = 1;
                return;
            }
            1 => {
                self.ps2_mouse.bytes[1] = byte;
                self.ps2_mouse.len = 2;
                return;
            }
            _ => {
                self.ps2_mouse.bytes[2] = byte;
                self.ps2_mouse.len = 0;
                self.ps2_mouse.bytes
            }
        };

        if packet[0] & (PS2_MOUSE_STATUS_X_OVERFLOW | PS2_MOUSE_STATUS_Y_OVERFLOW) != 0 {
            return;
        }

        let mut buttons = 0u8;
        if packet[0] & PS2_MOUSE_STATUS_LEFT != 0 {
            buttons |= input_evdev::POINTER_BUTTON_LEFT as u8;
        }
        if packet[0] & PS2_MOUSE_STATUS_RIGHT != 0 {
            buttons |= input_evdev::POINTER_BUTTON_RIGHT as u8;
        }
        if packet[0] & PS2_MOUSE_STATUS_MIDDLE != 0 {
            buttons |= input_evdev::POINTER_BUTTON_MIDDLE as u8;
        }

        self.push_pointer_packet(InputPointerPacketWire {
            buttons,
            reserved0: [0; 3],
            dx: i16::from(packet[1] as i8),
            dy: -i16::from(packet[2] as i8),
            wheel_vertical: 0,
            wheel_horizontal: 0,
        });
    }

    fn reset_ps2_mouse_packet(&mut self) {
        self.ps2_mouse = Ps2MousePacketState::default();
    }

    fn push_pointer_packet(&mut self, packet: InputPointerPacketWire) {
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
        self.push_pointer_button_edges(packet.buttons);
    }

    fn push_pointer_absolute(&mut self, absolute: InputPointerAbsoluteWire) {
        self.push(input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_POINTER_POSITION,
            action: input_evdev::INPUT_ACTION_NONE,
            code: 0,
            value0: absolute.x as i32,
            value1: absolute.y as i32,
            modifiers: 0,
            text: 0,
        });
        if absolute.wheel_vertical != 0 {
            self.push(input_evdev::InputEvent {
                kind: input_evdev::INPUT_KIND_POINTER_SCROLL,
                action: input_evdev::INPUT_ACTION_NONE,
                code: 0,
                value0: absolute.wheel_vertical as i32,
                value1: 0,
                modifiers: 0,
                text: 0,
            });
        }
        self.push_pointer_button_edges(absolute.buttons);
    }

    fn push_hid_keyboard_report(&mut self, report: InputHidKeyboardReportWire) {
        let mut keys = [0u8; 16];
        let key_count = (report.key_count as usize).min(keys.len());
        keys[..key_count].copy_from_slice(&report.keys[..key_count]);
        let state_index = if let Some(index) = self
            .hid_keyboards
            .iter()
            .position(|state| state.source_id == report.source_id)
        {
            index
        } else {
            self.hid_keyboards.push(HidKeyboardState {
                source_id: report.source_id,
                ..HidKeyboardState::default()
            });
            self.hid_keyboards.len() - 1
        };
        let previous_modifiers = self.hid_keyboards[state_index].modifiers;
        let previous_keys = self.hid_keyboards[state_index].keys;
        let previous_key_count = self.hid_keyboards[state_index].key_count;
        self.hid_keyboards[state_index].modifiers = report.modifiers;
        self.hid_keyboards[state_index].keys = keys;
        self.hid_keyboards[state_index].key_count = key_count;

        for usage in 0xE0u8..=0xE7 {
            let mask = 1u8 << (usage - 0xE0);
            if (previous_modifiers & mask) == (report.modifiers & mask) {
                continue;
            }
            if let Some(code) = input_evdev::hid_usage_to_keycode(usage) {
                self.push_keycode_event(code as u32, (report.modifiers & mask) != 0);
            }
        }

        let mut previous_index = 0usize;
        while previous_index < previous_key_count {
            let usage = previous_keys[previous_index];
            if !keys[..key_count].contains(&usage) {
                if let Some(code) = input_evdev::hid_usage_to_keycode(usage) {
                    self.push_keycode_event(code as u32, false);
                }
            }
            previous_index += 1;
        }

        let mut current_index = 0usize;
        while current_index < key_count {
            let usage = keys[current_index];
            if !previous_keys[..previous_key_count].contains(&usage) {
                if let Some(code) = input_evdev::hid_usage_to_keycode(usage) {
                    self.push_keycode_event(code as u32, true);
                }
            }
            current_index += 1;
        }
    }

    fn push_hid_pointer_report(&mut self, report: InputHidPointerReportWire) {
        if report.relative != 0 {
            self.push_pointer_packet(InputPointerPacketWire {
                buttons: report.buttons,
                reserved0: [0; 3],
                dx: report.x as i16,
                dy: report.y as i16,
                wheel_vertical: report.wheel_vertical,
                wheel_horizontal: 0,
            });
        } else {
            self.push_pointer_absolute(InputPointerAbsoluteWire {
                buttons: report.buttons,
                reserved0: [0; 3],
                x: report.x.max(0) as u32,
                y: report.y.max(0) as u32,
                wheel_vertical: report.wheel_vertical,
                reserved1: 0,
            });
        }
    }

    fn push_hid_raw_report(&mut self, raw: InputHidPolicyWire) {
        let report_len = (raw.report_len as usize).min(raw.report.len());
        let descriptor_len = (raw.descriptor_len as usize).min(raw.descriptor_prefix.len());
        if report_len == 0 {
            return;
        }
        let report = &raw.report[..report_len];
        let descriptor = &raw.descriptor_prefix[..descriptor_len];
        match raw.kind {
            INPUTD_HID_POLICY_KIND_KEYBOARD => {
                if let Some(keyboard) =
                    decode_hid_keyboard_report(raw.source_id, descriptor, report)
                {
                    self.push_hid_keyboard_report(keyboard);
                }
            }
            INPUTD_HID_POLICY_KIND_POINTER => {
                if let Some(pointer) = decode_hid_pointer_report(raw.source_id, descriptor, report)
                {
                    self.push_hid_pointer_report(pointer);
                }
            }
            _ => {
                if let Some(pointer) = decode_hid_pointer_report(raw.source_id, descriptor, report)
                {
                    self.push_hid_pointer_report(pointer);
                } else if let Some(keyboard) =
                    decode_hid_keyboard_report(raw.source_id, descriptor, report)
                {
                    self.push_hid_keyboard_report(keyboard);
                }
            }
        }
    }

    fn push_keycode_event(&mut self, code: u32, pressed: bool) {
        self.push(input_evdev::InputEvent {
            kind: input_evdev::INPUT_KIND_KEYBOARD,
            action: if pressed {
                input_evdev::INPUT_ACTION_PRESSED
            } else {
                input_evdev::INPUT_ACTION_RELEASED
            },
            code,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: 0,
        });
    }

    fn push_pointer_button_edges(&mut self, current: u8) {
        let previous = self.pointer_buttons;
        self.pointer_buttons = current;
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

fn decode_hid_keyboard_report(
    source_id: u64,
    descriptor: &[u8],
    report: &[u8],
) -> Option<InputHidKeyboardReportWire> {
    if !report_descriptor_has_keyboard(descriptor) {
        return None;
    }
    let report = strip_report_id(descriptor, report)?;
    if report.len() < 8 {
        return None;
    }
    let mut keys = [0_u8; 16];
    let mut key_count = 0usize;
    for usage in report[2..].iter().copied().filter(|usage| *usage != 0) {
        if key_count >= keys.len() {
            break;
        }
        keys[key_count] = usage;
        key_count += 1;
    }
    Some(InputHidKeyboardReportWire {
        source_id,
        modifiers: report[0],
        key_count: key_count as u8,
        reserved0: [0; 6],
        keys,
    })
}

fn decode_hid_pointer_report(
    source_id: u64,
    descriptor: &[u8],
    report: &[u8],
) -> Option<InputHidPointerReportWire> {
    if !report_descriptor_has_pointer(descriptor) {
        return None;
    }
    let report = strip_report_id(descriptor, report)?;
    let buttons = report.first().copied().unwrap_or(0) & 0x1f;

    if pointer_descriptor_is_relative(descriptor) {
        if report.len() < 3 {
            return None;
        }
        return Some(InputHidPointerReportWire {
            source_id,
            buttons,
            relative: 1,
            reserved0: [0; 2],
            x: i32::from(report[1] as i8),
            y: i32::from(report[2] as i8),
            wheel_vertical: report
                .get(3)
                .copied()
                .map(|value| value as i8 as i16)
                .unwrap_or(0),
            reserved1: 0,
        });
    }

    if report.len() < 5 {
        return None;
    }
    let raw_x = u16::from_le_bytes([report[1], report[2]]) as i32;
    let raw_y = u16::from_le_bytes([report[3], report[4]]) as i32;
    let logical_max = pointer_descriptor_logical_max(descriptor).max(1);
    Some(InputHidPointerReportWire {
        source_id,
        buttons,
        relative: 0,
        reserved0: [0; 2],
        x: scale_pointer_coordinate(raw_x, logical_max, DEFAULT_POINTER_WIDTH - 1),
        y: scale_pointer_coordinate(raw_y, logical_max, DEFAULT_POINTER_HEIGHT - 1),
        wheel_vertical: report
            .get(5)
            .copied()
            .map(|value| value as i8 as i16)
            .unwrap_or(0),
        reserved1: 0,
    })
}

fn report_descriptor_has_keyboard(descriptor: &[u8]) -> bool {
    let has_keyboard_usage = descriptor.windows(2).any(|item| item == [0x09, 0x06]);
    let has_key_usage_page = descriptor.windows(2).any(|item| item == [0x05, 0x07]);
    has_keyboard_usage && has_key_usage_page
}

fn report_descriptor_has_pointer(descriptor: &[u8]) -> bool {
    let has_mouse_usage = descriptor.windows(2).any(|item| item == [0x09, 0x02]);
    let has_x_usage = descriptor.windows(2).any(|item| item == [0x09, 0x30]);
    let has_y_usage = descriptor.windows(2).any(|item| item == [0x09, 0x31]);
    has_mouse_usage && has_x_usage && has_y_usage
}

fn strip_report_id<'a>(descriptor: &[u8], report: &'a [u8]) -> Option<&'a [u8]> {
    let Some(report_id) = descriptor
        .windows(2)
        .find_map(|item| (item[0] == 0x85).then_some(item[1]))
    else {
        return Some(report);
    };
    (report.first().copied() == Some(report_id)).then_some(&report[1..])
}

fn pointer_descriptor_is_relative(descriptor: &[u8]) -> bool {
    descriptor.windows(2).any(|item| item == [0x81, 0x06])
        && !descriptor.windows(3).any(|item| item == [0x26, 0xff, 0x7f])
}

fn pointer_descriptor_logical_max(descriptor: &[u8]) -> i32 {
    let mut max_value = 0_i32;
    for item in descriptor.windows(3) {
        if item[0] == 0x26 {
            max_value = max_value.max(u16::from_le_bytes([item[1], item[2]]) as i32);
        }
    }
    if max_value > 1 {
        max_value
    } else {
        0x7fff
    }
}

fn scale_pointer_coordinate(value: i32, logical_max: i32, target_max: i32) -> i32 {
    if target_max <= 0 || logical_max <= 0 {
        return 0;
    }
    let clamped = value.clamp(0, logical_max);
    ((clamped as i64 * target_max as i64) / logical_max as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::InputQueue;
    use rustos_user_abi::syscall::{
        InputHidPolicyWire, InputKeyboardEventWire, InputPointerPacketWire,
        INPUTD_HID_POLICY_KIND_KEYBOARD, INPUTD_HID_POLICY_KIND_POINTER,
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

    fn key_event() -> InputKeyboardEventWire {
        InputKeyboardEventWire {
            action: input_evdev::INPUT_ACTION_PRESSED,
            reserved0: 0,
            code: 30,
            modifiers: 0,
            text: b'a' as u32,
        }
    }

    fn hid_raw(kind: u16, report: &[u8], descriptor: &[u8]) -> InputHidPolicyWire {
        let mut raw = InputHidPolicyWire {
            source_id: 7,
            kind,
            report_len: report.len() as u16,
            descriptor_len: descriptor.len() as u16,
            ..InputHidPolicyWire::default()
        };
        raw.report[..report.len()].copy_from_slice(report);
        raw.descriptor_prefix[..descriptor.len()].copy_from_slice(descriptor);
        raw
    }

    #[test]
    fn inputd_queue_coalesces_pointer_motion() {
        let mut queue = InputQueue::default();
        queue.push(pointer_motion(3, -2));
        queue.push(pointer_motion(4, 6));

        assert_eq!(queue.len(), 1);
        let event = queue.pop_front().unwrap();
        assert_eq!(event.value0, 7);
        assert_eq!(event.value1, 4);
    }

    #[test]
    fn inputd_queue_keeps_latest_pointer_position() {
        let mut queue = InputQueue::default();
        queue.push(pointer_position(10, 20));
        queue.push(pointer_position(30, 40));

        assert_eq!(queue.len(), 1);
        let event = queue.pop_front().unwrap();
        assert_eq!(event.value0, 30);
        assert_eq!(event.value1, 40);
    }

    #[test]
    fn inputd_queue_owns_pointer_button_edges_from_raw_reports() {
        let mut queue = InputQueue::default();
        queue.push_pointer_packet(pointer_packet(1, 7, -3));

        assert_eq!(queue.len(), 2);
        let motion = queue.pop_front().unwrap();
        assert_eq!(motion.kind, input_evdev::INPUT_KIND_POINTER_MOTION);
        assert_eq!(motion.value0, 7);
        assert_eq!(motion.value1, -3);
        let button = queue.pop_front().unwrap();
        assert_eq!(button.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(button.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(button.code, input_evdev::POINTER_BUTTON_LEFT);

        queue.push_pointer_packet(pointer_packet(0, 0, 0));
        let release = queue.pop_front().unwrap();
        assert_eq!(release.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(release.action, input_evdev::INPUT_ACTION_RELEASED);
        assert_eq!(release.code, input_evdev::POINTER_BUTTON_LEFT);
    }

    #[test]
    fn inputd_decodes_raw_ps2_mouse_bytes() {
        let mut queue = InputQueue::default();
        queue.push_ps2_mouse_byte(0x09);
        queue.push_ps2_mouse_byte(7);
        queue.push_ps2_mouse_byte(3_u8.wrapping_neg());

        assert_eq!(queue.len(), 2);
        let motion = queue.pop_front().unwrap();
        assert_eq!(motion.kind, input_evdev::INPUT_KIND_POINTER_MOTION);
        assert_eq!(motion.value0, 7);
        assert_eq!(motion.value1, 3);
        let button = queue.pop_front().unwrap();
        assert_eq!(button.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(button.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(button.code, input_evdev::POINTER_BUTTON_LEFT);
    }

    #[test]
    fn inputd_resets_raw_ps2_mouse_packet_state() {
        let mut queue = InputQueue::default();
        queue.push_ps2_mouse_byte(0x09);
        queue.reset_ps2_mouse_packet();
        queue.push_ps2_mouse_byte(0x08);
        queue.push_ps2_mouse_byte(4);
        queue.push_ps2_mouse_byte(0);

        assert_eq!(queue.len(), 1);
        let motion = queue.pop_front().unwrap();
        assert_eq!(motion.kind, input_evdev::INPUT_KIND_POINTER_MOTION);
        assert_eq!(motion.value0, 4);
        assert_eq!(motion.value1, 0);
    }

    #[test]
    fn inputd_queue_owns_keyboard_reader_events_from_ingress() {
        let mut queue = InputQueue::default();
        queue.push_keyboard_event(key_event());

        let event = queue.pop_front().unwrap();
        assert_eq!(event.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(event.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(event.code, 30);
        assert_eq!(event.text, b'a' as u32);
    }

    #[test]
    fn inputd_decodes_raw_hid_keyboard_report() {
        let mut queue = InputQueue::default();
        let descriptor = [0x05, 0x01, 0x09, 0x06, 0x05, 0x07];
        let report = [0, 0, 0x04, 0, 0, 0, 0, 0];
        queue.push_hid_raw_report(hid_raw(
            INPUTD_HID_POLICY_KIND_KEYBOARD,
            &report,
            &descriptor,
        ));

        let event = queue.pop_front().unwrap();
        assert_eq!(event.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(event.action, input_evdev::INPUT_ACTION_PRESSED);
    }

    #[test]
    fn inputd_decodes_raw_hid_pointer_report() {
        let mut queue = InputQueue::default();
        let descriptor = [0x05, 0x01, 0x09, 0x02, 0x09, 0x30, 0x09, 0x31, 0x81, 0x06];
        let report = [1, 7_u8, 0xfd_u8];
        queue.push_hid_raw_report(hid_raw(
            INPUTD_HID_POLICY_KIND_POINTER,
            &report,
            &descriptor,
        ));

        let motion = queue.pop_front().unwrap();
        assert_eq!(motion.kind, input_evdev::INPUT_KIND_POINTER_MOTION);
        assert_eq!(motion.value0, 7);
        assert_eq!(motion.value1, -3);
        let button = queue.pop_front().unwrap();
        assert_eq!(button.kind, input_evdev::INPUT_KIND_POINTER_BUTTON);
        assert_eq!(button.action, input_evdev::INPUT_ACTION_PRESSED);
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
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_INPUTD,
        endpoint as u64,
    );
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

fn serve(endpoint: u64) {
    let mut queue = InputQueue::default();
    loop {
        let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            request.as_mut_ptr() as u64,
            request.len() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        let request_size = received as usize;
        if request_size == size_of::<CommercialMaxProtocolRequest>() {
            let request = read_unaligned::<CommercialMaxProtocolRequest>(&request);
            let reply = reply_commercial_request(reply_cap, &request, &mut queue);
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
            }
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
                Ok(()) => dispatch_read(&request, &mut response, &mut queue),
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
                Ok(()) => dispatch(&request, &mut response, &mut queue),
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
    }
}

fn reply_commercial_request(
    reply_cap: u64,
    request: &CommercialMaxProtocolRequest,
    queue: &mut InputQueue,
) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = validate_commercial_request(request)
        .and_then(|_| dispatch_commercial_request(request, &mut response, queue))
        .err()
        .unwrap_or(0);
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
        INPUTD_IPC_OP_DRAIN_INGEST => match drain_ingest(queue) {
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

fn dispatch_read(
    request: &InputdIpcRequest,
    response: &mut InputdReadResponse,
    queue: &mut InputQueue,
) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    if let Err(errno) = drain_ingest(queue) {
        return errno;
    }
    let requested = request
        .requested_len
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

fn drain_ingest(queue: &mut InputQueue) -> Result<usize, i32> {
    let mut events = vec![InputIngressWire::default(); INPUTD_INGEST_MAX_EVENTS];
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
        match wire.kind {
            INPUTD_INGRESS_KIND_EVENT if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push(wire.event);
            }
            INPUTD_INGRESS_KIND_KEYBOARD if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_keyboard_event(wire.keyboard);
            }
            INPUTD_INGRESS_KIND_PS2_SCANCODE if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_ps2_scancode(
                    wire.ps2_scancode.scancode,
                    wire.ps2_scancode.translated != 0,
                );
            }
            INPUTD_INGRESS_KIND_PS2_MOUSE_BYTE if wire.access == INPUTD_ACCESS_NATIVE => {
                if wire.flags & INPUTD_INGRESS_FLAG_RESET_STATE != 0 {
                    queue.reset_ps2_mouse_packet();
                    continue;
                }
                queue.push_ps2_mouse_byte(wire.ps2_mouse_byte.byte);
            }
            INPUTD_INGRESS_KIND_POINTER_PACKET if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_pointer_packet(wire.pointer_packet);
            }
            INPUTD_INGRESS_KIND_POINTER_ABSOLUTE if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_pointer_absolute(wire.pointer_absolute);
            }
            INPUTD_INGRESS_KIND_HID_KEYBOARD_REPORT if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_hid_keyboard_report(wire.hid_keyboard);
            }
            INPUTD_INGRESS_KIND_HID_POINTER_REPORT if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_hid_pointer_report(wire.hid_pointer);
            }
            INPUTD_INGRESS_KIND_HID_RAW_REPORT if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_hid_raw_report(wire.hid_raw);
            }
            _ => {}
        }
    }
    if count > 0 && !INPUT_DELIVERY_LOGGED.swap(true, Ordering::AcqRel) {
        debug_line(&format!(
            "input: pointer event delivered kind=inputd-drain count={count}"
        ));
    }
    Ok(count)
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
    queue: &InputQueue,
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
    match fetch_stats(queue) {
        Ok(stats) => response.stats = stats,
        Err(errno) => return errno,
    }
    0
}

fn fetch_stats(queue: &InputQueue) -> Result<InputStatsWire, i32> {
    let mut stats = InputStatsWire::default();
    let args = InputStatsBrokerArgs {
        abi_version: 1,
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
    stats.dropped_lossy = stats.dropped_lossy.saturating_add(queue.dropped_lossy);
    Ok(stats)
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
    queue: &mut InputQueue,
) -> Result<(), i32> {
    match request.header.op {
        COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST => {
            response.value0 = drain_ingest(queue)? as u64;
            write_stats_payload(queue, response)
        }
        COMMERCIAL_MAX_INPUTD_OP_INPUT_READER => {
            let access = request.arg0 as u16;
            response.value0 = match access {
                INPUTD_ACCESS_NATIVE => request.arg1.min(INPUTD_MAX_NATIVE_READ_BYTES),
                INPUTD_ACCESS_EVDEV => request.arg1.min(INPUTD_MAX_EVDEV_READ_BYTES),
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
        COMMERCIAL_MAX_INPUTD_OP_SERIO_BUS_POLICY => {
            response.descriptor_count = 1;
            response.descriptors[0] =
                input_descriptor("serio-bus-service-driver", request.header.op);
            response.capability = input_capability("serio-bus", request.header.op);
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_I8042_COMMAND_POLICY => {
            response.descriptor_count = 1;
            response.descriptors[0] =
                input_descriptor("i8042-command-service-driver", request.header.op);
            response.capability = input_capability("i8042-command", request.header.op);
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_PS2_PACKET_POLICY => {
            response.descriptor_count = 1;
            response.descriptors[0] = input_descriptor("ps2-packet-policy", request.header.op);
            response.capability = input_capability("ps2-packet", request.header.op);
            Ok(())
        }
        COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY => {
            fill_hid_report_policy(request, response);
            Ok(())
        }
        _ => Err(libc::EINVAL),
    }
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_INPUTD
        || request.header.flags != 0
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST
        | COMMERCIAL_MAX_INPUTD_OP_INPUT_READER
        | COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE
        | COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS
        | COMMERCIAL_MAX_INPUTD_OP_SERIO_BUS_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_I8042_COMMAND_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_PS2_PACKET_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn fill_hid_report_policy(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) {
    let payload_len = (request.payload_len as usize).min(request.payload.len());
    let requested_report_len = (request.arg2 as usize).min(INPUTD_HID_POLICY_REPORT_CAPACITY);
    let report_len = requested_report_len.min(payload_len);
    let descriptor_available = payload_len.saturating_sub(report_len);
    let descriptor_len = (request.arg3 as usize)
        .min(INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY)
        .min(descriptor_available);
    let kind = match request.arg1 as u16 {
        INPUTD_HID_POLICY_KIND_KEYBOARD => INPUTD_HID_POLICY_KIND_KEYBOARD,
        INPUTD_HID_POLICY_KIND_POINTER => INPUTD_HID_POLICY_KIND_POINTER,
        _ => INPUTD_HID_POLICY_KIND_UNKNOWN,
    };
    let mut policy = InputHidPolicyWire {
        source_id: request.arg0,
        kind,
        report_len: report_len as u16,
        descriptor_len: descriptor_len as u16,
        required_bytes: report_len as u16,
        ..InputHidPolicyWire::default()
    };
    policy.report[..report_len].copy_from_slice(&request.payload[..report_len]);
    let descriptor_start = report_len;
    let descriptor_end = descriptor_start + descriptor_len;
    policy.descriptor_prefix[..descriptor_len]
        .copy_from_slice(&request.payload[descriptor_start..descriptor_end]);

    response.value0 = policy.report_len as u64;
    response.value1 = policy.descriptor_len as u64;
    response.descriptor_count = 1;
    response.descriptors[0] = input_descriptor("hid-report-policy", request.header.op);
    response.capability = input_capability("hid-report", request.header.op);
    response.payload_len = write_payload_struct(&policy, &mut response.payload);
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
        COMMERCIAL_MAX_INPUTD_OP_SERIO_BUS_POLICY => 1 << 6,
        COMMERCIAL_MAX_INPUTD_OP_I8042_COMMAND_POLICY => 1 << 7,
        COMMERCIAL_MAX_INPUTD_OP_PS2_PACKET_POLICY => 1 << 8,
        COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY => 1 << 9,
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

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
