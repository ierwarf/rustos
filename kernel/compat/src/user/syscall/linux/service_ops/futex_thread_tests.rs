use super::*;

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
