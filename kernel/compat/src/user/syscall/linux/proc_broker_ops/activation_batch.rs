//! Atomic deferred-process activation batch at the registry/scheduler boundary.
//!
//! - **Owner:** kernel-compat owns one-shot activation capabilities;
//!   kernel-ps owns suspended task publication.
//! - **Boundary:** loaderd supplies an untrusted bounded target array and
//!   requester claim after binding the kernel-stamped IPC sender.
//! - **Lifecycle:** validate shape → snapshot every exact live identity → lock
//!   registry → preflight every exact capability and scheduler target → consume
//!   every capability while every target remains suspended → atomically publish
//!   every task.
//! - **Concurrency:** process-table identity snapshots complete before the
//!   process-broker registry lock, which then precedes and spans the
//!   allocation-free scheduler batch commit.
//! - **Failure:** any bad shape, authority, or scheduler target changes none;
//!   a post-commit capability mismatch is kernel corruption and panics.
//! - **Forbidden:** no partial cohort, duplicate/zero target, nonzero tail,
//!   authority consumption outside the completed-preflight critical section,
//!   runnable publication before every capability is consumed, or best-effort
//!   member skip.
//! - **Evidence:** `atomic-process-activation-batch` and the focused test below.

use super::*;
use rustos_user_abi::syscall::{
    LOADER_ACTIVATE_BATCH_MAX_TARGETS, PROC_BROKER_ABI_VERSION, RustosProcActivateBatchBrokerArgs,
};

const fn deferred_authority_is_batch_eligible(qualification_required: bool) -> bool {
    !qualification_required
}

fn deferred_authority_matches_exact_batch_request<I: Eq>(
    authority_owner: &I,
    authority_target: &I,
    qualification_required: bool,
    owner: &I,
    exact_target: &I,
) -> bool {
    authority_owner == owner
        && authority_target == exact_target
        && deferred_authority_is_batch_eligible(qualification_required)
}

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

    let Some(owner) = multitask::live_user_process_identity_by_pid(args.requester_pid) else {
        return linux_errno(LINUX_ESRCH);
    };
    let mut exact_targets = [None; LOADER_ACTIVATE_BATCH_MAX_TARGETS];
    for (index, target_pid) in targets.iter().copied().enumerate() {
        let Some(target) = multitask::live_user_process_identity_by_pid(target_pid) else {
            return linux_errno(LINUX_ESRCH);
        };
        exact_targets[index] = Some(target);
    }
    let exact_targets = &exact_targets[..target_count];

    let mut activations = DEFERRED_ACTIVATIONS.lock();
    if targets
        .iter()
        .copied()
        .zip(exact_targets.iter().copied())
        .any(|(target_pid, exact_target)| {
            let exact_target = exact_target
                .expect("proc activation batch invariant: validated target identity disappeared");
            activations.get(&target_pid).is_none_or(|authority| {
                !deferred_authority_matches_exact_batch_request(
                    &authority.owner,
                    &authority.target,
                    authority.qualification_required,
                    &owner,
                    &exact_target,
                )
            })
        })
    {
        return linux_errno(LINUX_EPERM);
    }
    if !multitask::activate_suspended_user_tasks_with_commit(targets, || {
        for (target_pid, exact_target) in targets.iter().copied().zip(exact_targets.iter().copied())
        {
            let exact_target = exact_target
                .expect("proc activation batch invariant: validated target identity disappeared");
            let authority = activations.remove(&target_pid).expect(
                "proc activation batch invariant: preflighted authority disappeared while locked",
            );
            assert_eq!(authority.owner, owner);
            assert_eq!(authority.target, exact_target);
            assert!(deferred_authority_matches_exact_batch_request(
                &authority.owner,
                &authority.target,
                authority.qualification_required,
                &owner,
                &exact_target,
            ));
        }
    }) {
        return linux_errno(LINUX_ESRCH);
    }
    drop(activations);
    for target_pid in targets.iter().copied() {
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
    use super::{
        deferred_authority_is_batch_eligible, deferred_authority_matches_exact_batch_request,
    };

    #[test]
    fn private_qualification_authority_is_never_batch_eligible() {
        assert!(deferred_authority_is_batch_eligible(false));
        assert!(!deferred_authority_is_batch_eligible(true));
    }

    #[test]
    fn exact_batch_authority_rejects_pid_equal_generation_or_mm_replacement() {
        let owner = (7_u64, 11_u32, 13_u32);
        let target = (42_u64, 17_u32, 19_u32);

        assert!(deferred_authority_matches_exact_batch_request(
            &owner, &target, false, &owner, &target,
        ));
        assert!(!deferred_authority_matches_exact_batch_request(
            &owner,
            &(42, 18, 19),
            false,
            &owner,
            &target,
        ));
        assert!(!deferred_authority_matches_exact_batch_request(
            &owner,
            &(42, 17, 20),
            false,
            &owner,
            &target,
        ));
    }

    #[test]
    fn activation_batch_keeps_preflight_and_commit_under_registry_lock() {
        let source = include_str!("activation_batch.rs");
        let lock = source
            .find("let mut activations = DEFERRED_ACTIVATIONS.lock()")
            .expect("activation registry lock");
        let authority = lock
            + source[lock..]
                .find("activations.get(&target_pid)")
                .expect("generation-bound authority preflight under registry lock");
        let publish = source
            .find("multitask::activate_suspended_user_tasks_with_commit")
            .expect("atomic scheduler transaction");
        let unlock = source
            .find("drop(activations)")
            .expect("capability registry release");
        let evidence = source
            .find("nucleus_core::debug::record_milestone")
            .expect("post-transaction evidence");
        assert!(lock < authority);
        assert!(authority < publish);
        assert!(publish < unlock);
        assert!(unlock < evidence);
        assert!(source.contains("activations.remove(&target_pid)"));
        assert!(source.contains("preflighted authority disappeared while locked"));
        assert!(source.contains("targets[..index].contains(&target_pid)"));
        assert!(source.contains("args.target_pids[target_count..]"));
        assert!(source[..lock].contains("live_user_process_identity_by_pid"));
        assert!(!source[lock..unlock].contains("live_user_process_identity_by_pid"));
    }
}
