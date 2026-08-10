//! Global monotonic deadline registry and single-owner wake delivery.
//!
//! - **Owner:** `kernel-hal` owns deadline records; the scheduler owns task
//!   block epochs and callers own condition rechecks.
//! - **Boundary:** Task IDs and deadlines are admitted only from exact
//!   scheduler context.
//! - **Lifecycle:** Register, recheck, expire/notify, resume, and disarm retain
//!   one exact waiter until its owner acknowledges cleanup.
//! - **Concurrency:** The tracked deadline lock is bounded and allocation-free;
//!   CPU0 owns expiry delivery while AP clockevents remain scheduler-local.
//!   IRQ work records wakeups and leaves policy to schedulable context.
//! - **Failure:** Cancel, nondeadline wake, timeout, and retirement are
//!   idempotent and reject stale task identities.
//! - **Forbidden:** No destructive “notify means remove,” calendar deadlines,
//!   polling fallback, or callback under the registry lock.
//! - **Evidence:** `monotonic-deadline-lifecycle`, `scheduler-lifecycle`, and
//!   `ui-main-loop-wakeup`.
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use x86_64::instructions::{hlt, interrupts, port::Port};

const CMOS_INDEX_PORT: u16 = 0x70;
const CMOS_DATA_PORT: u16 = 0x71;
const NMI_DISABLE: u8 = 0x80;
const CMOS_REGISTER_MASK: u8 = 0x7f;

const RTC_REG_A: u8 = 0x0A;
const RTC_REG_B: u8 = 0x0B;
const RTC_REG_C: u8 = 0x0C;
const RTC_REG_SECONDS: u8 = 0x00;
const RTC_REG_MINUTES: u8 = 0x02;
const RTC_REG_HOURS: u8 = 0x04;
const RTC_REG_WEEKDAY: u8 = 0x06;
const RTC_REG_DAY: u8 = 0x07;
const RTC_REG_MONTH: u8 = 0x08;
const RTC_REG_YEAR: u8 = 0x09;
const RTC_UPDATE_IN_PROGRESS: u8 = 1 << 7;
const RTC_PERIODIC_INTERRUPT_ENABLE: u8 = 1 << 6;
const RTC_TICKS_PER_SEC: u64 = 1024;
const RTC_SLEEP_WAITER_CAPACITY: usize = 256;
const RTC_SLEEP_RENOTIFY_TICKS: u64 = 8;
const RTC_SLEEP_UNNOTIFIED_TICK: u64 = u64::MAX;

static RTC_TICKS: AtomicU64 = AtomicU64::new(0);
static RTC_TICKS_COMPLETED: AtomicU64 = AtomicU64::new(0);
static RTC_INITIALIZED: AtomicBool = AtomicBool::new(false);
static RTC_LAST_ALIVE_SECOND: AtomicU64 = AtomicU64::new(u64::MAX);
static RTC_LAST_DIAG_PRINT_TICK: AtomicU64 = AtomicU64::new(0);
// Marked by RTC IRQ each time the integer-second wall clock advances; drained
// outside IRQ context by `drain_pending_heartbeat`. `u64::MAX` is the sentinel
// for "no second has ever been observed" (matches RTC_LAST_ALIVE_SECOND).
static HEARTBEAT_PENDING_SECOND: AtomicU64 = AtomicU64::new(u64::MAX);
static RTC_LAST_INPUT_PACKET_SUBMIT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_INPUT_READ_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_INPUT_READ_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_LINUX_IRQ_LOCK_DEPTH: AtomicU64 = AtomicU64::new(0);
static RTC_SLEEP_LOCK_MISSES: AtomicU64 = AtomicU64::new(0);
type RtcSleepWaiterLock = TrackedSpinLock<SleepWaiterTable, { LockClass::RtcSleepWaiter as u8 }>;
static RTC_SLEEP_WAITERS: RtcSleepWaiterLock = TrackedSpinLock::new(SleepWaiterTable::new());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SleepWaiter {
    task_id: u64,
    wake_tick: u64,
    last_notify_tick: u64,
    notification_count: u32,
}

#[derive(Clone, Copy)]
struct SleepWakeNotification {
    task_id: u64,
    notification_count: u32,
}

struct SleepWaiterTable {
    slots: [Option<SleepWaiter>; RTC_SLEEP_WAITER_CAPACITY],
}

impl SleepWaiterTable {
    const fn new() -> Self {
        Self {
            slots: [None; RTC_SLEEP_WAITER_CAPACITY],
        }
    }

    fn insert_or_update(&mut self, waiter: SleepWaiter) -> bool {
        let mut free_index = None;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            match slot {
                Some(existing) if existing.task_id == waiter.task_id => {
                    existing.wake_tick = waiter.wake_tick;
                    existing.last_notify_tick = RTC_SLEEP_UNNOTIFIED_TICK;
                    existing.notification_count = 0;
                    return true;
                }
                None if free_index.is_none() => free_index = Some(index),
                _ => {}
            }
        }

        let Some(index) = free_index else {
            return false;
        };
        self.slots[index] = Some(waiter);
        true
    }

    fn snapshot_ready(
        &mut self,
        now: u64,
        ready: &mut [SleepWakeNotification; RTC_SLEEP_WAITER_CAPACITY],
    ) -> usize {
        let mut ready_len = 0;
        for slot in self.slots.iter_mut() {
            let Some(waiter) = slot.as_mut() else {
                continue;
            };
            let first_notification = waiter.last_notify_tick == RTC_SLEEP_UNNOTIFIED_TICK;
            let retry_due = now.saturating_sub(waiter.last_notify_tick) >= RTC_SLEEP_RENOTIFY_TICKS;
            if waiter.wake_tick <= now && (first_notification || retry_due) {
                waiter.notification_count = waiter.notification_count.saturating_add(1);
                ready[ready_len] = SleepWakeNotification {
                    task_id: waiter.task_id,
                    notification_count: waiter.notification_count,
                };
                ready_len += 1;
                waiter.last_notify_tick = now;
            }
        }
        ready_len
    }

    fn remove_task(&mut self, task_id: u64) -> bool {
        let mut removed = false;
        for slot in self.slots.iter_mut() {
            if slot
                .map(|waiter| waiter.task_id == task_id)
                .unwrap_or(false)
            {
                *slot = None;
                removed = true;
            }
        }
        removed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawRtcDateTime {
    second: u8,
    minute: u8,
    hour: u8,
    weekday: u8,
    day: u8,
    month: u8,
    year: u8,
}

fn cmos_read(reg: u8) -> u8 {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        select_cmos_register(&mut index_port, reg, true);
        let value = data_port.read();
        select_cmos_register(&mut index_port, reg, false);
        value
    }
}

fn cmos_write(reg: u8, value: u8) {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        select_cmos_register(&mut index_port, reg, true);
        data_port.write(value);
        select_cmos_register(&mut index_port, reg, false);
    }
}

fn enable_nmi() {
    let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
    select_cmos_register(&mut index_port, 0, false);
}

fn select_cmos_register(index_port: &mut Port<u8>, reg: u8, disable_nmi: bool) {
    let reg = reg & CMOS_REGISTER_MASK;
    let value = if disable_nmi { NMI_DISABLE | reg } else { reg };
    unsafe {
        index_port.write(value);
    }
}

fn update_in_progress() -> bool {
    (cmos_read(RTC_REG_A) & RTC_UPDATE_IN_PROGRESS) != 0
}

fn read_raw_datetime() -> RawRtcDateTime {
    RawRtcDateTime {
        second: cmos_read(RTC_REG_SECONDS),
        minute: cmos_read(RTC_REG_MINUTES),
        hour: cmos_read(RTC_REG_HOURS),
        weekday: cmos_read(RTC_REG_WEEKDAY),
        day: cmos_read(RTC_REG_DAY),
        month: cmos_read(RTC_REG_MONTH),
        year: cmos_read(RTC_REG_YEAR),
    }
}

fn bcd_to_binary(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

fn expand_year(year: u8) -> u16 {
    if year >= 70 {
        1900 + year as u16
    } else {
        2000 + year as u16
    }
}

fn decode_datetime(raw: RawRtcDateTime, reg_b: u8) -> RtcDateTime {
    let is_binary = (reg_b & (1 << 2)) != 0;
    let is_24_hour = (reg_b & (1 << 1)) != 0;
    let is_pm = (raw.hour & 0x80) != 0;

    let mut second = raw.second;
    let mut minute = raw.minute;
    let mut hour = raw.hour & 0x7F;
    let mut weekday = raw.weekday;
    let mut day = raw.day;
    let mut month = raw.month;
    let mut year = raw.year;

    if !is_binary {
        second = bcd_to_binary(second);
        minute = bcd_to_binary(minute);
        hour = bcd_to_binary(hour);
        weekday = bcd_to_binary(weekday);
        day = bcd_to_binary(day);
        month = bcd_to_binary(month);
        year = bcd_to_binary(year);
    }

    if !is_24_hour {
        hour %= 12;
        if is_pm {
            hour = hour.saturating_add(12);
        }
    }

    RtcDateTime {
        year: expand_year(year),
        month,
        day,
        weekday,
        hour,
        minute,
        second,
    }
}

fn read_stable_datetime() -> RtcDateTime {
    loop {
        while update_in_progress() {
            spin_loop();
        }
        let first = read_raw_datetime();

        while update_in_progress() {
            spin_loop();
        }
        let second = read_raw_datetime();

        if first == second {
            let reg_b = cmos_read(RTC_REG_B);
            return decode_datetime(second, reg_b);
        }

        spin_loop();
    }
}

pub fn now() -> RtcDateTime {
    interrupts::without_interrupts(read_stable_datetime)
}

/// Elapsed time on the validated clocksource, in nanoseconds.
///
/// This is the value `ticks()` is derived from, and the one a caller wants
/// whenever it is reporting or comparing an instant rather than indexing the
/// tick-numbered deadline wheel. Rounding to `RTC_TICKS_PER_SEC` throws away
/// just under a millisecond, which is larger than most of the intervals the
/// system measures.
///
/// Before the clocksource is admitted there is no counter to read, so the
/// periodic interrupt count is the only elapsed-time evidence that exists;
/// scaling it keeps this function's domain continuous across that handover
/// rather than reporting zero for the whole of early boot. `ticks()` keeps its
/// own copy of that fallback because one tick is 976562.5 nanoseconds, so a
/// tick count routed through integer nanoseconds would not come back whole.
pub fn monotonic_nanos() -> u64 {
    let nanos = crate::arch::clock::monotonic_nanos();
    if nanos == 0 && crate::arch::clock::current_source().is_none() {
        // ORDERING: acquire matches the interrupt handler's release increment,
        // so a reader that sees the count also sees the state that edge left.
        return u64::try_from(
            u128::from(RTC_TICKS.load(Ordering::Acquire)).saturating_mul(1_000_000_000_u128)
                / u128::from(RTC_TICKS_PER_SEC),
        )
        .unwrap_or(u64::MAX);
    }
    nanos
}

pub fn ticks() -> u64 {
    let nanos = crate::arch::clock::monotonic_nanos();
    if nanos == 0 && crate::arch::clock::current_source().is_none() {
        return RTC_TICKS.load(Ordering::Acquire);
    }
    u64::try_from(
        u128::from(nanos).saturating_mul(u128::from(RTC_TICKS_PER_SEC)) / 1_000_000_000_u128,
    )
    .unwrap_or(u64::MAX)
}

pub const fn ticks_per_second() -> u64 {
    RTC_TICKS_PER_SEC
}

pub fn is_initialized() -> bool {
    RTC_INITIALIZED.load(Ordering::Acquire)
}

pub fn init() {
    interrupts::without_interrupts(|| {
        enable_nmi();
        // The RTC remains the battery-backed calendar source, but its periodic
        // interrupt is not a monotonic clockevent. Hypervisors are permitted to
        // coalesce those edges while a vCPU is descheduled; counting them made
        // RustOS time run at less than half wall rate under UI load. PIT drives
        // scheduling/deadline service and the invariant-TSC/HPET clocksource
        // supplies elapsed time.
        let prev_b = cmos_read(RTC_REG_B);
        cmos_write(RTC_REG_B, prev_b & !RTC_PERIODIC_INTERRUPT_ENABLE);

        // Read C once to clear any pending interrupt latch.
        let _ = cmos_read(RTC_REG_C);

        enable_nmi();
    });

    RTC_INITIALIZED.store(true, Ordering::Release);
}

pub fn on_interrupt() {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    let interrupt_count = RTC_TICKS.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    // A timer may interrupt the narrow acquisition/release window of any raw
    // lock. Preserve hardware accounting and acknowledgement, but defer
    // scheduler-facing sleeper callbacks until preemption is permitted again.
    // Absolute deadlines make the next clockevent an exact catch-up, not a
    // relative-time extension.
    if !nucleus_core::util::lockdep::preemption_disabled() {
        service_clock_event();
    }
    // Only emit the diagnostic when we observe more than one in-flight tick
    // (re-entrance or a missed completion). In the steady state delta is
    // always 1 — logging that every few ticks dominates the debugcon stream
    // (≈90% of boot output) without adding signal.
    let completed = RTC_TICKS_COMPLETED.load(Ordering::Acquire);
    let inflight = interrupt_count.saturating_sub(completed);
    if inflight > 1 {
        let last_diag_tick = RTC_LAST_DIAG_PRINT_TICK.load(Ordering::Acquire);
        if interrupt_count.saturating_sub(last_diag_tick) >= 4
            && RTC_LAST_DIAG_PRINT_TICK
                .compare_exchange(
                    last_diag_tick,
                    interrupt_count,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            let task = crate::hooks::current_task_id().unwrap_or(u64::MAX);
            crate::debug::println!(
                "rtc diag: tick={} completed={} delta={} task={}",
                interrupt_count,
                completed,
                inflight,
                task,
            );
        }
    }
    // Heartbeat snapshot + format + log used to run here. With KVM each outb to
    // debugcon is a VMExit, so emitting ~700 bytes from IRQ context drops the
    // current second's frame — visible as a periodic stutter. Just mark the
    // second as pending; housekeeping picks it up outside IRQ context.
    // Must read register C to acknowledge and re-arm RTC interrupts.
    let _ = cmos_read(RTC_REG_C);
    RTC_TICKS_COMPLETED.fetch_add(1, Ordering::AcqRel);
}

/// Services the single global absolute-deadline base from its CPU0 owner. PIT
/// calls this before each BSP scheduler pick; therefore a delayed or coalesced
/// owner interrupt catches up all expired waiters from the clocksource rather
/// than extending every timeout by the number of lost edges. AP LAPIC timers
/// must not also expire this base: an expired waiter deliberately remains
/// registered until its task acknowledges wakeup, so multi-CPU delivery would
/// multiply the same notification and scheduler-lock acquisition by CPU count.
pub fn service_clock_event() {
    // Both callers are CPU0 hardware clockevent leaves (PIT scheduling and
    // legacy RTC). Enter here rather than relying on one IDT wrapper so the
    // shared absolute deadline table is always treated as IRQ-owned.
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    let now_ticks = ticks();
    wake_ready_sleepers(now_ticks);
    let current_second = crate::arch::clock::monotonic_nanos() / 1_000_000_000;
    let last_reported_second = RTC_LAST_ALIVE_SECOND.load(Ordering::Acquire);
    if current_second != last_reported_second {
        HEARTBEAT_PENDING_SECOND.store(current_second, Ordering::Release);
    }
}

/// Drain a pending heartbeat second (if any) and emit its diagnostic log. Must
/// be called outside IRQ context — the housekeeping task is the intended
/// caller. Returns the number of seconds drained.
pub fn drain_pending_heartbeat() -> usize {
    let pending = HEARTBEAT_PENDING_SECOND.load(Ordering::Acquire);
    if pending == u64::MAX {
        return 0;
    }
    let last_reported = RTC_LAST_ALIVE_SECOND.load(Ordering::Acquire);
    if pending == last_reported {
        return 0;
    }
    if RTC_LAST_ALIVE_SECOND
        .compare_exchange(last_reported, pending, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return 0;
    }
    if !crate::debug::enabled!(heartbeat, info) {
        return 1;
    }
    emit_heartbeat_log(pending);
    1
}

fn emit_heartbeat_log(current_second: u64) {
    let snapshot = crate::hooks::heartbeat_snapshot();
    let input_snapshot = snapshot.input;
    let linux_irq_owner_count = snapshot.linux_irq_owner_count;
    let linux_irq_total_depth = snapshot.linux_irq_total_depth as usize;
    let linux_input_lock_active = snapshot.linux_input_lock_active;
    let linux_input_lock_last_seq = snapshot.linux_input_lock_last_seq;
    let input_packet_delta = input_snapshot.pointer_packet_submits.saturating_sub(
        RTC_LAST_INPUT_PACKET_SUBMIT_COUNT
            .swap(input_snapshot.pointer_packet_submits, Ordering::AcqRel),
    );
    let input_read_call_delta = input_snapshot.read_calls.saturating_sub(
        RTC_LAST_INPUT_READ_CALL_COUNT.swap(input_snapshot.read_calls, Ordering::AcqRel),
    );
    let input_read_event_delta = input_snapshot.read_events.saturating_sub(
        RTC_LAST_INPUT_READ_EVENT_COUNT.swap(input_snapshot.read_events, Ordering::AcqRel),
    );
    let linux_irq_depth_delta = (linux_irq_total_depth as u64).saturating_sub(
        RTC_LAST_LINUX_IRQ_LOCK_DEPTH.swap(linux_irq_total_depth as u64, Ordering::AcqRel),
    );
    crate::debug::info!(
        heartbeat,
        alloc::format!(
            "second={} userspace_display={} input_packet_delta={} input_read_calls_delta={} input_read_events_delta={} linux_irq_owners={} linux_irq_depth={} linux_irq_depth_delta={} input_lock_active={} input_lock_last_seq={} eventq_lock_active={} eventq_lock_last_seq={} queued={} pending_coalesced={} pending_pointer_position={} dropped_discrete={} dropped_lossy={}",
            current_second,
            snapshot.userspace_display_active,
            input_packet_delta,
            input_read_call_delta,
            input_read_event_delta,
            linux_irq_owner_count,
            linux_irq_total_depth,
            linux_irq_depth_delta,
            linux_input_lock_active,
            linux_input_lock_last_seq,
            input_snapshot.lock_active,
            input_snapshot.lock_last_seq,
            input_snapshot.queued,
            input_snapshot.pending_coalesced,
            input_snapshot.pending_pointer_position,
            input_snapshot.dropped_discrete,
            input_snapshot.dropped_lossy
        )
        .as_str()
    );
}

pub fn sleep(milliseconds: u64) {
    if milliseconds == 0 {
        return;
    }

    let target = sleep_deadline_from_ticks(ticks(), milliseconds);

    let restore_disabled = !interrupts::are_enabled();
    while ticks() < target {
        if !restore_disabled && block_current_user_until(target) {
            continue;
        }
        if restore_disabled {
            interrupts::enable();
            hlt();
            interrupts::disable();
        } else if crate::hooks::is_scheduler_initialized() {
            crate::hooks::yield_now();
        } else {
            hlt();
        }
        spin_loop();
    }
}

fn sleep_deadline_from_ticks(now: u64, milliseconds: u64) -> u64 {
    let ticks_needed = milliseconds
        .saturating_mul(RTC_TICKS_PER_SEC)
        .div_ceil(1000);
    let ticks_needed = core::cmp::max(1, ticks_needed);
    // Sleepers and the PIT clockevent service must share one time domain.
    // RTC periodic interrupts are deliberately disabled after clock-source
    // initialization, so RTC_TICKS stops advancing there. Mixing that legacy
    // counter with monotonic `ticks()` in block_current_user_until made every
    // post-init sleep loop forever: the waiter was immediately past its
    // monotonic deadline while the outer RTC_TICKS condition never changed.
    now.saturating_add(ticks_needed)
}

fn block_current_user_until(target: u64) -> bool {
    if !crate::hooks::is_scheduler_initialized() {
        return false;
    }

    // This executes inside syscall substrate. Resolving a "user snapshot"
    // would re-enter the process-table lock that the syscall may already
    // retain and self-deadlock before the sleep waiter is even registered.
    // The scheduler task id is the exact wake authority and is lockless under
    // the scheduler's interrupt exclusion.
    let Some(task_id) = crate::hooks::current_task_id() else {
        return false;
    };

    if ticks() >= target {
        return true;
    }
    if !crate::hooks::arm_block_current_task() {
        return false;
    }
    if !register_sleep_waiter(task_id, target) {
        let _ = crate::hooks::cancel_block_current_task();
        return false;
    }
    // Recheck after both the scheduler arm and deadline registration. A PIT
    // edge or unrelated wake in either gap clears the arm; the commit then
    // refuses to sleep instead of consuming the only wakeup.
    if ticks() >= target {
        disarm_sleep_waiter(task_id);
        let _ = crate::hooks::cancel_block_current_task();
        return true;
    }
    match crate::hooks::commit_block_current_task_and_yield() {
        Some(true) => {
            // A non-deadline wake may have resumed the task early. Remove its
            // stale timer record before the outer loop computes a new target.
            disarm_sleep_waiter(task_id);
            true
        }
        Some(false) => {
            disarm_sleep_waiter(task_id);
            true
        }
        None => {
            disarm_sleep_waiter(task_id);
            let _ = crate::hooks::cancel_block_current_task();
            false
        }
    }
}

fn register_sleep_waiter(task_id: u64, wake_tick: u64) -> bool {
    // RTC_SLEEP_WAITERS is consumed directly by the RTC interrupt handler.
    // Process context must therefore exclude that interrupt while holding the
    // spin lock; otherwise the IRQ can preempt this CPU and deadlock trying to
    // acquire the same non-reentrant lock.
    let waiter = SleepWaiter {
        task_id,
        wake_tick,
        last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
        notification_count: 0,
    };
    #[cfg(rustos_boot_image)]
    let inserted =
        interrupts::without_interrupts(|| RTC_SLEEP_WAITERS.lock().insert_or_update(waiter));
    // Host tests have no RTC interrupt consumer, and executing CLI/STI outside
    // ring0 is invalid. Keep the same single-lock transaction without issuing
    // privileged instructions in a non-boot build.
    #[cfg(not(rustos_boot_image))]
    let inserted = RTC_SLEEP_WAITERS.lock().insert_or_update(waiter);
    if inserted {
        true
    } else {
        crate::debug::println!(
            "rtc sleep waiter table full: task={} wake_tick={}",
            task_id,
            wake_tick
        );
        false
    }
}

pub fn arm_sleep_waiter_until_tick(task_id: u64, wake_tick: u64) -> bool {
    register_sleep_waiter(task_id, wake_tick)
}

/// Release the exact task's deadline record and report whether this call owned
/// one. A wait transaction can use the result to prove that completion did not
/// leave stale timer authority behind for a later reuse of the same task.
pub fn disarm_sleep_waiter(task_id: u64) -> bool {
    #[cfg(rustos_boot_image)]
    {
        interrupts::without_interrupts(|| RTC_SLEEP_WAITERS.lock().remove_task(task_id))
    }
    #[cfg(not(rustos_boot_image))]
    {
        RTC_SLEEP_WAITERS.lock().remove_task(task_id)
    }
}

fn wake_ready_sleepers(now: u64) {
    // Clockevent handlers must never spin on state owned by the interrupted
    // context. Registration/cancellation mask interrupts while mutating the
    // table, so a transient collision is safely retried by the next PIT/RTC
    // edge. Expiry is only a notification: the waiter remains owned until the
    // resumed task acknowledges it through `disarm_sleep_waiter`. Reissuing
    // the wake on later ticks preserves the deadline as independent recovery
    // authority if a scheduler transition is delayed or coalesced.
    let mut ready_tasks = [SleepWakeNotification {
        task_id: 0,
        notification_count: 0,
    }; RTC_SLEEP_WAITER_CAPACITY];
    let Some(ready_len) =
        try_snapshot_ready_sleep_waiters(&RTC_SLEEP_WAITERS, now, &mut ready_tasks)
    else {
        if RTC_SLEEP_LOCK_MISSES.fetch_add(1, Ordering::Relaxed) == 0 {
            crate::debug::println!("rtc: sleep-waiter lock collision deferred");
        }
        return;
    };
    for notification in ready_tasks.into_iter().take(ready_len) {
        let task_id = notification.task_id;
        if notification.notification_count >= 128
            && notification.notification_count.is_power_of_two()
        {
            crate::debug::println!(
                "rtc: sleep waiter remains unacknowledged task={} now={} notifications={}",
                task_id,
                now,
                notification.notification_count
            );
        }
        if !crate::hooks::wake_user_task(task_id) {
            crate::debug::println!("rtc: expired sleep waiter had no live task task={task_id}");
            if let Some(mut waiters) = RTC_SLEEP_WAITERS.try_lock() {
                waiters.remove_task(task_id);
            }
        }
    }
}

/// `None` means the table is transiently owned by process context and the
/// interrupt must return without spinning. `Some(0)` means the table was
/// observed and contains no expired task.
fn try_snapshot_ready_sleep_waiters(
    waiters: &RtcSleepWaiterLock,
    now: u64,
    ready: &mut [SleepWakeNotification; RTC_SLEEP_WAITER_CAPACITY],
) -> Option<usize> {
    waiters
        .try_lock()
        .map(|mut waiters| waiters.snapshot_ready(now, ready))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_to_binary_decodes_common_values() {
        assert_eq!(bcd_to_binary(0x00), 0);
        assert_eq!(bcd_to_binary(0x42), 42);
        assert_eq!(bcd_to_binary(0x59), 59);
    }

    #[test]
    fn expand_year_uses_expected_century_split() {
        assert_eq!(expand_year(69), 2069);
        assert_eq!(expand_year(70), 1970);
        assert_eq!(expand_year(99), 1999);
    }

    #[test]
    fn decode_datetime_handles_bcd_12_hour_pm() {
        let raw = RawRtcDateTime {
            second: 0x58,
            minute: 0x23,
            hour: 0x89,
            weekday: 0x06,
            day: 0x21,
            month: 0x12,
            year: 0x24,
        };
        let decoded = decode_datetime(raw, 0);

        assert_eq!(
            decoded,
            RtcDateTime {
                year: 2024,
                month: 12,
                day: 21,
                weekday: 6,
                hour: 21,
                minute: 23,
                second: 58,
            }
        );
    }

    #[test]
    fn decode_datetime_preserves_binary_24_hour_input() {
        let raw = RawRtcDateTime {
            second: 7,
            minute: 8,
            hour: 19,
            weekday: 4,
            day: 3,
            month: 2,
            year: 70,
        };
        let decoded = decode_datetime(raw, (1 << 2) | (1 << 1));

        assert_eq!(
            decoded,
            RtcDateTime {
                year: 1970,
                month: 2,
                day: 3,
                weekday: 4,
                hour: 19,
                minute: 8,
                second: 7,
            }
        );
    }

    #[test]
    fn select_cmos_register_masks_register_bit_and_controls_nmi_flag() {
        let disabled = (0x4a & CMOS_REGISTER_MASK) | NMI_DISABLE;
        let enabled = 0x4a & CMOS_REGISTER_MASK;

        assert_eq!(disabled, 0xca);
        assert_eq!(enabled, 0x4a);
        assert_eq!(disabled & NMI_DISABLE, NMI_DISABLE);
        assert_eq!(enabled & NMI_DISABLE, 0);
    }

    #[test]
    fn sleep_deadline_uses_monotonic_ticks_with_ceil_and_saturation() {
        assert_eq!(sleep_deadline_from_ticks(10_000, 1), 10_002);
        assert_eq!(sleep_deadline_from_ticks(10_000, 8), 10_009);
        assert_eq!(sleep_deadline_from_ticks(u64::MAX - 1, 1), u64::MAX);
    }

    #[test]
    fn sleep_waiter_update_expiry_and_cancel_preserve_exact_task_ownership() {
        let mut waiters = SleepWaiterTable::new();
        let mut ready = [SleepWakeNotification {
            task_id: 0,
            notification_count: 0,
        }; RTC_SLEEP_WAITER_CAPACITY];
        assert!(waiters.insert_or_update(SleepWaiter {
            task_id: 41,
            wake_tick: 10,
            last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
            notification_count: 0,
        }));
        assert!(waiters.insert_or_update(SleepWaiter {
            task_id: 42,
            wake_tick: 5,
            last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
            notification_count: 0,
        }));
        assert!(waiters.insert_or_update(SleepWaiter {
            task_id: 41,
            wake_tick: 3,
            last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
            notification_count: 0,
        }));

        assert_eq!(waiters.snapshot_ready(2, &mut ready), 0);
        assert_eq!(waiters.snapshot_ready(3, &mut ready), 1);
        assert_eq!(ready[0].task_id, 41);
        assert_eq!(waiters.snapshot_ready(3, &mut ready), 0);
        assert_eq!(waiters.snapshot_ready(5, &mut ready), 1);
        assert_eq!(ready[0].task_id, 42);
        assert_eq!(
            waiters.snapshot_ready(3 + RTC_SLEEP_RENOTIFY_TICKS, &mut ready),
            1
        );
        assert_eq!(ready[0].task_id, 41);
        assert_eq!(ready[0].notification_count, 2);
        // Expiry notifies but does not release the timer owner. Only the
        // resumed task's acknowledgement removes its exact waiter.
        waiters.remove_task(41);
        waiters.remove_task(42);
        assert_eq!(waiters.snapshot_ready(u64::MAX, &mut ready), 0);
    }

    #[test]
    fn sleep_waiter_clockevent_collision_is_nonblocking_and_retryable() {
        let waiters = RtcSleepWaiterLock::new(SleepWaiterTable::new());
        let mut ready = [SleepWakeNotification {
            task_id: 0,
            notification_count: 0,
        }; RTC_SLEEP_WAITER_CAPACITY];
        {
            let mut owner = waiters.lock();
            assert!(owner.insert_or_update(SleepWaiter {
                task_id: 77,
                wake_tick: 4,
                last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
                notification_count: 0,
            }));
            assert_eq!(
                try_snapshot_ready_sleep_waiters(&waiters, 4, &mut ready),
                None
            );
        }
        assert_eq!(
            try_snapshot_ready_sleep_waiters(&waiters, 4, &mut ready),
            Some(1)
        );
        assert_eq!(ready[0].task_id, 77);
        assert_eq!(
            try_snapshot_ready_sleep_waiters(&waiters, 4, &mut ready),
            Some(0)
        );
        assert_eq!(
            try_snapshot_ready_sleep_waiters(&waiters, 4 + RTC_SLEEP_RENOTIFY_TICKS, &mut ready),
            Some(1)
        );
        waiters.lock().remove_task(77);
        assert_eq!(
            try_snapshot_ready_sleep_waiters(&waiters, 4, &mut ready),
            Some(0)
        );
    }
}
