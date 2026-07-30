use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Mutex as RawSpinMutex;
use x86_64::instructions::interrupts;

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;
const PRE_BLOCK_SPINS: usize = 256;
const IRQ_OFF_SPIN_LIMIT: usize = 100_000;
const SCHED_SPIN_LIMIT: usize = 1_000_000;
const MAX_WAITERS: usize = crate::multitask::MAX_SCHEDULER_TASKS;

#[cfg(test)]
static NEXT_TEST_LOCK_OWNER: AtomicUsize = AtomicUsize::new(1);

#[cfg(test)]
std::thread_local! {
    /// Host unit tests execute concurrently without RustOS task identities.
    /// Giving every host thread token 1 makes ordinary contention look like a
    /// recursive acquire and can drive the diagnostic path through guest-only
    /// scheduler state. Preserve recursive-acquire detection with one stable,
    /// nonzero token per host test thread instead.
    static TEST_LOCK_OWNER: usize = NEXT_TEST_LOCK_OWNER.fetch_add(1, Ordering::Relaxed);
}

pub(crate) struct KernelWaitLock<T: ?Sized, const CLASS: u8> {
    state: AtomicUsize,
    owner: AtomicUsize,
    owner_depth: AtomicUsize,
    owner_acquire_file_ptr: AtomicUsize,
    owner_acquire_file_len: AtomicUsize,
    owner_acquire_line: AtomicUsize,
    owner_acquire_tsc: AtomicU64,
    waiters: RawSpinMutex<WaitQueue>,
    value: UnsafeCell<T>,
}

pub(crate) struct KernelWaitGuard<'a, T: ?Sized, const CLASS: u8> {
    lock: &'a KernelWaitLock<T, CLASS>,
    owner: usize,
    external_raw: Option<nucleus_core::util::lockdep::ExternalRawLockGuard>,
    acquire_site: &'static Location<'static>,
    acquire_tsc: u64,
}

struct WaitQueue {
    tasks: [u64; MAX_WAITERS],
    len: usize,
}

impl WaitQueue {
    const fn new() -> Self {
        Self {
            tasks: [0; MAX_WAITERS],
            len: 0,
        }
    }

    fn push_unique(&mut self, task_id: u64) -> bool {
        if self.tasks[..self.len].contains(&task_id) {
            return true;
        }
        if self.len == self.tasks.len() {
            return false;
        }
        self.tasks[self.len] = task_id;
        self.len += 1;
        true
    }

    fn remove(&mut self, task_id: u64) {
        let Some(index) = self.tasks[..self.len]
            .iter()
            .position(|queued| *queued == task_id)
        else {
            return;
        };
        for next in index + 1..self.len {
            self.tasks[next - 1] = self.tasks[next];
        }
        self.len -= 1;
        self.tasks[self.len] = 0;
    }

    fn pop_front(&mut self) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        let task_id = self.tasks[0];
        for next in 1..self.len {
            self.tasks[next - 1] = self.tasks[next];
        }
        self.len -= 1;
        self.tasks[self.len] = 0;
        Some(task_id)
    }
}

// SAFETY: the raw-spin state plus scheduler-owned waiter handoff serializes
// access to T, and the Send bound permits unique ownership to cross CPUs.
unsafe impl<T: ?Sized + Send, const CLASS: u8> Send for KernelWaitLock<T, CLASS> {}
unsafe impl<T: ?Sized + Send, const CLASS: u8> Sync for KernelWaitLock<T, CLASS> {}

impl<T, const CLASS: u8> KernelWaitLock<T, CLASS> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(UNLOCKED),
            owner: AtomicUsize::new(0),
            owner_depth: AtomicUsize::new(0),
            owner_acquire_file_ptr: AtomicUsize::new(0),
            owner_acquire_file_len: AtomicUsize::new(0),
            owner_acquire_line: AtomicUsize::new(0),
            owner_acquire_tsc: AtomicU64::new(0),
            waiters: RawSpinMutex::new(WaitQueue::new()),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized, const CLASS: u8> KernelWaitLock<T, CLASS> {
    #[track_caller]
    pub(crate) fn lock(&self) -> KernelWaitGuard<'_, T, CLASS> {
        let acquire_site = Location::caller();
        assert_sleepable_lock_context(acquire_site);
        let owner = current_lock_owner_token();
        let wait_start_tsc = read_tsc();
        self.assert_not_recursive(owner, acquire_site);
        let mut spins = 0usize;
        loop {
            if let Some(acquire_tsc) = self.try_acquire_at(owner, acquire_site) {
                maybe_report_lock_wait(
                    "KernelWaitLock",
                    core::any::type_name::<T>(),
                    acquire_site,
                    wait_start_tsc,
                    acquire_tsc,
                    spins,
                );
                return KernelWaitGuard {
                    lock: self,
                    owner,
                    external_raw: None,
                    acquire_site,
                    acquire_tsc,
                };
            }

            spins = spins.saturating_add(1);
            if can_block_current_task() && spins >= PRE_BLOCK_SPINS {
                if let Some(acquire_tsc) = self.block_until_woken_or_acquired(owner, acquire_site) {
                    maybe_report_lock_wait(
                        "KernelWaitLock",
                        core::any::type_name::<T>(),
                        acquire_site,
                        wait_start_tsc,
                        acquire_tsc,
                        spins,
                    );
                    return KernelWaitGuard {
                        lock: self,
                        owner,
                        external_raw: None,
                        acquire_site,
                        acquire_tsc,
                    };
                }
                spins = 0;
                continue;
            }

            let limit = if interrupts::are_enabled() {
                SCHED_SPIN_LIMIT
            } else {
                IRQ_OFF_SPIN_LIMIT
            };
            if spins >= limit {
                let (owner_file, owner_line) = self.owner_acquire_site();
                panic!(
                    "KernelWaitLock contention exceeded bounded spin limit: type={} waiter={} current_owner={} owner_depth={} owner_acquire={}:{} wait_at={}:{} spins={} limit={} irq_enabled={} scheduler_initialized={} current_task={:?}",
                    core::any::type_name::<T>(),
                    owner,
                    self.owner.load(Ordering::Acquire),
                    self.owner_depth.load(Ordering::Acquire),
                    owner_file,
                    owner_line,
                    acquire_site.file(),
                    acquire_site.line(),
                    spins,
                    limit,
                    interrupts::are_enabled(),
                    crate::multitask::is_initialized(),
                    crate::multitask::current_task_id(),
                );
            }
            spin_loop();
        }
    }

    #[track_caller]
    pub(crate) fn try_lock(&self) -> Option<KernelWaitGuard<'_, T, CLASS>> {
        let acquire_site = Location::caller();
        let raw_context = nucleus_core::util::lockdep::irq_context_depth() != 0
            || nucleus_core::util::lockdep::held_spin_lock_depth() != 0;
        if raw_context {
            let acquire_tsc = self
                .state
                .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .ok()
                .map(|_| read_tsc())?;
            let external_raw = nucleus_core::util::lockdep::record_external_raw_lock(CLASS);
            self.record_owner_metadata(0, acquire_site, acquire_tsc);
            return Some(KernelWaitGuard {
                lock: self,
                owner: 0,
                external_raw: Some(external_raw),
                acquire_site,
                acquire_tsc,
            });
        }
        assert_sleepable_lock_context(acquire_site);
        let owner = current_lock_owner_token();
        self.try_acquire_at(owner, Location::caller())
            .map(|acquire_tsc| KernelWaitGuard {
                lock: self,
                owner,
                external_raw: None,
                acquire_site,
                acquire_tsc,
            })
    }

    fn try_acquire_at(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
    ) -> Option<u64> {
        let acquired = self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            Some(self.record_acquire(owner, acquire_site))
        } else {
            None
        }
    }

    fn block_until_woken_or_acquired(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
    ) -> Option<u64> {
        let task_id = crate::multitask::current_task_id()?;

        let mut acquired_tsc = None;
        let mut waiter_published = false;
        interrupts::without_interrupts(|| {
            // Arm the scheduler state before publishing the waiter. Another
            // CPU may unlock at any point below; wake_task() must then cancel
            // the armed block instead of observing a still-running task and
            // letting us block after the only wake was consumed.
            if !crate::multitask::arm_block_current_task() {
                return;
            }
            {
                let mut waiters = self.waiters.lock();
                if !waiters.push_unique(task_id) {
                    let _ = crate::multitask::cancel_block_current_task();
                    panic!(
                        "KernelWaitLock waiter queue full: type={} task_id={} wait_at={}:{}",
                        core::any::type_name::<T>(),
                        task_id,
                        acquire_site.file(),
                        acquire_site.line()
                    );
                }
            }

            if let Some(tsc) = self.try_acquire_at(owner, acquire_site) {
                self.waiters.lock().remove(task_id);
                let _ = crate::multitask::cancel_block_current_task();
                acquired_tsc = Some(tsc);
                return;
            }
            waiter_published = true;
        });

        if acquired_tsc.is_some() {
            return acquired_tsc;
        }
        if waiter_published {
            let _ = crate::multitask::commit_block_current_task_and_yield();
            // A racing unlock, unrelated wake, or resumed lock waiter may have
            // consumed this entry already, so removal is idempotent.
            // A task can also be woken for a reason unrelated to this lock.
            // Do not leave that wake as a stale queue entry which later steals
            // an unlock from a live waiter.
            interrupts::without_interrupts(|| self.waiters.lock().remove(task_id));
        }
        None
    }

    fn unlock(&self) {
        self.clear_owner();
        self.state.store(UNLOCKED, Ordering::Release);
    }

    fn wake_waiter(&self) {
        wake_first_live_waiter(&self.waiters, crate::multitask::wake_task);
    }

    fn record_acquire(&self, owner: usize, acquire_site: &'static Location<'static>) -> u64 {
        nucleus_core::util::lockdep::record_sleepable_acquire(owner as u64, CLASS);
        let acquire_tsc = read_tsc();
        self.record_owner_metadata(owner, acquire_site, acquire_tsc);
        acquire_tsc
    }

    fn record_owner_metadata(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
        acquire_tsc: u64,
    ) {
        let file = acquire_site.file();
        self.owner_acquire_file_len
            .store(file.len(), Ordering::Release);
        self.owner_acquire_line
            .store(acquire_site.line() as usize, Ordering::Release);
        self.owner_acquire_file_ptr
            .store(file.as_ptr() as usize, Ordering::Release);
        self.owner_acquire_tsc.store(acquire_tsc, Ordering::Release);
        self.owner_depth.store(1, Ordering::Release);
        self.owner.store(owner, Ordering::Release);
    }

    fn clear_owner(&self) {
        self.owner.store(0, Ordering::Release);
        self.owner_depth.store(0, Ordering::Release);
        self.owner_acquire_file_ptr.store(0, Ordering::Release);
        self.owner_acquire_file_len.store(0, Ordering::Release);
        self.owner_acquire_line.store(0, Ordering::Release);
        self.owner_acquire_tsc.store(0, Ordering::Release);
    }

    fn assert_not_recursive(&self, owner: usize, acquire_site: &'static Location<'static>) {
        if self.owner.load(Ordering::Acquire) != owner {
            return;
        }
        let (owner_file, owner_line) = self.owner_acquire_site();
        panic!(
            "KernelWaitLock recursive acquire: type={} owner={} depth={} owner_acquire={}:{} wait_at={}:{} irq_enabled={} scheduler_initialized={} current_task={:?}",
            core::any::type_name::<T>(),
            owner,
            self.owner_depth.load(Ordering::Acquire),
            owner_file,
            owner_line,
            acquire_site.file(),
            acquire_site.line(),
            interrupts::are_enabled(),
            crate::multitask::is_initialized(),
            crate::multitask::current_task_id(),
        );
    }

    fn owner_acquire_site(&self) -> (&'static str, usize) {
        let ptr = self.owner_acquire_file_ptr.load(Ordering::Acquire);
        let len = self.owner_acquire_file_len.load(Ordering::Acquire);
        let line = self.owner_acquire_line.load(Ordering::Acquire);
        if ptr == 0 || len == 0 {
            return ("<unknown>", line);
        }
        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr as *const u8, len))
        };
        (file, line)
    }
}

fn assert_sleepable_lock_context(acquire_site: &'static Location<'static>) {
    assert_eq!(
        nucleus_core::util::lockdep::irq_context_depth(),
        0,
        "KernelWaitLock acquired from IRQ context at {}:{}",
        acquire_site.file(),
        acquire_site.line()
    );
    assert_eq!(
        nucleus_core::util::lockdep::held_spin_lock_depth(),
        0,
        "KernelWaitLock acquired while a tracked raw-spin lock is held at {}:{} class={:?}",
        acquire_site.file(),
        acquire_site.line(),
        nucleus_core::util::lockdep::current_lock_class(),
    );
}

fn wake_first_live_waiter(
    waiters: &RawSpinMutex<WaitQueue>,
    mut wake: impl FnMut(u64) -> bool,
) -> usize {
    let mut stale = 0usize;
    loop {
        let Some(task_id) = waiters.lock().pop_front() else {
            return stale;
        };
        if wake(task_id) {
            return stale;
        }
        stale += 1;
    }
}

impl<T: ?Sized, const CLASS: u8> Deref for KernelWaitGuard<'_, T, CLASS> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized, const CLASS: u8> DerefMut for KernelWaitGuard<'_, T, CLASS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized, const CLASS: u8> Drop for KernelWaitGuard<'_, T, CLASS> {
    fn drop(&mut self) {
        self.lock.unlock();
        if let Some(external_raw) = self.external_raw.take() {
            drop(external_raw);
        } else {
            nucleus_core::util::lockdep::release_sleepable_lock(self.owner as u64, CLASS);
        }
        self.lock.wake_waiter();
        maybe_report_lock_hold(
            "KernelWaitLock",
            core::any::type_name::<T>(),
            self.acquire_site,
            self.acquire_tsc,
        );
    }
}

fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

#[cfg(rustos_lock_telemetry_enabled)]
fn maybe_report_lock_wait(
    kind: &str,
    type_name: &str,
    acquire_site: &'static Location<'static>,
    wait_start_tsc: u64,
    acquire_tsc: u64,
    spins: usize,
) {
    let wait_cycles = acquire_tsc.saturating_sub(wait_start_tsc);
    let threshold = lock_telemetry_warn_wait_cycles();
    if spins == 0 || wait_cycles < threshold {
        return;
    }
    crate::debug::println_fmt(format_args!(
        "lock-telemetry: wait kind={} type={} wait_cycles={} threshold={} spins={} wait_at={}:{} irq_enabled={} task={:?}",
        kind,
        type_name,
        wait_cycles,
        threshold,
        spins,
        acquire_site.file(),
        acquire_site.line(),
        interrupts::are_enabled(),
        crate::multitask::current_task_id(),
    ));
}

#[cfg(not(rustos_lock_telemetry_enabled))]
fn maybe_report_lock_wait(
    _kind: &str,
    _type_name: &str,
    _acquire_site: &'static Location<'static>,
    _wait_start_tsc: u64,
    _acquire_tsc: u64,
    _spins: usize,
) {
}

#[cfg(rustos_lock_telemetry_enabled)]
fn maybe_report_lock_hold(
    kind: &str,
    type_name: &str,
    acquire_site: &'static Location<'static>,
    acquire_tsc: u64,
) {
    let hold_cycles = read_tsc().saturating_sub(acquire_tsc);
    let threshold = lock_telemetry_warn_hold_cycles();
    if hold_cycles < threshold {
        return;
    }
    crate::debug::println_fmt(format_args!(
        "lock-telemetry: hold kind={} type={} hold_cycles={} threshold={} held_from={}:{} irq_enabled={} task={:?}",
        kind,
        type_name,
        hold_cycles,
        threshold,
        acquire_site.file(),
        acquire_site.line(),
        interrupts::are_enabled(),
        crate::multitask::current_task_id(),
    ));
}

#[cfg(not(rustos_lock_telemetry_enabled))]
fn maybe_report_lock_hold(
    _kind: &str,
    _type_name: &str,
    _acquire_site: &'static Location<'static>,
    _acquire_tsc: u64,
) {
}

#[cfg(rustos_lock_telemetry_enabled)]
fn lock_telemetry_warn_wait_cycles() -> u64 {
    option_env!("RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES")
        .and_then(parse_u64)
        .unwrap_or(250_000)
}

#[cfg(rustos_lock_telemetry_enabled)]
fn lock_telemetry_warn_hold_cycles() -> u64 {
    option_env!("RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES")
        .and_then(parse_u64)
        .unwrap_or(250_000)
}

#[cfg(rustos_lock_telemetry_enabled)]
fn parse_u64(value: &str) -> Option<u64> {
    let mut parsed = 0u64;
    for byte in value.as_bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        parsed = parsed
            .saturating_mul(10)
            .saturating_add(u64::from(*byte - b'0'));
    }
    Some(parsed)
}

fn can_block_current_task() -> bool {
    #[cfg(test)]
    {
        false
    }

    #[cfg(not(test))]
    {
        interrupts::are_enabled()
            && crate::multitask::is_initialized()
            && crate::multitask::current_task_id().is_some()
    }
}

fn current_lock_owner_token() -> usize {
    #[cfg(test)]
    {
        TEST_LOCK_OWNER.with(|owner| *owner)
    }

    #[cfg(not(test))]
    {
        crate::multitask::current_task_id()
            .map(|task_id| task_id.saturating_add(1) as usize)
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{RawSpinMutex, WaitQueue, wake_first_live_waiter};

    #[test]
    fn wait_lock_unlock_skips_retired_front_waiters() {
        let waiters = RawSpinMutex::new(WaitQueue::new());
        assert!(waiters.lock().push_unique(11));
        assert!(waiters.lock().push_unique(12));
        assert!(waiters.lock().push_unique(13));

        let mut attempted = [0_u64; 3];
        let mut count = 0usize;
        let stale = wake_first_live_waiter(&waiters, |task_id| {
            attempted[count] = task_id;
            count += 1;
            task_id == 13
        });

        assert_eq!(stale, 2);
        assert_eq!(&attempted[..count], &[11, 12, 13]);
        assert_eq!(waiters.lock().len, 0);
    }

    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "the mutation witness must compile a reduced capacity and fail at runtime"
    )]
    fn wait_lock_capacity_covers_every_scheduler_task() {
        assert!(super::MAX_WAITERS >= crate::multitask::MAX_SCHEDULER_TASKS);
    }
}
