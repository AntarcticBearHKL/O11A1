//! The player engine: window thread, decode sessions, pacer, audio clock,
//! overlay state and every command the public [`crate::Player`] can send.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CONTROL, VK_MENU,
};

use crate::audio::{AudioCommand, AudioEngine};
use crate::clipboard;
use crate::danmaku::{
    convert_item, Band, DanmakuHit, DanmakuItem, DanmakuModel, DanmakuOverlay, DanmakuSettings,
    LongPress,
};
use crate::decode::{DecodedFrame, FrameData, VideoDecoder, VideoSpec};
use crate::detail::{CommentData, DetailEvent, DetailRequest};
use crate::enhance::{EnhanceMode, EnhancePipeline};
use crate::format::{clamp_seek, format_time};
use crate::gpu::{AspectMode, Composition, Gpu, GpuFrame, VideoSurface};
use crate::heartbeat::{Beat, Heartbeat};
use crate::interp::{self, InterpMode};
use crate::log;
use crate::overlay::{
    dm_menu_rect, dm_menu_row, dm_report_rect, dm_report_row, settings_menu_height, AudioRowUi,
    ChoiceRowUi, CommentUi, DanmakuMenuUi, ImageData, InfoUi, Overlay, PageUi, QualityRowUi, Rect,
    ReplyUi,
    SettingsMenu, SidebarTab, StoryboardCellUi, ToastUi, UiModel, UndoToastUi, DM_MENU_BLOCK,
    DM_MENU_COPY, DM_MENU_LABELS, DM_MENU_LIKE, DM_MENU_REPORT, DM_REPORT_LABELS, HEAT_BUCKETS,
    PERFORMANCE_CONTENT_H, SETTINGS_ROWS, TB_COMMENTS, TB_DANMAKU, TB_INFO, TB_MUTE, TB_PERF,
    TB_PRECISION, TB_SERIES, TB_SETTINGS, TB_SPEED, DEFAULT_QUALITY_HEIGHTS, quality_height_label,
};
use crate::source::{MediaSource, TrackRequest};
use crate::state::{PlayerState, SaveDebouncer, SAVE_DEBOUNCE};
use crate::storyboard::{self, cell_index_for};
use crate::window::{self, InputEvent};
use crate::PlayerConfig;

const DEFAULT_WINDOW: (u32, u32) = (1000, 600);
const START_AUDIO_WAIT_MS: u64 = 1_200;
/// Keep one source picture ready ahead of the presentation clock so 60 Hz
/// ticks can be built between source frames.
const INTERP_LOOKAHEAD_MS: u64 = 220;
/// Mean luma difference at which a tick is treated as a scene cut. A blend
/// across two shots is the source of the old white "flash"; show the nearest
/// real picture instead.
const INTERP_CUT_DIFF: u32 = 52;
/// A/B spans up to this length keep their decoded pictures in memory. Short
/// loops then replay from the cache instead of spawning a new decoder at A on
/// every pass. Longer spans use the background prefetch decoder.
const AB_CACHE_MAX_MS: u64 = 2_000;
/// Hard ceiling for the A/B frame cache (decoded NV12 bytes).
const AB_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Decoded pictures for one A/B pass, captured while the first pass plays.
struct AbCache {
    a: u64,
    b: u64,
    frames: Vec<DecodedFrame>,
    bytes: usize,
    ready: bool,
    disabled: bool,
}
const AV_DRIFT_MS: u64 = 200;
/// A picture may trail the device clock by this much before the engine treats
/// it as a desync instead of normal decode jitter.
const AV_VIDEO_LAG_MS: u64 = 250;
/// The picture must be this far behind before the buffer guard freezes the
/// sound. Waiting for an empty (zero-frame) ring already costs ~200 ms of
/// audio lead; two frames is the last point before the sound runs away.
const BUFFER_LOW_FRAMES: usize = 2;
/// Resume only when the decoder has rebuilt a real lead. A single frame is
/// not enough to survive the next cache chunk; eight frames is ~130 ms at
/// 60 fps and hides the 100 ms waveOut pause/resume quantisation.
const BUFFER_RESUME_FRAMES: usize = 8;
/// No new picture for this long with a nearly empty ring means the reader is
/// waiting on the network, not that the GPU hit a one-frame hitch.
const BUFFER_LOW_MS: u64 = 400;
/// Start warming the next part's cache when this much of the current part is
/// left. Ten seconds covers the playurl resolve plus the first range requests,
/// so the auto "next part" switch lands on a local opening chunk.
const NEXT_PAGE_PREFETCH_MS: u64 = 10_000;
/// How often the watched position of the current part is written to
/// `watch_progress.json`. Five seconds is one heartbeat interval and bounds
/// what a killed process can lose.
const WATCH_SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// While buffer-paused, wait until this much contiguous video is on disk.
/// Resuming as soon as the ring refills makes a link that is slower than the
/// bitrate pause and resume every few hundred milliseconds; a real lead turns
/// that into a few seconds of smooth playback between waits.
const BUFFER_LEAD_MS: u64 = 2_500;
/// Never hold the sound longer than this for a cache lead. The loopback server
/// proxies a stuck chunk after a few seconds, so the ring can be full while the
/// chunk is still not marked done. Eight seconds made a recoverable CDN hole
/// look like a frozen player; 1.5 s is long enough for a normal chunk and short
/// enough that the user sees a brief pause, not a dead window.
const BUFFER_WAIT_MAX_MS: u64 = 1_500;
/// How long the picture may sit past `AV_VIDEO_LAG_MS` while frames are still
/// arriving before the engine drops the gap. A single GPU/engine hitch must
/// not cost a seek; a decoder that cannot keep up must.
const AV_VIDEO_LAG_HOLD_MS: u64 = 900;
/// Minimum spacing between two video-only catch-up seeks.
const AV_CATCHUP_COOLDOWN_MS: u64 = 2_000;
/// After the sound was paused for a dry picture, the first picture back
/// starts this grace. If the decoder still cannot reach the frozen clock,
/// the picture is jumped there instead of holding the sound for tens of
/// seconds (a hardware decoder below real time).
const AV_STALL_CATCHUP_MS: u64 = 1_500;
/// A video-only catch-up that has not produced a picture in this long is
/// reported instead of leaving the sound paused forever.
const AV_CATCHUP_TIMEOUT_MS: u64 = 8_000;
/// Hold time for 屏蔽该用户 on a comment (left button).
const LONG_PRESS: Duration = Duration::from_millis(500);
/// Movement that turns a hold into a drag and cancels the long-press.
const LONG_PRESS_SLOP: f32 = 8.0;
/// 撤销 window after blocking a sender.
const UNDO_MS: u64 = 3_000;
/// `danmaku_report` reports 其他 as reason 11; the text field is only read there.
const REPORT_OTHER_REASON: u32 = 11;

/// Commands accepted by the player thread. The `Player` methods are the
/// convenient surface; this enum is public so embedders can mirror the set.
pub enum Command {
    Play,
    Pause,
    TogglePlay,
    Seek(u64),
    /// Replace the video shown by this window (the window, its size and its
    /// state stay; a card left-click reuses the most recently focused player).
    LoadSource(crate::PlayerConfig),
    SetVolume(u8),
    SetSpeed(f64),
    SetInterp(InterpMode),
    /// 0 = follow the display refresh, otherwise a fixed target in Hz.
    SetInterpHZ(u32),
    SetEnhance(EnhanceMode),
    SetDanmaku(bool),
    ToggleDanmaku,
    ToggleMini,
    ToggleStats,
    SetAspect(AspectMode),
    CycleAspect,
    SetOnTop(bool),
    ToggleOnTop,
    SetBrightness(f32),
    SetAudioOnly(bool),
    ToggleAudioOnly,
    Screenshot,
    CycleSpeed(i32),
    /// Reposition/resize the Electron-hosted popup (physical pixels).
    SetRect {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// Show/hide the Electron-hosted popup.
    SetVisible(bool),
    /// Hovered seek time (ms) for the native storyboard thumbnail; `None`
    /// clears the preview.
    Preview(Option<u64>),
    /// Show a locally sent danmaku immediately.
    SendDanmaku {
        text: String,
        mode: u32,
    },
    Close,
}

#[derive(Default)]
pub struct SharedState {
    pub position_ms: AtomicU64,
    pub duration_ms: AtomicU64,
    pub playing: AtomicBool,
    pub volume: AtomicU8,
    pub muted: AtomicBool,
    pub interp: AtomicU8,
    pub enhance: AtomicU8,
    pub speed_milli: AtomicU32,
    pub fps_tenths: AtomicU32,
    pub generated: AtomicU64,
    pub blend_us: AtomicU32,
    pub danmaku_visible: AtomicBool,
    pub danmaku_total: AtomicU32,
    pub mini: AtomicBool,
    pub aspect: AtomicU8,
    pub topmost: AtomicBool,
    pub brightness_milli: AtomicU32,
    pub audio_only: AtomicBool,
    pub closed: AtomicBool,
    /// True after the decoder reached the natural end of the stream. The shell
    /// uses it to advance the persistent play queue.
    pub ended: AtomicBool,
    /// End-of-video behaviour mirrored for the Electron queue: 0 pause,
    /// 1 loop, 2 next / close.
    pub loop_mode: AtomicU8,
    /// Hosted-window input counters. Electron polls these and turns them into
    /// play/pause and fullscreen actions; the native overlay is not drawn.
    pub click_seq: AtomicU64,
    pub double_click_seq: AtomicU64,
    /// Bumped when this native window becomes active. The shell targets the
    /// player with the newest value for card replacements.
    pub focus_seq: AtomicU64,
    /// Coalesced mouse-move counter used to reveal fullscreen controls.
    pub move_seq: AtomicU64,
    /// Right-clicked danmaku under the pointer, consumed by the Electron UI.
    pub danmaku_hits: std::sync::Mutex<std::collections::VecDeque<DanmakuHit>>,
    /// 500 ms left-press on a danmaku, consumed by the Electron UI.
    pub long_press_hits: std::sync::Mutex<std::collections::VecDeque<DanmakuHit>>,
    pub long_press_seq: AtomicU64,
    /// Bumped whenever this window writes danmaku settings to the global
    /// settings file. The engine watches it and re-broadcasts the resulting
    /// settings so every UI stays in sync with the native popup.
    pub settings_revision: AtomicU64,
}

impl SharedState {
    fn sync(&self, engine: &Engine) {
        self.position_ms.store(engine.position_ms, Ordering::SeqCst);
        self.duration_ms.store(engine.duration_ms, Ordering::SeqCst);
        self.playing.store(engine.playing, Ordering::SeqCst);
        self.volume.store(engine.volume, Ordering::SeqCst);
        self.muted.store(engine.muted, Ordering::SeqCst);
        self.interp.store(engine.interp.as_u8(), Ordering::SeqCst);
        self.enhance.store(engine.enhance.as_u8(), Ordering::SeqCst);
        self.danmaku_visible
            .store(engine.danmaku_visible, Ordering::SeqCst);
        self.danmaku_total.store(
            engine.danmaku.total().min(u32::MAX as usize) as u32,
            Ordering::SeqCst,
        );
        self.mini.store(engine.mini, Ordering::SeqCst);
        self.aspect.store(engine.aspect.as_u8(), Ordering::SeqCst);
        self.topmost
            .store(window::is_topmost(engine.hwnd), Ordering::SeqCst);
        self.brightness_milli.store(
            (engine.brightness * 1000.0).round() as u32,
            Ordering::SeqCst,
        );
        self.audio_only.store(engine.audio_only, Ordering::SeqCst);
        self.ended.store(engine.ended, Ordering::SeqCst);
        self.loop_mode.store(engine.loop_mode, Ordering::SeqCst);
        self.speed_milli
            .store((engine.speed * 1000.0).round() as u32, Ordering::SeqCst);
    }
}

/// Entry point used by [`crate::Player::open`]. Runs on its own thread for the
/// whole life of the window; `ready` is signalled once the composition tree is
/// up (or with the setup error).
pub fn run(
    config: PlayerConfig,
    source: MediaSource,
    shared: Arc<SharedState>,
    commands: Receiver<Command>,
    danmaku_rx: Receiver<Vec<DanmakuItem>>,
    danmaku_tx: Sender<Vec<DanmakuItem>>,
    detail_tx: Sender<DetailRequest>,
    detail_rx: Receiver<DetailEvent>,
    session: Option<Arc<o11a1::bilibili::Session>>,
    state: PlayerState,
    ready: Sender<Result<(), String>>,
    source_rx: Receiver<std::result::Result<crate::DeferredOpen, String>>,
    source_tx: Sender<std::result::Result<crate::DeferredOpen, String>>,
) {
    let result = Engine::new(
        config,
        source,
        shared.clone(),
        commands,
        danmaku_rx,
        danmaku_tx,
        detail_tx,
        detail_rx,
        session,
        state,
        source_rx,
        source_tx,
    );
    let mut engine = match result {
        Ok(engine) => {
            let _ = ready.send(Ok(()));
            engine
        }
        Err(error) => {
            let _ = ready.send(Err(format!("{error:#}")));
            shared.closed.store(true, Ordering::SeqCst);
            return;
        }
    };
    let result = engine.run_loop();
    if let Err(error) = &result {
        log::line(&format!("engine stopped with error: {error:#}"));
    }
    engine.shutdown();
    shared.closed.store(true, Ordering::SeqCst);
}

/// One frame-ruler notch is 12 logical pixels. Dragging the ruler left
/// (negative delta) advances playback; a wobble shorter than one notch is zero.
fn tuner_steps(delta_px: f32, notch_px: f32) -> i64 {
    if notch_px <= 0.0 {
        return 0;
    }
    (-delta_px / notch_px).trunc() as i64
}

/// The precision panel's frame-step modes, in click order. Any unexpected
/// stored value returns to the single-frame mode.
fn next_frame_step(step: u64) -> u64 {
    match step {
        1 => 2,
        2 => 5,
        5 => 10,
        _ => 1,
    }
}

/// One wheel notch is 120 units and moves the volume 5%. Precision touchpads
/// and free-spinning wheels send fractions of a notch; those accumulate so a
/// slow scroll still moves the volume instead of rounding every event away.
/// A direction reversal drops the leftover of the other direction so the first
/// notch back responds immediately.
fn wheel_volume_step(accum: &mut f32, direction: &mut f32, delta: i32) -> i32 {
    let sign = delta.signum() as f32;
    if sign == 0.0 {
        return 0;
    }
    if *direction != 0.0 && sign != *direction {
        *accum = 0.0;
    }
    *direction = sign;
    *accum += delta as f32 * 5.0 / 120.0;
    let step = (*accum).trunc() as i32;
    *accum -= step as f32;
    step
}

struct Engine {
    source: MediaSource,
    /// The open request, kept so a failed initial resolve can be retried.
    open_config: PlayerConfig,
    /// `true` while the window shows the loading card and no source exists.
    loading: bool,
    /// A resolve worker is in flight and its result is expected on `source_rx`.
    source_pending: bool,
    /// The replace request that produced the in-flight resolve (initial opens
    /// keep their config in `open_config`). Used to retry a failed swap.
    pending_replace: Option<PlayerConfig>,
    /// A newer card click while a resolve was in flight: run it as soon as the
    /// current answer lands, so the last click always wins.
    queued_replace: Option<PlayerConfig>,
    /// The last replacement that failed; the error card's retry re-runs it
    /// instead of restarting the old video.
    failed_replace: Option<PlayerConfig>,
    source_rx: Receiver<std::result::Result<crate::DeferredOpen, String>>,
    source_tx: Sender<std::result::Result<crate::DeferredOpen, String>>,
    requested_start_ms: u64,
    session: Option<Arc<o11a1::bilibili::Session>>,
    shared: Arc<SharedState>,
    commands: Receiver<Command>,
    hwnd: HWND,
    events: Arc<window::WindowEvents>,
    /// Electron-hosted video-only popup: no native chrome, no input handling.
    external: bool,
    /// Live stream: interpolation/enhancement are forced off and the settings
    /// tab shows a live-specific list.
    is_live: bool,
    /// True while the external storyboard thumbnail is on screen.
    external_preview_visible: bool,
    /// Press start for the Electron-hosted long-press gesture: start time,
    /// point, and the danmaku captured at press time (before it scrolls on).
    external_press: Option<(Instant, i32, i32, Option<DanmakuHit>)>,
    gpu: Gpu,
    video: VideoSurface,
    overlay: Overlay,
    composition: Composition,
    danmaku_layer: DanmakuOverlay,
    danmaku: DanmakuModel,
    danmaku_rx: Receiver<Vec<DanmakuItem>>,
    danmaku_tx: Sender<Vec<DanmakuItem>>,
    danmaku_visible: bool,
    danmaku_band: Band,
    next_danmaku_draw: Instant,
    /// The unfiltered live fetch, kept so the 历史 view can be closed (or a
    /// blocked sender undone) without another network round-trip.
    live_danmaku: Vec<DanmakuItem>,
    /// Right-click comment menu and its hover/like/report/block state.
    dm_menu: Option<DanmakuMenuUi>,
    /// Right press that opened the menu: start point + whether it moved far
    /// enough to count as a drag selection. Releasing after a drag activates
    /// the row under the pointer; a plain click leaves the menu open.
    dm_menu_drag: Option<(f32, f32, bool)>,
    /// Armed left long-press (tracker + the comment captured at press time).
    dm_long: Option<(LongPress, DanmakuHit)>,
    /// `dmid`s with a thumb-up request in flight (duplicate clicks ignored).
    dm_thumb_inflight: HashSet<String>,
    dm_action_tx: Sender<DmActionResult>,
    dm_action_rx: Receiver<DmActionResult>,
    /// 3 s 撤销 row after 屏蔽该用户.
    undo: Option<UndoBlock>,
    history_dates: Vec<String>,
    history_dates_loading: bool,
    history_dates_error: Option<String>,
    history_dates_rx: Option<Receiver<Result<Vec<String>, String>>>,
    history_date: Option<String>,
    history_items: Vec<DanmakuItem>,
    history_loading: bool,
    history_error: Option<String>,
    history_rx: Option<Receiver<Result<HistoryPayload, String>>>,
    /// A QA `dm_history_date` waiting for the async date list.
    qa_history_pick: Option<usize>,
    /// debounced `player_state.json` writes.
    state: PlayerState,
    state_save: SaveDebouncer,
    /// Last write of `watch_progress.json`; while a video plays the current
    /// position is stored every [`WATCH_SAVE_INTERVAL`].
    last_watch_save: Instant,
    enhance_pipeline: Option<EnhancePipeline>,
    decoder: Option<VideoDecoder>,
    /// A decoder pre-rolled at A while the current pass approaches B. It is
    /// swapped in at the loop point so the picture never reloads.
    loop_prefetch: Option<VideoDecoder>,
    /// Wheel notches only move a preview; the actual seek happens once the
    /// wheel has been idle for a moment.
    pending_wheel_seek: Option<(u64, Instant)>,
    audio: Option<AudioEngine>,
    /// `None` when a begin/first frame is still outstanding.
    client: (u32, u32),
    scale: f32,
    fullscreen: bool,
    saved_window: Option<(isize, RECT)>,

    // Playback state.
    duration_ms: u64,
    position_ms: u64,
    /// Presentation clock anchor in the media timeline and wall time.
    base_ms: u64,
    anchor: Option<Instant>,
    paused_at: u64,
    playing: bool,
    requested_play: bool,
    starting: bool,
    first_frame_at: Option<Instant>,
    seek_in_flight: bool,
    seek_target: u64,
    /// Media timestamp where the current decode session began (0, a seek
    /// target, or the A point of an AB loop). Drives the perf loading grid.
    stream_start_ms: u64,
    generation: u64,
    session_started: Instant,
    audio_master: bool,
    last_drift_log: Option<Instant>,
    last_stall_log: Option<Instant>,
    /// Last periodic cache throughput line (5 s cadence).
    last_cache_rate_log: Option<Instant>,
    /// Last hard A/V resync; throttled so a slow decoder cannot thrash seeks.
    last_av_resync: Option<Instant>,
    /// The picture first went (and stayed) more than `AV_VIDEO_LAG_MS`
    /// behind the sound while frames were still arriving. Turns normal
    /// decode jitter into a catch-up only after it persists.
    video_lag_since: Option<Instant>,
    /// In-flight video-only seek that skips a stretch the decoder could not
    /// produce in real time. The sound is paused for the short seek so both
    /// resume together at the new picture.
    video_resync: Option<VideoResync>,
    /// Sound paused because the picture ran dry; tracks whether a frame has
    /// come back so the catch-up grace starts from that moment.
    stall_pause: Option<StallPause>,
    ended: bool,
    last_frame_ts: u64,
    /// A non-modal window move/resize is in progress. The engine keeps the
    /// video presenting and defers chrome, shadow and overlay layout until
    /// the gesture ends.
    interacting: bool,
    /// Whether the active gesture changes the client size. A move only moves
    /// the composition; a resize also scales the frozen danmaku/UI layers so
    /// they follow the window edge.
    interaction_resizing: bool,
    /// Client size at the start of the gesture. The overlay visuals are scaled
    /// from this size to the current one during a resize, then reset on settle.
    interaction_start_client: (u32, u32),
    /// Latest client size from WM_SIZE during a resize; applied once per
    /// frame instead of once per Win32 message.
    pending_live_resize: Option<(u32, u32)>,

    // Audio/UI settings.
    volume: u8,
    muted: bool,
    /// End-of-video behaviour: 0 pause, 1 loop, 2 next / close.
    loop_mode: u8,
    speed: f32,
    interp: InterpMode,
    enhance: EnhanceMode,
    /// Requested interpolation grid: 0 follows the display refresh, otherwise
    /// a fixed target in Hz (bounded by `INTERP_MAX_OUT_HZ`).
    interp_hz: u32,
    interp_out_hz: Option<u32>,
    /// Refresh rate of the monitor under the player window, refreshed once a
    /// second. The 插帧 menu and the danmaku cadence follow this monitor, not
    /// the primary display.
    monitor_hz: u32,
    monitor_check_at: Instant,
    stats: bool,
    aspect: AspectMode,
    ontop: bool,
    brightness: f32,
    audio_only: bool,
    light_sidebar: bool,
    player_keys: Vec<u32>,
    player_keys_hint: String,
    sleep_deadline: Option<Instant>,
    sleep_choice: u32,

    // Display state. Pictures live in the video surface's GPU frame ring;
    // only the slot/timestamp/signature travel through the engine.
    cur: Option<GpuFrame>,
    cur_ts_ms: u64,
    next: Option<GpuFrame>,
    last_source_present: Option<Instant>,
    next_tick_at: Option<Instant>,
    out_period: Duration,

    // Counters.
    presents_window: u64,
    fps: f32,
    fps_window_start: Instant,
    /// Danmaku layer presents per second, sampled over the same 1 s window as
    /// the video FPS. The layer is independent of the video grid.
    danmaku_fps: f32,
    danmaku_frames_prev: u64,
    /// Successful overlay presents per second (toolbar, sidebar, HUD and all
    /// animations share this counter).
    ui_fps: f32,
    ui_presents_prev: u64,
    /// Average overlay draw cost in the current 1 s window (diagnostic).
    ui_draw_us_total: u64,
    ui_draw_count: u64,
    generated: u64,
    blend_us_total: u64,
    blend_count: u64,
    last_blend_us: u32,
    interp_ticks: u32,
    interp_blends: u32,
    interp_cuts: u32,

    // UI.
    ui_dirty: bool,
    /// Absolute deadline for the next regular overlay frame. Using a fixed
    /// 120 Hz grid (monitor refresh) keeps loop overhead and requestAnimation
    /// style jitter from stretching the interval to ~9.5 ms.
    next_ui_draw: Instant,
    status: String,
    drag_preview: Option<u64>,
    dragging: bool,
    /// Right-click precision seek panel (heatmap + A/B loop + frame wheel).
    precision_open: bool,
    precision_dragging: bool,
    /// 0 = toolbar, 1 = precision panel. A 360 ms time-based morph so the
    /// paper card visibly grows into the panel and back instead of popping.
    precision_anim: f32,
    /// Value and start time of the current morph. Retoggling mid-flight
    /// reverses from wherever the card currently is.
    precision_morph_from: f32,
    precision_morph_start: Instant,
    /// Frame-step dropdown in the precision panel.
    precision_step_open: bool,
    /// Speed panel (the second bottom-panel mode) and its live drag value.
    speed_open: bool,
    /// Left button is down on the speed wheel: press x and the stop it
    /// started from. The wheel is relative, like the frame wheel.
    speed_drag: Option<(f32, f32)>,
    /// Value shown while the wheel is dragged or still turning. It becomes
    /// the real playback speed on release / after the wheel idle.
    speed_preview: f32,
    /// Stop shown in the selection window while the pointer drags. It equals
    /// `speed_preview` at rest and freezes at the drag's start stop so the
    /// labels scroll with the finger.
    speed_base: f32,
    /// Debounce for wheel notches: one session restart after the wheel stops.
    speed_commit_at: Option<Instant>,
    /// Which bottom panel the morph card is showing: 0 toolbar, 1 precision,
    /// 2 speed. Sticky through the close animation so the content can fade.
    panel: u8,
    /// Wheel volume accumulator in percent. A standard notch is 120 units and
    /// moves 5%; precision touchpads send much smaller deltas, so they build up
    /// here instead of being rounded away to nothing.
    wheel_volume_accum: f32,
    /// Last wheel direction (-1 / 0 / 1); a reversal drops the stale remainder
    /// so the first notch of the new direction responds immediately.
    wheel_volume_dir: f32,
    /// Keep the right-edge volume bar on screen for a moment after a notch.
    volume_wheel_until: Option<Instant>,
    /// Horizontal drag on the frame ruler: (start_x, start_ms). The preview
    /// follows the total displacement and the real seek happens once on release.
    tuner_drag: Option<(f32, u64)>,
    /// Visual ruler displacement from the wheel centre, in pixels. It follows
    /// the finger without a cap for the whole drag and returns to zero after
    /// release; the draw pass derives the visible tick range from this value.
    tuner_offset: f32,
    /// One-shot 0..1 pulse for a wheel notch. The bead leaves the centre, peaks
    /// and comes back, so the wheel never keeps a permanent offset.
    tuner_pulse: f32,
    tuner_pulse_dir: f32,
    ab_a: Option<u64>,
    ab_b: Option<u64>,
    /// At B with a prefetch decoder still warming: hold the last picture for a
    /// short grace period instead of tearing down the decoder and reloading.
    ab_swap_wait: Option<(u64, Instant)>,
    /// Decoded A–B pictures captured on the first pass. When ready, every
    /// following pass replays from memory without opening a decoder.
    ab_cache: Option<AbCache>,
    ab_cache_playing: bool,
    ab_cache_cursor: usize,
    frame_step: u64,
    heatmap: [u8; HEAT_BUCKETS],
    /// Left press on the picture: a small move drags the frameless window.
    /// A clean release has no transport action; spacebar owns play/pause.
    window_drag_arm: Option<(i32, i32)>,
    /// Left press on the sidebar background: dragging moves the whole window,
    /// so the lane travels with the picture instead of feeling detached. A
    /// clean click never toggles playback and never closes the sidebar.
    sidebar_drag_arm: Option<(i32, i32)>,
    popup_drag: Option<u8>,
    /// End-of-video menu opened from the loop cell.
    loop_menu_open: bool,
    mini: bool,
    mini_saved: Option<MiniSaved>,
    mini_hint_until: Option<Instant>,
    quit: bool,

    // Custom chrome / right strip.
    chrome_shown: bool,
    last_mouse_activity: Instant,
    pointer: Option<(f32, f32)>,
    sidebar_open: bool,
    /// Windowed mode already grew the outer window by the sidebar lane.
    sidebar_extra: bool,
    sidebar_tab: SidebarTab,
    sidebar_scroll: f32,
    sidebar_scroll_target: f32,
    /// Cached height of the info description; `-1` means measure again.
    info_scroll_height: f32,
    info_scroll_width: f32,
    /// The queue prefetcher runs once per player window; it waits for the
    /// active entry's own cache and then fills the following entries.
    playlist_prefetch_started: bool,
    /// Page index whose media is already being warmed for the auto "next
    /// part" transition. Prevents starting a second resolver/worker pool for
    /// the same page while the current video is in its last ten seconds.
    next_page_prefetch: Option<usize>,
    danmaku_popup: bool,
    /// 发送弹幕 input and its in-flight post.
    dm_input: String,
    dm_input_focus: bool,
    dm_send_rx: Option<Receiver<(i64, String)>>,
    info: Option<InfoUi>,
    pages: Vec<PageUi>,
    current_page: usize,
    comments: Vec<CommentUi>,
    comments_total: i64,
    comments_hot: bool,
    comments_loading: bool,
    comments_end: bool,
    comments_cursor: String,
    /// 0 = 最热, 1 = 最新, eased every loop so the sort pill glides.
    comments_sort_mix: f32,
    /// Fade/slide entrance for a freshly replaced comment page.
    comments_enter: f32,
    /// Invalidates the overlay's measured comment rows.
    comments_generation: u64,
    /// The sort was changed while the previous page was in flight; reload the
    /// new sort as soon as that response arrives.
    comments_reload_pending: bool,
    /// Last update of the comment easing clocks.
    comments_anim_last: Instant,
    detail_tx: Sender<DetailRequest>,
    detail_rx: Receiver<DetailEvent>,
    avatars: std::collections::HashMap<String, ImageData>,
    switch_rx: Option<Receiver<Result<MediaSource, String>>>,
    switch_target: Option<usize>,
    /// A fully resolved next-part source whose cache workers are already
    /// running. `switch_page` consumes it instead of resolving playurl again.
    next_page_source: Arc<Mutex<Option<(usize, MediaSource)>>>,
    /// When the pending 分P resolve started, so a hung playurl cannot leave
    /// the window on the loading card forever.
    switch_started: Option<Instant>,
    /// Page the selection was on before an optimistic 分P switch, and the
    /// playback position to restore if playurl fails.
    switch_from_page: Option<usize>,
    switch_resume_ms: u64,
    /// Page being resolved/opened right now. The loading card names it and the
    /// selection moves on click, before playurl answers.
    page_loading: Option<usize>,
    track_rx: Option<Receiver<Result<MediaSource, String>>>,
    track_kind: Option<&'static str>,
    settings_menu: Option<SettingsMenu>,
    /// Persistent opening quality (height, 0 = highest available).
    quality_height_pref: u32,
    liked: bool,
    coined: bool,
    favorited: bool,
    toast: Option<ToastUi>,
    toast_until: Option<Instant>,
    queue_len: usize,
    dropped: u64,
    audio_ms: u64,
    /// Live ffmpeg child read rate for the debug HUD (bytes per second).
    net_pid: u32,
    net_read: u64,
    net_at: Instant,
    net_bps: f64,
    /// Read baseline when the current decoder PID started, used to estimate
    /// the downloaded lead in seconds.
    net_base_read: u64,
    net_base_pos: u64,
    /// Global cache download counter at the previous one-second sample.
    cache_net_read: u64,
    net_lead_s: f64,
    /// Media cache progress for the current source (0 for direct/local).
    cache_done: u64,
    cache_total: u64,
    // History heartbeat.
    aid: u64,
    heartbeat: Option<Heartbeat>,
    heartbeat_started: bool,
    heartbeat_accum: Duration,
    heartbeat_anchor: Option<Instant>,
    last_heartbeat: Instant,
    // Progress-bar storyboard.
    storyboard_tx: Option<Sender<storyboard::Request>>,
    storyboard_rx: Option<Receiver<storyboard::Event>>,
    storyboard_meta: Option<Arc<storyboard::Meta>>,
    storyboard_cell: Option<(usize, ImageData)>,
    storyboard_pending: Option<usize>,
    storyboard_failed: bool,
    hover_preview: Option<u64>,
    hover_pinned: bool,
    // Health / recovery.
    had_first_frame: bool,
    stalled: bool,
    stall_until: Option<Instant>,
    /// Audio was paused because the picture ran dry; resume it on the next
    /// presented frame. Keeping the device clock stopped is what preserves
    /// lip-sync when the network cannot feed the video cache fast enough.
    stall_audio_paused: bool,
    error: Option<String>,
    danmaku_opacity: u32,
    danmaku_font: u32,
    danmaku_speed: u32,
    danmaku_area: u32,
    block_scroll: bool,
    block_top: bool,
    block_bottom: bool,
    density_on: bool,

    // Automation.
    qa: Vec<QaAction>,
    qa_index: usize,
    auto_exit: Option<Duration>,
    started_at: Instant,
}

struct QaAction {
    at: Duration,
    kind: QaKind,
}

/// One video-only catch-up: a new decoder opened at the sound position while
/// the sound clock is frozen.
struct VideoResync {
    target: u64,
    started: Instant,
}

/// Sound paused because the picture ran dry. The first picture after the
/// pause starts a grace period for the decoder to reach the frozen clock.
struct StallPause {
    first_frame_at: Option<Instant>,
    /// When the pause started. A buffer wait on a cache chunk that never
    /// completes must not hold the sound forever; the proxy path can still
    /// feed the ring after the cache gave up.
    since: Instant,
}

/// A picture that stays more than `AV_VIDEO_LAG_MS` behind the sound while
/// frames keep arriving is a decode deficit, not jitter: the decoder cannot
/// play the medium at normal speed, so the un-decodable gap has to be
/// skipped. A short GPU/engine hitch clears before the hold expires.
fn picture_lag_requires_catch_up(
    lag_ms: u64,
    frames_flowing: bool,
    held_for: Duration,
) -> bool {
    frames_flowing
        && lag_ms > AV_VIDEO_LAG_MS
        && held_for >= Duration::from_millis(AV_VIDEO_LAG_HOLD_MS)
}

/// Buffer guard threshold. An empty ring for 200 ms is already a starved
/// reader; two frames may sit for 400 ms while the presenter still has
/// something to show. Anything deeper is jitter, not a stall.
fn buffer_is_low(queue_len: usize, no_picture_ms: u64) -> bool {
    (queue_len == 0 && no_picture_ms >= 200)
        || (queue_len <= BUFFER_LOW_FRAMES && no_picture_ms >= BUFFER_LOW_MS)
}

/// The auto "next part" transition gets a head start when the current part is
/// in its last ten seconds. Single/V loop modes and the final part do not.
fn next_page_prefetch_due(
    loop_mode: u8,
    position_ms: u64,
    duration_ms: u64,
    has_next: bool,
    already_prefetching: bool,
) -> bool {
    loop_mode == o11a1::settings::LOOP_NEXT_PART
        && has_next
        && !already_prefetching
        && duration_ms > 0
        && position_ms.saturating_add(NEXT_PAGE_PREFETCH_MS) >= duration_ms
}

struct MiniSaved {
    window: RECT,
    client: (u32, u32),
}

/// One in-flight 点赞/举报 network result.
struct DmActionResult {
    kind: &'static str,
    dmid: String,
    /// API code, or -1 when the request itself failed (no session/runtime).
    code: i64,
    message: String,
}

struct UndoBlock {
    mid_hash: String,
    until: Instant,
}

struct HistoryPayload {
    date: String,
    items: Vec<DanmakuItem>,
    source: &'static str,
}

enum QaKind {
    Seek,
    SeekEnd,
    Pause,
    Play,
    Interp,
    Enhance,
    Anime,
    Shot,
    Stats,
    Mute,
    /// QA only: set a mid-level volume and keep the wheel OSD up long enough
    /// for a screenshot.
    Volume,
    Fullscreen,
    Danmaku,
    DanmakuOff,
    DanmakuToggle,
    Mini,
    Aspect,
    OnTop,
    Bright,
    AudioOnly,
    SpeedUp,
    SpeedDown,
    SpeedPanel,
    Sidebar,
    SidebarInfo,
    InfoCopy,
    SidebarEpisodes,
    SidebarComments,
    SidebarPerformance,
    SidebarSettings,
    DanmakuPopup,
    DanmakuLive,
    DmMenu,
    DmHold,
    DmLike,
    DmReport,
    DmReportOne,
    DmBlock,
    DmCopy,
    DmUndo,
    DmHistory,
    DmHistoryDate,
    DmHistoryClose,
    Chrome,
    CommentsTime,
    Like,
    Coin,
    Fav,
    Page2,
    Quality,
    QualityLow,
    QualityNext,
    QualityLocked,
    AudioMenu,
    AudioNext,
    CommentsMore,
    Hover,
    Hover2,
    Sleep,
    SleepShort,
    Precision,
    FrameStep,
    AbA,
    AbB,
    AbClear,
    LoopMenu,
    LoopPause,
    LoopOne,
    LoopNext,
    LoopPart,
    InterpMenu,
    EnhanceMenu,
    Fault,
    Retry,
    Stall,
}

impl Engine {
    fn new(
        config: PlayerConfig,
        source: MediaSource,
        shared: Arc<SharedState>,
        commands: Receiver<Command>,
        danmaku_rx: Receiver<Vec<DanmakuItem>>,
        danmaku_tx: Sender<Vec<DanmakuItem>>,
        detail_tx: Sender<DetailRequest>,
        detail_rx: Receiver<DetailEvent>,
        session: Option<Arc<o11a1::bilibili::Session>>,
        mut state: PlayerState,
        source_rx: Receiver<std::result::Result<crate::DeferredOpen, String>>,
        source_tx: Sender<std::result::Result<crate::DeferredOpen, String>>,
    ) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let _ = timeBeginPeriod(1);
        }
        // The placeholder source has no URL: the window opens on the loading
        // card while a worker resolves the real media.
        let loading = source.is_placeholder();
        let requested_start_ms = config.start_ms;
        let host = config.host.clone();
        let title = format!("O11A1 · {}", source.title);
        let external = host.is_some();
        let is_live = config.is_live;
        if is_live {
            log::line("live mode: interpolation and enhancement disabled");
        }
        let (hwnd, events) = match &host {
            Some(host) => window::create_hosted(
                HWND(host.parent as *mut _),
                host.x,
                host.y,
                host.width,
                host.height,
            )?,
            None => {
                let (client_w, client_h) = window_size(&state);
                window::create(&title, client_w, client_h)?
            }
        };
        // Shift-resize needs the source aspect inside the window proc.
        events.set_video_aspect(source.source_width, source.source_height);
        // Apply 置顶 before the composition tree is built; the state is also
        // read back so the UI reflects the measured window style, not a wish.
        if state.topmost {
            if let Err(error) = window::set_topmost(hwnd, true) {
                log::line(&format!("player state topmost failed: {error:#}"));
            }
        }
        // The picture is locked to its source aspect; window mismatch is
        // filled with the window background instead of stretching.
        let initial_aspect = AspectMode::Fit;
        let client = window::client_size(hwnd);
        let scale = window::dpi_scale(hwnd);
        // Allocate the composition layers to the whole monitor. The sidebar
        // lane is then already inside a live swapchain, so opening it only
        // changes the layout and the window rectangle, never the buffers.
        let monitor = window::monitor_size(hwnd);
        let surface = (monitor.0.max(client.0), monitor.1.max(client.1));
        let gpu = Gpu::new().context("create D3D11 device")?;
        let video = VideoSurface::new(&gpu, source.width, source.height)
            .context("create video swapchain")?;
        let mut overlay = Overlay::new(&gpu, client.0, client.1, surface.0, surface.1, scale)
            .context("create overlay swapchain")?;
        if external {
            overlay.clear().context("clear hosted overlay")?;
        }
        let danmaku_settings = DanmakuSettings::load(client.1);
        let danmaku_fonts = danmaku_settings.font_sizes;
        let danmaku_opacity = danmaku_settings.opacity;
        let mut danmaku_layer =
            DanmakuOverlay::new(&gpu, surface.0, surface.1, danmaku_fonts, danmaku_opacity)
                .context("create danmaku swapchain")?;
        let composition =
            Composition::new(hwnd, &gpu, &video.swap, &danmaku_layer.swap, &overlay.swap)
                .context("create DirectComposition tree")?;
        composition.set_root_clip(client.0, client.1)?;
        let (x, y, picture_w, picture_h) =
            overlay
                .layout
                .stage_video_bounds(source.width, source.height, initial_aspect);
        let sx = picture_w / source.width.max(2) as f32;
        let sy = picture_h / source.height.max(2) as f32;
        composition.set_video_transform(x, y, sx, sy)?;
        let band = danmaku_band(&overlay.layout, &source, false, initial_aspect);
        let danmaku = DanmakuModel::new(danmaku_settings, band);
        let danmaku_env = std::env::var("BILI_PLAYER_DANMAKU").ok().map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            )
        });
        let danmaku_visible = danmaku_env.unwrap_or(state.danmaku_visible);

        let enhance_pipeline =
            match EnhancePipeline::new(&gpu.device, &gpu.context, source.width, source.height) {
                Ok(pipeline) => Some(pipeline),
                Err(error) => {
                    log::line(&format!("enhance fallback at startup reason={error:#}"));
                    None
                }
            };

        let duration_ms = source.duration_ms;
        let start_ms = clamp_seek(requested_start_ms, duration_ms);
        // The saved interpolation rate is validated against the monitor the
        // window actually opened on. Querying a NULL device fails on some
        // multi-monitor drivers and silently reports 60 Hz.
        let display_hz = interp::display_refresh_hz_for_window(hwnd);
        let interp_out_hz = interp::interp_plan(display_hz, source.fps, 0);
        let out_hz = interp_out_hz.unwrap_or_else(|| {
            if source.fps.is_finite() && source.fps > 1.0 {
                source.fps.round().clamp(1.0, 240.0) as u32
            } else {
                60
            }
        });
        log::line(&format!(
            "interp plan source_fps={:.3} display_hz={display_hz} out_hz={} cpu={}",
            source.fps,
            out_hz,
            interp_out_hz.is_some() as u8
        ));
        let volume_env = std::env::var("BILI_PLAYER_VOLUME")
            .ok()
            .and_then(|value| value.parse::<u8>().ok());
        let volume = volume_env.unwrap_or(state.volume).min(100);
        let muted = volume_env.is_none() && state.muted;
        let speed = crate::speed::clamp(state.speed);
        state.volume = volume;
        state.muted = muted;
        state.speed = speed;
        state.aspect = AspectMode::Fit.as_u8();
        let initial_stats = state.stats;
        let stored_settings = o11a1::settings::load();
        let source_title = source.title.clone();
        let source_id = source.bvid.clone().unwrap_or_default();
        let light_sidebar = !o11a1::theme::resolve(stored_settings.theme);
        let player_keys =
            if stored_settings.player_keys.len() == o11a1::settings::PLAYER_KEY_ACTIONS {
                stored_settings.player_keys.clone()
            } else {
                o11a1::settings::default_player_keys()
            };
        let player_keys_hint = player_keys_summary(&player_keys);
        log::line(&format!(
            "theme mode={} light_sidebar={}",
            stored_settings.theme,
            u8::from(light_sidebar)
        ));
        log::line(&format!(
            "player keys source=settings actions={} keys={}",
            player_keys.len(),
            player_keys_hint
        ));
        let heartbeat = match (&session, &source.bvid, source.cid) {
            (Some(session), Some(bvid), Some(cid)) => {
                Some(Heartbeat::spawn(session.clone(), bvid.clone(), cid))
            }
            _ => None,
        };
        let qa = parse_qa(duration_ms);
        let auto_exit = std::env::var("BILI_PLAYER_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);
        let (dm_action_tx, dm_action_rx) = mpsc::channel();
        let initial_brightness = state.brightness;
        let initial_audio_only = state.audio_only;

        let mut engine = Self {
            source,
            open_config: config,
            loading,
            source_pending: loading,
            pending_replace: None,
            queued_replace: None,
            failed_replace: None,
            source_rx,
            source_tx,
            requested_start_ms,
            session,
            shared,
            commands,
            hwnd,
            events,
            external,
            is_live,
            external_preview_visible: false,
            external_press: None,
            gpu,
            video,
            overlay,
            composition,
            danmaku_layer,
            danmaku,
            next_danmaku_draw: Instant::now(),
            danmaku_rx,
            danmaku_tx,
            danmaku_visible,
            danmaku_band: band,
            live_danmaku: Vec::new(),
            dm_menu: None,
            dm_menu_drag: None,
            dm_long: None,
            dm_thumb_inflight: HashSet::new(),
            dm_action_tx: dm_action_tx.clone(),
            dm_action_rx,
            undo: None,
            history_dates: Vec::new(),
            history_dates_loading: false,
            history_dates_error: None,
            history_dates_rx: None,
            history_date: None,
            history_items: Vec::new(),
            history_loading: false,
            history_error: None,
            history_rx: None,
            qa_history_pick: None,
            state,
            state_save: SaveDebouncer::default(),
            last_watch_save: Instant::now(),
            enhance_pipeline,
            decoder: None,
            loop_prefetch: None,
            pending_wheel_seek: None,
            audio: None,
            client,
            scale,
            fullscreen: false,
            saved_window: None,
            duration_ms,
            position_ms: start_ms,
            base_ms: start_ms,
            anchor: None,
            paused_at: start_ms,
            playing: false,
            requested_play: true,
            starting: true,
            first_frame_at: None,
            seek_in_flight: false,
            seek_target: start_ms,
            stream_start_ms: start_ms,
            generation: 1,
            session_started: Instant::now(),
            audio_master: false,
            last_drift_log: None,
            last_stall_log: None,
            last_cache_rate_log: None,
            last_av_resync: None,
            video_lag_since: None,
            video_resync: None,
            stall_pause: None,
            ended: false,
            last_frame_ts: start_ms,
            interacting: false,
            interaction_resizing: false,
            interaction_start_client: (0, 0),
            pending_live_resize: None,
            volume,
            muted,
            loop_mode: stored_settings.loop_mode,
            speed,
            interp: InterpMode::Off,
            enhance: EnhanceMode::Off,
            interp_hz: 0,
            interp_out_hz,
            monitor_hz: display_hz,
            monitor_check_at: Instant::now(),
            stats: initial_stats,
            aspect: AspectMode::Fit,
            ontop: window::is_topmost(hwnd),
            brightness: initial_brightness,
            audio_only: initial_audio_only,
            light_sidebar,
            player_keys,
            player_keys_hint,
            sleep_deadline: None,
            sleep_choice: 0,
            cur: None,
            cur_ts_ms: 0,
            next: None,
            last_source_present: None,
            next_tick_at: None,
            out_period: Duration::from_secs_f64(1.0 / out_hz.max(1) as f64),
            presents_window: 0,
            fps: 0.0,
            fps_window_start: Instant::now(),
            danmaku_fps: 0.0,
            danmaku_frames_prev: 0,
            ui_fps: 0.0,
            ui_presents_prev: 0,
            ui_draw_us_total: 0,
            ui_draw_count: 0,
            generated: 0,
            blend_us_total: 0,
            blend_count: 0,
            last_blend_us: 0,
            interp_ticks: 0,
            interp_blends: 0,
            interp_cuts: 0,
            ui_dirty: true,
            next_ui_draw: Instant::now(),
            status: if loading {
                "正在打开视频…".to_string()
            } else {
                "正在加载…".to_string()
            },
            drag_preview: None,
            dragging: false,
            precision_open: false,
            precision_dragging: false,
            precision_anim: 0.0,
            precision_morph_from: 0.0,
            precision_morph_start: Instant::now(),
            precision_step_open: false,
            speed_open: false,
            speed_drag: None,
            speed_preview: crate::speed::snap(speed),
            speed_base: crate::speed::snap(speed),
            speed_commit_at: None,
            panel: 0,
            wheel_volume_accum: 0.0,
            wheel_volume_dir: 0.0,
            volume_wheel_until: None,
            tuner_drag: None,
            tuner_offset: 0.0,
            tuner_pulse: 0.0,
            tuner_pulse_dir: 0.0,
            ab_a: None,
            ab_b: None,
            ab_swap_wait: None,
            ab_cache: None,
            ab_cache_playing: false,
            ab_cache_cursor: 0,
            frame_step: 1,
            heatmap: [0; HEAT_BUCKETS],
            window_drag_arm: None,
            sidebar_drag_arm: None,
            popup_drag: None,
            loop_menu_open: false,
            mini: false,
            mini_saved: None,
            mini_hint_until: None,
            quit: false,
            chrome_shown: true,
            last_mouse_activity: Instant::now(),
            pointer: None,
            sidebar_open: false,
            sidebar_extra: false,
            sidebar_tab: SidebarTab::Info,
            sidebar_scroll: 0.0,
            sidebar_scroll_target: 0.0,
            info_scroll_height: -1.0,
            info_scroll_width: 0.0,
            playlist_prefetch_started: false,
            next_page_prefetch: None,
            danmaku_popup: false,
            dm_input: String::new(),
            dm_input_focus: false,
            dm_send_rx: None,
            info: Some(InfoUi {
                title: source_title.clone(),
                video_id: source_id,
                owner: String::new(),
                views: 0,
                pubdate: 0,
                cover: None,
                description: String::new(),
                pages: Vec::new(),
                current_page: 0,
            }),
            pages: Vec::new(),
            current_page: 0,
            comments: Vec::new(),
            comments_total: 0,
            comments_hot: true,
            comments_loading: false,
            comments_end: false,
            comments_cursor: String::new(),
            comments_sort_mix: 0.0,
            comments_enter: 1.0,
            comments_generation: 0,
            comments_reload_pending: false,
            comments_anim_last: Instant::now(),
            detail_tx,
            detail_rx,
            avatars: std::collections::HashMap::new(),
            switch_rx: None,
            switch_target: None,
            next_page_source: Arc::new(Mutex::new(None)),
            switch_started: None,
            switch_from_page: None,
            switch_resume_ms: 0,
            page_loading: None,
            track_rx: None,
            track_kind: None,
            settings_menu: None,
            quality_height_pref: stored_settings.player_quality_height,
            liked: false,
            coined: false,
            favorited: false,
            toast: None,
            toast_until: None,
            queue_len: 0,
            dropped: 0,
            audio_ms: 0,
            net_pid: 0,
            net_read: 0,
            net_at: Instant::now(),
            net_bps: 0.0,
            net_base_read: 0,
            net_base_pos: 0,
            cache_net_read: 0,
            net_lead_s: 0.0,
            cache_done: 0,
            cache_total: 0,
            aid: 0,
            heartbeat,
            heartbeat_started: false,
            heartbeat_accum: Duration::ZERO,
            heartbeat_anchor: None,
            last_heartbeat: Instant::now(),
            storyboard_tx: None,
            storyboard_rx: None,
            storyboard_meta: None,
            storyboard_cell: None,
            storyboard_pending: None,
            storyboard_failed: false,
            hover_preview: None,
            hover_pinned: false,
            had_first_frame: false,
            stalled: false,
            stall_until: None,
            stall_audio_paused: false,
            error: None,
            danmaku_opacity: stored_settings.danmaku_opacity.min(100),
            danmaku_font: o11a1::settings::normalise_danmaku_font(stored_settings.danmaku_font),
            danmaku_speed: stored_settings.danmaku_speed.clamp(50, 200),
            danmaku_area: o11a1::settings::clamp_area(stored_settings.danmaku_area),
            block_scroll: stored_settings.block_scroll,
            block_top: stored_settings.block_top,
            block_bottom: stored_settings.block_bottom,
            density_on: true,
            qa,
            qa_index: 0,
            auto_exit,
            started_at: Instant::now(),
        };
        engine.sync_shared();
        log::line(&format!(
            "player state applied path={} volume={} muted={} speed={:.2} aspect={} brightness={:.2} audio_only={} ontop={} danmaku={} qn={:?} codecid={:?} audio_index={:?}",
            crate::state::state_path().display(),
            engine.volume,
            u8::from(engine.muted),
            engine.speed,
            engine.aspect.as_u8(),
            engine.brightness,
            u8::from(engine.audio_only),
            u8::from(engine.ontop),
            u8::from(engine.danmaku_visible),
            engine.state.quality_qn,
            engine.state.quality_codecid,
            engine.state.audio_index
        ));
        let _ = engine.detail_tx.send(DetailRequest::Info);
        log::line("detail info requested");
        log::line(&format!(
            "danmaku visible={} layer=ready",
            u8::from(engine.danmaku_visible)
        ));
        // The comment layer follows the display, not the 60 fps video grid.
        // 120 Hz panels therefore get 120 Hz scrolling comments.
        log::line(&format!(
            "danmaku cadence hz={} independent=1 monitor={}hz",
            engine.monitor_hz.clamp(30, 120),
            engine.monitor_hz
        ));
        if loading {
            log::line("player window ready; waiting for the resolved source");
        } else {
            engine.open_sessions(start_ms, false)?;
            engine.start_playlist_prefetch();
        }
        let start_mini = std::env::var("BILI_PLAYER_MINI")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "on" | "true" | "yes"
                )
            })
            .unwrap_or(false);
        if start_mini {
            engine.set_mini(true);
        }
        engine.draw_ui()?;
        // The first present on a freshly created composition tree can still be
        // a partially rasterised surface (toolbar only, no loading card). Let
        // it settle, draw a second complete frame, and only then reveal the
        // window so no empty frame flashes between the click and the player.
        std::thread::sleep(std::time::Duration::from_millis(32));
        engine.draw_ui()?;
        if !external {
            window::show_player(engine.hwnd);
        }
        Ok(engine)
    }

    // -------------------------------------------------------------- sessions

    fn video_spec(&self) -> VideoSpec {
        VideoSpec {
            url: self.source.video_url.clone(),
            headers: self.source.headers.clone(),
            remote: self.source.remote,
            source_width: self.source.source_width,
            source_height: self.source.source_height,
            width: self.source.width,
            height: self.source.height,
            fps: self.source.fps,
            video_bitrate: self.source.video_bitrate,
            speed: self.speed as f64,
            live: self.is_live,
            cache_key: if self.is_live {
                None
            } else {
                crate::source::media_cache_key(&self.source, false)
            },
            hw_decoder: self.source.hw_decoder.clone(),
        }
    }

    fn spawn_audio_at(&mut self, start_ms: u64) {
        let audio_url = self
            .source
            .audio_url
            .clone()
            .unwrap_or_else(|| self.source.video_url.clone());
        // A video-only stream (no DASH audio) still gets a session so commands
        // stay uniform; ffmpeg ends immediately and the clock simply never
        // becomes ready.
        let audio = AudioEngine::spawn(
            audio_url,
            self.source.headers.clone(),
            self.source.remote,
            start_ms,
            self.speed as f64,
            if self.muted { 0 } else { self.volume },
            if self.is_live || self.source.audio_url.is_none() {
                None
            } else {
                crate::source::media_cache_key(&self.source, true)
            },
        );
        // Starts paused until the picture's first frame is ready; the
        // seek/start state machine resumes both together.
        audio.send(AudioCommand::Pause);
        self.audio = Some(audio);
    }

    /// Fill the disk cache for the entries after the current one. The active
    /// entry's cache is drained first, so queue prefetch never competes with
    /// the picture the user is watching.
    /// Warm the next part's video/audio cache while the current part is still
    /// in its last ten seconds. The auto "next part" transition then opens on
    /// a cache that already holds the next page's opening chunk instead of
    /// starting the resolver and first range request on the end card.
    fn maybe_prefetch_next_page(&mut self, position: u64) {
        if self.is_live || self.switch_rx.is_some() {
            return;
        }
        let next_index = self.current_page + 1;
        let has_next = self.pages.get(next_index).is_some();
        if !next_page_prefetch_due(
            self.loop_mode,
            position,
            self.duration_ms,
            has_next,
            self.next_page_prefetch == Some(next_index),
        ) {
            return;
        }
        let Some(page) = self.pages.get(next_index) else {
            return;
        };
        if self.next_page_prefetch == Some(next_index) {
            return;
        }
        let Some(bvid) = self
            .source
            .bvid
            .clone()
            .filter(|value| !value.trim().is_empty())
        else {
            return;
        };
        let cid = page.cid;
        let title = page.title.clone();
        let quality = self.source.qn;
        let codecid = self.state.quality_codecid.unwrap_or(0);
        let audio_index = self.source.audio_index;
        self.next_page_prefetch = Some(next_index);
        log::line(&format!(
            "next page prefetch start from_page={} to_page={} bvid={bvid} cid={cid}",
            self.current_page + 1,
            next_index + 1
        ));
        let source_slot = self.next_page_source.clone();
        let spawned = std::thread::Builder::new()
            .name("next-page-prefetch".into())
            .spawn(move || {
                let config = PlayerConfig {
                    bvid: Some(bvid.clone()),
                    cid: Some(cid),
                    quality_qn: Some(quality),
                    quality_codecid: Some(codecid),
                    audio_index: Some(audio_index),
                    title,
                    ..Default::default()
                };
                match crate::source::resolve(&config) {
                    Ok(resolved) => {
                        if let Ok(mut slot) = source_slot.lock() {
                            *slot = Some((next_index, resolved.source.clone()));
                        }
                        let ok = crate::mediacache::warm_source(&resolved.source);
                        log::line(&format!(
                            "next page prefetch bvid={bvid} cid={cid} ok={}",
                            u8::from(ok)
                        ));
                    }
                    Err(error) => log::line(&format!(
                        "next page prefetch resolve failed bvid={bvid} cid={cid} err={error:#}"
                    )),
                }
            });
        if let Err(error) = spawned {
            self.next_page_prefetch = None;
            log::line(&format!("next page prefetch spawn failed: {error:#}"));
        }
    }

    fn start_playlist_prefetch(&mut self) {        log::line(&format!(
            "playlist prefetch check started={} live={} bvid={:?}",
            u8::from(self.playlist_prefetch_started),
            u8::from(self.is_live),
            self.source.bvid
        ));
        if self.playlist_prefetch_started || self.is_live {
            return;
        }
        let Some(current) = self
            .source
            .bvid
            .clone()
            .filter(|value| !value.trim().is_empty())
        else {
            log::line("playlist prefetch skipped: no current bvid");
            return;
        };
        let settings = o11a1::settings::load();
        let Some(index) = settings
            .playlist
            .iter()
            .position(|entry| entry.bvid == current)
        else {
            log::line("playlist prefetch skipped: current not in queue");
            return;
        };
        let entries: Vec<(String, u64)> = settings
            .playlist
            .iter()
            .skip(index + 1)
            .filter(|entry| !entry.bvid.trim().is_empty())
            .map(|entry| (entry.bvid.clone(), entry.cid))
            .collect();
        if entries.is_empty() {
            log::line("playlist prefetch skipped: no entries after current");
            return;
        }
        log::line(&format!(
            "playlist prefetch queued current_index={} entries={}",
            index,
            entries.len()
        ));
        self.playlist_prefetch_started = true;
        let quality_height = settings.player_quality_height;
        let codec = if settings.codec_pref > 0 {
            Some(settings.codec_pref as i64)
        } else {
            None
        };
        let active_video = crate::source::media_cache_key(&self.source, false);
        let active_audio = crate::source::media_cache_key(&self.source, true);
        let _ = std::thread::Builder::new()
            .name("playlist-prefetch".into())
            .spawn(move || {
                if let Some(key) = active_video {
                    crate::mediacache::wait_for_key(&key);
                }
                if let Some(key) = active_audio {
                    crate::mediacache::wait_for_key(&key);
                }
                log::line("playlist prefetch active-cache ready");
                for (bvid, cid) in entries {
                    log::line(&format!("playlist prefetch resolve start bvid={bvid} cid={cid}"));
                    let config = PlayerConfig {
                        bvid: Some(bvid.clone()),
                        cid: (cid > 0).then_some(cid),
                        quality_height: Some(quality_height),
                        quality_codecid: codec,
                        ..Default::default()
                    };
                    match crate::source::resolve(&config) {
                        Ok(resolved) => {
                            let ok = crate::mediacache::prefetch_source(&resolved.source);
                            log::line(&format!(
                                "playlist prefetch bvid={bvid} cid={cid} ok={}",
                                u8::from(ok)
                            ));
                        }
                        Err(error) => log::line(&format!(
                            "playlist prefetch resolve failed bvid={bvid} err={error:#}"
                        )),
                    }
                }
            });
    }

    fn open_sessions(&mut self, start_ms: u64, gate_open: bool) -> Result<()> {
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        let decoder = VideoDecoder::spawn(
            &self.video_spec(),
            start_ms,
            generation,
            gate_open,
            &self.gpu,
        )
        .with_context(|| format!("open video decoder at {start_ms} ms"))?;
        self.decoder = Some(decoder);

        // Above 2× the sound is unintelligible and its reader becomes the
        // bottleneck: at 10× the audio clock only reached ~4.7× and dragged
        // the picture with it. Run these speeds video-only and let the wall
        // clock drive the timeline.
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        // The audio chain supports the whole 0.1×–10× range (atempo stages),
        // and the wall-clock/atempo clock stays correct at every stop. Always
        // open sound, so 3×/5×/10× fast-forward is not silent.
        self.spawn_audio_at(start_ms);
        self.session_started = Instant::now();
        self.shared.position_ms.store(start_ms, Ordering::SeqCst);
        self.position_ms = start_ms;
        Ok(())
    }

    fn stop_sessions(&mut self) {
        self.stall_audio_paused = false;
        self.stall_pause = None;
        self.video_lag_since = None;
        self.video_resync = None;
        if let Some(mut decoder) = self.decoder.take() {
            decoder.stop();
        }
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
    }

    fn restart_at(&mut self, target_ms: u64) -> Result<()> {
        self.drop_loop_prefetch();
        self.stall_audio_paused = false;
        self.stall_pause = None;
        self.video_lag_since = None;
        self.video_resync = None;
        self.ab_swap_wait = None;
        self.ab_cache_playing = false;
        self.ab_cache_cursor = 0;
        self.pending_wheel_seek = None;
        let target = clamp_seek(target_ms, self.duration_ms);
        let was_playing = self.requested_play;
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        self.danmaku.reseed(target);
        let _ = self.danmaku_layer.clear();
        log::line(&format!(
            "danmaku seek reset target_ms={target} gen={generation} active=0"
        ));
        if let Some(mut decoder) = self.decoder.take() {
            decoder.stop();
        }
        if self.audio.is_none() {
            self.spawn_audio_at(target);
        }
        if let Some(audio) = &self.audio {
            // The old session's `ready` flag must not let the new picture
            // start before the new sound has even been spawned: reset the
            // shared clock before the commands are queued.
            audio.clock.ready.store(false, Ordering::SeqCst);
            audio.clock.playing.store(false, Ordering::SeqCst);
            audio.clock.position_ms.store(target, Ordering::SeqCst);
            audio.send(AudioCommand::Seek {
                target_ms: target,
                speed: self.speed as f64,
            });
            // The sound must not run ahead while the new picture is still
            // opening (ffmpeg's seek can take ~0.5 s). The seek's first frame
            // resumes it at the target, which is what keeps the A/V pair on the
            // same position instead of ~400 ms apart.
            audio.send(AudioCommand::Pause);
        }
        let decoder = VideoDecoder::spawn(&self.video_spec(), target, generation, false, &self.gpu)
            .with_context(|| format!("restart video decoder at {target} ms"))?;
        self.decoder = Some(decoder);
        self.session_started = Instant::now();
        self.seek_in_flight = true;
        self.starting = false;
        self.ended = false;
        self.playing = false;
        self.seek_target = target;
        self.stream_start_ms = target;
        self.position_ms = target;
        self.base_ms = target;
        self.anchor = None;
        self.paused_at = target;
        self.audio_master = false;
        self.first_frame_at = None;
        self.next = None;
        self.cur_ts_ms = target;
        self.next_tick_at = None;
        self.gate(false);
        self.status = "定位中…".to_string();
        self.ui_dirty = true;
        self.shared.position_ms.store(target, Ordering::SeqCst);
        log::line(&format!(
            "seek target_ms={target} gen={generation} requested_play={} duration_ms={}",
            u8::from(was_playing),
            self.duration_ms
        ));
        Ok(())
    }

    fn gate(&self, open: bool) {
        if let Some(decoder) = &self.decoder {
            decoder.gate.store(open, Ordering::SeqCst);
        }
    }

    // -------------------------------------------------------------- playback

    fn wall_position(&self) -> u64 {
        let Some(anchor) = self.anchor else {
            return self.base_ms;
        };
        let elapsed = anchor.elapsed().as_secs_f64() * self.speed as f64;
        let value = self.base_ms as f64 + elapsed * 1000.0;
        self.clamp_position(value.round() as u64)
    }

    fn clamp_position(&self, value: u64) -> u64 {
        if self.duration_ms > 0 {
            value.min(self.duration_ms)
        } else {
            value
        }
    }

    /// The device clock, when it is actually rendering. It stays readable
    /// while the sound is paused (stall / catch-up), which is exactly what
    /// keeps the picture frozen with it instead of running on a wall clock.
    fn audio_position(&self) -> Option<u64> {
        let audio = self.audio.as_ref()?;
        if !audio.clock.available.load(Ordering::SeqCst)
            || !audio.clock.ready.load(Ordering::SeqCst)
        {
            return None;
        }
        Some(self.clamp_position(
            audio.clock.position_ms.load(Ordering::SeqCst),
        ))
    }

    fn position_now(&self) -> u64 {
        if self.seek_in_flight {
            return self.seek_target;
        }
        if !self.playing {
            return self.paused_at;
        }
        // The sound is the clock whenever it exists: a picture that follows
        // a wall clock while the device runs at its own rate is exactly how
        // a slow decode turns into permanent lip desync.
        if let Some(audio_ms) = self.audio_position() {
            return audio_ms;
        }
        self.wall_position()
    }

    /// ---------------------------------------------------------- precision seek

    fn open_precision(&mut self) {
        self.heatmap = build_heatmap(self.danmaku.items(), self.duration_ms);
        if self.speed_open {
            self.commit_speed_preview();
            self.speed_open = false;
            self.speed_drag = None;
            self.speed_commit_at = None;
        }
        self.panel = 1;
        self.precision_open = true;
        self.precision_dragging = false;
        self.precision_morph_from = self.precision_anim;
        self.precision_morph_start = Instant::now();
        self.precision_step_open = false;
        self.tuner_drag = None;
        self.tuner_offset = 0.0;
        self.tuner_pulse = 0.0;
        self.tuner_pulse_dir = 0.0;
        // The precision panel replaces the toolbar, so any floating menu from
        // the toolbar is closed before the morph starts.
        self.close_dm_menu();
        self.loop_menu_open = false;
        self.danmaku_popup = false;
        self.dm_input_focus = false;
        self.settings_menu = None;
        self.chrome_shown = true;
        self.ui_dirty = true;
        self.note_activity();
        log::line("precision seek opened");
    }

    /// Toolbar button entry: open the precision panel, or return to the
    /// ordinary toolbar if it is already visible.
    fn toggle_precision(&mut self) {
        if self.precision_open {
            self.close_precision();
        } else {
            self.open_precision();
        }
    }

    fn close_precision(&mut self) {
        if !self.precision_open {
            return;
        }
        self.precision_open = false;
        self.precision_dragging = false;
        self.precision_morph_from = self.precision_anim;
        self.precision_morph_start = Instant::now();
        self.precision_step_open = false;
        self.tuner_drag = None;
        self.tuner_offset = 0.0;
        self.tuner_pulse = 0.0;
        self.tuner_pulse_dir = 0.0;
        self.drag_preview = None;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
        log::line("precision seek closed");
    }

    /// --------------------------------------------------------------- speed

    fn open_speed(&mut self) {
        if self.precision_open {
            self.precision_open = false;
            self.precision_dragging = false;
            self.precision_step_open = false;
            self.drag_preview = None;
            self.tuner_drag = None;
            self.tuner_offset = 0.0;
            self.tuner_pulse = 0.0;
            self.tuner_pulse_dir = 0.0;
        }
        self.panel = 2;
        self.speed_open = true;
        self.speed_drag = None;
        self.speed_preview = crate::speed::snap(self.speed);
        self.speed_base = self.speed_preview;
        self.speed_commit_at = None;
        self.tuner_offset = 0.0;
        self.tuner_pulse = 0.0;
        self.tuner_pulse_dir = 0.0;
        self.precision_morph_from = self.precision_anim;
        self.precision_morph_start = Instant::now();
        self.close_dm_menu();
        self.loop_menu_open = false;
        self.danmaku_popup = false;
        self.dm_input_focus = false;
        self.settings_menu = None;
        self.chrome_shown = true;
        self.ui_dirty = true;
        self.note_activity();
        log::line(&format!("speed panel opened speed={:.2}", self.speed));
    }

    fn toggle_speed(&mut self) {
        if self.speed_open {
            self.close_speed();
        } else {
            self.open_speed();
        }
    }

    fn close_speed(&mut self) {
        if !self.speed_open {
            return;
        }
        self.commit_speed_preview();
        self.speed_open = false;
        self.speed_drag = None;
        self.speed_commit_at = None;
        self.tuner_offset = 0.0;
        self.tuner_pulse = 0.0;
        self.tuner_pulse_dir = 0.0;
        self.precision_morph_from = self.precision_anim;
        self.precision_morph_start = Instant::now();
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
        log::line("speed panel closed");
    }

    /// Update the value shown in the panel without restarting the decoder.
    /// The wheel only has discrete stops, so every preview lands on one.
    fn preview_speed(&mut self, value: f32) {
        self.speed_preview = crate::speed::snap(value);
        self.ui_dirty = true;
    }

    /// Scroll the reel under the fixed selection window. Dragging left moves
    /// toward faster stops: the reel travels left and faster labels come in
    /// from the right. Every stop is one equal gap; the fractional travel is
    /// kept in `tuner_offset` so labels glide until release, when the chosen
    /// stop settles exactly in the window.
    fn drag_speed(&mut self, start_x: f32, start_speed: f32, x: f32) {
        let gap = crate::speed::WHEEL_GAP * self.overlay.layout.scale;
        let steps = (start_x - x) / gap;
        let target = crate::speed::from_index(
            crate::speed::index_of(start_speed) as i32 + steps.round() as i32,
        );
        self.preview_speed(target);
        self.speed_base = start_speed;
        self.tuner_offset = -steps * gap;
        self.tuner_pulse = 0.0;
    }

    fn commit_speed_preview(&mut self) {
        let speed = crate::speed::snap(self.speed_preview);
        self.speed_preview = speed;
        self.speed_base = speed;
        if (speed - self.speed).abs() < 0.001 {
            self.ui_dirty = true;
            return;
        }
        self.set_speed(speed as f64);
    }

    fn poll_pending_speed(&mut self) {
        let Some(at) = self.speed_commit_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.speed_commit_at = None;
        self.commit_speed_preview();
    }

    fn handle_speed_click(&mut self, px: f32, py: f32) -> bool {
        let layout = self.overlay.layout;
        if layout.speed_wheel.contains(px, py) {
            self.speed_drag = Some((px, self.speed_preview));
            self.speed_base = self.speed_preview;
            self.speed_commit_at = None;
            self.tuner_offset = 0.0;
            self.tuner_pulse = 0.0;
            return true;
        }
        layout.speed_panel.contains(px, py)
    }

    /// A and B are the only loop controls: once both points are set the
    /// player always loops that span. There is no separate enable switch.
    fn ab_range(&self) -> Option<(u64, u64)> {
        match (self.ab_a, self.ab_b) {
            (Some(a), Some(b)) if b > a => Some((a, b)),
            _ => None,
        }
    }

    fn drop_loop_prefetch(&mut self) {
        if let Some(mut decoder) = self.loop_prefetch.take() {
            decoder.stop();
            log::line("ab prefetch dropped");
        }
    }

    /// While the current pass approaches B, start decoding at A in the
    /// background. The warmed queue is swapped in at the loop point, so the
    /// picture never tears down the decoder or re-opens the source.
    fn poll_ab_prefetch(&mut self) {
        if self.ab_cache_playing
            || self
                .ab_cache
                .as_ref()
                .map(|cache| cache.ready)
                .unwrap_or(false)
        {
            return;
        }
        if !self.requested_play || self.ended {
            return;
        }
        let Some((a, b)) = self.ab_range() else {
            return;
        };
        if b <= a || self.loop_prefetch.is_some() {
            return;
        }
        // While the first pass is still seeking/starting at A, the clock can
        // still read the old position. The seek target is already A, so warm
        // the next pass from there instead of waiting for playback to resume.
        let position = if self.seek_in_flight {
            self.seek_target
        } else {
            self.position_now()
        };
        if position < a || position >= b {
            return;
        }
        // Start the next pass as soon as the current one enters A–B. Short
        // loops then get the whole span as load budget instead of the old
        // 1.2 s tail (and loops under 1.6 s no longer skip prefetch entirely).
        let generation = self.generation.wrapping_add(1);
        match VideoDecoder::spawn(&self.video_spec(), a, generation, true, &self.gpu) {
            Ok(decoder) => {
                self.loop_prefetch = Some(decoder);
                log::line(&format!(
                    "ab prefetch spawn a={a} b={b} position={position} gen={generation}"
                ));
            }
            Err(error) => {
                log::line(&format!("ab prefetch failed a={a} err={error:#}"));
            }
        }
    }

    /// Reset the presentation clock and audio/danmaku to A for another pass.
    /// Shared by the prefetch swap and the cached A–B replay.
    fn reset_loop_clock(&mut self, a: u64) {
        self.danmaku.reseed(a);
        let _ = self.danmaku_layer.clear();
        if let Some(audio) = &self.audio {
            audio.clock.ready.store(false, Ordering::SeqCst);
            audio.clock.playing.store(false, Ordering::SeqCst);
            audio.clock.position_ms.store(a, Ordering::SeqCst);
            audio.send(AudioCommand::Seek {
                target_ms: a,
                speed: self.speed as f64,
            });
        }
        self.position_ms = a;
        self.base_ms = a;
        self.paused_at = a;
        self.seek_target = a;
        self.stream_start_ms = a;
        self.seek_in_flight = false;
        self.starting = false;
        self.ended = false;
        self.playing = self.requested_play;
        self.audio_master = false;
        self.video_lag_since = None;
        self.video_resync = None;
        self.stall_pause = None;
        self.anchor = Some(Instant::now());
        self.cur_ts_ms = a;
        self.next = None;
        self.next_tick_at = Some(Instant::now());
        self.gate(self.playing);
        self.status.clear();
        self.shared.position_ms.store(a, Ordering::SeqCst);
        self.ui_dirty = true;
        self.sync_shared();
    }

    fn swap_loop_prefetch(&mut self, a: u64) -> bool {
        self.ab_swap_wait = None;
        let Some(mut prefetch) = self.loop_prefetch.take() else {
            return false;
        };
        if prefetch.queue.len() == 0 {
            // Keep the warming decoder for the grace wait instead of throwing
            // it away and reloading the video.
            self.loop_prefetch = Some(prefetch);
            log::line("ab prefetch not ready");
            return false;
        }
        if let Some(mut old) = self.decoder.take() {
            old.stop();
        }
        self.generation = prefetch.generation;
        self.decoder = Some(prefetch);
        self.reset_loop_clock(a);
        log::line(&format!(
            "ab loop swapped prefetched decoder a={a} gen={}",
            self.generation
        ));
        true
    }

    fn cache_ab_frame(&mut self, frame: &DecodedFrame) {
        if self.ab_cache_playing {
            return;
        }
        // Hardware frames pin slices from the decoder's fixed D3D11VA pool.
        // Keeping a two-second A/B span in GPU memory would starve the decoder;
        // the zero-copy path uses the prefetch decoder for A/B instead.
        if frame.data.is_gpu() {
            self.ab_cache = None;
            return;
        }
        let Some((a, b)) = self.ab_range() else {
            self.ab_cache = None;
            return;
        };
        if b <= a || b - a > AB_CACHE_MAX_MS {
            self.ab_cache = None;
            return;
        }
        let span_changed = self
            .ab_cache
            .as_ref()
            .map(|cache| cache.a != a || cache.b != b)
            .unwrap_or(true);
        if span_changed {
            self.ab_cache = Some(AbCache {
                a,
                b,
                frames: Vec::new(),
                bytes: 0,
                ready: false,
                disabled: false,
            });
        }
        let Some(cache) = self.ab_cache.as_mut() else {
            return;
        };
        if cache.disabled
            || cache.ready
            || frame.ts_ms < a
            || frame.ts_ms >= b
            || cache
                .frames
                .last()
                .map(|last| frame.ts_ms <= last.ts_ms)
                .unwrap_or(false)
        {
            return;
        }
        if cache.bytes.saturating_add(frame.data.len()) > AB_CACHE_MAX_BYTES {
            cache.disabled = true;
            cache.frames.clear();
            cache.bytes = 0;
            log::line("ab cache disabled: size cap");
            return;
        }
        cache.bytes += frame.data.len();
        cache.frames.push(DecodedFrame {
            data: frame.data.clone(),
            ts_ms: frame.ts_ms,
        });
        if frame.ts_ms.saturating_add(50) >= b {
            cache.ready = true;
            log::line(&format!(
                "ab cache ready a={a} b={b} frames={} bytes={}",
                cache.frames.len(),
                cache.bytes
            ));
        }
    }

    fn start_ab_cache_loop(&mut self, a: u64) -> bool {
        let ready = self
            .ab_cache
            .as_ref()
            .map(|cache| cache.ready && !cache.frames.is_empty())
            .unwrap_or(false);
        if !ready {
            return false;
        }
        self.ab_cache_playing = true;
        self.ab_cache_cursor = 0;
        self.ab_swap_wait = None;
        self.drop_loop_prefetch();
        self.gate(false);
        self.reset_loop_clock(a);
        // The cached pass does not consume the live decoder; keep its gate
        // closed so it cannot decode past B in the background.
        self.gate(false);
        log::line(&format!("ab cache replay start a={a}"));
        true
    }

    fn reset_ab_cache(&mut self) {
        self.ab_cache = None;
        self.ab_cache_playing = false;
        self.ab_cache_cursor = 0;
    }

    fn take_ab_cache_frame(&mut self, horizon_ms: u64) -> Option<DecodedFrame> {
        let cache = self.ab_cache.as_mut()?;
        while self.ab_cache_cursor < cache.frames.len() {
            let frame = &cache.frames[self.ab_cache_cursor];
            if frame.ts_ms > horizon_ms {
                return None;
            }
            let out = DecodedFrame {
                data: frame.data.clone(),
                ts_ms: frame.ts_ms,
            };
            self.ab_cache_cursor += 1;
            return Some(out);
        }
        None
    }

    fn set_ab_a(&mut self) {
        self.drop_loop_prefetch();
        self.ab_swap_wait = None;
        let was_cache = self.ab_cache_playing;
        self.reset_ab_cache();
        if was_cache {
            let at = self.position_now();
            self.seek(at);
        }
        let at = self.position_now();
        self.ab_a = Some(at);
        if self.ab_b.map(|b| b <= at).unwrap_or(false) {
            self.ab_b = None;
        }
        self.ui_dirty = true;
        log::line(&format!("ab a={at} range={:?}", self.ab_range()));
    }

    fn set_ab_b(&mut self) {
        self.drop_loop_prefetch();
        self.ab_swap_wait = None;
        let was_cache = self.ab_cache_playing;
        self.reset_ab_cache();
        if was_cache {
            let at = self.position_now();
            self.seek(at);
        }
        let at = self.position_now();
        if let Some(a) = self.ab_a {
            if at <= a {
                self.show_toast("B 必须晚于 A", true);
                return;
            }
        }
        self.ab_b = Some(at);
        if let Some((a, b)) = self.ab_range() {
            // B was just stamped at the playhead (or beyond it): jump back to A
            // so the promised A–B loop starts immediately.
            if self.position_now() >= b {
                self.seek(a);
            }
            log::line(&format!("ab range start a={a} b={b}"));
        }
        self.ui_dirty = true;
        log::line(&format!("ab b={at} range={:?}", self.ab_range()));
    }

    fn clear_ab(&mut self) {
        self.drop_loop_prefetch();
        self.ab_swap_wait = None;
        let was_cache = self.ab_cache_playing;
        self.reset_ab_cache();
        if was_cache {
            let at = self.position_now();
            self.seek(at);
        }
        self.ab_a = None;
        self.ab_b = None;
        self.pending_wheel_seek = None;
        self.ui_dirty = true;
        log::line("ab cleared");
    }

    /// Quantize a frame nudge against the source frame rate. The preview is
    /// shown immediately; the caller decides when to issue the decoder seek.
    fn nudge_frames(&mut self, frames: i64) -> Option<u64> {
        let fps = if self.source.fps > 1.0 {
            self.source.fps
        } else {
            30.0
        };
        let frame_ms = 1000.0 / fps;
        // While a previous detent's seek is still in flight, keep stacking
        // against its target. Reading the old clock for every fast wheel notch
        // would make most of them land on the same position.
        let current = self.drag_preview.unwrap_or_else(|| {
            if self.seek_in_flight {
                self.seek_target
            } else {
                self.position_now()
            }
        });
        let index = (current as f64 / frame_ms).round() + frames as f64;
        let target = self.clamp_position((index.max(0.0) * frame_ms).round() as u64);
        self.drag_preview = Some(target);
        self.ui_dirty = true;
        Some(target)
    }

    fn sidebar_max_scroll(&mut self) -> f32 {
        let layout = self.overlay.layout;
        let s = layout.scale;
        let content = layout.sidebar_content;
        let viewport = content.height().max(1.0);
        match self.sidebar_tab {
            SidebarTab::Comments => {
                let list_h = self.overlay.comments_list_height();
                let list_viewport = (viewport - layout.comments_header.height()).max(1.0);
                (list_h - list_viewport).max(0.0)
            }
            SidebarTab::Episodes => {
                let pages = self
                    .pages
                    .len()
                    .max(self.info.as_ref().map(|info| info.pages.len()).unwrap_or(0));
                let rows = pages as f32 * 52.0 * s;
                (52.0 * s + rows - (viewport - 8.0 * s)).max(0.0)
            }
            SidebarTab::Settings => {
                let rows = SETTINGS_ROWS as f32 * 48.0 * s;
                let count = self.settings_menu_len();
                let menu_h = if count > 0 {
                    settings_menu_height(s, count)
                } else {
                    0.0
                };
                (rows + menu_h - (viewport - 8.0 * s)).max(0.0)
            }
            SidebarTab::Performance => (PERFORMANCE_CONTENT_H * s - viewport).max(0.0),
            SidebarTab::Info => {
                let description = self
                    .info
                    .as_ref()
                    .map(|info| info.description.trim())
                    .filter(|text| !text.is_empty())
                    .unwrap_or("暂无简介");
                let width = layout.info_description.width();
                if self.info_scroll_height < 0.0
                    || (self.info_scroll_width - width).abs() > 0.5
                {
                    self.info_scroll_height =
                        self.overlay.info_description_height(description, width);
                    self.info_scroll_width = width;
                }
                let bottom = layout.info_description.top + self.info_scroll_height + 16.0 * s;
                (bottom - content.bottom).max(0.0)
            }
            _ => 0.0,
        }
    }

    fn animate_sidebar_scroll(&mut self) {
        if !self.sidebar_open {
            return;
        }
        let max = self.sidebar_max_scroll();
        self.sidebar_scroll_target = self.sidebar_scroll_target.clamp(0.0, max);
        let diff = self.sidebar_scroll_target - self.sidebar_scroll;
        if diff.abs() > 0.35 {
            self.sidebar_scroll += diff * 0.24;
            self.ui_dirty = true;
        } else if self.sidebar_scroll != self.sidebar_scroll_target {
            self.sidebar_scroll = self.sidebar_scroll_target;
            self.ui_dirty = true;
        }
    }

    fn enforce_ab_loop(&mut self) {
        if self.ab_cache_playing {
            return;
        }
        if self.seek_in_flight
            || self.dragging
            || self.precision_dragging
            || self.tuner_drag.is_some()
            || !self.playing
        {
            return;
        }
        let Some((a, b)) = self.ab_range() else {
            return;
        };
        if let Some((wait_a, since)) = self.ab_swap_wait {
            if wait_a != a {
                self.ab_swap_wait = None;
            } else if self
                .loop_prefetch
                .as_ref()
                .map(|decoder| decoder.queue.len() > 0)
                .unwrap_or(false)
            {
                // The next pass has its first frame: swap without a reload.
                self.swap_loop_prefetch(a);
                return;
            } else if since.elapsed() >= Duration::from_millis(600) {
                self.ab_swap_wait = None;
                self.drop_loop_prefetch();
                log::line("ab loop wait timeout; falling back to seek");
                self.seek(a);
                return;
            } else {
                return;
            }
        }
        if self.position_ms < a || self.position_ms >= b {
            if self
                .ab_cache
                .as_ref()
                .map(|cache| cache.ready)
                .unwrap_or(false)
                && self.start_ab_cache_loop(a)
            {
                return;
            }
            if self.swap_loop_prefetch(a) {
                return;
            }
            if self.loop_prefetch.is_some() {
                // The prefetch is spawned but its first frame is not ready yet.
                // Hold the last picture for a short grace instead of tearing
                // down the decoder, which is what showed as a reload at A.
                self.ab_swap_wait = Some((a, Instant::now()));
                self.gate(false);
                if let Some(audio) = &self.audio {
                    audio.send(AudioCommand::Pause);
                }
                self.ui_dirty = true;
                log::line(&format!(
                    "ab loop waiting for prefetch a={a} position={}",
                    self.position_ms
                ));
                return;
            }
            self.seek(a);
        }
    }

    /// Ease the toolbar↔bottom-panel morph and the frame wheel's glide. Runs
    /// every loop turn; marks the UI dirty while either is still moving, so
    /// the 16 ms redraw path stays active for the whole animation.
    fn advance_precision_anim(&mut self) {
        let target = if self.precision_open || self.speed_open {
            1.0
        } else {
            0.0
        };
        let progress = (self.precision_morph_start.elapsed().as_secs_f32() / 0.36).clamp(0.0, 1.0);
        let next = self.precision_morph_from + (target - self.precision_morph_from) * progress;
        if (next - self.precision_anim).abs() > 0.0001 {
            self.precision_anim = next;
            self.ui_dirty = true;
        }
        if progress >= 1.0 && self.precision_anim != target {
            self.precision_anim = target;
            self.ui_dirty = true;
        }
        if self.tuner_pulse > 0.0 {
            self.tuner_pulse = (self.tuner_pulse + 0.12).min(1.0);
            let travel = 12.0 * self.scale;
            self.tuner_offset =
                self.tuner_pulse_dir * travel * (std::f32::consts::PI * self.tuner_pulse).sin();
            if self.tuner_pulse >= 1.0 {
                self.tuner_pulse = 0.0;
                self.tuner_offset = 0.0;
            }
            self.ui_dirty = true;
        } else if self.tuner_drag.is_none() && self.speed_drag.is_none() {
            let glide = -self.tuner_offset;
            if glide.abs() > 0.05 {
                self.tuner_offset += glide * 0.42;
                self.ui_dirty = true;
            } else if self.tuner_offset != 0.0 {
                self.tuner_offset = 0.0;
                self.ui_dirty = true;
            }
        }
        // Keep the panel kind through the close animation so its content can
        // fade with the card; only forget it once the toolbar is fully back.
        if !self.precision_open && !self.speed_open && self.precision_anim <= 0.0001 && self.panel != 0
        {
            self.panel = 0;
            self.ui_dirty = true;
        }
    }

    /// The frame-step chip is a cycle button: 1 → 2 → 5 → 10 → 1 frames.
    /// Keeping the modes inside one click is faster than opening a dropdown
    /// while the right hand is already on the ruler.
    fn cycle_frame_step(&mut self) {
        self.frame_step = next_frame_step(self.frame_step);
        self.precision_step_open = false;
        self.ui_dirty = true;
        log::line(&format!("precision frame step={}", self.frame_step));
    }

    fn handle_precision_click(&mut self, px: f32, py: f32) -> bool {
        let layout = self.overlay.layout;
        if layout.precision_step.contains(px, py) {
            self.cycle_frame_step();
            return true;
        }
        if layout.precision_a.contains(px, py) {
            self.set_ab_a();
            return true;
        }
        if layout.precision_b.contains(px, py) {
            self.set_ab_b();
            return true;
        }
        if layout.precision_clear.contains(px, py) {
            self.clear_ab();
            return true;
        }
        if layout.precision_wheel.contains(px, py) {
            let start = self.drag_preview.unwrap_or_else(|| self.position_now());
            self.tuner_drag = Some((px, start));
            self.tuner_offset = 0.0;
            self.tuner_pulse = 0.0;
            self.precision_dragging = false;
            self.pending_wheel_seek = None;
            self.drag_preview = Some(start);
            self.ui_dirty = true;
            return true;
        }
        if layout.precision_track.contains(px, py) {
            self.precision_dragging = true;
            if self.duration_ms > 0 {
                let fraction = ((px - layout.precision_track.left)
                    / layout.precision_track.width().max(1.0))
                .clamp(0.0, 1.0);
                self.drag_preview = Some((fraction as f64 * self.duration_ms as f64) as u64);
                self.ui_dirty = true;
            }
            return true;
        }
        layout.precision.contains(px, py)
    }

    fn pause(&mut self) {
        self.drop_loop_prefetch();
        self.pending_wheel_seek = None;
        self.requested_play = false;
        if self.seek_in_flight {
            if let Some(audio) = &self.audio {
                audio.send(AudioCommand::Pause);
            }
            self.ui_dirty = true;
            return;
        }
        if self.starting {
            self.ui_dirty = true;
            return;
        }
        if !self.playing {
            return;
        }
        self.paused_at = self.position_now();
        self.record_watch_progress();
        self.position_ms = self.paused_at;
        self.playing = false;
        self.video_lag_since = None;
        self.on_user_pause();
        self.gate(false);
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::Pause);
        }
        self.status = "已暂停".to_string();
        self.ui_dirty = true;
        self.sync_shared();
        log::line(&format!("pause at_ms={}", self.paused_at));
    }

    fn play(&mut self) {
        self.requested_play = true;
        if self.ended {
            if let Err(error) = self.restart_at(0) {
                log::line(&format!("restart after end failed: {error:#}"));
            }
            return;
        }
        if self.starting {
            self.ui_dirty = true;
            return;
        }
        if self.playing || self.seek_in_flight {
            self.ui_dirty = true;
            return;
        }
        self.base_ms = self.paused_at;
        self.anchor = Some(Instant::now());
        self.playing = true;
        self.ended = false;
        self.video_lag_since = None;
        self.on_playback_started();
        self.gate(true);
        self.enforce_ab_loop();
        if let Some(audio) = &self.audio {
            // With the picture stalled or being jumped, the frame path owns
            // the resume; starting the sound first would run it ahead again.
            if self.video_resync.is_none() && !self.stall_audio_paused {
                audio.send(AudioCommand::Resume);
            }
        }
        self.status.clear();
        self.ui_dirty = true;
        self.sync_shared();
        log::line(&format!("play from_ms={}", self.base_ms));
    }

    fn toggle_play(&mut self) {
        if self.playing || self.starting {
            self.pause();
        } else {
            self.play();
        }
    }

    /// The wheel only moves the preview. Once the notches stop for a beat, the
    /// engine commits one real seek instead of tearing the decoder down for
    /// every notch.
    fn poll_pending_wheel_seek(&mut self) {
        let Some((target, at)) = self.pending_wheel_seek else {
            return;
        };
        if at.elapsed() < Duration::from_millis(280) {
            return;
        }
        self.pending_wheel_seek = None;
        self.drag_preview = None;
        log::line(&format!(
            "wheel preview commit target_ms={target} idle_ms={}",
            at.elapsed().as_millis()
        ));
        self.seek(target);
    }

    fn seek(&mut self, target_ms: u64) {
        if let Err(error) = self.restart_at(target_ms) {
            log::line(&format!("seek failed target_ms={target_ms} err={error:#}"));
        }
    }

    fn set_volume(&mut self, value: u8) {
        self.volume = value.min(100);
        self.muted = false;
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::SetVolume(self.volume));
        }
        self.shared.volume.store(self.volume, Ordering::SeqCst);
        self.shared.muted.store(false, Ordering::SeqCst);
        self.state_changed();
        self.ui_dirty = true;
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        // Unmuting a zero-volume stream would still be silent; give it back a
        // usable level. Wheel volume remains the only continuous control.
        if !self.muted && self.volume == 0 {
            self.volume = 50;
            self.shared.volume.store(self.volume, Ordering::SeqCst);
        }
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::SetVolume(if self.muted {
                0
            } else {
                self.volume
            }));
        }
        self.shared.muted.store(self.muted, Ordering::SeqCst);
        self.state_changed();
        self.ui_dirty = true;
        log::line(&format!("mute={}", u8::from(self.muted)));
    }

    fn set_danmaku_visible(&mut self, visible: bool) {
        if self.danmaku_visible == visible {
            self.ui_dirty = true;
            return;
        }
        self.danmaku_visible = visible;
        if visible {
            self.danmaku.reseed(self.position_now());
        } else {
            self.danmaku.clear();
            let _ = self.danmaku_layer.clear();
        }
        self.shared.danmaku_visible.store(visible, Ordering::SeqCst);
        self.state_changed();
        self.ui_dirty = true;
        log::line(&format!("danmaku visible={}", u8::from(visible)));
    }

    fn toggle_danmaku(&mut self) {
        let desired = !self.danmaku_visible;
        self.set_danmaku_visible(desired);
    }

    fn show_toast(&mut self, text: &str, error: bool) {
        self.toast = Some(ToastUi {
            text: text.to_string(),
            error,
        });
        self.toast_until = Some(Instant::now() + Duration::from_secs(3));
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
        log::line(&format!("toast error={} text={text}", u8::from(error)));
    }

    // ------------------------------------------------------- player state I/O

    /// Mark `player_state.json` dirty; the engine writes it once the burst of
    /// changes settles (`SAVE_DEBOUNCE`) and unconditionally on shutdown.
    fn state_changed(&mut self) {
        self.state_save.mark(Instant::now(), SAVE_DEBOUNCE);
    }

    /// Remember the client size after a manual resize. Stored in 96-DPI units
    /// so the same logical window size survives a monitor with different
    /// scaling.
    fn remember_window_size(&mut self, width: u32, height: u32) {
        if self.external {
            // The hosted window follows the Electron viewport.
            return;
        }
        let scale = window::dpi_scale(self.hwnd).max(0.1);
        let inset = if self.sidebar_open
            && !self.fullscreen
            && !self.mini
            && !window::is_maximized(self.hwnd)
        {
            self.sidebar_delta().max(0) as u32
        } else {
            0
        };
        let width = width.saturating_sub(inset);
        let width = ((width as f32 / scale).round() as u32).clamp(320, 16_384);
        let height = ((height as f32 / scale).round() as u32).clamp(180, 16_384);
        self.state.window_width = Some(width);
        self.state.window_height = Some(height);
        self.state_changed();
        log::line(&format!(
            "player window size remembered logical={width}x{height} scale={scale:.2}"
        ));
    }

    fn persist_state(&mut self) {
        self.state.volume = self.volume;
        self.state.muted = self.muted;
        self.state.speed = self.speed;
        self.state.aspect = self.aspect.as_u8();
        self.state.brightness = self.brightness;
        self.state.audio_only = self.audio_only;
        self.state.topmost = self.ontop;
        self.state.danmaku_visible = self.danmaku_visible;
        self.state.stats = self.stats;
        match self.state.save() {
            Ok(()) => log::line(&format!(
                "player state saved path={} volume={} muted={} speed={:.2} aspect={} brightness={:.2} audio_only={} ontop={} danmaku={} qn={:?} codecid={:?} audio_index={:?}",
                crate::state::state_path().display(),
                self.state.volume,
                u8::from(self.state.muted),
                self.state.speed,
                self.state.aspect,
                self.state.brightness,
                u8::from(self.state.audio_only),
                u8::from(self.state.topmost),
                u8::from(self.state.danmaku_visible),
                self.state.quality_qn,
                self.state.quality_codecid,
                self.state.audio_index
            )),
            Err(error) => log::line(&format!("player state save failed: {error:#}")),
        }
    }

    fn tick_player_state(&mut self) {
        if self.state_save.due(Instant::now()) {
            self.persist_state();
        }
        if self.last_watch_save.elapsed() >= WATCH_SAVE_INTERVAL {
            self.last_watch_save = Instant::now();
            // Pause and close store explicitly; the periodic write only needs
            // to run while the clock actually advances, so a window sitting
            // paused does not rewrite the same position every five seconds.
            if self.playing {
                self.record_watch_progress();
            }
        }
    }

    /// Store the current part's position for the next open. Only a picture
    /// that has actually presented counts: the loading card and the initial
    /// seek must never overwrite a stored resume point with zero.
    fn record_watch_progress(&mut self) {
        if !self.had_first_frame || self.duration_ms == 0 {
            return;
        }
        let Some(bvid) = self
            .source
            .bvid
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return;
        };
        let Some(cid) = self.source.cid.filter(|value| *value > 0) else {
            return;
        };
        let position = self.position_now().min(self.duration_ms);
        crate::watch::record(bvid, cid, position, self.duration_ms);
    }

    /// Follow the monitor under the player window. Moving the window between a
    /// 120 Hz and a 60 Hz screen rebuilds the interpolation plan and the 插帧
    /// menu; a fixed target that the new panel cannot pace falls back to 自动.
    fn tick_monitor_refresh(&mut self) {
        if self.monitor_check_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.monitor_check_at = Instant::now();
        let hz = self.monitor_refresh_hz();
        if hz == self.monitor_hz {
            return;
        }
        let previous = self.monitor_hz;
        self.monitor_hz = hz;
        log::line(&format!(
            "display monitor refresh changed from={previous} to={hz} interp_hz={}",
            self.interp_hz
        ));
        if self.interp_hz != 0 && !interp::level_supported(self.interp_hz, hz) {
            log::line(&format!(
                "interp target={} unsupported on {}hz, following auto",
                self.interp_hz, hz
            ));
            self.set_interp_hz(0);
        } else {
            self.set_interp_hz(self.interp_hz);
        }
        self.ui_dirty = true;
    }

    /// Drop the wheel volume capsule once its short reveal has elapsed. The
    /// main loop runs continuously while video plays, so the capsule leaves on
    /// time even if the mouse never moves again.
    fn tick_volume_wheel(&mut self) {
        if let Some(until) = self.volume_wheel_until {
            if Instant::now() >= until {
                self.volume_wheel_until = None;
                self.ui_dirty = true;
            }
        }
    }

    /// Remember the stream the user picked so the next `Player::open` starts on
    /// it. Only explicit switches call this; an automatic fallback for a video
    /// that lacks the stored tier must not overwrite the preference.
    fn remember_track(&mut self) {
        self.state.quality_qn = Some(self.source.qn);
        self.state.quality_codecid = Some(self.source.codecid);
        self.state.audio_index = Some(self.source.audio_index);
        self.state_changed();
    }

    // -------------------------------------------------- danmaku interactions

    fn danmaku_hit_at(&self, x: f32, y: f32) -> Option<DanmakuHit> {
        if !self.danmaku_visible || self.danmaku.is_empty() {
            return None;
        }
        self.danmaku.hit_test(self.position_now(), x, y)
    }

    fn open_dm_menu(&mut self, hit: &DanmakuHit, x: f32, y: f32) {
        self.dm_menu = Some(DanmakuMenuUi {
            x,
            y,
            dmid: hit.dmid.clone(),
            mid_hash: hit.mid_hash.clone(),
            text: hit.text.clone(),
            submenu: false,
        });
        self.dm_long = None;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
        log::line(&format!(
            "danmaku menu open idx={} dmid={:?} mid_hash={:?} text={:?} at={:.0},{:.0}",
            hit.index, hit.dmid, hit.mid_hash, hit.text, x, y
        ));
    }

    fn close_dm_menu(&mut self) {
        self.dm_menu_drag = None;
        if self.dm_menu.take().is_some() {
            self.ui_dirty = true;
            log::line("danmaku menu closed");
        }
    }

    /// Click routing for the context menu. Returns true when the click belonged
    /// to the menu (including a click on its padding) and must not fall through
    /// to play/pause.
    fn handle_dm_menu_click(&mut self, x: f32, y: f32) -> bool {
        let Some(menu) = self.dm_menu.clone() else {
            return false;
        };
        let layout = self.overlay.layout;
        let s = layout.scale;
        let panel = dm_menu_rect(menu.x, menu.y, layout.width, layout.height, s);
        if menu.submenu {
            let report = dm_report_rect(panel, layout.width, layout.height, s);
            for index in 0..DM_REPORT_LABELS.len() {
                if dm_report_row(report, index, s).contains(x, y) {
                    let reason = match index {
                        0..=3 => index as u32 + 1,
                        _ => REPORT_OTHER_REASON,
                    };
                    self.report_danmaku(reason);
                    return true;
                }
            }
            if report.contains(x, y) {
                return true;
            }
        }
        for index in 0..DM_MENU_LABELS.len() {
            if dm_menu_row(panel, index, s).contains(x, y) {
                match index {
                    DM_MENU_LIKE => self.thumbup_danmaku(),
                    DM_MENU_REPORT => {
                        if let Some(menu) = self.dm_menu.as_mut() {
                            menu.submenu = true;
                            self.ui_dirty = true;
                            log::line(&format!("danmaku menu report submenu dmid={}", menu.dmid));
                        }
                    }
                    DM_MENU_BLOCK => {
                        let mid_hash = menu.mid_hash.clone();
                        self.dm_menu = None;
                        self.block_danmaku_sender(&mid_hash);
                    }
                    DM_MENU_COPY => {
                        let text = menu.text.clone();
                        self.dm_menu = None;
                        self.copy_danmaku_text(&text);
                    }
                    _ => {}
                }
                return true;
            }
        }
        panel.contains(x, y)
    }

    fn copy_danmaku_text(&mut self, text: &str) {
        match clipboard::set_text(text) {
            Ok(()) => self.show_toast("已复制弹幕内容", false),
            Err(error) => self.show_toast(&format!("复制失败：{error:#}"), true),
        }
        self.ui_dirty = true;
        log::line(&format!(
            "danmaku copy content len={}",
            text.chars().count()
        ));
    }

    /// Copy the current video's BV / av id from the info sidebar.
    fn copy_video_id(&mut self) {
        let id = self
            .info
            .as_ref()
            .map(|info| info.video_id.trim().to_string())
            .unwrap_or_default();
        if id.is_empty() {
            self.show_toast("没有可复制的视频 ID", true);
            self.ui_dirty = true;
            return;
        }
        match clipboard::set_text(&id) {
            Ok(()) => self.show_toast("已复制视频 ID", false),
            Err(error) => self.show_toast(&format!("复制失败：{error:#}"), true),
        }
        self.ui_dirty = true;
        log::line(&format!("video id copy id={id}"));
    }

    fn thumbup_danmaku(&mut self) {
        let Some(menu) = self.dm_menu.clone() else {
            return;
        };
        self.dm_menu = None;
        self.ui_dirty = true;
        if menu.dmid.is_empty() {
            self.show_toast("该弹幕没有 dmid，无法点赞", true);
            return;
        }
        if !self.dm_thumb_inflight.insert(menu.dmid.clone()) {
            self.show_toast("该弹幕正在点赞…", false);
            return;
        }
        let (Some(session), Some(cid)) = (self.session.clone(), self.source.cid) else {
            self.dm_thumb_inflight.remove(&menu.dmid);
            self.show_toast("当前来源没有可用的登录会话", true);
            return;
        };
        let dmid = menu.dmid.clone();
        let tx = self.dm_action_tx.clone();
        log::line(&format!("danmaku thumbup request dmid={dmid}"));
        self.show_toast("正在点赞弹幕…", false);
        let _ = std::thread::Builder::new()
            .name("player-dm-thumbup".into())
            .spawn(move || {
                let result = match o11a1::bilibili::shared_runtime() {
                    Some(runtime) => runtime
                        .block_on(session.danmaku_thumbup(cid, &dmid, 1))
                        .map_err(|error| format!("{error:#}")),
                    None => Err("网络运行时不可用".to_string()),
                };
                let (code, message) = match result {
                    Ok((code, message)) => (code, message),
                    Err(error) => (-1, error),
                };
                let _ = tx.send(DmActionResult {
                    kind: "点赞",
                    dmid,
                    code,
                    message,
                });
            });
    }

    fn report_danmaku(&mut self, reason: u32) {
        let Some(menu) = self.dm_menu.clone() else {
            return;
        };
        self.dm_menu = None;
        self.ui_dirty = true;
        if menu.dmid.is_empty() {
            self.show_toast("该弹幕没有 dmid，无法举报", true);
            return;
        }
        let (Some(session), Some(cid)) = (self.session.clone(), self.source.cid) else {
            self.show_toast("当前来源没有可用的登录会话", true);
            return;
        };
        let dmid = menu.dmid.clone();
        let content = if reason == REPORT_OTHER_REASON {
            "其他".to_string()
        } else {
            String::new()
        };
        let tx = self.dm_action_tx.clone();
        log::line(&format!(
            "danmaku report request dmid={dmid} reason={reason} content={content:?}"
        ));
        self.show_toast("正在举报弹幕…", false);
        let _ = std::thread::Builder::new()
            .name("player-dm-report".into())
            .spawn(move || {
                let result = match o11a1::bilibili::shared_runtime() {
                    Some(runtime) => runtime
                        .block_on(session.danmaku_report(cid, &dmid, reason, &content))
                        .map_err(|error| format!("{error:#}")),
                    None => Err("网络运行时不可用".to_string()),
                };
                let (code, message) = match result {
                    Ok((code, message)) => (code, message),
                    Err(error) => (-1, error),
                };
                let _ = tx.send(DmActionResult {
                    kind: "举报",
                    dmid,
                    code,
                    message,
                });
            });
    }

    fn handle_dm_action(&mut self, result: DmActionResult) {
        if result.kind == "点赞" {
            self.dm_thumb_inflight.remove(&result.dmid);
        }
        let text = if result.code == 0 {
            format!("{}成功 code=0 {}", result.kind, result.message)
        } else {
            format!(
                "{}失败 code={}：{}",
                result.kind, result.code, result.message
            )
        };
        log::line(&format!(
            "danmaku action kind={} dmid={} code={} message={}",
            result.kind, result.dmid, result.code, result.message
        ));
        self.show_toast(text.trim(), result.code != 0);
    }

    /// Persist `mid_hash` in `settings.blocked_senders`, remove the sender's
    /// active comments and arm the 3 s undo row.
    fn block_danmaku_sender(&mut self, mid_hash: &str) {
        self.close_dm_menu();
        self.dm_long = None;
        if mid_hash.is_empty() {
            self.show_toast("该弹幕没有用户标识，无法屏蔽", true);
            return;
        }
        let removed = self.danmaku.block_sender(mid_hash);
        self.redraw_danmaku_now();
        let mut stored = o11a1::settings::load();
        let already = stored
            .blocked_senders
            .iter()
            .any(|sender| sender == mid_hash);
        if !already {
            stored.blocked_senders.push(mid_hash.to_string());
            if let Err(error) = o11a1::settings::save(&stored) {
                log::line(&format!("blocked sender save failed: {error:#}"));
                self.show_toast("屏蔽已生效，但保存设置失败", true);
            } else {
                log::line(&format!(
                    "danmaku blocked sender mid_hash={mid_hash} total={}",
                    stored.blocked_senders.len()
                ));
            }
        } else {
            log::line(&format!(
                "danmaku blocked sender mid_hash={mid_hash} already_present=1"
            ));
        }
        self.undo = Some(UndoBlock {
            mid_hash: mid_hash.to_string(),
            until: Instant::now() + Duration::from_millis(UNDO_MS),
        });
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
        log::line(&format!(
            "danmaku block mid_hash={mid_hash} removed_active={removed} undo_ms={UNDO_MS}"
        ));
    }

    fn undo_blocked_sender(&mut self) {
        let Some(undo) = self.undo.take() else {
            return;
        };
        self.ui_dirty = true;
        let mut stored = o11a1::settings::load();
        let before = stored.blocked_senders.len();
        stored
            .blocked_senders
            .retain(|sender| sender != &undo.mid_hash);
        if stored.blocked_senders.len() != before {
            if let Err(error) = o11a1::settings::save(&stored) {
                log::line(&format!("blocked sender undo save failed: {error:#}"));
                self.show_toast("撤销屏蔽失败：无法保存设置", true);
                return;
            }
        }
        self.danmaku.unblock_sender(&undo.mid_hash);
        self.reload_danmaku_view();
        log::line(&format!(
            "danmaku block undo mid_hash={} blocked_total={}",
            undo.mid_hash,
            stored.blocked_senders.len()
        ));
        self.show_toast("已撤销屏蔽", false);
    }

    fn tick_undo(&mut self) {
        if self
            .undo
            .as_ref()
            .map(|undo| Instant::now() >= undo.until)
            .unwrap_or(false)
        {
            self.undo = None;
            self.ui_dirty = true;
            log::line("danmaku block undo expired");
        }
    }

    // ----------------------------------------------------- danmaku history

    fn request_history_dates(&mut self) {
        if self.history_dates_rx.is_some() {
            return;
        }
        let (Some(session), Some(cid)) = (self.session.clone(), self.source.cid) else {
            self.show_toast("当前来源没有历史弹幕", true);
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.history_dates_rx = Some(rx);
        self.history_dates_loading = true;
        self.history_dates_error = None;
        self.ui_dirty = true;
        log::line(&format!("danmaku history dates request cid={cid}"));
        let _ = std::thread::Builder::new()
            .name("player-dm-history-dates".into())
            .spawn(move || {
                let result = match o11a1::bilibili::shared_runtime() {
                    Some(runtime) => runtime
                        .block_on(session.danmaku_history_dates(cid))
                        .map_err(|error| format!("{error:#}")),
                    None => Err("网络运行时不可用".to_string()),
                };
                let _ = tx.send(result);
            });
    }

    fn request_history(&mut self, date: String) {
        if self.history_rx.is_some() {
            self.show_toast("正在加载历史弹幕…", false);
            return;
        }
        let (Some(session), Some(cid)) = (self.session.clone(), self.source.cid) else {
            self.show_toast("当前来源没有历史弹幕", true);
            return;
        };
        self.history_loading = true;
        self.history_error = None;
        self.history_dates_error = None;
        self.ui_dirty = true;
        let (tx, rx) = mpsc::channel();
        self.history_rx = Some(rx);
        log::line(&format!("danmaku history request cid={cid} date={date}"));
        let _ = std::thread::Builder::new()
            .name("player-dm-history".into())
            .spawn(move || {
                let result = match o11a1::bilibili::shared_runtime() {
                    Some(runtime) => runtime
                        .block_on(session.danmaku_history(cid, &date))
                        .map(|history| HistoryPayload {
                            date: date.clone(),
                            items: history
                                .items
                                .into_iter()
                                .filter(|entry| !entry.text.trim().is_empty())
                                .map(convert_item)
                                .collect(),
                            source: history.source,
                        })
                        .map_err(|error| format!("{error:#}")),
                    None => Err("网络运行时不可用".to_string()),
                };
                let _ = tx.send(result);
            });
    }

    fn poll_history(&mut self) {
        let dates = self
            .history_dates_rx
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("历史弹幕日期线程已结束".to_string())),
            });
        if let Some(result) = dates {
            self.history_dates_rx = None;
            self.history_dates_loading = false;
            match result {
                Ok(dates) => {
                    log::line(&format!(
                        "danmaku history dates loaded n={} first={} last={}",
                        dates.len(),
                        dates.first().map(String::as_str).unwrap_or("-"),
                        dates.last().map(String::as_str).unwrap_or("-")
                    ));
                    self.history_dates = dates;
                    self.history_dates_error = None;
                    if let Some(index) = self.qa_history_pick.take() {
                        self.pick_history_date(index);
                    }
                }
                Err(error) => {
                    log::line(&format!("danmaku history dates failed reason={error}"));
                    self.history_dates_error = Some(error);
                }
            }
            self.ui_dirty = true;
        }

        let history = self.history_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("历史弹幕线程已结束".to_string())),
        });
        if let Some(result) = history {
            self.history_rx = None;
            self.history_loading = false;
            match result {
                Ok(payload) => {
                    if !self.sidebar_open || self.sidebar_tab != SidebarTab::History {
                        log::line(&format!(
                            "danmaku history ignored date={} reason=panel-closed",
                            payload.date
                        ));
                        self.history_loading = false;
                        self.ui_dirty = true;
                        return;
                    }
                    let date = payload.date.clone();
                    let total = payload.items.len();
                    self.history_date = Some(date.clone());
                    self.history_items = payload.items;
                    self.history_error = None;
                    self.reload_danmaku_view();
                    log::line(&format!(
                        "danmaku history applied date={date} n={total} source={} view=history",
                        payload.source
                    ));
                    self.show_toast(&format!("历史弹幕 {date} · {total} 条"), false);
                }
                Err(error) => {
                    log::line(&format!("danmaku history failed reason={error}"));
                    self.history_error = Some(error);
                }
            }
            self.ui_dirty = true;
        }
    }

    fn pick_history_date(&mut self, index: usize) {
        let Some(date) = self.history_dates.get(index).cloned() else {
            self.qa_history_pick = Some(index);
            return;
        };
        self.request_history(date);
    }

    /// Install the live list or the selected historical list into the model.
    /// The overlay is the same layer in both cases; only the item source and
    /// the log label change.
    fn reload_danmaku_view(&mut self) {
        let (items, view) = match &self.history_date {
            Some(_) => (self.history_items.clone(), "history"),
            None => (self.live_danmaku.clone(), "live"),
        };
        self.apply_danmaku_items(items, view);
        if !self.playing {
            // A date selection or an undo can happen while paused; the regular
            // 120 Hz draw only runs during playback.
            self.redraw_danmaku_now();
        }
    }

    fn restore_live_danmaku(&mut self) {
        if self.history_date.take().is_some() {
            self.history_items.clear();
            self.history_loading = false;
            self.reload_danmaku_view();
            log::line("danmaku history closed view=live");
        }
    }

    /// Drop every comment/action tied to the old cid when 分P/换集 retargets the
    /// player. In-flight workers may still finish and send to a receiver we
    /// already dropped, which cannot install anything into the new page.
    fn reset_danmaku_for_source(&mut self) {
        self.restore_live_danmaku();
        self.close_dm_menu();
        self.dm_long = None;
        self.undo = None;
        self.dm_thumb_inflight.clear();
        self.live_danmaku.clear();
        self.history_dates.clear();
        self.history_dates_rx = None;
        self.history_dates_loading = false;
        self.history_dates_error = None;
        self.history_rx = None;
        self.history_loading = false;
        self.history_error = None;
        self.history_date = None;
        self.history_items.clear();
        self.qa_history_pick = None;
    }

    // ---------------------------------------------------------- 历史 heartbeat

    fn watched_seconds(&self) -> u64 {
        let extra = self
            .heartbeat_anchor
            .map(|at| at.elapsed())
            .unwrap_or_default();
        (self.heartbeat_accum + extra).as_secs()
    }

    fn send_heartbeat(&mut self, play_type: u64) {
        let Some(heartbeat) = &self.heartbeat else {
            return;
        };
        let played_s = self.position_now() / 1000;
        let watched_s = self.watched_seconds();
        let duration_s = self.duration_ms / 1000;
        if play_type == 1 {
            log::line(&format!(
                "heartbeat start aid={} bvid={} cid={} played_s={played_s} duration_s={duration_s}",
                self.aid,
                self.source.bvid.as_deref().unwrap_or("-"),
                self.source.cid.unwrap_or(0)
            ));
        }
        heartbeat.beat(Beat {
            played_s,
            watched_s,
            duration_s,
            play_type,
        });
    }

    fn on_playback_started(&mut self) {
        if !self.heartbeat_started {
            self.heartbeat_started = true;
            self.heartbeat_anchor = Some(Instant::now());
            self.last_heartbeat = Instant::now();
            self.send_heartbeat(1);
        } else if self.heartbeat_anchor.is_none() {
            self.heartbeat_anchor = Some(Instant::now());
            self.last_heartbeat = Instant::now();
            self.send_heartbeat(3);
        }
    }

    fn on_user_pause(&mut self) {
        if let Some(at) = self.heartbeat_anchor.take() {
            self.heartbeat_accum += at.elapsed();
            self.send_heartbeat(2);
        }
    }

    fn on_playback_end(&mut self) {
        if let Some(at) = self.heartbeat_anchor.take() {
            self.heartbeat_accum += at.elapsed();
        }
        if self.heartbeat_started {
            self.send_heartbeat(4);
            self.heartbeat_started = false;
        }
    }

    fn tick_heartbeat(&mut self) {
        if !self.heartbeat_started || !self.playing || self.heartbeat_anchor.is_none() {
            return;
        }
        if self.last_heartbeat.elapsed() >= Duration::from_secs(15) {
            self.last_heartbeat = Instant::now();
            self.send_heartbeat(0);
        }
    }

    /// 分P/换集: the cid changed, so the old worker must not report the new
    /// position against the old episode.
    fn restart_heartbeat_for_source(&mut self) {
        self.heartbeat_started = false;
        self.heartbeat_anchor = None;
        self.heartbeat_accum = Duration::ZERO;
        self.last_heartbeat = Instant::now();
        self.heartbeat = match (&self.session, &self.source.bvid, self.source.cid) {
            (Some(session), Some(bvid), Some(cid)) => {
                let worker = Heartbeat::spawn(session.clone(), bvid.clone(), cid);
                if self.aid > 0 {
                    worker.set_aid(self.aid);
                }
                Some(worker)
            }
            _ => None,
        };
        log::line(&format!(
            "heartbeat retarget bvid={} cid={}",
            self.source.bvid.as_deref().unwrap_or("-"),
            self.source.cid.unwrap_or(0)
        ));
    }

    fn apply_danmaku_ui(&mut self) {
        self.apply_danmaku_ui_inner(true);
    }

    /// New video in the same window: the popup's values live in the global
    /// settings file, so reload them instead of keeping whatever the previous
    /// source happened to have in memory.
    fn reload_danmaku_settings(&mut self) {
        let stored = o11a1::settings::load();
        self.danmaku_opacity = stored.danmaku_opacity.min(100);
        self.danmaku_font = o11a1::settings::normalise_danmaku_font(stored.danmaku_font);
        self.danmaku_speed = stored.danmaku_speed.clamp(50, 200);
        self.danmaku_area = o11a1::settings::clamp_area(stored.danmaku_area);
        self.block_scroll = stored.block_scroll;
        self.block_top = stored.block_top;
        self.block_bottom = stored.block_bottom;
        self.apply_danmaku_ui_inner(false);
        log::line(&format!(
            "danmaku settings reloaded opacity={} font={} speed={} area={}",
            self.danmaku_opacity, self.danmaku_font, self.danmaku_speed, self.danmaku_area
        ));
    }

    fn apply_danmaku_ui_inner(&mut self, persist: bool) {
        let font_scale = self.danmaku_font as f32 / 100.0;
        self.danmaku.reconfigure(
            self.danmaku_opacity as f32 / 100.0,
            font_scale,
            self.danmaku_speed,
            self.danmaku_area,
            self.block_scroll,
            self.block_top,
            self.block_bottom,
        );
        // The model's metrics changed, but the Direct2D layer owns the text
        // formats and the opacity brush: refresh both before the immediate
        // redraw or the active comments keep the old size / alpha.
        if let Err(error) = self.danmaku_layer.set_fonts(self.danmaku.font_sizes()) {
            log::line(&format!("danmaku live font update failed: {error:#}"));
        }
        if let Err(error) = self
            .danmaku_layer
            .set_opacity(self.danmaku.settings().opacity)
        {
            log::line(&format!("danmaku live opacity update failed: {error:#}"));
        }
        // A settings change must be instantaneous: keep the active comments
        // and redraw them with the new opacity / font / placement instead of
        // clearing the layer and waiting for respawns.
        self.redraw_danmaku_now();
        if persist {
            let mut stored = o11a1::settings::load();
            stored.danmaku_opacity = self.danmaku_opacity;
            stored.danmaku_font = self.danmaku_font;
            stored.danmaku_speed = self.danmaku_speed;
            stored.danmaku_area = self.danmaku_area;
            stored.block_scroll = self.block_scroll;
            stored.block_top = self.block_top;
            stored.block_bottom = self.block_bottom;
            match o11a1::settings::save(&stored) {
                Ok(()) => self
                    .shared
                    .settings_revision
                    .fetch_add(1, Ordering::SeqCst),
                Err(error) => {
                    log::line(&format!("danmaku settings save failed: {error:#}"));
                    0
                }
            };
        }
        log::line(&format!(
            "danmaku ui opacity={} font={} speed={} area={} block={}/{}/{}",
            self.danmaku_opacity,
            self.danmaku_font,
            self.danmaku_speed,
            self.danmaku_area,
            u8::from(self.block_scroll),
            u8::from(self.block_top),
            u8::from(self.block_bottom)
        ));
        self.ui_dirty = true;
    }

    fn set_danmaku_opacity(&mut self, value: u32) {
        self.danmaku_opacity = value.min(100);
        self.apply_danmaku_ui();
    }

    fn set_danmaku_font(&mut self, percent: u32) {
        self.danmaku_font = percent.clamp(50, 150);
        self.apply_danmaku_ui();
    }

    fn set_danmaku_speed(&mut self, value: u32) {
        self.danmaku_speed = value.clamp(50, 200);
        self.apply_danmaku_ui();
    }

    fn set_danmaku_area(&mut self, value: u32) {
        self.danmaku_area = value.clamp(25, 100);
        self.apply_danmaku_ui();
    }

    fn toggle_block(&mut self, index: usize) {
        match index {
            0 => self.block_scroll = !self.block_scroll,
            1 => self.block_top = !self.block_top,
            _ => self.block_bottom = !self.block_bottom,
        }
        self.apply_danmaku_ui();
    }

    fn request_comments(&mut self, reset: bool) {
        if self.comments_loading {
            if reset {
                self.comments_reload_pending = true;
            }
            return;
        }
        if reset {
            self.sidebar_scroll_target = 0.0;
        }
        self.comments_loading = true;
        self.ui_dirty = true;
        log::line(&format!(
            "comments request hot={} reset={} cursor={}",
            u8::from(self.comments_hot),
            u8::from(reset),
            if self.comments_cursor.is_empty() {
                "-"
            } else {
                &self.comments_cursor
            }
        ));
        let _ = self.detail_tx.send(DetailRequest::Comments {
            hot: self.comments_hot,
            reset,
            cursor: if reset {
                String::new()
            } else {
                self.comments_cursor.clone()
            },
        });
    }

    fn set_comments_sort(&mut self, hot: bool) {
        if self.comments_hot == hot {
            return;
        }
        self.comments_hot = hot;
        self.ui_dirty = true;
        self.comments_reload_pending = true;
        if !self.comments_loading {
            self.comments_reload_pending = false;
            self.request_comments(true);
        }
    }

    fn request_comment_replies(&mut self, rpid: i64) {
        let Some(comment) = self
            .comments
            .iter_mut()
            .find(|comment| comment.rpid == rpid)
        else {
            return;
        };
        if comment.replies_loading || comment.replies_end {
            return;
        }
        let page = if comment.replies_page <= 0 {
            1
        } else {
            comment.replies_page + 1
        };
        comment.replies_loading = true;
        comment.expanded = true;
        self.comments_generation = self.comments_generation.wrapping_add(1);
        self.ui_dirty = true;
        log::line(&format!("comment replies request root={rpid} page={page}"));
        let _ = self.detail_tx.send(DetailRequest::CommentReplies {
            root_rpid: rpid,
            page,
        });
    }

    fn toggle_comment_thread(&mut self, rpid: i64, action: crate::overlay::CommentAction) {
        let Some(index) = self
            .comments
            .iter()
            .position(|comment| comment.rpid == rpid)
        else {
            return;
        };
        match action {
            crate::overlay::CommentAction::Toggle => {
                let expanded = self.comments[index].expanded;
                self.comments[index].expanded = !expanded;
                if !expanded {
                    let comment = &self.comments[index];
                    let shown = comment.replies.len() as i64;
                    if comment.reply_count > shown && !comment.replies_end {
                        self.request_comment_replies(rpid);
                    }
                }
            }
            crate::overlay::CommentAction::More => {
                self.request_comment_replies(rpid);
            }
        }
        self.comments_generation = self.comments_generation.wrapping_add(1);
        self.ui_dirty = true;
    }

    fn advance_comment_animations(&mut self) {
        let now = Instant::now();
        let dt = now
            .duration_since(self.comments_anim_last)
            .as_secs_f32()
            .min(0.05);
        self.comments_anim_last = now;
        let target = if self.comments_hot { 0.0 } else { 1.0 };
        let diff = target - self.comments_sort_mix;
        if diff.abs() > 0.002 {
            // ~60 ms time constant: a 180–220 ms glide independent of loop rate.
            let k = 1.0 - (-dt / 0.055).exp();
            self.comments_sort_mix += diff * k;
            self.ui_dirty = true;
        } else if self.comments_sort_mix != target {
            self.comments_sort_mix = target;
            self.ui_dirty = true;
        }
        if self.comments_enter < 1.0 {
            self.comments_enter = (self.comments_enter + dt / 0.22).min(1.0);
            self.ui_dirty = true;
        }
    }

    /// Fill the viewport without a load-more button: once the smooth scroll is
    /// within one comfortable screen of the end, fetch the next page. The
    /// overlay's measured height is only trusted after its layout was built.
    fn poll_comments_autoload(&mut self) {
        if !self.sidebar_open
            || self.sidebar_tab != SidebarTab::Comments
            || self.comments_loading
            || self.comments_end
            || self.comments.is_empty()
            || !self.overlay.comments_layout_ready(self.comments_generation)
        {
            return;
        }
        let max = self.sidebar_max_scroll();
        let margin = 220.0 * self.scale;
        if max <= 1.0 || self.sidebar_scroll_target >= max - margin {
            self.request_comments(false);
        }
    }

    fn handle_detail(&mut self, event: DetailEvent) {
        match event {
            DetailEvent::Info(Ok(info)) => {
                let pages: Vec<PageUi> = info
                    .pages
                    .iter()
                    .map(|page| PageUi {
                        cid: page.cid,
                        title: page.title.clone(),
                        duration: page.duration,
                    })
                    .collect();
                self.pages = pages;
                self.current_page = info.current_page;
                if info.aid > 0 {
                    self.aid = info.aid as u64;
                    if let Some(heartbeat) = &self.heartbeat {
                        heartbeat.set_aid(self.aid);
                    }
                    log::line(&format!(
                        "heartbeat aid={} bvid={}",
                        self.aid,
                        self.source.bvid.as_deref().unwrap_or("-")
                    ));
                }
                self.info = Some(InfoUi {
                    title: info.title,
                    video_id: self
                        .source
                        .bvid
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| {
                            if info.aid > 0 {
                                format!("av{}", info.aid)
                            } else {
                                String::new()
                            }
                        }),
                    owner: info.owner,
                    views: info.views,
                    pubdate: info.pubdate,
                    cover: info.cover,
                    description: info.description,
                    pages: self.pages.clone(),
                    current_page: info.current_page,
                });
                self.info_scroll_height = -1.0;
                self.ui_dirty = true;
                log::line(&format!(
                    "detail info loaded pages={} current={}",
                    self.pages.len(),
                    self.current_page
                ));
                if self.sidebar_open
                    && self.sidebar_tab == SidebarTab::Comments
                    && self.comments.is_empty()
                    && !self.comments_loading
                {
                    self.request_comments(true);
                }
            }
            DetailEvent::Info(Err(error)) => {
                log::line(&format!("detail info failed reason={error}"));
                self.show_toast(&format!("视频信息加载失败：{error}"), true);
            }
            DetailEvent::Comments(Ok(data)) => {
                if data.hot != self.comments_hot {
                    // A page for the other order arrived after the user
                    // switched. Drop it and start the current order.
                    self.comments_loading = false;
                    if self.comments_reload_pending {
                        self.comments_reload_pending = false;
                        self.request_comments(true);
                    }
                    return;
                }
                if data.reset {
                    self.comments.clear();
                    self.sidebar_scroll = 0.0;
                    self.sidebar_scroll_target = 0.0;
                    self.comments_enter = 0.0;
                }
                for comment in data.comments {
                    let comment = comment_ui(comment, &self.avatars);
                    self.comments.push(comment);
                }
                self.comments_total = data.total;
                self.comments_cursor = data.cursor;
                self.comments_end = data.end || self.comments_cursor.is_empty();
                self.comments_loading = false;
                self.comments_generation = self.comments_generation.wrapping_add(1);
                self.ui_dirty = true;
                log::line(&format!(
                    "comments loaded n={} total={} end={} reset={}",
                    self.comments.len(),
                    self.comments_total,
                    u8::from(self.comments_end),
                    u8::from(data.reset)
                ));
                if self.comments_reload_pending {
                    self.comments_reload_pending = false;
                    self.request_comments(true);
                }
            }
            DetailEvent::Comments(Err(error)) => {
                self.comments_loading = false;
                self.ui_dirty = true;
                log::line(&format!("comments failed reason={error}"));
                self.show_toast(&format!("评论加载失败：{error}"), true);
                if self.comments_reload_pending {
                    self.comments_reload_pending = false;
                    self.request_comments(true);
                }
            }
            DetailEvent::CommentReplies(Ok(data)) => {
                let Some(comment) = self
                    .comments
                    .iter_mut()
                    .find(|comment| comment.rpid == data.root_rpid)
                else {
                    self.ui_dirty = true;
                    return;
                };
                let mut seen: HashSet<i64> =
                    comment.replies.iter().map(|reply| reply.rpid).collect();
                for reply in data.replies {
                    if reply.rpid != 0 && !seen.insert(reply.rpid) {
                        continue;
                    }
                    let reply = reply_ui(reply, &self.avatars);
                    comment.replies.push(reply);
                }
                comment.replies_page = comment.replies_page.max(data.page);
                comment.replies_end = !data.has_more;
                comment.reply_count = comment.reply_count.max(data.total);
                comment.replies_loading = false;
                self.comments_generation = self.comments_generation.wrapping_add(1);
                self.ui_dirty = true;
                log::line(&format!(
                    "comment replies loaded root={} page={} n={} end={}",
                    data.root_rpid,
                    data.page,
                    comment.replies.len(),
                    u8::from(comment.replies_end)
                ));
            }
            DetailEvent::CommentReplies(Err(error)) => {
                for comment in self.comments.iter_mut() {
                    comment.replies_loading = false;
                }
                self.comments_generation = self.comments_generation.wrapping_add(1);
                self.ui_dirty = true;
                log::line(&format!("comment replies failed reason={error}"));
                self.show_toast(&format!("回复加载失败：{error}"), true);
            }
            DetailEvent::Avatars(list) => {
                for (url, image) in list {
                    self.avatars.insert(url.clone(), image.clone());
                    for comment in self.comments.iter_mut() {
                        if comment.avatar_url == url {
                            comment.avatar = Some(image.clone());
                        }
                        for reply in comment.replies.iter_mut() {
                            if reply.avatar_url == url {
                                reply.avatar = Some(image.clone());
                            }
                        }
                    }
                }
                self.ui_dirty = true;
            }
            DetailEvent::Toast { text, error } => self.show_toast(&text, error),
            DetailEvent::States {
                liked,
                coined,
                favorited,
            } => {
                self.liked = liked;
                self.coined = coined;
                self.favorited = favorited;
                self.ui_dirty = true;
            }
        }
    }

    fn set_speed(&mut self, speed: f64) {
        let speed = crate::speed::clamp(speed as f32);
        let shown = crate::speed::snap(speed);
        self.speed_preview = shown;
        self.speed_base = shown;
        self.speed_commit_at = None;
        if (speed - self.speed).abs() < 0.001 {
            return;
        }
        let position = self.position_now();
        self.speed = speed;
        self.shared
            .speed_milli
            .store((speed * 1000.0) as u32, Ordering::SeqCst);
        // The video reader's admission rate is speed-aware, so a speed change
        // needs a fresh decode session at the same picture position. The audio
        // engine gets the same speed through the Seek command.
        if let Err(error) = self.restart_at(position) {
            log::line(&format!("speed restart failed: {error:#}"));
        }
        self.state_changed();
        log::line(&format!("speed={speed:.2}"));
    }

    fn cycle_interp(&mut self) {
        if self.is_live {
            self.show_toast("直播不支持插帧", true);
            return;
        }
        let levels = self.interp_levels();
        let current = if self.interp == InterpMode::Off {
            0
        } else if self.interp_hz == 0 {
            levels.iter().position(|level| *level == Some(0)).unwrap_or(0)
        } else {
            levels
                .iter()
                .position(|level| *level == Some(self.interp_hz))
                .unwrap_or(0)
        };
        let next = levels[(current + 1) % levels.len()];
        self.select_interp_level(next);
    }

    /// Refresh rate of the monitor currently containing the player window.
    fn monitor_refresh_hz(&self) -> u32 {
        interp::display_refresh_hz_for_window(self.hwnd)
    }

    /// Ordered options for the 插帧 menu: 关闭, 自动, then the refresh rates
    /// the monitor under this window can actually pace.
    fn interp_levels(&self) -> Vec<Option<u32>> {
        interp::levels(self.monitor_refresh_hz())
    }

    fn interp_rows(&self) -> Vec<ChoiceRowUi> {
        self.interp_levels()
            .into_iter()
            .enumerate()
            .map(|(index, level)| {
                let (label, detail, current) = match level {
                    None => ("关闭".to_string(), "不插帧".to_string(), self.interp == InterpMode::Off),
                    Some(0) => (
                        "自动".to_string(),
                        "跟随屏幕刷新率".to_string(),
                        self.interp != InterpMode::Off && self.interp_hz == 0,
                    ),
                    Some(hz) => (
                        format!("{hz} Hz"),
                        "固定输出".to_string(),
                        self.interp != InterpMode::Off && self.interp_hz == hz,
                    ),
                };
                ChoiceRowUi {
                    index,
                    label,
                    detail,
                    current,
                }
            })
            .collect()
    }

    fn enhance_rows(&self) -> Vec<ChoiceRowUi> {
        [
            (EnhanceMode::Off, "不处理"),
            (EnhanceMode::Cas, "CAS 锐化"),
            (EnhanceMode::Anime, "Anime4K"),
        ]
        .into_iter()
        .map(|(mode, detail)| ChoiceRowUi {
            index: mode.as_u8() as usize,
            label: mode.label().to_string(),
            detail: detail.to_string(),
            current: !self.is_live && self.enhance == mode,
        })
        .collect()
    }

    /// Store the global 插帧 / 画质增强 choice. A change made in the native
    /// player must survive the next `player.open`; bumping the shared
    /// revision tells the engine to rebroadcast the refreshed settings.
    fn persist_interp_prefs(&mut self) {
        let mut stored = o11a1::settings::load();
        stored.interp_mode = self.interp.as_u8();
        stored.interp_hz = self.interp_hz;
        match o11a1::settings::save(&stored) {
            Ok(()) => {
                self.shared
                    .settings_revision
                    .fetch_add(1, Ordering::SeqCst);
            }
            Err(error) => log::line(&format!("interp prefs save failed: {error:#}")),
        }
    }

    fn persist_enhance_prefs(&mut self) {
        let mut stored = o11a1::settings::load();
        stored.player_enhance = self.enhance.as_u8() as u32;
        match o11a1::settings::save(&stored) {
            Ok(()) => {
                self.shared
                    .settings_revision
                    .fetch_add(1, Ordering::SeqCst);
            }
            Err(error) => log::line(&format!("enhance prefs save failed: {error:#}")),
        }
    }

    /// Re-apply the global 插帧 / 画质增强 choice after a new source is
    /// attached. The native menu and the settings page both write
    /// settings.json, so the file is the single truth between windows.
    fn reload_player_prefs(&mut self) {
        if self.is_live {
            return;
        }
        let stored = o11a1::settings::load();
        self.set_interp_hz(stored.interp_hz);
        self.apply_interp_mode(InterpMode::from_u8(stored.interp_mode));
        self.set_enhance(EnhanceMode::from_u8(stored.player_enhance as u8));
        log::line(&format!(
            "player prefs reloaded interp={} hz={} enhance={}",
            self.interp.as_u8(),
            self.interp_hz,
            self.enhance.as_u8()
        ));
    }

    /// One interpolation level selection from the toolbar menu or keyboard.
    /// `None` is 关闭; `Some(0)` is 自动 (display refresh); `Some(60/90/120)`
    /// is a fixed pacer target. Targets above the panel rate are rejected.
    fn select_interp_level(&mut self, level: Option<u32>) {
        if self.is_live {
            return;
        }
        if let Some(hz) = level {
            if !interp::level_supported(hz, self.monitor_refresh_hz()) {
                log::line(&format!("interp level rejected hz={hz}"));
                self.ui_dirty = true;
                return;
            }
            self.set_interp_hz(hz);
            self.apply_interp_mode(InterpMode::Gpu);
        } else {
            self.apply_interp_mode(InterpMode::Off);
        }
        self.persist_interp_prefs();
        log::line(&format!(
            "interp selected level={} hz={} out_hz={}",
            self.interp.label(),
            self.interp_hz,
            self.interp_out_hz.unwrap_or(0)
        ));
        self.ui_dirty = true;
    }

    /// Single entry point for every interpolation toggle (key, toolbar, RPC).
    /// Keeping the lookahead and the pacer tick here prevents a mode switch
    /// from dropping a due picture or leaving a stale grid behind.
    fn apply_interp_mode(&mut self, requested: InterpMode) {
        let requested = if self.is_live {
            InterpMode::Off
        } else {
            requested
        };
        // Honour the requested mode verbatim: a clip whose own rate already
        // fills the display simply has no output grid, it must not clear the
        // user's setting or flip the toolbar back to Off.
        let changed = self.interp != requested;
        self.interp = requested;
        self.shared
            .interp
            .store(requested.as_u8(), Ordering::SeqCst);
        self.interp_ticks = 0;
        self.interp_blends = 0;
        self.interp_cuts = 0;
        // Only start the pacer grid when one exists; a clip that already fills
        // the target keeps the mode on but plays through the regular
        // due-frame path, so a lookahead frame must not be parked forever.
        let grid = requested == InterpMode::Gpu && self.interp_out_hz.is_some();
        if grid {
            self.next = None;
            self.next_tick_at = Some(Instant::now());
            // A fresh session may already have its first source picture in the
            // queue. Parking it in `next` here used to strand it: the starting
            // branch only looked at the decoder queue, and `poll_interp` does
            // nothing until `cur` exists. Wait for the first picture to be
            // shown, then the tick fills the lookahead normally.
            if self.cur.is_some() {
                self.fill_next(self.position_now().saturating_add(INTERP_LOOKAHEAD_MS));
            }
        } else {
            // A lookahead frame may have been consumed from the queue; show it
            // now so switching never skips a source picture.
            if let Some(next) = self.next.take() {
                self.show_source(next);
            }
            self.next_tick_at = None;
        }
        if changed {
            log::line(&format!(
                "interp mode={} label={} out_hz={} source_fps={:.2}",
                self.interp.as_u8(),
                self.interp.label(),
                self.interp_out_hz.unwrap_or(0),
                self.source.fps
            ));
            self.ui_dirty = true;
        }
    }

    /// Point the interpolation pacer at a fixed target. `0` follows the
    /// display refresh; any other value is used as-is up to
    /// [`interp::INTERP_MAX_OUT_HZ`] and is deliberately NOT clamped to the
    /// display: if the compositor or the CPU cannot keep up, late ticks are
    /// dropped and the achieved rate settles wherever the machine can.
    fn set_interp_hz(&mut self, hz: u32) {
        self.interp_hz = if hz == 0 {
            0
        } else {
            hz.clamp(1, interp::INTERP_MAX_OUT_HZ)
        };
        let display_hz = self.monitor_refresh_hz();
        self.monitor_hz = display_hz;
        let plan = interp::interp_plan(display_hz, self.source.fps, self.interp_hz);
        let out_hz = plan.unwrap_or_else(|| {
            if self.source.fps.is_finite() && self.source.fps > 1.0 {
                self.source.fps.round().clamp(1.0, 240.0) as u32
            } else {
                60
            }
        });
        let previous = self.interp_out_hz;
        self.interp_out_hz = plan;
        self.out_period = Duration::from_secs_f64(1.0 / out_hz.max(1) as f64);
        self.interp_ticks = 0;
        self.interp_blends = 0;
        self.interp_cuts = 0;
        if plan.is_none() {
            // No output grid: release any parked lookahead picture so the
            // regular due-frame path never loses a source frame.
            if let Some(next) = self.next.take() {
                self.show_source(next);
            }
            self.next_tick_at = None;
        } else if self.interp == InterpMode::Gpu {
            self.next_tick_at = Some(Instant::now());
        }
        if previous != plan {
            log::line(&format!(
                "interp target={} display_hz={display_hz} out_hz={} source_fps={:.2}",
                self.interp_hz,
                plan.unwrap_or(0),
                self.source.fps
            ));
            self.ui_dirty = true;
        }
    }

    fn cycle_enhance(&mut self) {
        if self.is_live {
            self.show_toast("直播不支持画质增强", true);
            return;
        }
        self.select_enhance(self.enhance.next());
    }

    /// Show/hide the five-line debug panel at the video window's top-right.
    /// The switch lives in the sidebar's 性能 tab; the shortcut keys only flip
    /// the same flag and never move the sidebar.
    fn toggle_stats(&mut self) {
        self.stats = !self.stats;
        self.state.stats = self.stats;
        self.state_changed();
        self.ui_dirty = true;
        log::line(&format!("stats={}", u8::from(self.stats)));
    }

    fn set_enhance(&mut self, mode: EnhanceMode) {
        let mode = if self.is_live {
            EnhanceMode::Off
        } else {
            mode
        };
        if mode != EnhanceMode::Off && self.enhance_pipeline.is_none() {
            log::line(&format!(
                "enhance fallback requested={} reason=pipeline unavailable",
                mode.label()
            ));
            self.enhance = EnhanceMode::Off;
        } else {
            self.enhance = mode;
        }
        self.shared
            .enhance
            .store(self.enhance.as_u8(), Ordering::SeqCst);
        self.ui_dirty = true;
        log::line(&format!(
            "enhance mode={} label={}",
            self.enhance.as_u8(),
            self.enhance.label()
        ));
    }

    /// User-facing 画质增强 choice: apply it now, then store it globally so
    /// the next video starts with the same mode.
    fn select_enhance(&mut self, mode: EnhanceMode) {
        self.set_enhance(mode);
        self.persist_enhance_prefs();
        log::line(&format!(
            "enhance selected mode={} label={}",
            self.enhance.as_u8(),
            self.enhance.label()
        ));
    }

    fn set_aspect(&mut self, aspect: AspectMode) {
        if aspect != AspectMode::Fit && self.aspect == AspectMode::Fit {
            self.show_toast("视频比例已固定为原始比例", false);
        }
        self.aspect = AspectMode::Fit;
        self.shared
            .aspect
            .store(AspectMode::Fit.as_u8(), Ordering::SeqCst);
        self.update_video_rect();
        self.state_changed();
        self.ui_dirty = true;
        log::line(&format!(
            "aspect locked=fit requested={} client={}x{}",
            aspect.as_u8(),
            self.client.0,
            self.client.1
        ));
    }

    fn cycle_aspect(&mut self) {
        self.set_aspect(self.aspect.next());
    }

    fn set_ontop(&mut self, ontop: bool) {
        self.ontop = ontop;
        if !self.mini {
            if let Err(error) = window::set_topmost(self.hwnd, ontop) {
                log::line(&format!(
                    "ontop change failed enabled={} err={error:#}",
                    u8::from(ontop)
                ));
            }
        }
        let effective = window::is_topmost(self.hwnd);
        self.shared.topmost.store(effective, Ordering::SeqCst);
        self.state_changed();
        self.ui_dirty = true;
        log::line(&format!(
            "ontop={} effective={} mini={}",
            u8::from(ontop),
            u8::from(effective),
            u8::from(self.mini)
        ));
    }

    fn toggle_ontop(&mut self) {
        let desired = !self.ontop;
        self.set_ontop(desired);
    }

    fn set_brightness(&mut self, value: f32) {
        let value = value.clamp(0.4, 2.0);
        if (value - self.brightness).abs() < 0.001 {
            self.ui_dirty = true;
            return;
        }
        self.brightness = value;
        self.shared
            .brightness_milli
            .store((value * 1000.0).round() as u32, Ordering::SeqCst);
        self.ui_dirty = true;
        self.repaint_picture();
        self.state_changed();
        log::line(&format!(
            "brightness={value:.2} percent={:.0}",
            value * 100.0
        ));
    }

    fn step_brightness(&mut self, delta: f32) {
        self.set_brightness(self.brightness + delta);
    }

    /// QA-friendly presets: 100% → 60% → 150% → 200% → 100%.
    fn cycle_brightness(&mut self) {
        const PRESETS: [f32; 4] = [1.0, 0.6, 1.5, 2.0];
        let index = PRESETS
            .iter()
            .position(|preset| (preset - self.brightness).abs() < 0.001)
            .unwrap_or(0);
        self.set_brightness(PRESETS[(index + 1) % PRESETS.len()]);
    }

    fn set_audio_only(&mut self, audio_only: bool) {
        if self.audio_only == audio_only {
            self.ui_dirty = true;
            return;
        }
        self.audio_only = audio_only;
        self.shared.audio_only.store(audio_only, Ordering::SeqCst);
        self.state_changed();
        self.ui_dirty = true;
        if audio_only {
            if let Err(error) = self.video.present_black(&self.gpu) {
                log::line(&format!("audio-only black frame failed: {error:#}"));
            }
        } else {
            // Put the held picture back immediately when paused.
            self.repaint_picture();
        }
        log::line(&format!("audio_only={}", u8::from(audio_only)));
    }

    fn toggle_audio_only(&mut self) {
        let desired = !self.audio_only;
        self.set_audio_only(desired);
    }

    fn cycle_speed(&mut self, direction: i32) {
        const SPEEDS: [f32; 9] = [0.1, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 10.0];
        let index = SPEEDS
            .iter()
            .position(|speed| (speed - self.speed).abs() < 0.001)
            .unwrap_or(4);
        let next = (index as i32 + direction).rem_euclid(SPEEDS.len() as i32) as usize;
        self.set_speed(SPEEDS[next] as f64);
    }

    /// Re-present the held frame so brightness/audio-only changes are visible
    /// even while paused. Never changes the playback clock.
    fn repaint_picture(&mut self) {
        if self.cur.is_some() {
            // `present_current` handles the audio-only black frame and the
            // enhancement fallback bookkeeping in one place.
            self.present_current();
        } else if self.audio_only {
            if let Err(error) = self.video.present_black(&self.gpu) {
                log::line(&format!("audio-only black frame failed: {error:#}"));
            }
        }
    }

    fn screenshot(&mut self) {
        let path = screenshot_path();
        match window::capture(self.hwnd, &path) {
            Ok(()) => log::line(&format!("screenshot path={}", path.display())),
            Err(error) => log::line(&format!(
                "screenshot failed path={} err={error:#}",
                path.display()
            )),
        }
    }

    // ------------------------------------------------------------- the loop

    fn run_loop(&mut self) -> Result<()> {
        log::line(&format!(
            "window open hwnd={:?} client={}x{} scale={:.2} size={}x{}",
            self.hwnd.0,
            self.client.0,
            self.client.1,
            self.scale,
            self.source.width,
            self.source.height
        ));
        let mut last_status = String::new();
        while !self.quit {
            window::pump();
            // Advance the morph before this turn's events: an open/close
            // request then draws its first frame at the current mix instead
            // of jumping one animation step ahead.
            self.advance_precision_anim();
            for event in self.events.drain() {
                self.handle_event(event);
            }
            loop {
                match self.commands.try_recv() {
                    Ok(command) => self.handle_command(command),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.quit = true;
                        break;
                    }
                }
            }
            if self.quit {
                break;
            }
            if self.source_pending {
                match self.source_rx.try_recv() {
                    Ok(Ok(open)) => {
                        self.source_pending = false;
                        self.apply_deferred_source(open);
                        self.pending_replace = None;
                        self.failed_replace = None;
                        if let Some(next) = self.queued_replace.take() {
                            self.begin_source_replace(next);
                        }
                    }
                    Ok(Err(message)) => {
                        self.source_pending = false;
                        self.loading = false;
                        if self.pending_replace.is_some() {
                            self.failed_replace = self.pending_replace.take();
                        }
                        self.report_error(format!("打开视频失败：{message}"));
                        if let Some(next) = self.queued_replace.take() {
                            self.begin_source_replace(next);
                        }
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        self.source_pending = false;
                        self.loading = false;
                        if self.pending_replace.is_some() {
                            self.failed_replace = self.pending_replace.take();
                        }
                        self.report_error("打开视频失败：解析线程已退出");
                    }
                }
            }
            loop {
                match self.danmaku_rx.try_recv() {
                    Ok(items) => self.install_danmaku(items),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            loop {
                match self.detail_rx.try_recv() {
                    Ok(event) => self.handle_detail(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
            loop {
                match self.storyboard_rx.as_ref().map(|rx| rx.try_recv()) {
                    Some(Ok(event)) => self.handle_storyboard(event),
                    Some(Err(TryRecvError::Empty)) | None => break,
                    Some(Err(TryRecvError::Disconnected)) => {
                        self.storyboard_rx = None;
                        break;
                    }
                }
            }
            let track_result = self.track_rx.as_ref().and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("画质解析线程已结束".to_string())),
            });
            if let Some(result) = track_result {
                self.track_rx = None;
                let kind = self.track_kind.take().unwrap_or("播放流");
                match result {
                    Ok(source) => self.apply_track_switch(source),
                    Err(error) => {
                        log::line(&format!("{kind} switch failed reason={error}"));
                        self.show_toast(&format!("{kind}切换失败：{error}"), true);
                    }
                }
            }
            let switch_result = self.switch_rx.as_ref().and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => {
                    let timed_out = self
                        .switch_started
                        .map(|at| at.elapsed() >= Duration::from_secs(25))
                        .unwrap_or(false);
                    if timed_out {
                        log::line("source switch timeout");
                        Some(Err("分P解析超时".to_string()))
                    } else {
                        None
                    }
                }
                Err(TryRecvError::Disconnected) => Some(Err("分P解析线程已结束".to_string())),
            });
            if let Some(result) = switch_result {
                self.switch_rx = None;
                self.switch_started = None;
                match result {
                    Ok(source) => self.apply_switched_source(source),
                    Err(error) => {
                        self.switch_target = None;
                        self.page_loading = None;
                        log::line(&format!("source switch failed reason={error}"));
                        self.show_toast(&format!("分P切换失败：{error}"), true);
                        // The click already stopped the old picture/audio, so a
                        // bare toast would leave a dead loading card. Put the
                        // previous page back at the position it had.
                        self.restore_previous_page(Some(&error));
                    }
                }
            }
            self.run_qa();
            self.poll_pending_wheel_seek();
            self.poll_pending_speed();
            self.poll_danmaku_long_press();
            self.poll_history();
            self.tick_undo();
            let action_result = self.dm_action_rx.try_recv();
            match action_result {
                Ok(result) => self.handle_dm_action(result),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {}
            }
            self.tick_player_state();
            self.tick_volume_wheel();
            self.tick_heartbeat();
            self.tick_sleep();
            self.tick_monitor_refresh();
            self.check_sessions();
            if let Some(limit) = self.auto_exit {
                if self.started_at.elapsed() >= limit {
                    log::line(&format!("auto-exit seconds={:.3}", limit.as_secs_f64()));
                    break;
                }
            }

            self.flush_live_resize();
            self.poll_picture();
            self.draw_danmaku();

            // Status line follows the state, not every frame.
            let wanted_status = self.wanted_status();
            if wanted_status != last_status {
                last_status = wanted_status.clone();
                self.status = wanted_status;
                self.ui_dirty = true;
            }

            self.position_ms = self.position_now();
            self.poll_ab_prefetch();
            self.enforce_ab_loop();
            self.animate_sidebar_scroll();
            self.advance_comment_animations();
            self.poll_comments_autoload();
            self.shared
                .position_ms
                .store(self.position_ms, Ordering::SeqCst);

            if self.chrome_shown
                && !self.dragging
                && self.popup_drag.is_none()
                && !self.sidebar_open
                && !self.danmaku_popup
                && self.dm_menu.is_none()
                && self.last_mouse_activity.elapsed() >= Duration::from_secs(2)
            {
                self.chrome_shown = false;
                log::line(&format!(
                    "chrome hidden idle_ms={}",
                    self.last_mouse_activity.elapsed().as_millis()
                ));
                self.ui_dirty = true;
            }
            if let Some(rx) = self.dm_send_rx.take() {
                match rx.try_recv() {
                    Ok((code, message)) => {
                        if code == 0 {
                            self.dm_input.clear();
                        }
                        self.show_toast(&message, code != 0);
                        self.ui_dirty = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        self.dm_send_rx = Some(rx);
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.show_toast("发送弹幕失败：后台线程已退出", true);
                        self.ui_dirty = true;
                    }
                }
            }
            if self
                .toast_until
                .map(|until| Instant::now() >= until)
                .unwrap_or(false)
            {
                self.toast = None;
                self.toast_until = None;
                self.ui_dirty = true;
            }
            // The overlay runs on an adaptive grid: 60–120 Hz only while the
            // toolbar/sidebar/menu/OSD is actually animating, 10 Hz when the
            // chrome is hidden and the picture is just playing. The old
            // unconditional monitor-rate D2D redraw cost 2–4 ms every 8.3 ms
            // on a 120 Hz panel, stealing a large slice of the same core the
            // 4K frame upload needs.
            let now = Instant::now();
            let fast_ui = self.ui_fast();
            let hz = if fast_ui {
                self.monitor_hz.clamp(60, 120) as f32
            } else {
                10.0
            };
            let period = Duration::from_secs_f32(1.0 / hz);
            if fast_ui && self.next_ui_draw > now + period {
                self.next_ui_draw = now;
            }
            if self.ui_dirty || now >= self.next_ui_draw {
                let draw_at = Instant::now();
                if let Err(error) = self.draw_ui() {
                    log::line(&format!("overlay draw failed: {error:#}"));
                }
                self.ui_draw_us_total = self
                    .ui_draw_us_total
                    .saturating_add(draw_at.elapsed().as_micros() as u64);
                self.ui_draw_count = self.ui_draw_count.saturating_add(1);
                if now >= self.next_ui_draw {
                    self.next_ui_draw += period;
                    if self.next_ui_draw <= now {
                        self.next_ui_draw = now + period;
                    }
                }
                self.ui_dirty = false;
            }

            if self.fps_window_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = self.fps_window_start.elapsed().as_secs_f64();
                self.fps = self.presents_window as f32 / elapsed as f32;
                let danmaku_frames = self.danmaku_layer.frames;
                self.danmaku_fps =
                    danmaku_frames.saturating_sub(self.danmaku_frames_prev) as f32 / elapsed as f32;
                self.danmaku_frames_prev = danmaku_frames;
                let ui_presents = self.overlay.presents();
                self.ui_fps =
                    ui_presents.saturating_sub(self.ui_presents_prev) as f32 / elapsed as f32;
                self.ui_presents_prev = ui_presents;
                let ui_draw_us = if self.ui_draw_count > 0 {
                    self.ui_draw_us_total / self.ui_draw_count
                } else {
                    0
                };
                self.ui_draw_us_total = 0;
                self.ui_draw_count = 0;
                let (queue, dropped) = self
                    .decoder
                    .as_ref()
                    .map(|decoder| (decoder.queue.len(), decoder.queue.dropped()))
                    .unwrap_or((0, 0));
                let audio_pos = self
                    .audio
                    .as_ref()
                    .map(|audio| audio.clock.position_ms.load(Ordering::SeqCst))
                    .unwrap_or(0);
                self.queue_len = queue;
                self.dropped = dropped;
                self.audio_ms = audio_pos;
                let net_seconds = self.net_at.elapsed().as_secs_f64().max(0.001);
                let (net_pid, net_read) = self
                    .decoder
                    .as_ref()
                    .map(|decoder| {
                        let pid = decoder.pid();
                        (pid, crate::decode::process_read_bytes(pid))
                    })
                    .unwrap_or((0, None));
                if net_pid != 0 && net_pid == self.net_pid {
                    if let Some(bytes) = net_read {
                        if bytes >= self.net_read {
                            let seconds = net_seconds;
                            self.net_bps = (bytes - self.net_read) as f64 / seconds;
                            let total_bps = self.source.video_bitrate + self.source.audio_bitrate;
                            self.net_lead_s = if total_bps > 0 {
                                let bytes_per_media_second = total_bps as f64 / 8.0;
                                let read_delta =
                                    bytes.saturating_sub(self.net_base_read) as f64;
                                let media_seconds = self
                                    .position_ms
                                    .saturating_sub(self.net_base_pos)
                                    as f64
                                    / 1000.0;
                                (read_delta / bytes_per_media_second - media_seconds).max(0.0)
                            } else {
                                0.0
                            };
                        } else {
                            self.net_bps = 0.0;
                            self.net_lead_s = 0.0;
                        }
                        self.net_read = bytes;
                    } else {
                        self.net_bps = 0.0;
                        self.net_lead_s = 0.0;
                    }
                } else {
                    self.net_pid = net_pid;
                    self.net_read = net_read.unwrap_or(0);
                    self.net_base_read = self.net_read;
                    self.net_base_pos = self.position_ms;
                    self.net_bps = 0.0;
                    self.net_lead_s = 0.0;
                }
                // Cached media means the decoder reads loopback, not the CDN.
                // Report the cache workers' real network throughput instead.
                if crate::source::media_cache_key(&self.source, false).is_some() {
                    let cache_bytes = crate::mediacache::network_bytes();
                    if self.cache_net_read == 0 {
                        self.cache_net_read = cache_bytes;
                    }
                    let delta = cache_bytes.saturating_sub(self.cache_net_read);
                    self.net_bps = delta as f64 / net_seconds;
                    self.cache_net_read = cache_bytes;
                    let total_bps = self.source.video_bitrate + self.source.audio_bitrate;
                    if total_bps > 0 {
                        let mut done = 0u64;
                        let mut total = 0u64;
                        for audio in [false, true] {
                            if let Some(key) =
                                crate::source::media_cache_key(&self.source, audio)
                            {
                                if let Some((key_done, key_total)) =
                                    crate::mediacache::progress(&key)
                                {
                                    done = done.saturating_add(key_done);
                                    total = total.saturating_add(key_total);
                                }
                            }
                        }
                        if total > 0 {
                            self.cache_done = done;
                            self.cache_total = total;
                            let bytes_per_media_second = total_bps as f64 / 8.0;
                            self.net_lead_s = (done as f64 / bytes_per_media_second
                                - self.position_ms as f64 / 1000.0)
                                .max(0.0);
                        } else {
                            self.cache_done = 0;
                            self.cache_total = 0;
                        }
                    }
                } else {
                    self.cache_done = 0;
                    self.cache_total = 0;
                }
                // Keep the cache anchored to the picture, not to FFmpeg's
                // much farther read-ahead cursor. Also report whether the
                // workers are actually beating the stream's bitrate.
                if let Some((key, offset)) = self.cache_byte_offset() {
                    crate::mediacache::set_playhead(&key, offset);
                    let total_bps = self.source.video_bitrate + self.source.audio_bitrate;
                    if total_bps > 0
                        && self.cache_total > 0
                        && self.cache_done < self.cache_total
                    {
                        let ratio = (self.net_bps * 8.0) / total_bps as f64;
                        let due = self
                            .last_cache_rate_log
                            .map(|at| at.elapsed() >= Duration::from_secs(5))
                            .unwrap_or(true);
                        if due {
                            self.last_cache_rate_log = Some(Instant::now());
                            log::line(&format!(
                                "cache rate mbps={:.2} demand_mbps={:.2} ratio={:.2} lead_s={:.1} done={}/{}",
                                self.net_bps * 8.0 / 1_000_000.0,
                                total_bps as f64 / 1_000_000.0,
                                ratio,
                                self.cache_lead_ms().map(|ms| ms as f64 / 1000.0).unwrap_or(0.0),
                                self.cache_done,
                                self.cache_total
                            ));
                        }
                    }
                }
                self.net_at = Instant::now();
                let interp_ticks = self.interp_ticks;
                let interp_blends = self.interp_blends;
                let interp_cuts = self.interp_cuts;
                self.interp_ticks = 0;
                self.interp_blends = 0;
                self.interp_cuts = 0;
                log::line(&format!(
                    "fps presented={} rate={:.2} source_fps={:.2} out_hz={} interp={} enhance={} queue={queue} dropped={dropped} pos_ms={} audio_ms={audio_pos} av_gap={} danmaku={}/{} dm_frames={} dm_fps={:.2} ui_fps={:.2} ui_draw_us={ui_draw_us} ticks={interp_ticks} blends={interp_blends} cuts={interp_cuts}",
                    self.presents_window,
                    self.fps,
                    self.source.fps,
                    self.interp_out_hz.unwrap_or(0),
                    self.interp.label(),
                    self.enhance.label(),
                    self.position_ms,
                    self.position_ms.saturating_sub(self.last_frame_ts),
                    self.danmaku.total(),
                    self.danmaku.active_count(),
                    self.danmaku_layer.frames,
                    self.danmaku_fps,
                    self.ui_fps
                ));
                self.shared
                    .fps_tenths
                    .store((self.fps * 10.0).round() as u32, Ordering::SeqCst);
                // Interpolation is never disabled automatically. If it costs
                // too much on this machine the user turns it off themselves.
                self.presents_window = 0;
                self.fps_window_start = Instant::now();
            }

            window::tiny_sleep();
        }
        Ok(())
    }

    fn wanted_status(&self) -> String {
        if self.loading {
            return "正在打开视频…".to_string();
        }
        if self.ended {
            return "播放结束".to_string();
        }
        if self.seek_in_flight {
            return "定位中…".to_string();
        }
        if self.stalled {
            return "正在缓冲…".to_string();
        }
        if self.starting {
            if let Some(page) = self.page_loading {
                return format!("正在加载第 {} P…", page + 1);
            }
            return if self.cur.is_some() {
                "正在缓冲…".to_string()
            } else {
                "正在加载…".to_string()
            };
        }
        if !self.playing {
            return "已暂停".to_string();
        }
        if let Some(audio) = &self.audio {
            if self.audio_master && audio.clock.starved_ms.load(Ordering::SeqCst) > 350 {
                return "正在缓冲…".to_string();
            }
        }
        String::new()
    }

    /// Replay a cached A–B pass. No decoder/network work happens here: the
    /// first pass captured these pictures, and every loop reuses them.
    fn poll_ab_cache(&mut self) {
        if !self.playing {
            return;
        }
        let Some((a, b)) = self.ab_range() else {
            self.ab_cache_playing = false;
            self.ab_cache_cursor = 0;
            return;
        };
        let position = self.position_now();
        self.position_ms = position;
        self.shared.position_ms.store(position, Ordering::SeqCst);
        if position >= b {
            self.ab_cache_cursor = 0;
            self.reset_loop_clock(a);
            self.gate(false);
            log::line(&format!("ab cache loop a={a}"));
            return;
        }
        if self.interp == InterpMode::Gpu && self.interp_out_hz.is_some() {
            self.poll_interp(position);
            return;
        }
        let mut next_frame = None;
        if let Some(cache) = self.ab_cache.as_mut() {
            while self.ab_cache_cursor < cache.frames.len() {
                let frame = &cache.frames[self.ab_cache_cursor];
                if frame.ts_ms > position.saturating_add(6) {
                    break;
                }
                next_frame = Some(DecodedFrame {
                    data: frame.data.clone(),
                    ts_ms: frame.ts_ms,
                });
                self.ab_cache_cursor += 1;
            }
        }
        if let Some(frame) = next_frame {
            self.display_new(frame, false);
        }
    }

    /// Contiguous cached video ahead of the current position, in milliseconds.
    /// `None` when the source is not cached or the range is served through the
    /// CDN proxy, where the local lead is meaningless.
    fn cache_byte_offset(&self) -> Option<(String, u64)> {
        if !self.source.remote || self.source.video_bitrate == 0 {
            return None;
        }
        let key = crate::source::media_cache_key(&self.source, false)?;
        let bytes_per_ms = self.source.video_bitrate as u128 / 8_000;
        if bytes_per_ms == 0 {
            return None;
        }
        let offset = (self.position_ms as u128 * bytes_per_ms) as u64;
        Some((key, offset))
    }

    fn cache_lead_ms(&self) -> Option<u64> {
        let (key, offset) = self.cache_byte_offset()?;
        let lead = crate::mediacache::lead_bytes(&key, offset)?;
        let bytes_per_ms = self.source.video_bitrate as u128 / 8_000;
        if bytes_per_ms == 0 {
            return None;
        }
        Some((lead as u128 / bytes_per_ms) as u64)
    }

    /// Fill the chunk under the presentation clock before the cursor window.
    fn prioritize_cache(&self) {
        if let Some((key, offset)) = self.cache_byte_offset() {
            crate::mediacache::prioritize(&key, offset);
        }
    }

    /// Whether the current video representation is fully on disk. Used to hide
    /// the live transfer speed once there is nothing left to download.
    fn current_cache_complete(&self) -> bool {
        let Some(key) = crate::source::media_cache_key(&self.source, false) else {
            return false;
        };
        crate::mediacache::progress(&key)
            .map(|(done, total)| total > 0 && done >= total)
            .unwrap_or(false)
    }

    /// How many seconds of the next part are already in its cache. `None`
    /// before the ten-second prefetcher starts; `Some(0.0)` while its source is
    /// still resolving or its first range request is in flight.
    fn next_page_preload_s(&self) -> Option<f64> {
        if self.next_page_prefetch.is_none() {
            return None;
        }
        let slot = self.next_page_source.lock().ok()?;
        let Some((_, source)) = slot.as_ref() else {
            return Some(0.0);
        };
        let key = crate::source::media_cache_key(source, false)?;
        let bytes = crate::mediacache::lead_bytes(&key, 0).unwrap_or(0);
        let bps = source.video_bitrate;
        if bps == 0 {
            return Some(0.0);
        }
        let seconds = bytes as f64 * 8.0 / bps as f64;
        let duration = source.duration_ms as f64 / 1000.0;
        Some(if duration > 0.0 {
            seconds.min(duration)
        } else {
            seconds
        })
    }

    /// Take decoded frames due on the current presentation clock and present
    /// them; also drive the interpolation grid.
    fn poll_picture(&mut self) {
        if self.ab_cache_playing {
            self.poll_ab_cache();
            return;
        }
        if self.decoder.is_none() {
            return;
        }
        if self.ab_swap_wait.is_some() {
            // Hold the last frame while the next A pass warms. Presenting the
            // old decoder's post-B frames here would show a jump past B.
            return;
        }
        if self.video_resync.is_some() {
            // The decoder was jumped to the sound position. This runs even
            // while the engine is paused so the new picture is ready before
            // play resumes.
            self.poll_video_resync();
            return;
        }
        if let Some(until) = self.stall_until {
            if Instant::now() < until {
                self.stalled = true;
                self.ui_dirty = true;
            } else {
                self.stall_until = None;
            }
        }
        if self.seek_in_flight {
            let frame = self
                .decoder
                .as_ref()
                .and_then(|decoder| decoder.queue.take_first());
            if let Some(frame) = frame {
                let generation = self
                    .decoder
                    .as_ref()
                    .map(|decoder| decoder.generation)
                    .unwrap_or(self.generation);
                self.display_new(frame, true);
                self.seek_in_flight = false;
                self.base_ms = self.seek_target;
                self.anchor = Some(Instant::now());
                self.paused_at = self.seek_target;
                self.ended = false;
                self.audio_master = false;
                self.first_frame_at = Some(Instant::now());
                self.had_first_frame = true;
                if self.requested_play {
                    // Hold the new first picture until the sound session is
                    // ready, then resume both from the same target. Starting
                    // the picture immediately made the sound land ~1 s behind
                    // (the new ffmpeg audio session's spawn time), which is
                    // exactly the lip-sync failure the drift guard then has to
                    // report.
                    self.starting = true;
                    self.playing = false;
                    self.gate(false);
                    if let Some(audio) = &self.audio {
                        audio.send(AudioCommand::Pause);
                    }
                    self.status = "正在缓冲…".to_string();
                } else {
                    self.starting = false;
                    self.playing = false;
                    self.gate(false);
                    if let Some(audio) = &self.audio {
                        audio.send(AudioCommand::Pause);
                    }
                    self.status.clear();
                }
                self.ui_dirty = true;
                log::line(&format!(
                    "seek landed target_ms={} gen={generation} playing={} holding={}",
                    self.seek_target,
                    u8::from(self.requested_play),
                    u8::from(self.starting)
                ));
                self.sync_shared();
            } else {
                // A seek past the real end of the DASH track opens a decoder
                // that immediately EOFs with no picture. Waiting for that
                // picture left the player frozen at the target (and stopped
                // 下一分P from ever firing) — finish the stream instead.
                let closed = self
                    .decoder
                    .as_ref()
                    .map(|decoder| decoder.queue.is_closed())
                    .unwrap_or(true);
                if closed {
                    log::line(&format!(
                        "seek past end target_ms={} duration_ms={}",
                        self.seek_target, self.duration_ms
                    ));
                    self.seek_in_flight = false;
                    self.stalled = false;
                    self.stall_audio_paused = false;
                    let target = self.duration_ms.max(self.seek_target);
                    self.position_ms = target;
                    self.paused_at = target;
                    self.shared.position_ms.store(target, Ordering::SeqCst);
                    self.finish_stream(target);
                }
            }
            return;
        }
        if self.starting {
            let frame = self
                .decoder
                .as_ref()
                .and_then(|decoder| decoder.queue.take_first());
            if let Some(frame) = frame {
                let generation = self
                    .decoder
                    .as_ref()
                    .map(|decoder| decoder.generation)
                    .unwrap_or(self.generation);
                self.display_new(frame, true);
                self.first_frame_at = Some(Instant::now());
                self.had_first_frame = true;
                self.ui_dirty = true;
                let audio_ready = self
                    .audio
                    .as_ref()
                    .map(|audio| audio.clock.ready.load(Ordering::SeqCst))
                    .unwrap_or(false);
                log::line(&format!(
                    "first-frame gen={generation} size={}x{} ts_ms={} audio_ready={}",
                    self.source.width,
                    self.source.height,
                    self.last_frame_ts,
                    u8::from(audio_ready)
                ));
            }
            if self.cur.is_some() {
                let waited = self
                    .first_frame_at
                    .map(|at| at.elapsed())
                    .unwrap_or_default();
                let (audio_ready, _audio_available) = self
                    .audio
                    .as_ref()
                    .map(|audio| {
                        (
                            audio.clock.ready.load(Ordering::SeqCst),
                            audio.clock.available.load(Ordering::SeqCst),
                        )
                    })
                    .unwrap_or((false, false));
                if audio_ready || waited >= Duration::from_millis(START_AUDIO_WAIT_MS) {
                    self.starting = false;
                    self.base_ms = self.seek_target;
                    self.anchor = Some(Instant::now());
                    self.paused_at = self.seek_target;
                    self.playing = self.requested_play;
                    self.next = None;
                    self.next_tick_at = Some(Instant::now());
                    if self.playing {
                        self.on_playback_started();
                    }
                    self.gate(self.requested_play);
                    if let Some(audio) = &self.audio {
                        audio.send(if self.playing {
                            AudioCommand::Resume
                        } else {
                            AudioCommand::Pause
                        });
                    }
                    log::line(&format!(
                        "started playing={} waited_ms={} audio_ready={}",
                        u8::from(self.playing),
                        waited.as_millis(),
                        u8::from(audio_ready)
                    ));
                    self.ui_dirty = true;
                    self.sync_shared();
                }
            }
            return;
        }
        if !self.playing {
            return;
        }

        self.maybe_enable_audio_master();
        let now = Instant::now();
        let position = self.position_now();
        self.position_ms = position;
        self.shared.position_ms.store(position, Ordering::SeqCst);
        self.maybe_prefetch_next_page(position);
        self.log_drift_if_needed();

        let (queue_closed_now, queue_len_now) = self
            .decoder
            .as_ref()
            .map(|decoder| (decoder.queue.is_closed(), decoder.queue.len()))
            .unwrap_or((true, 0));
        let at_end = queue_closed_now
            && queue_len_now == 0
            && (self.duration_ms == 0 || position + 800 >= self.duration_ms);
        // Decoder/reader dry: stop the device clock and show the buffer card
        // instead of letting audio run away from a frozen picture. This is the
        // lip-sync guard for a network slower than the video bitrate.
        //
        // The old guard waited for `queue == 0`, by which point the presenter
        // had already consumed the last ~200 ms of pictures and the sound was
        // up to a second ahead. It then needed a video catch-up seek, which
        // dropped content and repeated at every cache boundary. Pausing at two
        // frames keeps the audio lead inside one waveOut quantum, so the two
        // clocks can resume together at the same position.
        let no_picture_for = self
            .last_source_present
            .map(|at| at.elapsed())
            .unwrap_or_default();
        let buffer_low = buffer_is_low(queue_len_now, no_picture_for.as_millis() as u64);
        if self.audio_position().is_some()
            && !self.audio_only
            && !at_end
            && buffer_low
            && !self.stall_audio_paused
        {
            self.stall_audio_paused = true;
            self.stall_pause = Some(StallPause {
                first_frame_at: None,
                since: Instant::now(),
            });
            self.stalled = true;
            self.ui_dirty = true;
            if let Some(audio) = &self.audio {
                audio.send(AudioCommand::Pause);
            }
            self.prioritize_cache();
            log::line(&format!(
                "buffer pause video-low position_ms={position} queue={queue_len_now} no_picture_ms={} audio_ms={} lead_ms={:?}",
                no_picture_for.as_millis(),
                self.audio
                    .as_ref()
                    .map(|audio| audio.clock.position_ms.load(Ordering::SeqCst))
                    .unwrap_or(0),
                self.cache_lead_ms()
            ));
        }

        // 插帧 owns the presentation grid: it consumes source frames in order,
        // keeps one frame of lookahead and emits 120 Hz ticks (blends included).
        // With interpolation off the due-frame path is kept, but its window
        // follows the source cadence: a fixed 6 ms window made a 4K60 picture
        // drop every frame the compositor delivered a hair late, which read as
        // ~47 fps even though nothing had failed.
        let interp_on =
            !self.interacting && self.interp == InterpMode::Gpu && self.interp_out_hz.is_some();
        let due_window = if self.source.fps.is_finite() && self.source.fps > 1.0 {
            (1000.0 / self.source.fps / 2.0).round().clamp(6.0, 24.0) as u64
        } else {
            6
        };
        let max_lag = self
            .source
            .fps
            .is_finite()
            .then_some((3000.0 / self.source.fps).round().clamp(30.0, 120.0) as u64)
            .unwrap_or(60);
        let frame = if interp_on {
            None
        } else {
            self.decoder.as_ref().and_then(|decoder| {
                decoder
                    .queue
                    .take_due_in_order(position, due_window, max_lag)
            })
        };
        let (queue_closed, queue_len) = self
            .decoder
            .as_ref()
            .map(|decoder| (decoder.queue.is_closed(), decoder.queue.len()))
            .unwrap_or((true, 0));
        let mut had_frame = frame.is_some();
        if let Some(frame) = frame {
            self.display_new(frame, false);
        }
        if interp_on && self.poll_interp(position) {
            had_frame = true;
        }

        // The sound is the reference. Resuming it onto a picture that is
        // still far behind is what turned one buffering stall into permanent
        // lip desync, so the pause stays until the picture catches up.
        let lag = position.saturating_sub(self.last_frame_ts);
        let frames_flowing = queue_len > 0
            || self
                .last_source_present
                .map(|at| at.elapsed() <= Duration::from_millis(600))
                .unwrap_or(false);
        if self.interacting {
            self.video_lag_since = None;
        } else if self.stall_audio_paused {
            let queue_ready = queue_len >= BUFFER_RESUME_FRAMES;
            let lead_ready = self
                .cache_lead_ms()
                .map(|lead| lead >= BUFFER_LEAD_MS)
                .unwrap_or(true);
            let waited_out = self
                .stall_pause
                .as_ref()
                .map(|pause| pause.since.elapsed() >= Duration::from_millis(BUFFER_WAIT_MAX_MS))
                .unwrap_or(true);
            if queue_ready && (lead_ready || waited_out) && lag <= AV_VIDEO_LAG_MS {
                self.resume_audio_with_picture(position);
            } else if queue_ready && (lead_ready || waited_out) {
                // The decoder is alive again but cannot reach the frozen
                // clock (hardware decode below real time). Give it a short
                // grace to skip the backlog, then jump the picture there
                // instead of holding the sound for tens of seconds.
                let grace_over = self
                    .stall_pause
                    .as_ref()
                    .and_then(|pause| pause.first_frame_at)
                    .map(|at| at.elapsed() >= Duration::from_millis(AV_STALL_CATCHUP_MS))
                    .unwrap_or(false);
                if grace_over
                    && self
                        .last_av_resync
                        .map(|at| at.elapsed() >= Duration::from_millis(AV_CATCHUP_COOLDOWN_MS))
                        .unwrap_or(true)
                {
                    self.start_video_catch_up(position);
                    return;
                }
            }
            // Queue still refilling: stay paused. A network stall is not a
            // reason to seek; seeking here would skip content and land on the
            // same missing chunk again.
        } else if self.playing
            && !self.audio_only
            && self.audio_position().is_some()
            && frames_flowing
            && lag > AV_VIDEO_LAG_MS
        {
            // A decoder that cannot keep up presents every picture it manages
            // to produce, so the video timeline advances at decode speed while
            // the sound runs in real time: slow motion and a gap that keeps
            // growing. Ordinary due-frame skipping cannot recover frames that
            // were never decoded. If the gap persists, jump the picture to the
            // sound and drop the stretch that could not be decoded.
            let held = self
                .video_lag_since
                .get_or_insert_with(Instant::now)
                .elapsed();
            if picture_lag_requires_catch_up(lag, frames_flowing, held)
                && self
                    .last_av_resync
                    .map(|at| at.elapsed() >= Duration::from_millis(AV_CATCHUP_COOLDOWN_MS))
                    .unwrap_or(true)
            {
                self.start_video_catch_up(position);
                return;
            }
        } else {
            self.video_lag_since = None;
        }
        // A queue with frames but no presented picture for two seconds means
        // the reader and the presentation clock disagree (or the clock
        // stopped); log it instead of silently freezing. Normal inter-frame
        // gaps never reach this threshold.
        if had_frame && self.stall_until.is_none() && !self.stall_audio_paused {
            self.last_stall_log = None;
            if self.stalled {
                self.stalled = false;
                self.ui_dirty = true;
            }
        } else if queue_len > 0 {
            let stalled_for = self
                .last_source_present
                .map(|at| at.elapsed())
                .unwrap_or_default();
            if stalled_for >= Duration::from_secs(2) {
                if !self.stalled {
                    self.stalled = true;
                    self.ui_dirty = true;
                    log::line(&format!(
                        "stall overlay position_ms={position} queue={queue_len}"
                    ));
                }
                let due = self
                    .last_stall_log
                    .map(|at| at.elapsed() >= Duration::from_secs(2))
                    .unwrap_or(true);
                if due {
                    self.last_stall_log = Some(now);
                    let front_ts = self
                        .decoder
                        .as_ref()
                        .and_then(|decoder| decoder.queue.front_ts());
                    log::line(&format!(
                        "picture stalled position_ms={position} queue={queue_len} front_ts={:?} playing={} starting={} seek={}",
                        front_ts,
                        u8::from(self.playing),
                        u8::from(self.starting),
                        u8::from(self.seek_in_flight)
                    ));
                }
            }
        }

        // End of stream: the reader has closed, the ring is empty, and the
        // clock has reached the tail (or no duration is known). While a
        // source switch is already in flight the tail must not be handled
        // again: `switch_page` returns early, so every loop turn would log and
        // retry until the worker answers.
        if queue_closed && queue_len == 0 && self.switch_rx.is_none() {
            // Bilibili's DASH video track often stops 0.5–2 s before the
            // duration in the playurl metadata. Waiting for `position + 800`
            // to pass the metadata duration then deadlocks at the tail: the
            // clock is frozen because the audio device has nothing left, so
            // 下一分P never runs and the window sits on the loading/buffer
            // card. The last decoded picture is the reliable signal here.
            let tail_short = self.duration_ms > 0
                && self.last_frame_ts.saturating_add(2_000) >= self.duration_ms;
            let reached_end =
                self.duration_ms == 0 || position + 800 >= self.duration_ms || tail_short;
            if reached_end {
                self.finish_stream(position);
            } else if self.had_first_frame
                && self.duration_ms > 0
                && position + 3_000 < self.duration_ms
                && self.error.is_none()
            {
                self.report_error("视频解码中断（ffmpeg 提前结束），点击重试");
            }
        }
    }

    /// Shared tail handling for a closed decoder: 循环播放 / 下一分P / end card.
    /// `position` is the clock at the moment the stream ran out.
    fn finish_stream(&mut self, position: u64) {
        if self.loop_mode == o11a1::settings::LOOP_ONE {
            // 循环播放: same window, same sessions restarted at zero.
            log::line("loop mode replay");
            self.ended = false;
            self.requested_play = true;
            self.seek(0);
            self.play();
            return;
        }
        if self.loop_mode == o11a1::settings::LOOP_NEXT_PART {
            let pages = if self.pages.is_empty() {
                self.info
                    .as_ref()
                    .map(|info| info.pages.len())
                    .unwrap_or(0)
            } else {
                self.pages.len()
            };
            if self.current_page + 1 < pages {
                log::line(&format!(
                    "loop mode next part from={} to={}",
                    self.current_page + 1,
                    self.current_page + 2
                ));
                self.ended = false;
                self.switch_page(self.current_page + 1);
                return;
            }
        }
        self.ended = true;
        // Freeze exactly on the tail. The audio clock often stops a few
        // hundred ms before the container duration, so leaving
        // `position_ms`/`paused_at` at the last poll made the progress bar
        // snap backwards when the end card appeared.
        let end_position = if self.duration_ms > 0 {
            self.duration_ms
        } else {
            position
        };
        self.position_ms = end_position;
        self.paused_at = end_position;
        self.shared
            .position_ms
            .store(end_position, Ordering::SeqCst);
        self.playing = false;
        self.requested_play = false;
        self.sync_shared();
        self.on_playback_end();
        self.gate(false);
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::Pause);
        }
        self.danmaku.clear();
        let _ = self.danmaku_layer.clear();
        log::line("danmaku cleared reason=end");
        self.ui_dirty = true;
        log::line(&format!(
            "ended position_ms={end_position} duration_ms={} loop_mode={}",
            self.duration_ms, self.loop_mode
        ));
    }

    fn display_new(&mut self, frame: DecodedFrame, first: bool) {
        if first {
            // The new part's first picture is on screen: the loading card must
            // stop naming it.
            self.page_loading = None;
        }
        self.cache_ab_frame(&frame);
        let Some(prepared) = self.prepare_frame(frame) else {
            return;
        };
        self.show_source(prepared);
        if first {
            self.next = None;
            self.next_tick_at = Some(Instant::now());
        }
    }

    /// Convert one decoded picture into the BGRA frame ring. CPU frames are
    /// uploaded as before; D3D11VA frames are sampled directly by the NV12 or
    /// P010 shader, so this is the only per-picture GPU work and no bytes cross
    /// the bus.
    fn prepare_frame(&mut self, frame: DecodedFrame) -> Option<GpuFrame> {
        if self.source.width == 0 || self.source.height == 0 {
            return None;
        }
        let result = match &frame.data {
            FrameData::Cpu(data) => self.video.upload(&self.gpu, data, frame.ts_ms),
            FrameData::Gpu(hw) => self.video.upload_hw(hw, frame.ts_ms),
        };
        match result {
            Ok(prepared) => Some(prepared),
            Err(error) => {
                log::line(&format!("gpu frame upload failed: {error:#}"));
                None
            }
        }
    }

    fn show_source(&mut self, frame: GpuFrame) {
        self.cur = Some(frame);
        self.cur_ts_ms = frame.ts_ms;
        self.last_frame_ts = frame.ts_ms;
        self.present_current();
        self.last_source_present = Some(Instant::now());
        if let Some(pause) = &mut self.stall_pause {
            if pause.first_frame_at.is_none() {
                pause.first_frame_at = Some(Instant::now());
            }
        }
        // Never resume the sound from here: the picture that just arrived may
        // still be far behind the paused device clock. `poll_picture` resumes
        // only once the picture has caught up (or was jumped there).
        if self.stalled && !self.stall_audio_paused {
            self.stalled = false;
            self.ui_dirty = true;
        }
    }

    /// Resume the device at the frozen sound position once the picture has
    /// reached it. Called from the presentation loop, not from every frame.
    fn resume_audio_with_picture(&mut self, position: u64) {
        self.stall_audio_paused = false;
        self.stall_pause = None;
        self.stalled = false;
        self.video_lag_since = None;
        self.base_ms = position;
        self.anchor = Some(Instant::now());
        self.paused_at = position;
        self.ui_dirty = true;
        if self.playing {
            if let Some(audio) = &self.audio {
                audio.send(AudioCommand::Resume);
            }
        }
        log::line(&format!(
            "buffer resume synced position_ms={position} last_frame_ts={}",
            self.last_frame_ts
        ));
    }

    /// Open a fresh video decoder at the sound position while freezing the
    /// sound for the short seek. The sound is the timeline; the picture may
    /// skip content, but it must never continue behind the sound.
    fn start_video_catch_up(&mut self, position: u64) {
        if self.interacting
            || self.video_resync.is_some()
            || self.is_live
            || !self.playing
        {
            return;
        }
        if self.decoder.is_none() {
            return;
        }
        let target = clamp_seek(position, self.duration_ms);
        if self.duration_ms > 0 && target.saturating_add(1_500) >= self.duration_ms {
            // Too close to the end: the tail path will finish the stream
            // instead of seeking into the end card.
            return;
        }
        self.last_av_resync = Some(Instant::now());
        self.video_lag_since = None;
        // A jump means the first A/B pass is missing pictures; a cache built
        // from it would replay with holes, so fall back to prefetch/seek.
        if self.ab_cache.as_ref().map(|cache| !cache.ready).unwrap_or(false) {
            self.reset_ab_cache();
        }
        let queue_before = self
            .decoder
            .as_ref()
            .map(|decoder| decoder.queue.len())
            .unwrap_or(0);
        // Freeze the sound before the decoder teardown: stopping ffmpeg can
        // take a few milliseconds and the target must stay where the user
        // heard the sound, not where it ran to during the teardown.
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::Pause);
        }
        if let Some(mut decoder) = self.decoder.take() {
            decoder.stop();
        }
        self.stall_audio_paused = true;
        self.stall_pause = None;
        self.stalled = true;
        self.ui_dirty = true;
        self.next = None;
        self.next_tick_at = None;
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        log::line(&format!(
            "av catch-up start target_ms={target} audio_ms={} last_frame_ts={} queue={queue_before}",
            self.audio
                .as_ref()
                .map(|audio| audio.clock.position_ms.load(Ordering::SeqCst))
                .unwrap_or(0),
            self.last_frame_ts
        ));
        match VideoDecoder::spawn(&self.video_spec(), target, generation, true, &self.gpu) {
            Ok(decoder) => {
                self.decoder = Some(decoder);
                self.video_resync = Some(VideoResync {
                    target,
                    started: Instant::now(),
                });
            }
            Err(error) => {
                // The old decoder is already gone; fall back to a normal
                // restart (which also reopens the sound) rather than leaving
                // the window with no picture.
                log::line(&format!(
                    "av catch-up spawn failed target_ms={target} err={error:#}"
                ));
                self.stall_audio_paused = false;
                self.stalled = false;
                if let Err(retry) = self.restart_at(target) {
                    self.report_error(format!("视频追赶失败，请点击重试：{retry:#}"));
                }
            }
        }
    }

    /// Land the catch-up decoder's first picture and resume the sound from
    /// the same position. A seek that fails to produce a picture reports an
    /// error instead of leaving the sound paused forever.
    fn poll_video_resync(&mut self) {
        let Some(state) = self.video_resync.as_ref() else {
            return;
        };
        let target = state.target;
        let started = state.started;
        let frame = self
            .decoder
            .as_ref()
            .and_then(|decoder| decoder.queue.take_first());
        if let Some(frame) = frame {
            self.video_resync = None;
            let landed = frame.ts_ms;
            let frozen = self.audio_position().unwrap_or(landed);
            self.stall_audio_paused = false;
            self.stall_pause = None;
            self.stalled = false;
            self.video_lag_since = None;
            self.base_ms = landed;
            self.anchor = Some(Instant::now());
            self.paused_at = landed;
            self.audio_master = true;
            self.display_new(frame, true);
            self.next_tick_at = Some(Instant::now());
            self.ui_dirty = true;
            if self.playing {
                if let Some(audio) = &self.audio {
                    audio.send(AudioCommand::Resume);
                }
            }
            log::line(&format!(
                "av catch-up landed target_ms={} frame_ts={landed} audio_ms={frozen} picture_gap_ms={}",
                target,
                frozen.saturating_sub(landed)
            ));
            self.sync_shared();
            return;
        }
        // A seek past the real end of the DASH track opens a decoder that
        // immediately EOFs with no picture. Finish (or report) instead of
        // leaving the sound paused until the catch-up timeout.
        let closed = self
            .decoder
            .as_ref()
            .map(|decoder| decoder.queue.is_closed())
            .unwrap_or(true);
        if closed {
            self.video_resync = None;
            self.stall_audio_paused = false;
            self.stall_pause = None;
            self.stalled = false;
            self.video_lag_since = None;
            let position = self.audio_position().unwrap_or(target);
            log::line(&format!(
                "av catch-up hit eof target_ms={target} position_ms={position} duration_ms={}",
                self.duration_ms
            ));
            if self.duration_ms > 0 && position.saturating_add(3_000) < self.duration_ms {
                self.report_error("视频解码中断（ffmpeg 提前结束），点击重试");
            } else {
                self.finish_stream(position);
            }
            return;
        }
        if started.elapsed() >= Duration::from_millis(AV_CATCHUP_TIMEOUT_MS) {
            self.video_resync = None;
            self.stall_audio_paused = false;
            self.stall_pause = None;
            self.stalled = false;
            self.video_lag_since = None;
            log::line(&format!(
                "av catch-up timeout target_ms={target} elapsed_ms={}",
                started.elapsed().as_millis()
            ));
            self.report_error("视频追赶超时，点击重试");
        }
    }

    fn present_current(&mut self) {
        let Some(frame) = self.cur.as_ref().copied() else {
            return;
        };
        let mut mode = if self.interacting {
            EnhanceMode::Off
        } else {
            self.enhance
        };
        let result = if self.audio_only {
            self.video
                .present_black(&self.gpu)
                .map(|presented| (false, presented))
        } else {
            self.video.present_frame(
                &self.gpu,
                &mut self.enhance_pipeline,
                &mut mode,
                &frame,
                self.brightness,
            )
        };
        match result {
            Ok((_enhanced, presented)) => {
                if presented {
                    self.presents_window = self.presents_window.saturating_add(1);
                }
                if !self.interacting && mode != self.enhance {
                    self.enhance = mode;
                    self.shared.enhance.store(mode.as_u8(), Ordering::SeqCst);
                    self.ui_dirty = true;
                }
            }
            Err(error) => log::line(&format!("present failed: {error:#}")),
        }
    }

    /// Hold the next source picture one lookahead window ahead of the clock.
    /// `take_front_until` keeps queue order; late frames are discarded rather
    /// than rewinding the interpolator.
    fn fill_next(&mut self, horizon_ms: u64) {
        if self.next.is_some() {
            return;
        }
        for _ in 0..6 {
            let frame = if self.ab_cache_playing {
                self.take_ab_cache_frame(horizon_ms)
            } else {
                match self.decoder.as_ref() {
                    Some(decoder) => decoder.queue.take_front_until(horizon_ms),
                    None => None,
                }
            };
            let Some(frame) = frame else {
                break;
            };
            if frame.ts_ms.saturating_add(1) < self.cur_ts_ms {
                continue;
            }
            if let Some(prepared) = self.prepare_frame(frame) {
                self.next = Some(prepared);
            }
            break;
        }
    }

    /// Build one output tick at `position_ms`. Returns true when a real source
    /// picture was presented, so the stall detector can treat it as progress.
    fn interp_tick(&mut self, position_ms: u64) -> bool {
        self.fill_next(position_ms.saturating_add(INTERP_LOOKAHEAD_MS));

        if let Some(next) = self.next.as_ref() {
            if next.ts_ms <= position_ms.saturating_add(1) {
                let next = self.next.take().expect("next checked");
                self.show_source(next);
                return true;
            }
        }

        let (Some(current), Some(next)) = (self.cur.as_ref(), self.next.as_ref()) else {
            return false;
        };
        if next.ts_ms <= self.cur_ts_ms {
            return false;
        }
        let span = next.ts_ms - self.cur_ts_ms;
        let phase = position_ms.saturating_sub(self.cur_ts_ms).min(span);
        let mut weight = ((phase as f64 / span as f64) * 256.0).round() as u16;
        // A cut or a very fast change must never be double-exposed. Presenting
        // the nearest source picture also keeps the 60 Hz cadence intact. The
        // signature is sampled during upload, so this costs no readback.
        if interp::signature_difference(&current.signature, &next.signature) >= INTERP_CUT_DIFF {
            weight = if weight < 128 { 0 } else { 256 };
            self.interp_cuts = self.interp_cuts.saturating_add(1);
        }
        self.present_blend(weight);
        false
    }

    /// Drive the 60 Hz grid independently of the source FPS. One tick per
    /// engine iteration; a long stall restarts the grid instead of bursting
    /// a row of catch-up frames.
    fn poll_interp(&mut self, position_ms: u64) -> bool {
        if self.interacting || self.cur.is_none() || self.interp_out_hz.is_none() {
            return false;
        }
        let now = Instant::now();
        if let Some(due) = self.next_tick_at {
            if now < due {
                return false;
            }
        }
        let mut next = self.next_tick_at.unwrap_or(now) + self.out_period;
        if next + self.out_period <= now {
            next = now + self.out_period;
        }
        self.next_tick_at = Some(next);
        self.interp_ticks = self.interp_ticks.saturating_add(1);
        self.interp_tick(position_ms)
    }

    fn present_blend(&mut self, weight: u16) {
        let started = Instant::now();
        let (Some(current), Some(next)) = (self.cur.as_ref().copied(), self.next.as_ref().copied())
        else {
            return;
        };
        let mut mode = if self.interacting {
            EnhanceMode::Off
        } else {
            self.enhance
        };
        let result = if self.audio_only {
            self.video
                .present_black(&self.gpu)
                .map(|presented| (false, presented))
        } else {
            self.video.present_blend(
                &self.gpu,
                &mut self.enhance_pipeline,
                &mut mode,
                &current,
                &next,
                weight,
                self.brightness,
            )
        };
        match result {
            Ok((_enhanced, presented)) => {
                if presented {
                    self.presents_window = self.presents_window.saturating_add(1);
                }
                if !self.interacting && mode != self.enhance {
                    self.enhance = mode;
                    self.shared.enhance.store(mode.as_u8(), Ordering::SeqCst);
                    self.ui_dirty = true;
                }
            }
            Err(error) => log::line(&format!("blend present failed: {error:#}")),
        }
        // Submission time, not GPU execution time: the blend and the enhance
        // chain are queued by the same context and the compositor paces them.
        let elapsed_us = started.elapsed().as_micros().max(1) as u32;
        self.blend_us_total = self.blend_us_total.saturating_add(elapsed_us as u64);
        self.blend_count = self.blend_count.saturating_add(1);
        self.last_blend_us = elapsed_us;
        self.generated = self.generated.saturating_add(1);
        self.interp_blends = self.interp_blends.saturating_add(1);
        self.shared
            .generated
            .store(self.generated, Ordering::SeqCst);
        self.shared.blend_us.store(elapsed_us, Ordering::SeqCst);
    }

    fn maybe_enable_audio_master(&mut self) {
        if self.audio_master {
            return;
        }
        let Some(audio) = &self.audio else {
            return;
        };
        // `playing` is false while the sound is intentionally paused for a
        // stall or a video catch-up; the device clock is still the right
        // reference then, so do not wait for it to run again.
        if !audio.clock.available.load(Ordering::SeqCst)
            || !audio.clock.ready.load(Ordering::SeqCst)
        {
            return;
        }
        let audio_ms = audio.clock.position_ms.load(Ordering::SeqCst);
        let wall_ms = self.wall_position();
        let difference = audio_ms.abs_diff(wall_ms);
        if difference <= 300 {
            self.audio_master = true;
            log::line(&format!(
                "audio clock master audio_ms={audio_ms} wall_ms={wall_ms} delta_ms={}",
                audio_ms as i64 - wall_ms as i64
            ));
        } else {
            // The sound joined late (or the picture was held at the loading
            // card). Anchor the picture to the device clock instead of keeping
            // a wall clock that the audio will never agree with: the decoder
            // then skips stale frames and the rest of the session stays
            // lip-synced.
            self.base_ms = audio_ms;
            self.anchor = Some(Instant::now());
            self.paused_at = audio_ms;
            self.audio_master = true;
            log::line(&format!(
                "audio clock master re-anchored audio_ms={audio_ms} wall_ms={wall_ms} delta_ms={}",
                audio_ms as i64 - wall_ms as i64
            ));
        }
    }

    fn log_drift_if_needed(&mut self) {
        if !self.audio_master || self.stall_audio_paused || self.stalled {
            return;
        }
        // The first ~3 s are device warm-up: waveOut's 80 ms queue and the
        // decoder spawn make a 200 ms wall-vs-device reading normal, and the
        // video is already following the device clock, so reporting it would
        // be noise rather than drift.
        if self.position_ms < 3_000 {
            return;
        }
        let Some(audio) = &self.audio else {
            return;
        };
        let audio_ms = audio.clock.position_ms.load(Ordering::SeqCst);
        let wall_ms = self.wall_position();
        let difference = audio_ms.abs_diff(wall_ms);
        if difference > AV_DRIFT_MS {
            let due = self
                .last_drift_log
                .map(|at| at.elapsed() >= Duration::from_secs(2))
                .unwrap_or(true);
            if due {
                self.last_drift_log = Some(Instant::now());
                log::line(&format!(
                    "av-drift audio_ms={audio_ms} wall_ms={wall_ms} delta_ms={} starved_ms={}",
                    audio_ms as i64 - wall_ms as i64,
                    audio.clock.starved_ms.load(Ordering::SeqCst)
                ));
            }
        }
    }

    fn sync_shared(&self) {
        self.shared.sync(self);
    }

    // ------------------------------------------------------------- commands

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Play => self.play(),
            Command::Pause => self.pause(),
            Command::TogglePlay => self.toggle_play(),
            Command::Seek(ms) => self.seek(ms),
            Command::LoadSource(config) => self.begin_source_replace(config),
            Command::SetVolume(value) => self.set_volume(value),
            Command::SetSpeed(value) => self.set_speed(value),
            Command::SetInterp(mode) => {
                self.apply_interp_mode(mode);
            }
            Command::SetInterpHZ(hz) => self.set_interp_hz(hz),
            Command::SetEnhance(mode) => self.set_enhance(mode),
            Command::SetDanmaku(visible) => self.set_danmaku_visible(visible),
            Command::ToggleDanmaku => self.toggle_danmaku(),
            Command::ToggleMini => self.toggle_mini(),
            Command::ToggleStats => self.toggle_stats(),
            Command::SetAspect(mode) => self.set_aspect(mode),
            Command::CycleAspect => self.cycle_aspect(),
            Command::SetOnTop(on) => self.set_ontop(on),
            Command::ToggleOnTop => self.toggle_ontop(),
            Command::SetBrightness(value) => self.set_brightness(value),
            Command::SetAudioOnly(on) => self.set_audio_only(on),
            Command::ToggleAudioOnly => self.toggle_audio_only(),
            Command::Screenshot => self.screenshot(),
            Command::CycleSpeed(direction) => self.cycle_speed(direction),
            Command::SetRect {
                x,
                y,
                width,
                height,
            } => {
                if self.external {
                    window::move_rect(self.hwnd, x, y, width, height);
                }
            }
            Command::SetVisible(visible) => {
                if self.external {
                    window::set_visible(self.hwnd, visible);
                }
            }
            Command::Preview(time) => {
                self.hover_preview = time;
                if let Some(time) = time {
                    self.ensure_storyboard();
                    self.request_storyboard_cell(time);
                } else if self.external_preview_visible {
                    let _ = self.overlay.clear();
                    self.external_preview_visible = false;
                }
                self.ui_dirty = true;
            }
            Command::SendDanmaku { text, mode } => {
                if !text.trim().is_empty() {
                    let item = DanmakuItem {
                        progress_ms: self.position_now(),
                        mode,
                        fontsize: 25,
                        color: 0xFF_FFFF,
                        text,
                        mid_hash: String::new(),
                        dmid: String::new(),
                    };
                    self.danmaku.push_item(item.clone());
                    self.live_danmaku.push(item);
                    self.redraw_danmaku_now();
                }
            }
            Command::Close => self.quit = true,
        }
    }

    fn handle_event(&mut self, event: InputEvent) {
        if self.external {
            match event {
                InputEvent::Quit => self.quit = true,
                InputEvent::Resize { width, height } => {
                    if width < 32 || height < 32 {
                        return;
                    }
                    self.client = (width, height);
                    self.apply_client_geometry();
                }
                InputEvent::ResizeEnded { .. } => {
                    // Hosted video windows follow the Electron viewport.
                }
                InputEvent::LeftDown { x, y } => {
                    let hit = self.danmaku_hit_at(x as f32, y as f32);
                    self.external_press = Some((Instant::now(), x, y, hit));
                }
                InputEvent::LeftUp => {
                    if let Some((started, _x, _y, hit)) = self.external_press.take() {
                        if started.elapsed() < Duration::from_millis(500) {
                            self.shared.click_seq.fetch_add(1, Ordering::SeqCst);
                        } else if let Some(hit) = hit {
                            self.shared.long_press_seq.fetch_add(1, Ordering::SeqCst);
                            if let Ok(mut queue) = self.shared.long_press_hits.lock() {
                                queue.push_back(hit);
                                if queue.len() > 4 {
                                    queue.pop_front();
                                }
                            }
                        }
                    }
                }
                InputEvent::DoubleClick { .. } => {
                    self.shared.double_click_seq.fetch_add(1, Ordering::SeqCst);
                }
                InputEvent::MouseMove { .. } => {
                    self.shared.move_seq.fetch_add(1, Ordering::SeqCst);
                }
                InputEvent::RightDown { x, y } => {
                    if let Some(hit) = self.danmaku_hit_at(x as f32, y as f32) {
                        if let Ok(mut queue) = self.shared.danmaku_hits.lock() {
                            queue.push_back(hit);
                            if queue.len() > 8 {
                                queue.pop_front();
                            }
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        match event {
            InputEvent::Quit => self.quit = true,
            InputEvent::Focus => {
                self.shared.focus_seq.fetch_add(1, Ordering::SeqCst);
                // The settings page may have changed the danmaku values while
                // this window was in the background. Pull them from disk as
                // soon as the window is active again instead of waiting for a
                // popup slider to be touched.
                self.reload_danmaku_settings();
            }
            InputEvent::Resize { width, height } => {
                if width < 32 || height < 32 {
                    return;
                }
                self.client = (width, height);
                if self.interacting {
                    // Keep only the newest size; the loop applies one layout
                    // and one video transform per frame, not per WM_SIZE.
                    self.pending_live_resize = Some((width, height));
                } else {
                    self.apply_client_geometry();
                }
            }
            InputEvent::ResizeEnded { width, height } => {
                self.remember_window_size(width, height);
            }
            InputEvent::Interaction { active, resizing } => {
                if active {
                    if !self.interacting {
                        self.interacting = true;
                        self.interaction_resizing = resizing;
                        self.interaction_start_client = self.client;
                        self.pending_live_resize = None;
                        self.video_lag_since = None;
                        self.video.set_interaction(true);
                        self.ui_dirty = false;
                        log::line(&format!(
                            "interaction begin resize={} client={}x{}",
                            u8::from(resizing),
                            self.client.0,
                            self.client.1
                        ));
                    }
                } else if self.interacting {
                    self.interacting = false;
                    self.interaction_resizing = false;
                    self.pending_live_resize = None;
                    self.video_lag_since = None;
                    self.video.set_interaction(false);
                    // Drop the temporary overlay scale before the final
                    // layout is drawn; otherwise the new UI would be scaled a
                    // second time.
                    if let Err(error) = self.composition.set_overlay_scale(1.0, 1.0) {
                        log::line(&format!("overlay scale reset failed: {error:#}"));
                    }
                    self.client = window::client_size(self.hwnd);
                    self.apply_client_geometry();
                    self.next_tick_at = Some(Instant::now());
                    self.ui_dirty = true;
                    self.draw_ui_now();
                    log::line(&format!(
                        "interaction settle client={}x{}",
                        self.client.0, self.client.1
                    ));
                }
            }
            InputEvent::Key(key) => {
                self.note_activity();
                self.handle_key(key)
            }
            InputEvent::Char(code) => {
                if !self.dm_input_focus {
                    return;
                }
                let Some(ch) = char::from_u32(code as u32) else {
                    return;
                };
                if ch.is_control() || self.dm_input.chars().count() >= 100 {
                    return;
                }
                self.dm_input.push(ch);
                self.ui_dirty = true;
            }
            InputEvent::MouseLeave => {
                self.pointer = None;
                if !self.hover_pinned && self.hover_preview.take().is_some() {
                    self.ui_dirty = true;
                }
                if !self.danmaku_popup && self.dm_menu.is_none() {
                    self.chrome_shown = false;
                    self.ui_dirty = true;
                }
            }
            InputEvent::MouseMove { x, y } => {
                self.note_activity();
                self.pointer = Some((x as f32, y as f32));
                // A held right press that leaves its start point becomes a drag
                // selection over the open context menu. Hovering 举报 opens the
                // reason submenu so the pointer can continue straight into it.
                if let Some((start_x, start_y, moved)) = self.dm_menu_drag.as_mut() {
                    if !*moved && (x as f32 - *start_x).hypot(y as f32 - *start_y) > 5.0 {
                        *moved = true;
                    }
                }
                if let Some(menu) = self.dm_menu.clone() {
                    if !menu.submenu {
                        if let Some((_, _, true)) = self.dm_menu_drag {
                            let layout = self.overlay.layout;
                            let panel = dm_menu_rect(
                                menu.x,
                                menu.y,
                                layout.width,
                                layout.height,
                                layout.scale,
                            );
                            if dm_menu_row(panel, DM_MENU_REPORT, layout.scale)
                                .contains(x as f32, y as f32)
                            {
                                if let Some(menu) = self.dm_menu.as_mut() {
                                    menu.submenu = true;
                                    self.ui_dirty = true;
                                }
                            }
                        }
                    }
                }
                if let Some((arm_x, arm_y)) = self.window_drag_arm {
                    if (x - arm_x).abs() > 4 || (y - arm_y).abs() > 4 {
                        self.window_drag_arm = None;
                        window::drag_window(self.hwnd);
                        return;
                    }
                }
                if let Some((arm_x, arm_y)) = self.sidebar_drag_arm {
                    if (x - arm_x).abs() > 4 || (y - arm_y).abs() > 4 {
                        self.sidebar_drag_arm = None;
                        window::drag_window(self.hwnd);
                        return;
                    }
                }
                if let Some((press, _)) = self.dm_long.as_mut() {
                    if !press.moved(x as f32, y as f32, LONG_PRESS_SLOP) {
                        log::line("danmaku long-press cancelled move");
                        self.dm_long = None;
                    }
                }
                let layout = self.overlay.layout;
                if let Some((start_x, start_speed)) = self.speed_drag {
                    self.drag_speed(start_x, start_speed, x as f32);
                } else if self.precision_dragging {
                    let fraction = ((x as f32 - layout.precision_track.left)
                        / layout.precision_track.width().max(1.0))
                    .clamp(0.0, 1.0);
                    if self.duration_ms > 0 {
                        self.drag_preview =
                            Some((fraction as f64 * self.duration_ms as f64) as u64);
                        self.ui_dirty = true;
                    }
                } else if let Some((start_x, start_ms)) = self.tuner_drag {
                    let notch = 12.0 * self.scale;
                    // Continuous drag: the ruler follows the finger for the
                    // whole gesture, but the playback position only changes on
                    // release. Trunc keeps a small wobble inside a notch from
                    // stepping a frame.
                    let delta = x as f32 - start_x;
                    let steps = tuner_steps(delta, notch);
                    let fps = if self.source.fps > 1.0 {
                        self.source.fps
                    } else {
                        30.0
                    };
                    let frame_ms = 1000.0 / fps;
                    let start_frame = (start_ms as f64 / frame_ms).round();
                    let target_frame = start_frame + (steps * self.frame_step as i64) as f64;
                    let target = self.clamp_position((target_frame.max(0.0) * frame_ms).round() as u64);
                    self.drag_preview = Some(target);
                    // The ruler is endless: keep the full drag distance so the
                    // scale can keep moving as far as the pointer travels. The
                    // draw pass derives the visible tick range from this value.
                    self.tuner_offset = delta;
                    self.tuner_pulse = 0.0;
                    self.ui_dirty = true;
                } else if self.dragging {
                    let fraction = layout.progress_fraction(x as f32);
                    if self.duration_ms > 0 {
                        self.drag_preview =
                            Some((fraction as f64 * self.duration_ms as f64) as u64);
                        self.ui_dirty = true;
                    }
                } else if let Some(kind) = self.popup_drag {
                    self.drag_popup(kind, x as f32, &layout);
                }
                self.update_hover_preview();
            }
            InputEvent::LeftDown { x, y } => self.handle_click(x, y),
            InputEvent::LeftUp => {
                if self.dm_long.take().is_some() {
                    // The press either became a block (fired) or stayed short:
                    // a comment click is not a play/pause click in both cases.
                    return;
                }
                if self.speed_drag.take().is_some() {
                    self.tuner_offset = 0.0;
                    self.tuner_pulse = 0.0;
                    self.commit_speed_preview();
                    return;
                }
                if self.precision_dragging {
                    self.precision_dragging = false;
                    if let Some(target) = self.drag_preview.take() {
                        self.seek(target);
                    }
                    return;
                }
                if let Some((_start_x, start_ms)) = self.tuner_drag.take() {
                    self.tuner_offset = 0.0;
                    self.tuner_pulse = 0.0;
                    self.ui_dirty = true;
                    if let Some(target) = self.drag_preview.take() {
                        if target != start_ms {
                            self.seek(target);
                        }
                    }
                    return;
                }
                if self.sidebar_drag_arm.take().is_some() {
                    return;
                }
                if self.window_drag_arm.take().is_some() {
                    return;
                }
                if self.dragging {
                    self.dragging = false;
                    let target = self.drag_preview.take().unwrap_or(self.position_now());
                    self.seek(target);
                }
                self.popup_drag = None;
            }
            InputEvent::RightDown { x, y } => self.handle_right_click(x, y),
            InputEvent::RightUp { x, y } => self.handle_right_up(x, y),
            InputEvent::DoubleClick { x, y } => {
                if self.mini {
                    self.set_mini(false);
                    return;
                }
                self.close_dm_menu();
                let layout = self.overlay.layout;
                if !self.fullscreen && layout.top_bar.contains(x as f32, y as f32) {
                    window::toggle_maximize(self.hwnd);
                    self.ui_dirty = true;
                    return;
                }
                if layout.panel.contains(x as f32, y as f32)
                    || (self.precision_open && layout.precision.contains(x as f32, y as f32))
                    || (self.speed_open && layout.speed_panel.contains(x as f32, y as f32))
                    || (self.sidebar_open && layout.sidebar.contains(x as f32, y as f32))
                {
                    return;
                }
                self.toggle_fullscreen();
            }
            InputEvent::Wheel(delta) => {
                if self.speed_open {
                    let over_panel = self
                        .pointer
                        .map(|(x, y)| self.overlay.layout.speed_panel.contains(x, y))
                        .unwrap_or(false);
                    if over_panel {
                        let raw = (delta as f32 / 120.0).round() as i32;
                        if raw != 0 {
                            // One notch is one wheel stop. The restart is
                            // debounced: the session must not be rebuilt per
                            // notch.
                            let next = crate::speed::stepped(self.speed_preview, raw);
                            self.preview_speed(next);
                            self.speed_base = self.speed_preview;
                            // Let the wheel glide one stop instead of snapping.
                            // The new base sits one gap off-centre, then the
                            // idle glide above eases it under the red needle.
                            self.tuner_offset =
                                raw as f32 * crate::speed::WHEEL_GAP * self.overlay.layout.scale;
                            self.tuner_pulse = 0.0;
                            self.speed_commit_at =
                                Some(Instant::now() + Duration::from_millis(260));
                        }
                    }
                    return;
                }
                if self.precision_open {
                    let over_panel = self
                        .pointer
                        .map(|(x, y)| self.overlay.layout.precision.contains(x, y))
                        .unwrap_or(false);
                    if over_panel {
                        let raw = (delta as f32 / 120.0).round() as i64;
                        if raw != 0 {
                            // Wheel-down is forward, matching drag-left. Every
                            // notch only moves the preview and restarts the
                            // idle countdown; the single real seek happens in
                            // `poll_pending_wheel_seek` after the notches stop.
                            let step = -raw;
                            self.tuner_offset = 0.0;
                            self.tuner_pulse = 0.001;
                            self.tuner_pulse_dir = if step > 0 { -1.0 } else { 1.0 };
                            if let Some(target) = self.nudge_frames(step * self.frame_step as i64) {
                                self.pending_wheel_seek = Some((target, Instant::now()));
                            }
                            self.ui_dirty = true;
                        }
                    }
                    return;
                }
                let step = (delta as f32 / 120.0).round() as i32;
                if self.sidebar_open
                    && self
                        .pointer
                        .map(|(x, y)| self.overlay.layout.sidebar.contains(x, y))
                        .unwrap_or(false)
                {
                    if step == 0 {
                        return;
                    }
                    let max = self.sidebar_max_scroll();
                    self.sidebar_scroll_target =
                        (self.sidebar_scroll_target - step as f32 * 88.0).clamp(0.0, max);
                    log::line(&format!(
                        "sidebar wheel step={step} target={} max={max} scroll={}",
                        self.sidebar_scroll_target, self.sidebar_scroll
                    ));
                    self.ui_dirty = true;
                    return;
                }
                let step = wheel_volume_step(
                    &mut self.wheel_volume_accum,
                    &mut self.wheel_volume_dir,
                    delta,
                );
                if step != 0 {
                    let value = (self.volume as i32 + step).clamp(0, 100) as u8;
                    if value != self.volume || self.muted {
                        self.set_volume(value);
                    }
                }
                // Wheel over the picture is a volume gesture: show the
                // right-edge level bar for a moment. The toolbar stays as it
                // is; there is no volume flyout to reveal.
                self.volume_wheel_until = Some(Instant::now() + Duration::from_millis(900));
                self.ui_dirty = true;
                log::line(&format!(
                    "volume wheel delta={delta} step={step} volume={}",
                    self.volume
                ));
            }
        }
    }

    fn note_activity(&mut self) {
        if !self.chrome_shown {
            log::line("chrome shown");
        }
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
    }

    fn handle_click(&mut self, x: i32, y: i32) {
        self.hover_pinned = false;
        if self.mini {
            // The mini window is frameless too: keep the top 26 px as the
            // drag handle. The picture itself has no click transport.
            if (y as f32) < 26.0 * self.scale {
                window::drag_window(self.hwnd);
            }
            return;
        }
        self.note_activity();
        self.sidebar_drag_arm = None;
        let layout = self.overlay.layout;
        let px = x as f32;
        let py = y as f32;
        if self.precision_open {
            if self.handle_precision_click(px, py) {
                return;
            }
            if !layout.precision.contains(px, py) {
                self.close_precision();
            }
        }
        if self.speed_open {
            if self.handle_speed_click(px, py) {
                return;
            }
            if !layout.speed_panel.contains(px, py) {
                self.close_speed();
            }
        }
        // Overlay layers first: the undo row and the context menu sit above
        // the picture, so a click inside them must not toggle play.
        if self.undo.is_some() && layout.undo_toast.contains(px, py) {
            if layout.undo_button.contains(px, py) {
                self.undo_blocked_sender();
            }
            return;
        }
        if self.dm_menu.is_some() {
            if self.handle_dm_menu_click(px, py) {
                return;
            }
            self.close_dm_menu();
            return;
        }
        if !self.fullscreen {
            if layout.top_close.contains(px, py) {
                log::line("window close button");
                window::close_window(self.hwnd);
                return;
            }
            if layout.top_bar.contains(px, py) {
                log::line("window drag");
                window::drag_window(self.hwnd);
                return;
            }
        }
        if self.error.is_some() {
            if layout.error_retry.contains(px, py) {
                self.retry();
            }
            return;
        }
        // A toolbar popover is modal for the picture: clicking the video only
        // dismisses it, it must not pause, hit a danmaku, or open another menu.
        if self.toolbar_popup_open()
            && !self.point_in_toolbar_popup(&layout, px, py)
            && self.is_picture_click(&layout, px, py)
        {
            self.close_toolbar_popups();
            self.window_drag_arm = None;
            self.dm_long = None;
            return;
        }
        if self.loop_menu_open {
            for (index, row) in layout.loop_rows.iter().enumerate() {
                if row.contains(px, py) {
                    self.set_loop_mode(o11a1::settings::LOOP_MENU_ORDER[index]);
                    return;
                }
            }
            if layout.loop_menu.contains(px, py) {
                return;
            }
            if layout.loop_btn.contains(px, py) {
                self.loop_menu_open = false;
                self.ui_dirty = true;
                return;
            }
            self.loop_menu_open = false;
            self.ui_dirty = true;
        }
        if self.sidebar_open {
            for (index, rect) in layout.sidebar_tabs.iter().enumerate() {
                if rect.contains(px, py) {
                    self.select_sidebar_tab(SidebarTab::from_index(index));
                    return;
                }
            }
            if layout.sidebar_content.contains(px, py) && self.handle_sidebar_click(px, py, &layout)
            {
                return;
            }
            if layout.sidebar.contains(px, py) {
                // Dragging the lane's background moves the window with it. A
                // click alone is swallowed, so it can never toggle playback or
                // dismiss the panel.
                if !self.fullscreen && !self.external {
                    self.sidebar_drag_arm = Some((x, y));
                }
                return;
            }
        }
        if self.danmaku_popup && self.chrome_shown && self.handle_popup_click(px, py, &layout) {
            return;
        }
        if self.chrome_shown {
            if layout.progress_hit(px, py) {
                self.dragging = true;
                self.drag_preview =
                    Some((layout.progress_fraction(px) as f64 * self.duration_ms as f64) as u64);
                self.ui_dirty = true;
                return;
            }
            if layout.loop_btn.contains(px, py) {
                self.loop_menu_open = !self.loop_menu_open;
                self.danmaku_popup = false;
                self.dm_input_focus = false;
                self.settings_menu = None;
                self.ui_dirty = true;
                log::line(&format!(
                    "loop menu={}",
                    u8::from(self.loop_menu_open)
                ));
                return;
            }
            for (index, rect) in layout.toolbar_buttons.iter().enumerate() {
                if rect.contains(px, py) {
                    self.toolbar_action(index);
                    return;
                }
            }
            if layout.panel.contains(px, py) {
                return;
            }
        }
        // A press on a rendered comment arms the 500 ms block gesture instead
        // of toggling playback. The release either completes the block or is
        // swallowed; no play/pause state changes on a comment.
        if let Some(hit) = self.danmaku_hit_at(px, py) {
            let mut press = LongPress::default();
            press.down(Instant::now(), px, py);
            log::line(&format!(
                "danmaku press dmid={:?} mid_hash={:?} text={:?}",
                hit.dmid, hit.mid_hash, hit.text
            ));
            self.dm_long = Some((press, hit));
            return;
        }
        if !self.fullscreen && !self.external {
            // Standalone window: a press on the picture can drag the window.
            // A clean release does nothing; spacebar owns play/pause.
            self.window_drag_arm = Some((x, y));
        }
    }

    fn handle_right_click(&mut self, x: i32, y: i32) {
        self.note_activity();
        let px = x as f32;
        let py = y as f32;
        self.dm_menu_drag = None;
        let layout = self.overlay.layout;
        // Right-clicking the open sidebar lane closes it; the rest of the
        // picture keeps its own right-click gestures.
        if self.sidebar_open && layout.sidebar.contains(px, py) {
            self.set_sidebar(false, self.sidebar_tab);
            return;
        }
        // A right press on the picture is always a dismiss gesture for the
        // lane, even when another layer (speed/precision) is open. Those
        // layers live in the toolbar rectangle, which is excluded here.
        if self.sidebar_open && self.is_picture_click(&layout, px, py) {
            self.set_sidebar(false, self.sidebar_tab);
            return;
        }
        if self.toolbar_popup_open()
            && !self.point_in_toolbar_popup(&layout, px, py)
            && self.is_picture_click(&layout, px, py)
        {
            self.close_toolbar_popups();
            return;
        }
        if self.speed_open {
            self.close_speed();
            return;
        }
        if self.precision_open {
            self.close_precision();
            return;
        }
        if self.chrome_shown && self.overlay.layout.progress_hit(px, py) {
            self.open_precision();
            return;
        }
        if self.dm_menu.is_some() {
            self.close_dm_menu();
            return;
        }
        if self.mini {
            return;
        }
        match self.danmaku_hit_at(px, py) {
            Some(hit) => {
                self.open_dm_menu(&hit, px, py);
                self.dm_menu_drag = Some((px, py, false));
            }
            None => {
                log::line(&format!("danmaku menu no-hit x={px:.0} y={py:.0}"));
            }
        }
    }

    /// Right-button release. A press that was dragged onto a menu row activates
    /// it, so the whole menu can be used as hold → move → release without a
    /// second left click. A plain click leaves the menu open.
    fn handle_right_up(&mut self, x: i32, y: i32) {
        self.note_activity();
        let Some((start_x, start_y, moved)) = self.dm_menu_drag.take() else {
            return;
        };
        if !moved || self.dm_menu.is_none() {
            return;
        }
        if (x as f32 - start_x).hypot(y as f32 - start_y) <= 5.0 {
            return;
        }
        if self.handle_dm_menu_click(x as f32, y as f32) {
            log::line(&format!("danmaku menu drag release x={x} y={y}"));
        }
    }

    /// Fire the armed long-press once the hold threshold is reached.
    fn poll_danmaku_long_press(&mut self) {
        let Some((press, _)) = self.dm_long.as_mut() else {
            return;
        };
        if !press.active() || !press.due(Instant::now(), LONG_PRESS) {
            return;
        }
        let (_, hit) = self.dm_long.take().unwrap();
        log::line(&format!(
            "danmaku long-press fired dmid={:?} mid_hash={:?}",
            hit.dmid, hit.mid_hash
        ));
        self.block_danmaku_sender(&hit.mid_hash);
    }

    fn toolbar_popup_open(&self) -> bool {
        self.danmaku_popup || self.loop_menu_open
    }

    fn point_in_toolbar_popup(&self, layout: &crate::overlay::Layout, x: f32, y: f32) -> bool {
        (self.danmaku_popup && layout.popup.contains(x, y))
            || (self.loop_menu_open && layout.loop_menu.contains(x, y))
    }

    fn is_picture_click(&self, layout: &crate::overlay::Layout, x: f32, y: f32) -> bool {
        layout.stage.contains(x, y)
            && !layout.panel.contains(x, y)
            && (self.fullscreen || !layout.top_bar.contains(x, y))
            && !(self.sidebar_open && layout.sidebar.contains(x, y))
    }

    fn close_toolbar_popups(&mut self) {
        if !self.toolbar_popup_open() {
            return;
        }
        self.danmaku_popup = false;
        self.dm_input_focus = false;
        self.loop_menu_open = false;
        self.ui_dirty = true;
        log::line("toolbar popup close");
    }

    fn toolbar_action(&mut self, index: usize) {
        self.loop_menu_open = false;
        match index {
            TB_PRECISION => self.toggle_precision(),
            TB_SPEED => self.toggle_speed(),
            TB_MUTE => self.toggle_mute(),
            TB_DANMAKU => {
                if !self.danmaku_popup {
                    // Opening the popup is also a sync point: an already-open
                    // player may predate changes made in the settings page.
                    self.reload_danmaku_settings();
                }
                self.danmaku_popup = !self.danmaku_popup;
                self.settings_menu = None;
                self.ui_dirty = true;
                log::line(&format!("danmaku popup={}", u8::from(self.danmaku_popup)));
            }
            TB_INFO => self.open_sidebar_tab(SidebarTab::Info),
            TB_COMMENTS => self.open_sidebar_tab(SidebarTab::Comments),
            TB_SERIES => self.open_sidebar_tab(SidebarTab::Episodes),
            TB_PERF => self.open_sidebar_tab(SidebarTab::Performance),
            TB_SETTINGS => self.open_sidebar_tab(SidebarTab::Settings),
            _ => {}
        }
    }

    fn set_loop_mode(&mut self, mode: u8) {
        let mode = o11a1::settings::normalise_loop_mode(mode);
        self.loop_mode = mode;
        self.loop_menu_open = false;
        self.ui_dirty = true;
        let mut stored = o11a1::settings::load();
        stored.loop_mode = mode;
        stored.loop_one = mode == o11a1::settings::LOOP_ONE;
        if let Err(error) = o11a1::settings::save(&stored) {
            log::line(&format!("loop_mode save failed: {error:#}"));
        }
        log::line(&format!(
            "loop_mode={mode} ({})",
            match mode {
                o11a1::settings::LOOP_PAUSE => "pause at end",
                o11a1::settings::LOOP_ONE => "loop",
                o11a1::settings::LOOP_NEXT => "next / close",
                o11a1::settings::LOOP_NEXT_PART => "next part",
                _ => "next / close",
            }
        ));
    }

    /// Width added to, or removed from, the outer window when the sidebar is
    /// open. The full-height lane sits directly beside the picture, so this is
    /// exactly the lane width and the picture keeps its original size.
    fn sidebar_delta(&self) -> i32 {
        (360.0 * self.scale.max(0.75)).round() as i32
    }

    /// Maximize / fullscreen / mini can leave the window and the lane out of
    /// sync. Re-grow or shrink once the window is back in normal windowed mode.
    fn reconcile_sidebar_window(&mut self) {
        if self.external || self.mini || self.fullscreen || window::is_maximized(self.hwnd) {
            return;
        }
        if self.sidebar_open == self.sidebar_extra {
            return;
        }
        let delta = if self.sidebar_open {
            self.sidebar_delta()
        } else {
            -self.sidebar_delta()
        };
        window::grow_width(self.hwnd, delta);
        self.sidebar_extra = self.sidebar_open;
        self.client = window::client_size(self.hwnd);
    }

    fn set_sidebar(&mut self, open: bool, tab: SidebarTab) {
        let changed = open != self.sidebar_open;
        self.restore_live_danmaku();
        self.close_dm_menu();
        self.sidebar_open = open;
        self.sidebar_tab = tab;
        self.sidebar_scroll = 0.0;
        self.sidebar_scroll_target = 0.0;
        self.settings_menu = None;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();

        let windowed = !self.external && !self.fullscreen && !window::is_maximized(self.hwnd);
        let mut delta = 0;
        if changed && open != self.sidebar_extra && windowed {
            self.sidebar_extra = open;
            delta = if open {
                self.sidebar_delta()
            } else {
                -self.sidebar_delta()
            };
        }

        // The WM_SIZE caused by `grow_width` is only dispatched on the next
        // loop turn. Lay the sidebar out for the *target* client width now:
        // using the old width here reserves the lane inside the old picture
        // and snaps the video to a 640x360 stage until the resize arrives.
        let live = window::client_size(self.hwnd);
        let target = if delta != 0 {
            ((live.0 as i32 + delta).max(720) as u32, live.1)
        } else {
            live
        };
        self.client = target;
        if let Err(error) = self.overlay.resize(target.0, target.1, self.scale) {
            log::line(&format!("overlay resize (sidebar) failed: {error:#}"));
        }
        self.overlay
            .set_chrome_state(self.sidebar_open, self.fullscreen);

        if open {
            // Cover the right lane before the picture transform changes. The
            // supersized UI surface already holds the finished sidebar, which
            // sits outside the current window until `grow_width` exposes it.
            if let Err(error) = self.composition.set_root_clip(target.0, target.1) {
                log::line(&format!("composition clip (sidebar) failed: {error:#}"));
            }
            self.draw_ui_now();
            self.update_video_rect_at(target.0, target.1);
        } else {
            // Expand the picture first; the old lane still covers its right
            // edge until the window shrinks and the lane is cleared.
            self.update_video_rect_at(target.0, target.1);
        }

        if delta != 0 {
            window::grow_width(self.hwnd, delta);
            let actual = window::client_size(self.hwnd);
            if actual != target {
                // The work-area clamp refused part of the growth (or the
                // minimum width). Re-layout for the real client and paint it
                // immediately instead of waiting for the queued WM_SIZE.
                self.client = actual;
                if let Err(error) = self.overlay.resize(actual.0, actual.1, self.scale) {
                    log::line(&format!(
                        "overlay resize (sidebar actual) failed: {error:#}"
                    ));
                }
                self.overlay
                    .set_chrome_state(self.sidebar_open, self.fullscreen);
                if let Err(error) = self.composition.set_root_clip(actual.0, actual.1) {
                    log::line(&format!(
                        "composition clip (sidebar actual) failed: {error:#}"
                    ));
                }
                self.draw_ui_now();
                self.update_video_rect_at(actual.0, actual.1);
            } else if !open {
                if let Err(error) = self.composition.set_root_clip(actual.0, actual.1) {
                    log::line(&format!(
                        "composition clip (sidebar close) failed: {error:#}"
                    ));
                }
                self.draw_ui_now();
            }
        } else if !open {
            self.draw_ui_now();
        }

        self.ui_dirty = true;
        if open {
            log::line(&format!("sidebar=1 tab={}", tab.label()));
            self.on_sidebar_tab(tab);
        } else {
            log::line("sidebar=0");
        }
    }

    fn open_sidebar_tab(&mut self, tab: SidebarTab) {
        if self.sidebar_open && self.sidebar_tab == tab {
            self.set_sidebar(false, tab);
            return;
        }
        self.set_sidebar(true, tab);
    }

    /// Left-clicking a top tab only switches the panel. Unlike the toolbar's
    /// active button it never closes the lane; right-click / Esc own closing.
    fn select_sidebar_tab(&mut self, tab: SidebarTab) {
        if self.sidebar_open && self.sidebar_tab == tab {
            self.note_activity();
            self.ui_dirty = true;
            return;
        }
        self.set_sidebar(true, tab);
    }

    fn on_sidebar_tab(&mut self, tab: SidebarTab) {
        match tab {
            SidebarTab::Comments => self.request_comments(true),
            SidebarTab::History => {
                if self.history_dates.is_empty() && !self.history_dates_loading {
                    self.request_history_dates();
                }
            }
            _ => {}
        }
    }

    fn handle_sidebar_click(&mut self, x: f32, y: f32, layout: &crate::overlay::Layout) -> bool {
        match self.sidebar_tab {
            SidebarTab::Info => {
                let y = y + self.sidebar_scroll;
                if layout.info_id.contains(x, y) {
                    self.copy_video_id();
                    return true;
                }
                for (index, rect) in layout.info_actions.iter().enumerate() {
                    if rect.contains(x, y) {
                        self.info_action(index);
                        return true;
                    }
                }
            }
            SidebarTab::Episodes => {
                let count = self
                    .pages
                    .len()
                    .max(self.info.as_ref().map(|i| i.pages.len()).unwrap_or(0));
                for index in 0..count {
                    if layout
                        .episode_row(index, self.sidebar_scroll)
                        .contains(x, y)
                    {
                        self.switch_page(index);
                        return true;
                    }
                }
            }
            SidebarTab::Comments => {
                let sort_y = y + self.sidebar_scroll;
                if layout.comments_sort[0].contains(x, sort_y) {
                    self.set_comments_sort(true);
                    return true;
                }
                if layout.comments_sort[1].contains(x, sort_y) {
                    self.set_comments_sort(false);
                    return true;
                }
                if let Some(hit) = self.overlay.comment_hit(x, y, self.sidebar_scroll) {
                    self.toggle_comment_thread(hit.rpid, hit.action);
                    return true;
                }
            }
            SidebarTab::History => {
                if layout.history_live.contains(x, y) {
                    self.restore_live_danmaku();
                    self.ui_dirty = true;
                    return true;
                }
                for index in 0..self.history_dates.len() {
                    if layout
                        .history_row(index, self.sidebar_scroll)
                        .contains(x, y)
                    {
                        let date = self.history_dates[index].clone();
                        if self.history_date.as_deref() == Some(date.as_str()) {
                            // Second click on the selected day returns to live.
                            self.restore_live_danmaku();
                        } else {
                            self.request_history(date);
                        }
                        return true;
                    }
                }
            }
            SidebarTab::Settings => {
                let scroll = self.sidebar_scroll;
                let menu_state = self.settings_menu_state();
                if let Some((row, count)) = menu_state {
                    for index in 0..count {
                        if layout
                            .settings_menu_option(row, count, scroll, index)
                            .contains(x, y)
                        {
                            self.select_settings_menu_row(index);
                            return true;
                        }
                    }
                    if layout.settings_menu_rect(row, count, scroll).contains(x, y) {
                        return true;
                    }
                }
                let expanded = menu_state.map(|(row, count)| {
                    (Some(row), settings_menu_height(self.scale.max(0.75), count))
                });
                let (expanded_row, expanded_h) = expanded.unwrap_or((None, 0.0));
                for index in 0..SETTINGS_ROWS {
                    if layout
                        .settings_row(index, scroll, expanded_row, expanded_h)
                        .contains(x, y)
                    {
                        self.settings_action(index);
                        return true;
                    }
                }
            }
            SidebarTab::Performance => {
                if layout
                    .performance_hud_row(self.sidebar_scroll)
                    .contains(x, y)
                {
                    self.toggle_stats();
                    return true;
                }
            }
        }
        false
    }

    fn info_action(&mut self, index: usize) {
        let request = match index {
            0 => DetailRequest::Like,
            1 => DetailRequest::Coin,
            _ => DetailRequest::Favorite,
        };
        let label = ["点赞", "投币", "收藏"][index.min(2)];
        log::line(&format!("detail action {label}"));
        if self.detail_tx.send(request).is_err() {
            self.show_toast("当前来源没有可用的登录会话", true);
        }
    }

    fn switch_page(&mut self, index: usize) {
        // Clicking the current page is a no-op, but a pending switch may be
        // replaced by a newer click: the receiver swap below drops the old
        // answer, so the last clicked row always wins.
        if index == self.current_page && self.switch_rx.is_none() {
            return;
        }
        let pages = if self.pages.is_empty() {
            self.info
                .as_ref()
                .map(|info| info.pages.clone())
                .unwrap_or_default()
        } else {
            self.pages.clone()
        };
        let Some(page) = pages.get(index) else {
            return;
        };
        let Some(bvid) = self.source.bvid.clone() else {
            return;
        };
        // Leaving this part: keep its position before the old sessions stop.
        self.record_watch_progress();
        let cid = page.cid;
        let quality = self.source.qn;
        // A page change is not a codec choice. Use the user's explicit codec
        // preference when one exists; otherwise re-run automatic selection.
        // Inheriting `self.source.codecid` here is what silently kept the next
        // page on 18 Mbit/s AVC after an automatic AV1/HEVC first page.
        let codecid = self.state.quality_codecid.unwrap_or(0);
        let audio_index = self.source.audio_index;
        log::line(&format!(
            "source switch request bvid={bvid} cid={cid} page={} title={:?}",
            index + 1,
            page.title
        ));
        if self.switch_rx.is_none() {
            // Remember where the old part was, so a failed resolve can put the
            // viewer back where they were instead of on a dead loading card.
            self.switch_from_page = Some(self.current_page);
            self.switch_resume_ms = self.position_now();
        }
        // Optimistic UI: move the 选集 highlight and show the loading card now,
        // not after playurl answers. The old picture/audio stop here, so the
        // spinner is honest about what is happening.
        self.current_page = index;
        self.next_page_prefetch = None;
        self.page_loading = Some(index);
        self.drop_loop_prefetch();
        self.reset_ab_cache();
        self.cur = None;
        self.next = None;
        self.cur_ts_ms = 0;
        self.last_frame_ts = 0;
        self.had_first_frame = false;
        self.starting = true;
        self.playing = false;
        self.requested_play = true;
        self.ended = false;
        self.seek_in_flight = false;
        self.position_ms = 0;
        self.paused_at = 0;
        self.seek_target = 0;
        self.base_ms = 0;
        self.anchor = None;
        self.audio_master = false;
        self.stall_audio_paused = false;
        self.stalled = false;
        self.stall_until = None;
        self.session_started = Instant::now();
        self.danmaku.clear();
        let _ = self.danmaku_layer.clear();
        self.ui_dirty = true;
        // Paint the new selection + loading card before stopping the old
        // decoder/audio. `stop_sessions` can wait for network/decoder threads,
        // and doing it first made the click look ignored until that wait ended.
        self.draw_ui_now();
        self.sync_shared();
        self.stop_sessions();
        let (tx, rx) = std::sync::mpsc::channel();
        // A newer click supersedes a pending one.
        self.switch_rx = Some(rx);
        self.switch_target = Some(index);
        self.switch_started = Some(Instant::now());
        // The ten-second prefetcher already resolved this page's playurl and
        // started its cache workers. Consume that source instead of paying the
        // resolve latency again on the end card.
        let prefetched = self.next_page_source.lock().ok().and_then(|mut slot| {
            if slot.as_ref().map(|(page, _)| *page) == Some(index) {
                slot.take().map(|(_, source)| source)
            } else {
                None
            }
        });
        let session = self.session.clone();
        let danmaku_tx = self.danmaku_tx.clone();
        let _ = std::thread::Builder::new()
            .name("player-source-switch".into())
            .spawn(move || {
                if let Some(source) = prefetched {
                    let duration_ms = source.duration_ms;
                    let Some(cid) = source.cid else {
                        let _ = tx.send(Err("新分P没有 cid".into()));
                        return;
                    };
                    log::line(&format!("next page source reused cid={cid}"));
                    let _ = tx.send(Ok(source));
                    if let Some(session) = session {
                        let items = crate::danmaku::fetch_with_session(
                            session, cid, duration_ms,
                        );
                        let _ = danmaku_tx.send(items);
                    }
                    return;
                }
                let config = PlayerConfig {
                    bvid: Some(bvid),
                    cid: Some(cid),
                    quality_qn: Some(quality),
                    quality_codecid: Some(codecid),
                    audio_index: Some(audio_index),
                    ..Default::default()
                };
                match crate::source::resolve(&config) {
                    Ok(resolved) => {
                        let duration_ms = resolved.source.duration_ms;
                        let source = resolved.source;
                        let Some(cid) = source.cid else {
                            let _ = tx.send(Err("新分P没有 cid".into()));
                            return;
                        };
                        // Switch first, then fetch: the engine clears the old
                        // danmaku when it applies the new source, so fetching
                        // before the switch would throw the new list away.
                        let _ = tx.send(Ok(source));
                        if let Some(session) = resolved.session {
                            let items = crate::danmaku::fetch_with_session(
                                std::sync::Arc::new(session),
                                cid,
                                duration_ms,
                            );
                            let _ = danmaku_tx.send(items);
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(format!("{error:#}")));
                    }
                }
            });
    }

    /// Put the previous 分P back after a failed switch: the optimistic click
    /// already stopped its decoder/audio, so reopen it at the position it had.
    fn restore_previous_page(&mut self, reason: Option<&str>) {
        self.switch_target = None;
        self.page_loading = None;
        let restore_page = self.switch_from_page.take().unwrap_or(self.current_page);
        let resume = self.switch_resume_ms;
        self.current_page = restore_page;
        self.requested_play = true;
        self.had_first_frame = false;
        self.cur = None;
        self.next = None;
        self.cur_ts_ms = 0;
        if let Err(reopen) = self.arm_new_sessions(resume) {
            let text = match reason {
                Some(reason) => {
                    format!("分P切换失败：{reason}；恢复播放失败：{reopen:#}")
                }
                None => format!("恢复播放失败：{reopen:#}"),
            };
            self.report_error(text);
        } else {
            log::line(&format!(
                "source switch reverted page={} at_ms={resume}",
                restore_page + 1
            ));
        }
    }

    fn apply_switched_source(&mut self, new_source: MediaSource) {
        self.reload_danmaku_settings();
        let size_changed =
            new_source.width != self.source.width || new_source.height != self.source.height;
        // Always rebuild the surface on a part switch. Reusing the old swapchain
        // after the decoder/audio sessions were stopped could leave a black
        // picture with audio only; a fresh layer makes the switch deterministic.
        let new_surface = match VideoSurface::new(&self.gpu, new_source.width, new_source.height) {
            Ok(surface) => Some(surface),
            Err(error) => {
                log::line(&format!("page switch surface failed: {error:#}"));
                self.show_toast("新分P的视频层创建失败，保留当前分P", true);
                // The old sessions were stopped when the click landed, so the
                // previous page has to be brought back explicitly.
                self.restore_previous_page(None);
                return;
            }
        };
        let new_pipeline = if size_changed {
            match EnhancePipeline::new(
                &self.gpu.device,
                &self.gpu.context,
                new_source.width,
                new_source.height,
            ) {
                Ok(pipeline) => Some(pipeline),
                Err(error) => {
                    log::line(&format!("enhance fallback on page switch reason={error:#}"));
                    None
                }
            }
        } else {
            None
        };
        if let Some(index) = self.switch_target.take() {
            self.current_page = index;
        }
        self.switch_from_page = None;
        self.reset_danmaku_for_source();
        self.stop_sessions();
        if let Some(surface) = new_surface {
            if let Err(error) = self.composition.set_video_content(&surface.swap) {
                log::line(&format!("page switch composition failed: {error:#}"));
                self.show_toast("切换分P失败，恢复当前分P", true);
                self.restore_previous_page(None);
                return;
            }
            self.video = surface;
            if size_changed {
                self.enhance_pipeline = new_pipeline;
                if self.enhance != EnhanceMode::Off && self.enhance_pipeline.is_none() {
                    self.enhance = EnhanceMode::Off;
                    self.shared.enhance.store(0, Ordering::SeqCst);
                    log::line("page switch enhance disabled reason=pipeline unavailable");
                }
            }
            self.cur = None;
            self.cur_ts_ms = 0;
            self.next = None;
            self.next_tick_at = None;
            self.last_source_present = None;
            self.had_first_frame = false;
        }
        self.source = new_source;
        self.info_scroll_height = -1.0;
        self.events.set_video_aspect(
            self.source.source_width,
            self.source.source_height,
        );
        self.restart_heartbeat_for_source();
        self.reset_storyboard();
        self.settings_menu = None;
        self.duration_ms = self.source.duration_ms;
        self.update_video_rect();
        self.reload_player_prefs();
        self.position_ms = 0;
        self.base_ms = 0;
        self.paused_at = 0;
        self.seek_target = 0;
        self.anchor = None;
        self.playing = false;
        self.requested_play = true;
        self.starting = true;
        self.first_frame_at = None;
        self.seek_in_flight = false;
        self.ended = false;
        self.audio_master = false;
        self.last_frame_ts = 0;
        self.generation = self.generation.saturating_add(1);
        self.danmaku.set_items(Vec::new());
        let _ = self.danmaku_layer.clear();
        if let Err(error) = self.open_sessions(0, false) {
            log::line(&format!("source switch open failed: {error:#}"));
            self.show_toast("分P加载失败", true);
            self.page_loading = None;
            self.switch_from_page = None;
            // The old decoder is already stopped at this point; without the
            // error card the splash would stay up forever.
            self.report_error(format!("分P加载失败：{error:#}"));
            return;
        }
        if let Some(info) = self.info.as_mut() {
            info.current_page = self.current_page;
        }
        self.ui_dirty = true;
        self.sync_shared();
        log::line(&format!(
            "source switched bvid={} cid={} title={:?}",
            self.source.bvid.as_deref().unwrap_or("-"),
            self.source.cid.unwrap_or(0),
            self.source.title
        ));
    }

    /// Replace the video in this window without recreating it. Left-clicking a
    /// card in the shell targets the most recently focused player, so the
    /// window keeps its size, position and chrome; only the media source and
    /// everything tied to it (session, info, pages, danmaku, heartbeat) change.
    fn begin_source_replace(&mut self, config: crate::PlayerConfig) {
        // The old video is about to be replaced; store its position first.
        self.record_watch_progress();
        if self.source_pending {
            // A newer click arrived while the previous source is resolving;
            // run it as soon as that answer lands (last click wins).
            self.queued_replace = Some(config);
            log::line("source replace queued behind in-flight resolve");
            return;
        }
        log::line(&format!(
            "source replace start bvid={:?} cid={:?} title={:?} live={}",
            config.bvid, config.cid, config.title, config.is_live
        ));
        self.pending_replace = Some(config.clone());
        self.failed_replace = None;
        self.stop_sessions();
        self.drop_loop_prefetch();
        // Drop the previous source's info/channel workers so a late detail or
        // danmaku answer cannot paint the old video over the loading card.
        let (detail_tx, _detail_rx_unused) = std::sync::mpsc::channel();
        let (_detail_tx_unused, detail_rx) = std::sync::mpsc::channel();
        self.detail_tx = detail_tx;
        self.detail_rx = detail_rx;
        self.heartbeat = None;
        self.heartbeat_started = false;
        self.session = None;
        self.reset_ab_cache();
        self.ab_swap_wait = None;
        self.ab_cache_playing = false;
        self.ab_cache_cursor = 0;
        self.cur = None;
        self.next = None;
        self.cur_ts_ms = 0;
        self.last_frame_ts = 0;
        self.had_first_frame = false;
        self.starting = true;
        self.loading = true;
        self.playing = false;
        self.requested_play = true;
        self.ended = false;
        self.seek_in_flight = false;
        self.stalled = false;
        self.stall_audio_paused = false;
        self.stall_until = None;
        self.position_ms = 0;
        self.paused_at = 0;
        self.seek_target = 0;
        self.base_ms = 0;
        self.anchor = None;
        self.audio_master = false;
        self.duration_ms = 0;
        self.requested_start_ms = config.start_ms;
        self.session_started = Instant::now();
        self.is_live = config.is_live;
        self.aid = 0;
        self.pages.clear();
        self.current_page = 0;
        self.info = None;
        self.info_scroll_height = -1.0;
        self.comments.clear();
        self.comments_total = 0;
        self.comments_end = false;
        self.comments_cursor.clear();
        self.comments_loading = false;
        self.comments_reload_pending = false;
        self.comments_enter = 1.0;
        self.comments_generation = self.comments_generation.wrapping_add(1);
        self.settings_menu = None;
        self.danmaku.clear();
        let _ = self.danmaku_layer.clear();
        self.shared
            .position_ms
            .store(0, Ordering::SeqCst);
        self.error = None;
        self.source_pending = true;
        let placeholder = if config.title.trim().is_empty() {
            "正在打开视频…"
        } else {
            config.title.trim()
        };
        window::set_title(self.hwnd, &format!("O11A1 · {placeholder}"));
        self.ui_dirty = true;
        self.sync_shared();
        let tx = self.source_tx.clone();
        let danmaku_tx = self.danmaku_tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("player-source-replace".into())
            .spawn(move || {
                let result = crate::resolve_deferred(config, danmaku_tx);
                let _ = tx.send(result);
            })
        {
            self.source_pending = false;
            self.loading = false;
            self.report_error(format!("开始加载视频失败：{error}"));
        }
    }

    /// Install the source that the worker resolved while the window was
    /// already showing its loading card: swap in the real video surface and
    /// start the decode/audio sessions.
    fn apply_deferred_source(&mut self, open: crate::DeferredOpen) {
        // A replacement can arrive while the previous video is still running.
        // Stopping here (instead of only at the caller) makes both the initial
        // open and an in-place replacement deterministic.
        self.stop_sessions();
        self.next_page_prefetch = None;
        if let Ok(mut slot) = self.next_page_source.lock() {
            *slot = None;
        }
        self.reload_danmaku_settings();
        self.reset_ab_cache();
        let crate::DeferredOpen {
            mut source,
            session,
            detail,
        } = open;
        if source.title.trim().is_empty() {
            source.title = self.source.title.clone();
        }
        let title = source.title.clone();
        let size_changed = source.width != self.source.width || source.height != self.source.height;
        let new_surface = if size_changed {
            match VideoSurface::new(&self.gpu, source.width, source.height) {
                Ok(surface) => Some(surface),
                Err(error) => {
                    log::line(&format!("initial source surface failed: {error:#}"));
                    self.loading = false;
                    self.report_error(format!("创建视频层失败：{error:#}"));
                    return;
                }
            }
        } else {
            None
        };
        let new_pipeline = if size_changed {
            match EnhancePipeline::new(
                &self.gpu.device,
                &self.gpu.context,
                source.width,
                source.height,
            ) {
                Ok(pipeline) => Some(pipeline),
                Err(error) => {
                    log::line(&format!(
                        "enhance fallback on initial source reason={error:#}"
                    ));
                    None
                }
            }
        } else {
            None
        };
        if let Some(surface) = new_surface {
            if let Err(error) = self.composition.set_video_content(&surface.swap) {
                log::line(&format!("initial source composition failed: {error:#}"));
                self.loading = false;
                self.report_error(format!("视频层绑定失败：{error:#}"));
                return;
            }
            self.video = surface;
            self.enhance_pipeline = new_pipeline;
            if self.enhance != EnhanceMode::Off && self.enhance_pipeline.is_none() {
                self.enhance = EnhanceMode::Off;
                self.shared.enhance.store(0, Ordering::SeqCst);
            }
            self.cur = None;
            self.cur_ts_ms = 0;
            self.next = None;
            self.next_tick_at = None;
            self.last_source_present = None;
        }
        self.loading = false;
        self.source = source;
        self.info_scroll_height = -1.0;
        self.events.set_video_aspect(
            self.source.source_width,
            self.source.source_height,
        );
        self.session = session;
        if let Some((tx, rx)) = detail {
            self.detail_tx = tx;
            self.detail_rx = rx;
            let _ = self.detail_tx.send(DetailRequest::Info);
            log::line("detail info requested after source resolve");
        }
        window::set_title(self.hwnd, &format!("O11A1 · {title}"));
        if let Some(info) = self.info.as_mut() {
            info.title = title;
            info.current_page = 0;
        }
        self.pages.clear();
        self.current_page = 0;
        self.comments.clear();
        self.comments_total = 0;
        self.comments_end = false;
        self.comments_cursor.clear();
        self.comments_loading = false;
        self.comments_reload_pending = false;
        self.comments_enter = 1.0;
        self.comments_generation = self.comments_generation.wrapping_add(1);
        self.restart_heartbeat_for_source();
        self.reset_storyboard();
        self.settings_menu = None;
        self.duration_ms = self.source.duration_ms;
        // An explicit start (history row, quality reopen, playlist advance)
        // always wins. Otherwise continue where the last watch stopped, as the
        // official player does; `resume_ms` is already filtered by the 3 s /
        // 5 s rule in the resolver.
        let start = if self.requested_start_ms > 0 {
            self.requested_start_ms
        } else {
            self.source.resume_ms.unwrap_or(0)
        };
        let start = clamp_seek(start, self.duration_ms);
        if self.requested_start_ms == 0 && start > 0 {
            log::line(&format!(
                "resume playback bvid={} cid={} start_ms={start}",
                self.source.bvid.as_deref().unwrap_or("-"),
                self.source.cid.unwrap_or(0)
            ));
        }
        self.position_ms = start;
        self.base_ms = start;
        self.paused_at = start;
        self.seek_target = start;
        self.anchor = None;
        self.playing = false;
        self.requested_play = true;
        self.starting = true;
        self.first_frame_at = None;
        self.had_first_frame = false;
        self.seek_in_flight = false;
        self.ended = false;
        self.audio_master = false;
        self.last_frame_ts = start;
        self.generation = self.generation.saturating_add(1);
        self.session_started = Instant::now();
        let display_hz = self.monitor_refresh_hz();
        self.monitor_hz = display_hz;
        self.interp_out_hz = interp::interp_plan(display_hz, self.source.fps, self.interp_hz);
        let out_hz = self.interp_out_hz.unwrap_or_else(|| {
            if self.source.fps.is_finite() && self.source.fps > 1.0 {
                self.source.fps.round().clamp(1.0, 240.0) as u32
            } else {
                60
            }
        });
        self.out_period = Duration::from_secs_f64(1.0 / out_hz.max(1) as f64);
        self.danmaku.set_items(Vec::new());
        let _ = self.danmaku_layer.clear();
        self.refresh_danmaku_geometry();
        self.update_video_rect();
        self.ui_dirty = true;
        log::line(&format!(
            "source attached title={:?} duration_ms={} start_ms={start} qn={} codec={} {}x{}",
            self.source.title,
            self.duration_ms,
            self.source.qn,
            self.source.codec,
            self.source.width,
            self.source.height
        ));
        if let Err(error) = self.open_sessions(start, false) {
            self.report_error(format!("打开播放会话失败：{error:#}"));
            return;
        }
        self.reload_player_prefs();
        self.sync_shared();
        self.start_playlist_prefetch();
    }

    fn settings_action(&mut self, index: usize) {
        if self.is_live {
            match index {
                1 => self.toggle_ontop(),
                2 => self.toggle_audio_only(),
                _ => {}
            }
            return;
        }
        if (index == 0 || index == 2) && !self.source.has_track_menu() {
            self.show_toast("当前来源是直链，没有可切换的画质/音轨", true);
            return;
        }
        // Any settings row other than the open inline menu's own row collapses
        // that menu in the same click, then performs its own action.
        let open_menu_row = self.settings_menu_state().map(|(row, _)| row);
        if open_menu_row.map(|row| row != index).unwrap_or(false) {
            self.settings_menu = None;
            self.ui_dirty = true;
        }
        match index {
            0 => {
                self.settings_menu = if self.settings_menu == Some(SettingsMenu::Quality) {
                    None
                } else {
                    Some(SettingsMenu::Quality)
                };
                self.ui_dirty = true;
                log::line(&format!(
                    "settings menu={:?} rows={}",
                    self.settings_menu,
                    self.quality_rows().len()
                ));
            }
            1 => {
                self.settings_menu = if self.settings_menu == Some(SettingsMenu::DefaultQuality) {
                    None
                } else {
                    Some(SettingsMenu::DefaultQuality)
                };
                self.ui_dirty = true;
                log::line(&format!(
                    "settings menu={:?} rows={}",
                    self.settings_menu,
                    DEFAULT_QUALITY_HEIGHTS.len()
                ));
            }
            2 => {
                self.settings_menu = if self.settings_menu == Some(SettingsMenu::Audio) {
                    None
                } else {
                    Some(SettingsMenu::Audio)
                };
                self.ui_dirty = true;
                log::line(&format!(
                    "settings menu={:?} rows={}",
                    self.settings_menu,
                    self.audio_rows().len()
                ));
            }
            3 => {
                self.settings_menu = if self.settings_menu == Some(SettingsMenu::Interp) {
                    None
                } else {
                    Some(SettingsMenu::Interp)
                };
                self.ui_dirty = true;
                log::line(&format!(
                    "settings menu={:?} rows={}",
                    self.settings_menu,
                    self.interp_rows().len()
                ));
            }
            4 => {
                self.settings_menu = if self.settings_menu == Some(SettingsMenu::Enhance) {
                    None
                } else {
                    Some(SettingsMenu::Enhance)
                };
                self.ui_dirty = true;
                log::line(&format!(
                    "settings menu={:?} rows={}",
                    self.settings_menu,
                    self.enhance_rows().len()
                ));
            }
            5 => self.toggle_ontop(),
            6 => self.toggle_audio_only(),
            _ => {}
        }
    }

    fn quality_rows(&self) -> Vec<QualityRowUi> {
        let mut tracks = self.source.video_tracks.clone();
        tracks.sort_by(|a, b| b.qn.cmp(&a.qn).then(a.codecid.cmp(&b.codecid)));
        let mut rows: Vec<QualityRowUi> = Vec::new();
        for track in &tracks {
            let tier = self
                .source
                .quality_tiers
                .iter()
                .find(|tier| tier.qn == track.qn);
            let detail = tier
                .map(|tier| tier.detail.clone())
                .filter(|detail| !detail.trim().is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "{}×{} · {:.0}fps",
                        track.width, track.height, track.frame_rate
                    )
                });
            rows.push(QualityRowUi {
                qn: track.qn,
                codecid: track.codecid,
                label: track.label.clone(),
                detail,
                locked: false,
                current: track.qn == self.source.qn && track.codecid == self.source.codecid,
            });
        }
        for tier in &self.source.quality_tiers {
            // Every playable representation already has a row; a locked tier has
            // no stream at all and is the 大会员 upsell row.
            if !tier.locked || rows.iter().any(|row| row.qn == tier.qn) {
                continue;
            }
            let detail = if tier.note.trim().is_empty() {
                if tier.detail.trim().is_empty() {
                    "大会员".to_string()
                } else {
                    format!("{} · 大会员", tier.detail)
                }
            } else if tier.detail.trim().is_empty() {
                tier.note.clone()
            } else {
                format!("{} · {}", tier.detail, tier.note)
            };
            rows.push(QualityRowUi {
                qn: tier.qn,
                codecid: 0,
                label: tier.label.clone(),
                detail,
                locked: true,
                current: false,
            });
        }
        rows
    }

    fn audio_rows(&self) -> Vec<AudioRowUi> {
        self.source
            .audio_tracks
            .iter()
            .enumerate()
            .map(|(index, track)| AudioRowUi {
                index,
                label: track.label.clone(),
                detail: format!("{:.0} kbps", track.bandwidth as f64 / 1000.0),
                current: index == self.source.audio_index,
            })
            .collect()
    }

    /// Inline menu parent row and option count, if one is open.
    fn settings_menu_state(&self) -> Option<(usize, usize)> {
        if self.is_live {
            return None;
        }
        match self.settings_menu {
            Some(SettingsMenu::Quality) => Some((0, self.quality_rows().len())),
            Some(SettingsMenu::DefaultQuality) => Some((1, DEFAULT_QUALITY_HEIGHTS.len())),
            Some(SettingsMenu::Audio) => Some((2, self.audio_rows().len())),
            Some(SettingsMenu::Interp) => Some((3, self.interp_rows().len())),
            Some(SettingsMenu::Enhance) => Some((4, self.enhance_rows().len())),
            None => None,
        }
    }

    fn settings_menu_len(&self) -> usize {
        self.settings_menu_state()
            .map(|(_, count)| count)
            .unwrap_or(0)
    }

    fn open_settings_menu(&mut self, menu: SettingsMenu) {
        self.sidebar_open = true;
        self.sidebar_tab = SidebarTab::Settings;
        self.sidebar_scroll = 0.0;
        self.sidebar_scroll_target = 0.0;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.overlay.set_chrome_state(true, self.fullscreen);
        self.settings_menu = Some(menu);
        self.ui_dirty = true;
        log::line(&format!(
            "settings menu={menu:?} rows={}",
            match menu {
                SettingsMenu::Quality => self.quality_rows().len(),
                SettingsMenu::DefaultQuality => DEFAULT_QUALITY_HEIGHTS.len(),
                SettingsMenu::Audio => self.audio_rows().len(),
                SettingsMenu::Interp => self.interp_rows().len(),
                SettingsMenu::Enhance => self.enhance_rows().len(),
            }
        ));
    }

    fn select_settings_menu_row(&mut self, index: usize) {
        match self.settings_menu {
            Some(SettingsMenu::DefaultQuality) => {
                let Some(height) = DEFAULT_QUALITY_HEIGHTS.get(index).copied() else {
                    return;
                };
                self.settings_menu = None;
                self.quality_height_pref = height;
                self.ui_dirty = true;
                let mut stored = o11a1::settings::load();
                stored.player_quality_height = height;
                if let Err(error) = o11a1::settings::save(&stored) {
                    log::line(&format!("default quality save failed: {error:#}"));
                }
                self.show_toast(
                    &format!(
                        "默认画质：{}（下次打开视频生效）",
                        quality_height_label(height)
                    ),
                    false,
                );
                log::line(&format!("default quality height={height}"));
            }
            Some(SettingsMenu::Quality) => {
                let Some(row) = self.quality_rows().get(index).cloned() else {
                    return;
                };
                self.settings_menu = None;
                self.ui_dirty = true;
                if row.locked {
                    log::line(&format!(
                        "quality locked qn={} label={} reason=大会员",
                        row.qn, row.label
                    ));
                    self.show_toast(&format!("{} 需要大会员，当前账号无法播放", row.label), true);
                    return;
                }
                if row.current {
                    self.show_toast("已经是当前画质", false);
                    return;
                }
                log::line(&format!(
                    "quality select qn={} codecid={} label={}",
                    row.qn, row.codecid, row.label
                ));
                self.request_track_switch(TrackRequest::quality(row.qn, row.codecid), "画质");
            }
            Some(SettingsMenu::Audio) => {
                let Some(row) = self.audio_rows().get(index).cloned() else {
                    return;
                };
                self.settings_menu = None;
                self.ui_dirty = true;
                if row.current {
                    self.show_toast("已经是当前音轨", false);
                    return;
                }
                log::line(&format!(
                    "audio select index={} label={}",
                    row.index, row.label
                ));
                self.request_track_switch(TrackRequest::audio(row.index), "音轨");
            }
            Some(SettingsMenu::Interp) => {
                let Some(level) = self.interp_levels().get(index).copied() else {
                    return;
                };
                self.settings_menu = None;
                self.ui_dirty = true;
                log::line(&format!("interp select index={index} level={level:?}"));
                self.select_interp_level(level);
            }
            Some(SettingsMenu::Enhance) => {
                let Some(row) = self.enhance_rows().get(index).cloned() else {
                    return;
                };
                self.settings_menu = None;
                self.ui_dirty = true;
                if row.current {
                    self.show_toast("已经是当前画质增强", false);
                    return;
                }
                log::line(&format!(
                    "enhance select mode={} label={}",
                    row.index, row.label
                ));
                self.select_enhance(EnhanceMode::from_u8(row.index as u8));
            }
            None => {}
        }
    }

    fn request_track_switch(&mut self, request: TrackRequest, kind: &'static str) {
        let Some(session) = self.session.clone() else {
            self.show_toast("当前来源没有可用的 B 站会话", true);
            return;
        };
        let Some(bvid) = self.source.bvid.clone() else {
            self.show_toast("当前是直链，无法切换画质/音轨", true);
            return;
        };
        let Some(cid) = self.source.cid else {
            self.show_toast("当前来源没有 cid", true);
            return;
        };
        let title = self.source.title.clone();
        let current_qn = self.source.qn;
        let current_codecid = self.source.codecid;
        let fallback_duration = self.duration_ms;
        let (tx, rx) = mpsc::channel();
        self.track_rx = Some(rx);
        self.track_kind = Some(kind);
        let _ = std::thread::Builder::new()
            .name("player-track-switch".into())
            .spawn(move || {
                let result = crate::source::resolve_track_switch(
                    &session,
                    &bvid,
                    cid,
                    &title,
                    current_qn,
                    current_codecid,
                    request,
                    fallback_duration,
                );
                let _ = tx.send(result.map_err(|error| format!("{error:#}")));
            });
        self.show_toast(&format!("正在切换{kind}…"), false);
    }

    fn apply_track_switch(&mut self, new_source: MediaSource) {
        self.reset_ab_cache();
        let old_bvid = self.source.bvid.clone();
        let old_cid = self.source.cid;
        if new_source.bvid != old_bvid || new_source.cid != old_cid {
            self.show_toast("播放流不属于当前视频，已忽略", true);
            return;
        }
        let position = self.position_now();
        let was_playing = self.requested_play && !self.ended;
        let target = if self.ended { 0 } else { position };
        let size_changed =
            new_source.width != self.source.width || new_source.height != self.source.height;
        let new_surface = if size_changed {
            match VideoSurface::new(&self.gpu, new_source.width, new_source.height) {
                Ok(surface) => Some(surface),
                Err(error) => {
                    log::line(&format!("track switch surface failed: {error:#}"));
                    self.show_toast("新画质的视频层创建失败，保留当前画质", true);
                    return;
                }
            }
        } else {
            None
        };
        let new_pipeline = if size_changed {
            match EnhancePipeline::new(
                &self.gpu.device,
                &self.gpu.context,
                new_source.width,
                new_source.height,
            ) {
                Ok(pipeline) => Some(pipeline),
                Err(error) => {
                    log::line(&format!(
                        "enhance fallback on track switch reason={error:#}"
                    ));
                    None
                }
            }
        } else {
            None
        };
        let old_source = self.source.clone();
        self.stop_sessions();
        if let Some(surface) = new_surface {
            if let Err(error) = self.composition.set_video_content(&surface.swap) {
                log::line(&format!("track switch composition failed: {error:#}"));
                self.show_toast("切换画质失败，回到原播放流", true);
                self.source = old_source;
                self.events.set_video_aspect(
                    self.source.source_width,
                    self.source.source_height,
                );
                if let Err(reopen) = self.arm_new_sessions(target) {
                    self.report_error(format!("恢复播放失败：{reopen:#}"));
                }
                return;
            }
            self.video = surface;
            if size_changed {
                self.enhance_pipeline = new_pipeline;
                if self.enhance != EnhanceMode::Off && self.enhance_pipeline.is_none() {
                    self.enhance = EnhanceMode::Off;
                    self.shared.enhance.store(0, Ordering::SeqCst);
                    self.show_toast("新尺寸下画质增强不可用，已关闭", true);
                }
            }
            // The old picture buffers are the previous size.
            self.cur = None;
            self.last_source_present = None;
            self.had_first_frame = false;
        }
        self.source = new_source;
        self.events.set_video_aspect(
            self.source.source_width,
            self.source.source_height,
        );
        self.duration_ms = self.source.duration_ms;
        self.settings_menu = None;
        self.update_video_rect();
        self.requested_play = was_playing;
        if let Err(error) = self.arm_new_sessions(target) {
            self.report_error(format!("切换播放流失败：{error:#}"));
            return;
        }
        self.sync_shared();
        log::line(&format!(
            "source track applied bvid={} cid={} qn={} codec={} audio_index={} audio={} size={}x{} position_ms={target}",
            self.source.bvid.as_deref().unwrap_or("-"),
            self.source.cid.unwrap_or(0),
            self.source.qn,
            self.source.codec,
            self.source.audio_index,
            self.source.audio_label,
            self.source.width,
            self.source.height
        ));
        self.show_toast("已切换播放流", false);
        self.remember_track();
    }

    /// Start a fresh decoder/audio pair and arm the post-seek state machine.
    fn arm_new_sessions(&mut self, target: u64) -> Result<()> {
        self.open_sessions(target, false)?;
        self.seek_in_flight = true;
        self.starting = false;
        self.ended = false;
        self.playing = false;
        self.seek_target = target;
        self.stream_start_ms = target;
        self.position_ms = target;
        self.base_ms = target;
        self.anchor = None;
        self.paused_at = target;
        self.audio_master = false;
        self.first_frame_at = None;
        self.next = None;
        self.cur_ts_ms = target;
        self.next_tick_at = None;
        self.stalled = false;
        self.error = None;
        self.gate(false);
        self.status = "定位中…".to_string();
        self.ui_dirty = true;
        self.shared.position_ms.store(target, Ordering::SeqCst);
        Ok(())
    }

    fn report_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::line(&format!("player error text={text}"));
        let position = self.position_now();
        self.paused_at = position;
        self.position_ms = position;
        self.base_ms = position;
        self.anchor = None;
        self.error = Some(text.clone());
        self.stalled = false;
        self.stall_audio_paused = false;
        self.stall_pause = None;
        self.video_lag_since = None;
        self.video_resync = None;
        self.playing = false;
        self.on_user_pause();
        self.gate(false);
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::Pause);
        }
        self.show_toast(&text, true);
        self.ui_dirty = true;
    }

    fn retry(&mut self) {
        // A replacement failed before it could attach: retry that exact
        // request instead of bringing back the old video.
        if let Some(config) = self.failed_replace.take() {
            log::line("retry last source replace");
            self.error = None;
            self.stalled = false;
            self.begin_source_replace(config);
            return;
        }
        // The initial resolve itself failed: there is no stream to reconnect,
        // so run the window's source lookup again.
        if self.source.video_url.is_empty() {
            self.restart_source_resolution();
            return;
        }
        let target = if self.ended {
            0
        } else {
            self.position_now().max(self.paused_at)
        };
        log::line(&format!(
            "retry request target_ms={target} had_error={}",
            u8::from(self.error.is_some())
        ));
        self.error = None;
        self.stalled = false;
        self.stop_sessions();
        self.requested_play = true;
        if let Err(error) = self.arm_new_sessions(target) {
            self.report_error(format!("重试失败：{error:#}"));
            return;
        }
        log::line("retry sessions reopened");
        self.show_toast("正在重新连接…", false);
    }

    /// Re-run `source::resolve` for a window whose first lookup failed. The
    /// result arrives on the same channel the initial open used.
    fn restart_source_resolution(&mut self) {
        if self.source_pending {
            return;
        }
        log::line("source resolve retry requested");
        self.error = None;
        self.stalled = false;
        self.loading = true;
        self.source_pending = true;
        self.ui_dirty = true;
        let config = self.open_config.clone();
        let tx = self.source_tx.clone();
        let danmaku_tx = self.danmaku_tx.clone();
        let _ = std::thread::Builder::new()
            .name("player-source-resolve".into())
            .spawn(move || {
                let result = crate::resolve_deferred(config, danmaku_tx);
                let _ = tx.send(result);
            });
        self.show_toast("正在重新打开视频…", false);
    }

    // ------------------------------------------------------------- 睡眠定时

    fn sleep_label(&self) -> String {
        match (self.sleep_choice, self.sleep_deadline) {
            (0, _) => "关闭".to_string(),
            (_, Some(deadline)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                format_time(remaining.as_millis().min(u64::MAX as u128) as u64)
            }
            (minutes, None) => format!("{minutes} 分"),
        }
    }

    fn set_sleep_minutes(&mut self, minutes: u32) {
        if minutes == 0 {
            self.sleep_choice = 0;
            self.sleep_deadline = None;
            log::line("sleep timer off");
            self.show_toast("睡眠定时已关闭", false);
        } else {
            self.sleep_choice = minutes;
            self.sleep_deadline = Some(Instant::now() + Duration::from_secs(minutes as u64 * 60));
            log::line(&format!("sleep timer set minutes={minutes}"));
            self.show_toast(&format!("睡眠定时：{minutes} 分钟"), false);
        }
        self.ui_dirty = true;
    }

    fn cycle_sleep(&mut self) {
        const CHOICES: [u32; 5] = [0, 15, 30, 60, 90];
        let index = CHOICES
            .iter()
            .position(|choice| *choice == self.sleep_choice)
            .unwrap_or(0);
        self.set_sleep_minutes(CHOICES[(index + 1) % CHOICES.len()]);
    }

    fn tick_sleep(&mut self) {
        let Some(deadline) = self.sleep_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.sleep_deadline = None;
        self.sleep_choice = 0;
        log::line("sleep timer expired action=pause");
        self.pause();
        self.show_toast("睡眠定时已到，已暂停播放", false);
        self.ui_dirty = true;
    }

    fn check_sessions(&mut self) {
        if self.loading {
            // No decoder/audio exists until the source resolves; the loading
            // card covers this window.
            return;
        }
        if self.error.is_some() {
            return;
        }
        if let Some(audio) = &self.audio {
            if audio.clock.failed.load(Ordering::SeqCst) {
                self.report_error("音频输出失败，点击重试重新打开音轨");
                return;
            }
        }
        if self.starting
            && self.cur.is_none()
            && !self.had_first_frame
            && self
                .decoder
                .as_ref()
                .map(|decoder| decoder.queue.is_closed())
                .unwrap_or(false)
        {
            // GPU-only: a closed pipe with no picture means the hardware
            // decoder refused this stream/profile. Report the driver's own
            // reason instead of waiting for the generic 30 s timeout.
            let tail = self
                .decoder
                .as_ref()
                .map(|decoder| decoder.stderr_tail())
                .unwrap_or_default();
            self.report_error(if tail.is_empty() {
                "GPU 硬解失败（解码器已退出），点击重试".to_string()
            } else {
                format!("GPU 硬解失败：{tail}")
            });
            return;
        }
        if self.starting
            && self.cur.is_none()
            && self.session_started.elapsed() >= Duration::from_secs(30)
        {
            self.report_error("视频加载超时，点击重试重新连接");
        }
    }

    /// Post the text in the 弹幕 popup through the WBI-signed `dm/post` API.
    fn send_danmaku(&mut self) {
        let text = self.dm_input.trim().to_string();
        if text.is_empty() {
            self.show_toast("请输入弹幕内容", true);
            return;
        }
        if self.dm_send_rx.is_some() {
            self.show_toast("正在发送上一条弹幕…", false);
            return;
        }
        let (Some(session), Some(bvid), Some(cid)) = (
            self.session.clone(),
            self.source.bvid.clone(),
            self.source.cid,
        ) else {
            self.show_toast("当前来源不支持发送弹幕", true);
            return;
        };
        let progress = self.position_now();
        let (tx, rx) = mpsc::channel();
        self.dm_send_rx = Some(rx);
        self.dm_input_focus = false;
        self.show_toast("正在发送弹幕…", false);
        let _ = std::thread::Builder::new()
            .name("player-dm-send".into())
            .spawn(move || {
                let result = match o11a1::bilibili::shared_runtime() {
                    Some(runtime) => runtime
                        .block_on(
                            session.danmaku_post(cid, &bvid, &text, progress, 1, 25, 16_777_215, 0),
                        )
                        .map_err(|error| format!("{error:#}")),
                    None => Err("网络运行时不可用".to_string()),
                };
                let message = match result {
                    Ok((0, _)) => (0, "弹幕已发送".to_string()),
                    Ok((code, message)) => (code, format!("发送失败 code={code}：{message}")),
                    Err(error) => (-1, format!("发送失败：{error}")),
                };
                let _ = tx.send(message);
            });
    }

    fn handle_popup_click(&mut self, x: f32, y: f32, layout: &crate::overlay::Layout) -> bool {
        if layout.popup_toggle.contains(x, y) {
            self.toggle_danmaku();
            return true;
        }
        if layout.popup.contains(x, y) {
            let slop = 12.0 * layout.scale;
            if (layout.popup_opacity.top - slop..=layout.popup_opacity.bottom + slop).contains(&y)
                && x >= layout.popup_opacity.left
            {
                self.popup_drag = Some(0);
                self.drag_popup(0, x, layout);
                return true;
            }
            if (layout.popup_font.top - slop..=layout.popup_font.bottom + slop).contains(&y)
                && x >= layout.popup_font.left
            {
                self.popup_drag = Some(2);
                self.drag_popup(2, x, layout);
                return true;
            }
            if (layout.popup_speed.top - slop..=layout.popup_speed.bottom + slop).contains(&y)
                && x >= layout.popup_speed.left
            {
                self.popup_drag = Some(1);
                self.drag_popup(1, x, layout);
                return true;
            }
            for (index, rect) in layout.popup_area.iter().enumerate() {
                if rect.contains(x, y) {
                    self.set_danmaku_area([25, 50, 75, 100][index]);
                    return true;
                }
            }
            for (index, rect) in layout.popup_blocks.iter().enumerate() {
                if rect.contains(x, y) {
                    self.toggle_block(index);
                    return true;
                }
            }
            if layout.popup_density.contains(x, y) {
                self.density_on = !self.density_on;
                self.danmaku.set_density(self.density_on);
                self.ui_dirty = true;
                log::line(&format!("danmaku density={}", u8::from(self.density_on)));
                return true;
            }
            return true;
        }
        false
    }

    fn drag_popup(&mut self, kind: u8, x: f32, layout: &crate::overlay::Layout) {
        let track = match kind {
            0 => layout.popup_opacity,
            2 => layout.popup_font,
            _ => layout.popup_speed,
        };
        let fraction = ((x - track.left) / track.width().max(1.0)).clamp(0.0, 1.0);
        match kind {
            0 => self.set_danmaku_opacity((fraction * 100.0).round() as u32),
            2 => self.set_danmaku_font((50.0 + fraction * 100.0).round() as u32),
            _ => self.set_danmaku_speed((50.0 + fraction * 150.0).round() as u32),
        }
    }

    fn player_key_action(&self, key: u32) -> Option<usize> {
        self.player_keys
            .iter()
            .position(|candidate| player_candidate_vk(*candidate) == Some(key))
    }

    fn run_player_key_action(&mut self, action: usize) {
        match action {
            0 => self.toggle_play(),
            1 => {
                let target = self.position_now().saturating_sub(5_000);
                self.seek(target);
            }
            2 => {
                let target = self.position_now().saturating_add(5_000);
                self.seek(target);
            }
            3 => {
                let value = (self.volume as i32 + 5).clamp(0, 100) as u8;
                self.set_volume(value);
            }
            4 => {
                let value = (self.volume as i32 - 5).clamp(0, 100) as u8;
                self.set_volume(value);
            }
            5 => self.toggle_mute(),
            6 => {
                if !self.mini {
                    self.toggle_fullscreen();
                }
            }
            7 => {
                if self.fullscreen {
                    self.toggle_fullscreen();
                }
            }
            8 => self.toggle_danmaku(),
            9 => self.info_action(0),
            10 => self.screenshot(),
            _ => {}
        }
    }

    fn handle_key(&mut self, key: u32) {
        if self.dm_input_focus {
            match key {
                0x1B => {
                    self.dm_input_focus = false;
                }
                0x0D => self.send_danmaku(),
                0x08 => {
                    self.dm_input.pop();
                }
                _ => {}
            }
            self.ui_dirty = true;
            return;
        }
        // Escape is structural: it always peels the topmost UI layer before a
        // configured 退出全屏 binding can consume it.
        if key == 0x1B {
            if self.dm_menu.is_some() {
                self.close_dm_menu();
            } else if self.settings_menu.is_some() {
                self.settings_menu = None;
                self.ui_dirty = true;
            } else if self.danmaku_popup {
                self.danmaku_popup = false;
                self.dm_input_focus = false;
                self.ui_dirty = true;
            } else if self.precision_step_open {
                self.precision_step_open = false;
                self.ui_dirty = true;
            } else if self.precision_open {
                self.close_precision();
            } else if self.speed_open {
                self.close_speed();
            } else if self.sidebar_open {
                self.set_sidebar(false, self.sidebar_tab);
            } else if self.fullscreen {
                self.toggle_fullscreen();
            } else if self.mini {
                self.set_mini(false);
            } else {
                self.quit = true;
            }
            return;
        }
        // Space is a fixed transport key: even a stale/remapped key table must
        // not turn it into fullscreen or leave playback without a shortcut.
        if key == 0x20 {
            self.toggle_play();
            return;
        }
        if let Some(action) = self.player_key_action(key) {
            self.run_player_key_action(action);
            return;
        }
        match key {
            0x4D => {
                // Ctrl/Alt+M stays the mini shortcut even when M itself was
                // remapped to another player action.
                let ctrl_or_alt = unsafe {
                    GetKeyState(VK_CONTROL.0 as i32) < 0 || GetKeyState(VK_MENU.0 as i32) < 0
                };
                if ctrl_or_alt {
                    self.toggle_mini();
                }
            }
            0x7A => {
                if !self.mini {
                    self.toggle_fullscreen();
                }
            }
            0x49 => self.cycle_interp(),
            0x45 => self.cycle_enhance(),
            0x50 => {
                self.toggle_stats();
            }
            0x41 => self.cycle_aspect(),
            0x54 => self.toggle_ontop(),
            0x56 => self.toggle_audio_only(),
            0xBD => self.step_brightness(-0.1),
            0xBB => self.step_brightness(0.1),
            0x30 => self.set_brightness(1.0),
            0xDB => self.cycle_speed(-1),
            0xDD => self.cycle_speed(1),
            _ => {}
        }
    }

    fn toggle_fullscreen(&mut self) {
        if self.mini {
            return;
        }
        window::toggle_fullscreen(self.hwnd, &mut self.fullscreen, &mut self.saved_window);
        if !self.fullscreen
            && self.sidebar_open != self.sidebar_extra
            && !window::is_maximized(self.hwnd)
        {
            let want = self.sidebar_open;
            self.sidebar_extra = want;
            let delta = if want {
                self.sidebar_delta()
            } else {
                -self.sidebar_delta()
            };
            window::grow_width(self.hwnd, delta);
        }
        if let Err(error) = window::set_topmost(self.hwnd, self.ontop) {
            log::line(&format!("fullscreen topmost failed: {error:#}"));
        }
        self.shared
            .topmost
            .store(window::is_topmost(self.hwnd), Ordering::SeqCst);
        self.client = window::client_size(self.hwnd);
        if let Err(error) = self
            .overlay
            .resize(self.client.0, self.client.1, self.scale)
        {
            log::line(&format!("overlay resize (fullscreen) failed: {error:#}"));
        }
        if let Err(error) = self.composition.set_root_clip(self.client.0, self.client.1) {
            log::line(&format!("composition clip (fullscreen) failed: {error:#}"));
        }
        self.overlay
            .set_chrome_state(self.sidebar_open, self.fullscreen);
        self.update_video_rect();
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.ui_dirty = true;
        log::line(&format!("fullscreen={}", u8::from(self.fullscreen)));
    }

    /// Apply the newest size seen during a drag/resize, once per frame. The
    /// overlay and danmaku swapchains are already monitor-sized, so this is a
    /// layout update plus one DComp commit; the old UI frame stays on screen
    /// until the gesture settles.
    fn flush_live_resize(&mut self) {
        let Some((width, height)) = self.pending_live_resize.take() else {
            return;
        };
        if !self.interacting || width < 32 || height < 32 {
            return;
        }
        self.apply_client_geometry_live(width, height);
    }

    /// Interaction geometry: clip and video transform only. No overlay layout
    /// redraw, no shadow, no rounded region, no hoisted UI text.
    fn apply_client_geometry_live(&mut self, width: u32, height: u32) {
        self.client = (width.max(1), height.max(1));
        if self.interaction_resizing {
            let start = self.interaction_start_client;
            if start.0 > 0 && start.1 > 0 {
                let scale_x = self.client.0 as f32 / start.0 as f32;
                let scale_y = self.client.1 as f32 / start.1 as f32;
                if let Err(error) = self.composition.set_overlay_scale(scale_x, scale_y) {
                    log::line(&format!("interaction overlay scale failed: {error:#}"));
                }
            }
        }
        if let Err(error) = self.overlay.resize(self.client.0, self.client.1, self.scale) {
            log::line(&format!("interaction layout failed: {error:#}"));
        }
        let (x, y, picture_w, picture_h) =
            self.overlay.layout.stage_video_bounds(
                self.source.width,
                self.source.height,
                AspectMode::Fit,
            );
        let scale_x = picture_w / self.source.width.max(2) as f32;
        let scale_y = picture_h / self.source.height.max(2) as f32;
        if let Err(error) = self
            .composition
            .set_video_layout(self.client.0, self.client.1, x, y, scale_x, scale_y)
        {
            log::line(&format!("interaction video layout failed: {error:#}"));
        }
    }

    fn apply_client_geometry(&mut self) {
        self.reconcile_sidebar_window();
        let (width, height) = self.client;
        if let Err(error) = self.overlay.resize(width, height, self.scale) {
            log::line(&format!("overlay resize failed: {error:#}"));
        }
        if self.external {
            if let Err(error) = self.overlay.clear() {
                log::line(&format!("overlay clear failed: {error:#}"));
            }
        }
        if let Err(error) = self.composition.set_root_clip(width, height) {
            log::line(&format!("composition clip failed: {error:#}"));
        }
        self.update_video_rect();
        self.ui_dirty = true;
    }

    fn set_mini(&mut self, mini: bool) {
        if self.mini == mini {
            return;
        }
        if mini {
            if self.fullscreen {
                self.toggle_fullscreen();
            }
            self.sidebar_open = false;
            if self.sidebar_extra {
                self.sidebar_extra = false;
                window::grow_width(self.hwnd, -self.sidebar_delta());
            }
            let window = window::outer_rect(self.hwnd);
            let client = window::client_size(self.hwnd);
            self.mini_saved = Some(MiniSaved { window, client });
            if let Err(error) = window::set_mini(self.hwnd) {
                log::line(&format!("mini enter failed: {error:#}"));
                self.mini_saved = None;
                return;
            }
            self.mini = true;
            self.mini_hint_until = Some(Instant::now() + Duration::from_secs(2));
            self.dragging = false;
            self.drag_preview = None;
            self.popup_drag = None;
            self.sidebar_open = false;
            self.danmaku_popup = false;
            self.chrome_shown = false;
            self.overlay
                .set_chrome_state(self.sidebar_open, self.fullscreen);
            self.client = window::client_size(self.hwnd);
            self.apply_client_geometry();
            self.shared.mini.store(true, Ordering::SeqCst);
            self.shared
                .topmost
                .store(window::is_topmost(self.hwnd), Ordering::SeqCst);
            self.ui_dirty = true;
            log::line(&format!(
                "mini=1 client={}x{} saved_client={}x{} saved_rect={}x{} topmost={}",
                self.client.0,
                self.client.1,
                client.0,
                client.1,
                window.right - window.left,
                window.bottom - window.top,
                u8::from(window::is_topmost(self.hwnd))
            ));
        } else {
            let Some(saved) = self.mini_saved.take() else {
                self.mini = false;
                self.mini_hint_until = None;
                return;
            };
            if let Err(error) = window::restore_rect(self.hwnd, saved.window) {
                log::line(&format!("mini restore failed: {error:#}"));
                self.mini_saved = Some(saved);
                return;
            }
            self.mini = false;
            self.mini_hint_until = None;
            self.chrome_shown = true;
            self.last_mouse_activity = Instant::now();
            self.overlay
                .set_chrome_state(self.sidebar_open, self.fullscreen);
            // Mini is always topmost; normal mode returns to the state the
            // user had chosen with T before entering mini.
            if let Err(error) = window::set_topmost(self.hwnd, self.ontop) {
                log::line(&format!("mini restore topmost failed: {error:#}"));
            }
            self.client = window::client_size(self.hwnd);
            self.apply_client_geometry();
            self.shared.mini.store(false, Ordering::SeqCst);
            self.shared
                .topmost
                .store(window::is_topmost(self.hwnd), Ordering::SeqCst);
            self.ui_dirty = true;
            log::line(&format!(
                "mini=0 client={}x{} restored={}x{} topmost={}",
                self.client.0,
                self.client.1,
                saved.client.0,
                saved.client.1,
                u8::from(window::is_topmost(self.hwnd))
            ));
        }
    }

    fn toggle_mini(&mut self) {
        let desired = !self.mini;
        self.set_mini(desired);
    }

    fn update_video_rect(&mut self) {
        // Always read the live client rect: the player window can be resized by
        // the sidebar, maximize, DPI move or the user drag, and a cached size
        // would leave the picture at the old scale (a tiny video in the corner).
        let client = window::client_size(self.hwnd);
        self.update_video_rect_at(client.0, client.1);
    }

    /// Same as [`Self::update_video_rect`] but for a client size the window is
    /// about to have. The sidebar needs the *new* width before `WM_SIZE` lands,
    /// otherwise the lane is subtracted inside the old width and the picture
    /// flashes at half size for a frame.
    fn update_video_rect_at(&mut self, client_width: u32, client_height: u32) {
        self.client = (client_width.max(1), client_height.max(1));
        let inset = if self.sidebar_open
            && !self.external
            && !self.fullscreen
            && !self.mini
            && !window::is_maximized(self.hwnd)
        {
            self.sidebar_delta().max(0)
        } else {
            0
        };
        self.events.set_video_inset_right(inset);
        let (x, y, width, height) = self.overlay.layout.stage_video_bounds(
            self.source.width,
            self.source.height,
            AspectMode::Fit,
        );
        let scale_x = width / self.source.width.max(2) as f32;
        let scale_y = height / self.source.height.max(2) as f32;
        if let Err(error) = self.composition.set_video_transform(x, y, scale_x, scale_y) {
            log::line(&format!("video transform failed: {error:#}"));
        }
        self.refresh_danmaku_geometry();
    }

    fn refresh_danmaku_geometry(&mut self) {
        let band = danmaku_band(
            &self.overlay.layout,
            &self.source,
            self.mini,
            AspectMode::Fit,
        );
        let position = self.position_now();
        self.danmaku.update(position);
        self.danmaku_band = band;
        self.danmaku.relayout(band);
        // The font size follows the window height, so the layer's cached D2D
        // text formats have to be pushed here as well. Without this the first
        // fullscreen / resize keeps the startup size until a slider is touched.
        if let Err(error) = self.danmaku_layer.set_fonts(self.danmaku.font_sizes()) {
            log::line(&format!("danmaku geometry font update failed: {error:#}"));
        }
        log::line(&format!(
            "danmaku font geometry model={:?} layer={:?} window_height={}",
            self.danmaku.font_sizes(),
            self.danmaku_layer.font_sizes(),
            band.window_height
        ));
        let _ = self.danmaku_layer.resize(self.client.0, self.client.1);
        // Repaint the retained comments immediately at the new geometry.
        self.redraw_danmaku_now();
        log::line(&format!(
            "danmaku layout client={}x{} band={}x{} rows_ready=1 active={}",
            self.client.0,
            self.client.1,
            band.width,
            band.height,
            self.danmaku.active_count()
        ));
    }

    // ------------------------------------------------------------ storyboard

    fn reset_storyboard(&mut self) {
        if let Some(tx) = self.storyboard_tx.take() {
            let _ = tx.send(storyboard::Request::Stop);
        }
        self.storyboard_rx = None;
        self.storyboard_meta = None;
        self.storyboard_cell = None;
        self.storyboard_pending = None;
        self.storyboard_failed = false;
        self.hover_preview = None;
        self.hover_pinned = false;
    }

    fn ensure_storyboard(&mut self) {
        if self.storyboard_tx.is_some() || self.storyboard_failed {
            return;
        }
        let (Some(session), Some(bvid), Some(cid)) = (
            self.session.clone(),
            self.source.bvid.clone(),
            self.source.cid,
        ) else {
            return;
        };
        let (tx, rx) = storyboard::spawn(session, bvid, cid);
        let _ = tx.send(storyboard::Request::Init);
        self.storyboard_tx = Some(tx);
        self.storyboard_rx = Some(rx);
        log::line(&format!(
            "storyboard request bvid={} cid={cid}",
            self.source.bvid.as_deref().unwrap_or("-")
        ));
    }

    fn preview_time(&self) -> Option<u64> {
        self.drag_preview.or(self.hover_preview)
    }

    fn request_storyboard_cell(&mut self, time_ms: u64) {
        let Some(meta) = &self.storyboard_meta else {
            return;
        };
        let index = cell_index_for(&meta.times, time_ms);
        if self.storyboard_cell.as_ref().map(|(cached, _)| *cached) == Some(index) {
            return;
        }
        if self.storyboard_pending == Some(index) {
            return;
        }
        self.storyboard_pending = Some(index);
        if let Some(tx) = &self.storyboard_tx {
            let _ = tx.send(storyboard::Request::Cell { time_ms });
        }
    }

    fn update_hover_preview(&mut self) {
        if self.hover_pinned {
            if let Some(time) = self.hover_preview {
                self.ensure_storyboard();
                self.request_storyboard_cell(time);
            }
            return;
        }
        let layout = self.overlay.layout;
        let on_track = !self.mini
            && self.chrome_shown
            && self.duration_ms > 0
            && self
                .pointer
                .map(|(x, y)| layout.progress_hit(x, y))
                .unwrap_or(false);
        if !on_track {
            if self.hover_preview.take().is_some() {
                self.ui_dirty = true;
            }
            return;
        }
        let Some((x, _)) = self.pointer else {
            return;
        };
        let fraction = layout.progress_fraction(x);
        let time = (fraction as f64 * self.duration_ms as f64).round() as u64;
        if self.hover_preview != Some(time) {
            self.hover_preview = Some(time);
            self.ui_dirty = true;
        }
        self.ensure_storyboard();
        self.request_storyboard_cell(time);
    }

    fn handle_storyboard(&mut self, event: storyboard::Event) {
        match event {
            storyboard::Event::Ready(meta) => {
                self.storyboard_meta = Some(meta);
                self.ui_dirty = true;
                if let Some(time) = self.preview_time() {
                    self.request_storyboard_cell(time);
                }
            }
            storyboard::Event::Cell { index, image } => {
                let wanted = self
                    .preview_time()
                    .and_then(|time| {
                        self.storyboard_meta
                            .as_ref()
                            .map(|meta| cell_index_for(&meta.times, time))
                    })
                    .unwrap_or(usize::MAX);
                if self.storyboard_pending == Some(index) || wanted == index {
                    self.storyboard_pending = None;
                    self.storyboard_cell = Some((index, image));
                    self.ui_dirty = true;
                }
            }
            storyboard::Event::Failed => {
                self.storyboard_failed = true;
                self.storyboard_meta = None;
                self.storyboard_cell = None;
                self.ui_dirty = true;
            }
        }
    }

    fn install_danmaku(&mut self, items: Vec<DanmakuItem>) {
        self.live_danmaku = items;
        if self.history_date.is_some() {
            log::line(&format!(
                "danmaku live fetched n={} view=history (overlay unchanged)",
                self.live_danmaku.len()
            ));
            return;
        }
        self.apply_danmaku_items(self.live_danmaku.clone(), "live");
    }

    fn apply_danmaku_items(&mut self, items: Vec<DanmakuItem>, view: &str) {
        let total = items.len();
        self.danmaku.set_items(items);
        self.heatmap = build_heatmap(self.danmaku.items(), self.duration_ms);
        self.danmaku_layer.reset_layouts();
        let position = self.position_now();
        self.danmaku.reseed(position);
        if let Err(error) = self.danmaku_layer.set_fonts(self.danmaku.font_sizes()) {
            log::line(&format!("danmaku set fonts failed: {error:#}"));
        }
        let _ = self.danmaku_layer.clear();
        self.next_danmaku_draw = Instant::now();
        if view == "live" {
            self.shared
                .danmaku_total
                .store(total.min(u32::MAX as usize) as u32, Ordering::SeqCst);
        }
        self.ui_dirty = true;
        log::line(&format!(
            "danmaku {} n={total} position_ms={position} visible={} view={view}",
            if view == "live" {
                "loaded"
            } else {
                "history applied"
            },
            u8::from(self.danmaku_visible)
        ));
    }

    /// Draw the current active set immediately, even while paused. Used after a
    /// sender block removes the only on-screen comment: the regular 120 Hz path
    /// would otherwise early-return on an empty frame and leave the removed
    /// text visible on the composition layer.
    fn redraw_danmaku_now(&mut self) {
        if !self.danmaku_visible {
            let _ = self.danmaku_layer.clear();
            return;
        }
        let position = self.position_now();
        let _ = self.danmaku.update(position);
        let frame = self.danmaku.frame(position);
        let band = self.danmaku_band;
        let clip = Rect {
            left: band.left,
            top: band.top,
            right: band.left + band.width,
            bottom: band.top + band.height,
        };
        if let Err(error) = self.danmaku_layer.draw(&frame, clip, frame.is_empty()) {
            log::line(&format!("danmaku immediate draw failed: {error:#}"));
        }
        self.next_danmaku_draw = Instant::now();
    }

    fn draw_danmaku(&mut self) {
        if self.interacting
            || !self.danmaku_visible
            || self.danmaku.is_empty()
            || self.danmaku.settings().opacity <= 0.0
            || !self.playing
        {
            return;
        }
        // The comment layer runs on the display grid, independent of the video
        // rate: 120 Hz panels get 120 Hz scrolling comments, a 60 Hz panel gets
        // 60. An absolute deadline grid is used instead of `last + interval`,
        // so a 1-2 ms loop-sleep jitter cannot skip a whole display slot.
        let hz = self.monitor_hz.clamp(30, 120);
        let interval = Duration::from_secs_f64(1.0 / f64::from(hz));
        let now = Instant::now();
        // Present up to 0.6 ms early: DWM shows the frame on the next refresh
        // anyway, and this absorbs timer/scheduler granularity.
        if now + Duration::from_micros(600) < self.next_danmaku_draw {
            return;
        }
        let position = self.position_now();
        let changed = self.danmaku.update(position);
        let frame = self.danmaku.frame(position);
        if frame.is_empty() && !changed {
            self.advance_danmaku_deadline(now, interval);
            return;
        }
        let band = self.danmaku_band;
        let clip = Rect {
            left: band.left,
            top: band.top,
            right: band.left + band.width,
            bottom: band.top + band.height,
        };
        let force_clear = frame.is_empty();
        if let Err(error) = self.danmaku_layer.draw(&frame, clip, force_clear) {
            log::line(&format!("danmaku draw failed: {error:#}"));
        }
        self.advance_danmaku_deadline(now, interval);
    }

    /// Keep the next present on the display grid. If the engine fell more than
    /// one frame behind (heavy video frame, UI draw, resize), restart from now
    /// instead of firing a catch-up burst.
    fn advance_danmaku_deadline(&mut self, now: Instant, interval: Duration) {
        let next = self.next_danmaku_draw + interval;
        self.next_danmaku_draw = if next <= now { now + interval } else { next };
    }

    // -------------------------------------------------------------------- UI

    /// Paint one frame synchronously. Sidebar open/close uses this so the
    /// lane and the picture are committed in the same turn as the window
    /// rectangle instead of waiting up to one UI interval.
    fn draw_ui_now(&mut self) {
        if let Err(error) = self.draw_ui_with(true) {
            log::line(&format!("overlay draw (immediate) failed: {error:#}"));
        } else {
            let hz = self.monitor_hz.clamp(60, 120) as f32;
            self.next_ui_draw = Instant::now() + Duration::from_secs_f32(1.0 / hz);
            self.ui_dirty = false;
        }
    }

    /// Whether the overlay needs the full animation grid. Hover, menus, the
    /// sidebar, the volume OSD, a toast and the precision/speed morph all
    /// animate; plain playback with the chrome hidden does not, and can be
    /// repainted at 10 Hz instead of the display refresh.
    fn ui_fast(&self) -> bool {
        self.interacting
            || self.dragging
            || self.chrome_shown
            || self.sidebar_open
            || self.danmaku_popup
            || self.dm_menu.is_some()
            || self.toast.is_some()
            || self.volume_wheel_until.is_some()
            || self.drag_preview.is_some()
            || self.hover_preview.is_some()
            || self.precision_open
            || self.speed_open
            || self.loop_menu_open
            || self.settings_menu.is_some()
            || self.loading
            || self.starting
            || self.stalled
            || self.error.is_some()
            || self.page_loading.is_some()
            || (self.precision_anim > 0.0001 && self.precision_anim < 0.9999)
    }

    fn draw_ui(&mut self) -> Result<()> {
        self.draw_ui_with(false)
    }

    fn draw_ui_with(&mut self, sync: bool) -> Result<()> {
        if self.interacting {
            // The window proc owns the drag; keeping the last overlay frame
            // avoids a full D2D relayout on every resize message.
            return Ok(());
        }
        if self.external {
            let preview = self.preview_time().and_then(|time| {
                self.storyboard_cell
                    .as_ref()
                    .map(|(_, image)| (time, image.clone()))
            });
            match preview {
                Some((time, image)) => {
                    let fraction = if self.duration_ms > 0 {
                        (time as f32 / self.duration_ms as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    self.overlay.draw_storyboard_preview(&image, fraction)?;
                    self.external_preview_visible = true;
                }
                None => {
                    if self.external_preview_visible {
                        self.overlay.clear()?;
                        self.external_preview_visible = false;
                    }
                }
            }
            return Ok(());
        }
        let buffering = self.error.is_none()
            && (self.loading
                || self.stalled
                || (self.starting && self.cur.is_some())
                || self.wanted_status() == "正在缓冲…");
        let storyboard_cell = match (
            self.preview_time(),
            &self.storyboard_meta,
            &self.storyboard_cell,
        ) {
            (Some(time), Some(meta), Some((index, image)))
                if cell_index_for(&meta.times, time) == *index =>
            {
                Some(StoryboardCellUi {
                    time_ms: time,
                    image: image.clone(),
                })
            }
            _ => None,
        };
        let model = UiModel {
            title: self.source.title.clone(),
            playing: self.playing,
            loop_mode: self.loop_mode,
            loop_menu_open: self.loop_menu_open,
            position_ms: self.drag_preview.unwrap_or(self.position_ms),
            duration_ms: self.duration_ms,
            volume: self.volume,
            muted: self.muted,
            volume_wheel: self
                .volume_wheel_until
                .map(|until| Instant::now() < until)
                .unwrap_or(false),
            interp: self.interp,
            interp_hz: self.interp_hz,
            enhance: self.enhance,
            speed: self.speed,
            speed_preview: self.speed_preview,
            speed_base: self.speed_base,
            panel: self.panel,
            fps: self.fps,
            danmaku_fps: self.danmaku_fps,
            ui_fps: self.ui_fps,
            source_fps: self.source.fps,
            generated: self.generated,
            blend_us: self.last_blend_us,
            stats: self.stats,
            loaded_bytes: self.cache_done,
            total_bytes: self.cache_total,
            status: self.status.clone(),
            source_size: (self.source.width, self.source.height),
            qn_label: self.source.qn_label.clone(),
            codec: self.source.codec.clone(),
            drag_preview: self.drag_preview,
            precision_dragging: self.precision_dragging,
            precision_step_open: false,
            precision_mix: self.precision_anim,
            ab_a: self.ab_a,
            ab_b: self.ab_b,
            frame_step: self.frame_step,
            heatmap: self.heatmap,
            tuner_offset: self.tuner_offset,
            danmaku_visible: self.danmaku_visible,
            danmaku_total: self.danmaku.total(),
            live_danmaku_count: self.live_danmaku.len(),
            danmaku_active: self.danmaku.active_count(),
            aspect: self.aspect,
            ontop: self.ontop,
            brightness: self.brightness,
            audio_only: self.audio_only,
            mini: self.mini,
            mini_hint: self
                .mini_hint_until
                .map(|until| Instant::now() < until)
                .unwrap_or(false),
            fullscreen: self.fullscreen,
            chrome_shown: self.chrome_shown,
            maximized: window::is_maximized(self.hwnd),
            pointer: self.pointer,
            sidebar_open: self.sidebar_open,
            sidebar_tab: self.sidebar_tab,
            sidebar_scroll: self.sidebar_scroll,
            danmaku_popup: self.danmaku_popup,
            danmaku_input: self.dm_input.clone(),
            danmaku_input_focus: self.dm_input_focus,
            dm_menu: self.dm_menu.clone(),
            undo: self.undo.as_ref().map(|_| UndoToastUi {
                text: format!("已屏蔽该用户 · {} 秒内可撤销", UNDO_MS / 1000),
            }),
            history_dates: self.history_dates.clone(),
            history_dates_loading: self.history_dates_loading,
            history_dates_error: self.history_dates_error.clone(),
            history_date: self.history_date.clone(),
            history_loading: self.history_loading,
            history_error: self.history_error.clone(),
            history_count: self.history_items.len(),
            info: self.info.clone(),
            pages: self.pages.clone(),
            current_page: self.current_page,
            comments: self.comments.clone(),
            comments_total: self.comments_total,
            comments_loading: self.comments_loading,
            comments_sort_mix: self.comments_sort_mix,
            comments_enter: self.comments_enter,
            comments_generation: self.comments_generation,
            liked: self.liked,
            coined: self.coined,
            favorited: self.favorited,
            toast: self.toast.clone(),
            quality_label: self.source.qn_label.clone(),
            player_quality_height: self.quality_height_pref,
            audio_label: self.source.audio_label.clone(),
            quality_rows: self.quality_rows(),
            audio_rows: self.audio_rows(),
            interp_rows: self.interp_rows(),
            enhance_rows: self.enhance_rows(),
            settings_menu: self.settings_menu,
            sleep_label: self.sleep_label(),
            keys_label: self.player_keys_hint.clone(),
            hover_preview: self.hover_preview,
            storyboard_cell,
            light_sidebar: self.light_sidebar,
            is_live: self.is_live,
            buffering,
            splash: self.loading || !self.had_first_frame,
            error: self.error.clone(),
            danmaku_opacity: self.danmaku_opacity,
            danmaku_font: self.danmaku_font,
            danmaku_speed: self.danmaku_speed,
            danmaku_area: self.danmaku_area,
            block_scroll: self.block_scroll,
            block_top: self.block_top,
            block_bottom: self.block_bottom,
            density_on: self.density_on,
            queue_len: self.queue_len,
            dropped: self.dropped,
            source_host: host_label(&self.source.video_url),
            audio_host: self
                .source
                .audio_url
                .as_deref()
                .map(host_label)
                .unwrap_or_default(),
            source_remote: self.source.remote,
            video_bitrate: self.source.video_bitrate,
            audio_bitrate: self.source.audio_bitrate,
            net_bps: self.net_bps,
            net_lead_s: self
                .cache_lead_ms()
                .map(|ms| ms as f64 / 1000.0)
                .unwrap_or(self.net_lead_s),
            next_preload_s: self.next_page_preload_s(),
            cache_complete: self.current_cache_complete(),
            download_streams: usize::from(self.decoder.is_some())
                + usize::from(self.audio.is_some())
                + usize::from(self.loop_prefetch.is_some()),
        };
        self.overlay
            .set_chrome_state(self.sidebar_open, self.fullscreen);
        let result = if sync {
            self.overlay.draw_sync(&model)
        } else {
            self.overlay.draw(&model)
        };
        result
    }

    // ------------------------------------------------------------------- QA

    fn qa_hover(&mut self, fraction: f32) {
        if self.duration_ms == 0 {
            return;
        }
        let layout = self.overlay.layout;
        let fraction = fraction.clamp(0.0, 1.0);
        let x = layout.track.left + layout.track.width() * fraction;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.pointer = Some((x, layout.track.top + 1.0));
        let time = (fraction as f64 * self.duration_ms as f64).round() as u64;
        self.hover_preview = Some(time);
        self.hover_pinned = true;
        self.ensure_storyboard();
        self.request_storyboard_cell(time);
        self.ui_dirty = true;
        log::line(&format!(
            "qa hover fraction={fraction:.2} time_ms={time} storyboard={}",
            u8::from(self.storyboard_meta.is_some())
        ));
    }

    /// Pick the first currently rendered comment and return a hit at its box
    /// centre (QA needs a deterministic comment without synthesising a mouse).
    fn qa_first_danmaku_hit(&self) -> Option<DanmakuHit> {
        let position = self.position_now();
        let fonts = self.danmaku.font_sizes();
        for render in self.danmaku.frame(position) {
            let font = fonts[render.bucket.min(2)];
            if let Some(hit) =
                self.danmaku
                    .hit_test(position, render.x + font * 0.5, render.y + font * 0.5)
            {
                return Some(hit);
            }
        }
        None
    }

    fn qa_open_dm_menu(&mut self) -> bool {
        match self.qa_first_danmaku_hit() {
            Some(hit) => {
                let (x, y) = hit.center();
                self.open_dm_menu(&hit, x, y);
                log::line(&format!(
                    "qa danmaku menu dmid={:?} mid_hash={:?}",
                    hit.dmid, hit.mid_hash
                ));
                true
            }
            None => {
                log::line("qa danmaku menu no visible comment");
                self.show_toast("当前画面没有可见弹幕", false);
                false
            }
        }
    }

    fn run_qa(&mut self) {
        let elapsed = self.started_at.elapsed();
        while self.qa_index < self.qa.len() && self.qa[self.qa_index].at <= elapsed {
            let index = self.qa_index;
            self.qa_index += 1;
            match self.qa[index].kind {
                QaKind::Seek => {
                    let target = if self.duration_ms > 60_000 {
                        self.duration_ms.min(30_000)
                    } else if self.duration_ms > 4_000 {
                        self.duration_ms / 2
                    } else {
                        1_000
                    };
                    log::line(&format!("qa seek target_ms={target}"));
                    self.seek(target);
                }
                QaKind::SeekEnd => {
                    let target = self.duration_ms.saturating_sub(700);
                    log::line(&format!("qa seek end target_ms={target}"));
                    self.seek(target);
                }
                QaKind::Pause => {
                    log::line("qa pause");
                    self.pause();
                }
                QaKind::Play => {
                    log::line("qa resume");
                    self.play();
                }
                QaKind::Interp => {
                    log::line("qa interp");
                    self.cycle_interp();
                }
                QaKind::Enhance => {
                    log::line("qa enhance");
                    self.select_enhance(EnhanceMode::Cas);
                }
                QaKind::Anime => {
                    log::line("qa anime");
                    self.select_enhance(EnhanceMode::Anime);
                }
                QaKind::Stats => {
                    // Readiness marker / capture trigger: do not steal a tab
                    // the QA sequence just opened (e.g. `info,stats`).
                    if !self.stats {
                        self.stats = true;
                        log::line("stats=1");
                    }
                }
                QaKind::Mute => self.toggle_mute(),
                QaKind::Volume => {
                    log::line("qa volume osd");
                    self.set_volume(62);
                    self.volume_wheel_until =
                        Some(Instant::now() + Duration::from_secs(6));
                    self.ui_dirty = true;
                }
                QaKind::Fullscreen => self.toggle_fullscreen(),
                QaKind::Danmaku => {
                    log::line("qa danmaku on");
                    self.set_danmaku_visible(true);
                }
                QaKind::DanmakuOff => {
                    log::line("qa danmaku off");
                    self.set_danmaku_visible(false);
                }
                QaKind::DanmakuToggle => {
                    log::line("qa danmaku toggle");
                    self.toggle_danmaku();
                }
                QaKind::Mini => {
                    log::line("qa mini");
                    self.toggle_mini();
                }
                QaKind::Aspect => {
                    log::line("qa aspect");
                    self.cycle_aspect();
                }
                QaKind::OnTop => {
                    log::line("qa ontop");
                    self.toggle_ontop();
                }
                QaKind::Bright => {
                    log::line("qa bright");
                    self.cycle_brightness();
                }
                QaKind::AudioOnly => {
                    log::line("qa audioonly");
                    self.toggle_audio_only();
                }
                QaKind::SpeedUp => {
                    log::line("qa speedup");
                    self.cycle_speed(1);
                }
                QaKind::SpeedDown => {
                    log::line("qa speeddown");
                    self.cycle_speed(-1);
                }
                QaKind::SpeedPanel => {
                    log::line("qa speedpanel");
                    self.open_speed();
                }
                QaKind::Sidebar => {
                    log::line("qa sidebar");
                    self.set_sidebar(!self.sidebar_open, self.sidebar_tab);
                }
                QaKind::SidebarInfo => {
                    log::line("qa sidebar info");
                    self.open_sidebar_tab(SidebarTab::Info);
                }
                QaKind::InfoCopy => {
                    log::line("qa info copy");
                    self.copy_video_id();
                }
                QaKind::SidebarEpisodes => {
                    log::line("qa sidebar episodes");
                    self.open_sidebar_tab(SidebarTab::Episodes);
                }
                QaKind::SidebarComments => {
                    log::line("qa sidebar comments");
                    self.open_sidebar_tab(SidebarTab::Comments);
                }
                QaKind::SidebarPerformance => {
                    log::line("qa sidebar performance");
                    self.open_sidebar_tab(SidebarTab::Performance);
                }
                QaKind::SidebarSettings => {
                    log::line("qa sidebar settings");
                    self.open_sidebar_tab(SidebarTab::Settings);
                }
                QaKind::Precision => {
                    log::line("qa precision toggle");
                    if self.precision_open {
                        self.close_precision();
                    } else {
                        self.open_precision();
                    }
                }
                QaKind::FrameStep => {
                    log::line("qa frame step");
                    self.cycle_frame_step();
                }
                QaKind::AbA => {
                    log::line("qa ab a");
                    self.set_ab_a();
                }
                QaKind::AbB => {
                    log::line("qa ab b");
                    self.set_ab_b();
                }
                QaKind::AbClear => {
                    log::line("qa ab clear");
                    self.clear_ab();
                }
                QaKind::LoopMenu => {
                    log::line("qa loop menu toggle");
                    self.loop_menu_open = !self.loop_menu_open;
                    self.danmaku_popup = false;
                    self.settings_menu = None;
                    self.chrome_shown = true;
                    self.last_mouse_activity = Instant::now();
                    self.ui_dirty = true;
                }
                QaKind::LoopPause => {
                    log::line("qa loop pause");
                    self.set_loop_mode(o11a1::settings::LOOP_PAUSE);
                }
                QaKind::LoopOne => {
                    log::line("qa loop one");
                    self.set_loop_mode(o11a1::settings::LOOP_ONE);
                }
                QaKind::LoopNext => {
                    log::line("qa loop next");
                    self.set_loop_mode(o11a1::settings::LOOP_NEXT);
                }
                QaKind::LoopPart => {
                    log::line("qa loop part");
                    self.set_loop_mode(o11a1::settings::LOOP_NEXT_PART);
                }
                QaKind::InterpMenu => {
                    log::line("qa interp menu");
                    self.open_settings_menu(SettingsMenu::Interp);
                }
                QaKind::EnhanceMenu => {
                    log::line("qa enhance menu");
                    self.open_settings_menu(SettingsMenu::Enhance);
                }
                QaKind::DanmakuPopup => {
                    log::line("qa danmaku popup");
                    if !self.danmaku_popup {
                        self.reload_danmaku_settings();
                    }
                    self.danmaku_popup = !self.danmaku_popup;
                    self.chrome_shown = true;
                    self.last_mouse_activity = Instant::now();
                    self.ui_dirty = true;
                    log::line(&format!("danmaku popup={}", u8::from(self.danmaku_popup)));
                }
                QaKind::DanmakuLive => {
                    // Exercise the same path the popup sliders use. The QA
                    // harness backs up and restores settings.json around the
                    // run, so writing through applies exactly like a click.
                    let before = self.danmaku.active_count();
                    self.danmaku_opacity = 70;
                    self.danmaku_font = 120;
                    self.danmaku_speed = 160;
                    self.danmaku_area = 75;
                    self.apply_danmaku_ui();
                    let after = self.danmaku.active_count();
                    log::line(&format!(
                        "qa danmaku live opacity={} font={} speed={} area={} before={before} after={after}",
                        self.danmaku_opacity, self.danmaku_font, self.danmaku_speed, self.danmaku_area
                    ));
                }
                QaKind::DmMenu => {
                    self.qa_open_dm_menu();
                }
                QaKind::DmHold => {
                    // Arm the exact Win32 long-press path without synthesising
                    // desktop mouse input; the 500 ms deadline fires in
                    // `poll_danmaku_long_press` like a real hold.
                    match self.qa_first_danmaku_hit() {
                        Some(hit) => {
                            let (x, y) = hit.center();
                            let mut press = LongPress::default();
                            press.down(Instant::now(), x, y);
                            self.dm_long = Some((press, hit));
                            log::line("qa danmaku hold armed");
                        }
                        None => {
                            log::line("qa danmaku hold no visible comment");
                            self.show_toast("当前画面没有可见弹幕", false);
                        }
                    }
                }
                QaKind::DmLike => {
                    if self.dm_menu.is_none() && !self.qa_open_dm_menu() {
                        continue;
                    }
                    log::line("qa danmaku like");
                    self.thumbup_danmaku();
                }
                QaKind::DmReport => {
                    if self.dm_menu.is_none() && !self.qa_open_dm_menu() {
                        continue;
                    }
                    if let Some(menu) = self.dm_menu.as_mut() {
                        menu.submenu = true;
                    }
                    self.ui_dirty = true;
                    log::line("qa danmaku report submenu");
                }
                QaKind::DmReportOne => {
                    if self.dm_menu.is_none() && !self.qa_open_dm_menu() {
                        continue;
                    }
                    log::line("qa danmaku report reason=1");
                    self.report_danmaku(1);
                }
                QaKind::DmBlock => match self.qa_first_danmaku_hit() {
                    Some(hit) => {
                        log::line("qa danmaku block");
                        self.block_danmaku_sender(&hit.mid_hash);
                    }
                    None => {
                        log::line("qa danmaku block no visible comment");
                        self.show_toast("当前画面没有可见弹幕", false);
                    }
                },
                QaKind::DmCopy => match self.qa_first_danmaku_hit() {
                    Some(hit) => {
                        log::line("qa danmaku copy");
                        self.copy_danmaku_text(&hit.text);
                    }
                    None => {
                        log::line("qa danmaku copy no visible comment");
                        self.show_toast("当前画面没有可见弹幕", false);
                    }
                },
                QaKind::DmUndo => {
                    log::line("qa danmaku undo");
                    self.undo_blocked_sender();
                }
                QaKind::DmHistory => {
                    log::line("qa danmaku history");
                    self.open_sidebar_tab(SidebarTab::History);
                    if self.history_dates.is_empty() && !self.history_dates_loading {
                        self.request_history_dates();
                    }
                }
                QaKind::DmHistoryDate => {
                    log::line("qa danmaku history date");
                    if !self.sidebar_open || self.sidebar_tab != SidebarTab::History {
                        self.open_sidebar_tab(SidebarTab::History);
                    }
                    if self.history_dates.is_empty() && !self.history_dates_loading {
                        self.request_history_dates();
                    }
                    self.pick_history_date(0);
                }
                QaKind::DmHistoryClose => {
                    log::line("qa danmaku history close");
                    self.set_sidebar(false, SidebarTab::History);
                }
                QaKind::Chrome => {
                    log::line("qa chrome");
                    self.chrome_shown = true;
                    self.last_mouse_activity = Instant::now();
                    self.ui_dirty = true;
                }
                QaKind::CommentsTime => {
                    log::line("qa comments time");
                    self.set_comments_sort(false);
                }
                QaKind::Like => self.info_action(0),
                QaKind::Coin => self.info_action(1),
                QaKind::Fav => self.info_action(2),
                QaKind::Page2 => {
                    log::line("qa page2");
                    self.switch_page(1);
                }
                QaKind::Quality => {
                    log::line("qa quality menu");
                    self.open_settings_menu(SettingsMenu::Quality);
                }
                QaKind::QualityLow => {
                    self.open_settings_menu(SettingsMenu::Quality);
                    let rows = self.quality_rows();
                    let mut candidates: Vec<(usize, i64)> = rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| !row.locked && !row.current)
                        .map(|(index, row)| (index, row.qn))
                        .collect();
                    candidates.sort_by_key(|(_, qn)| *qn);
                    if let Some((index, qn)) = candidates.first() {
                        log::line(&format!("qa quality low qn={qn} row={index}"));
                        self.select_settings_menu_row(*index);
                    } else {
                        self.settings_menu = None;
                        self.ui_dirty = true;
                        self.show_toast("没有其它可切换画质", false);
                    }
                }
                QaKind::QualityNext => {
                    self.open_settings_menu(SettingsMenu::Quality);
                    let rows = self.quality_rows();
                    let current = rows.iter().position(|row| row.current).unwrap_or(0);
                    let playable: Vec<usize> = rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| !row.locked)
                        .map(|(index, _)| index)
                        .collect();
                    if playable.len() > 1 {
                        let at = playable
                            .iter()
                            .position(|index| *index == current)
                            .unwrap_or(0);
                        let row = playable[(at + 1) % playable.len()];
                        log::line(&format!("qa quality next row={row}"));
                        self.select_settings_menu_row(row);
                    } else {
                        self.settings_menu = None;
                        self.ui_dirty = true;
                    }
                }
                QaKind::QualityLocked => {
                    self.open_settings_menu(SettingsMenu::Quality);
                    if let Some(index) = self.quality_rows().iter().position(|row| row.locked) {
                        log::line(&format!("qa quality locked row={index}"));
                        self.select_settings_menu_row(index);
                    } else {
                        self.settings_menu = None;
                        self.ui_dirty = true;
                        self.show_toast("当前视频没有大会员锁定画质", false);
                    }
                }
                QaKind::AudioMenu => {
                    log::line("qa audio menu");
                    self.open_settings_menu(SettingsMenu::Audio);
                }
                QaKind::AudioNext => {
                    self.open_settings_menu(SettingsMenu::Audio);
                    let rows = self.audio_rows();
                    if rows.len() > 1 {
                        let current = rows.iter().position(|row| row.current).unwrap_or(0);
                        let row = (current + 1) % rows.len();
                        log::line(&format!(
                            "qa audio next row={row} index={}",
                            rows[row].index
                        ));
                        self.select_settings_menu_row(row);
                    } else {
                        self.settings_menu = None;
                        self.ui_dirty = true;
                        self.show_toast("当前视频只有一条音轨", false);
                    }
                }
                QaKind::CommentsMore => {
                    log::line("qa comments more");
                    self.open_sidebar_tab(SidebarTab::Comments);
                    self.request_comments(false);
                }
                QaKind::Hover => self.qa_hover(0.35),
                QaKind::Hover2 => self.qa_hover(0.72),
                QaKind::Sleep => {
                    self.cycle_sleep();
                }
                QaKind::SleepShort => {
                    self.sleep_choice = 1;
                    self.sleep_deadline = Some(Instant::now() + Duration::from_secs(3));
                    self.ui_dirty = true;
                    log::line("sleep timer set seconds=3 qa=1");
                }
                QaKind::Fault => {
                    log::line("qa injected fault");
                    self.report_error("QA 注入：视频解码中断（用于验证重试按钮）");
                }
                QaKind::Retry => {
                    log::line("qa retry");
                    self.retry();
                }
                QaKind::Stall => {
                    self.stall_until = Some(Instant::now() + Duration::from_secs(5));
                    self.stalled = true;
                    self.ui_dirty = true;
                    log::line("qa stall overlay injected seconds=5");
                }
                QaKind::Shot => {
                    let path = shot_path();
                    std::thread::sleep(Duration::from_millis(150));
                    match window::capture(self.hwnd, &path) {
                        Ok(()) => log::line(&format!("qa shot path={}", path.display())),
                        Err(error) => log::line(&format!(
                            "qa shot failed path={} err={error:#}",
                            path.display()
                        )),
                    }
                }
            }
        }
    }

    fn shutdown(&mut self) {
        // Remove the window before any decoder/audio joins. The close click
        // must feel instant even when a session takes a few hundred ms to tear
        // down; cleanup continues out of sight.
        window::set_visible(self.hwnd, false);
        self.gate(false);
        self.sync_shared();
        self.record_watch_progress();
        self.persist_state();
        self.on_playback_end();
        if let Some(mut heartbeat) = self.heartbeat.take() {
            heartbeat.stop();
        }
        self.reset_storyboard();
        self.stop_sessions();
        self.composition.commit().ok();
        window::destroy(self.hwnd);
        unsafe {
            let _ = timeEndPeriod(1);
            CoUninitialize();
        }
        log::line("closed");
    }
}

/// Candidate index -> Win32 virtual-key code, matching the old settings UI's
/// 15-key pool (`0 M`, `1 N`, … `14 回车`).
fn player_candidate_vk(candidate: u32) -> Option<u32> {
    match candidate {
        0 => Some(0x4D),  // M
        1 => Some(0x4E),  // N
        2 => Some(0x42),  // B
        3 => Some(0x20),  // Space
        4 => Some(0x25),  // Left
        5 => Some(0x27),  // Right
        6 => Some(0x26),  // Up
        7 => Some(0x28),  // Down
        8 => Some(0x46),  // F
        9 => Some(0x44),  // D
        10 => Some(0x51), // Q
        11 => Some(0x53), // S
        12 => Some(0x1B), // Esc
        13 => Some(0x50), // P
        14 => Some(0x0D), // Enter
        _ => None,
    }
}

fn player_candidate_label(candidate: u32) -> &'static str {
    match candidate {
        0 => "M",
        1 => "N",
        2 => "B",
        3 => "空格",
        4 => "←",
        5 => "→",
        6 => "↑",
        7 => "↓",
        8 => "F",
        9 => "D",
        10 => "Q",
        11 => "S",
        12 => "Esc",
        13 => "P",
        14 => "回车",
        _ => "-",
    }
}

fn player_keys_summary(keys: &[u32]) -> String {
    let labels: Vec<&str> = keys
        .iter()
        .take(6)
        .map(|candidate| player_candidate_label(*candidate))
        .collect();
    let suffix = if keys.len() > 6 { "…" } else { "" };
    format!("{}{suffix}", labels.join("/"))
}

fn danmaku_band(
    layout: &crate::overlay::Layout,
    source: &MediaSource,
    mini: bool,
    aspect: AspectMode,
) -> Band {
    let (x, y, width, height) = layout.stage_video_bounds(source.width, source.height, aspect);
    let video_bottom = y + height;
    // Keep the first lane below the top controls: they always sit above the
    // comments, and hiding the first line under them made the top of the
    // video look empty while comments scrolled behind the chrome.
    let top = if mini {
        y
    } else {
        y.max(layout.top_bar.bottom + 2.0 * layout.scale)
    };
    let bottom_limit = if mini {
        video_bottom
    } else {
        (layout.panel.top - 8.0 * layout.scale).min(video_bottom)
    };
    Band {
        left: x,
        top,
        width,
        height: (video_bottom - top).max(1.0),
        bottom_limit: bottom_limit.max(top + 1.0),
        window_height: layout.height.round().max(1.0) as u32,
    }
}

/// Host part of a media URL, for the performance panel's source rows.
fn host_label(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or("").to_string()
}

/// Fold the loaded danmaku into a fixed 120-bucket density profile. The panel
/// only needs this when opened (and once per new comment list), so a single
/// pass per video is enough.
fn build_heatmap(items: &[DanmakuItem], duration_ms: u64) -> [u8; HEAT_BUCKETS] {
    let mut counts = [0u32; HEAT_BUCKETS];
    if duration_ms > 1 {
        for item in items {
            let index =
                ((item.progress_ms as u128 * HEAT_BUCKETS as u128) / duration_ms as u128) as usize;
            counts[index.min(HEAT_BUCKETS - 1)] =
                counts[index.min(HEAT_BUCKETS - 1)].saturating_add(1);
        }
    }
    let max = counts.iter().copied().max().unwrap_or(0);
    let mut out = [0u8; HEAT_BUCKETS];
    if max > 0 {
        for (slot, count) in out.iter_mut().zip(counts.iter()) {
            *slot = ((*count as f32 / max as f32).sqrt() * 255.0).round() as u8;
        }
    }
    out
}

fn window_size(state: &PlayerState) -> (u32, u32) {
    let text = std::env::var("BILI_PLAYER_SIZE").unwrap_or_default();
    if let Some((width, height)) = text.split_once('x') {
        if let (Ok(width), Ok(height)) = (width.trim().parse(), height.trim().parse()) {
            return (width, height);
        }
    }
    if let (Some(width), Some(height)) = (state.window_width, state.window_height) {
        let scale = window::system_dpi_scale();
        return (
            (width as f32 * scale).round().max(320.0) as u32,
            (height as f32 * scale).round().max(180.0) as u32,
        );
    }
    DEFAULT_WINDOW
}

fn shot_path() -> PathBuf {
    if let Ok(path) = std::env::var("BILI_PLAYER_SHOT") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    let mut path = std::env::temp_dir();
    path.push("opencode");
    path.push("native_player_qa.png");
    path
}

/// The S key writes into the app's screenshots folder in local time, e.g.
/// `<root>/screenshots/O11A1_20261110_143005.png`.
fn screenshot_path() -> PathBuf {
    let mut directory = o11a1::paths::root();
    directory.push("screenshots");
    if let Err(error) = std::fs::create_dir_all(&directory) {
        log::line(&format!(
            "screenshot mkdir failed path={} err={error:#}",
            directory.display()
        ));
    }
    let now = unsafe { GetLocalTime() };
    directory.push(format!(
        "O11A1_{:04}{:02}{:02}_{:02}{:02}{:02}.png",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    ));
    directory
}

fn comment_ui(
    data: CommentData,
    avatars: &std::collections::HashMap<String, ImageData>,
) -> CommentUi {
    let avatar = avatars.get(&data.avatar_url).cloned();
    let replies = data
        .replies
        .into_iter()
        .map(|reply| reply_ui(reply, avatars))
        .collect();
    CommentUi {
        rpid: data.rpid,
        user: data.user,
        message: data.message,
        likes: data.likes,
        ctime: data.ctime,
        pinned: data.pinned,
        reply_count: data.reply_count,
        avatar_url: data.avatar_url,
        avatar,
        replies,
        replies_page: 0,
        replies_end: false,
        replies_loading: false,
        expanded: false,
    }
}

fn reply_ui(data: CommentData, avatars: &std::collections::HashMap<String, ImageData>) -> ReplyUi {
    ReplyUi {
        rpid: data.rpid,
        user: data.user,
        message: data.message,
        likes: data.likes,
        ctime: data.ctime,
        avatar_url: data.avatar_url.clone(),
        avatar: avatars.get(&data.avatar_url).cloned(),
    }
}

fn parse_qa(_duration_ms: u64) -> Vec<QaAction> {
    let Ok(text) = std::env::var("BILI_PLAYER_QA") else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    let mut at = 3.5f64;
    for token in text
        .split(',')
        .map(|token| token.trim().to_ascii_lowercase())
    {
        let kind = match token.as_str() {
            "" => continue,
            "seek" => QaKind::Seek,
            "seek_end" | "seekend" | "end" => QaKind::SeekEnd,
            "pause" => QaKind::Pause,
            "resume" | "play" => QaKind::Play,
            "interp" => QaKind::Interp,
            "enhance" | "cas" => QaKind::Enhance,
            "anime" | "enhance2" => QaKind::Anime,
            "shot" | "screenshot" => QaKind::Shot,
            "stats" => QaKind::Stats,
            "mute" => QaKind::Mute,
            "volume" | "vol" => QaKind::Volume,
            "fullscreen" | "fs" => QaKind::Fullscreen,
            "danmaku" | "dm" => QaKind::Danmaku,
            "danmakuoff" | "dmoff" => QaKind::DanmakuOff,
            "danmakutoggle" | "dmtoggle" => QaKind::DanmakuToggle,
            "mini" | "minitoggle" => QaKind::Mini,
            "aspect" | "ratio" => QaKind::Aspect,
            "ontop" | "topmost" => QaKind::OnTop,
            "bright" | "brightness" => QaKind::Bright,
            "audioonly" | "audio" => QaKind::AudioOnly,
            "speedup" | "faster" => QaKind::SpeedUp,
            "speeddown" | "slower" => QaKind::SpeedDown,
            "speedpanel" | "speed_panel" | "speed" => QaKind::SpeedPanel,
            "sidebar" => QaKind::Sidebar,
            "info" => QaKind::SidebarInfo,
            "info_copy" | "infocopy" | "id_copy" | "idcopy" => QaKind::InfoCopy,
            "episodes" | "parts" => QaKind::SidebarEpisodes,
            "comments" | "comment" => QaKind::SidebarComments,
            "performance" | "perf" => QaKind::SidebarPerformance,
            "settings" | "set" => QaKind::SidebarSettings,
            "dmpopup" | "danmakupopup" => QaKind::DanmakuPopup,
            "dmlive" | "danmakulive" => QaKind::DanmakuLive,
            "dm_menu" | "dmmenu" | "danmakumenu" => QaKind::DmMenu,
            "dm_hold" | "dmhold" => QaKind::DmHold,
            "dm_like" | "dmlike" => QaKind::DmLike,
            "dm_report" | "dmreport" => QaKind::DmReport,
            "dm_report_one" | "dmreportone" | "dm_report1" => QaKind::DmReportOne,
            "dm_block" | "dmblock" => QaKind::DmBlock,
            "dm_copy" | "dmcopy" => QaKind::DmCopy,
            "dm_undo" | "dmundo" => QaKind::DmUndo,
            "dm_history" | "dmhistory" => QaKind::DmHistory,
            "dm_history_date" | "dmhistorydate" => QaKind::DmHistoryDate,
            "dm_history_close" | "dmhistoryclose" => QaKind::DmHistoryClose,
            "chrome" => QaKind::Chrome,
            "comments_time" | "commentstime" => QaKind::CommentsTime,
            "like" => QaKind::Like,
            "coin" => QaKind::Coin,
            "fav" | "favorite" => QaKind::Fav,
            "page2" => QaKind::Page2,
            "quality" => QaKind::Quality,
            "quality_low" | "qualitylow" => QaKind::QualityLow,
            "quality_next" | "qualitynext" => QaKind::QualityNext,
            "quality_locked" | "qualitylocked" => QaKind::QualityLocked,
            "audio_menu" | "audiomenu" => QaKind::AudioMenu,
            "audio_next" | "audionext" => QaKind::AudioNext,
            "comments_more" | "commentsmore" => QaKind::CommentsMore,
            "hover" => QaKind::Hover,
            "hover2" => QaKind::Hover2,
            "sleep" => QaKind::Sleep,
            "sleep_short" | "sleepshort" => QaKind::SleepShort,
            "precision" | "seek_precision" => QaKind::Precision,
            "frame_step" | "framestep" | "step" => QaKind::FrameStep,
            "ab_a" | "aba" => QaKind::AbA,
            "ab_b" | "abb" => QaKind::AbB,
            "ab_clear" | "abclear" => QaKind::AbClear,
            "loop_menu" | "loopmenu" => QaKind::LoopMenu,
            "loop_pause" | "looppause" => QaKind::LoopPause,
            "loop_one" | "loopone" => QaKind::LoopOne,
            "loop_next" | "loopnext" => QaKind::LoopNext,
            "loop_part" | "looppart" => QaKind::LoopPart,
            "interp_menu" | "interpmenu" => QaKind::InterpMenu,
            "enhance_menu" | "enhancemenu" => QaKind::EnhanceMenu,
            "fault" => QaKind::Fault,
            "retry" => QaKind::Retry,
            "stall" => QaKind::Stall,
            other => {
                log::line(&format!("qa ignoring unknown action={other}"));
                continue;
            }
        };
        actions.push(QaAction {
            at: Duration::from_secs_f64(at),
            kind,
        });
        at += 1.8;
    }
    if !actions.is_empty() {
        log::line(&format!(
            "qa actions={} start_at=3.5s step=1.8s",
            actions.len()
        ));
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_or_small_picture_lag_is_not_a_catch_up() {
        // Normal decode jitter: the picture is only a frame or two late.
        assert!(!picture_lag_requires_catch_up(
            120,
            true,
            Duration::from_secs(3)
        ));
        // A real stall with no frames arriving is the buffering path's job,
        // not a reason to seek.
        assert!(!picture_lag_requires_catch_up(
            400,
            false,
            Duration::from_secs(3)
        ));
        // One short GPU/engine hitch clears before the hold expires.
        assert!(!picture_lag_requires_catch_up(
            400,
            true,
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn sustained_picture_lag_triggers_a_catch_up() {
        // The decoder keeps producing frames but its media clock cannot keep
        // up with the sound: this is the slow-motion case and must skip the
        // undecodable gap.
        assert!(picture_lag_requires_catch_up(
            400,
            true,
            Duration::from_millis(AV_VIDEO_LAG_HOLD_MS)
        ));
        assert!(picture_lag_requires_catch_up(
            AV_VIDEO_LAG_MS + 1,
            true,
            Duration::from_millis(AV_VIDEO_LAG_HOLD_MS)
        ));
    }

    #[test]
    fn buffer_guard_needs_a_real_dry_ring() {
        assert!(!buffer_is_low(0, 150));
        assert!(buffer_is_low(0, 200));
        assert!(!buffer_is_low(2, 399));
        assert!(buffer_is_low(2, 400));
        assert!(!buffer_is_low(3, 5_000));
    }

    #[test]
    fn wheel_volume_moves_five_percent_per_notch() {
        let mut accum = 0.0;
        let mut direction = 0.0;
        assert_eq!(wheel_volume_step(&mut accum, &mut direction, 120), 5);
        assert_eq!(wheel_volume_step(&mut accum, &mut direction, 120), 5);
        assert_eq!(wheel_volume_step(&mut accum, &mut direction, -120), -5);
        assert_eq!(wheel_volume_step(&mut accum, &mut direction, 0), 0);
    }

    #[test]
    fn wheel_volume_accumulates_a_precision_touchpad() {
        let mut accum = 0.0;
        let mut direction = 0.0;
        let mut total = 0;
        for _ in 0..8 {
            total += wheel_volume_step(&mut accum, &mut direction, 15);
        }
        // Eight 15-unit events are one 120-unit notch: 5%.
        assert_eq!(total, 5);
        // The leftovers are below one percent and do not get lost.
        assert!(accum.abs() < 1.0);
        // Reversing direction responds on the first notch instead of eating
        // the leftover of the previous direction.
        assert_eq!(wheel_volume_step(&mut accum, &mut direction, -120), -5);
    }

    #[test]
    fn next_page_prefetch_starts_in_the_last_ten_seconds() {
        let duration = 399_000;
        assert!(next_page_prefetch_due(
            o11a1::settings::LOOP_NEXT_PART,
            duration - 10_000,
            duration,
            true,
            false
        ));
        assert!(!next_page_prefetch_due(
            o11a1::settings::LOOP_NEXT_PART,
            duration - 10_001,
            duration,
            true,
            false
        ));
        assert!(next_page_prefetch_due(
            o11a1::settings::LOOP_NEXT_PART,
            duration - 500,
            duration,
            true,
            false
        ));
        assert!(!next_page_prefetch_due(
            o11a1::settings::LOOP_NEXT_PART,
            duration - 500,
            duration,
            false,
            false
        ));
        assert!(!next_page_prefetch_due(
            o11a1::settings::LOOP_NEXT_PART,
            duration - 500,
            duration,
            true,
            true
        ));
        assert!(!next_page_prefetch_due(
            o11a1::settings::LOOP_ONE,
            duration - 500,
            duration,
            true,
            false
        ));
        assert!(!next_page_prefetch_due(
            o11a1::settings::LOOP_NEXT_PART,
            0,
            0,
            true,
            false
        ));
    }

    #[test]
    fn frame_step_chip_cycles_without_a_menu() {
        assert_eq!(next_frame_step(1), 2);
        assert_eq!(next_frame_step(2), 5);
        assert_eq!(next_frame_step(5), 10);
        assert_eq!(next_frame_step(10), 1);
        assert_eq!(next_frame_step(0), 1);
    }

    #[test]
    fn frame_ruler_drag_steps_only_after_a_full_notch() {
        // A small wobble must not step a frame.
        assert_eq!(tuner_steps(-11.0, 12.0), 0);
        assert_eq!(tuner_steps(11.0, 12.0), 0);
        // Dragging left advances; dragging right rewinds.
        assert_eq!(tuner_steps(-12.0, 12.0), 1);
        assert_eq!(tuner_steps(-50.0, 12.0), 4);
        assert_eq!(tuner_steps(50.0, 12.0), -4);
    }

    #[test]
    fn player_key_pool_matches_the_old_settings_ui() {
        assert_eq!(player_candidate_vk(0), Some(0x4D)); // M
        assert_eq!(player_candidate_vk(3), Some(0x20)); // Space
        assert_eq!(player_candidate_vk(4), Some(0x25)); // Left
        assert_eq!(player_candidate_vk(12), Some(0x1B)); // Esc
        assert_eq!(player_candidate_vk(14), Some(0x0D)); // Enter
        assert_eq!(player_candidate_vk(15), None);
        assert_eq!(player_candidate_label(3), "空格");
    }

    #[test]
    fn remapped_keys_resolve_by_action_index() {
        let keys = vec![8, 0, 5, 6, 7, 1, 3, 12, 9, 10, 11];
        let action = |key: u32| {
            keys.iter()
                .position(|c| player_candidate_vk(*c) == Some(key))
        };
        assert_eq!(action(0x46), Some(0)); // F is now play/pause
        assert_eq!(action(0x20), Some(6)); // Space is now fullscreen
        assert_eq!(action(0x51), Some(9)); // Q stays like
        assert_eq!(action(0x7A), None); // unmapped F11 is not an action
    }

    #[test]
    fn keys_summary_is_short_and_covers_the_defaults() {
        let summary = player_keys_summary(&o11a1::settings::default_player_keys());
        assert!(summary.contains("空格"));
        assert!(summary.contains("←"));
        assert!(summary.ends_with('…'));
    }
}
