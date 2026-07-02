use std::io::Write;
use std::mem::size_of;
use std::os::unix::net::UnixStream;

use super::{RuntimeRequest, RuntimeResponse, MAX_REQUEST_PATH_BYTES};
use super::{
    LAUNCH_TARGET_NEW_SESSION, OP_NOTIFY_READY, OP_REQUEST_LAUNCH_PATH, OP_REQUEST_TERMINATE,
    OP_SNAPSHOT_RUNNING_PROGRAMS, READY_COMPONENT_UI_SERVER, TERMINATE_TARGET_PID,
    TERMINATE_TARGET_SESSION,
};

pub(super) fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

pub(super) fn as_bytes_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
}

pub(super) fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    assert!(bytes.len() >= size_of::<T>());
    unsafe { bytes.as_ptr().cast::<T>().read_unaligned() }
}

pub(super) fn copy_ascii_into(dest: &mut [u8], value: &str) {
    dest.fill(0);
    for (index, byte) in value.bytes().enumerate() {
        if index >= dest.len() {
            break;
        }
        dest[index] = match byte {
            b' '..=b'~' => byte,
            _ => b'?',
        };
    }
}

pub(super) fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

pub(super) fn io_errno(err: std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(libc::EIO)
}

pub(super) fn write_response(
    stream: &mut UnixStream,
    response: RuntimeResponse,
) -> Result<(), i32> {
    stream.write_all(as_bytes(&response)).map_err(io_errno)
}

pub(super) fn request_path(request: &RuntimeRequest) -> Result<String, i32> {
    let len = usize::try_from(request.text_len).map_err(|_| libc::EINVAL)?;
    if len > request.text.len() {
        return Err(libc::EINVAL);
    }
    let path = String::from_utf8(request.text[..len].to_vec()).map_err(|_| libc::EINVAL)?;
    if !valid_request_text(path.as_str()) {
        return Err(libc::EINVAL);
    }
    Ok(path)
}

pub(super) fn validate_runtime_request(request: &RuntimeRequest) -> Result<(), i32> {
    if request.reserved0 != 0 {
        return Err(libc::EINVAL);
    }
    let text_len = usize::try_from(request.text_len).map_err(|_| libc::EINVAL)?;
    if text_len > request.text.len() {
        return Err(libc::EINVAL);
    }
    match request.op {
        OP_SNAPSHOT_RUNNING_PROGRAMS => {
            if request.target_kind != 0 || request.target_value != 0 || text_len != 0 {
                return Err(libc::EINVAL);
            }
        }
        OP_NOTIFY_READY => {
            if request.target_kind != READY_COMPONENT_UI_SERVER
                || request.target_value != 0
                || text_len != 0
            {
                return Err(libc::EINVAL);
            }
        }
        OP_REQUEST_TERMINATE => {
            if !matches!(
                request.target_kind,
                TERMINATE_TARGET_SESSION | TERMINATE_TARGET_PID
            ) || request.target_value == 0
                || text_len != 0
            {
                return Err(libc::EINVAL);
            }
        }
        OP_REQUEST_LAUNCH_PATH => {
            if request.target_kind != LAUNCH_TARGET_NEW_SESSION
                || request.target_value != 0
                || text_len == 0
            {
                return Err(libc::EINVAL);
            }
        }
        _ => return Err(libc::EINVAL),
    }
    Ok(())
}

fn valid_request_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_PATH_BYTES
        && value
            .bytes()
            .all(|byte| matches!(byte, b' '..=b'~') && byte != b'\\')
}
