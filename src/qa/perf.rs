//! Frame-budget and paint QA (f124 … f155): screenshot capture, morph timing
//! and the montage/diff helpers they share.

use crate::*;

/// Set the 背景模糊 slider through the REAL slider path: the property drives the
/// label/knob, the callback is what Rust debounces and regenerates from.
pub(crate) fn f144_set_blur(ui: &AppWindow, value: u32) {
    ui.global::<AppTheme>().set_backdrop_blur(value as i32);
    ui.invoke_settings_backdrop_blur(value as i32);
}

/// QA chain behind `BILI_F144_TEST=1`.
///
/// Drives the REAL properties/callbacks and window APIs only (no synthetic
/// input): positions the window with `set_position`, opens 设置 through
/// `open_settings`, moves the slider through `settings_backdrop_blur`, and reads
/// pixels back with `window_patch`. It proves the four f144 contracts in one run:
/// the glass shots, the settings page's zero plate alpha, the blur sweep, and
/// the parallax / 1024 px seam.
pub(crate) fn f144_step(
    ui: &AppWindow,
    step: usize,
    session_slot: &SessionSlot,
    store: &Arc<Mutex<settings::Settings>>,
    state: &Rc<F144State>,
) -> Option<usize> {
    // Plain-wallpaper probe: a 64x40 patch in the empty middle of the 70px top
    // strip (no card, no control, no animation), so a move changes only the
    // wallpaper underneath it.
    const PATCH: (i32, i32, i32, i32) = (600, 18, 64, 40);
    const BASE_X: f32 = 140.0;
    const BASE_Y: f32 = 90.0;
    match step {
        0 => {
            ui.invoke_back_to_feed();
            ui.set_settings_tab(1);
            ui.window()
                .set_size(slint::LogicalSize::new(1280.0, 800.0));
            ui.window()
                .set_position(slint::LogicalPosition::new(BASE_X, BASE_Y));
            let theme = ui.global::<AppTheme>();
            crate::debug_log(&format!(
                "f144 start blur={} transparency={} defaults blur={} transparency={}",
                theme.get_backdrop_blur(),
                theme.get_glass_transparency(),
                settings::default_backdrop_blur(),
                settings::default_glass_transparency()
            ));
        }
        1 => {
            let theme = ui.global::<AppTheme>();
            let scale = ui.window().scale_factor();
            let (rect_x, rect_y) = win::window_rect(ui.window())
                .map(|(x, y, _, _)| (x, y))
                .unwrap_or((0, 0));
            crate::debug_log(&format!(
                "f144 wall stage=home win={rect_x},{rect_y} px scale={scale:.2} wall={:.1},{:.1} tile={:.0}",
                theme.get_wall_x(),
                theme.get_wall_y(),
                theme.get_wallpaper_tile()
            ));
            if let Some(patch) = window_patch(ui.window(), PATCH.0, PATCH.1, PATCH.2, PATCH.3) {
                crate::debug_log(&format!(
                    "f144 sample stage=base wall={:.1},{:.1} rgba=[{}, {}, {}, {}]",
                    theme.get_wall_x(),
                    theme.get_wall_y(),
                    patch[0], patch[1], patch[2], patch[3]
                ));
                *state.base.borrow_mut() = Some(patch);
                state
                    .base_wall
                    .set((theme.get_wall_x(), theme.get_wall_y()));
            } else {
                crate::debug_log("f144 sample stage=base err=no-pixels");
            }
            crate::debug_log(&format!(
                "f144 capture tag=f144_glass_main ok={}",
                u8::from(capture_window_png(
                    &ui.window(),
                    &qa_capture_path("f144_glass_main")
                )
                .is_some())
            ));
            ui.invoke_open_settings();
            ui.set_settings_tab(1);
        }
        2 => {
            let theme = ui.global::<AppTheme>();
            crate::debug_log(&format!(
                "f144 settings view={} tab={} page_alpha={:.3} blur={} transparency={}",
                ui.get_view_mode(),
                ui.get_settings_tab(),
                ui.get_settings_page_alpha(),
                theme.get_backdrop_blur(),
                theme.get_glass_transparency()
            ));
            crate::debug_log(&format!(
                "f144 capture tag=f144_settings ok={}",
                u8::from(capture_window_png(
                    &ui.window(),
                    &qa_capture_path("f144_settings")
                )
                .is_some())
            ));
            ui.invoke_back_to_feed();
            f144_set_blur(ui, 0);
        }
        3 => {
            crate::debug_log(&format!(
                "f144 blur shot blur={} capture=f144_blur_0",
                ui.global::<AppTheme>().get_backdrop_blur()
            ));
            crate::debug_log(&format!(
                "f144 capture tag=f144_blur_0 ok={}",
                u8::from(capture_window_png(
                    &ui.window(),
                    &qa_capture_path("f144_blur_0")
                )
                .is_some())
            ));
            f144_set_blur(ui, 120);
        }
        4 => {
            crate::debug_log(&format!(
                "f144 blur shot blur={} capture=f144_blur_120",
                ui.global::<AppTheme>().get_backdrop_blur()
            ));
            crate::debug_log(&format!(
                "f144 capture tag=f144_blur_120 ok={}",
                u8::from(capture_window_png(
                    &ui.window(),
                    &qa_capture_path("f144_blur_120")
                )
                .is_some())
            ));
            f144_set_blur(ui, 160);
        }
        5 => {
            crate::debug_log(&format!(
                "f144 blur shot blur={} capture=f144_blur_160",
                ui.global::<AppTheme>().get_backdrop_blur()
            ));
            crate::debug_log(&format!(
                "f144 capture tag=f144_blur_160 ok={}",
                u8::from(capture_window_png(
                    &ui.window(),
                    &qa_capture_path("f144_blur_160")
                )
                .is_some())
            ));
            // Back to the default radius for the parallax probes: a softer
            // gradient makes a 200 px shift less obvious, so use 60 (the
            // shipped default) but sample a vivid area.
            f144_set_blur(ui, 60);
        }
        6 => {
            ui.window()
                .set_position(slint::LogicalPosition::new(BASE_X, BASE_Y));
        }
        7 => {
            let theme = ui.global::<AppTheme>();
            if let Some(patch) = window_patch(ui.window(), PATCH.0, PATCH.1, PATCH.2, PATCH.3) {
                *state.base.borrow_mut() = Some(patch);
                state
                    .base_wall
                    .set((theme.get_wall_x(), theme.get_wall_y()));
                crate::debug_log(&format!(
                    "f144 parallax base wall={:.1},{:.1}",
                    theme.get_wall_x(),
                    theme.get_wall_y()
                ));
            } else {
                crate::debug_log("f144 parallax base err=no-pixels");
            }
        }
        8 => {
            ui.window()
                .set_position(slint::LogicalPosition::new(BASE_X + 200.0, BASE_Y + 120.0));
        }
        9 => {
            let theme = ui.global::<AppTheme>();
            let patch = window_patch(ui.window(), PATCH.0, PATCH.1, PATCH.2, PATCH.3);
            let diff = match (state.base.borrow().as_ref(), patch.as_ref()) {
                (Some(base), Some(now)) => patch_diff(base, now),
                _ => None,
            };
            crate::debug_log(&format!(
                "f144 parallax moved=200,120 wall={:.1},{:.1} delta_wall={:.1},{:.1} max_diff={} mean_diff={:.2} changed={}",
                theme.get_wall_x(),
                theme.get_wall_y(),
                theme.get_wall_x() - state.base_wall.get().0,
                theme.get_wall_y() - state.base_wall.get().1,
                diff.map(|d| d.0.to_string()).unwrap_or_else(|| "-".into()),
                diff.map(|d| d.1).unwrap_or(0.0),
                u8::from(diff.map(|d| d.0 > 2).unwrap_or(false))
            ));
            ui.window()
                .set_position(slint::LogicalPosition::new(BASE_X + 1024.0, BASE_Y));
        }
        10 => {
            let theme = ui.global::<AppTheme>();
            let patch = window_patch(ui.window(), PATCH.0, PATCH.1, PATCH.2, PATCH.3);
            let diff = match (state.base.borrow().as_ref(), patch.as_ref()) {
                (Some(base), Some(now)) => patch_diff(base, now),
                _ => None,
            };
            crate::debug_log(&format!(
                "f144 seam moved=1024,0 wall={:.1},{:.1} max_diff={} mean_diff={:.3} match={}",
                theme.get_wall_x(),
                theme.get_wall_y(),
                diff.map(|d| d.0.to_string()).unwrap_or_else(|| "-".into()),
                diff.map(|d| d.1).unwrap_or(0.0),
                u8::from(diff.map(|d| d.0 <= 2).unwrap_or(false))
            ));
            ui.window()
                .set_position(slint::LogicalPosition::new(BASE_X, BASE_Y));
        }
        11 => {
            let Some(session) = active_session(session_slot) else {
                crate::debug_log("f144 player skip reason=no-session");
                return None;
            };
            let Some(player_state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
                crate::debug_log("f144 player skip reason=no-state");
                return None;
            };
            crate::debug_log("f144 player open=1");
            open_player_for(
                session_slot,
                &player_state,
                store,
                "BV1xx411c7mD".to_string(),
                0,
                "f144 壁纸".to_string(),
                0,
            );
            // Keep `session` alive until the window exists; the state owns it.
            drop(session);
        }
        12 => {
            let Some(player_state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
                return None;
            };
            let sessions = player_sessions(&player_state);
            let Some(session) = sessions.last() else {
                crate::debug_log("f144 player skip reason=no-window");
                return None;
            };
            let window = session.window.window();
            let before = win::window_rect(&window)
                .map(|(x, y, w, h)| format!("{x},{y} {w}x{h}"))
                .unwrap_or_else(|| "-".into());
            // Move by a delta from wherever the player was restored, so the
            // probe always exercises a real WM_MOVE (even on a rerun where the
            // saved layout already sits at the previous probe position).
            let scale = f64::from(window.scale_factor().max(0.2));
            let (x, y) = win::window_rect(&window)
                .map(|(x, y, _, _)| (f64::from(x) / scale + 120.0, f64::from(y) / scale + 80.0))
                .unwrap_or((500.0, 160.0));
            window.set_position(slint::LogicalPosition::new(x as f32, y as f32));
            crate::debug_log(&format!(
                "f144 player move before={before} want={x:.0},{y:.0}"
            ));
        }
        13 => {
            let Some(player_state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
                return None;
            };
            let sessions = player_sessions(&player_state);
            let Some(session) = sessions.last() else {
                crate::debug_log("f144 player skip reason=no-window");
                return None;
            };
            let theme = session.window.global::<AppTheme>();
            crate::debug_log(&format!(
                "f144 wall stage=player rect={} wall={:.1},{:.1} tile={:.0}",
                win::window_rect(session.window.window())
                    .map(|(x, y, w, h)| format!("{x},{y} {w}x{h}"))
                    .unwrap_or_else(|| "-".into()),
                theme.get_wall_x(),
                theme.get_wall_y(),
                theme.get_wallpaper_tile()
            ));
            crate::debug_log(&format!(
                "f144 capture tag=f144_glass_player ok={}",
                u8::from(capture_window_png(
                    session.window.window(),
                    &qa_capture_path("f144_glass_player")
                )
                .is_some())
            ));
            request_quit("f144 qa done");
            return None;
        }
        _ => return None,
    }
    Some(step + 1)
}

/// One link of the f144 chain: run a step, then schedule the next after that
/// step's settle time. Every callback runs on the UI thread; the state is an
/// `Rc` because it never leaves this thread.
pub(crate) fn f144_advance(
    ui: &AppWindow,
    step: usize,
    session_slot: &SessionSlot,
    store: &Arc<Mutex<settings::Settings>>,
    state: &Rc<F144State>,
) {
    // Settle time after step i: capture + (for slider steps) the 150 ms
    // debounce and the worker regeneration. Step 11 opens the player window, so
    // it needs the longest gap (window mapping + first paint); step 12 moves it.
    const DELAYS_MS: [u64; 13] =
        [900, 700, 1000, 1000, 1000, 800, 500, 500, 500, 400, 400, 2400, 700];
    let Some(next) = f144_step(ui, step, session_slot, store, state) else {
        return;
    };
    let delay = DELAYS_MS.get(step).copied().unwrap_or(500);
    let ui_weak = ui.as_weak();
    let slot = session_slot.clone();
    let store = store.clone();
    let state = state.clone();
    slint::Timer::single_shot(Duration::from_millis(delay), move || {
        if let Some(ui) = ui_weak.upgrade() {
            f144_advance(&ui, next, &slot, &store, &state);
        }
    });
}

pub(crate) fn f145_on_ui<T: Send + 'static>(
    weak: &Weak<AppWindow>,
    f: impl FnOnce(&AppWindow) -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let weak = weak.clone();
    slint::invoke_from_event_loop(move || {
        let value = weak.upgrade().map(|ui| f(&ui));
        let _ = tx.send(value);
    })
    .ok()?;
    rx.recv_timeout(Duration::from_secs(30)).ok().flatten()
}

/// Press at `from`, replay `steps` moves at ~125Hz to `to`, release. `counter`
/// and `value` read the REAL UI properties; the capture (when set) fires at
/// the drag's midpoint, with the button still down.
pub(crate) fn f145_schedule_drag(
    target: F145Target,
    from: (f32, f32),
    to: (f32, f32),
    steps: u32,
    value: Rc<dyn Fn() -> f32>,
    capture: Option<(String, u32)>,
    tx: Sender<Option<F145DragResult>>,
) {
    let press = |x: f32, y: f32| {
        target.with_window(|win| {
            win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x, y),
            });
            win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: slint::LogicalPosition::new(x, y),
                button: slint::platform::PointerEventButton::Left,
            });
        });
    };
    press(from.0, from.1);
    let before = value();
    let seeks0 = SEEK_REQUEST_COUNT.load(Ordering::SeqCst);
    let index = Rc::new(Cell::new(0u32));
    let moves = Rc::new(Cell::new(0u32));
    // "Value updates" = a real change of the property the knob/fill/readout
    // follows, sampled after every dispatched move. This is the honest pair
    // with `moves`: a handler that only writes on click/release scores 0.
    let changes = Rc::new(Cell::new(0u32));
    let last = Rc::new(Cell::new(before));
    let timer = Rc::new(slint::Timer::default());
    let timer_a = timer.clone();
    keep_qa_timer(timer.clone());
    let target_a = target.clone();
    let index_a = index.clone();
    let moves_a = moves.clone();
    let changes_a = changes.clone();
    let last_a = last.clone();
    let value_a = value.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(8),
        move || {
            let n = index_a.get();
            if n < steps {
                let t = (n + 1) as f32 / steps as f32;
                let x = from.0 + (to.0 - from.0) * t;
                let y = from.1 + (to.1 - from.1) * t;
                target_a.with_window(|win| {
                    win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(x, y),
                    });
                });
                moves_a.set(n + 1);
                let now = value_a();
                if (now - last_a.get()).abs() > 0.0001 {
                    changes_a.set(changes_a.get() + 1);
                    last_a.set(now);
                }
                if let Some((tag, at)) = capture.as_ref() {
                    if n + 1 == *at {
                        target_a.with_window(|win| {
                            let _ = capture_window_png(win, &qa_capture_path(tag));
                        });
                        crate::debug_log(&format!(
                            "f145 drag capture tag={tag} at_move={at}"
                        ));
                    }
                }
                index_a.set(n + 1);
                return;
            }
            // Release at the final point, exactly like a real gesture.
            target_a.with_window(|win| {
                win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(to.0, to.1),
                });
                win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(to.0, to.1),
                    button: slint::platform::PointerEventButton::Left,
                });
            });
            let result = F145DragResult {
                moves: moves_a.get(),
                updates: changes_a.get() as i32,
                before,
                after: value_a(),
                seek_before: seeks0,
                seek_after: SEEK_REQUEST_COUNT.load(Ordering::SeqCst),
            };
            let _ = tx.send(Some(result));
            timer_a.stop();
        },
    );
}

/// Schedule one player drag on the UI thread. The band rects and counters are
/// read from the live PlayerWindow, after the panel has grown/shrunk.
pub(crate) fn f145_schedule_player_drag(
    weak: &Weak<AppWindow>,
    kind: F145DragKind,
    capture: Option<(String, u32)>,
    tx: Sender<Option<F145DragResult>>,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            let _ = tx.send(None);
            return;
        };
        let _ = &ui;
        let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
            let _ = tx.send(None);
            return;
        };
        let Some(session) = player_sessions(&state).last().cloned() else {
            let _ = tx.send(None);
            return;
        };
        let player = &session.window;
        let (from, to) = match kind {
            F145DragKind::Seek => {
                let x = player.get_qa_seek_x();
                let y = player.get_qa_seek_y();
                let w = player.get_qa_seek_w();
                let h = player.get_qa_seek_hit_h();
                ((x + w * 0.10, y + h / 2.0), (x + w * 0.56, y + h / 2.0))
            }
            F145DragKind::Volume => {
                let x = player.get_qa_vol_x();
                let y = player.get_qa_vol_y();
                let w = player.get_qa_vol_w();
                let h = player.get_qa_vol_hit_h();
                (
                    (x + w / 2.0, y + h * 0.85),
                    (x + w / 2.0, y + h * 0.15),
                )
            }
            _ => {
                let _ = tx.send(None);
                return;
            }
        };
        crate::debug_log(&format!(
            "f145 drag start kind={} from={:.1},{:.1} to={:.1},{:.1} window={}x{} vol_shown={}",
            kind.tag(),
            from.0,
            from.1,
            to.0,
            to.1,
            player.window().size().width,
            player.window().size().height,
            u8::from(player.get_qa_vol_shown())
        ));
        let value: Rc<dyn Fn() -> f32> = match kind {
            F145DragKind::Seek => {
                let session = session.clone();
                Rc::new(move || session.window.get_drag_ms())
            }
            F145DragKind::Volume => {
                let session = session.clone();
                Rc::new(move || session.window.get_volume())
            }
            _ => Rc::new(|| 0.0),
        };
        f145_schedule_drag(
            F145Target::Player(session),
            from,
            to,
            240,
            value,
            capture,
            tx,
        );
    });
}

pub(crate) fn f145_schedule_app_drag(
    weak: &Weak<AppWindow>,
    kind: F145DragKind,
    tx: Sender<Option<F145DragResult>>,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            let _ = tx.send(None);
            return;
        };
        let (from, to) = match kind {
            F145DragKind::Blur => {
                let x = ui.get_qa_blur_x();
                let y = ui.get_qa_blur_y();
                let w = ui.get_qa_blur_w();
                let h = ui.get_qa_blur_band_h();
                ((x + w * 0.10, y + h / 2.0), (x + w * 0.90, y + h / 2.0))
            }
            F145DragKind::Opacity => {
                let x = ui.get_qa_opacity_x();
                let y = ui.get_qa_opacity_y();
                let w = ui.get_qa_opacity_w();
                let h = ui.get_qa_opacity_band_h();
                ((x + w * 0.10, y + h / 2.0), (x + w * 0.90, y + h / 2.0))
            }
            _ => {
                let _ = tx.send(None);
                return;
            }
        };
        crate::debug_log(&format!(
            "f145 drag start kind={} from={:.1},{:.1} to={:.1},{:.1} window={}x{}",
            kind.tag(),
            from.0,
            from.1,
            to.0,
            to.1,
            ui.window().size().width,
            ui.window().size().height
        ));
        let value: Rc<dyn Fn() -> f32> = match kind {
            F145DragKind::Blur => {
                // The knob follows the float fraction, not the 160-step int.
                let weak = ui.as_weak();
                Rc::new(move || {
                    weak.upgrade()
                        .map(|ui| ui.get_set_blur_frac() * 160.0)
                        .unwrap_or(0.0)
                })
            }
            F145DragKind::Opacity => {
                let weak = ui.as_weak();
                Rc::new(move || {
                    weak.upgrade()
                        .map(|ui| ui.global::<AppTheme>().get_glass_transparency())
                        .unwrap_or(0.0)
                })
            }
            _ => Rc::new(|| 0.0),
        };
        f145_schedule_drag(
            F145Target::App(ui.as_weak()),
            from,
            to,
            240,
            value,
            None,
            tx,
        );
    });
}

pub(crate) fn f145_run_drag(
    weak: &Weak<AppWindow>,
    kind: F145DragKind,
    capture: Option<(String, u32)>,
) -> Option<F145DragResult> {
    let (tx, rx) = std::sync::mpsc::channel();
    match kind {
        F145DragKind::Seek | F145DragKind::Volume => {
            f145_schedule_player_drag(weak, kind, capture, tx);
        }
        _ => f145_schedule_app_drag(weak, kind, tx),
    }
    rx.recv_timeout(Duration::from_secs(45)).ok().flatten()
}

pub(crate) fn f145_pointer(win: &slint::Window, x: f32, y: f32, click: bool) {
    let position = slint::LogicalPosition::new(x, y);
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    if click {
        win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    }
}

pub(crate) fn f145_wheel(win: &slint::Window, x: f32, y: f32, count: usize, delta: f32) {
    for _ in 0..count {
        win.dispatch_event(slint::platform::WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(x, y),
            delta_x: 0.0,
            delta_y: delta,
        });
    }
}

pub(crate) fn f145_latest_player() -> Option<Rc<PlayerSession>> {
    let state = WALLPAPER_STATE.with(|slot| slot.borrow().clone())?;
    player_sessions(&state).last().cloned()
}

pub(crate) fn f145_players() -> usize {
    WALLPAPER_STATE
        .with(|slot| slot.borrow().clone())
        .map(|state| player_sessions(&state).len())
        .unwrap_or(0)
}

/// The count read from the background thread: the thread-local `PlayerState`
/// only exists on the UI thread, so every read goes through the event loop.
pub(crate) fn f145_players_ui(weak: &Weak<AppWindow>) -> usize {
    f145_on_ui(weak, |_ui| f145_players()).unwrap_or(0)
}

pub(crate) fn f145_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f145 {}", line.as_ref()));
}

pub(crate) fn f146_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f146 {}", line.as_ref()));
}

/// The main window's wallpaper patch: an empty band of the top strip, above the
/// feed, with no card or control in it.
pub(crate) const F146_LUMA_PATCH: (i32, i32, i32, i32) = (560, 8, 160, 40);

pub(crate) fn f146_luma(patch: &[u8]) -> f64 {
    let mut sum = 0.0;
    let mut samples = 0.0;
    for px in patch.chunks_exact(4) {
        sum += 0.2126 * f64::from(px[0]) + 0.7152 * f64::from(px[1]) + 0.0722 * f64::from(px[2]);
        samples += 1.0;
    }
    if samples > 0.0 {
        sum / samples
    } else {
        0.0
    }
}

pub(crate) fn f146_patch_luma(weak: &Weak<AppWindow>) -> Option<f64> {
    f145_on_ui(weak, |ui| {
        window_patch(
            ui.window(),
            F146_LUMA_PATCH.0,
            F146_LUMA_PATCH.1,
            F146_LUMA_PATCH.2,
            F146_LUMA_PATCH.3,
        )
        .map(|patch| f146_luma(&patch))
    })
    .flatten()
}

pub(crate) fn f146_capture(weak: &Weak<AppWindow>, tag: &str) {
    let tag_owned = tag.to_string();
    let _ = f145_on_ui(weak, move |ui| {
        let path = qa_capture_path(&tag_owned);
        let size = capture_window_png(&ui.window(), &path);
        f146_log(format!("capture tag={tag_owned} size={size:?}"));
    });
}

/// Queue rows as the UI model shows them: titles, the current index and the
/// per-row `current` flag. UI thread only.
pub(crate) fn f146_queue() -> (Vec<String>, Option<usize>, Vec<bool>, usize) {
    let Some(host) = playlist() else {
        return (Vec::new(), None, Vec::new(), 0);
    };
    let (items, current, _mode) = host.snapshot();
    let flags = host
        .ui
        .upgrade()
        .map(|ui| {
            let rows = ui.get_playlist();
            (0..rows.row_count())
                .filter_map(|index| rows.row_data(index).map(|row| row.current))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let focused = host
        .state
        .upgrade()
        .map(|state| player_sessions(&state).len())
        .unwrap_or(0);
    (items, current, flags, focused)
}

pub(crate) fn f146_close_bvid(bvid: &str) -> bool {
    let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
        return false;
    };
    let Some(session) = player_sessions(&state)
        .into_iter()
        .find(|session| session.video.bvid == bvid)
    else {
        return false;
    };
    crate::debug_log(&format!(
        "f146 close bvid={bvid} win={}",
        session.id
    ));
    session.window.invoke_player_closed();
    true
}

/// QA: give `bvid`'s player window the focus the pointer would give it.
/// A programmatic `card_open(..., 1)` creates and shows the window without
/// Windows activation, so without this the queue would keep following the old
/// window and the pause/close checks would exercise the wrong session.
pub(crate) fn f146_focus_bvid(bvid: &str) -> bool {
    let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
        return false;
    };
    let Some(session) = player_sessions(&state)
        .into_iter()
        .find(|session| session.video.bvid == bvid)
    else {
        return false;
    };
    focus_player(&state, &session, true);
    crate::debug_log(&format!(
        "f146 focus bvid={bvid} win={} via=qa",
        session.id
    ));
    true
}

pub(crate) fn f146_drag_step(weak: &Weak<AppWindow>, step: i32) -> Option<bool> {
    f145_on_ui(weak, move |ui| {
        let drag = main_drag_handle()?;
        let ((ax, ay), _) = drag.anchor();
        drag.move_to_cursor((ax + step * 10, ay + step * 4));
        Some(drag.flush(ui.window()))
    })
    .flatten()
}

pub(crate) fn f147_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("{} {}", qa_wave(), line.as_ref()));
}

pub(crate) fn f147_capture_main(weak: &Weak<AppWindow>, tag: &str) {
    let tag_owned = qa_wave_tag(tag);
    let _ = f145_on_ui(weak, move |ui| {
        let path = qa_capture_path(&tag_owned);
        let size = capture_window_png(&ui.window(), &path);
        f147_log(format!("capture tag={tag_owned} size={size:?}"));
    });
}

pub(crate) fn f147_capture_player(weak: &Weak<AppWindow>, tag: &str) {
    let tag_owned = qa_wave_tag(tag);
    let _ = f145_on_ui(weak, move |_ui| {
        let Some(session) = f145_latest_player() else {
            f147_log(format!("capture tag={tag_owned} size=None reason=no-player"));
            return;
        };
        let path = qa_capture_path(&tag_owned);
        let size = capture_window_png(&session.window.window(), &path);
        f147_log(format!("capture tag={tag_owned} size={size:?}"));
    });
}

pub(crate) fn f147_probe_start(weak: Weak<AppWindow>) {
    f147_probe_stop();
    F147_GAPS.with(|gaps| gaps.borrow_mut().clear());
    F147_EVENTS.with(|events| events.borrow_mut().clear());
    F147_LAST.with(|last| last.set(Some(Instant::now())));
    let timer = Rc::new(slint::Timer::default());
    let started = Instant::now();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(4),
        move || {
            let now = Instant::now();
            let mut long_gap = None;
            F147_LAST.with(|last| {
                if let Some(previous) = last.get() {
                    let gap = now.duration_since(previous).as_secs_f64() * 1000.0;
                    F147_GAPS.with(|gaps| gaps.borrow_mut().push(gap));
                    if gap > 16.0 {
                        long_gap = Some((gap, started.elapsed().as_secs_f64() * 1000.0));
                    }
                }
                last.set(Some(now));
            });
            if let (Some((gap, at_ms)), Some(ui)) = (long_gap, weak.upgrade()) {
                let line = format!(
                    "morph gap_ms={gap:.2} at_ms={at_ms:.1} search_open={} bar_w={:.1} field_w={:.1} refreshing={} skeleton={}",
                    u8::from(ui.get_search_open()),
                    ui.get_home_bar_w(),
                    ui.get_qa_search_field_w(),
                    u8::from(ui.get_list_refreshing()),
                    u8::from(ui.global::<AppTheme>().get_skeletons_on())
                );
                F147_EVENTS.with(|events| events.borrow_mut().push(line));
            }
        },
    );
    F147_PROBE.with(|slot| {
        *slot.borrow_mut() = Some(F147Probe { timer });
    });
}

/// `(samples, p50, p95, max, over_16ms)` in milliseconds. UI thread only.
pub(crate) fn f147_probe_stop() -> (usize, f64, f64, f64, usize) {
    if let Some(probe) = F147_PROBE.with(|slot| slot.borrow_mut().take()) {
        probe.timer.stop();
    }
    let events = F147_EVENTS.with(|events| events.borrow_mut().split_off(0));
    let mut gaps = F147_GAPS.with(|gaps| gaps.borrow().clone());
    let stats = if gaps.is_empty() {
        (0, 0.0, 0.0, 0.0, 0)
    } else {
        let over = gaps.iter().filter(|gap| **gap > 16.0).count();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |fraction: f64| -> f64 {
            let index = ((gaps.len() as f64 * fraction) as usize).min(gaps.len() - 1);
            gaps[index]
        };
        (gaps.len(), at(0.50), at(0.95), *gaps.last().unwrap_or(&0.0), over)
    };
    // Flush outside the measured window. This runs after the timer stopped, so
    // the disk latencies cannot contaminate the sample.
    for event in events {
        f147_log(event);
    }
    stats
}

/// The vinyl contract as the main window sees it: `(playing,title,cover_w,
/// queue_len,current)`.
pub(crate) fn f147_vinyl(ui: &AppWindow) -> (bool, String, u32, usize, Option<usize>) {
    let rows = ui.get_playlist().row_count();
    let current = playlist().and_then(|host| host.current());
    (
        ui.get_now_playing(),
        ui.get_now_title().to_string(),
        ui.get_now_cover().size().width,
        rows,
        current,
    )
}

/// `(preload, done)` per playlist row, in model order. UI thread only.
pub(crate) fn f147_rings(ui: &AppWindow) -> Vec<(f32, bool)> {
    let rows = ui.get_playlist();
    (0..rows.row_count())
        .filter_map(|index| rows.row_data(index).map(|row| (row.preload, row.preload_done)))
        .collect()
}

pub(crate) fn f147_feed_index(ui: &AppWindow, bvid: &str) -> Option<usize> {
    let rows = ui.get_feed_items();
    (0..rows.row_count()).find(|index| {
        rows.row_data(*index)
            .map(|row| row.bvid.as_str() == bvid)
            .unwrap_or(false)
    })
}

/// Parse a card's `MM:SS` / `HH:MM:SS` badge into seconds.
pub(crate) fn f147_badge_secs(badge: &str) -> Option<u64> {
    let mut secs = 0u64;
    let mut parts = 0u32;
    for piece in badge.split(':') {
        let value: u64 = piece.trim().parse().ok()?;
        secs = secs * 60 + value;
        parts += 1;
    }
    (parts >= 2).then_some(secs)
}

/// The shortest feed row in `min_secs..=max_secs`, so the vinyl/end-of-track
/// leg survives the ring/preload wait but still reaches a real end in seconds
/// instead of waiting out a 17-minute feature. Falls back to the shortest row
/// under `max_secs` when the range is empty.
pub(crate) fn f147_short_feed_index(
    ui: &AppWindow,
    min_secs: u64,
    max_secs: u64,
) -> Option<(usize, String, u64)> {
    let rows = ui.get_feed_items();
    let mut best: Option<(usize, String, u64)> = None;
    let mut fallback: Option<(usize, String, u64)> = None;
    for index in 0..rows.row_count().min(24) {
        let Some(row) = rows.row_data(index) else {
            continue;
        };
        let Some(secs) = f147_badge_secs(row.badge.as_str()) else {
            continue;
        };
        if secs == 0 || secs > max_secs {
            continue;
        }
        if fallback
            .as_ref()
            .map(|(_, _, fallback_secs)| secs < *fallback_secs)
            .unwrap_or(true)
        {
            fallback = Some((index, row.badge.to_string(), secs));
        }
        if secs < min_secs {
            continue;
        }
        if best
            .as_ref()
            .map(|(_, _, best_secs)| secs < *best_secs)
            .unwrap_or(true)
        {
            best = Some((index, row.badge.to_string(), secs));
        }
    }
    best.or(fallback)
}

/// Up to `count` feed rows in `min_secs..=max_secs`, shortest first, excluding
/// `exclude`. Used for the two queued ring rows: short files make the
/// low-priority preload's arc visible inside the capture window.
pub(crate) fn f147_short_feed_indices(
    ui: &AppWindow,
    min_secs: u64,
    max_secs: u64,
    exclude: usize,
    count: usize,
) -> Vec<usize> {
    let rows = ui.get_feed_items();
    let mut candidates: Vec<(usize, u64)> = Vec::new();
    for index in 0..rows.row_count().min(24) {
        if index == exclude {
            continue;
        }
        let Some(row) = rows.row_data(index) else {
            continue;
        };
        let Some(secs) = f147_badge_secs(row.badge.as_str()) else {
            continue;
        };
        if secs >= min_secs && secs <= max_secs {
            candidates.push((index, secs));
        }
    }
    candidates.sort_by_key(|(_, secs)| *secs);
    let mut picked: Vec<usize> = candidates.into_iter().map(|(index, _)| index).take(count).collect();
    // Fallback: adjacent rows, so a sparse/SF feed still gets two queued rows.
    let total = rows.row_count().max(1);
    let mut offset = 1usize;
    while picked.len() < count && offset < total {
        let index = (exclude + offset) % total;
        if !picked.contains(&index) {
            picked.push(index);
        }
        offset += 1;
    }
    picked
}

/// The cid for a feed bvid, used by QA to drop that video's session cache
/// before the cold-start scrim capture.
pub(crate) fn f147_feed_cid(bvid: &str) -> Option<u64> {
    let host = playlist()?;
    let feed = host.feed.lock().ok()?;
    feed.iter().find(|video| video.bvid == bvid).map(|video| video.cid)
}

/// Close every open player window through its own close callback. A busy
/// desktop can leave a player open before the QA chain starts (an accidental
/// click); a player suppresses cover streaming, so a clean search capture needs
/// zero players. Returns the number of windows asked to close.
pub(crate) fn f147_close_players() -> usize {
    let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
        return 0;
    };
    let sessions = player_sessions(&state);
    let count = sessions.len();
    for session in sessions {
        session.window.invoke_player_closed();
    }
    count
}

/// Wheel events dispatched by the replay. Atomic because the test thread reads
/// it after stopping the UI-thread timer.
pub(crate) static F149_WHEELS: AtomicU32 = AtomicU32::new(0);

pub(crate) fn f149_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f149 {}", line.as_ref()));
}

/// A 4ms UI-thread clock. Each sample is the delay between two timer callbacks
/// on the thread that paints: a gap above a display frame is time no frame
/// could be submitted. Same probe as f147, plus p99 and the >8.5ms count.
pub(crate) fn f149_probe_start(phase: &'static str) {
    if let Some(timer) = F149_PROBE.with(|slot| slot.borrow_mut().take()) {
        timer.stop();
    }
    F149_GAPS.with(|gaps| gaps.borrow_mut().clear());
    F149_EVENTS.with(|events| events.borrow_mut().clear());
    F149_PHASE.with(|slot| *slot.borrow_mut() = phase.to_string());
    F149_PROBE_LAST.with(|last| last.set(Some(Instant::now())));
    let started = Instant::now();
    let timer = Rc::new(slint::Timer::default());
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(4),
        move || {
            let now = Instant::now();
            F149_PROBE_LAST.with(|last| {
                if let Some(previous) = last.get() {
                    let gap = now.duration_since(previous).as_secs_f64() * 1000.0;
                    F149_GAPS.with(|gaps| gaps.borrow_mut().push(gap));
                    if gap > 10.0 {
                        F149_EVENTS.with(|events| {
                            events.borrow_mut().push(format!(
                                "at_ms={:.1} gap_ms={gap:.2}",
                                started.elapsed().as_secs_f64() * 1000.0
                            ));
                        });
                    }
                }
                last.set(Some(now));
            });
        },
    );
    F149_PROBE.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// `(samples,p50,p95,p99,max,over16,over8_5)` in milliseconds.
pub(crate) fn f149_probe_stop() -> (usize, f64, f64, f64, f64, usize, usize) {
    if let Some(timer) = F149_PROBE.with(|slot| slot.borrow_mut().take()) {
        timer.stop();
    }
    let phase = F149_PHASE.with(|slot| slot.borrow().clone());
    let events = F149_EVENTS.with(|events| events.borrow_mut().split_off(0));
    let mut gaps = F149_GAPS.with(|gaps| gaps.borrow().clone());
    if gaps.is_empty() {
        return (0, 0.0, 0.0, 0.0, 0.0, 0, 0);
    }
    let over16 = gaps.iter().filter(|gap| **gap > 16.0).count();
    let over85 = gaps.iter().filter(|gap| **gap > 8.5).count();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |fraction: f64| -> f64 {
        let index = ((gaps.len() as f64 * fraction) as usize).min(gaps.len() - 1);
        gaps[index]
    };
    // Flush after the measurement stopped: disk I/O never lands in a sample.
    for event in events {
        f149_log(format!("gap_event phase={phase} {event}"));
    }
    (
        gaps.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        *gaps.last().unwrap_or(&0.0),
        over16,
        over85,
    )
}

pub(crate) fn f149_wheel_start(weak: Weak<AppWindow>, x: f32, y: f32, delta_y: f32) {
    f149_wheel_stop();
    let _ = f145_on_ui(&weak, move |ui| {
        let ui_weak = ui.as_weak();
        let timer = Rc::new(slint::Timer::default());
        keep_qa_timer(timer.clone());
        F149_WHEELS.store(0, Ordering::SeqCst);
        F149_WHEEL_TIMER.with(|slot| *slot.borrow_mut() = Some(timer.clone()));
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_micros(8_333),
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.window().dispatch_event(
                        slint::platform::WindowEvent::PointerScrolled {
                            position: slint::LogicalPosition::new(x, y),
                            delta_x: 0.0,
                            delta_y,
                        },
                    );
                    F149_WHEELS.fetch_add(1, Ordering::SeqCst);
                }
            },
        );
    });
}

pub(crate) fn f149_wheel_stop() {
    if let Some(timer) = F149_WHEEL_TIMER.with(|slot| slot.borrow_mut().take()) {
        timer.stop();
    }
}

pub(crate) fn f149_capture_main(weak: &Weak<AppWindow>, tag: &str) {
    let tag = tag.to_string();
    let _ = f145_on_ui(weak, move |ui| {
        let path = qa_capture_path(&tag);
        let size = capture_window_png(&ui.window(), &path);
        f149_log(format!("capture tag={tag} size={size:?}"));
    });
}

pub(crate) fn f149_patch_mean(patch: &[u8]) -> (f64, f64, f64) {
    if patch.len() < 4 {
        return (0.0, 0.0, 0.0);
    }
    let pixels = patch.len() / 4;
    let mut sum = [0u64; 3];
    for pixel in patch.chunks_exact(4) {
        sum[0] += pixel[0] as u64;
        sum[1] += pixel[1] as u64;
        sum[2] += pixel[2] as u64;
    }
    (
        sum[0] as f64 / pixels as f64,
        sum[1] as f64 / pixels as f64,
        sum[2] as f64 / pixels as f64,
    )
}

/// Two PNGs side by side with an 8px gutter, for the two-window evidence.
pub(crate) fn f149_montage_side(
    paths: &[PathBuf],
    out: &std::path::Path,
) -> Option<(u32, u32)> {
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        images.push(image::open(path).ok()?.to_rgba8());
    }
    if images.is_empty() {
        return None;
    }
    let gap = 8u32;
    let height = images.iter().map(|image| image.height()).max().unwrap_or(0);
    let width: u32 = images.iter().map(|image| image.width()).sum::<u32>()
        + gap * (images.len() as u32 - 1);
    let mut canvas = image::RgbaImage::new(width, height);
    let mut x = 0u32;
    for image in &images {
        image::imageops::overlay(&mut canvas, image, i64::from(x), 0);
        x += image.width() + gap;
    }
    match canvas.save(out) {
        Ok(()) => {
            f149_log(format!(
                "montage path={} size={}x{}",
                out.display(),
                width,
                height
            ));
            Some((width, height))
        }
        Err(err) => {
            f149_log(format!("montage path={} err={err}", out.display()));
            None
        }
    }
}


pub(crate) fn f150_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f150 {}", line.as_ref()));
}

pub(crate) fn f150_write_frame(frame: &F150Frame, tag: &str) -> Option<PathBuf> {
    let path = qa_capture_path(tag);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let image = image::RgbaImage::from_raw(frame.w, frame.h, frame.rgba.clone())?;
    image.save(&path).ok()?;
    Some(path)
}

/// Max per-channel difference, mean difference and changed-pixel count over
/// every pixel outside `exclude` (physical px). `None` when the sizes differ.
pub(crate) fn f150_diff_outside(
    a: &F150Frame,
    b: &F150Frame,
    exclude: &[(i32, i32, i32, i32)],
) -> Option<(u32, f64, usize)> {
    if a.w != b.w || a.h != b.h {
        return None;
    }
    let (w, h) = (a.w as i32, a.h as i32);
    let mut max_diff = 0u32;
    let mut sum = 0u64;
    let mut changed = 0usize;
    let mut counted = 0usize;
    for y in 0..h {
        for x in 0..w {
            if exclude
                .iter()
                .any(|(x0, y0, x1, y1)| x >= *x0 && x < *x1 && y >= *y0 && y < *y1)
            {
                continue;
            }
            let index = ((y as u32 * a.w + x as u32) * 4) as usize;
            let pixel_diff = (a.rgba[index] as i32 - b.rgba[index] as i32)
                .abs()
                .max((a.rgba[index + 1] as i32 - b.rgba[index + 1] as i32).abs())
                .max((a.rgba[index + 2] as i32 - b.rgba[index + 2] as i32).abs())
                as u32;
            max_diff = max_diff.max(pixel_diff);
            sum += u64::from(pixel_diff);
            counted += 1;
            if pixel_diff > 2 {
                changed += 1;
            }
        }
    }
    let mean = sum as f64 / counted.max(1) as f64;
    Some((max_diff, mean, changed))
}

/// Copy the screen rectangle of a visible window as top-down RGBA. Runs on any
/// thread: it never touches Slint, so a capture cannot delay the animation's
/// own paint and the 0/90/180ms samples land on the wall clock.
pub(crate) fn f150_grab_screen(rect: WinRect) -> Option<(u32, u32, Vec<u8>)> {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ, SRCCOPY,
    };
    if rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    let (w, h) = (rect.w, rect.h);
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if mem.is_null() || bmp.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen);
            return None;
        }
        let old = SelectObject(mem, bmp as HGDIOBJ);
        let copied = BitBlt(mem, 0, 0, w, h, screen, rect.x, rect.y, SRCCOPY);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            rgba.as_mut_ptr() as *mut std::ffi::c_void,
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if copied == 0 || lines == 0 {
            return None;
        }
    }
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    Some((w as u32, h as u32, rgba))
}

/// Capture the window at 0/90/180ms after `set_search_open(true)`. The panel is
/// hidden by the caller, so the bar is the only intended change. The grabs run
/// on a worker thread against the composed screen, not through the UI thread.
pub(crate) fn f150_capture_morph(weak: &Weak<AppWindow>) -> Option<Vec<F150Frame>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<F150Frame>>();
    let _ = f145_on_ui(weak, move |ui| {
        ui.set_search_open(true);
        let Some(rect) = player_window_rect(ui.window()) else {
            let _ = tx.send(Vec::new());
            return;
        };
        let started = Instant::now();
        std::thread::spawn(move || {
            let targets = [0u64, 90, 180];
            let mut frames = Vec::with_capacity(targets.len());
            for target in targets {
                let due = started + Duration::from_millis(target);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                let at_ms = started.elapsed().as_secs_f64() * 1000.0;
                if let Some((w, h, rgba)) = f150_grab_screen(rect) {
                    frames.push(F150Frame {
                        at_ms,
                        w,
                        h,
                        rgba,
                    });
                }
            }
            let _ = tx.send(frames);
        });
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

/// UI thread: this player's own frame counter and measured present rate.
/// Never `stats_snapshot()`: that would be the focused player's numbers.
pub(crate) fn f150_player_rows() -> Vec<(u64, f32, u64, bool, bool, u64, bool)> {
    let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
        return Vec::new();
    };
    player_sessions(&state)
        .into_iter()
        .map(|session| {
            (
                session.id,
                session
                    .control
                    .play_stats()
                    .map(|stats| stats.fps_presented)
                    .unwrap_or(-1.0),
                session.control.presented_frames(),
                session.control.paused(),
                session.control.buffering(),
                session.control.playback_position_ms(),
                session.window.get_playback_queued(),
            )
        })
        .collect()
}

pub(crate) fn f150_player_montage(sessions: &[Rc<PlayerSession>], tag: &str) -> Option<PathBuf> {
    let mut paths = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        let path = qa_capture_path(&format!("{tag}_win{}", index + 1));
        if capture_window_png(session.window.window(), &path).is_some() {
            paths.push(path);
        }
    }
    let out = qa_capture_path(tag);
    f149_montage_side(&paths, &out)?;
    Some(out)
}

pub(crate) fn f151_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f151 {}", line.as_ref()));
}

/// The player window as the SCREEN shows it. The freeze/overlay captures run
/// inside Slint timer callbacks, and `PrintWindow` from inside the window's own
/// message loop returns a black bitmap; a screen BitBlt has no such dependency
/// (and the player window is raised before the gesture). The picture is frozen
/// by the mailbox gate, so a grab a few ms after the release is the release
/// frame.
pub(crate) fn f151_screen_frame(window: &slint::Window) -> Option<F150Frame> {
    let rect = player_window_rect(window)?;
    let (w, h, rgba) = f150_grab_screen(rect)?;
    Some(F150Frame {
        at_ms: 0.0,
        w,
        h,
        rgba,
    })
}

/// Mean RGB and the worst channel cast, in percent of full scale, of a live
/// logical-pixel patch of a window.
pub(crate) fn f151_mean_rgb(
    window: &slint::Window,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<(f64, f64, f64, f64)> {
    let patch = window_patch(window, x, y, w, h)?;
    let mut sum = [0u64; 3];
    let mut count = 0u64;
    for px in patch.chunks_exact(4) {
        for channel in 0..3 {
            sum[channel] += u64::from(px[channel]);
        }
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let mean = [
        sum[0] as f64 / count as f64,
        sum[1] as f64 / count as f64,
        sum[2] as f64 / count as f64,
    ];
    let hi = mean[0].max(mean[1]).max(mean[2]);
    let lo = mean[0].min(mean[1]).min(mean[2]);
    Some((mean[0], mean[1], mean[2], (hi - lo) / 255.0 * 100.0))
}

/// Press at `from`, move `steps` times, release at `to`, then capture the
/// release frame and +100/+400ms. Everything runs on the UI thread, so the
/// release capture is in the same turn as the Up event.
#[allow(clippy::too_many_arguments)]
pub(crate) fn f151_schedule_seek(
    target: F145Target,
    from: (f32, f32),
    to: (f32, f32),
    steps: u32,
    release_tag: String,
    mid_tag: String,
    late_tag: String,
    tx: Sender<Option<F151SeekCapture>>,
) {
    let press = |x: f32, y: f32| {
        target.with_window(|win| {
            win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x, y),
            });
            win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: slint::LogicalPosition::new(x, y),
                button: slint::platform::PointerEventButton::Left,
            });
        });
    };
    let ui_moves0 = target.seek_moves();
    press(from.0, from.1);
    let press_ms = target.drag_ms().unwrap_or(-1.0);
    let seeks0 = SEEK_REQUEST_COUNT.load(Ordering::SeqCst);
    let index = Rc::new(Cell::new(0u32));
    let move_ms = Rc::new(Cell::new(-1.0f32));
    let timer = Rc::new(slint::Timer::default());
    keep_qa_timer(timer.clone());
    let timer_a = timer.clone();
    let target_a = target.clone();
    let index_a = index.clone();
    let move_ms_a = move_ms.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(8),
        move || {
            let n = index_a.get();
            if n < steps {
                let t = (n + 1) as f32 / steps as f32;
                let x = from.0 + (to.0 - from.0) * t;
                let y = from.1 + (to.1 - from.1) * t;
                target_a.with_window(|win| {
                    win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(x, y),
                    });
                });
                if let Some(ms) = target_a.drag_ms() {
                    move_ms_a.set(ms);
                }
                index_a.set(n + 1);
                return;
            }
            // One gesture, one release: a repeating timer that kept sending Up
            // events every 8 ms corrupted the next synthetic gesture (its press
            // never latched, and the fallback timer applied the old target).
            // One gesture, one release: a repeating timer that kept sending Up
            // events every 8 ms corrupted the next synthetic gesture (its press
            // never latched, and the fallback timer applied the old target).
            timer_a.stop();
            let Some(control) = target_a.control() else {
                let _ = tx.send(None);
                return;
            };
            let move_ms_now = move_ms_a.get();
            let released = target_a
                .with_window(|win| {
                    win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: slint::LogicalPosition::new(to.0, to.1),
                    });
                    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
                        position: slint::LogicalPosition::new(to.0, to.1),
                        button: slint::platform::PointerEventButton::Left,
                    });
                    let scale = f64::from(win.scale_factor().max(0.2));
                    let (px, py, pw, ph) = target_a.pill_rect()?;
                    Some((scale, (px, py, pw, ph)))
                })
                .flatten();
            let Some((scale, pill_logical)) = released else {
                let _ = tx.send(None);
                return;
            };
            let ui_moves = target_a.seek_moves() - ui_moves0;
            let release_ms = target_a.drag_ms().unwrap_or(-1.0);
            let dragging_after = target_a.dragging().unwrap_or(false);
            let frames_release = control.presented_frames();
            let freeze_release = control.seek_freeze();
            let drag = F145DragResult {
                moves: steps,
                updates: ui_moves,
                before: from.0,
                after: to.0,
                seek_before: seeks0,
                seek_after: SEEK_REQUEST_COUNT.load(Ordering::SeqCst),
            };
            // Screen grabs are safe inside a timer callback (`f151_screen_frame`
            // is a BitBlt, not `PrintWindow`), and the mailbox freeze holds the
            // picture still, so these are the true release/+100/+400 frames.
            let release_frame = target_a
                .with_window(|win| f151_screen_frame(win))
                .flatten();
            let Some(release) = release_frame else {
                let _ = tx.send(None);
                return;
            };
            let _ = f150_write_frame(&release, &release_tag);
            let at100: Rc<std::cell::RefCell<Option<F150Frame>>> =
                Rc::new(std::cell::RefCell::new(None));
            let frames_100 = Rc::new(Cell::new(0u64));
            let freeze_100 = Rc::new(Cell::new(false));
            let overlay_100 = Rc::new(Cell::new(false));
            let overlay_400 = Rc::new(Cell::new(false));
            let freeze_400 = Rc::new(Cell::new(false));
            let frames_400 = Rc::new(Cell::new(0u64));
            // +100ms: the second identity sample.
            {
                let target = target_a.clone();
                let control = control.clone();
                let at100 = at100.clone();
                let freeze_100 = freeze_100.clone();
                let overlay_100 = overlay_100.clone();
                let frames_100 = frames_100.clone();
                let mid_tag = mid_tag.clone();
                slint::Timer::single_shot(Duration::from_millis(100), move || {
                    let _ = target.with_window(|win| {
                        frames_100.set(control.presented_frames());
                        freeze_100.set(control.seek_freeze());
                        overlay_100.set(control.seek_buffering());
                        let frame = f151_screen_frame(win);
                        if let Some(frame) = frame.as_ref() {
                            let _ = f150_write_frame(frame, &mid_tag);
                        }
                        *at100.borrow_mut() = frame;
                    });
                });
            }
            // +400ms: the frozen picture / overlay evidence, and the result.
            {
                let target = target_a.clone();
                let control = control.clone();
                let at100 = at100.clone();
                let freeze_100 = freeze_100.clone();
                let overlay_100 = overlay_100.clone();
                let frames_100 = frames_100.clone();
                let overlay_400 = overlay_400.clone();
                let freeze_400 = freeze_400.clone();
                let frames_400 = frames_400.clone();
                let late_tag = late_tag.clone();
                let release = release.clone();
                let tx = tx.clone();
                slint::Timer::single_shot(Duration::from_millis(400), move || {
                    let _ = target.with_window(|win| {
                        frames_400.set(control.presented_frames());
                        freeze_400.set(control.seek_freeze());
                        overlay_400.set(control.seek_buffering());
                        if let Some(frame) = f151_screen_frame(win) {
                            let _ = f150_write_frame(&frame, &late_tag);
                        }
                    });
                    let at100 = at100.borrow_mut().take();
                    let capture = at100.map(|at100| F151SeekCapture {
                        drag,
                        ui_moves,
                        press_ms,
                        move_ms: move_ms_now,
                        release_ms,
                        dragging_after,
                        scale,
                        pill: (
                            (pill_logical.0 * scale as f32) as i32,
                            (pill_logical.1 * scale as f32) as i32,
                            (pill_logical.2 * scale as f32) as i32,
                            (pill_logical.3 * scale as f32) as i32,
                        ),
                        release: release.clone(),
                        at_100: at100,
                        frames_release,
                        frames_100: frames_100.get(),
                        frames_400: frames_400.get(),
                        freeze_release,
                        freeze_100: freeze_100.get(),
                        freeze_400: freeze_400.get(),
                        overlay_100: overlay_100.get(),
                        overlay_400: overlay_400.get(),
                    });
                    let _ = tx.send(capture);
                });
            }
        },
    );
}

/// The newest `seek swap` receipt after `start` bytes of the debug log.
pub(crate) fn f151_swap_receipt(start: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let path = env::var("BILI_DEBUG_LOG")?;
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.lines()
        .filter(|line| line.contains("seek swap target_ms="))
        .last()
        .map(|line| line.to_string())
}

pub(crate) fn f151_log_size() -> u64 {
    env::var("BILI_DEBUG_LOG")
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0)
}

pub(crate) fn f151_parse(line: &str, key: &str) -> String {
    line.find(key)
        .map(|at| {
            let rest = &line[at + key.len()..];
            rest.split_whitespace().next().unwrap_or("-").to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

/// Crop `(x,y,w,h)` (logical px) out of a saved window capture, with a small
/// margin, and save it. Returns the strip path.
pub(crate) fn f151_crop_png(
    source: &std::path::Path,
    tag: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Option<PathBuf> {
    let image = image::open(source).ok()?.to_rgba8();
    // `capture_window_png` saves physical px; the player window is 1280x800
    // logical in this run, so the image width gives the DPI factor.
    let factor = image.width() as f32 / 1280.0;
    let margin = 6.0 * factor;
    let cx = ((x * factor) - margin).max(0.0) as u32;
    let cy = ((y * factor) - margin).max(0.0) as u32;
    let cw = ((w * factor) + margin * 2.0).min(image.width() as f32 - cx as f32) as u32;
    let ch = ((h * factor) + margin * 2.0).min(image.height() as f32 - cy as f32) as u32;
    if cw == 0 || ch == 0 {
        return None;
    }
    let crop = image::imageops::crop_imm(&image, cx, cy, cw, ch).to_image();
    let path = qa_capture_path(tag);
    crop.save(&path).ok()?;
    Some(path)
}

pub(crate) fn f152_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f152 {}", line.as_ref()));
}


pub(crate) fn f152_reset_morph() {
    F152_MORPH.with(|frames| frames.borrow_mut().clear());
}

pub(crate) fn f152_take_morph() -> Vec<(Instant, f32)> {
    F152_MORPH.with(|frames| std::mem::take(&mut *frames.borrow_mut()))
}

pub(crate) fn f152_morph_stat(frames: &[(Instant, f32)]) -> F152MorphStat {
    let mut gaps: Vec<f64> = frames
        .windows(2)
        .map(|pair| pair[1].0.duration_since(pair[0].0).as_secs_f64() * 1000.0)
        .collect();
    let mut bits: Vec<u32> = frames.iter().map(|(_, value)| value.to_bits()).collect();
    bits.sort_unstable();
    bits.dedup();
    let percentile = |sorted: &[f64], fraction: f64| -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let index = ((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1);
        sorted[index]
    };
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    F152MorphStat {
        ticks: frames.len(),
        distinct: bits.len(),
        p50: percentile(&gaps, 0.50),
        p95: percentile(&gaps, 0.95),
        p99: percentile(&gaps, 0.99),
        max: gaps.last().copied().unwrap_or(0.0),
        span_ms: match (frames.first(), frames.last()) {
            (Some(first), Some(last)) => {
                last.0.duration_since(first.0).as_secs_f64() * 1000.0
            }
            _ => 0.0,
        },
        last: frames.last().map(|(_, value)| *value).unwrap_or(0.0),
        gaps,
    }
}

/// f152 morph captures at 0/60/120/180 ms after the open. Same screen-grab
/// worker as f150: the UI thread keeps painting at its own cadence while a
/// separate thread samples the composed screen on the wall clock.
pub(crate) fn f152_capture_morph(weak: &Weak<AppWindow>) -> Option<Vec<F150Frame>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<F150Frame>>();
    let _ = f145_on_ui(weak, move |ui| {
        ui.set_search_open(true);
        let Some(rect) = player_window_rect(ui.window()) else {
            let _ = tx.send(Vec::new());
            return;
        };
        let started = Instant::now();
        std::thread::spawn(move || {
            let mut frames = Vec::with_capacity(4);
            for target in [0u64, 60, 120, 180] {
                let due = started + Duration::from_millis(target);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                let at_ms = started.elapsed().as_secs_f64() * 1000.0;
                if let Some((w, h, rgba)) = f150_grab_screen(rect) {
                    frames.push(F150Frame { at_ms, w, h, rgba });
                }
            }
            let _ = tx.send(frames);
        });
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

/// One player row for the occlusion report: id, occluded, paused, frames,
/// position, audio pid and the suspended ffplay's frozen audio clock (ms).
/// Deliberately no WASAPI query: this is polled every 50 ms while waiting for a
/// transition, and the session state is read once at the pause/resume receipts
/// instead.
pub(crate) fn f152_player_rows() -> Vec<(u64, bool, bool, u64, u64, u32, Option<u64>)> {
    let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
        return Vec::new();
    };
    player_sessions(&state)
        .into_iter()
        .map(|session| {
            (
                session.id,
                session.control.occluded(),
                session.control.paused(),
                session.control.presented_frames(),
                session.control.playback_position_ms(),
                session.control.audio_pid(),
                session.control.audio_clock(),
            )
        })
        .collect()
}

pub(crate) fn f152_session_state(id: u64) -> Option<(bool, bool, u64, u64)> {
    f152_player_rows()
        .into_iter()
        .find(|row| row.0 == id)
        .map(|row| (row.1, row.2, row.3, row.4))
}

/// Wait until both open players have a picture and are not buffering.
pub(crate) fn f152_wait_playing(weak: &Weak<AppWindow>, players: usize, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        let ready = f145_on_ui(weak, move |_ui| {
            let rows = f152_player_rows();
            rows.len() >= players
                && rows
                    .iter()
                    .take(players)
                    .all(|row| row.3 > 0 && !row.2)
        })
        .unwrap_or(false);
        if ready {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

pub(crate) fn f152_wait_occluded(weak: &Weak<AppWindow>, id: u64, want: bool, timeout: Duration) -> Option<u64> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        let state = f145_on_ui(weak, move |_ui| f152_session_state(id)).flatten();
        if let Some((occluded, _, _, _)) = state {
            if occluded == want {
                return Some(started.elapsed().as_millis() as u64);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Bring one player above every other window (topmost + top of the band).
pub(crate) fn f152_raise_player(weak: &Weak<AppWindow>, id: u64) -> bool {
    f145_on_ui(weak, move |_ui| {
        let Some(state) = WALLPAPER_STATE.with(|slot| slot.borrow().clone()) else {
            return false;
        };
        let Some(session) = player_sessions(&state).into_iter().find(|session| session.id == id)
        else {
            return false;
        };
        let window = session.window.window();
        let topmost = win::set_topmost(window, true);
        let raised = win::raise(window);
        window.request_redraw();
        topmost || raised
    })
    .unwrap_or(false)
}

/// Capture one player's OWN rendered scene with Slint's `take_snapshot`, not a
/// desktop grab: the QA machine has unrelated windows in front, and PrintWindow
/// on an occluded window returns whatever is visible at that screen rect. The
/// snapshot is the window's tree (video frame included) regardless of z-order.
pub(crate) fn f152_capture_player(weak: &Weak<AppWindow>, id: u64, tag: &str) -> Option<PathBuf> {
    let tag = tag.to_string();
    f145_on_ui(weak, move |_ui| {
        let state = WALLPAPER_STATE.with(|slot| slot.borrow().clone())?;
        let session = player_sessions(&state)
            .into_iter()
            .find(|session| session.id == id)?;
        let buffer = session.window.window().take_snapshot().ok()?;
        let path = qa_capture_path(&tag);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let bytes: Vec<u8> = buffer
            .as_slice()
            .iter()
            .flat_map(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a])
            .collect();
        let png = image::RgbaImage::from_raw(buffer.width(), buffer.height(), bytes)?;
        png.save(&path).ok()?;
        Some(path)
    })
    .flatten()
}

/// f152 QA: open one known video in a NEW player window.
///
/// `BV1SEn96HEhe` is the playlist's first entry and the one video on this
/// machine whose playlist-preload cache file is COMPLETE (the playlist preload
/// wrote `%LOCALAPPDATA%\\o11a1\\cache\\BV1SEn96HEhe-43065541691-80-12.part`
/// and its map before any player touched it). Opened at 1080p with the same
/// automatic codec pick the preload used (HEVC, codec 12), it decodes the local
/// file, so the occlusion/resume proof does not depend on the QA network. The
/// front player can be any card: it only has to be an opaque window above this
/// one.
pub(crate) fn f152_open_cached(
    session_slot: &SessionSlot,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) -> bool {
    let Some(session) = active_session(session_slot) else {
        crate::debug_log("f152 cached open skipped reason=no-session");
        return false;
    };
    let video = bilibili::FeedVideo {
        bvid: "BV1SEn96HEhe".to_string(),
        cid: 43_065_541_691,
        aid: 0,
        title: "f152 cached playlist".to_string(),
        pic: String::new(),
        owner: String::new(),
        owner_mid: 0,
        view: 0,
        duration: 0,
        pub_ts: 0,
    };
    crate::debug_log("f152 open cached bvid=BV1SEn96HEhe cid=43065541691 qn=80 codec=auto(12)");
    open_player_inner(session, video, state, settings_store, OpenMode::NewWindow, None);
    true
}

pub(crate) fn f154_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f154 {}", line.as_ref()));
}

/// `BILI_QA_*` flags: 1/true/yes/on enable, anything else (and "unset") is off.
pub(crate) fn f154_env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// The renderer Slint picks when `SLINT_BACKEND` says nothing, in the exact
/// order of the winit backend's own default factory
/// (`i-slint-backend-winit`'s `default_renderer_factory`): skia first, then
/// femtovg-wgpu, then femtovg on OpenGL, then software.
///
/// f155: this crate's default feature set now includes `femtovg-wgpu`, so the
/// unqualified default is femtovg ON WGPU. `renderer-femtovg` (OpenGL) is still
/// compiled in via slint's own default features, but winit only selects it when
/// asked by name -- which is why the request and the default are logged apart.
pub(crate) fn f154_default_renderer() -> &'static str {
    if cfg!(feature = "skia") {
        "skia"
    } else if cfg!(feature = "femtovg-wgpu") {
        "femtovg-wgpu"
    } else {
        "femtovg"
    }
}

/// Renderers compiled into this binary. The two `1`s come from `slint`'s
/// default feature list (`renderer-femtovg`, `renderer-software`), which this
/// crate never disables; the two `cfg!` values come from this crate's own
/// features (`femtovg-wgpu` is in the default set since f155).
pub(crate) fn f154_compiled_renderers() -> String {
    format!(
        "femtovg=1 software=1 skia={} femtovg-wgpu={}",
        u8::from(cfg!(feature = "skia")),
        u8::from(cfg!(feature = "femtovg-wgpu")),
    )
}

/// Is `name` (a renderer name parsed out of `SLINT_BACKEND`) one this binary
/// actually compiled? Kept in step with [`f154_compiled_renderers`], because a
/// name that is NOT compiled makes Slint fall back to the default -- and the
/// boot line must say so instead of claiming the request won.
pub(crate) fn f154_renderer_compiled(name: &str) -> bool {
    match name {
        // slint's default features, never disabled.
        "femtovg" | "gl" | "software" | "sw" => true,
        "femtovg-wgpu" => cfg!(feature = "femtovg-wgpu"),
        "skia" | "skia-wgpu" | "skia-opengl" | "skia-software" => cfg!(feature = "skia"),
        _ => false,
    }
}

/// The name this app logs for a renderer: the `SLINT_BACKEND` spelling is not
/// canonical (`gl`/`femtovg`, `sw`/`software`), the log is.
pub(crate) fn f154_canonical_renderer(name: &str) -> &'static str {
    match name {
        "gl" | "femtovg" => "femtovg",
        "sw" | "software" => "software",
        "femtovg-wgpu" => "femtovg-wgpu",
        "skia" | "skia-wgpu" | "skia-opengl" | "skia-software" => "skia",
        _ => "unknown",
    }
}

/// The renderer name inside `SLINT_BACKEND`, parsed exactly like
/// `i-slint-backend-selector::parse_backend_env_var`: `winit-skia` and a bare
/// `skia` both mean skia; `gl` / `winit` mean "no renderer request".
pub(crate) fn f154_requested_renderer(request: &str) -> Option<String> {
    let text = request.trim().to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    let renderer = match text.split_once('-') {
        Some((_, rest)) => rest.to_string(),
        None => match text.as_str() {
            "gl" | "winit" => return None,
            bare => bare.to_string(),
        },
    };
    if renderer.is_empty() {
        None
    } else {
        Some(renderer)
    }
}

/// GPU libraries this process has actually loaded. Slint does not expose the
/// winning renderer through its public API, so the loaded modules are the
/// ground truth: the femtovg path needs an OpenGL ICD (`opengl32.dll` plus a
/// vendor driver or ANGLE), the wgpu paths need Vulkan/D3D12, and the software
/// renderer needs none of them.
pub(crate) fn f154_gpu_modules() -> Vec<String> {
    const WANTED: [&str; 12] = [
        "opengl32.dll",
        "libegl.dll",
        "libglesv2.dll",
        "nvoglv64.dll",
        "atio6axx.dll",
        "igvk64.dll",
        "vulkan-1.dll",
        "d3d11.dll",
        "d3d12.dll",
        "dxgi.dll",
        "dcomp.dll",
        "dwmapi.dll",
    ];
    f154_loaded_modules()
        .into_iter()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            WANTED.contains(&lower.as_str())
        })
        .collect()
}

/// Every module loaded into this process (lower-case file names).
pub(crate) fn f154_loaded_modules() -> Vec<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
        TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    };
    let mut names = Vec::new();
    // SAFETY: the snapshot handle is checked against INVALID_HANDLE_VALUE,
    // `entry.dwSize` is set as the API requires, and module names are copied
    // out of the buffer the API just filled before it is reused.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return names;
        }
        let mut entry: MODULEENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut more = Module32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry
                .szModule
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szModule.len());
            names.push(String::from_utf16_lossy(&entry.szModule[..len]));
            entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
            more = Module32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    names
}

/// First line of the renderer story: what was requested, what is compiled in,
/// what the request resolves to, and where it came from.
///
/// `source=default` (nothing asked), `source=env` (the request names a renderer
/// this binary has), or `source=env-uncompiled-fallback` (the request names one
/// it does not, so Slint falls back to [`f154_default_renderer`] -- the line
/// still reports what actually paints, not what was asked for).
pub(crate) fn f154_log_renderer_boot() {
    let request = env::var("SLINT_BACKEND")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let shown = request.clone().unwrap_or_else(|| "<unset>".to_string());
    let (resolved, source) = match request.as_deref().and_then(f154_requested_renderer) {
        Some(name) if f154_renderer_compiled(&name) => {
            (f154_canonical_renderer(&name), "env")
        }
        Some(_uncompiled) => (f154_default_renderer(), "env-uncompiled-fallback"),
        None => (f154_default_renderer(), "default"),
    };
    f154_log(format!(
        "renderer boot SLINT_BACKEND={shown} backend=winit compiled={} default={} resolved={resolved} source={source}",
        f154_compiled_renderers(),
        f154_default_renderer(),
    ));
    // The chain, spelled out once: what runs with no request, what winit falls
    // back to when the chosen renderer cannot create a surface, and which names
    // remain selectable in THIS binary.
    f154_log(format!(
        "renderer chain default={} auto_fallback={} explicit={}",
        f154_default_renderer(),
        // winit's per-window fallback skips OpenGL whenever femtovg-wgpu is
        // compiled in (`not(feature = "renderer-femtovg-wgpu")` on the glutin
        // arm), so the automatic chain after wgpu is software.
        if cfg!(feature = "femtovg-wgpu") { "software" } else { "femtovg,software" },
        if cfg!(feature = "skia") {
            "winit-skia,winit-femtovg-wgpu,winit-femtovg,winit-software"
        } else {
            "winit-femtovg-wgpu,winit-femtovg,winit-software"
        },
    ));
}



/// CPU milliseconds this thread has burned so far (kernel + user).
pub(crate) fn f154_thread_cpu_ms() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    fn ticks(time: &FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }
    // SAFETY: `GetCurrentThread` returns the calling thread's pseudo handle,
    // which `GetThreadTimes` may query; every out-parameter is a live local.
    unsafe {
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        if GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return 0.0;
        }
        // FILETIME counts 100 ns units; 10 000 of them are one millisecond.
        (ticks(&kernel) + ticks(&user)) as f64 / 10_000.0
    }
}

pub(crate) fn f154_cpu_start() {
    F154_CPU_START.with(|slot| slot.set(Some(f154_thread_cpu_ms())));
}

/// CPU milliseconds the UI thread spent since [`f154_cpu_start`], i.e. the
/// paint and layout work of every frame in between, without any wall-clock
/// noise from the compositor or the vsync wait.
pub(crate) fn f154_cpu_stop() -> f64 {
    let now = f154_thread_cpu_ms();
    F154_CPU_START.with(|slot| {
        let start = slot.take().unwrap_or(now);
        (now - start).max(0.0)
    })
}

/// Capture the window at 0 and 180 ms after the open, grabbed on the wall clock
/// from a worker thread (the f150/f152 screen grab), so the shot itself never
/// delays the paint it is measuring.
pub(crate) fn f154_capture_morph(weak: &Weak<AppWindow>, tag: &str) -> Option<Vec<PathBuf>> {
    let tag = tag.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
    let _ = f145_on_ui(weak, move |ui| {
        ui.set_search_open(true);
        let Some(rect) = player_window_rect(ui.window()) else {
            let _ = tx.send(Vec::new());
            return;
        };
        let started = Instant::now();
        std::thread::spawn(move || {
            let mut paths = Vec::new();
            for target in [0u64, 180] {
                let due = started + Duration::from_millis(target);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                if let Some((w, h, rgba)) = f150_grab_screen(rect) {
                    let frame = F150Frame {
                        at_ms: started.elapsed().as_secs_f64() * 1000.0,
                        w,
                        h,
                        rgba,
                    };
                    if let Some(path) = f150_write_frame(&frame, &format!("f154_{tag}_{target}")) {
                        paths.push(path);
                    }
                }
            }
            let _ = tx.send(paths);
        });
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

pub(crate) fn f155_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f155 {}", line.as_ref()));
}

/// `PrintWindow(PW_RENDERFULLCONTENT)` into a 32-bit top-down DIB, with NO
/// `BitBlt` fallback: this is exactly the call `window_rgba` (the QA capture
/// path) makes first, and the return value says whether the window's own
/// surface could be read.
pub(crate) fn f155_printwindow_rgba(window: &slint::Window) -> Option<(i32, i32, Vec<u8>, i32)> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    let hwnd = win::hwnd_of(window)?;
    let rect = player_window_rect(window)?;
    if rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    let (w, h) = (rect.w, rect.h);
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    let printed;
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if mem.is_null() || bmp.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen);
            return None;
        }
        let old = SelectObject(mem, bmp as HGDIOBJ);
        printed = PrintWindow(hwnd, mem, 2);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            rgba.as_mut_ptr() as *mut std::ffi::c_void,
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if lines == 0 {
            return None;
        }
    }
    // BGRA from the DIB -> RGBA, and alpha to opaque.
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    Some((w, h, rgba, printed))
}

/// Capture one window with PrintWindow only, save it, and log the numbers that
/// separate "the picture is really there" from "the surface came back black".
pub(crate) fn f155_shot(window: &slint::Window, tag: &str) -> Option<()> {
    let (w, h, rgba, printed) = f155_printwindow_rgba(window)?;
    let path = qa_capture_path(tag);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let saved = image::RgbaImage::from_raw(w as u32, h as u32, rgba.clone())
        .and_then(|png| png.save(&path).ok())
        .is_some();
    let pixels = (w as usize) * (h as usize);
    let mut sum = 0u64;
    let mut black = 0usize;
    for px in rgba.chunks_exact(4) {
        let luma = (u32::from(px[0]) * 299 + u32::from(px[1]) * 587 + u32::from(px[2]) * 114) / 1000;
        sum += u64::from(luma);
        if luma < 8 {
            black += 1;
        }
    }
    let mean = sum as f64 / pixels.max(1) as f64;
    let black_pct = black as f64 * 100.0 / pixels.max(1) as f64;
    // The same rect off the composed screen (what the user sees). If PrintWindow
    // returned black while this did not, the diff is the proof.
    let scale = f64::from(window.scale_factor().max(0.2));
    let logical_w = (f64::from(w) / scale).round() as i32;
    let logical_h = (f64::from(h) / scale).round() as i32;
    let screen = screen_patch(window, 0, 0, logical_w, logical_h);
    let (screen_mean, diff) = match screen.as_ref() {
        Some(patch) => {
            let mut ssum = 0u64;
            for px in patch.chunks_exact(4) {
                ssum += u64::from(
                    (u32::from(px[0]) * 299 + u32::from(px[1]) * 587 + u32::from(px[2]) * 114)
                        / 1000,
                );
            }
            let smean = ssum as f64 / (patch.len() / 4).max(1) as f64;
            (smean, patch_diff(patch, &rgba))
        }
        None => (-1.0, None),
    };
    f155_log(format!(
        "shot tag={tag} saved={} path={} size={w}x{h} printwindow={printed} mean={mean:.1} black_pct={black_pct:.1} screen_mean={screen_mean:.1} diff_max={} diff_mean={}",
        u8::from(saved),
        path.display(),
        diff.map(|d| d.0).map_or(-1i32, i32::from),
        diff.map(|d| (d.1 * 100.0).round() / 100.0).map_or(-1.0, |v| v),
    ));
    Some(())
}

/// One search open on the 4 ms UI-thread probe, the same measurement f149/f152
/// use, so the number is comparable with the idle and non-video rows.
pub(crate) fn f155_morph_gap(weak: &Weak<AppWindow>, phase: &'static str, hold: Duration) -> Option<(usize, f64, f64, f64, f64, usize, usize)> {
    let _ = f145_on_ui(weak, |ui| ui.set_search_open(false));
    std::thread::sleep(Duration::from_millis(320));
    let _ = f145_on_ui(weak, move |_ui| f149_probe_start(phase));
    let _ = f145_on_ui(weak, |ui| ui.set_search_open(true));
    std::thread::sleep(hold);
    f145_on_ui(weak, |_ui| f149_probe_stop())
}

pub(crate) fn f124_capture(ui: &AppWindow, tag: &str) {
    let path = qa_capture_path(tag);
    if let Some((w, h)) = capture_window_png(&ui.window(), &path) {
        crate::debug_log(&format!(
            "f124 capture tag={tag} path={} size={w}x{h}",
            path.display()
        ));
    } else {
        crate::debug_log(&format!("f124 capture tag={tag} failed"));
    }
}

pub(crate) fn f127_capture(ui: &AppWindow, tag: &str) {
    let path = qa_capture_path(tag);
    if let Some((w, h)) = capture_window_png(&ui.window(), &path) {
        crate::debug_log(&format!(
            "f127 capture tag={tag} path={} size={w}x{h}",
            path.display()
        ));
    } else {
        crate::debug_log(&format!("f127 capture tag={tag} failed"));
    }
}

/// Run `f` on the UI thread and wait for its result (UI properties may only be
/// read there).
pub(crate) fn f127_on_ui<T, F>(weak: &Weak<AppWindow>, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&AppWindow) -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let weak = weak.clone();
    slint::invoke_from_event_loop(move || {
        let value = weak.upgrade().map(|ui| f(&ui));
        let _ = tx.send(value);
    })
    .ok()?;
    rx.recv_timeout(Duration::from_secs(15)).ok().flatten()
}

pub(crate) fn f127_snap(weak: &Weak<AppWindow>) -> Option<F127Snap> {
    f127_on_ui(weak, |ui| F127Snap {
        view: ui.get_view_mode(),
        strip_y: ui.get_qa_fav_strip_y(),
        strip_h: ui.get_qa_fav_strip_h(),
        switch_y: ui.get_qa_fav_switch_y(),
        content_y: ui.get_qa_fav_content_y(),
        offset: ui.get_qa_fav_offset(),
        tab: ui.get_fav_tab(),
        cards: ui.get_masonry().row_count(),
        empty: ui.get_fav_empty(),
    })
}

/// Arm the page probes, let them publish, log one measurement, capture and
/// disarm. The values a tab does not publish (the content top on 历史 / 稍后,
/// and the strip offset where there is no strip at all) are cleared first, so a
/// stale number can never be read as a measurement.
pub(crate) fn f127_measure(weak: &Weak<AppWindow>, tag: &str, has_strip: bool) -> Option<F127Snap> {
    let _ = f127_on_ui(weak, move |ui| {
        ui.set_qa_fav_strip_y(-1.0);
        ui.set_qa_fav_strip_h(-1.0);
        ui.set_qa_fav_switch_y(-1.0);
        ui.set_qa_fav_content_y(-1.0);
        if !has_strip {
            ui.set_qa_fav_offset(-1.0);
        }
        ui.set_wheel_probe(2);
    });
    std::thread::sleep(Duration::from_millis(500));
    let snap = f127_snap(weak)?;
    snap.log(tag);
    let tag = tag.to_string();
    let _ = f127_on_ui(weak, move |ui| f127_capture(ui, &tag));
    let _ = f127_on_ui(weak, |ui| ui.set_wheel_probe(0));
    Some(snap)
}

/// Run `f` on the UI thread and wait for its result (UI properties may only be
/// read there).
pub(crate) fn f143_on_ui<T, F>(weak: &Weak<AppWindow>, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&AppWindow) -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let weak = weak.clone();
    slint::invoke_from_event_loop(move || {
        let value = weak.upgrade().map(|ui| f(&ui));
        let _ = tx.send(value);
    })
    .ok()?;
    rx.recv_timeout(Duration::from_secs(15)).ok().flatten()
}

pub(crate) fn f143_capture(ui: &AppWindow, tag: &str) {
    let path = qa_capture_path(tag);
    match capture_window_png(&ui.window(), &path) {
        Some((w, h)) => crate::debug_log(&format!(
            "f143 capture tag={tag} path={} size={w}x{h}",
            path.display()
        )),
        None => crate::debug_log(&format!("f143 capture tag={tag} failed")),
    }
}

/// Open the home feed (推荐 selected) through the real route.
pub(crate) fn f143_open_feed(weak: &Weak<AppWindow>) {
    let _ = f127_on_ui(weak, |ui| {
        if ui.get_view_mode() != 0 || ui.get_home_tab() != 0 {
            ui.set_home_tab(0);
            ui.set_view_mode(0);
            ui.invoke_back_to_feed();
        }
    });
}

pub(crate) fn f143_wait_feed(weak: &Weak<AppWindow>, timeout: Duration) -> usize {
    let start = Instant::now();
    let mut rows = 0;
    while start.elapsed() < timeout {
        rows = f127_on_ui(weak, |ui| ui.get_feed_items().row_count()).unwrap_or(0);
        if rows > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    rows
}

/// Screen pixels only (no PrintWindow): used for the search morph, where asking
/// the window to paint would let the 180 ms animation finish before the grab.
pub(crate) fn f124_capture_screen(ui: &AppWindow, tag: &str) {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ, SRCCOPY,
    };
    let Some(rect) = player_window_rect(&ui.window()) else {
        crate::debug_log(&format!("f124 capture tag={tag} failed reason=no-rect"));
        return;
    };
    // The grab reads the SCREEN, so lift our window above any neighbouring one
    // for the frame (no activation, no focus change), then drop it back.
    let hwnd = win::hwnd_of(&ui.window());
    if let Some(hwnd) = hwnd {
        unsafe {
            SetWindowPos(
                hwnd as HWND,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    let (w, h) = (rect.w, rect.h);
    if w <= 0 || h <= 0 {
        return;
    }
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if mem.is_null() || bmp.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen);
            return;
        }
        let old = SelectObject(mem, bmp as HGDIOBJ);
        BitBlt(mem, 0, 0, w, h, screen, rect.x, rect.y, SRCCOPY);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            rgba.as_mut_ptr() as *mut std::ffi::c_void,
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        DeleteObject(bmp as HGDIOBJ);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if lines == 0 {
            return;
        }
    }
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    if let Some(hwnd) = hwnd {
        unsafe {
            SetWindowPos(
                hwnd as HWND,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
    let path = qa_capture_path(tag);
    let Some(png) = image::RgbaImage::from_raw(w as u32, h as u32, rgba) else {
        return;
    };
    match png.save(&path) {
        Ok(()) => crate::debug_log(&format!(
            "f124 capture tag={tag} path={} size={w}x{h} source=screen",
            path.display()
        )),
        Err(err) => crate::debug_log(&format!("f124 capture tag={tag} err={err}")),
    }
}

pub(crate) fn f124_wheel_steps(ui: &AppWindow, x: f32, y: f32) {
    for _ in 0..3 {
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(x, y),
                delta_x: 0.0,
                delta_y: -40.0,
            });
    }
}

/// QA-only message rows: this account has no messages, so the design capture
/// uses clearly-labelled sample rows (the real empty page is captured too).
/// Everything about the row layout (76px, 40px avatar, type sizes) is the real
/// component; only the strings are fixtures.
/// One click through Slint's item tree (move, press, release). Used to prove
/// the wheel overlay still lets a chip's own TouchArea take the click.
pub(crate) fn f124_click(ui: &AppWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
}

pub(crate) fn f124_msg_fixtures(ui: &AppWindow, sessions: bool) {
    if sessions {
        let rows: Vec<SessionRow> = [
            ("示例用户 01", "今晚的直播推迟到九点，记得来看。", "2分钟前", "3"),
            ("示例用户 02", "上次说的那套预设我整理好了，发你。", "1小时前", ""),
            ("示例用户 03", "截图里的参数是 4.2 / 0.8 / 12，你试试。", "昨天", ""),
            ("示例用户 04", "收到，明天下午对一下进度。", "3天前", ""),
        ]
        .iter()
        .enumerate()
        .map(|(index, (name, preview, time, unread))| SessionRow {
            talker_id: format!("{}", index + 1).into(),
            name: (*name).into(),
            avatar: Image::default(),
            preview: (*preview).into(),
            time: (*time).into(),
            unread: (*unread).into(),
        })
        .collect();
        ui.set_msg_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<MsgRow>::new(),
        ))));
        ui.set_session_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
        ui.set_msg_loading(false);
        ui.set_msg_status("QA 示例会话".into());
    } else {
        let rows: Vec<MsgRow> = [
            ("示例用户 01", "回复了你的评论", "这段的节奏我很喜欢，尤其是最后十秒。", "5分钟前", "2条"),
            ("示例用户 02", "@了你", "这个思路是不是也能用在实拍里？", "20分钟前", ""),
            ("示例用户 03", "赞了你的视频", "《从零开始的建模流程》", "1小时前", ""),
            ("示例用户 04", "回复了你的评论", "收藏了，回头慢慢看。", "昨天", ""),
            ("示例用户 05", "赞了你的评论", "同意，工具只是工具，最后还是看人。", "2天前", ""),
        ]
        .iter()
        .map(|(nickname, title, detail, time, count)| MsgRow {
            nickname: (*nickname).into(),
            avatar: Image::default(),
            title: (*title).into(),
            detail: (*detail).into(),
            time: (*time).into(),
            count: (*count).into(),
        })
        .collect();
        ui.set_msg_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
        ui.set_session_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<SessionRow>::new(),
        ))));
        ui.set_msg_loading(false);
        ui.set_msg_status("QA 示例消息".into());
    }
    crate::debug_log(&format!(
        "f124 msg fixture=1 mode={} notices={} sessions={}",
        if sessions { "sessions" } else { "notices" },
        ui.get_msg_items().row_count(),
        ui.get_session_items().row_count()
    ));
}

/// The raw gaps of the LAST `f149_probe_*` run (UI thread only). The probe
/// clears its buffer on start, so after `stop` it holds exactly that run - the
/// pooled p95 needs the samples, not one run's own percentiles.
pub(crate) fn f149_probe_samples() -> Vec<f64> {
    F149_GAPS.with(|gaps| gaps.borrow().clone())
}

/// One run of the feed's wheel: Slint's own pointer-scrolled event, three to
/// five notches, exactly like a wheel over the list.
pub(crate) fn f136_wheel(ui: &AppWindow, x: f32, y: f32, delta: f32, count: usize) {
    for _ in 0..count {
        ui.window().dispatch_event(
            slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(x, y),
                delta_x: 0.0,
                delta_y: delta,
            },
        );
    }
}

/// A point inside the feed/tank list, derived from the measured tab origin.
pub(crate) fn f136_feed_point(ui: &AppWindow) -> (f32, f32) {
    (
        (ui.get_qa_fish_tab0_x() + 300.0).max(24.0),
        (ui.get_qa_fish_tab_y() + 150.0).max(24.0),
    )
}

pub(crate) fn f136_capture(ui: &AppWindow, tag: &str) {
    let path = qa_capture_path(tag);
    let size = capture_window_png(&ui.window(), &path);
    crate::debug_log(&format!("f136 capture tag={tag} size={size:?}"));
}

/// The active recipe ids and the draft length, for the retention/clearing logs.
pub(crate) fn f136_recipe_state(ui: &AppWindow) -> String {
    let recipes = ui.get_fish_recipes();
    let mut active = Vec::new();
    for index in 0..recipes.row_count() {
        if let Some(row) = recipes.row_data(index) {
            if row.active {
                active.push(row.id.to_string());
            }
        }
    }
    let draft: String = ui
        .get_fish_instruction_draft()
        .chars()
        .take(12)
        .collect();
    format!(
        "active={:?} draft_len={} draft_head={draft:?}",
        active,
        ui.get_fish_instruction_draft().chars().count()
    )
}

/// Log the real laid-out geometry + the tagline's contrast. The numbers are read
/// from the window's `qa_fish_*` out properties, which the page fills from its
/// live layout, not from constants duplicated here.
pub(crate) fn f136_measure(ui: &AppWindow) {
    crate::debug_log(&format!(
        "f136 measure tabs tab0_x={:.1} tab0_w={:.1} tab1_x={:.1} tab1_w={:.1} tab_y={:.1}",
        ui.get_qa_fish_tab0_x(),
        ui.get_qa_fish_tab0_w(),
        ui.get_qa_fish_tab1_x(),
        ui.get_qa_fish_tab1_w(),
        ui.get_qa_fish_tab_y()
    ));
    crate::debug_log(&format!(
        "f136 measure feed card_w={:.1} gap={:.1} cover_h={:.1} ring={:.1}",
        ui.get_qa_fish_card_w(),
        ui.get_qa_fish_card_gap(),
        ui.get_qa_fish_cover_h(),
        ui.get_qa_fish_ring()
    ));
    crate::debug_log(&format!(
        "f136 measure config row_h={:.1} recipe_w={:.1} recipe_h={:.1} recipe_gap={:.1}",
        ui.get_qa_fish_config_row(),
        ui.get_qa_fish_recipe_w(),
        ui.get_qa_fish_recipe_h(),
        ui.get_qa_fish_recipe_gap()
    ));
    let theme = ui.global::<AppTheme>();
    let soft = theme.get_text_soft();
    let card = composite_over(theme.get_card(), theme.get_bg());
    let glass = composite_over(theme.get_glass_tint(), theme.get_bg());
    crate::debug_log(&format!(
        "f136 contrast tagline ink=#{:02x}{:02x}{:02x} on_card=#{:02x}{:02x}{:02x} ratio={:.2}:1 on_glass=#{:02x}{:02x}{:02x} ratio={:.2}:1",
        soft.red(),
        soft.green(),
        soft.blue(),
        card.red(),
        card.green(),
        card.blue(),
        contrast_ratio(soft, card),
        glass.red(),
        glass.green(),
        glass.blue(),
        contrast_ratio(soft, glass)
    ));
}
