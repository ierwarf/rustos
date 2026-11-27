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
#[cfg(test)]
use alloc::vec;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
const RTC_SLEEP_INITIAL_CAPACITY: usize = 256;
const RTC_SLEEP_READY_BATCH: usize = 64;
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
/// Lock-free mirror of `SleepWaiterTable::next_due`, written only while the
/// table lock is held. The 1024 Hz clockevent compares against it and skips
/// the table entirely while no deadline or renotification is due, instead of
/// taking the lock that every bounded IPC wait arms and disarms and scanning
/// all 256 slots on every edge. A stale (earlier) value only costs one scan.
static RTC_SLEEP_NEXT_DUE: AtomicU64 = AtomicU64::new(u64::MAX);

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

/// Published mirror of `SleepWaiterTable` occupancy/capacity so
/// `register_sleep_waiter` preallocates backing before disabling interrupts.
static RTC_SLEEP_CAPACITY_HINT: AtomicUsize = AtomicUsize::new(0);
static RTC_SLEEP_COUNT_HINT: AtomicUsize = AtomicUsize::new(0);

struct SleepWaiterTable {
    slots: Vec<Option<SleepWaiter>>,
    len: usize,
    /// Earliest tick at which any waiter needs a (re)notification; never later
    /// than the true value. `u64::MAX` when nothing is armed.
    next_due: u64,
}

impl SleepWaiterTable {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
            next_due: u64::MAX,
        }
    }

    #[inline]
    fn needs_grow(&self) -> bool {
        self.slots.is_empty() || (self.len + 1) * 4 >= self.slots.len() * 3
    }

    #[inline]
    fn slot_mask(&self) -> usize {
        debug_assert!(self.slots.len().is_power_of_two());
        self.slots.len() - 1
    }

    #[inline]
    fn home_slot_for(task_id: u64, capacity: usize) -> usize {
        // Fibonacci hashing keeps sequential task IDs and IDs sharing low bits
        // spread across the power-of-two table without storing hash metadata.
        let shift = u64::BITS - capacity.ilog2();
        (task_id.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> shift) as usize
    }

    fn install_rehashed(slots: &mut [Option<SleepWaiter>], waiter: SleepWaiter) {
        let cap = slots.len();
        let mask = cap - 1;
        let home = Self::home_slot_for(waiter.task_id, cap);
        for probe in 0..cap {
            let index = (home + probe) & mask;
            if slots[index].is_none() {
                slots[index] = Some(waiter);
                return;
            }
        }
        unreachable!("rehashed SleepWaiterTable exceeded load bound");
    }

    fn rehash_with(&mut self, mut next: Vec<Option<SleepWaiter>>) -> Vec<Option<SleepWaiter>> {
        for waiter in self.slots.iter_mut().filter_map(Option::take) {
            Self::install_rehashed(&mut next, waiter);
        }
        core::mem::replace(&mut self.slots, next)
    }

    fn insert_with_backing(
        &mut self,
        waiter: SleepWaiter,
        backing: &mut Option<Vec<Option<SleepWaiter>>>,
    ) -> Result<Option<Vec<Option<SleepWaiter>>>, ()> {
        let mut evicted = None;
        if let Some(existing) = self.find_mut(waiter.task_id) {
            existing.wake_tick = waiter.wake_tick;
            existing.last_notify_tick = RTC_SLEEP_UNNOTIFIED_TICK;
            existing.notification_count = 0;
            self.next_due = self.next_due.min(waiter.wake_tick);
            return Ok(None);
        }
        if self.needs_grow() {
            let next = backing.take().ok_or(())?;
            if (self.len + 1) * 4 >= next.len() * 3 {
                *backing = Some(next);
                return Err(());
            }
            evicted = Some(self.rehash_with(next));
        }
        let cap = self.slots.len();
        let mask = cap - 1;
        let home = Self::home_slot_for(waiter.task_id, cap);
        for probe in 0..cap {
            let index = (home + probe) & mask;
            if self.slots[index].is_none() {
                self.slots[index] = Some(waiter);
                self.len += 1;
                self.next_due = self.next_due.min(waiter.wake_tick);
                return Ok(evicted);
            }
        }
        Err(())
    }

    #[cfg(test)]
    fn insert_or_update(&mut self, waiter: SleepWaiter) -> bool {
        let mut backing = if self.needs_grow() {
            let next_cap = if self.slots.is_empty() {
                RTC_SLEEP_INITIAL_CAPACITY
            } else {
                self.slots.len() * 2
            };
            Some(vec![None; next_cap])
        } else {
            None
        };
        let _ = self.insert_with_backing(waiter, &mut backing);
        true
    }

    fn find_mut(&mut self, task_id: u64) -> Option<&mut SleepWaiter> {
        if self.slots.is_empty() {
            return None;
        }
        let cap = self.slots.len();
        let mask = cap - 1;
        let home = Self::home_slot_for(task_id, cap);
        for probe in 0..cap {
            let index = (home + probe) & mask;
            match self.slots[index] {
                Some(ref existing) if existing.task_id == task_id => {
                    return self.slots[index].as_mut();
                }
                None => return None,
                Some(_) => {}
            }
        }
        None
    }

    fn snapshot_ready(&mut self, now: u64, ready: &mut [SleepWakeNotification]) -> usize {
        let mut ready_len = 0;
        let mut next_due = u64::MAX;
        let mut saturated = false;
        for slot in self.slots.iter_mut() {
            let Some(waiter) = slot.as_mut() else {
                continue;
            };
            let first_notification = waiter.last_notify_tick == RTC_SLEEP_UNNOTIFIED_TICK;
            let retry_due = now.saturating_sub(waiter.last_notify_tick) >= RTC_SLEEP_RENOTIFY_TICKS;
            if waiter.wake_tick <= now && (first_notification || retry_due) {
                if ready_len < ready.len() {
                    waiter.notification_count = waiter.notification_count.saturating_add(1);
                    ready[ready_len] = SleepWakeNotification {
                        task_id: waiter.task_id,
                        notification_count: waiter.notification_count,
                    };
                    ready_len += 1;
                    waiter.last_notify_tick = now;
                } else {
                    saturated = true;
                }
            }
            let due = if waiter.last_notify_tick == RTC_SLEEP_UNNOTIFIED_TICK {
                waiter.wake_tick
            } else {
                waiter.last_notify_tick.saturating_add(RTC_SLEEP_RENOTIFY_TICKS)
            };
            next_due = next_due.min(due);
        }
        self.next_due = if saturated { next_due.min(now) } else { next_due };
        ready_len
    }

    /// Pulls an existing waiter's deadline forward to `now` without creating
    /// one. False when the task has no armed deadline.
    fn expedite(&mut self, task_id: u64, now: u64) -> bool {
        let Some(waiter) = self.find_mut(task_id) else {
            return false;
        };
        waiter.wake_tick = waiter.wake_tick.min(now);
        waiter.last_notify_tick = RTC_SLEEP_UNNOTIFIED_TICK;
        let wake_tick = waiter.wake_tick;
        self.next_due = self.next_due.min(wake_tick);
        true
    }

    fn remove_task(&mut self, task_id: u64) -> bool {
        if self.slots.is_empty() {
            return false;
        }
        let cap = self.slots.len();
        let mask = cap - 1;
        let home = Self::home_slot_for(task_id, cap);
        for probe in 0..cap {
            let index = (home + probe) & mask;
            match self.slots[index] {
                Some(waiter) if waiter.task_id == task_id => {
                    self.close_deletion_hole(index);
                    self.len -= 1;
                    return true;
                }
                None => return false,
                Some(_) => {}
            }
        }
        false
    }

    fn close_deletion_hole(&mut self, mut hole: usize) {
        self.slots[hole] = None;
        let cap = self.slots.len();
        let mask = self.slot_mask();
        let mut scan = (hole + 1) & mask;
        while let Some(waiter) = self.slots[scan] {
            let home = Self::home_slot_for(waiter.task_id, cap);
            let scan_distance = scan.wrapping_sub(home) & mask;
            let hole_distance = hole.wrapping_sub(home) & mask;
            if hole_distance < scan_distance {
                self.slots[hole] = Some(waiter);
                self.slots[scan] = None;
                hole = scan;
            }
            scan = (scan + 1) & mask;
        }
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

/// Serializes the 0x70/0x71 index+data port pair. Every access is a two-step
/// sequence, so two CPUs (`clock_gettime` can run on any of them) would
/// otherwise read each other's selected register. Callers hold local
/// interrupts off, so the holder cannot be preempted by the RTC handler.
static CMOS_LOCK: spin::Mutex<()> = spin::Mutex::new(());

fn cmos_read(reg: u8) -> u8 {
    // SAFETY: 0x70/0x71 are the architectural CMOS index/data ports; callers
    // hold `CMOS_LOCK` so the index/data pair is not interleaved.
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
    // SAFETY: as in `cmos_read`; callers hold `CMOS_LOCK`.
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
    // SAFETY: writes the architectural CMOS index port under `CMOS_LOCK`.
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

/// Upper bound on status polls while waiting for the update-in-progress bit to
/// clear. A healthy RTC holds it for under 2 ms, and each poll is at least one
/// port round trip, so this is several times the architectural worst case.
const RTC_UPDATE_POLL_LIMIT: u32 = 4096;
/// Re-reads allowed when two consecutive snapshots disagree.
const RTC_STABLE_READ_ATTEMPTS: u32 = 16;
/// Register A on a floating bus (no CMOS behind ports 0x70/0x71) reads as all
/// ones, which would pin the update-in-progress bit forever.
const RTC_ABSENT_STATUS: u8 = 0xff;

fn wait_for_update_end() -> bool {
    for _ in 0..RTC_UPDATE_POLL_LIMIT {
        if !update_in_progress() {
            return true;
        }
        spin_loop();
    }
    false
}

/// Reads one stable calendar snapshot, or `None` when the RTC is absent,
/// stuck mid-update, or never produces two matching reads. The previous loop
/// spun forever with interrupts disabled in all three cases, which on a
/// platform without a working CMOS clock (dead cell, floating bus, hardware
/// reduced) hung boot instead of leaving the wall clock unreadable.
fn read_stable_datetime() -> Option<RtcDateTime> {
    if cmos_read(RTC_REG_A) == RTC_ABSENT_STATUS {
        return None;
    }
    for _ in 0..RTC_STABLE_READ_ATTEMPTS {
        if !wait_for_update_end() {
            return None;
        }
        let first = read_raw_datetime();

        if !wait_for_update_end() {
            return None;
        }
        let second = read_raw_datetime();

        if first == second {
            let reg_b = cmos_read(RTC_REG_B);
            return Some(decode_datetime(second, reg_b));
        }

        spin_loop();
    }
    None
}

/// The wall-clock calendar, or the all-zero value (month 0, day 0) when the RTC
/// cannot be read. Callers already reject that as "unreadable calendar" rather
/// than latching a fabricated epoch.
pub fn now() -> RtcDateTime {
    interrupts::without_interrupts(|| {
        let _cmos = CMOS_LOCK.lock();
        read_stable_datetime()
    })
    .unwrap_or(RtcDateTime {
        year: 0,
        month: 0,
        day: 0,
        weekday: 0,
        hour: 0,
        minute: 0,
        second: 0,
    })
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

/// `RTC_TICKS_PER_SEC / 1e9` in the clock's fixed point, rounded up.
///
/// Both operands are literals and the division still compiled to a `__udivti3`
/// call -- LLVM does not strength reduce a `u128` division by a constant.
/// Evaluating it in a `const` does, because the whole expression folds at
/// compile time.
///
/// Rounded up for the same reason as the clock's multipliers, and here the
/// truncating version was off by a whole tick: one second of uptime came back
/// as 1023, because 1024 landed at 1023.99999946 before the shift.
const NANOS_TO_TICKS_MULT: u64 = {
    let numerator = (RTC_TICKS_PER_SEC as u128) << crate::arch::clock::NANOS_SHIFT;
    ((numerator + 1_000_000_000 - 1) / 1_000_000_000) as u64
};

pub fn ticks() -> u64 {
    let nanos = crate::arch::clock::monotonic_nanos_coarse();
    if nanos == 0 && crate::arch::clock::current_source().is_none() {
        // ORDERING: Acquire pairs with the AcqRel `fetch_add` in `on_interrupt`; this is
        // only the pre-clocksource fallback, and the count itself is the whole payload.
        return RTC_TICKS.load(Ordering::Acquire);
    }
    crate::arch::clock::scale_by_nanos_mult(nanos, NANOS_TO_TICKS_MULT)
}

/// One observation of the clock for a path that needs its tick, monotonic and
/// scheduler-local views together (one ordered TSC read instead of four).
#[derive(Clone, Copy, Debug)]
pub struct ClockSample {
    pub ticks: u64,
    pub monotonic_nanos: u64,
    pub local_nanos: Option<u64>,
}

pub fn sample() -> ClockSample {
    if let Some((monotonic_nanos, local_nanos)) = crate::arch::clock::tsc_views() {
        return ClockSample {
            ticks: crate::arch::clock::scale_by_nanos_mult(monotonic_nanos, NANOS_TO_TICKS_MULT),
            monotonic_nanos,
            local_nanos: Some(local_nanos),
        };
    }
    ClockSample {
        ticks: ticks(),
        monotonic_nanos: monotonic_nanos(),
        local_nanos: crate::arch::clock::scheduler_local_nanos(),
    }
}

pub const fn ticks_per_second() -> u64 {
    RTC_TICKS_PER_SEC
}

pub fn is_initialized() -> bool {
    // ORDERING: Acquire pairs with the Release store in `init` (below): a caller
    // that sees `true` also sees the CMOS register programming that preceded it.
    RTC_INITIALIZED.load(Ordering::Acquire)
}

pub fn init() {
    interrupts::without_interrupts(|| {
        let _cmos = CMOS_LOCK.lock();
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

    // ORDERING: Release publishes the completed CMOS setup (done under `CMOS_LOCK`
    // with interrupts off) to `is_initialized` Acquire loads; it is set once.
    RTC_INITIALIZED.store(true, Ordering::Release);
}

pub fn on_interrupt() {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    // ORDERING: AcqRel on the edge counter, whose only writer is this IRQ leaf; it
    // pairs with Acquire loads in `ticks` and the uptime path. The count is the
    // whole payload, so the strength is conservative on a cold diagnostic path.
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
    // ORDERING: Acquire pairs with the AcqRel `fetch_add` that ends the previous
    // handler below, so `inflight` reflects only handlers truly still running.
    let completed = RTC_TICKS_COMPLETED.load(Ordering::Acquire);
    let inflight = interrupt_count.saturating_sub(completed);
    if inflight > 1 {
        // ORDERING: Acquire reads the last printed tick, written by the AcqRel CAS
        // below on whichever CPU printed last; it only rate-limits a diagnostic line.
        let last_diag_tick = RTC_LAST_DIAG_PRINT_TICK.load(Ordering::Acquire);
        if interrupt_count.saturating_sub(last_diag_tick) >= 4
            && RTC_LAST_DIAG_PRINT_TICK
                .compare_exchange(
                    last_diag_tick,
                    interrupt_count,
                    // ORDERING: CAS elects exactly one printer per 4-tick window; AcqRel on success,
                    // Acquire on failure to re-read the winner's tick. No data rides on the CAS.
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
    {
        let _cmos = CMOS_LOCK.lock();
        let _ = cmos_read(RTC_REG_C);
    }
    // ORDERING: AcqRel marks this edge fully handled (CMOS register C acked); it
    // pairs with the Acquire load in the next edge's in-flight check above.
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
    // ORDERING: Acquire pairs with the AcqRel CAS in `drain_pending_heartbeat`,
    // the only writer, so a drained second is never re-marked as pending.
    let last_reported_second = RTC_LAST_ALIVE_SECOND.load(Ordering::Acquire);
    if current_second != last_reported_second {
        // ORDERING: Release publishes the pending second to the Acquire load in
        // `drain_pending_heartbeat`; the value is the only payload (no pointers).
        HEARTBEAT_PENDING_SECOND.store(current_second, Ordering::Release);
    }
}

/// Drain a pending heartbeat second (if any) and emit its diagnostic log. Must
/// be called outside IRQ context — the housekeeping task is the intended
/// caller. Returns the number of seconds drained.
pub fn drain_pending_heartbeat() -> usize {
    // ORDERING: Acquire pairs with the Release store in `service_clock_event`
    // that marked this second pending.
    let pending = HEARTBEAT_PENDING_SECOND.load(Ordering::Acquire);
    if pending == u64::MAX {
        return 0;
    }
    // ORDERING: Acquire reads the last reported second; its writer is the CAS
    // below, so concurrent drainers agree on one reported value.
    let last_reported = RTC_LAST_ALIVE_SECOND.load(Ordering::Acquire);
    if pending == last_reported {
        return 0;
    }
    if RTC_LAST_ALIVE_SECOND
        // ORDERING: AcqRel CAS elects a single drainer per second (losers return 0);
        // failure Acquire re-reads the winner's value. Only the second number moves.
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
            // ORDERING: AcqRel swap takes the previous packet-count sample atomically for
            // the delta; only the heartbeat drain winner (CAS above) calls this, and the
            // counter is a diagnostic with no dependent data.
            .swap(input_snapshot.pointer_packet_submits, Ordering::AcqRel),
    );
    let input_read_call_delta = input_snapshot.read_calls.saturating_sub(
        // ORDERING: same single-consumer diagnostic swap, for the read-call count.
        RTC_LAST_INPUT_READ_CALL_COUNT.swap(input_snapshot.read_calls, Ordering::AcqRel),
    );
    let input_read_event_delta = input_snapshot.read_events.saturating_sub(
        // ORDERING: same single-consumer diagnostic swap, for the read-event count.
        RTC_LAST_INPUT_READ_EVENT_COUNT.swap(input_snapshot.read_events, Ordering::AcqRel),
    );
    let linux_irq_depth_delta = (linux_irq_total_depth as u64).saturating_sub(
        // ORDERING: same single-consumer diagnostic swap, for the Linux IRQ lock depth.
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
    sleep_unless(milliseconds, || false);
}

/// `sleep` that also ends as soon as `abort` holds. A non-deadline wake of the
/// sleeping task makes the loop re-evaluate `abort`, so an event producer can
/// cut the nap short instead of waiting for the next tick.
pub fn sleep_unless(milliseconds: u64, abort: impl Fn() -> bool) {
    sleep_unless_with_post_arm(milliseconds, abort, || false);
}

/// Like [`sleep_unless`], plus a lockless `post_arm_abort` check evaluated
/// after the scheduler block arm and timer waiter are both published, closing
/// the lost-wakeup window between `abort()` and `arm_block_current_task()`.
pub fn sleep_unless_with_post_arm(
    milliseconds: u64,
    abort: impl Fn() -> bool,
    post_arm_abort: impl Fn() -> bool,
) {
    if milliseconds == 0 {
        return;
    }

    let target = sleep_deadline_from_ticks(ticks(), milliseconds);

    let restore_disabled = !interrupts::are_enabled();
    while ticks() < target && !abort() {
        if !restore_disabled && block_current_user_until(target, &post_arm_abort) {
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

fn block_current_user_until(target: u64, post_arm_abort: &impl Fn() -> bool) -> bool {
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

    if ticks() >= target || post_arm_abort() {
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
    // edge, signal arrival, or unrelated wake in either gap cancels the sleep
    // instead of missing the wakeup.
    if ticks() >= target || post_arm_abort() {
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
    // acquire the same non-reentrant lock. Heap allocation happens outside the
    // interrupt-disabled critical section.
    let waiter = SleepWaiter {
        task_id,
        wake_tick,
        last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
        notification_count: 0,
    };
    let mut backing: Option<Vec<Option<SleepWaiter>>> = None;
    loop {
        // ORDERING: Relaxed capacity/count hints size the preallocation outside lock.
        let cap = RTC_SLEEP_CAPACITY_HINT.load(Ordering::Relaxed);
        let cnt = RTC_SLEEP_COUNT_HINT.load(Ordering::Relaxed);
        let need_cap = if cap == 0 || (cnt + 1) * 4 >= cap * 3 {
            let mut next = cap.max(RTC_SLEEP_INITIAL_CAPACITY / 2);
            while (cnt + 1) * 4 >= next * 3 {
                next = match next.checked_mul(2) {
                    Some(v) => v,
                    None => return false,
                };
            }
            next
        } else {
            0
        };
        if need_cap != 0 && backing.as_ref().is_none_or(|v| v.len() < need_cap) {
            let mut candidate = Vec::new();
            if candidate.try_reserve_exact(need_cap).is_err() {
                return false;
            }
            candidate.resize(need_cap, None);
            backing = Some(candidate);
        }

        #[allow(unused_mut)]
        let mut attempt = || {
            let mut table = RTC_SLEEP_WAITERS.lock();
            match table.insert_with_backing(waiter, &mut backing) {
                Ok(evicted) => {
                    // ORDERING: Release under table lock; clockevent's Acquire sees this deadline.
                    RTC_SLEEP_NEXT_DUE.store(table.next_due, Ordering::Release);
                    RTC_SLEEP_CAPACITY_HINT.store(table.slots.len(), Ordering::Relaxed);
                    RTC_SLEEP_COUNT_HINT.store(table.len, Ordering::Relaxed);
                    Ok(evicted)
                }
                Err(()) => {
                    RTC_SLEEP_CAPACITY_HINT.store(table.slots.len(), Ordering::Relaxed);
                    RTC_SLEEP_COUNT_HINT.store(table.len, Ordering::Relaxed);
                    Err(())
                }
            }
        };
        #[cfg(rustos_boot_image)]
        let res = if interrupts::are_enabled() {
            interrupts::without_interrupts(attempt)
        } else {
            attempt()
        };
        #[cfg(not(rustos_boot_image))]
        let res = attempt();
        match res {
            Ok(evicted) => {
                drop(evicted);
                drop(backing);
                return true;
            }
            Err(()) => continue,
        }
    }
}

pub fn arm_sleep_waiter_until_tick(task_id: u64, wake_tick: u64) -> bool {
    register_sleep_waiter(task_id, wake_tick)
}

/// Makes the task's already-armed deadline due on the next clockevent, which
/// then delivers a generic wake (re-sent every few ticks until the task
/// disarms). Used when an exact typed wake was refused: the waiter re-checks
/// its condition on any wake, so an early one costs a re-poll, while a lost
/// one cost the whole deadline. Never creates a waiter.
pub fn expedite_sleep_waiter(task_id: u64) -> bool {
    let now = ticks();
    let expedite = || {
        let mut table = RTC_SLEEP_WAITERS.lock();
        let expedited = table.expedite(task_id, now);
        // ORDERING: Release under the table lock, as in registration.
        RTC_SLEEP_NEXT_DUE.store(table.next_due, Ordering::Release);
        expedited
    };
    #[cfg(rustos_boot_image)]
    {
        interrupts::without_interrupts(expedite)
    }
    #[cfg(not(rustos_boot_image))]
    {
        expedite()
    }
}

/// Release the exact task's deadline record and report whether this call owned
/// one. A wait transaction can use the result to prove that completion did not
/// leave stale timer authority behind for a later reuse of the same task.
pub fn disarm_sleep_waiter(task_id: u64) -> bool {
    let disarm = || {
        let mut table = RTC_SLEEP_WAITERS.lock();
        let removed = table.remove_task(task_id);
        RTC_SLEEP_COUNT_HINT.store(table.len, Ordering::Relaxed);
        removed
    };
    #[cfg(rustos_boot_image)]
    {
        if interrupts::are_enabled() {
            interrupts::without_interrupts(disarm)
        } else {
            disarm()
        }
    }
    #[cfg(not(rustos_boot_image))]
    {
        disarm()
    }
}

fn wake_ready_sleepers(now: u64) {
    // ORDERING: Acquire pairs with the Release publication under the lock.
    if now < RTC_SLEEP_NEXT_DUE.load(Ordering::Acquire) {
        return;
    }
    if !notify_ready_sleepers(&RTC_SLEEP_WAITERS, now, crate::hooks::wake_task) {
        if RTC_SLEEP_LOCK_MISSES.fetch_add(1, Ordering::Relaxed) == 0 {
            crate::debug::println!("rtc: sleep-waiter lock collision deferred");
        }
        return;
    }
    // Publish the scan's recomputed due point. A collision leaves the old,
    // earlier value, which only makes the next edge scan again.
    if let Some(table) = RTC_SLEEP_WAITERS.try_lock() {
        // ORDERING: Release under the table lock, as in registration.
        RTC_SLEEP_NEXT_DUE.store(table.next_due, Ordering::Release);
    }
}

fn notify_ready_sleepers(
    waiters: &RtcSleepWaiterLock,
    now: u64,
    mut wake: impl FnMut(u64) -> bool,
) -> bool {
    // Clockevent handlers must never spin on state owned by the interrupted
    // context, and must neither allocate in IRQ context nor blow the stack.
    // Snapshots drain in bounded 64-entry batches.
    let mut ready_tasks = [SleepWakeNotification {
        task_id: 0,
        notification_count: 0,
    }; RTC_SLEEP_READY_BATCH];
    loop {
        let Some(ready_len) = try_snapshot_ready_sleep_waiters(waiters, now, &mut ready_tasks)
        else {
            return false;
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
            let admitted = wake(task_id);
            if !admitted && notification.notification_count.is_power_of_two() {
                crate::debug::println!("rtc: sleep wake deferred task={task_id} now={now}");
            }
        }
        if ready_len < RTC_SLEEP_READY_BATCH {
            return true;
        }
    }
}

/// `None` means the table is transiently owned by process context and the
/// interrupt must return without spinning. `Some(0)` means the table was
/// observed and contains no expired task.
fn try_snapshot_ready_sleep_waiters(
    waiters: &RtcSleepWaiterLock,
    now: u64,
    ready: &mut [SleepWakeNotification],
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
        }; RTC_SLEEP_READY_BATCH];
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
    fn sleep_waiter_hash_chain_survives_middle_and_wrapped_deletions() {
        let first = 1_u64;
        let home = SleepWaiterTable::home_slot_for(first, RTC_SLEEP_INITIAL_CAPACITY);
        let mut colliders = [first, 0, 0];
        let mut found = 1;
        for candidate in 2_u64..10_000 {
            if SleepWaiterTable::home_slot_for(candidate, RTC_SLEEP_INITIAL_CAPACITY) == home {
                colliders[found] = candidate;
                found += 1;
                if found == colliders.len() {
                    break;
                }
            }
        }
        assert_eq!(found, colliders.len());

        let mut waiters = SleepWaiterTable::new();
        for task_id in colliders {
            assert!(waiters.insert_or_update(SleepWaiter {
                task_id,
                wake_tick: 10,
                last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
                notification_count: 0,
            }));
        }
        assert!(waiters.remove_task(colliders[1]));
        assert!(!waiters.remove_task(colliders[1]));
        assert!(waiters.remove_task(colliders[0]));
        assert!(waiters.remove_task(colliders[2]));
    }

    #[test]
    fn sleep_waiter_table_grows_past_initial_capacity_and_reuses_deleted_slots() {
        let count = 1500_u64;
        let mut waiters = SleepWaiterTable::new();
        for task_id in 0..count {
            assert!(waiters.insert_or_update(SleepWaiter {
                task_id,
                wake_tick: task_id,
                last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
                notification_count: 0,
            }));
        }
        for task_id in 0..count {
            assert!(waiters.remove_task(task_id));
            assert!(waiters.insert_or_update(SleepWaiter {
                task_id: task_id + count,
                wake_tick: task_id,
                last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
                notification_count: 0,
            }));
        }
        for task_id in count..(count * 2) {
            assert!(waiters.remove_task(task_id));
        }
    }

    #[test]
    fn actual_clockevent_refusal_and_notification_keep_deadline_until_exact_ack() {
        let waiters = RtcSleepWaiterLock::new(SleepWaiterTable::new());
        assert!(waiters.lock().insert_or_update(SleepWaiter {
            task_id: 77,
            wake_tick: 4,
            last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
            notification_count: 0,
        }));
        let mut notifications = 0;
        let mut wake = |id| {
            assert_eq!(id, 77);
            notifications += 1;
            notifications > 2 // Two transient refusals, then admitted notification.
        };
        assert!(notify_ready_sleepers(&waiters, 4, &mut wake));
        assert_eq!(waiters.lock().slots.iter().flatten().count(), 1);
        // No busy polling between clockevents, and success is not an ACK either.
        assert!(notify_ready_sleepers(
            &waiters,
            4 + RTC_SLEEP_RENOTIFY_TICKS - 1,
            &mut wake
        ));
        for retry in 1..=3 {
            assert!(notify_ready_sleepers(
                &waiters,
                4 + retry * RTC_SLEEP_RENOTIFY_TICKS,
                &mut wake
            ));
        }
        assert_eq!(notifications, 4);
        let original = *waiters.lock().slots.iter().flatten().next().unwrap();
        assert_eq!(original.task_id, 77);
        assert_eq!(original.wake_tick, 4);
        assert_eq!(original.notification_count, 4);
        assert!(waiters.lock().remove_task(77));
        assert!(notify_ready_sleepers(&waiters, 100, |_| panic!(
            "ACKed waiter was re-notified"
        )));
    }

    #[test]
    fn sleep_waiter_clockevent_collision_is_nonblocking_and_retryable() {
        let waiters = RtcSleepWaiterLock::new(SleepWaiterTable::new());
        let mut ready = [SleepWakeNotification {
            task_id: 0,
            notification_count: 0,
        }; RTC_SLEEP_READY_BATCH];
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

    /// A second of uptime is `RTC_TICKS_PER_SEC` ticks, and every whole second
    /// after it still is.
    ///
    /// The tick conversion stopped dividing and started multiplying by a
    /// fixed-point reciprocal. With that reciprocal truncated instead of
    /// rounded up, one second read as 1023: the product landed at 1023.99999946
    /// and the shift took the fraction. Nothing else in this file would have
    /// noticed a deadline wheel running slow by one tick in every second.
    #[test]
    fn whole_seconds_convert_to_whole_ticks() {
        for seconds in [1_u64, 2, 60, 3_600, 86_400] {
            let nanos = seconds * 1_000_000_000;
            assert_eq!(
                crate::arch::clock::scale_by_nanos_mult(nanos, NANOS_TO_TICKS_MULT),
                seconds * RTC_TICKS_PER_SEC,
                "{seconds}s did not convert to whole ticks"
            );
        }
    }

    #[test]
    fn expedite_moves_only_an_existing_deadline_forward() {
        let mut waiters = SleepWaiterTable::new();
        let mut ready = [SleepWakeNotification {
            task_id: 0,
            notification_count: 0,
        }; RTC_SLEEP_READY_BATCH];
        assert!(!waiters.expedite(9, 5));
        assert!(waiters.insert_or_update(SleepWaiter {
            task_id: 9,
            wake_tick: 500,
            last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
            notification_count: 0,
        }));
        assert!(waiters.expedite(9, 5));
        assert_eq!(waiters.next_due, 5);
        assert_eq!(waiters.snapshot_ready(5, &mut ready), 1);
        assert_eq!(ready[0].task_id, 9);
        // Never moves a deadline later.
        assert!(waiters.expedite(9, 50));
        assert_eq!(waiters.slots.iter().flatten().next().unwrap().wake_tick, 5);
    }

    #[test]
    fn next_due_tracks_the_earliest_pending_notification() {
        let mut waiters = SleepWaiterTable::new();
        let mut ready = [SleepWakeNotification {
            task_id: 0,
            notification_count: 0,
        }; RTC_SLEEP_READY_BATCH];
        assert_eq!(waiters.next_due, u64::MAX);
        for (task_id, wake_tick) in [(1, 40), (2, 10)] {
            assert!(waiters.insert_or_update(SleepWaiter {
                task_id,
                wake_tick,
                last_notify_tick: RTC_SLEEP_UNNOTIFIED_TICK,
                notification_count: 0,
            }));
        }
        assert_eq!(waiters.next_due, 10);
        assert_eq!(waiters.snapshot_ready(5, &mut ready), 0);
        assert_eq!(waiters.next_due, 10);
        assert_eq!(waiters.snapshot_ready(10, &mut ready), 1);
        assert_eq!(waiters.next_due, 10 + RTC_SLEEP_RENOTIFY_TICKS);
        assert!(waiters.remove_task(2));
        assert_eq!(waiters.snapshot_ready(11, &mut ready), 0);
        assert_eq!(waiters.next_due, 40);
        assert!(waiters.remove_task(1));
        assert_eq!(waiters.snapshot_ready(12, &mut ready), 0);
        assert_eq!(waiters.next_due, u64::MAX);
    }
}
