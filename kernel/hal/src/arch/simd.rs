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
const CPUID_XSAVEOPT: u32 = 1 << 0;

const XFEATURE_X87: u64 = 1 << 0;
const XFEATURE_SSE: u64 = 1 << 1;
const XFEATURE_YMM: u64 = 1 << 2;

const SIMD_MODE_FXSAVE: u8 = 1;
const SIMD_MODE_XSAVE: u8 = 2;

const SIMD_PROFILE_UNINITIALIZED: u8 = 0;
const SIMD_PROFILE_READY: u8 = 2;

const SIMD_STATE_BYTES: usize = 4096;
const FXSAVE_STATE_BYTES: usize = 512;
const XMM_COPY_THRESHOLD_BYTES: usize = 256;
const YMM_COPY_THRESHOLD_BYTES: usize = 256;

static SIMD_MODE: AtomicU8 = AtomicU8::new(SIMD_MODE_FXSAVE);
static SIMD_PROFILE_STATE: AtomicU8 = AtomicU8::new(SIMD_PROFILE_UNINITIALIZED);
static XSTATE_MASK: AtomicU64 = AtomicU64::new(XFEATURE_X87 | XFEATURE_SSE);
static SIMD_STATE_REQUIRED_BYTES: AtomicUsize = AtomicUsize::new(FXSAVE_STATE_BYTES);
static XSTATE_LAYOUT_FINGERPRINT: AtomicU64 = AtomicU64::new(0);
static AVX_ENABLED: AtomicBool = AtomicBool::new(false);
static AVX2_ENABLED: AtomicBool = AtomicBool::new(false);
static XSAVEOPT_ENABLED: AtomicBool = AtomicBool::new(false);

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

impl Default for SimdState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn init() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        enable_sse_baseline();

        // The BSP selects one migration-safe profile before AP startup. APs
        // configure exactly that profile or fail closed; changing process-
        // global mode atomics from an AP would let a task be saved with one
        // instruction/layout and restored with another after migration.
        if nucleus_core::util::lockdep::current_cpu_index() != 0
            || SIMD_PROFILE_STATE.load(Ordering::Acquire) == SIMD_PROFILE_READY
        {
            configure_published_profile();
            return;
        }

        let max_leaf = __cpuid(0).eax;
        let leaf1 = __cpuid(1);
        if (leaf1.ecx & CPUID_FEATURE_XSAVE) == 0 || max_leaf < 0xD {
            publish_fxsave_profile();
            return;
        }

        let xsave_leaf = __cpuid_count(0xD, 0);
        let supported_xcr0 = u64::from(xsave_leaf.eax) | (u64::from(xsave_leaf.edx) << 32);
        if (supported_xcr0 & (XFEATURE_X87 | XFEATURE_SSE)) != (XFEATURE_X87 | XFEATURE_SSE) {
            publish_fxsave_profile();
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

        // CPUID.0Dh:0.EBX is the byte count for the features currently
        // enabled in XCR0. EAX/EDX are a feature bitmap, never a size.
        let xsave_leaf = __cpuid_count(0xD, 0);
        let required_bytes = (xsave_leaf.ebx as usize).max(FXSAVE_STATE_BYTES);
        if required_bytes > SIMD_STATE_BYTES {
            panic!(
                "SIMD xsave state requires {} bytes but buffer is only {} bytes",
                required_bytes, SIMD_STATE_BYTES,
            );
        }
        let xsaveopt_enabled = (__cpuid_count(0xD, 1).eax & CPUID_XSAVEOPT) != 0;
        let layout_fingerprint = xstate_layout_fingerprint(requested_mask);

        XSTATE_MASK.store(requested_mask, Ordering::Relaxed);
        SIMD_STATE_REQUIRED_BYTES.store(required_bytes, Ordering::Relaxed);
        XSTATE_LAYOUT_FINGERPRINT.store(layout_fingerprint, Ordering::Relaxed);
        AVX_ENABLED.store(avx_enabled, Ordering::Relaxed);
        AVX2_ENABLED.store(avx2_enabled, Ordering::Relaxed);
        XSAVEOPT_ENABLED.store(xsaveopt_enabled, Ordering::Relaxed);
        SIMD_MODE.store(SIMD_MODE_XSAVE, Ordering::Relaxed);
        // ORDERING: release publishes the complete immutable SIMD migration
        // contract before any AP may enable scheduling.
        SIMD_PROFILE_STATE.store(SIMD_PROFILE_READY, Ordering::Release);
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn publish_fxsave_profile() {
    XSTATE_MASK.store(XFEATURE_X87 | XFEATURE_SSE, Ordering::Relaxed);
    SIMD_STATE_REQUIRED_BYTES.store(FXSAVE_STATE_BYTES, Ordering::Relaxed);
    XSTATE_LAYOUT_FINGERPRINT.store(0, Ordering::Relaxed);
    AVX_ENABLED.store(false, Ordering::Relaxed);
    AVX2_ENABLED.store(false, Ordering::Relaxed);
    XSAVEOPT_ENABLED.store(false, Ordering::Relaxed);
    SIMD_MODE.store(SIMD_MODE_FXSAVE, Ordering::Relaxed);
    SIMD_PROFILE_STATE.store(SIMD_PROFILE_READY, Ordering::Release);
}

#[cfg(target_arch = "x86_64")]
unsafe fn configure_published_profile() {
    assert_eq!(
        SIMD_PROFILE_STATE.load(Ordering::Acquire),
        SIMD_PROFILE_READY,
        "SIMD invariant: AP initialized before BSP profile publication"
    );
    let mode = SIMD_MODE.load(Ordering::Acquire);
    if mode == SIMD_MODE_FXSAVE {
        assert_eq!(
            SIMD_STATE_REQUIRED_BYTES.load(Ordering::Acquire),
            FXSAVE_STATE_BYTES,
            "SIMD invariant: malformed FXSAVE profile"
        );
        return;
    }
    assert_eq!(
        mode, SIMD_MODE_XSAVE,
        "SIMD invariant: unknown published state mode"
    );

    let max_leaf = __cpuid(0).eax;
    let leaf1 = __cpuid(1);
    assert!(
        max_leaf >= 0xD && (leaf1.ecx & CPUID_FEATURE_XSAVE) != 0,
        "SIMD invariant: CPU lacks BSP-required XSAVE"
    );
    let xsave_leaf = __cpuid_count(0xD, 0);
    let supported_xcr0 = u64::from(xsave_leaf.eax) | (u64::from(xsave_leaf.edx) << 32);
    let required_mask = XSTATE_MASK.load(Ordering::Acquire);
    assert_eq!(
        supported_xcr0 & required_mask,
        required_mask,
        "SIMD invariant: CPU lacks BSP-required XCR0 components"
    );
    assert!(
        !AVX_ENABLED.load(Ordering::Acquire) || (leaf1.ecx & CPUID_FEATURE_AVX) != 0,
        "SIMD invariant: CPU lacks BSP-required AVX"
    );
    if AVX2_ENABLED.load(Ordering::Acquire) {
        assert!(
            max_leaf >= 7 && (__cpuid_count(7, 0).ebx & CPUID_EXT_FEATURE_AVX2) != 0,
            "SIMD invariant: CPU lacks BSP-required AVX2"
        );
    }
    if XSAVEOPT_ENABLED.load(Ordering::Acquire) {
        assert_ne!(
            __cpuid_count(0xD, 1).eax & CPUID_XSAVEOPT,
            0,
            "SIMD invariant: CPU lacks BSP-required XSAVEOPT"
        );
    }

    unsafe {
        write_cr4(read_cr4() | CR4_OSXSAVE);
        xsetbv0(required_mask);
    }
    let enabled_leaf = __cpuid_count(0xD, 0);
    let required_bytes = (enabled_leaf.ebx as usize).max(FXSAVE_STATE_BYTES);
    assert!(
        required_bytes <= SIMD_STATE_BYTES,
        "SIMD invariant: CPU XSAVE state exceeds fixed task buffer"
    );
    assert_eq!(
        required_bytes,
        SIMD_STATE_REQUIRED_BYTES.load(Ordering::Acquire),
        "SIMD invariant: CPU XSAVE byte layout differs from BSP"
    );
    assert_eq!(
        unsafe { xstate_layout_fingerprint(required_mask) },
        XSTATE_LAYOUT_FINGERPRINT.load(Ordering::Acquire),
        "SIMD invariant: CPU XSTATE component layout differs from BSP"
    );
}

#[cfg(target_arch = "x86_64")]
unsafe fn xstate_layout_fingerprint(mask: u64) -> u64 {
    // FNV-1a over every enabled extended component's architectural CPUID
    // descriptor. Equal total byte counts alone do not make XRSTOR images
    // migration-compatible when component offsets or format flags differ.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut component = 2u32;
    while component < 64 {
        if mask & (1u64 << component) != 0 {
            let leaf = __cpuid_count(0xD, component);
            for value in [component, leaf.eax, leaf.ebx, leaf.ecx, leaf.edx] {
                hash ^= u64::from(value);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        component += 1;
    }
    hash
}

pub fn mode_name() -> &'static str {
    match SIMD_MODE.load(Ordering::Acquire) {
        SIMD_MODE_XSAVE if avx2_enabled() => "xsave-avx2",
        SIMD_MODE_XSAVE if avx_enabled() => "xsave-avx",
        SIMD_MODE_XSAVE => "xsave-sse",
        _ => "fxsave-sse",
    }
}

pub fn avx_enabled() -> bool {
    AVX_ENABLED.load(Ordering::Acquire)
}

pub fn avx2_enabled() -> bool {
    AVX2_ENABLED.load(Ordering::Acquire)
}

#[inline]
/// # Safety
///
/// `area` must be writable, 64-byte aligned storage for the active SIMD mode.
pub unsafe fn save_state(area: &mut SimdState) {
    match SIMD_MODE.load(Ordering::Acquire) {
        SIMD_MODE_XSAVE if XSAVEOPT_ENABLED.load(Ordering::Acquire) => unsafe {
            xsaveopt(area, XSTATE_MASK.load(Ordering::Acquire))
        },
        SIMD_MODE_XSAVE => unsafe { xsave(area, XSTATE_MASK.load(Ordering::Acquire)) },
        _ => unsafe { fxsave(area) },
    }
}

#[inline]
/// # Safety
///
/// `area` must contain state saved for the active SIMD mode on this CPU.
pub unsafe fn restore_state(area: &SimdState) {
    match SIMD_MODE.load(Ordering::Acquire) {
        SIMD_MODE_XSAVE => unsafe { xrstor(area, XSTATE_MASK.load(Ordering::Acquire)) },
        _ => unsafe { fxrstor(area) },
    }
}

#[inline]
/// # Safety
///
/// `src` and `dst` must be valid for `len` bytes. Overlap is permitted.
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

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
/// # Safety
///
/// `src` and `dst` must be valid for `len` bytes. Overlap is permitted.
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
/// # Safety
///
/// `src` and `dst` must be valid for `len` bytes. Overlap is permitted.
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
unsafe fn xsaveopt(area: &mut SimdState, mask: u64) {
    unsafe {
        asm!(
            "xsaveopt64 [{ptr}]",
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
