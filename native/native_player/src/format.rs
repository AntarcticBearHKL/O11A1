//! Small pure helpers shared by the engine, the overlay and the tests.
//!
//! Everything in this module is Slint-free, allocation-light and unit-tested:
//! time formatting, seek clamping and the "fit a decoded picture into a capped
//! swapchain" rule.

/// `mm:ss` below one hour, `h:mm:ss` above it. The overlay's main readout.
pub fn format_time(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `mm:ss.mmm`, used by the stats panel and the QA log.
pub fn format_time_ms(ms: u64) -> String {
    let millis = ms % 1000;
    let base = format_time(ms);
    format!("{base}.{millis:03}")
}

/// One seek release is clamped to the media duration and to the safe tail.
///
/// The last 500 ms of a stream is the one region ffmpeg's input seek and the
/// CDN range fetch are least reliable in; a target there presents nothing, so
/// every seek stops just short of it (the same rule the old player used).
pub fn clamp_seek(target_ms: u64, duration_ms: u64) -> u64 {
    if duration_ms == 0 {
        return target_ms;
    }
    target_ms.min(duration_ms.saturating_sub(500))
}

/// Fit a decoded picture into `max_w`×`max_h`, preserving its aspect ratio and
/// keeping both dimensions even (NV12 chroma is 4:2:0: an odd width or height
/// makes the rawvideo frame size ambiguous and the shader targets useless).
pub fn fit_capped(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let width = width.max(2);
    let height = height.max(2);
    let scale = (max_w as f64 / width as f64)
        .min(max_h as f64 / height as f64)
        .min(1.0)
        .max(0.000_001);
    let even = |value: f64| -> u32 {
        let floored = value.floor().max(2.0) as u32;
        (floored / 2 * 2).max(2)
    };
    let out_w = even(width as f64 * scale).min(max_w / 2 * 2).max(2);
    let out_h = even(height as f64 * scale).min(max_h / 2 * 2).max(2);
    (out_w, out_h)
}

/// Parse `12:34.5` / `83` / `1:02:03` into milliseconds (CLI/files only).
pub fn parse_time_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(seconds) = text.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| (seconds * 1000.0).round() as u64);
    }
    let mut parts = text.split(':').rev();
    let seconds: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next().unwrap_or("0").parse().ok()?;
    let hours: f64 = parts.next().unwrap_or("0").parse().ok()?;
    if !(seconds.is_finite() && minutes.is_finite() && hours.is_finite()) {
        return None;
    }
    let seconds = seconds + minutes * 60.0 + hours * 3600.0;
    (seconds >= 0.0).then(|| (seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting_crosses_the_hour() {
        assert_eq!(format_time(0), "00:00");
        assert_eq!(format_time(5_000), "00:05");
        assert_eq!(format_time(65_000), "01:05");
        assert_eq!(format_time(3_600_000), "1:00:00");
        assert_eq!(format_time(3_661_000), "1:01:01");
        assert_eq!(format_time_ms(65_123), "01:05.123");
    }

    #[test]
    fn seek_clamp_keeps_the_unseekable_tail_out() {
        assert_eq!(clamp_seek(10_000, 120_000), 10_000);
        assert_eq!(clamp_seek(119_900, 120_000), 119_500);
        assert_eq!(clamp_seek(500_000, 120_000), 119_500);
        // Unknown duration is not evidence of a tail.
        assert_eq!(clamp_seek(42_000, 0), 42_000);
        // A media shorter than the tail margin can only start at 0.
        assert_eq!(clamp_seek(300, 300), 0);
    }

    #[test]
    fn fit_capped_keeps_aspect_and_even_dimensions() {
        assert_eq!(fit_capped(1920, 1080, 1920, 1080), (1920, 1080));
        assert_eq!(fit_capped(3840, 2160, 1920, 1080), (1920, 1080));
        // Never upscale a small source.
        assert_eq!(fit_capped(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fit_capped(1080, 1920, 1920, 1080), (606, 1080));
        // Odd source sizes are rounded down, never up past the cap.
        assert_eq!(fit_capped(1919, 1079, 1920, 1080), (1918, 1078));
    }

    #[test]
    fn time_parser_accepts_clock_and_seconds() {
        assert_eq!(parse_time_ms("90"), Some(90_000));
        assert_eq!(parse_time_ms("1:30"), Some(90_000));
        assert_eq!(parse_time_ms("1:02:03.5"), Some(3_723_500));
        assert_eq!(parse_time_ms("nope"), None);
    }
}
