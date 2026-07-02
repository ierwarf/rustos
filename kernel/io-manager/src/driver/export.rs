// RING3-MIGRATION-REFERENCE START: Linux .ko export resolution is an explicit
// ring0 substrate exception. Policy belongs in driverd, but kernel symbol shims
// stay in ring0 for Linux driver ABI compatibility.
use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, handle_alloc_error, realloc};

use super::module_registry;

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    if name == "_RNvYjNtNtCs6VtbPhscPip_4core3cmp3Ord3minB7_"
        || rust_symbol_has_suffix(name, "4core3cmp3Ord3min")
    {
        return Some(rust_module_ord_min_usize as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "4core9panicking18panic_bounds_check") {
        return Some(rust_module_panic_bounds_check as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "4core3str8converts9from_utf8") {
        return Some(rust_module_from_utf8 as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "4core5slice5index16slice_index_fail") {
        return Some(rust_module_slice_index_fail as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "___rustc12___rust_alloc") {
        return Some(rust_module_alloc as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "___rustc14___rust_dealloc") {
        return Some(rust_module_dealloc as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "___rustc14___rust_realloc") {
        return Some(rust_module_realloc as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "___rustc19___rust_alloc_zeroed") {
        return Some(rust_module_alloc_zeroed as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "___rustc26___rust_alloc_error_handler") {
        return Some(rust_module_alloc_error_handler as *const () as usize);
    }
    if rust_symbol_has_suffix(name, "___rustc35___rust_no_alloc_shim_is_unstable_v2") {
        return Some(rust_module_no_alloc_shim_is_unstable_v2 as *const () as usize);
    }

    module_registry::resolve_symbol(name)
}

fn rust_symbol_has_suffix(name: &str, suffix: &str) -> bool {
    name.ends_with(suffix)
}

fn rust_module_layout(size: usize, align: usize) -> Option<Layout> {
    Layout::from_size_align(size, align).ok()
}

unsafe fn rust_module_alloc(size: usize, align: usize) -> *mut u8 {
    rust_module_layout(size, align)
        .map(|layout| unsafe { alloc(layout) })
        .unwrap_or(core::ptr::null_mut())
}

unsafe fn rust_module_dealloc(ptr: *mut u8, size: usize, align: usize) {
    if let Some(layout) = rust_module_layout(size, align) {
        unsafe { dealloc(ptr, layout) };
    }
}

unsafe fn rust_module_realloc(
    ptr: *mut u8,
    old_size: usize,
    align: usize,
    new_size: usize,
) -> *mut u8 {
    rust_module_layout(old_size, align)
        .map(|layout| unsafe { realloc(ptr, layout, new_size) })
        .unwrap_or(core::ptr::null_mut())
}

unsafe fn rust_module_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    rust_module_layout(size, align)
        .map(|layout| unsafe { alloc_zeroed(layout) })
        .unwrap_or(core::ptr::null_mut())
}

fn rust_module_alloc_error_handler(size: usize, align: usize) -> ! {
    let Some(layout) = rust_module_layout(size, align) else {
        panic!("module allocation failed with invalid layout: size={size} align={align}");
    };
    handle_alloc_error(layout)
}

fn rust_module_no_alloc_shim_is_unstable_v2() {}

fn rust_module_ord_min_usize(left: usize, right: usize) -> usize {
    core::cmp::min(left, right)
}

fn rust_module_panic_bounds_check(index: usize, len: usize) -> ! {
    panic!("module index out of bounds: len={len} index={index}");
}

fn rust_module_from_utf8(bytes: &[u8]) -> Result<&str, core::str::Utf8Error> {
    core::str::from_utf8(bytes)
}

fn rust_module_slice_index_fail(start: usize, end: usize, len: usize) -> ! {
    panic!("module slice index out of bounds: start={start} end={end} len={len}");
}
// RING3-MIGRATION-REFERENCE END: Linux .ko export compatibility substrate exception.
