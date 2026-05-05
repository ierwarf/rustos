use core::arch::asm;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{__cpuid, __cpuid_count};

const CR0_MONITOR_COPROCESSOR: u64 = 1 << 1;
const CR0_EMULATION: u64 = 1 << 2;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_OSXSAVE: u64 = 1 << 18;

const CPUID_FEATURE_XSAVE: u32 = 1 << 26;
const CPUID_FEATURE_AVX: u32 = 1 << 28;
const CPUID_EXT_FEATURE_AVX2: u32 = 1 << 5;

const XFEATURE_X87: u64 = 1 << 0;
const XFEATURE_SSE: u64 = 1 << 1;
const XFEATURE_YMM: u64 = 1 << 2;

const SIMD_MODE_FXSAVE: u8 = 1;
const SIMD_MODE_XSAVE: u8 = 2;

const SIMD_STATE_BYTES: usize = 4096;
const FXSAVE_STATE_BYTES: usize = 512;
// These thresholds remain part of the staged memcpy fast-path even before all call sites switch.
#[allow(dead_code)]
const XMM_COPY_THRESHOLD_BYTES: usize = 256;
#[allow(dead_code)]
const YMM_COPY_THRESHOLD_BYTES: usize = 256;
const BGRA_BLIT_AVX2_THRESHOLD_PIXELS: usize = 16;
const ENABLE_SIMD_BGRA_BLIT: bool = true;

static SIMD_MODE: AtomicU8 = AtomicU8::new(SIMD_MODE_FXSAVE);
static XSTATE_MASK: AtomicU64 = AtomicU64::new(XFEATURE_X87 | XFEATURE_SSE);
static SIMD_STATE_REQUIRED_BYTES: AtomicUsize = AtomicUsize::new(FXSAVE_STATE_BYTES);
static AVX_ENABLED: AtomicBool = AtomicBool::new(false);
static AVX2_ENABLED: AtomicBool = AtomicBool::new(false);

#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct SimdState {
    bytes: [u8; SIMD_STATE_BYTES],
}

impl SimdState {
    pub const fn new() -> Self {
        let mut bytes = [0u8; SIMD_STATE_BYTES];
        // Default x87 control word and MXCSR state for a fresh task.
        bytes[0] = 0x7f;
        bytes[1] = 0x03;
        bytes[24] = 0x80;
        bytes[25] = 0x1f;
        Self { bytes }
    }
}

pub fn init() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        enable_sse_baseline();

        let max_leaf = __cpuid(0).eax;
        let leaf1 = __cpuid(1);
        if (leaf1.ecx & CPUID_FEATURE_XSAVE) == 0 || max_leaf < 0xD {
            SIMD_STATE_REQUIRED_BYTES.store(FXSAVE_STATE_BYTES, Ordering::Release);
            return;
        }

        let supported_xcr0 = __cpuid_count(0xD, 0).eax as u64;
        if (supported_xcr0 & (XFEATURE_X87 | XFEATURE_SSE)) != (XFEATURE_X87 | XFEATURE_SSE) {
            return;
        }

        let mut requested_mask = XFEATURE_X87 | XFEATURE_SSE;
        let mut avx_enabled = false;
        let mut avx2_enabled = false;
        if (leaf1.ecx & CPUID_FEATURE_AVX) != 0 && (supported_xcr0 & XFEATURE_YMM) != 0 {
            requested_mask |= XFEATURE_YMM;
            avx_enabled = true;
            if max_leaf >= 7 {
                let leaf7 = __cpuid_count(7, 0);
                avx2_enabled = (leaf7.ebx & CPUID_EXT_FEATURE_AVX2) != 0;
            }
        }

        write_cr4(read_cr4() | CR4_OSXSAVE);
        xsetbv0(requested_mask);

        XSTATE_MASK.store(requested_mask, Ordering::Release);
        AVX_ENABLED.store(avx_enabled, Ordering::Release);
        AVX2_ENABLED.store(avx2_enabled, Ordering::Release);
        SIMD_MODE.store(SIMD_MODE_XSAVE, Ordering::Release);

        let xsave_leaf = __cpuid_count(0xD, 0);
        let required_bytes = (xsave_leaf.eax.max(xsave_leaf.ebx) as usize).max(FXSAVE_STATE_BYTES);
        if required_bytes > SIMD_STATE_BYTES {
            panic!(
                "SIMD xsave state requires {} bytes but buffer is only {} bytes",
                required_bytes, SIMD_STATE_BYTES,
            );
        }
        SIMD_STATE_REQUIRED_BYTES.store(required_bytes, Ordering::Release);
    }
}

#[allow(dead_code)]
pub fn mode_name() -> &'static str {
    match SIMD_MODE.load(Ordering::Acquire) {
        SIMD_MODE_XSAVE if avx2_enabled() => "xsave-avx2",
        SIMD_MODE_XSAVE if avx_enabled() => "xsave-avx",
        SIMD_MODE_XSAVE => "xsave-sse",
        _ => "fxsave-sse",
    }
}

#[allow(dead_code)]
pub fn state_bytes() -> usize {
    SIMD_STATE_REQUIRED_BYTES.load(Ordering::Acquire)
}

#[allow(dead_code)]
pub fn avx_enabled() -> bool {
    AVX_ENABLED.load(Ordering::Acquire)
}

pub fn avx2_enabled() -> bool {
    AVX2_ENABLED.load(Ordering::Acquire)
}

#[inline]
pub unsafe fn save_state(area: &mut SimdState) {
    match SIMD_MODE.load(Ordering::Acquire) {
        SIMD_MODE_XSAVE => unsafe { xsave(area, XSTATE_MASK.load(Ordering::Acquire)) },
        _ => unsafe { fxsave(area) },
    }
}

#[inline]
pub unsafe fn restore_state(area: &SimdState) {
    match SIMD_MODE.load(Ordering::Acquire) {
        SIMD_MODE_XSAVE => unsafe { xrstor(area, XSTATE_MASK.load(Ordering::Acquire)) },
        _ => unsafe { fxrstor(area) },
    }
}

#[inline]
#[allow(dead_code)]
pub unsafe fn copy_fast(src: *const u8, dst: *mut u8, len: usize) {
    if len == 0 || src == dst {
        return;
    }

    if avx_enabled() && len >= YMM_COPY_THRESHOLD_BYTES {
        unsafe {
            copy_ymm(src, dst, len);
        }
        return;
    }

    if len >= XMM_COPY_THRESHOLD_BYTES {
        unsafe {
            copy_xmm(src, dst, len);
        }
        return;
    }

    unsafe {
        if regions_overlap(src, dst, len) {
            ptr::copy(src, dst, len);
        } else {
            ptr::copy_nonoverlapping(src, dst, len);
        }
    }
}

#[inline]
pub unsafe fn blit_bgra8888_row(
    dst: *mut u8,
    src: *const u8,
    pixels: usize,
    dst_bpp: usize,
    rgb_format: bool,
) {
    if pixels == 0 {
        return;
    }

    #[cfg(target_arch = "x86_64")]
    if ENABLE_SIMD_BGRA_BLIT
        && dst_bpp == 4
        && avx2_enabled()
        && pixels >= BGRA_BLIT_AVX2_THRESHOLD_PIXELS
    {
        unsafe {
            if rgb_format {
                blit_bgra8888_to_rgbx_ymm(dst, src, pixels);
            } else {
                blit_bgra8888_to_bgrx_ymm(dst, src, pixels);
            }
        }
        return;
    }

    unsafe {
        blit_bgra8888_row_scalar(dst, src, pixels, dst_bpp, rgb_format);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(dead_code)]
pub unsafe fn copy_xmm(src: *const u8, dst: *mut u8, len: usize) {
    use core::arch::x86_64::*;

    if len == 0 || src == dst {
        return;
    }

    if regions_overlap(src, dst, len) {
        unsafe {
            ptr::copy(src, dst, len);
        }
        return;
    }

    let mut i = 0usize;
    unsafe {
        while i + 64 <= len {
            let a = _mm_loadu_si128(src.add(i) as *const __m128i);
            let b = _mm_loadu_si128(src.add(i + 16) as *const __m128i);
            let c = _mm_loadu_si128(src.add(i + 32) as *const __m128i);
            let d = _mm_loadu_si128(src.add(i + 48) as *const __m128i);

            _mm_storeu_si128(dst.add(i) as *mut __m128i, a);
            _mm_storeu_si128(dst.add(i + 16) as *mut __m128i, b);
            _mm_storeu_si128(dst.add(i + 32) as *mut __m128i, c);
            _mm_storeu_si128(dst.add(i + 48) as *mut __m128i, d);
            i += 64;
        }

        if i < len {
            ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx")]
#[allow(dead_code)]
pub unsafe fn copy_ymm(src: *const u8, dst: *mut u8, len: usize) {
    use core::arch::x86_64::*;

    if len == 0 || src == dst {
        return;
    }

    if regions_overlap(src, dst, len) {
        unsafe {
            ptr::copy(src, dst, len);
        }
        return;
    }

    let mut i = 0usize;
    unsafe {
        while i + 128 <= len {
            let a = _mm256_loadu_si256(src.add(i) as *const __m256i);
            let b = _mm256_loadu_si256(src.add(i + 32) as *const __m256i);
            let c = _mm256_loadu_si256(src.add(i + 64) as *const __m256i);
            let d = _mm256_loadu_si256(src.add(i + 96) as *const __m256i);

            _mm256_storeu_si256(dst.add(i) as *mut __m256i, a);
            _mm256_storeu_si256(dst.add(i + 32) as *mut __m256i, b);
            _mm256_storeu_si256(dst.add(i + 64) as *mut __m256i, c);
            _mm256_storeu_si256(dst.add(i + 96) as *mut __m256i, d);
            i += 128;
        }

        if i < len {
            ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
        _mm256_zeroupper();
    }
}

unsafe fn blit_bgra8888_row_scalar(
    mut dst: *mut u8,
    mut src: *const u8,
    pixels: usize,
    dst_bpp: usize,
    rgb_format: bool,
) {
    unsafe {
        for _ in 0..pixels {
            let b = ptr::read(src);
            let g = ptr::read(src.add(1));
            let r = ptr::read(src.add(2));
            if rgb_format {
                ptr::write(dst, r);
                ptr::write(dst.add(1), g);
                ptr::write(dst.add(2), b);
            } else {
                ptr::write(dst, b);
                ptr::write(dst.add(1), g);
                ptr::write(dst.add(2), r);
            }
            if dst_bpp == 4 {
                ptr::write(dst.add(3), 0);
            }
            src = src.add(4);
            dst = dst.add(dst_bpp);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blit_bgra8888_to_rgbx_ymm(dst: *mut u8, src: *const u8, pixels: usize) {
    use core::arch::x86_64::*;

    let shuffle = _mm256_setr_epi8(
        2, 1, 0, -128, 6, 5, 4, -128, 10, 9, 8, -128, 14, 13, 12, -128, 2, 1, 0, -128, 6, 5, 4,
        -128, 10, 9, 8, -128, 14, 13, 12, -128,
    );

    let len = pixels * 4;
    let mut offset = 0usize;
    unsafe {
        while offset + 32 <= len {
            let chunk = _mm256_loadu_si256(src.add(offset) as *const __m256i);
            let converted = _mm256_shuffle_epi8(chunk, shuffle);
            _mm256_storeu_si256(dst.add(offset) as *mut __m256i, converted);
            offset += 32;
        }

        let tail_pixels = (len - offset) / 4;
        if tail_pixels != 0 {
            blit_bgra8888_row_scalar(dst.add(offset), src.add(offset), tail_pixels, 4, true);
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blit_bgra8888_to_bgrx_ymm(dst: *mut u8, src: *const u8, pixels: usize) {
    use core::arch::x86_64::*;

    let alpha_mask = _mm256_set1_epi32(0x00ff_ffffu32 as i32);
    let len = pixels * 4;
    let mut offset = 0usize;
    unsafe {
        while offset + 32 <= len {
            let chunk = _mm256_loadu_si256(src.add(offset) as *const __m256i);
            let converted = _mm256_and_si256(chunk, alpha_mask);
            _mm256_storeu_si256(dst.add(offset) as *mut __m256i, converted);
            offset += 32;
        }

        let tail_pixels = (len - offset) / 4;
        if tail_pixels != 0 {
            blit_bgra8888_row_scalar(dst.add(offset), src.add(offset), tail_pixels, 4, false);
        }
        _mm256_zeroupper();
    }
}

#[inline]
fn regions_overlap(src: *const u8, dst: *mut u8, len: usize) -> bool {
    let src_addr = src as usize;
    let dst_addr = dst as usize;
    match (src_addr.checked_add(len), dst_addr.checked_add(len)) {
        (Some(src_end), Some(dst_end)) => src_addr < dst_end && dst_addr < src_end,
        _ => true,
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn enable_sse_baseline() {
    let cr0 = (read_cr0() | CR0_MONITOR_COPROCESSOR) & !CR0_EMULATION;
    unsafe {
        write_cr0(cr0);
    }

    let cr4 = read_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT;
    unsafe {
        write_cr4(cr4);
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn fxsave(area: &mut SimdState) {
    unsafe {
        asm!(
            "fxsave64 [{ptr}]",
            ptr = in(reg) area,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn fxrstor(area: &SimdState) {
    unsafe {
        asm!(
            "fxrstor64 [{ptr}]",
            ptr = in(reg) area,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn xsave(area: &mut SimdState, mask: u64) {
    unsafe {
        asm!(
            "xsave64 [{ptr}]",
            ptr = in(reg) area,
            in("eax") mask as u32,
            in("edx") (mask >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn xrstor(area: &SimdState, mask: u64) {
    unsafe {
        asm!(
            "xrstor64 [{ptr}]",
            ptr = in(reg) area,
            in("eax") mask as u32,
            in("edx") (mask >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn xsetbv0(value: u64) {
    unsafe {
        asm!(
            "xsetbv",
            in("ecx") 0_u32,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_cr0() -> u64 {
    let value: u64;
    unsafe {
        asm!(
            "mov {value}, cr0",
            value = out(reg) value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn write_cr0(value: u64) {
    unsafe {
        asm!(
            "mov cr0, {value}",
            value = in(reg) value,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_cr4() -> u64 {
    let value: u64;
    unsafe {
        asm!(
            "mov {value}, cr4",
            value = out(reg) value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn write_cr4(value: u64) {
    unsafe {
        asm!(
            "mov cr4, {value}",
            value = in(reg) value,
            options(nostack, preserves_flags),
        );
    }
}
