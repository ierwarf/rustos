use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserFaultDisposition {
    Resumed,
    Retired,
    Unhandled,
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
    /// Accepted for registration compatibility; no HAL path consumes it (the
    /// previous registry stored it but never read it), so it has no slot.
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

/// One lock-free hook slot. Hooks are installed at boot and read on hot paths
/// (page fault, task sleep, deadline wake), so a read is a single atomic load:
/// no lock, no IRQ masking, and callbacks never run under any registry state.
///
/// `F` is only ever instantiated with `Option<fn(..) -> ..>` types, whose
/// guaranteed null-pointer niche makes `None` the all-zero word.
struct HookSlot<F: Copy> {
    word: AtomicUsize,
    _hook: PhantomData<F>,
}

impl<F: Copy> HookSlot<F> {
    const fn new() -> Self {
        Self {
            word: AtomicUsize::new(0),
            _hook: PhantomData,
        }
    }

    fn store(&self, hook: F) {
        const { assert!(size_of::<F>() == size_of::<usize>()) };
        // SAFETY: `F` is `Option<fn(..)>`, which has the size and layout of a
        // pointer with `None == 0`; every bit pattern it can hold is a valid
        // `usize`. Release publishes the callee's prior initialisation to
        // Acquire readers.
        let word = unsafe { core::mem::transmute_copy::<F, usize>(&hook) };
        self.word.store(word, Ordering::Release);
    }

    fn load(&self) -> F {
        const { assert!(size_of::<F>() == size_of::<usize>()) };
        let word = self.word.load(Ordering::Acquire);
        // SAFETY: `word` is either 0 or a value previously produced by
        // `store` from a valid `F`, and `F` is `Option<fn(..)>` where 0 is
        // `None`.
        unsafe { core::mem::transmute_copy::<usize, F>(&word) }
    }
}

struct TaskHookSlots {
    try_handle_current_user_page_fault: HookSlot<Option<TryHandleCurrentUserPageFaultHook>>,
    retire_current_user_task_due_to_fault: HookSlot<Option<RetireCurrentUserTaskDueToFaultHook>>,
    halt_current_retired_task: HookSlot<Option<fn() -> !>>,
    is_scheduler_initialized: HookSlot<Option<fn() -> bool>>,
    current_task_id: HookSlot<Option<fn() -> Option<u64>>>,
    arm_block_current_task: HookSlot<Option<fn() -> bool>>,
    cancel_block_current_task: HookSlot<Option<fn() -> bool>>,
    commit_block_current_task_and_yield: HookSlot<Option<fn() -> Option<bool>>>,
    wake_task: HookSlot<Option<fn(u64) -> bool>>,
    yield_now: HookSlot<Option<fn()>>,
}

static TASK_HOOKS: TaskHookSlots = TaskHookSlots {
    try_handle_current_user_page_fault: HookSlot::new(),
    retire_current_user_task_due_to_fault: HookSlot::new(),
    halt_current_retired_task: HookSlot::new(),
    is_scheduler_initialized: HookSlot::new(),
    current_task_id: HookSlot::new(),
    arm_block_current_task: HookSlot::new(),
    cancel_block_current_task: HookSlot::new(),
    commit_block_current_task_and_yield: HookSlot::new(),
    wake_task: HookSlot::new(),
    yield_now: HookSlot::new(),
};
static DISPATCH_PIC_IRQ_HOOK: HookSlot<Option<fn(u8) -> bool>> = HookSlot::new();
static HEARTBEAT_SNAPSHOT_HOOK: HookSlot<Option<fn() -> HeartbeatSnapshot>> = HookSlot::new();

/// Registration is a boot-time operation; callers install each hook group once
/// before the paths that read it can run, so per-slot publication is enough.
pub fn register_task_hooks(hooks: TaskHooks) {
    let slots = &TASK_HOOKS;
    slots
        .try_handle_current_user_page_fault
        .store(hooks.try_handle_current_user_page_fault);
    slots
        .retire_current_user_task_due_to_fault
        .store(hooks.retire_current_user_task_due_to_fault);
    slots
        .halt_current_retired_task
        .store(hooks.halt_current_retired_task);
    slots
        .is_scheduler_initialized
        .store(hooks.is_scheduler_initialized);
    slots.current_task_id.store(hooks.current_task_id);
    slots
        .arm_block_current_task
        .store(hooks.arm_block_current_task);
    slots
        .cancel_block_current_task
        .store(hooks.cancel_block_current_task);
    slots
        .commit_block_current_task_and_yield
        .store(hooks.commit_block_current_task_and_yield);
    slots.wake_task.store(hooks.wake_task);
    slots.yield_now.store(hooks.yield_now);
}

pub fn register_interrupt_hooks(hooks: InterruptHooks) {
    DISPATCH_PIC_IRQ_HOOK.store(hooks.dispatch_pic_irq);
}

pub fn register_heartbeat_hooks(hooks: HeartbeatHooks) {
    HEARTBEAT_SNAPSHOT_HOOK.store(hooks.snapshot);
}

/// Invokes the bounded, pageable user-fault admission hook.
pub fn try_handle_current_user_page_fault(
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    TASK_HOOKS
        .try_handle_current_user_page_fault
        .load()
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
    TASK_HOOKS
        .retire_current_user_task_due_to_fault
        .load()
        .map(|hook| hook(vector, error_code, cr2, rip, rsp))
        .unwrap_or(UserFaultDisposition::Unhandled)
}

pub fn halt_current_retired_task() -> ! {
    if let Some(hook) = TASK_HOOKS.halt_current_retired_task.load() {
        hook()
    }

    panic!("HAL retired-task halt hook is not registered");
}

pub fn is_scheduler_initialized() -> bool {
    TASK_HOOKS
        .is_scheduler_initialized
        .load()
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn current_task_id() -> Option<u64> {
    TASK_HOOKS.current_task_id.load().and_then(|hook| hook())
}

pub fn arm_block_current_task() -> bool {
    TASK_HOOKS
        .arm_block_current_task
        .load()
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn cancel_block_current_task() -> bool {
    TASK_HOOKS
        .cancel_block_current_task
        .load()
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn commit_block_current_task_and_yield() -> Option<bool> {
    TASK_HOOKS
        .commit_block_current_task_and_yield
        .load()
        .and_then(|hook| hook())
}

pub fn wake_task(task_id: u64) -> bool {
    TASK_HOOKS
        .wake_task
        .load()
        .map(|hook| hook(task_id))
        .unwrap_or(false)
}

pub fn yield_now() {
    if let Some(hook) = TASK_HOOKS.yield_now.load() {
        hook();
    }
}

pub fn dispatch_pic_irq(irq: u8) -> bool {
    DISPATCH_PIC_IRQ_HOOK
        .load()
        .map(|hook| hook(irq))
        .unwrap_or(false)
}

pub fn heartbeat_snapshot() -> HeartbeatSnapshot {
    HEARTBEAT_SNAPSHOT_HOOK
        .load()
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

    fn task_hooks_snapshot() -> TaskHooks {
        TaskHooks {
            try_handle_current_user_page_fault: TASK_HOOKS
                .try_handle_current_user_page_fault
                .load(),
            retire_current_user_task_due_to_fault: TASK_HOOKS
                .retire_current_user_task_due_to_fault
                .load(),
            halt_current_retired_task: TASK_HOOKS.halt_current_retired_task.load(),
            is_scheduler_initialized: TASK_HOOKS.is_scheduler_initialized.load(),
            current_task_id: TASK_HOOKS.current_task_id.load(),
            arm_block_current_task: TASK_HOOKS.arm_block_current_task.load(),
            cancel_block_current_task: TASK_HOOKS.cancel_block_current_task.load(),
            commit_block_current_task_and_yield: TASK_HOOKS
                .commit_block_current_task_and_yield
                .load(),
            wake_task: TASK_HOOKS.wake_task.load(),
            yield_now: TASK_HOOKS.yield_now.load(),
        }
    }

    /// Hook callbacks run with no registry state held, so a callback may
    /// itself (re)register hooks; reinstalling the current values is a no-op.
    fn callback_can_reregister_hooks() -> bool {
        register_task_hooks(task_hooks_snapshot());
        true
    }

    fn inspect_registry_from_commit_callback() -> Option<bool> {
        CALLBACK_OBSERVED_UNLOCKED_REGISTRY
            .store(callback_can_reregister_hooks(), Ordering::SeqCst);
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

        let saved = task_hooks_snapshot();
        register_task_hooks(TaskHooks {
            try_handle_current_user_page_fault: Some(resume_page_fault_callback),
            ..saved
        });

        assert_eq!(
            try_handle_current_user_page_fault(Some(2), 0x7000, 0x8000, 0x9000),
            UserFaultDisposition::Resumed
        );

        register_task_hooks(saved);
    }

    #[test]
    fn scheduler_callback_runs_after_hook_registry_read_guard_is_released() {
        let _serial = REGISTRY_TEST.lock().unwrap();
        CALLBACK_OBSERVED_UNLOCKED_REGISTRY.store(false, Ordering::SeqCst);
        let saved = task_hooks_snapshot();
        register_task_hooks(TaskHooks {
            commit_block_current_task_and_yield: Some(inspect_registry_from_commit_callback),
            ..saved
        });

        assert_eq!(commit_block_current_task_and_yield(), Some(false));
        assert!(CALLBACK_OBSERVED_UNLOCKED_REGISTRY.load(Ordering::SeqCst));

        register_task_hooks(saved);
    }

    fn inspect_registry_from_wake_callback(task_id: u64) -> bool {
        CALLBACK_OBSERVED_UNLOCKED_REGISTRY
            .store(callback_can_reregister_hooks(), Ordering::SeqCst);
        LAST_WAKE_TASK.store(task_id, Ordering::SeqCst);
        task_id == 4
    }

    #[test]
    fn deadline_wake_hook_forwards_exact_task_outside_registry_guard() {
        let _serial = REGISTRY_TEST.lock().unwrap();
        let saved = task_hooks_snapshot();
        register_task_hooks(TaskHooks {
            wake_task: None,
            ..saved
        });
        assert!(!wake_task(4));
        register_task_hooks(TaskHooks {
            wake_task: Some(inspect_registry_from_wake_callback),
            ..saved
        });
        assert!(wake_task(4));
        assert_eq!(LAST_WAKE_TASK.load(Ordering::SeqCst), 4);
        assert!(!wake_task(5));
        assert_eq!(LAST_WAKE_TASK.load(Ordering::SeqCst), 5);
        assert!(CALLBACK_OBSERVED_UNLOCKED_REGISTRY.load(Ordering::SeqCst));
        register_task_hooks(saved);
    }

    fn bench_task_id() -> Option<u64> {
        Some(1)
    }

    fn bench_sched_init() -> bool {
        true
    }

    #[test]
    #[ignore = "micro-benchmark; run with --release -- --ignored --nocapture"]
    fn hook_read_path_microbenchmark() {
        let _serial = REGISTRY_TEST.lock().unwrap();
        let saved = task_hooks_snapshot();
        register_task_hooks(TaskHooks {
            current_task_id: Some(bench_task_id),
            is_scheduler_initialized: Some(bench_sched_init),
            ..saved
        });
        for threads in [1usize, 4] {
            const ITERS: u64 = 5_000_000;
            let start = std::time::Instant::now();
            let handles: std::vec::Vec<_> = (0..threads)
                .map(|_| {
                    std::thread::spawn(|| {
                        let mut acc = 0u64;
                        for _ in 0..ITERS {
                            acc += current_task_id().unwrap_or(0);
                            acc += is_scheduler_initialized() as u64;
                        }
                        acc
                    })
                })
                .collect();
            for h in handles {
                std::hint::black_box(h.join().unwrap());
            }
            let ns = start.elapsed().as_nanos() as f64 / (ITERS * 2) as f64;
            std::println!("hook read: threads={threads} {ns:.2} ns/call (per-thread wall)");
        }
        register_task_hooks(saved);
    }
}
