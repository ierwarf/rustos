use core::ffi::c_void;

unsafe extern "C" fn compat_zero() -> usize {
    0
}

unsafe extern "C" fn compat_null() -> *mut c_void {
    core::ptr::null_mut()
}

unsafe extern "C" fn register_virtio_driver(driver: *mut c_void) -> i32 {
    crate::debug::info!(
        driver,
        "linux compat: virtio register driver ptr={:#x} status=registered-no-bus-binding",
        driver as usize
    );
    let _ = crate::driver::virtio_gpu::try_enable_primary_display();
    0
}

unsafe extern "C" fn unregister_virtio_driver(driver: *mut c_void) {
    crate::debug::info!(
        driver,
        "linux compat: virtio unregister driver ptr={:#x}",
        driver as usize
    );
}

unsafe extern "C" fn is_virtio_device(_dev: *mut c_void) -> i32 {
    1
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__register_virtio_driver" => Some(register_virtio_driver as *const () as usize),
        "unregister_virtio_driver" => Some(unregister_virtio_driver as *const () as usize),
        "is_virtio_device" => Some(is_virtio_device as *const () as usize),
        _ if is_stubbed_virtio_symbol(name) => Some(compat_zero as *const () as usize),
        _ if is_stubbed_virtio_pointer_symbol(name) => Some(compat_null as *const () as usize),
        _ => None,
    }
}

fn is_stubbed_virtio_symbol(name: &str) -> bool {
    name.starts_with("virtio_") || name.starts_with("virtqueue_")
}

fn is_stubbed_virtio_pointer_symbol(name: &str) -> bool {
    matches!(name, "virtqueue_dma_dev")
}
