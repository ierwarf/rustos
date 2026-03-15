use core::mem::size_of;

use spin::Mutex;

use crate::gui;
use crate::keyboard::{KeyAction, KeyboardEvent};
use crate::ring::RingBuffer;
use crate::user::abi::ui::{
    INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED,
    INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION,
    PIXEL_FORMAT_BGRA8888, POINTER_BUTTON_LEFT, UiDisplayInfo, UiInputEvent,
};

const INPUT_EVENT_QUEUE_CAPACITY: usize = 256;

static INPUT_EVENTS: Mutex<RingBuffer<UiInputEvent, INPUT_EVENT_QUEUE_CAPACITY>> =
    Mutex::new(RingBuffer::new());

impl UiDisplayInfo {
    pub const fn byte_len() -> usize {
        size_of::<Self>()
    }
}

impl UiInputEvent {
    pub const fn byte_len() -> usize {
        size_of::<Self>()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiPresentError {
    DisplayUnavailable,
    InvalidDimensions,
    InvalidStride,
    InvalidFormat,
    BufferTooSmall,
}

pub fn snapshot_display_info() -> Option<UiDisplayInfo> {
    let info = gui::display_info()?;
    Some(UiDisplayInfo {
        width: info.width,
        height: info.height,
        stride_bytes: info.stride_bytes,
        bytes_per_pixel: info.bytes_per_pixel,
        pixel_format: PIXEL_FORMAT_BGRA8888,
        reserved: 0,
    })
}

pub fn present_frame_bgra8888(
    width: usize,
    height: usize,
    stride_bytes: usize,
    bytes: &[u8],
) -> Result<(), UiPresentError> {
    let display = snapshot_display_info().ok_or(UiPresentError::DisplayUnavailable)?;
    if width != display.width as usize || height != display.height as usize {
        return Err(UiPresentError::InvalidDimensions);
    }

    let min_stride = width
        .checked_mul(display.bytes_per_pixel as usize)
        .ok_or(UiPresentError::InvalidStride)?;
    if stride_bytes < min_stride {
        return Err(UiPresentError::InvalidStride);
    }

    let required_len = stride_bytes
        .checked_mul(height)
        .ok_or(UiPresentError::BufferTooSmall)?;
    if bytes.len() < required_len {
        return Err(UiPresentError::BufferTooSmall);
    }

    if !gui::present_userspace_frame_bgra8888(width, height, stride_bytes, bytes) {
        return Err(UiPresentError::DisplayUnavailable);
    }

    Ok(())
}

pub fn push_keyboard_event(event: KeyboardEvent) {
    INPUT_EVENTS.lock().push_overwrite(UiInputEvent {
        kind: INPUT_KIND_KEYBOARD,
        action: map_key_action(event.action),
        code: event.code as u32,
        value0: 0,
        value1: 0,
        modifiers: event.modifiers.bits() as u32,
        text: event.text.unwrap_or(0) as u32,
    });
}

pub fn push_pointer_motion(dx: i16, dy: i16) {
    if dx == 0 && dy == 0 {
        return;
    }

    INPUT_EVENTS.lock().push_overwrite(UiInputEvent {
        kind: INPUT_KIND_POINTER_MOTION,
        action: INPUT_ACTION_NONE,
        code: 0,
        value0: dx as i32,
        value1: dy as i32,
        modifiers: 0,
        text: 0,
    });
}

pub fn push_pointer_button_left(pressed: bool) {
    INPUT_EVENTS.lock().push_overwrite(UiInputEvent {
        kind: INPUT_KIND_POINTER_BUTTON,
        action: if pressed {
            INPUT_ACTION_PRESSED
        } else {
            INPUT_ACTION_RELEASED
        },
        code: POINTER_BUTTON_LEFT,
        value0: 0,
        value1: 0,
        modifiers: 0,
        text: 0,
    });
}

pub fn read_input_events(dest: &mut [UiInputEvent]) -> usize {
    INPUT_EVENTS.lock().pop_into(dest)
}

#[allow(dead_code)]
pub fn copy_input_events(dest: &mut [UiInputEvent]) -> usize {
    INPUT_EVENTS.lock().copy_into(dest)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *INPUT_EVENTS.lock() = RingBuffer::new();
}

fn map_key_action(action: KeyAction) -> u16 {
    match action {
        KeyAction::Pressed => INPUT_ACTION_PRESSED,
        KeyAction::Released => INPUT_ACTION_RELEASED,
        KeyAction::Repeated => INPUT_ACTION_REPEATED,
    }
}
