use super::*;
extern crate std;
use crate::multitask::{process_table, scheduler};
const SLOT: usize = 125;
const ID: u64 = 125001;
fn stamp(n: u32) -> ExecReservation {
    process_table::test_exec_stamp(2, 3, n)
}
fn metadata() -> TaskIdentity {
    TaskIdentity {
        task_id: Some(ID),
        user_mode: true,
        abi: Some(UserAbi::Linux),
        process_handle: Some(stamp(1).process_handle()),
        process_id: Some(ID),
        console_session: ConsoleSessionHandle::from_raw(0),
        pager_charge: None,
    }
}
fn seed() {
    clear(SLOT);
    publish_metadata(SLOT, Some(metadata()));
}
fn recover() {
    with_writer(SLOT, |w| {
        w.exec_intent = None;
        w.lifecycle.exec_quiesced = false;
        WORDS[SLOT].store(0, Ordering::Release);
    });
    clear(SLOT);
}
#[test]
fn exact_cancel_then_new_reservation_rejects_old_image_cancel_and_drop_without_reopening() {
    let _serial = scheduler::execution_test_serial_guard();
    seed();
    let old = begin(SLOT, ID, stamp(1), false).unwrap();
    let key = old.credential();
    old.prepare_cancel();
    old.cancel(process_table::test_exec_cancellation(stamp(1)));
    let next = begin(SLOT, ID, stamp(2), false).unwrap();
    let word = WORDS[SLOT].load(Ordering::Acquire);
    assert!(!key.admits_image(SLOT, ID));
    let stale = Intent {
        key,
        live: Cell::new(true),
    };
    assert!(!stale.matches(SLOT, stamp(1)));
    assert!(stale.image(stamp(1)).is_none());
    assert!(
        std::panic::catch_unwind(|| stale.cancel(process_table::test_exec_cancellation(stamp(1))))
            .is_err()
    );
    assert_eq!(WORDS[SLOT].load(Ordering::Acquire), word);
    assert!(lifecycle(SLOT).exec_quiesced);
    drop(Intent {
        key,
        live: Cell::new(true),
    });
    assert_eq!(WORDS[SLOT].load(Ordering::Acquire), word);
    next.prepare_cancel();
    next.cancel(process_table::test_exec_cancellation(stamp(2)));
    clear(SLOT);
}
#[test]
fn unqualified_gate_and_identity_reset_cannot_spend_owned_cold_intent() {
    let _serial = scheduler::execution_test_serial_guard();
    seed();
    let intent = begin(SLOT, ID, stamp(1), false).unwrap();
    assert!(
        std::panic::catch_unwind(|| update_live_lifecycle(
            SLOT,
            ID,
            LifecycleGate::ExecQuiesced,
            false
        ))
        .is_err()
    );
    assert!(std::panic::catch_unwind(|| clear(SLOT)).is_err());
    publish_metadata(SLOT, None);
    assert!(busy(SLOT));
    assert!(!intent.matches(SLOT + 1, stamp(1)));
    publish_metadata(SLOT, Some(metadata()));
    intent.prepare_cancel();
    intent.cancel(process_table::test_exec_cancellation(stamp(1)));
    clear(SLOT);
}
#[test]
fn image_refusal_returns_to_same_sealed_owner_but_abandonment_and_publication_never_cancel() {
    let _serial = scheduler::execution_test_serial_guard();
    seed();
    let intent = begin(SLOT, ID, stamp(1), false).unwrap();
    let image = intent.image(stamp(1)).unwrap();
    assert!(intent.image(stamp(1)).is_none());
    image.finish(false);
    assert!(intent.matches(SLOT, stamp(1)));
    let image = intent.image(stamp(1)).unwrap();
    image.publication(stamp(1));
    image.finish(true);
    assert!(intent.credential().published(stamp(1)));
    assert!(
        std::panic::catch_unwind(|| intent.cancel(process_table::test_exec_cancellation(stamp(1))))
            .is_err()
    );
    assert!(busy(SLOT));
    assert!(lifecycle(SLOT).exec_quiesced);
    recover();
    seed();
    let intent = begin(SLOT, ID, stamp(2), false).unwrap();
    drop(intent.image(stamp(2)).unwrap());
    assert_eq!(WORDS[SLOT].load(Ordering::Acquire) & 7, POISONED);
    drop(intent);
    recover();
}
#[test]
fn full_qualifiers_refuse_before_any_gate_effect_and_generation_survives_uid_reset() {
    let _serial = scheduler::execution_test_serial_guard();
    seed();
    assert!(begin(SLOT, ID + 1, stamp(1), false).is_none());
    assert!(begin(SLOT, ID, process_table::test_exec_stamp(3, 3, 1), false).is_none());
    for field in 0..4 {
        let mut partial = metadata();
        match field {
            0 => partial.abi = None,
            1 => partial.process_handle = None,
            2 => partial.process_id = None,
            _ => partial.user_mode = false,
        }
        publish_metadata(SLOT, Some(partial));
        assert!(begin(SLOT, ID, stamp(1), false).is_none());
        assert!(!busy(SLOT));
    }
    publish_metadata(SLOT, Some(metadata()));
    let intent = begin(SLOT, ID, stamp(1), false).unwrap();
    let first = intent.credential().generation;
    intent.prepare_cancel();
    intent.cancel(process_table::test_exec_cancellation(stamp(1)));
    clear(SLOT);
    publish_metadata(SLOT, Some(metadata()));
    let intent = begin(SLOT, ID, stamp(2), false).unwrap();
    assert!(intent.credential().generation > first);
    intent.prepare_cancel();
    intent.cancel(process_table::test_exec_cancellation(stamp(2)));
    with_writer(SLOT, |w| w.exec_generation = u64::MAX / 8);
    assert!(begin(SLOT, ID, stamp(3), false).is_none());
    assert!(!busy(SLOT));
    clear(SLOT);
    with_writer(SLOT, |w| w.exec_generation = 0); // isolated exhaustion fixture
}
