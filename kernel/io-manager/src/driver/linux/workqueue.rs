use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::mem::offset_of;
use core::ptr;

use spin::Mutex;

use super::runtime;

const WORK_STRUCT_PENDING: u64 = 1 << 0;

#[repr(C)]
struct LinuxListHead {
    next: *mut LinuxListHead,
    prev: *mut LinuxListHead,
}

#[repr(C)]
struct LinuxHListNode {
    next: *mut LinuxHListNode,
    pprev: *mut *mut LinuxHListNode,
}

type LinuxWorkFunc = unsafe extern "C" fn(work: *mut LinuxWorkStruct);
type LinuxTimerFunc = unsafe extern "C" fn(timer: *mut LinuxTimerList);

#[repr(C)]
pub(crate) struct LinuxWorkStruct {
    data: u64,
    entry: LinuxListHead,
    func: Option<LinuxWorkFunc>,
}

#[repr(C)]
pub(crate) struct LinuxTimerList {
    entry: LinuxHListNode,
    expires: u64,
    function: Option<LinuxTimerFunc>,
    flags: u32,
}

#[repr(C)]
struct LinuxWorkqueueStruct {
    magic: u64,
    flags: u32,
    max_active: i32,
}

#[repr(C)]
pub(crate) struct LinuxDelayedWork {
    work: LinuxWorkStruct,
    timer: LinuxTimerList,
    wq: *mut LinuxWorkqueueStruct,
    cpu: i32,
}

#[derive(Clone, Copy)]
struct WorkqueueRecord {
    ptr: usize,
}

#[derive(Clone, Copy)]
struct PendingWork {
    wq: usize,
    work: usize,
}

#[derive(Clone, Copy)]
struct PendingTimer {
    timer: usize,
    expires: u64,
}

static WORKQUEUES: Mutex<Vec<WorkqueueRecord>> = Mutex::new(Vec::new());
static PENDING_WORK: Mutex<Vec<PendingWork>> = Mutex::new(Vec::new());
static PENDING_TIMERS: Mutex<Vec<PendingTimer>> = Mutex::new(Vec::new());
static SYSTEM_WORKQUEUE: LinuxWorkqueueStruct = LinuxWorkqueueStruct {
    magic: 0x5255_5354_4f53_5751,
    flags: 0,
    max_active: 0,
};

pub(crate) unsafe extern "C" fn alloc_workqueue(
    _fmt: *const c_char,
    flags: u32,
    max_active: i32,
) -> *mut c_void {
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "linux alloc_workqueue: begin fmt={:#x} flags={:#x} max_active={}",
            _fmt as usize,
            flags,
            max_active
        )
        .as_bytes(),
    );
    let wq = Box::new(LinuxWorkqueueStruct {
        magic: 0x5255_5354_4f53_5751,
        flags,
        max_active,
    });
    let ptr = Box::into_raw(wq);
    crate::debug::write_debugcon_only_line(
        alloc::format!("linux alloc_workqueue: boxed ptr={:#x}", ptr as usize).as_bytes(),
    );
    crate::debug::write_debugcon_only_line(b"linux alloc_workqueue: workqueues lock begin");
    let mut workqueues = WORKQUEUES.lock();
    crate::debug::write_debugcon_only_line(b"linux alloc_workqueue: workqueues lock acquired");
    workqueues.push(WorkqueueRecord { ptr: ptr as usize });
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "linux alloc_workqueue: workqueues push done len={}",
            workqueues.len()
        )
        .as_bytes(),
    );
    drop(workqueues);
    crate::debug::write_debugcon_only_line(b"linux alloc_workqueue: workqueues lock released");
    crate::debug::write_debugcon_only_line(
        alloc::format!(
            "linux alloc_workqueue: ptr={:#x} flags={:#x} max_active={}",
            ptr as usize,
            flags,
            max_active
        )
        .as_bytes(),
    );
    ptr.cast()
}

pub(crate) unsafe extern "C" fn destroy_workqueue(wq: *mut c_void) {
    let Some(wq_ptr) = ptr_to_usize(wq) else {
        return;
    };

    flush_matching_queue(wq_ptr);
    cancel_queue_timers(wq_ptr);

    {
        let mut workqueues = WORKQUEUES.lock();
        if let Some(index) = workqueues.iter().position(|entry| entry.ptr == wq_ptr) {
            workqueues.remove(index);
        }
    }

    unsafe {
        drop(Box::from_raw(wq.cast::<LinuxWorkqueueStruct>()));
    }
}

pub(crate) unsafe extern "C" fn __flush_workqueue(wq: *mut c_void) {
    let Some(wq_ptr) = ptr_to_usize(wq) else {
        return;
    };
    flush_matching_queue(wq_ptr);
}

pub(crate) unsafe extern "C" fn queue_work_on(
    _cpu: i32,
    wq: *mut c_void,
    work: *mut LinuxWorkStruct,
) -> bool {
    let Some(wq_ptr) = ptr_to_usize(wq) else {
        return false;
    };
    let Some(work_ptr) = ptr_to_usize(work.cast::<c_void>()) else {
        return false;
    };
    enqueue_work(wq_ptr, work_ptr, false)
}

pub(crate) unsafe extern "C" fn queue_delayed_work_on(
    cpu: i32,
    wq: *mut c_void,
    dwork: *mut LinuxDelayedWork,
    delay: u64,
) -> bool {
    let Some(wq_ptr) = ptr_to_usize(wq) else {
        return false;
    };
    let Some(_dwork_ptr) = ptr_to_usize(dwork.cast::<c_void>()) else {
        return false;
    };

    let delayed = unsafe { &mut *dwork };
    let work_ptr = ptr_to_usize((&mut delayed.work as *mut LinuxWorkStruct).cast::<c_void>())
        .expect("non-null delayed work");
    if work_is_pending(work_ptr) || timer_is_pending(timer_ptr(&mut delayed.timer)) {
        return false;
    }

    delayed.wq = wq.cast::<LinuxWorkqueueStruct>();
    delayed.cpu = cpu;
    set_work_pending(work_ptr, true);

    if delay == 0 {
        return enqueue_work(wq_ptr, work_ptr, true);
    }

    let timer_ptr = timer_ptr(&mut delayed.timer);
    delayed.timer.function = Some(delayed_work_timer_fn);
    delayed.timer.expires = runtime::current_jiffies().saturating_add(delay);
    mark_timer_pending(timer_ptr, true);
    PENDING_TIMERS.lock().push(PendingTimer {
        timer: timer_ptr,
        expires: delayed.timer.expires,
    });
    true
}

pub(crate) unsafe extern "C" fn delayed_work_timer_fn(timer: *mut LinuxTimerList) {
    if timer.is_null() {
        return;
    }

    let delayed_work = unsafe { delayed_work_from_timer(timer) };
    let wq_ptr = delayed_work.wq as usize;
    let work_ptr = (&mut delayed_work.work as *mut LinuxWorkStruct) as usize;
    let _ = enqueue_work(wq_ptr, work_ptr, true);
}

pub(crate) unsafe extern "C" fn init_timer_key(
    timer: *mut LinuxTimerList,
    func: Option<LinuxTimerFunc>,
    flags: u32,
    _name: *const c_char,
    _key: *mut c_void,
) {
    if timer.is_null() {
        return;
    }

    unsafe {
        (*timer).entry.next = ptr::null_mut();
        (*timer).entry.pprev = ptr::null_mut();
        (*timer).expires = 0;
        (*timer).function = func;
        (*timer).flags = flags;
    }
}

pub(crate) unsafe extern "C" fn mod_timer(timer: *mut LinuxTimerList, expires: u64) -> i32 {
    let Some(timer_ptr) = ptr_to_usize(timer.cast::<c_void>()) else {
        return 0;
    };

    let was_pending = timer_is_pending(timer_ptr);
    unsafe {
        (*timer).expires = expires;
    }
    mark_timer_pending(timer_ptr, true);

    let mut timers = PENDING_TIMERS.lock();
    if let Some(entry) = timers.iter_mut().find(|entry| entry.timer == timer_ptr) {
        entry.expires = expires;
    } else {
        timers.push(PendingTimer {
            timer: timer_ptr,
            expires,
        });
    }
    was_pending as i32
}

pub(crate) unsafe extern "C" fn cancel_work_sync(work: *mut LinuxWorkStruct) -> bool {
    let Some(work_ptr) = ptr_to_usize(work.cast::<c_void>()) else {
        return false;
    };
    let removed = {
        let mut pending = PENDING_WORK.lock();
        if let Some(index) = pending.iter().position(|entry| entry.work == work_ptr) {
            pending.swap_remove(index);
            true
        } else {
            false
        }
    };
    if removed {
        set_work_pending(work_ptr, false);
    }
    removed
}

pub(crate) unsafe extern "C" fn flush_work(work: *mut LinuxWorkStruct) -> bool {
    let Some(work_ptr) = ptr_to_usize(work.cast::<c_void>()) else {
        return false;
    };
    let mut flushed = false;
    loop {
        let serviced = service_queued_work(None);
        if serviced == 0 {
            break;
        }
        flushed = true;
        if !work_is_pending(work_ptr) {
            break;
        }
    }
    flushed
}

pub(crate) unsafe extern "C" fn cancel_delayed_work_sync(dwork: *mut LinuxDelayedWork) -> bool {
    if dwork.is_null() {
        return false;
    }
    let delayed = unsafe { &mut *dwork };
    let timer_removed = delete_timer(&mut delayed.timer as *mut LinuxTimerList) != 0;
    let work_removed = unsafe { cancel_work_sync(&mut delayed.work as *mut LinuxWorkStruct) };
    timer_removed || work_removed
}

pub(crate) unsafe extern "C" fn timer_init_key(
    timer: *mut LinuxTimerList,
    func: Option<LinuxTimerFunc>,
    flags: u32,
    name: *const c_char,
    key: *mut c_void,
) {
    unsafe { init_timer_key(timer, func, flags, name, key) };
}

pub(crate) unsafe extern "C" fn timer_delete(timer: *mut LinuxTimerList) -> i32 {
    delete_timer(timer)
}

pub(crate) unsafe extern "C" fn timer_shutdown_sync(timer: *mut LinuxTimerList) -> i32 {
    delete_timer(timer)
}

pub(crate) unsafe extern "C" fn timer_delete_sync(timer: *mut LinuxTimerList) -> i32 {
    delete_timer(timer)
}

pub(crate) fn service_pending() -> usize {
    let mut serviced = 0;
    loop {
        let mut progress = 0;
        progress += service_expired_timers();
        progress += service_queued_work(None);
        serviced += progress;
        if progress == 0 {
            break;
        }
    }
    serviced
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "alloc_workqueue" => Some(alloc_workqueue as *const () as usize),
        "destroy_workqueue" => Some(destroy_workqueue as *const () as usize),
        "__flush_workqueue" => Some(__flush_workqueue as *const () as usize),
        "queue_work_on" => Some(queue_work_on as *const () as usize),
        "queue_delayed_work_on" => Some(queue_delayed_work_on as *const () as usize),
        "delayed_work_timer_fn" => Some(delayed_work_timer_fn as *const () as usize),
        "init_timer_key" => Some(init_timer_key as *const () as usize),
        "timer_init_key" => Some(timer_init_key as *const () as usize),
        "mod_timer" => Some(mod_timer as *const () as usize),
        "cancel_work_sync" => Some(cancel_work_sync as *const () as usize),
        "cancel_delayed_work_sync" => Some(cancel_delayed_work_sync as *const () as usize),
        "flush_work" => Some(flush_work as *const () as usize),
        "timer_delete" => Some(timer_delete as *const () as usize),
        "timer_delete_sync" => Some(timer_delete_sync as *const () as usize),
        "timer_shutdown_sync" => Some(timer_shutdown_sync as *const () as usize),
        "system_wq" => Some(&SYSTEM_WORKQUEUE as *const LinuxWorkqueueStruct as usize),
        _ => None,
    }
}

fn enqueue_work(wq_ptr: usize, work_ptr: usize, allow_existing_pending: bool) -> bool {
    if wq_ptr == 0 || work_ptr == 0 {
        return false;
    }

    if !allow_existing_pending && work_is_pending(work_ptr) {
        return false;
    }

    let mut pending = PENDING_WORK.lock();
    if pending.iter().any(|entry| entry.work == work_ptr) {
        return false;
    }

    if !allow_existing_pending {
        set_work_pending(work_ptr, true);
    }
    pending.push(PendingWork {
        wq: wq_ptr,
        work: work_ptr,
    });
    true
}

fn flush_matching_queue(wq_ptr: usize) {
    loop {
        if service_queued_work(Some(wq_ptr)) == 0 {
            break;
        }
    }
}

fn cancel_queue_timers(wq_ptr: usize) {
    let removed = {
        let mut timers = PENDING_TIMERS.lock();
        let mut removed = Vec::new();
        let mut index = 0;
        while index < timers.len() {
            let timer_ptr = timers[index].timer;
            let Some(queue_ptr) = delayed_work_queue_from_timer(timer_ptr) else {
                index += 1;
                continue;
            };
            if queue_ptr != wq_ptr {
                index += 1;
                continue;
            }
            removed.push(timer_ptr);
            timers.swap_remove(index);
        }
        removed
    };

    for timer_ptr in removed {
        mark_timer_pending(timer_ptr, false);
        if let Some(work_ptr) = delayed_work_item_from_timer(timer_ptr) {
            set_work_pending(work_ptr, false);
        }
    }
}

fn service_expired_timers() -> usize {
    let now = runtime::current_jiffies();
    let expired = {
        let mut timers = PENDING_TIMERS.lock();
        let mut expired = Vec::new();
        let mut index = 0;
        while index < timers.len() {
            if timers[index].expires > now {
                index += 1;
                continue;
            }
            expired.push(timers.swap_remove(index).timer);
        }
        expired
    };

    for timer_ptr in expired.iter().copied() {
        mark_timer_pending(timer_ptr, false);
    }

    for timer_ptr in expired.iter().copied() {
        let timer = unsafe { &mut *(timer_ptr as *mut LinuxTimerList) };
        if let Some(func) = timer.function {
            unsafe {
                func(timer);
            }
        }
    }

    expired.len()
}

fn service_queued_work(filter_wq: Option<usize>) -> usize {
    let pending = {
        let mut queued = PENDING_WORK.lock();
        let mut ready = Vec::new();
        let mut index = 0;
        while index < queued.len() {
            let matches = filter_wq.is_none_or(|wq| queued[index].wq == wq);
            if !matches {
                index += 1;
                continue;
            }
            ready.push(queued.swap_remove(index));
        }
        ready
    };

    for entry in pending.iter().copied() {
        set_work_pending(entry.work, false);
        let work = unsafe { &mut *(entry.work as *mut LinuxWorkStruct) };
        if let Some(func) = work.func {
            unsafe {
                func(work);
            }
        }
    }

    pending.len()
}

fn delete_timer(timer: *mut LinuxTimerList) -> i32 {
    let Some(timer_ptr) = ptr_to_usize(timer.cast::<c_void>()) else {
        return 0;
    };

    let removed = {
        let mut timers = PENDING_TIMERS.lock();
        if let Some(index) = timers.iter().position(|entry| entry.timer == timer_ptr) {
            timers.swap_remove(index);
            true
        } else {
            false
        }
    };

    mark_timer_pending(timer_ptr, false);
    if let Some(work_ptr) = delayed_work_item_from_timer(timer_ptr) {
        set_work_pending(work_ptr, false);
    }

    removed as i32
}

fn work_is_pending(work_ptr: usize) -> bool {
    let work = unsafe { &*(work_ptr as *const LinuxWorkStruct) };
    (work.data & WORK_STRUCT_PENDING) != 0
}

fn set_work_pending(work_ptr: usize, pending: bool) {
    let work = unsafe { &mut *(work_ptr as *mut LinuxWorkStruct) };
    if pending {
        work.data |= WORK_STRUCT_PENDING;
    } else {
        work.data &= !WORK_STRUCT_PENDING;
    }
}

fn timer_is_pending(timer_ptr: usize) -> bool {
    let timer = unsafe { &*(timer_ptr as *const LinuxTimerList) };
    !timer.entry.pprev.is_null()
}

fn mark_timer_pending(timer_ptr: usize, pending: bool) {
    let timer = unsafe { &mut *(timer_ptr as *mut LinuxTimerList) };
    if pending {
        timer.entry.next = &mut timer.entry;
        timer.entry.pprev = &mut timer.entry.next;
    } else {
        timer.entry.next = ptr::null_mut();
        timer.entry.pprev = ptr::null_mut();
    }
}

fn timer_ptr(timer: &mut LinuxTimerList) -> usize {
    timer as *mut LinuxTimerList as usize
}

unsafe fn delayed_work_from_timer<'a>(timer: *mut LinuxTimerList) -> &'a mut LinuxDelayedWork {
    let delayed_work_ptr = unsafe {
        (timer as *mut u8).sub(offset_of!(LinuxDelayedWork, timer)) as *mut LinuxDelayedWork
    };
    unsafe { &mut *delayed_work_ptr }
}

fn delayed_work_timer_fn_addr() -> usize {
    delayed_work_timer_fn as *const () as usize
}

fn delayed_work_item_from_timer(timer_ptr: usize) -> Option<usize> {
    if timer_ptr == 0 {
        return None;
    }
    let timer = unsafe { &*(timer_ptr as *const LinuxTimerList) };
    if timer.function.map(|func| func as *const () as usize) != Some(delayed_work_timer_fn_addr()) {
        return None;
    }
    let delayed_work = unsafe { delayed_work_from_timer(timer_ptr as *mut LinuxTimerList) };
    Some((&mut delayed_work.work as *mut LinuxWorkStruct) as usize)
}

fn delayed_work_queue_from_timer(timer_ptr: usize) -> Option<usize> {
    if timer_ptr == 0 {
        return None;
    }
    let timer = unsafe { &*(timer_ptr as *const LinuxTimerList) };
    if timer.function.map(|func| func as *const () as usize) != Some(delayed_work_timer_fn_addr()) {
        return None;
    }
    let delayed_work = unsafe { delayed_work_from_timer(timer_ptr as *mut LinuxTimerList) };
    ptr_to_usize(delayed_work.wq.cast::<c_void>())
}

fn ptr_to_usize(ptr: *mut c_void) -> Option<usize> {
    let value = ptr as usize;
    if value == 0 { None } else { Some(value) }
}
