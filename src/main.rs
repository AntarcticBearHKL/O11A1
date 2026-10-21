#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bilibili QR-code login desktop client.
//!
//! All network work runs on a background thread (its own tokio runtime) and
//! pushes results to the UI through `slint::invoke_from_event_loop`. The UI
//! thread is never blocked. A generation counter makes stale attempts inert.

mod accounts;
mod autostart;
mod bilibili;
mod danmaku;
mod download;
mod glass;
mod job;
mod player;
#[allow(dead_code)] mod prefetch; // the player wires it in; QA helpers stay unused in a normal build
mod settings;
mod theme;
mod tray;
mod wbi;
mod win;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures::StreamExt;
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel, Weak};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsZoomed, SetWindowPos, SWP_NOACTIVATE,
    SWP_NOZORDER,
};

slint::include_modules!();

/// Number of `poll` iterations (~90 * 2s = 180s window).
const MAX_POLLS: usize = 90;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Cover fetches kept in flight per list. Kept low so cover decoding (which now
/// runs on the blocking pool) never floods the CPU or the shared runtime while
/// the player is decoding video; the decoder no longer blocks the async workers.
const THUMB_CONCURRENCY: usize = 6;

/// Cover fetches in flight for the preload ring's warm-up. Matches
/// [`THUMB_CONCURRENCY`]: a page of covers then lands in ~2 s on a typical CDN,
/// inside [`COVER_WARM_BUDGET`], instead of trickling past it.
const COVER_WARM_IN_FLIGHT: usize = 6;
/// Covers warmed per ring batch: one client page (see
/// [`feed_cover_warm_target`]). Never more than a 24-card page, which also
/// bounds what the ring can pin in memory.
const COVER_WARM_MAX: usize = 24;
/// A small margin past the page, so the first wheel nudge already has covers.
const COVER_WARM_MARGIN: usize = 4;
/// Wall-clock budget for one batch's warm-up. A slow CDN must not push the
/// startup wait past [`FEED_HOME_WAIT`]: whatever landed by the deadline is
/// attached, the rest streams after promotion exactly as before.
const COVER_WARM_BUDGET: Duration = Duration::from_millis(2500);

/// Bound for the decoded-cover cache both the streaming decoder and the
/// warm-up read: entries and pixel bytes. Eviction is least-recently-used.
const COVER_CACHE_ENTRIES: usize = 128;
const COVER_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Feed source of the preload ring: 0 = 首页推荐. The ring is tagged so a second
/// source can be added later without touching the promotion path.
const FEED_SOURCE_RECOMMEND: i32 = 0;
/// A ready batch older than this is dropped (bounded memory) and refetched.
const FEED_PREFETCH_TTL: Duration = Duration::from_secs(10 * 60);
/// How often the background filler looks for work (and for a session).
const FEED_PREFETCH_POLL: Duration = Duration::from_millis(250);
/// The startup path waits this long for the ring's first batch before showing
/// the home view anyway.
const FEED_HOME_WAIT: Duration = Duration::from_secs(6);
/// The `fresh_idx` cursor the startup/refresh load has always used.
const FEED_START_CURSOR: u64 = 1;

/// Number of player windows currently open (or opening). Main-window background
/// work — incremental cover fetch/patch, the skeleton shimmer and the
/// frosted-glass refresh loop — is suspended while it is non-zero so nothing
/// competes with playback for CPU, the UI thread or DWM.
///
/// A counter rather than a flag: several player windows can be open at once, so
/// closing one must not resume the main window while another still plays.
/// Bumped in `open_player_inner` and decremented on every player close path.
static PLAYER_OPEN_COUNT: AtomicUsize = AtomicUsize::new(0);

fn player_active() -> bool {
    PLAYER_OPEN_COUNT.load(Ordering::SeqCst) > 0
}

/// One more player window owns playback.
fn player_opened() {
    PLAYER_OPEN_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// One player window is gone; the main window resumes only at zero.
fn player_closed() {
    let _ = PLAYER_OPEN_COUNT.try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
        Some(count.saturating_sub(1))
    });
}

/// The one live session, shared with the player worker threads.
type SessionSlot = Arc<Mutex<Option<Arc<bilibili::Session>>>>;

/// Region list offered by 分区浏览 and the 排行榜 selector (番剧/直播 excluded).
const REGIONS: &[(&str, i64)] = &[
    ("动画", 1),
    ("国创", 167),
    ("音乐", 3),
    ("舞蹈", 129),
    ("游戏", 4),
    ("知识", 36),
    ("科技", 188),
    ("运动", 234),
    ("汽车", 223),
    ("生活", 160),
    ("美食", 211),
    ("动物圈", 217),
    ("鬼畜", 119),
    ("时尚", 155),
    ("娱乐", 5),
    ("影视", 181),
    ("纪录片", 177),
    ("电影", 23),
    ("电视剧", 11),
];

fn region_labels() -> Vec<SharedString> {
    REGIONS
        .iter()
        .map(|(name, _)| SharedString::from(*name))
        .collect()
}

fn rank_region_labels() -> Vec<SharedString> {
    let mut labels = Vec::with_capacity(REGIONS.len() + 1);
    labels.push(SharedString::from("全站"));
    for (name, _) in REGIONS {
        labels.push(SharedString::from(*name));
    }
    labels
}

/// Append `line` to the file named by `BILI_DEBUG_LOG`, when set. Diagnostic
/// only: never writes to stdout and is inert unless the env var is present.
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
        let _ = writeln!(file, "{line}");
    }
}

// ========================== 提示条 (toast queue) ==========================
//
// The player's transient user-facing messages now live in a tiny queue instead
// of the (inert) `player_status` / `interaction_status` strings. The UI draws
// at most three, newest at the BOTTOM, so the queue is kept oldest-first and
// published in that order. Each entry lives [`TOAST_LIFETIME`]; the one shared
// 3px hairline (`toast_progress`) follows the OLDEST visible toast.
//
// Everything here runs on the UI thread: callbacks add a toast, the player's
// existing 33ms tick calls [`toast_tick`], which only publishes at the
// [`TOAST_TICK`] cadence. Queues are per player window (several may be open),
// looked up by native handle and pruned once a window is gone.

const TOAST_LIFETIME: Duration = Duration::from_millis(2800);
const TOAST_MAX_VISIBLE: usize = 3;
const TOAST_TICK: Duration = Duration::from_millis(250);
pub(crate) const TOAST_KIND_INFO: i32 = 0;
pub(crate) const TOAST_KIND_SUCCESS: i32 = 1;
pub(crate) const TOAST_KIND_ERROR: i32 = 2;

/// One queued toast. `born` is when it entered the queue, not when it first
/// became visible: every entry keeps its own 2.8s lifetime.
struct ToastEntry {
    text: String,
    kind: i32,
    born: Instant,
}

/// Per-player-window queue plus the model the UI renders. `hwnd` is the
/// lookup key (Slint's `Weak` has no pointer identity); `window` keeps the
/// weak handle, so a queue whose player is gone is pruned before it can be
/// mistaken for a recycled HWND.
struct ToastQueue {
    window: Weak<PlayerWindow>,
    hwnd: isize,
    /// Oldest first; the UI reverses it so the newest sits at the bottom.
    items: VecDeque<ToastEntry>,
    model: Rc<VecModel<ToastItem>>,
    last_tick: Instant,
}

thread_local! {
    static TOAST_QUEUES: RefCell<Vec<ToastQueue>> = RefCell::new(Vec::new());
}

/// Remaining fraction of the oldest visible toast's lifetime (1.0 at birth,
/// 0.0 at expiry); 1.0 while the queue is empty, so the next toast starts full.
fn toast_progress(queue: &ToastQueue, now: Instant) -> f32 {
    let Some(oldest) = queue.items.front() else {
        return 1.0;
    };
    let elapsed = now.saturating_duration_since(oldest.born).as_secs_f32();
    (1.0 - elapsed / TOAST_LIFETIME.as_secs_f32()).clamp(0.0, 1.0)
}

/// The queue as the UI's model: oldest first, newest last.
fn toast_model_items(queue: &ToastQueue) -> Vec<ToastItem> {
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
fn toast(player: &PlayerWindow, text: impl Into<String>, kind: i32) {
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
fn toast_tick(player: &PlayerWindow) {
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
fn toast_forget(player: &PlayerWindow) {
    let Some(hwnd) = win::hwnd_of(player.window()) else {
        return;
    };
    TOAST_QUEUES.with(|cell| {
        if let Ok(mut queues) = cell.try_borrow_mut() {
            queues.retain(|queue| queue.hwnd != hwnd);
        }
    });
}

thread_local! {
    /// The player window that is currently on screen (the newest opened, or
    /// `None` after both close paths). `set_status` runs its toast on the UI
    /// thread but only knows its own `Weak`; this is the one the user sees.
    static ACTIVE_PLAYER: RefCell<Weak<PlayerWindow>> = RefCell::new(Weak::default());
    /// Last status toast, so a status that repeats within a second (e.g. the
    /// same error twice from a retry) is not shown twice for one event.
    static LAST_STATUS_TOAST: RefCell<Option<(String, Instant)>> = RefCell::new(None);
}

/// Remember the player window that is now on screen. Called once per open,
/// before `show()`'s first frame is drawn.
fn set_active_player(player: &PlayerWindow) {
    ACTIVE_PLAYER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = player.as_weak();
        }
    });
}

/// Forget the active player after a close, so a late worker message cannot land
/// a toast on a window that is already gone.
fn clear_active_player() {
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
fn active_player_hwnd() -> Option<isize> {
    ACTIVE_PLAYER
        .with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|slot| slot.upgrade())
        })
        .and_then(|player| win::hwnd_of(player.window()))
}

fn is_active_player(player: &PlayerWindow) -> bool {
    match (win::hwnd_of(player.window()), active_player_hwnd()) {
        (Some(current), Some(active)) => current == active,
        _ => false,
    }
}

/// The OS foreground window (`0` when none). Used by the player pointer watch
/// to follow activation without installing another native hook.
fn foreground_hwnd() -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow() as isize }
}

/// Make `session` the focused player: the window card_open(0) replaces, and the
/// one toasts, the tray boss key and the queue's end-of-track follow.
///
/// `notify_playlist` hands it the queue's "now playing" block (a user click on
/// an older window). At open time the queue already learned the video through
/// `Playlist::on_open`, so that path passes `false`.
fn focus_player(state: &Rc<PlayerState>, session: &Rc<PlayerSession>, notify_playlist: bool) {
    let hwnd = session.hwnd.get();
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
    if notify_playlist {
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
fn poll_player_focus(state: &Rc<PlayerState>, player: &PlayerWindow) {
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
fn place_player_window(player: &PlayerWindow, x: i32, y: i32) -> bool {
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

/// Put `player` so the stationary cursor at `(cx, cy)` is inside it, and move
/// `other` far away so a transition can only come from the window being moved.
fn window_probe_step(
    step: &str,
    cursor: (i32, i32),
    old: Option<&Rc<PlayerSession>>,
    new: Option<&Rc<PlayerSession>>,
) {
    let (cx, cy) = cursor;
    let under = |session: &Rc<PlayerSession>| {
        let Some((_, _, w, h)) = win::window_rect(session.window.window()) else {
            return;
        };
        // Keep the whole window inside the cursor's work area, then the cursor
        // sits 120/90 px into it (still inside unless it is on the last edge
        // pixel of the screen).
        let (wx, wy, ww, wh) = win::work_area_for_rect(cx, cy, 1, 1).unwrap_or((0, 0, cx + w, cy + h));
        let x = (cx.saturating_sub(120)).clamp(wx, wx.saturating_add(ww.saturating_sub(w)));
        let y = (cy.saturating_sub(90)).clamp(wy, wy.saturating_add(wh.saturating_sub(h)));
        place_player_window(&session.window, x, y);
    };
    let away = |session: &Rc<PlayerSession>| {
        let Some((_, _, w, h)) = win::window_rect(session.window.window()) else {
            return;
        };
        // The corner of the work area farthest from the cursor guarantees the
        // stationary cursor is OUTSIDE the moved window (window is far smaller
        // than the desktop's half).
        let (wx, wy, ww, wh) =
            win::work_area_for_rect(cx, cy, 1, 1).unwrap_or((0, 0, cx.saturating_add(w), cy.saturating_add(h)));
        let x = if cx < wx.saturating_add(ww / 2) {
            wx.saturating_add(ww.saturating_sub(w))
        } else {
            wx
        };
        let y = if cy < wy.saturating_add(wh / 2) {
            wy.saturating_add(wh.saturating_sub(h))
        } else {
            wy
        };
        place_player_window(&session.window, x, y);
    };
    match step {
        "old-under" => {
            if let Some(new) = new {
                away(new);
            }
            if let Some(old) = old {
                under(old);
            }
        }
        "old-away" => {
            if let Some(old) = old {
                away(old);
            }
        }
        "old-back" => {
            if let Some(new) = new {
                away(new);
            }
            if let Some(old) = old {
                under(old);
            }
        }
        "new-under" => {
            if let Some(old) = old {
                away(old);
            }
            if let Some(new) = new {
                under(new);
            }
        }
        _ => {}
    }
    let rects: Vec<String> = old
        .into_iter()
        .chain(new)
        .map(|session| {
            win::window_rect(session.window.window())
                .map(|(wx, wy, ww, wh)| format!("win{}={wx},{wy},{ww}x{wh}", session.id))
                .unwrap_or_else(|| format!("win{}=-", session.id))
        })
        .collect();
    crate::debug_log(&format!(
        "window probe step={step} cursor={cx},{cy} foreground={:#x} rects=[{}]",
        foreground_hwnd(),
        rects.join(" ")
    ));
}

thread_local! {
    /// The window-move probe runs at most once per process.
    static WINDOW_PROBE_STARTED: Cell<bool> = Cell::new(false);
    /// Keeps the probe's timer alive (the closure holds it weakly, like the
    /// player loops, so it stops itself after the last step).
    static WINDOW_PROBE_TIMER: RefCell<Option<Rc<slint::Timer>>> = RefCell::new(None);
}

/// Prove the per-window pointer watch with two open windows without touching
/// the cursor (`BILI_WINDOW_PROBE_TEST=<gap_ms>`).
///
/// The cursor stays where the user left it; only WINDOW RECTS move
/// (`SetWindowPos`, no activation): old window under the cursor → away → under
/// again (the older window's chrome must come back) → new window under →
/// restore. Nothing here moves the cursor or sends input.
fn maybe_start_window_probe(state: &Rc<PlayerState>) {
    if state.players.borrow().len() < 2 || WINDOW_PROBE_STARTED.with(|flag| flag.get()) {
        return;
    }
    let Ok(value) = std::env::var("BILI_WINDOW_PROBE_TEST") else {
        return;
    };
    let gap = value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|ms| *ms > 0)
        .unwrap_or(1500)
        .clamp(300, 20_000);
    let Some(cursor) = win::cursor_px() else {
        crate::debug_log("window probe skipped reason=no-cursor");
        return;
    };
    WINDOW_PROBE_STARTED.with(|flag| flag.set(true));
    const STEPS: [&str; 5] = [
        "old-under",
        "old-away",
        "old-back",
        "new-under",
        "restore",
    ];
    let state = state.clone();
    let index = Rc::new(Cell::new(0usize));
    let originals: Rc<RefCell<Vec<(u64, WinRect)>>> = Rc::new(RefCell::new(Vec::new()));
    let timer = Rc::new(slint::Timer::default());
    WINDOW_PROBE_TIMER.with(|held| *held.borrow_mut() = Some(timer.clone()));
    let tick_state = state.clone();
    let tick_index = index.clone();
    let tick_originals = originals.clone();
    let tick_timer = Rc::downgrade(&timer);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(gap),
        move || {
            let Some(timer) = tick_timer.upgrade() else {
                return;
            };
            let step = tick_index.get();
            let Some(name) = STEPS.get(step) else {
                timer.stop();
                crate::debug_log("window probe done");
                return;
            };
            let sessions = player_sessions(&tick_state);
            if sessions.len() < 2 {
                timer.stop();
                crate::debug_log("window probe stopped reason=player-closed");
                return;
            }
            if step == 0 {
                let mut saved = tick_originals.borrow_mut();
                for session in &sessions {
                    if let Some((x, y, w, h)) = win::window_rect(session.window.window()) {
                        let scale = session.window.window().scale_factor().max(0.2);
                        let (extra_w, extra_h) = match session.panel_geom.try_borrow() {
                            Ok(geom) => {
                                panel_extras(geom.left, geom.right, geom.bottom, scale)
                            }
                            Err(_) => (0, 0),
                        };
                        saved.push((
                            session.id,
                            WinRect {
                                x,
                                y,
                                w: w.saturating_sub(extra_w).max(1),
                                h: h.saturating_sub(extra_h).max(1),
                            },
                        ));
                    }
                }
            }
            let old = sessions.first();
            let new = sessions.last();
            if *name == "restore" {
                // The rects captured at step 0 are the window's BASE rects.
                // Restore the base plus whatever panel strips the UI has opened
                // in the meantime, so `player_base_layout` derives the same base
                // back instead of eating the panel width.
                for (id, rect) in tick_originals.borrow().iter() {
                    if let Some(session) = sessions.iter().find(|session| session.id == *id) {
                        let scale = session.window.window().scale_factor().max(0.2);
                        let (extra_w, extra_h) = match session.panel_geom.try_borrow() {
                            Ok(geom) => {
                                panel_extras(geom.left, geom.right, geom.bottom, scale)
                            }
                            Err(_) => (0, 0),
                        };
                        set_player_window_rect(
                            session.window.window(),
                            WinRect {
                                x: rect.x,
                                y: rect.y,
                                w: rect.w.saturating_add(extra_w),
                                h: rect.h.saturating_add(extra_h),
                            },
                        );
                    }
                }
                crate::debug_log(&format!(
                    "window probe step=restore windows={} restored=1",
                    sessions.len()
                ));
            } else {
                window_probe_step(name, cursor, old, new);
            }
            tick_index.set(step + 1);
        },
    );
}

// ========================== 播放列表 (playback queue) ==========================
//
// One queue for the whole app, owned by the main window's home bar. The queue
// is UI-thread state (like the toast queues): every mutation comes from a Slint
// callback or the player's own 33ms tick, so no lock is needed. Persistence goes
// through `settings.json` (`settings::QueueEntry`), restored at launch WITHOUT
// auto-playing: the panel and the bar show what was queued, nothing starts
// until the user asks.
//
// Modes: 0 = 手动 (stop at the end of a track), 1 = 自动 (next in queue order),
// 2 = 随机 (a random not-yet-played item; `played` is reset when it covers the
// queue). `play_next` / `play_prev` always walk the queue in order - only the
// automatic end-of-track advance is random.

/// One queued video; fields mirror `settings::QueueEntry` plus what the player
/// needs to open it.
#[derive(Clone, Debug)]
struct QueueEntry {
    bvid: String,
    cid: u64,
    aid: i64,
    title: String,
    owner: String,
    duration: String,
    /// Cover URL exactly as the card handed it out (empty = no cover).
    cover: String,
}

impl QueueEntry {
    fn from_feed(video: &bilibili::FeedVideo) -> Self {
        Self {
            bvid: video.bvid.clone(),
            cid: video.cid,
            aid: video.aid as i64,
            title: video.title.clone(),
            owner: video.owner.clone(),
            duration: format_duration(video.duration),
            cover: video.pic.clone(),
        }
    }

    fn from_settings(entry: &settings::QueueEntry) -> Self {
        Self {
            bvid: entry.bvid.clone(),
            cid: entry.cid,
            aid: entry.aid,
            title: entry.title.clone(),
            owner: entry.owner.clone(),
            duration: entry.duration.clone(),
            cover: entry.cover.clone(),
        }
    }

    fn to_settings(&self) -> settings::QueueEntry {
        settings::QueueEntry {
            bvid: self.bvid.clone(),
            cid: self.cid,
            aid: self.aid,
            title: self.title.clone(),
            owner: self.owner.clone(),
            duration: self.duration.clone(),
            cover: self.cover.clone(),
        }
    }

    /// The video as the existing open-player path expects it. The duration
    /// label is parsed back to seconds so the info header's stats still read
    /// correctly; anything unparseable becomes 0 (the header then omits it).
    fn to_video(&self) -> bilibili::FeedVideo {
        bilibili::FeedVideo {
            bvid: self.bvid.clone(),
            cid: self.cid,
            aid: self.aid.max(0) as u64,
            title: self.title.clone(),
            pic: self.cover.clone(),
            owner: self.owner.clone(),
            owner_mid: 0,
            view: 0,
            duration: duration_seconds(&self.duration),
            pub_ts: 0,
        }
    }
}

/// Parse a `MM:SS` / `HH:MM:SS` duration label back to seconds.
fn duration_seconds(label: &str) -> u64 {
    let mut total = 0u64;
    for part in label.split(':') {
        let Ok(value) = part.trim().parse::<u64>() else {
            return 0;
        };
        total = total.saturating_mul(60).saturating_add(value);
    }
    total
}

/// The queue itself: pure data and index arithmetic, so it can be unit-tested
/// without a UI.
struct PlaybackQueue {
    items: Vec<QueueEntry>,
    current: Option<usize>,
    mode: u8,
    /// Indices shuffle mode has already auto-advanced to; reset when it covers
    /// the whole queue.
    played: HashSet<usize>,
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            current: None,
            mode: settings::default_play_mode() as u8,
            played: HashSet::new(),
        }
    }
}

impl PlaybackQueue {
    /// Append one entry. Returns `false` when the bvid is already queued (the
    /// same video must never occupy two rows). At the cap the OLDEST entry is
    /// dropped and every index (current, played) shifts with it.
    fn push(&mut self, entry: QueueEntry) -> bool {
        if self.items.iter().any(|item| item.bvid == entry.bvid) {
            return false;
        }
        if self.items.len() >= settings::PLAYLIST_MAX {
            self.items.remove(0);
            self.shift_after_drop(0);
        }
        self.items.push(entry);
        true
    }

    /// Remove the entry at `index`; current and the shuffle set follow it.
    fn remove(&mut self, index: usize) -> Option<QueueEntry> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        self.shift_after_drop(index);
        Some(removed)
    }

    /// Drop the oldest entries so the queue stays inside the cap.
    fn shift_after_drop(&mut self, dropped: usize) {
        self.current = match self.current {
            Some(current) if current == dropped => None,
            Some(current) if current > dropped => Some(current - 1),
            other => other,
        };
        self.played = self
            .played
            .iter()
            .filter_map(|index| match (*index).cmp(&dropped) {
                std::cmp::Ordering::Less | std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(*index - 1),
            })
            .collect();
        if self.items.is_empty() {
            self.current = None;
        }
    }

    fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.played.clear();
    }

    /// The next/previous index in queue order, wrapping. `None` on an empty
    /// queue; with no current item it starts at the first (next) or last
    /// (previous) entry.
    fn step(&self, forward: bool) -> Option<usize> {
        let len = self.items.len();
        if len == 0 {
            return None;
        }
        Some(match self.current {
            Some(current) if forward => (current + 1) % len,
            Some(current) => (current + len - 1) % len,
            None if forward => 0,
            None => len - 1,
        })
    }

    /// Indices shuffle mode may pick: everything not auto-played yet and not
    /// the item that just ended. When that set is empty the whole queue has
    /// been played, so `played` is reset and the same rule is retried; a queue
    /// with a single item then replays it, exactly like mode 1 wraps.
    fn random_candidates(&mut self) -> Vec<usize> {
        let pick = |queue: &PlaybackQueue| {
            (0..queue.items.len())
                .filter(|index| !queue.played.contains(index) && Some(*index) != queue.current)
                .collect::<Vec<_>>()
        };
        let mut candidates = pick(self);
        if candidates.is_empty() && !self.items.is_empty() {
            self.played.clear();
            candidates = pick(self);
            if candidates.is_empty() {
                candidates = self.step(true).into_iter().collect();
            }
        }
        candidates
    }

    /// Publish the mode as the `play_mode` property expects it.
    fn mode_value(&self) -> i32 {
        i32::from(self.mode)
    }
}

/// What the bar's vinyl and title show: independent of the queue's `current`,
/// because a card opened directly from the feed also plays.
struct NowPlaying {
    bvid: String,
    title: String,
    cover: String,
}

/// The app-wide queue host: model, persistence and all the side effects the
/// pure [`PlaybackQueue`] must not know about (windows, covers, logs).
struct Playlist {
    queue: RefCell<PlaybackQueue>,
    ui: Weak<AppWindow>,
    state: std::rc::Weak<PlayerState>,
    session: SessionSlot,
    settings: Arc<Mutex<settings::Settings>>,
    feed: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    browse_bvids: Arc<Mutex<Vec<String>>>,
    space_bvids: Arc<Mutex<Vec<String>>>,
    now: RefCell<Option<NowPlaying>>,
    /// The active player reports "playing" (open and not paused).
    playing: Cell<bool>,
    /// A track ended in mode 0: nothing plays until the user starts something.
    stopped: Cell<bool>,
    /// The end of the current item was already handled, so one track can never
    /// advance the queue twice.
    end_handled: Cell<bool>,
    last_tick: Cell<Instant>,
    /// xorshift state for shuffle mode (no new dependency).
    rng: Cell<u64>,
}

thread_local! {
    static PLAYLIST: RefCell<Option<Rc<Playlist>>> = RefCell::new(None);
}

fn playlist() -> Option<Rc<Playlist>> {
    PLAYLIST.with(|cell| cell.try_borrow().ok().and_then(|slot| slot.clone()))
}

fn install_playlist(host: &Rc<Playlist>) {
    PLAYLIST.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = Some(host.clone());
        }
    });
}

impl Playlist {
    fn new(
        ui: Weak<AppWindow>,
        state: std::rc::Weak<PlayerState>,
        session: SessionSlot,
        settings: Arc<Mutex<settings::Settings>>,
        feed: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
        browse_bvids: Arc<Mutex<Vec<String>>>,
        space_bvids: Arc<Mutex<Vec<String>>>,
    ) -> Rc<Self> {
        let restored = settings
            .lock()
            .map(|store| (store.playlist.clone(), store.playlist_current, store.play_mode))
            .unwrap_or_else(|_| (Vec::new(), None, settings::default_play_mode()));
        let (items, current) = settings::normalise_playlist_state(restored.0, restored.1);
        let mode = settings::normalise_play_mode(restored.2) as u8;
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|age| age.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let host = Rc::new(Self {
            queue: RefCell::new(PlaybackQueue {
                items: items.iter().map(QueueEntry::from_settings).collect(),
                current,
                mode,
                played: HashSet::new(),
            }),
            ui,
            state,
            session,
            settings,
            feed,
            browse_bvids,
            space_bvids,
            now: RefCell::new(None),
            playing: Cell::new(false),
            stopped: Cell::new(false),
            end_handled: Cell::new(true),
            last_tick: Cell::new(Instant::now()),
            rng: Cell::new(seed | 1),
        });
        let len = host.queue.borrow().items.len();
        if len > 0 {
            crate::debug_log(&format!(
                "playlist restore items={len} current={:?} mode={mode} autoplay=0",
                host.queue.borrow().current
            ));
        } else {
            crate::debug_log(&format!("playlist restore items=0 current=none mode={mode}"));
        }
        host.publish();
        host
    }

    /// The main window's current view (0 = 首页推荐 feed).
    fn view_mode(&self) -> i32 {
        self.ui
            .upgrade()
            .map(|ui| ui.get_view_mode())
            .unwrap_or(0)
    }

    fn mode(&self) -> u8 {
        self.queue.borrow().mode
    }

    fn current(&self) -> Option<usize> {
        self.queue.borrow().current
    }

    fn has_bvid(&self, bvid: &str) -> bool {
        self.queue.borrow().items.iter().any(|item| item.bvid == bvid)
    }

    /// Diagnostic snapshot: the queue's titles, current index and mode.
    fn snapshot(&self) -> (Vec<String>, Option<usize>, u8) {
        let queue = self.queue.borrow();
        (
            queue.items.iter().map(|entry| entry.bvid.clone()).collect(),
            queue.current,
            queue.mode,
        )
    }

    /// A cover for `url` if it is already decoded; `Image::default()` (none)
    /// otherwise. Never fetches - `spawn_cover` does that off-thread.
    fn cover_image(&self, url: &str) -> Image {
        if url.is_empty() {
            return Image::default();
        }
        match cover_cache().lock() {
            Ok(mut cache) => cache
                .get(url)
                .map(|pixels| to_thumb_shared(&pixels))
                .unwrap_or_default(),
            Err(_) => Image::default(),
        }
    }

    /// Publish rows, the bar's now-playing block and the mode.
    ///
    /// Event-driven (enqueue/remove/clear/play/pause/end/cover), never per
    /// frame: the row model is rebuilt as a whole, which is cheap at 200 rows
    /// and keeps the `current`/`playing` flags trivially consistent.
    fn publish(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let playing = self.playing.get();
        let (rows, current, mode, len) = {
            let queue = self.queue.borrow();
            let rows: Vec<PlaylistRow> = queue
                .items
                .iter()
                .enumerate()
                .map(|(index, entry)| PlaylistRow {
                    title: entry.title.clone().into(),
                    owner: entry.owner.clone().into(),
                    duration: entry.duration.clone().into(),
                    cover: self.cover_image(&entry.cover),
                    current: queue.current == Some(index),
                    playing: queue.current == Some(index) && playing,
                })
                .collect();
            (rows, queue.current, queue.mode_value(), queue.items.len())
        };
        // The bar's own title/cover, independent of the queue's current row.
        let (now_bvid, now_title, now_cover) = match self.now.try_borrow() {
            Ok(now) => match now.as_ref() {
                Some(now) => (
                    now.bvid.clone(),
                    now.title.clone(),
                    self.cover_image(&now.cover),
                ),
                None => (String::new(), String::new(), Image::default()),
            },
            Err(_) => (String::new(), String::new(), Image::default()),
        };
        ui.set_playlist(ModelRc::from(Rc::new(VecModel::from(rows))));
        ui.set_play_mode(mode);
        ui.set_now_playing(playing);
        ui.set_now_title(now_title.into());
        ui.set_now_cover(now_cover);
        crate::debug_log(&format!(
            "playlist publish items={len} current={current:?} mode={mode} playing={playing} now={now_bvid}"
        ));
    }

    /// Write queue + current index + mode to `settings.json`.
    ///
    /// Written synchronously (a few KB on a user action) rather than through
    /// [`update_settings`]' off-thread save: a burst of enqueues must land in
    /// order, and a later spawned write finishing first could otherwise drop
    /// the newest items on the next launch. The file lock in `settings::save`
    /// keeps it atomic against the other writers.
    fn persist(&self) {
        let (snapshot, len, current, mode) = {
            let queue = self.queue.borrow();
            let mut snapshot = match self.settings.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            snapshot.playlist = queue.items.iter().map(QueueEntry::to_settings).collect();
            snapshot.playlist_current = queue.current;
            snapshot.play_mode = u32::from(queue.mode);
            (
                snapshot,
                queue.items.len(),
                queue.current,
                u32::from(queue.mode),
            )
        };
        if let Ok(mut guard) = self.settings.lock() {
            *guard = snapshot.clone();
        }
        let _ = settings::save(&snapshot);
        crate::debug_log(&format!(
            "playlist persist items={len} current={current:?} mode={mode}"
        ));
    }

    /// Fetch one cover (cache miss only) and republish when it lands.
    fn spawn_cover(&self, url: String) {
        if url.is_empty() {
            return;
        }
        if let Ok(mut cache) = cover_cache().lock() {
            if cache.get(&url).is_some() {
                return;
            }
        }
        let Some(session) = active_session(&self.session) else {
            return;
        };
        std::thread::spawn(move || {
            let Some(runtime) = bilibili::shared_runtime() else {
                return;
            };
            let Ok(pixels) = runtime.block_on(session.fetch_thumb(&url)) else {
                crate::debug_log(&format!("playlist cover failed url={url}"));
                return;
            };
            if let Ok(mut cache) = cover_cache().lock() {
                cache.put(&url, Arc::new(pixels));
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(host) = playlist() {
                    host.publish();
                }
            });
        });
    }

    /// Queue entry for a card index in the view that is on screen.
    ///
    /// The feed carries cid/aid itself, so it is queued synchronously. The
    /// discovery cards (分区 / 排行 / 热门 / 必刷 / 用户空间) only carry a bvid;
    /// those are resolved through the detail API first, then queued.
    fn enqueue_index(&self, index: i32, only: bool) {
        let index = index.max(0) as usize;
        let view = self.view_mode();
        if view == 0 {
            let video = self
                .feed
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned());
            match video {
                Some(video) => self.insert_entry(QueueEntry::from_feed(&video), only),
                None => crate::debug_log(&format!(
                    "playlist enqueue miss view={view} index={index}"
                )),
            }
            return;
        }
        let bvid = match view {
            11 => self
                .space_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned()),
            8 | 9 | 10 | 18 => self
                .browse_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned()),
            _ => None,
        };
        match bvid {
            Some(bvid) => self.enqueue_bvid(bvid, only),
            None => crate::debug_log(&format!(
                "playlist enqueue miss view={view} index={index}"
            )),
        }
    }

    /// Resolve a bvid from the discovery cards into a full queue entry.
    fn enqueue_bvid(&self, bvid: String, only: bool) {
        if self.has_bvid(&bvid) {
            crate::debug_log(&format!("playlist dup bvid={bvid}"));
            return;
        }
        let Some(session) = active_session(&self.session) else {
            crate::debug_log(&format!("playlist enqueue skipped bvid={bvid} reason=no-session"));
            return;
        };
        let ui = self.ui.clone();
        std::thread::spawn(move || {
            let Some(runtime) = bilibili::shared_runtime() else {
                return;
            };
            let entry = match runtime.block_on(session.video_view(&bvid)) {
                Ok(detail) => QueueEntry {
                    bvid: detail.bvid,
                    cid: detail.cid.max(0) as u64,
                    aid: detail.aid,
                    title: detail.title,
                    owner: detail.owner_name,
                    duration: format_duration(detail.duration),
                    cover: detail.pic,
                },
                Err(err) => {
                    crate::debug_log(&format!(
                        "playlist resolve failed bvid={bvid} err={err}"
                    ));
                    return;
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if ui.upgrade().is_none() {
                    return;
                }
                if let Some(host) = playlist() {
                    host.insert_entry(entry, only);
                }
            });
        });
    }

    /// Append one resolved entry, open the panel for a plain enqueue, and
    /// start it only when nothing is playing.
    fn insert_entry(&self, entry: QueueEntry, only: bool) {
        if !only {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_playlist_open(true);
            }
        }
        let bvid = entry.bvid.clone();
        if !self.queue.borrow_mut().push(entry) {
            crate::debug_log(&format!("playlist dup bvid={bvid}"));
            return;
        }
        let index = self.queue.borrow().items.len().saturating_sub(1);
        let cover = self
            .queue
            .borrow()
            .items
            .get(index)
            .map(|entry| entry.cover.clone())
            .unwrap_or_default();
        crate::debug_log(&format!(
            "playlist enqueue index={index} bvid={bvid} only={only} items={}",
            self.queue.borrow().items.len()
        ));
        self.spawn_cover(cover);
        self.publish();
        self.persist();
        // "Nothing is playing" includes a natural stop (mode 0) and a closed
        // player; a merely paused player still owns the screen, so a new card
        // is appended without interrupting it.
        let idle = !self.playing.get() && (self.stopped.get() || !player_active());
        if !only && idle {
            self.play_index(index);
        }
    }

    /// Open the queue entry at `index` through the existing player path.
    fn play_index(&self, index: usize) {
        let entry = match self.queue.borrow().items.get(index).cloned() {
            Some(entry) => entry,
            None => return,
        };
        let Some(session) = active_session(&self.session) else {
            crate::debug_log(&format!(
                "playlist play skipped index={index} bvid={} reason=no-session",
                entry.bvid
            ));
            return;
        };
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.queue.borrow_mut().current = Some(index);
        self.end_handled.set(false);
        self.stopped.set(false);
        crate::debug_log(&format!(
            "playlist play index={index} bvid={} title={} mode={}",
            entry.bvid,
            entry.title,
            self.mode()
        ));
        open_player_inner(
            session,
            entry.to_video(),
            &state,
            &self.settings,
            OpenMode::Replace,
        );
        self.publish();
        self.persist();
    }

    fn remove(&self, index: i32) {
        let index = index.max(0) as usize;
        let removed = self.queue.borrow_mut().remove(index);
        let Some(removed) = removed else {
            crate::debug_log(&format!("playlist remove ignored index={index}"));
            return;
        };
        crate::debug_log(&format!(
            "playlist remove index={index} bvid={} items={} current={:?}",
            removed.bvid,
            self.queue.borrow().items.len(),
            self.current()
        ));
        self.publish();
        self.persist();
    }

    fn clear(&self) {
        let len = self.queue.borrow().items.len();
        self.queue.borrow_mut().clear();
        crate::debug_log(&format!("playlist clear removed={len}"));
        self.publish();
        self.persist();
    }

    /// Manual next/previous: queue order in every mode, wrapping at the ends.
    fn step(&self, forward: bool) {
        let target = self.queue.borrow().step(forward);
        if let Some(index) = target {
            crate::debug_log(&format!(
                "playlist step forward={forward} index={index} mode={}",
                self.mode()
            ));
            self.play_index(index);
        }
    }

    /// Persist the mode the UI just advanced to; never advance it again here.
    fn cycle_mode(&self) {
        let reported = self
            .ui
            .upgrade()
            .map(|ui| ui.get_play_mode())
            .unwrap_or_else(|| i32::from(settings::default_play_mode() as u8));
        let mode = settings::normalise_play_mode(reported.max(0) as u32);
        self.queue.borrow_mut().mode = mode as u8;
        crate::debug_log(&format!("playlist mode={mode}"));
        self.publish();
        self.persist();
    }

    /// Called by the active player's 33ms tick; self-gated to 250ms.
    fn tick(&self, window: &PlayerWindow, control: &player::PlaybackControl) {
        if !is_active_player(window) {
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.last_tick.get()) < Duration::from_millis(250) {
            return;
        }
        self.last_tick.set(now);
        // A mode-0 stop only holds while the track really is over: seeking back
        // (or a fresh session) clears `track_ended`, which lets the bar follow
        // playback again instead of staying stopped forever.
        if self.stopped.get() && !control.paused() && !control.track_ended() {
            self.stopped.set(false);
        }
        let playing = !control.paused() && !self.stopped.get();
        if playing != self.playing.get() {
            self.playing.set(playing);
            self.publish();
        }
        if self.end_handled.get() || !control.track_ended() {
            return;
        }
        self.end_handled.set(true);
        let (mode, current, len) = {
            let queue = self.queue.borrow();
            (queue.mode, queue.current, queue.items.len())
        };
        crate::debug_log(&format!(
            "playlist end mode={mode} current={current:?} items={len}"
        ));
        match mode {
            0 => {
                crate::debug_log("playlist end action=stop mode=0");
                self.stopped.set(true);
                self.playing.set(false);
                self.publish();
            }
            2 => {
                let (candidates, target) = {
                    let mut queue = self.queue.borrow_mut();
                    // The item that just ended counts as played; then pick a
                    // random one that has not been.
                    if let Some(current) = queue.current {
                        queue.played.insert(current);
                    }
                    let candidates = queue.random_candidates();
                    let pick = if candidates.is_empty() {
                        None
                    } else {
                        Some(self.random_below(candidates.len()))
                    };
                    let target = pick.and_then(|index| candidates.get(index).copied());
                    (candidates.len(), target)
                };
                match target {
                    Some(index) => {
                        crate::debug_log(&format!(
                            "playlist end action=shuffle candidates={candidates} pick={index}"
                        ));
                        self.play_index(index);
                    }
                    None => crate::debug_log("playlist end action=none mode=2"),
                }
            }
            _ => {
                let target = self.queue.borrow().step(true);
                match target {
                    Some(index) => {
                        crate::debug_log(&format!("playlist end action=next pick={index} mode=1"));
                        self.play_index(index);
                    }
                    None => crate::debug_log("playlist end action=none mode=1"),
                }
            }
        }
    }

    /// xorshift64: a tiny, dependency-free shuffle pick.
    fn random_below(&self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        let mut value = self.rng.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.rng.set(value);
        value ^= value >> 30;
        value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^= value >> 31;
        (value % bound as u64) as usize
    }

    /// A player window opened (from the queue, the feed or the detail page).
    fn on_open(&self, player: &PlayerWindow, video: &bilibili::FeedVideo) {
        set_active_player(player);
        *self.now.borrow_mut() = Some(NowPlaying {
            bvid: video.bvid.clone(),
            title: video.title.clone(),
            cover: video.pic.clone(),
        });
        self.playing.set(true);
        self.stopped.set(false);
        self.end_handled.set(false);
        self.last_tick.set(Instant::now());
        self.spawn_cover(video.pic.clone());
        crate::debug_log(&format!(
            "playlist open bvid={} title={}",
            video.bvid, video.title
        ));
        self.publish();
    }

    /// A player window closed by the user (`✕` or Alt+F4; both paths call
    /// `close_player_window`). The queue keeps its position; the bar stops
    /// reporting playback. Superseding an open never reaches here — the new
    /// [`Self::on_open`] simply takes over.
    fn on_close(&self) {
        clear_active_player();
        self.playing.set(false);
        self.stopped.set(true);
        self.end_handled.set(true);
        self.publish();
        crate::debug_log("playlist close keep-current=1 playing=0");
    }

    /// Another open player window gained focus: the bar's now-playing block
    /// follows it, but the queue's position and stop/end state are kept.
    ///
    /// Closing the focused window while another one is still open lands here
    /// too, so the queue never stays attached to a window that is gone.
    fn on_focus(&self, player: &PlayerWindow, video: &bilibili::FeedVideo) {
        set_active_player(player);
        *self.now.borrow_mut() = Some(NowPlaying {
            bvid: video.bvid.clone(),
            title: video.title.clone(),
            cover: video.pic.clone(),
        });
        self.last_tick.set(Instant::now());
        self.spawn_cover(video.pic.clone());
        self.publish();
        crate::debug_log(&format!(
            "playlist focus bvid={} title={}",
            video.bvid, video.title
        ));
    }
}

// ------------------------ BILI_PLAYLIST_TEST=1 hook -------------------------
//
// Permanent, backend-driven proof for the queue engine. Every step goes through
// the SAME Slint callbacks the UI fires (`card_enqueue` / `playlist_play` /
// `cycle_play_mode` / `playlist_remove` / `playlist_clear`), and the
// end-of-track is produced through the real code path: the test seeks to
// `duration - 200ms`, then the player's 250ms tick latches
// `PlaybackControl::track_ended` and the queue advances exactly as it would at
// a natural end. Nothing here fabricates queue state.

/// Runs once per process; the timer lives in a thread-local so dropping the
/// closure cannot stop it by accident.
static PLAYLIST_TEST_STARTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static PLAYLIST_TEST_TIMER: RefCell<Option<Rc<slint::Timer>>> = RefCell::new(None);
}

/// Log the queue as the diagnostic run reads it.
fn log_playlist_state(label: &str) {
    if let Some(host) = playlist() {
        let (items, current, mode) = host.snapshot();
        crate::debug_log(&format!(
            "playlist test {label} items={} current={current:?} mode={mode} queue={items:?} now_playing={}",
            items.len(),
            host.ui
                .upgrade()
                .map(|ui| ui.get_now_playing())
                .unwrap_or(false)
        ));
    } else {
        crate::debug_log(&format!("playlist test {label} host=missing"));
    }
}

/// Seek to just before the end of the active player's track, which is the
/// "track ended" condition reached through the normal seek code path (the end
/// flag is cleared by the seek and re-latched by the player tick).
fn seek_near_track_end() -> bool {
    let Some(control) = player::active_control() else {
        return false;
    };
    let duration = control.duration_ms();
    if duration == 0 {
        return false;
    }
    let target = duration.saturating_sub(200);
    control.seek(target);
    crate::debug_log(&format!(
        "playlist test seek duration={duration} target={target}"
    ));
    true
}

fn maybe_start_playlist_test(ui: &AppWindow) {
    let enabled = std::env::var("BILI_PLAYLIST_TEST")
        .map(|value| value == "1")
        .unwrap_or(false);
    if !enabled || PLAYLIST_TEST_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::debug_log("playlist test start hook=BILI_PLAYLIST_TEST");
    let weak = ui.as_weak();
    let phase = Rc::new(Cell::new(0u32));
    let first_random = Rc::new(Cell::new(usize::MAX));
    let quiet = Rc::new(Cell::new(0u32));
    let timer = Rc::new(slint::Timer::default());
    PLAYLIST_TEST_TIMER.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            *slot = Some(timer.clone());
        }
    });
    let timer_a = timer.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(500),
        move || {
            let Some(ui) = weak.upgrade() else {
                timer_a.stop();
                return;
            };
            let current = phase.get();
            let next = match current {
                0 => {
                    // 1. Enqueue three feed rows through the real card callback.
                    crate::debug_log("playlist test step=enqueue count=3 via=card_enqueue");
                    ui.invoke_card_enqueue(0);
                    ui.invoke_card_enqueue(1);
                    ui.invoke_card_enqueue(2);
                    log_playlist_state("after-enqueue");
                    1
                }
                1 => {
                    crate::debug_log("playlist test step=play-index-0 via=playlist_play");
                    ui.invoke_playlist_play(0);
                    2
                }
                2 => {
                    // Wait for playurl to land, then pin mode 1 (automatic) the
                    // same way the UI does: set the property, fire the callback.
                    let ready = player::active_control()
                        .map(|control| control.duration_ms() > 0)
                        .unwrap_or(false);
                    if !ready {
                        2
                    } else {
                        ui.set_play_mode(1);
                        ui.invoke_cycle_play_mode();
                        crate::debug_log("playlist test step=mode-1 via=cycle_play_mode");
                        if seek_near_track_end() {
                            3
                        } else {
                            2
                        }
                    }
                }
                3 => {
                    if playlist().and_then(|host| host.current()) == Some(1) {
                        log_playlist_state("mode-1-advanced");
                        crate::debug_log("playlist test assert mode=1 from=0 to=1 ok=1");
                        ui.set_play_mode(2);
                        ui.invoke_cycle_play_mode();
                        crate::debug_log("playlist test step=mode-2 via=cycle_play_mode");
                        4
                    } else {
                        3
                    }
                }
                4 => {
                    let ready = player::active_control()
                        .map(|control| control.duration_ms() > 0)
                        .unwrap_or(false);
                    if ready {
                        if seek_near_track_end() {
                            5
                        } else {
                            4
                        }
                    } else {
                        4
                    }
                }
                5 => {
                    let current = playlist().and_then(|host| host.current());
                    match current {
                        Some(index) if index != 1 => {
                            first_random.set(index);
                            log_playlist_state("mode-2-first");
                            crate::debug_log(&format!(
                                "playlist test assert mode=2 first={index} repeat=0"
                            ));
                            6
                        }
                        _ => 5,
                    }
                }
                6 => {
                    let ready = player::active_control()
                        .map(|control| control.duration_ms() > 0)
                        .unwrap_or(false);
                    if ready {
                        if seek_near_track_end() {
                            7
                        } else {
                            6
                        }
                    } else {
                        6
                    }
                }
                7 => {
                    let current = playlist().and_then(|host| host.current());
                    let first = first_random.get();
                    match current {
                        Some(index) if index != first => {
                            log_playlist_state("mode-2-second");
                            crate::debug_log(&format!(
                                "playlist test assert mode=2 first={first} second={index} repeat={}",
                                usize::from(index == first)
                            ));
                            ui.set_play_mode(0);
                            ui.invoke_cycle_play_mode();
                            crate::debug_log("playlist test step=mode-0 via=cycle_play_mode");
                            8
                        }
                        _ => 7,
                    }
                }
                8 => {
                    let ready = player::active_control()
                        .map(|control| control.duration_ms() > 0)
                        .unwrap_or(false);
                    if ready {
                        if seek_near_track_end() {
                            quiet.set(0);
                            9
                        } else {
                            8
                        }
                    } else {
                        8
                    }
                }
                9 => {
                    // Give the 250ms tick time to handle the end, then confirm
                    // it stopped: the current index must not move.
                    let ticks = quiet.get() + 1;
                    quiet.set(ticks);
                    if ticks < 4 {
                        9
                    } else {
                        log_playlist_state("mode-0-stopped");
                        crate::debug_log(&format!(
                            "playlist test assert mode=0 stopped=1 now_playing={}",
                            ui.get_now_playing()
                        ));
                        // Status → toast: exactly the call `player::set_status`
                        // makes, twice, to prove one event shows one toast. The
                        // active player window is still open at this point.
                        crate::toast_active("播放结束", crate::TOAST_KIND_INFO);
                        crate::toast_active("播放结束", crate::TOAST_KIND_INFO);
                        crate::debug_log("playlist test toast-active twice second-must-skip");
                        10
                    }
                }
                10 => {
                    crate::debug_log("playlist test step=remove-index-0");
                    ui.invoke_playlist_remove(0);
                    log_playlist_state("after-remove");
                    11
                }
                11 => {
                    crate::debug_log("playlist test step=clear");
                    ui.invoke_playlist_clear();
                    log_playlist_state("after-clear");
                    // Persistence check: the cleared queue is what the file
                    // holds right now.
                    let stored = settings::load();
                    crate::debug_log(&format!(
                        "playlist test stored-after-clear items={} current={:?} mode={}",
                        stored.playlist.len(),
                        stored.playlist_current,
                        stored.play_mode
                    ));
                    // Leave three entries behind (append only: no autoplay) so
                    // the relaunch can prove restore without starting anything.
                    crate::debug_log("playlist test step=reseed count=3 via=card_enqueue_only");
                    ui.invoke_card_enqueue_only(0);
                    ui.invoke_card_enqueue_only(1);
                    ui.invoke_card_enqueue_only(2);
                    log_playlist_state("after-reseed");
                    12
                }
                12 => {
                    let stored = settings::load();
                    crate::debug_log(&format!(
                        "playlist test stored-for-restore items={} current={:?} mode={} autoplay={}",
                        stored.playlist.len(),
                        stored.playlist_current,
                        stored.play_mode,
                        ui.get_now_playing()
                    ));
                    crate::debug_log(
                        "playlist test done relaunch=BILI_PLAYLIST_TEST=1&restore-check",
                    );
                    13
                }
                13 => {
                    // The ✕ path (both close paths share `close_player_window`):
                    // the bar must stop reporting playback and the queue must
                    // keep its position.
                    let current = playlist().and_then(|host| host.current());
                    let window = playlist()
                        .and_then(|host| host.state.upgrade())
                        .and_then(|state| newest_player(&state));
                    match window {
                        Some(player) => {
                            crate::debug_log("playlist test step=close-window via=player_closed");
                            player.invoke_player_closed();
                            log_playlist_state("after-close");
                            crate::debug_log(&format!(
                                "playlist test assert close current={current:?} now_playing={} keep-current={}",
                                ui.get_now_playing(),
                                usize::from(playlist().and_then(|host| host.current()) == current)
                            ));
                            14
                        }
                        None => 14,
                    }
                }
                _ => {
                    timer_a.stop();
                    return;
                }
            };
            phase.set(next);
        },
    );
}

// ===================== 缩放进度条: 20-cell heat ===========================
//
// `ui/app.slint` derives the scrub window (base/span) from `scrub_level` +
// `scrub_cell_*` and fires `scrub_level_changed(level)`. Rust answers with
// exactly 20 densities for that window, taken from the curve the seek bar
// already has (`density_buckets`), normalised to the WINDOW's own peak. No
// refetch and no work per frame: only a zoom gesture recomputes it.

/// The strip always draws 20 cells.
const SCRUB_CELLS: usize = 20;
/// Mirrors of `player::DENSITY_BUCKET_MS` / `player::DENSITY_MAX_BUCKETS`, the
/// constants `compute_density` built the published curve with. Recomputing the
/// bucket width from the duration keeps the two in step without touching
/// `player.rs`.
const DENSITY_BUCKET_MS: u64 = 2000;
const DENSITY_MAX_BUCKETS: u64 = 400;

/// Bucket width `compute_density` used for a video of `duration_ms`.
fn density_bucket_ms(duration_ms: u64) -> u64 {
    let mut bucket = DENSITY_BUCKET_MS;
    if duration_ms / bucket + 1 > DENSITY_MAX_BUCKETS {
        bucket = (duration_ms / DENSITY_MAX_BUCKETS).max(1);
    }
    bucket
}

/// 20 heat values (0..=1) for `[base, base+span)` over the density curve.
///
/// Each density bucket contributes to the cells it overlaps, weighted by the
/// overlap length, so a bucket wider than a cell spreads instead of spiking.
/// The result is normalised to its own maximum; no data (or a non-positive
/// span) returns all zeros.
fn scrub_heat_values(
    buckets: &[f32],
    duration_ms: f32,
    base_ms: f32,
    span_ms: f32,
) -> Vec<f32> {
    let mut heat = vec![0.0f32; SCRUB_CELLS];
    if buckets.is_empty()
        || !base_ms.is_finite()
        || !span_ms.is_finite()
        || span_ms <= 0.0
        || !duration_ms.is_finite()
        || duration_ms <= 0.0
    {
        return heat;
    }
    let bucket_ms = density_bucket_ms(duration_ms as u64) as f64;
    let base = base_ms.max(0.0) as f64;
    let span = span_ms as f64;
    let cell = span / SCRUB_CELLS as f64;
    if cell <= 0.0 || bucket_ms <= 0.0 {
        return heat;
    }
    for (index, value) in buckets.iter().enumerate() {
        if !value.is_finite() || *value <= 0.0 {
            continue;
        }
        let bucket_start = index as f64 * bucket_ms;
        let bucket_end = bucket_start + bucket_ms;
        let start = bucket_start.max(base);
        let end = bucket_end.min(base + span);
        if end <= start {
            continue;
        }
        let mut position = start;
        while position < end {
            let cell_index =
                (((position - base) / cell).floor() as usize).min(SCRUB_CELLS - 1);
            let cell_end = base + (cell_index + 1) as f64 * cell;
            let part_end = end.min(cell_end);
            let overlap = part_end - position;
            if overlap > 0.0 {
                heat[cell_index] += (*value as f64 * (overlap / bucket_ms)) as f32;
            }
            if part_end <= position {
                break;
            }
            position = part_end;
        }
    }
    let peak = heat.iter().copied().fold(0.0f32, f32::max);
    if peak > 0.0 {
        for value in &mut heat {
            *value = (*value / peak).clamp(0.0, 1.0);
        }
    }
    heat
}

/// `20^(level-1)`, mirroring `scrub-divisor` in `ui/app.slint`.
fn scrub_divisor(level: i32) -> f32 {
    if level <= 1 {
        1.0
    } else if level == 2 {
        20.0
    } else if level == 3 {
        400.0
    } else if level == 4 {
        8000.0
    } else {
        160_000.0
    }
}

/// The scrub window `[base, base+span)` in ms, mirroring `scrub-base` /
/// `scrub-span` in `ui/app.slint`.
///
/// The UI's `scrub_base_ms` / `scrub_span_ms` are private Slint properties, so
/// their generated getters are not callable from Rust; the three inputs they
/// derive from (`scrub_cell_*`, `duration_ms`) ARE public and give exactly the
/// same numbers.
fn scrub_window(window: &PlayerWindow, level: i32) -> (f32, f32) {
    let duration = window.get_duration_ms().max(0.0);
    let base = if level >= 2 {
        window.get_scrub_cell_1() as f32 * duration / 20.0
    } else {
        0.0
    } + if level >= 3 {
        window.get_scrub_cell_2() as f32 * duration / 400.0
    } else {
        0.0
    } + if level >= 4 {
        window.get_scrub_cell_3() as f32 * duration / 8000.0
    } else {
        0.0
    } + if level >= 5 {
        window.get_scrub_cell_4() as f32 * duration / 160_000.0
    } else {
        0.0
    };
    let span = (duration / scrub_divisor(level)).max(1.0);
    (base, span)
}

/// Publish the 20 heat cells for `level` (empty model at level 0).
///
/// Reads only what the player already has: the published density curve, the
/// duration and the derived window. Runs on a zoom gesture, never per frame.
fn publish_scrub_heat(window: &PlayerWindow, model: &Rc<VecModel<f32>>, level: i32) {
    if level <= 0 {
        model.set_vec(Vec::new());
        crate::debug_log("scrub heat level=0 cleared");
        return;
    }
    let buckets = window.get_density_buckets();
    let values: Vec<f32> = (0..buckets.row_count())
        .filter_map(|index| buckets.row_data(index))
        .collect();
    let duration = window.get_duration_ms();
    let (base, span) = scrub_window(window, level);
    let heat = scrub_heat_values(&values, duration, base, span);
    let peak = heat.iter().copied().fold(0.0f32, f32::max);
    let list = heat
        .iter()
        .map(|value| format!("{value:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    crate::debug_log(&format!(
        "scrub heat level={level} base_ms={base:.0} span_ms={span:.0} buckets={} peak={peak:.3} values=[{list}]",
        values.len()
    ));
    model.set_vec(heat);
}

/// The open player's comment state: which video's comments are loaded, the
/// paging cursor, the rows the UI renders, the sort mode and a cache of each
/// comment's dialog (keyed by root `rpid`).
///
/// Held behind `Arc<Mutex<_>>` so the network workers can fill it while the UI
/// thread reads it. The UI publish pass is [`publish_comments`].
#[derive(Default)]
struct CommentsState {
    /// The video whose comments this state describes (`0` before a player opens).
    aid: i64,
    /// `true` = 热门 (mode 3), `false` = 按时间 (mode 2).
    sort_hot: bool,
    /// Cursor for the next page (`""` when the list is exhausted).
    cursor: String,
    is_end: bool,
    total: i64,
    /// The pinned comment, when the video has one.
    top: Option<bilibili::Comment>,
    rows: Vec<bilibili::Comment>,
    /// The page after `cursor`, already fetched. `on_comments_more` appends it
    /// without a network round trip (one page ahead, so scrolling is instant).
    prefetch: Option<CommentsPrefetch>,
    /// A prefetch request is in flight for `cursor`.
    prefetching: bool,
    /// One comment's dialog, cached by root `rpid` (page 1 only).
    replies: HashMap<i64, Vec<bilibili::Comment>>,
    /// `true` while another page of a dialog exists (查看更多回复).
    reply_more: HashMap<i64, bool>,
    /// Root `rpid`s the user expanded, so a republish (load more / avatar
    /// patch / sort change) keeps every open thread open.
    expanded: HashSet<i64>,
    /// The row index -> root `rpid` map of the model the UI currently holds,
    /// written by [`publish_comments`] from the very snapshot it publishes.
    /// The UI names a row by index only (rpids are int64, Slint's `int` is
    /// 32-bit), so this is where a click becomes an id again.
    row_rpids: Vec<i64>,
    /// Raw RGBA avatar pixels by URL. The worker threads fill this so a
    /// republished row keeps the picture it already showed.
    avatars: HashMap<String, (Vec<u8>, u32, u32)>,
    loading: bool,
    error: String,
    /// The window the workers publish to. Weak and `Send`, so it can ride in
    /// this slot; `None` before a player exists (e.g. BILI_COMMENTS_TEST).
    ui: Option<Weak<PlayerWindow>>,
}

/// One page fetched ahead of the panel: the cursor it was requested for (so a
/// sort change or a reload can discard it) and the page itself.
struct CommentsPrefetch {
    aid: i64,
    sort_hot: bool,
    cursor: String,
    page: bilibili::CommentPage,
}

/// Shared handle to the open player's [`CommentsState`].
type CommentsSlot = Arc<Mutex<CommentsState>>;

/// Shared handles the UI thread keeps for the open player windows.
///
/// Each window owns its playback control, panel geometry, pointer watch and
/// glass loop; nothing here is shared between two players. `players` holds them
/// oldest first and is the only strong reference that outlives the open call
/// (the window's own callbacks use `Weak`), so removing an entry closes exactly
/// that window.
struct PlayerState {
    /// Every player window currently open, oldest first.
    players: RefCell<Vec<Rc<PlayerSession>>>,
    /// The window that last had focus (WM activation or a click). Card opens
    /// with mode 0 replace THIS one; `None` falls back to the newest.
    focused: RefCell<Option<Rc<PlayerSession>>>,
    /// The mini (PiP) window while one owns a real Slint window; hidden windows
    /// stay here so they can be re-shown without rebuilding.
    mini: RefCell<Option<MiniPlayerWindow>>,
    /// The player window the mini currently belongs to (`0` = none), so closing
    /// another player never tears down a mini it does not own.
    mini_owner: Cell<isize>,
    /// The main window, so a player open/close (and the player's download
    /// button) can reach it without holding a borrow.
    main: Weak<AppWindow>,
    /// The app's single in-flight download slot, so a download started from the
    /// player supersedes (and cancels) the previous one like the page does.
    download_control: Arc<Mutex<Option<Arc<download::DownloadControl>>>>,
    /// Repeating timer that retries installing the frameless subclass on the
    /// player/mini windows until their native handle exists (secondary windows
    /// are mapped late).
    frameless_timer: slint::Timer,
    /// Repeating timer that re-asserts the main window's frameless style, so a
    /// re-decoration that arrives without an activation message is reverted
    /// within one tick.
    main_frameless_timer: slint::Timer,
    /// Effective dark flag for windows created after the 主题 setting changed.
    theme_dark: Cell<bool>,
}

/// One open player window and everything that belongs to exactly that window.
///
/// This is the owner of the window's strong handle: a session exists while its
/// window is open, and dropping it closes the window.
struct PlayerSession {
    /// 1-based ordinal, so a QA log can tell two windows apart.
    id: u64,
    window: PlayerWindow,
    control: Arc<player::PlaybackControl>,
    panel_geom: Rc<RefCell<PanelGeom>>,
    /// Native handle, cached once known (identity for focus/toast/glass).
    hwnd: Cell<isize>,
    /// A close path already ran for this window (a native WM_CLOSE can race
    /// the ✕); the second one is a no-op.
    closed: Cell<bool>,
    /// This window's 60Hz pointer watch + layout debounce.
    loops: RefCell<Option<Rc<PlayerLoops>>>,
    /// This window's frosted-glass loop (created at open, worker attached once
    /// the HWND exists).
    glass: RefCell<Option<Rc<PlayerGlass>>>,
    /// The decode thread mirrors frames into the mini through this slot; one
    /// per window so a background player never paints another's PiP.
    mini_slot: player::MiniSlot,
    /// The video this window shows, so a focus/close fallback can hand it to the
    /// queue's "now playing" block.
    video: bilibili::FeedVideo,
    /// Logical px this window was cascaded by when it was opened as a second
    /// window; subtracted again when persisting the base layout.
    cascade: i32,
}

thread_local! {
    /// Last ordinal handed to a player window (UI thread only).
    static PLAYER_SESSION_IDS: Cell<u64> = Cell::new(0);
}

fn next_player_session_id() -> u64 {
    PLAYER_SESSION_IDS.with(|cell| {
        let next = cell.get().saturating_add(1);
        cell.set(next);
        next
    })
}

/// Open sessions, oldest first (never while holding the `players` borrow).
fn player_sessions(state: &Rc<PlayerState>) -> Vec<Rc<PlayerSession>> {
    state.players.borrow().clone()
}

/// The session for one window handle, if it is still open.
fn session_for_hwnd(state: &Rc<PlayerState>, hwnd: isize) -> Option<Rc<PlayerSession>> {
    state
        .players
        .borrow()
        .iter()
        .find(|session| session.hwnd.get() == hwnd)
        .cloned()
}

/// The session a window's `Weak` resolves to, matched by native handle.
fn session_for_window(state: &Rc<PlayerState>, player: &PlayerWindow) -> Option<Rc<PlayerSession>> {
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
fn focused_session(state: &Rc<PlayerState>) -> Option<Rc<PlayerSession>> {
    if let Some(session) = state.focused.borrow().as_ref() {
        if session.hwnd.get() != 0 {
            return Some(session.clone());
        }
    }
    state.players.borrow().last().cloned()
}

/// The newest open player, as a plain window handle.
fn newest_player(state: &Rc<PlayerState>) -> Option<PlayerWindow> {
    state
        .players
        .borrow()
        .last()
        .map(|session| session.window.clone_strong())
}

/// Find the current mini window handle (if any) without holding the borrow.
fn mini_handle(state: &Rc<PlayerState>) -> Option<MiniPlayerWindow> {
    state.mini.borrow().as_ref().map(|mini| mini.clone_strong())
}

/// Undo the player-active suspension: resume main-window background work (the
/// incremental cover fetches, the skeleton shimmer and the frosted-glass
/// refresh loop, all gated on `player_active()`). Shared by both player close
/// paths; the main window stays suspended while another player is still open.
fn resume_main_window(_state: &Rc<PlayerState>) {
    player_closed();
}

/// Re-assert the topmost flag on windows whose native handle became available
/// after `show()` (the flag cannot be set until the OS window exists).
///
/// Only ever applies when the user explicitly enabled 置顶 (`on`): the default
/// is normal window ordering, so nothing is pinned above other windows unless
/// the user asked for it.
fn ensure_topmost(on: bool, main: Option<&PlayerWindow>, state: &Rc<PlayerState>) {
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
fn start_tray(weak: &Weak<AppWindow>) {
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
                        win::restore(ui.window());
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
                        win::set_visible(ui.window(), false);
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
fn toggle_main_window(ui: &AppWindow, hide_muted: &Arc<AtomicBool>) {
    let window = ui.window();
    let visible = win::is_visible(window).unwrap_or(false);
    let minimized = win::is_minimized(window).unwrap_or(false);
    if visible && !minimized {
        win::set_visible(window, false);
        if let Some(control) = player::active_control() {
            if !control.muted() {
                control.set_muted(true);
                hide_muted.store(true, Ordering::SeqCst);
            }
        }
        crate::debug_log("boss key: main window hidden");
    } else {
        win::restore(window);
        if hide_muted.swap(false, Ordering::SeqCst) {
            if let Some(control) = player::active_control() {
                control.set_muted(false);
            }
        }
        crate::debug_log("boss key: main window shown");
    }
}

/// What closing the main window does, read back from the stored preference.
///
/// This is the ONE dispatcher behind every close route: the custom title bar's
/// ✕ (`on_close_window`), the native close hook (`WM_CLOSE`, i.e. Alt+F4 and the
/// taskbar's 关闭) and the `BILI_CLOSE_TEST` driver call it, and the tray's 退出
/// reuses the same quit path ([`request_quit`]).
fn apply_close_action(ui: &AppWindow, settings_store: &Arc<Mutex<settings::Settings>>) {
    let window = ui.window();
    match close_action(settings_store) {
        0 => {
            // `SW_MINIMIZE` keeps the taskbar button, so the window can always be
            // brought back the normal way.
            let ok = win::minimize(window);
            crate::debug_log(&format!(
                "close: action=0 minimize ok={ok} {}",
                close_state(window)
            ));
        }
        1 => {
            let ok = win::set_visible(window, false);
            tray::notify_hidden_once();
            crate::debug_log(&format!(
                "close: action=1 hide-to-tray ok={ok} {}",
                close_state(window)
            ));
        }
        _ => request_quit("close action=2"),
    }
}

/// The stored 关闭主窗口 behaviour, snapped to one of [`settings::CLOSE_ACTIONS`].
fn close_action(settings_store: &Arc<Mutex<settings::Settings>>) -> u32 {
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
fn request_quit(reason: &str) {
    crate::debug_log(&format!("close: quit requested ({reason})"));
    let _ = slint::quit_event_loop();
}

/// Native close state for the close log lines: `visible=… iconic=… zoomed=…`.
fn close_state(window: &slint::Window) -> String {
    format!(
        "visible={} iconic={} zoomed={}",
        flag(win::is_visible(window)),
        flag(win::is_minimized(window)),
        flag(is_zoomed(window)),
    )
}

fn flag(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "none",
    }
}

/// Native `IsZoomed` for a Slint window (the maximise bit `win.rs` does not expose).
fn is_zoomed(window: &slint::Window) -> Option<bool> {
    let hwnd = win::hwnd_of(window)?;
    Some(unsafe { IsZoomed(hwnd as *mut std::ffi::c_void) } != 0)
}

/// Slint handles the native close hook reaches without capturing a `Weak`.
///
/// The hook must be `Send + Sync` and `Weak<AppWindow>` is neither, so the hook
/// itself captures nothing and reads these from the thread it fires on (the UI
/// thread, where they are installed before the window can be closed).
struct CloseTargets {
    ui: Weak<AppWindow>,
    store: Arc<Mutex<settings::Settings>>,
}

thread_local! {
    static CLOSE_TARGETS: RefCell<Option<CloseTargets>> = RefCell::new(None);
}

fn set_close_targets(ui: &Weak<AppWindow>, store: &Arc<Mutex<settings::Settings>>) {
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
fn dispatch_main_close() {
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

/// The HWND the close hook is registered for, `0` until it succeeds.
///
/// Keyed by handle rather than a plain "done" bit so a recreated window is
/// re-armed, and so the repeated retry stops as soon as it has worked once.
static CLOSE_HOOK_HWND: AtomicIsize = AtomicIsize::new(0);

/// Install the native close hook once the main window has a HWND.
///
/// Called from the 1500 ms frameless watchdog, which is the existing retry chain:
/// `set_close_hook` returns `false` until the handle exists. Only the main window
/// is hooked; the player and mini windows keep their own close behaviour.
fn install_close_hook(window: &slint::Window) {
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
fn pid_owns_windows(pid: u32) -> bool {
    let mut probe: (u32, usize) = (pid, 0);
    unsafe {
        EnumWindows(
            Some(count_pid_window),
            (&mut probe as *mut (u32, usize)) as isize,
        );
    }
    probe.1 > 0
}

unsafe extern "system" fn count_pid_window(hwnd: *mut std::ffi::c_void, lparam: isize) -> i32 {
    let probe = &mut *(lparam as *mut (u32, usize));
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == probe.0 {
        probe.1 += 1;
    }
    1
}

/// Diagnostic (`BILI_CLOSE_TEST`): after the window is really up, run the real
/// close path once and log the resulting native state, so which branch ran is
/// provable from `BILI_DEBUG_LOG` without synthetic input.
///
/// `attempts` retries while the HWND does not exist yet. `restore` (from
/// `BILI_CLOSE_RESTORE=1`) simulates the tray click one second later.
fn run_close_test(
    weak: Weak<AppWindow>,
    store: Arc<Mutex<settings::Settings>>,
    requested: u32,
    restore: bool,
    attempts: u32,
) {
    slint::Timer::single_shot(Duration::from_millis(3000), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if win::hwnd_of(ui.window()).is_none() {
            if attempts > 0 {
                run_close_test(weak.clone(), store.clone(), requested, restore, attempts - 1);
            } else {
                crate::debug_log("close test no-hwnd");
            }
            return;
        }
        let action = close_action(&store);
        if action != requested {
            crate::debug_log(&format!(
                "close test requested={requested} stored={action} using={action}"
            ));
        }
        crate::debug_log(&format!(
            "close test action={action} before {}",
            close_state(&ui.window())
        ));
        apply_close_action(&ui, &store);
        crate::debug_log(&format!(
            "close test action={action} {} pid_alive={}",
            close_state(&ui.window()),
            pid_owns_windows(std::process::id()),
        ));
        if restore {
            let retry = weak.clone();
            slint::Timer::single_shot(Duration::from_millis(1000), move || {
                let Some(ui) = retry.upgrade() else {
                    return;
                };
                let ok = win::restore(ui.window());
                crate::debug_log(&format!("close restore ok={ok} {}", close_state(&ui.window())));
            });
        }
    });
}

/// Apply the frameless + compositor-alpha treatment once the native window
/// exists.
///
/// The HWND is only valid after the event loop has created the window, so this
/// re-queues itself a bounded number of times on the UI thread until
/// `win::setup_glass` finds a handle, then logs the resulting frame style. The
/// frosted-glass refresh loop is started separately (`start_glass_loop`), on
/// the UI thread, and does no work of its own until the handle exists.
fn schedule_glass(weak: Weak<AppWindow>, dark: bool, tries: u32) {
    let retry = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = retry.upgrade() else {
            return;
        };
        if win::style_bits(ui.window()).is_none() {
            if tries == 0 {
                crate::debug_log("glass: no hwnd after retries");
                return;
            }
            schedule_glass(retry.clone(), dark, tries - 1);
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
        win::log_frame_state(ui.window(), "before-glass");
        win::install_subclass(ui.window());
        win::setup_glass(ui.window(), dark);
        win::log_frame_state(ui.window(), "after-glass");
    });
}

/// Shared state of the frosted-glass refresh loop (see [`glass_step`]).
///
/// One per main window: the worker thread that owns the GDI grab engine and
/// does every capture + blur, the shot origin/size the UI thread keeps from the
/// last delivered frame (so placement never needs the worker), the next forced
/// recapture, the sigma last requested and last pushed to the UI (so the debug
/// log stays one line per state change) and the 33ms timer that drives it.
/// Created on the UI thread and only ever touched from there - the worker's
/// replies are re-dispatched onto the UI thread by [`glass_deliver`].
struct GlassLoop {
    /// The grab worker; created on the first tick that finds a live HWND.
    worker: RefCell<Option<glass::GlassWorker>>,
    /// Grab origin and size (physical desktop px) of the last delivered frame.
    /// Kept here so `glass_place` can re-place the image with one
    /// `GetWindowRect` and no worker round-trip.
    shot: Cell<Option<((i32, i32), i32, i32)>>,
    /// Earliest next forced recapture; pulled to "now" while the blur is off so
    /// switching it back on grabs a fresh backdrop immediately instead of
    /// reusing whatever was cached minutes ago.
    next: Cell<Instant>,
    /// Sigma of the last grab request sent to the worker; negative = none yet.
    /// A change beyond the render threshold queues a re-blur.
    requested_sigma: Cell<f32>,
    /// Sigma of the last frame pushed to the UI; negative = none yet.
    logged_sigma: Cell<f32>,
    /// Placement (glass_x/y/w/h, logical px) last pushed to the UI, so the
    /// per-tick re-place can skip the property writes when nothing moved.
    placed: Cell<Option<(f32, f32, f32, f32)>>,
    /// The refresh timer, owned here so it lives exactly as long as the loop.
    timer: slint::Timer,
}

impl GlassLoop {
    fn new() -> Self {
        GlassLoop {
            worker: RefCell::new(None),
            shot: Cell::new(None),
            next: Cell::new(Instant::now()),
            requested_sigma: Cell::new(-1.0),
            logged_sigma: Cell::new(-1.0),
            placed: Cell::new(None),
            timer: slint::Timer::default(),
        }
    }
}

thread_local! {
    /// The main window's glass loop, as a weak handle.
    ///
    /// A finished frame arrives through `slint::invoke_from_event_loop`, whose
    /// closure must be `Send` and therefore cannot capture the `Rc<GlassLoop>`
    /// (all of its state is UI-thread-only). The loop registers itself here
    /// instead, so the delivery callback can find it again on the thread it
    /// belongs to. There is exactly one main window and one loop.
    static GLASS_LOOP: RefCell<std::rc::Weak<GlassLoop>> =
        RefCell::new(std::rc::Weak::new());
}

/// The registered glass loop, while one exists.
fn glass_loop() -> Option<Rc<GlassLoop>> {
    GLASS_LOOP.with(|slot| slot.borrow().upgrade())
}

/// Start the backdrop refresh loop on the UI thread.
///
/// The 33ms tick itself is cheap; [`glass_step`] decides whether it needs to
/// queue a grab and/or re-place the image, so this can run at the animation
/// cadence without work. It is started before the native handle exists - the
/// loop's first useful tick is the one that finds it and spawns the glass
/// worker.
fn start_glass_loop(ui: &AppWindow, state: &Rc<GlassLoop>) {
    if state.timer.running() {
        return;
    }
    GLASS_LOOP.with(|slot| *slot.borrow_mut() = Rc::downgrade(state));
    let weak = ui.as_weak();
    let loop_state = Rc::downgrade(state);
    state.timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(33),
        move || {
            if let Some(state) = loop_state.upgrade() {
                glass_step(&weak, &state);
            }
        },
    );
    crate::debug_log("glass: refresh loop started");
}

/// One iteration of the backdrop refresh loop.
///
/// Each tick is cheap when nothing changed: it reads the two 设置 sliders from
/// the theme, queues a grab on the glass worker at most every 400ms (or when
/// the blur radius moved) and re-places the last delivered frame. Capture and
/// blur never run here - the worker owns them, coalesces the requests and
/// delivers finished frames through [`glass_deliver`]. A window move is not a
/// reason to grab: the grab covers the whole desktop and [`glass_place`] keeps
/// it registered with the desktop, so it stays correct wherever the window is.
/// While a player window is open the whole loop is suspended (`PLAYER_ACTIVE`),
/// like the other main-window background work.
fn glass_step(weak: &Weak<AppWindow>, state: &Rc<GlassLoop>) {
    const GLASS_INTERVAL: Duration = Duration::from_millis(400);
    if player_active() {
        return;
    }
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let theme = ui.global::<AppTheme>();
    let blur = theme.get_glass_blur();
    let transparency = theme.get_glass_transparency();
    let now = Instant::now();

    if blur <= 0.5 {
        // Slider at zero: there is no blur layer at all. The tint alone sits
        // over the compositor's transparent window - a sharp, always-live
        // desktop - and the loop stays idle. `next` is pulled to now so that
        // turning the blur back on grabs a fresh backdrop immediately rather
        // than reusing whatever was cached minutes ago.
        if ui.get_glass_backdrop_valid() {
            ui.set_glass_backdrop(Image::default());
            ui.set_glass_backdrop_valid(false);
            crate::debug_log(&format!(
                "glass step blur=0 backdrop=off transparency={transparency:.0}"
            ));
        }
        state.next.set(now);
        return;
    }

    let Some(hwnd) = win::hwnd_of(ui.window()) else {
        return;
    };
    if state.worker.borrow().is_none() {
        let weak = weak.clone();
        let worker = glass::GlassWorker::spawn(hwnd, move |frame| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    glass_deliver(&ui, frame);
                }
            });
        });
        *state.worker.borrow_mut() = Some(worker);
    }
    let scale = ui.window().scale_factor().max(0.5);
    // Slider 0..100 -> Gaussian sigma in physical pixels; rendering in physical
    // pixels keeps the visual radius the same at any DPI scale.
    let sigma = blur / 100.0 * glass::SIGMA_MAX * scale;

    // A grab is due, nothing has been requested yet, or the radius moved since
    // the last request: queue one. The worker coalesces them (only the latest
    // survives) and captures/re-blurs only what it actually needs.
    let recapture = now >= state.next.get();
    if recapture {
        state.next.set(now + GLASS_INTERVAL);
    }
    let resigma =
        (sigma - state.requested_sigma.get()).abs() > 0.25 || state.requested_sigma.get() < 0.0;
    if recapture || resigma {
        state.requested_sigma.set(sigma);
        if let Some(worker) = state.worker.borrow().as_ref() {
            worker.request(glass::GlassRequest::Grab {
                sigma,
                scale,
                recapture,
            });
        }
    }

    glass_place(&ui, state);
}

/// Apply one finished frame on the UI thread.
///
/// Runs from the worker's `slint::invoke_from_event_loop` delivery: the image
/// (built here from the worker's `Send` pixel buffer - `slint::Image` itself is
/// not `Send`), the placement that came with it, and the shot origin/size the
/// drag path re-places from.
fn glass_deliver(ui: &AppWindow, frame: glass::GlassFrame) {
    let Some(state) = glass_loop() else {
        return;
    };
    state
        .shot
        .set(Some((frame.shot_origin, frame.shot_w, frame.shot_h)));
    ui.set_glass_backdrop(Image::from_rgba8(frame.buffer));
    ui.set_glass_backdrop_valid(true);
    if (frame.sigma - state.logged_sigma.get()).abs() > 0.25 {
        state.logged_sigma.set(frame.sigma);
        let theme = ui.global::<AppTheme>();
        crate::debug_log(&format!(
            "glass step sigma={:.1} blur={:.0} transparency={:.0}",
            frame.sigma,
            theme.get_glass_blur(),
            theme.get_glass_transparency(),
        ));
    }
    // The placement that came with the image, applied straight away so
    // `glass_place` below only has to cover plain moves and resizes.
    glass_apply(ui, &state, frame.placed);
}

/// Write a backdrop placement into the window, skipping the writes when it is
/// unchanged (the loop runs every 33ms, and Slint notifies on every property
/// write).
fn glass_apply(ui: &AppWindow, state: &GlassLoop, placed: (f32, f32, f32, f32)) {
    if state.placed.get() == Some(placed) {
        return;
    }
    state.placed.set(Some(placed));
    ui.set_glass_x(placed.0);
    ui.set_glass_y(placed.1);
    ui.set_glass_w(placed.2);
    ui.set_glass_h(placed.3);
    crate::debug_log(&format!(
        "glass place x={:.0} y={:.0} w={:.0} h={:.0}",
        placed.0, placed.1, placed.2, placed.3
    ));
}

/// Keep the desktop grab registered with the desktop: place it at the grab
/// origin minus the current window origin, in logical pixels.
///
/// Pure placement — one `GetWindowRect`, four property writes, no capture and
/// no blur (and no worker round-trip) — so it runs on every [`glass_step`] tick
/// and, more importantly, in the same pointer event as `WindowDrag::move_to`
/// (see the drag wiring): the image follows the window immediately instead of
/// waiting for the next capture, which is what removes the trailing blur during
/// a drag. Before the first delivered frame there is nothing to place.
fn glass_place(ui: &AppWindow, state: &GlassLoop) {
    let Some((shot_origin, shot_w, shot_h)) = state.shot.get() else {
        return;
    };
    // A minimised window reports a -32000 rect; the previous placement is the
    // one the restore comes back to.
    let Some(hwnd) = win::hwnd_of(ui.window()) else {
        return;
    };
    if glass::window_iconic(hwnd) {
        return;
    }
    let Some((wx, wy)) = win::window_origin(ui.window()) else {
        return;
    };
    let scale = ui.window().scale_factor().max(0.5);
    glass_apply(
        ui,
        state,
        glass::place_for(shot_origin, shot_w, shot_h, wx, wy, scale),
    );
}

/// Shared state of the PLAYER window's frosted-glass refresh loop (see
/// [`player_glass_step`]).
///
/// A deliberate near-copy of [`GlassLoop`] for the second window. The main loop
/// is suspended while a player is open (`PLAYER_ACTIVE`), so the player carries
/// its own worker - a whole-desktop grab with its own `WDA_EXCLUDEFROMCAPTURE`
/// toggle, see `glass.rs` - driven by its own 33ms timer. Exactly one
/// `PlayerGlass` lives per open player window; [`stop_player_glass`] drops it on
/// close, so reopening always starts a fresh worker. All of it is
/// UI-thread-only, like `GlassLoop`.
struct PlayerGlass {
    /// The grab worker, spawned once the player HWND exists.
    worker: RefCell<Option<glass::GlassWorker>>,
    /// The player HWND the worker was spawned for: a frame from a superseded
    /// player's worker is dropped instead of feeding the next player's loop.
    hwnd: Cell<isize>,
    /// The control of the window this loop belongs to, weakly so the loop never
    /// keeps the session alive. Used to release the video-glass sampling.
    control: std::sync::Weak<player::PlaybackControl>,
    /// Whether the panels currently sample the VIDEO frame instead of the
    /// desktop snapshot (fullscreen/overlay with a panel open).
    video_glass_on: Cell<bool>,
    /// Last `PlaybackControl::video_glass_generation` baked into `panel_glass`
    /// (0 = nothing baked for this activation).
    video_glass_generation: Cell<u64>,
    /// Last time a video frame was baked: the 10 fps gate.
    video_glass_at: Cell<Option<Instant>>,
    /// Frames baked since the last activation, logged when the source flips
    /// back so a QA run can see the effective rate.
    video_glass_bakes: Cell<u64>,
    /// Window rect (logical px) last written to `panel_glass_*` in video mode.
    video_glass_rect: Cell<Option<(f32, f32, f32, f32)>>,
    /// Grab origin and size (physical desktop px) of the last delivered frame,
    /// kept so the per-tick placement needs one `GetWindowRect` and no worker
    /// round-trip.
    shot: Cell<Option<((i32, i32), i32, i32)>>,
    /// Earliest next forced recapture (the same 400ms cadence as the main loop).
    next: Cell<Instant>,
    /// Sigma of the last grab request sent to the worker; negative = none yet.
    requested_sigma: Cell<f32>,
    /// Sigma of the last frame pushed to the UI; negative = none yet.
    logged_sigma: Cell<f32>,
    /// Placement (glass_x/y/w/h, logical px) last pushed to the UI, so the
    /// per-tick re-place can skip the property writes when nothing moved.
    placed: Cell<Option<(f32, f32, f32, f32)>>,
    /// The refresh timer, owned here so it lives exactly as long as the loop.
    timer: slint::Timer,
}

impl PlayerGlass {
    fn new(hwnd: isize, control: std::sync::Weak<player::PlaybackControl>) -> Self {
        PlayerGlass {
            worker: RefCell::new(None),
            hwnd: Cell::new(hwnd),
            control,
            video_glass_on: Cell::new(false),
            video_glass_generation: Cell::new(0),
            video_glass_at: Cell::new(None),
            video_glass_rect: Cell::new(None),
            video_glass_bakes: Cell::new(0),
            shot: Cell::new(None),
            next: Cell::new(Instant::now()),
            requested_sigma: Cell::new(-1.0),
            logged_sigma: Cell::new(-1.0),
            placed: Cell::new(None),
            timer: slint::Timer::default(),
        }
    }
}

thread_local! {
    /// Every open player window's glass loop, as weak handles.
    ///
    /// Same routing problem as `GLASS_LOOP`: the worker's delivery closure must
    /// be `Send` and cannot capture the `Rc<PlayerGlass>`, so each loop
    /// registers itself here and delivery finds it again on the UI thread.
    /// Several players can be open at once, so this is a list keyed by the
    /// loop's own HWND; [`stop_player_glass`] removes exactly its own entry and
    /// never touches another window's.
    static PLAYER_GLASS_LOOPS: RefCell<Vec<std::rc::Weak<PlayerGlass>>> =
        RefCell::new(Vec::new());
}

/// The glass loop that belongs to `player`, if it is running.
fn player_glass_for(player: &PlayerWindow) -> Option<Rc<PlayerGlass>> {
    let hwnd = win::hwnd_of(player.window())?;
    PLAYER_GLASS_LOOPS.with(|slot| {
        slot.borrow()
            .iter()
            .filter_map(|weak| weak.upgrade())
            .find(|glass| glass.hwnd.get() == hwnd)
    })
}

/// The main window's last delivered desktop snapshot, written into the player's
/// glass properties before its first frame.
///
/// Returns the grab origin/size so the new loop can re-place that same image for
/// this window's rect (`player_glass_step` does it on its first tick). `target`
/// is the physical rect the window will be shown at, so the placement is valid
/// on the very first painted frame; without it nothing is seeded and the caller
/// keeps the window hidden instead of showing a transparent surface.
fn seed_player_backdrop(
    state: &Rc<PlayerState>,
    player: &PlayerWindow,
    target: Option<WinRect>,
) -> Option<((i32, i32), i32, i32)> {
    let main = state.main.upgrade()?;
    if !main.get_glass_backdrop_valid() {
        crate::debug_log("player glass seed valid=0 reason=main-backdrop-invalid");
        return None;
    }
    let image = main.get_glass_backdrop();
    if image.size().width == 0 {
        crate::debug_log("player glass seed valid=0 reason=main-backdrop-empty");
        return None;
    }
    let Some(shot) = glass_loop().and_then(|loop_| loop_.shot.get()) else {
        crate::debug_log("player glass seed valid=0 reason=no-shot-origin");
        return None;
    };
    let Some(target) = target else {
        crate::debug_log("player glass seed valid=0 reason=no-layout-target");
        return None;
    };
    let scale = player.window().scale_factor().max(0.5);
    let placed = glass::place_for(shot.0, shot.1, shot.2, target.x, target.y, scale);
    player.set_glass_backdrop(image.clone());
    player.set_glass_backdrop_valid(true);
    player.set_glass_x(placed.0);
    player.set_glass_y(placed.1);
    player.set_glass_w(placed.2);
    player.set_glass_h(placed.3);
    // The panels read `panel_glass`; the video pump takes it over later.
    player_panel_glass_desktop(player);
    crate::debug_log(&format!(
        "player glass seed valid=1 pre_show=1 shot_origin={},{} shot={}x{} place={:.0},{:.0},{:.0}x{:.0}",
        shot.0 .0, shot.0 .1, shot.1, shot.2, placed.0, placed.1, placed.2, placed.3
    ));
    Some(shot)
}

/// Show a player window that had no backdrop to seed from, after the bounded
/// fallback delay. The create hook already made it frameless; this only maps it.
fn schedule_player_show(player: Weak<PlayerWindow>, delay_ms: u64) {
    slint::Timer::single_shot(Duration::from_millis(delay_ms), move || {
        let Some(player) = player.upgrade() else {
            return;
        };
        let prepared = win::prepare_frameless(player.window());
        if player.show().is_ok() {
            if !prepared {
                win::prepare_frameless(player.window());
            }
            crate::debug_log(&format!("player show deferred_after_ms={delay_ms}"));
        }
    });
}

/// Create one player window's glass loop, register it for frame delivery and
/// start its 33ms tick.
///
/// The grab worker is attached later ([`start_player_glass`]), once the native
/// handle exists; until then the tick only re-places the seeded image.
fn create_player_glass(
    player: &PlayerWindow,
    control: &Arc<player::PlaybackControl>,
    shot: Option<((i32, i32), i32, i32)>,
) -> Rc<PlayerGlass> {
    let hwnd = win::hwnd_of(player.window()).unwrap_or(0);
    let glass = Rc::new(PlayerGlass::new(hwnd, Arc::downgrade(control)));
    if let Some(shot) = shot {
        glass.shot.set(Some(shot));
    }
    PLAYER_GLASS_LOOPS.with(|loops| loops.borrow_mut().push(Rc::downgrade(&glass)));
    let weak = player.as_weak();
    let loop_state = Rc::downgrade(&glass);
    glass.timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(33),
        move || {
            if let Some(glass) = loop_state.upgrade() {
                player_glass_step(&weak, &glass);
            }
        },
    );
    glass
}

/// Attach a player window's grab worker once its native handle exists.
///
/// Called from the frameless retry chain (`retry_subclass`); the loop and its
/// timer were already created at open. Idempotent per session, torn down by
/// [`stop_player_glass`].
fn start_player_glass(session: &Rc<PlayerSession>) {
    let Some(hwnd) = win::hwnd_of(session.window.window()) else {
        return;
    };
    let Some(glass) = session.glass.borrow().as_ref().cloned() else {
        return;
    };
    if glass.worker.borrow().is_some() {
        return;
    }
    glass.hwnd.set(hwnd);
    // Compositor alpha + DWM chrome, exactly like the main window: without it
    // the player's transparent background would composite as opaque black.
    win::setup_glass(session.window.window(), true);
    let deliver_weak = session.window.as_weak();
    let worker = glass::GlassWorker::spawn(hwnd, move |frame| {
        let weak = deliver_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(player) = weak.upgrade() {
                player_glass_deliver(&player, frame);
            }
        });
    });
    *glass.worker.borrow_mut() = Some(worker);
    crate::debug_log(&format!(
        "player glass worker spawn hwnd={hwnd} win={}",
        session.id
    ));
}

/// Stop one player window's frosted-glass loop.
///
/// Stops the timer here (UI thread, the only place a `slint::Timer` may be
/// touched) and hands the worker to a throwaway thread: `GlassWorker`'s `Drop`
/// joins the worker thread, which can be in the middle of a GDI grab + blur, so
/// joining it on the UI thread would freeze the close for that long. The
/// delivery closure only uses `slint::invoke_from_event_loop` and compares the
/// HWND, and this loop is unregistered below, so a late frame is dropped instead
/// of reaching a closed window. Other windows' loops are untouched. Idempotent.
fn stop_player_glass(session: &Rc<PlayerSession>) {
    let Some(glass) = session.glass.borrow_mut().take() else {
        return;
    };
    // Turning the video glass off here means a closed player never leaves the
    // decode pipeline downscaling frames for a window that is gone.
    if glass.video_glass_on.replace(false) {
        if let Some(control) = glass.control.upgrade() {
            control.set_video_glass_needed(false);
        }
        crate::debug_log("panel glass source=desktop reason=close");
    }
    glass.timer.stop();
    PLAYER_GLASS_LOOPS.with(|slot| {
        let mut loops = slot.borrow_mut();
        loops.retain(|weak| {
            weak.upgrade()
                .map(|current| !Rc::ptr_eq(&current, &glass))
                .unwrap_or(false)
        });
    });
    // Take the worker out of the loop and drop it off the UI thread. The rest
    // of `glass` (plain cells) is dropped here, cheaply.
    let worker = glass.worker.borrow_mut().take();
    if let Some(worker) = worker {
        // Timed so the cost the old UI-thread join used to add to every close
        // stays visible: `close profile ... glass=` no longer includes it.
        let started = Instant::now();
        std::thread::spawn(move || {
            drop(worker);
            crate::debug_log(&format!(
                "player glass: worker joined after_ms={}",
                started.elapsed().as_millis()
            ));
        });
    }
    crate::debug_log(&format!(
        "player glass: loop stopped win={}",
        session.id
    ));
}

/// The player chrome's pointer watch cadence: 60 Hz, the same frame cadence as
/// a 60 Hz display. One `GetCursorPos` + one `GetWindowRect` per tick.
const POINTER_WATCH: Duration = Duration::from_millis(16);
/// The drag flush cadence: one window placement per display frame at 120 Hz.
/// The flush itself skips a tick whose target did not move, so a 60 Hz panel
/// simply sees half as many placements; the achieved rate is logged as
/// `drag flush hz=…` at the end of every drag.
const DRAG_FRAME: Duration = Duration::from_millis(8);

/// Per-drag tally of the frame-locked move timer.
///
/// `events` counts the pointer moves the drag path recorded, `placements` the
/// `SetWindowPos` calls the timer actually applied, and the rate is derived
/// from the drag's own duration when it ends. Counting here (rather than in
/// `win::WindowDrag`) keeps the log line independent of that module's private
/// profile counters.
struct DragFlushStats {
    started: Cell<Option<Instant>>,
    events: Cell<u64>,
    placements: Cell<u64>,
}

impl DragFlushStats {
    fn new() -> Self {
        Self {
            started: Cell::new(None),
            events: Cell::new(0),
            placements: Cell::new(0),
        }
    }

    /// Begin a drag: reset both counters and start the clock.
    fn begin(&self) {
        self.started.set(Some(Instant::now()));
        self.events.set(0);
        self.placements.set(0);
    }

    /// One recorded pointer move.
    fn event(&self) {
        self.events.set(self.events.get() + 1);
    }

    /// One applied window placement.
    fn placement(&self) {
        self.placements.set(self.placements.get() + 1);
    }

    /// Log the achieved rate once per drag. The end callback runs after the tick
    /// that released the button, so the second call is a no-op.
    fn finish(&self, source: &str) {
        let Some(started) = self.started.take() else {
            return;
        };
        let seconds = started.elapsed().as_secs_f64();
        let placements = self.placements.get();
        let hz = if seconds > 0.0 {
            placements as f64 / seconds
        } else {
            0.0
        };
        crate::debug_log(&format!(
            "drag flush hz={hz:.1} placements={placements} events={} source={source}",
            self.events.get()
        ));
    }
}

thread_local! {
    /// Timers owned by the QA harnesses. A Slint timer stops as soon as its last
    /// handle drops, and a harness starts its timers from inside a one-shot
    /// closure, so the handle is kept alive here for the process (diagnostics
    /// only; nothing is pushed outside a QA run).
    static QA_TIMERS: RefCell<Vec<Rc<slint::Timer>>> = RefCell::new(Vec::new());
}

/// Keep a harness timer alive after the closure that started it returns.
fn keep_qa_timer(timer: Rc<slint::Timer>) {
    QA_TIMERS.with(|slot| {
        if let Ok(mut timers) = slot.try_borrow_mut() {
            timers.push(timer);
        }
    });
}

/// How long the player window may stay hidden waiting for the own backdrop
/// worker when the main window had no snapshot to seed from yet.
const SEED_SHOW_FALLBACK_MS: u64 = 400;
/// How long a move/resize must settle before the base rect is persisted.
const LAYOUT_SETTLE: Duration = Duration::from_millis(600);

/// Per-player-window UI-thread timers: the 60Hz pointer watch behind
/// `pointer_inside` and the debounced layout persistence.
///
/// One per open player, created with the window and stopped by
/// [`stop_player_loops`] on both close paths, so a closed window never keeps a
/// tick alive.
struct PlayerLoops {
    /// The window the timers belong to, weak so the loop never keeps it alive.
    window: Weak<PlayerWindow>,
    /// The owning window's ordinal, so two players' transitions are tellable
    /// apart in one log.
    id: u64,
    pointer_timer: slint::Timer,
    layout_timer: slint::Timer,
    /// Last `pointer_inside` pushed to the UI: only transitions are written
    /// and logged.
    pointer_inside: Cell<bool>,
    /// Last window rect the layout debounce was armed for.
    layout_seen: Cell<Option<(i32, i32, i32, i32)>>,
    /// Last `(physical width, physical height, fullscreen)` published as the
    /// decode-target input. Only transitions are written to the control.
    display_seen: Cell<(u32, u32, bool)>,
}

/// Whether the real cursor is anywhere inside the player window.
///
/// The comparison is done in LOGICAL px (the physical window rect divided by
/// the window's scale factor), the same units `ui/app.slint` lays the chrome
/// out in. This is the whole point of the watch: the chrome stays up while the
/// pointer is over ANY part of the window - buttons, popovers, the panels, the
/// gaps between them - and hides only after it has actually left.
fn player_pointer_inside(player: &PlayerWindow) -> bool {
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
fn start_player_loops(
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
        layout_seen: Cell::new(None),
        display_seen: Cell::new((0, 0, false)),
    });
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
                if loops.pointer_inside.get() != inside {
                    loops.pointer_inside.set(inside);
                    player.set_pointer_inside(inside);
                    crate::debug_log(&format!(
                        "pointer inside={inside} tag=player win={}",
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
fn stop_player_loops(loops: &Rc<PlayerLoops>) {
    let was_running = loops.pointer_timer.running() || loops.layout_timer.running();
    loops.pointer_timer.stop();
    loops.layout_timer.stop();
    if let Some(player) = loops.window.upgrade() {
        player.set_pointer_inside(false);
    }
    if !was_running {
        return;
    }
    if loops.pointer_inside.get() {
        crate::debug_log(&format!(
            "pointer inside=0 tag=player-close win={}",
            loops.id
        ));
    }
    crate::debug_log(&format!("player loops: stopped win={}", loops.id));
}

/// One iteration of the player's backdrop refresh loop.
///
/// Mirrors [`glass_step`], but is deliberately NOT gated by `PLAYER_ACTIVE`:
/// this is the loop that must run WHILE the player is open. Each tick is cheap
/// when nothing changed - it reads the blur slider, queues a grab at most every
/// 400ms (or when the radius moved) and re-places the last delivered frame.
fn player_glass_step(weak: &Weak<PlayerWindow>, glass: &Rc<PlayerGlass>) {
    const GLASS_INTERVAL: Duration = Duration::from_millis(400);
    let Some(player) = weak.upgrade() else {
        return;
    };
    let theme = player.global::<AppTheme>();
    let blur = theme.get_glass_blur();
    let transparency = theme.get_glass_transparency();
    let now = Instant::now();

    if blur <= 0.5 {
        // Slider at zero, same as the main loop: there is no blur layer, the
        // tint alone sits over the compositor's transparent window. `next` is
        // pulled to now so turning the blur back on grabs a fresh backdrop
        // immediately instead of reusing whatever was cached.
        player_panel_glass_release(&player, glass, "blur=0");
        if player.get_glass_backdrop_valid() {
            player.set_glass_backdrop(Image::default());
            player.set_glass_backdrop_valid(false);
            player_panel_glass_desktop(&player);
            crate::debug_log(&format!(
                "player glass step blur=0 backdrop=off transparency={transparency:.0}"
            ));
        }
        glass.next.set(now);
        return;
    }

    let scale = player.window().scale_factor().max(0.5);
    // The same mapping as the main loop: slider 0..100 -> Gaussian sigma in
    // physical pixels, so the visual radius is identical at any DPI scale.
    let sigma = blur / 100.0 * glass::SIGMA_MAX * scale;

    // A grab is due, nothing has been requested yet, or the radius moved since
    // the last request: queue one. The worker coalesces them (only the latest
    // survives) and captures/re-blurs only what it actually needs.
    let recapture = now >= glass.next.get();
    if recapture {
        glass.next.set(now + GLASS_INTERVAL);
    }
    let resigma = (sigma - glass.requested_sigma.get()).abs() > 0.25
        || glass.requested_sigma.get() < 0.0;
    if recapture || resigma {
        glass.requested_sigma.set(sigma);
        if let Some(worker) = glass.worker.borrow().as_ref() {
            worker.request(glass::GlassRequest::Grab {
                sigma,
                scale,
                recapture,
            });
        }
    }

    player_glass_place(&player, glass);
    player_panel_glass(&player, glass);
}

/// Apply one finished player frame on the UI thread.
///
/// The counterpart of [`glass_deliver`]: the loop is found by this player's
/// HWND (several windows can have a loop at once), and a frame whose worker
/// belongs to a closed window is dropped instead of feeding another one.
fn player_glass_deliver(player: &PlayerWindow, frame: glass::GlassFrame) {
    let Some(glass) = player_glass_for(player) else {
        return;
    };
    if win::hwnd_of(player.window()) != Some(glass.hwnd.get()) {
        return;
    }
    glass
        .shot
        .set(Some((frame.shot_origin, frame.shot_w, frame.shot_h)));
    let image = Image::from_rgba8(frame.buffer);
    player.set_glass_backdrop(image.clone());
    player.set_glass_backdrop_valid(true);
    // The panels draw `panel_glass`, which defaults to this image by binding.
    // Once the video pump has written it the binding is gone, so mirror the
    // desktop snapshot explicitly whenever the panels fall back to it.
    if !glass.video_glass_on.get() {
        player.set_panel_glass(image);
        player.set_panel_glass_valid(true);
    }
    if (frame.sigma - glass.logged_sigma.get()).abs() > 0.25 {
        glass.logged_sigma.set(frame.sigma);
        let theme = player.global::<AppTheme>();
        crate::debug_log(&format!(
            "player glass step sigma={:.1} blur={:.0} transparency={:.0}",
            frame.sigma,
            theme.get_glass_blur(),
            theme.get_glass_transparency(),
        ));
    }
    player_glass_apply(player, &glass, frame.placed);
}

/// Write the player backdrop placement, skipping the writes when it is
/// unchanged (the loop runs every 33ms and Slint notifies on every property
/// write).
fn player_glass_apply(player: &PlayerWindow, glass: &PlayerGlass, placed: (f32, f32, f32, f32)) {
    if glass.placed.get() == Some(placed) {
        return;
    }
    glass.placed.set(Some(placed));
    player.set_glass_x(placed.0);
    player.set_glass_y(placed.1);
    player.set_glass_w(placed.2);
    player.set_glass_h(placed.3);
    if !glass.video_glass_on.get() {
        player_panel_glass_desktop(player);
    }
    crate::debug_log(&format!(
        "player glass place x={:.0} y={:.0} w={:.0} h={:.0}",
        placed.0, placed.1, placed.2, placed.3
    ));
}

/// Keep the player's desktop grab registered with the desktop: the grab origin
/// minus the current window origin, logical pixels.
///
/// Pure placement - one `GetWindowRect`, four property writes, no capture and
/// no blur - so it runs on every [`player_glass_step`] tick.
fn player_glass_place(player: &PlayerWindow, glass: &Rc<PlayerGlass>) {
    let Some((shot_origin, shot_w, shot_h)) = glass.shot.get() else {
        return;
    };
    // A minimised window reports a -32000 rect; the previous placement is the
    // one the restore comes back to.
    let Some(hwnd) = win::hwnd_of(player.window()) else {
        return;
    };
    if glass::window_iconic(hwnd) {
        return;
    }
    let Some((wx, wy)) = win::window_origin(player.window()) else {
        return;
    };
    let scale = player.window().scale_factor().max(0.5);
    player_glass_apply(
        player,
        glass,
        glass::place_for(shot_origin, shot_w, shot_h, wx, wy, scale),
    );
}

/// Log the panel-glass wiring as the UI sees it (QA, `BILI_PANEL_GLASS_TEST`).
fn log_panel_glass(player: &PlayerWindow, tag: &str) {
    crate::debug_log(&format!(
        "panel-glass tag={tag} fullscreen={} overlay={} left={} right={} valid={} x={:.0} y={:.0} w={:.0} h={:.0}",
        player.get_fullscreen(),
        player.get_panels_overlay(),
        player.get_left_panel(),
        player.get_right_panel(),
        player.get_panel_glass_valid(),
        player.get_panel_glass_x(),
        player.get_panel_glass_y(),
        player.get_panel_glass_w(),
        player.get_panel_glass_h()
    ));
}

/// Write the desktop-snapshot fallback into the panel-glass properties.
///
/// This is the source the panels had by binding before the video pump wrote
/// them: the same image and placement the chrome uses.
fn player_panel_glass_desktop(player: &PlayerWindow) {
    player.set_panel_glass(player.get_glass_backdrop());
    player.set_panel_glass_valid(player.get_glass_backdrop_valid());
    player.set_panel_glass_x(player.get_glass_x());
    player.set_panel_glass_y(player.get_glass_y());
    player.set_panel_glass_w(player.get_glass_w());
    player.set_panel_glass_h(player.get_glass_h());
}

/// Stop sampling the video and put the panels back on the desktop snapshot.
///
/// Idempotent: only a call that actually turns the video glass off releases
/// the decode pipeline and logs, so the per-tick call costs one `Cell` read.
fn player_panel_glass_release(player: &PlayerWindow, glass: &Rc<PlayerGlass>, reason: &str) {
    if !glass.video_glass_on.replace(false) {
        return;
    }
    if let Some(control) = glass.control.upgrade() {
        control.set_video_glass_needed(false);
    }
    glass.video_glass_generation.set(0);
    glass.video_glass_at.set(None);
    glass.video_glass_rect.set(None);
    player_panel_glass_desktop(player);
    let bakes = glass.video_glass_bakes.replace(0);
    crate::debug_log(&format!(
        "panel glass source=desktop reason={reason} valid={} w={:.0} h={:.0} bakes={bakes}",
        player.get_panel_glass_valid(),
        player.get_panel_glass_w(),
        player.get_panel_glass_h()
    ));
}

/// Sample the VIDEO frame into the panel glass while a panel floats over the
/// picture (fullscreen / `panels_overlay`), and fall back to the desktop
/// snapshot otherwise.
///
/// Runs on the player's 33ms glass tick, so the switch is at worst one tick
/// behind the panel/`fullscreen` property. The decode side publishes a 96×54
/// RGBA8 thumbnail at ~10 fps; this bakes a `slint::Image` from it only when
/// the generation moved and at most every 100ms. With no panel open the
/// pipeline is released (`set_video_glass_needed(false)`), which makes the
/// pacer skip the downscale and drops the retained buffer - the cost is then
/// one relaxed atomic load per decoded frame.
fn player_panel_glass(player: &PlayerWindow, glass: &Rc<PlayerGlass>) {
    const VIDEO_GLASS_STEP: Duration = Duration::from_millis(100);

    let overlay = player.get_panels_overlay() || player.get_fullscreen();
    let panel_open = player.get_right_panel() != 0 || player.get_left_panel() != 0;
    let control = glass.control.upgrade();

    let Some(control) = control else {
        player_panel_glass_release(player, glass, "no-control");
        return;
    };
    if !(overlay && panel_open) {
        player_panel_glass_release(
            player,
            glass,
            if overlay { "no-panel" } else { "windowed" },
        );
        return;
    }
    if !glass.video_glass_on.replace(true) {
        control.set_video_glass_needed(true);
        glass.video_glass_generation.set(0);
        glass.video_glass_at.set(None);
        glass.video_glass_rect.set(None);
        crate::debug_log("panel glass source=video");
    }

    let generation = control.video_glass_generation();
    if generation == 0 || generation == glass.video_glass_generation.get() {
        return;
    }
    // Bake at most every 100ms, but only advance the gate when a frame is
    // actually taken: gating before the generation check aliased with the
    // pacer's own 10 fps publish and halved the visible rate.
    let now = Instant::now();
    if let Some(last) = glass.video_glass_at.get() {
        if now.duration_since(last) < VIDEO_GLASS_STEP {
            return;
        }
    }
    glass.video_glass_at.set(Some(now));
    let Some((rgba, width, height)) = control.video_glass_frame() else {
        return;
    };
    if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
        return;
    }
    glass.video_glass_generation.set(generation);
    glass.video_glass_bakes.set(glass.video_glass_bakes.get().saturating_add(1));
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(&rgba);
    player.set_panel_glass(Image::from_rgba8(buffer));
    player.set_panel_glass_valid(true);

    // The strip clips this image at its own window offset, so the rect is the
    // whole window (the picture fills it in overlay mode) and every panel sees
    // the part of the video that is actually behind it.
    let Some((_, _, window_w, window_h)) = win::window_rect(player.window()) else {
        return;
    };
    let scale = f64::from(player.window().scale_factor().max(0.2));
    let rect = (
        0.0,
        0.0,
        (f64::from(window_w) / scale) as f32,
        (f64::from(window_h) / scale) as f32,
    );
    if glass.video_glass_rect.get() != Some(rect) {
        glass.video_glass_rect.set(Some(rect));
        player.set_panel_glass_x(rect.0);
        player.set_panel_glass_y(rect.1);
        player.set_panel_glass_w(rect.2);
        player.set_panel_glass_h(rect.3);
    }
}

/// Retry installing the frameless subclass on the player and mini windows until
/// their native handle exists. Secondary windows are mapped several frames after
/// `show()`, so the retries run on a short repeating timer (event-loop turns fire
/// too early). The hook is idempotent, so the timer simply stops once both
/// windows are closed.
fn retry_subclass(state: &Rc<PlayerState>) {
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
            // subclass and attaches its own grab worker once its HWND exists.
            for session in &players {
                let player = &session.window;
                if session.hwnd.get() == 0 {
                    if let Some(hwnd) = win::hwnd_of(player.window()) {
                        session.hwnd.set(hwnd);
                    }
                }
                if win::install_subclass(player.window()) {
                    let needs_worker = session
                        .glass
                        .borrow()
                        .as_ref()
                        .map(|glass| glass.worker.borrow().is_none())
                        .unwrap_or(false);
                    if needs_worker {
                        win::log_frame_state(player.window(), "player");
                    }
                    start_player_glass(session);
                }
            }
            match &mini {
                Some(mini) => {
                    if win::install_subclass(mini.window()) && !mini_logged.get() {
                        mini_logged.set(true);
                        win::log_frame_state(mini.window(), "mini");
                    }
                }
                None => mini_logged.set(false),
            }
        },
    );
}

/// Cursor position inside the window in logical px (the units the UI lays out
/// in), or `None` without a handle / readable cursor.
fn cursor_in_window(window: &slint::Window) -> Option<(f64, f64)> {
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
fn drag_source(window: &slint::Window, kind: &str) -> String {
    match cursor_in_window(window) {
        Some((_, y)) if kind == "player" && y < 44.0 => "player:title".to_string(),
        Some(_) if kind == "player" => "player:video".to_string(),
        Some((_, y)) if kind == "main" && y < 40.0 => "main:title".to_string(),
        Some(_) => format!("{kind}:other"),
        None => format!("{kind}:-"),
    }
}

/// `x,y` cursor position inside the window, logical px, for the drag log.
fn drag_pos(window: &slint::Window) -> String {
    match cursor_in_window(window) {
        Some((x, y)) => format!("{x:.0},{y:.0}"),
        None => "-".to_string(),
    }
}

/// The state that decides whether a drag can continue, for the drag log.
fn drag_state(window: &slint::Window) -> String {
    format!(
        "button={} inside={} visible={}",
        u8::from(win::left_button_down()),
        u8::from(win::cursor_inside(window).unwrap_or(false)),
        u8::from(win::is_visible(window).unwrap_or(false)),
    )
}

/// `x,y WxH` native window rect, physical px, for the resize logs.
fn resize_rect_text(window: &slint::Window) -> String {
    match win::window_rect(window) {
        Some((x, y, w, h)) => format!("{x},{y} {w}x{h}"),
        None => "-".to_string(),
    }
}

/// Diagnostic (BILI_DRAG_TEST=1): drive the real absolute drag path with
/// explicit cursor positions, log the native rect after every step, then log
/// the frameless invariant after the moves and after minimize/maximize cycles.
///
/// No synthetic mouse input: the anchor is read once from the live cursor and
/// every following step only supplies a coordinate to `move_to_cursor`.
fn run_drag_test(weak: Weak<AppWindow>, drag: Rc<win::WindowDrag>, tries: u32) {
    let retry_weak = weak.clone();
    let retry_drag = drag.clone();
    slint::Timer::single_shot(Duration::from_millis(250), move || {
        let Some(ui) = retry_weak.upgrade() else {
            return;
        };
        if win::style_bits(ui.window()).is_none() {
            if tries > 0 {
                run_drag_test(retry_weak.clone(), retry_drag.clone(), tries - 1);
            } else {
                crate::debug_log("dragtest window timeout");
            }
            return;
        }
        crate::debug_log("dragtest window-ready");
        win::log_frameless_invariant(ui.window(), "dragtest-before-move");

        if let Err(reason) = drag.start(ui.window()) {
            crate::debug_log(&format!("dragtest start denied reason={reason}"));
        }
        let ((anchor_x, anchor_y), (window_x, window_y)) = drag.anchor();
        crate::debug_log(&format!(
            "dragtest anchor cursor=({anchor_x},{anchor_y}) window=({window_x},{window_y})"
        ));

        for (step, offset) in [10, 20, 30, 30, 30, 30, 30, 30].iter().enumerate() {
            drag.move_to_cursor((anchor_x + offset, anchor_y));
            // One frame's flush - exactly what the 16ms drag timer does. The
            // target is applied here, never inside the pointer event.
            drag.flush(ui.window());
            let actual = win::window_origin(ui.window()).unwrap_or((-1, -1));
            let expected = (window_x + offset, window_y);
            crate::debug_log(&format!(
                "dragtest step={step} cursor_dx={offset} window=({},{}) \
                 expected=({},{}) match={}",
                actual.0,
                actual.1,
                expected.0,
                expected.1,
                u8::from(actual == expected)
            ));
        }

        // Pointer-rate burst: `BURST_SAMPLES` move events per frame is exactly
        // what a 1000 Hz mouse delivers to a 60 Hz display (16 samples per
        // 16ms frame), and one `flush` per frame is the drag timer's job. The
        // `drag profile` line at `end()` therefore reports the real
        // events:placements ratio of the production path.
        const BURST_FRAMES: i32 = 60;
        const BURST_SAMPLES: i32 = 16;
        let mut last_dx = 0;
        for frame in 0..BURST_FRAMES {
            // Within one 16ms frame the pointer has travelled 1px, so all 16
            // samples target (nearly) the same place: the frame's single flush
            // applies one placement and the profile shows one per frame,
            // not one per sample.
            for _sample in 0..BURST_SAMPLES {
                last_dx = 40 + frame;
                drag.move_to_cursor((anchor_x + last_dx, anchor_y));
            }
            drag.flush(ui.window());
        }
        let burst_actual = win::window_origin(ui.window()).unwrap_or((-1, -1));
        let burst_expected = (window_x + last_dx, window_y);
        crate::debug_log(&format!(
            "dragtest burst frames={BURST_FRAMES} samples_per_frame={BURST_SAMPLES} \
             window=({},{}) expected=({},{}) match={}",
            burst_actual.0,
            burst_actual.1,
            burst_expected.0,
            burst_expected.1,
            u8::from(burst_actual == burst_expected)
        ));

        drag.end();
        drag.move_to_cursor((anchor_x + 30, anchor_y));
        drag.flush(ui.window());
        let after_end = win::window_origin(ui.window()).unwrap_or((-1, -1));
        crate::debug_log(&format!(
            "dragtest after-end window=({},{})",
            after_end.0, after_end.1
        ));

        drag.nudge(ui.window(), -30, 0);
        let after_nudge = win::window_origin(ui.window()).unwrap_or((-1, -1));
        crate::debug_log(&format!(
            "dragtest nudge=(-30,0) window=({},{})",
            after_nudge.0, after_nudge.1
        ));
        win::log_frameless_invariant(ui.window(), "dragtest-after-move");

        // ---- drag reliability: N press / move / lost-grab / move / release ----
        //
        // Each cycle is the exact sequence the UI produces, through the same
        // `WindowDrag` calls (no synthetic input): press (`start`), a few move
        // events with one frame flush each, then the CANCEL Slint emits when
        // the window it just moved jumps out from under the cursor - the
        // button is still held, so `end_on_signal(true)` must refuse to end -
        // then more moves, then the real release.
        //
        // `BILI_DRAG_TEST_LEGACY_CANCEL=1` reacts to that cancel the way the
        // code did before the fix (end the drag unconditionally), which is the
        // before/after baseline for "拖几次才行".
        const CYCLES: i32 = 20;
        let legacy_cancel = std::env::var("BILI_DRAG_TEST_LEGACY_CANCEL")
            .map(|value| value == "1")
            .unwrap_or(false);
        let mut started = 0;
        let mut tracked_after_grab = 0;
        for cycle in 0..CYCLES {
            if drag.start(ui.window()).is_ok() {
                started += 1;
            }
            let ((cx, cy), (cx_w, cy_w)) = drag.anchor();
            for step in 1..=3 {
                drag.move_to_cursor((cx + step * 5, cy));
                drag.flush(ui.window());
            }
            // The grab is lost here: Slint cancels while VK_LBUTTON is held.
            if legacy_cancel || drag.end_on_signal(true) {
                drag.end();
            }
            for step in 4..=6 {
                drag.move_to_cursor((cx + step * 5, cy));
                drag.flush(ui.window());
            }
            let after = win::window_origin(ui.window()).unwrap_or((-1, -1));
            let tracked = after == (cx_w + 30, cy_w);
            if tracked {
                tracked_after_grab += 1;
            }
            crate::debug_log(&format!(
                "dragtest cycle={cycle} started=1 moved_after_grab={} window=({},{}) \
                 expected=({},{})",
                u8::from(tracked),
                after.0,
                after.1,
                cx_w + 30,
                cy_w
            ));
            if drag.end_on_signal(false) {
                drag.end();
            }
        }
        crate::debug_log(&format!(
            "dragtest cycles attempted={CYCLES} started={started} \
             tracked_after_grab={tracked_after_grab} legacy_cancel={}",
            u8::from(legacy_cancel)
        ));

        let after_min = weak.clone();
        slint::Timer::single_shot(Duration::from_millis(8000), move || {
            let Some(ui) = after_min.upgrade() else {
                return;
            };
            ui.window().set_minimized(true);
            crate::debug_log("dragtest minimized");
            let after_restore = after_min.clone();
            slint::Timer::single_shot(Duration::from_millis(900), move || {
                let Some(ui) = after_restore.upgrade() else {
                    return;
                };
                ui.window().set_minimized(false);
                let after_max = after_restore.clone();
                slint::Timer::single_shot(Duration::from_millis(900), move || {
                    let Some(ui) = after_max.upgrade() else {
                        return;
                    };
                    win::log_frameless_invariant(ui.window(), "dragtest-after-min-restore");
                    ui.window().set_maximized(true);
                    let after_max_restore = after_max.clone();
                    slint::Timer::single_shot(Duration::from_millis(900), move || {
                        let Some(ui) = after_max_restore.upgrade() else {
                            return;
                        };
                        ui.window().set_maximized(false);
                        let done = after_max_restore.clone();
                        slint::Timer::single_shot(Duration::from_millis(900), move || {
                            let Some(ui) = done.upgrade() else {
                                return;
                            };
                            win::log_frameless_invariant(
                                ui.window(),
                                "dragtest-after-max-restore",
                            );
                            crate::debug_log("dragtest done");
                        });
                    });
                });
            });
        });
    });
}

/// Diagnostic (`BILI_DRAG_RATE_TEST=<ms>`): measure how many window placements
/// per second the `DRAG_FRAME` cadence actually achieves on this machine.
///
/// The drag is started with the same `WindowDrag::start` the UI callback calls,
/// but the tick is driven by this harness instead of the UI's timer: the
/// production tick ends the drag as soon as `VK_LBUTTON` is up, so a drag cannot
/// be held open without a real press (and this test must never synthesise
/// input). Both timers run on the UI thread, the flush timer at exactly
/// `DRAG_FRAME` and an event feeder at half that, each event a new pixel, so
/// every flush has a target to place - the same event → placement ratio a real
/// 120 Hz drag has. `drag flush hz=…` reports the measured result.
fn run_drag_rate_test(
    weak: Weak<AppWindow>,
    drag: Rc<win::WindowDrag>,
    stats: Rc<DragFlushStats>,
    duration: Duration,
    tries: u32,
) {
    let retry_weak = weak.clone();
    let retry_drag = drag.clone();
    let retry_stats = stats.clone();
    slint::Timer::single_shot(Duration::from_millis(1500), move || {
        let Some(ui) = retry_weak.upgrade() else {
            return;
        };
        if win::style_bits(ui.window()).is_none() {
            if tries > 0 {
                run_drag_rate_test(retry_weak, retry_drag, retry_stats, duration, tries - 1);
            } else {
                crate::debug_log("drag-rate window timeout");
            }
            return;
        }
        if let Err(reason) = drag.start(ui.window()) {
            crate::debug_log(&format!("drag-rate start denied reason={reason}"));
            return;
        }
        stats.begin();
        let ((anchor_x, anchor_y), _) = drag.anchor();
        crate::debug_log(&format!(
            "drag-rate start anchor=({anchor_x},{anchor_y}) duration_ms={} frame_ms={}",
            duration.as_millis(),
            DRAG_FRAME.as_millis()
        ));
        // Pointer side: two explicit move events per flush, each a new pixel.
        let feeder = Rc::new(slint::Timer::default());
        {
            let feed_drag = drag.clone();
            let feed_stats = stats.clone();
            let step = Rc::new(Cell::new(0u32));
            feeder.start(slint::TimerMode::Repeated, DRAG_FRAME / 2, move || {
                let n = step.get().wrapping_add(1);
                step.set(n);
                // Oscillate inside a 40px window, so the test window returns to
                // where it started instead of walking across the screen.
                let phase = (n / 40) % 2;
                let offset = (n % 40) as i32;
                let dx = if phase == 0 { offset } else { 40 - offset };
                feed_stats.event();
                feed_drag.move_to_cursor((anchor_x + dx, anchor_y));
            });
        }
        keep_qa_timer(feeder.clone());
        // Placement side: exactly the production tick, one flush per frame.
        let flush = Rc::new(slint::Timer::default());
        {
            let place_weak = weak.clone();
            let place_drag = drag.clone();
            let place_stats = stats.clone();
            let place_timer = flush.clone();
            flush.start(slint::TimerMode::Repeated, DRAG_FRAME, move || {
                let Some(ui) = place_weak.upgrade() else {
                    place_timer.stop();
                    return;
                };
                if !place_drag.is_dragging() {
                    place_timer.stop();
                    return;
                }
                if place_drag.flush(&ui.window()) {
                    place_stats.placement();
                }
            });
        }
        keep_qa_timer(flush.clone());
        let stop_weak = weak.clone();
        let stop_feeder = feeder.clone();
        let stop_flush = flush.clone();
        let stop_drag = drag.clone();
        let stop_stats = stats.clone();
        slint::Timer::single_shot(duration, move || {
            stop_feeder.stop();
            stop_flush.stop();
            if let Some(ui) = stop_weak.upgrade() {
                // Last pending position, exactly like the end callback does.
                if stop_drag.flush(&ui.window()) {
                    stop_stats.placement();
                }
            }
            stop_stats.finish("main");
            stop_drag.end();
            crate::debug_log("drag-rate done");
        });
    });
}

/// Diagnostic (`BILI_RESIZE_TEST=<ms>`): prove the self-driven resize path does
/// not block the event loop.
///
/// Waits (bounded) for a player with a live stream, starts a real
/// [`win::WindowResize`] gesture on edge 8 (bottom-right), then drives that same
/// state with explicit target rects through [`win::WindowResize::update_to_rect`]
/// one step per `DRAG_FRAME`. This is the cursor-independent sibling of the
/// production `update`: same active flag, same minimum/work-area clamp, same
/// single `SetWindowPos` path, but no dependence on (and no movement of) the
/// real pointer, so the harness needs no synthetic input.
///
/// Once a second it logs the profiler's measured `fps_presented` and the
/// playhead. The video keeps playing because no OS modal loop is ever entered;
/// the run ends with `resize test done fps_min=… pos_advanced=…`, then restores
/// the window to the rect it found and closes the gesture (`resize end` /
/// `resize profile` lines come from `win.rs`).
fn run_resize_test(
    weak: Weak<PlayerWindow>,
    resize: Rc<win::WindowResize>,
    duration: Duration,
    tries: u32,
) {
    let retry_weak = weak.clone();
    let retry_resize = resize.clone();
    slint::Timer::single_shot(Duration::from_millis(500), move || {
        let Some(player) = retry_weak.upgrade() else {
            return;
        };
        let playing = player::stats_snapshot()
            .map(|stats| {
                // `fps_presented` is measured over a ~1s window that includes
                // decoder start-up (first frame can arrive seconds in), so a
                // non-zero reading is not enough: wait for the window to reach
                // the stream's own rate before timing the resize.
                let floor = if stats.declared_fps > 0.0 {
                    stats.declared_fps * 0.9
                } else {
                    24.0
                };
                stats.fps_presented > 0.0 && stats.fps_presented >= floor
            })
            .unwrap_or(false);
        if win::style_bits(player.window()).is_none() || !playing {
            if tries > 0 {
                run_resize_test(retry_weak, retry_resize, duration, tries - 1);
            } else {
                crate::debug_log("resize test video timeout");
            }
            return;
        }
        let Some((x, y, w, h)) = win::window_rect(player.window()) else {
            crate::debug_log("resize test no-rect");
            return;
        };
        if let Err(reason) = resize.start(player.window(), 8) {
            crate::debug_log(&format!("resize test start denied reason={reason}"));
            return;
        }
        crate::debug_log(&format!(
            "resize test start duration_ms={} rect={x},{y} {w}x{h} edge=8",
            duration.as_millis()
        ));
        let started = Instant::now();
        let last_log = Cell::new(started);
        let min_fps = Cell::new(f32::MAX);
        let last_pos = Cell::new(None::<u64>);
        let advanced = Cell::new(false);
        let frame = Cell::new(0u32);
        let timer = Rc::new(slint::Timer::default());
        let tick_timer = Rc::downgrade(&timer);
        let tick_weak = weak.clone();
        let tick_resize = resize.clone();
        timer.start(slint::TimerMode::Repeated, DRAG_FRAME, move || {
            let Some(timer) = tick_timer.upgrade() else {
                return;
            };
            let Some(player) = tick_weak.upgrade() else {
                timer.stop();
                return;
            };
            let elapsed = started.elapsed();
            let mut fps = 0.0;
            let mut pos = 0;
            if let Some(stats) = player::stats_snapshot() {
                fps = stats.fps_presented;
                pos = stats.position_ms;
                if fps < min_fps.get() {
                    min_fps.set(fps);
                }
                if let Some(previous) = last_pos.get() {
                    if pos > previous {
                        advanced.set(true);
                    }
                }
                last_pos.set(Some(pos));
            }
            if last_log.get().elapsed() >= Duration::from_millis(1000) {
                last_log.set(Instant::now());
                crate::debug_log(&format!(
                    "resize test t_ms={} fps={fps:.1} pos_ms={pos} rect={}",
                    elapsed.as_millis(),
                    resize_rect_text(player.window())
                ));
            }
            if elapsed >= duration {
                // Report, then close through the UI's OWN end callback: that is
                // what runs the release-time layout persist (`player layout
                // save ... tag=resize-end`), exactly as a real rim release
                // does. The grown rect is left in place for that write.
                crate::debug_log(&format!(
                    "resize test done fps_min={:.1} pos_advanced={} rect={}",
                    min_fps.get().min(fps),
                    u8::from(advanced.get()),
                    resize_rect_text(player.window())
                ));
                player.invoke_resize_end();
                timer.stop();
                // Second gesture: put the window back exactly where the test
                // found it, and end through the same callback so the original
                // layout is persisted again (the test leaves no trace).
                let restore_weak = tick_weak.clone();
                let restore_resize = tick_resize.clone();
                slint::Timer::single_shot(Duration::from_millis(900), move || {
                    let Some(player) = restore_weak.upgrade() else {
                        return;
                    };
                    if let Err(reason) = restore_resize.start(player.window(), 8) {
                        crate::debug_log(&format!(
                            "resize test restore denied reason={reason}"
                        ));
                        return;
                    }
                    restore_resize.update_to_rect(
                        player.window(),
                        (x, y, x.saturating_add(w), y.saturating_add(h)),
                    );
                    player.invoke_resize_end();
                    crate::debug_log(&format!(
                        "resize test restored rect={}",
                        resize_rect_text(player.window())
                    ));
                });
                return;
            }
            // Triangle wave over 40 quarter-steps: grow to +152x+95 and back
            // again, one step every four frames (~32ms), so the window moves at
            // the display cadence for the whole run instead of jumping.
            let n = frame.get().wrapping_add(1);
            frame.set(n);
            let phase = ((n / 4) % 40) as i32;
            let k = if phase < 20 { phase } else { 40 - phase };
            tick_resize.update_to_rect(
                player.window(),
                (
                    x,
                    y,
                    x.saturating_add(w).saturating_add(k * 8),
                    y.saturating_add(h).saturating_add(k * 5),
                ),
            );
        });
        keep_qa_timer(timer);
    });
}

/// Diagnostic (`BILI_VOL_TEST=<n>`): apply `count` rapid volume changes to the
/// live player and report what actually landed.
///
/// Waits (bounded) for a player window whose `ffplay` is running, then performs
/// every change on the UI thread — the same thread the volume slider runs on —
/// with no sleep between them. Logs the first and last three applied values, the
/// restart counter delta and one `audio-session` readback, so "no restart" is
/// visible without reading the whole log.
fn run_volume_test(count: u32) {
    std::thread::spawn(move || {
        let mut control = None;
        for _ in 0..900 {
            if let Some(candidate) = player::active_control() {
                let pid = candidate.audio_pid();
                // The session exists a few ms after the process: wait for the
                // real per-app audio session, otherwise this measures the
                // cold-start fallback instead of the steady state.
                if pid != 0 && win::audio_session_get(pid).is_some() {
                    control = Some(candidate);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let Some(control) = control else {
            crate::debug_log("volume test no audio session");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel::<(Vec<u64>, u64, u32)>();
        let run_control = control.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let pid = run_control.audio_pid();
            let before = run_control.audio_restart_count();
            let mut applied: Vec<u64> = Vec::with_capacity(count as usize);
            for step in 0..count {
                // A deterministic zig-zag over 40..=100, so all 50 values differ
                // and the last one is not the first one again.
                let value = 40 + ((step * 37) % 61) as i32;
                run_control.set_volume(value);
                applied.push(run_control.volume());
            }
            let restarts = run_control.audio_restart_count().saturating_sub(before);
            let _ = tx.send((applied, restarts, pid));
        });
        let Ok((applied, restarts, pid)) = rx.recv_timeout(Duration::from_secs(15)) else {
            crate::debug_log("volume test ui timeout");
            return;
        };
        let first: Vec<String> = applied.iter().take(3).map(u64::to_string).collect();
        let last: Vec<String> = applied
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(u64::to_string)
            .collect();
        crate::debug_log(&format!(
            "volume test n={} pid={pid} first={} last={} restarts={restarts}",
            applied.len(),
            first.join(","),
            last.join(","),
        ));
        // One readback once the last write has settled.
        std::thread::sleep(Duration::from_millis(400));
        match win::audio_session_get(pid) {
            Some((level, mute)) => crate::debug_log(&format!(
                "audio-session pid={pid} volume={level:.2} mute={} desired={:.2}/{}",
                u8::from(mute),
                control.volume() as f32 / 100.0,
                u8::from(control.muted()),
            )),
            None => crate::debug_log(&format!("audio-session pid={pid} readback=none")),
        }
    });
}

/// One floating page bar button, routed by the single dispatcher.
#[derive(Clone, Copy)]
enum PageAction {
    First,
    Prev,
    Next,
    Refresh,
}

/// `BILI_PAGE_TEST` value: `<n>` pages the feed n times, `<view>:<n>` first
/// opens that section and then pages it n times. A malformed value yields
/// `(0, 0)` (nothing runs).
fn parse_page_test(value: &str) -> (i32, u32) {
    let value = value.trim();
    match value.split_once(':') {
        Some((view, count)) => (
            view.trim().parse::<i32>().unwrap_or(0).clamp(0, 19),
            count.trim().parse::<u32>().unwrap_or(0),
        ),
        None => (0, value.parse::<u32>().unwrap_or(0)),
    }
}

/// Diagnostic (`BILI_PAGE_TEST`): after the section's first list load, invoke
/// `page_next` `count` times `gap` apart, so the no-scroll client paging can be
/// exercised end to end from the backend without synthetic input. A non-feed
/// view is first opened through its existing navigation callback (0 feed /
/// 1 搜索 / 5 历史 / 6 稍后再看 / 8 分区 / 9 排行榜 / 10 热门 / 13 动态 /
/// 18 入站必刷); `probe` is that section's row store, awaited so the paging
/// starts only once its rows are up. `BILI_PAGE_TEST_GAP_MS` overrides the gap.
fn run_page_test(
    weak: Weak<AppWindow>,
    view: i32,
    count: u32,
    gap: Duration,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    probe: Arc<Mutex<Vec<String>>>,
) {
    std::thread::spawn(move || {
        // The feed is the app's baseline list: waiting for it also waits for
        // the live session, so a section opened below has data to load.
        for _ in 0..480 {
            if feed_store
                .lock()
                .map(|guard| !guard.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if view != 0 {
            crate::debug_log(&format!("page-test open view={view}"));
            let weak_open = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_open.upgrade() {
                    match view {
                        1 => ui.invoke_do_search(SharedString::from("bilibili")),
                        5 => ui.invoke_nav_history(),
                        6 => ui.invoke_nav_toview(),
                        8 => ui.invoke_open_region(0),
                        9 => ui.invoke_nav_ranking(),
                        10 => ui.invoke_nav_popular(),
                        13 => ui.invoke_nav_dynamic(),
                        18 => ui.invoke_nav_precious(),
                        _ => {}
                    }
                }
            });
            // Wait for that section's own rows, then let its first page settle
            // (the covers stream in after the text rows).
            for _ in 0..240 {
                if probe
                    .lock()
                    .map(|guard| !guard.is_empty())
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
        for index in 0..count {
            crate::debug_log(&format!("page-test next view={view} n={}", index + 1));
            let weak_next = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_next.upgrade() {
                    ui.invoke_page_next();
                }
            });
            std::thread::sleep(gap);
        }
        crate::debug_log(&format!("page-test done view={view} n={count}"));
    });
}

// ---------------------------------------------------------------------------
// Client paging (the no-scroll lists).
//
// Every paged grid renders exactly one screen of whole rows from the absolute
// card index `list_offset`. Rust owns that index: it is `client page × list
// page size`, where the page size is the user's persisted cards-per-page
// setting (`settings.json` `page_size`, one of the 设置 menu's steps). Rust
// drives the UI's `list_page_size` property from that setting — the UI only
// reports changes back through `page_size_changed` — and the counters below
// live on the UI thread. Every loader re-publishes the offset once its data is
// in the model, so the pill, the grid and the data can never disagree.
//
// Two families of sections share the mechanism:
//   * sections fetched with `ps = list_page_size` (搜索 / 收藏夹内容 / 分区 /
//     热门 / 空间) show exactly one API page per screen, so their client page
//     is the API page and the offset stays 0;
//   * sections whose data comes in fixed-size batches or pages (首页推荐 /
//     历史 / 动态 / 稍后再看 / 排行榜 / 入站必刷) keep several client pages per
//     loaded data set; the counter walks those pages and the next one past the
//     end fetches (append for the cursors, a new batch for the feed).
// ---------------------------------------------------------------------------

/// Per-view client page (0-based over the loaded data), indexed by `view_mode`.
#[derive(Default)]
struct ClientPages {
    pages: [i64; 20],
}

impl ClientPages {
    fn get(&self, view: i32) -> i64 {
        usize::try_from(view)
            .ok()
            .and_then(|view| self.pages.get(view))
            .copied()
            .unwrap_or(0)
            .max(0)
    }

    fn set(&mut self, view: i32, page: i64) {
        if let Ok(view) = usize::try_from(view) {
            if let Some(slot) = self.pages.get_mut(view) {
                *slot = page.max(0);
            }
        }
    }
}

thread_local! {
    /// The client pages of the sections, UI-thread only like the other drivers
    /// (the page dispatcher and every loader's UI hand-off run here).
    static CLIENT_PAGES: RefCell<ClientPages> = RefCell::new(ClientPages::default());
}

/// UI thread: the client page of `view` (0-based).
fn client_page(view: i32) -> i64 {
    CLIENT_PAGES.with(|cell| cell.borrow().get(view))
}

/// UI thread: set the client page of `view` (clamped at 0).
fn set_client_page(view: i32, page: i64) {
    CLIENT_PAGES.with(|cell| cell.borrow_mut().set(view, page));
}

/// Client page size of the section on screen: the UI's `list_page_size`, which
/// Rust drives from the stored cards-per-page setting. An early read before the
/// first publish (or a stray non-positive value) is clamped to one card, so the
/// `page × size` arithmetic below can never divide by zero.
fn list_page_size(ui: &AppWindow) -> i64 {
    i64::from(ui.get_list_page_size()).max(1)
}

/// UI thread: the page size to fetch with, read before a worker thread starts
/// (the property is UI-thread only and follows the 设置 menu, not a resize).
fn current_page_size(weak: &Weak<AppWindow>) -> i64 {
    weak.upgrade().map(|ui| list_page_size(&ui)).unwrap_or(1)
}

/// Number of client pages a list of `len` rows spans at `size` per page (at
/// least one, so an empty list still reads "1 / 1").
fn page_count(size: i64, len: usize) -> i64 {
    let size = size.max(1);
    if len == 0 {
        1
    } else {
        ((len as i64) + size - 1) / size
    }
}

/// Absolute first-card index of client `page` over `len` loaded rows: `page ×
/// size`, pulled back so the last page of a fully loaded list is full when
/// possible. While the section can still fetch (`more`) a partial last page
/// stays where it is, because the next fetch extends it.
fn client_offset(page: i64, size: i64, len: usize, more: bool) -> i64 {
    let size = size.max(1);
    let raw = page.max(0).saturating_mul(size);
    let len = len as i64;
    if len <= 0 {
        return 0;
    }
    if raw + size <= len {
        return raw;
    }
    if more {
        return raw.min(len - 1);
    }
    (len - size).max(0)
}

/// Whether a further client page exists over `len` loaded rows. `more` says the
/// section can still fetch data, so the next page always exists.
fn has_client_next(page: i64, size: i64, len: usize, more: bool) -> bool {
    more || page.max(0) + 1 < page_count(size, len)
}

/// Total pages the search bar shows: `ceil(num_results / page size)`, capped by
/// the API's own page count so a page size the endpoint ignored cannot page
/// past its end. Out-of-range page sizes (the endpoint's default applies) keep
/// the API's own count.
fn search_total(num_results: i64, num_pages: i64, size: i64) -> i64 {
    let api = num_pages.max(1);
    match bilibili::page_size_param(size) {
        Some(size) if num_results > 0 => ((num_results + size - 1) / size).clamp(1, api),
        _ => api,
    }
}

/// UI thread: mirror the client page into the counter property the page bar
/// reads for `view` (the sections whose count no other property carries).
fn publish_page_counter(ui: &AppWindow, view: i32, page: i64) {
    let shown = (page + 1).clamp(1, i64::from(i32::MAX)) as i32;
    match view {
        0 => ui.set_feed_page(shown),
        5 => ui.set_history_page(shown),
        13 => ui.set_dynamic_page(shown),
        _ => {}
    }
}

/// UI thread: write `list_offset` for the section on screen and log it, so the
/// `offset = page × size` relation is visible in `BILI_DEBUG_LOG`. `len` is the
/// rows loaded in that section's model, `more` whether it can still fetch.
fn apply_client_offset(ui: &AppWindow, view: i32, len: usize, more: bool) {
    let size = list_page_size(ui);
    let pages = page_count(size, len);
    let mut page = client_page(view);
    if page >= pages {
        // The data no longer covers the page (a shorter reload); keep the
        // offset on the last page instead of pointing past the end.
        page = (pages - 1).max(0);
        set_client_page(view, page);
    }
    let offset = client_offset(page, size, len, more);
    ui.set_list_offset(offset as i32);
    publish_page_counter(ui, view, page);
    // The data page is the API page for the sections fetched with
    // `ps = list_page_size`; their client page stays 0, so log both.
    let data_page = match view {
        1 => ui.get_search_current_page(),
        4 => ui.get_folder_current_page(),
        8 => ui.get_region_current_page(),
        10 => ui.get_popular_current_page(),
        11 => ui.get_space_current_page(),
        _ => 0,
    };
    crate::debug_log(&format!(
        "page bar view={view} page={page} offset={offset} size={size} items={len} more={} data_page={data_page}",
        u8::from(more)
    ));
}

/// UI thread: re-publish the offset of the section on screen and refresh the
/// pill. `list_offset` is one window property shared by every grid, so this runs
/// after each load, on every view switch and on every client page move (it also
/// picks up a page size that moved with a resize).
fn publish_list_offset(ui: &AppWindow) {
    match ui.get_view_mode() {
        0 => apply_client_offset(ui, 0, ui.get_feed_items().row_count(), true),
        1 => apply_client_offset(ui, 1, ui.get_search_items().row_count(), true),
        4 => apply_client_offset(ui, 4, ui.get_folder_items().row_count(), true),
        5 => apply_client_offset(
            ui,
            5,
            ui.get_history_items().row_count(),
            ui.get_history_has_more(),
        ),
        6 => apply_client_offset(ui, 6, ui.get_toview_items().row_count(), false),
        8 => apply_client_offset(ui, 8, ui.get_region_items().row_count(), true),
        9 => apply_client_offset(ui, 9, ui.get_rank_items().row_count(), false),
        10 => apply_client_offset(ui, 10, ui.get_popular_items().row_count(), true),
        11 => apply_client_offset(ui, 11, ui.get_space_items().row_count(), true),
        13 => apply_client_offset(
            ui,
            13,
            ui.get_dynamic_items().row_count(),
            ui.get_dynamic_has_more(),
        ),
        18 => apply_client_offset(ui, 18, ui.get_precious_items().row_count(), false),
        _ => {}
    }
    // A client page move changes the offset without a load, so the pill is
    // refreshed here as well: the counter and the arrows follow the page.
    update_page_bar(ui);
}

/// UI thread: the section on screen was just entered by a navigation switch, so
/// it starts at its first client page (a refresh keeps the page instead).
fn enter_paged_view(ui: &AppWindow) {
    set_client_page(ui.get_view_mode(), 0);
    publish_list_offset(ui);
}

/// Page bar state for a client-paged section (历史 / 动态): the counter is the
/// client page over the loaded pages and prev walks back through them.
fn client_page_bar(
    ui: &AppWindow,
    view: i32,
    len: usize,
    more: bool,
    busy: bool,
) -> (String, bool, bool, bool, bool, bool) {
    let size = list_page_size(ui);
    let page = client_page(view);
    (
        (page + 1).to_string(),
        page > 0,
        page > 0,
        has_client_next(page, size, len, more),
        busy,
        true,
    )
}

/// Page bar state for a single-shot list (稍后再看 / 排行榜 / 入站必刷): the
/// client pages span the loaded rows and there is nothing left to fetch.
fn single_shot_bar(
    ui: &AppWindow,
    view: i32,
    len: usize,
    busy: bool,
) -> (String, bool, bool, bool, bool, bool) {
    let size = list_page_size(ui);
    let page = client_page(view);
    let pages = page_count(size, len);
    (
        format!("{} / {}", (page + 1).min(pages), pages),
        page > 0,
        page > 0,
        page + 1 < pages,
        busy,
        true,
    )
}

/// Push the floating page bar's state from the section that is on screen.
/// Every loader and navigation handler calls this after it changes its
/// section's state, so the pill always mirrors what the view shows. The counter
/// is the client page of the section: the sections fetched with
/// `ps = list_page_size` keep exactly one API page per screen, so their counter
/// is that API page; the client-paged ones read the shared client page.
fn update_page_bar(ui: &AppWindow) {
    let mode = ui.get_view_mode();
    let (text, has_first, has_prev, has_next, busy, can_refresh) = match mode {
        // 首页推荐: a rolling client page over the loaded batch; another batch
        // can always be fetched, so next is always live.
        0 => {
            let page = client_page(0) + 1;
            (
                page.to_string(),
                page > 1,
                page > 1,
                true,
                ui.get_list_refreshing(),
                true,
            )
        }
        // 搜索: `N / M`, M = ceil(num_results / page size) from the API count.
        1 => {
            let page = ui.get_search_current_page().max(1);
            let total = ui.get_search_total_pages().max(1);
            (
                format!("{page} / {total}"),
                page > 1,
                page > 1,
                page < total,
                ui.get_list_refreshing(),
                true,
            )
        }
        // 收藏夹内容 / 分区 / 热门: a rolling API page, one page per screen.
        4 => {
            let page = ui.get_folder_current_page().max(1);
            (
                page.to_string(),
                page > 1,
                page > 1,
                ui.get_folder_has_next(),
                ui.get_list_refreshing(),
                true,
            )
        }
        8 => {
            let page = ui.get_region_current_page().max(1);
            (
                page.to_string(),
                page > 1,
                page > 1,
                ui.get_region_has_next(),
                ui.get_list_refreshing(),
                true,
            )
        }
        10 => {
            let page = ui.get_popular_current_page().max(1);
            (
                page.to_string(),
                page > 1,
                page > 1,
                ui.get_popular_has_next(),
                ui.get_list_refreshing(),
                true,
            )
        }
        // 历史 / 动态: append-only cursors; the counter is the client page over
        // the loaded pages and prev walks back through them.
        5 => client_page_bar(
            ui,
            5,
            ui.get_history_items().row_count(),
            ui.get_history_has_more(),
            ui.get_list_refreshing(),
        ),
        13 => client_page_bar(
            ui,
            13,
            ui.get_dynamic_items().row_count(),
            ui.get_dynamic_has_more(),
            ui.get_list_refreshing(),
        ),
        // Single-shot lists: `N / M` over the loaded rows, nothing to fetch.
        6 => single_shot_bar(ui, 6, ui.get_toview_items().row_count(), ui.get_list_refreshing()),
        9 => single_shot_bar(ui, 9, ui.get_rank_items().row_count(), ui.get_list_refreshing()),
        18 => single_shot_bar(
            ui,
            18,
            ui.get_precious_items().row_count(),
            ui.get_list_refreshing(),
        ),
        // 消息: its grids render every row in their own ScrollView (no
        // `page-offset`), so the pill stays refresh-only here.
        14 => (
            "1 / 1".to_string(),
            false,
            false,
            false,
            ui.get_msg_loading(),
            true,
        ),
        // Non-list views: the bar is hidden, leave it inert.
        _ => (String::new(), false, false, false, false, false),
    };
    ui.set_page_bar_text(text.into());
    ui.set_page_bar_has_first(has_first);
    ui.set_page_bar_has_prev(has_prev);
    ui.set_page_bar_has_next(has_next);
    ui.set_page_bar_busy(busy);
    ui.set_page_bar_can_refresh(can_refresh);
}

/// Publish the list refresh indicator the UI renders in the video-list area:
/// `refreshing` starts and ends an animated indicator and `progress` is its
/// 0..1 fill. The feed publishes pages done over pages total (a determinate
/// bar); every other section publishes 0.0 at the start and 1.0 when the load
/// settles, which the UI renders as an indeterminate sweep. Replaces the old
/// per-section "正在加载…" text lines, so nothing writes those any more.
fn publish_refresh(ui: &AppWindow, refreshing: bool, progress: f32) {
    ui.set_list_refreshing(refreshing);
    ui.set_list_refresh_progress(if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    });
}

fn main() -> Result<()> {
    // Born frameless: the WH_CBT create hook rewrites winit's CREATESTRUCTW
    // style before the first window exists, so the native title bar is never
    // painted - not even for one frame. Must precede AppWindow::new() below.
    win::install_create_hook();

    // Kill all ffmpeg/ffplay children when this process dies, even abruptly.
    job::init();

    // 1 ms timer resolution before the first timer starts: the drag flush is
    // 8 ms (120 Hz) and the pacer's frame sleeps depend on the same winmm
    // refcount. One `Once` for the whole process, never a second period call.
    player::ensure_high_res_timer();

    let ui = AppWindow::new()?;
    let weak = ui.as_weak();
    let generation = Arc::new(AtomicU64::new(1));

    // Session and feed are shared with the player worker threads. `FeedVideo`
    // is `Send`, so both live behind `Arc<Mutex<_>>` rather than `Rc`.
    let session_slot: SessionSlot = Arc::new(Mutex::new(None));
    let feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>> = Arc::new(Mutex::new(Vec::new()));
    // Ready-made feed batches, filled in the background from the moment a
    // session exists (i.e. while the login view is still up).
    let feed_ring: Arc<Mutex<FeedPrefetchRing>> =
        Arc::new(Mutex::new(FeedPrefetchRing::new(FEED_SOURCE_RECOMMEND)));
    // The in-flight 离线下载, if any; only the newest download is retained.
    let download_control: Arc<Mutex<Option<Arc<download::DownloadControl>>>> =
        Arc::new(Mutex::new(None));
    let player_state = Rc::new(PlayerState {
        players: RefCell::new(Vec::new()),
        focused: RefCell::new(None),
        mini: RefCell::new(None),
        mini_owner: Cell::new(0),
        main: weak.clone(),
        frameless_timer: slint::Timer::default(),
        main_frameless_timer: slint::Timer::default(),
        theme_dark: Cell::new(false),
        download_control: download_control.clone(),
    });

    // Persisted preferences (settings.json), shared by the player controls and
    // the settings panel so both stay in sync.
    let settings_store: Arc<Mutex<settings::Settings>> = Arc::new(Mutex::new(settings::load()));

    // Resolve light/dark before the first frame so 跟随系统 never flashes light.
    let theme_dark = settings_store
        .lock()
        .map(|guard| theme::resolve(guard.theme))
        .unwrap_or(false);
    player_state.theme_dark.set(theme_dark);
    ui.global::<AppTheme>().set_dark(theme_dark);
    // The frosted-glass sliders as well, so the settings panel and the 33ms
    // refresh loop start from the stored values rather than from the defaults.
    // The cards-per-page setting drives the Rust-owned `list_page_size`
    // property from here on; the UI only reports menu changes back.
    if let Ok(guard) = settings_store.lock() {
        ui.global::<AppTheme>()
            .set_glass_transparency(guard.glass_transparency);
        ui.global::<AppTheme>().set_glass_blur(guard.glass_blur);
        ui.set_list_page_size(guard.page_size as i32);
    }

    // Search / detail view state, shared with their background workers.
    let search_state = Arc::new(Mutex::new(SearchState::default()));
    let search_seq = Arc::new(AtomicU64::new(0));
    let search_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let detail_state: Arc<Mutex<Option<DetailState>>> = Arc::new(Mutex::new(None));
    let detail_seq = Arc::new(AtomicU64::new(0));
    let related_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // The in-flight 离线下载, if any; only the newest download is retained.
    let download_control: Arc<Mutex<Option<Arc<download::DownloadControl>>>> =
        Arc::new(Mutex::new(None));

    let fav_folders: Arc<Mutex<Vec<bilibili::FavFolder>>> = Arc::new(Mutex::new(Vec::new()));
    let folder_state: Arc<Mutex<FolderState>> = Arc::new(Mutex::new(FolderState::default()));
    let folder_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // 收藏推荐 (home_tab 5): the folder list, the selected tab and the cards
    // behind the masonry, shared with its load workers.
    let fav_view: Arc<Mutex<FavViewState>> = Arc::new(Mutex::new(FavViewState::new()));
    // The 收藏推荐 masonry's videos, which carry each card's owner mid; the UP
    // name on a masonry card resolves through this list, not through the feed.
    let fav_videos: Arc<Mutex<Vec<bilibili::FeedVideo>>> = fav_view
        .lock()
        .map(|state| state.videos.clone())
        .unwrap_or_default();
    let history_page: Arc<Mutex<HistoryPageState>> = Arc::new(Mutex::new(HistoryPageState::default()));
    let history_kids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let history_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let toview_aids: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let toview_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let browse_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let region_state: Arc<Mutex<RegionState>> = Arc::new(Mutex::new(RegionState::default()));
    let popular_state: Arc<Mutex<PopularState>> = Arc::new(Mutex::new(PopularState::default()));
    let rank_rid: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
    let suggest_seq = Arc::new(AtomicU64::new(0));
    let shuffle_seq = Arc::new(AtomicU64::new(1 + bilibili::feed_batch()));
    let hot_keywords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let space_state: Arc<Mutex<SpaceState>> = Arc::new(Mutex::new(SpaceState::default()));
    let space_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let space_prev_mode: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));

    // UP page (the redesigned cards' UP name). Its own overlay and its own
    // published row model, independent of the older space view (11):
    //   up_page   mid / sort of the page on screen, behind `up_page_open`
    //   up_store  the videos behind `up_items`, for `up_open_video`
    //   up_seq    supersedes a slow load, so an old page can never land on a
    //             newer one (a sort switch or a second UP name click).
    let up_page: Arc<Mutex<UpPageState>> = Arc::new(Mutex::new(UpPageState::default()));
    let up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>> = Arc::new(Mutex::new(Vec::new()));
    let up_seq = Arc::new(AtomicU64::new(0));

    let dynamic_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let dynamic_offset: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let dynamic_has_more: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let session_meta: Arc<Mutex<Vec<(i64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let relation_state: Arc<Mutex<RelationState>> = Arc::new(Mutex::new(RelationState::default()));

    // 播放列表: one app-wide queue restored from `settings.json` (no autoplay).
    // Every card/panel callback below goes through this host.
    let playlist_host = Playlist::new(
        weak.clone(),
        Rc::downgrade(&player_state),
        session_slot.clone(),
        settings_store.clone(),
        feed_store.clone(),
        browse_bvids.clone(),
        space_bvids.clone(),
    );
    install_playlist(&playlist_host);
    // The host lives in the UI thread-local; this binding only keeps it alive
    // until the install above (the local itself is not used afterwards).
    drop(playlist_host);

    ui.set_region_labels(ModelRc::from(std::rc::Rc::new(VecModel::from(
        region_labels(),
    ))));
    ui.set_rank_region_labels(ModelRc::from(std::rc::Rc::new(VecModel::from(
        rank_region_labels(),
    ))));
    if let Ok(guard) = settings_store.lock() {
        publish_settings(&ui, &guard);
    }

    // Diagnostic hook (mirrors BILI_PAGE_TEST / BILI_DRAG_TEST): pin the 关闭
    // behaviour before the first frame. It goes through the real persistence
    // path and the real Slint publish, so `settings.json` and the panel prove
    // both without clicking anything.
    if let Ok(value) = std::env::var("BILI_CLOSE_ACTION") {
        match value.trim().parse::<u32>() {
            Ok(raw) => {
                let action = settings::normalise_close_action(raw);
                let snapshot =
                    update_settings(&settings_store, |prefs| prefs.close_action = action);
                publish_settings(&ui, &snapshot);
                crate::debug_log(&format!(
                    "close action={action} stored panel={}",
                    ui.get_set_close_action()
                ));
            }
            Err(err) => crate::debug_log(&format!("close action ignored value={value} err={err}")),
        }
    }
    crate::debug_log(&format!(
        "close action={} in effect",
        close_action(&settings_store)
    ));

    // Refresh button: supersede any in-flight attempt and start a fresh QR login.
    {
        let weak = weak.clone();
        let generation = generation.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let feed_ring = feed_ring.clone();
        let settings_store = settings_store.clone();
        ui.on_refresh(move || {
            start_login(
                weak.clone(),
                generation.clone(),
                session_slot.clone(),
                feed_store.clone(),
                feed_ring.clone(),
                settings_store.clone(),
                false,
            );
        });
    }

    // A feed row was clicked: open a player window for that video.
    {
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_open_video(move |index| {
            open_player(
                index,
                &session_slot,
                &feed_store,
                &player_state,
                &settings_store,
            );
        });
    }

    // Card click with a modifier: the one place that decides what happens to the
    // player windows already up.
    //   0 = left (replace the focused window)
    //   1 = right (a NEW window, the others keep playing)
    //   2 = ctrl+left (enqueue, playback untouched)
    //
    // The index is resolved against the view that is ON SCREEN. Until the UI
    // sends the view with the click, the current view is read here from
    // `view_mode`; the search grid therefore opens the search row that was
    // clicked instead of the same index of the home feed (the wrong-video bug).
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        let search_state = search_state.clone();
        let search_bvids = search_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        let up_page = up_page.clone();
        let up_seq = up_seq.clone();
        let up_store = up_store.clone();
        ui.on_card_open(move |index, mode| {
            let mode_name = match mode {
                1 => "new-window",
                2 => "enqueue",
                _ => "replace",
            };
            let view = weak.upgrade().map_or(0, |ui| ui.get_view_mode());
            crate::debug_log(&format!(
                "card open index={index} mode={mode} ({mode_name}) view={view}"
            ));
            if view == 1 {
                // 搜索结果: the grid was built from `search_bvids`, so the row is
                // resolved through that same list. The feed never participates.
                if mode == 2 {
                    let bvid = search_bvids
                        .lock()
                        .ok()
                        .and_then(|guard| guard.get(index.max(0) as usize).cloned());
                    match (playlist(), bvid) {
                        (Some(host), Some(bvid)) => {
                            crate::debug_log(&format!(
                                "card enqueue view=1 index={index} bvid={bvid}"
                            ));
                            host.enqueue_bvid(bvid, false);
                        }
                        _ => crate::debug_log(&format!(
                            "card enqueue miss view=1 index={index}"
                        )),
                    }
                    return;
                }
                // 用户 tab: the row is a UP主, so a click opens the UP page.
                if open_search_user(
                    weak.clone(),
                    session_slot.clone(),
                    search_state.clone(),
                    up_page.clone(),
                    up_seq.clone(),
                    up_store.clone(),
                    index,
                    "card",
                ) {
                    return;
                }
                if !open_search_index(
                    weak.clone(),
                    session_slot.clone(),
                    search_state.clone(),
                    search_bvids.clone(),
                    detail_state.clone(),
                    detail_seq.clone(),
                    related_bvids.clone(),
                    index,
                    "card",
                ) {
                    crate::debug_log(&format!(
                        "card open miss view=1 index={index} mode={mode}"
                    ));
                }
                return;
            }
            match mode {
                2 => {
                    let before = playlist().and_then(|host| host.current());
                    let len_before = playlist()
                        .map(|host| host.snapshot().0.len())
                        .unwrap_or_default();
                    if let Some(host) = playlist() {
                        host.enqueue_index(index, false);
                    }
                    let len_after = playlist()
                        .map(|host| host.snapshot().0.len())
                        .unwrap_or_default();
                    crate::debug_log(&format!(
                        "card enqueue index={index} items={len_before}->{len_after} current={before:?} playing_unchanged=1"
                    ));
                }
                1 => open_player_with(
                    index,
                    OpenMode::NewWindow,
                    &session_slot,
                    &feed_store,
                    &player_state,
                    &settings_store,
                ),
                _ => open_player_with(
                    index,
                    OpenMode::Replace,
                    &session_slot,
                    &feed_store,
                    &player_state,
                    &settings_store,
                ),
            }
        });
    }

    // UP page: a card's UP name and a search 用户 row both open it.
    //   open_up(index)    the index is resolved against the view on screen
    //                     (`resolve_card_mid`), never against the home feed
    //   open_up_mid(mid)  the search 用户 tab already knows the mid
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let search_state = search_state.clone();
        let folder_state = folder_state.clone();
        let history_page = history_page.clone();
        let fav_videos = fav_videos.clone();
        let up_page = up_page.clone();
        let up_seq = up_seq.clone();
        let up_store = up_store.clone();
        ui.on_open_up(move |index| {
            let (view, up_open, masonry) = weak
                .upgrade()
                .map(|ui| {
                    (
                        ui.get_view_mode(),
                        ui.get_up_page_open(),
                        ui.get_home_tab() == 5,
                    )
                })
                .unwrap_or((0, false, false));
            match resolve_card_mid(
                view,
                index,
                up_open,
                masonry,
                &up_store,
                &feed_store,
                &fav_videos,
                &search_state,
                &folder_state,
                &history_page,
            ) {
                Some(mid) => {
                    crate::debug_log(&format!(
                        "up open index={index} view={view} masonry={} mid={mid}",
                        u8::from(masonry)
                    ));
                    open_up_mid(
                        weak.clone(),
                        session_slot.clone(),
                        up_page.clone(),
                        up_seq.clone(),
                        up_store.clone(),
                        mid,
                    );
                }
                None => crate::debug_log(&format!(
                    "up open miss index={index} view={view} up_open={} masonry={}",
                    u8::from(up_open),
                    u8::from(masonry)
                )),
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let up_page = up_page.clone();
        let up_seq = up_seq.clone();
        let up_store = up_store.clone();
        ui.on_open_up_mid(move |mid| {
            let Ok(mid) = mid.trim().parse::<i64>() else {
                crate::debug_log(&format!("up open miss mid={mid:?} source=search"));
                return;
            };
            crate::debug_log(&format!("up open mid={mid} source=search"));
            open_up_mid(
                weak.clone(),
                session_slot.clone(),
                up_page.clone(),
                up_seq.clone(),
                up_store.clone(),
                mid,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let up_page = up_page.clone();
        let up_seq = up_seq.clone();
        let up_store = up_store.clone();
        ui.on_up_sort_changed(move |sort| {
            let sort = if sort == 1 { 1 } else { 0 };
            let mid = up_page.lock().map(|state| state.mid).unwrap_or(0);
            crate::debug_log(&format!("up sort changed sort={sort} mid={mid}"));
            if mid > 0 {
                load_up_page(
                    weak.clone(),
                    session_slot.clone(),
                    up_page.clone(),
                    up_seq.clone(),
                    up_store.clone(),
                    mid,
                    sort,
                    false,
                );
            }
        });
    }
    {
        let weak = weak.clone();
        let up_page = up_page.clone();
        let up_seq = up_seq.clone();
        let up_store = up_store.clone();
        ui.on_up_close(move || {
            if let Some(ui) = weak.upgrade() {
                close_up_page(&ui, &up_page, &up_seq, &up_store);
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        let up_store = up_store.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_up_open_video(move |index| {
            let index = index.max(0) as usize;
            let video = up_store
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned());
            let Some(video) = video else {
                crate::debug_log(&format!("up open video miss index={index}"));
                return;
            };
            crate::debug_log(&format!(
                "up open video index={index} bvid={} mid={} cid={}",
                video.bvid, video.owner_mid, video.cid
            ));
            if video.cid > 0 {
                let Some(session) = active_session(&session_slot) else {
                    crate::debug_log(&format!(
                        "up open video skipped index={index} reason=no-session"
                    ));
                    return;
                };
                // Replace semantics, exactly like card_open(0): the focused
                // player gives way to this video.
                open_player_inner(
                    session,
                    video,
                    &player_state,
                    &settings_store,
                    OpenMode::Replace,
                );
                return;
            }
            // The arc list carries no cid (see `bilibili::up_videos`), so the
            // detail API resolves it and then starts the player - the same path
            // a favourite or history card takes, and still a replace.
            open_detail(
                weak.clone(),
                session_slot.clone(),
                detail_state.clone(),
                detail_seq.clone(),
                related_bvids.clone(),
                video.bvid,
            );
        });
    }

    // Search box: run a fresh video query and switch to the results view.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let search_state = search_state.clone();
        let search_seq = search_seq.clone();
        let search_bvids = search_bvids.clone();
        let hot_keywords = hot_keywords.clone();
        ui.on_do_search(move |keyword| {
            begin_search(
                weak.clone(),
                session_slot.clone(),
                search_state.clone(),
                search_seq.clone(),
                search_bvids.clone(),
                hot_keywords.clone(),
                keyword.to_string(),
            );
        });
    }

    // Search tab switch (视频 / 用户): reset to page 1 and re-query.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let search_state = search_state.clone();
        let search_seq = search_seq.clone();
        let search_bvids = search_bvids.clone();
        ui.on_search_tab_changed(move |tab| {
            if let Ok(mut state) = search_state.lock() {
                state.tab = tab;
                state.page = 1;
                state.num_pages = 1;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_search_tab(tab);
                set_client_page(1, 0);
            }
            run_search(
                weak.clone(),
                session_slot.clone(),
                search_state.clone(),
                search_seq.clone(),
                search_bvids.clone(),
            );
        });
    }

    // Search pagination: `target` is the 1-based destination page.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let search_state = search_state.clone();
        let search_seq = search_seq.clone();
        let search_bvids = search_bvids.clone();
        ui.on_search_page(move |target| {
            if let Ok(mut state) = search_state.lock() {
                state.page = i64::from(target).max(1);
            }
            run_search(
                weak.clone(),
                session_slot.clone(),
                search_state.clone(),
                search_seq.clone(),
                search_bvids.clone(),
            );
        });
    }

    // A search result was clicked (keyboard/accessibility activation): resolve
    // through `search_bvids`, the list the grid was built from. On the 用户 tab
    // the row is a UP主, so the same activation opens the UP page instead.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let search_state = search_state.clone();
        let search_bvids = search_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        let up_page = up_page.clone();
        let up_seq = up_seq.clone();
        let up_store = up_store.clone();
        ui.on_open_search_item(move |index| {
            if open_search_user(
                weak.clone(),
                session_slot.clone(),
                search_state.clone(),
                up_page.clone(),
                up_seq.clone(),
                up_store.clone(),
                index,
                "activate",
            ) {
                return;
            }
            if !open_search_index(
                weak.clone(),
                session_slot.clone(),
                search_state.clone(),
                search_bvids.clone(),
                detail_state.clone(),
                detail_seq.clone(),
                related_bvids.clone(),
                index,
                "activate",
            ) {
                crate::debug_log(&format!("search open miss index={index} origin=activate"));
            }
        });
    }

    // A feed row was clicked: open the player directly (the feed already
    // carries bvid/cid/title, so no resolve round-trip is needed).
    {
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_open_feed_detail(move |index| {
            open_player(
                index,
                &session_slot,
                &feed_store,
                &player_state,
                &settings_store,
            );
        });
    }

    // ---- 播放列表 (home bar + panel) ----
    // Every callback resolves the queue host from the UI thread-local installed
    // above, so no closure carries queue state of its own.
    ui.on_card_enqueue(move |index| {
        if let Some(host) = playlist() {
            host.enqueue_index(index, false);
        }
    });
    ui.on_card_enqueue_only(move |index| {
        if let Some(host) = playlist() {
            host.enqueue_index(index, true);
        }
    });
    ui.on_playlist_play(move |index| {
        if let Some(host) = playlist() {
            host.play_index(index.max(0) as usize);
        }
    });
    ui.on_playlist_remove(move |index| {
        if let Some(host) = playlist() {
            host.remove(index);
        }
    });
    ui.on_playlist_clear(move || {
        if let Some(host) = playlist() {
            host.clear();
        }
    });
    ui.on_play_next(move || {
        if let Some(host) = playlist() {
            host.step(true);
        }
    });
    ui.on_play_prev(move || {
        if let Some(host) = playlist() {
            host.step(false);
        }
    });
    // The UI advances `play_mode` itself and then calls this; Rust only
    // persists and publishes the value it received (never advances again).
    ui.on_cycle_play_mode(move || {
        if let Some(host) = playlist() {
            host.cycle_mode();
        }
    });

    // Detail view: return to the feed.
    {
        let weak = weak.clone();
        ui.on_back_to_feed(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(0);
                enter_paged_view(&ui);
                update_page_bar(&ui);
            }
        });
    }

    // Detail view: select one 分P.
    {
        let weak = weak.clone();
        ui.on_select_part(move |index| {
            if let Some(ui) = weak.upgrade() {
                ui.set_detail_selected_part(index);
            }
        });
    }

    // Detail view: open a related video.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_open_related(move |index| {
            let bvid = related_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(bvid) = bvid {
                open_detail(
                    weak.clone(),
                    session_slot.clone(),
                    detail_state.clone(),
                    detail_seq.clone(),
                    related_bvids.clone(),
                    bvid,
                );
            }
        });
    }

    // Detail view: play the selected 分P.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        let detail_state = detail_state.clone();
        ui.on_play_detail(move || {
            let selected = weak
                .upgrade()
                .map(|ui| ui.get_detail_selected_part())
                .unwrap_or(0)
                .max(0) as usize;
            let info = detail_state.lock().ok().and_then(|guard| {
                guard.as_ref().map(|detail| {
                    let cid = detail.parts.get(selected).copied().unwrap_or(detail.cid);
                    (detail.bvid.clone(), detail.title.clone(), cid, detail.aid)
                })
            });
            if let Some((bvid, title, cid, aid)) = info {
                open_player_for(
                    &session_slot,
                    &player_state,
                    &settings_store,
                    bvid,
                    cid,
                    title,
                    aid,
                );
            }
        });
    }

    // Detail view: 离线下载 the selected 分P to a local .mp4.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let detail_state = detail_state.clone();
        let download_control = download_control.clone();
        ui.on_download_detail(move || {
            let selected = weak
                .upgrade()
                .map(|ui| ui.get_detail_selected_part())
                .unwrap_or(0)
                .max(0) as usize;
            let info = detail_state.lock().ok().and_then(|guard| {
                guard.as_ref().map(|detail| {
                    let cid = detail.parts.get(selected).copied().unwrap_or(detail.cid);
                    (detail.bvid.clone(), detail.title.clone(), cid)
                })
            });
            let Some((bvid, title, cid)) = info else {
                return;
            };
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            let control = download::DownloadControl::new();
            if let Ok(mut guard) = download_control.lock() {
                if let Some(old) = guard.take() {
                    old.cancel();
                }
                *guard = Some(control.clone());
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_download_busy(true);
                ui.set_download_status("正在准备下载…".into());
            }
            download::spawn(session, bvid, cid, title, weak.clone(), control);
        });
    }

    // Detail view: cancel the in-flight 离线下载.
    {
        let download_control = download_control.clone();
        ui.on_cancel_download(move || {
            if let Ok(guard) = download_control.lock() {
                if let Some(control) = guard.as_ref() {
                    control.cancel();
                }
            }
        });
    }

    // Subscription list navigation: 收藏夹 / 历史 / 稍后再看.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let fav_folders = fav_folders.clone();
        ui.on_nav_favorites(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(3);
            }
            load_fav_folders(weak.clone(), session_slot.clone(), fav_folders.clone());
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let history_page = history_page.clone();
        let history_kids = history_kids.clone();
        let history_bvids = history_bvids.clone();
        ui.on_nav_history(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(5);
                enter_paged_view(&ui);
            }
            if let Ok(mut state) = history_page.lock() {
                *state = HistoryPageState::default();
            }
            load_history(
                weak.clone(),
                session_slot.clone(),
                history_page.clone(),
                history_kids.clone(),
                history_bvids.clone(),
                false,
                None,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let toview_aids = toview_aids.clone();
        let toview_bvids = toview_bvids.clone();
        ui.on_nav_toview(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(6);
                enter_paged_view(&ui);
            }
            load_toview(
                weak.clone(),
                session_slot.clone(),
                toview_aids.clone(),
                toview_bvids.clone(),
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let fav_folders = fav_folders.clone();
        let folder_state = folder_state.clone();
        let folder_bvids = folder_bvids.clone();
        ui.on_open_fav_folder(move |fid| {
            let Ok(fid) = fid.parse::<i64>() else {
                return;
            };
            let title = fav_folders
                .lock()
                .ok()
                .and_then(|guard| {
                    guard
                        .iter()
                        .find(|folder| folder.id == fid)
                        .map(|folder| folder.name.clone())
                })
                .unwrap_or_default();
            if let Ok(mut state) = folder_state.lock() {
                state.media_id = fid;
                state.title = title;
                state.page = 1;
                state.has_more = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(4);
                enter_paged_view(&ui);
            }
            load_folder_page(
                weak.clone(),
                session_slot.clone(),
                folder_state.clone(),
                folder_bvids.clone(),
                1,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let folder_state = folder_state.clone();
        let folder_bvids = folder_bvids.clone();
        ui.on_fav_folder_page(move |delta| {
            let (target, has_prev, has_next) = match folder_state.lock() {
                Ok(state) => (
                    (state.page + i64::from(delta)).max(1),
                    state.page > 1,
                    state.has_more,
                ),
                Err(_) => return,
            };
            if (delta < 0 && !has_prev) || (delta > 0 && !has_next) {
                return;
            }
            load_folder_page(
                weak.clone(),
                session_slot.clone(),
                folder_state.clone(),
                folder_bvids.clone(),
                target,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let folder_bvids = folder_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_open_folder_item(move |index| {
            let bvid = folder_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(bvid) = bvid {
                open_detail(
                    weak.clone(),
                    session_slot.clone(),
                    detail_state.clone(),
                    detail_seq.clone(),
                    related_bvids.clone(),
                    bvid,
                );
            }
        });
    }

    // ---- 收藏推荐 (home_tab 5): folder tabs + masonry ----
    // The UI owns 全部 and lists only the user's own folders in `fav_tabs`;
    // `fav_tab == 0` is 全部 and `i + 1` is `fav_tabs[i]`.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let fav_view = fav_view.clone();
        ui.on_home_section(move |section| {
            if section == 5 {
                fav_start(weak.clone(), session_slot.clone(), fav_view.clone());
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let fav_view = fav_view.clone();
        ui.on_fav_tab_changed(move |tab| {
            fav_load_tab(weak.clone(), session_slot.clone(), fav_view.clone(), tab);
        });
    }
    {
        let session_slot = session_slot.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        let fav_view = fav_view.clone();
        ui.on_masonry_open(move |bvid| {
            let video = fav_view.lock().ok().and_then(|state| {
                state.videos.lock().ok().and_then(|videos| {
                    videos
                        .iter()
                        .find(|video| video.bvid.as_str() == bvid.as_str())
                        .cloned()
                })
            });
            let Some(video) = video else {
                crate::debug_log(&format!("fav view open bvid={bvid} reason=not_found"));
                return;
            };
            crate::debug_log(&format!(
                "fav view open bvid={} cid={} aid={}",
                video.bvid, video.cid, video.aid
            ));
            open_player_for(
                &session_slot,
                &player_state,
                &settings_store,
                video.bvid,
                video.cid,
                video.title,
                video.aid as i64,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let history_bvids = history_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_open_history_item(move |index| {
            let bvid = history_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(bvid) = bvid {
                if !bvid.is_empty() {
                    open_detail(
                        weak.clone(),
                        session_slot.clone(),
                        detail_state.clone(),
                        detail_seq.clone(),
                        related_bvids.clone(),
                        bvid,
                    );
                }
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let history_page = history_page.clone();
        let history_kids = history_kids.clone();
        let history_bvids = history_bvids.clone();
        ui.on_history_delete(move |index| {
            let kid = history_kids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            let Some(kid) = kid else {
                return;
            };
            let weak = weak.clone();
            let session_slot = session_slot.clone();
            let history_page = history_page.clone();
            let history_kids = history_kids.clone();
            let history_bvids = history_bvids.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                match runtime.block_on(session.history_delete(&kid)) {
                    Ok(()) => {
                        if let Ok(mut state) = history_page.lock() {
                            *state = HistoryPageState::default();
                        }
                        load_history(
                            weak,
                            session_slot,
                            history_page,
                            history_kids,
                            history_bvids,
                            false,
                            None,
                        );
                    }
                    Err(err) => {
                        crate::debug_log(&format!("history_delete err={err:#}"));
                        let message = format!("删除失败：{err}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_history_status(message.into());
                            }
                        });
                    }
                }
            });
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let history_page = history_page.clone();
        let history_kids = history_kids.clone();
        let history_bvids = history_bvids.clone();
        ui.on_history_clear(move || {
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            let weak = weak.clone();
            let session_slot = session_slot.clone();
            let history_page = history_page.clone();
            let history_kids = history_kids.clone();
            let history_bvids = history_bvids.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                match runtime.block_on(session.history_clear()) {
                    Ok(()) => {
                        if let Ok(mut state) = history_page.lock() {
                            *state = HistoryPageState::default();
                        }
                        load_history(
                            weak,
                            session_slot,
                            history_page,
                            history_kids,
                            history_bvids,
                            false,
                            None,
                        );
                    }
                    Err(err) => {
                        crate::debug_log(&format!("history_clear err={err:#}"));
                        let message = format!("清空失败：{err}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_history_status(message.into());
                            }
                        });
                    }
                }
            });
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let history_page = history_page.clone();
        let history_kids = history_kids.clone();
        let history_bvids = history_bvids.clone();
        ui.on_history_load_more(move || {
            load_history(
                weak.clone(),
                session_slot.clone(),
                history_page.clone(),
                history_kids.clone(),
                history_bvids.clone(),
                true,
                None,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let toview_bvids = toview_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_open_toview_item(move |index| {
            let bvid = toview_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(bvid) = bvid {
                open_detail(
                    weak.clone(),
                    session_slot.clone(),
                    detail_state.clone(),
                    detail_seq.clone(),
                    related_bvids.clone(),
                    bvid,
                );
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let toview_aids = toview_aids.clone();
        let toview_bvids = toview_bvids.clone();
        ui.on_toview_remove(move |index| {
            let aid = toview_aids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).copied());
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            let Some(aid) = aid else {
                return;
            };
            let weak = weak.clone();
            let session_slot = session_slot.clone();
            let toview_aids = toview_aids.clone();
            let toview_bvids = toview_bvids.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                match runtime.block_on(session.toview_del(aid)) {
                    Ok(()) => load_toview(weak, session_slot, toview_aids, toview_bvids),
                    Err(err) => {
                        crate::debug_log(&format!("toview_del err={err:#}"));
                        let message = format!("移除失败：{err}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_toview_status(message.into());
                            }
                        });
                    }
                }
            });
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let toview_aids = toview_aids.clone();
        let toview_bvids = toview_bvids.clone();
        ui.on_toview_clear(move || {
            let aids = toview_aids
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            let weak = weak.clone();
            let session_slot = session_slot.clone();
            let toview_aids = toview_aids.clone();
            let toview_bvids = toview_bvids.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                runtime.block_on(async move {
                    let mut failed = None;
                    for aid in aids {
                        if let Err(err) = session.toview_del(aid).await {
                            failed = Some(format!("{err}"));
                            break;
                        }
                    }
                    match failed {
                        None => load_toview(weak, session_slot, toview_aids, toview_bvids),
                        Some(err) => {
                            crate::debug_log(&format!("toview_clear err={err}"));
                            let message = format!("清空失败：{err}");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.set_toview_status(message.into());
                                }
                            });
                        }
                    }
                });
            });
        });
    }

    // 换一换: fetch a new recommendation batch past the pages already shown.
    {
        let weak = weak.clone();
        let generation = generation.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let feed_ring = feed_ring.clone();
        let shuffle_seq = shuffle_seq.clone();
        ui.on_feed_shuffle(move || {
            let start = shuffle_seq.fetch_add(bilibili::feed_batch(), Ordering::SeqCst);
            // The network batch now owns this cursor: anything queued for it
            // (or older) would be the same content twice.
            if let Ok(mut guard) = feed_ring.lock() {
                guard.rebase_past(start);
            }
            spawn_feed_load(
                weak.clone(),
                generation.clone(),
                session_slot.clone(),
                feed_store.clone(),
                start,
                0,
            );
        });
    }

    // Floating page bar: one dispatcher routes every bar button to the active
    // section's loader. `update_page_bar` (called by each loader and by the
    // navigation handlers) mirrors the section's state back into the pill.
    let dispatch_page: Rc<dyn Fn(PageAction)> = {
        let weak = weak.clone();
        let generation = generation.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let feed_ring = feed_ring.clone();
        let shuffle_seq = shuffle_seq.clone();
        let search_state = search_state.clone();
        let search_seq = search_seq.clone();
        let search_bvids = search_bvids.clone();
        let folder_state = folder_state.clone();
        let folder_bvids = folder_bvids.clone();
        let history_page = history_page.clone();
        let history_kids = history_kids.clone();
        let history_bvids = history_bvids.clone();
        let toview_aids = toview_aids.clone();
        let toview_bvids = toview_bvids.clone();
        let region_state = region_state.clone();
        let browse_bvids = browse_bvids.clone();
        let popular_state = popular_state.clone();
        let rank_rid = rank_rid.clone();
        let dynamic_ids = dynamic_ids.clone();
        let dynamic_offset = dynamic_offset.clone();
        let dynamic_has_more = dynamic_has_more.clone();
        let session_meta = session_meta.clone();
        Rc::new(move |action: PageAction| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let view = ui.get_view_mode();
            let size = list_page_size(&ui);
            match view {
                // 首页推荐: first/prev/next walk the client pages of the loaded
                // batch; next past its last page loads the next batch (a data
                // page turn, so the client page restarts at 0). Refresh reloads
                // the batch it is on and keeps the client page.
                0 => {
                    let len = ui.get_feed_items().row_count();
                    let page = client_page(0);
                    let batch = bilibili::feed_batch().max(1);
                    let seq = shuffle_seq.load(Ordering::SeqCst);
                    match action {
                        PageAction::First => {
                            set_client_page(0, 0);
                            publish_list_offset(&ui);
                        }
                        PageAction::Prev => {
                            set_client_page(0, page - 1);
                            publish_list_offset(&ui);
                        }
                        PageAction::Next => {
                            if (page + 1).saturating_mul(size) < len as i64 {
                                set_client_page(0, page + 1);
                                publish_list_offset(&ui);
                            } else {
                                let start = seq.max(1 + batch);
                                shuffle_seq.store(start + batch, Ordering::SeqCst);
                                // A queued copy of this cursor (or an older
                                // one) is now the batch on screen: rebase.
                                if let Ok(mut guard) = feed_ring.lock() {
                                    guard.rebase_past(start);
                                }
                                spawn_feed_load(
                                    weak.clone(),
                                    generation.clone(),
                                    session_slot.clone(),
                                    feed_store.clone(),
                                    start,
                                    0,
                                );
                            }
                        }
                        PageAction::Refresh => {
                            let current = generation.load(Ordering::SeqCst);
                            let session = active_session(&session_slot);
                            crate::debug_log(&format!(
                                "feed refresh dispatch gen={current} page={page}"
                            ));
                            if let Some(promoted) =
                                take_prefetched_batch(&feed_ring, FEED_SOURCE_RECOMMEND)
                            {
                                // Instant path: no feed fetch on this event.
                                let promoted_start = promoted.start;
                                shuffle_seq.store(
                                    promoted_start.saturating_add(batch),
                                    Ordering::SeqCst,
                                );
                                let published = show_prefetched_batch(
                                    &weak,
                                    &generation,
                                    current,
                                    session,
                                    &feed_store,
                                    promoted,
                                    page,
                                );
                                if !published {
                                    crate::debug_log("feed prefetch promote dropped=superseded");
                                }
                            } else {
                                let start = seq.saturating_sub(batch).max(1);
                                crate::debug_log(&format!(
                                    "feed prefetch promote none seq={seq} start={start} path=network"
                                ));
                                shuffle_seq.store(seq.max(start + batch), Ordering::SeqCst);
                                if let Ok(mut guard) = feed_ring.lock() {
                                    guard.rebase_past(start);
                                }
                                spawn_feed_load(
                                    weak.clone(),
                                    generation.clone(),
                                    session_slot.clone(),
                                    feed_store.clone(),
                                    start,
                                    page,
                                );
                            }
                        }
                    }
                }
                // 搜索: one API page per screen (`ps` = the page size), so the
                // client page is the API page; every action re-fetches it and
                // the offset restarts at 0. Clamped at the API's page count.
                1 => {
                    let (page, num_pages) = search_state
                        .lock()
                        .map(|state| (state.page.max(1), state.num_pages.max(1)))
                        .unwrap_or((1, 1));
                    let target = match action {
                        PageAction::First => 1,
                        PageAction::Prev => (page - 1).max(1),
                        PageAction::Next if page < num_pages => page + 1,
                        PageAction::Refresh => page,
                        PageAction::Next => return,
                    };
                    if let Ok(mut state) = search_state.lock() {
                        state.page = target.max(1);
                    }
                    run_search(
                        weak.clone(),
                        session_slot.clone(),
                        search_state.clone(),
                        search_seq.clone(),
                        search_bvids.clone(),
                    );
                }
                // 收藏夹内容 / 分区 / 热门: one API page per screen, so the
                // client page is the API page; next is gated by has_more.
                4 => {
                    let (page, has_prev, has_next) = match folder_state.lock() {
                        Ok(state) => (state.page.max(1), state.page > 1, state.has_more),
                        Err(_) => return,
                    };
                    let target = match action {
                        PageAction::First => 1,
                        PageAction::Prev if has_prev => page - 1,
                        PageAction::Next if has_next => page + 1,
                        PageAction::Refresh => page,
                        _ => return,
                    };
                    load_folder_page(
                        weak.clone(),
                        session_slot.clone(),
                        folder_state.clone(),
                        folder_bvids.clone(),
                        target,
                    );
                }
                8 => {
                    let (page, has_prev, has_next) = match region_state.lock() {
                        Ok(state) => (state.page.max(1), state.page > 1, state.has_more),
                        Err(_) => return,
                    };
                    let target = match action {
                        PageAction::First => 1,
                        PageAction::Prev if has_prev => page - 1,
                        PageAction::Next if has_next => page + 1,
                        PageAction::Refresh => page,
                        _ => return,
                    };
                    load_region(
                        weak.clone(),
                        session_slot.clone(),
                        region_state.clone(),
                        browse_bvids.clone(),
                        target,
                    );
                }
                10 => {
                    let (page, has_prev, has_next) = match popular_state.lock() {
                        Ok(state) => (state.page.max(1), state.page > 1, state.has_more),
                        Err(_) => return,
                    };
                    let target = match action {
                        PageAction::First => 1,
                        PageAction::Prev if has_prev => page - 1,
                        PageAction::Next if has_next => page + 1,
                        PageAction::Refresh => page,
                        _ => return,
                    };
                    load_popular(
                        weak.clone(),
                        session_slot.clone(),
                        browse_bvids.clone(),
                        popular_state.clone(),
                        target,
                    );
                }
                // 历史: the cursor only moves forward. first/prev/next walk the
                // client pages of the loaded rows; next past the last loaded
                // page appends the next cursor page and lands on it. Refresh
                // reloads from the newest page (the data no longer covers the
                // old page, so the client page restarts at 0).
                5 => {
                    let len = ui.get_history_items().row_count();
                    let more = ui.get_history_has_more();
                    let page = client_page(5);
                    match action {
                        PageAction::First => {
                            set_client_page(5, 0);
                            publish_list_offset(&ui);
                        }
                        PageAction::Prev => {
                            set_client_page(5, page - 1);
                            publish_list_offset(&ui);
                        }
                        PageAction::Next => {
                            if (page + 1).saturating_mul(size) < len as i64 {
                                set_client_page(5, page + 1);
                                publish_list_offset(&ui);
                            } else if more {
                                load_history(
                                    weak.clone(),
                                    session_slot.clone(),
                                    history_page.clone(),
                                    history_kids.clone(),
                                    history_bvids.clone(),
                                    true,
                                    Some(page + 1),
                                );
                            }
                        }
                        PageAction::Refresh => {
                            if let Ok(mut state) = history_page.lock() {
                                *state = HistoryPageState::default();
                            }
                            load_history(
                                weak.clone(),
                                session_slot.clone(),
                                history_page.clone(),
                                history_kids.clone(),
                                history_bvids.clone(),
                                false,
                                None,
                            );
                        }
                    }
                }
                // 动态: same shape as 历史 (offset cursor, append-only).
                13 => {
                    let len = ui.get_dynamic_items().row_count();
                    let more = ui.get_dynamic_has_more();
                    let page = client_page(13);
                    match action {
                        PageAction::First => {
                            set_client_page(13, 0);
                            publish_list_offset(&ui);
                        }
                        PageAction::Prev => {
                            set_client_page(13, page - 1);
                            publish_list_offset(&ui);
                        }
                        PageAction::Next => {
                            if (page + 1).saturating_mul(size) < len as i64 {
                                set_client_page(13, page + 1);
                                publish_list_offset(&ui);
                            } else if more {
                                load_dynamic(
                                    weak.clone(),
                                    session_slot.clone(),
                                    dynamic_ids.clone(),
                                    dynamic_offset.clone(),
                                    dynamic_has_more.clone(),
                                    true,
                                    Some(page + 1),
                                );
                            }
                        }
                        PageAction::Refresh => {
                            if let Ok(mut offset) = dynamic_offset.lock() {
                                offset.clear();
                            }
                            if let Ok(mut more) = dynamic_has_more.lock() {
                                *more = false;
                            }
                            if let Ok(mut ids) = dynamic_ids.lock() {
                                ids.clear();
                            }
                            ui.set_dynamic_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<DynamicRow>::new(),
                            ))));
                            ui.set_dynamic_status(String::new().into());
                            ui.set_dynamic_has_more(false);
                            load_dynamic(
                                weak.clone(),
                                session_slot.clone(),
                                dynamic_ids.clone(),
                                dynamic_offset.clone(),
                                dynamic_has_more.clone(),
                                false,
                                None,
                            );
                        }
                    }
                }
                // 稍后再看 / 排行榜 / 入站必刷: single-shot lists, client-paged
                // over the loaded rows; refresh reloads and keeps the page.
                6 => match action {
                    PageAction::First => {
                        set_client_page(6, 0);
                        publish_list_offset(&ui);
                    }
                    PageAction::Prev => {
                        set_client_page(6, client_page(6) - 1);
                        publish_list_offset(&ui);
                    }
                    PageAction::Next => {
                        let len = ui.get_toview_items().row_count();
                        if has_client_next(client_page(6), size, len, false) {
                            set_client_page(6, client_page(6) + 1);
                            publish_list_offset(&ui);
                        }
                    }
                    PageAction::Refresh => load_toview(
                        weak.clone(),
                        session_slot.clone(),
                        toview_aids.clone(),
                        toview_bvids.clone(),
                    ),
                },
                9 => match action {
                    PageAction::First => {
                        set_client_page(9, 0);
                        publish_list_offset(&ui);
                    }
                    PageAction::Prev => {
                        set_client_page(9, client_page(9) - 1);
                        publish_list_offset(&ui);
                    }
                    PageAction::Next => {
                        let len = ui.get_rank_items().row_count();
                        if has_client_next(client_page(9), size, len, false) {
                            set_client_page(9, client_page(9) + 1);
                            publish_list_offset(&ui);
                        }
                    }
                    PageAction::Refresh => {
                        let rid = rank_rid.lock().map(|guard| *guard).unwrap_or(0);
                        load_ranking(
                            weak.clone(),
                            session_slot.clone(),
                            browse_bvids.clone(),
                            rank_rid.clone(),
                            rid,
                        );
                    }
                },
                // 消息: its grids have no `page-offset`, so only refresh is live.
                14 => {
                    if matches!(action, PageAction::Refresh) {
                        load_unread(weak.clone(), session_slot.clone());
                        let tab = ui.get_msg_tab();
                        if tab >= 3 {
                            load_sessions(weak.clone(), session_slot.clone(), session_meta.clone());
                        } else {
                            load_msg_list(weak.clone(), session_slot.clone(), tab);
                        }
                    }
                }
                18 => match action {
                    PageAction::First => {
                        set_client_page(18, 0);
                        publish_list_offset(&ui);
                    }
                    PageAction::Prev => {
                        set_client_page(18, client_page(18) - 1);
                        publish_list_offset(&ui);
                    }
                    PageAction::Next => {
                        let len = ui.get_precious_items().row_count();
                        if has_client_next(client_page(18), size, len, false) {
                            set_client_page(18, client_page(18) + 1);
                            publish_list_offset(&ui);
                        }
                    }
                    PageAction::Refresh => {
                        load_precious(weak.clone(), session_slot.clone(), browse_bvids.clone())
                    }
                },
                _ => {}
            }
        })
    };
    {
        let dispatch = dispatch_page.clone();
        ui.on_page_first(move || dispatch(PageAction::First));
    }
    {
        let dispatch = dispatch_page.clone();
        ui.on_page_prev(move || dispatch(PageAction::Prev));
    }
    {
        let dispatch = dispatch_page.clone();
        ui.on_page_next(move || dispatch(PageAction::Next));
    }
    {
        let dispatch = dispatch_page.clone();
        ui.on_page_refresh(move || dispatch(PageAction::Refresh));
    }

    // 分区浏览: the region list is static; picking one loads its latest videos.
    {
        let weak = weak.clone();
        ui.on_nav_regions(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(7);
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let region_state = region_state.clone();
        let browse_bvids = browse_bvids.clone();
        ui.on_open_region(move |index| {
            let Some((name, rid)) = REGIONS.get(index.max(0) as usize).copied() else {
                return;
            };
            if let Ok(mut state) = region_state.lock() {
                state.rid = rid;
                state.name = name.to_string();
                state.page = 1;
                state.has_more = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(8);
                enter_paged_view(&ui);
            }
            load_region(
                weak.clone(),
                session_slot.clone(),
                region_state.clone(),
                browse_bvids.clone(),
                1,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let region_state = region_state.clone();
        let browse_bvids = browse_bvids.clone();
        ui.on_region_page(move |delta| {
            let (target, has_prev, has_next) = match region_state.lock() {
                Ok(state) => (
                    (state.page + i64::from(delta)).max(1),
                    state.page > 1,
                    state.has_more,
                ),
                Err(_) => return,
            };
            if (delta < 0 && !has_prev) || (delta > 0 && !has_next) {
                return;
            }
            load_region(
                weak.clone(),
                session_slot.clone(),
                region_state.clone(),
                browse_bvids.clone(),
                target,
            );
        });
    }

    // 排行榜: a region selector plus the ranking list for the active region.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let browse_bvids = browse_bvids.clone();
        let rank_rid = rank_rid.clone();
        ui.on_nav_ranking(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(9);
                enter_paged_view(&ui);
            }
            let rid = rank_rid.lock().map(|guard| *guard).unwrap_or(0);
            load_ranking(
                weak.clone(),
                session_slot.clone(),
                browse_bvids.clone(),
                rank_rid.clone(),
                rid,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let browse_bvids = browse_bvids.clone();
        let rank_rid = rank_rid.clone();
        ui.on_rank_region(move |index| {
            let ix = index.max(0) as usize;
            let rid = if ix == 0 {
                0
            } else {
                REGIONS.get(ix - 1).map(|(_, rid)| *rid).unwrap_or(0)
            };
            // A different ranking is a new data set: back to its first page.
            set_client_page(9, 0);
            load_ranking(
                weak.clone(),
                session_slot.clone(),
                browse_bvids.clone(),
                rank_rid.clone(),
                rid,
            );
        });
    }

    // 热门: the popular list, paged.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let browse_bvids = browse_bvids.clone();
        let popular_state = popular_state.clone();
        ui.on_nav_popular(move || {
            if let Ok(mut state) = popular_state.lock() {
                state.page = 1;
                state.has_more = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(10);
                enter_paged_view(&ui);
            }
            load_popular(
                weak.clone(),
                session_slot.clone(),
                browse_bvids.clone(),
                popular_state.clone(),
                1,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let browse_bvids = browse_bvids.clone();
        let popular_state = popular_state.clone();
        ui.on_popular_page(move |delta| {
            let (target, has_prev, has_next) = match popular_state.lock() {
                Ok(state) => (
                    (state.page + i64::from(delta)).max(1),
                    state.page > 1,
                    state.has_more,
                ),
                Err(_) => return,
            };
            if (delta < 0 && !has_prev) || (delta > 0 && !has_next) {
                return;
            }
            load_popular(
                weak.clone(),
                session_slot.clone(),
                browse_bvids.clone(),
                popular_state.clone(),
                target,
            );
        });
    }

    // 入站必刷: the fixed "must-watch" list.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let browse_bvids = browse_bvids.clone();
        ui.on_nav_precious(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(18);
                enter_paged_view(&ui);
            }
            load_precious(weak.clone(), session_slot.clone(), browse_bvids.clone());
        });
    }

    // A 分区/排行/热门 card was clicked: open the detail view for it.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let browse_bvids = browse_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_open_card(move |index| {
            let bvid = browse_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(bvid) = bvid {
                open_detail(
                    weak.clone(),
                    session_slot.clone(),
                    detail_state.clone(),
                    detail_seq.clone(),
                    related_bvids.clone(),
                    bvid,
                );
            }
        });
    }

    // Search box typing: debounced suggestion lookup.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let suggest_seq = suggest_seq.clone();
        ui.on_suggest_query(move |text| {
            load_suggestions(
                weak.clone(),
                session_slot.clone(),
                suggest_seq.clone(),
                text.to_string(),
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let search_state = search_state.clone();
        let search_seq = search_seq.clone();
        let search_bvids = search_bvids.clone();
        let hot_keywords = hot_keywords.clone();
        let suggest_seq = suggest_seq.clone();
        ui.on_suggest_selected(move |text| {
            suggest_seq.fetch_add(1, Ordering::SeqCst);
            begin_search(
                weak.clone(),
                session_slot.clone(),
                search_state.clone(),
                search_seq.clone(),
                search_bvids.clone(),
                hot_keywords.clone(),
                text.to_string(),
            );
        });
    }
    {
        let weak = weak.clone();
        let suggest_seq = suggest_seq.clone();
        ui.on_suggest_dismiss(move || {
            suggest_seq.fetch_add(1, Ordering::SeqCst);
            if let Some(ui) = weak.upgrade() {
                ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<SharedString>::new(),
                ))));
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let search_state = search_state.clone();
        let search_seq = search_seq.clone();
        let search_bvids = search_bvids.clone();
        let hot_keywords = hot_keywords.clone();
        ui.on_hot_selected(move |index| {
            let keyword = hot_keywords
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(keyword) = keyword {
                begin_search(
                    weak.clone(),
                    session_slot.clone(),
                    search_state.clone(),
                    search_seq.clone(),
                    search_bvids.clone(),
                    hot_keywords.clone(),
                    keyword,
                );
            }
        });
    }

    // UP主空间: open from a feed row's author or the detail page's UP name.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let space_state = space_state.clone();
        let space_prev_mode = space_prev_mode.clone();
        let space_bvids = space_bvids.clone();
        ui.on_open_space(move |mid| {
            let Ok(mid) = mid.parse::<i64>() else {
                return;
            };
            open_space(
                weak.clone(),
                session_slot.clone(),
                space_state.clone(),
                space_prev_mode.clone(),
                space_bvids.clone(),
                mid,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let space_state = space_state.clone();
        let space_bvids = space_bvids.clone();
        ui.on_space_page(move |delta| {
            // The 投稿 list is fetched with `ps = list_page_size`, so one API
            // page is exactly one screen and the API page is the client page:
            // prev/next walk it, and each step re-publishes `list_offset` (0)
            // once the new page is in the model.
            let (target, has_prev, has_next) = match space_state.lock() {
                Ok(state) => (
                    (state.page + i64::from(delta)).max(1),
                    state.page > 1,
                    state.has_more,
                ),
                Err(_) => return,
            };
            if (delta < 0 && !has_prev) || (delta > 0 && !has_next) {
                return;
            }
            load_space_videos(
                weak.clone(),
                session_slot.clone(),
                space_state.clone(),
                space_bvids.clone(),
                target,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let space_bvids = space_bvids.clone();
        let detail_state = detail_state.clone();
        let detail_seq = detail_seq.clone();
        let related_bvids = related_bvids.clone();
        ui.on_space_open_item(move |index| {
            let bvid = space_bvids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index.max(0) as usize).cloned());
            if let Some(bvid) = bvid {
                open_detail(
                    weak.clone(),
                    session_slot.clone(),
                    detail_state.clone(),
                    detail_seq.clone(),
                    related_bvids.clone(),
                    bvid,
                );
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let space_state = space_state.clone();
        ui.on_toggle_follow(move || {
            toggle_space_follow(weak.clone(), session_slot.clone(), space_state.clone());
        });
    }
    {
        let weak = weak.clone();
        let space_prev_mode = space_prev_mode.clone();
        ui.on_space_back(move || {
            space_back(weak.clone(), space_prev_mode.clone());
        });
    }

    // 动态: open the dynamic feed and load its first page.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let dynamic_ids = dynamic_ids.clone();
        let dynamic_offset = dynamic_offset.clone();
        let dynamic_has_more = dynamic_has_more.clone();
        ui.on_nav_dynamic(move || {
            if let Ok(mut offset) = dynamic_offset.lock() {
                offset.clear();
            }
            if let Ok(mut more) = dynamic_has_more.lock() {
                *more = false;
            }
            if let Ok(mut ids) = dynamic_ids.lock() {
                ids.clear();
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(13);
                enter_paged_view(&ui);
                ui.set_dynamic_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<DynamicRow>::new(),
                ))));
                ui.set_dynamic_status(String::new().into());
                ui.set_dynamic_has_more(false);
            }
            load_dynamic(
                weak.clone(),
                session_slot.clone(),
                dynamic_ids.clone(),
                dynamic_offset.clone(),
                dynamic_has_more.clone(),
                false,
                None,
            );
        });
    }

    // 动态: append the next page.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let dynamic_ids = dynamic_ids.clone();
        let dynamic_offset = dynamic_offset.clone();
        let dynamic_has_more = dynamic_has_more.clone();
        ui.on_dynamic_load_more(move || {
            load_dynamic(
                weak.clone(),
                session_slot.clone(),
                dynamic_ids.clone(),
                dynamic_offset.clone(),
                dynamic_has_more.clone(),
                true,
                None,
            );
        });
    }

    // 动态: like / un-like one card (write; code surfaced verbatim).
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let dynamic_ids = dynamic_ids.clone();
        ui.on_dynamic_thumb(move |index| {
            let index = index.max(0) as usize;
            let id = dynamic_ids
                .lock()
                .ok()
                .and_then(|ids| ids.get(index).cloned());
            let Some(id) = id else {
                return;
            };
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(row) = ui.get_dynamic_items().row_data(index) else {
                return;
            };
            let target = !row.liked;
            ui.set_dynamic_status(
                format!("正在{}…", if target { "点赞" } else { "取消赞" }).into(),
            );
            let weak = weak.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.dynamic_thumb(&id, target));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        match result {
                            Ok(code) => {
                                if code == 0 {
                                    let model = ui.get_dynamic_items();
                                    let mut rows: Vec<DynamicRow> = (0..model.row_count())
                                        .filter_map(|position| model.row_data(position))
                                        .collect();
                                    if let Some(row) = rows.get_mut(index) {
                                        row.liked = target;
                                        row.like_text =
                                            if target { "已赞".into() } else { "点赞".into() };
                                    }
                                    ui.set_dynamic_items(ModelRc::from(std::rc::Rc::new(
                                        VecModel::from(rows),
                                    )));
                                }
                                ui.set_dynamic_status(format!("动态点赞返回 code={code}").into());
                            }
                            Err(err) => {
                                ui.set_dynamic_status(format!("点赞失败：{err}").into());
                            }
                        }
                    }
                });
            });
        });
    }

    // 消息: open the message center (unread + 回复我的).
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        ui.on_nav_messages(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(14);
                ui.set_msg_tab(0);
                ui.set_msg_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<MsgRow>::new(),
                ))));
                ui.set_session_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<SessionRow>::new(),
                ))));
                ui.set_msg_status(String::new().into());
            }
            load_unread(weak.clone(), session_slot.clone());
            load_msg_list(weak.clone(), session_slot.clone(), 0);
        });
    }

    // 消息: switch tab (回复我的 / @我的 / 收到的赞 / 私信).
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let session_meta = session_meta.clone();
        ui.on_msg_tab_changed(move |tab| {
            if let Some(ui) = weak.upgrade() {
                ui.set_msg_tab(tab);
                ui.set_msg_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<MsgRow>::new(),
                ))));
                ui.set_session_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<SessionRow>::new(),
                ))));
                ui.set_msg_status(String::new().into());
            }
            if tab >= 3 {
                load_sessions(weak.clone(), session_slot.clone(), session_meta.clone());
            } else {
                load_msg_list(weak.clone(), session_slot.clone(), tab);
            }
        });
    }

    // 消息: open one private-message session (read-only).
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let session_meta = session_meta.clone();
        ui.on_open_session(move |index| {
            let index = index.max(0) as usize;
            let meta = session_meta
                .lock()
                .ok()
                .and_then(|list| list.get(index).cloned());
            let Some((talker_id, name)) = meta else {
                return;
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(17);
                ui.set_dm_title(name.clone().into());
                ui.set_dm_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<DmRow>::new(),
                ))));
                ui.set_dm_status(String::new().into());
            }
            load_dm_messages(weak.clone(), session_slot.clone(), talker_id);
        });
    }

    // 私信: back to the message center's 私信 tab.
    {
        let weak = weak.clone();
        ui.on_dm_back(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(14);
                update_page_bar(&ui);
            }
        });
    }

    // 关注 / 粉丝: open the paged relation lists.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let relation_state = relation_state.clone();
        ui.on_open_followings(move || {
            if let Ok(mut state) = relation_state.lock() {
                state.kind = 0;
                state.page = 1;
                state.has_more = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(15);
                ui.set_relation_title("关注".into());
                ui.set_relation_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<RelationRow>::new(),
                ))));
                ui.set_relation_status(String::new().into());
            }
            load_relation(weak.clone(), session_slot.clone(), relation_state.clone(), 1);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let relation_state = relation_state.clone();
        ui.on_open_fans(move || {
            if let Ok(mut state) = relation_state.lock() {
                state.kind = 1;
                state.page = 1;
                state.has_more = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(15);
                ui.set_relation_title("粉丝".into());
                ui.set_relation_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                    Vec::<RelationRow>::new(),
                ))));
                ui.set_relation_status(String::new().into());
            }
            load_relation(weak.clone(), session_slot.clone(), relation_state.clone(), 1);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let relation_state = relation_state.clone();
        ui.on_relation_page(move |delta| {
            let (target, has_prev, has_next) = match relation_state.lock() {
                Ok(state) => (
                    (state.page + i64::from(delta)).max(1),
                    state.page > 1,
                    state.has_more,
                ),
                Err(_) => return,
            };
            if (delta < 0 && !has_prev) || (delta > 0 && !has_next) {
                return;
            }
            load_relation(weak.clone(), session_slot.clone(), relation_state.clone(), target);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let space_state = space_state.clone();
        let space_prev_mode = space_prev_mode.clone();
        let space_bvids = space_bvids.clone();
        ui.on_open_relation_user(move |mid| {
            let Ok(mid) = mid.parse::<i64>() else {
                return;
            };
            open_space(
                weak.clone(),
                session_slot.clone(),
                space_state.clone(),
                space_prev_mode.clone(),
                space_bvids.clone(),
                mid,
            );
        });
    }

    // User menu: 退出登录 the account (endpoint + local record removal).
    {
        let weak = weak.clone();
        let generation = generation.clone();
        let session_slot = session_slot.clone();
        ui.on_logout(move || {
            logout_account(
                weak.clone(),
                generation.clone(),
                session_slot.clone(),
            );
        });
    }

    // Settings panel: open (re-syncing from the stored preferences) and close.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_open_settings(move || {
            if let Some(ui) = weak.upgrade() {
                if let Ok(guard) = settings_store.lock() {
                    publish_settings(&ui, &guard);
                }
                ui.set_settings_status(String::new().into());
                ui.set_view_mode(12);
            }
        });
    }
    {
        let weak = weak.clone();
        ui.on_settings_back(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(0);
                enter_paged_view(&ui);
                update_page_bar(&ui);
            }
        });
    }
    // Where the VIP page returns to: 会员中心 now lives in the settings
    // panel, so `vip_back` restores the view that opened it (settings, 12)
    // instead of dropping the user on the feed.
    let vip_return_mode = Rc::new(Cell::new(0));
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let vip_return_mode = vip_return_mode.clone();
        ui.on_vip_center(move || {
            if let Some(ui) = weak.upgrade() {
                let from = ui.get_view_mode();
                if from != 19 {
                    vip_return_mode.set(from);
                }
                ui.set_vip_busy(false);
                ui.set_vip_status_text(String::new().into());
                ui.set_vip_privilege_text(String::new().into());
                ui.set_vip_action_status(String::new().into());
                ui.set_view_mode(19);
            }
            vip_refresh(weak.clone(), session_slot.clone());
        });
    }
    {
        let weak = weak.clone();
        let vip_return_mode = vip_return_mode.clone();
        ui.on_vip_back(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_view_mode(vip_return_mode.get());
                publish_list_offset(&ui);
                update_page_bar(&ui);
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        ui.on_vip_refresh(move || {
            vip_refresh(weak.clone(), session_slot.clone());
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        ui.on_vip_experience(move || {
            vip_action(weak.clone(), session_slot.clone(), VipAction::Experience);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        ui.on_vip_receive(move || {
            vip_action(weak.clone(), session_slot.clone(), VipAction::Receive);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        ui.on_vip_sign(move || {
            vip_action(weak.clone(), session_slot.clone(), VipAction::Sign);
        });
    }
    // Cards per page (设置): the menu reports the chosen step. Normalise it,
    // persist it, drive the Rust-owned `list_page_size` property, then re-apply
    // the client paging so the offset is `page × size` again — the page index
    // is kept and only pulled back onto the last page when the loaded rows no
    // longer cover it — before the page bar is refreshed from the new size.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_page_size_changed(move |size| {
            let size = settings::normalise_page_size(i64::from(size));
            let snapshot = update_settings(&settings_store, |prefs| prefs.page_size = size);
            if let Some(ui) = weak.upgrade() {
                ui.set_list_page_size(snapshot.page_size as i32);
                publish_list_offset(&ui);
            }
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_quality(move |qn| {
            let snapshot =
                update_settings(&settings_store, |prefs| prefs.player_quality = i64::from(qn));
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_speed(move |milli| {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.player_speed = (milli.max(0) as u32).clamp(500, 2000);
            });
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_hwaccel(move |on| {
            let snapshot = update_settings(&settings_store, |prefs| prefs.hwaccel = on);
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_codec(move |codec| {
            let value = match codec {
                7 | 12 | 13 => codec as u32,
                _ => 0,
            };
            let snapshot = update_settings(&settings_store, |prefs| prefs.codec_pref = value);
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_dm_opacity(move |pct| {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.danmaku_opacity = (pct.max(0) as u32).min(100);
            });
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_dm_font(move |font| {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.danmaku_font = (font.max(0) as u32).min(2);
            });
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_dm_speed(move |percent| {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.danmaku_speed = (percent.max(0) as u32).clamp(50, 200);
            });
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_dm_area(move |area| {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.danmaku_area = settings::clamp_area(area.max(0) as u32);
            });
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_dm_type(move |scroll, top, bottom| {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.block_scroll = scroll;
                prefs.block_top = top;
                prefs.block_bottom = bottom;
            });
            apply_settings_to_player(&state, &snapshot);
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_clear_senders(move || {
            let snapshot =
                update_settings(&settings_store, |prefs| prefs.blocked_senders.clear());
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("已清空屏蔽列表".into());
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_add_keyword(move |word| {
            let word = word.trim().to_string();
            if word.is_empty() {
                return;
            }
            let snapshot = update_settings(&settings_store, |prefs| {
                if !prefs.blocked_keywords.iter().any(|entry| entry == &word) {
                    prefs.blocked_keywords.push(word.clone());
                }
            });
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_set_keyword_input(String::new().into());
                ui.set_settings_status(format!("已添加关键词 {word}").into());
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_remove_keyword(move |index| {
            let index = index.max(0) as usize;
            let snapshot = update_settings(&settings_store, |prefs| {
                if index < prefs.blocked_keywords.len() {
                    prefs.blocked_keywords.remove(index);
                }
            });
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_clear_keywords(move || {
            let snapshot =
                update_settings(&settings_store, |prefs| prefs.blocked_keywords.clear());
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("已清空关键词".into());
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_add_regex(move |pattern| {
            let pattern = pattern.trim().to_string();
            if pattern.is_empty() {
                return;
            }
            let snapshot = update_settings(&settings_store, |prefs| {
                if !prefs.blocked_regex.iter().any(|entry| entry == &pattern) {
                    prefs.blocked_regex.push(pattern.clone());
                }
            });
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_set_regex_input(String::new().into());
                ui.set_settings_status(format!("已添加正则 {pattern}").into());
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_remove_regex(move |index| {
            let index = index.max(0) as usize;
            let snapshot = update_settings(&settings_store, |prefs| {
                if index < prefs.blocked_regex.len() {
                    prefs.blocked_regex.remove(index);
                }
            });
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_clear_regex(move || {
            let snapshot = update_settings(&settings_store, |prefs| prefs.blocked_regex.clear());
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("已清空正则".into());
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_ai(move |on, level| {
            let level = (level.max(0) as u32).min(10);
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.ai_on = on;
                prefs.ai_level = level;
            });
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("正在设置智能云屏蔽…".into());
            }
            let Some(session) = active_session(&session_slot) else {
                if let Some(ui) = weak.upgrade() {
                    ui.set_settings_status("未登录，无法设置云屏蔽".into());
                }
                return;
            };
            let weak = weak.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let message = match runtime.block_on(session.danmaku_ai_config(on, Some(level))) {
                    Ok(code) => format!("智能云屏蔽：服务端返回 code={code}"),
                    Err(err) => format!("智能云屏蔽请求失败：{err}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_settings_status(message.into());
                    }
                });
            });
        });
    }

    // 主题 setting: persist it, then flip every live window in one pass.
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_theme(move |mode| {
            let mode = (mode.max(0) as u32).min(2);
            let snapshot = update_settings(&settings_store, |prefs| prefs.theme = mode);
            let dark = theme::resolve(snapshot.theme);
            state.theme_dark.set(dark);
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppTheme>().set_dark(dark);
                win::set_immersive_dark(ui.window(), dark);
            }
            for session in player_sessions(&state) {
                session.window.global::<AppTheme>().set_dark(dark);
            }
            if let Some(mini) = state.mini.borrow().as_ref() {
                mini.global::<AppTheme>().set_dark(dark);
            }
        });
    }

    // 外观: the two frosted-glass sliders. They write the theme global live
    // while dragging (the 33ms loop follows), so this only persists the settled
    // values. No debounce needed - the danmaku sliders already save per change.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_glass_changed(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let theme = ui.global::<AppTheme>();
            let transparency = settings::normalise_glass(
                theme.get_glass_transparency(),
                settings::default_glass_transparency(),
            );
            let blur = settings::normalise_glass(
                theme.get_glass_blur(),
                settings::default_glass_blur(),
            );
            update_settings(&settings_store, |prefs| {
                prefs.glass_transparency = transparency;
                prefs.glass_blur = blur;
            });
        });
    }

    // 快捷键自定义: point one player action at a candidate key.
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_remap_key(move |action, key| {
            let action = action.clamp(0, (settings::PLAYER_KEY_ACTIONS as i32) - 1) as usize;
            let key = key.clamp(0, (settings::PLAYER_KEY_CANDIDATES as i32) - 1) as u32;
            let snapshot = update_settings(&settings_store, |prefs| {
                if action < prefs.player_keys.len() {
                    prefs.player_keys[action] = key;
                }
            });
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("已更新快捷键".into());
            }
        });
    }
    {
        let weak = weak.clone();
        let state = player_state.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_reset_keys(move || {
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.player_keys = settings::default_player_keys();
            });
            apply_settings_to_player(&state, &snapshot);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("已恢复默认快捷键".into());
            }
        });
    }

    // 开机自启: persist the choice and add/remove the HKCU Run value.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_autostart(move |on| {
            let snapshot = update_settings(&settings_store, |prefs| prefs.autostart = on);
            let applied = autostart::set_enabled(on);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                let message = if !applied {
                    "开机自启设置失败（注册表不可写）".to_string()
                } else if on {
                    "已开启开机自启".to_string()
                } else {
                    "已关闭开机自启".to_string()
                };
                ui.set_settings_status(message.into());
            }
        });
    }

    // 关闭主窗口的行为: persist the choice. Every close route reads it back
    // through `apply_close_action`, so no window needs to hold a copy.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_close_action(move |action| {
            let action = settings::normalise_close_action(action.max(0) as u32);
            let snapshot = update_settings(&settings_store, |prefs| prefs.close_action = action);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status("已更新关闭主窗口的行为".into());
            }
            crate::debug_log(&format!("close action={action} selected"));
        });
    }

    // Custom title bar: minimize / maximize-restore / close-to-tray.
    {
        let weak = weak.clone();
        ui.on_minimize_window(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }
    {
        let weak = weak.clone();
        ui.on_maximize_window(move || {
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                let next = !window.is_maximized();
                window.set_maximized(next);
            }
        });
    }
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_close_window(move || {
            if let Some(ui) = weak.upgrade() {
                apply_close_action(&ui, &settings_store);
            }
        });
    }

    // Self-driven window drag: the OS modal move loop (and its frame
    // re-evaluation) is never entered.
    //
    // The frosted-glass state is created here, before the drag wiring, because
    // the drag callback below needs it: it re-places the backdrop in the same
    // pointer event that moves the window (the loop itself is started further
    // down, once the window chrome is queued).
    let glass_loop = Rc::new(GlassLoop::new());
    let main_drag = Rc::new(win::WindowDrag::new());
    // Frame-locked move: the pointer events below only RECORD a pending
    // placement; this `DRAG_FRAME` timer (started on drag_start, stopped on
    // drag_end) applies at most one `SetWindowPos` per display frame. It is the
    // only thing on the move path that touches the window, so 125-1000 Hz
    // pointer sampling no longer means 125-1000 window invalidations per second.
    let main_drag_stats = Rc::new(DragFlushStats::new());
    let main_drag_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let drag = main_drag.clone();
        let glass_loop = glass_loop.clone();
        let timer = main_drag_timer.clone();
        let stats = main_drag_stats.clone();
        ui.on_window_drag_start(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if let Err(reason) = drag.start(&ui.window()) {
                crate::debug_log(&format!("drag denied reason={reason} source=main"));
                return;
            }
            stats.begin();
            crate::debug_log(&format!(
                "drag start ok=1 source={} pos={}",
                drag_source(&ui.window(), "main"),
                drag_pos(&ui.window()),
            ));
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_drag = drag.clone();
            let tick_glass = glass_loop.clone();
            let tick_timer = Rc::downgrade(&timer);
            let tick_stats = stats.clone();
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(ui) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    // The drag's lifetime is owned HERE, not by Slint's
                    // pointer stream: a grab lost to the window moving out
                    // from under the cursor never reaches the end callback,
                    // so the real release (VK_LBUTTON, a read) closes it.
                    if !tick_drag.is_dragging() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&ui.window()) == Some(false) {
                        crate::debug_log("drag end reason=window-left");
                        tick_stats.finish("main");
                        tick_drag.end();
                        timer.stop();
                        return;
                    }
                    let moved = tick_drag.flush(&ui.window());
                    if moved {
                        tick_stats.placement();
                        // Same tick as the placement, never per pointer event:
                        // one `GetWindowRect` + four property writes, so the
                        // frosted backdrop follows the window without a second
                        // full-window repaint.
                        glass_place(&ui, &tick_glass);
                    }
                    if !win::left_button_down() {
                        crate::debug_log(&format!(
                            "drag end reason=pointer-up deferred=0 moved={moved}"
                        ));
                        tick_stats.finish("main");
                        tick_drag.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let drag = main_drag.clone();
        let stats = main_drag_stats.clone();
        ui.on_window_drag_move(move || {
            stats.event();
            drag.move_to();
        });
    }
    {
        let weak = weak.clone();
        let drag = main_drag.clone();
        let glass_loop = glass_loop.clone();
        let timer = main_drag_timer.clone();
        let stats = main_drag_stats.clone();
        ui.on_window_drag_end(move || {
            if let Some(ui) = weak.upgrade() {
                if !drag.end_on_signal(win::left_button_down()) {
                    // Slint cancelled the grab while the button is still held
                    // (the window it just moved jumped away from the cursor).
                    // Not a release: keep the drag and let the tick finish it.
                    crate::debug_log(&format!(
                        "drag end reason=lost-grab deferred=1 {}",
                        drag_state(&ui.window())
                    ));
                    return;
                }
                timer.stop();
                // Apply the last pending position exactly, then close the
                // profile.
                if drag.flush(&ui.window()) {
                    stats.placement();
                    glass_place(&ui, &glass_loop);
                }
            }
            timer.stop();
            stats.finish("main");
            drag.end();
            crate::debug_log("drag end reason=pointer-up deferred=0");
        });
    }

    // ---- self-driven window resize ---------------------------------------
    // The 8px rim answers `HTCLIENT` from `WM_NCHITTEST`
    // (`win::border_hit_test`), so the OS never enters its modal resize loop.
    // `ResizeFrame` in app.slint calls `resize_start(edge)` on a rim press, then
    // `resize_move()` per pointer event; this window's 8ms timer does the only
    // placement, one absolute `SetWindowPos` per display frame, so the video
    // keeps playing (and the event loop keeps ticking) for the whole gesture.
    let main_resize = Rc::new(win::WindowResize::new());
    main_resize.set_min(480, 360);
    let main_resize_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let resize = main_resize.clone();
        let timer = main_resize_timer.clone();
        ui.on_resize_start(move |edge: i32| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // A maximised window has no meaningful anchored edge: the restored
            // rect would be written while the window stays maximised.
            if ui.window().is_maximized() {
                return;
            }
            if let Err(reason) = resize.start(&ui.window(), edge) {
                crate::debug_log(&format!(
                    "resize denied reason={reason} edge={edge} source=main"
                ));
                return;
            }
            crate::debug_log(&format!(
                "resize start edge={edge} source=main rect={}",
                resize_rect_text(&ui.window())
            ));
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_resize = resize.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(ui) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    if !tick_resize.is_active() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&ui.window()) == Some(false) {
                        crate::debug_log("resize end reason=window-left source=main");
                        tick_resize.end();
                        timer.stop();
                        return;
                    }
                    tick_resize.update(&ui.window());
                    // Same release detection as the drag: the button is the
                    // authority, so a grab Slint dropped cannot strand the
                    // resize half-way.
                    if !win::left_button_down() {
                        crate::debug_log("resize end reason=pointer-up source=main");
                        tick_resize.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let resize = main_resize.clone();
        ui.on_resize_move(move || resize.move_to());
    }
    {
        let weak = weak.clone();
        let resize = main_resize.clone();
        let timer = main_resize_timer.clone();
        ui.on_resize_end(move || {
            // A cancel/leave that arrives while the button is still down is not
            // a release: the edge just moved out from under the cursor. Let the
            // frame tick finish the gesture on the real VK_LBUTTON up instead of
            // stranding it half-way (the same rule as the drag end).
            if resize.is_active() && win::left_button_down() {
                crate::debug_log("resize end reason=lost-grab deferred=1 source=main");
                return;
            }
            if let Some(ui) = weak.upgrade() {
                // Apply the last pending cursor position exactly, then close.
                resize.update(&ui.window());
            }
            timer.stop();
            resize.end();
        });
    }

    // Startup surface: when a saved record exists go straight to auto-login
    // (the 登录中… progress view). Only an empty record shows the login page,
    // which offers 扫码登录 and, when a record exists, 使用已保存账号登录.
    let saved = accounts::load();
    let saved_account_name = saved
        .as_ref()
        .map(|account| account.uname.clone())
        .unwrap_or_default();
    let has_saved_record = saved.is_some() || bilibili::token_path().exists();
    ui.set_saved_account_name(saved_account_name.clone().into());
    if has_saved_record {
        ui.set_login_state(1);
        ui.set_login_step("准备中…".into());
        ui.set_login_progress(0.0);
        crate::debug_log(&format!(
            "startup auto-login saved_account={saved_account_name:?} legacy_token={}",
            saved.is_none()
        ));
    } else {
        ui.set_login_state(0);
        ui.set_login_step("请选择登录方式".into());
        crate::debug_log("startup login page (no saved account)");
    }

    // 「扫码登录」 -> fresh QR login.
    {
        let weak = weak.clone();
        let generation = generation.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let feed_ring = feed_ring.clone();
        let settings_store = settings_store.clone();
        ui.on_login_qr(move || {
            start_login(
                weak.clone(),
                generation.clone(),
                session_slot.clone(),
                feed_store.clone(),
                feed_ring.clone(),
                settings_store.clone(),
                false,
            );
        });
    }
    // 「使用已保存账号登录（…）」 -> saved-account login.
    {
        let weak = weak.clone();
        let generation = generation.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        let feed_ring = feed_ring.clone();
        let settings_store = settings_store.clone();
        ui.on_login_saved(move || {
            start_login(
                weak.clone(),
                generation.clone(),
                session_slot.clone(),
                feed_store.clone(),
                feed_ring.clone(),
                settings_store.clone(),
                true,
            );
        });
    }
    // 「返回」 after a failed attempt: back to the choice view.
    {
        let weak = weak.clone();
        ui.on_login_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_login_state(0);
                ui.set_login_failed(false);
                ui.set_login_error("".into());
                ui.set_login_countdown("".into());
            }
        });
    }

    start_tray(&weak);
    crate::debug_log(&format!("autostart registry enabled={}", autostart::is_enabled()));

    // Keep feed batches preloaded from the moment a session exists: the filler
    // waits in the background during the login view, so the home page can open
    // with content already there and every refresh can promote instantly.
    spawn_feed_prefetch(
        session_slot.clone(),
        feed_ring.clone(),
        settings_store.clone(),
    );

    // Permanent QA hook (`BILI_FEED_PREFETCH_TEST=1`): ring state once a second
    // plus two simulated home refreshes 3 s apart, backend-only.
    if std::env::var("BILI_FEED_PREFETCH_TEST")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        run_feed_prefetch_test(weak.clone(), feed_ring.clone(), settings_store.clone());
    }

    // Diagnostic hook (mirrors BILI_AUTOPLAY / BILI_DANMAKU_SEED): start a login
    // immediately so the real network step sequence can be captured from
    // BILI_DEBUG_LOG without synthetic input. Values: "saved" or "qr"; with no
    // override a saved record still auto-logs in.
    match std::env::var("BILI_LOGIN_ON_START").as_deref() {
        Ok("saved") => ui.invoke_login_saved(),
        Ok("qr") => ui.invoke_login_qr(),
        _ if has_saved_record => ui.invoke_login_saved(),
        _ => {}
    }

    // QA hook: `BILI_CDN_TEST=<bvid>` classifies the primary URL and every
    // backup, probes the selection candidates and prints the table (through
    // BILI_DEBUG_LOG; the release binary has no console). Waits for the saved
    // account's session, which the normal auto-login creates.
    if let Some(bvid) = bilibili::cdn_qa_bvid() {
        let session_slot = session_slot.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if let Some(session) = active_session(&session_slot) {
                    let Some(runtime) = bilibili::shared_runtime() else {
                        return;
                    };
                    match runtime.block_on(session.cdn_selftest(&bvid)) {
                        Ok(_) => {}
                        Err(err) => {
                            crate::debug_log(&format!("cdn qa failed bvid={bvid} err={err}"))
                        }
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            crate::debug_log(&format!("cdn qa skipped bvid={bvid} reason=no-session"));
        });
    }

    // 收藏推荐: when the UI restored itself on that tab, fill it once a session
    // exists (the folder worker waits for one).
    if ui.get_home_tab() == 5 {
        crate::debug_log("fav view restore tab=5");
        fav_start(weak.clone(), session_slot.clone(), fav_view.clone());
    }

    // QA hook (`BILI_FAV_TEST=1`, mirrors BILI_CDN_TEST): prove the 收藏推荐
    // data layer (folders / items / related) from the app itself.
    if bilibili::fav_qa_enabled() {
        let session_slot = session_slot.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if let Some(session) = active_session(&session_slot) {
                    let Some(runtime) = bilibili::shared_runtime() else {
                        return;
                    };
                    match runtime.block_on(session.fav_selftest()) {
                        Ok(_) => {}
                        Err(err) => crate::debug_log(&format!("fav qa failed err={err:#}")),
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            crate::debug_log("fav qa skipped reason=no-session");
        });
    }

    // QA hook (`BILI_FAV_VIEW_TEST=<tab|all>`): open 收藏推荐 and walk its tabs
    // from the backend, so the folder count, per-tab card counts, real aspects,
    // the second visit's cache hit and `masonry_open` are all in the log.
    if let Ok(spec) = std::env::var("BILI_FAV_VIEW_TEST") {
        run_fav_view_test(weak.clone(), session_slot.clone(), fav_view.clone(), spec);
    }

    // Diagnostic hook (mirrors BILI_AUTOPLAY): start a real 离线下载 for the first
    // feed item once the feed has loaded, so the download child's process
    // lifetime can be exercised from the backend without synthetic input.
    if std::env::var("BILI_DOWNLOAD_ON_START")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let feed_store = feed_store.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                let video = feed_store
                    .lock()
                    .ok()
                    .and_then(|guard| guard.first().cloned());
                if let Some(video) = video {
                    if let Some(session) = active_session(&session_slot) {
                        crate::debug_log(&format!(
                            "download-test start bvid={} cid={}",
                            video.bvid, video.cid
                        ));
                        let control = download::DownloadControl::new();
                        download::spawn(
                            session,
                            video.bvid,
                            video.cid,
                            video.title,
                            weak,
                            control,
                        );
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    // Diagnostic hook (mirrors BILI_AUTOPLAY / BILI_DOWNLOAD_ON_START): fire the
    // 换一换 refresh path `count` times with a fixed gap once the first feed is
    // up, so a superseding refresh can be exercised from the backend without
    // synthetic input. Any non-empty value becomes `count`; the gap defaults to
    // 1200ms so the second refresh lands while the first is still streaming.
    if let Ok(value) = std::env::var("BILI_SHUFFLE_TEST") {
        let count = value.trim().parse::<u32>().unwrap_or(1);
        let gap = std::env::var("BILI_SHUFFLE_GAP_MS")
            .ok()
            .and_then(|ms| ms.trim().parse::<u64>().ok())
            .unwrap_or(1200);
        let weak = weak.clone();
        let feed_store = feed_store.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if feed_store
                    .lock()
                    .map(|guard| !guard.is_empty())
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            for index in 0..count {
                crate::debug_log(&format!("shuffle-test trigger n={}", index + 1));
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.invoke_feed_shuffle();
                    }
                });
                std::thread::sleep(Duration::from_millis(gap));
            }
        });
    }

    // Diagnostic hook (mirrors BILI_SHUFFLE_TEST / BILI_SEEK_TEST): page the
    // active section forward so the no-scroll client paging can be exercised
    // end to end from the backend without synthetic input. `<n>` pages the
    // feed; `<view>:<n>` first opens that section through its existing
    // navigation callback (1 搜索 / 5 历史 / 6 稍后再看 / 8 分区 / 9 排行榜 /
    // 10 热门 / 13 动态 / 18 入站必刷) and then pages it (see `run_page_test`).
    if let Ok(value) = std::env::var("BILI_PAGE_TEST") {
        let (view, count) = parse_page_test(&value);
        if count > 0 {
            let gap = std::env::var("BILI_PAGE_TEST_GAP_MS")
                .ok()
                .and_then(|ms| ms.trim().parse::<u64>().ok())
                .unwrap_or(600);
            let probe: Arc<Mutex<Vec<String>>> = match view {
                1 => search_bvids.clone(),
                5 => history_bvids.clone(),
                6 => toview_bvids.clone(),
                8 | 9 | 10 | 18 => browse_bvids.clone(),
                13 => dynamic_ids.clone(),
                _ => Arc::new(Mutex::new(Vec::new())),
            };
            run_page_test(
                weak.clone(),
                view,
                count,
                Duration::from_millis(gap),
                feed_store.clone(),
                probe,
            );
        }
    }

    // Diagnostic hook `BILI_SEARCH_TEST=<query>`: run a real search through the
    // search box's own callback, log the first three rendered rows (bvid + the
    // title the card shows), then click row 1 through the card callback and log
    // the bvid that was actually opened. The `search open` / `open_detail->play`
    // lines must carry that same bvid — the whole point of the hook.
    if let Ok(query) = std::env::var("BILI_SEARCH_TEST") {
        let query = query.trim().to_string();
        if !query.is_empty() {
            let weak = weak.clone();
            let session_slot = session_slot.clone();
            let search_bvids = search_bvids.clone();
            std::thread::spawn(move || {
                for _ in 0..600 {
                    if active_session(&session_slot).is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let do_weak = weak.clone();
                let keyword = query.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = do_weak.upgrade() {
                        crate::debug_log(&format!("search-test query={keyword:?}"));
                        ui.invoke_do_search(keyword.into());
                    }
                });
                // Wait for the page the card grid renders from. The rows are read
                // on the UI thread (Slint models are not thread safe) and sent
                // back through a channel; `search_bvids` alone is not enough to
                // prove the grid and the click agree.
                let (tx, rx) = std::sync::mpsc::channel::<Vec<(String, String)>>();
                let mut rows: Vec<(String, String)> = Vec::new();
                for _ in 0..300 {
                    std::thread::sleep(Duration::from_millis(100));
                    let tx_iter = tx.clone();
                    let poll_weak = weak.clone();
                    let bvids_slot = search_bvids.clone();
                    let queued = slint::invoke_from_event_loop(move || {
                        let bvids = bvids_slot
                            .lock()
                            .map(|guard| guard.clone())
                            .unwrap_or_default();
                        let mut out: Vec<(String, String)> = Vec::new();
                        if let Some(ui) = poll_weak.upgrade() {
                            let items = ui.get_search_items();
                            for (index, bvid) in bvids.iter().enumerate() {
                                let title = items
                                    .row_data(index)
                                    .map(|row| row.title.to_string())
                                    .unwrap_or_default();
                                out.push((bvid.clone(), title));
                            }
                        }
                        let _ = tx_iter.send(out);
                    });
                    if queued.is_err() {
                        break;
                    }
                    rows = rx
                        .recv_timeout(Duration::from_millis(500))
                        .unwrap_or_default();
                    if !rows.is_empty() {
                        break;
                    }
                }
                for (index, (bvid, title)) in rows.iter().take(3).enumerate() {
                    crate::debug_log(&format!(
                        "search-test result index={index} bvid={bvid} title={title:?}"
                    ));
                }
                if rows.is_empty() {
                    crate::debug_log("search-test no results");
                    return;
                }
                let click_weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = click_weak.upgrade() {
                        crate::debug_log(&format!(
                            "search-test click index=1 row_bvid={} view={}",
                            rows.get(1).map(|(bvid, _)| bvid.as_str()).unwrap_or("-"),
                            ui.get_view_mode(),
                        ));
                        // The exact callback a left click on a result card fires.
                        ui.invoke_card_open(1, 0);
                    }
                });
            });
        }
    }

    // Diagnostic hook `BILI_UP_TEST=<mid>`: the UP page data path, without the
    // page. Logs the header (`user_card`) and the first page of both orders.
    if let Ok(value) = std::env::var("BILI_UP_TEST") {
        if let Ok(mid) = value.trim().parse::<i64>() {
            if mid > 0 {
                let session_slot = session_slot.clone();
                std::thread::spawn(move || {
                    let mut session = None;
                    for _ in 0..600 {
                        session = active_session(&session_slot);
                        if session.is_some() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let Some(session) = session else {
                        crate::debug_log(&format!("up mid={mid} no_session"));
                        return;
                    };
                    let Some(runtime) = bilibili::shared_runtime() else {
                        return;
                    };
                    runtime.block_on(async move {
                        let (name, avatar, fans, videos) =
                            match session.user_card(mid).await {
                                Ok(card) => card,
                                Err(err) => {
                                    crate::debug_log(&format!("up mid={mid} card err={err:#}"));
                                    return;
                                }
                            };
                        crate::debug_log(&format!(
                            "up mid={mid} name={name:?} videos={videos} fans={fans} avatar={}",
                            u8::from(!avatar.is_empty())
                        ));
                        for order in [bilibili::UpOrder::Plays, bilibili::UpOrder::Newest] {
                            match session.up_videos(mid, order, 1).await {
                                Ok((items, has_more)) => {
                                    let first = items
                                        .first()
                                        .map(|video| video.bvid.as_str())
                                        .unwrap_or("-");
                                    crate::debug_log(&format!(
                                        "up mid={mid} name={name:?} videos={videos} fans={fans} order={} n={} first={first} more={}",
                                        order.param(),
                                        items.len(),
                                        u8::from(has_more),
                                    ));
                                }
                                Err(err) => crate::debug_log(&format!(
                                    "up mid={mid} order={} err={err:#}",
                                    order.param()
                                )),
                            }
                        }
                    });
                });
            }
        }
    }

    // Diagnostic hook `BILI_UP_PAGE_TEST=<mid>`: open the UP page through the
    // real search-用户 callback, switch the order once, then open the first
    // card. The lines that prove the whole path are `up open mid=… name=…`,
    // `up sort=0 n=…`, `up sort=1 n=…` and `up open video index=0 bvid=…`.
    if let Ok(value) = std::env::var("BILI_UP_PAGE_TEST") {
        if let Ok(mid) = value.trim().parse::<i64>() {
            if mid > 0 {
                let weak = weak.clone();
                let session_slot = session_slot.clone();
                let up_page = up_page.clone();
                let up_store = up_store.clone();
                std::thread::spawn(move || {
                    for _ in 0..600 {
                        if active_session(&session_slot).is_some() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let open_weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = open_weak.upgrade() {
                            crate::debug_log(&format!("up page test open mid={mid}"));
                            ui.invoke_open_up_mid(mid.to_string().into());
                        }
                    });
                    let open_rows = up_test_wait_rows(
                        &up_page,
                        &up_store,
                        0,
                        Duration::from_secs(45),
                    );
                    crate::debug_log(&format!("up page test sort=0 rows={open_rows}"));
                    let sort_weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = sort_weak.upgrade() {
                            crate::debug_log("up page test sort=1 request");
                            ui.invoke_up_sort_changed(1);
                        }
                    });
                    let newest_rows = up_test_wait_rows(
                        &up_page,
                        &up_store,
                        1,
                        Duration::from_secs(45),
                    );
                    crate::debug_log(&format!("up page test sort=1 rows={newest_rows}"));
                    let first = up_store
                        .lock()
                        .ok()
                        .and_then(|guard| guard.first().map(|video| video.bvid.clone()))
                        .unwrap_or_default();
                    crate::debug_log(&format!(
                        "up page test resolve index=0 bvid={first}"
                    ));
                    let click_weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = click_weak.upgrade() {
                            ui.invoke_up_open_video(0);
                        }
                    });
                });
            }
        }
    }

    // Diagnostic hook `BILI_VOL_TEST=<n>` (default 50): apply that many rapid
    // volume changes to the live player through the session path and log the
    // first/last three values plus one readback. The `audio volume … restart=0`
    // lines and the absence of `spawn ffplay` between them are the proof that a
    // volume change never restarted the audio process.
    if let Ok(value) = std::env::var("BILI_VOL_TEST") {
        let count = value
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(50)
            .min(500);
        run_volume_test(count);
    }

    // Diagnostic hook (mirrors BILI_PAGE_TEST / BILI_DOWNLOAD_ON_START): drive
    // the video-comments data path for `BILI_COMMENTS_TEST=<aid>` without a
    // player window. It runs the same handlers the UI will call (first page,
    // more, one dialog, 按时间 sort) and logs a compact summary, so the shapes
    // can be checked from BILI_DEBUG_LOG against real data.
    if let Ok(value) = std::env::var("BILI_COMMENTS_TEST") {
        if let Ok(aid) = value.trim().parse::<i64>() {
            if aid > 0 {
                let session_slot = session_slot.clone();
                let comments: CommentsSlot = Arc::new(Mutex::new(CommentsState::default()));
                std::thread::spawn(move || {
                    for _ in 0..240 {
                        if let Some(session) = active_session(&session_slot) {
                            run_comments_test(session, comments, aid);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    crate::debug_log(&format!("comments aid={aid} no_session"));
                });
            }
        }
    }

    // Frameless + compositor alpha, then the in-process frosted-glass loop: the
    // native HWND only exists once the event loop has built the window, so queue
    // the window chrome to run (and retry) on the UI thread after `run()` starts,
    // and let the refresh loop pick the handle up on its own tick.
    start_glass_loop(&ui, &glass_loop);
    schedule_glass(weak.clone(), theme_dark, 120);

    // Self-healing watchdog for the main window: re-assert the frameless style
    // every 1500ms. `reassert_frameless` is a no-op unless something re-decorated
    // the window, so this costs two `GetWindowLongPtr` reads per tick and
    // reverts any re-decoration within one tick even without an activation
    // message. The same tick installs the native close hook, which can only be
    // registered once the window has an HWND.
    player_state.main_frameless_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(1500),
        {
            let weak = weak.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    win::reassert_frameless(ui.window());
                    install_close_hook(ui.window());
                }
            }
        },
    );
    // The native hook is `Send + Sync` and cannot hold the window, so it reaches
    // the UI through this thread-local, installed on the UI thread before the
    // event loop (and therefore before any close) starts.
    set_close_targets(&weak, &settings_store);

    // Landscape default: the winit backend otherwise sizes the window from the
    // content's preferred size, so pin the initial landscape size explicitly.
    ui.window().set_size(slint::LogicalSize::new(1280.0, 800.0));

    if std::env::var("BILI_DRAG_TEST")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        run_drag_test(weak.clone(), main_drag.clone(), 240);
    }

    // Diagnostic hook `BILI_DRAG_RATE_TEST=<ms>` (default 1000): drive the real
    // 8ms drag timer for that long and log the achieved `drag flush hz=…`.
    if let Ok(value) = std::env::var("BILI_DRAG_RATE_TEST") {
        let duration = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(1000)
            .clamp(100, 10_000);
        run_drag_rate_test(
            weak.clone(),
            main_drag.clone(),
            main_drag_stats.clone(),
            Duration::from_millis(duration),
            240,
        );
    }

    // Diagnostic hook (mirrors BILI_DRAG_TEST): drive the real close path once,
    // ~3s after the window is up, so the branch that ran and the resulting
    // native state can be read back from BILI_DEBUG_LOG. `BILI_CLOSE_RESTORE=1`
    // brings the window back a second later like a tray click would.
    if let Ok(value) = std::env::var("BILI_CLOSE_TEST") {
        match value.trim().parse::<u32>() {
            Ok(raw) => {
                let restore = std::env::var("BILI_CLOSE_RESTORE")
                    .map(|flag| flag == "1")
                    .unwrap_or(false);
                run_close_test(
                    weak.clone(),
                    settings_store.clone(),
                    settings::normalise_close_action(raw),
                    restore,
                    20,
                );
            }
            Err(err) => crate::debug_log(&format!("close test ignored value={value} err={err}")),
        }
    }

    // Random starting tip on the login view: Slint exposes no clock to app code,
    // so publish a non-zero seed once. `tip_seed == 0` means "not seeded yet".
    {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.subsec_millis() as i32) | 1)
            .unwrap_or(1);
        ui.set_tip_seed(seed);
    }

    // Window clock for the freshness chips (`now_ts`, unix seconds): publish
    // once now, then once a minute. The timer lives in `main`'s scope until the
    // event loop returns, because a Slint timer stops as soon as its last
    // handle drops.
    let now_timer = slint::Timer::default();
    {
        let weak = weak.clone();
        let publish = move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_now_ts(now_unix_secs());
            }
        };
        publish();
        now_timer.start(slint::TimerMode::Repeated, Duration::from_secs(60), publish);
    }

    ui.run()?;

    // No window can be created after the event loop returns.
    win::unhook_create_hook();

    tray::shutdown();

    // Signal every player worker to stop so their ffmpeg/ffplay children are
    // reaped before the process exits (they would otherwise outlive us).
    for session in player_sessions(&player_state) {
        session.control.close();
    }
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

/// `BILI_CLOSE_PROFILE=1`: one `close profile ...` line per player close.
///
/// Every field is a completion timestamp in milliseconds since the callback's
/// first statement (`t0=0`), so the step that dominated a close is readable
/// from `BILI_DEBUG_LOG` without a profiler.
struct CloseClock {
    started: Instant,
    enabled: bool,
    hide: Cell<u128>,
    glass: Cell<u128>,
    control: Cell<u128>,
    extras: Cell<u128>,
}

impl CloseClock {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            enabled: win::close_profile_enabled(),
            hide: Cell::new(0),
            glass: Cell::new(0),
            control: Cell::new(0),
            extras: Cell::new(0),
        }
    }

    /// Milliseconds since entry (`0` when profiling is off, so the caller can
    /// read the clock unconditionally).
    fn ms(&self) -> u128 {
        if !self.enabled {
            return 0;
        }
        self.started.elapsed().as_millis()
    }

    fn log(&self) {
        if !self.enabled {
            return;
        }
        crate::debug_log(&format!(
            "close profile t0=0 hide={} glass={} control={} extras={} total={}",
            self.hide.get(),
            self.glass.get(),
            self.control.get(),
            self.extras.get(),
            self.ms(),
        ));
    }
}

/// Shared body of both player close paths (the UI's ✕ and a native close
/// request). Extracted so the two can never drift apart.
///
/// Closes exactly the window that called it: its loop, its glass worker, its
/// decode session. Other player windows are untouched and keep playing.
fn close_player_window(
    state: &Rc<PlayerState>,
    window: &Weak<PlayerWindow>,
    session: &std::rc::Weak<PlayerSession>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    tag: &str,
) {
    let clock = CloseClock::new();
    let record = session.upgrade();
    let hwnd = record
        .as_ref()
        .map(|record| record.hwnd.get())
        .filter(|hwnd| *hwnd != 0)
        .or_else(|| window.upgrade().and_then(|player| win::hwnd_of(player.window())));
    // A second close (a native WM_CLOSE racing the ✕) is a no-op after the
    // first: the window is already hidden and its session retired.
    if let Some(record) = record.as_ref() {
        if record.closed.get() {
            crate::debug_log(&format!(
                "player close ignored duplicate tag={tag} win={}",
                record.id
            ));
            return;
        }
        record.closed.set(true);
    }
    // 1. The user-visible part FIRST, in this event: the window is gone before
    //    any teardown runs. Everything below is invisible work, so a slow step
    //    can no longer delay it disappearing.
    if let Some(player) = window.upgrade() {
        toast_forget(&player);
        player.set_danmaku_running(false);
        let _ = player.hide();
    }
    // The mini belongs to exactly one player: only its owner closes it.
    if let Some(hwnd) = hwnd {
        if state.mini_owner.get() == hwnd {
            if let Some(mini) = state.mini.borrow_mut().take() {
                let _ = mini.hide();
            }
            state.mini_owner.set(0);
        }
    }
    clock.hide.set(clock.ms());

    // 2. The base rect, from the still-valid player rect (a hidden window keeps
    //    its placement), so the next open restores it.
    if let (Some(record), Some(player)) = (record.as_ref(), window.upgrade()) {
        persist_player_layout(
            &record.panel_geom,
            player.window(),
            settings_store,
            record.cascade,
            "close",
        );
    }

    // 3. This window's glass loop, and only this one.
    if let Some(record) = record.as_ref() {
        stop_player_glass(record);
    }
    clock.glass.set(clock.ms());

    // 4. This window's decode session: `close()` only stores the flag and hands
    //    the prefetch proxy to a background thread (its shutdown waits, bounded,
    //    for the server task), so no ffmpeg child is waited on here.
    if let Some(record) = record.as_ref() {
        record.control.close();
        if let Some(loops) = record.loops.borrow_mut().take() {
            stop_player_loops(&loops);
        }
    }
    clock.control.set(clock.ms());

    // 5. Unregister the window. The queue follows a surviving player when the
    //    closed one was focused, and only stops when no player is left.
    let mut closed_id = 0;
    if let Some(record) = record.as_ref() {
        closed_id = record.id;
        state
            .players
            .borrow_mut()
            .retain(|open| !Rc::ptr_eq(open, record));
        let was_focused = state
            .focused
            .borrow()
            .as_ref()
            .map(|focused| Rc::ptr_eq(focused, record))
            .unwrap_or(false);
        if was_focused {
            *state.focused.borrow_mut() = None;
        }
        let remaining = player_sessions(state);
        if let (true, Some(next)) = (was_focused, remaining.last().cloned()) {
            focus_player(state, &next, true);
        }
        if remaining.is_empty() {
            if let Some(host) = playlist() {
                host.on_close();
            } else {
                clear_active_player();
            }
            player::set_active_control(None);
            restore_main_min_track(state);
        }
        resume_main_window(state);
    }
    clock.extras.set(clock.ms());

    // 6. Keep the window object alive past this callback: dropping the last
    //    handle while Slint is executing one of its own callbacks is not safe.
    //    The deferred drop runs on a later event-loop turn.
    if let Some(record) = record {
        retire_session(record);
    }

    clock.log();
    crate::debug_log(&format!(
        "player close tag={tag} win={closed_id} remaining={}",
        state.players.borrow().len()
    ));
}

/// Drop a closed player window on a later event-loop turn.
///
/// The close runs inside that window's own Slint callback, so the handle must
/// outlive the callback; a zero-delay timer is enough and keeps the memory
/// (frame texture + models) from piling up across opens.
fn retire_session(session: Rc<PlayerSession>) {
    let id = session.id;
    slint::Timer::single_shot(Duration::from_millis(200), move || {
        drop(session);
        crate::debug_log(&format!("player retired win={id}"));
    });
}

/// How an open treats the players that are already up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpenMode {
    /// Left click / queue: close the focused window and put this video there.
    Replace,
    /// Right click (card_open mode 1): a second window, leaving the others
    /// playing.
    NewWindow,
}

/// Create and show the player window for `feed_store[index]` and start its
/// playback worker. Replaces the focused player (card_open mode 0).
fn open_player(
    index: i32,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) {
    open_player_with(
        index,
        OpenMode::Replace,
        session_slot,
        feed_store,
        state,
        settings_store,
    );
}

/// Open `feed_store[index]` in a new window or in place of the focused one.
fn open_player_with(
    index: i32,
    mode: OpenMode,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) {
    let index = index.max(0) as usize;
    let video = match feed_store.lock() {
        Ok(guard) => guard.get(index).cloned(),
        Err(_) => None,
    };
    let Some(video) = video else {
        crate::debug_log(&format!("card open miss index={index} mode={mode:?}"));
        return;
    };
    let Some(session) = active_session(session_slot) else {
        crate::debug_log(&format!("card open skipped index={index} mode={mode:?} reason=no-session"));
        return;
    };
    open_player_inner(session, video, state, settings_store, mode);
}

/// Play one explicit `bvid`/`cid` pair (used by the detail page's 播放 button).
fn open_player_for(
    session_slot: &SessionSlot,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    bvid: String,
    cid: u64,
    title: String,
    aid: i64,
) {
    let Some(session) = active_session(session_slot) else {
        return;
    };
    let video = bilibili::FeedVideo {
        bvid,
        cid,
        aid: aid.max(0) as u64,
        title,
        pic: String::new(),
        owner: String::new(),
        owner_mid: 0,
        view: 0,
        duration: 0,
        pub_ts: 0,
    };
    open_player_inner(session, video, state, settings_store, OpenMode::Replace);
}

// ================= 外扩面板: native window growth (Task 1) =================
//
// `ui/app.slint` reserves a strip inside the window when one of the two side
// panel ints is non-zero (`left_panel_w` 320, `right_panel_w` 360). Rust's only
// job is to grow or shrink the OUTER window by the same amount, from the
// correct edge, so the picture band keeps its exact size and position on
// screen:
//
//   right panel   width  +360, x unchanged
//   left panel    width  +320, x -320   (the picture must not move)
//
// The bottom strip (the 弹幕列表 page) was removed from the UI with its
// `bottom_panel = 1` page, so the bottom edge never changes the window: the
// `bottom_panel` int stays declared (inert) and contributes 0 to the geometry.
//
// In fullscreen (`panels_overlay`) the panels float over the picture instead
// and the window is never touched.
//
// Units: the strip sizes are Slint logical px, `GetWindowRect` is physical px.
// They are converted once per change with the window's scale factor, so the
// delta is exact at 100% DPI and proportionally correct at any other.

/// Strip sizes; keep in lockstep with `left_panel_w` / `right_panel_w` in
/// `ui/app.slint`.
const PANEL_LEFT_LOGICAL: f64 = 320.0;
const PANEL_RIGHT_LOGICAL: f64 = 360.0;
/// Smallest video band the UI allows (`PlayerWindow` min-width / min-height in
/// `ui/app.slint`). The native minimum is this plus the open strips.
const PLAYER_MIN_VIDEO_W: f64 = 480.0;
const PLAYER_MIN_VIDEO_H: f64 = 270.0;

/// A physical-pixel window rect (`GetWindowRect` / `SetWindowPos`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct WinRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl WinRect {
    fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }

    fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }
}

impl std::fmt::Display for WinRect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{},{}x{}", self.x, self.y, self.w, self.h)
    }
}

/// Monitor bounds: `monitor` is the full screen (fullscreen detection),
/// `work` excludes the taskbar and is what panel growth is clamped to.
struct MonitorAreas {
    monitor: WinRect,
    work: WinRect,
}

/// Per-player panel geometry. One of these is created in `open_player_inner`
/// and captured by that player's `panel_changed` handler and danmaku tick, so
/// two player windows never share a base size.
#[derive(Default)]
struct PanelGeom {
    /// Panel ints last applied to the window.
    left: i32,
    right: i32,
    bottom: i32,
    /// Window size with NO panel open, derived at every change as
    /// `live - extras currently open` so a manual resize is respected.
    base_w: i32,
    base_h: i32,
    /// Video-area top-left with clamping NOT applied. Kept across a clamped
    /// open so closing the panel restores the exact original position.
    home: Option<(i32, i32)>,
    /// The rect we last applied; a live rect that differs means the user moved
    /// or resized the window, so `home` follows it.
    applied: Option<WinRect>,
    /// `true` while the window is fullscreen and the panels float over it.
    overlay: bool,
}

fn panel_phys(logical: f64, scale: f32) -> i32 {
    (logical * f64::from(scale.max(0.2))).round() as i32
}

/// Physical `(width, height)` a panel set adds to the window. Mirrors the
/// `left_w` / `right_w` bindings in `ui/app.slint`: left/right count when
/// non-zero. The bottom strip was removed with the danmaku-list panel, so the
/// bottom int is inert and its height is always 0.
fn panel_extras(left: i32, right: i32, _bottom: i32, scale: f32) -> (i32, i32) {
    let mut w = 0;
    // The left panel no longer exists (its int is inert); only the right strip
    // can reserve width.
    let _ = left;
    if right != 0 {
        w += panel_phys(PANEL_RIGHT_LOGICAL, scale);
    }
    (w, 0)
}

fn player_window_rect(window: &slint::Window) -> Option<WinRect> {
    let hwnd = win::hwnd_of(window)?;
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as HWND, &mut rect) == 0 {
            return None;
        }
        Some(WinRect {
            x: rect.left,
            y: rect.top,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        })
    }
}

fn monitor_areas(window: &slint::Window) -> Option<MonitorAreas> {
    let hwnd = win::hwnd_of(window)?;
    unsafe {
        let monitor = MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let to_rect = |r: RECT| WinRect {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        };
        Some(MonitorAreas {
            monitor: to_rect(info.rcMonitor),
            work: to_rect(info.rcWork),
        })
    }
}

/// Move/resize without activating and without touching the z-order, so the
/// player never flickers or steals focus when a panel opens. `SWP_NOCOPYBITS`
/// is deliberately NOT passed: the newly exposed strip must paint.
fn set_player_window_rect(window: &slint::Window, rect: WinRect) -> bool {
    let Some(hwnd) = win::hwnd_of(window) else {
        return false;
    };
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            SWP_NOACTIVATE | SWP_NOZORDER,
        ) != 0
    }
}

fn rect_close(a: WinRect, b: WinRect) -> bool {
    (a.x - b.x).abs() <= 2
        && (a.y - b.y).abs() <= 2
        && (a.w - b.w).abs() <= 2
        && (a.h - b.h).abs() <= 2
}

/// The player's BASE rect in logical px: the window as it would be with no
/// side panel open.
///
/// `[x, y, w, h]`, `x`/`y` being the VIDEO band's top-left - the same `home`
/// the panel code derives - so a restored window grows from the right anchor
/// when a panel opens afterwards. Derived from the live rect minus the strips
/// that are open now, in physical px, then converted with the window's scale
/// factor.
fn player_base_layout(geom: &RefCell<PanelGeom>, window: &slint::Window) -> Option<[i32; 4]> {
    let scale = window.scale_factor().max(0.2);
    let to_logical = f64::from(scale);
    let state = geom.try_borrow().ok()?;
    let base = if state.overlay {
        // Fullscreen: the live rect is the screen, so the remembered
        // pre-fullscreen base is the only meaningful value.
        let (hx, hy) = state.home?;
        [hx, hy, state.base_w.max(1), state.base_h.max(1)]
    } else {
        let live = player_window_rect(window)?;
        let (extra_w, extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
        let left_px = if state.left != 0 {
            panel_phys(PANEL_LEFT_LOGICAL, scale)
        } else {
            0
        };
        [
            live.x.saturating_add(left_px),
            live.y,
            (live.w - extra_w).max(1),
            (live.h - extra_h).max(1),
        ]
    };
    Some([
        (f64::from(base[0]) / to_logical).round() as i32,
        (f64::from(base[1]) / to_logical).round() as i32,
        (f64::from(base[2]) / to_logical).round() as i32,
        (f64::from(base[3]) / to_logical).round() as i32,
    ])
}

/// Persist the base rect, skipping the write (and the log line) when nothing
/// changed. Called on drag end, on a settled move/resize and on both close
/// paths.
fn persist_player_layout(
    geom: &Rc<RefCell<PanelGeom>>,
    window: &slint::Window,
    settings_store: &Arc<Mutex<settings::Settings>>,
    offset: i32,
    tag: &str,
) {
    let Some(mut layout) = player_base_layout(geom, window) else {
        return;
    };
    // A cascaded second window persists the rect the user would get WITHOUT
    // the cascade, so opening/closing extra players never drifts the saved
    // position of the first one.
    layout[0] = layout[0].saturating_sub(offset);
    layout[1] = layout[1].saturating_sub(offset);
    let unchanged = settings_store
        .lock()
        .map(|prefs| prefs.player_layout == Some(layout))
        .unwrap_or(true);
    if unchanged {
        return;
    }
    update_settings(settings_store, |prefs| prefs.player_layout = Some(layout));
    crate::debug_log(&format!(
        "player layout save x={} y={} w={} h={} offset={offset} tag={tag}",
        layout[0], layout[1], layout[2], layout[3]
    ));
}

/// Intersection area (physical px²) of a rect and the work area of the monitor
/// it lands on. `0` when that monitor no longer exists where the rect is.
fn rect_work_overlap(rect: WinRect) -> i32 {
    let Some((wx, wy, ww, wh)) = win::work_area_for_rect(rect.x, rect.y, rect.w, rect.h) else {
        return 0;
    };
    let left = rect.x.max(wx);
    let top = rect.y.max(wy);
    let right = rect.right().min(wx.saturating_add(ww));
    let bottom = rect.bottom().min(wy.saturating_add(wh));
    if right <= left || bottom <= top {
        0
    } else {
        (right - left).saturating_mul(bottom - top)
    }
}

/// Restore the saved player base rect (logical px), BEFORE `show()`.
///
/// The logical position/size are requested from Slint, which applies them when
/// winit creates the window, so the first paint is already at the restored
/// spot. Only a rect that still lands on a monitor's work area is applied;
/// anything else (a hand-edited file, a monitor that is gone) falls back to the
/// OS default placement and is logged as `source=default`.
///
/// Returns the physical target, which [`verify_player_layout`] re-applies and
/// logs once the player has a native handle (the player HWND is created a few
/// frames after `show()`).
fn restore_player_layout(
    player: &PlayerWindow,
    saved: Option<[i32; 4]>,
    offset: i32,
) -> Option<WinRect> {
    let window = player.window();
    let rect = saved?;
    let scale = f64::from(window.scale_factor().max(0.2));
    // Clamp to the UI's own minimum: a hand-edited file must not ask for a
    // window the player layout cannot fit.
    let w = rect[2].max(PLAYER_MIN_VIDEO_W as i32);
    let h = rect[3].max(PLAYER_MIN_VIDEO_H as i32);
    // A NEW window opened next to existing ones is cascaded by `offset`
    // logical px, so it is visibly a second player instead of hiding exactly
    // behind the first.
    let x = rect[0].saturating_add(offset);
    let y = rect[1].saturating_add(offset);
    let target = WinRect {
        x: (f64::from(x) * scale).round() as i32,
        y: (f64::from(y) * scale).round() as i32,
        w: (f64::from(w) * scale).round() as i32,
        h: (f64::from(h) * scale).round() as i32,
    };
    if rect_work_overlap(target) == 0 {
        crate::debug_log(&format!(
            "player layout restore x={x} y={y} w={w} h={h} source=default reason=offscreen"
        ));
        return None;
    }
    window.set_position(slint::LogicalPosition::new(x as f32, y as f32));
    window.set_size(slint::LogicalSize::new(w as f32, h as f32));
    Some(target)
}

/// Apply and then LOG the player layout once the window has a native handle.
///
/// Retries on a 16ms timer (the player's HWND is mapped a few frames after
/// `show()`), re-applies the physical target so a stale pre-show scale factor
/// cannot misplace the window, and emits the one
/// `player layout restore ... source=saved|default` line that proves which
/// branch ran.
fn verify_player_layout(weak: Weak<PlayerWindow>, target: Option<WinRect>, tries: u32) {
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        let Some(player) = weak.upgrade() else {
            return;
        };
        let window = player.window();
        if win::hwnd_of(window).is_none() {
            if tries > 0 {
                verify_player_layout(weak.clone(), target, tries - 1);
            } else {
                crate::debug_log("player layout restore pending no-hwnd");
            }
            return;
        }
        let mut source = "default";
        if let Some(target) = target {
            if rect_work_overlap(target) > 0 && set_player_window_rect(window, target) {
                source = "saved";
            }
        }
        let scale = f64::from(window.scale_factor().max(0.2));
        let applied = player_window_rect(window).map(|rect| {
            [
                (f64::from(rect.x) / scale).round() as i32,
                (f64::from(rect.y) / scale).round() as i32,
                (f64::from(rect.w) / scale).round() as i32,
                (f64::from(rect.h) / scale).round() as i32,
            ]
        });
        let [x, y, w, h] = applied.unwrap_or([0, 0, 0, 0]);
        crate::debug_log(&format!(
            "player layout restore x={x} y={y} w={w} h={h} source={source}"
        ));
    });
}

/// Native minimum size: the smallest video band plus the strips that are open,
/// so the player can always be shrunk down to a valid picture.
fn player_min_track(window: &slint::Window, left: i32, right: i32, bottom: i32) {
    let scale = window.scale_factor().max(0.2);
    let (extra_w, extra_h) = panel_extras(left, right, bottom, scale);
    let w = panel_phys(PLAYER_MIN_VIDEO_W, scale) + extra_w;
    let h = panel_phys(PLAYER_MIN_VIDEO_H, scale) + extra_h;
    win::set_min_track_size(window, w.max(1), h.max(1));
}

/// `win::set_min_track_size` records ONE window process-wide, so the player's
/// minimum replaces the main window's while it is up. Both close paths hand
/// the slot back (values kept in lockstep with the main window's call).
fn restore_main_min_track(state: &Rc<PlayerState>) {
    if let Some(main) = state.main.upgrade() {
        let scale = main.window().scale_factor();
        win::set_min_track_size(
            main.window(),
            (PLAYER_MIN_VIDEO_W * f64::from(scale)) as i32,
            (360.0 * f64::from(scale)) as i32,
        );
    }
}

/// Grow/shrink the player window to the given panel ints.
///
/// `force` re-applies the CURRENT ints (used when returning from fullscreen)
/// and trusts the base/home captured on fullscreen entry instead of
/// re-deriving them from the rect winit just restored.
fn apply_panel_geometry(
    geom: &Rc<RefCell<PanelGeom>>,
    window: &slint::Window,
    left: i32,
    right: i32,
    bottom: i32,
    force: bool,
) {
    let mut state = geom.borrow_mut();
    if !force && state.left == left && state.right == right && state.bottom == bottom {
        return;
    }
    let scale = window.scale_factor().max(0.2);
    let old_left = state.left;
    let (old_extra_w, old_extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
    let Some(live) = player_window_rect(window) else {
        state.left = left;
        state.right = right;
        state.bottom = bottom;
        crate::debug_log(&format!(
            "panel left={left} right={right} bottom={bottom} overlay={} win=? base={}x{} no-hwnd",
            state.overlay, state.base_w, state.base_h
        ));
        return;
    };
    // Fullscreen: the strips float over the picture, the window is fullscreen
    // already and must not be resized. Remember the ints for the way out.
    if state.overlay {
        state.left = left;
        state.right = right;
        state.bottom = bottom;
        state.applied = None;
        crate::debug_log(&format!(
            "panel left={left} right={right} bottom={bottom} overlay=true win={}x{} \
             base={}x{} fullscreen-skip",
            live.w, live.h, state.base_w, state.base_h
        ));
        return;
    }
    let old_left_px = if old_left != 0 {
        panel_phys(PANEL_LEFT_LOGICAL, scale)
    } else {
        0
    };
    let live_video = (live.x.saturating_add(old_left_px), live.y);
    if force {
        // Returning from fullscreen: base/home were captured before the
        // flip, so only trust the live rect when they were never captured.
        if state.base_w <= 0 || state.base_h <= 0 || state.home.is_none() {
            state.base_w = (live.w - old_extra_w).max(1);
            state.base_h = (live.h - old_extra_h).max(1);
            state.home = Some(live_video);
        }
    } else {
        // Derive the no-panel base from the live size at every change: a
        // manual resize while a panel is open becomes part of the base.
        state.base_w = (live.w - old_extra_w).max(1);
        state.base_h = (live.h - old_extra_h).max(1);
        match state.applied {
            Some(applied) if rect_close(applied, live) => {}
            Some(applied) => {
                // The user moved the window while a panel was open: the
                // remembered home follows by the same delta.
                if let Some((hx, hy)) = state.home {
                    state.home = Some((
                        hx.saturating_add(live.x - applied.x),
                        hy.saturating_add(live.y - applied.y),
                    ));
                }
            }
            None => state.home = Some(live_video),
        }
    }
    let (home_x, home_y) = state.home.unwrap_or(live_video);
    let (new_extra_w, new_extra_h) = panel_extras(left, right, bottom, scale);
    let new_left_px = if left != 0 {
        panel_phys(PANEL_LEFT_LOGICAL, scale)
    } else {
        0
    };
    let mut target = WinRect {
        x: home_x.saturating_sub(new_left_px),
        y: home_y,
        w: (state.base_w + new_extra_w).max(1),
        h: (state.base_h + new_extra_h).max(1),
    };
    // Clamp into the work area: a right/bottom overflow slides the window
    // left/up by exactly that overflow. `home` keeps the unclamped spot, so
    // closing the panel restores the original rect. A window the user parked
    // partly off-screen is left alone unless it actually overflows.
    if let Some(area) = monitor_areas(window) {
        let work = area.work;
        if target.right() > work.right() {
            target.x = target.x.saturating_sub(target.right() - work.right());
            if target.x < work.x {
                target.x = work.x;
            }
        }
        if target.bottom() > work.bottom() {
            target.y = target.y.saturating_sub(target.bottom() - work.bottom());
            if target.y < work.y {
                target.y = work.y;
            }
        }
    }
    let ok = set_player_window_rect(window, target);
    state.applied = Some(target);
    state.left = left;
    state.right = right;
    state.bottom = bottom;
    player_min_track(window, left, right, bottom);
    crate::debug_log(&format!(
        "panel left={left} right={right} bottom={bottom} overlay=false win={}x{} base={}x{} \
         rect={} video={}x{} ok={ok} scale={scale:.2}",
        target.w,
        target.h,
        state.base_w,
        state.base_h,
        target,
        target.w - new_extra_w,
        target.h - new_extra_h
    ));
}

/// Capture the pre-fullscreen base size and the video's ideal position before
/// the window goes fullscreen; panels then float over the picture untouched.
fn enter_panel_overlay(geom: &Rc<RefCell<PanelGeom>>, window: &slint::Window) {
    let mut state = geom.borrow_mut();
    if state.overlay {
        return;
    }
    let scale = window.scale_factor().max(0.2);
    if let Some(live) = player_window_rect(window) {
        let (extra_w, extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
        state.base_w = (live.w - extra_w).max(1);
        state.base_h = (live.h - extra_h).max(1);
        let left_px = if state.left != 0 {
            panel_phys(PANEL_LEFT_LOGICAL, scale)
        } else {
            0
        };
        state.home = Some((live.x.saturating_add(left_px), live.y));
    }
    state.overlay = true;
    state.applied = None;
    crate::debug_log(&format!(
        "panel overlay=true enter left={} right={} bottom={} base={}x{}",
        state.left, state.right, state.bottom, state.base_w, state.base_h
    ));
}

/// Returning from fullscreen: the window is back at its pre-fullscreen rect;
/// grow it by whatever strips are open now.
fn exit_panel_overlay(geom: &Rc<RefCell<PanelGeom>>, window: &slint::Window) {
    let (left, right, bottom) = {
        let mut state = geom.borrow_mut();
        state.overlay = false;
        state.applied = None;
        (state.left, state.right, state.bottom)
    };
    apply_panel_geometry(geom, window, left, right, bottom, true);
}

/// Winit restores the pre-fullscreen rect asynchronously; poll until the window
/// no longer covers the whole monitor, then regrow.
fn schedule_overlay_exit(
    geom: Rc<RefCell<PanelGeom>>,
    weak: Weak<PlayerWindow>,
    tries: u32,
) {
    slint::Timer::single_shot(Duration::from_millis(120), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let still_fullscreen = match (
            player_window_rect(window.window()),
            monitor_areas(window.window()),
        ) {
            (Some(live), Some(area)) => {
                live.w >= area.monitor.w && live.h >= area.monitor.h
            }
            _ => false,
        };
        if still_fullscreen && tries < 25 {
            schedule_overlay_exit(geom.clone(), weak.clone(), tries + 1);
            return;
        }
        exit_panel_overlay(&geom, window.window());
    });
}

// --------------------- BILI_PANEL_TEST geometry QA hook ---------------------
//
// `BILI_PANEL_TEST=<left>,<right>,<bottom>,<ms>` drives the panel ints after
// the player opens (right, then left - one step per non-zero side), waits `ms`
// between steps, logs the live `GetWindowRect`, the derived base and the
// expected size at every step, then closes both sides and logs the final rect.
// `bottom` is accepted for compatibility but inert (its strip is gone) and is
// never written. It is the SAME property write the UI's own toggle functions
// do, so no synthetic input is involved.

#[link(name = "user32")]
extern "system" {
    /// `PrintWindow` (winuser.h). Declared here because windows-sys exposes it
    /// only under `Win32_Storage_Xps`, which this app does not enable; the QA
    /// hook uses it to capture the grown player window.
    fn PrintWindow(hwnd: isize, hdc: *mut std::ffi::c_void, flags: u32) -> i32;
}

/// Paint the whole window into a PNG (once per QA step).
fn capture_player_png(
    window: &slint::Window,
    tag: &str,
) -> Option<(u32, u32)> {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ, SRCCOPY,
    };
    let hwnd = win::hwnd_of(window)?;
    let rect = player_window_rect(window)?;
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
        if PrintWindow(hwnd, mem, 2) == 0 {
            // PW_RENDERFULLCONTENT refused: fall back to the screen pixels.
            BitBlt(mem, 0, 0, w, h, screen, rect.x, rect.y, SRCCOPY);
        }
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
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    let path = std::env::temp_dir().join(format!(
        "bili_panel_test_{tag}_{}x{}.png",
        w, h
    ));
    let png = image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
    match png.save(&path) {
        Ok(()) => {
            crate::debug_log(&format!(
                "paneltest png tag={tag} path={} size={w}x{h}",
                path.display()
            ));
            Some((w as u32, h as u32))
        }
        Err(err) => {
            crate::debug_log(&format!("paneltest png tag={tag} err={err}"));
            None
        }
    }
}

fn panel_test_spec() -> Option<(i32, i32, i32, u64)> {
    let raw = std::env::var("BILI_PANEL_TEST").ok()?;
    let mut parts = raw.split(',');
    let left = parts.next()?.trim().parse::<i32>().ok()?;
    let right = parts.next()?.trim().parse::<i32>().ok()?;
    let bottom = parts.next()?.trim().parse::<i32>().ok()?;
    let ms = parts
        .next()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1200)
        .clamp(200, 20_000);
    Some((left, right, bottom, ms))
}

fn panel_test_start(
    weak: Weak<PlayerWindow>,
    geom: Rc<RefCell<PanelGeom>>,
    left: i32,
    right: i32,
    bottom: i32,
    gap_ms: u64,
) {
    // Cumulative steps in the order right -> left, then close both. The
    // bottom int is inert: its strip was removed from the UI, so it is never
    // written and never adds a step (or height).
    let mut steps: Vec<(i32, i32, i32, String)> = Vec::new();
    let (mut open_l, mut open_r) = (0, 0);
    if right != 0 {
        open_r = right;
        steps.push((open_l, open_r, 0, format!("right={right}")));
    }
    if left != 0 {
        open_l = left;
        steps.push((open_l, open_r, 0, format!("left={left}")));
    }
    steps.push((0, 0, 0, "close-all".to_string()));
    crate::debug_log(&format!(
        "paneltest start left={left} right={right} bottom={bottom} bottom_inert=1 gap_ms={gap_ms} \
         steps={}",
        steps.len()
    ));
    let steps = Rc::new(steps);
    // Let playback start so the geometry is the settled one.
    slint::Timer::single_shot(Duration::from_millis(1500), move || {
        panel_test_step(weak, geom, steps, 0, gap_ms);
    });
}

fn panel_test_step(
    weak: Weak<PlayerWindow>,
    geom: Rc<RefCell<PanelGeom>>,
    steps: Rc<Vec<(i32, i32, i32, String)>>,
    index: usize,
    gap_ms: u64,
) {
    let geom_rc = Rc::clone(&geom);
    let Some((left, right, _bottom, tag)) = steps.get(index).cloned() else {
        return;
    };
    let Some(window) = weak.upgrade() else {
        return;
    };
    window.set_left_panel(0);
    // `left_panel` is inert now (only the right panel exists); a non-zero value
    // would still be honoured by nothing, so the test normalises it away.
    window.set_right_panel(if right != 0 { 1 } else { 0 });
    if right != 0 && steps.get(index).map(|s| s.1).unwrap_or(0) > 1 {
        window.set_right_tab(right.min(3));
    }
    // `bottom_panel` is deliberately NOT written: the bottom strip was removed
    // and the int is inert, so it must never become non-zero again.
    crate::debug_log(&format!(
        "paneltest set step={index} tag={tag} left={left} right={right} bottom=0 inert=1"
    ));
    let weak_next = weak.clone();
    let steps_next = steps.clone();
    let tag_next = tag.clone();
    slint::Timer::single_shot(Duration::from_millis(gap_ms), move || {
        if let Some(window) = weak_next.upgrade() {
            let scale = window.window().scale_factor().max(0.2);
            let state = geom_rc.borrow();
            let (extra_w, extra_h) = panel_extras(state.left, state.right, state.bottom, scale);
            let expect_w = state.base_w + extra_w;
            let expect_h = state.base_h + extra_h;
            let live = player_window_rect(window.window());
            let video = live.map(|rect| {
                (
                    rect.w - extra_w,
                    rect.h - extra_h,
                )
            });
            let aspect = video
                .filter(|(_, h)| *h > 0)
                .map(|(w, h)| w as f32 / h as f32)
                .unwrap_or(0.0);
            crate::debug_log(&format!(
                "paneltest rect step={index} tag={tag_next} has_rect={} rect={} base={}x{} \
                 expect={expect_w}x{expect_h} video={} aspect={aspect:.4} overlay={} scale={scale:.2}",
                live.is_some(),
                live.map(|rect| rect.to_string()).unwrap_or_else(|| "-".to_string()),
                state.base_w,
                state.base_h,
                video
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "-".to_string()),
                state.overlay
            ));
            drop(state);
            let _ = capture_player_png(window.window(), &format!("step{index}_{tag_next}"));
        }
        panel_test_step(weak_next, geom_rc, steps_next, index + 1, gap_ms);
    });
}

// -------------------------- 网络分析 profiler ------------------------------

/// Full-scale span of the buffer bar: `buffer_s / 2.0`, clamped 0..1. The
/// decode-ahead ring holds ~0.53 s at 30 fps, so the bar reads roughly a
/// quarter when the ring is full; 2 s is the "deep buffer" mark, chosen so
/// ordinary playback is visible instead of pinning a full bar.
const NET_BUFFER_FULL_SCALE_S: f32 = 2.0;
/// How often the pre-formatted strings are refreshed (the profiler itself
/// samples at 4 Hz, so faster would only repeat values).
const NET_PUBLISH_MS: i64 = 250;
/// One sparkline sample per second - the panel labels it 每秒一次.
const NET_HISTORY_MS: i64 = 1000;
/// Samples kept by `net_history` (64 s of history).
const NET_HISTORY_LEN: usize = 64;

fn codec_name(codecid: u32) -> &'static str {
    match codecid {
        7 => "AVC",
        12 => "HEVC",
        13 => "AV1",
        _ => "-",
    }
}

/// `172 kbps` / `1.6 Mbps`.
fn fmt_bps(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{} kbps", (bps + 500) / 1000)
    }
}

/// Metered upstream throughput for `net_rate` while the prefetch proxy is
/// active: `3.2 MB/s`, no `≈`, because these are counted bytes.
fn fmt_socket_rate(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} MB/s", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.0} KB/s", bps as f64 / 1_000.0)
    } else {
        format!("{bps} B/s")
    }
}

/// One `prefetch stats` line per second: rate, totals, the seek warm points
/// and the per-host split. The panel has no spare property for either
/// (`net_bitrate` stays the track's declared bitrate, which is a different,
/// honest number), so this is the diagnostic channel for them.
fn log_prefetch_split(bvid: &str, stats: &prefetch::PrefetchStats) {
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
fn trace_buffer_state(
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

/// Publish the eleven `net_*` strings. `net_rate` is metered
/// ([`prefetch::PrefetchStats::bytes_per_sec`], no `≈`) while the loopback
/// proxy feeds the decoder, and the profiler's ESTIMATE (`≈`) otherwise;
/// `net_bitrate` is always the track's declared bitrate.
/// Returns the buffer ratio so the caller can append it to the sparkline.
fn publish_net_stats(
    window: &PlayerWindow,
    stats: Option<&player::Stats>,
    prefetch: Option<&prefetch::PrefetchStats>,
) -> f32 {
    let Some(stats) = stats else {
        // No track yet: clear everything so the panel reads "-" per row.
        window.set_net_fps_presented(SharedString::default());
        window.set_net_fps_decoded(SharedString::default());
        window.set_net_first_frame(SharedString::default());
        window.set_net_buffer(SharedString::default());
        window.set_net_rate(SharedString::default());
        window.set_net_bitrate(SharedString::default());
        window.set_net_progress(SharedString::default());
        window.set_net_stalls(SharedString::default());
        window.set_net_codec(SharedString::default());
        window.set_net_buffer_ratio(0.0);
        return 0.0;
    };
    window.set_net_fps_presented(format!("{:.1} fps", stats.fps_presented).into());
    window.set_net_fps_decoded(format!("{:.1} fps", stats.fps_decoded).into());
    window.set_net_first_frame(
        match stats.first_frame_ms {
            Some(ms) => format!("{ms} ms"),
            None => "-".to_string(),
        }
        .into(),
    );
    window.set_net_buffer(
        format!(
            "{:.2} s ({:+.2})",
            stats.buffer_s, stats.buffer_delta_per_s
        )
        .into(),
    );
    window.set_net_rate(
        match prefetch {
            Some(metered) => fmt_socket_rate(metered.bytes_per_sec),
            None => format!("≈ {}", fmt_bps(stats.estimated_bps)),
        }
        .into(),
    );
    window.set_net_bitrate(fmt_bps(stats.declared_bps).into());
    window.set_net_progress(
        format!(
            "{} / {}",
            format_mmss(stats.position_ms),
            format_mmss(stats.duration_ms)
        )
        .into(),
    );
    window.set_net_stalls(
        format!("{} 次 · 最长 {} ms", stats.stalls, stats.longest_stall_ms).into(),
    );
    window.set_net_codec(
        if stats.width == 0 || stats.height == 0 {
            codec_name(stats.codecid).to_string()
        } else {
            format!(
                "{} · {}×{} · {:.0} fps",
                codec_name(stats.codecid),
                stats.width,
                stats.height,
                stats.declared_fps
            )
        }
        .into(),
    );
    let ratio = (stats.buffer_s / NET_BUFFER_FULL_SCALE_S).clamp(0.0, 1.0);
    window.set_net_buffer_ratio(ratio);
    ratio
}

/// Re-publish the 画质 menu so `current` follows the decoder.
///
/// `specs` is the catalogue `player::run` installed from `PlayInfo::qualities`
/// (one entry per raw DASH stream, in the same order the menu rows went out),
/// so the row index is exactly what `quality_pick(index)` reports back.
fn publish_quality_rows(
    window: &PlayerWindow,
    specs: &[player::QualitySpec],
    current: Option<(i64, i64, u64)>,
) {
    let rows = player::quality_rows(specs, current);
    let marked = rows
        .iter()
        .find(|row| row.current)
        .map(|row| row.label.to_string())
        .unwrap_or_else(|| "-".to_string());
    crate::debug_log(&format!(
        "quality rows published n={} current={marked}",
        rows.len()
    ));
    window.set_quality_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// The stream the automatic rule selects for `qn`.
///
/// Display-only mirror of [`player::auto_track_for`]: an explicit codec
/// preference wins (highest bitrate of that codec), otherwise the **highest**
/// bitrate at that quality — the web player's 1080P AVC stream is the
/// reference, and the old lowest-bitrate rule resolved 1080P to the soft
/// ~0.58 Mbps AV1 variant. Used to mark the current row after a `qn`-only
/// quality change, where the caller never named a stream.
fn auto_stream_for(
    specs: &[player::QualitySpec],
    qn: i64,
    codec_pref: i64,
) -> Option<(i64, i64, u64)> {
    let stream = |spec: &player::QualitySpec| (spec.qn, spec.codecid, spec.bandwidth);
    if codec_pref != 0 {
        if let Some(spec) = specs
            .iter()
            .filter(|spec| spec.qn == qn && spec.codecid == codec_pref)
            .max_by_key(|spec| spec.bandwidth)
        {
            return Some(stream(spec));
        }
    }
    // 自动, or the preferred codec is missing at this quality: the sharpest
    // stream at the requested quality (same rule as the decoder).
    specs
        .iter()
        .filter(|spec| spec.qn == qn)
        .max_by_key(|spec| spec.bandwidth)
        .map(stream)
}

/// Build and drive the player window for a resolved video.
///
/// `mode` decides what happens to the players already up: [`OpenMode::Replace`]
/// closes the focused one first (card_open mode 0 / queue / detail 播放), while
/// [`OpenMode::NewWindow`] leaves them untouched and adds a cascaded window
/// (card_open mode 1).
fn open_player_inner(
    session: Arc<bilibili::Session>,
    video: bilibili::FeedVideo,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    mode: OpenMode,
) {
    let bvid = video.bvid.clone();
    let aid = video.aid as i64;
    let cid = video.cid;

    // Anchor this player's comment state to its own video. The slot is created
    // per player (NOT on the shared `PlayerState`): several player windows can be
    // open at once, and a shared slot let the newest player take over the older
    // one's comment panel. The aid comes from the feed path (`FeedVideo.aid`) or
    // the card path (`DetailState.aid` via `open_player_for`); it is logged so a
    // QA run can see which one it was.
    let comments: CommentsSlot = Arc::new(Mutex::new(CommentsState {
        aid,
        sort_hot: true,
        ..CommentsState::default()
    }));
    crate::debug_log(&format!("player comments aid={aid} bvid={bvid}"));
    // The four callbacks (`comments_load` / `comments_more` / `comments_sort` /
    // `comment_replies`) plus `comment_toggle` are registered further down, once
    // the window exists; see the "Player comments" section.

    // Replace closes exactly the focused window through its own close path
    // (loops, glass, control, queue hand-off). NewWindow touches nothing.
    let replacing = if mode == OpenMode::Replace {
        focused_session(state)
    } else {
        None
    };
    if let Some(old) = replacing.as_ref() {
        crate::debug_log(&format!(
            "card open replace closed_win={} bvid={bvid}",
            old.id
        ));
        close_player_window(
            state,
            &old.window.as_weak(),
            &Rc::downgrade(old),
            settings_store,
            "replace",
        );
    }
    let open_count = state.players.borrow().len();
    crate::debug_log(&format!(
        "player open mode={mode:?} bvid={bvid} existing={open_count}"
    ));

    let player = match PlayerWindow::new() {
        Ok(player) => player,
        Err(_) => return,
    };
    let title_for_mini = video.title.clone();
    player.global::<AppTheme>().set_dark(state.theme_dark.get());
    player.set_video_title(video.title.clone().into());
    // Left sidebar's video-info header: everything the feed row already knows
    // (the card/detail path fills what it has and leaves the rest empty).
    player.set_info_title(video.title.clone().into());
    player.set_info_owner(
        if video.owner.is_empty() {
            "UP主未知".to_string()
        } else {
            format!("UP主 · {}", video.owner)
        }
        .into(),
    );
    let stats = match (video.view, video.duration) {
        (0, 0) => String::new(),
        (view, 0) => format!("{} 次播放", format_view(view)),
        (0, duration) => format_duration(duration),
        (view, duration) => format!("{} 次播放 · {}", format_view(view), format_duration(duration)),
    };
    player.set_info_stats(stats.into());
    // Transient messages are the toast stack now; `player_status` /
    // `interaction_status` stay declared but are never written (the UI keeps
    // them inert).
    player.set_playing(true);
    player.set_position_ms(0.0);
    player.set_duration_ms(0.0);
    // The very first frame the window paints must be the buffering layer: the
    // pre-roll gate has not armed yet (playurl + decoder spawn take a moment)
    // and the tick would otherwise publish `buffering=false` over an empty
    // picture. Set synchronously, before `show()`, so no frame can leak the
    // bare glass surface; `on_danmaku_tick` keeps both values in sync after.
    player.set_buffering(true);
    player.set_buffer_progress(0.0);
    player.set_time_current(format_mmss(0).into());
    player.set_time_total(format_mmss(0).into());
    player.set_time_remaining(format!("-{}", format_mmss(0)).into());
    player.set_volume(100.0);
    player.set_volume_text("100".into());
    player.set_muted(false);
    player.set_speed_milli(1000.0);
    player.set_speed_text("1.0x".into());
    player.set_danmaku_on(true);
    player.set_danmaku_text("弹幕开".into());
    player.set_fullscreen(false);
    player.set_fullscreen_text("全屏".into());
    player.set_quality_qn(0);
    player.set_quality_label("画质".into());
    player.set_liked(false);
    player.set_like_text("点赞".into());
    player.set_coin_multiply(0);
    player.set_coin_text("投币".into());
    player.set_favorited(false);
    player.set_fav_text("收藏".into());
    player.set_topmost(false);
    player.set_topmost_text("置顶".into());
    player.set_mini_open(false);
    player.set_audio_index(0);
    player.set_audio_label("音轨".into());
    // 0 = 适配 (contain): letterbox, never stretch. Must match the Slint
    // property default in `ui/app.slint`; this explicit seed would otherwise
    // override it at open and resize would distort the picture.
    player.set_fit_mode(0);
    player.set_fit_menu_open(false);
    player.set_brightness(0);
    player.set_brightness_menu_open(false);
    player.set_audio_only(false);
    // `info_open` / `comments_open` / `panel_open` are inert since the player
    // redesign: the three `*_panel` ints below own the layout and the UI's own
    // toggle functions write them. Nothing here drives the old flags (their
    // defaults are false), so the two systems cannot fight.
    player.set_info_lines(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<SharedString>::new(),
    ))));
    player.set_sleep_text(String::new().into());
    player.set_sleep_menu_open(false);
    player.set_history_dates(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<SharedString>::new(),
    ))));
    player.set_history_status("点击「历史」获取可用日期".into());
    player.set_history_selected(String::new().into());
    player.set_send_status(String::new().into());
    player.set_danmaku_input(
        std::env::var("BILI_DANMAKU_SEED")
            .unwrap_or_default()
            .into(),
    );

    let prefs = settings_store
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    // Codec preference, captured for the 画质 row republishes below (a plain
    // copy, so the closures never hold the settings lock).
    let codec_pref = prefs.codec_pref as i64;
    // The player has its own `AppTheme` instance: mirror the two frosted-glass
    // sliders from the stored settings so its backdrop loop blurs with the SAME
    // values as the main window.
    player
        .global::<AppTheme>()
        .set_glass_transparency(prefs.glass_transparency);
    player.global::<AppTheme>().set_glass_blur(prefs.glass_blur);
    player.set_danmaku_opacity(prefs.danmaku_opacity as f32 / 100.0);
    player.set_danmaku_opacity_text(prefs.danmaku_opacity.to_string().into());
    player.set_danmaku_font(prefs.danmaku_font as i32);
    player.set_danmaku_speed(prefs.danmaku_speed as i32);
    player.set_danmaku_area(prefs.danmaku_area as i32);
    player.set_block_scroll(prefs.block_scroll);
    player.set_block_top(prefs.block_top);
    player.set_block_bottom(prefs.block_bottom);
    player.set_speed_milli(prefs.player_speed as f32);
    player.set_speed_text(format_speed(prefs.player_speed as f64 / 1000.0).into());
    player.set_key_map(key_map_model(&prefs.player_keys));

    // Frameless at the moment of presentation: strip the caption and install
    // the non-client subclass synchronously, before/around `show()`, so the
    // player is never visible decorated. If the handle is not ready before
    // `show()`, repeat on the very next statement (no timer, no event round trip).
    let player_prepared = win::prepare_frameless(player.window());
    // Restore the last window position/size BEFORE showing: the first painted
    // frame then already sits where it will stay, and the seeded backdrop can be
    // placed from that target without waiting for the HWND.
    let saved_layout = settings_store
        .lock()
        .map(|prefs| prefs.player_layout)
        .unwrap_or(None);
    // A second window is cascaded so it never lands exactly on the first.
    let cascade = if mode == OpenMode::NewWindow {
        36 * open_count as i32
    } else {
        0
    };
    let layout_target = restore_player_layout(&player, saved_layout, cascade);
    // The first painted frame must already be blurred. The main window's last
    // delivered desktop snapshot is the same whole-desktop image this player's
    // own worker will grab, so it is re-used here: image AND placement are set
    // before `show()`. Without a snapshot (or without a known rect) the window
    // stays hidden for the bounded fallback instead of painting see-through.
    let glass_seed = seed_player_backdrop(state, &player, layout_target);
    let seeded = glass_seed.is_some();
    if seeded {
        if player.show().is_err() {
            return;
        }
        if !player_prepared {
            win::prepare_frameless(player.window());
        }
    } else {
        schedule_player_show(player.as_weak(), SEED_SHOW_FALLBACK_MS);
    }
    crate::debug_log(&format!(
        "player prepare pre-show={player_prepared} glass_seed={seeded} show_deferred={}",
        !seeded
    ));
    // Smallest size the player can be dragged to: the video band's minimum
    // plus the strips that are open (none yet at this point).
    player_min_track(player.window(), 0, 0, 0);
    verify_player_layout(player.as_weak(), layout_target, 120);

    // The player is up: suspend every main-window background job that would
    // otherwise compete with playback. Cover fetches in flight finish quietly
    // (their patch is dropped in `stream_thumbs`), no new fetch starts, the
    // skeleton shimmer stops, and the frosted-glass refresh loop pauses (it
    // skips every tick while `player_active()`). Every close path decrements
    // the counter, and the main window resumes only at zero.
    player_opened();
    if let Some(main) = state.main.upgrade() {
        skeleton_halt(&main);
    }

    // Frameless: the subclass removes the non-client area entirely (no caption,
    // no native frame flash) while keeping WS_THICKFRAME for resize/snap. The
    // HWND is mapped a few frames after show(), so retry on the UI-thread timer.
    retry_subclass(&state);

    let weak = player.as_weak();
    // The comment workers publish through this weak handle (see `publish_comments`);
    // the aid/sort fields were anchored to this video above.
    if let Ok(mut guard) = comments.lock() {
        guard.ui = Some(weak.clone());
    }
    // 外扩面板 geometry, one per player window: the base size and the last
    // applied rect are per-window state, captured by this player's own
    // `panel_changed` handler below, by its danmaku tick and by the drag-end
    // layout persist.
    let panel_geom: Rc<RefCell<PanelGeom>> = Rc::new(RefCell::new(PanelGeom::default()));
    let player_drag = Rc::new(win::WindowDrag::new());
    // Same frame-locked move as the main window, plus a backdrop re-place in
    // the tick: the player's own glass loop only runs at 33ms, so without this
    // the frost visibly trailed the window while it was dragged.
    let player_drag_stats = Rc::new(DragFlushStats::new());
    let player_drag_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let drag = player_drag.clone();
        let timer = player_drag_timer.clone();
        let stats = player_drag_stats.clone();
        player.on_window_drag_start(move || {
            let Some(player) = weak.upgrade() else {
                return;
            };
            if let Err(reason) = drag.start(&player.window()) {
                crate::debug_log(&format!("drag denied reason={reason} source=player"));
                return;
            }
            stats.begin();
            crate::debug_log(&format!(
                "drag start ok=1 source={} pos={}",
                drag_source(&player.window(), "player"),
                drag_pos(&player.window()),
            ));
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_drag = drag.clone();
            let tick_timer = Rc::downgrade(&timer);
            let tick_stats = stats.clone();
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(player) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    // See the main-window tick: the real VK_LBUTTON release is
                    // the only thing that ends a drag, so a grab Slint dropped
                    // while the window moved under the cursor cannot strand
                    // the drag half-way (the user had to press again).
                    if !tick_drag.is_dragging() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&player.window()) == Some(false) {
                        crate::debug_log("drag end reason=window-left source=player");
                        tick_stats.finish("player");
                        tick_drag.end();
                        timer.stop();
                        return;
                    }
                    let moved = tick_drag.flush(&player.window());
                    if moved {
                        tick_stats.placement();
                        if let Some(glass) = player_glass_for(&player) {
                            player_glass_place(&player, &glass);
                        }
                    }
                    if !win::left_button_down() {
                        crate::debug_log(&format!(
                            "drag end reason=pointer-up deferred=0 moved={moved} source=player"
                        ));
                        tick_stats.finish("player");
                        tick_drag.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let drag = player_drag.clone();
        let stats = player_drag_stats.clone();
        player.on_window_drag_move(move || {
            stats.event();
            drag.move_to();
        });
    }
    {
        let weak = weak.clone();
        let drag = player_drag.clone();
        let timer = player_drag_timer.clone();
        let geom = panel_geom.clone();
        let store = settings_store.clone();
        let cascade = cascade;
        let stats = player_drag_stats.clone();
        player.on_window_drag_end(move || {
            if let Some(player) = weak.upgrade() {
                if !drag.end_on_signal(win::left_button_down()) {
                    crate::debug_log(&format!(
                        "drag end reason=lost-grab deferred=1 source=player {}",
                        drag_state(&player.window())
                    ));
                    return;
                }
                timer.stop();
                if drag.flush(&player.window()) {
                    stats.placement();
                    if let Some(glass) = player_glass_for(&player) {
                        player_glass_place(&player, &glass);
                    }
                }
                // Drag-end persist (task 4): the settled base rect.
                persist_player_layout(&geom, player.window(), &store, cascade, "drag-end");
            }
            timer.stop();
            drag.end();
            stats.finish("player");
            crate::debug_log("drag end reason=pointer-up deferred=0 source=player");
        });
    }
    // ---- self-driven window resize (same contract as the main window) ----
    // The player's 400ms resize-restart is driven by the display-size watch in
    // `start_player_loops`, which reads the native rect at 60Hz: it therefore
    // fires once the gestured rect settles, exactly as with a native resize.
    let player_resize = Rc::new(win::WindowResize::new());
    player_resize.set_min(480, 270);
    let player_resize_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let resize = player_resize.clone();
        let timer = player_resize_timer.clone();
        player.on_resize_start(move |edge: i32| {
            let Some(player) = weak.upgrade() else {
                return;
            };
            // Fullscreen and maximised windows have no meaningful anchored
            // edge (the picture must stay with the monitor).
            if player.window().is_fullscreen() || player.window().is_maximized() {
                return;
            }
            if let Err(reason) = resize.start(&player.window(), edge) {
                crate::debug_log(&format!(
                    "resize denied reason={reason} edge={edge} source=player"
                ));
                return;
            }
            crate::debug_log(&format!(
                "resize start edge={edge} source=player rect={}",
                resize_rect_text(&player.window())
            ));
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_resize = resize.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(player) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    if !tick_resize.is_active() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&player.window()) == Some(false) {
                        crate::debug_log("resize end reason=window-left source=player");
                        tick_resize.end();
                        timer.stop();
                        return;
                    }
                    tick_resize.update(&player.window());
                    if !win::left_button_down() {
                        crate::debug_log("resize end reason=pointer-up source=player");
                        tick_resize.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let resize = player_resize.clone();
        player.on_resize_move(move || resize.move_to());
    }
    {
        let weak = weak.clone();
        let resize = player_resize.clone();
        let timer = player_resize_timer.clone();
        let geom = panel_geom.clone();
        let store = settings_store.clone();
        player.on_resize_end(move || {
            if resize.is_active() && win::left_button_down() {
                crate::debug_log("resize end reason=lost-grab deferred=1 source=player");
                return;
            }
            if let Some(player) = weak.upgrade() {
                // Apply the last pending cursor position, then persist the
                // settled base rect. The debounce (tag=settle) writes it too;
                // this release-time write keeps the exact size even when the
                // app closes inside the 600ms settle window.
                resize.update(&player.window());
                persist_player_layout(&geom, player.window(), &store, cascade, "resize-end");
            }
            timer.stop();
            resize.end();
        });
    }
    let control = player::PlaybackControl::new();
    control.set_danmaku_font(prefs.danmaku_font);
    control.set_danmaku_speed(prefs.danmaku_speed);
    control.set_danmaku_area(prefs.danmaku_area);
    control.set_type_blocks(prefs.block_scroll, prefs.block_top, prefs.block_bottom);
    control.set_blocked_senders(prefs.blocked_senders.clone());
    control.set_block_rules(&prefs.blocked_keywords, &prefs.blocked_regex);
    control.set_hwaccel(prefs.hwaccel);
    control.set_codec_pref(prefs.codec_pref as i64);
    control.set_speed(prefs.player_speed as f64 / 1000.0);
    // Everything that belongs to exactly this window. The glass loop is created
    // now (not when the HWND appears) so the seeded image is already in place
    // and its 33ms tick can position it on the first frame. The session is
    // registered before the callbacks below, which resolve it by HWND.
    let session_rec = Rc::new(PlayerSession {
        id: next_player_session_id(),
        window: player.clone_strong(),
        control: control.clone(),
        panel_geom: panel_geom.clone(),
        hwnd: Cell::new(win::hwnd_of(player.window()).unwrap_or(0)),
        closed: Cell::new(false),
        loops: RefCell::new(None),
        glass: RefCell::new(Some(create_player_glass(&player, &control, glass_seed))),
        mini_slot: Arc::new(Mutex::new(None)),
        video: video.clone(),
        cascade,
    });
    // The window's own callbacks capture this slot, so the pointer watch lives
    // exactly as long as THIS window even if the session registry changes.
    let loops_slot: Rc<RefCell<Option<Rc<PlayerLoops>>>> = Rc::new(RefCell::new(None));
    state.players.borrow_mut().push(session_rec.clone());
    crate::debug_log(&format!(
        "player session open win={} hwnd={} players={}",
        session_rec.id,
        session_rec.hwnd.get(),
        state.players.borrow().len()
    ));
    let track = Arc::new(player::DanmakuTrack::new());
    let sleep_mode = Arc::new(AtomicU64::new(0));
    let sleep_deadline = Arc::new(AtomicU64::new(0));
    let clock = Instant::now();
    // The danmaku model is created once here and mutated in place by the tick, so
    // a frame diffs rows instead of re-creating every danmaku node each tick.
    let danmaku_model = Rc::new(VecModel::<DanmakuItem>::from(Vec::<DanmakuItem>::new()));
    player.set_danmaku_items(ModelRc::from(danmaku_model.clone()));
    // 缩放进度条 heat: one 20-cell model per player window, replaced in place by
    // `publish_scrub_heat` on a zoom gesture. A fresh window starts empty (level
    // 0), which is also the reset a "different video" needs.
    let scrub_heat = Rc::new(VecModel::<f32>::from(Vec::<f32>::new()));
    player.set_scrub_heat(ModelRc::from(scrub_heat.clone()));
    // The row-id list is per player as well. Several player windows can be open
    // at once, and a list shared through `PlayerState` let each window's tick
    // delete and re-append the other window's rows — the flicker the user saw
    // on the second video. Both the model and this list now live in the tick's
    // own closure, so a player only ever diffs its own rows.
    let danmaku_ids = Rc::new(RefCell::new(Vec::<String>::new()));
    // Set by any callback that changes what the tick renders while paused, so a
    // paused-but-edited frame still repaints (see the skip guard below).
    let refresh = Rc::new(Cell::new(true));
    let last_position = Rc::new(Cell::new(u64::MAX));
    let last_danmaku_on = Rc::new(Cell::new(true));
    // Last opacity baked into the model rows; a change repaints even while
    // paused (see the skip guard). -1.0 is never a valid opacity.
    let last_opacity = Rc::new(Cell::new(-1.0f32));
    // Gate for the once-a-second `danmaku diff` debug-log line.
    let last_diff_log = Rc::new(Cell::new(0u64));
    // 网络分析 sparkline: 64 samples, newest last, mutated in place so the
    // model is never rebuilt.
    let net_history = Rc::new(VecModel::<f32>::from(Vec::<f32>::new()));
    player.set_net_history(ModelRc::from(net_history.clone()));
    let net_last_publish = Rc::new(Cell::new(-NET_PUBLISH_MS));
    let net_last_history = Rc::new(Cell::new(-NET_HISTORY_MS));
    // Gate for the once-a-second `prefetch stats` line (per-host split).
    let net_last_split = Rc::new(Cell::new(-NET_HISTORY_MS));
    // `BILI_BUFFER_TRACE=1`: log the first 500 ms of `buffering` / position at
    // 50 ms, so the pre-show buffering state can be proven frame by frame.
    let buffer_trace = Rc::new(
        std::env::var("BILI_BUFFER_TRACE")
            .map(|value| value.trim() == "1")
            .unwrap_or(false),
    );
    let buffer_trace_at = Rc::new(Cell::new(-50i64));
    // Last `(buffering, picture)` pair published to the UI, so the QA flag can
    // log the invariant on every transition instead of only the first 500 ms.
    let buffer_state = Rc::new(Cell::new((true, false)));
    // Last `buffer_detail` string pushed, so the property is only written when
    // the measured line changed (and the change is logged as a sample).
    let buffer_detail_last = Rc::new(RefCell::new(String::new()));
    // 4 Hz gate for recomputing that line (the proxy snapshot takes a lock).
    let buffer_detail_at = Rc::new(Cell::new(-NET_PUBLISH_MS));

    player.set_danmaku_running(true);

    // The window grows/shrinks to the strips the UI reserved. The ints are the
    // only input - Rust never writes them.
    {
        let geom = panel_geom.clone();
        let window = weak.clone();
        player.on_panel_changed(move || {
            if let Some(window) = window.upgrade() {
                apply_panel_geometry(
                    &geom,
                    window.window(),
                    window.get_left_panel(),
                    window.get_right_panel(),
                    window.get_bottom_panel(),
                    false,
                );
            }
        });
    }

    // 缩放进度条: the UI derives the window (base/span) from `scrub_level` and
    // fires this after every real change. Answer with 20 densities for that
    // window from the curve the seek bar already has; level 0 clears the model.
    {
        let window = weak.clone();
        let scrub_heat = scrub_heat.clone();
        player.on_scrub_level_changed(move |level| {
            if let Some(window) = window.upgrade() {
                publish_scrub_heat(&window, &scrub_heat, level);
            }
        });
    }

    {
        let control = control.clone();
        let track = track.clone();
        let window = weak.clone();
        let state = state.clone();
        let danmaku_model = danmaku_model.clone();
        let danmaku_ids = danmaku_ids.clone();
        let sleep_mode = sleep_mode.clone();
        let sleep_deadline = sleep_deadline.clone();
        let refresh = refresh.clone();
        let last_opacity = last_opacity.clone();
        let last_diff_log = last_diff_log.clone();
        let panel_geom = panel_geom.clone();
        let net_history = net_history.clone();
        let net_last_publish = net_last_publish.clone();
        let net_last_history = net_last_history.clone();
        let net_last_split = net_last_split.clone();
        let buffer_trace = buffer_trace.clone();
        let buffer_trace_at = buffer_trace_at.clone();
        let buffer_state = buffer_state.clone();
        let bvid_log = bvid.clone();
        // Keeps this window's own pointer watch/layout debounce alive for as
        // long as this window's tick exists (dropping the timer would stop it).
        let loops_keepalive = loops_slot.clone();
        let buffer_detail_last = buffer_detail_last.clone();
        let buffer_detail_at = buffer_detail_at.clone();
        player.on_danmaku_tick(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let _ = &loops_keepalive;
            // 提示条: expire/republish the toast queue. Self-gated to ~250ms, and
            // called before the paused-frame skip guard so a paused player's
            // toasts still expire.
            toast_tick(&window);
            // 播放列表: this window is the queue's active player, so report the
            // play/pause state and watch for the end of the track (self-gated
            // to 250ms, like the toast tick).
            if let Some(host) = playlist() {
                host.tick(&window, &control);
            }
            // 外扩面板: the ints are the single source of truth. The UI's own
            // toggle functions fire `panel_changed`, and this reconciliation
            // also catches a property written directly (BILI_PANEL_TEST), so a
            // panel can never be laid out without its window growing.
            apply_panel_geometry(
                &panel_geom,
                window.window(),
                window.get_left_panel(),
                window.get_right_panel(),
                window.get_bottom_panel(),
                false,
            );
            // 网络分析: the profiler is sampled every tick but only formatted
            // at 4 Hz; one sparkline sample per second.
            let stats = control.play_stats();
            // Metered counters from the loopback proxy, `None` while it is not
            // feeding this decode session (the panel then shows the estimate).
            let prefetch_stats = control.prefetch_stats();
            // Pre-roll gate (src/player.rs): true while the pacer holds the
            // first frame. On top of that the layer must stay up while no
            // picture exists at all (between `open` and the gate arming), so
            // the first painted frame is never the bare glass surface. 仅音频
            // has no picture by design and is therefore exempt.
            let net_now = clock.elapsed().as_millis() as i64;
            let has_picture = control.audio_only() || window.get_frame().size().width > 0;
            let gate = control.buffering();
            let buffering = gate || !has_picture;
            window.set_buffering(buffering);
            window.set_buffer_progress(if gate {
                control.buffer_progress()
            } else if buffering {
                0.0
            } else {
                1.0
            });
            // The quantified line under the loading animation: measured proxy
            // throughput/read-ahead (see `PlaybackControl::buffer_detail`), only
            // while the overlay is up. Recomputed at 4 Hz (the proxy snapshot
            // takes a lock) and written on change, so a loading seek logs a
            // handful of honest samples instead of 60 identical ones.
            let mut detail = buffer_detail_last.borrow().clone();
            if buffering {
                if net_now - buffer_detail_at.get() >= 250 {
                    buffer_detail_at.set(net_now);
                    detail = control.buffer_detail();
                }
            } else {
                detail.clear();
            }
            if *buffer_detail_last.borrow() != detail {
                *buffer_detail_last.borrow_mut() = detail.clone();
                window.set_buffer_detail(detail.clone().into());
                crate::debug_log(&format!(
                    "buffer detail t_ms={net_now} buffering={buffering} seek={} text={}",
                    control.seek_buffering(),
                    if detail.is_empty() { "-" } else { detail.as_str() }
                ));
            }
            if *buffer_trace {
                trace_buffer_state(&window, &control, &buffer_trace_at, net_now);
                if buffer_state.get() != (buffering, has_picture) {
                    buffer_state.set((buffering, has_picture));
                    crate::debug_log(&format!(
                        "buffer state t_ms={net_now} buffering={buffering} picture={has_picture} position_ms={}",
                        control.playback_position_ms()
                    ));
                }
            }
            if net_now - net_last_publish.get() >= NET_PUBLISH_MS {
                net_last_publish.set(net_now);
                let ratio =
                    publish_net_stats(&window, stats.as_ref(), prefetch_stats.as_ref());
                if stats.is_some() && net_now - net_last_history.get() >= NET_HISTORY_MS {
                    net_last_history.set(net_now);
                    if net_history.row_count() >= NET_HISTORY_LEN {
                        let _ = net_history.remove(0);
                    }
                    net_history.push(ratio);
                    // The per-host split has no property of its own; it goes
                    // to the diagnostic log at the same one-per-second cadence.
                    if let Some(metered) = prefetch_stats.as_ref() {
                        if net_now - net_last_split.get() >= NET_HISTORY_MS {
                            net_last_split.set(net_now);
                            log_prefetch_split(&bvid_log, metered);
                        }
                    }
                }
            }
            let position = control.playback_position_ms();
            let paused = control.paused();
            let danmaku_on = window.get_danmaku_on();
            // Read (and clamp) the layer opacity here: the danmaku layer no
            // longer binds to it, the tick bakes it into each item's colour.
            // Comparing it below keeps a slider move repainting while paused.
            let opacity = window.get_danmaku_opacity().clamp(0.0, 1.0);
            let mode = sleep_mode.load(Ordering::SeqCst);
            // Paused with no countdown and nothing changed since the last frame:
            // every value below is identical, so skip the text writes and the
            // danmaku walk instead of repeating them 30x a second.
            if paused
                && mode == 0
                && position == last_position.get()
                && danmaku_on == last_danmaku_on.get()
                && opacity == last_opacity.get()
                && !refresh.get()
            {
                return;
            }
            last_position.set(position);
            last_danmaku_on.set(danmaku_on);
            last_opacity.set(opacity);
            refresh.set(false);
            let duration = control.duration_ms();
            window.set_duration_ms(duration as f32);
            window.set_time_total(format_mmss(duration).into());
            // While the player is buffering the clock is not moving forward,
            // so the readouts hold instead of creeping to a position that has
            // no picture behind it yet.
            if !buffering {
                window.set_position_ms(position as f32);
                window.set_time_current(format_mmss(position).into());
                window.set_time_remaining(
                    format!("-{}", format_mmss(duration.saturating_sub(position))).into(),
                );
            }
            if let Some(mini) = mini_handle(&state) {
                mini.set_playing(!paused);
            }
            if mode != 0 {
                let now_ms = clock.elapsed().as_millis() as u64;
                let fire = if mode == 5 {
                    duration > 0 && position.saturating_add(800) >= duration
                } else {
                    now_ms >= sleep_deadline.load(Ordering::SeqCst)
                };
                if fire {
                    sleep_mode.store(0, Ordering::SeqCst);
                    control.set_paused(true);
                    window.set_playing(false);
                    window.set_sleep_text("睡眠定时已到，已暂停".into());
                } else if mode == 5 {
                    window.set_sleep_text("睡眠：播完当前视频".into());
                } else {
                    let remain = sleep_deadline.load(Ordering::SeqCst).saturating_sub(now_ms);
                    window.set_sleep_text(format!("睡眠：{}", fmt_countdown(remain)).into());
                }
            }
            if !danmaku_on {
                return;
            }
            let font_px = player::danmaku_font_px(control.danmaku_font());
            let (block_scroll, block_top, block_bottom) = control.type_blocks();
            let filter = player::DanmakuFilter {
                speed_percent: control.danmaku_speed(),
                area_percent: control.danmaku_area(),
                block_scroll,
                block_top,
                block_bottom,
                blocked_senders: control.blocked_senders(),
                block_rules: control.block_rules(),
            };
            let items = track.compute(position, font_px, &filter);
            {
                // Bake the layer opacity into the colour alpha: a group opacity
                // on the danmaku layer would force an intermediate composition
                // layer every frame (see ui/app.slint). `opacity` was read and
                // clamped above.
                let items: Vec<DanmakuItem> = items
                    .into_iter()
                    .map(|mut item| {
                        item.color = item.color.with_alpha(opacity);
                        item
                    })
                    .collect();
                let model = &danmaku_model;
                let mut ids = danmaku_ids.borrow_mut();
                let mut updated = 0usize;
                let mut inserted = 0usize;
                let mut removed = 0usize;
                // Rows whose dmid left the visible set go away; iterate
                // backwards so the earlier indexes stay valid.
                let mut index = ids.len();
                while index > 0 {
                    index -= 1;
                    if !items.iter().any(|item| item.dmid.as_str() == ids[index].as_str()) {
                        model.remove(index);
                        ids.remove(index);
                        removed += 1;
                    }
                }
                // Rows still visible are updated in place (position / colour
                // only - no node is re-created); new dmids are appended. Order
                // is irrelevant: x/y are explicit item properties.
                for item in items {
                    match ids.iter().position(|id| id.as_str() == item.dmid.as_str()) {
                        Some(index) => {
                            model.set_row_data(index, item);
                            updated += 1;
                        }
                        None => {
                            ids.push(item.dmid.to_string());
                            model.push(item);
                            inserted += 1;
                        }
                    }
                }
                let now_ms = clock.elapsed().as_millis() as u64;
                if now_ms.saturating_sub(last_diff_log.get()) >= 1000 {
                    last_diff_log.set(now_ms);
                    crate::debug_log(&format!(
                        "danmaku diff bvid={} updated={updated} inserted={inserted} removed={removed} visible={}",
                        bvid_log,
                        ids.len()
                    ));
                }
            }
        });
    }

    {
        let control = control.clone();
        let window = weak.clone();
        player.on_toggle_pause(move || {
            let paused = control.toggle_paused();
            if let Some(window) = window.upgrade() {
                window.set_playing(!paused);
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_take_screenshot(move || {
            // The decoded frame is display-sized now, so its dimensions come
            // from the session that produced it — never a fixed 960x540.
            let Some((bytes, width, height)) = control.snapshot_frame() else {
                if let Some(window) = window.upgrade() {
                    toast(&window, "暂无画面可截图", TOAST_KIND_ERROR);
                }
                return;
            };
            let window = window.clone();
            std::thread::spawn(move || {
                let path = screenshot_path();
                let saved = image::RgbaImage::from_raw(width, height, bytes)
                    .map(|img| img.save(&path));
                let (message, kind) = match saved {
                    Some(Ok(())) => (
                        format!("已保存截图：{}", path.display()),
                        TOAST_KIND_SUCCESS,
                    ),
                    Some(Err(err)) => (format!("截图保存失败：{err}"), TOAST_KIND_ERROR),
                    None => ("截图数据异常".to_string(), TOAST_KIND_ERROR),
                };
                crate::debug_log(&format!("screenshot {message}"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        toast(&window, message, kind);
                    }
                });
            });
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_toggle_audio_only(move || {
            let on = control.toggle_audio_only();
            if let Some(window) = window.upgrade() {
                window.set_audio_only(on);
                toast(
                    &window,
                    if on {
                        "仅音频：已停止画面，继续播放声音"
                    } else {
                        "已恢复画面"
                    },
                    TOAST_KIND_INFO,
                );
            }
        });
    }
    {
        let sleep_mode = sleep_mode.clone();
        let sleep_deadline = sleep_deadline.clone();
        let window = weak.clone();
        player.on_set_sleep(move |minutes| {
            if minutes == 0 {
                sleep_mode.store(0, Ordering::SeqCst);
                if let Some(window) = window.upgrade() {
                    window.set_sleep_text(String::new().into());
                }
                return;
            }
            if minutes < 0 {
                sleep_mode.store(5, Ordering::SeqCst);
                if let Some(window) = window.upgrade() {
                    window.set_sleep_text("睡眠：播完当前视频".into());
                }
                return;
            }
            let ms = (minutes as u64).saturating_mul(60_000);
            sleep_deadline.store(clock.elapsed().as_millis() as u64 + ms, Ordering::SeqCst);
            sleep_mode.store(1, Ordering::SeqCst);
            if let Some(window) = window.upgrade() {
                window.set_sleep_text(format!("睡眠：{}", fmt_countdown(ms)).into());
            }
            crate::debug_log(&format!("sleep set minutes={minutes}"));
        });
    }
    // `toggle_info` used to flip the (now inert) `info_open` flag. The left
    // panel is `left_panel = 1` and is toggled by the UI itself, which calls
    // `panel_changed()`; Rust only reacts to that through `on_panel_changed`.
    {
        let control = control.clone();
        let refresh = refresh.clone();
        player.on_seek(move |position| {
            let target = position.max(0.0) as u64;
            crate::debug_log(&format!("seek requested ms={target}"));
            control.seek(target);
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_volume_changed(move |volume| {
            control.set_volume(volume as i32);
            if let Some(window) = window.upgrade() {
                window.set_volume(control.volume() as f32);
                window.set_volume_text(control.volume().to_string().into());
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_toggle_mute(move || {
            let muted = control.toggle_mute();
            if let Some(window) = window.upgrade() {
                window.set_muted(muted);
                window.set_volume_text(control.volume().to_string().into());
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_set_speed(move |milli| {
            control.set_speed(milli as f64 / 1000.0);
            if let Some(window) = window.upgrade() {
                window.set_speed_milli((control.speed() * 1000.0).round() as f32);
                window.set_speed_text(format_speed(control.speed()).into());
            }
        });
    }
    {
        let geom = panel_geom.clone();
        let window = weak.clone();
        player.on_toggle_fullscreen(move || {
            if let Some(window) = window.upgrade() {
                let next = !window.window().is_fullscreen();
                if next {
                    // Capture the base size / picture position before the
                    // window covers the monitor; panels float over the picture
                    // while fullscreen and the window is never resized.
                    enter_panel_overlay(&geom, window.window());
                }
                window.window().set_fullscreen(next);
                window.set_fullscreen(next);
                window.set_panels_overlay(next);
                window.set_fullscreen_text(if next { "退出全屏" } else { "全屏" }.into());
                if !next {
                    // Winit restores the pre-fullscreen rect asynchronously;
                    // grow back by the strips that are open now afterwards.
                    schedule_overlay_exit(geom.clone(), window.as_weak(), 0);
                }
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_set_quality(move |qn| {
            control.set_quality(qn as i64);
            if let Some(window) = window.upgrade() {
                let changed = window.get_quality_qn() != qn;
                let label = bilibili::quality_label(qn as i64);
                window.set_quality_qn(qn);
                window.set_quality_label(label.clone().into());
                // `set_quality` dropped any exact pick, so mark the row of the
                // stream the automatic rule will open for this `qn`.
                let specs = control.quality_catalog();
                publish_quality_rows(
                    &window,
                    &specs,
                    auto_stream_for(&specs, qn as i64, codec_pref),
                );
                if changed {
                    toast(&window, format!("画质：{label}"), TOAST_KIND_INFO);
                }
            }
        });
    }
    {
        // 画质 menu: the row index names one exact raw DASH stream (`qn` alone
        // is ambiguous - 1080P60 ships as AVC / HEVC / AV1 at several
        // bitrates). Selecting the stream already playing is a no-op in the
        // control, so the decoder is not restarted for nothing.
        let control = control.clone();
        let window = weak.clone();
        player.on_quality_pick(move |index| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let specs = control.quality_catalog();
            let picked = if index < 0 {
                None
            } else {
                specs.get(index as usize)
            }
            .cloned();
            let Some(spec) = picked else {
                crate::debug_log(&format!(
                    "quality pick index={index} ignored n={}",
                    specs.len()
                ));
                return;
            };
            crate::debug_log(&format!(
                "quality pick index={index} qn={} codec={} bw={}",
                spec.qn, spec.codecid, spec.bandwidth
            ));
            let changed = !window
                .get_quality_rows()
                .row_data(index as usize)
                .map(|row| row.current)
                .unwrap_or(false);
            if !changed {
                // The row the decoder is already on: no restart, no toast.
                crate::debug_log(&format!(
                    "quality pick index={index} already-current qn={}",
                    spec.qn
                ));
                return;
            }
            control.set_quality_exact(spec.qn, spec.codecid, spec.bandwidth);
            window.set_quality_qn(spec.qn as i32);
            window.set_quality_label(spec.label.clone().into());
            publish_quality_rows(
                &window,
                &specs,
                Some((spec.qn, spec.codecid, spec.bandwidth)),
            );
            toast(&window, format!("画质：{}", spec.label), TOAST_KIND_INFO);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_settings_changed(move |opacity, font| {
            let font = font.max(0) as u32;
            control.set_danmaku_font(font);
            let clamped = opacity.clamp(0.0, 1.0);
            let opacity_pct = (clamped * 100.0).round() as u32;
            if let Some(window) = window.upgrade() {
                window.set_danmaku_opacity(clamped);
                window.set_danmaku_font(font as i32);
                window.set_danmaku_opacity_text(opacity_pct.to_string().into());
            }
            update_settings(&settings_store, |prefs| {
                prefs.danmaku_opacity = opacity_pct;
                prefs.danmaku_font = font;
            });
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_speed_changed(move |speed| {
            let percent = speed.max(0) as u32;
            control.set_danmaku_speed(percent);
            if let Some(window) = window.upgrade() {
                window.set_danmaku_speed(percent as i32);
            }
            update_settings(&settings_store, |prefs| prefs.danmaku_speed = percent.clamp(50, 200));
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_area_changed(move |area| {
            let percent = area.max(0) as u32;
            control.set_danmaku_area(percent);
            if let Some(window) = window.upgrade() {
                window.set_danmaku_area(settings::clamp_area(percent) as i32);
            }
            update_settings(&settings_store, |prefs| prefs.danmaku_area = settings::clamp_area(percent));
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_danmaku_type_block_changed(move |scroll, top, bottom| {
            control.set_type_blocks(scroll, top, bottom);
            if let Some(window) = window.upgrade() {
                window.set_block_scroll(scroll);
                window.set_block_top(top);
                window.set_block_bottom(bottom);
            }
            update_settings(&settings_store, |prefs| {
                prefs.block_scroll = scroll;
                prefs.block_top = top;
                prefs.block_bottom = bottom;
            });
            refresh.set(true);
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        let settings_store = settings_store.clone();
        let refresh = refresh.clone();
        player.on_block_at(move |x, y| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let items = window.get_danmaku_items();
            let mut hash = String::new();
            for index in 0..items.row_count() {
                let Some(item) = items.row_data(index) else {
                    continue;
                };
                if x >= item.x
                    && x <= item.x + item.w
                    && y >= item.y
                    && y <= item.y + item.size + 6.0
                {
                    hash = item.mid_hash.to_string();
                    break;
                }
            }
            crate::debug_log(&format!("block_at x={x:.0} y={y:.0} hash_len={}", hash.len()));
            if hash.is_empty() {
                return;
            }
            let snapshot = update_settings(&settings_store, |prefs| {
                if !prefs.blocked_senders.iter().any(|entry| entry == &hash) {
                    prefs.blocked_senders.push(hash);
                }
            });
            control.set_blocked_senders(snapshot.blocked_senders);
            refresh.set(true);
            toast(&window, "已屏蔽该发送者的弹幕", TOAST_KIND_SUCCESS);
        });
    }
    let liked_flag = Arc::new(AtomicBool::new(false));
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let liked_flag = liked_flag.clone();
        std::thread::spawn(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            match runtime.block_on(session.like_state(&bvid)) {
                Ok(state) => {
                    let liked = state == 1;
                    crate::debug_log(&format!("like_state ok liked={liked}"));
                    liked_flag.store(liked, Ordering::SeqCst);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            window.set_liked(liked);
                            window.set_like_text(if liked { "已赞" } else { "点赞" }.into());
                        }
                    });
                }
                Err(err) => crate::debug_log(&format!("like_state err={err}")),
            }
        });
    }
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let title = video.title.clone();
        let window = weak.clone();
        let weak_main = state.main.clone();
        let download_control = state.download_control.clone();
        player.on_download_current(move || {
            let control = download::DownloadControl::new();
            if let Ok(mut guard) = download_control.lock() {
                if let Some(old) = guard.take() {
                    old.cancel();
                }
                *guard = Some(control.clone());
            }
            if let Some(ui) = weak_main.upgrade() {
                ui.set_download_busy(true);
                ui.set_download_status("正在准备下载…".into());
            }
            if let Some(ui) = window.upgrade() {
                toast(&ui, "已开始下载", TOAST_KIND_SUCCESS);
            }
            download::spawn(
                session.clone(),
                bvid.clone(),
                cid,
                title.clone(),
                weak_main.clone(),
                control,
            );
        });
    }
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let liked_flag = liked_flag.clone();
        player.on_toggle_like(move || {
            let target = !liked_flag.load(Ordering::SeqCst);
            let session = session.clone();
            let bvid = bvid.clone();
            let window = window.clone();
            let liked_flag = liked_flag.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                match runtime.block_on(session.like(&bvid, target)) {
                    Ok(()) => {
                        crate::debug_log(&format!("like set target={target} ok"));
                        liked_flag.store(target, Ordering::SeqCst);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = window.upgrade() {
                                window.set_liked(target);
                                window.set_like_text(if target { "已赞" } else { "点赞" }.into());
                            }
                        });
                    }
                    Err(err) => {
                        crate::debug_log(&format!("like set target={target} err={err:#}"));
                        let message = format!("点赞失败：{err}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(window) = window.upgrade() {
                                toast(&window, message, TOAST_KIND_ERROR);
                            }
                        });
                    }
                }
            });
        });
    }

    {
        let control = control.clone();
        let window = weak.clone();
        let state = state.clone();
        player.on_toggle_topmost(move || {
            let on = control.toggle_topmost();
            let mut applied = false;
            if let Some(window) = window.upgrade() {
                applied = win::set_topmost(window.window(), on);
                window.set_topmost(on);
                window.set_topmost_text(if on { "取消置顶" } else { "置顶" }.into());
            }
            if let Some(mini) = mini_handle(&state) {
                let _ = win::set_topmost(mini.window(), on);
            }
            if on {
                if let Some(player) = window.upgrade() {
                    ensure_topmost(true, Some(&player), &state);
                }
            }
            crate::debug_log(&format!("topmost on={on} applied={applied}"));
        });
    }
    {
        let control = control.clone();
        let state = state.clone();
        let window = weak.clone();
        let title = video.title.clone();
        let mini_slot = session_rec.mini_slot.clone();
        player.on_toggle_mini(move || {
            toggle_mini(&state, &mini_slot, &window, &control, &title);
        });
    }
    {
        let window = weak.clone();
        player.on_toggle_minimize(move || {
            if let Some(window) = window.upgrade() {
                window.window().set_minimized(true);
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_set_audio(move |index| {
            let index = index.max(0) as usize;
            control.set_audio_index(index);
            if let Some(window) = window.upgrade() {
                window.set_audio_index(index as i32);
                let items = window.get_audio_items();
                let mut label = window.get_audio_label();
                for position in 0..items.row_count() {
                    if let Some(entry) = items.row_data(position) {
                        if entry.index as usize == index {
                            label = entry.label;
                            break;
                        }
                    }
                }
                window.set_audio_label(label);
            }
        });
    }

    // Coin state on open, then the 投币/收藏/稍后再看 handlers.
    let coin_state = Arc::new(AtomicU64::new(0));
    let player_fav_ids: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let coin_state = coin_state.clone();
        std::thread::spawn(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            if let Ok(multiply) = runtime.block_on(session.coin_state(&bvid)) {
                let multiply = multiply.clamp(0, 2) as u64;
                coin_state.store(multiply, Ordering::SeqCst);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        window.set_coin_multiply(multiply as i32);
                        window.set_coin_text(
                            if multiply > 0 { "已投币" } else { "投币" }.into(),
                        );
                    }
                });
            }
        });
    }
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        let coin_state = coin_state.clone();
        player.on_set_coin(move |multiply| {
            let multiply = i64::from(multiply).clamp(1, 2);
            let session = session.clone();
            let bvid = bvid.clone();
            let window = window.clone();
            let coin_state = coin_state.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.add_coin(&bvid, multiply, false));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(()) => {
                                coin_state.store(multiply as u64, Ordering::SeqCst);
                                window.set_coin_multiply(multiply as i32);
                                window.set_coin_text("已投币".into());
                                toast(
                                    &window,
                                    format!("已投 {multiply} 枚硬币"),
                                    TOAST_KIND_SUCCESS,
                                );
                            }
                            Err(err) => {
                                crate::debug_log(&format!("add_coin err={err:#}"));
                                toast(
                                    &window,
                                    format!("投币失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            });
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let player_fav_ids = player_fav_ids.clone();
        player.on_open_fav_menu(move || {
            let session = session.clone();
            let window = window.clone();
            let player_fav_ids = player_fav_ids.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                runtime.block_on(async move {
                    let folders = session.fav_folders().await.unwrap_or_default();
                    let (favoured, _) = session.fav_state(aid).await.unwrap_or((false, 0));
                    let ids: Vec<i64> = folders.iter().map(|folder| folder.id).collect();
                    let rows: Vec<PlayerFavRow> = folders
                        .iter()
                        .map(|folder| PlayerFavRow {
                            fid: folder.id.to_string().into(),
                            title: format!("{}（{}）", folder.name, folder.count).into(),
                        })
                        .collect();
                    if let Ok(mut guard) = player_fav_ids.lock() {
                        *guard = ids;
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            window.set_fav_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                rows,
                            ))));
                            window.set_favorited(favoured);
                            window.set_fav_text(if favoured { "已收藏" } else { "收藏" }.into());
                        }
                    });
                });
            });
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_toggle_fav(move |fid| {
            let Ok(folder_id) = fid.parse::<i64>() else {
                return;
            };
            if aid <= 0 {
                if let Some(window) = window.upgrade() {
                    toast(&window, "该视频缺少 aid，无法收藏", TOAST_KIND_ERROR);
                }
                return;
            }
            let session = session.clone();
            let window = window.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.fav_deal(aid, &[folder_id], &[]));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(()) => {
                                window.set_favorited(true);
                                window.set_fav_text("已收藏".into());
                                toast(&window, "已收藏", TOAST_KIND_SUCCESS);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("fav_deal add err={err:#}"));
                                toast(
                                    &window,
                                    format!("收藏失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            });
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let player_fav_ids = player_fav_ids.clone();
        player.on_unfav_all(move || {
            if aid <= 0 {
                if let Some(window) = window.upgrade() {
                    toast(&window, "该视频缺少 aid，无法取消收藏", TOAST_KIND_ERROR);
                }
                return;
            }
            let ids = player_fav_ids
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            let session = session.clone();
            let window = window.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.fav_deal(aid, &[], &ids));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(()) => {
                                window.set_favorited(false);
                                window.set_fav_text("收藏".into());
                                toast(&window, "已取消收藏", TOAST_KIND_SUCCESS);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("fav_deal del err={err:#}"));
                                toast(
                                    &window,
                                    format!("取消收藏失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            });
        });
    }
    {
        let session = session.clone();
        let bvid = bvid.clone();
        let window = weak.clone();
        player.on_add_toview(move || {
            let session = session.clone();
            let bvid = bvid.clone();
            let window = window.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.toview_add(&bvid));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(()) => {
                                toast(&window, "已加入稍后再看", TOAST_KIND_SUCCESS);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("toview_add err={err:#}"));
                                toast(
                                    &window,
                                    format!("加入稍后再看失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    // 历史弹幕: fetch the available dates, then load one day's danmaku.
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_open_history(move || {
            let session = session.clone();
            let window = window.clone();
            if let Some(window) = window.upgrade() {
                window.set_history_status("正在获取历史弹幕日期…".into());
            }
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_history_dates(cid));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(dates) => {
                                let count = dates.len();
                                let rows: Vec<SharedString> = dates
                                    .into_iter()
                                    .map(SharedString::from)
                                    .collect();
                                window.set_history_dates(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(rows),
                                )));
                                window.set_history_status(format!("{count} 个可用日期").into());
                            }
                            Err(err) => {
                                crate::debug_log(&format!("history dates err={err:#}"));
                                window.set_history_status(format!("获取日期失败：{err}").into());
                            }
                        }
                    }
                });
            });
        });
    }
    {
        let session = session.clone();
        let window = weak.clone();
        let track = track.clone();
        let control = control.clone();
        player.on_set_history_date(move |date| {
            let date = date.to_string();
            let session = session.clone();
            let window = window.clone();
            let track = track.clone();
            let control = control.clone();
            if let Some(window) = window.upgrade() {
                window.set_history_status(format!("正在加载 {date} 的弹幕…").into());
            }
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let duration_ms = control.duration_ms();
                let result = runtime.block_on(session.danmaku_history(cid, &date));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok(history) => {
                                let count = history.items.len();
                                let (density, _, _, _) =
                                    player::compute_density(&history.items, duration_ms);
                                let rows = player::build_danmaku_rows(&history.items);
                                track.set_items(history.items);
                                window.set_density_buckets(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(density),
                                )));
                                window.set_danmaku_list(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(rows),
                                )));
                                window.set_danmaku_list_count(count as i32);
                                window.set_history_status(
                                    format!("{date}：{count} 条（{}）", history.source).into(),
                                );
                                // The bottom strip is gone, so nothing is opened
                                // here; `danmaku_list_open` stays inert. The
                                // dates/rows stay published for the settings
                                // panel's history list.
                            }
                            Err(err) => {
                                crate::debug_log(&format!("history date={date} err={err:#}"));
                                window.set_history_status(format!("加载失败：{err}").into());
                            }
                        }
                    }
                });
            });
        });
    }

    // 发送弹幕: POST the current progress with a scroll-mode white comment.
    {
        let session = session.clone();
        let window = weak.clone();
        let control = control.clone();
        let refresh = refresh.clone();
        player.on_send_danmaku(move |text| {
            let text = text.to_string();
            if text.trim().is_empty() {
                if let Some(window) = window.upgrade() {
                    window.set_send_status("请输入弹幕内容".into());
                }
                return;
            }
            refresh.set(true);
            let progress = control.playback_position_ms();
            let session = session.clone();
            let window = window.clone();
            let bvid = bvid.clone();
            if let Some(window) = window.upgrade() {
                window.set_send_status("正在发送…".into());
            }
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_post(
                    cid,
                    &bvid,
                    &text,
                    progress,
                    1,
                    25,
                    16_777_215,
                    0,
                ));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok((code, message)) => {
                                if code == 0 {
                                    window.set_send_status("发送成功 code=0".into());
                                    window.set_danmaku_input(String::new().into());
                                } else {
                                    window.set_send_status(
                                        format!("发送被拒绝：code={code} {message}").into(),
                                    );
                                }
                            }
                            Err(err) => {
                                crate::debug_log(&format!("dm/post err={err:#}"));
                                window.set_send_status(format!("发送失败：{err}").into());
                            }
                        }
                    }
                });
            });
        });
    }

    // 弹幕举报: reason 11 (其他) with a short note.
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_report_danmaku(move |dmid| {
            let dmid = dmid.to_string();
            if dmid.is_empty() {
                if let Some(window) = window.upgrade() {
                    toast(&window, "该弹幕缺少 dmid，无法举报", TOAST_KIND_ERROR);
                }
                return;
            }
            let session = session.clone();
            let window = window.clone();
            if let Some(window) = window.upgrade() {
                toast(&window, format!("正在举报 dmid={dmid}…"), TOAST_KIND_INFO);
            }
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_report(cid, &dmid, 11, "客户端举报"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = window.upgrade() {
                        match result {
                            Ok((code, message)) => {
                                crate::debug_log(&format!(
                                    "dm/report code={code} message={message}"
                                ));
                                let (text, kind) = if code == 0 {
                                    ("举报已提交".to_string(), TOAST_KIND_SUCCESS)
                                } else {
                                    (
                                        format!("举报被拒绝：code={code} {message}"),
                                        TOAST_KIND_ERROR,
                                    )
                                };
                                toast(&window, text, kind);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("dm/report err={err:#}"));
                                toast(
                                    &window,
                                    format!("举报失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    // 弹幕点赞: toggle op 1 / 2 based on the row's current state.
    {
        let session = session.clone();
        let window = weak.clone();
        player.on_thumbup_danmaku(move |dmid| {
            let dmid = dmid.to_string();
            if dmid.is_empty() {
                return;
            }
            let Some(ui) = window.upgrade() else {
                return;
            };
            let items = ui.get_danmaku_list();
            let mut thumbed = false;
            let mut index = None;
            for position in 0..items.row_count() {
                if let Some(row) = items.row_data(position) {
                    if row.dmid.as_str() == dmid.as_str() {
                        thumbed = row.thumbed;
                        index = Some(position);
                        break;
                    }
                }
            }
            let op = if thumbed { 2 } else { 1 };
            let target = !thumbed;
            toast(
                &ui,
                format!("正在{}弹幕…", if target { "赞" } else { "取消赞" }),
                TOAST_KIND_INFO,
            );
            let session = session.clone();
            let window = window.clone();
            std::thread::spawn(move || {
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    return;
                };
                let result = runtime.block_on(session.danmaku_thumbup(cid, &dmid, op));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = window.upgrade() {
                        match result {
                            Ok((code, message)) => {
                                crate::debug_log(&format!(
                                    "dm/thumbup code={code} message={message}"
                                ));
                                if code == 0 {
                                    if let Some(position) = index {
                                        let model = ui.get_danmaku_list();
                                        let mut rows: Vec<DanmakuRow> = (0..model.row_count())
                                            .filter_map(|p| model.row_data(p))
                                            .collect();
                                        if let Some(row) = rows.get_mut(position) {
                                            row.thumbed = target;
                                        }
                                        ui.set_danmaku_list(ModelRc::from(std::rc::Rc::new(
                                            VecModel::from(rows),
                                        )));
                                    }
                                }
                                let (text, kind) = if code == 0 {
                                    if target {
                                        ("弹幕点赞成功".to_string(), TOAST_KIND_SUCCESS)
                                    } else {
                                        ("已取消弹幕点赞".to_string(), TOAST_KIND_SUCCESS)
                                    }
                                } else {
                                    (
                                        format!("弹幕点赞被拒绝：code={code} {message}"),
                                        TOAST_KIND_ERROR,
                                    )
                                };
                                toast(&ui, text, kind);
                            }
                            Err(err) => {
                                crate::debug_log(&format!("dm/thumbup err={err:#}"));
                                toast(
                                    &ui,
                                    format!("弹幕点赞失败：{err}"),
                                    TOAST_KIND_ERROR,
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    {
        let state = state.clone();
        let window = weak.clone();
        let record = Rc::downgrade(&session_rec);
        let store = settings_store.clone();
        let loops_keepalive = loops_slot.clone();
        player.on_player_closed(move || {
            close_player_window(&state, &window, &record, &store, "button");
            // The window's own close path also owns its pointer watch, even if
            // the session was already unregistered.
            if let Some(loops) = loops_keepalive.borrow_mut().take() {
                stop_player_loops(&loops);
            }
        });
    }
    {
        let state = state.clone();
        let window = weak.clone();
        let record = Rc::downgrade(&session_rec);
        let store = settings_store.clone();
        let loops_keepalive = loops_slot.clone();
        player.window().on_close_requested(move || {
            close_player_window(&state, &window, &record, &store, "native");
            if let Some(loops) = loops_keepalive.borrow_mut().take() {
                stop_player_loops(&loops);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    player::set_active_control(Some(control.clone()));
    // This window's 60Hz pointer watch for the chrome (`pointer_inside`) and
    // the debounced layout persist. One per window: the loop is stored in the
    // session AND captured by this window's tick/close callbacks, so a second
    // player always gets its own watch. Stopped by this window's close path.
    let loops = start_player_loops(&state, &session_rec, &settings_store);
    *session_rec.loops.borrow_mut() = Some(loops.clone());
    *loops_slot.borrow_mut() = Some(loops);
    // QA hook (`BILI_PLAYER_CLOSE_TEST=<ms>`): close the player through the
    // REAL ✕ callback after `ms`, so the close timeline can be read from
    // `BILI_DEBUG_LOG` without synthetic input. `native` instead posts
    // `WM_CLOSE`, which drives the window's own close-request path.
    if let Ok(value) = std::env::var("BILI_PLAYER_CLOSE_TEST") {
        let value = value.trim().to_string();
        let closer = weak.clone();
        std::thread::spawn(move || {
            let (delay, native) = match value.as_str() {
                "native" => (3000u64, true),
                other => (other.parse::<u64>().unwrap_or(3000), false),
            };
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(player) = closer.upgrade() else {
                    return;
                };
                if native {
                    if let Some(hwnd) = win::hwnd_of(player.window()) {
                        crate::debug_log(&format!("player close test post WM_CLOSE hwnd={hwnd}"));
                        win::post_close(hwnd);
                    }
                } else {
                    crate::debug_log("player close test invoke ✕ callback");
                    player.invoke_player_closed();
                }
            });
        });
    }
    // QA hook: prove the outward-panel geometry numerically (no synthetic
    // input). See `panel_test_start`.
    if let Some((left, right, bottom, gap_ms)) = panel_test_spec() {
        panel_test_start(
            weak.clone(),
            panel_geom.clone(),
            left,
            right,
            bottom,
            gap_ms,
        );
    }
    // QA hook (`BILI_RESIZE_TEST=<ms>`): drive the REAL self-driven resize
    // path with explicit rects (`update_to_rect`, so it is cursor-independent
    // and never touches the pointer) while a video plays, and log
    // `fps_presented` once a second. If the OS modal resize loop were still in
    // play the pacer would stop for the whole gesture; instead the fps must
    // stay at the stream rate and the position must keep advancing.
    if let Ok(value) = std::env::var("BILI_RESIZE_TEST") {
        let duration = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(4000)
            .clamp(500, 30_000);
        run_resize_test(
            weak.clone(),
            player_resize.clone(),
            Duration::from_millis(duration),
            60,
        );
    }
    // QA hook (`BILI_TOAST_TEST=<delay_ms>`): invoke the real 截图 callback once
    // after `delay_ms` (default 2500), so the toast publish/expiry sequence can
    // be read from `BILI_DEBUG_LOG` without synthetic input.
    if let Ok(value) = std::env::var("BILI_TOAST_TEST") {
        let delay = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(2500)
            .clamp(500, 20_000);
        let weak = weak.clone();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            if let Some(player) = weak.upgrade() {
                crate::debug_log(&format!("toast-test invoke screenshot delay_ms={delay}"));
                player.invoke_take_screenshot();
            }
        });
    }
    // QA hook (`BILI_SCRUB_TEST=<delay_ms>`): select the whole-video scrub level
    // through the same property + callback the UI uses, then return to level 0
    // 1.5s later. Logs the published 20-cell heat and the clear, proving the
    // window/normalisation without synthetic input.
    if let Ok(value) = std::env::var("BILI_SCRUB_TEST") {
        let delay = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(8000)
            .clamp(500, 60_000);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let first = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(player) = first.upgrade() else {
                    return;
                };
                crate::debug_log(&format!(
                    "scrub-test open level=1 density_buckets={} duration_ms={:.0}",
                    player.get_density_buckets().row_count(),
                    player.get_duration_ms()
                ));
                player.set_scrub_level(1);
                player.invoke_scrub_level_changed(1);
            });
            std::thread::sleep(Duration::from_millis(1500));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    crate::debug_log("scrub-test close level=0");
                    player.set_scrub_level(0);
                    player.invoke_scrub_level_changed(0);
                }
            });
        });
    }
    // QA hook (`BILI_QUALITY_PICK=<index>[,<delay_ms>]`): invoke the real 画质
    // menu callback after `delay_ms` (default 12000), so an exact-stream switch
    // can be read from `BILI_DEBUG_LOG` (`quality pick …` +
    // `stream … pick=exact`) without synthetic input.
    if let Ok(value) = std::env::var("BILI_QUALITY_PICK") {
        let mut parts = value.split(',');
        let index = parts
            .next()
            .and_then(|part| part.trim().parse::<i32>().ok())
            .filter(|index| *index >= 0);
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(12_000)
            .clamp(500, 120_000);
        if let Some(index) = index {
            let weak = weak.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(player) = weak.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "quality-test invoke pick index={index} rows={}",
                        player.get_quality_rows().row_count()
                    ));
                    player.invoke_quality_pick(index);
                });
            });
        }
    }
    // QA hook (`BILI_COMMENTS_MORE_TEST=<delay_ms>[,<count>]`): call the real
    // `comments_more()` callback, so the page-ahead buffer is provable from
    // `BILI_DEBUG_LOG` (`comments prefetch …` then `comments more from
    // prefetch …`) without synthetic input.
    if let Ok(value) = std::env::var("BILI_COMMENTS_MORE_TEST") {
        let mut parts = value.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(6_000)
            .clamp(500, 120_000);
        let count = parts
            .next()
            .and_then(|part| part.trim().parse::<u32>().ok())
            .unwrap_or(2)
            .clamp(1, 10);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            for step in 0..count {
                let weak_step = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(player) = weak_step.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "comments-more-test step={step} rows_before={} loading={} end={}",
                        player.get_comment_items().row_count(),
                        player.get_comments_loading(),
                        player.get_comments_end()
                    ));
                    player.invoke_comments_more();
                });
                std::thread::sleep(Duration::from_millis(600));
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    crate::debug_log(&format!(
                        "comments-more-test done rows={} loading={} end={}",
                        player.get_comment_items().row_count(),
                        player.get_comments_loading(),
                        player.get_comments_end()
                    ));
                }
            });
        });
    }
    // QA hook (`BILI_PANEL_GLASS_TEST=<delay_ms>[,<hold_ms>]`): open the right
    // panel, enter fullscreen through the REAL toggle, hold, then leave again.
    // The `panel glass source=video|desktop` lines plus the logged properties
    // prove the panels sample the video frame in overlay mode and the desktop
    // snapshot otherwise - no synthetic input.
    if let Ok(value) = std::env::var("BILI_PANEL_GLASS_TEST") {
        let mut parts = value.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(8_000)
            .clamp(2_000, 60_000);
        let hold = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(4_000)
            .clamp(1_000, 60_000);
        let weak = weak.clone();
        let control = control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        player.set_right_panel(1);
                        player.set_right_tab(2);
                        player.invoke_panel_changed();
                        log_panel_glass(&player, "panel-open");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(900));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        player.invoke_toggle_fullscreen();
                        log_panel_glass(&player, "fullscreen");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(1_600));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        log_panel_glass(&player, "overlay");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(1_500 + hold));
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(player) = weak.upgrade() {
                        player.invoke_toggle_fullscreen();
                        log_panel_glass(&player, "exit");
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(1_600));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    log_panel_glass(&player, "windowed");
                    crate::debug_log(&format!(
                        "panel-glass video frames={}",
                        control.video_glass_generation()
                    ));
                }
            });
        });
    }
    // Diagnostic hook (mirrors BILI_AUTOPLAY / BILI_MINI): seek to BILI_SEEK_TEST
    // once playback is ready, logging the position before and after, so a seek
    // landing can be read from BILI_DEBUG_LOG without synthetic input.
    // `<seconds>` is an absolute offset; `p<N>` seeks to N% of the duration
    // (always inside the track, so a deep seek works on any test video).
    if let Ok(spec) = std::env::var("BILI_SEEK_TEST") {
        let spec = spec.trim().to_string();
        if !spec.is_empty() {
            let control = control.clone();
            std::thread::spawn(move || {
                for _ in 0..120 {
                    if control.duration_ms() > 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                std::thread::sleep(Duration::from_secs(15));
                let before = control.playback_position_ms();
                let duration = control.duration_ms();
                let target = if let Some(percent) = spec.strip_prefix('p') {
                    let percent = percent.parse::<u64>().unwrap_or(95).clamp(1, 99);
                    duration.saturating_mul(percent) / 100
                } else {
                    spec.parse::<u64>().unwrap_or(0).saturating_mul(1000)
                };
                let target = if duration > 0 {
                    target.min(duration.saturating_sub(1_000))
                } else {
                    target
                };
                crate::debug_log(&format!(
                    "seek-test spec={spec} before_ms={before} target_ms={target} duration_ms={duration}"
                ));
                control.seek(target);
                std::thread::sleep(Duration::from_millis(1_500));
                let after = control.playback_position_ms();
                crate::debug_log(&format!(
                    "seek-test after_ms={after} target_ms={target} delta_ms={}",
                    after as i64 - target as i64
                ));
            });
        }
    }
    // Diagnostic hook (mirrors BILI_AUTOPLAY): open the mini PiP immediately so
    // its frameless frame style can be read back from the backend.
    if std::env::var("BILI_MINI").map(|value| value == "1").unwrap_or(false) {
        toggle_mini(
            &state,
            &session_rec.mini_slot,
            &weak,
            &control,
            &title_for_mini,
        );
    }
    // ---- 评论区 (left sidebar) ----
    // The exact registration the old TODO asked for: the data layer's four entry
    // points plus the UI's row-expand (Rust owns `expanded`, so a republish keeps
    // the thread open). All of them clone `session`, which `player::spawn` takes
    // right below. The two comment callbacks carry a row INDEX (never an id):
    // the index -> rpid map is the one `publish_comments` wrote alongside the
    // model, so a rpid wider than 32 bits survives the round trip.
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comments_load(move || {
            on_comments_load(session.clone(), comments.clone(), aid, true);
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comments_more(move || {
            on_comments_more(session.clone(), comments.clone());
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comments_sort(move |hot| {
            on_comments_sort(session.clone(), comments.clone(), hot);
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comment_replies(move |row, pn| {
            on_comment_replies(
                session.clone(),
                comments.clone(),
                row,
                pn as i64,
            );
        });
    }
    {
        let session = session.clone();
        let comments = comments.clone();
        player.on_comment_toggle(move |row| {
            on_comment_toggle(session.clone(), comments.clone(), row);
        });
    }

    // Auto-load the first page when the player opens (the panel opens onto a
    // list that is already filling, and a replay does not depend on the user
    // touching the 💬 button first). Logged so a QA run can see it fired once.
    if aid != 0 {
        crate::debug_log(&format!("comments auto-load aid={aid}"));
        on_comments_load(
            session.clone(),
            comments.clone(),
            aid,
            true,
        );
    } else {
        crate::debug_log("comments auto-load skipped aid=0");
    }

    // QA hook (`BILI_COMMENT_TOGGLE_TEST=<delay_ms>[,<row>[,<hold_ms>]]`): drive
    // the REAL row-index contract on the open player - open the 评论 tab, toggle
    // a row, log the row's true rpid and the published row, hold the thread open
    // for the capture, then toggle it closed again. The row the UI would send is
    // an index; the id is only ever resolved here, so a rpid past 2^31
    // round-trips intact.
    //
    // `row` omitted = the first row with a 楼中楼 (best chance of being on
    // screen for the shot); `row` = -1 = the first row with MORE than one page
    // (so 查看更多回复 is exercised too). `hold_ms` keeps the thread open long
    // enough for PrintWindow.
    if let Ok(spec) = std::env::var("BILI_COMMENT_TOGGLE_TEST") {
        let mut parts = spec.split(',');
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(9_000)
            .clamp(500, 120_000);
        let wanted_row = parts.next().and_then(|part| part.trim().parse::<i32>().ok());
        let hold = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(12_000)
            .clamp(500, 120_000);
        let session = session.clone();
        let comments = comments.clone();
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let row = match wanted_row {
                Some(row) if row >= 0 => Some(row),
                _ => {
                    let long_thread = wanted_row == Some(-1);
                    let mut chosen = None;
                    for _ in 0..400 {
                        if let Ok(guard) = comments.lock() {
                            if !guard.rows.is_empty() {
                                chosen = if long_thread {
                                    guard
                                        .rows
                                        .iter()
                                        .position(|comment| comment.rcount > 20)
                                        .map(|index| index as i32)
                                } else {
                                    // The busiest thread among the first three
                                    // rows, i.e. one that stays on screen for
                                    // the capture.
                                    guard
                                        .rows
                                        .iter()
                                        .take(3)
                                        .enumerate()
                                        .filter(|(_, comment)| comment.rcount > 0)
                                        .max_by_key(|(_, comment)| comment.rcount)
                                        .map(|(index, _)| index as i32)
                                        .or_else(|| {
                                            guard
                                                .rows
                                                .iter()
                                                .position(|comment| comment.rcount > 0)
                                                .map(|index| index as i32)
                                        })
                                };
                                break;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    chosen
                }
            };
            let Some(row) = row else {
                crate::debug_log("comment-toggle-test no-row-with-replies");
                return;
            };
            // 2 = right panel open on the 评论 tab (`changed right_panel` maps
            // it), so the open thread is on screen for PrintWindow.
            let weak_open = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak_open.upgrade() {
                    player.set_right_panel(2);
                    player.set_right_tab(1);
                    // `changed right_panel` only remaps the tab; the window
                    // grows in Rust's `panel_changed` handler.
                    player.invoke_panel_changed();
                    crate::debug_log(&format!(
                        "comment-toggle-test panel open right_panel={} right_tab={}",
                        player.get_right_panel(),
                        player.get_right_tab()
                    ));
                }
            });
            std::thread::sleep(Duration::from_millis(700));
            let root_rpid = comment_row_rpid(&comments, row);
            if let Some(rpid) = root_rpid {
                crate::debug_log(&format!(
                    "comment-toggle-test row={row} rpid={rpid} fits_i32={}",
                    i32::try_from(rpid).is_ok()
                ));
            }
            on_comment_toggle(session.clone(), comments.clone(), row);
            std::thread::sleep(Duration::from_millis(2_500));
            log_published_comment_row(&weak, row, "open");
            // 查看更多回复 goes through `on_comment_replies(row, pn)` - the same
            // row index, the next page of the SAME parent thread.
            if let Some(root_rpid) = root_rpid {
                let next_page = match comments.lock() {
                    Ok(guard) => {
                        let loaded = guard.replies.get(&root_rpid).map(Vec::len).unwrap_or(0);
                        guard
                            .reply_more
                            .get(&root_rpid)
                            .copied()
                            .unwrap_or(false)
                            .then(|| (loaded / 20 + 1) as i64)
                    }
                    Err(_) => None,
                };
                if let Some(pn) = next_page {
                    crate::debug_log(&format!(
                        "comment-toggle-test more row={row} rpid={root_rpid} pn={pn}"
                    ));
                    on_comment_replies(session.clone(), comments.clone(), row, pn);
                    std::thread::sleep(Duration::from_millis(2_000));
                    log_published_comment_row(&weak, row, "more");
                }
            }
            std::thread::sleep(Duration::from_millis(hold));
            on_comment_toggle(session, comments, row);
            std::thread::sleep(Duration::from_millis(900));
            log_published_comment_row(&weak, row, "closed");
        });
    }

    // The info header's cover: one thumbnail fetch, patched in when it lands.
    if !video.pic.is_empty() {
        let session = session.clone();
        let weak = weak.clone();
        let pic = video.pic.clone();
        std::thread::spawn(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            let Ok(pixels) = runtime.block_on(session.fetch_thumb(&pic)) else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(player) = weak.upgrade() {
                    player.set_info_cover(to_thumb(&Some(pixels)));
                    player.set_info_cover_ready(true);
                }
            });
        });
    }

    let mini_slot = session_rec.mini_slot.clone();
    // 播放列表: this window is now the app's active player. Runs before the
    // decode thread starts so the bar shows the title/cover immediately and the
    // queue's own tick can recognize the window.
    if let Some(host) = playlist() {
        host.on_open(&player, &video);
    } else {
        set_active_player(&player);
    }
    player::spawn(session, video, weak, mini_slot, control, track);
    // The new window takes focus: card_open(0) replaces IT, and a later click on
    // an older window moves focus there (`poll_player_focus`).
    focus_player(state, &session_rec, false);
    maybe_start_window_probe(state);
}

// ---------------------------------------------------------------------------
// Player comments (data + 评论区 sidebar)
// ---------------------------------------------------------------------------
//
// `open_player_inner` resets `PlayerState.comments` to the open video's aid,
// stores the window handle the workers publish through and registers the four
// UI callbacks (`comments_load` / `comments_more` / `comments_sort` /
// `comment_replies`) plus `comment_toggle`, the panel's row expand. The first
// page is auto-loaded there (once, at player open).
//
// Every worker stores into `CommentsState`, then calls `publish_comments`: the
// rows are turned into `[CommentRow]` on the UI thread (avatars from the pixel
// cache; `Image` is not `Send`) and patched into the panel's model in place.
// Avatars then stream in four at a time and patch their own row by `rpid`.
//
// The pinned comment (`guard.top`) is fetched but not rendered yet: the list
// starts at the first 20 normal comments (see the report's deferred list).

/// Load the first page of `aid`'s comments, replacing any current state.
///
/// Entry point for `on_comments_load`.
fn on_comments_load(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    aid: i64,
    sort_hot: bool,
) {
    if aid == 0 {
        return;
    }
    if let Ok(mut guard) = comments.lock() {
        // The window handle survives a reload (a sort change or 评论 refresh
        // must keep publishing to the same player).
        let ui = guard.ui.clone();
        *guard = CommentsState {
            aid,
            sort_hot,
            ui,
            ..CommentsState::default()
        };
    }
    comments_fetch(session, comments, aid, sort_hot, String::new(), false);
}

/// Continue the current list from its cursor. Entry point for
/// `on_comments_more`; a no-op while a page is in flight or the list has ended.
///
/// The page fetched ahead of the UI is consumed first: appending it needs no
/// network and shows no loading skeleton, so a scroll is instant. Only when no
/// buffer matches the current cursor does this fall back to a real fetch.
fn on_comments_more(session: Arc<bilibili::Session>, comments: CommentsSlot) {
    let buffered = match comments.lock() {
        Ok(mut guard) => {
            let cursor = guard.cursor.clone();
            let aid = guard.aid;
            let sort_hot = guard.sort_hot;
            match guard.prefetch.take() {
                Some(buffer)
                    if buffer.aid == aid
                        && buffer.sort_hot == sort_hot
                        && buffer.cursor == cursor =>
                {
                    Some(buffer)
                }
                other => {
                    // Stale buffer (another cursor / video / sort): keep it
                    // only if it still could be used, otherwise drop it.
                    guard.prefetch = other;
                    None
                }
            }
        }
        Err(_) => return,
    };
    if let Some(buffer) = buffered {
        let rows = buffer.page.comments.len();
        let absorbed = match comments.lock() {
            Ok(mut guard) => {
                comments_absorb(&mut guard, buffer.aid, buffer.sort_hot, buffer.page, true)
            }
            Err(_) => None,
        };
        if let Some((cursor, is_end)) = absorbed {
            crate::debug_log(&format!(
                "comments more from prefetch n={rows} next={} is_end={is_end}",
                !cursor.is_empty()
            ));
            publish_comments(&comments);
            comments_prefetch_next(session, comments);
            return;
        }
    }
    let (aid, sort_hot, cursor, is_end, loading) = match comments.lock() {
        Ok(guard) => (
            guard.aid,
            guard.sort_hot,
            guard.cursor.clone(),
            guard.is_end,
            guard.loading,
        ),
        Err(_) => return,
    };
    if aid == 0 || is_end || loading || cursor.is_empty() {
        return;
    }
    comments_fetch(session, comments, aid, sort_hot, cursor, true);
}

/// Switch between 热门 (`sort_hot = true`) and 按时间 (`false`) and reload the
/// first page. Entry point for `on_comments_sort`.
fn on_comments_sort(session: Arc<bilibili::Session>, comments: CommentsSlot, sort_hot: bool) {
    let aid = match comments.lock() {
        Ok(guard) => guard.aid,
        Err(_) => return,
    };
    if aid == 0 {
        return;
    }
    on_comments_load(session, comments, aid, sort_hot);
}

/// Root `rpid` for a row index the UI sent back, from the map published with
/// the current model. `None` covers a negative / out-of-range row (a click that
/// was queued while the list was replaced by a sort change or another video).
fn comment_row_rpid(comments: &CommentsSlot, row: i32) -> Option<i64> {
    let row = usize::try_from(row).ok()?;
    let guard = comments.lock().ok()?;
    guard.row_rpids.get(row).copied().filter(|rpid| *rpid > 0)
}

/// Fetch one comment's dialog. Entry point for `on_comment_replies`; `row` is
/// the 楼中楼's parent row in `comment_items` and `pn` is 1-based. The row is
/// resolved to its root `rpid` here, then the reply rows are cached under that
/// rpid - so a reply keeps its own rpid for later 查看更多回复 pages.
fn on_comment_replies(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    row: i32,
    pn: i64,
) {
    let Some(root_rpid) = comment_row_rpid(&comments, row) else {
        crate::debug_log(&format!(
            "comment replies row={row} pn={pn} ignored=out-of-range"
        ));
        return;
    };
    crate::debug_log(&format!(
        "comment replies row={row} rpid={root_rpid} pn={pn}"
    ));
    comment_replies_fetch(session, comments, root_rpid, pn);
}

/// Toggle one root comment's 楼中楼. Entry point for the panel's row click;
/// `row` indexes `comment_items` and is resolved to the root `rpid` before the
/// thread state is touched.
///
/// `expanded` lives in [`CommentsState`], not in the model row, so a republish
/// (load more, sort change, avatar patch) keeps every thread the user opened
/// open. Opening a thread fetches page 1 when its dialog is not cached yet.
fn on_comment_toggle(session: Arc<bilibili::Session>, comments: CommentsSlot, row: i32) {
    let Some(root_rpid) = comment_row_rpid(&comments, row) else {
        crate::debug_log(&format!("comment toggle row={row} ignored=out-of-range"));
        return;
    };
    let (fetch, expanded) = match comments.lock() {
        Ok(mut guard) => {
            if guard.expanded.remove(&root_rpid) {
                (false, false)
            } else {
                guard.expanded.insert(root_rpid);
                (!guard.replies.contains_key(&root_rpid), true)
            }
        }
        Err(_) => return,
    };
    crate::debug_log(&format!(
        "comment toggle row={row} rpid={root_rpid} expanded={expanded} fetch={fetch}"
    ));
    publish_comments(&comments);
    if fetch {
        comment_replies_fetch(session, comments, root_rpid, 1);
    }
}

/// Push the current comment state to the open player's 评论区.
///
/// The model build runs inside `invoke_from_event_loop` (`Image` is not `Send`),
/// so this is safe from any thread; a closed window or the headless
/// `BILI_COMMENTS_TEST` path (no `ui` handle) is a no-op.
fn publish_comments(comments: &CommentsSlot) {
    let ui = match comments.lock() {
        Ok(guard) => guard.ui.clone(),
        Err(_) => None,
    };
    let Some(ui) = ui else {
        return;
    };
    let comments = comments.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = ui.upgrade() else {
            return;
        };
        let (items, row_rpids, loading, error, total, is_end, sort_hot) = {
            let Ok(guard) = comments.lock() else {
                return;
            };
            let items: Vec<CommentRow> = guard
                .rows
                .iter()
                .map(|row| build_comment_row(row, &guard))
                .collect();
            // Same snapshot as `items`, so index i in the model is rpid
            // `row_rpids[i]` for every callback that arrives about this list.
            let row_rpids: Vec<i64> = guard.rows.iter().map(|row| row.rpid).collect();
            (
                items,
                row_rpids,
                guard.loading,
                guard.error.clone(),
                guard.total,
                guard.is_end,
                guard.sort_hot,
            )
        };
        // Publish the mapping just before the model that uses it; both writes
        // are on the UI thread, so a click can never see one without the other.
        if let Ok(mut guard) = comments.lock() {
            guard.row_rpids = row_rpids.clone();
        }
        // What the row-chip state ended up as after this republish (the QA
        // proof that ``expanded`` reached the model, including for a row whose
        // rpid does not fit in 32 bits).
        for (index, item) in items.iter().enumerate() {
            if item.expanded {
                crate::debug_log(&format!(
                    "comment publish row={index} rpid={} replies={} total={} expanded=1",
                    row_rpids.get(index).copied().unwrap_or(0),
                    item.replies.row_count(),
                    item.replies_text,
                ));
            }
        }
        sync_comment_model(&window, items);
        window.set_comments_loading(loading);
        window.set_comments_error(error.into());
        window.set_comments_total(to_i32(total));
        window.set_comments_end(is_end);
        window.set_comments_hot(sort_hot);
    });
}

/// Update the panel's model in place when the list only grew or changed state
/// (load more, expand, avatar), so the ListView keeps its scroll position; a
/// head that no longer matches (sort change / other video) replaces the model.
fn sync_comment_model(window: &PlayerWindow, items: Vec<CommentRow>) {
    let model = window.get_comment_items();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<CommentRow>>() else {
        window.set_comment_items(ModelRc::from(Rc::new(VecModel::from(items))));
        return;
    };
    let same_head = vec.row_count() > 0
        && items.first().map(|row| row.rpid) == vec.row_data(0).map(|row| row.rpid);
    if same_head && items.len() >= vec.row_count() {
        for (index, item) in items.into_iter().enumerate() {
            if index < vec.row_count() {
                vec.set_row_data(index, item);
            } else {
                vec.push(item);
            }
        }
        return;
    }
    vec.set_vec(items);
}

/// Log one published row as the UI sees it (`BILI_COMMENT_TOGGLE_TEST`): the
/// model row, its reply count, the chip text and Rust's `expanded` flag. `label`
/// is `open` / `closed`, so the log shows the chip state AFTER the republish.
fn log_published_comment_row(weak: &Weak<PlayerWindow>, row: i32, label: &str) {
    let weak = weak.clone();
    let label = label.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(player) = weak.upgrade() else {
            return;
        };
        let model = player.get_comment_items();
        let item = usize::try_from(row).ok().and_then(|index| model.row_data(index));
        match item {
            Some(item) => {
                let chip = if item.expanded {
                    "收起回复".to_string()
                } else {
                    item.replies_text.to_string()
                };
                crate::debug_log(&format!(
                    "comment-toggle-test {label} row={row} replies={} total={} expanded={} chip={chip}",
                    item.replies.row_count(),
                    item.replies_text,
                    u8::from(item.expanded),
                ))
            }
            None => crate::debug_log(&format!(
                "comment-toggle-test {label} row={row} model_rows={} missing",
                model.row_count()
            )),
        }
    });
}

/// One panel row: the comment plus the thread state (avatar from the pixel
/// cache, expanded set, fetched dialog) the UI needs.
fn build_comment_row(row: &bilibili::Comment, state: &CommentsState) -> CommentRow {
    let cached = state.replies.get(&row.rpid);
    let expanded = state.expanded.contains(&row.rpid);
    let replies: Vec<CommentReply> = match (expanded, cached) {
        (true, Some(rows)) => rows
            .iter()
            .map(|reply| build_reply_row(reply, state))
            .collect(),
        _ => Vec::new(),
    };
    let loaded = cached.map(Vec::len).unwrap_or(0);
    CommentRow {
        rpid: row.rpid as i32,
        uname: row.user.uname.clone().into(),
        message: row.message.clone().into(),
        like_text: like_label(row.like),
        time_text: time_ago(row.ctime).into(),
        replies_text: if row.rcount > 0 {
            format!("{} 条回复", row.rcount)
        } else {
            String::new()
        }
        .into(),
        initial: initial_of(&row.user.uname).into(),
        has_avatar: !row.user.avatar.is_empty() && state.avatars.contains_key(&row.user.avatar),
        avatar: avatar_image(state.avatars.get(&row.user.avatar)),
        expanded,
        next_pn: (loaded / 20 + 1) as i32,
        has_more: expanded && state.reply_more.get(&row.rpid).copied().unwrap_or(false),
        replies: ModelRc::from(Rc::new(VecModel::from(replies))),
    }
}

/// A 楼中楼 row: the flat reply shape (`CommentReply`), which carries no
/// nesting and no 查看更多回复 link of its own.
fn build_reply_row(row: &bilibili::Comment, state: &CommentsState) -> CommentReply {
    CommentReply {
        rpid: row.rpid as i32,
        uname: row.user.uname.clone().into(),
        message: row.message.clone().into(),
        like_text: if row.like > 0 {
            format!("{} 赞", format_view(row.like))
        } else {
            String::new()
        }
        .into(),
        time_text: time_ago(row.ctime).into(),
        initial: initial_of(&row.user.uname).into(),
        has_avatar: !row.user.avatar.is_empty() && state.avatars.contains_key(&row.user.avatar),
        avatar: avatar_image(state.avatars.get(&row.user.avatar)),
    }
}

/// A like count for the meta line (`""` when nobody liked it yet).
fn like_label(count: u64) -> SharedString {
    if count == 0 {
        "赞".into()
    } else {
        format!("{} 赞", format_view(count)).into()
    }
}

/// Placeholder glyph for an avatar that has not arrived: the first character
/// of the user name, which is what the panel shows instead of an empty disc.
fn initial_of(name: &str) -> String {
    name.chars()
        .next()
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Raw RGBA -> `Image` for the avatar slot (`Image::default()` when missing).
fn avatar_image(pixels: Option<&(Vec<u8>, u32, u32)>) -> Image {
    match pixels {
        Some((bytes, width, height)) => {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
            buffer.make_mut_bytes().copy_from_slice(bytes);
            Image::from_rgba8(buffer)
        }
        None => Image::default(),
    }
}

/// Root rows whose avatar is neither cached nor in flight yet.
fn pending_comment_avatars(comments: &CommentsSlot) -> Vec<(i64, String)> {
    let Ok(guard) = comments.lock() else {
        return Vec::new();
    };
    let mut out: Vec<(i64, String)> = Vec::new();
    for row in &guard.rows {
        let url = &row.user.avatar;
        if url.is_empty() || guard.avatars.contains_key(url) {
            continue;
        }
        if !out.iter().any(|(_, pending)| pending == url) {
            out.push((row.rpid, url.clone()));
        }
    }
    out
}

/// Fetch avatars four at a time and patch each row as it lands, so the list
/// fills progressively instead of waiting for the slowest picture.
fn spawn_comment_avatars(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    pending: Vec<(i64, String)>,
) {
    if pending.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let mut stream = futures::stream::iter(pending.into_iter().map(|(rpid, url)| {
            let session = session.clone();
            async move {
                let pixels = session.fetch_avatar(&url).await.ok();
                (rpid, url, pixels)
            }
        }))
        .buffer_unordered(4);
        while let Some((rpid, url, pixels)) = runtime.block_on(stream.next()) {
            let Some(pixels) = pixels else {
                continue;
            };
            if let Ok(mut guard) = comments.lock() {
                guard.avatars.insert(url, pixels.clone());
            }
            let ui = match comments.lock() {
                Ok(guard) => guard.ui.clone(),
                Err(_) => None,
            };
            let Some(ui) = ui else {
                continue;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = ui.upgrade() {
                    patch_comment_avatar(&window, rpid, &pixels);
                }
            });
        }
    });
}

/// Replace one avatar in the rendered model (root rows and their dialogs),
/// leaving every text/state field untouched.
fn patch_comment_avatar(window: &PlayerWindow, rpid: i64, pixels: &(Vec<u8>, u32, u32)) {
    let image = avatar_image(Some(pixels));
    let model = window.get_comment_items();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<CommentRow>>() else {
        return;
    };
    for index in 0..vec.row_count() {
        let Some(mut row) = vec.row_data(index) else {
            continue;
        };
        let mut changed = false;
        if row.rpid == rpid as i32 && !row.has_avatar {
            row.avatar = image.clone();
            row.has_avatar = true;
            changed = true;
        }
        if let Some(replies) = row
            .replies
            .as_any()
            .downcast_ref::<VecModel<CommentReply>>()
        {
            for reply_index in 0..replies.row_count() {
                let Some(mut reply) = replies.row_data(reply_index) else {
                    continue;
                };
                if reply.rpid == rpid as i32 && !reply.has_avatar {
                    reply.avatar = image.clone();
                    reply.has_avatar = true;
                    replies.set_row_data(reply_index, reply);
                }
            }
        }
        if changed {
            vec.set_row_data(index, row);
        }
    }
}

/// Fold one landed page into the state.
///
/// Returns the new `(cursor, is_end)` when the page belonged to the current
/// list, and `None` when a newer load (sort change / other video) owns the slot
/// and the caller must drop it.
fn comments_absorb(
    state: &mut CommentsState,
    aid: i64,
    sort_hot: bool,
    page: bilibili::CommentPage,
    append: bool,
) -> Option<(String, bool)> {
    if state.aid != aid || state.sort_hot != sort_hot {
        return None;
    }
    let bilibili::CommentPage {
        top,
        comments,
        cursor,
        is_end,
        total,
    } = page;
    state.cursor = cursor;
    state.is_end = is_end;
    state.total = total;
    if append {
        state.rows.extend(comments);
    } else {
        state.top = top;
        state.rows = comments;
        state.replies.clear();
        state.reply_more.clear();
        state.expanded.clear();
    }
    state.loading = false;
    Some((state.cursor.clone(), state.is_end))
}

/// Fetch the page after the current cursor into the slot, so the next
/// `comments_more()` appends it without a round trip.
///
/// Never fetches past `is_end`, never duplicates an in-flight request, and
/// keeps at most one buffered page. A page that lands after the cursor moved
/// (another video / sort / a real `more`) is dropped.
fn comments_prefetch_next(session: Arc<bilibili::Session>, comments: CommentsSlot) {
    let (aid, sort_hot, cursor) = match comments.lock() {
        Ok(mut guard) => {
            if guard.aid == 0
                || guard.is_end
                || guard.cursor.is_empty()
                || guard.prefetch.is_some()
                || guard.prefetching
            {
                return;
            }
            guard.prefetching = true;
            (guard.aid, guard.sort_hot, guard.cursor.clone())
        }
        Err(_) => return,
    };
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            if let Ok(mut guard) = comments.lock() {
                guard.prefetching = false;
            }
            return;
        };
        let result = runtime.block_on(session.comments(aid, sort_hot, &cursor));
        match result {
            Ok(page) => {
                crate::debug_log(&format!(
                    "comments prefetch aid={aid} sort={} n={} next={} is_end={} total={}",
                    if sort_hot { "hot" } else { "time" },
                    page.comments.len(),
                    !page.cursor.is_empty(),
                    page.is_end,
                    page.total
                ));
                if let Ok(mut guard) = comments.lock() {
                    guard.prefetching = false;
                    // Only buffer a page that still belongs to this cursor.
                    if guard.aid == aid
                        && guard.sort_hot == sort_hot
                        && guard.cursor == cursor
                        && !guard.is_end
                    {
                        guard.prefetch = Some(CommentsPrefetch {
                            aid,
                            sort_hot,
                            cursor,
                            page,
                        });
                    }
                }
            }
            Err(err) => {
                crate::debug_log(&format!(
                    "comments prefetch error aid={aid} sort={} err={err}",
                    if sort_hot { "hot" } else { "time" }
                ));
                if let Ok(mut guard) = comments.lock() {
                    guard.prefetching = false;
                }
            }
        }
    });
}

/// Fetch one page of comments and store it in `comments`.
///
/// `append` continues the list (评论加载更多); otherwise the page replaces it
/// (first load / sort change). Runs on its own thread over the shared runtime,
/// like every other list worker, and fetches the page after it into the buffer
/// as soon as this one lands.
fn comments_fetch(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    aid: i64,
    sort_hot: bool,
    cursor: String,
    append: bool,
) {
    if let Ok(mut guard) = comments.lock() {
        guard.loading = true;
        guard.error.clear();
    }
    // Show the loading bar immediately (the worker may take a second).
    publish_comments(&comments);
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            if let Ok(mut guard) = comments.lock() {
                guard.loading = false;
            }
            publish_comments(&comments);
            return;
        };
        match runtime.block_on(session.comments(aid, sort_hot, &cursor)) {
            Ok(page) => {
                crate::debug_log(&format!(
                    "comments loaded aid={aid} sort={} append={append} n={} next={} is_end={} total={}",
                    if sort_hot { "hot" } else { "time" },
                    page.comments.len(),
                    !page.cursor.is_empty(),
                    page.is_end,
                    page.total
                ));
                // A newer load (sort change / other video) wins.
                let absorbed = match comments.lock() {
                    Ok(mut guard) => comments_absorb(&mut guard, aid, sort_hot, page, append),
                    Err(_) => None,
                };
                if absorbed.is_none() {
                    return;
                }
                // Publish the rows, then stream their avatars in.
                publish_comments(&comments);
                let pending = pending_comment_avatars(&comments);
                spawn_comment_avatars(session.clone(), comments.clone(), pending);
                // One page ahead, so the next scroll never waits on the network.
                comments_prefetch_next(session.clone(), comments.clone());
            }
            Err(err) => {
                if let Ok(mut guard) = comments.lock() {
                    guard.loading = false;
                    guard.error = format!("{err}");
                    crate::debug_log(&format!("comments error aid={aid} err={}", guard.error));
                }
                // The panel shows this real error text under the list.
                publish_comments(&comments);
            }
        }
    });
}

/// Fetch one comment's dialog and cache its rows under `root_rpid`.
fn comment_replies_fetch(
    session: Arc<bilibili::Session>,
    comments: CommentsSlot,
    root_rpid: i64,
    pn: i64,
) {
    let aid = match comments.lock() {
        Ok(guard) => guard.aid,
        Err(_) => return,
    };
    if aid == 0 || root_rpid == 0 {
        return;
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        match runtime.block_on(session.comment_replies(aid, root_rpid, pn)) {
            Ok(page) => {
                crate::debug_log(&format!(
                    "comment replies loaded aid={aid} root={root_rpid} pn={pn} n={} total={} page={} size={} has_more={}",
                    page.replies.len(),
                    page.total,
                    page.page,
                    page.size,
                    page.has_more
                ));
                // `has_more` must be read before the rows move into the cache.
                let has_more = page.has_more && !page.replies.is_empty();
                if let Ok(mut guard) = comments.lock() {
                    if guard.aid != aid {
                        return;
                    }
                    let entry = guard.replies.entry(root_rpid).or_default();
                    if pn <= 1 {
                        *entry = page.replies;
                    } else {
                        // 查看更多回复: append the next page, skipping any row
                        // the cache already holds.
                        for row in page.replies {
                            if !entry.iter().any(|existing| existing.rpid == row.rpid) {
                                entry.push(row);
                            }
                        }
                    }
                    guard.reply_more.insert(root_rpid, has_more);
                }
                // Publish the thread, then stream its avatars in.
                publish_comments(&comments);
                let pending: Vec<(i64, String)> = match comments.lock() {
                    Ok(guard) => guard
                        .replies
                        .get(&root_rpid)
                        .map(|rows| {
                            rows.iter()
                                .filter(|row| {
                                    !row.user.avatar.is_empty()
                                        && !guard.avatars.contains_key(&row.user.avatar)
                                })
                                .map(|row| (row.rpid, row.user.avatar.clone()))
                                .collect()
                        })
                        .unwrap_or_default(),
                    Err(_) => Vec::new(),
                };
                spawn_comment_avatars(session.clone(), comments.clone(), pending);
            }
            Err(err) => {
                crate::debug_log(&format!(
                    "comment replies error aid={aid} root={root_rpid} pn={pn} err={err}"
                ));
            }
        }
    });
}

/// `BILI_COMMENTS_TEST=<aid>` driver: exercises the four handlers above without
/// a player window, then logs a compact summary so the data path can be checked
/// from `BILI_DEBUG_LOG`.
fn run_comments_test(session: Arc<bilibili::Session>, comments: CommentsSlot, aid: i64) {
    on_comments_load(session.clone(), comments.clone(), aid, true);
    if !wait_comments_idle(&comments, 240) {
        crate::debug_log(&format!("comments aid={aid} load_timeout"));
        return;
    }
    let (n, next, is_end, first_rpid, preview, total, top) = match comments.lock() {
        Ok(guard) => (
            guard.rows.len(),
            !guard.cursor.is_empty(),
            guard.is_end,
            guard.rows.first().map(|row| row.rpid).unwrap_or(0),
            guard.rows.first().map(|row| row.replies.len()).unwrap_or(0),
            guard.total,
            guard.top.is_some(),
        ),
        Err(_) => return,
    };
    crate::debug_log(&format!(
        "comments aid={aid} n={n} next={next} is_end={is_end} first_rpid={first_rpid} replies={preview} total={total} top={top}"
    ));

    // One comment's parsed fields, so every field of the domain shape is proven
    // against real data (not just the counts).
    if let Ok(guard) = comments.lock() {
        if let Some(first) = guard.rows.first() {
            crate::debug_log(&format!(
                "comments first aid={aid} rpid={} mid={} uname={} like={} ctime={} rcount={} avatar={} msg_len={}",
                first.rpid,
                first.user.mid,
                first.user.uname,
                first.like,
                first.ctime,
                first.rcount,
                first.user.avatar,
                first.message.chars().count()
            ));
        }
    }

    // The cursor round-trips on a second page, when there is one.
    if !is_end {
        on_comments_more(session.clone(), comments.clone());
        if wait_comments_idle(&comments, 240) {
            if let Ok(guard) = comments.lock() {
                crate::debug_log(&format!(
                    "comments more aid={aid} n={} next={} is_end={}",
                    guard.rows.len(),
                    !guard.cursor.is_empty(),
                    guard.is_end
                ));
            }
        }
    }

    // One comment's dialog, then its second page (查看更多回复). The dialog is
    // keyed by the root rpid, so the headless driver calls the fetch layer
    // directly; the UI's row-index entry point is exercised on the player
    // window by `BILI_COMMENT_TOGGLE_TEST`.
    if first_rpid != 0 {
        comment_replies_fetch(session.clone(), comments.clone(), first_rpid, 1);
        if wait_replies(&comments, first_rpid, 240) {
            let page1 = cached_replies(&comments, first_rpid);
            crate::debug_log(&format!(
                "comments aid={aid} root={first_rpid} replies={page1}"
            ));
            if page1 > 0 {
                comment_replies_fetch(session.clone(), comments.clone(), first_rpid, 2);
                if wait_replies_at_least(&comments, first_rpid, page1 + 1, 240) {
                    crate::debug_log(&format!(
                        "comments replies-more aid={aid} root={first_rpid} replies={}",
                        cached_replies(&comments, first_rpid)
                    ));
                }
            }
        }
    }

    // 按时间 (mode 2), so both sort modes are exercised.
    on_comments_sort(session.clone(), comments.clone(), false);
    if wait_comments_idle(&comments, 240) {
        if let Ok(guard) = comments.lock() {
            crate::debug_log(&format!(
                "comments sort=time aid={aid} n={} is_end={}",
                guard.rows.len(),
                guard.is_end
            ));
        }
    }
}

/// Bounded wait for the current comments page to land (`loading` clears).
fn wait_comments_idle(comments: &CommentsSlot, tries: u32) -> bool {
    for _ in 0..tries {
        match comments.lock() {
            Ok(guard) if !guard.loading => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Bounded wait for one comment's dialog to be cached.
fn wait_replies(comments: &CommentsSlot, root_rpid: i64, tries: u32) -> bool {
    for _ in 0..tries {
        if let Ok(guard) = comments.lock() {
            if guard.replies.contains_key(&root_rpid) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Rows cached for one comment's dialog (`0` when none).
fn cached_replies(comments: &CommentsSlot, root_rpid: i64) -> usize {
    comments
        .lock()
        .map(|guard| guard.replies.get(&root_rpid).map(Vec::len).unwrap_or(0))
        .unwrap_or(0)
}

/// Bounded wait for a comment's dialog cache to reach `min` rows.
fn wait_replies_at_least(comments: &CommentsSlot, root_rpid: i64, min: usize, tries: u32) -> bool {
    for _ in 0..tries {
        if cached_replies(comments, root_rpid) >= min {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Show/hide the mini (PiP) window. Playback never stops here: the mini shares
/// the running decode thread, which mirrors frames into it through `slot`.
/// Closing the mini only clears that slot.
///
/// `slot` is the slot of the player window that owns the button, so a second
/// open player's decode thread can never paint this mini.
fn toggle_mini(
    state: &Rc<PlayerState>,
    slot: &player::MiniSlot,
    main: &Weak<PlayerWindow>,
    control: &Arc<player::PlaybackControl>,
    title: &str,
) {
    let owner = main
        .upgrade()
        .and_then(|player| win::hwnd_of(player.window()))
        .unwrap_or(0);
    if let Some(mini) = mini_handle(state) {
        if mini.window().is_visible() {
            mini.hide().ok();
            if let Ok(mut slot) = slot.lock() {
                *slot = None;
            }
            state.mini_owner.set(0);
            if let Some(main) = main.upgrade() {
                main.set_mini_open(false);
            }
            crate::debug_log("mini hidden");
            return;
        }
        let mini_prepared = win::prepare_frameless(mini.window());
        mini.show().ok();
        if !mini_prepared {
            win::prepare_frameless(mini.window());
        }
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(mini.as_weak());
        }
        state.mini_owner.set(owner);
        if let Some(main) = main.upgrade() {
            main.set_mini_open(true);
        }
        if control.topmost() {
            let _ = win::set_topmost(mini.window(), true);
        }
        crate::debug_log("mini shown again");
        return;
    }

    let mini = match MiniPlayerWindow::new() {
        Ok(mini) => mini,
        Err(err) => {
            crate::debug_log(&format!("mini create err={err}"));
            if let Some(main) = main.upgrade() {
                toast(&main, "小窗创建失败", TOAST_KIND_ERROR);
            }
            return;
        }
    };
    mini.global::<AppTheme>().set_dark(state.theme_dark.get());
    mini.set_video_title(format!("小窗 · {title}").into());
    mini.set_playing(!control.paused());
    if let Some(main) = main.upgrade() {
        mini.set_frame(main.get_frame());
        main.set_mini_open(true);
    }

    let weak = mini.as_weak();
    let mini_drag = Rc::new(win::WindowDrag::new());
    // The mini drags with the same frame-locked path as the two main windows,
    // so it needs its own 16ms tick (see the main-window wiring).
    let mini_drag_timer = Rc::new(slint::Timer::default());
    {
        let mini_weak = weak.clone();
        let drag = mini_drag.clone();
        let timer = mini_drag_timer.clone();
        mini.on_window_drag_start(move || {
            let Some(mini) = mini_weak.upgrade() else {
                return;
            };
            if let Err(reason) = drag.start(&mini.window()) {
                crate::debug_log(&format!("drag denied reason={reason} source=mini"));
                return;
            }
            crate::debug_log(&format!(
                "drag start ok=1 source={} pos={}",
                drag_source(&mini.window(), "mini"),
                drag_pos(&mini.window()),
            ));
            if timer.running() {
                return;
            }
            let tick_weak = mini_weak.clone();
            let tick_drag = drag.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(mini) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    if !tick_drag.is_dragging() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&mini.window()) == Some(false) {
                        crate::debug_log("drag end reason=window-left source=mini");
                        tick_drag.end();
                        timer.stop();
                        return;
                    }
                    tick_drag.flush(&mini.window());
                    if !win::left_button_down() {
                        crate::debug_log("drag end reason=pointer-up deferred=0 source=mini");
                        tick_drag.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let drag = mini_drag.clone();
        mini.on_window_drag_move(move || drag.move_to());
    }
    {
        let mini_weak = weak.clone();
        let drag = mini_drag.clone();
        let timer = mini_drag_timer.clone();
        mini.on_window_drag_end(move || {
            if let Some(mini) = mini_weak.upgrade() {
                if !drag.end_on_signal(win::left_button_down()) {
                    crate::debug_log(&format!(
                        "drag end reason=lost-grab deferred=1 source=mini {}",
                        drag_state(&mini.window())
                    ));
                    return;
                }
                timer.stop();
                drag.flush(&mini.window());
            }
            timer.stop();
            drag.end();
            crate::debug_log("drag end reason=pointer-up deferred=0 source=mini");
        });
    }
    // ---- self-driven window resize (same contract as the player) ----
    let mini_resize = Rc::new(win::WindowResize::new());
    mini_resize.set_min(320, 200);
    let mini_resize_timer = Rc::new(slint::Timer::default());
    {
        let mini_weak = weak.clone();
        let resize = mini_resize.clone();
        let timer = mini_resize_timer.clone();
        mini.on_resize_start(move |edge: i32| {
            let Some(mini) = mini_weak.upgrade() else {
                return;
            };
            if mini.window().is_maximized() {
                return;
            }
            if let Err(reason) = resize.start(&mini.window(), edge) {
                crate::debug_log(&format!(
                    "resize denied reason={reason} edge={edge} source=mini"
                ));
                return;
            }
            crate::debug_log(&format!(
                "resize start edge={edge} source=mini rect={}",
                resize_rect_text(&mini.window())
            ));
            if timer.running() {
                return;
            }
            let tick_weak = mini_weak.clone();
            let tick_resize = resize.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(
                slint::TimerMode::Repeated,
                DRAG_FRAME,
                move || {
                    let Some(timer) = tick_timer.upgrade() else {
                        return;
                    };
                    let Some(mini) = tick_weak.upgrade() else {
                        timer.stop();
                        return;
                    };
                    if !tick_resize.is_active() {
                        timer.stop();
                        return;
                    }
                    if win::is_visible(&mini.window()) == Some(false) {
                        crate::debug_log("resize end reason=window-left source=mini");
                        tick_resize.end();
                        timer.stop();
                        return;
                    }
                    tick_resize.update(&mini.window());
                    if !win::left_button_down() {
                        crate::debug_log("resize end reason=pointer-up source=mini");
                        tick_resize.end();
                        timer.stop();
                    }
                },
            );
        });
    }
    {
        let resize = mini_resize.clone();
        mini.on_resize_move(move || resize.move_to());
    }
    {
        let mini_weak = weak.clone();
        let resize = mini_resize.clone();
        let timer = mini_resize_timer.clone();
        mini.on_resize_end(move || {
            if resize.is_active() && win::left_button_down() {
                crate::debug_log("resize end reason=lost-grab deferred=1 source=mini");
                return;
            }
            if let Some(mini) = mini_weak.upgrade() {
                resize.update(&mini.window());
            }
            timer.stop();
            resize.end();
        });
    }
    {
        let control = control.clone();
        let main = main.clone();
        let mini_weak = weak.clone();
        mini.on_mini_toggle_pause(move || {
            let paused = control.toggle_paused();
            if let Some(mini) = mini_weak.upgrade() {
                mini.set_playing(!paused);
            }
            if let Some(main) = main.upgrade() {
                main.set_playing(!paused);
            }
        });
    }
    {
        let state = state.clone();
        let main = main.clone();
        let mini_weak = weak.clone();
        let slot = slot.clone();
        mini.on_mini_close(move || {
            if let Some(mini) = mini_weak.upgrade() {
                mini.hide().ok();
            }
            if let Ok(mut held) = slot.lock() {
                *held = None;
            }
            state.mini_owner.set(0);
            if let Some(main) = main.upgrade() {
                main.set_mini_open(false);
            }
            crate::debug_log("mini closed by button");
        });
    }
    {
        let mini_weak = weak.clone();
        mini.on_mini_minimize(move || {
            if let Some(mini) = mini_weak.upgrade() {
                mini.window().set_minimized(true);
            }
        });
    }
    {
        let state = state.clone();
        let main = main.clone();
        let slot = slot.clone();
        mini.window().on_close_requested(move || {
            if let Ok(mut held) = slot.lock() {
                *held = None;
            }
            state.mini_owner.set(0);
            if let Some(main) = main.upgrade() {
                main.set_mini_open(false);
            }
            crate::debug_log("mini closed by window");
            slint::CloseRequestResponse::HideWindow
        });
    }

    let mini_prepared = win::prepare_frameless(mini.window());
    if mini.show().is_err() {
        crate::debug_log("mini show failed");
        if let Some(main) = main.upgrade() {
            toast(&main, "小窗显示失败", TOAST_KIND_ERROR);
            main.set_mini_open(false);
        }
        return;
    }
    if !mini_prepared {
        win::prepare_frameless(mini.window());
    }
    // Frameless mini window: the same subclass removes the non-client area.
    retry_subclass(&state);
    if let Ok(mut held) = slot.lock() {
        *held = Some(weak);
    }
    state.mini_owner.set(owner);
    if control.topmost() {
        let _ = win::set_topmost(mini.window(), true);
    }
    *state.mini.borrow_mut() = Some(mini);
    crate::debug_log("mini opened");
}

/// Supersede any in-flight attempt and start a login on a freshly bumped
/// generation. `saved` selects the saved-account flow, otherwise QR login.
fn start_login(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    saved: bool,
) {
    let my = generation.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        ui.set_logged_in(false);
        ui.set_qr_image(Image::default());
        ui.set_login_state(1);
        ui.set_login_step("准备中…".into());
        ui.set_login_percent(0);
        ui.set_login_progress(0.0);
        ui.set_login_failed(false);
        ui.set_login_error("".into());
        ui.set_login_countdown("".into());
    }
    spawn_attempt(
        weak,
        generation,
        session_slot,
        feed_store,
        feed_ring,
        settings_store,
        my,
        saved,
    );
}

/// Spawn a worker thread owning its own current-thread tokio runtime.
fn spawn_attempt(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    my: u64,
    auto: bool,
) {
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            let message = "初始化网络运行时失败".to_string();
            post(&weak, &generation, my, move |ui| ui.set_status(message.into()));
            return;
        };

        runtime.block_on(async move {
            if let Err(err) = run_attempt(
                weak.clone(),
                generation.clone(),
                session_slot,
                feed_store,
                feed_ring,
                settings_store,
                my,
                auto,
            )
            .await
            {
                let message = format!("发生错误：{err}");
                post(&weak, &generation, my, move |ui| ui.set_status(message.into()));
            }
        });
    });
}

async fn run_attempt(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    my: u64,
    auto: bool,
) -> Result<()> {
    if auto {
        post_step(&weak, &generation, my, "read-account", "读取本地账号", 10);
        let account = match accounts::load() {
            Some(account) => Some(account),
            None => accounts::migrate_from_token().await,
        };
        let Some(account) = account else {
            post_login_choice(&weak, &generation, my, "没有已保存的账号，请扫码登录");
            return Ok(());
        };
        post_step(
            &weak,
            &generation,
            my,
            "fingerprint",
            "获取设备指纹（buvid3/buvid4/b_nut/bili_ticket）",
            25,
        );
        let session = match bilibili::Session::from_token(&accounts::to_token(&account)).await {
            Ok(session) => Arc::new(session),
            Err(err) => {
                post_login_choice(
                    &weak,
                    &generation,
                    my,
                    &format!("无法恢复已保存的登录（{err}），请重新登录"),
                );
                return Ok(());
            }
        };
        post_step(&weak, &generation, my, "nav", "校验登录状态（nav）", 45);
        match session.nav().await {
            Ok(bilibili::NavOutcome::LoggedIn(user)) => {
                post_step(&weak, &generation, my, "account-info", "获取账号信息", 65);
                publish_identity(&weak, &generation, my, &user, "restore");
                publish_session(&session_slot, session.clone());
                show_user(&weak, &generation, my, &session, user).await;
                post_step(&weak, &generation, my, "feed", "加载首页推荐", 90);
                show_home_after_login(
                    &weak,
                    &generation,
                    my,
                    &session_slot,
                    &feed_store,
                    &feed_ring,
                    session.clone(),
                    feed_prefetch_capacity(&settings_store),
                )
                .await;
                post_step(&weak, &generation, my, "done", "完成", 100);
                return Ok(());
            }
            Ok(bilibili::NavOutcome::NotLoggedIn { code, message }) => {
                post_login_choice(
                    &weak,
                    &generation,
                    my,
                    &format!("已保存账号登录失效（code={code} {message}），请重新登录"),
                );
            }
            Err(err) => {
                post_login_choice(
                    &weak,
                    &generation,
                    my,
                    &format!("校验登录状态失败（{err}），请重新登录"),
                );
            }
        }
        return Ok(());
    }

    // ---- QR login --------------------------------------------------------
    post_step(
        &weak,
        &generation,
        my,
        "fingerprint",
        "获取设备指纹（buvid3/buvid4/b_nut/bili_ticket）",
        10,
    );
    let session = Arc::new(bilibili::Session::new().await?);
    post_step(&weak, &generation, my, "qr-gen", "请求二维码", 25);

    let qr = session.qr_generate().await?;
    let (pixels, width, height) = build_qr_rgba(&qr.url)?;
    let qr_side = width as f32;

    post(&weak, &generation, my, move |ui| {
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
        buffer.make_mut_bytes().copy_from_slice(&pixels);
        ui.set_qr_image(Image::from_rgba8(buffer));
        ui.set_qr_size(qr_side);
        ui.set_logged_in(false);
    });
    // Waiting-for-scan: QR centered + progress 25% -> 70% + ~180s countdown.
    post_login_qr(
        &weak,
        &generation,
        my,
        "等待扫码…",
        25,
        &format!(
            "二维码有效期约 {} 秒",
            MAX_POLLS as u64 * POLL_INTERVAL.as_secs()
        ),
    );

    let mut last_state: i64 = -1;
    let mut success: Option<bilibili::PollResult> = None;

    for poll_index in 0..MAX_POLLS {
        if generation.load(Ordering::SeqCst) != my {
            return Ok(()); // superseded
        }

        let poll = match session.qr_poll(&qr.key).await {
            Ok(p) => p,
            // Transient network hiccup: keep polling.
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        match poll.state_code {
            0 => {
                success = Some(poll);
                break;
            }
            86038 => {
                post_login_fail(
                    &weak,
                    &generation,
                    my,
                    "waiting",
                    "等待扫码",
                    70,
                    "二维码已过期（code=86038），请点击「刷新二维码」",
                );
                return Ok(());
            }
            86090 => {
                if last_state != 86090 {
                    last_state = 86090;
                    post_step(&weak, &generation, my, "scanned", "已扫码，等待手机确认", 75);
                }
            }
            _ => {
                last_state = poll.state_code;
                let frac = (poll_index + 1) as f32 / MAX_POLLS as f32;
                let percent = 25 + (frac * 45.0) as i32;
                let remaining = (MAX_POLLS - poll_index) as u64 * POLL_INTERVAL.as_secs();
                post_login_qr(
                    &weak,
                    &generation,
                    my,
                    "等待扫码…",
                    percent,
                    &format!("二维码有效期剩余约 {remaining} 秒"),
                );
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let poll = match success {
        Some(p) => p,
        None => {
            post_login_fail(
                &weak,
                &generation,
                my,
                "waiting",
                "等待扫码",
                70,
                "二维码超时，请点击「刷新二维码」",
            );
            return Ok(());
        }
    };

    post_step(&weak, &generation, my, "confirmed", "手机已确认，正在校验", 80);

    // ---- SSO cross-domain jump (best-effort; may set extra cookies) ------
    if !poll.cross_url.is_empty() {
        let _ = session.visit(&poll.cross_url).await;
    }

    // ---- account info (nav required, the rest best-effort) ---------------
    let user = match session.nav().await? {
        bilibili::NavOutcome::LoggedIn(user) => user,
        bilibili::NavOutcome::NotLoggedIn { code, message } => {
            post_login_fail(
                &weak,
                &generation,
                my,
                "nav",
                "获取账号信息（nav）",
                95,
                &format!("登录校验失败：code={code} {message}"),
            );
            return Ok(());
        }
    };

    // ---- build the account and persist it --------------------------------
    post_step(&weak, &generation, my, "save", "保存登录凭证", 88);
    publish_identity(&weak, &generation, my, &user, "login");
    let account = accounts::Account {
        mid: user.mid,
        uname: user.uname.clone(),
        face: user.face.clone(),
        cookies: poll.cookies,
        refresh_token: poll.refresh_token,
        added_at: accounts::now_secs(),
        vip: user.vip_status,
    };
    if let Err(err) = accounts::save(&account) {
        post_status(
            &weak,
            &generation,
            my,
            &format!("提示：登录成功但保存账号失败：{err}"),
        );
    }
    publish_session(&session_slot, session.clone());

    post_step(&weak, &generation, my, "nav", "获取账号信息（nav）", 95);
    show_user(&weak, &generation, my, &session, user).await;
    post_step(&weak, &generation, my, "feed", "加载首页推荐", 100);
    show_home_after_login(
        &weak,
        &generation,
        my,
        &session_slot,
        &feed_store,
        &feed_ring,
        session.clone(),
        feed_prefetch_capacity(&settings_store),
    )
    .await;
    Ok(())
}

/// Snapshot the single session so a background worker can own it.
fn active_session(session_slot: &SessionSlot) -> Option<Arc<bilibili::Session>> {
    session_slot
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned())
}

#[derive(Clone, Copy)]
enum VipAction {
    Experience,
    Receive,
    Sign,
}

/// Load the current 大会员 state (`privilege/my`) without blocking the UI thread.
fn vip_refresh(weak: Weak<AppWindow>, session_slot: SessionSlot) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_vip_busy(false);
            ui.set_vip_status_text("未登录".into());
        }
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_vip_busy(true);
        ui.set_vip_status_text("加载中…".into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let result = runtime.block_on(session.vip_privilege_my());
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_vip_busy(false);
                match result {
                    Ok(vip) => {
                        let text = if vip.message.is_empty() || vip.message == "0" {
                            format!("code={}", vip.code)
                        } else {
                            format!("code={} {}", vip.code, vip.message)
                        };
                        ui.set_vip_status_text(text.into());
                        ui.set_vip_privilege_text(vip.summary.into());
                    }
                    Err(err) => ui.set_vip_status_text(format!("获取失败：{err}").into()),
                }
            }
        });
    });
}

/// Run one 会员中心 write action and surface the returned API code.
fn vip_action(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    action: VipAction,
) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_vip_action_status("未登录".into());
        }
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_vip_busy(true);
        ui.set_vip_action_status("请求中…".into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let result = runtime.block_on(async move {
            match action {
                VipAction::Experience => session.vip_experience_add().await,
                VipAction::Receive => session.vip_privilege_receive().await,
                VipAction::Sign => session.vip_task_sign().await,
            }
        });
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_vip_busy(false);
                match result {
                    Ok(vip) => {
                        let text = if vip.message.is_empty() {
                            format!("code={}", vip.code)
                        } else {
                            format!("code={} {}", vip.code, vip.message)
                        };
                        ui.set_vip_action_status(text.into());
                    }
                    Err(err) => ui.set_vip_action_status(format!("失败：{err}").into()),
                }
            }
        });
    });
}

/// Publish the freshly built session to the shared slot.
fn publish_session(session_slot: &SessionSlot, session: Arc<bilibili::Session>) {
    if let Ok(mut guard) = session_slot.lock() {
        *guard = Some(session);
    }
}

/// Publish the top-left identity strip (nickname + numeric mid) as soon as the
/// account is known.
///
/// Called right after a successful `nav()` for both a saved-account restore and
/// a fresh QR login, before the avatar/relation fetches in [`show_user`], so the
/// strip is filled before `show_home_after_login` reveals the home view and never
/// shows an empty name.
fn publish_identity(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    user: &bilibili::NavUser,
    source: &str,
) {
    let uname = user.uname.clone();
    let mid = user.mid.to_string();
    crate::debug_log(&format!("identity user={uname} mid={mid} source={source}"));
    post(weak, generation, my, move |ui| {
        ui.set_user_name(uname.into());
        ui.set_user_id(mid.into());
    });
}

/// Fetch best-effort extras and push the full account panel to the UI.
async fn show_user(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session: &bilibili::Session,
    user: bilibili::NavUser,
) {
    let relation = session.relation(user.mid).await.ok();
    let _ = session.myinfo().await; // best-effort; result intentionally unused
    let avatar = if user.face.is_empty() {
        None
    } else {
        session.fetch_avatar(&user.face).await.ok()
    };

    let uname = user.uname.clone();
    let mid = user.mid.to_string();
    let level = format!("Lv{}", user.level);
    let coins = format!("{:.1}", user.money);
    let moral = format!("{}", user.moral as i64);
    let vip = user.vip_text.clone();
    let vip_active = user.vip_status;
    let (following, follower) = match &relation {
        Some(r) => (r.following.to_string(), r.follower.to_string()),
        None => ("—".to_string(), "—".to_string()),
    };

    post(weak, generation, my, move |ui| {
        ui.set_user_name(uname.clone().into());
        ui.set_user_id(mid.clone().into());
        ui.set_p_uname(uname.into());
        ui.set_p_mid(mid.into());
        ui.set_p_level(level.into());
        ui.set_p_coins(coins.into());
        ui.set_p_moral(moral.into());
        ui.set_p_vip(vip.into());
        ui.set_p_vip_active(vip_active);
        ui.set_p_following(following.into());
        ui.set_p_follower(follower.into());

        if let Some((bytes, w, h)) = avatar {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
            buffer.make_mut_bytes().copy_from_slice(&bytes);
            ui.set_avatar(Image::from_rgba8(buffer));
        }

        // The home view is revealed by `show_home_after_login`, once a feed
        // batch is ready (or after at most `FEED_HOME_WAIT`), so the user does
        // not land on an empty page. Account fields above are already set.
        ui.set_status("已登录".into());
        update_page_bar(ui);
    });
}

#[derive(Default)]
struct SearchState {
    keyword: String,
    tab: i32,
    page: i64,
    /// Page count the API last reported (the bar's `N / M` is derived from the
    /// result count and the page size, capped by this).
    num_pages: i64,
    /// Owner mids of the rows currently on screen, aligned with `search_bvids`
    /// (the 用户 tab carries mids and no bvids). A card's UP name resolves
    /// through this, never through another view's list.
    mids: Vec<i64>,
}

struct DetailState {
    bvid: String,
    title: String,
    cid: u64,
    aid: i64,
    parts: Vec<u64>,
}

struct SearchRowData {
    pixels: Option<(Vec<u8>, u32, u32)>,
    title: String,
    meta: String,
    sub: String,
    badge: String,
}

struct SearchOutcome {
    page: i64,
    num_pages: i64,
    num_results: i64,
    rows: Vec<(String, SearchRowData)>,
    bvids: Vec<String>,
    /// Owner mids aligned with `rows`; the 用户 tab fills it with every row.
    mids: Vec<i64>,
}

#[derive(Default)]
struct FolderState {
    media_id: i64,
    title: String,
    page: i64,
    has_more: bool,
    /// Owner mids of the folder page's rows, aligned with `folder_bvids`.
    mids: Vec<i64>,
}

#[derive(Default)]
struct HistoryPageState {
    max: i64,
    view_at: i64,
    business: String,
    has_more: bool,
    /// Owner mids of the loaded history rows, aligned with `history_bvids`
    /// (append loads extend both in step).
    mids: Vec<i64>,
}

/// The UP page on screen: whose 投稿 it is, its header, and which order was
/// last published.
#[derive(Default)]
struct UpPageState {
    mid: i64,
    name: String,
    /// The header avatar URL from `user_card`. `up_avatar_text` stays `""` for
    /// now (the UI shows the name only), so this is kept for the next step and
    /// reported as a presence flag in the log.
    avatar: String,
    fans: u64,
    /// The 投稿 count from `user_card`.
    videos: u64,
    /// The sort of the page last PUBLISHED (not merely requested): 0 = 播放最多,
    /// 1 = 最新发布. `None` while a load is in flight; the `BILI_UP_PAGE_TEST`
    /// hook polls it.
    published: Option<i32>,
}

#[derive(Default)]
struct RegionState {
    rid: i64,
    name: String,
    page: i64,
    has_more: bool,
}

#[derive(Default)]
struct PopularState {
    page: i64,
    has_more: bool,
}

#[derive(Default)]
struct SpaceState {
    mid: i64,
    page: i64,
    has_more: bool,
    attribute: i64,
}

#[derive(Default)]
struct RelationState {
    kind: i64,
    page: i64,
    has_more: bool,
}

struct DynamicRowData {
    id: String,
    author: String,
    face: Option<(Vec<u8>, u32, u32)>,
    time: String,
    text: String,
    has_cover: bool,
    cover: Option<(Vec<u8>, u32, u32)>,
    title: String,
    meta: String,
    liked: bool,
    like_text: String,
    stats: String,
}

struct MsgRowData {
    nickname: String,
    avatar: Option<(Vec<u8>, u32, u32)>,
    title: String,
    detail: String,
    time: String,
    count: String,
}

struct SessionRowData {
    talker_id: String,
    name: String,
    avatar: Option<(Vec<u8>, u32, u32)>,
    preview: String,
    time: String,
    unread: String,
}

struct RelationRowData {
    mid: String,
    uname: String,
    face: Option<(Vec<u8>, u32, u32)>,
    sign: String,
    official: String,
}

struct CardRowData {
    pixels: Option<(Vec<u8>, u32, u32)>,
    title: String,
    meta: String,
    badge: String,
}

struct FolderRowData {
    pixels: Option<(Vec<u8>, u32, u32)>,
    title: String,
    meta: String,
    badge: String,
}

struct HistoryRowData {
    pixels: Option<(Vec<u8>, u32, u32)>,
    title: String,
    meta: String,
    badge: String,
}

struct ToViewRowData {
    pixels: Option<(Vec<u8>, u32, u32)>,
    title: String,
    meta: String,
    badge: String,
}

fn to_thumb(pixels: &Option<(Vec<u8>, u32, u32)>) -> Image {
    match pixels {
        Some((bytes, width, height)) => {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
            buffer.make_mut_bytes().copy_from_slice(bytes);
            Image::from_rgba8(buffer)
        }
        None => Image::default(),
    }
}

/// [`to_thumb`] for a shared cache entry: the one copy into the Slint buffer is
/// the only copy, so a warm cover is not cloned per patch.
fn to_thumb_shared(pixels: &CoverPixels) -> Image {
    let (bytes, width, height) = pixels;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
    buffer.make_mut_bytes().copy_from_slice(bytes);
    Image::from_rgba8(buffer)
}

// ---------------------------------------------------------------------------
// Progressive (two-phase) list rendering.
//
// Every video list loads in two steps:
//   Phase A - the JSON is in; the rows (title / owner / counts / badge and the
//             full card layout) are pushed to the model with an empty cover and
//             the view stops showing a spinner. `... load rendered ms=` logs here.
//   Phase B - the covers stream in with bounded concurrency and every arrival
//             patches exactly its own row through `VecModel::set_row_data`,
//             so the list never rebuilds and the text is readable immediately.
//
// A patch is applied only while the row it was fetched for is still the row at
// that index: `ThumbGuard` re-checks the live generation counter and the
// bvid / dynamic-id table on the UI thread, so a superseded refresh can never
// paint its covers into the rows of the newer one.
// ---------------------------------------------------------------------------

/// Per-list bookkeeping for the streaming cover patches. Shared between the
/// worker stream and the UI patch closures; the summary log line it emits is
/// the proof that every requested cover landed on its own row.
struct ThumbSync {
    label: String,
    total: u64,
    settled: AtomicU64,
    applied: AtomicU64,
    discarded: AtomicU64,
}

impl ThumbSync {
    fn new(label: String, total: usize) -> Arc<Self> {
        Arc::new(Self {
            label,
            total: total as u64,
            settled: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
        })
    }

    /// UI thread: one requested cover was applied or superseded. When the last
    /// one settles, log `thumbs patched n=<applied>/<total> discarded=<n>`.
    fn settle(&self, applied: bool) {
        if applied {
            self.applied.fetch_add(1, Ordering::SeqCst);
        } else {
            self.discarded.fetch_add(1, Ordering::SeqCst);
        }
        let settled = self.settled.fetch_add(1, Ordering::SeqCst) + 1;
        if settled == self.total {
            crate::debug_log(&format!(
                "{} thumbs patched n={}/{} discarded={}",
                self.label,
                self.applied.load(Ordering::SeqCst),
                self.total,
                self.discarded.load(Ordering::SeqCst),
            ));
        }
    }
}

/// Identity check evaluated on the UI thread just before a cover patch lands.
enum ThumbGuard {
    /// The list is owned by a monotonic sequence counter (search).
    Seq(Arc<AtomicU64>, u64),
    /// The row at `index` must still carry this bvid / dynamic id.
    Ids(Arc<Mutex<Vec<String>>>, String),
    /// The home feed: the generation counter must be current and the row must
    /// still hold the same video.
    GenFeed(Arc<AtomicU64>, u64, Arc<Mutex<Vec<bilibili::FeedVideo>>>, String),
    /// 收藏推荐: the view's generation must be current and the row must still
    /// hold the same video (a tab switch bumps the generation).
    Fav(Arc<AtomicU64>, u64, Arc<Mutex<Vec<bilibili::FeedVideo>>>, String),
}

impl ThumbGuard {
    fn allows(&self, index: usize) -> bool {
        match self {
            ThumbGuard::Seq(seq, my) => seq.load(Ordering::SeqCst) == *my,
            ThumbGuard::Ids(ids, id) => ids
                .lock()
                .ok()
                .and_then(|guard| guard.get(index).cloned())
                .map(|current| current == *id)
                .unwrap_or(false),
            ThumbGuard::GenFeed(generation, my, store, id)
            | ThumbGuard::Fav(generation, my, store, id) => {
                generation.load(Ordering::SeqCst) == *my
                    && store
                        .lock()
                        .ok()
                        .and_then(|guard| guard.get(index).map(|video| video.bvid.clone()))
                        .map(|current| current == *id)
                        .unwrap_or(false)
            }
        }
    }
}

/// Replace one row's cover in an already-rendered `VecModel`, keeping its text.
fn patch_model_thumb<T: Clone + 'static>(
    model: ModelRc<T>,
    index: usize,
    thumb: Image,
    set_thumb: fn(&mut T, Image),
) {
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<T>>() {
        if let Some(mut row) = vec_model.row_data(index) {
            set_thumb(&mut row, thumb);
            vec_model.set_row_data(index, row);
        }
    }
}

/// Post one incremental cover patch for a `T`-row list.
fn post_thumb_patch<T: Clone + 'static>(
    weak: &Weak<AppWindow>,
    index: usize,
    pixels: Option<(Vec<u8>, u32, u32)>,
    guard: ThumbGuard,
    get_model: fn(&AppWindow) -> ModelRc<T>,
    set_thumb: fn(&mut T, Image),
    sync: Arc<ThumbSync>,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let applied = guard.allows(index);
            if applied {
                patch_model_thumb(get_model(&ui), index, to_thumb(&pixels), set_thumb);
            }
            skeleton_settle(&ui, 1);
            sync.settle(applied);
        }
    });
}

/// One decoded cover: raw RGBA8 pixels plus the dimensions `to_thumb` needs.
type CoverPixels = (Vec<u8>, u32, u32);

/// Fetches issued by the display path (`stream_thumbs`) and by the ring
/// warm-up, counted separately so the QA hook can prove that a promotion no
/// longer streams the first screenful. Cache hits are not fetches.
static COVER_FETCHES_DISPLAY: AtomicU64 = AtomicU64::new(0);
static COVER_FETCHES_WARM: AtomicU64 = AtomicU64::new(0);
/// Display fetches issued for a row inside the first screenful of a promoted
/// batch. The warm-up exists to make this zero.
static COVER_FETCHES_FIRST_SCREEN: AtomicU64 = AtomicU64::new(0);

fn cover_fetches_display() -> u64 {
    COVER_FETCHES_DISPLAY.load(Ordering::Relaxed)
}

fn cover_fetches_warm() -> u64 {
    COVER_FETCHES_WARM.load(Ordering::Relaxed)
}

fn cover_fetches_first_screen() -> u64 {
    COVER_FETCHES_FIRST_SCREEN.load(Ordering::Relaxed)
}

/// Bounded, least-recently-used cache of decoded covers, keyed by the image URL
/// exactly as the feed hands it out. Both the streaming decoder and the ring
/// warm-up go through it, so a promoted batch paints what its warm-up already
/// downloaded and decoded instead of fetching the same bytes again.
struct CoverCache {
    entries: HashMap<String, Arc<CoverPixels>>,
    /// Recency order, front = least recently used.
    order: VecDeque<String>,
    bytes: usize,
}

impl CoverCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&mut self, url: &str) -> Option<Arc<CoverPixels>> {
        let entry = self.entries.get(url)?.clone();
        self.touch(url);
        Some(entry)
    }

    fn touch(&mut self, url: &str) {
        if let Some(pos) = self.order.iter().position(|key| key == url) {
            self.order.remove(pos);
        }
        self.order.push_back(url.to_string());
    }

    fn put(&mut self, url: &str, pixels: Arc<CoverPixels>) {
        let bytes = pixels.0.len();
        // Empty or oversized decodes are not worth a cache slot; the caller
        // still paints them.
        if bytes == 0 || bytes > COVER_CACHE_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(url) {
            self.bytes = self.bytes.saturating_sub(old.0.len());
            if let Some(pos) = self.order.iter().position(|key| key == url) {
                self.order.remove(pos);
            }
        }
        self.bytes += bytes;
        self.entries.insert(url.to_string(), pixels);
        self.order.push_back(url.to_string());
        while self.bytes > COVER_CACHE_BYTES || self.entries.len() > COVER_CACHE_ENTRIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.0.len());
            }
        }
    }
}

static COVER_CACHE: OnceLock<Mutex<CoverCache>> = OnceLock::new();

fn cover_cache() -> &'static Mutex<CoverCache> {
    COVER_CACHE.get_or_init(|| Mutex::new(CoverCache::new()))
}

/// Which path issued a cover fetch, for the QA counters and nothing else.
#[derive(Clone, Copy)]
enum CoverPath {
    Display,
    Warm,
}

/// Fetch + decode one cover through the shared cache, skipping the network
/// entirely on a hit and while the player is open. `None` means "skipped or
/// failed"; the list resumes with a fresh fetch the moment the player closes.
/// The lock is never held across the network call.
async fn cover_fetch(
    session: &Arc<bilibili::Session>,
    url: &str,
    path: CoverPath,
) -> Option<Arc<CoverPixels>> {
    if url.is_empty() || player_active() {
        return None;
    }
    if let Ok(mut cache) = cover_cache().lock() {
        if let Some(hit) = cache.get(url) {
            return Some(hit);
        }
    }
    match path {
        CoverPath::Display => &COVER_FETCHES_DISPLAY,
        CoverPath::Warm => &COVER_FETCHES_WARM,
    }
    .fetch_add(1, Ordering::Relaxed);
    let pixels = Arc::new(session.fetch_thumb(url).await.ok()?);
    if let Ok(mut cache) = cover_cache().lock() {
        cache.put(url, pixels.clone());
    }
    Some(pixels)
}

/// Fetch a small cover on the shared runtime, skipping it entirely while the
/// player is open. `None` means "skipped or failed"; the list resumes with a
/// fresh fetch the moment the player closes.
async fn thumb_gated(session: &Arc<bilibili::Session>, url: &str) -> Option<CoverPixels> {
    cover_fetch(session, url, CoverPath::Display)
        .await
        .map(|pixels| (*pixels).clone())
}

/// Fetch an avatar/cover via `fetch_image`, skipping it while the player is open.
async fn image_gated(session: &Arc<bilibili::Session>, url: &str) -> Option<(Vec<u8>, u32, u32)> {
    if url.is_empty() || player_active() {
        return None;
    }
    session.fetch_image(url).await.ok()
}

/// Stream one list's covers with `THUMB_CONCURRENCY` in flight and hand every
/// arrival to `on_one` (worker thread) the moment it is decoded. `on_issue`
/// runs as each fetch is actually started (the bounded stream only starts the
/// next one when a slot frees), so a caller can count what the list asked the
/// network for. While the player is open no new fetch starts and late arrivals
/// are dropped unpatched.
async fn stream_thumbs<F, I>(
    session: &Arc<bilibili::Session>,
    pics: Vec<String>,
    mut on_issue: I,
    mut on_one: F,
) where
    F: FnMut(usize, Option<(Vec<u8>, u32, u32)>),
    I: FnMut(usize),
{
    let fetches = pics.into_iter().enumerate().map(|(index, pic)| {
        on_issue(index);
        let session = session.clone();
        async move {
            (index, thumb_gated(&session, &pic).await)
        }
    });
    let mut stream = futures::stream::iter(fetches).buffer_unordered(THUMB_CONCURRENCY);
    while let Some((index, pixels)) = stream.next().await {
        if player_active() {
            continue;
        }
        on_one(index, pixels);
    }
}

// ---------------------------------------------------------------------------
// Skeleton shimmer driver (UI thread only).
//
// While any incremental cover is still pending, a repeating timer advances the
// shared `skeleton-phase` so every placeholder sweeps a soft highlight and the
// floating refresh glyph spins. The timer stops as soon as the last pending
// cover settles (and after a 30s safety deadline), so idle frames cost nothing.
// ---------------------------------------------------------------------------
struct SkeletonState {
    timer: slint::Timer,
    remaining: u64,
    phase: f32,
    armed_at: Instant,
}

thread_local! {
    static SKELETON: RefCell<SkeletonState> = RefCell::new(SkeletonState {
        timer: slint::Timer::default(),
        remaining: 0,
        phase: 0.0,
        armed_at: Instant::now(),
    });
}

/// UI thread: arm the skeleton shimmer for `count` pending covers.
fn skeleton_arm(ui: &AppWindow, count: u64) {
    if count == 0 || player_active() {
        return;
    }
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        state.remaining += count;
        state.armed_at = Instant::now();
        ui.global::<AppTheme>().set_skeletons_on(true);
        if !state.timer.running() {
            let weak = ui.as_weak();
            state.timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(90),
                move || skeleton_tick(&weak),
            );
        }
    });
}

/// UI thread: stop the skeleton shimmer immediately (used when the player opens,
/// so no timer keeps repainting the hidden main window during playback).
fn skeleton_halt(ui: &AppWindow) {
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        state.remaining = 0;
        state.timer.stop();
    });
    ui.global::<AppTheme>().set_skeletons_on(false);
}

fn skeleton_tick(weak: &Weak<AppWindow>) {
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        let timed_out = state.armed_at.elapsed() > Duration::from_secs(30);
        if state.remaining == 0 || timed_out {
            state.remaining = 0;
            state.timer.stop();
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppTheme>().set_skeletons_on(false);
            }
            return;
        }
        state.phase = (state.phase + 0.11) % 1.0;
        if let Some(ui) = weak.upgrade() {
            ui.global::<AppTheme>().set_skeleton_phase(state.phase);
        }
    });
}

/// UI thread: `count` pending covers settled (patched, superseded or empty).
fn skeleton_settle(ui: &AppWindow, count: u64) {
    SKELETON.with(|cell| {
        let mut state = cell.borrow_mut();
        state.remaining = state.remaining.saturating_sub(count);
        if state.remaining == 0 && state.timer.running() {
            state.timer.stop();
            ui.global::<AppTheme>().set_skeletons_on(false);
        }
    });
}

/// Clamp an exact count / Unix timestamp to the `int` a Slint row carries.
fn saturating_i32(value: i64) -> i32 {
    value.clamp(0, i64::from(i32::MAX)) as i32
}

/// Build the text-only UI row for a feed video (empty cover; UI thread only).
fn build_feed_row_text(video: &bilibili::FeedVideo) -> FeedRow {
    FeedRow {
        thumb: Image::default(),
        title: video.title.clone().into(),
        owner: video.owner.clone().into(),
        owner_mid: video.owner_mid.to_string().into(),
        meta: format!(
            "{}播放 · {}",
            format_view(video.view),
            format_duration(video.duration)
        )
        .into(),
        badge: format_duration(video.duration).into(),
        // The card redesign's own fields: the publish stamp (0 = unknown) and
        // the exact play count, so a card never has to parse `meta`.
        pub_ts: saturating_i32(video.pub_ts),
        view_count: saturating_i32(video.view as i64),
    }
}

/// The covers a ring batch carries into its promotion: the first screenful,
/// already decoded, so the first paint of a refreshed page is complete.
struct WarmCovers {
    /// Row index → decoded cover, ascending. Only a prefix of the batch is
    /// here; the rows past it still stream in after promotion.
    rows: Vec<(usize, Arc<CoverPixels>)>,
    /// Covers a full batch would stream (its `THUMB_MAX_PER_VIEW` cap), for the
    /// logs.
    cap: usize,
    /// Rows the first screenful spans ([`feed_cover_warm_target`]), whether or
    /// not every one of them landed.
    target: usize,
}

impl WarmCovers {
    fn new(cap: usize, target: usize) -> Self {
        Self {
            rows: Vec::new(),
            cap,
            target,
        }
    }

    fn count(&self) -> usize {
        self.rows.len()
    }
}

/// Covers warmed per ring batch: one client page as laid out at the 480×270
/// minimum window, where a single grid row fits, so a page's cards are what the
/// user sees before scrolling. The margin covers the next row, and
/// [`COVER_WARM_MAX`] caps the work (and the memory the ring can pin).
fn feed_cover_warm_target(page_size: i64) -> usize {
    ((page_size.max(1) as usize).saturating_add(COVER_WARM_MARGIN)).min(COVER_WARM_MAX)
}

/// `BILI_COVER_WARM=0` turns the ring's cover warm-up off. Kept for the
/// before/after QA measurement; the default is on.
fn cover_warm_enabled() -> bool {
    std::env::var("BILI_COVER_WARM")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

/// Decode the first [`feed_cover_warm_target`] covers of a prefetched batch
/// through the same cache and fetch path the visible list uses. Called on the
/// filler thread with no ring lock held; `player_active` skips it like every
/// other cover fetch, and the deadline keeps a slow CDN from delaying the
/// batch (and with it the startup wait). Failures are not fatal: whatever
/// landed is attached and the rest streams after promotion.
fn warm_feed_covers(
    session: &Arc<bilibili::Session>,
    items: &[bilibili::FeedVideo],
    target: usize,
) -> WarmCovers {
    let cap = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
    let mut warm = WarmCovers::new(cap, target);
    let wanted = target.min(cap);
    if wanted == 0 {
        return warm;
    }
    let Some(runtime) = bilibili::shared_runtime() else {
        crate::debug_log(&format!(
            "feed covers warm n={wanted} ok=0 ms=0 reason=runtime"
        ));
        return warm;
    };
    let began = Instant::now();
    let mut rows: Vec<(usize, Arc<CoverPixels>)> = Vec::with_capacity(wanted);
    // The deadline covers the whole warm-up, not just the gaps between
    // arrivals: a stalled request must not hold the batch (and the startup
    // wait) hostage. Dropping the stream cancels the outstanding fetches; their
    // rows simply stream after promotion.
    let truncated = runtime.block_on(async {
        let fetches = items.iter().take(wanted).enumerate().map(|(index, video)| {
            let session = session.clone();
            let pic = video.pic.clone();
            async move { (index, cover_fetch(&session, &pic, CoverPath::Warm).await) }
        });
        let mut stream = futures::stream::iter(fetches).buffer_unordered(COVER_WARM_IN_FLIGHT);
        let pump = async {
            while let Some((index, pixels)) = stream.next().await {
                if let Some(pixels) = pixels {
                    rows.push((index, pixels));
                }
            }
        };
        tokio::time::timeout(COVER_WARM_BUDGET, pump).await.is_err()
    });
    rows.sort_by_key(|(index, _)| *index);
    warm.rows = rows;
    let elapsed_ms = began.elapsed().as_millis();
    crate::debug_log(&format!(
        "feed covers warm n={wanted} ok={} ms={elapsed_ms}",
        warm.count()
    ));
    if truncated {
        crate::debug_log(&format!(
            "feed covers warm truncated n={wanted} ok={} budget_ms={}",
            warm.count(),
            COVER_WARM_BUDGET.as_millis()
        ));
    }
    warm
}

/// One ready-made recommendation batch: exactly what a refresh would fetch,
/// waiting for a promotion.
struct PrefetchedFeedBatch {
    /// Feed source this batch belongs to ([`FEED_SOURCE_RECOMMEND`] for now).
    source: i32,
    /// `fresh_idx` cursor the batch was fetched from (the same value a refresh
    /// passes to `homepage_feed_from`).
    start: u64,
    items: Vec<bilibili::FeedVideo>,
    /// The first screenful's decoded covers, attached at publish time.
    covers: WarmCovers,
    /// When the fetch **and** its cover warm-up finished; drives the ~10
    /// minute memory bound.
    ready_at: Instant,
}

/// The preload ring: ready-made batches waiting to be promoted on refresh.
///
/// Only the bookkeeping lives behind the mutex — a fill never holds the lock
/// across the network, so the UI thread can promote while a refill is in
/// flight. `next_start` is this ring's own cursor and advances by
/// [`bilibili::feed_batch`] per filled slot, so two promotions can never hand
/// out the same batch. `epoch` invalidates a fetch that started before a
/// rebase (the app showed that cursor from the network instead).
struct FeedPrefetchRing {
    source: i32,
    ready: VecDeque<PrefetchedFeedBatch>,
    /// Cursor of the next batch to fill; `0` = not initialised.
    next_start: u64,
    /// Cursor of the fill currently on the network, if any (rebase check).
    in_flight: Option<u64>,
    /// Bumped when an in-flight fill must be discarded.
    epoch: u64,
}

impl FeedPrefetchRing {
    fn new(source: i32) -> Self {
        Self {
            source,
            ready: VecDeque::new(),
            next_start: 0,
            in_flight: None,
            epoch: 0,
        }
    }

    /// Cursor for the next fill: the startup load's cursor until one advances it.
    fn cursor(&self) -> u64 {
        if self.next_start == 0 {
            FEED_START_CURSOR
        } else {
            self.next_start
        }
    }

    /// Drop every batch older than [`FEED_PREFETCH_TTL`]; returns how many.
    fn prune(&mut self) -> usize {
        let before = self.ready.len();
        self.ready
            .retain(|batch| batch.ready_at.elapsed() < FEED_PREFETCH_TTL);
        before - self.ready.len()
    }

    /// Switch the ring to another source: nothing queued for the old source may
    /// ever be shown, so it is dropped and the cursor starts over.
    fn switch_source(&mut self, source: i32) {
        if self.source == source {
            return;
        }
        let dropped = self.ready.len();
        self.ready.clear();
        self.next_start = 0;
        self.in_flight = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.source = source;
        crate::debug_log(&format!(
            "feed prefetch drop source={source} batches={dropped} reason=source"
        ));
    }

    /// Take the oldest ready batch of `source`. Consuming it is what makes a
    /// promotion one-shot: the filler then refills the freed slot.
    fn promote(&mut self, source: i32) -> Option<PrefetchedFeedBatch> {
        self.switch_source(source);
        self.prune();
        while let Some(batch) = self.ready.pop_front() {
            // Defensive: a batch tagged for another source is never shown.
            if batch.source != source {
                continue;
            }
            // Never hand out a cursor that was already promoted, even if a
            // rebase left older entries behind.
            self.ready.retain(|queued| queued.start > batch.start);
            return Some(batch);
        }
        None
    }

    /// The app fetched the batch at `start` on the network path (startup
    /// fallback, 换一换, a page turn), so a queued or in-flight copy of it would
    /// be the same content twice: drop/rebase past it.
    fn rebase_past(&mut self, start: u64) {
        let before = self.ready.len();
        self.ready.retain(|batch| batch.start > start);
        if self.in_flight.map(|cursor| cursor <= start).unwrap_or(false) {
            self.epoch = self.epoch.wrapping_add(1);
        }
        let next = start.saturating_add(bilibili::feed_batch().max(1));
        self.next_start = self.cursor().max(next);
        crate::debug_log(&format!(
            "feed prefetch rebase start={start} next_start={} dropped={}",
            self.next_start,
            before - self.ready.len()
        ));
    }
}

/// The configured ring capacity (already normalised by settings::load, but a
/// later UI change may be in flight too, so clamp again).
fn feed_prefetch_capacity(settings_store: &Arc<Mutex<settings::Settings>>) -> usize {
    settings_store
        .lock()
        .map(|guard| guard.feed_prefetch_batches.min(settings::FEED_PREFETCH_MAX) as usize)
        .unwrap_or(0)
}

/// UI thread: consume one ready batch, if there is one.
fn take_prefetched_batch(
    ring: &Arc<Mutex<FeedPrefetchRing>>,
    source: i32,
) -> Option<PrefetchedFeedBatch> {
    ring.lock().ok().and_then(|mut guard| guard.promote(source))
}

/// Ring snapshot for the logs: ready count (after pruning) and next cursor.
fn feed_prefetch_state(ring: &Arc<Mutex<FeedPrefetchRing>>) -> (usize, u64) {
    ring.lock()
        .map(|mut guard| {
            guard.prune();
            (guard.ready.len(), guard.cursor())
        })
        .unwrap_or((0, 0))
}

/// True once the home view has been revealed after login. Written on the UI
/// thread and read by the prefetch QA hook's worker (an atomic, because reading
/// Slint properties off the UI thread is not allowed).
static HOME_SHOWN: AtomicBool = AtomicBool::new(false);

/// Keep the ring filled with the configured number of batches. Runs on its own
/// thread from startup: it waits for a session (so it also runs while the login
/// screen is up), then fills one slot at a time and sleeps between checks.
fn spawn_feed_prefetch(
    session_slot: SessionSlot,
    ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
) {
    std::thread::spawn(move || loop {
        let capacity = feed_prefetch_capacity(&settings_store);
        let Some(session) = active_session(&session_slot) else {
            std::thread::sleep(FEED_PREFETCH_POLL);
            continue;
        };
        // Reserve the cursor and mark the fill in flight under the same lock:
        // a rebase that lands after this point sees `in_flight` and can
        // invalidate this fetch before it is queued.
        let (ready, source, start, epoch, want_fill) = {
            let Ok(mut guard) = ring.lock() else {
                std::thread::sleep(FEED_PREFETCH_POLL);
                continue;
            };
            guard.switch_source(FEED_SOURCE_RECOMMEND);
            guard.prune();
            // A settings change can lower the cap while slots are queued.
            while guard.ready.len() > capacity {
                guard.ready.pop_back();
            }
            let want_fill = capacity > 0 && guard.ready.len() < capacity;
            let start = guard.cursor();
            if want_fill {
                guard.in_flight = Some(start);
            }
            (
                guard.ready.len(),
                guard.source,
                start,
                guard.epoch,
                want_fill,
            )
        };
        if !want_fill {
            std::thread::sleep(FEED_PREFETCH_POLL);
            continue;
        }
        crate::debug_log(&format!(
            "feed prefetch refill started source={source} start={start} slot={}/{}",
            ready + 1,
            capacity
        ));
        let began = Instant::now();
        // Same fetch as the refresh path, only the progress callback is inert.
        let items = fetch_feed(session.clone(), start, Arc::new(|_, _| {}));
        let elapsed_ms = began.elapsed().as_millis();
        let count = items.len();
        // Warm the covers the first paint needs **before** the batch is queued,
        // so `ready` means "text and first screenful decoded". No ring lock is
        // held here (a refill may be promoted while it runs; `epoch` decides).
        let page_size = settings_store
            .lock()
            .map(|guard| guard.page_size)
            .unwrap_or_else(|_| settings::default_page_size());
        // Do not spend cover fetches on a batch that is already obsolete (a
        // rebase landed) or no longer fits the ring.
        let still_wanted = ring
            .lock()
            .map(|guard| guard.epoch == epoch && guard.ready.len() < capacity)
            .unwrap_or(false);
        let target = feed_cover_warm_target(page_size);
        let cap = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
        let covers = if items.is_empty() || !still_wanted || !cover_warm_enabled() {
            WarmCovers::new(cap, target)
        } else {
            warm_feed_covers(&session, &items, target)
        };
        let covers_warm = covers.count();
        let covers_cap = covers.cap;
        // Text + warm-up: what "ready" now costs.
        let ready_ms = began.elapsed().as_millis();
        let outcome = {
            let Ok(mut guard) = ring.lock() else {
                continue;
            };
            if guard.in_flight == Some(start) {
                guard.in_flight = None;
            }
            if guard.epoch != epoch {
                "rebase"
            } else if guard.ready.len() >= capacity {
                "full"
            } else if items.is_empty() {
                "empty"
            } else {
                guard.next_start = start.saturating_add(bilibili::feed_batch().max(1));
                guard.ready.push_back(PrefetchedFeedBatch {
                    source,
                    start,
                    items,
                    covers,
                    ready_at: Instant::now(),
                });
                guard.prune();
                "ok"
            }
        };
        match outcome {
            "ok" => {
                let (now_ready, _) = feed_prefetch_state(&ring);
                crate::debug_log(&format!(
                    "feed prefetch refill ok source={source} start={start} items={count} ms={elapsed_ms}"
                ));
                crate::debug_log(&format!(
                    "feed prefetch ready={now_ready}/{capacity} covers={covers_warm}/{covers_cap} source={source} ms={ready_ms}"
                ));
                if cover_warm_enabled() && covers_warm == 0 && covers_cap > 0 && !player_active() {
                    crate::debug_log(&format!(
                        "feed prefetch refill covers=failed source={source} start={start}"
                    ));
                }
            }
            "empty" => {
                crate::debug_log(&format!(
                    "feed prefetch refill err=empty source={source} start={start} ms={elapsed_ms}"
                ));
                // Do not hammer a failing endpoint; the cursor stays put so the
                // same slot is retried once the network recovers.
                std::thread::sleep(Duration::from_secs(1));
            }
            reason => {
                crate::debug_log(&format!(
                    "feed prefetch refill dropped source={source} start={start} reason={reason}"
                ));
            }
        }
    });
}

/// One cover a rendered list still has to fetch: the row it belongs to, its CDN
/// url and its bvid (the patch guard's identity), in the order they stream.
struct PendingCover {
    index: usize,
    pic: String,
    bvid: String,
}

/// The covers `stream_feed_covers` has left for a batch: its cover cap minus
/// the rows a ring warm-up already decoded (a promoted batch repaints none of
/// its warm covers). Also returns the cap itself, the model's cover count.
fn pending_feed_covers(
    videos: &[bilibili::FeedVideo],
    warm: &[(usize, Arc<CoverPixels>)],
) -> (usize, Vec<PendingCover>) {
    let cap = videos.len().min(bilibili::THUMB_MAX_PER_VIEW);
    let warmed: HashSet<usize> = warm.iter().map(|(index, _)| *index).collect();
    let pending = videos
        .iter()
        .take(cap)
        .enumerate()
        .filter(|(index, _)| !warmed.contains(index))
        .map(|(index, video)| PendingCover {
            index,
            pic: video.pic.clone(),
            bvid: video.bvid.clone(),
        })
        .collect();
    (cap, pending)
}

/// Fetch only the recommendation JSON on a private thread with its own
/// runtime; the covers stream separately in `load_feed`.
fn fetch_feed(
    session: Arc<bilibili::Session>,
    fresh_start: u64,
    on_page: Arc<dyn Fn(u64, u64) + Send + Sync>,
) -> Vec<bilibili::FeedVideo> {
    let Some(runtime) = crate::bilibili::shared_runtime() else {
        return Vec::new();
    };
    runtime.block_on(async move {
        match session.homepage_feed_from(fresh_start, &*on_page).await {
            Ok(items) => items,
            Err(err) => {
                crate::debug_log(&format!("feed err={err}"));
                Vec::new()
            }
        }
    })
}

/// Fetch one feed batch (identified by its `fresh_idx` start) on a worker
/// thread. The 换一换 path and the page bar's first / prev / next / refresh all
/// land here; `target_page` is the client page the batch lands on (0 for a new
/// batch, the reloaded page for a refresh).
fn spawn_feed_load(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    start: u64,
    target_page: i64,
) {
    crate::debug_log(&format!(
        "feed load spawn start={start} target_page={target_page}"
    ));
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let my = generation.load(Ordering::SeqCst);
        runtime.block_on(load_feed(
            &weak,
            &generation,
            my,
            &session_slot,
            &feed_store,
            start,
            target_page,
        ));
    });
}

/// Fetch the single account's recommendation list and show it: the text rows
/// first (phase A), then the covers streamed into the rendered rows (phase B).
async fn load_feed(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    fresh_start: u64,
    target_page: i64,
) {
    let load_started = Instant::now();
    post(weak, generation, my, |ui| {
        publish_refresh(ui, true, 0.0);
        // The bar's refresh mark spins off the shared skeleton phase while the
        // load is in flight; this arm keeps that ticker alive until phase A.
        skeleton_arm(ui, 1);
        update_page_bar(ui);
    });

    let Some(session) = active_session(session_slot) else {
        post(weak, generation, my, |ui| {
            publish_refresh(ui, false, 1.0);
            ui.set_status("没有可用的账号会话".into());
            skeleton_settle(ui, 1);
            update_page_bar(ui);
        });
        return;
    };

    // The feed fetches `feed_batch()` pages in one call; the callback lands once
    // per page on the worker thread and republishes the bar as pages_done/total.
    let progress_weak = weak.clone();
    let progress_generation = generation.clone();
    let on_page: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |done, total| {
        crate::debug_log(&format!("refresh progress {done}/{total}"));
        let fraction = if total == 0 {
            1.0
        } else {
            done as f32 / total as f32
        };
        post(&progress_weak, &progress_generation, my, move |ui| {
            publish_refresh(ui, true, fraction);
        });
    });

    let videos = match std::thread::spawn({
        let session = session.clone();
        move || fetch_feed(session, fresh_start, on_page)
    })
    .join()
    {
        Ok(videos) => videos,
        Err(_) => Vec::new(),
    };
    crate::debug_log(&format!(
        "feed load fetched ms={} items={}",
        load_started.elapsed().as_millis(),
        videos.len()
    ));
    if videos.is_empty() {
        post(weak, generation, my, |ui| {
            publish_refresh(ui, false, 1.0);
            ui.set_status("首页推荐为空".into());
            skeleton_settle(ui, 1);
            update_page_bar(ui);
        });
        return;
    }

    let (cap, pending) = pending_feed_covers(&videos, &[]);
    let item_count = videos.len();

    // ---- Phase A: text rows on screen immediately ----
    let feed_store_a = feed_store.clone();
    let generation_a = generation.clone();
    let started_a = load_started;
    post(weak, generation, my, move |ui| {
        if publish_feed_rows(
            ui,
            &generation_a,
            my,
            &feed_store_a,
            videos,
            target_page,
            cap,
        ) {
            crate::debug_log(&format!(
                "feed load rendered ms={} items={item_count}",
                started_a.elapsed().as_millis()
            ));
        }
    });

    // ---- Phase B: each cover patches its own row as it arrives ----
    stream_feed_covers(
        weak,
        generation,
        my,
        &session,
        feed_store,
        pending,
        0,
        &format!("feed start={fresh_start}"),
    )
    .await;
}

/// UI thread: show one feed batch's text rows and point the pager at
/// `target_page`. No network happens here — the covers come from
/// [`stream_feed_covers`] on a worker. Returns whether the batch was published
/// (a superseded generation is dropped instead).
fn publish_feed_rows(
    ui: &AppWindow,
    generation: &Arc<AtomicU64>,
    my: u64,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    videos: Vec<bilibili::FeedVideo>,
    target_page: i64,
    cap: usize,
) -> bool {
    publish_feed_rows_with_covers(
        ui,
        generation,
        my,
        feed_store,
        videos,
        target_page,
        cap,
        &[],
    )
}

/// [`publish_feed_rows`] with a ring batch's already-decoded covers attached to
/// their rows before the model goes live: a promoted batch's first paint is
/// complete, so nothing streams into the first screenful afterwards.
fn publish_feed_rows_with_covers(
    ui: &AppWindow,
    generation: &Arc<AtomicU64>,
    my: u64,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    videos: Vec<bilibili::FeedVideo>,
    target_page: i64,
    cap: usize,
    covers: &[(usize, Arc<CoverPixels>)],
) -> bool {
    if generation.load(Ordering::SeqCst) != my {
        return false;
    }
    let mut items: Vec<FeedRow> = videos.iter().map(build_feed_row_text).collect();
    for (index, pixels) in covers {
        if let Some(row) = items.get_mut(*index) {
            row.thumb = to_thumb_shared(pixels);
        }
    }
    if let Ok(mut guard) = feed_store.lock() {
        *guard = videos;
    }
    ui.set_feed_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
    publish_refresh(ui, false, 1.0);
    // The bar's counter is the client page over this batch; a new batch
    // lands on page 0, a refresh keeps the page it reloaded.
    set_client_page(0, target_page);
    publish_list_offset(ui);
    // Only covers that still have to arrive arm the shimmer: a warm batch's
    // attached rows are already complete.
    skeleton_arm(ui, cap.saturating_sub(covers.len()) as u64);
    skeleton_settle(ui, 1);
    update_page_bar(ui);
    if std::env::var("BILI_AUTOPLAY")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        ui.invoke_open_video(0);
    }
    // Diagnostic hook (mirrors BILI_AUTOPLAY): open a SECOND video a few
    // seconds after the first, reproducing the multi-player case from the
    // backend. `BILI_MULTI_TEST=<delay_ms>` (default 6000).
    if let Ok(value) = std::env::var("BILI_MULTI_TEST") {
        let delay = value.trim().parse::<u64>().unwrap_or(6000);
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    crate::debug_log(&format!("multi-test open second video delay={delay}"));
                    ui.invoke_open_video(1);
                }
            });
        });
    }

    // Multi-open QA (`BILI_MULTI_OPEN_TEST=<gap_ms>`, default 7000): opens two
    // real player windows through card_open mode 1 (the same path the UI's right
    // click uses), so the per-window pointer watch can be proven with the probe.
    if let Ok(value) = std::env::var("BILI_MULTI_OPEN_TEST") {
        let gap = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(7000)
            .clamp(1000, 60_000);
        let weak = ui.as_weak();
        let feed_store = feed_store.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if feed_store
                    .lock()
                    .map(|feed| !feed.is_empty())
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            for index in [0, 1] {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        crate::debug_log(&format!(
                            "multi-open-test invoke card_open index={index} mode=1"
                        ));
                        ui.invoke_card_open(index, 1);
                    }
                });
                std::thread::sleep(Duration::from_millis(gap));
            }
            crate::debug_log("multi-open-test done");
        });
    }

    // Card-mode QA (`BILI_CARD_TEST=<gap_ms>`, default 4000): drives the same
    // three modes the UI's card_open callback delivers. The window/queue effects
    // are logged by the callbacks themselves (`card open`, `player session
    // open`, `player close`, `card enqueue`). No synthetic input.
    if let Ok(value) = std::env::var("BILI_CARD_TEST") {
        let gap = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .unwrap_or(4000)
            .clamp(1000, 30_000);
        let weak = ui.as_weak();
        let feed_store = feed_store.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if feed_store
                    .lock()
                    .map(|feed| !feed.is_empty())
                    .unwrap_or(false)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            for (index, mode, label) in [
                (0, 1, "new-window-1"),
                (1, 1, "new-window-2"),
                (2, 2, "enqueue"),
                (3, 0, "replace"),
            ] {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        crate::debug_log(&format!(
                            "card-test invoke index={index} mode={mode} step={label}"
                        ));
                        ui.invoke_card_open(index, mode);
                    }
                });
                std::thread::sleep(Duration::from_millis(gap));
            }
            crate::debug_log("card-test done");
        });
    }

    // Window-move probe (`BILI_WINDOW_PROBE_TEST=<gap_ms>`, default 1500) is
    // started by `open_player_inner` once two windows exist: it moves only the
    // WINDOW rects (`SetWindowPos`) under a stationary cursor and runs on the UI
    // thread (a Slint timer), because it needs `Rc<PlayerState>`.

    // Diagnostic hook (mirrors BILI_AUTOPLAY): open the detail view for one
    // feed row once the feed has loaded, so the detail view can be captured
    // from PrintWindow / BILI_DEBUG_LOG without synthetic input.
    if let Ok(value) = std::env::var("BILI_DETAIL_ON_START") {
        if let Ok(index) = value.trim().parse::<i32>() {
            crate::debug_log(&format!("detail-test open index={index}"));
            ui.invoke_open_feed_detail(index);
        }
    }
    // Diagnostic hook (mirrors BILI_DETAIL_ON_START): open the settings
    // panel after the feed has loaded so its 账户 section (会员中心 /
    // 退出登录) can be captured from PrintWindow without synthetic input.
    if std::env::var("BILI_SETTINGS_ON_START")
        .map(|value| value == "1")
        .unwrap_or(false)
    {
        crate::debug_log("settings-test open");
        ui.invoke_open_settings();
    }
    // Diagnostic hook: exercise the whole playback queue through its real
    // callbacks once the feed it enqueues from is on screen.
    maybe_start_playlist_test(ui);
    true
}

/// Phase B: stream one feed batch's remaining covers into the rows that
/// [`publish_feed_rows`] already rendered. `pending` is ordered by row, so a
/// warm batch skips exactly the covers its promotion already attached. Every
/// patch is guarded by the generation and the row's bvid, so a superseded batch
/// can never paint over a newer one. `first_screen` is the row count the ring's
/// warm-up covers; any fetch issued below it is a warm-up miss (diagnostic).
async fn stream_feed_covers(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session: &Arc<bilibili::Session>,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    pending: Vec<PendingCover>,
    first_screen: usize,
    label: &str,
) {
    let thumbs_started = Instant::now();
    let fallbacks_before = bilibili::thumb_fallbacks();
    let total = pending.len();
    let sync = ThumbSync::new(label.to_string(), total);
    let weak_b = weak.clone();
    let generation_b = generation.clone();
    let feed_store_b = feed_store.clone();
    let indices: Vec<usize> = pending.iter().map(|cover| cover.index).collect();
    let bvids: Vec<String> = pending.iter().map(|cover| cover.bvid.clone()).collect();
    let pics: Vec<String> = pending.iter().map(|cover| cover.pic.clone()).collect();
    let indices_issue = indices.clone();
    let count_first_screen = move |slot: usize| {
        if indices_issue.get(slot).copied().unwrap_or(usize::MAX) < first_screen {
            COVER_FETCHES_FIRST_SCREEN.fetch_add(1, Ordering::Relaxed);
        }
    };
    let mut delivered = 0usize;
    stream_thumbs(session, pics, count_first_screen, |slot, pixels| {
        delivered += 1;
        let index = indices.get(slot).copied().unwrap_or(slot);
        let bvid = bvids.get(slot).cloned().unwrap_or_default();
        let guard = ThumbGuard::GenFeed(generation_b.clone(), my, feed_store_b.clone(), bvid);
        let weak_c = weak_b.clone();
        let sync_c = sync.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak_c.upgrade() {
                let applied = guard.allows(index);
                if applied {
                    patch_model_thumb(ui.get_feed_items(), index, to_thumb(&pixels), |row, image| {
                        row.thumb = image
                    });
                }
                skeleton_settle(&ui, 1);
                sync_c.settle(applied);
            }
        });
    })
    .await;
    crate::debug_log(&format!(
        "feed thumbs n={} fallback={} ms={} label={label}",
        delivered,
        bilibili::thumb_fallbacks().saturating_sub(fallbacks_before),
        thumbs_started.elapsed().as_millis()
    ));
    flush_missing(weak, &sync, total.saturating_sub(delivered));
}

/// UI thread: publish a promoted batch with its warm covers attached and stream
/// the rest from a background thread. Used by the home refresh, which must not
/// fetch on the UI path. Logs the promotion line itself, so `covers=` is the
/// count of covers that were already painted by this one event.
fn show_prefetched_batch(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session: Option<Arc<bilibili::Session>>,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    batch: PrefetchedFeedBatch,
    target_page: i64,
) -> bool {
    let began = Instant::now();
    let age_ms = batch.ready_at.elapsed().as_millis();
    let item_count = batch.items.len();
    let batch_start = batch.start;
    let PrefetchedFeedBatch {
        items, covers, ..
    } = batch;
    let warm_target = covers.target;
    let warm = covers.rows;
    let warm_count = warm.len();
    let cover_cap = covers.cap;
    let (cap, pending) = pending_feed_covers(&items, &warm);
    let pending_count = pending.len();
    // How many covers of the first screenful the display path still has to
    // fetch after this promotion: `0` is the whole point of the warm-up.
    let first_screen_pending = pending.iter().filter(|cover| cover.index < warm_target).count();
    let Some(ui) = weak.upgrade() else {
        return false;
    };
    if !publish_feed_rows_with_covers(&ui, generation, my, feed_store, items, target_page, cap, &warm)
    {
        return false;
    }
    crate::debug_log(&format!(
        "feed prefetch promote items={item_count} covers={warm_count}/{cover_cap} pending={pending_count} first_screen_pending={first_screen_pending} age_ms={age_ms} ms={} start={batch_start} path=ring",
        began.elapsed().as_millis()
    ));
    let Some(session) = session else {
        return true;
    };
    let weak_b = weak.clone();
    let generation_b = generation.clone();
    let feed_store_b = feed_store.clone();
    std::thread::spawn(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(stream_feed_covers(
            &weak_b,
            &generation_b,
            my,
            &session,
            &feed_store_b,
            pending,
            warm_target,
            "feed prefetch",
        ));
    });
    true
}

/// UI thread: leave the login view for the populated home page.
fn reveal_home(ui: &AppWindow) {
    ui.set_logged_in(true);
    ui.set_view_mode(0);
    enter_paged_view(ui);
    update_page_bar(ui);
    HOME_SHOWN.store(true, Ordering::SeqCst);
}

/// Outcome of waiting for the ring's first batch on the startup path.
enum StartupBatch {
    Ready(PrefetchedFeedBatch),
    Timeout,
    /// A newer login attempt superseded this one; leave the UI alone.
    Superseded,
}

/// Wait up to `timeout` for a ready batch of `source`, polling the ring. Uses
/// the async sleep so the shared runtime keeps its workers serving the network
/// while the login view waits.
async fn wait_prefetched_batch(
    ring: &Arc<Mutex<FeedPrefetchRing>>,
    source: i32,
    timeout: Duration,
    generation: &Arc<AtomicU64>,
    my: u64,
) -> StartupBatch {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(batch) = take_prefetched_batch(ring, source) {
            return StartupBatch::Ready(batch);
        }
        if generation.load(Ordering::SeqCst) != my {
            return StartupBatch::Superseded;
        }
        if Instant::now() >= deadline {
            return StartupBatch::Timeout;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// After a successful login: reveal the home view with a batch already on
/// screen (a), or after [`FEED_HOME_WAIT`] regardless (b), so the login screen
/// can never trap the user. The timeout path keeps today's network load and
/// rebases the ring past the cursor that load uses.
async fn show_home_after_login(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session_slot: &SessionSlot,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    ring: &Arc<Mutex<FeedPrefetchRing>>,
    session: Arc<bilibili::Session>,
    capacity: usize,
) {
    let waited = Instant::now();
    if capacity == 0 {
        // Preloading is off: nothing can arrive, so do not make the user wait.
        let elapsed = waited.elapsed().as_millis();
        post(weak, generation, my, move |ui| {
            reveal_home(ui);
            crate::debug_log(&format!(
                "startup home show waited_ms={elapsed} batch=disabled"
            ));
        });
        load_feed(
            weak,
            generation,
            my,
            session_slot,
            feed_store,
            FEED_START_CURSOR,
            0,
        )
        .await;
        return;
    }
    match wait_prefetched_batch(ring, FEED_SOURCE_RECOMMEND, FEED_HOME_WAIT, generation, my).await {
        StartupBatch::Superseded => {}
        StartupBatch::Ready(batch) => {
            let item_count = batch.items.len();
            let batch_start = batch.start;
            let warm = batch.covers.rows;
            let warm_count = warm.len();
            let cover_cap = batch.covers.cap;
            let (cap, pending) = pending_feed_covers(&batch.items, &warm);
            let feed_store_a = feed_store.clone();
            let generation_a = generation.clone();
            post(weak, generation, my, move |ui| {
                reveal_home(ui);
                let published = publish_feed_rows_with_covers(
                    ui,
                    &generation_a,
                    my,
                    &feed_store_a,
                    batch.items,
                    0,
                    cap,
                    &warm,
                );
                crate::debug_log(&format!(
                    "startup home show waited_ms={} batch=ready",
                    waited.elapsed().as_millis()
                ));
                if published {
                    crate::debug_log(&format!(
                        "startup feed items={item_count} start={batch_start} covers={warm_count}/{cover_cap}"
                    ));
                }
            });
            let weak_b = weak.clone();
            let generation_b = generation.clone();
            let feed_store_b = feed_store.clone();
            let session_b = session.clone();
            std::thread::spawn(move || {
                let Some(runtime) = bilibili::shared_runtime() else {
                    return;
                };
                runtime.block_on(stream_feed_covers(
                    &weak_b,
                    &generation_b,
                    my,
                    &session_b,
                    &feed_store_b,
                    pending,
                    0,
                    &format!("feed start={batch_start}"),
                ));
            });
        }
        StartupBatch::Timeout => {
            // Show the home view now and let the existing network path fill it.
            let elapsed = waited.elapsed().as_millis();
            post(weak, generation, my, move |ui| {
                reveal_home(ui);
                crate::debug_log(&format!(
                    "startup home show waited_ms={elapsed} batch=timeout"
                ));
            });
            // That load uses the startup cursor, so a queued/in-flight copy of
            // it would be the same content twice.
            if let Ok(mut guard) = ring.lock() {
                guard.rebase_past(FEED_START_CURSOR);
            }
            load_feed(
                weak,
                generation,
                my,
                session_slot,
                feed_store,
                FEED_START_CURSOR,
                0,
            )
            .await;
        }
    }
}

/// `BILI_FEED_PREFETCH_TEST=1`: after login, log the ring state every second
/// and simulate two home refreshes 3 s apart. The logs prove the first is a
/// ring promotion (no feed fetch) while the refill happens afterwards, and the
/// 2 s fetch count after each refresh proves whether its first screenful had to
/// stream (display fetches) or was already painted by the promotion.
fn run_feed_prefetch_test(
    weak: Weak<AppWindow>,
    ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
) {
    std::thread::spawn(move || {
        // Wait for the home view: login finished and the first batch is up.
        // `HOME_SHOWN` is the atomic the UI thread sets in `reveal_home`;
        // reading the Slint property from this thread is not allowed.
        for _ in 0..480 {
            if HOME_SHOWN.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let capacity = feed_prefetch_capacity(&settings_store);
        crate::debug_log(&format!(
            "feed prefetch test start capacity={capacity} cover_warm={}",
            u8::from(cover_warm_enabled())
        ));
        // (when the refresh was asked for, display/warm fetch marks at that
        // moment); reported 2 s later, the window the old streaming path needed
        // to paint a page.
        let mut marks: Option<(Instant, u64, u64, u64)> = None;
        let mut refreshes = 0;
        let mut last_refresh: Option<Instant> = None;
        for second in 0..20 {
            if let Some((at, display, warm, first_screen)) = marks {
                if at.elapsed() >= Duration::from_secs(2) {
                    crate::debug_log(&format!(
                        "feed covers post_refresh window_ms={} fetches_display={} fetches_first_screen={} fetches_warm={} t={second}s",
                        at.elapsed().as_millis(),
                        cover_fetches_display().saturating_sub(display),
                        cover_fetches_first_screen().saturating_sub(first_screen),
                        cover_fetches_warm().saturating_sub(warm),
                    ));
                    marks = None;
                }
            }
            let (ready, next_start) = feed_prefetch_state(&ring);
            crate::debug_log(&format!(
                "feed prefetch test ring={ready}/{capacity} source={FEED_SOURCE_RECOMMEND} next_start={next_start} t={second}s"
            ));
            // A refresh is only interesting once a batch is ready: with an
            // empty ring it takes the network path, which is not what this hook
            // measures (and its late result would race the next promotion).
            // One refresh per measurement window, and 6 s apart, so the
            // previous page's tail has finished and `post_refresh` stays exact.
            let spaced = last_refresh
                .map(|at| at.elapsed() >= Duration::from_secs(6))
                .unwrap_or(true);
            if refreshes < 2 && second >= 8 && ready > 0 && marks.is_none() && spaced {
                refreshes += 1;
                last_refresh = Some(Instant::now());
                crate::debug_log(&format!(
                    "feed prefetch test refresh n={refreshes} ready={ready}"
                ));
                marks = Some((
                    Instant::now(),
                    cover_fetches_display(),
                    cover_fetches_warm(),
                    cover_fetches_first_screen(),
                ));
                let weak_ui = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_ui.upgrade() {
                        ui.invoke_page_refresh();
                    }
                });
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        if let Some((at, display, warm, first_screen)) = marks {
            crate::debug_log(&format!(
                "feed covers post_refresh window_ms={} fetches_display={} fetches_first_screen={} fetches_warm={} t=final",
                at.elapsed().as_millis(),
                cover_fetches_display().saturating_sub(display),
                cover_fetches_first_screen().saturating_sub(first_screen),
                cover_fetches_warm().saturating_sub(warm),
            ));
        }
        let (ready, next_start) = feed_prefetch_state(&ring);
        crate::debug_log(&format!(
            "feed prefetch test done ring={ready}/{capacity} next_start={next_start}"
        ));
    });
}


/// UI hand-off of the covers a stream never delivered (worker cut short);
/// keeps the skeleton counter exact and closes the patch summary.
fn flush_missing(weak: &Weak<AppWindow>, sync: &Arc<ThumbSync>, missing: usize) {
    if missing == 0 {
        return;
    }
    let weak = weak.clone();
    let sync = sync.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            skeleton_settle(&ui, missing as u64);
            for _ in 0..missing {
                sync.settle(false);
            }
        }
    });
}

/// Phase B for one already-rendered `T`-row list: stream `pics` with bounded
/// concurrency and patch each decoded cover into the row at
/// `index_offset + index`. `keys` holds the per-row bvid / dynamic id;
/// `guard_of` builds the identity check that discards a patch whose load was
/// superseded, so stale covers can never land on newer rows.
async fn stream_cover_list<T: Clone + 'static, G>(
    weak: &Weak<AppWindow>,
    session: &Arc<bilibili::Session>,
    pics: Vec<String>,
    keys: Vec<String>,
    index_offset: usize,
    mut guard_of: G,
    get_model: fn(&AppWindow) -> ModelRc<T>,
    set_thumb: fn(&mut T, Image),
    label: String,
) where
    G: FnMut(usize, String) -> ThumbGuard,
{
    let total = pics.len();
    let sync = ThumbSync::new(label.clone(), total);
    let started = Instant::now();
    let fallbacks_before = bilibili::thumb_fallbacks();
    let mut delivered = 0usize;
    stream_thumbs(session, pics, |_| {}, |index, pixels| {
        delivered += 1;
        let row = index_offset + index;
        let key = keys.get(index).cloned().unwrap_or_default();
        post_thumb_patch(
            weak,
            row,
            pixels,
            guard_of(row, key),
            get_model,
            set_thumb,
            sync.clone(),
        );
    })
    .await;
    crate::debug_log(&format!(
        "{label} thumbs n={} fallback={} ms={}",
        delivered,
        bilibili::thumb_fallbacks().saturating_sub(fallbacks_before),
        started.elapsed().as_millis()
    ));
    flush_missing(weak, &sync, total.saturating_sub(delivered));
}

/// Phase B for a shared `[CardRow]` list (分区 / 排行 / 热门 / 必刷): stream the
/// covers of the first `THUMB_MAX_PER_VIEW` cards.
async fn stream_card_covers(
    weak: &Weak<AppWindow>,
    session: &Arc<bilibili::Session>,
    cards: &[bilibili::VideoCard],
    ids: Arc<Mutex<Vec<String>>>,
    get_model: fn(&AppWindow) -> ModelRc<CardRow>,
    label: String,
) {
    let cap = cards.len().min(bilibili::THUMB_MAX_PER_VIEW);
    let pics: Vec<String> = cards.iter().take(cap).map(|card| card.pic.clone()).collect();
    let bvids: Vec<String> = cards
        .iter()
        .take(cap)
        .map(|card| card.bvid.clone())
        .collect();
    stream_cover_list(
        weak,
        session,
        pics,
        bvids,
        0,
        move |_index, key| ThumbGuard::Ids(ids.clone(), key),
        get_model,
        |row, image| row.thumb = image,
        label,
    )
    .await;
}

/// Build the UI row for a decoded card (creates `slint::Image`; UI thread only).
fn build_card_row(row: &CardRowData) -> CardRow {
    CardRow {
        thumb: to_thumb(&row.pixels),
        title: row.title.clone().into(),
        meta: row.meta.clone().into(),
        badge: row.badge.clone().into(),
    }
}

/// Push a card list's bvids for click routing and build its UI rows.
fn publish_cards(
    rows: &[(String, CardRowData)],
    browse_bvids: &Arc<Mutex<Vec<String>>>,
) -> Vec<CardRow> {
    let bvids: Vec<String> = rows.iter().map(|(bvid, _)| bvid.clone()).collect();
    if let Ok(mut guard) = browse_bvids.lock() {
        *guard = bvids;
    }
    rows.iter().map(|(_, row)| build_card_row(row)).collect()
}

fn load_region(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    region_state: Arc<Mutex<RegionState>>,
    browse_bvids: Arc<Mutex<Vec<String>>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (rid, name, page) = match region_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.rid, state.name.clone(), state.page)
        }
        Err(_) => return,
    };
    // One API page per screen: fetch with the live client page size so the
    // loaded page fills exactly one screen (the endpoint's default applies when
    // the size is out of range).
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_region_title(name.into());
        ui.set_region_current_page(page as i32);
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let card_page = match session.region_latest(rid, page, ps).await {
                Ok(card_page) => card_page,
                Err(err) => {
                    crate::debug_log(&format!("region rid={rid} err={err:#}"));
                    let message = format!("分区视频加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_region_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<CardRow>::new(),
                            ))));
                            ui.set_region_page_text(message.into());
                            ui.set_region_has_prev(false);
                            ui.set_region_has_next(false);
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let num = card_page.page;
            let has_more = card_page.has_more;
            let cards = card_page.items;
            let rows: Vec<(String, CardRowData)> = cards
                .iter()
                .map(|card| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        card.owner,
                        format_view(card.view),
                        format_duration(card.duration)
                    );
                    (
                        card.bvid.clone(),
                        CardRowData {
                            pixels: None,
                            title: card.title.clone(),
                            meta,
                            badge: format_duration(card.duration),
                        },
                    )
                })
                .collect();
            let loaded = rows.len();
            let covers = cards.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let weak_a = weak.clone();
            let browse_a = browse_bvids.clone();
            let region_state_a = region_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let items = publish_cards(&rows, &browse_a);
                    if let Ok(mut state) = region_state_a.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    ui.set_region_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_region_page_text(format!("第 {num} 页").into());
                    ui.set_region_current_page(num as i32);
                    ui.set_region_has_prev(num > 1);
                    ui.set_region_has_next(has_more);
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(8, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("region load rendered items={loaded}"));
                }
            });
            stream_card_covers(
                &weak,
                &session,
                &cards,
                browse_bvids,
                |ui| ui.get_region_items(),
                format!("region rid={rid} page={num}"),
            )
            .await;
        });
    });
}

fn load_ranking(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    browse_bvids: Arc<Mutex<Vec<String>>>,
    rank_rid: Arc<Mutex<i64>>,
    rid: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Ok(mut guard) = rank_rid.lock() {
        *guard = rid;
    }
    let index = if rid == 0 {
        0
    } else {
        REGIONS
            .iter()
            .position(|(_, region)| *region == rid)
            .map(|position| position + 1)
            .unwrap_or(0)
    };
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_rank_region_index(index as i32);
        ui.set_rank_status(String::new().into());
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let cards = match session.ranking(rid).await {
                Ok(cards) => cards,
                Err(err) => {
                    crate::debug_log(&format!("ranking rid={rid} err={err:#}"));
                    let message = format!("排行榜加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_rank_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<CardRow>::new(),
                            ))));
                            ui.set_rank_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let total = cards.len();
            let covers = total.min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, CardRowData)> = cards
                .iter()
                .map(|card| {
                    let meta = format!(
                        "{} · {}播放 · 赞{}",
                        card.owner,
                        format_view(card.view),
                        format_view(card.like)
                    );
                    (
                        card.bvid.clone(),
                        CardRowData {
                            pixels: None,
                            title: card.title.clone(),
                            meta,
                            badge: format_duration(card.duration),
                        },
                    )
                })
                .collect();
            let weak_a = weak.clone();
            let browse_a = browse_bvids.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let items = publish_cards(&rows, &browse_a);
                    ui.set_rank_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_rank_status(String::new().into());
                    // Single-shot list: the client page spans the loaded rows.
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("ranking load rendered items={total}"));
                }
            });
            stream_card_covers(
                &weak,
                &session,
                &cards,
                browse_bvids,
                |ui| ui.get_rank_items(),
                format!("ranking rid={rid}"),
            )
            .await;
        });
    });
}

fn load_popular(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    browse_bvids: Arc<Mutex<Vec<String>>>,
    popular_state: Arc<Mutex<PopularState>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let page = page.max(1);
    if let Ok(mut state) = popular_state.lock() {
        state.page = page;
    }
    // One API page per screen (see `load_region`).
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_popular_current_page(page as i32);
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let card_page = match session.popular(page, ps).await {
                Ok(card_page) => card_page,
                Err(err) => {
                    crate::debug_log(&format!("popular err={err:#}"));
                    let message = format!("热门加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_popular_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<CardRow>::new(),
                            ))));
                            ui.set_popular_page_text(message.into());
                            ui.set_popular_has_prev(false);
                            ui.set_popular_has_next(false);
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let num = card_page.page;
            let has_more = card_page.has_more;
            let cards = card_page.items;
            let covers = cards.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, CardRowData)> = cards
                .iter()
                .map(|card| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        card.owner,
                        format_view(card.view),
                        format_duration(card.duration)
                    );
                    (
                        card.bvid.clone(),
                        CardRowData {
                            pixels: None,
                            title: card.title.clone(),
                            meta,
                            badge: format_duration(card.duration),
                        },
                    )
                })
                .collect();
            let loaded = rows.len();
            let weak_a = weak.clone();
            let browse_a = browse_bvids.clone();
            let popular_state_a = popular_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let items = publish_cards(&rows, &browse_a);
                    if let Ok(mut state) = popular_state_a.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    ui.set_popular_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_popular_page_text(format!("第 {num} 页").into());
                    ui.set_popular_current_page(num as i32);
                    ui.set_popular_has_prev(num > 1);
                    ui.set_popular_has_next(has_more);
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(10, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("popular load rendered items={loaded}"));
                }
            });
            stream_card_covers(
                &weak,
                &session,
                &cards,
                browse_bvids,
                |ui| ui.get_popular_items(),
                format!("popular page={num}"),
            )
            .await;
        });
    });
}

/// Load the 入站必刷 list (single fetch, up to 100 rows) into the card view.
fn load_precious(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    browse_bvids: Arc<Mutex<Vec<String>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_precious_status(String::new().into());
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let cards = match session.precious().await {
                Ok(cards) => cards,
                Err(err) => {
                    crate::debug_log(&format!("precious err={err:#}"));
                    let message = format!("入站必刷加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_precious_items(ModelRc::from(std::rc::Rc::new(
                                VecModel::from(Vec::<CardRow>::new()),
                            )));
                            ui.set_precious_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let covers = cards.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, CardRowData)> = cards
                .iter()
                .map(|card| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        card.owner,
                        format_view(card.view),
                        format_duration(card.duration)
                    );
                    (
                        card.bvid.clone(),
                        CardRowData {
                            pixels: None,
                            title: card.title.clone(),
                            meta,
                            badge: format_duration(card.duration),
                        },
                    )
                })
                .collect();
            let count = rows.len();
            let weak_a = weak.clone();
            let browse_a = browse_bvids.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let items = publish_cards(&rows, &browse_a);
                    ui.set_precious_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_precious_status(String::new().into());
                    // Single-shot list: the client page spans the loaded rows.
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("precious load rendered items={count}"));
                }
            });
            stream_card_covers(
                &weak,
                &session,
                &cards,
                browse_bvids,
                |ui| ui.get_precious_items(),
                "precious".to_string(),
            )
            .await;
        });
    });
}

/// Open the UP page for `mid`: open the overlay, then fetch the header card and
/// page 1 (播放最多) through [`load_up_page`].
///
/// Both entry points land here — a card's UP name (`open_up(index)`, after
/// [`resolve_card_mid`]) and a search 用户 row (`open_up_mid(mid)`) — so the page
/// can only be filled one way.
fn open_up_mid(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    mid: i64,
) {
    if mid <= 0 {
        crate::debug_log(&format!("up open skipped mid={mid} reason=no-mid"));
        return;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_up_page_open(true);
        ui.set_up_name(String::new().into());
        ui.set_up_fans(String::new().into());
        ui.set_up_videos_text(String::new().into());
        ui.set_up_sort(0);
        ui.set_up_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<FeedRow>::new(),
        ))));
    }
    if let Ok(mut store) = up_store.lock() {
        store.clear();
    }
    if let Ok(mut state) = up_page.lock() {
        *state = UpPageState {
            mid,
            ..UpPageState::default()
        };
    }
    load_up_page(
        weak,
        session_slot,
        up_page,
        up_seq,
        up_store,
        mid,
        0,
        true,
    );
}

/// Close the UP page and forget its page list, so a later index can never
/// resolve to a video of the closed page.
fn close_up_page(
    ui: &AppWindow,
    up_page: &Arc<Mutex<UpPageState>>,
    up_seq: &Arc<AtomicU64>,
    up_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
) {
    // Bump first: any load still in flight is superseded before its publish.
    up_seq.fetch_add(1, Ordering::SeqCst);
    ui.set_up_page_open(false);
    ui.set_up_sort(0);
    ui.set_up_name(String::new().into());
    ui.set_up_fans(String::new().into());
    ui.set_up_videos_text(String::new().into());
    ui.set_up_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<FeedRow>::new(),
    ))));
    if let Ok(mut store) = up_store.lock() {
        store.clear();
    }
    if let Ok(mut state) = up_page.lock() {
        *state = UpPageState::default();
    }
    crate::debug_log("up close");
}

/// `BILI_UP_PAGE_TEST`: wait (bounded) until page `sort` has been PUBLISHED,
/// then report how many rows the page list holds. Polling the Rust state instead
/// of the Slint model keeps the hook off the UI thread between callbacks.
fn up_test_wait_rows(
    up_page: &Arc<Mutex<UpPageState>>,
    up_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    sort: i32,
    limit: Duration,
) -> usize {
    let started = Instant::now();
    while started.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(200));
        let published = up_page
            .lock()
            .map(|state| state.published == Some(sort))
            .unwrap_or(false);
        if published {
            return up_store.lock().map(|page| page.len()).unwrap_or(0);
        }
    }
    0
}

/// Fetch one UP page (`sort` 0 = 播放最多, 1 = 最新发布) and publish it.
///
/// `announce` is the open path's `up open …` line; the per-load `up sort=… n=…`
/// line is always logged, so a sort switch is visible in the debug log too. The
/// 5 minute caches in `bilibili.rs` make a sort switch between the two orders
/// cheap, and a repeated switch a cache hit.
fn load_up_page(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    mid: i64,
    sort: i32,
    announce: bool,
) {
    let sort = if sort == 1 { 1 } else { 0 };
    let Some(session) = active_session(&session_slot) else {
        crate::debug_log(&format!("up open skipped mid={mid} reason=no-session"));
        return;
    };
    let my = up_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut state) = up_page.lock() {
        state.mid = mid;
        state.published = None;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_up_page_open(true);
        ui.set_up_sort(sort);
    }
    let order = if sort == 1 {
        bilibili::UpOrder::Newest
    } else {
        bilibili::UpOrder::Plays
    };
    // The header only changes when another UP主 is opened: a sort switch reuses
    // the card already in `up_page`, so it never spends a request on it.
    let need_card = announce
        || up_page
            .lock()
            .map(|state| state.mid != mid || state.name.is_empty())
            .unwrap_or(true);
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            // Header first: the card's own 5 minute cache makes a re-open free.
            let card = if need_card {
                match session.user_card(mid).await {
                    Ok(card) => Some(card),
                    Err(err) => {
                        crate::debug_log(&format!("up mid={mid} card err={err:#}"));
                        None
                    }
                }
            } else {
                None
            };
            let (items, has_more) = match session.up_videos(mid, order, 1).await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!(
                        "up mid={mid} order={} err={err:#}",
                        order.param()
                    ));
                    return;
                }
            };
            let name = card
                .as_ref()
                .map(|(name, _, _, _)| name.clone())
                .unwrap_or_default();
            if announce {
                let (avatar, fans, videos) = card
                    .as_ref()
                    .map(|(_, avatar, fans, videos)| (u8::from(!avatar.is_empty()), *fans, *videos))
                    .unwrap_or((0, 0, 0));
                crate::debug_log(&format!(
                    "up open mid={mid} name={name:?} items={} fans={fans} videos={videos} sort={sort} avatar={avatar}",
                    items.len()
                ));
            }
            crate::debug_log(&format!(
                "up sort={sort} n={} mid={mid} order={} more={}",
                items.len(),
                order.param(),
                u8::from(has_more)
            ));

            let row_count = items.len();
            let page = items.clone();
            let rows_weak = weak.clone();
            let rows_seq = up_seq.clone();
            let rows_store = up_store.clone();
            let rows_state = up_page.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if rows_seq.load(Ordering::SeqCst) != my {
                    return;
                }
                let Some(ui) = rows_weak.upgrade() else {
                    return;
                };
                if let Ok(mut guard) = rows_store.lock() {
                    *guard = page.clone();
                }
                let rows: Vec<FeedRow> = page.iter().map(build_feed_row_text).collect();
                if let Some(row) = rows.first() {
                    crate::debug_log(&format!(
                        "up row index=0 bvid={} pub_ts={} view_count={} owner_mid={}",
                        page.first().map(|video| video.bvid.as_str()).unwrap_or("-"),
                        row.pub_ts,
                        row.view_count,
                        row.owner_mid
                    ));
                }
                // Fold the freshly fetched header into the page state, then
                // publish the header from that state (a sort switch has none of
                // its own and republishes what is already there).
                if let Some((name, avatar, fans, videos)) = card {
                    if let Ok(mut state) = rows_state.lock() {
                        state.mid = mid;
                        state.name = name;
                        state.avatar = avatar;
                        state.fans = fans;
                        state.videos = videos;
                    }
                }
                let header = rows_state
                    .lock()
                    .map(|state| {
                        (
                            state.name.clone(),
                            state.fans,
                            state.videos,
                            u8::from(!state.avatar.is_empty()),
                        )
                    })
                    .unwrap_or_default();
                ui.set_up_name(header.0.into());
                ui.set_up_fans(format_view(header.1).into());
                ui.set_up_videos_text(format!("{} 个投稿", format_view(header.2)).into());
                ui.set_up_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
                ui.set_up_page_open(true);
                ui.set_up_sort(sort);
                if let Ok(mut state) = rows_state.lock() {
                    state.published = Some(sort);
                }
                crate::debug_log(&format!(
                    "up page rendered mid={mid} sort={sort} items={row_count} avatar={}",
                    header.3
                ));
            });

            // Covers stream in after the rows are live, through the same cache
            // and guard the feed uses; a closed or re-ordered page drops them.
            let covers = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let pics: Vec<String> = items.iter().take(covers).map(|item| item.pic.clone()).collect();
            let keys: Vec<String> = items
                .iter()
                .take(covers)
                .map(|item| item.bvid.clone())
                .collect();
            let guard_seq = up_seq.clone();
            let guard_store = up_store.clone();
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| {
                    ThumbGuard::GenFeed(guard_seq.clone(), my, guard_store.clone(), key)
                },
                |ui| ui.get_up_items(),
                |row, image| row.thumb = image,
                format!("up mid={mid} sort={sort}"),
            )
            .await;
        });
    });
}

/// The UP主 behind the card at `index` on the view that is on screen.
///
/// The view is read the same way `card_open` reads it, so a click on a card's UP
/// name can never open another view's row:
///   0  feed (`feed_store` carries the owner directly) — or, on the 收藏推荐 home
///      tab, the masonry's own list (`masonry`, `fav_videos`)
///   1  search, 视频 or 用户 tab (`SearchState::mids`, aligned with its rows)
///   4  one favourite folder (`FolderState::mids`)
///   5  history (`HistoryPageState::mids`, append-extended)
/// While the UP overlay is open, its own page list answers instead.
/// Views whose rows carry no owner mid (稍后再看, the discovery grids, the old
/// space view) answer `None` rather than guess.
#[allow(clippy::too_many_arguments)]
fn resolve_card_mid(
    view: i32,
    index: i32,
    up_open: bool,
    masonry: bool,
    up_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    fav_videos: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    search_state: &Arc<Mutex<SearchState>>,
    folder_state: &Arc<Mutex<FolderState>>,
    history_page: &Arc<Mutex<HistoryPageState>>,
) -> Option<i64> {
    let index = usize::try_from(index).ok()?;
    let mid_of = |mids: &[i64]| mids.get(index).copied().unwrap_or(0);
    let owner_at = |store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>| {
        store
            .lock()
            .ok()
            .and_then(|guard| guard.get(index).map(|video| video.owner_mid))
            .unwrap_or(0)
    };
    let mid = if up_open {
        owner_at(up_store)
    } else if view == 0 {
        if masonry {
            owner_at(fav_videos)
        } else {
            owner_at(feed_store)
        }
    } else {
        match view {
            1 => search_state.lock().map(|state| mid_of(&state.mids)).unwrap_or(0),
            4 => folder_state.lock().map(|state| mid_of(&state.mids)).unwrap_or(0),
            5 => history_page.lock().map(|state| mid_of(&state.mids)).unwrap_or(0),
            _ => 0,
        }
    };
    if mid > 0 {
        Some(mid)
    } else {
        None
    }
}

/// Open a UP主's space: switch to the space view and load profile + 投稿 page 1.
fn open_space(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
    space_prev_mode: Arc<Mutex<i64>>,
    space_bvids: Arc<Mutex<Vec<String>>>,
    mid: i64,
) {
    crate::debug_log(&format!("open_space mid={mid}"));
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        let prev = ui.get_view_mode();
        if prev != 11 {
            if let Ok(mut mode) = space_prev_mode.lock() {
                *mode = prev as i64;
            }
        }
        ui.set_view_mode(11);
        ui.set_space_status(String::new().into());
        ui.set_space_list_loading(true);
        ui.set_space_page_text(String::new().into());
        ui.set_space_has_prev(false);
        ui.set_space_has_next(false);
        ui.set_space_follow_busy(false);
        // The space grid is paged too (its own pager row walks the same
        // pages): enter at page 0 and re-publish the shared offset.
        set_client_page(11, 0);
        publish_list_offset(&ui);
    }
    if let Ok(mut state) = space_state.lock() {
        *state = SpaceState {
            mid,
            page: 1,
            has_more: false,
            attribute: 0,
        };
    }
    load_space_profile(weak.clone(), session_slot.clone(), space_state.clone());
    load_space_videos(weak, session_slot, space_state, space_bvids, 1);
}

/// Return from the space view to the view that opened it.
fn space_back(weak: Weak<AppWindow>, space_prev_mode: Arc<Mutex<i64>>) {
    let mode = space_prev_mode.lock().map(|mode| *mode).unwrap_or(0);
    let mode = if mode == 11 { 0 } else { mode };
    if let Some(ui) = weak.upgrade() {
        ui.set_view_mode(mode as i32);
        // Returning to where the user was: re-publish that section's offset
        // (the space pager moved the shared property) but keep its page.
        publish_list_offset(&ui);
        update_page_bar(&ui);
    }
}

/// Human label for a `x/relation` `attribute`.
fn follow_text(attribute: i64) -> String {
    match attribute {
        2 => "已关注".to_string(),
        6 => "互相关注".to_string(),
        _ => "关注".to_string(),
    }
}

fn load_space_profile(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let mid = match space_state.lock() {
        Ok(state) => state.mid,
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_space_loading(true);
        ui.set_space_name(String::new().into());
        ui.set_space_sign(String::new().into());
        ui.set_space_stats(String::new().into());
        ui.set_space_avatar(Image::default());
        ui.set_space_follow_status(String::new().into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let info = session.space_info(mid).await;
            let stat = session.relation(mid).await.ok();
            let attribute = session.relation_state(mid).await.unwrap_or(-1);
            let avatar = match &info {
                Ok(info) if !info.face.is_empty() => session.fetch_avatar(&info.face).await.ok(),
                _ => None,
            };
            let (name, sign, stats, status) = match info {
                Ok(info) => {
                    let sign = if info.sign.trim().is_empty() {
                        "（暂无签名）".to_string()
                    } else {
                        info.sign.clone()
                    };
                    let (fans, following) = match &stat {
                        Some(stat) => (stat.follower.max(0) as u64, stat.following.max(0) as u64),
                        None => (info.fans.max(0) as u64, info.friend.max(0) as u64),
                    };
                    let mut stats = format!(
                        "Lv{} · 粉丝 {} · 关注 {}",
                        info.level,
                        format_view(fans),
                        format_view(following),
                    );
                    if !info.official.is_empty() {
                        stats.push_str(&format!(" · {}", info.official));
                    }
                    if !info.vip.is_empty() {
                        stats.push_str(&format!(" · {}", info.vip));
                    }
                    (info.name, sign, stats, String::new())
                }
                Err(err) => {
                    crate::debug_log(&format!("space_info mid={mid} err={err:#}"));
                    (
                        String::new(),
                        String::new(),
                        String::new(),
                        format!("UP主信息加载失败：{err}"),
                    )
                }
            };
            let weak_profile = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_profile.upgrade() {
                    if let Some((bytes, w, h)) = avatar {
                        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
                        buffer.make_mut_bytes().copy_from_slice(&bytes);
                        ui.set_space_avatar(Image::from_rgba8(buffer));
                    }
                    if !name.is_empty() {
                        ui.set_space_name(name.into());
                    }
                    if !sign.is_empty() {
                        ui.set_space_sign(sign.into());
                    }
                    if !stats.is_empty() {
                        ui.set_space_stats(stats.into());
                    }
                    ui.set_space_loading(false);
                    ui.set_space_status(status.into());
                }
            });
            if attribute >= 0 {
                if let Ok(mut state) = space_state.lock() {
                    state.attribute = attribute;
                }
                let weak_attr = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_attr.upgrade() {
                        ui.set_space_follow_state(attribute as i32);
                        ui.set_space_follow_text(follow_text(attribute).into());
                    }
                });
            }
        });
    });
}

fn load_space_videos(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
    space_bvids: Arc<Mutex<Vec<String>>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (mid, page) = match space_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.mid, state.page)
        }
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    // One API page per screen: the space grid is client-paged too, and its own
    // pager row walks the same pages.
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        ui.set_space_list_loading(true);
        ui.set_space_current_page(page as i32);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = session.space_videos(mid, page, ps).await;
            let (rows, bvids, num, has_more, status, pics, covers) = match result {
                Ok(page_data) => {
                    let items = page_data.items;
                    let pics: Vec<String> = items.iter().map(|item| item.pic.clone()).collect();
                    let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
                    let rows: Vec<(String, CardRowData)> = items
                        .iter()
                        .map(|item| {
                            let meta = format!(
                                "{}播放 · {}弹幕 · {}",
                                format_view(item.play),
                                format_view(item.danmaku),
                                item.length
                            );
                            (
                                item.bvid.clone(),
                                CardRowData {
                                    pixels: None,
                                    title: item.title.clone(),
                                    meta,
                                    badge: item.length.clone(),
                                },
                            )
                        })
                        .collect();
                    let bvids: Vec<String> = rows.iter().map(|(bvid, _)| bvid.clone()).collect();
                    (
                        rows,
                        bvids,
                        page_data.page,
                        page_data.has_more,
                        String::new(),
                        pics,
                        covers,
                    )
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "space_videos mid={mid} page={page} err={err:#}"
                    ));
                    (
                        Vec::new(),
                        Vec::new(),
                        page,
                        false,
                        format!("投稿列表加载失败：{err}"),
                        Vec::new(),
                        0,
                    )
                }
            };
            let loaded = rows.len();
            let keys: Vec<String> = bvids.iter().take(covers).cloned().collect();
            let mut pics = pics;
            pics.truncate(covers);
            let weak_a = weak.clone();
            let space_bvids_a = space_bvids.clone();
            let space_state_a = space_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut guard) = space_bvids_a.lock() {
                        *guard = bvids;
                    }
                    if let Ok(mut state) = space_state_a.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    let items: Vec<CardRow> =
                        rows.iter().map(|(_, row)| build_card_row(row)).collect();
                    ui.set_space_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_space_list_loading(false);
                    ui.set_space_page_text(format!("第 {num} 页").into());
                    ui.set_space_current_page(num as i32);
                    ui.set_space_has_prev(num > 1);
                    ui.set_space_has_next(has_more);
                    if !status.is_empty() {
                        ui.set_space_status(status.into());
                    }
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(11, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    crate::debug_log(&format!("space load rendered items={loaded}"));
                }
            });
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| ThumbGuard::Ids(space_bvids.clone(), key),
                |ui| ui.get_space_items(),
                |row, image| row.thumb = image,
                format!("space mid={mid} page={num}"),
            )
            .await;
        });
    });
}

fn toggle_space_follow(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let mid = match space_state.lock() {
        Ok(state) => state.mid,
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        if ui.get_space_follow_busy() {
            return;
        }
        ui.set_space_follow_busy(true);
        ui.set_space_follow_status(String::new().into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let current = space_state.lock().map(|state| state.attribute).unwrap_or(0);
            let act = if current == 2 || current == 6 { 2 } else { 1 };
            let result = session.relation_modify(mid, act).await;
            let (attribute, message) = match result {
                Ok(()) => {
                    let attribute = session
                        .relation_state(mid)
                        .await
                        .unwrap_or(if act == 1 { 2 } else { 0 });
                    (attribute, format!("操作成功（act={act}）"))
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "relation_modify mid={mid} act={act} err={err:#}"
                    ));
                    (current, format!("操作失败：{err}"))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut state) = space_state.lock() {
                        state.attribute = attribute;
                    }
                    ui.set_space_follow_state(attribute as i32);
                    ui.set_space_follow_text(follow_text(attribute).into());
                    ui.set_space_follow_status(message.into());
                    ui.set_space_follow_busy(false);
                }
            });
        });
    });
}

/// Log the account out server-side (only while the login is live), clear the
/// saved record, and return to the login page.
fn logout_account(weak: Weak<AppWindow>, generation: Arc<AtomicU64>, session_slot: SessionSlot) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_status("没有可退出的账号".into());
        }
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_status("正在退出登录…".into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let live = matches!(
                session.nav().await,
                Ok(bilibili::NavOutcome::LoggedIn(_))
            );
            let status = if live {
                match session.logout().await {
                    Ok(()) => "已退出登录".to_string(),
                    Err(err) => {
                        crate::debug_log(&format!("logout err={err:#}"));
                        format!("退出请求失败（已清除本地账号）：{err}")
                    }
                }
            } else {
                "登录已失效，已清除本地账号".to_string()
            };
            accounts::clear();
            if let Ok(mut guard) = session_slot.lock() {
                *guard = None;
            }
            let my = generation.fetch_add(1, Ordering::SeqCst) + 1;
            crate::debug_log(&format!("logout cleared saved record status={status}"));
            crate::debug_log("identity cleared reason=logout");
            post(&weak, &generation, my, move |ui| {
                ui.set_logged_in(false);
                ui.set_qr_image(Image::default());
                ui.set_saved_account_name("".into());
                ui.set_user_name("".into());
                ui.set_user_id("".into());
                ui.set_login_state(0);
                ui.set_login_step("请选择登录方式".into());
                ui.set_login_percent(0);
                ui.set_login_progress(0.0);
                ui.set_login_failed(false);
                ui.set_login_error("".into());
                ui.set_login_countdown("".into());
                ui.set_status(status.into());
            });
        });
    });
}

/// Debounced (140 ms) suggestion lookup; a newer keystroke makes this inert.
fn load_suggestions(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    suggest_seq: Arc<AtomicU64>,
    text: String,
) {
    let seq = suggest_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let keyword = text.trim().to_string();
    if keyword.is_empty() {
        if let Some(ui) = weak.upgrade() {
            ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                Vec::<SharedString>::new(),
            ))));
        }
        return;
    }
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            tokio::time::sleep(Duration::from_millis(140)).await;
            if suggest_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            let items = session.search_suggest(&keyword).await.unwrap_or_default();
            if suggest_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            let _ = slint::invoke_from_event_loop(move || {
                if suggest_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    let list: Vec<SharedString> =
                        items.into_iter().map(SharedString::from).collect();
                    ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(list))));
                }
            });
        });
    });
}

/// Fetch the 热搜 list (display names + search keywords) for the results view.
fn load_hot_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    hot_keywords: Arc<Mutex<Vec<String>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let list = session.hot_search().await.unwrap_or_default();
            let display: Vec<SharedString> = list
                .iter()
                .map(|hot| SharedString::from(hot.show_name.clone()))
                .collect();
            let keywords: Vec<String> = list.into_iter().map(|hot| hot.keyword).collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut guard) = hot_keywords.lock() {
                        *guard = keywords;
                    }
                    ui.set_hot_items(ModelRc::from(std::rc::Rc::new(VecModel::from(display))));
                }
            });
        });
    });
}

/// Start a fresh video search: switch to the results view, clear suggestions
/// and refresh the 热搜 strip.
fn begin_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_seq: Arc<AtomicU64>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    hot_keywords: Arc<Mutex<Vec<String>>>,
    keyword: String,
) {
    let keyword = keyword.trim().to_string();
    if keyword.is_empty() {
        return;
    }
    if let Ok(mut state) = search_state.lock() {
        state.keyword = keyword.clone();
        state.tab = 0;
        state.page = 1;
        state.num_pages = 1;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_search_text(keyword.clone().into());
        ui.set_view_mode(1);
        enter_paged_view(&ui);
        ui.set_search_tab(0);
        ui.set_search_status(String::new().into());
        ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<SharedString>::new(),
        ))));
    }
    run_search(
        weak.clone(),
        session_slot.clone(),
        search_state,
        search_seq,
        search_bvids,
    );
    load_hot_search(weak, session_slot, hot_keywords);
}

fn active_parts(session_slot: &SessionSlot) -> Option<(Arc<bilibili::Session>, i64)> {
    let session = active_session(session_slot)?;
    let mid = accounts::load().map(|account| account.mid).unwrap_or(0);
    Some((session, mid))
}

fn load_fav_folders(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    fav_folders: Arc<Mutex<Vec<bilibili::FavFolder>>>,
) {
    let Some((session, _mid)) = active_parts(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_fav_list_loading(true);
        ui.set_fav_list_status(String::new().into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = session.fav_folders().await;
            let (rows, status) = match result {
                Ok(folders) => {
                    let rows: Vec<FavFolderRow> = folders
                        .iter()
                        .map(|folder| FavFolderRow {
                            fid: folder.id.to_string().into(),
                            title: folder.name.clone().into(),
                            count: format!("{} 个内容", folder.count).into(),
                        })
                        .collect();
                    if let Ok(mut guard) = fav_folders.lock() {
                        *guard = folders;
                    }
                    (rows, String::new())
                }
                Err(err) => {
                    crate::debug_log(&format!("fav_folders err={err:#}"));
                    (Vec::new(), format!("收藏夹加载失败：{err}"))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_fav_folder_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
                    ui.set_fav_list_loading(false);
                    ui.set_fav_list_status(status.into());
                }
            });
        });
    });
}

// ---------------------------------------------------------------------------
// 收藏推荐: the mixed favourite-folder masonry (home_tab 5)
//
// The shipped UI owns 全部 and only lists the user's own folders:
// `fav_tabs[i]` is folder `i`'s name and `fav_tab == i + 1` (`0` = 全部, see
// `ui/app.slint`). The names live on in `FavViewState.folders`, which is what
// maps a tab back to the folder's `media_id`.
//
// Tab 0 is a Pinterest-style mix: the largest folders' first page, two random
// picks per folder expanded through `related()`, merged, deduped, shuffled and
// capped. Both the mix and each folder tab are cached in-process for
// [`FAV_VIEW_TTL`], so switching back inside the window costs no network.
// ---------------------------------------------------------------------------

/// How long a built 收藏推荐 tab (mixed or per folder) stays valid.
const FAV_VIEW_TTL: Duration = Duration::from_secs(5 * 60);
/// Folders the mixed stream draws from (largest first).
const FAV_MIX_MAX_FOLDERS: usize = 8;
/// `related()` calls the mixed stream may issue (2 picks × 8 folders).
const FAV_MIX_RELATED: usize = 16;
/// Network work in flight while building a tab.
const FAV_MIX_CONCURRENCY: usize = 4;
/// Cards a tab publishes at most.
const FAV_CARDS_MAX: usize = 45;
/// The neutral 16:9 height/width ratio the card falls back to.
const FAV_DEFAULT_ASPECT: f32 = 0.5625;

/// UI-side truth for the 收藏推荐 view. The generation is bumped by every load
/// request, so a worker that finishes after a tab switch is dropped.
struct FavViewState {
    /// Published order: `fav_tabs[i]` is `folders[i].name`, tab `i + 1`.
    folders: Vec<bilibili::FavFolder>,
    /// Tab the last load asked for (`0` = 全部, `i + 1` = `folders[i]`).
    tab: i32,
    /// The tab whose cards are on screen; `None` while a load is in flight.
    published: Option<i32>,
    /// A load is in flight (the QA hook polls this).
    loading: bool,
    /// The folder fetch is in flight / has completed at least once.
    folders_loading: bool,
    folders_loaded: bool,
    generation: Arc<AtomicU64>,
    /// The videos behind the current `masonry`, for `masonry_open`.
    videos: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
}

impl FavViewState {
    fn new() -> Self {
        Self {
            folders: Vec::new(),
            tab: 0,
            published: None,
            loading: false,
            folders_loading: false,
            folders_loaded: false,
            generation: Arc::new(AtomicU64::new(0)),
            videos: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// One built tab, with the count of folders that contributed to a mix.
#[derive(Clone)]
struct FavCacheEntry {
    at: Instant,
    cards: Vec<bilibili::FeedVideo>,
    sources: usize,
}

#[derive(Default)]
struct FavViewCache {
    /// Folder ids the mixed entry was built from: a different set invalidates it.
    mix_key: Vec<i64>,
    mixed: Option<FavCacheEntry>,
    folders: HashMap<i64, FavCacheEntry>,
}

static FAV_VIEW_CACHE: OnceLock<Mutex<FavViewCache>> = OnceLock::new();

fn fav_view_cache() -> &'static Mutex<FavViewCache> {
    FAV_VIEW_CACHE.get_or_init(|| Mutex::new(FavViewCache::default()))
}

fn fav_mixed_cached(key: &[i64]) -> Option<FavCacheEntry> {
    let guard = fav_view_cache().lock().ok()?;
    if guard.mix_key != key {
        return None;
    }
    let entry = guard.mixed.as_ref()?;
    if entry.at.elapsed() >= FAV_VIEW_TTL {
        return None;
    }
    Some(entry.clone())
}

fn fav_folder_cached(media_id: i64) -> Option<FavCacheEntry> {
    let guard = fav_view_cache().lock().ok()?;
    let entry = guard.folders.get(&media_id)?;
    if entry.at.elapsed() >= FAV_VIEW_TTL {
        return None;
    }
    Some(entry.clone())
}

fn fav_mixed_store(key: Vec<i64>, cards: &[bilibili::FeedVideo], sources: usize) {
    if let Ok(mut guard) = fav_view_cache().lock() {
        guard.folders.retain(|_, entry| entry.at.elapsed() < FAV_VIEW_TTL);
        guard.mix_key = key;
        guard.mixed = Some(FavCacheEntry {
            at: Instant::now(),
            cards: cards.to_vec(),
            sources,
        });
    }
}

fn fav_folder_store(media_id: i64, cards: &[bilibili::FeedVideo]) {
    if let Ok(mut guard) = fav_view_cache().lock() {
        guard.folders.retain(|_, entry| entry.at.elapsed() < FAV_VIEW_TTL);
        guard.folders.insert(
            media_id,
            FavCacheEntry {
                at: Instant::now(),
                cards: cards.to_vec(),
                sources: 1,
            },
        );
    }
}

/// splitmix64 step. Same generator the data layer uses; the crate has no `rand`
/// dependency and this is only used for shuffling, not for anything secret.
fn fav_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Clock seed, so repeated visits to 全部 sample different videos.
fn fav_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ 0x9E37_79B9_7F4A_7C15
}

fn fav_shuffle<T>(items: &mut [T]) {
    let mut state = fav_seed();
    for index in (1..items.len()).rev() {
        let pick = (fav_rand(&mut state) % (index as u64 + 1)) as usize;
        items.swap(index, pick);
    }
}

/// Two distinct picks from one folder's page (one when it holds a single item).
fn fav_pick_two(items: &[bilibili::FeedVideo]) -> Vec<bilibili::FeedVideo> {
    let mut picks = Vec::with_capacity(2);
    if items.is_empty() {
        return picks;
    }
    let mut state = fav_seed();
    let len = items.len();
    let first = (fav_rand(&mut state) % len as u64) as usize;
    picks.push(items[first].clone());
    if len > 1 {
        let second = (fav_rand(&mut state) % len as u64) as usize;
        let second = if second == first {
            (second + 1) % len
        } else {
            second
        };
        picks.push(items[second].clone());
    }
    picks
}

/// Append `items` to `target`, skipping any bvid already in the list.
fn fav_push_unique(
    target: &mut Vec<bilibili::FeedVideo>,
    seen: &mut HashSet<String>,
    items: &[bilibili::FeedVideo],
) {
    for video in items {
        if video.bvid.is_empty() {
            continue;
        }
        if seen.insert(video.bvid.clone()) {
            target.push(video.clone());
        }
    }
}

/// The mixed stream's folder set: largest first, capped.
fn fav_largest_folders(folders: &[bilibili::FavFolder]) -> Vec<bilibili::FavFolder> {
    let mut picked = folders.to_vec();
    picked.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.id.cmp(&b.id)));
    picked.truncate(FAV_MIX_MAX_FOLDERS);
    picked
}

/// Height/width of a decoded cover, or the neutral ratio when a dimension is
/// unusable.
fn fav_aspect(pixels: &CoverPixels) -> f32 {
    let (_, width, height) = pixels;
    if *width == 0 || *height == 0 {
        FAV_DEFAULT_ASPECT
    } else {
        *height as f32 / *width as f32
    }
}

fn fav_aspect_stats(rows: &[MasonryCard]) -> (f32, f32, f32) {
    if rows.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f32;
    for row in rows {
        min = min.min(row.aspect);
        max = max.max(row.aspect);
        sum += row.aspect;
    }
    (min, sum / rows.len() as f32, max)
}

async fn fav_fetch_pages(
    session: &Arc<bilibili::Session>,
    folders: &[bilibili::FavFolder],
) -> Vec<(Vec<bilibili::FeedVideo>, Option<String>)> {
    let fetches = folders.iter().map(|folder| {
        let session = session.clone();
        let media_id = folder.id;
        async move {
            match session.fav_items(media_id, 1).await {
                Ok((items, _)) => (items, None),
                Err(err) => (
                    Vec::new(),
                    Some(format!("folder {media_id}: {err:#}")),
                ),
            }
        }
    });
    let mut stream = futures::stream::iter(fetches).buffer_unordered(FAV_MIX_CONCURRENCY);
    let mut pages = Vec::new();
    while let Some(page) = stream.next().await {
        pages.push(page);
    }
    pages
}

/// `related()` for every pick, at most 16 calls and 4 in flight; failures are
/// skipped (an empty list).
async fn fav_related_batch(
    session: &Arc<bilibili::Session>,
    picks: &[bilibili::FeedVideo],
) -> Vec<Vec<bilibili::FeedVideo>> {
    let fetches = picks.iter().take(FAV_MIX_RELATED).map(|video| {
        let session = session.clone();
        let bvid = video.bvid.clone();
        async move {
            match session.related(&bvid).await {
                Ok(items) => items,
                Err(_) => Vec::new(),
            }
        }
    });
    let mut stream = futures::stream::iter(fetches).buffer_unordered(FAV_MIX_CONCURRENCY);
    let mut related = Vec::new();
    while let Some(items) = stream.next().await {
        related.push(items);
    }
    related
}

/// Build the mixed 全部 stream. Returns the cards, how many folders contributed
/// and the first folder error (only used when nothing at all came back).
async fn fav_build_mixed(
    session: &Arc<bilibili::Session>,
    folders: &[bilibili::FavFolder],
) -> (Vec<bilibili::FeedVideo>, usize, Option<String>) {
    let picked = fav_largest_folders(folders);
    let pages = fav_fetch_pages(session, &picked).await;
    let mut sources = 0usize;
    let mut first_error: Option<String> = None;
    let mut cards: Vec<bilibili::FeedVideo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut picks: Vec<bilibili::FeedVideo> = Vec::new();
    for (items, error) in &pages {
        if let Some(error) = error {
            if first_error.is_none() {
                first_error = Some(error.clone());
            }
        }
        if items.is_empty() {
            continue;
        }
        sources += 1;
        fav_push_unique(&mut cards, &mut seen, items);
        picks.extend(fav_pick_two(items));
    }
    picks.truncate(FAV_MIX_RELATED);
    for related in fav_related_batch(session, &picks).await {
        fav_push_unique(&mut cards, &mut seen, &related);
    }
    fav_shuffle(&mut cards);
    cards.truncate(FAV_CARDS_MAX);
    (cards, sources, first_error)
}

/// Build one folder tab: its own page plus two picks' related videos.
async fn fav_build_folder(
    session: &Arc<bilibili::Session>,
    media_id: i64,
) -> Result<Vec<bilibili::FeedVideo>> {
    let (items, _) = session.fav_items(media_id, 1).await?;
    let picks = fav_pick_two(&items);
    let related = fav_related_batch(session, &picks).await;
    let mut cards: Vec<bilibili::FeedVideo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    fav_push_unique(&mut cards, &mut seen, &items);
    for list in &related {
        fav_push_unique(&mut cards, &mut seen, list);
    }
    fav_shuffle(&mut cards);
    cards.truncate(FAV_CARDS_MAX);
    Ok(cards)
}

/// One card's cover still to arrive.
struct FavPendingCover {
    index: usize,
    pic: String,
    bvid: String,
}

/// Replace one masonry row's cover in place (never re-publishes the list).
fn patch_masonry_row(model: ModelRc<MasonryCard>, index: usize, image: Image, aspect: f32) {
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<MasonryCard>>() {
        if let Some(mut row) = vec_model.row_data(index) {
            row.cover = image;
            row.aspect = aspect;
            vec_model.set_row_data(index, row);
        }
    }
}

/// Publish `videos` as the masonry, taking every already-decoded cover (and its
/// real aspect) from the shared cache. Returns how many covers were warm, the
/// rows still to stream and the published aspect range.
fn fav_publish(
    ui: &AppWindow,
    view: &Arc<Mutex<FavViewState>>,
    tab: i32,
    videos: Vec<bilibili::FeedVideo>,
) -> (usize, Vec<FavPendingCover>, (f32, f32, f32)) {
    let warm: Vec<Option<Arc<CoverPixels>>> = match cover_cache().lock() {
        Ok(mut cache) => videos
            .iter()
            .map(|video| {
                if video.pic.is_empty() {
                    None
                } else {
                    cache.get(&video.pic)
                }
            })
            .collect(),
        Err(_) => videos.iter().map(|_| None).collect(),
    };
    let mut rows = Vec::with_capacity(videos.len());
    let mut pending = Vec::new();
    let mut real = 0usize;
    for (index, video) in videos.iter().enumerate() {
        let (cover, aspect) = match warm.get(index).and_then(|hit| hit.as_ref()) {
            Some(pixels) => {
                real += 1;
                (to_thumb_shared(pixels), fav_aspect(pixels))
            }
            None => {
                if !video.pic.is_empty() {
                    pending.push(FavPendingCover {
                        index,
                        pic: video.pic.clone(),
                        bvid: video.bvid.clone(),
                    });
                }
                (Image::default(), FAV_DEFAULT_ASPECT)
            }
        };
        rows.push(MasonryCard {
            bvid: video.bvid.clone().into(),
            title: video.title.clone().into(),
            owner: video.owner.clone().into(),
            duration: format_duration(video.duration).into(),
            cover,
            aspect,
        });
    }
    let stats = fav_aspect_stats(&rows);
    if let Ok(mut state) = view.lock() {
        if let Ok(mut store) = state.videos.lock() {
            *store = videos;
        }
        state.published = Some(tab);
        state.loading = false;
    }
    ui.set_masonry(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    (real, pending, stats)
}

/// Empty the masonry while a load runs: the UI paints its skeleton blocks.
fn fav_clear_cards(ui: &AppWindow, view: &Arc<Mutex<FavViewState>>) {
    ui.set_masonry(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<MasonryCard>::new(),
    ))));
    if let Ok(mut state) = view.lock() {
        if let Ok(mut store) = state.videos.lock() {
            store.clear();
        }
        state.published = None;
    }
}

/// Publish the genuine empty state (no folders / a folder with nothing in it).
fn fav_publish_empty(ui: &AppWindow, view: &Arc<Mutex<FavViewState>>, tab: i32) {
    fav_clear_cards(ui, view);
    if let Ok(mut state) = view.lock() {
        state.published = Some(tab);
        state.loading = false;
    }
}

fn fav_publish_tabs(ui: &AppWindow, folders: &[bilibili::FavFolder]) {
    let names: Vec<SharedString> = folders
        .iter()
        .map(|folder| folder.name.clone().into())
        .collect();
    ui.set_fav_tabs(ModelRc::from(std::rc::Rc::new(VecModel::from(names))));
}

fn fav_log_tab(
    tab: i32,
    folder: Option<i64>,
    cards: usize,
    real: usize,
    aspect: (f32, f32, f32),
    ms: u128,
    hit: bool,
    sources: usize,
) {
    let folder = folder
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    crate::debug_log(&format!(
        "fav view tab={tab} folder={folder} cards={cards} real_aspect={real}/{cards} sources={sources} ms={ms} hit={} aspect={:.3}/{:.3}/{:.3}",
        u8::from(hit),
        aspect.0,
        aspect.1,
        aspect.2
    ));
}

fn fav_log_mix(cards: usize, real: usize, sources: usize, ms: u128, hit: bool) {
    crate::debug_log(&format!(
        "fav mix cards={cards} real_aspect={real}/{cards} sources={sources} ms={ms} hit={}",
        u8::from(hit)
    ));
}

/// Stream a published tab's missing covers and patch each row as it decodes.
fn fav_spawn_covers(
    weak: Weak<AppWindow>,
    session: Option<Arc<bilibili::Session>>,
    generation: Arc<AtomicU64>,
    store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    my: u64,
    tab: i32,
    pending: Vec<FavPendingCover>,
) {
    let Some(session) = session else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        let began = Instant::now();
        let total = pending.len();
        let pics: Vec<String> = pending.iter().map(|cover| cover.pic.clone()).collect();
        let indices: Vec<usize> = pending.iter().map(|cover| cover.index).collect();
        let bvids: Vec<String> = pending.iter().map(|cover| cover.bvid.clone()).collect();
        let mut delivered = 0usize;
        runtime.block_on(async {
            stream_thumbs(&session, pics, |_| {}, |slot, pixels| {
                delivered += 1;
                let Some(pixels) = pixels else {
                    return;
                };
                let index = indices.get(slot).copied().unwrap_or(usize::MAX);
                let bvid = bvids.get(slot).cloned().unwrap_or_default();
                let guard = ThumbGuard::Fav(generation.clone(), my, store.clone(), bvid);
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        if guard.allows(index) {
                            // Decode on the UI thread: `Image` is not `Send`.
                            let aspect = fav_aspect(&pixels);
                            patch_masonry_row(ui.get_masonry(), index, to_thumb(&Some(pixels)), aspect);
                        }
                    }
                });
            })
            .await;
        });
        crate::debug_log(&format!(
            "fav covers n={}/{} ms={} tab={tab}",
            delivered,
            total,
            began.elapsed().as_millis()
        ));
    });
}

/// Load one tab (respecting both caches) and publish it. Must run on the UI
/// thread: it reads the folder list, checks the cache and touches the model.
fn fav_load_tab(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    view: Arc<Mutex<FavViewState>>,
    tab: i32,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let (folders, generation, store, my, tab) = {
        let Ok(mut state) = view.lock() else {
            return;
        };
        let tab = tab.max(0);
        let tab = if tab as usize > state.folders.len() {
            0
        } else {
            tab
        };
        state.tab = tab;
        state.loading = true;
        let my = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
        (
            state.folders.clone(),
            state.generation.clone(),
            state.videos.clone(),
            my,
            tab,
        )
    };
    ui.set_fav_tab(tab);

    let folder = if tab == 0 {
        None
    } else {
        folders.get((tab - 1) as usize).cloned()
    };
    if tab > 0 && folder.is_none() {
        fav_publish_empty(&ui, &view, tab);
        crate::debug_log("fav view empty reason=no_folders");
        return;
    }

    let key: Vec<i64> = folders.iter().map(|folder| folder.id).collect();
    let cached = match &folder {
        Some(folder) => fav_folder_cached(folder.id),
        None => fav_mixed_cached(&key),
    };
    if let Some(entry) = cached {
        let cards = entry.cards;
        let total = cards.len();
        let folder_id = folder.as_ref().map(|folder| folder.id);
        let (real, pending, aspect) = fav_publish(&ui, &view, tab, cards);
        fav_log_tab(tab, folder_id, total, real, aspect, 0, true, entry.sources);
        if folder.is_none() {
            fav_log_mix(total, real, entry.sources, 0, true);
        }
        fav_spawn_covers(
            weak,
            active_session(&session_slot),
            generation,
            store,
            my,
            tab,
            pending,
        );
        return;
    }

    // Cache miss: clear first, so no row from the previous tab survives.
    fav_clear_cards(&ui, &view);
    let Some(session) = active_session(&session_slot) else {
        fav_publish_empty(&ui, &view, tab);
        crate::debug_log("fav view empty reason=error:no session");
        return;
    };
    let key_c = key;
    let folder_c = folder.clone();
    let weak_w = weak.clone();
    let view_w = view.clone();
    let generation_w = generation.clone();
    let store_w = store.clone();
    std::thread::spawn(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_w.upgrade() {
                    fav_publish_empty(&ui, &view_w, tab);
                }
            });
            return;
        };
        let began = Instant::now();
        let (cards, sources, error) = if let Some(folder) = folder_c.as_ref() {
            match runtime.block_on(fav_build_folder(&session, folder.id)) {
                Ok(cards) => (cards, 1usize, None),
                Err(err) => (Vec::new(), 0usize, Some(format!("{err:#}"))),
            }
        } else {
            runtime.block_on(fav_build_mixed(&session, &folders))
        };
        let ms = began.elapsed().as_millis();
        if !cards.is_empty() {
            match folder_c.as_ref() {
                Some(folder) => fav_folder_store(folder.id, &cards),
                None => fav_mixed_store(key_c, &cards, sources),
            }
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak_w.upgrade() else {
                return;
            };
            if generation_w.load(Ordering::SeqCst) != my {
                return;
            }
            if view_w.lock().map(|state| state.tab).unwrap_or(-1) != tab {
                return;
            }
            if cards.is_empty() {
                fav_publish_empty(&ui, &view_w, tab);
                match &error {
                    Some(error) => {
                        crate::debug_log(&format!("fav view empty reason=error:{error}"))
                    }
                    None => crate::debug_log(&format!(
                        "fav view tab={tab} cards=0 ms={ms} hit=0"
                    )),
                }
                return;
            }
            let total = cards.len();
            let folder_id = folder_c.as_ref().map(|folder| folder.id);
            let (real, pending, aspect) = fav_publish(&ui, &view_w, tab, cards);
            fav_log_tab(tab, folder_id, total, real, aspect, ms, false, sources);
            if folder_c.is_none() {
                fav_log_mix(total, real, sources, ms, false);
            }
            fav_spawn_covers(
                weak_w.clone(),
                Some(session),
                generation_w.clone(),
                store_w.clone(),
                my,
                tab,
                pending,
            );
        });
    });
}

/// UI thread: fetch the folders (10 min cached in the data layer), publish the
/// tab strip and load the tab the view is on.
fn fav_fetch_folders(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    view: Arc<Mutex<FavViewState>>,
) {
    {
        let Ok(mut state) = view.lock() else {
            return;
        };
        if state.folders_loading {
            return;
        }
        state.folders_loading = true;
    }
    std::thread::spawn(move || {
        let mut session = None;
        for _ in 0..240 {
            session = active_session(&session_slot);
            if session.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let slot = session_slot.clone();
        let Some(session) = session else {
            let weak = weak.clone();
            let view = view.clone();
            let _ = slint::invoke_from_event_loop(move || {
                fav_apply_folders(
                    &weak,
                    &slot,
                    &view,
                    Vec::new(),
                    Some("no session".to_string()),
                );
            });
            return;
        };
        let result = match bilibili::shared_runtime() {
            Some(runtime) => runtime.block_on(session.fav_folders()),
            None => Err(anyhow!("no runtime")),
        };
        let (folders, error) = match result {
            Ok(folders) => (folders, None),
            Err(err) => {
                crate::debug_log(&format!("fav view folders err={err:#}"));
                (Vec::new(), Some(format!("{err:#}")))
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            fav_apply_folders(&weak, &slot, &view, folders, error);
        });
    });
}

/// UI thread: publish the folder list (and its strip) and reload when the set
/// changed or the shown tab is not the requested one.
fn fav_apply_folders(
    weak: &Weak<AppWindow>,
    session_slot: &SessionSlot,
    view: &Arc<Mutex<FavViewState>>,
    folders: Vec<bilibili::FavFolder>,
    error: Option<String>,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let (folders, tab, published, changed) = {
        let Ok(mut state) = view.lock() else {
            return;
        };
        state.folders_loaded = true;
        state.folders_loading = false;
        let had = !state.folders.is_empty();
        let changed = state
            .folders
            .iter()
            .map(|folder| folder.id)
            .ne(folders.iter().map(|folder| folder.id));
        // A failed refetch keeps the list the strip is already showing.
        if !folders.is_empty() || !had {
            state.folders = folders;
        }
        if state.tab > state.folders.len() as i32 {
            state.tab = 0;
            ui.set_fav_tab(0);
        }
        (
            state.folders.clone(),
            state.tab,
            state.published,
            changed,
        )
    };

    fav_publish_tabs(&ui, &folders);
    if error.is_none() {
        crate::debug_log(&format!("fav view folders n={}", folders.len()));
    }
    if folders.is_empty() {
        fav_publish_empty(&ui, view, tab);
        match &error {
            Some(error) => crate::debug_log(&format!("fav view empty reason=error:{error}")),
            None => crate::debug_log("fav view empty reason=no_folders"),
        }
        return;
    }
    if changed || published != Some(tab) {
        fav_load_tab(weak.clone(), session_slot.clone(), view.clone(), tab);
    }
}

/// Enter 收藏推荐: publish the known strip, load the tab already selected and
/// refresh the folders in the background.
fn fav_start(weak: Weak<AppWindow>, session_slot: SessionSlot, view: Arc<Mutex<FavViewState>>) {
    let (folders, tab, published) = match view.lock() {
        Ok(state) => (state.folders.clone(), state.tab, state.published),
        Err(_) => return,
    };
    if !folders.is_empty() {
        if let Some(ui) = weak.upgrade() {
            fav_publish_tabs(&ui, &folders);
        }
        if published != Some(tab) {
            fav_load_tab(weak.clone(), session_slot.clone(), view.clone(), tab);
        }
    }
    fav_fetch_folders(weak, session_slot, view);
}

/// `BILI_FAV_VIEW_TEST=<tab|all>`: the tabs the hook drives. `all` visits 全部
/// and every folder.
fn fav_view_test_tabs(spec: &str, folders: usize) -> Vec<i32> {
    if spec.trim().eq_ignore_ascii_case("all") {
        (0..=folders as i32).collect()
    } else {
        let tab = spec.trim().parse::<i32>().unwrap_or(0).max(0);
        vec![tab.min(folders as i32)]
    }
}

fn fav_wait_for<F>(view: &Arc<Mutex<FavViewState>>, timeout: Duration, predicate: F) -> bool
where
    F: Fn(&FavViewState) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if view
            .lock()
            .map(|state| predicate(&state))
            .unwrap_or(false)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Backend-driven tab switch: sets `fav_tab` and fires the same callback the
/// chip does, so the real load path runs without synthetic input.
fn fav_test_drive(weak: &Weak<AppWindow>, tab: i32, label: &str) {
    let label = label.to_string();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            crate::debug_log(&format!("fav view test tab={tab} drive={label}"));
            ui.set_fav_tab(tab);
            ui.invoke_fav_tab_changed(tab);
        }
    });
}

/// Drive one tab and wait for the load it queued: the generation must advance
/// past `before` (proving the callback ran) and the tab must be published.
fn fav_test_drive_and_wait(
    weak: &Weak<AppWindow>,
    view: &Arc<Mutex<FavViewState>>,
    tab: i32,
    label: &str,
) -> bool {
    let before = view
        .lock()
        .map(|state| state.generation.load(Ordering::SeqCst))
        .unwrap_or(0);
    fav_test_drive(weak, tab, label);
    if !fav_wait_for(view, Duration::from_secs(30), |state| {
        state.generation.load(Ordering::SeqCst) > before
    }) {
        return false;
    }
    fav_wait_for(view, Duration::from_secs(180), |state| {
        !state.loading && state.published == Some(tab)
    })
}

/// `BILI_FAV_VIEW_TEST=<tab|all>`: open 收藏推荐 from the backend, walk every
/// requested tab, revisit the first one (cache hit, `ms=0`) and open its first
/// card through `masonry_open`, so the whole view is provable from the log.
fn run_fav_view_test(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    view: Arc<Mutex<FavViewState>>,
    spec: String,
) {
    std::thread::spawn(move || {
        for _ in 0..240 {
            if active_session(&session_slot).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    crate::debug_log("fav view test open tab=5");
                    ui.set_view_mode(0);
                    ui.set_home_tab(5);
                    ui.invoke_home_section(5);
                }
            });
        }
        if !fav_wait_for(&view, Duration::from_secs(180), |state| {
            state.folders_loaded
        }) {
            crate::debug_log("fav view test failed reason=folders-timeout");
            return;
        }
        let folders = view
            .lock()
            .map(|state| state.folders.len())
            .unwrap_or(0);
        if folders == 0 {
            crate::debug_log("fav view test failed reason=no-folders");
            return;
        }
        let targets = fav_view_test_tabs(&spec, folders);
        for &tab in &targets {
            let label = format!("visit-{tab}");
            if !fav_test_drive_and_wait(&weak, &view, tab, &label) {
                crate::debug_log(&format!("fav view test tab={tab} failed=timeout"));
                continue;
            }
            let cards = view
                .lock()
                .ok()
                .and_then(|state| state.videos.lock().ok().map(|videos| videos.len()))
                .unwrap_or(0);
            crate::debug_log(&format!("fav view test tab={tab} cards={cards} done=1"));
        }
        // Let the last tab's cover stream settle, so the revisit publishes with
        // real cover aspects (the shared cache now knows their dimensions).
        std::thread::sleep(Duration::from_secs(8));
        // Second visits must be served from the in-process cache (their load
        // lines report ms=0 hit=1): the mixed stream and the last folder tab.
        let mut revisits: Vec<i32> = Vec::new();
        if let Some(first) = targets.first() {
            revisits.push(*first);
        }
        if let Some(last) = targets.last() {
            if !revisits.contains(last) {
                revisits.push(*last);
            }
        }
        for tab in revisits {
            let _ = fav_test_drive_and_wait(&weak, &view, tab, "revisit");
            crate::debug_log(&format!("fav view test tab={tab} revisit=1"));
        }
        let bvid = view.lock().ok().and_then(|state| {
            state
                .videos
                .lock()
                .ok()
                .and_then(|videos| videos.first().map(|video| video.bvid.clone()))
        });
        match bvid {
            Some(bvid) => {
                crate::debug_log(&format!("fav view test masonry_open bvid={bvid}"));
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.invoke_masonry_open(bvid.into());
                    }
                });
            }
            None => crate::debug_log("fav view test masonry_open skipped reason=no-cards"),
        }
        crate::debug_log("fav view test done");
    });
}

fn load_folder_page(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    folder_state: Arc<Mutex<FolderState>>,
    folder_bvids: Arc<Mutex<Vec<String>>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (media_id, page, title) = match folder_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.media_id, state.page, state.title.clone())
        }
        Err(_) => return,
    };
    // One API page per screen (see `load_region`).
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_folder_title(title.into());
        ui.set_folder_current_page(page as i32);
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let list = match session.fav_list(media_id, page, ps).await {
                Ok(list) => list,
                Err(err) => {
                    crate::debug_log(&format!("fav_list err={err:#}"));
                    let message = format!("收藏内容加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_folder_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<FolderItemRow>::new(),
                            ))));
                            ui.set_folder_page_text(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let items = list.items;
            let mut pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, FolderRowData)> = items
                .iter()
                .map(|item| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        item.upper,
                        format_view(item.play),
                        format_duration(item.duration)
                    );
                    (
                        item.bvid.clone(),
                        FolderRowData {
                            pixels: None,
                            title: item.title.clone(),
                            meta,
                            badge: format_duration(item.duration),
                        },
                    )
                })
                .collect();
            let bvids: Vec<String> = rows.iter().map(|(bvid, _)| bvid.clone()).collect();
            let mids: Vec<i64> = items.iter().map(|item| item.owner_mid).collect();
            let page = list.page;
            let has_more = list.has_more;
            let title = list.title;
            let loaded = rows.len();
            let weak_a = weak.clone();
            let folder_bvids_a = folder_bvids.clone();
            let folder_state_a = folder_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut guard) = folder_bvids_a.lock() {
                        *guard = bvids;
                    }
                    if let Ok(mut state) = folder_state_a.lock() {
                        state.title = title.clone();
                        state.page = page;
                        state.has_more = has_more;
                        state.mids = mids;
                    }
                    let items: Vec<FolderItemRow> = rows
                        .iter()
                        .map(|(_, row)| FolderItemRow {
                            thumb: to_thumb(&row.pixels),
                            title: row.title.clone().into(),
                            meta: row.meta.clone().into(),
                            badge: row.badge.clone().into(),
                        })
                        .collect();
                    ui.set_folder_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    if !title.is_empty() {
                        ui.set_folder_title(title.into());
                    }
                    ui.set_folder_page_text(format!("第 {page} 页").into());
                    ui.set_folder_current_page(page as i32);
                    ui.set_folder_has_prev(page > 1);
                    ui.set_folder_has_next(has_more);
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(4, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("folder load rendered items={loaded}"));
                }
            });
            pics.truncate(covers);
            let keys: Vec<String> = items.iter().take(covers).map(|item| item.bvid.clone()).collect();
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| ThumbGuard::Ids(folder_bvids.clone(), key),
                |ui| ui.get_folder_items(),
                |row, image| row.thumb = image,
                format!("folder page={page}"),
            )
            .await;
        });
    });
}

/// Load one page of the watch history and (append to) the card list.
///
/// `append` loads the next cursor page on top of the loaded rows; `target_page`
/// is the client page the list lands on afterwards (`None` keeps the current
/// one, and a non-append load restarts at 0 because the data is replaced).
fn load_history(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    history_page: Arc<Mutex<HistoryPageState>>,
    history_kids: Arc<Mutex<Vec<String>>>,
    history_bvids: Arc<Mutex<Vec<String>>>,
    append: bool,
    target_page: Option<i64>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (max, view_at, business) = match history_page.lock() {
        Ok(state) => (state.max, state.view_at, state.business.clone()),
        Err(_) => return,
    };
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.history(max, view_at, &business).await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("history err={err:#}"));
                    let message = format!("历史记录加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_history_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let items = page.items;
            let pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, String, HistoryRowData)> = items
                .iter()
                .map(|item| {
                    let progress = item.progress.max(0) as u64;
                    let meta = format!(
                        "{} · {} / {}",
                        item.author,
                        format_duration(progress),
                        format_duration(item.duration)
                    );
                    (
                        item.bvid.clone(),
                        item.kid.clone(),
                        HistoryRowData {
                            pixels: None,
                            title: item.title.clone(),
                            meta,
                            badge: format_duration(item.duration),
                        },
                    )
                })
                .collect();
            let mids: Vec<i64> = items.iter().map(|item| item.owner_mid).collect();
            let cursor = page.cursor;
            let has_more = page.has_more;
            let base_len = if append {
                history_bvids.lock().map(|guard| guard.len()).unwrap_or(0)
            } else {
                0
            };
            let loaded = rows.len();
            let weak_a = weak.clone();
            let history_bvids_a = history_bvids.clone();
            let history_kids_a = history_kids.clone();
            let history_page_a = history_page.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let mut all_rows: Vec<HistoryRow> = if append {
                        let model = ui.get_history_items();
                        (0..model.row_count())
                            .filter_map(|index| model.row_data(index))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut all_bvids: Vec<String> = if append {
                        history_bvids_a
                            .lock()
                            .map(|guard| guard.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let mut all_kids: Vec<String> = if append {
                        history_kids_a
                            .lock()
                            .map(|guard| guard.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let mut all_mids: Vec<i64> = if append {
                        history_page_a
                            .lock()
                            .map(|state| state.mids.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    for (bvid, kid, row) in rows {
                        all_rows.push(HistoryRow {
                            thumb: to_thumb(&row.pixels),
                            title: row.title.into(),
                            meta: row.meta.into(),
                            badge: row.badge.into(),
                        });
                        all_bvids.push(bvid);
                        all_kids.push(kid);
                    }
                    all_mids.extend(mids);
                    if let Ok(mut guard) = history_bvids_a.lock() {
                        *guard = all_bvids;
                    }
                    if let Ok(mut guard) = history_kids_a.lock() {
                        *guard = all_kids;
                    }
                    if let Ok(mut state) = history_page_a.lock() {
                        if let Some(cursor) = &cursor {
                            state.max = cursor.max;
                            state.view_at = cursor.view_at;
                            state.business = cursor.business.clone();
                        } else {
                            state.max = 0;
                        }
                        state.has_more = has_more;
                        state.mids = all_mids;
                    }
                    // The bar's counter is the client page over the loaded rows:
                    // a reset reload starts at 0, an append lands on the page
                    // the bar asked for (or keeps the current one).
                    if !append {
                        set_client_page(5, 0);
                    } else if let Some(target) = target_page {
                        set_client_page(5, target);
                    }
                    ui.set_history_items(ModelRc::from(std::rc::Rc::new(VecModel::from(all_rows))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_history_has_more(has_more);
                    ui.set_history_status(String::new().into());
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("history load rendered items={loaded}"));
                }
            });
            let keys: Vec<String> = items
                .iter()
                .take(covers)
                .map(|item| item.bvid.clone())
                .collect();
            let mut pics = pics;
            pics.truncate(covers);
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                base_len,
                move |_index, key| ThumbGuard::Ids(history_bvids.clone(), key),
                |ui| ui.get_history_items(),
                |row, image| row.thumb = image,
                format!("history append={append}"),
            )
            .await;
        });
    });
}

fn load_toview(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    toview_aids: Arc<Mutex<Vec<i64>>>,
    toview_bvids: Arc<Mutex<Vec<String>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_toview_status(String::new().into());
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.toview().await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("toview err={err:#}"));
                    let message = format!("稍后再看加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_toview_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let items = page.items;
            let pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);

            let rows: Vec<(i64, String, ToViewRowData)> = items
                .iter()
                .map(|item| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        item.owner,
                        format_view(item.view),
                        format_duration(item.duration)
                    );
                    (
                        item.aid,
                        item.bvid.clone(),
                        ToViewRowData {
                            pixels: None,
                            title: item.title.clone(),
                            meta,
                            badge: format_duration(item.duration),
                        },
                    )
                })
                .collect();
            let loaded = rows.len();
            // The endpoint's own total for the list (diagnostic only now that the
            // "共 N 个视频" status line is gone).
            let count = page.count;
            let weak_a = weak.clone();
            let toview_aids_a = toview_aids.clone();
            let toview_bvids_a = toview_bvids.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let mut aids = Vec::with_capacity(rows.len());
                    let mut bvids = Vec::with_capacity(rows.len());
                    let items: Vec<ToViewRow> = rows
                        .iter()
                        .map(|(aid, bvid, row)| {
                            aids.push(*aid);
                            bvids.push(bvid.clone());
                            ToViewRow {
                                thumb: to_thumb(&row.pixels),
                                title: row.title.clone().into(),
                                meta: row.meta.clone().into(),
                                badge: row.badge.clone().into(),
                            }
                        })
                        .collect();
                    if let Ok(mut guard) = toview_aids_a.lock() {
                        *guard = aids;
                    }
                    if let Ok(mut guard) = toview_bvids_a.lock() {
                        *guard = bvids;
                    }
                    ui.set_toview_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_toview_status(String::new().into());
                    // Single-shot list: the client page spans the loaded rows.
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("toview load rendered items={loaded} total={count}"));
                }
            });
            let keys: Vec<String> = items
                .iter()
                .take(covers)
                .map(|item| item.bvid.clone())
                .collect();
            let mut pics = pics;
            pics.truncate(covers);
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| ThumbGuard::Ids(toview_bvids.clone(), key),
                |ui| ui.get_toview_items(),
                |row, image| row.thumb = image,
                "toview".to_string(),
            )
            .await;
        });
    });
}

/// Run one search query on a worker thread and push the rows to the UI.
fn run_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_seq: Arc<AtomicU64>,
    search_bvids: Arc<Mutex<Vec<String>>>,
) {
    let (keyword, tab, page) = match search_state.lock() {
        Ok(state) => (state.keyword.clone(), state.tab, state.page),
        Err(_) => return,
    };
    if keyword.trim().is_empty() {
        return;
    }
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    // One API page per screen: fetch with the live client page size so the
    // loaded page fills exactly one screen.
    let ps = current_page_size(&weak);
    let seq = search_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        update_page_bar(&ui);
    }

    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let outcome = if tab == 0 {
                session
                    .search_video(&keyword, page, "totalrank", ps)
                    .await
                    .map(|result| SearchOutcome {
                        page: result.page,
                        num_pages: result.num_pages,
                        num_results: result.num_results,
                        rows: result
                            .items
                            .iter()
                            .map(|item| {
                                let meta = format!(
                                    "{} · {}播放 · {}",
                                    item.author,
                                    format_view(item.play),
                                    item.duration
                                );
                                (
                                    item.pic.clone(),
                                    SearchRowData {
                                        pixels: None,
                                        title: item.title.clone(),
                                        meta,
                                        sub: item.description.clone(),
                                        badge: item.duration.clone(),
                                    },
                                )
                            })
                            .collect(),
                        bvids: result.items.iter().map(|item| item.bvid.clone()).collect(),
                        mids: result.items.iter().map(|item| item.mid).collect(),
                    })
                    .map_err(|err| format!("{err}"))
            } else {
                session
                    .search_user(&keyword, page, ps)
                    .await
                    .map(|result| SearchOutcome {
                        page: result.page,
                        num_pages: result.num_pages,
                        num_results: result.num_results,
                        rows: result
                            .items
                            .iter()
                            .map(|item| {
                                let meta =
                                    format!("粉丝 {} · 投稿 {}", format_view(item.fans), item.videos);
                                (
                                    item.upic.clone(),
                                    SearchRowData {
                                        pixels: None,
                                        title: item.uname.clone(),
                                        meta,
                                        sub: item.usign.clone(),
                                        badge: String::new(),
                                    },
                                )
                            })
                            .collect(),
                        bvids: Vec::new(),
                        mids: result.items.iter().map(|item| item.mid).collect(),
                    })
                    .map_err(|err| format!("{err}"))
            };

            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(message) => {
                    let state_empty = search_state.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if search_seq.load(Ordering::SeqCst) != seq {
                            return;
                        }
                        if let Ok(mut state) = state_empty.lock() {
                            state.mids.clear();
                        }
                        if let Some(ui) = weak.upgrade() {
                            let empty: Vec<SearchRow> = Vec::new();
                            ui.set_search_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                empty,
                            ))));
                            publish_refresh(&ui, false, 1.0);
                            ui.set_search_status(format!("搜索失败：{message}").into());
                            ui.set_search_page_text(String::new().into());
                            ui.set_search_current_page(1);
                            ui.set_search_has_prev(false);
                            ui.set_search_has_next(false);
                            ui.set_search_total_pages(1);
                            set_client_page(1, 0);
                            publish_list_offset(&ui);
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let pics: Vec<String> = outcome.rows.iter().map(|(url, _)| url.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<SearchRowData> = outcome.rows.into_iter().map(|(_, row)| row).collect();
            let bvids = outcome.bvids;
            let mids = outcome.mids;
            let page = outcome.page;
            let num_pages = outcome.num_pages;
            let num_results = outcome.num_results;

            let loaded = rows.len();
            let weak_a = weak.clone();
            let search_seq_a = search_seq.clone();
            let search_bvids_a = search_bvids.clone();
            let search_state_a = search_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if search_seq_a.load(Ordering::SeqCst) != seq {
                    return;
                }
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut guard) = search_bvids_a.lock() {
                        *guard = bvids;
                    }
                    if let Ok(mut state) = search_state_a.lock() {
                        state.mids = mids;
                    }
                    let items: Vec<SearchRow> = rows
                        .iter()
                        .map(|row| SearchRow {
                            thumb: to_thumb(&row.pixels),
                            title: row.title.clone().into(),
                            meta: row.meta.clone().into(),
                            sub: row.sub.clone().into(),
                            badge: row.badge.clone().into(),
                        })
                        .collect();
                    // One API page per screen: the client page restarts at 0
                    // and the bar counts pages of the live page size.
                    let total = search_total(num_results, num_pages, list_page_size(&ui));
                    if let Ok(mut state) = search_state_a.lock() {
                        state.num_pages = num_pages.max(1);
                    }
                    set_client_page(1, 0);
                    ui.set_search_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_search_status(String::new().into());
                    ui.set_search_page_text(
                        format!("第 {}/{} 页", page.max(1), total).into(),
                    );
                    ui.set_search_current_page(page.max(1) as i32);
                    ui.set_search_has_prev(page > 1);
                    ui.set_search_has_next(page < total);
                    ui.set_search_total_pages(total as i32);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("search load rendered items={loaded}"));
                }
            });
            let mut pics = pics;
            pics.truncate(covers);
            stream_cover_list(
                &weak,
                &session,
                pics,
                Vec::new(),
                0,
                move |_index, _key| ThumbGuard::Seq(search_seq.clone(), seq),
                |ui| ui.get_search_items(),
                |row, image| row.thumb = image,
                format!("search tab={tab} page={page}"),
            )
            .await;
        });
    });
}

/// Open the UP page for a search 用户 row.
///
/// The 用户 tab's rows carry mids and no bvids, so both the grid's activation
/// callback and a card click on that tab land here. `false` means the row is not
/// a user row (视频 tab, stale index) and the caller keeps its own path.
#[allow(clippy::too_many_arguments)]
fn open_search_user(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    index: i32,
    origin: &str,
) -> bool {
    let mid = search_state.lock().ok().and_then(|state| {
        if state.tab != 1 {
            return None;
        }
        usize::try_from(index)
            .ok()
            .and_then(|index| state.mids.get(index).copied())
            .filter(|mid| *mid > 0)
    });
    let Some(mid) = mid else {
        return false;
    };
    crate::debug_log(&format!(
        "up open index={index} source=search-{origin} mid={mid}"
    ));
    open_up_mid(weak, session_slot, up_page, up_seq, up_store, mid);
    true
}

/// Open search result `index` through `search_bvids`, the list the result grid
/// was built from, by resolving it into the detail API (which starts the
/// player).
///
/// One implementation for both entry points — the grid's card click
/// (`card_open`, origin `card`) and the stock activation callback
/// (`open_search_item`, origin `activate`) — so a click can never disagree with
/// the row it was on. `false` means nothing was opened: no video rows (the 用户
/// tab), a stale index, or no session.
#[allow(clippy::too_many_arguments)]
fn open_search_index(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    detail_state: Arc<Mutex<Option<DetailState>>>,
    detail_seq: Arc<AtomicU64>,
    related_bvids: Arc<Mutex<Vec<String>>>,
    index: i32,
    origin: &str,
) -> bool {
    let tab = search_state.lock().map(|state| state.tab).unwrap_or(0);
    if tab != 0 {
        crate::debug_log(&format!(
            "search open ignored origin={origin} index={index} tab={tab}"
        ));
        return false;
    }
    let index = index.max(0) as usize;
    let bvid = search_bvids
        .lock()
        .ok()
        .and_then(|guard| guard.get(index).cloned());
    let Some(bvid) = bvid else {
        return false;
    };
    crate::debug_log(&format!(
        "search open origin={origin} index={index} bvid={bvid}"
    ));
    open_detail(
        weak,
        session_slot,
        detail_state,
        detail_seq,
        related_bvids,
        bvid,
    );
    true
}

/// Fetch one video's detail + related list and present the detail view.
fn open_detail(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    detail_state: Arc<Mutex<Option<DetailState>>>,
    detail_seq: Arc<AtomicU64>,
    _related_bvids: Arc<Mutex<Vec<String>>>,
    bvid: String,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let seq = detail_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        // Card clicks resolve the video and open the player directly; the
        // detail page no longer opens, only the part selection is reset.
        ui.set_detail_selected_part(0);
    }

    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let detail = match session.video_view(&bvid).await {
                Ok(detail) => detail,
                Err(err) => {
                    let message = format!("视频详情加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if detail_seq.load(Ordering::SeqCst) != seq {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            ui.set_detail_loading(false);
                            ui.set_detail_desc(message.into());
                        }
                    });
                    return;
                }
            };

            let mut cids: Vec<u64> = detail
                .pages
                .iter()
                .map(|page| page.cid.max(0) as u64)
                .collect();
            if cids.is_empty() {
                cids.push(detail.cid.max(0) as u64);
            }
            let state = DetailState {
                bvid: detail.bvid.clone(),
                title: detail.title.clone(),
                cid: detail.cid.max(0) as u64,
                aid: detail.aid,
                parts: cids,
            };

            crate::debug_log(&format!("open_detail->play bvid={}", detail.bvid));

            let _ = slint::invoke_from_event_loop(move || {
                if detail_seq.load(Ordering::SeqCst) != seq {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut guard) = detail_state.lock() {
                        *guard = Some(state);
                    }
                    ui.invoke_play_detail();
                }
            });
        });
    });
}

fn format_view(view: u64) -> String {
    if view >= 100_000_000 {
        format!("{:.1}亿", view as f64 / 100_000_000.0)
    } else if view >= 10_000 {
        format!("{:.1}万", view as f64 / 10_000.0)
    } else {
        view.to_string()
    }
}

fn format_duration(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn format_mmss(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn format_speed(speed: f64) -> String {
    let label = if (speed - speed.round()).abs() < 1e-9 {
        format!("{speed:.1}")
    } else {
        let text = format!("{speed:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    format!("{label}x")
}

/// `mm:ss` for a 睡眠定时 countdown.
fn fmt_countdown(ms: u64) -> String {
    format_mmss(ms)
}

/// Destination PNG path for 截图: the user's Pictures folder when present,
/// otherwise the project directory.
fn screenshot_path() -> PathBuf {
    let dir = std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Pictures"))
        .filter(|path| path.is_dir())
        .or_else(|| settings::settings_path().parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    dir.join(format!("bilibili_shot_{stamp}.png"))
}

/// Run `f` on the UI thread, but only if `my` is still the current generation.
fn post<F>(weak: &Weak<AppWindow>, generation: &Arc<AtomicU64>, my: u64, f: F)
where
    F: FnOnce(&AppWindow) + Send + 'static,
{
    let weak = weak.clone();
    let generation = generation.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if generation.load(Ordering::SeqCst) != my {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    });
}

fn post_status(weak: &Weak<AppWindow>, generation: &Arc<AtomicU64>, my: u64, text: &str) {
    let text = text.to_string();
    post(weak, generation, my, move |ui| ui.set_status(text.into()));
}

/// Emit one login step: switch the login view to the progress state, set the
/// step label and percentage, and log `id` so the real network stage is visible
/// in `BILI_DEBUG_LOG`. `id` names the code stage; `label` is what the user reads.
fn post_step(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    id: &str,
    label: &str,
    percent: i32,
) {
    crate::debug_log(&format!("login-step id={id} pct={percent} label={label}"));
    let label = label.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_login_state(1);
        ui.set_login_step(label.into());
        ui.set_login_percent(percent);
        ui.set_login_progress(percent as f32 / 100.0);
        ui.set_login_countdown("".into());
        ui.set_login_failed(false);
        ui.set_login_error("".into());
    });
}

/// Switch the login view to the "waiting for scan" state (QR centered) and
/// refresh the live countdown/progress while the poll loop runs.
fn post_login_qr(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    step: &str,
    percent: i32,
    countdown: &str,
) {
    crate::debug_log(&format!(
        "login-step id=waiting pct={percent} label={step} countdown={countdown}"
    ));
    let step = step.to_string();
    let countdown = countdown.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_login_state(2);
        ui.set_login_step(step.into());
        ui.set_login_percent(percent);
        ui.set_login_progress(percent as f32 / 100.0);
        ui.set_login_countdown(countdown.into());
        ui.set_login_failed(false);
        ui.set_login_error("".into());
    });
}

/// Report a failed login step: keep the progress view but surface the failed
/// step label plus the API/error code, and mark the attempt failed.
fn post_login_fail(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    id: &str,
    label: &str,
    percent: i32,
    code: &str,
) {
    crate::debug_log(&format!(
        "login-step id={id} pct={percent} label={label} FAILED code={code}"
    ));
    let label = label.to_string();
    let code = code.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_login_state(1);
        ui.set_login_step(label.into());
        ui.set_login_percent(percent);
        ui.set_login_progress(percent as f32 / 100.0);
        ui.set_login_countdown("".into());
        ui.set_login_failed(true);
        ui.set_login_error(code.into());
    });
}

/// Fall back to the login choice page (auto-login failed or nothing saved).
fn post_login_choice(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    reason: &str,
) {
    crate::debug_log(&format!("login-choice shown reason={reason}"));
    let reason = reason.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_logged_in(false);
        ui.set_qr_image(Image::default());
        ui.set_login_state(0);
        ui.set_login_step("请选择登录方式".into());
        ui.set_login_percent(0);
        ui.set_login_progress(0.0);
        ui.set_login_failed(false);
        ui.set_login_error(reason.into());
        ui.set_login_countdown("".into());
    });
}

/// Wrap a player-key mapping as a Slint model for the `[int]` maps.
fn key_map_model(keys: &[u32]) -> ModelRc<i32> {
    let rows: Vec<i32> = keys.iter().map(|key| *key as i32).collect();
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}

/// Push a preferences snapshot into the settings panel's bound properties.
fn publish_settings(ui: &AppWindow, snapshot: &settings::Settings) {
    ui.set_set_quality(snapshot.player_quality as i32);
    ui.set_set_speed_milli(snapshot.player_speed as i32);
    ui.set_set_hwaccel(snapshot.hwaccel);
    ui.set_set_codec(snapshot.codec_pref as i32);
    ui.set_set_dm_opacity(snapshot.danmaku_opacity as i32);
    ui.set_set_dm_font(snapshot.danmaku_font as i32);
    ui.set_set_dm_speed(snapshot.danmaku_speed as i32);
    ui.set_set_dm_area(snapshot.danmaku_area as i32);
    ui.set_set_block_scroll(snapshot.block_scroll);
    ui.set_set_block_top(snapshot.block_top);
    ui.set_set_block_bottom(snapshot.block_bottom);
    ui.set_set_ai_on(snapshot.ai_on);
    ui.set_set_ai_level(snapshot.ai_level as i32);
    ui.set_set_theme(snapshot.theme as i32);
    // The two frosted-glass sliders live on the theme global (they drive the
    // window tint's alpha and the blur loop), not on a settings-panel property.
    ui.global::<AppTheme>()
        .set_glass_transparency(snapshot.glass_transparency);
    ui.global::<AppTheme>().set_glass_blur(snapshot.glass_blur);
    // The cards-per-page setting is Rust-driven: the UI only reports menu
    // changes (`page_size_changed`), so every publish pushes the stored value
    // into the paging property.
    ui.set_list_page_size(snapshot.page_size as i32);
    ui.set_set_autostart(snapshot.autostart);
    // 关闭主窗口的行为 (0 = 最小化到任务栏, 1 = 隐藏到系统托盘, 2 = 直接退出),
    // so the panel opens on the stored value.
    ui.set_set_close_action(snapshot.close_action as i32);
    ui.set_set_key_map(key_map_model(&snapshot.player_keys));
    let rows: Vec<SharedString> = snapshot
        .blocked_senders
        .iter()
        .map(|hash| SharedString::from(hash.clone()))
        .collect();
    ui.set_set_blocked_senders(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    let keywords: Vec<SharedString> = snapshot
        .blocked_keywords
        .iter()
        .map(|word| SharedString::from(word.clone()))
        .collect();
    ui.set_set_blocked_keywords(ModelRc::from(std::rc::Rc::new(VecModel::from(keywords))));
    let regexes: Vec<SharedString> = snapshot
        .blocked_regex
        .iter()
        .map(|pattern| SharedString::from(pattern.clone()))
        .collect();
    ui.set_set_blocked_regex(ModelRc::from(std::rc::Rc::new(VecModel::from(regexes))));
}

/// Mutate the stored preferences, persist them off-thread, and return a snapshot.
fn update_settings<F>(store: &Arc<Mutex<settings::Settings>>, mutate: F) -> settings::Settings
where
    F: FnOnce(&mut settings::Settings),
{
    let snapshot = match store.lock() {
        Ok(mut guard) => {
            mutate(&mut guard);
            guard.clone()
        }
        Err(_) => return settings::Settings::default(),
    };
    let to_save = snapshot.clone();
    std::thread::spawn(move || {
        let _ = settings::save(&to_save);
    });
    snapshot
}

/// Apply a preferences snapshot to every live player window.
fn apply_settings_to_player(state: &Rc<PlayerState>, snapshot: &settings::Settings) {
    let sessions = player_sessions(state);
    let mut codec_changed = None;
    for session in &sessions {
        let control = &session.control;
        control.set_danmaku_font(snapshot.danmaku_font);
        control.set_danmaku_speed(snapshot.danmaku_speed);
        control.set_danmaku_area(snapshot.danmaku_area);
        control.set_type_blocks(
            snapshot.block_scroll,
            snapshot.block_top,
            snapshot.block_bottom,
        );
        control.set_blocked_senders(snapshot.blocked_senders.clone());
        control.set_block_rules(&snapshot.blocked_keywords, &snapshot.blocked_regex);
        control.set_hwaccel(snapshot.hwaccel);
        let new_codec = snapshot.codec_pref as i64;
        if control.codec_pref() != new_codec {
            codec_changed = Some(new_codec);
        }
        control.set_codec_pref(new_codec);
        control.set_speed(snapshot.player_speed as f64 / 1000.0);
    }
    if let Some(codec) = codec_changed {
        if let Some(session) = focused_session(state) {
            let label = if codec == 0 {
                "自动".to_string()
            } else {
                bilibili::codec_label(codec).to_string()
            };
            toast(
                &session.window,
                format!("编码偏好：{label}"),
                TOAST_KIND_INFO,
            );
        }
    }
    for session in &sessions {
        let window = &session.window;
        window.set_danmaku_opacity(snapshot.danmaku_opacity as f32 / 100.0);
        window.set_danmaku_opacity_text(snapshot.danmaku_opacity.to_string().into());
        window.set_danmaku_font(snapshot.danmaku_font as i32);
        window.set_danmaku_speed(snapshot.danmaku_speed as i32);
        window.set_danmaku_area(snapshot.danmaku_area as i32);
        window.set_block_scroll(snapshot.block_scroll);
        window.set_block_top(snapshot.block_top);
        window.set_block_bottom(snapshot.block_bottom);
        window.set_key_map(key_map_model(&snapshot.player_keys));
    }
}

/// Load one page of the 动态 feed and (append to) the card list.
///
/// `append` loads the next cursor page on top of the loaded rows; `target_page`
/// is the client page the list lands on afterwards (`None` keeps the current
/// one, and a non-append load restarts at 0 because the data is replaced).
fn load_dynamic(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    dynamic_ids: Arc<Mutex<Vec<String>>>,
    dynamic_offset: Arc<Mutex<String>>,
    dynamic_has_more: Arc<Mutex<bool>>,
    append: bool,
    target_page: Option<i64>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let offset = dynamic_offset
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_dynamic_status(String::new().into());
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.dynamic_feed(&offset, "all").await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("dynamic_feed err={err:#}"));
                    let message = format!("动态加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_dynamic_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let items = page.items;
            let covers = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, DynamicRowData)> = items
                .iter()
                .map(|item| {
                    let stats = format!(
                        "赞 {} · 评论 {} · 转发 {}",
                        format_view(item.like_count),
                        format_view(item.comment_count),
                        format_view(item.forward_count)
                    );
                    let liked = item.liked;
                    let has_cover = !item.archive_cover.is_empty() || !item.images.is_empty();
                    (
                        item.id_str.clone(),
                        DynamicRowData {
                            id: item.id_str.clone(),
                            author: item.author_name.clone(),
                            face: None,
                            time: item.pub_time.clone(),
                            text: item.text.clone(),
                            has_cover,
                            cover: None,
                            title: item.archive_title.clone(),
                            meta: item.archive_meta.clone(),
                            liked,
                            like_text: if liked {
                                "已赞".to_string()
                            } else {
                                "点赞".to_string()
                            },
                            stats,
                        },
                    )
                })
                .collect();
            let new_offset = page.offset;
            let has_more = page.has_more;
            let base_len = if append {
                dynamic_ids.lock().map(|ids| ids.len()).unwrap_or(0)
            } else {
                0
            };
            let loaded = rows.len();
            let weak_a = weak.clone();
            let dynamic_ids_a = dynamic_ids.clone();
            let dynamic_offset_a = dynamic_offset.clone();
            let dynamic_has_more_a = dynamic_has_more.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let mut all_rows: Vec<DynamicRow> = if append {
                        let model = ui.get_dynamic_items();
                        (0..model.row_count())
                            .filter_map(|index| model.row_data(index))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut all_ids: Vec<String> = if append {
                        dynamic_ids_a
                            .lock()
                            .map(|ids| ids.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    for (id, row) in rows {
                        all_ids.push(id);
                        all_rows.push(DynamicRow {
                            id: row.id.into(),
                            author: row.author.into(),
                            face: to_thumb(&row.face),
                            time: row.time.into(),
                            text: row.text.into(),
                            has_cover: row.has_cover,
                            cover: to_thumb(&row.cover),
                            title: row.title.into(),
                            meta: row.meta.into(),
                            liked: row.liked,
                            like_text: row.like_text.into(),
                            stats: row.stats.into(),
                        });
                    }
                    if let Ok(mut guard) = dynamic_ids_a.lock() {
                        *guard = all_ids;
                    }
                    if let Ok(mut guard) = dynamic_offset_a.lock() {
                        *guard = new_offset;
                    }
                    if let Ok(mut guard) = dynamic_has_more_a.lock() {
                        *guard = has_more;
                    }
                    ui.set_dynamic_items(ModelRc::from(std::rc::Rc::new(VecModel::from(all_rows))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_dynamic_has_more(has_more);
                    ui.set_dynamic_status(String::new().into());
                    // The bar's counter is the client page over the loaded
                    // rows: a reset reload starts at 0, an append lands on the
                    // page the bar asked for (or keeps the current one).
                    if !append {
                        set_client_page(13, 0);
                    } else if let Some(target) = target_page {
                        set_client_page(13, target);
                    }
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("dynamic load rendered items={loaded}"));
                }
            });

            let sync = ThumbSync::new(format!("dynamic append={append}"), covers);
            let thumbs_started = Instant::now();
            let fallbacks_before = bilibili::thumb_fallbacks();
            let mut delivered = 0usize;
            let fetches = items.iter().take(covers).enumerate().map(|(index, item)| {
                let session = session.clone();
                let face = item.author_face.clone();
                let cover = if !item.archive_cover.is_empty() {
                    item.archive_cover.clone()
                } else {
                    item.images.first().cloned().unwrap_or_default()
                };
                let id = item.id_str.clone();
                async move {
                    let face_img = image_gated(&session, &face).await;
                    let cover_img = thumb_gated(&session, &cover).await;
                    (index, id, face_img, cover_img)
                }
            });
            let mut stream = futures::stream::iter(fetches).buffer_unordered(THUMB_CONCURRENCY);
            while let Some((index, id, face_img, cover_img)) = stream.next().await {
                if player_active() {
                    continue;
                }
                delivered += 1;
                let row = base_len + index;
                let weak_c = weak.clone();
                let dynamic_ids_c = dynamic_ids.clone();
                let sync_c = sync.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_c.upgrade() {
                        let applied = ThumbGuard::Ids(dynamic_ids_c, id).allows(row);
                        if applied {
                            patch_model_thumb(
                                ui.get_dynamic_items(),
                                row,
                                to_thumb(&face_img),
                                |item_row, image| item_row.face = image,
                            );
                            patch_model_thumb(
                                ui.get_dynamic_items(),
                                row,
                                to_thumb(&cover_img),
                                |item_row, image| item_row.cover = image,
                            );
                        }
                        skeleton_settle(&ui, 1);
                        sync_c.settle(applied);
                    }
                });
            }
            crate::debug_log(&format!(
                "dynamic thumbs n={} fallback={} ms={}",
                delivered,
                bilibili::thumb_fallbacks().saturating_sub(fallbacks_before),
                thumbs_started.elapsed().as_millis()
            ));
            flush_missing(&weak, &sync, covers.saturating_sub(delivered));
        });
    });
}

/// Refresh the 消息 unread badge.
fn load_unread(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let unread = session.msg_unread().await.unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_msg_unread_total(to_i32(unread.total));
                    ui.set_msg_unread_text(
                        format!("回复 {} · @ {} · 赞 {}", unread.reply, unread.at, unread.like)
                            .into(),
                    );
                }
            });
        });
    });
}

/// Load one 回复我的 / @我的 / 收到的赞 list.
fn load_msg_list(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    tab: i32,
) {
    let kind = match tab {
        0 => bilibili::MsgKind::Reply,
        1 => bilibili::MsgKind::At,
        _ => bilibili::MsgKind::Like,
    };
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_msg_loading(true);
        ui.set_msg_status(String::new().into());
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.msg_list(kind).await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("msg_list err={err:#}"));
                    let message = format!("消息加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_msg_loading(false);
                            ui.set_msg_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let items = page.items;
            let avatars = futures::stream::iter(items.iter().map(|item| {
                let session = session.clone();
                let url = item.avatar.clone();
                async move {
                    image_gated(&session, &url).await
                }
            }))
            .buffered(THUMB_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
            let rows: Vec<MsgRowData> = items
                .into_iter()
                .zip(avatars)
                .map(|(item, avatar)| MsgRowData {
                    nickname: item.nickname,
                    avatar,
                    title: item.title,
                    detail: item.detail,
                    time: time_ago(item.time),
                    count: if item.count > 0 {
                        format!("{}条", item.count)
                    } else {
                        String::new()
                    },
                })
                .collect();
            let total = rows.len();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let items: Vec<MsgRow> = rows
                        .into_iter()
                        .map(|row| MsgRow {
                            nickname: row.nickname.into(),
                            avatar: to_thumb(&row.avatar),
                            title: row.title.into(),
                            detail: row.detail.into(),
                            time: row.time.into(),
                            count: row.count.into(),
                        })
                        .collect();
                    ui.set_msg_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_msg_loading(false);
                    ui.set_msg_status(
                        if total == 0 {
                            "暂无消息".into()
                        } else {
                            format!("共 {total} 条").into()
                        },
                    );
                    update_page_bar(&ui);
                }
            });
        });
    });
}

/// Load the 私信 session list (names resolved via `space_info`).
fn load_sessions(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    session_meta: Arc<Mutex<Vec<(i64, String)>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_msg_loading(true);
        ui.set_msg_status(String::new().into());
        update_page_bar(&ui);
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let sessions = match session.dm_sessions().await {
                Ok(list) => list,
                Err(err) => {
                    crate::debug_log(&format!("dm_sessions err={err:#}"));
                    let message = format!("私信会话加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_msg_loading(false);
                            ui.set_msg_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let sessions: Vec<_> = sessions
                .into_iter()
                .take(bilibili::DM_SESSION_LIMIT)
                .collect();
            let fetched = futures::stream::iter(sessions.iter().map(|entry| {
                let session = session.clone();
                let talker = entry.talker_id;
                async move {
                    match session.space_info(talker).await {
                        Ok(info) => {
                            let face = image_gated(&session, &info.face).await;
                            let name = if info.name.is_empty() {
                                format!("用户 {talker}")
                            } else {
                                info.name
                            };
                            (name, face)
                        }
                        Err(_) => (format!("用户 {talker}"), None),
                    }
                }
            }))
            .buffered(4)
            .collect::<Vec<_>>()
            .await;
            let mut meta: Vec<(i64, String)> = Vec::with_capacity(sessions.len());
            let rows: Vec<SessionRowData> = sessions
                .into_iter()
                .zip(fetched)
                .map(|(entry, (name, face))| {
                    meta.push((entry.talker_id, name.clone()));
                    SessionRowData {
                        talker_id: entry.talker_id.to_string(),
                        name,
                        avatar: face,
                        preview: entry.preview,
                        time: time_ago(entry.at_time / 1000),
                        unread: if entry.unread > 0 {
                            format!("{}", entry.unread)
                        } else {
                            String::new()
                        },
                    }
                })
                .collect();
            let total = rows.len();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut guard) = session_meta.lock() {
                        *guard = meta;
                    }
                    let items: Vec<SessionRow> = rows
                        .into_iter()
                        .map(|row| SessionRow {
                            talker_id: row.talker_id.into(),
                            name: row.name.into(),
                            avatar: to_thumb(&row.avatar),
                            preview: row.preview.into(),
                            time: row.time.into(),
                            unread: row.unread.into(),
                        })
                        .collect();
                    ui.set_session_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_msg_loading(false);
                    ui.set_msg_status(
                        if total == 0 {
                            "暂无会话".into()
                        } else {
                            format!("显示最近 {total} 个会话").into()
                        },
                    );
                    update_page_bar(&ui);
                }
            });
        });
    });
}

/// Load one 私信 session's messages (read-only).
fn load_dm_messages(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    talker_id: i64,
) {
    let Some((session, my_mid)) = active_parts(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_dm_loading(true);
        ui.set_dm_status(String::new().into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let messages = match session.dm_messages(talker_id).await {
                Ok(list) => list,
                Err(err) => {
                    crate::debug_log(&format!("dm_messages err={err:#}"));
                    let message = format!("私信加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_dm_loading(false);
                            ui.set_dm_status(message.into());
                        }
                    });
                    return;
                }
            };
            let rows: Vec<DmRow> = messages
                .into_iter()
                .map(|message| DmRow {
                    from_me: message.sender_uid == my_mid,
                    text: if message.text.is_empty() {
                        "[非文本消息]".to_string()
                    } else {
                        message.text
                    }
                    .into(),
                    time: time_ago(message.timestamp).into(),
                })
                .collect();
            let total = rows.len();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_dm_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
                    ui.set_dm_loading(false);
                    if total == 0 {
                        ui.set_dm_status("暂无消息".into());
                    }
                }
            });
        });
    });
}

/// Load one page of the active account's 关注 / 粉丝 list.
fn load_relation(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    relation_state: Arc<Mutex<RelationState>>,
    page: i64,
) {
    let Some((session, mid)) = active_parts(&session_slot) else {
        return;
    };
    let (kind, page) = match relation_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.kind, state.page)
        }
        Err(_) => return,
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_relation_loading(true);
        ui.set_relation_status(String::new().into());
    }
    std::thread::spawn(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = if kind == 1 {
                session.fans(mid, page).await
            } else {
                session.followings(mid, page).await
            };
            let page_data = match result {
                Ok(data) => data,
                Err(err) => {
                    crate::debug_log(&format!("relation kind={kind} err={err:#}"));
                    let message = format!("用户列表加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_relation_loading(false);
                            ui.set_relation_status(message.into());
                        }
                    });
                    return;
                }
            };
            let users = page_data.items;
            let faces = futures::stream::iter(users.iter().map(|user| {
                let session = session.clone();
                let url = user.face.clone();
                async move {
                    image_gated(&session, &url).await
                }
            }))
            .buffered(THUMB_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
            let rows: Vec<RelationRowData> = users
                .into_iter()
                .zip(faces)
                .map(|(user, face)| RelationRowData {
                    mid: user.mid.to_string(),
                    uname: user.uname,
                    face,
                    sign: user.sign,
                    official: user.official,
                })
                .collect();
            let total = page_data.total;
            let num = page_data.page;
            let has_more = page_data.has_more;
            let title = if kind == 1 { "粉丝" } else { "关注" };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut state) = relation_state.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    let items: Vec<RelationRow> = rows
                        .into_iter()
                        .map(|row| RelationRow {
                            mid: row.mid.into(),
                            uname: row.uname.into(),
                            face: to_thumb(&row.face),
                            sign: row.sign.into(),
                            official: row.official.into(),
                        })
                        .collect();
                    ui.set_relation_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_relation_loading(false);
                    ui.set_relation_title(title.into());
                    ui.set_relation_total(
                        format!("共 {} 人", format_view(total.max(0) as u64)).into(),
                    );
                    ui.set_relation_current_page(num as i32);
                    ui.set_relation_has_prev(num > 1);
                    ui.set_relation_has_next(has_more);
                }
            });
        });
    });
}

fn to_i32(value: i64) -> i32 {
    value.clamp(0, i32::MAX as i64) as i32
}

/// Current Unix time in whole seconds, clamped to `i32`.
///
/// The UI's freshness chips (`now_ts`) compare this against a published
/// timestamp; seconds fit in `i32` until 2038, which is well past the useful
/// life of that indicator, and Slint's `int` is 32-bit.
fn now_unix_secs() -> i32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i32::MAX as u64) as i32)
        .unwrap_or(0)
}

/// A short relative time for a unix-seconds timestamp.
fn time_ago(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let diff = (now - secs).max(0);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{}分钟前", diff / 60)
    } else if diff < 86_400 {
        format!("{}小时前", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}天前", diff / 86_400)
    } else {
        let (_, month, day) = civil_from_days(secs.div_euclid(86_400));
        format!("{month:02}-{day:02}")
    }
}

/// Civil (year, month, day) for a day count since the Unix epoch
/// (Howard Hinnant's algorithm).
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Encode `text` as a QR code and rasterize it to raw RGBA8.
///
/// The module size is chosen so the code is displayed at an exact integer
/// number of device pixels per module (no fractional scaling of modules).
fn build_qr_rgba(text: &str) -> Result<(Vec<u8>, u32, u32)> {
    let code =
        qrcode::QrCode::new(text.as_bytes()).map_err(|e| anyhow!("生成二维码失败：{e:?}"))?;
    let modules = code.width();
    let quiet = 4usize;

    let target = 320usize;
    let scale = (target / (modules + quiet * 2)).clamp(2, 12);
    let side = (modules + quiet * 2) * scale;

    // White background, opaque.
    let mut buffer = vec![255u8; side * side * 4];

    let colors = code.to_colors();
    for (index, color) in colors.iter().enumerate() {
        if *color == qrcode::Color::Dark {
            let mx = index % modules;
            let my = index / modules;
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = (mx + quiet) * scale + dx;
                    let y = (my + quiet) * scale + dy;
                    let offset = (y * side + x) * 4;
                    buffer[offset] = 0;
                    buffer[offset + 1] = 0;
                    buffer[offset + 2] = 0;
                    buffer[offset + 3] = 255;
                }
            }
        }
    }

    Ok((buffer, side as u32, side as u32))
}

#[cfg(test)]
mod cover_warm_tests {
    use super::*;

    fn video(index: usize) -> bilibili::FeedVideo {
        bilibili::FeedVideo {
            bvid: format!("BV{index}"),
            pic: format!("https://example.invalid/{index}.jpg"),
            ..bilibili::FeedVideo::default()
        }
    }

    fn cover(bytes: usize) -> Arc<CoverPixels> {
        Arc::new((vec![0u8; bytes], 1, 1))
    }

    #[test]
    fn warm_target_is_one_page_plus_margin_capped() {
        // A 12-card page plus the next row; a full 24-card page hits the cap.
        assert_eq!(feed_cover_warm_target(12), 16);
        assert_eq!(feed_cover_warm_target(24), COVER_WARM_MAX);
        assert_eq!(feed_cover_warm_target(48), COVER_WARM_MAX);
        // Defensive: a nonsensical page size still warms a small prefix.
        assert_eq!(feed_cover_warm_target(0), 1 + COVER_WARM_MARGIN);
    }

    #[test]
    fn pending_covers_skips_warm_rows_by_index() {
        let videos: Vec<bilibili::FeedVideo> = (0..5).map(video).collect();
        let warm = vec![(0, cover(4)), (2, cover(4))];
        let (cap, pending) = pending_feed_covers(&videos, &warm);
        assert_eq!(cap, 5);
        assert_eq!(
            pending.iter().map(|cover| cover.index).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        assert_eq!(pending[0].bvid, "BV1");
        assert_eq!(pending[0].pic, "https://example.invalid/1.jpg");
    }

    #[test]
    fn pending_covers_caps_at_the_thumb_limit() {
        let videos: Vec<bilibili::FeedVideo> =
            (0..bilibili::THUMB_MAX_PER_VIEW + 5).map(video).collect();
        let (cap, pending) = pending_feed_covers(&videos, &[]);
        assert_eq!(cap, bilibili::THUMB_MAX_PER_VIEW);
        assert_eq!(pending.len(), bilibili::THUMB_MAX_PER_VIEW);
    }

    #[test]
    fn cover_cache_hit_returns_the_same_decode() {
        let mut cache = CoverCache::new();
        let pixels = cover(16);
        cache.put("a", pixels.clone());
        let hit = cache.get("a");
        assert!(hit.is_some());
        if let Some(hit) = hit {
            assert!(Arc::ptr_eq(&hit, &pixels));
        }
    }

    #[test]
    fn cover_cache_evicts_least_recently_used() {
        let mut cache = CoverCache::new();
        for index in 0..COVER_CACHE_ENTRIES {
            cache.put(&format!("k{index}"), cover(2));
        }
        // A hit makes the oldest entry the most recent, so its neighbour goes.
        assert!(cache.get("k0").is_some());
        cache.put("fresh", cover(2));
        assert_eq!(cache.entries.len(), COVER_CACHE_ENTRIES);
        assert!(cache.get("k1").is_none());
        assert!(cache.get("k0").is_some());
        assert!(cache.get("fresh").is_some());
    }

    #[test]
    fn cover_cache_ignores_empty_and_oversized_decodes() {
        let mut cache = CoverCache::new();
        cache.put("empty", cover(0));
        cache.put("huge", Arc::new((vec![0u8; COVER_CACHE_BYTES + 1], 1, 1)));
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}

#[cfg(test)]
mod playlist_tests {
    use super::*;

    fn entry(bvid: &str) -> QueueEntry {
        QueueEntry {
            bvid: bvid.to_string(),
            cid: 1,
            aid: 1,
            title: bvid.to_string(),
            owner: "UP".to_string(),
            duration: "01:30".to_string(),
            cover: String::new(),
        }
    }

    fn queue(bvids: &[&str]) -> PlaybackQueue {
        PlaybackQueue {
            items: bvids.iter().map(|bvid| entry(bvid)).collect(),
            current: None,
            mode: 1,
            played: HashSet::new(),
        }
    }

    #[test]
    fn push_rejects_a_duplicate_bvid() {
        let mut queue = queue(&["BV1", "BV2"]);
        assert!(queue.push(entry("BV3")));
        assert!(!queue.push(entry("BV1")));
        assert_eq!(queue.items.len(), 3);
    }

    #[test]
    fn push_at_the_cap_drops_the_oldest_and_shifts_the_current() {
        let bvids: Vec<String> = (0..settings::PLAYLIST_MAX)
            .map(|index| format!("BV{index}"))
            .collect();
        let refs: Vec<&str> = bvids.iter().map(String::as_str).collect();
        let mut queue = queue(&refs);
        queue.current = Some(settings::PLAYLIST_MAX / 2);
        queue.played.insert(settings::PLAYLIST_MAX / 2 + 1);
        assert!(queue.push(entry("BVNEW")));
        assert_eq!(queue.items.len(), settings::PLAYLIST_MAX);
        assert_eq!(queue.items.first().map(|item| item.bvid.as_str()), Some("BV1"));
        assert_eq!(
            queue.items.last().map(|item| item.bvid.as_str()),
            Some("BVNEW")
        );
        assert_eq!(queue.current, Some(settings::PLAYLIST_MAX / 2 - 1));
        assert!(queue.played.contains(&(settings::PLAYLIST_MAX / 2)));
    }

    #[test]
    fn step_wraps_in_both_directions_in_every_mode() {
        let mut queue_value = queue(&["BV1", "BV2", "BV3"]);
        queue_value.mode = 2;
        assert_eq!(queue_value.step(true), Some(0));
        assert_eq!(queue_value.step(false), Some(2));
        queue_value.current = Some(2);
        assert_eq!(queue_value.step(true), Some(0));
        assert_eq!(queue_value.step(false), Some(1));
        let empty = queue(&[]);
        assert_eq!(empty.step(true), None);
        assert_eq!(empty.step(false), None);
    }

    #[test]
    fn remove_keeps_current_on_the_same_item() {
        let mut queue = queue(&["BV1", "BV2", "BV3"]);
        queue.current = Some(2);
        assert!(queue.remove(0).is_some());
        assert_eq!(queue.current, Some(1));
        assert_eq!(
            queue.items.iter().map(|item| item.bvid.as_str()).collect::<Vec<_>>(),
            vec!["BV2", "BV3"]
        );
        // Removing the current item leaves nothing current; playback itself is
        // untouched, the queue simply no longer owns it.
        assert!(queue.remove(1).is_some());
        assert_eq!(queue.current, None);
        assert!(queue.remove(5).is_none());
    }

    #[test]
    fn clear_empties_the_queue() {
        let mut queue = queue(&["BV1"]);
        queue.current = Some(0);
        queue.played.insert(0);
        queue.clear();
        assert!(queue.items.is_empty());
        assert_eq!(queue.current, None);
        assert!(queue.played.is_empty());
    }

    #[test]
    fn shuffle_never_repeats_until_the_queue_is_exhausted() {
        let mut queue = queue(&["BV1", "BV2", "BV3"]);
        queue.mode = 2;
        queue.current = Some(0);
        let mut seen = HashSet::new();
        for _ in 0..3 {
            if let Some(current) = queue.current {
                queue.played.insert(current);
            }
            let candidates = queue.random_candidates();
            assert!(!candidates.is_empty());
            let pick = candidates[0];
            assert!(seen.insert(pick), "pick {pick} repeated before exhaustion");
            queue.current = Some(pick);
        }
        // Every item played: the set resets and a new round may repeat.
        if let Some(current) = queue.current {
            queue.played.insert(current);
        }
        let candidates = queue.random_candidates();
        assert!(!candidates.is_empty());
    }

    #[test]
    fn shuffle_with_one_item_replays_it() {
        let mut queue = queue(&["BV1"]);
        queue.current = Some(0);
        queue.played.insert(0);
        assert_eq!(queue.random_candidates(), vec![0]);
    }

    #[test]
    fn duration_label_parses_back_to_seconds() {
        assert_eq!(duration_seconds("01:30"), 90);
        assert_eq!(duration_seconds("1:02:03"), 3723);
        assert_eq!(duration_seconds(""), 0);
        assert_eq!(duration_seconds("bad"), 0);
    }
}

#[cfg(test)]
mod fav_view_tests {
    use super::*;

    fn video(bvid: &str) -> bilibili::FeedVideo {
        bilibili::FeedVideo {
            bvid: bvid.to_string(),
            ..bilibili::FeedVideo::default()
        }
    }

    fn folder(id: i64, count: u32) -> bilibili::FavFolder {
        bilibili::FavFolder {
            id,
            name: format!("folder-{id}"),
            count,
        }
    }

    #[test]
    fn pick_two_is_distinct_and_in_bounds() {
        let items: Vec<bilibili::FeedVideo> = (0..3).map(|i| video(&format!("BV{i}"))).collect();
        for _ in 0..32 {
            let picks = fav_pick_two(&items);
            assert_eq!(picks.len(), 2);
            assert_ne!(picks[0].bvid, picks[1].bvid);
            for pick in &picks {
                assert!(items.iter().any(|item| item.bvid == pick.bvid));
            }
        }
    }

    #[test]
    fn pick_two_handles_empty_and_single_pages() {
        assert!(fav_pick_two(&[]).is_empty());
        let one = [video("BV1")];
        let picks = fav_pick_two(&one);
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].bvid, "BV1");
    }

    #[test]
    fn push_unique_keeps_the_first_occurrence() {
        let mut cards = vec![video("BV1")];
        let mut seen: HashSet<String> = cards.iter().map(|video| video.bvid.clone()).collect();
        let more = [video("BV1"), video("BV2"), video(""), video("BV2"), video("BV3")];
        fav_push_unique(&mut cards, &mut seen, &more);
        let bvids: Vec<&str> = cards.iter().map(|video| video.bvid.as_str()).collect();
        assert_eq!(bvids, vec!["BV1", "BV2", "BV3"]);
    }

    #[test]
    fn largest_folders_prefers_count_and_caps_at_eight() {
        let folders: Vec<bilibili::FavFolder> = (0..10).map(|id| folder(id, id as u32)).collect();
        let picked = fav_largest_folders(&folders);
        assert_eq!(picked.len(), FAV_MIX_MAX_FOLDERS);
        // 9..2 by count, descending.
        assert_eq!(picked[0].count, 9);
        assert_eq!(picked[7].count, 2);
        assert!(fav_largest_folders(&[]).is_empty());
    }

    #[test]
    fn aspect_comes_from_the_decoded_cover() {
        let square: CoverPixels = (vec![0; 4], 100, 100);
        assert!((fav_aspect(&square) - 1.0).abs() < f32::EPSILON);
        let tall: CoverPixels = (vec![0; 4], 200, 300);
        assert!((fav_aspect(&tall) - 1.5).abs() < 0.0001);
        // Unknown dimensions fall back to the neutral 16:9.
        assert_eq!(fav_aspect(&(vec![0; 4], 0, 120)), FAV_DEFAULT_ASPECT);
        assert_eq!(fav_aspect(&(vec![0; 4], 120, 0)), FAV_DEFAULT_ASPECT);
    }

    #[test]
    fn shuffle_keeps_every_item() {
        let mut items: Vec<bilibili::FeedVideo> =
            (0..45).map(|i| video(&format!("BV{i}"))).collect();
        fav_shuffle(&mut items);
        let mut bvids: Vec<String> = items.iter().map(|video| video.bvid.clone()).collect();
        bvids.sort();
        let mut expected: Vec<String> = (0..45).map(|i| format!("BV{i}")).collect();
        expected.sort();
        assert_eq!(bvids, expected);
        // Degenerate lengths must not panic.
        let mut empty: Vec<bilibili::FeedVideo> = Vec::new();
        fav_shuffle(&mut empty);
        let mut one = vec![video("BV1")];
        fav_shuffle(&mut one);
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn view_test_tabs_cover_all_or_one() {
        assert_eq!(fav_view_test_tabs("all", 3), vec![0, 1, 2, 3]);
        assert_eq!(fav_view_test_tabs("2", 3), vec![2]);
        assert_eq!(fav_view_test_tabs("9", 3), vec![3]);
        assert_eq!(fav_view_test_tabs("", 2), vec![0]);
    }

    #[test]
    fn mix_key_changes_invalidate_the_cached_stream() {
        // The cache is process-wide; use ids no other test touches and clear
        // the mixed entry afterwards so ordering cannot leak between tests.
        let key = vec![-9001, -9002];
        fav_mixed_store(key.clone(), &[video("BV1")], 2);
        assert!(fav_mixed_cached(&key).is_some());
        assert!(fav_mixed_cached(&[-9001]).is_none());
        let hit = fav_mixed_cached(&key);
        assert_eq!(hit.map(|entry| entry.sources), Some(2));
        if let Ok(mut guard) = fav_view_cache().lock() {
            guard.mixed = None;
            guard.mix_key.clear();
            guard.folders.remove(&-9001);
        }
    }
}

#[cfg(test)]
mod up_resolver_tests {
    use super::*;

    fn store(items: &[(i64, &str)]) -> Arc<Mutex<Vec<bilibili::FeedVideo>>> {
        let videos: Vec<bilibili::FeedVideo> = items
            .iter()
            .map(|(mid, bvid)| bilibili::FeedVideo {
                bvid: (*bvid).to_string(),
                owner_mid: *mid,
                ..bilibili::FeedVideo::default()
            })
            .collect();
        Arc::new(Mutex::new(videos))
    }

    /// The card's UP name must resolve against the view on screen, never against
    /// the home feed: feed rows, the 用户 search tab, a favourite folder page and
    /// the history list each answer from their own list, and the UP overlay's own
    /// page wins while it is open.
    #[test]
    fn card_mid_follows_the_view_on_screen() {
        let feed = store(&[(11, "BVfeed")]);
        let masonry = store(&[(66, "BVmasonry")]);
        let up = store(&[(22, "BVup")]);
        let search = Arc::new(Mutex::new(SearchState {
            tab: 1,
            mids: vec![33, 0],
            ..SearchState::default()
        }));
        let folder = Arc::new(Mutex::new(FolderState {
            mids: vec![44],
            ..FolderState::default()
        }));
        let history = Arc::new(Mutex::new(HistoryPageState {
            mids: vec![0, 55],
            ..HistoryPageState::default()
        }));

        let mid = |view, index, up_open, on_masonry| {
            resolve_card_mid(
                view,
                index,
                up_open,
                on_masonry,
                &up,
                &feed,
                &masonry,
                &search,
                &folder,
                &history,
            )
        };

        // View 0: the feed row carries its owner; the 收藏推荐 tab answers from
        // its own masonry list instead.
        assert_eq!(mid(0, 0, false, false), Some(11));
        assert_eq!(mid(0, 0, false, true), Some(66));
        // The UP overlay's list wins while the page is open.
        assert_eq!(mid(0, 0, true, false), Some(22));
        // 用户 search row, favourite-folder row, second history row.
        assert_eq!(mid(1, 0, false, false), Some(33));
        assert_eq!(mid(4, 0, false, false), Some(44));
        assert_eq!(mid(5, 1, false, false), Some(55));
        // A missing owner, a stale index or a view with no mids never guess.
        assert_eq!(mid(1, 1, false, false), None);
        assert_eq!(mid(5, 9, false, false), None);
        assert_eq!(mid(13, 0, false, false), None);
        assert_eq!(mid(0, -1, false, false), None);
    }
}


