use core::ptr;

use crate::canvas::div255_round;

#[cfg(target_arch = "x86_64")]
use std::sync::OnceLock;

const COPY_U32_AVX2_THRESHOLD: usize = 32;
const BLEND_U32_AVX2_THRESHOLD: usize = 32;
// The scheduler saves/restores XSAVE state and the boot probe exercises the
// rendered framebuffer, so keep the wide UI row operations enabled for the
// display hot path.
const ENABLE_UI_SIMD_FAST_PATHS: bool = true;

#[inline]
pub(crate) fn copy_u32s(src: &[u32], dst: &mut [u32]) {
    let len = src.len().min(dst.len());
    debug_assert_eq!(src.len(), dst.len());
    if len == 0 {
        return;
    }
    let src = &src[..len];
    let dst = &mut dst[..len];

    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && src.len() >= COPY_U32_AVX2_THRESHOLD && avx2_enabled() {
        unsafe {
            copy_u32s_avx2(src.as_ptr(), dst.as_mut_ptr(), src.len());
        }
        return;
    }

    dst.copy_from_slice(src);
}

#[inline]
pub(crate) fn copy_xrgb_opaque_row_u32(src: &[u32], dst: &mut [u32]) {
    let len = src.len().min(dst.len());
    debug_assert_eq!(src.len(), dst.len());
    if len == 0 {
        return;
    }
    let src = &src[..len];
    let dst = &mut dst[..len];

    #[cfg(target_arch = "x86_64")]
    {
        if ENABLE_UI_SIMD_FAST_PATHS && len >= COPY_U32_AVX2_THRESHOLD && avx2_enabled() {
            unsafe {
                copy_xrgb_opaque_row_u32_avx2(src.as_ptr(), dst.as_mut_ptr(), len);
            }
            return;
        }
        copy_xrgb_opaque_row_u32_sse2(src, dst);
        return;
    }

    #[cfg(not(target_arch = "x86_64"))]
    copy_xrgb_opaque_row_u32_scalar(src, dst);
}

#[cfg(any(not(target_arch = "x86_64"), test))]
fn copy_xrgb_opaque_row_u32_scalar(src: &[u32], dst: &mut [u32]) {
    for (destination, source) in dst.iter_mut().zip(src) {
        *destination = *source | 0xff00_0000;
    }
}

#[cfg(target_arch = "x86_64")]
fn copy_xrgb_opaque_row_u32_sse2(src: &[u32], dst: &mut [u32]) {
    use core::arch::x86_64::{
        __m128i, _mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_storeu_si128,
    };

    let opaque = unsafe { _mm_set1_epi32(0xff00_0000_u32 as i32) };
    let mut index = 0;
    while index + 4 <= dst.len() {
        unsafe {
            let pixels = _mm_loadu_si128(src.as_ptr().add(index).cast::<__m128i>());
            _mm_storeu_si128(
                dst.as_mut_ptr().add(index).cast::<__m128i>(),
                _mm_or_si128(pixels, opaque),
            );
        }
        index += 4;
    }
    for (destination, source) in dst[index..].iter_mut().zip(&src[index..]) {
        *destination = *source | 0xff00_0000;
    }
}

#[cfg(test)]
#[inline]
pub(crate) fn row_diff_span_u32(before: &[u32], after: &[u32]) -> Option<(usize, usize)> {
    let len = before.len().min(after.len());
    if len == 0 {
        return None;
    }
    let before = &before[..len];
    let after = &after[..len];

    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && len >= COPY_U32_AVX2_THRESHOLD && avx2_enabled() {
        return unsafe { row_diff_span_u32_avx2(before.as_ptr(), after.as_ptr(), len) };
    }

    row_diff_span_u32_scalar(before, after)
}

/// Compare every row of a rectangular sub-region without per-row AVX2
/// transitions, invoking `on_diff(row_index, first_col, last_col)` for each
/// row that differs.
pub(crate) fn for_each_diff_row_u32(
    before: &[u32],
    after: &[u32],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    stride: usize,
    mut on_diff: impl FnMut(usize, usize, usize),
) {
    if width == 0 || height == 0 {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && width >= COPY_U32_AVX2_THRESHOLD && avx2_enabled() {
        unsafe {
            for_each_diff_row_u32_avx2(before, after, x, y, width, height, stride, on_diff);
        }
        return;
    }

    for row_idx in 0..height {
        let row_start = (y + row_idx) * stride + x;
        let row_end = (row_start + width).min(before.len()).min(after.len());
        if row_start >= row_end {
            break;
        }
        if let Some((first, last)) =
            row_diff_span_u32_scalar(&before[row_start..row_end], &after[row_start..row_end])
        {
            on_diff(y + row_idx, x + first, x + last);
        }
    }
}

/// Diff a rectangular sub-region of `scratch` against `atlas`, copying only
/// the changed span of each row into `atlas` in the same cache-warm pass.
pub(crate) fn diff_and_commit_rect_u32(
    atlas: &mut [u32],
    scratch: &[u32],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    stride: usize,
    mut on_diff: impl FnMut(usize, usize, usize),
) {
    if width == 0 || height == 0 {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && width >= COPY_U32_AVX2_THRESHOLD && avx2_enabled() {
        unsafe {
            diff_and_commit_rect_u32_avx2(atlas, scratch, x, y, width, height, stride, on_diff);
        }
        return;
    }

    for row_idx in 0..height {
        let row_start = (y + row_idx) * stride + x;
        let row_end = (row_start + width).min(atlas.len()).min(scratch.len());
        if row_start >= row_end {
            break;
        }
        if let Some((first, last)) =
            row_diff_span_u32_scalar(&atlas[row_start..row_end], &scratch[row_start..row_end])
        {
            atlas[row_start + first..=row_start + last]
                .copy_from_slice(&scratch[row_start + first..=row_start + last]);
            on_diff(y + row_idx, x + first, x + last);
        }
    }
}

fn row_diff_span_u32_scalar(before: &[u32], after: &[u32]) -> Option<(usize, usize)> {
    let first = before.iter().zip(after).position(|(a, b)| a != b)?;
    let last = before.iter().zip(after).rposition(|(a, b)| a != b)?;
    Some((first, last))
}

#[inline]
pub(crate) fn all_pixels_opaque_u32(pixels: &[u32]) -> bool {
    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && pixels.len() >= COPY_U32_AVX2_THRESHOLD && avx2_enabled() {
        return unsafe { all_pixels_opaque_u32_avx2(pixels.as_ptr(), pixels.len()) };
    }

    all_pixels_opaque_u32_scalar(pixels)
}

fn all_pixels_opaque_u32_scalar(pixels: &[u32]) -> bool {
    pixels.iter().all(|pixel| pixel >> 24 == 0xff)
}

#[inline]
pub(crate) fn blend_solid_bgr(dst: &mut [u32], src_color: u32, alpha: u8) {
    if dst.is_empty() || alpha == 0 {
        return;
    }
    if alpha == u8::MAX {
        dst.fill(src_color);
        return;
    }

    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && dst.len() >= BLEND_U32_AVX2_THRESHOLD && avx2_enabled() {
        unsafe {
            blend_solid_bgr_avx2(dst.as_mut_ptr(), dst.len(), src_color, alpha);
        }
        return;
    }

    blend_solid_bgr_scalar(dst, src_color, alpha);
}

fn blend_solid_bgr_scalar(dst: &mut [u32], src_color: u32, alpha: u8) {
    let alpha = alpha as u32;
    let inv_alpha = 255_u32.saturating_sub(alpha);
    let src_b = (src_color & 0xff) * alpha;
    let src_g = ((src_color >> 8) & 0xff) * alpha;
    let src_r = ((src_color >> 16) & 0xff) * alpha;

    for pixel in dst {
        let dst_b = *pixel & 0xff;
        let dst_g = (*pixel >> 8) & 0xff;
        let dst_r = (*pixel >> 16) & 0xff;

        let out_b = div255_round(src_b + dst_b * inv_alpha);
        let out_g = div255_round(src_g + dst_g * inv_alpha);
        let out_r = div255_round(src_r + dst_r * inv_alpha);
        *pixel = (out_r << 16) | (out_g << 8) | out_b;
    }
}

/// Blend a coverage mask into an opaque BGR row. Zero coverage preserves the
/// complete destination word, full coverage copies the complete color word,
/// and intermediate coverage writes BGR with a zero high byte, just like the
/// scalar canvas primitive.
#[inline]
pub(crate) fn blend_alpha_mask_bgr(alpha: &[u8], dst: &mut [u32], color: u32) {
    debug_assert_eq!(alpha.len(), dst.len());
    let len = alpha.len().min(dst.len());
    let alpha = &alpha[..len];
    let dst = &mut dst[..len];
    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && len >= BLEND_U32_AVX2_THRESHOLD && avx2_enabled() {
        // Safe slices establish readable/writable bounds and disjoint source
        // and destination storage; AVX2 and OS YMM support are runtime checked.
        unsafe {
            blend_alpha_mask_bgr_avx2(alpha.as_ptr(), dst.as_mut_ptr(), len, color);
        }
        return;
    }
    blend_alpha_mask_bgr_scalar(alpha, dst, color);
}

fn blend_alpha_mask_bgr_scalar(alpha: &[u8], dst: &mut [u32], color: u32) {
    let src_r = (color >> 16) & 255;
    let src_g = (color >> 8) & 255;
    let src_b = color & 255;
    for (dst, &coverage) in dst.iter_mut().zip(alpha) {
        if coverage == 0 {
            continue;
        }
        if coverage == u8::MAX {
            *dst = color;
            continue;
        }
        let a = u32::from(coverage);
        let inv = 255 - a;
        let out_r = div255_round(src_r * a + ((*dst >> 16) & 255) * inv);
        let out_g = div255_round(src_g * a + ((*dst >> 8) & 255) * inv);
        let out_b = div255_round(src_b * a + (*dst & 255) * inv);
        *dst = (out_r << 16) | (out_g << 8) | out_b;
    }
}

/// Source-over from premultiplied ARGB into an opaque BGR row. Saturation is
/// intentional even for malformed non-premultiplied input; transparent zero
/// words leave the destination untouched, including its unused high byte.
#[inline]
pub(crate) fn blend_premultiplied_bgr(src: &[u32], dst: &mut [u32]) {
    debug_assert_eq!(src.len(), dst.len());
    let len = src.len().min(dst.len());
    let src = &src[..len];
    let dst = &mut dst[..len];
    #[cfg(target_arch = "x86_64")]
    if ENABLE_UI_SIMD_FAST_PATHS && len >= BLEND_U32_AVX2_THRESHOLD && avx2_enabled() {
        unsafe {
            blend_premultiplied_bgr_avx2(src.as_ptr(), dst.as_mut_ptr(), len);
        }
        return;
    }
    blend_premultiplied_bgr_scalar(src, dst);
}

fn blend_premultiplied_bgr_scalar(src: &[u32], dst: &mut [u32]) {
    for (dst, &pixel) in dst.iter_mut().zip(src) {
        if pixel == 0 {
            continue;
        }
        let alpha = pixel >> 24;
        if alpha == 255 {
            *dst = pixel & 0x00ff_ffff;
            continue;
        }
        let inv = 255 - alpha;
        let out_r = ((pixel >> 16) & 255) + div255_round(((*dst >> 16) & 255) * inv);
        let out_g = ((pixel >> 8) & 255) + div255_round(((*dst >> 8) & 255) * inv);
        let out_b = (pixel & 255) + div255_round((*dst & 255) * inv);
        *dst = (out_r.min(255) << 16) | (out_g.min(255) << 8) | out_b.min(255);
    }
}

#[cfg(target_arch = "x86_64")]
fn avx2_enabled() -> bool {
    static AVX2_ENABLED: OnceLock<bool> = OnceLock::new();
    *AVX2_ENABLED.get_or_init(|| std::arch::is_x86_feature_detected!("avx2"))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_u32s_avx2(src: *const u32, dst: *mut u32, len: usize) {
    use core::arch::x86_64::*;

    let mut i = 0usize;
    unsafe {
        while i + 8 <= len {
            let chunk = _mm256_loadu_si256(src.add(i) as *const __m256i);
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, chunk);
            i += 8;
        }
        if i < len {
            ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blend_solid_bgr_avx2(dst: *mut u32, len: usize, src_color: u32, alpha: u8) {
    use core::arch::x86_64::*;

    let zero = _mm256_setzero_si256();
    let src = _mm256_set1_epi32(src_color as i32);
    let alpha16 = _mm256_set1_epi16(alpha as i16);
    let src_lo = _mm256_mullo_epi16(_mm256_unpacklo_epi8(src, zero), alpha16);
    let src_hi = _mm256_mullo_epi16(_mm256_unpackhi_epi8(src, zero), alpha16);
    let inv_alpha16 = _mm256_set1_epi16((255 - alpha) as i16);
    let bias = _mm256_set1_epi16(128);
    let bgr = _mm256_set1_epi32(0x00ff_ffff);
    let mut i = 0usize;

    unsafe {
        while i + 8 <= len {
            let dst_pixels = _mm256_loadu_si256(dst.add(i) as *const __m256i);
            let dst_lo = _mm256_unpacklo_epi8(dst_pixels, zero);
            let dst_hi = _mm256_unpackhi_epi8(dst_pixels, zero);

            let acc_lo = _mm256_add_epi16(
                src_lo,
                _mm256_mullo_epi16(dst_lo, inv_alpha16),
            );
            let acc_hi = _mm256_add_epi16(
                src_hi,
                _mm256_mullo_epi16(dst_hi, inv_alpha16),
            );

            let adj_lo = _mm256_add_epi16(acc_lo, bias);
            let adj_hi = _mm256_add_epi16(acc_hi, bias);
            let out_lo =
                _mm256_srli_epi16(_mm256_add_epi16(adj_lo, _mm256_srli_epi16(adj_lo, 8)), 8);
            let out_hi =
                _mm256_srli_epi16(_mm256_add_epi16(adj_hi, _mm256_srli_epi16(adj_hi, 8)), 8);
            let packed = _mm256_and_si256(_mm256_packus_epi16(out_lo, out_hi), bgr);

            _mm256_storeu_si256(dst.add(i) as *mut __m256i, packed);
            i += 8;
        }

        if i < len {
            blend_solid_bgr_scalar(
                core::slice::from_raw_parts_mut(dst.add(i), len - i),
                src_color,
                alpha,
            );
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn div255_round_avx2(value: core::arch::x86_64::__m256i) -> core::arch::x86_64::__m256i {
    use core::arch::x86_64::*;
    let biased = _mm256_add_epi16(value, _mm256_set1_epi16(128));
    _mm256_srli_epi16(_mm256_add_epi16(biased, _mm256_srli_epi16(biased, 8)), 8)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blend_alpha_mask_bgr_avx2(alpha: *const u8, dst: *mut u32, len: usize, color: u32) {
    use core::arch::x86_64::*;

    let zero = _mm256_setzero_si256();
    let source = _mm256_set1_epi32(color as i32);
    let source_lo = _mm256_unpacklo_epi8(source, zero);
    let source_hi = _mm256_unpackhi_epi8(source, zero);
    let max_alpha = _mm256_set1_epi16(255);
    let max_alpha32 = _mm256_set1_epi32(255);
    let spread = _mm256_set1_epi32(0x0101_0101);
    let bgr = _mm256_set1_epi32(0x00ff_ffff);
    let mut i = 0;
    unsafe {
        while i + 8 <= len {
            // Exactly eight mask bytes are read, including on the final chunk.
            let mask8 = _mm_loadl_epi64(alpha.add(i) as *const __m128i);
            let mask_bits = _mm_cvtsi128_si64(mask8);
            if mask_bits == 0 {
                i += 8;
                continue;
            }
            if mask_bits == -1 {
                _mm256_storeu_si256(dst.add(i) as *mut __m256i, source);
                i += 8;
                continue;
            }
            let coverage32 = _mm256_cvtepu8_epi32(mask8);
            let coverage_bytes = _mm256_mullo_epi32(coverage32, spread);
            let alpha_lo = _mm256_unpacklo_epi8(coverage_bytes, zero);
            let alpha_hi = _mm256_unpackhi_epi8(coverage_bytes, zero);
            let old = _mm256_loadu_si256(dst.add(i) as *const __m256i);
            let old_lo = _mm256_unpacklo_epi8(old, zero);
            let old_hi = _mm256_unpackhi_epi8(old, zero);
            let out_lo = div255_round_avx2(_mm256_add_epi16(
                _mm256_mullo_epi16(source_lo, alpha_lo),
                _mm256_mullo_epi16(old_lo, _mm256_sub_epi16(max_alpha, alpha_lo)),
            ));
            let out_hi = div255_round_avx2(_mm256_add_epi16(
                _mm256_mullo_epi16(source_hi, alpha_hi),
                _mm256_mullo_epi16(old_hi, _mm256_sub_epi16(max_alpha, alpha_hi)),
            ));
            let intermediate = _mm256_and_si256(_mm256_packus_epi16(out_lo, out_hi), bgr);
            let transparent = _mm256_blendv_epi8(
                intermediate,
                old,
                _mm256_cmpeq_epi32(coverage32, zero),
            );
            let output = _mm256_blendv_epi8(
                transparent,
                source,
                _mm256_cmpeq_epi32(coverage32, max_alpha32),
            );
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, output);
            i += 8;
        }
        if i < len {
            blend_alpha_mask_bgr_scalar(
                core::slice::from_raw_parts(alpha.add(i), len - i),
                core::slice::from_raw_parts_mut(dst.add(i), len - i),
                color,
            );
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blend_premultiplied_bgr_avx2(src: *const u32, dst: *mut u32, len: usize) {
    use core::arch::x86_64::*;

    let zero = _mm256_setzero_si256();
    let bgr = _mm256_set1_epi32(0x00ff_ffff);
    let ones = _mm256_set1_epi8(-1);
    let opaque_alpha = _mm256_set1_epi32(255);
    // Shuffle each pixel's high byte to its four byte lanes. AVX2 byte shuffles
    // are lane-local, hence the indices restart in the upper 128-bit half.
    let alpha_shuffle = _mm256_setr_epi8(
        3, 3, 3, 3, 7, 7, 7, 7, 11, 11, 11, 11, 15, 15, 15, 15,
        3, 3, 3, 3, 7, 7, 7, 7, 11, 11, 11, 11, 15, 15, 15, 15,
    );
    let mut i = 0;
    unsafe {
        while i + 8 <= len {
            let source = _mm256_loadu_si256(src.add(i) as *const __m256i);
            if _mm256_testz_si256(source, source) != 0 {
                i += 8;
                continue;
            }
            if _mm256_movemask_epi8(_mm256_cmpeq_epi32(
                _mm256_srli_epi32(source, 24),
                opaque_alpha,
            )) == -1
            {
                _mm256_storeu_si256(
                    dst.add(i) as *mut __m256i,
                    _mm256_and_si256(source, bgr),
                );
                i += 8;
                continue;
            }
            let old = _mm256_loadu_si256(dst.add(i) as *const __m256i);
            let inv = _mm256_sub_epi8(ones, _mm256_shuffle_epi8(source, alpha_shuffle));
            let scaled_lo = div255_round_avx2(_mm256_mullo_epi16(
                _mm256_unpacklo_epi8(old, zero),
                _mm256_unpacklo_epi8(inv, zero),
            ));
            let scaled_hi = div255_round_avx2(_mm256_mullo_epi16(
                _mm256_unpackhi_epi8(old, zero),
                _mm256_unpackhi_epi8(inv, zero),
            ));
            // A malformed channel can reach 510; unsigned packing supplies
            // exactly the scalar saturating minimum with 255.
            let out_lo = _mm256_add_epi16(_mm256_unpacklo_epi8(source, zero), scaled_lo);
            let out_hi = _mm256_add_epi16(_mm256_unpackhi_epi8(source, zero), scaled_hi);
            let output = _mm256_and_si256(_mm256_packus_epi16(out_lo, out_hi), bgr);
            let output = _mm256_blendv_epi8(output, old, _mm256_cmpeq_epi32(source, zero));
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, output);
            i += 8;
        }
        if i < len {
            blend_premultiplied_bgr_scalar(
                core::slice::from_raw_parts(src.add(i), len - i),
                core::slice::from_raw_parts_mut(dst.add(i), len - i),
            );
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_xrgb_opaque_row_u32_avx2(src: *const u32, dst: *mut u32, len: usize) {
    use core::arch::x86_64::*;

    let opaque = _mm256_set1_epi32(0xff00_0000_u32 as i32);
    let mut i = 0usize;
    unsafe {
        while i + 32 <= len {
            let v0 = _mm256_or_si256(_mm256_loadu_si256(src.add(i) as *const __m256i), opaque);
            let v1 = _mm256_or_si256(_mm256_loadu_si256(src.add(i + 8) as *const __m256i), opaque);
            let v2 = _mm256_or_si256(_mm256_loadu_si256(src.add(i + 16) as *const __m256i), opaque);
            let v3 = _mm256_or_si256(_mm256_loadu_si256(src.add(i + 24) as *const __m256i), opaque);
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, v0);
            _mm256_storeu_si256(dst.add(i + 8) as *mut __m256i, v1);
            _mm256_storeu_si256(dst.add(i + 16) as *mut __m256i, v2);
            _mm256_storeu_si256(dst.add(i + 24) as *mut __m256i, v3);
            i += 32;
        }
        while i + 8 <= len {
            let v = _mm256_or_si256(_mm256_loadu_si256(src.add(i) as *const __m256i), opaque);
            _mm256_storeu_si256(dst.add(i) as *mut __m256i, v);
            i += 8;
        }
        while i < len {
            *dst.add(i) = *src.add(i) | 0xff00_0000;
            i += 1;
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn row_diff_span_u32_avx2_inner(
    before: *const u32,
    after: *const u32,
    len: usize,
) -> Option<(usize, usize)> {
    use core::arch::x86_64::*;

    let mut i = 0usize;
    let mut first = None;
    unsafe {
        while i + 32 <= len {
            let eq0 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(i) as *const __m256i),
                _mm256_loadu_si256(after.add(i) as *const __m256i),
            );
            let eq1 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(i + 8) as *const __m256i),
                _mm256_loadu_si256(after.add(i + 8) as *const __m256i),
            );
            let eq2 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(i + 16) as *const __m256i),
                _mm256_loadu_si256(after.add(i + 16) as *const __m256i),
            );
            let eq3 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(i + 24) as *const __m256i),
                _mm256_loadu_si256(after.add(i + 24) as *const __m256i),
            );
            let all = _mm256_and_si256(_mm256_and_si256(eq0, eq1), _mm256_and_si256(eq2, eq3));
            if _mm256_movemask_epi8(all) != -1 {
                for (offset, eq) in [(0, eq0), (8, eq1), (16, eq2), (24, eq3)] {
                    let mask = _mm256_movemask_epi8(eq);
                    if mask != -1 {
                        let diff_mask = (!mask) as u32;
                        first = Some(i + offset + (diff_mask.trailing_zeros() as usize / 4));
                        break;
                    }
                }
                break;
            }
            i += 32;
        }
        if first.is_none() {
            while i + 8 <= len {
                let va = _mm256_loadu_si256(before.add(i) as *const __m256i);
                let vb = _mm256_loadu_si256(after.add(i) as *const __m256i);
                let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi32(va, vb));
                if mask != -1 {
                    let diff_mask = (!mask) as u32;
                    first = Some(i + (diff_mask.trailing_zeros() as usize / 4));
                    break;
                }
                i += 8;
            }
        }
        let first = match first {
            Some(pos) => pos,
            None => {
                while i < len {
                    if *before.add(i) != *after.add(i) {
                        first = Some(i);
                        break;
                    }
                    i += 1;
                }
                first?
            }
        };

        let mut end = len;
        while end >= first + 32 {
            let base = end - 32;
            let eq0 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(base) as *const __m256i),
                _mm256_loadu_si256(after.add(base) as *const __m256i),
            );
            let eq1 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(base + 8) as *const __m256i),
                _mm256_loadu_si256(after.add(base + 8) as *const __m256i),
            );
            let eq2 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(base + 16) as *const __m256i),
                _mm256_loadu_si256(after.add(base + 16) as *const __m256i),
            );
            let eq3 = _mm256_cmpeq_epi32(
                _mm256_loadu_si256(before.add(base + 24) as *const __m256i),
                _mm256_loadu_si256(after.add(base + 24) as *const __m256i),
            );
            let all = _mm256_and_si256(_mm256_and_si256(eq0, eq1), _mm256_and_si256(eq2, eq3));
            if _mm256_movemask_epi8(all) != -1 {
                for (offset, eq) in [(24, eq3), (16, eq2), (8, eq1), (0, eq0)] {
                    let mask = _mm256_movemask_epi8(eq);
                    if mask != -1 {
                        let diff_mask = (!mask) as u32;
                        let last_in_chunk = 7 - (diff_mask.leading_zeros() as usize / 4);
                        return Some((first, base + offset + last_in_chunk));
                    }
                }
            }
            end = base;
        }
        while end >= first + 8 {
            let base = end - 8;
            let va = _mm256_loadu_si256(before.add(base) as *const __m256i);
            let vb = _mm256_loadu_si256(after.add(base) as *const __m256i);
            let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi32(va, vb));
            if mask != -1 {
                let diff_mask = (!mask) as u32;
                let last_in_chunk = 7 - (diff_mask.leading_zeros() as usize / 4);
                return Some((first, base + last_in_chunk));
            }
            end = base;
        }
        for j in (first..end).rev() {
            if *before.add(j) != *after.add(j) {
                return Some((first, j));
            }
        }
        Some((first, first))
    }
}

#[cfg(all(target_arch = "x86_64", test))]
#[target_feature(enable = "avx2")]
unsafe fn row_diff_span_u32_avx2(
    before: *const u32,
    after: *const u32,
    len: usize,
) -> Option<(usize, usize)> {
    use core::arch::x86_64::_mm256_zeroupper;
    let span = unsafe { row_diff_span_u32_avx2_inner(before, after, len) };
    _mm256_zeroupper();
    span
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn for_each_diff_row_u32_avx2(
    before: &[u32],
    after: &[u32],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    stride: usize,
    mut on_diff: impl FnMut(usize, usize, usize),
) {
    use core::arch::x86_64::_mm256_zeroupper;
    let max_len = before.len().min(after.len());
    let before_ptr = before.as_ptr();
    let after_ptr = after.as_ptr();
    for row_idx in 0..height {
        let row_start = (y + row_idx) * stride + x;
        let row_end = (row_start + width).min(max_len);
        if row_start >= row_end {
            break;
        }
        let row_len = row_end - row_start;
        let span = unsafe {
            row_diff_span_u32_avx2_inner(
                before_ptr.add(row_start),
                after_ptr.add(row_start),
                row_len,
            )
        };
        if let Some((first, last)) = span {
            on_diff(y + row_idx, x + first, x + last);
        }
    }
    _mm256_zeroupper();
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn diff_and_commit_rect_u32_avx2(
    atlas: &mut [u32],
    scratch: &[u32],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    stride: usize,
    mut on_diff: impl FnMut(usize, usize, usize),
) {
    use core::arch::x86_64::*;
    let max_len = atlas.len().min(scratch.len());
    let atlas_ptr = atlas.as_mut_ptr();
    let scratch_ptr = scratch.as_ptr();
    unsafe {
        for row_idx in 0..height {
            let row_start = (y + row_idx) * stride + x;
            let row_end = (row_start + width).min(max_len);
            if row_start >= row_end {
                break;
            }
            let row_len = row_end - row_start;
            if let Some((first, last)) = row_diff_span_u32_avx2_inner(
                atlas_ptr.add(row_start),
                scratch_ptr.add(row_start),
                row_len,
            ) {
                let copy_len = last + 1 - first;
                let src = scratch_ptr.add(row_start + first);
                let dst = atlas_ptr.add(row_start + first);
                let mut i = 0usize;
                while i + 32 <= copy_len {
                    let v0 = _mm256_loadu_si256(src.add(i) as *const __m256i);
                    let v1 = _mm256_loadu_si256(src.add(i + 8) as *const __m256i);
                    let v2 = _mm256_loadu_si256(src.add(i + 16) as *const __m256i);
                    let v3 = _mm256_loadu_si256(src.add(i + 24) as *const __m256i);
                    _mm256_storeu_si256(dst.add(i) as *mut __m256i, v0);
                    _mm256_storeu_si256(dst.add(i + 8) as *mut __m256i, v1);
                    _mm256_storeu_si256(dst.add(i + 16) as *mut __m256i, v2);
                    _mm256_storeu_si256(dst.add(i + 24) as *mut __m256i, v3);
                    i += 32;
                }
                while i + 8 <= copy_len {
                    let v = _mm256_loadu_si256(src.add(i) as *const __m256i);
                    _mm256_storeu_si256(dst.add(i) as *mut __m256i, v);
                    i += 8;
                }
                if i < copy_len {
                    ptr::copy_nonoverlapping(src.add(i), dst.add(i), copy_len - i);
                }
                on_diff(y + row_idx, x + first, x + last);
            }
        }
        _mm256_zeroupper();
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn all_pixels_opaque_u32_avx2(pixels: *const u32, len: usize) -> bool {
    use core::arch::x86_64::*;

    let alpha_mask = _mm256_set1_epi32(0xff00_0000_u32 as i32);
    let mut i = 0usize;
    unsafe {
        while i + 32 <= len {
            let v0 = _mm256_loadu_si256(pixels.add(i) as *const __m256i);
            let v1 = _mm256_loadu_si256(pixels.add(i + 8) as *const __m256i);
            let v2 = _mm256_loadu_si256(pixels.add(i + 16) as *const __m256i);
            let v3 = _mm256_loadu_si256(pixels.add(i + 24) as *const __m256i);
            let combined = _mm256_and_si256(_mm256_and_si256(v0, v1), _mm256_and_si256(v2, v3));
            let eq = _mm256_cmpeq_epi32(_mm256_and_si256(combined, alpha_mask), alpha_mask);
            if _mm256_movemask_epi8(eq) != -1 {
                _mm256_zeroupper();
                return false;
            }
            i += 32;
        }
        while i + 8 <= len {
            let chunk = _mm256_loadu_si256(pixels.add(i) as *const __m256i);
            let eq = _mm256_cmpeq_epi32(_mm256_and_si256(chunk, alpha_mask), alpha_mask);
            if _mm256_movemask_epi8(eq) != -1 {
                _mm256_zeroupper();
                return false;
            }
            i += 8;
        }
        _mm256_zeroupper();
        all_pixels_opaque_u32_scalar(core::slice::from_raw_parts(pixels.add(i), len - i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_blend_rounds_identically_in_wide_and_scalar_paths() {
        for alpha in [1_u8, 7, 64, 128, 200, 254] {
            let src = 0xb737_c0a5;
            let base: Vec<u32> = (0..67_u32)
                .map(|i| {
                    (i * 17 & 0xff) << 24
                        | (i * 3 & 0xff) << 16
                        | (i * 5 & 0xff) << 8
                        | (i * 11 & 0xff)
                })
                .collect();
            let mut wide = base.clone();
            blend_solid_bgr(&mut wide, src, alpha);
            let mut scalar = base.clone();
            blend_solid_bgr_scalar(&mut scalar, src, alpha);
            assert_eq!(wide, scalar, "alpha={alpha}");
        }
    }

    #[test]
    fn coverage_blend_preserves_endpoints_high_bytes_and_tails() {
        let base: Vec<u32> = (0..83_u32)
            .map(|i| i.wrapping_mul(0x97a3_175b))
            .collect();
        for color in [0, 0x00ff_ffff, 0xbd37_c0a5] {
            for coverage in [
                vec![0; 83],
                vec![255; 83],
                (0..83).map(|i| [0, 255, 1, 254, 128, 64, 200][i % 7]).collect(),
            ] {
                // Offset starts exercise unaligned source/destination addresses.
                let mut actual = base.clone();
                let mut expected = base.clone();
                blend_alpha_mask_bgr(&coverage[1..82], &mut actual[1..82], color);
                blend_alpha_mask_bgr_scalar(&coverage[1..82], &mut expected[1..82], color);
                assert_eq!(actual, expected, "color={color:08x}");
            }
        }
    }

    #[test]
    fn premultiplied_blend_matches_scalar_for_all_alpha_and_invalid_channels() {
        let mut source = vec![0; 8];
        source.extend([0xff12_3456; 8]);
        for alpha in 0..=255_u32 {
            source.push((alpha << 24) | 0x00f3_81cd);
            source.push(alpha << 24);
            source.push(0);
        }
        source.push(0x7fff_ffff);
        let base: Vec<u32> = (0..source.len())
            .map(|i| (i as u32).wrapping_mul(0x97a3_175b))
            .collect();
        let mut actual = base.clone();
        let mut expected = base;
        blend_premultiplied_bgr(&source, &mut actual);
        blend_premultiplied_bgr_scalar(&source, &mut expected);
        assert_eq!(actual, expected);
    }

    #[test]
    fn row_diff_span_matches_scalar_across_lengths_and_positions() {
        for len in [0_usize, 1, 7, 8, 9, 31, 32, 33, 63, 64, 67, 96, 127, 128, 131] {
            let base: Vec<u32> = (0..len as u32).map(|i| i.wrapping_mul(0x1357_9bdf)).collect();
            assert_eq!(row_diff_span_u32(&base, &base), None);
            assert_eq!(row_diff_span_u32_scalar(&base, &base), None);
            for first in [0, 1, 7, 8, 31, 32, 63, len.saturating_sub(2), len.saturating_sub(1)] {
                for last in [first, len.saturating_sub(1)] {
                    if first >= len || last >= len {
                        continue;
                    }
                    let mut modified = base.clone();
                    modified[first] ^= 1;
                    modified[last] ^= 0x8000_0000;
                    assert_eq!(
                        row_diff_span_u32(&base, &modified),
                        row_diff_span_u32_scalar(&base, &modified),
                        "len={len} first={first} last={last}"
                    );
                }
            }
        }
    }

    #[test]
    fn all_pixels_opaque_matches_scalar_across_lengths_and_positions() {
        for len in [0_usize, 1, 7, 8, 31, 32, 33, 67, 96, 127, 128, 131] {
            let opaque = vec![0xff12_3456_u32; len];
            assert!(all_pixels_opaque_u32(&opaque));
            assert!(all_pixels_opaque_u32_scalar(&opaque));
            for pos in [0, 1, 7, 8, 31, 32, len / 2, len.saturating_sub(1)] {
                if pos >= len {
                    continue;
                }
                let mut translucent = opaque.clone();
                translucent[pos] = 0xfe12_3456;
                assert_eq!(
                    all_pixels_opaque_u32(&translucent),
                    all_pixels_opaque_u32_scalar(&translucent),
                    "len={len} pos={pos}"
                );
            }
        }
    }

    #[test]
    fn copy_xrgb_opaque_row_matches_scalar_across_lengths() {
        for len in [0_usize, 1, 3, 4, 7, 8, 31, 32, 33, 63, 64, 99, 131] {
            let src: Vec<u32> = (0..len as u32)
                .map(|i| (i.wrapping_mul(0x0103_070b)) & 0x7fff_ffff)
                .collect();
            let mut actual = vec![0xdead_beef; len];
            let mut expected = vec![0xdead_beef; len];
            copy_xrgb_opaque_row_u32(&src, &mut actual);
            copy_xrgb_opaque_row_u32_scalar(&src, &mut expected);
            assert_eq!(actual, expected, "len={len}");
        }
    }

    #[test]
    fn diff_and_commit_rect_updates_only_changed_spans_inside_rect() {
        let (stride, height) = (100_usize, 16_usize);
        let base: Vec<u32> = (0..stride * height).map(|i| i as u32).collect();
        let mut scratch = base.clone();
        // Outside rect: must remain untouched in `atlas` and unreported.
        scratch[0] ^= 0x1111;
        scratch[15 * stride + 99] ^= 0x2222;
        // Inside rect (x=5, y=2, w=71, h=5): spans exercise 32-pixel AVX2 unroll + tail.
        scratch[2 * stride + 6] ^= 1;
        scratch[2 * stride + 74] ^= 2;
        scratch[4 * stride + 40] ^= 4;
        scratch[6 * stride + 5] ^= 8;

        let mut atlas = base.clone();
        let mut reported = Vec::new();
        diff_and_commit_rect_u32(&mut atlas, &scratch, 5, 2, 71, 5, stride, |y, first, last| {
            reported.push((y, first, last));
        });
        assert_eq!(reported, vec![(2, 6, 74), (4, 40, 40), (6, 5, 5)]);
        assert_eq!(atlas[0], base[0], "outside rect must not be modified");
        assert_eq!(atlas[15 * stride + 99], base[15 * stride + 99]);
        for y in 2..7 {
            let row = y * stride;
            assert_eq!(&atlas[row + 5..row + 76], &scratch[row + 5..row + 76]);
        }
    }
}
