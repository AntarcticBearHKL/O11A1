//! f199 QA: the scroll frame table for the three scrolling pages (首页 feed /
//! 历史 / 养鱼), measured with the f149 4ms UI-thread probe and the real
//! `PointerScrolled` replay.
//!
//! `BILI_F199_TEST=1` runs two passes back to back on the SAME session:
//!
//!   * `phase=before` sets `AppTheme.scroll-degrade = false`, i.e. exactly the
//!     pre-f199 paint path (every card clipped into its own offscreen layer);
//!   * `phase=after` restores the product default (`true`), so the card plates
//!     drop that clip while the list moves.
//!
//! Each page is scrolled three times for three seconds (within the 60 s the
//! report asks for), and the run appends one line per page/phase with the
//! pooled p50/p95/p99/max and the count above one display frame (16.7 ms) and
//! above the 33 ms spike line. Developer tool, not product code.

use crate::*;

/// `(samples, p50, p95, p99, max)` of a pooled gap set, in milliseconds.
fn percentiles(mut samples: Vec<f64>) -> (usize, f64, f64, f64, f64) {
    if samples.is_empty() {
        return (0, 0.0, 0.0, 0.0, 0.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |fraction: f64| -> f64 {
        let index = ((samples.len() as f64 * fraction) as usize).min(samples.len() - 1);
        samples[index]
    };
    (
        samples.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        *samples.last().unwrap_or(&0.0),
    )
}

fn f199_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f199 {}", line.as_ref()));
}

/// The product switch the two passes flip (`AppTheme.scroll-degrade`).
fn f199_degrade(weak: &Weak<AppWindow>, on: bool) {
    let _ = f145_on_ui(weak, move |ui| {
        let theme = ui.global::<AppTheme>();
        theme.set_scroll_degrade(on);
        theme.set_scroll_window(on);
    });
}

/// Real wheel input back to the top, so every rep starts from the same place.
fn f199_rewind(weak: &Weak<AppWindow>, x: f32, y: f32) {
    let _ = f145_on_ui(weak, move |ui| {
        for _ in 0..30 {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(x, y),
                    delta_x: 0.0,
                    delta_y: 400.0,
                });
        }
    });
    std::thread::sleep(Duration::from_millis(340));
}

/// Three reps of three seconds of real wheel input; returns the pooled gaps.
/// Each rep is also logged on its own, so a spike can be attributed to the
/// first (cold) scroll of a page rather than to the steady state.
fn f199_scroll(weak: &Weak<AppWindow>, label: &str, x: f32, y: f32, reps: usize, ms: u64) -> Vec<f64> {
    let mut pooled: Vec<f64> = Vec::new();
    // One throwaway scroll before the measured reps. The FIRST pass over a page
    // pays the cold costs that are not scrolling (glyph atlas entries for titles
    // the session had not drawn yet, the fish tank's first row-window churn);
    // the report's table is the steady state, which is what the user feels after
    // the first flick, and every rep after this one is measured.
    f149_wheel_start(weak.clone(), x, y, -12.0);
    std::thread::sleep(Duration::from_millis(2_600));
    let _ = f145_on_ui(weak, |_ui| f149_wheel_stop());
    std::thread::sleep(Duration::from_millis(400));
    for rep in 0..reps {
        f199_rewind(weak, x, y);
        let _ = f145_on_ui(weak, |_ui| f149_probe_start("f199_scroll"));
        f149_wheel_start(weak.clone(), x, y, -12.0);
        std::thread::sleep(Duration::from_millis(ms));
        let _ = f145_on_ui(weak, |_ui| f149_wheel_stop());
        let rep_gaps = f145_on_ui(weak, |_ui| f149_probe_samples()).unwrap_or_default();
        let (n, _, p95, p99, max) = percentiles(rep_gaps.clone());
        let over33 = rep_gaps.iter().filter(|gap| **gap > 33.0).count();
        f199_log(format!(
            "scroll {label} rep={rep} samples={n} p95_ms={p95:.2} p99_ms={p99:.2} max_ms={max:.2} over33ms={over33}"
        ));
        pooled.extend(rep_gaps);
        // Let the 120ms scroll tail (and its clip restore) run before the next
        // rep rewinds, so the tail is never counted as a spike.
        std::thread::sleep(Duration::from_millis(260));
    }
    pooled
}

fn f199_one(weak: &Weak<AppWindow>, page: &str, degrade: bool, x: f32, y: f32) -> Vec<f64> {
    f199_degrade(weak, degrade);
    // The window has to be SHOWN and in front: Slint skips painting an occluded
    // window, and an idle UI thread would report a perfect (and meaningless)
    // frame table. `win::restore` is the app's own show+foreground path.
    let _ = f145_on_ui(weak, |ui| {
        win::restore(&ui.window());
    });
    // The `before` pass re-creates every fish row (80 cards), which is one slow
    // frame by design; it must not land inside the measured window.
    std::thread::sleep(Duration::from_millis(700));
    let label = format!(
        "page={page} phase={}",
        if degrade { "after" } else { "before" }
    );
    let gaps = f199_scroll(weak, &label, x, y, 3, 3_000);
    let (samples, p50, p95, p99, max) = percentiles(gaps.clone());
    let over16 = gaps.iter().filter(|gap| **gap > 16.7).count();
    let over33 = gaps.iter().filter(|gap| **gap > 33.0).count();
    f199_log(format!(
        "scroll page={page} phase={} samples={samples} p50_ms={p50:.2} p95_ms={p95:.2} p99_ms={p99:.2} max_ms={max:.2} over16ms={over16} over33ms={over33}",
        if degrade { "after" } else { "before" }
    ));
    gaps
}

/// `BILI_F199_SHOT=<kind>`: put the app in the state a screenshot needs, then
/// capture it through the app's own PrintWindow path (no synthetic clicks):
///
///   * `login-drag` — force the logged-out card, then capture the FIRST frame
///     after the window drag starts (a real left-button drag from the script's
///     `SendInput`), so the PNG proves the drag is live on the login page;
///   * `fish-drag` — open 养鱼 and do the same from its margin strip;
///   * `history` — 历史 with the 清空 button armed (the confirm state), then the
///     显示条数 picker after a real switch to 50 (the trimmed list).
pub(crate) fn maybe_start_f199_shot(weak: Weak<AppWindow>) {
    let Some(kind) = env::var("BILI_F199_SHOT") else {
        return;
    };
    match kind.as_str() {
        "login-drag" | "fish-drag" => {
            let fish = kind == "fish-drag";
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(if fish { 9_000 } else { 12_000 }));
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if fish {
                        ui.invoke_fish_open();
                    } else {
                        ui.set_logged_in(false);
                        ui.set_login_state(0);
                        ui.set_login_error("".into());
                    }
                    let tag = if fish { "f199_fish_drag" } else { "f199_login_drag" };
                    let timer = Rc::new(slint::Timer::default());
                    keep_qa_timer(timer.clone());
                    let shot_ui = ui.as_weak();
                    let done = Rc::new(Cell::new(false));
                    let tick = timer.clone();
                    timer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_millis(100),
                        move || {
                            if done.get() {
                                tick.stop();
                                return;
                            }
                            // Only the frame during a REAL drag counts: the log
                            // line ("window drag start") is written before this
                            // watcher can fire, so the PNG is the drag in flight.
                            let dragging = main_drag_handle()
                                .map(|drag| drag.is_dragging())
                                .unwrap_or(false);
                            if !dragging {
                                return;
                            }
                            let Some(ui) = shot_ui.upgrade() else {
                                tick.stop();
                                return;
                            };
                            if let Some((w, h)) =
                                capture_window_png(&ui.window(), &qa_capture_path(tag))
                            {
                                done.set(true);
                                crate::debug_log(&format!(
                                    "f199 shot tag={tag} size={w}x{h} during_drag=1"
                                ));
                                tick.stop();
                            }
                        },
                    );
                });
            });
        }
        "history" => {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(9_000));
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.invoke_nav_history();
                    let timer = Rc::new(slint::Timer::default());
                    keep_qa_timer(timer.clone());
                    let shot_ui = ui.as_weak();
                    let phase = Rc::new(Cell::new(0u32));
                    let wait = Rc::new(Cell::new(0u32));
                    let tick = timer.clone();
                    timer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_millis(800),
                        move || {
                            let Some(ui) = shot_ui.upgrade() else {
                                tick.stop();
                                return;
                            };
                            let step = phase.get();
                            match step {
                                0 => {
                                    // Wait for the first page and its covers.
                                    let rows = ui.get_history_items().row_count();
                                    let waited = wait.get();
                                    if (rows == 0 || ui.get_history_loading()) && waited < 20 {
                                        wait.set(waited + 1);
                                        if ui.get_view_mode() != 5 {
                                            ui.invoke_nav_history();
                                        }
                                        return;
                                    }
                                    // The 清空 button's armed state is the exact
                                    // one the first click produces (the 4s timer
                                    // is the only other writer).
                                    ui.set_history_clear_armed(true);
                                    phase.set(1);
                                }
                                1 => {
                                    let tag = "f199_history_clear";
                                    let size =
                                        capture_window_png(&ui.window(), &qa_capture_path(tag));
                                    crate::debug_log(&format!(
                                        "f199 shot tag={tag} size={size:?} armed={} rows={}",
                                        ui.get_history_clear_armed(),
                                        ui.get_history_items().row_count()
                                    ));
                                    ui.set_history_clear_armed(false);
                                    // Through the REAL picker callback: index 0 is
                                    // the 50 pill, so the model is trimmed by the
                                    // same path a click uses.
                                    ui.invoke_history_limit_changed(0);
                                    phase.set(2);
                                }
                                2 => {
                                    let tag = "f199_history_limit";
                                    let size =
                                        capture_window_png(&ui.window(), &qa_capture_path(tag));
                                    crate::debug_log(&format!(
                                        "f199 shot tag={tag} size={size:?} limit={} rows={}",
                                        ui.get_history_limit(),
                                        ui.get_history_items().row_count()
                                    ));
                                    // Back to the shipped default.
                                    ui.invoke_history_limit_changed(1);
                                    phase.set(3);
                                }
                                _ => {
                                    crate::debug_log("f199 shot done");
                                    tick.stop();
                                }
                            }
                        },
                    );
                });
            });
        }
        _ => {}
    }
}

/// One env-driven run (`BILI_F199_TEST=1`), see the module comment.
pub(crate) fn maybe_start_f199_test(weak: Weak<AppWindow>) {
    if !env::flag("BILI_F199_TEST") {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(9_000));
        let _ = f145_on_ui(&weak, |ui| {
            ui.set_qa_no_panel(true);
            ui.set_search_open(false);
            ui.set_playlist_open(false);
            ui.window()
                .set_size(slint::LogicalSize::new(1280.0, 800.0));
            ui.window()
                .set_position(slint::LogicalPosition::new(40.0, 40.0));
        });
        // Light theme, like every other measured scroll pass.
        let _ = f145_on_ui(&weak, |ui| ui.invoke_settings_theme(1));
        std::thread::sleep(Duration::from_millis(700));

        // ---- 首页 feed ----
        let _ = f145_on_ui(&weak, |ui| {
            ui.invoke_back_to_feed();
            ui.set_view_mode(0);
            ui.set_home_tab(0);
        });
        let mut rows = 0usize;
        for _ in 0..80 {
            rows = f145_on_ui(&weak, |ui| ui.get_feed_items().row_count()).unwrap_or(0);
            if rows >= 8 {
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        f199_log(format!("feed rows={rows}"));
        f199_one(&weak, "feed", false, 640.0, 420.0);
        f199_one(&weak, "feed", true, 640.0, 420.0);

        // ---- 历史 ----
        let _ = f145_on_ui(&weak, |ui| ui.invoke_nav_history());
        let mut hist = 0usize;
        for _ in 0..60 {
            hist = f145_on_ui(&weak, |ui| ui.get_history_items().row_count()).unwrap_or(0);
            if hist > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        f199_log(format!(
            "history rows={hist} limit={}",
            f145_on_ui(&weak, |ui| ui.get_history_limit()).unwrap_or(-1)
        ));
        f199_one(&weak, "history", false, 640.0, 420.0);
        f199_one(&weak, "history", true, 640.0, 420.0);

        // ---- 养鱼 ----
        let _ = f145_on_ui(&weak, |ui| {
            ui.set_fish_qa(true);
            ui.invoke_fish_open();
        });
        // `fish_rows` (today's verdicts) can legitimately be empty on an account
        // whose tank has history only; the page is still long and scrollable, so
        // the loop waits for the ROUTE, not for cards.
        let mut cards = 0usize;
        for attempt in 0..40 {
            let mode = f145_on_ui(&weak, |ui| ui.get_view_mode()).unwrap_or(-1);
            cards = f145_on_ui(&weak, |ui| ui.get_fish_rows().row_count()).unwrap_or(0);
            if mode == 20 && (cards > 0 || attempt >= 4) {
                break;
            }
            if mode != 20 {
                let _ = f145_on_ui(&weak, |ui| ui.invoke_fish_open());
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        std::thread::sleep(Duration::from_millis(600));
        f199_log(format!(
            "fish cards={cards} hist={} days={}",
            f145_on_ui(&weak, |ui| ui.get_fish_hist_count()).unwrap_or(-1),
            f145_on_ui(&weak, |ui| ui.get_fish_hist_days()).unwrap_or(-1)
        ));
        f199_one(&weak, "fish", false, 640.0, 420.0);
        f199_one(&weak, "fish", true, 640.0, 420.0);

        // Leave the product switch on (the shipped default) and park the page.
        f199_degrade(&weak, true);
        let _ = f145_on_ui(&weak, |ui| {
            ui.set_fish_qa(false);
            ui.invoke_back_to_feed();
        });
        crate::debug_log("f199 done");
    });
}
