//! Owner: actual migration eligibility and physical transition-wake consumers.
//! Boundary: original births/loans/frame/funding; earlier Transition and IRQ inputs modeled.
//! Lifecycle: mailbox custody precedes modeled RSP ACK, actual drain and original claim.
//! Concurrency: duplicate wake cannot renew the queue owner or bypass terminal custody.
//! Failure: nonrunnable, target-excluded and executing candidates refuse independently.
//! Forbidden: modeled Transition/RSP as hardware ACK or the full execution joint proof.
//! Evidence: historical IDs now use actual born resources and production algorithms.
use super::*;
use crate::multitask::scheduler::{activation, root_creation, dispatch_claim, wake_publication};

pub(in crate::multitask::scheduler) fn source_eligibility() {
    root_creation::tests::suspended_roots(&[0x7d01], 3,
        |_| crate::multitask::UserTaskBootstrap::new(crate::user::abi::UserAbi::Linux,
            x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x1000),
            x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x4000)),

        |roots, scope| {
            let slot = roots[0].0;
            assert!(activation::Activation::capture(&[(slot, 0x7d01)]).unwrap()
                .commit_owned(scope, |_| 3, || {}));
            let source = runqueue::owner(slot).cpu.unwrap();
            let target = 1 - source;
            let _ = runqueue::drain_remote_wakes(source);
            assert!(mobile(slot, source));
            assert!(candidate(slot, source, target).is_some());
            runqueue::set_runnable(slot, false);
            assert!(!mobile(slot, source));
            assert!(candidate(slot, source, target).is_none());
            runqueue::set_runnable(slot, true);
            runqueue::affinity_payload::set_affinity(slot, 1 << source, 3, false);
            assert!(mobile(slot, source));
            assert!(candidate(slot, source, target).is_none());
            runqueue::affinity_payload::set_affinity(slot, 3, 3, false);
            {
                // Earlier assembly ownership is a native input, not a hardware ACK.
                let _transition = crate::multitask::cpu_local::install_test_transition_owner(source, 119, slot);
                assert!(!mobile(slot, source));
                assert!(candidate(slot, source, target).is_none());
            }
            assert!(mobile(slot, source));
            assert!(candidate(slot, source, target).is_some());
        });
}

pub(in crate::multitask::scheduler) fn transition_wake() {
    dispatch_claim::tests::fixture(false, |slot, turn| {
        let weight = runqueue::weight::value(slot);
        runqueue::affinity_payload::set_affinity(slot, 3, 3, false);
        runqueue::test_blocked_return(slot, 0, weight);
        runqueue::wait::set_reason(slot, runqueue::wait::REASON_GENERIC, 0);
        let transition = crate::multitask::cpu_local::install_test_transition_owner(1, 119, slot);
        let refs = current_identity::execution::count(slot);
        // SAFETY: original turn retains its masked same-CPU scope throughout.
        assert!(unsafe { wake_publication::guarded(slot, 82800, &|_| 3) });
        assert_ne!(runqueue::wait::ready_since_ticks(slot), 0);
        let published = runqueue::owner(slot);
        assert_eq!(published.state, runqueue::RunOwnerState::RemoteQueued);
        assert_eq!(published.cpu, Some(1));
        assert!(!runqueue::is_local_dispatchable(slot, 1));
        // SAFETY: duplicate wake remains inside the SAME original turn.
        assert!(unsafe { wake_publication::guarded(slot, 82800, &|_| 3) });
        assert_eq!(runqueue::owner(slot), published);
        assert_eq!(current_identity::execution::count(slot), refs);
        transition.commit_assembly(); // modeled earlier RSP arrival, not real return
        assert!(!runqueue::is_local_dispatchable(slot, 1));
        assert_eq!(runqueue::drain_remote_wakes(1), 1);
        assert!(runqueue::is_local_dispatchable(slot, 1));
        // Retain exact physical identity; never clear it to enable scalar claim.
        assert!(transfer(turn, slot, 1, 0));
        let token = frame_publication::local_frame_token(slot, 0).unwrap();
        let frame = dispatch_claim::tests::capture(slot, turn, dispatch_claim::FrameCustody::Local);
        assert!(frame.prepare().unwrap().claim());
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Running);
        assert!(current_identity::execution::banks::owns_execution(0, slot));
        assert!(matches!(dispatch_claim::DispatchFrame::capture(turn, slot, 82800,
            token.owner_generation, token.saved_rsp, dispatch_claim::FrameCustody::Local),
            dispatch_claim::FrameCapture::Stale));
        dispatch_claim::tests::finish_modeled_preparation(slot, turn.original_current().unwrap().1);
    });
}
