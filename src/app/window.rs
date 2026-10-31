//! Native window behaviour: chrome, backdrop, drag/resize, tray, toasts and window bookkeeping.

use crate::*;

pub(crate) fn player_active() -> bool {
    PLAYER_OPEN_COUNT.load(Ordering::SeqCst) > 0
}

/// One more player window owns playback.
pub(crate) fn player_opened() {
    let count = PLAYER_OPEN_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    crate::debug_log(&format!("players opened n={count}"));
}

/// One player window is gone; the main window resumes only at zero.
pub(crate) fn player_closed() {
    let _ = PLAYER_OPEN_COUNT.try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
        Some(count.saturating_sub(1))
    });
    crate::debug_log(&format!(
        "players closed n={}",
        PLAYER_OPEN_COUNT.load(Ordering::SeqCst)
    ));
}

pub(crate) fn region_labels() -> Vec<SharedString> {
    REGIONS
        .iter()
        .map(|(name, _)| SharedString::from(*name))
        .collect()
}

/// Append `line` to the file named by `BILI_DEBUG_LOG`, when set. Diagnostic
/// only: never writes to stdout and is inert unless the env var is present.
///
/// One `write_all` per line: the pool, the prefetch workers and the player all
/// log from different threads, and `writeln!` on a `File` issues several writes
/// per line, which let two lines interleave in the middle of a word.
pub(crate) fn debug_log(line: &str) {
    let Ok(path) = std::env::var("BILI_DEBUG_LOG") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let mut record = String::with_capacity(line.len() + 1);
        record.push_str(line);
        record.push('\n');
        let _ = file.write_all(record.as_bytes());
    }
}

pub(crate) const TOAST_LIFETIME: Duration = Duration::from_millis(2800);

pub(crate) const TOAST_MAX_VISIBLE: usize = 3;

pub(crate) const TOAST_TICK: Duration = Duration::from_millis(250);

pub(crate) const TOAST_KIND_INFO: i32 = 0;

pub(crate) const TOAST_KIND_SUCCESS: i32 = 1;

pub(crate) const TOAST_KIND_ERROR: i32 = 2;

/// One queued toast. `born` is when it entered the queue, not when it first
/// became visible: every entry keeps its own 2.8s lifetime.
pub(crate) struct ToastEntry {
    pub(crate) text: String,
    pub(crate) kind: i32,
    pub(crate) born: Instant,
}

/// Per-player-window queue plus the model the UI renders. `hwnd` is the
/// lookup key (Slint's `Weak` has no pointer identity); `window` keeps the
/// weak handle, so a queue whose player is gone is pruned before it can be
/// mistaken for a recycled HWND.
pub(crate) struct ToastQueue {
    pub(crate) window: Weak<PlayerWindow>,
    pub(crate) hwnd: isize,
    /// Oldest first; the UI reverses it so the newest sits at the bottom.
    pub(crate) items: VecDeque<ToastEntry>,
    pub(crate) model: Rc<VecModel<ToastItem>>,
    pub(crate) last_tick: Instant,
}

/// Remaining fraction of the oldest visible toast's lifetime (1.0 at birth,
/// 0.0 at expiry); 1.0 while the queue is empty, so the next toast starts full.
pub(crate) fn toast_progress(queue: &ToastQueue, now: Instant) -> f32 {
    let Some(oldest) = queue.items.front() else {
        return 1.0;
    };
    let elapsed = now.saturating_duration_since(oldest.born).as_secs_f32();
    (1.0 - elapsed / TOAST_LIFETIME.as_secs_f32()).clamp(0.0, 1.0)
}

/// The queue as the UI's model: oldest first, newest last.
pub(crate) fn toast_model_items(queue: &ToastQueue) -> Vec<ToastItem> {
    queue
        .items
        .iter()
        .map(|entry| ToastItem {
            text: entry.text.clone().into(),
            kind: entry.kind,
        })
        .collect()
}

/// Show one user-facing toast on `player`.
///
/// At most [`TOAST_MAX_VISIBLE`] stay up: a fourth drops the oldest. The queue
/// is republished immediately, so the new toast appears without waiting for the
/// next tick.
pub(crate) fn toast(player: &PlayerWindow, text: impl Into<String>, kind: i32) {
    let text = text.into();
    let Some(hwnd) = win::hwnd_of(player.window()) else {
        crate::debug_log(&format!("toast skipped no-hwnd text={text}"));
        return;
    };
    let now = Instant::now();
    let published = TOAST_QUEUES.with(|cell| {
        let mut queues = match cell.try_borrow_mut() {
            Ok(queues) => queues,
            Err(_) => return None,
        };
        // Drop queues whose player window is gone (closed or superseded).
        queues.retain(|queue| queue.window.upgrade().is_some());
        let position = queues.iter().position(|queue| queue.hwnd == hwnd);
        let position = match position {
            Some(position) => position,
            None => {
                let model = Rc::new(VecModel::<ToastItem>::from(Vec::<ToastItem>::new()));
                player.set_toasts(ModelRc::from(model.clone()));
                queues.push(ToastQueue {
                    window: player.as_weak(),
                    hwnd,
                    items: VecDeque::new(),
                    model,
                    last_tick: now,
                });
                queues.len().saturating_sub(1)
            }
        };
        let queue = &mut queues[position];
        queue.items.push_back(ToastEntry {
            text: text.clone(),
            kind,
            born: now,
        });
        let mut dropped = Vec::new();
        while queue.items.len() > TOAST_MAX_VISIBLE {
            if let Some(entry) = queue.items.pop_front() {
                dropped.push(entry.text);
            }
        }
        queue.model.set_vec(toast_model_items(queue));
        let progress = toast_progress(queue, now);
        Some((progress, queue.items.len(), dropped))
    });
    let Some((progress, visible, dropped)) = published else {
        return;
    };
    player.set_toast_progress(progress);
    for text in dropped {
        crate::debug_log(&format!("toast drop text={text}"));
    }
    crate::debug_log(&format!(
        "toast publish kind={kind} visible={visible} progress={progress:.2} text={text}"
    ));
}

/// One tick of the toast queue: expire entries and republish the shared
/// progress bar. Cheap and self-gating - it publishes at most every
/// [`TOAST_TICK`], so the player's 33ms tick can call it unconditionally.
pub(crate) fn toast_tick(player: &PlayerWindow) {
    let Some(hwnd) = win::hwnd_of(player.window()) else {
        return;
    };
    let now = Instant::now();
    let tick = TOAST_QUEUES.with(|cell| -> Option<(Option<f32>, Vec<String>)> {
        let mut queues = match cell.try_borrow_mut() {
            Ok(queues) => queues,
            Err(_) => return None,
        };
        queues.retain(|queue| queue.window.upgrade().is_some());
        let queue = queues.iter_mut().find(|queue| queue.hwnd == hwnd)?;
        if queue.items.is_empty() {
            return None;
        }
        if now.saturating_duration_since(queue.last_tick) < TOAST_TICK {
            return Some((None, Vec::new()));
        }
        queue.last_tick = now;
        let mut expired = Vec::new();
        // Birth times are insertion-ordered, so once the front is alive every
        // later entry is too.
        while queue
            .items
            .front()
            .map(|entry| now.saturating_duration_since(entry.born) >= TOAST_LIFETIME)
            .unwrap_or(false)
        {
            if let Some(entry) = queue.items.pop_front() {
                expired.push(entry.text);
            }
        }
        if !expired.is_empty() {
            queue.model.set_vec(toast_model_items(queue));
        }
        let progress = toast_progress(queue, now);
        Some((Some(progress), expired))
    });
    let Some((progress, expired)) = tick else {
        return;
    };
    if let Some(progress) = progress {
        player.set_toast_progress(progress);
    }
    for text in expired {
        crate::debug_log(&format!("toast expire text={text}"));
    }
}

/// Drop a closing/superseded window's queue. Called from both close paths so a
/// closed player never leaves its `Rc<VecModel>` alive in the thread-local.
pub(crate) fn toast_forget(player: &PlayerWindow) {
    let Some(hwnd) = win::hwnd_of(player.window()) else {
        return;
    };
    TOAST_QUEUES.with(|cell| {
        if let Ok(mut queues) = cell.try_borrow_mut() {
            queues.retain(|queue| queue.hwnd != hwnd);
        }
    });
}

/// Remember the player window that is now on screen. Called once per open,
/// before `show()`'s first frame is drawn.
pub(crate) fn set_active_player(player: &PlayerWindow) {
    ACTIVE_PLAYER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = player.as_weak();
        }
    });
}

/// Forget the active player after a close, so a late worker message cannot land
/// a toast on a window that is already gone.
pub(crate) fn clear_active_player() {
    ACTIVE_PLAYER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = Weak::default();
        }
    });
}

/// Route a user-facing player status into the active window's toast queue.
///
/// Called from [`player::set_status`] inside an `invoke_from_event_loop`
/// closure, i.e. always on the UI thread. The raw detail was already logged by
/// the caller; this only decides whether the user sees it, once per event.
pub(crate) fn toast_active(text: impl Into<String>, kind: i32) {
    let text = text.into();
    if text.is_empty() {
        return;
    }
    let repeat = LAST_STATUS_TOAST.with(|cell| {
        let mut last = match cell.try_borrow_mut() {
            Ok(last) => last,
            Err(_) => return false,
        };
        let now = Instant::now();
        let repeated = last
            .as_ref()
            .map(|(previous, at)| {
                previous == &text && now.saturating_duration_since(*at) < Duration::from_millis(1000)
            })
            .unwrap_or(false);
        *last = Some((text.clone(), now));
        repeated
    });
    if repeat {
        crate::debug_log(&format!("toast skipped repeat text={text}"));
        return;
    }
    // Resolve the window first: upgrading runs no closure, so no second borrow.
    let player = ACTIVE_PLAYER.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|slot| slot.upgrade())
    });
    let Some(player) = player else {
        crate::debug_log(&format!("toast skipped no-player text={text}"));
        return;
    };
    toast(&player, text, kind);
}

/// Whether `player` is the window the queue and the toasts follow.
///
/// Slint window handles have no stable pointer identity (`PlayerWindow` is a
/// value handle), so identity is compared through the native HWND — the same
/// key the toast queues already use.
pub(crate) fn active_player_hwnd() -> Option<isize> {
    ACTIVE_PLAYER
        .with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|slot| slot.upgrade())
        })
        .and_then(|player| win::hwnd_of(player.window()))
}

pub(crate) fn is_active_player(player: &PlayerWindow) -> bool {
    match (win::hwnd_of(player.window()), active_player_hwnd()) {
        (Some(current), Some(active)) => current == active,
        _ => false,
    }
}

/// The OS foreground window (`0` when none). Used by the player pointer watch
/// to follow activation without installing another native hook.
pub(crate) fn foreground_hwnd() -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow() as isize }
}

/// Make `session` the focused player: the window card_open(0) replaces, and the
/// one toasts, the tray boss key and the queue's end-of-track follow.
///
/// `notify_playlist` hands it the queue's "now playing" block (a user click on
/// an older window). At open time the queue already learned the video through
/// `Playlist::on_open`, so that path passes `false`.
pub(crate) fn focus_player(state: &Rc<PlayerState>, session: &Rc<PlayerSession>, notify_playlist: bool) {
    // The session is registered before `show()` (its stored hwnd is 0 there);
    // resolve it once here so the very first focus — and every focus handed
    // over on a close — is real instead of a silent early return.
    let mut hwnd = session.hwnd.get();
    if hwnd == 0 {
        if let Some(actual) = win::hwnd_of(session.window.window()) {
            session.hwnd.set(actual);
            hwnd = actual;
        }
    }
    if hwnd == 0 {
        return;
    }
    let already = state
        .focused
        .borrow()
        .as_ref()
        .map(|current| current.hwnd.get() == hwnd)
        .unwrap_or(false);
    if already {
        return;
    }
    *state.focused.borrow_mut() = Some(session.clone());
    set_active_player(&session.window);
    player::set_active_control(Some(session.control.clone()));
    // A 直播 room is not a queue item: it never becomes the playlist's
    // now-playing row (and the queue never re-opens it).
    if notify_playlist && !session.live {
        if let Some(host) = playlist() {
            host.on_focus(&session.window, &session.video);
        }
    }
    crate::debug_log(&format!(
        "player focus win={} hwnd={hwnd} notify={notify_playlist}",
        session.id
    ));
}

/// Follow WM activation by polling the foreground window once per pointer tick.
///
/// Only a player window that actually owns the foreground becomes focused; when
/// another app (or the main window) is in front, the last focused player is
/// kept, so card_open(0) still has a sensible target.
pub(crate) fn poll_player_focus(state: &Rc<PlayerState>, player: &PlayerWindow) {
    let Some(hwnd) = win::hwnd_of(player.window()) else {
        return;
    };
    if hwnd == 0 || foreground_hwnd() != hwnd {
        return;
    }
    if let Some(session) = session_for_window(state, player) {
        if session.hwnd.get() != hwnd {
            session.hwnd.set(hwnd);
        }
        focus_player(state, &session, true);
    }
}

/// Move one player window to `(x, y)` keeping its size, WITHOUT activating it
/// (`SetWindowPos` with `SWP_NOACTIVATE`). Window API only.
pub(crate) fn place_player_window(player: &PlayerWindow, x: i32, y: i32) -> bool {
    let Some((_, _, w, h)) = win::window_rect(player.window()) else {
        return false;
    };
    set_player_window_rect(
        player.window(),
        WinRect {
            x,
            y,
            w: w.max(1),
            h: h.max(1),
        },
    )
}

/// Parse a `MM:SS` / `HH:MM:SS` duration label back to seconds.
pub(crate) fn duration_seconds(label: &str) -> u64 {
    let mut total = 0u64;
    for part in label.split(':') {
        let Ok(value) = part.trim().parse::<u64>() else {
            return 0;
        };
        total = total.saturating_mul(60).saturating_add(value);
    }
    total
}

pub(crate) fn next_player_session_id() -> u64 {
    PLAYER_SESSION_IDS.with(|cell| {
        let next = cell.get().saturating_add(1);
        cell.set(next);
        next
    })
}

/// Open sessions, oldest first (never while holding the `players` borrow).
pub(crate) fn player_sessions(state: &Rc<PlayerState>) -> Vec<Rc<PlayerSession>> {
    state.players.borrow().clone()
}

/// True while any open player is actually playing or buffering. A user-paused
/// player is not "busy": the fish may transcribe while nothing is on screen.
pub(crate) fn any_player_busy(state: &Rc<PlayerState>) -> bool {
    player_sessions(state).iter().any(|session| {
        let control = &session.control;
        control.presented_frames() > 0 && (control.buffering() || !control.paused())
    })
}

/// The session for one window handle, if it is still open.
pub(crate) fn session_for_hwnd(state: &Rc<PlayerState>, hwnd: isize) -> Option<Rc<PlayerSession>> {
    state
        .players
        .borrow()
        .iter()
        .find(|session| session.hwnd.get() == hwnd)
        .cloned()
}

/// The session a window's `Weak` resolves to, matched by native handle.
pub(crate) fn session_for_window(state: &Rc<PlayerState>, player: &PlayerWindow) -> Option<Rc<PlayerSession>> {
    let hwnd = win::hwnd_of(player.window())?;
    if let Some(found) = session_for_hwnd(state, hwnd) {
        return Some(found);
    }
    // Created before its HWND existed: resolve and cache the handle now.
    for session in player_sessions(state) {
        if win::hwnd_of(session.window.window()) == Some(hwnd) {
            session.hwnd.set(hwnd);
            return Some(session);
        }
    }
    None
}

/// The window that last had focus, else the newest one.
pub(crate) fn focused_session(state: &Rc<PlayerState>) -> Option<Rc<PlayerSession>> {
    if let Some(session) = state.focused.borrow().as_ref() {
        if session.hwnd.get() != 0 {
            return Some(session.clone());
        }
    }
    state.players.borrow().last().cloned()
}

/// The newest open player, as a plain window handle.
pub(crate) fn newest_player(state: &Rc<PlayerState>) -> Option<PlayerWindow> {
    state
        .players
        .borrow()
        .last()
        .map(|session| session.window.clone_strong())
}

/// Find the current mini window handle (if any) without holding the borrow.
pub(crate) fn mini_handle(state: &Rc<PlayerState>) -> Option<MiniPlayerWindow> {
    state.mini.borrow().as_ref().map(|mini| mini.clone_strong())
}

/// Undo the player-active suspension: resume main-window background work (the
/// incremental cover fetches and the skeleton shimmer, both gated on
/// `player_active()`). Shared by both player close
/// paths; the main window stays suspended while another player is still open.
pub(crate) fn resume_main_window(_state: &Rc<PlayerState>) {
    player_closed();
}

/// Re-assert the topmost flag on windows whose native handle became available
/// after `show()` (the flag cannot be set until the OS window exists).
///
/// Only ever applies when the user explicitly enabled 置顶 (`on`): the default
/// is normal window ordering, so nothing is pinned above other windows unless
/// the user asked for it.
pub(crate) fn ensure_topmost(on: bool, main: Option<&PlayerWindow>, state: &Rc<PlayerState>) {
    if !on {
        return;
    }
    if let Some(main) = main {
        if win::is_topmost(main.window()) == Some(false) {
            win::set_topmost(main.window(), true);
        }
    }
    if let Some(mini) = mini_handle(state) {
        if mini.window().is_visible() && win::is_topmost(mini.window()) == Some(false) {
            win::set_topmost(mini.window(), true);
        }
    }
}

/// Bring up the system tray (icon + menu + global hotkey + notifications).
pub(crate) fn start_tray(weak: &Weak<AppWindow>) {
    let hide_muted = Arc::new(AtomicBool::new(false));
    let actions = tray::Actions {
        toggle: {
            let weak = weak.clone();
            let hide_muted = hide_muted.clone();
            Box::new(move || {
                let weak = weak.clone();
                let hide_muted = hide_muted.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        toggle_main_window(&ui, &hide_muted);
                    }
                });
            })
        },
        show: {
            let weak = weak.clone();
            Box::new(move || {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        // Same path as the tray double-click: clears the
                        // `WS_EX_TOOLWINDOW` bit and gives the taskbar its tab
                        // back, so "显示主窗口" really restores the window.
                        let state = win::show_from_tray(ui.window());
                        crate::debug_log(&format!("tray: show {}", state.show_summary()));
                    }
                });
            })
        },
        hide: {
            let weak = weak.clone();
            Box::new(move || {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let state = win::hide_to_tray(ui.window());
                        crate::debug_log(&format!("tray: hide {}", state.hide_summary()));
                    }
                });
            })
        },
        quit: Box::new(|| {
            let _ = slint::invoke_from_event_loop(|| request_quit("tray menu quit"));
        }),
    };
    match tray::start(actions, "O11A1") {
        Ok(handle) => tray::set_global(handle),
        Err(err) => crate::debug_log(&format!("tray start err={err}")),
    }
}

/// Boss key: flip the main window's native visibility, muting playback with it.
///
/// A minimised window still reports `IsWindowVisible`, so it counts as hidden
/// here: the boss key restores it instead of hiding it a second time.
pub(crate) fn toggle_main_window(ui: &AppWindow, hide_muted: &Arc<AtomicBool>) {
    let window = ui.window();
    let visible = win::is_visible(window).unwrap_or(false);
    let minimized = win::is_minimized(window).unwrap_or(false);
    if visible && !minimized {
        let state = win::hide_to_tray(window);
        if let Some(control) = player::active_control() {
            if !control.muted() {
                control.set_muted(true);
                hide_muted.store(true, Ordering::SeqCst);
            }
        }
        crate::debug_log(&format!("boss key: main window hidden {}", state.hide_summary()));
    } else {
        let state = win::show_from_tray(window);
        if hide_muted.swap(false, Ordering::SeqCst) {
            if let Some(control) = player::active_control() {
                control.set_muted(false);
            }
        }
        crate::debug_log(&format!("boss key: main window shown {}", state.show_summary()));
    }
}

/// What closing the main window does, read back from the stored preference.
///
/// This is the ONE dispatcher behind every close route: the custom title bar's
/// ✕ (`on_close_window`), the native close hook (`WM_CLOSE`, i.e. Alt+F4 and the
/// taskbar's 关闭) and the `BILI_CLOSE_TEST` driver call it, and the tray's 退出
/// reuses the same quit path ([`request_quit`]).
pub(crate) fn apply_close_action(ui: &AppWindow, settings_store: &Arc<Mutex<settings::Settings>>) {
    let window = ui.window();
    match close_action(settings_store) {
        settings::CLOSE_ACTION_QUIT => {
            crate::debug_log(&format!(
                "close: action={} quit requested {}",
                settings::CLOSE_ACTION_QUIT,
                close_state(window)
            ));
            request_quit("close action=2");
        }
        _ => {
            // Hide to the tray *properly*: `SW_HIDE` alone leaves the taskbar
            // button (and the Alt-Tab entry) behind, which reads as "the window
            // cannot be closed". `hide_to_tray` drops the tab through
            // `ITaskbarList` and marks the window `WS_EX_TOOLWINDOW`.
            let state = win::hide_to_tray(window);
            // f188: no balloon. The owner asked for the "hidden to tray" toast
            // to stay off - the window disappearing is signal enough, and the
            // tray icon is always there.
            crate::debug_log(&format!(
                "close: action={} hide-to-tray {} {}",
                settings::CLOSE_ACTION_TRAY,
                state.hide_summary(),
                close_state(window)
            ));
        }
    }
}

/// The stored 关闭主窗口 behaviour, snapped to one of [`settings::CLOSE_ACTIONS`].
pub(crate) fn close_action(settings_store: &Arc<Mutex<settings::Settings>>) -> u32 {
    settings_store
        .lock()
        .map(|guard| settings::normalise_close_action(guard.close_action))
        .unwrap_or_else(|_| settings::default_close_action())
}

/// The single quit path: close action 2 and the tray's 退出 both end here.
///
/// It asks Slint's event loop to return instead of calling `process::exit`, so
/// the teardown at the end of `main` (player children reaped, tray icon removed)
/// still runs. Safe from the close hook: it fires on the UI thread.
pub(crate) fn request_quit(reason: &str) {
    crate::debug_log(&format!("quit reason={reason}"));
    let _ = slint::quit_event_loop();
}

/// Native close state for the close log lines: `visible=… iconic=… zoomed=…`.
pub(crate) fn close_state(window: &slint::Window) -> String {
    format!(
        "visible={} iconic={} zoomed={}",
        flag(win::is_visible(window)),
        flag(win::is_minimized(window)),
        flag(is_zoomed(window)),
    )
}

pub(crate) fn flag(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "none",
    }
}

/// Native `IsZoomed` for a Slint window (the maximise bit `win.rs` does not expose).
pub(crate) fn is_zoomed(window: &slint::Window) -> Option<bool> {
    let hwnd = win::hwnd_of(window)?;
    Some(unsafe { IsZoomed(hwnd as *mut std::ffi::c_void) } != 0)
}

pub(crate) fn set_close_targets(ui: &Weak<AppWindow>, store: &Arc<Mutex<settings::Settings>>) {
    let targets = CloseTargets {
        ui: ui.clone(),
        store: store.clone(),
    };
    CLOSE_TARGETS.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            *slot = Some(targets);
        }
    });
}

/// The registered `WM_CLOSE` hook: same handler as the title bar's ✕.
pub(crate) fn dispatch_main_close() {
    CLOSE_TARGETS.with(|slot| {
        let Ok(borrow) = slot.try_borrow() else {
            return;
        };
        let Some(targets) = borrow.as_ref() else {
            return;
        };
        if let Some(ui) = targets.ui.upgrade() {
            apply_close_action(&ui, &targets.store);
        }
    });
}

/// Install the native close hook once the main window has a HWND.
///
/// Called from the 1500 ms frameless watchdog, which is the existing retry chain:
/// `set_close_hook` returns `false` until the handle exists. Only the main window
/// is hooked; the player and mini windows keep their own close behaviour.
pub(crate) fn install_close_hook(window: &slint::Window) {
    let Some(hwnd) = win::hwnd_of(window) else {
        return;
    };
    if CLOSE_HOOK_HWND.load(Ordering::SeqCst) == hwnd {
        return;
    }
    if win::set_close_hook(window, dispatch_main_close) {
        CLOSE_HOOK_HWND.store(hwnd, Ordering::SeqCst);
        crate::debug_log(&format!("close: hook installed hwnd={hwnd}"));
    }
}

/// Whether `pid` still owns a top-level window (live-process probe).
///
/// Every Slint window and the tray's hidden window are top-level, so a process
/// that has finished tearing down owns none. `std::process::id()` supplies the id
/// without enabling another `windows-sys` feature.
pub(crate) fn pid_owns_windows(pid: u32) -> bool {
    let mut probe: (u32, usize) = (pid, 0);
    unsafe {
        EnumWindows(
            Some(count_pid_window),
            (&mut probe as *mut (u32, usize)) as isize,
        );
    }
    probe.1 > 0
}

/// Apply the frameless + compositor-alpha treatment once the native window
/// exists.
///
/// The HWND is only valid after the event loop has created the window, so this
/// re-queues itself a bounded number of times on the UI thread until
/// `win::setup_glass` finds a handle, then logs the resulting frame style.
/// `win::setup_glass` here is the compositor alpha + DWM chrome (the per-pixel
/// backdrop is the generated gradient, not a screen capture).
pub(crate) fn schedule_window_chrome(weak: Weak<AppWindow>, dark: bool, tries: u32) {
    let retry = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = retry.upgrade() else {
            return;
        };
        if win::style_bits(ui.window()).is_none() {
            if tries == 0 {
                crate::debug_log("window chrome: no hwnd after retries");
                return;
            }
            schedule_window_chrome(retry.clone(), dark, tries - 1);
            return;
        }
        let scale = ui.window().scale_factor();
        // Keep in lockstep with the root `min-width`/`min-height` in ui/app.slint.
        win::set_min_track_size(ui.window(), (480.0 * scale) as i32, (360.0 * scale) as i32);
        // The logged-in header is the drag region now; keep this in lockstep
        // with its 76px strip in ui/app.slint (12px float gap + 64px card).
        // The app's own drag TouchArea covers the whole strip, so pressing in
        // the gap or on the card moves the window instead of starting an OS
        // resize (the login page still uses its own 20px strip inside that
        // range).
        win::set_top_drag_strip((76.0 * scale) as i32);
        win::log_frame_state(ui.window(), "before-chrome");
        win::install_subclass(ui.window());
        // Every native move republishes the wallpaper origin (parallax). This is
        // the only trigger - no polling timer: an idle window does no work.
        {
            let move_weak = ui.as_weak();
            win::set_move_hook(ui.window(), move || {
                if let Some(ui) = move_weak.upgrade() {
                    publish_wallpaper(ui.global::<AppTheme>(), ui.window());
                }
            });
        }
        let (wall_x, wall_y) = publish_wallpaper(ui.global::<AppTheme>(), ui.window());
        crate::debug_log(&format!(
            "wallpaper origin win=main stage=chrome x={wall_x:.1} y={wall_y:.1}"
        ));
        win::setup_glass(ui.window(), dark);
        win::log_frame_state(ui.window(), "after-chrome");
        // Pin the landscape size once the layout has settled. On a scaled
        // display the window can come up at the content's preferred width
        // instead of the intended logical 1280x800, which shrinks every
        // horizontal metric (the bar included). The OS still clamps to the
        // work area.
        let size = ui.window().size();
        if size.width < 1200 {
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(600), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.window()
                        .set_size(slint::LogicalSize::new(1280.0, 800.0));
                    crate::debug_log(&format!(
                        "window size pinned {:?}",
                        ui.window().size()
                    ));
                }
            });
        }
    });
}

/// The wallpaper's origin for one window: `-(screen position mod tile)`, in
/// logical px.
///
/// The tile is pinned to the monitor, not to the window: a window whose
/// top-left sits at `(x, y)` shows the tile starting at `-x mod tile`. Two
/// consequences the UI relies on: moving by one full tile reproduces the same
/// pixels (the tile is seamless), and a small move reveals a different part.
pub(crate) fn wallpaper_origin(window: &slint::Window, tile: f32) -> (f32, f32) {
    let tile = f64::from(tile.max(1.0));
    let Some((x, y, _, _)) = win::window_rect(window) else {
        return (0.0, 0.0);
    };
    let scale = f64::from(window.scale_factor().max(0.2));
    let left = (f64::from(x) / scale).rem_euclid(tile);
    let top = (f64::from(y) / scale).rem_euclid(tile);
    (-(left as f32), -(top as f32))
}

/// Publish the wallpaper lattice for one window: the tile edge (once per
/// publish, cheap) and the origin derived from the live screen position.
///
/// Called at creation, on every theme switch, and from each window's
/// `WM_MOVE` hook. No timer: an idle window publishes nothing.
pub(crate) fn publish_wallpaper(theme: AppTheme, window: &slint::Window) -> (f32, f32) {
    let tile = backdrop::tile_px();
    let (wall_x, wall_y) = wallpaper_origin(window, tile);
    theme.set_wallpaper_tile(tile);
    theme.set_wall_x(wall_x);
    theme.set_wall_y(wall_y);
    (wall_x, wall_y)
}

/// Paint the generated wallpaper tile into the main window and mark it valid.
///
/// Called once at creation and again on every theme/blur switch, before anything
/// is shown: the first painted frame is already wallpaper + tint.
pub(crate) fn set_backdrop(ui: &AppWindow, dark: bool) {
    // The wallpaper IS the diffused image: at blur 0 the coloured balls read as
    // crisp discs, at the default they melt into a mesh gradient. Both variants
    // are published: `WallpaperLayer` selects by `AppTheme.dark`, so a theme
    // switch swaps the tile in the same frame. Generating the second variant
    // here is cached after the first switch (~one blur pass, UI thread).
    let blur_start = Instant::now();
    let dark_wallpaper = backdrop::image(true);
    let light_wallpaper = backdrop::image(false);
    let wallpaper = if dark {
        dark_wallpaper.clone()
    } else {
        light_wallpaper.clone()
    };
    let blurred = backdrop::blurred(dark);
    ui.set_wallpaper(dark_wallpaper);
    ui.set_wallpaper_light(light_wallpaper);
    ui.set_wallpaper_valid(true);
    ui.set_wallpaper(wallpaper);
    // Every GlassPanel in this window paints the SAME pre-blurred copy; the
    // first call per theme pays for the blur (three separable box passes on the
    // cached sharp pixels), every later call gets the cached image. Logged so
    // the one-time cost is visible and the per-frame cost is provably zero.
    crate::debug_log(&format!(
        "glass blur dark={} ms={:.1} size={}x{} tile={}px",
        u8::from(dark),
        blur_start.elapsed().as_secs_f64() * 1000.0,
        blurred.size().width,
        blurred.size().height,
        backdrop::tile_px(),
    ));
    ui.global::<AppTheme>().set_glass_blur(blurred);
    ui.global::<AppTheme>()
        .set_backdrop_blur(backdrop::blur_px() as i32);
    let (wall_x, wall_y) = publish_wallpaper(ui.global::<AppTheme>(), ui.window());
    crate::debug_log(&format!(
        "wallpaper publish win=main x={wall_x:.1} y={wall_y:.1} tile={:.0}",
        backdrop::tile_px()
    ));
}

/// The same for a player window.
pub(crate) fn set_player_backdrop(player: &PlayerWindow, dark: bool) {
    let dark_wallpaper = backdrop::image(true);
    let light_wallpaper = backdrop::image(false);
    let wallpaper = if dark {
        dark_wallpaper.clone()
    } else {
        light_wallpaper.clone()
    };
    player.set_wallpaper(dark_wallpaper);
    player.set_wallpaper_light(light_wallpaper);
    player.set_wallpaper(wallpaper);
    player.set_wallpaper_valid(true);
    player.global::<AppTheme>().set_glass_blur(backdrop::blurred(dark));
    player
        .global::<AppTheme>()
        .set_backdrop_blur(backdrop::blur_px() as i32);
    let (wall_x, wall_y) = publish_wallpaper(player.global::<AppTheme>(), player.window());
    crate::debug_log(&format!(
        "wallpaper publish win=player x={wall_x:.1} y={wall_y:.1} tile={:.0}",
        backdrop::tile_px()
    ));
}

/// The same for the mini player.
pub(crate) fn set_mini_backdrop(mini: &MiniPlayerWindow, dark: bool) {
    let dark_wallpaper = backdrop::image(true);
    let light_wallpaper = backdrop::image(false);
    let wallpaper = if dark {
        dark_wallpaper.clone()
    } else {
        light_wallpaper.clone()
    };
    mini.set_wallpaper(dark_wallpaper);
    mini.set_wallpaper_light(light_wallpaper);
    mini.set_wallpaper(wallpaper);
    mini.set_wallpaper_valid(true);
    mini.global::<AppTheme>().set_glass_blur(backdrop::blurred(dark));
    mini.global::<AppTheme>()
        .set_backdrop_blur(backdrop::blur_px() as i32);
    let (wall_x, wall_y) = publish_wallpaper(mini.global::<AppTheme>(), mini.window());
    crate::debug_log(&format!(
        "wallpaper publish win=mini x={wall_x:.1} y={wall_y:.1} tile={:.0}",
        backdrop::tile_px()
    ));
}

/// Replace the wallpaper / panel image in every live window of the current
/// theme. UI thread only. `dark` is the theme the pixels were generated for: a
/// generation that finished after the user switched theme is installed into
/// that theme's cache but never painted over the other palette.
pub(crate) fn republish_wallpaper(image: &slint::Image, dark: bool) {
    WALLPAPER_STATE.with(|slot| {
        let Some(state) = slot.borrow().clone() else {
            return;
        };
        if state.theme_dark.get() != dark {
            return;
        }
        if let Some(ui) = state.main.upgrade() {
            if dark {
                ui.set_wallpaper(image.clone());
            } else {
                ui.set_wallpaper_light(image.clone());
            }
            ui.global::<AppTheme>().set_glass_blur(image.clone());
            ui.global::<AppTheme>()
                .set_backdrop_blur(backdrop::blur_px() as i32);
        }
        for session in player_sessions(&state) {
            if dark {
                session.window.set_wallpaper(image.clone());
            } else {
                session.window.set_wallpaper_light(image.clone());
            }
            session
                .window
                .global::<AppTheme>()
                .set_glass_blur(image.clone());
        }
        let mini = state.mini.borrow().as_ref().map(|w| w.clone_strong());
        if let Some(mini) = mini {
            if dark {
                mini.set_wallpaper(image.clone());
            } else {
                mini.set_wallpaper_light(image.clone());
            }
            mini.global::<AppTheme>().set_glass_blur(image.clone());
        }
    });
}

pub(crate) fn apply_home_test(ui: &AppWindow, spec: &str) {
    for part in spec.split(',') {
        match part.trim() {
            "menu" => {
                ui.set_menu_hold(true);
                ui.set_section_menu_open(true);
            }
            "library" => {
                ui.set_menu_hold(true);
                ui.set_library_menu_open(true);
            }
            "search" => ui.set_search_open(true),
            "fish" => ui.invoke_fish_open(),
            "favorites" => ui.invoke_nav_favorites(),
            "settings" => ui.invoke_open_settings(),
            "account" => ui.set_account_pop(true),
            "typed" => {
                ui.set_search_open(true);
                ui.set_search_text("测".into());
            }
            // Press 开始 through the real callback (no key => the engine goes
            // to NeedsKey without any network).
            "start" => ui.invoke_fish_start(),
            "close" => {
                ui.set_menu_hold(false);
                ui.set_section_menu_open(false);
                ui.set_library_menu_open(false);
                ui.set_search_open(false);
            }
            _ => {}
        }
    }
}

/// The drag flush cadence: one window placement per display frame at 120 Hz.
/// The flush itself skips a tick whose target did not move, so a 60 Hz panel
/// simply sees half as many placements; the achieved rate is logged as
/// `drag flush hz=…` at the end of every drag.
pub(crate) const DRAG_FRAME: Duration = Duration::from_millis(8);

/// The main window drag handle, if the window has been built.
pub(crate) fn main_drag_handle() -> Option<Rc<win::WindowDrag>> {
    MAIN_DRAG.with(|slot| slot.try_borrow().ok().and_then(|drag| drag.clone()))
}

/// Whether the main drag should treat the button as held: the real Win32 state
/// or the QA replay's stand-in.
pub(crate) fn main_drag_button_held() -> bool {
    win::left_button_down() || QA_DRAG_HOLD.with(std::cell::Cell::get)
}

/// Human-readable `AudioSessionState` for the occlusion lines.
pub(crate) fn audio_state_name(state: Option<u32>) -> &'static str {
    match state {
        Some(0) => "inactive",
        Some(1) => "active",
        Some(2) => "expired",
        _ => "-",
    }
}

/// Whether the real cursor is anywhere inside the player window.
///
/// The comparison is done in LOGICAL px (the physical window rect divided by
/// the window's scale factor), the same units `ui/app.slint` lays the chrome
/// out in. This is the whole point of the watch: the chrome stays up while the
/// pointer is over ANY part of the window - buttons, popovers, the panels, the
/// gaps between them - and hides only after it has actually left.
pub(crate) fn player_pointer_inside(player: &PlayerWindow) -> bool {
    let window = player.window();
    // A hidden or minimised window must never keep the chrome up.
    if win::is_visible(window) != Some(true) || win::is_minimized(window) == Some(true) {
        return false;
    }
    let Some((wx, wy, ww, wh)) = win::window_rect(window) else {
        return false;
    };
    let Some((cx, cy)) = win::cursor_px() else {
        return false;
    };
    let scale = f64::from(window.scale_factor().max(0.2));
    let x = (cx - wx) as f64 / scale;
    let y = (cy - wy) as f64 / scale;
    x >= 0.0 && x < f64::from(ww) / scale && y >= 0.0 && y < f64::from(wh) / scale
}

/// Start one player window's 60Hz pointer watch and the layout-settle debounce.
///
/// One loop per window, owned by that window's [`PlayerSession`] AND captured by
/// that window's own tick/close callbacks (see `open_player_inner`), so a second
/// player always gets its own watch instead of being skipped by a shared slot.
/// Torn down by [`stop_player_loops`].
pub(crate) fn start_player_loops(
    state: &Rc<PlayerState>,
    session: &Rc<PlayerSession>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) -> Rc<PlayerLoops> {
    let player = &session.window;
    let panel_geom = &session.panel_geom;
    let loops = Rc::new(PlayerLoops {
        window: player.as_weak(),
        id: session.id,
        pointer_timer: slint::Timer::default(),
        layout_timer: slint::Timer::default(),
        pointer_inside: Cell::new(false),
        chrome_idle: Cell::new(ChromeIdle::new(Instant::now())),
        layout_seen: Cell::new(None),
        display_seen: Cell::new((0, 0, false)),
        occlusion_checked: Cell::new(None),
        occluded_since: Cell::new(None),
        occluded_hold: Cell::new(false),
        occluded_at: Cell::new(None),
    });
    // f183: book this player into the occlusion allow-list. Only the app's own
    // player windows may pause another player; every other window (a game, a
    // browser in fullscreen, the main window) is ignored by the z-order test.
    // The handle can appear a few frames after `show()`, so the watchdog books
    // it in again (idempotent) and `stop_player_loops` unregisters it.
    if let Some(hwnd) = win::hwnd_of(player.window()) {
        win::register_player_window(hwnd);
    }
    {
        // Armed by the pointer watch whenever the rect changed, and re-armed by
        // every further change, so it fires once the window has been still for
        // [`LAYOUT_SETTLE`] - the resize-end / move-end persist.
        let weak = player.as_weak();
        let geom = panel_geom.clone();
        let store = settings_store.clone();
        let offset = session.cascade;
        loops
            .layout_timer
            .start(slint::TimerMode::SingleShot, LAYOUT_SETTLE, move || {
                if let Some(player) = weak.upgrade() {
                    persist_player_layout(&geom, player.window(), &store, offset, "settle");
                }
            });
    }
    {
        let weak = player.as_weak();
        let loop_weak = Rc::downgrade(&loops);
        let state = Rc::downgrade(state);
        let control = session.control.clone();
        loops
            .pointer_timer
            .start(slint::TimerMode::Repeated, POINTER_WATCH, move || {
                let (Some(player), Some(loops)) = (weak.upgrade(), loop_weak.upgrade()) else {
                    return;
                };
                let inside = player_pointer_inside(&player);
                let now = Instant::now();
                // ---- f152: full-occlusion hold --------------------------
                // Windows stops compositing a completely covered window, so
                // its picture freezes while its audio would keep playing. The
                // watchdog samples an 8x8 grid of the client rect against the
                // z-order above this window; after 300 ms of complete coverage
                // it applies the SAME hold as a user pause (pacer freezes the
                // picture, audio process suspends, position preserved), but
                // through `control.set_occluded` so clearing it can never
                // release a pause the user asked for. Uncovering the window
                // releases it immediately at the next sample.
                //
                // f183 narrows the trigger: a cover only counts when it is ONE
                // OF THIS PROGRAM'S PLAYER WINDOWS (two overlapping players is
                // the case f152 exists for). Any other window - a fullscreen
                // game, a browser, another video player - is ignored, so the
                // sound keeps playing while the picture is simply not visible.
                let occlusion_due = loops
                    .occlusion_checked
                    .get()
                    .map_or(true, |at| {
                        now.saturating_duration_since(at).as_millis()
                            >= u128::from(OCCLUSION_CHECK_MS)
                    });
                if occlusion_due {
                    let last_check = loops.occlusion_checked.get();
                    loops.occlusion_checked.set(Some(now));
                    if let Some(hwnd) = win::hwnd_of(player.window()) {
                        // A player that has not presented its first frame yet
                        // has nothing to freeze (its sound is still held silent
                        // until that frame), and the window may still be in its
                        // pre-show placement. Only a real picture can be
                        // "paused by occlusion".
                        //
                        // f183: the only covers that count are the app's own
                        // player windows. Another program - a fullscreen game,
                        // a browser, a video player - covering this window is
                        // explicitly NOT a reason to pause: the sound must keep
                        // playing even though the picture is not visible.
                        win::register_player_window(hwnd);
                        let players = win::player_windows();
                        let covered = win::is_fully_covered(hwnd, &players);
                        if covered && control.presented_frames() > 0 {
                            // The pause receipt names the covering players; only
                            // a fully covered window needs this second walk.
                            let occluders =
                                win::covering_player_windows(hwnd, &players).unwrap_or_default();
                            let since = loops.occluded_since.get().unwrap_or(now);
                            loops.occluded_since.set(Some(since));
                            if !loops.occluded_hold.get()
                                && now.saturating_duration_since(since).as_millis()
                                    >= u128::from(OCCLUSION_PAUSE_DELAY_MS)
                            {
                                control.set_occluded(true);
                                loops.occluded_hold.set(true);
                                loops.occluded_at.set(Some(now));
                                OCCLUDED_PAUSES.fetch_add(1, Ordering::Relaxed);
                                let audio_pid = control.audio_pid();
                                crate::debug_log(&format!(
                                    "player occluded pause win={} covered_ms={} occluders={occluders:?} audio_pid={audio_pid} audio_state={}",
                                    loops.id,
                                    now.saturating_duration_since(since).as_millis(),
                                    audio_state_name(win::audio_session_state(audio_pid)),
                                ));
                            }
                        } else {
                            // `wait_ms` is the detection latency: the time
                            // since the previous z-order sample (<= the check
                            // interval). The release itself is immediate.
                            let wait_ms = last_check
                                .map(|at| now.saturating_duration_since(at).as_millis())
                                .unwrap_or(0);
                            loops.occluded_since.set(None);
                            if loops.occluded_hold.get() {
                                loops.occluded_hold.set(false);
                                control.set_occluded(false);
                                let hidden_ms = loops
                                    .occluded_at
                                    .get()
                                    .map(|at| now.saturating_duration_since(at).as_millis())
                                    .unwrap_or(0);
                                loops.occluded_at.set(None);
                                OCCLUDED_RESUMES.fetch_add(1, Ordering::Relaxed);
                                let audio_pid = control.audio_pid();
                                crate::debug_log(&format!(
                                    "player occluded resume win={} wait_ms={wait_ms} hidden_ms={hidden_ms} audio_pid={audio_pid} audio_state={}",
                                    loops.id,
                                    audio_state_name(win::audio_session_state(audio_pid)),
                                ));
                            }
                        }
                    }
                }
                // ---- chrome idle hide ------------------------------------
                // The chrome hides while the pointer is still INSIDE the window
                // after CHROME_IDLE_MS, the classic player behaviour. A 1 px
                // move resets the timer and shows it again immediately.
                //
                // The UI's `chrome_hold` (popover / seek drag / playlist panel)
                // keeps the chrome up exactly like a held button.
                let held = win::left_button_down() || player.get_chrome_hold();
                let mut idle = loops.chrome_idle.get();
                let was_hidden = idle.hidden;
                let visible = idle.poll(win::cursor_px(), inside, held, now);
                loops.chrome_idle.set(idle);
                if idle.hidden && !was_hidden {
                    crate::debug_log(&format!(
                        "chrome idle hide still_ms={} win={}",
                        idle.still_ms(now),
                        loops.id
                    ));
                } else if !idle.hidden && was_hidden && inside {
                    crate::debug_log(&format!(
                        "chrome idle show still_ms={} win={}",
                        idle.still_ms(now),
                        loops.id
                    ));
                }
                if loops.pointer_inside.get() != visible {
                    loops.pointer_inside.set(visible);
                    player.set_pointer_inside(visible);
                    crate::debug_log(&format!(
                        "pointer inside={visible} tag=player win={}",
                        loops.id
                    ));
                }
                // WM_ACTIVATE equivalent without another native hook: the
                // foreground window is polled here (one `GetForegroundWindow`
                // per tick) and the player that owns it becomes the focused
                // one. Clicking a player therefore retargets card_open(0).
                if let Some(state) = state.upgrade() {
                    poll_player_focus(&state, &player);
                }
                let rect = win::window_rect(player.window());
                if rect.is_some() && loops.layout_seen.get() != rect {
                    loops.layout_seen.set(rect);
                    loops.layout_timer.restart();
                }
                // ---- decode target input ---------------------------------
                // The picture is drawn in this window, so its physical size is
                // the resolution worth decoding (fullscreen: the window covers
                // the monitor, so its native rect already IS the screen size).
                // Only transitions reach the control; the decode loop debounces
                // a resize for 400ms and then restarts itself at the current
                // position when the target moved by more than 15%.
                if let Some((_, _, w, h)) = rect {
                    // A minimised window has a degenerate rect: keep the last
                    // real target instead of re-decoding at icon size.
                    let minimized = win::is_minimized(player.window()) == Some(true);
                    let fullscreen = player.get_fullscreen();
                    if !minimized && w > 0 && h > 0 {
                        let size = (w as u32, h as u32, fullscreen);
                        if loops.display_seen.get() != size {
                            loops.display_seen.set(size);
                            control.note_display_size(size.0, size.1, fullscreen);
                            crate::debug_log(&format!(
                                "player display win={} w={} h={} fullscreen={fullscreen}",
                                loops.id, size.0, size.1
                            ));
                        }
                    }
                }
            });
    }
    crate::debug_log(&format!(
        "player loops: pointer watch + layout debounce started win={}",
        session.id
    ));
    loops
}

/// Stop one player window's pointer watch and layout debounce.
///
/// Also drops `pointer_inside` to false, so a closed window can never leave its
/// chrome (or the UI's expectation of it) latched on. Other windows' loops are
/// untouched. Idempotent.
pub(crate) fn stop_player_loops(loops: &Rc<PlayerLoops>) {
    let was_running = loops.pointer_timer.running() || loops.layout_timer.running();
    loops.pointer_timer.stop();
    loops.layout_timer.stop();
    if let Some(player) = loops.window.upgrade() {
        player.set_pointer_inside(false);
        // f183: a closed player must stop being a cover. A handle that is
        // already gone is pruned by `player_windows` on the next read anyway.
        if let Some(hwnd) = win::hwnd_of(player.window()) {
            win::unregister_player_window(hwnd);
        }
    }
    // A hidden/closed window must reopen with the chrome visible: forget the
    // stillness streak and the idle latch.
    loops.chrome_idle.set(ChromeIdle::new(Instant::now()));
    let was_inside = loops.pointer_inside.replace(false);
    if !was_running {
        return;
    }
    if was_inside {
        crate::debug_log(&format!(
            "pointer inside=0 tag=player-close win={}",
            loops.id
        ));
    }
    crate::debug_log(&format!("player loops: stopped win={}", loops.id));
}

/// Log the panel / fullscreen wiring as the UI sees it (QA,
/// `BILI_PANEL_GLASS_TEST`).
pub(crate) fn log_panel_state(player: &PlayerWindow, tag: &str) {
    crate::debug_log(&format!(
        "panel-state tag={tag} fullscreen={} overlay={} left={} right={} right_panel_w={:.0}",
        player.get_fullscreen(),
        player.get_panels_overlay(),
        player.get_left_panel(),
        player.get_right_panel(),
        player.get_right_panel_w(),
    ));
}

/// Retry installing the frameless subclass on the player and mini windows until
/// their native handle exists. Secondary windows are mapped several frames after
/// `show()`, so the retries run on a short repeating timer (event-loop turns fire
/// too early). The hook is idempotent, so the timer simply stops once both
/// windows are closed.
pub(crate) fn retry_subclass(state: &Rc<PlayerState>) {
    let weak = Rc::downgrade(state);
    let mini_logged = Cell::new(false);
    state.frameless_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(1000),
        move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let players = player_sessions(&state);
            let mini = state.mini.borrow().as_ref().map(|w| w.clone_strong());
            if players.is_empty() && mini.is_none() {
                state.frameless_timer.stop();
                return;
            }
            // Every open player, not just the newest: each one installs its own
            // subclass once its HWND exists.
            for session in &players {
                let player = &session.window;
                if session.hwnd.get() == 0 {
                    if let Some(hwnd) = win::hwnd_of(player.window()) {
                        session.hwnd.set(hwnd);
                    }
                }
                // The open path ran before this HWND existed, so register the
                // active window here once it is real. Without this the queue
                // could not hand focus to a surviving window on close.
                let focused_missing = state.focused.borrow().is_none();
                if focused_missing && is_active_player(&session.window) {
                    focus_player(&state, session, false);
                }
                if win::install_subclass(player.window()) {
                    // Compositor alpha + DWM chrome for this HWND (the backdrop
                    // itself is painted by Slint, no capture involved).
                    win::setup_glass(player.window(), state.theme_dark.get());
                    win::log_frame_state(player.window(), "player");
                }
                // Wallpaper parallax on every native move, installed once the
                // handle exists (the call is idempotent per HWND).
                {
                    let had_hook = win::move_hook_registered(player.window());
                    let move_weak = player.as_weak();
                    win::set_move_hook(player.window(), move || {
                        if let Some(player) = move_weak.upgrade() {
                            publish_wallpaper(
                                player.global::<AppTheme>(),
                                player.window(),
                            );
                        }
                    });
                    // The saved player layout is applied while the HWND is still
                    // being mapped, before this hook exists. Publish ONCE here
                    // (the tick that installs the hook), so the first painted
                    // placement is already anchored; after that `WM_MOVE` is the
                    // only trigger - no position polling.
                    if !had_hook {
                        publish_wallpaper(player.global::<AppTheme>(), player.window());
                    }
                }
            }
            match &mini {
                Some(mini) => {
                    if win::install_subclass(mini.window()) && !mini_logged.get() {
                        mini_logged.set(true);
                        win::log_frame_state(mini.window(), "mini");
                    }
                    let had_hook = win::move_hook_registered(mini.window());
                    let move_weak = mini.as_weak();
                    win::set_move_hook(mini.window(), move || {
                        if let Some(mini) = move_weak.upgrade() {
                            publish_wallpaper(mini.global::<AppTheme>(), mini.window());
                        }
                    });
                    // Same one-shot initial placement coverage as the player.
                    if !had_hook {
                        publish_wallpaper(mini.global::<AppTheme>(), mini.window());
                    }
                }
                None => mini_logged.set(false),
            }
        },
    );
}

/// Cursor position inside the window in logical px (the units the UI lays out
/// in), or `None` without a handle / readable cursor.
pub(crate) fn cursor_in_window(window: &slint::Window) -> Option<(f64, f64)> {
    let (wx, wy, _, _) = win::window_rect(window)?;
    let (cx, cy) = win::cursor_px()?;
    let scale = f64::from(window.scale_factor().max(0.2));
    Some(((cx - wx) as f64 / scale, (cy - wy) as f64 / scale))
}

/// `source=<window>:<region>` for the drag lifecycle log.
///
/// The Slint callback carries no hit-test information, so the region is DERIVED
/// from the cursor's window-relative position: the player's top 44px strip and
/// the main window's top 40px strip are the drag strips above the picture,
/// everything else in the player is the picture itself.
pub(crate) fn drag_source(window: &slint::Window, kind: &str) -> String {
    match cursor_in_window(window) {
        Some((_, y)) if kind == "player" && y < 44.0 => "player:title".to_string(),
        Some(_) if kind == "player" => "player:video".to_string(),
        Some((_, y)) if kind == "main" && y < 40.0 => "main:title".to_string(),
        Some(_) => format!("{kind}:other"),
        None => format!("{kind}:-"),
    }
}

/// `x,y` cursor position inside the window, logical px, for the drag log.
pub(crate) fn drag_pos(window: &slint::Window) -> String {
    match cursor_in_window(window) {
        Some((x, y)) => format!("{x:.0},{y:.0}"),
        None => "-".to_string(),
    }
}

/// The state that decides whether a drag can continue, for the drag log.
pub(crate) fn drag_state(window: &slint::Window) -> String {
    format!(
        "button={} inside={} visible={}",
        u8::from(win::left_button_down()),
        u8::from(win::cursor_inside(window).unwrap_or(false)),
        u8::from(win::is_visible(window).unwrap_or(false)),
    )
}

/// `x,y WxH` native window rect, physical px, for the resize logs.
pub(crate) fn resize_rect_text(window: &slint::Window) -> String {
    match win::window_rect(window) {
        Some((x, y, w, h)) => format!("{x},{y} {w}x{h}"),
        None => "-".to_string(),
    }
}

pub(crate) const NET_MAP_SLICES: usize = 120;

/// Slice states, strongest last: a cell shows the strongest state it contains.
/// Downloaded bytes (state 1) are immediately playable and include the point
/// cache, so there is no separate "warm point" state.
pub(crate) const NET_MAP_MISSING: i32 = 0;

pub(crate) const NET_MAP_HELD: i32 = 1;

pub(crate) const NET_MAP_ACTIVE: i32 = 3;

pub(crate) fn codec_name(codecid: u32) -> &'static str {
    match codecid {
        7 => "AVC",
        12 => "HEVC",
        13 => "AV1",
        _ => "-",
    }
}

/// `172 kbps` / `1.6 Mbps`.
pub(crate) fn fmt_bps(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{} kbps", (bps + 500) / 1000)
    }
}

/// Metered upstream throughput for `net_rate` while the prefetch proxy is
/// active: `3.2 MB/s`, no `≈`, because these are counted bytes.
pub(crate) fn fmt_socket_rate(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} MB/s", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.0} KB/s", bps as f64 / 1_000.0)
    } else {
        format!("{bps} B/s")
    }
}

/// `48.0 MB` / `512 KB` / `0 B`, decimal units: the same base as
/// [`fmt_socket_rate`], so a claimed range and a rate read off one screen.
pub(crate) fn fmt_mb(bytes: u64) -> String {
    if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

/// `https://upos-sz-mirrorcos.bilivideo.com:443/` -> the bare host, so the
/// 线程 rows show what they are talking to instead of a URL prefix.
pub(crate) fn trim_host(host: &str) -> &str {
    let trimmed = host.trim();
    let bare = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let bare = bare.split('/').next().unwrap_or(bare);
    bare.split(':').next().unwrap_or(bare)
}

/// 120 equal slices over the whole file. Two passes, in this order:
///
/// 1. an active claim (3) marks its range;
/// 2. everything the session can **serve right now** (1) marks its range,
///    overwriting the active state.
///
/// The second pass is deliberately a force, not a max: a slice with any
/// available byte is playable, so a worker that re-fetches a range already in
/// the memory window must never paint the playhead's slice as "downloading".
/// `available` already merges the disk file, the read-ahead window and the
/// point cache (see [`prefetch::PrefetchStats::available`]), so `downloaded` and
/// `point_cache` are not marked separately.
/// `None`, or a file whose length is not known yet, is all missing.
pub(crate) fn net_map_slices(stats: Option<&prefetch::PrefetchStats>) -> Vec<i32> {
    let mut slices = vec![NET_MAP_MISSING; NET_MAP_SLICES];
    let Some(stats) = stats else {
        return slices;
    };
    let Some(total) = stats.total_bytes.filter(|total| *total > 0) else {
        return slices;
    };
    let slice = total as f64 / NET_MAP_SLICES as f64;
    // Pass 1: active claims.
    let active: Vec<(u64, u64)> = stats
        .workers
        .iter()
        .filter(|worker| worker.busy && worker.end > worker.start)
        .map(|worker| (worker.start, worker.end))
        .collect();
    for (first, last) in net_map_span(&active, slice) {
        for index in first..last.clamp(first + 1, NET_MAP_SLICES) {
            slices[index] = slices[index].max(NET_MAP_ACTIVE);
        }
    }
    // Pass 2: availability wins unconditionally.
    for (first, last) in net_map_span(&stats.available, slice) {
        for index in first..last.clamp(first + 1, NET_MAP_SLICES) {
            slices[index] = NET_MAP_HELD;
        }
    }
    slices
}

/// Map byte ranges to `[first, last)` slice spans for [`net_map_slices`].
pub(crate) fn net_map_span(ranges: &[(u64, u64)], slice: f64) -> Vec<(usize, usize)> {
    ranges
        .iter()
        .filter_map(|(start, end)| {
            (*end > *start).then_some((
                ((*start as f64 / slice).floor() as usize).min(NET_MAP_SLICES - 1),
                (*end as f64 / slice).ceil() as usize,
            ))
        })
        .collect()
}

/// `已下载 48.0 MB / 455.9 MB (10.5%)`. "已下载" is the available set the strip
/// paints (disk + memory window + point cache), not the disk-only figure, so
/// the number and the strip can never disagree. The ranges are already merged,
/// so a point inside the window is counted once.
pub(crate) fn net_total_text(stats: &prefetch::PrefetchStats) -> String {
    let held: u64 = stats
        .available
        .iter()
        .map(|(start, end)| end.saturating_sub(*start))
        .sum();
    match stats.total_bytes.filter(|total| *total > 0) {
        Some(total) => format!(
            "已下载 {} / {} ({:.1}%)",
            fmt_mb(held),
            fmt_mb(total),
            held as f64 * 100.0 / total as f64
        ),
        None => format!("已下载 {}", fmt_mb(held)),
    }
}

/// One `prefetch stats` line per second: rate, totals, the seek warm points
/// and the per-host split. The panel has no spare property for either
/// (`net_bitrate` stays the track's declared bitrate, which is a different,
/// honest number), so this is the diagnostic channel for them.
pub(crate) fn log_prefetch_split(bvid: &str, stats: &prefetch::PrefetchStats) {
    let hosts = stats
        .hosts
        .iter()
        .map(|(host, bytes)| format!("{host}:{:.1}MiB", *bytes as f64 / 1_048_576.0))
        .collect::<Vec<_>>()
        .join(",");
    crate::debug_log(&format!(
        "prefetch stats bvid={bvid} rate={} fetched={} read_ahead={} failed_ranges={} warm={}/{} hits={} hosts=[{hosts}]",
        fmt_socket_rate(stats.bytes_per_sec),
        stats.bytes_fetched,
        stats.read_ahead_bytes,
        stats.failed_ranges,
        stats.warm_points_done,
        stats.warm_points_total,
        stats.point_hits
    ));
}

/// `BILI_BUFFER_TRACE=1`: the first 500 ms of the buffering layer at 50 ms.
///
/// Prints the values the UI actually sees (`buffering` / `buffer_progress` are
/// read back from the window, not recomputed) plus whether a picture exists, so
/// a QA log can prove no frame showed `buffering=false` over an empty picture.
pub(crate) fn trace_buffer_state(
    window: &PlayerWindow,
    control: &player::PlaybackControl,
    last: &Cell<i64>,
    now_ms: i64,
) {
    const TRACE_MS: i64 = 500;
    const TRACE_STEP_MS: i64 = 50;
    if now_ms > TRACE_MS || now_ms - last.get() < TRACE_STEP_MS {
        return;
    }
    last.set(now_ms);
    crate::debug_log(&format!(
        "buffer trace t_ms={now_ms} buffering={} progress={:.2} position_ms={} picture={} paused={}",
        window.get_buffering(),
        window.get_buffer_progress(),
        control.playback_position_ms(),
        window.get_frame().size().width > 0,
        control.paused()
    ));
}

/// Start the sampler once. Never blocks and never fails loudly.
pub(crate) fn start_gpu_sampler() {
    GPU_SAMPLER.get_or_init(|| {
        std::thread::spawn(|| {
            use std::os::windows::process::CommandExt;
            let mut misses = 0u32;
            loop {
                let mut command = std::process::Command::new("nvidia-smi");
                command
                    .arg("--query-gpu=utilization.gpu")
                    .arg("--format=csv,noheader,nounits")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .creation_flags(GPU_CREATE_NO_WINDOW);
                let reading = command.output().ok().and_then(|output| {
                    if !output.status.success() {
                        return None;
                    }
                    let text = String::from_utf8_lossy(&output.stdout);
                    text.lines()
                        .next()
                        .and_then(|line| line.trim().parse::<u32>().ok())
                        .map(|percent| percent.min(100))
                });
                match reading {
                    Some(percent) => {
                        misses = 0;
                        GPU_UTIL.store(u64::from(percent) + 1, Ordering::Relaxed);
                    }
                    None => {
                        // The tool is either absent or the driver is gone; three
                        // failed rounds is enough to stop asking for this run.
                        GPU_UTIL.store(0, Ordering::Relaxed);
                        misses += 1;
                        if misses >= 3 {
                            crate::debug_log("gpu sampler stop reason=nvidia_smi_unavailable");
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(GPU_SAMPLE_MS));
            }
        });
    });
}

/// `GPU 57%`, or `""` while the sampler has no reading.
pub(crate) fn gpu_utilization_text() -> String {
    match GPU_UTIL.load(Ordering::Relaxed) {
        0 => String::new(),
        raw => format!("GPU {}%", raw - 1),
    }
}

/// Media-time step a single fast frame may advance. A stall can cost the
/// comments at most this much travel in one frame; the media clock is absolute,
/// so the next frames continue from where the track actually is.
pub(crate) const DANMAKU_FRAME_MAX_STEP_MS: f32 = 100.0;

/// A slow tick further than this from the media time on screen is a seek or a
/// new track, not a stall: re-anchor there instead of crawling.
pub(crate) const DANMAKU_FRAME_RESYNC_MS: f32 = 300.0;

/// Media milliseconds between two fast frames at `hz`.
pub(crate) fn danmaku_frame_interval_ms(hz: u32) -> f32 {
    1_000.0 / hz.clamp(30, 120) as f32
}

/// The interval handed to the Slint `Timer`.
///
/// Slint stores a `duration` as whole milliseconds (the generated code calls
/// `Duration::from_millis`), so 120 Hz - 8.333 ms - can only wake every 8 ms.
/// The exact-rate gate in [`DanmakuLayer::frame_tick`] drops that surplus wake
/// instead of letting the lane run 4 % fast.
pub(crate) fn danmaku_wake_ms(hz: u32) -> i64 {
    (1_000 / i64::from(hz.clamp(30, 120))).max(1)
}

/// The display's refresh rate, detected once.
///
/// The active display mode answers first (`EnumDisplaySettingsW`), the screen DC
/// second (`GetDeviceCaps(VREFRESH)`), and 60 is the floor: a driver that
/// reports 0/1 for "hardware default" must not turn into "no frames".
pub(crate) fn display_refresh_hz() -> u32 {
    static REFRESH: OnceLock<u32> = OnceLock::new();
    *REFRESH.get_or_init(|| {
        use windows_sys::Win32::Graphics::Gdi::{
            EnumDisplaySettingsW, GetDC, GetDeviceCaps, ReleaseDC, DEVMODEW, ENUM_CURRENT_SETTINGS,
            VREFRESH,
        };
        let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        let mut hz = 0u32;
        let mut source = "fallback";
        let read = unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) };
        if read != 0 && mode.dmDisplayFrequency > 1 {
            hz = mode.dmDisplayFrequency as u32;
            source = "EnumDisplaySettings";
        }
        if hz == 0 {
            let dc = unsafe { GetDC(std::ptr::null_mut()) };
            if !dc.is_null() {
                let caps = unsafe { GetDeviceCaps(dc, VREFRESH as i32) };
                unsafe { ReleaseDC(std::ptr::null_mut(), dc) };
                if caps > 1 {
                    hz = caps as u32;
                    source = "GetDeviceCaps";
                }
            }
        }
        if hz == 0 {
            hz = 60;
        }
        crate::debug_log(&format!(
            "danmaku frame display refresh_hz={hz} source={source}"
        ));
        hz
    })
}

/// The fast lane's rate: `BILI_DANMAKU_HZ` verbatim (30/60/120, the A/B), else
/// `min(120, display)` so a 144 Hz panel runs 120 and a 60 Hz panel is never
/// asked for frames it cannot show.
pub(crate) fn danmaku_frame_hz(display_hz: u32) -> (u32, &'static str) {
    let raw = std::env::var("BILI_DANMAKU_HZ").ok();
    danmaku_frame_hz_for(display_hz, raw.as_deref())
}

/// [`danmaku_frame_hz`] with the environment passed in, for the self-test.
pub(crate) fn danmaku_frame_hz_for(display_hz: u32, env: Option<&str>) -> (u32, &'static str) {
    if let Some(raw) = env {
        match raw.trim().parse::<u32>() {
            Ok(value) if matches!(value, 30 | 60 | 120) => return (value, "env"),
            Ok(value) => crate::debug_log(&format!(
                "danmaku hz env ignored value={value} allowed=30,60,120"
            )),
            Err(_) => {}
        }
    }
    (display_hz.clamp(30, 120), "display")
}

/// px per ms a scrolling comment travels, from the engine's own formula
/// (`(VIDEO_W + width) / (SCROLL_MS * 100 / speed_percent)`).
pub(crate) fn danmaku_scroll_speed(width: f32, speed_percent: u32) -> f32 {
    (DANMAKU_VIDEO_W + width) * speed_percent.clamp(50, 200) as f32 / (DANMAKU_SCROLL_MS * 100.0)
}

/// Whether an x is where a 顶部/底部 comment is drawn: dead centre.
pub(crate) fn danmaku_is_centred(x: f32, width: f32) -> bool {
    (x - (DANMAKU_VIDEO_W - width) / 2.0).abs() < 0.5
}
