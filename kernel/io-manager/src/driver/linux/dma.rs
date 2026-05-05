use core::ffi::c_void;

pub(crate) unsafe extern "C" fn dma_set_mask_and_coherent(dev: *mut c_void, mask: u64) -> i32 {
    crate::driver::dma::set_mask_and_coherent(dev, mask)
}

pub(crate) unsafe extern "C" fn dma_set_mask(dev: *mut c_void, mask: u64) -> i32 {
    crate::driver::dma::set_mask(dev, mask)
}

pub(crate) unsafe extern "C" fn dma_set_coherent_mask(dev: *mut c_void, mask: u64) -> i32 {
    crate::driver::dma::set_coherent_mask(dev, mask)
}

pub(crate) unsafe extern "C" fn dma_alloc_attrs(
    dev: *mut c_void,
    size: usize,
    dma_handle: *mut u64,
    _gfp: u32,
    _attrs: u64,
) -> *mut c_void {
    crate::driver::dma::alloc_coherent(dev, size, dma_handle)
}

pub(crate) unsafe extern "C" fn dma_alloc_coherent(
    dev: *mut c_void,
    size: usize,
    dma_handle: *mut u64,
    gfp: u32,
) -> *mut c_void {
    unsafe { dma_alloc_attrs(dev, size, dma_handle, gfp, 0) }
}

pub(crate) unsafe extern "C" fn dma_free_attrs(
    dev: *mut c_void,
    _size: usize,
    cpu_addr: *mut c_void,
    dma_handle: u64,
    _attrs: u64,
) {
    crate::driver::dma::free_coherent(dev, cpu_addr, dma_handle);
}

pub(crate) unsafe extern "C" fn dma_free_coherent(
    dev: *mut c_void,
    size: usize,
    cpu_addr: *mut c_void,
    dma_handle: u64,
) {
    unsafe { dma_free_attrs(dev, size, cpu_addr, dma_handle, 0) };
}

pub(crate) unsafe extern "C" fn dmam_alloc_attrs(
    dev: *mut c_void,
    size: usize,
    dma_handle: *mut u64,
    gfp: u32,
    attrs: u64,
) -> *mut c_void {
    let cpu_addr = unsafe { dma_alloc_attrs(dev, size, dma_handle, gfp, attrs) };
    if !cpu_addr.is_null() && !dma_handle.is_null() {
        crate::driver::devres::register_dma_coherent(dev, size, cpu_addr, unsafe { *dma_handle });
    }
    cpu_addr
}

pub(crate) unsafe extern "C" fn dmam_alloc_coherent(
    dev: *mut c_void,
    size: usize,
    dma_handle: *mut u64,
    gfp: u32,
) -> *mut c_void {
    let cpu_addr = unsafe { dma_alloc_coherent(dev, size, dma_handle, gfp) };
    if !cpu_addr.is_null() && !dma_handle.is_null() {
        crate::driver::devres::register_dma_coherent(dev, size, cpu_addr, unsafe { *dma_handle });
    }
    cpu_addr
}

pub(crate) unsafe extern "C" fn dmam_free_coherent(
    dev: *mut c_void,
    size: usize,
    cpu_addr: *mut c_void,
    dma_handle: u64,
) {
    crate::driver::devres::forget_dma_coherent(dev, cpu_addr, dma_handle);
    let _ = size;
    crate::driver::dma::free_coherent(dev, cpu_addr, dma_handle);
}

pub(crate) unsafe extern "C" fn dma_map_single(
    dev: *mut c_void,
    cpu_addr: *mut c_void,
    size: usize,
    _dir: u32,
) -> u64 {
    crate::driver::dma::map_single(dev, cpu_addr, size)
}

pub(crate) unsafe extern "C" fn dma_map_single_attrs(
    dev: *mut c_void,
    cpu_addr: *mut c_void,
    size: usize,
    dir: u32,
    _attrs: u64,
) -> u64 {
    unsafe { dma_map_single(dev, cpu_addr, size, dir) }
}

pub(crate) unsafe extern "C" fn dma_unmap_single(
    dev: *mut c_void,
    dma_addr: u64,
    size: usize,
    _dir: u32,
) {
    crate::driver::dma::unmap_single(dev, dma_addr, size);
}

pub(crate) unsafe extern "C" fn dma_unmap_single_attrs(
    dev: *mut c_void,
    dma_addr: u64,
    size: usize,
    dir: u32,
    _attrs: u64,
) {
    unsafe { dma_unmap_single(dev, dma_addr, size, dir) };
}

pub(crate) unsafe extern "C" fn dma_mapping_error(_dev: *mut c_void, dma_addr: u64) -> i32 {
    crate::driver::dma::mapping_error(dma_addr)
}

pub(crate) unsafe extern "C" fn dma_sync_single_for_cpu(
    dev: *mut c_void,
    dma_addr: u64,
    size: usize,
    _dir: u32,
) {
    crate::driver::dma::sync_single_for_cpu(dev, dma_addr, size);
}

pub(crate) unsafe extern "C" fn dma_sync_single_for_device(
    dev: *mut c_void,
    dma_addr: u64,
    size: usize,
    _dir: u32,
) {
    crate::driver::dma::sync_single_for_device(dev, dma_addr, size);
}

pub(crate) unsafe extern "C" fn dma_sync_single_range_for_cpu(
    dev: *mut c_void,
    dma_addr: u64,
    offset: usize,
    size: usize,
    dir: u32,
) {
    crate::driver::dma::sync_single_for_cpu(dev, dma_addr.saturating_add(offset as u64), size);
    let _ = dir;
}

pub(crate) unsafe extern "C" fn dma_sync_single_range_for_device(
    dev: *mut c_void,
    dma_addr: u64,
    offset: usize,
    size: usize,
    dir: u32,
) {
    crate::driver::dma::sync_single_for_device(dev, dma_addr.saturating_add(offset as u64), size);
    let _ = dir;
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "dma_set_mask_and_coherent" => Some(dma_set_mask_and_coherent as *const () as usize),
        "dma_set_mask" => Some(dma_set_mask as *const () as usize),
        "dma_set_coherent_mask" => Some(dma_set_coherent_mask as *const () as usize),
        "dma_alloc_attrs" => Some(dma_alloc_attrs as *const () as usize),
        "dma_alloc_coherent" => Some(dma_alloc_coherent as *const () as usize),
        "dma_free_attrs" => Some(dma_free_attrs as *const () as usize),
        "dma_free_coherent" => Some(dma_free_coherent as *const () as usize),
        "dmam_alloc_attrs" => Some(dmam_alloc_attrs as *const () as usize),
        "dmam_alloc_coherent" => Some(dmam_alloc_coherent as *const () as usize),
        "dmam_free_coherent" => Some(dmam_free_coherent as *const () as usize),
        "dma_map_single" => Some(dma_map_single as *const () as usize),
        "dma_map_single_attrs" => Some(dma_map_single_attrs as *const () as usize),
        "dma_unmap_single" => Some(dma_unmap_single as *const () as usize),
        "dma_unmap_single_attrs" => Some(dma_unmap_single_attrs as *const () as usize),
        "dma_mapping_error" => Some(dma_mapping_error as *const () as usize),
        "dma_sync_single_for_cpu" => Some(dma_sync_single_for_cpu as *const () as usize),
        "__dma_sync_single_for_cpu" => Some(dma_sync_single_for_cpu as *const () as usize),
        "dma_sync_single_for_device" => Some(dma_sync_single_for_device as *const () as usize),
        "__dma_sync_single_for_device" => Some(dma_sync_single_for_device as *const () as usize),
        "dma_sync_single_range_for_cpu" => {
            Some(dma_sync_single_range_for_cpu as *const () as usize)
        }
        "dma_sync_single_range_for_device" => {
            Some(dma_sync_single_range_for_device as *const () as usize)
        }
        _ => None,
    }
}
