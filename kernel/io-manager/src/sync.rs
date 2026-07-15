use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::{Mutex as RawSpinMutex, MutexGuard as RawSpinMutexGuard};
use x86_64::instructions::interrupts;

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;
const PRE_BLOCK_SPINS: usize = 256;
const IRQ_OFF_SPIN_LIMIT: usize = 100_000;
const SCHED_SPIN_LIMIT: usize = 1_000_000;
const MAX_WAITERS: usize = 64;
const KERNEL_SPIN_LOCK_IRQ_OFF_SPIN_LIMIT: usize = 100_000;
const KERNEL_SPIN_LOCK_SCHED_SPIN_LIMIT: usize = 1_000_000;

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

pub(crate) struct KernelSpinLock<T: ?Sized> {
    owner: AtomicUsize,
    owner_depth: AtomicUsize,
    owner_acquire_file_ptr: AtomicUsize,
    owner_acquire_file_len: AtomicUsize,
    owner_acquire_line: AtomicUsize,
    owner_acquire_tsc: AtomicU64,
    inner: RawSpinMutex<T>,
}

pub(crate) struct KernelSpinGuard<'a, T: ?Sized> {
    lock: &'a KernelSpinLock<T>,
    guard: RawSpinMutexGuard<'a, T>,
    acquire_site: &'static Location<'static>,
    acquire_tsc: u64,
    _not_send: PhantomData<*mut ()>,
}

pub(crate) struct KernelWaitLock<T: ?Sized> {
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

pub(crate) struct KernelWaitGuard<'a, T: ?Sized> {
    lock: &'a KernelWaitLock<T>,
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

unsafe impl<T: ?Sized + Send> Send for KernelWaitLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for KernelWaitLock<T> {}

impl<T> KernelWaitLock<T> {
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

unsafe impl<T: ?Sized + Send> Send for KernelSpinLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for KernelSpinLock<T> {}

impl<T> KernelSpinLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            owner: AtomicUsize::new(0),
            owner_depth: AtomicUsize::new(0),
            owner_acquire_file_ptr: AtomicUsize::new(0),
            owner_acquire_file_len: AtomicUsize::new(0),
            owner_acquire_line: AtomicUsize::new(0),
            owner_acquire_tsc: AtomicU64::new(0),
            inner: RawSpinMutex::new(value),
        }
    }
}

impl<T: ?Sized> KernelSpinLock<T> {
    #[track_caller]
    pub(crate) fn lock(&self) -> KernelSpinGuard<'_, T> {
        let acquire_site = Location::caller();
        let owner = current_lock_owner_token();
        let wait_start_tsc = read_tsc();
        self.assert_not_recursive(owner, acquire_site);

        if let Some(guard) = self.try_acquire_at(owner, acquire_site) {
            maybe_report_lock_wait(
                "KernelSpinLock",
                core::any::type_name::<T>(),
                acquire_site,
                wait_start_tsc,
                guard.acquire_tsc,
                0,
            );
            return guard;
        }

        let mut spins = 0usize;
        loop {
            self.assert_not_recursive(owner, acquire_site);
            spins = spins.saturating_add(1);
            if spins >= self.spin_limit() {
                self.panic_contention_timeout(owner, spins, acquire_site);
            }
            spin_loop();
            if let Some(guard) = self.try_acquire_at(owner, acquire_site) {
                maybe_report_lock_wait(
                    "KernelSpinLock",
                    core::any::type_name::<T>(),
                    acquire_site,
                    wait_start_tsc,
                    guard.acquire_tsc,
                    spins,
                );
                return guard;
            }
        }
    }

    fn try_acquire_at(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
    ) -> Option<KernelSpinGuard<'_, T>> {
        let guard = self.inner.try_lock()?;
        let acquire_tsc = self.record_acquire(owner, acquire_site);
        Some(KernelSpinGuard {
            lock: self,
            guard,
            acquire_site,
            acquire_tsc,
            _not_send: PhantomData,
        })
    }

    fn record_acquire(&self, owner: usize, acquire_site: &'static Location<'static>) -> u64 {
        let acquire_tsc = read_tsc();
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
        acquire_tsc
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
            "KernelSpinLock recursive acquire: type={} owner={} depth={} owner_acquire={}:{} wait_at={}:{} irq_enabled={} scheduler_initialized={} current_task={:?}",
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

    fn panic_contention_timeout(
        &self,
        waiter: usize,
        spins: usize,
        acquire_site: &'static Location<'static>,
    ) -> ! {
        let (owner_file, owner_line) = self.owner_acquire_site();
        panic!(
            "KernelSpinLock contention exceeded bounded spin limit: type={} waiter={} current_owner={} owner_depth={} owner_acquire={}:{} wait_at={}:{} spins={} limit={} irq_enabled={} scheduler_initialized={} current_task={:?}",
            core::any::type_name::<T>(),
            waiter,
            self.owner.load(Ordering::Acquire),
            self.owner_depth.load(Ordering::Acquire),
            owner_file,
            owner_line,
            acquire_site.file(),
            acquire_site.line(),
            spins,
            self.spin_limit(),
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

    fn spin_limit(&self) -> usize {
        if interrupts::are_enabled() {
            KERNEL_SPIN_LOCK_SCHED_SPIN_LIMIT
        } else {
            KERNEL_SPIN_LOCK_IRQ_OFF_SPIN_LIMIT
        }
    }
}

impl<T: ?Sized> Deref for KernelSpinGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for KernelSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<T: ?Sized> Drop for KernelSpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.clear_owner();
        maybe_report_lock_hold(
            "KernelSpinLock",
            core::any::type_name::<T>(),
            self.acquire_site,
            self.acquire_tsc,
        );
    }
}

impl<T: ?Sized> KernelWaitLock<T> {
    #[track_caller]
    pub(crate) fn lock(&self) -> KernelWaitGuard<'_, T> {
        let acquire_site = Location::caller();
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
    pub(crate) fn try_lock(&self) -> Option<KernelWaitGuard<'_, T>> {
        let acquire_site = Location::caller();
        self.try_acquire_at(current_lock_owner_token(), Location::caller())
            .map(|acquire_tsc| KernelWaitGuard {
                lock: self,
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
        let Some(task_id) = crate::multitask::current_task_id() else {
            return None;
        };

        let mut acquired_tsc = None;
        let mut blocked = false;
        interrupts::without_interrupts(|| {
            {
                let mut waiters = self.waiters.lock();
                if !waiters.push_unique(task_id) {
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
                acquired_tsc = Some(tsc);
                return;
            }

            blocked = crate::multitask::block_current_task();
            if !blocked {
                self.waiters.lock().remove(task_id);
            }
        });

        if acquired_tsc.is_some() {
            return acquired_tsc;
        }
        if blocked {
            crate::multitask::yield_now();
        }
        None
    }

    fn unlock(&self) {
        self.clear_owner();
        self.state.store(UNLOCKED, Ordering::Release);
        if let Some(task_id) = self.waiters.lock().pop_front() {
            let _ = crate::multitask::wake_task(task_id);
        }
    }

    fn record_acquire(&self, owner: usize, acquire_site: &'static Location<'static>) -> u64 {
        let acquire_tsc = read_tsc();
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
        acquire_tsc
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

impl<T: ?Sized> Deref for KernelWaitGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for KernelWaitGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for KernelWaitGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
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
