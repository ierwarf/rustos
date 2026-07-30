//! Narrow Windows syscall-frame validation, policy handoff, and commit adapter.
//!
//! - **Owner:** kernel-compat owns frame/user-copy mechanism; syscalld owns
//!   Win32 admission policy and kernel-ps owns affinity state.
//! - **Boundary:** untrusted registers, pointers, handles, information classes,
//!   service replies, and topology stamps cross user/service/kernel owners.
//! - **Lifecycle:** decode and validate → snapshot/stamp → policy call →
//!   response admission → final user copy or scheduler commit.
//! - **Concurrency:** scheduler snapshots and commits serialize under its raw
//!   lock; user copies retain the exact current process generation.
//! - **Failure:** malformed frames, stale stamps, foreign handles, bad service
//!   replies, or failed final revalidation return NTSTATUS without mutation.
//! - **Forbidden:** no policy fallback, raw APIC exposure, unchecked pointer,
//!   implicit non-pseudo handle, or partial process-affinity update.
//! - **Evidence:** `cpu-affinity-observation`, `task-affinity-lifecycle`, and
//!   focused dispatch/ABI differential tests.

// RING3-MIGRATION-REFERENCE START: decode exception: syscalld owns Win32
// syscall policy. Ring0 keeps syscall frame validation, current-process
// user-copy, and dispatch substrate.
use core::mem::size_of;
use core::slice;

use rustos_user_abi::syscall::{
    CPU_TOPOLOGY_OBSERVATION_ABI_VERSION, SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_CLOSE, SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION,
    SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS, SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE,
    SYSCALL_OFFLOAD_OP_WIN32_GET_CURRENT_PROCESSOR_NUMBER,
    SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_PROCESS_AFFINITY,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_SYSTEM_INFORMATION,
    SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY, SYSCALL_OFFLOAD_OP_WIN32_READ_FILE,
    SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE, SYSCALL_OFFLOAD_OP_WIN32_SET_PROCESS_AFFINITY,
    SYSCALL_OFFLOAD_OP_WIN32_SET_THREAD_AFFINITY, SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE,
    WIN32_SYSCALL_OFFLOAD_ABI_VERSION, WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK,
    WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT, Win32SyscallOffloadRequest,
    Win32SyscallOffloadResponse,
};
use rustos_user_abi::windows::{
    ERROR_INSUFFICIENT_BUFFER as WIN32_ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_FUNCTION as WIN32_ERROR_INVALID_FUNCTION,
    ERROR_INVALID_HANDLE as WIN32_ERROR_INVALID_HANDLE,
    ERROR_INVALID_LEVEL as WIN32_ERROR_INVALID_LEVEL,
    ERROR_INVALID_PARAMETER as WIN32_ERROR_INVALID_PARAMETER, STATUS_INFO_LENGTH_MISMATCH,
    STATUS_INVALID_HANDLE, STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER,
    STATUS_INVALID_SYSTEM_SERVICE, WindowsSystemBasicInformation,
};

use super::super::SyscallFrame;
use super::Api;
use crate::user::sysops::usermem;

pub(crate) fn dispatch_syscall(frame: &mut SyscallFrame) -> u64 {
    let api = match syscall_check(frame) {
        Ok(api) => api,
        Err(error) => return error,
    };
    let Some(current) = crate::multitask::current_user_snapshot() else {
        // A Win32 syscall is never valid without its owning user task.  Do
        // not send a forged pid/tid/session triple to syscalld.
        return STATUS_INVALID_PARAMETER;
    };
    let mut request = Win32SyscallOffloadRequest {
        version: WIN32_SYSCALL_OFFLOAD_ABI_VERSION,
        op: win32_offload_op(api),
        arg0: frame.rdi,
        arg1: frame.rsi,
        arg2: frame.rdx,
        arg3: frame.r8,
        arg4: frame.r9,
        arg5: 0,
        pid: current.process_id(),
        tid: current.thread_id(),
        session_handle: current.console_session().raw(),
        ..Win32SyscallOffloadRequest::default()
    };
    if matches!(
        api,
        Api::NtQuerySystemInformation
            | Api::RustosQueryProcessAffinity
            | Api::RustosSetProcessAffinity
            | Api::RustosSetThreadAffinity
            | Api::RustosGetCurrentProcessorNumber
    ) {
        let online_mask = kernel_hal::api::cpu::admitted_online_mask();
        stamp_windows_topology(&mut request, online_mask);
        if api == Api::RustosQueryProcessAffinity {
            let snapshot = match crate::multitask::windows_process_affinity(online_mask) {
                Ok(snapshot) => snapshot,
                Err(error) => return affinity_error_to_ntstatus(error),
            };
            assert_eq!(
                snapshot.system_mask, online_mask,
                "SMP invariant: Windows affinity snapshot changed topology owner"
            );
            request.arg3 = snapshot.process_mask;
        }
    }
    match super::super::linux::call_syscalld_raw(as_bytes(&request)) {
        Ok(bytes) if bytes.len() == size_of::<Win32SyscallOffloadResponse>() => {
            let response = read_unaligned::<Win32SyscallOffloadResponse>(bytes.as_slice());
            if response.version != WIN32_SYSCALL_OFFLOAD_ABI_VERSION
                || response.op != request.op
                || response.reserved0 != 0
            {
                return STATUS_INVALID_SYSTEM_SERVICE;
            }
            if response.status == 0 {
                apply_win32_mechanism(api, &request, response.result)
            } else {
                policy_status_to_ntstatus(response.status)
            }
        }
        _ => STATUS_INVALID_SYSTEM_SERVICE,
    }
}

fn policy_status_to_ntstatus(status: u32) -> u64 {
    match status {
        WIN32_ERROR_INVALID_FUNCTION => STATUS_INVALID_SYSTEM_SERVICE,
        WIN32_ERROR_INSUFFICIENT_BUFFER => STATUS_INFO_LENGTH_MISMATCH,
        WIN32_ERROR_INVALID_LEVEL => STATUS_INVALID_INFO_CLASS,
        WIN32_ERROR_INVALID_HANDLE => STATUS_INVALID_HANDLE,
        WIN32_ERROR_INVALID_PARAMETER => STATUS_INVALID_PARAMETER,
        _ => STATUS_INVALID_PARAMETER,
    }
}

fn win32_offload_op(api: Api) -> u16 {
    match api {
        Api::NtWriteFile => SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE,
        Api::NtReadFile => SYSCALL_OFFLOAD_OP_WIN32_READ_FILE,
        Api::NtDelayExecution => SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION,
        Api::NtClose => SYSCALL_OFFLOAD_OP_WIN32_CLOSE,
        Api::NtGetConsoleMode => SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE,
        Api::NtSetConsoleMode => SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE,
        Api::RtlExitUserProcess => SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS,
        Api::NtAllocateVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY,
        Api::NtFreeVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY,
        Api::NtProtectVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY,
        Api::NtQueryVirtualMemory => SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY,
        Api::NtQuerySystemInformation => SYSCALL_OFFLOAD_OP_WIN32_QUERY_SYSTEM_INFORMATION,
        Api::RustosQueryProcessAffinity => SYSCALL_OFFLOAD_OP_WIN32_QUERY_PROCESS_AFFINITY,
        Api::RustosSetProcessAffinity => SYSCALL_OFFLOAD_OP_WIN32_SET_PROCESS_AFFINITY,
        Api::RustosSetThreadAffinity => SYSCALL_OFFLOAD_OP_WIN32_SET_THREAD_AFFINITY,
        Api::RustosGetCurrentProcessorNumber => {
            SYSCALL_OFFLOAD_OP_WIN32_GET_CURRENT_PROCESSOR_NUMBER
        }
    }
}

fn stamp_windows_topology(request: &mut Win32SyscallOffloadRequest, online_mask: u64) {
    assert!(
        online_mask != 0
            && online_mask
                & !((1_u64 << rustos_user_abi::syscall::CPU_TOPOLOGY_MAX_LOGICAL_CPUS) - 1)
                == 0,
        "SMP invariant: invalid Online mask at Windows topology boundary"
    );
    let online_count = u64::from(online_mask.count_ones());
    request.arg4 = online_mask;
    request.arg5 = online_count
        | (CPU_TOPOLOGY_OBSERVATION_ABI_VERSION << WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT);
}

fn apply_win32_mechanism(
    api: Api,
    request: &Win32SyscallOffloadRequest,
    policy_result: u64,
) -> u64 {
    match api {
        Api::NtQuerySystemInformation => apply_query_system_information(request),
        Api::RustosQueryProcessAffinity => apply_query_process_affinity(request),
        Api::RustosSetProcessAffinity => {
            let online_mask = request.arg4;
            match crate::multitask::set_windows_process_affinity(request.arg1, online_mask) {
                Ok(_) => 1,
                Err(error) => affinity_error_to_ntstatus(error),
            }
        }
        Api::RustosSetThreadAffinity => {
            let online_mask = request.arg4;
            match crate::multitask::set_windows_current_thread_affinity(request.arg1, online_mask) {
                Ok(commit) => commit.previous_mask,
                Err(error) => affinity_error_to_ntstatus(error),
            }
        }
        Api::RustosGetCurrentProcessorNumber => current_processor_number(
            request.arg4,
            nucleus_core::util::lockdep::current_cpu_index(),
        ),
        _ => policy_result,
    }
}

fn current_processor_number(online_mask: u64, logical_index: usize) -> u64 {
    let bit = 1_u64
        .checked_shl(u32::try_from(logical_index).expect("logical CPU index overflow"))
        .expect("logical CPU index exceeds affinity mask");
    assert!(
        online_mask & bit != 0,
        "SMP invariant: Windows observed execution on a CPU outside Online"
    );
    logical_index as u64
}

fn apply_query_system_information(request: &Win32SyscallOffloadRequest) -> u64 {
    let online_count = (request.arg5 & WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK) as u8;
    let information = WindowsSystemBasicInformation::from_online_count(online_count);
    if usermem::validate_current_user_write_buffer(
        request.arg1,
        WindowsSystemBasicInformation::BYTES,
    )
    .is_err()
        || request.arg3 != 0
            && usermem::validate_current_user_write_buffer(request.arg3, size_of::<u32>()).is_err()
    {
        return STATUS_INVALID_PARAMETER;
    }
    if usermem::write_current_user_bytes(request.arg1, as_bytes(&information)).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    if request.arg3 != 0
        && usermem::write_current_user_bytes(
            request.arg3,
            &(WindowsSystemBasicInformation::BYTES as u32).to_le_bytes(),
        )
        .is_err()
    {
        return STATUS_INVALID_PARAMETER;
    }
    0
}

fn apply_query_process_affinity(request: &Win32SyscallOffloadRequest) -> u64 {
    if usermem::validate_current_user_write_buffer(request.arg1, size_of::<u64>()).is_err()
        || usermem::validate_current_user_write_buffer(request.arg2, size_of::<u64>()).is_err()
    {
        return STATUS_INVALID_PARAMETER;
    }
    if usermem::write_current_user_bytes(request.arg1, &request.arg3.to_le_bytes()).is_err()
        || usermem::write_current_user_bytes(request.arg2, &request.arg4.to_le_bytes()).is_err()
    {
        return STATUS_INVALID_PARAMETER;
    }
    1
}

fn affinity_error_to_ntstatus(error: crate::multitask::AffinityError) -> u64 {
    match error {
        crate::multitask::AffinityError::InvalidMask => STATUS_INVALID_PARAMETER,
        crate::multitask::AffinityError::MissingTask
        | crate::multitask::AffinityError::PermissionDenied
        | crate::multitask::AffinityError::WrongAbi => STATUS_INVALID_HANDLE,
    }
}

fn as_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: `value` is live and immutably borrowed for the returned slice;
    // the slice covers exactly its initialized object representation.
    unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= size_of::<T>());
    // SAFETY: the length assertion proves a complete `T` representation is
    // readable and `read_unaligned` imposes no alignment precondition.
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

fn syscall_check(frame: &SyscallFrame) -> Result<Api, u64> {
    let Some(api) = Api::from_syscall_number(frame.rax) else {
        return Err(super::super::SYSCALL_ERR_INVALID);
    };
    if !super::super::syscall_frame_security_check(frame) {
        super::super::validate_syscall_entry_or_terminate(frame);
    }
    Ok(api)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_topology_stamp_is_versioned_exact_and_reserved_zero() {
        let mut request = Win32SyscallOffloadRequest::default();
        stamp_windows_topology(&mut request, 0b1111);
        assert_eq!(request.arg4, 0b1111);
        assert_eq!(request.arg5 & WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK, 4);
        assert_eq!(
            request.arg5 >> WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT,
            CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
        );
    }

    #[test]
    fn windows_current_processor_number_is_exact_and_online_bounded() {
        assert_eq!(current_processor_number(0b1111, 0), 0);
        assert_eq!(current_processor_number(0b1111, 3), 3);
    }

    #[test]
    #[should_panic(expected = "outside Online")]
    fn windows_current_processor_number_panics_on_unpublished_cpu() {
        let _ = current_processor_number(0b0011, 3);
    }
}
// RING3-MIGRATION-REFERENCE END: syscalld-owned Win32 syscall dispatch decode exception.
