use crate::console;
use crate::input::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
use crate::session::{
    ConsoleSessionId, active_console_sessions, focused_console_session, set_focused_console_session,
};
use crate::tty;
use crate::user::abi::console as console_abi;
use crate::user::abi::device as device_abi;
use crate::user::process_state::UserProcessState;
use x86_64::VirtAddr;

use super::{DeviceError, read_user_struct, write_user_struct};

const MAX_CONSOLE_SNAPSHOT_BYTES: usize = 4096;

pub(crate) fn ioctl(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceError> {
    match request {
        console_abi::CONSOLE_IOCTL_GET_STATE => {
            let mut info = console_abi::ConsoleStateInfo::default();
            for session in active_console_sessions().iter() {
                info.active_session_mask |= 1_u64 << session.index();
            }
            info.focused_session_index = focused_console_session().index() as u32;
            info.output_generations = console::snapshot_output_generations();
            write_user_struct(process_state.address_space(), arg, &info)?;
            Ok(0)
        }
        console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            let mut snapshot = read_user_struct::<console_abi::ConsoleSnapshotSessionOutputRequest>(
                process_state.address_space(),
                arg,
            )?;
            let session = ConsoleSessionId::from_index(snapshot.session_index as usize)
                .ok_or(DeviceError::InvalidArgument)?;
            let capacity =
                usize::try_from(snapshot.capacity).map_err(|_| DeviceError::InvalidArgument)?;
            if capacity == 0 {
                snapshot.count = 0;
                write_user_struct(process_state.address_space(), arg, &snapshot)?;
                return Ok(0);
            }

            let copy_len = capacity.min(MAX_CONSOLE_SNAPSHOT_BYTES);
            let mut bytes = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
            let count = console::snapshot_recent_output(session, &mut bytes[..copy_len]);
            if count != 0 {
                process_state
                    .address_space()
                    .validate_user_write_buffer(VirtAddr::new(snapshot.bytes_ptr), count)?;
                process_state
                    .address_space()
                    .copy_into_user(VirtAddr::new(snapshot.bytes_ptr), &bytes[..count])?;
            }
            snapshot.count = count as u64;
            write_user_struct(process_state.address_space(), arg, &snapshot)?;
            Ok(0)
        }
        console_abi::CONSOLE_IOCTL_SET_FOCUS => {
            let request = read_user_struct::<console_abi::ConsoleSetFocusRequest>(
                process_state.address_space(),
                arg,
            )?;
            let session = ConsoleSessionId::from_index(request.session_index as usize)
                .ok_or(DeviceError::InvalidArgument)?;
            if !set_focused_console_session(session) && focused_console_session() != session {
                return Err(DeviceError::NotFound);
            }
            Ok(0)
        }
        console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
            let request = read_user_struct::<console_abi::ConsoleSendInputEventRequest>(
                process_state.address_space(),
                arg,
            )?;
            let session = ConsoleSessionId::from_index(request.session_index as usize)
                .ok_or(DeviceError::InvalidArgument)?;
            let event =
                keyboard_event_from_input(request.event).ok_or(DeviceError::InvalidArgument)?;
            tty::on_key_event_for_session(session, event);
            Ok(0)
        }
        _ => Err(DeviceError::Unsupported),
    }
}

fn keyboard_event_from_input(event: device_abi::InputEvent) -> Option<KeyboardEvent> {
    if event.kind != device_abi::INPUT_KIND_KEYBOARD {
        return None;
    }

    let action = match event.action {
        device_abi::INPUT_ACTION_PRESSED => KeyAction::Pressed,
        device_abi::INPUT_ACTION_RELEASED => KeyAction::Released,
        device_abi::INPUT_ACTION_REPEATED => KeyAction::Repeated,
        _ => return None,
    };
    let code = KeyCode::from_u32(event.code)?;
    let text = u8::try_from(event.text).ok().filter(|byte| *byte != 0);
    Some(KeyboardEvent {
        code,
        action,
        modifiers: Modifiers::from_bits_truncate(event.modifiers as u8),
        text,
    })
}
