extern crate std;
use super::*;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());
static RELEASED: Mutex<std::vec::Vec<(u64, u64, usize)>> = Mutex::new(std::vec::Vec::new());
fn release(frame: u64, token: u64, group: usize) {
    RELEASED.lock().unwrap().push((frame, token, group));
}
fn owner() -> FrameSourceOwner {
    FrameSourceOwner {
        service_slot: 7,
        service_generation: 11,
    }
}
fn key() -> FrameSourceKey {
    FrameSourceKey {
        namespace: 2,
        object: 3,
        object_generation: 4,
        content_generation: 5,
        page_offset: 4096,
    }
}
#[test]
fn immutable_sources_never_authorize_source_writes() {
    let table = Table::<1>::new(release);
    for rights in [0, 2, 3, 4, 6, 7, 8, u64::MAX] {
        assert_eq!(
            table.publish(owner(), key(), rights, 0, |_| panic!(
                "invalid rights allocated"
            )),
            Err(FrameSourceError::Invalid)
        );
    }
}

fn publish<const N: usize>(table: &Table<N>, frame: u64) -> u64 {
    table
        .publish(
            owner(),
            key(),
            frame_capability::FRAME_RIGHT_READ | frame_capability::FRAME_RIGHT_EXECUTE,
            9,
            |_| Ok(frame),
        )
        .unwrap()
}

#[test]
fn withdrawal_rejects_new_borrows_but_existing_installer_keeps_custody() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<1>::new(release);
    let token = publish(&table, 0x1000);
    let pin = table
        .pin(token, owner(), key(), frame_capability::FRAME_RIGHT_READ)
        .unwrap();
    table.retire(token, owner()).unwrap();
    assert!(table.pin_raw(token).is_err());
    assert!(RELEASED.lock().unwrap().is_empty());
    assert_eq!(pin.source.frame, 0x1000);
    drop(pin);
    assert!(
        RELEASED.lock().unwrap().is_empty(),
        "Drop must only enqueue"
    );
    assert_eq!(table.reclaim(1), 1);
    assert_eq!(*RELEASED.lock().unwrap(), [(0x1000, token, 9)]);
    assert!(table.retire(token, owner()).is_err());
    let second = publish(&table, 0x2000);
    assert_ne!(token, second);
    assert!(table.pin_raw(token).is_err());
    table.retire(second, owner()).unwrap();
    assert_eq!(table.reclaim(1), 1);
    assert_eq!(RELEASED.lock().unwrap().len(), 2);
}

#[test]
fn every_identity_component_and_rights_are_bound() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<2>::new(release);
    let token = publish(&table, 0x1000);
    for wrong in [
        FrameSourceOwner {
            service_slot: 8,
            ..owner()
        },
        FrameSourceOwner {
            service_generation: 12,
            ..owner()
        },
    ] {
        assert!(matches!(
            table.pin(token, wrong, key(), 1),
            Err(FrameSourceError::Authority)
        ));
        assert_eq!(table.retire(token, wrong), Err(FrameSourceError::Authority));
    }
    for wrong in [
        FrameSourceKey {
            namespace: 3,
            ..key()
        },
        FrameSourceKey { object: 4, ..key() },
        FrameSourceKey {
            object_generation: 5,
            ..key()
        },
        FrameSourceKey {
            content_generation: 6,
            ..key()
        },
        FrameSourceKey {
            page_offset: 8192,
            ..key()
        },
    ] {
        assert!(matches!(
            table.pin(token, owner(), wrong, 1),
            Err(FrameSourceError::Mismatch)
        ));
    }
    assert!(matches!(
        table.pin(token, owner(), key(), frame_capability::FRAME_RIGHT_WRITE),
        Err(FrameSourceError::Mismatch)
    ));
    assert!(matches!(
        table.pin(token, owner(), key(), 0),
        Err(FrameSourceError::Mismatch)
    ));
    assert!(RELEASED.lock().unwrap().is_empty());
    table.retire(token, owner()).unwrap();
}

#[test]
fn exhausted_table_never_calls_allocator_and_failed_supply_can_retry() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<1>::new(release);
    assert_eq!(
        table.publish(owner(), key(), 1, 9, |_| Err(FrameSourceError::Quota)),
        Err(FrameSourceError::Quota)
    );
    assert!(RELEASED.lock().unwrap().is_empty());
    let token = publish(&table, 0x1000);
    assert_eq!(
        table.publish(owner(), key(), 1, 9, |_| panic!(
            "capacity must refuse before allocation"
        )),
        Err(FrameSourceError::NoSlots)
    );
    table.retire(token, owner()).unwrap();
    assert_eq!(table.reclaim(1), 1);
    let token = publish(&table, 0x2000);
    table.retire(token, owner()).unwrap();
    assert_eq!(table.reclaim(1), 1);
}

#[test]
fn generation_and_reference_exhaustion_do_not_wrap() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<1>::new(release);
    table.slots[0]
        .word
        .store(word(GEN_MAX, 0, FREE), Ordering::Relaxed);
    assert_eq!(
        table.publish(owner(), key(), 1, 9, |_| panic!(
            "exhausted identity reused"
        )),
        Err(FrameSourceError::NoSlots)
    );
    table.slots[0].word.store(0, Ordering::Relaxed);
    let token = publish(&table, 0x1000);
    let value = table.slots[0].word.load(Ordering::Relaxed);
    table.slots[0]
        .word
        .store(word(generation(value), REF_MAX, LIVE), Ordering::Relaxed);
    assert!(matches!(
        table.pin_raw(token),
        Err(FrameSourceError::RefOverflow)
    ));
    table.slots[0].word.store(value, Ordering::Relaxed);
    table.retire(token, owner()).unwrap();
    for malformed in [0, 1, INDEX_MASK, u64::MAX] {
        assert!(table.pin_raw(malformed).is_err());
    }
}

#[test]
fn provider_retirement_is_exact_and_does_not_wait_for_installers() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<3>::new(release);
    let first = publish(&table, 0x1000);
    let new_owner = FrameSourceOwner {
        service_generation: 12,
        ..owner()
    };
    let other = table
        .publish(new_owner, key(), 1, 10, |_| Ok(0x2000))
        .unwrap();
    let pin = table.pin_raw(first).unwrap();
    assert_eq!(table.retire_owner(owner()), 1);
    assert_eq!(table.retire_owner(owner()), 0);
    assert!(table.pin_raw(first).is_err());
    assert!(table.pin(other, new_owner, key(), 1).is_ok());
    assert!(RELEASED.lock().unwrap().is_empty());
    drop(pin);
    assert!(RELEASED.lock().unwrap().is_empty());
    assert_eq!(table.reclaim(1), 1);
    assert_eq!(RELEASED.lock().unwrap().len(), 1);
    table.retire(other, new_owner).unwrap();
    assert_eq!(table.reclaim(1), 1);
}

#[test]
fn concurrent_pin_and_retire_release_once_after_all_borrows() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<1>::new(release);
    let token = publish(&table, 0x1000);
    let retained = table.pin_raw(token).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let table = &table;
            scope.spawn(move || {
                for _ in 0..100 {
                    if let Ok(pin) = table.pin(token, owner(), key(), 1) {
                        assert_eq!(pin.source.frame, 0x1000);
                    }
                }
            });
        }
        scope.spawn(|| table.retire(token, owner()).unwrap());
    });
    assert!(RELEASED.lock().unwrap().is_empty());
    drop(retained);
    assert!(RELEASED.lock().unwrap().is_empty());
    assert_eq!(table.reclaim(1), 1);
    assert_eq!(*RELEASED.lock().unwrap(), [(0x1000, token, 9)]);
}

#[test]
fn cold_queue_budget_and_slot_reuse_preserve_detached_backlog() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<8>::new(release);
    let mut tokens = std::vec::Vec::new();
    for n in 0..8 {
        tokens.push(publish(&table, 0x1000 * (n + 1)));
    }
    assert_eq!(table.retire_owner(owner()), 8);
    assert!(RELEASED.lock().unwrap().is_empty());
    assert_eq!(table.reclaim(0), 0);
    assert_eq!(table.reclaim(3), 3);
    for n in 0..3 {
        let token = publish(&table, 0x1000 * (n + 9));
        assert!(!tokens.contains(&token));
        tokens.push(token);
        table.retire(token, owner()).unwrap();
    }
    assert_eq!(table.reclaim(2), 2);
    assert_eq!(table.reclaim(32), 6);
    assert_eq!(table.reclaim(32), 0);
    assert_eq!(RELEASED.lock().unwrap().len(), 11);
    let unique: std::collections::BTreeSet<_> = RELEASED
        .lock()
        .unwrap()
        .iter()
        .map(|(_, token, _)| *token)
        .collect();
    assert_eq!(unique.len(), 11);
}

#[test]
fn concurrent_retirement_and_cold_consumers_lose_no_nodes() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<32>::new(release);
    let tokens: std::vec::Vec<_> = (0..32).map(|n| publish(&table, 0x1000 * (n + 1))).collect();
    std::thread::scope(|scope| {
        for &token in &tokens {
            let table = &table;
            scope.spawn(move || {
                table.retire(token, owner()).unwrap();
                table.reclaim(1);
            });
        }
    });
    table.reclaim(64);
    assert_eq!(RELEASED.lock().unwrap().len(), 32);
    let unique: std::collections::BTreeSet<_> = RELEASED
        .lock()
        .unwrap()
        .iter()
        .map(|(_, token, _)| *token)
        .collect();
    assert_eq!(unique.len(), 32);
    assert!(tokens.iter().all(|token| table.pin_raw(*token).is_err()));
}

#[test]
fn malformed_authority_refuses_before_reserving_or_allocating() {
    let _serial = SERIAL.lock().unwrap();
    RELEASED.lock().unwrap().clear();
    let table = Table::<1>::new(release);
    let bad = || panic!("invalid supply reached allocator");
    assert_eq!(
        table.publish(
            FrameSourceOwner {
                service_slot: 0,
                ..owner()
            },
            key(),
            1,
            9,
            |_| bad()
        ),
        Err(FrameSourceError::Invalid)
    );
    assert_eq!(
        table.publish(
            owner(),
            FrameSourceKey {
                page_offset: 1,
                ..key()
            },
            1,
            9,
            |_| bad()
        ),
        Err(FrameSourceError::Invalid)
    );
    assert_eq!(
        table.publish(owner(), key(), 0, 9, |_| bad()),
        Err(FrameSourceError::Invalid)
    );
    assert_eq!(
        table.publish(
            owner(),
            key(),
            frame_capability::FRAME_RIGHT_WRITE | frame_capability::FRAME_RIGHT_EXECUTE,
            9,
            |_| bad()
        ),
        Err(FrameSourceError::Invalid)
    );
    let token = publish(&table, 0x1000);
    table.retire(token, owner()).unwrap();
}
