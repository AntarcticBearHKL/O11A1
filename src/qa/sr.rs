//! f202: 画质增强 — drive the tiers and read their cost back.
//!
//! `BILI_SR_QA=1` arms it for one player window. The harness exists because a
//! GPU tier's cost cannot be inferred from anything else: the numbers below are
//! the sink's own `D3D11_QUERY_TIMESTAMP` pair (µs of GPU time inside the
//! chain), the layer's presents per second and its coalesced (dropped) frames,
//! all read from the counters the video thread bumps.
//!
//! Environment:
//!
//! ```text
//! BILI_SR_QA=1              arm the harness
//! BILI_SR_SETTLE_MS=6000    let the session reach steady playback first
//! BILI_SR_SEEK_MS=120000    seek here before pausing (same frame for every tier)
//! BILI_SR_PAUSE=1           pause before the first tier (screenshot runs)
//! BILI_SR_MODES=0:8,1:8,2:8,3:8,4:8   apply each tier for N seconds
//! ```
//!
//! Line shapes (grep for `f202 sr`):
//!
//! ```text
//! f202 sr apply mode=2 label=Anime4K
//! f202 sr t=14s mode=2 label=Anime4K cost_us=1830 video_s=59.8 drop=0 presents=838
//! ```
//!
//! `cost_us` is `u64::MAX` until the first timestamp pair lands (never `0`, which
//! would read as "free"), `drop` is the coalesced-frame delta in that second and
//! `presents` is the cumulative count.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Parse `mode:seconds,mode:seconds,...`; `None` when unset or empty.
fn schedule() -> Vec<(u32, u64)> {
    let Some(raw) = crate::qa::env::var("BILI_SR_MODES") else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|item| {
            let mut parts = item.trim().split(':');
            let mode = parts.next()?.trim().parse::<u32>().ok()?;
            let seconds = parts.next().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(6);
            if crate::dcomp::enhance::MODES.contains(&mode) {
                Some((mode, seconds.clamp(1, 600)))
            } else {
                None
            }
        })
        .collect()
}

fn env_ms(name: &str, default: u64) -> u64 {
    crate::qa::env::var(name)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_flag(name: &str) -> bool {
    crate::qa::env::var(name)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Start the f202 harness. No-op without `BILI_SR_QA`, like every hook here.
pub(crate) fn maybe_start_sr_test(
    player: slint::Weak<crate::PlayerWindow>,
    control: Arc<crate::player::PlaybackControl>,
) {
    if crate::qa::env::var("BILI_SR_QA").is_none() {
        return;
    }
    let schedule = schedule();
    std::thread::Builder::new()
        .name("f202-sr".into())
        .spawn(move || {
            crate::media_hooks::note_thread_role("f202-sr");
            let settle = env_ms("BILI_SR_SETTLE_MS", 6_000);
            std::thread::sleep(Duration::from_millis(settle));
            let seek_ms = env_ms("BILI_SR_SEEK_MS", u64::MAX);
            if seek_ms != u64::MAX {
                control.seek(seek_ms);
                // A seek re-buffers; pausing too early captures the buffering
                // plate instead of the picture.
                std::thread::sleep(Duration::from_millis(3_000));
            }
            if env_flag("BILI_SR_PAUSE") && !control.paused() {
                control.toggle_paused();
                std::thread::sleep(Duration::from_millis(500));
            }
            crate::debug_log(&format!(
                "f202 sr armed paused={} position_ms={} schedule={:?}",
                control.paused(),
                control.playback_position_ms(),
                schedule
            ));

            // Apply the first tier before the sampler starts so the first second
            // of data belongs to a tier and not to the previous one.
            let started = Instant::now();
            let mut last_presented = 0u64;
            let mut last_coalesced = 0u64;
            let mut last_at = Instant::now();
            let mut applied: Option<(u32, u64, Instant)> = None;
            let mut next_index = 0usize;
            loop {
                // A tier's time slice is up: move to the next one.
                let due = match applied {
                    None => true,
                    Some((_, hold, at)) => at.elapsed() >= Duration::from_secs(hold),
                };
                if due && !schedule.is_empty() {
                    let index = next_index % schedule.len();
                    next_index = next_index.saturating_add(1);
                    let (mode, hold) = schedule[index];
                    crate::dcomp::enhance::set_desired(mode);
                    applied = Some((mode, hold, Instant::now()));
                    if let Some(window) = player.upgrade() {
                        window.set_enhance_mode(mode as i32);
                    }
                    // A paused player produces no frames, so a tier change would
                    // never repaint. `dcomp::repaint` re-runs the chain over the
                    // frame already staged - the same pixels for every tier,
                    // which is what makes the captures comparable (a seek would
                    // work too, but it re-buffers and the spinner lands in the
                    // shot).
                    if control.paused() {
                        if let Some(hwnd) = crate::win::player_windows().first().copied() {
                            crate::dcomp::repaint(hwnd);
                        }
                        std::thread::sleep(Duration::from_millis(450));
                    }
                    crate::debug_log(&format!(
                        "f202 sr apply mode={mode} label={}",
                        crate::dcomp::enhance::label(mode)
                    ));
                }
                std::thread::sleep(Duration::from_millis(1_000));
                let elapsed = started.elapsed().as_secs();
                let window_s = last_at.elapsed().as_secs_f64().max(0.001);
                last_at = Instant::now();
                let Some(hwnd) = crate::win::player_windows().first().copied() else {
                    crate::debug_log(&format!("f202 sr t={elapsed}s player=none"));
                    continue;
                };
                let Some(counters) = crate::dcomp::video_counters(hwnd) else {
                    crate::debug_log(&format!("f202 sr t={elapsed}s counters=none"));
                    continue;
                };
                let mode = crate::dcomp::enhance::desired();
                let presents_delta = counters.presented.saturating_sub(last_presented);
                let drop = counters.coalesced.saturating_sub(last_coalesced);
                last_presented = counters.presented;
                last_coalesced = counters.coalesced;
                let video_s = presents_delta as f64 / window_s;
                let cost = if counters.enhance_us == u64::MAX {
                    "pending".to_string()
                } else {
                    counters.enhance_us.to_string()
                };
                crate::debug_log(&format!(
                    "f202 sr t={elapsed}s mode={mode} sink_mode={} label={} cost_us={cost} video_s={video_s:.1} drop={drop} presents={} enhance_frames={}",
                    counters.enhance_mode,
                    crate::dcomp::enhance::label(mode),
                    counters.presented,
                    counters.enhance_frames
                ));
                if schedule.is_empty() && applied.is_none() {
                    // Nothing scheduled: just report whatever the session uses.
                    applied = Some((mode, 3600, Instant::now()));
                }
            }
        })
        .ok();
}

/// f204: open the 画质增强 menu for a capture — `BILI_F204_MENU=<delay_ms>[,<tier>]`.
///
/// The menu normally opens through the 画质增强 row's own click, but that row
/// sits below the fold of the settings strip and a capture run cannot scroll to
/// it reliably. This drives exactly the properties that click writes
/// (`right_panel`, `right_tab`) plus the window's `open_enhance_menu`, so the
/// picture captured is the real menu, not a fixture. An optional tier applies
/// one filter through the same callback the row calls (the row's own mirror is
/// updated too, as if the row had been picked).
pub(crate) fn maybe_open_enhance_menu(player: slint::Weak<crate::PlayerWindow>) {
    let Some(raw) = crate::qa::env::var("BILI_F204_MENU") else {
        return;
    };
    let mut parts = raw.split(',');
    let delay = parts
        .next()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(4_000)
        .clamp(500, 60_000);
    let tier = parts
        .next()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|mode| crate::dcomp::enhance::MODES.contains(mode));
    let spawned = std::thread::Builder::new()
        .name("f204-menu".into())
        .spawn(move || {
            crate::media_hooks::note_thread_role("f204-menu");
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(window) = player.upgrade() else {
                    return;
                };
                window.set_right_panel(1);
                window.set_right_tab(3);
                window.set_qa_hold_chrome(true);
                window.invoke_open_enhance_menu();
                if let Some(tier) = tier {
                    window.set_enhance_mode(tier as i32);
                    window.invoke_set_enhance(tier as i32);
                }
                crate::debug_log(&format!(
                    "f204 menu open tab=3 tier={tier:?} dcomp_setting={} dcomp_active={}",
                    window.get_player_dcomp(),
                    window.get_dcomp_active()
                ));
            });
        });
    if let Err(error) = spawned {
        crate::debug_log(&format!("f204 menu spawn failed err={error}"));
    }
}
