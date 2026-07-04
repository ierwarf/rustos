// RING3-MIGRATION-REFERENCE START: Linux .ko MMIO mapping shim compatibility
// substrate exception. driverd owns driver policy; ring0 keeps privileged
// MMIO map helpers required by in-kernel .ko execution.
use core::ffi::c_void;

use super::compat::LinuxCompatResource;

const MEMREMAP_WC: u64 = 1 << 2;

pub(crate) unsafe extern "C" fn ioremap(offset: u64, size: usize) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("ioremap", 0, offset, size as u64);
    map_mmio(offset, size, false)
}

pub(crate) unsafe extern "C" fn ioremap_uc(offset: u64, size: usize) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("ioremap_uc", 0, offset, size as u64);
    map_mmio(offset, size, false)
}

pub(crate) unsafe extern "C" fn ioremap_nocache(offset: u64, size: usize) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("ioremap_nocache", 0, offset, size as u64);
    map_mmio(offset, size, false)
}

pub(crate) unsafe extern "C" fn ioremap_wc(offset: u64, size: usize) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("ioremap_wc", 0, offset, size as u64);
    map_mmio(offset, size, true)
}

pub(crate) unsafe extern "C" fn iounmap(addr: *mut c_void) {
    if addr.is_null() {
        return;
    }
    crate::driver::mmio::unmap(addr);
}

pub(crate) unsafe extern "C" fn devm_ioremap(
    dev: *mut c_void,
    offset: u64,
    size: u64,
) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("devm_ioremap", dev as usize, offset, size);
    let Some(size) = usize::try_from(size).ok() else {
        return core::ptr::null_mut();
    };
    let addr = map_mmio(offset, size, false);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_ioremap_uc(
    dev: *mut c_void,
    offset: u64,
    size: u64,
) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("devm_ioremap_uc", dev as usize, offset, size);
    let Some(size) = usize::try_from(size).ok() else {
        return core::ptr::null_mut();
    };
    let addr = map_mmio(offset, size, false);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_ioremap_wc(
    dev: *mut c_void,
    offset: u64,
    size: u64,
) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("devm_ioremap_wc", dev as usize, offset, size);
    let Some(size) = usize::try_from(size).ok() else {
        return core::ptr::null_mut();
    };
    let addr = map_mmio(offset, size, true);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_iounmap(_dev: *mut c_void, addr: *mut c_void) {
    crate::driver::devres::forget_mmio(_dev, addr);
    crate::driver::mmio::unmap(addr);
}

pub(crate) unsafe extern "C" fn memremap(offset: u64, size: usize, flags: u64) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol("memremap", 0, offset, size as u64);
    map_mmio(offset, size, (flags & MEMREMAP_WC) != 0)
}

pub(crate) unsafe extern "C" fn devm_memremap(
    dev: *mut c_void,
    offset: u64,
    size: usize,
    flags: u64,
) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol(
        "devm_memremap",
        dev as usize,
        offset,
        size as u64,
    );
    let addr = map_mmio(offset, size, (flags & MEMREMAP_WC) != 0);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_memunmap(_dev: *mut c_void, addr: *mut c_void) {
    crate::driver::devres::forget_mmio(_dev, addr);
    crate::driver::mmio::unmap(addr);
}

pub(crate) unsafe extern "C" fn devm_ioremap_resource(
    dev: *mut c_void,
    res: *const LinuxCompatResource,
) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol(
        "devm_ioremap_resource",
        dev as usize,
        res as u64,
        0,
    );
    let addr = map_resource(res, false);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_ioremap_resource_wc(
    dev: *mut c_void,
    res: *const LinuxCompatResource,
) -> *mut c_void {
    crate::driver::symbol_events::record_mmio_symbol(
        "devm_ioremap_resource_wc",
        dev as usize,
        res as u64,
        0,
    );
    let addr = map_resource(res, true);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "ioremap" => Some(ioremap as *const () as usize),
        "ioremap_uc" => Some(ioremap_uc as *const () as usize),
        "ioremap_nocache" => Some(ioremap_nocache as *const () as usize),
        "ioremap_wc" => Some(ioremap_wc as *const () as usize),
        "iounmap" => Some(iounmap as *const () as usize),
        "devm_ioremap" => Some(devm_ioremap as *const () as usize),
        "devm_ioremap_uc" => Some(devm_ioremap_uc as *const () as usize),
        "devm_ioremap_wc" => Some(devm_ioremap_wc as *const () as usize),
        "devm_iounmap" => Some(devm_iounmap as *const () as usize),
        "memremap" => Some(memremap as *const () as usize),
        "devm_memremap" => Some(devm_memremap as *const () as usize),
        "devm_memunmap" => Some(devm_memunmap as *const () as usize),
        "devm_ioremap_resource" => Some(devm_ioremap_resource as *const () as usize),
        "devm_ioremap_resource_wc" => Some(devm_ioremap_resource_wc as *const () as usize),
        _ => None,
    }
}

fn map_resource(res: *const LinuxCompatResource, write_combine: bool) -> *mut c_void {
    if res.is_null() {
        return core::ptr::null_mut();
    }

    let resource = unsafe { &*res };
    if resource.end < resource.start {
        return core::ptr::null_mut();
    }

    let Some(size) = resource
        .end
        .checked_sub(resource.start)
        .and_then(|length| length.checked_add(1))
        .and_then(|length| usize::try_from(length).ok())
    else {
        return core::ptr::null_mut();
    };
    map_mmio(resource.start, size, write_combine)
}

fn map_mmio(offset: u64, size: usize, write_combine: bool) -> *mut c_void {
    if size == 0 {
        return core::ptr::null_mut();
    }
    crate::driver::mmio::map(offset, size, write_combine)
}
// RING3-MIGRATION-REFERENCE END: Linux .ko MMIO mapping shim compatibility substrate exception.
