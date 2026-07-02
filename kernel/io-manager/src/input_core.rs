// RING3-MIGRATION-REFERENCE START: input-ingress exception: inputd owns input
// event coalescing, lossy/drop policy, and evdev translation. Ring0 keeps only
// shared input ABI aliases for hardware ingress.
//! Shared input ABI aliases used by kernel hardware ingress.
//!
//! Reader queue coalescing, evdev translation, and drop policy live in `inputd`
//! and the shared `input-evdev` crate so ring0 stays a thin report source.

pub use driver_abi::PointerPacket;
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
// RING3-MIGRATION-REFERENCE END: inputd-owned input core ingress exception.
