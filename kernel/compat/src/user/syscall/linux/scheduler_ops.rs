use super::*;

/// Allows a user helper to surrender its inherited System-class admission and
/// elevated permanent fair share.
///
/// There is intentionally no symmetric promotion syscall: executable launch
/// policy admits the initial class, and synchronous IPC inherits a class only
/// for the lifetime of its exact reply capability.  A caller can therefore
/// reduce contention but cannot manufacture privileged scheduling priority or
/// increase a pre-existing low fair weight.
pub(super) fn syscall_linux_rustos_sched_demote_self() -> u64 {
    if multitask::demote_current_user_task_to_user_class() {
        0
    } else {
        linux_errno(LINUX_EPERM)
    }
}
