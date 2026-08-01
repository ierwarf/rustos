//! Static post-root launch ordering and latency boundaries.
//!
//! The runtimed/uiserver closure is immutable early-image content and does
//! not depend on DVM storage. Its exact child therefore crosses an activation
//! boundary before initd prepares storaged, allowing both independent paths
//! to overlap without weakening either endpoint-admission barrier.

use super::{
    DEVMGRD_EXEC_PATH, INPUTD_EXEC_PATH, LOADERD_EXEC_PATH, NETD_EXEC_PATH, RUNTIMED_EXEC_PATH,
    STORAGED_EXEC_PATH, SYSCALLD_EXEC_PATH, VFSD_EXEC_PATH,
};

pub(super) fn init_exec_priority(exec: &str) -> u8 {
    match exec {
        SYSCALLD_EXEC_PATH => 0,
        VFSD_EXEC_PATH => 1,
        LOADERD_EXEC_PATH => 2,
        NETD_EXEC_PATH => 3,
        DEVMGRD_EXEC_PATH => 4,
        INPUTD_EXEC_PATH => 5,
        RUNTIMED_EXEC_PATH => 6,
        STORAGED_EXEC_PATH => 7,
        _ => 8,
    }
}

pub(super) fn requires_immediate_activation_after_spawn(exec: &str) -> bool {
    exec == RUNTIMED_EXEC_PATH
}

#[cfg(test)]
mod tests {
    use super::{init_exec_priority, requires_immediate_activation_after_spawn};
    use crate::{
        bootstrap_barrier::RUNTIMED_BOOTSTRAP_SERVICES, RUNTIMED_EXEC_PATH, STORAGED_EXEC_PATH,
    };
    use rustos_user_abi::syscall::IPC_SERVICE_STORAGED;

    #[test]
    fn runtimed_bootstrap_does_not_wait_for_storage_dvm_publication() {
        assert!(!RUNTIMED_BOOTSTRAP_SERVICES.contains(&IPC_SERVICE_STORAGED));
        assert!(init_exec_priority(RUNTIMED_EXEC_PATH) < init_exec_priority(STORAGED_EXEC_PATH));
        assert!(requires_immediate_activation_after_spawn(
            RUNTIMED_EXEC_PATH
        ));
        assert!(!requires_immediate_activation_after_spawn(
            STORAGED_EXEC_PATH
        ));
    }
}
