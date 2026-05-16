use alloc::boxed::Box;
use core::cell::UnsafeCell;
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
    state_ptr: NonNull<UserProcessState>,
}

impl ProcessRef {
    pub const fn process_id(&self) -> u64 {
        self.process_id
    }

    pub fn state(&self) -> &UserProcessState {
        unsafe { self.state_ptr.as_ref() }
    }

    pub fn state_mut(&mut self) -> &mut UserProcessState {
        unsafe {
            self.state_ptr
                .as_ptr()
                .as_mut()
                .expect("process state pointer must be valid")
        }
    }

    pub fn with_state<R>(&self, f: impl FnOnce(u64, &UserProcessState) -> R) -> R {
        f(self.process_id, self.state())
    }

    pub fn with_state_mut<R>(&mut self, f: impl FnOnce(u64, &mut UserProcessState) -> R) -> R {
        f(self.process_id, self.state_mut())
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
    state: UnsafeCell<UserProcessState>,
}

unsafe impl Send for ProcessObject {}
unsafe impl Sync for ProcessObject {}

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
            state: UnsafeCell::new(state),
        }
    }

    fn state_ptr(&self) -> NonNull<UserProcessState> {
        NonNull::new(self.state.get()).expect("process state pointer must not be null")
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

    fn lookup_slot(&self, handle: ProcessHandle) -> Option<&ProcessSlot> {
        let slot = self.slots.get(handle.index())?;
        (slot.generation == handle.generation()).then_some(slot)
    }

    fn lookup_slot_mut(&mut self, handle: ProcessHandle) -> Option<&mut ProcessSlot> {
        let slot = self.slots.get_mut(handle.index())?;
        (slot.generation == handle.generation()).then_some(slot)
    }

    fn lookup_object(&self, handle: ProcessHandle) -> Option<&ProcessObject> {
        self.lookup_slot(handle)?.object.as_deref()
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
    let ref_count = object.ref_count.checked_add(1)?;
    let thread_count = object.thread_count.checked_add(1)?;
    object.ref_count = ref_count;
    object.thread_count = thread_count;
    object.exiting = false;
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

pub fn process_id(handle: ProcessHandle) -> Option<u64> {
    let table = PROCESS_TABLE.lock();
    Some(table.lookup_object(handle)?.process_id)
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
    let process_id = process.process_id();
    Some(f(process_id, process.state()))
}

pub fn with_process_state_mut<R>(
    handle: ProcessHandle,
    f: impl FnOnce(u64, &mut UserProcessState) -> R,
) -> Option<R> {
    let mut process = retain_process(handle)?;
    let process_id = process.process_id();
    Some(f(process_id, process.state_mut()))
}

pub fn with_process_state_by_pid_mut<R>(
    process_id: u64,
    f: impl FnOnce(&mut UserProcessState) -> R,
) -> Option<R> {
    let mut process = retain_process_by_pid(process_id)?;
    Some(f(process.state_mut()))
}

pub fn with_process_state_by_pid<R>(
    process_id: u64,
    f: impl FnOnce(&UserProcessState) -> R,
) -> Option<R> {
    let process = retain_process_by_pid(process_id)?;
    Some(f(process.state()))
}

pub fn replace_for_exec(
    handle: ProcessHandle,
    address_space: ProcessAddressSpace,
    linux_process_state: crate::user::linux::LinuxProcessState,
    linux_memory_map: crate::user::linux::LinuxMemoryMapState,
    linux_runtime_profile: crate::user::linux::LinuxRuntimeProfile,
    exec_path: &str,
) -> Option<()> {
    let mut table = PROCESS_TABLE.lock();
    let object = table.lookup_object_mut(handle)?;
    unsafe {
        (*object.state.get()).replace_for_exec(
            address_space,
            linux_process_state,
            linux_memory_map,
            linux_runtime_profile,
            exec_path,
        );
    }
    object.mm_generation = ProcessTable::next_generation(object.mm_generation);
    object.exiting = false;
    object.queued_for_reap = false;
    Some(())
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
    use super::{create_process, detach_task, reap_exited_processes, retain_process};
    use crate::memory::paging::ProcessAddressSpace;
    use crate::user::process_state::UserProcessState;

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
        let handle = create_process(42, new_state()).expect("process handle");
        let retained = retain_process(handle).expect("retained process");
        detach_task(handle).expect("detach");
        assert_eq!(reap_exited_processes(), 0);
        drop(retained);
        assert_eq!(reap_exited_processes(), 1);
    }
}
