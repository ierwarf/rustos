// RING3-MIGRATION-REFERENCE START: rootd service capability policy should own
// broker dispatch admission. Ring0 keeps the syscall demux and capability-gated
// broker entry substrate.
use super::*;

#[path = "device_broker_ops.rs"]
mod device_broker_ops;
#[path = "early_system_broker_ops.rs"]
mod early_system_broker_ops;
#[path = "entropy_broker_ops.rs"]
mod entropy_broker_ops;
#[path = "input_broker_ops.rs"]
mod input_broker_ops;
#[path = "lifecycle_broker_ops.rs"]
mod lifecycle_broker_ops;
#[path = "net_broker_ops.rs"]
mod net_broker_ops;
#[path = "waitset_broker_ops.rs"]
pub(crate) mod waitset_broker_ops;

use device_broker_ops::*;
use early_system_broker_ops::*;
use entropy_broker_ops::*;
use input_broker_ops::*;
use lifecycle_broker_ops::*;
use net_broker_ops::*;
use waitset_broker_ops::*;

pub(super) use device_broker_ops::device_sysop_error_to_linux_errno;

pub(super) fn is_linux_rustos_broker_syscall(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_RUSTOS_NET_BROKER
            | linux_abi::SYS_RUSTOS_INPUT_STATS_BROKER
            | linux_abi::SYS_RUSTOS_INPUT_INGEST_BROKER
            | linux_abi::SYS_RUSTOS_INPUT_WAIT_BROKER
            | linux_abi::SYS_RUSTOS_DEVICE_OPEN_BROKER
            | linux_abi::SYS_RUSTOS_DEVICE_IOCTL_BROKER
            | linux_abi::SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER
            | linux_abi::SYS_RUSTOS_ROOTD_WAIT_BROKER
            | linux_abi::SYS_RUSTOS_ROOTD_TERMINATE_BROKER
            | linux_abi::SYS_RUSTOS_WAITSET_SIGNAL_BROKER
            | linux_abi::SYS_RUSTOS_ENTROPY_BROKER
            | linux_abi::SYS_RUSTOS_EARLY_SYSTEM_BROKER
    )
}

pub(super) fn dispatch_linux_rustos_broker_syscall(frame: &SyscallFrame) -> u64 {
    match frame.rax {
        linux_abi::SYS_RUSTOS_NET_BROKER => syscall_linux_rustos_net_broker(frame.rdi),
        linux_abi::SYS_RUSTOS_INPUT_STATS_BROKER => {
            syscall_linux_rustos_input_stats_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_INPUT_INGEST_BROKER => {
            syscall_linux_rustos_input_ingest_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_INPUT_WAIT_BROKER => syscall_linux_rustos_input_wait_broker(),
        linux_abi::SYS_RUSTOS_DEVICE_OPEN_BROKER => {
            syscall_linux_rustos_device_open_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_DEVICE_IOCTL_BROKER => {
            syscall_linux_rustos_device_ioctl_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER => {
            syscall_linux_rustos_lifecycle_drain_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_ROOTD_WAIT_BROKER => {
            syscall_linux_rustos_rootd_wait_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_ROOTD_TERMINATE_BROKER => {
            syscall_linux_rustos_rootd_terminate_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_WAITSET_SIGNAL_BROKER => {
            syscall_linux_rustos_waitset_signal_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_ENTROPY_BROKER => {
            syscall_linux_rustos_entropy_broker(frame.rdi, frame.rsi)
        }
        linux_abi::SYS_RUSTOS_EARLY_SYSTEM_BROKER => {
            syscall_linux_rustos_early_system_broker(frame.rdi)
        }
        _ => linux_errno(LINUX_ENOSYS),
    }
}
// RING3-MIGRATION-REFERENCE END: rootd-owned broker dispatch capability policy.
