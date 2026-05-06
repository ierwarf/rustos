use core::ffi::c_void;

use super::compat::{LinuxCompatDeviceDriver, compat_cstr};

unsafe extern "C" fn compat_zero() -> usize {
    0
}

unsafe extern "C" fn compat_enosys() -> isize {
    -38
}

unsafe extern "C" fn compat_null() -> *mut c_void {
    core::ptr::null_mut()
}

unsafe extern "C" fn register_virtio_driver(driver: *mut c_void) -> i32 {
    let driver_name = virtio_driver_name(driver);
    let driver_name_hash = driver_name.map(stable_ascii_hash).unwrap_or(0);
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-register",
        driver as usize as u64,
        driver_name_hash,
    );
    let status = if matches!(driver_name, Some("virtio_net")) {
        crate::network::note_virtio_net_driver_registered();
        0
    } else {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "linux-virtio-register-other",
            driver as usize as u64,
            driver_name_hash,
        );
        -19
    };
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-register-return",
        driver as usize as u64,
        driver_name_hash,
    );
    status
}

unsafe extern "C" fn unregister_virtio_driver(driver: *mut c_void) {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-unregister",
        driver as usize as u64,
        0,
    );
}

unsafe extern "C" fn is_virtio_device(_dev: *mut c_void) -> i32 {
    1
}

fn virtio_driver_name(driver: *mut c_void) -> Option<&'static str> {
    if driver.is_null() {
        return None;
    }
    let device_driver = driver.cast::<LinuxCompatDeviceDriver>();
    let name = unsafe { (*device_driver).name };
    compat_cstr(name)
}

fn stable_ascii_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    if let Some(symbol) = resolve_symbol_meta(name) {
        return Some(symbol.addr);
    }
    match name {
        _ if is_unimplemented_virtqueue_status_symbol(name) => {
            Some(compat_enosys as *const () as usize)
        }
        _ if is_stubbed_virtio_symbol(name) => Some(compat_zero as *const () as usize),
        _ if is_stubbed_virtio_pointer_symbol(name) => Some(compat_null as *const () as usize),
        _ => None,
    }
}

pub(crate) fn resolve_symbol_meta(name: &str) -> Option<super::LinuxCompatSymbol> {
    super::linux_compat_symbols!(name, {
        "__register_virtio_driver" => register_virtio_driver;
        "unregister_virtio_driver" => unregister_virtio_driver;
        "is_virtio_device" => is_virtio_device;
    })
}

pub(crate) fn symbol_abi(name: &str) -> Option<super::LinuxCompatExportAbi> {
    resolve_symbol_meta(name).map(|symbol| symbol.abi)
}

fn is_unimplemented_virtqueue_status_symbol(name: &str) -> bool {
    matches!(
        name,
        "virtqueue_add_inbuf"
            | "virtqueue_add_inbuf_premapped"
            | "virtqueue_add_outbuf"
            | "virtqueue_add_outbuf_premapped"
            | "virtqueue_add_sgs"
            | "virtqueue_dma_mapping_error"
            | "virtqueue_kick"
            | "virtqueue_notify"
            | "virtqueue_reset"
            | "virtqueue_resize"
    )
}

fn is_stubbed_virtio_symbol(name: &str) -> bool {
    name.starts_with("virtio_") || name.starts_with("virtqueue_")
}

fn is_stubbed_virtio_pointer_symbol(name: &str) -> bool {
    matches!(name, "virtqueue_dma_dev")
}
