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
static CPU_NUMBER: i32 = 0;
static THIS_CPU_OFF: usize = 0;
static X86_HYPER_TYPE: i32 = 0;
static IRQ_SPIN_LOCKS: Mutex<Vec<&'static CompatLockState>> = Mutex::new(Vec::new());
static MUTEX_LOCKS: Mutex<Vec<&'static CompatLockState>> = Mutex::new(Vec::new());
static IRQ_LOCK_OWNERS: Mutex<Vec<IrqOwnerState>> = Mutex::new(Vec::new());
static IRQ_LOCK_SNAPSHOT_OWNER_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_LOCK_SNAPSHOT_TOTAL_DEPTH: AtomicUsize = AtomicUsize::new(0);
static MUTEX_DEBUG_REMAINING: AtomicUsize = AtomicUsize::new(64);
const MAX_COMPAT_PRINTK_BYTES: usize = 4096;
const MAX_COMPAT_RANDOM_BYTES: usize = 1 << 20;
const MAX_COMPAT_SG_INIT_ENTRIES: usize = 4096;
const COMPAT_LOCK_SPIN_HARD_LIMIT: usize = 5_000_000;
const LINUX_SG_END: usize = 0x02;

#[repr(C)]
struct LinuxCompatScatterlist {
    page_link: usize,
    offset: u32,
    length: u32,
    dma_address: u64,
}

const _: [(); 24] = [(); core::mem::size_of::<LinuxCompatScatterlist>()];

#[repr(C)]
pub(crate) struct LinuxTimespec64 {
    tv_sec: i64,
    tv_nsec: i64,
}

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

pub fn tick_jiffies(delta: u64) -> u64 {
    JIFFIES.fetch_add(delta, Ordering::Relaxed) + delta
}

pub(crate) unsafe extern "C" fn _raw_spin_lock_irq(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    if irq_lock_owner_depth(owner) != 0 {
        acquire_compat_lock(state, owner);
        register_irq_lock_owner(owner, false);
        return;
    }

    acquire_compat_lock_irq(state, owner);
}

pub(crate) unsafe extern "C" fn _raw_spin_lock(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    acquire_compat_lock(state, owner);
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

pub(crate) unsafe extern "C" fn _raw_spin_unlock(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    release_compat_lock(state, owner);
}

pub(crate) unsafe extern "C" fn _raw_spin_lock_bh(lock: *mut c_void) {
    unsafe { _raw_spin_lock(lock) };
}

pub(crate) unsafe extern "C" fn _raw_spin_unlock_bh(lock: *mut c_void) {
    unsafe { _raw_spin_unlock(lock) };
}

pub(crate) unsafe extern "C" fn _raw_spin_trylock(lock: *mut c_void) -> i32 {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    i32::from(try_acquire_compat_lock(state, owner))
}

pub(crate) unsafe extern "C" fn __local_bh_enable_ip(_ip: usize, _cnt: u32) {}

pub(crate) unsafe extern "C" fn __rcu_read_lock() {}

pub(crate) unsafe extern "C" fn __rcu_read_unlock() {}

pub(crate) unsafe extern "C" fn _raw_spin_lock_irqsave(lock: *mut c_void) -> usize {
    let flags = interrupts::are_enabled() as usize;
    unsafe { _raw_spin_lock_irq(lock) };
    flags
}

pub(crate) unsafe extern "C" fn _raw_spin_unlock_irqrestore(lock: *mut c_void, flags: usize) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&IRQ_SPIN_LOCKS, lock as usize);
    release_compat_lock(state, owner);

    if unregister_irq_lock_owner(owner) && flags != 0 {
        interrupts::enable();
    }
}

pub(crate) unsafe extern "C" fn __mutex_init(
    _lock: *mut c_void,
    _name: *const c_char,
    _key: *mut c_void,
) {
}

pub(crate) unsafe extern "C" fn mutex_lock(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&MUTEX_LOCKS, lock as usize);
    log_mutex_event(
        "begin",
        state.key,
        owner,
        state.owner.load(Ordering::Relaxed),
        state.depth.load(Ordering::Relaxed),
    );
    acquire_compat_lock(state, owner);
    log_mutex_event(
        "acquired",
        state.key,
        owner,
        state.owner.load(Ordering::Relaxed),
        state.depth.load(Ordering::Relaxed),
    );
}

pub(crate) unsafe extern "C" fn mutex_lock_interruptible(lock: *mut c_void) -> i32 {
    unsafe { mutex_lock(lock) };
    0
}

pub(crate) unsafe extern "C" fn mutex_lock_killable(lock: *mut c_void) -> i32 {
    unsafe { mutex_lock(lock) };
    0
}

pub(crate) unsafe extern "C" fn mutex_unlock(lock: *mut c_void) {
    let owner = current_lock_owner_token();
    let state = compat_lock_state(&MUTEX_LOCKS, lock as usize);
    log_mutex_event(
        "unlock",
        state.key,
        owner,
        state.owner.load(Ordering::Relaxed),
        state.depth.load(Ordering::Relaxed),
    );
    release_compat_lock(state, owner);
}

pub(crate) unsafe extern "C" fn usleep_range_state(
    min_microseconds: u32,
    max_microseconds: u32,
    _state: u32,
) {
    assert_no_irq_spinlock_held("usleep_range_state");
    let sleep_us = min_microseconds.max(max_microseconds);
    let sleep_ms = sleep_us.div_ceil(1000) as u64;
    if sleep_ms != 0 {
        crate::arch::rtc::sleep(sleep_ms);
        tick_jiffies(sleep_ms);
    }
}

pub(crate) unsafe extern "C" fn schedule() {
    sync_jiffies_from_rtc();
    service_compat_pending();
}

pub(crate) unsafe extern "C" fn schedule_timeout(timeout: i64) -> i64 {
    assert_no_irq_spinlock_held("schedule_timeout");
    if timeout > 0 {
        let ticks = timeout as u64;
        let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
        let milliseconds = ticks.saturating_mul(1000).div_ceil(ticks_per_second);
        if milliseconds != 0 {
            crate::arch::rtc::sleep(milliseconds);
        }
        tick_jiffies(ticks);
    } else {
        sync_jiffies_from_rtc();
    }
    service_compat_pending();
    0
}

pub(crate) unsafe extern "C" fn __msecs_to_jiffies(milliseconds: u32) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    (milliseconds as u64)
        .saturating_mul(ticks_per_second)
        .div_ceil(1000)
}

pub(crate) unsafe extern "C" fn jiffies_to_usecs(jiffies: u64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    jiffies.saturating_mul(1_000_000) / ticks_per_second
}

pub(crate) unsafe extern "C" fn _printk(fmt: *const c_char) -> i32 {
    emit_linux_printk(b"linux printk: ", fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_printk(
    _level: *const c_char,
    _dev: *mut c_void,
    fmt: *const c_char,
) -> i32 {
    emit_linux_printk(b"linux dev: ", fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_err(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    emit_linux_printk(b"linux dev_err: ", fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_warn(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    emit_linux_printk(b"linux dev_warn: ", fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_notice(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    emit_linux_printk(b"linux dev_notice: ", fmt);
    0
}

pub(crate) unsafe extern "C" fn _dev_info(_dev: *mut c_void, fmt: *const c_char) -> i32 {
    emit_linux_printk(b"linux dev_info: ", fmt);
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

pub(crate) unsafe extern "C" fn ktime_get_coarse_ts64(ts: *mut LinuxTimespec64) {
    if ts.is_null() {
        return;
    }
    let (seconds, nanoseconds) = monotonic_time_parts();
    unsafe {
        (*ts).tv_sec = seconds;
        (*ts).tv_nsec = nanoseconds;
    }
}

pub(crate) unsafe extern "C" fn ktime_get_mono_fast_ns() -> u64 {
    let (seconds, nanoseconds) = monotonic_time_parts();
    (seconds.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nanoseconds.max(0) as u64)
}

pub(crate) unsafe extern "C" fn ktime_get() -> i64 {
    unsafe { ktime_get_mono_fast_ns() as i64 }
}

pub(crate) unsafe extern "C" fn sched_clock() -> u64 {
    unsafe { ktime_get_mono_fast_ns() }
}

pub(crate) unsafe extern "C" fn get_random_bytes(dest: *mut c_void, len: usize) {
    if dest.is_null() || len == 0 {
        return;
    }
    let len = len.min(MAX_COMPAT_RANDOM_BYTES);
    let mut state = unsafe { ktime_get_mono_fast_ns() } ^ 0x9e37_79b9_7f4a_7c15;
    let bytes = unsafe { slice::from_raw_parts_mut(dest.cast::<u8>(), len) };
    for byte in bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
}

pub(crate) unsafe extern "C" fn sg_init_table(sgl: *mut c_void, nents: u32) {
    if sgl.is_null() || nents == 0 {
        return;
    }
    let nents = (nents as usize).min(MAX_COMPAT_SG_INIT_ENTRIES);
    let entries = unsafe { slice::from_raw_parts_mut(sgl.cast::<LinuxCompatScatterlist>(), nents) };
    for entry in entries.iter_mut() {
        *entry = LinuxCompatScatterlist {
            page_link: 0,
            offset: 0,
            length: 0,
            dma_address: 0,
        };
    }
    if let Some(last) = entries.last_mut() {
        last.page_link = LINUX_SG_END;
    }
}

pub(crate) unsafe extern "C" fn sg_init_one(sg: *mut c_void, buf: *const c_void, buflen: u32) {
    if sg.is_null() {
        return;
    }
    let entry = sg.cast::<LinuxCompatScatterlist>();
    unsafe {
        ptr::write(
            entry,
            LinuxCompatScatterlist {
                page_link: (buf as usize & !0x3) | LINUX_SG_END,
                offset: (buf as usize & 0x3) as u32,
                length: buflen,
                dma_address: buf as u64,
            },
        );
    }
}

pub(crate) unsafe extern "C" fn refcount_warn_saturate(_ptr: *mut c_void, _type_: i32) {}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "_raw_spin_lock" => Some(_raw_spin_lock as *const () as usize),
        "_raw_spin_lock_bh" => Some(_raw_spin_lock_bh as *const () as usize),
        "_raw_spin_lock_irq" => Some(_raw_spin_lock_irq as *const () as usize),
        "_raw_spin_lock_irqsave" => Some(_raw_spin_lock_irqsave as *const () as usize),
        "_raw_spin_trylock" => Some(_raw_spin_trylock as *const () as usize),
        "_raw_spin_unlock" => Some(_raw_spin_unlock as *const () as usize),
        "_raw_spin_unlock_bh" => Some(_raw_spin_unlock_bh as *const () as usize),
        "_raw_spin_unlock_irq" => Some(_raw_spin_unlock_irq as *const () as usize),
        "_raw_spin_unlock_irqrestore" => Some(_raw_spin_unlock_irqrestore as *const () as usize),
        "__local_bh_enable_ip" => Some(__local_bh_enable_ip as *const () as usize),
        "__rcu_read_lock" => Some(__rcu_read_lock as *const () as usize),
        "__rcu_read_unlock" => Some(__rcu_read_unlock as *const () as usize),
        "__mutex_init" => Some(__mutex_init as *const () as usize),
        "mutex_lock" => Some(mutex_lock as *const () as usize),
        "mutex_lock_interruptible" => Some(mutex_lock_interruptible as *const () as usize),
        "mutex_lock_killable" => Some(mutex_lock_killable as *const () as usize),
        "mutex_unlock" => Some(mutex_unlock as *const () as usize),
        "usleep_range_state" => Some(usleep_range_state as *const () as usize),
        "schedule" => Some(schedule as *const () as usize),
        "schedule_timeout" => Some(schedule_timeout as *const () as usize),
        "__msecs_to_jiffies" => Some(__msecs_to_jiffies as *const () as usize),
        "jiffies_to_usecs" => Some(jiffies_to_usecs as *const () as usize),
        "_printk" => Some(_printk as *const () as usize),
        "_dev_printk" => Some(_dev_printk as *const () as usize),
        "_dev_err" => Some(_dev_err as *const () as usize),
        "_dev_warn" => Some(_dev_warn as *const () as usize),
        "_dev_notice" => Some(_dev_notice as *const () as usize),
        "_dev_info" => Some(_dev_info as *const () as usize),
        "__dynamic_dev_dbg" => Some(__dynamic_dev_dbg as *const () as usize),
        "snprintf" => Some(snprintf as *const () as usize),
        "sprintf" => Some(sprintf as *const () as usize),
        "ktime_get_coarse_ts64" => Some(ktime_get_coarse_ts64 as *const () as usize),
        "ktime_get" => Some(ktime_get as *const () as usize),
        "ktime_get_mono_fast_ns" => Some(ktime_get_mono_fast_ns as *const () as usize),
        "sched_clock" => Some(sched_clock as *const () as usize),
        "get_random_bytes" => Some(get_random_bytes as *const () as usize),
        "sg_init_table" => Some(sg_init_table as *const () as usize),
        "sg_init_one" => Some(sg_init_one as *const () as usize),
        "refcount_warn_saturate" => Some(refcount_warn_saturate as *const () as usize),
        "jiffies" => Some(&JIFFIES as *const AtomicU64 as usize),
        "boot_cpu_data" => Some(&BOOT_CPU_DATA as *const [u8; 256] as usize),
        "cpu_number" => Some(&CPU_NUMBER as *const i32 as usize),
        "this_cpu_off" => Some(&THIS_CPU_OFF as *const usize as usize),
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

    let copy_limit = size.saturating_sub(1).min(MAX_COMPAT_PRINTK_BYTES);
    let copy_len = bytes.len().min(copy_limit);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest as *mut u8, copy_len);
        *dest.add(copy_len) = 0;
    }
    copy_len as i32
}

pub fn service_compat_pending() {
    const MAX_SERVICE_PASSES: usize = 4;

    for _ in 0..MAX_SERVICE_PASSES {
        let usb_work = crate::usb::service_pending();
        let serio_work = crate::input::serio_lower_half_service_pending();
        let threaded_irq_work = crate::driver::irq::service_threaded_irqs();
        let workqueue_work = crate::driver::linux::workqueue::service_pending();
        let work = usb_work + serio_work + threaded_irq_work + workqueue_work;
        if work == 0 {
            break;
        }
    }
}

pub fn debug_irq_lock_snapshot() -> (usize, usize) {
    let Some(owners) = IRQ_LOCK_OWNERS.try_lock() else {
        return (
            IRQ_LOCK_SNAPSHOT_OWNER_COUNT.load(Ordering::Acquire),
            IRQ_LOCK_SNAPSHOT_TOTAL_DEPTH.load(Ordering::Acquire),
        );
    };
    let owner_count = owners.len();
    let total_depth = owners.iter().map(|state| state.depth).sum();
    IRQ_LOCK_SNAPSHOT_OWNER_COUNT.store(owner_count, Ordering::Release);
    IRQ_LOCK_SNAPSHOT_TOTAL_DEPTH.store(total_depth, Ordering::Release);
    (owner_count, total_depth)
}

pub(crate) fn assert_no_irq_spinlock_held(api: &str) {
    let owner = current_lock_owner_token();
    let depth = irq_lock_owner_depth(owner);
    if depth == 0 {
        return;
    }
    panic!(
        "linux compat {} called while irq spinlock held: owner={} depth={} irq_enabled={}",
        api,
        owner,
        depth,
        interrupts::are_enabled()
    );
}

fn emit_linux_printk(prefix: &[u8], fmt: *const c_char) {
    let Some(bytes) = cstr_bytes(fmt) else {
        return;
    };
    let bytes = trim_trailing_line_endings(strip_linux_log_level_prefix(bytes));
    if bytes.is_empty() {
        return;
    }

    crate::debug::write_debugcon_only_parts_line(&[prefix, bytes]);
}

fn strip_linux_log_level_prefix(bytes: &[u8]) -> &[u8] {
    if bytes.len() >= 2 && bytes[0] == 0x01 {
        match bytes[1] {
            b'0'..=b'7' | b'c' | b'd' => return &bytes[2..],
            _ => {}
        }
    }
    bytes
}

fn cstr_bytes<'a>(fmt: *const c_char) -> Option<&'a [u8]> {
    if fmt.is_null() {
        return None;
    }

    let mut len = 0usize;
    let mut cursor = fmt;
    while len < MAX_COMPAT_PRINTK_BYTES && unsafe { *cursor } != 0 {
        len += 1;
        cursor = unsafe { cursor.add(1) };
    }
    Some(unsafe { slice::from_raw_parts(fmt as *const u8, len) })
}

fn trim_trailing_line_endings(bytes: &[u8]) -> &[u8] {
    let mut len = bytes.len();
    while len != 0 && matches!(bytes[len - 1], b'\n' | b'\r') {
        len -= 1;
    }
    &bytes[..len]
}

fn current_lock_owner_token() -> usize {
    let thread = crate::multitask::current_user_thread_id().unwrap_or(0) as usize;
    if thread != 0 {
        return (thread << 1) | 1;
    }

    let process = crate::multitask::current_user_process_id().unwrap_or(0) as usize;
    if process != 0 {
        return (process << 1) | 1;
    }

    // Keep zero reserved for "unowned" lock state.
    1
}

fn monotonic_time_parts() -> (i64, i64) {
    let ticks = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let seconds = ticks / ticks_per_second;
    let tick_remainder = ticks % ticks_per_second;
    let nanoseconds =
        ((tick_remainder as u128) * 1_000_000_000u128 / (ticks_per_second as u128)) as i64;
    (seconds.min(i64::MAX as u64) as i64, nanoseconds)
}

fn sync_jiffies_from_rtc() {
    let ticks = crate::arch::rtc::ticks();
    let current = JIFFIES.load(Ordering::Acquire);
    if ticks > current {
        JIFFIES.store(ticks, Ordering::Release);
    }
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
    if try_acquire_compat_lock(state, owner) {
        return;
    }

    for spins in 1..=COMPAT_LOCK_SPIN_HARD_LIMIT {
        if matches!(spins, 1_000 | 100_000 | 1_000_000) {
            log_compat_lock_spin(state, owner);
        }
        spin_loop();
        if try_acquire_compat_lock(state, owner) {
            return;
        }
    }
    panic_compat_lock_timeout(state, owner);
}

fn acquire_compat_lock_irq(state: &'static CompatLockState, owner: usize) {
    if !interrupts::are_enabled() {
        acquire_compat_lock(state, owner);
        register_irq_lock_owner(owner, false);
        return;
    }

    for spins in 1..=COMPAT_LOCK_SPIN_HARD_LIMIT {
        interrupts::disable();
        if try_acquire_compat_lock(state, owner) {
            register_irq_lock_owner(owner, true);
            return;
        }
        interrupts::enable();

        if matches!(spins, 1_000 | 100_000 | 1_000_000) {
            log_compat_lock_spin(state, owner);
        }
        spin_loop();
    }
    panic_compat_lock_timeout(state, owner);
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

    if !owned_by_current {
        crate::debug::println!(
            "linux compat lock release mismatch: key={:#x} owner={} current_owner={} depth={} irq_enabled={}",
            state.key,
            owner,
            state.owner.load(Ordering::Relaxed),
            state.depth.load(Ordering::Relaxed),
            interrupts::are_enabled()
        );
    }

    state.depth.store(0, Ordering::Release);
    state.owner.store(0, Ordering::Release);
    state.held.store(false, Ordering::Release);
}

fn try_acquire_compat_lock(state: &'static CompatLockState, owner: usize) -> bool {
    if state.owner.load(Ordering::Acquire) == owner {
        state.depth.fetch_add(1, Ordering::AcqRel);
        return true;
    }

    if state
        .held
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }

    state.owner.store(owner, Ordering::Release);
    state.depth.store(1, Ordering::Release);
    true
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn log_compat_lock_spin(state: &'static CompatLockState, owner: usize) {
    crate::debug::println!(
        "linux compat lock spin: key={:#x} owner={} current_owner={} depth={} irq_enabled={}",
        state.key,
        owner,
        state.owner.load(Ordering::Relaxed),
        state.depth.load(Ordering::Relaxed),
        interrupts::are_enabled()
    );
}

fn panic_compat_lock_timeout(state: &'static CompatLockState, owner: usize) -> ! {
    panic!(
        "linux compat lock spin hard limit exceeded: key={:#x} owner={} current_owner={} depth={} irq_enabled={}",
        state.key,
        owner,
        state.owner.load(Ordering::Relaxed),
        state.depth.load(Ordering::Relaxed),
        interrupts::are_enabled()
    );
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn log_mutex_event(phase: &str, key: usize, owner: usize, current_owner: usize, depth: usize) {
    let remaining = MUTEX_DEBUG_REMAINING.load(Ordering::Relaxed);
    if remaining == 0 {
        return;
    }
    if MUTEX_DEBUG_REMAINING
        .compare_exchange(
            remaining,
            remaining - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    crate::debug::println!(
        "linux compat mutex {}: key={:#x} owner={} current_owner={} depth={} irq_enabled={}",
        phase,
        key,
        owner,
        current_owner,
        depth,
        interrupts::are_enabled()
    );
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

fn irq_lock_owner_depth(owner: usize) -> usize {
    IRQ_LOCK_OWNERS
        .lock()
        .iter()
        .find(|state| state.owner == owner)
        .map(|state| state.depth)
        .unwrap_or(0)
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
