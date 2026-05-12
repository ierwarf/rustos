// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// use alloc::string::String;
// use alloc::vec;
//
// use crate::input::keyboard::{KeyAction, KeyCode, KeyboardEvent, Modifiers};
// use crate::io::console;
// use crate::io::session::{self, ConsoleSessionHandle, ConsoleSessionInfo, ConsoleSessionState};
// use crate::io::tty;
// use crate::multitask;
// use crate::user::abi::console as console_abi;
// use crate::user::abi::device as device_abi;
// use crate::user::process_state::UserProcessState;
// use x86_64::VirtAddr;
//
// use super::{DeviceError, read_user_struct, write_user_struct};
//
// const MAX_CONSOLE_SNAPSHOT_BYTES: usize = 4096;
//
// pub(crate) fn ioctl(
//     process_state: &mut UserProcessState,
//     request: u64,
//     arg: u64,
// ) -> Result<u64, DeviceError> {
//     match request {
//         console_abi::CONSOLE_IOCTL_GET_STATE => {
//             let mut sessions = [ConsoleSessionInfo::default(); console_abi::MAX_CONSOLE_SESSIONS];
//             let count = session::snapshot_console_sessions(&mut sessions);
//             let info = console_abi::ConsoleStateInfo {
//                 focused_session_handle: session::focused_console_session()
//                     .unwrap_or(ConsoleSessionHandle::SYSTEM)
//                     .raw(),
//                 session_count: count as u32,
//                 reserved: 0,
//             };
//             write_user_struct(process_state.address_space(), arg, &info)?;
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
//             let mut snapshot = read_user_struct::<console_abi::ConsoleSnapshotSessionsRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let capacity =
//                 usize::try_from(snapshot.capacity).map_err(|_| DeviceError::InvalidArgument)?;
//             if capacity == 0 {
//                 snapshot.count = 0;
//                 write_user_struct(process_state.address_space(), arg, &snapshot)?;
//                 return Ok(0);
//             }
//
//             let copy_capacity = capacity.min(console_abi::MAX_CONSOLE_SESSIONS);
//             let mut sessions = [ConsoleSessionInfo::default(); console_abi::MAX_CONSOLE_SESSIONS];
//             let count = session::snapshot_console_sessions(&mut sessions).min(copy_capacity);
//             let mut user_sessions =
//                 [console_abi::ConsoleSessionInfo::default(); console_abi::MAX_CONSOLE_SESSIONS];
//             for (dest, source) in user_sessions
//                 .iter_mut()
//                 .zip(sessions.into_iter().take(count))
//             {
//                 dest.session_handle = source.handle.raw();
//                 dest.state = encode_session_state(source.state);
//                 dest.focused = u16::from(source.focused);
//                 dest.output_generation = console::session_output_generation(source.handle);
//                 dest.title = source.title;
//             }
//             let bytes_len = count
//                 .checked_mul(core::mem::size_of::<console_abi::ConsoleSessionInfo>())
//                 .ok_or(DeviceError::InvalidArgument)?;
//             if count != 0 {
//                 process_state
//                     .address_space()
//                     .validate_user_write_buffer(VirtAddr::new(snapshot.sessions_ptr), bytes_len)?;
//                 let bytes = unsafe {
//                     core::slice::from_raw_parts(user_sessions.as_ptr().cast::<u8>(), bytes_len)
//                 };
//                 process_state
//                     .address_space()
//                     .copy_into_user(VirtAddr::new(snapshot.sessions_ptr), bytes)?;
//             }
//             snapshot.count = count as u64;
//             write_user_struct(process_state.address_space(), arg, &snapshot)?;
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
//             let mut snapshot = read_user_struct::<console_abi::ConsoleSnapshotSessionOutputRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let session = ConsoleSessionHandle::from_raw(snapshot.session_handle);
//             if !session::is_console_session_active(session) {
//                 return Err(DeviceError::InvalidArgument);
//             }
//             let capacity =
//                 usize::try_from(snapshot.capacity).map_err(|_| DeviceError::InvalidArgument)?;
//             if capacity == 0 {
//                 snapshot.count = 0;
//                 write_user_struct(process_state.address_space(), arg, &snapshot)?;
//                 return Ok(0);
//             }
//
//             let copy_len = capacity.min(MAX_CONSOLE_SNAPSHOT_BYTES);
//             let mut bytes = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
//             let count = console::snapshot_recent_output(session, &mut bytes[..copy_len]);
//             if count != 0 {
//                 process_state
//                     .address_space()
//                     .validate_user_write_buffer(VirtAddr::new(snapshot.bytes_ptr), count)?;
//                 process_state
//                     .address_space()
//                     .copy_into_user(VirtAddr::new(snapshot.bytes_ptr), &bytes[..count])?;
//             }
//             snapshot.count = count as u64;
//             write_user_struct(process_state.address_space(), arg, &snapshot)?;
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_SET_FOCUS => {
//             let request = read_user_struct::<console_abi::ConsoleSetFocusRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let session = ConsoleSessionHandle::from_raw(request.session_handle);
//             if !session::set_focused_console_session(session)
//                 && session::focused_console_session() != Some(session)
//             {
//                 return Err(DeviceError::NotFound);
//             }
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
//             let request = read_user_struct::<console_abi::ConsoleSendInputEventRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let session = ConsoleSessionHandle::from_raw(request.session_handle);
//             if !session::is_console_session_active(session) {
//                 return Err(DeviceError::InvalidArgument);
//             }
//             let event =
//                 keyboard_event_from_input(request.event).ok_or(DeviceError::InvalidArgument)?;
//             tty::on_key_event_for_session(session, event);
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_CREATE_SESSION => {
//             let mut request = read_user_struct::<console_abi::ConsoleCreateSessionRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let title = read_user_string(
//                 process_state,
//                 request.title_ptr,
//                 request.title_len,
//                 console_abi::CONSOLE_SESSION_TITLE_CAPACITY,
//             )?;
//             let exec_path = read_user_string(
//                 process_state,
//                 request.exec_path_ptr,
//                 request.exec_path_len,
//                 console_abi::CONSOLE_SESSION_PATH_CAPACITY,
//             )?;
//             let handle = session::create_console_session(request.program_id, &title, &exec_path)
//                 .map_err(|_| DeviceError::InvalidArgument)?;
//             request.session_handle = handle.raw();
//             write_user_struct(process_state.address_space(), arg, &request)?;
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_CLOSE_SESSION => {
//             let request = read_user_struct::<console_abi::ConsoleCloseSessionRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let session = ConsoleSessionHandle::from_raw(request.session_handle);
//             if !close_console_session(session) {
//                 return Err(DeviceError::NotFound);
//             }
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_BIND_CURRENT_SESSION => {
//             let request = read_user_struct::<console_abi::ConsoleBindCurrentSessionRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let session = ConsoleSessionHandle::from_raw(request.session_handle);
//             if !(session.is_system() || session::is_console_session_active(session)) {
//                 return Err(DeviceError::NotFound);
//             }
//             if !multitask::set_current_console_session(session) {
//                 return Err(DeviceError::Unsupported);
//             }
//             Ok(0)
//         }
//         console_abi::CONSOLE_IOCTL_SET_SESSION_STATE => {
//             let request = read_user_struct::<console_abi::ConsoleSetSessionStateRequest>(
//                 process_state.address_space(),
//                 arg,
//             )?;
//             let session = ConsoleSessionHandle::from_raw(request.session_handle);
//             let state = decode_session_state(request.state).ok_or(DeviceError::InvalidArgument)?;
//             if !session::transition_console_session_state(session, state) {
//                 return Err(DeviceError::NotFound);
//             }
//             Ok(0)
//         }
//         _ => Err(DeviceError::Unsupported),
//     }
// }
//
// fn encode_session_state(state: ConsoleSessionState) -> u16 {
//     match state {
//         ConsoleSessionState::Queued => console_abi::CONSOLE_SESSION_STATE_QUEUED,
//         ConsoleSessionState::LoadingImage => console_abi::CONSOLE_SESSION_STATE_LOADING_IMAGE,
//         ConsoleSessionState::Spawning => console_abi::CONSOLE_SESSION_STATE_SPAWNING,
//         ConsoleSessionState::Running => console_abi::CONSOLE_SESSION_STATE_RUNNING,
//         ConsoleSessionState::Closing => console_abi::CONSOLE_SESSION_STATE_CLOSING,
//     }
// }
//
// fn decode_session_state(state: u16) -> Option<ConsoleSessionState> {
//     match state {
//         console_abi::CONSOLE_SESSION_STATE_QUEUED => Some(ConsoleSessionState::Queued),
//         console_abi::CONSOLE_SESSION_STATE_LOADING_IMAGE => Some(ConsoleSessionState::LoadingImage),
//         console_abi::CONSOLE_SESSION_STATE_SPAWNING => Some(ConsoleSessionState::Spawning),
//         console_abi::CONSOLE_SESSION_STATE_RUNNING => Some(ConsoleSessionState::Running),
//         console_abi::CONSOLE_SESSION_STATE_CLOSING => Some(ConsoleSessionState::Closing),
//         _ => None,
//     }
// }
//
// fn close_console_session(session: ConsoleSessionHandle) -> bool {
//     if session.is_system() || !session::is_console_session_active(session) {
//         return false;
//     }
//     let _ = session::transition_console_session_state(session, ConsoleSessionState::Closing);
//     let _ = session::assign_console_session_pid(session, None);
//     console::reset_session(session);
//     tty::reset_session(session);
//     session::remove_console_session(session)
// }
//
// fn read_user_string(
//     process_state: &mut UserProcessState,
//     user_ptr: u64,
//     user_len: u64,
//     max_len: usize,
// ) -> Result<String, DeviceError> {
//     let len = usize::try_from(user_len).map_err(|_| DeviceError::InvalidArgument)?;
//     if len == 0 || len > max_len {
//         return Err(DeviceError::InvalidArgument);
//     }
//     let mut bytes = vec![0_u8; len];
//     process_state
//         .address_space()
//         .validate_user_read_buffer(VirtAddr::new(user_ptr), len)?;
//     process_state
//         .address_space()
//         .copy_from_user(VirtAddr::new(user_ptr), &mut bytes)?;
//     String::from_utf8(bytes).map_err(|_| DeviceError::InvalidArgument)
// }
//
// fn keyboard_event_from_input(event: device_abi::InputEvent) -> Option<KeyboardEvent> {
//     if event.kind != device_abi::INPUT_KIND_KEYBOARD {
//         return None;
//     }
//
//     let action = match event.action {
//         device_abi::INPUT_ACTION_PRESSED => KeyAction::Pressed,
//         device_abi::INPUT_ACTION_RELEASED => KeyAction::Released,
//         device_abi::INPUT_ACTION_REPEATED => KeyAction::Repeated,
//         _ => return None,
//     };
//     let code = KeyCode::from_u32(event.code)?;
//     let text = u8::try_from(event.text).ok().filter(|byte| *byte != 0);
//     Some(KeyboardEvent {
//         code,
//         action,
//         modifiers: Modifiers::from_bits_truncate(event.modifiers as u8),
//         text,
//     })
// }
