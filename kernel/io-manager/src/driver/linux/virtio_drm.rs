use core::ffi::{c_char, c_void};

static COMPAT_DATA: [usize; 64] = [0; 64];
static COMPAT_VIDEO_FIRMWARE_DRIVERS_ONLY: i32 = 0;

unsafe extern "C" fn compat_printk(_fmt: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn drm_dev_register(_dev: *mut c_void, _flags: u64) -> i32 {
    0
}

unsafe extern "C" fn drm_dev_unregister(_dev: *mut c_void) {}

unsafe extern "C" fn drm_dev_alloc(_driver: *const c_void, _parent: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

unsafe extern "C" fn drm_dev_get(dev: *mut c_void) -> *mut c_void {
    dev
}

unsafe extern "C" fn drm_dev_put(_dev: *mut c_void) {}

unsafe extern "C" fn drm_printk_stub(_fmt: *const c_char) {}

unsafe extern "C" fn dma_fence_context_alloc(count: u32) -> u64 {
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(count.max(1) as u64, core::sync::atomic::Ordering::Relaxed)
}

unsafe extern "C" fn dma_fence_signal_locked(_fence: *mut c_void) -> i32 {
    0
}

unsafe extern "C" fn dma_fence_match_context(_fence: *mut c_void, _context: u64) -> i32 {
    0
}

unsafe extern "C" fn dma_fence_wait_timeout(
    _fence: *mut c_void,
    _intr: i32,
    timeout: isize,
) -> isize {
    timeout.max(0)
}

unsafe extern "C" fn dma_resv_test_signaled(_resv: *mut c_void, _usage: u32) -> i32 {
    1
}

unsafe extern "C" fn dma_resv_wait_timeout(
    _resv: *mut c_void,
    _usage: u32,
    _intr: i32,
    timeout: isize,
) -> isize {
    timeout.max(0)
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    if let Some(symbol) = resolve_symbol_meta(name) {
        return Some(symbol.addr);
    }
    match name {
        _ if is_virtio_drm_data_symbol(name) => Some(COMPAT_DATA.as_ptr() as usize),
        "video_firmware_drivers_only" => {
            Some((&COMPAT_VIDEO_FIRMWARE_DRIVERS_ONLY as *const i32) as usize)
        }
        _ => None,
    }
}

pub(crate) fn resolve_symbol_meta(name: &str) -> Option<super::LinuxCompatSymbol> {
    super::linux_compat_symbols!(name, {
        "drm_dev_register" => drm_dev_register;
        "drm_dev_unplug" => drm_dev_unregister;
        "drm_dev_alloc" => drm_dev_alloc;
        "drm_dev_get" => drm_dev_get;
        "drm_dev_put" => drm_dev_put;
        "__drm_dev_dbg" => drm_printk_stub, preserve_stack_tail;
        "__drm_err" => drm_printk_stub, preserve_stack_tail;
        "__drm_printfn_seq_file" => drm_printk_stub, preserve_stack_tail;
        "__drm_puts_seq_file" => drm_printk_stub, preserve_stack_tail;
        "drm_dev_printk" => drm_printk_stub, preserve_stack_tail;
        "dma_fence_context_alloc" => dma_fence_context_alloc;
        "dma_fence_signal_locked" => dma_fence_signal_locked;
        "dma_fence_match_context" => dma_fence_match_context;
        "dma_fence_wait_timeout" => dma_fence_wait_timeout;
        "dma_resv_test_signaled" => dma_resv_test_signaled;
        "dma_resv_wait_timeout" => dma_resv_wait_timeout;
        "__warn_printk" => compat_printk, preserve_stack_tail;
    })
}

pub(crate) fn symbol_abi(name: &str) -> Option<super::LinuxCompatExportAbi> {
    resolve_symbol_meta(name).map(|symbol| symbol.abi)
}

fn is_virtio_drm_data_symbol(name: &str) -> bool {
    name.starts_with("__SCK__tp_func_")
        || name.starts_with("__SCT__tp_func_")
        || name.starts_with("__tracepoint_")
        || matches!(
            name,
            "__SCT__might_resched"
                | "__SCT__preempt_schedule_notrace"
                | "__cpu_online_mask"
                | "__preempt_count"
                | "boot_cpu_data"
                | "cpu_number"
                | "iomem_resource"
                | "kmalloc_caches"
                | "param_ops_int"
                | "random_kmalloc_seed"
                | "system_wq"
                | "this_cpu_off"
                | "drm_gem_shmem_vm_ops"
                | "vmemmap_base"
        )
}
