//! f201: the rewritten 性能 panel, driven and read back for one player window.
//!
//! `BILI_F201_TEST=<seconds>[,pause]` (default 30 s, clamped 5…3600):
//!
//! 1. opens the panel on the 性能 tab through the same properties the UI's own
//!    toggle writes (`right_panel = 1`, `right_tab = 2`) and holds the chrome up
//!    so a screenshot catches the panel instead of the idle picture;
//! 2. reads the `perf_*` properties back **once a second** and logs them - the
//!    digits in `f201 panel …` are exactly what the window is showing, so a
//!    capture and its line can be compared one to one;
//! 3. `pause` pauses playback halfway through (the paused capture); `ab` swaps
//!    the 性能 body for the 信息 tab mid-run, so the same session's frame rate
//!    before and after is the panel's own cost (the window keeps its size).
//!
//! The idle/buffering capture needs no mode here: `scripts/qa-f201.ps1 -Stage
//! idle` arms the existing `BILI_STALL_TEST` hook, which suspends the decoder
//! for real, empties the ring and leaves the panel publishing 0 presents with
//! the buffering plate up - the honest "no picture" state.
//!
//! The panel itself logs its own source counters as `perf panel …` (see
//! `PerfPanel::publish`); the player logs `fps_presented` (`BILI_PROFILER_LOG`)
//! and `shown_s` (`BILI_INTERP_LOG`). Those three lines per second are what the
//! cross-check reads.

use std::time::{Duration, Instant};

/// Start the f201 panel probe for one player window. No-op without
/// `BILI_F201_TEST`, exactly like every other hook here.
pub(crate) fn f201_spawn_panel(player: slint::Weak<crate::PlayerWindow>) {
    let Some(raw) = crate::qa::env::var("BILI_F201_TEST") else {
        return;
    };
    let mut parts = raw.split(',');
    let seconds = parts
        .next()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(30)
        .clamp(5, 3600);
    let mode = parts
        .next()
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let act_at = (seconds / 2).max(2);
    let spawned = std::thread::Builder::new()
        .name("f201-panel".into())
        .spawn(move || {
            crate::media_hooks::note_thread_role("f201-panel");
            // Let the session reach its first frames: the panel's first publish
            // otherwise measures a video that has not started yet.
            std::thread::sleep(Duration::from_millis(1_500));
            let opener = player.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = opener.upgrade() else {
                    return;
                };
                window.set_right_panel(1);
                window.set_right_tab(2);
                window.set_qa_hold_chrome(true);
                crate::debug_log("f201 panel open tab=2 hold_chrome=1");
            });
            let started = Instant::now();
            for second in 1..=seconds {
                std::thread::sleep(Duration::from_millis(1_000));
                if second == act_at {
                    if mode == "pause" {
                        if let Some(control) = crate::player::active_control() {
                            control.set_paused(true);
                            crate::debug_log(&format!(
                                "f201 pause t={second}s pos_ms={}",
                                control.playback_position_ms()
                            ));
                        }
                    }
                    if mode == "ab" {
                        // Regression A/B: the window keeps its size and the
                        // panel stays open, only the 性能 body (and its 4 ms
                        // probe) goes away. Everything else - playback, the
                        // mailboxes, the danmaku lane - is untouched, so the
                        // 上屏 rate before and after this instant is the panel's
                        // own cost.
                        let window = player.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = window.upgrade() {
                                window.set_right_tab(0);
                                crate::debug_log("f201 ab panel body off tab=0");
                            }
                        });
                    }
                }
                // The panel's own strings, read off the window: this is the
                // screenshot, in text.
                let reader = player.clone();
                let elapsed = started.elapsed().as_secs();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(window) = reader.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "f201 panel t={elapsed}s tab={} gen={} shown={} coal={} gap={} \
                         decode={} stalls={} dm_s={} dm_paint={} dm_rows={} ui_gap={} \
                         ui_paint={} renderer={} rate={} peak={} conn={} buffer={} \
                         failed={} interp_on={} interp_hz={} interp_cost={} interp_reach={}",
                        window.get_right_tab(),
                        one_line(window.get_perf_gen().as_str()),
                        one_line(window.get_perf_shown().as_str()),
                        one_line(window.get_perf_coal().as_str()),
                        one_line(window.get_perf_gap().as_str()),
                        one_line(window.get_perf_decode().as_str()),
                        one_line(window.get_perf_stalls().as_str()),
                        one_line(window.get_perf_dm_rate().as_str()),
                        one_line(window.get_perf_dm_paint().as_str()),
                        one_line(window.get_perf_dm_rows().as_str()),
                        one_line(window.get_perf_ui_gap().as_str()),
                        one_line(window.get_perf_ui_paint().as_str()),
                        one_line(window.get_perf_renderer().as_str()),
                        one_line(window.get_perf_net_rate().as_str()),
                        one_line(window.get_perf_net_peak().as_str()),
                        one_line(window.get_perf_net_conn().as_str()),
                        one_line(window.get_perf_net_buffer().as_str()),
                        one_line(window.get_perf_net_failed().as_str()),
                        u8::from(window.get_perf_interp_on()),
                        one_line(window.get_perf_interp_hz().as_str()),
                        one_line(window.get_perf_interp_cost().as_str()),
                        one_line(window.get_perf_interp_reach().as_str()),
                    ));
                });
            }
        });
    if let Err(error) = spawned {
        crate::debug_log(&format!("f201 panel spawn failed err={error}"));
    }
}

/// A property read as one log column: spaces become `_` so the line stays
/// parseable, and an empty value is `-` rather than a missing field.
fn one_line(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    trimmed.replace(' ', "_")
}
