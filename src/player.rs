//! Video playback driven by child processes.
//!
//! `ffmpeg.exe` decodes the referer-gated DASH video stream to raw RGBA frames
//! at the **display size** (see [`target_frame_size`]); `ffplay.exe` plays the
//! audio track. Neither library is linked — both are spawned with
//! `CREATE_NO_WINDOW`. Frames are handed to the player window through
//! `slint::invoke_from_event_loop`, so the UI thread never blocks. Audio is
//! best-effort: any failure degrades to video-only.

use std::collections::VecDeque;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use regex::Regex;
use slint::{ModelRc, SharedString, VecModel};

use crate::danmaku::Danmaku;
use crate::prefetch::{self, PrefetchConfig, PrefetchHandle, PrefetchStats};
use crate::{
    bilibili, AudioEntry, DanmakuItem, DanmakuRow, MiniPlayerWindow, PlayerWindow, QualityEntry,
    QualityRow,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A track counts as finished when the playhead is this close to the duration;
/// the last frame is presented slightly before the pacer reads end-of-stream.
const TRACK_END_SLACK_MS: u64 = 300;
/// How far from the reported duration a decoder end-of-stream may land before
/// it is treated as a read failure instead of the real end of the track.
const TRACK_END_FAILSAFE_MS: u64 = 3000;

/// The control of the most recently opened player, reachable from the tray
/// thread (which cannot capture the UI-thread `Rc<PlayerState>`). Used only by
/// the boss key to mute/unmute playback while the main window is hidden.
static ACTIVE_CONTROL: std::sync::Mutex<Option<Arc<PlaybackControl>>> =
    std::sync::Mutex::new(None);

pub fn set_active_control(control: Option<Arc<PlaybackControl>>) {
    if let Ok(mut guard) = ACTIVE_CONTROL.lock() {
        *guard = control;
    }
}

pub fn active_control() -> Option<Arc<PlaybackControl>> {
    ACTIVE_CONTROL.lock().ok().and_then(|guard| guard.clone())
}

/// Real playback/streaming metrics of the most recently opened player, or
/// `None` when no player is open (or none has prepared a track yet).
///
/// This is the whole profiler API: one call returns the complete [`Stats`]
/// snapshot, so a UI timer can poll it directly.
#[allow(dead_code)] // Read by the 网络分析 panel, which is wired separately.
pub fn stats_snapshot() -> Option<Stats> {
    active_control().and_then(|control| control.play_stats())
}


/// Shared slot holding the mini (PiP) window handle while it is open. `Weak` is
/// `Send`, so the decode thread can reach the mini window from the UI closure.
pub type MiniSlot = Arc<std::sync::Mutex<Option<slint::Weak<MiniPlayerWindow>>>>;

// ---- Decode target -------------------------------------------------------
//
// The decoder used to scale every stream to a fixed 960x540, so a 1080P source
// was shown at 540p upscaled to the window — visibly softer than the web
// player. The target is now the size the picture is actually drawn at:
// `min(source, window physical size, 1920x1080)`, aspect preserved and rounded
// to even numbers (ffmpeg's rgba/rgb24 scaler is happier with them).
//
// Fullscreen uses the monitor (the player window covers it, so the native rect
// already is the screen). A window without a native handle yet falls back to
// [`DEFAULT_DISPLAY_W`]x[`DEFAULT_DISPLAY_H`]; the UI publishes the real size
// within one 60 Hz tick.

/// Cap on a decoded frame, independent of the window or the source.
const FRAME_MAX_W: u32 = 1920;
const FRAME_MAX_H: u32 = 1080;
/// Display fallback before the UI has published a window size.
const DEFAULT_DISPLAY_W: u32 = 1280;
const DEFAULT_DISPLAY_H: u32 = 720;

/// One raw RGBA frame size. Every frame of a session is exactly this big, so
/// the reader and the ring can work in bytes without carrying dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSize {
    pub w: u32,
    pub h: u32,
}

impl FrameSize {
    pub const fn bytes(self) -> usize {
        (self.w as usize) * (self.h as usize) * 4
    }
}

/// The frame size to decode for a `source` track drawn in a `window` of
/// `window_w`x`window_h` physical px.
///
/// * never larger than the source (no upscaling),
/// * never larger than the window (no wasted decode),
/// * never larger than [`FRAME_MAX_W`]x[`FRAME_MAX_H`],
/// * aspect preserved, even width and height.
///
/// A zero source (unknown / audio-only) uses the window capped the same way.
pub fn target_frame_size(source_w: u32, source_h: u32, window_w: u32, window_h: u32) -> FrameSize {
    // Even floor/round: integer sizes keep the byte stride exact.
    fn even_down(value: u32) -> u32 {
        (value & !1).max(2)
    }
    fn even_round(value: f64) -> u32 {
        let rounded = value.round().max(2.0) as u32;
        (rounded & !1).max(2)
    }

    let window_w = window_w.max(1);
    let window_h = window_h.max(1);
    let (mut source_w, mut source_h) = (source_w, source_h);
    if source_w < 2 || source_h < 2 {
        // Unknown geometry: assume the window's aspect at the window's size.
        source_w = window_w;
        source_h = window_h;
    }
    let cap_w = FRAME_MAX_W.min(window_w).min(source_w);
    let cap_h = FRAME_MAX_H.min(window_h).min(source_h);
    let scale = (f64::from(cap_w) / f64::from(source_w))
        .min(f64::from(cap_h) / f64::from(source_h))
        .min(1.0);
    if scale >= 0.999 {
        // Source fits: decode natively (only made even).
        return FrameSize {
            w: even_down(source_w),
            h: even_down(source_h),
        };
    }
    // Height is the constraining axis when a 16:9 source is letterboxed into a
    // 16:9 window; derive the width from the source aspect so the pair stays
    // exact to within one pixel instead of accumulating two independent roundings.
    let h = even_round(f64::from(source_h) * scale);
    let w = even_round(f64::from(h) * f64::from(source_w) / f64::from(source_h));
    FrameSize {
        w: w.min(even_down(cap_w)),
        h: h.min(even_down(cap_h)),
    }
}

/// Whether `next` differs from `current` by more than [`RESIZE_RESTART_RATIO`]
/// on either axis. Small drags are absorbed (the picture scales a few percent
/// and nobody notices); a real resize restarts the decoder once.
fn size_changed_above_ratio(next: FrameSize, current: FrameSize) -> bool {
    let dw = (f64::from(next.w) - f64::from(current.w)).abs() / f64::from(current.w.max(1));
    let dh = (f64::from(next.h) - f64::from(current.h)).abs() / f64::from(current.h.max(1));
    dw > f64::from(RESIZE_RESTART_RATIO) || dh > f64::from(RESIZE_RESTART_RATIO)
}

/// Share of the current target a resize must exceed before the decoder is
/// restarted (15%: below it the difference is not visible at panel size).
const RESIZE_RESTART_RATIO: f32 = 0.15;
/// A resize must be still for this long before the decoder restarts, so a drag
/// or a maximise animation costs one restart instead of dozens.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Frame bytes the decode-ahead ring may hold. A fixed frame count is wrong
/// once the frame size varies: 16 frames of 1080p RGBA are 133 MB, 16 frames of
/// 480p only 16 MB. The budget keeps one player around 64 MB while still
/// buffering a useful amount of video at small sizes.
const RING_BUDGET_BYTES: usize = 64 << 20;
const RING_MIN_CAPACITY: usize = 4;
const RING_MAX_CAPACITY: usize = 16;

/// Frames the ring holds for `frame_bytes`-sized frames: the 64 MB budget
/// divided by one frame, clamped to `4..=16`.
fn ring_capacity_for(frame_bytes: usize) -> usize {
    if frame_bytes == 0 {
        return RING_MAX_CAPACITY;
    }
    (RING_BUDGET_BYTES / frame_bytes).clamp(RING_MIN_CAPACITY, RING_MAX_CAPACITY)
}

// ---- Frosted-glass thumbnail of the current video frame ------------------
//
// The player window's panels and letterbox bars sample a desktop snapshot for
// their glass. That snapshot is stale in fullscreen (nothing of the desktop
// shows through any more), so the panels can instead use a blurred copy of the
// video itself. This is a downscale to 96x54 followed by three small box-blur
// passes: at panel size the result only has to be colour, and 96x54 keeps the
// per-update cost in the microsecond range.
//
// The work runs on the pacer thread, on a frame it already popped from the
// ring (so no extra frame copy is made), at most every
// [`VIDEO_GLASS_INTERVAL`]. With no consumer it costs one atomic load per
// frame and nothing else; see [`PlaybackControl::set_video_glass_needed`].

/// Oldest published thumbnail: the panels only need the current colour, and
/// 10 fps is well below the blur's visibility threshold.
pub const VIDEO_GLASS_W: u32 = 96;
pub const VIDEO_GLASS_H: u32 = 54;
const VIDEO_GLASS_BYTES: usize = (VIDEO_GLASS_W as usize) * (VIDEO_GLASS_H as usize) * 4;
/// Update cadence: 10 fps keeps the cost measurable and the picture current.
const VIDEO_GLASS_INTERVAL: Duration = Duration::from_millis(100);
/// Box-blur kernel radius and pass count (radius 2, three passes ≈ gaussian).
const VIDEO_GLASS_BLUR_RADIUS: usize = 2;
const VIDEO_GLASS_BLUR_PASSES: usize = 3;

/// Fallback UI-push gate used when the track's frame period is unknown; the
/// effective gate is derived from the frame period (see [`push_interval_for`]).
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// While the pacer waits for a frame, the ring being empty this long means the
/// source has silently stalled (the TCP connection is alive but no frames
/// arrive): jump to the audio clock and respawn both decoders.
const STALL_EMPTY_MS: u64 = 2000;
/// Relaxed stall threshold until a (re)started decoder has produced its first
/// frame. ffmpeg is still connecting and seeking then, and a 2 s limit would
/// resync before it ever gets going (which is what makes a resync loop). A
/// healthy start takes ~1 s; this leaves generous headroom for a loaded
/// machine while still bounding a genuinely hung start.
const STALL_STARTUP_MS: u64 = 20_000;
/// How often the pacer re-checks an empty ring. Short enough that pause / seek /
/// close stay responsive while it would otherwise block on the reader; the ring
/// is normally non-empty, so this wait is rarely taken.
const STALL_POLL_MS: u64 = 100;

// ---- Start-of-playback pre-roll gate ------------------------------------
//
// A fresh session holds at its first available frame until the ring has
// buffered `PRE_ROLL_TARGET_MS` of video (or the timeout / end of stream), so
// the picture does not start moving while the decode-ahead ring is still
// nearly empty.
//
// Honest limit: the ring capacity follows the frame size (`ring_capacity_for`),
// so the requested 1.5 s is only reachable on low-frame-rate tracks. At 30 fps
// and 1080p the ring holds 7 frames (~0.23 s) and at the QA window size 16
// frames (~0.53 s); the gate then simply opens when the ring is full. It always
// asks for at most what the ring can hold.

/// Video the pre-roll asks to buffer, ms.
const PRE_ROLL_TARGET_MS: u64 = 1500;
/// Ceiling on the pre-roll wait, counted from the first decoded frame; a slow
/// network degrades to "start anyway" instead of stalling playback.
const PRE_ROLL_TIMEOUT_MS: u64 = 4000;
/// Same gate after a watchdog-confirmed stall: ring full, 2 s ceiling.
const PRE_ROLL_RECOVERY_TIMEOUT_MS: u64 = 2000;
/// Poll period of an armed gate: short enough that pause / seek / close react
/// quickly, long enough to be free while the ring fills.
const PRE_ROLL_POLL_MS: u64 = 20;

/// How often the pacer turns the cumulative frame counters into the rates of
/// [`Stats`]. Deliberately not per frame: 4 Hz is plenty for a profiler panel
/// and keeps the pacing loop free of formatting and extra clock calls.
const PROF_TICK_MS: u64 = 250;
/// Samples kept by the profiler's sliding window (5 × 250 ms ≈ 1 s).
const PROF_WINDOW: usize = 5;
/// Shortest window span trusted as a rate; below it the sample is skipped so a
/// seek or respawn cannot produce a spurious spike.
const PROF_MIN_WINDOW_MS: u64 = 200;

/// Per-frame pacing epsilon: every frame waits until the audio clock reaches
/// its presentation time. The old coarse "ahead" tolerance (120 ms) let the
/// loop drain 3-4 frames per sleep cycle and push only one of them, which
/// capped a 30 fps track at ~7 fps on screen.
const SYNC_PACE_EPS_MS: i64 = 5;
/// A frame this far behind the audio clock is dropped instead of pushed.
const SYNC_BEHIND_MS: i64 = 120;
/// When the video falls this far behind the audio clock the stream has stalled
/// (CDN hiccup): the position jumps to the audio clock and both decoders are
/// respawned there instead of dropping every frame forever.
const SYNC_RESYNC_MS: i64 = 3000;
/// A (re)spawned session gets this long to present its first frame before an
/// A/V gap may trigger the behind resync. It is defence in depth for the seek
/// path: a clock left over from the replaced session must never seek the user
/// away from the position they just picked, and a fresh session has no
/// meaningful A/V relationship before its first frame. Bounded (rather than
/// "first frame only") so a genuinely desynced start is still recovered.
const RESYNC_START_GRACE_MS: u64 = 500;
/// Cooldown between resyncs, shared by the >3 s-behind delta path and the
/// empty-ring stall watchdog. Tightened from 10 s so recovery from a silent
/// stall is quick (the stall itself is detected after ~2 s).
const RESYNC_COOLDOWN: Duration = Duration::from_secs(3);
/// Cap on one sync sleep so a stale clock can never stall the loop.
const MAX_SYNC_SLEEP_MS: u64 = 500;
/// How long an audio-clock sample may be extrapolated (with the wall clock)
/// before positions fall back to the wall clock entirely. ffplay refreshes its
/// `-stats` line many times per second, so this only covers burst gaps.
const AUDIO_CLOCK_FRESH_MS: u64 = 1500;
/// A seek into data the prefetch proxy has NOT fetched yet is buffering from the
/// first moment: the overlay is published before the decoder is even spawned.
///
/// A seek the proxy can still serve from its read-ahead keeps the current
/// picture instead, so a buffered seek never flashes the overlay (< 1 frame).
/// When there is no proxy (direct CDN read) the target's bufferedness cannot be
/// known up front: the overlay is published only once the new picture is still
/// missing after this grace, which keeps the common fast seek silent.
const SEEK_BUFFER_GRACE: Duration = Duration::from_millis(450);
/// Seconds of video the seek overlay treats as "enough buffered to start".
/// Used only to turn the proxy's measured read-ahead bytes into an honest
/// percentage; it is not a claim about the network.
const SEEK_BUFFER_WINDOW_S: f64 = 2.0;
/// How often the pacer re-tries applying the stored gain/mute to an audio
/// session that is not up yet. `ffplay` creates its WASAPI session a few
/// milliseconds after spawn, so the first attempt usually misses.
const AUDIO_SESSION_RETRY_MS: u64 = 100;

/// Monotonic milliseconds since the first call, for cross-thread freshness
/// checks (`Instant` itself cannot live in an atomic).
fn monotonic_ms() -> u64 {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    // +1 so the very first call never returns 0: `audio_clock_stamp_ms` uses 0
    // as its "no sample yet" sentinel, so a clock sampled during the process's
    // first millisecond would otherwise look absent and the position would fall
    // back to the wall clock. Every consumer only subtracts two samples, so a
    // constant offset cancels out.
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Burst guard for UI pushes: a quarter of a frame period, clamped to 8..=100 ms.
/// The pacing sleep already spaces frames one period apart; this only stops a
/// burst when the audio clock micro-jumps forward.
fn push_interval_for(frame_ms: f64) -> Duration {
    if frame_ms.is_finite() && frame_ms > 0.0 {
        Duration::from_secs_f64((frame_ms / 1000.0 * 0.25).clamp(0.008, 0.100))
    } else {
        MIN_FRAME_INTERVAL
    }
}

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
}

/// `PROCESS_SUSPEND_RESUME` (winnt.h): the access `NtSuspendProcess` needs.
const PROCESS_SUSPEND_RESUME: u32 = 0x0800;
/// `PROCESS_QUERY_LIMITED_INFORMATION` (winnt.h): `GetExitCodeProcess` needs
/// this (or `PROCESS_QUERY_INFORMATION`) to report whether the child is alive;
/// without it the liveness probe fails and the hook would stay silent.
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
/// `STILL_ACTIVE` (winbase.h): `GetExitCodeProcess` value of a live process.
const STILL_ACTIVE: u32 = 259;

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetExitCodeProcess(process: isize, code: *mut u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: isize) -> i32;
    fn NtResumeProcess(process: isize) -> i32;
}

/// Raise the process timer resolution to 1 ms. Without it the per-frame pacing
/// sleeps are quantised to the default ~15.6 ms tick, which capped a 30 fps
/// track at ~23 fps. `Once` keeps the winmm refcount balanced for the process,
/// and the function is `pub(crate)` so the UI-thread drag timers (8 ms) run on
/// the same one refcount instead of taking a second one.
pub(crate) fn ensure_high_res_timer() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: winmm is present on every Windows install and the call only
        // raises the timer resolution for this process.
        let rc = unsafe { timeBeginPeriod(1) };
        let probe = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        crate::debug_log(&format!(
            "timer-resolution rc={rc} sleep10={}ms",
            probe.elapsed().as_millis()
        ));
    });
}

/// Danmaku layout: the engine composes in a 960x540 logical space.
///
/// The Slint overlay maps that space onto the video band
/// (`parent.width / 960`, `parent.height / 540` in `ui/app.slint`), so the same
/// positions land correctly at any window or frame size. The decoded frame is
/// now display-sized instead of 960x540, but this space is deliberately NOT
/// scaled by `frame_w / 960`: the overlay's own band mapping already is that
/// transform, and applying it here too would double-scale every item (a 16:9
/// frame is not 960 wide). See [`active_items`].
const VIDEO_W: f32 = 960.0;
const VIDEO_H: f32 = 540.0;
/// Rows available for scrolling danmaku.
const LANES: usize = 14;
const LANE_H: f32 = 28.0;
const SCROLL_MS: f32 = 8000.0;
const FIXED_MS: u64 = 4000;

/// Bucket width for the locally computed danmaku-density curve.
const DENSITY_BUCKET_MS: u64 = 2000;
/// Cap on density buckets; wider buckets are used for long videos.
const DENSITY_MAX_BUCKETS: u64 = 400;
/// Cap on danmaku-list rows materialised into the UI model.
const DANMAKU_LIST_MAX: usize = 5000;

const MEDIA_HEADERS: &str = "Referer: https://www.bilibili.com/\r\nUser-Agent: Mozilla/5.0 \
                             (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like \
                             Gecko) Chrome/125.0.0.0 Safari/537.36\r\n";

/// One reading of the player's real playback/streaming counters, for the
/// 网络分析 panel.
///
/// Every field is measured from a counter that exists in the playback path;
/// nothing here is a declared value dressed up as a measurement. The rates are
/// derived by the pacer's 4 Hz sampler over a sliding ~1 s window (never per
/// frame), so a paused or just-restarted stream legitimately reads `0.0`
/// until frames flow again.
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // The panel's API surface; the UI reads these fields.
pub struct Stats {
    /// Frames handed to the UI per second (pacer → `set_frame`), measured.
    pub fps_presented: f32,
    /// Frames read from the decoder's stdout per second (reader thread).
    pub fps_decoded: f32,
    /// Milliseconds from this session's first decoder spawn to the first frame
    /// entering the ring; `None` until that frame arrives.
    pub first_frame_ms: Option<u32>,
    /// Seconds of video buffered ahead of the playhead: ring occupancy divided
    /// by the track's frame period. This is decode-ahead, not whole-file buffer.
    pub buffer_s: f32,
    /// Change of `buffer_s` per second over the sampler's window (positive =
    /// the decode-ahead ring is growing).
    pub buffer_delta_per_s: f32,
    /// **Estimate** of the network rate seen by the decoder: the track's
    /// declared bitrate scaled by `1 + buffer_delta_per_s`, clamped to `>= 0`.
    /// It assumes buffer growth tracks throughput, so it is not a metered
    /// socket rate.
    pub estimated_bps: u64,
    /// The track's declared bitrate from the play URL, bits/s.
    pub declared_bps: u64,
    /// Current playhead, ms (audio clock while fresh, wall clock otherwise).
    pub position_ms: u64,
    /// Total media duration, ms (`0` until the play info arrives).
    pub duration_ms: u64,
    /// Empty-ring stalls detected by the watchdog since this player opened.
    pub stalls: u32,
    /// Longest of those stalls, ms (measured from the first empty poll to the
    /// frame that resolved it).
    pub longest_stall_ms: u32,
    /// Frames decoded but skipped by the pacer because they fell behind the
    /// audio clock (its existing drop counter).
    pub dropped_frames: u64,
    /// DASH codec id of the active track (`7` AVC, `12` HEVC, `13` AV1,
    /// `0` unknown).
    pub codecid: u32,
    pub width: u32,
    pub height: u32,
    /// Size of the RGBA frame the decoder actually produces for this session
    /// (the display-sized target, see [`target_frame_size`]).
    pub frame_w: u32,
    pub frame_h: u32,
    /// The track's declared frame rate (`fps=` in the play URL).
    pub declared_fps: f32,
    /// Selected quality (`qn`).
    pub qn: i64,
    pub paused: bool,
    pub audio_only: bool,
}

/// Static stream parameters of the active track; written once per spawn (never
/// on a hot path) and read only when a snapshot is taken.
#[derive(Clone, Copy, Default)]
struct ProfMedia {
    qn: i64,
    codecid: u32,
    width: u32,
    height: u32,
    fps: f32,
    bps: u64,
}

/// Atomics behind [`Stats`].
///
/// The reader thread and the pacer only ever touch `presented`, `decoded`,
/// `ring_len` and `first_frame_ms` with plain relaxed stores/adds - no lock, no
/// syscall, no formatting. The sampler writes the derived rates 4×/s and the
/// snapshot reads them on demand.
struct Prof {
    /// Frames handed to the UI, cumulative for this player window.
    presented: AtomicU64,
    /// Frames read from the decoder's stdout, cumulative for this player window.
    decoded: AtomicU64,
    /// Current ring occupancy in frames.
    ring_len: AtomicU64,
    /// Frame period of the active track in microseconds (`0` = unknown).
    frame_us: AtomicU64,
    /// Spawn→first-frame latency of this session and whether it is set.
    first_frame_ms: AtomicU64,
    first_seen: AtomicBool,
    /// Derived rates, kept as integers so no atomic f32 is needed.
    fps_presented_milli: AtomicU64,
    fps_decoded_milli: AtomicU64,
    /// Change of the buffered length, µs of video per second of wall time.
    buffer_delta_us_per_s: AtomicI64,
    stalls: AtomicU64,
    longest_stall_ms: AtomicU64,
    media: Mutex<ProfMedia>,
    /// A track has been prepared; a snapshot is meaningful from then on.
    active: AtomicBool,
}

impl Prof {
    fn new() -> Self {
        Self {
            presented: AtomicU64::new(0),
            decoded: AtomicU64::new(0),
            ring_len: AtomicU64::new(0),
            frame_us: AtomicU64::new(0),
            first_frame_ms: AtomicU64::new(0),
            first_seen: AtomicBool::new(false),
            fps_presented_milli: AtomicU64::new(0),
            fps_decoded_milli: AtomicU64::new(0),
            buffer_delta_us_per_s: AtomicI64::new(0),
            stalls: AtomicU64::new(0),
            longest_stall_ms: AtomicU64::new(0),
            media: Mutex::new(ProfMedia::default()),
            active: AtomicBool::new(false),
        }
    }
}

/// Shared state for one player window, readable from both the UI and the
/// worker thread.
///
/// `seek_seq` and `audio_seq` are restart triggers for the decode loop: bumping
/// `seek_seq` restarts *both* streams at the tracked position (used by seek and
/// speed changes), while `audio_seq` restarts only the audio process at the
/// current position (used by volume / mute, so the picture never stutters).
pub struct PlaybackControl {
    closed: AtomicBool,
    paused: AtomicBool,
    position_ms: AtomicU64,
    duration_ms: AtomicU64,
    seek_seq: AtomicU64,
    audio_seq: AtomicU64,
    /// Generation of the audio clock. Every spawned `ffplay` claims a value at
    /// spawn and stamps its `-stats` samples with it; a sample from a process
    /// that a seek / quality switch / volume change already replaced is
    /// ignored, so the old process' last burst can never be mistaken for the
    /// new session's position.
    audio_clock_seq: AtomicU64,
    /// Latest ffplay-reported playback position (ms) and the monotonic time it
    /// was recorded (`0` = no live clock yet). Together they are the master
    /// clock while fresh.
    audio_clock_ms: AtomicU64,
    audio_clock_stamp_ms: AtomicU64,
    /// Playback speed times 1000 (1000 = 1.0x).
    speed_milli: AtomicU64,
    /// Selected DASH video quality (`qn`).
    quality_qn: AtomicU64,
    /// 画质 menu catalogue: one entry per raw DASH stream, in the same order as
    /// the `quality_rows` model published to the UI, so `quality_pick(index)`
    /// can name an exact stream.
    quality_catalog: std::sync::Mutex<Vec<QualitySpec>>,
    /// The exact stream a `quality_pick` selected (`qn`, `codecid`,
    /// `bandwidth`). `None` = the automatic rule in [`auto_track_for`].
    quality_exact: std::sync::Mutex<Option<(i64, i64, u64)>>,
    /// Danmaku font step: 0 = 小, 1 = 标准, 2 = 大.
    danmaku_font: AtomicU64,
    /// Danmaku scroll speed as a percent (50..=200).
    danmaku_speed: AtomicU64,
    /// Danmaku display area: top percent of the video (25 / 50 / 75 / 100).
    danmaku_area: AtomicU64,
    /// Per-type danmaku blocks.
    block_scroll: AtomicBool,
    block_top: AtomicBool,
    block_bottom: AtomicBool,
    /// Sender `midHash`es that are never rendered.
    blocked_senders: std::sync::Mutex<Vec<String>>,
    /// Compiled keyword/regex block rules applied to danmaku text.
    block_rules: std::sync::Mutex<BlockRules>,
    /// Whether hardware decoding (d3d11va) is requested.
    hwaccel: AtomicBool,
    /// Preferred video codec (`0` = 自动, else a DASH `codecid`).
    codec_pref: AtomicU64,
    /// 仅音频 mode: video decode is stopped, audio keeps playing.
    audio_only: AtomicBool,
    /// Frames decoded but not pushed to the UI (drop-frame counter).
    dropped: AtomicU64,
    /// `true` while the start-of-playback pre-roll holds the first frame: the
    /// pacer presents nothing and the position does not advance. Read by the
    /// UI's buffering indicator; a plain relaxed load.
    buffering: AtomicBool,
    /// Ring occupancy the armed pre-roll waits for, in frames (`0` = none).
    buffer_target: AtomicU64,
    /// Target (ms) of a user seek waiting for its first presented frame,
    /// `u64::MAX` = none. Set by [`PlaybackControl::seek`], consumed by the
    /// decode loop so the buffering overlay can be armed from the seek itself.
    pending_seek_ms: AtomicU64,
    /// Position (ms) playback was at when that seek was issued, so the decoder
    /// can tell a backward seek (already served) from a forward one.
    pending_seek_from_ms: AtomicU64,
    /// `true` while the current `buffering` state belongs to a seek (rather
    /// than the start-of-playback pre-roll), so the overlay can show the seek's
    /// own quantified detail line.
    seek_buffering: AtomicBool,
    /// Monotonic ms at which that seek was issued (for the duration log line).
    seek_buffering_at: AtomicU64,
    /// Physical size of the player window last published by the UI's 60 Hz
    /// watch, and whether it was fullscreen. `0x0` means "not measured yet".
    display_w: AtomicU64,
    display_h: AtomicU64,
    display_fullscreen: AtomicBool,
    /// Size of the RGBA frame the running decode session produces (`0x0` until
    /// the first session starts). Read by the screenshot path.
    frame_w: AtomicU64,
    frame_h: AtomicU64,
    /// Most recent frame that was pushed to the UI, for 截图.
    last_frame: std::sync::Mutex<LastFrame>,
    /// Volume in `0..=100`.
    volume: AtomicU64,
    muted: AtomicBool,
    /// PID of the audio `ffplay` this player owns (`0` = none). Gain and mute
    /// are applied to ITS audio session, so a volume change never restarts the
    /// process.
    audio_pid: AtomicU64,
    /// A gain/mute write is still owed to the current session: set when a new
    /// `ffplay` spawns (its WASAPI session appears some milliseconds later) and
    /// cleared once the write landed.
    audio_session_pending: AtomicBool,
    /// Monotonic ms of the last session-sync attempt, so the pacer retries a
    /// session that is not up yet at a bounded rate instead of every frame.
    audio_session_try_ms: AtomicU64,
    /// How many gain/mute changes fell back to restarting `ffplay`. Diagnostic:
    /// a live session serves them all, so this stays put.
    audio_restarts: AtomicU64,
    /// Whether the player windows are pinned above others (`WS_EX_TOPMOST`).
    topmost: AtomicBool,
    /// Selected `audio_tracks[]` index.
    audio_index: AtomicU64,
    /// Real playback/streaming counters behind [`Stats`].
    prof: Prof,
    /// Loopback prefetch proxy feeding this player's decoder, if one is up.
    /// Per player on purpose: several player windows can be open at once and
    /// must never share a port, a read cursor or a shutdown flag.
    prefetch: std::sync::Mutex<Option<PrefetchSession>>,
    /// Whether the running decode session reads through the proxy (false when
    /// a quality switch selected a track the proxy was not opened for).
    prefetch_in_use: AtomicBool,
    /// Sticky "this track has finished" flag for the current playback session,
    /// latched by [`PlaybackControl::track_ended`] and the decode loop and
    /// cleared by a seek, a quality switch or a new control (see
    /// [`TRACK_END_SLACK_MS`]).
    ended: AtomicBool,
    /// A consumer (the panel glass) wants a blurred frame. While false the
    /// pacer does no downscale and the published buffer is dropped.
    video_glass_needed: AtomicBool,
    /// Published 96x54 RGBA8 thumbnail, empty while nothing consumes it.
    video_glass: std::sync::Mutex<Vec<u8>>,
    /// Cumulative thumbnail updates and compute microseconds, read by the
    /// `BILI_VIDEO_GLASS_LOG=1` line.
    video_glass_frames: AtomicU64,
    video_glass_us: AtomicU64,
}

/// One row of the 画质 menu: a single raw DASH stream (not a `qn`).
///
/// `PlayInfo::qualities` already carries one entry per `dash.video[]`
/// representation, sorted `qn` high→low then bitrate high→low. This is the
/// copy the player keeps beside the UI model, so a row index resolves back to
/// the exact `(qn, codecid, bandwidth)` triple and the already-formatted copy
/// can be re-published (`current`) without another playurl call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualitySpec {
    pub qn: i64,
    pub codecid: i64,
    pub bandwidth: u64,
    pub label: String,
    pub detail: String,
    pub badge: String,
}

/// The pre-formatted `detail` line of a 画质 row:
/// `1920×1080 · 60fps · HEVC · 4.2 Mbps`.
fn quality_detail(option: &bilibili::QualityOption) -> String {
    format!(
        "{}×{} · {}fps · {} · {}",
        option.width,
        option.height,
        quality_fps(option.fps),
        option.codec,
        quality_mbps(option.bandwidth_bps)
    )
}

/// Frame rate as the menu shows it: whole numbers, `-` when the manifest had
/// no usable value.
fn quality_fps(fps: f32) -> String {
    if fps.is_finite() && fps > 0.5 && fps < 1000.0 {
        format!("{fps:.0}")
    } else {
        "-".to_string()
    }
}

/// Declared bitrate in Mbps: one decimal from 1 Mbps up, two below it, so a
/// low-bitrate stream never reads `0.0 Mbps`.
fn quality_mbps(bps: u64) -> String {
    let mbps = bps as f64 / 1_000_000.0;
    if mbps >= 1.0 {
        format!("{mbps:.1} Mbps")
    } else {
        format!("{mbps:.2} Mbps")
    }
}

/// The badge chip of a 画质 row (`""` when the stream carries none).
fn quality_badge(option: &bilibili::QualityOption) -> &'static str {
    if option.hdr {
        "HDR"
    } else if option.dolby_vision {
        "杜比"
    } else if option.eight_k {
        "8K"
    } else if bilibili::is_vip_quality(option.qn) {
        "会员"
    } else {
        ""
    }
}

/// The automatic stream rule for playback: **highest bandwidth at `qn`**.
///
/// The web player's 1080P AVC stream is the reference: at the same `qn` and
/// resolution, the higher-bitrate representation is the sharper one. (The old
/// rule took the *lowest* bitrate, which resolved 1080P to the ~0.58 Mbps AV1
/// stream instead of the ~1.18 Mbps AVC one — the second cause of the soft
/// picture.)
///
/// An explicit `codec_pref` (7 AVC / 12 HEVC / 13 AV1) still wins, as the
/// highest-bitrate variant of that codec at `qn`. If the preferred codec is not
/// available at that `qn`, the highest-bandwidth stream among the rest is used
/// instead of silently dropping to another quality tier.
///
/// `qn` with no stream falls back to `default_qn`, then to the first track —
/// the same fallbacks as [`bilibili::PlayInfo::video_track_for`]. The manual
/// 画质 picker ([`PlaybackControl::set_quality_exact`]) bypasses this rule.
pub fn auto_track_for(
    info: &bilibili::PlayInfo,
    qn: i64,
    codec_pref: i64,
) -> Option<&bilibili::VideoTrack> {
    auto_track_at_qn(info, qn, codec_pref)
        .or_else(|| auto_track_at_qn(info, info.default_qn, codec_pref))
        .or_else(|| info.video_tracks.first())
}

fn auto_track_at_qn(
    info: &bilibili::PlayInfo,
    qn: i64,
    codec_pref: i64,
) -> Option<&bilibili::VideoTrack> {
    if codec_pref != 0 {
        let preferred = info
            .video_tracks
            .iter()
            .filter(|track| track.id == qn && track.codecid == codec_pref)
            .max_by_key(|track| track.bandwidth);
        if preferred.is_some() {
            return preferred;
        }
    }
    // 自动 (or the preferred codec is missing at this quality): the sharpest
    // stream at the requested quality.
    info.video_tracks
        .iter()
        .filter(|track| track.id == qn)
        .max_by_key(|track| track.bandwidth)
}

/// The bandwidths offered at `qn`, for the one `stream auto` log line.
fn bandwidths_at_qn(info: &bilibili::PlayInfo, qn: i64) -> String {
    info.video_tracks
        .iter()
        .filter(|track| track.id == qn)
        .map(|track| format!("{}/{}", track.codecid, track.bandwidth))
        .collect::<Vec<_>>()
        .join(",")
}

/// Build the catalogue (and the published row copy) from the raw stream list.
pub fn quality_specs(qualities: &[bilibili::QualityOption]) -> Vec<QualitySpec> {
    qualities
        .iter()
        .map(|option| QualitySpec {
            qn: option.qn,
            codecid: option.codecid,
            bandwidth: option.bandwidth_bps,
            label: option.label.clone(),
            detail: quality_detail(option),
            badge: quality_badge(option).to_string(),
        })
        .collect()
}

/// Turn the catalogue into the UI model, marking the stream that is playing.
///
/// `current` is the exact `(qn, codecid, bandwidth)` the decoder selected, so
/// a pick at the same `qn` but another codec marks the right row.
pub fn quality_rows(specs: &[QualitySpec], current: Option<(i64, i64, u64)>) -> Vec<QualityRow> {
    specs
        .iter()
        .map(|spec| QualityRow {
            label: spec.label.clone().into(),
            detail: spec.detail.clone().into(),
            badge: spec.badge.clone().into(),
            current: current
                == Some((spec.qn, spec.codecid, spec.bandwidth)),
        })
        .collect()
}

/// Most recent frame handed to the UI, with the exact size it was decoded at.
struct LastFrame {
    bytes: Vec<u8>,
    w: u32,
    h: u32,
}

/// One player's prefetch proxy: the handle plus the CDN URL it is bound to.
/// Dropping the handle shuts the server down and closes the loopback port.
struct PrefetchSession {
    handle: PrefetchHandle,
    /// The exact DASH URL the proxy was started for. A decode session only uses
    /// `handle.local_url` while its track URL equals this one.
    source_url: String,
    /// Host label used in the `prefetch active` / `prefetch disabled` lines.
    host: String,
}

impl PlaybackControl {
    pub fn new() -> Arc<Self> {
        let control = Arc::new(Self {
            closed: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            position_ms: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            seek_seq: AtomicU64::new(0),
            audio_seq: AtomicU64::new(0),
            audio_clock_seq: AtomicU64::new(0),
            audio_clock_ms: AtomicU64::new(0),
            audio_clock_stamp_ms: AtomicU64::new(0),
            speed_milli: AtomicU64::new(1000),
            quality_qn: AtomicU64::new(0),
            quality_catalog: std::sync::Mutex::new(Vec::new()),
            quality_exact: std::sync::Mutex::new(None),
            danmaku_font: AtomicU64::new(1),
            danmaku_speed: AtomicU64::new(100),
            danmaku_area: AtomicU64::new(100),
            block_scroll: AtomicBool::new(false),
            block_top: AtomicBool::new(false),
            block_bottom: AtomicBool::new(false),
            blocked_senders: std::sync::Mutex::new(Vec::new()),
            block_rules: std::sync::Mutex::new(BlockRules::default()),
            hwaccel: AtomicBool::new(true),
            codec_pref: AtomicU64::new(0),
            audio_only: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            buffering: AtomicBool::new(false),
            buffer_target: AtomicU64::new(0),
            pending_seek_ms: AtomicU64::new(u64::MAX),
            pending_seek_from_ms: AtomicU64::new(0),
            seek_buffering: AtomicBool::new(false),
            seek_buffering_at: AtomicU64::new(0),
            display_w: AtomicU64::new(0),
            display_h: AtomicU64::new(0),
            display_fullscreen: AtomicBool::new(false),
            frame_w: AtomicU64::new(0),
            frame_h: AtomicU64::new(0),
            last_frame: std::sync::Mutex::new(LastFrame {
                bytes: Vec::new(),
                w: 0,
                h: 0,
            }),
            volume: AtomicU64::new(100),
            muted: AtomicBool::new(false),
            audio_pid: AtomicU64::new(0),
            audio_session_pending: AtomicBool::new(false),
            audio_session_try_ms: AtomicU64::new(0),
            audio_restarts: AtomicU64::new(0),
            topmost: AtomicBool::new(false),
            audio_index: AtomicU64::new(0),
            prof: Prof::new(),
            prefetch: std::sync::Mutex::new(None),
            prefetch_in_use: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            video_glass_needed: AtomicBool::new(false),
            video_glass: std::sync::Mutex::new(Vec::new()),
            video_glass_frames: AtomicU64::new(0),
            video_glass_us: AtomicU64::new(0),
        });
        spawn_video_glass_qa(&control);
        control
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // The proxy owns a listener, a runtime task and up to six fetch
        // workers. `PrefetchHandle::shutdown` waits (bounded) for the server
        // task on a non-runtime thread, and both close paths run on the UI
        // thread, so the wait happens on a throwaway thread instead.
        if let Some(handle) = self.take_prefetch("player closed") {
            std::thread::spawn(move || drop(handle));
        }
    }

    /// Install the proxy this player's decoder should read from. Replaces (and
    /// shuts down) any previous one for the same player.
    pub fn install_prefetch(&self, handle: PrefetchHandle, source_url: String, host: String) {
        let previous = {
            let mut guard = match self.prefetch.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.replace(PrefetchSession {
                handle,
                source_url,
                host,
            })
        };
        self.prefetch_in_use.store(true, Ordering::SeqCst);
        if let Some(previous) = previous {
            let stats = previous.handle.snapshot();
            crate::debug_log(&format!(
                "prefetch disabled reason=replaced host={} bytes_fetched={}",
                previous.host, stats.bytes_fetched
            ));
            drop(previous);
        }
    }

    /// Whether a proxy is installed for this player.
    pub fn prefetch_active(&self) -> bool {        match self.prefetch.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// The URL to hand ffmpeg for `direct`: the loopback URL while the proxy
    /// was opened for exactly this track, otherwise the CDN URL unchanged.
    /// Also records whether the panel should show metered numbers.
    ///
    /// Called at the top of every decode session, so a seek/pause/quality
    /// respawn and a restart after [`PlaybackControl::disable_prefetch`] both
    /// re-evaluate the URL instead of reusing a stale one.
    pub fn media_url_for(&self, direct: &str) -> String {
        let local = self.prefetch_local_url();
        let source = self.prefetch_source();
        match (local, source) {
            (Some(local), Some(source)) if source == direct => {
                self.prefetch_in_use.store(true, Ordering::SeqCst);
                local
            }
            _ => {
                self.prefetch_in_use.store(false, Ordering::SeqCst);
                direct.to_string()
            }
        }
    }

    /// The CDN URL the installed proxy is bound to.
    fn prefetch_source(&self) -> Option<String> {
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .map(|session| session.source_url.clone())
    }

    /// Metered counters, but only while the current decode session reads
    /// through the proxy. `None` keeps the profiler's estimate path.
    pub fn prefetch_stats(&self) -> Option<PrefetchStats> {
        if !self.prefetch_in_use.load(Ordering::SeqCst) {
            return None;
        }
        self.prefetch_raw_stats()
    }

    /// Counters regardless of the in-use flag; the startup warmup watchdog
    /// polls this while ffmpeg is still connecting.
    pub fn prefetch_raw_stats(&self) -> Option<PrefetchStats> {
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.as_ref().map(|session| session.handle.snapshot())
    }

    /// The loopback URL of the installed proxy, if any. [`PlaybackControl::media_url_for`]
    /// gates it on the track the proxy was opened for.
    pub fn prefetch_local_url(&self) -> Option<String> {
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .map(|session| session.handle.local_url.clone())
    }

    /// Stop using the proxy: log why, then drop the handle (server task,
    /// fetch workers, listener). Returns whether a proxy was actually running.
    pub fn disable_prefetch(&self, reason: &str) -> bool {
        match self.take_prefetch(reason) {
            Some(handle) => {
                drop(handle);
                true
            }
            None => false,
        }
    }

    /// Remove the proxy from the player and log one `prefetch disabled` line.
    fn take_prefetch(&self, reason: &str) -> Option<PrefetchHandle> {
        self.prefetch_in_use.store(false, Ordering::SeqCst);
        let session = {
            let mut guard = match self.prefetch.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.take()
        }?;
        let stats = session.handle.snapshot();
        crate::debug_log(&format!(
            "prefetch disabled reason={reason} host={} local={} bytes_fetched={} failed_ranges={}",
            session.host, session.handle.local_url, stats.bytes_fetched, stats.failed_ranges
        ));
        Some(session.handle)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Flip the paused state and return the new value.
    pub fn toggle_paused(&self) -> bool {
        let next = !self.paused.load(Ordering::SeqCst);
        self.paused.store(next, Ordering::SeqCst);
        next
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Whether playback is currently paused (readable from the UI thread).
    pub fn paused(&self) -> bool {
        self.is_paused()
    }

    /// Whether this session's track has finished: sticky for the current
    /// playback session and cleared by a seek, a quality change or a new
    /// [`PlaybackControl`].
    ///
    /// Two signals feed it. The decode loop latches it when the decoder reports
    /// a clean end of stream ([`Self::note_stream_finished`]), and this accessor
    /// also latches it as soon as the playhead is within [`TRACK_END_SLACK_MS`]
    /// of the known duration — the last frame can be presented a few hundred ms
    /// before the pacer notices the pipe closing. Reading it is a cheap
    /// atomic load plus (once per 250ms tick) two atomic loads for the position
    /// and duration.
    pub fn track_ended(&self) -> bool {
        if !self.ended.load(Ordering::SeqCst) && !self.is_paused() {
            let duration = self.duration_ms();
            if duration > 0
                && self
                    .playback_position_ms()
                    .saturating_add(TRACK_END_SLACK_MS)
                    >= duration
            {
                self.ended.store(true, Ordering::SeqCst);
            }
        }
        self.ended.load(Ordering::SeqCst)
    }

    /// Latch the end of the track when the decoder reports the stream finished.
    ///
    /// A read error also marks the source as done, so the position is checked
    /// as well: only "at (or past) the end" counts, which keeps a mid-file
    /// failure from advancing the queue. An unknown duration (`0`) trusts the
    /// decoder.
    fn note_stream_finished(&self) {
        let duration = self.duration_ms();
        if duration == 0
            || self
                .playback_position_ms()
                .saturating_add(TRACK_END_FAILSAFE_MS)
                >= duration
        {
            self.ended.store(true, Ordering::SeqCst);
        }
    }

    /// Clear the sticky end flag: a new session, a seek and a quality switch
    /// all mean "this track is not over any more".
    fn clear_track_ended(&self) {
        self.ended.store(false, Ordering::SeqCst);
    }

    fn position_ms(&self) -> u64 {
        self.position_ms.load(Ordering::SeqCst)
    }

    /// Current playback position in milliseconds, taken from the audio clock
    /// while it is fresh and from the wall clock otherwise (video-only tracks,
    /// or before ffplay reports its first position).
    ///
    /// While the pre-roll gate holds the picture this reports the held
    /// position instead, so the time readout does not run ahead of the frame on
    /// screen and the buffer progress stays meaningful.
    pub fn playback_position_ms(&self) -> u64 {
        if self.buffering() {
            return self.position_ms();
        }
        self.audio_clock().unwrap_or_else(|| self.position_ms())
    }

    /// Record the newest audio playback position (ms) reported by ffplay and
    /// mark the clock fresh. `generation` is the sample generation claimed by
    /// the process that produced it (see
    /// [`Self::begin_audio_clock_generation`]).
    ///
    /// Returns whether the sample was accepted. A sample stamped with an older
    /// generation is dropped: after a backward seek the replaced ffplay keeps
    /// emitting `-stats` for a moment, and letting its position through would
    /// make the new session look seconds "behind" and re-seek it forward.
    pub fn set_audio_clock_ms(&self, generation: u64, position_ms: u64) -> bool {
        self.audio_clock_ms.store(position_ms, Ordering::SeqCst);
        self.audio_clock_stamp_ms.store(monotonic_ms(), Ordering::SeqCst);
        // Validate *after* publishing. `begin_audio_clock_generation` bumps the
        // generation before it drops the clock, so a bump racing this sample
        // either is visible to this check (reject it here) or invalidates the
        // clock after it. A stale sample can therefore never survive.
        if self.audio_clock_seq() != generation {
            self.audio_clock_stamp_ms.store(0, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Latest accepted audio-clock generation (the generation of the running
    /// `ffplay`'s samples).
    fn audio_clock_seq(&self) -> u64 {
        self.audio_clock_seq.load(Ordering::SeqCst)
    }

    /// Claim the generation for the `ffplay` that is about to be spawned and
    /// drop the replaced process' clock. The increment happens before the spawn
    /// so a `-stats` sample already in flight fails the generation check in
    /// [`Self::set_audio_clock_ms`].
    fn begin_audio_clock_generation(&self) -> u64 {
        let generation = self.audio_clock_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.audio_clock_stamp_ms.store(0, Ordering::SeqCst);
        generation
    }

    /// Latest audio playback position in ms, extrapolated from the last ffplay
    /// sample with the wall clock while it is recent; `None` means "no usable
    /// audio clock". Extrapolation keeps the position smooth between ffplay's
    /// `-stats` bursts instead of freezing and then snapping.
    pub fn audio_clock(&self) -> Option<u64> {
        let stamp = self.audio_clock_stamp_ms.load(Ordering::SeqCst);
        if stamp == 0 {
            return None;
        }
        let age = monotonic_ms().saturating_sub(stamp);
        if age > AUDIO_CLOCK_FRESH_MS {
            return None;
        }
        let base = self.audio_clock_ms.load(Ordering::SeqCst);
        let speed = self.speed_milli.load(Ordering::SeqCst) as f64 / 1000.0;
        Some(base.saturating_add((age as f64 * speed) as u64))
    }

    /// Drop the audio clock and retire the samples of the process it came
    /// from, so positions fall back to the wall clock until the (re)spawned
    /// audio process reports a fresh one.
    fn invalidate_audio_clock(&self) {
        self.begin_audio_clock_generation();
    }

    fn set_position_ms(&self, value: u64) {
        self.position_ms.store(value, Ordering::SeqCst);
    }

    pub fn set_duration_ms(&self, value: u64) {
        self.duration_ms.store(value, Ordering::SeqCst);
    }

    /// Total media duration in milliseconds; `0` until the play info arrives.
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms.load(Ordering::SeqCst)
    }

    /// Seek to `position_ms`, clamped to the known duration, then restart both
    /// the video and audio streams at that point.
    pub fn seek(&self, position_ms: u64) {
        let duration = self.duration_ms();
        let target = if duration > 0 {
            position_ms.min(duration)
        } else {
            position_ms
        };
        // Remember where the seek came from: a backward jump is inside data the
        // proxy already served, and must never flash the loading overlay.
        self.pending_seek_from_ms
            .store(self.playback_position_ms(), Ordering::SeqCst);
        self.position_ms.store(target, Ordering::SeqCst);
        // Hand the target to the decode loop: it decides whether the proxy
        // still has that region, and therefore whether the buffering overlay
        // belongs on screen until the first frame of the new position.
        self.pending_seek_ms.store(target, Ordering::SeqCst);
        self.bump_seek();
    }

    fn seek_seq(&self) -> u64 {
        self.seek_seq.load(Ordering::SeqCst)
    }

    /// Trigger a full restart of both streams and drop the audio clock so the
    /// position does not jump back to the pre-seek value while ffplay respawns.
    fn bump_seek(&self) {
        // A seek (or a quality switch, which reuses this path) leaves whatever
        // part of the track was playing: the end flag belongs to the old spot.
        self.clear_track_ended();
        self.invalidate_audio_clock();
        self.seek_seq.fetch_add(1, Ordering::SeqCst);
    }

    fn restart_audio(&self) {
        self.invalidate_audio_clock();
        self.audio_seq.fetch_add(1, Ordering::SeqCst);
    }

    fn audio_seq(&self) -> u64 {
        self.audio_seq.load(Ordering::SeqCst)
    }

    /// Current playback speed as a factor (`0.5 ..= 2.0`).
    pub fn speed(&self) -> f64 {
        self.speed_milli.load(Ordering::SeqCst) as f64 / 1000.0
    }

    /// Change the speed factor; a change restarts both streams at the current
    /// position so picture and sound stay in sync.
    pub fn set_speed(&self, speed: f64) {
        let clamped = speed.clamp(0.5, 2.0);
        let milli = (clamped * 1000.0).round() as u64;
        if self.speed_milli.swap(milli, Ordering::SeqCst) != milli {
            self.bump_seek();
        }
    }

    /// Currently selected quality (`qn`).
    pub fn quality(&self) -> i64 {
        self.quality_qn.load(Ordering::SeqCst) as i64
    }

    /// Store the quality used for the very first decode without triggering a
    /// restart (playback has not started yet).
    pub fn set_quality_initial(&self, qn: i64) {
        self.quality_qn.store(qn as u64, Ordering::SeqCst);
        // A new video starts on the automatic rule for its preferred `qn`.
        if let Ok(mut guard) = self.quality_exact.lock() {
            *guard = None;
        }
        self.clear_track_ended();
    }

    /// Switch quality; restarts both streams at the current position via the
    /// same seek machinery, so audio and danmaku stay in sync.
    ///
    /// This is the `qn`-only entry point: the decoder picks the stream for that
    /// `qn` with the usual rule (codec preference, then the highest bitrate at
    /// that quality — see [`auto_track_for`]). It drops any exact pick made
    /// before it.
    pub fn set_quality(&self, qn: i64) {
        let cleared = self
            .quality_exact
            .lock()
            .map(|mut guard| guard.take().is_some())
            .unwrap_or(false);
        if self.quality_qn.swap(qn as u64, Ordering::SeqCst) != qn as u64 || cleared {
            self.bump_seek();
        }
    }

    /// Switch to one exact DASH stream (画质 menu pick) and restart both
    /// streams at the current position.
    ///
    /// `qn` + `codecid` + `bandwidth` identify a single `dash.video[]`
    /// representation; the decode loop looks it up before falling back to the
    /// automatic rule. Re-picking the same exact stream does not restart
    /// anything.
    pub fn set_quality_exact(&self, qn: i64, codecid: i64, bandwidth: u64) {
        let previous = self
            .quality_exact
            .lock()
            .ok()
            .and_then(|mut guard| guard.replace((qn, codecid, bandwidth)));
        let changed = previous != Some((qn, codecid, bandwidth));
        if self.quality_qn.swap(qn as u64, Ordering::SeqCst) != qn as u64 || changed {
            self.bump_seek();
        }
    }

    /// The exact stream selected by the 画质 menu, if any.
    fn exact_quality(&self) -> Option<(i64, i64, u64)> {
        self.quality_exact
            .lock()
            .ok()
            .and_then(|guard| *guard)
    }

    /// Publish the 画质 catalogue the menu rows were built from.
    pub fn set_quality_catalog(&self, specs: Vec<QualitySpec>) {
        if let Ok(mut guard) = self.quality_catalog.lock() {
            *guard = specs;
        }
    }

    /// Snapshot of the 画质 catalogue, indexed exactly like the UI rows.
    pub fn quality_catalog(&self) -> Vec<QualitySpec> {
        self.quality_catalog
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Danmaku font step (0 = 小, 1 = 标准, 2 = 大).
    pub fn danmaku_font(&self) -> u32 {
        self.danmaku_font.load(Ordering::SeqCst) as u32
    }

    pub fn set_danmaku_font(&self, step: u32) {
        self.danmaku_font.store(step.min(2) as u64, Ordering::SeqCst);
    }

    /// Danmaku scroll speed percent (`50..=200`).
    pub fn danmaku_speed(&self) -> u32 {
        self.danmaku_speed.load(Ordering::SeqCst) as u32
    }

    pub fn set_danmaku_speed(&self, percent: u32) {
        self.danmaku_speed
            .store(percent.clamp(50, 200) as u64, Ordering::SeqCst);
    }

    /// Danmaku display-area percent (`25 / 50 / 75 / 100`).
    pub fn danmaku_area(&self) -> u32 {
        self.danmaku_area.load(Ordering::SeqCst) as u32
    }

    pub fn set_danmaku_area(&self, percent: u32) {
        self.danmaku_area
            .store(crate::settings::clamp_area(percent) as u64, Ordering::SeqCst);
    }

    /// Read the three per-type block flags.
    pub fn type_blocks(&self) -> (bool, bool, bool) {
        (
            self.block_scroll.load(Ordering::SeqCst),
            self.block_top.load(Ordering::SeqCst),
            self.block_bottom.load(Ordering::SeqCst),
        )
    }

    pub fn set_type_blocks(&self, scroll: bool, top: bool, bottom: bool) {
        self.block_scroll.store(scroll, Ordering::SeqCst);
        self.block_top.store(top, Ordering::SeqCst);
        self.block_bottom.store(bottom, Ordering::SeqCst);
    }

    /// Snapshot the blocked-sender list.
    pub fn blocked_senders(&self) -> Vec<String> {
        self.blocked_senders
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn set_blocked_senders(&self, senders: Vec<String>) {
        if let Ok(mut guard) = self.blocked_senders.lock() {
            *guard = senders;
        }
    }

    /// Snapshot the compiled keyword/regex block rules.
    pub fn block_rules(&self) -> BlockRules {
        self.block_rules
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Recompile the keyword/regex block rules from the raw settings lists.
    pub fn set_block_rules(&self, keywords: &[String], patterns: &[String]) {
        let rules = BlockRules::build(keywords, patterns);
        if let Ok(mut guard) = self.block_rules.lock() {
            *guard = rules;
        }
    }

    /// Whether hardware decoding is currently requested.
    pub fn hwaccel(&self) -> bool {
        self.hwaccel.load(Ordering::SeqCst)
    }

    /// Toggle hardware decoding; a change restarts both streams.
    pub fn set_hwaccel(&self, on: bool) {
        if self.hwaccel.swap(on, Ordering::SeqCst) != on {
            self.bump_seek();
        }
    }

    /// Store the hardware-decoding state without a restart (internal fallback).
    pub fn force_software_decode(&self) {
        self.hwaccel.store(false, Ordering::SeqCst);
    }

    /// Set the paused state explicitly (used by the 睡眠定时).
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// Preferred video codec (`0` = 自动, else a DASH `codecid`).
    pub fn codec_pref(&self) -> i64 {
        self.codec_pref.load(Ordering::SeqCst) as i64
    }

    /// Change the codec preference; a change restarts both streams.
    pub fn set_codec_pref(&self, codec: i64) {
        let clamped = match codec {
            7 | 12 | 13 => codec,
            _ => 0,
        };
        if self.codec_pref.swap(clamped as u64, Ordering::SeqCst) != clamped as u64 {
            self.bump_seek();
        }
    }

    /// Whether 仅音频 mode is active.
    pub fn audio_only(&self) -> bool {
        self.audio_only.load(Ordering::SeqCst)
    }

    /// Flip 仅音频 mode and return the new value; the change restarts playback
    /// so the decode loop re-arms with (or without) a video process.
    pub fn toggle_audio_only(&self) -> bool {
        let next = !self.audio_only.load(Ordering::SeqCst);
        self.audio_only.store(next, Ordering::SeqCst);
        self.bump_seek();
        next
    }

    /// Frames decoded but not pushed to the UI.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    fn set_dropped(&self, value: u64) {
        self.dropped.store(value, Ordering::SeqCst);
    }

    // ---- start-of-playback pre-roll gate --------------------------------
    //
    // Both readers are one relaxed atomic load (plus the ring counter for the
    // progress), so the UI can poll them from its frame timer.

    /// Whether the pacer is holding the first frame and buffering.
    pub fn buffering(&self) -> bool {
        self.buffering.load(Ordering::Relaxed)
    }

    /// Progress of the pre-roll toward its target, `0.0..=1.0`.
    ///
    /// `min(ring_len / target_frames, 1.0)`, and exactly `1.0` whenever the
    /// gate is open (or was never armed).
    pub fn buffer_progress(&self) -> f32 {
        if !self.buffering() {
            return 1.0;
        }
        let target = self.buffer_target.load(Ordering::Relaxed);
        let ring = self.prof.ring_len.load(Ordering::Relaxed);
        buffer_progress_milli(ring as usize, target as usize) as f32 / 1000.0
    }

    /// Close the gate: the target has been reached, the timeout fired, or the
    /// stream ended. Publishes `1.0` implicitly (see [`Self::buffer_progress`]).
    fn end_buffering(&self) {
        self.buffering.store(false, Ordering::Relaxed);
    }

    /// Arm the gate for `target_frames` of ring occupancy.
    fn begin_buffering(&self, target_frames: usize) {
        self.buffer_target
            .store(target_frames.max(1) as u64, Ordering::Relaxed);
        self.buffering.store(true, Ordering::Relaxed);
    }

    // ---- seek buffering --------------------------------------------------
    //
    // `seek()` parks its target here; the decode loop consumes it when the
    // restarted session begins. The overlay is only armed when the target is
    // NOT in the bytes the proxy already pulled, so a buffered seek keeps the
    // current picture and no frame ever shows the loading layer.

    /// Consume the seek target waiting for a restarted decoder, if any.
    fn take_pending_seek(&self) -> Option<u64> {
        let raw = self.pending_seek_ms.swap(u64::MAX, Ordering::SeqCst);
        (raw != u64::MAX).then_some(raw)
    }

    /// Whether the prefetch proxy has NOT yet fetched the region a seek to
    /// `target_ms` needs. `None` = no proxy or no length yet, so the answer is
    /// unknown (the caller then waits for the grace instead of guessing).
    ///
    /// The proxy keeps two things: a contiguous read-ahead window directly after
    /// the current read cursor (`read_ahead_bytes`), and evenly spaced warmed
    /// seek points (4 MiB each). A target inside either is served without
    /// touching the network, so it is "buffered"; everything further forward is
    /// not. A backward seek is always treated as buffered: those bytes were
    /// already served, and the backward-seek path must keep its instant picture.
    ///
    /// The byte offsets are estimated from the reported duration and the media
    /// length (`total * ms / duration`), which is exact for CBR and close enough
    /// for VBR to decide "inside the buffer" - the reported numbers to the user
    /// are still the proxy's own measurements.
    fn seek_beyond_prefetch(&self, target_ms: u64) -> Option<bool> {
        let duration = self.duration_ms();
        if duration == 0 || target_ms >= duration {
            return None;
        }
        let from_ms = self.pending_seek_from_ms.load(Ordering::SeqCst);
        if target_ms <= from_ms {
            return Some(false);
        }
        let stats = self.prefetch_raw_stats()?;
        let total = stats.bytes_total?;
        if stats.bytes_fetched == 0 {
            return None;
        }
        let target_bytes = (total as u128)
            .saturating_mul(target_ms as u128)
            .saturating_div(duration as u128) as u64;
        let cursor_bytes = (total as u128)
            .saturating_mul(from_ms as u128)
            .saturating_div(duration as u128) as u64;
        let reach = cursor_bytes.saturating_add(stats.read_ahead_bytes);
        if target_bytes <= reach {
            return Some(false);
        }
        // A warmed seek point covers a chunk around its own offset.
        if stats.warm_points_total > 0 && stats.warm_points_done >= stats.warm_points_total {
            let points = f64::from(stats.warm_points_total);
            let frac = target_ms as f64 / duration as f64;
            let nearest = (frac * points).round() / points;
            let point_bytes = (prefetch::DEFAULT_POINT_BYTES as f64)
                .min(total as f64 / points)
                * 0.5;
            if (frac - nearest).abs() * total as f64 <= point_bytes {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Publish the buffering overlay for a seek and start its clock. `target`
    /// is the position the user picked, used only for the log line.
    fn begin_seek_buffering(&self, target_ms: u64) {
        self.seek_buffering_at
            .store(monotonic_ms(), Ordering::Relaxed);
        self.seek_buffering.store(true, Ordering::Relaxed);
        // One frame is all the pacer needs to consider the seek served; the
        // overlay is closed by the presented frame, not by ring occupancy.
        self.begin_buffering(1);
        crate::debug_log(&format!(
            "seek buffer publish target_ms={target_ms} buffering=1"
        ));
    }

    /// Whether the overlay currently on screen belongs to a seek.
    pub fn seek_buffering(&self) -> bool {
        self.seek_buffering.load(Ordering::Relaxed)
    }

    /// Close a seek's overlay after its first presented frame (idempotent).
    /// Returns the measured wait in ms when a wait was actually published.
    fn end_seek_buffering(&self) -> Option<u64> {
        if !self.seek_buffering.swap(false, Ordering::Relaxed) {
            return None;
        }
        self.end_buffering();
        let started = self.seek_buffering_at.load(Ordering::Relaxed);
        Some(monotonic_ms().saturating_sub(started))
    }

    /// The quantified line the UI shows under the loading animation while
    /// `buffering` is up. Empty once nothing is loading.
    ///
    /// Every number is measured (proxy throughput and read-ahead, declared
    /// stream bitrate); nothing is extrapolated into a fake ETA.
    pub fn buffer_detail(&self) -> String {
        if !self.buffering() {
            return String::new();
        }
        let stats = self.prefetch_stats();
        let rate = stats.as_ref().map(|s| s.bytes_per_sec).unwrap_or(0);
        let ahead = stats.as_ref().map(|s| s.read_ahead_bytes).unwrap_or(0);
        let mbps = rate as f64 / 1_000_000.0;
        let declared = self
            .play_stats()
            .map(|stats| stats.declared_bps)
            .unwrap_or(0);
        // "Needed window": the proxy bytes for the next couple of seconds at
        // the stream's declared bitrate. Only a yardstick for the percentage.
        let needed = (declared as f64 / 8.0 * SEEK_BUFFER_WINDOW_S) as u64;
        if needed > 0 && ahead > 0 {
            let pct = (ahead.saturating_mul(100) / needed).min(100);
            if pct < 100 && rate > 0 {
                return format!("加载中 {pct}% · {mbps:.1} MB/s");
            }
            if rate > 0 {
                let secs = ahead as f64 / rate as f64;
                return format!("{mbps:.1} MB/s · 已缓冲 {secs:.1}s");
            }
        }
        if rate > 0 {
            let secs = ahead as f64 / rate as f64;
            return format!("{mbps:.1} MB/s · 已缓冲 {secs:.1}s");
        }
        // No measured throughput yet: say so instead of inventing numbers.
        "加载中…".to_string()
    }

    /// Ring occupancy in frames, published by the reader after every push and
    /// by the pacer after every pop.
    fn ring_len(&self) -> usize {
        self.prof.ring_len.load(Ordering::Relaxed) as usize
    }

    // ---- 网络分析 (profiler) counters -------------------------------------
    //
    // The four `note_*` recorders below are the only profiler calls on a hot
    // path: each is one relaxed atomic operation, no lock and no allocation.

    /// Count one frame handed to the UI (pacer, once per pushed frame).
    fn note_frame_presented(&self) {
        self.prof.presented.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one frame read from the decoder's stdout and publish the ring
    /// occupancy the reader just reached (`reader` thread, once per frame).
    fn note_frame_decoded(&self, ring_len: usize) {
        self.prof.decoded.fetch_add(1, Ordering::Relaxed);
        self.prof.ring_len.store(ring_len as u64, Ordering::Relaxed);
    }

    /// Publish the ring occupancy after the pacer took a frame out of it.
    fn note_ring_len(&self, ring_len: usize) {
        self.prof.ring_len.store(ring_len as u64, Ordering::Relaxed);
    }

    /// Record how long the session's very first frame took to reach the ring.
    /// Called once per player window; later (respawned) decoders are ignored.
    fn note_first_frame(&self, latency_ms: u64) {
        if !self.prof.first_seen.swap(true, Ordering::Relaxed) {
            self.prof.first_frame_ms.store(latency_ms, Ordering::Relaxed);
        }
    }

    /// Record one detected empty-ring stall and its length so far.
    fn note_stall(&self, duration_ms: u64) {
        self.prof.stalls.fetch_add(1, Ordering::Relaxed);
        self.note_stall_longest(duration_ms);
    }

    /// Raise the longest-stall mark without counting a new stall (used when an
    /// already-counted stall finally resolves).
    fn note_stall_longest(&self, duration_ms: u64) {
        self.prof
            .longest_stall_ms
            .fetch_max(duration_ms, Ordering::Relaxed);
    }

    /// Record the stream parameters of the track a decoder was just spawned
    /// for, and reset the buffer readings that belong to the old child.
    fn note_stream(&self, track: Option<&bilibili::VideoTrack>, frame_ms: f64) {
        let frame_us = if frame_ms.is_finite() && frame_ms > 0.0 {
            (frame_ms * 1000.0).round() as u64
        } else {
            0
        };
        self.prof.frame_us.store(frame_us, Ordering::Relaxed);
        self.prof.ring_len.store(0, Ordering::Relaxed);
        self.prof
            .buffer_delta_us_per_s
            .store(0, Ordering::Relaxed);
        // A fresh decoder has dropped nothing yet.
        self.set_dropped(0);
        let media = match track {
            Some(track) => ProfMedia {
                qn: track.id,
                codecid: track.codecid.max(0) as u32,
                width: track.width,
                height: track.height,
                fps: track.frame_rate as f32,
                bps: track.bandwidth,
            },
            None => ProfMedia::default(),
        };
        if let Ok(mut guard) = self.prof.media.lock() {
            *guard = media;
        }
        self.prof.active.store(track.is_some(), Ordering::Relaxed);
    }

    /// Store the rates the 4 Hz sampler derived (thousandths of a frame/s).
    fn set_prof_rates(&self, presented_per_s: f32, decoded_per_s: f32) {
        let presented = if presented_per_s.is_finite() {
            (presented_per_s * 1000.0).max(0.0) as u64
        } else {
            0
        };
        let decoded = if decoded_per_s.is_finite() {
            (decoded_per_s * 1000.0).max(0.0) as u64
        } else {
            0
        };
        self.prof
            .fps_presented_milli
            .store(presented, Ordering::Relaxed);
        self.prof
            .fps_decoded_milli
            .store(decoded, Ordering::Relaxed);
    }

    /// Store the buffered video length and its per-second change.
    fn set_prof_buffer(&self, delta_us_per_s: f64) {
        let delta = if delta_us_per_s.is_finite() {
            delta_us_per_s.clamp(-1.0e9, 1.0e9) as i64
        } else {
            0
        };
        self.prof
            .buffer_delta_us_per_s
            .store(delta, Ordering::Relaxed);
    }

    /// Real playback/streaming metrics for the 网络分析 panel.
    ///
    /// `None` until a video track has been prepared for this player. The
    /// counters live in atomics; the only lock is the static stream metadata,
    /// held for the few instructions it takes to copy it, so this is safe to
    /// call from a UI timer.
    pub fn play_stats(&self) -> Option<Stats> {
        if !self.prof.active.load(Ordering::Relaxed) {
            return None;
        }
        let media = self
            .prof
            .media
            .lock()
            .map(|guard| *guard)
            .unwrap_or_default();
        let ring_len = self.prof.ring_len.load(Ordering::Relaxed);
        let frame_us = self.prof.frame_us.load(Ordering::Relaxed);
        // Buffered seconds = ring occupancy ÷ frames per second.
        let buffer_s = if frame_us > 0 {
            ring_len as f32 * frame_us as f32 / 1.0e6
        } else {
            0.0
        };
        let buffer_delta_per_s =
            self.prof.buffer_delta_us_per_s.load(Ordering::Relaxed) as f32 / 1.0e6;
        // Honest label: an estimate, not a measured socket rate.
        let estimated_bps = ((media.bps as f64) * (1.0 + buffer_delta_per_s as f64)).max(0.0) as u64;
        let first_frame_ms = if self.prof.first_seen.load(Ordering::Relaxed) {
            Some(self.prof.first_frame_ms.load(Ordering::Relaxed).min(u32::MAX as u64) as u32)
        } else {
            None
        };
        Some(Stats {
            fps_presented: self.prof.fps_presented_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            fps_decoded: self.prof.fps_decoded_milli.load(Ordering::Relaxed) as f32 / 1000.0,
            first_frame_ms,
            buffer_s,
            buffer_delta_per_s,
            estimated_bps,
            declared_bps: media.bps,
            position_ms: self.playback_position_ms(),
            duration_ms: self.duration_ms(),
            stalls: self.prof.stalls.load(Ordering::Relaxed).min(u32::MAX as u64) as u32,
            longest_stall_ms: self
                .prof
                .longest_stall_ms
                .load(Ordering::Relaxed)
                .min(u32::MAX as u64) as u32,
            dropped_frames: self.dropped(),
            codecid: media.codecid,
            width: media.width,
            height: media.height,
            frame_w: self.frame_width(),
            frame_h: self.frame_height(),
            declared_fps: media.fps,
            qn: media.qn,
            paused: self.is_paused(),
            audio_only: self.audio_only(),
        })
    }

    // ---- display / frame size --------------------------------------------

    /// Publish the player window's physical size (and fullscreen state) from
    /// the UI's 60 Hz watch. Returns whether it changed.
    ///
    /// Cheap enough for every tick: two compares and (on a change) two relaxed
    /// stores. Zero dimensions are ignored (no window yet, or a minimised one
    /// reports none), so a transient failure can never make the decoder pick a
    /// nonsense target.
    pub fn note_display_size(&self, w: u32, h: u32, fullscreen: bool) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        // Both swaps must run before the comparison: `a != x || b != y`
        // short-circuits, which would leave the height unpublished whenever the
        // width changed (the first publish, and every resize).
        let previous_w = self.display_w.swap(w as u64, Ordering::SeqCst);
        let previous_h = self.display_h.swap(h as u64, Ordering::SeqCst);
        let fs_changed = self.display_fullscreen.swap(fullscreen, Ordering::SeqCst) != fullscreen;
        previous_w != w as u64 || previous_h != h as u64 || fs_changed
    }

    /// The published window size in physical px and whether it is fullscreen.
    /// `0x0` until the UI's first tick; callers substitute a sane default.
    pub fn display_size(&self) -> (u32, u32, bool) {
        (
            self.display_w.load(Ordering::SeqCst).min(u32::MAX as u64) as u32,
            self.display_h.load(Ordering::SeqCst).min(u32::MAX as u64) as u32,
            self.display_fullscreen.load(Ordering::SeqCst),
        )
    }

    /// Record the frame size the running decode session produces.
    fn set_frame_size(&self, size: FrameSize) {
        self.frame_w.store(size.w as u64, Ordering::Relaxed);
        self.frame_h.store(size.h as u64, Ordering::Relaxed);
    }

    fn frame_width(&self) -> u32 {
        self.frame_w.load(Ordering::Relaxed).min(u32::MAX as u64) as u32
    }

    fn frame_height(&self) -> u32 {
        self.frame_h.load(Ordering::Relaxed).min(u32::MAX as u64) as u32
    }

    /// Copy the most recent frame pushed to the UI and the size it was decoded
    /// at. `None` before the first frame.
    pub fn snapshot_frame(&self) -> Option<(Vec<u8>, u32, u32)> {
        let guard = self.last_frame.lock().ok()?;
        if guard.w == 0 || guard.h == 0 || guard.bytes.is_empty() {
            return None;
        }
        Some((guard.bytes.clone(), guard.w, guard.h))
    }

    /// Adopt a frame the UI thread already copied into its pixel buffer (the
    /// bytes are moved, not copied again), so the screenshot path keeps one
    /// full frame without a second per-frame copy on the pacer.
    fn store_last_frame(&self, bytes: Vec<u8>, size: FrameSize) {
        if let Ok(mut guard) = self.last_frame.lock() {
            guard.bytes = bytes;
            guard.w = size.w;
            guard.h = size.h;
        }
    }

    /// Ask for (or release) the blurred video-frame thumbnail used by the
    /// panel and letterbox glass.
    ///
    /// Turning it off drops the published buffer and makes the pacer skip the
    /// downscale entirely: the only remaining cost is one relaxed atomic load
    /// per decoded frame. Turning it on publishes the next frame within
    /// [`VIDEO_GLASS_INTERVAL`].
    pub fn set_video_glass_needed(&self, on: bool) {
        if self.video_glass_needed.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if !on {
            if let Ok(mut guard) = self.video_glass.lock() {
                // Release the ~20 KB buffer: a disabled pipeline holds no image.
                *guard = Vec::new();
            }
        }
        crate::debug_log(&format!("video glass needed={on}"));
    }

    /// Whether a consumer currently asks for the thumbnail. Read once per
    /// decoded frame by the pacer: the disabled path does no work.
    fn video_glass_wanted(&self) -> bool {
        self.video_glass_needed.load(Ordering::Relaxed)
    }

    /// The current blurred frame as straight-alpha RGBA8, with its dimensions.
    ///
    /// Returns `None` until the consumer has been asked for and a frame has
    /// been published (and after [`Self::set_video_glass_needed`]`(false)`).
    /// It is a ~20 KB copy under the buffer lock, meant to be called on the UI
    /// thread once per update; build the Slint image exactly like the mini
    /// player does (`SharedPixelBuffer::<Rgba8Pixel>` + `Image::from_rgba8`).
    /// The pixels are opaque, so they are identical in premultiplied form.
    #[allow(dead_code)] // Consumed by the panel-glass wiring in ui/app.slint.
    pub fn video_glass_frame(&self) -> Option<(Vec<u8>, u32, u32)> {
        if !self.video_glass_wanted() {
            return None;
        }
        let guard = self.video_glass.lock().ok()?;
        if guard.len() != VIDEO_GLASS_BYTES {
            return None;
        }
        Some((guard.clone(), VIDEO_GLASS_W, VIDEO_GLASS_H))
    }

    /// How many thumbnails have been published so far.
    ///
    /// A consumer keeps the last value it built a Slint image from and only
    /// rebuilds when this moved, so a 10 fps pump never re-uploads the same
    /// frame twice.
    pub fn video_glass_generation(&self) -> u64 {
        self.video_glass_frames.load(Ordering::Relaxed)
    }

    /// Swap a freshly blurred thumbnail into the published slot.
    ///
    /// The caller's buffer becomes the old published one (reused next update),
    /// so only a pointer swap happens under the lock — never a pixel copy.
    fn publish_video_glass(&self, pixels: &mut Vec<u8>, compute_us: u64) {
        if let Ok(mut guard) = self.video_glass.lock() {
            std::mem::swap(&mut *guard, pixels);
        }
        self.video_glass_frames.fetch_add(1, Ordering::Relaxed);
        self.video_glass_us
            .fetch_add(compute_us, Ordering::Relaxed);
    }

    /// Volume in `0..=100` (the value before mute is applied).
    pub fn volume(&self) -> u64 {
        self.volume.load(Ordering::SeqCst)
    }

    /// PID of the audio `ffplay` this player owns (`0` when none is running).
    pub fn audio_pid(&self) -> u32 {
        self.audio_pid.load(Ordering::SeqCst).min(u64::from(u32::MAX)) as u32
    }

    /// How many gain/mute changes fell back to a `ffplay` restart.
    pub fn audio_restart_count(&self) -> u64 {
        self.audio_restarts.load(Ordering::SeqCst)
    }

    /// Publish the pid of a freshly spawned audio process.
    ///
    /// A new process means a new WASAPI session, whose gain starts at whatever
    /// `ffplay` was spawned with, so the stored volume/mute is owed to it.
    fn set_audio_pid(&self, pid: u32) {
        self.audio_pid.store(u64::from(pid), Ordering::SeqCst);
        self.audio_session_pending.store(pid != 0, Ordering::SeqCst);
        self.audio_session_try_ms.store(0, Ordering::SeqCst);
    }

    /// Apply the stored volume + mute to the live audio session.
    ///
    /// `false` when no audio process is running or its session is not up yet;
    /// the caller then falls back to the old restart-the-child path.
    fn apply_audio_session(&self) -> bool {
        let pid = self.audio_pid();
        if pid == 0 {
            return false;
        }
        let level = self.volume() as f32 / 100.0;
        if !crate::win::audio_session_volume(pid, level) {
            return false;
        }
        crate::win::audio_session_mute(pid, self.muted.load(Ordering::SeqCst))
    }

    /// Retry the owed gain/mute write, at most every
    /// [`AUDIO_SESSION_RETRY_MS`]. Called once per pacer iteration (cheap: one
    /// relaxed load while nothing is pending).
    pub fn sync_audio_session(&self) {
        if !self.audio_session_pending.load(Ordering::Relaxed) {
            return;
        }
        if self.audio_pid() == 0 {
            return;
        }
        let now = monotonic_ms();
        let last = self.audio_session_try_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < AUDIO_SESSION_RETRY_MS {
            return;
        }
        self.audio_session_try_ms.store(now, Ordering::Relaxed);
        if self.apply_audio_session() {
            self.audio_session_pending.store(false, Ordering::SeqCst);
            let readback = crate::win::audio_session_get(self.audio_pid())
                .map(|(level, mute)| format!("{level:.2}/{mute}"))
                .unwrap_or_else(|| "-".to_string());
            crate::debug_log(&format!(
                "audio-session pid={} volume={:.2} mute={} readback={readback}",
                self.audio_pid(),
                self.volume() as f32 / 100.0,
                u8::from(self.muted()),
            ));
        }
    }

    /// Set the volume (`0..=100`) through the audio session.
    ///
    /// The process keeps its fixed `-volume 100` and the gain is the session's,
    /// so this never restarts `ffplay`. Only when no session is up yet (the very
    /// first spawn, or a pause that killed the child) does it fall back to the
    /// old restart, which is then the single call that respawns.
    pub fn set_volume(&self, volume: i32) {
        let clamped = volume.clamp(0, 100) as u64;
        if self.volume.swap(clamped, Ordering::SeqCst) == clamped {
            return;
        }
        let session = self.apply_audio_session();
        if session {
            crate::debug_log(&format!(
                "audio volume value={clamped} pid={} session=1 restart=0",
                self.audio_pid()
            ));
        } else {
            crate::debug_log(&format!(
                "audio volume value={clamped} pid={} session=0 restart=1",
                self.audio_pid()
            ));
            self.audio_session_pending.store(true, Ordering::SeqCst);
            self.audio_restarts.fetch_add(1, Ordering::SeqCst);
            self.restart_audio();
        }
    }

    /// Toggle mute and return the new state; applied through the session, with
    /// the same one-call restart fallback as [`PlaybackControl::set_volume`].
    pub fn toggle_mute(&self) -> bool {
        let next = !self.muted.load(Ordering::SeqCst);
        self.set_muted(next);
        next
    }

    /// Whether audio is currently muted.
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Set the mute state through the session (restart only when it is not up).
    pub fn set_muted(&self, muted: bool) {
        if self.muted.swap(muted, Ordering::SeqCst) == muted {
            return;
        }
        let pid = self.audio_pid();
        let applied = pid != 0 && crate::win::audio_session_mute(pid, muted);
        crate::debug_log(&format!(
            "audio mute value={} pid={pid} session={} restart={}",
            u8::from(muted),
            u8::from(applied),
            u8::from(!applied),
        ));
        if !applied {
            self.audio_session_pending.store(true, Ordering::SeqCst);
            self.audio_restarts.fetch_add(1, Ordering::SeqCst);
            self.restart_audio();
        }
    }

    /// Whether the windows should stay above others.
    pub fn topmost(&self) -> bool {
        self.topmost.load(Ordering::SeqCst)
    }

    /// Flip the topmost preference and return the new value.
    pub fn toggle_topmost(&self) -> bool {
        let next = !self.topmost.load(Ordering::SeqCst);
        self.topmost.store(next, Ordering::SeqCst);
        next
    }

    /// Selected `audio_tracks[]` index.
    pub fn audio_index(&self) -> usize {
        self.audio_index.load(Ordering::SeqCst) as usize
    }

    /// Switch audio track; restarts only the audio stream at the current
    /// position so the picture never stutters.
    pub fn set_audio_index(&self, index: usize) {
        if self.audio_index.swap(index as u64, Ordering::SeqCst) != index as u64 {
            self.restart_audio();
        }
    }
}

/// Compiled keyword + regex block rules. Keywords are stored lower-cased for a
/// case-insensitive substring test; invalid patterns are dropped at build time.
#[derive(Clone, Default)]
pub struct BlockRules {
    keywords: Vec<String>,
    regexes: Vec<Regex>,
}

impl BlockRules {
    /// Compile `patterns`, ignoring those the regex engine rejects.
    pub fn build(keywords: &[String], patterns: &[String]) -> Self {
        Self {
            keywords: keywords
                .iter()
                .map(|word| word.trim().to_lowercase())
                .filter(|word| !word.is_empty())
                .collect(),
            regexes: patterns
                .iter()
                .filter_map(|pattern| Regex::new(pattern).ok())
                .collect(),
        }
    }

    /// Whether `text` is matched by any keyword (case-insensitive substring) or
    /// any regex pattern.
    fn matches(&self, text: &str) -> bool {
        if self.keywords.is_empty() && self.regexes.is_empty() {
            return false;
        }
        if !self.keywords.is_empty() {
            let lowered = text.to_lowercase();
            if self.keywords.iter().any(|word| lowered.contains(word.as_str())) {
                return true;
            }
        }
        self.regexes.iter().any(|regex| regex.is_match(text))
    }
}

/// Live danmaku display settings, rebuilt from the player controls each tick.
#[derive(Clone)]
pub struct DanmakuFilter {
    pub speed_percent: u32,
    pub area_percent: u32,
    pub block_scroll: bool,
    pub block_top: bool,
    pub block_bottom: bool,
    pub blocked_senders: Vec<String>,
    pub block_rules: BlockRules,
}

impl Default for DanmakuFilter {
    fn default() -> Self {
        Self {
            speed_percent: 100,
            area_percent: 100,
            block_scroll: false,
            block_top: false,
            block_bottom: false,
            blocked_senders: Vec::new(),
            block_rules: BlockRules::default(),
        }
    }
}

/// Danmaku for the current video. Placement is cached and recomputed only when
/// the lane-affecting settings (scroll speed, display area) change.
pub struct DanmakuTrack {
    inner: std::sync::Mutex<TrackInner>,
}

struct TrackInner {
    raw: Vec<Danmaku>,
    placed: Vec<Placed>,
    placed_speed: u32,
    placed_area: u32,
}

/// One danmaku plus its precomputed lane, keyed by index into `raw`.
struct Placed {
    index: usize,
    lane: u32,
}

impl DanmakuTrack {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(TrackInner {
                raw: Vec::new(),
                placed: Vec::new(),
                placed_speed: 0,
                placed_area: 0,
            }),
        }
    }

    /// Replace the track contents; called once per playback from the fetch
    /// thread.
    pub fn set_items(&self, items: Vec<Danmaku>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.raw = items;
            inner.placed_speed = 0;
        }
    }

    /// Visible danmaku for `position_ms`, rendered at `font_px`.
    pub fn compute(
        &self,
        position_ms: u64,
        font_px: f32,
        filter: &DanmakuFilter,
    ) -> Vec<DanmakuItem> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        let scroll_ms = scroll_ms_for(filter.speed_percent);
        if inner.placed_speed != filter.speed_percent || inner.placed_area != filter.area_percent {
            inner.placed = place_items(
                &inner.raw,
                scroll_ms,
                lanes_for_area(filter.area_percent),
            );
            inner.placed_speed = filter.speed_percent;
            inner.placed_area = filter.area_percent;
        }
        active_items(
            &inner.raw,
            &inner.placed,
            position_ms,
            font_px,
            scroll_ms,
            filter,
        )
    }
}

impl Default for DanmakuTrack {
    fn default() -> Self {
        Self::new()
    }
}

/// Time (ms) for a scrolling danmaku to fully cross the screen at `percent`
/// speed (`100` = the baseline).
fn scroll_ms_for(percent: u32) -> f32 {
    SCROLL_MS * 100.0 / percent.clamp(50, 200) as f32
}

/// How many of the `LANES` rows a display area of `percent` exposes.
fn lanes_for_area(percent: u32) -> usize {
    let lanes = LANES * percent.min(100) as usize / 100;
    lanes.clamp(1, LANES)
}

/// Map a danmaku font step (0 / 1 / 2) to screen pixels.
pub fn danmaku_font_px(step: u32) -> f32 {
    match step {
        0 => 16.0,
        1 => 20.0,
        _ => 26.0,
    }
}

/// Count danmaku per fixed-time bucket and normalise the curve to `0..=1`.
///
/// Wide videos use proportionally wider buckets so the returned value count
/// never exceeds [`DENSITY_MAX_BUCKETS`]. Returns the normalised values, the
/// peak raw count, the peak bucket index, and the bucket width used. Pure and
/// allocation-light, so it runs off the UI thread once per fetch.
pub(crate) fn compute_density(
    items: &[Danmaku],
    duration_ms: u64,
) -> (Vec<f32>, u32, usize, u64) {
    if items.is_empty() {
        return (Vec::new(), 0, 0, DENSITY_BUCKET_MS);
    }
    let span_ms = if duration_ms > 0 {
        duration_ms
    } else {
        items
            .last()
            .map(|item| item.progress_ms as u64 + DENSITY_BUCKET_MS)
            .unwrap_or(DENSITY_BUCKET_MS)
    };
    let mut bucket_ms = DENSITY_BUCKET_MS;
    let mut count = (span_ms / bucket_ms + 1) as usize;
    if count as u64 > DENSITY_MAX_BUCKETS {
        bucket_ms = (span_ms / DENSITY_MAX_BUCKETS).max(1);
        count = (span_ms / bucket_ms + 1) as usize;
    }
    let count = count.max(1);

    let mut buckets = vec![0u32; count];
    for item in items {
        let index = (item.progress_ms as u64 / bucket_ms).min((count - 1) as u64) as usize;
        buckets[index] = buckets[index].saturating_add(1);
    }

    let mut peak_count = 0u32;
    let mut peak_index = 0usize;
    for (index, value) in buckets.iter().enumerate() {
        if *value > peak_count {
            peak_count = *value;
            peak_index = index;
        }
    }
    let scale = if peak_count > 0 {
        1.0 / peak_count as f32
    } else {
        0.0
    };
    let values = buckets
        .into_iter()
        .map(|value| value as f32 * scale)
        .collect();
    (values, peak_count, peak_index, bucket_ms)
}

/// Build the danmaku-list rows (`mm:ss` + text), capped for very large videos.
pub(crate) fn build_danmaku_rows(items: &[Danmaku]) -> Vec<DanmakuRow> {
    items
        .iter()
        .take(DANMAKU_LIST_MAX)
        .map(|item| DanmakuRow {
            time_text: fmt_mmss(item.progress_ms as u64).into(),
            text: item.text.clone().into(),
            time_ms: item.progress_ms as f32,
            dmid: item.dmid.clone().into(),
            thumbed: false,
        })
        .collect()
}

/// `mm:ss` for a danmaku timestamp.
fn fmt_mmss(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// Estimated rendered width; CJK glyphs are roughly one em, ASCII ~0.55 em.
fn estimate_width(text: &str, size: f32) -> f32 {
    let mut width = 0.0f32;
    for ch in text.chars() {
        width += if ch.is_ascii() { size * 0.55 } else { size };
    }
    width.max(size)
}

/// Assign every scrolling danmaku a lane (and every top/bottom one a row).
///
/// Done once per settings change, in `progress` order, so the assignment is
/// stable across ticks: a comment never jumps lanes mid-flight. A lane is
/// reusable once the previous comment has fully entered the frame.
fn place_items(items: &[Danmaku], scroll_ms: f32, max_lanes: usize) -> Vec<Placed> {
    let mut placed = Vec::with_capacity(items.len());
    let mut lane_free = vec![0u64; max_lanes.max(1)];
    let lane_count = lane_free.len();
    let mut top_row = 0usize;
    let mut bottom_row = 0usize;

    for (index, danmaku) in items.iter().enumerate() {
        let nominal = match danmaku.fontsize {
            0..=18 => 16.0,
            19..=25 => 20.0,
            _ => 26.0,
        };
        let width = estimate_width(&danmaku.text, nominal);
        let progress = danmaku.progress_ms as u64;

        let lane = match danmaku.mode {
            1..=3 => {
                let mut chosen = None;
                for (slot, free) in lane_free.iter_mut().enumerate() {
                    if *free <= progress {
                        let enter = ((width / (VIDEO_W + width)) * scroll_ms) as u64;
                        *free = progress + enter.max(600);
                        chosen = Some(slot as u32);
                        break;
                    }
                }
                let Some(lane) = chosen else {
                    continue;
                };
                lane
            }
            5 => {
                let lane = (top_row % lane_count) as u32;
                top_row += 1;
                lane
            }
            4 => {
                let lane = (bottom_row % lane_count) as u32;
                bottom_row += 1;
                lane
            }
            _ => continue,
        };

        placed.push(Placed { index, lane });
    }

    placed
}

/// Build the visible model for `position_ms`, rendered at `font_px`. Runs on
/// the UI thread at ~30 fps and never touches the decoder.
fn active_items(
    raw: &[Danmaku],
    placed: &[Placed],
    position_ms: u64,
    font_px: f32,
    scroll_ms: f32,
    filter: &DanmakuFilter,
) -> Vec<DanmakuItem> {
    let bold = font_px >= 20.0;
    let scroll_span = scroll_ms as u64;
    let mut out = Vec::new();
    for item in placed {
        let Some(danmaku) = raw.get(item.index) else {
            continue;
        };
        match danmaku.mode {
            1..=3 if filter.block_scroll => continue,
            5 if filter.block_top => continue,
            4 if filter.block_bottom => continue,
            _ => {}
        }
        if !danmaku.mid_hash.is_empty()
            && filter
                .blocked_senders
                .iter()
                .any(|sender| sender == &danmaku.mid_hash)
        {
            continue;
        }
        if filter.block_rules.matches(&danmaku.text) {
            continue;
        }
        let progress = danmaku.progress_ms as u64;
        if position_ms < progress {
            continue;
        }
        let width = estimate_width(&danmaku.text, font_px);

        let (x, y) = match danmaku.mode {
            1..=3 => {
                if position_ms >= progress + scroll_span {
                    continue;
                }
                let elapsed = (position_ms - progress) as f32;
                // Whole-pixel x: a sub-pixel offset changes the glyph phase on
                // every tick, forcing the renderer to re-shape/re-rasterise the
                // text 30x a second.
                let x = (VIDEO_W - (VIDEO_W + width) * (elapsed / scroll_ms)).round();
                (x, item.lane as f32 * LANE_H)
            }
            5 => {
                if position_ms >= progress + FIXED_MS {
                    continue;
                }
                ((VIDEO_W - width) / 2.0, item.lane as f32 * LANE_H)
            }
            4 => {
                if position_ms >= progress + FIXED_MS {
                    continue;
                }
                let y = VIDEO_H - ((item.lane as f32) + 1.0) * LANE_H;
                ((VIDEO_W - width) / 2.0, y)
            }
            _ => continue,
        };

        // The model diff keys rows by this id. The server normally sends a
        // dmid; fall back to the raw index (stable while this track's raw list
        // is unchanged) so a comment without one still gets a unique key.
        let dmid = if danmaku.dmid.is_empty() {
            format!("@{:x}", item.index)
        } else {
            danmaku.dmid.clone()
        };

        out.push(DanmakuItem {
            x,
            y,
            w: width,
            text: danmaku.text.clone().into(),
            color: color_from_rgb(danmaku.color),
            size: font_px,
            bold,
            mid_hash: danmaku.mid_hash.clone().into(),
            dmid: dmid.into(),
        });
    }
    out
}

fn color_from_rgb(rgb: u32) -> slint::Color {
    let red = ((rgb >> 16) & 0xff) as u8;
    let green = ((rgb >> 8) & 0xff) as u8;
    let blue = (rgb & 0xff) as u8;
    slint::Color::from_rgb_u8(red, green, blue)
}

/// Start playback for one video on a dedicated background thread.
pub fn spawn(
    session: Arc<bilibili::Session>,
    video: bilibili::FeedVideo,
    window: slint::Weak<PlayerWindow>,
    mini: MiniSlot,
    control: Arc<PlaybackControl>,
    track: Arc<DanmakuTrack>,
) {
    std::thread::spawn(move || run(session, video, window, mini, control, track));
}

/// Hard cap for the foreground URL decision, from [`resolve_media_url`] entry
/// to its return. The cache path is a mutex and a few string compares, so the
/// budget is met by construction; the value is logged for QA.
const RESOLVE_BUDGET_MS: u128 = 250;
/// Hosts handed to one proxy instance (the full CDN probe also uses four).
const PREFETCH_HOSTS: usize = 4;
/// A proxy that has not pulled a single byte upstream after this long is
/// treated as broken and playback restarts against the direct URL.
const PREFETCH_WARMUP_MS: u64 = 3000;

/// Hosts of the rewritable sources, best first, with the P2P / IP:Port and
/// akamaized forms removed: the first cannot keep the signature, the second
/// needs its own token and answers 403 for a bilivideo URL.
fn proxy_hosts(primary: &str, backups: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::with_capacity(PREFETCH_HOSTS);
    for url in std::iter::once(primary).chain(backups.iter().map(String::as_str)) {
        if hosts.len() >= PREFETCH_HOSTS {
            break;
        }
        if !bilibili::is_rewritable(url) || bilibili::classify_cdn(url) == bilibili::CdnKind::Mcdn {
            continue;
        }
        let Some(host) = bilibili::url_host(url) else {
            continue;
        };
        if host.ends_with("akamaized.net") || hosts.iter().any(|seen| seen == &host) {
            continue;
        }
        hosts.push(host);
    }
    hosts
}

/// The safest URL to start on when no host has been measured: the best-ranked
/// source Bilibili itself returned, skipping PCDN forms. Falls back to the
/// original URL when every candidate is skipped.
fn best_unmeasured(
    primary: &str,
    backups: &[String],
) -> (String, String, bilibili::CdnKind) {
    let p2p_rank = bilibili::cdn_rank(bilibili::CdnKind::Mcdn);
    let mut best: Option<(&str, bilibili::CdnKind, u8)> = None;
    for url in std::iter::once(primary).chain(backups.iter().map(String::as_str)) {
        if url.is_empty() || !bilibili::is_rewritable(url) {
            continue;
        }
        let kind = bilibili::classify_cdn(url);
        let rank = bilibili::cdn_rank(kind);
        if rank >= p2p_rank {
            continue;
        }
        if bilibili::url_host(url)
            .map(|host| host.ends_with("akamaized.net"))
            .unwrap_or(true)
        {
            continue;
        }
        if best.map_or(true, |(_, _, current)| rank < current) {
            best = Some((url, kind, rank));
        }
    }
    match best {
        Some((url, kind, _)) => (
            url.to_string(),
            bilibili::url_host(url).unwrap_or_default(),
            kind,
        ),
        None => (
            primary.to_string(),
            bilibili::url_host(primary).unwrap_or_default(),
            bilibili::classify_cdn(primary),
        ),
    }
}

/// Decide which URL playback starts on **now**, with a hard
/// [`RESOLVE_BUDGET_MS`] foreground budget, plus the host list to hand the
/// proxy.
///
/// * A host the 10-minute probe cache already knows returns through
///   [`bilibili::Session::cached_choice`] without any network I/O.
/// * Otherwise the best *unmeasured* source is used immediately and the real
///   probe runs in the background. Its result lands in the process-wide host
///   cache, so the next open (or the next video) starts on the winner.
fn resolve_media_url(
    session: &Arc<bilibili::Session>,
    primary: &str,
    backups: &[String],
) -> (String, Vec<String>) {
    let started = Instant::now();
    if let Some(choice) = session.cached_choice(primary, backups) {
        let mut hosts = proxy_hosts(primary, backups);
        if !choice.host.is_empty() {
            hosts.retain(|host| host != &choice.host);
            hosts.insert(0, choice.host.clone());
            hosts.truncate(PREFETCH_HOSTS);
        }
        let ms = started.elapsed().as_millis();
        crate::debug_log(&format!(
            "cdn resolve host={} kind={} cached=true ms={ms} budget_ms={RESOLVE_BUDGET_MS} hosts={}",
            choice.host,
            choice.kind.label(),
            hosts.len()
        ));
        return (choice.url, hosts);
    }

    let (url, host, kind) = best_unmeasured(primary, backups);
    let hosts = proxy_hosts(primary, backups);
    if let Some(runtime) = crate::bilibili::shared_runtime() {
        let session = session.clone();
        let primary = primary.to_string();
        let backups = backups.to_vec();
        std::thread::spawn(move || {
            let probe_started = Instant::now();
            let choice = runtime.block_on(session.pick_fastest(&primary, &backups));
            crate::debug_log(&format!(
                "cdn probe background host={} kind={} cached={} ms={} measured={}",
                choice.host,
                choice.kind.label(),
                choice.from_cache,
                probe_started.elapsed().as_millis(),
                choice.measured.len()
            ));
        });
    }
    let ms = started.elapsed().as_millis();
    crate::debug_log(&format!(
        "cdn resolve host={host} kind={} cached=false ms={ms} budget_ms={RESOLVE_BUDGET_MS} hosts={} probe=background",
        kind.label(),
        hosts.len()
    ));
    (url, hosts)
}

/// Put a prefetch proxy in front of the initial video track. Any failure is
/// logged as `prefetch disabled reason=…` and playback simply reads the CDN URL
/// directly; nothing here can stop the player from starting.
///
/// On success the selected track's `url` is replaced with the URL the proxy
/// feeds (`chosen`). That is what makes the substitution survive every session
/// restart: `decode_loop` reads `track.url`, so a seek/pause respawn keeps
/// using the loopback URL, and after `disable_prefetch` the same field already
/// holds a direct CDN URL (the chosen one) instead of a dead loopback port.
fn start_prefetch(
    session: &Arc<bilibili::Session>,
    info: &mut bilibili::PlayInfo,
    qn: i64,
    codec_pref: i64,
    warm_points: usize,
    control: &Arc<PlaybackControl>,
) {
    let selected = (|| {
        let track = auto_track_for(info, qn, codec_pref)?;
        let index = info.video_tracks.iter().position(|candidate| {
            candidate.id == track.id
                && candidate.codecid == track.codecid
                && candidate.bandwidth == track.bandwidth
        })?;
        let primary = track
            .urls
            .first()
            .cloned()
            .unwrap_or_else(|| track.url.clone());
        let backups: Vec<String> = track.urls.iter().skip(1).cloned().collect();
        Some((index, primary, backups))
    })();
    let Some((index, primary, backups)) = selected else {
        crate::debug_log("prefetch disabled reason=no-video-track");
        return;
    };
    if primary.is_empty() {
        crate::debug_log("prefetch disabled reason=empty-media-url");
        return;
    }
    let (chosen, hosts) = resolve_media_url(session, &primary, &backups);
    let host = bilibili::url_host(&chosen).unwrap_or_default();
    // An akamaized target carries its own `hdnts` token bound to that host, so
    // the proxy must forward to it verbatim instead of rotating the bilibili
    // mirrors through it (which would answer 403).
    let hosts = if host.ends_with("akamaized.net") {
        vec![host.clone()]
    } else {
        hosts
    };
    let config = PrefetchConfig {
        hosts: hosts.clone(),
        // 设置 › 预取 › 跳转位点: read from the store at open time, never from a
        // cached copy, so changing it applies to the next player.
        warm_points,
        // QA hook (`BILI_PREFETCH_READ_AHEAD_MB=<n>`): shrink the read-ahead so
        // a seek outside it can be exercised on a short test video. Clamped by
        // `PrefetchConfig::normalized`, like every other value.
        read_ahead_bytes: std::env::var("BILI_PREFETCH_READ_AHEAD_MB")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .map(|mb| mb.saturating_mul(1 << 20))
            .unwrap_or(prefetch::DEFAULT_READ_AHEAD_BYTES),
        ..Default::default()
    };
    // QA hook: `BILI_PREFETCH_TEST=fail` exercises the fallback path without
    // touching the network.
    let force_fail = std::env::var("BILI_PREFETCH_TEST")
        .map(|value| value.trim().eq_ignore_ascii_case("fail"))
        .unwrap_or(false);
    let spawned = if force_fail {
        Err(anyhow!("forced by BILI_PREFETCH_TEST"))
    } else {
        prefetch::spawn(&chosen, config.clone())
    };
    match spawned {
        Ok(handle) => {
            let local = handle.local_url.clone();
            let count = config.hosts.len().max(1);
            let read_ahead_mib = config.read_ahead_bytes >> 20;
            crate::debug_log(&format!(
                "prefetch active url={host} local={local} hosts={count} read_ahead={read_ahead_mib} warm_points={}",
                config.warm_points
            ));
            info.video_tracks[index].url = chosen.clone();
            control.install_prefetch(handle, chosen, host);
            warmup_watchdog(control.clone());
        }
        Err(err) => {
            crate::debug_log(&format!(
                "prefetch disabled reason={err} url={host} local=- hosts=0 read_ahead=0"
            ));
        }
    }
}

/// Watch the proxy's upstream byte counter while ffmpeg connects. The proxy
/// fetches ahead on its own, so a byte counter that is still zero after
/// [`PREFETCH_WARMUP_MS`] means the proxy is not going to feed anything; it is
/// shut down and the decode session restarts against the direct URL.
fn warmup_watchdog(control: Arc<PlaybackControl>) {
    std::thread::spawn(move || {
        let started = Instant::now();
        loop {
            if control.is_closed() || !control.prefetch_active() {
                return;
            }
            let Some(stats) = control.prefetch_raw_stats() else {
                return;
            };
            if stats.bytes_fetched > 0 {
                crate::debug_log(&format!(
                    "prefetch warm first_byte_ms={} bytes={} read_ahead={}",
                    started.elapsed().as_millis(),
                    stats.bytes_fetched,
                    stats.read_ahead_bytes
                ));
                return;
            }
            if started.elapsed() >= Duration::from_millis(PREFETCH_WARMUP_MS) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        crate::debug_log(&format!(
            "prefetch unhealthy bytes_fetched=0 after_ms={}",
            started.elapsed().as_millis()
        ));
        if control.disable_prefetch("no bytes within 3000ms") {
            // Force the current decode session to restart; the proxy is gone,
            // so the next spawn reads the CDN URL directly.
            control.seek(control.playback_position_ms());
        }
    });
}

fn run(
    session: Arc<bilibili::Session>,
    video: bilibili::FeedVideo,
    window: slint::Weak<PlayerWindow>,
    mini: MiniSlot,
    control: Arc<PlaybackControl>,
    track: Arc<DanmakuTrack>,
) {
    let Some(runtime) = crate::bilibili::shared_runtime() else {
        set_status(&window, "初始化播放运行时失败", crate::TOAST_KIND_ERROR);
        set_playing(&window, false);
        return;
    };

    let mut info = match runtime.block_on(session.playurl(&video.bvid, video.cid)) {
        Ok(info) => info,
        Err(err) => {
            crate::debug_log(&format!(
                "playurl failed bvid={} cid={} err={err}",
                video.bvid, video.cid
            ));
            set_status(&window, &format!("播放失败：{err}"), crate::TOAST_KIND_ERROR);
            set_playing(&window, false);
            return;
        }
    };

    control.set_duration_ms(info.duration.saturating_mul(1000));
    set_status(&window, "", crate::TOAST_KIND_INFO);

    let prefs = crate::settings::load();
    let preferred = prefs.player_quality;
    let initial_qn = if preferred != 0
        && info.video_tracks.iter().any(|track| track.id == preferred)
    {
        preferred
    } else {
        info.default_qn
    };
    control.set_quality_initial(initial_qn);
    // The stream the first decode session will actually open, so the menu can
    // mark the current row before that session has logged its `stream` line.
    // Same rule as the decoder (highest bandwidth at the `qn`).
    let initial_stream = auto_track_for(&info, initial_qn, prefs.codec_pref as i64)
        .map(|track| (track.id, track.codecid, track.bandwidth))
        .unwrap_or((initial_qn, 0, 0));
    set_qualities(&window, &control, &info, initial_stream);
    crate::debug_log(&format!(
        "tracks bvid={} n={} list={}",
        video.bvid,
        info.video_tracks.len(),
        info.video_tracks
            .iter()
            .map(|track| format!("{}/{}", track.id, track.codecid))
            .collect::<Vec<_>>()
            .join(",")
    ));
    set_audio_tracks(&window, &info.audio_tracks, 0);
    control.set_audio_index(0);
    // The proxy is opened for the track the first decode session will use. A
    // later quality switch selects a different track, and `media_url_for` then
    // falls back to that track's direct URL instead of serving wrong bytes
    // through this proxy.
    start_prefetch(
        &session,
        &mut info,
        initial_qn,
        prefs.codec_pref as i64,
        prefs.seek_points as usize,
        &control,
    );
    // Danmaku is fetched on its own thread: decoding never waits on the network.
    {
        let track = track.clone();
        let window = window.clone();
        let cid = video.cid;
        let duration = info.duration;
        std::thread::spawn(move || {
            let Some(runtime) = crate::bilibili::shared_runtime() else {
                return;
            };
            let list = runtime.block_on(session.danmaku_all(cid, duration));
            let fetched = list.len();
            let (density, peak_count, peak_index, bucket_ms) =
                compute_density(&list, duration.saturating_mul(1000));
            crate::debug_log(&format!(
                "danmaku fetched={fetched} buckets={} peak_count={peak_count} peak_index={peak_index} bucket_ms={bucket_ms}",
                density.len()
            ));
            let rows = build_danmaku_rows(&list);
            track.set_items(list);
            let window = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window.upgrade() {
                    window.set_density_buckets(ModelRc::from(std::rc::Rc::new(VecModel::from(
                        density,
                    ))));
                    window.set_danmaku_list(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
                    window.set_danmaku_list_count(fetched as i32);
                }
            });
        });
    }

    if std::env::var("BILI_HWACCEL").map(|value| value == "0").unwrap_or(false) {
        control.force_software_decode();
    }
    let mut fell_back = false;
    let mut last_resync: Option<Instant> = None;
    loop {
        match decode_loop(&info, video.cid, &window, &control, &mini, &mut last_resync) {
            Ok(()) => {
                // A close (or a supersede) also returns `Ok`, but it is not a
                // finished track: only a live session reports 播放结束, so a
                // closing window cannot toast into the next player.
                if !control.is_closed() {
                    set_status(&window, "播放结束", crate::TOAST_KIND_INFO);
                }
                break;
            }
            Err(err) => {
                // A proxy failure must never be the reason a player stays
                // black: drop it and retry the same position directly. The
                // retry is bounded because `disable_prefetch` removes it.
                if control.prefetch_active() {
                    crate::debug_log(&format!(
                        "decode failed via prefetch err={err}; retrying direct"
                    ));
                    set_status(
                        &window,
                        "预取代理异常，已改用直连",
                        crate::TOAST_KIND_INFO,
                    );
                    control.disable_prefetch(&format!("decode failed err={err}"));
                    continue;
                }
                if control.hwaccel() && !fell_back {
                    fell_back = true;
                    crate::debug_log(&format!(
                        "hwaccel failed err={err}; falling back to software decode"
                    ));
                    set_status(
                        &window,
                        "硬件解码不可用，已回退软件解码",
                        crate::TOAST_KIND_INFO,
                    );
                    control.force_software_decode();
                    continue;
                }
                if !control.is_closed() {
                    set_status(
                        &window,
                        &format!("播放中断：{err}"),
                        crate::TOAST_KIND_ERROR,
                    );
                }
                break;
            }
        }
    }
    control.disable_prefetch("playback ended");
    set_playing(&window, false);
}

/// 4 Hz sampler that turns the cumulative [`Prof`] counters into the rates and
/// deltas exposed by [`Stats`], and emits the optional `BILI_PROFILER_LOG=1`
/// line once per second.
///
/// It lives on the decode thread and receives the `Instant` the pacer already
/// holds, so the frames between ticks cost one comparison and nothing else.
/// The window holds `(monotonic ms, presented, decoded, buffered µs)` samples.
struct ProfilerTick {
    last_tick: Option<Instant>,
    window: VecDeque<(u64, u64, u64, u64)>,
    /// `BILI_PROFILER_LOG=1` was set for this process.
    log: bool,
    last_log_ms: u64,
}

impl ProfilerTick {
    fn new(control: &PlaybackControl) -> Self {
        // A new decode session has no measured rate yet; do not let the panel
        // keep showing the previous session's numbers.
        control.set_prof_rates(0.0, 0.0);
        Self {
            last_tick: None,
            window: VecDeque::with_capacity(PROF_WINDOW),
            log: std::env::var("BILI_PROFILER_LOG")
                .map(|value| value == "1")
                .unwrap_or(false),
            last_log_ms: 0,
        }
    }

    /// Sample the counters and refresh [`Stats`]' derived values.
    fn tick(&mut self, control: &PlaybackControl, now: Instant) {
        if let Some(last) = self.last_tick {
            if now.duration_since(last) < Duration::from_millis(PROF_TICK_MS) {
                return;
            }
        }
        self.last_tick = Some(now);
        let ms = monotonic_ms();
        let prof = &control.prof;
        let presented = prof.presented.load(Ordering::Relaxed);
        let decoded = prof.decoded.load(Ordering::Relaxed);
        // Buffered video = ring occupancy × frame period, both real counters.
        let buffer_us = prof
            .ring_len
            .load(Ordering::Relaxed)
            .saturating_mul(prof.frame_us.load(Ordering::Relaxed));
        self.window.push_back((ms, presented, decoded, buffer_us));
        while self.window.len() > PROF_WINDOW {
            self.window.pop_front();
        }
        if let (Some(old), Some(new)) = (self.window.front().copied(), self.window.back().copied()) {
            let span = new.0.saturating_sub(old.0);
            if span >= PROF_MIN_WINDOW_MS {
                let per_s = 1000.0 / span as f32;
                control.set_prof_rates(
                    new.1.saturating_sub(old.1) as f32 * per_s,
                    new.2.saturating_sub(old.2) as f32 * per_s,
                );
                // Growth of the buffered length, µs of video per second: the
                // span is in ms, so one second is `span * 1000` µs-units... i.e.
                // multiply by 1000 to divide by seconds.
                control.set_prof_buffer((new.3 as f64 - old.3 as f64) * 1000.0 / span as f64);
            }
        }
        if self.log && ms.saturating_sub(self.last_log_ms) >= 1000 {
            self.last_log_ms = ms;
            if let Some(stats) = control.play_stats() {
                // `first_ms=-1` means "no frame has entered the ring yet".
                let first_ms = stats.first_frame_ms.map_or(-1, i64::from);
                crate::debug_log(&format!(
                    "profiler fps_presented={:.1} fps_decoded={:.1} first_ms={} buffer_s={:.2} dbuf={:.2} est_bps={} decl_bps={} qn={} codec={} frame={}x{} stalls={} stall_max_ms={} pos_ms={} dur_ms={}",
                    stats.fps_presented,
                    stats.fps_decoded,
                    first_ms,
                    stats.buffer_s,
                    stats.buffer_delta_per_s,
                    stats.estimated_bps,
                    stats.declared_bps,
                    stats.qn,
                    stats.codecid,
                    stats.frame_w,
                    stats.frame_h,
                    stats.stalls,
                    stats.longest_stall_ms,
                    stats.position_ms,
                    stats.duration_ms,
                ));
            }
        }
    }
}

/// Buffered fraction toward `target_frames`, in thousandths (`0..=1000`).
/// `target_frames` is at least the ring's one frame, so this cannot divide by
/// zero; a ring already past the target reports full.
fn buffer_progress_milli(ring_len: usize, target_frames: usize) -> u64 {
    let target = target_frames.max(1) as u64;
    ((ring_len as u64).min(target) * 1000 / target).min(1000)
}

/// Why a playback session holds before presenting frames.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GateKind {
    /// First session of this decode loop: wait for the full pre-roll target.
    Initial,
    /// Respawn after a watchdog-confirmed empty-ring stall: wait for the ring
    /// to fill again, with a shorter ceiling.
    Recovery,
}

impl GateKind {
    /// Frames to wait for: `ceil(PRE_ROLL_TARGET_MS × fps / 1000)`, clamped to
    /// the ring capacity (the pre-roll can never exceed what the ring holds for
    /// this session's frame size).
    fn target_frames(self, frame_ms: f64, capacity: usize) -> usize {
        let capacity = capacity.max(1);
        let requested = match self {
            GateKind::Initial if frame_ms.is_finite() && frame_ms > 0.0 => {
                (PRE_ROLL_TARGET_MS as f64 / frame_ms).ceil()
            }
            // 30 fps is the fallback period used elsewhere in this file.
            GateKind::Initial => (PRE_ROLL_TARGET_MS / 33) as f64,
            GateKind::Recovery => capacity as f64,
        };
        (requested.max(1.0) as usize).min(capacity)
    }

    /// How long to wait for that target before starting anyway.
    fn timeout_ms(self) -> u64 {
        match self {
            GateKind::Initial => PRE_ROLL_TIMEOUT_MS,
            GateKind::Recovery => PRE_ROLL_RECOVERY_TIMEOUT_MS,
        }
    }
}

/// Outcome of one gate poll.
enum GateStep {
    /// Keep holding: the ring is still short of the target.
    Wait,
    /// Release the pacer (target reached, timeout, or end of stream).
    Open,
    /// Not a single frame arrived within the startup watchdog; treat it as a
    /// hung start and resync instead of holding forever.
    Stall,
}

/// A user seek that has not presented its first frame yet.
///
/// `published` is the honest gate: the overlay is on screen only when the seek
/// was known to be outside the proxy's fetched data (`begin_seek_buffering`),
/// or when the grace below expired without a new picture. A seek served from
/// the read-ahead therefore never shows the loading layer at all.
struct SeekWait {
    /// Position the user picked, ms (log line only).
    target: u64,
    /// The overlay has been armed for this seek.
    published: bool,
    /// The proxy already has the target region: the old picture stays on screen
    /// and the overlay is never published, however long ffmpeg takes to seek.
    buffered: bool,
    /// When an unknown-bufferedness seek is declared slow.
    deadline: Instant,
}

/// Arm the seek overlay once an unknown-bufferedness seek has waited too long.
fn arm_seek_overlay(control: &PlaybackControl, wait: &mut SeekWait) {
    if wait.published || wait.buffered || Instant::now() < wait.deadline {
        return;
    }
    wait.published = true;
    control.begin_seek_buffering(wait.target);
    crate::debug_log(&format!(
        "seek buffer armed target_ms={} reason=grace_ms={}",
        wait.target,
        SEEK_BUFFER_GRACE.as_millis()
    ));
}

/// One armed pre-roll gate. It lives on the pacer thread; the UI only sees the
/// atomics it publishes through [`PlaybackControl`].
struct PreRollGate {
    kind: GateKind,
    /// Ring occupancy that opens the gate, frames.
    target: usize,
    timeout: Duration,
    /// When the gate was armed (the session's first decode).
    armed_at: Instant,
    /// When the first decoded frame reached the ring. The timeout runs from
    /// here, so a slow connect is not counted against the pre-roll.
    first_frame_at: Option<Instant>,
    /// `BILI_PREROLL_LOG=1` was set for this process.
    log: bool,
}

impl PreRollGate {
    /// Arm the gate and publish `buffering()` for the UI.
    fn arm(control: &PlaybackControl, kind: GateKind, frame_ms: f64, capacity: usize) -> Self {
        let gate = Self {
            kind,
            target: kind.target_frames(frame_ms, capacity),
            timeout: Duration::from_millis(kind.timeout_ms()),
            armed_at: Instant::now(),
            first_frame_at: None,
            log: std::env::var("BILI_PREROLL_LOG")
                .map(|value| value == "1")
                .unwrap_or(false),
        };
        control.begin_buffering(gate.target);
        if gate.log {
            crate::debug_log(&format!(
                "preroll start target={} ring={} t_ms=0 kind={}",
                gate.target,
                control.ring_len(),
                match gate.kind {
                    GateKind::Initial => "initial",
                    GateKind::Recovery => "recovery",
                }
            ));
        }
        gate
    }

    /// One poll. Reads the published ring occupancy only - nothing is popped,
    /// so the reader keeps filling the ring while the gate is closed.
    fn poll(&mut self, control: &PlaybackControl, source: &FrameSource) -> GateStep {
        let ring = control.ring_len();
        if ring > 0 && self.first_frame_at.is_none() {
            self.first_frame_at = Some(Instant::now());
        }
        if source.is_done() {
            // Clean end of stream or a read error: fail through at once rather
            // than wait for a target that can never be reached.
            return GateStep::Open;
        }
        let Some(first) = self.first_frame_at else {
            // No frame at all yet, so the pre-roll timeout does not run; bound
            // the wait with the startup watchdog so a hung decoder still
            // resyncs instead of holding forever.
            if self.armed_at.elapsed() >= Duration::from_millis(STALL_STARTUP_MS) {
                return GateStep::Stall;
            }
            return GateStep::Wait;
        };
        if ring >= self.target {
            return GateStep::Open;
        }
        if first.elapsed() >= self.timeout {
            return GateStep::Open;
        }
        GateStep::Wait
    }
}


/// Watches the published window size and decides when the picture is worth
/// re-decoding.
///
/// A drag changes the window dozens of times a second; restarting the decoder
/// for each step would stutter and thrash the network. The rule is:
///
/// * the new target must differ from the session's target by more than 15% on
///   an axis (smaller differences are not visible — the Slint Image scales the
///   frame to the band anyway),
/// * that target must then stay unchanged for [`RESIZE_DEBOUNCE`] (400 ms).
///
/// Only then does the pacer end the session; the outer loop respawns at the
/// current position through the normal restart path.
struct ResizeWatch {
    /// Frame size the running session was spawned for.
    applied: FrameSize,
    /// Candidate size and the instant it was first seen.
    candidate: Option<(FrameSize, Instant)>,
}

impl ResizeWatch {
    fn new(applied: FrameSize) -> Self {
        Self {
            applied,
            candidate: None,
        }
    }

    /// `Some(target)` when the decoder should restart at that size.
    fn poll(
        &mut self,
        source: (u32, u32),
        display: (u32, u32),
        now: Instant,
    ) -> Option<FrameSize> {
        let target = target_frame_size(source.0, source.1, display.0, display.1);
        if !size_changed_above_ratio(target, self.applied) {
            // Back within tolerance of what is on screen: no restart is due.
            self.candidate = None;
            return None;
        }
        match self.candidate {
            Some((candidate, since)) if candidate == target => {
                if now.duration_since(since) >= RESIZE_DEBOUNCE {
                    self.applied = target;
                    self.candidate = None;
                    Some(target)
                } else {
                    None
                }
            }
            // First sighting (or the size moved again during the debounce):
            // restart the 400 ms window.
            _ => {
                self.candidate = Some((target, now));
                None
            }
        }
    }
}

/// Decode-and-pace loop. Pausing kills both children; resuming respawns them
/// at the tracked position (`-ss`). Video frames are paced against the audio
/// clock parsed from ffplay's `-stats` line (the wall clock is only a fallback
/// while no audio clock is available); in 仅音频 mode no video process is spawned.
fn decode_loop(
    info: &bilibili::PlayInfo,
    cid: u64,
    window: &slint::Weak<PlayerWindow>,
    control: &Arc<PlaybackControl>,
    mini: &MiniSlot,
    last_resync: &mut Option<Instant>,
) -> Result<()> {
    ensure_high_res_timer();
    // Arming policy across session restarts: the first session of this decode
    // loop is pre-rolled, and so is one restarted by the empty-ring watchdog.
    // Seek, pause, quality and speed restarts are deliberately not gated - a
    // seek has to feel immediate.
    let mut gate_kind: Option<GateKind> = Some(GateKind::Initial);
    // Why the next session starts: `spawn` for the first one of this decode
    // loop, `resize` when the display-size watch asked for a re-decode, and
    // `restart` for a seek / pause / quality / speed / volume restart. Logged
    // on the `video target` line.
    let mut spawn_reason = "spawn";
    // Thumbnail buffers survive session restarts, so only the first update of a
    // player allocates.
    let mut glass = VideoGlassWorker::new();
    loop {
        if control.is_closed() {
            return Ok(());
        }
        if control.is_paused() {
            std::thread::sleep(Duration::from_millis(40));
            continue;
        }

        let start_ms = control.position_ms();
        let seen_seek = control.seek_seq();
        // A user seek consumed here restarted this session. Decide now whether
        // its target is outside the proxy's fetched bytes: only then is the
        // overlay published from the seek itself. Otherwise the picture stays
        // and the grace in `arm_seek_overlay` decides.
        let mut seek_wait = control.take_pending_seek().map(|target| {
            let mut wait = SeekWait {
                target,
                published: false,
                buffered: false,
                deadline: Instant::now() + SEEK_BUFFER_GRACE,
            };
            match control.seek_beyond_prefetch(target) {
                Some(true) => {
                    wait.published = true;
                    control.begin_seek_buffering(target);
                    let stats = control.prefetch_raw_stats();
                    crate::debug_log(&format!(
                        "seek buffer armed target_ms={target} reason=unfetched fetched={} total={}",
                        stats.as_ref().map(|s| s.bytes_fetched).unwrap_or(0),
                        stats
                            .as_ref()
                            .and_then(|s| s.bytes_total)
                            .map(|total| total.to_string())
                            .unwrap_or_else(|| "-".to_string())
                    ));
                }
                Some(false) => {
                    // Known buffered: no overlay, no grace - just keep the
                    // current picture until the new position's first frame.
                    wait.buffered = true;
                    crate::debug_log(&format!(
                        "seek buffer skipped target_ms={target} reason=prefetched overlay=0"
                    ));
                }
                None => crate::debug_log(&format!(
                    "seek buffer pending target_ms={target} reason=unknown-bufferedness"
                )),
            }
            wait
        });
        let speed = control.speed();
        let hwaccel = control.hwaccel();
        let audio_only = control.audio_only();
        // A 画质 menu pick names one exact stream; anything else (a `qn`-only
        // caller, the codec preference, a new video) uses the automatic rule.
        let exact = control
            .exact_quality()
            .and_then(|(qn, codecid, bandwidth)| {
                info.video_tracks.iter().find(|candidate| {
                    candidate.id == qn
                        && candidate.codecid == codecid
                        && candidate.bandwidth == bandwidth
                })
            });
        let pick = if exact.is_some() { "exact" } else { "auto" };
        let track = exact.or_else(|| auto_track_for(info, control.quality(), control.codec_pref()));
        let video_url = match track {
            Some(track) => control.media_url_for(&track.url),
            None => return Err(anyhow!("未找到可用视频流")),
        };
        let via = if video_url.starts_with("http://127.0.0.1:") {
            "prefetch"
        } else {
            "direct"
        };

        publish_info(window, track, control, cid);
        if let Some(track) = track {
            // `bws_at_qn` proves the automatic rule picked the highest-bitrate
            // stream: every candidate at that qn, `codecid/bandwidth`, and the
            // chosen `bandwidth` is the maximum among them.
            crate::debug_log(&format!(
                "stream qn={} codecid={} res={}x{} bandwidth={} fps={:.3} audio_only={audio_only} via={via} pick={pick} rule={} bws_at_qn=[{}]",
                track.id,
                track.codecid,
                track.width,
                track.height,
                track.bandwidth,
                track.frame_rate,
                if exact.is_some() {
                    "exact"
                } else {
                    "highest-bandwidth"
                },
                bandwidths_at_qn(info, track.id),
            ));
        }

        // Milliseconds per decoded frame, used as this frame's presentation
        // timestamp (`frame_index * frame_ms`).
        let frame_ms = track
            .map(|track| track.frame_rate)
            .filter(|rate| *rate > 0.0 && *rate <= 240.0)
            .map(|rate| 1000.0 / rate)
            .unwrap_or(1000.0 / 30.0);
        // Publish the declared stream parameters (codec, resolution, declared
        // fps/bitrate, qn) and clear the buffer readings of the old child.
        control.note_stream(track, frame_ms);

        // ---- decode at the display size, not at 540p ---------------------
        // The picture is drawn in the player window (fullscreen: the whole
        // monitor), so that is the resolution worth decoding. Capped at the
        // source and at 1920x1080, aspect preserved (see `target_frame_size`).
        let source_size = track.map(|track| (track.width, track.height)).unwrap_or((0, 0));
        let (published_w, published_h, fullscreen) = control.display_size();
        let (display_w, display_h) = if published_w == 0 || published_h == 0 {
            (DEFAULT_DISPLAY_W, DEFAULT_DISPLAY_H)
        } else {
            (published_w, published_h)
        };
        let target = target_frame_size(source_size.0, source_size.1, display_w, display_h);
        let frame_bytes = target.bytes();
        let capacity = ring_capacity_for(frame_bytes);
        control.set_frame_size(target);
        crate::debug_log(&format!(
            "video target w={} h={} reason={spawn_reason} capacity={capacity} source={}x{} window={}x{} fullscreen={fullscreen} frame_bytes={frame_bytes} budget_mb={}",
            target.w,
            target.h,
            source_size.0,
            source_size.1,
            display_w,
            display_h,
            RING_BUDGET_BYTES >> 20,
        ));
        // Only the first session of this decode loop is a `spawn`; every later
        // one names what caused it (`resize`, or a plain restart = seek/pause/
        // quality/speed/volume).
        spawn_reason = "restart";

        // Pre-roll is a picture gate: 仅音频 has no frames to hold back.
        let pre_roll = if audio_only { None } else { gate_kind };

        // The profiler measures spawn→first-frame latency from here.
        let spawn_ms = monotonic_ms();
        let mut video_child = if audio_only {
            None
        } else {
            Some(
                spawn_video(&video_url, start_ms, hwaccel, target)
                    .map_err(|err| anyhow!("无法启动解码器：{err}"))?,
            )
        };
        let audio_url = info.audio_url_for(control.audio_index());
        // Snapshot the audio-restart trigger *before* this session starts its
        // audio process: a request that lands during the spawn is then still
        // seen by the loop below instead of being swallowed.
        let mut seen_audio = control.audio_seq();
        // While a pre-roll gate holds the picture, audio starts when the gate
        // opens (at the same position). Otherwise the audio clock would run
        // ahead of the held frame and the pacer would drop the buffered frames
        // to catch up the moment the gate opens, defeating the pre-roll.
        let mut audio_child = if pre_roll.is_some() {
            None
        } else {
            audio_url.as_deref().and_then(|url| {
                start_audio(url, start_ms, speed, control)
            })
        };
        let has_audio = audio_child.is_some();

        let stderr_reader = video_child
            .as_mut()
            .and_then(|child| child.stderr.take())
            .map(|mut pipe| {
                std::thread::spawn(move || {
                    let mut text = String::new();
                    let _ = pipe.read_to_string(&mut text);
                    text.trim().to_string()
                })
            });

        let stdout = if audio_only {
            None
        } else {
            match video_child.as_mut().and_then(|child| child.stdout.take()) {
                Some(stdout) => Some(stdout),
                None => {
                    if let Some(child) = video_child.as_mut() {
                        kill(child);
                    }
                    if let Some(proc) = audio_child.take() {
                        proc.stop();
                        // The pid is dead now: a gain write must not target it.
                        control.set_audio_pid(0);
                    }
                    return Err(anyhow!("解码器未提供输出流"));
                }
            }
        };
        // Hand the pipe to a reader thread; the pacer drains the ring instead of
        // reading the pipe itself (so it can time out on a stalled source).
        let mut source = stdout.map(|stdout| {
            FrameSource::spawn(stdout, control.clone(), spawn_ms, frame_bytes, capacity)
        });

        let mut frames: u64 = 0;
        let mut pushes: u64 = 0;
        let mut played = Duration::from_millis(start_ms);
        let mut last = Instant::now();
        let push_interval = push_interval_for(frame_ms);
        let mut last_push = last.checked_sub(push_interval).unwrap_or(last);
        let mut last_info = last;
        let mut paused = false;
        let mut seeking = false;
        let mut mode_changed = false;
        let mut finished = false;
        // When this session's first frame may still be awaited before the A/V
        // guard is allowed to act (see [`RESYNC_START_GRACE_MS`]).
        let resync_grace_until = Instant::now() + Duration::from_millis(RESYNC_START_GRACE_MS);
        // When the ring first went empty while the pacer waited for a frame;
        // `None` while frames are flowing. Drives the stall watchdog.
        let mut empty_since: Option<Instant> = None;
        // 4 Hz profiler sampler for this decode session (rates, buffer delta,
        // and the 1 Hz `BILI_PROFILER_LOG` line).
        let mut profiler = ProfilerTick::new(control);
        // Start of the empty-ring period once the watchdog confirmed it is a
        // real stall; the full length is reported when it resolves.
        let mut stall_open: Option<Instant> = None;
        // This session ended because the empty-ring watchdog confirmed a stall
        // (as opposed to a seek / pause): the next one must re-enter buffering.
        let mut stall_resync = false;
        // Watches the window size for the whole session: a resize that changes
        // the display target by more than 15% restarts the decoder once it has
        // been still for 400 ms (see [`ResizeWatch`]).
        let mut resize = ResizeWatch::new(target);

        match source.as_ref() {
            Some(source) => {
                // Armed before the first frame: the pacer polls it at the top
                // of every iteration and pops nothing until it opens.
                let mut gate =
                    pre_roll.map(|kind| PreRollGate::arm(control, kind, frame_ms, capacity));
                loop {
                if control.is_closed() {
                    break;
                }
                // Owed gain/mute after a fresh spawn: one cheap load per frame
                // while nothing is pending, a bounded retry (100ms) until the
                // new process' audio session exists.
                control.sync_audio_session();
                if control.is_paused() {
                    paused = true;
                    // No frames are presented while paused; do not leave the
                    // panel showing the rate measured before the pause.
                    control.set_prof_rates(0.0, 0.0);
                    break;
                }
                if control.seek_seq() != seen_seek {
                    seeking = true;
                    break;
                }
                if control.audio_only() {
                    mode_changed = true;
                    break;
                }

                // Audio restarts wait for the gate: starting ffplay during the
                // pre-roll would let its clock run ahead of the held frame.
                if gate.is_none() && control.audio_seq() != seen_audio {
                    seen_audio = control.audio_seq();
                    if let Some(proc) = audio_child.take() {
                        proc.stop();
                        // The pid is dead now: a gain write must not target it.
                        control.set_audio_pid(0);
                    }
                    let resume_ms = control
                        .audio_clock()
                        .unwrap_or_else(|| played.as_millis() as u64);
                    let audio_url = info.audio_url_for(control.audio_index());
                    audio_child = audio_url.as_deref().and_then(|url| {
                        start_audio(url, resume_ms, speed, control)
                    });
                    crate::debug_log(&format!(
                        "audio restart index={} ms={resume_ms}",
                        control.audio_index()
                    ));
                }

                // ---- start-of-playback pre-roll -------------------------
                // Hold at the first available frame: while the gate is closed
                // the pacer pops nothing, so the reader fills the ring, no
                // frame reaches the UI, and `played` is not advanced (the
                // position therefore holds at `start_ms`).
                if let Some(armed) = gate.as_mut() {
                    match armed.poll(control, source) {
                        GateStep::Wait => {
                            profiler.tick(control, Instant::now());
                            std::thread::sleep(Duration::from_millis(PRE_ROLL_POLL_MS));
                            continue;
                        }
                        GateStep::Stall => {
                            let waited = armed.armed_at.elapsed().as_millis() as u64;
                            if armed.log {
                                crate::debug_log(&format!(
                                    "preroll stall empty_ms={waited} target={} kind=start",
                                    armed.target
                                ));
                            }
                            control.note_stall(waited);
                            control.end_buffering();
                            gate = None;
                            // No frame ever arrived: reuse the startup-stall
                            // resync instead of holding forever.
                            stall_resync = true;
                            *last_resync = Some(Instant::now());
                            control.invalidate_audio_clock();
                            seeking = true;
                            break;
                        }
                        GateStep::Open => {
                            let ring = control.ring_len();
                            let elapsed = armed
                                .first_frame_at
                                .map_or(0, |first| first.elapsed().as_millis() as u64);
                            let progress = control.buffer_progress();
                            if armed.log {
                                let line = match armed.kind {
                                    GateKind::Recovery => "preroll resume",
                                    GateKind::Initial => "preroll open",
                                };
                                crate::debug_log(&format!(
                                    "{line} ring={ring} elapsed_ms={elapsed} progress={progress:.2} target={} presented={}",
                                    armed.target,
                                    control.prof.presented.load(Ordering::Relaxed)
                                ));
                            }
                            // The picture starts moving now: restart the wall
                            // clock so the held position does not jump by the
                            // buffering time, and bring audio up at the same
                            // position so A/V begins together.
                            last = Instant::now();
                            if audio_child.is_none() {
                                // Re-read the URL: the audio track or volume may
                                // have changed while the gate held.
                                let start_url = info.audio_url_for(control.audio_index());
                                let requested_before = control.audio_seq();
                                audio_child = start_url.as_deref().and_then(|url| {
                                    start_audio(url, start_ms, speed, control)
                                });
                                // Consume only the requests made before this
                                // spawn; a bump that lands while the process
                                // starts must still restart audio below.
                                if control.audio_seq() == requested_before {
                                    seen_audio = requested_before;
                                }
                            }
                            control.end_buffering();
                            gate = None;
                        }
                    }
                }

                let frame = match source.next_frame(Duration::from_millis(STALL_POLL_MS)) {
                    Ok(Some(frame)) => {
                        empty_since = None;
                        // A confirmed stall just resolved: keep the length of
                        // the whole empty period (it was counted on detection).
                        if let Some(began) = stall_open.take() {
                            control.note_stall_longest(began.elapsed().as_millis() as u64);
                        }
                        frame
                    }
                    Ok(None) => {
                        // The video decoder reported the stream finished: this
                        // is the real end of the track (or a mid-file read
                        // error, which the position guard filters out). The
                        // queue's 250ms tick watches this flag.
                        control.note_stream_finished();
                        finished = true;
                        break;
                    }
                    Err(()) => {
                        // Ring still empty after the poll. If it stays empty the
                        // source has silently stalled (TCP alive, no frames) and
                        // the pacing check below would never run - so detect it
                        // here and reuse the F29 resync.
                        let since = *empty_since.get_or_insert_with(Instant::now);
                        // Until this child has produced its first frame it is
                        // still priming; only a real hang there is a stall.
                        let threshold = if frames == 0 {
                            STALL_STARTUP_MS
                        } else {
                            STALL_EMPTY_MS
                        };
                        if since.elapsed() >= Duration::from_millis(threshold) {
                            // Confirmed stall: count it once, remember the start
                            // of the empty period so its full length is reported
                            // when it resolves, and never double-count the
                            // watchdog's cooldown re-reports.
                            if stall_open.is_none() {
                                stall_open = Some(since);
                                control.note_stall(since.elapsed().as_millis() as u64);
                            }
                            let now = Instant::now();
                            played += Duration::from_secs_f64(
                                now.duration_since(last).as_secs_f64() * speed,
                            );
                            last = now;
                            let wall_ms = played.as_millis() as u64;
                            control.set_position_ms(wall_ms);
                            let sync_ms = control.audio_clock().unwrap_or(wall_ms);
                            let ready =
                                last_resync.map_or(true, |t| t.elapsed() > RESYNC_COOLDOWN);
                            crate::debug_log(&format!(
                                "video stall empty_ms={} audio_clock_ms={} wall_ms={wall_ms} frames={frames} pushes={pushes} resync={ready}",
                                since.elapsed().as_millis(),
                                control.audio_clock().map(|v| v as i64).unwrap_or(-1),
                            ));
                            if ready {
                                *last_resync = Some(Instant::now());
                                control.set_position_ms(sync_ms);
                                control.invalidate_audio_clock();
                                // Confirmed silent stall: the respawned session
                                // must buffer again before resuming.
                                stall_resync = true;
                                seeking = true;
                                break;
                            }
                            // Cooldown active: re-arm so the next report is ~2 s
                            // later instead of once per poll.
                            empty_since = Some(Instant::now());
                        }
                        // Keep the panel live while the ring stays empty: the
                        // rates decay to 0 instead of freezing at the last
                        // value. One clock call per 100 ms poll is negligible.
                        if let Some(wait) = seek_wait.as_mut() {
                            arm_seek_overlay(control, wait);
                        }
                        profiler.tick(control, Instant::now());
                        continue;
                    }
                };

                let now = Instant::now();
                played += Duration::from_secs_f64(now.duration_since(last).as_secs_f64() * speed);
                last = now;
                let wall_ms = played.as_millis() as u64;
                control.set_position_ms(wall_ms);

                // Frosted-glass thumbnail for the panels: the frame is already
                // in hand here, so this costs no extra copy, and it is skipped
                // entirely while nothing consumes it.
                glass.poll(control, &frame, now, target);

                // A resize (or a fullscreen toggle) changes the size worth
                // decoding. One restart once the new size has been still for
                // `RESIZE_DEBOUNCE`, at the current position - the decoder is
                // replaced through the same path as a seek.
                let (now_w, now_h, _) = control.display_size();
                let watch_display = if now_w == 0 || now_h == 0 {
                    (display_w, display_h)
                } else {
                    (now_w, now_h)
                };
                if let Some(next) = resize.poll(source_size, watch_display, now) {
                    let position = control.playback_position_ms();
                    crate::debug_log(&format!(
                        "video target w={} h={} reason=resize capacity={} source={}x{} window={}x{} fullscreen={} from={}x{} pos_ms={position}",
                        next.w,
                        next.h,
                        ring_capacity_for(next.bytes()),
                        source_size.0,
                        source_size.1,
                        watch_display.0,
                        watch_display.1,
                        control.display_size().2,
                        target.w,
                        target.h,
                    ));
                    spawn_reason = "resize";
                    control.set_position_ms(position);
                    control.invalidate_audio_clock();
                    seeking = true;
                    break;
                }

                let audio_ms = control.audio_clock();
                let sync_ms = audio_ms.unwrap_or(wall_ms);
                let video_ms = start_ms as i64 + (frames as f64 * frame_ms).round() as i64;
                let delta = video_ms - sync_ms as i64;

                let mut behind = false;
                // A fresh session has no meaningful A/V relation before its
                // first frame: hold the guard back for the short start grace
                // so an inherited clock cannot re-seek the position the user
                // just picked. After the first frame (or the grace) the
                // existing drift/stall behaviour is unchanged.
                let start_grace = frames == 0 && now < resync_grace_until;
                if delta > SYNC_PACE_EPS_MS {
                    let wait = (delta as u64).min(MAX_SYNC_SLEEP_MS);
                    std::thread::sleep(Duration::from_millis(wait));
                } else if delta < -SYNC_RESYNC_MS
                    && !start_grace
                    && last_resync.map_or(true, |t| t.elapsed() > RESYNC_COOLDOWN)
                {
                    crate::debug_log(&format!(
                        "resync behind delta_ms={delta} audio_clock_ms={} frames={frames} pushes={pushes}",
                        audio_ms.map(|v| v as i64).unwrap_or(-1)
                    ));
                    *last_resync = Some(Instant::now());
                    control.set_position_ms(sync_ms);
                    control.invalidate_audio_clock();
                    seeking = true;
                    break;
                } else if delta < -SYNC_BEHIND_MS && !start_grace {
                    behind = true;
                }

                let gate_now = Instant::now();
                if !behind && gate_now.duration_since(last_push) >= push_interval {
                    last_push = gate_now;
                    pushes += 1;
                    control.note_frame_presented();
                    // The frame is moved into the push (no extra copy on the
                    // pacer); the UI thread copies it once into its Slint pixel
                    // buffer and keeps that copy for 截图.
                    push_frame(window, mini, control, frame, target);
                    // The seek is served: the picture the user picked is on
                    // screen, so the overlay closes in this same frame.
                    if let Some(wait) = seek_wait.take() {
                        if wait.published {
                            let waited = control.end_seek_buffering();
                            crate::debug_log(&format!(
                                "seek served target_ms={} overlay_ms={} pushes={pushes}",
                                wait.target,
                                waited.map(|ms| ms.to_string())
                                    .unwrap_or_else(|| "0".to_string())
                            ));
                        } else {
                            // Known-buffered (or a fast unknown) seek: the
                            // old picture was never replaced by the overlay.
                            crate::debug_log(&format!(
                                "seek served target_ms={} buffered={} overlay_ms=0 pushes={pushes}",
                                wait.target, wait.buffered
                            ));
                        }
                    }
                }
                frames += 1;
                // Diagnostic hook: arm the stall injection once this decoder is
                // actually playing (first frame), so `<secs>` counts playback
                // time rather than connect/seek time.
                if frames == 1 {
                    if let Some(child) = video_child.as_ref() {
                        maybe_arm_stall_test(child);
                    }
                }

                if now.duration_since(last_info) >= Duration::from_secs(1) {
                    last_info = now;
                    control.set_dropped(frames.saturating_sub(pushes));
                    publish_info(window, track, control, cid);
                    crate::debug_log(&format!(
                        "sync wall_ms={wall_ms} video_frame_time_ms={video_ms} audio_clock_ms={} audio_fresh={} delta_ms={delta} frames={frames} pushes={pushes}",
                        audio_ms.map(|value| value as i64).unwrap_or(0),
                        audio_ms.is_some()
                    ));
                }
                profiler.tick(control, now);
                }
            }
            None => loop {
                if control.is_closed() {
                    break;
                }
                if control.is_paused() {
                    paused = true;
                    // No frames are presented while paused; do not leave the
                    // panel showing the rate measured before the pause.
                    control.set_prof_rates(0.0, 0.0);
                    break;
                }
                if control.seek_seq() != seen_seek {
                    seeking = true;
                    break;
                }
                if !control.audio_only() {
                    mode_changed = true;
                    break;
                }

                if control.audio_seq() != seen_audio {
                    seen_audio = control.audio_seq();
                    if let Some(proc) = audio_child.take() {
                        proc.stop();
                        // The pid is dead now: a gain write must not target it.
                        control.set_audio_pid(0);
                    }
                    let resume_ms = control
                        .audio_clock()
                        .unwrap_or_else(|| played.as_millis() as u64);
                    let audio_url = info.audio_url_for(control.audio_index());
                    audio_child = audio_url.as_deref().and_then(|url| {
                        start_audio(url, resume_ms, speed, control)
                    });
                }

                if !has_audio {
                    finished = true;
                    break;
                }
                let audio_exited = match audio_child.as_mut() {
                    Some(proc) => proc.exited(),
                    None => true,
                };
                if audio_exited {
                    finished = true;
                    break;
                }

                let now = Instant::now();
                played += Duration::from_secs_f64(now.duration_since(last).as_secs_f64() * speed);
                last = now;
                let wall_ms = played.as_millis() as u64;
                control.set_position_ms(wall_ms);
                let audio_ms = control.audio_clock();
                let position = audio_ms.unwrap_or(wall_ms);
                let duration = control.duration_ms();
                if duration > 0 && position >= duration {
                    control.note_stream_finished();
                    finished = true;
                    break;
                }
                if now.duration_since(last_info) >= Duration::from_secs(1) {
                    last_info = now;
                    publish_info(window, track, control, cid);
                    crate::debug_log(&format!(
                        "sync audio_only wall_ms={wall_ms} audio_clock_ms={} audio_fresh={} delta_ms={}",
                        audio_ms.map(|value| value as i64).unwrap_or(0),
                        audio_ms.is_some(),
                        position as i64 - wall_ms as i64,
                    ));
                }
                profiler.tick(control, now);
                std::thread::sleep(Duration::from_millis(50));
            },
        }

        // Close out a stall that was still open when this session ended (the
        // resync path breaks out of the pacer loop without a frame arriving).
        if let Some(began) = stall_open.take() {
            control.note_stall_longest(began.elapsed().as_millis() as u64);
        }

        // A session that ended while still buffering (pause, seek, stall or
        // close) must not leave the UI showing a buffer state.
        if control.end_seek_buffering().is_some() {
            crate::debug_log("seek buffer cleared reason=session-end");
        }
        control.end_buffering();
        // Only the empty-ring watchdog re-arms the gate for the next session; a
        // seek, pause, mode change or quality/speed restart resumes at once.
        gate_kind = if stall_resync {
            Some(GateKind::Recovery)
        } else {
            None
        };

        let exit_status = video_child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .map(|status| status.to_string())
            .unwrap_or_else(|| "running".to_string());

        if let Some(child) = video_child.as_mut() {
            kill(child);
        }
        // The child is dead, so the reader's blocking read has returned; join it
        // before the next iteration so no stale frames or threads leak.
        if let Some(source) = source.as_mut() {
            source.stop();
        }
        if let Some(proc) = audio_child.take() {
            proc.stop();
        }
        let stderr_text = stderr_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        crate::debug_log(&format!(
            "decode frames={frames} pushes={pushes} finished={finished} paused={paused} seeking={seeking} mode_changed={mode_changed} exit={exit_status} stderr={}",
            stderr_text.chars().take(240).collect::<String>()
        ));

        if control.is_closed() {
            return Ok(());
        }
        if paused || seeking || mode_changed {
            continue;
        }
        if finished && frames == 0 && !stderr_text.is_empty() {
            return Err(anyhow!("{stderr_text}"));
        }
        return Ok(());
    }
}

/// Shared state of the decode-ahead ring.
struct FrameRing {
    frames: VecDeque<Box<[u8]>>,
    /// The reader thread has finished (clean end of stream or a read error);
    /// no further frames will arrive.
    done: bool,
    /// `stop()` asked the reader to exit even while it waits for ring space.
    stopped: bool,
}

/// A video child's stdout plus the thread that decodes ahead into a bounded
/// ring. The reader owns the blocking pipe read, so the pacer only ever waits
/// on the ring and a silent stall can never block it (see the stall watchdog in
/// [`decode_loop`]).
struct FrameSource {
    ring: Arc<(Mutex<FrameRing>, Condvar)>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// Profiler counters: the reader counts decoded frames and publishes the
    /// ring occupancy, and the pacer updates it after every pop.
    control: Arc<PlaybackControl>,
}

impl FrameSource {
    /// Spawn the reader thread that fills the ring from `stdout`.
    ///
    /// `spawn_ms` is the monotonic timestamp taken just before the child was
    /// spawned; the first frame to land in the ring turns it into the
    /// profiler's spawn→first-frame latency.
    fn spawn(
        mut stdout: std::process::ChildStdout,
        control: Arc<PlaybackControl>,
        spawn_ms: u64,
        frame_bytes: usize,
        capacity: usize,
    ) -> Self {
        let capacity = capacity.max(1);
        let ring = Arc::new((
            Mutex::new(FrameRing {
                frames: VecDeque::with_capacity(capacity),
                done: false,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let reader_ring = ring.clone();
        let reader_control = control.clone();
        let reader = std::thread::spawn(move || {
            let (lock, cvar) = &*reader_ring;
            let mut first = true;
            loop {
                match lock.lock() {
                    Ok(guard) if guard.stopped => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
                let mut slot = vec![0u8; frame_bytes].into_boxed_slice();
                match read_frame(&mut stdout, &mut slot) {
                    Ok(true) => {
                        let Ok(mut guard) = lock.lock() else { return };
                        // Backpressure: block while the ring is full, so ffmpeg
                        // blocks on the pipe exactly as it did before.
                        while guard.frames.len() >= capacity && !guard.stopped {
                            guard = match cvar.wait(guard) {
                                Ok(guard) => guard,
                                Err(_) => return,
                            };
                        }
                        if guard.stopped {
                            return;
                        }
                        guard.frames.push_back(slot);
                        reader_control.note_frame_decoded(guard.frames.len());
                        if first {
                            first = false;
                            reader_control
                                .note_first_frame(monotonic_ms().saturating_sub(spawn_ms));
                        }
                        drop(guard);
                        cvar.notify_all();
                    }
                    Ok(false) => {
                        if let Ok(mut guard) = lock.lock() {
                            guard.done = true;
                        }
                        cvar.notify_all();
                        return;
                    }
                    Err(_) => {
                        if let Ok(mut guard) = lock.lock() {
                            guard.done = true;
                        }
                        cvar.notify_all();
                        return;
                    }
                }
            }
        });
        Self {
            ring,
            reader: Some(reader),
            control,
        }
    }

    /// Wait up to `timeout` for the next frame.
    ///
    /// `Ok(Some(frame))` when one arrives, `Ok(None)` when the source has ended
    /// (clean end of stream or a read error), and `Err(())` when the wait timed
    /// out with the ring still empty - the caller treats that as a stall.
    fn next_frame(&self, timeout: Duration) -> Result<Option<Box<[u8]>>, ()> {
        let (lock, cvar) = &*self.ring;
        let Ok(mut guard) = lock.lock() else {
            return Ok(None);
        };
        if let Some(frame) = guard.frames.pop_front() {
            self.control.note_ring_len(guard.frames.len());
            drop(guard);
            cvar.notify_all();
            return Ok(Some(frame));
        }
        if guard.done {
            return Ok(None);
        }
        guard = match cvar.wait_timeout(guard, timeout) {
            Ok((guard, _)) => guard,
            Err(_) => return Ok(None),
        };
        if let Some(frame) = guard.frames.pop_front() {
            self.control.note_ring_len(guard.frames.len());
            drop(guard);
            cvar.notify_all();
            return Ok(Some(frame));
        }
        if guard.done {
            return Ok(None);
        }
        Err(())
    }

    /// Whether the reader has finished (clean end of stream or a read error),
    /// so no further frames will arrive. Used by the pre-roll gate to fail
    /// through instead of waiting for a target that can never be reached; the
    /// lock is only held for the flag read (the reader releases it around the
    /// blocking pipe read).
    fn is_done(&self) -> bool {
        let (lock, _cvar) = &*self.ring;
        match lock.lock() {
            Ok(guard) => guard.done,
            Err(_) => true,
        }
    }

    /// Stop and join the reader. The video child must already be killed so its
    /// blocking read returns; a reader waiting for ring space is woken by the
    /// `stopped` flag.
    fn stop(&mut self) {
        let (lock, cvar) = &*self.ring;
        if let Ok(mut guard) = lock.lock() {
            guard.stopped = true;
            guard.frames.clear();
        }
        cvar.notify_all();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // The ring is gone with this child; occupancy is honestly 0.
        self.control.note_ring_len(0);
    }
}

/// Read exactly one full frame; `Ok(false)` at a clean end of stream.
fn read_frame(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

fn push_frame(
    window: &slint::Weak<PlayerWindow>,
    mini: &MiniSlot,
    control: &Arc<PlaybackControl>,
    frame: Box<[u8]>,
    size: FrameSize,
) {
    let window = window.clone();
    let mini = mini.clone();
    let control = control.clone();
    // `into_vec` reuses the allocation the reader filled, so the pacer adds no
    // full-frame copy of its own. The UI thread copies once into the Slint
    // pixel buffer and hands the same Vec to the screenshot slot.
    let bytes = frame.into_vec();
    let _ = slint::invoke_from_event_loop(move || {
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size.w, size.h);
        buffer.make_mut_bytes().copy_from_slice(&bytes);
        control.store_last_frame(bytes, size);
        let image = slint::Image::from_rgba8(buffer);
        if let Some(window) = window.upgrade() {
            window.set_frame(image.clone());
        }
        let mini = mini
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .and_then(|weak| weak.upgrade());
        if let Some(mini) = mini {
            mini.set_frame(image);
        }
    });
}

/// Reusable working set for the video-glass thumbnail.
///
/// It lives on the pacer thread, which already owns the popped frame, so the
/// 10 fps downscale/blur never touches the UI thread and the frame on screen is
/// not copied a second time. Buffers are allocated once per decode loop and
/// swapped with [`PlaybackControl::publish_video_glass`], so a steady-state
/// update allocates nothing.
struct VideoGlassWorker {
    /// 96x54 RGBA8 output, swapped into the control on publish.
    pixels: Vec<u8>,
    /// Same-size intermediate for the separable blur.
    scratch: Vec<u8>,
    last_update: Option<Instant>,
}

impl VideoGlassWorker {
    fn new() -> Self {
        Self {
            pixels: vec![0; VIDEO_GLASS_BYTES],
            scratch: vec![0; VIDEO_GLASS_BYTES],
            last_update: None,
        }
    }

    /// Publish a blurred copy of `frame` when a consumer wants it and the
    /// [`VIDEO_GLASS_INTERVAL`] has elapsed.
    ///
    /// With no consumer this is one relaxed atomic load: no downscale, no blur,
    /// no allocation, no buffer retained by the control.
    fn poll(&mut self, control: &PlaybackControl, frame: &[u8], now: Instant, size: FrameSize) {
        if !control.video_glass_wanted() {
            // Re-arm the throttle so a re-enabled consumer publishes at once.
            self.last_update = None;
            return;
        }
        if let Some(last) = self.last_update {
            if now.duration_since(last) < VIDEO_GLASS_INTERVAL {
                return;
            }
        }
        if frame.len() != size.bytes() {
            return;
        }
        // The control hands back whatever it held (possibly an empty buffer
        // after a disable), so keep the working copy at thumbnail size.
        if self.pixels.len() != VIDEO_GLASS_BYTES {
            self.pixels.resize(VIDEO_GLASS_BYTES, 0);
        }
        self.last_update = Some(now);
        let started = Instant::now();
        downscale_to_glass(frame, &mut self.pixels, size);
        for _ in 0..VIDEO_GLASS_BLUR_PASSES {
            box_blur_glass(&mut self.pixels, &mut self.scratch, VIDEO_GLASS_BLUR_RADIUS);
        }
        let compute_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        control.publish_video_glass(&mut self.pixels, compute_us);
    }
}

/// Average a 3x3 grid of samples inside each output pixel's block of the
/// decoded frame into one pixel of the 96x54 thumbnail (straight RGBA8; alpha
/// like the colour channels).
///
/// The source is the display-sized frame (`frame_w`x`frame_h`), so the block is
/// `frame_w/96` x `frame_h/54`, computed per axis instead of assuming the old
/// fixed 960x540 ten-pixel step. The grid samples (not every source pixel) are
/// what keeps the update around 0.1 ms: three box-blur passes follow, so the
/// frosted result is indistinguishable from an exact box average at panel size.
fn downscale_to_glass(src: &[u8], dst: &mut [u8], size: FrameSize) {
    let out_w = VIDEO_GLASS_W as usize;
    let out_h = VIDEO_GLASS_H as usize;
    let in_w = size.w.max(1) as usize;
    let in_h = size.h.max(1) as usize;
    let row_bytes = in_w * 4;
    let samples = 9u32;
    for oy in 0..out_h {
        let y0 = oy * in_h / out_h;
        let y1 = ((oy + 1) * in_h / out_h).max(y0 + 1).min(in_h);
        let ys = [y0, (y0 + y1) / 2, y1 - 1];
        for ox in 0..out_w {
            let x0 = ox * in_w / out_w;
            let x1 = ((ox + 1) * in_w / out_w).max(x0 + 1).min(in_w);
            let xs = [x0, (x0 + x1) / 2, x1 - 1];
            let mut sum = [0u32; 4];
            for y in ys {
                let row = y * row_bytes;
                for x in xs {
                    let at = row + x * 4;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(src[at + channel]);
                    }
                }
            }
            let out = (oy * out_w + ox) * 4;
            for (channel, total) in sum.iter().enumerate() {
                dst[out + channel] = (total / samples) as u8;
            }
        }
    }
}

/// One separable box-blur pass (horizontal, then vertical) over the 96x54
/// thumbnail, with clamped (replicated) edges. `scratch` is the same size as
/// `pixels`.
///
/// A sliding window keeps this at one add plus one subtract per channel per
/// pixel instead of re-summing the whole kernel.
fn box_blur_glass(pixels: &mut [u8], scratch: &mut [u8], radius: usize) {
    let w = VIDEO_GLASS_W as usize;
    let h = VIDEO_GLASS_H as usize;
    let taps = (radius * 2 + 1) as u32;
    let last_x = w as isize - 1;
    let last_y = h as isize - 1;
    let iradius = radius as isize;

    // Horizontal: pixels -> scratch.
    for y in 0..h {
        let row = y * w * 4;
        let mut sums = [0u32; 4];
        for i in 0..radius * 2 + 1 {
            let x = (i as isize - iradius).clamp(0, last_x) as usize;
            let at = row + x * 4;
            for (channel, total) in sums.iter_mut().enumerate() {
                *total += u32::from(pixels[at + channel]);
            }
        }
        for x in 0..w {
            let out = row + x * 4;
            for (channel, total) in sums.iter().enumerate() {
                scratch[out + channel] = (total / taps) as u8;
            }
            // Slide to x + 1: drop x - radius, add x + radius + 1 (clamped).
            let rem = (x as isize - iradius).clamp(0, last_x) as usize;
            let add = (x as isize + iradius + 1).clamp(0, last_x) as usize;
            let rem_at = row + rem * 4;
            let add_at = row + add * 4;
            for (channel, total) in sums.iter_mut().enumerate() {
                *total += u32::from(pixels[add_at + channel]);
                *total -= u32::from(pixels[rem_at + channel]);
            }
        }
    }

    // Vertical: scratch -> pixels.
    for x in 0..w {
        let mut sums = [0u32; 4];
        for i in 0..radius * 2 + 1 {
            let y = (i as isize - iradius).clamp(0, last_y) as usize;
            let at = (y * w + x) * 4;
            for (channel, total) in sums.iter_mut().enumerate() {
                *total += u32::from(scratch[at + channel]);
            }
        }
        for y in 0..h {
            let out = (y * w + x) * 4;
            for (channel, total) in sums.iter().enumerate() {
                pixels[out + channel] = (total / taps) as u8;
            }
            let rem = (y as isize - iradius).clamp(0, last_y) as usize;
            let add = (y as isize + iradius + 1).clamp(0, last_y) as usize;
            let rem_at = (rem * w + x) * 4;
            let add_at = (add * w + x) * 4;
            for (channel, total) in sums.iter_mut().enumerate() {
                *total += u32::from(scratch[add_at + channel]);
                *total -= u32::from(scratch[rem_at + channel]);
            }
        }
    }
}

/// QA hooks for the video-glass thumbnail.
///
/// `BILI_VIDEO_GLASS_LOG=1` writes one
/// `video glass frames=<n> ms_avg=<…> size=96x54` line per second, counting the
/// updates of the last second, so `frames=0` proves the disabled path does no
/// work. `BILI_VIDEO_GLASS_TEST=<secs>` enables the consumer for that many
/// seconds and releases it again, which proves the whole path before the UI
/// wiring exists (and without synthetic input).
fn spawn_video_glass_qa(control: &Arc<PlaybackControl>) {
    let log = std::env::var("BILI_VIDEO_GLASS_LOG")
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    let test_secs = std::env::var("BILI_VIDEO_GLASS_TEST")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0);
    if !log && test_secs.is_none() {
        return;
    }
    let weak = Arc::downgrade(control);
    // The QA window runs on its own thread so the 1 Hz log line starts at once
    // and covers the enabled period instead of only its tail.
    if let Some(secs) = test_secs {
        let test_weak = weak.clone();
        let test = std::thread::Builder::new()
            .name("video-glass-test".to_string())
            .spawn(move || {
                if let Some(control) = test_weak.upgrade() {
                    control.set_video_glass_needed(true);
                }
                std::thread::sleep(Duration::from_secs(secs));
                if let Some(control) = test_weak.upgrade() {
                    control.set_video_glass_needed(false);
                }
            });
        let _ = test;
    }
    if !log {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("video-glass-qa".to_string())
        .spawn(move || {
            let mut seen_frames = 0u64;
            let mut seen_us = 0u64;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(control) = weak.upgrade() else {
                    return;
                };
                if control.is_closed() {
                    return;
                }
                let frames = control.video_glass_frames.load(Ordering::Relaxed);
                let us = control.video_glass_us.load(Ordering::Relaxed);
                let delta_frames = frames.saturating_sub(seen_frames);
                let delta_us = us.saturating_sub(seen_us);
                seen_frames = frames;
                seen_us = us;
                let ms_avg = if delta_frames > 0 {
                    delta_us as f64 / delta_frames as f64 / 1000.0
                } else {
                    0.0
                };
                crate::debug_log(&format!(
                    "video glass frames={delta_frames} ms_avg={ms_avg:.3} size={}x{}",
                    VIDEO_GLASS_W, VIDEO_GLASS_H
                ));
            }
        });
    let _ = spawned;
}

/// Publish the 视频信息统计面板 values for the active video track.
fn publish_info(
    window: &slint::Weak<PlayerWindow>,
    track: Option<&bilibili::VideoTrack>,
    control: &Arc<PlaybackControl>,
    cid: u64,
) {
    let qn = control.quality();
    let mut lines: Vec<SharedString> = Vec::new();
    match track {
        Some(track) => {
            lines.push(
                format!("画质：{}（qn {}）", bilibili::quality_label(track.id), track.id).into(),
            );
            let codecs = if track.codecs.is_empty() {
                "-".to_string()
            } else {
                track.codecs.clone()
            };
            lines.push(
                format!(
                    "编码：{}（codecid {} · {}）",
                    bilibili::codec_label(track.codecid),
                    track.codecid,
                    codecs
                )
                .into(),
            );
            lines.push(format!("分辨率：{}×{}", track.width, track.height).into());
            lines.push(format!("码率：{} Kbps", track.bandwidth / 1000).into());
        }
        None => {
            lines.push(format!("画质：{}（qn {}）", bilibili::quality_label(qn), qn).into());
        }
    }
    lines.push(
        format!(
            "硬件解码：{}",
            if control.hwaccel() { "开" } else { "关" }
        )
        .into(),
    );
    lines.push(
        format!(
            "仅音频：{}",
            if control.audio_only() { "开" } else { "关" }
        )
        .into(),
    );
    lines.push(format!("丢帧：{}", control.dropped()).into());
    lines.push(format!("cid：{cid}").into());
    lines.push(format!("时长：{}", fmt_mmss(control.duration_ms())).into());
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_info_lines(ModelRc::from(std::rc::Rc::new(VecModel::from(lines))));
        }
    });
}

/// Publish a transient player message.
///
/// `player_status` is still written (the UI keeps the property declared), but
/// the visible copy is the toast stack: every non-empty message is forwarded to
/// [`crate::toast_active`], which resolves the player window that is actually
/// open on the UI thread. `kind` is one of `crate::TOAST_KIND_*`; the raw text
/// always stays in the diagnostic log.
fn set_status(window: &slint::Weak<PlayerWindow>, text: &str, kind: i32) {
    let window = window.clone();
    let text = text.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        crate::debug_log(&format!("player status kind={kind} text={text}"));
        if let Some(window) = window.upgrade() {
            window.set_player_status(text.clone().into());
            if !text.is_empty() {
                crate::toast_active(text, kind);
            }
        }
    });
}

fn set_playing(window: &slint::Weak<PlayerWindow>, playing: bool) {
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_playing(playing);
        }
    });
}

/// Publish the available qualities and the current selection to the UI.
///
/// Two models go out: the old collapsed-by-`qn` `quality_items` (still
/// declared, no longer drawn) and `quality_rows`, one row per raw DASH stream
/// (the 画质 menu). The catalogue behind the rows is stored on the control so
/// `quality_pick(index)` resolves the exact `(qn, codecid, bandwidth)` later.
fn set_qualities(
    window: &slint::Weak<PlayerWindow>,
    control: &Arc<PlaybackControl>,
    info: &bilibili::PlayInfo,
    current: (i64, i64, u64),
) {
    let mut seen: Vec<i64> = Vec::new();
    let mut entries: Vec<QualityEntry> = Vec::new();
    for track in &info.video_tracks {
        if seen.contains(&track.id) {
            continue;
        }
        seen.push(track.id);
        entries.push(QualityEntry {
            qn: track.id as i32,
            label: bilibili::quality_label(track.id).into(),
            vip: bilibili::is_vip_quality(track.id),
        });
    }
    let specs = quality_specs(&info.qualities);
    control.set_quality_catalog(specs.clone());
    let rows = quality_rows(&specs, Some(current));
    crate::debug_log(&format!(
        "quality rows n={} current={}/{}/{} list={}",
        rows.len(),
        current.0,
        current.1,
        current.2,
        rows.iter()
            .take(3)
            .map(|row| format!("{}/{}", row.label, row.detail))
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    let label = bilibili::quality_label(current.0);
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_quality_items(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
            window.set_quality_rows(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
            window.set_quality_qn(current.0 as i32);
            window.set_quality_label(label.into());
        }
    });
}

/// Publish the available DASH audio tracks and the current selection to the UI.
fn set_audio_tracks(
    window: &slint::Weak<PlayerWindow>,
    tracks: &[bilibili::AudioTrack],
    index: usize,
) {
    let entries: Vec<AudioEntry> = tracks
        .iter()
        .enumerate()
        .map(|(position, track)| AudioEntry {
            index: position as i32,
            label: bilibili::audio_label(track.id, track.bandwidth).into(),
            vip: track.vip,
        })
        .collect();
    let label = tracks
        .get(index)
        .map(|track| bilibili::audio_label(track.id, track.bandwidth))
        .unwrap_or_else(|| "音轨".to_string());
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_audio_items(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
            window.set_audio_index(index as i32);
            window.set_audio_label(label.into());
        }
    });
}

/// Directory holding `ffmpeg.exe` / `ffplay.exe`; overridable via env.
fn ffmpeg_dir() -> PathBuf {
    std::env::var("BILI_FFMPEG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Programs\ffmpeg-unpack\bin"))
}

fn base_command(exe: &str) -> Command {
    let dir = ffmpeg_dir();
    let mut command = Command::new(dir.join(exe));
    if let Some(existing) = std::env::var_os("PATH") {
        let mut path = dir.into_os_string();
        path.push(";");
        path.push(existing);
        command.env("PATH", path);
    }
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn format_secs(ms: u64) -> String {
    format!("{:.3}", ms as f64 / 1000.0)
}

fn spawn_video(
    url: &str,
    start_ms: u64,
    hwaccel: bool,
    size: FrameSize,
) -> std::io::Result<Child> {
    let mut command = base_command("ffmpeg.exe");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        // Decode on all cores and survive CDN hiccups mid-stream. The reconnect
        // options resume a dropped connection instead of ending playback; no
        // `-reconnect_at_eof` (that would restart a VOD from the beginning).
        .arg("-threads")
        .arg("0")
        .arg("-fflags")
        .arg("+discardcorrupt")
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        .arg("-rw_timeout")
        .arg("15000000");
    if hwaccel {
        command.arg("-hwaccel").arg("d3d11va");
    }

    // No `-re`/`-readrate`: ffmpeg decodes ahead and blocks on the rawvideo
    // pipe, and the decode loop paces the frames itself against the audio clock.
    // The scaler target is the display size computed in `target_frame_size`
    // (never a fixed 960x540), so a 1080P source keeps its detail.
    let filter = format!("scale={}:{}", size.w, size.h);

    if start_ms > 0 {
        command.arg("-ss").arg(format_secs(start_ms));
    }
    crate::debug_log(&format!(
        "spawn ffmpeg hwaccel={hwaccel} start_ms={start_ms} vf={filter} out=rawvideo:rgba:{}x{} bytes={}",
        size.w,
        size.h,
        size.bytes()
    ));
    command
        .arg("-headers")
        .arg(MEDIA_HEADERS)
        .arg("-i")
        .arg(url)
        .arg("-an")
        .arg("-vf")
        .arg(filter)
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| {
            crate::job::assign(&child);
            child
        })
}

fn spawn_audio(url: &str, start_ms: u64, speed: f64) -> std::io::Result<Child> {
    let mut command = base_command("ffplay.exe");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("quiet")
        .arg("-stats")
        .arg("-nodisp")
        .arg("-autoexit")
        // Unbounded input buffer + the same reconnect policy as the video
        // decoder so audio keeps flowing through CDN hiccups.
        .arg("-infbuf")
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        .arg("-rw_timeout")
        .arg("15000000")
        // Fixed, never the user's level: gain and mute are applied to this
        // process' WASAPI session (see `PlaybackControl::set_volume`), so a
        // volume change is a session write instead of a process restart.
        .arg("-volume")
        .arg("100");
    if (speed - 1.0).abs() > 1e-9 {
        command.arg("-af").arg(format!("atempo={speed}"));
    }
    if start_ms > 0 {
        command.arg("-ss").arg(format_secs(start_ms));
    }
    command
        .arg("-headers")
        .arg(MEDIA_HEADERS)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| {
            crate::job::assign(&child);
            child
        })
}

/// A running `ffplay` plus the thread draining its stats from stderr.
struct AudioProc {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl AudioProc {
    fn stop(mut self) {
        kill(&mut self.child);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// Spawn `ffplay` for `url` and start the thread that turns its `-stats` line
/// into the shared audio clock.
///
/// The child's pid is published to `control` so gain/mute go to its audio
/// session, and the `spawn ffplay` line is the restart marker QA greps for.
fn start_audio(
    url: &str,
    start_ms: u64,
    speed: f64,
    control: &Arc<PlaybackControl>,
) -> Option<AudioProc> {
    // Claim this process' clock generation before spawning, so every sample it
    // emits is attributed to this process and the replaced one cannot write
    // past the seek/restart that retired it.
    let generation = control.begin_audio_clock_generation();
    let mut child = spawn_audio(url, start_ms, speed).ok()?;
    let pid = child.id();
    control.set_audio_pid(pid);
    crate::debug_log(&format!(
        "spawn ffplay pid={pid} start_ms={start_ms} speed={speed:.2} volume=100"
    ));
    let reader = child.stderr.take().map(|pipe| {
        let control = control.clone();
        std::thread::spawn(move || read_audio_stats(pipe, control, generation))
    });
    Some(AudioProc { child, reader })
}

/// Drain ffplay's stderr and keep the newest playback position in `control`.
///
/// `-stats` emits `\r`-separated status lines whose first token is the current
/// position in seconds (e.g. `  12.34 A-V: ...` or `M-A:`); everything else on
/// stderr is ignored.
fn read_audio_stats(mut pipe: impl Read, control: Arc<PlaybackControl>, generation: u64) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                while let Some(index) = pending
                    .iter()
                    .position(|byte| *byte == b'\r' || *byte == b'\n')
                {
                    let segment: Vec<u8> = pending.drain(..=index).collect();
                    apply_audio_stats(&segment, &control, generation);
                }
                if pending.len() > 8192 {
                    let segment = std::mem::take(&mut pending);
                    apply_audio_stats(&segment, &control, generation);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Parse one ffplay status segment; `nan`/negative/absent positions are skipped.
fn apply_audio_stats(segment: &[u8], control: &Arc<PlaybackControl>, generation: u64) {
    let text = String::from_utf8_lossy(segment);
    let line = text.trim();
    if !(line.contains("A-V:") || line.contains("M-A:") || line.contains("fd=")) {
        return;
    }
    let Some(first) = line.split_whitespace().next() else {
        return;
    };
    if let Ok(seconds) = first.parse::<f64>() {
        if seconds.is_finite() && seconds >= 0.0 {
            control.set_audio_clock_ms(generation, (seconds * 1000.0).round() as u64);
        }
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Diagnostic hook `BILI_STALL_TEST=<secs>`: after `<secs>` of playback suspend
/// the video decoder (`NtSuspendProcess`) so its TCP connection stays open but
/// no frames arrive - a silent stall that exercises the watchdog without
/// touching the network. The resync kills the suspended child and respawns it;
/// the resume after a few seconds is only a safety net if detection is off.
///
/// Fires once per process and only against a still-live child (a hardware
/// decoder that already failed over is skipped, so the fallback child is the
/// one suspended). Kept permanently as diagnostic infrastructure.
fn maybe_arm_stall_test(child: &Child) {
    static FIRED: AtomicBool = AtomicBool::new(false);
    if FIRED.load(Ordering::SeqCst) {
        return;
    }
    let Ok(value) = std::env::var("BILI_STALL_TEST") else {
        return;
    };
    let Ok(secs) = value.trim().parse::<u64>() else {
        return;
    };
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        if FIRED.load(Ordering::SeqCst) {
            return;
        }
        // SAFETY: plain Win32 calls; the handle is owned by this thread and
        // closed below. A dead pid yields a null handle, handled explicitly.
        let handle = unsafe {
            OpenProcess(
                PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle == 0 {
            crate::debug_log(&format!("stall-test open failed pid={pid}"));
            return;
        }
        let mut code: u32 = 0;
        let alive = unsafe { GetExitCodeProcess(handle, &mut code) } != 0 && code == STILL_ACTIVE;
        if !alive {
            // The child already exited (e.g. hwaccel fallback); let the next
            // spawned child arm its own timer.
            crate::debug_log(&format!("stall-test child gone pid={pid}"));
            unsafe { CloseHandle(handle) };
            return;
        }
        FIRED.store(true, Ordering::SeqCst);
        let suspend = unsafe { NtSuspendProcess(handle) };
        crate::debug_log(&format!(
            "stall-test suspend pid={pid} after_secs={secs} status={suspend}"
        ));
        // The watchdog needs ~2 s to detect the empty ring; resume well after
        // that so the resync (which replaces this child) normally wins.
        std::thread::sleep(Duration::from_secs(6));
        let resume = unsafe { NtResumeProcess(handle) };
        crate::debug_log(&format!("stall-test resume pid={pid} status={resume}"));
        unsafe { CloseHandle(handle) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_roll_target_is_clamped_to_the_ring() {
        // 30 fps asks for ceil(1.5 * 30) = 45 frames of video; a 16-frame ring
        // caps it, which is the honest ~0.53 s ceiling at that size/rate.
        assert_eq!(GateKind::Initial.target_frames(1000.0 / 30.0, 16), 16);
        // 10 fps fits the full 1.5 s request: ceil(1.5 * 10) = 15 frames.
        assert_eq!(GateKind::Initial.target_frames(1000.0 / 10.0, 16), 15);
        // A 1080p ring holds only 7 frames: the gate never asks for more.
        assert_eq!(GateKind::Initial.target_frames(1000.0 / 10.0, 7), 7);
        // Unknown / nonsense frame periods fall back to 30 fps, then cap.
        assert_eq!(GateKind::Initial.target_frames(0.0, 16), 16);
        assert_eq!(GateKind::Initial.target_frames(f64::NAN, 5), 5);
        // Recovery waits for the whole ring, whatever the frame rate.
        assert_eq!(GateKind::Recovery.target_frames(1000.0 / 30.0, 16), 16);
        assert_eq!(GateKind::Recovery.target_frames(1000.0 / 240.0, 4), 4);
        assert_eq!(GateKind::Initial.timeout_ms(), PRE_ROLL_TIMEOUT_MS);
        assert_eq!(GateKind::Recovery.timeout_ms(), PRE_ROLL_RECOVERY_TIMEOUT_MS);
    }

    #[test]
    fn target_frame_size_uses_the_display_and_preserves_aspect() {
        // QA window 1128x633 on a 1080p source: the picture is drawn ~632 high,
        // 1124 wide at the source aspect - NOT 960x540.
        let qa = target_frame_size(1920, 1080, 1128, 633);
        assert_eq!(qa, FrameSize { w: 1124, h: 632 });
        assert_ne!(qa, FrameSize { w: 960, h: 540 });
        // Fullscreen is capped at 1920x1080, not the 4K monitor.
        assert_eq!(
            target_frame_size(1920, 1080, 2560, 1440),
            FrameSize {
                w: 1920,
                h: 1080
            }
        );
        // 4K source in fullscreen: still capped at 1080p (the budget/cost cap).
        assert_eq!(
            target_frame_size(3840, 2160, 2560, 1440),
            FrameSize {
                w: 1920,
                h: 1080
            }
        );
        // Never upscale a small source into a large window.
        assert_eq!(
            target_frame_size(1280, 720, 1920, 1080),
            FrameSize {
                w: 1280,
                h: 720
            }
        );
        // A 4:3 source keeps its aspect (not stretched to the window's 16:9).
        let four_three = target_frame_size(1440, 1080, 1000, 700);
        assert_eq!(four_three, FrameSize { w: 932, h: 700 });
        assert!(four_three.w <= 1000 && four_three.h <= 700);
        // Odd source/clamp values are made even, and the width comes from the
        // source aspect, not from an independent rounding.
        let odd = target_frame_size(1920, 1080, 1129, 635);
        assert_eq!(odd.w % 2, 0);
        assert_eq!(odd.h % 2, 0);
        assert!(odd.w <= 1129 && odd.h <= 635);
        // Unknown source geometry (audio-only / not in the manifest): the
        // window, capped, never zero.
        let unknown = target_frame_size(0, 0, 1128, 633);
        assert_eq!(unknown, FrameSize { w: 1128, h: 632 });
        let tiny = target_frame_size(0, 0, 1, 1);
        assert!(tiny.w >= 2 && tiny.h >= 2);
    }

    #[test]
    fn ring_capacity_follows_the_frame_budget() {
        // 1920x1080 RGBA = 8.3 MB: 64 MB / 8.3 MB = 8 frames (~63 MB).
        let uhd = FrameSize {
            w: 1920,
            h: 1080,
        };
        assert_eq!(ring_capacity_for(uhd.bytes()), 8);
        // 1124x632 = 2.8 MB fits 16 frames (45 MB), still under the budget.
        let qa = FrameSize { w: 1124, h: 632 };
        assert_eq!(ring_capacity_for(qa.bytes()), 16);
        // The old 960x540 frame (2.1 MB) also fits the 16-frame ceiling.
        assert_eq!(
            ring_capacity_for(FrameSize { w: 960, h: 540 }.bytes()),
            16
        );
        // A tiny frame is bounded by the ceiling, never unbounded.
        assert_eq!(ring_capacity_for(FrameSize { w: 2, h: 2 }.bytes()), 16);
        // Nonsense input still yields a usable ring.
        assert_eq!(ring_capacity_for(0), 16);
        // The budget is respected at the large end.
        assert!(ring_capacity_for(uhd.bytes()) * uhd.bytes() <= RING_BUDGET_BYTES);
        assert!(ring_capacity_for(qa.bytes()) * qa.bytes() <= RING_BUDGET_BYTES);
    }

    #[test]
    fn display_size_publishes_both_axes_and_reads_back() {
        let control = PlaybackControl::new();
        assert_eq!(control.display_size(), (0, 0, false));
        // The first publish changes BOTH axes; the height must be stored even
        // though the width compare already reported a change.
        assert!(control.note_display_size(1128, 633, false));
        assert_eq!(control.display_size(), (1128, 633, false));
        // Same size again is not a change.
        assert!(!control.note_display_size(1128, 633, false));
        // A height-only change still publishes, and fullscreen is tracked.
        assert!(control.note_display_size(1128, 700, true));
        assert_eq!(control.display_size(), (1128, 700, true));
        // Zero dimensions are ignored (no native handle / minimised).
        assert!(!control.note_display_size(0, 0, true));
        assert_eq!(control.display_size(), (1128, 700, true));
    }

    #[test]
    fn resize_watch_debounces_and_only_fires_above_15_percent() {
        let base = FrameSize { w: 1124, h: 632 };
        let mut watch = ResizeWatch::new(base);
        let start = Instant::now();
        // A 10% growth is inside the tolerance: never a restart.
        let small = (1124 + 112, 633 + 63);
        assert_eq!(watch.poll((1920, 1080), small, start), None);
        assert_eq!(
            watch.poll((1920, 1080), small, start + Duration::from_secs(2)),
            None
        );
        // A real resize: one restart, only after 400 ms of stillness.
        let large = (1800, 1000);
        assert_eq!(watch.poll((1920, 1080), large, start), None);
        assert_eq!(
            watch.poll((1920, 1080), large, start + Duration::from_millis(399)),
            None
        );
        let fired = watch
            .poll((1920, 1080), large, start + Duration::from_millis(400))
            .expect("a stable >15% resize restarts");
        assert_eq!(fired, target_frame_size(1920, 1080, 1800, 1000));
        // Repeated polls at the same size do not restart again.
        assert_eq!(
            watch.poll(
                (1920, 1080),
                large,
                start + Duration::from_millis(3000)
            ),
            None
        );
        // A size that moves again during the debounce restarts the window.
        let mut watch = ResizeWatch::new(base);
        let a = (1800, 1000);
        let b = (1500, 900);
        watch.poll((1920, 1080), a, start);
        watch.poll((1920, 1080), b, start + Duration::from_millis(300));
        assert_eq!(
            watch.poll((1920, 1080), b, start + Duration::from_millis(500)),
            None,
            "300ms are not enough for the new size"
        );
        assert!(watch
            .poll((1920, 1080), b, start + Duration::from_millis(700))
            .is_some());
    }

    /// One playable DASH stream for the automatic-rule tests.
    fn video_track(qn: i64, codecid: i64, bandwidth: u64) -> bilibili::VideoTrack {
        bilibili::VideoTrack {
            id: qn,
            codecid,
            codecs: bilibili::codec_label(codecid).to_string(),
            width: 1920,
            height: 1080,
            bandwidth,
            frame_rate: 30.0,
            url: format!("https://example.invalid/{codecid}/{bandwidth}"),
            urls: Vec::new(),
        }
    }

    fn play_info(
        tracks: Vec<bilibili::VideoTrack>,
        default_qn: i64,
    ) -> bilibili::PlayInfo {
        bilibili::PlayInfo {
            video_tracks: tracks,
            qualities: Vec::new(),
            default_qn,
            audio_tracks: Vec::new(),
            duration: 60,
        }
    }

    #[test]
    fn auto_rule_picks_the_highest_bandwidth_at_the_qn() {
        // 1080P exactly as Bilibili returns it: AV1 is the low-bitrate variant,
        // AVC the one the web player uses. The old rule chose 0.58 Mbps AV1.
        let info = play_info(
            vec![
                video_track(80, 7, 1_180_000),
                video_track(80, 12, 900_000),
                video_track(80, 13, 580_000),
            ],
            80,
        );
        let auto = auto_track_for(&info, 80, 0).expect("a track at 80");
        assert_eq!((auto.codecid, auto.bandwidth), (7, 1_180_000));
        assert_ne!(auto.bandwidth, 580_000, "not the lowest-bitrate stream");

        // An explicit codec preference still wins at the qn ...
        assert_eq!(
            auto_track_for(&info, 80, 13).map(|t| t.bandwidth),
            Some(580_000)
        );
        // ... and if it is missing at that qn, the highest of the rest is used
        // instead of dropping to another quality tier.
        assert_eq!(
            auto_track_for(&info, 80, 12).map(|t| t.bandwidth),
            Some(900_000)
        );
        let no_av1 = play_info(
            vec![
                video_track(80, 7, 1_180_000),
                video_track(80, 12, 900_000),
            ],
            80,
        );
        assert_eq!(
            auto_track_for(&no_av1, 80, 13).map(|t| (t.codecid, t.bandwidth)),
            Some((7, 1_180_000))
        );

        // A qn with no stream falls back to `default_qn` with the same rule.
        let fallback = play_info(
            vec![
                video_track(32, 7, 400_000),
                video_track(80, 7, 1_180_000),
                video_track(80, 13, 580_000),
            ],
            80,
        );
        assert_eq!(
            auto_track_for(&fallback, 116, 0).map(|t| (t.id, t.bandwidth)),
            Some((80, 1_180_000))
        );
        // No tracks at all: no stream, not a panic.
        assert!(auto_track_for(&play_info(Vec::new(), 80), 80, 0).is_none());
    }

    #[test]
    fn buffer_progress_tracks_the_ring_toward_the_target() {
        assert_eq!(buffer_progress_milli(0, 16), 0);
        assert_eq!(buffer_progress_milli(4, 16), 250);
        assert_eq!(buffer_progress_milli(8, 16), 500);
        assert_eq!(buffer_progress_milli(16, 16), 1000);
        assert_eq!(buffer_progress_milli(20, 16), 1000);
        // A zero target cannot happen through `arm`, but must not divide by 0.
        assert_eq!(buffer_progress_milli(0, 0), 0);
        assert_eq!(buffer_progress_milli(1, 0), 1000);
    }

    /// One raw DASH stream for the 画质 row tests.
    fn quality_option(qn: i64, codecid: i64, bandwidth: u64) -> bilibili::QualityOption {
        let (hdr, dolby_vision, eight_k) = (qn == 125, qn == 126, qn == 127);
        bilibili::QualityOption {
            qn,
            label: bilibili::quality_label(qn).to_string(),
            width: 1920,
            height: 1080,
            fps: 60.0,
            codecid,
            codec: bilibili::codec_label(codecid),
            bandwidth_bps: bandwidth,
            hdr,
            dolby_vision,
            eight_k,
            url: String::new(),
            backup_urls: Vec::new(),
        }
    }

    #[test]
    fn quality_rows_carry_the_formatted_detail_and_badge() {
        // `qn` 80 (1080P) carries no badge; 116 is the 大会员 tier.
        let stream = quality_option(80, 12, 4_200_000);
        assert_eq!(
            quality_detail(&stream),
            "1920×1080 · 60fps · HEVC · 4.2 Mbps"
        );
        assert_eq!(quality_badge(&stream), "");
        assert_eq!(quality_badge(&quality_option(116, 12, 4_200_000)), "会员");
        // Below 1 Mbps the detail keeps two decimals so it never reads 0.0.
        let low = quality_option(32, 7, 580_000);
        assert!(quality_detail(&low).ends_with("0.58 Mbps"));
        assert!(quality_detail(&low).contains("AVC"));
        // The badge precedence: HDR, 杜比, 8K, then 会员 for the other tiers.
        assert_eq!(quality_badge(&quality_option(125, 12, 9_000_000)), "HDR");
        assert_eq!(quality_badge(&quality_option(126, 12, 9_000_000)), "杜比");
        assert_eq!(quality_badge(&quality_option(127, 13, 9_000_000)), "8K");
        assert_eq!(quality_badge(&quality_option(120, 13, 9_000_000)), "会员");
        assert_eq!(quality_badge(&quality_option(80, 7, 9_000_000)), "");
    }

    #[test]
    fn quality_rows_mark_the_exact_stream_that_is_playing() {
        // 1080P60 exists as AVC and HEVC at different bitrates: only the exact
        // (qn, codecid, bandwidth) triple may be `current`.
        let specs = quality_specs(&[
            quality_option(116, 12, 6_000_000),
            quality_option(116, 12, 4_200_000),
            quality_option(116, 7, 5_000_000),
            quality_option(80, 12, 2_000_000),
        ]);
        assert_eq!(specs.len(), 4);
        let current = Some((116, 12, 4_200_000));
        let rows = quality_rows(&specs, current);
        assert_eq!(
            rows.iter().filter(|row| row.current).count(),
            1,
            "exactly one row is current"
        );
        assert!(rows[1].current);
        assert!(!rows[0].current);
        assert!(!rows[2].current);
        // Nothing playing (no playurl yet): no row is marked.
        assert!(quality_rows(&specs, None)
            .iter()
            .all(|row| !row.current));
    }

    #[test]
    fn quality_pick_keeps_an_exact_stream_and_qn_only_clears_it() {
        let control = PlaybackControl::new();
        let specs = quality_specs(&[
            quality_option(116, 12, 6_000_000),
            quality_option(116, 12, 4_200_000),
        ]);
        control.set_quality_catalog(specs.clone());
        assert_eq!(control.quality_catalog(), specs);

        control.set_quality_exact(116, 12, 4_200_000);
        assert_eq!(control.exact_quality(), Some((116, 12, 4_200_000)));
        assert_eq!(control.quality(), 116);
        // The same pick again is a no-op, a different one restarts.
        control.set_quality_exact(116, 12, 4_200_000);
        assert_eq!(control.exact_quality(), Some((116, 12, 4_200_000)));
        // A `qn`-only caller drops the exact stream and keeps its old rule.
        control.set_quality(116);
        assert_eq!(control.exact_quality(), None);
        assert_eq!(control.quality(), 116);
        // A new video starts on the automatic rule.
        control.set_quality_exact(116, 12, 6_000_000);
        control.set_quality_initial(80);
        assert_eq!(control.exact_quality(), None);
        assert_eq!(control.quality(), 80);
    }

    #[test]
    fn buffering_api_publishes_progress_and_full_when_open() {
        let control = PlaybackControl::new();
        // Never armed: not buffering and progress reads full.
        assert!(!control.buffering());
        assert_eq!(control.buffer_progress(), 1.0);

        control.begin_buffering(16);
        assert!(control.buffering());
        assert_eq!(control.buffer_progress(), 0.0);
        control.note_frame_decoded(8);
        assert_eq!(control.buffer_progress(), 0.5);

        control.end_buffering();
        assert!(!control.buffering());
        assert_eq!(control.buffer_progress(), 1.0);
    }

    #[test]
    fn held_position_does_not_follow_the_audio_clock_while_buffering() {
        let control = PlaybackControl::new();
        control.set_position_ms(1234);
        // `monotonic_ms()` never returns the 0 sentinel (its first sample is 1),
        // so the clock below is usable no matter which test ran first.
        let generation = control.begin_audio_clock_generation();
        assert!(control.set_audio_clock_ms(generation, 99_000));
        // The fresh clock is extrapolated with the wall clock, so it may read a
        // millisecond or two past the sample.
        assert!(control.playback_position_ms() >= 99_000);
        control.begin_buffering(16);
        assert_eq!(control.playback_position_ms(), 1234);
        control.end_buffering();
        assert!(control.playback_position_ms() >= 99_000);
    }

    #[test]
    fn stale_audio_samples_are_rejected_after_a_seek() {
        let control = PlaybackControl::new();
        control.set_duration_ms(264_000);
        // The old ffplay owns this generation and has just reported itself.
        let old = control.begin_audio_clock_generation();
        assert!(control.set_audio_clock_ms(old, 14_570));
        assert_eq!(control.audio_clock().map(|ms| ms / 1000), Some(14));

        // A backward seek retires that process and drops its clock.
        control.seek(5_000);
        assert_eq!(control.audio_clock(), None);
        assert_ne!(control.audio_clock_seq(), old);

        // The replaced ffplay keeps emitting `-stats` for a moment; those
        // samples must not resurrect the clock the seek just dropped.
        assert!(!control.set_audio_clock_ms(old, 15_638));
        assert_eq!(control.audio_clock(), None);
        assert!(!control.set_audio_clock_ms(old.saturating_sub(1), 14_185));
        assert_eq!(control.audio_clock(), None);

        // The new session's own samples are accepted and stay near its target.
        let current = control.audio_clock_seq();
        assert!(control.set_audio_clock_ms(current, 5_020));
        let position = control.audio_clock().unwrap_or(0);
        assert!((5_020..6_000).contains(&position), "position {position}");
        assert_eq!(control.position_ms(), 5_000);

        // An audio-only restart (volume / mute) retires samples the same way.
        control.restart_audio();
        assert!(!control.set_audio_clock_ms(current, 6_000));
        assert_eq!(control.audio_clock(), None);
    }

    /// The size the glass tests decode at (the old fixed size; the worker no
    /// longer assumes it, so any even size works).
    const TEST_FRAME: FrameSize = FrameSize { w: 960, h: 540 };

    /// A solid, opaque frame in the exact buffer size the decoder produces.
    fn solid_frame(rgba: [u8; 4]) -> Vec<u8> {
        let mut frame = vec![0u8; TEST_FRAME.bytes()];
        for pixel in frame.chunks_exact_mut(4) {
            pixel.copy_from_slice(&rgba);
        }
        frame
    }

    #[test]
    fn video_glass_does_no_work_until_a_consumer_asks() {
        let control = PlaybackControl::new();
        let mut worker = VideoGlassWorker::new();
        let frame = solid_frame([40, 80, 120, 255]);

        // Disabled: no thumbnail, no publish, no retained buffer.
        worker.poll(&control, &frame, Instant::now(), TEST_FRAME);
        assert!(control.video_glass_frame().is_none());
        assert_eq!(control.video_glass_frames.load(Ordering::Relaxed), 0);
        assert_eq!(control.video_glass_us.load(Ordering::Relaxed), 0);

        // Asked for, but nothing published yet.
        control.set_video_glass_needed(true);
        assert!(control.video_glass_frame().is_none());

        let started = Instant::now();
        worker.poll(&control, &frame, started, TEST_FRAME);
        let published = control.video_glass_frame();
        assert!(published.is_some());
        let (pixels, width, height) = published.unwrap_or_default();
        assert_eq!((width, height), (VIDEO_GLASS_W, VIDEO_GLASS_H));
        assert_eq!(pixels.len(), VIDEO_GLASS_BYTES);
        // Averaging a constant image and blurring it leaves the colour intact.
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel, &[40, 80, 120, 255][..]);
        }
        assert_eq!(control.video_glass_frames.load(Ordering::Relaxed), 1);

        // ~10 fps gate: a second frame within the interval is skipped.
        worker.poll(&control, &frame, started + Duration::from_millis(20), TEST_FRAME);
        assert_eq!(control.video_glass_frames.load(Ordering::Relaxed), 1);
        worker.poll(&control, &frame, started + Duration::from_millis(150), TEST_FRAME);
        assert_eq!(control.video_glass_frames.load(Ordering::Relaxed), 2);

        // Released again: the buffer is dropped and the pipeline does no work.
        control.set_video_glass_needed(false);
        assert!(control.video_glass_frame().is_none());
        worker.poll(&control, &frame, started + Duration::from_millis(500), TEST_FRAME);
        assert_eq!(control.video_glass_frames.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn video_glass_downscale_and_blur_soften_a_hard_edge() {
        let control = PlaybackControl::new();
        let mut worker = VideoGlassWorker::new();
        control.set_video_glass_needed(true);

        // Left half black, right half white: a hard vertical edge.
        let mut frame = vec![0u8; TEST_FRAME.bytes()];
        for (index, pixel) in frame.chunks_exact_mut(4).enumerate() {
            let x = (index % TEST_FRAME.w as usize) as u32;
            let value = if x < TEST_FRAME.w / 2 { 0 } else { 255 };
            pixel.copy_from_slice(&[value, value, value, 255]);
        }
        worker.poll(&control, &frame, Instant::now(), TEST_FRAME);
        let pixels = match control.video_glass_frame() {
            Some((pixels, _, _)) => pixels,
            None => Vec::new(),
        };
        assert_eq!(pixels.len(), VIDEO_GLASS_BYTES);

        let red = |x: usize| pixels[x * 4];
        let edge = VIDEO_GLASS_W as usize / 2;
        // The blur bleeds across the edge instead of leaving a 0/255 step.
        assert!(red(edge - 1) > 0 && red(edge - 1) < 255);
        assert!(red(edge) > 0 && red(edge) < 255);
        // Far from the edge the colours survive.
        assert_eq!(red(0), 0);
        assert_eq!(red(VIDEO_GLASS_W as usize - 1), 255);
        // Alpha is opaque everywhere.
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel[3], 255);
        }
    }

    #[test]
    fn video_glass_downscales_every_frame_size() {
        // The decoder is no longer 960x540: the block size is computed per
        // axis, so any display target must work (and stay in bounds).
        for size in [
            FrameSize { w: 1124, h: 632 },
            FrameSize {
                w: 1920,
                h: 1080,
            },
            FrameSize { w: 480, h: 270 },
            FrameSize { w: 200, h: 120 },
            FrameSize { w: 96, h: 54 },
        ] {
            let mut src = vec![0u8; size.bytes()];
            for pixel in src.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[7, 9, 11, 255]);
            }
            let mut dst = vec![0u8; VIDEO_GLASS_BYTES];
            downscale_to_glass(&src, &mut dst, size);
            for pixel in dst.chunks_exact(4) {
                assert_eq!(pixel, &[7, 9, 11, 255][..], "size {size:?}");
            }
        }
    }

    /// The per-frame cost the UI thread pays at the 1920x1080 target: one copy
    /// of the RGBA frame into Slint's pixel buffer plus the image handle. The
    /// pacer moves the frame in without a second copy (see `push_frame`), so
    /// this is the whole UI-side bill; it must stay far below one 33 ms frame.
    #[test]
    fn ui_frame_copy_fits_the_1080p30_budget() {
        let size = FrameSize {
            w: 1920,
            h: 1080,
        };
        let source = vec![21u8; size.bytes()];
        let updates = 60u32;
        let started = Instant::now();
        for _ in 0..updates {
            let mut buffer =
                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size.w, size.h);
            buffer.make_mut_bytes().copy_from_slice(&source);
            std::hint::black_box(slint::Image::from_rgba8(buffer));
        }
        let avg_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(updates);
        let mb_per_s = (size.bytes() as f64 * f64::from(updates))
            / started.elapsed().as_secs_f64()
            / 1.0e6;
        crate::debug_log(&format!(
            "ui frame copy ms_avg={avg_ms:.3} mb_per_s={mb_per_s:.0} updates={updates} size={}x{}",
            size.w, size.h
        ));
        // One 1080p frame period is 33.3 ms; the copy must be a small part of it.
        assert!(avg_ms < 8.0, "1080p UI frame copy avg {avg_ms:.3} ms");
    }

    /// The UI-side half of an update: copy the thumbnail out and build the
    /// Slint image exactly like the mini player does. Budget is 1.5 ms per
    /// update at 10 fps; this is the exact call the panel wiring makes.
    #[test]
    fn video_glass_ui_update_stays_within_the_frame_budget() {
        let control = PlaybackControl::new();
        control.set_video_glass_needed(true);
        let mut worker = VideoGlassWorker::new();
        worker.poll(&control, &solid_frame([10, 120, 220, 255]), Instant::now(), TEST_FRAME);
        assert!(control.video_glass_frame().is_some());

        let updates = 200u32;
        let started = Instant::now();
        for _ in 0..updates {
            let (rgba, width, height) = control.video_glass_frame().unwrap_or_default();
            if width == 0 || height == 0 {
                continue;
            }
            let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
            buffer.make_mut_bytes().copy_from_slice(&rgba);
            std::hint::black_box(slint::Image::from_rgba8(buffer));
        }
        let avg_ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(updates);
        crate::debug_log(&format!(
            "video glass ui ms_avg={avg_ms:.4} updates={updates} size={}x{}",
            VIDEO_GLASS_W, VIDEO_GLASS_H
        ));
        assert!(avg_ms < 1.5, "video glass UI update avg {avg_ms:.3} ms");
    }
}
