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

fn validate_activation_batch_args(args: &RustosProcActivateBatchBrokerArgs) -> LinuxResult<&[u64]> {
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
        return Err(LINUX_EINVAL);
    }
    let targets = &args.target_pids[..target_count];
    for (index, target_pid) in targets.iter().copied().enumerate() {
        if target_pid == 0 || targets[..index].contains(&target_pid) {
            return Err(LINUX_EINVAL);
        }
    }
    Ok(targets)
}

pub(in crate::user::syscall::linux) fn syscall_linux_rustos_proc_activate_batch_broker(
    args_ptr: u64,
) -> u64 {
    linux_unit_result((|| {
        if !current_process_can_load() {
            return Err(LINUX_EPERM);
        }
        let args: RustosProcActivateBatchBrokerArgs = read_user_struct_as(args_ptr)?;
        let targets = validate_activation_batch_args(&args)?;

        let owner =
            multitask::live_user_process_identity_by_pid(args.requester_pid).ok_or(LINUX_ESRCH)?;
        let mut exact_targets = [None; LOADER_ACTIVATE_BATCH_MAX_TARGETS];
        for (slot, target_pid) in exact_targets[..targets.len()]
            .iter_mut()
            .zip(targets.iter().copied())
        {
            *slot = Some(
                multitask::live_user_process_identity_by_pid(target_pid).ok_or(LINUX_ESRCH)?,
            );
        }
        let exact_targets = &exact_targets[..targets.len()];

        let mut failure = LINUX_ESRCH;
        if !multitask::activate_suspended_user_tasks_with_commit(targets, || {
            // PS already owns original Current/Cohort/frame loans. Only permission
            // transfer holds this raw leaf; no scheduler work or PID lookup does.
            let mut activations = DEFERRED_ACTIVATIONS.lock();
            let grant = deferred_activation::take_exact::<LOADER_ACTIVATE_BATCH_MAX_TARGETS, _, MAX_DEFERRED_ACTIVATIONS>(
                &mut activations, targets, |pid, authority| {
                    let index = targets.iter().position(|target| *target == pid).unwrap();
                    let exact_target = exact_targets[index].expect("validated target identity disappeared");
                    deferred_authority_matches_exact_batch_request(
                        &authority.owner, &authority.target, authority.qualification_required,
                        &owner, &exact_target,
                    )
                },
            );
            if grant.is_none() { failure = LINUX_EPERM; }
            drop(activations);
            grant
        }) {
            return Err(failure);
        }
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
            targets.len() as u64,
        );
        Ok(())
    })())
}

#[cfg(test)]
mod tests {
    use super::{
        deferred_authority_is_batch_eligible, deferred_authority_matches_exact_batch_request,
        validate_activation_batch_args, LINUX_EINVAL, LOADER_ACTIVATE_BATCH_MAX_TARGETS,
        PROC_BROKER_ABI_VERSION, RustosProcActivateBatchBrokerArgs,
    };

    #[test]
    fn validate_activation_batch_args_rejects_malformed_shapes_and_admits_unique_cohort() {
        let mut args = RustosProcActivateBatchBrokerArgs {
            abi_version: PROC_BROKER_ABI_VERSION,
            target_count: 2,
            flags: 0,
            requester_pid: 7,
            target_pids: [11, 13, 0, 0, 0, 0, 0, 0],
        };
        assert_eq!(validate_activation_batch_args(&args), Ok(&[11_u64, 13][..]));

        args.abi_version = PROC_BROKER_ABI_VERSION - 1;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
        args.abi_version = PROC_BROKER_ABI_VERSION;

        args.flags = 1;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
        args.flags = 0;

        args.requester_pid = 0;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
        args.requester_pid = 7;

        args.target_count = 0;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
        args.target_count = (LOADER_ACTIVATE_BATCH_MAX_TARGETS + 1) as u16;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
        args.target_count = 2;

        // Nonzero tail outside target_count must be rejected.
        args.target_pids[2] = 17;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
        args.target_pids[2] = 0;

        // Zero PID inside prefix must be rejected.
        args.target_pids[1] = 0;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));

        // Duplicate PID inside prefix must be rejected.
        args.target_pids[1] = 11;
        assert_eq!(validate_activation_batch_args(&args), Err(LINUX_EINVAL));
    }

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
                .find("deferred_activation::take_exact::<")
                .expect("all-or-none original grant transfer under registry lock");
        let publish = source
            .find("multitask::activate_suspended_user_tasks_with_commit")
            .expect("atomic scheduler transaction");
        let unlock = source
            .find("drop(activations)")
            .expect("capability registry release");
        let evidence = source
            .find("nucleus_core::debug::record_milestone")
            .expect("post-transaction evidence");
        assert!(publish < lock, "no raw registry guard may enter scheduler preflight");
        assert!(lock < authority);
        assert!(authority < unlock);
        assert!(unlock < evidence);
        assert!(source[unlock..evidence].contains("grant"), "transfer original grant to PS, not a scalar success");
        assert!(source.contains("deferred_authority_matches_exact_batch_request("));
        assert!(source.contains("targets[..index].contains(&target_pid)"));
        assert!(source.contains("args.target_pids[target_count..]"));
        assert!(source[..lock].contains("live_user_process_identity_by_pid"));
        assert!(!source[lock..unlock].contains("live_user_process_identity_by_pid"));
    }
}
