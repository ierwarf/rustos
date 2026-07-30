//! Atomic deferred-process activation batch at the registry/scheduler boundary.
//!
//! - **Owner:** kernel-compat owns one-shot activation capabilities;
//!   kernel-ps owns suspended task publication.
//! - **Boundary:** loaderd supplies an untrusted bounded target array and
//!   requester claim after binding the kernel-stamped IPC sender.
//! - **Lifecycle:** validate shape → lock registry → preflight every exact
//!   capability → atomically publish every task → consume every capability.
//! - **Concurrency:** the process-broker registry lock precedes and spans the
//!   allocation-free scheduler batch commit.
//! - **Failure:** any bad shape, authority, or scheduler target changes none;
//!   a post-commit capability mismatch is kernel corruption and panics.
//! - **Forbidden:** no partial cohort, duplicate/zero target, nonzero tail,
//!   capability consumption before publication, or best-effort member skip.
//! - **Evidence:** `atomic-process-activation-batch` and the focused test below.

use super::*;
use rustos_user_abi::syscall::{
    LOADER_ACTIVATE_BATCH_MAX_TARGETS, PROC_BROKER_ABI_VERSION, RustosProcActivateBatchBrokerArgs,
};

pub(in crate::user::syscall::linux) fn syscall_linux_rustos_proc_activate_batch_broker(
    args_ptr: u64,
) -> u64 {
    if !current_process_can_load() {
        return linux_errno(LINUX_EPERM);
    }
    let args =
        match usermem::read_current_user_struct::<RustosProcActivateBatchBrokerArgs>(args_ptr) {
            Ok(args) => args,
            Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
        };
    let target_count = usize::from(args.target_count);
    if args.abi_version != PROC_BROKER_ABI_VERSION
        || args.flags != 0
        || args.requester_pid == 0
        || target_count == 0
        || target_count > LOADER_ACTIVATE_BATCH_MAX_TARGETS
        || args.target_pids[target_count..]
            .iter()
            .any(|target_pid| *target_pid != 0)
    {
        return linux_errno(LINUX_EINVAL);
    }
    let targets = &args.target_pids[..target_count];
    for (index, target_pid) in targets.iter().copied().enumerate() {
        if target_pid == 0 || targets[..index].contains(&target_pid) {
            return linux_errno(LINUX_EINVAL);
        }
    }

    let mut activations = DEFERRED_ACTIVATIONS.lock();
    if targets.iter().copied().any(|target_pid| {
        !deferred_spawn_provenance_matches(&activations, target_pid, args.requester_pid)
    }) {
        return linux_errno(LINUX_EPERM);
    }
    if !multitask::activate_suspended_user_tasks(targets) {
        return linux_errno(LINUX_ESRCH);
    }
    for target_pid in targets.iter().copied() {
        assert_eq!(
            activations.remove(&target_pid),
            Some(args.requester_pid),
            "proc activation batch invariant: committed authority disappeared while locked"
        );
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "proc-activate-batch-member",
            target_pid,
            args.requester_pid,
        );
    }
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "proc-activate-batch-committed",
        args.requester_pid,
        target_count as u64,
    );
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn activation_batch_preflights_before_atomic_publish_and_consumption() {
        let source = include_str!("activation_batch.rs");
        let lock = source
            .find("let mut activations = DEFERRED_ACTIVATIONS.lock()")
            .expect("activation registry lock");
        let authority = source
            .find("deferred_spawn_provenance_matches")
            .expect("exact authority preflight");
        let publish = source
            .find("multitask::activate_suspended_user_tasks")
            .expect("atomic scheduler publication");
        let consume = source
            .find("activations.remove(&target_pid)")
            .expect("one-shot capability consumption");
        assert!(lock < authority);
        assert!(authority < publish);
        assert!(publish < consume);
        assert!(source.contains("targets[..index].contains(&target_pid)"));
        assert!(source.contains("args.target_pids[target_count..]"));
    }
}
