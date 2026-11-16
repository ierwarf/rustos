//! Windows NT synchronization primitives and real scheduler blocking wait queues.
//!
//! Replaces `wineserver` / `ntdll/unix/sync.c` with Ring0 scheduler wait/wake
//! (`arm_block_current_task` / `commit_block_current_task_and_yield` /
//! `wake_user_task`) and per-process / named NT sync objects.

use alloc::string::String;
use alloc::vec::Vec;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::windows::{
    STATUS_ABANDONED_WAIT_0, STATUS_ALERTED, STATUS_INVALID_HANDLE, STATUS_INVALID_INFO_CLASS,
    STATUS_INVALID_PARAMETER, STATUS_MUTANT_NOT_OWNED, STATUS_NO_MEMORY,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SEMAPHORE_LIMIT_EXCEEDED, STATUS_SUCCESS, STATUS_TIMEOUT,
    STATUS_WAIT_0, WIN32_EVENT_HANDLE_BASE, WIN32_MUTANT_HANDLE_BASE,
    WIN32_SEMAPHORE_HANDLE_BASE, WIN32_THREAD_HANDLE_BASE, WIN32_PROCESS_HANDLE_BASE,
    WIN32_TIMER_HANDLE_BASE, WIN32_FD_HANDLE_BASE, WIN32_SECTION_HANDLE_BASE,
};

use super::super::SyscallFrame;
use super::nt_dispatch::{
    current_filetime_100ns, read_object_attributes_unicode, read_stack_arg,
    write_user_u32_checked, write_user_u64,
};
use crate::multitask;
use crate::user::process_state::{
    Win32EventObject, Win32MutantObject, Win32ProcessObjects, Win32SemaphoreObject,
};
use crate::user::sysops::usermem;

const MAX_WAIT_OBJECTS: usize = 64;
const SYNC_WAITERS_CAPACITY: usize = 256;
const ALERT_SLOTS_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitTarget {
    None,
    Handle { pid: u64, handle: u64 },
    AlertTid { tid: u64 },
    Keyed { pid: u64, key: u64, release: bool },
    ThreadExit { tid: u64 },
    ProcessExit { pid: u64 },
}

#[derive(Clone, Copy, Debug)]
struct SyncWaiter {
    task_id: u64,
    count: usize,
    targets: [WaitTarget; MAX_WAIT_OBJECTS],
    completed: bool,
}

static WIN32_SYNC_WAITERS: TrackedSpinLock<
    [Option<SyncWaiter>; SYNC_WAITERS_CAPACITY],
    { LockClass::FutexWaiter as u8 },
> = TrackedSpinLock::new([None; SYNC_WAITERS_CAPACITY]);

#[derive(Clone, Copy, Debug)]
struct ThreadAlertFlag {
    tid: u64,
    alerted: bool,
}

static WIN32_THREAD_ALERTS: TrackedSpinLock<
    [Option<ThreadAlertFlag>; ALERT_SLOTS_CAPACITY],
    { LockClass::FutexWaiter as u8 },
> = TrackedSpinLock::new([None; ALERT_SLOTS_CAPACITY]);

#[derive(Clone, Debug)]
enum NamedSyncKind {
    Event { signaled: bool, manual_reset: bool },
    Mutant { owner_tid: Option<u64>, recursion_count: u32, abandoned: bool },
    Semaphore { count: i32, max_count: i32 },
}

#[derive(Clone, Debug)]
struct NamedSyncEntry {
    name: String,
    kind: NamedSyncKind,
}

static WIN32_NAMED_SYNC: TrackedSpinLock<
    Vec<NamedSyncEntry>,
    { LockClass::CompatWaitset as u8 },
> = TrackedSpinLock::new(Vec::new());

fn normalize_sync_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

pub(super) fn deadline_from_nt_timeout(timeout_ptr: u64) -> Result<Option<(u64, bool)>, u64> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let raw_100ns = usermem::read_current_user_struct::<i64>(timeout_ptr)
        .map_err(|_| STATUS_INVALID_PARAMETER)?;
    if raw_100ns == 0 {
        return Ok(Some((crate::arch::rtc::ticks(), true)));
    }
    let delta_100ns: u64 = if raw_100ns < 0 {
        raw_100ns.unsigned_abs()
    } else {
        let now_ft = current_filetime_100ns();
        if raw_100ns <= now_ft {
            return Ok(Some((crate::arch::rtc::ticks(), true)));
        }
        (raw_100ns - now_ft) as u64
    };
    let tps = crate::arch::rtc::ticks_per_second().max(1);
    let wait_ticks = ((delta_100ns as u128)
        .saturating_mul(tps as u128)
        .saturating_add(9_999_999)
        / 10_000_000)
        .min(u64::MAX as u128) as u64;
    let now_tick = crate::arch::rtc::ticks();
    Ok(Some((now_tick.saturating_add(wait_ticks.max(1)), false)))
}

fn wake_tasks(tids: &[u64]) {
    for &tid in tids {
        if tid != 0 {
            let _ = multitask::wake_user_task(tid);
        }
    }
}

fn collect_waiters_matching<F>(mut pred: F, max_wake: usize) -> Vec<u64>
where
    F: FnMut(&SyncWaiter) -> bool,
{
    let mut wakes = Vec::new();
    let mut table = WIN32_SYNC_WAITERS.lock();
    for slot in table.iter_mut() {
        let Some(waiter) = slot.as_mut() else {
            continue;
        };
        if !waiter.completed && pred(waiter) {
            waiter.completed = true;
            wakes.push(waiter.task_id);
            if wakes.len() >= max_wake {
                break;
            }
        }
    }
    wakes
}

fn clear_sync_waiter(task_id: u64) {
    let mut table = WIN32_SYNC_WAITERS.lock();
    for slot in table.iter_mut() {
        if slot.is_some_and(|w| w.task_id == task_id) {
            *slot = None;
        }
    }
}

fn register_sync_waiter(waiter: SyncWaiter) -> bool {
    let mut table = WIN32_SYNC_WAITERS.lock();
    for slot in table.iter_mut() {
        if slot.is_some_and(|w| w.task_id == waiter.task_id) {
            *slot = Some(waiter);
            return true;
        }
    }
    for slot in table.iter_mut() {
        if slot.is_none() {
            *slot = Some(waiter);
            return true;
        }
    }
    false
}

/// Wakes any thread waiting in `NtWaitForSingleObject`/`NtWaitForMultipleObjects`
/// on the given exiting `tid` or `pid`.
pub(super) fn notify_task_exited(pid: u64, tid: u64, process_exiting: bool) {
    let wakes = collect_waiters_matching(
        |w| {
            w.targets[..w.count].iter().any(|t| match *t {
                WaitTarget::ThreadExit { tid: target_tid } => target_tid == tid,
                WaitTarget::ProcessExit { pid: target_pid } => {
                    process_exiting && target_pid == pid
                }
                _ => false,
            })
        },
        SYNC_WAITERS_CAPACITY,
    );
    wake_tasks(&wakes);
}

fn is_event_handle(h: u64) -> bool {
    (WIN32_EVENT_HANDLE_BASE..WIN32_MUTANT_HANDLE_BASE).contains(&h)
}

fn is_mutant_handle(h: u64) -> bool {
    (WIN32_MUTANT_HANDLE_BASE..WIN32_SEMAPHORE_HANDLE_BASE).contains(&h)
}

fn is_semaphore_handle(h: u64) -> bool {
    (WIN32_SEMAPHORE_HANDLE_BASE..WIN32_TIMER_HANDLE_BASE).contains(&h)
}

fn is_thread_handle(h: u64) -> bool {
    (WIN32_THREAD_HANDLE_BASE..WIN32_PROCESS_HANDLE_BASE).contains(&h)
}

fn is_process_handle(h: u64) -> bool {
    (WIN32_PROCESS_HANDLE_BASE..0x5000_0000).contains(&h)
}

/// Checks if a single handle is currently signaled without mutating it.
fn handle_is_signaled(objs: &Win32ProcessObjects, cur_tid: u64, handle: u64) -> Result<bool, u64> {
    if is_event_handle(handle) {
        let Some(ev) = objs.events.iter().find(|e| e.handle == handle) else {
            return Err(STATUS_INVALID_HANDLE);
        };
        return Ok(ev.signaled);
    }
    if is_mutant_handle(handle) {
        let Some(m) = objs.mutants.iter().find(|m| m.handle == handle) else {
            return Err(STATUS_INVALID_HANDLE);
        };
        return Ok(m.owner_tid.is_none() || m.owner_tid == Some(cur_tid));
    }
    if is_semaphore_handle(handle) {
        let Some(s) = objs.semaphores.iter().find(|s| s.handle == handle) else {
            return Err(STATUS_INVALID_HANDLE);
        };
        return Ok(s.count > 0);
    }
    if is_thread_handle(handle) {
        let Some(th) = objs.threads.iter().find(|t| t.handle == handle) else {
            return Err(STATUS_INVALID_HANDLE);
        };
        return Ok(!multitask::is_user_task_alive(th.tid));
    }
    if is_process_handle(handle) {
        let Some(pr) = objs.processes.iter().find(|p| p.handle == handle) else {
            return Err(STATUS_INVALID_HANDLE);
        };
        return Ok(multitask::user_target_abi_if_alive(pr.pid, 0).is_none());
    }
    if (WIN32_FD_HANDLE_BASE..WIN32_SECTION_HANDLE_BASE).contains(&handle) {
        return Ok(true);
    }
    Err(STATUS_INVALID_HANDLE)
}

/// Consumes/acquires a handle once known to be signaled. Returns `true` if abandoned mutant.
fn acquire_signaled_handle(objs: &mut Win32ProcessObjects, cur_tid: u64, handle: u64) -> bool {
    if is_event_handle(handle) {
        if let Some(ev) = objs.events.iter_mut().find(|e| e.handle == handle)
            && !ev.manual_reset
        {
            ev.signaled = false;
        }
        return false;
    }
    if is_mutant_handle(handle) {
        if let Some(m) = objs.mutants.iter_mut().find(|m| m.handle == handle) {
            let was_abandoned = m.abandoned;
            m.owner_tid = Some(cur_tid);
            m.recursion_count = m.recursion_count.saturating_add(1);
            m.abandoned = false;
            return was_abandoned;
        }
        return false;
    }
    if is_semaphore_handle(handle) {
        if let Some(s) = objs.semaphores.iter_mut().find(|s| s.handle == handle)
            && s.count > 0
        {
            s.count -= 1;
        }
        return false;
    }
    false
}

fn target_for_handle(objs: &Win32ProcessObjects, pid: u64, handle: u64) -> Result<WaitTarget, u64> {
    if is_event_handle(handle) || is_mutant_handle(handle) || is_semaphore_handle(handle) {
        Ok(WaitTarget::Handle { pid, handle })
    } else if is_thread_handle(handle) {
        let th = objs
            .threads
            .iter()
            .find(|t| t.handle == handle)
            .ok_or(STATUS_INVALID_HANDLE)?;
        Ok(WaitTarget::ThreadExit { tid: th.tid })
    } else if is_process_handle(handle) {
        let pr = objs
            .processes
            .iter()
            .find(|p| p.handle == handle)
            .ok_or(STATUS_INVALID_HANDLE)?;
        Ok(WaitTarget::ProcessExit { pid: pr.pid })
    } else if (WIN32_FD_HANDLE_BASE..WIN32_SECTION_HANDLE_BASE).contains(&handle) {
        Ok(WaitTarget::Handle { pid, handle })
    } else {
        Err(STATUS_INVALID_HANDLE)
    }
}

pub(super) fn nt_create_event(frame: &SyscallFrame, is_create: bool) -> u64 {
    let (handle_ptr, _access, oa_ptr, event_type) = (frame.r10, frame.rdx, frame.r8, frame.r9);
    if handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let name = if oa_ptr != 0 {
        read_object_attributes_unicode(oa_ptr)
            .ok()
            .map(|(_, s)| normalize_sync_name(&s))
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let manual_reset;
    let initial_state;
    if is_create {
        manual_reset = event_type == 0;
        initial_state = read_stack_arg(frame, 4).unwrap_or(0) != 0;
        if let Some(ref key) = name {
            let mut named = WIN32_NAMED_SYNC.lock();
            if let Some(existing) = named.iter().find(|e| e.name == *key) {
                match existing.kind {
                    NamedSyncKind::Event { signaled, manual_reset: mr } => {
                        drop(named);
                        return install_event_handle(handle_ptr, signaled, mr, Some(key.clone()));
                    }
                    _ => return STATUS_INVALID_HANDLE,
                }
            }
            named.push(NamedSyncEntry {
                name: key.clone(),
                kind: NamedSyncKind::Event {
                    signaled: initial_state,
                    manual_reset,
                },
            });
        }
    } else {
        let Some(ref key) = name else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        let named = WIN32_NAMED_SYNC.lock();
        let Some(existing) = named.iter().find(|e| e.name == *key) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        match existing.kind {
            NamedSyncKind::Event { signaled, manual_reset: mr } => {
                manual_reset = mr;
                initial_state = signaled;
            }
            _ => return STATUS_OBJECT_NAME_NOT_FOUND,
        }
    }

    install_event_handle(handle_ptr, initial_state, manual_reset, name)
}

fn install_event_handle(
    handle_ptr: u64,
    signaled: bool,
    manual_reset: bool,
    name: Option<String>,
) -> u64 {
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_EVENT_HANDLE_BASE + objs.next_event_seq + 1;
        objs.next_event_seq += 1;
        objs.events.push(Win32EventObject {
            handle: h,
            signaled,
            manual_reset,
            name,
        });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_modify_event(handle: u64, prev_ptr: u64, signaled: bool, pulse: bool) -> u64 {
    let pid = multitask::current_user_process_id().unwrap_or(0);
    let mut sync_name: Option<String> = None;
    let res = multitask::with_current_user_process_state_mut(|_, _, st| {
        let ev = st
            .win32_objects_mut()
            .events
            .iter_mut()
            .find(|e| e.handle == handle)?;
        let was = ev.signaled;
        let manual_reset = ev.manual_reset;
        sync_name = ev.name.clone();
        ev.signaled = if pulse { false } else { signaled };
        Some((was, manual_reset))
    })
    .flatten();
    let Some((was, manual_reset)) = res else {
        return STATUS_INVALID_HANDLE;
    };
    if let Some(ref key) = sync_name {
        let mut named = WIN32_NAMED_SYNC.lock();
        if let Some(entry) = named.iter_mut().find(|e| e.name == *key)
            && let NamedSyncKind::Event { signaled: ref mut s, .. } = entry.kind
        {
            *s = if pulse { false } else { signaled };
        }
    }
    if signaled || pulse {
        let max_wake = if manual_reset { SYNC_WAITERS_CAPACITY } else { 1 };
        let wakes = collect_waiters_matching(
            |w| {
                w.targets[..w.count]
                    .iter()
                    .any(|t| matches!(*t, WaitTarget::Handle { pid: wp, handle: wh } if wp == pid && wh == handle))
            },
            max_wake,
        );
        if !pulse && !manual_reset && !wakes.is_empty() {
            // Auto-reset event consumed by waking a single waiter.
            let _ = multitask::with_current_user_process_state_mut(|_, _, st| {
                if let Some(ev) = st
                    .win32_objects_mut()
                    .events
                    .iter_mut()
                    .find(|e| e.handle == handle)
                {
                    ev.signaled = false;
                }
            });
        }
        wake_tasks(&wakes);
    }
    if prev_ptr != 0 {
        return write_user_u32_checked(prev_ptr, was as u32);
    }
    STATUS_SUCCESS
}

pub(super) fn nt_query_event(handle: u64, info_class: u64, buf_ptr: u64, len: u64) -> u64 {
    if info_class != 0 {
        return STATUS_INVALID_INFO_CLASS;
    }
    if len < 8 || buf_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let state = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .events
            .iter()
            .find(|e| e.handle == handle)
            .map(|e| (if e.manual_reset { 0_u32 } else { 1_u32 }, e.signaled as u32))
    })
    .flatten();
    let Some((event_type, signaled)) = state else {
        return STATUS_INVALID_HANDLE;
    };
    let mut raw = [0_u8; 8];
    raw[0..4].copy_from_slice(&event_type.to_le_bytes());
    raw[4..8].copy_from_slice(&signaled.to_le_bytes());
    if usermem::write_current_user_bytes(buf_ptr, &raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_create_or_open_mutant(frame: &SyscallFrame, is_create: bool) -> u64 {
    let (handle_ptr, _access, oa_ptr, initial_owner) =
        (frame.r10, frame.rdx, frame.r8, frame.r9 != 0);
    if handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let cur_tid = multitask::current_user_thread_id().unwrap_or(1);
    let name = if oa_ptr != 0 {
        read_object_attributes_unicode(oa_ptr)
            .ok()
            .map(|(_, s)| normalize_sync_name(&s))
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let owner_tid;
    let recursion_count;
    let abandoned;
    if is_create {
        owner_tid = initial_owner.then_some(cur_tid);
        recursion_count = u32::from(initial_owner);
        abandoned = false;
        if let Some(ref key) = name {
            let mut named = WIN32_NAMED_SYNC.lock();
            if let Some(existing) = named.iter().find(|e| e.name == *key) {
                match existing.kind {
                    NamedSyncKind::Mutant { owner_tid: ot, recursion_count: rc, abandoned: ab } => {
                        drop(named);
                        return install_mutant_handle(handle_ptr, ot, rc, ab, Some(key.clone()));
                    }
                    _ => return STATUS_INVALID_HANDLE,
                }
            }
            named.push(NamedSyncEntry {
                name: key.clone(),
                kind: NamedSyncKind::Mutant {
                    owner_tid,
                    recursion_count,
                    abandoned,
                },
            });
        }
    } else {
        let Some(ref key) = name else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        let named = WIN32_NAMED_SYNC.lock();
        let Some(existing) = named.iter().find(|e| e.name == *key) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        match existing.kind {
            NamedSyncKind::Mutant { owner_tid: ot, recursion_count: rc, abandoned: ab } => {
                owner_tid = ot;
                recursion_count = rc;
                abandoned = ab;
            }
            _ => return STATUS_OBJECT_NAME_NOT_FOUND,
        }
    }

    install_mutant_handle(handle_ptr, owner_tid, recursion_count, abandoned, name)
}

fn install_mutant_handle(
    handle_ptr: u64,
    owner_tid: Option<u64>,
    recursion_count: u32,
    abandoned: bool,
    name: Option<String>,
) -> u64 {
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_MUTANT_HANDLE_BASE + objs.next_mutant_seq + 1;
        objs.next_mutant_seq += 1;
        objs.mutants.push(Win32MutantObject {
            handle: h,
            owner_tid,
            recursion_count,
            abandoned,
            name,
        });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_release_mutant(handle: u64, prev_count_ptr: u64) -> u64 {
    let cur_tid = multitask::current_user_thread_id().unwrap_or(1);
    let pid = multitask::current_user_process_id().unwrap_or(0);
    let outcome = multitask::with_current_user_process_state_mut(|_, _, st| {
        let m = st
            .win32_objects_mut()
            .mutants
            .iter_mut()
            .find(|m| m.handle == handle)?;
        if m.owner_tid != Some(cur_tid) || m.recursion_count == 0 {
            return Some(Err(STATUS_MUTANT_NOT_OWNED));
        }
        let prev = m.recursion_count;
        m.recursion_count -= 1;
        if m.recursion_count == 0 {
            m.owner_tid = None;
        }
        Some(Ok((prev, m.owner_tid.is_none())))
    })
    .flatten();
    match outcome {
        None => return STATUS_INVALID_HANDLE,
        Some(Err(status)) => return status,
        Some(Ok((prev, released))) => {
            if released {
                let wakes = collect_waiters_matching(
                    |w| {
                        w.targets[..w.count].iter().any(|t| {
                            matches!(*t, WaitTarget::Handle { pid: wp, handle: wh } if wp == pid && wh == handle)
                        })
                    },
                    1,
                );
                wake_tasks(&wakes);
            }
            if prev_count_ptr != 0 {
                return write_user_u32_checked(prev_count_ptr, prev);
            }
        }
    }
    STATUS_SUCCESS
}

pub(super) fn nt_query_mutant(frame: &SyscallFrame) -> u64 {
    let (handle, info_class, buf_ptr, len) =
        (frame.r10, frame.rdx, frame.r8, (frame.r9 as u32) as u64);
    if info_class != 0 {
        return STATUS_INVALID_INFO_CLASS;
    }
    if len < 8 || buf_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let cur_tid = multitask::current_user_thread_id().unwrap_or(1);
    let state = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .mutants
            .iter()
            .find(|m| m.handle == handle)
            .map(|m| {
                let current_count: i32 = if m.owner_tid.is_none() {
                    1
                } else {
                    -(m.recursion_count as i32)
                };
                let owned_by_caller = (m.owner_tid == Some(cur_tid)) as u8;
                let abandoned = m.abandoned as u8;
                (current_count, owned_by_caller, abandoned)
            })
    })
    .flatten();
    let Some((count, owned, abandoned)) = state else {
        return STATUS_INVALID_HANDLE;
    };
    let mut raw = [0_u8; 8];
    raw[0..4].copy_from_slice(&count.to_le_bytes());
    raw[4] = owned;
    raw[5] = abandoned;
    if usermem::write_current_user_bytes(buf_ptr, &raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_create_or_open_semaphore(frame: &SyscallFrame, is_create: bool) -> u64 {
    let (handle_ptr, _access, oa_ptr, initial_count) =
        (frame.r10, frame.rdx, frame.r8, frame.r9 as i32);
    if handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let max_count = if is_create {
        read_stack_arg(frame, 4).unwrap_or(0) as i32
    } else {
        1
    };
    if is_create && (max_count <= 0 || initial_count < 0 || initial_count > max_count) {
        return STATUS_INVALID_PARAMETER;
    }
    let name = if oa_ptr != 0 {
        read_object_attributes_unicode(oa_ptr)
            .ok()
            .map(|(_, s)| normalize_sync_name(&s))
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let count;
    let max_c;
    if is_create {
        count = initial_count;
        max_c = max_count;
        if let Some(ref key) = name {
            let mut named = WIN32_NAMED_SYNC.lock();
            if let Some(existing) = named.iter().find(|e| e.name == *key) {
                match existing.kind {
                    NamedSyncKind::Semaphore { count: c, max_count: mc } => {
                        drop(named);
                        return install_semaphore_handle(handle_ptr, c, mc, Some(key.clone()));
                    }
                    _ => return STATUS_INVALID_HANDLE,
                }
            }
            named.push(NamedSyncEntry {
                name: key.clone(),
                kind: NamedSyncKind::Semaphore { count, max_count: max_c },
            });
        }
    } else {
        let Some(ref key) = name else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        let named = WIN32_NAMED_SYNC.lock();
        let Some(existing) = named.iter().find(|e| e.name == *key) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        match existing.kind {
            NamedSyncKind::Semaphore { count: c, max_count: mc } => {
                count = c;
                max_c = mc;
            }
            _ => return STATUS_OBJECT_NAME_NOT_FOUND,
        }
    }

    install_semaphore_handle(handle_ptr, count, max_c, name)
}

fn install_semaphore_handle(
    handle_ptr: u64,
    count: i32,
    max_count: i32,
    name: Option<String>,
) -> u64 {
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_SEMAPHORE_HANDLE_BASE + objs.next_semaphore_seq + 1;
        objs.next_semaphore_seq += 1;
        objs.semaphores.push(Win32SemaphoreObject {
            handle: h,
            count,
            max_count,
            name,
        });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_release_semaphore(handle: u64, release_count: i32, prev_count_ptr: u64) -> u64 {
    if release_count <= 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let pid = multitask::current_user_process_id().unwrap_or(0);
    let outcome = multitask::with_current_user_process_state_mut(|_, _, st| {
        let sem = st
            .win32_objects_mut()
            .semaphores
            .iter_mut()
            .find(|s| s.handle == handle)?;
        let Some(next) = sem.count.checked_add(release_count) else {
            return Some(Err(STATUS_SEMAPHORE_LIMIT_EXCEEDED));
        };
        if next > sem.max_count {
            return Some(Err(STATUS_SEMAPHORE_LIMIT_EXCEEDED));
        }
        let prev = sem.count;
        sem.count = next;
        Some(Ok(prev))
    })
    .flatten();
    match outcome {
        None => return STATUS_INVALID_HANDLE,
        Some(Err(status)) => return status,
        Some(Ok(prev)) => {
            let wakes = collect_waiters_matching(
                |w| {
                    w.targets[..w.count].iter().any(|t| {
                        matches!(*t, WaitTarget::Handle { pid: wp, handle: wh } if wp == pid && wh == handle)
                    })
                },
                release_count as usize,
            );
            wake_tasks(&wakes);
            if prev_count_ptr != 0 {
                return write_user_u32_checked(prev_count_ptr, prev as u32);
            }
        }
    }
    STATUS_SUCCESS
}

pub(super) fn nt_query_semaphore(frame: &SyscallFrame) -> u64 {
    let (handle, info_class, buf_ptr, len) =
        (frame.r10, frame.rdx, frame.r8, (frame.r9 as u32) as u64);
    if info_class != 0 {
        return STATUS_INVALID_INFO_CLASS;
    }
    if len < 8 || buf_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let state = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .semaphores
            .iter()
            .find(|s| s.handle == handle)
            .map(|s| (s.count, s.max_count))
    })
    .flatten();
    let Some((count, max_count)) = state else {
        return STATUS_INVALID_HANDLE;
    };
    let mut raw = [0_u8; 8];
    raw[0..4].copy_from_slice(&count.to_le_bytes());
    raw[4..8].copy_from_slice(&max_count.to_le_bytes());
    if usermem::write_current_user_bytes(buf_ptr, &raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_wait_for_single_object(handle: u64, timeout_ptr: u64) -> u64 {
    wait_handles_blocking(&[handle], false, timeout_ptr)
}

pub(super) fn nt_wait_for_multiple_objects(
    count: u64,
    handles_ptr: u64,
    wait_type: u64,
    timeout_ptr: u64,
) -> u64 {
    let count = count as usize;
    if count == 0 || count > MAX_WAIT_OBJECTS || handles_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let wait_all = wait_type == 0; // WaitAll = 0, WaitAny = 1
    let mut raw = [0_u8; MAX_WAIT_OBJECTS * 8];
    let byte_len = count * 8;
    if usermem::copy_from_current_user_exact(handles_ptr, &mut raw[..byte_len]).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let mut handles = [0_u64; MAX_WAIT_OBJECTS];
    for (idx, chunk) in raw[..byte_len].chunks_exact(8).enumerate() {
        handles[idx] = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
    }
    wait_handles_blocking(&handles[..count], wait_all, timeout_ptr)
}

fn try_satisfy_wait(
    objs: &mut Win32ProcessObjects,
    cur_tid: u64,
    handles: &[u64],
    wait_all: bool,
) -> Result<Option<u64>, u64> {
    if wait_all {
        let mut all_ready = true;
        for &h in handles {
            if !handle_is_signaled(objs, cur_tid, h)? {
                all_ready = false;
                break;
            }
        }
        if !all_ready {
            return Ok(None);
        }
        let mut abandoned = false;
        for &h in handles {
            abandoned |= acquire_signaled_handle(objs, cur_tid, h);
        }
        return Ok(Some(if abandoned {
            STATUS_ABANDONED_WAIT_0
        } else {
            STATUS_WAIT_0
        }));
    }

    for (idx, &h) in handles.iter().enumerate() {
        if handle_is_signaled(objs, cur_tid, h)? {
            let abandoned = acquire_signaled_handle(objs, cur_tid, h);
            let base = if abandoned {
                STATUS_ABANDONED_WAIT_0
            } else {
                STATUS_WAIT_0
            };
            return Ok(Some(base + idx as u64));
        }
    }
    Ok(None)
}

fn wait_handles_blocking(handles: &[u64], wait_all: bool, timeout_ptr: u64) -> u64 {
    let deadline = match deadline_from_nt_timeout(timeout_ptr) {
        Ok(d) => d,
        Err(status) => return status,
    };
    let pid = multitask::current_user_process_id().unwrap_or(0);
    let cur_tid = multitask::current_user_thread_id().unwrap_or(0);
    if cur_tid == 0 {
        return STATUS_INVALID_PARAMETER;
    }

    loop {
        while !multitask::arm_block_current_task() {
            if multitask::current_task_id() != Some(cur_tid) {
                return STATUS_INVALID_PARAMETER;
            }
            if let Some((tick, _)) = deadline
                && crate::arch::rtc::ticks() >= tick
            {
                return STATUS_TIMEOUT;
            }
            multitask::cond_resched();
        }

        let reg_outcome: Result<Option<u64>, u64> = multitask::with_current_user_process_state_mut(
            |_, _, st| {
                let objs = st.win32_objects_mut();
                if let Some(status) = try_satisfy_wait(objs, cur_tid, handles, wait_all)? {
                    return Ok(Some(status));
                }
                if matches!(deadline, Some((_, true))) {
                    return Err(STATUS_TIMEOUT);
                }
                let mut waiter = SyncWaiter {
                    task_id: cur_tid,
                    count: handles.len(),
                    targets: [WaitTarget::None; MAX_WAIT_OBJECTS],
                    completed: false,
                };
                for (i, &h) in handles.iter().enumerate() {
                    waiter.targets[i] = target_for_handle(objs, pid, h)?;
                }
                if !register_sync_waiter(waiter) {
                    return Err(STATUS_NO_MEMORY);
                }
                Ok(None)
            },
        )
        .unwrap_or(Err(STATUS_INVALID_HANDLE));

        match reg_outcome {
            Ok(Some(status)) => {
                let _ = multitask::cancel_block_current_task();
                return status;
            }
            Err(status) => {
                clear_sync_waiter(cur_tid);
                let _ = multitask::cancel_block_current_task();
                return status;
            }
            Ok(None) => {}
        }

        if let Some((tick, _)) = deadline {
            if crate::arch::rtc::ticks() >= tick {
                clear_sync_waiter(cur_tid);
                let _ = multitask::cancel_block_current_task();
                return STATUS_TIMEOUT;
            }
            if !crate::arch::rtc::arm_sleep_waiter_until_tick(cur_tid, tick) {
                clear_sync_waiter(cur_tid);
                let _ = multitask::cancel_block_current_task();
                return STATUS_NO_MEMORY;
            }
        }

        let committed = multitask::commit_block_current_task_and_yield();
        clear_sync_waiter(cur_tid);
        if deadline.is_some() {
            crate::arch::rtc::disarm_sleep_waiter(cur_tid);
        }
        if committed.is_none() {
            return STATUS_INVALID_PARAMETER;
        }
        if let Some((tick, _)) = deadline
            && crate::arch::rtc::ticks() >= tick
        {
            // One final check in case the object became signaled just as the deadline fired.
            let final_check = multitask::with_current_user_process_state_mut(|_, _, st| {
                try_satisfy_wait(st.win32_objects_mut(), cur_tid, handles, wait_all)
            })
            .unwrap_or(Ok(None));
            if let Ok(Some(status)) = final_check {
                return status;
            }
            return STATUS_TIMEOUT;
        }
    }
}

fn take_thread_alert(tid: u64) -> bool {
    let mut alerts = WIN32_THREAD_ALERTS.lock();
    for slot in alerts.iter_mut() {
        if let Some(entry) = slot.as_mut()
            && entry.tid == tid
        {
            let was = entry.alerted;
            entry.alerted = false;
            return was;
        }
    }
    false
}

fn set_thread_alert(tid: u64) {
    let mut alerts = WIN32_THREAD_ALERTS.lock();
    for slot in alerts.iter_mut() {
        if let Some(entry) = slot.as_mut()
            && entry.tid == tid
        {
            entry.alerted = true;
            return;
        }
    }
    for slot in alerts.iter_mut() {
        if slot.is_none() {
            *slot = Some(ThreadAlertFlag { tid, alerted: true });
            return;
        }
    }
}

pub(super) fn nt_alert_thread_by_thread_id(tid: u64) -> u64 {
    if tid == 0 || !multitask::is_user_task_alive(tid) {
        return STATUS_INVALID_HANDLE;
    }
    set_thread_alert(tid);
    let wakes = collect_waiters_matching(
        |w| {
            w.targets[..w.count]
                .iter()
                .any(|t| matches!(*t, WaitTarget::AlertTid { tid: wt } if wt == tid))
        },
        1,
    );
    wake_tasks(&wakes);
    STATUS_SUCCESS
}

pub(super) fn nt_test_alert() -> u64 {
    let tid = multitask::current_user_thread_id().unwrap_or(0);
    if tid != 0 && take_thread_alert(tid) {
        STATUS_ALERTED
    } else {
        STATUS_SUCCESS
    }
}

pub(super) fn nt_wait_for_alert_by_thread_id(_addr: u64, timeout_ptr: u64) -> u64 {
    let deadline = match deadline_from_nt_timeout(timeout_ptr) {
        Ok(d) => d,
        Err(status) => return status,
    };
    let cur_tid = multitask::current_user_thread_id().unwrap_or(0);
    if cur_tid == 0 {
        return STATUS_INVALID_PARAMETER;
    }

    loop {
        if take_thread_alert(cur_tid) {
            return STATUS_ALERTED;
        }
        if matches!(deadline, Some((_, true))) {
            return STATUS_TIMEOUT;
        }
        while !multitask::arm_block_current_task() {
            if let Some((tick, _)) = deadline
                && crate::arch::rtc::ticks() >= tick
            {
                return STATUS_TIMEOUT;
            }
            multitask::cond_resched();
        }
        if take_thread_alert(cur_tid) {
            let _ = multitask::cancel_block_current_task();
            return STATUS_ALERTED;
        }
        let mut targets = [WaitTarget::None; MAX_WAIT_OBJECTS];
        targets[0] = WaitTarget::AlertTid { tid: cur_tid };
        if !register_sync_waiter(SyncWaiter {
            task_id: cur_tid,
            count: 1,
            targets,
            completed: false,
        }) {
            let _ = multitask::cancel_block_current_task();
            return STATUS_NO_MEMORY;
        }
        if let Some((tick, _)) = deadline {
            if crate::arch::rtc::ticks() >= tick {
                clear_sync_waiter(cur_tid);
                let _ = multitask::cancel_block_current_task();
                return STATUS_TIMEOUT;
            }
            if !crate::arch::rtc::arm_sleep_waiter_until_tick(cur_tid, tick) {
                clear_sync_waiter(cur_tid);
                let _ = multitask::cancel_block_current_task();
                return STATUS_NO_MEMORY;
            }
        }
        let committed = multitask::commit_block_current_task_and_yield();
        clear_sync_waiter(cur_tid);
        if deadline.is_some() {
            crate::arch::rtc::disarm_sleep_waiter(cur_tid);
        }
        if committed.is_none() {
            return STATUS_INVALID_PARAMETER;
        }
        if take_thread_alert(cur_tid) {
            return STATUS_ALERTED;
        }
        if let Some((tick, _)) = deadline
            && crate::arch::rtc::ticks() >= tick
        {
            return STATUS_TIMEOUT;
        }
    }
}

pub(super) fn nt_open_keyed_event(handle_ptr: u64) -> u64 {
    if handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    write_user_u64(handle_ptr, WIN32_EVENT_HANDLE_BASE)
}

pub(super) fn nt_keyed_event_op(key: u64, timeout_ptr: u64, is_release: bool) -> u64 {
    if (key & 1) != 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let deadline = match deadline_from_nt_timeout(timeout_ptr) {
        Ok(d) => d,
        Err(status) => return status,
    };
    let pid = multitask::current_user_process_id().unwrap_or(0);
    let cur_tid = multitask::current_user_thread_id().unwrap_or(0);
    if cur_tid == 0 {
        return STATUS_INVALID_PARAMETER;
    }

    // Check if an opposite-side partner (`Wait` vs `Release`) is already waiting on `(pid, key)`.
    let partner_wakes = collect_waiters_matching(
        |w| {
            w.targets[..w.count].iter().any(|t| {
                matches!(*t, WaitTarget::Keyed { pid: wp, key: wk, release } if wp == pid && wk == key && release != is_release)
            })
        },
        1,
    );
    if !partner_wakes.is_empty() {
        wake_tasks(&partner_wakes);
        return STATUS_SUCCESS;
    }
    if matches!(deadline, Some((_, true))) {
        return STATUS_TIMEOUT;
    }

    while !multitask::arm_block_current_task() {
        if let Some((tick, _)) = deadline
            && crate::arch::rtc::ticks() >= tick
        {
            return STATUS_TIMEOUT;
        }
        multitask::cond_resched();
    }
    let partner_wakes = collect_waiters_matching(
        |w| {
            w.targets[..w.count].iter().any(|t| {
                matches!(*t, WaitTarget::Keyed { pid: wp, key: wk, release } if wp == pid && wk == key && release != is_release)
            })
        },
        1,
    );
    if !partner_wakes.is_empty() {
        let _ = multitask::cancel_block_current_task();
        wake_tasks(&partner_wakes);
        return STATUS_SUCCESS;
    }
    let mut targets = [WaitTarget::None; MAX_WAIT_OBJECTS];
    targets[0] = WaitTarget::Keyed {
        pid,
        key,
        release: is_release,
    };
    if !register_sync_waiter(SyncWaiter {
        task_id: cur_tid,
        count: 1,
        targets,
        completed: false,
    }) {
        let _ = multitask::cancel_block_current_task();
        return STATUS_NO_MEMORY;
    }
    if let Some((tick, _)) = deadline {
        if !crate::arch::rtc::arm_sleep_waiter_until_tick(cur_tid, tick) {
            clear_sync_waiter(cur_tid);
            let _ = multitask::cancel_block_current_task();
            return STATUS_NO_MEMORY;
        }
    }
    let _ = multitask::commit_block_current_task_and_yield();
    let still_waiting = {
        let mut table = WIN32_SYNC_WAITERS.lock();
        let mut found_incomplete = false;
        for slot in table.iter_mut() {
            if let Some(w) = slot.as_ref()
                && w.task_id == cur_tid
            {
                found_incomplete = !w.completed;
                *slot = None;
            }
        }
        found_incomplete
    };
    if deadline.is_some() {
        crate::arch::rtc::disarm_sleep_waiter(cur_tid);
    }
    if still_waiting
        && deadline.is_some_and(|(tick, _)| crate::arch::rtc::ticks() >= tick)
    {
        return STATUS_TIMEOUT;
    }
    STATUS_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_handle_satisfaction_tracks_event_mutant_and_semaphore() {
        let mut objs = Win32ProcessObjects::default();
        objs.events.push(Win32EventObject {
            handle: WIN32_EVENT_HANDLE_BASE + 1,
            signaled: true,
            manual_reset: false,
            name: None,
        });
        objs.mutants.push(Win32MutantObject {
            handle: WIN32_MUTANT_HANDLE_BASE + 1,
            owner_tid: None,
            recursion_count: 0,
            abandoned: false,
            name: None,
        });
        objs.semaphores.push(Win32SemaphoreObject {
            handle: WIN32_SEMAPHORE_HANDLE_BASE + 1,
            count: 2,
            max_count: 2,
            name: None,
        });

        let handles = [
            WIN32_EVENT_HANDLE_BASE + 1,
            WIN32_MUTANT_HANDLE_BASE + 1,
            WIN32_SEMAPHORE_HANDLE_BASE + 1,
        ];
        assert_eq!(try_satisfy_wait(&mut objs, 42, &handles, true), Ok(Some(STATUS_WAIT_0)));
        assert!(!objs.events[0].signaled);
        assert_eq!(objs.mutants[0].owner_tid, Some(42));
        assert_eq!(objs.mutants[0].recursion_count, 1);
        assert_eq!(objs.semaphores[0].count, 1);

        // WaitAll must now fail because auto-reset event was consumed.
        assert_eq!(try_satisfy_wait(&mut objs, 42, &handles, true), Ok(None));
        // Another thread cannot take mutant held by tid 42.
        assert_eq!(
            try_satisfy_wait(&mut objs, 99, &[WIN32_MUTANT_HANDLE_BASE + 1], false),
            Ok(None)
        );
        // Same thread acquires mutant recursively.
        assert_eq!(
            try_satisfy_wait(&mut objs, 42, &[WIN32_MUTANT_HANDLE_BASE + 1], false),
            Ok(Some(STATUS_WAIT_0))
        );
        assert_eq!(objs.mutants[0].recursion_count, 2);
    }
}
