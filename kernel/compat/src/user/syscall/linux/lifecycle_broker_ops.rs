use super::*;

use rustos_user_abi::syscall::{
    LifecycleDrainBrokerArgs, IPC_SERVICE_CAP_PROCESS_POLICY, IPC_SERVICE_CAP_ROOT_SUPERVISOR,
};

pub(super) fn syscall_linux_rustos_lifecycle_drain_broker(args_ptr: u64) -> u64 {
    let is_process_policy =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_PROCESS_POLICY);
    let is_root_supervisor =
        ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_ROOT_SUPERVISOR);
    if !is_process_policy && !is_root_supervisor {
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
