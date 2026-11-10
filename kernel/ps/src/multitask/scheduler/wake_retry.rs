//! Durable retry of refused wakes for the exact wait they were meant for.
//!
//! - **Owner:** each CPU owns the tokens its refused wake producers recorded;
//!   only that CPU, with local interrupts masked, mutates its queue.
//! - **Boundary:** a token is an observation (task, slot, wait epoch, reason),
//!   never wake, frame, funding or queue custody. Each retry re-captures the
//!   original binding and runs the ordinary typed wake for that exact reason.
//! - **Lifecycle:** refused wake -> record while that wait instance is
//!   published -> retried at every IRQ/scheduler entry of the recording CPU ->
//!   dropped once woken, once the wait instance completed or changed, once the
//!   slot names another task. Once a bounded diagnostic window expires, a
//!   still-live exact wait retains its retry rather than losing wake custody.
//! - **Failure:** a refusal is usually a transient window (the target between
//!   Current and a published frame, a busy frame reader, a racing typed wake).
//!   Dropping it left the waiter parked until an unrelated event or its own
//!   deadline; at eight vCPUs that stranded service worker pools and reply
//!   waits. The wait epoch advances on every arm, cancel and wake, so a retry
//!   can never wake a later, unrelated wait of the same task.
//! - **Evidence:** `wake_retry` native tests and `kernel-scheduler-wake-retry-*`
//!   milestones.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use nucleus_core::util::lockdep::{DispatchPreemption, MAX_TRACKED_CPUS};

use super::runqueue::wait::{WaitInstance, wait_instance};
use super::{MAX_TASK, runqueue_policy, stack_resources, task_directory, wake_publication};

/// Age at which a refused exact wake is reported. The deadline is for
/// diagnostics, not permission to discard a still-live waiter.
const RETRY_WINDOW_TICKS: u64 = 4 * crate::arch::rtc::ticks_per_second();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Token {
    task_id: u64,
    slot: usize,
    serial: u64,
    kind: u8,
    id: u64,
    expires_tick: u64,
}

#[derive(Clone, Copy)]
struct Queue {
    tokens: [Option<Token>; MAX_TASK],
    len: usize,
}

impl Queue {
    const fn new() -> Self {
        Self { tokens: [None; MAX_TASK], len: 0 }
    }

    /// One token per task, but ordering is by the *wait epoch*, not by the
    /// time a producer finally acquired this CPU's retry queue. A producer
    /// can capture an old epoch, lose a race to a new refused wake, and only
    /// then enqueue its stale observation. Never replace the newer token.
    ///
    /// Repeated refusals of the same epoch also retain the original expiry
    /// (including the u64::MAX "already reported" sentinel), so an active
    /// producer cannot indefinitely postpone or repeat the prolonged-wait
    /// diagnostic by refreshing the timer on every retry.
    fn insert(&mut self, mut token: Token) -> bool {
        if let Some(existing) = self.tokens[..self.len]
            .iter_mut()
            .flatten()
            .find(|existing| existing.task_id == token.task_id)
        {
            if existing.slot == token.slot {
                if existing.serial > token.serial {
                    return true;
                }
                if existing.serial == token.serial {
                    if (existing.kind, existing.id) != (token.kind, token.id) {
                        // One wait serial has one exact typed identity. A
                        // stale or torn observation must not replace it.
                        return true;
                    }
                    token.expires_tick = existing.expires_tick;
                }
            }
            *existing = token;
            return true;
        }
        if self.len == MAX_TASK {
            return false;
        }
        self.tokens[self.len] = Some(token);
        self.len += 1;
        true
    }

    fn take(&mut self) -> Self {
        core::mem::replace(self, Self::new())
    }
}

struct CpuQueue {
    queue: UnsafeCell<Queue>,
    // CPU-private queue contents; the pending observation may be read remotely
    // to prevent a periodic IRQ from bypassing durable retry work.
    pending: AtomicBool,
}

struct Queues([CpuQueue; MAX_TRACKED_CPUS]);

// SAFETY: only the indexed CPU accesses its queue, always with local interrupts
// masked by an original `DispatchPreemption` scope, and a masked kernel frame
// never migrates, so no two contexts alias one cell.
unsafe impl Sync for Queues {}

static QUEUES: Queues = Queues([const {
    CpuQueue { queue: UnsafeCell::new(Queue::new()), pending: AtomicBool::new(false) }
}; MAX_TRACKED_CPUS]);

/// Retry observations are local scheduler work even with empty fair/FIFO queues.
/// A single refused service attempt must not let subsequent idle ticks bypass it.
pub(in crate::multitask) fn pending(cpu: usize) -> bool {
    QUEUES.0.get(cpu).is_some_and(|cell| cell.pending.load(Ordering::Acquire))
}

fn scoped(scope: &DispatchPreemption<'_>) -> bool {
    scope.is_masked()
        && scope.cpu() == nucleus_core::util::lockdep::current_cpu_index()
        && scope.cpu() < MAX_TRACKED_CPUS
}

fn with_queue<R>(scope: &DispatchPreemption<'_>, f: impl FnOnce(&mut Queue) -> R) -> R {
    // SAFETY: `scoped` proved this CPU owns the cell with interrupts masked.
    unsafe { with_cpu_queue(scope.cpu(), f) }
}

/// # Safety
/// `cpu` is the current CPU and the caller holds its masked dispatch scope.
unsafe fn with_cpu_queue<R>(cpu: usize, f: impl FnOnce(&mut Queue) -> R) -> R {
    let cell = &QUEUES.0[cpu];
    // SAFETY: forwarded from the caller's exclusive masked custody of `cpu`.
    let queue = unsafe { &mut *cell.queue.get() };
    let result = f(queue);
    // ORDERING: publish the resulting durable-work observation after queue
    // mutation. This CPU cannot take a periodic IRQ until the mask is released.
    cell.pending.store(queue.len != 0, Ordering::Release);
    result
}

fn live_slot(task_id: u64) -> Option<usize> {
    task_directory::candidates(task_id)
        .find(|&slot| stack_resources::task_start::read(slot).map(|start| start.id) == Some(task_id))
}

/// Records a refused wake of `task_id`. `expected` restricts the record to a
/// typed wait (a refused reply wake must not be replayed against another wait);
/// `None` records whatever wait is published, exactly what the refused generic
/// wake would have woken. Nothing is recorded when the task is not waiting.
pub(in crate::multitask) fn record_in(
    scope: &DispatchPreemption<'_>,
    task_id: u64,
    expected: Option<(u8, u64)>,
) -> bool {
    if task_id == 0 || !scoped(scope) {
        return false;
    }
    // SAFETY: `scoped` proved masked custody of this CPU.
    unsafe { record_on(scope.cpu(), task_id, expected) }
}

/// `record_in` for a producer whose caller retains the masked scope but does
/// not pass it down (terminal SIGCHLD transfer).
///
/// # Safety
/// The caller holds this CPU's original masked `DispatchPreemption` scope.
pub(super) unsafe fn record_under_caller_scope(task_id: u64, expected: Option<(u8, u64)>) -> bool {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    if task_id == 0 || cpu >= MAX_TRACKED_CPUS {
        return false;
    }
    // SAFETY: forwarded from the caller's masked custody of this CPU.
    unsafe { record_on(cpu, task_id, expected) }
}

/// # Safety
/// Same as `with_cpu_queue`.
unsafe fn record_on(cpu: usize, task_id: u64, expected: Option<(u8, u64)>) -> bool {
    let Some(slot) = live_slot(task_id) else {
        return false;
    };
    let WaitInstance::Waiting { serial, kind, id } = wait_instance(slot) else {
        return false;
    };
    if expected.is_some_and(|expected| expected != (kind, id)) {
        return false;
    }
    let token = Token {
        task_id,
        slot,
        serial,
        kind,
        id,
        expires_tick: crate::arch::rtc::ticks().saturating_add(RETRY_WINDOW_TICKS),
    };
    // SAFETY: forwarded from the caller's masked custody of `cpu`.
    let recorded = unsafe { with_cpu_queue(cpu, |queue| queue.insert(token)) };
    if !recorded {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Sched,
            "kernel-scheduler-wake-retry-full",
            task_id,
            (u64::from(kind) << 56) | (id & 0x00ff_ffff_ffff_ffff),
        );
    }
    recorded
}

/// `record_in` for a producer that no longer holds its masked scope.
pub(in crate::multitask) fn record_refused(task_id: u64, expected: Option<(u8, u64)>) -> bool {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    record_in(&scope, task_id, expected)
}

/// A refused exact reply wake for `task_id`'s wait on `reply`.
pub(in crate::multitask) fn record_refused_reply(task_id: u64, reply: u64) -> bool {
    record_refused(task_id, Some((super::runqueue::wait::REASON_ENDPOINT_REPLY, reply)))
}

/// A refused exact pager-fault completion for `task_id`'s wait on `token`.
pub(in crate::multitask) fn record_refused_pager_fault(task_id: u64, token: u64) -> bool {
    record_refused(task_id, Some((super::runqueue::wait::REASON_PAGER_FAULT, token)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Retry {
    /// Woken, or nothing left to wake: drop the token.
    Done,
    /// Still the same uncompleted wait and still refused: keep it.
    Keep,
}

fn retry(
    scope: &DispatchPreemption<'_>,
    token: Token,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> Retry {
    retry_after_observing(scope, token, eligible_cpus, || {})
}

fn retry_after_observing(
    scope: &DispatchPreemption<'_>,
    token: Token,
    eligible_cpus: &dyn Fn(usize) -> u64,
    before_capture: impl FnOnce(),
) -> Retry {
    if stack_resources::task_start::read(token.slot).map(|start| start.id) != Some(token.task_id) {
        return Retry::Done;
    }
    match wait_instance(token.slot) {
        WaitInstance::Idle => return Retry::Done,
        WaitInstance::WakePending => return Retry::Keep,
        WaitInstance::Waiting { serial, kind, id } => {
            if (serial, kind, id) != (token.serial, token.kind, token.id) {
                return Retry::Done;
            }
        }
    }
    before_capture();
    let Some(binding) = wake_publication::Binding::capture(token.slot, token.task_id) else {
        return Retry::Keep;
    };
    let _ = scope;
    // SAFETY: `scoped` proved the original masked CPU custody that the typed
    // wake engine requires; the fresh binding is consumed by this one wake.
    match unsafe {
        wake_publication::retained_wake_matching_instance(
            binding, (token.serial, token.kind, token.id), eligible_cpus,
        )
    } {
        Ok(_) | Err(true) => Retry::Done,
        Err(false) => Retry::Keep,
    }
}

/// A prolonged refusal is still a live exact wait. Warn once and retain
/// exactly the same wait identity/serial until an actual terminal transition.
fn retain_prolonged_retry(mut token: Token, now: u64) -> (Token, bool) {
    let prolonged = now >= token.expires_tick && token.expires_tick != u64::MAX;
    if prolonged {
        token.expires_tick = u64::MAX;
    }
    (token, prolonged)
}

/// Retries every token this CPU recorded. Called at each IRQ/scheduler entry
/// after the deferred wake drain; returns how many tokens were resolved.
pub(in crate::multitask) fn service(scope: &DispatchPreemption<'_>) -> usize {
    service_with(scope, &runqueue_policy::online_affinity_mask)
}

pub(super) fn service_with(
    scope: &DispatchPreemption<'_>,
    eligible_cpus: &dyn Fn(usize) -> u64,
) -> usize {
    if !scoped(scope) || with_queue(scope, |queue| queue.len == 0) {
        return 0;
    }
    let batch = with_queue(scope, Queue::take);
    let now = crate::arch::rtc::ticks();
    let mut resolved = 0;
    for token in batch.tokens[..batch.len].iter().flatten().copied() {
        match retry(scope, token, eligible_cpus) {
            Retry::Done => resolved += 1,
            Retry::Keep => {
                let (token, prolonged) = retain_prolonged_retry(token, now);
                if prolonged {
                    crate::debug::record_milestone(
                        crate::debug::LogCategory::Sched,
                        "kernel-scheduler-wake-retry-prolonged",
                        token.task_id,
                        (u64::from(token.kind) << 56) | (token.id & 0x00ff_ffff_ffff_ffff),
                    );
                }
                // A wake refused inside this pass may already have recorded a
                // newer observation of the same task; keep the newer one.
                // Time alone never withdraws one-shot reply/receive custody.
                with_queue(scope, |queue| {
                    if !queue.tokens[..queue.len]
                        .iter()
                        .flatten()
                        .any(|newer| newer.task_id == token.task_id)
                    {
                        assert!(
                            queue.insert(token),
                            "scheduler exact wake retry silently lost token task={} slot={} epoch={}",
                            token.task_id,
                            token.slot,
                            token.serial,
                        );
                    }
                });
            }
        }
    }
    resolved
}

#[cfg(test)]
pub(super) fn pending_on(scope: &DispatchPreemption<'_>) -> usize {
    with_queue(scope, |queue| queue.len)
}

#[cfg(test)]
mod tests;
