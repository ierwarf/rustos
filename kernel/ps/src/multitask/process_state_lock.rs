//! Task-owned serialization for one process's mutable address-space state.
//!
//! Process state operations may clone page tables, allocate handle snapshots,
//! or fault in bookkeeping. They are therefore forbidden from using a raw
//! spin lock: a descheduled owner would pin a waiter CPU and make a fixed spin
//! count depend on hypervisor scheduling. This lock spins only for a short
//! handoff window and then parks a schedulable task on a bounded wait queue.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::Mutex as RawSpinMutex;
use x86_64::instructions::interrupts;

use nucleus_core::util::lockdep::{self, LockClass};

const UNLOCKED: usize = 0;
const LOCKED: usize = 1;
const PRE_BLOCK_SPINS: usize = 256;
const IRQ_OFF_SPIN_LIMIT: usize = 100_000;
const MAX_WAITERS: usize = super::MAX_SCHEDULER_TASKS;

pub(super) struct ProcessStateLock<T: ?Sized> {
    state: AtomicUsize,
    owner: AtomicUsize,
    owner_acquire_file_ptr: AtomicUsize,
    owner_acquire_file_len: AtomicUsize,
    owner_acquire_line: AtomicUsize,
    waiters: RawSpinMutex<WaitQueue>,
    value: UnsafeCell<T>,
}

pub(super) struct ProcessStateGuard<'a, T: ?Sized> {
    lock: &'a ProcessStateLock<T>,
    owner: usize,
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

// SAFETY: the owner word and scheduler wake protocol provide exclusive access
// to T; the Send bound permits that exclusive ownership to move across CPUs.
unsafe impl<T: ?Sized + Send> Send for ProcessStateLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for ProcessStateLock<T> {}

impl<T> ProcessStateLock<T> {
    pub(super) const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(UNLOCKED),
            owner: AtomicUsize::new(0),
            owner_acquire_file_ptr: AtomicUsize::new(0),
            owner_acquire_file_len: AtomicUsize::new(0),
            owner_acquire_line: AtomicUsize::new(0),
            waiters: RawSpinMutex::new(WaitQueue::new()),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> ProcessStateLock<T> {
    #[track_caller]
    pub(super) fn lock(&self) -> ProcessStateGuard<'_, T> {
        let acquire_site = Location::caller();
        assert_process_state_wait_context(acquire_site);
        let owner = current_owner_token();
        self.assert_not_recursive(owner, acquire_site);
        let mut spins = 0usize;

        loop {
            if self.try_acquire(owner, acquire_site) {
                return ProcessStateGuard { lock: self, owner };
            }

            spins = spins.saturating_add(1);
            if can_block_current_task() && spins >= PRE_BLOCK_SPINS {
                if self.block_until_woken_or_acquired(owner, acquire_site) {
                    return ProcessStateGuard { lock: self, owner };
                }
                spins = 0;
                continue;
            }

            if !interrupts::are_enabled() && spins >= IRQ_OFF_SPIN_LIMIT {
                let (owner_file, owner_line) = self.owner_acquire_site();
                panic!(
                    "ProcessStateLock contention cannot block: waiter={} current_owner={} owner_acquire={}:{} wait_at={}:{} spins={} irq_enabled={} current_task={:?}",
                    owner,
                    self.owner.load(Ordering::Acquire),
                    owner_file,
                    owner_line,
                    acquire_site.file(),
                    acquire_site.line(),
                    spins,
                    interrupts::are_enabled(),
                    super::current_task_id(),
                );
            }
            spin_loop();
        }
    }

    #[track_caller]
    pub(super) fn try_lock(&self) -> Option<ProcessStateGuard<'_, T>> {
        let acquire_site = Location::caller();
        assert_process_state_wait_context(acquire_site);
        let owner = current_owner_token();
        if self.try_acquire(owner, acquire_site) {
            Some(ProcessStateGuard { lock: self, owner })
        } else {
            None
        }
    }

    fn try_acquire(&self, owner: usize, acquire_site: &'static Location<'static>) -> bool {
        if self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        lockdep::record_sleepable_acquire(owner as u64, LockClass::ProcessState as u8);
        self.record_owner(owner, acquire_site);
        true
    }

    fn block_until_woken_or_acquired(
        &self,
        owner: usize,
        acquire_site: &'static Location<'static>,
    ) -> bool {
        let Some(task_id) = super::current_task_id() else {
            return false;
        };
        let mut acquired = false;
        let mut waiter_published = false;

        interrupts::without_interrupts(|| {
            // Arm before publishing. An unlock racing with publication can
            // then cancel the armed block instead of losing the sole wake.
            if !super::arm_block_current_task() {
                return;
            }
            {
                let mut waiters = self.waiters.lock();
                if !waiters.push_unique(task_id) {
                    let _ = super::cancel_block_current_task();
                    panic!(
                        "ProcessStateLock waiter queue full task_id={} wait_at={}:{}",
                        task_id,
                        acquire_site.file(),
                        acquire_site.line()
                    );
                }
            }
            if self.try_acquire(owner, acquire_site) {
                self.waiters.lock().remove(task_id);
                let _ = super::cancel_block_current_task();
                acquired = true;
                return;
            }
            waiter_published = true;
        });

        if acquired {
            return true;
        }
        if waiter_published {
            let _ = super::commit_block_current_task_and_yield();
            // Unrelated wakes and retired waiters make removal necessarily
            // idempotent. Never leave a stale entry to steal a later unlock.
            interrupts::without_interrupts(|| self.waiters.lock().remove(task_id));
        }
        false
    }

    fn unlock(&self, owner: usize) {
        assert_eq!(
            self.owner.load(Ordering::Acquire),
            owner,
            "ProcessStateLock released by foreign task"
        );
        self.clear_owner();
        self.state.store(UNLOCKED, Ordering::Release);
        lockdep::release_sleepable_lock(owner as u64, LockClass::ProcessState as u8);
        wake_first_live_waiter(&self.waiters);
    }

    fn record_owner(&self, owner: usize, acquire_site: &'static Location<'static>) {
        let file = acquire_site.file();
        self.owner_acquire_file_len
            .store(file.len(), Ordering::Relaxed);
        self.owner_acquire_line
            .store(acquire_site.line() as usize, Ordering::Relaxed);
        self.owner_acquire_file_ptr
            .store(file.as_ptr() as usize, Ordering::Relaxed);
        // ORDERING: Release publishes all acquisition metadata after the lock
        // Acquire and before a contending waiter diagnoses its owner.
        self.owner.store(owner, Ordering::Release);
    }

    fn clear_owner(&self) {
        // ORDERING: Release clears diagnostic ownership before the protected
        // value is made available by the lock-state Release.
        self.owner.store(0, Ordering::Release);
        self.owner_acquire_file_ptr.store(0, Ordering::Relaxed);
        self.owner_acquire_file_len.store(0, Ordering::Relaxed);
        self.owner_acquire_line.store(0, Ordering::Relaxed);
    }

    fn assert_not_recursive(&self, owner: usize, acquire_site: &'static Location<'static>) {
        if self.owner.load(Ordering::Acquire) != owner {
            return;
        }
        let (owner_file, owner_line) = self.owner_acquire_site();
        panic!(
            "ProcessStateLock recursive acquire owner={} owner_acquire={}:{} wait_at={}:{}",
            owner,
            owner_file,
            owner_line,
            acquire_site.file(),
            acquire_site.line(),
        );
    }

    fn owner_acquire_site(&self) -> (&'static str, usize) {
        // ORDERING: The owner Acquire above admits the relaxed metadata fields
        // published before its Release store.
        let ptr = self.owner_acquire_file_ptr.load(Ordering::Relaxed);
        let len = self.owner_acquire_file_len.load(Ordering::Relaxed);
        let line = self.owner_acquire_line.load(Ordering::Relaxed);
        if ptr == 0 || len == 0 {
            return ("<unknown>", line);
        }
        let file = unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr as *const u8, len))
        };
        (file, line)
    }
}

impl<T: ?Sized> Deref for ProcessStateGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for ProcessStateGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for ProcessStateGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock(self.owner);
    }
}

fn assert_process_state_wait_context(acquire_site: &'static Location<'static>) {
    assert_eq!(
        lockdep::irq_context_depth(),
        0,
        "ProcessStateLock acquired from IRQ context at {}:{}",
        acquire_site.file(),
        acquire_site.line()
    );
    assert_eq!(
        lockdep::held_spin_lock_depth(),
        0,
        "ProcessStateLock acquired while raw-spin class held at {}:{} class={:?}",
        acquire_site.file(),
        acquire_site.line(),
        lockdep::current_lock_class(),
    );
}

fn wake_first_live_waiter(waiters: &RawSpinMutex<WaitQueue>) {
    loop {
        let Some(task_id) = waiters.lock().pop_front() else {
            return;
        };
        if super::wake_task(task_id) {
            return;
        }
    }
}

fn can_block_current_task() -> bool {
    #[cfg(test)]
    {
        false
    }
    #[cfg(not(test))]
    {
        interrupts::are_enabled() && super::is_initialized() && super::current_task_id().is_some()
    }
}

fn current_owner_token() -> usize {
    #[cfg(test)]
    {
        1
    }
    #[cfg(not(test))]
    {
        super::current_task_id()
            .map(|task_id| task_id.saturating_add(1) as usize)
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{RawSpinMutex, WaitQueue};

    #[test]
    fn waiter_queue_removal_is_idempotent_and_ordered() {
        let waiters = RawSpinMutex::new(WaitQueue::new());
        assert!(waiters.lock().push_unique(11));
        assert!(waiters.lock().push_unique(12));
        assert!(waiters.lock().push_unique(12));
        waiters.lock().remove(99);
        assert_eq!(waiters.lock().pop_front(), Some(11));
        assert_eq!(waiters.lock().pop_front(), Some(12));
        assert_eq!(waiters.lock().pop_front(), None);
    }

    #[test]
    fn waiter_capacity_covers_scheduler_tasks() {
        assert!(super::MAX_WAITERS >= super::super::MAX_SCHEDULER_TASKS);
    }
}
