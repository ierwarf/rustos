// RING3-MIGRATION-REFERENCE START: Linux .ko virtio-drm shims are explicit
// ring0 compatibility substrate. Display/provider policy belongs in
// uiserver/driverd.
use core::ffi::{c_char, c_void};

static COMPAT_DATA: [usize; 64] = [0; 64];
static COMPAT_VIDEO_FIRMWARE_DRIVERS_ONLY: i32 = 0;

unsafe extern "C" fn compat_printk(_fmt: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn drm_dev_register(_dev: *mut c_void, _flags: u64) -> i32 {
    crate::driver::symbol_events::record_drm_probe_init_symbol(
        "drm_dev_register",
        _dev as usize,
        _flags,
    );
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "virtio-drm-dev-register",
        _dev as usize as u64,
        _flags,
    );
    match crate::driver::linux::virtio_gpu::ensure_primary_provider() {
        Ok(()) => crate::debug::info!(display, "virtio-gpu: drm provider ready"),
        Err(err) => crate::debug::warn!(
            display,
            "virtio-gpu: drm provider publish failed: {:?}",
            err
        ),
    }
    0
}

unsafe extern "C" fn drm_dev_unregister(_dev: *mut c_void) {
    crate::driver::symbol_events::record_drm_probe_init_symbol(
        "drm_dev_unregister",
        _dev as usize,
        0,
    );
}

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

unsafe extern "C" fn compat_return_zero() -> usize {
    0
}

unsafe extern "C" fn compat_return_one() -> usize {
    1
}

unsafe extern "C" fn compat_noop() {}

unsafe extern "C" fn compat_return_null() -> *mut c_void {
    core::ptr::null_mut()
}

unsafe extern "C" fn compat_alloc(size: usize) -> *mut c_void {
    unsafe { super::base::__kmalloc_noprof(size.max(8), 0) }
}

unsafe extern "C" fn compat_kmalloc(_owner: *mut c_void, size: usize, gfp: u32) -> *mut c_void {
    unsafe { super::base::__kmalloc_noprof(size.max(8), gfp) }
}

unsafe extern "C" fn compat_cache_alloc(_cache: *mut c_void, gfp: u32) -> *mut c_void {
    unsafe { super::base::__kmalloc_noprof(4096, gfp) }
}

unsafe extern "C" fn drm_gem_shmem_create(_dev: *mut c_void, size: usize) -> *mut c_void {
    unsafe { super::base::__kmalloc_noprof(size.max(8), 0) }
}

unsafe extern "C" fn compat_sg_alloc_table(table: *mut c_void, nents: u32, _gfp: u32) -> i32 {
    if table.is_null() {
        return -22;
    }
    unsafe {
        core::ptr::write_bytes(table, 0, 32);
        let words = core::slice::from_raw_parts_mut(table.cast::<usize>(), 4);
        words[1] = nents as usize;
        words[2] = nents as usize;
    }
    0
}

unsafe extern "C" fn compat_sg_free_table(_table: *mut c_void) {}

unsafe extern "C" fn compat_free(ptr: *mut c_void) {
    unsafe { super::base::kfree(ptr) };
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
        _ if is_virtio_drm_one_stub(name) => Some(compat_return_one as *const () as usize),
        _ if is_virtio_drm_alloc_stub(name) => Some(resolve_alloc_stub(name)),
        _ if is_virtio_drm_free_stub(name) => Some(compat_free as *const () as usize),
        _ if is_virtio_drm_zero_stub(name) => Some(compat_return_zero as *const () as usize),
        _ if is_virtio_drm_noop_stub(name) => Some(compat_noop as *const () as usize),
        _ if is_virtio_drm_null_stub(name) => Some(compat_return_null as *const () as usize),
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
        "drm_dev_enter" => compat_return_one;
        "drm_dev_exit" => compat_noop;
        "seq_write" => compat_return_zero;
        "sg_alloc_table" => compat_sg_alloc_table;
        "sg_free_table" => compat_sg_free_table;
        "sync_file_create" => compat_return_null;
        "sync_file_get_fence" => compat_return_null;
        "trace_event_buffer_commit" => compat_noop;
        "trace_event_buffer_reserve" => compat_return_null;
        "trace_event_printf" => compat_return_zero;
        "trace_event_raw_init" => compat_return_zero;
        "trace_event_reg" => compat_return_zero;
        "trace_handle_return" => compat_return_zero;
        "trace_raw_output_prep" => compat_return_zero;
        "vga_get" => compat_return_zero;
        "vga_put" => compat_noop;
        "vm_get_page_prot" => compat_return_zero;
        "vmalloc_to_page" => compat_return_null;
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

fn is_virtio_drm_zero_stub(name: &str) -> bool {
    matches!(
        name,
        "__devm_request_region"
            | "__dma_sync_sg_for_device"
            | "__drm_atomic_helper_plane_duplicate_state"
            | "__kmem_cache_create_args"
            | "__trace_trigger_soft_disabled"
            | "__vma_start_write"
            | "aperture_remove_conflicting_pci_devices"
            | "bpf_trace_run3"
            | "cachemode2protval"
            | "cc_mkdec"
            | "dma_buf_dynamic_attach"
            | "dma_buf_map_attachment"
            | "dma_buf_pin"
            | "dma_buf_unmap_attachment"
            | "dma_fence_init"
            | "dma_fence_release"
            | "dma_fence_unwrap_first"
            | "dma_fence_unwrap_next"
            | "dma_map_resource"
            | "dma_max_mapping_size"
            | "dma_resv_add_fence"
            | "dma_resv_reserve_fences"
            | "seq_write"
            | "trace_event_printf"
            | "trace_event_raw_init"
            | "trace_event_reg"
            | "trace_handle_return"
            | "trace_raw_output_prep"
            | "vga_get"
            | "vm_get_page_prot"
            | "drm_add_modes_noedid"
            | "drm_atomic_get_crtc_state"
            | "drm_atomic_helper_check"
            | "drm_atomic_helper_check_plane_state"
            | "drm_atomic_helper_commit"
            | "drm_atomic_helper_connector_duplicate_state"
            | "drm_atomic_helper_crtc_duplicate_state"
            | "drm_atomic_helper_damage_merged"
            | "drm_atomic_helper_dirtyfb"
            | "drm_atomic_helper_page_flip"
            | "drm_atomic_helper_plane_reset"
            | "drm_atomic_helper_set_config"
            | "drm_atomic_helper_update_plane"
            | "drm_client_setup"
            | "drm_compat_ioctl"
            | "drm_connector_attach_edid_property"
            | "drm_connector_attach_encoder"
            | "drm_connector_init"
            | "drm_connector_register"
            | "drm_crtc_init_with_planes"
            | "drm_cvt_mode"
            | "drm_edid_connector_add_modes"
            | "drm_edid_connector_update"
            | "drm_edid_read_custom"
            | "drm_event_reserve_init"
            | "drm_fbdev_shmem_driver_fbdev_probe"
            | "drm_framebuffer_init"
            | "drm_gem_create_mmap_offset"
            | "drm_gem_dumb_map_offset"
            | "drm_gem_fb_create_handle"
            | "drm_gem_handle_create"
            | "drm_gem_lock_reservations"
            | "drm_gem_plane_helper_prepare_fb"
            | "drm_gem_private_object_init"
            | "drm_gem_shmem_get_pages_sgt"
            | "drm_gem_shmem_get_sg_table"
            | "drm_gem_shmem_pin_locked"
            | "drm_gem_shmem_print_info"
            | "drm_gem_shmem_vmap_locked"
            | "drm_gem_unlock_reservations"
            | "drm_helper_hpd_irq_event"
            | "drm_helper_mode_fill_fb_struct"
            | "drm_helper_probe_single_connector_modes"
            | "drm_ioctl"
            | "drm_kms_helper_hotplug_event"
            | "drm_mm_insert_node_in_range"
            | "drm_open"
            | "drm_poll"
            | "drm_read"
            | "drm_release"
            | "drm_send_event"
            | "drm_set_preferred_mode"
            | "drm_simple_encoder_init"
            | "drm_syncobj_add_point"
            | "drm_syncobj_find_fence"
            | "drm_syncobj_replace_fence"
            | "fd_install"
            | "fput"
            | "get_unused_fd_flags"
            | "ida_alloc_range"
            | "is_vmalloc_addr"
            | "pci_get_device"
            | "perf_trace_buf_alloc"
            | "perf_trace_run_bpf_submit"
            | "pgprot_writecombine"
            | "remap_pfn_range"
    )
}

fn is_virtio_drm_one_stub(name: &str) -> bool {
    matches!(name, "drm_dev_enter")
}

fn is_virtio_drm_noop_stub(name: &str) -> bool {
    matches!(
        name,
        "__drmm_universal_plane_alloc"
            | "dma_buf_detach"
            | "dma_buf_put"
            | "dma_buf_unpin"
            | "dma_unmap_resource"
            | "drm_atomic_helper_connector_destroy_state"
            | "drm_atomic_helper_connector_reset"
            | "drm_atomic_helper_crtc_destroy_state"
            | "drm_atomic_helper_crtc_reset"
            | "drm_atomic_helper_disable_plane"
            | "drm_atomic_helper_plane_destroy_state"
            | "drm_atomic_helper_shutdown"
            | "drm_connector_cleanup"
            | "drm_connector_unregister"
            | "drm_crtc_cleanup"
            | "drm_debugfs_create_files"
            | "drm_dev_exit"
            | "drm_edid_free"
            | "drm_gem_dmabuf_mmap"
            | "drm_gem_dmabuf_release"
            | "drm_gem_dmabuf_vmap"
            | "drm_gem_dmabuf_vunmap"
            | "drm_gem_fb_destroy"
            | "drm_gem_free_mmap_offset"
            | "drm_gem_map_attach"
            | "drm_gem_map_detach"
            | "drm_gem_map_dma_buf"
            | "drm_gem_mmap"
            | "drm_gem_object_free"
            | "drm_gem_object_lookup"
            | "drm_gem_object_release"
            | "drm_gem_prime_import"
            | "drm_gem_shmem_free"
            | "drm_gem_shmem_mmap"
            | "drm_gem_shmem_unpin_locked"
            | "drm_gem_shmem_vunmap_locked"
            | "drm_gem_unmap_dma_buf"
            | "drm_gem_vm_close"
            | "drm_gem_vm_open"
            | "drm_mm_init"
            | "drm_mm_print"
            | "drm_mm_remove_node"
            | "drm_mm_takedown"
            | "drm_mode_config_reset"
            | "drm_mode_probed_add"
            | "drm_plane_enable_fb_damage_clips"
            | "drm_syncobj_free"
            | "drm_syncobj_replace_fence"
            | "drmm_mode_config_init"
            | "ida_free"
            | "kmem_cache_destroy"
            | "kmem_cache_free"
            | "pci_dev_put"
            | "put_unused_fd"
            | "trace_event_buffer_commit"
            | "vga_put"
    )
}

fn is_virtio_drm_alloc_stub(name: &str) -> bool {
    matches!(
        name,
        "drm_gem_shmem_create" | "drm_syncobj_find" | "drmm_kmalloc" | "kmem_cache_alloc_noprof"
    )
}

fn is_virtio_drm_free_stub(name: &str) -> bool {
    matches!(name, "drmm_kfree")
}

fn is_virtio_drm_null_stub(name: &str) -> bool {
    matches!(
        name,
        "sync_file_create"
            | "sync_file_get_fence"
            | "trace_event_buffer_reserve"
            | "vmalloc_to_page"
    )
}

fn resolve_alloc_stub(name: &str) -> usize {
    match name {
        "drmm_kmalloc" => compat_kmalloc as *const () as usize,
        "kmem_cache_alloc_noprof" => compat_cache_alloc as *const () as usize,
        "drm_gem_shmem_create" => drm_gem_shmem_create as *const () as usize,
        _ => compat_alloc as *const () as usize,
    }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko virtio-drm compatibility substrate exception.
