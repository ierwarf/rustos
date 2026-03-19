use alloc::alloc::{Layout, alloc, dealloc};
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::ptr;
use core::slice;

#[repr(C)]
struct LinuxKernelParamOps {
    flags: u32,
    set: usize,
    get: usize,
    free: usize,
}

#[repr(C)]
struct CompatAllocHeader {
    base: *mut u8,
    total_size: usize,
    layout_align: usize,
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
    loop {
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
    }
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

pub(crate) unsafe extern "C" fn kfree(ptr: *const c_void) {
    if ptr.is_null() {
        return;
    }

    let user_ptr = ptr as *mut u8;
    let header = unsafe {
        user_ptr.sub(core::mem::size_of::<CompatAllocHeader>()) as *mut CompatAllocHeader
    };
    let layout = match Layout::from_size_align(unsafe { (*header).total_size }, unsafe {
        (*header).layout_align
    }) {
        Ok(layout) => layout,
        Err(_) => return,
    };
    let base = unsafe { (*header).base };
    unsafe {
        dealloc(base, layout);
    }
}

pub(crate) unsafe extern "C" fn __kmalloc_cache_noprof(
    _cache: *const c_void,
    size: usize,
    _gfp: u32,
) -> *mut c_void {
    allocate_bytes(size, core::mem::align_of::<usize>()) as *mut c_void
}

pub(crate) unsafe extern "C" fn kmemdup_noprof(
    src: *const c_void,
    len: usize,
    _gfp: u32,
) -> *mut c_void {
    let dest = allocate_bytes(len, 1);
    if dest.is_null() {
        return ptr::null_mut();
    }
    if !src.is_null() && len != 0 {
        unsafe {
            ptr::copy_nonoverlapping(src as *const u8, dest, len);
        }
    }
    dest as *mut c_void
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
    let dest = allocate_bytes(len + 1, 1);
    if dest.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(s as *const u8, dest, len);
        *dest.add(len) = 0;
    }
    dest as *mut c_char
}

pub(crate) unsafe extern "C" fn kstrtou8(s: *const c_char, base: u32, out: *mut u8) -> i32 {
    parse_integer_out::<u8>(s, base, out)
}

pub(crate) unsafe extern "C" fn kstrtouint(s: *const c_char, base: u32, out: *mut u32) -> i32 {
    parse_integer_out::<u32>(s, base, out)
}

pub(crate) unsafe extern "C" fn kstrtobool(s: *const c_char, out: *mut bool) -> i32 {
    if s.is_null() || out.is_null() {
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

pub(crate) unsafe extern "C" fn msleep(milliseconds: u32) {
    crate::rtc::sleep(milliseconds as u64);
}

pub(crate) unsafe extern "C" fn virt_to_phys(addr: *const c_void) -> u64 {
    if addr.is_null() {
        0
    } else {
        crate::paging::kernel_virtual_to_physical_addr(addr as u64)
    }
}

pub(crate) unsafe extern "C" fn phys_to_virt(addr: u64) -> *mut c_void {
    crate::paging::higher_half_addr(addr) as *mut c_void
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
        "__kmalloc_cache_noprof" => Some(__kmalloc_cache_noprof as *const () as usize),
        "kmalloc_caches" => Some(&KMALLOC_CACHES as *const [usize; 1] as usize),
        "random_kmalloc_seed" => Some(&RANDOM_KMALLOC_SEED as *const u64 as usize),
        "kmemdup_noprof" => Some(kmemdup_noprof as *const () as usize),
        "kstrndup" => Some(kstrndup as *const () as usize),
        "kstrtou8" => Some(kstrtou8 as *const () as usize),
        "kstrtouint" => Some(kstrtouint as *const () as usize),
        "kstrtobool" => Some(kstrtobool as *const () as usize),
        "param_ops_bool" => Some(&PARAM_OPS_BOOL as *const LinuxKernelParamOps as usize),
        "param_ops_int" => Some(&PARAM_OPS_INT as *const LinuxKernelParamOps as usize),
        "param_ops_uint" => Some(&PARAM_OPS_UINT as *const LinuxKernelParamOps as usize),
        "msleep" => Some(msleep as *const () as usize),
        "virt_to_phys" => Some(virt_to_phys as *const () as usize),
        "phys_to_virt" => Some(phys_to_virt as *const () as usize),
        _ => None,
    }
}

fn allocate_bytes(size: usize, align: usize) -> *mut u8 {
    let header_size = core::mem::size_of::<CompatAllocHeader>();
    let requested_align = align.max(1);
    let layout_align = requested_align
        .max(core::mem::align_of::<CompatAllocHeader>())
        .next_power_of_two();
    let total_size = header_size
        .checked_add(size.max(1))
        .and_then(|value| value.checked_add(layout_align.saturating_sub(1)))
        .unwrap_or(0);
    if total_size == 0 {
        return ptr::null_mut();
    }

    let layout = match Layout::from_size_align(total_size, layout_align) {
        Ok(layout) => layout,
        Err(_) => return ptr::null_mut(),
    };
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        return ptr::null_mut();
    }

    let user_start = align_up(base as usize + header_size, requested_align);
    let header = (user_start - header_size) as *mut CompatAllocHeader;
    unsafe {
        header.write(CompatAllocHeader {
            base,
            total_size,
            layout_align,
        });
    }
    user_start as *mut u8
}

fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        return value;
    }
    let mask = align - 1;
    (value + mask) & !mask
}

fn c_strlen(s: *const c_char) -> usize {
    if s.is_null() {
        return 0;
    }

    let mut len = 0usize;
    let mut cursor = s;
    while unsafe { *cursor } != 0 {
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
        if limit.is_some_and(|value| index >= value) {
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

fn ascii_lower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
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

    if consumed_digits == 0 {
        return None;
    }
    Some((value, index))
}

fn ascii_digit_value(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'z' => Some((byte - b'a' + 10) as u32),
        b'A'..=b'Z' => Some((byte - b'A' + 10) as u32),
        _ => None,
    }
}
