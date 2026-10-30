//! Owner: actual independent root/fork construction and sealed process birth witnesses.
//! Boundary: real Ticket/Join/spawn/domain/actor/bank owners, modeled MM/topology inputs.
//! Lifecycle: invisible prepared birth, fallible rollback, pinned visibility, retained ACK/wake.
//! Concurrency: canonical native isolation; actual actor Current bank and parent Cohort.
//! Failure: stale copied receipts, nonce exhaustion and visible abandonment refuse.
//! Forbidden: modeled Current or fixture terminal ACK as assembly/physical teardown proof.
//! Evidence: shared constructors and real process/resource/queue state, not joint hardware proof.
use super::*;
const KERNEL: usize = 121;
const KID: u64 = 121701;
const ID: u64 = 120701;
/// Three independently born process/context/domain owners for nested IPC.
/// The kernel actor and earlier running queue are native inputs, not RSP ACK.
/// Each real constructor owns its admission and backing through final release.
pub(in crate::multitask::scheduler) fn donation_roots(
    first_id: u64,
    f: impl FnOnce([usize; 3], &nucleus_core::util::lockdep::DispatchPreemption),
) {
    kernel(|actor, turn| {
        let mut roots = Vec::new();
        for i in 0..3 {
            let mut p = policy();
            p.budget_ns = if i == 0 { 4000 } else { 7000 };
            p.domain = if i == 0 { 0x41 } else { 0x42 };
            p.criticality = if i == 0 { 2 } else { 1 };
            let admission = Admission::capture(actor, None, Some(p)).unwrap();
            roots.push(construct_child(actor, &admission, turn,
                first_id + i, None, false, |_, _| {}));
        }
        f([roots[0].0, roots[1].0, roots[2].0], turn);
        for (i, (slot, handle)) in roots.into_iter().enumerate().rev() {
            super::super::donation_ledger::release_task(first_id + i as u64);
            release(slot, first_id + i as u64, handle);
        }
    });
}

#[test]
fn root_job_authority_is_an_explicit_consumed_bootstrap_grant() {
    let _fixture = super::super::tests::boxed_scheduler();
    use crate::user::handles::KernelHandle;
    use kernel_object::api::handle::{HandleRights, JobHandleRights};
    let fd = rustos_user_abi::syscall::JOB_BOOTSTRAP_ROOT_FD;
    let mut b = bootstrap(UserAbi::Linux);
    b.logical_admin = true;
    b.set_exec_path("services/rootd/rootd.elf");
    let normal = Input::AddressSpace(ProcessAddressSpace::modeled_root_for_host(0x4000))
        .materialize(&mut b, ID).unwrap();
    assert!(normal.handles().get_entry(fd).is_none(), "path and admin do not grant root authority");
    let mut b = bootstrap(UserAbi::Linux);
    b.root_job_authority = true;
    let granted = Input::AddressSpace(ProcessAddressSpace::modeled_root_for_host(0x5000))
        .materialize(&mut b, ID).unwrap();
    assert!(!b.root_job_authority, "materialization consumes the bootstrap grant");
    let entry = granted.handles().get_entry(fd).unwrap();
    assert_eq!(entry.rights(), HandleRights::Job(JobHandleRights::ALL));
    assert!(matches!(entry.handle(), KernelHandle::Job(id) if *id == crate::multitask::ROOT_JOB));
    b.root_job_authority = true;
    assert!(Input::State(state()).materialize(&mut b, ID).is_none(), "a fork-state input cannot mint root authority");
}

pub(in crate::multitask::scheduler) fn policy() -> super::super::super::SchedulingContextAdmission {
    super::super::super::SchedulingContextAdmission {
        budget_ns: 4000,
        period_ns: 10000,
        refill_capacity: 4,
        cpu_mask: 1,
        criticality: 1,
        domain: 120700,
        policy_epoch: 1,
        timeout_endpoint_cap: 0,
    }
}
pub(in crate::multitask) fn bootstrap(abi: UserAbi) -> UserTaskBootstrap {
    let mut b = UserTaskBootstrap::new(
        abi,
        x86_64::VirtAddr::new(0x400000),
        x86_64::VirtAddr::new(0x700000),
    );
    b.registers.rax = 31;
    // Existing native MM cursor fixture: modeled root is not a physical page table.
    b.linux_process_state = Some(crate::user::linux::LinuxProcessState::default());
    b
}
fn state() -> UserProcessState { state_with_root(0x4000) }
pub(in crate::multitask::scheduler) fn state_with_root(root: u64) -> UserProcessState {
    UserProcessState::new(
        ProcessAddressSpace::modeled_root_for_host(root),
        Some(crate::user::linux::LinuxProcessState::default()),
        None,
        None,
        None,
        false,
        "/root-test",
    )
}
fn fixture(f: impl FnOnce()) {
    let _fixture = super::super::tests::boxed_scheduler();
    // Catalog-only witnesses leave modeled Identity rows without physical
    // Resources. This fixture owns the canonical native namespace locks; clear
    // only those empty projections, never a real context, stack or live loan.
    for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
        if stack_resources::context::read(slot).is_none()
            && stack_resources::task_start::read(slot).is_none()
            && !stack_resources::allocated(slot)
        {
            assert_eq!(identity::execution::count(slot), 0);
            assert!(!identity::cohort::busy(slot));
            identity::clear(slot);
        }
    }
    let mut turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
    turn.mask_irqs();
    let ticket = root_slots::reserve(KERNEL, KID).unwrap();
    kernel_creation::publish_reserved(
        ticket,
        |_| {},
        119,
        8,
        16,
        0x202,
        0x1000,
        0x4000,
        0,
        |_, _| {},
        |_| 1,
    );
    drop(turn);
    f();
    identity::update_live_lifecycle(KERNEL, KID, identity::LifecycleGate::Retired, true);
    runqueue::retire(KERNEL, NICE_0_LOAD);
    let context = stack_resources::context::read(KERNEL).unwrap();
    assert!(scheduling_runtime::close_for_reclaim(
        KERNEL,
        context.scheduling_context.identity()
    ));
    let mut reuse = stack_resources::reuse::begin_retired(KERNEL, KID).unwrap();
    super::super::dispatch_policy::remove_reclaimed_slot(KERNEL);
    runqueue::release_retired(KERNEL);
    scheduling_runtime::release(KERNEL, context.scheduling_context.identity());
    stack_resources::context::clear(KERNEL);
    stack_resources::task_start::clear(KERNEL);
    frame_publication::clear(KERNEL);
    drop(reuse.take_storage());
    reuse.finish();
}
fn current_actor(
    slot: usize,
    id: u64,
    f: impl FnOnce(&Actor, &nucleus_core::util::lockdep::DispatchPreemption),
) {
    let current = crate::multitask::cpu_local::install_test_current_owner(0, slot);
    let token = frame_publication::test_read(slot).unwrap();
    let pin = frame_publication::pin_for_identity(token, id).unwrap();
    stack_resources::cpu_leases::with_test_bank(0, pin, false, || {
        let mut turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
        turn.mask_irqs();
        let actor = Actor::capture(turn.cpu()).unwrap();
        f(&actor, &turn);
    });
    drop(current);
}
fn kernel(f: impl FnOnce(&Actor, &nucleus_core::util::lockdep::DispatchPreemption)) {
    fixture(|| {
        current_actor(KERNEL, KID, |actor, turn| {
            assert!(!actor.is_user());
            f(actor, turn);
        })
    });
}
fn construct_child(
    actor: &Actor,
    admission: &Admission,
    turn: &nucleus_core::util::lockdep::DispatchPreemption,
    id: u64,
    parent: Option<u64>,
    suspended: bool,
    f: impl FnOnce(usize, u64),
) -> (usize, process_table::ProcessHandle) {
    let reservation = process_table::reserve_spawn().unwrap();
    let handle = reservation.process_handle();
    let (user, slot) = construct(
        id,
        Input::State(state()),
        bootstrap(UserAbi::Linux),
        parent,
        119,
        0x23,
        0x1b,
        0x202,
        suspended,
        reservation,
        actor,
        admission,
        turn,
        |_| 1,
        f,
    )
    .unwrap();
    assert_eq!(user, actor.is_user());
    assert_eq!(
        stack_resources::context::read(slot).unwrap().process_handle,
        Some(handle)
    );
    (slot, handle)
}
/// Actual independent process birth for cross-MM IRQ joins. Only the earlier
/// physical page-table root is modeled; process/domain/frame/UID custody and
/// activation use the production constructors, not a scalar MM replacement.
pub(in crate::multitask::scheduler) fn active_independent_root(
    id: u64, boot: UserTaskBootstrap, root: u64,
) -> (usize, process_table::ProcessHandle) {
    let mut turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
    turn.mask_irqs();
    active_independent_root_with_scope(id, boot, root, &turn)
}
pub(in crate::multitask::scheduler) fn active_independent_root_with_scope(
    id: u64, boot: UserTaskBootstrap, root: u64,
    turn: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
) -> (usize, process_table::ProcessHandle) {
    let actor = Actor::capture(turn.cpu()).unwrap();
    let admission = Admission::capture(&actor, None, Some(policy())).unwrap();
    let reservation = process_table::reserve_spawn().unwrap();
    let handle = reservation.process_handle();
    let (_, slot) = construct(id, Input::State(state_with_root(root)), boot, None,
        119, 0x23, 0x1b, 0x202, true, reservation, &actor, &admission, turn,
        |_| 1, |_, _| {}).unwrap();
    drop(admission);
    drop(actor);
    let activation = super::super::activation::Activation::capture(&[(slot, id)]).unwrap();
    with_activation_current(turn, || {
        assert!(activation.commit_owned(turn, |_| 1, || {}));
    });
    (slot, handle)
}

/// Earlier native CPU entry input for actual activation consumers. Physical
/// Current is already parked by the enclosing fixture; retain real UID/funding
/// units and use the actual queue return afterwards, never a scalar restore.
pub(in crate::multitask::scheduler) fn with_activation_current<R>(
    turn: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    f: impl FnOnce() -> R,
) -> R {
    let slot = crate::multitask::cpu_local::current_cpu_task_slot().unwrap();
    let owns_uid = identity::execution::banks::owns_current(turn.cpu(), slot);
    if owns_uid && current_terminal::OriginalCurrent::borrow(turn).is_some() {
        return f();
    }
    let id = stack_resources::task_start::read(slot).unwrap().id;
    let banks = (!owns_uid).then(|| {
        let banks = identity::execution::banks::isolate(turn.cpu());
        identity::execution::banks::initialize(turn.cpu(), identity::execution::initial(slot, id).unwrap());
        banks
    });
    let was_local = runqueue::owner(slot).state == runqueue::RunOwnerState::Local;
    if was_local { runqueue::test_running_claim(slot, turn.cpu(), runqueue::weight::value(slot)); }
    // install asserts NO active interval: an existing borrowed interval can
    // never be replaced merely because OriginalCurrent::borrow was refused.
    execution_interval::test_open_captured(slot, 10);
    let result = f();
    match execution_interval::consume(turn.cpu(), slot, id) {
        execution_interval::ConsumedInterval::Active(interval) => { let _ = interval.charge(0, 1); }
        _ => panic!("activation fixture lost its original Current interval"),
    }
    drop(banks);
    if was_local && !identity::lifecycle(slot).retired {
        assert_eq!(runqueue::publish_return(slot, turn.cpu(), runqueue::weight::value(slot), true),
            runqueue::ReturnPublication::Local);
    }
    result
}

/// Actual root/actor producer fixture for independent activation witnesses.
/// The policy/ABI inputs are modeled; no fixture supplies execution permission.
pub(in crate::multitask::scheduler) fn suspended_roots(
    ids: &[u64],
    mask: u64,
    boot: impl Fn(u64) -> UserTaskBootstrap,
    f: impl FnOnce(
        &[(usize, u64, process_table::ProcessHandle)],
        &nucleus_core::util::lockdep::DispatchPreemption,
    ),
) {
    kernel(|actor, turn| {
        // Only CPUs 0/1 participate in these owned queue fixtures. Other tests
        // deliberately own independent pending-flag cells 3/5.
        for cpu in [0, 1] {
            *dispatch_policy::lock(cpu) = dispatch_policy::CpuDispatchPolicy::new();
            dispatch_policy::clear_atomic_activation_pending(cpu);
        }
        let mut supplied = policy();
        supplied.cpu_mask = mask;
        let admission = Admission::capture(actor, None, Some(supplied)).unwrap();
        let mut roots = Vec::new();
        for &id in ids {
            let reservation = process_table::reserve_spawn().unwrap();
            let (_, slot) = construct(
                id,
                Input::State(state()),
                boot(id),
                None,
                119,
                0x23,
                0x1b,
                0x202,
                true,
                reservation,
                actor,
                &admission,
                turn,
                |_| mask,
                |_, _| {},
            )
            .unwrap();
            assert_eq!(
                stack_resources::context::read(slot).unwrap().process_handle,
                Some(reservation.process_handle())
            );
            roots.push((slot, id, reservation.process_handle()));
        }
        with_activation_current(turn, || f(&roots, turn));
        for (slot, id, handle) in roots {
            release(slot, id, handle);
        }
        for cpu in [0, 1] {
            *dispatch_policy::lock(cpu) = dispatch_policy::CpuDispatchPolicy::new();
            dispatch_policy::clear_atomic_activation_pending(cpu);
        }
    });
}

/// Native physical exec fixture. Current scalar/HAL entry are modeled; real
/// root producer, backing bank, writer and process/MM owners still execute.
pub(in crate::multitask) fn exec_root(
    id: u64,
    current: bool,
    boot: UserTaskBootstrap,
    f: impl FnOnce(usize, process_table::ProcessHandle),
) {
    exec_root_with_release(id, current, boot, f, release);
}

/// Same original physical root/Current fixture; terminal tests supply explicit
/// consumer ACK/reclaim AFTER the bank drops, never repeat a stopped producer.
pub(in crate::multitask::scheduler) fn exec_root_with_release(
    id: u64,
    current: bool,
    boot: UserTaskBootstrap,
    f: impl FnOnce(usize, process_table::ProcessHandle),
    release: impl FnOnce(usize, u64, process_table::ProcessHandle),
) {
    exec_root_with_admission(id, current, boot, policy(), f, release)
}

/// Same actual fixture engine, with an explicit native initial funding policy.
/// Does not replace runtime admission/CPU masks with a test dispatch shortcut.
pub(in crate::multitask::scheduler) fn exec_root_with_admission(
    id: u64,
    current: bool,
    boot: UserTaskBootstrap,
    policy: super::super::super::SchedulingContextAdmission,
    f: impl FnOnce(usize, process_table::ProcessHandle),
    release: impl FnOnce(usize, u64, process_table::ProcessHandle),
) {
    exec_root_with_backing(id, current, boot, policy, state(), f, release);
}

/// Initial native MM input is owned BEFORE original birth. Never replace a
/// published scalar root merely to bypass active-MM reclamation exclusion.
pub(in crate::multitask::scheduler) fn exec_root_with_backing(
    id: u64,
    current: bool,
    boot: UserTaskBootstrap,
    policy: super::super::super::SchedulingContextAdmission,
    input: UserProcessState,
    f: impl FnOnce(usize, process_table::ProcessHandle),
    release: impl FnOnce(usize, u64, process_table::ProcessHandle),
) {
    fixture(|| {
        let mut born = None;
        current_actor(KERNEL, KID, |actor, turn| {
            let admission = Admission::capture(actor, None, Some(policy)).unwrap();
            let reservation = process_table::reserve_spawn().unwrap();
            let (_, slot) = construct(
                id,
                Input::State(input),
                boot,
                None,
                119,
                0x23,
                0x1b,
                0x202,
                false,
                reservation,
                actor,
                &admission,
                turn,
                |_| 1,
                |_, _| {},
            )
            .unwrap();
            born = Some((slot, reservation.process_handle()));
        });
        let (slot, handle) = born.unwrap();
        let actor = if current { slot } else { KERNEL };
        let actor_id = if current { id } else { KID };
        let modeled_current = crate::multitask::cpu_local::install_test_current_owner(0, actor);
        let token = frame_publication::test_read(actor).unwrap();
        let pin = frame_publication::pin_for_identity(token, actor_id).unwrap();
        stack_resources::cpu_leases::with_test_bank(0, pin, false, || f(slot, handle));
        drop(modeled_current);
        release(slot, id, handle);
    });
}
pub(in crate::multitask::scheduler) fn release(slot: usize, id: u64, handle: process_table::ProcessHandle) {
    // Explicit native external ACK injection only; real seals/bank/context/MM
    // and process detachment still execute and refuse outstanding loans.
    super::super::thread_creation::tests::release_born(slot, id, handle);
}

#[test]
fn actual_root_construct_owns_pinned_process_and_complete_blocked_slot_ack_before_wake() {
    kernel(|actor, turn| {
        let admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let (slot, handle) =
            construct_child(actor, &admission, turn, ID, None, false, |slot, id| {
                assert_eq!(id, ID);
                assert_eq!(
                    runqueue::owner(slot).state,
                    runqueue::RunOwnerState::Blocked
                );
                assert!(!stack_resources::admission::is_reserved(slot));
                let context = stack_resources::context::read(slot).unwrap();
                let h = context.process_handle.unwrap();
                assert!(!crate::multitask::process_placement::root_membership::pending(h));
                assert_eq!(
                    process_table::live_process_identity_by_pid(ID),
                    process_table::live_process_identity(h)
                );
                assert_eq!(crate::multitask::process_placement::mask(h), Some(u64::MAX));
                assert!(context.scheduling_context.is_budgeted());
                assert_eq!(
                    identity::read(slot)
                        .unwrap()
                        .pager_charge
                        .unwrap()
                        .policy_epoch,
                    1
                );
                assert!(
                    std::panic::catch_unwind(|| stack_resources::context::clear(slot)).is_err()
                );
                let token = frame_publication::test_read(slot).unwrap();
                assert_eq!(token.owner_generation, runqueue::owner(slot).generation);
                // SAFETY: real Blocked frame reader and bank pins retain byte exclusion.
                let frame =
                    unsafe { frame_observation::observe(slot, 0, token.saved_rsp, false) }.unwrap();
                assert_eq!(frame.saved.rax, 31);
                assert_eq!(frame.saved.rip, 0x400000);
                assert_eq!(frame.saved.rsp, 0x700000);
            });
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Local);
        assert_eq!(
            runqueue::affinity_payload::affinity_snapshot(slot),
            (1, u64::MAX, false)
        );
        release(slot, ID, handle);
    });
}
#[test]
fn suspended_root_finishes_both_real_acks_without_wake_or_fresh_dispatch_permission() {
    kernel(|actor, turn| {
        let admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let (slot, handle) = construct_child(actor, &admission, turn, ID, None, true, |slot, _| {
            assert!(identity::lifecycle(slot).suspended);
            assert!(
                identity::execution::with_claim(slot, |_| panic!("suspended root claimed"))
                    .is_none()
            );
        });
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Blocked
        );
        assert_eq!(runqueue::wait::ready_since_ticks(slot), 0);
        release(slot, ID, handle);
    });
}
#[test]
fn terminal_after_root_ack_cannot_reuse_child_before_wake_or_resurrect_retired_owner() {
    kernel(|actor, turn| {
        let admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let (slot, handle) =
            construct_child(actor, &admission, turn, ID, None, false, |slot, id| {
                identity::update_live_lifecycle(slot, id, identity::LifecycleGate::Retired, true);
                runqueue::retire(slot, runqueue::weight::value(slot));
                let context = stack_resources::context::read(slot).unwrap();
                assert!(scheduling_runtime::close_for_reclaim(
                    slot,
                    context.scheduling_context.identity()
                ));
                assert!(stack_resources::reuse::begin_retired(slot, id).is_none());
                assert!(root_slots::reserve(slot, id + 1).is_none());
            });
        assert_eq!(
            runqueue::owner(slot).state,
            runqueue::RunOwnerState::Retired
        );
        release(slot, ID, handle);
    });
}
#[test]
fn prepared_root_is_invisible_and_copied_spawn_receipt_cannot_publish_or_cancel_real_seal() {
    kernel(|_, _| {
        let reservation = process_table::reserve_spawn().unwrap();
        let ticket = root_slots::reserve(1, ID).unwrap();
        let root = root_slots::RootPublication::prepare(reservation, ticket, 1)
            .ok()
            .unwrap();
        assert!(
            crate::multitask::process_placement::root_membership::pending(
                reservation.process_handle()
            )
        );
        assert_eq!(process_table::live_process_identity_by_pid(ID), None);
        assert!(!process_table::cancel_spawn(reservation));
        assert!(process_table::publish_spawn(reservation, ID, None, state()).is_none());
        // A second root cannot treat this invisible prefix as a live process.
        assert!(
            crate::multitask::process_placement::root_membership::RootJoin::begin(
                reservation.process_handle(),
                (1, ID),
                root.ticket().generation(),
                1
            )
            .is_none()
        );
        root.cancel();
        assert!(!stack_resources::admission::is_reserved(1));
        assert!(!stack_resources::allocated(1));
        assert!(!process_table::cancel_spawn(reservation));
        // Stop precisely between actual process cancellation and physical slot
        // rollback. Releasing the non-Copy seal must not reauthorize a copied
        // receipt during this interval, even though the Ticket is still live.
        let reservation = process_table::reserve_spawn().unwrap();
        let ticket = root_slots::reserve(1, ID).unwrap();
        let join = crate::multitask::process_placement::root_membership::RootJoin::prepare(
            reservation,
            (1, ID),
            ticket.generation(),
            1,
        )
        .unwrap();
        drop(join);
        assert!(ticket.is_reserved_current());
        assert!(process_table::publish_spawn(reservation, ID, None, state()).is_none());
        assert!(!process_table::cancel_spawn(reservation));
        assert_eq!(process_table::live_process_identity_by_pid(ID), None);
        root_slots::cancel_unpublished(ticket);
    });
}
#[test]
fn exhausted_root_nonce_refuses_before_pid_visibility_and_returns_exact_reserved_slot_for_cancel() {
    kernel(|_, _| {
        let reservation = process_table::reserve_spawn().unwrap();
        let handle = reservation.process_handle();
        let old = crate::multitask::process_placement::root_membership::set_nonce_fixture(
            handle,
            u64::MAX,
        );
        let ticket = root_slots::reserve(1, ID).unwrap();
        let ticket = root_slots::RootPublication::prepare(reservation, ticket, 1)
            .err()
            .unwrap();
        assert!(ticket.is_reserved_current());
        assert_eq!(process_table::live_process_identity_by_pid(ID), None);
        root_slots::cancel_unpublished(ticket);
        assert!(process_table::cancel_spawn(reservation));
        // Explicit quiescent overflow injection recovery, never production rewind.
        crate::multitask::process_placement::root_membership::set_nonce_fixture(handle, old);
    });
}
#[test]
fn actual_windows_materialization_error_cancels_all_unpublished_process_and_slot_custody() {
    kernel(|actor, turn| {
        let admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let reservation = process_table::reserve_spawn().unwrap();
        let mut image = bootstrap(UserAbi::Windows);
        image.windows_thread_state = Some(
            crate::user::process_state::WindowsThreadRuntimeState::new(ID, u64::MAX),
        );
        assert!(
            construct(
                ID,
                Input::AddressSpace(ProcessAddressSpace::modeled_root_for_host(0x4000)),
                image,
                None,
                119,
                0x23,
                0x1b,
                0x202,
                false,
                reservation,
                actor,
                &admission,
                turn,
                |_| 1,
                |_, _| panic!("failed materialization reached ACK")
            )
            .is_none()
        );
        assert_eq!(process_table::live_process_identity_by_pid(ID), None);
        assert!(!stack_resources::allocated(1));
        assert!(!stack_resources::admission::is_reserved(1));
        assert!(!process_table::cancel_spawn(reservation));
    });
}
#[test]
fn newly_visible_root_abandonment_retains_actual_process_pin_and_poisoned_join_without_registration()
 {
    kernel(|_, _| {
        let reservation = process_table::reserve_spawn().unwrap();
        let handle = reservation.process_handle();
        let ticket = root_slots::reserve(1, ID).unwrap();
        let mut join = crate::multitask::process_placement::root_membership::RootJoin::prepare(
            reservation,
            (1, ID),
            ticket.generation(),
            1,
        )
        .unwrap();
        assert!(join.register().is_none());
        assert_eq!(join.publish(None, state()), Some(handle));
        drop(join);
        assert!(crate::multitask::process_placement::root_membership::pending(handle));
        assert!(
            crate::multitask::process_placement::root_membership::RootJoin::begin(
                handle,
                (1, ID),
                ticket.generation(),
                1
            )
            .is_none()
        );
        assert!(!process_table::cancel_spawn(reservation));
        // Explicit native injected poison recovery, not an abandoned-production ACK.
        crate::multitask::process_placement::root_membership::recover_fixture(handle);
        root_slots::cancel_unpublished(ticket);
        process_table::detach_task(handle).unwrap();
    });
}
#[test]
fn actual_fork_parent_cohort_retains_original_policy_and_mask_through_child_root_ack() {
    kernel(|actor, turn| {
        let root_admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let (parent, h) = construct_child(actor, &root_admission, turn, ID, None, true, |_, _| {});
        let admission = Admission::capture(actor, Some(ID), None).unwrap();
        assert!(identity::cohort::busy(parent));
        assert_eq!(identity::execution::count(parent), 2);
        assert_eq!(admission.policy, root_admission.policy);
        let (child, ch) =
            construct_child(actor, &admission, turn, ID + 1, Some(ID), true, |_, _| {
                assert!(identity::cohort::Cohort::begin(&[(parent, ID)]).is_none());
                assert!(
                    crate::multitask::process_placement::Change::begin(h, (parent, ID)).is_some()
                );
                assert_eq!(identity::execution::count(parent), 2);
            });
        drop(admission);
        assert!(!identity::cohort::busy(parent));
        assert_eq!(identity::execution::count(parent), 0);
        release(child, ID + 1, ch);
        release(parent, ID, h);
    });
}

#[test]
fn same_uid_root_reuse_requires_new_process_transaction_and_actual_ticket_epoch() {
    kernel(|actor, turn| {
        let admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let old = process_table::reserve_spawn().unwrap();
        let (_, slot) = construct(
            ID,
            Input::State(state()),
            bootstrap(UserAbi::Linux),
            None,
            119,
            0x23,
            0x1b,
            0x202,
            true,
            old,
            actor,
            &admission,
            turn,
            |_| 1,
            |_, _| {},
        )
        .unwrap();
        release(slot, ID, old.process_handle());
        process_table::reap_exited_processes();
        let new = process_table::reserve_spawn().unwrap();
        assert_ne!(new.transaction_id(), old.transaction_id());
        assert!(!process_table::cancel_spawn(old));
        assert!(process_table::publish_spawn(old, ID, None, state()).is_none());
        let (_, reused) = construct(
            ID,
            Input::State(state()),
            bootstrap(UserAbi::Linux),
            None,
            119,
            0x23,
            0x1b,
            0x202,
            true,
            new,
            actor,
            &admission,
            turn,
            |_| 1,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(slot, reused);
        assert_eq!(stack_resources::task_start::read(reused).unwrap().id, ID);
        release(reused, ID, new.process_handle());
    });
}
#[test]
fn original_user_actor_fork_retains_two_real_parent_loans_without_double_cohort_capture() {
    fixture(|| {
        let (parent, handle) = {
            let mut born = None;
            current_actor(KERNEL, KID, |actor, turn| {
                let admission = Admission::capture(actor, None, Some(policy())).unwrap();
                born = Some(construct_child(
                    actor,
                    &admission,
                    turn,
                    ID,
                    None,
                    false,
                    |_, _| {},
                ));
            });
            born.unwrap()
        };
        current_actor(parent, ID, |actor, turn| {
            assert!(actor.is_user());
            assert_eq!(identity::execution::count(parent), 2);
            let admission = Admission::capture(actor, Some(ID), None).unwrap();
            assert!(admission._parent.is_none());
            let (child, handle) =
                construct_child(actor, &admission, turn, ID + 1, Some(ID), true, |_, _| {
                    assert_eq!(identity::execution::count(parent), 2);
                    assert!(identity::with_reclaim_binding(parent, Some(ID), || ()).is_none());
                });
            release(child, ID + 1, handle);
        });
        assert_eq!(identity::execution::count(parent), 0);
        release(parent, ID, handle);
    });
}
#[test]
fn actual_orphan_runtime_refusal_does_not_publish_pid_or_release_foreign_funding() {
    kernel(|actor, turn| {
        let admission = Admission::capture(actor, None, Some(policy())).unwrap();
        let mut foreign = scheduling_context::SchedulingContext::bind(1, ID + 90);
        assert!(foreign.admit(
            admission.policy,
            admission.domain.slot(),
            admission.domain.generation()
        ));
        let reservation = process_table::reserve_spawn().unwrap();
        assert!(
            construct(
                ID,
                Input::State(state()),
                bootstrap(UserAbi::Linux),
                None,
                119,
                0x23,
                0x1b,
                0x202,
                true,
                reservation,
                actor,
                &admission,
                turn,
                |_| 1,
                |_, _| panic!("foreign runtime reached PID ACK")
            )
            .is_none()
        );
        assert_eq!(process_table::live_process_identity_by_pid(ID), None);
        assert!(foreign.runtime_snapshot().is_some());
        assert!(!stack_resources::allocated(1));
        assert!(!process_table::cancel_spawn(reservation));
        assert!(scheduling_runtime::close_for_reclaim(1, foreign.identity()));
        scheduling_runtime::release(1, foreign.identity());
    });
}

pub(in crate::multitask::scheduler) fn unbudgeted_refused() {
    kernel(|actor, _| {
        assert!(Admission::capture(actor, None, None).is_none());
        let mut invalid = policy();
        invalid.cpu_mask = 0;
        assert!(Admission::capture(actor, None, Some(invalid)).is_none());
        assert!(Admission::capture(actor, Some(ID), None).is_none());
    });
}
#[test]
fn actual_kernel_actor_does_not_grant_unbudgeted_or_missing_parent_admission() {
    unbudgeted_refused();
}
