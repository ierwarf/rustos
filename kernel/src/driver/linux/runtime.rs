use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::hint::spin_loop;
use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

static JIFFIES: AtomicU64 = AtomicU64::new(0);
static BOOT_CPU_DATA: [u8; 256] = [0; 256];
static X86_HYPER_TYPE: i32 = 0;
static IRQ_SPIN_LOCKS: Mutex<Vec<&'static CompatLockState>> = Mutex::new(Vec::new());
static MUTEX_LOCKS: Mutex<Vec<&'static CompatLockState>> = Mutex::new(Vec::new());
static IRQ_LOCK_OWNERS: Mutex<Vec<IrqOwnerState>> = Mutex::new(Vec::new());

struct CompatLockState {
    key: usize,
    held: AtomicBool,
    owner: AtomicUsize,
    depth: AtomicUsize,
}

impl CompatLockState {
    fn new(key: usize) -> Self {
        Self {
            key,
            held: AtomicBool::new(false),
            owner: AtomicUsize::new(0),
            depth: AtomicUsize::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct IrqOwnerState {
    owner: usize,
    depth: usize,
    restore_enabled: bool,
}

pub(crate) fn current_jiffies() -> u64 {
    JIFFIES.load(Ordering::Relaxed)
}

pub(crate) fn tick_jiffies(delta: u64) -> u64 {
    JIFFIES.fetch_add(delta, Ordering::Relaxed) + delta
}

pub(crate) unsafe extern "C" fn _raw_spin_lock_irq(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let restore_enabled = interrupts::are_enabled();
    let first_irq_lock = register_irq_lock_owner(owner, restore_enabled);
    interrupts::disable();

    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    acquire_compat_lock(state, owner);

    if !first_irq_lock {
        return;
    }
}

pub(crate) unsafe extern "C" fn _raw_spin_unlock_irq(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    release_compat_lock(state, owner);

    if unregister_irq_lock_owner(owner) {
        interrupts::enable();
    } else {
        interrupts::disable();
    }
}

pub(crate) unsafe extern "C" fn mutex_lock(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&MUTEX_LOCKS, lock as usize);
    acquire_compat_lock(state, owner);
}

pub(crate) unsafe extern "C" fn mutex_lock_interruptible(lock: *mut c_void) -> i32 {
    unsafe { mutex_lock(lock) };
    0
}

pub(crate) unsafe extern "C" fn mutex_unlock(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&MUTEX_LOCKS, lock as usize);
    release_compat_lock(state, owner);
}

pub(crate) unsafe extern "C" fn usleep_range_state(
    min_microseconds: u32,
    max_microseconds: u32,
    _state: u32,
) {
    let sleep_us = min_microseconds.max(max_microseconds);
    let sleep_ms = sleep_us.div_ceil(1000) as u64;
    if sleep_ms != 0 {
        crate::rtc::sleep(sleep_ms);
        tick_jiffies(sleep_ms);
    }
}

pub(crate) unsafe extern "C" fn _printk(fmt: *const c_char) -> i32 {
    write_cstr(fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_printk(
    _level: *const c_char,
    _dev: *mut c_void,
    fmt: *const c_char,
) -> i32 {
    write_cstr(fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_err(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    write_cstr(fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_warn(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    write_cstr(fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_notice(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    write_cstr(fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_info(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    write_cstr(fmt);
    0
}

pub(crate) unsafe extern "C" fn __dynamic_dev_dbg() -> i32 {
    0
}

pub(crate) unsafe extern "C" fn snprintf(
    dest: *mut c_char,
    size: usize,
    fmt: *const c_char,
) -> i32 {
    copy_format_string(dest, size, fmt)
}

pub(crate) unsafe extern "C" fn sprintf(dest: *mut c_char, fmt: *const c_char) -> i32 {
    copy_format_string(dest, usize::MAX, fmt)
}

pub(crate) unsafe extern "C" fn vmware_tdx_hypercall() -> i64 {
    -38
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "_raw_spin_lock_irq" => Some(_raw_spin_lock_irq as *const () as usize),
        "_raw_spin_unlock_irq" => Some(_raw_spin_unlock_irq as *const () as usize),
        "mutex_lock" => Some(mutex_lock as *const () as usize),
        "mutex_lock_interruptible" => Some(mutex_lock_interruptible as *const () as usize),
        "mutex_unlock" => Some(mutex_unlock as *const () as usize),
        "usleep_range_state" => Some(usleep_range_state as *const () as usize),
        "_printk" => Some(_printk as *const () as usize),
        "_dev_printk" => Some(_dev_printk as *const () as usize),
        "_dev_err" => Some(_dev_err as *const () as usize),
        "_dev_warn" => Some(_dev_warn as *const () as usize),
        "_dev_notice" => Some(_dev_notice as *const () as usize),
        "_dev_info" => Some(_dev_info as *const () as usize),
        "__dynamic_dev_dbg" => Some(__dynamic_dev_dbg as *const () as usize),
        "snprintf" => Some(snprintf as *const () as usize),
        "sprintf" => Some(sprintf as *const () as usize),
        "jiffies" => Some(&JIFFIES as *const AtomicU64 as usize),
        "boot_cpu_data" => Some(&BOOT_CPU_DATA as *const [u8; 256] as usize),
        "x86_hyper_type" => Some(&X86_HYPER_TYPE as *const i32 as usize),
        "vmware_tdx_hypercall" => Some(vmware_tdx_hypercall as *const () as usize),
        _ => None,
    }
}

fn copy_format_string(dest: *mut c_char, size: usize, fmt: *const c_char) -> i32 {
    if dest.is_null() || size == 0 {
        return 0;
    }

    let Some(bytes) = cstr_bytes(fmt) else {
        unsafe {
            *dest = 0;
        }
        return 0;
    };

    let copy_len = bytes.len().min(size.saturating_sub(1));
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest as *mut u8, copy_len);
        *dest.add(copy_len) = 0;
    }
    copy_len as i32
}

fn write_cstr(fmt: *const c_char) {
    let Some(bytes) = cstr_bytes(fmt) else {
        return;
    };
    if !bytes.is_empty() {
        // Linux driver printk paths must not contend with the interactive
        // console lock during probe/interrupt handling. Route them to the
        // debug channel only so driver diagnostics cannot deadlock console I/O.
        crate::debug::write_bytes(bytes);
    }
}

fn cstr_bytes<'a>(fmt: *const c_char) -> Option<&'a [u8]> {
    if fmt.is_null() {
        return None;
    }

    let mut len = 0usize;
    let mut cursor = fmt;
    while unsafe { *cursor } != 0 {
        len += 1;
        cursor = unsafe { cursor.add(1) };
    }
    Some(unsafe { slice::from_raw_parts(fmt as *const u8, len) })
}

fn current_lock_owner_token() -> usize {
    let rsp: usize;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    let task = crate::multitask::current_user_id().unwrap_or(0) as usize;
    (task << 12) ^ (rsp & !0xfffusize)
}

fn compat_lock_state(
    registry: &Mutex<Vec<&'static CompatLockState>>,
    key: usize,
) -> &'static CompatLockState {
    let mut locks = registry.lock();
    if let Some(state) = locks.iter().copied().find(|state| state.key == key) {
        return state;
    }

    // Linux modules pass opaque in-memory lock objects that the kernel would
    // normally manage. Mirror them with host-side spin states keyed by address.
    let state = Box::leak(Box::new(CompatLockState::new(key)));
    locks.push(state);
    state
}

fn acquire_compat_lock(state: &'static CompatLockState, owner: usize) {
    if state.owner.load(Ordering::Acquire) == owner {
        state.depth.fetch_add(1, Ordering::AcqRel);
        return;
    }

    while state
        .held
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spin_loop();
    }

    state.owner.store(owner, Ordering::Release);
    state.depth.store(1, Ordering::Release);
}

fn release_compat_lock(state: &'static CompatLockState, owner: usize) {
    let owned_by_current = state.owner.load(Ordering::Acquire) == owner;
    if owned_by_current {
        let depth = state.depth.load(Ordering::Acquire);
        if depth > 1 {
            state.depth.store(depth - 1, Ordering::Release);
            return;
        }
    }

    state.depth.store(0, Ordering::Release);
    state.owner.store(0, Ordering::Release);
    state.held.store(false, Ordering::Release);
}

fn register_irq_lock_owner(owner: usize, restore_enabled: bool) -> bool {
    let mut owners = IRQ_LOCK_OWNERS.lock();
    if let Some(state) = owners.iter_mut().find(|state| state.owner == owner) {
        state.depth += 1;
        return false;
    }

    owners.push(IrqOwnerState {
        owner,
        depth: 1,
        restore_enabled,
    });
    true
}

fn unregister_irq_lock_owner(owner: usize) -> bool {
    let mut owners = IRQ_LOCK_OWNERS.lock();
    let Some(index) = owners.iter().position(|state| state.owner == owner) else {
        return false;
    };

    if owners[index].depth > 1 {
        owners[index].depth -= 1;
        return false;
    }

    let restore_enabled = owners[index].restore_enabled;
    owners.swap_remove(index);
    restore_enabled
}
