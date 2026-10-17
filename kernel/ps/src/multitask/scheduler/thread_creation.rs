//! Owner: exact user-thread Ticket, original MM loan and parent physical Cohort.
//! Boundary: independent reservation, unpublished cancellation and suspended publication.
//! Lifecycle: process attachment and old MM survive through exact Slot ACK or cancellation ACK.
//! Concurrency: IRQ-excluded producer; C55 Join and parent Cohort exclude inheritance replacement.
//! Failure: untouched refusal cancels; abandoned reservation retains MM with poisoned real Ticket.
//! Forbidden: Scheduler facade, directory recovery, copied parent as lifetime or implicit reuse ACK.
//! Evidence: actual Ticket/resource/MM/membership/frame/runtime/queue owner witnesses.
use super::*;
use crate::multitask::current_identity as identity;
use crate::multitask::process_table::MmExecutionPin;
use stack_resources::admission::Ticket;

pub(in crate::multitask) struct Reservation {
    ticket: Option<Ticket>,
    mm: Option<MmExecutionPin>,
    parent: (usize, u64),
    handle: ProcessHandle,
    pid: u64,
    root: u64,
    weight: u32,
    vruntime: u64,
    policy: Option<scheduling_context::SchedulingContextPolicy>,
    domain: Option<usize>,
    domain_generation: Option<u32>,
    live: bool,
}
impl Reservation {
    fn ticket(&self) -> &Ticket {
        self.ticket.as_ref().expect("thread producer lost ticket")
    }
    pub(in crate::multitask) fn process_handle(&self) -> ProcessHandle {
        self.handle
    }
    pub(in crate::multitask) fn process_id(&self) -> u64 {
        self.pid
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.live {
            // The real Ticket's Drop leaves producer occupancy poisoned. Keep
            // the original physical MM too; abandonment is never cancellation.
            if let Some(mm) = self.mm.take() {
                core::mem::forget(mm);
            }
        }
    }
}

pub(in crate::multitask::scheduler) fn capture_creator(
    slot: usize,
) -> Option<identity::execution::BindingPin> {
    let context = stack_resources::context::read(slot)?;
    let start = stack_resources::task_start::read(slot)?;
    if !context.user_mode {
        return None;
    }
    identity::execution::pin_creation(slot, start.id, context.process_handle?, context.user_abi?)
}

fn reserve_with(id: u64, creator: &identity::execution::BindingPin) -> Option<Reservation> {
    let parent = (creator.slot(), creator.id());
    let context = stack_resources::context::read(parent.0)?;
    if stack_resources::task_start::read(parent.0)?.id != parent.1 || !context.user_mode {
        return None;
    }
    let handle = context.process_handle?;
    let pid = context.process_id?;
    let root = runqueue::address_space::root(parent.0);
    let mm = stack_resources::context::pin_thread_reservation_mm(parent.0, parent.1, handle, root)?;
    for slot in FIRST_DYNAMIC_TASK_SLOT..MAX_TASK {
        let Some(ticket) = stack_resources::admission::reserve(slot, id) else {
            continue;
        };
        if root_slots::prepare_storage(&ticket).is_none() {
            assert!(ticket.begin_cancel());
            drop(ticket.take_storage());
            ticket.finish_cancel();
            return None;
        }
        if process_table::attach_task(handle).is_none() {
            assert!(ticket.begin_cancel());
            drop(ticket.take_storage());
            ticket.finish_cancel();
            return None;
        }
        return Some(Reservation {
            ticket: Some(ticket),
            mm: Some(mm),
            parent,
            handle,
            pid,
            root,
            weight: runqueue::weight::value(parent.0),
            vruntime: runqueue::vruntime(parent.0)
                .saturating_add(SCHED_NEW_TASK_VRUNTIME_PENALTY_NS),
            policy: context.scheduling_context.policy(),
            domain: context.scheduling_context.domain_slot(),
            domain_generation: context.scheduling_context.domain_generation(),
            live: true,
        });
    }
    None
}

pub(in crate::multitask) fn cancel(reservation: Reservation) {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    cancel_owned(reservation, &preemption);
}
fn cancel_owned(
    mut reservation: Reservation,
    _preemption: &nucleus_core::util::lockdep::DispatchPreemption,
) {
    assert!(
        reservation.live && reservation.ticket().is_reserved_current(),
        "thread cancel lost exact producer"
    );
    assert!(reservation.ticket().begin_cancel());
    // No leaf crosses process detachment or allocation destruction; the actual
    // Ticket and original MM remain occupied until both effects return.
    process_table::detach_task(reservation.handle)
        .expect("thread producer lost process attachment");
    drop(reservation.ticket().take_storage());
    reservation.ticket.take().unwrap().finish_cancel();
    reservation.live = false;
}

struct Publication {
    membership: thread_slots::CommitGrant,
    parent: identity::cohort::Cohort,
    task_mask: u64,
}
impl Publication {
    fn capture(r: &Reservation, abi: UserAbi) -> Option<Self> {
        if !r.live
            || stack_resources::context::read(r.ticket().slot()).is_some()
            || process_table::is_process_exiting(r.pid) != Some(false)
        {
            return None;
        }
        let membership = thread_slots::CommitGrant::capture_ticket(
            r.ticket(),
            r.handle,
            r.parent,
            r.root,
            abi,
            r.mm.as_ref()?,
        )?;
        // Capture actual parent backing and seal its task-mask writer before
        // using any copied inheritance. Process Join separately owns C55 mask.
        let parent = identity::cohort::Cohort::begin(&[r.parent])?;
        let context = stack_resources::context::read(r.parent.0)?;
        if stack_resources::task_start::read(r.parent.0)?.id != r.parent.1
            || !context.user_mode
            || context.user_abi != Some(abi)
            || context.process_handle != Some(r.handle)
            || context.process_id != Some(r.pid)
            || identity::lifecycle(r.parent.0).retired
        {
            return None;
        }
        let task_mask = runqueue::affinity_payload::affinity_snapshot(r.parent.0).0 & membership.mask();
        if task_mask == 0 {
            return None;
        }
        Some(Self {
            membership,
            parent,
            task_mask,
        })
    }
}

fn publish(
    mut r: Reservation,
    bootstrap: UserTaskBootstrap,
    cs: u64,
    ss: u64,
    flags: u64,
    mut publication: Publication,
    preemption: &nucleus_core::util::lockdep::DispatchPreemption,
    before_ack: impl FnOnce(&Reservation, &Publication),
) -> Option<(usize, u32)> {
    let slot = r.ticket().slot();
    let id = r.ticket().task_id();
    let mut scheduling_context = scheduling_context::SchedulingContext::bind(slot, id);
    // Admission failure is still untouched: no frame/descriptor effect yet.
    match (r.policy, r.domain, r.domain_generation) {
        (Some(policy), Some(domain), Some(generation)) => {
            if !scheduling_context.admit(policy, domain, generation) {
                drop(publication);
                cancel_owned(r, preemption);
                return None;
            }
        }
        (None, None, None) => {}
        _ => {
            drop(publication);
            cancel_owned(r, preemption);
            return None;
        }
    }
    publication.parent.begin_effects();
    let saved_rsp = execution_image::reserved_user_entry(r.ticket(), &bootstrap, cs, ss, flags);
    r.ticket().begin_commit();
    let base = stack_resources::storage_base(slot);
    let kernel_base = (base + TASK_STACK_GUARD_BYTES) as u64;
    let kernel_top = align_kernel_stack_top(base + TASK_STACK_SIZE) as u64;
    let context = TaskContext {
        scheduling_context,
        #[cfg(test)]
        saved_rsp,
        #[cfg(test)]
        test_ready: false,
        #[cfg(test)]
        ready_since_ticks: 0,
        #[cfg(test)]
        blocked: true,
        #[cfg(test)]
        blocked_since_ticks: crate::arch::rtc::ticks(),
        #[cfg(test)]
        wake_armed: false,
        #[cfg(test)]
        block_reason: BlockReason::None,
        #[cfg(test)]
        weight: r.weight,
        #[cfg(test)]
        vruntime_ns: r.vruntime,
        #[cfg(test)]
        exec_start_ticks: 0,
        #[cfg(test)]
        address_space_root: r.root,
        #[cfg(test)]
        kernel_stack_base: kernel_base,
        #[cfg(test)]
        kernel_stack_top: kernel_top,
        #[cfg(test)]
        alternate_kernel_stack_base: 0,
        #[cfg(test)]
        alternate_kernel_stack_top: 0,
        user_mode: true,
        user_abi: Some(bootstrap.abi),
        console_session: bootstrap.console_session,
        process_handle: Some(r.handle),
        process_id: Some(r.pid),
        user_stack: bootstrap.user_stack,
        windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
            state.thread_id = id;
            state
        }),
    };
    r.ticket().install_context(context);
    runqueue::initialize_vruntime(slot, r.vruntime);
    runqueue::initialize_exec_start_ticks(slot, 0);
    runqueue::weight::initialize(slot, r.weight);
    runqueue::address_space::initialize(slot, r.root);
    runqueue::wait::initialize(slot);
    runqueue::wait::set_blocked_since_ticks(slot, crate::arch::rtc::ticks());
    runqueue::initialize_saved_rsp(slot, saved_rsp);
    runqueue::initialize_kernel_stack_bounds(slot, kernel_base, kernel_top);
    runqueue::initialize_alternate_kernel_stack_bounds(slot);
    runqueue::simd_tls::initialize_simd_state(slot, id);
    r.ticket().publish_start(TaskStart {
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
            .expect("thread producer lost stack guard"),
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    stack_resources::publish_binding(slot, id, view);
    frame_publication::publish(
        slot,
        saved_rsp,
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
    let pager_charge = context
        .scheduling_context
        .policy()
        .map(|policy| PagerChargeStamp {
            context_slot: context.scheduling_context.identity().slot(),
            context_generation: context.scheduling_context.identity().generation(),
            scheduling_domain: policy.domain,
            domain_generation: u64::from(context.scheduling_context.domain_generation().unwrap()),
            policy_epoch: policy.policy_epoch,
            period_ns: policy.period_ns,
        });
    identity::publish_metadata(
        slot,
        Some(TaskIdentity {
            task_id: Some(id),
            user_mode: true,
            abi: Some(bootstrap.abi),
            process_handle: Some(r.handle),
            process_id: Some(r.pid),
            console_session: bootstrap.console_session,
            pager_charge,
        }),
    );
    for (gate, value) in [
        (identity::LifecycleGate::Retired, false),
        (identity::LifecycleGate::Suspended, true),
        (identity::LifecycleGate::JobStopped, false),
        (identity::LifecycleGate::ExecQuiesced, false),
    ] {
        identity::update_live_lifecycle(slot, id, gate, value);
    }
    linux_thread_state::install_reserved(r.ticket(), bootstrap.linux_thread_state);
    assert!(runqueue::affinity_payload::initialize_reserved(
        r.ticket(), publication.task_mask, publication.membership.mask(),
    ), "thread constructor lost exact affinity publication");
    publication.membership.register(r.ticket());
    runqueue::admit_blocked(slot);
    // The complete hidden header joins its initial Blocked owner before the
    // physical Ticket admits any reader; no stale Dormant epoch escapes ACK.
    frame_publication::bind_local_owner_generation(slot, runqueue::owner(slot).generation);
    before_ack(&r, &publication);
    r.ticket.take().unwrap().finish_commit();
    r.live = false;
    publication.parent.complete();
    Some((slot, r.weight))
}

pub(in crate::multitask) fn commit(
    reservation: Reservation,
    bootstrap: UserTaskBootstrap,
    cs: u64,
    ss: u64,
    flags: u64,
) -> Option<(usize, u32)> {
    let result = {
        let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
        preemption.mask_irqs();
        match Publication::capture(&reservation, bootstrap.abi) {
            Some(publication) => publish(
                reservation,
                bootstrap,
                cs,
                ss,
                flags,
                publication,
                &preemption,
                |_, _| {},
            ),
            None => {
                cancel_owned(reservation, &preemption);
                None
            }
        }
    };
    super::super::irq::flush_deferred_target_reschedules();
    result
}

#[cfg(test)]
pub(super) mod tests;
pub(in crate::multitask) mod work;
