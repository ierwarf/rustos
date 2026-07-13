use std::collections::VecDeque;
use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::thread;
use std::time::Duration;

use keyboard_core::{KeyAction, KeyboardDriver, KeyboardEvent, ScanCodeSet};
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY, COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE,
    COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY, COMMERCIAL_MAX_INPUTD_OP_I8042_COMMAND_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST, COMMERCIAL_MAX_INPUTD_OP_INPUT_READER,
    COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS, COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY, COMMERCIAL_MAX_INPUTD_OP_PS2_PACKET_POLICY,
    COMMERCIAL_MAX_INPUTD_OP_SERIO_BUS_POLICY, COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_INPUTD, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, INPUTD_ACCESS_EVDEV, INPUTD_ACCESS_NATIVE,
    INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY, INPUTD_HID_POLICY_KIND_KEYBOARD,
    INPUTD_HID_POLICY_KIND_POINTER, INPUTD_HID_POLICY_KIND_UNKNOWN,
    INPUTD_HID_POLICY_REPORT_CAPACITY, INPUTD_INGEST_MAX_EVENTS, INPUTD_INGRESS_FLAG_DVM_SOURCE,
    INPUTD_INGRESS_FLAG_RESET_STATE, INPUTD_INGRESS_KIND_DVM_LINUX_KEY, INPUTD_INGRESS_KIND_EVENT,
    INPUTD_INGRESS_KIND_HID_KEYBOARD_REPORT, INPUTD_INGRESS_KIND_HID_POINTER_REPORT,
    INPUTD_INGRESS_KIND_HID_RAW_REPORT, INPUTD_INGRESS_KIND_KEYBOARD,
    INPUTD_INGRESS_KIND_POINTER_ABSOLUTE, INPUTD_INGRESS_KIND_POINTER_PACKET,
    INPUTD_INGRESS_KIND_PS2_MOUSE_BYTE, INPUTD_INGRESS_KIND_PS2_SCANCODE, INPUTD_IPC_ABI_VERSION,
    INPUTD_IPC_OP_AUTHORIZE_READ, INPUTD_IPC_OP_DRAIN_INGEST, INPUTD_IPC_OP_PING,
    INPUTD_IPC_OP_READ, INPUTD_IPC_OP_SET_POINTER_SURFACE, INPUTD_IPC_OP_STATS,
    INPUTD_READ_FLAG_NONBLOCK, INPUTD_READ_PAYLOAD_CAPACITY, IPC_MAX_INLINE_BYTES,
    IPC_SERVICE_INPUTD, InputHidKeyboardReportWire, InputHidPointerReportWire, InputHidPolicyWire,
    InputIngestBrokerArgs, InputIngressWire, InputKeyboardEventWire, InputPointerAbsoluteWire,
    InputPointerPacketWire, InputStatsBrokerArgs, InputStatsWire, InputdIpcRequest,
    InputdIpcResponse, InputdPointerSurfaceRequest, InputdReadResponse, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_INPUT_INGEST_BROKER, SYS_RUSTOS_INPUT_STATS_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_TRY_RECV,
};

// Raw input is an independent source from the inputd endpoint.  A bounded
// user-space poll keeps ring0 as a transport queue while avoiding a runnable
// service spin when neither a request nor a hardware report is pending.
const INPUT_INGEST_POLL_INTERVAL: Duration = Duration::from_millis(4);
const INPUTD_QUEUE_MAX_EVENTS: usize = 4096;
const INPUTD_MAX_NATIVE_READ_BYTES: u64 = input_evdev::MAX_NATIVE_READ_BYTES as u64;
const INPUTD_MAX_EVDEV_READ_BYTES: u64 = input_evdev::MAX_EVDEV_READ_BYTES as u64;
const PS2_MOUSE_STATUS_LEFT: u8 = 1 << 0;
const PS2_MOUSE_STATUS_RIGHT: u8 = 1 << 1;
const PS2_MOUSE_STATUS_MIDDLE: u8 = 1 << 2;
const PS2_MOUSE_STATUS_ALWAYS_ONE: u8 = 1 << 3;
const PS2_MOUSE_STATUS_X_OVERFLOW: u8 = 1 << 6;
const PS2_MOUSE_STATUS_Y_OVERFLOW: u8 = 1 << 7;
#[derive(Default)]
struct HidKeyboardState {
    source_id: u64,
    modifiers: u8,
    keys: [u8; 16],
    key_count: usize,
    keyboard: KeyboardDriver,
}

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

    fn max_x(self) -> i32 {
        self.width.saturating_sub(1).min(i32::MAX as u32) as i32
    }

    fn max_y(self) -> i32 {
        self.height.saturating_sub(1).min(i32::MAX as u32) as i32
    }
}

#[derive(Default)]
struct InputQueue {
    events: VecDeque<input_evdev::InputEvent>,
    hid_keyboards: Vec<HidKeyboardState>,
    ps2_keyboard: KeyboardDriver,
    dvm_keyboard: KeyboardDriver,
    dvm_keyboard_observed: bool,
    dvm_pointer_observed: bool,
    ps2_mouse: Ps2MousePacketState,
    pointer_surface: PointerSurface,
    last_pointer_position: Option<(i32, i32)>,
    dropped_discrete: u64,
    dropped_lossy: u64,
    native_pointer_buttons: u8,
    dvm_pointer_buttons: u8,
    published_pointer_buttons: u8,
    read_authorizations: VecDeque<InputReadAuthorization>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputReadAuthorization {
    pid: u64,
    tid: u64,
    fd: u64,
    access: u16,
    approved_len: u64,
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
        if !self.make_room_for(event) {
            return;
        }
        self.events.push_back(event);
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
        self.events.pop_front()
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
        self.last_pointer_position = None;
        Ok(())
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
            self.push_keyboard_driver_event(event);
        }
    }

    /// Translate a Linux `EV_KEY` value from the DVM only in ring 3.  This
    /// keeps layout, modifiers, lock state, and printable text identical to
    /// the native HID/PS2 paths instead of leaking Linux codes into apps.
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
        self.dvm_keyboard = KeyboardDriver::default();
        self.dvm_pointer_buttons = 0;
        self.publish_pointer_button_edges();
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

    fn push_pointer_motion_and_scroll(&mut self, packet: InputPointerPacketWire) {
        if packet.dx != 0 || packet.dy != 0 {
            self.last_pointer_position = None;
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

    fn push_pointer_packet(&mut self, packet: InputPointerPacketWire) {
        self.push_pointer_motion_and_scroll(packet);
        self.native_pointer_buttons = packet.buttons;
        self.publish_pointer_button_edges();
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

    /// Return the first accepted DVM keyboard/pointer observations since the
    /// previous poll. These markers are intentionally one-shot diagnostics:
    /// they prove the end-to-end ingress route without turning normal input
    /// into a per-event logging channel.
    fn take_dvm_ingress_observations(&mut self) -> (bool, bool) {
        let keyboard = std::mem::take(&mut self.dvm_keyboard_observed);
        let pointer = std::mem::take(&mut self.dvm_pointer_observed);
        (keyboard, pointer)
    }

    fn push_pointer_absolute(&mut self, absolute: InputPointerAbsoluteWire) {
        if !self.pointer_surface.configured() {
            self.native_pointer_buttons = absolute.buttons;
            self.publish_pointer_button_edges();
            return;
        }
        let x = (absolute.x.min(self.pointer_surface.max_x() as u32)) as i32;
        let y = (absolute.y.min(self.pointer_surface.max_y() as u32)) as i32;
        if self.last_pointer_position != Some((x, y)) {
            self.push(input_evdev::InputEvent {
                kind: input_evdev::INPUT_KIND_POINTER_POSITION,
                action: input_evdev::INPUT_ACTION_NONE,
                code: 0,
                value0: x,
                value1: y,
                modifiers: 0,
                text: 0,
            });
            self.last_pointer_position = Some((x, y));
        }
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
        self.native_pointer_buttons = absolute.buttons;
        self.publish_pointer_button_edges();
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
                self.push_hid_key_transition(state_index, code, (report.modifiers & mask) == 0);
            }
        }

        let mut previous_index = 0usize;
        while previous_index < previous_key_count {
            let usage = previous_keys[previous_index];
            if !keys[..key_count].contains(&usage) {
                if let Some(code) = input_evdev::hid_usage_to_keycode(usage) {
                    self.push_hid_key_transition(state_index, code, true);
                }
            }
            previous_index += 1;
        }

        let mut current_index = 0usize;
        while current_index < key_count {
            let usage = keys[current_index];
            if !previous_keys[..previous_key_count].contains(&usage) {
                if let Some(code) = input_evdev::hid_usage_to_keycode(usage) {
                    self.push_hid_key_transition(state_index, code, false);
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
                if let Some(pointer) = decode_hid_pointer_report(
                    raw.source_id,
                    descriptor,
                    report,
                    self.pointer_surface,
                ) {
                    self.push_hid_pointer_report(pointer);
                }
            }
            _ => {
                if let Some(pointer) = decode_hid_pointer_report(
                    raw.source_id,
                    descriptor,
                    report,
                    self.pointer_surface,
                ) {
                    self.push_hid_pointer_report(pointer);
                } else if let Some(keyboard) =
                    decode_hid_keyboard_report(raw.source_id, descriptor, report)
                {
                    self.push_hid_keyboard_report(keyboard);
                }
            }
        }
    }

    fn push_hid_key_transition(
        &mut self,
        state_index: usize,
        code: input_evdev::KeyCode,
        released: bool,
    ) {
        self.hid_keyboards[state_index]
            .keyboard
            .inject_key_transition(code, released);
        while let Some(event) = self.hid_keyboards[state_index].keyboard.pop_event() {
            self.push_keyboard_driver_event(event);
        }
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
        let current = self.native_pointer_buttons | self.dvm_pointer_buttons;
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

fn decode_hid_keyboard_report(
    source_id: u64,
    descriptor: &[u8],
    report: &[u8],
) -> Option<InputHidKeyboardReportWire> {
    let (report_id, report) = report_payload_for_descriptor(descriptor, report)?;
    let fields = parse_hid_input_fields(descriptor, report_id);
    if !hid_fields_have_keyboard(&fields) {
        return None;
    }
    let mut modifiers = 0u8;
    let mut keys = [0_u8; 16];
    let mut key_count = 0usize;

    for field in fields {
        if field.usage_page != HID_USAGE_PAGE_KEYBOARD {
            continue;
        }
        let value = read_hid_field(report, field)?;
        if field.variable {
            if field.usage >= HID_USAGE_KEYBOARD_LEFT_CONTROL
                && field.usage <= HID_USAGE_KEYBOARD_RIGHT_GUI
                && value != 0
            {
                modifiers |= 1u8 << (field.usage - HID_USAGE_KEYBOARD_LEFT_CONTROL);
            } else if value != 0 && field.usage <= u8::MAX as u16 && key_count < keys.len() {
                keys[key_count] = field.usage as u8;
                key_count += 1;
            }
            continue;
        }

        if value > 0 && value <= u8::MAX as i32 && key_count < keys.len() {
            keys[key_count] = value as u8;
            key_count += 1;
        }
    }

    Some(InputHidKeyboardReportWire {
        source_id,
        modifiers,
        key_count: key_count as u8,
        reserved0: [0; 6],
        keys,
    })
}

fn decode_hid_pointer_report(
    source_id: u64,
    descriptor: &[u8],
    report: &[u8],
    pointer_surface: PointerSurface,
) -> Option<InputHidPointerReportWire> {
    let (report_id, report) = report_payload_for_descriptor(descriptor, report)?;
    let fields = parse_hid_input_fields(descriptor, report_id);
    if !hid_fields_have_pointer(&fields) {
        return None;
    }

    let mut buttons = 0u8;
    let mut x = None::<(i32, HidInputField)>;
    let mut y = None::<(i32, HidInputField)>;
    let mut wheel_vertical = 0i16;

    for field in fields {
        let value = read_hid_field(report, field)?;
        match (field.usage_page, field.usage) {
            (HID_USAGE_PAGE_BUTTON, usage @ 1..=8) => {
                if value != 0 {
                    buttons |= 1u8 << (usage - 1);
                }
            }
            (HID_USAGE_PAGE_GENERIC_DESKTOP, HID_USAGE_X) => x = Some((value, field)),
            (HID_USAGE_PAGE_GENERIC_DESKTOP, HID_USAGE_Y) => y = Some((value, field)),
            (HID_USAGE_PAGE_GENERIC_DESKTOP, HID_USAGE_WHEEL) => {
                wheel_vertical = value.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
            _ => {}
        }
    }

    let (raw_x, x_field) = x?;
    let (raw_y, y_field) = y?;
    let relative = x_field.relative || y_field.relative;
    if !relative && !pointer_surface.configured() {
        return None;
    }
    let x_value = if relative {
        raw_x
    } else {
        scale_pointer_coordinate(
            raw_x,
            x_field.logical_min,
            x_field.logical_max,
            pointer_surface.max_x(),
        )
    };
    let y_value = if relative {
        raw_y
    } else {
        scale_pointer_coordinate(
            raw_y,
            y_field.logical_min,
            y_field.logical_max,
            pointer_surface.max_y(),
        )
    };

    Some(InputHidPointerReportWire {
        source_id,
        buttons,
        relative: relative as u8,
        reserved0: [0; 2],
        x: x_value,
        y: y_value,
        wheel_vertical,
        reserved1: 0,
    })
}

const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const HID_USAGE_PAGE_KEYBOARD: u16 = 0x07;
const HID_USAGE_PAGE_BUTTON: u16 = 0x09;
const HID_USAGE_X: u16 = 0x30;
const HID_USAGE_Y: u16 = 0x31;
const HID_USAGE_WHEEL: u16 = 0x38;
const HID_USAGE_KEYBOARD_LEFT_CONTROL: u16 = 0xe0;
const HID_USAGE_KEYBOARD_RIGHT_GUI: u16 = 0xe7;

#[derive(Clone, Copy, Debug, Default)]
struct HidInputField {
    usage_page: u16,
    usage: u16,
    bit_offset: usize,
    bit_size: u8,
    logical_min: i32,
    logical_max: i32,
    variable: bool,
    relative: bool,
}

fn report_payload_for_descriptor<'a>(
    descriptor: &[u8],
    report: &'a [u8],
) -> Option<(u8, &'a [u8])> {
    if report.is_empty() {
        return None;
    }
    if !descriptor_has_report_id(descriptor) {
        return Some((0, report));
    }
    Some((report[0], &report[1..]))
}

fn descriptor_has_report_id(descriptor: &[u8]) -> bool {
    descriptor.windows(2).any(|item| item[0] == 0x85)
}

fn parse_hid_input_fields(descriptor: &[u8], wanted_report_id: u8) -> Vec<HidInputField> {
    let mut fields = Vec::new();
    let mut usage_page = 0u16;
    let mut logical_min = 0i32;
    let mut logical_max = 0i32;
    let mut report_size = 0u8;
    let mut report_count = 0u8;
    let mut report_id = 0u8;
    let mut usages = Vec::<u16>::new();
    let mut usage_min = None::<u16>;
    let mut usage_max = None::<u16>;
    let mut offsets = Vec::<(u8, usize)>::new();
    let mut index = 0usize;

    while index < descriptor.len() {
        let prefix = descriptor[index];
        index += 1;
        if prefix == 0xfe {
            if index + 1 >= descriptor.len() {
                break;
            }
            let size = descriptor[index] as usize;
            index = index
                .saturating_add(2)
                .saturating_add(size)
                .min(descriptor.len());
            continue;
        }
        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if index + size > descriptor.len() {
            break;
        }
        let data = &descriptor[index..index + size];
        index += size;
        let item_type = (prefix >> 2) & 0x03;
        let tag = (prefix >> 4) & 0x0f;
        let unsigned = hid_item_unsigned(data);
        let signed = hid_item_signed(data);

        match (item_type, tag) {
            (1, 0x0) => usage_page = unsigned as u16,
            (1, 0x1) => logical_min = signed,
            (1, 0x2) => logical_max = signed,
            (1, 0x7) => report_size = unsigned.min(u8::MAX as u32) as u8,
            (1, 0x8) => report_id = unsigned.min(u8::MAX as u32) as u8,
            (1, 0x9) => report_count = unsigned.min(u8::MAX as u32) as u8,
            (2, 0x0) => usages.push(unsigned as u16),
            (2, 0x1) => usage_min = Some(unsigned as u16),
            (2, 0x2) => usage_max = Some(unsigned as u16),
            (0, 0x8) => {
                let flags = unsigned as u8;
                let is_constant = flags & 0x01 != 0;
                let is_variable = flags & 0x02 != 0;
                let is_relative = flags & 0x04 != 0;
                let count = report_count.max(1);
                let size_bits = report_size as usize;
                let start_offset = report_offset_mut(&mut offsets, report_id);
                for field_index in 0..count {
                    let bit_offset = *start_offset + field_index as usize * size_bits;
                    if !is_constant && report_id == wanted_report_id {
                        let usage = usage_for_field(field_index, &usages, usage_min, usage_max);
                        fields.push(HidInputField {
                            usage_page,
                            usage,
                            bit_offset,
                            bit_size: report_size,
                            logical_min,
                            logical_max,
                            variable: is_variable,
                            relative: is_relative,
                        });
                    }
                }
                *start_offset = start_offset.saturating_add(count as usize * size_bits);
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            (0, _) => {
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            _ => {}
        }
    }

    fields
}

fn hid_fields_have_keyboard(fields: &[HidInputField]) -> bool {
    fields
        .iter()
        .any(|field| field.usage_page == HID_USAGE_PAGE_KEYBOARD)
}

fn report_offset_mut(offsets: &mut Vec<(u8, usize)>, report_id: u8) -> &mut usize {
    if let Some(index) = offsets.iter().position(|(id, _)| *id == report_id) {
        return &mut offsets[index].1;
    }
    offsets.push((report_id, 0));
    &mut offsets.last_mut().expect("offset inserted").1
}

fn usage_for_field(
    field_index: u8,
    usages: &[u16],
    usage_min: Option<u16>,
    usage_max: Option<u16>,
) -> u16 {
    usages
        .get(field_index as usize)
        .copied()
        .or_else(|| {
            let min = usage_min?;
            let max = usage_max.unwrap_or(min);
            Some(min.saturating_add(field_index as u16).min(max))
        })
        .or_else(|| usages.last().copied())
        .unwrap_or(0)
}

fn hid_fields_have_pointer(fields: &[HidInputField]) -> bool {
    fields.iter().any(|field| {
        field.usage_page == HID_USAGE_PAGE_GENERIC_DESKTOP && field.usage == HID_USAGE_X
    }) && fields.iter().any(|field| {
        field.usage_page == HID_USAGE_PAGE_GENERIC_DESKTOP && field.usage == HID_USAGE_Y
    })
}

fn read_hid_field(report: &[u8], field: HidInputField) -> Option<i32> {
    if field.bit_size == 0 || field.bit_size > 32 {
        return None;
    }
    let raw = read_bits_le(report, field.bit_offset, field.bit_size as usize)?;
    if field.logical_min < 0 {
        let shift = 32usize.saturating_sub(field.bit_size as usize);
        Some(((raw << shift) as i32) >> shift)
    } else {
        Some(raw as i32)
    }
}

fn read_bits_le(report: &[u8], bit_offset: usize, bit_size: usize) -> Option<u32> {
    if bit_size == 0 || bit_size > 32 {
        return None;
    }
    let end_bit = bit_offset.checked_add(bit_size)?;
    if end_bit > report.len().checked_mul(8)? {
        return None;
    }
    let mut value = 0u32;
    for bit in 0..bit_size {
        let source_bit = bit_offset + bit;
        let byte = *report.get(source_bit / 8)?;
        let bit_value = (byte >> (source_bit % 8)) & 1;
        value |= u32::from(bit_value) << bit;
    }
    Some(value)
}

fn hid_item_unsigned(data: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    bytes[..data.len().min(4)].copy_from_slice(&data[..data.len().min(4)]);
    u32::from_le_bytes(bytes)
}

fn hid_item_signed(data: &[u8]) -> i32 {
    match data.len() {
        0 => 0,
        1 => i32::from(data[0] as i8),
        2 => i32::from(i16::from_le_bytes([data[0], data[1]])),
        _ => i32::from_le_bytes([
            data.first().copied().unwrap_or(0),
            data.get(1).copied().unwrap_or(0),
            data.get(2).copied().unwrap_or(0),
            data.get(3).copied().unwrap_or(0),
        ]),
    }
}

fn scale_pointer_coordinate(
    value: i32,
    logical_min: i32,
    logical_max: i32,
    target_max: i32,
) -> i32 {
    if target_max <= 0 || logical_max <= logical_min {
        return 0;
    }
    let clamped = value.clamp(logical_min, logical_max);
    let numerator = i64::from(clamped.saturating_sub(logical_min)) * i64::from(target_max);
    let denominator = i64::from(logical_max.saturating_sub(logical_min)).max(1);
    (numerator / denominator) as i32
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::InputQueue;
    use rustos_user_abi::syscall::{
        INPUTD_HID_POLICY_KIND_KEYBOARD, INPUTD_HID_POLICY_KIND_POINTER, InputHidPolicyWire,
        InputKeyboardEventWire, InputPointerPacketWire, InputdIpcRequest,
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
    fn inputd_queue_preserves_pointer_button_edges_between_positions() {
        let mut queue = InputQueue::default();
        queue.push(pointer_position(10, 20));
        queue.push_pointer_packet(pointer_packet(1, 0, 0));
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

        queue.push_pointer_packet(pointer_packet(1, 0, 0));

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
    fn inputd_keeps_dvm_pointer_buttons_separate_from_native_fallback() {
        let mut queue = InputQueue::default();
        queue.push_pointer_packet(pointer_packet(1, 0, 0));
        let native_press = queue.pop_front().unwrap();
        assert_eq!(native_press.action, input_evdev::INPUT_ACTION_PRESSED);

        // A DVM press must not duplicate the button edge, and its disconnect
        // must not release a simultaneously held fallback pointer button.
        queue.push_dvm_pointer_packet(pointer_packet(1, 0, 0));
        assert!(queue.is_empty());
        queue.push_dvm_pointer_packet(pointer_packet(0, 0, 0));
        assert!(queue.is_empty());

        queue.push_pointer_packet(pointer_packet(0, 0, 0));
        let native_release = queue.pop_front().unwrap();
        assert_eq!(native_release.action, input_evdev::INPUT_ACTION_RELEASED);
    }

    #[test]
    fn inputd_dvm_reset_releases_only_dvm_pointer_state() {
        let mut queue = InputQueue::default();
        queue.push_dvm_pointer_packet(pointer_packet(1, 0, 0));
        let press = queue.pop_front().unwrap();
        assert_eq!(press.action, input_evdev::INPUT_ACTION_PRESSED);

        queue.reset_dvm_input();
        let release = queue.pop_front().unwrap();
        assert_eq!(release.action, input_evdev::INPUT_ACTION_RELEASED);
        assert!(queue.is_empty());
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
    fn inputd_decodes_raw_hid_keyboard_report() {
        let mut queue = InputQueue::default();
        let descriptor = [
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
            0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
            0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65,
            0x81, 0x00, 0xc0,
        ];
        let report = [0x02, 0, 0x04, 0, 0, 0, 0, 0];
        queue.push_hid_raw_report(hid_raw(
            INPUTD_HID_POLICY_KIND_KEYBOARD,
            &report,
            &descriptor,
        ));

        let event = queue.pop_front().unwrap();
        assert_eq!(event.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(event.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(
            event.code,
            input_evdev::hid_usage_to_keycode(0xe1).unwrap() as u32
        );
        let event = queue.pop_front().unwrap();
        assert_eq!(event.kind, input_evdev::INPUT_KIND_KEYBOARD);
        assert_eq!(event.action, input_evdev::INPUT_ACTION_PRESSED);
        assert_eq!(
            event.code,
            input_evdev::hid_usage_to_keycode(0x04).unwrap() as u32
        );
        assert_ne!(event.modifiers, 0);
        assert_eq!(event.text, b'A' as u32);
    }

    #[test]
    fn inputd_decodes_raw_hid_pointer_report() {
        let mut queue = InputQueue::default();
        let descriptor = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x75, 0x05,
            0x95, 0x01, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81,
            0x25, 0x7f, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xc0, 0xc0,
        ];
        let report = [1, 7_u8, 0xfd_u8, 0];
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

    #[test]
    fn inputd_decodes_absolute_hid_tablet_report_from_descriptor_fields() {
        let mut queue = InputQueue::default();
        queue
            .set_pointer_surface(1280, 800, 9)
            .expect("pointer surface should be accepted");
        let descriptor = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff,
            0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75,
            0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
        ];
        let report = [0, 0xff, 0x7f, 0xff, 0x7f, 0];
        queue.push_hid_raw_report(hid_raw(
            INPUTD_HID_POLICY_KIND_POINTER,
            &report,
            &descriptor,
        ));

        let position = queue.pop_front().unwrap();
        assert_eq!(position.kind, input_evdev::INPUT_KIND_POINTER_POSITION);
        assert_eq!(position.value0, 1279);
        assert_eq!(position.value1, 799);
    }

    #[test]
    fn inputd_drops_absolute_hid_position_until_pointer_surface_is_known() {
        let mut queue = InputQueue::default();
        let descriptor = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff,
            0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xc0, 0xc0,
        ];
        let report = [0, 0xff, 0x7f, 0xff, 0x7f];
        queue.push_hid_raw_report(hid_raw(
            INPUTD_HID_POLICY_KIND_POINTER,
            &report,
            &descriptor,
        ));

        assert!(queue.is_empty());
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
    let register = register_service_endpoint(IPC_SERVICE_INPUTD, endpoint as u64);
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
    // This buffer is reused by both the periodic ingress turn and request
    // handlers.  Do not allocate a new 96 KiB wire batch on every poll.
    let mut ingest_scratch = vec![InputIngressWire::default(); INPUTD_INGEST_MAX_EVENTS];
    let mut ingest_failures = 0_u64;
    // These are lifecycle observations, not event telemetry. Preserve the
    // first proof of each DVM route without allowing an active device to flood
    // debugcon and perturb the UI path it is meant to validate.
    let mut dvm_keyboard_ingress_logged = false;
    let mut dvm_pointer_ingress_logged = false;
    loop {
        let ingested = match drain_ingest(&mut queue, &mut ingest_scratch) {
            Ok(count) => {
                ingest_failures = 0;
                count
            }
            Err(errno) => {
                ingest_failures = ingest_failures.saturating_add(1);
                if ingest_failures <= 3 || ingest_failures.is_power_of_two() {
                    debug_line(&format!(
                        "inputd: input ingest broker failed errno={errno} failures={ingest_failures}"
                    ));
                }
                0
            }
        };
        let (dvm_keyboard, dvm_pointer) = queue.take_dvm_ingress_observations();
        if dvm_keyboard && !dvm_keyboard_ingress_logged {
            debug_line("inputd: DVM keyboard ingress observed");
            dvm_keyboard_ingress_logged = true;
        }
        if dvm_pointer && !dvm_pointer_ingress_logged {
            debug_line("inputd: DVM pointer ingress observed");
            dvm_pointer_ingress_logged = true;
        }
        let mut request = [0_u8; IPC_MAX_INLINE_BYTES];
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_TRY_RECV,
            endpoint,
            request.as_mut_ptr() as u64,
            request.len() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            if ingested == 0 {
                thread::sleep(INPUT_INGEST_POLL_INTERVAL);
            }
            continue;
        }
        if received == 0 {
            thread::sleep(INPUT_INGEST_POLL_INTERVAL);
            continue;
        }
        let request_size = received as usize;
        if request_size == size_of::<CommercialMaxProtocolRequest>() {
            let request = read_unaligned::<CommercialMaxProtocolRequest>(&request);
            let reply =
                reply_commercial_request(reply_cap, &request, &mut queue, &mut ingest_scratch);
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
            }
            continue;
        }
        if request_size == size_of::<InputdPointerSurfaceRequest>() {
            let request = read_unaligned::<InputdPointerSurfaceRequest>(&request);
            let mut response = InputdIpcResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdIpcResponse::default()
            };
            response.status = dispatch_pointer_surface_request(&request, &mut queue);
            response.approved_len = (response.status == 0) as u64;
            let reply = syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const InputdIpcResponse) as u64,
                size_of::<InputdIpcResponse>() as u64,
            );
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
                Ok(()) => dispatch_read(&request, &mut response, &mut queue, &mut ingest_scratch),
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
                Ok(()) => dispatch(&request, &mut response, &mut queue, &mut ingest_scratch),
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
    ingest_scratch: &mut [InputIngressWire],
) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = validate_commercial_request(request)
        .and_then(|_| dispatch_commercial_request(request, &mut response, queue, ingest_scratch))
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
    ingest_scratch: &mut [InputIngressWire],
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
        match wire.kind {
            INPUTD_INGRESS_KIND_EVENT if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push(wire.event);
            }
            INPUTD_INGRESS_KIND_KEYBOARD if wire.access == INPUTD_ACCESS_NATIVE => {
                queue.push_keyboard_event(wire.keyboard);
            }
            INPUTD_INGRESS_KIND_DVM_LINUX_KEY if wire.access == INPUTD_ACCESS_NATIVE => {
                if wire.flags & INPUTD_INGRESS_FLAG_RESET_STATE != 0 {
                    queue.reset_dvm_input();
                    continue;
                }
                queue.push_dvm_linux_key(wire.keyboard.code, wire.keyboard.action);
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
                if wire.flags & INPUTD_INGRESS_FLAG_DVM_SOURCE != 0 {
                    queue.push_dvm_pointer_packet(wire.pointer_packet);
                } else {
                    queue.push_pointer_packet(wire.pointer_packet);
                }
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
    stats.dropped_discrete = stats
        .dropped_discrete
        .saturating_add(queue.dropped_discrete);
    stats.dropped_lossy = stats.dropped_lossy.saturating_add(queue.dropped_lossy);
    Ok(stats)
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
        COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY => {
            response.value0 = (u64::from(queue.pointer_surface.width) << 32)
                | u64::from(queue.pointer_surface.height);
            response.value1 = queue.pointer_surface.generation;
            response.descriptor_count = 1;
            response.descriptors[0] = input_descriptor("pointer-surface-policy", request.header.op);
            response.capability = input_capability("pointer-surface", request.header.op);
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
        | COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY
        | COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY => Ok(()),
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

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn register_service_endpoint(service_id: u64, endpoint: u64) -> i64 {
    let mut last = 0;
    for _ in 0..65_536 {
        last = syscall2(
            SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
            service_id,
            endpoint,
        );
        if last >= 0 {
            return last;
        }
        let errno = (-last) as i32;
        if errno != libc::EACCES && errno != libc::EPERM && errno != libc::ENOENT {
            return last;
        }
        thread::yield_now();
    }
    last
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
