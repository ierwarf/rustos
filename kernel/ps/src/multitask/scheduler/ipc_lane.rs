//! Owner: same-CPU synchronous call switch between two resident IPC tasks.
//! Boundary: software-schedule trap -> checks -> donation bind -> one point of
//! no return (the CAS of the parked receiver to Running) -> infallible moves.
//! Lifecycle: C (armed on its reply) blocks and parks at the RSP ACK; S (parked
//! on the endpoint) resumes from its cell without re-claiming its loans.
//! Concurrency: masked own CPU, no `CpuDispatchTurn`; remote CPUs see only owner
//! words, which are CASed exactly as the dispatcher would.
//! Failure: `Declined` changed nothing; `Generic` leaves the receiver woken
//! through the ordinary wake and lets the unchanged dispatcher finish the turn.
//! Forbidden: mutating state before the last fallible check, a Running receiver
//! without an executed switch, or a DirectHandoff owner without a dispatcher.
//! Evidence: `irq_entry_tests/lane.rs` differential and fallback witnesses.
use super::{
    current_identity, dispatch_policy, donation_ledger, donation_publication, frame_publication,
    ipc_publication, local_dispatch, resident, return_architecture::ArchitectureState, runqueue,
    runtime_profile, scheduling_context, stack_resources, sync_handoff, task_directory,
    wake_publication,
};
use crate::multitask::{cpu_local, current_identity::execution as identity_execution};
use core::cell::UnsafeCell;
use super::scheduling_runtime::FundingPin;
use nucleus_core::util::lockdep::{DispatchPreemption, MAX_TRACKED_CPUS};

/// What the trap does next.
pub(in crate::multitask) enum Outcome {
    /// Nothing changed; run the ordinary publication and dispatch.
    Declined,
    /// The switch is published; return this RSP to the stub.
    Switched(usize),
    /// The caller is committed and the receiver is awake in fair custody; the
    /// ordinary dispatcher must pick the next task.
    Generic,
}

/// Architecture state of the CPU's Current task and of the task it replaced,
/// kept by every install so a lane switch never recomputes it under a lock.
/// Only that CPU's masked trap and RSP-commit callback touch its element.
struct CpuArch(UnsafeCell<[Option<(usize, ArchitectureState)>; 2]>);
// SAFETY: element `cpu` is accessed only by `cpu` with IRQs masked.
unsafe impl Sync for CpuArch {}
static ARCH: [CpuArch; MAX_TRACKED_CPUS] = [const { CpuArch(UnsafeCell::new([None, None])) }; MAX_TRACKED_CPUS];

/// Record the architecture an install just made Current (`[0]`), keeping the
/// previous Current (`[1]`) until the RSP ACK decides whether it parks.
pub(super) fn note_installed(cpu: usize, slot: usize, arch: ArchitectureState) {
    // SAFETY: own-CPU masked install; see `CpuArch`.
    let cell = unsafe { &mut *ARCH[cpu].0.get() };
    cell[1] = cell[0];
    cell[0] = Some((slot, arch));
}

/// Bases that can change while a task runs are re-read from their atomic
/// mirrors; the stack top, process flag and Windows GS base are fixed per task.
fn fresh(slot: usize, mut arch: ArchitectureState) -> ArchitectureState {
    arch.root = runqueue::address_space::root(slot);
    arch.fs = runqueue::simd_tls::tls_fs_base(slot);
    arch
}

fn current_arch(cpu: usize, slot: usize) -> Option<ArchitectureState> {
    // SAFETY: own-CPU masked read; see `CpuArch`.
    let (cached, arch) = unsafe { (*ARCH[cpu].0.get())[0] }?;
    (cached == slot).then(|| fresh(slot, arch))
}

/// Architecture of the task that just left `cpu`, valid at its RSP ACK.
pub(super) fn outgoing_arch(cpu: usize, slot: usize) -> Option<ArchitectureState> {
    // SAFETY: own-CPU masked read in the commit callback; see `CpuArch`.
    let (cached, arch) = unsafe { (*ARCH[cpu].0.get())[1] }?;
    (cached == slot).then(|| fresh(slot, arch))
}

fn ticks_to_ns(start: u64, end: u64) -> u64 {
    end.saturating_sub(start)
        .saturating_mul(1_000_000_000)
        .saturating_div(crate::arch::rtc::ticks_per_second().max(1))
}

/// Settle the Current task's interval once, as `CurrentReturn::account` does
/// for a voluntary return: charge the funder, advance virtual time by at least
/// the minimum granularity, and clear the execution baselines. The funding it
/// held is returned when the charge left credit, for the successor to continue
/// on (one pin shuttles along a call/reply chain instead of being dropped and
/// re-pinned at every hop).
fn settle(cpu: usize, slot: usize, id: u64, clock: &crate::arch::rtc::ClockSample) -> Option<FundingPin> {
    use super::execution_interval::{ConsumedInterval, consume};
    let mut interval = match consume(cpu, slot, id) {
        ConsumedInterval::Active(interval) => interval,
        ConsumedInterval::AlreadySettled | ConsumedInterval::Absent => return None,
    };
    let elapsed = interval.elapsed_ns(clock.ticks, clock.local_nanos, ticks_to_ns);
    runtime_profile::owner::local(|row| row.account(slot, elapsed));
    let mut carried = None;
    if let Some(pin) = interval.take_funding() {
        if elapsed == 0 {
            carried = Some(pin);
        } else if let Some((funder, outcome, continues)) =
            pin.charge_in_place(clock.monotonic_nanos, elapsed)
        {
            if outcome.exhausted && outcome.charged_ns != 0 {
                scheduling_context::latch_budget_exhaustion(funder, outcome.charged_ns);
            }
            if continues {
                carried = Some(pin);
            }
        }
    }
    let fair = elapsed.max(super::SCHED_MIN_GRANULARITY_NS);
    runqueue::add_vruntime(
        slot,
        super::task_weight::weighted_vruntime_delta(fair, runqueue::weight::value(slot)),
    );
    runqueue::set_exec_start_ticks(slot, 0);
    runqueue::set_exec_start_local_ns(slot, 0);
    carried
}

/// Root owner and reply label `slot` is funded under right now: the borrowed
/// donor context when a donation reaches it, else its own.
fn funding_label(slot: usize, id: u64) -> Option<(u64, u64)> {
    match donation_ledger::borrowed_context_charge_token(slot) {
        Some((owner_slot, reply)) => {
            Some((current_identity::read_dispatch(owner_slot)?.task_id?, reply))
        }
        None => Some((id, 0)),
    }
}

/// Publish the Current task's frame and return it to the owner word as Blocked,
/// the same sequence `CurrentReturn::publish_frame(.., false)` and
/// `publish_queue` run for a committed wait.
fn publish_blocked(cpu: usize, slot: usize, rsp: usize) -> bool {
    runqueue::wait::set_ready_since_ticks(slot, 0);
    let Some(view) = frame_publication::return_view(slot) else { return false };
    runqueue::set_saved_rsp(slot, rsp);
    frame_publication::publish(
        slot, rsp, view.kernel_stack_base, view.kernel_stack_top, view.alternate_stack_base,
        view.alternate_stack_top, view.user_mode_task, view.kernel_process_task,
        view.stack_canary_intact, view.scheduler_storage_base, view.scheduler_storage_end,
    );
    !matches!(
        runqueue::publish_return(slot, cpu, runqueue::weight::value(slot), true),
        runqueue::ReturnPublication::Terminal
    )
}

/// The call-direction lane. See the module contract.
pub(in crate::multitask) fn call(
    scope: &DispatchPreemption<'_>, endpoint: u64, reply: u64, receiver_id: u64, rsp: usize,
) -> Outcome {
    use runqueue::wait::{REASON_ENDPOINT_RECEIVE as RECEIVE, REASON_ENDPOINT_REPLY as REPLY};
    use runqueue::RunOwnerState::{Blocked, Running};
    let declined = Outcome::Declined;
    let cpu = scope.cpu();
    let clock = crate::arch::rtc::sample();
    if !ipc_publication::scoped(scope) || endpoint == 0 || reply == 0 || !cpu_local::lane_idle(cpu) {
        return declined;
    }
    let Some(c) = cpu_local::current_cpu_task_slot() else { return declined };
    let Some(c_id) = stack_resources::task_start::read(c).map(|start| start.id) else { return declined };
    let c_owner = runqueue::owner(c);
    if c_id == receiver_id || runqueue::is_idle_slot(c) || c == super::ROOT_TASK_SLOT
        || c_owner.state != Running || c_owner.cpu != Some(cpu) || !c_owner.runnable
        || !c_owner.wait_armed || c_owner.wait_reason_kind != REPLY
        || !stack_resources::cpu_leases::owns_current(cpu, c)
        || !identity_execution::banks::owns_current(cpu, c)
        || runqueue::wait::reason(c) != (REPLY, reply)
    {
        return declined;
    }
    let Some(prev_arch) = current_arch(cpu, c) else { return declined };
    let Some(s) = task_directory::hint(receiver_id)
        .filter(|&s| s != c && stack_resources::task_start::read(s).map(|t| t.id) == Some(receiver_id))
    else { return declined };
    let s_owner = runqueue::owner(s);
    let generation = s_owner.generation;
    if s_owner.state != Blocked || s_owner.wait_armed || s_owner.wait_reason_kind != RECEIVE
        || resident::parked_cpu(s, generation) != Some(cpu)
        || runqueue::wait::reason(s) != (RECEIVE, endpoint)
        || !ipc_publication::direct_switch_admitted(cpu)
        || donation_ledger::class_publication_sequence() & 1 != 0
    {
        return declined;
    }
    let Some(token) = frame_publication::observed_frame_token(s, cpu, false) else { return declined };
    if !admitted(cpu, c, c_id) || !admitted(cpu, s, receiver_id) || token.owner_generation != generation {
        return declined;
    }
    // Donation bind is the last fallible step that can still decline: the
    // ordinary path would bind the same reservation next.
    let (Some(donor), Some(receiver)) =
        (identity_execution::pin_held(c, c_id), identity_execution::pin_held(s, receiver_id))
    else { return declined };
    // A donor funded by its own context binds in one ledger acquisition; a
    // borrowed basis takes the full two-step bind with its root loan.
    let _donation = match donation_publication::bind_lane(scope, reply, &donor, &receiver) {
        Some(true) => None,
        Some(false) => return declined,
        None => match donation_publication::bind_original(scope, reply, donor, &receiver, false) {
            Some(loans) => Some(loans),
            None => return declined,
        },
    };

    // Committed from here: every failure is `Generic`.
    let generic = || {
        let _ = wake_publication::wake(scope, receiver_id, &super::runqueue_policy::online_affinity_mask);
    };
    // SAFETY: masked lane scope; `c` is Current on this CPU (checked above).
    unsafe { runqueue::simd_tls::save_simd_state_lane(c, c_id) };
    if runqueue::commit_wait(c) != runqueue::WaitCommitOutcome::Committed {
        generic();
        return Outcome::Generic;
    }
    runqueue::wait::set_blocked_since_ticks(c, clock.ticks);
    let from = Party { slot: c, id: c_id, arch: prev_arch };
    let to = Target { slot: s, id: receiver_id, owner: s_owner, wait: (RECEIVE, endpoint) };
    finish(cpu, from, to, rsp, &clock, generic)
}

/// The reply-direction lane: the committed server `S` (Current, blocked on its
/// next receive) answers the parked caller `C` and the CPU continues as `C`.
pub(in crate::multitask) fn reply(
    scope: &DispatchPreemption<'_>, reply: u64, caller_id: u64, context_owner: u64,
    context: kernel_object::api::identity::ObjectIdentity, rsp: usize,
) -> Outcome {
    use runqueue::wait::{REASON_ENDPOINT_RECEIVE as RECEIVE, REASON_ENDPOINT_REPLY as REPLY};
    use runqueue::RunOwnerState::{Blocked, Running};
    let declined = Outcome::Declined;
    let cpu = scope.cpu();
    let clock = crate::arch::rtc::sample();
    if !ipc_publication::scoped(scope) || reply == 0 || !cpu_local::lane_idle(cpu) {
        return declined;
    }
    let Some(s) = cpu_local::current_cpu_task_slot() else { return declined };
    let Some(s_id) = stack_resources::task_start::read(s).map(|start| start.id) else { return declined };
    let s_owner = runqueue::owner(s);
    if s_id == caller_id || runqueue::is_idle_slot(s) || s == super::ROOT_TASK_SLOT
        || s_owner.state != Running || s_owner.cpu != Some(cpu) || s_owner.runnable
        || s_owner.wait_armed || s_owner.wait_reason_kind != RECEIVE
        || !stack_resources::cpu_leases::owns_current(cpu, s)
        || !identity_execution::banks::owns_current(cpu, s)
    {
        return declined;
    }
    let (exact, live) = task_directory::reply_context_custody(context_owner, context);
    if !exact && live {
        return declined;
    }
    let Some(prev_arch) = current_arch(cpu, s) else { return declined };
    let Some(c) = task_directory::hint(caller_id)
        .filter(|&c| c != s && stack_resources::task_start::read(c).map(|t| t.id) == Some(caller_id))
    else { return declined };
    let c_owner = runqueue::owner(c);
    let generation = c_owner.generation;
    if c_owner.state != Blocked || c_owner.wait_armed || c_owner.wait_reason_kind != REPLY
        || resident::parked_cpu(c, generation) != Some(cpu)
        || runqueue::wait::reason(c) != (REPLY, reply)
        || !ipc_publication::recent_sleeper(runqueue::wait::blocked_since_ticks(c),
            clock.ticks, crate::arch::rtc::ticks_per_second())
        || !ipc_publication::direct_switch_admitted(cpu)
        || donation_ledger::class_publication_sequence() & 1 != 0
    {
        return declined;
    }
    let Some(token) = frame_publication::observed_frame_token(c, cpu, false) else { return declined };
    if token.owner_generation != generation || !admitted(cpu, s, s_id) || !admitted(cpu, c, caller_id) {
        return declined;
    }
    // Committed from here: the reply capability's donation is released, as the
    // ordinary reply wake does first, and every failure is `Generic`.
    let _ = donation_ledger::release_reply(reply, super::ipc_donation::DonationNamespace::IpcReply);
    let generic = || {
        let _ = wake_publication::reply_with(scope, reply, caller_id,
            &super::runqueue_policy::online_affinity_mask);
    };
    // SAFETY: masked lane scope; `s` is Current on this CPU (checked above).
    unsafe { runqueue::simd_tls::save_simd_state_lane(s, s_id) };
    let from = Party { slot: s, id: s_id, arch: prev_arch };
    let to = Target { slot: c, id: caller_id, owner: c_owner, wait: (REPLY, reply) };
    finish(cpu, from, to, rsp, &clock, generic)
}

/// Whether `slot` may run on `cpu`: published lifecycle dispatchable and the
/// task and process affinity masks both include the CPU.
fn admitted(cpu: usize, slot: usize, id: u64) -> bool {
    current_identity::read_dispatch(slot)
        .is_some_and(|p| p.task_id == Some(id) && p.lifecycle_dispatchable())
        && {
            let (task, process, _) = runqueue::affinity_payload::affinity_snapshot(slot);
            u32::try_from(cpu).ok().and_then(|cpu| 1_u64.checked_shl(cpu))
                .is_some_and(|bit| task & process & bit != 0)
        }
}

struct Party {
    slot: usize,
    id: u64,
    arch: ArchitectureState,
}
struct Target {
    slot: usize,
    id: u64,
    owner: runqueue::RunOwnerSnapshot,
    wait: (u8, u64),
}

/// Settle the committed outgoing task, claim the parked incoming one and move
/// to it. `generic` runs if the claim loses a race, leaving the incoming task
/// awake for the ordinary dispatcher.
fn finish(
    cpu: usize, from: Party, to: Target, rsp: usize, clock: &crate::arch::rtc::ClockSample,
    generic: impl FnOnce(),
) -> Outcome {
    // A donation chain keeps one funder end to end: when the Current interval is
    // already rooted at the incoming task's owner it is forwarded as it stands
    // and the funder is charged once for the whole span when the chain ends.
    // Otherwise the interval is settled and the incoming funding prepared afresh.
    let label = funding_label(to.slot, to.id);
    let forward = label.is_some_and(|(owner, _)| {
        super::execution_interval::can_forward(cpu, from.slot, from.id, owner)
    });
    let (funding, shuttled) = if forward {
        (None, true)
    } else {
        let carried = settle(cpu, from.slot, from.id, clock);
        match (carried, label) {
            (Some(pin), Some((owner, reply))) if pin.label().0 == owner => {
                (Some(pin.relabeled(reply)), true)
            }
            (carried, _) => {
                drop(carried);
                match scheduling_context::dispatch::prepare_grant(to.slot, cpu) {
                    Some(grant) if grant.task_id == to.id => (grant.funding, false),
                    _ => {
                        generic();
                        return Outcome::Generic;
                    }
                }
            }
        }
    };
    // The point of no return: the parked task leaves its wait and becomes Running.
    let generation = to.owner.generation;
    let running = to.owner.lane_running(cpu);
    let (kind, key) = to.wait;
    let claim = || {
        runqueue::affinity_payload::with_placement_admission(to.slot, Some(to.id), cpu, || {
            runqueue::wait::lane_wake_clear(to.slot, kind, key, || {
                runqueue::lane_cas(to.slot, to.owner, running).ok()
            })
        })
        .and_then(|(claimed, grant)| claimed.map(|()| grant))
    };
    let now = clock.monotonic_nanos;
    let grant = match (funding.as_ref(), shuttled) {
        (Some(pin), false) => pin.with_dispatch_admission(cpu, now, claim),
        _ => claim(),
    };
    let Some(mut grant) = grant else {
        generic();
        return Outcome::Generic;
    };
    let loans = resident::take(to.slot, generation).expect("lane claim won the owner CAS without its cell");
    grant.bind_claimed(running);

    // Infallible from here: bank moves, publication, registers.
    stack_resources::cpu_leases::prepare(cpu, loans.pin);
    identity_execution::banks::prepare_lane(cpu, loans.permit);
    if forward {
        let hop = super::execution_interval::forward(
            cpu, (from.slot, from.id), (to.slot, to.id, running.generation), label.map_or(0, |(_, reply)| reply), clock);
        runtime_profile::owner::local(|row| row.account(from.slot, hop));
        runqueue::add_vruntime(
            from.slot,
            super::task_weight::weighted_vruntime_delta(
                hop.max(super::SCHED_MIN_GRANULARITY_NS), runqueue::weight::value(from.slot)),
        );
    } else {
        super::execution_interval::open_lane(cpu, to.slot, to.id, running.generation, funding, clock);
    }
    assert!(publish_blocked(cpu, from.slot, rsp), "lane outgoing task lost Running custody");
    let next_arch = fresh(to.slot, loans.arch);
    super::return_architecture::lane_install(from.arch, next_arch, to.slot, to.id);
    note_installed(cpu, to.slot, next_arch);
    cpu_local::lane_publish(cpu, from.slot, to.slot);
    cpu_local::lane_install_placement(cpu, grant);
    complete(cpu, from.slot, from.id, to.slot, to.id, loans.saved_rsp, from.arch.root != next_arch.root, clock.ticks);
    Outcome::Switched(loans.saved_rsp)
}

/// The side effects `dispatcher::complete` records for a synchronous dispatch.
fn complete(cpu: usize, from: usize, from_id: u64, to: usize, to_id: u64, next_rsp: usize, new_root: bool, ticks: u64) {
    runqueue::wait::set_ready_since_ticks(to, 0);
    dispatch_policy::record_completed_choice(
        cpu, !runqueue::is_idle_slot(to) && !local_dispatch::is_user_class(to), false);
    sync_handoff::record_dispatch(cpu, true, ticks);
    runtime_profile::owner::local(|row| {
        row.dispatch(to);
        row.runtime_profile_task_switches = row.runtime_profile_task_switches.saturating_add(1);
        if new_root {
            row.runtime_profile_address_space_switches =
                row.runtime_profile_address_space_switches.saturating_add(1);
        }
    });
    if runtime_profile::owner::migration(to, to_id, cpu as u8) {
        runtime_profile::owner::local(|row| {
            row.runtime_profile_cross_cpu_migrations =
                row.runtime_profile_cross_cpu_migrations.saturating_add(1);
        });
    }
    runqueue::affinity_payload::record_last_cpu(to, cpu as u8);
    cpu_local::set_scheduler_current_task_scratch_for_cpu(cpu, to);
    nucleus_core::util::lockdep::record_scheduler_dispatch(
        cpu, from_id, to_id, from, to, next_rsp, false, false);
    nucleus_core::util::lockdep::set_current_task_owner(to_id.checked_add(1).expect("task lock owner exhausted"));
}
