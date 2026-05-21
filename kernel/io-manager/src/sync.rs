use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicUsize, Ordering};

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

pub(crate) struct KernelSpinLock<T: ?Sized> {
    owner: AtomicUsize,
    owner_depth: AtomicUsize,
    owner_acquire_file_ptr: AtomicUsize,
    owner_acquire_file_len: AtomicUsize,
    owner_acquire_line: AtomicUsize,
    inner: RawSpinMutex<T>,
}

pub(crate) struct KernelSpinGuard<'a, T: ?Sized> {
    lock: &'a KernelSpinLock<T>,
    guard: RawSpinMutexGuard<'a, T>,
    _not_send: PhantomData<*mut ()>,
}

pub(crate) struct KernelWaitLock<T: ?Sized> {
    state: AtomicUsize,
    owner: AtomicUsize,
    owner_depth: AtomicUsize,
    owner_acquire_file_ptr: AtomicUsize,
    owner_acquire_file_len: AtomicUsize,
    owner_acquire_line: AtomicUsize,
    waiters: RawSpinMutex<WaitQueue>,
    value: UnsafeCell<T>,
}

pub(crate) struct KernelWaitGuard<'a, T: ?Sized> {
    lock: &'a KernelWaitLock<T>,
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
            inner: RawSpinMutex::new(value),
        }
    }
}

impl<T: ?Sized> KernelSpinLock<T> {
    #[track_caller]
    pub(crate) fn lock(&self) -> KernelSpinGuard<'_, T> {
        let acquire_site = Location::caller();
        let owner = current_lock_owner_token();
        self.assert_not_recursive(owner, acquire_site);

        if let Some(guard) = self.try_acquire_at(owner, acquire_site) {
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
                return guard;
            }
        }
    }

    #[track_caller]
    pub(crate) fn try_lock(&self) -> Option<KernelSpinGuard<'_, T>> {
        self.try_acquire_at(current_lock_owner_token(), Location::caller())
    }

    fn try_acquire_at(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
    ) -> Option<KernelSpinGuard<'_, T>> {
        let guard = self.inner.try_lock()?;
        self.record_acquire(owner, acquire_site);
        Some(KernelSpinGuard {
            lock: self,
            guard,
            _not_send: PhantomData,
        })
    }

    fn record_acquire(&self, owner: usize, acquire_site: &'static Location<'static>) {
        let file = acquire_site.file();
        self.owner_acquire_file_len
            .store(file.len(), Ordering::Release);
        self.owner_acquire_line
            .store(acquire_site.line() as usize, Ordering::Release);
        self.owner_acquire_file_ptr
            .store(file.as_ptr() as usize, Ordering::Release);
        self.owner_depth.store(1, Ordering::Release);
        self.owner.store(owner, Ordering::Release);
    }

    fn clear_owner(&self) {
        self.owner.store(0, Ordering::Release);
        self.owner_depth.store(0, Ordering::Release);
        self.owner_acquire_file_ptr.store(0, Ordering::Release);
        self.owner_acquire_file_len.store(0, Ordering::Release);
        self.owner_acquire_line.store(0, Ordering::Release);
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
    }
}

impl<T: ?Sized> KernelWaitLock<T> {
    #[track_caller]
    pub(crate) fn lock(&self) -> KernelWaitGuard<'_, T> {
        let acquire_site = Location::caller();
        let owner = current_lock_owner_token();
        self.assert_not_recursive(owner, acquire_site);
        let mut spins = 0usize;
        loop {
            if self.try_acquire_at(owner, acquire_site) {
                return KernelWaitGuard { lock: self };
            }

            spins = spins.saturating_add(1);
            if can_block_current_task() && spins >= PRE_BLOCK_SPINS {
                if self.block_until_woken_or_acquired(owner, acquire_site) {
                    return KernelWaitGuard { lock: self };
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
        self.try_acquire_at(current_lock_owner_token(), Location::caller())
            .then_some(KernelWaitGuard { lock: self })
    }

    fn try_acquire_at(&self, owner: usize, acquire_site: &'static Location<'static>) -> bool {
        let acquired = self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            self.record_acquire(owner, acquire_site);
        }
        acquired
    }

    fn block_until_woken_or_acquired(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
    ) -> bool {
        let Some(task_id) = crate::multitask::current_task_id() else {
            return false;
        };

        let mut acquired = false;
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

            if self.try_acquire_at(owner, acquire_site) {
                self.waiters.lock().remove(task_id);
                acquired = true;
                return;
            }

            blocked = crate::multitask::block_current_task();
            if !blocked {
                self.waiters.lock().remove(task_id);
            }
        });

        if acquired {
            return true;
        }
        if blocked {
            crate::multitask::yield_now();
        }
        false
    }

    fn unlock(&self) {
        self.clear_owner();
        self.state.store(UNLOCKED, Ordering::Release);
        if let Some(task_id) = self.waiters.lock().pop_front() {
            let _ = crate::multitask::wake_task(task_id);
        }
    }

    fn record_acquire(&self, owner: usize, acquire_site: &'static Location<'static>) {
        let file = acquire_site.file();
        self.owner_acquire_file_len
            .store(file.len(), Ordering::Release);
        self.owner_acquire_line
            .store(acquire_site.line() as usize, Ordering::Release);
        self.owner_acquire_file_ptr
            .store(file.as_ptr() as usize, Ordering::Release);
        self.owner_depth.store(1, Ordering::Release);
        self.owner.store(owner, Ordering::Release);
    }

    fn clear_owner(&self) {
        self.owner.store(0, Ordering::Release);
        self.owner_depth.store(0, Ordering::Release);
        self.owner_acquire_file_ptr.store(0, Ordering::Release);
        self.owner_acquire_file_len.store(0, Ordering::Release);
        self.owner_acquire_line.store(0, Ordering::Release);
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
    }
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
        1
    }

    #[cfg(not(test))]
    {
        crate::multitask::current_task_id()
            .map(|task_id| task_id.saturating_add(1) as usize)
            .unwrap_or(1)
    }
}
