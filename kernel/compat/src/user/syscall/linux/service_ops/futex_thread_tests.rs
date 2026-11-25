use super::*;

#[test]
fn pthread_clone_plan_is_local_and_rejects_nonthread_or_bad_tls_flags() {
    let required = linux_abi::CLONE_VM | linux_abi::CLONE_FS
        | linux_abi::CLONE_FILES | linux_abi::CLONE_SIGHAND | linux_abi::CLONE_THREAD;
    let stack = paging::USER_SPACE_BASE + 0x10000;
    let tls = paging::USER_SPACE_BASE + 0x20000;
    assert!(valid_thread_plan_locally(required | linux_abi::CLONE_SETTLS, stack, tls));
    assert!(!valid_thread_plan_locally(required & !linux_abi::CLONE_THREAD, stack, tls));
    assert!(!valid_thread_plan_locally(required, 0, tls));
    assert!(!valid_thread_plan_locally(required | 1, stack, tls),
        "thread clone cannot supply a nonzero exit signal");
}

#[test]
fn futex_return_advances_retired_thread_cleanup_before_userspace_join_resumes() {
    let source = include_str!("futex_thread.rs");
    let dispatch = source
        .split("pub fn futex_impl")
        .nth(1)
        .and_then(|rest| rest.split("fn validate_futex_policy_locally").next())
        .expect("futex dispatch");
    let runtime_cleanup = dispatch
        .find("service_retired_task_runtime_cleanup(RETIRED_TASK_CLEANUP_BUDGET)")
        .expect("bounded retired-thread runtime cleanup");
    let scheduler_reap = dispatch
        .find("multitask::service_deferred_work()")
        .expect("scheduler retirement progress");
    let userspace_return = dispatch
        .find("match result")
        .expect("userspace result publication");
    assert!(runtime_cleanup < scheduler_reap);
    assert!(scheduler_reap < userspace_return);
    assert!(
        dispatch.contains("process_owns_any_published_service_endpoint"),
        "published service owners must skip blocking retirement cleanup inside futex"
    );
}

#[test]
fn futex_keys_preserve_private_generation_and_shared_backing_identity() {
    let shared_alias_a = FutexKey::Shared {
        backing: multitask::SharedFutexBackingKey::Memfd {
            object_id: 7,
            byte_offset: 0x1234,
        },
    };
    let shared_alias_b = FutexKey::Shared {
        backing: multitask::SharedFutexBackingKey::Memfd {
            object_id: 7,
            byte_offset: 0x1234,
        },
    };
    let remapped_object = FutexKey::Shared {
        backing: multitask::SharedFutexBackingKey::Memfd {
            object_id: 8,
            byte_offset: 0x1234,
        },
    };
    assert_eq!(shared_alias_a, shared_alias_b);
    assert_ne!(shared_alias_a, remapped_object);

    let private_old = FutexKey::Private {
        mm_generation: 9,
        address_space_root: 0x1000,
        uaddr: 0x2000,
    };
    let private_reused_root = FutexKey::Private {
        mm_generation: 10,
        address_space_root: 0x1000,
        uaddr: 0x2000,
    };
    assert_ne!(private_old, private_reused_root);
}

#[test]
fn kernel_generated_wake_uses_shared_then_exact_private_fallback() {
    let private = FutexKey::Private {
        mm_generation: 11,
        address_space_root: 0x1000,
        uaddr: 0x2000,
    };
    let shared = FutexKey::Shared {
        backing: multitask::SharedFutexBackingKey::Memfd {
            object_id: 7,
            byte_offset: 0x1234,
        },
    };
    assert_eq!(
        kernel_generated_futex_key_candidates(private, Some(shared)),
        [Some(shared), Some(private)]
    );
    assert_eq!(
        kernel_generated_futex_key_candidates(private, None),
        [Some(private), None]
    );
}

#[test]
fn task_identity_cleanup_removes_a_requeued_waiter() {
    let original = FutexKey::Private {
        mm_generation: 1,
        address_space_root: 0x1000,
        uaddr: 0x2000,
    };
    let requeued = FutexKey::Private {
        mm_generation: 1,
        address_space_root: 0x1000,
        uaddr: 0x3000,
    };
    let mut waiters = [
        Some(FutexWaiter {
            key: requeued,
            task_id: 7,
            bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
        }),
        Some(FutexWaiter {
            key: original,
            task_id: 8,
            bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
        }),
    ];

    assert!(take_futex_waiter_from(&mut waiters, 7));
    assert!(waiters[0].is_none());
    assert_eq!(waiters[1].unwrap().task_id, 8);
}

#[test]
fn supported_futex_admission_is_local_and_complete() {
    for op in [
        linux_abi::FUTEX_WAIT,
        linux_abi::FUTEX_WAKE,
        linux_abi::FUTEX_REQUEUE,
        linux_abi::FUTEX_CMP_REQUEUE,
        linux_abi::FUTEX_WAIT | linux_abi::FUTEX_PRIVATE_FLAG,
        linux_abi::FUTEX_WAIT_BITSET | linux_abi::FUTEX_CLOCK_REALTIME,
        linux_abi::FUTEX_WAKE_BITSET | linux_abi::FUTEX_PRIVATE_FLAG,
    ] {
        assert_eq!(
            validate_futex_policy_locally(op, linux_abi::FUTEX_BITSET_MATCH_ANY as u64),
            Ok(())
        );
    }
    assert_eq!(
        validate_futex_policy_locally(linux_abi::FUTEX_WAIT_BITSET, 0),
        Err(LINUX_EINVAL)
    );
    assert_eq!(
        validate_futex_policy_locally(u64::MAX, linux_abi::FUTEX_BITSET_MATCH_ANY as u64),
        Err(LINUX_ENOSYS)
    );
}

#[test]
fn retired_task_cleanup_is_exact_and_idempotent() {
    let task_id = u64::MAX - 501;
    register_futex_waiter(FutexWaiter {
        key: FutexKey::Private {
            mm_generation: 1,
            address_space_root: 0x4000,
            uaddr: 0x5000,
        },
        task_id,
        bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
    })
    .expect("register retired-task waiter");

    assert!(cleanup_retired_task_waiter(task_id));
    assert!(!cleanup_retired_task_waiter(task_id));
}

#[test]
fn robust_owner_death_preserves_waiters_and_rejects_foreign_owner() {
    let task_id = 77_u64;
    assert_eq!(
        robust_owner_death_value(FUTEX_WAITERS_BIT | task_id as u32, task_id),
        Some((FUTEX_WAITERS_BIT | FUTEX_OWNER_DIED_BIT, true))
    );
    assert_eq!(
        robust_owner_death_value(task_id as u32, task_id),
        Some((FUTEX_OWNER_DIED_BIT, false))
    );
    assert_eq!(robust_owner_death_value(78, task_id), None);
}

#[test]
fn robust_futex_offset_is_checked_before_user_access() {
    let entry = paging::USER_SPACE_BASE + 0x100;
    assert_eq!(robust_futex_address(entry, 16), Some(entry + 16));
    assert_eq!(robust_futex_address(entry, -16), Some(entry - 16));
    assert_eq!(robust_futex_address(entry, 2), None);
    assert_eq!(robust_futex_address(u64::MAX - 3, 8), None);
    assert_eq!(robust_futex_address(paging::USER_SPACE_BASE, -4), None);
}

#[test]
fn futex_wake_count_follows_linux_int_semantics() {
    assert_eq!(futex_wake_limit(0), 1, "legacy FUTEX_WAKE with val 0 still wakes one");
    assert_eq!(futex_wake_limit(u64::from(u32::MAX)), 1, "-1 wakes one");
    assert_eq!(futex_wake_limit(0x8000_0000), 1, "INT_MIN wakes one");
    assert_eq!(futex_wake_limit(1), 1);
    assert_eq!(futex_wake_limit(7), 7);
    assert_eq!(futex_wake_limit(i32::MAX as u64), i32::MAX as usize);
    // Upper register bits are ignored: val is a 32-bit int.
    assert_eq!(futex_wake_limit(0xdead_beef_0000_0003), 3);
}

#[test]
fn futex_waiter_table_covers_every_scheduler_task_and_never_reports_ebusy() {
    assert!(FUTEX_WAITERS_CAPACITY >= multitask::MAX_SCHEDULER_TASKS);
    let key = FutexKey::Private {
        mm_generation: 1,
        address_space_root: 0x1000,
        uaddr: 0x2000,
    };
    let waiter = |task_id| FutexWaiter {
        key,
        task_id,
        bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
    };
    let mut waiters = [None; 4];
    for id in 0..4 {
        assert_eq!(register_futex_waiter_in(&mut waiters, waiter(id)), Ok(()));
    }
    assert_eq!(register_futex_waiter_in(&mut waiters, waiter(9)), Err(LINUX_ENOMEM));
    // A stale entry for the running task is replaced, not duplicated or EBUSY.
    assert_eq!(register_futex_waiter_in(&mut waiters, waiter(2)), Ok(()));
    assert_eq!(waiters.iter().flatten().filter(|w| w.task_id == 2).count(), 1);
}

#[test]
fn futex_requeue_counts_are_c_ints_from_the_timeout_register() {
    assert_eq!(futex_int_count(0), Ok(0));
    assert_eq!(futex_int_count(i32::MAX as u64), Ok(i32::MAX as usize));
    assert_eq!(futex_int_count(0xdead_beef_0000_0005), Ok(5));
    assert_eq!(futex_int_count(u64::from(u32::MAX)), Err(LINUX_EINVAL));
    assert_eq!(futex_int_count(0x8000_0000), Err(LINUX_EINVAL));
    // A set high bit in the 64-bit register is not a negative count.
    assert_eq!(futex_int_count(0x1_0000_0001), Ok(1));
}

#[test]
fn futex_op_validation_precedes_alignment_and_wake_op_validates_encoding() {
    assert_eq!(
        validate_futex_policy_locally(linux_abi::FUTEX_WAKE_BITSET, 0),
        Err(LINUX_EINVAL)
    );
    // FUTEX_WAKE_OP (5) accepts valid encoded op/cmp and rejects invalid sub-ops with ENOSYS.
    assert_eq!(validate_futex_policy_locally(5, 0), Ok(()));
    assert_eq!(validate_futex_policy_locally(5, 7 << 28), Err(LINUX_ENOSYS));
    // FUTEX_CLOCK_REALTIME is valid only with WAIT_BITSET.
    assert_eq!(
        validate_futex_policy_locally(linux_abi::FUTEX_WAIT | linux_abi::FUTEX_CLOCK_REALTIME, 1),
        Err(LINUX_ENOSYS)
    );
    assert_eq!(futex_impl(2, 5, 0, 0, 0, 0), linux_errno(LINUX_EINVAL));
}

#[test]
fn thread_spawn_refusals_have_distinct_stages_and_faithful_errnos() {
    use multitask::SpawnTaskError as E;
    // (reason, telemetry stage, Linux errno). Stage 11 names only the
    // exhausted scheduler task table; every other cause has its own stage.
    let table = [
        (E::InvalidWeightMicros, 10, LINUX_EINVAL),
        (E::NoFreeTaskSlot, 11, LINUX_EAGAIN),
        (E::MemoryQuota, 13, LINUX_ENOMEM),
        (E::ThreadStackAllocation, 14, LINUX_ENOMEM),
        (E::ProcessThreadLimit, 15, LINUX_EAGAIN),
        (E::JobThreadLimit, 16, LINUX_EAGAIN),
        (E::JobThreadMemory, 17, LINUX_ENOMEM),
        (E::ProcessExiting, 18, LINUX_EAGAIN),
        (E::ProcessExecInProgress, 19, LINUX_EAGAIN),
        (E::ThreadCreatorUnavailable, 20, LINUX_EAGAIN),
        (E::ThreadPublicationRefused, 21, LINUX_EAGAIN),
        (E::ThreadMaterializationFailed, 22, LINUX_EAGAIN),
        (E::ProcessLimit, 23, LINUX_EAGAIN),
    ];
    for (index, (reason, stage, errno)) in table.iter().copied().enumerate() {
        assert_eq!(clone_spawn_refusal_stage(reason), stage, "{reason:?}");
        assert_eq!(process_spawn_error_to_linux_errno(reason), errno, "{reason:?}");
        for (other, other_stage, _) in table.iter().copied().skip(index + 1) {
            assert_ne!(stage, other_stage, "{reason:?} and {other:?} share a stage");
        }
        // The log's detail word names the authority limit that rejected.
        match reason.rejecting_limit() {
            Some(limit) => assert_eq!(
                multitask::TASK_CAPACITY.limit_value(limit),
                match limit {
                    multitask::CapacityLimit::TaskTable => multitask::TASK_CAPACITY.total_slots() as u64,
                    multitask::CapacityLimit::ProcessThreads => multitask::TASK_CAPACITY.process_thread_ceiling() as u64,
                    multitask::CapacityLimit::RootProcesses => multitask::TASK_CAPACITY.root_process_ceiling() as u64,
                    multitask::CapacityLimit::KernelStack => multitask::TASK_CAPACITY.kernel_stack_bytes() as u64,
                }
            ),
            None => {}
        }
        // Stages already used by the earlier, non-spawn clone steps are reserved.
        assert!(![1, 2, 3, 4, 5, 6, 7, 8, 9, 12].contains(&stage));
    }
}

#[test]
fn futex_bucket_table_supports_wait_wake_and_cross_key_requeue() {
    let mut table = FutexWaiterTable::new();
    let k1 = FutexKey::Private {
        mm_generation: 1,
        address_space_root: 0x1000,
        uaddr: 0x2000,
    };
    let k2 = FutexKey::Private {
        mm_generation: 1,
        address_space_root: 0x1000,
        uaddr: 0x3000,
    };
    for tid in 10..14 {
        table
            .insert(FutexWaiter {
                key: k1,
                task_id: tid,
                bitset: linux_abi::FUTEX_BITSET_MATCH_ANY,
            })
            .expect("insert");
    }
    let mut woke = [0_u64; 4];
    let (w, r) = table.select_requeue(k1, k2, 1, 2, linux_abi::FUTEX_BITSET_MATCH_ANY, &mut woke);
    assert_eq!((w, r), (1, 2));
    let (w2, _) = table.select_requeue(k2, k2, 4, 0, linux_abi::FUTEX_BITSET_MATCH_ANY, &mut woke);
    assert_eq!(w2, 2);
    assert!(table.take_task(10));
    assert!(!table.take_task(10));
}

#[test]
fn thread_start_metadata_syscalls_need_no_service_round_trip() {
    // rseq is unsupported by definition; arch_prctl validation is a pure
    // function of (code, arg) and the user VA bound.
    assert_eq!(syscall_linux_rseq(0, 32, 0, 0), linux_errno(LINUX_ENOSYS));
    assert!(valid_arch_prctl_locally(linux_abi::ARCH_SET_FS, 0));
    assert!(valid_arch_prctl_locally(linux_abi::ARCH_SET_FS, paging::USER_SPACE_BASE));
    assert!(!valid_arch_prctl_locally(linux_abi::ARCH_SET_FS, paging::USER_SPACE_END_EXCLUSIVE));
    assert!(!valid_arch_prctl_locally(0x1234, 0));
    // A bad robust-list length is refused before any state is touched.
    assert_eq!(syscall_linux_set_robust_list(0, 8), linux_errno(LINUX_EINVAL));
}

#[test]
fn tkill_and_tgkill_reject_nonpositive_thread_and_group_ids() {
    assert_eq!(thread_directed_target_error(5, None), None);
    assert_eq!(thread_directed_target_error(5, Some(5)), None);
    assert_eq!(thread_directed_target_error(0, None), Some(LINUX_EINVAL));
    assert_eq!(thread_directed_target_error(-1, Some(5)), Some(LINUX_EINVAL));
    assert_eq!(thread_directed_target_error(5, Some(0)), Some(LINUX_EINVAL));
    assert_eq!(thread_directed_target_error(5, Some(-1)), Some(LINUX_EINVAL));
}

#[test]
fn kill_refuses_group_and_broadcast_targets_instead_of_signalling_the_caller() {
    for pid in [0_u64, (-1_i32) as u32 as u64, (-7_i32) as u32 as u64, 0x5_0000_0000] {
        assert_eq!(syscall_linux_kill(pid, 0), linux_errno(LINUX_ENOSYS), "pid {pid:#x}");
    }
}
