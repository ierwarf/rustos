use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicU64, Ordering};

static JIFFIES: AtomicU64 = AtomicU64::new(0);
static BOOT_CPU_DATA: [u8; 256] = [0; 256];
static CPU_NUMBER: i32 = 0;
static THIS_CPU_OFF: usize = 0;
static X86_HYPER_TYPE: i32 = 0;

#[repr(C)]
pub(crate) struct LinuxTimespec64 {
    tv_sec: i64,
    tv_nsec: i64,
}

pub(crate) fn current_jiffies() -> u64 {
    JIFFIES.load(Ordering::Relaxed)
}

pub fn tick_jiffies(delta: u64) -> u64 {
    JIFFIES.fetch_add(delta, Ordering::Relaxed) + delta
}

pub(crate) fn service_compat_pending() {
    crate::multitask::cond_resched();
}

pub(crate) unsafe extern "C" fn get_random_bytes(buf: *mut c_void, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }

    let seed = JIFFIES
        .load(Ordering::Relaxed)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(buf as usize as u64);
    let mut state = seed ^ 0xa5a5_5a5a_d3c1_b2a0;
    let bytes = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
    for byte in bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
}

pub(crate) unsafe extern "C" fn _raw_spin_lock(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn _raw_spin_lock_bh(lock: *mut c_void) {
    unsafe { _raw_spin_lock(lock) };
}

pub(crate) unsafe extern "C" fn _raw_spin_lock_irq(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn _raw_spin_lock_irqsave(_lock: *mut c_void) -> usize {
    0
}

pub(crate) unsafe extern "C" fn _raw_spin_trylock(_lock: *mut c_void) -> i32 {
    1
}

pub(crate) unsafe extern "C" fn _raw_spin_unlock(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn _raw_spin_unlock_bh(lock: *mut c_void) {
    unsafe { _raw_spin_unlock(lock) };
}

pub(crate) unsafe extern "C" fn _raw_spin_unlock_irq(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn _raw_spin_unlock_irqrestore(_lock: *mut c_void, _flags: usize) {}

pub(crate) unsafe extern "C" fn __local_bh_enable_ip(_ip: usize, _cnt: u32) {}

pub(crate) unsafe extern "C" fn __rcu_read_lock() {}

pub(crate) unsafe extern "C" fn __rcu_read_unlock() {}

pub(crate) unsafe extern "C" fn __mutex_init(
    _lock: *mut c_void,
    _name: *const c_char,
    _key: *mut c_void,
) {
}

pub(crate) unsafe extern "C" fn mutex_lock(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn mutex_lock_interruptible(lock: *mut c_void) -> i32 {
    unsafe { mutex_lock(lock) };
    0
}

pub(crate) unsafe extern "C" fn mutex_lock_killable(lock: *mut c_void) -> i32 {
    unsafe { mutex_lock(lock) };
    0
}

pub(crate) unsafe extern "C" fn mutex_unlock(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn ww_mutex_lock(_lock: *mut c_void, _ctx: *mut c_void) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn ww_mutex_lock_interruptible(
    _lock: *mut c_void,
    _ctx: *mut c_void,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn ww_mutex_unlock(_lock: *mut c_void) {}

pub(crate) unsafe extern "C" fn schedule() {
    service_compat_pending();
}

pub(crate) unsafe extern "C" fn schedule_timeout(timeout: i64) -> i64 {
    if timeout > 0 {
        tick_jiffies(timeout as u64);
    }
    service_compat_pending();
    0
}

pub(crate) unsafe extern "C" fn cond_resched() -> i32 {
    service_compat_pending();
    0
}

pub(crate) unsafe extern "C" fn usleep_range_state(
    min_microseconds: u32,
    max_microseconds: u32,
    _state: u32,
) {
    let sleep_us = min_microseconds.max(max_microseconds);
    let sleep_ms = sleep_us.div_ceil(1000) as u64;
    if sleep_ms != 0 {
        tick_jiffies(sleep_ms);
    }
    service_compat_pending();
}

pub(crate) unsafe extern "C" fn __msecs_to_jiffies(milliseconds: u32) -> u64 {
    milliseconds as u64
}

pub(crate) unsafe extern "C" fn jiffies_to_usecs(jiffies: u64) -> u64 {
    jiffies.saturating_mul(1000)
}

pub(crate) unsafe extern "C" fn ktime_get() -> i64 {
    unsafe { ktime_get_mono_fast_ns() as i64 }
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

pub(crate) unsafe extern "C" fn sched_clock() -> u64 {
    unsafe { ktime_get_mono_fast_ns() }
}

pub(crate) unsafe extern "C" fn sg_init_table(sgl: *mut c_void, nents: u32) {
    if !sgl.is_null() && nents != 0 {
        unsafe { core::ptr::write_bytes(sgl, 0, nents as usize * 24) };
    }
}

pub(crate) unsafe extern "C" fn sg_init_one(sg: *mut c_void, buf: *const c_void, buflen: u32) {
    if sg.is_null() {
        return;
    }
    unsafe {
        let words = core::slice::from_raw_parts_mut(sg.cast::<usize>(), 3);
        words[0] = buf as usize | 0x2;
        words[1] = buflen as usize;
        words[2] = buf as usize;
    }
}

pub(crate) unsafe extern "C" fn refcount_warn_saturate(_ptr: *mut c_void, _type_: i32) {}

pub(crate) unsafe extern "C" fn _printk(_fmt: *const c_char) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn _dev_printk(
    _level: *const c_char,
    _dev: *mut c_void,
    _fmt: *const c_char,
) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn _dev_err(_dev: *mut c_void, _fmt: *const c_char) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn _dev_warn(_dev: *mut c_void, _fmt: *const c_char) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn _dev_notice(_dev: *mut c_void, _fmt: *const c_char) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn _dev_info(_dev: *mut c_void, _fmt: *const c_char) -> i32 {
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

pub(crate) unsafe extern "C" fn scnprintf(
    dest: *mut c_char,
    size: usize,
    fmt: *const c_char,
) -> i32 {
    copy_format_string(dest, size, fmt)
}

pub(crate) unsafe extern "C" fn sprintf(dest: *mut c_char, fmt: *const c_char) -> i32 {
    copy_format_string(dest, usize::MAX, fmt)
}

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
        "ww_mutex_lock" => Some(ww_mutex_lock as *const () as usize),
        "ww_mutex_lock_interruptible" => Some(ww_mutex_lock_interruptible as *const () as usize),
        "ww_mutex_unlock" => Some(ww_mutex_unlock as *const () as usize),
        "usleep_range_state" => Some(usleep_range_state as *const () as usize),
        "schedule" => Some(schedule as *const () as usize),
        "schedule_timeout" => Some(schedule_timeout as *const () as usize),
        "cond_resched" => Some(cond_resched as *const () as usize),
        "_cond_resched" => Some(cond_resched as *const () as usize),
        "__cond_resched" => Some(cond_resched as *const () as usize),
        "__msecs_to_jiffies" => Some(__msecs_to_jiffies as *const () as usize),
        "jiffies_to_usecs" => Some(jiffies_to_usecs as *const () as usize),
        "ktime_get_coarse_ts64" => Some(ktime_get_coarse_ts64 as *const () as usize),
        "ktime_get" => Some(ktime_get as *const () as usize),
        "ktime_get_mono_fast_ns" => Some(ktime_get_mono_fast_ns as *const () as usize),
        "sched_clock" => Some(sched_clock as *const () as usize),
        "jiffies" => Some(&JIFFIES as *const AtomicU64 as usize),
        "boot_cpu_data" => Some(&BOOT_CPU_DATA as *const [u8; 256] as usize),
        "cpu_number" => Some(&CPU_NUMBER as *const i32 as usize),
        "this_cpu_off" => Some(&THIS_CPU_OFF as *const usize as usize),
        "x86_hyper_type" => Some(&X86_HYPER_TYPE as *const i32 as usize),
        "get_random_bytes" => Some(get_random_bytes as *const () as usize),
        "sg_init_table" => Some(sg_init_table as *const () as usize),
        "sg_init_one" => Some(sg_init_one as *const () as usize),
        "refcount_warn_saturate" => Some(refcount_warn_saturate as *const () as usize),
        "_printk" => Some(_printk as *const () as usize),
        "_dev_printk" => Some(_dev_printk as *const () as usize),
        "_dev_err" => Some(_dev_err as *const () as usize),
        "_dev_warn" => Some(_dev_warn as *const () as usize),
        "_dev_notice" => Some(_dev_notice as *const () as usize),
        "_dev_info" => Some(_dev_info as *const () as usize),
        "__dynamic_dev_dbg" => Some(__dynamic_dev_dbg as *const () as usize),
        "snprintf" => Some(snprintf as *const () as usize),
        "scnprintf" => Some(scnprintf as *const () as usize),
        "sprintf" => Some(sprintf as *const () as usize),
        _ => None,
    }
}

fn monotonic_time_parts() -> (i64, i64) {
    let ticks = crate::arch::rtc::ticks();
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let seconds = ticks / ticks_per_second;
    let nanoseconds = (ticks % ticks_per_second).saturating_mul(1_000_000_000) / ticks_per_second;
    (seconds as i64, nanoseconds as i64)
}

fn copy_format_string(dest: *mut c_char, size: usize, fmt: *const c_char) -> i32 {
    if dest.is_null() || size == 0 {
        return 0;
    }
    let bytes = cstr_bytes(fmt).unwrap_or(&[]);
    let copy_limit = size.saturating_sub(1).min(4096);
    let copy_len = bytes.len().min(copy_limit);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dest.cast::<u8>(), copy_len);
        *dest.add(copy_len) = 0;
    }
    copy_len as i32
}

fn cstr_bytes<'a>(fmt: *const c_char) -> Option<&'a [u8]> {
    if fmt.is_null() {
        return None;
    }
    let mut len = 0usize;
    let mut cursor = fmt;
    while len < 4096 && unsafe { *cursor } != 0 {
        len += 1;
        cursor = unsafe { cursor.add(1) };
    }
    Some(unsafe { core::slice::from_raw_parts(fmt as *const u8, len) })
}
