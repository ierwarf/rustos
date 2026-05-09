use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;
const PRE_BLOCK_SPINS: usize = 256;
const IRQ_OFF_SPIN_LIMIT: usize = 100_000;
const SCHED_SPIN_LIMIT: usize = 1_000_000;
const MAX_WAITERS: usize = 64;

pub(crate) struct KernelWaitLock<T: ?Sized> {
    state: AtomicUsize,
    waiters: Mutex<WaitQueue>,
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
            waiters: Mutex::new(WaitQueue::new()),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> KernelWaitLock<T> {
    pub(crate) fn lock(&self) -> KernelWaitGuard<'_, T> {
        let mut spins = 0usize;
        loop {
            if self.try_acquire() {
                return KernelWaitGuard { lock: self };
            }

            spins = spins.saturating_add(1);
            if can_block_current_task() && spins >= PRE_BLOCK_SPINS {
                if self.block_until_woken_or_acquired() {
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
                panic!(
                    "KernelWaitLock contention exceeded bounded spin limit: irq_enabled={} scheduler_initialized={} current_task={:?}",
                    interrupts::are_enabled(),
                    crate::multitask::is_initialized(),
                    crate::multitask::current_task_id(),
                );
            }
            spin_loop();
        }
    }

    pub(crate) fn try_lock(&self) -> Option<KernelWaitGuard<'_, T>> {
        self.try_acquire().then_some(KernelWaitGuard { lock: self })
    }

    fn try_acquire(&self) -> bool {
        self.state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn block_until_woken_or_acquired(&self) -> bool {
        let Some(task_id) = crate::multitask::current_task_id() else {
            return false;
        };

        let mut acquired = false;
        let mut blocked = false;
        interrupts::without_interrupts(|| {
            {
                let mut waiters = self.waiters.lock();
                if !waiters.push_unique(task_id) {
                    panic!("KernelWaitLock waiter queue full: task_id={}", task_id);
                }
            }

            if self.try_acquire() {
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
        self.state.store(UNLOCKED, Ordering::Release);
        if let Some(task_id) = self.waiters.lock().pop_front() {
            let _ = crate::multitask::wake_task(task_id);
        }
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
    interrupts::are_enabled()
        && crate::multitask::is_initialized()
        && crate::multitask::current_task_id().is_some()
}
