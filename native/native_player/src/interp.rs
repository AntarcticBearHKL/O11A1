//! 插帧 — the CPU linear-blend mode.
//!
//! The player keeps the last two decoded pictures and presents an extra blended
//! picture between them on a 60 Hz pacer grid. This module owns the plan (is
//! there anything to generate?) and the blend itself. The SSE2/AVX2 kernels are
//! the same fixed-point `(a*(256-w) + b*w + 128) >> 8` passes the shipped Slint
//! player measured at ~7 ms for a 1080p frame; the scalar version exists for
//! tests and non-x86 fallbacks.

use std::sync::OnceLock;

/// Modes accepted by [`crate::Player::set_interp`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpMode {
    Off = 0,
    /// Linear blend on the CPU, output capped at 60 fps.
    Cpu = 1,
}

impl InterpMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Cpu,
            _ => Self::Off,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The Chinese overlay label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Cpu => "CPU",
        }
    }

    /// Cycle 关闭 → CPU → 关闭.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Cpu,
            Self::Cpu => Self::Off,
        }
    }
}

/// The output ceiling for interpolation, regardless of the display.
pub const INTERP_MAX_OUT_HZ: u32 = 60;

/// Resolve the output rate for one session, or `None` when the source already
/// fills the display: an output grid below the track's own rate generates
/// nothing, and one only 1 % faster is not worth a full-frame pass.
pub fn interp_plan(display_hz: u32, track_fps: f64) -> Option<u32> {
    if !(track_fps.is_finite() && track_fps > 1.0) {
        return None;
    }
    let out_hz = display_hz.clamp(1, INTERP_MAX_OUT_HZ);
    if out_hz as f64 <= track_fps * 1.01 {
        return None;
    }
    Some(out_hz)
}

/// Current refresh rate of the primary display in Hz, with a 60 Hz fallback
/// for a driver that reports 0/1 ("hardware default").
pub fn display_refresh_hz() -> u32 {
    static HZ: OnceLock<u32> = OnceLock::new();
    *HZ.get_or_init(|| {
        use windows::Win32::Graphics::Gdi::{
            EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS,
        };
        unsafe {
            let mut mode = DEVMODEW::default();
            mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ok = EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode);
            if ok.as_bool() && (20..=500).contains(&mode.dmDisplayFrequency) {
                mode.dmDisplayFrequency
            } else {
                60
            }
        }
    })
}

/// Linear blend of two BGRA frames into `out`, ignoring alpha and forcing it
/// opaque. `weight` is the weight of `b` in `0..=256` (`0` = a, `256` = b).
pub fn blend_bgra_into(out: &mut [u8], a: &[u8], b: &[u8], weight: u16) {
    let len = out.len().min(a.len()).min(b.len());
    let weight = weight.min(256);
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: every pointer is valid for `len` bytes; the routine
            // handles head/tail inside that range.
            unsafe { blend_bgra_avx2(out.as_mut_ptr(), a.as_ptr(), b.as_ptr(), len, weight) };
            return;
        }
        // SAFETY: SSE2 is part of the x86-64 baseline.
        unsafe { blend_bgra_sse2(out.as_mut_ptr(), a.as_ptr(), b.as_ptr(), len, weight) };
    }
    #[cfg(not(target_arch = "x86_64"))]
    blend_bgra_scalar(out, a, b, weight);
}

/// Scalar reference; bit-identical to the SIMD paths for every length.
#[cfg(any(test, not(target_arch = "x86_64")))]
pub fn blend_bgra_scalar(out: &mut [u8], a: &[u8], b: &[u8], weight: u16) {
    let len = out.len().min(a.len()).min(b.len());
    let weight = weight.min(256) as u32;
    let inv = 256 - weight;
    for index in 0..len {
        out[index] = ((a[index] as u32 * inv + b[index] as u32 * weight + 128) >> 8) as u8;
    }
    let mut alpha = 3;
    while alpha < len {
        out[alpha] = 255;
        alpha += 4;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blend_bgra_avx2(out: *mut u8, a: *const u8, b: *const u8, len: usize, weight: u16) {
    use std::arch::x86_64::*;
    let zero = _mm256_setzero_si256();
    let w = weight as i16;
    let inv = 256 - w;
    let wv = _mm256_set1_epi16(w);
    let invv = _mm256_set1_epi16(inv);
    let round = _mm256_set1_epi16(128);
    let alpha = _mm256_set1_epi32(0xFF00_0000u32 as i32);
    let mut offset = 0usize;
    // SAFETY: each access is a full 32-byte block inside `len`.
    unsafe {
        while offset + 32 <= len {
            let av = _mm256_loadu_si256(a.add(offset) as *const __m256i);
            let bv = _mm256_loadu_si256(b.add(offset) as *const __m256i);
            let alo = _mm256_unpacklo_epi8(av, zero);
            let ahi = _mm256_unpackhi_epi8(av, zero);
            let blo = _mm256_unpacklo_epi8(bv, zero);
            let bhi = _mm256_unpackhi_epi8(bv, zero);
            let lo = _mm256_srli_epi16(
                _mm256_add_epi16(
                    _mm256_add_epi16(_mm256_mullo_epi16(alo, invv), _mm256_mullo_epi16(blo, wv)),
                    round,
                ),
                8,
            );
            let hi = _mm256_srli_epi16(
                _mm256_add_epi16(
                    _mm256_add_epi16(_mm256_mullo_epi16(ahi, invv), _mm256_mullo_epi16(bhi, wv)),
                    round,
                ),
                8,
            );
            let packed = _mm256_packus_epi16(lo, hi);
            _mm256_storeu_si256(out.add(offset) as *mut __m256i, _mm256_or_si256(packed, alpha));
            offset += 32;
        }
        let inv = 256 - weight as u32;
        let w = weight as u32;
        for index in offset..len {
            *out.add(index) = ((*a.add(index) as u32 * inv + *b.add(index) as u32 * w + 128) >> 8)
                as u8;
        }
        let mut tail = offset + 3;
        while tail < len {
            *out.add(tail) = 255;
            tail += 4;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn blend_bgra_sse2(out: *mut u8, a: *const u8, b: *const u8, len: usize, weight: u16) {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let w = weight as i16;
    let inv = 256 - w;
    let wv = _mm_set1_epi16(w);
    let invv = _mm_set1_epi16(inv);
    let round = _mm_set1_epi16(128);
    let alpha = _mm_set1_epi32(0xFF00_0000u32 as i32);
    let mut offset = 0usize;
    // SAFETY: each access is a full 16-byte block inside `len`.
    unsafe {
        while offset + 16 <= len {
            let av = _mm_loadu_si128(a.add(offset) as *const __m128i);
            let bv = _mm_loadu_si128(b.add(offset) as *const __m128i);
            let alo = _mm_unpacklo_epi8(av, zero);
            let ahi = _mm_unpackhi_epi8(av, zero);
            let blo = _mm_unpacklo_epi8(bv, zero);
            let bhi = _mm_unpackhi_epi8(bv, zero);
            let lo = _mm_srli_epi16(
                _mm_add_epi16(
                    _mm_add_epi16(_mm_mullo_epi16(alo, invv), _mm_mullo_epi16(blo, wv)),
                    round,
                ),
                8,
            );
            let hi = _mm_srli_epi16(
                _mm_add_epi16(
                    _mm_add_epi16(_mm_mullo_epi16(ahi, invv), _mm_mullo_epi16(bhi, wv)),
                    round,
                ),
                8,
            );
            let packed = _mm_packus_epi16(lo, hi);
            _mm_storeu_si128(out.add(offset) as *mut __m128i, _mm_or_si128(packed, alpha));
            offset += 16;
        }
        let inv = 256 - weight as u32;
        let w = weight as u32;
        for index in offset..len {
            *out.add(index) = ((*a.add(index) as u32 * inv + *b.add(index) as u32 * w + 128) >> 8)
                as u8;
        }
        let mut tail = offset + 3;
        while tail < len {
            *out.add(tail) = 255;
            tail += 4;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if index % 4 == 3 {
                    255
                } else {
                    (state >> 24) as u8
                }
            })
            .collect()
    }

    #[test]
    fn interp_plan_only_when_the_display_is_faster() {
        assert_eq!(interp_plan(60, 30.0), Some(60));
        assert_eq!(interp_plan(144, 30.0), Some(60));
        assert_eq!(interp_plan(120, 24.0), Some(60));
        assert_eq!(interp_plan(60, 60.0), None);
        assert_eq!(interp_plan(60, 59.94), None);
        assert_eq!(interp_plan(30, 60.0), None);
        assert_eq!(interp_plan(60, f64::NAN), None);
    }

    #[test]
    fn blend_matches_the_scalar_reference() {
        let a = sample(1920 * 4 + 13, 7);
        let b = sample(1920 * 4 + 13, 9);
        for weight in [0u16, 1, 64, 128, 200, 255, 256] {
            let mut simd = vec![0u8; a.len()];
            let mut scalar = vec![0u8; a.len()];
            blend_bgra_into(&mut simd, &a, &b, weight);
            blend_bgra_scalar(&mut scalar, &a, &b, weight);
            assert_eq!(simd, scalar, "weight {weight}");
        }
    }

    #[test]
    fn blend_endpoints_are_the_inputs_and_alpha_is_opaque() {
        let a = vec![10u8, 20, 30, 0, 40, 50, 60, 0];
        let b = vec![100u8, 110, 120, 0, 130, 140, 150, 0];
        let mut out = vec![0u8; 8];
        blend_bgra_into(&mut out, &a, &b, 0);
        assert_eq!(&out[..3], &a[..3]);
        assert_eq!(out[3], 255);
        blend_bgra_into(&mut out, &a, &b, 256);
        assert_eq!(&out[4..7], &b[4..7]);
        assert_eq!(out[7], 255);
    }
}
