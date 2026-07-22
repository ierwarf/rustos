use alloc::boxed::Box;
use core::ptr::NonNull;

use spin::Mutex;

use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;

const MAX_PROCESS_OBJECTS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessHandle {
    index: usize,
    generation: u32,
}

impl ProcessHandle {
    pub const fn new(index: usize, generation: u32) -> Self {
        Self { index, generation }
    }

    pub const fn index(self) -> usize {
        self.index
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

pub struct ProcessRef {
    handle: ProcessHandle,
    process_id: u64,
    state_ptr: NonNull<Mutex<UserProcessState>>,
}

impl ProcessRef {
    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    pub fn with_state<R>(&self, f: impl FnOnce(u64, &UserProcessState) -> R) -> R {
        let state = unsafe { self.state_ptr.as_ref() }.lock();
        f(self.process_id, &state)
    }

    pub fn with_state_mut<R>(&self, f: impl FnOnce(u64, &mut UserProcessState) -> R) -> R {
        let mut state = unsafe { self.state_ptr.as_ref() }.lock();
        f(self.process_id, &mut state)
    }
}

impl Drop for ProcessRef {
    fn drop(&mut self) {
        release_process_ref(self.handle);
    }
}

struct ProcessObject {
    process_id: u64,
    parent_process_id: Option<u64>,
    ref_count: usize,
    thread_count: usize,
    mm_generation: u32,
    exiting: bool,
    queued_for_reap: bool,
    exit_status: Option<i32>,
    waited: bool,
    state: Mutex<UserProcessState>,
}

impl ProcessObject {
    fn new(process_id: u64, parent_process_id: Option<u64>, state: UserProcessState) -> Self {
        Self {
            process_id,
            parent_process_id,
            ref_count: 1,
            thread_count: 1,
            mm_generation: 1,
            exiting: false,
            queued_for_reap: false,
            exit_status: None,
            waited: parent_process_id.is_none(),
            state: Mutex::new(state),
        }
    }

    fn state_ptr(&self) -> NonNull<Mutex<UserProcessState>> {
        NonNull::from(&self.state)
    }
}

struct ProcessSlot {
    generation: u32,
    object: Option<Box<ProcessObject>>,
}

impl ProcessSlot {
    const fn empty() -> Self {
        Self {
            generation: 1,
            object: None,
        }
    }
}

struct ProcessTable {
    slots: [ProcessSlot; MAX_PROCESS_OBJECTS],
    reap_queue: [Option<ProcessHandle>; MAX_PROCESS_OBJECTS],
    reap_len: usize,
    reap_scan_pending: bool,
}

impl ProcessTable {
    const fn new() -> Self {
        Self {
            slots: [const { ProcessSlot::empty() }; MAX_PROCESS_OBJECTS],
            reap_queue: [None; MAX_PROCESS_OBJECTS],
            reap_len: 0,
            reap_scan_pending: false,
        }
    }

    fn lookup_slot_mut(&mut self, handle: ProcessHandle) -> Option<&mut ProcessSlot> {
        let slot = self.slots.get_mut(handle.index())?;
        (slot.generation == handle.generation()).then_some(slot)
    }

    fn lookup_object_mut(&mut self, handle: ProcessHandle) -> Option<&mut ProcessObject> {
        self.lookup_slot_mut(handle)?.object.as_deref_mut()
    }

    fn next_generation(current: u32) -> u32 {
        let next = current.wrapping_add(1);
        if next == 0 { 1 } else { next }
    }

    fn push_reap_handle(&mut self, handle: ProcessHandle) {
        if self.reap_len >= self.reap_queue.len() {
            self.reap_scan_pending = true;
            return;
        }
        self.reap_queue[self.reap_len] = Some(handle);
        self.reap_len += 1;
    }
}

static PROCESS_TABLE: Mutex<ProcessTable> = Mutex::new(ProcessTable::new());

fn reclaim_slot(slot: &mut ProcessSlot) -> Option<Box<ProcessObject>> {
    let object = slot.object.as_mut()?;
    if object.ref_count != 0 || object.thread_count != 0 {
        object.queued_for_reap = false;
        return None;
    }
    if object.exit_status.is_some() && !object.waited {
        object.queued_for_reap = false;
        return None;
    }

    let object = slot.object.take()?;
    slot.generation = ProcessTable::next_generation(slot.generation);
    Some(object)
}

pub fn create_process(process_id: u64, state: UserProcessState) -> Option<ProcessHandle> {
    create_process_with_parent(process_id, None, state)
}

pub fn create_process_with_parent(
    process_id: u64,
    parent_process_id: Option<u64>,
    state: UserProcessState,
) -> Option<ProcessHandle> {
    let mut table = PROCESS_TABLE.lock();
    let object = Box::new(ProcessObject::new(process_id, parent_process_id, state));
    let (index, slot) = table
        .slots
        .iter_mut()
        .enumerate()
        .find(|(_, slot)| slot.object.is_none())?;
    slot.object = Some(object);
    let handle = ProcessHandle::new(index, slot.generation);
    Some(handle)
}

pub fn attach_task(handle: ProcessHandle) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    if object.exiting {
        return None;
    }
    let ref_count = object.ref_count.checked_add(1)?;
    let thread_count = object.thread_count.checked_add(1)?;
    object.ref_count = ref_count;
    object.thread_count = thread_count;
    Some(())
}

pub fn detach_task(handle: ProcessHandle) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let should_queue = {
        let object = table.lookup_object_mut(handle)?;
        if object.thread_count == 0 || object.ref_count == 0 {
            return Some(());
        }
        object.thread_count -= 1;
        object.ref_count -= 1;
        if object.thread_count == 0 {
            object.exiting = true;
        }
        let should_queue =
            object.thread_count == 0 && object.ref_count == 0 && !object.queued_for_reap;
        if should_queue {
            object.queued_for_reap = true;
        }
        should_queue
    };
    if should_queue {
        table.push_reap_handle(handle);
    }
    Some(())
}

pub fn retain_process(handle: ProcessHandle) -> Option<ProcessRef> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    object.ref_count = object.ref_count.checked_add(1)?;
    let process_id = object.process_id;
    let state_ptr = object.state_ptr();
    Some(ProcessRef {
        handle,
        process_id,
        state_ptr,
    })
}

pub fn retain_process_by_pid(process_id: u64) -> Option<ProcessRef> {
    let mut table = PROCESS_TABLE.lock();
    for (index, slot) in table.slots.iter_mut().enumerate() {
        let generation = slot.generation;
        let Some(object) = slot.object.as_deref_mut() else {
            continue;
        };
        if object.process_id != process_id {
            continue;
        }
        object.ref_count = object.ref_count.checked_add(1)?;
        let state_ptr = object.state_ptr();
        return Some(ProcessRef {
            handle: ProcessHandle::new(index, generation),
            process_id,
            state_ptr,
        });
    }
    None
}

pub fn release_process_ref(handle: ProcessHandle) {
    let mut table = PROCESS_TABLE.lock();
    let should_queue = {
        let Some(object) = table.lookup_object_mut(handle) else {
            return;
        };
        if object.ref_count == 0 {
            return;
        }
        object.ref_count -= 1;
        let should_queue =
            object.thread_count == 0 && object.ref_count == 0 && !object.queued_for_reap;
        if should_queue {
            object.queued_for_reap = true;
        }
        should_queue
    };
    if should_queue {
        table.push_reap_handle(handle);
    }
}

pub fn parent_process_id_of(process_id: u64) -> Option<u64> {
    let table = PROCESS_TABLE.lock();
    for slot in table.slots.iter() {
        let Some(object) = slot.object.as_deref() else {
            continue;
        };
        if object.process_id == process_id {
            return object.parent_process_id;
        }
    }
    None
}

pub enum WaitResult {
    Exited { pid: u64, status: i32 },
    Pending,
    NoMatchingChild,
}

pub fn with_process_state<R>(
    handle: ProcessHandle,
    f: impl FnOnce(u64, &UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process(handle)?;
    Some(process.with_state(f))
}

pub fn with_process_state_mut<R>(
    handle: ProcessHandle,
    f: impl FnOnce(u64, &mut UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process(handle)?;
    Some(process.with_state_mut(f))
}

pub fn with_process_state_by_pid_mut<R>(
    process_id: u64,
    f: impl FnOnce(&mut UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process_by_pid(process_id)?;
    Some(process.with_state_mut(|_, state| f(state)))
}

pub fn with_process_state_by_pid<R>(
    process_id: u64,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process_by_pid(process_id)?;
    Some(process.with_state(|_, state| f(state)))
}

pub fn replace_for_exec(
    handle: ProcessHandle,
    address_space: ProcessAddressSpace,
    linux_process_state: crate::user::linux::LinuxProcessState,
    linux_memory_map: crate::user::linux::LinuxMemoryMapState,
    linux_runtime_profile: crate::user::linux::LinuxRuntimeProfile,
    exec_path: &str,
) -> Option<alloc::vec::Vec<crate::user::handles::KernelHandle>> {
    let process = retain_process(handle)?;
    process.with_state_mut(|_, state| {
        // State replacement and the process-table exit marker are one
        // transaction. If exit won the table lock, a stale exec preparation
        // must not mutate the address space or clear the exit decision.
        let mut table = PROCESS_TABLE.lock();
        let object = table.lookup_object_mut(handle)?;
        if !exec_may_replace(object) {
            return None;
        }
        let closed = state.replace_for_exec(
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            exec_path,
        );
        object.mm_generation = ProcessTable::next_generation(object.mm_generation);
        object.queued_for_reap = false;
        Some(closed)
    })
}

fn exec_may_replace(object: &ProcessObject) -> bool {
    !object.exiting
}

pub fn note_process_exit_status(process_id: u64, status: i32) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id)?;
    object.exit_status = Some(status);
    object.exiting = true;
    Some(())
}

/// Mark a process as exiting before callers tear down resources that it owns.
///
/// The process object remains available until its tasks retire and its parent
/// reaps it, but new authority publication must fail once this bit is set.
pub fn mark_process_exiting(process_id: u64) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id)?;
    object.exiting = true;
    Some(())
}

/// Atomically marks the process exiting and reports whether this call won the
/// transition. Resource owners use the `true` result to perform exactly-once
/// service-side open-description teardown under concurrent exit_group calls.
pub fn mark_process_exiting_once(process_id: u64) -> Option<bool> {
    let mut table = PROCESS_TABLE.lock();
    let object = table
        .slots
        .iter_mut()
        .filter_map(|slot| slot.object.as_deref_mut())
        .find(|object| object.process_id == process_id)?;
    let first = !object.exiting;
    object.exiting = true;
    Some(first)
}

pub fn is_process_exiting(process_id: u64) -> Option<bool> {
    let table = PROCESS_TABLE.lock();
    table
        .slots
        .iter()
        .filter_map(|slot| slot.object.as_deref())
        .find(|object| object.process_id == process_id)
        .map(|object| object.exiting)
}

pub fn thread_count_by_pid(process_id: u64) -> Option<usize> {
    let table = PROCESS_TABLE.lock();
    table
        .slots
        .iter()
        .filter_map(|slot| slot.object.as_deref())
        .find(|object| object.process_id == process_id)
        .map(|object| object.thread_count)
}

pub fn wait_for_child(parent_process_id: u64, target_pid: i64) -> WaitResult {
    let mut table = PROCESS_TABLE.lock();
    let mut saw_child = false;
    let mut queued_handle = None;
    let mut exited = None;

    for (index, slot) in table.slots.iter_mut().enumerate() {
        let Some(object) = slot.object.as_deref_mut() else {
            continue;
        };
        if object.parent_process_id != Some(parent_process_id) {
            continue;
        }
        let pid = object.process_id;
        let matches = match target_pid {
            -1 => true,
            pid_filter if pid_filter > 0 => pid == pid_filter as u64,
            _ => false,
        };
        if !matches {
            continue;
        }

        saw_child = true;
        let Some(status) = object.exit_status else {
            continue;
        };
        object.waited = true;
        if object.thread_count == 0 && object.ref_count == 0 && !object.queued_for_reap {
            object.queued_for_reap = true;
            queued_handle = Some(ProcessHandle::new(index, slot.generation));
        }
        exited = Some(WaitResult::Exited { pid, status });
        break;
    }

    if let Some(handle) = queued_handle {
        table.push_reap_handle(handle);
    }

    if let Some(result) = exited {
        return result;
    }

    if saw_child {
        return WaitResult::Pending;
    }
    WaitResult::NoMatchingChild
}

pub fn reap_exited_processes() -> usize {
    let mut reaped = 0usize;
    let mut reclaimed: [Option<Box<ProcessObject>>; MAX_PROCESS_OBJECTS] =
        [const { None }; MAX_PROCESS_OBJECTS];
    let mut reclaimed_len = 0usize;

    {
        let mut table = PROCESS_TABLE.lock();
        let queue = core::mem::replace(&mut table.reap_queue, [None; MAX_PROCESS_OBJECTS]);
        let queue_len = core::mem::replace(&mut table.reap_len, 0);
        let scan_all = core::mem::replace(&mut table.reap_scan_pending, false);
        for handle in queue.into_iter().take(queue_len).flatten() {
            let Some(slot) = table.lookup_slot_mut(handle) else {
                continue;
            };
            let Some(object) = reclaim_slot(slot) else {
                continue;
            };
            reclaimed[reclaimed_len] = Some(object);
            reclaimed_len += 1;
            reaped += 1;
        }
        if scan_all {
            for slot in &mut table.slots {
                let Some(object) = reclaim_slot(slot) else {
                    continue;
                };
                reclaimed[reclaimed_len] = Some(object);
                reclaimed_len += 1;
                reaped += 1;
            }
        }
    }
    drop(reclaimed);
    reaped
}

#[cfg(test)]
mod tests {
    use super::{
        ProcessObject, attach_task, create_process, detach_task, is_process_exiting,
        mark_process_exiting, reap_exited_processes, retain_process, thread_count_by_pid,
    };
    use crate::memory::paging::ProcessAddressSpace;
    use crate::user::process_state::UserProcessState;

    // These tests exercise the one global process table and call the global
    // reaper. Running them concurrently lets one test reap another test's
    // exited slot, turning exact lifecycle assertions into scheduler-order
    // flakes. Keep only this shared-state group serialized.
    static PROCESS_TABLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn new_state() -> UserProcessState {
        UserProcessState::new(
            ProcessAddressSpace::empty_for_tests(),
            None,
            None,
            None,
            None,
            false,
            "/test.elf",
        )
    }

    #[test]
    fn retained_ref_delays_reclaim_until_drop() {
        let _guard = PROCESS_TABLE_TEST_LOCK
            .lock()
            .expect("process table test lock");
        let handle = create_process(42, new_state()).expect("process handle");
        let retained = retain_process(handle).expect("retained process");
        detach_task(handle).expect("detach");
        assert_eq!(reap_exited_processes(), 0);
        drop(retained);
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn process_address_space_and_exec_exit_are_serialized() {
        let mut object = ProcessObject::new(7, None, new_state());
        let held = object.state.lock();
        assert!(object.state.try_lock().is_none());
        drop(held);
        assert!(object.state.try_lock().is_some());
        assert!(super::exec_may_replace(&object));
        object.exiting = true;
        assert!(!super::exec_may_replace(&object));
    }

    #[test]
    fn leader_thread_retirement_does_not_mark_live_process_exited() {
        let _guard = PROCESS_TABLE_TEST_LOCK
            .lock()
            .expect("process table test lock");
        let handle = create_process(43, new_state()).expect("process handle");
        attach_task(handle).expect("second thread");
        assert_eq!(thread_count_by_pid(43), Some(2));

        detach_task(handle).expect("leader detach");

        assert_eq!(thread_count_by_pid(43), Some(1));
        assert_eq!(is_process_exiting(43), Some(false));
        detach_task(handle).expect("last thread detach");
        assert_eq!(is_process_exiting(43), Some(true));
        assert_eq!(reap_exited_processes(), 1);
    }

    #[test]
    fn exiting_process_rejects_new_thread_attachment() {
        let _guard = PROCESS_TABLE_TEST_LOCK
            .lock()
            .expect("process table test lock");
        let handle = create_process(44, new_state()).expect("process handle");
        mark_process_exiting(44).expect("mark exiting");

        assert_eq!(attach_task(handle), None);
        assert_eq!(thread_count_by_pid(44), Some(1));
        assert_eq!(is_process_exiting(44), Some(true));

        detach_task(handle).expect("detach final thread");
        assert_eq!(reap_exited_processes(), 1);
    }
}
