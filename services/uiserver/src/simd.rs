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
}
