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
    matches!(syscall_number, linux_abi::SYS_RUSTOS_NET_BROKER)
}

pub(super) fn dispatch_linux_rustos_broker_syscall(frame: &SyscallFrame) -> u64 {
    match frame.rax {
        linux_abi::SYS_RUSTOS_NET_BROKER => syscall_linux_rustos_net_broker(frame.rdi),
        _ => linux_errno(LINUX_ENOSYS),
    }
}
