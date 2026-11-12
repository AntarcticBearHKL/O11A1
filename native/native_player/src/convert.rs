//! NV12 → BGRA conversion for the upload path.
//!
//! ffmpeg writes NV12 (1.5 bytes/pixel) and the swapchain wants BGRA. Doing the
//! yuv→rgb conversion here (SSE2, 8 pixels per iteration, BT.709 limited range,
//! the matrix every Bilibili DASH stream declares) is much cheaper than asking
//! swscale for RGBA over the pipe and then swizzling it.

/// Convert one NV12 frame (`src.len() == w*h*3/2`) into `dst` (`w*h*4`, BGRA).
pub fn nv12_to_bgra(dst: &mut [u8], src: &[u8], width: usize, height: usize) {
    let needed_src = width * height * 3 / 2;
    let needed_dst = width * height * 4;
    if width < 2
        || height < 2
        || src.len() < needed_src
        || dst.len() < needed_dst
    {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: both slices are at least `needed_*` bytes and the routine
        // stays inside them.
        unsafe { nv12_to_bgra_sse2(dst.as_mut_ptr(), src.as_ptr(), width, height) };
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    nv12_to_bgra_scalar(dst, src, width, height);
}

const Y_GAIN: i32 = 298;
const RV_GAIN: i32 = 403;
const GU_GAIN: i32 = 48;
const GV_GAIN: i32 = 120;
const BU_GAIN: i32 = 475;

/// Scalar reference. Bit-identical to the SSE2 path (same 8.8 decomposition).
#[cfg(any(test, not(target_arch = "x86_64")))]
pub fn nv12_to_bgra_scalar(dst: &mut [u8], src: &[u8], width: usize, height: usize) {
    let y_plane = src;
    let uv_plane = &src[width * height..];
    for row in 0..height {
        let y_row = &y_plane[row * width..row * width + width];
        let uv_row = &uv_plane[(row / 2) * width..(row / 2) * width + width];
        let out = &mut dst[row * width * 4..(row + 1) * width * 4];
        for col in 0..width {
            let yv = y_row[col] as i32 - 16;
            let y = yv + ((yv * (Y_GAIN - 256) + 128) >> 8);
            let u = uv_row[(col / 2) * 2] as i32 - 128;
            let v = uv_row[(col / 2) * 2 + 1] as i32 - 128;
            let r = y + v + ((v * (RV_GAIN - 256) + 128) >> 8);
            let g = y - ((GU_GAIN * u + GV_GAIN * v + 128) >> 8);
            let b = y + u + ((u * (BU_GAIN - 256) + 128) >> 8);
            let at = col * 4;
            out[at] = b.clamp(0, 255) as u8;
            out[at + 1] = g.clamp(0, 255) as u8;
            out[at + 2] = r.clamp(0, 255) as u8;
            out[at + 3] = 255;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn nv12_to_bgra_sse2(dst: *mut u8, src: *const u8, width: usize, height: usize) {
    use std::arch::x86_64::*;

    let y_plane = src;
    let uv_plane = src.add(width * height);
    let luma_bias = _mm_set1_epi16(16);
    let chroma_bias = _mm_set1_epi16(128);
    let y_frac = _mm_set1_epi16((Y_GAIN - 256) as i16);
    let rv_frac = _mm_set1_epi16((RV_GAIN - 256) as i16);
    let bu_frac = _mm_set1_epi16((BU_GAIN - 256) as i16);
    let gu_gain = _mm_set1_epi16(GU_GAIN as i16);
    let gv_gain = _mm_set1_epi16(GV_GAIN as i16);
    let alpha = _mm_set1_epi8(-1i8);
    let u_mask = _mm_set1_epi16(0x00FF);
    let half = _mm_set1_epi16(128);

    for row in 0..height {
        let y_row = y_plane.add(row * width);
        let uv_row = uv_plane.add((row / 2) * width);
        let out = dst.add(row * width * 4);
        let mut col = 0usize;
        while col + 8 <= width {
            let y8 = _mm_loadl_epi64(y_row.add(col) as *const __m128i);
            let uv8 = _mm_loadl_epi64(uv_row.add(col) as *const __m128i);

            let y16 = _mm_unpacklo_epi8(y8, _mm_setzero_si128());
            let yv = _mm_sub_epi16(y16, luma_bias);
            let y = _mm_add_epi16(
                yv,
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(yv, y_frac), half), 8),
            );
            let u16 = _mm_sub_epi16(_mm_and_si128(uv8, u_mask), chroma_bias);
            let v16 = _mm_sub_epi16(_mm_srli_epi16(uv8, 8), chroma_bias);
            let u_px = _mm_unpacklo_epi16(u16, u16);
            let v_px = _mm_unpacklo_epi16(v16, v16);

            let r_term = _mm_add_epi16(
                v_px,
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(v_px, rv_frac), half), 8),
            );
            let b_term = _mm_add_epi16(
                u_px,
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(u_px, bu_frac), half), 8),
            );
            let g_uv = _mm_add_epi16(
                _mm_add_epi16(
                    _mm_mullo_epi16(u_px, gu_gain),
                    _mm_mullo_epi16(v_px, gv_gain),
                ),
                half,
            );

            let r = _mm_adds_epi16(y, r_term);
            let g = _mm_subs_epi16(y, _mm_srai_epi16(g_uv, 8));
            let b = _mm_adds_epi16(y, b_term);

            let r8 = _mm_packus_epi16(r, r);
            let g8 = _mm_packus_epi16(g, g);
            let b8 = _mm_packus_epi16(b, b);

            let bg = _mm_unpacklo_epi8(b8, g8);
            let ra = _mm_unpacklo_epi8(r8, alpha);
            _mm_storeu_si128(
                out.add(col * 4) as *mut __m128i,
                _mm_unpacklo_epi16(bg, ra),
            );
            _mm_storeu_si128(
                out.add(col * 4 + 16) as *mut __m128i,
                _mm_unpackhi_epi16(bg, ra),
            );
            col += 8;
        }
        for x in col..width {
            let yv = *y_row.add(x) as i32 - 16;
            let y = yv + ((yv * (Y_GAIN - 256) + 128) >> 8);
            let u = *uv_row.add((x / 2) * 2) as i32 - 128;
            let v = *uv_row.add((x / 2) * 2 + 1) as i32 - 128;
            let r = y + v + ((v * (RV_GAIN - 256) + 128) >> 8);
            let g = y - ((GU_GAIN * u + GV_GAIN * v + 128) >> 8);
            let b = y + u + ((u * (BU_GAIN - 256) + 128) >> 8);
            let at = out.add(x * 4);
            *at = b.clamp(0, 255) as u8;
            *at.add(1) = g.clamp(0, 255) as u8;
            *at.add(2) = r.clamp(0, 255) as u8;
            *at.add(3) = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_and_scalar_agree() {
        let width = 66;
        let height = 8;
        let mut source = vec![0u8; width * height * 3 / 2];
        for (index, value) in source.iter_mut().enumerate() {
            *value = (index * 37 % 251) as u8;
        }
        let mut simd = vec![0u8; width * height * 4];
        let mut scalar = vec![0u8; width * height * 4];
        nv12_to_bgra(&mut simd, &source, width, height);
        nv12_to_bgra_scalar(&mut scalar, &source, width, height);
        assert_eq!(simd, scalar);
    }

    #[test]
    fn black_is_black_and_alpha_opaque() {
        let width = 4;
        let height = 2;
        let mut source = vec![16u8; width * height * 3 / 2];
        // BT.709 limited-range black: luma 16, chroma 128.
        for value in &mut source[width * height..] {
            *value = 128;
        }
        let mut dst = vec![0u8; width * height * 4];
        nv12_to_bgra(&mut dst, &source, width, height);
        for pixel in dst.chunks_exact(4) {
            assert!(pixel[0] <= 1 && pixel[1] <= 1 && pixel[2] <= 1);
            assert_eq!(pixel[3], 255);
        }
    }
}
