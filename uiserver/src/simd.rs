use core::ptr;

#[cfg(target_arch = "x86_64")]
use std::sync::OnceLock;

const COPY_U32_AVX2_THRESHOLD: usize = 32;
const BLEND_U32_AVX2_THRESHOLD: usize = 32;

#[inline]
pub(crate) fn copy_u32s(src: &[u32], dst: &mut [u32]) {
    debug_assert_eq!(src.len(), dst.len());
    if src.is_empty() {
        return;
    }

    #[cfg(target_arch = "x86_64")]
    if avx2_enabled() && src.len() >= COPY_U32_AVX2_THRESHOLD {
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
    if avx2_enabled() && dst.len() >= BLEND_U32_AVX2_THRESHOLD {
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
    let src_b = src_color & 0xff;
    let src_g = (src_color >> 8) & 0xff;
    let src_r = (src_color >> 16) & 0xff;

    for pixel in dst {
        let dst_b = *pixel & 0xff;
        let dst_g = (*pixel >> 8) & 0xff;
        let dst_r = (*pixel >> 16) & 0xff;

        let out_b = (src_b * alpha + dst_b * inv_alpha) / 255;
        let out_g = (src_g * alpha + dst_g * inv_alpha) / 255;
        let out_r = (src_r * alpha + dst_r * inv_alpha) / 255;
        *pixel = (out_r << 16) | (out_g << 8) | out_b;
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
    let src_lo = _mm256_unpacklo_epi8(src, zero);
    let src_hi = _mm256_unpackhi_epi8(src, zero);
    let alpha16 = _mm256_set1_epi16(alpha as i16);
    let inv_alpha16 = _mm256_set1_epi16((255 - alpha) as i16);
    let bias = _mm256_set1_epi16(128);
    let mut i = 0usize;

    unsafe {
        while i + 8 <= len {
            let dst_pixels = _mm256_loadu_si256(dst.add(i) as *const __m256i);
            let dst_lo = _mm256_unpacklo_epi8(dst_pixels, zero);
            let dst_hi = _mm256_unpackhi_epi8(dst_pixels, zero);

            let acc_lo = _mm256_add_epi16(
                _mm256_mullo_epi16(src_lo, alpha16),
                _mm256_mullo_epi16(dst_lo, inv_alpha16),
            );
            let acc_hi = _mm256_add_epi16(
                _mm256_mullo_epi16(src_hi, alpha16),
                _mm256_mullo_epi16(dst_hi, inv_alpha16),
            );

            let adj_lo = _mm256_add_epi16(acc_lo, bias);
            let adj_hi = _mm256_add_epi16(acc_hi, bias);
            let out_lo = _mm256_srli_epi16(
                _mm256_add_epi16(adj_lo, _mm256_srli_epi16(adj_lo, 8)),
                8,
            );
            let out_hi = _mm256_srli_epi16(
                _mm256_add_epi16(adj_hi, _mm256_srli_epi16(adj_hi, 8)),
                8,
            );
            let packed = _mm256_packus_epi16(out_lo, out_hi);

            _mm256_storeu_si256(dst.add(i) as *mut __m256i, packed);
            i += 8;
        }

        if i < len {
            blend_solid_bgr_scalar(core::slice::from_raw_parts_mut(dst.add(i), len - i), src_color, alpha);
        }
        _mm256_zeroupper();
    }
}
