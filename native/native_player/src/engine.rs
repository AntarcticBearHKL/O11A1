//! The player engine: window thread, decode sessions, pacer, audio clock,
//! overlay state and every command the public [`crate::Player`] can send.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
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
use crate::convert;
use crate::danmaku::{
    convert_item, Band, DanmakuHit, DanmakuItem, DanmakuModel, DanmakuOverlay, DanmakuSettings,
    LongPress,
};
use crate::decode::{DecodedFrame, VideoDecoder, VideoSpec};
use crate::detail::{DetailEvent, DetailRequest};
use crate::enhance::{EnhanceMode, EnhancePipeline};
use crate::format::{clamp_seek, format_time};
use crate::gpu::{AspectMode, Composition, Gpu, VideoSurface};
use crate::heartbeat::{Beat, Heartbeat};
use crate::interp::{self, InterpMode};
use crate::log;
use crate::overlay::{
    dm_menu_rect, dm_menu_row, dm_report_rect, dm_report_row, AudioRowUi, CommentUi,
    DanmakuMenuUi, ImageData, InfoUi, Overlay, PageUi, QualityRowUi, Rect, SettingsMenu,
    SidebarTab, StoryboardCellUi, ToastUi, UiModel, UndoToastUi, DM_MENU_BLOCK, DM_MENU_COPY,
    DM_MENU_LIKE, DM_MENU_REPORT, DM_MENU_LABELS, DM_REPORT_LABELS, SETTINGS_ROWS, TB_ASPECT,
    TB_DANMAKU, TB_DANMAKU_SETTINGS, TB_FULLSCREEN, TB_MINI, TB_PLAYLIST, TB_SETTINGS, TB_SHOT,
    TB_SIDEBAR, TB_SPEED, TB_VOLUME,
};
use crate::source::{MediaSource, TrackRequest};
use crate::state::{PlayerState, SaveDebouncer, SAVE_DEBOUNCE};
use crate::storyboard::{self, cell_index_for};
use crate::window::{self, InputEvent};
use crate::PlayerConfig;

const DEFAULT_WINDOW: (u32, u32) = (1000, 600);
const START_AUDIO_WAIT_MS: u64 = 1_200;
const AV_DRIFT_MS: u64 = 200;
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
    SetVolume(u8),
    SetSpeed(f64),
    SetInterp(InterpMode),
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
    SetRect { x: i32, y: i32, width: u32, height: u32 },
    /// Show/hide the Electron-hosted popup.
    SetVisible(bool),
    /// Hovered seek time (ms) for the native storyboard thumbnail; `None`
    /// clears the preview.
    Preview(Option<u64>),
    /// Show a locally sent danmaku immediately.
    SendDanmaku { text: String, mode: u32 },
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
    /// Hosted-window input counters. Electron polls these and turns them into
    /// play/pause and fullscreen actions; the native overlay is not drawn.
    pub click_seq: AtomicU64,
    pub double_click_seq: AtomicU64,
    /// Coalesced mouse-move counter used to reveal fullscreen controls.
    pub move_seq: AtomicU64,
    /// Right-clicked danmaku under the pointer, consumed by the Electron UI.
    pub danmaku_hits: std::sync::Mutex<std::collections::VecDeque<DanmakuHit>>,
    /// 500 ms left-press on a danmaku, consumed by the Electron UI.
    pub long_press_hits: std::sync::Mutex<std::collections::VecDeque<DanmakuHit>>,
    pub long_press_seq: AtomicU64,
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
        self.danmaku_total
            .store(engine.danmaku.total().min(u32::MAX as usize) as u32, Ordering::SeqCst);
        self.mini.store(engine.mini, Ordering::SeqCst);
        self.aspect.store(engine.aspect.as_u8(), Ordering::SeqCst);
        self.topmost
            .store(window::is_topmost(engine.hwnd), Ordering::SeqCst);
        self.brightness_milli
            .store((engine.brightness * 1000.0).round() as u32, Ordering::SeqCst);
        self.audio_only
            .store(engine.audio_only, Ordering::SeqCst);
        self.ended.store(engine.ended, Ordering::SeqCst);
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

struct Engine {
    source: MediaSource,
    /// The open request, kept so a failed initial resolve can be retried.
    open_config: PlayerConfig,
    /// `true` while the window shows the loading card and no source exists.
    loading: bool,
    /// A resolve worker is in flight and its result is expected on `source_rx`.
    source_pending: bool,
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
    last_danmaku_draw: Instant,
    /// The unfiltered live fetch, kept so the 历史 view can be closed (or a
    /// blocked sender undone) without another network round-trip.
    live_danmaku: Vec<DanmakuItem>,
    /// Right-click comment menu and its hover/like/report/block state.
    dm_menu: Option<DanmakuMenuUi>,
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
    enhance_pipeline: Option<EnhancePipeline>,
    decoder: Option<VideoDecoder>,
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
    generation: u64,
    session_started: Instant,
    audio_master: bool,
    last_drift_log: Option<Instant>,
    last_stall_log: Option<Instant>,
    ended: bool,
    last_frame_ts: u64,

    // Audio/UI settings.
    volume: u8,
    muted: bool,
    /// 单曲循环: restart in the same window when the stream ends.
    loop_one: bool,
    speed: f32,
    interp: InterpMode,
    enhance: EnhanceMode,
    interp_out_hz: Option<u32>,
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

    // Display state.
    cur: Option<Vec<u8>>,
    prev: Option<Vec<u8>>,
    blend_buffer: Vec<u8>,
    last_source_present: Option<Instant>,
    blend_at: Option<Instant>,
    out_period: Duration,

    // Counters.
    presents_window: u64,
    fps: f32,
    fps_window_start: Instant,
    generated: u64,
    blend_us_total: u64,
    blend_count: u64,
    last_blend_us: u32,

    // UI.
    ui_dirty: bool,
    last_ui_draw: Instant,
    status: String,
    drag_preview: Option<u64>,
    dragging: bool,
    /// Left press on the picture: a small move drags the frameless window,
    /// a clean release keeps the old play/pause click.
    window_drag_arm: Option<(i32, i32)>,
    popup_drag: Option<u8>,
    mini: bool,
    mini_saved: Option<MiniSaved>,
    mini_hint_until: Option<Instant>,
    quit: bool,

    // Custom chrome / right strip.
    chrome_shown: bool,
    last_mouse_activity: Instant,
    pointer: Option<(f32, f32)>,
    sidebar_open: bool,
    sidebar_tab: SidebarTab,
    sidebar_scroll: f32,
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
    detail_tx: Sender<DetailRequest>,
    detail_rx: Receiver<DetailEvent>,
    avatars: std::collections::HashMap<String, ImageData>,
    switch_rx: Option<Receiver<Result<MediaSource, String>>>,
    switch_target: Option<usize>,
    track_rx: Option<Receiver<Result<MediaSource, String>>>,
    track_kind: Option<&'static str>,
    settings_menu: Option<SettingsMenu>,
    liked: bool,
    coined: bool,
    favorited: bool,
    toast: Option<ToastUi>,
    toast_until: Option<Instant>,
    ui_ms: f32,
    queue_len: usize,
    dropped: u64,
    audio_ms: u64,
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
    Pause,
    Play,
    Interp,
    Enhance,
    Anime,
    Shot,
    Stats,
    Mute,
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
    Sidebar,
    SidebarInfo,
    SidebarEpisodes,
    SidebarComments,
    SidebarPerformance,
    SidebarSettings,
    DanmakuPopup,
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
        let (hwnd, events) = match &host {
            Some(host) => window::create_hosted(
                HWND(host.parent as *mut _),
                host.x,
                host.y,
                host.width,
                host.height,
            )?,
            None => {
                let (client_w, client_h) = window_size();
                window::create(&title, client_w, client_h)?
            }
        };
        // Apply 置顶 before the composition tree is built; the state is also
        // read back so the UI reflects the measured window style, not a wish.
        if state.topmost {
            if let Err(error) = window::set_topmost(hwnd, true) {
                log::line(&format!("player state topmost failed: {error:#}"));
            }
        }
        let initial_aspect = AspectMode::from_u8(state.aspect);
        let client = window::client_size(hwnd);
        let scale = window::dpi_scale(hwnd);
        let gpu = Gpu::new().context("create D3D11 device")?;
        let video = VideoSurface::new(&gpu, source.width, source.height)
            .context("create video swapchain")?;
        let mut overlay = Overlay::new(&gpu, client.0, client.1, scale)
            .context("create overlay swapchain")?;
        if external {
            overlay.clear().context("clear hosted overlay")?;
        }
        let danmaku_settings = DanmakuSettings::load(client.1);
        let danmaku_fonts = danmaku_settings.font_sizes;
        let danmaku_opacity = danmaku_settings.opacity;
        let mut danmaku_layer = DanmakuOverlay::new(
            &gpu,
            client.0,
            client.1,
            danmaku_fonts,
            danmaku_opacity,
        )
        .context("create danmaku swapchain")?;
        let composition = Composition::new(hwnd, &gpu, &video.swap, &danmaku_layer.swap, &overlay.swap)
            .context("create DirectComposition tree")?;
        composition.set_root_clip(client.0, client.1)?;
        let (x, y, sx, sy) = crate::gpu::video_transform(
            client.0 as f32,
            client.1 as f32,
            source.width,
            source.height,
            initial_aspect,
        );
        composition.set_video_transform(x, y, sx, sy)?;
        let band = danmaku_band(client, &overlay.layout, &source, false, initial_aspect);
        let danmaku = DanmakuModel::new(danmaku_settings, band);
        danmaku_layer.clear().context("clear danmaku layer")?;
        let danmaku_env = std::env::var("BILI_PLAYER_DANMAKU")
            .ok()
            .map(|value| {
                !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false" | "no"
                )
            });
        let danmaku_visible = danmaku_env.unwrap_or(state.danmaku_visible);

        let enhance_pipeline = match EnhancePipeline::new(&gpu.device, &gpu.context, source.width, source.height) {
            Ok(pipeline) => Some(pipeline),
            Err(error) => {
                log::line(&format!(
                    "enhance fallback at startup reason={error:#}"
                ));
                None
            }
        };

        let duration_ms = source.duration_ms;
        let start_ms = clamp_seek(requested_start_ms, duration_ms);
        let display_hz = interp::display_refresh_hz();
        let interp_out_hz = interp::interp_plan(display_hz, source.fps);
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
        let speed = if state.speed.is_finite() {
            state.speed.clamp(0.5, 2.0)
        } else {
            1.0
        };
        state.volume = volume;
        state.muted = muted;
        state.speed = speed;
        state.aspect = initial_aspect.as_u8();
        let stored_settings = o11a1::settings::load();
        let source_title = source.title.clone();
        let light_sidebar = !o11a1::theme::resolve(stored_settings.theme);
        let player_keys = if stored_settings.player_keys.len() == o11a1::settings::PLAYER_KEY_ACTIONS {
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
            (Some(session), Some(bvid), Some(cid)) => Some(Heartbeat::spawn(
                session.clone(),
                bvid.clone(),
                cid,
            )),
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
            source_rx,
            source_tx,
            requested_start_ms,
            session,
            shared,
            commands,
            hwnd,
            events,
            external,
            external_preview_visible: false,
            external_press: None,
            gpu,
            video,
            overlay,
            composition,
            danmaku_layer,
            danmaku,
            danmaku_rx,
            danmaku_tx,
            danmaku_visible,
            danmaku_band: band,
            last_danmaku_draw: Instant::now(),
            live_danmaku: Vec::new(),
            dm_menu: None,
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
            enhance_pipeline,
            decoder: None,
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
            generation: 1,
            session_started: Instant::now(),
            audio_master: false,
            last_drift_log: None,
            last_stall_log: None,
            ended: false,
            last_frame_ts: start_ms,
            volume,
            muted,
            loop_one: stored_settings.loop_one,
            speed,
            interp: InterpMode::Off,
            enhance: EnhanceMode::Off,
            interp_out_hz,
            stats: false,
            aspect: initial_aspect,
            ontop: window::is_topmost(hwnd),
            brightness: initial_brightness,
            audio_only: initial_audio_only,
            light_sidebar,
            player_keys,
            player_keys_hint,
            sleep_deadline: None,
            sleep_choice: 0,
            cur: None,
            prev: None,
            blend_buffer: Vec::new(),
            last_source_present: None,
            blend_at: None,
            out_period: Duration::from_secs_f64(1.0 / out_hz.max(1) as f64),
            presents_window: 0,
            fps: 0.0,
            fps_window_start: Instant::now(),
            generated: 0,
            blend_us_total: 0,
            blend_count: 0,
            last_blend_us: 0,
            ui_dirty: true,
            last_ui_draw: Instant::now(),
            status: if loading {
                "正在打开视频…".to_string()
            } else {
                "正在加载…".to_string()
            },
            drag_preview: None,
            dragging: false,
            window_drag_arm: None,
            popup_drag: None,
            mini: false,
            mini_saved: None,
            mini_hint_until: None,
            quit: false,
            chrome_shown: true,
            last_mouse_activity: Instant::now(),
            pointer: None,
            sidebar_open: false,
            sidebar_tab: SidebarTab::Info,
            sidebar_scroll: 0.0,
            danmaku_popup: false,
            dm_input: String::new(),
            dm_input_focus: false,
            dm_send_rx: None,
            info: Some(InfoUi {
                title: source_title.clone(),
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
            detail_tx,
            detail_rx,
            avatars: std::collections::HashMap::new(),
            switch_rx: None,
            switch_target: None,
            track_rx: None,
            track_kind: None,
            settings_menu: None,
            liked: false,
            coined: false,
            favorited: false,
            toast: None,
            toast_until: None,
            ui_ms: 0.0,
            queue_len: 0,
            dropped: 0,
            audio_ms: 0,
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
            error: None,
            danmaku_opacity: stored_settings.danmaku_opacity.min(100),
            danmaku_font: stored_settings.danmaku_font.min(2),
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
            "danmaku cadence hz={} independent=1",
            crate::interp::display_refresh_hz().clamp(30, 120)
        ));
        if loading {
            log::line("player window ready; waiting for the resolved source");
        } else {
            engine.open_sessions(start_ms, false)?;
        }
        let start_mini = std::env::var("BILI_PLAYER_MINI")
            .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
            .unwrap_or(false);
        if start_mini {
            engine.set_mini(true);
        }
        engine.draw_ui()?;
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
            speed: self.speed as f64,
        }
    }

    fn open_sessions(&mut self, start_ms: u64, gate_open: bool) -> Result<()> {
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        let decoder = VideoDecoder::spawn(&self.video_spec(), start_ms, generation, gate_open)
            .with_context(|| format!("open video decoder at {start_ms} ms"))?;
        self.decoder = Some(decoder);

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
        );
        self.audio = Some(audio);
        // Every session starts paused until the picture's first frame is ready
        // and the seek/start state machine resumes both together. Without this
        // a slow HEVC seek let the sound run ~2 s ahead of the picture.
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::Pause);
        }
        self.session_started = Instant::now();
        self.shared.position_ms.store(start_ms, Ordering::SeqCst);
        self.position_ms = start_ms;
        Ok(())
    }

    fn stop_sessions(&mut self) {
        if let Some(mut decoder) = self.decoder.take() {
            decoder.stop();
        }
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
    }

    fn restart_at(&mut self, target_ms: u64) -> Result<()> {
        let target = clamp_seek(target_ms, self.duration_ms);
        let was_playing = self.requested_play;
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        self.danmaku.reseed(target);
        if let Err(error) = self.danmaku_layer.clear() {
            log::line(&format!("danmaku clear on seek failed: {error:#}"));
        }
        log::line(&format!(
            "danmaku seek reset target_ms={target} gen={generation} active=0"
        ));
        if let Some(mut decoder) = self.decoder.take() {
            decoder.stop();
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
        let decoder = VideoDecoder::spawn(&self.video_spec(), target, generation, false)
            .with_context(|| format!("restart video decoder at {target} ms"))?;
        self.decoder = Some(decoder);
        self.session_started = Instant::now();
        self.seek_in_flight = true;
        self.starting = false;
        self.ended = false;
        self.playing = false;
        self.seek_target = target;
        self.position_ms = target;
        self.base_ms = target;
        self.anchor = None;
        self.paused_at = target;
        self.audio_master = false;
        self.first_frame_at = None;
        self.blend_at = None;
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

    fn position_now(&self) -> u64 {
        if self.seek_in_flight {
            return self.seek_target;
        }
        if !self.playing {
            return self.paused_at;
        }
        if self.audio_master {
            if let Some(audio) = &self.audio {
                let audio_ms = audio.clock.position_ms.load(Ordering::SeqCst);
                if audio_ms > 0 || self.base_ms == 0 {
                    return self.clamp_position(audio_ms);
                }
            }
        }
        self.wall_position()
    }

    fn pause(&mut self) {
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
        self.position_ms = self.paused_at;
        self.playing = false;
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
        self.on_playback_started();
        self.gate(true);
        if let Some(audio) = &self.audio {
            audio.send(AudioCommand::Resume);
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
            if let Err(error) = self.danmaku_layer.clear() {
                log::line(&format!("danmaku clear failed: {error:#}"));
            }
        }
        self.shared
            .danmaku_visible
            .store(visible, Ordering::SeqCst);
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

    fn persist_state(&mut self) {
        self.state.volume = self.volume;
        self.state.muted = self.muted;
        self.state.speed = self.speed;
        self.state.aspect = self.aspect.as_u8();
        self.state.brightness = self.brightness;
        self.state.audio_only = self.audio_only;
        self.state.topmost = self.ontop;
        self.state.danmaku_visible = self.danmaku_visible;
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
                            log::line(&format!(
                                "danmaku menu report submenu dmid={}",
                                menu.dmid
                            ));
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
        log::line(&format!("danmaku copy content len={}", text.chars().count()));
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
        let dates = self.history_dates_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("历史弹幕日期线程已结束".to_string()))
            }
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
        let font_scale = match self.danmaku_font {
            0 => 0.85,
            2 => 1.2,
            _ => 1.0,
        };
        self.danmaku.reconfigure(
            self.danmaku_opacity as f32 / 100.0,
            font_scale,
            self.danmaku_speed,
            self.danmaku_area,
            self.block_scroll,
            self.block_top,
            self.block_bottom,
        );
        if let Err(error) = self.danmaku_layer.set_opacity(self.danmaku_opacity as f32 / 100.0) {
            log::line(&format!("danmaku opacity failed: {error:#}"));
        }
        if let Err(error) = self.danmaku_layer.set_fonts(self.danmaku.font_sizes()) {
            log::line(&format!("danmaku fonts failed: {error:#}"));
        }
        if let Err(error) = self.danmaku_layer.clear() {
            log::line(&format!("danmaku settings clear failed: {error:#}"));
        }
        let mut stored = o11a1::settings::load();
        stored.danmaku_opacity = self.danmaku_opacity;
        stored.danmaku_font = self.danmaku_font;
        stored.danmaku_speed = self.danmaku_speed;
        stored.danmaku_area = self.danmaku_area;
        stored.block_scroll = self.block_scroll;
        stored.block_top = self.block_top;
        stored.block_bottom = self.block_bottom;
        if let Err(error) = o11a1::settings::save(&stored) {
            log::line(&format!("danmaku settings save failed: {error:#}"));
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

    fn set_danmaku_font(&mut self, index: usize) {
        self.danmaku_font = index.min(2) as u32;
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
            return;
        }
        if reset {
            self.comments.clear();
            self.comments_cursor.clear();
            self.comments_end = false;
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
            cursor: self.comments_cursor.clone(),
        });
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
                    log::line(&format!("heartbeat aid={} bvid={}", self.aid, self.source.bvid.as_deref().unwrap_or("-")));
                }
                self.info = Some(InfoUi {
                    title: info.title,
                    owner: info.owner,
                    views: info.views,
                    pubdate: info.pubdate,
                    cover: info.cover,
                    description: info.description,
                    pages: self.pages.clone(),
                    current_page: info.current_page,
                });
                self.ui_dirty = true;
                log::line(&format!(
                    "detail info loaded pages={} current={}",
                    self.pages.len(),
                    self.current_page
                ));
                if self.sidebar_open && self.sidebar_tab == SidebarTab::Comments {
                    self.request_comments(true);
                }
            }
            DetailEvent::Info(Err(error)) => {
                log::line(&format!("detail info failed reason={error}"));
                self.show_toast(&format!("视频信息加载失败：{error}"), true);
            }
            DetailEvent::Comments(Ok(data)) => {
                if data.reset {
                    self.comments.clear();
                }
                for comment in data.comments {
                    let avatar = self.avatars.get(&comment.avatar_url).cloned();
                    self.comments.push(CommentUi {
                        user: comment.user,
                        message: comment.message,
                        likes: comment.likes,
                        avatar_url: comment.avatar_url,
                        avatar,
                    });
                }
                self.comments_total = data.total;
                self.comments_cursor = data.cursor;
                self.comments_end = data.end;
                self.comments_loading = false;
                self.ui_dirty = true;
                log::line(&format!(
                    "comments loaded n={} total={} end={}",
                    self.comments.len(),
                    self.comments_total,
                    u8::from(self.comments_end)
                ));
            }
            DetailEvent::Comments(Err(error)) => {
                self.comments_loading = false;
                self.ui_dirty = true;
                log::line(&format!("comments failed reason={error}"));
                self.show_toast(&format!("评论加载失败：{error}"), true);
            }
            DetailEvent::Avatars(list) => {
                for (url, image) in list {
                    self.avatars.insert(url.clone(), image.clone());
                    for comment in self.comments.iter_mut() {
                        if comment.avatar_url == url {
                            comment.avatar = Some(image.clone());
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
        let speed = speed.clamp(0.5, 2.0) as f32;
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
        self.interp = self.interp.next();        self.shared.interp.store(self.interp.as_u8(), Ordering::SeqCst);
        self.ui_dirty = true;
        log::line(&format!(
            "interp mode={} label={} out_hz={}",
            self.interp.as_u8(),
            self.interp.label(),
            self.interp_out_hz.unwrap_or(0)
        ));
        if self.interp == InterpMode::Cpu && self.interp_out_hz.is_none() {
            log::line(&format!(
                "interp requested but source_fps={:.2} already fills the grid",
                self.source.fps
            ));
        }
    }

    fn cycle_enhance(&mut self) {
        self.set_enhance(self.enhance.next());
    }

    fn toggle_stats(&mut self) {
        self.stats = !self.stats;
        if self.stats {
            self.open_sidebar_tab(SidebarTab::Performance);
        } else if self.sidebar_open && self.sidebar_tab == SidebarTab::Performance {
            self.set_sidebar(false, SidebarTab::Performance);
        }
        self.ui_dirty = true;
        log::line(&format!("stats={}", u8::from(self.stats)));
    }

    fn set_enhance(&mut self, mode: EnhanceMode) {
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

    fn set_aspect(&mut self, aspect: AspectMode) {
        if self.aspect == aspect {
            self.ui_dirty = true;
            return;
        }
        self.aspect = aspect;
        self.shared.aspect.store(aspect.as_u8(), Ordering::SeqCst);
        self.update_video_rect();
        self.state_changed();
        self.ui_dirty = true;
        log::line(&format!(
            "aspect mode={} label={} client={}x{}",
            aspect.as_u8(),
            aspect.label(),
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
                log::line(&format!("ontop change failed enabled={} err={error:#}", u8::from(ontop)));
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
        const SPEEDS: [f32; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
        let index = SPEEDS
            .iter()
            .position(|speed| (speed - self.speed).abs() < 0.001)
            .unwrap_or(2);
        let next = (index as i32 + direction).rem_euclid(SPEEDS.len() as i32) as usize;
        self.set_speed(SPEEDS[next] as f64);
    }

    /// Re-present the held frame so brightness/audio-only changes are visible
    /// even while paused. Never changes the playback clock.
    fn repaint_picture(&mut self) {
        let mut mode = self.enhance;
        if let Some(bytes) = self.cur.as_deref() {
            match Self::present_bytes(
                &mut self.video,
                &self.gpu,
                &mut self.enhance_pipeline,
                &mut mode,
                bytes,
                self.brightness,
                self.audio_only,
            ) {
                Ok((_enhanced, presented)) => {
                    if presented {
                        self.presents_window = self.presents_window.saturating_add(1);
                    }
                    if mode != self.enhance {
                        self.enhance = mode;
                        self.shared.enhance.store(mode.as_u8(), Ordering::SeqCst);
                        self.ui_dirty = true;
                    }
                }
                Err(error) => log::line(&format!("repaint failed: {error:#}")),
            }
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
            self.hwnd.0, self.client.0, self.client.1, self.scale, self.source.width, self.source.height
        ));
        let mut last_status = String::new();
        while !self.quit {
            window::pump();
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
                    }
                    Ok(Err(message)) => {
                        self.source_pending = false;
                        self.loading = false;
                        self.report_error(format!("打开视频失败：{message}"));
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        self.source_pending = false;
                        self.loading = false;
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
                Err(TryRecvError::Disconnected) => {
                    Some(Err("画质解析线程已结束".to_string()))
                }
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
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("分P解析线程已结束".to_string())),
            });
            if let Some(result) = switch_result {
                self.switch_rx = None;
                match result {
                    Ok(source) => self.apply_switched_source(source),
                    Err(error) => {
                        self.switch_target = None;
                        log::line(&format!("source switch failed reason={error}"));
                        self.show_toast(&format!("分P切换失败：{error}"), true);
                    }
                }
            }
            self.run_qa();
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
            self.tick_heartbeat();
            self.tick_sleep();
            self.check_sessions();
            if let Some(limit) = self.auto_exit {
                if self.started_at.elapsed() >= limit {
                    log::line(&format!(
                        "auto-exit seconds={:.3}",
                        limit.as_secs_f64()
                    ));
                    break;
                }
            }

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
            self.shared.position_ms.store(self.position_ms, Ordering::SeqCst);

            if self.chrome_shown
                && !self.dragging
                && self.popup_drag.is_none()
                && !self.sidebar_open
                && !self.danmaku_popup
                && self.dm_menu.is_none()
                && self.last_mouse_activity.elapsed() >= Duration::from_secs(3)
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
            if self.ui_dirty || self.last_ui_draw.elapsed() >= Duration::from_millis(200) {
                if let Err(error) = self.draw_ui() {
                    log::line(&format!("overlay draw failed: {error:#}"));
                }
                self.last_ui_draw = Instant::now();
                self.ui_dirty = false;
            }

            if self.fps_window_start.elapsed() >= Duration::from_secs(1) {
                let elapsed = self.fps_window_start.elapsed().as_secs_f64();
                self.fps = self.presents_window as f32 / elapsed as f32;
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
                log::line(&format!(
                    "fps presented={} rate={:.2} source_fps={:.2} out_hz={} interp={} enhance={} queue={queue} dropped={dropped} pos_ms={} audio_ms={audio_pos} danmaku={}/{} dm_frames={}",
                    self.presents_window,
                    self.fps,
                    self.source.fps,
                    self.interp_out_hz.unwrap_or(0),
                    self.interp.label(),
                    self.enhance.label(),
                    self.position_ms,
                    self.danmaku.total(),
                    self.danmaku.active_count(),
                    self.danmaku_layer.frames
                ));
                self.shared
                    .fps_tenths
                    .store((self.fps * 10.0).round() as u32, Ordering::SeqCst);
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
            if self.audio_master
                && audio.clock.starved_ms.load(Ordering::SeqCst) > 350
            {
                return "正在缓冲…".to_string();
            }
        }
        String::new()
    }

    /// Take decoded frames due on the current presentation clock and present
    /// them; also drive the interpolation grid.
    fn poll_picture(&mut self) {
        if self.decoder.is_none() {
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

        let now = Instant::now();
        let position = self.position_now();
        self.position_ms = position;
        self.shared.position_ms.store(position, Ordering::SeqCst);

        self.maybe_enable_audio_master();
        self.log_drift_if_needed();

        // Take the newest due picture. The reader is gated to real time, so at
        // most one source frame is due per output period in normal playback.
        let frame = self
            .decoder
            .as_ref()
            .and_then(|decoder| decoder.queue.take_due(position, 6));
        let (queue_closed, queue_len) = self
            .decoder
            .as_ref()
            .map(|decoder| (decoder.queue.is_closed(), decoder.queue.len()))
            .unwrap_or((true, 0));
        let had_frame = frame.is_some();
        if let Some(frame) = frame {
            self.last_frame_ts = frame.ts_ms;
            self.display_new(frame, false);
        } else if self.interp == InterpMode::Cpu && self.interp_out_hz.is_some() {
            if let Some(due) = self.blend_at {
                if now >= due && self.prev.is_some() && self.cur.is_some() {
                    self.present_blend();
                    self.blend_at = None;
                }
            }
        }
        // A queue with frames but no presented picture for two seconds means
        // the reader and the presentation clock disagree (or the clock
        // stopped); log it instead of silently freezing. Normal inter-frame
        // gaps never reach this threshold.
        if had_frame && self.stall_until.is_none() {
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
        // clock has reached the tail (or no duration is known).
        if queue_closed && queue_len == 0 {
            let reached_end = self.duration_ms == 0 || position + 800 >= self.duration_ms;
            if reached_end {
                if self.loop_one {
                    // 单曲循环: same window, same sessions restarted at zero.
                    log::line("loop_one replay");
                    self.ended = false;
                    self.requested_play = true;
                    self.seek(0);
                    self.play();
                    return;
                }
                self.ended = true;
                self.playing = false;
                self.requested_play = false;
                self.sync_shared();
                self.on_playback_end();
                self.gate(false);
                if let Some(audio) = &self.audio {
                    audio.send(AudioCommand::Pause);
                }
                self.danmaku.clear();
                if let Err(error) = self.danmaku_layer.clear() {
                    log::line(&format!("danmaku clear on end failed: {error:#}"));
                }
                log::line("danmaku cleared reason=end");
                self.ui_dirty = true;
                log::line(&format!(
                    "ended position_ms={position} duration_ms={}",
                    self.duration_ms
                ));
            } else if self.had_first_frame
                && self.duration_ms > 0
                && position + 3_000 < self.duration_ms
                && self.error.is_none()
            {
                self.report_error("视频解码中断（ffmpeg 提前结束），点击重试");
            }
        }
    }

    fn display_new(&mut self, frame: DecodedFrame, first: bool) {
        let width = self.source.width as usize;
        let height = self.source.height as usize;
        let size = width * height * 4;
        let mut buffer = match self.prev.take() {
            Some(buffer) if buffer.len() == size => buffer,
            _ => vec![0u8; size],
        };
        convert::nv12_to_bgra(&mut buffer, &frame.data, width, height);
        let old_cur = self.cur.replace(buffer);
        // Keep the previous picture for the CPU blend; the buffer we just took
        // from `prev` is the one being overwritten, so steady state is two
        // allocations, not one per frame.
        self.prev = old_cur;
        self.last_frame_ts = frame.ts_ms;
        let mut mode = self.enhance;
        let Some(bytes) = self.cur.as_deref() else {
            return;
        };
        match Self::present_bytes(
            &mut self.video,
            &self.gpu,
            &mut self.enhance_pipeline,
            &mut mode,
            bytes,
            self.brightness,
            self.audio_only,
        ) {
            Ok((enhanced, presented)) => {
                if presented {
                    self.presents_window = self.presents_window.saturating_add(1);
                }
                let _ = enhanced;
                if mode != self.enhance {
                    self.enhance = mode;
                    self.shared.enhance.store(mode.as_u8(), Ordering::SeqCst);
                    self.ui_dirty = true;
                }
            }
            Err(error) => log::line(&format!("present failed: {error:#}")),
        }
        self.last_source_present = Some(Instant::now());
        self.blend_at = if self.interp == InterpMode::Cpu && self.interp_out_hz.is_some() {
            Some(Instant::now() + self.out_period / 2)
        } else {
            None
        };
        if first {
            self.last_frame_ts = frame.ts_ms;
        }
    }

    fn present_blend(&mut self) {
        let (Some(previous), Some(current)) = (self.prev.as_ref(), self.cur.as_ref()) else {
            return;
        };
        let size = current.len();
        if self.blend_buffer.len() != size {
            self.blend_buffer.resize(size, 0);
        }
        let started = Instant::now();
        interp::blend_bgra_into(&mut self.blend_buffer, previous, current, 128);
        let elapsed_us = started.elapsed().as_micros().max(1) as u32;
        self.blend_us_total = self.blend_us_total.saturating_add(elapsed_us as u64);
        self.blend_count = self.blend_count.saturating_add(1);
        self.last_blend_us = elapsed_us;
        let mut mode = self.enhance;
        let bytes = std::mem::take(&mut self.blend_buffer);
        let result = Self::present_bytes(
            &mut self.video,
            &self.gpu,
            &mut self.enhance_pipeline,
            &mut mode,
            &bytes,
            self.brightness,
            self.audio_only,
        );
        self.blend_buffer = bytes;
        match result {
            Ok((enhanced, presented)) => {
                if presented {
                    self.presents_window = self.presents_window.saturating_add(1);
                }
                let _ = enhanced;
                if mode != self.enhance {
                    self.enhance = mode;
                    self.shared.enhance.store(mode.as_u8(), Ordering::SeqCst);
                    self.ui_dirty = true;
                }
            }
            Err(error) => log::line(&format!("blend present failed: {error:#}")),
        }
        self.generated = self.generated.saturating_add(1);
        self.shared.generated.store(self.generated, Ordering::SeqCst);
        self.shared.blend_us.store(elapsed_us, Ordering::SeqCst);
    }

    fn present_bytes(
        video: &mut VideoSurface,
        gpu: &Gpu,
        pipeline: &mut Option<EnhancePipeline>,
        mode: &mut EnhanceMode,
        bytes: &[u8],
        brightness: f32,
        audio_only: bool,
    ) -> Result<(bool, bool)> {
        if audio_only {
            return video.present_black(gpu).map(|presented| (false, presented));
        }
        video.stage(gpu, bytes)?;
        video.present(gpu, pipeline, mode, brightness)
    }

    fn maybe_enable_audio_master(&mut self) {
        if self.audio_master {
            return;
        }
        let Some(audio) = &self.audio else {
            return;
        };
        if !audio.clock.available.load(Ordering::SeqCst)
            || !audio.clock.ready.load(Ordering::SeqCst)
            || !audio.clock.playing.load(Ordering::SeqCst)
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
            // The sound joined late; keep the wall clock rather than jump the
            // picture backwards. Logged once per session (not every loop).
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if !REPORTED.swap(true, Ordering::SeqCst) {
                log::line(&format!(
                    "av-drift audio_ms={audio_ms} wall_ms={wall_ms} delta_ms={} action=keep-wall-clock",
                    audio_ms as i64 - wall_ms as i64
                ));
            }
        }
    }

    fn log_drift_if_needed(&mut self) {
        if !self.audio_master {
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
            Command::SetVolume(value) => self.set_volume(value),
            Command::SetSpeed(value) => self.set_speed(value),
            Command::SetInterp(mode) => {
                self.interp = mode;
                self.shared.interp.store(mode.as_u8(), Ordering::SeqCst);
                self.ui_dirty = true;
                log::line(&format!("interp mode={} label={}", mode.as_u8(), mode.label()));
            }
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
                    self.shared
                        .double_click_seq
                        .fetch_add(1, Ordering::SeqCst);
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
            InputEvent::Resize { width, height } => {
                if width < 32 || height < 32 {
                    return;
                }
                self.client = (width, height);
                self.apply_client_geometry();
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
                if !self.sidebar_open && !self.danmaku_popup && self.dm_menu.is_none() {
                    self.chrome_shown = false;
                    self.ui_dirty = true;
                }
            }
            InputEvent::MouseMove { x, y } => {
                self.note_activity();
                self.pointer = Some((x as f32, y as f32));
                if let Some((arm_x, arm_y)) = self.window_drag_arm {
                    if (x - arm_x).abs() > 4 || (y - arm_y).abs() > 4 {
                        self.window_drag_arm = None;
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
                if self.dragging {
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
                if self.window_drag_arm.take().is_some() {
                    self.toggle_play();
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
                    || (self.sidebar_open && layout.sidebar.contains(x as f32, y as f32))
                {
                    return;
                }
                self.toggle_fullscreen();
            }
            InputEvent::Wheel(delta) => {
                let step = (delta as f32 / 120.0).round() as i32;
                if self.sidebar_open
                    && self
                        .pointer
                        .map(|(x, y)| self.overlay.layout.sidebar.contains(x, y))
                        .unwrap_or(false)
                {
                    self.sidebar_scroll =
                        (self.sidebar_scroll - step as f32 * 42.0).max(0.0);
                    self.ui_dirty = true;
                    return;
                }
                let value = (self.volume as i32 + step * 5).clamp(0, 100) as u8;
                self.set_volume(value);
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
            // The mini window is frameless too: keep the top 26 px as a drag
            // handle, and make the rest of the picture play/pause.
            if (y as f32) < 26.0 * self.scale {
                window::drag_window(self.hwnd);
            } else {
                self.toggle_play();
            }
            return;
        }
        self.note_activity();
        let layout = self.overlay.layout;
        let px = x as f32;
        let py = y as f32;
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
        if !self.fullscreen && self.chrome_shown {
            if layout.top_close.contains(px, py) {
                log::line("window close button");
                window::close_window(self.hwnd);
                return;
            }
            if layout.top_min.contains(px, py) {
                log::line("window minimize button");
                window::minimize(self.hwnd);
                return;
            }
            if layout.top_max.contains(px, py) {
                log::line("window maximize button");
                window::toggle_maximize(self.hwnd);
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
        if self.sidebar_open {
            if layout.sidebar_close.contains(px, py) {
                self.set_sidebar(false, self.sidebar_tab);
                return;
            }
            for (index, rect) in layout.sidebar_tabs.iter().enumerate() {
                if rect.contains(px, py) {
                    self.open_sidebar_tab(SidebarTab::from_index(index));
                    return;
                }
            }
            if layout.sidebar_content.contains(px, py)
                || layout.comments_more.contains(px, py)
            {
                if self.handle_sidebar_click(px, py, &layout) {
                    return;
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
                self.drag_preview = Some(
                    (layout.progress_fraction(px) as f64 * self.duration_ms as f64) as u64,
                );
                self.ui_dirty = true;
                return;
            }
            if layout.play.contains(px, py) {
                self.toggle_play();
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
            // Standalone window: defer play/pause until release so a drag on
            // the picture can move the window instead.
            self.window_drag_arm = Some((x, y));
        } else {
            self.toggle_play();
        }
    }

    fn handle_right_click(&mut self, x: i32, y: i32) {
        self.note_activity();
        let px = x as f32;
        let py = y as f32;
        if self.dm_menu.is_some() {
            self.close_dm_menu();
            return;
        }
        if self.mini {
            return;
        }
        match self.danmaku_hit_at(px, py) {
            Some(hit) => self.open_dm_menu(&hit, px, py),
            None => {
                log::line(&format!("danmaku menu no-hit x={px:.0} y={py:.0}"));
            }
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

    fn toolbar_action(&mut self, index: usize) {
        match index {
            TB_VOLUME => self.toggle_mute(),
            TB_DANMAKU => self.toggle_danmaku(),
            TB_DANMAKU_SETTINGS => {
                self.danmaku_popup = !self.danmaku_popup;
                self.ui_dirty = true;
                log::line(&format!("danmaku popup={}", u8::from(self.danmaku_popup)));
            }
            TB_SPEED => self.cycle_speed(1),
            TB_ASPECT => self.cycle_aspect(),
            TB_SHOT => self.screenshot(),
            TB_MINI => self.toggle_mini(),
            TB_FULLSCREEN => self.toggle_fullscreen(),
            TB_PLAYLIST => self.open_sidebar_tab(SidebarTab::Episodes),
            TB_SIDEBAR => self.set_sidebar(!self.sidebar_open, self.sidebar_tab),
            TB_SETTINGS => self.open_sidebar_tab(SidebarTab::Settings),
            _ => {}
        }
    }

    fn set_sidebar(&mut self, open: bool, tab: SidebarTab) {
        self.restore_live_danmaku();
        self.close_dm_menu();
        self.sidebar_open = open;
        self.sidebar_tab = tab;
        self.sidebar_scroll = 0.0;
        self.settings_menu = None;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.overlay
            .set_chrome_state(self.sidebar_open, self.fullscreen);
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
        self.restore_live_danmaku();
        self.close_dm_menu();
        self.sidebar_open = true;
        self.sidebar_tab = tab;
        self.sidebar_scroll = 0.0;
        self.settings_menu = None;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.overlay
            .set_chrome_state(self.sidebar_open, self.fullscreen);
        self.ui_dirty = true;
        log::line(&format!("sidebar=1 tab={}", tab.label()));
        self.on_sidebar_tab(tab);
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
                    if layout.episode_row(index, self.sidebar_scroll).contains(x, y) {
                        self.switch_page(index);
                        return true;
                    }
                }
            }
            SidebarTab::Comments => {
                if layout.comments_sort[0].contains(x, y) {
                    self.comments_hot = true;
                    self.request_comments(true);
                    return true;
                }
                if layout.comments_sort[1].contains(x, y) {
                    self.comments_hot = false;
                    self.request_comments(true);
                    return true;
                }
                if layout.comments_more.contains(x, y) && !self.comments_loading && !self.comments_end {
                    self.request_comments(false);
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
                    if layout.history_row(index, self.sidebar_scroll).contains(x, y) {
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
                if self.settings_menu.is_some() {
                    if layout.settings_menu.contains(x, y) {
                        let count = self.settings_menu_len();
                        for index in 0..count {
                            if layout.settings_menu_row(index, 0.0).contains(x, y) {
                                self.select_settings_menu_row(index);
                                return true;
                            }
                        }
                        return true;
                    }
                    self.settings_menu = None;
                    self.ui_dirty = true;
                    return true;
                }
                for index in 0..SETTINGS_ROWS {
                    if layout.settings_row(index, self.sidebar_scroll).contains(x, y) {
                        self.settings_action(index);
                        return true;
                    }
                }
            }
            SidebarTab::Performance => {}
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
        if index == self.current_page {
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
            self.show_toast("当前是直链，无法切换到分P", true);
            return;
        };
        let cid = page.cid;
        let quality = self.source.qn;
        let codecid = self.source.codecid;
        let audio_index = self.source.audio_index;
        log::line(&format!(
            "source switch request bvid={bvid} cid={cid} page={} title={:?}",
            index + 1,
            page.title
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        self.switch_rx = Some(rx);
        self.switch_target = Some(index);
        let danmaku_tx = self.danmaku_tx.clone();
        let _ = std::thread::Builder::new()
            .name("player-source-switch".into())
            .spawn(move || {
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
        self.show_toast("正在切换分P…", false);
    }

    fn apply_switched_source(&mut self, new_source: MediaSource) {
        let size_changed =
            new_source.width != self.source.width || new_source.height != self.source.height;
        let new_surface = if size_changed {
            match VideoSurface::new(&self.gpu, new_source.width, new_source.height) {
                Ok(surface) => Some(surface),
                Err(error) => {
                    log::line(&format!("page switch surface failed: {error:#}"));
                    self.show_toast("新分P的视频层创建失败，保留当前分P", true);
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
                        "enhance fallback on page switch reason={error:#}"
                    ));
                    None
                }
            }
        } else {
            None
        };
        if let Some(index) = self.switch_target.take() {
            self.current_page = index;
        }
        self.reset_danmaku_for_source();
        self.stop_sessions();
        if let Some(surface) = new_surface {
            if let Err(error) = self.composition.set_video_content(&surface.swap) {
                log::line(&format!("page switch composition failed: {error:#}"));
                self.show_toast("切换分P失败，恢复当前分P", true);
                if let Err(reopen) = self.arm_new_sessions(0) {
                    self.report_error(format!("恢复播放失败：{reopen:#}"));
                }
                return;
            }
            self.video = surface;
            self.enhance_pipeline = new_pipeline;
            if self.enhance != EnhanceMode::Off && self.enhance_pipeline.is_none() {
                self.enhance = EnhanceMode::Off;
                self.shared.enhance.store(0, Ordering::SeqCst);
                self.show_toast("新分辨率下画质增强不可用，已关闭", true);
            }
            self.cur = None;
            self.prev = None;
            self.blend_buffer.clear();
            self.last_source_present = None;
            self.had_first_frame = false;
        }
        self.source = new_source;
        self.restart_heartbeat_for_source();
        self.reset_storyboard();
        self.settings_menu = None;
        self.duration_ms = self.source.duration_ms;
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
        if let Err(error) = self.danmaku_layer.clear() {
            log::line(&format!("danmaku clear on source switch failed: {error:#}"));
        }
        if let Err(error) = self.open_sessions(0, false) {
            log::line(&format!("source switch open failed: {error:#}"));
            self.show_toast("分P加载失败", true);
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
        self.show_toast("已切换分P", false);
    }

    /// Install the source that the worker resolved while the window was
    /// already showing its loading card: swap in the real video surface and
    /// start the decode/audio sessions.
    fn apply_deferred_source(&mut self, open: crate::DeferredOpen) {
        let crate::DeferredOpen {
            mut source,
            session,
            detail,
        } = open;
        if source.title.trim().is_empty() {
            source.title = self.source.title.clone();
        }
        let title = source.title.clone();
        let size_changed =
            source.width != self.source.width || source.height != self.source.height;
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
            self.prev = None;
            self.blend_buffer.clear();
            self.last_source_present = None;
        }
        self.loading = false;
        self.source = source;
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
        self.restart_heartbeat_for_source();
        self.reset_storyboard();
        self.settings_menu = None;
        self.duration_ms = self.source.duration_ms;
        let start = clamp_seek(self.requested_start_ms, self.duration_ms);
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
        let display_hz = interp::display_refresh_hz();
        self.interp_out_hz = interp::interp_plan(display_hz, self.source.fps);
        let out_hz = self.interp_out_hz.unwrap_or_else(|| {
            if self.source.fps.is_finite() && self.source.fps > 1.0 {
                self.source.fps.round().clamp(1.0, 240.0) as u32
            } else {
                60
            }
        });
        self.out_period = Duration::from_secs_f64(1.0 / out_hz.max(1) as f64);
        self.danmaku.set_items(Vec::new());
        if let Err(error) = self.danmaku_layer.clear() {
            log::line(&format!("danmaku clear on initial source failed: {error:#}"));
        }
        self.refresh_danmaku_geometry();
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
        self.sync_shared();
    }

    fn settings_action(&mut self, index: usize) {
        if (index == 0 || index == 1) && !self.source.has_track_menu() {
            self.show_toast("当前来源是直链，没有可切换的画质/音轨", true);
            return;
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
            2 => self.cycle_speed(1),
            3 => self.cycle_aspect(),
            4 => self.step_brightness(0.1),
            5 => self.cycle_interp(),
            6 => self.cycle_enhance(),
            7 => self.toggle_danmaku(),
            8 => {
                self.danmaku_popup = !self.danmaku_popup;
                self.ui_dirty = true;
            }
            9 => self.toggle_ontop(),
            10 => self.toggle_audio_only(),
            11 => self.cycle_sleep(),
            12 => {
                log::line(&format!("player keys hint={}", self.player_keys_hint));
                self.show_toast(
                    &format!("快捷键来自 设置 › 快捷键（{}）", self.player_keys_hint),
                    false,
                );
            }
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

    fn settings_menu_len(&self) -> usize {
        match self.settings_menu {
            Some(SettingsMenu::Quality) => self.quality_rows().len(),
            Some(SettingsMenu::Audio) => self.audio_rows().len(),
            None => 0,
        }
    }

    fn open_settings_menu(&mut self, menu: SettingsMenu) {
        self.sidebar_open = true;
        self.sidebar_tab = SidebarTab::Settings;
        self.sidebar_scroll = 0.0;
        self.chrome_shown = true;
        self.last_mouse_activity = Instant::now();
        self.overlay.set_chrome_state(true, self.fullscreen);
        self.settings_menu = Some(menu);
        self.ui_dirty = true;
        log::line(&format!(
            "settings menu={menu:?} rows={}",
            match menu {
                SettingsMenu::Quality => self.quality_rows().len(),
                SettingsMenu::Audio => self.audio_rows().len(),
            }
        ));
    }

    fn select_settings_menu_row(&mut self, index: usize) {
        match self.settings_menu {
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
                    self.show_toast(
                        &format!("{} 需要大会员，当前账号无法播放", row.label),
                        true,
                    );
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
            self.prev = None;
            self.blend_buffer.clear();
            self.last_source_present = None;
            self.had_first_frame = false;
        }
        self.source = new_source;
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
        self.position_ms = target;
        self.base_ms = target;
        self.anchor = None;
        self.paused_at = target;
        self.audio_master = false;
        self.first_frame_at = None;
        self.blend_at = None;
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
            && self.session_started.elapsed() >= Duration::from_secs(15)
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
        let (Some(session), Some(bvid), Some(cid)) =
            (self.session.clone(), self.source.bvid.clone(), self.source.cid)
        else {
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
                        .block_on(session.danmaku_post(
                            cid,
                            &bvid,
                            &text,
                            progress,
                            1,
                            25,
                            16_777_215,
                            0,
                        ))
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
        if layout.popup_send.contains(x, y) {
            let send_button_left = layout.popup_send.right - 56.0 * layout.scale;
            if x >= send_button_left {
                self.send_danmaku();
            } else {
                self.dm_input_focus = true;
            }
            self.ui_dirty = true;
            return true;
        }
        if layout.popup_close.contains(x, y) {
            self.danmaku_popup = false;
            self.dm_input_focus = false;
            self.ui_dirty = true;
            return true;
        }
        if layout.popup.contains(x, y) {
            let slop = 8.0 * layout.scale;
            if (layout.popup_opacity.top - slop..=layout.popup_opacity.bottom + slop)
                .contains(&y)
                && x >= layout.popup_opacity.left
            {
                self.popup_drag = Some(0);
                self.drag_popup(0, x, layout);
                return true;
            }
            if (layout.popup_speed.top - slop..=layout.popup_speed.bottom + slop).contains(&y)
                && x >= layout.popup_speed.left
            {
                self.popup_drag = Some(1);
                self.drag_popup(1, x, layout);
                return true;
            }
            for (index, rect) in layout.popup_font.iter().enumerate() {
                if rect.contains(x, y) {
                    self.set_danmaku_font(index);
                    return true;
                }
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
        let s = layout.scale;
        let left = layout.popup.left + 90.0 * s;
        let right = layout.popup.right - 54.0 * s;
        let fraction = ((x - left) / (right - left).max(1.0)).clamp(0.0, 1.0);
        if kind == 0 {
            self.set_danmaku_opacity((fraction * 100.0).round() as u32);
        } else {
            self.set_danmaku_speed((50.0 + fraction * 150.0).round() as u32);
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
        if let Err(error) = self
            .composition
            .set_root_clip(self.client.0, self.client.1)
        {
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

    fn apply_client_geometry(&mut self) {
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
        let (x, y, scale_x, scale_y) = crate::gpu::video_transform(
            self.client.0 as f32,
            self.client.1 as f32,
            self.source.width,
            self.source.height,
            self.aspect,
        );
        if let Err(error) = self
            .composition
            .set_video_transform(x, y, scale_x, scale_y)
        {
            log::line(&format!("video transform failed: {error:#}"));
        }
        self.refresh_danmaku_geometry();
    }

    fn refresh_danmaku_geometry(&mut self) {
        let band = danmaku_band(
            self.client,
            &self.overlay.layout,
            &self.source,
            self.mini,
            self.aspect,
        );
        let position = self.position_now();
        self.danmaku_band = band;
        self.danmaku.relayout(band);
        self.danmaku.reseed(position);
        if let Err(error) = self.danmaku_layer.resize(self.client.0, self.client.1) {
            log::line(&format!("danmaku layer resize failed: {error:#}"));
        }
        if let Err(error) = self.danmaku_layer.set_fonts(self.danmaku.font_sizes()) {
            log::line(&format!("danmaku fonts failed: {error:#}"));
        }
        if let Err(error) = self.danmaku_layer.clear() {
            log::line(&format!("danmaku clear after resize failed: {error:#}"));
        }
        log::line(&format!(
            "danmaku layout client={}x{} band={}x{} rows_ready=1",
            self.client.0, self.client.1, band.width, band.height
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
        log::line(&format!("storyboard request bvid={} cid={cid}", self.source.bvid.as_deref().unwrap_or("-")));
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
        self.danmaku_layer.reset_layouts();
        let position = self.position_now();
        self.danmaku.reseed(position);
        if let Err(error) = self.danmaku_layer.set_fonts(self.danmaku.font_sizes()) {
            log::line(&format!("danmaku set fonts failed: {error:#}"));
        }
        let _ = self.danmaku_layer.clear();
        self.last_danmaku_draw = Instant::now();
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
        self.last_danmaku_draw = Instant::now();
    }

    fn draw_danmaku(&mut self) {
        if !self.danmaku_visible
            || self.danmaku.is_empty()
            || self.danmaku.settings().opacity <= 0.0
            || !self.playing
        {
            return;
        }
        // The comment layer runs on the display grid, independent of the video
        // rate: 120 Hz panels get 120 Hz scrolling comments, a 60 Hz panel gets
        // 60. The old fixed 16 ms cap silently pinned every panel to 60.
        let hz = crate::interp::display_refresh_hz().clamp(30, 120);
        let interval = Duration::from_secs_f64(1.0 / f64::from(hz));
        let now = Instant::now();
        if now.duration_since(self.last_danmaku_draw) < interval {
            return;
        }
        let position = self.position_now();
        let changed = self.danmaku.update(position);
        let frame = self.danmaku.frame(position);
        if frame.is_empty() && !changed {
            self.last_danmaku_draw = now;
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
        match self.danmaku_layer.draw(&frame, clip, force_clear) {
            Ok(()) => {
                self.last_danmaku_draw = now;
            }
            Err(error) => {
                log::line(&format!("danmaku draw failed: {error:#}"));
                self.last_danmaku_draw = now;
            }
        }
    }

    // -------------------------------------------------------------------- UI

    fn draw_ui(&mut self) -> Result<()> {
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
            position_ms: self.drag_preview.unwrap_or(self.position_ms),
            duration_ms: self.duration_ms,
            volume: self.volume,
            muted: self.muted,
            interp: self.interp,
            enhance: self.enhance,
            speed: self.speed,
            fps: self.fps,
            source_fps: self.source.fps,
            generated: self.generated,
            blend_us: self.last_blend_us,
            stats: self.stats,
            status: self.status.clone(),
            source_size: (self.source.width, self.source.height),
            qn_label: self.source.qn_label.clone(),
            codec: self.source.codec.clone(),
            drag_preview: self.drag_preview,
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
            comments_hot: self.comments_hot,
            comments_loading: self.comments_loading,
            comments_end: self.comments_end,
            liked: self.liked,
            coined: self.coined,
            favorited: self.favorited,
            toast: self.toast.clone(),
            quality_label: self.source.qn_label.clone(),
            audio_label: self.source.audio_label.clone(),
            quality_rows: self.quality_rows(),
            audio_rows: self.audio_rows(),
            settings_menu: self.settings_menu,
            sleep_label: self.sleep_label(),
            keys_label: self.player_keys_hint.clone(),
            hover_preview: self.hover_preview,
            storyboard_cell,
            light_sidebar: self.light_sidebar,
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
            audio_ms: self.audio_ms,
            ui_ms: self.ui_ms,
            renderer: "D3D11 + DirectComposition".to_string(),
        };
        self.overlay.set_chrome_state(self.sidebar_open, self.fullscreen);
        let started = Instant::now();
        let result = self.overlay.draw(&model);
        self.ui_ms = started.elapsed().as_secs_f32() * 1000.0;
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
            if let Some(hit) = self
                .danmaku
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
                    self.set_enhance(EnhanceMode::Cas);
                }
                QaKind::Anime => {
                    log::line("qa anime");
                    self.set_enhance(EnhanceMode::Anime);
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
                QaKind::Fullscreen => self.toggle_fullscreen(),
                QaKind::Danmaku => {
                    log::line("qa danmaku on");
                    self.set_danmaku_visible(true);
                    self.last_danmaku_draw = Instant::now() - Duration::from_millis(20);
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
                QaKind::Sidebar => {
                    log::line("qa sidebar");
                    self.set_sidebar(!self.sidebar_open, self.sidebar_tab);
                }
                QaKind::SidebarInfo => {
                    log::line("qa sidebar info");
                    self.open_sidebar_tab(SidebarTab::Info);
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
                QaKind::DanmakuPopup => {
                    log::line("qa danmaku popup");
                    self.danmaku_popup = !self.danmaku_popup;
                    self.chrome_shown = true;
                    self.last_mouse_activity = Instant::now();
                    self.ui_dirty = true;
                    log::line(&format!(
                        "danmaku popup={}",
                        u8::from(self.danmaku_popup)
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
                QaKind::DmBlock => {
                    match self.qa_first_danmaku_hit() {
                        Some(hit) => {
                            log::line("qa danmaku block");
                            self.block_danmaku_sender(&hit.mid_hash);
                        }
                        None => {
                            log::line("qa danmaku block no visible comment");
                            self.show_toast("当前画面没有可见弹幕", false);
                        }
                    }
                }
                QaKind::DmCopy => {
                    match self.qa_first_danmaku_hit() {
                        Some(hit) => {
                            log::line("qa danmaku copy");
                            self.copy_danmaku_text(&hit.text);
                        }
                        None => {
                            log::line("qa danmaku copy no visible comment");
                            self.show_toast("当前画面没有可见弹幕", false);
                        }
                    }
                }
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
                    self.comments_hot = false;
                    self.request_comments(true);
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
                        let current = rows
                            .iter()
                            .position(|row| row.current)
                            .unwrap_or(0);
                        let row = (current + 1) % rows.len();
                        log::line(&format!("qa audio next row={row} index={}", rows[row].index));
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
        self.gate(false);
        self.sync_shared();
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
    client: (u32, u32),
    layout: &crate::overlay::Layout,
    source: &MediaSource,
    mini: bool,
    aspect: AspectMode,
) -> Band {
    let (x, y, width, height) = crate::gpu::video_bounds(
        client.0 as f32,
        client.1 as f32,
        source.width,
        source.height,
        aspect,
    );
    let video_bottom = y + height;
    // Keep the first lane below the top glass bar: the bar always sits above
    // the comments, and hiding the first line under it made the top of the
    // video look empty while comments scrolled behind the title.
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
        window_height: client.1,
    }
}

fn window_size() -> (u32, u32) {    let text = std::env::var("BILI_PLAYER_SIZE").unwrap_or_default();
    if let Some((width, height)) = text.split_once('x') {
        if let (Ok(width), Ok(height)) = (width.trim().parse(), height.trim().parse()) {
            return (width, height);
        }
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

fn parse_qa(_duration_ms: u64) -> Vec<QaAction> {    let Ok(text) = std::env::var("BILI_PLAYER_QA") else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    let mut at = 3.5f64;
    for token in text.split(',').map(|token| token.trim().to_ascii_lowercase()) {
        let kind = match token.as_str() {
            "" => continue,
            "seek" => QaKind::Seek,
            "pause" => QaKind::Pause,
            "resume" | "play" => QaKind::Play,
            "interp" => QaKind::Interp,
            "enhance" | "cas" => QaKind::Enhance,
            "anime" | "enhance2" => QaKind::Anime,
            "shot" | "screenshot" => QaKind::Shot,
            "stats" => QaKind::Stats,
            "mute" => QaKind::Mute,
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
            "sidebar" => QaKind::Sidebar,
            "info" => QaKind::SidebarInfo,
            "episodes" | "parts" => QaKind::SidebarEpisodes,
            "comments" | "comment" => QaKind::SidebarComments,
            "performance" | "perf" => QaKind::SidebarPerformance,
            "settings" | "set" => QaKind::SidebarSettings,
            "dmpopup" | "danmakupopup" => QaKind::DanmakuPopup,
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
        log::line(&format!("qa actions={} start_at=3.5s step=1.8s", actions.len()));
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let action = |key: u32| keys.iter().position(|c| player_candidate_vk(*c) == Some(key));
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
