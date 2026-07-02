// RING3-MIGRATION-REFERENCE START: Linux .ko base helper shims are explicit
// ring0 compatibility substrate. Policy belongs in driverd/syscalld, but these
// ABI helpers stay in kernel for in-kernel Linux module execution.
use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::{ptr, slice};

use crate::sync::KernelSpinLock as Mutex;
use x86_64::instructions::interrupts;

#[repr(C)]
struct LinuxKernelParamOps {
    flags: u32,
    set: usize,
    get: usize,
    free: usize,
}

#[repr(C)]
struct CompatAllocHeader {
    magic: u64,
    base: *mut u8,
    total_size: usize,
    layout_align: usize,
    requested_size: usize,
}

static KMALLOC_CACHES: [usize; 1] = [0];
static RANDOM_KMALLOC_SEED: u64 = 0;
static PARAM_OPS_BOOL: LinuxKernelParamOps = LinuxKernelParamOps {
    flags: 0,
    set: 0,
    get: 0,
    free: 0,
};
static PARAM_OPS_INT: LinuxKernelParamOps = LinuxKernelParamOps {
    flags: 0,
    set: 0,
    get: 0,
    free: 0,
};
static PARAM_OPS_UINT: LinuxKernelParamOps = LinuxKernelParamOps {
    flags: 0,
    set: 0,
    get: 0,
    free: 0,
};
static PARAM_OPS_CHARP: LinuxKernelParamOps = LinuxKernelParamOps {
    flags: 0,
    set: 0,
    get: 0,
    free: 0,
};
static PARAM_ARRAY_OPS: LinuxKernelParamOps = LinuxKernelParamOps {
    flags: 0,
    set: 0,
    get: 0,
    free: 0,
};
static COMPAT_ALLOCATIONS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

const ZERO_SIZE_PTR: usize = 16;
const __GFP_ZERO: u32 = 1 << 8;
const COMPAT_ALLOC_MAGIC: u64 = 0x7275_7374_6f73_6b6d;
const MAX_COMPAT_CSTR_BYTES: usize = 4096;

pub(crate) unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    c_strlen(s)
}

pub(crate) unsafe extern "C" fn strcmp(lhs: *const c_char, rhs: *const c_char) -> i32 {
    compare_cstr(lhs, rhs, None, false)
}

pub(crate) unsafe extern "C" fn strncmp(
    lhs: *const c_char,
    rhs: *const c_char,
    limit: usize,
) -> i32 {
    compare_cstr(lhs, rhs, Some(limit), false)
}

pub(crate) unsafe extern "C" fn strcasecmp(lhs: *const c_char, rhs: *const c_char) -> i32 {
    compare_cstr(lhs, rhs, None, true)
}

pub(crate) unsafe extern "C" fn strsep(
    stringp: *mut *mut c_char,
    delim: *const c_char,
) -> *mut c_char {
    if stringp.is_null() {
        return ptr::null_mut();
    }
    let current = unsafe { *stringp };
    if current.is_null() {
        return ptr::null_mut();
    }

    let delimiters = delimiter_bytes(delim);
    let mut cursor = current;
    let mut scanned = 0usize;
    while scanned < MAX_COMPAT_CSTR_BYTES {
        let byte = unsafe { *cursor };
        if byte == 0 {
            unsafe {
                *stringp = ptr::null_mut();
            }
            return current;
        }
        if delimiters.contains(&(byte as u8)) {
            unsafe {
                *cursor = 0;
                *stringp = cursor.add(1);
            }
            return current;
        }
        cursor = unsafe { cursor.add(1) };
        scanned += 1;
    }

    unsafe {
        *stringp = ptr::null_mut();
    }
    current
}

pub(crate) unsafe extern "C" fn simple_strtoul(
    s: *const c_char,
    endp: *mut *mut c_char,
    base: u32,
) -> usize {
    match parse_unsigned_cstr(s, base) {
        Some((value, consumed)) => {
            if !endp.is_null() {
                unsafe {
                    *endp = if s.is_null() {
                        ptr::null_mut()
                    } else {
                        s.add(consumed) as *mut c_char
                    };
                }
            }
            value as usize
        }
        None => {
            if !endp.is_null() {
                unsafe {
                    *endp = s as *mut c_char;
                }
            }
            0
        }
    }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko base helper compatibility substrate exception.

pub(crate) unsafe extern "C" fn kfree(ptr: *const c_void) {
    if zero_or_null_ptr(ptr) || !take_compat_allocation(ptr) {
        return;
    }

    let user_ptr = ptr as *mut u8;
    let header = unsafe {
        user_ptr.sub(core::mem::size_of::<CompatAllocHeader>()) as *mut CompatAllocHeader
    };
    if unsafe { (*header).magic } != COMPAT_ALLOC_MAGIC {
        return;
    }
    let layout = match Layout::from_size_align(unsafe { (*header).total_size }, unsafe {
        (*header).layout_align
    }) {
        Ok(layout) => layout,
        Err(_) => return,
    };
    let base = unsafe { (*header).base };
    unsafe {
        (*header).magic = 0;
        dealloc(base, layout);
    }
}

pub(crate) unsafe extern "C" fn alloc_pages_noprof(gfp: u32, order: u32) -> *mut c_void {
    let size = 4096usize.checked_shl(order.min(20)).unwrap_or(0);
    if size == 0 {
        return ptr::null_mut();
    }
    allocate_bytes_with_flags(size, 4096, gfp) as *mut c_void
}

pub(crate) unsafe extern "C" fn __free_pages(page: *mut c_void, _order: u32) {
    unsafe { kfree(page) };
}

pub(crate) unsafe extern "C" fn __folio_put(_folio: *mut c_void) {}

pub(crate) unsafe extern "C" fn __kmalloc_cache_noprof(
    _cache: *const c_void,
    gfp: u32,
    size: usize,
) -> *mut c_void {
    allocate_bytes_with_flags(size, core::mem::align_of::<usize>(), gfp) as *mut c_void
}

pub(crate) unsafe extern "C" fn __kmalloc_noprof(size: usize, gfp: u32) -> *mut c_void {
    allocate_bytes_with_flags(size, core::mem::align_of::<usize>(), gfp) as *mut c_void
}

pub(crate) unsafe extern "C" fn __kmalloc_large_noprof(size: usize, gfp: u32) -> *mut c_void {
    unsafe { __kmalloc_noprof(size, gfp) }
}

pub(crate) unsafe extern "C" fn __kvmalloc_node_noprof(
    size: usize,
    gfp: u32,
    _node: i32,
) -> *mut c_void {
    unsafe { __kmalloc_noprof(size, gfp) }
}

pub(crate) unsafe extern "C" fn kmemdup_noprof(
    src: *const c_void,
    len: usize,
    _gfp: u32,
) -> *mut c_void {
    if src.is_null() && len != 0 {
        return ptr::null_mut();
    }
    let dest = allocate_bytes(len, 1);
    if dest.is_null() {
        return ptr::null_mut();
    }
    if !src.is_null() && len != 0 {
        unsafe {
            ptr::copy_nonoverlapping(src.cast::<u8>(), dest, len);
        }
    }
    dest.cast()
}

pub(crate) unsafe extern "C" fn kstrndup(
    s: *const c_char,
    max_len: usize,
    _gfp: u32,
) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    let len = c_strlen_bounded(s, max_len);
    let Some(alloc_len) = len.checked_add(1) else {
        return ptr::null_mut();
    };
    let dest = allocate_bytes(alloc_len, 1);
    if dest.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(s.cast::<u8>(), dest, len);
        *dest.add(len) = 0;
    }
    dest.cast()
}

pub(crate) unsafe extern "C" fn kstrtou8(s: *const c_char, base: u32, out: *mut u8) -> i32 {
    parse_integer_out::<u8>(s, base, out)
}

pub(crate) unsafe extern "C" fn kstrtouint(s: *const c_char, base: u32, out: *mut u32) -> i32 {
    parse_integer_out::<u32>(s, base, out)
}

pub(crate) unsafe extern "C" fn kstrtobool(s: *const c_char, out: *mut bool) -> i32 {
    if out.is_null() {
        return -22;
    }
    let Some(slice) = trimmed_cstr_bytes(s) else {
        return -22;
    };
    let value = if eq_ascii_case(slice, b"1")
        || eq_ascii_case(slice, b"y")
        || eq_ascii_case(slice, b"yes")
        || eq_ascii_case(slice, b"true")
        || eq_ascii_case(slice, b"on")
    {
        true
    } else if eq_ascii_case(slice, b"0")
        || eq_ascii_case(slice, b"n")
        || eq_ascii_case(slice, b"no")
        || eq_ascii_case(slice, b"false")
        || eq_ascii_case(slice, b"off")
    {
        false
    } else {
        return -22;
    };
    unsafe {
        *out = value;
    }
    0
}

pub(crate) unsafe extern "C" fn krealloc_noprof(
    ptr: *const c_void,
    new_size: usize,
    gfp: u32,
) -> *mut c_void {
    if zero_or_null_ptr(ptr) {
        return allocate_bytes_with_flags(new_size, core::mem::align_of::<usize>(), gfp).cast();
    }

    let Some(old_size) = allocation_requested_size(ptr) else {
        return ptr::null_mut();
    };
    let user_ptr = ptr as *mut u8;
    let header = unsafe {
        user_ptr.sub(core::mem::size_of::<CompatAllocHeader>()) as *mut CompatAllocHeader
    };
    let layout = match Layout::from_size_align(unsafe { (*header).total_size }, unsafe {
        (*header).layout_align
    }) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };
    let Some(new_total) = core::mem::size_of::<CompatAllocHeader>().checked_add(new_size.max(1))
    else {
        return ptr::null_mut();
    };
    let base = unsafe { realloc((*header).base, layout, new_total) };
    if base.is_null() {
        return ptr::null_mut();
    }
    let new_header = base as *mut CompatAllocHeader;
    unsafe {
        (*new_header).magic = COMPAT_ALLOC_MAGIC;
        (*new_header).base = base;
        (*new_header).total_size = new_total;
        (*new_header).requested_size = new_size;
    }
    let new_user = unsafe { base.add(core::mem::size_of::<CompatAllocHeader>()) };
    let copy_len = old_size.min(new_size);
    if copy_len != 0 && new_user != user_ptr {
        unsafe {
            ptr::copy(user_ptr, new_user, copy_len);
        }
    }
    replace_compat_allocation(ptr, new_user.cast());
    new_user.cast()
}

pub(crate) unsafe extern "C" fn kvfree(ptr: *const c_void) {
    unsafe { kfree(ptr) };
}

pub(crate) unsafe extern "C" fn vzalloc_noprof(size: usize) -> *mut c_void {
    allocate_zeroed_bytes(size, core::mem::align_of::<usize>()).cast()
}

pub(crate) unsafe extern "C" fn vfree(ptr: *const c_void) {
    unsafe { kfree(ptr) };
}

pub(crate) unsafe extern "C" fn memcpy(
    dest: *mut c_void,
    src: *const c_void,
    len: usize,
) -> *mut c_void {
    if !dest.is_null() && !src.is_null() && len != 0 {
        unsafe {
            ptr::copy_nonoverlapping(src.cast::<u8>(), dest.cast::<u8>(), len);
        }
    }
    dest
}

pub(crate) unsafe extern "C" fn memset(dest: *mut c_void, value: i32, len: usize) -> *mut c_void {
    if !dest.is_null() && len != 0 {
        unsafe {
            ptr::write_bytes(dest.cast::<u8>(), value as u8, len);
        }
    }
    dest
}

pub(crate) unsafe extern "C" fn memcmp(lhs: *const c_void, rhs: *const c_void, len: usize) -> i32 {
    if lhs.is_null() || rhs.is_null() || len == 0 {
        return 0;
    }
    let lhs = unsafe { slice::from_raw_parts(lhs.cast::<u8>(), len) };
    let rhs = unsafe { slice::from_raw_parts(rhs.cast::<u8>(), len) };
    for (left, right) in lhs.iter().zip(rhs.iter()) {
        if left != right {
            return (*left as i32) - (*right as i32);
        }
    }
    0
}

pub(crate) unsafe extern "C" fn strnlen(s: *const c_char, max_len: usize) -> usize {
    c_strlen_bounded(s, max_len)
}

pub(crate) unsafe extern "C" fn strrchr(s: *const c_char, ch: i32) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    let target = ch as u8;
    let mut last = ptr::null_mut();
    let mut cursor = s;
    let mut scanned = 0usize;
    while scanned < MAX_COMPAT_CSTR_BYTES {
        let byte = unsafe { *cursor as u8 };
        if byte == target {
            last = cursor as *mut c_char;
        }
        if byte == 0 {
            break;
        }
        cursor = unsafe { cursor.add(1) };
        scanned += 1;
    }
    last
}

pub(crate) unsafe extern "C" fn sscanf(_buffer: *const c_char, _format: *const c_char) -> i32 {
    0
}

pub(crate) unsafe extern "C" fn memdup_user(src: *const c_void, len: usize) -> *mut c_void {
    unsafe { kmemdup_noprof(src, len, 0) }
}

pub(crate) unsafe extern "C" fn vmemdup_user(src: *const c_void, len: usize) -> *mut c_void {
    unsafe { kmemdup_noprof(src, len, 0) }
}

pub(crate) unsafe extern "C" fn strncpy_from_user(
    dest: *mut c_char,
    src: *const c_char,
    count: usize,
) -> isize {
    if dest.is_null() || src.is_null() {
        return -14;
    }
    let mut copied = 0usize;
    while copied < count {
        let byte = unsafe { *src.add(copied) };
        unsafe {
            *dest.add(copied) = byte;
        }
        if byte == 0 {
            return copied as isize;
        }
        copied += 1;
    }
    count as isize
}

pub(crate) unsafe extern "C" fn __get_user_4(out: *mut u32, src: *const u32) -> i32 {
    if out.is_null() || src.is_null() {
        return -14;
    }
    unsafe {
        *out = src.read_unaligned();
    }
    0
}

pub(crate) unsafe extern "C" fn __put_user_4(value: u32, dst: *mut u32) -> i32 {
    if dst.is_null() {
        return -14;
    }
    unsafe {
        dst.write_unaligned(value);
    }
    0
}

pub(crate) unsafe extern "C" fn __copy_overflow(_size: usize, _count: usize) -> ! {
    panic!("linux compat module triggered __copy_overflow");
}

pub(crate) unsafe extern "C" fn sized_strscpy(
    dest: *mut c_char,
    src: *const c_char,
    count: usize,
) -> isize {
    if dest.is_null() || count == 0 {
        return -22;
    }
    let Some(bytes) = trimmed_cstr_bytes(src) else {
        unsafe {
            *dest = 0;
        }
        return -22;
    };
    if bytes.len() >= count {
        let copy_len = count.saturating_sub(1);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), dest.cast::<u8>(), copy_len);
            *dest.add(copy_len) = 0;
        }
        return -7;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), dest.cast::<u8>(), bytes.len());
        *dest.add(bytes.len()) = 0;
    }
    bytes.len() as isize
}

pub(crate) unsafe extern "C" fn _find_next_zero_bit(
    addr: *const usize,
    size: usize,
    offset: usize,
) -> usize {
    if addr.is_null() || offset >= size {
        return size;
    }
    let bits_per_word = usize::BITS as usize;
    let words = size.div_ceil(bits_per_word);
    let slice = unsafe { slice::from_raw_parts(addr, words) };
    let mut bit = offset;
    while bit < size {
        let word = slice[bit / bits_per_word];
        if (word & (1usize << (bit % bits_per_word))) == 0 {
            return bit;
        }
        bit += 1;
    }
    size
}

pub(crate) unsafe extern "C" fn _find_first_bit(addr: *const usize, size: usize) -> usize {
    unsafe { _find_next_bit(addr, size, 0) }
}

pub(crate) unsafe extern "C" fn _find_next_bit(
    addr: *const usize,
    size: usize,
    offset: usize,
) -> usize {
    if addr.is_null() || offset >= size {
        return size;
    }
    let word_bits = usize::BITS as usize;
    let word_count = size.div_ceil(word_bits);
    let words = unsafe { slice::from_raw_parts(addr, word_count) };
    let mut bit = offset;
    while bit < size {
        let word = words[bit / word_bits];
        if ((word >> (bit % word_bits)) & 1) != 0 {
            return bit;
        }
        bit += 1;
    }
    size
}

pub(crate) unsafe extern "C" fn devm_kmalloc(
    _dev: *mut c_void,
    size: usize,
    gfp: u32,
) -> *mut c_void {
    unsafe { __kmalloc_noprof(size, gfp) }
}

pub(crate) unsafe extern "C" fn devm_kfree(_dev: *mut c_void, ptr: *const c_void) {
    unsafe { kfree(ptr) };
}

pub(crate) unsafe extern "C" fn msleep(milliseconds: u32) {
    const SERVICE_QUANTUM_MS: u64 = 10;
    let mut remaining = milliseconds as u64;
    while remaining != 0 {
        let slice = remaining.min(SERVICE_QUANTUM_MS);
        if crate::arch::rtc::is_initialized() {
            crate::arch::rtc::sleep(slice);
        } else {
            for _ in 0..(slice * 10_000) {
                core::hint::spin_loop();
            }
        }
        crate::driver::linux::runtime::service_compat_pending();
        remaining -= slice;
    }
}

pub(crate) unsafe extern "C" fn virt_to_phys(addr: *const c_void) -> u64 {
    let addr = addr as u64;
    if addr < crate::memory::kernel_vm::KERNEL_VIRT_OFFSET {
        return 0;
    }
    crate::memory::paging::kernel_virtual_to_physical_addr(addr)
}

pub(crate) unsafe extern "C" fn phys_to_virt(addr: u64) -> *mut c_void {
    if addr >= crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT {
        return ptr::null_mut();
    }
    crate::memory::paging::higher_half_addr(addr) as *mut c_void
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "strlen" => Some(strlen as *const () as usize),
        "strcmp" => Some(strcmp as *const () as usize),
        "strncmp" => Some(strncmp as *const () as usize),
        "strcasecmp" => Some(strcasecmp as *const () as usize),
        "strsep" => Some(strsep as *const () as usize),
        "simple_strtoul" => Some(simple_strtoul as *const () as usize),
        "kfree" => Some(kfree as *const () as usize),
        "alloc_pages_noprof" => Some(alloc_pages_noprof as *const () as usize),
        "__free_pages" => Some(__free_pages as *const () as usize),
        "__folio_put" => Some(__folio_put as *const () as usize),
        "__kmalloc_cache_noprof" => Some(__kmalloc_cache_noprof as *const () as usize),
        "__kmalloc_noprof" => Some(__kmalloc_noprof as *const () as usize),
        "__kmalloc_large_noprof" => Some(__kmalloc_large_noprof as *const () as usize),
        "__kvmalloc_node_noprof" => Some(__kvmalloc_node_noprof as *const () as usize),
        "kmalloc_caches" => Some(&KMALLOC_CACHES as *const [usize; 1] as usize),
        "random_kmalloc_seed" => Some(&RANDOM_KMALLOC_SEED as *const u64 as usize),
        "kmemdup_noprof" => Some(kmemdup_noprof as *const () as usize),
        "krealloc_noprof" => Some(krealloc_noprof as *const () as usize),
        "kvfree" => Some(kvfree as *const () as usize),
        "vzalloc_noprof" => Some(vzalloc_noprof as *const () as usize),
        "vfree" => Some(vfree as *const () as usize),
        "memcpy" => Some(memcpy as *const () as usize),
        "memset" => Some(memset as *const () as usize),
        "memcmp" => Some(memcmp as *const () as usize),
        "strnlen" => Some(strnlen as *const () as usize),
        "strrchr" => Some(strrchr as *const () as usize),
        "sscanf" => Some(sscanf as *const () as usize),
        "memdup_user" => Some(memdup_user as *const () as usize),
        "vmemdup_user" => Some(vmemdup_user as *const () as usize),
        "strncpy_from_user" => Some(strncpy_from_user as *const () as usize),
        "__get_user_4" => Some(__get_user_4 as *const () as usize),
        "__put_user_4" => Some(__put_user_4 as *const () as usize),
        "__copy_overflow" => Some(__copy_overflow as *const () as usize),
        "sized_strscpy" => Some(sized_strscpy as *const () as usize),
        "_find_first_bit" => Some(_find_first_bit as *const () as usize),
        "_find_next_bit" => Some(_find_next_bit as *const () as usize),
        "_find_next_zero_bit" => Some(_find_next_zero_bit as *const () as usize),
        "kstrndup" => Some(kstrndup as *const () as usize),
        "kstrtou8" => Some(kstrtou8 as *const () as usize),
        "kstrtouint" => Some(kstrtouint as *const () as usize),
        "kstrtobool" => Some(kstrtobool as *const () as usize),
        "param_ops_bool" => Some(&PARAM_OPS_BOOL as *const LinuxKernelParamOps as usize),
        "param_ops_int" => Some(&PARAM_OPS_INT as *const LinuxKernelParamOps as usize),
        "param_ops_uint" => Some(&PARAM_OPS_UINT as *const LinuxKernelParamOps as usize),
        "param_ops_charp" => Some(&PARAM_OPS_CHARP as *const LinuxKernelParamOps as usize),
        "param_array_ops" => Some(&PARAM_ARRAY_OPS as *const LinuxKernelParamOps as usize),
        "devm_kmalloc" => Some(devm_kmalloc as *const () as usize),
        "devm_kfree" => Some(devm_kfree as *const () as usize),
        "msleep" => Some(msleep as *const () as usize),
        "virt_to_phys" => Some(virt_to_phys as *const () as usize),
        "phys_to_virt" => Some(phys_to_virt as *const () as usize),
        _ => None,
    }
}

fn allocate_bytes(size: usize, align: usize) -> *mut u8 {
    allocate_bytes_inner(size, align, false)
}

fn allocate_zeroed_bytes(size: usize, align: usize) -> *mut u8 {
    allocate_bytes_inner(size, align, true)
}

fn allocate_bytes_with_flags(size: usize, align: usize, gfp: u32) -> *mut u8 {
    allocate_bytes_inner(size, align, (gfp & __GFP_ZERO) != 0)
}

fn allocate_bytes_inner(size: usize, align: usize, zeroed: bool) -> *mut u8 {
    let header_size = core::mem::size_of::<CompatAllocHeader>();
    let requested_align = align.max(1);
    let Some(layout_align) = requested_align
        .max(core::mem::align_of::<CompatAllocHeader>())
        .checked_next_power_of_two()
    else {
        return ptr::null_mut();
    };
    let total_size = header_size
        .checked_add(size.max(1))
        .and_then(|value| value.checked_add(layout_align.saturating_sub(1)))
        .unwrap_or(0);
    if total_size == 0 {
        return ptr::null_mut();
    }

    let Ok(layout) = Layout::from_size_align(total_size, layout_align) else {
        return ptr::null_mut();
    };
    let base = if zeroed {
        unsafe { alloc_zeroed(layout) }
    } else {
        unsafe { alloc(layout) }
    };
    if base.is_null() {
        return ptr::null_mut();
    }

    let Some(user_start) = (base as usize)
        .checked_add(header_size)
        .and_then(|value| align_up(value, requested_align))
    else {
        unsafe {
            dealloc(base, layout);
        }
        return ptr::null_mut();
    };
    let header = (user_start - header_size) as *mut CompatAllocHeader;
    unsafe {
        header.write(CompatAllocHeader {
            magic: COMPAT_ALLOC_MAGIC,
            base,
            total_size,
            layout_align,
            requested_size: size,
        });
    }
    register_compat_allocation(user_start as *const c_void);
    user_start as *mut u8
}

fn allocation_requested_size(ptr: *const c_void) -> Option<usize> {
    if zero_or_null_ptr(ptr) || !compat_allocation_is_registered(ptr) {
        return None;
    }
    let user_ptr = ptr.cast::<u8>();
    let header = unsafe {
        user_ptr.sub(core::mem::size_of::<CompatAllocHeader>()) as *const CompatAllocHeader
    };
    if unsafe { (*header).magic } != COMPAT_ALLOC_MAGIC {
        return None;
    }
    Some(unsafe { (*header).requested_size })
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align <= 1 {
        return Some(value);
    }
    let mask = align - 1;
    Some(value.checked_add(mask)? & !mask)
}

fn zero_or_null_ptr(ptr: *const c_void) -> bool {
    (ptr as usize) <= ZERO_SIZE_PTR
}

fn register_compat_allocation(ptr: *const c_void) {
    if zero_or_null_ptr(ptr) {
        return;
    }
    interrupts::without_interrupts(|| {
        COMPAT_ALLOCATIONS.lock().push(ptr as usize);
    });
}

fn replace_compat_allocation(old_ptr: *const c_void, new_ptr: *const c_void) {
    interrupts::without_interrupts(|| {
        let mut allocations = COMPAT_ALLOCATIONS.lock();
        if let Some(entry) = allocations
            .iter_mut()
            .find(|entry| **entry == old_ptr as usize)
        {
            *entry = new_ptr as usize;
        }
    });
}

fn take_compat_allocation(ptr: *const c_void) -> bool {
    if zero_or_null_ptr(ptr) {
        return false;
    }
    interrupts::without_interrupts(|| {
        let mut allocations = COMPAT_ALLOCATIONS.lock();
        if let Some(index) = allocations.iter().position(|entry| *entry == ptr as usize) {
            allocations.swap_remove(index);
            true
        } else {
            false
        }
    })
}

fn compat_allocation_is_registered(ptr: *const c_void) -> bool {
    if zero_or_null_ptr(ptr) {
        return false;
    }
    interrupts::without_interrupts(|| {
        COMPAT_ALLOCATIONS
            .lock()
            .iter()
            .any(|entry| *entry == ptr as usize)
    })
}

fn c_strlen(s: *const c_char) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut len = 0usize;
    let mut cursor = s;
    while len < MAX_COMPAT_CSTR_BYTES && unsafe { *cursor } != 0 {
        len += 1;
        cursor = unsafe { cursor.add(1) };
    }
    len
}

fn c_strlen_bounded(s: *const c_char, max_len: usize) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut len = 0usize;
    let mut cursor = s;
    let max_len = max_len.min(MAX_COMPAT_CSTR_BYTES);
    while len < max_len && unsafe { *cursor } != 0 {
        len += 1;
        cursor = unsafe { cursor.add(1) };
    }
    len
}

fn compare_cstr(
    lhs: *const c_char,
    rhs: *const c_char,
    limit: Option<usize>,
    fold_ascii: bool,
) -> i32 {
    let mut index = 0usize;
    loop {
        let effective_limit = limit
            .unwrap_or(MAX_COMPAT_CSTR_BYTES)
            .min(MAX_COMPAT_CSTR_BYTES);
        if index >= effective_limit {
            return 0;
        }
        let left = if lhs.is_null() {
            0
        } else {
            unsafe { *lhs.add(index) as u8 }
        };
        let right = if rhs.is_null() {
            0
        } else {
            unsafe { *rhs.add(index) as u8 }
        };
        let left_cmp = if fold_ascii { ascii_lower(left) } else { left };
        let right_cmp = if fold_ascii {
            ascii_lower(right)
        } else {
            right
        };
        if left_cmp != right_cmp {
            return (left_cmp as i32) - (right_cmp as i32);
        }
        if left == 0 || right == 0 {
            return 0;
        }
        index += 1;
    }
}

fn delimiter_bytes(delim: *const c_char) -> Vec<u8> {
    if delim.is_null() {
        return Vec::new();
    }
    unsafe { slice::from_raw_parts(delim as *const u8, c_strlen(delim)).to_vec() }
}

fn trimmed_cstr_bytes<'a>(s: *const c_char) -> Option<&'a [u8]> {
    if s.is_null() {
        return None;
    }
    let bytes = unsafe { slice::from_raw_parts(s as *const u8, c_strlen(s)) };
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    Some(&bytes[start..end])
}

fn eq_ascii_case(lhs: &[u8], rhs: &[u8]) -> bool {
    lhs.len() == rhs.len()
        && lhs
            .iter()
            .zip(rhs.iter())
            .all(|(left, right)| ascii_lower(*left) == ascii_lower(*right))
}

fn parse_integer_out<T>(s: *const c_char, base: u32, out: *mut T) -> i32
where
    T: TryFrom<u64>,
{
    if out.is_null() {
        return -22;
    }
    let Some((value, _)) = parse_unsigned_cstr(s, base) else {
        return -22;
    };
    let Ok(value) = T::try_from(value) else {
        return -34;
    };
    unsafe {
        *out = value;
    }
    0
}

fn parse_unsigned_cstr(s: *const c_char, base: u32) -> Option<(u64, usize)> {
    let bytes = trimmed_cstr_bytes(s)?;
    if bytes.is_empty() {
        return None;
    }

    let mut index = 0usize;
    let mut radix = base;
    if radix == 0 {
        if bytes.len() >= 2 && bytes[0] == b'0' && (bytes[1] == b'x' || bytes[1] == b'X') {
            radix = 16;
            index = 2;
        } else if bytes[0] == b'0' && bytes.len() > 1 {
            radix = 8;
            index = 1;
        } else {
            radix = 10;
        }
    } else if radix == 16
        && bytes.len() >= 2
        && bytes[0] == b'0'
        && (bytes[1] == b'x' || bytes[1] == b'X')
    {
        index = 2;
    }
    if !(2..=36).contains(&radix) {
        return None;
    }

    let mut value = 0u64;
    let mut consumed_digits = 0usize;
    while index < bytes.len() {
        let digit = match ascii_digit_value(bytes[index]) {
            Some(value) if value < radix => value,
            _ => break,
        };
        value = value.checked_mul(radix as u64)?;
        value = value.checked_add(digit as u64)?;
        index += 1;
        consumed_digits += 1;
    }
    (consumed_digits != 0).then_some((value, index))
}

fn ascii_digit_value(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'z' => Some((byte - b'a' + 10) as u32),
        b'A'..=b'Z' => Some((byte - b'A' + 10) as u32),
        _ => None,
    }
}

fn ascii_lower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}
