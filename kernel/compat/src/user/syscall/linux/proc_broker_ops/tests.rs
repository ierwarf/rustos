use super::*;

#[test]
fn broker_authority_identity_exhaustion_never_wraps() {
    let counter = AtomicU64::new(u64::MAX);
    assert_eq!(allocate_nonwrapping_broker_identity(&counter), None);
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn file_mapping_len_must_fit_inside_memory_mapping() {
    assert_eq!(validate_file_mapping_len(4096, 4096), Ok(()));
    assert_eq!(validate_file_mapping_len(4096, 0), Ok(()));
    assert_eq!(validate_file_mapping_len(4096, 4097), Err(LINUX_EINVAL));
}

#[test]
fn truncated_file_mapping_never_commits_zero_filled_tail() {
    assert_eq!(validate_complete_file_copy(4096, 4096), Ok(()));
    assert_eq!(validate_complete_file_copy(4096, 4095), Err(LINUX_EIO));
    assert_eq!(validate_complete_file_copy(1, 0), Err(LINUX_EIO));
}

#[test]
fn executable_file_backing_requires_a_terminally_sealed_snapshot() {
    let snapshot = MemfdHandle::new(String::from("loader-test"), true);
    assert!(!executable_snapshot_is_immutable(&snapshot));
    snapshot
        .add_seals(
            (linux_abi::F_SEAL_WRITE | linux_abi::F_SEAL_GROW | linux_abi::F_SEAL_SHRINK) as u32,
        )
        .expect("partial seals");
    assert!(!executable_snapshot_is_immutable(&snapshot));
    snapshot
        .add_seals(linux_abi::F_SEAL_SEAL as u32)
        .expect("terminal seal");
    assert!(executable_snapshot_is_immutable(&snapshot));
}

#[test]
fn exited_prepare_owner_cannot_republish_after_cleanup() {
    assert_eq!(proc_prepare_publication_status(true, 0), Err(LINUX_ESRCH));
    assert_eq!(
        proc_prepare_publication_status(false, MAX_PROC_PREPARES),
        Err(LINUX_EAGAIN)
    );
    assert_eq!(
        proc_prepare_publication_status(false, MAX_PROC_PREPARES - 1),
        Ok(())
    );
}

#[test]
fn scheduling_context_grant_is_rootd_epoch_bound_and_terminally_consumed() {
    let source = include_str!("scheduling_context_grants.rs");
    assert!(source.contains("struct SchedulingContextGrant"));
    assert!(source.contains("rootd: multitask::ProcessIdentity"));
    assert!(source.contains("requester: multitask::ProcessIdentity"));
    assert!(source.contains("rootd_epoch: u64"));
    assert!(source.contains("grant.exec_path != exec_path"));
    assert!(source.contains("grant.policy != authority.policy"));
    assert!(source.contains(".remove(&authority.token)"));
    assert!(source.contains("consume_direct_bootstrap"));
    assert!(source.contains("if matched.next().is_some()"));
    assert!(source.contains("let grant = grants.remove(&token)"));
}

#[test]
fn deferred_activation_authority_is_exact_one_shot_and_nontransferable() {
    let source = include_str!("../proc_broker_ops.rs");
    assert!(source.contains("struct DeferredActivationAuthority"));
    assert!(source.contains("owner: multitask::ProcessIdentity"));
    assert!(source.contains("target: multitask::ProcessIdentity"));
    assert!(source.contains("deferred_spawn_authority_matches"));
    assert!(source.contains("deferred_activation_identities_match"));
    let legacy_pid_only = ["type DeferredActivationRegistry = FnvIndexMap<u64", ", u64"].concat();
    assert!(!source.contains(legacy_pid_only.as_str()));
}

#[test]
fn single_activation_resolves_claimed_requester_identity_not_loaderd_context() {
    const LOADERD_PID: u64 = 7;
    const REQUESTER_PID: u64 = 11;
    const TARGET_PID: u64 = 13;
    let mut looked_up = [0_u64; 2];
    let mut lookup_count = 0_usize;
    let identities = resolve_deferred_activation_identities(REQUESTER_PID, TARGET_PID, |pid| {
        assert_ne!(
            pid, LOADERD_PID,
            "loaderd context is not requester authority"
        );
        looked_up[lookup_count] = pid;
        lookup_count += 1;
        match pid {
            REQUESTER_PID => Some((REQUESTER_PID, 3_u32, 5_u32)),
            TARGET_PID => Some((TARGET_PID, 7_u32, 11_u32)),
            _ => None,
        }
    })
    .expect("live requester and target identities");
    assert_eq!(looked_up, [REQUESTER_PID, TARGET_PID]);
    assert_eq!(identities.0, (REQUESTER_PID, 3, 5));
    assert_eq!(identities.1, (TARGET_PID, 7, 11));
    let mut target_lookup_attempted = false;
    assert_eq!(
        resolve_deferred_activation_identities(REQUESTER_PID, TARGET_PID, |pid| {
            if pid == TARGET_PID {
                target_lookup_attempted = true;
                Some(TARGET_PID)
            } else {
                None
            }
        }),
        Err(LINUX_ESRCH),
        "no live requester must fail closed before target authority is considered"
    );
    assert!(!target_lookup_attempted);

    let source = include_str!("../proc_broker_ops.rs");
    let activation = source
        .split("pub(super) fn syscall_linux_rustos_proc_activate_broker")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub(super) fn syscall_linux_rustos_proc_validate_deferred_spawn_broker")
                .next()
        })
        .expect("single activate broker");
    let service_gate = activation
        .find("if !current_process_can_load()")
        .expect("loaderd capability gate");
    let requester_identity = activation
        .find("resolve_deferred_activation_identities(")
        .expect("claimed requester identity resolution");
    let authority_match = activation
        .find("deferred_spawn_authority_matches(&activations, args.target_pid, owner, target)")
        .expect("exact owner and target authority match");
    assert!(service_gate < requester_identity);
    assert!(requester_identity < authority_match);
    assert!(!activation.contains("current_user_process_identity"));
    let resolver_args = &activation[requester_identity..authority_match];
    let requester_arg = resolver_args
        .find("args.requester_pid")
        .expect("requester PID passed to resolver");
    let target_arg = resolver_args
        .find("args.target_pid")
        .expect("target PID passed to resolver");
    assert!(requester_arg < target_arg);

    let validation = source
        .split("pub(super) fn syscall_linux_rustos_proc_validate_deferred_spawn_broker")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub(super) fn syscall_linux_rustos_proc_authorize_exec_broker")
                .next()
        })
        .expect("deferred spawn validation broker");
    assert!(validation.contains("live_user_process_identity_by_pid(args.requester_pid)"));

    let batch = include_str!("../proc_broker_ops/activation_batch.rs");
    assert!(batch.contains("live_user_process_identity_by_pid(args.requester_pid)"));
}

#[test]
fn deferred_activation_identity_matcher_rejects_stale_generation() {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Identity {
        pid: u64,
        process_generation: u32,
        mm_generation: u32,
    }

    let owner = Identity {
        pid: 11,
        process_generation: 3,
        mm_generation: 5,
    };
    let target = Identity {
        pid: 13,
        process_generation: 7,
        mm_generation: 11,
    };
    assert!(deferred_activation_identities_match(
        &owner, &target, &owner, &target
    ));
    let stale_owner = Identity {
        process_generation: owner.process_generation + 1,
        ..owner
    };
    assert!(!deferred_activation_identities_match(
        &owner,
        &target,
        &stale_owner,
        &target,
    ));
}

#[test]
fn loader_commit_revalidates_live_requester_role_before_consuming_authority() {
    let source = include_str!("../proc_broker_ops.rs");
    let spawn_commit = source
        .split("pub(super) fn syscall_linux_rustos_proc_commit_broker")
        .nth(1)
        .and_then(|rest| {
            rest.split("pub(super) fn syscall_linux_rustos_proc_activate_broker")
                .next()
        })
        .expect("spawn commit broker");
    let spawn_role = spawn_commit
        .find("requester_owns_live_spawn_role(args.requester_pid)")
        .expect("live spawn role recheck");
    let prepare_consume = spawn_commit
        .find("let state = {")
        .expect("prepare authority consumption");
    assert!(spawn_role < prepare_consume);

    let exec_commit = source
        .split("pub(super) fn syscall_linux_rustos_proc_exec_target_broker")
        .nth(1)
        .and_then(|rest| rest.split("fn exec_transition_from_prepared").next())
        .expect("exec target broker");
    let procd_role = exec_commit
        .find("process_owns_live_service_endpoint(args.requester_pid, IPC_SERVICE_PROCD)")
        .expect("live procd role recheck");
    let ticket_consume = exec_commit
        .find("let mut tickets = EXEC_TICKETS.lock()")
        .expect("exec ticket consumption");
    assert!(procd_role < ticket_consume);
}
