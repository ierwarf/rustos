use crate::input::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
use crate::io::console;
use crate::io::session::{self, ConsoleSessionHandle, ConsoleSessionInfo, ConsoleSessionState};
use crate::io::tty;
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
            let mut sessions = [ConsoleSessionInfo::default(); console_abi::MAX_CONSOLE_SESSIONS];
            let count = session::snapshot_console_sessions(&mut sessions);
            let info = console_abi::ConsoleStateInfo {
                focused_session_handle: session::focused_console_session()
                    .unwrap_or(ConsoleSessionHandle::SYSTEM)
                    .raw(),
                session_count: count as u32,
                reserved: 0,
            };
            write_user_struct(process_state.address_space(), arg, &info)?;
            Ok(0)
        }
        console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            let mut snapshot = read_user_struct::<console_abi::ConsoleSnapshotSessionsRequest>(
                process_state.address_space(),
                arg,
            )?;
            let capacity =
                usize::try_from(snapshot.capacity).map_err(|_| DeviceError::InvalidArgument)?;
            if capacity == 0 {
                snapshot.count = 0;
                write_user_struct(process_state.address_space(), arg, &snapshot)?;
                return Ok(0);
            }

            let copy_capacity = capacity.min(console_abi::MAX_CONSOLE_SESSIONS);
            let mut sessions = [ConsoleSessionInfo::default(); console_abi::MAX_CONSOLE_SESSIONS];
            let count = session::snapshot_console_sessions(&mut sessions).min(copy_capacity);
            let mut user_sessions =
                [console_abi::ConsoleSessionInfo::default(); console_abi::MAX_CONSOLE_SESSIONS];
            for (dest, source) in user_sessions
                .iter_mut()
                .zip(sessions.into_iter().take(count))
            {
                dest.session_handle = source.handle.raw();
                dest.state = encode_session_state(source.state);
                dest.focused = u16::from(source.focused);
                dest.output_generation = console::session_output_generation(source.handle);
                dest.title = source.title;
            }
            let bytes_len = count
                .checked_mul(core::mem::size_of::<console_abi::ConsoleSessionInfo>())
                .ok_or(DeviceError::InvalidArgument)?;
            if count != 0 {
                process_state
                    .address_space()
                    .validate_user_write_buffer(VirtAddr::new(snapshot.sessions_ptr), bytes_len)?;
                let bytes = unsafe {
                    core::slice::from_raw_parts(user_sessions.as_ptr().cast::<u8>(), bytes_len)
                };
                process_state
                    .address_space()
                    .copy_into_user(VirtAddr::new(snapshot.sessions_ptr), bytes)?;
            }
            snapshot.count = count as u64;
            write_user_struct(process_state.address_space(), arg, &snapshot)?;
            Ok(0)
        }
        console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            let mut snapshot = read_user_struct::<console_abi::ConsoleSnapshotSessionOutputRequest>(
                process_state.address_space(),
                arg,
            )?;
            let session = ConsoleSessionHandle::from_raw(snapshot.session_handle);
            if !session::is_console_session_active(session) {
                return Err(DeviceError::InvalidArgument);
            }
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
            let session = ConsoleSessionHandle::from_raw(request.session_handle);
            if !session::set_focused_console_session(session)
                && session::focused_console_session() != Some(session)
            {
                return Err(DeviceError::NotFound);
            }
            Ok(0)
        }
        console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
            let request = read_user_struct::<console_abi::ConsoleSendInputEventRequest>(
                process_state.address_space(),
                arg,
            )?;
            let session = ConsoleSessionHandle::from_raw(request.session_handle);
            if !session::is_console_session_active(session) {
                return Err(DeviceError::InvalidArgument);
            }
            let event =
                keyboard_event_from_input(request.event).ok_or(DeviceError::InvalidArgument)?;
            tty::on_key_event_for_session(session, event);
            Ok(0)
        }
        _ => Err(DeviceError::Unsupported),
    }
}

fn encode_session_state(state: ConsoleSessionState) -> u16 {
    match state {
        ConsoleSessionState::Queued => console_abi::CONSOLE_SESSION_STATE_QUEUED,
        ConsoleSessionState::LoadingImage => console_abi::CONSOLE_SESSION_STATE_LOADING_IMAGE,
        ConsoleSessionState::Spawning => console_abi::CONSOLE_SESSION_STATE_SPAWNING,
        ConsoleSessionState::Running => console_abi::CONSOLE_SESSION_STATE_RUNNING,
        ConsoleSessionState::Closing => console_abi::CONSOLE_SESSION_STATE_CLOSING,
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
