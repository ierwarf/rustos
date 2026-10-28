#![no_std]

/// Pure custody predicate shared with the production stable-frame reader.
/// A successful observation still requires the caller's seqlock/version and
/// post-validation owner rechecks; this predicate alone grants no dispatch.
#[inline]
pub const fn local_frame_custody(
    published_generation: u64,
    owner_generation: u64,
    owner_is_local: bool,
    owner_cpu_matches: bool,
    runnable: bool,
    executing_or_transitioning: bool,
) -> bool {
    published_generation == owner_generation
        && owner_is_local
        && owner_cpu_matches
        && runnable
        && !executing_or_transitioning
}

/// Exact direct handoff custody uses the same generation and stack exclusion
/// as a local queue owner, but carries its own FIFO authority rather than a
/// fair-runqueue membership proof.
#[inline]
pub const fn direct_handoff_frame_custody(
    published_generation: u64,
    owner_generation: u64,
    owner_is_direct_handoff: bool,
    owner_cpu_matches: bool,
    runnable: bool,
    executing_or_transitioning: bool,
) -> bool {
    local_frame_custody(
        published_generation,
        owner_generation,
        owner_is_direct_handoff,
        owner_cpu_matches,
        runnable,
        executing_or_transitioning,
    )
}

/// Activation FIFO custody is not a fair-pick hint: it belongs to the exact
/// committed child and to the Local owner generation observed at selection.
/// The caller must still validate the frame and CAS that generation at claim.
#[inline]
pub const fn first_turn_claim_custody(
    committed_task_id: u64,
    live_task_id: u64,
    selected_generation: u64,
    owner_generation: u64,
    owner_is_local: bool,
    owner_cpu_matches: bool,
    runnable: bool,
) -> bool {
    committed_task_id != 0
        && committed_task_id == live_task_id
        && selected_generation != 0
        && selected_generation == owner_generation
        && owner_is_local
        && owner_cpu_matches
        && runnable
}

/// A published budget stamp may prepare only the exact live context/domain
/// generation on a CPU admitted by that live policy. Period and domain policy
/// epochs are checked separately while both runtime cells are locked.
#[inline]
pub const fn published_budget_identity_custody(
    stamped_context_slot: u64,
    stamped_context_generation: u64,
    stamped_domain_generation: u64,
    live_context_slot: u64,
    live_context_generation: u64,
    live_domain_generation: u64,
    cpu_allowed: bool,
) -> bool {
    stamped_context_slot != 0
        && stamped_context_slot == live_context_slot
        && stamped_context_generation == live_context_generation
        && stamped_domain_generation == live_domain_generation
        && cpu_allowed
}

/// Only a complete, stable negative admission may be omitted from a local
/// candidate scan. An overlapping affinity writer (`None`) is *not* a
/// rejection; the caller must retain its incomplete-observation fallback.
#[inline]
pub const fn stable_local_admission_rejected(
    lifecycle_dispatchable: bool,
    affinity_admission: Option<bool>,
) -> bool {
    !lifecycle_dispatchable || matches!(affinity_admission, Some(false))
}

/// An earlier strict-class decision may release later policy only after a
/// complete negative observation. Incomplete is not evidence of no due peer.
#[inline]
pub const fn early_policy_may_continue(complete: bool, peer_selected: bool) -> bool {
    complete && !peer_selected
}

/// A selected Local frame must name both the owner generation and the exact
/// saved RSP that assembly will restore. Epoch/custody are rechecked after
/// the actual saved-context validation and before the generation-bound CAS.
#[inline]
pub const fn selected_local_frame_identity(
    selected_generation: u64,
    published_generation: u64,
    dispatch_rsp: usize,
    published_rsp: usize,
) -> bool {
    selected_generation != 0
        && selected_generation == published_generation
        && dispatch_rsp != 0
        && dispatch_rsp == published_rsp
}

/// The currently executing task has a new IRQ-saved RSP, not the old saved
/// frame's RSP. Its published geometry may belong to the immediately prior
/// Local/Direct owner or to the same Running generation after publication;
/// no earlier generation or foreign execution CPU may supply it.
#[inline]
pub const fn running_frame_geometry_custody(
    published_generation: u64,
    running_generation: u64,
    exact_current_cpu: bool,
) -> bool {
    exact_current_cpu
        && (published_generation == running_generation
            || (published_generation < u64::MAX && published_generation + 1 == running_generation))
}

/// The outgoing CPU may retain its freshly saved Local continuation only
/// while it still holds exact Current execution custody on that same CPU.
#[inline]
pub const fn retained_current_frame_custody(
    published: u64,
    owner: u64,
    local_cpu: bool,
    runnable: bool,
    exact_current_cpu: bool,
) -> bool {
    published != 0 && published == owner && local_cpu && runnable && exact_current_cpu
}

/// A cleanup ACK belongs to the exact already-claimed task and full immutable
/// payload. The cell lock and terminal publication establish those inputs.
#[inline]
pub const fn retirement_cleanup_ack_custody(
    expected_task: u64,
    returned_task: u64,
    claimed: bool,
    exact_payload: bool,
) -> bool {
    claimed && expected_task == returned_task && exact_payload
}

/// A budget cell's exact denial may remove a fair candidate only while its
/// selected donation/reply token remains unchanged after both cell locks.
/// Missing or republished custody is unknown, never a negative grant.
#[inline]
pub const fn budget_admission_after_token_check(
    admission: Option<bool>,
    token_unchanged: bool,
) -> Option<bool> {
    if token_unchanged { admission } else { None }
}

/// Whether a terminal withdrawal must wait instead of retiring the run-owner
/// word of a slot.
///
/// A dispatch claim is the owner CAS to `Running(cpu)`. The claiming CPU then
/// installs the claimed task's architecture and publishes it as that CPU's
/// Current; both steps check that the owner word is still `Running`. Until the
/// publication a slot is `Running` with **no** execution role, and a remote
/// withdrawal that retires it in that window makes the installing CPU's capture
/// refuse a claim it already owns. Once the slot is published Current the same
/// withdrawal is the normal remote-stop mechanism: the running CPU sees the
/// terminal owner at its next dispatch. So the rule is exactly: wait for a
/// claim that is in flight, and only that.
#[inline]
pub const fn terminal_withdraw_must_wait(
    owner_is_running: bool,
    execution_role_published: bool,
) -> bool {
    owner_is_running && !execution_role_published
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_frame_must_match_both_owner_and_saved_stack() {
        assert!(selected_local_frame_identity(7, 7, 0x2000, 0x2000));
        assert!(!selected_local_frame_identity(7, 8, 0x2000, 0x2000));
        assert!(!selected_local_frame_identity(7, 7, 0x2000, 0x3000));
        assert!(!selected_local_frame_identity(0, 0, 0x2000, 0x2000));
    }

    #[test]
    fn early_policy_requires_complete_negative_evidence_before_later_arms() {
        assert!(!super::early_policy_may_continue(false, false));
        assert!(!super::early_policy_may_continue(false, true));
        assert!(!super::early_policy_may_continue(true, true));
        assert!(super::early_policy_may_continue(true, false));
    }

    #[test]
    fn retained_current_cannot_use_foreign_cpu_or_inexact_local_frame() {
        use super::retained_current_frame_custody as admits;
        assert!(admits(7, 7, true, true, true));
        assert!(!admits(0, 0, true, true, true));
        assert!(!admits(7, 8, true, true, true));
        assert!(!admits(7, 7, false, true, true));
        assert!(!admits(7, 7, true, false, true));
        assert!(!admits(7, 7, true, true, false));
    }

    #[test]
    fn retirement_ack_needs_claimed_exact_task_and_full_payload() {
        use super::retirement_cleanup_ack_custody as admits;
        assert!(admits(19, 19, true, true));
        assert!(!admits(19, 20, true, true));
        assert!(!admits(19, 19, false, true));
        assert!(!admits(19, 19, true, false));
    }

    #[test]
    fn executing_frame_geometry_allows_only_current_or_prior_owner_generation() {
        use super::running_frame_geometry_custody as admits;
        assert!(admits(0, 1, true)); // first root turn
        assert!(admits(7, 8, true)); // saved Local before Running claim
        assert!(admits(8, 8, true)); // refreshed while Running
        assert!(!admits(6, 8, true));
        assert!(!admits(8, 8, false));
        assert!(!admits(u64::MAX, 0, true));
    }

    #[test]
    fn budget_denial_requires_exact_unreplaced_reply_custody() {
        assert_eq!(
            budget_admission_after_token_check(Some(false), true),
            Some(false)
        );
        assert_eq!(budget_admission_after_token_check(Some(false), false), None);
        assert_eq!(budget_admission_after_token_check(None, true), None);
        assert_eq!(budget_admission_after_token_check(Some(true), false), None);
    }

    #[test]
    fn only_stable_negative_publications_can_be_skipped() {
        assert!(stable_local_admission_rejected(false, None));
        assert!(stable_local_admission_rejected(true, Some(false)));
        assert!(!stable_local_admission_rejected(true, None));
        assert!(!stable_local_admission_rejected(true, Some(true)));
    }

    #[test]
    fn generation_and_stack_custody_are_both_mandatory() {
        assert!(local_frame_custody(7, 7, true, true, true, false));
        assert!(!local_frame_custody(7, 8, true, true, true, false));
        assert!(!local_frame_custody(7, 7, true, true, true, true));
    }

    #[test]
    fn activation_requires_committed_identity_and_exact_local_owner() {
        assert!(first_turn_claim_custody(7, 7, 8, 8, true, true, true));
        assert!(!first_turn_claim_custody(7, 9, 8, 8, true, true, true));
        assert!(!first_turn_claim_custody(7, 7, 8, 9, true, true, true));
        assert!(!first_turn_claim_custody(7, 7, 8, 8, false, true, true));
        assert!(!first_turn_claim_custody(7, 7, 8, 8, true, false, true));
        assert!(!first_turn_claim_custody(7, 7, 8, 8, true, true, false));
    }

    #[test]
    fn budget_stamp_cannot_prepare_a_recycled_context_or_domain() {
        assert!(published_budget_identity_custody(4, 8, 12, 4, 8, 12, true));
        assert!(!published_budget_identity_custody(4, 8, 12, 4, 9, 12, true));
        assert!(!published_budget_identity_custody(4, 8, 12, 4, 8, 13, true));
        assert!(!published_budget_identity_custody(
            4, 8, 12, 4, 8, 12, false
        ));
    }

    #[test]
    fn direct_handoff_requires_its_own_exact_nonexecuting_owner() {
        assert!(direct_handoff_frame_custody(9, 9, true, true, true, false));
        assert!(!direct_handoff_frame_custody(
            9, 10, true, true, true, false
        ));
        assert!(!direct_handoff_frame_custody(
            9, 9, false, true, true, false
        ));
        assert!(!direct_handoff_frame_custody(9, 9, true, true, true, true));
    }

    #[test]
    fn terminal_withdraw_waits_only_for_a_claim_that_is_in_flight() {
        // Claimed, not yet published as Current: must wait.
        assert!(terminal_withdraw_must_wait(true, false));
        // Published Current: the normal remote stop proceeds.
        assert!(!terminal_withdraw_must_wait(true, true));
        // Not Running at all (Local, Blocked, ...): nothing is in flight.
        assert!(!terminal_withdraw_must_wait(false, false));
        assert!(!terminal_withdraw_must_wait(false, true));
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn selected_frame_cannot_inherit_a_republished_owner_or_saved_stack() {
        let selected_generation: u64 = kani::any();
        let published_generation: u64 = kani::any();
        let dispatch_rsp: usize = kani::any();
        let published_rsp: usize = kani::any();
        let matched = selected_local_frame_identity(
            selected_generation,
            published_generation,
            dispatch_rsp,
            published_rsp,
        );
        kani::cover!(matched);
        if matched {
            assert_ne!(selected_generation, 0);
            assert_eq!(selected_generation, published_generation);
            assert_ne!(dispatch_rsp, 0);
            assert_eq!(dispatch_rsp, published_rsp);
        }
    }

    #[kani::proof]
    fn unresolved_early_policy_never_releases_a_later_class() {
        let complete: bool = kani::any();
        let peer_selected: bool = kani::any();
        let allow = early_policy_may_continue(complete, peer_selected);
        kani::cover!(allow);
        kani::cover!(!complete && !allow);
        if allow {
            assert!(complete);
            assert!(!peer_selected);
        }
        if !complete || peer_selected {
            assert!(!allow);
        }
    }

    #[kani::proof]
    fn running_frame_geometry_never_inherits_a_distant_generation_or_foreign_cpu() {
        let published: u64 = kani::any();
        let running: u64 = kani::any();
        let exact_current_cpu: bool = kani::any();
        let accepted = super::running_frame_geometry_custody(published, running, exact_current_cpu);
        kani::cover!(accepted);
        if accepted {
            assert!(exact_current_cpu);
            assert!(published == running || published.checked_add(1) == Some(running));
        }
        if !exact_current_cpu || (published != running && published.checked_add(1) != Some(running))
        {
            assert!(!accepted);
        }
    }

    #[kani::proof]
    fn retained_current_frame_requires_exact_published_generation_and_cpu_custody() {
        let published: u64 = kani::any();
        let owner: u64 = kani::any();
        let local_cpu: bool = kani::any();
        let runnable: bool = kani::any();
        let exact_current_cpu: bool = kani::any();
        let admitted = retained_current_frame_custody(
            published,
            owner,
            local_cpu,
            runnable,
            exact_current_cpu,
        );
        kani::cover!(admitted);
        if admitted {
            assert_ne!(published, 0);
            assert_eq!(published, owner);
            assert!(local_cpu && runnable && exact_current_cpu);
        }
        if !exact_current_cpu || !local_cpu || !runnable || published != owner || published == 0 {
            assert!(!admitted);
        }
    }

    #[kani::proof]
    fn retirement_cleanup_ack_never_settles_an_unclaimed_or_foreign_task() {
        let expected: u64 = kani::any();
        let returned: u64 = kani::any();
        let claimed: bool = kani::any();
        let exact_payload: bool = kani::any();
        let admitted = retirement_cleanup_ack_custody(expected, returned, claimed, exact_payload);
        kani::cover!(admitted);
        assert_eq!(admitted, claimed && expected == returned && exact_payload);
        if admitted {
            assert_eq!(expected, returned);
            assert!(claimed && exact_payload);
        }
    }

    #[kani::proof]
    fn republished_reply_token_cannot_supply_a_stable_budget_denial() {
        let admission: Option<bool> = kani::any();
        let token_unchanged: bool = kani::any();
        let checked = budget_admission_after_token_check(admission, token_unchanged);
        kani::cover!(checked == Some(false));
        if checked == Some(false) {
            assert!(token_unchanged);
            assert_eq!(admission, Some(false));
        }
        if !token_unchanged || admission.is_none() {
            assert!(checked.is_none());
        }
    }

    #[kani::proof]
    fn incomplete_affinity_is_never_negative_authority() {
        let lifecycle_dispatchable: bool = kani::any();
        let affinity_admission: Option<bool> = kani::any();
        let rejected = stable_local_admission_rejected(lifecycle_dispatchable, affinity_admission);
        kani::cover!(rejected && lifecycle_dispatchable);
        kani::cover!(!rejected && affinity_admission.is_none());
        if lifecycle_dispatchable && affinity_admission.is_none() {
            assert!(!rejected);
        }
        if rejected && lifecycle_dispatchable {
            assert_eq!(affinity_admission, Some(false));
        }
    }

    #[kani::proof]
    fn accepted_frame_has_exact_generation_local_cpu_and_no_execution_owner() {
        let published_generation: u64 = kani::any();
        let owner_generation: u64 = kani::any();
        let is_local: bool = kani::any();
        let cpu_matches: bool = kani::any();
        let runnable: bool = kani::any();
        let executing: bool = kani::any();
        let allowed = local_frame_custody(
            published_generation,
            owner_generation,
            is_local,
            cpu_matches,
            runnable,
            executing,
        );
        kani::cover!(allowed);
        if allowed {
            assert_eq!(published_generation, owner_generation);
            assert!(is_local && cpu_matches && runnable && !executing);
        }
    }

    #[kani::proof]
    fn stale_generation_and_execution_owner_always_reject() {
        let owner_generation: u64 = kani::any();
        let is_local: bool = kani::any();
        let cpu_matches: bool = kani::any();
        let runnable: bool = kani::any();
        let executing: bool = kani::any();
        let stale = owner_generation.wrapping_add(1);
        kani::cover!(is_local && cpu_matches && runnable && !executing);
        assert!(!local_frame_custody(
            stale,
            owner_generation,
            is_local,
            cpu_matches,
            runnable,
            executing,
        ));
        assert!(!local_frame_custody(
            owner_generation,
            owner_generation,
            is_local,
            cpu_matches,
            runnable,
            true,
        ));
    }

    #[kani::proof]
    fn activation_never_claims_a_reused_child_or_republished_owner() {
        let committed_task_id: u64 = kani::any();
        let live_task_id: u64 = kani::any();
        let selected_generation: u64 = kani::any();
        let owner_generation: u64 = kani::any();
        let is_local: bool = kani::any();
        let cpu_matches: bool = kani::any();
        let runnable: bool = kani::any();
        let admitted = first_turn_claim_custody(
            committed_task_id,
            live_task_id,
            selected_generation,
            owner_generation,
            is_local,
            cpu_matches,
            runnable,
        );
        kani::cover!(admitted);
        if admitted {
            assert_ne!(committed_task_id, 0);
            assert_eq!(committed_task_id, live_task_id);
            assert_eq!(selected_generation, owner_generation);
            assert_ne!(selected_generation, 0);
            assert!(is_local && cpu_matches && runnable);
        }
        if committed_task_id != live_task_id
            || selected_generation != owner_generation
            || !is_local
            || !cpu_matches
            || !runnable
        {
            assert!(!admitted);
        }
    }

    #[kani::proof]
    fn published_budget_never_prepares_a_recycled_cell_or_disallowed_cpu() {
        let slot: u64 = kani::any();
        let context_generation: u64 = kani::any();
        let domain_generation: u64 = kani::any();
        let live_slot: u64 = kani::any();
        let live_context_generation: u64 = kani::any();
        let live_domain_generation: u64 = kani::any();
        let cpu_allowed: bool = kani::any();
        let admitted = published_budget_identity_custody(
            slot,
            context_generation,
            domain_generation,
            live_slot,
            live_context_generation,
            live_domain_generation,
            cpu_allowed,
        );
        kani::cover!(admitted);
        if admitted {
            assert_ne!(slot, 0);
            assert_eq!(slot, live_slot);
            assert_eq!(context_generation, live_context_generation);
            assert_eq!(domain_generation, live_domain_generation);
            assert!(cpu_allowed);
        }
    }

    #[kani::proof]
    fn accepted_direct_handoff_has_exact_generation_and_no_execution_owner() {
        let published_generation: u64 = kani::any();
        let owner_generation: u64 = kani::any();
        let is_direct_handoff: bool = kani::any();
        let cpu_matches: bool = kani::any();
        let runnable: bool = kani::any();
        let executing: bool = kani::any();
        let accepted = direct_handoff_frame_custody(
            published_generation,
            owner_generation,
            is_direct_handoff,
            cpu_matches,
            runnable,
            executing,
        );
        kani::cover!(accepted);
        if accepted {
            assert_eq!(published_generation, owner_generation);
            assert!(is_direct_handoff && cpu_matches && runnable && !executing);
        }
    }

    #[kani::proof]
    fn terminal_withdraw_never_retires_an_in_flight_claim_and_never_blocks_a_published_stop() {
        let owner_is_running: bool = kani::any();
        let role_published: bool = kani::any();
        let must_wait = terminal_withdraw_must_wait(owner_is_running, role_published);
        kani::cover!(must_wait);
        kani::cover!(!must_wait && owner_is_running);
        // A claimed-but-unpublished slot is never retired under its claimer.
        if owner_is_running && !role_published {
            assert!(must_wait);
        }
        // Waiting is never required once the slot is published or not Running,
        // so a remote stop of an executing or queued task is not starved.
        if !owner_is_running || role_published {
            assert!(!must_wait);
        }
    }
}
