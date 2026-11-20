//! 插帧 — the GPU linear-blend mode.
//!
//! The player keeps the last two decoded pictures and presents an extra blended
//! picture between them on a pacer grid sized by the requested target (or the
//! display refresh). This module owns the plan (is there anything to generate?)
//! and the cheap CPU-side scene-cut signature. The blend itself runs on the
//! GPU (`gpu::present_blend` → `shaders/enhance.hlsl` `blend_main`): the two
//! decoded pictures stay in D3D11 textures, so a 4K blend never touches system
//! memory.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Modes accepted by [`crate::Player::set_interp`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpMode {
    Off = 0,
    /// Linear blend on the GPU: two decoded textures, one pixel-shader pass.
    Gpu = 1,
}

impl InterpMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Gpu,
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
            Self::Gpu => "GPU",
        }
    }

    /// Cycle 关闭 → GPU → 关闭.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Gpu,
            Self::Gpu => Self::Off,
        }
    }
}

/// The hard safety ceiling for the interpolation grid, independent of the
/// display. A requested target is never silently lowered to make playback
/// easier: the compositor and the CPU decide what is actually achieved.
pub const INTERP_MAX_OUT_HZ: u32 = 240;

/// Resolve the output rate for one session. `target_hz == 0` follows the
/// display refresh; a non-zero value is the requested fixed target (still
/// bounded by [`INTERP_MAX_OUT_HZ`]). `None` when an output grid would not add
/// anything: a grid at or below the track's own rate generates no frames, and
/// one only 1 % faster is not worth a full-frame pass.
pub fn interp_plan(display_hz: u32, track_fps: f64, target_hz: u32) -> Option<u32> {
    if !(track_fps.is_finite() && track_fps > 1.0) {
        return None;
    }
    let out_hz = if target_hz == 0 {
        display_hz.clamp(1, INTERP_MAX_OUT_HZ)
    } else {
        target_hz.clamp(1, INTERP_MAX_OUT_HZ)
    };
    if out_hz as f64 <= track_fps * 1.01 {
        return None;
    }
    Some(out_hz)
}

/// Whether one requested target can be used on this display. `0` means the
/// display refresh itself and is always allowed; fixed levels at or below the
/// panel rate are allowed, with a 1 Hz tolerance for 59.94/119.88 reports.
pub fn level_supported(hz: u32, display_hz: u32) -> bool {
    hz == 0 || hz <= display_hz.saturating_add(1)
}

/// Ordered 插帧 menu levels for one display: 关闭, 自动, then every fixed rate
/// the panel can actually pace. A 60 Hz panel has no 90/120 row.
pub fn levels(display_hz: u32) -> Vec<Option<u32>> {
    let mut levels = vec![None, Some(0)];
    for hz in [60u32, 90, 120] {
        if level_supported(hz, display_hz) {
            levels.push(Some(hz));
        }
    }
    levels
}

/// Current refresh rate of the primary display in Hz, with a 60 Hz fallback
/// for a driver that reports 0/1 ("hardware default").
///
/// `EnumDisplaySettingsW(NULL, ...)` returns false on some multi-monitor
/// drivers, which used to pin every panel to the 60 Hz fallback. Enumerate the
/// attached devices and ask the primary one by name instead.
pub fn display_refresh_hz() -> u32 {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW, DISPLAY_DEVICEW,
        DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, ENUM_CURRENT_SETTINGS,
    };

    static HZ: OnceLock<u32> = OnceLock::new();
    *HZ.get_or_init(|| unsafe {
        let mut fallback = 0u32;
        let mut index = 0u32;
        loop {
            let mut device = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                break;
            }
            index += 1;
            if (device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP).0 == 0 {
                continue;
            }
            let mut mode = DEVMODEW::default();
            mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ok = EnumDisplaySettingsW(
                PCWSTR(device.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            );
            if !ok.as_bool() || !(20..=500).contains(&mode.dmDisplayFrequency) {
                continue;
            }
            if (device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE).0 != 0 {
                return mode.dmDisplayFrequency;
            }
            if fallback == 0 {
                fallback = mode.dmDisplayFrequency;
            }
        }
        if fallback != 0 {
            fallback
        } else {
            60
        }
    })
}

/// Refresh rate of the monitor currently containing `hwnd`. The player can be
/// dragged between a 60 Hz and a 120 Hz screen, so interpolation options and
/// the danmaku cadence must follow the monitor under the window, not the
/// primary display. The monitor result is cached for two seconds; moving the
/// window to another monitor changes the key immediately.
pub fn display_refresh_hz_for_window(hwnd: windows::Win32::Foundation::HWND) -> u32 {
    use std::collections::hash_map::Entry;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS,
        MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    };

    static CACHE: OnceLock<Mutex<HashMap<isize, (u32, Instant)>>> = OnceLock::new();
    if hwnd.0.is_null() {
        return display_refresh_hz();
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return display_refresh_hz();
    }
    let key = monitor.0 as isize;
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = cache.lock() {
        if let Entry::Occupied(entry) = map.entry(key) {
            let (hz, at) = *entry.get();
            if at.elapsed() < Duration::from_secs(2) {
                return hz;
            }
        }
    }

    let mut hz = display_refresh_hz();
    unsafe {
        // MONITORINFO itself no longer carries the device name in the modern
        // bindings; the EX variant does, and GetMonitorInfoW fills it through
        // the shared MONITORINFO prefix.
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let mut mode = DEVMODEW::default();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool()
            && EnumDisplaySettingsW(
                windows::core::PCWSTR(info.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
            .as_bool()
            && (20..=500).contains(&mode.dmDisplayFrequency)
        {
            hz = mode.dmDisplayFrequency;
        }
    }
    if let Ok(mut map) = cache.lock() {
        map.insert(key, (hz, Instant::now()));
    }
    hz
}

/// Sparse luma fingerprint used for scene-cut detection. The blend shader
/// never reads system memory, so the cut detector must not scan a BGRA picture:
/// one luma sample per block center (32×18 grid) is enough to separate a hard
/// cut from fast motion, and it is the only CPU look at pixel bytes per frame.
pub const SIG_COLS: usize = 32;
pub const SIG_ROWS: usize = 18;
pub const SIG_LEN: usize = SIG_COLS * SIG_ROWS;
pub type Signature = [u8; SIG_LEN];

/// Sample the NV12 luma plane on a fixed 32×18 grid. `nv12` is the decoder's
/// packed frame (`width*height*3/2` bytes); a short buffer yields a zero
/// signature instead of reading out of bounds.
pub fn luma_signature(nv12: &[u8], width: usize, height: usize) -> Signature {
    let mut signature = [0u8; SIG_LEN];
    if width < SIG_COLS || height < SIG_ROWS || nv12.len() < width * height {
        return signature;
    }
    let y_plane = &nv12[..width * height];
    for row in 0..SIG_ROWS {
        let y = (((row * 2 + 1) * height) / (SIG_ROWS * 2)).min(height - 1);
        for col in 0..SIG_COLS {
            let x = (((col * 2 + 1) * width) / (SIG_COLS * 2)).min(width - 1);
            signature[row * SIG_COLS + col] = y_plane[y * width + x];
        }
    }
    signature
}

/// Mean absolute difference of two signatures, `0..=255`. Same scale the old
/// dense BGRA comparison reported, so `INTERP_CUT_DIFF` keeps its meaning.
pub fn signature_difference(a: &Signature, b: &Signature) -> u32 {
    let mut sum = 0u64;
    for index in 0..SIG_LEN {
        sum += (a[index] as i32 - b[index] as i32).unsigned_abs() as u64;
    }
    (sum / SIG_LEN as u64).min(255) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_levels_follow_the_panel_refresh() {
        assert_eq!(levels(60), vec![None, Some(0), Some(60)]);
        assert_eq!(
            levels(120),
            vec![None, Some(0), Some(60), Some(90), Some(120)]
        );
        // 59.94 / 119.88 reports keep their nearest fixed row.
        assert_eq!(levels(59), vec![None, Some(0), Some(60)]);
        assert_eq!(
            levels(119),
            vec![None, Some(0), Some(60), Some(90), Some(120)]
        );
    }

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

    fn frame(width: usize, height: usize, seed: u32) -> Vec<u8> {
        sample(width * height * 3 / 2, seed)
    }

    #[test]
    fn interp_plan_only_when_the_display_is_faster() {
        // 0 = follow the display refresh.
        assert_eq!(interp_plan(60, 30.0, 0), Some(60));
        assert_eq!(interp_plan(144, 30.0, 0), Some(144));
        assert_eq!(interp_plan(120, 24.0, 0), Some(120));
        // Fixed targets are honoured even above the display; the compositor
        // decides what is actually presented.
        assert_eq!(interp_plan(120, 30.0, 90), Some(90));
        assert_eq!(interp_plan(120, 30.0, 144), Some(144));
        assert_eq!(interp_plan(60, 60.0, 120), Some(120));
        assert_eq!(interp_plan(120, 30.0, 500), Some(INTERP_MAX_OUT_HZ));
        // Nothing to generate when the grid is not faster than the source.
        assert_eq!(interp_plan(60, 60.0, 0), None);
        assert_eq!(interp_plan(60, 59.94, 0), None);
        assert_eq!(interp_plan(30, 60.0, 0), None);
        assert_eq!(interp_plan(120, 60.0, 60), None);
        assert_eq!(interp_plan(60, f64::NAN, 0), None);
    }

    #[test]
    fn interp_levels_respect_display_refresh() {
        assert!(level_supported(0, 60));
        assert!(level_supported(60, 60));
        assert!(level_supported(60, 59));
        assert!(!level_supported(90, 60));
        assert!(!level_supported(120, 60));
        assert!(!level_supported(120, 90));
        assert!(level_supported(90, 90));
        assert!(level_supported(120, 119));
        assert!(level_supported(120, 120));
    }

    #[test]
    fn signature_tracks_luma_and_ignores_identical_pictures() {
        let width = 1920;
        let height = 1080;
        let a = frame(width, height, 11);
        assert_eq!(signature_difference(&luma_signature(&a, width, height), &luma_signature(&a, width, height)), 0);
        // An inverted luma plane is a hard cut on every signature sample.
        let mut cut = a.clone();
        for value in &mut cut[..width * height] {
            *value = 255 - *value;
        }
        let difference = signature_difference(
            &luma_signature(&a, width, height),
            &luma_signature(&cut, width, height),
        );
        assert!(difference > 40, "difference {difference}");
    }

    #[test]
    fn signature_stays_inside_a_short_or_tiny_frame() {
        let signature = luma_signature(&[0u8; 8], 4, 4);
        assert_eq!(signature, [0u8; SIG_LEN]);
    }
}
