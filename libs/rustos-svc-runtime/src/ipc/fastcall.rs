//! Pointer-free short forms for the typed service helpers. Selection is made
//! BEFORE a syscall; an error or committed result never triggers a second call.
//! Other shapes use the common fastcall buffer slow mode, not a replay.

use rustos_user_abi::syscall::*;

const EOVERFLOW: i64 = 75;

fn short_call_shape(request_len: usize, reply_capacity: usize) -> bool {
    request_len != 0 && request_len <= IPC_REGISTER_BYTES && reply_capacity == IPC_REGISTER_BYTES
}

fn short_receive_shape(capacity: usize) -> bool {
    // A smaller capacity must reject without consuming a too-large request;
    // a larger capacity promises to accept messages beyond the register image.
    capacity == IPC_REGISTER_BYTES
}

unsafe fn words(pointer: *const u8, length: usize) -> Option<[u64; 2]> {
    if length > IPC_REGISTER_BYTES || (length != 0 && pointer.is_null()) {
        return None;
    }
    let bytes = if length == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(pointer, length) }
    };
    ipc_register_pack(bytes)
}

unsafe fn copy_payload(result: &IpcRegisterResult, destination: *mut u8) -> i64 {
    if result.status < 0 {
        return result.status;
    }
    let length = result.status as usize;
    if length > IPC_REGISTER_BYTES {
        return -EOVERFLOW;
    }
    let bytes = ipc_register_unpack(result.words);
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, length) };
    result.status
}

pub(super) unsafe fn call(
    endpoint: u64,
    request: *const u8,
    request_len: usize,
    reply: *mut u8,
    reply_capacity: usize,
) -> Option<i64> {
    if !short_call_shape(request_len, reply_capacity) || reply.is_null() {
        return None;
    }
    let payload = unsafe { words(request, request_len) }?;
    let result = unsafe {
        ipc_register_syscall(
            SYS_RUSTOS_IPC_REGISTER_CALL,
            endpoint,
            request_len as u64,
            payload,
            0,
        )
    };
    Some(unsafe { copy_payload(&result, reply) })
}

pub(super) unsafe fn reply(
    reply_cap: u64,
    response: *const u8,
    response_len: usize,
) -> Option<i64> {
    let payload = unsafe { words(response, response_len) }?;
    Some(
        unsafe {
            ipc_register_syscall(
                SYS_RUSTOS_IPC_REGISTER_REPLY,
                reply_cap,
                response_len as u64,
                payload,
                0,
            )
        }
        .status,
    )
}

pub(super) unsafe fn receive(
    endpoint: u64,
    request: *mut u8,
    capacity: usize,
    reply: *mut u64,
    pid: *mut u64,
    tid: *mut u64,
) -> Option<i64> {
    if !short_receive_shape(capacity)
        || request.is_null()
        || reply.is_null()
        || pid.is_null()
        || tid.is_null()
    {
        return None;
    }
    let result =
        unsafe { ipc_register_syscall(SYS_RUSTOS_IPC_REGISTER_RECV, endpoint, 0, [0; 2], 0) };
    Some(unsafe { receive_result(result, request, reply, pid, tid) })
}

pub(super) unsafe fn reply_receive(
    endpoint: u64,
    reply_cap: u64,
    response: *const u8,
    response_len: usize,
    request: *mut u8,
    capacity: usize,
    reply: *mut u64,
    pid: *mut u64,
    tid: *mut u64,
) -> Option<i64> {
    if !short_receive_shape(capacity)
        || request.is_null()
        || reply.is_null()
        || pid.is_null()
        || tid.is_null()
    {
        return None;
    }
    let payload = unsafe { words(response, response_len) }?;
    let result = unsafe {
        ipc_register_syscall(
            SYS_RUSTOS_IPC_REGISTER_REPLY_RECV,
            endpoint,
            response_len as u64,
            payload,
            reply_cap,
        )
    };
    Some(unsafe { receive_result(result, request, reply, pid, tid) })
}

unsafe fn receive_result(
    result: IpcRegisterResult,
    request: *mut u8,
    reply: *mut u64,
    pid: *mut u64,
    tid: *mut u64,
) -> i64 {
    let status = unsafe { copy_payload(&result, request) };
    // Never manufacture authority on error, even for a malformed return image.
    let metadata = if status >= 0 {
        [result.reply, result.sender_pid, result.sender_tid]
    } else {
        [0; 3]
    };
    for (pointer, value) in [reply, pid, tid].into_iter().zip(metadata) {
        unsafe { pointer.write_unaligned(value) };
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_selection_never_narrows_or_widens_advertised_receive_capacity() {
        for capacity in [0, 1, 15, 16, 17, 256, usize::MAX] {
            assert_eq!(short_receive_shape(capacity), capacity == 16);
            for length in [0, 1, 15, 16, 17, usize::MAX] {
                assert_eq!(
                    short_call_shape(length, capacity),
                    (1..=16).contains(&length) && capacity == 16
                );
            }
        }
    }

    #[test]
    fn short_receive_copies_exact_bytes_and_authenticated_metadata() {
        let mut bytes = [0xcc; 16];
        let (mut reply, mut pid, mut tid) = (0, 0, 0);
        let result = IpcRegisterResult {
            status: 3,
            words: ipc_register_pack(&[1, 2, 3]).unwrap(),
            reply: 0x1234,
            sender_pid: 42,
            sender_tid: 43,
        };
        assert_eq!(
            unsafe { receive_result(result, bytes.as_mut_ptr(), &mut reply, &mut pid, &mut tid) },
            3
        );
        assert_eq!(&bytes[..3], &[1, 2, 3]);
        assert_eq!(&bytes[3..], &[0xcc; 13]);
        assert_eq!((reply, pid, tid), (0x1234, 42, 43));
    }

    #[test]
    fn short_results_preserve_pre_and_post_commit_errors_without_retry_or_copy() {
        for status in [-1, -22, -75, -IPC_REPLY_RECV_COMMITTED_ERROR_BASE - 75, 17] {
            let mut bytes = [0xcc; 16];
            let (mut reply, mut pid, mut tid) = (99, 99, 99);
            let result = IpcRegisterResult {
                status,
                words: [u64::MAX; 2],
                reply: 99,
                sender_pid: 99,
                sender_tid: 99,
            };
            let returned = unsafe {
                receive_result(result, bytes.as_mut_ptr(), &mut reply, &mut pid, &mut tid)
            };
            assert_eq!(returned, if status == 17 { -EOVERFLOW } else { status });
            assert_eq!(bytes, [0xcc; 16]);
            assert_eq!((reply, pid, tid), (0, 0, 0));
        }
        assert_eq!(unsafe { words(core::ptr::null(), 0) }, Some([0; 2]));
        assert_eq!(unsafe { words(core::ptr::null(), 1) }, None);
    }
}
