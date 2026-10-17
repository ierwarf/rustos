use super::*;

#[test]
fn closing_keeps_old_readers_and_refuses_rebind_until_last_release() {
    let cell = PinCell::new();
    cell.bind(11);
    assert!(cell.pin(11));
    assert!(!cell.close(11));
    assert!(!cell.pin(11));
    assert!(!cell.pin(12));
    assert!(std::panic::catch_unwind(|| cell.bind(12)).is_err());
    assert!(cell.release(11));
    cell.bind(12);
    assert!(!cell.pin(11));
    assert!(cell.pin(12));
    assert!(!cell.release(12));
    assert!(cell.close(12));
}

#[test]
fn metadata_republication_never_reopens_closing_or_resets_readers() {
    let cell = PinCell::new();
    cell.bind(21);
    assert!(cell.pin(21));
    cell.bind(21);
    assert_eq!(cell.0.load(Ordering::Acquire) & COUNT, 1);
    assert!(!cell.close(21));
    cell.bind(21);
    assert!(!cell.pin(21));
    assert!(cell.release(21));
    assert!(cell.close(21));
}

#[test]
fn foreign_generation_saturation_and_duplicate_release_fail_closed() {
    let cell = PinCell::new();
    cell.bind(u32::MAX);
    assert!(!cell.pin(1));
    assert!(std::panic::catch_unwind(|| cell.release(u32::MAX)).is_err());
    assert!(std::panic::catch_unwind(|| cell.close(1)).is_err());
    cell.0.store(
        (u64::from(u32::MAX) << GENERATION_SHIFT) | COUNT,
        Ordering::Release,
    );
    assert!(!cell.pin(u32::MAX));
    assert_eq!(cell.0.load(Ordering::Acquire) & COUNT, COUNT);
}

#[test]
fn concurrent_close_cannot_observe_zero_then_admit_a_new_reader() {
    for _ in 0..128 {
        let cell = PinCell::new();
        cell.bind(31);
        assert!(cell.pin(31)); // keeps the allocation until all racers join
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let cell = &cell;
                scope.spawn(move || {
                    for _ in 0..64 {
                        if cell.pin(31) {
                            cell.release(31);
                        }
                    }
                });
            }
            assert!(!cell.close(31));
            assert!(!cell.pin(31));
        });
        assert!(cell.release(31));
        assert_eq!(cell.0.load(Ordering::Acquire) & COUNT, 0);
        assert!(!cell.pin(31));
    }
}

#[test]
fn descriptor_and_syscall_pins_delay_real_reap_and_notify_without_table_release() {
    let _isolation = super::super::tests::isolate_process_table();
    let handle = super::super::create_process(0xa551, super::super::tests::new_state()).unwrap();
    let descriptor = super::super::identity::pin_process_object(handle, 0xa551).unwrap();
    let syscall = super::super::own_process_ref(handle, 0xa551).unwrap();
    assert!(super::super::own_process_ref(handle, 0xa552).is_none());
    super::super::detach_task(handle).unwrap();
    assert_eq!(super::super::reap_exited_processes(), 0);
    assert!(super::super::attach_task(handle).is_none());
    assert!(super::super::own_process_ref(handle, 0xa551).is_none());
    drop(descriptor);
    assert_eq!(syscall.with_state(|pid, _| pid), 0xa551);
    assert_eq!(super::super::reap_exited_processes(), 0);
    drop(syscall);
    assert_eq!(super::super::reap_exited_processes(), 1);
    let recycled = super::super::create_process(0xa552, super::super::tests::new_state()).unwrap();
    assert_eq!(recycled.index(), handle.index());
    assert_ne!(recycled.generation(), handle.generation());
    assert!(pin(handle).is_none());
    assert!(super::super::own_process_ref(recycled, 0xa551).is_none());
    assert!(super::super::own_process_ref(recycled, 0xa552).is_some());
}

#[test]
fn process_placement_transaction_retains_real_object_until_refusal_releases_without_rebinding() {
    use crate::multitask::process_placement as placement;
    let _isolation = super::super::tests::isolate_process_table();
    let _serial = crate::multitask::scheduler::execution_test_serial_guard();
    placement::reset_for_tests();
    let handle = super::super::create_process(0xa561, super::super::tests::new_state()).unwrap();
    placement::register_root(handle, 126, 126011, 3).unwrap();
    let change = placement::Change::begin(handle, (126, 126011)).unwrap();
    super::super::detach_task(handle).unwrap();
    assert_eq!(super::super::reap_exited_processes(), 0);
    drop(change);
    assert_eq!(super::super::reap_exited_processes(), 1);
    placement::unregister(handle, 126, 126011);
    let fresh = super::super::create_process(0xa562, super::super::tests::new_state()).unwrap();
    assert_eq!(fresh.index(), handle.index());
    assert_ne!(fresh.generation(), handle.generation());
    assert!(placement::Change::begin(handle, (126, 126011)).is_none());
    placement::reset_for_tests();
}
