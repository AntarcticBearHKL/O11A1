//! f200: the three layers' real per-second counters, in one line.
//!
//! `BILI_F200_LAYERS=<seconds>` (default 40) arms it for one player window. The
//! f174 soak prints a 5 s average from a thread it already owns; this is the
//! per-second series the f200 acceptance reads, and it is the only place that
//! can see all three layers (video, 弹幕, UI) at once without the UI thread
//! being involved at all - every number comes from an atomic counter the layer's
//! own thread bumps.
//!
//! Line shape:
//!
//! ```text
//! f200 layers t=12s video_s=117.4 dm_s=60.2 ui_s=60.0 dm_paint_ms=7.42
//!     dm_render_ms=6.10 dm_stage_ms=1.10 dm_rows=96 ui_ms=2.41 stage_us=2100
//!     pushed=120 coal=0 out_hz=120 interp_ms=1.20 p50=8.35 p95=8.60
//! ```
//!
//! `video_s` / `dm_s` / `ui_s` are presents per second in that interval (not a
//! cumulative count), so a stalled layer shows as a dip in its own column
//! instead of being averaged away over a run.
//!
//! f207 adds the comment layer's compose cost and cache to the same line:
//! `dm_compose_us_p50/dm_compose_us_p95` (µs, the strips path's own frame
//! cost), `dm_quads` (quads drawn in the last frame), `dm_strips`/`dm_atlas_mb`
//! (cached masks and atlas footprint), `dm_mask` (`hits/misses`) and
//! `dm_strips_on` (1 = the strips path is presenting, 0 = the Slint fallback).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Start the sampler once per process. No-op without `BILI_F200_LAYERS`.
pub(crate) fn f200_spawn_layers() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    let Some(raw) = crate::qa::env::var("BILI_F200_LAYERS") else {
        return;
    };
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let seconds = raw.trim().parse::<u64>().unwrap_or(40).clamp(5, 3600);
    std::thread::Builder::new()
        .name("f200-layers".into())
        .spawn(move || {
            crate::media_hooks::note_thread_role("f200-layers");
            let started = Instant::now();
            let mut last_video = 0u64;
            let mut last_dm = 0u64;
            let mut last_try = 0u64;
            let mut last_skip = 0u64;
            let mut last_ui = 0u64;
            let mut last_at = Instant::now();
            // f209 counters (see the line below).
            let mut last_upload = 0u64;
            let mut last_mf_dec = 0u64;
            let mut last_mf_tick = 0u64;
            let mut last_mf_drop = 0u64;
            let mut last_gen = 0u64;
            let mut last_coal = 0u64;
            let mut last_drop = 0u64;
            let mut logged_none = false;
            while started.elapsed().as_secs() < seconds {
                std::thread::sleep(Duration::from_millis(1000));
                let elapsed = started.elapsed().as_secs();
                let window = last_at.elapsed().as_secs_f64().max(0.001);
                last_at = Instant::now();
                let Some(hwnd) = crate::win::player_windows().first().copied() else {
                    if !logged_none {
                        logged_none = true;
                        crate::debug_log(&format!("f200 layers t={elapsed}s player=none"));
                    }
                    continue;
                };
                let video = crate::dcomp::video_counters(hwnd);
                let ui = crate::dcomp::ui_counters(hwnd);
                let dm = crate::dcomp::danmaku_stats(hwnd);
                let video_s = video
                    .map(|c| c.presented.saturating_sub(last_video) as f64 / window)
                    .unwrap_or(0.0);
                let ui_s = ui
                    .map(|(paints, _)| paints.saturating_sub(last_ui) as f64 / window)
                    .unwrap_or(0.0);
                let dm_s = dm
                    .map(|s| s.paints.saturating_sub(last_dm) as f64 / window)
                    .unwrap_or(0.0);
                let dm_try = dm
                    .map(|s| s.attempts.saturating_sub(last_try) as f64 / window)
                    .unwrap_or(0.0);
                let dm_skip = dm
                    .map(|s| s.skips.saturating_sub(last_skip) as f64 / window)
                    .unwrap_or(0.0);
                if let Some(c) = video {
                    last_video = c.presented;
                }
                if let Some((paints, _)) = ui {
                    last_ui = paints;
                }
                if let Some(s) = dm {
                    last_dm = s.paints;
                    last_try = s.attempts;
                    last_skip = s.skips;
                }
                let (out_hz, interp_ms, p50, p95, generated, dropped) =
                    match crate::player::active_control() {
                        Some(control) => {
                            let (generated, dropped, avg) = control.interp_stats();
                            let (p50, p95, _) = control.present_percentiles();
                            (
                                control.interp_out_hz(),
                                avg,
                                p50,
                                p95,
                                u64::from(generated),
                                u64::from(dropped),
                            )
                        }
                        None => (0, 0.0, 0.0, 0.0, 0, 0),
                    };
                // f209: 生成 / 上屏 / 合并, per second, in one place: the two rates
                // are the acceptance numbers and the gap between them is what the
                // sink coalesced (a `Job::Tick` is a display slot, so `coal` here
                // counts slots that were thrown away, never frames).
                let gen_s = generated.saturating_sub(last_gen) as f64 / window;
                let drop_s = dropped.saturating_sub(last_drop) as f64 / window;
                last_gen = generated;
                last_drop = dropped;
                let (stage_us, pushed, coalesced) = video
                    .map(|c| (c.stage_us, c.pushed, c.coalesced))
                    .unwrap_or((0, 0, 0));
                let coal_s = coalesced.saturating_sub(last_coal) as f64 / window;
                last_coal = coalesced;                let (warped, warp_us, gate_us) = video
                    .map(|c| (c.warped, c.warp_us, c.gate_us))
                    .unwrap_or((0, 0, 0));
                let (ui_ms, dm) = (
                    ui.map(|(_, ms)| ms).unwrap_or(0.0),
                    dm.unwrap_or_default(),
                );
                // f209: the zero-copy path's own numbers, once a second, next to
                // the presentation rate they are judged with. `upload_bytes` is
                // what the CPU moved into the GPU for the picture: the whole
                // point of this path is that it stays 0.
                let (upload_bytes, mf_dec, mf_tick, mf_drop) = video
                    .map(|c| (c.upload_bytes, c.mf_decoded, c.mf_ticks, c.mf_dropped))
                    .unwrap_or((0, 0, 0, 0));
                let (mf_upload_s, mf_dec_s, mf_tick_s, mf_drop_s) = (
                    upload_bytes.saturating_sub(last_upload) as f64 / window,
                    mf_dec.saturating_sub(last_mf_dec) as f64 / window,
                    mf_tick.saturating_sub(last_mf_tick) as f64 / window,
                    mf_drop.saturating_sub(last_mf_drop) as f64 / window,
                );
                last_upload = upload_bytes;
                last_mf_dec = mf_dec;
                last_mf_tick = mf_tick;
                last_mf_drop = mf_drop;
                crate::debug_log(&format!(
                    "f200 layers t={elapsed}s hwnd={hwnd} video_s={video_s:.1} dm_s={dm_s:.1} \
                     ui_s={ui_s:.1} dm_paint_ms={:.2} dm_render_ms={:.2} dm_stage_ms={:.2} \
                     dm_rows={} dm_compose_us_p50={} dm_compose_us_p95={} dm_quads={} \
                     dm_strips={} dm_atlas_mb={:.1} dm_mask={}/{} dm_strips_on={} \
                     dm_try={dm_try:.1} dm_skip={dm_skip:.1} \
                     ui_ms={ui_ms:.2} stage_us={stage_us} pushed={pushed} \
                     coal={coalesced} warped={warped} warp_us={warp_us} gate_us={gate_us} \
                     gen_s={gen_s:.1} drop_s={drop_s:.1} coal_s={coal_s:.1} \
                     upload_bytes={upload_bytes} upload_s={mf_upload_s:.0} \
                     mf_dec={mf_dec_s:.1} mf_tick={mf_tick_s:.1} mf_drop={mf_drop_s:.1} mf_read_us={} mf_pre_us={} \
                     out_hz={out_hz} interp_ms={interp_ms:.2} p50={p50:.2} p95={p95:.2}",
                    dm.paint_ms,
                    dm.render_ms,
                    dm.stage_ms,
                    dm.rows,
                    dm.compose_p50_us,
                    dm.compose_p95_us,
                    dm.quads,
                    dm.strips,
                    dm.strip_bytes as f64 / (1024.0 * 1024.0),
                    dm.strip_hits,
                    dm.strip_misses,
                    u8::from(dm.strips_on),
                    video.map(|c| c.mf_decode_us).unwrap_or(0),
                    video.map(|c| c.mf_pre_us).unwrap_or(0)
                ));
            }
        })
        .ok();
}
