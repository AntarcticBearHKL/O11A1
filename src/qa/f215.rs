//! f215 QA: the wallpaper-sampled surfaces, and the scroll / morph table that
//! prices them.
//!
//! Two independent drivers, both inert unless their variable is set:
//!
//!   `BILI_F215_SHOT=<stage,stage>` captures the surfaces the report cites.
//!     Stages: `cards` (首页 feed, both themes), `toolbar` (bar + 搜索面板),
//!     `popup` (播放列表 + 账号弹层), `sidebar` (设置 页 two-pane layout),
//!     `player` (播放器 chrome; needs `BILI_OPEN_BVID`, and the capture is the
//!     player window itself).
//!
//!   `BILI_F215_PERF=scroll|morph|both` runs the f199 scroll protocol and the
//!     f159 morph protocol for each card variant, at `BILI_F215_SIZE`
//!     (default 1280x800) and in the light theme, and logs the pooled
//!     p50/p95/p99/max. Variants come from `BILI_F215_VARIANTS`
//!     (default `flat,sample`):
//!       flat   cards_flat = true  (the pre-f215 plate: tint only)
//!       sample cards_flat = false (the product default: wallpaper sample)
//!     `BILI_QA_NO_GLASS=1` additionally turns EVERY GlassPanel flat, which is
//!     the attribution run when a number moves: it separates "the cards' extra
//!     quads" from "the whole panel family".
//!
//! Both drivers are entered from the f199 hook points in `main.rs` (the same
//! two call sites f199 already owns), so no new product path exists.

use crate::*;

fn f215_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f215 {}", line.as_ref()));
}

/// `BILI_F215_SIZE=1920x1080`. Defaults to the f199 reference size.
fn f215_size() -> (f32, f32) {
    let raw = env::var("BILI_F215_SIZE").unwrap_or_else(|| "1280x800".to_string());
    let mut parts = raw.split(['x', 'X']);
    let width = parts
        .next()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| *value >= 640.0)
        .unwrap_or(1280.0);
    let height = parts
        .next()
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| *value >= 480.0)
        .unwrap_or(800.0);
    (width, height)
}

fn f215_apply_size(weak: &Weak<AppWindow>) {
    let (w, h) = f215_size();
    let _ = f145_on_ui(weak, move |ui| {
        ui.window().set_size(slint::LogicalSize::new(w, h));
        ui.window()
            .set_position(slint::LogicalPosition::new(40.0, 40.0));
    });
}

/// One capture of the main window, into `BILI_CAPTURE_DIR` (or target/qa).
fn f215_shot(weak: &Weak<AppWindow>, tag: &str) -> Option<(u32, u32)> {
    let tag = tag.to_string();
    f145_on_ui(weak, move |ui| {
        let path = qa_capture_path(&tag);
        capture_window_png(&ui.window(), &path)
    })
    .flatten()
}

fn f215_shot_player(weak: &Weak<AppWindow>, tag: &str) -> Option<(u32, u32)> {
    let tag = tag.to_string();
    f145_on_ui(weak, move |_ui| {
        let session = f145_latest_player()?;
        let path = qa_capture_path(&tag);
        capture_window_png(&session.window.window(), &path)
    })
    .flatten()
}

/// Wait (bounded) for the home feed to have rows, so a capture is never of the
/// skeleton page.
fn f215_wait_feed(weak: &Weak<AppWindow>, rows: usize, tries: usize) {
    for _ in 0..tries {
        let count = f145_on_ui(weak, |ui| ui.get_feed_items().row_count()).unwrap_or(0);
        if count >= rows {
            return;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn f215_wait_login(weak: &Weak<AppWindow>) {
    for _ in 0..750 {
        if f145_on_ui(weak, |ui| ui.get_logged_in()).unwrap_or(false) {
            return;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    f215_log("login timeout (captures will show the login page)");
}

fn f215_theme(weak: &Weak<AppWindow>, dark: bool) {
    let _ = f145_on_ui(weak, move |ui| ui.invoke_settings_theme(if dark { 2 } else { 1 }));
    std::thread::sleep(Duration::from_millis(1100));
}

fn f215_home(weak: &Weak<AppWindow>) {
    let _ = f145_on_ui(weak, |ui| {
        ui.set_search_open(false);
        ui.set_playlist_open(false);
        ui.set_account_pop(false);
        ui.invoke_back_to_feed();
        ui.set_view_mode(0);
        ui.set_home_tab(0);
        // Scroll the feed to the top: the capture must start at the first row.
        for _ in 0..24 {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(640.0, 420.0),
                    delta_x: 0.0,
                    delta_y: 400.0,
                });
        }
    });
    std::thread::sleep(Duration::from_millis(1400));
}

/// `BILI_F215_SHOT`: the surface captures. Returns true when it owns the run.
pub(crate) fn maybe_start_shot(weak: Weak<AppWindow>) -> bool {
    let Some(spec) = env::var("BILI_F215_SHOT") else {
        return false;
    };
    let spec = spec.trim().to_string();
    let all = spec.is_empty() || spec == "1" || spec.eq_ignore_ascii_case("all");
    f215_log(format!("shot start spec={spec}"));
    std::thread::spawn(move || {
        let wants = |stage: &str| all || spec.split(',').any(|part| part.trim() == stage);
        f215_wait_login(&weak);
        f215_apply_size(&weak);
        std::thread::sleep(Duration::from_millis(900));
        for dark in [false, true] {
            let theme = if dark { "dark" } else { "light" };
            f215_theme(&weak, dark);

            if wants("cards") {
                f215_home(&weak);
                f215_wait_feed(&weak, 6, 50);
                let size = f215_shot(&weak, &format!("f215_home_cards_{theme}"));
                f215_log(format!("shot cards theme={theme} size={size:?}"));
            }

            if wants("toolbar") {
                f215_home(&weak);
                // The open bar + 搜索面板: the bar is the widest surface in the
                // app and the panel is the one popup that hangs off it.
                let _ = f145_on_ui(&weak, |ui| {
                    ui.invoke_search_opened();
                    ui.set_search_open(true);
                });
                std::thread::sleep(Duration::from_millis(1200));
                let size = f215_shot(&weak, &format!("f215_toolbar_{theme}"));
                f215_log(format!("shot toolbar theme={theme} size={size:?}"));
                let _ = f145_on_ui(&weak, |ui| ui.set_search_open(false));
                std::thread::sleep(Duration::from_millis(500));
            }

            if wants("popup") {
                f215_home(&weak);
                // 播放列表: the panel that floats over the feed. `qa_hold_playlist`
                // stops the real-cursor hover state machine from closing it, so
                // the capture is deterministic headlessly; the open write is the
                // same in-out property Rust uses.
                let _ = f145_on_ui(&weak, |ui| {
                    ui.global::<AppTheme>().set_qa_hold_playlist(true);
                    ui.set_playlist_open(true);
                });
                std::thread::sleep(Duration::from_millis(600));
                let size = f215_shot(&weak, &format!("f215_popup_{theme}"));
                f215_log(format!(
                    "shot popup theme={theme} size={size:?} open={:?}",
                    f145_on_ui(&weak, |ui| ui.get_playlist_open())
                ));
                let _ = f145_on_ui(&weak, |ui| {
                    ui.set_playlist_open(false);
                    ui.global::<AppTheme>().set_qa_hold_playlist(false);
                    ui.set_account_pop(true);
                });
                std::thread::sleep(Duration::from_millis(700));
                let size = f215_shot(&weak, &format!("f215_popup_account_{theme}"));
                f215_log(format!("shot popup-account theme={theme} size={size:?}"));
                let _ = f145_on_ui(&weak, |ui| ui.set_account_pop(false));
            }

            if wants("sidebar") {
                f215_home(&weak);
                let _ = f145_on_ui(&weak, |ui| ui.invoke_open_settings());
                std::thread::sleep(Duration::from_millis(1600));
                let size = f215_shot(&weak, &format!("f215_sidebar_{theme}"));
                f215_log(format!("shot sidebar theme={theme} size={size:?}"));
                let _ = f145_on_ui(&weak, |ui| ui.invoke_back_to_feed());
                std::thread::sleep(Duration::from_millis(600));
            }

            if wants("seam") {
                // Tile-boundary proof: park the window so the 1024px seam falls
                // in the middle of the first card row (`` != 0``), then capture.
                // A wrong wrap (`-tile` without `mod`, or a doubled pixel) shows
                // as a hard vertical line there; the analyser in
                // `scripts/qa-f215-contrast.py --seam` measures it.
                let _ = f145_on_ui(&weak, |ui| {
                    ui.window()
                        .set_position(slint::LogicalPosition::new(512.0, 256.0));
                });
                std::thread::sleep(Duration::from_millis(1400));
                f215_home(&weak);
                let size = f215_shot(&weak, &format!("f215_seam_{theme}"));
                f215_log(format!(
                    "shot seam theme={theme} size={size:?} wall_x={} wall_y={}",
                    f145_on_ui(&weak, |ui| ui.global::<AppTheme>().get_wall_x() as f32)
                        .unwrap_or(-1.0),
                    f145_on_ui(&weak, |ui| ui.global::<AppTheme>().get_wall_y() as f32)
                        .unwrap_or(-1.0)
                ));
                // Second frame after a further settle: distinguishes "one stale
                // composition right after WM_MOVE" from "the sample never
                // updates". Both are captured; the seam analysis reads `seam2`.
                std::thread::sleep(Duration::from_millis(2500));
                let _ = f145_on_ui(&weak, |ui| ui.window().request_redraw());
                std::thread::sleep(Duration::from_millis(700));
                let size = f215_shot(&weak, &format!("f215_seam2_{theme}"));
                f215_log(format!("shot seam2 theme={theme} size={size:?}"));
                // Back to the reference position for the remaining stages.
                let _ = f145_on_ui(&weak, |ui| {
                    ui.window()
                        .set_position(slint::LogicalPosition::new(40.0, 40.0));
                });
                std::thread::sleep(Duration::from_millis(900));
            }

            if wants("player") {
                // The player window only exists while a video is open; the
                // 信息/评论/性能/设置 strip is opened through its own state.
                for _ in 0..40 {
                    if f145_on_ui(&weak, |_ui| f145_latest_player().is_some()).unwrap_or(false) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                let _ = f145_on_ui(&weak, |_ui| {
                    if let Some(session) = f145_latest_player() {
                        session.window.set_right_panel(1);
                        session.window.set_right_tab(0);
                        // Chrome + the 音量 / 弹幕 popovers held up (they are the
                        // player's own right-click-class menus), so the capture
                        // shows the chrome skin and a floating menu together.
                        session.window.set_qa_hold_chrome(true);
                        session.window.set_qa_hold_popovers(true);
                    }
                });
                std::thread::sleep(Duration::from_millis(1400));
                let size = f215_shot_player(&weak, &format!("f215_player_chrome_{theme}"));
                f215_log(format!("shot player theme={theme} size={size:?}"));
                let _ = f145_on_ui(&weak, |_ui| {
                    if let Some(session) = f145_latest_player() {
                        session.window.set_qa_hold_popovers(false);
                        session.window.set_qa_hold_chrome(false);
                        session.window.set_right_panel(0);
                    }
                });
                std::thread::sleep(Duration::from_millis(600));
            }
        }
        // Park on the product theme the run started in (light) and the feed.
        f215_theme(&weak, false);
        f215_home(&weak);
        f215_log("shot done");
    });
    true
}

/// One card variant for the perf table.
fn f215_variant(weak: &Weak<AppWindow>, variant: &str, stage: &str) -> bool {
    let (flat, no_glass) = match variant {
        "flat" => (true, false),
        "sample" => (false, false),
        "noglass" => (false, true),
        other => {
            f215_log(format!("perf unknown variant={other}"));
            return false;
        }
    };
    let _ = f145_on_ui(weak, move |ui| {
        ui.global::<AppTheme>().set_cards_flat(flat);
        ui.global::<AppTheme>().set_qa_no_glass(no_glass);
    });
    std::thread::sleep(Duration::from_millis(900));
    let (w, h) = f215_size();
    let label = format!("{variant} {}x{}", w as i32, h as i32);

    if stage == "scroll" || stage == "both" {
        // Product scroll path: `scroll-lite` on, cards_flat as the variant. The
        // wallpaper sample is never switched off - only the rounded clip is,
        // and that is the shipped behaviour the baseline was measured with.
        f199_degrade(weak, true);
        let _ = f145_on_ui(weak, |ui| win::restore(&ui.window()));
        std::thread::sleep(Duration::from_millis(700));
        let gaps = f199_scroll(weak, &format!("variant={variant} size={w:.0}x{h:.0}"), 640.0, 420.0, 3, 3_000);
        let (n, p50, p95, p99, max) = crate::qa::f199::percentiles(gaps.clone());
        let over33 = gaps.iter().filter(|gap| **gap > 33.0).count();
        f215_log(format!(
            "perf phase=scroll variant={variant} size={w:.0}x{h:.0} samples={n} p50_ms={p50:.2} p95_ms={p95:.2} p99_ms={p99:.2} max_ms={max:.2} over33ms={over33} cards_flat={} no_glass={}",
            u8::from(flat),
            u8::from(no_glass)
        ));
    }

    if stage == "morph" || stage == "both" {
        // The f159 protocol: 20 x (open, hold, close) on the real 180ms morph
        // clock, pooled over every rep.
        let reps: usize = env::var("BILI_F215_REPS")
            .and_then(|value| value.trim().parse().ok())
            .filter(|value: &usize| *value > 0)
            .unwrap_or(20);
        let _ = f145_on_ui(weak, |ui| {
            ui.set_search_open(false);
            ui.invoke_back_to_feed();
            ui.set_view_mode(0);
        });
        std::thread::sleep(Duration::from_millis(1200));
        let (gaps, per_rep) = f159_morph_reps(weak, reps, &label);
        let (n, p50, p95, p99, max) = f159_stats(&gaps);
        let median_rep = f159_median(&per_rep);
        f215_log(format!(
            "perf phase=morph variant={variant} size={w:.0}x{h:.0} reps={} samples={n} p50_ms={p50:.2} p95_ms={p95:.2} p99_ms={p99:.2} max_ms={max:.2} rep_p95_median_ms={median_rep:.2} cards_flat={} no_glass={}",
            per_rep.len(),
            u8::from(flat),
            u8::from(no_glass)
        ));
    }
    true
}

/// `BILI_F215_PERF`: the scroll / morph table. Returns true when it owns the run.
pub(crate) fn maybe_start_perf(weak: Weak<AppWindow>) -> bool {
    let Some(spec) = env::var("BILI_F215_PERF") else {
        return false;
    };
    let spec = spec.trim().to_ascii_lowercase();
    let stage = if spec == "morph" {
        "morph"
    } else if spec == "scroll" {
        "scroll"
    } else {
        "both"
    };
    let variants: Vec<String> = env::var("BILI_F215_VARIANTS")
        .unwrap_or_else(|| "flat,sample".to_string())
        .split(',')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    f215_log(format!(
        "perf start stage={stage} variants={variants:?} size={:?}",
        f215_size()
    ));
    std::thread::spawn(move || {
        f215_wait_login(&weak);
        f215_apply_size(&weak);
        // Light theme, like every measured scroll pass the report compares to.
        f215_theme(&weak, false);
        let _ = f145_on_ui(&weak, |ui| {
            ui.set_qa_no_panel(true);
            ui.set_search_open(false);
            ui.set_playlist_open(false);
            ui.invoke_back_to_feed();
            ui.set_view_mode(0);
            ui.set_home_tab(0);
        });
        f215_wait_feed(&weak, 8, 80);
        f215_log(format!(
            "perf feed rows={}",
            f145_on_ui(&weak, |ui| ui.get_feed_items().row_count()).unwrap_or(0)
        ));
        for variant in &variants {
            f215_variant(&weak, variant, stage);
        }
        // Product defaults back on, whatever the last variant was.
        let _ = f145_on_ui(&weak, |ui| {
            ui.global::<AppTheme>().set_cards_flat(false);
            ui.global::<AppTheme>().set_qa_no_glass(false);
        });
        f199_degrade(&weak, true);
        f215_log("perf done");
    });
    true
}
