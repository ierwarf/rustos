//! Untimed service fastcall-buffer witnesses. Data, deadlines, metadata and
//! physical FP state must survive the real large/mixed/fused transport; native
//! helper tests alone do not prove the linked syscall path.

use rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS;
use rustos_user_abi::syscall::*;

fn checked(entry: u64, block: &IpcRegisterBufferArgs) -> IpcRegisterResult {
    checked_pointer(entry, block as *const _ as u64)
}

fn checked_pointer(entry: u64, pointer: u64) -> IpcRegisterResult {
    let (result, fp_ok) = unsafe { crate::ipc_simd::checked_buffer_syscall(entry, pointer) };
    assert!(
        fp_ok,
        "service buffer fastcall changed FP controls or leaked data"
    );
    assert_eq!(
        (
            result.words,
            result.reply,
            result.sender_pid,
            result.sender_tid
        ),
        ([0; 2], 0, 0, 0)
    );
    result
}

pub(crate) fn preflight() -> bool {
    // Copy faults and a forged non-IPC operation must fail before authority,
    // without weakening the fastcall sanitization/control-state obligation.
    let forged = IpcRegisterBufferArgs {
        operation: 39,
        args: [0; 6],
    };
    if checked(SYS_RUSTOS_IPC_REGISTER_CALL, &forged).status != -22 {
        return false;
    }
    // Null is outside RustOS's user range: the existing user-copy contract
    // returns EINVAL, not EFAULT. A mapped, unreadable page tests EFAULT.
    if checked_pointer(SYS_RUSTOS_IPC_REGISTER_CALL, 0).status != -22 {
        return false;
    }
    // SAFETY: Request a private inaccessible page without dereferencing it.
    let guard = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if guard == libc::MAP_FAILED {
        return false;
    }
    let fault = checked_pointer(SYS_RUSTOS_IPC_REGISTER_CALL, guard as u64);
    // SAFETY: Release exactly the successful mapping after the syscall ends.
    let released = unsafe { libc::munmap(guard, 4096) };
    if fault.status != -14 || released != 0 {
        return false;
    }
    let endpoint = unsafe { crate::syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if endpoint <= 0 {
        return false;
    }
    let pid = unsafe { crate::syscall0(39) } as u64;
    let tid = unsafe { crate::syscall0(186) } as u64;
    let endpoint = endpoint as u64;
    let server = std::thread::spawn(move || serve(endpoint, pid, tid));
    let mut valid = true;
    for (index, length) in [16usize, 32].into_iter().enumerate() {
        let request = [0xa0 + index as u8; 32];
        let mut response = [0xcc; 64];
        let block = IpcRegisterBufferArgs {
            operation: SYS_RUSTOS_IPC_CALL_BOUNDED,
            args: [
                endpoint,
                request.as_ptr() as u64,
                length as u64,
                response.as_mut_ptr() as u64,
                response.len() as u64,
                IPC_BOOT_CONTROL_HARD_LIMIT_MS,
            ],
        };
        let result = checked(SYS_RUSTOS_IPC_REGISTER_CALL, &block);
        valid &= result.status == length as i64
            && response[..length] == request[..length]
            && response[length..].iter().all(|&byte| byte == 0xcc);
    }
    let joined = server.join().is_ok_and(|result| result);
    valid && joined
}

fn serve(endpoint: u64, pid: u64, tid: u64) -> bool {
    let mut request = [0; 64];
    let (mut cap, mut sender_pid, mut sender_tid) = (0u64, 0u64, 0u64);
    let args = IpcRecvWithSenderArgs {
        endpoint,
        request_ptr: request.as_mut_ptr() as u64,
        request_capacity: request.len() as u64,
        reply_cap_ptr: (&mut cap as *mut u64) as u64,
        sender_pid_ptr: (&mut sender_pid as *mut u64) as u64,
        sender_tid_ptr: (&mut sender_tid as *mut u64) as u64,
    };
    let first = IpcRegisterBufferArgs {
        operation: SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
        args: [
            &args as *const _ as u64,
            IPC_BOOT_CONTROL_HARD_LIMIT_MS,
            0,
            0,
            0,
            0,
        ],
    };
    let mut result = checked(SYS_RUSTOS_IPC_REGISTER_RECV, &first);
    for (index, length) in [16usize, 32].into_iter().enumerate() {
        if result.status != length as i64
            || cap == 0
            || sender_pid != pid
            || sender_tid != tid
            || request[..length]
                .iter()
                .any(|&byte| byte != 0xa0 + index as u8)
        {
            return false;
        }
        let response = request;
        if index == 1 {
            let block = IpcRegisterBufferArgs {
                operation: SYS_RUSTOS_IPC_REPLY,
                args: [cap, response.as_ptr() as u64, length as u64, 0, 0, 0],
            };
            return checked(SYS_RUSTOS_IPC_REGISTER_REPLY, &block).status == 0;
        }
        let fused = IpcReplyRecvWithSenderArgs {
            abi_version: IPC_ABI_VERSION,
            endpoint,
            reply_cap: cap,
            response_ptr: response.as_ptr() as u64,
            response_len: length as u64,
            request_ptr: request.as_mut_ptr() as u64,
            request_capacity: request.len() as u64,
            next_reply_cap_ptr: (&mut cap as *mut u64) as u64,
            sender_pid_ptr: (&mut sender_pid as *mut u64) as u64,
            sender_tid_ptr: (&mut sender_tid as *mut u64) as u64,
            ..IpcReplyRecvWithSenderArgs::default()
        };
        let block = IpcRegisterBufferArgs {
            operation: SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
            args: [&fused as *const _ as u64, 0, 0, 0, 0, 0],
        };
        result = checked(SYS_RUSTOS_IPC_REGISTER_REPLY_RECV, &block);
    }
    false
}
