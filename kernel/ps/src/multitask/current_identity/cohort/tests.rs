use super::*;
use crate::multitask::current_identity as identity;
const A: usize = 126;
const B: usize = 127;
fn metadata(id: u64) -> identity::TaskIdentity {
    identity::TaskIdentity {
        task_id: Some(id),
        user_mode: false,
        abi: None,
        process_handle: None,
        process_id: None,
        console_session: crate::io::session::ConsoleSessionHandle::from_raw(0),
        pager_charge: None,
    }
}
fn isolated(f: impl FnOnce()) {
    let _serial = crate::multitask::scheduler::execution_test_serial_guard();
    identity::clear(A);
    identity::clear(B);
    identity::publish_metadata(A, Some(metadata(126001)));
    identity::publish_metadata(B, Some(metadata(127001)));
    f();
    identity::clear(A);
    identity::clear(B);
}
#[test]
fn job_control_original_cohort_excludes_dispatch_through_each_gate_and_cannot_spend_affinity() {
    isolated(|| {
        let mut owner = Cohort::begin_job_control(&[(A, 126001), (B, 127001)]).unwrap();
        owner.begin_effects();
        assert!(owner.owns_job_control(A, 126001));
        assert!(std::panic::catch_unwind(|| identity::update_live_lifecycle(
            A, 126001, identity::LifecycleGate::JobStopped, true)).is_err());
        assert!(!owner.owns_affinity(A, 126001));
        assert!(!owner.owns_affinity(B, 127001));
        assert!(!admission_ready(A) && !admission_ready(B));
        assert_eq!(owner.set_job_stopped(A, 126001, true), Some(true));
        assert!(!admission_ready(A) && !admission_ready(B));
        assert!(execution::with_claim(B, |_| panic!("partial group escaped")).is_none());
        assert_eq!(owner.set_job_stopped(B, 127001, true), Some(true));
        assert_eq!(owner.set_job_stopped(A, 999, false), None);
        assert_eq!(execution::count(A), 1);
        assert_eq!(execution::count(B), 1);
        owner.complete();
        assert!(admission_ready(A) && admission_ready(B));
        assert!(identity::lifecycle(A).job_stopped && identity::lifecycle(B).job_stopped);
        assert!(execution::initial(A, 126001).is_none());
        let mut resume = Cohort::begin_job_control(&[(A, 126001), (B, 127001)]).unwrap();
        resume.begin_effects();
        assert_eq!(resume.set_job_stopped(B, 127001, false), Some(true));
        assert!(!admission_ready(A) && !admission_ready(B));
        assert!(execution::with_claim(B, |_| panic!("partial resume escaped")).is_none());
        // Close an already captured member: retain original pin, never reopen
        // its stopped gate or mistake a hidden identity for another member.
        identity::update_live_lifecycle(A, 126001, identity::LifecycleGate::Retired, true);
        assert_eq!(resume.set_job_stopped(A, 126001, false), Some(false));
        assert!(identity::lifecycle(A).job_stopped);
        assert_eq!(resume.set_job_stopped(B, 127001, false), Some(false));
        resume.complete();
        assert!(identity::lifecycle(A).retired);
        assert!(execution::initial(A, 126001).is_none());
        assert!(execution::with_claim(B, |_| ()).is_some());
    });
}

#[test]
fn cohort_admission_is_closed_through_every_partial_publication_and_one_release_opens_all() {
    isolated(|| {
        identity::update_live_lifecycle(A, 126001, identity::LifecycleGate::Suspended, true);
        identity::update_live_lifecycle(B, 127001, identity::LifecycleGate::Suspended, true);
        let mut owner = Cohort::begin_activation(&[(A, 126001), (B, 127001)]).unwrap();
        assert_eq!(execution::count(A), 1);
        assert_eq!(execution::count(B), 1);
        assert!(
            execution::with_claim(A, |_| panic!("first member escaped closed cohort")).is_none()
        );
        assert!(execution::initial(A, 126001).is_none());
        owner.begin_effects();
        assert!(
            std::panic::catch_unwind(|| identity::update_live_lifecycle(
                A,
                126001,
                identity::LifecycleGate::Suspended,
                false
            ))
            .is_err()
        );
        assert!(owner.unsuspend(A, 126001));
        assert!(!owner.owns_affinity(A, 126001));
        assert!(!admission_ready(A));
        assert!(!admission_ready(B));
        assert!(owner.unsuspend(B, 127001));
        owner.complete();
        assert!(admission_ready(A));
        assert!(admission_ready(B));
        assert_eq!(execution::count(A), 0);
        assert_eq!(execution::count(B), 0);
        assert!(execution::with_claim(A, |_| ()).is_some());
        assert!(execution::with_claim(B, |_| ()).is_some());
    });
}

#[test]
fn complete_release_opens_every_member_before_slow_per_slot_unseal_without_a_global_claim_lock() {
    isolated(|| {
        std::thread::scope(|scope| {
            let mut owner = Cohort::begin(&[(A, 126001), (B, 127001)]).unwrap();
            owner.begin_effects();
            let (entered, wait_entered) = std::sync::mpsc::channel();
            let (release, wait_release) = std::sync::mpsc::channel();
            let hold = scope.spawn(move || {
                with_writer(B, |_| {
                    entered.send(()).unwrap();
                    wait_release.recv().unwrap();
                })
            });
            wait_entered.recv().unwrap();
            let finish = scope.spawn(move || owner.complete());
            let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while BATCHES[A].load(Ordering::Acquire) & 3 != COMPLETE
                && std::time::Instant::now() < until
            {
                std::thread::yield_now();
            }
            let all_ready = admission_ready(A) && admission_ready(B);
            let still_pinned = busy(B) && execution::count(B) == 1;
            release.send(()).unwrap();
            hold.join().unwrap();
            finish.join().unwrap();
            assert!(
                all_ready,
                "unseal exposed only a prefix instead of one cohort release"
            );
            assert!(
                still_pinned,
                "publication spent the slow member's lifetime pin early"
            );
            assert_eq!(execution::count(B), 0);
        });
    });
}
#[test]
fn failed_member_and_duplicate_preflight_roll_back_only_exact_binding_pins_and_epoch_never_replays()
{
    isolated(|| {
        let before = BATCHES[A].load(Ordering::Acquire);
        assert!(Cohort::begin(&[(A, 126001), (B, 999)]).is_none());
        assert!(!busy(A));
        assert!(!busy(B));
        assert_eq!(execution::count(A), 0);
        assert!(BATCHES[A].load(Ordering::Acquire) > before);
        assert!(Cohort::begin(&[(A, 126001), (A, 126001)]).is_none());
        let owner = Cohort::begin(&[(A, 126001), (B, 127001)]).unwrap();
        let cookie = owner.cookie;
        drop(owner);
        let owner = Cohort::begin(&[(A, 126001), (B, 127001)]).unwrap();
        assert_ne!(cookie, owner.cookie);
        assert!(!admission_ready(A));
        assert!(execution::initial(A, 126001).is_none());
        drop(owner);
    });
}
#[test]
fn callback_failure_before_effects_can_abort_but_partial_effects_quarantine_binding_and_admission()
{
    isolated(|| {
        let refused = std::panic::catch_unwind(|| {
            let _owner = Cohort::begin(&[(A, 126001), (B, 127001)]).unwrap();
            panic!("authority callback refused");
        });
        assert!(refused.is_err());
        assert!(!busy(A));
        assert_eq!(execution::count(A), 0);
        let failed = std::panic::catch_unwind(|| {
            let mut owner = Cohort::begin(&[(A, 126001), (B, 127001)]).unwrap();
            owner.begin_effects();
            panic!("partial cold publication");
        });
        assert!(failed.is_err());
        assert!(busy(A));
        assert!(busy(B));
        assert!(!admission_ready(A));
        assert_eq!(execution::count(A), 1);
        assert!(Cohort::begin(&[(A, 126001)]).is_none());
        assert!(std::panic::catch_unwind(|| identity::clear(A)).is_err());
        // Explicit isolated host recovery; never production rollback/ACK.
        let cookie = MEMBERS[A].load(Ordering::Acquire);
        for slot in [A, B] {
            assert_eq!(MEMBERS[slot].swap(0, Ordering::AcqRel), cookie);
            execution::recover_binding_pin_for_test(slot);
        }
        BATCHES[A].fetch_and(!3, Ordering::Release);
    });
}
