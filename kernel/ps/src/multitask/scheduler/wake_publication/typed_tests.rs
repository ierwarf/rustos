//! Owner: actual typed wait phase, physical Binding and queue/FIFO witnesses.
//! Boundary: payload/arm CAS -> original pending phase -> frame/queue -> FIFO.
//! Lifecycle: exact match excludes later arms until last wake effect releases.
//! Concurrency: actual owner atomics; native IRQ/earlier claim/consumer ACK modeled.
//! Failure: wrong kind/key/process, cancelled phase, terminal and reused UID refuse.
//! Forbidden: scalar payload lifetime or modeled Current as hardware return proof.
//! Evidence: production-shared epoch/CAS, physical banks and real handoff FIFO.
use super::*;
const ID: u64 = 81600;
const TOKEN: u64 = 901;
fn root(current: bool, f: impl FnOnce(usize)) {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x400000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x700000);
    root_creation::tests::exec_root(ID, current, boot, |slot, _| f(slot));
}
fn scope() -> nucleus_core::util::lockdep::DispatchPreemption<'static> {
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
    scope.mask_irqs();
    scope
}
fn arm(slot: usize) {
    // Only the earlier queue claim is modeled, not incoming MM/register return.
    runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
    assert!(runqueue::wait::try_publish_arm(
        slot,
        runqueue::wait::REASON_PAGER_FAULT,
        TOKEN
    ));
}

#[test]
fn actual_typed_concurrent_arm_and_capture_have_one_winner_without_runowner_generation_change() {
    root(true, |slot| {
        let _scope = scope();
        arm(slot);
        let generation = runqueue::owner(slot).generation;
        for _ in 0..16 {
            assert!(runqueue::wait::try_publish_arm(
                slot,
                runqueue::wait::REASON_PAGER_FAULT,
                TOKEN
            ));
            let start = std::sync::Barrier::new(2);
            let finish = std::sync::Barrier::new(2);
            let (captured, armed) = std::thread::scope(|threads| {
                let armer = threads.spawn(|| {
                    start.wait();
                    let armed = runqueue::wait::try_publish_arm(
                        slot,
                        runqueue::wait::REASON_PAGER_SERVICE,
                        0,
                    );
                    finish.wait();
                    armed
                });
                let binding = Binding::capture(slot, ID).unwrap();
                start.wait();
                let phase = runqueue::wait::TypedWake::capture(
                    &binding,
                    runqueue::wait::REASON_PAGER_FAULT,
                    TOKEN,
                );
                finish.wait(); // keep ORIGINAL pending phase until rival arm completes
                let captured = phase.is_some();
                if let Some(phase) = phase {
                    assert!(phase.clear());
                }
                (captured, armer.join().unwrap())
            });
            assert_ne!(captured, armed);
            assert_eq!(runqueue::owner(slot).generation, generation);
            assert_eq!(
                runqueue::wait::reason(slot),
                if captured {
                    (0, 0)
                } else {
                    (runqueue::wait::REASON_PAGER_SERVICE, 0)
                }
            );
        }
    });
}

#[test]
fn actual_typed_terminal_after_capture_cannot_clear_or_resurrect_and_keeps_original_reuse_fence() {
    root(true, |slot| {
        let scope = scope();
        arm(slot);
        let refs = identity::execution::count(slot);
        let binding = Binding::capture(slot, ID).unwrap();
        let phase =
            runqueue::wait::TypedWake::capture(&binding, runqueue::wait::REASON_PAGER_FAULT, TOKEN)
                .unwrap();
        let stop =
            terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        // Actual queue terminal effect. Enclosing process detach/consumer ACK
        // remains a separate model; keep the ORIGINAL producer reservation.
        runqueue::retire(slot, runqueue::weight::value(slot));
        assert!(!phase.clear());
        assert!(!runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN
        ));
        assert!(!pager_reply_with(&scope, TOKEN, ID, &|_| 1));
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        assert_eq!(identity::execution::count(slot), refs + 1);
        assert!(runqueue::owner(slot).state.is_terminal());
        drop(phase);
        drop(binding);
        assert_eq!(identity::execution::count(slot), refs);
        drop(stop);
    });
}

#[test]
fn actual_typed_phase_excludes_same_payload_rearm_until_original_publication_releases() {
    root(true, |slot| {
        let _scope = scope();
        arm(slot);
        let refs = identity::execution::count(slot);
        let binding = Binding::capture(slot, ID).unwrap();
        let generation = runqueue::owner(slot).generation;
        let phase =
            runqueue::wait::TypedWake::capture(&binding, runqueue::wait::REASON_PAGER_FAULT, TOKEN)
                .unwrap();
        assert!(runqueue::wait::TypedWake::capture(&binding, runqueue::wait::REASON_PAGER_FAULT, TOKEN).is_none());
        assert!(phase.clear());
        assert!(!phase.clear());
        assert_eq!(runqueue::owner(slot).generation, generation);
        assert_eq!(runqueue::wait::reason(slot), (0, 0));
        assert!(!runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN
        ));
        assert!(!runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_SERVICE,
            0
        ));
        assert_eq!(identity::execution::count(slot), refs + 1);
        drop(phase);
        assert!(runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN
        ));
        assert_eq!(runqueue::owner(slot).generation, generation);
        assert_eq!(
            runqueue::wait::reason(slot),
            (runqueue::wait::REASON_PAGER_FAULT, TOKEN)
        );
        drop(binding);
        assert_eq!(identity::execution::count(slot), refs);
    });
}

#[test]
fn actual_typed_cancel_invalidates_original_match_without_clearing_or_rearming_a_new_wait() {
    root(true, |slot| {
        let _scope = scope();
        arm(slot);
        let binding = Binding::capture(slot, ID).unwrap();
        let phase =
            runqueue::wait::TypedWake::capture(&binding, runqueue::wait::REASON_PAGER_FAULT, TOKEN)
                .unwrap();
        // Scalar arm withdrawal preserves the same payload, but invalidates
        // the original epoch. Same-key equality must not replace that custody.
        runqueue::wait::set_armed(slot, false);
        assert_eq!(runqueue::wait::reason(slot), (runqueue::wait::REASON_PAGER_FAULT, TOKEN));
        assert!(!phase.clear());
        runqueue::wait::clear_arm(slot);
        assert!(!phase.clear());
        assert!(!runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN
        ));
        drop(phase);
        assert!(runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN + 1
        ));
        assert!(
            runqueue::wait::TypedWake::capture(&binding, runqueue::wait::REASON_PAGER_FAULT, TOKEN)
                .is_none()
        );
        assert_eq!(
            runqueue::wait::reason(slot),
            (runqueue::wait::REASON_PAGER_FAULT, TOKEN + 1)
        );
        assert!(runqueue::wait::armed(slot));
    });
}

#[test]
fn actual_typed_original_phase_survives_current_wake_through_last_effect_not_just_clear() {
    root(true, |slot| {
        let _scope = scope();
        arm(slot);
        let binding = Binding::capture(slot, ID).unwrap();
        let refs = identity::execution::count(slot);
        // SAFETY: SAME modeled masked CPU, original physical Current/Binding.
        let binding = unsafe {
            retained_wake_matching(
                binding,
                Some((runqueue::wait::REASON_PAGER_FAULT, TOKEN)),
                &|_| 1,
                |binding| {
                    assert_eq!(binding.id(), ID);
                    assert_eq!(identity::execution::count(slot), refs);
                    assert_eq!(runqueue::wait::reason(slot), (0, 0));
                    // Running arm WOULD succeed if wake released its phase before here.
                    assert!(!runqueue::wait::try_publish_arm(
                        slot,
                        runqueue::wait::REASON_PAGER_FAULT,
                        TOKEN
                    ));
                    true
                },
            )
        }
        .ok()
        .unwrap();
        assert!(runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN
        ));
        drop(binding);
    });
}

#[test]
fn actual_typed_pager_matches_exact_key_and_keeps_original_loan_through_real_fifo() {
    root(false, |slot| {
        let scope = scope();
        sync_handoff::remove_slot_all_cpus(slot);
        let fifo = sync_handoff::model_len(0);
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_PAGER_FAULT, TOKEN);
        let before = runqueue::owner(slot);
        let original = Binding::capture(slot, ID).unwrap();
        assert!(runqueue::wait::TypedWake::capture(&original, runqueue::wait::REASON_PAGER_FAULT, TOKEN + 1).is_none());
        drop(original);
        assert!(!pager_reply_with(&scope, TOKEN + 1, ID, &|_| 1));
        assert!(!pager_reply_with(&scope, TOKEN, ID + 1, &|_| 1));
        assert_eq!(runqueue::owner(slot), before);
        assert_eq!(
            runqueue::wait::reason(slot),
            (runqueue::wait::REASON_PAGER_FAULT, TOKEN)
        );
        assert_eq!(sync_handoff::model_len(0), fifo);
        let refs = identity::execution::count(slot);
        let binding = Binding::capture(slot, ID).unwrap();
        // SAFETY: SAME original modeled scope survives reader, queue and FIFO.
        let binding = unsafe {
            retained_wake_matching(
                binding,
                Some((runqueue::wait::REASON_PAGER_FAULT, TOKEN)),
                &|_| 1,
                |binding| {
                    assert_eq!(identity::execution::count(slot), refs + 1);
                    assert!(
                        runqueue::wait::TypedWake::capture(
                            binding,
                            runqueue::wait::REASON_GENERIC,
                            0
                        )
                        .is_none()
                    );
                    let stop =
                        terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false)
                            .unwrap();
                    assert!(Binding::capture(slot, ID).is_none());
                    assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
                    drop(stop); // Untouched reservation rollback; external ACK is not inferred.
                    publish_reply_binding(&scope, binding)
                },
            )
        }
        .ok()
        .unwrap();
        assert_eq!(sync_handoff::model_len(0), fifo + 1);
        assert_eq!(runqueue::wait::reason(slot), (0, 0));
        drop(binding);
        assert_eq!(identity::execution::count(slot), refs);
        assert!(!pager_reply_with(&scope, TOKEN, ID, &|_| 1));
        assert_eq!(sync_handoff::model_len(0), fifo + 1);
        sync_handoff::remove_slot_all_cpus(slot);
    });
}

#[test]
fn actual_typed_service_requires_original_process_and_wait_kind_and_rejects_foreign_scope() {
    root(false, |slot| {
        let pid = stack_resources::context::read(slot)
            .unwrap()
            .process_id
            .unwrap();
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_PAGER_FAULT, TOKEN);
        let scope = scope();
        let owner = runqueue::owner(slot);
        assert_eq!(pager_service_with(&scope, pid, &|_| 1), None);
        assert_eq!(runqueue::owner(slot), owner);
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_PAGER_SERVICE, 0);
        let owner = runqueue::owner(slot);
        assert_eq!(pager_service_with(&scope, pid + 1, &|_| 1), None);
        // A stale diagnostic/advisory PID may pass the cheap pre-filter. It
        // must never replace the original physical binding's process check.
        let original = identity::read(slot).unwrap();
        let advisory = identity::TaskIdentity { process_id: Some(pid + 1), ..original };
        identity::publish_metadata(slot, Some(advisory));
        let stale = pager_service_with(&scope, pid + 1, &|_| 1);
        identity::publish_metadata(slot, Some(original));
        assert_eq!(stale, None);
        assert_eq!(runqueue::owner(slot), owner);
        drop(scope);
        let unmasked =
            nucleus_core::util::lockdep::DispatchPreemption::enter_model_with_enabled_irqs(0);
        assert_eq!(pager_service_with(&unmasked, pid, &|_| 1), None);
        assert!(!pager_reply_with(&unmasked, TOKEN, ID, &|_| 1));
        drop(unmasked);
        let foreign = nucleus_core::util::lockdep::DispatchPreemption::enter_model(1);
        assert_eq!(pager_service_with(&foreign, pid, &|_| 1), None);
        assert!(!pager_reply_with(&foreign, TOKEN, ID, &|_| 1));
        assert_eq!(runqueue::owner(slot), owner);
        drop(foreign);
        let scope = self::scope();
        assert_eq!(pager_service_with(&scope, pid, &|_| 1), Some(ID));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert!(runqueue::published_runnable_count(0) > 0);
        assert_eq!(runqueue::wait::reason(slot), (0, 0));
        assert_eq!(pager_service_with(&scope, pid, &|_| 1), None);
    });
}

#[test]
fn actual_typed_refused_running_arm_preserves_original_blocked_payload_and_current_return_wake() {
    root(true, |slot| {
        let scope = scope();
        arm(slot);
        assert_eq!(
            runqueue::wait::commit(slot),
            runqueue::WaitCommitOutcome::Committed
        );
        let owner = runqueue::owner(slot);
        assert!(!runqueue::wait::try_publish_arm(
            slot,
            runqueue::wait::REASON_PAGER_FAULT,
            TOKEN + 1
        ));
        assert_eq!(runqueue::owner(slot), owner);
        assert_eq!(
            runqueue::wait::reason(slot),
            (runqueue::wait::REASON_PAGER_FAULT, TOKEN)
        );
        assert!(matches!(
            runqueue::publish_return(slot, 0, runqueue::weight::value(slot), true),
            runqueue::ReturnPublication::Blocked { .. }
        ));
        let rsp = runqueue::saved_rsp(slot);
        runqueue::set_saved_rsp(slot, 1); // consumed frame: never dereference
        assert!(pager_reply_with(&scope, TOKEN, ID, &|_| 1));
        assert_eq!(runqueue::saved_rsp(slot), 1);
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert_eq!(runqueue::wait::reason(slot), (0, 0));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        runqueue::set_saved_rsp(slot, rsp);
        sync_handoff::remove_slot_all_cpus(slot);
    });
}
