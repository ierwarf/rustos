//! Same historical obligation, now with real born resources, original Current,
//! actual generation/UID, physical frame, funding and final claim. Earlier
//! architectural admission/elapsed time are modeled by the common fixture.
use crate::multitask::scheduler::{
    dispatch_claim::{self, DispatchFrame, FrameCapture, FrameCustody, tests::fixture},
    frame_publication, runqueue, scheduling_runtime,
};

#[test]
fn retained_current_frame_rejects_foreign_cpu_changed_rsp_and_republished_owner() {
    fixture(true, |slot, turn| {
        let id = crate::multitask::current_identity::read_dispatch(slot).unwrap().task_id.unwrap();
        let generation = runqueue::owner(slot).generation;
        let rsp = runqueue::saved_rsp(slot);
        let capture = |g, r| DispatchFrame::capture(turn, slot, id, g, r, FrameCustody::Current);
        assert!(matches!(capture(generation, rsp), FrameCapture::Ready(_)));
        assert!(matches!(capture(generation, rsp + 16), FrameCapture::Stale));
        assert!(frame_publication::current_local_frame_token(slot, 1).is_none());
        runqueue::publish_blocked(slot, 0, runqueue::weight::value(slot));
        runqueue::publish_local(slot, 0, runqueue::weight::value(slot));
        assert!(matches!(capture(generation, rsp), FrameCapture::Stale));
        let fresh = runqueue::owner(slot).generation;
        assert!(matches!(capture(fresh, rsp), FrameCapture::Ready(_)));

        // Original terminal and placement admission is checked at the real
        // claim leaf, not replaced by a copied lifecycle/affinity snapshot.
        let FrameCapture::Ready(frame) = capture(fresh, rsp) else { panic!("lost frame"); };
        let funded = frame.prepare().unwrap();
        runqueue::affinity_payload::set_affinity(slot, 2, 2, false);
        assert!(!funded.claim());
        runqueue::affinity_payload::set_affinity(slot, 1, 1, false);
        assert!(!crate::multitask::scheduler::stack_resources::cpu_leases::owns_prepared(0, slot));

        let context = crate::multitask::scheduler::stack_resources::context::read(slot).unwrap().scheduling_context;
        let policy = context.policy().unwrap();
        let domain = context.domain_slot().unwrap();
        let domain_generation = context.domain_generation().unwrap();
        assert!(scheduling_runtime::charge_domain_runtime(domain, domain_generation, policy, 0, 1, policy.budget_ns).is_some());
        let FrameCapture::Ready(frame) = capture(fresh, rsp) else { panic!("lost budget-refused frame"); };
        assert!(frame.prepare().is_none());
        // Retained Current fallback cannot bypass budget just because its
        // original physical bank and same-generation frame remain valid.
        let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_claim::claim_fallback(turn, slot,
                crate::multitask::scheduler::donation_ledger::class_publication_sequence())
        }));
        assert!(refusal.is_err());
        assert!(!crate::multitask::scheduler::stack_resources::cpu_leases::owns_prepared(0, slot));
    });
}
