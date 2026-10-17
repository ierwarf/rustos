//! Owner: two original CPU return banks, frozen exec census and shared old MM.
//! Boundary: actual remote wake/claim/return, sibling stop/drain, self exec and cleanup.
//! Lifecycle: born shared-MM sibling executes remotely, exits by real return, then exec finalizes.
//! Concurrency: native actor interleavings retain ORIGINAL parked banks on each CPU.
//! Failure: neither terminal publication nor incoming RSP arrival alone acknowledges census drain.
//! Forbidden: native CPU identity, roots, register effects or RSP arrival as hardware evidence.
//! Evidence: one production dispatcher/return/census/image/terminal pipeline; modeled architecture only.
use super::*;
use nucleus_core::util::lockdep::{DispatchPreemption, with_native_cpu};
use crate::multitask::process_placement::exec_membership::ExecMembership;
const EXECUTOR: usize = 120;
const EXECUTOR_ID: u64 = ID + 20;
const NEW_ROOT: u64 = 0x9000;

fn cpu_scope() -> DispatchPreemption<'static> {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    scope
}
fn install(entry: &mut DispatchEntry, dispatch: SchedulerDispatch, fast: bool) {
    let effects = install_model(entry, dispatch, fast);
    assert!(effects.len() >= 4);
    assert_eq!(effects.last(), Some(&return_architecture::ArchitectureEffect::Simd(dispatch.next_slot)));
}
fn census_pending(scope: &DispatchPreemption, intent: &current_identity::exec_intent::Intent,
    census: &ExecMembership, sibling: usize) {
    assert_eq!(exec_transaction::quiescence::quiesce(scope, intent, census, &|_| 3), Some(false));
    assert!(current_identity::lifecycle(sibling).retired);
    assert_eq!(retirement_cleanup::test_consumer_publication(sibling), (false, false));
    assert!(stack_resources::reuse::begin_retired(sibling, ID + 1).is_none());
}

// Complete through the SAME actual kernel Executor, exact runtime ACK and
// physical/resource reaper, not a footer that manufactures a kernel ACK or
// reimplements metadata teardown. Only the external runtime's ACK arrival is
// an input; its exact original cleanup payload and all effects are real.
fn complete_original_retired(slot: usize, id: u64,
    replay: Option<crate::multitask::RetiredTaskCleanup>) -> crate::multitask::RetiredTaskCleanup {
    let executor = retirement_cleanup::executor::capture_current().unwrap();
    if retirement_cleanup::kernel_pending(slot) {
        retirement_cleanup::claim_next_kernel(&executor).unwrap().complete(|task| {
            let _ = wake_publication::wake_task(task);
        });
    }
    assert!(!retirement_cleanup::kernel_pending(slot));
    let cleanup = retirement_cleanup::claim_next().unwrap();
    assert_eq!(cleanup.task_id(), id);
    if let Some(old) = replay {
        // The NEW runtime row is already Claimed: phase alone cannot mask a
        // broken old-incarnation payload check. Replayed ABI data is no loan.
        assert_ne!(old.task_id(), cleanup.task_id());
        assert!(!retirement_cleanup::complete(old));
    }
    assert!(retirement_cleanup::complete(cleanup));
    let reclaim = reaper::claim_next(&executor).unwrap();
    assert!(stack_resources::context::read(slot).is_none());
    reclaim.complete();
    assert!(current_identity::read(slot).is_none());
    cleanup
}

#[test]
fn actual_foreign_shared_mm_returns_drain_frozen_exec_census_before_self_exec_and_terminal_reuse() {
    let policy = crate::multitask::SchedulingContextAdmission { cpu_mask: 3, ..root_creation::tests::policy() };
    // Independently owned initial user MM differs from BOTH real kernel
    // executor roots; source+sibling share this original backing from birth.
    with_peer_backing(true, false, policy, root_creation::tests::state_with_root(0x8000), |source, sibling| {
        let sibling = sibling.unwrap();
        let handle = stack_resources::context::read(source).unwrap().process_handle.unwrap();
        assert_eq!(stack_resources::context::read(sibling).unwrap().process_handle, Some(handle));
        execution_interval::test_open_captured(source, 2);
        // The second earlier kernel executor is actually constructed. ONLY its
        // earlier CPU admission is a fixture input, just like CPU0's bootstrap.
        with_native_cpu(1, || {
            let scope = cpu_scope();
            let ticket = root_slots::reserve(EXECUTOR, EXECUTOR_ID).unwrap();
            kernel_creation::publish_reserved(ticket, |_| {}, 119, 8, 16, 0x202,
                0x1000, 0x4000, 0, |_, _| {}, |_| 2);
            drop(scope);
        });
        let token = frame_publication::test_read(EXECUTOR).unwrap();
        let pin = frame_publication::pin_for_identity(token, EXECUTOR_ID).unwrap();
        let admission = install_test_current_owner(1, EXECUTOR);
        current_identity::execution::banks::initialize(1,
            current_identity::execution::initial(EXECUTOR, EXECUTOR_ID).unwrap());
        stack_resources::cpu_leases::with_test_bank(1, pin, false, || {
            with_native_cpu(1, || {
                runqueue::test_running_claim(EXECUTOR, 1, runqueue::weight::value(EXECUTOR));
                execution_interval::test_open_captured(EXECUTOR, 2);
            });
            // A real blocked->remote-mailbox producer moves the born sibling.
            // No incoming bank or successful claim is manufactured for it.
            let scope = cpu_scope();
            runqueue::wait::set_blocked(sibling, true);
            runqueue::publish_blocked(sibling, 0, runqueue::weight::value(sibling));
            frame_publication::bind_local_owner_generation(sibling, runqueue::owner(sibling).generation);
            runqueue::affinity_payload::set_affinity(sibling, 2, 3, false);
            assert!(wake_publication::wake(&scope, ID + 1, &|_| 2));
            assert_eq!(runqueue::owner(sibling).cpu, Some(1));
            drop(scope);
            with_native_cpu(1, || {
                let mut entry = DispatchEntry::from_scope(cpu_scope());
                // Explicit earlier executor wait, not an absent bitmap fallback.
                runqueue::wait::set_blocked(EXECUTOR, true);
                runqueue::set_runnable(EXECUTOR, false);
                let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(EXECUTOR), true, None);
                assert_eq!(dispatch.next_slot, sibling);
                install(&mut entry, dispatch, fast);
                drop(entry);
                assert_eq!(task_execution_owner(EXECUTOR), Some(TaskExecutionOwner::Transition(1)));
                assert!(stack_resources::cpu_leases::owns_current(1, sibling));
                assert!(current_identity::execution::banks::owns_current(1, sibling));
            });
            // Change effective charge metadata AFTER CPU1's successful original
            // claim, while its RSP ACK is pending. Actual original interval
            // funding remains the sibling's OWN context through later terminal
            // withdrawal; it must never be reconstructed from this new donor.
            let scope = cpu_scope();
            assert!(donation_publication::bind_with(&scope, 0x7f21, ID, ID + 1, true));
            assert_eq!(donation_ledger::borrowed_context_charge_token(sibling), Some((source, 0x7f21)));
            drop(scope);
            // CPU1 is PARKED awaiting its incoming RSP. CPU0's original Current
            // and MM/funding still execute; it freezes real membership and stops
            // the sibling, but may not infer quiescence from terminal metadata.
            let reservation = process_table::begin_exec(handle).unwrap();
            let intent = current_identity::exec_intent::begin(source, ID, reservation, true).unwrap();
            let census = ExecMembership::begin(&intent).unwrap();
            assert!(census.members().contains(&(sibling, ID + 1)));
            let scope = cpu_scope();
            census_pending(&scope, &intent, &census, sibling);
            drop(scope);
            with_native_cpu(1, || {
                commit_context_switch();
                assert_eq!(task_execution_owner(EXECUTOR), None);
                assert_eq!(task_execution_owner(sibling), Some(TaskExecutionOwner::Current(1)));
                let scope = cpu_scope();
                let original = current_terminal::OriginalCurrent::borrow(&scope).unwrap();
                let snapshot = original.runtime_snapshot().unwrap();
                assert_eq!((snapshot.executing_task_id, snapshot.context_owner_task_id), (ID + 1, ID + 1));
                assert_eq!(donation_ledger::borrowed_context_charge_token(sibling), Some((source, 0x7f21)));
            });
            let scope = cpu_scope();
            census_pending(&scope, &intent, &census, sibling);
            drop(scope);
            with_native_cpu(1, || {
                let mut entry = DispatchEntry::from_scope(cpu_scope());
                assert!(wake_publication::wake(entry.preemption(), EXECUTOR_ID, &|_| 2));
                let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(sibling), true, None);
                assert_eq!(dispatch.next_slot, EXECUTOR);
                install(&mut entry, dispatch, fast);
                drop(entry);
                assert_eq!(task_execution_owner(sibling), Some(TaskExecutionOwner::Transition(1)));
                assert_ne!(current_identity::execution::count(sibling), 0);
            });
            let scope = cpu_scope();
            census_pending(&scope, &intent, &census, sibling);
            drop(scope);
            // The SAME unique CPU1 callback drains physical/MM AND UID outgoing
            // custody. Only NOW can CPU0 complete original deferred detach.
            with_native_cpu(1, || {
                commit_context_switch();
                commit_context_switch();
                assert_eq!(task_execution_owner(sibling), None);
                assert_eq!(current_identity::execution::count(sibling), 0);
                assert!(stack_resources::cpu_leases::owns_current(1, EXECUTOR));
            });
            let scope = cpu_scope();
            assert_eq!(exec_transaction::quiescence::quiesce(&scope, &intent, &census, &|_| 3), Some(true));
            assert_eq!(retirement_cleanup::test_consumer_publication(sibling), (true, true));
            assert_eq!(donation_ledger::borrowed_context_charge_token(sibling), None);
            assert_eq!(stack_resources::context::read(sibling).unwrap().process_handle, None);
            drop(scope);
            // The actual original kernel executor consumes the SAME detach
            // payload before the process may authorize exec. Only RTC/IPC
            // external completion is modeled; process detach and row ACK are real.
            with_native_cpu(1, || {
                let executor = retirement_cleanup::executor::capture_current().unwrap();
                let claim = retirement_cleanup::claim_next_kernel(&executor).unwrap();
                claim.complete_modeled_external(|effects| {
                    assert_eq!(effects.test_detach_payload(), Some(handle));
                    assert!(stack_resources::cpu_leases::owns_current(1, EXECUTOR));
                    assert!(retirement_cleanup::kernel_pending(sibling));
                    assert_eq!(process_table::thread_count_by_pid(ID), Some(2));
                });
                assert!(!retirement_cleanup::kernel_pending(sibling));
            });
            assert!(process_table::authorize_exec(reservation));
            let finalized = exec_transaction::with_census_image(&intent, &census,
                reservation, NEW_ROOT, true, |lease| {
                    let staged = process_table::stage_exec_state(reservation,
                        crate::memory::address_space::ProcessAddressSpace::modeled_root_for_host(NEW_ROOT),
                        crate::user::linux::LinuxProcessState::default(),
                        crate::user::linux::LinuxMemoryMapState::new(),
                        crate::user::linux::LinuxRuntimeProfile::new(), "/joint-exec.elf").unwrap();
                    census.mark_staged();
                    lease.bind_staged_mm(&staged);
                    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
                    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x33000);
                    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x66000);
                    let published = with_exec_image_turn(cpu_scope(), lease, |turn| {
                        assert!(stack_resources::cpu_leases::owns_current(0, source));
                        assert!(stack_resources::cpu_leases::owns_current(1, EXECUTOR));
                        exec_transaction::publication::commit(lease, turn, &mut boot, None).unwrap()
                    }).unwrap();
                    Some(process_table::finalize_exec_state(staged, published))
                }).unwrap().unwrap();
            census.finish(&finalized);
            let (_, _, _, old) = finalized.into_parts();
            old.retire_execution_state();
            assert!(!current_identity::exec_intent::busy(source));
            assert_eq!(runqueue::address_space::root(source), NEW_ROOT);
            with_native_cpu(1, || complete_original_retired(sibling, ID + 1, None));
            // The remaining born Current also leaves through the actual next
            // complete return before cleanup; fixture bank teardown cannot ACK it.
            let mut entry = DispatchEntry::from_scope(cpu_scope());
            let kernel_id = stack_resources::task_start::read(121).unwrap().id;
            assert!(wake_publication::wake(entry.preemption(), kernel_id, &|_| 1));
            let stop = terminal_quarantine::begin(source, Some(ID), TaskRetireReason::Exited, false).unwrap();
            // SAFETY: original masked CPU0 Current and original interval survive every effect.
            unsafe { terminal_publication::guarded(stop, &|_| 3); }
            let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(source), true, None);
            assert_eq!(dispatch.next_slot, 121);
            install(&mut entry, dispatch, fast);
            drop(entry);
            commit_context_switch();
            let old_cleanup = complete_original_retired(source, ID, None);
            assert!(stack_resources::context::read(source).is_none());
            assert!(stack_resources::context::read(sibling).is_none());
            // SAME namespace and both actual kernel Current/UID/interval banks
            // span a new independently constructed incarnation after shared-MM
            // exec, dual RSP drain, terminal effects and both cleanup ACKs.
            // No bank teardown or scalar generation rewrite authorizes rebirth.
            let masked = cpu_scope();
            let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
            boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x3000);
            boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x6000);
            let (reborn, reborn_handle) = root_creation::tests::active_independent_root_with_scope(
                ID + 2, boot, NEW_ROOT + 0x4000, &masked);
            assert_eq!(reborn, source);
            assert_ne!(reborn_handle, handle);
            assert!(wake_publication::Binding::capture(reborn, ID).is_none());
            assert!(wake_publication::Binding::capture(reborn, ID + 2).is_some());
            assert!(stack_resources::cpu_leases::owns_current(0, 121));
            assert!(current_identity::execution::banks::owns_current(0, 121));
            assert!(stack_resources::cpu_leases::owns_current(1, EXECUTOR));
            assert!(current_identity::execution::banks::owns_current(1, EXECUTOR));
            let stop = terminal_quarantine::begin(reborn, Some(ID + 2), TaskRetireReason::Exited, false).unwrap();
            // SAFETY: original masked kernel Current survives every new effect.
            unsafe { terminal_publication::guarded(stop, &|_| 1); }
            drop(masked); // actual kernel Executor, not dispatch scope, owns external work
            complete_original_retired(reborn, ID + 2, Some(old_cleanup));
            assert!(stack_resources::context::read(reborn).is_none());
            for (cpu, slot, id) in [(0, 121, kernel_id), (1, EXECUTOR, EXECUTOR_ID)] {
                with_native_cpu(cpu, || {
                    assert!(matches!(execution_interval::consume(cpu, slot, id), execution_interval::ConsumedInterval::Active(_)));
                });
            }
        });
        drop(admission);
        kernel_creation::tests::cleanup(EXECUTOR);
    }, |slot, _, _| {
        assert!(stack_resources::context::read(slot).is_none());
        assert!(current_identity::read(slot).is_none());
    }, true);
}
