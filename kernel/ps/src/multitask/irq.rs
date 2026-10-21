//! Per-CPU timer, software-reschedule, and reschedule-IPI entry ordering.
//!
//! - **Owner:** `kernel-ps` owns scheduling decisions; HAL owns interrupt
//!   acknowledgement and clockevent delivery.
//! - **Boundary:** Interrupt frames and deferred-reschedule requests enter the
//!   scheduler only at architecture-defined safe points.
//! - **Lifecycle:** Wake deadlines, publish/validate the trapped continuation,
//!   select, hand off, then acknowledge in the required order.
//! - **Concurrency:** Entry runs with interrupts excluded and tracked IRQ
//!   context; arbitrary kernel frames are not blindly preempted.
//! - **Failure:** Invalid user continuation retires the exact task; root/kernel
//!   continuation corruption is fatal.
//! - **Forbidden:** No policy in IRQ context, early acknowledgement that loses
//!   a deadline, or clearing a CPU's request without a same-CPU safe point.
//! - **Evidence:** `scheduler-lifecycle`, `scheduler-dispatch`, and
//!   `smp-reschedule-ipi-lifecycle`.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(not(test))]
use kernel_hal::api::cpu;
use x86_64::instructions::interrupts;

use super::{
    USER_RETURN_RESCHEDULE_ARMED, context::SavedContext,
    cpu_local::irq_entry::DispatchEntry,
    scheduler_initialized,
};

mod fast_ipc;

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(super) use test_support::{
    flush_deferred_target_reschedules, prepare_test_deferred_target_reschedule,
    reset_test_deferred_target_reschedule_flush_epoch, test_deferred_target_reschedule_flush_epoch,
    test_deferred_target_reschedule_pending_mask, test_deferred_target_reschedule_sent_mask,
};

static AP_FIRST_WORK_DISPATCH_RECORDED: [AtomicBool;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CPU_FIRST_CLOCKEVENT_RECORDED: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CPU_FIRST_USER_DISPATCH_RECORDED: [AtomicBool;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static CPU_FIRST_RESCHEDULE_IPI_RECORDED: [AtomicBool;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
static TARGET_RESCHEDULE_IPI_PENDING: AtomicU64 = AtomicU64::new(0);
static PERIODIC_FAIRNESS_TICKS: [core::sync::atomic::AtomicU8;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
// Explicit wake/block/handoff requests schedule immediately. A runnable peer
// that merely needs fair CPU time is sampled at this bounded per-CPU cadence,
// keeping the legacy global backend below its convoy knee until the per-CPU
// backend cutover. Eight 1 ms ticks remain below the 20 ms maximum fair burst.
const PERIODIC_FAIRNESS_INTERVAL_TICKS: u8 = 8;

pub fn timer_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::timer_interrupt_handler_addr()
}

pub fn rtc_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::rtc_interrupt_handler_addr()
}

pub fn software_schedule_interrupt_handler_addr() -> u64 {
    crate::lowlevel::interrupts::software_schedule_interrupt_handler_addr()
}

fn current_cpu_index() -> usize {
    let logical_index = nucleus_core::util::lockdep::current_cpu_index();
    assert!(
        logical_index < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
        "scheduler invariant: current logical CPU exceeds reschedule capacity"
    );
    logical_index
}

fn user_return_reschedule_flag(logical_index: usize) -> &'static AtomicU64 {
    USER_RETURN_RESCHEDULE_ARMED
        .get(logical_index)
        .expect("scheduler invariant: user-return CPU index out of range")
}

fn cpu_lifecycle_generation(logical_index: usize) -> u64 {
    #[cfg(test)]
    {
        assert!(logical_index < nucleus_core::util::lockdep::MAX_TRACKED_CPUS);
        return 1;
    }
    #[cfg(not(test))]
    {
        let logical_index = u8::try_from(logical_index)
            .expect("scheduler invariant: logical CPU index exceeds lifecycle identity");
        cpu::lifecycle_snapshot(logical_index)
            .filter(|snapshot| snapshot.state == cpu::CpuLifecycleState::Online)
            .map(|snapshot| snapshot.generation)
            .expect("scheduler invariant: reschedule target is not Online")
    }
}

fn set_local_deferred_reschedule() {
    let logical_index = current_cpu_index();
    let _ = super::reschedule_observation::publish_request(
        logical_index,
        cpu_lifecycle_generation(logical_index),
    );
}

const fn remote_notification_required(target: usize, current: usize) -> bool {
    target != current
}

const fn periodic_user_continuation_is_local(
    interrupted_user_frame: bool,
    runqueue_work_pending: bool,
    deferred_reschedule_pending: bool,
    user_return_reschedule_pending: bool,
) -> bool {
    interrupted_user_frame
        && !runqueue_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
}

const fn periodic_idle_continuation_is_local(
    interrupted_kernel_frame: bool,
    current_task_is_idle: bool,
    local_work_pending: bool,
    deferred_reschedule_pending: bool,
    user_return_reschedule_pending: bool,
) -> bool {
    interrupted_kernel_frame
        && current_task_is_idle
        && !local_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
}

fn retain_periodic_continuation(logical_index: usize, context_ptr: *const SavedContext) -> bool {
    // SAFETY: the interrupt stub supplies one complete frame for the duration
    // of this IRQ dispatch. Reading CS does not consume or mutate continuation
    // ownership.
    let interrupted_user_frame =
        unsafe { &*context_ptr }.cs == crate::arch::gdt::user_code_selector().0 as u64;
    let interrupted_kernel_frame = !interrupted_user_frame;
    let local_work_pending = super::scheduler::local_dispatch_work_pending(logical_index);
    let deferred_reschedule_pending = super::reschedule_observation::request_pending(logical_index);
    let user_return_reschedule_pending =
        user_return_reschedule_flag(logical_index).load(Ordering::Acquire) != 0;
    // An IRQ-off anonymous fault only raises this lock-free low-water bit.  A
    // timer that interrupted a complete user frame is the first legal place to
    // schedule the dedicated normal-context reserve producer; the fault PTE
    // was already installed and is never deferred to that producer.
    //
    // The user-frame condition is load-bearing, not descriptive. An
    // interrupted *kernel* frame has its own retention rule below
    // (`periodic_idle_continuation_is_local`), which exists because only the
    // idle task may be displaced that way; overriding it here would hand the
    // scheduler a kernel continuation it never agreed to give up. Fairness
    // accounting is reset on the way out for the same reason the non-fair
    // branch resets it: this tick did not spend the user slice it measures.
    if interrupted_user_frame
        && kernel_mm::api::frame_capability::pager_fault_frame_refill_requested()
    {
        PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
        return false;
    }
    let fair_dispatch_due = if local_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
        && interrupted_user_frame
    {
        let elapsed = PERIODIC_FAIRNESS_TICKS[logical_index]
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if elapsed >= PERIODIC_FAIRNESS_INTERVAL_TICKS {
            PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    } else {
        PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
        false
    };
    periodic_user_continuation_is_local(
        interrupted_user_frame,
        local_work_pending && fair_dispatch_due,
        deferred_reschedule_pending,
        user_return_reschedule_pending,
    ) || periodic_idle_continuation_is_local(
        interrupted_kernel_frame,
        super::cpu_local::current_cpu_task_is_idle(logical_index),
        local_work_pending,
        deferred_reschedule_pending,
        user_return_reschedule_pending,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RescheduleIpiGate {
    RetainRequest,
    AcknowledgeOnly,
    Dispatch,
}

const fn reschedule_ipi_gate(
    scheduler_ready: bool,
    preemption_disabled: bool,
    request_present: bool,
) -> RescheduleIpiGate {
    if !scheduler_ready || preemption_disabled {
        RescheduleIpiGate::RetainRequest
    } else if !request_present {
        RescheduleIpiGate::AcknowledgeOnly
    } else {
        RescheduleIpiGate::Dispatch
    }
}

pub(crate) fn install_interrupt_dispatch_callbacks() {
    crate::lowlevel::interrupts::register_context_switch_commit(
        super::cpu_local::commit_context_switch,
    );
    crate::lowlevel::interrupts::register_timer_interrupt_dispatch(timer_interrupt_dispatch);
    crate::lowlevel::interrupts::register_rtc_interrupt_dispatch(rtc_interrupt_dispatch);
    crate::lowlevel::interrupts::register_software_schedule_interrupt_dispatch(
        software_schedule_interrupt_dispatch,
    );
    crate::lowlevel::interrupts::register_saved_user_context_dispatch(saved_user_context_dispatch);
    crate::lowlevel::interrupts::register_reschedule_ipi_interrupt_dispatch(
        reschedule_ipi_interrupt_dispatch,
    );
}

extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    // The low-level timer assembly enters this function directly rather than
    // the generic PIC wrapper, so publish the IRQ context for the complete
    // clockevent, scheduler, PIT, and EOI transaction.
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    let logical_index = current_cpu_index();
    // Test the preemption gate before sleeper callbacks, debug decoration, or
    // even the lock-backed scheduler-initialized snapshot. An interrupt may
    // land in the small interval after a raw lock raised preemption depth but
    // before its lock class was published; that valid owner must simply defer
    // all scheduler-facing clockevent work to the next absolute tick.
    if nucleus_core::util::lockdep::preemption_disabled() {
        set_local_deferred_reschedule();
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // The deadline registry is one global clock base, not eight per-CPU timer
    // bases. Only its BSP/PIT owner may expire it. Letting every LAPIC tick
    // walk the same registry reissued each still-unacknowledged wake once per
    // CPU and formed an SMP thundering herd on the global scheduler lock.
    // Absolute deadlines make a delayed owner edge catch up without extending
    // the wait; AP clockevents remain independent scheduler preemption edges.
    if logical_index == 0 {
        crate::arch::rtc::service_clock_event();
    }
    if !scheduler_initialized() {
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // The common steady-state case on an otherwise idle CPU has exactly one
    // running user task and no queued or remotely published work. A periodic
    // clockevent cannot change that decision. Keep the CPU-local continuation
    // without bouncing the lifecycle-global scheduler cache line; a local
    // enqueue, remote mailbox edge, affinity/retirement request, or syscall-tail
    // request disables this fast path before the next selection point.
    if retain_periodic_continuation(logical_index, context_ptr) {
        complete_clockevent(logical_index);
        return context_ptr;
    }
    // Idle CPUs never poll foreign runqueues from a periodic interrupt. Wake
    // placement targets an exact CPU and publishes its mailbox edge; active
    // balancing is source-owned. Treating any foreign runnable as local work
    // made every idle CPU enter the lifecycle-global scheduler on every tick,
    // creating an O(CPUs x ticks) convoy even though none could claim that
    // foreign queue directly.
    //
    // A task may have published its blocked/retired state immediately before
    // completing a bounded raw-lock transaction. That state alone must not
    // authorize the timer path to switch stacks while the preemption guard is
    // live. The eventual explicit scheduler handoff below the lock remains the
    // sole transition point.
    let current_rsp = context_ptr as usize;
    // SAFETY: the interrupt stub supplied a complete CPU-local frame. The
    // original masked turn retains Current/Transition and physical banks;
    // independent owner leaves serialize their bounded cross-CPU effects.
    let (next_rsp, user_dispatch, runtime_profile, reschedule_goal) = unsafe {
        let mut entry = DispatchEntry::enter();
        if timer_interrupted_kernel_frame(context_ptr, &entry) {
            complete_clockevent(logical_index);
            return context_ptr;
        }
        // A user-frame clockevent is itself a generation-bound scheduling
        // request. Publish it before clearing coalesced deferred work so every
        // Online CPU can prove that an actual request, rather than merely an
        // interrupt arrival, reached a scheduler safe point.
        let logical_index = current_cpu_index();
        let _ = super::reschedule_observation::publish_request(
            logical_index,
            cpu_lifecycle_generation(logical_index),
        );
        let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
            .expect("timer request lost before same-CPU scheduling safe point");
        // ORDERING: Release completes same-CPU request consumption before the
        // selected continuation is restored.
        user_return_reschedule_flag(logical_index).store(0, Ordering::Release);
        super::scheduler::runtime_profile::record_entry(super::scheduler::SchedulerEntryCause::Timer);
        let runtime_profile = super::scheduler::runtime_profile::take_owned(entry.turn(), crate::arch::rtc::ticks());
        let (dispatch, _) = entry.dispatch(current_rsp, false, None);
        if logical_index == 0 {
            crate::arch::pit::set_divisor(0, dispatch.tick_divisor);
        }
        let user = entry.install(dispatch, false);
        (dispatch.next_rsp, user, runtime_profile, reschedule_goal)
    };

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::Timer,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    record_first_ap_work_dispatch(logical_index);
    complete_clockevent(logical_index);
    next_rsp as *mut SavedContext
}

fn record_first_ap_work_dispatch(logical_index: usize) {
    if logical_index == 0 {
        return;
    }
    let Some(witness) = nucleus_core::util::lockdep::scheduler_dispatch_witness(logical_index)
    else {
        return;
    };
    if witness.to_idle
        || AP_FIRST_WORK_DISPATCH_RECORDED[logical_index]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "smp-ap-first-work-dispatch",
        logical_index as u64,
        (witness.to_task << 32) | witness.to_slot as u64,
    );
}

fn complete_clockevent(logical_index: usize) {
    if logical_index == 0 {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
    } else {
        // Rearm only after all scheduler/debug work is complete. At high CPU
        // counts that work can exceed one 1 ms period; arming at entry then
        // makes the deadline expire while IF=0 and causes an immediate
        // interrupt on IRET, starving a newly selected user context before
        // its first instruction. Absolute catch-up still preserves monotonic
        // time while guaranteeing the next edge is strictly in the future.
        crate::arch::timer::arm_next_tick();
        crate::arch::msi::local_apic_eoi();
    }
    record_first_cpu_event(
        &CPU_FIRST_CLOCKEVENT_RECORDED,
        "smp-cpu-first-clockevent",
        logical_index,
    );
}

extern "C" fn rtc_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    // Like the PIT/LAPIC dispatcher, the low-level RTC stub enters this
    // function directly. Keep IRQ context published through wake processing,
    // scheduler selection, acknowledgement, and the final stack decision;
    // `rtc::on_interrupt`'s inner guard covers only device accounting.
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    crate::arch::rtc::on_interrupt();
    if nucleus_core::util::lockdep::preemption_disabled() {
        set_local_deferred_reschedule();
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }
    if !scheduler_initialized() {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    if retain_periodic_continuation(current_cpu_index(), context_ptr) {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    // Drain sleeper wakeups BEFORE the scheduler pick so newly-ready tasks
    // are visible to `dispatch_schedule`. The previous ordering ran the pick
    // first, then woke sleepers — so a sleeper whose target tick had just
    // arrived waited a full extra IRQ (and longer, since a heavier User-class
    // task could keep being picked over and over while the wake was stale).
    // Matches Linux's `tick_sched_timer` -> `update_process_times` -> wake
    // ordering, and the Fuchsia Zircon "wake then reschedule" pattern.
    let current_rsp = context_ptr as usize;
    // One original turn survives probe, wake/profile, dispatch and return.
    let mut entry = DispatchEntry::enter();
    let interrupted_kernel_frame = unsafe {
        timer_interrupted_kernel_frame(context_ptr, &entry)
    };
    if interrupted_kernel_frame {
        crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
        return context_ptr;
    }

    let logical_index = current_cpu_index();
    let _ = super::reschedule_observation::publish_request(
        logical_index,
        cpu_lifecycle_generation(logical_index),
    );
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
        .expect("RTC request lost before same-CPU scheduling safe point");
    let (next_rsp, user_dispatch, runtime_profile) = {
        super::scheduler::runtime_profile::record_entry(super::scheduler::SchedulerEntryCause::Rtc);
        let runtime_profile = super::scheduler::runtime_profile::take_owned(entry.turn(), crate::arch::rtc::ticks());
        let (dispatch, _) = entry.dispatch(current_rsp, false, None);
        let user = entry.install(dispatch, false);
        let result = (dispatch.next_rsp, user, runtime_profile);
        drop(entry);
        result
    };

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::Rtc,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
    next_rsp as *mut SavedContext
}

unsafe fn timer_interrupted_kernel_frame(context_ptr: *const SavedContext, entry: &DispatchEntry) -> bool {
    // SAFETY: IRQ stub owns this complete frame and the original masked entry
    // excludes local reentry. The borrowed Current bank, not this pointer, owns
    // task backing; no raw owner crosses this scalar interrupt-frame read.
    let cs = unsafe { (*context_ptr).cs };
    if !entry.kernel_continuation(cs) { return false; }
    set_local_deferred_reschedule();
    true
}

pub fn reschedule_if_requested() {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return;
    }
    let logical_index = current_cpu_index();
    if let Some(goal) = super::reschedule_observation::claim_pending(logical_index) {
        yield_now();
        super::reschedule_observation::record_consumption(
            logical_index,
            goal,
            super::reschedule_observation::RescheduleRoute::LocalSafePoint,
        );
    }
}

pub fn cond_resched() {
    reschedule_if_requested();
}

/// Claim a timer or latency-handoff request from the common syscall tail.
///
/// A successful claim authorizes syscall entry to publish its validated user
/// return image directly as a scheduler context. The low-level transfer still
/// performs the ordinary scheduler dispatch and outgoing-stack ownership
/// commit; only the redundant kernel interrupt continuation is absent.
pub fn claim_interruptible_syscall_reschedule() -> bool {
    if nucleus_core::util::lockdep::preemption_disabled() {
        return false;
    }
    let logical_index = current_cpu_index();
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index);
    if consume_syscall_tail_reschedule(
        reschedule_goal.is_some(),
        user_return_reschedule_flag(logical_index),
    ) {
        if let Some(reschedule_goal) = reschedule_goal {
            super::reschedule_observation::record_consumption(
                logical_index,
                reschedule_goal,
                super::reschedule_observation::RescheduleRoute::SyscallTail,
            );
        }
        true
    } else {
        false
    }
}

fn consume_syscall_tail_reschedule(
    deferred_claimed: bool,
    user_return: &core::sync::atomic::AtomicU64,
) -> bool {
    let user_return = user_return.swap(0, Ordering::AcqRel);
    deferred_claimed || user_return != 0
}

pub(crate) fn request_deferred_reschedule() {
    let source_cpu = current_cpu_index();
    let _ = super::reschedule_observation::publish_request(
        source_cpu,
        cpu_lifecycle_generation(source_cpu),
    );
}

/// Publish one scheduling request to the CPU that owns the runnable task.
///
/// Remote wake placement has already transferred exact task custody to the
/// target mailbox before this function is called.  Notification is therefore
/// a coalesced hint, not ownership: a same-CPU target consumes the durable bit
/// at its next safe point and a remote target receives exactly one private IPI
/// on the 0->1 edge.  Ordinary wakeups never broadcast to unrelated CPUs.
#[cfg(not(test))]
pub(super) fn request_target_reschedule(target: usize) {
    assert!(
        target < nucleus_core::util::lockdep::MAX_TRACKED_CPUS,
        "scheduler targeted reschedule CPU exceeds capacity"
    );
    let snapshot = {
        let logical_index = u8::try_from(target)
            .expect("scheduler targeted reschedule CPU exceeds lifecycle identity");
        cpu::lifecycle_snapshot(logical_index)
            .filter(|snapshot| snapshot.state == cpu::CpuLifecycleState::Online)
            .expect("scheduler targeted reschedule CPU is not Online")
    };
    let publication = super::reschedule_observation::publish_request(target, snapshot.generation);
    if !publication.newly_pending {
        return;
    }
    let source = current_cpu_index();
    if !remote_notification_required(target, source) {
        return;
    }
    if nucleus_core::util::lockdep::preemption_disabled() {
        // ORDERING: Release transfers notification custody to the scheduler
        // unlock safe point. The target request bit and mailbox payload were
        // both published before this bit becomes visible.
        TARGET_RESCHEDULE_IPI_PENDING.fetch_or(1_u64 << target, Ordering::Release);
        return;
    }
    send_target_reschedule_ipi(target, snapshot.generation, publication.sequence);
}

#[cfg(not(test))]
fn send_target_reschedule_ipi(target: usize, generation: u64, sequence: u64) {
    let descriptor = cpu::topology()
        .and_then(|topology| {
            topology
                .cpus()
                .iter()
                .find(|descriptor| usize::from(descriptor.logical_index) == target)
                .copied()
        })
        .expect("scheduler targeted reschedule CPU has no topology descriptor");
    super::reschedule_observation::record_notification(target, generation, sequence);
    crate::arch::msi::send_reschedule_ipi(descriptor.apic_id).unwrap_or_else(|error| {
        panic!(
            "scheduler invariant: targeted reschedule IPI to logical CPU {} APIC {} failed: {error:?}",
            descriptor.logical_index, descriptor.apic_id
        )
    });
}

/// Emit exact target notification edges after the scheduler raw lock has
/// actually been released. Multiple flushers are safe: each target bit
/// coalesces IPIs and a racing publication remains for the next safe point.
fn flush_deferred_target_reschedules_with(
    scheduler_ready: bool,
    current_cpu: usize,
    mut online_generation: impl FnMut(usize) -> Option<u64>,
    mut pending_sequence: impl FnMut(usize) -> Option<u64>,
    mut send: impl FnMut(usize, u64, u64),
) {
    if !scheduler_ready {
        return;
    }
    // ORDERING: AcqRel claims every target whose mailbox/request became
    // durable while a raw scheduler owner disabled preemption. New target bits
    // racing this swap remain for the next safe point.
    let targeted = TARGET_RESCHEDULE_IPI_PENDING.swap(0, Ordering::AcqRel);
    for target in 0..nucleus_core::util::lockdep::MAX_TRACKED_CPUS {
        if targeted & (1_u64 << target) == 0 {
            continue;
        }
        if !remote_notification_required(target, current_cpu) {
            // The durable local request is enough. xAPIC fixed self-IPIs are
            // forbidden and the return path is already a safe point.
            continue;
        }
        let generation = online_generation(target)
            .unwrap_or_else(|| panic!("scheduler deferred target is not Online: {target}"));
        if let Some(sequence) = pending_sequence(target) {
            send(target, generation, sequence);
        }
    }
}

#[cfg(not(test))]
pub(super) fn flush_deferred_target_reschedules() {
    flush_deferred_target_reschedules_with(
        scheduler_initialized(),
        current_cpu_index(),
        |target| {
            let logical_index =
                u8::try_from(target).expect("scheduler deferred target exceeds lifecycle identity");
            cpu::lifecycle_snapshot(logical_index)
                .filter(|snapshot| snapshot.state == cpu::CpuLifecycleState::Online)
                .map(|snapshot| snapshot.generation)
        },
        |target| {
            super::reschedule_observation::request_pending(target)
                .then(|| super::reschedule_observation::request_sequence_snapshot(target))
        },
        send_target_reschedule_ipi,
    );
}

/// Requests a voluntary switch in the common interruptible syscall tail.
pub(crate) fn request_user_return_reschedule() {
    // ORDERING: Release publishes the request to this CPU's syscall tail.
    user_return_reschedule_flag(current_cpu_index()).store(1, Ordering::Release);
}

/// `sched_yield`: a voluntary switch only when something else can run here.
/// Running tasks are absent from the runnable bitmap, so an empty queue, mailbox,
/// synchronous FIFO and activation window mean the dispatcher could only pick the
/// caller again; the common syscall tail then returns straight to user mode
/// instead of entering the scheduler (the periodic clockevent applies the same rule).
pub(crate) fn request_yield_reschedule() {
    let cpu = current_cpu_index();
    if super::scheduler::other_dispatch_work_pending(cpu) {
        // ORDERING: Release publishes the request to this CPU's syscall tail.
        user_return_reschedule_flag(cpu).store(1, Ordering::Release);
    }
}

extern "C" fn reschedule_ipi_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    let logical_index = current_cpu_index();
    // ORDERING: Acquire observes the target CPU's durable request publication
    // before choosing whether the notification may consume it.
    let request_present = super::reschedule_observation::request_pending(logical_index);
    let preemption_disabled = nucleus_core::util::lockdep::preemption_disabled();
    // An interrupted scheduler/raw lock may be the lock this callback would
    // otherwise acquire, so the preemption gate must be evaluated first.
    let scheduler_ready = !preemption_disabled && scheduler_initialized();
    match reschedule_ipi_gate(scheduler_ready, preemption_disabled, request_present) {
        RescheduleIpiGate::RetainRequest | RescheduleIpiGate::AcknowledgeOnly => {
            // RetainRequest leaves the durable bit armed. A syscall tail,
            // cond_resched, timer edge, or later IPI consumes it after the
            // interrupted raw-lock section.
            crate::arch::msi::local_apic_eoi();
            return context_ptr;
        }
        RescheduleIpiGate::Dispatch => {}
    }
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
        .expect("scheduler invariant: reschedule request disappeared on its target CPU");
    // The delivered IPI covers every request coalesced before this exact claim,
    // including a publication that raced the sender-side notify marker.
    super::reschedule_observation::record_notification(
        logical_index,
        cpu_lifecycle_generation(logical_index),
        reschedule_goal,
    );

    let current_rsp = context_ptr as usize;
    // SAFETY: the IPI stub supplied a complete CPU-local frame. Original
    // masked CPU custody excludes nesting; bounded independent owner leaves
    // serialize cross-CPU publication without lending the directory guard.
    let (next_rsp, user_dispatch, runtime_profile) = unsafe {
        let mut entry = DispatchEntry::enter();
        if timer_interrupted_kernel_frame(context_ptr, &entry) {
            crate::arch::msi::local_apic_eoi();
            return context_ptr;
        }
        super::scheduler::runtime_profile::record_entry(super::scheduler::SchedulerEntryCause::RescheduleIpi);
        let runtime_profile = super::scheduler::runtime_profile::take_owned(entry.turn(), crate::arch::rtc::ticks());
        let (dispatch, _) = entry.dispatch(current_rsp, true, None);
        let user = entry.install(dispatch, false);
        (dispatch.next_rsp, user, runtime_profile)
    };
    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::RemoteIpi,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    record_first_ap_work_dispatch(logical_index);
    crate::arch::msi::local_apic_eoi();
    record_first_cpu_event(
        &CPU_FIRST_RESCHEDULE_IPI_RECORDED,
        "smp-cpu-first-reschedule-ipi",
        logical_index,
    );
    next_rsp as *mut SavedContext
}

extern "C" fn software_schedule_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    assert!(
        !nucleus_core::util::lockdep::preemption_disabled(),
        "software scheduler entered while raw spin lock held depth={} class={:?}",
        nucleus_core::util::lockdep::preemption_depth(),
        nucleus_core::util::lockdep::current_lock_class()
    );
    if !scheduler_initialized() {
        return context_ptr;
    }
    // SAFETY: the software interrupt stub owns this complete frame until this
    // callback returns its selected continuation. Reading caller-saved words
    // does not transfer or mutate stack custody.
    let software_frame = unsafe { &*context_ptr };
    let fast_ipc_call = (software_frame.rax
        == crate::lowlevel::interrupts::FAST_IPC_CALL_SCHEDULE_TAG)
        .then_some((software_frame.rdi, software_frame.rsi, software_frame.rdx));
    let fast_ipc_reply = (software_frame.rax
        == crate::lowlevel::interrupts::FAST_IPC_REPLY_SCHEDULE_TAG)
        .then_some((
            software_frame.rdi,
            software_frame.rsi,
            software_frame.rdx,
            software_frame.rcx,
            software_frame.r8,
        ));
    let logical_index = current_cpu_index();
    // The software interrupt is an explicit local scheduling request. Bind it
    // to the current CPU lifecycle before opening its independent original turn,
    // then attest consumption after the selected frame is ready.
    let _ = super::reschedule_observation::publish_request(
        logical_index,
        cpu_lifecycle_generation(logical_index),
    );
    let reschedule_goal = super::reschedule_observation::claim_pending(logical_index)
        .expect("software scheduling request lost before dispatch");
    let current_rsp = context_ptr as usize;
    // A call between two resident tasks on this CPU switches here, without a
    // dispatch turn. Declining changes nothing; `Generic` has already committed
    // the caller and woken the receiver, so only the ordinary pick remains.
    let mut lane_generic = false;
    if let Some((endpoint, reply, receiver_task_id)) = fast_ipc_call {
        let mut lane_scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        lane_scope.mask_irqs();
        match super::scheduler::ipc_lane::call(&lane_scope, endpoint, reply, receiver_task_id, current_rsp) {
            super::scheduler::ipc_lane::Outcome::Switched(next_rsp) => {
                // The mask outlives this scope until the stub's iretq restores flags.
                lane_scope.retain_irq_mask();
                drop(lane_scope);
                let _ = fast_ipc::stamp_fast_ipc_call_outcome(
                    context_ptr, super::scheduler::FastIpcCallHandoffOutcome::CommittedSameCpu);
                return next_rsp as *mut SavedContext;
            }
            super::scheduler::ipc_lane::Outcome::Generic => lane_generic = true,
            super::scheduler::ipc_lane::Outcome::Declined => {}
        }
    }
    let mut lane_reply_woke = false;
    if let Some((reply, caller_task_id, context_owner_task_id, context_slot, context_generation)) =
        fast_ipc_reply
    {
        let scheduling_context = kernel_object::api::identity::ObjectIdentity::new(
            kernel_object::api::identity::ObjectOwner::Ps,
            kernel_object::api::identity::ObjectKind::SchedulingContext,
            context_slot,
            context_generation,
        )
        .expect("fast IPC reply trap carried an invalid scheduling-context identity");
        let mut lane_scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        lane_scope.mask_irqs();
        match super::scheduler::ipc_lane::reply(&lane_scope, reply, caller_task_id,
            context_owner_task_id, scheduling_context, current_rsp)
        {
            super::scheduler::ipc_lane::Outcome::Switched(next_rsp) => {
                lane_scope.retain_irq_mask();
                drop(lane_scope);
                return next_rsp as *mut SavedContext;
            }
            super::scheduler::ipc_lane::Outcome::Generic => lane_reply_woke = true,
            super::scheduler::ipc_lane::Outcome::Declined => {}
        }
    }
    let (next_rsp, user_dispatch, runtime_profile, fast_ipc_outcome) = {
        let mut entry = DispatchEntry::enter();
        entry.save_simd();
        super::scheduler::runtime_profile::record_entry(super::scheduler::SchedulerEntryCause::Software);
        let runtime_profile = super::scheduler::runtime_profile::take_owned(entry.turn(), crate::arch::rtc::ticks());
        // A committed same-CPU handoff may carry its receiver frame loan to the
        // dispatcher in `switch`; otherwise direct custody is recorded in the
        // sole FIFO. The scalar peer ID is only a hint either way.
        let mut switch = None;
        let fast_ipc_outcome = if lane_generic {
            Some(super::scheduler::FastIpcCallHandoffOutcome::CommittedSameCpu)
        } else {
            fast_ipc_call.map(|(endpoint, reply, receiver_task_id)| {
                super::scheduler::ipc_publication::call_switching(
                    entry.preemption(), endpoint, reply, receiver_task_id, &mut switch,
                )
            })
        };
        if fast_ipc_outcome.is_some_and(|outcome| {
            !matches!(
                outcome,
                super::scheduler::FastIpcCallHandoffOutcome::CommittedSameCpu
                    | super::scheduler::FastIpcCallHandoffOutcome::CommittedCrossCpu
            )
        }) {
            let outcome = fast_ipc_outcome.unwrap();
            drop(switch);
            drop(entry);
            super::reschedule_observation::record_consumption(
                logical_index,
                reschedule_goal,
                super::reschedule_observation::RescheduleRoute::LocalSafePoint,
            );
            return fast_ipc::stamp_fast_ipc_call_outcome(context_ptr, outcome);
        }
        let mut immediate_task_id = fast_ipc_call.filter(|_| !lane_generic).and_then(|(_, _, receiver_task_id)| {
            fast_ipc_outcome
                .is_some_and(|outcome| {
                    outcome == super::scheduler::FastIpcCallHandoffOutcome::CommittedSameCpu
                })
                .then_some(receiver_task_id)
        });
        if let Some((
            reply,
            caller_task_id,
            context_owner_task_id,
            context_slot,
            context_generation,
        )) = fast_ipc_reply
        {
            let scheduling_context = kernel_object::api::identity::ObjectIdentity::new(
                kernel_object::api::identity::ObjectOwner::Ps,
                kernel_object::api::identity::ObjectKind::SchedulingContext,
                context_slot,
                context_generation,
            )
            .expect("fast IPC reply trap carried an invalid scheduling-context identity");
            let outcome = if lane_reply_woke {
                // The lane already woke the caller through the ordinary reply wake.
                super::scheduler::FastIpcReplyHandoffOutcome::LocalFallback
            } else {
                super::scheduler::ipc_publication::reply_switching(
                    entry.preemption(),
                    reply,
                    caller_task_id,
                    context_owner_task_id,
                    scheduling_context,
                    &mut switch,
                )
                .expect("fast IPC reply trap carried stale scheduling-context custody")
            };
            // The producer publishes typed fair/FIFO custody, not a Direct
            // execution grant. Its successful outcome may request an attempt
            // through that SAME FIFO; requiring the removed Direct producer
            // here made every real reverse reply miss the fast return path.
            immediate_task_id = outcome.dispatch_hint(caller_task_id);
            // Rejection has already consumed/refused its ORIGINAL typed loan;
            // a generic fresh lookup must not clear a later wait incarnation.
        }
        // A committed same-CPU fastpath carries its exact peer directly
        // through this scheduler transaction. Generic and cross-CPU handoffs
        // retain the ordered FIFO path.
        let (dispatch, exact_fast_dispatch) =
            entry.dispatch_switching(current_rsp, true, immediate_task_id, switch);
        let user = entry.install(dispatch, exact_fast_dispatch);
        (dispatch.next_rsp, user, runtime_profile, fast_ipc_outcome)
    };

    if let Some(outcome) = fast_ipc_outcome {
        let _ = fast_ipc::stamp_fast_ipc_call_outcome(context_ptr, outcome);
    }

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    super::reschedule_observation::record_consumption(
        logical_index,
        reschedule_goal,
        super::reschedule_observation::RescheduleRoute::LocalSafePoint,
    );
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    next_rsp as *mut SavedContext
}

/// Dispatch a syscall-owned user image after the syscall tail has already
/// claimed and attested the pending reschedule request.
///
/// Unlike the software-interrupt route this callback must not publish a new
/// request merely to enter the scheduler. It also reports whether the
/// scheduler skipped architecture restore for a same-task selection, allowing
/// the assembly leaf to pair syscall entry's `swapgs` exactly once.
extern "C" fn saved_user_context_dispatch(
    context_ptr: *mut SavedContext,
) -> crate::lowlevel::interrupts::SavedUserContextDispatch {
    assert!(
        !nucleus_core::util::lockdep::preemption_disabled(),
        "saved user scheduler dispatch entered while raw spin lock held depth={} class={:?}",
        nucleus_core::util::lockdep::preemption_depth(),
        nucleus_core::util::lockdep::current_lock_class()
    );
    if !scheduler_initialized() {
        return crate::lowlevel::interrupts::SavedUserContextDispatch {
            context: context_ptr,
            swapgs_before_iret: 1,
        };
    }

    let logical_index = current_cpu_index();
    let current_rsp = context_ptr as usize;
    // SAFETY: syscall entry supplied the complete current-task user frame,
    // interrupts are disabled by the assembly transfer, and the scheduler
    // lock serializes every cross-CPU task-state and stack-owner transition.
    let (next_rsp, user_dispatch, same_task, runtime_profile) = {
        let mut entry = DispatchEntry::enter();
        entry.save_simd();
        super::scheduler::runtime_profile::record_entry(super::scheduler::SchedulerEntryCause::Software);
        let runtime_profile = super::scheduler::runtime_profile::take_owned(entry.turn(), crate::arch::rtc::ticks());
        let (dispatch, _) = entry.dispatch(current_rsp, true, None);
        let same_task = !dispatch.requires_architectural_restore();
        let user = entry.install(dispatch, false);
        (dispatch.next_rsp, user, same_task, runtime_profile)
    };

    super::scheduler::publish_scheduler_runtime_profile(runtime_profile);
    record_first_user_dispatch(logical_index, user_dispatch);
    record_atomic_activation_dispatch(logical_index);
    crate::lowlevel::interrupts::SavedUserContextDispatch {
        context: next_rsp as *mut SavedContext,
        swapgs_before_iret: u64::from(same_task),
    }
}

fn record_atomic_activation_dispatch(logical_index: usize) {
    let Some(witness) = nucleus_core::util::lockdep::scheduler_dispatch_witness(logical_index)
    else {
        return;
    };
    if !witness.atomic_activation_handoff {
        return;
    }
    // The scheduler journal records this while holding its raw owner. Emit
    // only after that guard is gone so reliable SMP diagnostics cannot block
    // a global scheduling transaction.
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        "smp-atomic-activation-dispatch",
        logical_index as u64,
        (witness.to_task << 32) | witness.to_slot as u64,
    );
}

fn record_first_user_dispatch(logical_index: usize, user_dispatch: bool) {
    if user_dispatch {
        record_first_cpu_event(
            &CPU_FIRST_USER_DISPATCH_RECORDED,
            "smp-cpu-first-user-dispatch",
            logical_index,
        );
    }
}

fn record_first_cpu_event(
    recorded: &[AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS],
    name: &'static str,
    logical_index: usize,
) {
    if recorded[logical_index]
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Sched,
        name,
        logical_index as u64,
        1,
    );
}

pub fn yield_now() {
    let preemption = nucleus_core::util::lockdep::preemption_snapshot();
    assert!(
        preemption.depth == 0,
        "task yielded while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
        preemption.logical_cpu,
        preemption.apic_id,
        preemption.depth,
        preemption.held_depth,
        preemption.pending_depth,
        preemption.top_class,
    );
    interrupts::without_interrupts(|| {
        crate::lowlevel::interrupts::trigger_software_schedule();
    });
}

/// Linearizes a successful block commit with the software schedule trap.
///
/// A separate `commit_block_current_task(); yield_now();` sequence leaves an
/// interruptible gap in which a wake can make the still-executing task
/// runnable before it nevertheless enters the voluntary schedule path. Keep
/// interrupts excluded from the scheduler state transition through the trap,
/// matching the formal `CommitCurrentBlock` transition where a blocked task
/// ceases to own the CPU at the same linearization point.
pub fn commit_block_current_task_and_yield() -> Option<bool> {
    if nucleus_core::util::lockdep::preemption_depth() != 0 {
        // The six-field snapshot is only the failure report; the gate is one load.
        let preemption = nucleus_core::util::lockdep::preemption_snapshot();
        panic!(
            "task blocked while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
            preemption.logical_cpu,
            preemption.apic_id,
            preemption.depth,
            preemption.held_depth,
            preemption.pending_depth,
            preemption.top_class,
        );
    }
    interrupts::without_interrupts(|| {
        // SAFETY: original masked CPU scope contains the backing loan; it is
        // dropped before entering the scheduling trap below.
        let committed = unsafe { super::scheduler::commit_current_wait() };
        if committed == Some(true) {
            crate::lowlevel::interrupts::trigger_software_schedule();
        }
        committed
    })
}

/// Commit the server's endpoint receive block, then settle the completed reply
/// and dispatch its exact caller in the same scheduler acquisition.
///
/// A wake that wins the endpoint waiter race leaves reply settlement to the
/// caller, which can use the ordinary non-yielding path.
pub fn commit_block_current_task_with_fast_reply_and_yield(
    reply: u64,
    completion: kernel_ipc_runtime::api::ReplyCompletion,
) -> Option<bool> {
    let custody = completion
        .scheduling_context
        .expect("fast reply block transition lost scheduling-context custody");
    assert_eq!(
        custody.caller_task_id(),
        completion.caller_task_id,
        "fast reply block transition names different caller identities"
    );
    if nucleus_core::util::lockdep::preemption_depth() != 0 {
        // The six-field snapshot is only the failure report; the gate is one load.
        let preemption = nucleus_core::util::lockdep::preemption_snapshot();
        panic!(
            "task blocked with fast reply while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
            preemption.logical_cpu,
            preemption.apic_id,
            preemption.depth,
            preemption.held_depth,
            preemption.pending_depth,
            preemption.top_class,
        );
    }
    interrupts::without_interrupts(|| {
        // SAFETY: original masked CPU scope contains the backing loan; it is
        // dropped before entering the exact reply scheduling trap below.
        let committed = unsafe { super::scheduler::commit_current_wait() };
        if committed == Some(true) {
            let identity = custody.identity();
            crate::lowlevel::interrupts::trigger_fast_ipc_reply_schedule(
                reply,
                completion.caller_task_id,
                custody.context_owner_task_id(),
                identity.slot(),
                identity.generation(),
            );
        }
        committed
    })
}

/// Fuse exact Current pager commit, fixed fault publication and worker handoff.
/// Original masked Current loan survives the worker's LAST effect, but MUST
/// drop before the trap can reschedule. Resume borrows the then-actual Current
/// MM/stack custody, not a saved-frame or a newly manufactured execution claim.
pub fn commit_pager_fault_block_and_yield(token: u64) -> Option<bool> {
    if token == 0 { return Some(false); }
    let raw = nucleus_core::util::lockdep::preemption_snapshot();
    assert_eq!(raw.depth, 0, "pager fault blocked during raw lock acquisition or ownership");
    interrupts::without_interrupts(|| {
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter();
        scope.mask_irqs();
        // SAFETY: this ORIGINAL mask and actual physical Current bank enclose
        // commit, rollback and all worker effects; neither survives the trap.
        let wait = unsafe { super::scheduler::CurrentWait::capture() }?;
        if !wait.commit() { return Some(false); }
        let published = super::pager_fault::mark_pager_fault_blocked(token).is_ok();
        if !published && !wait.restore_pager(token) {
            panic!("pager fault block rollback lost exact current wait token={token:#x}");
        }
        if published
            && let Some(worker_id) = super::pager_fault::take_pager_fault_waiter_for(token)
        {
            let _ = super::scheduler::pager_handoff::handoff(&scope, &wait, token, worker_id);
        }
        drop(wait);
        drop(scope);
        // Outer original exception IRQ mask survives; no physical loan or raw
        // leaf crosses the blocking trap. Publish a fresh continuation even
        // when fault-slot cancellation restored the exact wait intent.
        crate::lowlevel::interrupts::trigger_software_schedule();

        // Earlier dispatch/RSP already installed this actual Current. Borrow
        // its ORIGINAL allocation/context MM through CR3/TSS/FS/GS effects;
        // same-task dispatch may have left the kernel GS pair in place.
        let mut resumed = nucleus_core::util::lockdep::DispatchPreemption::enter();
        resumed.mask_irqs();
        // SAFETY: resumed owns the actual same-CPU Current through last effect.
        let current = unsafe { super::scheduler::CurrentWait::capture() }
            .expect("pager continuation lost actual Current custody");
        super::scheduler::return_architecture::CurrentArchitecture::capture(&resumed, &current)
            .expect("pager continuation lost original user MM/stack custody")
            .install();
        drop(current);
        drop(resumed);
        Some(published)
    })
}

/// Linearizes the Phase-5 same-CPU call transfer with the schedule trap.
/// A committed sender cannot execute with blocked owner state while interrupts
/// are enabled; every rejected result returns without trapping so the caller
/// can rollback the fixed frame and restart the slowpath from scratch.
pub fn commit_fast_ipc_call_handoff_and_yield(
    endpoint: u64,
    reply: u64,
    receiver_task_id: u64,
) -> super::scheduler::FastIpcCallHandoffOutcome {
    if nucleus_core::util::lockdep::preemption_depth() != 0 {
        // The six-field snapshot is only the failure report; the gate is one load.
        let preemption = nucleus_core::util::lockdep::preemption_snapshot();
        panic!(
            "fast IPC committed while raw spin lock held cpu={} apic={:#x} depth={} held_depth={} pending_depth={} class={:?}",
            preemption.logical_cpu,
            preemption.apic_id,
            preemption.depth,
            preemption.held_depth,
            preemption.pending_depth,
            preemption.top_class,
        );
    }
    interrupts::without_interrupts(|| {
        let code = crate::lowlevel::interrupts::trigger_fast_ipc_call_schedule(
            endpoint,
            reply,
            receiver_task_id,
        );
        match code {
            0 => super::scheduler::FastIpcCallHandoffOutcome::CommittedSameCpu,
            1 => super::scheduler::FastIpcCallHandoffOutcome::CommittedCrossCpu,
            2 => super::scheduler::FastIpcCallHandoffOutcome::SenderMismatch,
            3 => super::scheduler::FastIpcCallHandoffOutcome::ReceiverMismatch,
            4 => super::scheduler::FastIpcCallHandoffOutcome::DonationUnavailable,
            5 => super::scheduler::FastIpcCallHandoffOutcome::EligibilityUnavailable,
            6 => super::scheduler::FastIpcCallHandoffOutcome::DirectCustodyUnavailable,
            7 => super::scheduler::FastIpcCallHandoffOutcome::OrderingUnavailable,
            other => panic!("fast IPC software trap returned invalid outcome {other}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::{
        RescheduleIpiGate, consume_syscall_tail_reschedule,
        periodic_idle_continuation_is_local, periodic_user_continuation_is_local,
        remote_notification_required, reschedule_ipi_gate,
    };

    #[test]
    fn periodic_tick_stays_local_only_without_any_dispatch_authority() {
        assert!(periodic_user_continuation_is_local(
            true, false, false, false
        ));
        assert!(!periodic_user_continuation_is_local(
            false, false, false, false
        ));
        assert!(!periodic_user_continuation_is_local(
            true, true, false, false
        ));
        assert!(!periodic_user_continuation_is_local(
            true, false, true, false
        ));
        assert!(!periodic_user_continuation_is_local(
            true, false, false, true
        ));
    }

    #[test]
    fn periodic_idle_tick_stays_local_only_without_any_queue_or_request() {
        assert!(periodic_idle_continuation_is_local(
            true, true, false, false, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            true, true, true, false, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            true, true, false, true, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            true, false, false, false, false
        ));
        assert!(!periodic_idle_continuation_is_local(
            false, true, false, false, false
        ));
    }

    #[test]
    fn exact_target_notification_rejects_self_ipi() {
        assert!(!remote_notification_required(2, 2));
        assert!(remote_notification_required(1, 2));
        assert!(remote_notification_required(3, 2));
    }

    #[test]
    fn syscall_tail_consumes_every_deferred_or_handoff_request_exactly_once() {
        let handoff = AtomicU64::new(0);
        assert!(!consume_syscall_tail_reschedule(false, &handoff));

        assert!(consume_syscall_tail_reschedule(true, &handoff));
        assert!(!consume_syscall_tail_reschedule(false, &handoff));

        handoff.store(1, Ordering::Release);
        assert!(consume_syscall_tail_reschedule(true, &handoff));
        assert_eq!(handoff.load(Ordering::Acquire), 0);
    }

    #[test]
    fn reschedule_ipi_gate_retains_locked_work_and_dispatches_only_at_safe_point() {
        assert_eq!(
            reschedule_ipi_gate(false, false, true),
            RescheduleIpiGate::RetainRequest
        );
        assert_eq!(
            reschedule_ipi_gate(true, true, true),
            RescheduleIpiGate::RetainRequest
        );
        assert_eq!(
            reschedule_ipi_gate(true, false, false),
            RescheduleIpiGate::AcknowledgeOnly
        );
        assert_eq!(
            reschedule_ipi_gate(true, false, true),
            RescheduleIpiGate::Dispatch
        );
    }
}
