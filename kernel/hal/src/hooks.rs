use nucleus_core::user_abi::UserAbi;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, Ordering, fence};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserFaultDisposition {
    Resumed,
    Retired,
    Unhandled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurrentUserSnapshot {
    pub abi: UserAbi,
    pub thread_id: u64,
    pub process_id: u64,
    pub console_session_raw: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputEventQueueDebugSnapshot {
    pub pointer_packet_submits: u64,
    pub read_calls: u64,
    pub read_events: u64,
    pub lock_active: u64,
    pub lock_last_seq: u64,
    pub queued: usize,
    pub pending_coalesced: bool,
    pub pending_pointer_position: bool,
    pub dropped_discrete: u64,
    pub dropped_lossy: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HeartbeatSnapshot {
    pub userspace_display_active: bool,
    pub input: InputEventQueueDebugSnapshot,
    pub linux_irq_owner_count: usize,
    pub linux_irq_total_depth: u64,
    pub linux_input_lock_active: bool,
    pub linux_input_lock_last_seq: u64,
}

pub type RetireCurrentUserTaskDueToFaultHook =
    fn(u8, Option<u64>, u64, u64, u64) -> UserFaultDisposition;

/// Attempts the sole pageable user-fault admission path before generic fault
/// retirement. The hook receives only the hardware page-fault record; it must
/// fail closed with `Unhandled` when no exact, allocation-free pager custody
/// can be established.
pub type TryHandleCurrentUserPageFaultHook = fn(Option<u64>, u64, u64, u64) -> UserFaultDisposition;

#[derive(Clone, Copy, Default)]
pub struct TaskHooks {
    pub try_handle_current_user_page_fault: Option<TryHandleCurrentUserPageFaultHook>,
    pub retire_current_user_task_due_to_fault: Option<RetireCurrentUserTaskDueToFaultHook>,
    pub halt_current_retired_task: Option<fn() -> !>,
    pub current_user_snapshot: Option<fn() -> Option<CurrentUserSnapshot>>,
    pub is_scheduler_initialized: Option<fn() -> bool>,
    pub current_task_id: Option<fn() -> Option<u64>>,
    pub arm_block_current_task: Option<fn() -> bool>,
    pub cancel_block_current_task: Option<fn() -> bool>,
    pub commit_block_current_task_and_yield: Option<fn() -> Option<bool>>,
    /// Deadline waiters are exact scheduler tasks, including kernel consumers.
    pub wake_task: Option<fn(u64) -> bool>,
    pub yield_now: Option<fn()>,
}

#[derive(Clone, Copy, Default)]
pub struct InterruptHooks {
    pub dispatch_pic_irq: Option<fn(u8) -> bool>,
}

#[derive(Clone, Copy, Default)]
pub struct HeartbeatHooks {
    pub snapshot: Option<fn() -> HeartbeatSnapshot>,
}

#[derive(Clone, Copy, Default)]
struct HookRegistry {
    task: TaskHooks,
    interrupt: InterruptHooks,
    heartbeat: HeartbeatHooks,
}

/// Seqlock-published hook table. Hooks are registered a handful of times at
/// boot but read on every page fault, sleep and deadline wake on every CPU.
/// A `spin::RwLock` read is an atomic RMW on one shared counter, so those reads
/// bounced its cache line between all CPUs; a seqlock read is two loads and a
/// copy. Writers stay serialized by `writer`.
struct HookCell {
    seq: AtomicU64,
    writer: spin::Mutex<()>,
    value: UnsafeCell<HookRegistry>,
}

// SAFETY: `value` is written only while `writer` is held and `seq` is odd;
// readers copy it volatilely and retry unless `seq` was even and unchanged.
unsafe impl Sync for HookCell {}

struct HookWriteGuard<'a> {
    cell: &'a HookCell,
    _writer: spin::MutexGuard<'a, ()>,
}

impl HookCell {
    const fn new(value: HookRegistry) -> Self {
        Self {
            seq: AtomicU64::new(0),
            writer: spin::Mutex::new(()),
            value: UnsafeCell::new(value),
        }
    }

    fn read(&self) -> HookRegistry {
        loop {
            // ORDERING: Acquire pairs with the writer's final Release store.
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 0 {
                // SAFETY: a torn copy is discarded by the sequence recheck; the
                // fields are plain function pointers with no drop glue.
                let value = unsafe { core::ptr::read_volatile(self.value.get()) };
                // ORDERING: the copy completes before the recheck load.
                fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == before {
                    return value;
                }
            }
            core::hint::spin_loop();
        }
    }

    fn begin_write<'a>(&'a self, writer: spin::MutexGuard<'a, ()>) -> HookWriteGuard<'a> {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        // ORDERING: the odd sequence is visible before any field write.
        fence(Ordering::Release);
        HookWriteGuard { cell: self, _writer: writer }
    }

    fn write(&self) -> HookWriteGuard<'_> {
        self.begin_write(self.writer.lock())
    }

    #[cfg(test)]
    fn try_write(&self) -> Option<HookWriteGuard<'_>> {
        self.writer.try_lock().map(|writer| self.begin_write(writer))
    }
}

impl Deref for HookWriteGuard<'_> {
    type Target = HookRegistry;
    fn deref(&self) -> &HookRegistry {
        // SAFETY: the writer lock is held; no other writer exists.
        unsafe { &*self.cell.value.get() }
    }
}

impl DerefMut for HookWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut HookRegistry {
        // SAFETY: the writer lock is held and the sequence is odd.
        unsafe { &mut *self.cell.value.get() }
    }
}

impl Drop for HookWriteGuard<'_> {
    fn drop(&mut self) {
        let seq = self.cell.seq.load(Ordering::Relaxed);
        // ORDERING: Release publishes every field write with the even sequence.
        self.cell.seq.store(seq.wrapping_add(1), Ordering::Release);
    }
}

static HOOKS: HookCell = HookCell::new(HookRegistry {
    task: TaskHooks {
        try_handle_current_user_page_fault: None,
        retire_current_user_task_due_to_fault: None,
        halt_current_retired_task: None,
        current_user_snapshot: None,
        is_scheduler_initialized: None,
        current_task_id: None,
        arm_block_current_task: None,
        cancel_block_current_task: None,
        commit_block_current_task_and_yield: None,
        wake_task: None,
        yield_now: None,
    },
    interrupt: InterruptHooks {
        dispatch_pic_irq: None,
    },
    heartbeat: HeartbeatHooks { snapshot: None },
});

pub fn register_task_hooks(hooks: TaskHooks) {
    HOOKS.write().task = hooks;
}

pub fn register_interrupt_hooks(hooks: InterruptHooks) {
    HOOKS.write().interrupt = hooks;
}

pub fn register_heartbeat_hooks(hooks: HeartbeatHooks) {
    HOOKS.write().heartbeat = hooks;
}

fn task_hooks() -> TaskHooks {
    HOOKS.read().task
}

fn interrupt_hooks() -> InterruptHooks {
    HOOKS.read().interrupt
}

fn heartbeat_hooks() -> HeartbeatHooks {
    HOOKS.read().heartbeat
}

/// Invokes the bounded, pageable user-fault admission hook without retaining
/// the hook registry lock across the callback.
pub fn try_handle_current_user_page_fault(
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    task_hooks()
        .try_handle_current_user_page_fault
        .map(|hook| hook(error_code, cr2, rip, rsp))
        .unwrap_or(UserFaultDisposition::Unhandled)
}

pub fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    task_hooks()
        .retire_current_user_task_due_to_fault
        .map(|hook| hook(vector, error_code, cr2, rip, rsp))
        .unwrap_or(UserFaultDisposition::Unhandled)
}

pub fn halt_current_retired_task() -> ! {
    if let Some(hook) = task_hooks().halt_current_retired_task {
        hook()
    }

    panic!("HAL retired-task halt hook is not registered");
}

pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
    task_hooks().current_user_snapshot.and_then(|hook| hook())
}

pub fn is_scheduler_initialized() -> bool {
    task_hooks()
        .is_scheduler_initialized
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn current_task_id() -> Option<u64> {
    task_hooks().current_task_id.and_then(|hook| hook())
}

pub fn arm_block_current_task() -> bool {
    task_hooks()
        .arm_block_current_task
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn cancel_block_current_task() -> bool {
    task_hooks()
        .cancel_block_current_task
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn commit_block_current_task_and_yield() -> Option<bool> {
    task_hooks()
        .commit_block_current_task_and_yield
        .and_then(|hook| hook())
}

pub fn wake_task(task_id: u64) -> bool {
    task_hooks()
        .wake_task
        .map(|hook| hook(task_id))
        .unwrap_or(false)
}

pub fn yield_now() {
    if let Some(hook) = task_hooks().yield_now {
        hook();
    }
}

pub fn dispatch_pic_irq(irq: u8) -> bool {
    interrupt_hooks()
        .dispatch_pic_irq
        .map(|hook| hook(irq))
        .unwrap_or(false)
}

pub fn heartbeat_snapshot() -> HeartbeatSnapshot {
    heartbeat_hooks()
        .snapshot
        .map(|hook| hook())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // These tests temporarily replace the same live registry; save/restore
    // alone does not exclude another native test's callback installation.
    static REGISTRY_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static CALLBACK_OBSERVED_UNLOCKED_REGISTRY: AtomicBool = AtomicBool::new(false);
    static LAST_WAKE_TASK: AtomicU64 = AtomicU64::new(0);

    fn inspect_registry_from_commit_callback() -> Option<bool> {
        CALLBACK_OBSERVED_UNLOCKED_REGISTRY.store(HOOKS.try_write().is_some(), Ordering::SeqCst);
        Some(false)
    }

    fn resume_page_fault_callback(
        _error_code: Option<u64>,
        _cr2: u64,
        _rip: u64,
        _rsp: u64,
    ) -> UserFaultDisposition {
        UserFaultDisposition::Resumed
    }

    #[test]
    fn page_fault_hook_is_optional_and_its_disposition_is_preserved() {
        let _serial = REGISTRY_TEST.lock().unwrap();
        assert_eq!(
            try_handle_current_user_page_fault(Some(0), 0x4000, 0x5000, 0x6000),
            UserFaultDisposition::Unhandled
        );

        let saved = {
            let mut registry = HOOKS.write();
            let saved = registry.task;
            registry.task.try_handle_current_user_page_fault = Some(resume_page_fault_callback);
            saved
        };

        assert_eq!(
            try_handle_current_user_page_fault(Some(2), 0x7000, 0x8000, 0x9000),
            UserFaultDisposition::Resumed
        );

        HOOKS.write().task = saved;
    }

    #[test]
    fn scheduler_callback_runs_after_hook_registry_read_guard_is_released() {
        let _serial = REGISTRY_TEST.lock().unwrap();
        CALLBACK_OBSERVED_UNLOCKED_REGISTRY.store(false, Ordering::SeqCst);
        let saved = {
            let mut registry = HOOKS.write();
            let saved = registry.task;
            registry.task.commit_block_current_task_and_yield =
                Some(inspect_registry_from_commit_callback);
            saved
        };

        assert_eq!(commit_block_current_task_and_yield(), Some(false));
        assert!(CALLBACK_OBSERVED_UNLOCKED_REGISTRY.load(Ordering::SeqCst));

        HOOKS.write().task = saved;
    }

    fn inspect_registry_from_wake_callback(task_id: u64) -> bool {
        CALLBACK_OBSERVED_UNLOCKED_REGISTRY.store(HOOKS.try_write().is_some(), Ordering::SeqCst);
        LAST_WAKE_TASK.store(task_id, Ordering::SeqCst);
        task_id == 4
    }

    #[test]
    fn deadline_wake_hook_forwards_exact_task_outside_registry_guard() {
        let _serial = REGISTRY_TEST.lock().unwrap();
        let saved = {
            let mut registry = HOOKS.write();
            let saved = registry.task;
            registry.task.wake_task = None;
            saved
        };
        assert!(!wake_task(4));
        HOOKS.write().task.wake_task = Some(inspect_registry_from_wake_callback);
        assert!(wake_task(4));
        assert_eq!(LAST_WAKE_TASK.load(Ordering::SeqCst), 4);
        assert!(!wake_task(5));
        assert_eq!(LAST_WAKE_TASK.load(Ordering::SeqCst), 5);
        assert!(CALLBACK_OBSERVED_UNLOCKED_REGISTRY.load(Ordering::SeqCst));
        HOOKS.write().task = saved;
    }
}
