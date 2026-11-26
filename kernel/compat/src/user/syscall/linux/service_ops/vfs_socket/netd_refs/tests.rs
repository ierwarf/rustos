use super::*;

#[test]
fn local_socket_dup_releases_only_the_last_reference_to_the_provider() {
    let mut registry = Registry::<4>::new();
    assert_eq!(registry.reserve(11, 7, 1), Ok(()));
    assert_eq!(registry.acquire(11), Ok(())); // fork/dup
    assert_eq!(registry.acquire(11), Ok(())); // SCM_RIGHTS or epoll pin
    assert_eq!(registry.release(11), Ok(false));
    assert_eq!(registry.release(11), Ok(false));
    assert!(registry.claim(Phase::Close, 0, None).is_none());
    assert_eq!(registry.release(11), Ok(true));
    assert_eq!(registry.acquire(11), Err(LINUX_EBADF));
    let work = registry.claim(Phase::Close, 0, None).unwrap();
    assert_eq!((work.token, work.epoch, work.operation), (11, 7, 1));
    assert!(registry.claim(Phase::Close, 0, None).is_none());
    registry.confirmed_close(work);
    let ack = registry.claim(Phase::Ack, 0, None).unwrap();
    registry.complete(ack);
    assert_eq!(registry.len, 0);
}

#[test]
fn final_close_and_ack_keep_initial_capacity_even_while_in_flight() {
    let mut registry = Registry::<2>::new();
    registry.reserve(11, 7, 1).unwrap();
    registry.reserve(12, 7, 2).unwrap();
    registry.release(11).unwrap();
    let close = registry.claim(Phase::Close, 0, None).unwrap();
    assert_eq!(registry.reserve(13, 7, 3), Err(LINUX_ENOSPC));
    registry.release(12).unwrap();
    registry.confirmed_close(close); // no fresh admission despite a full ledger
    let ack = registry.claim(Phase::Ack, 0, None).unwrap();
    assert_eq!(registry.reserve(13, 7, 3), Err(LINUX_ENOSPC));
    registry.retry(ack, 0); // lost ACK reply retains exactly the same slot
    assert_eq!(registry.len, 2);
    assert!(registry.claim(Phase::Ack, 0, None).is_none());
    let retry = registry.claim(Phase::Ack, retry_delay_ns(1), None).unwrap();
    assert_eq!(retry.index, ack.index);
    assert_eq!(retry.operation, ack.operation);
    registry.complete(retry);
    assert_eq!(registry.reserve(13, 7, 3), Ok(()));
}

#[test]
fn failed_close_rotates_without_blocking_a_ready_neighbour() {
    let mut registry = Registry::<4>::new();
    for token in 1..=3 {
        registry.reserve(token, 7, token).unwrap();
        registry.release(token).unwrap();
    }
    let failed = registry.claim(Phase::Close, 0, None).unwrap();
    assert_eq!(failed.token, 1);
    registry.retry(failed, 0);
    let second = registry.claim(Phase::Close, 0, None).unwrap();
    let third = registry.claim(Phase::Close, 0, None).unwrap();
    assert_eq!((second.token, third.token), (2, 3));
    assert!(registry.claim(Phase::Close, 0, None).is_none());
    let retried = registry.claim(Phase::Close, retry_delay_ns(1), None).unwrap();
    assert_eq!((retried.token, retried.operation), (failed.token, failed.operation));
    assert_eq!(retry_delay_ns(u8::MAX), 1_024_000_000);
}

#[test]
fn rejected_initial_publication_returns_its_slot_without_provider_close() {
    let mut registry = Registry::<2>::new();
    registry.reserve(11, 7, 1).unwrap();
    registry.cancel_unpublished(11, 7);
    assert!(registry.claim(Phase::Close, 0, None).is_none());
    assert!(registry.claim(Phase::Ack, 0, None).is_none());
    assert_eq!(registry.len, 0);
    assert_eq!(registry.reserve(11, 8, 2), Ok(()));
}

#[test]
fn old_provider_token_cannot_alias_a_new_publication_until_custody_finishes() {
    let mut registry = Registry::<2>::new();
    registry.reserve(11, 7, 1).unwrap();
    assert_eq!(registry.reserve(11, 8, 2), Err(LINUX_EEXIST));
    registry.release(11).unwrap();
    let work = registry.claim(Phase::Close, 0, None).unwrap();
    assert_eq!(registry.reserve(11, 8, 2), Err(LINUX_EEXIST));
    // The epoch-bound IPC helper proved that the old provider was revoked.
    registry.complete(work);
    assert_eq!(registry.reserve(11, 8, 2), Ok(()));
    assert_eq!(registry.slots[registry.find(11).unwrap()].epoch, 8);
}

#[test]
fn socket_churn_unlinks_hash_entries_and_reuses_stable_slots_without_tombstones() {
    let mut registry = Registry::<4>::new();
    for token in 1..=128 {
        registry.reserve(token, 7, token).unwrap();
        registry.cancel_unpublished(token, 7);
        assert_eq!(registry.next_unused, 1);
        assert!(registry.buckets.iter().all(|head| *head == NO_SLOT));
        assert!(registry.find(token).is_none());
    }
    // Collisions do not move the in-flight owner's slot when a neighbour is
    // removed from the middle/head of the independent bucket chain.
    let first = 1;
    let second = (2..=128).find(|token|
        Registry::<4>::bucket(*token) == Registry::<4>::bucket(first)).unwrap();
    registry.reserve(first, 7, 1).unwrap();
    registry.release(first).unwrap();
    let held = registry.claim(Phase::Close, 0, None).unwrap();
    registry.reserve(second, 7, 2).unwrap();
    registry.cancel_unpublished(second, 7);
    assert_eq!(registry.find(first), Some(held.index as usize));
    registry.confirmed_close(held);
    let ack = registry.claim(Phase::Ack, 0, None).unwrap();
    registry.complete(ack);
    assert!(registry.buckets.iter().all(|head| *head == NO_SLOT));
}

#[test]
fn initial_publication_reserves_custody_before_transfer_and_rolls_back_both_failures() {
    // Static integration witness only: no service or scheduler is executed.
    let source = include_str!("../../../net_broker_ops.rs");
    let body = source.split_once("fn broker_prepare_socket_publication(").unwrap().1
        .split_once("fn transfer_registry_error_to_linux_errno").unwrap().0;
    assert!(body.find("reserve_initial_netd_socket_ref").unwrap()
        < body.find("register_new_inet_socket_transfer").unwrap());
    assert_eq!(body.matches("cancel_initial_netd_socket_ref").count(), 2);
    assert!(body.find("reclaim_unbound_socket_transfers").unwrap()
        < body.rfind("cancel_initial_netd_socket_ref").unwrap());
}
