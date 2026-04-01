use alloc::alloc::{alloc, Layout};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use spin::Mutex;

use super::compat::LinuxCompatWaitQueueHead;

struct SemaphoreState {
    key: usize,
    count: isize,
}

struct KfifoState {
    key: usize,
    bytes: Vec<u8>,
    capacity: usize,
}

struct PowerSupplyState {
    handle: usize,
    drv_data: usize,
}

static NEXT_HANDLE_ID: AtomicUsize = AtomicUsize::new(1);
static NEXT_CHRDEV: AtomicU32 = AtomicU32::new(0x100);
static CURRENT_TASK_STUB: [u8; 4096] = [0; 4096];
static SCHED_SET_STATE_TRACEPOINT: usize = 0;
static MAY_RESCHED_SECTION: u8 = 0;
static SEMAPHORES: Mutex<Vec<SemaphoreState>> = Mutex::new(Vec::new());
static KFIFOS: Mutex<Vec<KfifoState>> = Mutex::new(Vec::new());
static POWER_SUPPLIES: Mutex<Vec<PowerSupplyState>> = Mutex::new(Vec::new());

pub(crate) fn init_cpu_local_symbols() {
    crate::user::syscall::set_linux_compat_current_task_ptr(
        &CURRENT_TASK_STUB as *const [u8; 4096] as usize,
    );
}

pub(crate) unsafe extern "C" fn ___ratelimit(_state: *mut c_void, _func: *const c_char) -> bool {
    true
}

pub(crate) unsafe extern "C" fn __check_object_size(
    _ptr: *const c_void,
    _bytes: usize,
    _to_user: bool,
) {
}

pub(crate) unsafe extern "C" fn validate_usercopy_range(
    _ptr: *const c_void,
    _bytes: usize,
) -> bool {
    true
}

pub(crate) unsafe extern "C" fn _copy_to_user(
    dest: *mut c_void,
    src: *const c_void,
    len: usize,
) -> usize {
    if dest.is_null() || src.is_null() {
        return len;
    }
    unsafe { ptr::copy_nonoverlapping(src.cast::<u8>(), dest.cast::<u8>(), len) };
    0
}

pub(crate) unsafe extern "C" fn _copy_from_user(
    dest: *mut c_void,
    src: *const c_void,
    len: usize,
) -> usize {
    if dest.is_null() || src.is_null() {
        return len;
    }
    unsafe { ptr::copy_nonoverlapping(src.cast::<u8>(), dest.cast::<u8>(), len) };
    0
}

pub(crate) unsafe extern "C" fn compat_ptr_ioctl(
    _file: *mut c_void,
    _cmd: u32,
    _arg: usize,
) -> i32 {
    -25
}

pub(crate) unsafe extern "C" fn noop_llseek(_file: *mut c_void, _offset: i64, _whence: i32) -> i64 {
    0
}

pub(crate) unsafe extern "C" fn __init_waitqueue_head(
    head: *mut LinuxCompatWaitQueueHead,
    _name: *const c_char,
    _key: *mut c_void,
) {
    if head.is_null() {
        return;
    }
    unsafe {
        *head = LinuxCompatWaitQueueHead::default();
    }
}

pub(crate) unsafe extern "C" fn add_wait_queue(
    _head: *mut LinuxCompatWaitQueueHead,
    _entry: *mut c_void,
) {
}

pub(crate) unsafe extern "C" fn remove_wait_queue(
    _head: *mut LinuxCompatWaitQueueHead,
    _entry: *mut c_void,
) {
}

pub(crate) unsafe extern "C" fn init_wait_entry(_entry: *mut c_void, _flags: i32) {}

pub(crate) unsafe extern "C" fn prepare_to_wait(
    _head: *mut LinuxCompatWaitQueueHead,
    _entry: *mut c_void,
    _state: i32,
) {
}

pub(crate) unsafe extern "C" fn prepare_to_wait_event(
    _head: *mut LinuxCompatWaitQueueHead,
    _entry: *mut c_void,
    _state: i32,
) -> isize {
    0
}

pub(crate) unsafe extern "C" fn finish_wait(
    _head: *mut LinuxCompatWaitQueueHead,
    _entry: *mut c_void,
) {
}

pub(crate) unsafe extern "C" fn default_wake_function(
    _entry: *mut c_void,
    _mode: u32,
    _wake_flags: i32,
    _key: *mut c_void,
) -> i32 {
    1
}

pub(crate) unsafe extern "C" fn autoremove_wake_function(
    entry: *mut c_void,
    mode: u32,
    wake_flags: i32,
    key: *mut c_void,
) -> i32 {
    unsafe { default_wake_function(entry, mode, wake_flags, key) }
}

pub(crate) unsafe extern "C" fn __wake_up(
    _head: *mut LinuxCompatWaitQueueHead,
    _mode: u32,
    _nr_exclusive: i32,
    _key: *mut c_void,
) {
}

pub(crate) unsafe extern "C" fn down(sem: *mut c_void) {
    let _ = take_semaphore(sem);
}

pub(crate) unsafe extern "C" fn down_interruptible(sem: *mut c_void) -> i32 {
    let _ = take_semaphore(sem);
    0
}

pub(crate) unsafe extern "C" fn down_trylock(sem: *mut c_void) -> i32 {
    if try_take_semaphore(sem) {
        0
    } else {
        1
    }
}

pub(crate) unsafe extern "C" fn up(sem: *mut c_void) {
    release_semaphore(sem);
}

pub(crate) unsafe extern "C" fn down_read(lock: *mut c_void) {
    let _ = take_semaphore(lock);
}

pub(crate) unsafe extern "C" fn up_read(lock: *mut c_void) {
    release_semaphore(lock);
}

pub(crate) unsafe extern "C" fn down_write(lock: *mut c_void) {
    let _ = take_semaphore(lock);
}

pub(crate) unsafe extern "C" fn up_write(lock: *mut c_void) {
    release_semaphore(lock);
}

pub(crate) unsafe extern "C" fn fasync_helper(
    _fd: i32,
    _file: *mut c_void,
    _on: i32,
    _fasync: *mut *mut c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn kill_fasync(_fasync: *mut *mut c_void, _sig: i32, _band: i32) {}

pub(crate) unsafe extern "C" fn single_open(
    _file: *mut c_void,
    _show: *const c_void,
    _data: *mut c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn single_release(_inode: *mut c_void, _file: *mut c_void) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn alloc_chrdev_region(
    devt: *mut u32,
    _baseminor: u32,
    count: u32,
    _name: *const c_char,
) -> i32 {
    if devt.is_null() || count == 0 {
        return -22;
    }
    unsafe {
        *devt = NEXT_CHRDEV.fetch_add(count.max(1), Ordering::Relaxed);
    }
    0
}

pub(crate) unsafe extern "C" fn unregister_chrdev_region(_devt: u32, _count: u32) {}

pub(crate) unsafe extern "C" fn cdev_init(_cdev: *mut c_void, _fops: *const c_void) {}

pub(crate) unsafe extern "C" fn cdev_add(_cdev: *mut c_void, _devt: u32, _count: u32) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn cdev_del(_cdev: *mut c_void) {}

pub(crate) unsafe extern "C" fn debugfs_create_dir(
    _name: *const c_char,
    _parent: *mut c_void,
) -> *mut c_void {
    new_handle()
}

pub(crate) unsafe extern "C" fn debugfs_create_file_full(
    _name: *const c_char,
    _mode: u32,
    _parent: *mut c_void,
    _data: *mut c_void,
    _fops: *const c_void,
    _aux: *mut c_void,
) -> *mut c_void {
    new_handle()
}

pub(crate) unsafe extern "C" fn debugfs_remove(entry: *mut c_void) {
    if entry.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(entry as *mut usize));
    }
}

pub(crate) unsafe extern "C" fn power_supply_register(
    _parent: *mut c_void,
    _desc: *const c_void,
    cfg: *const c_void,
) -> *mut c_void {
    let handle = new_handle();
    let drv_data = if cfg.is_null() {
        0
    } else {
        unsafe {
            // struct power_supply_config: fwnode, drv_data, ...
            *((cfg as *const usize).add(1))
        }
    };
    POWER_SUPPLIES.lock().push(PowerSupplyState {
        handle: handle as usize,
        drv_data,
    });
    handle
}

pub(crate) unsafe extern "C" fn power_supply_unregister(psy: *mut c_void) {
    let Some(handle) = ptr_key(psy) else {
        return;
    };
    let mut supplies = POWER_SUPPLIES.lock();
    if let Some(index) = supplies.iter().position(|entry| entry.handle == handle) {
        supplies.remove(index);
    }
    unsafe {
        drop(Box::from_raw(psy as *mut usize));
    }
}

pub(crate) unsafe extern "C" fn power_supply_changed(_psy: *mut c_void) {}

pub(crate) unsafe extern "C" fn power_supply_get_drvdata(psy: *mut c_void) -> *mut c_void {
    let Some(handle) = ptr_key(psy) else {
        return ptr::null_mut();
    };
    POWER_SUPPLIES
        .lock()
        .iter()
        .find(|entry| entry.handle == handle)
        .map(|entry| entry.drv_data as *mut c_void)
        .unwrap_or(ptr::null_mut())
}

pub(crate) unsafe extern "C" fn power_supply_powers(_psy: *mut c_void, _dev: *mut c_void) {}

pub(crate) unsafe extern "C" fn seq_read(
    _file: *mut c_void,
    _buf: *mut c_char,
    _size: usize,
    _ppos: *mut u64,
) -> isize {
    0
}

pub(crate) unsafe extern "C" fn seq_lseek(_file: *mut c_void, _offset: i64, _whence: i32) -> i64 {
    0
}

pub(crate) unsafe extern "C" fn seq_printf(
    _seq: *mut c_void,
    _fmt: *const c_char,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _arg3: usize,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn scnprintf(
    dest: *mut c_char,
    size: usize,
    fmt: *const c_char,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _arg3: usize,
) -> i32 {
    copy_c_string(dest, size, fmt)
}

pub(crate) unsafe extern "C" fn sysfs_emit(
    dest: *mut c_char,
    fmt: *const c_char,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _arg3: usize,
) -> i32 {
    copy_c_string(dest, usize::MAX, fmt)
}

pub(crate) unsafe extern "C" fn kasprintf(
    _gfp: u32,
    fmt: *const c_char,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
    _arg3: usize,
) -> *mut c_char {
    let Some(bytes) = cstr_bytes(fmt) else {
        return ptr::null_mut();
    };
    let layout = match Layout::array::<u8>(bytes.len() + 1) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };
    let dest = unsafe { alloc(layout) };
    if dest.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
        *dest.add(bytes.len()) = 0;
    }
    dest.cast()
}

pub(crate) unsafe extern "C" fn __kfifo_alloc(
    fifo: *mut c_void,
    size: u32,
    esize: usize,
    _gfp: u32,
) -> i32 {
    let Some(key) = ptr_key(fifo) else {
        return -22;
    };
    let capacity = (size as usize).saturating_mul(esize.max(1));
    let mut fifos = KFIFOS.lock();
    if let Some(existing) = fifos.iter_mut().find(|existing| existing.key == key) {
        existing.bytes.clear();
        existing.capacity = capacity;
        return 0;
    }
    fifos.push(KfifoState {
        key,
        bytes: Vec::new(),
        capacity,
    });
    0
}

pub(crate) unsafe extern "C" fn __kfifo_free(fifo: *mut c_void) {
    let Some(key) = ptr_key(fifo) else {
        return;
    };
    let mut fifos = KFIFOS.lock();
    if let Some(index) = fifos.iter().position(|existing| existing.key == key) {
        fifos.remove(index);
    }
}

pub(crate) unsafe extern "C" fn __kfifo_in(
    fifo: *mut c_void,
    from: *const c_void,
    len: u32,
) -> u32 {
    let Some(key) = ptr_key(fifo) else {
        return 0;
    };
    let mut fifos = KFIFOS.lock();
    let Some(state) = fifos.iter_mut().find(|existing| existing.key == key) else {
        return 0;
    };
    if from.is_null() {
        return 0;
    }
    let max_len = state.capacity.saturating_sub(state.bytes.len());
    let copy_len = core::cmp::min(len as usize, max_len);
    let slice = unsafe { core::slice::from_raw_parts(from.cast::<u8>(), copy_len) };
    state.bytes.extend_from_slice(slice);
    copy_len as u32
}

pub(crate) unsafe extern "C" fn __kfifo_to_user(
    fifo: *mut c_void,
    to: *mut c_void,
    len: u32,
    copied: *mut u32,
) -> i32 {
    let Some(key) = ptr_key(fifo) else {
        return -22;
    };
    let mut fifos = KFIFOS.lock();
    let Some(state) = fifos.iter_mut().find(|existing| existing.key == key) else {
        return -22;
    };
    let copy_len = core::cmp::min(len as usize, state.bytes.len());
    if !to.is_null() && copy_len != 0 {
        unsafe {
            ptr::copy_nonoverlapping(state.bytes.as_ptr(), to.cast::<u8>(), copy_len);
        }
    }
    state.bytes.drain(..copy_len);
    if !copied.is_null() {
        unsafe {
            *copied = copy_len as u32;
        }
    }
    0
}

pub(crate) unsafe extern "C" fn add_uevent_var(
    _env: *mut c_void,
    _fmt: *const c_char,
    _arg0: usize,
    _arg1: usize,
    _arg2: usize,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn __trace_set_current_state(_state: i64) {}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "___ratelimit" => Some(___ratelimit as *const () as usize),
        "__check_object_size" => Some(__check_object_size as *const () as usize),
        "validate_usercopy_range" => Some(validate_usercopy_range as *const () as usize),
        "_copy_to_user" => Some(_copy_to_user as *const () as usize),
        "_copy_from_user" => Some(_copy_from_user as *const () as usize),
        "compat_ptr_ioctl" => Some(compat_ptr_ioctl as *const () as usize),
        "noop_llseek" => Some(noop_llseek as *const () as usize),
        "__init_waitqueue_head" => Some(__init_waitqueue_head as *const () as usize),
        "add_wait_queue" => Some(add_wait_queue as *const () as usize),
        "remove_wait_queue" => Some(remove_wait_queue as *const () as usize),
        "init_wait_entry" => Some(init_wait_entry as *const () as usize),
        "prepare_to_wait" => Some(prepare_to_wait as *const () as usize),
        "prepare_to_wait_event" => Some(prepare_to_wait_event as *const () as usize),
        "finish_wait" => Some(finish_wait as *const () as usize),
        "default_wake_function" => Some(default_wake_function as *const () as usize),
        "autoremove_wake_function" => Some(autoremove_wake_function as *const () as usize),
        "__wake_up" => Some(__wake_up as *const () as usize),
        "down" => Some(down as *const () as usize),
        "down_interruptible" => Some(down_interruptible as *const () as usize),
        "down_trylock" => Some(down_trylock as *const () as usize),
        "up" => Some(up as *const () as usize),
        "down_read" => Some(down_read as *const () as usize),
        "up_read" => Some(up_read as *const () as usize),
        "down_write" => Some(down_write as *const () as usize),
        "up_write" => Some(up_write as *const () as usize),
        "fasync_helper" => Some(fasync_helper as *const () as usize),
        "kill_fasync" => Some(kill_fasync as *const () as usize),
        "single_open" => Some(single_open as *const () as usize),
        "single_release" => Some(single_release as *const () as usize),
        "alloc_chrdev_region" => Some(alloc_chrdev_region as *const () as usize),
        "unregister_chrdev_region" => Some(unregister_chrdev_region as *const () as usize),
        "cdev_init" => Some(cdev_init as *const () as usize),
        "cdev_add" => Some(cdev_add as *const () as usize),
        "cdev_del" => Some(cdev_del as *const () as usize),
        "debugfs_create_dir" => Some(debugfs_create_dir as *const () as usize),
        "debugfs_create_file_full" => Some(debugfs_create_file_full as *const () as usize),
        "debugfs_remove" => Some(debugfs_remove as *const () as usize),
        "power_supply_register" => Some(power_supply_register as *const () as usize),
        "power_supply_unregister" => Some(power_supply_unregister as *const () as usize),
        "power_supply_changed" => Some(power_supply_changed as *const () as usize),
        "power_supply_get_drvdata" => Some(power_supply_get_drvdata as *const () as usize),
        "power_supply_powers" => Some(power_supply_powers as *const () as usize),
        "seq_read" => Some(seq_read as *const () as usize),
        "seq_lseek" => Some(seq_lseek as *const () as usize),
        "seq_printf" => Some(seq_printf as *const () as usize),
        "scnprintf" => Some(scnprintf as *const () as usize),
        "sysfs_emit" => Some(sysfs_emit as *const () as usize),
        "kasprintf" => Some(kasprintf as *const () as usize),
        "__kfifo_alloc" => Some(__kfifo_alloc as *const () as usize),
        "__kfifo_free" => Some(__kfifo_free as *const () as usize),
        "__kfifo_in" => Some(__kfifo_in as *const () as usize),
        "__kfifo_to_user" => Some(__kfifo_to_user as *const () as usize),
        "add_uevent_var" => Some(add_uevent_var as *const () as usize),
        "const_current_task" => Some(crate::user::syscall::linux_compat_current_task_offset()),
        "__trace_set_current_state" => Some(__trace_set_current_state as *const () as usize),
        "__tracepoint_sched_set_state_tp" => {
            Some(&SCHED_SET_STATE_TRACEPOINT as *const usize as usize)
        }
        "__SCT__might_resched" => Some(&MAY_RESCHED_SECTION as *const u8 as usize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_symbol;

    #[test]
    fn current_task_uses_gs_relative_offset() {
        assert_eq!(
            resolve_symbol("const_current_task"),
            Some(crate::user::syscall::linux_compat_current_task_offset())
        );
    }
}

fn cstr_bytes<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

fn copy_c_string(dest: *mut c_char, size: usize, src: *const c_char) -> i32 {
    if dest.is_null() || size == 0 {
        return 0;
    }
    let Some(bytes) = cstr_bytes(src) else {
        unsafe {
            *dest = 0;
        }
        return 0;
    };
    let copy_len = core::cmp::min(bytes.len(), size.saturating_sub(1));
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest.cast::<u8>(), copy_len);
        *dest.add(copy_len) = 0;
    }
    copy_len as i32
}

fn ptr_key(ptr: *mut c_void) -> Option<usize> {
    (!ptr.is_null()).then_some(ptr as usize)
}

fn new_handle() -> *mut c_void {
    Box::into_raw(Box::new(NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed))) as *mut c_void
}

fn take_semaphore(sem: *mut c_void) -> bool {
    let Some(key) = ptr_key(sem) else {
        return false;
    };
    let mut semaphores = SEMAPHORES.lock();
    if !semaphores.iter().any(|existing| existing.key == key) {
        semaphores.push(SemaphoreState { key, count: 1 });
    }
    let Some(existing) = semaphores.iter_mut().find(|existing| existing.key == key) else {
        return false;
    };
    if existing.count <= 0 {
        return false;
    }
    existing.count -= 1;
    true
}

fn try_take_semaphore(sem: *mut c_void) -> bool {
    take_semaphore(sem)
}

fn release_semaphore(sem: *mut c_void) {
    let Some(key) = ptr_key(sem) else {
        return;
    };
    let mut semaphores = SEMAPHORES.lock();
    if !semaphores.iter().any(|existing| existing.key == key) {
        semaphores.push(SemaphoreState { key, count: 1 });
    }
    if let Some(existing) = semaphores.iter_mut().find(|existing| existing.key == key) {
        existing.count = existing.count.saturating_add(1);
    }
}
