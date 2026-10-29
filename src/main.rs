#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Bilibili QR-code login desktop client.
//!
//! All network work runs on a background thread (its own tokio runtime) and
//! pushes results to the UI through `slint::invoke_from_event_loop`. The UI
//! thread is never blocked. A generation counter makes stale attempts inert.


pub(crate) use std::cell::{Cell, RefCell};
pub(crate) use std::collections::{HashMap, HashSet, VecDeque};
pub(crate) use std::path::PathBuf;
pub(crate) use std::rc::Rc;
pub(crate) use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicUsize, Ordering};
pub(crate) use std::sync::{Arc, Mutex, OnceLock};
pub(crate) use std::time::{Duration, Instant};

pub(crate) use anyhow::{anyhow, Result};
pub(crate) use futures::StreamExt;
pub(crate) use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel, Weak};
pub(crate) use windows_sys::Win32::Foundation::{HWND, RECT};
pub(crate) use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
pub(crate) use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsZoomed,
    SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
};

mod accounts;
mod autostart;
mod backdrop;
mod bilibili;
mod danmaku;
mod download;
mod fish;
mod fishwatch;
mod job;
mod media_hooks;
mod netpool;
mod player;
mod player_view;
mod prefetch; // the player wires it in
mod qa;
mod settings;
mod theme;
mod tray;
mod wbi;
mod win;

slint::include_modules!();

mod app;
pub(crate) use app::*;

pub(crate) use qa::*;













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




thread_local! {
    static TOAST_QUEUES: RefCell<Vec<ToastQueue>> = RefCell::new(Vec::new());
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











thread_local! {
    /// The window-move probe runs at most once per process.
    static WINDOW_PROBE_STARTED: Cell<bool> = Cell::new(false);
    /// Keeps the probe's timer alive (the closure holds it weakly, like the
    /// player loops, so it stops itself after the last step).
    static WINDOW_PROBE_TIMER: RefCell<Option<Rc<slint::Timer>>> = RefCell::new(None);
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









thread_local! {
    static PLAYLIST: RefCell<Option<Rc<Playlist>>> = RefCell::new(None);
    /// The 番剧 page's UI-thread opener: a season fetch runs on a netpool
    /// worker, but `open_player_inner` needs the UI-thread `Rc<PlayerState>`.
    /// The worker only posts the finished season back; the event-loop closure
    /// reads this host here, where an Rc is legal.
    static PGC_PLAYER_HOST: RefCell<Option<PgcPlayerHost>> = RefCell::new(None);
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



thread_local! {
    static PLAYLIST_TEST_TIMER: RefCell<Option<Rc<slint::Timer>>> = RefCell::new(None);
}




// ===================== 缩放进度条: 20-cell heat ===========================
//
// `ui/app.slint` derives the scrub window (base/span) from `scrub_level` +
// `scrub_cell_*` and fires `scrub_level_changed(level)`. Rust answers with
// exactly 20 densities for that window, taken from the curve the seek bar
// already has (`density_buckets`), normalised to the WINDOW's own peak. No
// refetch and no work per frame: only a zoom gesture recomputes it.














thread_local! {
    /// Last ordinal handed to a player window (UI thread only).
    static PLAYER_SESSION_IDS: Cell<u64> = Cell::new(0);
}




















thread_local! {
    static CLOSE_TARGETS: RefCell<Option<CloseTargets>> = RefCell::new(None);
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








// The live `PlayerState` for the wallpaper republish, which runs inside an
// `invoke_from_event_loop` closure: that closure must be `Send` while
// `Rc<PlayerState>` is not, and the closure does execute on the UI thread, so
// the ordinary UI-thread handle lives here.
thread_local! {
    static WALLPAPER_STATE: RefCell<Option<Rc<PlayerState>>> = const { RefCell::new(None) };
}












thread_local! {
    /// Timers owned by the QA harnesses. A Slint timer stops as soon as its last
    /// handle drops, and a harness starts its timers from inside a one-shot
    /// closure, so the handle is kept alive here for the process (diagnostics
    /// only; nothing is pushed outside a QA run).
    static QA_TIMERS: RefCell<Vec<Rc<slint::Timer>>> = RefCell::new(Vec::new());
    /// The main window's production drag state, exposed to the f146 replay so
    /// it can prove that a press starting on the bar (with 播放列表 open) really
    /// enters the drag path, then drive the drag's own placement math.
    static MAIN_DRAG: RefCell<Option<Rc<win::WindowDrag>>> = const { RefCell::new(None) };
    /// QA-only: stand in for a physically held left button during a synthetic
    /// drag. The drag timer and the end handler read the real VK_LBUTTON, which
    /// a replay never presses; this flag keeps the drag alive exactly as a held
    /// button would. Production leaves it false.
    static QA_DRAG_HOLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
//     空间) show exactly one API page per screen, so their client page
//     is the API page and the offset stays 0;
//   * sections whose data comes in fixed-size batches or pages (首页推荐 /
//     历史 / 动态 / 稍后再看 / 排行榜) keep several client pages per
//     loaded data set; the counter walks those pages and the next one past the
//     end fetches (append for the cursors, a new batch for the feed).
// ---------------------------------------------------------------------------



thread_local! {
    /// The client pages of the sections, UI-thread only like the other drivers
    /// (the page dispatcher and every loader's UI hand-off run here).
    static CLIENT_PAGES: RefCell<ClientPages> = RefCell::new(ClientPages::default());
}

















// ===================== f145 QA: player chrome / sliders / playlist ==========
//
// One env-driven run (`BILI_F145_TEST=1`) that drives the REAL windows with
// Slint pointer/wheel events (no OS input) and captures what the owner asked
// for. Every measurement is logged with the `f145 ` prefix, so the report can
// quote it verbatim.
//
// The run is on a background thread; every window/timer touch goes through
// `invoke_from_event_loop`. Pointer replays themselves run on the UI thread's
// event loop at 8ms per step (~125Hz), so the value-update counters are
// measured against a real repaint cadence, not a blocked loop.

use std::sync::mpsc::Sender;



















// ===================== f146 QA: wallpaper / popovers / playlist =============
//
// One env-driven run (`BILI_F146_TEST=1`) that proves the four f146 contracts:
// the theme-bound wallpaper (luma per theme), both popover centres, the taller
// shadowless playlist and its real window drag while open, and the
// direct-open row lifecycle. Every number is logged with the `f146 ` prefix.











// ===================== f147/f148 QA: UI wave ================================
//
// `BILI_F147_TEST=1` or `BILI_F148_TEST=1` drives the real callbacks through
// one run and captures every contract of the wave (the prefix only names the
// evidence; the tree under test is identical):
//   * search empty -> hot panel, then a result page with home cards
//   * the 180ms morph's UI-thread gap distribution (p95)
//   * pre-roll scrim: 150ms (no frame) and once a frame exists
//   * interpolation mode 0..3, read back from the control in the same turn
//   * 下载分布 playhead invariant while the picture runs at >= 28 fps
//   * three playlist rows with advancing preload rings; remove deletes cache
//   * vinyl: B at index 0, close B -> A index 0 + playing, close A -> cleared
//
// Every number uses the wave's prefix (`f147 ` / `f148 `), so a run's log can
// be asserted with grep. Captures land in the shared QA directory
// (`qa_capture_path`). The f147 code below is parameterised by `QA_WAVE`, so
// the same chain also produces the f148 evidence without a second copy.









thread_local! {
    static F147_PROBE: RefCell<Option<F147Probe>> = RefCell::new(None);
    static F147_GAPS: RefCell<Vec<f64>> = RefCell::new(Vec::new());
    /// Long-gap event lines, buffered: writing them from inside the 4 ms
    /// timer would put a synchronous file append on the very thread being
    /// measured and amplify the next gap. They are flushed after the sample.
    static F147_EVENTS: RefCell<Vec<String>> = RefCell::new(Vec::new());
    static F147_LAST: Cell<Option<Instant>> = Cell::new(None);
}
















// ===================== f149 QA: paint cost / 120 fps =======================
//
// `BILI_F149_TEST=1` drives the real home page and the real player window and
// measures what the f149 wave changed:
//   * UI-thread frame gap (4ms probe, p50/p95/p99) while a 120Hz wheel replay
//     scrolls the home feed, and again while the 180ms search morph runs;
//   * the number of live `GlassPanel`s and of panels that paint the wallpaper
//     image on the home screen (Rust resets the counters, re-enters the home
//     page, then reads them);
//   * one unified search panel: empty (热搜), typing with no suggestions yet
//     (热搜 still), suggestions arrived (联想, same height);
//   * toolbar / interp / playlist captures;
//   * the wallpaper origin and pixels of both windows at the same screen
//     offset modulo the 1024px tile, in light mode.
// Every line is prefixed `f149 `; captures go to `BILI_CAPTURE_DIR`.

thread_local! {
    static F149_PROBE: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
    static F149_GAPS: RefCell<Vec<f64>> = const { RefCell::new(Vec::new()) };
    static F149_PROBE_LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    static F149_WHEEL_TIMER: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
    static F149_PHASE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Long gaps (>10 ms) with their offset into the measured window, flushed
    /// after the timer stops. This is what says whether a morph spike is the
    /// first layout frame or a periodic stall.
    static F149_EVENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}












// ===================== f150 QA: search flash / panel width / seek / 3 players ==
//
// One env-driven run (`BILI_F150_TEST=1`) that proves the four f150 contracts on
// the REAL windows:
//   1. the 180ms search morph changes nothing outside the bar: three
//      PrintWindow captures at 0/90/180ms, plus the max per-channel pixel diff
//      of every pixel outside the bar's swept rect (panel hidden, so the bar is
//      the only intended change);
//   2. the hot/suggest panel is the bar's field rect (bar x+12, width-24);
//   3. a real seek-bar release publishes the target to the shown position and
//      the readout in the same event (`seek display target=.. shown=.. +0ms`),
//      and keeps it until the real playhead lands;
//   4. three player windows all decode/present without focus, and a fourth
//      shows the explicit 最多同时播放 3 个 queue state, then starts when one of
//      the first three closes.
// Captures land in `BILI_CAPTURE_DIR`; every line is prefixed `f150 `.












// ===================== f151 QA: frozen seek + theme chrome =================
//
// `BILI_F151_TEST=1` drives the REAL windows and proves the three f151 items:
//   1. A release freezes the picture in the release event. The release frame
//      and the +100ms frame are compared pixel by pixel over the video band
//      (danmaku off, pill excluded): identical while `presented_frames` did not
//      move. The +50/+200/+400/+600 ms receipts come from the seek callback in
//      `run_player` (`seek pic=… overlay=… pos_shown=… pos_real=…`), and the
//      swap's A/V relation comes from the `seek swap` line (delta_ms).
//   2. The interp cell is measured against the speaker / danmaku / comment
//      cells (x/y/w/h/radius/font) and captured in all four modes.
//   3. The home bar, its search popover, the player pill and its volume popover
//      are sampled from the live surfaces in light and dark: mean RGB and the
//      channel cast.
// Captures land in `BILI_CAPTURE_DIR`, every line is prefixed `f151 `.












// ===================== f152 QA: occlusion pause + manual morph clock =======
//
// `BILI_F152_TEST=1` drives the REAL windows and proves the two f152 items:
//   1. Two player windows are placed on the SAME physical rect and raised. The
//      back one must log `player occluded pause` after 300 ms of complete
//      coverage, its audio session must read Inactive and its picture must stop
//      advancing; moving the front window away must log
//      `player occluded resume`, turn the session Active and advance the
//      picture again within 300 ms. A 20 s side-by-side window (with a brief
//      PARTIAL overlap first) must log no occlusion pause at all.
//   2. 20 search opens are measured on the manual 8 ms morph clock: per-open
//      tick count, distinct `morph` values and the per-frame gap p50/p95/p99.
//      Captures at 0/60/120/180 ms prove the same endpoints as before.
// Captures land in `BILI_CAPTURE_DIR`, every line is prefixed `f152 `.



thread_local! {
    /// One entry per morph clock tick: when it landed and the value it set.
    static F152_MORPH: RefCell<Vec<(Instant, f32)>> = const { RefCell::new(Vec::new()) };
}
















// ===================== f154 QA: which renderer, and what a frame costs =====
//
// Two diagnostic jobs, neither of which changes a normal run:
//
//   1. Say WHICH renderer is painting, from three angles that can disagree:
//      the `SLINT_BACKEND` request, the renderer features compiled into this
//      binary, and the GPU libraries the process has actually loaded.
//   2. Run the f152 20-open protocol once per paint layer (`BILI_QA_*`), so the
//      per-frame gap is attributed to the wallpaper tiles / the glass panels /
//      the drop shadows / the cover art / the rounded-clip layers.
//
// `BILI_F154_TEST=1` starts (2); `BILI_F154_TAG` names the run in the log and
// in the two capture files (`f154_<tag>_0.png`, `f154_<tag>_180.png`).














thread_local! {
    /// UI-thread CPU clock for one open: `GetThreadTimes` kernel+user of the
    /// thread that runs the event loop, sampled from that thread itself.
    static F154_CPU_START: Cell<Option<f64>> = const { Cell::new(None) };
}







// ===================== f174 QA: a sink for the layers below the app =========
//
// `wgpu`, `wgpu_hal` and Slint's own renderers report through `log`, and until
// f174 nothing in this process installed a logger, so every one of their
// records was dropped. That is why a wgpu surface failure could only show its
// *summary*: `WGPU surface validation error in get_current_texture`, printed by
// `main`'s `Result` - the error behind it (a lost device, an invalid surface,
// an already-acquired frame) was formatted by wgpu and thrown away.
//
// `BILI_LOG=<level>` installs the sink: `error` (or `1`/`on`/`yes`), `warn`,
// `info`, `debug`, `trace`. `BILI_LOG_TARGETS=<csv prefixes>` narrows the
// modules kept, default `wgpu` - a record is kept when its target starts with
// one of the prefixes, so `wgpu` covers `wgpu_core`, `wgpu_hal` and their
// children, and `slint,wgpu` adds the renderer. Lines go to `BILI_DEBUG_LOG`
// (the file the rest of the app writes) when that is set, else to stderr; a
// release build has no console, so the file is the useful one.
//
// Without `BILI_LOG` the max level stays `Off`: no formatting, no allocation,
// nothing written, and the sink costs one atomic load per record that never
// happens.

/// The `log` sink: filters by level and target prefix, writes one line.
struct F174LogSink {
    targets: Vec<String>,
}

impl F174LogSink {
    fn wants(&self, metadata: &log::Metadata<'_>) -> bool {
        let target = metadata.target();
        self.targets.iter().any(|prefix| target.starts_with(prefix.as_str()))
    }
}

impl log::Log for F174LogSink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level() && self.wants(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        f174_emit(&format!(
            "log {} {} {}",
            record.level(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {}
}

/// One sink line to the place the rest of the diagnostics go.
fn f174_emit(line: &str) {
    static TO_FILE: OnceLock<bool> = OnceLock::new();
    let to_file = *TO_FILE
        .get_or_init(|| std::env::var_os("BILI_DEBUG_LOG").is_some_and(|v| !v.is_empty()));
    if to_file {
        crate::debug_log(line);
    } else {
        eprintln!("{line}");
    }
}

/// `BILI_LOG` / `BILI_LOG_TARGETS`: install [`F174LogSink`] once, before the
/// first window exists (the renderer's own start-up lines are part of what a
/// crash report needs).
fn f174_install_log_sink() {
    let Some(raw) = std::env::var("BILI_LOG").ok().filter(|v| !v.trim().is_empty()) else {
        return;
    };
    let level = match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "yes" | "true" | "error" => log::LevelFilter::Error,
        "warn" | "warning" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        "off" | "0" | "no" | "false" => return,
        other => {
            crate::debug_log(&format!("f174 log level ignored value={other}"));
            return;
        }
    };
    let targets = std::env::var("BILI_LOG_TARGETS")
        .unwrap_or_else(|_| "wgpu".to_string())
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let sink = F174LogSink { targets: targets.clone() };
    if log::set_boxed_logger(Box::new(sink)).is_ok() {
        log::set_max_level(level);
        crate::debug_log(&format!(
            "f174 log sink level={level} targets={:?}",
            targets
        ));
    }
}

// ===================== f174 QA: the surface / playback soak =================
//
// `BILI_SOAK_TEST=<seconds>` is the "the GPU backend survived a real session"
// proof, and the one run that separates a backend that *renders* from a
// backend that *keeps rendering*. It opens nothing itself: combine it with
// `BILI_OPEN_BVID=<bvid>` (which opens a real player through the normal path)
// or with `BILI_F152_TEST=1` (the cached track).
//
// Every 5 s it logs the active player's counters - the pacer's own presented
// frame delta over that window, `fps_presented`, the playhead, ring occupancy,
// buffering and stalls - and at 40 s / 80 s it issues a real `seek` through the
// player's own entry point (on the UI thread, like a click) so a session that
// only breaks on a seek or a track restart cannot pass. At `<seconds>` it logs
// one `f174 soak done` summary and asks the event loop to return, so a scripted
// run proves survival from the log AND from the exit code.
//
// A wgpu surface failure shows up as the process dying mid-soak (`Error: WGPU
// surface validation error in get_current_texture`, exit code 1), which leaves
// the soak line train short: the last `t=` is the failure window.

/// `BILI_SOAK_TEST=<seconds>`: see the block comment above.
fn f174_spawn_soak() {
    let Ok(raw) = std::env::var("BILI_SOAK_TEST") else {
        return;
    };
    let seconds = match raw.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        Ok(_) => return,
        Err(err) => {
            crate::debug_log(&format!("f174 soak ignored value={raw} err={err}"));
            return;
        }
    };
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut last_frames = 0u64;
        let mut last_at = Instant::now();
        let mut fps_min = f64::MAX;
        let mut fps_max = 0.0f64;
        let mut windows = 0usize;
        let mut seek_targets = vec![40u64, 80u64];
        crate::debug_log(&format!("f174 soak start seconds={seconds}"));
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let elapsed = started.elapsed().as_secs();
            let control = player::active_control();
            if let (Some(control), Some(stats)) = (control.as_ref(), player::stats_snapshot()) {
                let frames = control.presented_frames();
                let window = last_at.elapsed().as_secs_f64().max(0.001);
                let fps = frames.saturating_sub(last_frames) as f64 / window;
                last_frames = frames;
                last_at = Instant::now();
                let advancing = !control.paused() && !control.buffering() && fps > 1.0;
                if advancing {
                    windows += 1;
                    fps_min = fps_min.min(fps);
                    fps_max = fps_max.max(fps);
                }
                crate::debug_log(&format!(
                    "f174 soak t={elapsed}s fps_window={fps:.2} fps_presented={:.2} frames={frames} paused={} buffering={} ring={} pos_ms={} stalls={}",
                    stats.fps_presented,
                    control.paused(),
                    control.buffering(),
                    control.ring_occupancy(),
                    stats.position_ms,
                    stats.stalls
                ));
            } else {
                // No player yet (open is still resolving) or it just closed.
                last_frames = 0;
                last_at = Instant::now();
                crate::debug_log(&format!("f174 soak t={elapsed}s player=none"));
            }
            if let Some(target) = seek_targets.first().copied() {
                if elapsed >= target {
                    seek_targets.remove(0);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(control) = player::active_control() else {
                            return;
                        };
                        // Half the way in, so the seek lands in the middle of
                        // the media rather than at its last second.
                        let duration = control.duration_ms();
                        let at = if duration > 0 { duration / 2 } else { 30_000 };
                        crate::debug_log(&format!(
                            "f174 soak seek t={target}s target_ms={at} duration_ms={duration}"
                        ));
                        control.seek(at);
                    });
                }
            }
            if elapsed >= seconds {
                crate::debug_log(&format!(
                    "f174 soak done seconds={elapsed} windows={windows} fps_min={} fps_max={}",
                    if fps_min == f64::MAX { 0.0 } else { fps_min },
                    fps_max
                ));
                let _ = slint::invoke_from_event_loop(|| request_quit("f174 soak done"));
                return;
            }
        }
    });
}

// ===================== f155 QA: wgpu default, and what it must not break ====
//
// `BILI_F155_TEST=1` walks the pages once, PrintWindow-captures each of them,
// then opens a real player and leaves it playing for 60 s:
//
//   * every capture goes through `PrintWindow` with NO screen fallback, which
//     is the check a wgpu flip-model swapchain can fail (black frame): the line
//     prints the PrintWindow return code, the mean luma, the black-pixel share
//     and the max/mean diff against the same rect grabbed off the screen;
//   * the player's frame size/qn is logged so the same run proves 1080p;
//   * while that player is still playing, five search morphs on the 4 ms
//     UI-thread probe measure whether wgpu's presentation steals the thread.
//
// Line prefix is `f155 `, captures land in `BILI_CAPTURE_DIR` as
// `f155_<page>_<theme>.png`.









// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).
// Product-side: every run calls this (see the `qa` feature note).







pub(crate) fn main() -> Result<()> {
    // f174: the sink for everything below the app (wgpu, wgpu_hal, the
    // renderers). First statement that can run: the renderer's own start-up
    // records are part of what a crash report needs.
    f174_install_log_sink();

    // The pipeline's diagnostic hooks (thread roles, trace tags, disk queue,
    // startup gate). Installed before the first window or worker exists so
    // `prefetch` can report into `player` without referring to it.
    media_hooks::install(&AppMediaHooks);

    // f159: the cold-start reference. First statement on purpose - everything
    // the process does after this point counts toward the first visible screen.
    f159_boot_mark();
    // f174: the renderer, decided by measurement. Default = femtovg on wgpu.
    //
    //   renderer        rounded clips  playback       morph p50/p95     stable
    //   femtovg/OpenGL  yes            30 fps steady  25.67/32.81 ms    yes
    //   software        NO (squares)   30 fps steady   8.57/11.57 ms    yes
    //   femtovg/wgpu    yes            30 fps steady   8.29/ 9.71 ms    yes*
    //   skia            yes            NO picture      8.26/10.30 ms    ?
    //
    // Only wgpu is both fast enough for the 120 Hz morph AND correct on the
    // rounded clips; OpenGL's clip layer alone costs ~17 ms per frame
    // (`BILI_QA_NO_CLIP=1` measures 8.52/10.57 without it), and the software
    // renderer paints every `clip: true` + `border-radius` subtree as a black
    // square. (*) wgpu used to exit ~55 s into a session with
    // `WGPU surface validation error in get_current_texture`; f174 could not
    // reproduce it in >12 min of wgpu sessions (140 s/200 s soaks, f152, f155)
    // and identified the string as wgpu's device-LOST status, so the default is
    // wgpu again. The escape hatches stay in the binary, unchanged:
    //   SLINT_BACKEND=winit-femtovg   -> the f173 default (OpenGL)
    //   SLINT_BACKEND=winit-software  -> the CPU renderer
    //   WGPU_BACKEND=dx12|vulkan      -> which wgpu backend the default uses
    // Nothing is forced here, so an explicit SLINT_BACKEND always wins (the
    // decision and the feature list live in `Cargo.toml`).
    // Priority, once, on this thread (the UI thread for the whole process:
    // `main` creates every Slint window and runs the event loop). The process
    // stays at NORMAL_PRIORITY_CLASS - so everything it spawns (pacer, pipe
    // readers, ffmpeg/ffplay) gets a fair share of a busy machine instead of
    // being scheduled out - and this thread goes to THREAD_PRIORITY_HIGHEST
    // inside that class, so the window always beats our own workers.
    // Throughput-only workers lower themselves (`netpool`, the pipe reader).
    // See `win::tune_priorities` for the measurements behind this.
    //
    // First statement on purpose: the class must be in place before the first
    // worker thread is created.
    win::tune_priorities();
    debug_log("priority process=normal ui-thread=highest");

    // Born frameless: the WH_CBT create hook rewrites winit's CREATESTRUCTW
    // style before the first window exists, so the native title bar is never
    // painted - not even for one frame. Must precede AppWindow::new() below.
    win::install_create_hook();

    // Kill all ffmpeg/ffplay children when this process dies, even abruptly.
    job::init();

    // `BILI_IDLE_TEST=1`: exercise the chrome idle state machine with fake
    // cursor samples (no synthetic input, no window touched).
    spawn_idle_self_test();

    // f174: `BILI_SOAK_TEST=<seconds>`, the surface/playback soak (pairs with
    // `BILI_OPEN_BVID`). Reads the player's own counters from a worker thread,
    // so it never touches the UI thread between samples.
    f174_spawn_soak();

    // 1 ms timer resolution before the first timer starts: the drag flush is
    // 8 ms (120 Hz) and the pacer's frame sleeps depend on the same winmm
    // refcount. One `Once` for the whole process, never a second period call.
    player::ensure_high_res_timer();

    let ui = AppWindow::new()?;
    let weak = ui.as_weak();
    // f154: which renderer Slint is about to use, and the QA paint switches
    // (all off unless a `BILI_QA_*` variable is set; inert otherwise).
    f154_install_renderer_probe();
    f154_apply_qa_flags(&ui);
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
    WALLPAPER_STATE.with(|slot| *slot.borrow_mut() = Some(player_state.clone()));

    // `BILI_STALL_TRACE=1`: the UI-thread gap detector for this event loop. The
    // main, player and mini windows all live on this one Slint thread, so a
    // single detector covers all of them; the sample says which one was showing
    // and carries that player's own counters.
    {
        let state = player_state.clone();
        player::install_stall_trace(move || {
            let session = state
                .focused
                .try_borrow()
                .ok()
                .and_then(|focused| focused.clone())
                .or_else(|| {
                    state
                        .players
                        .try_borrow()
                        .ok()
                        .and_then(|players| players.last().cloned())
                });
            match session {
                Some(session) => player::StallSample {
                    view: "player",
                    playing: !session.control.paused(),
                    buffering: session.control.buffering(),
                    ring: session.control.ring_occupancy(),
                    frames: session.control.presented_frames(),
                },
                None => player::StallSample {
                    view: "main",
                    playing: false,
                    buffering: false,
                    ring: 0,
                    frames: 0,
                },
            }
        });
    }

    // Persisted preferences (settings.json), shared by the player controls and
    // the settings panel so both stay in sync.
    let settings_store: Arc<Mutex<settings::Settings>> = Arc::new(Mutex::new(settings::load()));

    // 背景模糊 is applied to `backdrop` BEFORE the window's first paint: the
    // first frame is already the stored radius, not a 60 px flash. (The window
    // is not shown until `ui.run()` below.)
    if let Ok(guard) = settings_store.lock() {
        backdrop::set_blur(guard.backdrop_blur);
        crate::debug_log(&format!(
            "appearance loaded backdrop_blur={} (applied before first paint) transparency={}",
            backdrop::blur_px(),
            guard.glass_transparency
        ));
    }

    // The network thread pool: created once, here, before the first request and
    // before any player session can bind its fetch slots. Size comes from
    // 设置 → 优化 (`net_threads`, default 12) and `BILI_NET_THREADS` can override
    // it for an A/B. `netpool::init` clamps to 2..=32 and logs
    // `netpool init threads=N`.
    {
        let (configured, override_value) = (
            settings_store
                .lock()
                .map(|guard| guard.net_threads)
                .unwrap_or_else(|poisoned| poisoned.into_inner().net_threads),
            std::env::var("BILI_NET_THREADS").ok(),
        );
        let threads = override_value
            .as_deref()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(configured as usize);
        crate::debug_log(&format!(
            "netpool config settings_net_threads={configured} env={} resolved={threads}",
            override_value.as_deref().unwrap_or("-")
        ));
        netpool::init(threads);
        let stats = netpool::stats();
        crate::debug_log(&format!(
            "netpool ready threads={} running={} queued={} completed={}",
            stats.threads, stats.running, stats.queued, stats.completed
        ));
    }

    // Resolve light/dark before the first frame so 跟随系统 never flashes light.
    let theme_dark = settings_store
        .lock()
        .map(|guard| theme::resolve(guard.theme))
        .unwrap_or(false);
    player_state.theme_dark.set(theme_dark);
    ui.global::<AppTheme>().set_dark(theme_dark);
    // The window backdrop is available before the first paint: no capture, no
    // worker, no "transparent until the first frame" state.
    set_backdrop(&ui, theme_dark);
    // The transparency slider as well, so the settings panel starts from the
    // stored value rather than from the default.
    // The cards-per-page setting drives the Rust-owned `list_page_size`
    // property from here on; the UI only reports menu changes back.
    if let Ok(guard) = settings_store.lock() {
        ui.global::<AppTheme>()
            .set_glass_transparency(guard.glass_transparency);
        ui.global::<AppTheme>()
            .set_backdrop_blur(guard.backdrop_blur as i32);
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

    let folder_state: Arc<Mutex<FolderState>> = Arc::new(Mutex::new(FolderState::default()));
    let folder_bvids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // 收藏 (推荐流 / 收藏夹): the folder list, the selected folder and the cards
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
    // 番剧 (view_mode 22): page state, stale-load guard and the season ids that
    // back the two published row models.
    let pgc_host = PgcHost {
        state: Arc::new(Mutex::new(PgcState::default())),
        seq: Arc::new(AtomicU64::new(0)),
        covers: Arc::new(Mutex::new(Vec::new())),
        ep_covers: Arc::new(Mutex::new(Vec::new())),
        settings: settings_store.clone(),
    };
    install_pgc_page(&weak, &pgc_host);
    PGC_PLAYER_HOST.with(|cell| {
        *cell.borrow_mut() = Some(PgcPlayerHost {
            state: player_state.clone(),
            settings: settings_store.clone(),
        });
    });
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
    ui.set_pgc_tabs(ModelRc::from(std::rc::Rc::new(VecModel::from(
        pgc_tab_labels(),
    ))));
    ui.set_pgc_sorts(ModelRc::from(std::rc::Rc::new(VecModel::from(
        pgc_sort_labels(),
    ))));
    crate::debug_log(&format!(
        "pgc sorts labels={:?} stored={:?}",
        pgc_sort_labels()
            .iter()
            .map(|label| label.to_string())
            .collect::<Vec<_>>(),
        ui.get_pgc_sort()
    ));
    if let Ok(guard) = settings_store.lock() {
        publish_settings(&ui, &guard);
    }
    // 防干扰 readback: what the toolbar and the search panel actually render
    // from after this start-up, so a restart can be proven from the log alone.
    crate::debug_log(&format!(
        "settings guard loaded hide_live_tab={} hide_hot_search={} panel_live={} panel_hot={}",
        ui.get_hide_live_tab(),
        ui.get_hide_hot_search(),
        ui.get_set_hide_live(),
        ui.get_set_hide_hot_search()
    ));

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
        let up_page = up_page.clone();
        ui.on_up_toggle_follow(move || {
            toggle_up_follow(weak.clone(), session_slot.clone(), up_page.clone());
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
                    None,
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
    // Opening the empty field loads the hot searches for the panel under the
    // bar. A later query reuses the same list in `begin_search`.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let hot_keywords = hot_keywords.clone();
        ui.on_search_opened(move || {
            load_hot_search(weak.clone(), session_slot.clone(), hot_keywords.clone());
        });
    }
    // f152: the search morph runs on an explicit 8 ms clock. The UI samples the
    // process-monotonic clock through `mono_ms` and publishes each tick through
    // `morph_frame` (the QA gap table; harmless outside the QA run).
    ui.on_mono_ms(mono_ms_i32);
    ui.on_morph_frame(f152_note_morph);

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
        // The queue mark's own callback: it must NEVER open the card. The
        // player-open log (or its absence) next to this line is the proof.
        crate::debug_log(&format!(
            "playlist enqueue via=card-icon index={index} open=0"
        ));
        if let Some(host) = playlist() {
            // `only = true`: the mark queues without starting a player (the
            // old `false` auto-played the item when the queue was idle, which
            // looked like the card had opened). The toast is the feedback.
            host.enqueue_index(index, true);
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
    // 播放列表 header's play/pause: the focused player (or the newest live
    // one) toggles. The queue publishes immediately so the button icon and the
    // row's meter answer on the same click; the player's own tick confirms it.
    {
        let state = player_state.clone();
        ui.on_playlist_toggle_pause(move || {
            let session = focused_session(&state)
                .or_else(|| player_sessions(&state).last().cloned());
            let Some(session) = session else {
                return;
            };
            let paused = session.control.toggle_paused();
            session.window.set_playing(!paused);
            if let Some(host) = playlist() {
                host.set_playing(!paused);
            }
            crate::debug_log(&format!(
                "playlist pause win={} paused={}",
                session.id,
                u8::from(paused)
            ));
        });
    }
    // The UI advances `play_mode` itself and then calls this; Rust only
    // persists and publishes the value it received (never advances again).
    ui.on_cycle_play_mode(move || {
        if let Some(host) = playlist() {
            host.cycle_mode();
        }
    });
    // Diagnostic: name the path that opened/closed 播放列表. The only path that
    // OPENS it from the home chrome is the vinyl click (`source=click`); hover
    // has no write at all. Outside click / Esc / menu switches / Rust name
    // themselves, so `grep "playlist open source="` is the proof.
    ui.on_playlist_changed(|open, source| {
        crate::debug_log(&format!(
            "playlist open source={source} open={}",
            if open { 1 } else { 0 }
        ));
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
        let fav_view = fav_view.clone();
        ui.on_nav_favorites(move || {
            if let Some(ui) = weak.upgrade() {
                // 收藏夹 is the same page as 推荐流 (one strip, one masonry):
                // the route only switches the page's mode, then loads the
                // folder the strip already has selected in that mode.
                ui.set_view_mode(3);
            }
            fav_start(weak.clone(), session_slot.clone(), fav_view.clone());
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
        let fav_view = fav_view.clone();
        let folder_state = folder_state.clone();
        let folder_bvids = folder_bvids.clone();
        ui.on_open_fav_folder(move |fid| {
            let Ok(fid) = fid.parse::<i64>() else {
                return;
            };
            // The folder names live in the 收藏 page's own state (the strip's
            // list), so the folder-contents route reads them from there.
            let title = fav_view
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .folders
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

    // ---- 收藏 (推荐流 / 收藏夹): one folder strip, two tabs ----
    // The UI owns 全部 and lists only the user's own folders in `fav_tabs`;
    // `fav_tab == 0` is 全部 and `i + 1` is `fav_tabs[i]`. The same chip drives
    // both tabs: 推荐流 loads the 收藏推荐 feed for that folder, 收藏夹 loads
    // the folder's plain listing (see `FavMode`).
    // ---- 分区 / 收藏 menu: one 分区 callback dispatches to the loader ----
    // The menu row fires `home_section(idx)` only; the handler below maps it to
    // the same entry points the bar's legacy callbacks used, so picking a
    // section actually loads that feed. It logs every pick.
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let fav_view = fav_view.clone();
        let pgc_host = pgc_host.clone();
        ui.on_home_section(move |section| {
            crate::debug_log(&format!("section pick index={section} source=menu"));
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match section {
                0 => ui.invoke_back_to_feed(),
                1 => ui.invoke_nav_regions(),
                3 => pgc_open_page(weak.clone(), session_slot.clone(), pgc_host.clone()),
                5 => fav_start(weak.clone(), session_slot.clone(), fav_view.clone()),
                _ => {}
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let fav_view = fav_view.clone();
        ui.on_fav_tab_changed(move |tab| {
            // The same chip drives both tabs: the mode comes from the page the
            // UI is on (收藏夹 = view_mode 3), so folder X means "the 推荐流
            // feed seeded by X" on one tab and "X's plain listing" on the other.
            let mode = weak
                .upgrade()
                .map(|ui| fav_page_mode(&ui))
                .unwrap_or(FavMode::Stream);
            fav_load_tab(
                weak.clone(),
                session_slot.clone(),
                fav_view.clone(),
                tab,
                mode,
            );
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
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
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
            }));
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
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
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
            }));
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
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
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
            }));
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
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
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
            }));
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
                // 收藏夹内容 / 分区: one API page per screen, so the
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
                // 稍后再看 / 排行榜: single-shot lists, client-paged
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
        let session_slot = session_slot.clone();
        let region_state = region_state.clone();
        let browse_bvids = browse_bvids.clone();
        ui.on_nav_regions(move || {
            // One merged 分区 page: the strip + the 分区推荐/排行榜 toggle live
            // on views 8/9, so entering 分区 opens the selected region's feed
            // (or the first one when nothing was picked yet).
            let index = weak.upgrade().map(|ui| ui.get_region_index().max(0)).unwrap_or(0);
            let Some((_, rid)) = REGIONS.get(index as usize).copied() else {
                return;
            };
            if let Ok(mut state) = region_state.lock() {
                state.rid = rid;
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
                ui.set_region_index(index.max(0));
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

    // ---- 番剧 page (view_mode 22): tabs, 推荐/排行榜, sort, day pick, paging --
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_tab_pick(move |tab| {
            let tab = tab.clamp(0, 6);
            if let Ok(mut state) = pgc_host.state.lock() {
                state.tab = tab;
                state.sub = 0;
                state.page = 1;
                state.loading = false;
            }
            // The dropdown follows the season_type's own remembered sort.
            let sort = pgc_season_type(tab)
                .and_then(|season_type| {
                    pgc_host
                        .settings
                        .lock()
                        .ok()
                        .map(|prefs| pgc_stored_sort(&prefs, season_type))
                })
                .unwrap_or_else(|| settings::PGC_SORT_DEFAULT.to_string());
            if let Some(ui) = weak.upgrade() {
                ui.set_pgc_tab(tab);
                ui.set_pgc_sub(0);
                ui.set_pgc_sort(pgc_sort_index(&sort));
                ui.set_pgc_sort_open(false);
                ui.set_view_mode(22);
            }
            crate::debug_log(&format!(
                "pgc tab pick tab={tab} sort={sort} order={}",
                pgc_sort_order(&sort)
            ));
            if tab == 0 {
                pgc_load_timeline(weak.clone(), session_slot.clone(), pgc_host.clone());
            } else {
                pgc_load_index(
                    weak.clone(),
                    session_slot.clone(),
                    pgc_host.clone(),
                    1,
                    false,
                );
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_sub_pick(move |sub| {
            let sub = if sub == 1 { 1 } else { 0 };
            let tab = pgc_host
                .state
                .lock()
                .map(|state| state.tab)
                .unwrap_or(0);
            if tab == 0 {
                return;
            }
            if let Ok(mut state) = pgc_host.state.lock() {
                state.sub = sub;
                state.page = 1;
                state.loading = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_pgc_sub(sub);
                ui.set_pgc_sort_open(false);
                ui.set_view_mode(22);
            }
            crate::debug_log(&format!("pgc sub pick sub={sub} tab={tab}"));
            if sub == 1 {
                pgc_load_rank(weak.clone(), session_slot.clone(), pgc_host.clone());
            } else {
                pgc_load_index(
                    weak.clone(),
                    session_slot.clone(),
                    pgc_host.clone(),
                    1,
                    false,
                );
            }
        });
    }
    // ---- 番剧排序 (分区索引 only) ----
    {
        let weak = weak.clone();
        ui.on_pgc_sort_toggle(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_pgc_sort_open(!ui.get_pgc_sort_open());
                crate::debug_log(&format!("pgc sort menu open={}", ui.get_pgc_sort_open()));
            }
        });
    }
    {
        let weak = weak.clone();
        ui.on_pgc_sort_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_pgc_sort_open(false);
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_sort_pick(move |index| {
            let index = index.clamp(0, PGC_SORTS.len() as i32 - 1);
            let key = pgc_sort_key_at(index).to_string();
            let order = pgc_sort_order(&key);
            let Some(season_type) = pgc_host
                .state
                .lock()
                .map(|state| state.tab)
                .ok()
                .and_then(pgc_season_type)
            else {
                return;
            };
            // Persist per season_type, so 番剧 and 国创 keep their own choice
            // across a restart.
            let snapshot = update_settings(&pgc_host.settings, |prefs| {
                prefs
                    .pgc_sorts
                    .insert(season_type.to_string(), key.clone());
            });
            if let Ok(mut state) = pgc_host.state.lock() {
                state.sort = key.clone();
                // A sort change always lands on 推荐: the rank list has its own
                // fixed ordering.
                state.sub = 0;
                state.page = 1;
                state.loading = false;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_pgc_sort(index);
                ui.set_pgc_sort_open(false);
                ui.set_pgc_sub(0);
                ui.set_view_mode(22);
                // 滚动归零: the grid's own offset (read back into the log), so
                // the new first page is read from the top and no old card can
                // linger above it.
                ui.set_pgc_grid_scroll(0.0);
                crate::debug_log(&format!(
                    "pgc sort scroll reset={:.1}",
                    ui.get_pgc_grid_scroll()
                ));
            }
            crate::debug_log(&format!(
                "pgc sort pick season_type={season_type} key={key} order={order} stored={:?}",
                snapshot.pgc_sorts.get(&season_type.to_string())
            ));
            pgc_load_index(weak.clone(), session_slot.clone(), pgc_host.clone(), 1, false);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_day_pick(move |day| {
            crate::debug_log(&format!("pgc day pick day={day}"));
            pgc_select_day(
                weak.clone(),
                session_slot.clone(),
                pgc_host.clone(),
                day,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_more(move || {
            let (tab, sub, page, has_more, loading) = match pgc_host.state.lock() {
                Ok(state) => (
                    state.tab,
                    state.sub,
                    state.page,
                    state.has_more,
                    state.loading,
                ),
                Err(_) => return,
            };
            if tab == 0 || sub != 0 || !has_more || loading {
                return;
            }
            crate::debug_log(&format!("pgc more tab={tab} from_page={page}"));
            pgc_load_index(
                weak.clone(),
                session_slot.clone(),
                pgc_host.clone(),
                page + 1,
                true,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_retry(move || {
            pgc_reload(weak.clone(), session_slot.clone(), pgc_host.clone());
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let pgc_host = pgc_host.clone();
        ui.on_pgc_open(move |index| {
            pgc_open_row(
                weak.clone(),
                session_slot.clone(),
                pgc_host.clone(),
                index,
            );
        });
    }

    // A 分区 card was clicked: open the detail view for it.
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
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
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
            }));
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

    // Settings page: open (re-syncing from the stored preferences). It is a
    // normal view_mode tab (21), rendered inside the content column with the
    // bottom bar visible - not an overlay.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_open_settings(move || {
            if let Some(ui) = weak.upgrade() {
                if let Ok(guard) = settings_store.lock() {
                    publish_settings(&ui, &guard);
                }
                ui.set_settings_status(String::new().into());
                ui.set_view_mode(21);
                crate::debug_log(&format!(
                    "settings open view_mode={} account_overlay=0",
                    ui.get_view_mode()
                ));
            }
        });
    }
    // Where the VIP page returns to: 会员中心 lives in the settings page, so
    // `vip_back` restores the view that opened it (settings, 21) instead of
    // dropping the user on the feed.
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
            crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
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
            }));
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
                set_backdrop(&ui, dark);
                ui.global::<AppTheme>().set_dark(dark);
                win::set_immersive_dark(ui.window(), dark);
            }
            for session in player_sessions(&state) {
                set_player_backdrop(&session.window, dark);
                session.window.global::<AppTheme>().set_dark(dark);
                win::set_immersive_dark(session.window.window(), dark);
            }
            if let Some(mini) = state.mini.borrow().as_ref() {
                set_mini_backdrop(mini, dark);
                mini.global::<AppTheme>().set_dark(dark);
                win::set_immersive_dark(mini.window(), dark);
            }
        });
    }

    // 外观: the window-backdrop transparency slider. It writes the theme global
    // live while dragging (the tint follows immediately), so this only persists
    // the settled value.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        let state = player_state.clone();
        ui.on_settings_glass_changed(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let theme = ui.global::<AppTheme>();
            let transparency = settings::normalise_glass(
                theme.get_glass_transparency(),
                settings::default_glass_transparency(),
            );
            update_settings(&settings_store, |prefs| {
                prefs.glass_transparency = transparency;
            });
            // Every window's AppTheme is its own instance: push the settled
            // value into the live players/mini too, so their `window-bg`
            // (and therefore the wallpaper film) stays identical to home.
            for session in player_sessions(&state) {
                session
                    .window
                    .global::<AppTheme>()
                    .set_glass_transparency(transparency);
            }
            if let Some(mini) = state.mini.borrow().as_ref() {
                mini.global::<AppTheme>()
                    .set_glass_transparency(transparency);
            }
        });
    }

    // 外观: 背景模糊. The slider writes the live value into AppTheme while the
    // pointer moves (the label follows); this handler is the SETTLE path:
    //   * debounce 150 ms, so a drag costs ONE regeneration, not one per pixel;
    //   * `backdrop::set_blur` publishes the new radius (and invalidates the
    //     cached tile);
    //   * the tile is regenerated on a worker thread - the UI thread keeps
    //     painting the previous image instead of stalling for the blur;
    //   * on completion the UI thread installs it into the backdrop cache and
    //     republishes `AppTheme.glass-blur` + every window's wallpaper.
    // `backdrop blur=… gen_ms=…` is the measured generation cost.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        let debounce = Rc::new(slint::Timer::default());
        let pending = Rc::new(Cell::new(backdrop::blur_px()));
        let last_input = Rc::new(Cell::new(Instant::now()));
        let seq = Arc::new(AtomicU64::new(0));
        ui.on_settings_backdrop_blur(move |value| {
            let value = settings::normalise_backdrop_blur(value.max(0) as u32);
            pending.set(value);
            last_input.set(Instant::now());
            let weak = weak.clone();
            let settings_store = settings_store.clone();
            let pending = pending.clone();
            let last_input = last_input.clone();
            let seq = seq.clone();
            let tick = seq.clone();
            debounce.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(150),
                move || {
                    let value = pending.get();
                    let settle_ms = last_input.get().elapsed().as_secs_f64() * 1000.0;
                    // Bump the sequence BEFORE the slow work: a later settle
                    // invalidates this generation, so an old tile can never land
                    // on top of a newer one.
                    let generation = tick.fetch_add(1, Ordering::SeqCst) + 1;
                    backdrop::set_blur(value);
                    update_settings(&settings_store, |prefs| {
                        prefs.backdrop_blur = value;
                    });
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let dark = ui.global::<AppTheme>().get_dark();
                    let trigger = Instant::now();
                    let worker_seq = seq.clone();
                    std::thread::spawn(move || {
                        let pixels = backdrop::generate(dark, value);
                        let gen_ms = trigger.elapsed().as_secs_f64() * 1000.0;
                        let _ = slint::invoke_from_event_loop(move || {
                            let latest = worker_seq.load(Ordering::SeqCst);
                            if latest == generation {
                                // Only the newest generation is installed AND
                                // painted: an older one must not leave its pixels
                                // in the cache for a later theme switch.
                                let image = backdrop::install(dark, pixels);
                                republish_wallpaper(&image, dark);
                                crate::debug_log(&format!(
                                    "backdrop blur={value} gen_ms={gen_ms:.1} settle_ms={settle_ms:.1} dark={} win=all",
                                    u8::from(dark)
                                ));
                            } else {
                                crate::debug_log(&format!(
                                    "backdrop blur={value} gen_ms={gen_ms:.1} settle_ms={settle_ms:.1} stale=1"
                                ));
                            }
                        });
                    });
                },
            );
        });
    }

    // QA chain (`BILI_F144_TEST=1`): the wallpaper / settings / blur evidence.
    maybe_start_f144_test(weak.clone(), session_slot.clone(), settings_store.clone());
    maybe_start_f145_test(weak.clone(), session_slot.clone());
    maybe_start_f146_test(weak.clone());
    maybe_start_f147_test(weak.clone());
    maybe_start_f148_test(weak.clone());
    maybe_start_f149_test(weak.clone(), session_slot.clone(), settings_store.clone());
    maybe_start_f150_test(weak.clone());
    maybe_start_f151_test(weak.clone());
    maybe_start_f152_test(
        weak.clone(),
        session_slot.clone(),
        settings_store.clone(),
    );
    maybe_start_f154_test(weak.clone());
    maybe_start_f155_test(
        weak.clone(),
        session_slot.clone(),
        settings_store.clone(),
    );

    // QA hook (`BILI_HOME_TEST=menu|library|search|section:<i>[,more]`): drives
    // the bottom bar's states and a section pick through the REAL properties and
    // callback, so the menu / morph / ranking captures need no synthetic input.
    // The section pick runs ONCE (it loads a feed); the menu/search flags are
    // re-asserted for ~12 s because a menu with the pointer parked outside
    // closes on the bar's own 300 ms away timer, by design.
    if let Ok(spec) = std::env::var("BILI_HOME_TEST") {
        let weak = weak.clone();
        let spec = spec.trim().to_string();
        if !spec.is_empty() {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(6_000));
                let mut first = true;
                for _ in 0..80 {
                    let weak = weak.clone();
                    let spec = spec.clone();
                    let section = std::mem::take(&mut first);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else {
                            return;
                        };
                        if section {
                            crate::debug_log(&format!(
                                "home test spec={spec} logged_in={}",
                                ui.get_logged_in()
                            ));
                            for part in spec.split(',') {
                                if let Some(index) = part.trim().strip_prefix("section:") {
                                    if let Ok(index) = index.trim().parse::<i32>() {
                                        ui.invoke_home_section(index);
                                    }
                                }
                            }
                        }
                        apply_home_test(&ui, &spec);
                    });
                    std::thread::sleep(Duration::from_millis(150));
                }
            });
        }
    }

    // QA hook (`BILI_SEARCH_MORPH_TEST=<tag>[,<delay_ms>[,<at_ms>]]`): drive
    // the field open through its real property and sample HomeBar's live rect.
    //
    //   * without `at_ms`: a 10ms series for 220ms (the measurement table),
    //     then close and capture the three instants in one run;
    //   * with `at_ms`: open, wait exactly `at_ms`, capture once, close - so
    //     each required PNG (0/90/180) comes from its own clean run and is not
    //     delayed by the previous shot's BitBlt.
    //
    // Every number is the animation's actual geometry, read from HomeBar.
    if let Ok(spec) = std::env::var("BILI_SEARCH_MORPH_TEST") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "f133_search".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(9_000)
            .clamp(2_000, 120_000);
        let instant = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .map(|ms| ms.min(1_000));
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let weak_open = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak_open.upgrade() else {
                    return;
                };
                let start = Rc::new(Cell::new(Instant::now()));
                if let Some(at) = instant {
                    log_search_field(&ui, "closed", start.get());
                    ui.set_search_open(true);
                    log_search_field(&ui, "capture_open", start.get());
                    let weak_shot = ui.as_weak();
                    let path = qa_capture_path(&format!("{tag}_{at}"));
                    let start_shot = start.clone();
                    slint::Timer::single_shot(Duration::from_millis(at), move || {
                        let Some(ui) = weak_shot.upgrade() else {
                            return;
                        };
                        log_search_field(&ui, "capture", start_shot.get());
                        let _ = capture_window_png(&ui.window(), &path);
                        ui.set_search_open(false);
                    });
                    return;
                }
                log_search_field(&ui, "closed", start.get());
                ui.set_search_open(true);
                let tag_shot = tag.clone();
                let weak_tick = ui.as_weak();
                let timer = Rc::new(slint::Timer::default());
                keep_qa_timer(timer.clone());
                let timer_tick = timer.clone();
                let start_tick = start.clone();
                timer.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(10),
                    move || {
                        let Some(ui) = weak_tick.upgrade() else {
                            timer_tick.stop();
                            return;
                        };
                        let elapsed = start_tick.get().elapsed().as_millis();
                        log_search_field(&ui, "sample", start_tick.get());
                        if elapsed < 220 {
                            return;
                        }
                        timer_tick.stop();
                        ui.set_search_open(false);
                        let tag_shot = tag_shot.clone();
                        // Phase B: the three capture instants. BitBlt from the
                        // UI thread, so each shot also logs the geometry it
                        // actually shows.
                        let weak_cap = ui.as_weak();
                        slint::Timer::single_shot(Duration::from_millis(320), move || {
                            let Some(ui) = weak_cap.upgrade() else {
                                return;
                            };
                            let names = [
                                format!("{tag_shot}_0"),
                                format!("{tag_shot}_90"),
                                format!("{tag_shot}_180"),
                            ];
                            let paths: Vec<std::path::PathBuf> =
                                names.iter().map(|name| qa_capture_path(name)).collect();
                            let shot_start = Instant::now();
                            ui.set_search_open(true);
                            log_search_field(&ui, "capture_0", shot_start);
                            let _ = capture_window_png(&ui.window(), &paths[0]);
                            let weak_90 = ui.as_weak();
                            let path_90 = paths[1].clone();
                            slint::Timer::single_shot(Duration::from_millis(90), move || {
                                let Some(ui) = weak_90.upgrade() else {
                                    return;
                                };
                                log_search_field(&ui, "capture_90", shot_start);
                                let _ = capture_window_png(&ui.window(), &path_90);
                                let weak_180 = ui.as_weak();
                                let path_180 = paths[2].clone();
                                slint::Timer::single_shot(
                                    Duration::from_millis(90),
                                    move || {
                                        let Some(ui) = weak_180.upgrade() else {
                                            return;
                                        };
                                        log_search_field(&ui, "capture_180", shot_start);
                                        let _ =
                                            capture_window_png(&ui.window(), &path_180);
                                        ui.set_search_open(false);
                                    },
                                );
                            });
                        });
                    },
                );
            });
        });
    }

    // ---- 直播 (view_mode 23): index aggregate + area list + room player ----
    // One shared host for the page: the parsed aggregate (so returning to 推荐
    // costs no request), the selected chip, the flat card model the two views
    // index into, and the per-load sequence that drops a stale landing.
    let live_host = {
        ui.set_live_modules(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<LiveModuleRow>::new(),
        ))));
        ui.set_live_cards(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<LiveCard>::new(),
        ))));
        Arc::new(LiveHost {
            state: Arc::new(Mutex::new(LiveState::default())),
            seq: Arc::new(AtomicU64::new(0)),
            ids: Arc::new(Mutex::new(Vec::<String>::new())),
            covers: Arc::new(Mutex::new(Vec::<String>::new())),
        })
    };
    if let Ok(mut guard) = LIVE_QA_HOST.lock() {
        *guard = Some(live_host.clone());
    }
    {
        // 直播 pill: open the page and publish what is already loaded, or start
        // the first aggregate fetch.
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let host = live_host.clone();
        ui.on_live_open(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let cached = host
                .state
                .lock()
                .ok()
                .filter(|state| !state.modules.is_empty())
                .map(|state| (state.modules.clone(), state.areas.clone()));
            ui.set_live_area_sel(0);
            ui.set_view_mode(23);
            match cached {
                Some((modules, areas)) => {
                    live_publish_index_cached(&ui, &host, &modules, &areas);
                    crate::debug_log(&format!(
                        "live open page source=cache modules={} cards={}",
                        modules.len(),
                        modules.iter().map(|m| m.rooms.len()).sum::<usize>()
                    ));
                }
                None => {
                    crate::debug_log("live open page source=fetch");
                    live_load_index(weak.clone(), session_slot.clone(), host.clone(), false);
                }
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let host = live_host.clone();
        ui.on_live_area_pick(move |index| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let index = index.max(0) as usize;
            ui.set_live_area_sel(index as i32);
            if index == 0 {
                let cached = host
                    .state
                    .lock()
                    .ok()
                    .map(|state| (state.modules.clone(), state.areas.clone()));
                if let Some((modules, areas)) = cached {
                    live_publish_index_cached(&ui, &host, &modules, &areas);
                }
                return;
            }
            live_load_area(weak.clone(), session_slot.clone(), host.clone(), index as i64, 1, false);
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let host = live_host.clone();
        ui.on_live_more(move || {
            let (sel, page, has_more, loading) = match host.state.lock() {
                Ok(state) => (state.area_sel, state.page, state.has_more, state.loading),
                Err(_) => return,
            };
            if sel == 0 || !has_more || loading {
                return;
            }
            live_load_area(
                weak.clone(),
                session_slot.clone(),
                host.clone(),
                sel as i64,
                page + 1,
                true,
            );
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let host = live_host.clone();
        ui.on_live_retry(move || {
            let sel = host.state.lock().map(|state| state.area_sel).unwrap_or(0);
            if sel == 0 {
                live_load_index(weak.clone(), session_slot.clone(), host.clone(), true);
            } else {
                live_load_area(
                    weak.clone(),
                    session_slot.clone(),
                    host.clone(),
                    sel as i64,
                    1,
                    false,
                );
            }
        });
    }
    {
        let weak = weak.clone();
        let session_slot = session_slot.clone();
        let player_state = player_state.clone();
        let settings_store = settings_store.clone();
        let host = live_host.clone();
        ui.on_live_room_open(move |index| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let room = host
                .state
                .lock()
                .ok()
                .and_then(|state| state.rooms.get(index.max(0) as usize).cloned());
            let Some(room) = room else {
                crate::debug_log(&format!("live room open ignored row={index} reason=no-room"));
                return;
            };
            let Some(session) = active_session(&session_slot) else {
                return;
            };
            crate::debug_log(&format!(
                "live room open row={index} room={} title={} uname={}",
                room.room_id, room.title, room.uname
            ));
            let _ = &ui;
            open_live_player(session, room, &player_state, &settings_store);
        });
    }

    // `BILI_LIVE_TEST`: the input-free driver for the live page and room.
    start_live_test_hook(weak.clone());
    // 防干扰 QA chain (`BILI_F158_TEST`): the settings category, the two
    // toolbar states, the search panel with 热搜 hidden, and the persistence
    // read-back. Inert unless the env var is set.
    start_f158_test_hook(weak.clone());
    // f159 QA chain (`BILI_F159_TEST`): the 5+5 bar, the breathing skeleton, the
    // white chips and the card glass/flat A/B. Inert unless the env var is set.
    start_f159_test_hook(weak.clone());

    // ---- 养鱼 (fish dashboard, view_mode 20) ----
    // The engine is opened ONCE here; a failure (unwritable fish.db) leaves the
    // page in its error state instead of panicking. Updates are PUSHED by the
    // engine's snapshot hook and gated inside `apply_fish`.
    FISH_SESSION.with(|slot| *slot.borrow_mut() = active_session(&session_slot));
    FISH_PLAYER.with(|slot| *slot.borrow_mut() = Some(player_state.clone()));
    FISH_SETTINGS.with(|slot| *slot.borrow_mut() = Some(settings_store.clone()));
    // The playback gate the ASR step polls. A 500 ms UI-thread timer is the
    // only place that can read `PlayerState` (Rc/RefCell), and it publishes a
    // single atomic, so the worker never touches UI state.
    {
        let player = player_state.clone();
        let timer = Rc::new(slint::Timer::default());
        FISH_PLAYBACK_TIMER.with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                *slot = Some(timer.clone());
            }
        });
        let held = timer.clone();
        let last = Rc::new(Cell::new(false));
        let _ = &held;
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                let busy = any_player_busy(&player);
                if busy != last.get() {
                    last.set(busy);
                    crate::debug_log(&format!("fish playback busy={}", busy as i32));
                }
                fish::note_playback_busy(busy);
            },
        );
    }
    {
        let engine = match fish::FishEngine::open() {
            Ok(engine) => Some(engine),
            Err(err) => {
                crate::debug_log(&format!("fish open failed: {err:#}"));
                None
            }
        };
        let key_state = settings_store
            .lock()
            .map(|prefs| {
                (
                    !prefs.deepseek_key.trim().is_empty(),
                    settings::mask_key(&prefs.deepseek_key),
                )
            })
            .unwrap_or((false, String::new()));
        let fish = Rc::new(FishUi {
            engine: engine.clone(),
            last_verdict: RefCell::new(None),
            last_stats: RefCell::new(None),
            day: Cell::new(local_clock().1),
            open_error: RefCell::new(engine.is_none()),
            key_state: RefCell::new(key_state),
            tried_covers: RefCell::new(HashSet::new()),
            feed_sort: Cell::new(0),
            feed_filter: Cell::new(0),
            adopted: RefCell::new(HashSet::new()),
            feed_store: feed_store.clone(),
            settings_store: settings_store.clone(),
        });

        // ---- 鱼缸 configuration: publish the stored values once ----
        let initial = settings_store
            .lock()
            .map(|prefs| prefs.clone())
            .unwrap_or_default();
        let bowls = (initial.fish_budget_videos / 20).clamp(1, 10) as i32;
        let sources = settings::normalise_fish_sources(initial.fish_sources);
        let feed_time = settings::normalise_fish_time(&initial.fish_feed_time);
        let auto = Rc::new(RefCell::new(FishAuto {
            enabled: initial.fish_enabled,
            auto: initial.fish_auto_feed,
            time_minutes: settings::fish_time_minutes(&feed_time).unwrap_or(20 * 60),
            sources,
            last_day: None,
        }));
        ui.set_fish_enabled(initial.fish_enabled);
        ui.set_fish_bowls(bowls);
        ui.set_fish_src_home(sources & settings::FISH_SOURCE_HOME != 0);
        ui.set_fish_src_follow(sources & settings::FISH_SOURCE_FOLLOW != 0);
        ui.set_fish_src_fav(sources & settings::FISH_SOURCE_FAV != 0);
        ui.set_fish_autofeed(initial.fish_auto_feed);
        ui.set_fish_feed_time(feed_time.as_str().into());
        ui.set_fish_feed_time_draft(feed_time.as_str().into());
        ui.set_fish_model(initial.fish_model.as_str().into());
        ui.set_fish_model_draft(initial.fish_model.as_str().into());
        ui.set_fish_tier(settings::normalise_fish_tier(initial.fish_tier) as i32);
        ui.set_fish_asr_enabled(initial.fish_asr_enabled);
        ui.set_fish_asr_engine(initial.fish_asr_engine.as_str().into());
        ui.set_fish_asr_device(initial.fish_asr_device.as_str().into());
        ui.set_fish_asr_model(initial.fish_asr_model.as_str().into());
        ui.set_fish_asr_threads(initial.fish_asr_threads as i32);
        ui.set_fish_vision_detail(initial.fish_vision_detail.as_str().into());
        ui.set_fish_asr_ready(false);
        ui.set_fish_model_ready(false);
        ui.set_fish_model_progress("模型未下载".into());
        ui.set_fish_asr_status("正在检测本机 ASR 引擎…".into());
        publish_watch_steps(&ui, &fish);
        // Probe the real engine on a worker thread: importing ctranslate2 and
        // querying CUDA costs ~1 s and must not delay the first frame.
        {
            let weak = weak.clone();
            let prefs = initial.clone();
            if !prefs.fish_asr_enabled {
                // ASR is off: do not spend a python startup (or a CUDA probe)
                // on a feature the user has not switched on. The panel reads
                // "未启用" and the first toggle starts the real probe.
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_fish_asr_ready(false);
                    ui.set_fish_asr_status("语音转写未启用".into());
                });
            } else {
            std::thread::spawn(move || {
                let config = fish::watch_config(&prefs);
                let available = fishwatch::asr::detect(&config);
                let model_ready = fishwatch::asr::model_ready(&config);
                let status = fishwatch::asr::status_text(&config);
                let spec = fishwatch::asr::spec_for(&config);
                let progress = match spec {
                    Some(spec) if !model_ready => format!(
                        "{} 模型未下载（{}）",
                        spec.id,
                        fishwatch::asr::models::human_bytes(spec.total_bytes())
                    ),
                    Some(spec) => format!("{} 模型已校验", spec.id),
                    None => "没有可用的模型档位".to_string(),
                };
                crate::debug_log(&format!(
                    "fish asr probe engine={:?} device={} model_ready={} status={status}",
                    available.as_ref().map(|a| a.engine),
                    available
                        .as_ref()
                        .map(|a| a.device.as_str())
                        .unwrap_or("-"),
                    model_ready as i32
                ));
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_fish_asr_ready(available.is_some());
                    ui.set_fish_model_ready(model_ready);
                    ui.set_fish_model_progress(progress.as_str().into());
                    ui.set_fish_model_frac(if model_ready { 1.0 } else { 0.0 });
                    ui.set_fish_asr_status(status.as_str().into());
                    if let Some(fish) = FISH_UI.with(|slot| slot.borrow().clone()) {
                        publish_watch_steps(&ui, &fish);
                    }
                });
            });
            }
        }

        // The hook runs on the engine's worker thread, so it may only capture
        // Send data: the `Weak` plus the snapshot. `FishUi` is thread-local and
        // is looked up again on the UI thread.
        FISH_UI.with(|slot| *slot.borrow_mut() = Some(fish.clone()));
        if let Some(engine) = engine.as_ref() {
            let weak = weak.clone();
            engine.set_snapshot_hook(Some(Arc::new(move |snap| {
                let weak = weak.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if let Some(fish) = FISH_UI.with(|slot| slot.borrow().clone()) {
                        apply_fish(&ui, &fish, snap, false);
                    }
                });
            })));
        }

        // 养鱼 pill: open the page and publish what the engine has now. The
        // session is refreshed here too: `FISH_SESSION` was captured before
        // auto-login finished, and a stale `None` would leave every feed cover
        // unfetched for the rest of the run.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let session_slot = session_slot.clone();
            ui.on_fish_open(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                if let Some(session) = active_session(&session_slot) {
                    FISH_SESSION.with(|slot| *slot.borrow_mut() = Some(session));
                }
                ui.set_view_mode(20);
                let snap = fish
                    .engine
                    .as_ref()
                    .map(|engine| engine.snapshot())
                    .unwrap_or_default();
                apply_fish(&ui, &fish, snap, true);
                // `logged_in` may have flipped since startup: the step rows'
                // availability depends on it, so republish them on every open.
                publish_watch_steps(&ui, &fish);
                crate::debug_log("fish open page");
            });
        }
        // 开始: enqueue the home feed the app already holds, then run. The
        // 大肥鱼 master switch gates every run: off means no thread and no
        // token spend, while the verdict history stays readable.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let session_slot = session_slot.clone();
            let feed_store = feed_store.clone();
            let auto = auto.clone();
            ui.on_fish_start(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                if !auto.borrow().enabled {
                    crate::debug_log("fish start blocked enabled=0");
                    return;
                }
                let Some(engine) = fish.engine.as_ref() else {
                    return;
                };
                let sources = auto.borrow().sources;
                let items = if sources & settings::FISH_SOURCE_HOME != 0 {
                    feed_store
                        .lock()
                        .map(|guard| guard.clone())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                engine.enqueue(&items);
                match active_session(&session_slot) {
                    Some(session) => {
                        FISH_SESSION.with(|slot| *slot.borrow_mut() = Some(session.clone()));
                        engine.start(session);
                        crate::debug_log(&format!(
                            "fish start feed={} queued={} sources={sources}",
                            items.len(),
                            engine.snapshot().queued
                        ));
                    }
                    None => {
                        crate::debug_log("fish start no-session");
                        ui.set_fish_state(4);
                        ui.set_fish_error_text("还没登录，鱼看不见池子".into());
                    }
                }
                apply_fish(&ui, &fish, engine.snapshot(), true);
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_pause(move || {
                if let Some(engine) = fish.engine.as_ref() {
                    engine.pause();
                    crate::debug_log("fish pause");
                    if let Some(ui) = weak.upgrade() {
                        apply_fish(&ui, &fish, engine.snapshot(), true);
                    }
                }
            });
        }
        // DeepSeek key: persist, then the engine re-reads its config.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let settings_store = settings_store.clone();
            ui.on_fish_save_key(move |key| {
                let key = key.trim().to_string();
                if key.is_empty() {
                    return;
                }
                let snapshot = update_settings(&settings_store, |prefs| {
                    prefs.deepseek_key = key.clone();
                });
                *fish.key_state.borrow_mut() = (
                    !snapshot.deepseek_key.trim().is_empty(),
                    settings::mask_key(&snapshot.deepseek_key),
                );
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!(
                    "fish key saved len={} masked={}",
                    key.len(),
                    settings::mask_key(&snapshot.deepseek_key)
                ));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_key_draft(SharedString::new());
                    let snap = fish
                        .engine
                        .as_ref()
                        .map(|engine| engine.snapshot())
                        .unwrap_or_default();
                    apply_fish(&ui, &fish, snap, true);
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_save_instruction(move |text| {
                if let Some(ui) = weak.upgrade() {
                    fish_save_instruction_text(&ui, &fish, text.as_str());
                }
            });
        }
        // 食谱: one click writes `preset.body` through the same instruction path
        // as typing it, so the next run's prompt uses exactly that text. The id
        // line is what the QA/owner log reads.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_apply_preset(move |id| {
                let Some(preset) = fish::presets()
                    .iter()
                    .find(|preset| preset.id == id.as_str())
                else {
                    crate::debug_log(&format!("fish preset unknown id={id}"));
                    return;
                };
                crate::debug_log(&format!("fish preset id={}", preset.id));
                if let Some(ui) = weak.upgrade() {
                    fish_save_instruction_text(&ui, &fish, preset.body);
                }
            });
        }
        // 推荐流 toolbar: Rust owns the model, so a sort/filter pick rebuilds it.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_feed_query(move |sort, filter| {
                fish.feed_sort.set(sort.clamp(0, 2));
                fish.feed_filter.set(filter.clamp(0, 3));
                if let Some(ui) = weak.upgrade() {
                    if let Some(engine) = fish.engine.as_ref() {
                        publish_fish_feed(&ui, &fish, engine);
                    }
                }
            });
        }
        // Tab switches are logged (and the feed keeps its offset because both
        // tabs stay instantiated).
        ui.on_fish_tab_changed(move |tab| {
            crate::debug_log(&format!("fish tab={tab}"));
        });
        // 开启大肥鱼: the master gate. Persisted, and turning it off pauses a
        // running worker so no further token is spent.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let settings_store = settings_store.clone();
            let auto = auto.clone();
            ui.on_fish_set_enabled(move |on| {
                update_settings(&settings_store, |prefs| prefs.fish_enabled = on);
                auto.borrow_mut().enabled = on;
                if !on {
                    if let Some(engine) = fish.engine.as_ref() {
                        engine.pause();
                    }
                }
                crate::debug_log(&format!("fish enabled={}", on as i32));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_enabled(on);
                    let snap = fish
                        .engine
                        .as_ref()
                        .map(|engine| engine.snapshot())
                        .unwrap_or_default();
                    apply_fish(&ui, &fish, snap, true);
                }
            });
        }
        // 每日预算: 1…10 碗, one 碗 = 20 videos. Stored in the existing
        // `fish_budget_videos` field, so the engine's next run reads it.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let settings_store = settings_store.clone();
            ui.on_fish_set_budget(move |bowls| {
                let bowls = bowls.clamp(1, 10);
                update_settings(&settings_store, |prefs| {
                    prefs.fish_budget_videos = bowls as u32 * 20;
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!(
                    "fish budget bowls={bowls} videos={}",
                    bowls * 20
                ));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_bowls(bowls);
                }
            });
        }
        // 喂食来源: persisted as a bitmask. Only 首页 currently has a queue
        // source; the other two are carried so the engine's feed loader can
        // honour them without another settings field.
        {
            let weak = weak.clone();
            let settings_store = settings_store.clone();
            let auto = auto.clone();
            ui.on_fish_set_sources(move |home, follow, fav| {
                let mut mask = 0;
                if home {
                    mask |= settings::FISH_SOURCE_HOME;
                }
                if follow {
                    mask |= settings::FISH_SOURCE_FOLLOW;
                }
                if fav {
                    mask |= settings::FISH_SOURCE_FAV;
                }
                let mask = settings::normalise_fish_sources(mask);
                update_settings(&settings_store, |prefs| prefs.fish_sources = mask);
                auto.borrow_mut().sources = mask;
                crate::debug_log(&format!(
                    "fish sources home={} follow={} fav={}",
                    mask & settings::FISH_SOURCE_HOME != 0,
                    mask & settings::FISH_SOURCE_FOLLOW != 0,
                    mask & settings::FISH_SOURCE_FAV != 0
                ));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_src_home(mask & settings::FISH_SOURCE_HOME != 0);
                    ui.set_fish_src_follow(mask & settings::FISH_SOURCE_FOLLOW != 0);
                    ui.set_fish_src_fav(mask & settings::FISH_SOURCE_FAV != 0);
                }
            });
        }
        // 自动喂食: on/off + HH:MM. Enabling it sets `last_day` to today, so
        // switching it on at 21:00 does not immediately fire the 20:00 slot.
        {
            let weak = weak.clone();
            let settings_store = settings_store.clone();
            let auto = auto.clone();
            ui.on_fish_set_autofeed(move |on, time| {
                let time = settings::normalise_fish_time(time.as_str());
                update_settings(&settings_store, |prefs| {
                    prefs.fish_auto_feed = on;
                    prefs.fish_feed_time = time.clone();
                });
                {
                    let mut state = auto.borrow_mut();
                    state.auto = on;
                    state.time_minutes =
                        settings::fish_time_minutes(&time).unwrap_or(20 * 60);
                    if on {
                        state.last_day = Some(local_day_of_year());
                    }
                }
                crate::debug_log(&format!("fish autofeed={} time={time}", on as i32));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_autofeed(on);
                    ui.set_fish_feed_time(time.as_str().into());
                    ui.set_fish_feed_time_draft(time.as_str().into());
                }
            });
        }
        // The time field's ±30 min stepper. Non-UI state: the field value and
        // the stored "HH:MM" move together, so the next tick sees the new slot.
        {
            let weak = weak.clone();
            let settings_store = settings_store.clone();
            let auto = auto.clone();
            ui.on_fish_shift_time(move |delta| {
                let current = auto.borrow().time_minutes;
                let next = (current as i32 + delta).rem_euclid(24 * 60) as u32;
                let time = format!("{:02}:{:02}", next / 60, next % 60);
                update_settings(&settings_store, |prefs| {
                    prefs.fish_feed_time = time.clone();
                });
                auto.borrow_mut().time_minutes = next;
                crate::debug_log(&format!("fish feed time={time}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_feed_time(time.as_str().into());
                    ui.set_fish_feed_time_draft(time.as_str().into());
                }
            });
        }
        // 模型: the engine re-reads it on the next run.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let settings_store = settings_store.clone();
            ui.on_fish_save_model(move |model| {
                let model = settings::normalise_fish_model(model.as_str());
                update_settings(&settings_store, |prefs| prefs.fish_model = model.clone());
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish model={model}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_model(model.as_str().into());
                    ui.set_fish_model_draft(model.as_str().into());
                }
            });
        }
        // A verdict row: resolve the bvid and open the real player. A feed card
        // that is opened is an ADOPTED verdict: it grows the fish (once per
        // bvid per session), before the network resolve starts.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_open_video(move |bvid| {
                let bvid = bvid.to_string();
                if fish.adopted.borrow_mut().insert(bvid.clone()) {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_fish_adopted(ui.get_fish_adopted().saturating_add(1));
                    }
                }
                crate::debug_log(&format!("fish open row bvid={bvid}"));
                open_fish_video(weak.clone(), bvid);
            });
        }
        // 清空推荐流: the queue and today's cards leave, every verdict stays in
        // 历史. The UI arms the button first, so this is the confirmed click.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let auto = auto.clone();
            ui.on_fish_clear(move || {
                let Some(engine) = fish.engine.as_ref() else {
                    return;
                };
                match engine.clear_queue() {
                    Ok(report) => {
                        // 之后不再自动喂: today's auto-feed slot counts as used,
                        // so clearing at 17:00 is not undone by the 20:00 run.
                        if auto.borrow().auto {
                            auto.borrow_mut().last_day = Some(local_day_of_year());
                            crate::debug_log("fish clear autofeed today=used");
                        }
                        crate::debug_log(&format!(
                            "fish clear ok queue={} verdicts_kept={}",
                            engine.snapshot().queued,
                            report.verdicts_kept
                        ));
                        if let Some(ui) = weak.upgrade() {
                            apply_fish(&ui, &fish, engine.snapshot(), true);
                        }
                    }
                    Err(err) => crate::debug_log(&format!("fish clear failed: {err:#}")),
                }
            });
        }
        // 刷新: re-read verdicts + stats (no network).
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_refresh(move || {
                if let Some(ui) = weak.upgrade() {
                    let snap = fish
                        .engine
                        .as_ref()
                        .map(|engine| engine.snapshot())
                        .unwrap_or_default();
                    apply_fish(&ui, &fish, snap, true);
                }
            });
        }

        // ---- 看片力度: tier + per-step switches ----
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_tier(move |tier| {
                let tier = settings::normalise_fish_tier(tier.max(0) as u32);
                let preset = if tier < settings::FISH_TIER_CUSTOM {
                    Some(fishwatch::StepSelection::for_tier(tier))
                } else {
                    None
                };
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_tier = tier;
                    if let Some(preset) = &preset {
                        prefs.fish_step_meta = preset.meta;
                        prefs.fish_step_subtitle = preset.subtitle;
                        prefs.fish_step_outline = preset.outline;
                        prefs.fish_step_comments = preset.comments;
                        prefs.fish_step_asr = preset.asr;
                        prefs.fish_step_frames = preset.frames;
                    }
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish tier={tier}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_tier(tier as i32);
                    publish_watch_steps(&ui, &fish);
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_toggle_step(move |id, on| {
                let Some(step) = fishwatch::steps::StepId::from_str(id.as_str()) else {
                    crate::debug_log(&format!("fish toggle unknown step={id}"));
                    return;
                };
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_tier = settings::FISH_TIER_CUSTOM;
                    match step {
                        fishwatch::steps::StepId::Meta => prefs.fish_step_meta = on,
                        fishwatch::steps::StepId::Subtitle => prefs.fish_step_subtitle = on,
                        fishwatch::steps::StepId::Outline => prefs.fish_step_outline = on,
                        fishwatch::steps::StepId::Comments => prefs.fish_step_comments = on,
                        fishwatch::steps::StepId::Asr => prefs.fish_step_asr = on,
                        fishwatch::steps::StepId::Frames => prefs.fish_step_frames = on,
                    }
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish step toggle id={} on={}", step.as_str(), on as i32));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_tier(settings::FISH_TIER_CUSTOM as i32);
                    publish_watch_steps(&ui, &fish);
                }
            });
        }

        // ---- 耳与眼: ASR + 视觉 settings (persisted; the engine reads them) ----
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_asr_enabled(move |on| {
                update_settings(&fish.settings_store, |prefs| prefs.fish_asr_enabled = on);
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish asr enabled={}", on as i32));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_asr_enabled(on);
                    publish_watch_steps(&ui, &fish);
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_asr_engine(move |engine_name| {
                let engine_name = settings::normalise_fish_asr_engine(engine_name.as_str());
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_asr_engine = engine_name.clone()
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish asr engine={engine_name}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_asr_engine(engine_name.as_str().into());
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_asr_device(move |device| {
                let device = settings::normalise_fish_asr_device(device.as_str());
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_asr_device = device.clone()
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish asr device={device}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_asr_device(device.as_str().into());
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_asr_model(move |model| {
                let model = settings::normalise_fish_asr_model(model.as_str());
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_asr_model = model.clone()
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish asr model={model}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_asr_model(model.as_str().into());
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_asr_threads(move |threads| {
                let threads = settings::normalise_fish_asr_threads(threads.max(1) as u32);
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_asr_threads = threads
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish asr threads={threads}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_asr_threads(threads as i32);
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_set_vision_detail(move |detail| {
                let detail = settings::normalise_fish_vision_detail(detail.as_str());
                update_settings(&fish.settings_store, |prefs| {
                    prefs.fish_vision_detail = detail.clone()
                });
                if let Some(engine) = fish.engine.as_ref() {
                    engine.refresh_from_settings();
                }
                crate::debug_log(&format!("fish vision detail={detail}"));
                if let Some(ui) = weak.upgrade() {
                    ui.set_fish_vision_detail(detail.as_str().into());
                }
            });
        }
        {
            let weak = weak.clone();
            let fish = fish.clone();
            ui.on_fish_download_model(move || {
                static DOWNLOAD_BUSY: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if DOWNLOAD_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    crate::debug_log("fish model download ignored (already running)");
                    return;
                }
                let prefs = fish
                    .settings_store
                    .lock()
                    .map(|prefs| prefs.clone())
                    .unwrap_or_default();
                let config = fish::watch_config(&prefs);
                let Some(spec) = fishwatch::asr::spec_for(&config) else {
                    DOWNLOAD_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
                    if let Some(ui) = weak.upgrade() {
                        ui.set_fish_model_progress("没有匹配的模型档位".into());
                    }
                    return;
                };
                crate::debug_log(&format!(
                    "fish model download start id={} engine={} dir={} bytes={}",
                    spec.id,
                    spec.engine,
                    config.model_dir.display(),
                    spec.total_bytes()
                ));
                let root = config.model_dir.clone();
                let weak_thread = weak.clone();
                let _ = weak.upgrade_in_event_loop(|ui| {
                    ui.set_fish_model_progress("准备下载…".into());
                    ui.set_fish_model_frac(0.0);
                });
                std::thread::spawn(move || {
                    let mut last = Instant::now()
                        .checked_sub(Duration::from_secs(2))
                        .unwrap_or_else(Instant::now);
                    let result = fishwatch::asr::models::download(&root, spec, &mut |progress| {
                        if last.elapsed() < Duration::from_millis(250)
                            && progress.fraction < 1.0
                        {
                            return;
                        }
                        last = Instant::now();
                        let weak = weak_thread.clone();
                        let text = progress.text.clone();
                        let fraction = progress.fraction;
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            ui.set_fish_model_progress(text.as_str().into());
                            ui.set_fish_model_frac(fraction);
                        });
                    });
                    DOWNLOAD_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
                    match result {
                        Ok(()) => {
                            crate::debug_log(&format!(
                                "fish model download done id={} verify=ok sha256={}",
                                spec.id, spec.files[0].sha256
                            ));
                            let weak = weak_thread.clone();
                            let _ = weak.upgrade_in_event_loop(move |ui| {
                                ui.set_fish_model_ready(true);
                                ui.set_fish_model_frac(1.0);
                                ui.set_fish_model_progress(
                                    format!(
                                        "{} 模型已下载并校验（{}）",
                                        spec.id,
                                        fishwatch::asr::models::human_bytes(spec.total_bytes())
                                    )
                                    .as_str()
                                    .into(),
                                );
                                if let Some(fish) = FISH_UI.with(|slot| slot.borrow().clone()) {
                                    let prefs = fish
                                        .settings_store
                                        .lock()
                                        .map(|prefs| prefs.clone())
                                        .unwrap_or_default();
                                    let config = fish::watch_config(&prefs);
                                    ui.set_fish_asr_ready(
                                        fishwatch::asr::detect(&config).is_some(),
                                    );
                                    ui.set_fish_asr_status(
                                        fishwatch::asr::status_text(&config).as_str().into(),
                                    );
                                    publish_watch_steps(&ui, &fish);
                                }
                            });
                        }
                        Err(err) => {
                            crate::debug_log(&format!("fish model download failed: {err:#}"));
                            let weak = weak_thread.clone();
                            let text = format!("下载失败：{err:#}（已下载部分会续传，可重试）");
                            let _ = weak.upgrade_in_event_loop(move |ui| {
                                ui.set_fish_model_ready(false);
                                ui.set_fish_model_progress(text.as_str().into());
                            });
                        }
                    }
                });
            });
        }

        // First publish so the page is correct before it is ever opened.
        if let Some(engine) = engine.as_ref() {
            let weak = weak.clone();
            let snap = engine.snapshot();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if let Some(fish) = FISH_UI.with(|slot| slot.borrow().clone()) {
                    apply_fish(&ui, &fish, snap, true);
                }
            });
        }

        // ---- 自动喂食: one 20s tick, one run per local day ----
        // The tick only reads in-memory configuration; `last_day` is set when
        // auto-feed is switched on, so enabling it in the evening never fires
        // the same day's passed slot. Off (`fish_enabled`) exits immediately:
        // no auto-analysis, no token spend.
        {
            let weak = weak.clone();
            let fish = fish.clone();
            let auto = auto.clone();
            let feed_store = feed_store.clone();
            let session_slot = session_slot.clone();
            let timer = Rc::new(slint::Timer::default());
            FISH_AUTO_TIMER.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    *slot = Some(timer.clone());
                }
            });
            let held = timer.clone();
            timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(20),
                move || {
                    let Some(ui) = weak.upgrade() else {
                        held.stop();
                        return;
                    };
                    // Local midnight rollover: a new day means 今日, the card
                    // times and the 7/30-day windows are all different. This 20s
                    // tick is the only clock the page already runs, so it is
                    // what flips the page - no extra timer, no restart.
                    let (_, today_key) = local_clock();
                    if fish.day.get() != today_key {
                        fish.day.set(today_key);
                        if let Some(engine) = fish.engine.as_ref() {
                            crate::debug_log(&format!("fish day rollover day={today_key}"));
                            apply_fish(&ui, &fish, engine.snapshot(), true);
                        }
                    }
                    let Some(engine) = fish.engine.as_ref() else {
                        return;
                    };
                    let (minutes, day) = local_clock();
                    let fire = {
                        let state = auto.borrow();
                        state.enabled
                            && state.auto
                            && state.last_day != Some(day)
                            && minutes >= state.time_minutes
                    };
                    if !fire {
                        return;
                    }
                    if matches!(engine.snapshot().state, fish::FishState::Running) {
                        return;
                    }
                    let Some(session) = active_session(&session_slot) else {
                        return;
                    };
                    let sources = auto.borrow().sources;
                    let items = if sources & settings::FISH_SOURCE_HOME != 0 {
                        feed_store
                            .lock()
                            .map(|guard| guard.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    engine.enqueue(&items);
                    FISH_SESSION.with(|slot| *slot.borrow_mut() = Some(session.clone()));
                    engine.start(session);
                    auto.borrow_mut().last_day = Some(day);
                    crate::debug_log(&format!(
                        "fish auto feed time={:02}:{:02} queued={} sources={sources}",
                        minutes / 60,
                        minutes % 60,
                        engine.snapshot().queued
                    ));
                    apply_fish(&ui, &fish, engine.snapshot(), true);
                },
            );
        }

        // QA hook (`BILI_F136_TEST=<start_delay_ms>`): open 养鱼, capture both
        // tabs, prove the feed keeps its scroll offset across a tab switch,
        // apply a recipe and prove an edited instruction clears the highlight,
        // then capture the empty feed. The geometry comes from the real layout
        // (the page's `fish_qa` publisher). All state changes go through the
        // page's own callbacks; the settings touched are restored at the end.
        if let Ok(spec) = std::env::var("BILI_F136_TEST") {
            let delay = spec
                .trim()
                .parse::<u64>()
                .unwrap_or(12_000)
                .clamp(1_000, 120_000);
            start_f136_test(weak.clone(), session_slot.clone(), delay);
        }

        // QA hook (`BILI_F160_TEST=<delay_ms>`): capture the 看片力度 / 耳与眼 /
        // evidence panels and one verdict card. `BILI_F160_KIND=asr|vision`
        // names the verdict screenshot.
        if let Ok(spec) = std::env::var("BILI_F160_TEST") {
            let delay = spec
                .trim()
                .parse::<u64>()
                .unwrap_or(12_000)
                .clamp(1_000, 120_000);
            start_f160_test(weak.clone(), delay);
        }

        // QA hook (`BILI_F160_PAUSE=1`): real player + real ASR job, to prove
        // the playback gate pauses and resumes the transcription.
        if std::env::var("BILI_F160_PAUSE").as_deref() == Ok("1") {
            start_f160_pause_test(
                weak.clone(),
                session_slot.clone(),
                settings_store.clone(),
                10_000,
            );
        }

        // QA hook (`BILI_F143_TEST=fish`): the rebuilt verdict-first card,
        // captured with real covers and once with a no-cover fixture.
        maybe_start_f143_fish(weak.clone());

        // QA hook (`BILI_F167_TEST=<delay_ms>`): the 鱼缸 timeline + 清空 pass.
        // Seeds a scratch database (`BILI_FISH_DB`) with three local days and
        // captures today / history / dark / cleared / empty, with probes.
        if let Ok(spec) = std::env::var("BILI_F167_TEST") {
            let delay = spec
                .trim()
                .parse::<u64>()
                .unwrap_or(9_000)
                .clamp(1_000, 120_000);
            start_f167_test(weak.clone(), engine.clone(), delay);
        }
    }

    // QA hook (`BILI_LOGIN_SHOT=<tag>[,<delay_ms>]`): force the login card into
    // the progress state (avatar + name/UID + bar + hint) on top of a live
    // session, then capture the MAIN window with `PrintWindow`. Same idea as
    // BILI_LOGIN_VIEW, but it also names the PNG, so the single-surface login
    // layout is proven from a real render. No synthetic input.
    if let Ok(spec) = std::env::var("BILI_LOGIN_SHOT") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "login".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(12_000)
            .clamp(1_000, 120_000);
        // Third field: which state to show (0 chooser / 1 progress / 2 scan).
        // Fourth: `anon` clears the saved-account presentation for a shot of
        // the no-account variant.
        let state = parts
            .next()
            .and_then(|part| part.trim().parse::<i32>().ok())
            .unwrap_or(1)
            .clamp(0, 2);
        let anon = parts
            .next()
            .map(|part| part.trim() == "anon")
            .unwrap_or(false);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let tag_path = qa_capture_path(&tag);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_logged_in(false);
                ui.set_login_failed(false);
                ui.set_login_error("".into());
                if anon {
                    ui.set_saved_account_name("".into());
                    ui.set_saved_account_uid("".into());
                    ui.set_saved_account_avatar(slint::Image::default());
                }
                ui.set_login_state(0);
                if state >= 1 {
                    ui.set_login_state(1);
                    ui.set_login_percent(62);
                    ui.set_login_progress(0.62);
                    ui.set_login_step("准备中…".into());
                }
                if state == 2 {
                    ui.set_login_state(2);
                    ui.set_login_percent(25);
                    ui.set_login_progress(0.25);
                    ui.set_login_step("等待扫码…".into());
                    ui.set_login_countdown("二维码有效期约 180 秒".into());
                }
                let weak_cap = ui.as_weak();
                slint::Timer::single_shot(Duration::from_millis(1_000), move || {
                    let Some(ui) = weak_cap.upgrade() else {
                        return;
                    };
                    crate::debug_log(&format!(
                        "login-shot state={} progress={:.2} saved_account={:?} uid={:?} avatar_loaded={}",
                        ui.get_login_state(),
                        ui.get_login_progress(),
                        ui.get_saved_account_name(),
                        ui.get_saved_account_uid(),
                        ui.get_saved_account_avatar().size().width > 0
                    ));
                    let _ = capture_window_png(&ui.window(), &tag_path);
                });
            });
        });
    }
    // QA hook (`BILI_PLAYLIST_SHOT=<tag>[,<delay_ms>]`): queue three feed rows
    // queue-only (which must NOT open the panel), then open 播放列表 through the
    // vinyl's own click callback, capture, and click again to close. The
    // `playlist open source=click` lines in BILI_DEBUG_LOG are the proof that
    // only the click path moves the panel.
    if let Ok(spec) = std::env::var("BILI_PLAYLIST_SHOT") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "playlist_click".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(20_000)
            .clamp(2_000, 180_000);
        let weak = weak.clone();
        let path = qa_capture_path(&tag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let weak_start = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak_start.upgrade() else {
                    return;
                };
                let weak = ui.as_weak();
                // UI-thread objects: created here, never sent across threads.
                let step = Rc::new(Cell::new(0u32));
                let waited = Rc::new(Cell::new(0u32));
                let timer = Rc::new(slint::Timer::default());
    let timer_a = timer.clone();
                keep_qa_timer(timer.clone());
                            timer.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(700),
                    move || {
                        let Some(ui) = weak.upgrade() else {
                            timer_a.stop();
                            return;
                        };
                        match step.get() {
                            0 => {
                                if ui.get_feed_items().row_count() == 0 {
                                    waited.set(waited.get() + 1);
                                    if waited.get() > 60 {
                                        crate::debug_log("playlist-shot feed empty, abort");
                                        timer_a.stop();
                                    }
                                    return;
                                }
                                crate::debug_log(&format!(
                                    "playlist-shot enqueue feed_rows={}",
                                    ui.get_feed_items().row_count()
                                ));
                                ui.invoke_card_enqueue_only(0);
                                ui.invoke_card_enqueue_only(1);
                                ui.invoke_card_enqueue_only(2);
                                step.set(1);
                            }
                            1 => {
                                if ui.get_playlist().row_count() >= 2 || waited.get() > 25 {
                                    crate::debug_log(&format!(
                                        "playlist-shot open rows={} via=vinyl click",
                                        ui.get_playlist().row_count()
                                    ));
                                    ui.invoke_qa_vinyl_click();
                                    step.set(2);
                                } else {
                                    waited.set(waited.get() + 1);
                                }
                            }
                            2 => {
                                let _ = capture_window_png(&ui.window(), &path);
                                crate::debug_log(&format!(
                                    "playlist-shot open_state={} rows={}",
                                    ui.get_playlist_open(),
                                    ui.get_playlist().row_count()
                                ));
                                ui.invoke_qa_vinyl_click();
                                step.set(3);
                            }
                            _ => {
                                // Leave the queue as it was found (the saved
                                // settings start empty), so a QA run does not
                                // edit the owner's playlist.
                                ui.invoke_playlist_clear();
                                crate::debug_log(&format!(
                                    "playlist-shot closed_state={} cleared=1",
                                    ui.get_playlist_open()
                                ));
                                timer_a.stop();
                            }
                        }
                    },
                );
            });
        });
    }
    // QA hook (`BILI_SETTINGS_TAB_TEST=<tag>[,<delay_ms>]`): log the bar rect,
    // call the real `open_settings()` and log the bar rect + view_mode + the
    // nine nav-label widths again, then capture the page. Proves 设置 is a
    // normal view_mode tab with the bar still up, and that every label fits.
    if let Ok(spec) = std::env::var("BILI_SETTINGS_TAB_TEST") {
        let mut parts = spec.split(',');
        let tag = parts
            .next()
            .map(|part| part.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .unwrap_or_else(|| "settings_tab".to_string());
        let delay = parts
            .next()
            .and_then(|part| part.trim().parse::<u64>().ok())
            .unwrap_or(12_000)
            .clamp(1_000, 120_000);
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            let before_weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = before_weak.upgrade() else {
                    return;
                };
                let (x, y, w, h) = home_bar_rect(&ui);
                crate::debug_log(&format!(
                    "settings-test before view_mode={} bar_x={x:.0} bar_y={y:.0} bar_w={w:.0} bar_h={h:.0}",
                    ui.get_view_mode()
                ));
                ui.invoke_open_settings();
            });
            std::thread::sleep(Duration::from_millis(1_200));
            let after_weak = weak.clone();
            let path = qa_capture_path(&tag);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = after_weak.upgrade() else {
                    return;
                };
                let (x, y, w, h) = home_bar_rect(&ui);
                let widths: Vec<f32> = ui.get_settings_nav_widths().iter().collect();
                let labels = [
                    "账户", "外观", "防干扰", "播放", "快捷键", "系统", "弹幕",
                    "屏蔽", "智能云屏键", "优化",
                ];
                let widest = widths
                    .iter()
                    .copied()
                    .fold(0.0f32, f32::max);
                crate::debug_log(&format!(
                    "settings-test after view_mode={} settings_tab={} bar_x={x:.0} bar_y={y:.0} bar_w={w:.0} bar_h={h:.0}",
                    ui.get_view_mode(),
                    ui.get_settings_tab()
                ));
                crate::debug_log(&format!(
                    "settings-test nav column=190 pad=12 widths={:?} labels={:?} widest={widest:.1} fits={}",
                    widths.iter().map(|w| (*w * 10.0).round() / 10.0).collect::<Vec<f32>>(),
                    labels,
                    u8::from(widest <= 190.0 - 24.0)
                ));
                let _ = capture_window_png(&ui.window(), &path);
            });
        });
    }

    // QA hook (`BILI_LOGIN_VIEW=1`): render the logged-out login card on top of
    // a live session, so the chooser (avatar, saved account, QR entry) can be
    // captured without destroying the saved cookies.
    if std::env::var("BILI_LOGIN_VIEW").map(|v| v.trim() == "1").unwrap_or(false) {
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(12_000));
            let _ = weak.upgrade_in_event_loop(|ui| {
                ui.set_logged_in(false);
                ui.set_login_state(0);
                ui.set_login_error("".into());
                crate::debug_log("login view forced (qa)");
            });
        });
    }

    // QA hook (`BILI_NAV_TEST=1`): walk every bar destination through its REAL
    // callback and log the view_mode plus the bar's computed rect, so "the bar
    // never moves" is provable from BILI_DEBUG_LOG without synthetic input.
    // 番剧 (7) and 直播 (8) are walked too: 防干扰 removes the 直播 PILL, never
    // the destination itself, so both entries must still open with it hidden.
    if std::env::var("BILI_NAV_TEST").map(|v| v.trim() == "1").unwrap_or(false) {
        let weak = weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(6_000));
            for step in 0..9u32 {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    match step {
                        0 => ui.invoke_back_to_feed(),
                        1 => ui.invoke_nav_favorites(),
                        2 => ui.invoke_nav_regions(),
                        3 => ui.invoke_fish_open(),
                        4 => ui.invoke_nav_dynamic(),
                        5 => ui.invoke_nav_messages(),
                        6 => ui.invoke_open_settings(),
                        7 => ui.invoke_home_section(3),
                        _ => ui.invoke_live_open(),
                    }
                    let size = ui.window().size();
                    crate::debug_log(&format!(
                        "nav test step={step} view={} win={}x{} bar_y={} bar_h=56",
                        ui.get_view_mode(),
                        size.width,
                        size.height,
                        size.height as i32 - 56 - 28
                    ));
                });
                std::thread::sleep(Duration::from_millis(1500));
            }
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

    // 下载连接数: persist how many parallel upstream fetchers the prefetch
    // proxy opens. `PlaybackControl` exposes the proxy only through stats/url
    // accessors (`set_prefetch_background_paused` is its one live control), so
    // there is no setter for the connection count here: a running stream keeps
    // the count it was opened with, and `player::start_prefetch` reads the
    // stored value for the next video. The log line says that plainly rather
    // than pretending the live proxy changed.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_prefetch_connections(move |n| {
            let connections = settings::normalise_prefetch_connections(n.max(0) as u32);
            let snapshot = update_settings(&settings_store, |prefs| {
                prefs.prefetch_connections = connections;
            });
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status(format!("下载连接数 {connections}（下个视频生效）").into());
            }
            crate::debug_log(&format!(
                "settings connections={connections} applied=next-video"
            ));
        });
    }

    // 防干扰 → 隐藏直播: persist the flag and publish it; the toolbar derives its
    // pill count, positions and `left_w` from the root property, so there is no
    // second list to keep in sync. 直播 (view_mode 23) is reachable only through
    // that pill, so turning the switch on while it is showing falls back to 推荐
    // in the same turn - otherwise the page would be stranded with no entrance.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_hide_live(move |on| {
            let on = settings::normalise_hide_live_tab(on);
            let snapshot = update_settings(&settings_store, |prefs| prefs.hide_live_tab = on);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                let view_before = ui.get_view_mode();
                let fell_back = on && view_before == 23;
                if fell_back {
                    ui.invoke_back_to_feed();
                }
                ui.set_settings_status(
                    if on {
                        "已隐藏直播入口（左侧 4 个）"
                    } else {
                        "已恢复直播入口（左侧 5 个）"
                    }
                    .into(),
                );
                crate::debug_log(&format!(
                    "settings hide_live={on} stored={} view_before={view_before} view_after={} fallback={} pills={}",
                    snapshot.hide_live_tab,
                    ui.get_view_mode(),
                    u8::from(fell_back),
                    ui.get_qa_left_pill_count()
                ));
            }
        });
    }

    // 防干扰 → 隐藏搜索热搜: persist the flag. The search panel reads the root
    // property, so an open field drops the 热搜 list (and collapses to no panel
    // when there is no 联想) on the next frame, with no reload needed.
    {
        let weak = weak.clone();
        let settings_store = settings_store.clone();
        ui.on_settings_hide_hot_search(move |on| {
            let on = settings::normalise_hide_hot_search(on);
            let snapshot =
                update_settings(&settings_store, |prefs| prefs.hide_hot_search = on);
            if let Some(ui) = weak.upgrade() {
                publish_settings(&ui, &snapshot);
                ui.set_settings_status(
                    if on {
                        "已隐藏搜索热搜"
                    } else {
                        "已恢复搜索热搜"
                    }
                    .into(),
                );
                crate::debug_log(&format!(
                    "settings hide_hot_search={on} stored={} hot_rows={}",
                    snapshot.hide_hot_search,
                    ui.get_hot_items().row_count()
                ));
            }
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
    let main_drag = Rc::new(win::WindowDrag::new());
    MAIN_DRAG.with(|slot| *slot.borrow_mut() = Some(main_drag.clone()));
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
                    }
                    if !main_drag_button_held() {
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
        let timer = main_drag_timer.clone();
        let stats = main_drag_stats.clone();
        ui.on_window_drag_end(move || {
            if let Some(ui) = weak.upgrade() {
                if !drag.end_on_signal(main_drag_button_held()) {
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
    // The chooser shows which account it will log into: id + circular face.
    // The face is a public CDN image, so it is fetched without a session and
    // long before the auto-login call finishes.
    if let Some(account) = saved.as_ref() {
        ui.set_saved_account_uid(account.mid.to_string().into());
        if !account.face.is_empty() {
            load_saved_avatar(ui.as_weak(), account.face.clone());
        }
    }
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

    // QA hook (`BILI_QUALITY_TEST=<bvid>`, mirrors BILI_CDN_TEST): print the
    // raw tier lists (`accept_quality` / `accept_description` /
    // `support_formats`) and every stream this account may play, so a locked
    // 大会员 tier can be told apart from a tier the video does not have.
    if let Some(bvid) = bilibili::quality_qa_bvid() {
        let session_slot = session_slot.clone();
        std::thread::spawn(move || {
            for _ in 0..240 {
                if let Some(session) = active_session(&session_slot) {
                    let Some(runtime) = bilibili::shared_runtime() else {
                        return;
                    };
                    match runtime.block_on(session.quality_selftest(&bvid)) {
                        Ok(_) => {}
                        Err(err) => {
                            crate::debug_log(&format!("quality qa failed bvid={bvid} err={err}"))
                        }
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            crate::debug_log(&format!("quality qa skipped bvid={bvid} reason=no-session"));
        });
    }

    // QA hook (`BILI_OPEN_BVID=<bvid>`, mirrors BILI_DETAIL_ON_START): resolve
    // the bvid and open its real player window once the saved session exists,
    // so a specific video (e.g. a 4K upload) can be driven from the env without
    // synthetic input. The resolved row goes through the feed store and the
    // real `card_open(0, 0)` callback, so the player's normal open path runs.
    if let Ok(bvid) = std::env::var("BILI_OPEN_BVID") {
        let bvid = bvid.trim().to_string();
        if !bvid.is_empty() {
            let session_slot = session_slot.clone();
            let feed_store_open = feed_store.clone();
            let ui_weak = weak.clone();
            std::thread::spawn(move || {
                for _ in 0..240 {
                    if let Some(session) = active_session(&session_slot) {
                        let Some(runtime) = bilibili::shared_runtime() else {
                            return;
                        };
                        let detail = match runtime.block_on(session.video_view(&bvid)) {
                            Ok(detail) => detail,
                            Err(err) => {
                                crate::debug_log(&format!(
                                    "open-bvid failed bvid={bvid} err={err}"
                                ));
                                return;
                            }
                        };
                        let cid = if detail.cid > 0 {
                            detail.cid as u64
                        } else {
                            detail
                                .pages
                                .first()
                                .map(|page| page.cid as u64)
                                .unwrap_or(0)
                        };
                        if cid == 0 {
                            crate::debug_log(&format!(
                                "open-bvid skipped bvid={bvid} reason=no-cid"
                            ));
                            return;
                        }
                        let video = bilibili::FeedVideo {
                            bvid: detail.bvid.clone(),
                            cid,
                            aid: detail.aid.max(0) as u64,
                            title: detail.title.clone(),
                            pic: detail.pic.clone(),
                            owner: detail.owner_name.clone(),
                            owner_mid: detail.owner_mid,
                            view: detail.stat_view,
                            duration: detail.duration,
                            pub_ts: detail.pubdate,
                        };
                        crate::debug_log(&format!(
                            "open-bvid bvid={bvid} cid={cid} title_len={}",
                            detail.title.len()
                        ));
                        if let Ok(mut feed) = feed_store_open.lock() {
                            feed.insert(0, video);
                        }
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.invoke_card_open(0, 0);
                            }
                        });
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                crate::debug_log(&format!("open-bvid skipped bvid={bvid} reason=no-session"));
            });
        }
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

    // Diagnostic hook `BILI_PGC_TEST=1`: drive the 番剧 page, its paging, card
    // playback and the player's whole-season 选集 through the real callbacks.
    if std::env::var("BILI_PGC_TEST")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
    {
        run_pgc_test(weak.clone(), session_slot.clone(), pgc_host.clone());
    }

    // Diagnostic hook `BILI_PGC_F171=1`: the 排序 dropdown (four verified sorts,
    // per-season_type persistence) and 继续观看 (badge, resume, 选集 highlight),
    // all through the real callbacks, with the readbacks in the log.
    if std::env::var("BILI_PGC_F171")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
    {
        run_pgc_f171_test(weak.clone(), session_slot.clone(), pgc_host.clone());
    }

    // Diagnostic hook (mirrors BILI_SHUFFLE_TEST / BILI_SEEK_TEST): page the
    // active section forward so the no-scroll client paging can be exercised
    // end to end from the backend without synthetic input. `<n>` pages the
    // feed; `<view>:<n>` first opens that section through its existing
    // navigation callback (1 搜索 / 5 历史 / 6 稍后再看 / 8 分区 / 9 排行榜 /
    // 13 动态) and then pages it (see `run_page_test`).
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

    // Diagnostic hook `BILI_PICK_TEST=hot:<n>` / `BILI_PICK_TEST=suggest:<text>`:
    // fires the same callback the popup under the bar fires when a hot keyword
    // or a 联想 row is clicked, then reports whether the panel closed and the
    // results page actually loaded. The click has to behave like typing the
    // text and pressing Enter: results in, popup gone.
    if let Ok(spec) = std::env::var("BILI_PICK_TEST") {
        let spec = spec.trim().to_string();
        if !spec.is_empty() {
            let weak = weak.clone();
            let session_slot = session_slot.clone();
            std::thread::spawn(move || {
                for _ in 0..600 {
                    if active_session(&session_slot).is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                // The startup routine ends with its own `view_mode = 0` when the
                // first screen is ready (`login-step id=done`), so a click that
                // lands before it is undone a moment later. Wait for the feed to
                // be on screen first, then give the last startup step its time.
                for _ in 0..200 {
                    if f145_on_ui(&weak, |ui| ui.get_feed_items().row_count() > 0)
                        .unwrap_or(false)
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                std::thread::sleep(Duration::from_millis(2500));
                let (kind, value) = spec.split_once(':').unwrap_or(("hot", spec.as_str()));
                let kind = kind.trim().to_string();
                let value = value.trim().to_string();
                // Open the field first, exactly like a user click does.
                let open_weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = open_weak.upgrade() {
                        ui.set_search_text(SharedString::from(""));
                        ui.set_search_open(true);
                        ui.invoke_search_opened();
                    }
                });
                std::thread::sleep(Duration::from_millis(1200));
                let before = f145_on_ui(&weak, |ui| {
                    (
                        u8::from(ui.get_search_open()),
                        ui.get_hot_items().row_count(),
                        ui.get_suggest_items().row_count(),
                    )
                })
                .unwrap_or((0, 0, 0));
                let pick_weak = weak.clone();
                let kind_click = kind.clone();
                let value_click = value.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = pick_weak.upgrade() {
                        if kind_click == "suggest" {
                            ui.invoke_suggest_selected(value_click.into());
                        } else if let Ok(index) = value_click.parse::<i32>() {
                            ui.invoke_hot_selected(index);
                        } else {
                            // No index: use the first hot keyword the panel got.
                            ui.invoke_hot_selected(0);
                        }
                    }
                });
                let mut after = (0u8, 0i32, 0i32);
                for _ in 0..60 {
                    std::thread::sleep(Duration::from_millis(100));
                    if let Some(state) =
                        f145_on_ui(&weak, |ui| (u8::from(ui.get_search_open()), ui.get_view_mode(), 0))
                    {
                        after.0 = state.0;
                        after.1 = state.1;
                        if state.1 == 1 {
                            break;
                        }
                    }
                }
                let rows = f145_on_ui(&weak, |ui| ui.get_search_items().row_count()).unwrap_or(0);
                crate::debug_log(&format!(
                    "pick-test kind={kind} value={value:?} open_before={} hot={} suggest={} open_after={} view_after={} result_rows={rows}",
                    before.0, before.1, before.2, after.0, after.1,
                ));
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

    // ---- f124 QA: wheel replay, captures and measurements -----------------
    // One env-driven pass (`BILI_F124_TEST=<delay_ms>`) that walks the pages
    // this wave changed. The wheel proof dispatches REAL wheel events into the
    // Slint window's item tree at a point taken from the strip's own window
    // rect: the pointer is in the strip's LEFT PADDING, not on a chip, and no
    // OS cursor / synthetic input is involved. See `start_f124_pass`.
    maybe_start_f124(weak.clone());

    // ---- f127 QA: the shared 收藏 folder strip (推荐流 / 收藏夹) ----------
    // `BILI_F127_TEST=<delay_ms>` walks the two folder tabs and 历史, captures
    // each and logs the strip slot / switcher / content geometry. See
    // `run_f127_test`.
    maybe_start_f127(weak.clone(), session_slot.clone(), fav_view.clone());

    // ---- f143 QA: toolbar selection, refresh fill, 收藏 skeleton ----------
    // `BILI_F143_TEST=bar|refresh|fav` runs one capture pass. See
    // `maybe_start_f143`; the fish phase is started from the 养鱼 block.
    maybe_start_f143(weak.clone(), session_slot.clone(), fav_view.clone());

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

    // Frameless + compositor alpha: the native HWND only exists once the event
    // loop has built the window, so queue the window chrome to run (and retry)
    // on the UI thread after `run()` starts.
    schedule_window_chrome(weak.clone(), theme_dark, 120);

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





// ---------------------------------------------------------------------------
// f127 QA: the shared 收藏 folder strip (推荐流 / 收藏夹).
//
// `BILI_F127_TEST=<delay_ms>` walks the 收藏 page through the REAL routes and
// proves the one-strip design from the log and three captures:
//   1. 推荐流 + folder 1          -> f127_fav_stream
//   2. 收藏夹 + the same folder    -> f127_fav_folder
//   3. 历史 (strip hidden)         -> f127_fav_history
//   4. back to 推荐流              -> the folder and the strip offset survived
// Between 1 and 2 a REAL wheel event moves the strip's offset, so the tab
// switch has something to preserve. Nothing here clicks the OS cursor: the
// wheel enters Slint's own item tree at the strip's left padding, exactly like
// the f124 pass.
// ---------------------------------------------------------------------------









// ---------------------------------------------------------------------------
// f143 QA: the toolbar selection / refresh-fill / 收藏 skeleton pass.
//
// `BILI_F143_TEST=bar|refresh|fav` drives one phase through the REAL routes and
// callbacks. Captures land in `BILI_CAPTURE_DIR` (default target/qa):
//   bar      -> f143_bar_selected.png  (the home bar with 推荐 selected)
//   refresh  -> f143_refresh_fill.png  (a 40 % determinate fill on 刷新)
//   fav      -> f143_fav_loaded.png + f143_fav_skeleton.png at the same scroll
// Only property writes / callbacks the app itself uses are involved; no OS
// cursor and no synthetic input.
// ---------------------------------------------------------------------------
thread_local! {
    /// The real 收藏 model, parked on the UI thread while the f143 skeleton
    /// fixture is on screen (ModelRc is UI-thread-only, so it cannot ride the
    /// QA thread's stack).
    static F143_KEPT: RefCell<Option<ModelRc<MasonryCard>>> = const { RefCell::new(None) };
}









thread_local! {
    /// The real 推荐流 model parked while the no-cover fixture is on screen.
    static F143_FISH_KEPT: RefCell<Option<ModelRc<FishRow>>> = const { RefCell::new(None) };
}

















// -------------------------- 网络分析 profiler ------------------------------





















// ---- GPU utilisation for the interpolation row ---------------------------
//
// The player's interpolation badge shows `GPU 57%` next to the measured frame
// cost. `nvidia-smi` takes 100-200 ms per call, so it is sampled every two
// seconds on its own thread and the tick only reads the cached number. A machine
// without `nvidia-smi` (or without an NVIDIA GPU) publishes nothing: the UI
// hides the cell instead of showing a fake 0%.




// ===================== 弹幕 motion lane (fast tick) =====================
//
// The player used to have a single 33 ms timer whose handler did everything:
// toasts, the 画质 menu, interp stats, the playlist, panel geometry, the
// buffering overlay, the profiler text and the danmaku walk. Asking that body
// for 120 Hz would have made the app slower, not smoother, so the timer is now
// split:
//
//   * `danmaku_tick` (33 ms, unchanged) keeps every non-motion job and, once per
//     tick, publishes the visible comment set plus the media-clock anchor.
//   * `danmaku_frame_tick` (`danmaku_frame_interval`, at most 8 ms) owns only the
//     motion of the comments already on screen. It takes no lock (a `RefCell`
//     on the UI thread), allocates nothing, formats nothing and reads no clock
//     but `Instant`: per comment it is one multiply, one rounding and - only
//     when the whole pixel changed - one model row write.
//
// Motion is time-based, never accumulated: the media time drawn at a frame is
// `anchor_position + (now - anchor_instant)`, and the anchor is refreshed by the
// slow tick from the real playback position. A late or missed frame therefore
// cannot drift; the only limit is [`DANMAKU_FRAME_MAX_STEP_MS`] per frame, so a
// 200 ms stall moves the comments 100 ms and catches up over the next frame
// instead of teleporting them across the picture.
































// -- QA: 选集 (f153) ---------------------------------------------------------
//
// `BILI_SERIES_TEST=1` drives the whole feature from the env, through the real
// UI paths (the panel properties the toolbar cell writes, the `series_pick`
// callback the row click fires), so nothing has to be synthesised and no OS
// input is used:
//
//   BILI_SERIES_OPEN_MS      wait after the player opens (default 6000)
//   BILI_SERIES_PICK         flat row index to pick (omitted = no switch)
//   BILI_SERIES_SHOT_DIR     capture directory
//   BILI_SERIES_SHOT_BUTTON  file name for the pill crop
//   BILI_SERIES_SHOT_PANEL   file name for the open list
//   BILI_SERIES_SHOT_AFTER   file name after the switch
//
// It logs the open/close UI frame gaps (p50/p95, same 4ms probe as f147) and
// samples the picture's luma through the switch (a black frame would show as a
// dark sample).









// ---------------------------------------------------------------------------
// 直播 page (view_mode 23)
//
// The aggregate (`xlive/.../index/getList`) is the whole page: 11 module rows
// of 10 rooms today, plus 24 area entrances. The chips load their grid from
// `room/v1/area/getRoomList` (code 0, 30/page); the newer `second/getList`
// answers `-352` for this client even WBI-signed, so it is only the fallback
// and a refusal there is named in the UI (`-352 风控`) instead of "暂不可用".
//
// Models: `live_cards` is ONE flat model; a 推荐 row addresses its slice with
// `start`/`count` (`live_modules`), and at most 4 columns per row keep the
// cards legible. Covers stream in after the rows land, patched by index.
// ---------------------------------------------------------------------------
















// ===================== f159 QA: 5+5 bar, skeleton, chips, card A/B =========
//
// `BILI_F159_TEST=<stages>` — the input-free driver for the f159 wave:
//
//   toolbar  : the 5 + 5 bar in both 防干扰 states (numbers + captures)
//   skeleton : the still, breathing placeholder (capture + breathing gaps)
//   chips    : the top chip strips of 收藏 / 分区 / 番剧 / 直播 (captures plus an
//              exact-pixel scan of the chip fill)
//   ab       : the card surface A/B - 20 x 5s feed scroll and 20 x search morph
//              for each variant, measured with the same 4ms UI-thread probe the
//              f149 numbers came from
//   cold     : THIS process's first-screen time, then exit (one process per
//              sample, which is what "cold" means)
//   `1`/`all`= toolbar,skeleton,chips,ab. `cold` is opt-in: it needs 20 separate
//              launches per variant.
//
// Every line is prefixed `f159 ` except the toolbar probe, which keeps the
// `f158 toolbar state=...` shape it has always had (plus the new 5 + 5 fields).
























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































// ---------------------------------------------------------------------------
// 登录后全 tab 预热
//
// 登录只等首页预加载；进入之后把其余 tab 的**第一页**也热起来，切过去不再
// 从零开始。规则全部可从 `BILI_DEBUG_LOG` 复查：
//
// * `login-step id=done` 之后再等 [`WARM_START_DELAY`]，首屏渲染期间绝不抢带宽；
// * 最多 [`WARM_CONCURRENCY`] 个 tab 同时热（一个 tab 一个任务），单任务硬上限
//   [`WARM_TASK_LIMIT`]，超时只放弃这一页，不拖住后面的 tab；
// * 每个 tab 只热第一页 + [`WARM_COVERS`] 张封面；封面走播放列表同一个
//   [`cover_cache`]，已经在缓存里的不再拉（`cached=` 就是命中数）；
// * 任何一个播放器打开（播放 / 缓冲）时整个队列暂停，播完继续——和养鱼 ASR
//   共用 [`player_active`] 这道闸门；
// * `hide_live_tab` 关掉的 tab 直接跳过。
//
// 每个任务一行 `warm tab=… ms=… items=… covers=… cached=…`，整轮结束一行
// `warm done tabs=N/N ms=… bytes=…`。`BILI_WARM=0` 关闭整套预热（QA 的
// before/after 对照就是它）。
// ---------------------------------------------------------------------------

































thread_local! {
    /// The 番剧 page as the player's progress writer needs it: the episode that
    /// just reached its first frame must light up the card behind the player,
    /// and that card model lives on the main window.
    static PGC_PAGE: RefCell<Option<PgcPageHost>> = RefCell::new(None);
}













// ---- 继续观看 (seen-episode progress) --------------------------------------
//
// The table lives in `continue.json` (see `settings::ContinueStore`); this is
// the process-wide handle every reader and the ONE writer (the player tick,
// after the episode's first frame) share.























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

























// ---------------------------------------------------------------------------
// Skeleton shimmer driver (UI thread only).
//
// While any incremental cover is still pending, a repeating timer advances the
// shared `skeleton-phase` so every placeholder sweeps a soft highlight and the
// floating refresh glyph spins. The timer stops as soon as the last pending
// cover settles (and after a 30s safety deadline), so idle frames cost nothing.
// ---------------------------------------------------------------------------

thread_local! {
    static SKELETON: RefCell<SkeletonState> = RefCell::new(SkeletonState {
        timer: slint::Timer::default(),
        remaining: 0,
        phase: 0.0,
        armed_at: Instant::now(),
    });
}








































// ================= 番剧 / PGC page (view_mode 22) ==========================
//
// Three real endpoints drive this page, each fetched on its own netpool worker
// and published on the UI thread:
//   * 每日放送   pgc/web/timeline      (7 days, today marked)
//   * 分区索引   pgc/season/index/result (20/page, appended on scroll)
//   * 排行榜     pgc/web/rank/list     (99 rows, one request)
// A card click fetches the season detail, checks the episode's region gate and
// only then opens the player, which also receives the whole season as its 选集
// list (`series_kind = 3`).

































// ---------------------------------------------------------------------------
// 收藏: the folder-scoped masonry behind 推荐流 and 收藏夹
//
// ONE page, two tabs (see the 收藏 page block in `ui/app.slint`):
//   * 推荐流 (`FavMode::Stream`, home_tab 5, view_mode 0) is the 收藏推荐 feed:
//     tab 0 is the Pinterest-style mix (the largest folders' first page, two
//     random picks per folder expanded through `related()`, merged, deduped,
//     shuffled, capped); tab `i + 1` is that one folder as a seed - its own
//     page plus two picks' related videos - i.e. the same feed restricted to
//     the folder. There is no "recommendation feed" endpoint to filter on the
//     server: this feed IS built here, so the folder is the exact seed.
//   * 收藏夹 (`FavMode::Library`, view_mode 3) is the plain listing: tab 0 is
//     every folder's first page merged in folder order, tab `i + 1` is that
//     folder's own videos filled page by page (capped) - no `related()`
//     expansion, no shuffle. Same masonry, same strip, same selection.
//
// The shipped UI owns 全部 and only lists the user's own folders:
// `fav_tabs[i]` is folder `i`'s name and `fav_tab == i + 1` (`0` = 全部, see
// `ui/app.slint`). The names live on in `FavViewState.folders`, which is what
// maps a tab back to the folder's `media_id`. Every built (mode, tab) is cached
// in-process for [`FAV_VIEW_TTL`], so switching back inside the window costs no
// network.
// ---------------------------------------------------------------------------










































































#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(time: *mut WinLocalTime);
}









// ---------------------------------------------------------------------------
// f167 · 鱼缸 timeline + 清空 capture pass
// ---------------------------------------------------------------------------



























thread_local! {
    /// The dashboard state, reachable from the engine's hook (which may only
    /// capture `Send` data) and from every callback.
    static FISH_UI: RefCell<Option<Rc<FishUi>>> = const { RefCell::new(None) };
    /// The auto-feed timer and the f136 QA timer are kept alive here for the
    /// process lifetime (dropping a `slint::Timer` stops it).
    static FISH_AUTO_TIMER: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
    static FISH_PLAYBACK_TIMER: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
    static F136_TIMER: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
    static F160_PAUSE_TIMER: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
}

thread_local! {
    /// The active session and the player state, so a dashboard row click can
    /// reach the normal player path from a background resolve.
    static FISH_SESSION: RefCell<Option<Arc<bilibili::Session>>> = const { RefCell::new(None) };
    static FISH_PLAYER: RefCell<Option<Rc<PlayerState>>> = const { RefCell::new(None) };
}


thread_local! {
    /// `settings_store` for the dashboard's row click.
    static FISH_SETTINGS: RefCell<Option<Arc<Mutex<settings::Settings>>>> =
        const { RefCell::new(None) };
}
























