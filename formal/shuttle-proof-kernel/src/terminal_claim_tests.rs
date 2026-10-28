/// Same bound as the Loom kernel with PCT permutations across the claimer's CAS,
/// install re-read and publication and the remote stop's retirement loop.
#[test]
fn terminal_withdraw_never_retires_an_in_flight_claim_pct() {
    shuttle::check_pct(
        || {
            const LOCAL: usize = 1;
            const RUNNING: usize = 3;
            const RETIRED: usize = 7;
            let owner = Arc::new(AtomicUsize::new(LOCAL));
            let role = Arc::new(AtomicBool::new(false));
            let fatal = Arc::new(AtomicBool::new(false));

            // `true` = retired; `false` = parked behind an in-flight claim.
            fn withdraw(owner: &AtomicUsize, role: &AtomicBool) -> bool {
                loop {
                    let observed = owner.load(Ordering::Acquire);
                    if observed == RETIRED {
                        return true;
                    }
                    if rustos_scheduler_frame_gate::terminal_withdraw_must_wait(
                        observed == RUNNING,
                        role.load(Ordering::Acquire),
                    ) {
                        return false;
                    }
                    if owner
                        .compare_exchange(observed, RETIRED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return true;
                    }
                }
            }

            let (claim_owner, claim_role, claim_fatal) =
                (owner.clone(), role.clone(), fatal.clone());
            let claimer = thread::spawn(move || {
                if claim_owner
                    .compare_exchange(LOCAL, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // capture_checked: the claimed owner must still be Running.
                    if claim_owner.load(Ordering::Acquire) != RUNNING {
                        claim_fatal.store(true, Ordering::Release);
                    }
                    claim_role.store(true, Ordering::Release);
                }
            });
            let (stop_owner, stop_role) = (owner.clone(), role.clone());
            let stopper = thread::spawn(move || withdraw(&stop_owner, &stop_role));

            claimer.join().unwrap();
            let retired = stopper.join().unwrap();
            assert!(
                !fatal.load(Ordering::Acquire),
                "install saw a retired owner"
            );
            if !retired {
                // The parked stop is resumed after the claimer finished.
                assert!(
                    withdraw(&owner, &role),
                    "a parked stop must retire once published"
                );
            }
            assert_eq!(owner.load(Ordering::Acquire), RETIRED);
        },
        iterations(),
        pct_depth(),
    );
}
