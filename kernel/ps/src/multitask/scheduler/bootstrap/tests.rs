//! Owner: one-shot bootstrap authority and effect-before-ACK native witnesses.
//! Boundary: pure owner state, not physical HAL/banks/CR3/TSS/RSP equivalence.
//! Lifecycle: CPU publications all complete before Ready; poison forbids reuse.
//! Concurrency: local cells model competing BSP claims without a directory lock.
//! Failure: stale/duplicate/prefix requests refuse before the next producer effect.
//! Forbidden: injected recovery cannot claim production restart/hardware acceptance.
//! Evidence: exact production state methods plus signed compiler/source routing.
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::vec::Vec;
fn publish(key: &ImageKey<'_>, cpu: usize, id: u64) {
    let grant = key.cpu(cpu, id).unwrap();
    grant.begin_effects();
    grant.image.cpus[cpu].owner.store(1, Ordering::Relaxed);
    grant.begin_publication(1).unwrap();
    grant.complete_publication();
}
#[test]
fn boot_image_refuses_empty_zero_generation_capacity_and_competing_bsp_without_reopening_poison() {
    let cell = ImageCell::new();
    assert!(cell.claim(&[]).is_none());
    assert!(cell.claim(&[0]).is_none());
    assert!(cell.claim(&[1; CAP + 1]).is_none());
    let key = cell.claim(&[4, 8]).unwrap();
    assert!(cell.claim(&[4, 8]).is_none());
    drop(key);
    // ORDERING: Acquire observes the test stage; injected tuple/reset stays inside this local fixture.
    assert_eq!(cell.phase.load(Ordering::Acquire), POISON);
    assert!(cell.claim(&[4, 8]).is_none());
}
#[test]
fn every_cpu_bank_ack_precedes_one_ready_release_and_incomplete_prefix_preserves_other_producer() {
    let cell = ImageCell::new();
    let mut key = cell.claim(&[1, 2, 3]).unwrap();
    publish(&key, 0, 0);
    assert!(key.seal().is_none());
    assert!(cell.entry(1, 2).is_none());
    publish(&key, 2, 22);
    assert!(key.seal().is_none());
    publish(&key, 1, 11);
    let ready = key.seal().unwrap();
    ready.assert_complete();
    drop(ready);
    assert!(key.seal().is_none());
    assert!(cell.claim(&[1, 2, 3]).is_none());
    assert!(
        cell.cpus[..3]
            .iter()
            // ORDERING: Acquire observes the test stage; injected tuple/reset stays inside this local fixture.
            .all(|c| c.phase.load(Ordering::Acquire) == PUBLISHED)
    );
}
#[test]
fn initial_producer_binds_cpu_slot_uid_generation_and_wrong_alias_refuses_before_effects() {
    let cell = ImageCell::new();
    let key = cell.claim(&[5, 9]).unwrap();
    assert!(key.cpu(0, 99).is_none());
    assert!(key.cpu(1, 0).is_none());
    assert!(key.cpu(2, 3).is_none());
    let root = key.cpu(0, 0).unwrap();
    assert_eq!((root.slot(), root.id(), root.generation()), (0, 0, 5));
    assert!(key.cpu(0, 0).is_none());
    root.begin_effects();
    cell.cpus[0].generation.store(6, Ordering::Relaxed);
    assert!(root.begin_publication(1).is_none());
    cell.cpus[0].generation.store(5, Ordering::Relaxed);
    cell.cpus[0].slot.store(4, Ordering::Relaxed);
    assert!(root.begin_publication(1).is_none());
    cell.cpus[0].slot.store(0, Ordering::Relaxed);
    cell.cpus[0].id.store(99, Ordering::Relaxed);
    assert!(root.begin_publication(1).is_none());
    cell.cpus[0].id.store(0, Ordering::Relaxed);
    assert!(root.begin_publication(0).is_none());
    root.begin_publication(7).unwrap();
    root.complete_publication();
    let ap = key.cpu(1, 11).unwrap();
    assert_eq!((ap.slot(), ap.generation()), (MAX_TASK - 1, 9));
}
#[test]
fn duplicate_publication_cannot_spend_bank_or_ack_and_abandonment_retains_poisoned_image() {
    let cell = ImageCell::new();
    let key = cell.claim(&[1]).unwrap();
    let root = key.cpu(0, 0).unwrap();
    assert!(root.begin_publication(1).is_none());
    root.begin_effects();
    root.begin_publication(1).unwrap();
    assert!(root.begin_publication(1).is_none());
    root.complete_publication();
    assert!(catch_unwind(AssertUnwindSafe(|| root.complete_publication())).is_err());
    // ORDERING: Acquire observes the test stage; injected tuple/reset stays inside this local fixture.
    assert_eq!(cell.cpus[0].phase.load(Ordering::Acquire), PUBLISHED);
    drop(root);
    drop(key);
    assert_eq!(cell.phase.load(Ordering::Acquire), POISON);
    let other = ImageCell::new();
    let key = other.claim(&[1]).unwrap();
    let grant = key.cpu(0, 0).unwrap();
    grant.begin_effects();
    drop(grant);
    // ORDERING: Acquire observes the test stage; injected tuple/reset stays inside this local fixture.
    assert_eq!(other.cpus[0].phase.load(Ordering::Acquire), CPU_POISON);
    assert_eq!(other.phase.load(Ordering::Acquire), POISON);
    assert!(key.cpu(0, 0).is_none());
}
#[test]
fn ap_entry_is_generation_exact_unique_and_poisoned_entry_never_reuses_next_cpu() {
    let cell = ImageCell::new();
    let mut key = cell.claim(&[1, 7, 8]).unwrap();
    publish(&key, 0, 0);
    publish(&key, 1, 11);
    publish(&key, 2, 22);
    key.seal().unwrap();
    assert!(cell.entry(0, 1).is_none());
    assert!(cell.entry(1, 0).is_none());
    assert!(cell.entry(1, 8).is_none());
    assert!(cell.entry(3, 8).is_none());
    let entry = cell.entry(1, 7).unwrap();
    assert!(cell.entry(1, 7).is_none());
    drop(entry);
    assert!(cell.entry(1, 7).is_none());
    // ORDERING: Acquire observes the test stage; injected tuple/reset stays inside this local fixture.
    assert_eq!(cell.cpus[1].phase.load(Ordering::Acquire), CPU_POISON);
    let entry = cell.entry(2, 8).unwrap();
    assert_eq!(entry.cpu(), 2);
    assert_eq!(entry.slot(), MAX_TASK - 2);
}
#[test]
fn concurrent_bsp_claims_have_one_winner_without_global_directory_or_callback_lock() {
    let cell = std::sync::Arc::new(ImageCell::new());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let cell = cell.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let key = cell.claim(&[1, 2]);
            let won = key.is_some();
            barrier.wait();
            drop(key);
            won
        }));
    }
    barrier.wait();
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>(),
        1
    );
}
#[test]
fn duplicate_task_uid_cannot_bind_two_physical_cpu_banks() {
    let cell = ImageCell::new();
    let key = cell.claim(&[1, 2, 3]).unwrap();
    publish(&key, 0, 0);
    publish(&key, 1, 11);
    assert!(key.cpu(2, 11).is_none());
    // ORDERING: Acquire observes the test stage; injected tuple/reset stays inside this local fixture.
    assert_eq!(cell.cpus[2].phase.load(Ordering::Acquire), 0);
    publish(&key, 2, 22);
}
