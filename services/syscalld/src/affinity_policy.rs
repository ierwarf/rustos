//! Pure admission for kernel-stamped logical-CPU observations and mutations.
//!
//! - **Owner:** syscalld owns Linux/Windows affinity and topology policy;
//!   ring0 owns CPU and scheduler mechanism.
//! - **Boundary:** kernel-stamped Online/task/process masks, caller/target
//!   identities, pseudo handles, user pointers, and ABI versions cross owners.
//! - **Lifecycle:** validate the complete stamp and target shape, admit one
//!   observation or mutation, then let ring0 revalidate and commit.
//! - **Concurrency:** this module is pure; ring0 serializes topology snapshots
//!   and scheduler mutation and never trusts service-returned topology.
//! - **Failure:** stale, empty, oversized, count-mismatched, foreign-owner,
//!   invalid-handle, or reserved-bearing requests fail without mutation.
//! - **Forbidden:** no fabricated CPU zero, mask widening, raw APIC identity,
//!   implicit current handle, or partial Windows process update.
//! - **Evidence:** `cpu-affinity-observation`,
//!   `task-affinity-lifecycle`, and the focused tests below.

use rustos_user_abi::syscall::{
    LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, Win32SyscallOffloadRequest,
    CPU_TOPOLOGY_MAX_LOGICAL_CPUS, CPU_TOPOLOGY_OBSERVATION_ABI_VERSION, LINUX_CPUSET_BYTES,
    WIN32_SYSTEM_INFORMATION_CLASS_BASIC, WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK,
    WIN32_TOPOLOGY_OBSERVATION_RESERVED_SHIFT, WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT,
};
use rustos_user_abi::windows::WindowsSystemBasicInformation;
use rustos_user_abi::windows::{HANDLE_CURRENT_PROCESS, HANDLE_CURRENT_THREAD};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsTopologyError {
    InvalidClass,
    InvalidPointerOrStamp,
    BufferTooSmall,
}

pub fn admit_online_mask(request: &LinuxSyscallOffloadRequest) -> Option<u64> {
    let online_mask = request.arg0;
    let online_count = request.arg1;
    let valid_mask_bits = (1_u64 << CPU_TOPOLOGY_MAX_LOGICAL_CPUS) - 1;
    (request.arg2 == CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
        && request.pid != 0
        && request.arg3 == request.pid
        && online_mask != 0
        && online_mask & !valid_mask_bits == 0
        && online_count != 0
        && online_count <= CPU_TOPOLOGY_MAX_LOGICAL_CPUS
        && online_count == u64::from(online_mask.count_ones()))
    .then_some(online_mask)
}

pub fn handle_sched_getaffinity(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    if request.flags < LINUX_CPUSET_BYTES as u64 {
        response.status = crate::errno::EINVAL;
        return;
    }
    let Some((_online_mask, task_mask)) = admit_linux_task_mask(request) else {
        response.status = crate::errno::EINVAL;
        return;
    };
    response.payload.fill(0);
    response.payload[..LINUX_CPUSET_BYTES].copy_from_slice(&task_mask.to_le_bytes());
    response.status = 0;
    response.payload_len = LINUX_CPUSET_BYTES as u32;
}

pub fn handle_sched_setaffinity(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    if request.flags < LINUX_CPUSET_BYTES as u64 || admit_linux_task_mask(request).is_none() {
        response.status = crate::errno::EINVAL;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

pub fn admit_linux_task_mask(request: &LinuxSyscallOffloadRequest) -> Option<(u64, u64)> {
    let online_mask = admit_online_mask(request)?;
    let task_mask = u64::from(request.mask);
    (task_mask != 0 && task_mask & !online_mask == 0).then_some((online_mask, task_mask))
}

pub fn admit_windows_online_mask(request: &Win32SyscallOffloadRequest) -> Option<(u64, u8)> {
    let online_mask = request.arg4;
    let packed = request.arg5;
    let online_count = packed & WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK;
    let version = (packed >> WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT)
        & WIN32_TOPOLOGY_OBSERVATION_FIELD_MASK;
    let reserved = packed >> WIN32_TOPOLOGY_OBSERVATION_RESERVED_SHIFT;
    let valid_mask_bits = (1_u64 << CPU_TOPOLOGY_MAX_LOGICAL_CPUS) - 1;
    (version == CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
        && reserved == 0
        && online_mask != 0
        && online_mask & !valid_mask_bits == 0
        && online_count != 0
        && online_count <= CPU_TOPOLOGY_MAX_LOGICAL_CPUS
        && online_count == u64::from(online_mask.count_ones()))
    .then_some((online_mask, online_count as u8))
}

pub fn admit_windows_basic_information(
    request: &Win32SyscallOffloadRequest,
) -> Result<(u64, u8), WindowsTopologyError> {
    if request.arg0 != WIN32_SYSTEM_INFORMATION_CLASS_BASIC {
        return Err(WindowsTopologyError::InvalidClass);
    }
    if request.arg1 == 0 {
        return Err(WindowsTopologyError::InvalidPointerOrStamp);
    }
    if request.arg2 < WindowsSystemBasicInformation::BYTES as u64 {
        return Err(WindowsTopologyError::BufferTooSmall);
    }
    admit_windows_online_mask(request).ok_or(WindowsTopologyError::InvalidPointerOrStamp)
}

pub fn admit_windows_query_process_affinity(
    request: &Win32SyscallOffloadRequest,
) -> Option<(u64, u64)> {
    let (online_mask, _) = admit_windows_online_mask(request)?;
    let process_mask = request.arg3;
    (request.arg0 == HANDLE_CURRENT_PROCESS
        && request.arg1 != 0
        && request.arg2 != 0
        && process_mask != 0
        && process_mask & !online_mask == 0)
        .then_some((process_mask, online_mask))
}

pub fn admit_windows_set_process_affinity(request: &Win32SyscallOffloadRequest) -> Option<u64> {
    let (online_mask, _) = admit_windows_online_mask(request)?;
    (request.arg0 == HANDLE_CURRENT_PROCESS
        && request.arg1 != 0
        && request.arg1 & !online_mask == 0)
        .then_some(request.arg1)
}

pub fn admit_windows_set_thread_affinity(request: &Win32SyscallOffloadRequest) -> Option<u64> {
    let (online_mask, _) = admit_windows_online_mask(request)?;
    (request.arg0 == HANDLE_CURRENT_THREAD && request.arg1 != 0 && request.arg1 & !online_mask == 0)
        .then_some(request.arg1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mask: u64, count: u64) -> LinuxSyscallOffloadRequest {
        LinuxSyscallOffloadRequest {
            pid: 42,
            arg0: mask,
            arg1: count,
            arg2: CPU_TOPOLOGY_OBSERVATION_ABI_VERSION,
            arg3: 42,
            mask: mask as u32,
            ..LinuxSyscallOffloadRequest::default()
        }
    }

    #[test]
    fn sched_getaffinity_returns_exact_kernel_stamped_task_mask() {
        assert_eq!(admit_online_mask(&request(0b1111, 4)), Some(0b1111));
        assert_eq!(
            admit_linux_task_mask(&LinuxSyscallOffloadRequest {
                mask: 0b0101,
                ..request(0b1111, 4)
            }),
            Some((0b1111, 0b0101))
        );
    }

    #[test]
    fn sched_getaffinity_rejects_invalid_topology_observations() {
        let base = request(0b11, 2);
        for invalid in [
            LinuxSyscallOffloadRequest { arg2: 0, ..base },
            request(0, 0),
            request(0b11, 1),
            LinuxSyscallOffloadRequest { arg3: 41, ..base },
            request(1 << CPU_TOPOLOGY_MAX_LOGICAL_CPUS, 1),
        ] {
            assert_eq!(admit_online_mask(&invalid), None);
        }
        for invalid in [
            LinuxSyscallOffloadRequest { mask: 0, ..base },
            LinuxSyscallOffloadRequest {
                mask: 0b100,
                ..base
            },
        ] {
            assert_eq!(admit_linux_task_mask(&invalid), None);
        }
    }

    #[test]
    fn windows_basic_system_information_uses_exact_kernel_topology_stamp() {
        let request = Win32SyscallOffloadRequest {
            arg4: 0b1111,
            arg5: 4
                | (CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
                    << WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT),
            ..Win32SyscallOffloadRequest::default()
        };
        assert_eq!(admit_windows_online_mask(&request), Some((0b1111, 4)));
    }

    #[test]
    fn windows_topology_rejects_stale_count_mismatch_and_reserved_bits() {
        let base = Win32SyscallOffloadRequest {
            arg4: 0b11,
            arg5: 2
                | (CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
                    << WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT),
            ..Win32SyscallOffloadRequest::default()
        };
        for invalid in [
            Win32SyscallOffloadRequest { arg4: 0, ..base },
            Win32SyscallOffloadRequest { arg5: 1, ..base },
            Win32SyscallOffloadRequest {
                arg5: base.arg5 | (1 << WIN32_TOPOLOGY_OBSERVATION_RESERVED_SHIFT),
                ..base
            },
        ] {
            assert_eq!(admit_windows_online_mask(&invalid), None);
        }
    }

    #[test]
    fn windows_basic_information_rejects_class_pointer_and_length_before_publish() {
        let base = Win32SyscallOffloadRequest {
            arg1: 0x8000_0000_00,
            arg2: WindowsSystemBasicInformation::BYTES as u64,
            arg4: 0b11,
            arg5: 2
                | (CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
                    << WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT),
            ..Win32SyscallOffloadRequest::default()
        };
        assert_eq!(admit_windows_basic_information(&base), Ok((0b11, 2)));
        assert_eq!(
            admit_windows_basic_information(&Win32SyscallOffloadRequest { arg0: 1, ..base }),
            Err(WindowsTopologyError::InvalidClass)
        );
        assert_eq!(
            admit_windows_basic_information(&Win32SyscallOffloadRequest { arg1: 0, ..base }),
            Err(WindowsTopologyError::InvalidPointerOrStamp)
        );
        assert_eq!(
            admit_windows_basic_information(&Win32SyscallOffloadRequest {
                arg2: WindowsSystemBasicInformation::BYTES as u64 - 1,
                ..base
            }),
            Err(WindowsTopologyError::BufferTooSmall)
        );
    }

    fn windows_affinity_request(handle: u64, mask: u64) -> Win32SyscallOffloadRequest {
        Win32SyscallOffloadRequest {
            arg0: handle,
            arg1: mask,
            arg4: 0b1111,
            arg5: 4
                | (CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
                    << WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT),
            ..Win32SyscallOffloadRequest::default()
        }
    }

    #[test]
    fn windows_affinity_admission_is_handle_exact_and_online_bounded() {
        assert_eq!(
            admit_windows_set_process_affinity(&windows_affinity_request(
                HANDLE_CURRENT_PROCESS,
                0b0101
            )),
            Some(0b0101)
        );
        assert_eq!(
            admit_windows_set_thread_affinity(&windows_affinity_request(
                HANDLE_CURRENT_THREAD,
                0b0010
            )),
            Some(0b0010)
        );
        for invalid in [
            windows_affinity_request(0, 0b1),
            windows_affinity_request(HANDLE_CURRENT_PROCESS, 0),
            windows_affinity_request(HANDLE_CURRENT_PROCESS, 0b1_0000),
        ] {
            assert_eq!(admit_windows_set_process_affinity(&invalid), None);
        }
    }

    #[test]
    fn windows_process_affinity_query_binds_both_output_pointers_and_process_mask() {
        let request = Win32SyscallOffloadRequest {
            arg0: HANDLE_CURRENT_PROCESS,
            arg1: 0x1000,
            arg2: 0x2000,
            arg3: 0b0101,
            arg4: 0b1111,
            arg5: 4
                | (CPU_TOPOLOGY_OBSERVATION_ABI_VERSION
                    << WIN32_TOPOLOGY_OBSERVATION_VERSION_SHIFT),
            ..Win32SyscallOffloadRequest::default()
        };
        assert_eq!(
            admit_windows_query_process_affinity(&request),
            Some((0b0101, 0b1111))
        );
        assert_eq!(
            admit_windows_query_process_affinity(&Win32SyscallOffloadRequest {
                arg3: 0b1_0000,
                ..request
            }),
            None
        );
    }
}
