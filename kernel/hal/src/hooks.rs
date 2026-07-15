use nucleus_core::user_abi::UserAbi;
use spin::RwLock;

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

#[derive(Clone, Copy, Default)]
pub struct TaskHooks {
    pub retire_current_user_task_due_to_fault: Option<RetireCurrentUserTaskDueToFaultHook>,
    pub halt_current_retired_task: Option<fn() -> !>,
    pub current_user_snapshot: Option<fn() -> Option<CurrentUserSnapshot>>,
    pub is_scheduler_initialized: Option<fn() -> bool>,
    pub current_task_id: Option<fn() -> Option<u64>>,
    pub arm_block_current_task: Option<fn() -> bool>,
    pub cancel_block_current_task: Option<fn() -> bool>,
    pub commit_block_current_task: Option<fn() -> Option<bool>>,
    pub wake_user_task: Option<fn(u64) -> bool>,
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

static HOOKS: RwLock<HookRegistry> = RwLock::new(HookRegistry {
    task: TaskHooks {
        retire_current_user_task_due_to_fault: None,
        halt_current_retired_task: None,
        current_user_snapshot: None,
        is_scheduler_initialized: None,
        current_task_id: None,
        arm_block_current_task: None,
        cancel_block_current_task: None,
        commit_block_current_task: None,
        wake_user_task: None,
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

pub fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    HOOKS
        .read()
        .task
        .retire_current_user_task_due_to_fault
        .map(|hook| hook(vector, error_code, cr2, rip, rsp))
        .unwrap_or(UserFaultDisposition::Unhandled)
}

pub fn halt_current_retired_task() -> ! {
    if let Some(hook) = HOOKS.read().task.halt_current_retired_task {
        hook()
    }

    panic!("HAL retired-task halt hook is not registered");
}

pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
    HOOKS
        .read()
        .task
        .current_user_snapshot
        .and_then(|hook| hook())
}

pub fn is_scheduler_initialized() -> bool {
    HOOKS
        .read()
        .task
        .is_scheduler_initialized
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn current_task_id() -> Option<u64> {
    HOOKS.read().task.current_task_id.and_then(|hook| hook())
}

pub fn arm_block_current_task() -> bool {
    HOOKS
        .read()
        .task
        .arm_block_current_task
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn cancel_block_current_task() -> bool {
    HOOKS
        .read()
        .task
        .cancel_block_current_task
        .map(|hook| hook())
        .unwrap_or(false)
}

pub fn commit_block_current_task() -> Option<bool> {
    HOOKS
        .read()
        .task
        .commit_block_current_task
        .and_then(|hook| hook())
}

pub fn wake_user_task(task_id: u64) -> bool {
    HOOKS
        .read()
        .task
        .wake_user_task
        .map(|hook| hook(task_id))
        .unwrap_or(false)
}

pub fn yield_now() {
    if let Some(hook) = HOOKS.read().task.yield_now {
        hook();
    }
}

pub fn dispatch_pic_irq(irq: u8) -> bool {
    HOOKS
        .read()
        .interrupt
        .dispatch_pic_irq
        .map(|hook| hook(irq))
        .unwrap_or(false)
}

pub fn heartbeat_snapshot() -> HeartbeatSnapshot {
    HOOKS
        .read()
        .heartbeat
        .snapshot
        .map(|hook| hook())
        .unwrap_or_default()
}
