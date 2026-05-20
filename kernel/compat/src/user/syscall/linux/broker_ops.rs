use super::*;

#[path = "device_broker_ops.rs"]
mod device_broker_ops;
#[path = "driver_broker_ops.rs"]
mod driver_broker_ops;
#[path = "input_broker_ops.rs"]
mod input_broker_ops;
#[path = "lifecycle_broker_ops.rs"]
mod lifecycle_broker_ops;
#[path = "net_broker_ops.rs"]
mod net_broker_ops;
#[path = "storage_broker_ops.rs"]
mod storage_broker_ops;

use device_broker_ops::*;
use driver_broker_ops::*;
use input_broker_ops::*;
use lifecycle_broker_ops::*;
use net_broker_ops::*;
use storage_broker_ops::*;

pub(super) use device_broker_ops::device_sysop_error_to_linux_errno;

pub(super) fn is_linux_rustos_broker_syscall(syscall_number: u64) -> bool {
    matches!(
        syscall_number,
        linux_abi::SYS_RUSTOS_DEVICE_IOCTL_BROKER
            | linux_abi::SYS_RUSTOS_DEVICE_OPEN_BROKER
            | linux_abi::SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER
            | linux_abi::SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER
            | linux_abi::SYS_RUSTOS_DRIVER_PROVIDER_ACTIVE_BROKER
            | linux_abi::SYS_RUSTOS_NET_BROKER
            | linux_abi::SYS_RUSTOS_STORAGE_LIST_BROKER
            | linux_abi::SYS_RUSTOS_BOOT_EXTENT_BROKER
            | linux_abi::SYS_RUSTOS_INPUT_STATS_BROKER
            | linux_abi::SYS_RUSTOS_INPUT_INGEST_BROKER
            | linux_abi::SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER
    )
}

pub(super) fn dispatch_linux_rustos_broker_syscall(frame: &SyscallFrame) -> u64 {
    match frame.rax {
        linux_abi::SYS_RUSTOS_DEVICE_IOCTL_BROKER => {
            syscall_linux_rustos_device_ioctl_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_DEVICE_OPEN_BROKER => {
            syscall_linux_rustos_device_open_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER => {
            syscall_linux_rustos_driver_load_module_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER => {
            syscall_linux_rustos_driver_probe_alias_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_DRIVER_PROVIDER_ACTIVE_BROKER => {
            syscall_linux_rustos_driver_provider_active_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_NET_BROKER => syscall_linux_rustos_net_broker(frame.rdi),
        linux_abi::SYS_RUSTOS_STORAGE_LIST_BROKER => {
            syscall_linux_rustos_storage_list_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_BOOT_EXTENT_BROKER => {
            syscall_linux_rustos_boot_extent_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_INPUT_STATS_BROKER => {
            syscall_linux_rustos_input_stats_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_INPUT_INGEST_BROKER => {
            syscall_linux_rustos_input_ingest_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER => {
            syscall_linux_rustos_lifecycle_drain_broker(frame.rdi)
        }
        _ => linux_errno(LINUX_ENOSYS),
    }
}
