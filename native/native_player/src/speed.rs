//! Playback speed: the 0.1×–10× range, the wheel's stop table, and the
//! `atempo` chain large factors need (one filter only accepts 0.5×–2×, so
//! 0.1× and 10× are built from several stages).

/// Slowest playback the panel can select.
pub const MIN: f32 = 0.1;
/// Fastest playback the panel can select.
pub const MAX: f32 = 10.0;

/// Horizontal distance between adjacent wheel stops at scale 1.0. The wheel
/// can show roughly nine stops at once; the rest scroll under the selection
/// window.
pub const WHEEL_GAP: f32 = 52.0;

/// Every stop on the speed wheel, slowest first. Below 1× each 0.1× is a
/// stop; above it the stops widen so fast playback stays usable. Adjacent
/// stops are always one equal wheel step apart.
pub const TICKS: [f32; 21] = [
    0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0,
    8.0, 9.0, 10.0,
];

/// Index of the stop nearest to `value`, clamped to the wheel's ends.
pub fn index_of(value: f32) -> usize {
    let value = clamp(value);
    TICKS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (*a - value)
                .abs()
                .partial_cmp(&(*b - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
        .unwrap_or(TICKS.len() - 1)
}

/// The stop at `index`, clamped to the wheel's ends.
pub fn from_index(index: i32) -> f32 {
    let last = (TICKS.len() - 1) as i32;
    TICKS[index.clamp(0, last) as usize]
}

/// Move `value` by `steps` wheel stops, clamped to the supported range.
pub fn stepped(value: f32, steps: i32) -> f32 {
    from_index(index_of(value) as i32 + steps)
}

/// Snap to the nearest wheel stop, clamped to 0.1–10.
pub fn snap(value: f32) -> f32 {
    from_index(index_of(value) as i32)
}

/// Clamp a requested speed to the supported range; non-finite values fall
/// back to normal speed.
pub fn clamp(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN, MAX)
    } else {
        1.0
    }
}

/// Readout / chip label: `0.1×`, `1×`, `1.25×`, `1.5×`, `2×`, `10×`.
pub fn label(value: f32) -> String {
    let value = clamp(value);
    let rounded = value.round();
    if (value - rounded).abs() < 0.005 {
        format!("{}×", rounded as i32)
    } else {
        let text = format!("{value:.2}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{text}×")
    }
}

/// ffmpeg audio filter for one speed, or `None` at normal speed. A single
/// `atempo` instance only accepts 0.5–2.0, so extremes are chained:
/// 0.1× → 0.5·0.5·0.5·0.8 and 10× → 2·2·2·1.25.
pub fn atempo(speed: f64) -> Option<String> {
    if !speed.is_finite() || (speed - 1.0).abs() < 0.001 {
        return None;
    }
    let mut remaining = (speed as f32).clamp(MIN, MAX) as f64;
    let mut stages = Vec::new();
    while remaining > 2.0 + 1e-9 {
        stages.push(2.0);
        remaining /= 2.0;
    }
    while remaining < 0.5 - 1e-9 {
        stages.push(0.5);
        remaining /= 0.5;
    }
    if (remaining - 1.0).abs() > 0.001 {
        stages.push(remaining);
    }
    if stages.is_empty() {
        None
    } else {
        Some(
            stages
                .iter()
                .map(|rate| format!("atempo={rate:.6}"))
                .collect::<Vec<_>>()
                .join(","),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_stops_match_the_documented_scale() {
        assert_eq!(TICKS.len(), 21);
        assert_eq!(index_of(0.1), 0);
        assert_eq!(index_of(1.0), 9);
        assert_eq!(index_of(10.0), 20);
        assert!((from_index(10) - 1.25).abs() < 1e-6);
        assert!((from_index(11) - 1.5).abs() < 1e-6);
        assert!((from_index(12) - 2.0).abs() < 1e-6);
        assert!((from_index(19) - 9.0).abs() < 1e-6);
    }

    #[test]
    fn wheel_steps_move_one_equal_stop_and_clamp() {
        assert!((stepped(1.0, 1) - 1.25).abs() < 1e-6);
        assert!((stepped(1.0, -1) - 0.9).abs() < 1e-6);
        assert!((stepped(1.5, 1) - 2.0).abs() < 1e-6);
        assert!((stepped(2.0, -1) - 1.5).abs() < 1e-6);
        assert_eq!(stepped(0.1, -40), 0.1);
        assert_eq!(stepped(10.0, 40), 10.0);
    }

    #[test]
    fn snaps_only_to_wheel_stops() {
        assert!((snap(1.03) - 1.0).abs() < 1e-6);
        assert!((snap(1.14) - 1.25).abs() < 1e-6);
        assert!((snap(1.4) - 1.5).abs() < 1e-6);
        assert!((snap(2.6) - 3.0).abs() < 1e-6);
        assert_eq!(snap(0.01), 0.1);
        assert_eq!(snap(99.0), 10.0);
    }

    #[test]
    fn labels_name_every_stop_without_trailing_zeros() {
        assert_eq!(label(0.1), "0.1×");
        assert_eq!(label(0.9), "0.9×");
        assert_eq!(label(1.0), "1×");
        assert_eq!(label(1.25), "1.25×");
        assert_eq!(label(1.5), "1.5×");
        assert_eq!(label(2.0), "2×");
        assert_eq!(label(9.0), "9×");
        assert_eq!(label(10.0), "10×");
        assert_eq!(label(f32::NAN), "1×");
    }

    #[test]
    fn atempo_chain_covers_the_whole_range() {
        assert!(atempo(1.0).is_none());
        assert_eq!(atempo(1.5).as_deref(), Some("atempo=1.500000"));
        let fast = atempo(10.0).expect("10x needs a chain");
        assert_eq!(fast.split(',').count(), 4);
        let slow = atempo(0.1).expect("0.1x needs a chain");
        assert_eq!(slow.split(',').count(), 4);
        for part in fast.split(',').chain(slow.split(',')) {
            let rate: f64 = part.trim_start_matches("atempo=").parse().unwrap();
            assert!((0.5..=2.0).contains(&rate), "atempo stage out of range: {rate}");
        }
    }
}
