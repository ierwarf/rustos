use x86_64::instructions::interrupts;

use super::{
    CurrentUserSnapshot, RetainedCurrentUserAddressSpace, RetainedCurrentUserProcessState,
    UserFaultDisposition, WaitChildResult, process_table, scheduler_mut, scheduler_ref,
};
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessState, LinuxThreadState};
use crate::user::process_state::{
    ProcessSecurityContext, UserProcessState, WindowsThreadRuntimeState,
};

pub fn current_user_address_space() -> Option<RetainedCurrentUserAddressSpace> {
    let (_, abi, process) = retain_current_user_process_binding()?;
    Some(RetainedCurrentUserAddressSpace {
        abi,
        process_id: process.process_id(),
        process,
    })
}

pub fn current_user_id() -> Option<u64> {
    current_user_snapshot().map(|snapshot| snapshot.thread_id())
}

pub fn current_task_id() -> Option<u64> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_task_id() })
}

pub fn current_user_log_ids() -> Option<(u64, u64)> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_log_ids() })
}

pub fn current_user_process_id() -> Option<u64> {
    current_user_snapshot().map(|snapshot| snapshot.process_id())
}

pub fn current_user_process_thread_count() -> Option<usize> {
    let process_id = current_user_process_id()?;
    process_table::thread_count_by_pid(process_id)
}

pub fn current_linux_thread_state() -> Option<LinuxThreadState> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_linux_thread_state() })
}

pub fn current_user_stack_state() -> Option<super::UserStackState> {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_user_stack_state() })
}

#[allow(dead_code)]
pub fn current_user_thread_id() -> Option<u64> {
    current_user_snapshot().map(|snapshot| snapshot.thread_id())
}

pub fn current_user_snapshot() -> Option<CurrentUserSnapshot> {
    let (thread_id, abi, process_handle, console_session) =
        interrupts::without_interrupts(|| unsafe {
            scheduler_ref().current_user_process_binding()
        })?;
    process_table::with_process_state(process_handle, |process_id, process_state| {
        CurrentUserSnapshot::new(
            abi,
            thread_id,
            process_id,
            console_session,
            process_state.security(),
        )
    })
}

pub fn is_user_task_alive(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().is_user_task_alive(task_id) })
}

pub fn terminate_user_task(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        let requested_by_pid = scheduler_ref().current_user_id();
        scheduler_mut().terminate_user_task(task_id, requested_by_pid)
    })
}

pub fn block_current_user_task() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().block_current_user_task() })
}

pub fn wake_user_task(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().wake_user_task(task_id) })
}

pub fn block_current_task() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().block_current_task() })
}

/// Arms a race-free block on the current task; must be paired with
/// `commit_block_current_task`. Returns false if the slot is invalid or this is
/// the root task.
pub fn arm_block_current_task() -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().arm_block_current_task() })
}

/// Commits a previously armed block. Returns `Some(true)` if blocked,
/// `Some(false)` if a wake raced us and we stayed runnable, `None` on invalid
/// context. Callers must re-check their wakeup condition when `Some(false)` is
/// returned instead of yielding.
pub fn commit_block_current_task() -> Option<bool> {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().commit_block_current_task() })
}

pub fn wake_task(task_id: u64) -> bool {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().wake_task(task_id) })
}

/// Biases the next scheduler pick toward `task_id`. Combine with `wake_task` +
/// `yield_now` to implement direct hand-off (caller donates remaining quantum
/// to the receiver), eliminating round-robin latency on IPC roundtrips.
pub fn set_next_pick_hint(task_id: u64) {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().set_next_pick_hint(task_id) })
}

pub fn current_console_session() -> ConsoleSessionHandle {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_console_session() })
}

pub fn set_current_console_session(session: impl Into<ConsoleSessionHandle>) -> bool {
    let session = session.into();
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_current_console_session(session)
    })
}

pub fn exec_current_user_process(
    address_space: ProcessAddressSpace,
    bootstrap: super::UserTaskBootstrap,
) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exec_current_user_process(address_space, bootstrap)
    })
}

pub fn exec_user_process_by_pid(
    process_id: u64,
    thread_id: u64,
    address_space: ProcessAddressSpace,
    bootstrap: super::UserTaskBootstrap,
) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exec_user_process_by_pid(process_id, thread_id, address_space, bootstrap)
    })
}

pub fn linux_thread_snapshot_by_ids(
    process_id: u64,
    thread_id: u64,
) -> Option<super::LinuxThreadSnapshot> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_ref().linux_thread_snapshot_by_ids(process_id, thread_id)
    })
}

pub fn with_current_user_linux_state_mut<R>(
    f: impl FnOnce(
        u64,
        u64,
        UserAbi,
        &mut ProcessAddressSpace,
        &mut Option<LinuxProcessState>,
        &mut Option<LinuxThreadState>,
    ) -> R,
) -> Option<R> {
    let (process_id, tid, abi, mut process, mut linux_thread_state) =
        retain_current_linux_thread_binding()?;
    let linux_thread_state = unsafe { linux_thread_state.as_mut() };
    let (address_space, linux_process_state) = process
        .state_mut()
        .address_space_and_linux_process_state_mut();
    Some(f(
        process_id,
        tid,
        abi,
        address_space,
        linux_process_state,
        linux_thread_state,
    ))
}

pub fn with_current_user_process_state_mut<R>(
    f: impl FnOnce(u64, UserAbi, &mut UserProcessState) -> R,
) -> Option<R> {
    let (thread_id, abi, mut process) = retain_current_user_process_binding()?;
    Some(process.with_state_mut(|_, process_state| f(thread_id, abi, process_state)))
}

pub fn with_current_user_process_state<R>(
    f: impl FnOnce(u64, UserAbi, &UserProcessState) -> R,
) -> Option<R> {
    let (thread_id, abi, process) = retain_current_user_process_binding()?;
    Some(process.with_state(|_, process_state| f(thread_id, abi, process_state)))
}

pub fn with_process_state_by_pid_mut<R>(
    process_id: u64,
    f: impl FnOnce(&mut UserProcessState) -> R,
) -> Option<R> {
    process_table::with_process_state_by_pid_mut(process_id, f)
}

pub fn note_process_exit_status(process_id: u64, status: i32) -> Option<()> {
    process_table::note_process_exit_status(process_id, status)
}

pub fn parent_process_id_of(process_id: u64) -> Option<u64> {
    process_table::parent_process_id_of(process_id)
}

pub fn wait_for_child(parent_process_id: u64, target_pid: i64) -> WaitChildResult {
    match process_table::wait_for_child(parent_process_id, target_pid) {
        process_table::WaitResult::Exited { pid, status } => {
            WaitChildResult::Exited { pid, status }
        }
        process_table::WaitResult::Pending => WaitChildResult::Pending,
        process_table::WaitResult::NoMatchingChild => WaitChildResult::NoMatchingChild,
    }
}

pub fn with_current_mm<R>(f: impl FnOnce(&ProcessAddressSpace) -> R) -> Option<R> {
    let (_, _, process) = retain_current_user_process_binding()?;
    Some(f(process.state().address_space()))
}

pub fn with_current_process_credentials<R>(
    f: impl FnOnce(ProcessSecurityContext) -> R,
) -> Option<R> {
    with_current_user_process_state(|_, _, process_state| f(process_state.security()))
}

pub fn retain_current_user_process_state() -> Option<RetainedCurrentUserProcessState> {
    let (_, abi, process) = retain_current_user_process_binding()?;
    Some(RetainedCurrentUserProcessState {
        process_id: process.process_id(),
        abi,
        process,
    })
}

pub fn with_current_process_state_mut<R>(
    f: impl FnOnce(u64, &mut UserProcessState) -> R,
) -> Option<R> {
    let mut process = retain_current_process_ref()?;
    Some(process.with_state_mut(f))
}

pub fn with_current_process_state<R>(f: impl FnOnce(u64, &UserProcessState) -> R) -> Option<R> {
    let process = retain_current_process_ref()?;
    Some(process.with_state(f))
}

pub fn with_process_state_by_pid<R>(
    process_id: u64,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    process_table::with_process_state_by_pid(process_id, f)
}

pub fn with_current_user_process_and_linux_thread_state_mut<R>(
    f: impl FnOnce(u64, u64, UserAbi, &mut UserProcessState, &mut Option<LinuxThreadState>) -> R,
) -> Option<R> {
    let (process_id, tid, abi, mut process, mut linux_thread_state) =
        retain_current_linux_thread_binding()?;
    let linux_thread_state = unsafe { linux_thread_state.as_mut() };
    Some(f(
        process_id,
        tid,
        abi,
        process.state_mut(),
        linux_thread_state,
    ))
}

pub fn queue_linux_signal(process_id: u64, task_id: u64, signal: u64) -> bool {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().queue_linux_signal(process_id, task_id, signal)
    })
}

#[allow(dead_code)]
pub fn with_current_user_windows_thread_state_mut<R>(
    f: impl FnOnce(u64, &mut WindowsThreadRuntimeState) -> R,
) -> Option<R> {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().with_current_user_windows_thread_state_mut(f)
    })
}

pub fn any_user_process_state(mut f: impl FnMut(u64, &UserProcessState) -> bool) -> bool {
    let (handles, len) = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().user_process_handles_snapshot()
    });
    for handle in handles.into_iter().take(len).flatten() {
        let Some(process) = process_table::retain_process(handle) else {
            continue;
        };
        if process.with_state(|process_id, process_state| f(process_id, process_state)) {
            return true;
        }
    }
    false
}

fn retain_current_user_process_binding() -> Option<(u64, UserAbi, process_table::ProcessRef)> {
    let (thread_id, abi, process_handle, _) = interrupts::without_interrupts(|| unsafe {
        scheduler_ref().current_user_process_binding()
    })?;
    let process = process_table::retain_process(process_handle)?;
    Some((thread_id, abi, process))
}

fn retain_current_linux_thread_binding() -> Option<(
    u64,
    u64,
    UserAbi,
    process_table::ProcessRef,
    core::ptr::NonNull<Option<LinuxThreadState>>,
)> {
    let binding = interrupts::without_interrupts(|| unsafe {
        scheduler_mut().current_linux_thread_binding()
    })?;
    let process = process_table::retain_process(binding.process_handle)?;
    Some((
        process.process_id(),
        binding.tid,
        binding.abi,
        process,
        binding.linux_thread_state,
    ))
}

fn retain_current_process_ref() -> Option<process_table::ProcessRef> {
    let process_handle =
        interrupts::without_interrupts(|| unsafe { scheduler_ref().current_process_handle() })?;
    process_table::retain_process(process_handle)
}

pub fn retire_current_user_task_due_to_fault(
    vector: u8,
    error_code: Option<u64>,
    cr2: u64,
    rip: u64,
    rsp: u64,
) -> UserFaultDisposition {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().retire_current_user_task_due_to_fault(vector, error_code, cr2, rip, rsp)
    })
}

pub fn halt_current_retired_task() -> ! {
    loop {
        interrupts::enable_and_hlt();
    }
}

pub(crate) fn exit_current_task() -> ! {
    interrupts::without_interrupts(|| unsafe {
        let task_id = scheduler_ref().current_task_id();
        let callers_to_wake = task_id
            .map(|task_id| {
                kernel_ipc_runtime::api::fail_endpoints_owned_by_task(
                    task_id,
                    kernel_ipc_runtime::api::IpcError::PeerClosed,
                )
            })
            .unwrap_or_default();
        let scheduler = scheduler_mut();
        for task_id in callers_to_wake {
            let _ = scheduler.wake_task(task_id);
        }
        scheduler.exit_current_task();
    });
    halt_current_retired_task()
}

pub fn exit_current_user_task() -> ! {
    exit_current_task()
}

#[allow(dead_code)]
pub fn current_last_error() -> u32 {
    interrupts::without_interrupts(|| unsafe { scheduler_ref().current_last_error() })
}

pub fn set_current_last_error(value: u32) {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().set_current_last_error(value);
    });
}

pub fn service_deferred_work() -> usize {
    interrupts::without_interrupts(|| unsafe { scheduler_mut().reap_inactive_retired_slots() })
}
