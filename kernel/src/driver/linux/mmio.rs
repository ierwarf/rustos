use core::ffi::c_void;

use super::compat::LinuxCompatResource;

const MEMREMAP_WC: u64 = 1 << 2;

pub(crate) unsafe extern "C" fn ioremap(offset: u64, size: usize) -> *mut c_void {
    map_mmio(offset, size, false)
}

pub(crate) unsafe extern "C" fn ioremap_uc(offset: u64, size: usize) -> *mut c_void {
    map_mmio(offset, size, false)
}

pub(crate) unsafe extern "C" fn ioremap_nocache(offset: u64, size: usize) -> *mut c_void {
    map_mmio(offset, size, false)
}

pub(crate) unsafe extern "C" fn ioremap_wc(offset: u64, size: usize) -> *mut c_void {
    map_mmio(offset, size, true)
}

pub(crate) unsafe extern "C" fn iounmap(addr: *mut c_void) {
    crate::driver::mmio::unmap(addr);
}

pub(crate) unsafe extern "C" fn devm_ioremap(
    dev: *mut c_void,
    offset: u64,
    size: u64,
) -> *mut c_void {
    let addr = map_mmio(offset, size as usize, false);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_ioremap_uc(
    dev: *mut c_void,
    offset: u64,
    size: u64,
) -> *mut c_void {
    let addr = map_mmio(offset, size as usize, false);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_ioremap_wc(
    dev: *mut c_void,
    offset: u64,
    size: u64,
) -> *mut c_void {
    let addr = map_mmio(offset, size as usize, true);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_iounmap(_dev: *mut c_void, addr: *mut c_void) {
    crate::driver::devres::forget_mmio(_dev, addr);
    crate::driver::mmio::unmap(addr);
}

pub(crate) unsafe extern "C" fn memremap(offset: u64, size: usize, flags: u64) -> *mut c_void {
    map_mmio(offset, size, (flags & MEMREMAP_WC) != 0)
}

pub(crate) unsafe extern "C" fn devm_memremap(
    dev: *mut c_void,
    offset: u64,
    size: usize,
    flags: u64,
) -> *mut c_void {
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
    let addr = map_resource(res, false);
    crate::driver::devres::register_mmio(dev, addr);
    addr
}

pub(crate) unsafe extern "C" fn devm_ioremap_resource_wc(
    dev: *mut c_void,
    res: *const LinuxCompatResource,
) -> *mut c_void {
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
    let size = resource
        .end
        .saturating_sub(resource.start)
        .saturating_add(1);
    map_mmio(resource.start, size as usize, write_combine)
}

fn map_mmio(offset: u64, size: usize, write_combine: bool) -> *mut c_void {
    crate::driver::mmio::map(offset, size, write_combine)
}
