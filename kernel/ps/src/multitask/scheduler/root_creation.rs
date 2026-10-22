//! Owner: independent root/fork process birth, materialization and final publication.
//! Boundary: actual Current actor, parent Cohort, domain lease, Ticket and sealed spawn.
//! Lifecycle: preclaim root nonce -> fallible private state -> pinned PID -> Slot/Join ACK -> wake.
//! Concurrency: non-yielding own-CPU IRQ/preemption; no raw leaf crosses state/frame work.
//! Failure: unpublished state/runtime/process/slot rollback; visible abandonment quarantines.
//! Forbidden: copied spawn receipt, facade, physical backing or telemetry as effect/resume grant.
//! Evidence: actual shared owners; native namespace inputs are not physical MM/CPU completion.
use super::*;
use crate::multitask::current_identity as identity;

enum Actor {
    Kernel(identity::execution::kernel_executor::Executor),
    User {
        slot: usize,
        id: u64,
        context: TaskContext,
        _pin: identity::execution::BindingPin,
        _cohort: identity::cohort::Cohort,
    },
}
impl Actor {
    fn capture(cpu: usize) -> Option<Self> {
        let slot = crate::multitask::cpu_local::current_cpu_task_slot()?;
        if !stack_resources::cpu_leases::owns_current(cpu, slot) {
            return None;
        }
        let context = stack_resources::context::read(slot)?;
        let id = stack_resources::task_start::read(slot)?.id;
        if !context.scheduling_context.is_bound_to(id) {
            return None;
        }
        if !context.user_mode {
            return Some(Self::Kernel(retirement_cleanup::executor::capture_binding(
                slot, id,
            )?));
        }
        let pin = thread_creation::capture_creator(slot)?;
        let cohort = identity::cohort::Cohort::begin(&[(slot, id)])?;
        Some(Self::User {
            slot,
            id,
            context,
            _pin: pin,
            _cohort: cohort,
        })
    }
    fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }
}
struct Parent {
    _pin: identity::execution::BindingPin,
    _cohort: identity::cohort::Cohort,
}
struct Admission {
    _parent: Option<Parent>,
    domain: domain_admission::Admission,
    policy: scheduling_context::SchedulingContextPolicy,
    process_mask: u64,
    task_mask: u64,
}
fn parent_inputs(
    slot: usize,
    id: u64,
    pid: u64,
) -> Option<(scheduling_context::SchedulingContextPolicy, u64)> {
    let context = stack_resources::context::read(slot)?;
    if !context.user_mode
        || context.process_id != Some(pid)
        || !context.scheduling_context.is_bound_to(id)
        || stack_resources::task_start::read(slot)?.id != id
    {
        return None;
    }
    let mask = crate::multitask::process_placement::mask(context.process_handle?)?;
    Some((context.scheduling_context.policy()?, mask))
}
impl Admission {
    fn capture(
        actor: &Actor,
        parent: Option<u64>,
        supplied: Option<super::super::SchedulingContextAdmission>,
    ) -> Option<Self> {
        let mut retained_parent = None;
        let (inherited, process_mask) = match parent {
            None => (None, u64::MAX),
            Some(pid) => {
                let captured = match actor {
                    Actor::User {
                        slot, id, context, ..
                    } if context.process_id == Some(pid) => parent_inputs(*slot, *id, pid)?,
                    _ => {
                        let (pin, cohort, captured) = (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK)
                            .find_map(|slot| {
                                let context = stack_resources::context::read(slot)?;
                                if context.process_id != Some(pid) {
                                    return None;
                                }
                                let pin = thread_creation::capture_creator(slot)?;
                                let id = stack_resources::task_start::read(slot)?.id;
                                let cohort = identity::cohort::Cohort::begin(&[(slot, id)])?;
                                Some((pin, cohort, parent_inputs(slot, id, pid)?))
                            })?;
                        retained_parent = Some(Parent {
                            _pin: pin,
                            _cohort: cohort,
                        });
                        captured
                    }
                };
                (Some(captured.0), captured.1)
            }
        };
        let policy = supplied
            .map(|a| scheduling_context::SchedulingContextPolicy {
                budget_ns: a.budget_ns,
                period_ns: a.period_ns,
                refill_capacity: a.refill_capacity,
                cpu_mask: a.cpu_mask,
                criticality: a.criticality,
                domain: a.domain,
                policy_epoch: a.policy_epoch,
                timeout_endpoint_cap: a.timeout_endpoint_cap,
            })
            .or(inherited)?;
        if !policy.is_valid() {
            return None;
        }
        let task_mask = process_mask & policy.cpu_mask;
        if task_mask == 0 {
            return None;
        }
        let domain = domain_admission::admit(policy)?;
        Some(Self {
            _parent: retained_parent,
            domain,
            policy,
            process_mask,
            task_mask,
        })
    }
}

pub(in crate::multitask) enum Input {
    AddressSpace(ProcessAddressSpace),
    State(UserProcessState),
}
impl Input {
    pub(super) fn materialize(
        self,
        bootstrap: &mut UserTaskBootstrap,
        id: u64,
    ) -> Option<UserProcessState> {
        match self {
            Self::State(state) => Some(state),
            Self::AddressSpace(space) => {
                let mut state = UserProcessState::new(
                    space,
                    bootstrap.linux_process_state.take(),
                    bootstrap.linux_memory_map.take(),
                    bootstrap.linux_runtime_profile.take(),
                    bootstrap.windows_runtime.take(),
                    bootstrap.logical_admin,
                    bootstrap.exec_path(),
                );
                if let Some(thread) = bootstrap.windows_thread_state
                    && process::initialize_windows_thread_identifiers(
                        state.address_space_mut(),
                        thread.teb_address,
                        id,
                        id,
                    )
                    .is_err()
                {
                    return None;
                }
                Some(state)
            }
        }
    }
}

/// All fallible materialization precedes PID visibility. Actual spawn seal and
/// root nonce are already owned; no user yield/ProcessStateLock crosses this scope.
#[allow(clippy::too_many_arguments)]
pub(in crate::multitask) fn allocate(
    id: u64,
    input: Input,
    bootstrap: UserTaskBootstrap,
    parent: Option<u64>,
    pit: u16,
    cs: u64,
    ss: u64,
    flags: u64,
    suspended: bool,
    supplied: Option<super::super::SchedulingContextAdmission>,
    reservation: process_table::SpawnReservation,
) -> Option<(bool, usize)> {
    assert!(
        crate::multitask::cpu_local::scheduler_initialized(),
        "root creation preceded complete image"
    );
    let mut turn = nucleus_core::util::lockdep::DispatchPreemption::enter();
    turn.mask_irqs();
    let actor = Actor::capture(turn.cpu())?;
    let admission = Admission::capture(&actor, parent, supplied)?;
    let result = construct(
        id,
        input,
        bootstrap,
        parent,
        pit,
        cs,
        ss,
        flags,
        suspended,
        reservation,
        &actor,
        &admission,
        &turn,
        runqueue_policy::online_affinity_mask,
        |_, _| {},
    );
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}

#[allow(clippy::too_many_arguments)]
fn construct(
    id: u64,
    input: Input,
    mut bootstrap: UserTaskBootstrap,
    parent: Option<u64>,
    pit: u16,
    cs: u64,
    ss: u64,
    flags: u64,
    suspended: bool,
    reservation: process_table::SpawnReservation,
    actor: &Actor,
    admission: &Admission,
    turn: &nucleus_core::util::lockdep::DispatchPreemption,
    eligible_cpus: impl FnOnce(usize) -> u64,
    before_wake: impl FnOnce(usize, u64),
) -> Option<(bool, usize)> {
    match actor {
        Actor::Kernel(executor) => executor.assert_current_in(turn),
        Actor::User { slot, id, .. } => {
            assert_eq!(
                crate::multitask::cpu_local::current_cpu_task_slot(),
                Some(*slot)
            );
            assert!(stack_resources::cpu_leases::owns_current(turn.cpu(), *slot));
            assert_eq!(
                stack_resources::task_start::read(*slot).map(|s| s.id),
                Some(*id)
            );
        }
    }
    let vruntime = kernel_creation::creation_floor(turn.cpu());
    let ticket =
        (FIRST_DYNAMIC_TASK_SLOT..MAX_TASK).find_map(|slot| root_slots::reserve(slot, id))?;
    let mut root =
        match root_slots::RootPublication::prepare(reservation, ticket, admission.process_mask) {
            Ok(root) => root,
            Err(ticket) => {
                root_slots::cancel_unpublished(ticket);
                return None;
            }
        };
    let Some(state) = input.materialize(&mut bootstrap, id) else {
        root.cancel();
        return None;
    };
    let phys = state.address_space_root();
    let slot = root.ticket().slot();
    let mut scheduling = scheduling_context::SchedulingContext::bind(slot, id);
    if !scheduling.admit(
        admission.policy,
        admission.domain.slot(),
        admission.domain.generation(),
    ) {
        drop(state);
        root.cancel();
        return None;
    }
    let rsp = execution_image::reserved_user_entry(root.ticket(), &bootstrap, cs, ss, flags);
    let Some(handle) = root.publish_process(parent, state) else {
        scheduling_runtime::release(slot, scheduling.identity());
        root.cancel();
        return None;
    };
    let slot = publish(
        root,
        handle,
        bootstrap,
        scheduling,
        phys,
        rsp,
        pit,
        vruntime,
        suspended,
        eligible_cpus,
        admission,
        before_wake,
    );
    if let Actor::Kernel(executor) = actor {
        executor.assert_current_in(turn);
    }
    Some((actor.is_user(), slot))
}

#[allow(clippy::too_many_arguments)]
fn publish(
    root: root_slots::RootPublication,
    handle: process_table::ProcessHandle,
    bootstrap: UserTaskBootstrap,
    scheduling: scheduling_context::SchedulingContext,
    phys: u64,
    rsp: usize,
    pit: u16,
    vruntime: u64,
    suspended: bool,
    eligible_cpus: impl FnOnce(usize) -> u64,
    admission: &Admission,
    before_wake: impl FnOnce(usize, u64),
) -> usize {
    let slot = root.ticket().slot();
    let id = root.ticket().task_id();
    // The exact Reserved writer's returned fixed ABI RSP was retained through
    // pinned PID publication; no pointer is recomputed from unowned metadata.
    let base = stack_resources::storage_base(slot);
    let kernel_base = (base + TASK_STACK_GUARD_BYTES) as u64;
    let kernel_top = align_kernel_stack_top(base + TASK_STACK_SIZE) as u64;
    let weight = task_weight::weight_from_pit_divisor(pit);
    root.begin_commit();
    let context = TaskContext {
        scheduling_context: scheduling,
        #[cfg(test)]
        saved_rsp: rsp as usize,
        #[cfg(test)]
        address_space_root: phys,
        user_mode: true,
        user_abi: Some(bootstrap.abi),
        console_session: bootstrap.console_session,
        process_handle: Some(handle),
        process_id: Some(id),
        user_stack: bootstrap.user_stack,
        windows_thread_state: bootstrap.windows_thread_state.map(|state| WindowsThreadIdentity {
            thread_id: id,
            teb_address: state.teb_address,
        }),
    };
    root.install_context(context);
    runqueue::initialize_vruntime(slot, vruntime);
    runqueue::initialize_exec_start_ticks(slot, 0);
    runqueue::weight::initialize(slot, weight);
    runqueue::address_space::initialize(slot, phys);
    runqueue::wait::initialize(slot);
    if suspended {
        runqueue::wait::set_blocked_since_ticks(slot, crate::arch::rtc::ticks());
    } else {
        runqueue::wait::set_ready_since_ticks(slot, crate::arch::rtc::ticks());
    }
    runqueue::initialize_saved_rsp(slot, rsp as usize);
    runqueue::initialize_kernel_stack_bounds(slot, kernel_base, kernel_top);
    runqueue::initialize_alternate_kernel_stack_bounds(slot);
    runqueue::simd_tls::initialize_simd_state(slot, id);
    root.publish_start(TaskStart {
        entry: super::super::noop_task_entry,
        id,
    });
    let view = PublishedFrameView {
        user_mode_task: true,
        kernel_process_task: false,
        kernel_stack_base: kernel_base,
        kernel_stack_top: kernel_top,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        stack_canary_intact: stack_resources::owned_canary_intact(slot)
            .expect("root producer lost guard"),
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    stack_resources::publish_binding(slot, id, view);
    frame_publication::publish(
        slot,
        rsp as usize,
        kernel_base,
        kernel_top,
        0,
        0,
        true,
        false,
        view.stack_canary_intact,
        0,
        0,
    );
    task_directory::record(id, slot);
    let policy = admission.policy;
    let pager_charge = Some(PagerChargeStamp {
        context_slot: scheduling.identity().slot(),
        context_generation: scheduling.identity().generation(),
        scheduling_domain: policy.domain,
        domain_generation: u64::from(admission.domain.generation()),
        policy_epoch: policy.policy_epoch,
        period_ns: policy.period_ns,
    });
    identity::publish_metadata(
        slot,
        Some(TaskIdentity {
            task_id: Some(id),
            user_mode: true,
            abi: Some(bootstrap.abi),
            process_handle: Some(handle),
            process_id: Some(id),
            console_session: bootstrap.console_session,
            pager_charge,
        }),
    );
    for (gate, value) in [
        (identity::LifecycleGate::Retired, false),
        (identity::LifecycleGate::Suspended, suspended),
        (identity::LifecycleGate::JobStopped, false),
        (identity::LifecycleGate::ExecQuiesced, false),
    ] {
        identity::update_live_lifecycle(slot, id, gate, value);
    }
    linux_thread_state::install_reserved(root.ticket(), bootstrap.linux_thread_state);
    assert!(runqueue::affinity_payload::initialize_reserved(
        root.ticket(), admission.task_mask, admission.process_mask,
    ), "root constructor lost exact affinity publication");
    let backing = root.finish_retaining(view);
    before_wake(slot, id);
    if !suspended {
        let target = runqueue_policy::affinity_target_cpu(slot, eligible_cpus(slot));
        let cpu = nucleus_core::util::lockdep::current_cpu_index();
        let wake = if target == cpu {
            runqueue::publish_local_wake(slot, target, weight)
        } else {
            runqueue::publish_remote_wake(slot, target, weight)
        };
        #[cfg(not(test))]
        if let runqueue::RemoteWakeOutcome::Published { cpu, notify: true } = wake {
            crate::multitask::irq::request_target_reschedule(cpu);
        }
        #[cfg(test)]
        let _ = wake;
    }
    drop(backing);
    slot
}
#[cfg(test)]
pub(in crate::multitask) mod tests;
