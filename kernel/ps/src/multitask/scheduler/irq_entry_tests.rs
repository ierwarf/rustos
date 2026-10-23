//! Owner: actual born allocation and original independent IRQ turn witnesses.
//! Boundary: production entry, original Current probe/SIMD/claim and publication.
//! Lifecycle: real banks and funding survive return acceptance; scratch is no grant.
//! Concurrency: masked own CPU, terminal intent and one original interval.
//! Failure: unmasked admission and blocked intent refuse/defer without re-grant.
//! Forbidden: hardware claims from native boot/IRQ/architecture/RSP model inputs.
//! Evidence: real production paths; privileged install and RSP ACK are modeled.
use super::*;
use crate::multitask::scheduler;
use crate::multitask::cpu_local::*;
use crate::multitask::cpu_local::{dispatch_turn, irq_entry::DispatchEntry};
use crate::user::abi::UserAbi;
const ID: u64 = 130900;
mod cross_root;
mod foreign_shared;
mod lane;
mod switch;
pub(in crate::multitask) fn current(f: impl FnOnce(usize)) {
    current_with_peer(false, |slot, _| f(slot));
}
pub(super) fn current_with_peer(peer: bool, f: impl FnOnce(usize, Option<usize>)) {
    with_peer_release(peer, f, thread_creation::tests::release_born, false);
}
fn with_peer_release(peer: bool, f: impl FnOnce(usize, Option<usize>), release: impl Fn(usize, u64, process_table::ProcessHandle), root_first: bool) {
    with_peer_kind(peer, false, f, release, root_first);
}
fn with_peer_kind(peer: bool, independent: bool, f: impl FnOnce(usize, Option<usize>), release: impl Fn(usize, u64, process_table::ProcessHandle), root_first: bool) {
    with_peer_admission(peer, independent, root_creation::tests::policy(), f, release, root_first);
}
fn with_peer_admission(peer: bool, independent: bool, policy: crate::multitask::SchedulingContextAdmission,
    f: impl FnOnce(usize, Option<usize>), release: impl Fn(usize, u64, process_table::ProcessHandle), root_first: bool) {
    with_peer_backing(peer, independent, policy, root_creation::tests::state_with_root(0x4000), f, release, root_first);
}

// The original owned MM input precedes birth; never rewrite a live scalar root
// to make the actual reaper's shared-active-MM exclusion pass.
fn with_peer_backing(peer: bool, independent: bool, policy: crate::multitask::SchedulingContextAdmission,
    input: UserProcessState, f: impl FnOnce(usize, Option<usize>),
    release: impl Fn(usize, u64, process_table::ProcessHandle), root_first: bool) {
    let child = core::cell::Cell::new(None);
    scheduler::root_creation::tests::exec_root_with_backing(ID, true,
        scheduler::root_creation::tests::bootstrap(UserAbi::Linux), policy, input, |slot, handle| {
        // ORDERING: explicit native earlier bootstrap ACK, not backing admission.
        with_initialized_fixture(|| {
        let masks = runqueue::affinity_payload::affinity_snapshot(121);
        runqueue::affinity_payload::set_affinity(121, 1, 1, false);
        if !independent {
            // Earlier executor wait is an explicit native input. The actual
            // bitmap, not an all-slots scan failure, owns fair membership.
            runqueue::test_blocked_return(121, 0, runqueue::weight::value(121));
        }
        let rsp = runqueue::saved_rsp(slot);
        // SAFETY: fixture owns the actual born stack before any reader opens;
        // model the earlier complete admitted user IRQ frame, not its lifetime.
        unsafe {
            let frame = &mut *(rsp as *mut crate::multitask::context::SavedContext);
            frame.rip = crate::memory::paging::USER_SPACE_BASE + 0x1000;
            frame.rsp = crate::memory::paging::USER_SPACE_BASE + 0x4000;
        }
        let _bank = crate::multitask::current_identity::execution::banks::isolate(0);
        crate::multitask::current_identity::execution::banks::initialize(0,
            crate::multitask::current_identity::execution::initial(slot, ID).unwrap());
        runqueue::test_running_claim(slot, 0, runqueue::weight::value(slot));
        let peer = peer.then(|| {
            let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
            boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x2000);
            boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x5000);
            let (receiver, child_handle) = if independent {
                root_creation::tests::active_independent_root(ID + 1, boot, 0x8000)
            } else {
                (thread_creation::tests::active_sibling(slot, ID + 1, boot), handle)
            };
            child.set(Some((receiver, child_handle)));
            receiver
        });
        f(slot, peer);
        runqueue::affinity_payload::set_affinity(121, masks.0, masks.1, masks.2);
        });
    }, |slot, id, handle| {
        if root_first { release(slot, id, handle); }
        if let Some((receiver, child_handle)) = child.get() {
            release(receiver, ID + 1, child_handle);
        }
        if !root_first { release(slot, id, handle); }
    });
}
pub(super) fn scope() -> nucleus_core::util::lockdep::DispatchPreemption<'static> {
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model_with_enabled_irqs(0);
    scope.mask_irqs();
    scope
}
/// Same architecture algorithm, with only register/SIMD effects intercepted.
/// Every effect still borrows the original successful claim and BOTH banks.
fn install_model(entry: &mut DispatchEntry, dispatch: SchedulerDispatch, fast: bool) -> std::vec::Vec<return_architecture::ArchitectureEffect> {
    let turn = entry.turn();
    let (cpu, from) = turn.original_current().unwrap();
    let to = dispatch.next_slot;
    let mut effects = std::vec::Vec::new();
    return_architecture::ReturnArchitecture::capture(turn, dispatch).unwrap().install_model(fast, |effect| {
        assert_eq!(turn.original_current(), Some((cpu, from)));
        assert!(execution_interval::original_claim_is_active(turn));
        assert!(stack_resources::cpu_leases::owns_current(cpu, from));
        assert!(stack_resources::cpu_leases::owns_prepared(cpu, to));
        assert!(current_identity::execution::banks::owns_execution(cpu, to));
        assert!(!turn.architecture_ready(), "ready must follow the LAST effect");
        effects.push(effect);
    });
    assert!(turn.architecture_ready());
    assert!(return_architecture::ReturnArchitecture::capture(turn, dispatch).is_none());
    assert_eq!(effects.last(), Some(&return_architecture::ArchitectureEffect::Simd(to)));
    // Fault ONLY telemetry immediately before the actual final consumer. A
    // prior acceptance would mask a forbidden scratch-based destination.
    set_scheduler_current_task_scratch_for_cpu(cpu, 121);
    let returns_to_user = turn.claimed_return().unwrap().returns_to_user();
    assert_eq!(entry.accept_installed_return(), returns_to_user);
    effects
}
#[test]
fn actual_irq_control_probe_keeps_original_current_bank_without_directory() {
    current(|slot| {
        let refs = crate::multitask::current_identity::execution::count(slot);
        let entry = DispatchEntry::from_scope(scope());
        assert_eq!(entry.turn().original_current(), Some((0, slot)));
        assert!(entry.kernel_continuation(crate::arch::gdt::kernel_code_selector().0 as u64));
        assert!(!entry.kernel_continuation(crate::arch::gdt::user_code_selector().0 as u64));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        runqueue::set_runnable(slot, false);
        assert!(!entry.kernel_continuation(crate::arch::gdt::kernel_code_selector().0 as u64));
        runqueue::set_runnable(slot, true);
        assert!(entry.turn().claimed_return().is_none());
        drop(entry);
        assert_eq!(published_current_slot(0), Some(slot));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert_eq!(crate::multitask::current_identity::execution::count(slot), refs);
    });
}
#[test]
fn actual_irq_unmasked_scope_refuses_before_current_or_frame_effects() {
    current(|slot| {
        let owner = runqueue::owner(slot);
        let rsp = runqueue::saved_rsp(slot);
        let old_phase = dispatch_turn::test_phase(0);
        let failed = std::panic::catch_unwind(|| {
            DispatchEntry::from_scope(nucleus_core::util::lockdep::DispatchPreemption::enter_model_with_enabled_irqs(0))
        });
        assert!(failed.is_err());
        assert_eq!(dispatch_turn::test_phase(0), old_phase);
        assert_eq!(runqueue::owner(slot), owner);
        assert_eq!(runqueue::saved_rsp(slot), rsp);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
    });
}
#[test]
fn actual_irq_fast_producer_claim_and_changed_return_keep_both_born_banks_until_rsp_ack() {
    current_with_peer(true, |slot, peer| {
        let hardware_arrivals = rsp_handoff::hardware_rsp_arrivals(0);
        let peer = peer.unwrap();
        runqueue::wait::set_reason(peer, runqueue::wait::REASON_ENDPOINT_RECEIVE, 131000);
        runqueue::wait::set_blocked(peer, true);
        runqueue::publish_blocked(peer, 0, runqueue::weight::value(peer));
        frame_publication::bind_local_owner_generation(peer, runqueue::owner(peer).generation);
        execution_interval::test_open_captured(slot, 2);
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        assert!(donation_publication::reserve_call_with(entry.preemption(), ID).donation_reserved);
        // SAFETY: this original masked turn retains the actual Current bank
        // throughout the typed arm and producer commit.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReply(131001)));
        drop(wait);
        assert_eq!(ipc_publication::call(entry.preemption(), 131000, 131001, ID + 1),
            FastIpcCallHandoffOutcome::CommittedSameCpu);
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(slot), true, Some(ID + 1));
        assert!(fast);
        assert_eq!(dispatch.next_slot, peer);
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(stack_resources::cpu_leases::owns_prepared(0, peer));
        assert!(return_architecture::ReturnArchitecture::capture(entry.turn(), dispatch).is_some());
        // Execute the SAME architecture state machine. Only privileged effects
        // are intercepted, never replaced with a phase-ready fixture input.
        let effects = install_model(&mut entry, dispatch, fast);
        assert_eq!(effects.len(), 4);
        assert!(matches!(effects[0], return_architecture::ArchitectureEffect::PrivilegeStack(_)));
        assert!(matches!(effects[1], return_architecture::ArchitectureEffect::SyscallStack(_, _)));
        assert!(matches!(effects[2], return_architecture::ArchitectureEffect::Segments(_, true)));
        set_scheduler_current_task_scratch_for_cpu(0, 121);
        drop(entry);
        assert_eq!(published_current_slot(0), Some(peer));
        assert_eq!(task_execution_owner(slot), Some(TaskExecutionOwner::Transition(0)));
        assert!(current_identity::execution::count(slot) != 0);
        assert!(current_identity::execution::banks::owns_current(0, peer));
        assert_eq!(dispatch_turn::test_phase(0), 3, "original scope must park until incoming RSP");
        assert!(stack_resources::cpu_leases::owns_current(0, peer));
        assert!(!stack_resources::cpu_leases::owns_prepared(0, peer));
        // Actual callback algorithm, with invalid machine observations supplied
        // explicitly by this native model. Neither original bank may be ACKed.
        for wrong_rsp in [0, runqueue::saved_rsp(slot), usize::MAX] {
            let rejected = std::panic::catch_unwind(|| {
                commit_context_switch_at_rsp(0, Some(wrong_rsp));
            });
            assert!(rejected.is_err(), "old/foreign RSP must fail before lifetime ACK");
            assert_eq!(dispatch_turn::test_phase(0), 4, "refusal quarantines the turn");
            assert!(stack_resources::cpu_leases::owns_outgoing(0, slot));
            assert!(stack_resources::cpu_leases::owns_current(0, peer));
            assert!(current_identity::execution::banks::owns_outgoing(0, slot));
            assert!(current_identity::execution::banks::owns_current(0, peer));
            assert_eq!(task_execution_owner(slot), Some(TaskExecutionOwner::Transition(0)));
            // Isolated model retry retains the SAME parked MM/placement/scope
            // and seal. Production has no poison recovery.
            dispatch_turn::test_retry_refused_arrival(0);
        }
        // Only arrival on incoming RSP is an external native input. The actual
        // ACK callback now drains the real outgoing allocation/UID banks.
        commit_context_switch();
        assert!(stack_resources::cpu_leases::owns_current(0, peer));
        assert!(task_execution_owner(slot).is_none());
        assert_eq!(dispatch_turn::test_phase(0), 0);
        assert!(matches!(execution_interval::consume(0, peer, ID + 1), execution_interval::ConsumedInterval::Active(_)));
        assert_eq!(rsp_handoff::hardware_rsp_arrivals(0), hardware_arrivals,
            "modeled native RSP must never increase hardware execution evidence");
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        sync_handoff::remove_slot_all_cpus(peer);
        set_scheduler_current_task_scratch_for_cpu(0, peer);
    });
}
#[test]
fn actual_dispatch_return_terminal_and_rsp_ack_keep_original_banks_until_real_reuse_seals() {
    with_peer_release(true, |slot, peer| {
        let peer = peer.unwrap();
        runqueue::wait::set_reason(peer, runqueue::wait::REASON_ENDPOINT_RECEIVE, 131010);
        runqueue::wait::set_blocked(peer, true);
        runqueue::publish_blocked(peer, 0, runqueue::weight::value(peer));
        frame_publication::bind_local_owner_generation(peer, runqueue::owner(peer).generation);
        execution_interval::test_open_captured(slot, 2);
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        assert!(donation_publication::reserve_call_with(entry.preemption(), ID).donation_reserved);
        // SAFETY: this original masked Current owns both real physical banks.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReply(131011)));
        drop(wait);
        assert_eq!(ipc_publication::call(entry.preemption(), 131010, 131011, ID + 1), FastIpcCallHandoffOutcome::CommittedSameCpu);
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(slot), true, Some(ID + 1));
        assert!(fast);
        assert_eq!(dispatch.next_slot, peer);
        install_model(&mut entry, dispatch, fast);
        drop(entry);
        assert_eq!(task_execution_owner(slot), Some(TaskExecutionOwner::Transition(0)));
        assert!(stack_resources::cpu_leases::owns_current(0, peer));
        assert!(current_identity::execution::count(slot) != 0);
        let stop = terminal_quarantine::begin(slot, Some(ID), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: original same-CPU mask is parked across AwaitingRSP. The
        // terminal owner must defer until BOTH original outgoing banks drain.
        unsafe { terminal_publication::guarded(stop, &|_| 1); }
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (false, false));
        assert_eq!(retirement_cleanup::deferred(slot), Some(TaskRetireReason::Exited));
        assert!(wake_publication::Binding::capture(slot, ID).is_none());
        assert!(stack_resources::reuse::begin_retired(slot, ID).is_none());
        assert!(stack_resources::context::read(slot).is_some());
        // Native external input is ONLY arrival on incoming RSP; this is the
        // real callback, not fixture physical publish/commit or row reset.
        commit_context_switch();
        assert_eq!(task_execution_owner(slot), None);
        assert_eq!(current_identity::execution::count(slot), 0);
        assert_eq!(dispatch_turn::test_phase(0), 0);
        let masked = scope();
        // SAFETY: new same-CPU masked scope drains the retained original stop;
        // no source UID, context or execution grant is reacquired.
        unsafe { terminal_publication::drain_guarded(&|_| 1); }
        assert_eq!(retirement_cleanup::deferred(slot), None);
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
        assert!(retirement_cleanup::reuse::begin(slot, Some(ID)).is_none(),
            "original reset seal must refuse until external/kernel cleanup ACK");
        let execution_interval::ConsumedInterval::Active(interval) = execution_interval::consume(0, peer, ID + 1)
            else { panic!("original incoming funding lost before settlement"); };
        assert_eq!(interval.charge(100, 1).unwrap().0, ID,
            "revoked donation still settles its SAME original pinned funder");
        assert!(matches!(execution_interval::consume(0, peer, ID + 1), execution_interval::ConsumedInterval::AlreadySettled));
        let stop = terminal_quarantine::begin(peer, Some(ID + 1), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: SAME physical/UID Current remains banked through all effects.
        unsafe { terminal_publication::guarded(stop, &|_| 1); }
        assert_eq!(retirement_cleanup::test_consumer_publication(peer), (true, true));
        assert!(stack_resources::reuse::begin_retired(peer, ID + 1).is_none());
        drop(masked);
        // Duplicate RSP callback cannot free the still-Current incoming bank.
        commit_context_switch();
        assert!(stack_resources::cpu_leases::owns_current(0, peer));
        sync_handoff::remove_slot_all_cpus(peer);
    }, |slot, id, handle| {
        // Explicit external cleanup ACK only AFTER actual CPU banks drop.
        // Original consumer/reset/resource/MM seals perform full real reuse.
        terminal_publication::tests::release_published(slot, id, handle);
        assert!(stack_resources::context::read(slot).is_none());
        assert!(current_identity::read(slot).is_none());
    }, true);
}

#[test]
fn actual_native_resource_publication_refuses_missing_architecture_or_settled_original_interval() {
    for settled in [false, true] {
        current(|slot| {
            execution_interval::test_open_captured(slot, 2);
            let mut entry = DispatchEntry::from_scope(scope());
            let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(slot), false, None);
            assert_eq!(dispatch.next_slot, slot);
            if settled {
                install_model(&mut entry, dispatch, fast);
                assert!(matches!(execution_interval::consume(0, slot, ID), execution_interval::ConsumedInterval::Active(_)));
            }
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(entry)));
            assert!(failed.is_err(), "real native turns must execute production resource checks");
            assert_eq!(dispatch_turn::test_phase(0), 4);
            assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
            assert!(stack_resources::cpu_leases::owns_current(0, slot));
            assert_eq!(published_current_slot(0), Some(slot));
            // Explicit failed-fixture cleanup, NOT an architectural return.
            if !settled { assert!(matches!(execution_interval::consume(0, slot, ID), execution_interval::ConsumedInterval::Active(_))); }
            stack_resources::cpu_leases::publish(0, slot, slot);
            stack_resources::cpu_leases::commit(0);
        });
    }
}
#[test]
fn actual_irq_diagnostic_compares_original_physical_bank_and_uid_permit() {
    current(|slot| {
        use crate::multitask::run_authority::{self, Mismatch};
        let _ = run_authority::take_window();
        let entry = DispatchEntry::from_scope(scope());
        runtime_profile::observe_original_current(entry.turn());
        assert!(run_authority::take_window().is_none());
        // Keep the real physical bank; model a lost UID permit separately.
        {
            let _missing = current_identity::execution::banks::isolate(0);
            runtime_profile::observe_original_current(entry.turn());
            let (first, count) = run_authority::take_window().unwrap();
            assert_eq!(count, 1);
            assert_eq!(first & 0xffff, Mismatch::CurrentPermitMissing as u64);
            assert_eq!((first >> 32) & 0xffff, slot as u64);
        }
        drop(entry);
    });
}
#[test]
fn actual_irq_dispatch_uses_original_claim_interval_and_ignores_return_scratch() {
    current(|slot| {
        execution_interval::test_open_captured(slot, 2);
        let mut entry = DispatchEntry::from_scope(scope());
        let rsp = runqueue::saved_rsp(slot);
        // Actual physical SIMD save precedes producer intent; fallback must
        // not re-save a later kernel image over the original user's image.
        entry.save_simd();
        let (dispatch, fast) = entry.dispatch(rsp, true, Some(ID + 99));
        entry.save_simd();
        assert!(!fast);
        assert_eq!(dispatch.next_slot, slot);
        assert_eq!(entry.turn().claimed_return().unwrap().id, ID);
        assert!(entry.turn().owns_priority_choice());
        assert!(execution_interval::original_claim_is_active(entry.turn()));
        assert!(stack_resources::cpu_leases::owns_prepared(0, slot));
        assert!(scheduler::return_architecture::ReturnArchitecture::capture(entry.turn(), dispatch).is_some());
        let effects = install_model(&mut entry, dispatch, fast);
        assert_eq!(effects, [return_architecture::ArchitectureEffect::Simd(slot)],
            "same Current must restore SIMD without replaying CR3/segments/stack");
        set_scheduler_current_task_scratch_for_cpu(0, 121);
        assert!(execution_interval::original_claim_is_active(entry.turn()));
        drop(entry);
        assert_eq!(published_current_slot(0), Some(slot), "scratch cannot replace claimed return custody");
        assert!(matches!(execution_interval::consume(0, slot, ID), execution_interval::ConsumedInterval::Active(_)));
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(!stack_resources::cpu_leases::owns_prepared(0, slot));
        assert_eq!(dispatch_turn::test_phase(0), 0, "same Current needs no RSP ACK");
        set_scheduler_current_task_scratch_for_cpu(0, slot);
    });
}
