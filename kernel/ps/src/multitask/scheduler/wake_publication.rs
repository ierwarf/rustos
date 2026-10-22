//! Owner: exact UID wake lifetime from terminal admission through queue publication.
//! Boundary: Terminal50 -> Identity51 -> Resources53 capture, then loan-backed work.
//! Lifecycle: capture -> Current/Transition token or physical frame read -> publication.
//! Concurrency: no raw leaf spans queue/IRQ work; binding excludes terminal slot reuse.
//! Failure: reject foreign/sealed bindings; invalid frame transfers original stop owner.
//! Forbidden: saved Current frame reads, copied slot grants, or telemetry as preemption.
//! Evidence: actual physical/frame/queue owners; candidate lookup grants no execution.
use super::*;
use crate::multitask::{cpu_local, current_identity as identity};

/// A lifetime loan, NOT a fresh execution grant or a transferable CPU turn.
pub(super) struct Binding {
    pin: identity::execution::BindingPin,
    context: TaskContext,
}
impl Binding {
    pub(super) fn resolve(task_id: u64) -> Option<Self> {
        let candidate = task_directory::hint(task_id)
            .filter(|&slot| stack_resources::task_start::read(slot).map(|s| s.id) == Some(task_id))
            .or_else(|| {
                (0..MAX_TASK).find(|&slot| {
                    stack_resources::task_start::read(slot).map(|s| s.id) == Some(task_id)
                })
            });
        Self::capture(candidate?, task_id)
    }
    pub(super) fn capture(slot: usize, id: u64) -> Option<Self> {
        if !stack_resources::admission::wake_allowed(slot) {
            return None;
        }
        let pin = identity::execution::pin_wake(slot, id)?;
        Self::from_original(pin)
    }
    pub(super) fn from_original(pin: identity::execution::BindingPin) -> Option<Self> {
        let slot = pin.slot();
        if !stack_resources::admission::wake_allowed(slot) {
            return None;
        }
        let context = stack_resources::context::read(slot)?;
        if stack_resources::task_start::read(slot).map(|s| s.id) != Some(pin.id()) {
            return None;
        }
        Some(Self { pin, context })
    }
    pub(super) fn slot(&self) -> usize {
        self.pin.slot()
    }
    pub(super) fn original_pin(&self) -> &identity::execution::BindingPin {
        &self.pin
    }
    pub(super) fn id(&self) -> u64 {
        self.pin.id()
    }
    pub(super) fn context(&self) -> &TaskContext {
        &self.context
    }
}

/// Directory-free entry. Exact original masked CPU custody owns local reentry;
/// a directory hint/scan supplies only a candidate, never binding permission.
pub(in crate::multitask) fn wake_task(task_id: u64) -> bool {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    let result = wake(&preemption, task_id, &runqueue_policy::online_affinity_mask);
    if !result {
        // Refused while the target still waits: keep that exact wait for a
        // retry at this CPU's next scheduler entry instead of losing it.
        let _ = super::wake_retry::record_in(&preemption, task_id, None);
    }
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}

/// Observation only, after a refused call wake: does `task_id` still hold an
/// armed or committed receive phase for exactly `endpoint`? Grants no wake,
/// funding, frame or queue custody; the caller only republishes the endpoint
/// waiter entry its own enqueue consumed.
pub(in crate::multitask) fn receive_phase_on_endpoint(task_id: u64, endpoint: u64) -> bool {
    endpoint != 0
        && task_directory::candidates(task_id)
            .find(|&slot| stack_resources::task_start::read(slot).map(|s| s.id) == Some(task_id))
            .is_some_and(|slot| {
                runqueue::wait::reason(slot) == (runqueue::wait::REASON_ENDPOINT_RECEIVE, endpoint)
                    && (runqueue::wait::armed(slot) || runqueue::wait::blocked(slot))
            })
}

/// User-qualified entry retains the SAME original binding through every wake
/// effect. A task-directory candidate cannot turn a kernel task into a user wake.
pub(in crate::multitask) fn wake_user(task_id: u64) -> bool {
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    let result = wake_user_in(&scope, task_id, &runqueue_policy::online_affinity_mask);
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}
fn wake_user_in(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>, id: u64,
    eligible: &dyn Fn(usize) -> u64,
) -> bool {
    if !ipc_publication::scoped(scope) { return false; }
    let Some(binding) = task_directory::candidates(id).find_map(|slot| Binding::capture(slot, id))
        else { return false; };
    if !binding.context().user_mode { return false; }
    // SAFETY: original masked CPU and original user BindingPin survive all effects.
    let woken = unsafe { guarded_owned(binding, eligible) };
    if !woken {
        // Only a user target reaches here, so the retry cannot widen the
        // user-wake authority; it replays exactly the refused wait instance.
        let _ = super::wake_retry::record_in(scope, id, None);
    }
    woken
}

pub(in crate::multitask) fn wake(
    preemption: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    task_id: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> bool {
    if !preemption.is_masked()
        || preemption.cpu() != nucleus_core::util::lockdep::current_cpu_index()
    {
        return false;
    }
    let Some(binding) = Binding::resolve(task_id) else {
        return false;
    };
    // SAFETY: original borrowed masked owner excludes local reentry through
    // all effects. Actual BindingPin/backing and FrameReader arbitrate foreign
    // terminal/reuse and final claims, not the candidate or directory mutex.
    unsafe { guarded_owned(binding, eligible_cpus) }
}

/// Normal terminal reply: original CPU and binding survive wake AND exact FIFO
/// publication. Donation withdrawal precedes resolution even when wake refuses.
pub(in crate::multitask) fn reply(reply: u64, task_id: u64) -> bool {
    let mut preemption = nucleus_core::util::lockdep::DispatchPreemption::enter();
    preemption.mask_irqs();
    let result = reply_with(
        &preemption,
        reply,
        task_id,
        &runqueue_policy::online_affinity_mask,
    );
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}


pub(super) fn reply_with(
    preemption: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    reply: u64,
    task_id: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> bool {
    reply_switching(preemption, reply, task_id, eligible_cpus, None)
}

/// `reply_with` that may hand a same-CPU caller's frame loan to the dispatcher
/// through `switch`; any condition it cannot prove leaves the ordinary wake.
pub(super) fn reply_switching(
    preemption: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    reply: u64,
    task_id: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
    switch: Option<&mut Option<dispatch_claim::DirectSwitch>>,
) -> bool {
    if !preemption.is_masked()
        || preemption.cpu() != nucleus_core::util::lockdep::current_cpu_index()
    {
        return false;
    }
    let _ = donation_ledger::release_reply(reply, ipc_donation::DonationNamespace::IpcReply);
    let Some(binding) = Binding::resolve(task_id) else {
        return false;
    };
    if let Some(switch) = switch {
        if ipc_publication::reply_direct(preemption, reply, &binding, eligible_cpus, switch) {
            return true;
        }
    }
    // SAFETY: the original borrowed masked CPU survives the returned loan.
    let Ok(binding) = (unsafe {
        retained_wake_matching_sync(
            binding,
            Some((runqueue::wait::REASON_ENDPOINT_REPLY, reply)),
            eligible_cpus,
            |binding| publish_reply_binding(preemption, binding),
        )
    }) else {
        return false;
    };
    drop(binding);
    true
}

fn publish_reply_binding(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>, binding: &Binding,
) -> bool {
    let owner = runqueue::owner(binding.slot());
    if owner.state == runqueue::RunOwnerState::Running && owner.runnable
        && owner.cpu.is_some_and(|cpu| stack_resources::cpu_leases::owns_current(cpu, binding.slot()))
    {
        // Reverse reply won BEFORE the caller's original Current wait commit.
        // Its actual bank/return CAS owns continuation; never mint a FIFO token
        // for an executing frame or report failure after this successful wake.
        return true;
    }
    let token = ReplyWakeHandoff::from_owner(
        binding.slot(),
        binding.id(),
        runqueue::owner(binding.slot()),
    );
    let retained = token.is_some_and(|token| {
        let _publication = dispatch_priority::Publication::begin(scope, token.target_cpu());
        sync_handoff::enqueue_reply_wake(token)
    });
    // Retain ORIGINAL physical loan across both generation checks and FIFO leaf.
    // Terminal seal can forbid execution meanwhile, but cannot reuse its backing.
    retained
}

fn typed_scope_valid(scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>) -> bool {
    scope.is_masked() && scope.cpu() == nucleus_core::util::lockdep::current_cpu_index()
}

pub(in crate::multitask) fn pager_reply(token: u64, task_id: u64) -> bool {
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    let result = pager_reply_with(
        &scope,
        token,
        task_id,
        &runqueue_policy::online_affinity_mask,
    );
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}
pub(super) fn pager_reply_with(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    token: u64,
    task_id: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> bool {
    if !typed_scope_valid(scope) || token == 0 {
        return false;
    }
    let _ = donation_ledger::release_reply(token, ipc_donation::DonationNamespace::PagerFault);
    let Some(binding) = Binding::resolve(task_id) else {
        return false;
    };
    // SAFETY: SAME original masked CPU and binding survive typed phase and FIFO.
    let Ok(binding) = (unsafe {
        retained_wake_matching_sync(
            binding,
            Some((runqueue::wait::REASON_PAGER_FAULT, token)),
            eligible_cpus,
            |binding| publish_reply_binding(scope, binding),
        )
    }) else {
        return false;
    };
    drop(binding);
    true
}

pub(in crate::multitask) fn pager_service(process_id: u64) -> Option<u64> {
    let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
    scope.mask_irqs();
    let result = pager_service_with(&scope, process_id, &runqueue_policy::online_affinity_mask);
    #[cfg(not(test))]
    crate::multitask::irq::flush_deferred_target_reschedules();
    result
}
pub(super) fn pager_service_with(
    scope: &nucleus_core::util::lockdep::DispatchPreemption<'_>,
    process_id: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> Option<u64> {
    if !typed_scope_valid(scope) || process_id == 0 {
        return None;
    }
    // A pager-service wait has no deadline: a wake refused in a transient
    // window would leave pagerd parked with the control request unserved.
    let mut refused = [0_u64; 4];
    let mut refused_len = 0;
    for slot in 0..MAX_TASK {
        // Advisory pre-filter: pinning a binding for every slot just to read
        // its process id dominated this search. The captured binding's exact
        // process id below remains the authority.
        if !super::super::current_identity::read_dispatch(slot)
            .is_some_and(|publication| publication.process_id == Some(process_id))
        {
            continue;
        }
        let Some(id) = stack_resources::task_start::read(slot).map(|s| s.id) else {
            continue;
        };
        let Some(binding) = Binding::capture(slot, id) else {
            continue;
        };
        if binding.context.process_id != Some(process_id) {
            continue;
        }
        // SAFETY: original masked CPU survives SAME physical binding and typed phase.
        match unsafe {
            retained_wake_matching(
                binding,
                Some((runqueue::wait::REASON_PAGER_SERVICE, 0)),
                eligible_cpus,
                |_| true,
            )
        } {
            Ok(binding) => {
                let id = binding.id();
                drop(binding);
                return Some(id);
            }
            Err(false) if refused_len < refused.len() => {
                refused[refused_len] = id;
                refused_len += 1;
            }
            Err(_) => {}
        }
    }
    // No worker woke: retry the first one still parked on this exact wait.
    // `record_in` ignores candidates that were refused for not waiting.
    let pager_service = Some((runqueue::wait::REASON_PAGER_SERVICE, 0));
    let _ = refused[..refused_len]
        .iter()
        .any(|&id| super::wake_retry::record_in(scope, id, pager_service));
    None
}

/// Complete original wake producer, independent of Scheduler data/authority.
///
/// # Safety
/// Caller retains original same-CPU masked IRQ/preemption through all effects.
/// BindingPin/backing retain UID/resources, FrameReader excludes final claims,
/// and Current/Transition wake arbitration preserves either return CAS order.
/// A snapshot of preemption counters is not a replacement for caller custody.
#[cfg(test)]
pub(super) unsafe fn guarded(slot: usize, id: u64, eligible_cpus: &dyn Fn(usize) -> u64) -> bool {
    let Some(binding) = Binding::capture(slot, id) else {
        return false;
    };
    // SAFETY: same original caller custody; transfer, not reacquisition.
    unsafe { guarded_owned(binding, eligible_cpus) }
}

/// # Safety
/// Same masked IRQ/preemption custody as `guarded`; consume the original
/// binding held through Linux signal publication, never resolve its slot anew.
pub(super) unsafe fn guarded_owned(binding: Binding, eligible_cpus: &dyn Fn(usize) -> u64) -> bool {
    // SAFETY: retain the original caller's masked custody. Ordinary callers
    // release the returned binding here; reply keeps it through FIFO publication.
    match unsafe { retained_wake(binding, eligible_cpus) } {
        Ok(_binding) => true,
        Err(success) => success,
    }
}

/// The successful producer returns the SAME loan, not a new slot lookup.
/// Invalid-frame terminal transfer consumes it before borrower drain instead.
/// # Safety
/// Original same-CPU masked IRQ/preemption custody survives all effects.
unsafe fn retained_wake(
    binding: Binding,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> Result<Binding, bool> {
    // SAFETY: forward the same original caller custody, without typed admission.
    unsafe { retained_wake_matching(binding, None, eligible_cpus, |_| true) }
}

/// # Safety
/// Same original caller CPU scope; typed pending phase never holds a leaf over
/// frame/queue work and the physical binding survives until phase release.
pub(super) unsafe fn retained_wake_matching(
    binding: Binding,
    expected: Option<(u8, u64)>,
    eligible_cpus: &dyn Fn(usize) -> u64,
    on_wake: impl FnOnce(&Binding) -> bool,
) -> Result<Binding, bool> {
    // SAFETY: forward the original caller custody without additional admission.
    unsafe {
        retained_wake_admitted(binding, expected, eligible_cpus, |_| true, on_wake, false, false)
    }
}

/// `retained_wake_matching` for a synchronous completion: the waker is about
/// to block (a server returning to its receive), so the woken task is placed
/// on this CPU when nothing else is queued here, instead of on its last CPU.
/// Each such cross-CPU wake cost a reschedule IPI and a halted-vCPU wakeup,
/// which dominated an eight-vCPU IPC round trip.
/// # Safety
/// Same as `retained_wake_matching`.
pub(super) unsafe fn retained_wake_matching_sync(
    binding: Binding,
    expected: Option<(u8, u64)>,
    eligible_cpus: &dyn Fn(usize) -> u64,
    on_wake: impl FnOnce(&Binding) -> bool,
) -> Result<Binding, bool> {
    // SAFETY: forwarded unchanged from the caller's obligations.
    unsafe {
        retained_wake_admitted(binding, expected, eligible_cpus, |_| true, on_wake, false, true)
    }
}

/// Exact receive phase encloses donation admission BEFORE any runnable effect.
/// The caller retains donor/root loans; corruption must therefore transfer its
/// original stop producer, never synchronously wait for those same loans.
/// # Safety
/// Same original masked CPU and retained physical sender through final effect.
pub(super) unsafe fn receive_call(
    binding: Binding,
    endpoint: u64,
    eligible_cpus: &dyn Fn(usize) -> u64,
    before_wake: impl FnOnce(&Binding) -> bool,
    on_wake: impl FnOnce(&Binding) -> bool,
) -> Result<Binding, bool> {
    // SAFETY: forward original masked CPU and physical receiver; caller keeps
    // its sender/funding loans until this same phase and last effect end.
    unsafe { retained_wake_admitted(binding,
        Some((runqueue::wait::REASON_ENDPOINT_RECEIVE, endpoint)), eligible_cpus,
        before_wake, on_wake, true, true) }
}

/// Sole physical wake implementation; admission is inside the original typed
/// pending phase and outside every raw leaf. No copied or recaptured wake loan.
unsafe fn retained_wake_admitted(
    binding: Binding,
    expected: Option<(u8, u64)>,
    eligible_cpus: &dyn Fn(usize) -> u64,
    before_wake: impl FnOnce(&Binding) -> bool,
    on_wake: impl FnOnce(&Binding) -> bool,
    defer_terminal: bool,
    sync_wake: bool,
) -> Result<Binding, bool> {
    let typed = if let Some((kind, id)) = expected {
        let Some(loan) = runqueue::wait::TypedWake::capture(&binding, kind, id) else {
            return Err(false);
        };
        Some(loan)
    } else {
        None
    };
    if !before_wake(&binding) {
        return Err(false);
    }
    let slot = binding.slot();
    let id = binding.id();
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let owner = runqueue::owner(slot);
    let blocked = runqueue::wait::blocked(slot);
    let armed = runqueue::wait::armed(slot);
    let blocked_since = runqueue::wait::blocked_since_ticks(slot);
    match cpu_local::task_execution_owner(slot) {
        Some(cpu_local::TaskExecutionOwner::Current(current_cpu)) => {
            // Never dereference the already consumed frame. Exact physical bank,
            // not scalar Current alone, retains the executing backing.
            if !stack_resources::cpu_leases::owns_current(current_cpu, slot) {
                return Err(false);
            }
            if !clear_wait(slot, typed.as_ref()) {
                return Err(false);
            }
            // The CPU may have published its outgoing Blocked queue custody
            // after our Current observation. Wake AFTER that return CAS must
            // publish queue custody too; clearing an arm alone would lose it.
            // Original BindingPin/backing survives publication and notification;
            // physical Current/Transition banks still exclude frame execution.
            if runqueue::owner(slot).state == runqueue::RunOwnerState::Blocked
                && matches!(
                    publish(slot, current_cpu, cpu),
                    runqueue::RemoteWakeOutcome::Rejected
                )
            {
                return Err(false);
            }
            runqueue::wait::set_ready_since_ticks(slot, 0);
            if !on_wake(&binding) {
                return Err(false);
            }
            drop(typed);
            return Ok(binding);
        }
        Some(cpu_local::TaskExecutionOwner::Transition(target)) => {
            // Typed clear precedes queue effects. Its pending phase excludes
            // later arms through publication; return CAS observes restored run
            // intent atomically. Generic wake retains mailbox-before-clear.
            if typed.is_some() && !clear_wait(slot, typed.as_ref()) {
                return Err(false);
            }
            // No read of the assembly-owned frame is allowed.
            if matches!(
                publish(slot, target, cpu),
                runqueue::RemoteWakeOutcome::Rejected
            ) {
                return Err(false);
            }
            if typed.is_none() {
                let _ = clear_wait(slot, None);
            }
            runqueue::wait::set_ready_since_ticks(slot, crate::arch::rtc::ticks().max(1));
            if !on_wake(&binding) {
                return Err(false);
            }
            drop(typed);
            return Ok(binding);
        }
        None => {}
    }
    let Some(token) = frame_publication::observed_frame_token(slot, cpu, false) else {
        return Err(false);
    };
    let Some(pin) = frame_publication::pin_for_identity(token, binding.pin.id()) else {
        return Err(false);
    };
    // SAFETY: original masked CPU custody excludes local reentry. This is
    // nonexecuting admission; actual Resource53 reader/final claim exclusion,
    // not a directory serializer, stabilizes the frame before dereference.
    let Some(reader) = (unsafe { stack_resources::frame_readers::acquire(&pin, cpu) }) else {
        return Err(false);
    };
    if frame_publication::observed_frame_token(slot, cpu, false).is_none_or(|now| {
        now.frame_epoch != token.frame_epoch
            || now.owner_generation != token.owner_generation
            || now.saved_rsp != token.saved_rsp
    }) {
        return Err(false);
    }
    let invalid =
        context_validation::validate_published_saved_context(slot, token.saved_rsp, token.view)
            .err();
    if let Some(reason) = invalid {
        if slot == ROOT_TASK_SLOT {
            panic!("scheduler root kernel context is corrupted: {}", reason);
        }
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "wake-invalid-context",
            id,
            token.saved_rsp as u64,
        );
        // Reserve while the exact binding is retained. Transfer that SAME
        // original non-Copy producer, never re-resolve a bare slot after Drop.
        let stop = terminal_quarantine::begin(
            slot,
            Some(id),
            TaskRetireReason::CorruptedContext {
                saved_rsp: token.saved_rsp,
                reason,
                reason_code: context_validation_reason_code(reason),
            },
            false,
        );
        drop(reader);
        drop(pin);
        drop(typed);
        drop(binding);
        if let Some(stop) = stop {
            if !defer_terminal && nucleus_core::util::lockdep::irq_context_depth() == 0 {
                // SAFETY: original caller scope survives transfer; own reader
                // and binding are released before terminal borrower drain.
                unsafe {
                    terminal_publication::guarded(stop, eligible_cpus);
                }
            } else {
                terminal_quarantine::publish(stop);
            }
        }
        return Err(true);
    }
    let already = runqueue::wake_is_already_runnable(owner, blocked, armed);
    let now = crate::arch::rtc::ticks();
    let woken_class = class(slot);
    let system = woken_class == SchedClass::System;
    let floor = kernel_creation::ready_floor(cpu, Some(system));
    if !clear_wait(slot, typed.as_ref()) {
        return Err(false);
    }
    if !already {
        runqueue::wait::set_ready_since_ticks(slot, now);
        runqueue::raise_vruntime_floor(slot, floor.saturating_sub(SLEEPER_LATENCY_BONUS_NS));
    }
    if blocked_since != 0 {
        record_blocked_wait(slot, id, binding.context.process_id, blocked_since, now);
    }
    let lifecycle = identity::lifecycle(slot);
    let runnable = !lifecycle.retired && !lifecycle.job_stopped && !lifecycle.exec_quiesced;
    if runnable {
        let target = if sync_wake {
            runqueue_policy::sync_wake_target_cpu(slot, eligible_cpus(slot))
        } else {
            runqueue_policy::affinity_target_cpu(slot, eligible_cpus(slot))
        };
        let outcome = publish(slot, target, cpu);
        if matches!(outcome, runqueue::RemoteWakeOutcome::Rejected) {
            return Err(false);
        }
        if let Some(current) = cpu_local::current_cpu_task_slot().filter(|current| *current != slot)
        {
            let current_class = class(current);
            if woken_class < current_class
                || (woken_class == current_class
                    && runqueue::vruntime(current).saturating_sub(runqueue::vruntime(slot))
                        >= SCHED_MIN_GRANULARITY_NS)
            {
                if let Some(target) = runqueue::owner(slot).cpu {
                    #[cfg(not(test))]
                    crate::multitask::irq::request_target_reschedule(target);
                    #[cfg(test)]
                    let _ = target;
                }
            }
        }
    }
    // Reader, original BindingPin and typed phase survive queue AND FIFO effects.
    if !on_wake(&binding) {
        return Err(false);
    }
    drop(reader);
    drop(pin);
    drop(typed);
    Ok(binding)
}

fn clear_wait(slot: usize, typed: Option<&runqueue::wait::TypedWake<'_>>) -> bool {
    if let Some(typed) = typed {
        typed.clear()
    } else {
        runqueue::wait::clear_wake(slot);
        true
    }
}

fn class(slot: usize) -> SchedClass {
    if runqueue::is_idle_slot(slot) {
        SchedClass::Idle
    } else if runqueue::weight::value(slot) & SYSTEM_CLASS_WEIGHT_FLAG != 0
        || donation_ledger::inherited_system(slot)
    {
        SchedClass::System
    } else {
        SchedClass::User
    }
}

fn publish(slot: usize, target: usize, cpu: usize) -> runqueue::RemoteWakeOutcome {
    let weight = runqueue::weight::value(slot);
    let outcome = if target == cpu {
        runqueue::publish_local_wake(slot, target, weight)
    } else {
        runqueue::publish_remote_wake(slot, target, weight)
    };
    #[cfg(not(test))]
    if let runqueue::RemoteWakeOutcome::Published { cpu, notify: true } = outcome {
        crate::multitask::irq::request_target_reschedule(cpu);
    }
    outcome
}

fn record_blocked_wait(slot: usize, id: u64, process: Option<u64>, since: u64, now: u64) {
    let Some(process) = process.filter(|pid| *pid != 0) else {
        return;
    };
    let elapsed_ms = now.saturating_sub(since).saturating_mul(1000)
        / crate::arch::rtc::ticks_per_second().max(1);
    if elapsed_ms < LONG_BLOCKED_WAIT_THRESHOLD_MS {
        return;
    }
    let peer_slot = cpu_local::current_cpu_task_slot().unwrap_or(ROOT_TASK_SLOT);
    let peer = identity::read(peer_slot);
    nucleus_core::util::lockdep::record_scheduler_observation(
        nucleus_core::util::lockdep::current_cpu_index(),
        nucleus_core::util::lockdep::SchedulerObservation {
            kind: nucleus_core::util::lockdep::SchedulerObservationKind::BlockedWait,
            subject_task: id,
            subject_pid: process,
            subject_slot: slot,
            peer_task: peer.and_then(|p| p.task_id).unwrap_or(0),
            peer_pid: peer.and_then(|p| p.process_id).unwrap_or(0),
            peer_slot,
            elapsed_ms,
            state_flags: 0,
            ready_since_ticks: 0,
            blocked_since_ticks: since,
        },
    );
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod typed_tests;
