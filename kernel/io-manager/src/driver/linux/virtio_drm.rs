use core::ffi::{c_char, c_void};

static COMPAT_DATA: [usize; 64] = [0; 64];
static COMPAT_VIDEO_FIRMWARE_DRIVERS_ONLY: i32 = 0;

unsafe extern "C" fn compat_zero() -> usize {
    0
}

unsafe extern "C" fn compat_null() -> *mut c_void {
    core::ptr::null_mut()
}

unsafe extern "C" fn compat_printk(_fmt: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn register_virtio_driver(driver: *mut c_void) -> i32 {
    crate::debug::println!(
        "linux compat: virtio register driver ptr={:#x} status=registered-no-bus-binding",
        driver as usize
    );
    let _ = crate::driver::virtio_gpu::try_enable_primary_display();
    0
}

unsafe extern "C" fn unregister_virtio_driver(driver: *mut c_void) {
    crate::debug::println!(
        "linux compat: virtio unregister driver ptr={:#x}",
        driver as usize
    );
}

unsafe extern "C" fn is_virtio_device(_dev: *mut c_void) -> i32 {
    1
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
    match name {
        "__register_virtio_driver" => Some(register_virtio_driver as *const () as usize),
        "unregister_virtio_driver" => Some(unregister_virtio_driver as *const () as usize),
        "is_virtio_device" => Some(is_virtio_device as *const () as usize),

        "drm_dev_register" => Some(drm_dev_register as *const () as usize),
        "drm_dev_unplug" => Some(drm_dev_unregister as *const () as usize),
        "drm_dev_alloc" => Some(drm_dev_alloc as *const () as usize),
        "drm_dev_get" => Some(drm_dev_get as *const () as usize),
        "drm_dev_put" => Some(drm_dev_put as *const () as usize),
        "__drm_dev_dbg"
        | "__drm_err"
        | "__drm_printfn_seq_file"
        | "__drm_puts_seq_file"
        | "drm_dev_printk" => Some(drm_printk_stub as *const () as usize),

        "dma_fence_context_alloc" => Some(dma_fence_context_alloc as *const () as usize),
        "dma_fence_signal_locked" => Some(dma_fence_signal_locked as *const () as usize),
        "dma_fence_match_context" => Some(dma_fence_match_context as *const () as usize),
        "dma_fence_wait_timeout" => Some(dma_fence_wait_timeout as *const () as usize),
        "dma_resv_test_signaled" => Some(dma_resv_test_signaled as *const () as usize),
        "dma_resv_wait_timeout" => Some(dma_resv_wait_timeout as *const () as usize),

        "__warn_printk" => Some(compat_printk as *const () as usize),

        _ if is_virtio_drm_data_symbol(name) => Some(COMPAT_DATA.as_ptr() as usize),
        "video_firmware_drivers_only" => {
            Some((&COMPAT_VIDEO_FIRMWARE_DRIVERS_ONLY as *const i32) as usize)
        }
        _ if is_stubbed_virtio_drm_pointer_symbol(name) => Some(compat_null as *const () as usize),
        _ if is_stubbed_virtio_drm_symbol(name) => Some(compat_zero as *const () as usize),
        _ => None,
    }
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

fn is_stubbed_virtio_drm_symbol(name: &str) -> bool {
    name.starts_with("drm_")
        || name.starts_with("__drm_")
        || name.starts_with("__drmm_")
        || name.starts_with("__devm_")
        || name.starts_with("__dma_")
        || name.starts_with("__ubsan_")
        || name.starts_with("__vma_")
        || name.starts_with("drmm_")
        || name.starts_with("dma_buf_")
        || name.starts_with("dma_fence_")
        || name.starts_with("dma_resv_")
        || name.starts_with("virtio_")
        || name.starts_with("virtqueue_")
        || name.starts_with("sync_file_")
        || name.starts_with("ww_mutex_")
        || name.starts_with("trace_event_")
        || name.starts_with("perf_trace_")
        || name.starts_with("bpf_trace_")
        || name.starts_with("sg_")
        || name.starts_with("seq_")
        || name.starts_with("__traceiter_")
        || name.starts_with("__probestub_")
        || matches!(
            name,
            "aperture_remove_conflicting_pci_devices"
                | "___ratelimit"
                | "cachemode2protval"
                | "cc_mkdec"
                | "__kmem_cache_create_args"
                | "kmem_cache_alloc_noprof"
                | "kmem_cache_destroy"
                | "kmem_cache_free"
                | "strncpy_from_user"
                | "dma_map_resource"
                | "dma_max_mapping_size"
                | "dma_unmap_resource"
                | "fd_install"
                | "finish_wait"
                | "flush_work"
                | "fput"
                | "get_unused_fd_flags"
                | "ida_alloc_range"
                | "ida_free"
                | "is_vmalloc_addr"
                | "noop_llseek"
                | "pci_dev_put"
                | "pci_get_device"
                | "pci_is_vga"
                | "pgprot_writecombine"
                | "put_unused_fd"
                | "remap_pfn_range"
                | "schedule"
                | "schedule_timeout"
                | "sync_file_create"
                | "sync_file_get_fence"
                | "trace_handle_return"
                | "trace_raw_output_prep"
                | "validate_usercopy_range"
                | "vga_get"
                | "vga_get_interruptible"
                | "vga_put"
                | "vm_get_page_prot"
                | "vmalloc_to_page"
                | "__trace_trigger_soft_disabled"
                | "__wake_up"
                | "init_wait_entry"
                | "prepare_to_wait_event"
                | "queue_work_on"
        )
}

fn is_stubbed_virtio_drm_pointer_symbol(name: &str) -> bool {
    matches!(
        name,
        "drm_edid_read_custom"
            | "drm_gem_object_lookup"
            | "drm_gem_prime_import"
            | "drm_gem_shmem_create"
            | "drm_syncobj_find"
            | "drm_syncobj_find_fence"
            | "dma_buf_dynamic_attach"
            | "dma_buf_map_attachment"
            | "dma_fence_unwrap_first"
            | "dma_fence_unwrap_next"
            | "memdup_user"
            | "vmemdup_user"
    )
}
