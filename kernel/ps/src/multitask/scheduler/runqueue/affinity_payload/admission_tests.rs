use super::*;
use crate::multitask::scheduler::{runqueue, scheduling_runtime};
const SLOT: usize = 125;
fn isolated(f: impl FnOnce()) {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    reset_before_publication();
    f();
    reset_before_publication();
}
#[test]
fn each_mask_uid_and_cpu_bound_refuses_before_callback_including_root_uid_zero() {
    isolated(|| {
        let a = &AFFINITIES[SLOT];
        *a.binding.lock() = Some(0);
        a.initialized.store(true, Ordering::Relaxed);
        store(a, 1, 1, false);
        assert_eq!(with_admission(SLOT, Some(0), 0, || 7), Some(7));
        assert!(with_admission(SLOT, None, 0, || panic!("absent UID spent affinity")).is_none());
        assert!(
            with_admission(SLOT, Some(1), 0, || panic!("foreign UID spent affinity")).is_none()
        );
        assert!(
            with_admission(SLOT, Some(0), 64, || panic!("oversized CPU spent affinity")).is_none()
        );
        // Deliberately malformed independent masks witness each admission gate;
        // producer validation must not mask a missing consumer qualifier.
        a.task_mask.store(0, Ordering::Relaxed);
        assert!(with_admission(SLOT, Some(0), 0, || panic!("task mask ignored")).is_none());
        a.task_mask.store(1, Ordering::Relaxed);
        a.process_mask.store(0, Ordering::Relaxed);
        assert!(with_admission(SLOT, Some(0), 0, || panic!("process mask ignored")).is_none());
    });
}
#[test]
fn concurrent_affinity_writer_waits_until_admitted_cas_callback_has_left_leaf() {
    isolated(|| {
        std::thread::scope(|scope| {
            let (entered, wait_entered) = std::sync::mpsc::channel();
            let (release, wait_release) = std::sync::mpsc::channel();
            let (attempting, wait_attempting) = std::sync::mpsc::channel();
            let (done, wait_done) = std::sync::mpsc::channel();
            let claim = scope.spawn(move || {
                with_admission(SLOT, None, 0, || {
                    entered.send(()).unwrap();
                    wait_release.recv().unwrap();
                    assert_eq!(AFFINITIES[SLOT].task_mask.load(Ordering::Relaxed), u64::MAX);
                    13
                })
            });
            wait_entered.recv().unwrap();
            let writer = scope.spawn(move || {
                attempting.send(()).unwrap();
                set_affinity(SLOT, 2, 2, true);
                done.send(()).unwrap();
            });
            wait_attempting.recv().unwrap();
            let blocked = wait_done.recv_timeout(std::time::Duration::from_millis(100));
            // Release even on a broken exclusion witness, so scoped workers
            // cannot strand the test while reporting the first useful failure.
            if blocked != Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                release.send(()).unwrap();
            }
            assert_eq!(blocked, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
            assert_eq!(affinity_snapshot(SLOT), (u64::MAX, u64::MAX, false));
            release.send(()).unwrap();
            assert_eq!(claim.join().unwrap(), Some(13));
            writer.join().unwrap();
            assert_eq!(affinity_snapshot(SLOT), (2, 2, true));
            assert!(with_admission(SLOT, None, 0, || panic!("closed CPU admitted")).is_none());
            assert_eq!(with_admission(SLOT, None, 1, || 19), Some(19));
        })
    });
}
#[test]
fn overlapping_writers_publish_only_complete_commits_and_unwind_releases_admission() {
    isolated(|| {
        assert!(
            std::panic::catch_unwind(|| with_admission(SLOT, None, 0, || panic!("CAS unwind")))
                .is_err()
        );
        std::thread::scope(|scope| {
            for bit in [1, 2] {
                scope.spawn(move || {
                    for _ in 0..64 {
                        set_affinity(SLOT, bit, bit, bit == 2);
                    }
                });
            }
        });
        let (task, process, pending) = affinity_snapshot(SLOT);
        assert_eq!(task, process);
        assert_eq!(pending, task == 2);
        assert!(with_admission(SLOT, None, task.trailing_zeros() as usize, || true).unwrap());
        assert_eq!(AFFINITIES[SLOT].version.load(Ordering::Acquire) & 1, 0);
    });
}

#[test]
fn reset_is_empty_authority_not_unrestricted_permission_and_old_uid_cannot_rebind() {
    isolated(|| {
        let a = &AFFINITIES[SLOT];
        *a.binding.lock() = Some(0);
        a.initialized.store(true, Ordering::Relaxed);
        store(a, 1, 1, false);
        let before = affinity_snapshot(SLOT);
        assert!(std::panic::catch_unwind(|| set_affinity(SLOT, 2, 2, false)).is_err());
        assert_eq!(affinity_snapshot(SLOT), before);
        assert_eq!(*a.binding.lock(), Some(0));
        reset_affinity(SLOT);
        assert!(with_admission(SLOT, Some(0), 0, || panic!("reset allowed old UID")).is_none());
        *a.binding.lock() = Some(1);
        store(a, 1, 1, false);
        assert!(
            with_admission(SLOT, Some(0), 0, || panic!("old UID spent new affinity")).is_none()
        );
        assert_eq!(with_admission(SLOT, Some(1), 0, || 23), Some(23));
    });
}
#[test]
fn invalid_mask_writer_cannot_modify_binding_version_or_previous_commit() {
    isolated(|| {
        set_affinity(SLOT, 1, 3, false);
        let a = &AFFINITIES[SLOT];
        let version = a.version.load(Ordering::Acquire);
        for (task, process) in [(0, 3), (1, 0), (4, 3)] {
            assert!(std::panic::catch_unwind(|| set_affinity(SLOT, task, process, true)).is_err());
            assert_eq!(a.version.load(Ordering::Acquire), version);
            assert_eq!(affinity_snapshot(SLOT), (1, 3, false));
        }
    });
}

#[test]
fn exhausted_affinity_epoch_refuses_before_any_mask_or_migration_effect() {
    isolated(|| {
        let affinity = &AFFINITIES[SLOT];
        let saved = affinity.version.load(Ordering::Relaxed);
        let before = affinity_snapshot(SLOT);
        affinity.version.store(u64::MAX - 1, Ordering::Release);
        let refused = std::panic::catch_unwind(|| set_affinity(SLOT, 1, 1, true));
        assert!(refused.is_err());
        assert_eq!(affinity.version.load(Ordering::Relaxed), u64::MAX - 1);
        assert_eq!(affinity_snapshot(SLOT), before);
        affinity.version.store(saved, Ordering::Release);
    });
}
