//! Owned task-context metadata. Readers receive scalar snapshots, never a
//! reference into mutable Scheduler storage. Cross-owner construction/exec and
//! process reference transfers still require their enclosing transaction.

use super::super::{ConsoleSessionHandle, ProcessHandle, TaskContext, UserAbi, UserStackState};
use super::RESOURCES;
use core::sync::atomic::Ordering;

pub(in crate::multitask::scheduler) fn read(slot: usize) -> Option<TaskContext> {
    RESOURCES.get(slot)?.with(|resources| resources.context)
}

/// Qualifies the original participant's start and funding in ONE resource leaf.
/// The caller retains this non-Copy pin through the final donation publication;
/// this observation grants neither worker execution nor a new participant loan.
pub(in crate::multitask::scheduler) fn retained_scheduling_context(
    pin: &crate::multitask::current_identity::execution::BindingPin,
) -> Option<kernel_object::api::identity::ObjectIdentity> {
    RESOURCES.get(pin.slot())?.with(|resources| {
        if resources.start?.id != pin.id() { return None; }
        let context = resources.context?;
        if !context.scheduling_context.is_bound_to(pin.id()) { return None; }
        Some(context.scheduling_context.identity())
    })
}

/// Scalar reply custody ONLY: same actual resource binding/start/context leaf.
/// No execution, funding, frame or MM loan escapes this comparison. Caller
/// joins the original Identity51 publication before this Resources53 read.
pub(in crate::multitask::scheduler) fn reply_custody(
    slot: usize,
    id: u64,
    expected: kernel_object::api::identity::ObjectIdentity,
    identity_live: bool,
) -> Option<(bool, bool)> {
    RESOURCES.get(slot)?.with(|r| {
        if r.binding.map(|(uid, _)| uid) != Some(id) || r.start.map(|s| s.id) != Some(id) {
            return None;
        }
        let context = r.context?;
        let bound = context.scheduling_context.is_bound_to(id);
        let matches = bound && context.scheduling_context.identity() == expected;
        let live = bound
            && identity_live
            && !super::super::runqueue::owner(slot).state.is_terminal();
        Some((matches, live))
    })
}

/// Snapshot under the already-retained physical Current/Prepared allocation.
/// No new MM or identity reference is acquired, including after terminal close.
pub(in crate::multitask::scheduler) fn retained_architecture(
    cpu: usize, slot: usize, id: u64, view: super::super::PublishedFrameView,
) -> Option<super::super::return_architecture::ArchitectureState> {
    if !(super::cpu_leases::owns_current(cpu, slot) || super::cpu_leases::owns_prepared(cpu, slot)) {
        return None;
    }
    let cell = RESOURCES.get(slot)?;
    cell.with(|r| architecture_locked(cell, r, slot, id, view))
}

pub(super) fn architecture_locked(
    cell: &super::ResourceCell, r: &super::Resources, slot: usize, id: u64,
    view: super::super::PublishedFrameView,
) -> Option<super::super::return_architecture::ArchitectureState> {
        let c = r.context?;
        if cell.pins.load(Ordering::Acquire) == 0
            || (r.start?.id, r.binding?) != (id, (id, super::Geometry::from_view(view)))
            || r.reservation.is_some() || r.frame_writing || r.reclaim.is_some()
            || view.user_mode_task != c.user_mode
            || view.kernel_process_task != (!c.user_mode && c.process_handle.is_some()) {
            return None;
        }
        let root = super::super::runqueue::address_space::root(slot);
        match c.process_handle {
            Some(handle) if !r.mm_pin.as_ref().is_some_and(|mm| mm.matches(handle, root)) => return None,
            None if c.user_mode || r.mm_pin.is_some() => return None,
            _ => {}
        }
        Some(super::super::return_architecture::ArchitectureState {
            root,
            stack_top: view.kernel_stack_top,
            fs: super::super::runqueue::simd_tls::tls_fs_base(slot),
            gs: c.windows_thread_state.map(|s| s.teb_address).unwrap_or(0),
            process: c.process_handle.is_some(),
        })
}

/// Physical old-MM backing across a sleepable unpublished thread reservation.
/// No table/process lock enters Resource53; activation is acquired only at commit.
pub(in crate::multitask::scheduler) fn pin_thread_reservation_mm(
    slot: usize,
    task_id: u64,
    handle: ProcessHandle,
    root: u64,
) -> Option<crate::multitask::process_table::MmExecutionPin> {
    RESOURCES.get(slot)?.with(|resources| {
        if resources.closing || resources.start.map(|start| start.id) != Some(task_id) {
            return None;
        }
        let context = resources.context?;
        if !context.user_mode || context.process_handle != Some(handle) {
            return None;
        }
        let mm = resources.mm_pin.as_ref()?;
        if mm.root() != root {
            return None;
        }
        mm.try_clone()
    })
}

/// Scalar metadata for this CPU's executing task. IRQ exclusion stabilizes
/// Current; its parked allocation reference prevents clearing/rebinding the
/// joined context/start record. This is not fresh dispatch or MM authority.
fn current() -> Option<(super::super::TaskStart, TaskContext)> {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let slot = crate::multitask::cpu_local::current_cpu_task_slot()?;
    joined_current(slot, super::cpu_leases::owns_current(cpu, slot))
}

pub(in crate::multitask) fn current_user_abi() -> Option<UserAbi> {
    current()
        .filter(|(_, context)| context.user_mode && context.process_handle.is_some())
        .and_then(|(_, context)| context.user_abi)
}

pub(in crate::multitask) fn current_user_stack_state() -> Option<UserStackState> {
    current()
        .filter(|(_, context)| context.user_mode)
        .and_then(|(_, context)| context.user_stack)
}

pub(in crate::multitask) fn current_console_session() -> Option<ConsoleSessionHandle> {
    current().map(|(_, context)| context.console_session)
}

pub(in crate::multitask) fn current_user_binding()
-> Option<(u64, UserAbi, ProcessHandle, ConsoleSessionHandle)> {
    let (start, context) = current()?;
    context.user_mode.then_some(())?;
    Some((
        start.id,
        context.user_abi?,
        context.process_handle?,
        context.console_session,
    ))
}

/// Exact entry under IRQ-stable allocation-bank Current custody, not directory recovery.
pub(in crate::multitask) fn current_task_start() -> Option<super::super::TaskStart> {
    current().map(|(start, _)| start)
}

pub(in crate::multitask) fn current_user_log_ids() -> Option<(u64, u64)> {
    let (start, context) = current()?;
    context.user_mode.then_some(())?;
    Some((context.process_id?, start.id))
}

/// Scalar wait-key namespace only. No MM pointer, lease or blocking authority escapes.
/// The exact physical Current pin survives both context qualification and root read.
pub(in crate::multitask) fn current_user_wait_binding() -> Option<(u64, UserAbi, u64)> {
    let (start, context) = current()?;
    context.user_mode.then_some(())?;
    let slot = crate::multitask::cpu_local::current_cpu_task_slot()?;
    Some((start.id, context.user_abi?, super::super::runqueue::address_space::root(slot)))
}

pub(in crate::multitask) fn current_process_handle() -> Option<ProcessHandle> {
    current()?.1.process_handle
}

pub(in crate::multitask) fn current_process_id(handle: ProcessHandle) -> Option<u64> {
    let (_, context) = current()?;
    (context.process_handle == Some(handle))
        .then_some(context.process_id)
        .flatten()
}

fn joined_current(slot: usize, current: bool) -> Option<(super::super::TaskStart, TaskContext)> {
    if !current {
        return None;
    }
    RESOURCES.get(slot)?.with(|resources| {
        let start = resources.start?;
        let context = resources.context?;
        context
            .scheduling_context
            .is_bound_to(start.id)
            .then_some((start, context))
    })
}

/// Negative native fault injection only: keep original allocation/MM owners,
/// but corrupt the copied SC binding while no participant can hold that row.
/// No raw resource leaf spans the callback; unwind restores the original SC.
#[cfg(test)]
pub(in crate::multitask::scheduler) fn test_mismatched_context<R>(
    slot: usize, foreign_id: u64, f: impl FnOnce() -> R,
) -> R {
    struct Restore(usize, super::super::scheduling_context::SchedulingContext);
    impl Drop for Restore {
        fn drop(&mut self) {
            RESOURCES[self.0].with(|resources| {
                resources.context.as_mut().unwrap().scheduling_context = self.1;
            });
        }
    }
    assert_eq!(super::super::current_identity::execution::count(slot), 0);
    let original = RESOURCES[slot].with(|resources| {
        let context = resources.context.as_mut().unwrap();
        let original = context.scheduling_context;
        context.scheduling_context = super::super::scheduling_context::SchedulingContext::bind(slot, foreign_id);
        original
    });
    let _restore = Restore(slot, original);
    f()
}

pub(in crate::multitask::scheduler) fn install(slot: usize, context: TaskContext) {
    install_with_reservation(slot, context, None)
}
pub(super) fn install_with_reservation(
    slot: usize,
    context: TaskContext,
    reservation: Option<super::admission::Reservation>,
) {
    let process_pin = context.process_handle.map(|handle| {
        crate::multitask::process_table::pin_process_object(
            handle,
            context.process_id.expect("process context missing PID"),
        )
        .expect("task context missing exact process allocation custody")
    });
    let mm_pin = context.process_handle.map(|handle| {
        crate::multitask::process_table::pin_process_mm(
            handle,
            context.process_id.expect("MM context missing PID"),
        )
        .expect("task context missing exact MM execution custody")
    });
    RESOURCES[slot].with(|resources| {
        assert_eq!(
            resources.reservation, reservation,
            "foreign slot context installation"
        );
        assert_eq!(
            usize::try_from(context.scheduling_context.identity().slot()).ok(),
            Some(slot + 1),
            "task context installed in a foreign slot"
        );
        assert!(
            !resources.closing,
            "closing task context cannot be installed"
        );
        assert!(
            resources.context.is_none() && resources.start.is_none(),
            "task context replaced without teardown"
        );
        assert_eq!(
            RESOURCES[slot].pins.load(Ordering::Acquire),
            0,
            "task context installed over execution/frame custody"
        );
        assert!(
            resources.process_pin.is_none(),
            "process context custody overwritten"
        );
        assert!(resources.mm_pin.is_none(), "MM context custody overwritten");
        resources.context = Some(context);
        resources.process_pin = process_pin;
        resources.mm_pin = mm_pin;
    });
}

pub(in crate::multitask::scheduler) fn replace_exec_mm(
    slot: usize,
    task_id: u64,
    reservation: crate::multitask::process_table::ExecReservation,
    root: u64,
    incoming: crate::multitask::process_table::MmExecutionPin,
) {
    assert!(
        incoming.matches_exec(reservation, root),
        "foreign exec MM execution pin"
    );
    let mut incoming = Some(incoming);
    let old = RESOURCES[slot].with(|resources| {
        super::super::outgoing_image::assert_mutable(slot);
        assert!(
            resources.frame_writing
                && resources.start.is_some_and(|start| start.id == task_id)
                && resources.context.is_some_and(
                    |context| context.process_handle == Some(reservation.process_handle())
                ),
            "exec MM replacement lacks exact image custody"
        );
        core::mem::replace(&mut resources.mm_pin, incoming.take())
    });
    drop(old); // release after the leaf; no HAL/heap work in pin Drop
}
/// Non-Copy physical MM credential retained outside leaves during hardware work.
/// The CPU turn separately owns IRQ/preemption. This pin linearizes fresh
/// acquisition against MM close, but does not replace cold exec/quiescence
/// arbitration for the complete claim/activation/CPU-publication transaction.
pub(in crate::multitask::scheduler) struct ExecutionRootLease {
    root: u64,
    _pin: Option<crate::multitask::process_table::MmExecutionPin>,
    _cpu_affinity: core::marker::PhantomData<*mut ()>,
}
impl ExecutionRootLease {
    pub(in crate::multitask::scheduler) fn root(&self) -> u64 {
        self.root
    }
}
#[cfg(test)]
pub(in crate::multitask::scheduler) fn lease_execution_root(
    slot: usize,
    root: u64,
    cpu: usize,
) -> Option<ExecutionRootLease> {
    lease_root_inner(slot, root, cpu, false)
}
pub(in crate::multitask::scheduler) fn lease_for_architecture(
    slot: usize,
    root: u64,
    cpu: usize,
) -> Option<ExecutionRootLease> {
    lease_root_inner(slot, root, cpu, true)
}
fn lease_root_inner(
    slot: usize,
    root: u64,
    cpu: usize,
    activate: bool,
) -> Option<ExecutionRootLease> {
    if !(super::cpu_leases::owns_current(cpu, slot) || super::cpu_leases::owns_prepared(cpu, slot))
    {
        return None;
    }
    #[cfg(not(test))]
    if activate && !crate::multitask::current_identity::execution::banks::owns_execution(cpu, slot)
    {
        return None;
    }
    let (pin, activation) = RESOURCES.get(slot)?.with(|resources| {
        let context = resources.context?;
        let pin = match context.process_handle {
            Some(handle) => {
                let pin = resources.mm_pin.as_ref()?;
                if !pin.matches(handle, root) {
                    return None;
                }
                Some(Some(pin.try_clone()?))
            }
            None => (!context.user_mode && resources.mm_pin.is_none()).then_some(None),
        }?;
        let activation = if activate {
            super::activation::capture(resources, slot, cpu)?
        } else {
            None
        };
        Some((pin, activation))
    })?;
    if let Some(activation) = activation {
        if !crate::multitask::cpu_local::can_retain_mm_activation(cpu, &activation) {
            return None;
        }
        crate::multitask::cpu_local::retain_mm_activation(cpu, activation);
    }
    Some(ExecutionRootLease {
        root,
        _pin: pin,
        _cpu_affinity: core::marker::PhantomData,
    })
}

#[cfg(test)]
pub(in crate::multitask::scheduler) fn execution_root_is_current(
    slot: usize,
    root: u64,
    cpu: usize,
) -> bool {
    lease_execution_root(slot, root, cpu).is_some()
}

pub(in crate::multitask::scheduler) fn clear(slot: usize) {
    let (process_pin, mm_pin) = RESOURCES[slot].with(|resources| {
        assert!(
            !crate::multitask::current_identity::exec_intent::busy(slot),
            "context reset with cold exec intent custody"
        );
        assert_eq!(
            crate::multitask::current_identity::execution::count(slot),
            0,
            "context reset with captured lifecycle execution custody"
        );
        assert!(
            !resources.frame_writing,
            "task context reset with frame writer custody"
        );
        assert_eq!(
            RESOURCES[slot].pins.load(Ordering::Acquire),
            0,
            "task context reset with execution/frame custody"
        );
        assert!(
            resources.context.is_none() || resources.closing,
            "live task context reset without closing"
        );
        crate::multitask::process_placement::reclaim::unregister(&super::Membership { resources, slot });
        resources.context = None;
        (resources.process_pin.take(), resources.mm_pin.take())
    });
    drop(mm_pin); // atomic only; last reference queues the complete root
    drop(process_pin);
}

pub(in crate::multitask::scheduler) fn replace_exec(
    slot: usize,
    task_id: u64,
    abi: UserAbi,
    console: ConsoleSessionHandle,
    stack: Option<UserStackState>,
) -> Option<()> {
    RESOURCES[slot].with(|resources| {
        if super::super::outgoing_image::frozen(slot) { return None; }
        if resources.closing || resources.start.is_none_or(|start| start.id != task_id) {
            return None;
        }
        let context = resources.context.as_mut()?;
        if !context.user_mode || context.process_id != Some(task_id) {
            return None;
        }
        // Only exec fields change. Funding, process binding and Windows state
        // cannot be replayed from a stale whole-context snapshot.
        context.user_abi = Some(abi);
        context.console_session = console;
        context.user_stack = stack;
        Some(())
    })
}

pub(in crate::multitask::scheduler) fn detach_process(
    slot: usize,
    task_id: u64,
    expected: ProcessHandle,
) -> Option<()> {
    RESOURCES[slot].with(|resources| {
        if super::super::outgoing_image::frozen(slot) { return None; }
        if resources.closing || resources.start.is_none_or(|start| start.id != task_id) {
            return None;
        }
        let context = resources.context.as_mut()?;
        if context.process_handle != Some(expected) {
            return None;
        }
        // The terminal producer captures this reference into its immutable
        // kernel side-effect claim before clearing the descriptor's copy.
        context.process_handle = None;
        Some(())
    })
}

#[cfg(test)]
pub(super) mod tests;

#[cfg(test)]
mod activation_tests;
#[cfg(test)]
pub(in crate::multitask::scheduler) mod intent_tests;
#[cfg(test)]
mod mm_tests;
