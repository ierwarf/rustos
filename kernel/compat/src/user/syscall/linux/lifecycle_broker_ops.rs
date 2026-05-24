use super::*;

use rustos_user_abi::syscall::{LifecycleDrainBrokerArgs, IPC_SERVICE_CAP_PROCESS_POLICY};

pub(super) fn syscall_linux_rustos_lifecycle_drain_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_POLICY) {
        return linux_errno(LINUX_EPERM);
    }
    let args = match usermem::read_current_user_struct::<LifecycleDrainBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    match offload_ops::drain_lifecycle_events(&args) {
        Ok(count) => count,
        Err(errno) => linux_errno(errno),
    }
}
