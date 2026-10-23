use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn only_restore_may_use_prepared_custody_and_outgoing_is_never_current() {
    assert!(access_has_custody(true, false, false));
    assert!(access_has_custody(true, false, true));
    assert!(access_has_custody(false, true, true));
    assert!(!access_has_custody(false, true, false));
    assert!(!access_has_custody(false, false, false));
    assert!(!access_has_custody(false, false, true));
}

#[test]
fn simd_binding_requires_explicit_terminal_release_before_uid_reuse() {
    let state = SlotSimdState::new();
    assert!(catch_unwind(AssertUnwindSafe(|| state.bound(0, |_| ()))).is_err());
    state.initialize(0); // root UID zero is a real binding, not an empty sentinel
    state.bound(0, |_| ());
    assert!(catch_unwind(AssertUnwindSafe(|| state.bound(1, |_| ()))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| state.initialize(1))).is_err());
    state.replace(0, false);
    state.bound(0, |_| ());
    state.replace(0, true);
    assert!(catch_unwind(AssertUnwindSafe(|| state.bound(0, |_| ()))).is_err());
    state.initialize(1);
    state.bound(1, |_| ());
    assert!(catch_unwind(AssertUnwindSafe(|| state.replace(0, true))).is_err());
}

#[test]
fn register_ipc_mode_is_exact_task_paired_and_cleared_on_every_replacement() {
    let state = SlotSimdState::new();
    state.initialize(0);
    state.with(|image| {
        set_register_ipc(image, 0, true);
        assert!(image.register_ipc);
        assert!(catch_unwind(AssertUnwindSafe(|| set_register_ipc(image, 1, false))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| set_register_ipc(image, 0, true))).is_err());
        set_register_ipc(image, 0, false);
        assert!(!image.register_ipc);
        set_register_ipc(image, 0, true);
    });
    state.replace(0, false);
    state.with(|image| {
        assert!(!image.register_ipc);
        set_register_ipc(image, 0, true);
    });
    state.replace(0, true);
    state.initialize(1);
    state.with(|image| {
        assert!(!image.register_ipc);
        assert!(catch_unwind(AssertUnwindSafe(|| set_register_ipc(image, 0, true))).is_err());
    });
}

#[test]
fn exclusive_token_refuses_overlap_before_any_raw_image_is_borrowed() {
    let state = SlotSimdState::new();
    let first = state.claim();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _second = state.claim();
        }))
        .is_err()
    );
    drop(first);
    let _next = state.claim();
}

#[test]
fn exclusive_simd_guard_survives_refusal_and_unwind_without_leaking_access() {
    let state = SlotSimdState::new();
    state.initialize(41);
    state.bound(41, |_| {
        assert!(
            catch_unwind(AssertUnwindSafe(
                || state.bound(41, |_| panic!("overlap admitted"))
            ))
            .is_err()
        );
    });
    assert!(
        catch_unwind(AssertUnwindSafe(
            || state.bound(41, |_| panic!("callback unwind"))
        ))
        .is_err()
    );
    state.bound(41, |_| ());
    assert!(!state.busy.load(Ordering::Acquire));
}

#[test]
fn remote_simd_access_never_aliases_a_live_mutable_image() {
    let state = SlotSimdState::new();
    state.initialize(51);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            state.bound(51, |_| {
                barrier.wait();
                barrier.wait();
            })
        });
        barrier.wait();
        assert!(catch_unwind(AssertUnwindSafe(|| state.replace(51, false))).is_err());
        assert!(
            catch_unwind(AssertUnwindSafe(
                || state.bound(51, |_| panic!("remote alias admitted"))
            ))
            .is_err()
        );
        barrier.wait();
    });
    state.replace(51, false);
    state.bound(51, |_| ());
}
