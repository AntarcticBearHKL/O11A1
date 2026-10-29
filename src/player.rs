//! Video playback driven by child processes.
//!
//! `ffmpeg.exe` decodes the referer-gated DASH video stream to raw NV12 at the
//! **source size capped at 1080p** (see [`target_frame_size`]);
//! `ffplay.exe` plays the audio track. Neither library is linked — both are
//! spawned with `CREATE_NO_WINDOW`. Frames are handed to the player window
//! through `slint::invoke_from_event_loop`, so the UI thread never blocks: the
//! reader converts straight into the buffer the UI will show, and a mailbox
//! keeps only the newest pending frame so a busy UI composites fresh pictures
//! instead of a backlog. Audio is best-effort: any failure degrades to
//! video-only.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use regex::Regex;
use slint::{ModelRc, SharedString, VecModel};

use crate::danmaku::Danmaku;
use crate::prefetch::{self, PrefetchConfig, PrefetchHandle, PrefetchStats};
use crate::{
    bilibili, AudioEntry, AudioQualityRow, DanmakuItem, DanmakuRow, MiniPlayerWindow, PlayerWindow,
    QualityEntry, QualityRow,
};

// Block-matching interpolation is the pacer's private dependency. It lives
// under the player module instead of the crate root so enabling it needs no
// change in `main.rs`; only the mode value has to travel through
// `PlaybackControl` (see `set_interp_mode`).
#[path = "motion.rs"]
mod motion;
// Hardware optical flow (mode 3). Same reasoning: it is the pacer's private
// dependency, reachable through `PlaybackControl`'s mode byte only.
#[path = "ofa.rs"]
mod ofa;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A track counts as finished when the playhead is this close to the duration;
/// the last frame is presented slightly before the pacer reads end-of-stream.
const TRACK_END_SLACK_MS: u64 = 300;
/// How far from the reported duration a decoder end-of-stream may land before
/// it is treated as a read failure instead of the real end of the track.
const TRACK_END_FAILSAFE_MS: u64 = 3000;
/// A playhead that reached the duration may only latch "ended" on its own once
/// no frame has been presented for this long. Without this a position/duration
/// mismatch (a short API duration, a stale audio clock) ended a video that was
/// still playing — and, being sticky, it stayed ended.
const END_POSITION_IDLE_MS: u64 = 1500;
/// The decoder's own `Duration:` replaces the API value only when the two
/// disagree by more than this fraction of the API value.
const DURATION_SOURCE_TOLERANCE: f64 = 0.02;
/// Upper bound on a duration either source may publish (24 h); anything larger
/// is a parse artefact, not a video.
const DURATION_MAX_MS: u64 = 24 * 60 * 60 * 1000;

/// `BILI_END_TEST=<percent>`: shorten the reported duration by this much, so
/// the false "播放完成" can be reproduced on a healthy video without waiting
/// for a real duration/position mismatch. `0` (the normal state) disables it.
fn end_test_shorten_percent() -> u64 {
    static PERCENT: OnceLock<u64> = OnceLock::new();
    *PERCENT.get_or_init(|| {
        std::env::var("BILI_END_TEST")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|percent| (1..=90).contains(percent))
            .unwrap_or(0)
    })
}

/// The control of the most recently opened player, reachable from the tray
/// thread (which cannot capture the UI-thread `Rc<PlayerState>`). Used only by
/// the boss key to mute/unmute playback while the main window is hidden.
static ACTIVE_CONTROL: std::sync::Mutex<Option<Arc<PlaybackControl>>> = std::sync::Mutex::new(None);

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
// f176: decode follows the stream, so the cap is the largest tier this
// client offers (4K). Anything bigger is still scaled down to keep the raw
// NV12 pipe from growing past ~50 MB/s at 30 fps.
const FRAME_MAX_W: u32 = 3840;
const FRAME_MAX_H: u32 = 2160;
/// Display fallback before the UI has published a window size.
const DEFAULT_DISPLAY_W: u32 = 1280;
const DEFAULT_DISPLAY_H: u32 = 720;
/// How much faster than realtime the reader may fill the ring once it is above
/// its low-water mark. 1.25× keeps a decoded reserve growing (the ring starts
/// half full) while capping the post-open/post-seek burst that used to run at
/// whatever ffmpeg could decode.
const READER_RATE_LIMIT: f64 = 1.25;

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

/// The frame size to decode for a `source` track.
///
/// The target is a property of the **source**, never of the window: a window
/// resize or a fullscreen toggle must not change what is decoded (and therefore
/// must not restart the decoder). The rule is:
///
/// * never larger than the source (no upscaling),
/// * never larger than [`FRAME_MAX_W`]x[`FRAME_MAX_H`],
/// * aspect preserved, even width and height.
///
/// A zero source (unknown / audio-only) falls back to the 720p default.
///
/// f176: automatic opens are no longer capped. The old `AUTO_QUALITY_CAP`
/// (1080P) is gone with the request "open a video on the best source"; the
/// stream list already only carries tiers this account may open.

/// The 画质 a player starts one video on.
///
/// `preferred` is 设置 → 播放 → 默认画质: a concrete `qn` the user chose
/// (honoured as-is, 4K included — that is an explicit instruction) or `0` for
/// 自动.
///
/// 自动 is `min(highest playable, 1080P)`, not "the best stream": a 4K/8K source
/// would otherwise open at 4K on every video by itself, which is both a much
/// heavier decode and a picture this window cannot show. A tier above the cap
/// is used only when the video has nothing at or below it (so a 1080P60-only
/// source still plays).
pub fn initial_quality(info: &bilibili::PlayInfo, preferred: i64) -> i64 {
    if preferred != 0 && info.video_tracks.iter().any(|track| track.id == preferred) {
        return preferred;
    }
    auto_quality(&info.video_tracks)
}

/// The **highest playable qn** in the stream list.
///
/// f176: the owner asked for "open a video on the best source available", so
/// auto no longer caps at 1080P. The list only contains tracks this account may
/// actually open (locked 大会员 tiers have no `dash.video[]` entry), so the max
/// is by construction playable; `AUTO_QUALITY_CAP` stays as documentation of
/// the old ceiling and is no longer used for selection.
fn auto_quality(tracks: &[bilibili::VideoTrack]) -> i64 {
    tracks
        .iter()
        .map(|track| track.id)
        .filter(|id| *id > 0)
        .max()
        .unwrap_or(0)
}

/// The frame size a session decodes at: the source, capped at 1080p.
fn target_frame_size(source_w: u32, source_h: u32) -> FrameSize {
    // Even floor/round: integer sizes keep the byte stride exact.
    fn even_down(value: u32) -> u32 {
        (value & !1).max(2)
    }
    fn even_round(value: f64) -> u32 {
        let rounded = value.round().max(2.0) as u32;
        (rounded & !1).max(2)
    }

    let (source_w, source_h) = if source_w < 2 || source_h < 2 {
        (DEFAULT_DISPLAY_W, DEFAULT_DISPLAY_H)
    } else {
        (source_w, source_h)
    };
    let scale = (f64::from(FRAME_MAX_W) / f64::from(source_w))
        .min(f64::from(FRAME_MAX_H) / f64::from(source_h))
        .min(1.0);
    if scale >= 0.999 {
        // Source fits: decode natively (only made even).
        return FrameSize {
            w: even_down(source_w),
            h: even_down(source_h),
        };
    }
    // Height is the constraining axis for a 16:9 source capped at 16:9; derive
    // the width from the source aspect so the pair stays exact to within one
    // pixel instead of accumulating two independent roundings.
    let h = even_round(f64::from(source_h) * scale);
    let w = even_round(f64::from(h) * f64::from(source_w) / f64::from(source_h));
    FrameSize {
        w: w.min(even_down(FRAME_MAX_W)),
        h: h.min(even_down(FRAME_MAX_H)),
    }
}

/// Frame bytes the decode-ahead ring may hold. A fixed frame count is wrong
/// once the frame size varies: 32 frames of 1080p RGBA are 265 MB, 32 frames of
/// 480p only 33 MB. The budget keeps one player around 128 MB while still
/// buffering a useful amount of video: 16 frames of 1080p (533 ms at 30 fps,
/// 640 ms at 25 fps), which is what makes the 150/600 ms buffer guard physically
/// reachable instead of flapping against a 300 ms ceiling.
const RING_BUDGET_BYTES: usize = 128 << 20;
const RING_MIN_CAPACITY: usize = 4;
const RING_MAX_CAPACITY: usize = 32;

/// Frames the ring holds for `frame_bytes`-sized frames: the 64 MB budget
/// divided by one frame, clamped to `4..=16`.
fn ring_capacity_for(frame_bytes: usize) -> usize {
    if frame_bytes == 0 {
        return RING_MAX_CAPACITY;
    }
    (RING_BUDGET_BYTES / frame_bytes).clamp(RING_MIN_CAPACITY, RING_MAX_CAPACITY)
}

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
// A fresh session holds at its first available frame instead of starting to
// move while the buffer is still thin. Which signal opens the gate depends on
// whether the prefetch proxy is in front of the stream: with the proxy the gate
// waits for its cache (see the cache-based gate below), without one it waits
// for the ring to hold enough video.
//
// The ring rule is only a yardstick, never a promise: the ring capacity follows
// the frame size (`ring_capacity_for`), so a 1080p session holds 15 frames
// (~0.5 s) and a smaller source up to 32. The gate always asks for at most what
// the ring can hold - which is exactly why the proxy cache is the better signal
// when there is one.

/// Frames of ring occupancy that must be decoded before the first frame is
/// presented. ~0.5 s at 30 fps: enough that the pacer never has to pop from an
/// empty ring in the first moments, which is what the first-seconds stutter
/// was. Clamped to the ring's own capacity per session.
const PRE_ROLL_START_FRAMES: usize = 15;
/// Ceiling on the ring fill of the startup handover; a slow source starts with
/// whatever it has instead of holding the overlay forever.
const PRE_ROLL_TIMEOUT_MS: u64 = 4000;
/// Same gate after a watchdog-confirmed stall: ring full, 2 s ceiling.
const PRE_ROLL_RECOVERY_TIMEOUT_MS: u64 = 2000;
/// Poll period of an armed gate: short enough that pause / seek / close react
/// quickly, long enough to be free while the ring fills.
const PRE_ROLL_POLL_MS: u64 = 20;

// ---- Startup stages ------------------------------------------------------
//
// The loading overlay's bar is one monotonic 0..1 value made of four named
// stages, each with its own sub-signal, so it moves from the first millisecond
// instead of jumping 0 → 100 when the cache crosses a threshold:
//
//   stage      bar        sub-signal
//   Bytes      0 - 40 %   proxy cache ahead of the start position / need
//   Decode     40 - 55 %  decoder spawned, frames decoded until the first one
//   Audio      55 - 75 %  ffplay resumed, age of its first clock sample
//   PreRoll    75 - 99 %  ring occupancy toward `PRE_ROLL_START_FRAMES`
//
// The bar is `max` over the four stages' own sub-progress, so a stage that
// finishes early cannot be dragged back, and `100 %` is published only when the
// first frame is really handed to the UI (`StartupStage::OPEN_MILLI`).
//
// The first frame is not presented until the sound is ready: the audio clock
// fresh, the audio audible (not the silent seed), and |clock − picture| inside
// `STARTUP_SYNC_TOLERANCE_MS`. After `STARTUP_AUDIO_TIMEOUT_MS` the picture
// starts muted instead (logged `startup audio timeout`); that path is the
// exception, not the rule.

/// How old the first audio clock sample may be before the picture starts anyway.
const STARTUP_AUDIO_TIMEOUT_MS: u64 = 3_000;
/// Spawn → first clock sample that the Audio stage's bar accounts for.
/// `ffplay` needs most of a second to fill its device buffer and report its
/// first position (measured 600-1400 ms on this machine), so the bar sweeps the
/// stage over that window; the measured age always wins once it is longer.
const STARTUP_AUDIO_EXPECT_MS: u64 = 1_200;
/// Decoder spawn → first frame that the Decode stage's bar accounts for
/// (`ffmpeg` probe + keyframe, measured ~1.5 s).
const STARTUP_DECODE_EXPECT_MS: u64 = 1_500;
/// |audio_clock − video_position| accepted when the first frame is presented.
const STARTUP_SYNC_TOLERANCE_MS: i64 = 50;
/// How fast the displayed bar moves toward the measured value, bar-units (of
/// 1000) per millisecond. 3.0 = the whole bar in ~330 ms, faster than any
/// startup's last stage and far enough above 10 Hz that every step is visible.
const STARTUP_SLEW_PER_MS: f32 = 3.0;
/// Time over which the Bytes stage's bar creeps to 90 % of its slice when the
/// proxy has not written the first chunk yet. Only fills the frozen gap: real
/// bytes always dominate as soon as they arrive.
const STARTUP_BYTES_CREEP_MS: u64 = 1_500;
/// Cadence of the `startup stage=… t=… pct=…` trace. The bar itself is
/// published on every 20 ms gate poll; the trace is 20 Hz so a startup can be
/// replayed from the log at finer resolution than the 10 Hz the UI needs.
const STARTUP_TRACE_MS: u64 = 50;
/// How long a finished startup keeps reading 100 % (and the overlay up), so the
/// UI's "is there a picture yet" test cannot flash the bar back to 0 in the tick
/// between the handover and its own composite of that frame.
const STARTUP_CLOSING_MS: u64 = 100;

/// The four startup stages, in presentation order. The discriminants are the
/// order the detail text and the sub-progress array use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum StartupStage {
    Bytes = 0,
    Decode = 1,
    Audio = 2,
    PreRoll = 3,
}

impl StartupStage {
    const ALL: [StartupStage; 4] = [
        StartupStage::Bytes,
        StartupStage::Decode,
        StartupStage::Audio,
        StartupStage::PreRoll,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// Published name of the stage (`startup stage=Bytes` and the trace).
    fn label(self) -> &'static str {
        match self {
            StartupStage::Bytes => "Bytes",
            StartupStage::Decode => "Decode",
            StartupStage::Audio => "Audio",
            StartupStage::PreRoll => "PreRoll",
        }
    }

    /// The slice of the bar this stage owns, as `(start, end)` fractions.
    ///
    /// `PreRoll` stops at 0.99, so a running startup never reads 100 %: the last
    /// percent belongs to the frame actually being presented.
    fn range(self) -> (f32, f32) {
        match self {
            StartupStage::Bytes => (0.00, 0.40),
            StartupStage::Decode => (0.40, 0.55),
            StartupStage::Audio => (0.55, 0.75),
            StartupStage::PreRoll => (0.75, 0.99),
        }
    }
}

/// The bar value, thousandths, implied by the four stage sub-values.
///
/// The bar always sits inside the slice of the **first stage that is not
/// complete** - the same stage [`startup_detail`] names - so the percentage and
/// the text can never disagree ("取流 42 %" while the bar shows audio progress).
/// The publisher slews the bar toward this value, which turns an instant stage
/// completion (the cache crossing its threshold between two polls) into a
/// visible sweep. Pure, so the mapping is unit-tested.
fn startup_overall_milli(subs: &[u32; 4]) -> u32 {
    let mut value = 0.99f32;
    for stage in StartupStage::ALL {
        let raw = subs[stage.index()].min(1000);
        if raw >= 1000 {
            continue;
        }
        let (start, end) = stage.range();
        value = start + (end - start) * raw as f32 / 1000.0;
        break;
    }
    // 100 % is reserved for `finish_startup`: the overlay closes on the frame
    // itself, so a running bar never claims to be complete.
    (value * 1000.0).round().clamp(0.0, 999.0) as u32
}

/// The line under the loading animation for the current stage: the stage that
/// is not complete yet, in order. Pure, so the text contract is unit-tested.
fn startup_detail(subs: &[u32; 4], ring_s: f32) -> String {
    for stage in StartupStage::ALL {
        let sub = subs[stage.index()].min(1000);
        if sub >= 1000 {
            continue;
        }
        return match stage {
            StartupStage::Bytes => format!("取流 {}%", sub / 10),
            StartupStage::Decode => "解码中".to_string(),
            StartupStage::Audio => "准备音频".to_string(),
            StartupStage::PreRoll => format!("缓冲 {ring_s:.1}s"),
        };
    }
    "即将播放".to_string()
}

/// Drops the startup stage machine when the session that armed it ends without
/// presenting its first frame (a seek, pause, quality switch or decoder error).
///
/// The guard lives in the pacer's session scope, so every exit path - `break`,
/// `continue`, an early `return` - retires the overlay instead of leaving it up
/// on a session that no longer exists.
struct StartupGuard {
    control: Arc<PlaybackControl>,
    /// Cleared once the startup finished (or was aborted) explicitly.
    active: bool,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.active && self.control.startup_active() {
            self.control.abort_startup("session-end");
        }
    }
}

// ---- Cache-based startup gate -------------------------------------------
//
// The ring pre-roll above can only ever wait for what the ring holds: 8-16
// frames, i.e. 0.3-0.5 s of video. The user still saw the first seconds
// stutter, and explicitly accepts a short wait instead ("等一会 / 不能卡顿").
// With the prefetch proxy in front of the stream the honest readiness signal is
// the proxy's own cache, not the ring: the gate now opens once enough of the
// stream ahead of the start position is in memory, so the decoder never has to
// outrun the network at the very beginning.
//
// Overlay, not jank: while the gate holds, no frame is pushed (the pacer pops
// nothing) and audio is not started, so the user sees the loading layer and
// then a picture that simply keeps running.

/// Cache ahead of the start position that opens the gate, bytes. Only binds on
/// high-bitrate tracks: 1080p at 2.5 Mbit/s needs ~0.6 MiB for the two seconds
/// below.
const STARTUP_READY_BYTES: u64 = 8 << 20;
/// Seconds of the track's declared bitrate that also open the gate. The smaller
/// of the two thresholds wins, so a low-bitrate track does not have to download
/// 8 MiB before it may start.
const STARTUP_READY_SECONDS: u64 = 2;
/// Ceiling on the cache gate. A slow link starts anyway (and logs that it did)
/// instead of showing the loading layer forever.
const STARTUP_READY_TIMEOUT_MS: u64 = 6_000;

/// How often the pacer turns the cumulative frame counters into the rates of
/// [`Stats`]. Deliberately not per frame: 4 Hz is plenty for a profiler panel
/// and keeps the pacing loop free of formatting and extra clock calls.
const PROF_TICK_MS: u64 = 250;
/// Samples kept by the profiler's sliding window (5 × 250 ms ≈ 1 s).
const PROF_WINDOW: usize = 5;
/// Shortest window span trusted as a rate; below it the sample is skipped so a
/// seek or respawn cannot produce a spurious spike.
const PROF_MIN_WINDOW_MS: u64 = 200;

/// Startup gate: background work (seek-point warming) is released only after
/// the player's first frame has been presented this long...
const GATE_PLAYING_MS: u64 = 5_000;
/// ...**and** the stream has been free of stall / auto-pause / buffering for at
/// least this long. Together they are the "healthy start" the proxy waits for.
const GATE_HEALTHY_MS: u64 = 3_000;

/// Per-frame pacing epsilon: every frame waits until the audio clock reaches
/// its presentation time. The old coarse "ahead" tolerance (120 ms) let the
/// loop drain 3-4 frames per sleep cycle and push only one of them, which
/// capped a 30 fps track at ~7 fps on screen.
const SYNC_PACE_EPS_MS: i64 = 5;
/// A frame this far behind the audio clock is dropped instead of pushed.
const SYNC_BEHIND_MS: i64 = 120;
/// How far the audio clock may sit from the picture's own timeline and still
/// set the pace. `wall_ms` is exact playback time, so a `-stats` clock that
/// runs slow (or a device that does) must never drag the frame rate with it:
/// outside this window the picture keeps its nominal rate and the A/V guard
/// owns the relation. Matches the lag the seed logic already accepts as device
/// latency ([`AUDIO_SEED_MAX_MS`]).
const SYNC_CLOCK_WINDOW_MS: i64 = AUDIO_SEED_MAX_MS;
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
/// Startup seed: after the FIRST presented frame the audio clock may still be
/// behind by its own device latency. Inside `AUDIO_SEED_WINDOW` a gap up to this
/// much is accepted (the picture is presented instead of waiting); a larger gap
/// is a real desync and still holds. Steady state is untouched once the window
/// ends.
const AUDIO_SEED_MAX_MS: i64 = 400;
const AUDIO_SEED_WINDOW: Duration = Duration::from_millis(1_200);
/// How long an audio-clock sample may be extrapolated (with the wall clock)
/// before positions fall back to the wall clock entirely. ffplay refreshes its
/// `-stats` line many times per second, so this only covers burst gaps.
const AUDIO_CLOCK_FRESH_MS: u64 = 1500;
/// Playback the master clock's rate correction is spread over. A new `-stats`
/// sample is adopted as a *rate* correction closing the measurement error over
/// this much playback, never as a step in the clock itself; at the 2 % cap that
/// is a one-frame-per-few-seconds correction instead of a hold of the whole
/// error, which is what the picture used to pay as a dip.
const AUDIO_CLOCK_SLEW_MS: u64 = 2_000;
/// Hard cap on that correction: 2 % of playback speed, so a bogus sample (or a
/// real 100 ms lurch) can never run the clock away; 100 ms of error closes in
/// 5 s.
const AUDIO_CLOCK_MAX_PPM: f64 = 20_000.0;
/// A sample this far ahead of the extrapolation is a real discontinuity (a
/// resumed process, a seek inside ffplay, a long decode stall), not `-stats`
/// jitter: it is adopted at once, because a 2 % correction would need tens of
/// seconds to close it while the sound ran away from the picture. Forward
/// adoption costs the pacer nothing (it just stops holding), so the bar is low.
const AUDIO_CLOCK_JUMP_MS: u64 = 120;
/// A sample this far *behind* the extrapolation is adopted too, but the bar is
/// higher: adopting it makes the pacer hold the picture for that long, i.e.
/// lose that many frames, so only a guard-scale discontinuity (the sound really
/// is a third of a second behind the picture) is worth it. Measured `-stats`
/// jitter is ±150 ms at worst, and that stays on the slew path.
const AUDIO_CLOCK_LURCH_MS: u64 = 250;
/// A forward sample this far ahead of the extrapolation is a **jump**: it is
/// never adopted on its own. Both a device artefact and a real skip inside
/// `ffplay` arrive as one such report, and they are indistinguishable until the
/// next `-stats` burst. Measured (the owner's t≈6 s dip): a 629 ms buffer pause
/// made `ffplay`'s first resumed sample report the audio **device** position
/// 629 ms ahead of the content; adopting it put the clock 525 ms ahead of the
/// picture, the A/V guard suspended the sound for drift, the ring drained and
/// the pacer presented 17 frames in a 1009 ms second (16.8 fps), then 3.8 fps.
/// The report is kept as a candidate instead (see `set_audio_clock_ms`): the
/// clock keeps its trajectory, so a rejected sample can never reach the guard.
const AUDIO_CLOCK_GUARD_MS: u64 = 400;
/// Two consecutive reports this close to each other **at the new base** confirm
/// a forward jump as real, and the clock is re-seeded there. A skip inside
/// `ffplay` looks exactly like that; the device artefact does not (the next
/// burst returns to the content position).
const AUDIO_CLOCK_CONFIRM_MS: u64 = 150;
/// How long after a suspension is released `ffplay` still reports the audio
/// **device** position. Its first `-stats` line then reads "frozen content +
/// the suspension's length" (measured +1.03 s for a 1.2 s suspension) and the
/// next line returns to the content. Inside this window a forward jump is the
/// device, never the content, so it is dropped without even becoming a
/// candidate ([`SHADOW_RESUME_QUIET_MS`] does the same for a seek shadow's own
/// process). Confirmation is deliberately not offered here: two device samples
/// in a row must never re-seed the clock either.
const AUDIO_CLOCK_RESUME_QUIET_MS: u64 = 800;
/// A seek's silent window used to be decided from the proxy's read-ahead while
/// the old picture kept playing. f151 freezes the picture on the release, so the
/// overlay is now published purely by time (`SEEK_FREEZE_OVERLAY_MS`) and the
/// old 450ms read-ahead grace is gone.
///
/// Seconds of video the seek overlay treats as "enough buffered to start".
/// Used only to turn the proxy's measured read-ahead bytes into an honest
/// percentage; it is not a claim about the network.
const SEEK_BUFFER_WINDOW_S: f64 = 2.0;
/// How often the pacer re-tries applying the stored gain/mute to an audio
/// session that is not up yet. `ffplay` creates its WASAPI session a few
/// milliseconds after spawn, so the first attempt usually misses.
const AUDIO_SESSION_RETRY_MS: u64 = 100;
/// The audio clock may lead the presented picture by this much before the A/V
/// guard freezes the audio process (`av guard action=pause`). The sound is
/// slaved to the picture: a paused (or merely slower) picture must never leave
/// the audio clock running, which is what grew a 2–7 s lead.
const AV_LEAD_MS: i64 = 300;
/// Release bound of a drift pause. While the process is suspended its clock is
/// frozen, so the picture closes the gap at its own rate; the sound is released
/// once it is no longer ahead, which gives the guard hysteresis instead of a
/// single edge.
const AV_RESUME_LEAD_MS: i64 = 0;
/// How long a drift pause may last before the picture is declared unable to
/// catch up. Only then is the audio process restarted — once, at the picture
/// position. A suspend does not lose the content position, so every pause that
/// can be released inside this window is released in place.
const AV_CATCHUP_MS: u64 = 1_500;
/// Minimum spacing between two guard-driven audio restarts (see
/// [`AV_CATCHUP_MS`]): one restart is a correction, two in quick succession are
/// the thrash the guard exists to prevent.
const AV_RESTART_COOLDOWN: Duration = Duration::from_secs(5);
/// A lead this large is a desync, not drift: the guard counts one *episode*
/// each time the relation crosses it.
const AV_THRASH_LEAD_MS: i64 = 1_000;
/// Two big-lead episodes inside this window after a correction mean the
/// correction does not hold: the guard logs `av thrash` and stops restarting
/// the audio (the picture takes the correction instead).
const AV_THRASH_WINDOW: Duration = Duration::from_secs(30);

/// A/V guard state as the profiler line publishes it: no audio clock at all,
/// running, or suspended (`av=off|run|hold`).
const AV_OFF: u64 = 0;
const AV_RUN: u64 = 1;
const AV_HOLD: u64 = 2;
/// Buffered decode-ahead (ms) below which the pacer auto-pauses and shows the
/// buffering overlay instead of playing choppy. Auto-resume waits for the
/// higher threshold below.
const AUTO_PAUSE_LEAD_MS: f64 = 150.0;
/// Buffered decode-ahead (ms) that releases an auto pause. Capped by what the
/// ring can physically hold (see [`auto_buffer_thresholds`]).
const AUTO_RESUME_LEAD_MS: f64 = 600.0;
/// At most one auto pause/resume transition per this window, so a lead that
/// hovers around a threshold cannot flap the UI.
const AUTO_TRANSITION_MIN_MS: u64 = 500;
/// A session's first frames come out of an empty ring (a seek has just
/// restarted the decoder), so the buffer guard stays quiet this long after a
/// session starts; otherwise every seek would immediately auto-pause.
const AUTO_PAUSE_START_GRACE_MS: u64 = 1_500;
/// An auto pause that cannot recover within this long is a stall: restart the
/// decoder instead of holding the buffering overlay forever.
const AUTO_PAUSE_TIMEOUT_MS: u64 = 15_000;
/// Stuck auto pause: no new ring high-water for this long is a stall even
/// though the overall timeout has not expired yet. Three seconds is the
/// longest a viewer should ever see a frozen picture with a healthy network.
const AUTO_PAUSE_STUCK_MS: u64 = 3_000;
/// A closed gate whose own condition has been satisfied for this long is
/// forced open. The overlay is a promise about the buffer, never a trap: a
/// stuck one must be impossible even if the pacer thread is busy or dead.
const GATE_FORCE_OPEN_MS: u64 = 1_000;
/// A repeated seek to the same target inside this window is ignored: the UI
/// may deliver a release seek after a drag seek for the same position.
const SEEK_DEDUPE_MS: u64 = 400;
/// How long a seek shadow may pre-roll before the seek falls back to the hard
/// restart. A seek must always land, so the shadow can never own the wait.
const SEEK_SHADOW_TIMEOUT_MS: u64 = 3_000;
/// f151: how long a release's picture stays frozen **silently** before the
/// buffering overlay is published. A shadow that is ready inside this window
/// swaps straight in and the user never sees the overlay; after it, the wait
/// needs to be visible (`§1`).
const SEEK_FREEZE_OVERLAY_MS: u64 = 300;
/// How often the frozen seek loop re-polls its shadow. The picture is not being
/// presented, so this only trades a few wakeups for a swap that starts within a
/// frame of the shadow becoming ready.
const SEEK_FREEZE_POLL_MS: u64 = 4;
/// Longest A/V hold the pacer may sleep while a seek shadow is pre-rolling.
///
/// The normal hold is up to 500 ms, which also delayed the *poll* that would
/// have swapped the pre-rolled session: a ready shadow waited behind a sync
/// sleep of the picture it was replacing. 50 ms bounds that delay while keeping
/// the old picture's relation within the acceptance lane; the adopted session's
/// own guard owns the relation from the first new frame.
const SEEK_SHADOW_SYNC_HOLD_MS: u64 = 50;
/// The lane width of a shadow's swap decision: its `ffplay` clock must be this
/// close to the target frame before the swap is allowed (`§2` acceptance).
const SEEK_SHADOW_SYNC_MS: i64 = 50;
/// After a suspended `ffplay` is resumed, its first `-stats` line reports the
/// **device** position — measured +1.03 s for a 1.2 s suspension — not the
/// content position it was frozen at (the next line returns to it). Samples that
/// jump forward inside this window are the device, never the content, so they
/// are dropped instead of being adopted as the session's clock.
const SHADOW_RESUME_QUIET_MS: u64 = 400;
/// A shadow audio process that has not reported a position by this age is
/// accepted at the target it was spawned at (`unclocked`), so a silent `ffplay`
/// cannot hold a seek past its ceiling.
const SEEK_SHADOW_AUDIO_WAIT_MS: u64 = 2_500;
/// Ring frames a seek shadow needs before it may swap.
///
/// This is deliberately much smaller than the start-of-playback pre-roll
/// ([`PRE_ROLL_START_FRAMES`]): a shadow's decoder keeps filling its ring at
/// 2–3× realtime after the swap, so the pacer does not need half a second of
/// video in hand before the first frame. Six frames are ~200 ms of content
/// (plus a decode margin) and remove the measured 260–450 ms that the
/// 15-frame pre-roll added to every cached seek. Clamped to the ring's
/// capacity per session, like every other pre-roll target.
const SEEK_SHADOW_PREROLL_FRAMES: usize = 6;
/// How long a **local** shadow waits for `ffplay`'s first position before it
/// accepts the request position instead.
///
/// `ffplay -ss` has the sound laid down by construction, but its first `-stats`
/// sample is 500–800 ms away even on a cache file (the device buffer has to
/// fill); that would become the whole seek latency once the ring only waits for
/// six frames. A cached seek is picture-bound, so once the picture is ready (and
/// the sound is provably muted) the shadow freezes the sound where it is (or at
/// the target when it never reported) and the session seeds its clock from the
/// request. The grace is only long enough for a sample that was already in
/// flight; f151 cut it from 300 ms to 40 ms so a local shadow can actually land
/// inside the release's 300 ms silent window. Network shadows keep
/// [`SEEK_SHADOW_AUDIO_WAIT_MS`]: there the connection warm-up is real work and
/// the A/V relation is worth the longer wait.
const SEEK_SHADOW_LOCAL_AUDIO_WAIT_MS: u64 = 40;

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

/// Burst guard for UI pushes in the plain path: a quarter of a frame period,
/// clamped to 8..=100 ms. The pacing sleep above already spaces frames one
/// period apart; this only stops a burst when the audio clock micro-jumps
/// forward.
fn push_interval_for(frame_ms: f64) -> Duration {
    if frame_ms.is_finite() && frame_ms > 0.0 {
        Duration::from_secs_f64((frame_ms / 1000.0 * 0.25).clamp(0.008, 0.100))
    } else {
        MIN_FRAME_INTERVAL
    }
}

// ---- Frame-blending interpolation ---------------------------------------
//
// The cheapest "smooth motion": when the display refreshes faster than the
// track, the pacer presents display frames *between* decoded source frames by
// linearly blending the two nearest ones. It costs one blend pass and one
// extra UI push per display frame, requests no extra frames from the decoder
// and never changes the clock or the reported position: interpolated frames
// are display-only.
//
// The phase is derived from the audio clock (the same master the pacer already
// uses), so the blended picture is locked to the sound rather than to a free
// running counter. Off is the zero-cost path: one relaxed atomic load per
// presented source frame and nothing else.
//
// Mode 2 (`BlockMatch`) keeps the same display grid and the same pacer, but the
// one slot nearest the middle of each source interval is produced by the
// block-matching motion path in [`motion`] instead of a linear blend; every
// other slot still blends exactly as mode 1 does. Motion is attempted at most
// once per source interval and only while the measured pair cost fits the
// display slot, so the pacer can never be held longer than one frame.

// Mode 3 (`Ofa`) keeps that same display grid, but every slot of an interval
// is warped from a dense flow field produced by the GPU's optical-flow engine
// (see [`ofa`]). The engine has no fence in its D3D11 binding, so the field of
// a pair is readable only one interval later; the pacer therefore draws an
// interval from the *previous* pair, which costs exactly one pair of latency.

/// Modes accepted by [`PlaybackControl::set_interp_mode`]. The enum is
/// `#[repr(u8)]`; its discriminants are the persisted `settings::interp_mode`
/// values, so the UI's `(mode + 1) % 4` cycle and this type agree by
/// construction.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpMode {
    Off = 0,
    Blend = 1,
    BlockMatch = 2,
    Ofa = 3,
}

/// The persisted value of [`InterpMode::Ofa`], for the window's cycle, which
/// has to skip this step when the runtime is missing.
pub const INTERP_MODE_OFA: u8 = InterpMode::Ofa as u8;

impl InterpMode {
    /// Snap a stored/UI value to a mode. Unknown values fall back to off, so a
    /// corrupt `settings.json` can never change how the pacer runs.
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Blend,
            2 => Self::BlockMatch,
            3 => Self::Ofa,
            _ => Self::Off,
        }
    }

    fn as_u8(self) -> u8 {
        self as u8
    }

    /// The label the player window shows for this mode.
    pub fn text(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Blend => "混合",
            Self::BlockMatch => "块匹配",
            Self::Ofa => "OFA",
        }
    }
}

/// Output ceiling for every mode and every frame size: 60 fps.
///
/// The user's decision: 平滑 is a 1080p-and-below feature at up to 60 fps
/// output. The display rate is still read (it decides whether there is
/// anything to generate at all, and it defines the phase grid), but the
/// output grid is `min(60, display_hz)`. Measured (`spike_blend_pipe_at_60hz`):
/// a 1080p display tick costs ~14 ms of a 16.7 ms slot (blend + the Slint
/// copy, p95 11.8 ms), so 60 is the only rate that holds at that size, and a
/// single ceiling for all sizes is one budget the UI can promise.
const INTERP_MAX_OUT_HZ: u32 = 60;
/// Blending only pays off when the output is meaningfully faster than the track;
/// a 60 Hz display against a 59.94 fps track is not worth a full-frame pass.
const INTERP_MIN_GAIN: f64 = 1.01;
/// Pair-time histogram for the QA line: 0.5 ms buckets up to 32 ms.
const BM_HIST_BUCKETS: usize = 64;
const BM_HIST_BUCKET_MS: f32 = 0.5;

/// `(p50, p95)` of a pair-time histogram, ms (each value is its bucket's upper
/// edge). Shared by the block-matching and optical-flow counters, whose pair
/// times have the same range and the same QA purpose.
fn percentiles(hist: &[AtomicU64; BM_HIST_BUCKETS]) -> (f32, f32) {
    let total: u64 = hist.iter().map(|bucket| bucket.load(Ordering::Relaxed)).sum();
    if total == 0 {
        return (0.0, 0.0);
    }
    let pick = |percent: f64| -> f32 {
        let mut seen = 0u64;
        for (index, bucket) in hist.iter().enumerate() {
            seen += bucket.load(Ordering::Relaxed);
            if seen as f64 >= (total as f64 * percent).ceil() {
                return (index + 1) as f32 * BM_HIST_BUCKET_MS;
            }
        }
        BM_HIST_BUCKETS as f32 * BM_HIST_BUCKET_MS
    };
    (pick(0.50), pick(0.95))
}

/// The resolved interpolation rate for one decode session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterpPlan {
    out_hz: u32,
    /// Why `out_hz` was clamped below the display rate (for the log line).
    reason: Option<&'static str>,
}

/// Resolve the output rate for a session, or `None` when blending would be
/// pointless (the display is not faster than the track).
///
/// The caps, enforced here rather than in the caller:
/// * the output grid is `min(60, display_hz)` for **every** mode and every
///   frame size — there is no 120 fps path any more (see
///   [`INTERP_MAX_OUT_HZ`]);
/// * a display faster than 60 logs `blend cap …`;
/// * below 60 the display rate is kept as-is, so the grid still matches the
///   phase the compositor presents at.
fn interp_plan_for(display_hz: u32, track_fps: f64) -> Option<InterpPlan> {
    if !(track_fps.is_finite() && track_fps > 1.0) {
        return None;
    }
    let out_hz = display_hz.min(INTERP_MAX_OUT_HZ);
    if out_hz as f64 <= track_fps * INTERP_MIN_GAIN {
        // The track already fills the display: no frames to generate.
        return None;
    }
    Some(InterpPlan {
        out_hz,
        reason: (display_hz > INTERP_MAX_OUT_HZ).then_some("60fps_cap"),
    })
}

/// Whether frame generation must pause because a window is being dragged.
///
/// The move is applied by the app's own 8 ms timer (`win::WindowDrag`), one
/// `SetWindowPos` per display frame. Every *pushed* picture invalidates the
/// whole player window, so a 60 fps interpolated stream asks for a full 1080p
/// recomposite (measured ~8-14 ms) on top of the move, twice per decoded
/// frame; that is the judder the user reported. While a drag is in flight the
/// pacer therefore presents the source frames (one push per decoded frame, no
/// blend, no motion pair, no OFA upload) and rebuilds the display grid from
/// the video timeline when the drag ends. Audio, the decoder, the ring and the
/// clock are untouched, and nothing is restarted.
///
/// `BILI_DRAG_INTERP=1` keeps interpolation running during the drag; that is
/// the before/after baseline for the drag measurements, not a product mode.
fn interp_paused_for_drag() -> bool {
    static DRAG_INTERP_ALWAYS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *DRAG_INTERP_ALWAYS.get_or_init(|| {
        std::env::var("BILI_DRAG_INTERP")
            .map(|value| value == "1")
            .unwrap_or(false)
    }) {
        return false;
    }
    crate::win::window_drag_active()
}

/// Whether the block-matching motion path may run for this frame size.
///
/// Above 1280x720 the cost of the search + warp exceeds the display slot on
/// this machine, so mode 2 degrades to plain blending (the pacer logs
/// `blockmatch unsupported frame=WxH → blend` once per session). The output
/// grid itself is the same 60 fps one every mode uses (see [`interp_plan_for`]).
fn interp_motion_supported(size: FrameSize) -> bool {
    motion::MotionInterp::supports(size.w, size.h)
}

/// Largest frame the optical-flow path accepts, in each direction.
///
/// The engine itself handles up to 8192 px, but the cost is the frame transport
/// (two uploads per pair plus the CPU warp), and 1080p is the measured budget:
/// 1920x1080 costs ~9.7 ms of a 16.7 ms slot even with a GPU warp, so anything
/// larger blends instead. The decoder never produces more than 1920x1080, so
/// this is a guard, not a policy.
const OFA_MAX_W: u32 = 1920;
const OFA_MAX_H: u32 = 1080;

/// Whether the OFA path may run for this frame size. Size only: the runtime,
/// the device and the session are resolved by the attach worker, never here.
fn interp_ofa_size_ok(size: FrameSize) -> bool {
    size.w <= OFA_MAX_W && size.h <= OFA_MAX_H
}

/// Wall-clock budget for attaching mode 3 once a picture exists.
///
/// The DLL load, the D3D11 device and the OF session all run on a helper thread
/// (`ofa::Ofa::spawn_attach`), so this is not a startup budget: the first frame
/// is already on screen and the pacer keeps blending while the engine is built.
/// When the budget expires the mode degrades to blend for the rest of the
/// decode loop with one `ofa disabled reason=attach_timeout` line. A real
/// The first attach of a process also pays the cold `nvofapi64.dll` load and
/// driver initialization (measured >1 s on this machine); warm attaches and
/// device/session creation take ~50 ms. Three seconds covers the cold start
/// with margin and still leaves the picture blending for only a moment if the
/// driver never answers.
const OFA_ATTACH_BUDGET_MS: u64 = 3_000;

/// Whether the OFA cycle step is worth offering, cheaply enough for a UI click:
/// only the runtime is checked here (no device is created). A machine that
/// passes this but has no usable engine still degrades to blend at playback,
/// with a log line and a toast.
pub fn ofa_present() -> bool {
    ofa::Ofa::present()
}

/// The one line (and toast) a decode loop emits when mode 3 gives up and the
/// picture continues as blend. Called only once per decode loop: the caller
/// records the reason in `ofa_disabled`.
fn announce_ofa_disabled(
    window: &slint::Weak<PlayerWindow>,
    reason: &'static str,
    size: FrameSize,
) {
    crate::debug_log(&format!(
        "ofa disabled reason={reason} frame={}x{} → blend",
        size.w, size.h
    ));
    set_status(
        window,
        &format!("OFA 已停用（{reason}），继续使用混合"),
        crate::TOAST_KIND_INFO,
    );
}

/// Share of a source interval the optical-flow pair may spend, before the
/// pacer starts skipping pairs. The other slots of the interval are full-frame
/// passes already and use about half of it together, so half the interval is
/// the most a pair can take without pushing a display tick past its deadline:
/// measured at 1080p on this machine a pair needs ~20 ms of a 33 ms interval
/// under load (5.0 ms isolated), so the pacer blends there and warps when the
/// same frame size has room, which is exactly what the QA line's `ofa_n` and
/// `ofa_submit_s` show.
const OFA_BUDGET_SHARE: f32 = 0.6;
/// Opportunities skipped after a pair overran its budget (doubles per repeat,
/// capped by [`OFA_MAX_SKIP`]). The last skipped opportunity is a probe.
const OFA_OVER_BUDGET_SKIPS: u32 = 4;
const OFA_MAX_SKIP: u32 = 64;
/// The first pairs are cold (first uploads, cold caches) and still teach the
/// estimate, but never trigger the backoff.
const OFA_WARMUP_PAIRS: u64 = 4;

/// Cost gate for one optical-flow pair.
///
/// The same shape as [`motion::MotionInterp`]'s budget: a probe always runs, a
/// pair that overruns the interval schedules a growing number of blended
/// intervals, and a probe at the end of each backoff lets the warp come back
/// when the machine recovers (or the frame gets smaller).
struct OfaBudget {
    /// Smoothed pair cost, ms.
    ema_ms: f32,
    skip_left: u32,
    backoff: u32,
    pairs: u64,
}

impl OfaBudget {
    fn new() -> Self {
        Self {
            ema_ms: 0.0,
            skip_left: 0,
            backoff: 0,
            pairs: 0,
        }
    }

    /// Whether this interval may submit a pair and warp it.
    fn should_use(&mut self, budget_ms: f32) -> bool {
        if self.skip_left > 0 {
            self.skip_left -= 1;
            return self.skip_left == 0;
        }
        if self.pairs < OFA_WARMUP_PAIRS || self.ema_ms <= budget_ms {
            return true;
        }
        self.backoff = self
            .backoff
            .saturating_mul(2)
            .max(OFA_OVER_BUDGET_SKIPS)
            .min(OFA_MAX_SKIP);
        self.skip_left = self.backoff;
        false
    }

    /// Record the cost of the pair that just ran.
    fn record(&mut self, pair_ms: f32, budget_ms: f32) {
        if !pair_ms.is_finite() || pair_ms < 0.0 {
            return;
        }
        self.pairs += 1;
        if self.pairs <= OFA_WARMUP_PAIRS {
            self.ema_ms = if self.pairs == 1 {
                pair_ms
            } else {
                self.ema_ms.min(pair_ms)
            };
            self.backoff = 0;
            self.skip_left = 0;
            return;
        }
        self.ema_ms = self.ema_ms * 0.7 + pair_ms * 0.3;
        if pair_ms > budget_ms {
            if self.skip_left == 0 {
                self.backoff = self
                    .backoff
                    .saturating_mul(2)
                    .max(OFA_OVER_BUDGET_SKIPS)
                    .min(OFA_MAX_SKIP);
            }
            self.skip_left = self.skip_left.max(self.backoff);
        } else {
            self.backoff = 0;
            self.skip_left = 0;
        }
    }
}

/// Current refresh rate of the primary display, in Hz (cached for the process).
///
/// The player draws its picture on whichever monitor Windows placed the window
/// on; the primary display's current mode is the honest single number available
/// without moving Win32 window plumbing into this module. A value outside
/// 20..=500 Hz (the API returns 0/1 for "hardware default") falls back to 60.
fn display_refresh_hz() -> u32 {
    static HZ: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| {
        use windows_sys::Win32::Graphics::Gdi::{
            EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS,
        };
        // SAFETY: DEVMODEW is plain old data and zero is a valid empty value;
        // dmSize tells the API the struct version.
        let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        // SAFETY: a null device name selects the current display; `mode` is
        // correctly sized for the call.
        let ok =
            unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) };
        if ok != 0 && (20..=500).contains(&mode.dmDisplayFrequency) {
            mode.dmDisplayFrequency
        } else {
            60
        }
    })
}

/// Linear blend of two RGBA frames into `out`, ignoring alpha and keeping it
/// opaque. `weight` is the weight of `b` in `0..=256` (`0` = a, `256` = b).
///
/// The SSE2 path processes four pixels per iteration: widen bytes to u16,
/// multiply by the two weights, add the rounding term, shift back and pack.
/// Every intermediate fits in 16 bits unsigned (`255 * 256 + 128`), so there is
/// no saturation surprise; alpha is forced to 255 afterwards. The scalar tail
/// covers frame lengths that are not a multiple of 16 bytes. The function
/// allocates nothing and never reads or writes outside `out`.
fn blend_rgba_into(out: &mut [u8], a: &[u8], b: &[u8], weight: u16) {
    let len = out.len().min(a.len()).min(b.len());
    let weight = weight.min(256);
    #[cfg(target_arch = "x86_64")]
    {
        // SSE2 is part of the x86_64 baseline, so no runtime dispatch is needed.
        // SAFETY: the pointers are valid for `len` bytes and the loop stays
        // inside that range; the tail is handled by the scalar loop below.
        unsafe { blend_rgba_sse2(out.as_mut_ptr(), a.as_ptr(), b.as_ptr(), len, weight) };
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let inv = 256 - weight as u32;
        let w = weight as u32;
        for i in 0..len {
            out[i] = ((a[i] as u32 * inv + b[i] as u32 * w + 128) >> 8) as u8;
        }
        for pixel in out[..len].chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn blend_rgba_sse2(out: *mut u8, a: *const u8, b: *const u8, len: usize, weight: u16) {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let w = weight as i16;
    let inv = 256 - w;
    let wv = _mm_set1_epi16(w);
    let invv = _mm_set1_epi16(inv);
    let round = _mm_set1_epi16(128);
    let alpha = _mm_set1_epi32(0xFF00_0000u32 as i32);
    let mut offset = 0usize;
    // SAFETY: every load/store below is within `len` bytes (the loop condition
    // guarantees a full 16-byte block remains).
    unsafe {
        while offset + 16 <= len {
            let av = _mm_loadu_si128(a.add(offset) as *const __m128i);
            let bv = _mm_loadu_si128(b.add(offset) as *const __m128i);
            let alo = _mm_unpacklo_epi8(av, zero);
            let ahi = _mm_unpackhi_epi8(av, zero);
            let blo = _mm_unpacklo_epi8(bv, zero);
            let bhi = _mm_unpackhi_epi8(bv, zero);
            let lo = _mm_srli_epi16(
                _mm_add_epi16(
                    _mm_add_epi16(_mm_mullo_epi16(alo, invv), _mm_mullo_epi16(blo, wv)),
                    round,
                ),
                8,
            );
            let hi = _mm_srli_epi16(
                _mm_add_epi16(
                    _mm_add_epi16(_mm_mullo_epi16(ahi, invv), _mm_mullo_epi16(bhi, wv)),
                    round,
                ),
                8,
            );
            let packed = _mm_packus_epi16(lo, hi);
            let opaque = _mm_or_si128(packed, alpha);
            _mm_storeu_si128(out.add(offset) as *mut __m128i, opaque);
            offset += 16;
        }
        let inv = 256 - weight as u32;
        let w = weight as u32;
        for i in offset..len {
            *out.add(i) = ((*a.add(i) as u32 * inv + *b.add(i) as u32 * w + 128) >> 8) as u8;
        }
        // Force alpha opaque in the scalar tail as well.
        let mut tail = offset + 3;
        while tail < len {
            *out.add(tail) = 255;
            tail += 4;
        }
    }
}

/// One display tick of the interpolation grid, in the video timeline.
///
/// The grid is global (not reset per source frame), so the output interval stays
/// constant even when the display rate is not a multiple of the track rate.
struct InterpTicker {
    /// Tick step in the video timeline: `(1000 / out_hz) * playback speed`. At
    /// 2× a display frame covers twice as much video time, so the pacer still
    /// pushes at the display rate rather than twice it.
    step_ms: f64,
    /// Video timestamp of the next display frame, ms.
    next_ms: f64,
}

impl InterpTicker {
    fn new(out_hz: u32, speed: f64, video_ms: f64) -> Self {
        let speed = if speed.is_finite() && speed > 0.0 {
            speed
        } else {
            1.0
        };
        let out_ms = 1000.0 / out_hz.max(1) as f64;
        Self {
            step_ms: out_ms * speed,
            next_ms: video_ms,
        }
    }

    /// Skip ticks the clock has already passed, so a late source frame cannot
    /// produce a burst of back-to-back pushes. Returns the number skipped.
    fn skip_late(&mut self, now_ms: f64) -> u64 {
        let mut skipped = 0u64;
        // More than a full display period late = one lost display frame.
        while self.next_ms + self.step_ms < now_ms {
            self.next_ms += self.step_ms;
            skipped += 1;
        }
        skipped
    }
}

/// Live presentation clock in the video timeline.
///
/// The audio clock is the master while it is fresh and not frozen by the A/V
/// guard; the caller's own wall clock (advanced by the playback speed) is the
/// exact same fallback the pacer already uses for video-only tracks and for a
/// frozen audio process.
fn interp_clock_ms(
    control: &PlaybackControl,
    use_audio: bool,
    wall_at_frame_ms: f64,
    frame_at: Instant,
    speed: f64,
) -> f64 {
    if use_audio {
        if let Some(ms) = control.audio_clock() {
            return ms as f64;
        }
    }
    wall_at_frame_ms + frame_at.elapsed().as_secs_f64() * speed * 1000.0
}

/// Sleep until `tick_ms` on the presentation clock. The wait is recomputed from
/// the clock (never a fixed sleep) so the schedule follows the audio phase.
///
/// The wait is also bounded by the **wall** time at which this tick is due: the
/// audio clock can be stale (it is only fresh for [`AUDIO_CLOCK_FRESH_MS`] after
/// a sample, and `ffplay` publishes its first one ~1.5 s in), and waiting on a
/// frozen clock held the pacer for up to 1.3 s at the start of a session — the
/// measured startup stutter (fps 2/12/17/15 → 30, ~735 ms of content lost). A
/// tick that the wall clock says is due is never waited for.
///
/// Returns the milliseconds it actually slept, so the dip meter can report the
/// tick waits separately from the blend work.
fn wait_for_interp_tick(
    control: &PlaybackControl,
    use_audio: bool,
    wall_at_frame_ms: f64,
    frame_at: Instant,
    speed: f64,
    tick_ms: f64,
) -> u64 {
    let speed = speed.max(1.0e-3);
    // Wall ms after `frame_at` at which this tick is due.
    let wall_due_ms = (tick_ms - wall_at_frame_ms) / speed;
    let mut slept_ms = 0u64;
    for _ in 0..64 {
        let waited_ms = frame_at.elapsed().as_secs_f64() * 1000.0;
        if waited_ms >= wall_due_ms {
            break;
        }
        let now_ms = interp_clock_ms(control, use_audio, wall_at_frame_ms, frame_at, speed);
        let wait_ms = tick_ms - now_ms;
        if wait_ms <= 0.6 {
            break;
        }
        // Re-check at least every 20 ms so a clock correction cannot make the
        // loop sleep through several display frames.
        let chunk = wait_ms
            .min(MAX_SYNC_SLEEP_MS as f64)
            .min(20.0)
            .min(wall_due_ms - waited_ms);
        if chunk <= 0.0 {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(chunk / 1000.0));
        slept_ms = slept_ms.saturating_add(chunk.max(0.0) as u64);
    }
    slept_ms
}

/// Reusable blend buffers handed between the pacer and the UI thread.
///
/// The pacer takes one, blends into it and hands ownership to the event-loop
/// closure; once the window has replaced it, the same buffer goes back into the
/// pool. Steady state therefore allocates nothing per display frame. The first
/// few buffers are allocated (and paged in) once, with one spare frame of depth
/// so a busy UI thread does not force an allocation on every push.
///
/// The buffers are [`slint::SharedPixelBuffer`]s, not `Vec<u8>`: the pacer
/// blends **directly into the image the UI will show**, so a display frame is
/// handed over with no copy at all. A buffer is only recycled after the window
/// dropped its reference to it (one generation later — see
/// [`FrameMailbox::show`]), so `make_mut_bytes` never has to copy on write.
struct BlendPool {
    buffers: Mutex<Vec<FrameBuf>>,
    /// The initial spares were created for this player.
    primed: AtomicBool,
    /// Buffers allocated because the UI had not returned one yet (QA only).
    misses: AtomicU64,
}

impl BlendPool {
    fn new() -> Self {
        Self {
            buffers: Mutex::new(Vec::new()),
            primed: AtomicBool::new(false),
            misses: AtomicU64::new(0),
        }
    }

    fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Take a buffer of exactly `w`×`h`, reusing a recycled allocation when one
    /// is available.
    fn take(&self, size: FrameSize) -> FrameBuf {
        let recycled = self.buffers.lock().ok().and_then(|mut guard| guard.pop());
        if let Some(buffer) = recycled {
            if buffer.width() == size.w && buffer.height() == size.h {
                return buffer;
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let buffer = FrameBuf::new(size.w, size.h);
        if !self.primed.swap(true, Ordering::Relaxed) {
            if let Ok(mut guard) = self.buffers.lock() {
                for _ in 0..2 {
                    guard.push(FrameBuf::new(size.w, size.h));
                }
            }
        }
        buffer
    }

    /// Return a used buffer. The pool is bounded: a closed or slow UI must not
    /// pin an unbounded number of full frames.
    fn recycle(&self, buffer: FrameBuf) {
        if let Ok(mut guard) = self.buffers.lock() {
            if guard.len() < 3 {
                guard.push(buffer);
            }
        }
    }
}

/// One RGBA frame buffer in the shape both the decoder and Slint use.
type FrameBuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

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
    fn GetCurrentThread() -> isize;
    fn GetCurrentProcess() -> isize;
    fn GetThreadTimes(
        thread: isize,
        creation: *mut FileTimeRaw,
        exit: *mut FileTimeRaw,
        kernel: *mut FileTimeRaw,
        user: *mut FileTimeRaw,
    ) -> i32;
    fn GetProcessTimes(
        process: isize,
        creation: *mut FileTimeRaw,
        exit: *mut FileTimeRaw,
        kernel: *mut FileTimeRaw,
        user: *mut FileTimeRaw,
    ) -> i32;
    fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    fn SetPriorityClass(process: isize, class: u32) -> i32;
}

/// `winbase.h` FILETIME: 100 ns units since 1601, split across two `u32`s.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTimeRaw {
    low: u32,
    high: u32,
}

impl FileTimeRaw {
    fn ticks(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

/// `THREADENTRY32` (tlhelp32.h): 7 × 4 bytes, no padding on x64.
#[repr(C)]
#[derive(Clone, Copy)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_pid: u32,
    base_pri: i32,
    delta_pri: i32,
    flags: u32,
}

/// `TH32CS_SNAPTHREAD` / `THREAD_QUERY_LIMITED_INFORMATION` (tlhelp32.h,
/// winnt.h).
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const THREAD_QUERY_LIMITED_INFORMATION: u32 = 0x0800;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    fn Thread32First(snapshot: isize, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: isize, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> isize;
}

/// Thread id → role, for the core trace. Registered by each spawned thread.
static THREAD_ROLES: Mutex<Vec<(u32, &'static str)>> = Mutex::new(Vec::new());

pub(crate) fn note_thread_role(role: &'static str) {
    // SAFETY: GetCurrentThreadId is always valid and needs no handle.
    let tid = unsafe { GetCurrentThreadId() };
    if let Ok(mut guard) = THREAD_ROLES.lock() {
        if !guard.iter().any(|(id, _)| *id == tid) {
            guard.push((tid, role));
        }
    }
}

fn thread_role(tid: u32) -> &'static str {
    THREAD_ROLES
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .iter()
                .find(|(id, _)| *id == tid)
                .map(|(_, role)| *role)
        })
        .unwrap_or("other")
}

/// CPU ticks (100 ns) of every thread of this process: `(tid, ticks)`.
fn thread_cpu_table() -> Vec<(u32, u64)> {
    // SAFETY: plain Toolhelp enumeration; the snapshot handle is closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == -1 {
            return Vec::new();
        }
        let pid = GetCurrentProcessId();
        let mut entry = ThreadEntry32 {
            size: std::mem::size_of::<ThreadEntry32>() as u32,
            usage: 0,
            thread_id: 0,
            owner_pid: 0,
            base_pri: 0,
            delta_pri: 0,
            flags: 0,
        };
        let mut out = Vec::new();
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.owner_pid == pid {
                    let handle = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.thread_id);
                    if handle != 0 {
                        out.push((entry.thread_id, thread_cpu_ticks(handle)));
                        CloseHandle(handle);
                    }
                }
                entry.size = std::mem::size_of::<ThreadEntry32>() as u32;
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        out
    }
}

/// `BILI_CORE_TRACE=<ms|s>`: per-second process cores for the first `ms` of a
/// player (a value below 1000 is read as *seconds*), or exactly
/// `BILI_CORE_TRACE_MS=<ms>`. Split by thread role (`ui`, `pacer`, `reader`,
/// `stderr`, `audio-stats`, `prefetch`, `other`) and, once per second, per
/// thread id.
///
/// One `GetProcessTimes` + one Toolhelp pass per second; diagnostic only.
fn start_core_trace() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let window = core_trace_window();
        let Some(window) = window else {
            return;
        };
        std::thread::spawn(move || {
            note_thread_role("core-trace");
            let start = Instant::now();
            let mut last_at = Instant::now();
            let mut last_proc = process_cpu_ticks(unsafe { GetCurrentProcess() });
            let mut last_threads = thread_cpu_table();
            // Per-thread sum and sample count over the whole window, for the
            // summary line that answers "where did the cores go" in one place.
            let mut sums: std::collections::BTreeMap<u32, (f64, u32)> =
                std::collections::BTreeMap::new();
            let mut seconds = 0u32;
            let mut total_sum = 0.0f64;
            let mut total_max = 0.0f64;
            while start.elapsed() < window {
                std::thread::sleep(Duration::from_millis(1000));
                let now = Instant::now();
                let secs = now.duration_since(last_at).as_secs_f64().max(1.0e-6);
                let proc = process_cpu_ticks(unsafe { GetCurrentProcess() });
                let total = (proc.saturating_sub(last_proc)) as f64 / 1.0e7 / secs;
                let current = thread_cpu_table();
                let mut by_role: std::collections::BTreeMap<&'static str, f64> =
                    std::collections::BTreeMap::new();
                let mut table: Vec<(u32, f64)> = Vec::new();
                for (tid, ticks) in &current {
                    let previous = last_threads
                        .iter()
                        .find(|(id, _)| id == tid)
                        .map(|(_, t)| *t)
                        .unwrap_or(*ticks);
                    let cores = (ticks.saturating_sub(previous)) as f64 / 1.0e7 / secs;
                    if cores < 0.005 {
                        continue;
                    }
                    *by_role.entry(thread_role(*tid)).or_insert(0.0) += cores;
                    table.push((*tid, cores));
                    let entry = sums.entry(*tid).or_insert((0.0, 0));
                    entry.0 += cores;
                    entry.1 += 1;
                }
                table.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                seconds += 1;
                total_sum += total;
                total_max = total_max.max(total);
                // Full table (every thread that used at least 0.5 % of a core)
                // so a suspect that is not in the top four is still visible.
                let table_text: Vec<String> = table
                    .iter()
                    .map(|(tid, cores)| format!("{}={cores:.2}", thread_role(*tid)))
                    .collect();
                crate::debug_log(&format!(
                    "core trace t={}s total={total:.2} ui={:.2} pacer={:.2} reader={:.2} stderr={:.2} audio={:.2} prefetch={:.2} other={:.2} threads={} table={}",
                    start.elapsed().as_secs(),
                    by_role.get("ui").copied().unwrap_or(0.0),
                    by_role.get("pacer").copied().unwrap_or(0.0),
                    by_role.get("reader").copied().unwrap_or(0.0),
                    by_role.get("stderr").copied().unwrap_or(0.0),
                    by_role.get("audio-stats").copied().unwrap_or(0.0),
                    by_role.get("prefetch").copied().unwrap_or(0.0),
                    by_role.get("other").copied().unwrap_or(0.0),
                    current.len(),
                    table_text.join(" ")
                ));
                last_at = now;
                last_proc = proc;
                last_threads = current;
            }
            // One line per thread for the whole window: the per-second table
            // above answers "when", this answers "what", over all `seconds`.
            let mut rows: Vec<(u32, f64, u32)> = sums
                .into_iter()
                .map(|(tid, (sum, count))| (tid, sum, count))
                .collect();
            rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (tid, sum, count) in &rows {
                crate::debug_log(&format!(
                    "core thread tid={tid} role={} cores_mean={:.3} cores_max={:.2} samples={count}",
                    thread_role(*tid),
                    sum / (*count).max(1) as f64,
                    sum / (*count).max(1) as f64
                ));
            }
            crate::debug_log(&format!(
                "core trace done seconds={seconds} total_mean={:.2} total_max={:.2} threads={}",
                total_sum / seconds.max(1) as f64,
                total_max,
                rows.len()
            ));
        });
    });
}

/// Trace window from `BILI_CORE_TRACE_MS` (milliseconds) or `BILI_CORE_TRACE`
/// (milliseconds, or seconds when below 1000 — `BILI_CORE_TRACE=20` means
/// twenty seconds, not a one-second trace).
fn core_trace_window() -> Option<Duration> {
    let raw = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
    };
    if let Some(ms) = raw("BILI_CORE_TRACE_MS") {
        return Some(Duration::from_millis(ms.clamp(500, 600_000)));
    }
    let ms = raw("BILI_CORE_TRACE")?;
    let ms = if ms < 1000 { ms * 1000 } else { ms };
    Some(Duration::from_millis(ms.clamp(1000, 600_000)))
}

/// Log every panic (thread, location, message) through `debug_log`.
///
/// A panic on the decode thread used to be completely silent: the picture
/// stopped, the overlay stayed up, and nothing in the log said why. Installed
/// once per process; the default hook still runs afterwards.
fn install_panic_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("?").to_string();
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_default();
            crate::debug_log(&format!(
                "panic thread={name} at={location} msg={payload}"
            ));
            previous(info);
        }));
    });
}

/// `THREAD_PRIORITY_BELOW_NORMAL` (winbase.h).
const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
/// `NORMAL_PRIORITY_CLASS` (winbase.h): the class every media child runs in.
///
/// `main` puts the whole process in `BELOW_NORMAL_PRIORITY_CLASS` (see
/// `win::tune_priorities`), and a child inherits that class. For ffmpeg/ffplay
/// that inheritance is fatal on a loaded machine: the decoder is a **deadline**
/// task (it owes 30 or 60 frames per second), not throughput work, and a
/// below-normal thread only runs when nothing at normal priority is ready.
///
/// Measured on this host while 13 normal-priority `UnityShaderCompiler`
/// processes kept all 12 logical CPUs busy (Windows perf counters:
/// `_Total=1174` of `1200`):
///   * the same 1080p input, `-frames:v 1`: normal priority → 1st frame in
///     4.0 s (`cpu=0.50 s`); below-normal → **0.34 s of CPU in 65 s**, still
///     sitting in `Stream mapping`, no frame, no stderr. And the app's own log
///     showed `pacer beat frames=0 pushes=0 ring=0 gate=1`,
///     `fps_decoded=0.0` for the whole session.
/// That is the "player window opens but never shows a picture" report: the
/// decoder was not broken, it was scheduled out of existence.
///
/// Normal class keeps the media children at the same base priority (8) as the
/// UI thread (`highest` inside the below-normal class is also 8), so neither
/// starves the other; only the non-deadline helpers (pipe read, NV12
/// conversion, net pool) stay below.
const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
/// `PROCESS_SET_INFORMATION`: the access `SetPriorityClass` needs.
const PROCESS_SET_INFORMATION: u32 = 0x0200;

/// Run the calling thread below normal priority.
///
/// Every thread in this module that is not the UI thread (pipe read, NV12
/// conversion, stderr parse) does work that only has to keep up with 30–60 Hz.
/// On a busy machine Normal priority let them compete with — and win against —
/// the window's pointer and scroll handlers on the cores they share, which is
/// exactly the "the whole app is sluggish while a video plays" report.
fn lower_current_thread_priority() {
    if frame_path_legacy() {
        return;
    }
    // SAFETY: both calls are thread-local/handle-only; GetCurrentThread
    // returns a pseudo handle that must not be closed and is always valid.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Put a decoder/audio child process into the normal class.
///
/// Bounded and idempotent; a process that is already gone simply gets no
/// change. See [`NORMAL_PRIORITY_CLASS`] for why this is not below-normal:
/// ffmpeg owes a fixed frame rate, so on a busy host it must not be scheduled
/// behind background work. The pipe reader/converter thread keeps calling
/// [`lower_current_thread_priority`] inside the reader, so the work that can
/// afford to yield still does.
fn raise_process_priority(pid: u32) {
    set_process_priority(pid, NORMAL_PRIORITY_CLASS);
}

fn set_process_priority(pid: u32, class: u32) {
    if pid == 0 || frame_path_legacy() {
        return;
    }
    // SAFETY: OpenProcess returns 0 on failure (checked); the handle is closed.
    unsafe {
        let handle = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
        if handle != 0 {
            SetPriorityClass(handle, class);
            CloseHandle(handle);
        }
    }
}

fn thread_cpu_ticks(thread: isize) -> u64 {
    let mut creation = FileTimeRaw::default();
    let mut exit = FileTimeRaw::default();
    let mut kernel = FileTimeRaw::default();
    let mut user = FileTimeRaw::default();
    // SAFETY: all five pointers point at distinct, initialised FILETIMEs.
    let ok = unsafe {
        GetThreadTimes(
            thread,
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0;
    }
    kernel.ticks().saturating_add(user.ticks())
}

fn process_cpu_ticks(process: isize) -> u64 {
    let mut creation = FileTimeRaw::default();
    let mut exit = FileTimeRaw::default();
    let mut kernel = FileTimeRaw::default();
    let mut user = FileTimeRaw::default();
    // SAFETY: as above.
    let ok = unsafe {
        GetProcessTimes(
            process,
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0;
    }
    kernel.ticks().saturating_add(user.ticks())
}

/// `BILI_UI_LOAD_TEST=<ms>`: measure the UI thread while it presents.
///
/// Installed on the UI thread at the first frame and left running for `ms`:
/// each 50 ms tick adds up this thread's own CPU time (`GetThreadTimes` on the
/// current thread — the Slint event loop thread, which is where every pointer,
/// scroll, resize and paint runs) and the wall gap since the previous tick. A
/// line is emitted every 5 s (average cores + the largest gap in that window),
/// plus a total at the end. The average says whether the player is starving the
/// UI; the gap catches a single long stall that an average hides, and the 5 s
/// windows let a seek's own stall be told apart from steady playback.
/// Diagnostic only; no work outside a run.
fn start_ui_load_probe() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        note_thread_role("ui");
        let Some(ms) = std::env::var("BILI_UI_LOAD_TEST")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
        else {
            return;
        };
        let window = Duration::from_millis(ms.clamp(500, 600_000));
        let timer = Rc::new(slint::Timer::default());
        let start = Instant::now();
        let thread_cpu0 = thread_cpu_ticks(unsafe { GetCurrentThread() });
        let process_cpu0 = process_cpu_ticks(unsafe { GetCurrentProcess() });
        let last = Rc::new(Cell::new(Instant::now()));
        let win_start = Rc::new(Cell::new(Instant::now()));
        let win_cpu0 = Rc::new(Cell::new(thread_cpu0));
        let win_gap = Rc::new(Cell::new(0u64));
        let max_gap = Rc::new(Cell::new(0u64));
        let ticks = Rc::new(Cell::new(0u64));
        let tick_last = last.clone();
        let tick_win_start = win_start.clone();
        let tick_win_cpu0 = win_cpu0.clone();
        let tick_win_gap = win_gap.clone();
        let tick_max_gap = max_gap.clone();
        let tick_count = ticks.clone();
        let timer_handle = timer.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let now = Instant::now();
                let gap = now.duration_since(tick_last.get()).as_millis() as u64;
                tick_last.set(now);
                // The 50 ms cadence is the timer's own interval; anything above
                // it (plus timer slack) is time the event loop spent inside one
                // callback — a pointer handler, a scroll, a recomposite.
                let over = gap.saturating_sub(55);
                if over > tick_win_gap.get() {
                    tick_win_gap.set(over);
                }
                if over > tick_max_gap.get() {
                    tick_max_gap.set(over);
                }
                tick_count.set(tick_count.get().saturating_add(1));
                let win_elapsed = tick_win_start.get().elapsed();
                if win_elapsed >= Duration::from_secs(5) {
                    let cpu = thread_cpu_ticks(unsafe { GetCurrentThread() })
                        .saturating_sub(tick_win_cpu0.get());
                    let secs = win_elapsed.as_secs_f64().max(1.0e-6);
                    // FILETIME ticks are 100 ns.
                    crate::debug_log(&format!(
                        "ui-load win_ms={} cores={:.2} max_gap_ms={}",
                        win_elapsed.as_millis(),
                        (cpu as f64 / 1.0e7) / secs,
                        tick_win_gap.get(),
                    ));
                    tick_win_start.set(now);
                    tick_win_cpu0.set(thread_cpu_ticks(unsafe { GetCurrentThread() }));
                    tick_win_gap.set(0);
                }
                if start.elapsed() >= window {
                    timer_handle.stop();
                    let elapsed = start.elapsed().as_secs_f64().max(1.0e-6);
                    let thread_cpu = thread_cpu_ticks(unsafe { GetCurrentThread() })
                        .saturating_sub(thread_cpu0);
                    let process_cpu = process_cpu_ticks(unsafe { GetCurrentProcess() })
                        .saturating_sub(process_cpu0);
                    let thread_s = thread_cpu as f64 / 1.0e7;
                    let process_s = process_cpu as f64 / 1.0e7;
                    crate::debug_log(&format!(
                        "ui-load total cores={:.2} process_cores={:.2} max_gap_ms={} ticks={} window_ms={}",
                        thread_s / elapsed,
                        process_s / elapsed,
                        tick_max_gap.get(),
                        tick_count.get(),
                        window.as_millis(),
                    ));
                }
            },
        );
        // The probe must outlive this call: a Slint timer stops when its last
        // handle drops. It stops itself at the end of the window.
        std::mem::forget(timer);
    });
}

#[link(name = "ntdll")]
extern "system" {
    fn NtSuspendProcess(process: isize) -> i32;
    fn NtResumeProcess(process: isize) -> i32;
}

// ---------------------------------------------------------------------------
// Stall trace (`BILI_STALL_TRACE=1`)
// ---------------------------------------------------------------------------

/// `BILI_STALL_TRACE=1`: log every UI-thread gap longer than this.
const STALL_GAP_MS: u128 = 200;
/// The gap detector's own cadence.
const STALL_TICK: Duration = Duration::from_millis(50);
/// How often it samples per-thread CPU. A Toolhelp pass walks every thread in
/// the system to find this process's, so it runs at the core trace's own cadence
/// (1 s) and never on the 50 ms tick: a stall then reports each thread's cores
/// over the ≤1 s window that contained it.
const STALL_CPU_SAMPLE: Duration = Duration::from_secs(1);

/// Thread id → the step it is sitting in right now. Read by the deadlock trace
/// and by the stall trace; a store only happens when one of them is on.
static STALL_TRACE: OnceLock<bool> = OnceLock::new();

/// `BILI_STALL_TRACE=1`: the UI-thread gap detector (and, with it, the stage
/// tags it names in `last=`).
pub(crate) fn stall_trace_enabled() -> bool {
    *STALL_TRACE.get_or_init(|| {
        std::env::var("BILI_STALL_TRACE")
            .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

/// The stage tags a stall line can name. A fixed table, so the hot path stores
/// one index instead of a string.
const STAGE_TAGS: [&str; 16] = [
    "-",
    "pacer:loop",
    "pacer:pop",
    "pacer:kill",
    "pacer:join-reader",
    "pacer:join-stderr",
    "pacer:audio-stop",
    "reader:read",
    "reader:convert",
    "reader:wait-ring",
    "reader:throttle",
    "ui:publish",
    "prefetch:write",
    "fish:step",
    "cover:decode",
    "ui:tick",
];

/// Index+1 of the most recent stage tag anywhere in the process (`0` = none).
static LAST_STAGE: AtomicU64 = AtomicU64::new(0);
/// 养鱼 worker steps in flight (the engine runs one request at a time).
static FISH_STEPS: AtomicU64 = AtomicU64::new(0);
/// Cover fetches+decodes in flight.
static COVER_DECODES: AtomicU64 = AtomicU64::new(0);

fn stage_index(tag: &str) -> u64 {
    STAGE_TAGS
        .iter()
        .position(|known| *known == tag)
        .map(|index| index as u64 + 1)
        .unwrap_or(0)
}

/// The newest stage tag in the process, `-` when nothing has been tagged yet.
fn last_stage() -> &'static str {
    let index = LAST_STAGE.load(Ordering::Relaxed) as usize;
    STAGE_TAGS
        .get(index.saturating_sub(1))
        .copied()
        .unwrap_or("-")
}

/// Record that a 养鱼 step started/finished, so a stall can say whether that
/// worker was busy at the time.
pub(crate) fn note_fish_step(active: bool) {
    if active {
        FISH_STEPS.fetch_add(1, Ordering::Relaxed);
    } else {
        FISH_STEPS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Record that a cover fetch+decode started/finished.
pub(crate) fn note_cover_decode(active: bool) {
    if active {
        COVER_DECODES.fetch_add(1, Ordering::Relaxed);
    } else {
        COVER_DECODES.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What a window's event loop reports to the gap detector when it stalls.
pub(crate) struct StallSample {
    /// `main` or `player`: which window that event loop is showing.
    pub view: &'static str,
    pub playing: bool,
    pub buffering: bool,
    /// Ring occupancy in frames (0 with no player open).
    pub ring: u64,
    /// Cumulative frames handed to the UI; the detector turns the delta across
    /// two samples into `fps=`.
    pub frames: u64,
}

thread_local! {
    /// One detector per event-loop thread: main, player and mini windows share
    /// one Slint event loop, so in practice this is installed exactly once.
    static STALL_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Install the UI-thread gap detector on the calling (event-loop) thread.
///
/// Every gap over [`STALL_GAP_MS`] logs one line naming what the app was doing
/// while the event loop could not run:
///
/// ```text
/// stall gap_ms=340 at=18234 view=player playing=1 buffering=0 ring=11 fps=29.8 fish=0 covers=2 threads=35 disk_q=0 last=pacer:pop
/// stall thread tid=8124 role=ui tag=ui:publish cores=0.02
/// stall thread tid=9912 role=disk-writer tag=prefetch:write cores=0.87
/// ```
///
/// The second and following lines are each thread's share of the cores *during
/// the gap* (delta of `GetThreadTimes` against the previous sample), so a stall
/// ends with the thread that ate the CPU, not just the one that resumed.
///
/// It covers the main window too: the main and player windows share this event
/// loop, so `view=` says which one the loop was showing and the sample carries
/// that player's own counters.
pub(crate) fn install_stall_trace(mut sample: impl FnMut() -> StallSample + 'static) {
    if !stall_trace_enabled() {
        return;
    }
    if STALL_INSTALLED.with(|cell| cell.replace(true)) {
        return;
    }
    note_thread_role("ui");
    crate::debug_log(&format!(
        "stall trace installed tick_ms={} gap_ms={STALL_GAP_MS}",
        STALL_TICK.as_millis()
    ));
    let timer = Rc::new(slint::Timer::default());
    let mut ticked_at = Instant::now();
    let mut cpu_at = Instant::now();
    let mut cpu_table = thread_cpu_table();
    let mut last_frames: Option<(u64, Instant)> = None;
    let mut heartbeat = Instant::now();
    let mut ticks = 0u64;
    let mut stalls = 0u64;
    let mut max_gap = 0u64;
    let mut primed = false;
    timer.start(slint::TimerMode::Repeated, STALL_TICK, move || {
        let now = Instant::now();
        let gap = now.duration_since(ticked_at);
        ticked_at = now;
        // The first tick measures "install -> the event loop started", which is
        // startup wiring, not a stall.
        let primed_now = primed;
        primed = true;
        ticks += 1;
        if primed_now {
            max_gap = max_gap.max(gap.as_millis() as u64);
        }
        let snap = sample();
        let fps = match last_frames {
            Some((frames, at)) => {
                let secs = now.duration_since(at).as_secs_f64().max(1.0e-6);
                snap.frames.saturating_sub(frames) as f64 / secs
            }
            None => 0.0,
        };
        last_frames = Some((snap.frames, now));
        if primed_now && gap.as_millis() > STALL_GAP_MS {
            stalls += 1;
            let table = thread_cpu_table();
            let span = now.duration_since(cpu_at).as_secs_f64().max(1.0e-6);
            crate::debug_log(&format!(
                "stall gap_ms={} at={} view={} playing={} buffering={} ring={} fps={fps:.1} fish={} covers={} threads={} disk_q={} last={}",
                gap.as_millis(),
                monotonic_ms(),
                snap.view,
                u8::from(snap.playing),
                u8::from(snap.buffering),
                snap.ring,
                FISH_STEPS.load(Ordering::Relaxed),
                COVER_DECODES.load(Ordering::Relaxed),
                table.len(),
                crate::media_hooks::disk_queue_depth(),
                last_stage(),
            ));
            let mut rows: Vec<(u32, f64)> = table
                .iter()
                .filter_map(|(tid, ticks)| {
                    let before = cpu_table
                        .iter()
                        .find(|(id, _)| id == tid)
                        .map(|(_, ticks)| *ticks)
                        .unwrap_or(*ticks);
                    let cores = ticks.saturating_sub(before) as f64 / 1.0e7 / span;
                    (cores >= 0.01).then_some((*tid, cores))
                })
                .collect();
            rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (tid, cores) in rows.iter().take(10) {
                crate::debug_log(&format!(
                    "stall thread tid={tid} role={} tag={} cores={cores:.2}",
                    thread_role(*tid),
                    thread_tag(*tid),
                ));
            }
            // Re-baseline on the fresh table: the stall's own dump must not show
            // up as CPU in the next one.
            cpu_table = table;
            cpu_at = now;
        } else if now.duration_since(cpu_at) >= STALL_CPU_SAMPLE {
            cpu_table = thread_cpu_table();
            cpu_at = now;
        }
        // Proof of life: a silent log must mean "no stall", never "the timer
        // never ran". One line per 10 s with the worst gap since the last one.
        if heartbeat.elapsed() >= Duration::from_secs(10) {
            crate::debug_log(&format!(
                "stall trace ticks={ticks} stalls={stalls} max_gap_ms={max_gap} window_s={}",
                heartbeat.elapsed().as_secs()
            ));
            heartbeat = now;
            ticks = 0;
            stalls = 0;
            max_gap = 0;
        }
    });
    // Timers stop when the last handle drops; this one lives for the process.
    std::mem::forget(timer);
}

/// The step `tid` is in, `-` when it never tagged one.
fn thread_tag(tid: u32) -> &'static str {
    THREAD_TAGS
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .iter()
                .find(|(id, _)| *id == tid)
                .map(|(_, tag)| *tag)
        })
        .unwrap_or("-")
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
/// source-sized (not a fixed 960x540) but this space is deliberately NOT
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
    /// DASH codec id of the active track (`7` AVC, `12` HEVC, `13` AV1,
    /// `0` unknown).
    pub codecid: u32,
    pub width: u32,
    pub height: u32,
    /// Size of the RGBA frame the decoder actually produces for this session
    /// (the source-based target, see [`target_frame_size`]).
    pub frame_w: u32,
    pub frame_h: u32,
    /// The track's declared frame rate (`fps=` in the play URL).
    pub declared_fps: f32,
    /// Selected quality (`qn`).
    pub qn: i64,
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
    /// A/V relation as the guard last measured it: sound minus picture (ms) and
    /// the guard's state (`AV_OFF` no clock, `AV_RUN`, `AV_HOLD` suspended). See
    /// [`PlaybackControl::note_av_state`].
    av_lead_ms: AtomicI64,
    av_state: AtomicU64,
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
            av_lead_ms: AtomicI64::new(0),
            av_state: AtomicU64::new(AV_OFF),
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
    /// Whether this player's one history reporter thread was started (see
    /// [`HistoryReporter`]). `run` is re-entered after a pacer panic; the claim
    /// keeps that restart from opening a second reporter for the same session.
    history_claimed: AtomicBool,
    paused: AtomicBool,
    /// f152: the window is completely covered, so Windows stops compositing it
    /// and the picture cannot advance. This is a HOLD, not the user's pause
    /// latch: the pacer freezes the picture and suspends the audio exactly like
    /// a user pause, but the transport intent is untouched, so uncovering the
    /// window resumes by itself. A user pause always wins: clearing this flag
    /// never clears `paused`.
    occluded: AtomicBool,
    position_ms: AtomicU64,
    /// Duration the API declared for the cid being played (playurl), ms.
    /// [`Self::duration_ms`] prefers the decoder's own value when the two
    /// disagree by more than [`DURATION_SOURCE_TOLERANCE`].
    api_duration_ms: AtomicU64,
    /// Duration the running decoder reported in its own `Duration:` line, ms
    /// (`0` until that line is parsed, and reset for every new session so a
    /// previous cid can never lend its length to this one).
    decoder_duration_ms: AtomicU64,
    /// Monotonic ms of the newest presented frame (`0` = none yet). A playhead
    /// that reached the duration only ends the track when this is at least
    /// [`END_POSITION_IDLE_MS`] old — a moving picture is never "ended".
    presented_at_ms: AtomicU64,
    /// Whether a video decoder process is running for this player right now.
    decoder_live: AtomicBool,
    /// `BILI_END_TRACE=1`: one line per end-of-track decision (see
    /// [`Self::end_trace`]).
    end_trace: bool,
    /// Rate limiter for the trace's rejected (non-latching) lines.
    last_end_trace_ms: AtomicU64,
    /// A play press asked for a fresh session while none was running (the
    /// decode thread's hold loop consumes it).
    resume_request: AtomicBool,
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
    /// Rate correction of the master clock, ppm (0 = the nominal playback
    /// speed). A sample that lands behind the extrapolation is not adopted as
    /// a step; the clock is *slowed* by this much until it agrees with the
    /// sound again, so the pacer never has to hold the picture for the jitter
    /// of one `-stats` line (see `set_audio_clock_ms`).
    audio_clock_ppm: AtomicI64,
    /// Raw sample-vs-extrapolation errors for the profiler: how far a `-stats`
    /// position pulled the clock back (`back_ms`, the cost the pacer used to
    /// pay) or pushed it forward (`fwd_ms`), over `samples` samples.
    audio_clock_back_ms: AtomicU64,
    audio_clock_fwd_ms: AtomicU64,
    audio_clock_samples: AtomicU64,
    audio_clock_worst_ms: AtomicU64,
    /// Monotonic ms the sound was last suspended and the release that ended it
    /// (`0` = never). The pair opens the resume quiet window in which a forward
    /// `-stats` report is the audio **device**, not the content (see
    /// [`AUDIO_CLOCK_RESUME_QUIET_MS`]), and it is the evidence the
    /// `audio clock jump` line prints.
    audio_freeze_at_ms: AtomicU64,
    audio_resume_at_ms: AtomicU64,
    /// A forward jump that is not confirmed yet: the reported position and the
    /// monotonic ms it arrived (`0` = none). A second report within
    /// [`AUDIO_CLOCK_CONFIRM_MS`] of it re-seeds the clock; anything else is an
    /// artefact and the clock never moved (see `set_audio_clock_ms`).
    audio_jump_cand_ms: AtomicU64,
    audio_jump_cand_at_ms: AtomicU64,
    /// Monotonic ms of the newest `ffplay` spawn, and the clock generation whose
    /// first real position sample was already logged (`u64::MAX` = none).
    audio_spawn_at_ms: AtomicU64,
    audio_first_logged_gen: AtomicU64,
    /// Monotonic ms at which this player's first frame entered the ring
    /// (`0` = none yet). Persisted per player, not per decode session, so a
    /// seek/quality respawn does not re-arm the startup gate.
    first_frame_at_ms: AtomicU64,
    /// Playback speed times 1000 (1000 = 1.0x).
    speed_milli: AtomicU64,
    /// Selected DASH video quality (`qn`).
    quality_qn: AtomicU64,
    /// Monotonic ms of the newest 画质 switch request that has not presented a
    /// frame yet (`0` = none). Read by the pacer at the first frame of a
    /// session, which logs the switch's own latency (`session first present`).
    quality_switch_at_ms: AtomicU64,
    /// The `qn` that switch asked for (log line only).
    quality_switch_qn: AtomicU64,
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
    /// 原始分辨率: decode at the source's own frame size instead of the
    /// window-sized picture ([`reader_output_size`]). Read once per session, so
    /// toggling it restarts the decoder at the current position.
    native_frame_size: AtomicBool,
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
    /// `true` while the buffer guard holds playback (auto pause). Unlike the
    /// user's pause this does not tear the decode session down: the pacer keeps
    /// polling so the ring can refill and playback resumes by itself.
    auto_paused: AtomicBool,
    /// Last accepted user-seek target and the monotonic ms it was accepted at,
    /// so a duplicate seek (drag release after a drag seek) is a cheap no-op.
    last_seek_target: AtomicU64,
    last_seek_at: AtomicU64,
    /// Ring occupancy the armed pre-roll waits for, in frames (`0` = none).
    buffer_target: AtomicU64,
    /// Proxy cache the cache-based startup gate waits for, in bytes (`0` = the
    /// gate is ring-based, never armed, or already open). While this is non-zero
    /// `buffer_progress`/`buffer_detail` report cache progress instead of ring
    /// occupancy.
    buffer_need_bytes: AtomicU64,
    /// Cache progress published by that gate, thousandths (`0..=1000`). The
    /// pacer measures it; the UI only loads it, so `buffer_progress` stays
    /// lock-free at 60 Hz.
    buffer_pct: AtomicU32,
    /// Monotonic ms the current startup armed at (`0` = no startup running).
    /// Together with `startup_subs` this is the whole stage machine the overlay
    /// draws: the pacer publishes, the UI only loads.
    startup_armed_ms: AtomicU64,
    /// Per-stage sub-progress of that startup, thousandths, in
    /// [`StartupStage`] order. Each is monotonic for the life of the startup.
    startup_subs: [AtomicU32; 4],
    /// The bar value derived from `startup_subs` ([`startup_overall_milli`]),
    /// thousandths, slewed toward it at [`STARTUP_SLEW_PER_MS`]. `1000` only
    /// once the first frame is handed over.
    startup_milli: AtomicU32,
    /// Monotonic ms of the previous bar publish (the slew's time base).
    startup_milli_ms: AtomicU64,
    /// Ring frames the startup's PreRoll stage waits for (clamped per session).
    startup_target_frames: AtomicU64,
    /// Proxy cache bytes the Bytes stage waits for (`0` = no proxy: ring mode).
    startup_need_bytes: AtomicU64,
    /// Decoded-frames counter at the moment the startup armed, so the Decode
    /// stage reports this session's frames, not the player's lifetime total.
    startup_decoded_base: AtomicU64,
    /// Last trace stamp and stage of the `startup stage=…` line (≥ 10 Hz).
    startup_log_ms: AtomicU64,
    startup_log_stage: AtomicU8,
    /// Monotonic ms until which a *finished* startup still reads 100 % and keeps
    /// the overlay up, so the UI's own "is there a picture yet" test cannot flash
    /// the bar back to 0 for the tick between the handover and its composite.
    startup_closing_until: AtomicU64,
    /// Monotonic ms at which this player window opened, so the first presented
    /// frame can be reported as `open -> picture` without any UI involvement.
    opened_at_ms: u64,
    /// Target (ms) of a user seek waiting for its first presented frame,
    /// `u64::MAX` = none. Set by [`PlaybackControl::seek`], consumed by the
    /// decode loop so the buffering overlay can be armed from the seek itself.
    pending_seek_ms: AtomicU64,
    /// Position (ms) playback was at when that seek was issued, so the decoder
    /// can tell a backward seek (already served) from a forward one.
    pending_seek_from_ms: AtomicU64,
    /// The position the UI must show while a seek is being served: the target
    /// the user picked, from the release until the new picture's first frame.
    /// `u64::MAX` = no seek in flight. The pacer keeps publishing the *old*
    /// picture's position while a seek shadow pre-rolls; the readout and the
    /// playhead follow the new target instead, so they cannot snap back to the
    /// old spot for the length of the pre-roll.
    position_hold_ms: AtomicU64,
    /// `true` while the current `buffering` state belongs to a seek (rather
    /// than the start-of-playback pre-roll), so the overlay can show the seek's
    /// own quantified detail line.
    seek_buffering: AtomicBool,
    /// Monotonic ms at which that seek was issued (for the duration log line).
    seek_buffering_at: AtomicU64,
    /// f151: monotonic ms of the release whose picture is frozen, `0` = the
    /// picture follows the decoder. Set by [`PlaybackControl::seek`] the instant
    /// the UI calls it, so the **next** old-session frame is refused by the
    /// mailbox: the frame on screen at the release stays there until the new
    /// position's first frame replaces it. Cleared by that first frame.
    ///
    /// This is what makes a release read as "the picture stops where I let go"
    /// instead of "the number changed and the video kept playing for 0.6 s".
    seek_freeze_at: AtomicU64,
    /// Content position (ms) of the most recently presented picture frame.
    /// Unlike `position_ms` (which the release immediately points at the
    /// target) this is where the pixels on screen really are: the frozen
    /// picture's position while a seek is in flight, the target after the swap.
    /// The f151 log line and the end-of-track rules read it.
    presented_ms: AtomicU64,
    /// The video this window is playing. A 选集 pick replaces it **in place**:
    /// same window, same control, only the decode thread re-arms on the new
    /// `bvid`/`cid` (see [`PlaybackControl::switch_to`]).
    source: std::sync::Mutex<PlaybackSource>,
    /// Bumped on every 选集 switch; the history reporter starts a new record
    /// for the new video on a change.
    source_seq: AtomicU64,
    /// A switch the running `run` loop has to perform. Written by
    /// [`PlaybackControl::switch_to`] (UI thread), taken by the decode thread.
    switch_request: std::sync::Mutex<Option<PlaybackSource>>,
    /// Monotonic ms the pending switch was requested (`0` = none), read once by
    /// the `series switch first_frame_ms` receipt.
    switch_at_ms: AtomicU64,
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
    /// Keep the audio process silent until the first picture of its session is
    /// on screen. `ffplay` is spawned with the decoder (so its clock is warm by
    /// the time the first frame is presented, which is what removes the ~1 s
    /// startup A/V debt), and must not be heard before there is a picture.
    audio_hold_silent: AtomicBool,
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
    /// Requested DASH audio id (0 = 自动（最高）). See [`PlaybackControl::set_audio_quality`].
    audio_quality_id: AtomicU64,
    /// (id, bandwidth, flags) of the last published audio track list; flags:
    /// 1 = 杜比全景声, 2 = Hi-Res 无损, 4 = 大会员 only.
    audio_meta: std::sync::Mutex<Vec<(i64, u64, u8)>>,
    /// Real playback/streaming counters behind [`Stats`].
    prof: Prof,
    /// Loopback prefetch proxy feeding this player's decoder, if one is up.
    /// Per player on purpose: several player windows can be open at once and
    /// must never share a port, a read cursor or a shutdown flag.
    prefetch: std::sync::Mutex<Option<PrefetchSession>>,
    /// Loopback prefetch proxy feeding this player's DASH **audio** track, if
    /// one is up. Separate from [`PlaybackControl::prefetch`] for the same
    /// reason it is separate from another window's: its own port, its own read
    /// cursor and its own session cache file.
    audio_prefetch: std::sync::Mutex<Option<AudioPrefetchSession>>,    /// Whether the running decode session reads through the proxy. A session
    /// only reads through the engine bound to **its own** stream: a 画质 switch
    /// installs one for the new tier before the session spawns (see
    /// `switch_prefetch`), so this is false only while there is no proxy at all.
    prefetch_in_use: AtomicBool,
    /// A stream whose proxy spawn already failed (`qn`, `codecid`). No second
    /// attempt for this player, so a broken mirror costs one failure instead of
    /// one spawn per session restart.
    prefetch_failed_stream: std::sync::Mutex<Option<(i64, i64)>>,
    /// Sticky "this track has finished" flag for the current playback session,
    /// latched by [`PlaybackControl::track_ended`] and the decode loop and
    /// cleared by a seek, a quality switch or a new control (see
    /// [`TRACK_END_SLACK_MS`]).
    ended: AtomicBool,
    interp_mode: AtomicU64,
    /// Cumulative display frames produced by the interpolator, cumulative blend
    /// compute time in µs, and display ticks skipped because they were late.
    /// Reset on every mode change, so the disabled state honestly reports zero.
    interp_generated: AtomicU64,
    interp_dropped: AtomicU64,
    interp_us: AtomicU64,
    /// Cumulative block-matching pair time in µs and pair count, for `bm_ms`.
    /// Every generated frame carries a cost in `interp_us`; this pair counter
    /// is the smaller subset that ran the motion path.
    interp_bm_us: AtomicU64,
    interp_bm_pairs: AtomicU64,
    /// Pair-time histogram, 0.5 ms buckets up to 32 ms, for the QA line's
    /// `bm_p50`/`bm_p95`. The average alone hides a bimodal machine.
    interp_bm_hist: [AtomicU64; BM_HIST_BUCKETS],
    /// Fastest and slowest blend of the current episode, for the QA line: a
    /// large spread means the pacer thread was preempted, not that the blend
    /// itself became expensive.
    interp_us_min: AtomicU64,
    interp_us_max: AtomicU64,
    /// Cumulative optical-flow pair time in µs (submit + flow readback + warp),
    /// pair count, the readback part of it, and the pairs submitted. Kept apart
    /// from the block-matching counters so the QA line can report mode 3's own
    /// transport cost without mixing it with a mode that did not run.
    interp_ofa_us: AtomicU64,
    interp_ofa_pairs: AtomicU64,
    interp_ofa_flow_us: AtomicU64,
    interp_ofa_submits: AtomicU64,
    /// Pair-time histogram for the QA line's p50/p95, same buckets as `bm`.
    interp_ofa_hist: [AtomicU64; BM_HIST_BUCKETS],
    /// Output rate the pacer resolved for the current episode (`0` while off,
    /// or when the display is not faster than the track). Published to the UI
    /// as `<hz> fps` beside the measured cost.
    interp_out_hz: AtomicU64,
}

/// The video one player window is on. A 选集 pick replaces this in place; the
/// window itself never closes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackSource {
    pub bvid: String,
    pub cid: u64,
    /// Archive id (comments / history); `0` when the caller does not know it.
    pub aid: i64,
    /// Display title for the window's own title strip.
    pub title: String,
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
            current: current == Some((spec.qn, spec.codecid, spec.bandwidth)),
        })
        .collect()
}

/// Most recent frame handed to the UI, with the exact size it was decoded at.
struct LastFrame {
    /// The frame itself (reference counted, so keeping it costs no copy) and
    /// the size it was decoded at.
    buffer: Option<FrameBuf>,
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

/// One player's audio proxy: a second, small prefetch engine in front of the
/// DASH audio track `ffplay` reads.
///
/// The sound used to be the slow half of a seek: `ffplay` was handed the direct
/// DASH URL, so every seek reconnected to the CDN and ran `-ss` over the network
/// before it could report a position (1179–1936 ms measured). Behind the same
/// local proxy as the picture, its first clock sample costs a loopback read
/// instead of a CDN round trip.
///
/// It is a **separate engine**, not a second target of the video one: the proxy
/// serves one read cursor at a time, so a pre-rolling seek's sound and the sound
/// still playing would fight over it. Two engines are two caches, two ports and
/// two independent windows — exactly like two player windows.
pub(crate) struct AudioPrefetchSession {
    handle: PrefetchHandle,
    /// The exact DASH audio URL the proxy was opened for. A session only reads
    /// through it while the track URL it resolved equals this one.
    source_url: String,
}

/// Read-ahead window of the audio proxy. The audio stream is 24–48 KB/s, so
/// 8 MiB is minutes of sound: one small engine is enough for a whole seek.
const AUDIO_READ_AHEAD_BYTES: u64 = 8 << 20;
/// Upstream range size of the audio proxy.
const AUDIO_CHUNK_BYTES: u64 = 1 << 20;
/// Parallel fetchers of the audio proxy.
const AUDIO_CONNECTIONS: usize = 2;
/// How far before its byte estimate a seek's audio pre-warm starts. The estimate
/// is `length × position / duration`, so it is off by a fraction of a percent of
/// the media; a chunk of margin makes the first read a hit without fetching a
/// region the seek will not read.
const AUDIO_PRIME_BACKOFF_BYTES: u64 = AUDIO_CHUNK_BYTES;
/// Bytes a seek's audio pre-warm fetches into the point cache: two chunks, i.e.
/// ±one chunk around the byte estimate — the estimate's error, not minutes of
/// sound the client will fetch itself once its cursor lands.
const AUDIO_PRIME_BYTES: u64 = 2 * AUDIO_CHUNK_BYTES;

/// What the disk map says about one seek target.
///
/// `path` is `Some` only when the cache file really holds the region the decoder
/// needs, so it is both the answer ("can this be played locally?") and the
/// evidence for the `seek cached=` line.
struct SeekCacheProbe {
    /// The local file to decode, when the region is on disk.
    path: Option<PathBuf>,
    /// Byte offset the check starts at (the target estimate, backed off by an
    /// eighth of the media).
    safe: u64,
    /// Bytes required from `safe` (to the end of the media by default).
    need: u64,
    /// Contiguous bytes the disk map holds at `safe`.
    run: u64,
    /// Present ranges on disk, and their total bytes (the 下载分布 map).
    ranges: usize,
    bytes: u64,
}

impl PlaybackControl {
    pub fn new() -> Arc<Self> {
        let control = Arc::new(Self {
            closed: AtomicBool::new(false),
            history_claimed: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            occluded: AtomicBool::new(false),
            position_ms: AtomicU64::new(0),
            api_duration_ms: AtomicU64::new(0),
            decoder_duration_ms: AtomicU64::new(0),
            presented_at_ms: AtomicU64::new(0),
            decoder_live: AtomicBool::new(false),
            end_trace: std::env::var("BILI_END_TRACE")
                .map(|value| value == "1")
                .unwrap_or(false),
            last_end_trace_ms: AtomicU64::new(0),
            resume_request: AtomicBool::new(false),
            seek_seq: AtomicU64::new(0),
            audio_seq: AtomicU64::new(0),
            audio_clock_seq: AtomicU64::new(0),
            audio_clock_ms: AtomicU64::new(0),
            audio_clock_stamp_ms: AtomicU64::new(0),
            audio_clock_ppm: AtomicI64::new(0),
            audio_clock_back_ms: AtomicU64::new(0),
            audio_clock_fwd_ms: AtomicU64::new(0),
            audio_clock_samples: AtomicU64::new(0),
            audio_clock_worst_ms: AtomicU64::new(0),
            audio_freeze_at_ms: AtomicU64::new(0),
            audio_resume_at_ms: AtomicU64::new(0),
            audio_jump_cand_ms: AtomicU64::new(0),
            audio_jump_cand_at_ms: AtomicU64::new(0),
            audio_spawn_at_ms: AtomicU64::new(0),
            audio_first_logged_gen: AtomicU64::new(u64::MAX),
            first_frame_at_ms: AtomicU64::new(0),
            speed_milli: AtomicU64::new(1000),
            quality_qn: AtomicU64::new(0),
            quality_switch_at_ms: AtomicU64::new(0),
            quality_switch_qn: AtomicU64::new(0),
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
            native_frame_size: AtomicBool::new(false),
            codec_pref: AtomicU64::new(0),
            audio_only: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            buffering: AtomicBool::new(false),
            auto_paused: AtomicBool::new(false),
            last_seek_target: AtomicU64::new(u64::MAX),
            last_seek_at: AtomicU64::new(0),
            buffer_target: AtomicU64::new(0),
            buffer_need_bytes: AtomicU64::new(0),
            buffer_pct: AtomicU32::new(0),
            startup_armed_ms: AtomicU64::new(0),
            startup_subs: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
            startup_milli: AtomicU32::new(0),
            startup_milli_ms: AtomicU64::new(0),
            startup_target_frames: AtomicU64::new(PRE_ROLL_START_FRAMES as u64),
            startup_need_bytes: AtomicU64::new(0),
            startup_decoded_base: AtomicU64::new(0),
            startup_log_ms: AtomicU64::new(0),
            startup_log_stage: AtomicU8::new(u8::MAX),
            startup_closing_until: AtomicU64::new(0),
            opened_at_ms: monotonic_ms(),
            pending_seek_ms: AtomicU64::new(u64::MAX),
            pending_seek_from_ms: AtomicU64::new(0),
            position_hold_ms: AtomicU64::new(u64::MAX),
            seek_buffering: AtomicBool::new(false),
            seek_buffering_at: AtomicU64::new(0),
            seek_freeze_at: AtomicU64::new(0),
            presented_ms: AtomicU64::new(0),
            source: std::sync::Mutex::new(PlaybackSource::default()),
            source_seq: AtomicU64::new(0),
            switch_request: std::sync::Mutex::new(None),
            switch_at_ms: AtomicU64::new(0),
            display_w: AtomicU64::new(0),
            display_h: AtomicU64::new(0),
            display_fullscreen: AtomicBool::new(false),
            frame_w: AtomicU64::new(0),
            frame_h: AtomicU64::new(0),
            last_frame: std::sync::Mutex::new(LastFrame {
                buffer: None,
                w: 0,
                h: 0,
            }),
            volume: AtomicU64::new(100),
            muted: AtomicBool::new(false),
            audio_pid: AtomicU64::new(0),
            audio_session_pending: AtomicBool::new(false),
            audio_hold_silent: AtomicBool::new(false),
            audio_session_try_ms: AtomicU64::new(0),
            audio_restarts: AtomicU64::new(0),
            topmost: AtomicBool::new(false),
            audio_index: AtomicU64::new(0),
            audio_quality_id: AtomicU64::new(0),
            audio_meta: std::sync::Mutex::new(Vec::new()),
            prof: Prof::new(),
            prefetch: std::sync::Mutex::new(None),
            audio_prefetch: std::sync::Mutex::new(None),
            prefetch_in_use: AtomicBool::new(false),
            prefetch_failed_stream: std::sync::Mutex::new(None),
            ended: AtomicBool::new(false),
            interp_mode: AtomicU64::new(0),
            interp_generated: AtomicU64::new(0),
            interp_dropped: AtomicU64::new(0),
            interp_us: AtomicU64::new(0),
            interp_bm_us: AtomicU64::new(0),
            interp_bm_pairs: AtomicU64::new(0),
            interp_bm_hist: std::array::from_fn(|_| AtomicU64::new(0)),
            interp_us_min: AtomicU64::new(u64::MAX),
            interp_us_max: AtomicU64::new(0),
            interp_ofa_us: AtomicU64::new(0),
            interp_ofa_pairs: AtomicU64::new(0),
            interp_ofa_flow_us: AtomicU64::new(0),
            interp_ofa_submits: AtomicU64::new(0),
            interp_ofa_hist: std::array::from_fn(|_| AtomicU64::new(0)),
            interp_out_hz: AtomicU64::new(0),
        });
        control
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // f151: a close during a frozen seek must not leave the flag set for a
        // window that never presents again (the mailbox would refuse every
        // frame of the mini player it mirrors).
        self.seek_freeze_at.store(0, Ordering::SeqCst);
        self.position_hold_ms.store(u64::MAX, Ordering::SeqCst);
        // The proxy owns a listener, a runtime task and up to six fetch
        // workers. `PrefetchHandle::shutdown` waits (bounded) for the server
        // task on a non-runtime thread, and both close paths run on the UI
        // thread, so the wait happens on a throwaway thread instead.
        if let Some(handle) = self.take_prefetch("player closed") {
            std::thread::spawn(move || drop(handle));
        }
        if let Some(audio) = self.take_audio_prefetch("player closed") {
            retire_audio_prefetch(audio.handle);
        }
    }

    /// Install the proxy this player's decoder should read from. Replaces (and
    /// shuts down) any previous one for the same player.
    pub fn install_prefetch(&self, handle: PrefetchHandle, source_url: String, host: String) {
        // Playback first: a fresh proxy starts with its seek-point warm loop
        // held, so nothing speculative competes with the first frames. The
        // pacer's 4 Hz sampler releases it through
        // [`PlaybackControl::set_prefetch_background_paused`] once playback has
        // been healthy long enough (and holds it again during a stall).
        if crate::media_hooks::startup_gate_enabled() {
            handle.set_background_paused(true);
        }
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
        if let Some(path) = self.prefetch.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|session| session.handle.cache_path())
        }) {
            crate::debug_log(&format!("prefetch cache-file path={}", path.display()));
        }
        if let Some(previous) = previous {
            let stats = previous.handle.snapshot();
            crate::debug_log(&format!(
                "prefetch disabled reason=replaced host={} bytes_fetched={}",
                previous.host, stats.bytes_fetched
            ));
            // `shutdown` joins the server task (bounded) and then deletes the
            // session cache file: up to a few seconds the new stream must not
            // wait for. A 画质 switch calls this on the decode thread, right
            // before the first frame of the new tier is due, so the old engine
            // is retired on its own thread.
            std::thread::spawn(move || drop(previous));
        }
    }

    /// Install (or replace) this player's audio proxy.
    ///
    /// The previous engine is retired on a throwaway thread: `shutdown` waits
    /// (bounded, 3 s) for its server task before it deletes the session cache
    /// file, and this is called from the decode thread at a seek swap, where a
    /// wait would be time the first frame of the target does not have.
    ///
    /// Replacing the engine is what keeps "one audio cache file per session"
    /// true: the engine that was feeding the sound the user just left is closed
    /// and its file deleted (its own `cache removed` line), and the engine of
    /// the sound now playing carries on as the session's.
    pub fn install_audio_prefetch(&self, session: AudioPrefetchSession) {
        // Same startup gate as the picture's proxy: a fresh engine starts with
        // its speculative (whole-file) queue held, so nothing competes with the
        // first frames. The sound's hot window is never gated.
        if crate::media_hooks::startup_gate_enabled() {
            session.handle.set_background_paused(true);
        }
        let previous = {
            let mut guard = match self.audio_prefetch.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.replace(session)
        };
        if let Some(previous) = previous {
            crate::debug_log(&format!(
                "audio prefetch disabled reason=replaced local={}",
                previous.handle.local_url
            ));
            retire_audio_prefetch(previous.handle);
        }
    }

    /// The URL to hand `ffplay` for the DASH audio track `direct`: the audio
    /// proxy's loopback URL while that proxy was opened for exactly this track,
    /// otherwise the CDN URL unchanged — the same discipline as
    /// [`PlaybackControl::media_url_for`] for the picture.
    ///
    /// Called at the top of every sound (re)spawn, so a seek, a pause/resume, a
    /// volume restart and an audio-track switch all re-evaluate the URL instead
    /// of reusing a stale one.
    pub fn audio_media_url(&self, direct: &str) -> String {
        let guard = match self.audio_prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_ref() {
            Some(session) if session.source_url == direct => session.handle.local_url.clone(),
            _ => direct.to_string(),
        }
    }

    /// The audio URL for the track [`PlaybackControl::audio_index`] selects,
    /// through the audio proxy when one is installed for it.
    ///
    /// Every `ffplay` spawn in the decode loop reads this instead of
    /// `PlayInfo::audio_url_for`, so there is one place where "where do the
    /// sound's bytes come from" is decided.
    pub fn audio_url_for(&self, info: &bilibili::PlayInfo) -> Option<String> {
        let direct = info.audio_url_for(self.audio_index())?;
        Some(self.audio_media_url(&direct))
    }

    /// The audio proxy's cache file **when it holds the whole track**, for a
    /// seek shadow.
    ///
    /// The audio track is a few MB and its read-ahead covers it early, so a
    /// cached seek usually has every byte on disk. Opening that file directly
    /// makes `ffplay` create its WASAPI session and report its first position
    /// without waiting on the picture proxy (which the shadow's own decoder is
    /// reading at the same instant). The audio's own cache file is read-only
    /// here; the running sound keeps the proxy and its cursor untouched.
    fn audio_cache_file(&self) -> Option<PathBuf> {
        let guard = match self.audio_prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let session = guard.as_ref()?;
        let total = session.handle.snapshot().bytes_total?;
        if total == 0 {
            return None;
        }
        session.handle.cache_path_for(0, total)
    }

    /// Pre-warm the audio proxy at the byte offset of `ms`, so a pre-rolled
    /// seek's sound reads locally from its first read.
    ///
    /// The bytes go into the proxy's seek-point cache, which is served *before*
    /// the response lock and without moving the read cursor: the sound that is
    /// still playing keeps its window and its open body, and the seek's own
    /// `ffplay` is answered from memory. `length × position / duration` is the
    /// only mapping available before the demuxer's index is read, so the
    /// backoff covers the estimate's error.
    ///
    /// `None` when there is no audio proxy, no known length or no duration;
    /// otherwise `(offset, length, covered)` for the `seek shadow audio prime`
    /// line.
    fn prime_audio_at(&self, ms: u64) -> Option<(u64, u64, bool)> {
        let duration = self.duration_ms();
        if duration == 0 || ms >= duration {
            return None;
        }
        let guard = match self.audio_prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let session = guard.as_ref()?;
        let total = session.handle.snapshot().bytes_total?;
        if total == 0 {
            return None;
        }
        let offset = audio_prime_offset(total, ms, duration);
        let covered = session.handle.prime_at(offset, AUDIO_PRIME_BYTES);
        Some((offset, total, covered))
    }

    /// Remove the audio proxy from the player and log one line.
    fn take_audio_prefetch(&self, reason: &str) -> Option<AudioPrefetchSession> {
        let session = {
            let mut guard = match self.audio_prefetch.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.take()
        }?;
        let stats = session.handle.snapshot();
        crate::debug_log(&format!(
            "audio prefetch disabled reason={reason} local={} bytes_fetched={} failed_ranges={}",
            session.handle.local_url, stats.bytes_fetched, stats.failed_ranges
        ));
        Some(session)
    }

    /// Whether a proxy is installed for this player.
    pub fn prefetch_active(&self) -> bool {
        match self.prefetch.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// Hold (`true`) or release the installed proxy's background work.
    ///
    /// Called by the pacer's 4 Hz sampler from the playback state it already
    /// tracks (first frame, ring occupancy, stall counter, buffering overlay),
    /// never from the UI thread. A no-op when no proxy is installed, and cheap
    /// enough to call every tick: one uncontended lock plus an atomic store.
    pub fn set_prefetch_background_paused(&self, paused: bool) {
        if !crate::media_hooks::startup_gate_enabled() {
            return;
        }
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(session) = guard.as_ref() {
            session.handle.set_background_paused(paused);
        }
        drop(guard);
        // The audio proxy's only background work is the whole-file queue, which
        // nothing reads (a seek primes its own engine): it follows the picture's
        // gate so a fresh opponent never starts a whole audio download either.
        let audio = match self.audio_prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(session) = audio.as_ref() {
            session.handle.set_background_paused(paused);
        }
    }

    /// Monotonic ms of this player's first frame (`0` until it arrives).
    fn first_frame_at_ms(&self) -> u64 {
        self.first_frame_at_ms.load(Ordering::Relaxed)
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
        guard.as_ref().map(|session| session.source_url.clone())
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

    /// Contiguous bytes of media the proxy already holds from `offset` forward,
    /// plus the bytes it holds ahead of its own read cursor, under one lock.
    ///
    /// This is the startup gate's readiness signal. `cached_bytes_at(offset)`
    /// alone is not enough: the proxy drops bytes behind the read cursor, so
    /// once the decoder has read the head of the stream (which it does within a
    /// few hundred milliseconds) a probe at the start offset reports 0 even
    /// though the cache ahead is full. `read_ahead_bytes` measures exactly that
    /// window, and for a start at the head of the media the two describe the
    /// same contiguous run.
    ///
    /// `None` = no proxy is feeding this decode session.
    pub fn prefetch_cache_probe(&self, offset: u64) -> Option<(u64, u64)> {
        if !self.prefetch_in_use.load(Ordering::SeqCst) {
            return None;
        }
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.as_ref().map(|session| {
            let at_offset = session.handle.cached_bytes_at(offset);
            let read_ahead = session.handle.snapshot().read_ahead_bytes;
            (at_offset, read_ahead)
        })
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

    /// Path of this session's cache file when it holds the whole region from
    /// `offset` to the end of the media, so ffmpeg can decode the local file
    /// without ever reaching a hole.

    /// `BILI_CACHE_LOCAL_MB=<n>` lowers the requirement to `n` contiguous MiB
    /// from the offset (QA A/B for the seek latency of the local path); the
    /// default `0` is "to the end of the file", which is the only rule that
    /// cannot break mid-playback.
    /// `ms` is the session's start position. The check backs off before it: the
    /// byte offset of a timestamp is only known to the demuxer, so the file must
    /// be complete from *one eighth of the media before* the estimate to the end
    /// — then whatever offset ffmpeg's own index picks, it cannot run into a
    /// hole. A completely downloaded file therefore serves every seek locally.
    pub fn prefetch_cache_file(&self, ms: u64) -> Option<std::path::PathBuf> {
        self.probe_seek_cache(ms).and_then(|probe| probe.path)
    }

    /// The one seek-cache decision, with the numbers it was made from.
    ///
    /// **The disk map is the only evidence.** A target counts as cached when the
    /// session cache file holds `[safe, end)` on disk (`cache_path_for` →
    /// `DiskCache::covers_range`) — the same test that lets the decoder open the
    /// file locally, so "cached" and "zero upstream" can never disagree.
    ///
    /// It used to answer from a *size estimate* — a warm point within half a
    /// chunk, or a target inside the read-ahead window — which claimed cached
    /// for regions the map did not hold. The seek then fetched ~8 MB upstream
    /// and took 1.7–3.1 s while the overlay stayed hidden (`overlay_ms=0`).
    fn probe_seek_cache(&self, ms: u64) -> Option<SeekCacheProbe> {
        if !self.prefetch_in_use.load(Ordering::SeqCst) {
            return None;
        }
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let session = guard.as_ref()?;
        let total = session.handle.snapshot().bytes_total?;
        let duration = self.duration_ms();
        if total == 0 || duration == 0 || ms >= duration {
            return None;
        }
        let estimate = (total as u128)
            .saturating_mul(ms as u128)
            .saturating_div(duration as u128) as u64;
        let safe = estimate.saturating_sub(total / 8);
        let need = local_decode_run(total - safe);
        let path = session.handle.cache_path_for(safe, need);
        let (run, ranges, bytes) = session.handle.disk_cover(safe);
        Some(SeekCacheProbe {
            path,
            safe,
            need,
            run,
            ranges,
            bytes,
        })
    }

    /// Whether a seek to `ms` can be served entirely from the cache file.
    ///
    /// `true` only when [`Self::probe_seek_cache`] found the whole decoded
    /// region on disk; everything else is "unknown" (see the pacer's seek
    /// decision) and the 450 ms grace decides whether the overlay appears.
    fn seek_is_cached(&self, ms: u64) -> bool {
        self.probe_seek_cache(ms)
            .map(|probe| probe.path.is_some())
            .unwrap_or(false)
    }

    /// The one `seek cached=` line: what the disk map said about this seek.
    fn log_seek_cache(&self, target_ms: u64) -> bool {
        let probe = self.probe_seek_cache(target_ms);
        let cached = probe.as_ref().is_some_and(|probe| probe.path.is_some());
        match probe {
            Some(probe) => crate::debug_log(&format!(
                "seek cached={} target={target_ms} disk_ranges={}/{}B disk_run={}B need={}B safe={} src={}",
                u8::from(cached),
                probe.ranges,
                probe.bytes,
                probe.run,
                probe.need,
                probe.safe,
                if cached { "disk" } else { "net" },
            )),
            None => crate::debug_log(&format!(
                "seek cached=0 target={target_ms} disk_ranges=0/0B disk_run=0B need=0B safe=0 src=net reason=no-session"
            )),
        }
        cached
    }

    /// Preload the tracks *after* the current one, in playlist order, into their
    /// own session cache files (background priority, one connection, cancel with
    /// [`PlaybackControl::cancel_preload`]).
    ///
    /// **Follow-up call site** (the UI session owns `main.rs`): in the playlist
    /// host's `advance`/`play_item` path — where the ordered `bvid`s of
    /// `playlist()` are known — resolve the next N items with
    /// `session.playurl(bvid, cid)` and call this with, for each, the video track
    /// the decoder would pick:
    ///
    /// ```ignore
    /// let key = format!("{bvid}-{cid}-{}-{}", track.id, track.codecid);
    /// let url = track.urls.first().cloned().unwrap_or_else(|| track.url.clone());
    /// control.preload_playlist(&[(key, url)]);
    /// ```
    ///
    /// The key must match the one `player::run` names its cache file with, which
    /// is exactly this format, so the preloaded file is *reused* when that item
    /// is opened (and removed when its own player closes).
    pub fn preload_playlist(&self, order: &[(String, String)]) -> usize {
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .map(|session| session.handle.preload_keys(order))
            .unwrap_or(0)
    }

    /// Stop using the proxy: log why, then drop the handle (server task,
    /// fetch workers, listener). Returns whether a proxy was actually running.
    ///
    /// The audio proxy goes with it: once the picture falls back to the CDN,
    /// keeping the sound on a proxy that is about to be replaced would only
    /// split the seek decision in two. The next decode session re-resolves both
    /// URLs, and the sound's own respawn reads the direct one.
    pub fn disable_prefetch(&self, reason: &str) -> bool {
        let audio = self.take_audio_prefetch(reason);
        if let Some(audio) = audio {
            retire_audio_prefetch(audio.handle);
        }
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

    /// Claim this player's single history reporter thread (see
    /// [`HistoryReporter`]). `false` means it already exists: `run` is re-entered
    /// after a pacer panic, and that restart must not open a second one.
    fn claim_history_reporter(&self) -> bool {
        self.history_claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Whether a play press asked for a new session (see [`Self::request_resume`]).
    /// The decode thread's hold loop polls it; it consumes the request with
    /// [`Self::clear_resume_request`].
    fn resume_requested(&self) -> bool {
        self.resume_request.load(Ordering::SeqCst)
    }

    fn clear_resume_request(&self) {
        self.resume_request.store(false, Ordering::SeqCst);
    }

    /// Flip the paused state and return the new value.
    ///
    /// While the buffer guard holds playback, the first press means "play":
    /// it releases the auto pause instead of latching a user pause.
    ///
    /// A press while the track is ended (or while no decoder is running) also
    /// means "play": the UI shows the play affordance then, yet the control is
    /// not paused, so a plain flip would latch a pause on a picture that is
    /// already stopped. It asks for a fresh session instead.
    pub fn toggle_paused(&self) -> bool {
        if self.auto_paused.swap(false, Ordering::SeqCst) {
            self.paused.store(false, Ordering::SeqCst);
            self.end_buffering("user-play");
            self.request_resume();
            return false;
        }
        let ended = self.ended.load(Ordering::SeqCst);
        let dead = !self.decoder_live.load(Ordering::SeqCst);
        // Only a player that has shown a picture can be "dead": before the
        // first frame a press is an ordinary pause request, and `ended` alone is
        // enough (a real end keeps its decoder alive in the end hold).
        let played = self.prof.presented.load(Ordering::Relaxed) > 0;
        if !self.paused.load(Ordering::SeqCst) && (ended || (dead && played)) {
            self.request_resume();
            return false;
        }
        let next = !self.paused.load(Ordering::SeqCst);
        self.paused.store(next, Ordering::SeqCst);
        next
    }

    /// A play press is a command to play: when the track was declared over or
    /// no decoder session is running, ask for a fresh one at the current
    /// position. Nothing else recovers a player whose decode thread has stopped
    /// — the sticky `ended` flag would otherwise keep it dead for good.
    ///
    /// In steady state this is two atomic loads and returns.
    fn request_resume(&self) {
        let ended = self.ended.load(Ordering::SeqCst);
        let live = self.decoder_live.load(Ordering::SeqCst);
        if !ended && live {
            return;
        }
        let position = self.playback_position_ms();
        // A press at the real end restarts from the top; a press after a false
        // end (or an interrupted session) resumes where the picture stopped.
        let duration = self.duration_ms();
        let at_end = duration > 0 && position.saturating_add(TRACK_END_SLACK_MS) >= duration;
        let target = if ended && at_end { 0 } else { position };
        // A pause press must not restart anything.
        if self.paused.load(Ordering::SeqCst) {
            return;
        }
        crate::debug_log(&format!(
            "resume request pos_ms={position} target_ms={target} ended={} decoder_running={} dur_ms={duration}",
            u8::from(ended),
            u8::from(live)
        ));
        self.set_auto_paused(false);
        self.pending_seek_from_ms.store(position, Ordering::SeqCst);
        self.position_ms.store(target, Ordering::SeqCst);
        self.pending_seek_ms.store(target, Ordering::SeqCst);
        self.last_seek_target.store(target, Ordering::SeqCst);
        self.last_seek_at.store(monotonic_ms(), Ordering::SeqCst);
        self.resume_request.store(true, Ordering::SeqCst);
        self.bump_seek();
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Whether the window is held because it is completely covered (f152).
    ///
    /// Read by the pacer's hold and by the occlusion watchdog. It is public
    /// because the QA run reports it; production only writes it from the
    /// watchdog in `main.rs`.
    pub fn occluded(&self) -> bool {
        self.occluded.load(Ordering::SeqCst)
    }

    /// Set or clear the occlusion hold. Returns whether the flag changed.
    ///
    /// Distinct from [`Self::set_paused`] on purpose: the watchdog must be able
    /// to release only its own hold and never a pause the user asked for.
    pub fn set_occluded(&self, on: bool) -> bool {
        self.occluded.swap(on, Ordering::SeqCst) != on
    }

    /// Whether the buffer guard is holding playback (auto pause).
    fn auto_paused(&self) -> bool {
        self.auto_paused.load(Ordering::SeqCst)
    }

    fn set_auto_paused(&self, on: bool) {
        self.auto_paused.store(on, Ordering::SeqCst);
    }

    /// Whether playback is currently paused (readable from the UI thread).
    ///
    /// An auto pause reports as paused: the user sees a still picture and the
    /// buffering overlay, and the play/pause affordance must offer "play".
    pub fn paused(&self) -> bool {
        self.is_paused() || self.auto_paused()
    }

    /// Whether the *user* paused (a real play/pause latch), as opposed to the
    /// buffer guard's auto pause. The watch-history reporter maps only this to
    /// the API's `play_type=2` / `3`: a buffering hold is not a pause and must
    /// not produce a pause/resume pair around every stall.
    pub fn user_paused(&self) -> bool {
        self.is_paused()
    }

    /// Whether this session's track has finished: sticky for the current
    /// playback session and cleared by a seek, a quality change or a new
    /// [`PlaybackControl`].
    ///
    /// Two signals feed it, and **neither may be a playhead alone**:
    ///
    /// * the decode loop latched it when the decoder really reported the end of
    ///   the stream ([`Self::note_stream_finished`] — its own gate requires the
    ///   position to be at the end too), or
    /// * the playhead reached the known duration **and** the picture has stopped
    ///   advancing for [`END_POSITION_IDLE_MS`].
    ///
    /// The second condition is the fix for the false "播放完成" mid-video: a
    /// duration that is shorter than the stream (the API rounds to whole
    /// seconds and can lag the real media) used to latch the end while frames
    /// were still arriving. A picture that is still being presented now keeps
    /// the track alive no matter where the playhead reads.
    pub fn track_ended(&self) -> bool {
        if self.ended.load(Ordering::SeqCst) {
            return true;
        }
        if self.is_paused() {
            return false;
        }
        let duration = self.duration_ms();
        if duration == 0 {
            return false;
        }
        let position = self.engine_position_ms();
        if position.saturating_add(TRACK_END_SLACK_MS) < duration {
            return false;
        }
        // At (or past) the end. The picture decides: a frame presented within
        // the idle window means the decoder is still producing, so this is a
        // duration/position mismatch, not the end of the track.
        let presented_at = self.presented_at_ms.load(Ordering::SeqCst);
        let idle_ms = if presented_at == 0 {
            u64::MAX
        } else {
            monotonic_ms().saturating_sub(presented_at)
        };
        if idle_ms < END_POSITION_IDLE_MS {
            self.end_trace("position", false);
            return false;
        }
        self.ended.store(true, Ordering::SeqCst);
        self.end_trace("position", true);
        true
    }

    /// Latch the end of the track when the decoder reports the stream finished.
    ///
    /// A read error also marks the source as done, so the position is checked
    /// as well: only "at (or past) the end" counts, which keeps a mid-file
    /// failure from advancing the queue. An unknown duration (`0`) trusts the
    /// decoder. The end-of-stream report itself is only accepted when the ring
    /// is drained as well: frames still queued mean the picture has not been
    /// shown yet, so the track cannot be over.
    fn note_stream_finished(&self) {
        let duration = self.duration_ms();
        let position = self.engine_position_ms();
        let ring = self.ring_len();
        let at_end = duration == 0
            || position.saturating_add(TRACK_END_FAILSAFE_MS) >= duration;
        if !at_end {
            self.end_trace("eof", false);
            return;
        }
        if ring > 0 {
            // The decoder closed with frames still in the ring: those frames
            // are the real end of the picture. Let the pacer drain them; the
            // position/idle rule (or the next session's cleanup) latches then.
            self.end_trace("eof", false);
            return;
        }
        self.ended.store(true, Ordering::SeqCst);
        self.end_trace("eof", true);
    }

    /// The one `end trace` line (`BILI_END_TRACE=1`): every decision about
    /// ending the track, with the numbers it was made from.
    ///
    /// `src` is `position`, `eof` or `run` (the decode thread's own end hold);
    /// a rejected position/eof decision is rate-limited to two per second so a
    /// long video that runs past a short API duration produces a readable trace
    /// instead of a flood.
    fn end_trace(&self, src: &str, latched: bool) {
        if !self.end_trace {
            return;
        }
        if !latched {
            let now = monotonic_ms();
            let last = self.last_end_trace_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) < 500 {
                return;
            }
            self.last_end_trace_ms.store(now, Ordering::Relaxed);
        }
        let audio_stamp = self.audio_clock_stamp_ms.load(Ordering::SeqCst);
        let audio_age = if audio_stamp == 0 {
            -1
        } else {
            monotonic_ms().saturating_sub(audio_stamp) as i64
        };
        let frame_us = self.prof.frame_us.load(Ordering::Relaxed);
        let ring_s = if frame_us == 0 {
            0.0
        } else {
            (self.ring_len() as f64 * frame_us as f64) / 1_000_000.0
        };
        crate::debug_log(&format!(
            "end trace pos={} dur={} src={src} latch={} api_dur={} dec_dur={} dur_src={} audio_age={audio_age} ring_s={ring_s:.2} presented={} decoder_running={} buffering={} paused={} ended={} seek_seq={}",
            self.engine_position_ms(),
            self.duration_ms(),
            u8::from(latched),
            self.api_duration_ms.load(Ordering::SeqCst),
            self.decoder_duration_ms.load(Ordering::SeqCst),
            self.duration_source(),
            self.prof.presented.load(Ordering::Relaxed),
            u8::from(self.decoder_live.load(Ordering::SeqCst)),
            u8::from(self.buffering()),
            u8::from(self.is_paused()),
            u8::from(self.ended.load(Ordering::SeqCst)),
            self.seek_seq.load(Ordering::SeqCst),
        ));
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
        // A seek that has not put its picture on screen yet owns the readout:
        // the target is what the user asked for, and the old picture that is
        // still being presented must not drag the playhead back to its own
        // position for the length of the pre-roll.
        let hold = self.position_hold_ms.load(Ordering::SeqCst);
        if hold != u64::MAX {
            return hold;
        }
        self.engine_position_ms()
    }

    /// Where the *picture* is: the audio clock while it is fresh, the pacer's
    /// own position otherwise. The seek hold ([`Self::playback_position_ms`]) is
    /// deliberately not applied — the end-of-track rules and the seek's
    /// "where did this jump come from" must read the picture that is on screen,
    /// never the position the user just asked for.
    fn engine_position_ms(&self) -> u64 {
        // f151: a release freezes the picture where it was let go. The audio
        // clock and `position_ms` both already point at the target, so neither
        // may answer for the frame on screen until the new position's first
        // frame is presented.
        if self.seek_freeze() {
            let presented = self.presented_ms.load(Ordering::SeqCst);
            if presented != 0 {
                return presented;
            }
        }
        // A paused picture is a held position: the audio clock is suspended
        // with it (see the A/V guard), so extrapolating it would creep forward
        // while nothing moves.
        if self.buffering() || self.is_paused() {
            return self.position_ms();
        }
        self.audio_clock().unwrap_or_else(|| self.position_ms())
    }

    /// f151: the release froze the picture; no old-session frame may replace
    /// the one on screen until the new position's first frame.
    pub fn seek_freeze(&self) -> bool {
        self.seek_freeze_at.load(Ordering::SeqCst) != 0
    }

    /// Monotonic ms of the release that froze the picture (`0` = not frozen).
    fn seek_freeze_at_ms(&self) -> u64 {
        self.seek_freeze_at.load(Ordering::SeqCst)
    }

    /// The new position's first frame is about to be shown: the picture follows
    /// the decoder again. The mailbox reads [`Self::seek_freeze`], so this must
    /// happen immediately before that frame is pushed.
    fn end_picture_freeze(&self) {
        self.seek_freeze_at.store(0, Ordering::SeqCst);
    }

    /// Content position of the frame currently on screen (f151 `pos_real`).
    pub fn presented_position_ms(&self) -> u64 {
        self.presented_ms.load(Ordering::SeqCst)
    }

    /// Claim the readout for the seek target: the playhead and the time label
    /// show it from the release until the first frame of the new position is
    /// presented ([`Self::clear_position_hold`], from `first_frame_handover`).
    fn hold_position(&self, ms: u64) {
        self.position_hold_ms.store(ms, Ordering::SeqCst);
    }

    /// The new picture of a seek is on screen: stop holding the readout.
    fn clear_position_hold(&self) {
        self.position_hold_ms.store(u64::MAX, Ordering::SeqCst);
    }

    /// Record the newest audio playback position (ms) reported by ffplay and
    /// mark the clock fresh. `generation` is the sample generation claimed by
    /// the process that produced it (see
    /// [`Self::begin_audio_clock_generation`]).
    ///
    /// **The clock is slewed, not stepped.** `ffplay -stats` reports its
    /// position in a burst every few hundred ms, quantised to the audio buffer;
    /// a sample therefore lands tens of milliseconds behind (or ahead of) the
    /// extrapolation it interrupts. Adopting it as the new base — which is what
    /// this used to do — stepped the master clock backwards, and the pacer paid
    /// every step by holding the picture for that much wall time. Measured: a
    /// 30 fps window presenting 26–28 frames in a second with a full ring, no
    /// dropped frames and `sync_ms` at the whole second — the residual dips.
    ///
    /// The fix keeps the clock *continuous* (base = the extrapolation at this
    /// instant) and stores a rate correction that closes the error over
    /// [`AUDIO_CLOCK_SLEW_MS`] of playback. A sample that is behind slows the
    /// clock by up to 2 %, which costs at most ~0.6 frames a second instead of
    /// the whole error at once; a sample that is ahead speeds it up the same
    /// way. Real drift is still tracked, just without a step.
    ///
    /// Returns whether the sample was accepted. A sample stamped with an older
    /// generation is dropped: after a backward seek the replaced ffplay keeps
    /// emitting `-stats` for a moment, and letting its position through would
    /// make the new session look seconds "behind" and re-seek it forward.
    ///
    /// **A forward jump is not adopted on one sample.** A `-stats` report that
    /// lands more than [`AUDIO_CLOCK_GUARD_MS`] ahead is either the audio
    /// **device** position of a just-resumed process or a real skip inside
    /// `ffplay`, and the next burst tells them apart: a skip repeats at the new
    /// base (two reports within [`AUDIO_CLOCK_CONFIRM_MS`]) and re-seeds the
    /// clock, the device artefact returns to the content and the clock never
    /// moved. Nothing here may reach the A/V guard as a *picture hold*: a hold
    /// is only ever paid for a confirmed relation.
    pub fn set_audio_clock_ms(&self, generation: u64, position_ms: u64) -> bool {
        let now = monotonic_ms().max(1);
        let stamp = self.audio_clock_stamp_ms.load(Ordering::SeqCst);
        let base = self.audio_clock_ms.load(Ordering::SeqCst);
        let speed = self.speed_milli.load(Ordering::SeqCst) as f64 / 1000.0;
        if stamp == 0 || now.saturating_sub(stamp) > AUDIO_CLOCK_FRESH_MS {
            // No usable clock to stay continuous with (first sample of this
            // generation, or one so stale the extrapolation is fiction).
            self.clear_jump_candidate();
            self.audio_clock_ms.store(position_ms, Ordering::SeqCst);
            self.audio_clock_ppm.store(0, Ordering::SeqCst);
        } else {
            let age = now.saturating_sub(stamp);
            let extrapolated = base.saturating_add((age as f64 * speed) as u64);
            let error = position_ms as i64 - extrapolated as i64;
            // Profiler: how much the raw sample disagreed, in both directions.
            let magnitude = error.unsigned_abs();
            if error < 0 {
                self.audio_clock_back_ms
                    .fetch_add(magnitude, Ordering::Relaxed);
            } else {
                self.audio_clock_fwd_ms
                    .fetch_add(magnitude, Ordering::Relaxed);
            }
            self.audio_clock_samples.fetch_add(1, Ordering::Relaxed);
            self.audio_clock_worst_ms.fetch_max(magnitude, Ordering::Relaxed);
            // Close the error over AUDIO_CLOCK_SLEW_MS of playback, capped so a
            // bogus sample can never wrench the rate.
            if clock_trace() {
                crate::debug_log(&format!(
                    "clk age_ms={age} reported_ms={position_ms} extrapolated_ms={extrapolated} err_ms={error} speed={speed:.3}"
                ));
            }
            if error > AUDIO_CLOCK_JUMP_MS as i64 {
                // After a suspension the process' first report is the audio
                // device (frozen content + the suspension's length), never the
                // content: that is the measured t≈6 s dip. It is dropped here
                // and the clock simply keeps extrapolating from the position the
                // sound was frozen at.
                let quiet = self.resume_age_ms(now) <= AUDIO_CLOCK_RESUME_QUIET_MS;
                if quiet {
                    self.clear_jump_candidate();
                    self.audio_clock_ms.store(extrapolated, Ordering::SeqCst);
                    self.clock_jump_log(
                        now, age, extrapolated, position_ms, error, false, "resume-device",
                    );
                } else if error as u64 <= AUDIO_CLOCK_GUARD_MS {
                    // A small lurch is cheaper to take: the pacer's cost is at
                    // most the guard's own lead budget.
                    self.clear_jump_candidate();
                    self.audio_clock_ms.store(position_ms, Ordering::SeqCst);
                    self.audio_clock_ppm.store(0, Ordering::SeqCst);
                    self.clock_jump_log(
                        now, age, extrapolated, position_ms, error, true, "under-guard",
                    );
                } else if self.jump_candidate_confirmed(position_ms, now, speed) {
                    // The next burst agrees: the content really skipped ahead.
                    // Re-seed instead of holding the picture for a slew that
                    // would need tens of seconds.
                    self.clear_jump_candidate();
                    self.audio_clock_ms.store(position_ms, Ordering::SeqCst);
                    self.audio_clock_ppm.store(0, Ordering::SeqCst);
                    self.clock_jump_log(
                        now, age, extrapolated, position_ms, error, true, "confirmed",
                    );
                } else {
                    // One report only: the clock keeps the trajectory it had, so
                    // this sample cannot reach the A/V guard. The report is
                    // remembered: a second one at the same base re-seeds.
                    self.audio_clock_ms.store(extrapolated, Ordering::SeqCst);
                    self.audio_jump_cand_ms.store(position_ms, Ordering::SeqCst);
                    self.audio_jump_cand_at_ms.store(now, Ordering::SeqCst);
                    self.clock_jump_log(
                        now, age, extrapolated, position_ms, error, false, "unconfirmed",
                    );
                }
            } else if error < -(AUDIO_CLOCK_LURCH_MS as i64) {
                // The mirror case, and the only backward step that is still
                // worth taking: the sound really is a guard-scale distance
                // behind the picture (a device glitch, a resumed process that
                // lost its buffer). Holding the picture for it is correct — the
                // alternative is a picture that runs ahead of the sound by that
                // much for the rest of the session, because a 2 % slew would
                // need tens of seconds to close it.
                self.clear_jump_candidate();
                self.audio_clock_ms.store(position_ms, Ordering::SeqCst);
                self.audio_clock_ppm.store(0, Ordering::SeqCst);
            } else {
                // Everything below the lurch threshold — including a sample
                // that lands behind the extrapolation — is closed as a rate
                // correction over [`AUDIO_CLOCK_SLEW_MS`]. A small backward step
                // is never worth it: every one of them made the pacer hold the
                // picture long enough to lose frames, which is the residual dip
                // (measured 26–28 frames in a second with a full ring,
                // `skipped=0`, and `sync_ms` swallowing the whole second).
                self.clear_jump_candidate();
                let ppm = error as f64 * 1.0e6 / AUDIO_CLOCK_SLEW_MS as f64;
                self.audio_clock_ppm.store(
                    ppm.clamp(-AUDIO_CLOCK_MAX_PPM, AUDIO_CLOCK_MAX_PPM) as i64,
                    Ordering::SeqCst,
                );
                self.audio_clock_ms.store(extrapolated, Ordering::SeqCst);
            }
        }
        self.audio_clock_stamp_ms.store(now, Ordering::SeqCst);
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

    /// Forget the forward-jump candidate: the clock has just taken a decision
    /// about this sample, so a *later* jump can never be "confirmed" against a
    /// stale one.
    fn clear_jump_candidate(&self) {
        self.audio_jump_cand_ms.store(0, Ordering::SeqCst);
        self.audio_jump_cand_at_ms.store(0, Ordering::SeqCst);
    }

    /// Milliseconds since the sound was last released from a suspension;
    /// `u64::MAX` when it never was (see [`AUDIO_CLOCK_RESUME_QUIET_MS`]).
    fn resume_age_ms(&self, now: u64) -> u64 {
        let at = self.audio_resume_at_ms.load(Ordering::SeqCst);
        if at == 0 {
            u64::MAX
        } else {
            now.saturating_sub(at)
        }
    }

    /// Whether the remembered forward-jump candidate is corroborated by
    /// `position_ms`: the two reports must lie within
    /// [`AUDIO_CLOCK_CONFIRM_MS`] of each other on the candidate's own
    /// trajectory. That is what a real skip inside `ffplay` looks like (every
    /// following burst stays at the skipped position); a device artefact does
    /// not (the next burst returns to the content position).
    fn jump_candidate_confirmed(&self, position_ms: u64, now: u64, speed: f64) -> bool {
        let cand = self.audio_jump_cand_ms.load(Ordering::SeqCst);
        let at = self.audio_jump_cand_at_ms.load(Ordering::SeqCst);
        if cand == 0 || at == 0 {
            return false;
        }
        let expected = cand.saturating_add((now.saturating_sub(at) as f64 * speed) as u64);
        position_ms.abs_diff(expected) <= AUDIO_CLOCK_CONFIRM_MS
    }

    /// The `audio clock jump` evidence line: one `-stats` report that arrived
    /// more than [`AUDIO_CLOCK_JUMP_MS`] ahead of the extrapolated clock, with
    /// the process it came from and what coincided with it — a suspension
    /// release (`resume_age_ms`/`frozen_ms`), a buffering hold, a seek shadow's
    /// attach (a fresh generation always seeds and never gets here).
    /// `accepted=0` is a report the clock refused, `reason` names why.
    fn clock_jump_log(
        &self,
        now: u64,
        age: u64,
        from: u64,
        to: u64,
        error: i64,
        accepted: bool,
        reason: &str,
    ) {
        let resume_at = self.audio_resume_at_ms.load(Ordering::SeqCst);
        let freeze_at = self.audio_freeze_at_ms.load(Ordering::SeqCst);
        let resume_age = if resume_at == 0 {
            -1
        } else {
            now.saturating_sub(resume_at) as i64
        };
        let frozen_ms = if resume_at == 0 || freeze_at == 0 {
            -1
        } else {
            resume_at.saturating_sub(freeze_at) as i64
        };
        crate::debug_log(&format!(
            "audio clock jump from={from} to={to} dt={age} err={error} accepted={} reason={reason} pid={} resume_age_ms={resume_age} frozen_ms={frozen_ms} buffering={} samples={}",
            u8::from(accepted),
            self.audio_pid(),
            u8::from(self.buffering()),
            self.audio_clock_samples.load(Ordering::Relaxed),
        ));
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
    ///
    /// A new generation is a *legitimate* discontinuity (a spawn, a seek, a
    /// volume restart): its first sample seeds the clock, so the jump guard, the
    /// resume quiet window and their evidence are all dropped with the old
    /// process' position.
    fn begin_audio_clock_generation(&self) -> u64 {
        let generation = self.audio_clock_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.audio_clock_stamp_ms.store(0, Ordering::SeqCst);
        self.audio_clock_ppm.store(0, Ordering::SeqCst);
        self.clear_jump_candidate();
        self.audio_resume_at_ms.store(0, Ordering::SeqCst);
        self.audio_freeze_at_ms.store(0, Ordering::SeqCst);
        generation
    }

    /// Remember when the newest `ffplay` was spawned, so the first position it
    /// reports can be measured against it (the `audio clock first` line).
    fn note_audio_spawn(&self) {
        self.audio_spawn_at_ms
            .store(monotonic_ms(), Ordering::SeqCst);
        self.audio_first_logged_gen
            .store(u64::MAX, Ordering::SeqCst);
    }

    /// Milliseconds from that spawn to now, once per generation; `None` when
    /// this generation's first sample was already reported.
    fn audio_first_sample_ms(&self, generation: u64) -> Option<u64> {
        if self
            .audio_first_logged_gen
            .swap(generation, Ordering::SeqCst)
            == generation
        {
            return None;
        }
        let spawned = self.audio_spawn_at_ms.load(Ordering::SeqCst);
        if spawned == 0 {
            return None;
        }
        Some(monotonic_ms().saturating_sub(spawned))
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
        // The slew correction is part of the clock's *rate*, so it is applied to
        // the extrapolation and never to the stored sample.
        let ppm = self.audio_clock_ppm.load(Ordering::SeqCst) as f64;
        let rate = speed * (1.0 + ppm / 1.0e6);
        Some(base.saturating_add((age as f64 * rate) as u64))
    }

    /// The last position `ffplay` reported, **without** freshness or
    /// extrapolation. Used while the audio process is suspended by the A/V
    /// guard: its clock is frozen on purpose, so extending it with the wall
    /// clock would be a lie and the guard could never measure the picture
    /// catching up.
    fn audio_clock_base(&self) -> Option<u64> {
        if self.audio_clock_stamp_ms.load(Ordering::SeqCst) == 0 {
            return None;
        }
        Some(self.audio_clock_ms.load(Ordering::SeqCst))
    }

    /// Freeze the audio clock at `ms`, the extrapolated position at the instant
    /// the process is suspended.
    ///
    /// `ffplay` reports a position only in its `-stats` burst (a few hundred ms
    /// apart), so the *last* sample can be stale by that much. Suspending on the
    /// stale value would make the guard measure a lead that is already gone and
    /// release the sound again — a flap. The extrapolated value is exactly where
    /// the process will resume, and a suspended process emits no new samples, so
    /// nothing overwrites it until the resume.
    fn freeze_audio_clock_at(&self, ms: u64) {
        self.audio_clock_ms.store(ms, Ordering::SeqCst);
        // A suspended process has no rate to correct: the clock is the frozen
        // position until it is rebased on the resume sample.
        self.audio_clock_ppm.store(0, Ordering::SeqCst);
        let now = monotonic_ms().max(1);
        self.audio_clock_stamp_ms.store(now, Ordering::SeqCst);
        self.audio_freeze_at_ms.store(now, Ordering::SeqCst);
        // A freeze invalidates a pending jump: it belonged to a clock that no
        // longer exists.
        self.clear_jump_candidate();
    }

    /// Re-anchor the clock's extrapolation at the position it already holds.
    ///
    /// Called when a pause ends: the stored sample is still the content
    /// position, but its stamp is from *before* the pause, and extending it with
    /// the wall clock would pretend the sound played through the pause and make
    /// it look seconds ahead of the picture.
    ///
    /// `suspended` says whether the process' own clock really stopped (a
    /// `NtSuspendProcess` pause). Only then does the release open the quiet
    /// window of [`AUDIO_CLOCK_RESUME_QUIET_MS`]: `ffplay`'s first `-stats`
    /// report after a suspension reads the audio **device** position, i.e. the
    /// frozen content plus the suspension's length, and adopting that is exactly
    /// the t≈6 s dip. A mute fallback never stops the clock, so its release is
    /// not a discontinuity at all.
    fn rebase_audio_clock(&self, suspended: bool) {
        if self.audio_clock_stamp_ms.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.audio_clock_ppm.store(0, Ordering::SeqCst);
        let now = monotonic_ms().max(1);
        self.audio_clock_stamp_ms.store(now, Ordering::SeqCst);
        if suspended {
            self.audio_resume_at_ms.store(now, Ordering::SeqCst);
        }
        self.clear_jump_candidate();
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
        self.api_duration_ms.store(value, Ordering::SeqCst);
    }

    /// Accept the decoder's own `Duration:` line (ffmpeg's stderr) for the
    /// session that just started.
    ///
    /// Only the stream itself can be authoritative when the API value is wrong,
    /// but the line is not always usable: a `cache-file` session reads a
    /// **partial** file (a byte range from the seek point), whose duration is
    /// that fragment's, not the video's. `accept` is therefore false for those,
    /// and the caller decides (see `spawn`'s `local_play`).
    ///
    /// A value outside `0 < ms <= 24 h` is a parse artefact and is dropped.
    fn set_decoder_duration_ms(&self, ms: u64, accept: bool) {
        if !accept || ms == 0 || ms > DURATION_MAX_MS {
            return;
        }
        let previous = self.decoder_duration_ms.swap(ms, Ordering::SeqCst);
        if previous / 1000 != ms / 1000 {
            crate::debug_log(&format!(
                "duration decoder ms={ms} api_ms={} source={}",
                self.api_duration_ms.load(Ordering::SeqCst),
                self.duration_source()
            ));
        }
    }

    /// Forget the previous session's decoder duration. Called when a session
    /// starts, so the API value is in force until this decoder reports its own.
    fn clear_decoder_duration(&self) {
        self.decoder_duration_ms.store(0, Ordering::SeqCst);
    }

    /// Total media duration in milliseconds; `0` until the play info arrives.
    ///
    /// Preference order: the decoder's own `Duration:` when it disagrees with
    /// the API by more than [`DURATION_SOURCE_TOLERANCE`] (the API is whole
    /// seconds and can be shorter than the media, which used to end a video
    /// early), otherwise the API value.
    ///
    /// `BILI_END_TEST=<percent>` shortens whatever is in force by that much.
    /// It is the QA hook that reproduces the owner's false "播放完成" on demand:
    /// the playhead then crosses the "end" in the middle of a perfectly healthy
    /// video, which is exactly the state that used to latch for good.
    pub fn duration_ms(&self) -> u64 {
        let api = self.api_duration_ms.load(Ordering::SeqCst);
        let decoder = self.decoder_duration_ms.load(Ordering::SeqCst);
        let base = if decoder == 0 {
            api
        } else if api == 0 {
            decoder
        } else if (api.abs_diff(decoder) as f64) > api as f64 * DURATION_SOURCE_TOLERANCE {
            decoder
        } else {
            api
        };
        let shorten = end_test_shorten_percent();
        if shorten == 0 || base == 0 {
            base
        } else {
            base.saturating_sub(base.saturating_mul(shorten) / 100)
        }
    }

    /// Which source [`Self::duration_ms`] is currently using (`api`/`decoder`),
    /// for the end trace.
    fn duration_source(&self) -> &'static str {
        let api = self.api_duration_ms.load(Ordering::SeqCst);
        let decoder = self.decoder_duration_ms.load(Ordering::SeqCst);
        if decoder == 0 {
            return "api";
        }
        if api == 0 {
            return "decoder";
        }
        if (api.abs_diff(decoder) as f64) > api as f64 * DURATION_SOURCE_TOLERANCE {
            "decoder"
        } else {
            "api"
        }
    }

    /// Seek to `position_ms`, clamped to the known duration, then restart both
    /// the video and audio streams at that point.
    ///
    /// A seek is deliberately cheap and idempotent: it publishes one atomic
    /// target and bumps the restart trigger, nothing else. A duplicate target
    /// (the release seek that follows a drag seek for the same position) is
    /// dropped, and a seek that arrives while another is in flight simply
    /// replaces the pending target, so the loop performs one restart for the
    /// newest position instead of replaying a queue.
    pub fn seek(&self, position_ms: u64) {
        let duration = self.duration_ms();
        let target = if duration > 0 {
            position_ms.min(duration)
        } else {
            position_ms
        };
        // Same target already queued for the next session: nothing to do.
        if self.pending_seek_ms.load(Ordering::SeqCst) == target {
            crate::debug_log(&format!("seek ignored target_ms={target} reason=queued"));
            return;
        }
        // The UI can deliver two seeks for one release (drag + release). Within
        // the dedupe window the second one is the same intent, not a new one.
        let now = monotonic_ms();
        if self.last_seek_target.load(Ordering::SeqCst) == target
            && now.saturating_sub(self.last_seek_at.load(Ordering::SeqCst)) < SEEK_DEDUPE_MS
        {
            crate::debug_log(&format!("seek ignored target_ms={target} reason=duplicate"));
            return;
        }
        self.last_seek_target.store(target, Ordering::SeqCst);
        self.last_seek_at.store(now, Ordering::SeqCst);
        // A seek is a play command: the end-of-track state is not a pause, so a
        // seek after the last frame must start the new session immediately.
        if self.ended.load(Ordering::SeqCst) {
            self.paused.store(false, Ordering::SeqCst);
        }
        // A pending auto pause belongs to the old position.
        self.set_auto_paused(false);
        // Remember where the seek came from: a backward jump is inside data the
        // proxy already served, and must never flash the loading overlay.
        self.pending_seek_from_ms
            .store(self.engine_position_ms(), Ordering::SeqCst);
        self.position_ms.store(target, Ordering::SeqCst);
        // The readout follows the release, not the old picture that keeps
        // playing until the shadow is ready.
        self.hold_position(target);
        // f151: **freeze the picture in this event.** The readout already
        // jumps; the pixels must stop with it. The pacer stops presenting and
        // the mailbox refuses the old session's next frame (see
        // [`FrameMailbox`]), so the frame on screen at the release is the one
        // that stays there until the new position's first frame. The pacer
        // arms the buffering overlay after `SEEK_FREEZE_OVERLAY_MS` if the
        // swap has not happened by then.
        //
        // 仅音频 has no picture, and a session that has not presented one yet
        // has nothing to freeze; both keep the old restart path and its
        // startup/seek overlay.
        let has_picture = self.presented_ms.load(Ordering::SeqCst) != 0 && !self.audio_only();
        if has_picture {
            let release_ms = monotonic_ms().max(1);
            let was_frozen = self.seek_freeze_at.swap(release_ms, Ordering::SeqCst) != 0;
            if !was_frozen {
                crate::debug_log(&format!(
                    "seek freeze target_ms={target} from_ms={} presented_ms={}",
                    self.pending_seek_from_ms.load(Ordering::SeqCst),
                    self.presented_ms.load(Ordering::SeqCst),
                ));
            }
        }
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

    /// Set the frame-interpolation mode: `0` = off, `1` = blend, `2` =
    /// block-matching motion, `3` = hardware optical flow. Unknown values are
    /// normalised to off. Changing the mode resets the interpolation counters,
    /// so a disabled player reports `generated=0, avg=0, bm=0, ofa=0`.
    ///
    /// The pacer reads this once per presented source frame; enabling it takes
    /// effect on the next source frame, without restarting the decoder.
    pub fn set_interp_mode(&self, mode: u8) {
        // The mode byte is its own persisted value: `InterpMode` accepts 0..=3
        // and maps anything else to off, so a newer `settings.json` can never
        // put the pacer in an unknown state.
        let mode = InterpMode::from_u8(mode).as_u8();
        let previous = self.interp_mode.swap(mode as u64, Ordering::SeqCst);
        if previous == mode as u64 {
            return;
        }
        self.interp_generated.store(0, Ordering::SeqCst);
        self.interp_dropped.store(0, Ordering::SeqCst);
        self.interp_us.store(0, Ordering::SeqCst);
        self.interp_us_min.store(u64::MAX, Ordering::SeqCst);
        self.interp_us_max.store(0, Ordering::SeqCst);
        self.interp_out_hz.store(0, Ordering::SeqCst);
        self.interp_bm_us.store(0, Ordering::SeqCst);
        self.interp_bm_pairs.store(0, Ordering::SeqCst);
        for bucket in &self.interp_bm_hist {
            bucket.store(0, Ordering::SeqCst);
        }
        self.interp_ofa_us.store(0, Ordering::SeqCst);
        self.interp_ofa_pairs.store(0, Ordering::SeqCst);
        self.interp_ofa_flow_us.store(0, Ordering::SeqCst);
        self.interp_ofa_submits.store(0, Ordering::SeqCst);
        for bucket in &self.interp_ofa_hist {
            bucket.store(0, Ordering::SeqCst);
        }
        crate::debug_log(&format!("interp mode={mode} source=control"));
    }

    /// Current frame-interpolation mode (`0` off, `1` blend, `2` block match,
    /// `3` hardware optical flow).
    pub fn interp_mode(&self) -> u8 {
        self.interp_mode.load(Ordering::Relaxed) as u8
    }

    /// The mode as the enum the pacer matches on.
    pub fn interp_mode_kind(&self) -> InterpMode {
        InterpMode::from_u8(self.interp_mode())
    }

    /// `(generated, dropped, avg_ms)` for the current interpolation episode:
    /// display frames produced, display ticks skipped as late, and the average
    /// cost per produced frame in milliseconds (`0.0` while nothing was
    /// generated).
    pub fn interp_stats(&self) -> (u32, u32, f32) {
        let generated = self.interp_generated.load(Ordering::Relaxed);
        let dropped = self.interp_dropped.load(Ordering::Relaxed);
        let avg_ms = if generated == 0 {
            0.0
        } else {
            self.interp_us.load(Ordering::Relaxed) as f32 / generated as f32 / 1000.0
        };
        (
            generated.min(u32::MAX as u64) as u32,
            dropped.min(u32::MAX as u64) as u32,
            avg_ms,
        )
    }

    /// Output rate the pacer resolved for this episode (`0` = nothing to
    /// interpolate, or the mode is off).
    pub fn interp_out_hz(&self) -> u32 {
        self.interp_out_hz.load(Ordering::Relaxed).min(u32::MAX as u64) as u32
    }

    /// Average block-matching pair time in milliseconds for the current
    /// episode (`0.0` until a pair has run). This is the motion path's own
    /// cost, a subset of the per-display-frame `avg_ms` from [`Self::interp_stats`].
    pub fn interp_bm_ms(&self) -> f32 {
        let pairs = self.interp_bm_pairs.load(Ordering::Relaxed);
        if pairs == 0 {
            return 0.0;
        }
        self.interp_bm_us.load(Ordering::Relaxed) as f32 / pairs as f32 / 1000.0
    }

    /// `(p50, p95)` of the pair times in the current episode, ms, from the
    /// 0.5 ms histogram (each value is its bucket's upper edge). `(0, 0)` until
    /// a pair has run. QA-only: the reported average is [`Self::interp_bm_ms`].
    pub fn interp_bm_percentiles(&self) -> (f32, f32) {
        percentiles(&self.interp_bm_hist)
    }

    /// Average optical-flow pair time in ms for the current episode: submit
    /// (upload + execute) + flow readback + every warp of that interval. `0.0`
    /// until a pair has been consumed. This is the cost mode 3 adds on top of
    /// the blend it replaces, so it is a subset of `interp_stats`'s `avg_ms`.
    pub fn interp_ofa_ms(&self) -> f32 {
        let pairs = self.interp_ofa_pairs.load(Ordering::Relaxed);
        if pairs == 0 {
            return 0.0;
        }
        self.interp_ofa_us.load(Ordering::Relaxed) as f32 / pairs as f32 / 1000.0
    }

    /// Average flow readback cost of the current episode, ms (`0.0` until one
    /// ran). The rest of `interp_ofa_ms` is the frame upload and the CPU warp.
    pub fn interp_ofa_flow_ms(&self) -> f32 {
        let pairs = self.interp_ofa_pairs.load(Ordering::Relaxed);
        if pairs == 0 {
            return 0.0;
        }
        self.interp_ofa_flow_us.load(Ordering::Relaxed) as f32 / pairs as f32 / 1000.0
    }

    /// `(p50, p95)` of the optical-flow pair times in the current episode, ms.
    pub fn interp_ofa_percentiles(&self) -> (f32, f32) {
        percentiles(&self.interp_ofa_hist)
    }

    /// Cumulative pairs submitted to the optical-flow engine (`0` unless mode 3
    /// ran); the QA line turns the delta into pairs per second.
    pub fn interp_ofa_submits(&self) -> u64 {
        self.interp_ofa_submits.load(Ordering::Relaxed)
    }

    /// The label of the current mode for the player window (`关闭/混合/块匹配`).
    pub fn interp_mode_text(&self) -> &'static str {
        self.interp_mode_kind().text()
    }

    /// `60 fps · 2.4 ms`, or `—` when nothing is being generated (mode off, or
    /// the display is not faster than the track).
    pub fn interp_stats_text(&self) -> String {
        let out_hz = self.interp_out_hz();
        let (generated, _dropped, avg_ms) = self.interp_stats();
        if self.interp_mode() == InterpMode::Off.as_u8() || out_hz == 0 || generated == 0 {
            return "—".to_string();
        }
        format!("{out_hz} fps · {avg_ms:.1} ms")
    }

    /// Record one produced display frame and the µs its blend (or motion) pass
    /// took.
    fn note_interp_frame(&self, blend_us: u64) {
        self.interp_generated.fetch_add(1, Ordering::Relaxed);
        self.interp_us.fetch_add(blend_us, Ordering::Relaxed);
        self.interp_us_min.fetch_min(blend_us, Ordering::Relaxed);
        self.interp_us_max.fetch_max(blend_us, Ordering::Relaxed);
    }

    /// Record one completed block-matching pair and its measured cost.
    fn note_motion_pair(&self, pair_ms: f32) {
        if !pair_ms.is_finite() || pair_ms < 0.0 {
            return;
        }
        self.interp_bm_pairs.fetch_add(1, Ordering::Relaxed);
        self.interp_bm_us
            .fetch_add((pair_ms * 1000.0) as u64, Ordering::Relaxed);
        let bucket = ((pair_ms / BM_HIST_BUCKET_MS) as usize).min(BM_HIST_BUCKETS - 1);
        self.interp_bm_hist[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Record one completed optical-flow interval: its total pair cost and the
    /// readback part of it, both ms.
    fn note_ofa_pair(&self, pair_ms: f32, flow_ms: f32) {
        if !pair_ms.is_finite() || pair_ms < 0.0 {
            return;
        }
        self.interp_ofa_pairs.fetch_add(1, Ordering::Relaxed);
        self.interp_ofa_us
            .fetch_add((pair_ms * 1000.0) as u64, Ordering::Relaxed);
        if flow_ms.is_finite() && flow_ms >= 0.0 {
            self.interp_ofa_flow_us
                .fetch_add((flow_ms * 1000.0) as u64, Ordering::Relaxed);
        }
        let bucket = ((pair_ms / BM_HIST_BUCKET_MS) as usize).min(BM_HIST_BUCKETS - 1);
        self.interp_ofa_hist[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Record one pair submitted to the optical-flow engine.
    fn note_ofa_submit(&self) {
        self.interp_ofa_submits.fetch_add(1, Ordering::Relaxed);
    }

    /// Publish the output rate the pacer resolved for this episode.
    fn set_interp_out_hz(&self, hz: u32) {
        self.interp_out_hz.store(hz as u64, Ordering::Relaxed);
    }

    /// `(min_ms, max_ms)` of the blend passes in the current episode.
    fn interp_cost_range(&self) -> (f32, f32) {
        let min = self.interp_us_min.load(Ordering::Relaxed);
        let max = self.interp_us_max.load(Ordering::Relaxed);
        let min = if min == u64::MAX { 0 } else { min };
        (min as f32 / 1000.0, max as f32 / 1000.0)
    }

    /// Record display ticks skipped because the clock had already passed them.
    fn note_interp_dropped(&self, count: u64) {
        self.interp_dropped.fetch_add(count, Ordering::Relaxed);
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
        // A per-video failure memo: the next video's streams are new URLs.
        if let Ok(mut guard) = self.prefetch_failed_stream.lock() {
            *guard = None;
        }
        self.clear_track_ended();
    }

    // -- 选集 (episode switch, same window) ---------------------------------

    /// Seed the video this player is on. Called once at open (and again by the
    /// decode thread as it arms), never a restart.
    pub fn set_source(&self, bvid: &str, cid: u64, aid: i64, title: &str) {
        if let Ok(mut source) = self.source.lock() {
            *source = PlaybackSource {
                bvid: bvid.to_string(),
                cid,
                aid,
                title: title.to_string(),
            };
        }
    }

    /// The video this player is on right now (updated the instant a switch is
    /// requested, so the UI and the worker agree).
    pub fn source(&self) -> PlaybackSource {
        self.source
            .lock()
            .map(|source| source.clone())
            .unwrap_or_default()
    }

    /// Monotonic generation of the current video (changes on every switch).
    pub fn source_seq(&self) -> u64 {
        self.source_seq.load(Ordering::SeqCst)
    }

    /// A switch is waiting for the decode thread to pick it up.
    pub fn switch_pending(&self) -> bool {
        self.switch_request
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    /// Take the pending switch (the decode thread's side of the handoff).
    pub fn take_switch_request(&self) -> Option<PlaybackSource> {
        self.switch_request
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Switch this player to another episode **in the same window**.
    ///
    /// The semantics are the quality-restart path, extended to a different
    /// video: the seek generation is bumped so the running decode session
    /// retires (old worker exits), the last picture is frozen with the seek
    /// gate (so nothing goes black and no old frame is replaced by a new one
    /// before the new stream's first frame), duration and progress go back to
    /// zero, and the decode thread re-arms on the new `bvid`/`cid` (which also
    /// refetches danmaku for the new cid). Window, geometry, toolbar state and
    /// the queue are untouched.
    ///
    /// Returns `false` when the pick is the episode already playing (or the
    /// player is closed), so the caller does not touch its title/comments.
    pub fn switch_to(&self, bvid: String, cid: u64, aid: i64, title: String) -> bool {
        if cid == 0 || self.is_closed() {
            return false;
        }
        let next = PlaybackSource {
            bvid,
            cid,
            aid,
            title,
        };
        let same = self
            .source
            .lock()
            .map(|source| source.cid == next.cid && source.bvid == next.bvid)
            .unwrap_or(false);
        if same {
            return false;
        }
        if let Ok(mut source) = self.source.lock() {
            *source = next.clone();
        }
        if let Ok(mut slot) = self.switch_request.lock() {
            *slot = Some(next);
        }
        self.source_seq.fetch_add(1, Ordering::SeqCst);
        // Duration / readout belong to the video being left: the new playurl
        // publishes its own length when it lands.
        self.api_duration_ms.store(0, Ordering::SeqCst);
        self.decoder_duration_ms.store(0, Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);
        self.presented_ms.store(0, Ordering::SeqCst);
        self.set_auto_paused(false);
        // The readout (and the playhead) start at 0 while the old picture stays.
        self.hold_position(0);
        // Hold the frame on screen until the new stream's first frame, exactly
        // like a release (see `seek_freeze`): the mailbox refuses both the old
        // session's trailing frames and the new session's frames until the
        // decode loop opens the gate.
        if self.presented_frames() > 0 && !self.audio_only() {
            let release_ms = monotonic_ms().max(1);
            self.seek_freeze_at.store(release_ms, Ordering::SeqCst);
        }
        self.switch_at_ms.store(monotonic_ms().max(1), Ordering::SeqCst);
        // Retire the running session (its decoder exits at the top of its next
        // poll and the `run` loop arms the new source).
        self.bump_seek();
        true
    }

    /// One `series switch first_frame_ms=…` receipt per switch: called by the
    /// decode loop when the new source's first frame reaches the UI.
    pub fn note_switch_first_frame(&self) {
        let at = self.switch_at_ms.swap(0, Ordering::SeqCst);
        if at != 0 {
            crate::debug_log(&format!(
                "series switch first_frame_ms={}",
                monotonic_ms().saturating_sub(at)
            ));
        }
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
            self.note_quality_switch(qn);
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
            self.note_quality_switch(qn);
            self.bump_seek();
        }
    }

    /// Arm the one-shot receipt for a 画质 switch: the next session's first
    /// presented frame logs how long the switch took (`session first present`).
    fn note_quality_switch(&self, qn: i64) {
        self.quality_switch_qn.store(qn.max(0) as u64, Ordering::SeqCst);
        self.quality_switch_at_ms
            .store(monotonic_ms().max(1), Ordering::SeqCst);
    }

    /// The switch request waiting for its first frame, as `(qn, elapsed_ms)`.
    /// Cleared by the read, so one switch yields exactly one receipt.
    fn take_quality_switch(&self) -> Option<(i64, u64)> {
        let at = self.quality_switch_at_ms.swap(0, Ordering::SeqCst);
        if at == 0 {
            return None;
        }
        Some((
            self.quality_switch_qn.load(Ordering::SeqCst) as i64,
            monotonic_ms().saturating_sub(at),
        ))
    }

    /// The exact stream selected by the 画质 menu, if any.
    fn exact_quality(&self) -> Option<(i64, i64, u64)> {
        self.quality_exact.lock().ok().and_then(|guard| *guard)
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
        self.danmaku_font
            .store(step.min(2) as u64, Ordering::SeqCst);
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
        self.danmaku_area.store(
            crate::settings::clamp_area(percent) as u64,
            Ordering::SeqCst,
        );
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

    /// Whether this session decodes at the source's own size (原始分辨率).
    pub fn native_frame_size(&self) -> bool {
        self.native_frame_size.load(Ordering::SeqCst)
    }

    /// 设置 → 播放 → 原始分辨率. A change restarts the decoder at the current
    /// position, because the frame size is a property of one decode session.
    pub fn set_native_frame_size(&self, native: bool) {
        if self.native_frame_size.swap(native, Ordering::SeqCst) != native {
            self.bump_seek();
        }
    }

    /// Store the frame-size mode for the very first session without a restart
    /// (playback has not started yet).
    pub fn set_native_frame_size_initial(&self, native: bool) {
        self.native_frame_size.store(native, Ordering::SeqCst);
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
    ///
    /// Covers both the seek/auto-pause overlay (`buffering`) and the startup
    /// stage machine, which is armed before the decoder even spawns so the bar
    /// is up from the first UI tick of a session.
    pub fn buffering(&self) -> bool {
        self.buffering.load(Ordering::Relaxed) || self.startup_active() || self.startup_closing()
    }

    /// Whether a startup stage machine is running for the current session.
    fn startup_active(&self) -> bool {
        self.startup_armed_ms.load(Ordering::Relaxed) != 0
    }

    /// Whether a startup finished so recently that its 100 % must still be on
    /// screen (see `startup_closing_until`).
    fn startup_closing(&self) -> bool {
        let until = self.startup_closing_until.load(Ordering::Relaxed);
        until != 0 && monotonic_ms() < until
    }

    /// Milliseconds since the startup armed; `None` when none is running.
    fn startup_age_ms(&self) -> Option<u64> {
        let armed = self.startup_armed_ms.load(Ordering::Relaxed);
        if armed == 0 {
            None
        } else {
            Some(monotonic_ms().saturating_sub(armed))
        }
    }

    /// Progress of the pre-roll toward its target, `0.0..=1.0`.
    ///
    /// While a startup is running this is the stage machine's monotonic bar
    /// (`startup_milli`). Otherwise: for a ring gate `min(ring_len /
    /// target_frames, 1.0)`; for the cache gate the fraction of
    /// [`Self::buffer_need_bytes`] the proxy already holds (as published by the
    /// pacer, so no lock is taken here). Exactly `1.0` whenever the gate is open
    /// (or was never armed).
    pub fn buffer_progress(&self) -> f32 {
        if self.startup_active() {
            return self.startup_milli.load(Ordering::Relaxed) as f32 / 1000.0;
        }
        if self.startup_closing() {
            return 1.0;
        }
        if !self.buffering() {
            return 1.0;
        }
        if self.buffer_need_bytes.load(Ordering::Relaxed) > 0 {
            return self.buffer_pct.load(Ordering::Relaxed) as f32 / 1000.0;
        }
        let target = self.buffer_target.load(Ordering::Relaxed);
        let ring = self.prof.ring_len.load(Ordering::Relaxed);
        buffer_progress_milli(ring as usize, target as usize) as f32 / 1000.0
    }

    // ---- startup stage machine ------------------------------------------
    //
    // One writer (the pacer) and one reader (the UI tick). Every field is a
    // plain atomic, so publishing a stage costs no lock and the 60 Hz UI read
    // never waits on the decode loop.

    /// Arm the stage machine for a session that is about to spawn its decoder.
    ///
    /// Returns the guard that aborts the machine if the session ends before the
    /// first frame (seek, pause, decoder error): a retired session must not
    /// leave the overlay up.
    fn begin_startup(self: &Arc<Self>, src: &str) -> StartupGuard {
        for slot in &self.startup_subs {
            slot.store(0, Ordering::Relaxed);
        }
        self.startup_milli.store(0, Ordering::Relaxed);
        self.startup_milli_ms.store(0, Ordering::Relaxed);
        self.startup_need_bytes.store(0, Ordering::Relaxed);
        self.startup_target_frames
            .store(PRE_ROLL_START_FRAMES as u64, Ordering::Relaxed);
        self.startup_decoded_base
            .store(self.prof.decoded.load(Ordering::Relaxed), Ordering::Relaxed);
        self.startup_log_ms.store(0, Ordering::Relaxed);
        // `u8::MAX` forces the first `startup stage=…` line at t=0.
        self.startup_log_stage.store(u8::MAX, Ordering::Relaxed);
        self.startup_closing_until.store(0, Ordering::Relaxed);
        self.startup_armed_ms
            .store(monotonic_ms().max(1), Ordering::SeqCst);
        crate::debug_log(&format!("startup begin src={src}"));
        self.publish_startup(StartupStage::Bytes, 0.0);
        StartupGuard {
            control: self.clone(),
            active: true,
        }
    }

    /// Publish the session's ring target for the PreRoll stage (clamped to what
    /// the ring actually holds for this session's frame size).
    fn set_startup_target(&self, frames: usize) {
        self.startup_target_frames
            .store(frames.max(1) as u64, Ordering::Relaxed);
    }

    /// Publish the cache byte target of the Bytes stage (`0` = no proxy).
    fn set_startup_need_bytes(&self, bytes: u64) {
        self.startup_need_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Frames decoded since this startup armed (the Decode stage's sub-signal).
    fn startup_decoded_since(&self) -> u64 {
        self.prof
            .decoded
            .load(Ordering::Relaxed)
            .saturating_sub(self.startup_decoded_base.load(Ordering::Relaxed))
    }

    /// The four stage sub-values, thousandths.
    fn startup_subs(&self) -> [u32; 4] {
        [
            self.startup_subs[0].load(Ordering::Relaxed),
            self.startup_subs[1].load(Ordering::Relaxed),
            self.startup_subs[2].load(Ordering::Relaxed),
            self.startup_subs[3].load(Ordering::Relaxed),
        ]
    }

    /// Publish one stage's sub-progress (`0.0..=1.0`, monotonic per stage) and
    /// the bar value derived from all four.
    ///
    /// Also writes the `startup stage=<name> t=<ms> pct=<…>` trace at least
    /// every [`STARTUP_TRACE_MS`] and on a stage change, so a startup can be
    /// replayed from the log with its timestamps.
    fn publish_startup(&self, stage: StartupStage, sub: f32) {
        let armed = self.startup_armed_ms.load(Ordering::Relaxed);
        if armed == 0 {
            return;
        }
        let milli = (sub.clamp(0.0, 1.0) * 1000.0).round() as u32;
        self.startup_subs[stage.index()].fetch_max(milli, Ordering::Relaxed);
        let subs = self.startup_subs();
        let target = startup_overall_milli(&subs);
        // Slew, don't jump. A stage completion is often instantaneous (the
        // proxy's first chunk crosses the whole byte target between two polls),
        // and the next slice starts 15-40 points higher; moving at
        // `STARTUP_SLEW_PER_MS` turns that into a sweep of a few hundred ms
        // while never running ahead of what was measured.
        let now = monotonic_ms();
        let previous_ms = self.startup_milli_ms.swap(now, Ordering::Relaxed);
        let elapsed = now.saturating_sub(previous_ms).max(1) as f32;
        let step = (elapsed * STARTUP_SLEW_PER_MS).round() as u32;
        let previous = self.startup_milli.load(Ordering::Relaxed);
        let next = target.min(previous.saturating_add(step.max(1)).max(previous));
        self.startup_milli.fetch_max(next, Ordering::Relaxed);
        // The stage the user is actually looking at: the first incomplete one.
        let current = StartupStage::ALL
            .into_iter()
            .find(|candidate| subs[candidate.index()] < 1000)
            .unwrap_or(StartupStage::PreRoll);
        let last_ms = self.startup_log_ms.load(Ordering::Relaxed);
        let last_stage = self.startup_log_stage.load(Ordering::Relaxed);
        if last_stage == current as u8 && now.saturating_sub(last_ms) < STARTUP_TRACE_MS {
            return;
        }
        self.startup_log_ms.store(now, Ordering::Relaxed);
        self.startup_log_stage.store(current as u8, Ordering::Relaxed);
        self.log_startup(current, &subs, armed, now);
    }

    fn log_startup(&self, stage: StartupStage, subs: &[u32; 4], armed: u64, now: u64) {
        let t = now.saturating_sub(armed);
        let overall = self.startup_milli.load(Ordering::Relaxed);
        crate::debug_log(&format!(
            "startup stage={} t={}ms pct={:.1} subs=[{},{},{},{}] ring={} target={} audio_pid={}",
            stage.label(),
            t,
            overall as f32 / 10.0,
            subs[0],
            subs[1],
            subs[2],
            subs[3],
            self.prof.ring_len.load(Ordering::Relaxed),
            self.startup_target_frames.load(Ordering::Relaxed),
            self.audio_pid(),
        ));
    }

    /// The startup is done: the first frame is being handed to the UI. Publishes
    /// the only `100 %` the bar ever shows, then leaves the startup state so the
    /// overlay closes on this frame.
    fn finish_startup(&self, via: &str) {
        let armed = self.startup_armed_ms.swap(0, Ordering::SeqCst);
        if armed == 0 {
            return;
        }
        let subs = self.startup_subs();
        self.startup_milli.store(1000, Ordering::Relaxed);
        self.startup_log_stage.store(u8::MAX, Ordering::Relaxed);
        // Stay at 100 % for a couple of UI ticks: the overlay's own "no picture
        // yet" test (`!has_picture`) must never win the race against the frame
        // that was just handed over and flash the bar back to 0.
        self.startup_closing_until
            .store(monotonic_ms().saturating_add(STARTUP_CLOSING_MS), Ordering::Relaxed);
        crate::debug_log(&format!(
            "startup stage=open t={}ms pct=100 via={via} ring={} subs=[{},{},{},{}]",
            monotonic_ms().saturating_sub(armed),
            self.prof.ring_len.load(Ordering::Relaxed),
            subs[0],
            subs[1],
            subs[2],
            subs[3],
        ));
    }

    /// The session ended before its first frame: drop the stage machine without
    /// ever publishing 100 %, so the next session starts its own startup.
    fn abort_startup(&self, src: &str) {
        let armed = self.startup_armed_ms.swap(0, Ordering::SeqCst);
        if armed == 0 {
            return;
        }
        self.startup_closing_until.store(0, Ordering::Relaxed);
        let subs = self.startup_subs();
        let stage = StartupStage::ALL
            .into_iter()
            .find(|candidate| subs[candidate.index()] < 1000)
            .unwrap_or(StartupStage::PreRoll);
        crate::debug_log(&format!(
            "startup abort src={src} stage={} t={}ms pct={:.1}",
            stage.label(),
            monotonic_ms().saturating_sub(armed),
            self.startup_milli.load(Ordering::Relaxed) as f32 / 10.0,
        ));
    }

    /// Publish the cache gate's progress (thousandths).
    fn set_buffer_pct(&self, milli: u32) {
        self.buffer_pct.store(milli.min(1000), Ordering::Relaxed);
    }

    /// Close the gate: the target has been reached, the timeout fired, or the
    /// stream ended. Publishes `1.0` implicitly (see [`Self::buffer_progress`]).
    ///
    /// Every close logs `gate open src=…`, and every arm logs `gate arm src=…`,
    /// so a gate that is armed and never released is visible in one log.
    fn end_buffering(&self, src: &str) {
        let was = self.buffering.swap(false, Ordering::Relaxed);
        let target = self.buffer_target.load(Ordering::Relaxed);
        let need = self.buffer_need_bytes.load(Ordering::Relaxed);
        self.buffer_need_bytes.store(0, Ordering::Relaxed);
        if was {
            crate::debug_log(&format!(
                "gate open src={src} ring={} target={target} need={need}",
                self.prof.ring_len.load(Ordering::Relaxed)
            ));
        }
    }

    /// Arm the gate for `target_frames` of ring occupancy.
    fn begin_buffering(&self, src: &str, target_frames: usize) {
        self.startup_closing_until.store(0, Ordering::Relaxed);
        self.buffer_need_bytes.store(0, Ordering::Relaxed);
        self.buffer_target
            .store(target_frames.max(1) as u64, Ordering::Relaxed);
        self.buffering.store(true, Ordering::Relaxed);
        crate::debug_log(&format!(
            "gate arm src={src} target={} ring={} kind=ring",
            target_frames.max(1),
            self.prof.ring_len.load(Ordering::Relaxed)
        ));
    }

    /// Arm the cache gate for `need_bytes` of proxy cache. The pacer fills in
    /// the progress with [`Self::set_buffer_pct`] on every poll.
    fn begin_cache_buffering(&self, src: &str, need_bytes: u64) {
        self.startup_closing_until.store(0, Ordering::Relaxed);
        self.buffer_target.store(1, Ordering::Relaxed);
        self.set_buffer_pct(0);
        self.buffer_need_bytes
            .store(need_bytes.max(1), Ordering::Relaxed);
        self.buffering.store(true, Ordering::Relaxed);
        crate::debug_log(&format!(
            "gate arm src={src} target=1 need={} ring={} kind=cache",
            need_bytes.max(1),
            self.prof.ring_len.load(Ordering::Relaxed)
        ));
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

    /// Put a target the pacer took for a shadow back, so the hard-restart path
    /// still starts at the position the user asked for. A target the UI queued
    /// since is newer and wins.
    fn restore_pending_seek(&self, target_ms: u64) {
        let _ = self.pending_seek_ms.compare_exchange(
            u64::MAX,
            target_ms,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Whether a seek target is waiting for the next decoder session. A plain
    /// atomic load; used by the end-of-track hold to notice the seek that asks
    /// for playback again.
    fn has_pending_seek(&self) -> bool {
        self.pending_seek_ms.load(Ordering::SeqCst) != u64::MAX
    }

    /// Whether a seek's target still has to come from the network.
    ///
    /// Three answers, and only one of them is a measurement:
    ///
    /// * `Some(false)` — the **disk map** holds the region the decoder needs, so
    ///   the seek opens the cache file and costs zero upstream. The overlay is
    ///   never published (the picture the user picked is already on disk).
    /// * `None` — unknown: the map does not hold it (a hole, a partial file, no
    ///   session at all). The 450 ms grace decides whether the overlay appears,
    ///   and the pacer logs `cached=false src=net`.
    /// * `Some(true)` — used only to *arm* the overlay immediately when the
    ///   target is clearly beyond everything the proxy has fetched. It is
    ///   advisory: it can never claim "cached".
    ///
    /// The old rule answered "buffered" from size estimates — a warm point within
    /// half a chunk, or a target inside the read-ahead window — which claimed
    /// cached for regions the map did not hold: the seek then fetched ~8 MB
    /// upstream and took 1.7–3.1 s while the overlay stayed hidden.
    fn seek_beyond_prefetch(&self, target_ms: u64) -> Option<bool> {
        let duration = self.duration_ms();
        if duration == 0 || target_ms >= duration {
            return None;
        }
        if self.seek_is_cached(target_ms) {
            return Some(false);
        }
        // Not on disk: unknown, unless the target is past everything fetched so
        // far (then the wait is certain enough to show the overlay at once).
        let stats = self.prefetch_raw_stats()?;
        let total = stats.bytes_total?;
        let fetched = stats.bytes_fetched.max(stats.cache_bytes);
        if fetched == 0 {
            return None;
        }
        let target_bytes = (total as u128)
            .saturating_mul(target_ms as u128)
            .saturating_div(duration as u128) as u64;
        let from_ms = self.pending_seek_from_ms.load(Ordering::SeqCst);
        let cursor_bytes = (total as u128)
            .saturating_mul(from_ms as u128)
            .saturating_div(duration as u128) as u64;
        let reach = cursor_bytes.saturating_add(stats.read_ahead_bytes);
        if target_bytes > reach && target_bytes > fetched {
            return Some(true);
        }
        None
    }

    /// Publish the buffering overlay for a seek and start its clock. `target`
    /// is the position the user picked, used only for the log line.
    fn begin_seek_buffering(&self, target_ms: u64) {
        self.seek_buffering_at
            .store(monotonic_ms(), Ordering::Relaxed);
        self.seek_buffering.store(true, Ordering::Relaxed);
        // One frame is all the pacer needs to consider the seek served; the
        // overlay is closed by the presented frame, not by ring occupancy.
        self.begin_buffering("seek", 1);
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
        self.end_buffering("seek-served");
        let started = self.seek_buffering_at.load(Ordering::Relaxed);
        Some(monotonic_ms().saturating_sub(started))
    }

    /// Monotonic ms at which the newest seek was requested (`0` before the
    /// first one). The first presented frame of that seek measures its own
    /// latency against this.
    fn seek_requested_at_ms(&self) -> u64 {
        self.last_seek_at.load(Ordering::SeqCst)
    }

    /// Compact, comparable view of the download map the 下载分布 panel draws:
    /// `(digest, ranges, bytes)`. Two dumps with the same digest describe the
    /// same byte ranges, which is how the seek test proves the map does not
    /// change when the decoder restarts. It digests the **disk** map: the memory
    /// window is a hot cache in front of it and legitimately moves with the
    /// cursor.
    fn prefetch_map_dump(&self) -> Option<(String, u64, u64)> {
        if !self.prefetch_in_use.load(Ordering::SeqCst) {
            return None;
        }
        let guard = match self.prefetch.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let session = guard.as_ref()?;
        let ranges = session.handle.disk_ranges();
        let bytes: u64 = ranges
            .iter()
            .map(|(start, end)| end.saturating_sub(*start))
            .sum();
        // FNV-1a over the range list: cheap, and any changed byte shows up.
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for (start, end) in &ranges {
            for word in [*start, *end] {
                for byte in word.to_le_bytes() {
                    hash ^= byte as u64;
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        Some((format!("{hash:016x}"), ranges.len() as u64, bytes))
    }

    /// Estimated byte offset of `ms` in the media, from the proxy's measured
    /// length and the reported duration. `None` when either is unknown (the
    /// cache gate then measures from byte 0, which is where a fresh player
    /// starts anyway).
    fn byte_offset_for_ms(&self, ms: u64) -> Option<u64> {
        let duration = self.duration_ms();
        if duration == 0 {
            return None;
        }
        let total = self.prefetch_raw_stats()?.bytes_total?;
        Some(
            (total as u128)
                .saturating_mul(ms as u128)
                .saturating_div(duration as u128) as u64,
        )
    }

    /// The quantified line the UI shows under the loading animation while
    /// `buffering` is up. Empty once nothing is loading.
    ///
    /// Every number is measured (proxy throughput and read-ahead, declared
    /// stream bitrate); nothing is extrapolated into a fake ETA.
    pub fn buffer_detail(&self) -> String {
        if self.startup_closing() {
            return String::new();
        }
        if self.startup_active() {
            // The stage line: `取流 42%`, `解码中`, `准备音频`, `缓冲 0.8s`.
            // The buffered duration is the ring's own occupancy at this
            // session's frame period, so the number is measured, not guessed.
            let frame_ms = self.prof.frame_us.load(Ordering::Relaxed) as f32 / 1000.0;
            let ring_s = self.prof.ring_len.load(Ordering::Relaxed) as f32 * frame_ms / 1000.0;
            return startup_detail(&self.startup_subs(), ring_s);
        }
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
        // A cache gate publishes the exact byte target it is waiting for, so the
        // percentage here is the same one that opens the gate (and the overlay
        // disappears at 100%). A ring/seek overlay keeps the 2 s yardstick.
        if self.buffer_need_bytes.load(Ordering::Relaxed) > 0 {
            let pct = (self.buffer_pct.load(Ordering::Relaxed) / 10).min(100);
            if rate > 0 {
                return format!("加载中 {pct}% · {mbps:.1} MB/s");
            }
            return format!("加载中 {pct}%");
        }
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
                return format!("{mbps:.1} MB/s · 已缓存 {secs:.1}s");
            }
        }
        if rate > 0 {
            let secs = ahead as f64 / rate as f64;
            return format!("{mbps:.1} MB/s · 已缓存 {secs:.1}s");
        }
        // No measured throughput yet: say so instead of inventing numbers.
        "加载中…".to_string()
    }

    /// Ring occupancy in frames, published by the reader after every push and
    /// by the pacer after every pop.
    fn ring_len(&self) -> usize {
        self.prof.ring_len.load(Ordering::Relaxed) as usize
    }

    /// Ring occupancy in frames, for the stall trace (`ring=`).
    pub fn ring_occupancy(&self) -> u64 {
        self.prof.ring_len.load(Ordering::Relaxed)
    }

    /// Frames handed to the UI so far. The stall trace divides the delta of two
    /// samples by their wall time to report `fps=`.
    pub fn presented_frames(&self) -> u64 {
        self.prof.presented.load(Ordering::Relaxed)
    }

    // ---- 网络分析 (profiler) counters -------------------------------------
    //
    // The four `note_*` recorders below are the only profiler calls on a hot
    // path: each is one relaxed atomic operation, no lock and no allocation.

    /// Count one frame handed to the UI (pacer, once per pushed frame).
    ///
    /// The first push is also the honest end of "open → picture": it is logged
    /// once per player, from the pacer, so a startup measurement needs no UI
    /// timer. Everything below is one relaxed add plus an integer compare.
    fn note_frame_presented(&self) {
        let previous = self.prof.presented.fetch_add(1, Ordering::Relaxed);
        // The end rule needs to know whether the picture is still advancing: a
        // frame handed to the UI now is the proof that this track is not over.
        self.presented_at_ms.store(monotonic_ms(), Ordering::SeqCst);
        if previous == 0 {
            crate::debug_log(&format!(
                "first present ms={}",
                monotonic_ms().saturating_sub(self.opened_at_ms)
            ));
        }
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
            self.prof
                .first_frame_ms
                .store(latency_ms, Ordering::Relaxed);
            // Monotonic stamp for the startup gate: "playback started here",
            // persisted per player so a seek respawn does not re-arm the gate.
            self.first_frame_at_ms.store(monotonic_ms(), Ordering::Relaxed);
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

    /// The stream the decoder is actually running right now: `(qn, codecid,
    /// bandwidth)` as recorded by the last session's `note_stream`.
    ///
    /// This is the readback the 画质 menu is marked from (`stream qn=` on the
    /// log), not the value a pick stored: if a pick's exact stream vanished from
    /// the playurl and the decoder fell back to the automatic rule, the menu
    /// must follow the decoder, never the click.
    pub fn current_stream(&self) -> Option<(i64, i64, u64)> {
        let media = self.prof.media.lock().ok()?;
        if media.qn <= 0 || !self.prof.active.load(Ordering::Relaxed) {
            return None;
        }
        Some((media.qn, i64::from(media.codecid), media.bps))
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
        self.prof.buffer_delta_us_per_s.store(0, Ordering::Relaxed);
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

    /// Record the A/V relation the guard measured on the frame it just handled
    /// (sound minus picture, ms) and the guard's state. Two relaxed stores on
    /// the frame path; read once a second by the profiler line, which is what
    /// makes the lead distribution measurable.
    fn note_av_state(&self, lead_ms: i64, state: u64) {
        self.prof.av_lead_ms.store(lead_ms, Ordering::Relaxed);
        self.prof.av_state.store(state, Ordering::Relaxed);
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
        let estimated_bps =
            ((media.bps as f64) * (1.0 + buffer_delta_per_s as f64)).max(0.0) as u64;
        let first_frame_ms = if self.prof.first_seen.load(Ordering::Relaxed) {
            Some(
                self.prof
                    .first_frame_ms
                    .load(Ordering::Relaxed)
                    .min(u32::MAX as u64) as u32,
            )
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
            stalls: self
                .prof
                .stalls
                .load(Ordering::Relaxed)
                .min(u32::MAX as u64) as u32,
            longest_stall_ms: self
                .prof
                .longest_stall_ms
                .load(Ordering::Relaxed)
                .min(u32::MAX as u64) as u32,
            codecid: media.codecid,
            width: media.width,
            height: media.height,
            frame_w: self.frame_width(),
            frame_h: self.frame_height(),
            declared_fps: media.fps,
            qn: media.qn,
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
        let buffer = guard.buffer.as_ref()?;
        if guard.w == 0 || guard.h == 0 || buffer.as_bytes().is_empty() {
            return None;
        }
        Some((buffer.as_bytes().to_vec(), guard.w, guard.h))
    }

    /// Adopt a frame for 截图. The buffer is reference counted, so this keeps the
    /// newest picture without a per-frame copy; `snapshot_frame` is the only
    /// place that copies, and only when a screenshot is actually taken.
    fn store_last_frame(&self, buffer: &FrameBuf, size: FrameSize) {
        if let Ok(mut guard) = self.last_frame.lock() {
            guard.buffer = Some(buffer.clone());
            guard.w = size.w;
            guard.h = size.h;
        }
    }

    /// Volume in `0..=100` (the value before mute is applied).
    pub fn volume(&self) -> u64 {
        self.volume.load(Ordering::SeqCst)
    }

    /// PID of the audio `ffplay` this player owns (`0` when none is running).
    pub fn audio_pid(&self) -> u32 {
        self.audio_pid
            .load(Ordering::SeqCst)
            .min(u64::from(u32::MAX)) as u32
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
        // Silent until this session's first frame is on screen, then the
        // user's own mute state.
        let mute = self.muted.load(Ordering::SeqCst)
            || self.audio_hold_silent.load(Ordering::SeqCst);
        crate::win::audio_session_mute(pid, mute)
    }

    /// Release the "no sound before a picture" hold: the first frame of this
    /// session is on screen, so the stored gain/mute is owed to the process.
    fn release_audio_hold(&self) {
        if self.audio_hold_silent.swap(false, Ordering::SeqCst) {
            self.audio_session_pending.store(true, Ordering::SeqCst);
        }
    }

    /// Release the hold **now** and report whether the audio is really audible.
    ///
    /// The startup handover cannot wait for the pacer's 100 ms retry: the first
    /// frame and the first audible sample have to happen in the same instant.
    /// Returns `true` when a running session accepted the gain, i.e. its mute is
    /// off and its level is above zero (`None` = no audio process at all).
    fn release_audio_hold_now(&self) -> Option<bool> {
        self.release_audio_hold();
        let pid = self.audio_pid();
        if pid == 0 {
            return None;
        }
        self.audio_session_try_ms
            .store(monotonic_ms(), Ordering::SeqCst);
        if self.apply_audio_session() {
            self.audio_session_pending.store(false, Ordering::SeqCst);
        }
        crate::win::audio_session_get(pid).map(|(level, mute)| !mute && level > 0.001)
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

    /// The requested DASH audio id (`0` = 自动（最高）).
    pub fn audio_quality(&self) -> i64 {
        self.audio_quality_id.load(Ordering::SeqCst) as i64
    }

    /// Pick an audio tier by its DASH id. Applies LIVE: the picture, the pacer
    /// and the decoder are untouched, only `ffplay` is restarted at the current
    /// position (see [`Self::set_audio_index`]). Returns the index it resolved
    /// to, or `None` when this video does not carry that id.
    pub fn set_audio_quality(&self, id: i64) -> Option<usize> {
        self.audio_quality_id.store(id.max(0) as u64, Ordering::SeqCst);
        let meta = self.audio_meta.lock().ok()?;
        let index = if id <= 0 {
            0
        } else {
            meta.iter().position(|entry| entry.0 == id)?
        };
        drop(meta);
        self.set_audio_index(index);
        Some(index)
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
            if self
                .keywords
                .iter()
                .any(|word| lowered.contains(word.as_str()))
            {
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
            inner.placed = place_items(&inner.raw, scroll_ms, lanes_for_area(filter.area_percent));
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
pub(crate) fn compute_density(items: &[Danmaku], duration_ms: u64) -> (Vec<f32>, u32, usize, u64) {
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

/// Per-thread "what am I doing now" tag, read by the deadlock trace and by the
/// stall trace.
///
/// Off by default: `tag_here` is two relaxed loads, and a mutex store only when
/// `BILI_DEADLOCK_TRACE=1` or `BILI_STALL_TRACE=1`. With either on, a stalled
/// pipeline names the exact step each worker is sitting in (the measured 9 s of
/// silence after a resync could be a join on the reader, a blocking pipe read,
/// or a driver call).
static THREAD_TAGS: Mutex<Vec<(u32, &'static str)>> = Mutex::new(Vec::new());
static TAG_TRACE: OnceLock<bool> = OnceLock::new();

fn tag_trace_enabled() -> bool {
    *TAG_TRACE.get_or_init(|| {
        std::env::var("BILI_DEADLOCK_TRACE")
            .map(|value| value == "1")
            .unwrap_or(false)
    })
}

/// Record the step the calling thread is in, and the newest step in the process
/// (the stall line's `last=`).
pub(crate) fn tag_here(tag: &'static str) {
    let deadlock = tag_trace_enabled();
    let stall = stall_trace_enabled();
    if !deadlock && !stall {
        return;
    }
    // SAFETY: GetCurrentThreadId is always valid.
    let tid = unsafe { GetCurrentThreadId() };
    if let Ok(mut guard) = THREAD_TAGS.lock() {
        match guard.iter_mut().find(|(id, _)| *id == tid) {
            Some(entry) => entry.1 = tag,
            None => guard.push((tid, tag)),
        }
    }
    LAST_STAGE.store(stage_index(tag), Ordering::Relaxed);
}

/// Dump every worker's last tag when the picture stops advancing.
///
/// `BILI_DEADLOCK_TRACE=1` only. Samples the presented counter every 2 s; a
/// second consecutive sample without progress (and no pause/close) prints one
/// line, so a hang names the step instead of just going quiet.
fn spawn_stall_trace(control: Arc<PlaybackControl>) {
    if !tag_trace_enabled() {
        return;
    }
    std::thread::spawn(move || {
        note_thread_role("stall-trace");
        let mut last = u64::MAX;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if control.is_closed() {
                return;
            }
            let presented = control.prof.presented.load(Ordering::Relaxed);
            if presented == last && !control.is_paused() {
                let tags: Vec<String> = THREAD_TAGS
                    .lock()
                    .map(|guard| {
                        guard
                            .iter()
                            .map(|(tid, tag)| format!("{tid}:{tag}"))
                            .collect()
                    })
                    .unwrap_or_default();
                crate::debug_log(&format!(
                    "stall trace presented={presented} ring={} buffering={} auto={} audio_pid={} decoder_live={} tags=[{}]",
                    control.ring_len(),
                    u8::from(control.buffering()),
                    u8::from(control.auto_paused()),
                    control.audio_pid(),
                    u8::from(control.decoder_live.load(Ordering::SeqCst)),
                    tags.join(" ")
                ));
            }
            last = presented;
        }
    });
}

/// The gate watchdog's condition: is the armed gate's own target met?
///
/// `None` when no gate is armed. A ring gate is met when the ring holds its
/// target frames; a cache gate when the pacer published 100 % of the bytes it
/// waits for. Both watchdogs (pacer and the standalone thread) use exactly this
/// test, so "forced open" can only fire when the overlay has nothing left to
/// wait for.
fn gate_condition_met(control: &PlaybackControl) -> Option<bool> {
    // A running startup owns the overlay: its own stage machine decides when the
    // picture may start, and its stage targets are not a byte/ring gate the
    // watchdog could "force open" without breaking the A/V handover.
    if control.startup_active() {
        return None;
    }
    if !control.buffering() {
        return None;
    }
    let target = control.buffer_target.load(Ordering::Relaxed) as usize;
    let need = control.buffer_need_bytes.load(Ordering::Relaxed);
    Some(if need > 0 {
        control.buffer_pct.load(Ordering::Relaxed) >= 1000
    } else {
        target > 0 && control.ring_len() >= target
    })
}

/// Last-resort gate watchdog, on its own thread for the player's whole life.
///
/// The pacer's own watchdog covers a busy pacer; this one covers a pacer that is
/// blocked or **dead** (a panic, a stuck driver call). The overlay is a promise
/// about the buffer, never a trap: if the gate's own condition has held for
/// [`GATE_FORCE_OPEN_MS`] and the pacer has not closed it, it is closed here.
fn spawn_gate_watchdog(control: Arc<PlaybackControl>) {
    std::thread::spawn(move || {
        note_thread_role("gate-watchdog");
        let mut healthy_since: Option<Instant> = None;
        loop {
            if control.is_closed() {
                return;
            }
            if let Some(satisfied) = gate_condition_met(&control) {
                let target = control.buffer_target.load(Ordering::Relaxed) as usize;
                let need = control.buffer_need_bytes.load(Ordering::Relaxed);
                if satisfied {
                    let since = *healthy_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(GATE_FORCE_OPEN_MS) {
                        crate::debug_log(&format!(
                            "gate forced open src=watchdog-thread ring={} target={target} need={need} held_ms={} auto={}",
                            control.ring_len(),
                            since.elapsed().as_millis(),
                            u8::from(control.auto_paused())
                        ));
                        control.end_buffering("watchdog-thread");
                        if control.auto_paused() {
                            control.set_auto_paused(false);
                        }
                        healthy_since = None;
                    }
                } else {
                    healthy_since = None;
                }
            } else {
                healthy_since = None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
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
    install_panic_log();
    start_core_trace();
    spawn_gate_watchdog(control.clone());
    spawn_stall_trace(control.clone());
    std::thread::spawn(move || {
        note_thread_role("pacer");
        // A panic in the decode path must not leave a dead player: log it (see
        // `install_panic_log`) and start the decode loop again from the current
        // position, bounded so a deterministic panic cannot spin.
        let mut attempts = 0u32;
        loop {
            let session = session.clone();
            let video = video.clone();
            let window = window.clone();
            let mini = mini.clone();
            let control = control.clone();
            let track = track.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
                let control = control.clone();
                move || run(session, video, window, mini, control, track)
            }));
            match outcome {
                Ok(()) => return,
                Err(_) => {
                    attempts = attempts.saturating_add(1);
                    crate::debug_log(&format!(
                        "pacer restarted after panic attempt={attempts} pos_ms={}",
                        control.playback_position_ms()
                    ));
                    control.end_buffering("pacer-restart");
                    control.set_auto_paused(false);
                    control.decoder_live.store(false, Ordering::SeqCst);
                    if attempts >= 3 || control.is_closed() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
        }
    });
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
fn best_unmeasured(primary: &str, backups: &[String]) -> (String, String, bilibili::CdnKind) {
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
    connections: usize,
    cache_key: String,
    audio_cache_key: String,
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
        // 设置 → 预取 → 跳转位点: read from the store at open time, never from a
        // cached copy, so changing it applies to the next player.
        warm_points,
        // 设置 → 预取 → 并发连接: same rule. The startup gate needs the cache to
        // fill in a couple of seconds, which is what these connections are for.
        connections,
        // `%LOCALAPPDATA%\o11a1\cache\<bvid>-<cid>-<quality>.part`: the session
        // cache file is what "已下载" means and what a cached seek decodes.
        cache_key: Some(cache_key),
        // The API already knows the duration: with the length a response states,
        // the proxy derives the exact bitrate. That sets the read-ahead horizon
        // (~90 s of media) and the speculative whole-file rate cap (~2× the
        // bitrate), which is what keeps playback prefetch off the cores.
        media_duration_ms: Some(info.duration.saturating_mul(1000)).filter(|ms| *ms > 0),
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
                "prefetch active url={host} local={local} hosts={count} read_ahead={read_ahead_mib} connections={} warm_points={}",
                config.connections,
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
    // The sound gets its own proxy, for the same reason the picture has one:
    // `ffplay` is handed a loopback URL instead of the signed CDN one, so its
    // `-ss` warm-up on a seek reads bytes this machine already has. A failure
    // here is only a slower sound — the direct URL is used unchanged.
    if !force_fail {
        if let Some(audio) = start_audio_prefetch(info, control.audio_index(), audio_cache_key) {
            control.install_audio_prefetch(audio);
        }
    }
}

/// `BILI_PREFETCH_SWITCH=0`: the pre-fix behaviour on a 画质 switch — the new
/// tier's session is not given an engine of its own, so it reads the CDN URL
/// while the previous tier's proxy keeps downloading its own file. Kept as an
/// A/B knob for the switch QA run (`BILI_QUALITY_SWITCH_TEST`).
fn prefetch_switch_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !std::env::var("BILI_PREFETCH_SWITCH")
            .map(|value| value.trim() == "0")
            .unwrap_or(false)
    })
}

/// The session cache file name of one raw stream: `<bvid>-<cid>-<qn>-<codecid>`.
///
/// The name carries the tier **and** the codec because the bytes do: 1080P AVC
/// and 1080P AV1 are different bitstreams at different offsets, so a file named
/// after the video alone would serve one stream's bytes to the other. The
/// playlist preload (`PrefetchHandle::preload_keys`) writes exactly this name,
/// which is what lets a preloaded item open from disk.
fn stream_cache_key(bvid: &str, cid: u64, qn: i64, codecid: i64) -> String {
    format!("{bvid}-{cid}-{qn}-{codecid}")
}

/// Rebuild this player's picture proxy for `track` — the 画质 switch path.
///
/// A switch opens a **different bitstream**, so it needs a different backend:
/// its own cache file (`.part` + `.map`), its own loopback port, its own server
/// task and its own disk writer. Reusing the previous tier's session is what
/// makes a switched player show "loading" forever: the old engine is bound to
/// the old URL and keeps prefetching the old file (the UI shows that as
/// download speed), while the new stream — if it reads through a proxy at all —
/// would be served bytes of another bitstream at the old offsets.
///
/// The engine is installed **before** the session spawns, so the first byte of
/// the new tier is requested before the first frame is due
/// (`prefetch switch … cache=…` then `prefetch fetch … bytes>0`). Speculative
/// seek-point warming is off for the new engine (`warm_points: 0`): the first
/// frame needs the read-ahead, not ten other regions of the file.
///
/// A failure is logged and the session reads the CDN URL directly; the stream is
/// remembered so one broken mirror cannot cost a spawn attempt per seek.
fn switch_prefetch(
    control: &Arc<PlaybackControl>,
    track: &bilibili::VideoTrack,
    connections: usize,
) -> Option<String> {
    if track.url.is_empty() {
        crate::debug_log(&format!(
            "prefetch switch disabled reason=empty-url qn={} codecid={}",
            track.id, track.codecid
        ));
        return None;
    }
    let failed = control
        .prefetch_failed_stream
        .lock()
        .ok()
        .and_then(|guard| *guard);
    if failed == Some((track.id, track.codecid)) {
        return None;
    }
    let host = bilibili::url_host(&track.url).unwrap_or_default();
    let hosts = if host.is_empty() {
        Vec::new()
    } else {
        vec![host.clone()]
    };
    let source = control.source();
    let cache_key = stream_cache_key(&source.bvid, source.cid, track.id, track.codecid);
    let config = PrefetchConfig {
        hosts,
        warm_points: 0,
        connections,
        cache_key: Some(cache_key.clone()),
        media_duration_ms: Some(control.duration_ms()).filter(|ms| *ms > 0),
        ..Default::default()
    };
    match prefetch::spawn(&track.url, config) {
        Ok(handle) => {
            let local = handle.local_url.clone();
            let cache = handle
                .cache_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_string());
            crate::debug_log(&format!(
                "prefetch switch qn={} codecid={} host={host} local={local} cache={cache} connections={connections}",
                track.id, track.codecid
            ));
            // Replaces and (on its own thread) shuts down the previous tier's
            // engine: port, tasks, writer and `.part` all belong to the stream
            // that was left.
            control.install_prefetch(handle, track.url.clone(), host);
            warmup_watchdog(control.clone());
            Some(local)
        }
        Err(err) => {
            crate::debug_log(&format!(
                "prefetch switch disabled reason={err} qn={} codecid={} host={host}",
                track.id, track.codecid
            ));
            if let Ok(mut guard) = control.prefetch_failed_stream.lock() {
                *guard = Some((track.id, track.codecid));
            }
            None
        }
    }
}

/// Put a small prefetch proxy in front of the DASH audio track `ffplay` will
/// read, and return it for installation on the player.
///
/// Same decision as [`start_prefetch`] for the picture, with the audio's own
/// numbers: the stream is 24–48 KB/s, so 8 MiB of read-ahead is minutes of
/// sound, one 1 MiB range is ~20 s of it, and two connections saturate what a
/// seek needs before the video pre-roll is over. No seek points: the audio
/// cache is a scratch buffer for the sound playing right now, not a map of the
/// file to warm.
///
/// The cache file is `<bvid>-<cid>-audio-<id>.part` (the caller builds the key),
/// and it is deleted by the same `PrefetchHandle::shutdown` that deletes the
/// video cache — one `cache removed` line per file.
fn start_audio_prefetch(
    info: &bilibili::PlayInfo,
    audio_index: usize,
    cache_key: String,
) -> Option<AudioPrefetchSession> {
    let track = info
        .audio_tracks
        .get(audio_index)
        .or_else(|| info.audio_tracks.first())?;
    let primary = track
        .urls
        .first()
        .cloned()
        .unwrap_or_else(|| track.url.clone());
    if primary.is_empty() {
        crate::debug_log("audio prefetch disabled reason=empty-media-url");
        return None;
    }
    let backups: Vec<String> = track.urls.iter().skip(1).cloned().collect();
    let host = bilibili::url_host(&primary).unwrap_or_default();
    // An akamaized target carries its own `hdnts` token bound to that host, so
    // the mirrors of the *video* stream must not be rotated through it (they
    // would answer 403). The audio track's own mirrors are the ones that are
    // signed for it.
    let hosts = if host.ends_with("akamaized.net") {
        vec![host.clone()]
    } else {
        proxy_hosts(&primary, &backups)
    };
    let config = PrefetchConfig {
        read_ahead_bytes: AUDIO_READ_AHEAD_BYTES,
        chunk_bytes: AUDIO_CHUNK_BYTES,
        connections: AUDIO_CONNECTIONS,
        // A seek's own shadow proxy primes its target (see
        // `PrefetchHandle::prime_at`); scheduled seek points would download the
        // file for seeks nobody makes.
        warm_points: 0,
        hosts,
        cache_key: Some(cache_key),
        media_duration_ms: Some(info.duration.saturating_mul(1000)).filter(|ms| *ms > 0),
        ..Default::default()
    };
    match prefetch::spawn(&primary, config) {
        Ok(handle) => {
            let local = handle.local_url.clone();
            let cache = handle
                .cache_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_string());
            crate::debug_log(&format!(
                "audio prefetch active url={host} local={local} read_ahead={} chunk={} connections={AUDIO_CONNECTIONS} cache={cache}",
                AUDIO_READ_AHEAD_BYTES >> 20,
                AUDIO_CHUNK_BYTES >> 20
            ));
            Some(AudioPrefetchSession {
                handle,
                source_url: primary,
            })
        }
        Err(err) => {
            crate::debug_log(&format!(
                "audio prefetch disabled reason={err} url={host} local=-"
            ));
            None
        }
    }
}

/// Close an audio proxy off the calling thread.
///
/// `PrefetchHandle::shutdown` waits (bounded, 3 s) for its server task and then
/// deletes the session cache file. Two callers must never pay that wait: the
/// decode thread at a seek swap (the target's first frame is due) and the
/// window's close path (the UI thread).
fn retire_audio_prefetch(handle: PrefetchHandle) {
    std::thread::spawn(move || drop(handle));
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

// -- Watch history (历史记录) ------------------------------------------------
//
// The official web player keeps the account's 历史记录 alive with a heartbeat
// every few seconds; without it a video watched here never shows up there. The
// reporter below is one thread per playback session: it watches this player's
// [`PlaybackControl`] and posts the same `x/click-interface/web/heartbeat`
// bodies the web player posts (see `bilibili::Session::report_heartbeat`).
//
// Two rules matter more than the cadence:
//
// * **Nothing is reported before 5 s of position.** An accidental open (or a
//   video that never decodes) must leave no entry, so even the `play_type=1`
//   start beat waits for the gate.
// * **A slow or rejected request never touches playback.** Every call is made
//   on this thread's own `block_on` over the shared network runtime; its code
//   (including a risk-control rejection) is only logged.

/// Distance between two `play_type=0` beats while the picture advances.
const HISTORY_PLAY_INTERVAL: Duration = Duration::from_secs(15);
/// Floor between any two requests, so a pause/resume storm cannot spam the
/// endpoint. The final `play_type=4` is exempt (it is the one that stores the
/// final position).
const HISTORY_MIN_GAP: Duration = Duration::from_secs(10);
/// A session that never reached this position was an accidental open.
const HISTORY_MIN_PLAYED_S: u64 = 5;
/// How often the reporter samples the control. Cheap (atomics only).
const HISTORY_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Unix seconds, for the heartbeat's `start_ts`.
fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// One playback session's history reporter. Owns its own request pacing, so a
/// blocked request can never delay the UI, the pacer or the decoder.
struct HistoryReporter {
    session: Arc<bilibili::Session>,
    aid: u64,
    bvid: String,
    cid: u64,
    /// Declared duration from the playurl result, seconds.
    duration_s: u64,
    /// Unix second this session began (see [`run`]).
    start_ts: u64,
    /// When the newest request was sent (`None` = nothing sent yet).
    last_sent: Option<Instant>,
    /// Position of that request in ms, so a `play_type=0` beat only goes out
    /// while the playhead really advanced.
    last_sent_pos_ms: u64,
    /// `PlaybackControl::source_seq()` this record belongs to. A 选集 switch
    /// bumps it: this reporter thread stays (one per player) but closes the
    /// old record with `play_type=4` and starts a new one for the new video.
    source_seq: u64,
    /// Position of the previous sample. A switch zeroes the readout before this
    /// thread sees it, and the closing beat must report where the OLD video
    /// actually stopped, not the new video's 0.
    previous_pos_ms: u64,
}

impl HistoryReporter {
    fn new(
        session: Arc<bilibili::Session>,
        video: &bilibili::FeedVideo,
        duration_s: u64,
        start_ts: u64,
        source_seq: u64,
    ) -> Self {
        Self {
            session,
            aid: video.aid,
            bvid: video.bvid.clone(),
            cid: video.cid,
            duration_s,
            start_ts,
            last_sent: None,
            last_sent_pos_ms: 0,
            source_seq,
            previous_pos_ms: 0,
        }
    }

    /// Whether a request may be sent now under the 10 s floor.
    fn allowed(&self) -> bool {
        self.last_sent
            .map(|at| at.elapsed() >= HISTORY_MIN_GAP)
            .unwrap_or(true)
    }

    /// Post one heartbeat and log the API code. A failure (transport error or a
    /// non-zero code returned by the API) is only a log line: playback must
    /// never be paused or stopped by history reporting.
    fn report(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        play_type: u64,
        played_s: u64,
        watched_s: u64,
    ) {
        self.last_sent = Some(Instant::now());
        self.last_sent_pos_ms = played_s.saturating_mul(1000);
        let result = runtime.block_on(self.session.report_heartbeat(
            self.aid,
            &self.bvid,
            self.cid,
            played_s,
            watched_s,
            self.duration_s,
            self.start_ts,
            play_type,
        ));
        match result {
            Ok(code) => crate::debug_log(&format!(
                "history report code={code} played={played_s} type={play_type} watched={watched_s}"
            )),
            Err(err) => crate::debug_log(&format!(
                "history report code=err played={played_s} type={play_type} watched={watched_s} err={err}"
            )),
        }
    }

    /// Sample the control until the session ends or is closed, then send the
    /// final `play_type=4` (if the session ever became reportable) and return.
    fn run(mut self, runtime: &tokio::runtime::Runtime, control: &PlaybackControl) {
        let mut frames_base = control.presented_frames();
        let mut started = false;
        let mut user_paused = control.user_paused();
        let mut watched_ms = 0u64;
        let mut last_tick = Instant::now();
        // Armed by the start beat; a beat is due this interval later.
        let mut next_play_at = Instant::now();
        loop {
            std::thread::sleep(HISTORY_POLL_INTERVAL);
            let now = Instant::now();
            let delta_ms = now.saturating_duration_since(last_tick).as_millis() as u64;
            last_tick = now;

            let closed = control.is_closed();
            let ended = !closed && control.track_ended();
            let pos_ms = control.playback_position_ms();
            let paused = control.user_paused();
            let buffering = control.buffering();
            let previous_pos_ms = self.previous_pos_ms;
            self.previous_pos_ms = pos_ms;

            // 选集: this player now plays another video. Close the current
            // record at its last position (the site stores `progress` from the
            // final beat) and re-anchor every field on the new source, so the
            // new episode gets its OWN history entry on its own aid/bvid/cid.
            let source_seq = control.source_seq();
            if source_seq != self.source_seq {
                let played_s = previous_pos_ms / 1000;
                let watched_s = watched_ms / 1000;
                if started && played_s >= HISTORY_MIN_PLAYED_S {
                    self.report(runtime, 4, played_s, watched_s);
                }
                crate::debug_log(&format!(
                    "history stop bvid={} reason=switch played={played_s} watched={watched_s} sent={}",
                    self.bvid,
                    u8::from(started && played_s >= HISTORY_MIN_PLAYED_S)
                ));
                self.source_seq = source_seq;
                let source = control.source();
                self.aid = source.aid.max(0) as u64;
                self.bvid = source.bvid;
                self.cid = source.cid;
                self.start_ts = unix_now_secs();
                self.duration_s = 0;
                self.last_sent = None;
                self.last_sent_pos_ms = 0;
                frames_base = control.presented_frames();
                started = false;
                watched_ms = 0;
                user_paused = control.user_paused();
                next_play_at = Instant::now();
                last_tick = Instant::now();
                continue;
            }
            // The new video's playurl lands after the switch: adopt its real
            // duration once, so the beats below carry the right length.
            if self.duration_s == 0 {
                self.duration_s = control.duration_ms() / 1000;
            }

            // The start proof: a frame this session actually handed to the UI.
            // Never at spawn — a video that fails before its first frame must
            // not create a history entry.
            if !started && control.presented_frames() > frames_base {
                started = true;
                crate::debug_log(&format!(
                    "history first-frame bvid={} pos_ms={pos_ms}",
                    self.bvid
                ));
            }
            // Wall time spent actually playing: a paused picture and a buffering
            // hold are both excluded, as is everything before the first frame.
            if started && !paused && !buffering {
                watched_ms = watched_ms.saturating_add(delta_ms);
            }

            let played_s = pos_ms / 1000;
            let watched_s = watched_ms / 1000;
            // The 5 s gate covers *every* beat, the start included: a short open
            // must not send a single request.
            let reportable = started && played_s >= HISTORY_MIN_PLAYED_S;

            if closed || ended {
                if reportable {
                    // The final beat ignores the 10 s floor: it is the request
                    // whose position the site stores as `progress`.
                    self.report(runtime, 4, played_s, watched_s);
                }
                crate::debug_log(&format!(
                    "history stop bvid={} reason={} played={played_s} watched={watched_s} sent={}",
                    self.bvid,
                    if closed { "closed" } else { "ended" },
                    u8::from(reportable)
                ));
                return;
            }
            if !reportable {
                continue;
            }

            // `1` once, on the first frame this session presented.
            if self.last_sent.is_none() {
                self.report(runtime, 1, played_s, watched_s);
                next_play_at = Instant::now() + HISTORY_PLAY_INTERVAL;
                last_tick = Instant::now();
            }

            // A real user pause / resume, once each. Buffer-guard holds are not
            // pauses (see [`PlaybackControl::user_paused`]), and a beat the
            // floor suppresses is simply dropped: state changes must not queue
            // up behind it.
            if paused && !user_paused {
                user_paused = true;
                if self.allowed() {
                    self.report(runtime, 2, played_s, watched_s);
                    last_tick = Instant::now();
                }
            } else if !paused && user_paused {
                user_paused = false;
                if self.allowed() {
                    self.report(runtime, 3, played_s, watched_s);
                    last_tick = Instant::now();
                }
            }

            // Periodic beat every 15 s, and only while the playhead advances.
            if !paused
                && !buffering
                && Instant::now() >= next_play_at
                && pos_ms > self.last_sent_pos_ms
                && self.allowed()
            {
                self.report(runtime, 0, played_s, watched_s);
                next_play_at = Instant::now() + HISTORY_PLAY_INTERVAL;
                last_tick = Instant::now();
            }
        }
    }
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
    // The history heartbeat's `start_ts` is the second this playback session
    // began — before playurl, so it matches "opened at" rather than "decoded at".
    let history_start_ts = unix_now_secs();

    // 选集: this thread owns the window's decode loop for its whole life; a pick
    // only replaces the SOURCE. `fell_back`, `last_resync` and the frame path
    // (blend pool + mailbox) are per-window state and deliberately survive a
    // switch, exactly like they survive a seek — only the playurl result, the
    // tracks, the prefetch proxy and the danmaku are re-armed for the new cid.
    //
    // The loop body below keeps the original indentation: it is the whole
    // per-video path (playurl → tracks → prefetch → danmaku → decode), and
    // re-indenting 300 lines would bury the three switch points in diff noise.
    let mut source = PlaybackSource {
        bvid: video.bvid.clone(),
        cid: video.cid,
        aid: video.aid as i64,
        title: video.title.clone(),
    };
    control.set_source(&source.bvid, source.cid, source.aid, &source.title);
    let mut fell_back = false;
    let mut last_resync: Option<Instant> = None;
    let blend_pool: Arc<BlendPool> = Arc::new(BlendPool::new());
    let mailbox = Arc::new(FrameMailbox::new(
        window.clone(),
        mini.clone(),
        blend_pool.clone(),
        control.clone(),
    ));
    'source: loop {
    let video = bilibili::FeedVideo {
        bvid: source.bvid.clone(),
        cid: source.cid,
        aid: source.aid.max(0) as u64,
        title: source.title.clone(),
        pic: video.pic.clone(),
        owner: video.owner.clone(),
        owner_mid: video.owner_mid,
        view: video.view,
        duration: video.duration,
        pub_ts: video.pub_ts,
    };
    let mut info = match runtime.block_on(session.playurl(&video.bvid, video.cid)) {
        Ok(info) => info,
        Err(err) => {
            crate::debug_log(&format!(
                "playurl failed bvid={} cid={} err={err}",
                video.bvid, video.cid
            ));
            set_status(
                &window,
                &format!("播放失败：{err}"),
                crate::TOAST_KIND_ERROR,
            );
            set_playing(&window, false);
            // A failed 选集 switch must not kill this window's decode thread:
            // hold the frozen picture and wait for another pick (or the close),
            // so a single bad episode leaves the player usable.
            if control.source_seq() > 0 && !control.is_closed() {
                crate::debug_log(&format!(
                    "series switch failed bvid={} cid={} err={err}",
                    video.bvid, video.cid
                ));
                while !control.is_closed() && !control.switch_pending() {
                    std::thread::sleep(Duration::from_millis(60));
                }
                if control.is_closed() {
                    return;
                }
                if let Some(next) = control.take_switch_request() {
                    source = next;
                    control.set_source(&source.bvid, source.cid, source.aid, &source.title);
                    continue 'source;
                }
            }
            return;
        }
    };

    control.set_duration_ms(info.duration.saturating_mul(1000));
    set_status(&window, "", crate::TOAST_KIND_INFO);

    // Watch history (历史记录): one lightweight reporter thread per session,
    // started here where the playurl result (`info.duration`) and the `video`
    // (aid / bvid / cid) are both in scope. It only samples this player's
    // control; nothing it does touches the UI or the pacer.
    if video.aid == 0 {
        // The queue path can carry `aid = 0`; the heartbeat body needs a real
        // one (the API would reject it), so this session is not reported.
        crate::debug_log(&format!(
            "history skipped reason=no-aid bvid={} cid={}",
            video.bvid, video.cid
        ));
    } else if control.claim_history_reporter() {
        let reporter = HistoryReporter::new(
            session.clone(),
            &video,
            info.duration,
            history_start_ts,
            control.source_seq(),
        );
        let reporter_control = control.clone();
        let _ = std::thread::Builder::new()
            .name("history".into())
            .spawn(move || {
                note_thread_role("history");
                let Some(runtime) = crate::bilibili::shared_runtime() else {
                    crate::debug_log("history skipped reason=no-runtime");
                    return;
                };
                reporter.run(runtime, &reporter_control);
            });
    }

    let prefs = crate::settings::load();
    // The QA hook forces a mode for the backend verification: `1` = blend,
    // `2` = block-match motion, `3` = hardware optical flow. Anything else (or
    // unset) keeps the mode this window's control already has: `main.rs` applies
    // `settings.interp_mode` when the player window is created, and an
    // `interp_cycle()` the user made while the window is open must not be undone
    // by the next video this same player opens.
    match std::env::var("BILI_INTERP_TEST").as_deref() {
        Ok("0") => control.set_interp_mode(0),
        Ok("1") => control.set_interp_mode(1),
        Ok("2") => control.set_interp_mode(2),
        Ok("3") => control.set_interp_mode(3),
        _ => {}
    }
    let preferred = prefs.player_quality;
    let initial_qn = initial_quality(&info, preferred);
    control.set_quality_initial(initial_qn);
    // 设置 → 播放 → 原始分辨率: the frame size is a session property, so the
    // first session takes it now and a change applies from the next restart.
    control.set_native_frame_size_initial(prefs.player_native_size);
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
    set_audio_tracks(&window, &info.audio_tracks, &control);
    if let Ok(mut meta) = control.audio_meta.lock() {
        *meta = info
            .audio_tracks
            .iter()
            .map(|track| {
                let mut flags = 0u8;
                if track.dolby {
                    flags |= 1;
                }
                if track.hi_res {
                    flags |= 2;
                }
                if track.vip {
                    flags |= 4;
                }
                (track.id, track.bandwidth, flags)
            })
            .collect();
    }
    // The proxy is opened for the track the first decode session will use. A
    // later 画质 switch opens a different bitstream, and the decode loop then
    // replaces this engine with one of its own before that session spawns (see
    // `switch_prefetch`): a proxy only ever serves the stream it was bound to.
    //
    // The cache file is named after the *stream*, not the player: a preload of
    // this playlist item (`PrefetchHandle::preload_keys`) writes exactly this
    // name, and reopening the same video+quality reuses it.
    let cache_key =
        stream_cache_key(video.bvid.as_str(), video.cid, initial_stream.0, initial_stream.1);
    // The audio proxy's own file, named after the audio stream it serves (the
    // track id is what distinguishes 64K/132K/192K/无损 for the same video).
    let audio_id = info
        .audio_tracks
        .get(control.audio_index())
        .or_else(|| info.audio_tracks.first())
        .map(|track| track.id)
        .unwrap_or(0);
    let audio_cache_key = format!("{}-{}-audio-{}", video.bvid, video.cid, audio_id);
    start_prefetch(
        &session,
        &mut info,
        initial_qn,
        prefs.codec_pref as i64,
        prefs.seek_points as usize,
        prefs.prefetch_connections as usize,
        cache_key,
        audio_cache_key,
        &control,
    );
    // Danmaku is fetched on its own thread: decoding never waits on the network.
    {
        let track = track.clone();
        let window = window.clone();
        let session = session.clone();
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

    if std::env::var("BILI_HWACCEL")
        .map(|value| value == "0")
        .unwrap_or(false)
    {
        control.force_software_decode();
    }
    // QA hook (`BILI_PLAY_AFTER_END=<ms>`): once the track latches as ended,
    // press play through the real `toggle_paused` path after `<ms>` and log what
    // the control did. This is the recovery proof for a (false or real) end
    // without synthesising input.
    if let Ok(value) = std::env::var("BILI_PLAY_AFTER_END") {
        if let Ok(delay) = value.trim().parse::<u64>() {
            let control = control.clone();
            std::thread::spawn(move || {
                for _ in 0..1200 {
                    if control.is_closed() {
                        return;
                    }
                    if control.track_ended() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if control.is_closed() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(delay.clamp(0, 30_000)));
                let before = control.playback_position_ms();
                let paused = control.toggle_paused();
                crate::debug_log(&format!(
                    "play-after-end press pos_ms={before} paused={} ended={} seek_seq={}",
                    u8::from(paused),
                    u8::from(control.track_ended()),
                    control.seek_seq()
                ));
                std::thread::sleep(Duration::from_millis(3000));
                crate::debug_log(&format!(
                    "play-after-end result pos_ms={} ended={} paused={} seek_seq={}",
                    control.playback_position_ms(),
                    u8::from(control.track_ended()),
                    u8::from(control.paused()),
                    control.seek_seq()
                ));
            });
        }
    }
    // QA hook (`BILI_SEEK_AT=<secs,secs,...>`): issue a scripted sequence of
    // real user seeks through `PlaybackControl::seek`, one every
    // `BILI_SEEK_AT_EVERY_MS` (default 3000) after `BILI_SEEK_AT_DELAY_MS`
    // (default 6000). This is how the seek-cache decision is verified over a
    // fully cached file, a partial one and an uncached region without synthetic
    // input.
    if let Ok(value) = std::env::var("BILI_SEEK_AT") {
        let targets: Vec<u64> = value
            .split(',')
            .filter_map(|part| part.trim().parse::<f64>().ok())
            .filter(|secs| *secs >= 0.0)
            .map(|secs| (secs * 1000.0) as u64)
            .collect();
        if !targets.is_empty() {
            let control = control.clone();
            let delay = std::env::var("BILI_SEEK_AT_DELAY_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(6000)
                .clamp(500, 600_000);
            let every = std::env::var("BILI_SEEK_AT_EVERY_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(3000)
                .clamp(500, 120_000);
            std::thread::spawn(move || {
                for _ in 0..600 {
                    if control.is_closed() {
                        return;
                    }
                    if control.duration_ms() > 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::thread::sleep(Duration::from_millis(delay));
                for (index, target) in targets.iter().enumerate() {
                    if control.is_closed() {
                        return;
                    }
                    let before = control.playback_position_ms();
                    crate::debug_log(&format!(
                        "seek-at n={} target_ms={target} before_ms={before} dur_ms={}",
                        index + 1,
                        control.duration_ms()
                    ));
                    control.seek(*target);
                    std::thread::sleep(Duration::from_millis(every));
                }
                crate::debug_log("seek-at done");
            });
        }
    }
    loop {
        match decode_loop(
            &info,
            video.cid,
            &window,
            &control,
            &mailbox,
            &blend_pool,
            &mut last_resync,
        ) {
            Ok(()) => {
                if control.is_closed() {
                    break;
                }
                // 选集 pick: the decode session retired for the new video (see
                // `decode_loop`'s first check). Re-arm on it — same window, same
                // control, same mailbox; only playurl/tracks/danmaku change.
                if let Some(next) = control.take_switch_request() {
                    crate::debug_log(&format!(
                        "series switch worker from={} cid={} to={} cid={} aid={}",
                        source.bvid, source.cid, next.bvid, next.cid, next.aid,
                    ));
                    // 弹幕 belong to the cid being left: drop them now so the old
                    // episode's comments never scroll over the new picture while
                    // the new cid's list is still in flight.
                    track.set_items(Vec::new());
                    source = next;
                    control.set_source(&source.bvid, source.cid, source.aid, &source.title);
                    continue 'source;
                }
                // A seek is waiting for a decoder session: start it at once
                // (this is the end→seek and seek-during-finish path). `pending`
                // is set by every seek, so a race with the session ending cannot
                // be missed, unlike a comparison with a sequence captured before
                // a loop that may itself have consumed several seeks.
                if control.has_pending_seek() {
                    continue;
                }
                if !control.track_ended() {
                    // The session stopped with the track NOT over: the decoder
                    // pipe broke (or closed early) while frames were still due.
                    // Do not declare the video finished — hold the decode thread
                    // open and wait for a seek or a play press, both of which
                    // queue a new position and restart it.
                    control.end_trace("run", false);
                    set_status(
                        &window,
                        "播放中断，拖动进度条或按播放重试",
                        crate::TOAST_KIND_INFO,
                    );
                    crate::debug_log(&format!(
                        "decode hold reason=interrupted pos_ms={} dur_ms={}",
                        control.playback_position_ms(),
                        control.duration_ms()
                    ));
                    while !control.is_closed()
                        && !control.has_pending_seek()
                        && !control.resume_requested()
                        && !control.switch_pending()
                    {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    if control.is_closed() {
                        break;
                    }
                    control.clear_resume_request();
                    continue;
                }
                // The track is over but the picture stays: only a live player
                // toasts, and a closing window cannot toast into the next
                // player.
                set_status(&window, "播放结束", crate::TOAST_KIND_INFO);
                set_playing(&window, false);
                // Hold the finished picture until a seek asks for another
                // position. The decode thread stays alive, so dragging the bar
                // back after the end restarts the decoder at the target instead
                // of leaving a dead player.
                crate::debug_log(&format!(
                    "end hold position_ms={} duration_ms={}",
                    control.playback_position_ms(),
                    control.duration_ms()
                ));
                while !control.is_closed()
                    && !control.has_pending_seek()
                    && !control.switch_pending()
                {
                    std::thread::sleep(Duration::from_millis(40));
                }
                if control.is_closed() {
                    break;
                }
                crate::debug_log(&format!(
                    "end resume position_ms={} duration_ms={}",
                    control.playback_position_ms(),
                    control.duration_ms()
                ));
                continue;
            }
            Err(err) => {
                // A proxy failure must never be the reason a player stays
                // black: drop it and retry the same position directly. The
                // retry is bounded because `disable_prefetch` removes it.
                if control.prefetch_active() {
                    crate::debug_log(&format!(
                        "decode failed via prefetch err={err}; retrying direct"
                    ));
                    set_status(&window, "预取代理异常，已改用直连", crate::TOAST_KIND_INFO);
                    control.disable_prefetch(&format!("decode failed err={err}"));
                    continue;
                }
                if control.hwaccel() && !fell_back {
                    fell_back = true;
                    if frame_gpu_enabled() && !gpu_frames_failed() {
                        // The GPU scaler is the part that failed (or the CUDA
                        // decoder under it): keep the CPU path for the rest of
                        // the session instead of retrying a broken driver.
                        disable_gpu_frames();
                        crate::debug_log(&format!(
                            "frame gpu failed err={err}; GPU frames disabled, falling back to software decode"
                        ));
                    }
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
                    // Recovery is mandatory: an error must not leave a player
                    // that can never play again. Hold the decode thread open —
                    // a seek or a play press queues a fresh session, and the
                    // retry starts from the position the picture stopped at.
                    set_playing(&window, false);
                    crate::debug_log(&format!(
                        "decode hold reason=error pos_ms={} dur_ms={}",
                        control.playback_position_ms(),
                        control.duration_ms()
                    ));
                    while !control.is_closed()
                        && !control.has_pending_seek()
                        && !control.resume_requested()
                        && !control.switch_pending()
                    {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    if control.is_closed() {
                        break;
                    }
                    control.clear_resume_request();
                    continue;
                }
                break;
            }
        }
    }
    // The decode loop only ends when the player is closed (a switch never
    // reaches here: it continues the source loop above).
    break;
    }
    control.disable_prefetch("playback ended");
    set_playing(&window, false);
}

/// `BILI_PRESENT_TRACE=1`: per-100 ms presented-frame buckets for the first two
/// seconds of a decode session, plus where the frames that did not arrive went.
///
/// This is the instrument the "the picture is slow right after open / after a
/// seek" report needs: a single fps number cannot tell a decoder that was still
/// connecting from a pacer held by the A/V guard, a ring that stayed empty, or a
/// catch-up burst that dropped frames.
struct PresentTrace {
    enabled: bool,
    /// First presented frame of the session; the buckets are relative to it.
    start: Option<Instant>,
    buckets: [u32; 20],
    /// Number of 100 ms polls that found the ring empty (waiting for the
    /// decoder), and their total wall time.
    empty_polls: u64,
    /// Time spent in the A/V sync sleep (the pacer holding the picture back).
    sync_ms: u64,
    /// Source frames dropped as "behind the clock".
    dropped: u64,
    /// Display slots skipped because their tick was already late.
    ticks_dropped: u64,
    logged: bool,
}

impl PresentTrace {
    fn new() -> Self {
        Self {
            enabled: std::env::var("BILI_PRESENT_TRACE")
                .map(|value| value == "1")
                .unwrap_or(false),
            start: None,
            buckets: [0; 20],
            empty_polls: 0,
            sync_ms: 0,
            dropped: 0,
            ticks_dropped: 0,
            logged: false,
        }
    }

    fn present(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let start = *self.start.get_or_insert(now);
        let ms = now.duration_since(start).as_millis() as usize;
        if let Some(bucket) = self.buckets.get_mut(ms / 100) {
            *bucket = bucket.saturating_add(1);
        }
        self.maybe_log(now, start);
    }

    fn empty(&mut self) {
        self.empty_polls = self.empty_polls.saturating_add(1);
    }

    fn sync(&mut self, ms: u64) {
        self.sync_ms = self.sync_ms.saturating_add(ms);
    }

    fn drop_source(&mut self, n: u64) {
        self.dropped = self.dropped.saturating_add(n);
    }

    fn drop_ticks(&mut self, n: u64) {
        self.ticks_dropped = self.ticks_dropped.saturating_add(n);
    }

    /// Log once, 2.1 s after the first presented frame (a little past the
    /// window so the last bucket is complete).
    fn maybe_log(&mut self, now: Instant, start: Instant) {
        if self.logged || now.duration_since(start) < Duration::from_millis(2100) {
            return;
        }
        self.logged = true;
        let counts: Vec<String> = self.buckets.iter().map(|n| n.to_string()).collect();
        crate::debug_log(&format!(
            "present trace buckets=[{}] empty_polls={} sync_ms={} dropped={} ticks_dropped={}",
            counts.join(","),
            self.empty_polls,
            self.sync_ms,
            self.dropped,
            self.ticks_dropped,
        ));
    }
}

/// Dip attribution for the `BILI_PROFILER_LOG=1` stream: one line for every
/// second in which the pacer handed the UI fewer than 95 % of the track's own
/// frame rate — 28.5 fps on a 30 fps track, and the matching share of a 24/25 fps
/// one — after the first [`DIP_GRACE_S`] seconds of the session.
///
/// The profiler's `fps_presented` says *that* a second was short; only the
/// pacer can say *why*, and the causes look completely different here:
///
/// * **not decoded** — `decoded` is short as well and the ring stayed low:
///   the reader/decoder starved and there was nothing to present.
/// * **dropped** — `skipped` grew in that second: the pacer threw frames away
///   (`skip_behind` = behind the clock, `gate_waits` = the burst gate).
/// * **late** — `decoded` was healthy, `skipped` did not move and the ring was
///   full, but `sync_ms`/`gap_ms` are large: the frame existed and the pacer
///   held it until its slot had passed.
///
/// Everything is one branch on a `bool` and integer adds when disabled; the
/// expensive formatting happens only for a second that is already a dip.
struct DipMeter {
    enabled: bool,
    /// Threshold for this track's frame rate, in milli-fps.
    min_fps_milli: u64,
    /// First tick at which this session had presented a frame.
    start: Option<Instant>,
    /// Seconds elapsed since `start`, 0-based.
    second: u64,
    /// Cumulative counters at the previous boundary.
    presented0: u64,
    decoded0: u64,
    skipped0: u64,
    ticks_dropped0: u64,
    /// Raw audio-clock sample errors at the previous boundary (see
    /// `set_audio_clock_ms`): what the master clock asked the pacer to pay this
    /// second, split by direction.
    clk_back0: u64,
    clk_fwd0: u64,
    clk_n0: u64,
    /// This second's accumulated wall time.
    sync_ms: u64,
    pop_ms: u64,
    interp_ms: u64,
    /// Slowest frame of this second: ring wait before it, and the time from
    /// "popped from the ring" to "handed to the UI".
    worst_pop_ms: u64,
    worst_admit_ms: u64,
    /// Slowest phase of this second, so a dip names where the time went
    /// instead of only how much: before the pop, the pop itself, the work
    /// between the clock hold and the push, and the time after it.
    worst_pre_ms: u64,
    worst_busy_ms: u64,
    worst_book_ms: u64,
    /// Wall time between the previous frame's clock check and the next one,
    /// i.e. the total iteration period.
    worst_gap_ms: u64,
    /// Per-iteration phases as they are noted, so the part of the iteration
    /// that no phase accounts for is measurable: the loop was asleep somewhere
    /// outside its own bookkeeping (the gate poll, the pause loop) or the
    /// thread was not scheduled.
    iter_pre: u64,
    iter_pop: u64,
    iter_book: u64,
    iter_sync: u64,
    iter_busy: u64,
    worst_unexplained_ms: u64,
    /// Smallest/largest A/V delta (picture minus clock) seen this second, and
    /// how many iterations had no audio clock at all.
    delta_min: i64,
    delta_max: i64,
    delta_prev: Option<i64>,
    /// Largest one-frame step of the A/V relation: a clock discontinuity.
    worst_step_ms: u64,
    no_clock: u64,
    /// Why this second lost source frames (only `behind` can lose one now) and
    /// how often the burst gate made a frame *wait* instead.
    skip_behind: u64,
    gate_waits: u64,
    /// Sum of per-frame work, so a systematic overhead is distinguishable
    /// from one spike.
    busy_sum_ms: u64,
    /// Pacer iterations (presenting and holding both) in this second.
    loops: u64,
    ring_min: usize,
    ring_max: usize,
    /// Previous tick, for the worst pacer iteration period of the second.
    prev: Option<Instant>,
}

/// A second below this fraction of the track's own frame rate is a dip worth a
/// line. 95 % of 30 fps is the 28.5 the QA run uses; taking it from the track's
/// period rather than a fixed 28 keeps a 24/25 fps source from reading as a dip
/// when it is presenting every one of its frames.
const DIP_MIN_SHARE: f64 = 0.95;
/// Startup seconds are excluded: the pre-roll and the first-clock settle are
/// expected to be short and are covered by `present trace`.
const DIP_GRACE_S: u64 = 5;

impl DipMeter {
    fn new(frame_ms: f64) -> Self {
        // Same switch as the profiler line: one QA run should not need two
        // environment variables to get both halves of the same story.
        let enabled = std::env::var("BILI_PROFILER_LOG")
            .map(|value| value == "1")
            .unwrap_or(false)
            || std::env::var("BILI_DIP_LOG")
                .map(|value| value == "1")
                .unwrap_or(false);
        let track_fps = if frame_ms.is_finite() && frame_ms > 0.0 {
            1000.0 / frame_ms
        } else {
            30.0
        };
        let min_fps_milli = (track_fps * DIP_MIN_SHARE * 1000.0) as u64;
        Self {
            enabled,
            min_fps_milli,
            start: None,
            second: 0,
            presented0: 0,
            decoded0: 0,
            skipped0: 0,
            ticks_dropped0: 0,
            clk_back0: 0,
            clk_fwd0: 0,
            clk_n0: 0,
            sync_ms: 0,
            pop_ms: 0,
            interp_ms: 0,
            worst_pop_ms: 0,
            worst_admit_ms: 0,
            worst_pre_ms: 0,
            worst_busy_ms: 0,
            worst_book_ms: 0,
            iter_pre: 0,
            iter_pop: 0,
            iter_book: 0,
            iter_sync: 0,
            iter_busy: 0,
            worst_unexplained_ms: 0,
            delta_min: i64::MAX,
            delta_max: i64::MIN,
            delta_prev: None,
            worst_step_ms: 0,
            no_clock: 0,
            skip_behind: 0,
            gate_waits: 0,
            busy_sum_ms: 0,
            ring_min: usize::MAX,
            ring_max: 0,
            prev: None,
            worst_gap_ms: 0,
            loops: 0,
        }
    }

    /// Close a second boundary and emit the line when that second was short.
    /// Called at the top of every pacer iteration, so it runs for the stall /
    /// empty-ring paths too (a starve has no push to hang the meter on).
    ///
    /// `gated` is the pre-roll / startup gate: while it is closed the pacer
    /// intentionally presents nothing, which is not a dip.
    fn tick(&mut self, control: &PlaybackControl, now: Instant, skipped: u64, gated: bool) {
        if !self.enabled {
            return;
        }
        let prof = &control.prof;
        let presented = prof.presented.load(Ordering::Relaxed);
        let decoded = prof.decoded.load(Ordering::Relaxed);
        let ticks_dropped = control.interp_dropped.load(Ordering::Relaxed);
        let ring = control.ring_len();
        self.loops = self.loops.saturating_add(1);
        if let Some(prev) = self.prev {
            let gap = now.saturating_duration_since(prev).as_millis() as u64;
            self.worst_gap_ms = self.worst_gap_ms.max(gap);
            // The one iteration that just ended: whatever its own phases did
            // not account for was spent outside the loop's measured work (a
            // gate poll, the pause hold) or off-CPU entirely.
            let phases = self
                .iter_pre
                .saturating_add(self.iter_pop)
                .saturating_add(self.iter_book)
                .saturating_add(self.iter_sync)
                .saturating_add(self.iter_busy);
            self.worst_unexplained_ms = self.worst_unexplained_ms.max(gap.saturating_sub(phases));
        }
        self.iter_pre = 0;
        self.iter_pop = 0;
        self.iter_book = 0;
        self.iter_sync = 0;
        self.iter_busy = 0;
        self.prev = Some(now);
        let start = match self.start {
            Some(start) => start,
            None => {
                if presented == 0 {
                    // Not playing yet (pre-roll, buffering): the clock has not
                    // started, so nothing in this second is a dip.
                    self.reset_window(control, now, skipped, ring);
                    return;
                }
                self.start = Some(now);
                self.reset_window(control, now, skipped, ring);
                return;
            }
        };
        let span = now.saturating_duration_since(start);
        self.ring_min = self.ring_min.min(ring);
        self.ring_max = self.ring_max.max(ring);
        if span.as_millis() < 1000 {
            return;
        }
        let span_ms = span.as_millis() as u64;
        let d_presented = presented.saturating_sub(self.presented0);
        let d_decoded = decoded.saturating_sub(self.decoded0);
        let d_skipped = skipped.saturating_sub(self.skipped0);
        let d_ticks = ticks_dropped.saturating_sub(self.ticks_dropped0);
        let clk_back = control
            .audio_clock_back_ms
            .load(Ordering::Relaxed)
            .saturating_sub(self.clk_back0);
        let clk_fwd = control
            .audio_clock_fwd_ms
            .load(Ordering::Relaxed)
            .saturating_sub(self.clk_fwd0);
        let clk_n = control
            .audio_clock_samples
            .load(Ordering::Relaxed)
            .saturating_sub(self.clk_n0);
        // Normalise to a per-second count: the boundary follows the loop, not
        // a wall clock, so a span above 1 s is expected and must not read as a
        // dip by itself.
        let fps_milli = d_presented * 1_000_000 / span_ms.max(1);
        let emit = !gated
            && !control.auto_paused()
            && self.second >= DIP_GRACE_S
            && fps_milli < self.min_fps_milli;
        if emit {
            let lead = prof.av_lead_ms.load(Ordering::Relaxed);
            let av = match prof.av_state.load(Ordering::Relaxed) {
                AV_RUN => "run",
                AV_HOLD => "hold",
                _ => "off",
            };
            let fps = d_presented as f64 * 1000.0 / span_ms.max(1) as f64;
            let dmin = if self.delta_min == i64::MAX {
                0
            } else {
                self.delta_min
            };
            let dmax = if self.delta_max == i64::MIN {
                0
            } else {
                self.delta_max
            };
            crate::debug_log(&format!(
                "dip t={second}s span_ms={span_ms} presented={presented} decoded={decoded} fps={fps:.2} skipped={skipped} skip_behind={skip_behind} gate_waits={gate_waits} ticks_dropped={ticks} ring={ring_min}..{ring_max} loops={loops} sync_ms={sync} pop_ms={pop} pre_ms={pre} busy_ms={busy} busy_sum_ms={busy_sum} book_ms={book} rest_ms={rest} interp_ms={interp} gap_ms={gap} admit_ms={admit} delta={dmin}..{dmax} step_max_ms={step} no_clock={no_clock} clk_back_ms={clk_back} clk_fwd_ms={clk_fwd} clk_n={clk_n} pos_ms={pos} dur_ms={dur} lead_ms={lead} av={av} paused={paused}",
                second = self.second,
                presented = d_presented,
                decoded = d_decoded,
                skipped = d_skipped,
                skip_behind = self.skip_behind,
                gate_waits = self.gate_waits,
                ticks = d_ticks,
                ring_min = self.ring_min,
                ring_max = self.ring_max,
                loops = self.loops,
                sync = self.sync_ms,
                pop = self.pop_ms,
                pre = self.worst_pre_ms,
                busy = self.worst_busy_ms,
                busy_sum = self.busy_sum_ms,
                book = self.worst_book_ms,
                rest = self.worst_unexplained_ms,
                interp = self.interp_ms,
                gap = self.worst_gap_ms,
                admit = self.worst_admit_ms,
                step = self.worst_step_ms,
                no_clock = self.no_clock,
                clk_back = clk_back,
                clk_fwd = clk_fwd,
                clk_n = clk_n,
                pos = control.playback_position_ms(),
                dur = control.duration_ms(),
                paused = u8::from(control.is_paused()),
            ));
        }
        // Next boundary: rebase every counter on *this* instant so nothing is
        // counted twice and a long stall cannot leak into the next second.
        self.second += 1;
        self.reset_window(control, now, skipped, ring);
    }

    /// Start a fresh per-second window at `now` (every counter rebased).
    fn reset_window(
        &mut self,
        control: &PlaybackControl,
        now: Instant,
        skipped: u64,
        ring: usize,
    ) {
        let prof = &control.prof;
        self.start = Some(now);
        self.presented0 = prof.presented.load(Ordering::Relaxed);
        self.decoded0 = prof.decoded.load(Ordering::Relaxed);
        self.skipped0 = skipped;
        self.ticks_dropped0 = control.interp_dropped.load(Ordering::Relaxed);
        self.clk_back0 = control.audio_clock_back_ms.load(Ordering::Relaxed);
        self.clk_fwd0 = control.audio_clock_fwd_ms.load(Ordering::Relaxed);
        self.clk_n0 = control.audio_clock_samples.load(Ordering::Relaxed);
        self.sync_ms = 0;
        self.pop_ms = 0;
        self.interp_ms = 0;
        self.worst_pop_ms = 0;
        self.worst_gap_ms = 0;
        self.worst_admit_ms = 0;
        self.worst_pre_ms = 0;
        self.worst_busy_ms = 0;
        self.worst_book_ms = 0;
        self.worst_unexplained_ms = 0;
        self.iter_pre = 0;
        self.iter_pop = 0;
        self.iter_book = 0;
        self.iter_sync = 0;
        self.iter_busy = 0;
        self.delta_min = i64::MAX;
        self.delta_max = i64::MIN;
        self.worst_step_ms = 0;
        self.no_clock = 0;
        self.skip_behind = 0;
        self.gate_waits = 0;
        self.busy_sum_ms = 0;
        self.loops = 0;
        self.ring_min = ring;
        self.ring_max = ring;
    }

    /// The pacer slept this long waiting for the clock (the A/V hold).
    fn sync(&mut self, ms: u64) {
        if self.enabled {
            self.sync_ms = self.sync_ms.saturating_add(ms);
            self.iter_sync = self.iter_sync.saturating_add(ms);
        }
    }

    /// Time this frame spent blocked on the ring (no decoded frame available).
    fn pop(&mut self, ms: u64) {
        if self.enabled {
            self.pop_ms = self.pop_ms.saturating_add(ms);
            self.worst_pop_ms = self.worst_pop_ms.max(ms);
            self.iter_pop = self.iter_pop.saturating_add(ms);
        }
    }

    /// Loop-top to "frame in hand": gate / pause / plan / position bookkeeping.
    fn pre(&mut self, ms: u64) {
        if self.enabled {
            self.worst_pre_ms = self.worst_pre_ms.max(ms);
            self.iter_pre = self.iter_pre.saturating_add(ms);
        }
    }

    /// The A/V relation this frame was paced against (`None` = no clock), so a
    /// second that held can be told from a second that was starved.
    fn delta(&mut self, delta: i64, has_clock: bool) {
        if !self.enabled {
            return;
        }
        self.delta_min = self.delta_min.min(delta);
        self.delta_max = self.delta_max.max(delta);
        // A step in the relation between two frames is a step in the *clock*
        // (a new `ffplay` sample, the audio clock going stale, the wall-clock
        // fallback taking over), not the picture moving on its own. One frame
        // can only change it by its own period.
        if let Some(previous) = self.delta_prev {
            let step = delta.saturating_sub(previous).unsigned_abs();
            self.worst_step_ms = self.worst_step_ms.max(step);
        }
        self.delta_prev = Some(delta);
        if !has_clock {
            self.no_clock = self.no_clock.saturating_add(1);
        }
    }

    /// A source frame was thrown away because the picture was behind the clock.
    fn skip_behind(&mut self, n: u64) {
        if self.enabled {
            self.skip_behind = self.skip_behind.saturating_add(n);
        }
    }

    /// The burst gate made this frame wait for `push_interval` instead of the
    /// pacer dropping it. A non-zero count means a catch-up after a hiccup.
    fn gate_wait(&mut self) {
        if self.enabled {
            self.gate_waits = self.gate_waits.saturating_add(1);
        }
    }

    /// Clock hold released to "frame handed to the UI": the real per-frame work
    /// (blend, screenshot stash, mailbox). Measured after the sleep so a frame
    /// that is merely *held* does not look like a frame that is *slow*.
    fn busy(&mut self, ms: u64) {
        if self.enabled {
            self.worst_busy_ms = self.worst_busy_ms.max(ms);
            self.busy_sum_ms = self.busy_sum_ms.saturating_add(ms);
            self.iter_busy = self.iter_busy.saturating_add(ms);
        }
    }

    /// Frame in hand to clock hold: bookkeeping, look-ahead, guard, plan.
    fn book(&mut self, ms: u64) {
        if self.enabled {
            self.worst_book_ms = self.worst_book_ms.max(ms);
            self.iter_book = self.iter_book.saturating_add(ms);
        }
    }

    /// Time spent inside the display-tick loop (blend + tick waits).
    fn interp(&mut self, ms: u64) {
        if self.enabled {
            self.interp_ms = self.interp_ms.saturating_add(ms);
        }
    }

    /// Admission time of one frame: from "popped out of the ring" to "handed
    /// to the UI". The maximum is reported, so one slow frame is visible even
    /// when the rest of the second was fine.
    fn admit(&mut self, ms: u64) {
        if self.enabled {
            self.worst_admit_ms = self.worst_admit_ms.max(ms);
        }
    }
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
    /// Startup gate: the last tick at which the stream was unhealthy (stall,
    /// auto-pause, buffering overlay, seek buffering). Background work is held
    /// until the stream has been healthy for [`GATE_HEALTHY_MS`] and playback
    /// has been running for [`GATE_PLAYING_MS`]; a new unhealthy tick holds it
    /// again at the next tick.
    unhealthy_at: Instant,
    /// Startup gate: the empty-ring stall counter as of the previous tick, so a
    /// stall that starts and ends between two ticks still counts as unhealthy.
    seen_stalls: u64,
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
            unhealthy_at: Instant::now(),
            seen_stalls: control.prof.stalls.load(Ordering::Relaxed),
        }
    }

    /// Playback-first scheduling: hold or release the proxy's seek-point warm
    /// loop from the state this sampler already observes.
    ///
    /// The proxy is held from installation (see
    /// [`PlaybackControl::install_prefetch`]) until **both** of these hold:
    ///
    /// * the first frame entered the ring at least [`GATE_PLAYING_MS`] ago, and
    /// * no stall / auto-pause / buffering / seek-buffering has been seen for
    ///   [`GATE_HEALTHY_MS`].
    ///
    /// After that a new unhealthy tick holds background work again until the
    /// stream has been healthy for [`GATE_HEALTHY_MS`] — so a slow link warms
    /// its seek points in the gaps instead of making the picture worse.
    /// Everything is measured from counters the pacer already maintains; no new
    /// work is added to the frame path.
    fn drive_startup_gate(&mut self, control: &PlaybackControl, now: Instant) {
        if !crate::media_hooks::startup_gate_enabled() {
            return;
        }
        let stalls = control.prof.stalls.load(Ordering::Relaxed);
        // A stall that resolved between two ticks still restarts the window, and
        // an empty ring is a starve even before the watchdog counts it.
        let unhealthy = stalls != self.seen_stalls
            || control.ring_len() == 0
            || control.buffering()
            || control.auto_paused()
            || control.seek_buffering();
        self.seen_stalls = stalls;
        if unhealthy {
            self.unhealthy_at = now;
        }
        let first_frame_ms = control.first_frame_at_ms();
        let playing_ms = if first_frame_ms == 0 {
            0
        } else {
            monotonic_ms().saturating_sub(first_frame_ms)
        };
        let healthy_ms = now
            .saturating_duration_since(self.unhealthy_at)
            .as_millis() as u64;
        control.set_prefetch_background_paused(!startup_gate_ready(playing_ms, healthy_ms));
    }

    /// Sample the counters and refresh [`Stats`]' derived values.
    fn tick(&mut self, control: &PlaybackControl, now: Instant) {
        if let Some(last) = self.last_tick {
            if now.duration_since(last) < Duration::from_millis(PROF_TICK_MS) {
                return;
            }
        }
        self.last_tick = Some(now);
        self.drive_startup_gate(control, now);
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
        if let (Some(old), Some(new)) = (self.window.front().copied(), self.window.back().copied())
        {
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
                // The A/V relation the guard measured on the newest presented
                // frame (sound minus picture). `av=hold` means the sound was
                // suspended for that measurement, `av=off` that there is no
                // audio clock at all.
                let lead = control.prof.av_lead_ms.load(Ordering::Relaxed);
                let av = match control.prof.av_state.load(Ordering::Relaxed) {
                    AV_RUN => "run",
                    AV_HOLD => "hold",
                    _ => "off",
                };
                crate::debug_log(&format!(
                    "profiler fps_presented={:.1} fps_decoded={:.1} first_ms={} buffer_s={:.2} dbuf={:.2} est_bps={} decl_bps={} qn={} codec={} frame={}x{} stalls={} stall_max_ms={} pos_ms={} dur_ms={} lead_ms={lead} av={av}",
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

/// The startup gate's release rule: background work may run only after playback
/// has been running for [`GATE_PLAYING_MS`] **and** the stream has been healthy
/// for [`GATE_HEALTHY_MS`]. Pure, so the timing contract is unit-tested.
fn startup_gate_ready(playing_ms: u64, healthy_ms: u64) -> bool {
    playing_ms >= GATE_PLAYING_MS && healthy_ms >= GATE_HEALTHY_MS
}

/// Buffered fraction toward `target_frames`, in thousandths (`0..=1000`).
/// `target_frames` is at least the ring's one frame, so this cannot divide by
/// zero; a ring already past the target reports full.
fn buffer_progress_milli(ring_len: usize, target_frames: usize) -> u64 {
    let target = target_frames.max(1) as u64;
    ((ring_len as u64).min(target) * 1000 / target).min(1000)
}

/// The `(pause, resume)` buffered-lead thresholds, in milliseconds, for a
/// session whose frames are `frame_ms` apart and whose ring holds `capacity`
/// frames.
///
/// The targets are [`AUTO_PAUSE_LEAD_MS`] and [`AUTO_RESUME_LEAD_MS`], but the
/// ring is a physical ceiling: `capacity × frame_ms` is the most video the
/// pacer can ever have buffered ahead. A 1080p ring, for example, holds 8
/// frames (~267 ms at 30 fps), so a fixed 600 ms resume threshold could never
/// be reached and the player would stay paused forever. The resume threshold
/// is therefore capped one frame below that ceiling, and the pause threshold
/// two frames below it; the two always stay at least one frame apart.
fn auto_buffer_thresholds(frame_ms: f64, capacity: usize) -> (f64, f64) {
    let frame_ms = if frame_ms.is_finite() && frame_ms > 0.0 {
        frame_ms
    } else {
        1000.0 / 30.0
    };
    let capacity_ms = capacity.max(1) as f64 * frame_ms;
    let pause = AUTO_PAUSE_LEAD_MS.min((capacity_ms - 2.0 * frame_ms).max(frame_ms));
    let resume = AUTO_RESUME_LEAD_MS
        .min(capacity_ms - frame_ms)
        .max(pause + frame_ms);
    (pause, resume)
}

/// Frames of ring occupancy that release an auto pause, never more than the
/// ring can hold.
fn auto_resume_frames(resume_ms: f64, frame_ms: f64, capacity: usize) -> usize {
    let frame_ms = if frame_ms.is_finite() && frame_ms > 0.0 {
        frame_ms
    } else {
        1000.0 / 30.0
    };
    ((resume_ms / frame_ms).ceil() as usize).clamp(1, capacity.max(1))
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
    /// Frames of ring occupancy this gate waits for, clamped to the ring
    /// capacity (the pre-roll can never exceed what the ring holds for this
    /// session's frame size).
    ///
    /// The first session of a player asks for [`PRE_ROLL_START_FRAMES`] (~0.5 s
    /// at 30 fps); a post-stall recovery asks for the whole ring, because it is
    /// recovering from an empty one.
    fn target_frames(self, capacity: usize) -> usize {
        let capacity = capacity.max(1);
        match self {
            GateKind::Initial => PRE_ROLL_START_FRAMES.min(capacity),
            GateKind::Recovery => capacity,
        }
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

/// How a startup gate decides that playback may begin.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GateMode {
    /// The prefetch proxy must hold enough of the stream ahead of the start
    /// position. This is the gate that fixes the stuttering first seconds: the
    /// ring can only ever hold 0.3-0.5 s, the proxy cache holds seconds.
    Cache,
    /// No proxy (direct CDN URL): the original ring pre-roll.
    Ring,
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
    /// Monotonic ms of the seek request, for the first-frame latency.
    requested_at_ms: u64,
    /// Download-map dump and upstream byte counter when the seek started; the
    /// first presented frame compares against both (map identical, upstream 0
    /// for a cached target).
    map_before: Option<(String, u64, u64)>,
    bytes_before: u64,
}

/// The one `seek served` line: latency, overlay, and the two proofs the seek
/// test reads — the download map digest before/after, and how many upstream
/// bytes the seek cost (0 for a target the cache file holds).
fn log_seek_served(control: &PlaybackControl, wait: &SeekWait, pushes: u64) {
    let overlay_ms = if wait.published {
        control
            .end_seek_buffering()
            .map(|ms| ms.to_string())
            .unwrap_or_else(|| "0".to_string())
    } else {
        "0".to_string()
    };
    let first_frame_ms = monotonic_ms().saturating_sub(wait.requested_at_ms);
    let map_after = control.prefetch_map_dump();
    let upstream = control
        .prefetch_raw_stats()
        .map(|stats| stats.bytes_fetched)
        .unwrap_or(0)
        .saturating_sub(wait.bytes_before);
    let (map_same, before, after) = match (&wait.map_before, &map_after) {
        (Some(before), Some(after)) => (
            before == after,
            format!("{}r/{}b/{}", before.0, before.1, before.2),
            format!("{}r/{}b/{}", after.0, after.1, after.2),
        ),
        _ => (false, "-".to_string(), "-".to_string()),
    };
    crate::debug_log(&format!(
        "seek served target_ms={} buffered={} overlay_ms={overlay_ms} pushes={pushes} first_frame_ms={first_frame_ms} upstream_bytes={upstream} map_before={before} map_after={after} map_same={}",
        wait.target,
        wait.buffered,
        u8::from(map_same)
    ));
}

/// Arm the seek overlay once the release's silent window has expired.
///
/// f151: the picture is frozen from the release, so the overlay is no longer
/// "something the old picture hides"; it is the only thing telling the user the
/// wait is real. It is therefore published purely on time (300ms), for cached
/// and uncached targets alike, and stays up until the new first frame closes it.
fn arm_seek_overlay(control: &PlaybackControl, wait: &mut SeekWait) {
    if wait.published || Instant::now() < wait.deadline {
        return;
    }
    wait.published = true;
    control.begin_seek_buffering(wait.target);
    crate::debug_log(&format!(
        "seek buffer armed target_ms={} reason=freeze_ms={} cached={}",
        wait.target,
        SEEK_FREEZE_OVERLAY_MS,
        u8::from(wait.buffered)
    ));
}

/// Contiguous bytes the cache file must hold from a session's start before the
/// decoder reads it as a **local file** instead of the proxy.
///
/// The default is "the whole rest of the media", because a sparse cache file
/// has holes where nothing was fetched and ffmpeg would read zeros there: only a
/// run that reaches EOF cannot break mid-playback. `BILI_CACHE_LOCAL_MB=<n>` is
/// the QA hook that lowers the bar to `n` contiguous MiB (used to measure the
/// local-file seek latency against the proxy path on a partially cached file).
fn local_decode_run(remaining: u64) -> u64 {
    static FORCED: OnceLock<Option<u64>> = OnceLock::new();
    let forced = *FORCED.get_or_init(|| {
        std::env::var("BILI_CACHE_LOCAL_MB")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|mb| *mb > 0)
            .map(|mb| mb.saturating_mul(1 << 20))
    });
    match forced {
        Some(bytes) => bytes.min(remaining.max(1)),
        None => remaining.max(1),
    }
}

/// Bytes of proxy cache that open the startup gate for a `declared_bps` stream:
/// the smaller of [`STARTUP_READY_BYTES`] and [`STARTUP_READY_SECONDS`] of that
/// bitrate. An unknown bitrate (`0`) can only be judged by the byte cap.
fn startup_ready_bytes(declared_bps: u64) -> u64 {
    let by_time = declared_bps
        .saturating_div(8)
        .saturating_mul(STARTUP_READY_SECONDS);
    if by_time == 0 {
        STARTUP_READY_BYTES
    } else {
        by_time.min(STARTUP_READY_BYTES)
    }
}

/// Contiguous bytes the player can read from `start` without waiting on the
/// network.
///
/// [`PlaybackControl::prefetch_cache_probe`] returns the bytes still held at the
/// start offset and the bytes buffered ahead of the proxy's read cursor. The
/// player consumes the head of the stream within a few hundred milliseconds, so
/// after that only the second number is meaningful; the larger of the two is
/// the honest "how much can we read right now". `None` = no proxy.
fn cached_bytes_from(control: &PlaybackControl, start: u64) -> Option<u64> {
    let (at_start, read_ahead) = control.prefetch_cache_probe(start)?;
    Some(at_start.max(read_ahead))
}

/// Cache progress toward `need` bytes, thousandths (`0..=1000`).
fn cache_progress_milli(cached: u64, need: u64) -> u32 {
    let need = need.max(1);
    ((cached.min(need) * 1000) / need) as u32
}

/// Publish the Bytes stage of a running startup from the proxy cache ahead of
/// `start_byte`. No-op without a startup or without a proxy: the gate publishes
/// its own curve in that case.
fn publish_startup_bytes(
    control: &PlaybackControl,
    start_byte: u64,
    need_bytes: u64,
    elapsed_ms: u64,
) {
    if !control.startup_active() {
        return;
    }
    let Some(cached) = cached_bytes_from(control, start_byte) else {
        return;
    };
    control.set_startup_need_bytes(need_bytes);
    let measured = cached as f32 / need_bytes.max(1) as f32;
    // Same creep as the gate (see `PreRollGate::publish_stages`): no visible
    // freeze while the proxy's first chunk is still being written.
    let creep = elapsed_ms as f32 / STARTUP_BYTES_CREEP_MS as f32;
    control.publish_startup(StartupStage::Bytes, measured.max(creep.min(0.9)));
}

/// QA hook for the A/B run: `BILI_PRE_ROLL=ring` keeps the old ring pre-roll, so
/// the two gates can be compared on the same video and machine (see the
/// `startup ready` line for which one opened). Anything else = automatic.
fn ring_preroll_forced() -> bool {
    static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| {
        std::env::var("BILI_PRE_ROLL")
            .map(|value| value.trim().eq_ignore_ascii_case("ring"))
            .unwrap_or(false)
    })
}

/// Diagnostic hook `BILI_AUDIO_PAUSE_TEST=<at_ms>[,<hold_ms>]`: suspend the
/// sound in the middle of playback and release it `hold_ms` later (default 600),
/// through the same [`AudioGate`] path a buffer pause uses.
///
/// It is the deterministic form of the owner's t≈6 s dip: a network hiccup makes
/// the buffer guard suspend `ffplay`, and on release its first `-stats` report is
/// the audio **device** position (frozen content + the suspension). The clock
/// must drop that report instead of adopting it (see
/// [`AUDIO_CLOCK_RESUME_QUIET_MS`] and `PlaybackControl::set_audio_clock_ms`).
/// `at_ms` counts from the decode loop's start; fires once per decode loop.
fn audio_pause_test() -> Option<(u64, u64)> {
    let raw = std::env::var("BILI_AUDIO_PAUSE_TEST").ok()?;
    let mut parts = raw.split(',');
    let at_ms = parts.next()?.trim().parse::<u64>().ok()?;
    let hold_ms = parts
        .next()
        .and_then(|part| part.trim().parse::<u64>().ok())
        .unwrap_or(600)
        .clamp(100, 4_000);
    Some((at_ms, hold_ms))
}

/// One armed pre-roll gate. It lives on the pacer thread; the UI only sees the
/// atomics it publishes through [`PlaybackControl`].
struct PreRollGate {
    kind: GateKind,
    mode: GateMode,
    /// Ring occupancy that opens a `Ring` gate, frames.
    target: usize,
    /// Cache bytes that open a `Cache` gate.
    need_bytes: u64,
    /// Byte offset of this session's start position in the media.
    start_byte: u64,
    /// Last measured contiguous cache ahead of `start_byte`.
    cached_bytes: u64,
    /// How the gate opened: `cache`, `ring` or `timeout` (the `startup ready`
    /// line).
    via: &'static str,
    timeout: Duration,
    /// When the gate was armed (the session's first decode).
    armed_at: Instant,
    /// When the first decoded frame reached the ring. The ring gate's timeout
    /// runs from here, so a slow connect is not counted against the pre-roll.
    first_frame_at: Option<Instant>,
    /// `BILI_PREROLL_LOG=1` was set for this process.
    log: bool,
}

impl PreRollGate {
    /// Arm the gate and publish `buffering()` for the UI.
    ///
    /// The cache gate is used for the first session of a player that really
    /// reads through the proxy. A direct URL has no cache to measure, so the
    /// ring pre-roll stays; a recovery gate after a stall is about the ring
    /// refilling, so it keeps its shorter ring rule too.
    fn arm(
        control: &PlaybackControl,
        kind: GateKind,
        capacity: usize,
        start_ms: u64,
    ) -> Self {
        let cache = kind == GateKind::Initial
            && !ring_preroll_forced()
            && control.prefetch_stats().is_some();
        let declared_bps = control
            .play_stats()
            .map(|stats| stats.declared_bps)
            .unwrap_or(0);
        let need_bytes = if cache {
            startup_ready_bytes(declared_bps)
        } else {
            0
        };
        let gate = Self {
            kind,
            mode: if cache { GateMode::Cache } else { GateMode::Ring },
            target: kind.target_frames(capacity),
            need_bytes,
            start_byte: if cache {
                control.byte_offset_for_ms(start_ms).unwrap_or(0)
            } else {
                0
            },
            cached_bytes: 0,
            via: if cache { "cache" } else { "ring" },
            timeout: Duration::from_millis(if cache {
                STARTUP_READY_TIMEOUT_MS
            } else {
                kind.timeout_ms()
            }),
            armed_at: Instant::now(),
            first_frame_at: None,
            log: std::env::var("BILI_PREROLL_LOG")
                .map(|value| value == "1")
                .unwrap_or(false),
        };
        // The stage machine owns the overlay's bar; these targets are what its
        // Bytes and PreRoll stages measure against.
        control.set_startup_target(gate.target);
        control.set_startup_need_bytes(gate.need_bytes);
        if cache {
            control.begin_cache_buffering("preroll-cache", gate.need_bytes);
            crate::debug_log(&format!(
                "startup gate cache need_bytes={} start_byte={} declared_bps={declared_bps} timeout_ms={}",
                gate.need_bytes,
                gate.start_byte,
                STARTUP_READY_TIMEOUT_MS
            ));
        } else {
            control.begin_buffering("preroll-ring", gate.target);
        }
        if gate.log {
            crate::debug_log(&format!(
                "preroll start target={} ring={} t_ms=0 kind={} mode={}",
                gate.target,
                control.ring_len(),
                match gate.kind {
                    GateKind::Initial => "initial",
                    GateKind::Recovery => "recovery",
                },
                match gate.mode {
                    GateMode::Cache => "cache",
                    GateMode::Ring => "ring",
                }
            ));
        }
        gate
    }

    /// One poll. Nothing is popped either way, so the reader keeps filling the
    /// ring while the gate is closed.
    fn poll(&mut self, control: &PlaybackControl, source: &FrameSource) -> GateStep {
        match self.mode {
            GateMode::Cache => self.poll_cache(control, source),
            GateMode::Ring => self.poll_ring(control, source),
        }
    }

    /// Publish the Bytes and Decode stages of the startup bar from what this
    /// poll just measured. No-op unless a startup is running (a recovery gate
    /// after a stall uses the plain overlay).
    fn publish_stages(&self, control: &PlaybackControl) {
        if !control.startup_active() {
            return;
        }
        let bytes = if self.mode == GateMode::Cache && self.need_bytes > 0 {
            let measured = self.cached_bytes as f32 / self.need_bytes as f32;
            // The proxy writes in multi-megabyte chunks, so a byte fraction can
            // sit at 0 and then cross the whole target between two polls (the
            // old "0 or 100"). The creep only fills the otherwise frozen gap at
            // the very start; real bytes dominate as soon as they exist.
            let creep = self.armed_at.elapsed().as_millis() as f32 / STARTUP_BYTES_CREEP_MS as f32;
            measured.max(creep.min(0.9))
        } else {
            // No proxy: there is no cache to measure, so the stage shows the
            // byte stream opening (the decoder's connect) instead of a fake
            // percentage of a file that does not exist.
            (self.armed_at.elapsed().as_millis() as f32 / 400.0).min(0.95)
        };
        control.publish_startup(StartupStage::Bytes, bytes);
        let ring = control.ring_len();
        let decode = if ring > 0 {
            1.0
        } else {
            // Two measured sub-signals: frames this session already decoded, and
            // the decoder's own start-up age. The larger keeps the bar moving
            // while ffmpeg is still probing (0 frames, ~1.5 s).
            let by_frames = control.startup_decoded_since() as f32 / 6.0;
            let by_age =
                self.armed_at.elapsed().as_millis() as f32 / STARTUP_DECODE_EXPECT_MS as f32;
            by_frames.max(by_age).min(0.95)
        };
        control.publish_startup(StartupStage::Decode, decode);
    }

    /// Cache gate: open once the proxy holds enough bytes ahead of the start
    /// position, or the ceiling elapsed.
    ///
    /// Opening on the cache alone is safe **now**: the startup handover holds
    /// the first frame until the ring has [`PRE_ROLL_START_FRAMES`] and the audio
    /// clock is ready, so the pacer can no longer be released against an empty
    /// ring (the old `startup ready … ring=0` starvation chain).
    fn poll_cache(&mut self, control: &PlaybackControl, source: &FrameSource) -> GateStep {
        self.publish_stages(control);
        if source.is_done() {
            // End of stream or a read error: the cache can never grow, and the
            // pacer handles the end itself.
            self.via = "ring";
            return GateStep::Open;
        }
        if self.armed_at.elapsed() >= self.timeout {
            // Say so instead of hiding it: the picture starts now, possibly
            // still thin, and the buffer guard takes over from here.
            self.via = "timeout";
            return GateStep::Open;
        }
        let Some(cached) = cached_bytes_from(control, self.start_byte) else {
            // The proxy went away (unhealthy warmup, quality switch): there is
            // no cache to wait for any more. Fall back to today's ring rule
            // instead of holding the overlay on a signal that will never come.
            return self.fall_back_to_ring(control);
        };
        self.cached_bytes = cached;
        control.set_buffer_pct(cache_progress_milli(self.cached_bytes, self.need_bytes));
        let measured = self.cached_bytes as f32 / self.need_bytes.max(1) as f32;
        let creep = self.armed_at.elapsed().as_millis() as f32 / STARTUP_BYTES_CREEP_MS as f32;
        control.publish_startup(StartupStage::Bytes, measured.max(creep.min(0.9)));
        if self.cached_bytes >= self.need_bytes {
            self.via = "cache";
            return GateStep::Open;
        }
        GateStep::Wait
    }

    /// Ring gate: no proxy (direct CDN), or a post-stall recovery.
    ///
    /// A first session opens as soon as one frame is in the ring: the startup
    /// handover enforces the real pre-roll (frames and audio) from there, so the
    /// audio warm-up overlaps the ring fill instead of following it. A recovery
    /// gate still waits for the ring itself.
    fn poll_ring(&mut self, control: &PlaybackControl, source: &FrameSource) -> GateStep {
        let ring = control.ring_len();
        self.publish_stages(control);
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
        let need = match self.kind {
            GateKind::Initial => 1,
            GateKind::Recovery => self.target,
        };
        if ring >= need {
            self.via = "ring";
            return GateStep::Open;
        }
        if first.elapsed() >= self.timeout {
            self.via = "timeout";
            return GateStep::Open;
        }
        GateStep::Wait
    }

    /// Swap a cache gate for the ring rule after the proxy disappeared.
    fn fall_back_to_ring(&mut self, control: &PlaybackControl) -> GateStep {
        self.mode = GateMode::Ring;
        self.via = "ring";
        self.need_bytes = 0;
        self.timeout = Duration::from_millis(self.kind.timeout_ms());
        control.begin_buffering("preroll-fallback", self.target);
        crate::debug_log("startup gate fallback=ring reason=proxy-gone");
        GateStep::Wait
    }
}

/// Outcome of one startup-handover poll.
enum StartupStep {
    /// Keep holding the first frame.
    Wait,
    /// The handover is done: the pacer may present its first frame.
    Go,
    /// The decoder never produced a frame: reuse the startup-stall resync.
    Stall,
}

/// The startup handover after the byte gate opens: resume the (silent) audio at
/// the session's start position and hold the first frame until the sound is
/// ready **and** the ring holds [`PRE_ROLL_START_FRAMES`].
///
/// This is the fix for the owner's report: the picture used to start the moment
/// the bytes were cached, with a `ffplay` that had not reported a clock yet, so
/// the sound joined ~1 s late (and the seed logic then restarted it). Now the
/// sound is prepared first and the picture starts on the same content position:
///
/// * the audio process is spawned suspended with the decoder (its CDN/device
///   warm-up cannot run ahead of the held picture), and its WASAPI session is
///   muted before it is resumed, so the warm-up is never audible;
/// * `ffplay`'s first `-stats` sample reports the *content* position it starts
///   at, which is exactly the frame the pacer holds, so the two agree within
///   [`STARTUP_SYNC_TOLERANCE_MS`] by construction;
/// * the first frame releases the mute in the same instant (`release_audio_hold_now`),
///   so the picture and the sound begin together.
struct StartupHold {
    /// `true` until the handover opens (or is given up on).
    active: bool,
    /// How the byte gate opened (`cache`, `ring`, `timeout`), for the log.
    via: &'static str,
    /// Session start position, ms: picture and sound both start here.
    start_ms: u64,
    /// Ring frames the PreRoll stage waits for.
    target: usize,
    /// When the audio was resumed (the clock wait runs from here).
    audio_at: Option<Instant>,
    /// The session has no audio at all: the Audio stage completes at once.
    no_audio: bool,
    /// The audio clock is fresh and within tolerance of the held picture.
    audio_ready: bool,
    /// The one silent restart used when the clock landed ahead of the picture.
    audio_restarted: bool,
    /// The [`STARTUP_AUDIO_TIMEOUT_MS`] ceiling fired: the picture starts muted.
    audio_timed_out: bool,
    /// The audio session's mute has been confirmed: the warm-up is silent.
    audio_muted: bool,
    /// When the handover began (the byte gate opened).
    began: Instant,
}

impl StartupHold {
    /// The user paused while the startup was still holding the first frame: the
    /// wall time of the pause is not part of the startup's budget, so its
    /// ceilings and the audio-clock wait restart from here. A latch that was
    /// already reached stays reached (the sound is where the picture is).
    fn rebase(&mut self) {
        self.began = Instant::now();
        self.audio_at = None;
    }

    fn new(via: &'static str, start_ms: u64, target: usize, no_audio: bool) -> Self {
        Self {
            active: true,
            via,
            start_ms,
            target: target.max(1),
            audio_at: None,
            no_audio,
            audio_ready: false,
            audio_restarted: false,
            audio_timed_out: false,
            audio_muted: false,
            began: Instant::now(),
        }
    }

    /// Publish the Audio/PreRoll/Decode sub-progress and decide whether the
    /// first frame may be presented.
    ///
    /// Every branch publishes on each poll (20 ms), so the bar can never freeze
    /// while a stage is running: the Audio stage moves with its clock wait even
    /// before a sample exists, the PreRoll stage with the ring it is filling.
    fn poll(
        &mut self,
        control: &Arc<PlaybackControl>,
        source: &FrameSource,
        audio_child: &mut Option<AudioProc>,
        audio_gate: &mut AudioGate,
        info: &bilibili::PlayInfo,
        speed: f64,
    ) -> StartupStep {
        if !self.active {
            return StartupStep::Go;
        }
        let ring = control.ring_len();
        // The byte gate has opened: its stage is complete, whatever opened it.
        control.publish_startup(StartupStage::Bytes, 1.0);
        control.publish_startup(
            StartupStage::Decode,
            if ring > 0 {
                1.0
            } else {
                (self.began.elapsed().as_millis() as f32 / STARTUP_DECODE_EXPECT_MS as f32)
                    .min(0.95)
            },
        );
        // ---- Audio: resume the muted process and wait for its first clock ----
        if self.no_audio || self.audio_timed_out {
            control.publish_startup(StartupStage::Audio, 1.0);
        } else {
            if self.audio_at.is_none() {
                // Resume first: a process suspended since spawn has not created
                // its WASAPI session yet, so it cannot be muted before it runs.
                // The silence of the warm-up is guaranteed by the process' own
                // `audio_hold_silent` latch, which the pacer applies to the
                // session the moment it exists (and no sound can come out before
                // ffplay has filled its device buffer anyway).
                // `resume` reports whether it changed the state; a gate that was
                // never suspended (a failed spawn) is already runnable, so the
                // clock wait starts either way and the ceiling bounds it.
                let resumed = audio_gate.resume(control);
                self.audio_at = Some(Instant::now());
                crate::debug_log(&format!(
                    "startup stage=Audio audio_resume pid={} resumed={} ring={ring}",
                    control.audio_pid(),
                    u8::from(resumed)
                ));
            }
            if !self.audio_muted {
                let pid = control.audio_pid();
                let applied = if pid != 0 && crate::win::audio_session_mute(pid, true) {
                    true
                } else {
                    // Already silent through the hold latch (see
                    // `PlaybackControl::apply_audio_session`)?
                    crate::win::audio_session_get(pid).map_or(false, |(_, mute)| mute)
                };
                if applied {
                    self.audio_muted = true;
                    crate::debug_log(&format!(
                        "startup audio muted pid={pid} ring={ring}"
                    ));
                }
            }
            let clock = control.audio_clock();
            let lead = clock.map(|ms| ms as i64 - self.start_ms as i64);
            if !self.audio_ready {
                if matches!(lead, Some(delta) if delta.abs() <= STARTUP_SYNC_TOLERANCE_MS) {
                    // The first fresh sample sits on the held frame: latch it,
                    // the pair is locked. From here the clock is only read for
                    // the log; the picture may still be waiting for its ring.
                    self.audio_ready = true;
                    crate::debug_log(&format!(
                        "startup audio locked delta_ms={} waited_ms={}",
                        lead.unwrap_or(0),
                        self.audio_at.map_or(0, |at| at.elapsed().as_millis()),
                    ));
                } else if let Some(delta) = lead {
                    if delta > STARTUP_SYNC_TOLERANCE_MS {
                        // The sample landed ahead of the held picture (a seek's
                        // keyframe offset, or the device had already consumed a
                        // little). The picture may not skip content and the sound
                        // may not play ahead, so start the audio over - silently,
                        // so this correction is never heard.
                        if !self.audio_restarted {
                            self.audio_restarted = true;
                            crate::debug_log(&format!(
                                "startup audio ahead lead_ms={delta} restart_at_ms={} reason=startup",
                                self.start_ms
                            ));
                            restart_audio_holding_at(
                                control,
                                audio_child,
                                audio_gate,
                                info,
                                self.start_ms,
                                speed,
                                "startup",
                            );
                            self.audio_at = Some(Instant::now());
                        } else if delta <= AUDIO_SEED_MAX_MS {
                            // The second process reports a sample this far in: it
                            // is device latency, not a desync (the same bound the
                            // mid-session seed accepts). Latch it rather than
                            // restart in a loop; the freeze below stops it here.
                            self.audio_ready = true;
                            crate::debug_log(&format!(
                                "startup audio lead accept delta_ms={delta} restarts=1"
                            ));
                        }
                    }
                }
            }
            // The clock has reached the held frame but the ring is not full yet:
            // freeze the sound where it is instead of letting it run ahead of a
            // picture that cannot start. A suspend keeps the content position,
            // and the first presented frame releases it (the pacer's starvation
            // release runs in the same instant as the push).
            if self.audio_ready && ring < self.target && !audio_gate.is_paused() {
                if let Some(now_ms) = control.audio_clock() {
                    control.freeze_audio_clock_at(now_ms);
                }
                if audio_gate.pause(false) {
                    crate::debug_log(&format!(
                        "startup audio hold ring={ring} target={} pid={}",
                        self.target,
                        control.audio_pid()
                    ));
                }
            }
            // The Audio stage moves with the age of its own wait (the measured
            // spawn -> first clock latency, floored while a sample exists but
            // does not match the held picture yet).
            let waited = self
                .audio_at
                .unwrap_or(self.began)
                .elapsed()
                .as_millis() as u64;
            let sub = if self.audio_ready {
                1.0
            } else {
                let by_age = waited as f32 / STARTUP_AUDIO_EXPECT_MS as f32;
                by_age.max(if clock.is_some() { 0.9 } else { 0.0 }).min(0.97)
            };
            control.publish_startup(StartupStage::Audio, sub);
            // A dead audio child can never report: say so and start the picture
            // muted rather than waiting out the ceiling.
            let audio_gone =
                self.audio_at.is_some() && audio_child.as_mut().map_or(true, |proc| proc.exited());
            if !self.audio_ready && (waited >= STARTUP_AUDIO_TIMEOUT_MS || audio_gone) {
                self.audio_timed_out = true;
                control.publish_startup(StartupStage::Audio, 1.0);
                crate::debug_log(&format!(
                    "startup audio timeout waited_ms={waited} clock={} start_ms={} pid={} ring={ring} gone={}",
                    clock
                        .map(|ms| ms.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    self.start_ms,
                    control.audio_pid(),
                    u8::from(audio_gone),
                ));
            }
        }
        // ---- PreRoll: the ring toward the start target ----
        control.publish_startup(
            StartupStage::PreRoll,
            (ring as f32 / self.target as f32).min(1.0),
        );
        // ---- the decision ----
        if source.is_done() {
            self.active = false;
            crate::debug_log(&format!(
                "startup ready via={} source=ended ring={ring} target={}",
                self.via, self.target
            ));
            return StartupStep::Go;
        }
        let audio_done = self.no_audio || self.audio_ready || self.audio_timed_out;
        let preroll_ready = ring >= self.target;
        // Never present a picture while the sound of its own warm-up could still
        // be heard: the session mute has to be confirmed first (the 3 s audio
        // ceiling above is the only way past it).
        let silent = self.audio_muted || self.no_audio || self.audio_timed_out;
        // Let the bar reach the picture instead of finishing its last third in
        // the same tick: the slew is bounded (330 ms for the whole bar), and
        // every stage is already complete here, so this only delays the handover
        // by the time the visible bar needs to arrive.
        let bar_arrived = control.buffer_progress() >= 0.90;
        if preroll_ready && audio_done && silent && bar_arrived {
            self.active = false;
            // While the handover has the sound frozen, its clock must be read
            // un-extrapolated: a suspended process is exactly where it stopped,
            // and the wall clock would pretend it played through the hold.
            let clock = if audio_gate.is_paused() {
                control.audio_clock_base()
            } else {
                control.audio_clock()
            };
            crate::debug_log(&format!(
                "startup ready via={} ring={ring} target={} clock_ms={} start_ms={} delta_ms={} audio_ready={} timed_out={} waited_ms={} audio_paused={}",
                self.via,
                self.target,
                clock.map(|ms| ms.to_string()).unwrap_or_else(|| "-".to_string()),
                self.start_ms,
                clock
                    .map(|ms| ms as i64 - self.start_ms as i64)
                    .unwrap_or(0),
                u8::from(self.audio_ready),
                u8::from(self.audio_timed_out),
                self.began.elapsed().as_millis(),
                u8::from(audio_gate.is_paused()),
            ));
            return StartupStep::Go;
        }
        // Empty ring: the decoder itself is not producing. Only the stall
        // watchdog may give up on it; a ring fill with frames in it uses the
        // pre-roll ceiling instead, so a slow source still starts.
        if ring == 0 {
            if self.began.elapsed() >= Duration::from_millis(STALL_STARTUP_MS) {
                return StartupStep::Stall;
            }
            return StartupStep::Wait;
        }
        if audio_done && self.began.elapsed() >= Duration::from_millis(PRE_ROLL_TIMEOUT_MS) {
            // The ring is thin but not empty and the sound is ready: start with
            // what is there rather than hold the overlay on a slow source.
            self.active = false;
            crate::debug_log(&format!(
                "startup preroll timeout ring={ring} target={} waited_ms={}",
                self.target,
                self.began.elapsed().as_millis()
            ));
            return StartupStep::Go;
        }
        StartupStep::Wait
    }
}

/// Restart the audio child at `ms` **silently**, for the startup handover.
///
/// [`restart_audio_at`] would start an audible process, which is exactly what
/// must not happen while the picture is still held: the sound would play ahead
/// of a frame that does not exist yet. This keeps the "no sound before a
/// picture" invariant and hands the new process to the same mute-then-resume
/// path as a fresh session.
fn restart_audio_holding_at(
    control: &Arc<PlaybackControl>,
    audio_child: &mut Option<AudioProc>,
    audio_gate: &mut AudioGate,
    info: &bilibili::PlayInfo,
    ms: u64,
    speed: f64,
    reason: &str,
) {
    if let Some(proc) = audio_child.take() {
        proc.stop();
        control.set_audio_pid(0);
    }
    audio_gate.forget();
    let audio_url = control.audio_url_for(info);
    *audio_child = audio_url
        .as_deref()
        .and_then(|url| start_audio(url, ms, speed, true, control));
    if audio_child.is_some() {
        audio_gate.set_pid(control.audio_pid());
    }
    crate::debug_log(&format!(
        "startup audio restart at_ms={ms} silent=1 reason={reason} pid={}",
        control.audio_pid()
    ));
}

/// The session's first frame is on screen: release the sound it belongs to and
/// close the startup stage machine in the same instant.
///
/// This is the one place where "picture exists" and "sound may be heard" become
/// true together. The log line is the startup's A/V receipt: when the first
/// frame was presented, the audio clock's position at that instant, their delta
/// (the number the sync requirement is measured on) and whether the sound was
/// really audible rather than still held by the silent seed.
fn first_frame_handover(control: &PlaybackControl, video_ms: i64) {
    // A picture of the seek's target is on screen: the readout belongs to the
    // pacer again, and the playhead is where the picture really is.
    control.clear_position_hold();
    let age = control.startup_age_ms();
    let audible = control.release_audio_hold_now();
    let clock = control.audio_clock();
    if let Some(age) = age {
        let delta = clock
            .map(|ms| (ms as i64 - video_ms).to_string())
            .unwrap_or_else(|| "-".to_string());
        crate::debug_log(&format!(
            "startup first present t={age}ms video_ms={video_ms} audio_clock_ms={} delta_ms={delta} audible={} pid={} ring={} muted={}",
            clock
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| "-".to_string()),
            match audible {
                Some(true) => "1",
                Some(false) => "0",
                None => "none",
            },
            control.audio_pid(),
            control.ring_len(),
            u8::from(control.muted()),
        ));
        control.finish_startup("present");
    }
}

/// One `session first present` line for the 画质 switch that is waiting for it.
///
/// Called from both presentation paths (the interpolated slot loop and the
/// plain one-frame path) at the session's first presented frame, because which
/// path runs is a property of the moment (a window drag suspends interpolation).
/// A switch is only done when the new tier's picture is on screen;
/// `switch_after_ms` is measured from the menu pick, so it is the number the
/// user's wait is judged on, and `matched` proves the presented stream is the
/// one that was asked for.
fn log_session_first_present(control: &PlaybackControl, stream: Option<(i64, i64, u64)>) {
    let Some((want_qn, after_ms)) = control.take_quality_switch() else {
        return;
    };
    let (qn, codecid, bandwidth) = stream.unwrap_or((0, 0, 0));
    crate::debug_log(&format!(
        "session first present qn={qn} codecid={codecid} bandwidth={bandwidth} switch_qn={want_qn} switch_after_ms={after_ms} matched={}",
        u8::from(qn == want_qn)
    ));
}

/// How the A/V guard silences the audio child.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioGateMode {    Running,
    /// The process is suspended with `NtSuspendProcess`: its WASAPI stream
    /// underruns into silence and its clock stops.
    Suspended,
    /// The process could not be suspended, so its WASAPI session is muted.
    Muted,
    /// Neither worked; the guard leaves the process alone instead of retrying
    /// a failing syscall every frame.
    Failed,
}

/// Freezes and releases the audio child without restarting it.
///
/// `ffplay` has no IPC pause (`-nodisp` means there is no window to send `p`
/// to), so the process itself is suspended. That is the only pause that also
/// stops the audio clock; muting alone would let sound run silently ahead and
/// force a re-sync later. Muting is the fallback when the handle cannot be
/// opened.
struct AudioGate {
    pid: u32,
    mode: AudioGateMode,
    /// The pause came from the drift guard (release on the clock window) rather
    /// than from starvation (release with the next presented frame).
    drift: bool,
}

impl AudioGate {
    fn new() -> Self {
        Self {
            pid: 0,
            mode: AudioGateMode::Running,
            drift: false,
        }
    }

    fn set_pid(&mut self, pid: u32) {
        if self.pid != pid {
            self.pid = pid;
            self.mode = AudioGateMode::Running;
            self.drift = false;
        }
    }

    /// Drop all local state without touching the process (the caller is about
    /// to kill and respawn it).
    fn forget(&mut self) {
        self.pid = 0;
        self.mode = AudioGateMode::Running;
        self.drift = false;
    }

    fn is_paused(&self) -> bool {
        matches!(self.mode, AudioGateMode::Suspended | AudioGateMode::Muted)
    }

    fn paused_for_drift(&self) -> bool {
        self.is_paused() && self.drift
    }

    /// Whether the fallback path (a WASAPI mute) is in use, which never stops
    /// the clock and therefore always needs a restart on release.
    fn was_muted(&self) -> bool {
        self.mode == AudioGateMode::Muted
    }

    /// Freeze the audio. Returns whether this call changed the state.
    fn pause(&mut self, drift: bool) -> bool {
        if self.is_paused() || self.mode == AudioGateMode::Failed || self.pid == 0 {
            return false;
        }
        self.drift = drift;
        let suspended = suspend_process(self.pid, true);
        let muted = if suspended {
            false
        } else {
            crate::win::audio_session_mute(self.pid, true)
        };
        self.mode = if suspended {
            AudioGateMode::Suspended
        } else if muted {
            AudioGateMode::Muted
        } else {
            AudioGateMode::Failed
        };
        crate::debug_log(&format!(
            "av audio pid={} action=pause via={} drift={}",
            self.pid,
            if suspended {
                "suspend"
            } else if muted {
                "mute"
            } else {
                "none"
            },
            u8::from(drift),
        ));
        self.mode != AudioGateMode::Failed
    }

    /// Release the audio. Returns whether this call changed the state.
    fn resume(&mut self, control: &PlaybackControl) -> bool {
        let mode = self.mode;
        if !self.is_paused() {
            self.mode = AudioGateMode::Running;
            self.drift = false;
            return false;
        }
        let ok = match mode {
            AudioGateMode::Suspended => suspend_process(self.pid, false),
            AudioGateMode::Muted => crate::win::audio_session_mute(self.pid, control.muted()),
            _ => false,
        };
        self.mode = AudioGateMode::Running;
        self.drift = false;
        if ok {
            // The suspend stopped the process (and with it the content), so the
            // resume continues from the frozen position: re-anchor the clock's
            // extrapolation there instead of extending it across the pause. A
            // suspended process is also the case whose next `-stats` report is
            // the audio *device* position (see `AUDIO_CLOCK_RESUME_QUIET_MS`).
            control.rebase_audio_clock(mode == AudioGateMode::Suspended);
        }
        crate::debug_log(&format!(
            "av audio pid={} action=resume via={} ok={}",
            self.pid,
            match mode {
                AudioGateMode::Suspended => "suspend",
                AudioGateMode::Muted => "mute",
                _ => "none",
            },
            u8::from(ok),
        ));
        ok
    }
}

/// Stop the current audio child, if any, and spawn a fresh one at `ms`.
///
/// This is the exception, never the routine: a pause is a *suspend* and a
/// release is a *resume* (see [`AudioGate`]), and the drift guard only gets here
/// when the picture could not catch up inside [`AV_CATCHUP_MS`] or when the mute
/// fallback (whose clock never stopped) has to be retired. The new process
/// starts at the picture position, so it can only lag, never lead.
fn restart_audio_at(
    control: &Arc<PlaybackControl>,
    audio_child: &mut Option<AudioProc>,
    audio_gate: &mut AudioGate,
    info: &bilibili::PlayInfo,
    ms: u64,
    speed: f64,
    reason: &str,
) {
    if let Some(proc) = audio_child.take() {
        proc.stop();
        control.set_audio_pid(0);
    }
    audio_gate.forget();
    let audio_url = control.audio_url_for(info);
    *audio_child = audio_url
        .as_deref()
        .and_then(|url| start_audio(url, ms, speed, false, control));
    if audio_child.is_some() {
        audio_gate.set_pid(control.audio_pid());
    }
    crate::debug_log(&format!(
        "av audio restart at_ms={ms} reason={reason} pid={}",
        control.audio_pid()
    ));
}

/// Suspend or resume a child process. Returns `false` when the process is gone
/// or the call failed; the caller then falls back to muting the audio session.
fn suspend_process(pid: u32, suspended: bool) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32/NT calls; the handle is owned by this call and closed
    // below. A dead pid yields a null handle, handled explicitly.
    let handle = unsafe {
        OpenProcess(
            PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle == 0 {
        return false;
    }
    let mut code: u32 = 0;
    let alive = unsafe { GetExitCodeProcess(handle, &mut code) } != 0 && code == STILL_ACTIVE;
    let ok = if !alive {
        false
    } else if suspended {
        (unsafe { NtSuspendProcess(handle) }) == 0
    } else {
        (unsafe { NtResumeProcess(handle) }) == 0
    };
    unsafe { CloseHandle(handle) };
    ok
}

/// One `av guard` line, at most once per second for a repeated action. A state
/// change always logs, so a pause/resume pair is never hidden by the limit.
fn av_log(last_ms: &mut u64, last_action: &mut i8, lead_ms: i64, paused: bool, reason: &str) {
    let now = monotonic_ms();
    let action = paused as i8;
    if *last_action != action || now.saturating_sub(*last_ms) >= 1000 {
        *last_ms = now;
        *last_action = action;
        let action = if paused { "pause" } else { "resume" };
        crate::debug_log(&format!(
            "av guard lead_ms={lead_ms} action={action} reason={reason}"
        ));
    }
}

/// Freeze the sound with the picture.
///
/// The invariant the whole A/V relation rests on: **the audio process never
/// runs while the picture does not advance.** A buffering hold, an auto pause, a
/// seek or a stall all suspend it; the next presented frame releases it. Without
/// this the process kept playing from its own buffer and the lead grew by
/// seconds, and the guard's reaction (restarting the process) restarted the
/// session's A/V warm-up with it.
fn pause_audio(
    control: &PlaybackControl,
    gate: &mut AudioGate,
    drift: bool,
    lead_ms: i64,
    last_ms: &mut u64,
    last_action: &mut i8,
    reason: &str,
) {
    if gate.is_paused() {
        return;
    }
    // Freeze the clock at the instant of the suspend, not at the last `-stats`
    // sample (see `PlaybackControl::freeze_audio_clock_at`).
    if let Some(now_ms) = control.audio_clock() {
        control.freeze_audio_clock_at(now_ms);
    }
    if gate.pause(drift) {
        av_log(last_ms, last_action, lead_ms, true, reason);
    }
}

/// Release the sound in place (the session is not restarted).
fn resume_audio(
    control: &PlaybackControl,
    gate: &mut AudioGate,
    lead_ms: i64,
    last_ms: &mut u64,
    last_action: &mut i8,
    reason: &str,
) -> bool {
    if gate.resume(control) {
        av_log(last_ms, last_action, lead_ms, false, reason);
        true
    } else {
        false
    }
}

/// What the A/V guard wants done with this frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AvStep {
    /// Nothing: the relation is inside the window (or the picture has to
    /// correct on its own, which the pacer's delta logic already does).
    None,
    /// Suspend the audio process; the picture catches up while it is frozen.
    Pause,
    /// Release the suspended process in place (no restart).
    Resume,
    /// The picture could not catch up inside [`AV_CATCHUP_MS`]: restart the
    /// audio once, at the picture position.
    Restart,
    /// Two big leads in [`AV_THRASH_WINDOW`] after corrections: the correction
    /// does not hold. Log `av thrash` and never restart the audio again in this
    /// session; the picture takes the correction (skip / hold) instead.
    Thrash,
}

/// The drift guard's state: it corrects, it does not thrash.
///
/// The measured failure this replaces: above 1 s of lead the guard restarted
/// `ffplay` immediately, every resume of a >400 ms pause restarted it too, and
/// each restart cost ~1 s of A/V warm-up — 28 `ffplay` spawns and 13 decoder
/// spawns in 40 s, with the picture at half rate. A restart is now the *last*
/// resort, rate-limited, and two of them failing to hold is a hard stop.
struct AvGuard {
    /// When the current drift pause began (`None` while running).
    paused_at: Option<Instant>,
    /// Guard corrections, for the `av thrash` line.
    pauses: u32,
    restarts: u32,
    last_restart_at: Option<Instant>,
    /// Start of the current big-lead episode (crossing, not every sample).
    big_since: Option<Instant>,
    /// Start of the window the episodes below are counted in.
    window_at: Option<Instant>,
    episodes: u32,
    /// The hard invariant tripped: no more audio restarts this session.
    thrash: bool,
}

impl AvGuard {
    fn new() -> Self {
        Self {
            paused_at: None,
            pauses: 0,
            restarts: 0,
            last_restart_at: None,
            big_since: None,
            window_at: None,
            episodes: 0,
            thrash: false,
        }
    }

    /// Remember that the guard paused the sound, so the catch-up bound runs.
    fn note_paused(&mut self, now: Instant) {
        self.paused_at = Some(now);
        self.pauses = self.pauses.saturating_add(1);
    }

    /// One decision per presented frame. `lead_ms` is sound minus picture and
    /// `paused` is the audio gate's current state.
    fn step(&mut self, lead_ms: i64, paused: bool, now: Instant) -> AvStep {
        // Count big-lead *episodes*: one per crossing of the threshold, and only
        // after the guard has corrected at least once ("after any correction").
        if lead_ms.abs() > AV_THRASH_LEAD_MS {
            if self.big_since.is_none() {
                self.big_since = Some(now);
                if self.pauses + self.restarts > 0 {
                    let fresh_window = self
                        .window_at
                        .map_or(true, |at| now.duration_since(at) > AV_THRASH_WINDOW);
                    if fresh_window {
                        self.window_at = Some(now);
                        self.episodes = 0;
                    }
                    self.episodes = self.episodes.saturating_add(1);
                }
            }
        } else {
            self.big_since = None;
        }
        if self.episodes >= 2 && !self.thrash {
            self.thrash = true;
            return AvStep::Thrash;
        }
        if self.thrash {
            // Hard invariant: no restarts, no further drift pauses. The generic
            // delta logic holds (picture ahead) or skips (sound ahead) instead,
            // so the relation is corrected on the picture side.
            return if paused { AvStep::Resume } else { AvStep::None };
        }
        if paused {
            // The process is suspended, so its clock is frozen: the picture
            // closes the gap frame by frame.
            if lead_ms <= AV_RESUME_LEAD_MS {
                self.paused_at = None;
                return AvStep::Resume;
            }
            let waited = self
                .paused_at
                .map_or(0, |at| now.duration_since(at).as_millis() as u64);
            let cooldown_done = self
                .last_restart_at
                .map_or(true, |at| now.duration_since(at) >= AV_RESTART_COOLDOWN);
            if waited >= AV_CATCHUP_MS && cooldown_done {
                self.paused_at = None;
                self.restarts = self.restarts.saturating_add(1);
                self.last_restart_at = Some(now);
                return AvStep::Restart;
            }
            return AvStep::None;
        }
        if lead_ms > AV_LEAD_MS {
            return AvStep::Pause;
        }
        AvStep::None
    }
}

/// Decode-and-pace loop. Pausing neither kills the children nor ends the
/// session: the sound is suspended with the picture, the reader fills the ring
/// and blocks, and the resume continues the same session. Only a seek, a
/// quality/speed change or a decoder death restarts the decoder. Video frames
/// are paced against the audio clock parsed from ffplay's `-stats` line (the
/// wall clock is only a fallback while no audio clock is available); in 仅音频
/// mode no video process is spawned.
fn decode_loop(
    info: &bilibili::PlayInfo,
    cid: u64,
    window: &slint::Weak<PlayerWindow>,
    control: &Arc<PlaybackControl>,
    mailbox: &Arc<FrameMailbox>,
    blend_pool: &Arc<BlendPool>,
    last_resync: &mut Option<Instant>,
) -> Result<()> {
    ensure_high_res_timer();
    // 设置 → 预取 → 并发连接, read once per decode loop: a 画质 switch spawns
    // its own engine mid-session and must use the same policy as the open.
    let prefetch_connections = crate::settings::load().prefetch_connections as usize;
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
    // Consecutive sessions that ended on a decoder read failure without
    // presenting a frame. A stream that keeps breaking is reported instead of
    // being respawned forever; one that played and then broke is retried.
    let mut decode_retries: u32 = 0;
    // `BILI_AUDIO_PAUSE_TEST` state: the one-shot mid-playback suspend
    // (`Some` while it is holding the sound), whether it already ran, the decode
    // loop's start (its `at_ms` time base) and the parsed `(at_ms, hold_ms)`.
    let audio_pause_test = audio_pause_test();
    let audio_test_start = monotonic_ms();
    let mut audio_test_at: Option<Instant> = None;
    let mut audio_test_done = false;
    // The interpolation work buffer pool is the caller's now (see `run`), so a
    // seek does not re-allocate it.
    let mut interp_log = InterpLog::new();
    // The optical-flow engine (mode 3) survives restarts as well: a seek must not
    // pay for a D3D11 device and an OF session again. Its per-session state is
    // cleared where the mode is resolved (`reset`), and `ensure` re-creates the
    // session if the frame size changed.
    let mut ofa_engine = ofa::Ofa::new();
    // The engine has a live device and an OF session (set by the attach worker).
    let mut ofa_engine_open = false;
    // Sticky for this decode loop: mode 3 gave up, every later interval blends.
    // `None` means the mode may still attach; a user mode change clears it so a
    // later re-entry can retry.
    let mut ofa_disabled: Option<&'static str> = None;
    // The `ofa active frame=…` line is a per-decode-loop receipt, not per frame.
    let mut ofa_active_logged = false;
    // A seek shadow that came ready: the next iteration **adopts** it instead of
    // spawning (its ring, decoder and positioned sound already exist).
    let mut takeover: Option<SeekShadow> = None;
    // A target the pacer took for a shadow and could not keep (a fallback, or a
    // session end while it pre-rolled): the next session starts exactly here.
    let mut restart_at: Option<u64> = None;
    loop {
        if control.is_closed() {
            return Ok(());
        }
        // 选集 switch: this decode session belongs to the video being left. The
        // `run` loop re-arms on the new source (new playurl / tracks / danmaku),
        // so this one exits here — before a paused hold could keep it alive.
        if control.switch_pending() {
            return Ok(());
        }
        if control.is_paused() {
            std::thread::sleep(Duration::from_millis(40));
            continue;
        }

        let adopted = takeover.take();
        // A pending seek is the session's target, full stop: the position atomic
        // can be overwritten by the pacer of the session that is being retired,
        // so reading it here would race the seek and start at the old spot.
        let pending_seek = if adopted.is_some() {
            None
        } else {
            restart_at.take().or_else(|| control.take_pending_seek())
        };
        let mut seen_seek = control.seek_seq();
        let start_ms = adopted
            .as_ref()
            .map(|shadow| shadow.target_ms)
            .or(pending_seek)
            .unwrap_or_else(|| control.position_ms());
        if adopted.is_some() {
            crate::debug_log(&format!(
                "seek adopt target_ms={start_ms} release_ms={}",
                adopted
                    .as_ref()
                    .map(|shadow| shadow.release_at_ms)
                    .unwrap_or(0)
            ));
        }
        // A new session starts from a clean buffer state: any auto pause belonged
        // to the position (or the seek that superseded it) we are leaving.
        control.set_auto_paused(false);
        // A user seek consumed here restarted this session. The release froze
        // the old picture, so the only question left is **when** to show the
        // buffering overlay: the shadow path arms it from the pacer at
        // `SEEK_FREEZE_OVERLAY_MS`; a hard restart (no shadow, or a shadow that
        // failed) arms it here, measured from the release, never from this
        // session's start.
        //
        // A hard restart caused by a *fallback* consumes no `pending_seek` (the
        // pacer took it), so the frozen target is recovered from the session's
        // own start: while the freeze is on, `start_ms` is the position the
        // release asked for.
        let wait_target = pending_seek.or_else(|| {
            if adopted.is_none() && control.seek_freeze() {
                Some(start_ms)
            } else {
                None
            }
        });
        let mut seek_wait = wait_target.map(|target| {
            // One consumed seek = one decoder restart. A newer seek replaces the
            // pending target instead of queueing another session.
            crate::debug_log(&format!("seek start target={target} restart=1"));
            let map_before = control.prefetch_map_dump();
            let bytes_before = control
                .prefetch_raw_stats()
                .map(|stats| stats.bytes_fetched)
                .unwrap_or(0);
            // The release may already be older than the silent window (a
            // shadow that timed out, or a slow adopt), in which case the
            // overlay belongs on screen at the first poll.
            let requested_at_ms = control.seek_requested_at_ms();
            let waited = monotonic_ms().saturating_sub(requested_at_ms);
            let grace = SEEK_FREEZE_OVERLAY_MS.saturating_sub(waited);
            let mut wait = SeekWait {
                target,
                // A freeze overlay may already be up (the pacer's 300 ms
                // rule): it belongs to this seek, so the served line measures
                // and closes it instead of claiming `overlay_ms=0`.
                published: control.seek_buffering(),
                buffered: false,
                deadline: Instant::now() + Duration::from_millis(grace),
                requested_at_ms,
                map_before,
                bytes_before,
            };
            // The decision line: what the disk map says about this target, and
            // how much of the 300ms silent window is left.
            let cached = control.log_seek_cache(target);
            match control.seek_beyond_prefetch(target) {
                Some(true) => {
                    let stats = control.prefetch_raw_stats();
                    crate::debug_log(&format!(
                        "seek buffer pending target_ms={target} reason=unfetched cached=0 fetched={} total={} wait_ms={grace}",
                        stats.as_ref().map(|s| s.bytes_fetched).unwrap_or(0),
                        stats
                            .as_ref()
                            .and_then(|s| s.bytes_total)
                            .map(|total| total.to_string())
                            .unwrap_or_else(|| "-".to_string())
                    ));
                }
                Some(false) if cached => {
                    // On disk: the decoder opens the cache file and the new
                    // position's first frame is local. It still gets the same
                    // 300ms silent window - a frozen picture may not wait
                    // silently past it, even for a cached target.
                    wait.buffered = true;
                    crate::debug_log(&format!(
                        "seek buffer pending target_ms={target} reason=cached-on-disk cached=1 wait_ms={grace}"
                    ));
                }
                _ => crate::debug_log(&format!(
                    "seek buffer pending target_ms={target} reason=not-on-disk cached=0 wait_ms={grace}"
                )),
            }
            wait
        });
        let speed = control.speed();
        let hwaccel = control.hwaccel();
        let audio_only = control.audio_only();
        // A 画质 menu pick names one exact stream; anything else (a `qn`-only
        // caller, the codec preference, a new video) uses the automatic rule.
        // Shared with the seek shadow, so both open the same representation.
        let (track, exact) = session_track(control, info);
        let pick = if exact { "exact" } else { "auto" };
        // The stream this session really opens, for the per-session first-frame
        // receipt (and to prove the 画质 menu's marked row against it).
        let session_stream = track.map(|track| (track.id, track.codecid, track.bandwidth));
        let video_url = match track {
            Some(track) => {
                // The proxy is per **stream**, not per player. A 画质 switch
                // changes the bitstream, so the tier being left must hand over
                // to an engine of its own (new port, new cache file, old one
                // shut down) *before* this session opens anything — otherwise
                // the old tier's session keeps downloading its own file while
                // this one reads the CDN directly, which is the "speed but no
                // progress" a switched player shows.
                let bound = control.prefetch_source();
                if prefetch_switch_enabled() && bound.as_deref() != Some(track.url.as_str()) {
                    switch_prefetch(control, track, prefetch_connections);
                }
                control.media_url_for(&track.url)
            }
            None => return Err(anyhow!("未找到可用视频流")),
        };
        // A session whose bytes are already in the session cache file decodes
        // that **local file**: ffmpeg's `-ss` on a local file is a table lookup
        // instead of an HTTP round trip plus a demuxer byte-range search, which
        // is what makes a cached seek instant. The proxy stays for every other
        // case (and keeps filling the file in the background).
        let start_byte = control.byte_offset_for_ms(start_ms).unwrap_or(0);
        let local_cache = control.prefetch_cache_file(start_ms);
        let (video_url, local_play) = match local_cache {
            Some(path) => (path.display().to_string(), true),
            None => (video_url, false),
        };
        let via = if local_play {
            "cache-file"
        } else if video_url.starts_with("http://127.0.0.1:") {
            "prefetch"
        } else {
            "direct"
        };
        if local_play {
            crate::debug_log(&format!(
                "video cache-file path={video_url} start_ms={start_ms} start_byte={start_byte}"
            ));
        }

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
                if exact {
                    "exact"
                } else {
                    "highest-bandwidth"
                },
                bandwidths_at_qn(info, track.id),
            ));
        }

        // Milliseconds per decoded frame, used as this frame's presentation
        // timestamp (`frame_index * frame_ms`).
        let frame_ms = frame_ms_for(track);
        // Publish the declared stream parameters (codec, resolution, declared
        // fps/bitrate, qn) and clear the buffer readings of the old child.
        control.note_stream(track, frame_ms);

        // ---- startup stage machine -----------------------------------------
        // Armed *before* the decoder exists, so the loading bar is up on the
        // very first UI tick of the session and moves from 0 instead of
        // appearing at 100 the moment the cache crosses its threshold. Only the
        // first session of a player is staged: a seek has to feel immediate.
        //
        // The guard retires the machine if this session ends without presenting
        // a frame (seek, pause, decoder error), so a retired session can never
        // leave the overlay on screen.
        // An adopted shadow already has its picture and sound ready: it must not
        // re-enter the staged startup gate (that would hold the frame again).
        let pre_roll_kind = if audio_only || adopted.is_some() {
            None
        } else {
            gate_kind
        };
        let _startup_guard = pre_roll_kind
            .filter(|kind| *kind == GateKind::Initial)
            .map(|_| control.begin_startup("preroll"));

        // ---- decode at the picture the window shows (默认) -----------------
        // The source is capped at 1080p; on top of that the session asks for the
        // window's video band unless 原始分辨率 is on. The size is decided once
        // here, per session, so a resize never restarts the decoder.
        let source_size = track
            .map(|track| (track.width, track.height))
            .unwrap_or((0, 0));
        // A fresh player's first session can start before the UI's 60 Hz size
        // watch has published anything (`0x0`), and the frame size is decided
        // here, once per session. Measured consequence of not waiting: the third
        // window of a multi-open decoded **1920x1080** for a 1188x671 window
        // (`window=0x0`), and that session never presented a frame at all.
        //
        // So wait for the real size, and if the UI has still not published one,
        // decode for the default window instead of the full source: a soft
        // picture for a moment is recoverable, a 1080p decode per player is what
        // took the machine down.
        if !audio_only && control.display_size().0 == 0 {
            // The bar must not sit at 0 while the UI publishes its first window
            // size: probe the proxy cache on every pass, which is a single lock
            // and no network work.
            let need = startup_ready_bytes(
                control
                    .play_stats()
                    .map(|stats| stats.declared_bps)
                    .unwrap_or(0),
            );
            let wait_started = Instant::now();
            for _ in 0..400 {
                if control.display_size().0 != 0 {
                    break;
                }
                publish_startup_bytes(
                    control,
                    start_byte,
                    need,
                    wait_started.elapsed().as_millis() as u64,
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let (mut display_w, mut display_h, fullscreen) = control.display_size();
        if display_w == 0 || display_h == 0 {
            display_w = DEFAULT_DISPLAY_W;
            display_h = DEFAULT_DISPLAY_H;
            crate::debug_log(&format!(
                "video size default window=unknown out_max={display_w}x{display_h}"
            ));
        }
        let decode = target_frame_size(source_size.0, source_size.1);
        // What the ring and the UI carry: the window's picture band (默认) or
        // the source size when 原始分辨率 is on.
        let out = reader_output_size(decode, (display_w, display_h), control.native_frame_size());
        // An adopted shadow's ring was produced at the size it was started with:
        // the session must keep exactly that size, whatever the window says now.
        let (out, capacity) = match adopted.as_ref() {
            Some(shadow) => (shadow.size, shadow.capacity),
            None => (out, ring_capacity_for(out.bytes())),
        };
        let frame_bytes = out.bytes();
        control.set_frame_size(out);
        crate::debug_log(&format!(
            "video target w={} h={} out={}x{} reason={spawn_reason} capacity={capacity} source={}x{} window={}x{} fullscreen={fullscreen} frame_bytes={frame_bytes} budget_mb={}",
            decode.w,
            decode.h,
            out.w,
            out.h,
            source_size.0,
            source_size.1,
            display_w,
            display_h,
            RING_BUDGET_BYTES >> 20,
        ));
        // Only the first session of this decode loop is a `spawn`; every later
        // one names what caused it (a plain restart = seek/pause/quality/speed/
        // volume). Window changes are deliberately not a cause any more.
        spawn_reason = "restart";

        // Pre-roll is a picture gate: 仅音频 has no frames to hold back.
        let pre_roll = if audio_only || adopted.is_some() {
            None
        } else {
            gate_kind
        };

        // The previous session's decoder duration belongs to the previous
        // position/cid: the API value is in force until this process reports
        // its own `Duration:` (which happens before its first frame).
        control.clear_decoder_duration();
        // A session is starting, so any queued play request has been served.
        control.clear_resume_request();

        // The profiler measures spawn→first-frame latency from here.
        let spawn_ms = monotonic_ms();
        // Session resources: either a fresh spawn or the seek shadow that
        // pre-rolled exactly this target (see [`SeekShadow`]).
        let mut video_child;
        let stderr_reader;
        let mut source;
        let mut audio_child: Option<AudioProc>;
        let mut audio_gate = AudioGate::new();
        let has_audio: bool;
        let mut audio_start_attempted: bool;
        // The swap receipt, logged at the first frame the adopted session
        // presents (the instant the user's target becomes visible).
        let mut swap_trace: Option<SwapTrace> = None;
        let audio_url = control.audio_url_for(info);
        // Snapshot the audio-restart trigger *before* this session starts its
        // audio process: a request that lands during the spawn is then still
        // seen by the loop below instead of being swallowed.
        let mut seen_audio = control.audio_seq();

        if let Some(mut shadow) = adopted {
            // ---- adopt: the swap is a move, not a restart ---------------------
            // The ring already holds the target's pre-roll and the sound is
            // positioned on it (muted, frozen). Nothing is waited for here: the
            // first frame this session pops is the shadow's first frame.
            swap_trace = Some(shadow.trace());
            let audio_pid = shadow.audio.as_ref().map(|audio| audio.pid).unwrap_or(0);
            let audio_suspended = shadow.audio.as_ref().map_or(false, |audio| audio.suspended);
            let audio_clock = shadow.audio.as_ref().map(|audio| audio.clock.clone());
            let audio_frozen_ms = shadow.audio_frozen_ms;
            video_child = shadow.video_child.take();
            stderr_reader = shadow.stderr.take();
            source = shadow.source.take();
            if let Some(source) = source.as_ref() {
                source.adopt();
            }
            audio_child = shadow
                .audio
                .take()
                .map(|audio| audio.proc);
            has_audio = audio_pid != 0 && audio_child.is_some();
            audio_start_attempted = true;
            control.decoder_live.store(true, Ordering::SeqCst);
            if has_audio {
                // The process is positioned at the target; the player owns it
                // from here, and it stays silent until the first frame of the
                // target is on screen (`first_frame_handover`).
                control.set_audio_pid(audio_pid);
                control.audio_hold_silent.store(true, Ordering::SeqCst);
                audio_gate.set_pid(audio_pid);
                if let Some(clock) = audio_clock {
                    clock.attach(control, audio_frozen_ms);
                }
                if audio_suspended {
                    suspend_process(audio_pid, false);
                }
                crate::debug_log(&format!(
                    "seek adopt audio pid={audio_pid} at_ms={audio_frozen_ms} target_ms={start_ms} resumed={}",
                    u8::from(audio_suspended)
                ));
            } else {
                control.set_audio_pid(0);
                control.audio_hold_silent.store(false, Ordering::SeqCst);
            }
        } else {
            video_child = if audio_only {
                None
            } else {
                Some(
                    spawn_video(
                        &video_url,
                        start_ms,
                        hwaccel,
                        out,
                        local_play,
                        gpu_frame_scale(hwaccel, local_play, out, decode),
                    )
                    .map_err(|err| anyhow!("无法启动解码器：{err}"))?,
                )
            };
            // Sound never starts before a picture exists — but the process starts
            // *with* the decoder, silent (`-volume 0`, held muted until the first
            // frame is presented).
            //
            // This is the startup A/V fix. `ffplay` needs ~1 s before it reports a
            // playing position (measured: `audio clock first after_ms=1012`). When
            // it was spawned after the first frame, the picture was already ~1 s
            // ahead of the sound by the time the clock appeared, and the pacer had
            // to choose between skipping that content (the measured 735 ms lost,
            // fps 2/12/17/15) and freezing. Starting it with the decoder means its
            // clock is warm when the first frame lands, so the two streams begin
            // within a frame or two of each other and nothing is skipped.
            // The audio process is started below, right after the video reader
            // thread is running: `spawn ffplay` costs tens of milliseconds and must
            // never delay the first video frame.
            audio_child = None;

            stderr_reader = video_child
                .as_mut()
                .and_then(|child| child.stderr.take())
                .map(|pipe| {
                    let reader_control = control.clone();
                    // A `cache-file` session reads a byte range that starts at the
                    // seek point, so its `Duration:` is the fragment's, not the
                    // video's: only a session that starts at 0 or reads the real
                    // stream may publish a duration.
                    let accept_duration = !local_play || start_ms == 0;
                    std::thread::spawn(move || {
                        note_thread_role("stderr");
                        read_video_stderr(pipe, reader_control, accept_duration)
                    })
                });
            control.decoder_live.store(true, Ordering::SeqCst);

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
            // Hand the pipe to a reader thread; the pacer drains the ring instead
            // of reading the pipe itself (so it can time out on a stalled source).
            // The reader converts (and, for a small window, downscales) into the
            // frame the UI will show, so nothing is copied after it leaves this
            // thread.
            source = stdout.map(|stdout| {
                FrameSource::spawn(stdout, control.clone(), spawn_ms, out, frame_ms, capacity)
            });
            // Now that the video reader owns the pipe, start the sound for this
            // session (silent until the first frame, see above).
            audio_child = audio_url
                .as_deref()
                .and_then(|url| start_audio(url, start_ms, speed, !audio_only, control));
            has_audio = audio_child.is_some();
            audio_start_attempted = has_audio;
            if has_audio {
                audio_gate.set_pid(control.audio_pid());
                // The sound is suspended until this session's first frame —
                // gated or not. `ffplay` reads the very stream this session
                // decodes, so it starts (and its clock runs) while the decoder
                // is still connecting and seeking: on a 画质 switch that lead was
                // measured at 1.2 s. The pacer then holds the picture against a
                // clock that is ahead of it and the display grid skips every
                // slot, so the session presents **nothing** — and because no
                // frame was presented, the A/V guard and the resync are both
                // disabled (`picture_live = pushes > 0`), which is the "switched
                // quality, loading forever, speed but no progress" report.
                // Suspended, its clock stands at the session's start position,
                // which is the held frame, and `first_frame_handover` releases
                // it in the same instant as the push.
                if audio_gate.pause(false) {
                    crate::debug_log(&format!(
                        "av audio pid={} action=pause via=suspend reason={}",
                        control.audio_pid(),
                        if pre_roll.is_some() { "preroll" } else { "session" }
                    ));
                }
            }
        }
        // From here on the session works in frames of the size the ring holds.
        let target = out;

        let mut frames: u64 = 0;
        let mut pushes: u64 = 0;
        // This session stopped because the decoder's pipe broke mid-track (a
        // read error away from the end), not because the video ended.
        let mut interrupted = false;
        // Until when the startup seed accepts a small audio-clock deficit (see
        // [`AUDIO_SEED_MAX_MS`]). Armed by the first presented frame.
        let mut seed_until: Option<Instant> = None;
        // Post-restart A/V hold: set when the audio had to be restarted at the
        // picture position mid-session (drift/starve). The picture is held (not
        // advanced, not skipped) until the new process reports its clock, so
        // both streams land on the same content position. The *startup* wait is
        // a different, staged one: see `StartupHold`.
        let mut hold_for_audio_at: Option<Instant> = None;
        let mut seed_logged = false;
        // The one-shot clock seed has been handled (accepted or restarted).
        let mut seed_done = false;
        let mut played = Duration::from_millis(start_ms);
        let mut last = Instant::now();
        let push_interval = push_interval_for(frame_ms);
        let mut last_push = last.checked_sub(push_interval).unwrap_or(last);
        let mut last_info = last;
        let mut seeking = false;
        let mut mode_changed = false;
        let mut finished = false;
        // `BILI_AV_TRACE`-style state: `true` while the user pause holds this
        // session open, so entering/leaving it is logged once.
        let mut pause_hold_logged = false;
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
        // ---- A/V guard and buffer sustain (session-local state) --------------
        // `true` while the buffer guard holds the picture. The session stays
        // alive (unlike a user pause): the reader keeps filling the ring and the
        // pacer resumes by itself once the lead recovers.
        let mut auto_paused = false;
        // Start of the current auto pause, for the bounded recovery.
        let mut auto_paused_at: Option<Instant> = None;
        // Highest ring lead seen during the current auto pause and when it was
        // reached: no new high-water for [`AUTO_PAUSE_STUCK_MS`] is a stall.
        let mut auto_best_lead: f64 = 0.0;
        let mut auto_best_at: Option<Instant> = None;
        // Since when a closed gate's own condition has been satisfied (the gate
        // watchdog force-opens it after [`GATE_FORCE_OPEN_MS`]).
        let mut gate_healthy_since: Option<Instant> = None;
        // Last auto pause/resume transition, and the thresholds for this
        // session's frame size (see `auto_buffer_thresholds`).
        let mut last_auto: Option<Instant> = None;
        // Fresh sessions start with an empty ring while the decoder primes; the
        // entry check waits this out (see AUTO_PAUSE_START_GRACE_MS). The grace
        // is re-armed from the **first presented frame** below: a session's
        // spawn→first-frame cost (ffmpeg probe + keyframe, measured at ~1.8 s
        // with the default probe) is much longer than this grace, and arming it
        // at spawn let the guard pause the picture on the very first frame —
        // the 735 ms of content lost in the first 3 s (fps 2/12/17/15 → 30).
        let mut auto_grace_until = Instant::now() + Duration::from_millis(AUTO_PAUSE_START_GRACE_MS);
        let (auto_pause_ms, auto_resume_ms) = auto_buffer_thresholds(frame_ms, capacity);
        let auto_resume_lead_frames = auto_resume_frames(auto_resume_ms, frame_ms, capacity);
        // Rate limiter for the `av guard` intervention lines.
        let mut last_av_log_ms = 0u64;
        let mut last_av_action: i8 = -1;
        // Drift guard: the sound follows the picture, corrections are pauses,
        // a restart is the last resort and the thrash invariant is final.
        let mut av = AvGuard::new();
        // ---- frame-blending interpolation (session-local state) --------------
        // The output rate for this session, resolved on first use. `None` while
        // the mode is off (so detection costs nothing) or when the display is
        // not faster than the track.
        let mut interp_plan: Option<InterpPlan> = None;
        let mut interp_plan_checked = false;
        // Display tick grid; reset whenever interpolation (re)starts.
        let mut interp_ticker: Option<InterpTicker> = None;
        // The rate the current grid steps at (`1000 / out_hz` of video time). A
        // mode switch or a drag pause can change the rate mid-session, and a
        // grid built for one step would keep pushing that step's ticks.
        let mut interp_ticker_hz: u32 = 0;
        // Block-matching working set (mode 2), reused for every pair. It is
        // allocated when the plan resolves and never per frame.
        let mut motion_interp = motion::MotionInterp::new();
        // `frame_{i-1}` of the pair the flow in `ofa_flow` belongs to. The warp
        // needs both frames of that pair on the CPU, and this one is no longer
        // owned by the pacer by the time the field is readable (see `ofa`).
        let mut ofa_prev: Option<Vec<u8>> = None;
        // `frame_{i-1}` of the pair submitted at the end of the last interval;
        // promoted into `ofa_prev` when that pair's field is read, so the two
        // can never drift apart when an interval skips its slots.
        let mut ofa_pending_prev: Option<Vec<u8>> = None;
        // Field of the pair submitted one interval ago: `(flow, grid_w, grid_h)`.
        let mut ofa_flow: Option<(Vec<i16>, u32, u32)> = None;
        // Mode 3's device/session is built on a helper thread after the first
        // frame; these two are that hand-off and its start time. Dropping the
        // receiver abandons the attempt (the worker releases its half-built
        // engine), which is exactly what the budget path does.
        let mut ofa_attach: Option<ofa::AttachHandle> = None;
        let mut ofa_attach_at: Option<Instant> = None;
        // The OF session for this frame size is open (mode 3).
        let mut ofa_ready = false;
        // Whether the next interval may pay for a pair (see [`OfaBudget`]).
        let mut ofa_budget = OfaBudget::new();
        // The pair budget of this session, wall ms; logged with the first pair.
        let mut ofa_budget_ms = 0.0f32;
        let mut ofa_budget_logged = false;
        // The clamp to blend above 720p is a per-session decision; log it once.
        let mut blockmatch_clamp_logged = false;
        // The first motion pair of a session logs its slot budget, so a live
        // log explains why the path did (or did not) keep running.
        let mut blockmatch_budget_logged = false;
        // Whether the current `interp drag pause=…` state was already logged.
        let mut interp_drag_paused = false;
        // The source frame after the one being presented, pulled from the ring
        // early so the blend has both neighbours. Never requested from the
        // decoder: it is already decoded and merely moves one frame ahead.
        let mut ahead: Option<FrameBuf> = None;
        // Source frames not presented because they fell behind the clock (the
        // profiler's dropped counter; display ticks have their own counter).
        let mut skipped: u64 = 0;
        // `BILI_PRESENT_TRACE=1`: where the first two seconds of this session's
        // frames went (see [`PresentTrace`]).
        let mut trace = PresentTrace::new();
        // `BILI_PROFILER_LOG=1`: one line per second that handed the UI fewer
        // than 28 frames, with the pacer's own account of where they went (see
        // [`DipMeter`]).
        let mut dip = DipMeter::new(frame_ms);
        // `BILI_PACE_TRACE=1`: one line per source frame with where its wall
        // time went (ring wait, sync sleep, blend, total). This is the
        // instrument the startup-stutter investigation needs: fps alone cannot
        // tell a slow decoder from a pacer that is holding.
        let pace_trace = std::env::var("BILI_PACE_TRACE")
            .map(|value| value == "1")
            .unwrap_or(false);
        // `BILI_PACER_BEAT=1`: one line per second from the pacer itself, so a
        // frozen picture can be attributed to the pacer (alive or dead) instead
        // of being guessed from the UI's log.
        let pace_beat = std::env::var("BILI_PACER_BEAT")
            .map(|value| value == "1")
            .unwrap_or(false);
        let mut last_beat = Instant::now();

        match source.as_ref() {
            Some(source) => {
                // Armed before the first frame: the pacer polls it at the top
                // of every iteration and pops nothing until it opens.
                let mut gate = pre_roll.map(|kind| {
                    PreRollGate::arm(control, kind, capacity, start_ms)
                });
                // The startup handover (Audio + PreRoll stages). One per first
                // session; it takes over from the byte gate and does not present
                // its first frame until the sound and the ring are both ready.
                let mut startup = pre_roll
                    .filter(|kind| *kind == GateKind::Initial)
                    .map(|_| {
                        StartupHold::new(
                            "gate",
                            start_ms,
                            PRE_ROLL_START_FRAMES.min(capacity),
                            !has_audio,
                        )
                    });
                // Trace bookkeeping: the frame-arrival instant of the previous
                // iteration, so the next one can report where the gap went.
                let mut last_mark: Option<Instant> = None;
                // A seek's pre-rolled session. At most one is in flight: a newer
                // release replaces it, so the last target wins (`§1`). The
                // release froze this session's picture, so nothing is presented
                // while the shadow pre-rolls.
                let mut shadow: Option<SeekShadow> = None;
                // f151: the `seek freeze pic=paused` line is a transition, not
                // a per-poll line.
                let mut seek_freeze_logged = false;
                loop {
                    tag_here("pacer:loop");
                    let loop_top = Instant::now();
                    dip.tick(control, loop_top, skipped, gate.is_some());
                    let tail_us = last_mark
                        .map(|mark| loop_top.duration_since(mark).as_micros())
                        .unwrap_or(0);
                    // Per-iteration clock hold, so the phases add up to `gap`.
                    let mut sleep_ms = 0u64;
                    if pace_beat && last_beat.elapsed() >= Duration::from_secs(1) {
                        last_beat = Instant::now();
                        crate::debug_log(&format!(
                            "pacer beat frames={frames} pushes={pushes} ring={} target={} auto={} buffering={} audio_paused={} paused={} gate={}",
                            control.ring_len(),
                            control.buffer_target.load(Ordering::Relaxed),
                            u8::from(auto_paused),
                            u8::from(control.buffering()),
                            u8::from(audio_gate.is_paused()),
                            u8::from(control.is_paused()),
                            u8::from(gate.is_some())
                        ));
                    }
                    if control.is_closed() {
                        break;
                    }
                    // Owed gain/mute after a fresh spawn: one cheap load per frame
                    // while nothing is pending, a bounded retry (100ms) until the
                    // new process' audio session exists.
                    control.sync_audio_session();
                    // ---- user pause: hold the picture, keep the session ----
                    // A pause must stop consuming, not end the session. The old
                    // behaviour killed both children and respawned them at the
                    // held position on resume: a black frame, a new ffmpeg probe
                    // (~1 s of startup) and a fresh `ffplay` warm-up for every
                    // press. The reader simply fills the ring and blocks, the
                    // sound is suspended with the picture, and the resume
                    // continues the same session — which requirement 2 demands
                    // (only a seek, quality/speed change or a decoder death may
                    // restart it).
                    //
                    // A hold can be released between two iterations of this
                    // loop (the occlusion watchdog runs on the UI thread), so
                    // the release bookkeeping cannot live only in the block's
                    // tail - it would leave `pause_hold_logged` latched and the
                    // next pause would miss its enter line.
                    if pause_hold_logged && !control.is_paused() && !control.occluded() {
                        pause_hold_logged = false;
                        last = Instant::now();
                        empty_since = None;
                        if let Some(hold) = startup.as_mut() {
                            hold.rebase();
                        }
                        auto_grace_until =
                            Instant::now() + Duration::from_millis(AUTO_PAUSE_START_GRACE_MS);
                        crate::debug_log(&format!(
                            "pause hold resume frames={frames} pushes={pushes} ring={} audio_paused={}",
                            control.ring_len(),
                            u8::from(audio_gate.is_paused())
                        ));
                    }
                    if control.is_paused() || control.occluded() {
                        // No frames are presented while paused; do not leave the
                        // panel showing the rate measured before the pause.
                        control.set_prof_rates(0.0, 0.0);
                        if pushes > 0 {
                            let held_ms = played.as_millis() as i64;
                            let lead = control
                                .audio_clock()
                                .map(|ms| ms as i64 - held_ms)
                                .unwrap_or(0);
                            pause_audio(
                                control,
                                &mut audio_gate,
                                false,
                                lead,
                                &mut last_av_log_ms,
                                &mut last_av_action,
                                if control.occluded() { "occluded" } else { "pause" },
                            );
                            control.note_av_state(lead, AV_HOLD);
                        }
                        if !pause_hold_logged {
                            pause_hold_logged = true;
                            crate::debug_log(&format!(
                                "pause hold enter frames={frames} pushes={pushes} ring={} audio_paused={} occluded={}",
                                control.ring_len(),
                                u8::from(audio_gate.is_paused()),
                                u8::from(control.occluded())
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                        profiler.tick(control, Instant::now());
                        if control.is_closed() {
                            break;
                        }
                        // A seek (or a mode change) during the pause is a real
                        // state change: it retires this session.
                        if control.seek_seq() != seen_seek || control.has_pending_seek() {
                            seeking = true;
                            break;
                        }
                        if control.audio_only() {
                            mode_changed = true;
                            break;
                        }
                        // A pause also retires a seek that is still pre-rolling:
                        // a paused player must not swap a picture in. The target
                        // is handed to the restart, exactly like a seek issued
                        // while paused.
                        if let Some(rest) = shadow.take() {
                            restart_at = Some(rest.target_ms);
                            rest.dispose("paused");
                            seeking = true;
                            break;
                        }
                        if control.is_paused() || control.occluded() {
                            continue;
                        }
                        pause_hold_logged = false;
                        // The pause's wall time is not playback time, and the
                        // guard's start grace belongs to a moving picture.
                        last = Instant::now();
                        empty_since = None;
                        // A startup that is still holding its first frame does
                        // not get to time out over a pause the user asked for.
                        if let Some(hold) = startup.as_mut() {
                            hold.rebase();
                        }
                        auto_grace_until =
                            Instant::now() + Duration::from_millis(AUTO_PAUSE_START_GRACE_MS);
                        crate::debug_log(&format!(
                            "pause hold resume frames={frames} pushes={pushes} ring={} audio_paused={}",
                            control.ring_len(),
                            u8::from(audio_gate.is_paused())
                        ));
                        continue;
                    }
                    // ---- seek: pre-roll it, keep this picture -----------------
                    // A seek used to break this session and pay a full `ffmpeg` +
                    // `ffplay` start (~1.2 s) with nothing on screen. Now a second
                    // session is started at the target (`SeekShadow`) while this
                    // one keeps presenting, and the swap happens only when that
                    // shadow has frames **and** a positioned, silenced sound. A
                    // newer release replaces the shadow in flight: one pre-roll at
                    // a time, the last target wins.
                    if control.seek_seq() != seen_seek || control.has_pending_seek() {
                        if let Some(target) = control.take_pending_seek() {
                            let seq = control.seek_seq();
                            let same_target =
                                shadow.as_ref().map_or(false, |in_flight| in_flight.target_ms == target);
                            if same_target {
                                // The same position again (a release after a drag
                                // seek): the pre-roll in flight already owns it,
                                // only the generation moves.
                                if let Some(in_flight) = shadow.as_mut() {
                                    in_flight.seq = seq;
                                }
                            } else {
                                if let Some(previous) = shadow.take() {
                                    previous.dispose("replaced");
                                }
                                // The picture is what makes a shadow worth it:
                                // with no frame of this session on screen there
                                // is nothing to keep, and the hard restart is
                                // already the shortest path.
                                if pushes > 0 && !audio_only {
                                    match SeekShadow::start(
                                        control, info, target, seq, hwaccel, speed,
                                    ) {
                                        Ok(started) => shadow = Some(started),
                                        Err(err) => {
                                            crate::debug_log(&format!(
                                                "seek shadow failed target_ms={target} err={err}"
                                            ));
                                            restart_at = Some(target);
                                        }
                                    }
                                } else {
                                    crate::debug_log(&format!(
                                        "seek shadow skipped target_ms={target} pushes={pushes} audio_only={}",
                                        u8::from(audio_only)
                                    ));
                                    restart_at = Some(target);
                                }
                            }
                            seen_seek = seq;
                            if shadow.is_none() {
                                seeking = true;
                                break;
                            }
                        } else {
                            // The seek generation moved without a target: a 画质
                            // or speed change. It restarts this session (the new
                            // stream or rate has to be opened); it is not a seek
                            // that must keep the old picture on screen.
                            seeking = true;
                            break;
                        }
                    }
                    // f151: the 300ms silent window is measured on every path,
                    // not only while the ring is empty: a restart can be held
                    // by the audio seed gate with a full ring, and a frozen
                    // picture must not wait silently past it.
                    if let Some(wait) = seek_wait.as_mut() {
                        arm_seek_overlay(control, wait);
                    }
                    if let Some(pre) = shadow.as_mut() {
                        match pre.poll(speed) {
                            SeekStep::Wait => {}
                            SeekStep::Ready => {
                                crate::debug_log(&format!(
                                    "seek shadow ready target_ms={} release_ms={} ring={}/{} audio_pid={} audio_ready={}",
                                    pre.target_ms,
                                    monotonic_ms().saturating_sub(pre.release_at_ms),
                                    pre.ring,
                                    pre.target_frames,
                                    pre.audio.as_ref().map(|audio| audio.pid).unwrap_or(0),
                                    u8::from(pre.audio_ready)
                                ));
                                takeover = shadow.take();
                                // The session is ending for a new one, exactly
                                // like the hard restart path: the outer loop must
                                // start the adopted session instead of
                                // interpreting this break as "playback ended".
                                seeking = true;
                                break;
                            }
                            SeekStep::Failed(reason) => {
                                let target = pre.target_ms;
                                crate::debug_log(&format!(
                                    "seek shadow fallback target_ms={target} reason={reason} elapsed_ms={}",
                                    monotonic_ms().saturating_sub(pre.release_at_ms)
                                ));
                                if let Some(rest) = shadow.take() {
                                    rest.dispose("fallback");
                                }
                                restart_at = Some(target);
                                seeking = true;
                                break;
                            }
                        }
                    }
                    // ---- f151: the release froze the picture ------------------
                    // A seek no longer keeps the old session playing while the
                    // shadow pre-rolls. The release stopped the mailbox from
                    // accepting this session's frames, and here the pacer stops
                    // producing them: no pop, no clock, no sound. The frame on
                    // screen at the release is the last one until the swap, so
                    // the picture cannot advance while the readout already
                    // shows the target.
                    //
                    // A `Ready` above has already left for the swap and a
                    // `Failed` for the hard restart; the only state left here is
                    // `Wait`. Its sound is suspended once, and after
                    // `SEEK_FREEZE_OVERLAY_MS` the wait becomes visible: the
                    // buffering overlay is published and stays until the new
                    // position's first frame closes it.
                    if control.seek_freeze() && shadow.is_some() {
                        if !seek_freeze_logged {
                            seek_freeze_logged = true;
                            let held_ms = played.as_millis() as i64;
                            let lead = control
                                .audio_clock()
                                .map(|ms| ms as i64 - held_ms)
                                .unwrap_or(0);
                            if pushes > 0 {
                                // The sound stops with the picture, in the same
                                // instant: a frozen frame over running audio
                                // would be a different lie.
                                pause_audio(
                                    control,
                                    &mut audio_gate,
                                    false,
                                    lead,
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    "seek",
                                );
                            }
                            control.set_prof_rates(0.0, 0.0);
                            crate::debug_log(&format!(
                                "seek freeze pic=paused frames={frames} pushes={pushes} presented_ms={} audio_clock_ms={} overlay={}",
                                control.presented_position_ms(),
                                control
                                    .audio_clock()
                                    .map(|ms| ms.to_string())
                                    .unwrap_or_else(|| "-".to_string()),
                                u8::from(control.seek_buffering()),
                            ));
                        }
                        let target = shadow.as_ref().map(|s| s.target_ms).unwrap_or(0);
                        let waited =
                            monotonic_ms().saturating_sub(control.seek_freeze_at_ms());
                        if !control.seek_buffering()
                            && waited >= SEEK_FREEZE_OVERLAY_MS
                            && !control.is_paused()
                        {
                            control.begin_seek_buffering(target);
                            crate::debug_log(&format!(
                                "seek freeze overlay=1 target_ms={target} waited_ms={waited} presented_ms={}",
                                control.presented_position_ms()
                            ));
                        }
                        profiler.tick(control, Instant::now());
                        std::thread::sleep(Duration::from_millis(SEEK_FREEZE_POLL_MS));
                        continue;
                    }
                    if control.audio_only() {
                        mode_changed = true;
                        break;
                    }
                    // ---- startup A/V gate --------------------------------
                    // A restarted `ffplay` needs ~1 s before it reports its first
                    // position. If the picture kept running, it would be that far
                    // ahead of the sound by the time we saw the clock, and the
                    // pacer would have to throw that content away (the measured
                    // 735 ms). Hold the picture instead: the reader keeps filling
                    // the ring, nothing is skipped, and both streams start at the
                    // same content position. Bounded by the seed window, so a
                    // broken audio URL can only cost that one window.
                    if let Some(since) = hold_for_audio_at {
                        if control.audio_clock().is_some()
                            || since.elapsed() >= AUDIO_SEED_WINDOW
                        {
                            hold_for_audio_at = None;
                            // The hold's wall time is not playback time.
                            last = Instant::now();
                            crate::debug_log(&format!(
                                "audio start gate open waited_ms={} frames={frames} ring={}",
                                since.elapsed().as_millis(),
                                control.ring_len()
                            ));
                        } else {
                            profiler.tick(control, Instant::now());
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                    }

                    // Audio restarts wait for the gate: starting ffplay during the
                    // pre-roll would let its clock run ahead of the held frame. They
                    // also wait for the first presented frame of the session: audio
                    // never exists before there is a picture. A restart requested
                    // while the buffer guard holds is deferred until it releases,
                    // so a volume change cannot un-pause the sound with no picture.
                    if gate.is_none() && control.audio_seq() != seen_audio {
                        seen_audio = control.audio_seq();
                        if let Some(proc) = audio_child.take() {
                            proc.stop();
                            // The pid is dead now: a gain write must not target it.
                            control.set_audio_pid(0);
                            audio_gate.set_pid(0);
                        }
                        audio_start_attempted = false;
                        if pushes > 0 && !auto_paused {
                            let resume_ms = control
                                .audio_clock()
                                .unwrap_or_else(|| played.as_millis() as u64);
                            let audio_url = control.audio_url_for(info);
                            audio_child = audio_url
                                .as_deref()
                                .and_then(|url| start_audio(url, resume_ms, speed, false, control));
                            audio_gate.set_pid(control.audio_pid());
                            audio_start_attempted = true;
                            crate::debug_log(&format!(
                                "audio restart index={} ms={resume_ms}",
                                control.audio_index()
                            ));
                        }
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
                                control.end_buffering("preroll-stall");
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
                                let waited_ms = armed.armed_at.elapsed().as_millis() as u64;
                                let progress = control.buffer_progress();
                                match armed.kind {
                                    // The measurement line for the A/B run:
                                    // open -> ready, and which signal opened it.
                                    GateKind::Initial => crate::debug_log(&format!(
                                        "startup ready bytes={} need={} waited_ms={waited_ms} via={} ring={ring} progress={progress:.2} presented={}",
                                        armed.cached_bytes,
                                        armed.need_bytes,
                                        armed.via,
                                        control.prof.presented.load(Ordering::Relaxed)
                                    )),
                                    GateKind::Recovery => {
                                        if armed.log {
                                            crate::debug_log(&format!(
                                                "preroll resume ring={ring} elapsed_ms={waited_ms} progress={progress:.2} target={} via={} presented={}",
                                                armed.target,
                                                armed.via,
                                                control.prof.presented.load(Ordering::Relaxed)
                                            ));
                                        }
                                    }
                                }
                                // The picture starts moving now - if the sound is
                                // ready too. The byte gate opening only hands
                                // over to the startup hold below (Audio +
                                // PreRoll); nothing is popped until that is done.
                                //
                                // The wall clock is *not* restarted here: it is
                                // reset when the first frame is popped, so the
                                // held position stays at the session's start.
                                if let Some(hold) = startup.as_mut() {
                                    hold.via = armed.via;
                                }
                                control.end_buffering("preroll-open");
                                gate = None;
                            }
                        }
                    }

                    // ---- startup handover: sound first, then the first frame ----
                    // The byte gate has opened; the picture is still held. The
                    // audio warms up silently and the ring fills, and the first
                    // frame is released only when both are ready (or the audio
                    // gives up after `STARTUP_AUDIO_TIMEOUT_MS`). Nothing is
                    // popped here, so the ring keeps filling and `played` stays
                    // at the session's start position.
                    if let Some(hold) = startup.as_mut() {
                        match hold.poll(
                            control,
                            source,
                            &mut audio_child,
                            &mut audio_gate,
                            info,
                            speed,
                        ) {
                            StartupStep::Wait => {
                                profiler.tick(control, Instant::now());
                                std::thread::sleep(Duration::from_millis(PRE_ROLL_POLL_MS));
                                continue;
                            }
                            StartupStep::Stall => {
                                control.note_stall(hold.began.elapsed().as_millis() as u64);
                                control.end_buffering("startup-stall");
                                startup = None;
                                stall_resync = true;
                                *last_resync = Some(Instant::now());
                                control.invalidate_audio_clock();
                                seeking = true;
                                break;
                            }
                            StartupStep::Go => {
                                startup = None;
                                last = Instant::now();
                            }
                        }
                    }

                    // ---- gate watchdog: a stuck overlay must be impossible ---
                    // Independent of whichever path armed the gate: once the
                    // gate's own condition has held for a second, close it. The
                    // pacer may be auto-paused (the picture is held) or busy; the
                    // overlay is a promise about the buffer, never a trap.
                    if let Some(satisfied) = gate_condition_met(control) {
                        let target = control.buffer_target.load(Ordering::Relaxed) as usize;
                        let need = control.buffer_need_bytes.load(Ordering::Relaxed);
                        if satisfied {
                            let since = *gate_healthy_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= Duration::from_millis(GATE_FORCE_OPEN_MS) {
                                crate::debug_log(&format!(
                                    "gate forced open src=watchdog ring={} target={target} need={need} held_ms={} auto={} audio_paused={}",
                                    control.ring_len(),
                                    since.elapsed().as_millis(),
                                    u8::from(auto_paused),
                                    u8::from(audio_gate.is_paused())
                                ));
                                control.end_buffering("watchdog");
                                if auto_paused {
                                    auto_paused = false;
                                    auto_paused_at = None;
                                    auto_best_at = None;
                                    control.set_auto_paused(false);
                                    last = Instant::now();
                                }
                                gate_healthy_since = None;
                            }
                        } else {
                            gate_healthy_since = None;
                        }
                    } else {
                        gate_healthy_since = None;
                    }

                    // ---- buffer sustain: auto pause -------------------------
                    // Decided before a frame is popped, so a held pacer leaves the
                    // whole ring free for the reader to refill. `frames > 0` keeps
                    // session start (where the ring is legitimately empty while the
                    // decoder primes) out of this decision; a seek overlay already
                    // owns the buffering state.
                    if auto_paused {
                        // One frame may be held in `ahead` for the interpolator; it
                        // is decoded video just as much as the ring's, so count it.
                        let lead =
                            (control.ring_len() + ahead.is_some() as usize) as f64 * frame_ms;
                        let reader_done = source.is_done();
                        // Both edges obey the 500 ms transition guard, so a lead
                        // that hovers around a threshold cannot flap the UI. A user
                        // release or the end of the stream is not a transition.
                        let due = last_auto.map_or(true, |t| {
                            t.elapsed() >= Duration::from_millis(AUTO_TRANSITION_MIN_MS)
                        });
                        let released = reader_done
                            || !control.auto_paused()
                            || (lead >= auto_resume_ms && due);
                        if released {
                            auto_paused = false;
                            auto_paused_at = None;
                            auto_best_lead = 0.0;
                            auto_best_at = None;
                            control.set_auto_paused(false);
                            control.end_buffering("auto-resume");
                            last_auto = Some(Instant::now());
                            // The hold's wall time is not playback time: restart the
                            // pacer's clock so the position does not jump forward by
                            // the buffering duration.
                            last = Instant::now();
                            crate::debug_log(&format!(
                                "auto resume lead_ms={:.0} target_ms={auto_resume_ms:.0}",
                                lead
                            ));
                        } else {
                            let waited =
                                auto_paused_at.map_or(0, |t| t.elapsed().as_millis() as u64);
                            // Track the ring's high-water mark: a lead that never
                            // improves is a stall even before the long timeout.
                            let now_auto = Instant::now();
                            if lead > auto_best_lead + 0.5 {
                                auto_best_lead = lead;
                                auto_best_at = Some(now_auto);
                            }
                            let no_progress = auto_best_at
                                .map_or(false, |at| {
                                    now_auto.duration_since(at)
                                        >= Duration::from_millis(AUTO_PAUSE_STUCK_MS)
                                });
                            // No picture while held: freeze the sound with it.
                            let audio_lead = control
                                .audio_clock_base()
                                .map(|ms| ms as i64 - played.as_millis() as i64)
                                .unwrap_or(0);
                            if pushes > 0 {
                                pause_audio(
                                    control,
                                    &mut audio_gate,
                                    false,
                                    audio_lead,
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    "buffer",
                                );
                            }
                            if waited >= AUTO_PAUSE_TIMEOUT_MS || no_progress {
                                // The ring never refilled (or stopped improving):
                                // this is a stall, not a transient dip. Reuse the
                                // watchdog's resync path.
                                crate::debug_log(&format!(
                                    "auto pause timeout lead_ms={lead:.0} best_ms={auto_best_lead:.0} waited_ms={waited} stuck={}",
                                    u8::from(no_progress)
                                ));
                                control.set_auto_paused(false);
                                control.end_buffering("auto-timeout");
                                control.set_position_ms(played.as_millis() as u64);
                                control.invalidate_audio_clock();
                                stall_resync = true;
                                seeking = true;
                                break;
                            }
                            profiler.tick(control, Instant::now());
                            std::thread::sleep(Duration::from_millis(PRE_ROLL_POLL_MS));
                            continue;
                        }
                    } else if frames > 0
                        && Instant::now() >= auto_grace_until
                        && !control.buffering()
                        && seek_wait.is_none()
                        && !source.is_done()
                    {
                        let lead =
                            (control.ring_len() + ahead.is_some() as usize) as f64 * frame_ms;
                        let due = last_auto.map_or(true, |t| {
                            t.elapsed() >= Duration::from_millis(AUTO_TRANSITION_MIN_MS)
                        });
                        if due && lead < auto_pause_ms {
                            auto_paused = true;
                            auto_paused_at = Some(Instant::now());
                            auto_best_lead = lead;
                            auto_best_at = Some(Instant::now());
                            last_auto = Some(Instant::now());
                            control.set_auto_paused(true);
                            control.begin_buffering("auto-pause", auto_resume_lead_frames);
                            let audio_lead = control
                                .audio_clock()
                                .map(|ms| ms as i64 - played.as_millis() as i64)
                                .unwrap_or(0);
                            if pushes > 0 {
                                pause_audio(
                                    control,
                                    &mut audio_gate,
                                    false,
                                    audio_lead,
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    "buffer",
                                );
                            }
                            crate::debug_log(&format!(
                                "auto pause lead_ms={lead:.0} target_ms={auto_resume_ms:.0}"
                            ));
                            profiler.tick(control, Instant::now());
                            continue;
                        }
                    }

                    // ---- `BILI_AUDIO_PAUSE_TEST` (diagnostic) -------------------
                    // The deterministic form of the owner's t≈6 s dip: suspend the
                    // sound in the middle of playback and release it `hold_ms`
                    // later, through the same `AudioGate` path a buffer hold uses
                    // (a suspend, then a release that rebases the clock). Nothing
                    // is popped while it runs, exactly like a buffer hold, so the
                    // picture and the content position stand still and the ring
                    // keeps filling.
                    //
                    // The release is the interesting half: `ffplay`'s first
                    // `-stats` report after a suspension is the audio **device**
                    // position (frozen content + the suspension), and the clock
                    // must drop it instead of adopting it — otherwise the sound is
                    // suddenly hundreds of ms "ahead", the A/V guard suspends it
                    // for drift and the picture crawls on a stopped clock.
                    if !audio_test_done {
                        if let Some((at_ms, hold_ms)) = audio_pause_test {
                            if frames > 0 && pushes > 0 && !control.buffering() {
                                match audio_test_at {
                                    None => {
                                        if monotonic_ms().saturating_sub(audio_test_start)
                                            >= at_ms
                                        {
                                            let audio_lead = control
                                                .audio_clock()
                                                .map(|ms| ms as i64 - played.as_millis() as i64)
                                                .unwrap_or(0);
                                            pause_audio(
                                                control,
                                                &mut audio_gate,
                                                false,
                                                audio_lead,
                                                &mut last_av_log_ms,
                                                &mut last_av_action,
                                                "test",
                                            );
                                            audio_test_at = Some(Instant::now());
                                            crate::debug_log(&format!(
                                                "audio test pause at_ms={at_ms} clock_ms={}",
                                                control
                                                    .audio_clock_base()
                                                    .map(|ms| ms as i64)
                                                    .unwrap_or(-1)
                                            ));
                                            profiler.tick(control, Instant::now());
                                            std::thread::sleep(Duration::from_millis(
                                                PRE_ROLL_POLL_MS,
                                            ));
                                            continue;
                                        }
                                    }
                                    Some(at) => {
                                        if (at.elapsed().as_millis() as u64) < hold_ms {
                                            profiler.tick(control, Instant::now());
                                            std::thread::sleep(Duration::from_millis(
                                                PRE_ROLL_POLL_MS,
                                            ));
                                            continue;
                                        }
                                        audio_test_at = None;
                                        audio_test_done = true;
                                        resume_audio(
                                            control,
                                            &mut audio_gate,
                                            0,
                                            &mut last_av_log_ms,
                                            &mut last_av_action,
                                            "test",
                                        );
                                        // The hold's wall time is not playback
                                        // time (the auto-resume path does the same).
                                        last = Instant::now();
                                        crate::debug_log(&format!(
                                            "audio test resume hold_ms={hold_ms}"
                                        ));
                                    }
                                }
                            }
                        }
                    }

                    // ---- interpolation: resolve the mode and the output grid ----
                    // One relaxed atomic load per source frame when off. The plan
                    // (the 60 fps output ceiling and the display rate) is resolved
                    // once per session, the first time the mode is actually on.
                    let interp_mode = InterpMode::from_u8(control.interp_mode());
                    let want_active = matches!(
                        interp_mode,
                        InterpMode::Blend | InterpMode::BlockMatch | InterpMode::Ofa
                    );
                    if want_active && !interp_plan_checked {
                        interp_plan_checked = true;
                        interp_plan = interp_plan_for(display_refresh_hz(), 1000.0 / frame_ms);
                        match interp_plan {
                        Some(plan) => {
                            if let Some(reason) = plan.reason {
                                crate::debug_log(&format!(
                                    "blend cap out_hz={} display_hz={} reason={reason}",
                                    plan.out_hz,
                                    display_refresh_hz()
                                ));
                            }
                            crate::debug_log(&format!(
                                "interp plan out_hz={} display_hz={} track_fps={:.3} frame={}x{} mode={}",
                                plan.out_hz,
                                display_refresh_hz(),
                                1000.0 / frame_ms,
                                target.w,
                                target.h,
                                interp_mode.as_u8()
                            ));
                        }
                        None => crate::debug_log(&format!(
                            "interp skip out_hz=0 display_hz={} track_fps={:.3} reason=display_not_above_track mode={}",
                            display_refresh_hz(),
                            1000.0 / frame_ms,
                            interp_mode.as_u8()
                        )),
                    }
                    }
                    // A self-driven window drag suspends generation (see
                    // `interp_paused_for_drag`): the plan and the published
                    // rate stay, the display grid is dropped, and the plain
                    // one-push-per-source-frame path runs instead. Audio, the
                    // decoder and the clock are untouched.
                    let interp_ready = want_active && interp_plan.is_some();
                    let drag_paused = interp_ready && interp_paused_for_drag();
                    let interp_active = interp_ready && !drag_paused;
                    // Mode 2 keeps the same display grid; only its mid-interval
                    // slot uses the motion path. Above the measured budget the
                    // mode degrades to exactly the blend it would otherwise be:
                    // say so once, then let every slot blend.
                    let motion_ok = interp_active
                        && interp_mode == InterpMode::BlockMatch
                        && interp_motion_supported(target);
                    if interp_active
                        && interp_mode == InterpMode::BlockMatch
                        && !motion_ok
                        && !blockmatch_clamp_logged
                    {
                        blockmatch_clamp_logged = true;
                        crate::debug_log(&format!(
                            "blockmatch unsupported frame={}x{} → blend",
                            target.w, target.h
                        ));
                    }
                    if motion_ok {
                        // Allocate the working set before the first pair, not
                        // inside its measured region.
                        motion_interp.reserve(target.w, target.h);
                    }
                    // Mode 3 (hardware optical flow) never gates startup: the
                    // first frame is always presented by the blend path, and the
                    // DLL load, D3D11 device and OF session are built on a
                    // helper thread afterwards. While that runs (and forever,
                    // if it fails or overruns its budget) every interval is a
                    // plain blend, so the picture keeps moving.
                    let ofa_mode = interp_active && interp_mode == InterpMode::Ofa;
                    if !ofa_mode {
                        // Leaving the mode abandons an in-flight attempt and
                        // clears the sticky failure so a later re-entry retries.
                        if let Some(handle) = ofa_attach.take() {
                            handle.cancel();
                        }
                        ofa_attach_at = None;
                        ofa_disabled = None;
                    }
                    let mut ofa_ok = ofa_mode && ofa_disabled.is_none() && ofa_ready;
                    if ofa_mode && ofa_disabled.is_none() && !ofa_ok {
                        if pushes == 0 {
                            // The first source frame is presented now; OFA must
                            // not do any work before it reaches the UI.
                        } else if !interp_ofa_size_ok(target) {
                            ofa_disabled = Some("frame_too_large");
                            announce_ofa_disabled(window, "frame_too_large", target);
                        } else if ofa_engine_open {
                            // A previous session of this decode loop already
                            // built the engine; only a frame-size change needs
                            // work here. It is measured against the same budget.
                            let started = Instant::now();
                            if ofa_engine.ensure(target.w, target.h) {
                                ofa_engine.reset();
                                ofa_ready = true;
                                ofa_ok = true;
                                if !ofa_active_logged {
                                    ofa_active_logged = true;
                                    crate::debug_log(&format!(
                                        "ofa active frame={}x{} ensure_ms={:.3}",
                                        target.w,
                                        target.h,
                                        started.elapsed().as_secs_f64() * 1000.0
                                    ));
                                }
                            } else {
                                let reason =
                                    ofa_engine.off_reason().unwrap_or("session_failed");
                                ofa_disabled = Some(reason);
                                ofa_engine_open = false;
                                announce_ofa_disabled(window, reason, target);
                            }
                            if ofa_disabled.is_none()
                                && started.elapsed()
                                    >= Duration::from_millis(OFA_ATTACH_BUDGET_MS)
                            {
                                ofa_disabled = Some("attach_timeout");
                                ofa_engine_open = false;
                                ofa_ok = false;
                                ofa_ready = false;
                                announce_ofa_disabled(window, "attach_timeout", target);
                            }
                        } else if let Some(rx) = ofa_attach.as_ref() {
                            match rx.try_recv() {
                                Ok(Ok(engine)) => {
                                    let attach_ms = ofa_attach_at
                                        .map(|at| at.elapsed().as_secs_f64() * 1000.0)
                                        .unwrap_or(0.0);
                                    ofa_attach = None;
                                    ofa_attach_at = None;
                                    ofa_engine = engine;
                                    ofa_engine_open = true;
                                    if ofa_engine.ensure(target.w, target.h) {
                                        ofa_engine.reset();
                                        ofa_ready = true;
                                        ofa_ok = true;
                                        if !ofa_active_logged {
                                            ofa_active_logged = true;
                                            crate::debug_log(&format!(
                                                "ofa active frame={}x{} attach_ms={attach_ms:.3}",
                                                target.w, target.h
                                            ));
                                        }
                                    } else {
                                        let reason = ofa_engine
                                            .off_reason()
                                            .unwrap_or("session_failed");
                                        ofa_disabled = Some(reason);
                                        ofa_engine_open = false;
                                        announce_ofa_disabled(window, reason, target);
                                    }
                                }
                                Ok(Err(reason)) => {
                                    if let Some(handle) = ofa_attach.take() {
                                        handle.cancel();
                                    }
                                    ofa_attach_at = None;
                                    ofa_disabled = Some(reason);
                                    ofa_engine_open = false;
                                    announce_ofa_disabled(window, reason, target);
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {
                                    let over_budget = ofa_attach_at.is_some_and(|at| {
                                        at.elapsed()
                                            >= Duration::from_millis(OFA_ATTACH_BUDGET_MS)
                                    });
                                    if over_budget {
                                        // Cancel and drop the handle: the worker
                                        // stops before touching D3D11 (so it
                                        // cannot cost the picture a frame) and
                                        // releases whatever it built.
                                        if let Some(handle) = ofa_attach.take() {
                                            handle.cancel();
                                        }
                                        ofa_attach_at = None;
                                        ofa_disabled = Some("attach_timeout");
                                        ofa_engine_open = false;
                                        announce_ofa_disabled(window, "attach_timeout", target);
                                    }
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    ofa_attach = None;
                                    ofa_attach_at = None;
                                    ofa_disabled = Some("attach_thread_gone");
                                    ofa_engine_open = false;
                                    announce_ofa_disabled(window, "attach_thread_gone", target);
                                }
                            }
                        } else {
                            ofa_attach = Some(ofa::Ofa::spawn_attach(target.w, target.h));
                            ofa_attach_at = Some(Instant::now());
                            crate::debug_log(&format!(
                                "ofa attach start frame={}x{} after_first_frame=1",
                                target.w, target.h
                            ));
                        }
                    }
                    if !ofa_ok {
                        // Leaving the mode (or never entering it) drops the
                        // stash: the next pair must not be warped with a field
                        // from another position, or another frame size.
                        if ofa_ready {
                            ofa_engine.reset();
                            ofa_ready = false;
                        }
                        ofa_prev = None;
                        ofa_pending_prev = None;
                        ofa_flow = None;
                        ofa_budget = OfaBudget::new();
                    }
                    // The rate is the plan's, not the (possibly suspended)
                    // generation state: while a drag is in flight the label
                    // keeps reading the same output rate and only the
                    // generated-frames counter stops moving.
                    let out_hz = if interp_ready {
                        interp_plan.map_or(0, |plan| plan.out_hz)
                    } else {
                        0
                    };
                    if !interp_active {
                        // Mode was switched off, or a window drag suspended
                        // generation: return to the plain
                        // one-push-per-source-frame path.
                        interp_ticker = None;
                        interp_ticker_hz = 0;
                    } else if interp_ticker.is_none() || interp_ticker_hz != out_hz {
                        // (Re)start the display grid at the frame about to be
                        // shown, at the rate just resolved. The rate can change
                        // mid-session (a pause/resume can pick up a different
                        // display rate), so a grid built for the old step is
                        // rebuilt rather than reused.
                        let video_ms = start_ms as f64 + frames as f64 * frame_ms;
                        interp_ticker = Some(InterpTicker::new(out_hz, speed, video_ms));
                        interp_ticker_hz = out_hz;
                    }
                    if control.interp_out_hz() != out_hz {
                        control.set_interp_out_hz(out_hz);
                    }
                    // One line per transition, so a drag trace reads
                    // "paused -> source rate -> resumed" without inference.
                    if drag_paused != interp_drag_paused {
                        interp_drag_paused = drag_paused;
                        crate::debug_log(&format!(
                            "interp drag pause={} out_hz={out_hz} frame={}x{} source=pacer",
                            u8::from(drag_paused),
                            target.w,
                            target.h
                        ));
                    }

                    tag_here("pacer:pop");
                    let pop_started = Instant::now();
                    dip.pre(pop_started.saturating_duration_since(loop_top).as_millis() as u64);
                    let frame = match ahead.take() {
                        Some(frame) => frame,
                        None => match source.next_frame(Duration::from_millis(
                            // A seek's pre-roll is polled at its own cadence: the
                            // swap must not wait for this ring's 100 ms stall poll.
                            // f151: the release's freeze also shortens it, so the
                            // pacer stops presenting a few ms after the release
                            // even when the shadow is still being opened.
                            if shadow.is_some() || control.seek_freeze() {
                                SEEK_FREEZE_POLL_MS
                            } else {
                                STALL_POLL_MS
                            },
                        )) {
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
                                // The video decoder reported the stream finished:
                                // this is the real end of the track (or a mid-file
                                // read error, which the position guard filters out).
                                // A read *failure* away from the end is an
                                // interruption: retry the session at the position
                                // the picture stopped at instead of declaring the
                                // video over (the false "播放完成" that used to
                                // leave the player dead).
                                let read_failed = source.failed();
                                let duration = control.duration_ms();
                                let at_end = duration == 0
                                    || control
                                        .playback_position_ms()
                                        .saturating_add(TRACK_END_FAILSAFE_MS)
                                        >= duration;
                                if read_failed && !at_end {
                                    interrupted = true;
                                    control.end_trace("read", false);
                                    crate::debug_log(&format!(
                                        "decode interrupted pos_ms={} dur_ms={duration} frames={frames} pushes={pushes}",
                                        control.playback_position_ms()
                                    ));
                                    break;
                                }
                                // The queue's 250ms tick watches this flag.
                                control.note_stream_finished();
                                finished = true;
                                break;
                            }
                            Err(()) => {
                                // Ring still empty after the poll. If it stays empty the
                                // source has silently stalled (TCP alive, no frames) and
                                // the pacing check below would never run - so detect it
                                // here and reuse the F29 resync.
                                trace.empty();
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
                                // The ring is empty, so no picture can be presented: no
                                // sound may keep running. Freeze the audio until the
                                // next presented frame releases it in the same instant
                                // as the push.
                                //
                                // Not before this session has shown a picture,
                                // though: at open the ring is legitimately empty for
                                // the first pop, and suspending `ffplay` there left the
                                // sound frozen behind a picture that had not started
                                // (measured `action=pause via=suspend` right after
                                // `gate open … ring=0`).
                                let audio_lead = control
                                    .audio_clock()
                                    .map(|ms| ms as i64 - played.as_millis() as i64)
                                    .unwrap_or(0);
                                if pushes > 0 {
                                    pause_audio(
                                        control,
                                        &mut audio_gate,
                                        false,
                                        audio_lead,
                                        &mut last_av_log_ms,
                                        &mut last_av_action,
                                        "starve",
                                    );
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
                        },
                    };

                    let now = Instant::now();
                    dip.pop(now.duration_since(pop_started).as_millis() as u64);
                    // Time spent waiting for the decoder's first frame (connect,
                    // seek, pre-roll) is not playback time: this session's clock
                    // starts at its first frame. Without this a slow seek pushed
                    // the wall position past the target and the first frames were
                    // dropped as "behind", so a seek near the end could latch the
                    // end without ever presenting its picture.
                    if frames == 0 {
                        last = now;
                        played = Duration::from_millis(start_ms);
                        // The buffer guard's grace runs from real playback, not
                        // from the spawn: otherwise a slow decoder start makes
                        // the guard pause the first frame (see above).
                        auto_grace_until = now + Duration::from_millis(AUTO_PAUSE_START_GRACE_MS);
                    } else {
                        played +=
                            Duration::from_secs_f64(now.duration_since(last).as_secs_f64() * speed);
                        last = now;
                    }
                    let mut wall_ms = played.as_millis() as u64;
                    control.set_position_ms(wall_ms);

                    // The interpolator needs the frame after this one. The decoder
                    // is ahead (the ring exists for exactly that), so pull it out
                    // now without waiting: it is the same decoded stream, merely one
                    // frame ahead of the presentation point, and no extra work is
                    // requested from ffmpeg. When the ring is empty the current
                    // frame is reused for the generated frames instead.
                    let next_frame = if interp_active {
                        source.try_next_frame()
                    } else {
                        None
                    };

                    // The decode target is source-based; a window resize or
                    // fullscreen toggle changes only how the same frame is scaled by
                    // the UI, so there is no restart path here any more.

                    // The live clock paces the picture. While the A/V guard holds
                    // the audio frozen that clock is deliberately stopped, so pace
                    // against the wall clock until the guard releases it.
                    let live_ms = control.audio_clock();
                    let frozen_ms = control.audio_clock_base();
                    let mut audio_ms = if audio_gate.is_paused() {
                        None
                    } else {
                        live_ms
                    };
                    // The audio clock is the master, but only inside a bounded
                    // window around the picture's own timeline. `wall_ms` is
                    // exactly playback time (it is accumulated from `Instant`
                    // deltas at the playback speed), so it says what 1.0× means
                    // for *this* session; a `-stats` position that runs slow, or
                    // a device that does, must never turn the frame rate down
                    // with it (measured: a 2-player window that settled at 29.0
                    // fps with `sync_ms` sleeping 46 ms a frame). Outside the
                    // window the picture keeps its nominal rate and the sound is
                    // left to the A/V guard, which owns the relation anyway.
                    let raw_sync = audio_ms.unwrap_or(wall_ms) as i64;
                    let sync_ms = raw_sync.clamp(
                        wall_ms as i64 - SYNC_CLOCK_WINDOW_MS,
                        wall_ms as i64 + SYNC_CLOCK_WINDOW_MS,
                    );
                    let video_ms = start_ms as i64 + (frames as f64 * frame_ms).round() as i64;
                    let mut delta = video_ms - sync_ms;
                    dip.delta(delta, audio_ms.is_some());
                    // ---- startup clock seed (one shot, first sample) ----------
                    // The first sample of a fresh `ffplay` arrives after its
                    // own CDN/device startup, so its content position can be up
                    // to ~1 s behind the picture that is already running. That
                    // lag used to be paid by HOLDING the picture (the ~1 s of
                    // content lost in the first two seconds). Two cases:
                    //  * a small lag (<= AUDIO_SEED_MAX_MS) is device latency:
                    //    accept it, present normally, never hold for it;
                    //  * a real lag is fixed at the source: restart `ffplay` at
                    //    the presented picture position. It starts warm from the
                    //    prefetch cache, and the seed window absorbs its own
                    //    first-sample latency, so the relation is exact after.
                    //
                    // The restart invalidates the clock (a new generation), so
                    // this frame must be paced by the *picture*, not by a clock
                    // that no longer exists: re-anchor the wall clock to the
                    // presented content and forget the stale delta. Without this
                    // the same frame slept `MAX_SYNC_SLEEP_MS` for a 945 ms lag
                    // and the next ~500 ms of frames were then dropped as
                    // "behind" the wall clock — the measured startup stutter.
                    if !seed_done {
                        if let Some(live_audio) = audio_ms {
                            seed_done = true;
                            if delta > AUDIO_SEED_MAX_MS {
                                crate::debug_log(&format!(
                                    "audio seed restart lag_ms={delta} at_ms={video_ms} audio_ms={live_audio}"
                                ));
                                restart_audio_at(
                                    control,
                                    &mut audio_child,
                                    &mut audio_gate,
                                    info,
                                    video_ms.max(0) as u64,
                                    speed,
                                    "startup",
                                );
                                audio_ms = None;
                                last = Instant::now();
                                played = Duration::from_millis(video_ms.max(0) as u64);
                                wall_ms = played.as_millis() as u64;
                                control.set_position_ms(wall_ms);
                                seed_until = Some(last + AUDIO_SEED_WINDOW);
                                // Hold the picture until the new process reports
                                // its position, so the restart latency is not
                                // charged to the content.
                                hold_for_audio_at = Some(Instant::now());
                                delta = 0;
                            } else if delta > SYNC_PACE_EPS_MS {
                                seed_logged = true;
                                crate::debug_log(&format!(
                                    "audio seed accept delta_ms={delta} window_ms={}",
                                    AUDIO_SEED_WINDOW.as_millis()
                                ));
                            }
                        }
                    }
                    // Lead of the audio clock over the presented picture. While the
                    // audio is frozen its base is used unchanged: the picture keeps
                    // moving and closes the gap.
                    let guard_ms = if audio_gate.is_paused() {
                        frozen_ms
                    } else {
                        live_ms
                    };
                    let lead_ms = guard_ms.map(|ms| ms as i64 - video_ms).unwrap_or(0);

                    // A fresh session has no meaningful A/V relation before its
                    // first frame: hold the guard back for the short start grace so
                    // an inherited clock cannot re-seek the position the user just
                    // picked.
                    let start_grace = frames == 0 && now < resync_grace_until;
                    // Until this session has put a picture on screen there is no
                    // A/V relation to guard at all: `ffplay` is already playing
                    // while the decoder is still priming, so its clock legitimately
                    // leads the (nonexistent) picture. Acting on that measured
                    // `av audio action=pause via=suspend drift` for a 4.5 s lead and
                    // then `resync behind delta_ms=-4566 frames=0` — a self-seek to
                    // 0 that killed and respawned the decoder before the first
                    // frame, which is the "player hangs on open" report.
                    let picture_live = pushes > 0;
                    // ---- A/V drift guard: correct, never thrash ----------------
                    // The sound may lead the picture by at most `AV_LEAD_MS`.
                    // Beyond it the process is suspended (its clock stops with
                    // it) and the picture closes the gap; it is released the
                    // moment it is no longer ahead. A restart is the last resort
                    // — only when the picture cannot catch up inside
                    // `AV_CATCHUP_MS`, at most once per `AV_RESTART_COOLDOWN` —
                    // and two big leads after a correction turn the restarts off
                    // for this session (`av thrash`): the picture takes the
                    // correction instead. The rate-limited `av guard` line
                    // records every intervention.
                    if !start_grace && picture_live {
                        match av.step(lead_ms, audio_gate.is_paused(), now) {
                            AvStep::Pause => {
                                av.note_paused(now);
                                pause_audio(
                                    control,
                                    &mut audio_gate,
                                    true,
                                    lead_ms,
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    "drift",
                                );
                            }
                            AvStep::Resume => {
                                if audio_gate.was_muted() {
                                    // Muting never stopped the clock, so a
                                    // resume would put the sound exactly where
                                    // it ran to: a fresh process at the picture.
                                    restart_audio_at(
                                        control,
                                        &mut audio_child,
                                        &mut audio_gate,
                                        info,
                                        video_ms.max(0) as u64,
                                        speed,
                                        "drift",
                                    );
                                    audio_ms = None;
                                    delta = 0;
                                    last = Instant::now();
                                    played = Duration::from_millis(video_ms.max(0) as u64);
                                    wall_ms = played.as_millis() as u64;
                                    control.set_position_ms(wall_ms);
                                } else {
                                    resume_audio(
                                        control,
                                        &mut audio_gate,
                                        lead_ms,
                                        &mut last_av_log_ms,
                                        &mut last_av_action,
                                        "drift",
                                    );
                                }
                            }
                            AvStep::Restart => {
                                crate::debug_log(&format!(
                                    "av restart lead_ms={lead_ms} at_ms={video_ms} pauses={} restarts={}",
                                    av.pauses, av.restarts
                                ));
                                restart_audio_at(
                                    control,
                                    &mut audio_child,
                                    &mut audio_gate,
                                    info,
                                    video_ms.max(0) as u64,
                                    speed,
                                    "drift",
                                );
                                // Hold the picture while the fresh process
                                // reports its position: its spawn latency is not
                                // charged to the content.
                                hold_for_audio_at = Some(Instant::now());
                                audio_ms = None;
                                delta = 0;
                                last = Instant::now();
                                played = Duration::from_millis(video_ms.max(0) as u64);
                                wall_ms = played.as_millis() as u64;
                                control.set_position_ms(wall_ms);
                            }
                            AvStep::Thrash => {
                                crate::debug_log(&format!(
                                    "av thrash lead_ms={lead_ms} episodes={} window_ms={} pauses={} restarts={} action=picture",
                                    av.episodes,
                                    AV_THRASH_WINDOW.as_millis(),
                                    av.pauses,
                                    av.restarts
                                ));
                                // From here on the picture corrects (hold when
                                // it leads, skip when it trails) and the audio
                                // process is left alone. A still-suspended sound
                                // is released so it is not stuck for the rest of
                                // the session.
                                if audio_gate.paused_for_drift() {
                                    resume_audio(
                                        control,
                                        &mut audio_gate,
                                        lead_ms,
                                        &mut last_av_log_ms,
                                        &mut last_av_action,
                                        "thrash",
                                    );
                                }
                            }
                            AvStep::None => {}
                        }
                    }
                    control.note_av_state(
                        lead_ms,
                        if audio_gate.is_paused() {
                            AV_HOLD
                        } else if guard_ms.is_none() {
                            AV_OFF
                        } else {
                            AV_RUN
                        },
                    );
                    // A frozen audio clock is not the pacer's master: presenting
                    // against the wall clock lets the picture catch up instead of
                    // dropping frames to chase a clock that was stopped on purpose.
                    let audio_frozen = audio_gate.is_paused();
                    let mut behind = false;
                    // A picture that leads the clock is *always* wrong, so this
                    // hold runs whatever the audio state is. `sync_ms` is the
                    // audio clock when it is usable and the wall clock otherwise
                    // (including while the gate has the sound frozen).
                    //
                    // This is the startup/post-seek fix. The hold used to be
                    // skipped entirely while the audio was frozen: the starve
                    // path freezes the sound the moment the ring runs empty, and
                    // the pacer then presented the whole pre-rolled ring as a
                    // burst (measured: 40 frames in <1 s). That ran the picture
                    // ~0.7 s ahead of the clock, which then triggered the audio
                    // seed restart, which held the picture for another ~0.85 s
                    // and dropped 38 display ticks as late — the low frame rate
                    // the owner sees right after open/seek.
                    if delta > SYNC_PACE_EPS_MS {
                        // Startup window: a small deficit is device latency
                        // or the warm-up of the restarted process. Never
                        // hold the picture for it; anything larger is a real
                        // desync and keeps the original behaviour, as does
                        // every frame after the window.
                        let seeding = !audio_frozen && seed_until.is_some_and(|until| now < until);
                        if seeding && delta <= AUDIO_SEED_MAX_MS {
                            if !seed_logged {
                                seed_logged = true;
                                crate::debug_log(&format!(
                                    "audio seed accept delta_ms={delta} window_ms={}",
                                    AUDIO_SEED_WINDOW.as_millis()
                                ));
                            }
                        } else {
                            // A seek shadow is pre-rolling (or a release just
                            // froze the picture): this session is about to be
                            // replaced, so an A/V hold must not keep the swap
                            // from being polled for a whole sleep. Cap the hold
                            // to the shadow's poll cadence so the
                            // release→visible latency is not charged to the old
                            // picture's sync (the new session's guard owns the
                            // relation after the swap).
                            let seek_pending = control.has_pending_seek()
                                || control.seek_freeze();
                            let wait = (delta as u64)
                                .min(MAX_SYNC_SLEEP_MS)
                                .min(if shadow.is_some() || seek_pending {
                                    SEEK_SHADOW_SYNC_HOLD_MS
                                } else {
                                    MAX_SYNC_SLEEP_MS
                                });
                            trace.sync(wait);
                            dip.sync(wait);
                            sleep_ms = wait;
                            if trace.enabled && wait >= 20 {
                                crate::debug_log(&format!(
                                    "pace hold wait_ms={wait} delta_ms={delta} video_ms={video_ms} sync_ms={sync_ms} audio={} frozen={} frames={frames} ring={}",
                                    audio_ms.map(|v| v as i64).unwrap_or(-1),
                                    u8::from(audio_frozen),
                                    control.ring_len()
                                ));
                            }
                            std::thread::sleep(Duration::from_millis(wait));
                        }
                    } else if !audio_frozen && picture_live {
                        if delta < -SYNC_RESYNC_MS
                            && !start_grace
                            && !av.thrash
                            && last_resync.map_or(true, |t| t.elapsed() > RESYNC_COOLDOWN)
                        {
                            crate::debug_log(&format!(
                            "resync behind delta_ms={delta} audio_clock_ms={} frames={frames} pushes={pushes}",
                            audio_ms.map(|v| v as i64).unwrap_or(-1)
                        ));
                            *last_resync = Some(Instant::now());
                            control.set_position_ms(sync_ms.max(0) as u64);
                            control.invalidate_audio_clock();
                            seeking = true;
                            break;
                        } else if delta < -SYNC_BEHIND_MS && !start_grace {
                            behind = true;
                            if trace.enabled {
                                crate::debug_log(&format!(
                                    "pace drop delta_ms={delta} video_ms={video_ms} sync_ms={sync_ms} audio={} frames={frames} skipped={skipped}",
                                    audio_ms.map(|v| v as i64).unwrap_or(-1)
                                ));
                            }
                        }
                    }

                    let gate_now = Instant::now();
                    // Everything between the frame being in hand and the clock
                    // hold being released (position bookkeeping, the look-ahead
                    // pull, the A/V guard, the interpolation plan) — the part
                    // of the iteration that is neither the decoder wait nor the
                    // deliberate sleep. A dip whose time is here is the pacer
                    // itself, not the clock and not the decoder.
                    dip.book(
                        gate_now
                            .saturating_duration_since(now)
                            .as_millis()
                            .saturating_sub(sleep_ms as u128)
                            .min(2000) as u64,
                    );
                    let interp_started = Instant::now();
                    // ---- mode 3: the field of the pair submitted last interval ----
                    // Read unconditionally while the mode runs, so the pipeline
                    // stays aligned even when this interval ends up dropping its
                    // slots: a field is always consumed together with the pair it
                    // was computed from.
                    let mut ofa_pair_ms: Option<f32> = None;
                    let mut ofa_flow_ms = 0.0f32;
                    // Whether this interval pays for a pair at all (submit +
                    // readback + warp). The gate is consulted once per interval;
                    // the submit at its end obeys the same answer.
                    let mut ofa_interval = false;
                    if ofa_ok {
                        // A pair costs roughly a third of the interval it runs
                        // in; the rest belongs to the blend/push of its slots.
                        if !ofa_budget_logged {
                            ofa_budget_logged = true;
                            ofa_budget_ms = (frame_ms * OFA_BUDGET_SHARE as f64) as f32;
                            crate::debug_log(&format!(
                                "ofa pair budget_ms={ofa_budget_ms:.3} frame_ms={frame_ms:.3} frame={}x{}",
                                target.w, target.h
                            ));
                        }
                        if ofa_budget.should_use(ofa_budget_ms) {
                            ofa_interval = true;
                            ofa_flow = ofa_engine.try_flow();
                        } else {
                            // Blend this interval: no readback, no upload, and
                            // the field in flight is dropped (it belongs to a
                            // pair whose slots were blended).
                            ofa_engine.reset();
                            ofa_flow = None;
                        }
                        ofa_prev = if ofa_flow.is_some() {
                            ofa_pending_prev.take()
                        } else {
                            None
                        };
                        if ofa_flow.is_some() {
                            ofa_flow_ms = ofa_engine.flow_ms();
                            ofa_pair_ms = Some(ofa_engine.last_ms() + ofa_flow_ms);
                        }
                    }
                    if interp_active {
                        // ---- display-rate presentation -------------------------
                        // Push display frames on the tick grid. The grid is a
                        // function of the audio clock (see `wait_for_interp_tick`),
                        // not of a fixed sleep, so a clock correction moves the
                        // schedule with it instead of accumulating drift.
                        if behind {
                            // This source frame is already behind the clock: drop
                            // it as a whole, exactly like the plain path. Its
                            // display slots are lost with it.
                            skipped = skipped.saturating_add(1);
                            dip.skip_behind(1);
                            control.note_interp_dropped(1);
                            trace.drop_source(1);
                        } else if let Some(ticker) = interp_ticker.as_mut() {
                            let video_ms_f = video_ms as f64;
                            let tick_end = video_ms_f + frame_ms;
                            let use_audio = !audio_frozen;
                            let wall_at_frame_ms = wall_ms as f64;
                            // Ticks the clock passed while the decoder was held:
                            // skip, never replay as a burst.
                            let live =
                                interp_clock_ms(control, use_audio, wall_at_frame_ms, now, speed);
                            let late = ticker.skip_late(live);
                            if late > 0 {
                                control.note_interp_dropped(late);
                                trace.drop_ticks(late as u64);
                            }
                            // The clock is entirely past this source interval:
                            // every slot of it would be skipped, so this frame
                            // could not be presented at all. Nothing else would
                            // rescue the session — with no frame presented
                            // (`picture_live = pushes > 0`) the A/V guard, the
                            // behind-drop and the resync are all disabled — so a
                            // clock that leads the picture livelocks it. Re-anchor
                            // the grid on the frame in hand: a late picture that
                            // is shown is what lets the guard correct the
                            // relation, and the next interval starts on the grid.
                            if ticker.next_ms >= tick_end {
                                *ticker = InterpTicker::new(out_hz.max(1), speed, video_ms_f);
                            }
                            let mut first_tick = true;
                            // One motion pair per source interval, in the display
                            // slot closest to the interval's midpoint: that is the
                            // picture a linear blend would ghost the most.
                            let mid_ms = video_ms_f + frame_ms * 0.5;
                            let mut motion_slot_done = false;
                            // A pair must fit one display slot in wall time. The
                            // ticker's step is video time, so divide the playback
                            // speed back out before comparing.
                            let wall_slot_ms =
                                (ticker.step_ms / speed.max(1.0e-3)) as f32;
                            let pair_budget_ms = (wall_slot_ms * 0.9).clamp(5.0, 30.0);
                            while ticker.next_ms < tick_end {
                                let live = interp_clock_ms(
                                    control,
                                    use_audio,
                                    wall_at_frame_ms,
                                    now,
                                    speed,
                                );
                                if ticker.next_ms + ticker.step_ms * 0.5 < live {
                                    // Late by more than half a display period:
                                    // skip this slot instead of pushing late.
                                    ticker.next_ms += ticker.step_ms;
                                    control.note_interp_dropped(1);
                                    trace.drop_ticks(1);
                                    continue;
                                }
                                let tick_wait = wait_for_interp_tick(
                                    control,
                                    use_audio,
                                    wall_at_frame_ms,
                                    now,
                                    speed,
                                    ticker.next_ms,
                                );
                                dip.interp(tick_wait);
                                // Phase from the audio clock: the blend matches the
                                // sound position, not the wall clock.
                                let live = interp_clock_ms(
                                    control,
                                    use_audio,
                                    wall_at_frame_ms,
                                    now,
                                    speed,
                                );
                                let phase = ((live - video_ms_f) / frame_ms).clamp(0.0, 0.999_999);
                                let weight = (phase * 256.0).round() as u16;
                                let other: &[u8] = next_frame
                                    .as_ref()
                                    .map_or_else(|| frame.as_bytes(), |frame| frame.as_bytes());
                                // Mode 3 draws every slot of this interval from the
                                // *previous* pair: its field is only readable one
                                // interval later (see `ofa`), and mixing pairs inside
                                // one interval would play the picture backwards for
                                // half a frame. Without a field, the current pair
                                // blends exactly like mode 1.
                                let ofa_pair = match (ofa_flow.as_ref(), ofa_prev.as_deref()) {
                                    (Some(_), Some(previous)) => Some((previous, frame.as_bytes())),
                                    _ => None,
                                };
                                let (slot_a, slot_b): (&[u8], &[u8]) = match ofa_pair {
                                    Some(pair) => pair,
                                    None => (frame.as_bytes(), other),
                                };
                                // The mid-interval slot is the only one that pays
                                // for motion; the first tick at or past the
                                // midpoint (minus half a step) is the closest one.
                                let motion_slot = motion_ok
                                    && !motion_slot_done
                                    && next_frame.is_some()
                                    && ticker.next_ms >= mid_ms - ticker.step_ms * 0.5;
                                // Every slot of an OFA interval is warped, except the
                                // ones at the ends of the pair: at phase 0 (or 1) the
                                // warp reads one frame only, which the blend already
                                // does for a fraction of the cost.
                                let warp_slot = ofa_pair.is_some()
                                    && (0.125..=0.875).contains(&phase);
                                let mut buffer = blend_pool.take(target);
                                let buffer_bytes = buffer.make_mut_bytes();
                                let pass_start = Instant::now();
                                let mut pair_ms = None;
                                let mut warped = false;
                                if warp_slot {
                                    if let Some((flow, gw, gh)) = ofa_flow.as_ref() {
                                        let warp_start = Instant::now();
                                        warped = ofa::warp(
                                            buffer_bytes,
                                            slot_a,
                                            slot_b,
                                            flow,
                                            *gw,
                                            *gh,
                                            target.w,
                                            target.h,
                                            phase as f32,
                                        );
                                        if warped {
                                            let ms = warp_start.elapsed().as_secs_f32() * 1000.0;
                                            ofa_pair_ms =
                                                Some(ofa_pair_ms.unwrap_or(0.0) + ms);
                                        }
                                    }
                                }
                                if !warped && motion_slot {
                                    // The decision is made once per interval even
                                    // when this pair is refused, so the budget
                                    // backoff cannot spill into a later slot.
                                    motion_slot_done = true;
                                    if !blockmatch_budget_logged {
                                        blockmatch_budget_logged = true;
                                        crate::debug_log(&format!(
                                            "blockmatch pair budget_ms={pair_budget_ms:.3} step_ms={:.3} speed={speed} frame={}x{}",
                                            ticker.step_ms, target.w, target.h
                                        ));
                                    }
                                    if motion_interp.should_use(pair_budget_ms)
                                        && motion_interp.interpolate(
                                            buffer_bytes,
                                            frame.as_bytes(),
                                            other,
                                            target.w,
                                            target.h,
                                            phase as f32,
                                            pair_budget_ms,
                                        )
                                    {
                                        pair_ms = Some(motion_interp.last_pair_ms());
                                    } else {
                                        // Unsupported input or no budget left:
                                        // this slot is an ordinary blend.
                                        blend_rgba_into(buffer_bytes, frame.as_bytes(), other, weight);
                                    }
                                } else if !warped {
                                    blend_rgba_into(buffer_bytes, slot_a, slot_b, weight);
                                }
                                let blend_us = pass_start.elapsed().as_micros() as u64;
                                control.note_interp_frame(blend_us);
                                if let Some(ms) = pair_ms {
                                    control.note_motion_pair(ms);
                                }
                                if first_tick {
                                    // Release a starvation pause in the same instant
                                    // the first display frame of this interval goes
                                    // out, so sound never trails the new picture.
                                    if audio_gate.is_paused() && !audio_gate.paused_for_drift() {
                                        let resume_lead = control
                                            .audio_clock_base()
                                            .map(|ms| ms as i64 - video_ms)
                                            .unwrap_or(0);
                                        // A pause is released in place; only the
                                        // mute fallback (whose clock never
                                        // stopped) needs a fresh process.
                                        if audio_gate.was_muted() {
                                            restart_audio_at(
                                                control,
                                                &mut audio_child,
                                                &mut audio_gate,
                                                info,
                                                video_ms.max(0) as u64,
                                                speed,
                                                "starve",
                                            );
                                            av_log(
                                                &mut last_av_log_ms,
                                                &mut last_av_action,
                                                resume_lead,
                                                false,
                                                "starve",
                                            );
                                        } else {
                                            resume_audio(
                                                control,
                                                &mut audio_gate,
                                                resume_lead,
                                                &mut last_av_log_ms,
                                                &mut last_av_action,
                                                "starve",
                                            );
                                        }
                                    }
                                }
                                pushes += 1;
                                if pushes == 1 {
                                    seed_until = Some(now + AUDIO_SEED_WINDOW);
                                    // A picture exists now, so the sound it
                                    // belongs to may be heard - in the same
                                    // instant, and the startup bar closes on it.
                                    first_frame_handover(control, video_ms);
                                    // The sound this session was holding (see the
                                    // spawn path) belongs to the picture on
                                    // screen now: release it without restarting
                                    // it, so it continues from the position it
                                    // was frozen at.
                                    if audio_gate.is_paused() && !audio_gate.paused_for_drift() {
                                        let lead = video_ms
                                            - control.audio_clock().unwrap_or(0) as i64;
                                        resume_audio(
                                            control,
                                            &mut audio_gate,
                                            lead,
                                            &mut last_av_log_ms,
                                            &mut last_av_action,
                                            "present",
                                        );
                                    }
                                    // The switch receipt: a 画质 change is only
                                    // done when the new tier's first frame is
                                    // on screen. `switch_after_ms` is measured
                                    // from the menu pick, not from this
                                    // session's spawn, so it is the number the
                                    // user's wait is judged on.
                                    log_session_first_present(control, session_stream);
                                    // The swap receipt: the shadow's first frame
                                    // *is* the frame the user asked for, and the
                                    // sound was unmuted with it.
                                    if let Some(swap) = swap_trace.take() {
                                        log_seek_swap(control, &swap, video_ms, pushes);
                                    }
                                }
                                control.note_frame_presented();
                                let push_at = Instant::now();
                                dip.admit(push_at.saturating_duration_since(now).as_millis() as u64);
                                dip.busy(
                                    push_at.saturating_duration_since(gate_now).as_millis() as u64,
                                );
                                trace.present(push_at);
                                // f151: this is the new position's first
                                // frame (or a frame of a session with no seek
                                // in flight): the picture may follow the
                                // decoder again. The gate must open *before*
                                // the push, because that is what the mailbox
                                // checks.
                                if pushes == 1 {
                                    control.end_picture_freeze();
                                    // 选集 switch receipt: this frame is the new
                                    // episode's first picture (no-op otherwise).
                                    control.note_switch_first_frame();
                                }
                                control
                                    .presented_ms
                                    .store(video_ms.max(0) as u64, Ordering::SeqCst);
                                mailbox.push(MailFrame {
                                    buffer,
                                    pooled: true,
                                });
                                if first_tick {
                                    first_tick = false;
                                    // The seek is served: the picture the user
                                    // picked is on screen (this first display frame
                                    // of the new position).
                                    if let Some(wait) = seek_wait.take() {
                                        log_seek_served(control, &wait, pushes);
                                    }
                                    // Sound never exists before the picture it
                                    // belongs to.
                                    if audio_child.is_none() && !audio_start_attempted {
                                        audio_start_attempted = true;
                                        let audio_start_ms = if frames == 0 {
                                            start_ms
                                        } else {
                                            video_ms.max(0) as u64
                                        };
                                        let start_url = control.audio_url_for(info);
                                        let requested_before = control.audio_seq();
                                        audio_child = start_url.as_deref().and_then(|url| {
                                            start_audio(url, audio_start_ms, speed, false, control)
                                        });
                                        audio_gate.set_pid(control.audio_pid());
                                        if control.audio_seq() == requested_before {
                                            seen_audio = requested_before;
                                        }
                                        crate::debug_log(&format!(
                                        "audio start after first frame start_ms={audio_start_ms}"
                                    ));
                                    }
                                }
                                ticker.next_ms += ticker.step_ms;
                            }
                            // ---- mode 3: submit this pair for the next interval ----
                            // After the last slot, so the upload (5.5 ms at 1080p)
                            // never delays a tick. The field it produces is read at
                            // the start of the next interval; `frame` becomes the
                            // pair's A by being handed to the screenshot slot below.
                            if ofa_ok && ofa_interval {
                                let mut submitted = false;
                                if let Some(next) = next_frame.as_ref() {
                                    submitted =
                                        ofa_engine.submit(frame.as_bytes(), next.as_bytes(), target.w, target.h);
                                    if submitted {
                                        control.note_ofa_submit();
                                        // The warp of that pair needs both frames on
                                        // the CPU next interval; the decoder's buffer
                                        // for A is handed to the UI, so keep a copy.
                                        ofa_pending_prev = Some(frame.as_bytes().to_vec());
                                    }
                                }
                                if !submitted {
                                    // No next frame (ring empty) or the engine
                                    // refused the pair: this interval's slots were
                                    // still warped from the old field, but nothing
                                    // is in flight now, so the next one blends, and
                                    // the GPU's copy of the shared frame is no
                                    // longer the pair's A.
                                    ofa_pending_prev = None;
                                    ofa_engine.reset();
                                }
                                if let Some(total) = ofa_pair_ms {
                                    control.note_ofa_pair(total, ofa_flow_ms);
                                    ofa_budget.record(total, ofa_budget_ms);
                                }
                                if let Some(reason) = ofa_engine.off_reason() {
                                    if ofa_disabled.is_none() {
                                        ofa_disabled = Some(reason);
                                        ofa_engine_open = false;
                                        announce_ofa_disabled(window, reason, target);
                                    }
                                    ofa_ready = false;
                                    ofa_prev = None;
                                    ofa_pending_prev = None;
                                    ofa_flow = None;
                                }
                            }
                            // 截图 sees the newest source picture. The buffer is
                            // reference counted, so keeping it costs one atomic
                            // increment here and no copy at all — the copy only
                            // happens when a screenshot is actually taken.
                            control.store_last_frame(&frame, target);
                            last_push = Instant::now();
                        }
                    } else if !behind {
                        // The burst gate is a **wait**, never a drop.
                        //
                        // A source frame that arrives sooner than
                        // `push_interval` after the previous push is the normal
                        // shape of a catch-up: the picture clock moved while
                        // this thread was not running (a scheduler hiccup, the
                        // once-a-second panel publish, a slow blend), so the
                        // frames that follow it are due back to back. Waiting
                        // `push_interval` keeps the UI from being handed a
                        // burst while still presenting every frame.
                        //
                        // This branch used to *discard* the frame instead, which
                        // turned one hiccup into a permanent loss of the frames
                        // in that second — the measured 27.1–27.9 dips with a
                        // full ring, `skipped` moving and the A/V delta never
                        // negative (a `gate_wait` in the dip line).
                        let since_push = gate_now.duration_since(last_push);
                        if since_push < push_interval {
                            dip.gate_wait();
                            std::thread::sleep(push_interval - since_push);
                        }
                        last_push = Instant::now();
                        // A frame is being presented now: release a starvation pause
                        // in the same instant, so sound never trails the new picture.
                        if audio_gate.is_paused() && !audio_gate.paused_for_drift() {
                            let resume_lead = control
                                .audio_clock_base()
                                .map(|ms| ms as i64 - video_ms)
                                .unwrap_or(0);
                            // A suspend is released in place: the pause did not
                            // cost the sound any content. Only the mute fallback
                            // (whose clock never stopped) needs a fresh process
                            // at the picture position.
                            if audio_gate.was_muted() {
                                restart_audio_at(
                                    control,
                                    &mut audio_child,
                                    &mut audio_gate,
                                    info,
                                    video_ms.max(0) as u64,
                                    speed,
                                    "starve",
                                );
                                av_log(
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    resume_lead,
                                    false,
                                    "starve",
                                );
                            } else {
                                resume_audio(
                                    control,
                                    &mut audio_gate,
                                    resume_lead,
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    "starve",
                                );
                            }
                        }
                        pushes += 1;
                        if pushes == 1 {
                            seed_until = Some(now + AUDIO_SEED_WINDOW);
                            // A picture exists now, so the sound it belongs to
                            // may be heard - in the same instant, and the
                            // startup bar closes on it.
                            first_frame_handover(control, video_ms);
                            // Release the sound this session was holding (see
                            // the display-rate path above): same instant as the
                            // picture, no restart.
                            if audio_gate.is_paused() && !audio_gate.paused_for_drift() {
                                let lead = video_ms - control.audio_clock().unwrap_or(0) as i64;
                                resume_audio(
                                    control,
                                    &mut audio_gate,
                                    lead,
                                    &mut last_av_log_ms,
                                    &mut last_av_action,
                                    "present",
                                );
                            }
                            // The switch receipt (see the display-rate path).
                            log_session_first_present(control, session_stream);
                            if let Some(swap) = swap_trace.take() {
                                log_seek_swap(control, &swap, video_ms, pushes);
                            }
                        }
                        control.note_frame_presented();
                        let push_at = Instant::now();
                        dip.admit(push_at.saturating_duration_since(now).as_millis() as u64);
                        dip.busy(push_at.saturating_duration_since(gate_now).as_millis() as u64);
                        trace.present(push_at);
                        // The frame is handed over as-is: the reader's own
                        // buffer is what the UI shows (no copy on either
                        // thread), and the mailbox coalesces it if the UI is
                        // still busy with an older picture.
                        //
                        // f151: a first frame of the new position also ends the
                        // release's freeze, immediately before the push the
                        // mailbox gates on.
                        if pushes == 1 {
                            control.end_picture_freeze();
                            // 选集 switch receipt (see the display-rate path).
                            control.note_switch_first_frame();
                        }
                        control
                            .presented_ms
                            .store(video_ms.max(0) as u64, Ordering::SeqCst);
                        mailbox.push(MailFrame {
                            buffer: frame,
                            pooled: false,
                        });
                        // The seek is served: the picture the user picked is on
                        // screen, so the overlay closes in this same frame.
                        if let Some(wait) = seek_wait.take() {
                            log_seek_served(control, &wait, pushes);
                        }
                        // The first frame of this session is on screen: start the
                        // audio now, at the same session position. Both streams of a
                        // seek therefore begin at the target, and sound never exists
                        // before the picture it belongs to.
                        if pushes > 0 && audio_child.is_none() && !audio_start_attempted {
                            audio_start_attempted = true;
                            // The session's first start is at its own position; a
                            // deferred start (a volume change while the buffer guard
                            // held) joins the picture where it is now.
                            let audio_start_ms = if frames == 0 {
                                start_ms
                            } else {
                                video_ms.max(0) as u64
                            };
                            let start_url = control.audio_url_for(info);
                            let requested_before = control.audio_seq();
                            audio_child = start_url
                                .as_deref()
                                .and_then(|url| start_audio(url, audio_start_ms, speed, false, control));
                            audio_gate.set_pid(control.audio_pid());
                            // Consume only requests made before this spawn; a bump
                            // landing while the process starts must still restart.
                            if control.audio_seq() == requested_before {
                                seen_audio = requested_before;
                            }
                            crate::debug_log(&format!(
                                "audio start after first frame start_ms={audio_start_ms}"
                            ));
                        }
                    } else {
                        // The picture is behind the clock by more than
                        // [`SYNC_BEHIND_MS`]: this source frame is content the
                        // picture cannot use in time. Dropping it is the only
                        // loss path in the plain player now (the burst gate
                        // waits instead), so `skip_behind` in the dip line
                        // names it.
                        skipped = skipped.saturating_add(1);
                        dip.skip_behind(1);
                        trace.drop_source(1);
                    }
                    // The next source frame (pulled ahead for blending) is the one
                    // the next iteration presents. When interpolation is off this
                    // is always `None`.
                    ahead = next_frame;
                    frames += 1;
                    if pace_trace {
                        crate::debug_log(&format!(
                            "pace frame={frames} wall_ms={wall_ms} video_ms={video_ms} delta_ms={delta} ring={} ahead={} skipped={skipped} pushes={pushes} top_us={} pop_us={} tail_us={tail_us} interp_us={} iter_ms={} audio={} interp={}",
                            control.ring_len(),
                            u8::from(ahead.is_some()),
                            pop_started.duration_since(loop_top).as_micros(),
                            pop_started.elapsed().as_micros(),
                            interp_started.elapsed().as_micros(),
                            now.elapsed().as_millis(),
                            audio_ms.map(|value| value as i64).unwrap_or(-1),
                            u8::from(want_active)
                        ));
                    }
                    last_mark = Some(now);
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
                        control.set_dropped(skipped);
                        publish_info(window, track, control, cid);
                        crate::debug_log(&format!(
                        "sync wall_ms={wall_ms} video_frame_time_ms={video_ms} audio_clock_ms={} audio_fresh={} delta_ms={delta} frames={frames} pushes={pushes}",
                        audio_ms.map(|value| value as i64).unwrap_or(0),
                        audio_ms.is_some()
                    ));
                        // The 平滑 figure exactly as the player publishes it, so
                        // a QA trace can read the label instead of inferring it
                        // from out_hz and the per-frame cost.
                        if want_active {
                            crate::debug_log(&format!(
                                "interp text mode={} stats={} display_hz={}",
                                interp_mode.text(),
                                control.interp_stats_text(),
                                display_refresh_hz()
                            ));
                        }
                        // How many frames the UI actually had to composite (and
                        // how many the mailbox replaced before it got to them).
                        if trace.enabled {
                            let (pushed, coalesced, posted, shown) = mailbox.counters();
                            crate::debug_log(&format!(
                                "mailbox pushed={pushed} coalesced={coalesced} posted={posted} shown={shown} proc_coalesce={:.2}",
                                coalesced as f64 / pushed.max(1) as f64,
                            ));
                        }
                    }
                    interp_log.tick(
                        control,
                        interp_mode.as_u8(),
                        out_hz,
                        blend_pool.misses(),
                    );
                    profiler.tick(control, now);
                }
                // A pre-roll that never swapped because this session ended first
                // (end of track, stall, decoder error) is released here, and its
                // target is put back so the seek still lands through the hard
                // restart.
                if let Some(rest) = shadow.take() {
                    control.restore_pending_seek(rest.target_ms);
                    rest.dispose("session-end");
                }
            }
            None => loop {
                if control.is_closed() {
                    break;
                }
                if control.is_paused() || control.occluded() {
                    // 仅音频: the sound *is* the picture, so a pause holds this
                    // session (and suspends the process) instead of ending it —
                    // a resume continues where it stopped, with no respawn.
                    // f152: an occlusion hold stops the sound for the same
                    // reason - an invisible window must not keep playing.
                    control.set_prof_rates(0.0, 0.0);
                    let held_ms = played.as_millis() as i64;
                    let lead = control
                        .audio_clock()
                        .map(|ms| ms as i64 - held_ms)
                        .unwrap_or(0);
                    pause_audio(
                        control,
                        &mut audio_gate,
                        false,
                        lead,
                        &mut last_av_log_ms,
                        &mut last_av_action,
                        "pause",
                    );
                    control.note_av_state(lead, AV_HOLD);
                    if !pause_hold_logged {
                        pause_hold_logged = true;
                        crate::debug_log(&format!(
                            "pause hold enter audio_only=1 wall_ms={held_ms}"
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    // The pause's wall time is not playback time.
                    last = Instant::now();
                    profiler.tick(control, Instant::now());
                    continue;
                }
                if pause_hold_logged {
                    pause_hold_logged = false;
                    crate::debug_log(&format!(
                        "pause hold resume audio_only=1 wall_ms={}",
                        played.as_millis()
                    ));
                    last = Instant::now();
                    // A pause released in place: the process wasn't replaced.
                    if audio_gate.is_paused() {
                        let lead = control
                            .audio_clock_base()
                            .map(|ms| ms as i64 - played.as_millis() as i64)
                            .unwrap_or(0);
                        if audio_gate.was_muted() {
                            let ms = played.as_millis() as u64;
                            let url = control.audio_url_for(info);
                            if let Some(proc) = audio_child.take() {
                                proc.stop();
                                control.set_audio_pid(0);
                            }
                            audio_gate.forget();
                            audio_child = url
                                .as_deref()
                                .and_then(|url| start_audio(url, ms, speed, false, control));
                            audio_gate.set_pid(control.audio_pid());
                        } else {
                            resume_audio(
                                control,
                                &mut audio_gate,
                                lead,
                                &mut last_av_log_ms,
                                &mut last_av_action,
                                "pause",
                            );
                        }
                    }
                }
                if control.seek_seq() != seen_seek || control.has_pending_seek() {
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
                    let audio_url = control.audio_url_for(info);
                    audio_child = audio_url
                        .as_deref()
                        .and_then(|url| start_audio(url, resume_ms, speed, false, control));
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
                // 仅音频 has no picture to prove the track is alive, so the
                // sound is the proof instead: a playhead at the duration while
                // ffplay still reports a fresh position is a duration mismatch,
                // not the end (the process exiting ends the session above).
                if duration > 0 && position >= duration && audio_ms.is_none() {
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
        //
        // f151 exception: when this session ends **because a release froze it**
        // and its overlay is already on screen, the overlay belongs to the
        // seek, not to this session. It must stay up over the frozen frame
        // until the new position's first frame closes it (that frame is pushed
        // by the adopted/restarted session). Closing it here would uncover the
        // stale picture and hide the wait the user is in.
        if control.seek_freeze() && control.seek_buffering() {
            crate::debug_log("seek buffer kept reason=seek-freeze");
        } else {
            if control.end_seek_buffering().is_some() {
                crate::debug_log("seek buffer cleared reason=session-end");
            }
            control.end_buffering("session-end");
        }
        // An auto pause belongs to the session that is ending; the next session
        // always starts from a released state.
        control.set_auto_paused(false);
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
            tag_here("pacer:kill");
            kill(child);
        }
        // No decoder is running between sessions: a play press in this window
        // must be able to restart one (see `request_resume`).
        control.decoder_live.store(false, Ordering::SeqCst);
        // The child is dead, so the reader's blocking read has returned; join it
        // before the next iteration so no stale frames or threads leak.
        if let Some(source) = source.as_mut() {
            tag_here("pacer:join-reader");
            source.stop();
        }
        if let Some(proc) = audio_child.take() {
            tag_here("pacer:audio-stop");
            proc.stop();
        }
        tag_here("pacer:join-stderr");
        let stderr_text = stderr_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        crate::debug_log(&format!(
            "decode frames={frames} pushes={pushes} finished={finished} interrupted={interrupted} seeking={seeking} mode_changed={mode_changed} exit={exit_status} stderr={}",
            stderr_text.chars().take(240).collect::<String>()
        ));

        if control.is_closed() {
            return Ok(());
        }
        if seeking || mode_changed {
            continue;
        }
        // A decoder read failure away from the end restarts the session at the
        // position the picture stopped at. A stream that dies before its first
        // frame gives up after a few tries, and one that had been playing is
        // retried a bounded number of times so a broken file cannot spin.
        if interrupted {
            control.decoder_live.store(false, Ordering::SeqCst);
            if frames > 0 {
                decode_retries = 0;
            }
            decode_retries = decode_retries.saturating_add(1);
            crate::debug_log(&format!(
                "decode interrupted retry={decode_retries} pos_ms={} dur_ms={}",
                control.playback_position_ms(),
                control.duration_ms()
            ));
            if decode_retries <= 3 {
                std::thread::sleep(Duration::from_millis(250));
                spawn_reason = "retry";
                continue;
            }
            return Ok(());
        }
        if finished && frames == 0 && !stderr_text.is_empty() {
            return Err(anyhow!("{stderr_text}"));
        }
        return Ok(());
    }
}

/// Shared state of the decode-ahead ring.
struct FrameRing {
    frames: VecDeque<FrameBuf>,
    /// The reader thread has finished (clean end of stream or a read error);
    /// no further frames will arrive.
    done: bool,
    /// The reader stopped on a **read error**, not a clean end of stream. A
    /// failure away from the end of the track is an interruption, never "this
    /// video is over".
    failed: bool,
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
    /// Whether this source's ring and decode counters belong to the player. A
    /// seek shadow's do **not**: publishing them would overwrite the occupancy
    /// of the session that is still on screen (the auto-pause guard and the
    /// buffer detail read it). Switched on when the shadow is adopted.
    publish: Arc<AtomicBool>,
    /// Monotonic ms the source's first frame reached the ring (`0` = none yet).
    /// Recorded for shadows too, so their first-frame cost is measured rather
    /// than inferred.
    first_at_ms: Arc<AtomicU64>,
}

impl FrameSource {
    /// Spawn the reader thread that fills the ring from `stdout`.
    ///
    /// `spawn_ms` is the monotonic timestamp taken just before the child was
    /// spawned; the first frame to land in the ring turns it into the
    /// profiler's spawn→first-frame latency.
    ///
    /// `size` is the size of every frame on the pipe **and** in the ring: the
    /// session's [`reader_output_size`], which ffmpeg's own scaler produces
    /// (SSE2, and it also cuts the pipe traffic). The reader therefore converts
    /// straight into the destination with no second pass.
    fn spawn(
        stdout: std::process::ChildStdout,
        control: Arc<PlaybackControl>,
        spawn_ms: u64,
        size: FrameSize,
        frame_ms: f64,
        capacity: usize,
    ) -> Self {
        Self::spawn_with(stdout, control, spawn_ms, size, frame_ms, capacity, true)
    }

    /// A seek shadow's source: identical decoding, but its ring occupancy is
    /// kept out of the player's counters until the swap adopts it.
    fn spawn_shadow(
        stdout: std::process::ChildStdout,
        control: Arc<PlaybackControl>,
        spawn_ms: u64,
        size: FrameSize,
        frame_ms: f64,
        capacity: usize,
    ) -> Self {
        Self::spawn_with(stdout, control, spawn_ms, size, frame_ms, capacity, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_with(
        mut stdout: std::process::ChildStdout,
        control: Arc<PlaybackControl>,
        spawn_ms: u64,
        size: FrameSize,
        frame_ms: f64,
        capacity: usize,
        publish: bool,
    ) -> Self {
        let capacity = capacity.max(1);
        // NV12 on the pipe (the default) or the old RGBA pipe (`BILI_PIX_FMT=rgba`).
        let convert = pipe_is_nv12();
        let pipe_frame_bytes = if convert {
            nv12_bytes(size)
        } else {
            size.bytes()
        };
        let pixel_w = size.w as usize;
        let pixel_h = size.h as usize;
        let ring = Arc::new((
            Mutex::new(FrameRing {
                frames: VecDeque::with_capacity(capacity),
                done: false,
                failed: false,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let publish = Arc::new(AtomicBool::new(publish));
        let first_at_ms = Arc::new(AtomicU64::new(0));
        let reader_ring = ring.clone();
        let reader_control = control.clone();
        let reader_publish = publish.clone();
        let reader_first_at = first_at_ms.clone();
        let reader = std::thread::spawn(move || {
            note_thread_role("reader");
            // Decoding, conversion and pipe reads are background work: below
            // normal priority means the UI thread's pointer, scroll and resize
            // handlers preempt them on the cores they share.
            lower_current_thread_priority();
            let (lock, cvar) = &*reader_ring;
            // One pipe scratch buffer for the whole session. Its RGBA frame is
            // written straight into the buffer the UI will show, so a decoded
            // frame is never copied: NV12 bytes come in, and the conversion
            // (with the optional downscale) writes the destination.
            // 16 bytes of slack: the bilinear chroma load reads two bytes past
            // the last chroma row of the frame.
            let mut pipe_buffer = vec![0u8; pipe_frame_bytes + 16].into_boxed_slice();
            let mut first = true;
            // Admission pacing: above the ring's low-water mark frames enter at
            // most `READER_RATE_LIMIT`× realtime. ffmpeg can decode a 1080p
            // stream many times faster than it plays, so without this an
            // open/seek filled the whole ring in one burst — CPU the UI thread
            // needs for the very frame it is drawing.
            let frame_period = Duration::from_secs_f64((frame_ms / 1000.0).max(1.0 / 240.0));
            let pace_step = frame_period.mul_f64(1.0 / READER_RATE_LIMIT);
            let low_water = (capacity / 2).max(1);
            // Schedule of admissions, kept work-inclusive: the wait is what is
            // *left* of the interval after this frame's read, convert and
            // downscale, so the reader really runs at 1.25× realtime instead of
            // 1.25× plus its own cost (which made it fall behind the decoder on
            // a busy machine and starve the ring).
            let mut last_admit = Instant::now().checked_sub(frame_period).unwrap_or_else(Instant::now);
            loop {
                match lock.lock() {
                    Ok(guard) if guard.stopped => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
                let mut slot = FrameBuf::new(size.w, size.h);
                let read_into: &mut [u8] = if convert {
                    &mut pipe_buffer[..pipe_frame_bytes]
                } else {
                    slot.make_mut_bytes()
                };
                tag_here("reader:read");
                match read_frame(&mut stdout, read_into) {
                    Ok(true) => {
                        tag_here("reader:convert");
                        if convert && pixel_w > 0 && pixel_h > 0 {
                            // Straight conversion, no scaling: the pipe already
                            // carries the size the UI draws (ffmpeg's swscale is
                            // SSE2-optimised; the scalar downscale this used to
                            // do on the reader cost 0.74 cores at 1080p→668p).
                            nv12_to_rgba(slot.make_mut_bytes(), &pipe_buffer, pixel_w, pixel_h);
                        }
                        tag_here("reader:wait-ring");
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
                        let above_low_water = guard.frames.len() >= low_water;
                        guard.frames.push_back(slot);
                        if reader_publish.load(Ordering::Relaxed) {
                            reader_control.note_frame_decoded(guard.frames.len());
                        }
                        if first {
                            first = false;
                            let latency = monotonic_ms().saturating_sub(spawn_ms);
                            reader_first_at.store(monotonic_ms(), Ordering::Relaxed);
                            if reader_publish.load(Ordering::Relaxed) {
                                reader_control.note_first_frame(latency);
                            }
                        }
                        drop(guard);
                        cvar.notify_all();
                        // Throttle only while the ring is already comfortably
                        // full: below the low-water mark frames must arrive as
                        // fast as possible so the picture never starves.
                        if above_low_water && !frame_path_legacy() {
                            tag_here("reader:throttle");
                            let target = last_admit + pace_step;
                            let now = Instant::now();
                            if target > now {
                                std::thread::sleep(target - now);
                            } else if now.duration_since(target) > frame_period {
                                // Far behind (a pause, a long stall): restart the
                                // schedule instead of racing to catch up.
                                last_admit = now;
                            }
                            last_admit += pace_step;
                            let after = Instant::now();
                            if last_admit < after {
                                last_admit = after;
                            }
                        } else {
                            last_admit = Instant::now();
                        }
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
                            guard.failed = true;
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
            publish,
            first_at_ms,
        }
    }

    /// Whether this source's ring belongs to the player (see [`Self::publish`]).
    fn publish(&self) -> bool {
        self.publish.load(Ordering::Relaxed)
    }

    /// Start publishing the ring and decode counters (the shadow is adopted by
    /// the session it pre-rolled for).
    fn adopt(&self) {
        self.publish.store(true, Ordering::Relaxed);
        let (lock, _cvar) = &*self.ring;
        if let Ok(guard) = lock.lock() {
            self.control.note_ring_len(guard.frames.len());
        }
    }

    /// Ring occupancy, whether the reader is done and whether it failed — the
    /// shadow's non-blocking progress check (no counters published).
    fn occupancy(&self) -> (usize, bool, bool) {
        let (lock, _cvar) = &*self.ring;
        match lock.lock() {
            Ok(guard) => (guard.frames.len(), guard.done, guard.failed),
            Err(_) => (0, true, true),
        }
    }

    /// Monotonic ms the first frame reached the ring (`0` = none yet).
    fn first_frame_at_ms(&self) -> u64 {
        self.first_at_ms.load(Ordering::Relaxed)
    }

    /// Wait up to `timeout` for the next frame.
    ///
    /// `Ok(Some(frame))` when one arrives, `Ok(None)` when the source has ended
    /// (clean end of stream or a read error), and `Err(())` when the wait timed
    /// out with the ring still empty - the caller treats that as a stall.
    fn next_frame(&self, timeout: Duration) -> Result<Option<FrameBuf>, ()> {
        let (lock, cvar) = &*self.ring;
        let Ok(mut guard) = lock.lock() else {
            return Ok(None);
        };
        if let Some(frame) = guard.frames.pop_front() {
            self.note_pop(guard.frames.len());
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
            self.note_pop(guard.frames.len());
            drop(guard);
            cvar.notify_all();
            return Ok(Some(frame));
        }
        if guard.done {
            return Ok(None);
        }
        Err(())
    }

    /// Publish the occupancy after a pop, unless this ring is a shadow's.
    fn note_pop(&self, ring_len: usize) {
        if self.publish() {
            self.control.note_ring_len(ring_len);
        }
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

    /// Whether the reader ended on a **read error** rather than a clean end of
    /// stream. The pacer uses it to tell "the video is over" from "the pipe
    /// broke mid-track".
    fn failed(&self) -> bool {
        let (lock, _cvar) = &*self.ring;
        match lock.lock() {
            Ok(guard) => guard.failed,
            Err(_) => false,
        }
    }

    /// Take the next frame only if one is already decoded, without waiting.
    ///
    /// The interpolator needs the frame after the one it is presenting while it
    /// blends. This is the same ring read as [`Self::next_frame`], just without
    /// the timeout: no frame is requested from the decoder and nothing blocks,
    /// so the pacer can keep scheduling display ticks.
    fn try_next_frame(&self) -> Option<FrameBuf> {
        let (lock, cvar) = &*self.ring;
        let Ok(mut guard) = lock.lock() else {
            return None;
        };
        let frame = guard.frames.pop_front()?;
        self.note_pop(guard.frames.len());
        drop(guard);
        cvar.notify_all();
        Some(frame)
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
        // The ring is gone with this child; occupancy is honestly 0 (unless this
        // was a shadow's ring, which never published it in the first place).
        if self.publish() {
            self.control.note_ring_len(0);
        }
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

// ---------------------------------------------------------------------------
// NV12 -> RGBA (the reader thread's only per-frame work)
// ---------------------------------------------------------------------------
//
// ffmpeg writes NV12 (1.5 bytes/pixel) instead of RGBA (4) because the
// yuv→rgb+range conversion is the most expensive part of what it was doing for
// us: CPU NV12→RGBA in swscale cost 0.36 cores at 1440x1080/30 fps, the whole
// decode+pipe only 0.21 with `-pix_fmt nv12`. The conversion has to happen
// somewhere, so it happens here, on the reader thread, in SSE2 — 8 pixels per
// iteration, BT.709 limited range (the matrix every Bilibili DASH stream
// declares), integer 8.8 fixed point.

/// BT.709 limited-range integer coefficients, 8.8 fixed point.
const Y_GAIN: i16 = 298; // 1.164 * 256
const RV_GAIN: i16 = 403; // 1.5748 * 256
const GU_GAIN: i16 = 48; // 0.1873 * 256
const GV_GAIN: i16 = 120; // 0.4681 * 256
const BU_GAIN: i16 = 475; // 1.8556 * 256

/// Convert one NV12 frame to RGBA (`dst.len() == w*h*4`). NV12 subsamples
/// chroma by two in both directions, so `w` and `h` are always even — which
/// [`target_frame_size`] guarantees for every decoded frame.
fn nv12_to_rgba(dst: &mut [u8], src: &[u8], w: usize, h: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        // SSE2 is part of the x86-64 baseline, so this is always available.
        unsafe { nv12_to_rgba_sse2(dst.as_mut_ptr(), src.as_ptr(), w, h) };
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    nv12_to_rgba_scalar(dst, src, w, h);
}

/// The picture size a session decodes at when 原始分辨率 is off: the window's
/// video band (see [`reader_output_size`]).
///
/// `BILI_FRAME_GPU=1`: decode into a CUDA surface and let the GPU do the
/// downscale, downloading only the display-size NV12. Measured 245-257 ms to
/// the first frame vs 504-521 ms for the CPU `scale` filter. If the driver
/// cannot do it the child fails exactly like a hardware decode failure and the
/// player falls back to software decode (see the `hwaccel failed` branch in the
/// decode loop); [`disable_gpu_frames`] then keeps the rest of the session on
/// the CPU path.
fn frame_gpu_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BILI_FRAME_GPU")
            .map(|value| value.trim() == "1")
            .unwrap_or(false)
    })
}

/// Set when a GPU-scaled decoder died: the rest of the session (and every
/// later one) uses the CPU path instead of retrying a broken driver.
static GPU_FRAMES_FAILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn gpu_frames_failed() -> bool {
    GPU_FRAMES_FAILED.load(Ordering::SeqCst)
}

fn disable_gpu_frames() {
    GPU_FRAMES_FAILED.store(true, Ordering::SeqCst);
}

/// Whether this session should ask ffmpeg for a GPU-scaled picture.
///
/// Only when it actually saves pipe bytes ([`size`] is smaller than the decoded
/// size), the session keeps hardware decode, the source is remote (a local
/// cache file decodes in software - hardware setup is the whole latency
/// budget there) and the pipe carries NV12 (`BILI_PIX_FMT=rgba` is a CPU A/B).
fn gpu_frame_scale(hwaccel: bool, local: bool, size: FrameSize, decode: FrameSize) -> bool {
    frame_gpu_enabled()
        && !gpu_frames_failed()
        && hwaccel
        && !local
        && pipe_is_nv12()
        && size.bytes() < decode.bytes()
}

/// The size of the frames a session decodes, converts and hands to the UI.
///
/// **Default: follow the window.** The decoder is asked for the picture the
/// window actually shows (the video band inside it, aspect preserved, never
/// upscaled), so a 1080p source in a 1216×684 window decodes 1216×684 instead
/// of 1920×1080: the pipe carries a third of the bytes, the reader converts a
/// third of the pixels, Slint uploads a third of the image, and the screen shows
/// the same picture. It is also what makes the 画质 row honest — the player no
/// longer decodes a resolution the window cannot display.
///
/// `native` (设置 → 播放 → 原始分辨率 = `settings.player_native_size`) keeps the
/// source size instead, for anyone who wants the full frame in the ring.
///
/// The size is chosen once per session, so a resize never restarts the decoder
/// (the next session — a seek, a 画质 switch — picks up the new window). The
/// `BILI_FRAME_SCALE` A/B knob overrides the setting: `1` = window-sized,
/// `0` = native.
///
/// The pipe is the reason this exists: a 1080p source writes 3.1 MB/frame
/// (NV12) into the rawvideo pipe, a 1188×668 picture 1.19 MB. The scale itself
/// belongs on the GPU when it is available — see [`frame_gpu_enabled`].
fn reader_output_size(decode: FrameSize, display: (u32, u32), native: bool) -> FrameSize {
    // `BILI_FRAME_GPU=1` needs the display-sized path: the GPU scaler is given
    // exactly the size the ring carries, so the download is already the size
    // the UI draws.
    let enabled = match std::env::var("BILI_FRAME_SCALE")
        .ok()
        .map(|value| value.trim().to_string())
        .as_deref()
    {
        Some("0") => false,
        Some("1") => true,
        _ => frame_gpu_enabled() || !native,
    };
    if !enabled || frame_path_legacy() || display.0 == 0 || display.1 == 0 {
        return decode;
    }
    // The picture band inside the window: the UI draws the frame into the video
    // area, whose height is the width over the frame's own aspect. Cap by the
    // window on both axes and keep the aspect exact.
    let aspect = decode.w as f64 / decode.h as f64;
    let mut w = display.0 as f64;
    let mut h = w / aspect;
    if h > display.1 as f64 {
        h = display.1 as f64;
        w = h * aspect;
    }
    // Never upscale, and never shrink so far that the picture turns to mush.
    if w >= decode.w as f64 || h >= decode.h as f64 {
        return decode;
    }
    let w = ((w as u32) & !1).max(2);
    let h = ((h as u32) & !1).max(2);
    FrameSize { w, h }
}

/// Scalar reference implementation (also the non-x86 fallback). It uses the
/// same 8.8 decomposition as the SSE2 path, so the two are bit-identical.
#[cfg(any(test, not(target_arch = "x86_64")))]
fn nv12_to_rgba_scalar(dst: &mut [u8], src: &[u8], w: usize, h: usize) {
    let y_plane = src;
    let uv_plane = &src[w * h..];
    for row in 0..h {
        let y_row = &y_plane[row * w..row * w + w];
        // Chroma rows are shared by two luma rows.
        let uv_row = &uv_plane[(row / 2) * w..(row / 2) * w + w];
        let out = &mut dst[row * w * 4..(row + 1) * w * 4];
        for col in 0..w {
            let yv = y_row[col] as i32 - 16;
            let y = yv + ((yv * (Y_GAIN as i32 - 256) + 128) >> 8);
            let u = uv_row[(col / 2) * 2] as i32 - 128;
            let v = uv_row[(col / 2) * 2 + 1] as i32 - 128;
            let r = y + v + ((v * (RV_GAIN as i32 - 256) + 128) >> 8);
            let g = y - ((GU_GAIN as i32 * u + GV_GAIN as i32 * v + 128) >> 8);
            let b = y + u + ((u * (BU_GAIN as i32 - 256) + 128) >> 8);
            let at = col * 4;
            out[at] = r.clamp(0, 255) as u8;
            out[at + 1] = g.clamp(0, 255) as u8;
            out[at + 2] = b.clamp(0, 255) as u8;
            out[at + 3] = 255;
        }
    }
}

/// SSE2 body of [`nv12_to_rgba`]: eight pixels per iteration.
///
/// The gains are split as `1 + k/256` (`298/256`, `403/256`, `475/256`,
/// `48/256`, `120/256`) so every product stays inside `i16` and `mullo`+`srai`
/// is exact — the same numbers the scalar reference uses.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn nv12_to_rgba_sse2(dst: *mut u8, src: *const u8, w: usize, h: usize) {
    use std::arch::x86_64::*;

    let y_plane = src;
    let uv_plane = src.add(w * h);
    let luma_bias = _mm_set1_epi16(16);
    let chroma_bias = _mm_set1_epi16(128);
    let y_frac = _mm_set1_epi16(Y_GAIN - 256); // 42
    let rv_frac = _mm_set1_epi16(RV_GAIN - 256); // 147
    let bu_frac = _mm_set1_epi16(BU_GAIN - 256); // 219
    let gu_gain = _mm_set1_epi16(GU_GAIN);
    let gv_gain = _mm_set1_epi16(GV_GAIN);
    let alpha = _mm_set1_epi8(-1i8);
    let u_mask = _mm_set1_epi16(0x00FF);

    for row in 0..h {
        let y_row = y_plane.add(row * w);
        let uv_row = uv_plane.add((row / 2) * w);
        let out = dst.add(row * w * 4);
        let mut col = 0usize;
        while col + 8 <= w {
            // 8 luma samples and the 4 chroma pairs they share: the chroma byte
            // offset of luma column `col` is `col` (one U/V pair per 2 pixels).
            let y8 = _mm_loadl_epi64(y_row.add(col) as *const __m128i);
            let uv8 = _mm_loadl_epi64(uv_row.add(col) as *const __m128i);

            let y16 = _mm_unpacklo_epi8(y8, _mm_setzero_si128());
            let yv = _mm_sub_epi16(y16, luma_bias);
            // Round (not truncate) the fractional terms, like swscale's tables.
            let half = _mm_set1_epi16(128);
            let y = _mm_add_epi16(
                yv,
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(yv, y_frac), half), 8),
            );
            // `and` / `srli` already put the four chroma samples in words 0..3;
            // unpacking them with zero first turned that into [U0,0,U1,0,…] and
            // shifted every second pixel pair onto a bogus chroma value.
            let u16 = _mm_sub_epi16(_mm_and_si128(uv8, u_mask), chroma_bias);
            let v16 = _mm_sub_epi16(_mm_srli_epi16(uv8, 8), chroma_bias);
            let u_px = _mm_unpacklo_epi16(u16, u16);
            let v_px = _mm_unpacklo_epi16(v16, v16);
            // Chroma is duplicated per pixel pair (nearest 4:2:0 upsampling),
            // which measured *closer* to swscale's own NV12→RGBA than a 3:1
            // bilinear interpolation (mean delta 1.75 vs 1.94, max 28 vs 42 over
            // a real frame), and costs one `unpack` instead of two `pavgw` plus
            // two extra loads.

            let r_term = _mm_add_epi16(
                v_px,
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(v_px, rv_frac), half), 8),
            );
            let b_term = _mm_add_epi16(
                u_px,
                _mm_srai_epi16(_mm_add_epi16(_mm_mullo_epi16(u_px, bu_frac), half), 8),
            );
            let g_uv = _mm_add_epi16(
                _mm_add_epi16(
                    _mm_mullo_epi16(u_px, gu_gain),
                    _mm_mullo_epi16(v_px, gv_gain),
                ),
                half,
            );

            let r = _mm_adds_epi16(y, r_term);
            let g = _mm_subs_epi16(y, _mm_srai_epi16(g_uv, 8));
            let b = _mm_adds_epi16(y, b_term);

            let r8 = _mm_packus_epi16(r, r);
            let g8 = _mm_packus_epi16(g, g);
            let b8 = _mm_packus_epi16(b, b);

            let rg = _mm_unpacklo_epi8(r8, g8);
            let ba = _mm_unpacklo_epi8(b8, alpha);
            _mm_storeu_si128(
                out.add(col * 4) as *mut __m128i,
                _mm_unpacklo_epi16(rg, ba),
            );
            _mm_storeu_si128(
                out.add(col * 4 + 16) as *mut __m128i,
                _mm_unpackhi_epi16(rg, ba),
            );
            col += 8;
        }
        // Tail: the scalar rule keeps the last <8 pixels correct.
        for x in col..w {
            let yv = *y_row.add(x) as i32 - 16;
            let y = yv + ((yv * (Y_GAIN as i32 - 256) + 128) >> 8);
            let u = *uv_row.add((x / 2) * 2) as i32 - 128;
            let v = *uv_row.add((x / 2) * 2 + 1) as i32 - 128;
            let r = y + v + ((v * (RV_GAIN as i32 - 256) + 128) >> 8);
            let g = y - ((GU_GAIN as i32 * u + GV_GAIN as i32 * v + 128) >> 8);
            let b = y + u + ((u * (BU_GAIN as i32 - 256) + 128) >> 8);
            let at = out.add(x * 4);
            *at = r.clamp(0, 255) as u8;
            *at.add(1) = g.clamp(0, 255) as u8;
            *at.add(2) = b.clamp(0, 255) as u8;
            *at.add(3) = 255;
        }
    }
}

/// `BILI_FRAME_PATH=legacy`: the pre-optimisation frame hand-off, kept as the
/// before/after baseline for the UI-load measurement (the same role
/// `BILI_DRAG_INTERP` plays for the drag measurements). It is not a product
/// mode.
///
/// Legacy means exactly what the player did before: every frame is delivered to
/// the event loop as its own event, and the UI thread allocates a fresh pixel
/// buffer and copies the whole frame into it. The fixed path hands the reader's
/// buffer straight to Slint, coalesces frames that the UI has not got to yet,
/// throttles the reader to ~1.25× realtime and runs the background threads
/// below normal priority.
fn frame_path_legacy() -> bool {
    static LEGACY: OnceLock<bool> = OnceLock::new();
    *LEGACY.get_or_init(|| {
        std::env::var("BILI_FRAME_PATH")
            .map(|value| value.trim().eq_ignore_ascii_case("legacy"))
            .unwrap_or(false)
    })
}

/// `BILI_CLK_TRACE=1`: one line per accepted audio-clock sample with the error
/// it closed — the measurement that decides whether the master clock is slewed
/// or stepped (see [`PlaybackControl::set_audio_clock_ms`]).
fn clock_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("BILI_CLK_TRACE")
            .map(|value| value == "1")
            .unwrap_or(false)
    })
}

/// One frame on its way to the UI.
struct MailFrame {
    buffer: FrameBuf,
    /// The buffer came from the blend pool and goes back once the window has
    /// replaced the image that held it.
    pooled: bool,
}

/// The newest frame waiting for the UI, plus the single event-loop delivery
/// that will show it.
///
/// The pacer can produce frames faster than the UI thread can composite them
/// (a full 1080p window recomposite costs several milliseconds; a 60 Hz blend
/// grid produces 60 a second while a drag is also running). Posting every one
/// of them queued a backlog of stale pictures on the event loop, and each item
/// of that backlog cost the UI thread another full recomposite — the reason the
/// *whole app* became sluggish while a video played.
///
/// The mailbox keeps one slot: a frame that is still waiting is replaced (and
/// counted as coalesced) instead of queued, so the UI thread only ever shows
/// the newest picture. One delivery event is in flight at a time, and a
/// delivery shows at most [`MAILBOX_FRAMES_PER_TURN`] frames, so a burst can
/// never monopolise the loop.
struct FrameMailbox {
    inner: Mutex<MailboxInner>,
    window: slint::Weak<PlayerWindow>,
    mini: MiniSlot,
    pool: Arc<BlendPool>,
    /// f151: the frame gate for a frozen seek. Read on both the pacer thread
    /// (`push`) and the UI thread (`show`), so a frame that was produced before
    /// the release but delivered after it can never replace the frozen picture.
    control: Arc<PlaybackControl>,
    /// Frames accepted into the slot.
    pushed: AtomicU64,
    /// Frames replaced before the UI took them.
    coalesced: AtomicU64,
    /// Delivery events posted to the event loop.
    posted: AtomicU64,
    /// Frames actually shown.
    shown: AtomicU64,
    /// `BILI_FRAME_PATH=legacy` (the QA baseline, see [`frame_path_legacy`]).
    legacy: bool,
}

struct MailboxInner {
    frame: Option<MailFrame>,
    /// A delivery event is queued (or being executed).
    pending: bool,
    /// The buffer currently displayed by the window (one generation behind the
    /// newest frame), and whether it came from the blend pool. The window drops
    /// its reference when it shows a newer picture, so this handle is the only
    /// owner left after that and the buffer can go back to the pool without a
    /// copy-on-write.
    displayed: Option<(FrameBuf, bool)>,
}

/// Frames one delivery event may show. Two covers a 60 fps grid on a 30 fps
/// source (a coalesced pair) without ever letting a backlog turn one event into
/// a long recomposite streak.
const MAILBOX_FRAMES_PER_TURN: usize = 2;

impl FrameMailbox {
    fn new(
        window: slint::Weak<PlayerWindow>,
        mini: MiniSlot,
        pool: Arc<BlendPool>,
        control: Arc<PlaybackControl>,
    ) -> Self {
        Self {
            inner: Mutex::new(MailboxInner {
                frame: None,
                pending: false,
                displayed: None,
            }),
            window,
            mini,
            pool,
            control,
            pushed: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            posted: AtomicU64::new(0),
            shown: AtomicU64::new(0),
            legacy: frame_path_legacy(),
        }
    }

    /// Hand a frame to the UI. Called from the pacer thread; never blocks on
    /// the event loop.
    fn push(self: &Arc<Self>, frame: MailFrame) {
        // f151: a release froze the picture. An old-session frame produced
        // before the pacer saw the freeze must not enter the slot; the frame on
        // screen at the release is the last one the user sees until the swap.
        if self.control.seek_freeze() {
            if frame.pooled {
                self.pool.recycle(frame.buffer);
            }
            return;
        }
        self.pushed.fetch_add(1, Ordering::Relaxed);
        if self.legacy {
            // QA baseline: one event and one full-frame copy per frame, exactly
            // like the old `push_frame` / `push_recycled_frame`.
            self.posted.fetch_add(1, Ordering::Relaxed);
            let this = self.clone();
            let _ = slint::invoke_from_event_loop(move || this.show_legacy(frame));
            return;
        }
        let post = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            if let Some(replaced) = inner.frame.replace(frame) {
                self.coalesced.fetch_add(1, Ordering::Relaxed);
                // Never shown, so this allocation is ours alone.
                if replaced.pooled {
                    self.pool.recycle(replaced.buffer);
                }
            }
            if inner.pending {
                false
            } else {
                inner.pending = true;
                true
            }
        };
        if post {
            self.posted.fetch_add(1, Ordering::Relaxed);
            let this = self.clone();
            let _ = slint::invoke_from_event_loop(move || this.drain());
        }
    }

    /// UI thread: show the waiting frames, at most [`MAILBOX_FRAMES_PER_TURN`]
    /// per turn, then either hand the turn back or ask for another one.
    fn drain(self: &Arc<Self>) {
        start_ui_load_probe();
        let mut repost = false;
        for _ in 0..MAILBOX_FRAMES_PER_TURN {
            let frame = match self.inner.lock() {
                Ok(mut inner) => inner.frame.take(),
                Err(_) => None,
            };
            let Some(frame) = frame else { break };
            self.show(frame);
        }
        if let Ok(mut inner) = self.inner.lock() {
            if inner.frame.is_some() {
                // Frames arrived while this turn ran; keep `pending` set and
                // ask for one more turn (a pusher then cannot post twice).
                repost = true;
            } else {
                inner.pending = false;
            }
        }
        if repost {
            let this = self.clone();
            let _ = slint::invoke_from_event_loop(move || this.drain());
        }
    }

    /// UI thread: put one frame on screen and retire the previous one.
    fn show(&self, frame: MailFrame) {
        tag_here("ui:publish");
        let MailFrame { buffer, pooled } = frame;
        // f151: the same freeze the pacer checks, re-read on the UI thread. A
        // delivery that was queued before the release runs *after* the seek's
        // own event (both are on this thread), so this second gate is what
        // makes the frozen frame exact rather than one frame late.
        if self.control.seek_freeze() {
            if pooled {
                self.pool.recycle(buffer);
            }
            return;
        }
        self.shown.fetch_add(1, Ordering::Relaxed);
        let image = slint::Image::from_rgba8(buffer.clone());
        {
            let view = crate::player_view::SlintPlayerView(self.window.clone());
            crate::player_view::Presentation::new(&view)
                .frame(crate::player_view::Picture::Image(image.clone()));
        }
        let mini = self
            .mini
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .and_then(|weak| weak.upgrade());
        if let Some(mini) = mini {
            mini.set_frame(image);
        }
        // The window's property now holds the new image and has dropped the
        // previous one, so the buffer that was displayed a moment ago has no
        // other owner: recycle it for the pacer.
        let retired = match self.inner.lock() {
            Ok(mut inner) => inner.displayed.replace((buffer, pooled)),
            Err(_) => None,
        };
        if let Some((previous, was_pooled)) = retired {
            if was_pooled {
                self.pool.recycle(previous);
            }
        }
    }

    /// `BILI_FRAME_PATH=legacy` only: the old hand-off — allocate a pixel buffer
    /// on the UI thread and copy the frame into it.
    fn show_legacy(&self, frame: MailFrame) {
        tag_here("ui:publish");
        start_ui_load_probe();
        let MailFrame { buffer, pooled } = frame;
        if self.control.seek_freeze() {
            if pooled {
                self.pool.recycle(buffer);
            }
            return;
        }
        let mut copy = FrameBuf::new(buffer.width(), buffer.height());
        copy.make_mut_bytes().copy_from_slice(buffer.as_bytes());
        let image = slint::Image::from_rgba8(copy);
        if let Some(window) = self.window.upgrade() {
            window.set_frame(image.clone());
        }
        let mini = self
            .mini
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .and_then(|weak| weak.upgrade());
        if let Some(mini) = mini {
            mini.set_frame(image);
        }
        self.shown.fetch_add(1, Ordering::Relaxed);
        if pooled {
            self.pool.recycle(buffer);
        }
    }

    /// `frames coalesced` counters, for the QA line.
    fn counters(&self) -> (u64, u64, u64, u64) {
        (
            self.pushed.load(Ordering::Relaxed),
            self.coalesced.load(Ordering::Relaxed),
            self.posted.load(Ordering::Relaxed),
            self.shown.load(Ordering::Relaxed),
        )
    }
}

/// Once-a-second `interp ...` line, enabled by the QA hook or an explicit log
/// request. The counters live on [`PlaybackControl`], so the line survives
/// session restarts; the per-second rate is the delta over the window.
struct InterpLog {
    enabled: bool,
    last: Option<Instant>,
    last_generated: u32,
    last_dropped: u32,
    last_misses: u64,
    last_mode: u8,
    /// Cumulative optical-flow submits at the last line, for the pairs/s rate.
    last_ofa_submits: u64,
}

impl InterpLog {
    fn new() -> Self {
        let enabled = std::env::var("BILI_INTERP_TEST")
            .map(|value| value == "1" || value == "2" || value == "3")
            .unwrap_or(false)
            || std::env::var("BILI_INTERP_LOG")
                .map(|value| value == "1")
                .unwrap_or(false);
        Self {
            enabled,
            last: None,
            last_generated: 0,
            last_dropped: 0,
            last_misses: 0,
            last_mode: 0,
            last_ofa_submits: 0,
        }
    }

    /// Emit `interp mode=<n> generated=<n>/s out_hz=<n> avg_ms=<x> bm_ms=<x> ofa_…`
    /// once a second. `out_hz` is `0` while interpolation is off/unsupported.
    /// `bm_ms` is the average block-matching pair time (`0` unless mode 2 ran a
    /// pair in this episode); `ofa_pair_ms`/`ofa_flow_ms` are the mode-3 pair
    /// total (upload + execute + readback + warps) and its readback part, and
    /// `ofa_submit/s` is how many pairs per second actually reached the engine
    /// (one per source interval when nothing is dropping frames). The QA-only
    /// tail (`drop`/`min_ms`/`max_ms`/`pool_miss`) separates a slow pass from a
    /// preempted pacer thread; `drop` counts the display slots that were lost
    /// because they were late.
    fn tick(&mut self, control: &PlaybackControl, mode: u8, out_hz: u32, pool_misses: u64) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let (generated, dropped, avg_ms) = control.interp_stats();
        let bm_ms = control.interp_bm_ms();
        let (bm_p50, bm_p95) = control.interp_bm_percentiles();
        let bm_n = control.interp_bm_pairs.load(Ordering::Relaxed);
        let ofa_ms = control.interp_ofa_ms();
        let ofa_flow_ms = control.interp_ofa_flow_ms();
        let (ofa_p50, ofa_p95) = control.interp_ofa_percentiles();
        let ofa_n = control.interp_ofa_pairs.load(Ordering::Relaxed);
        let ofa_submits = control.interp_ofa_submits();
        let mode_changed = mode != self.last_mode;
        let Some(last) = self.last else {
            self.last = Some(now);
            self.last_generated = generated;
            self.last_dropped = dropped;
            self.last_misses = pool_misses;
            self.last_mode = mode;
            self.last_ofa_submits = ofa_submits;
            return;
        };
        // A mode change (or the counter reset it performs) starts a new window.
        if mode_changed
            || generated < self.last_generated
            || dropped < self.last_dropped
            || ofa_submits < self.last_ofa_submits
        {
            self.last = Some(now);
            self.last_generated = generated;
            self.last_dropped = dropped;
            self.last_misses = pool_misses;
            self.last_mode = mode;
            self.last_ofa_submits = ofa_submits;
            return;
        }
        let elapsed = now.duration_since(last);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let window_ms = elapsed.as_millis().max(1) as u64;
        let per_s = |now: u64, before: u64| now.saturating_sub(before) * 1000 / window_ms;
        let gen_s = per_s(u64::from(generated), u64::from(self.last_generated));
        let drop_s = per_s(u64::from(dropped), u64::from(self.last_dropped));
        let (min_ms, max_ms) = control.interp_cost_range();
        let miss_s = per_s(pool_misses, self.last_misses);
        let ofa_sub_s = per_s(ofa_submits, self.last_ofa_submits);
        self.last = Some(now);
        self.last_generated = generated;
        self.last_dropped = dropped;
        self.last_misses = pool_misses;
        self.last_mode = mode;
        self.last_ofa_submits = ofa_submits;
        crate::debug_log(&format!(
            "interp mode={mode} generated={gen_s}/s out_hz={out_hz} avg_ms={avg_ms:.3} bm_ms={bm_ms:.3} bm_p50_ms={bm_p50:.3} bm_p95_ms={bm_p95:.3} bm_n={bm_n} ofa_pair_ms={ofa_ms:.3} ofa_p50_ms={ofa_p50:.3} ofa_p95_ms={ofa_p95:.3} ofa_flow_ms={ofa_flow_ms:.3} ofa_submit_s={ofa_sub_s} ofa_n={ofa_n} drop={drop_s}/s min_ms={min_ms:.3} max_ms={max_ms:.3} pool_miss={miss_s}/s"
        ));
    }
}

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
                format!(
                    "画质：{}（qn {}）",
                    bilibili::quality_label(track.id),
                    track.id
                )
                .into(),
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
    lines.push(format!("硬件解码：{}", if control.hwaccel() { "开" } else { "关" }).into());
    lines.push(format!("仅音频：{}", if control.audio_only() { "开" } else { "关" }).into());
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
        let view = crate::player_view::SlintPlayerView(window);
        crate::player_view::Presentation::new(&view).status(&text, kind);
    });
}

fn set_playing(window: &slint::Weak<PlayerWindow>, playing: bool) {
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let view = crate::player_view::SlintPlayerView(window);
        let mut presentation = crate::player_view::Presentation::new(&view);
        if playing {
            presentation.resume();
        } else {
            presentation.pause();
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
    control: &Arc<PlaybackControl>,
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

    // ---- picker: `audio_quality` (a DASH id, 0 = 自动（最高）) ----
    let request = control.audio_quality();
    let chosen = if request <= 0 {
        0
    } else {
        tracks
            .iter()
            .position(|track| track.id == request)
            .unwrap_or(0)
    };
    control.set_audio_index(chosen);
    let available: Vec<i64> = tracks.iter().map(|track| track.id).collect();
    let chosen_id = tracks.get(chosen).map(|track| track.id).unwrap_or(0);
    let chosen_bw = tracks.get(chosen).map(|track| track.bandwidth).unwrap_or(0);
    crate::debug_log(&format!(
        "audio quality request={request} available={available:?} chosen={chosen_id} bw={}kbps",
        chosen_bw / 1000
    ));
    if request > 0 && chosen_id != request {
        let name = crate::bilibili::audio_label(request, 0);
        if let Some(window) = window.upgrade() {
            crate::toast(
                &window,
                format!("该视频没有所选音质，已用 {}", bilibili::audio_label(chosen_id, chosen_bw)),
                crate::TOAST_KIND_INFO,
            );
        }
        crate::debug_log(&format!("audio quality fallback request={request} chosen={chosen_id} name={name}"));
    }

    // ---- the 音质 section of the 画质 popup ----
    let rows = audio_quality_rows(tracks, request);
    let label = tracks
        .get(chosen)
        .map(|track| bilibili::audio_label(track.id, track.bandwidth))
        .unwrap_or_else(|| "音轨".to_string());
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_audio_items(ModelRc::from(std::rc::Rc::new(VecModel::from(entries))));
            window.set_audio_index(chosen as i32);
            window.set_audio_label(label.into());
            window.set_audio_quality_rows(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
            window.set_audio_quality(request as i32);
        }
    });
}

/// Build the 音质 rows: 自动 plus every advertised tier. A tier the video does
/// not carry is rendered locked; its `detail` explains why. The detail of a
/// present tier is its REAL measured bandwidth, never a hardcoded number.
fn audio_quality_rows(tracks: &[bilibili::AudioTrack], request: i64) -> Vec<AudioQualityRow> {
    const TIERS: [(i64, &str, bool); 5] = [
        (30280, "192K", false),
        (30232, "132K", false),
        (30216, "64K", false),
        (30250, "杜比全景声", true),
        (30251, "Hi-Res 无损", true),
    ];
    let mut rows = Vec::with_capacity(TIERS.len() + 1);
    let auto_present = !tracks.is_empty();
    rows.push(AudioQualityRow {
        id: 0,
        label: "自动（最高）".into(),
        detail: if auto_present {
            format!(
                "跟随视频 · 当前 {}",
                tracks
                    .first()
                    .map(|track| format!("{} kbps", track.bandwidth / 1000))
                    .unwrap_or_else(|| "—".to_string())
            )
        } else {
            "该视频没有音轨".to_string()
        }
        .into(),
        current: request <= 0 && auto_present,
        locked: !auto_present,
    });
    for (id, name, premium) in TIERS {
        match tracks.iter().find(|track| track.id == id) {
            Some(track) => rows.push(AudioQualityRow {
                id: id as i32,
                label: name.into(),
                detail: format!(
                    "{} · {} kbps",
                    crate::settings::audio_tier_name(id as u32),
                    track.bandwidth / 1000
                )
                .into(),
                current: request == id,
                locked: false,
            }),
            None => rows.push(AudioQualityRow {
                id: id as i32,
                label: name.into(),
                detail: if premium {
                    "需要大会员 / 该视频没有此音质".to_string()
                } else {
                    "该视频没有此音质".to_string()
                }
                .into(),
                current: false,
                locked: true,
            }),
        }
    }
    rows
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

/// One ffmpeg probe window. The media is a DASH `m4s` whose `moov` sits in the
/// first block, so the default 5 s / 5 MB analysis only delays the first frame.
/// `BILI_FFMPEG_PROBESIZE` / `BILI_FFMPEG_ANALYZE` are the QA hooks used to
/// measure that (see the startup table in the player notes); when set they
/// override both windows.
///
/// A **local cache-file** read is the one case where the probe is pure startup
/// cost: there is no connection to warm, the whole `moov` is already on disk,
/// and the seek's first frame is latency. 64 KiB still parses the init segment
/// (verified against the cached DASH m4s at every seek target) and shaves the
/// first part of the measured spawn→first-frame time. Network/unknown sources
/// keep the existing 128 KiB window, where a larger probe is free relative to
/// the connection latency.
fn probe_args(local: bool) -> (&'static str, &'static str) {
    static REMOTE: OnceLock<(String, String)> = OnceLock::new();
    static LOCAL: OnceLock<(String, String)> = OnceLock::new();
    let read = |name: &str, fallback: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    };
    let args = if local {
        LOCAL.get_or_init(|| {
            (
                read("BILI_FFMPEG_PROBESIZE", "65536"),
                read("BILI_FFMPEG_ANALYZE", "0"),
            )
        })
    } else {
        REMOTE.get_or_init(|| {
            (
                read("BILI_FFMPEG_PROBESIZE", "131072"),
                read("BILI_FFMPEG_ANALYZE", "0"),
            )
        })
    };
    (args.0.as_str(), args.1.as_str())
}

/// Bytes ffmpeg writes per frame on the rawvideo pipe in NV12
/// (4:2:0, one plane + interleaved chroma: `w*h*3/2`).
fn nv12_bytes(size: FrameSize) -> usize {
    let w = size.w as usize;
    let h = size.h as usize;
    w * h + (w / 2) * 2 * (h / 2)
}

/// Whether the pipe carries NV12 (`true`, default) or RGBA (`false`).
///
/// `BILI_PIX_FMT=rgba` is the A/B hook for the CPU measurement: it restores the
/// old RGBA pipe so ffmpeg's own cost can be compared on the same video, machine
/// and build. The reader converts NV12 with [`nv12_to_rgba`]; an RGBA pipe is
/// read straight into the frame slot.
fn pipe_is_nv12() -> bool {
    static NV12: OnceLock<bool> = OnceLock::new();
    *NV12.get_or_init(|| {
        !std::env::var("BILI_PIX_FMT")
            .map(|value| value.trim().eq_ignore_ascii_case("rgba"))
            .unwrap_or(false)
    })
}

fn spawn_video(
    url: &str,
    start_ms: u64,
    hwaccel: bool,
    size: FrameSize,
    local: bool,
    gpu: bool,
) -> std::io::Result<Child> {
    let (probe_size, probe_duration) = probe_args(local);
    let nv12 = pipe_is_nv12();
    let pix_fmt = if nv12 { "nv12" } else { "rgba" };
    let mut command = base_command("ffmpeg.exe");
    command
        .arg("-hide_banner")
        // `info` (not `error`): the input dump carries the stream's own
        // `Duration:`, which is the only authoritative length when the API
        // value is wrong (see `parse_ffmpeg_duration`). `-nostats` suppresses
        // the periodic progress line, so the cost is the one-off banner.
        .arg("-loglevel")
        .arg("info")
        .arg("-nostats")
        .arg("-nostdin")
        // Decode on all cores. The probe window is deliberately tiny: the
        // stream is a known DASH m4s whose `moov` is in the first block, so the
        // default 5 s / 5 MB analysis only delays the first frame (measured:
        // 1.79 s spawn→first frame with the defaults).
        .arg("-threads")
        .arg("0")
        .arg("-probesize")
        .arg(probe_size)
        .arg("-analyzeduration")
        .arg(probe_duration);
    if local {
        // A known DASH cache file needs no stream guessing: the `moov` carries
        // the codec parameters, so one probe packet and no fps probing is
        // enough. On the seek path this is pure first-frame latency.
        command
            .arg("-fpsprobesize")
            .arg("0")
            .arg("-max_probe_packets")
            .arg("1");
    }
    command.arg("-fflags").arg("+discardcorrupt");
    if !local {
        // Survive CDN hiccups mid-stream. The reconnect options resume a
        // dropped connection instead of ending playback; no
        // `-reconnect_at_eof` (that would restart a VOD from the beginning).
        command
            .arg("-reconnect")
            .arg("1")
            .arg("-reconnect_streamed")
            .arg("1")
            .arg("-reconnect_delay_max")
            .arg("5")
            .arg("-rw_timeout")
            .arg("15000000");
    }
    if gpu {
        // CUDA surfaces, not d3d11: this ffmpeg has no D3D11 scaling filter,
        // while `scale_cuda` is present (ffmpeg 7.1.1, checked on this
        // machine). The frame is downloaded once, at the display size.
        command
            .arg("-hwaccel")
            .arg("cuda")
            .arg("-hwaccel_output_format")
            .arg("cuda");
    } else if hwaccel && !local {
        // Hardware decode costs ~500 ms of device setup per process (measured on
        // this machine: 876 ms to the first frame from a local file with
        // `-hwaccel d3d11va`, 94 ms without). For a **local cache-file session**
        // — a cached seek, or opening a fully downloaded video — that startup is
        // the whole latency budget, and the file's decode is cheap, so software
        // decode is the right trade. A streaming session keeps hardware decode:
        // there the steady-state cost is what matters.
        command.arg("-hwaccel").arg("d3d11va");
    }

    // No `-re`/`-readrate`: ffmpeg decodes ahead and blocks on the rawvideo
    // pipe, and the decode loop paces the frames itself against the audio clock.
    // The scaler target is source-based (see `target_frame_size`), never the
    // window and never a fixed 960x540, so a 1080P source keeps its detail and a
    // resize cannot change the decode.
    //
    // The pipe carries **NV12** (1.5 bytes/pixel) instead of RGBA (4): the
    // yuv→rgb conversion is the single most expensive thing ffmpeg did for us,
    // and doing it in the reader thread with SSE2 costs a fraction of that (see
    // `nv12_to_rgba`). Measured on 1440x1080/30 fps: ffmpeg 0.57 → 0.21 cores.
    let filter = if gpu {
        format!(
            "scale_cuda={}:{}:format=nv12,hwdownload,format=nv12",
            size.w, size.h
        )
    } else {
        format!("scale={}:{}", size.w, size.h)
    };

    if start_ms > 0 {
        command.arg("-ss").arg(format_secs(start_ms));
    }
    crate::debug_log(&format!(
        "spawn ffmpeg hwaccel={} gpu={} start_ms={start_ms} local={local} vf={filter} out=rawvideo:{pix_fmt}:{}x{} bytes={} probesize={probe_size} analyzeduration={probe_duration}",
        u8::from(hwaccel && !local),
        u8::from(gpu),
        size.w,
        size.h,
        if nv12 { nv12_bytes(size) } else { size.bytes() },
    ));
    if !local {
        command.arg("-headers").arg(MEDIA_HEADERS);
    }
    command
        .arg("-i")
        .arg(url)
        .arg("-an")
        .arg("-vf")
        .arg(filter)
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg(pix_fmt)
        .arg("-");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| {
            crate::job::assign(&child);
            // Every video child - streaming or local cache-file - is a deadline
            // task and runs at the normal class (see `NORMAL_PRIORITY_CLASS`).
            // The pipe reader/converter it feeds lowers *itself* instead, which
            // is where the yieldable work actually is.
            raise_process_priority(child.id());
            child
        })
}

fn spawn_audio(url: &str, start_ms: u64, speed: f64, local: bool) -> std::io::Result<Child> {
    let mut command = base_command("ffplay.exe");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("quiet")
        .arg("-stats")
        .arg("-nodisp")
        .arg("-autoexit");
    if !local {
        // Reconnect policy for a proxied/CDN audio body. NO `-infbuf`: it makes
        // ffplay fill an unbounded input buffer before it starts, which measured
        // ~1.3 s from spawn to the first reported position (`audio clock first
        // after_ms=1320`) — that latency was the whole startup A/V debt.
        // ffplay's default bounded buffer starts the sound in a few hundred ms.
        command
            .arg("-reconnect")
            .arg("1")
            .arg("-reconnect_streamed")
            .arg("1")
            .arg("-reconnect_delay_max")
            .arg("5")
            .arg("-rw_timeout")
            .arg("15000000");
    }
    command
        // Fixed, never the user's level: gain and mute are applied to this
        // process' WASAPI session (see `PlaybackControl::set_volume`), so a
        // volume change is a session write instead of a process restart.
        //
        // **Never `-volume 0`.** A process spawned at mixer volume 0 is silent
        // forever: the session gain multiplies that zero, so the old silent seed
        // could not be made audible and had to be killed and respawned before
        // the first sound (the measured picture-then-sound start). The silence
        // before the first frame is a *session mute* now, which the first frame
        // releases in one call.
        .arg("-volume")
        .arg("100");
    if (speed - 1.0).abs() > 1e-9 {
        command.arg("-af").arg(format!("atempo={speed}"));
    }
    if start_ms > 0 {
        command.arg("-ss").arg(format_secs(start_ms));
    }
    if !local {
        command.arg("-headers").arg(MEDIA_HEADERS);
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| {
            crate::job::assign(&child);
            // The audio clock is the player's master clock: it owes a sample
            // every ~20 ms, so it is a deadline task as well (see
            // `NORMAL_PRIORITY_CLASS`).
            raise_process_priority(child.id());
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
    silent: bool,
    control: &Arc<PlaybackControl>,
) -> Option<AudioProc> {
    // Claim this process' clock generation before spawning, so every sample it
    // emits is attributed to this process and the replaced one cannot write
    // past the seek/restart that retired it.
    let generation = control.begin_audio_clock_generation();
    control.note_audio_spawn();
    control
        .audio_hold_silent
        .store(silent, Ordering::SeqCst);
    let mut child = spawn_audio(url, start_ms, speed, false).ok()?;
    let pid = child.id();
    control.set_audio_pid(pid);
    crate::debug_log(&format!(
        "spawn ffplay pid={pid} start_ms={start_ms} speed={speed:.2} volume={} silent={}",
        if silent { 0 } else { 100 },
        u8::from(silent)
    ));
    let reader = child.stderr.take().map(|pipe| {
        let sink = AudioStatsSink::Session {
            control: control.clone(),
            generation,
        };
        std::thread::spawn(move || {
            note_thread_role("audio-stats");
            read_audio_stats(pipe, sink)
        })
    });
    Some(AudioProc { child, reader })
}

// ========================== seek shadow ==========================
//
// A seek used to tear the decode session down (`§1`): the pacer broke out, the
// decoder and the sound were killed, and a fresh `ffmpeg` + `ffplay` paid their
// startup again — ~1.2 s of nothing on screen, and a sound that arrived after
// the picture because its own warm-up started when the new picture did.
//
// The shadow is the startup discipline applied to a seek. On the release the
// running session keeps presenting its picture, and a **second** session is
// started at the target in the background: its decoder pre-rolls
// [`PRE_ROLL_START_FRAMES`] frames, its `ffplay` is spawned at the target,
// muted at the WASAPI session and frozen on the target position. The two
// streams are handed over in one instant — the shadow's first frame is pushed
// and the sound is unmuted in the same breath — and only then is the old
// session joined and its cache handle released. If the shadow is not ready
// within [`SEEK_SHADOW_TIMEOUT_MS`] the seek falls back to the old hard
// restart, so a seek always lands.

/// The `ffplay` position of a seek shadow, kept *out* of the player's clock
/// while the old picture still plays.
///
/// The player has exactly one master clock, and it belongs to the session on
/// screen. A shadow's samples must not touch it (`§1`): the pacer of the old
/// session would start pacing against the position the user asked for. They are
/// collected here and handed over in [`Self::attach`], which is the swap.
struct ShadowClock {
    ms: AtomicU64,
    stamp_ms: AtomicU64,
    samples: AtomicU64,
    /// Set at the swap: from then on this process *is* the session's sound, and
    /// its samples go to the player's clock under the generation claimed there.
    attached: std::sync::Mutex<Option<(Arc<PlaybackControl>, u64)>>,
    /// Monotonic ms the process was handed to the player's clock, and the
    /// content position it was frozen at (see [`SHADOW_RESUME_QUIET_MS`]).
    attached_at_ms: AtomicU64,
    frozen_ms: AtomicU64,
}

impl ShadowClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ms: AtomicU64::new(0),
            stamp_ms: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            attached: std::sync::Mutex::new(None),
            attached_at_ms: AtomicU64::new(0),
            frozen_ms: AtomicU64::new(0),
        })
    }

    /// One reported position. Returns whether it was the process' first.
    fn publish(&self, ms: u64) -> bool {
        let first = self.samples.fetch_add(1, Ordering::SeqCst) == 0;
        self.ms.store(ms, Ordering::SeqCst);
        let now = monotonic_ms().max(1);
        self.stamp_ms.store(now, Ordering::SeqCst);
        let guard = match self.attached.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((control, generation)) = guard.as_ref() {
            let quiet = now.saturating_sub(self.attached_at_ms.load(Ordering::SeqCst))
                <= SHADOW_RESUME_QUIET_MS;
            let device_jump = ms
                > self
                    .frozen_ms
                    .load(Ordering::SeqCst)
                    .saturating_add(AUDIO_CLOCK_JUMP_MS);
            if !(quiet && device_jump) {
                control.set_audio_clock_ms(*generation, ms);
            }
        }
        first
    }

    /// The newest reported position, without extrapolation.
    fn last(&self) -> Option<u64> {
        if self.samples.load(Ordering::SeqCst) == 0 {
            return None;
        }
        Some(self.ms.load(Ordering::SeqCst))
    }

    /// Where the process is at this instant (`-stats` bursts are hundreds of ms
    /// apart, so the last sample alone would be a stale freeze point).
    fn position(&self, speed: f64) -> u64 {
        let ms = self.ms.load(Ordering::SeqCst);
        let stamp = self.stamp_ms.load(Ordering::SeqCst);
        if stamp == 0 {
            return ms;
        }
        let age = monotonic_ms().saturating_sub(stamp);
        ms.saturating_add((age as f64 * speed) as u64)
    }

    /// Hand this process to the player's clock: the swap. Returns the generation
    /// the player's clock now accepts for it.
    fn attach(&self, control: &Arc<PlaybackControl>, ms: u64) -> u64 {
        let generation = control.begin_audio_clock_generation();
        self.frozen_ms.store(ms, Ordering::SeqCst);
        self.attached_at_ms.store(monotonic_ms().max(1), Ordering::SeqCst);
        let mut guard = match self.attached.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some((control.clone(), generation));
        drop(guard);
        // The frozen position is the content the first shadow frame shows, so
        // the session starts with a live, honest clock even before the resumed
        // process reports its next sample.
        control.set_audio_clock_ms(generation, ms);
        generation
    }
}

/// The shadow's `ffplay`: spawned at the target, muted at its WASAPI session
/// (the warm-up must never be heard) and frozen on the target once its clock
/// agrees with it.
struct ShadowAudio {
    proc: AudioProc,
    pid: u32,
    clock: Arc<ShadowClock>,
    /// The session accepted the mute: nothing of the warm-up can be heard.
    muted: bool,
    /// Suspended on the latched position: it cannot run ahead of a picture that
    /// is still the one on screen.
    suspended: bool,
    /// The one silent restart used when the first sample landed ahead of the
    /// target (a seek's keyframe offset), like the startup handover.
    restarted: bool,
    spawn_at: Instant,
    spawn_ms: u64,
    /// Monotonic age of the first position sample (the shadow's A/V debt).
    first_clock_after_ms: Option<u64>,
}

impl ShadowAudio {
    fn stop(self) {
        self.proc.stop();
    }

    /// Spawn `ffplay` for the shadow. Deliberately does **not** touch the
    /// player's clock, pid or mute state: the session on screen owns them until
    /// the swap.
    fn start(url: &str, start_ms: u64, speed: f64, local: bool) -> Option<Self> {
        let clock = ShadowClock::new();
        let spawn_at = Instant::now();
        let spawn_ms = monotonic_ms();
        let mut child = spawn_audio(url, start_ms, speed, local).ok()?;
        let pid = child.id();
        let reader = child.stderr.take().map(|pipe| {
            let sink = AudioStatsSink::Shadow(clock.clone());
            std::thread::spawn(move || {
                note_thread_role("shadow-audio");
                read_audio_stats(pipe, sink);
            })
        });
        crate::debug_log(&format!(
            "seek shadow ffplay pid={pid} target_ms={start_ms} speed={speed:.2} silent=1 local={}",
            u8::from(local)
        ));
        Some(Self {
            proc: AudioProc { child, reader },
            pid,
            clock,
            muted: false,
            suspended: false,
            restarted: false,
            spawn_at,
            spawn_ms,
            first_clock_after_ms: None,
        })
    }
}

/// One poll's verdict for a pre-rolling seek shadow.
enum SeekStep {
    /// Keep the old picture: the shadow still needs frames or its sound.
    Wait,
    /// Everything is in place: the pacer may swap now.
    Ready,
    /// The shadow cannot serve this seek: fall back to the hard restart.
    Failed(&'static str),
}

/// A decode session pre-rolling at a seek target while the current one keeps
/// presenting.
///
/// The struct owns everything the next session needs, so the swap is a move and
/// not a restart: the pre-rolled ring, the decoder (and its stderr drain) and
/// the positioned, muted `ffplay`. Nothing here publishes to [`PlaybackControl`]
/// except the audio mute, so the session on screen is untouched.
struct SeekShadow {
    /// The player this pre-roll belongs to: `poll` reads its audio proxy for
    /// the pre-warm and the swap receipts go through it.
    control: Arc<PlaybackControl>,
    target_ms: u64,
    /// The seek generation this shadow was started for (a newer one replaces it).
    seq: u64,
    started: Instant,
    /// Monotonic ms just before its decoder was spawned (first-frame cost).
    spawn_ms: u64,
    /// Monotonic ms of the release, for the latency line.
    release_at_ms: u64,
    local_play: bool,
    size: FrameSize,
    /// Ring capacity the adopted session keeps.
    capacity: usize,
    /// Ring frames the swap waits for.
    target_frames: usize,
    /// The playurl has no audio at all: there is nothing to wait for.
    no_audio: bool,
    audio_url: Option<String>,
    /// The sound opens the audio cache file (no upstream bytes) rather than
    /// the proxy/CDN.
    audio_local_play: bool,
    /// Byte offset the audio proxy was pre-warmed at (`None` until the stream
    /// length is known), for the `seek shadow audio prime` line.
    audio_prime_at: Option<u64>,
    /// Media duration in ms, for the audio byte-offset estimate.
    media_ms: u64,
    video_child: Option<Child>,
    stderr: Option<std::thread::JoinHandle<String>>,
    source: Option<FrameSource>,
    audio: Option<ShadowAudio>,
    /// Ring frames this shadow has decoded (from the source's own ring).
    ring: usize,
    /// Monotonic ms at which the ring first held [`SeekShadow::target_frames`]:
    /// the picture's half of the swap, separate from the sound's, so a receipt
    /// says which of the two gated the wait.
    ring_ready_ms: Option<u64>,
    /// Monotonic age of the shadow's first decoded frame (its first-frame cost).
    first_frame_ms: Option<u64>,
    /// The sound is latched on the target (and frozen there).
    audio_ready: bool,
    /// The position the sound was frozen at — the value the session's clock is
    /// seeded with at the swap.
    audio_frozen_ms: u64,
    /// The audio laid itself down by construction (`-ss target`) but never
    /// reported a position: the swap seeds the clock from the request.
    audio_unclocked: bool,
    /// The download map and upstream byte counter at the release (the
    /// `seek served` proof, measured for a swap exactly as for a restart).
    map_before: Option<(String, u64, u64)>,
    bytes_before: u64,
}

impl SeekShadow {
    /// Open the target: resolve the same stream the session would, decode it
    /// into a private ring and get the sound positioned and silent.
    #[allow(clippy::too_many_arguments)]
    fn start(
        control: &Arc<PlaybackControl>,
        info: &bilibili::PlayInfo,
        target_ms: u64,
        seq: u64,
        hwaccel: bool,
        speed: f64,
    ) -> Result<Self> {
        let (track, _exact) = session_track(control, info);
        let Some(track) = track else {
            return Err(anyhow!("未找到可用视频流"));
        };
        // The same evidence the hard restart logs: what the download map held
        // when the release was taken, and how many upstream bytes it cost.
        let map_before = control.prefetch_map_dump();
        let bytes_before = control
            .prefetch_raw_stats()
            .map(|stats| stats.bytes_fetched)
            .unwrap_or(0);
        // Same media decision as the session spawn: a target the cache file
        // holds is decoded locally, everything else goes through the proxy.
        let media = control.media_url_for(&track.url);
        let local_cache = control.prefetch_cache_file(target_ms);
        let (url, local_play) = match local_cache {
            Some(path) => (path.display().to_string(), true),
            None => (media, false),
        };
        let via = if local_play {
            "cache-file"
        } else if url.starts_with("http://127.0.0.1:") {
            "prefetch"
        } else {
            "direct"
        };
        // The session's own size decision, so the ring the shadow hands over is
        // the ring the pacer expects.
        let (mut display_w, mut display_h, _fullscreen) = control.display_size();
        if display_w == 0 || display_h == 0 {
            display_w = DEFAULT_DISPLAY_W;
            display_h = DEFAULT_DISPLAY_H;
        }
        let decode = target_frame_size(track.width, track.height);
        let size = reader_output_size(decode, (display_w, display_h), control.native_frame_size());
        let capacity = ring_capacity_for(size.bytes());
        let frame_ms = frame_ms_for(Some(track));
        let spawned_ms = monotonic_ms();
        let mut child = spawn_video(
            &url,
            target_ms,
            hwaccel,
            size,
            local_play,
            gpu_frame_scale(hwaccel, local_play, size, decode),
        )
        .map_err(|err| anyhow!("无法启动解码器：{err}"))?;
        let accept_duration = !local_play || target_ms == 0;
        let stderr = child.stderr.take().map(|pipe| {
            let control = control.clone();
            std::thread::spawn(move || {
                note_thread_role("shadow-stderr");
                read_video_stderr(pipe, control, accept_duration)
            })
        });
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                kill(&mut child);
                if let Some(handle) = stderr {
                    let _ = handle.join();
                }
                return Err(anyhow!("解码器未提供输出流"));
            }
        };
        let source = FrameSource::spawn_shadow(
            stdout,
            control.clone(),
            spawned_ms,
            size,
            frame_ms,
            capacity,
        );
        // ---- the sound: the session's audio proxy, primed at the target ------
        // One engine serves the whole player: its connection pool is already
        // warm and — for any stream a few dozen MB long — the bytes the sound
        // on screen is playing (and often the whole file) are already in its
        // window or in its session cache file. The pre-warm (`poll`) puts the
        // target into the point cache, which is served *before* the response
        // lock, so the seek's `ffplay` never has to displace the body the sound
        // on screen still has open.
        let proxy_url = control.audio_url_for(info);
        let cache_file = control.audio_cache_file();
        let audio_local_play = cache_file.is_some();
        let audio_url = match cache_file {
            Some(path) => Some(path.display().to_string()),
            None => proxy_url.clone(),
        };
        let audio_via = if audio_local_play {
            "cache-file"
        } else {
            match proxy_url.as_deref() {
                None => "none",
                Some(url) if url.starts_with("http://127.0.0.1:") => "prefetch",
                Some(_) => "direct",
            }
        };
        let audio = audio_url.as_deref().and_then(|url| {
            ShadowAudio::start(url, target_ms, speed, audio_local_play)
        });
        let no_audio = audio_url.is_none() || audio.is_none();
        let now = Instant::now();
        let started_ms = monotonic_ms();
        crate::debug_log(&format!(
            "seek shadow start target_ms={target_ms} via={via} local={} size={}x{} capacity={capacity} frames={} audio={} audio_via={audio_via} release_ms={} detect_ms={} setup_ms={}",
            u8::from(local_play),
            size.w,
            size.h,
            SEEK_SHADOW_PREROLL_FRAMES.min(capacity).max(1),
            u8::from(!no_audio),
            control.seek_requested_at_ms(),
            spawned_ms.saturating_sub(control.seek_requested_at_ms()),
            started_ms.saturating_sub(spawned_ms),
        ));
        Ok(Self {
            control: control.clone(),
            target_ms,
            seq,
            started: now,
            spawn_ms: spawned_ms,
            release_at_ms: control.seek_requested_at_ms(),
            local_play,
            size,
            capacity,
            target_frames: SEEK_SHADOW_PREROLL_FRAMES.min(capacity).max(1),
            no_audio,
            audio_url,
            audio_local_play,
            audio_prime_at: None,
            media_ms: info.duration.saturating_mul(1000),
            video_child: Some(child),
            stderr,
            source: Some(source),
            audio,
            ring: 0,
            ring_ready_ms: None,
            first_frame_ms: None,
            audio_ready: false,
            audio_frozen_ms: 0,
            audio_unclocked: false,
            map_before,
            bytes_before,
        })
    }

    /// One poll of the pre-roll. Never blocks: the old picture is being
    /// presented by the caller between two polls.
    fn poll(&mut self, speed: f64) -> SeekStep {
        let waited = self.started.elapsed();
        if waited >= Duration::from_millis(SEEK_SHADOW_TIMEOUT_MS) {
            return SeekStep::Failed("timeout");
        }
        let Some(source) = self.source.as_ref() else {
            return SeekStep::Failed("no-source");
        };
        let (ring, done, failed) = source.occupancy();
        self.ring = ring;
        if self.ring_ready_ms.is_none() && ring >= self.target_frames {
            self.ring_ready_ms = Some(waited.as_millis() as u64);
        }
        if self.first_frame_ms.is_none() {
            let at = source.first_frame_at_ms();
            if at != 0 {
                self.first_frame_ms = Some(at.saturating_sub(self.spawn_ms));
            }
        }
        if ring == 0 && (done || failed) {
            return SeekStep::Failed(if failed {
                "decoder-failed"
            } else {
                "decoder-ended"
            });
        }
        if done && ring < self.target_frames {
            return SeekStep::Failed("decoder-ended");
        }

        // ---- the sound: mute it, then freeze it on the target ----
        // The pre-warm of the shadow's audio proxy belongs in this poll: the
        // engine learns the stream length from its first upstream response, and
        // until then there is no byte offset to ask for. Priming *before*
        // `ffplay` runs its `-ss` is what makes that seek read locally; if the
        // client's own request lands first it does the same thing through
        // `Engine::seek`, so either order is correct.
        if self.audio_prime_at.is_none() && self.media_ms > 0 {
            if let Some((offset, total, covered)) = self.control.prime_audio_at(self.target_ms) {
                self.audio_prime_at = Some(offset);
                crate::debug_log(&format!(
                    "seek shadow audio prime offset={offset} total={total} target_ms={} covered={} waited_ms={}",
                    self.target_ms,
                    u8::from(covered),
                    self.started.elapsed().as_millis()
                ));
            }
        }
        if self.audio.is_some() {
            if let Some(audio) = self.audio.as_mut() {
                if !audio.muted
                    && crate::win::audio_session_mute(audio.pid, true)
                {
                    audio.muted = true;
                    crate::debug_log(&format!(
                        "seek shadow audio muted pid={} target_ms={} waited_ms={}",
                        audio.pid,
                        self.target_ms,
                        self.started.elapsed().as_millis()
                    ));
                }
                if audio.first_clock_after_ms.is_none() {
                    if let Some(ms) = audio.clock.last() {
                        audio.first_clock_after_ms = Some(
                            (monotonic_ms().saturating_sub(audio.spawn_ms)).max(1),
                        );
                        crate::debug_log(&format!(
                            "seek shadow audio clock first pos_ms={ms} after_ms={} target_ms={} delta_ms={}",
                            audio.first_clock_after_ms.unwrap_or(0),
                            self.target_ms,
                            ms as i64 - self.target_ms as i64
                        ));
                    }
                }
            }
            // A dead sound cannot be waited for: the picture must still land.
            // Checked every poll (not just before the first clock), because a
            // process that dies after latching leaves the swap without sound.
            if self.audio.as_mut().map_or(false, |audio| audio.proc.exited()) {
                if let Some(audio) = self.audio.take() {
                    crate::debug_log(&format!(
                        "seek shadow audio gone pid={} target_ms={} waited_ms={} latched={}",
                        audio.pid,
                        self.target_ms,
                        self.started.elapsed().as_millis(),
                        u8::from(self.audio_ready)
                    ));
                    audio.stop();
                }
                self.no_audio = true;
                self.audio_ready = true;
            } else if !self.audio_ready {
                match self.poll_audio(speed) {
                    Ok(()) => {}
                    Err(reason) => return SeekStep::Failed(reason),
                }
            }
        }

        let ring_ready = self.ring >= self.target_frames;
        // A local seek is picture-bound. `ffplay`'s first stats sample can be
        // 500–800 ms out even from a cache file; waiting for it would make the
        // sound the latency budget the six-frame pre-roll just removed. Once
        // the picture is ready, freeze the (muted) sound after a 40 ms grace
        // and seed the clock from the request — `-ss target` already placed it.
        // The sound must already be muted: suspending it before its WASAPI
        // session exists would prevent that session from ever being created,
        // and the swap would wait for a mute that can no longer happen.
        if ring_ready
            && self.local_play
            && !self.audio_ready
            && self.audio.as_ref().is_some_and(|audio| audio.muted)
            && self.started.elapsed() >= Duration::from_millis(SEEK_SHADOW_LOCAL_AUDIO_WAIT_MS)
        {
            let frozen = self
                .audio
                .as_ref()
                .and_then(|audio| audio.clock.last().map(|_| audio.clock.position(speed)))
                .filter(|ms| (*ms as i64 - self.target_ms as i64).abs() <= AUDIO_SEED_MAX_MS);
            let suspended = self.audio.as_mut().is_some_and(|audio| {
                if suspend_process(audio.pid, true) {
                    audio.suspended = true;
                    true
                } else {
                    false
                }
            });
            self.audio_unclocked = true;
            self.audio_ready = true;
            self.audio_frozen_ms = frozen.unwrap_or(self.target_ms);
            crate::debug_log(&format!(
                "seek shadow audio accept target_ms={} pos_ms={} suspended={} waited_ms={} reason=local_picture_bound",
                self.target_ms,
                self.audio_frozen_ms,
                u8::from(suspended),
                self.started.elapsed().as_millis()
            ));
        }
        // Never swap in a sound whose warm-up was not silenced: the pre-roll is
        // the old picture's time, and a leaking shadow would be heard over it.
        let silent = self.no_audio || self.audio.as_ref().map_or(true, |audio| audio.muted);
        if ring_ready && (self.no_audio || self.audio_ready) && silent {
            return SeekStep::Ready;
        }
        SeekStep::Wait
    }

    /// Decide whether the shadow's sound is positioned on the target.
    ///
    /// `Ok(())` when it is (and frozen), or when this poll could only wait. An
    /// `Err` retires the shadow: an audio process that could not be rebuilt is
    /// not something a seek may hang on.
    fn poll_audio(&mut self, speed: f64) -> std::result::Result<(), &'static str> {
        let Some(audio) = self.audio.as_mut() else {
            return Ok(());
        };
        let target = self.target_ms as i64;
        let waited = self.started.elapsed();
        if let Some(ms) = audio.clock.last() {
            let delta = ms as i64 - target;
            // Within the swap's lane, or the residual of the one silent restart
            // (a device that starts a little ahead): both are the same content
            // frame. Anything else is corrected at the source, once.
            let latched = delta.abs() <= SEEK_SHADOW_SYNC_MS
                || (audio.restarted && delta.abs() <= AUDIO_SEED_MAX_MS);
            if latched {
                // Freeze it here so a picture that is still the old one cannot
                // let the sound run ahead of the target.
                let frozen = audio.clock.position(speed);
                if suspend_process(audio.pid, true) {
                    audio.suspended = true;
                    self.audio_frozen_ms = frozen;
                    self.audio_ready = true;
                    crate::debug_log(&format!(
                        "seek shadow audio locked pos_ms={frozen} target_ms={} delta_ms={} waited_ms={} restarted={}",
                        self.target_ms,
                        frozen as i64 - target,
                        waited.as_millis(),
                        u8::from(audio.restarted)
                    ));
                } else if audio.restarted {
                    // Cannot freeze it: accept the measured position and let the
                    // session's own A/V guard own the relation from here.
                    self.audio_frozen_ms = frozen;
                    self.audio_ready = true;
                    crate::debug_log(&format!(
                        "seek shadow audio suspend=0 pos_ms={frozen} target_ms={} delta_ms={}",
                        self.target_ms,
                        frozen as i64 - target
                    ));
                } else {
                    return self.restart_audio(speed);
                }
                return Ok(());
            }
            if delta > 0 {
                if !audio.restarted {
                    // Ahead of the target (the seek's keyframe offset, or the
                    // device had already played): start it over at the target.
                    return self.restart_audio(speed);
                }
                // Still ahead after the restart: accept it rather than restart
                // in a loop; the guard and the seed window absorb the residual.
                let frozen = audio.clock.position(speed);
                self.audio_frozen_ms = frozen;
                self.audio_ready = true;
                crate::debug_log(&format!(
                    "seek shadow audio accept pos_ms={frozen} target_ms={} delta_ms={} restarted=1",
                    self.target_ms,
                    frozen as i64 - target
                ));
                return Ok(());
            }
            // Behind the target: the sound is playing forward toward it. Bound
            // the wait so a stream that never gets there still lands the seek.
            if waited < Duration::from_millis(SEEK_SHADOW_AUDIO_WAIT_MS) {
                return Ok(());
            }
        }
        if audio.spawn_at.elapsed() >= Duration::from_millis(SEEK_SHADOW_AUDIO_WAIT_MS) {
            // No usable sample: `-ss target` is the position by construction (the
            // process has not played a sample outside it), so the swap seeds the
            // clock from the request instead of hanging the seek on a sound that
            // reports nothing. Logged as `unclocked` — the A/V line then reports
            // the seeded relation, not a measured one.
            self.audio_unclocked = true;
            self.audio_frozen_ms = self.target_ms;
            self.audio_ready = true;
            crate::debug_log(&format!(
                "seek shadow audio unclocked target_ms={} waited_ms={} pid={} restarted={} clock={}",
                self.target_ms,
                waited.as_millis(),
                audio.pid,
                u8::from(audio.restarted),
                audio
                    .clock
                    .last()
                    .map(|ms| ms.to_string())
                    .unwrap_or_else(|| "-".to_string())
            ));
        }
        Ok(())
    }

    /// Replace the shadow's sound with a fresh, silent process at the target.
    fn restart_audio(&mut self, speed: f64) -> std::result::Result<(), &'static str> {
        let Some(url) = self.audio_url.clone() else {
            self.no_audio = true;
            self.audio_ready = true;
            return Ok(());
        };
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        let Some(mut audio) =
            ShadowAudio::start(&url, self.target_ms, speed, self.audio_local_play)
        else {
            // The process could not be rebuilt; the picture still has to land.
            self.no_audio = true;
            self.audio_ready = true;
            crate::debug_log(&format!(
                "seek shadow audio respawn failed target_ms={}",
                self.target_ms
            ));
            return Ok(());
        };
        audio.restarted = true;
        crate::debug_log(&format!(
            "seek shadow audio restart target_ms={} pid={} silent=1",
            self.target_ms, audio.pid
        ));
        self.audio = Some(audio);
        Ok(())
    }

    /// The swap receipt: everything the pacer logs at the first shadow frame.
    fn trace(&self) -> SwapTrace {
        SwapTrace {
            target_ms: self.target_ms,
            release_at_ms: self.release_at_ms,
            ready_at_ms: monotonic_ms(),
            shadow_first_frame_ms: self.first_frame_ms.unwrap_or(0),
            audio_first_clock_after_ms: self
                .audio
                .as_ref()
                .and_then(|audio| audio.first_clock_after_ms),
            audio_frozen_ms: self.audio_frozen_ms,
            audio_unclocked: self.audio_unclocked,
            ring: self.ring,
            target_frames: self.target_frames,
            ring_ready_ms: self.ring_ready_ms.unwrap_or(0),
            local_play: self.local_play,
            audio_local: self.audio_local_play
                || self
                    .audio_url
                    .as_deref()
                    .is_some_and(|url| url.starts_with("http://127.0.0.1:")),
            audio_prime_at: self.audio_prime_at,
            map_before: self.map_before.clone(),
            bytes_before: self.bytes_before,
            // f151: the overlay the release armed while this shadow pre-rolled.
            // It is still on screen at adoption (the session-end path keeps it
            // for a frozen seek) and the first new frame closes it.
            overlay: self.control.seek_buffering(),
        }
    }

    /// Kill everything the pre-roll owns. Idempotent: the adoption path takes
    /// the children out of the shadow, so the drop then finds nothing.
    fn release_children(&mut self) {
        if let Some(child) = self.video_child.as_mut() {
            kill(child);
        }
        if let Some(mut source) = self.source.take() {
            source.stop();
        }
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }
        if let Some(handle) = self.stderr.take() {
            let _ = handle.join();
        }
    }

    /// Kill the pre-roll and release everything it holds. The target is left
    /// to the caller: a fallback must still land the seek.
    fn dispose(self, reason: &str) {
        let waited = self.started.elapsed().as_millis() as u64;
        crate::debug_log(&format!(
            "seek shadow dispose reason={reason} target_ms={} elapsed_ms={waited} ring={} first_frame_ms={} audio_ready={}",
            self.target_ms,
            self.ring,
            self.first_frame_ms.unwrap_or(0),
            u8::from(self.audio_ready)
        ));
        // The children are released by `Drop` (the one place that owns them),
        // which also covers a shadow dropped by a closing window.
    }
}

impl Drop for SeekShadow {
    fn drop(&mut self) {
        self.release_children();
    }
}

/// The byte offset a seek's audio pre-warm starts at.
///
/// `length × position / duration` is the only mapping available before the
/// demuxer's index is read, and for a CBR DASH audio track it is within a
/// fraction of a percent. The backoff is one chunk so an estimate that lands
/// slightly late still finds the bytes the client will ask for.
fn audio_prime_offset(total: u64, target_ms: u64, duration_ms: u64) -> u64 {
    if total == 0 || duration_ms == 0 {
        return 0;
    }
    let estimate = (total as u128)
        .saturating_mul(target_ms as u128)
        .saturating_div(duration_ms as u128) as u64;
    estimate
        .saturating_sub(AUDIO_PRIME_BACKOFF_BYTES)
        .min(total.saturating_sub(1))
}

/// The receipt of one seek swap, logged at the first frame the adopted session
/// presents — the instant the user's target becomes visible.
struct SwapTrace {
    target_ms: u64,
    /// Monotonic ms of the release (the UI's own timestamp).
    release_at_ms: u64,
    /// Monotonic ms the shadow was declared ready.
    ready_at_ms: u64,
    /// Age of the shadow's first decoded frame.
    shadow_first_frame_ms: u64,
    /// Age of the shadow`ffplay`'s first reported position.
    audio_first_clock_after_ms: Option<u64>,
    /// The content position the sound was frozen at (the session clock's seed).
    audio_frozen_ms: u64,
    audio_unclocked: bool,
    ring: usize,
    target_frames: usize,
    /// Age of the release at which the ring first held `target_frames`.
    ring_ready_ms: u64,
    /// The target came from the cache file (no upstream bytes for its pre-roll).
    local_play: bool,
    /// The shadow's sound read its own audio proxy (not the CDN).
    audio_local: bool,
    /// Byte offset the audio proxy was pre-warmed at, when its length was known.
    audio_prime_at: Option<u64>,
    /// The download map and upstream byte counter at the release, so the
    /// `seek served` proof (map unchanged, zero upstream for a cached target)
    /// holds for a swap too.
    map_before: Option<(String, u64, u64)>,
    bytes_before: u64,
    overlay: bool,
}

/// The swap's proof line: release→visible frame, the A/V relation at that frame
/// and whether the sound was really audible.
///
/// It also emits the existing `seek served` line, so the seek contract the QA
/// runs already read (map unchanged, zero upstream bytes for a cached target)
/// holds for a shadow swap as well.
fn log_seek_swap(control: &PlaybackControl, swap: &SwapTrace, video_ms: i64, pushes: u64) {
    log_seek_served(
        control,
        &SeekWait {
            target: swap.target_ms,
            // f151: a shadow whose wait passed the silent window published the
            // overlay; closing it here measures the wait on the `seek served`
            // line and takes it down with the swap.
            published: swap.overlay,
            buffered: swap.local_play,
            deadline: Instant::now(),
            requested_at_ms: swap.release_at_ms,
            map_before: swap.map_before.clone(),
            bytes_before: swap.bytes_before,
        },
        pushes,
    );
    let visible_ms = monotonic_ms().saturating_sub(swap.release_at_ms);
    let clock = control.audio_clock();
    let delta = clock
        .map(|ms| ms as i64 - video_ms)
        .unwrap_or(video_ms - swap.target_ms as i64);
    let (level, mute) = crate::win::audio_session_get(control.audio_pid()).unwrap_or((0.0, true));
    crate::debug_log(&format!(
        "seek swap target_ms={} release_to_visible_ms={visible_ms} ready_ms={} ring_ready_ms={} shadow_first_frame_ms={} audio_first_ms={} audio_frozen_ms={} audio_clock_ms={} delta_ms={delta} audible={} level={level:.2} mute={} ring={}/{} pid={} unclocked={} local={} audio_local={} audio_prime={} overlay={}",
        swap.target_ms,
        swap.ready_at_ms.saturating_sub(swap.release_at_ms),
        swap.ring_ready_ms,
        swap.shadow_first_frame_ms,
        swap.audio_first_clock_after_ms
            .map(|ms| ms.to_string())
            .unwrap_or_else(|| "-".to_string()),
        swap.audio_frozen_ms,
        clock
            .map(|ms| ms.to_string())
            .unwrap_or_else(|| "-".to_string()),
        u8::from(!mute && level > 0.001),
        u8::from(mute),
        swap.ring,
        swap.target_frames,
        control.audio_pid(),
        u8::from(swap.audio_unclocked),
        u8::from(swap.local_play),
        u8::from(swap.audio_local),
        swap.audio_prime_at
            .map(|offset| offset.to_string())
            .unwrap_or_else(|| "-".to_string()),
        u8::from(swap.overlay),
    ));
}

/// The stream a session (or a seek shadow) opens: the exact 画质 pick when there
/// is one, the automatic highest-bandwidth rule otherwise. Shared so a shadow
/// can never pre-roll a different stream than the session it replaces.
fn session_track<'a>(
    control: &PlaybackControl,
    info: &'a bilibili::PlayInfo,
) -> (Option<&'a bilibili::VideoTrack>, bool) {
    let exact = control.exact_quality().and_then(|(qn, codecid, bandwidth)| {
        info.video_tracks.iter().find(|candidate| {
            candidate.id == qn
                && candidate.codecid == codecid
                && candidate.bandwidth == bandwidth
        })
    });
    let exact_hit = exact.is_some();
    (
        exact.or_else(|| auto_track_for(info, control.quality(), control.codec_pref())),
        exact_hit,
    )
}

/// Milliseconds per decoded frame for a stream (`30 fps` when the manifest omits
/// a usable rate). Shared by the session and its shadow.
fn frame_ms_for(track: Option<&bilibili::VideoTrack>) -> f64 {
    track
        .map(|track| track.frame_rate)
        .filter(|rate| *rate > 0.0 && *rate <= 240.0)
        .map(|rate| 1000.0 / rate)
        .unwrap_or(1000.0 / 30.0)
}

/// Where an `ffplay`'s reported position goes.
///
/// A session's process writes the master clock of the player. A seek shadow's
/// process must **not** (`§1`): it is decoding the position the user asked for
/// while the old picture is still being presented against the old clock, so its
/// samples are collected in a [`ShadowClock`] and only handed to the player's
/// clock in the instant the shadow is adopted.
enum AudioStatsSink {
    Session {
        control: Arc<PlaybackControl>,
        generation: u64,
    },
    Shadow(Arc<ShadowClock>),
}

impl AudioStatsSink {
    /// Accept one reported position; returns whether it was the process' first.
    fn accept(&self, ms: u64) -> bool {
        match self {
            Self::Session {
                control,
                generation,
            } => control.set_audio_clock_ms(*generation, ms),
            Self::Shadow(clock) => clock.publish(ms),
        }
    }
}

/// Drain ffplay's stderr and keep the newest playback position in `control`.
///
/// `-stats` emits `\r`-separated status lines whose first token is the current
/// position in seconds (e.g. `  12.34 A-V: ...` or `M-A:`); everything else on
/// stderr is ignored.
fn read_audio_stats(mut pipe: impl Read, sink: AudioStatsSink) {
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
                    apply_audio_stats(&segment, &sink);
                }
                if pending.len() > 8192 {
                    let segment = std::mem::take(&mut pending);
                    apply_audio_stats(&segment, &sink);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Parse one ffplay status segment; `nan`/negative/absent positions are skipped.
fn apply_audio_stats(segment: &[u8], sink: &AudioStatsSink) {
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
            let ms = (seconds * 1000.0).round() as u64;
            // One line per ffplay process: how long it took to report a real
            // position. That latency is the startup A/V debt (the picture runs
            // while the sound has not started), so it is worth a log line.
            if sink.accept(ms) {
                if let AudioStatsSink::Session {
                    control,
                    generation,
                } = sink
                {
                    if let Some(first_ms) = control.audio_first_sample_ms(*generation) {
                        crate::debug_log(&format!(
                            "audio clock first pos_ms={ms} after_ms={first_ms}"
                        ));
                    }
                }
            }
        }
    }
}

/// Parse ffmpeg's own `Duration: HH:MM:SS.cc` line into milliseconds.
///
/// The line is part of the input dump ffmpeg prints at info level, before the
/// first frame, e.g. `  Duration: 00:01:24.00, start: 0.000000, bitrate: 2049
/// kb/s`. `N/A` (a live or fragmented input with no declared length), a
/// missing field or a malformed one all yield `None`, and the API value stays
/// in force.
fn parse_ffmpeg_duration(line: &str) -> Option<u64> {
    let rest = line.split("Duration:").nth(1)?;
    let token = rest.split(',').next()?.trim();
    let mut parts = token.split(':');
    let hours: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;
    if !(hours.is_finite() && minutes.is_finite() && seconds.is_finite()) {
        return None;
    }
    if hours < 0.0 || minutes < 0.0 || seconds < 0.0 {
        return None;
    }
    let ms = ((hours * 3600.0 + minutes * 60.0 + seconds) * 1000.0).round();
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    Some(ms as u64)
}

/// Drain a video decoder's stderr on its own thread.
///
/// Two jobs, both from the same pipe:
///
/// * publish the process' own `Duration:` for this session (see
///   [`parse_ffmpeg_duration`]); the line arrives in the first read, long
///   before EOF, so the length is known while the session plays rather than
///   only when it ends;
/// * keep the **tail** for the `decode … stderr=` line, so a failure still
///   reports the actual error even though the banner is now at info level.
fn read_video_stderr(
    mut pipe: impl Read,
    control: Arc<PlaybackControl>,
    accept_duration: bool,
) -> String {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(&mut pipe);
    let mut line = String::new();
    let mut tail: VecDeque<String> = VecDeque::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let text = line.trim_end();
                if accept_duration && control.decoder_duration_ms.load(Ordering::SeqCst) == 0 {
                    if let Some(ms) = parse_ffmpeg_duration(text) {
                        control.set_decoder_duration_ms(ms, accept_duration);
                    }
                }
                if !text.trim().is_empty() {
                    tail.push_back(text.to_string());
                    if tail.len() > 8 {
                        tail.pop_front();
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    tail.into_iter().collect::<Vec<_>>().join("\n")
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
        // `BILI_STALL_HOLD_MS` shortens the hold for the auto-pause/resume QA,
        // where the buffer guard (not the watchdog) is what recovers.
        let hold_ms = std::env::var("BILI_STALL_HOLD_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(6_000);
        std::thread::sleep(Duration::from_millis(hold_ms));
        let resume = unsafe { NtResumeProcess(handle) };
        crate::debug_log(&format!(
            "stall-test resume pid={pid} status={resume} hold_ms={hold_ms}"
        ));
        unsafe { CloseHandle(handle) };
    });
}


// ---------------------------------------------------------------------------
// 直播 (live playback)
//
// A live room is NOT a DASH session: there is no manifest, no duration, no
// seek, no prefetch and no `PlayInfo`. It therefore gets its own, separate
// pipeline next to the VOD one and shares only what is media-agnostic:
//
//   ffmpeg -re -i <hls/flv>  -> raw NV12 frames -> same `FrameMailbox`
//   ffplay  -nodisp <same url>               -> the sound
//
// The video pipe is paced by `-re` (native read rate) and by ffmpeg's own
// backpressure on the rawvideo pipe, so a burst of HLS segments cannot
// fast-forward the picture; the mailbox coalesces anything the UI cannot
// present. `PlaybackControl` still owns pause / volume / mute (the live arm
// only reads `paused` and `audio_seq`), and closing the window kills both
// children through `control.is_closed()`.
//
// Nothing here is reachable from `run` / `decode_loop`: a VOD window never
// creates a `LiveSource`, and a live window never fetches a `PlayInfo`.
// ---------------------------------------------------------------------------

/// What the live window needs to know about its room.
#[derive(Clone, Debug, Default)]
pub struct LiveSource {
    pub room_id: i64,
    pub title: String,
    pub uname: String,
    pub area: String,
    /// B站 tells us in `get_info`; a vertical room gets a 720x1280 canvas.
    pub is_portrait: bool,
    /// The room's 头图 (the index card's cover, or `get_info`'s `keyframe`). It
    /// is the poster the window shows while nothing has been decoded yet.
    pub cover: String,
}

impl LiveSource {
    fn frame_size(&self) -> FrameSize {
        if self.is_portrait {
            FrameSize { w: 720, h: 1280 }
        } else {
            FrameSize { w: 1280, h: 720 }
        }
    }
}

/// Live counters for the QA hooks (frames delivered, first-frame latency, the
/// reconnect count and the last `live_status` the room API reported).
#[derive(Default)]
pub struct LiveStats {
    pub frames: AtomicU64,
    pub first_frame_ms: AtomicU64,
    pub reconnects: AtomicU64,
    pub live_status: AtomicI64,
    /// One entry per attempt: `url_kind`, the API wait and whether the decoder
    /// produced a frame. Read by `BILI_LIVE_TEST`.
    pub attempts: std::sync::Mutex<Vec<String>>,
}

impl LiveStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> (u64, u64, u64, i64) {
        (
            self.frames.load(Ordering::SeqCst),
            self.first_frame_ms.load(Ordering::SeqCst),
            self.reconnects.load(Ordering::SeqCst),
            self.live_status.load(Ordering::SeqCst),
        )
    }
}

const LIVE_REFERER: &str = "https://live.bilibili.com/";
const LIVE_HEADERS: &str = "Referer: https://live.bilibili.com/\r\nUser-Agent: Mozilla/5.0 \
                            (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like \
                            Gecko) Chrome/125.0.0.0 Safari/537.36\r\n";

/// The live window's two children, reachable from the watchdog that must kill
/// them on close / pause even while the pump thread is blocked on the pipe.
#[derive(Default)]
struct LiveChildren {
    video: Option<Child>,
    audio: Option<Child>,
    /// Bumped for every attempt. A first-frame deadline armed by attempt N must
    /// never kill the child of attempt N+1 (the deadlines outlive their pump).
    generation: u64,
}

/// Characters of decoder stderr kept in one log line (the task's 2 KB budget).
const LIVE_STDERR_LIMIT: usize = 2048;

/// Flatten a decoder's stderr into one log-safe line: whitespace collapsed, at
/// most [`LIVE_STDERR_LIMIT`] characters kept from the END (the last lines carry
/// the reason). `BILI_DEBUG_LOG` is written once per line, so a raw multi-line
/// capture would interleave with the other threads.
fn live_stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let len = flat.chars().count();
    if len > LIVE_STDERR_LIMIT {
        let cut = flat
            .char_indices()
            .nth(len - LIVE_STDERR_LIMIT)
            .map(|(at, _)| at)
            .unwrap_or(0);
        format!("…{}", &flat[cut..])
    } else {
        flat
    }
}

/// The bounded window of one decoder's stderr, shared between the draining
/// thread and the first-frame deadline.
#[derive(Default)]
struct LiveStderr {
    bytes: usize,
    buf: Vec<u8>,
}

impl LiveStderr {
    /// Append a chunk, keeping at most four windows (a decoder that screams for
    /// minutes must not grow this buffer without limit).
    fn push(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len();
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > 4 * LIVE_STDERR_LIMIT {
            let drop = self.buf.len() - 2 * LIVE_STDERR_LIMIT;
            self.buf.drain(..drop);
        }
    }

    fn tail(&self) -> String {
        live_stderr_tail(&self.buf)
    }
}

/// Drain the child's stderr on its own thread, keeping a bounded window. The
/// bytes are shared instead of only logged at EOF: when an attempt is killed
/// for producing no frame, the deadline log is the only place that can show
/// what the decoder was saying while it stalled.
fn live_drain_stderr(
    mut stderr: std::process::ChildStderr,
    kind: &'static str,
    sink: Arc<Mutex<LiveStderr>>,
) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut sink) = sink.lock() {
                        sink.push(&buf[..n]);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let (bytes, tail) = match sink.lock() {
            Ok(sink) => (sink.bytes, sink.tail()),
            Err(_) => (0, String::new()),
        };
        crate::debug_log(&format!("live ffmpeg stderr kind={kind} bytes={bytes} tail={tail}"));
    });
}

fn spawn_live_video(
    url: &str,
    size: FrameSize,
    kind: &'static str,
    sink: &Arc<Mutex<LiveStderr>>,
) -> std::io::Result<Child> {
    let mut command = base_command("ffmpeg.exe");
    // `fps=30` makes the pipe a fixed 30 Hz grid: the reader paces it against
    // the wall clock, so a 60 fps source and a burst of HLS segments both play
    // in real time instead of racing ahead (measured without it: 34.3 fps
    // presented over 30 s, i.e. ~14 % fast).
    let filter = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps=30",
        w = size.w,
        h = size.h
    );
    command
        .arg("-hide_banner")
        // `warning`: the open/read/protocol diagnostics that explain "spawned
        // but no frame" never appear at `error`. The tail is bounded in
        // [`live_drain_stderr`], so the volume is controlled there.
        .arg("-loglevel")
        .arg("warning")
        .arg("-nostats")
        .arg("-nostdin")
        .arg("-threads")
        .arg("0")
        .arg("-fflags")
        .arg("+discardcorrupt")
        // No `-re`: the reader paces the pipe at 30 fps (see `live_pump`), and
        // ffmpeg's own write blocks once ~one frame is queued, which is the
        // cushion that absorbs a segment arriving in a burst.
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        // A dead rendition must fail fast: at 15 s a bad HLS URL held the
        // window on 缓冲 for that whole timeout before FLV was tried
        // (measured click→first frame 19.5 s; 7.3 s after this bound). 5 s now:
        // a live read that makes no progress for that long is not coming back,
        // and the first-frame budget below is the outer bound anyway.
        .arg("-rw_timeout")
        .arg("5000000")
        .arg("-headers")
        .arg(LIVE_HEADERS)
        .arg("-i")
        .arg(url)
        .arg("-an")
        .arg("-vf")
        .arg(filter)
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg(if pipe_is_nv12() { "nv12" } else { "rgba" })
        .arg("-");
    crate::debug_log(&format!("live spawn ffmpeg {w}x{h} kind={kind}", w = size.w, h = size.h));
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stderr) = child.stderr.take() {
        live_drain_stderr(stderr, kind, sink.clone());
    }
    crate::job::assign(&child);
    raise_process_priority(child.id());
    Ok(child)
}

/// Local relay for the audio child.
///
/// ffplay on this machine cannot read a remote live body directly: against
/// `api.live.bilibili.com`'s CDN it opens the TCP/TLS connection, sends the GET
/// and then never receives a body (measured: `nan` position, `aq=0` for 30 s,
/// while ffmpeg/ffprobe read the same URL in ~2 s). Serving the same bytes from
/// `127.0.0.1` plays immediately, which is exactly how the VOD audio child
/// already works (it reads the local prefetch proxy). So the live sound gets a
/// one-connection relay: reqwest pulls the stream, the socket hands it to
/// ffplay. The video child keeps reading the CDN directly.
struct LiveRelay {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl LiveRelay {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/live", self.port)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn start_live_relay(session: Arc<bilibili::Session>, source: String) -> Option<LiveRelay> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        // ffplay connects within a few hundred ms; give it 10 s and then let
        // the listener go so a failed spawn cannot leak a thread.
        let _ = listener.set_nonblocking(true);
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            if stop_thread.load(Ordering::SeqCst) {
                return;
            }
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        crate::debug_log("live relay accept timeout");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(60));
                }
                Err(_) => return,
            }
        };
        let _ = socket.set_nonblocking(false);
        let _ = socket.set_nodelay(true);
        let mut socket = socket;
        let mut head = [0u8; 4096];
        let _ = socket.read(&mut head);
        if socket
            .write_all(b"HTTP/1.1 200 OK
Content-Type: video/x-flv
Connection: close

")
            .is_err()
        {
            return;
        }
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        let result = runtime.block_on(async move {
            let mut response = session.open_stream(&source, LIVE_REFERER).await?;
            let mut bytes = 0u64;
            while let Some(chunk) = response.chunk().await? {
                if stop_thread.load(Ordering::SeqCst) {
                    break;
                }
                socket.write_all(&chunk)?;
                bytes += chunk.len() as u64;
            }
            crate::debug_log(&format!("live relay done bytes={bytes}"));
            Ok::<(), anyhow::Error>(())
        });
        if let Err(err) = result {
            crate::debug_log(&format!("live relay err={err:#}"));
        }
    });
    Some(LiveRelay { port, stop })
}

fn spawn_live_audio(url: &str) -> std::io::Result<Child> {
    let mut command = base_command("ffplay.exe");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("quiet")
        .arg("-stats")
        .arg("-nodisp")
        .arg("-vn")
        .arg("-autoexit")
        .arg("-rw_timeout")
        .arg("15000000")
        .arg("-volume")
        .arg("100");
    // A localhost relay URL needs no referer / reconnect options; a direct
    // CDN URL (only used if the relay could not bind) keeps them.
    if !url.starts_with("http://127.0.0.1") {
        command
            .arg("-reconnect")
            .arg("1")
            .arg("-reconnect_streamed")
            .arg("1")
            .arg("-reconnect_delay_max")
            .arg("5")
            .arg("-headers")
            .arg(LIVE_HEADERS);
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn()?;
    crate::job::assign(&child);
    Ok(child)
}

/// Spawn the live playback thread for one window. `mini` mirrors the picture
/// into the PiP, exactly like the VOD mailbox does.
pub fn spawn_live(
    session: Arc<bilibili::Session>,
    source: LiveSource,
    window: slint::Weak<PlayerWindow>,
    mini: MiniSlot,
    control: Arc<PlaybackControl>,
    stats: Arc<LiveStats>,
) {
    install_panic_log();
    std::thread::spawn(move || {
        note_thread_role("live");
        let pool = Arc::new(BlendPool::new());
        let mailbox = Arc::new(FrameMailbox::new(
            window.clone(),
            mini.clone(),
            pool.clone(),
            control.clone(),
        ));
        let children: Arc<std::sync::Mutex<LiveChildren>> =
            Arc::new(std::sync::Mutex::new(LiveChildren::default()));
        live_watchdog(control.clone(), children.clone());
        live_room_info_loop(session.clone(), source.clone(), window.clone(), control.clone(), stats.clone());
        live_run(session, source, window, control, stats, mailbox, children);
    });
}

/// Kill the children on close and on pause. Runs on its own thread so a blocked
/// `read_exact` on the rawvideo pipe is interrupted by the child going away.
fn live_watchdog(control: Arc<PlaybackControl>, children: Arc<std::sync::Mutex<LiveChildren>>) {
    std::thread::spawn(move || loop {
        let closed = control.is_closed();
        let paused = control.paused();
        if closed || paused {
            if let Ok(mut guard) = children.lock() {
                if let Some(mut child) = guard.video.take() {
                    kill(&mut child);
                }
                if let Some(mut child) = guard.audio.take() {
                    kill(&mut child);
                }
            }
            if closed {
                crate::debug_log("live watchdog children killed reason=closed");
                return;
            }
        } else {
            // The WASAPI gain/mute is owed to every new ffplay; the control
            // retries it here at its own bounded rate.
            control.sync_audio_session();
        }
        std::thread::sleep(Duration::from_millis(120));
    });
}

fn live_set_viewers(window: &slint::Weak<PlayerWindow>, online: u64, status: i64) {
    let window = window.clone();
    let text: slint::SharedString = format!("{} 人气", format_viewers(online)).into();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_live_viewers(text);
            window.set_live_online(online.min(i32::MAX as u64) as i32);
            window.set_live_status(status as i32);
        }
    });
}

/// Every 30 s: refresh the viewer count and the live status. Text only — the
/// stream is never restarted for this.
fn live_room_info_loop(
    session: Arc<bilibili::Session>,
    source: LiveSource,
    window: slint::Weak<PlayerWindow>,
    control: Arc<PlaybackControl>,
    stats: Arc<LiveStats>,
) {
    std::thread::spawn(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        loop {
            if control.is_closed() {
                return;
            }
            match runtime.block_on(session.live_room_info(source.room_id)) {
                Ok(info) => {
                    stats.live_status.store(info.live_status, Ordering::SeqCst);
                    live_set_viewers(&window, info.online, info.live_status);
                    crate::debug_log(&format!(
                        "live room info room={} status={} online={} title={} area={}",
                        info.room_id, info.live_status, info.online, info.title, info.area_name
                    ));
                }
                Err(err) => crate::debug_log(&format!(
                    "live room info err room={} err={err:#}",
                    source.room_id
                )),
            }
            for _ in 0..150 {
                if control.is_closed() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    });
}

/// One delivery to try for a room, in the order this client prefers them.
///
/// Measured against room 27579539 on this machine (see the `live session` /
/// `live ffmpeg stderr` lines): FLV opens and delivers its first frame in
/// 2.7-4.1 s; HLS-fmp4 connects but produced no frame inside 8 s
/// (`Found duplicated MOOV Atom. Skipped it`); the HLS-ts rendition never opens
/// at all (`tls ... Error in the pull function`, `Error number -138`). So FLV is
/// first, fmp4 second, ts last - and each one only gets
/// [`LIVE_FIRST_FRAME_MS`].
fn live_candidates(play: &bilibili::LivePlay) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for (kind, url) in [
        ("flv", play.flv.as_ref()),
        ("hls_fmp4", play.hls_fmp4.as_ref()),
        ("hls_ts", play.hls_ts.as_ref()),
    ] {
        if let Some(url) = url {
            if url.len() > 12 {
                out.push((kind, url.clone()));
            }
        }
    }
    if out.is_empty() {
        // A payload that only carries the merged field: keep the room playable
        // rather than spinning on "no source".
        if let Some(url) = play.hls.as_ref() {
            if url.len() > 12 {
                out.push(("hls", url.clone()));
            }
        }
    }
    out
}

/// `BILI_LIVE_FORCE=flv|hls_fmp4|hls_ts|hls` makes that delivery the one the
/// session starts on (the others stay behind it as fallbacks). The "bad source"
/// QA run proves a dead rendition is abandoned inside one budget instead of
/// freezing the window.
fn live_forced_kind() -> Option<&'static str> {
    static FORCED: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| {
        let value = std::env::var("BILI_LIVE_FORCE").ok()?;
        match value.trim().to_ascii_lowercase().as_str() {
            "flv" => Some("flv"),
            "hls_fmp4" | "hls-fmp4" | "fmp4" => Some("hls_fmp4"),
            "hls_ts" | "hls-ts" | "ts" => Some("hls_ts"),
            "hls" => Some("hls"),
            _ => None,
        }
    })
}

/// How long one delivery may take to produce its FIRST frame before the session
/// rotates to the next one. A working FLV start is measured at 2.7-4.1 s, so 8 s
/// still admits a slow but real start while cutting a frozen source off in one
/// budget instead of the 20 s+ a blocked `read_exact` used to cost.
const LIVE_FIRST_FRAME_MS: u64 = 8_000;
/// Gap between two deliveries of the same room. A source rotation is not a
/// network failure, so it does not consume the reconnect backoff.
const LIVE_SWITCH_MS: u64 = 150;

/// Kill the attempt's decoder when no frame arrived within the budget.
///
/// `live_pump` blocks in `read_exact` on the rawvideo pipe, so a source that
/// connects but delivers nothing never returns on its own (that is exactly the
/// reported "spawned ffmpeg, then 20 s of nothing, no attempt=3"). Killing the
/// child closes the pipe, which returns the pump to the rotation - the same
/// mechanism `live_watchdog` uses for close/pause. `generation` guarantees this
/// deadline can only ever kill its own attempt's child.
fn live_arm_frame_deadline(
    children: Arc<std::sync::Mutex<LiveChildren>>,
    control: Arc<PlaybackControl>,
    first_frame: Arc<AtomicBool>,
    sink: Arc<Mutex<LiveStderr>>,
    generation: u64,
    kind: &'static str,
) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let budget = Duration::from_millis(LIVE_FIRST_FRAME_MS);
        while started.elapsed() < budget {
            if first_frame.load(Ordering::SeqCst) || control.is_closed() {
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        if first_frame.load(Ordering::SeqCst) || control.is_closed() {
            return;
        }
        let (bytes, tail) = match sink.lock() {
            Ok(sink) => (sink.bytes, sink.tail()),
            Err(_) => (0, String::new()),
        };
        let mut killed = false;
        if let Ok(mut guard) = children.lock() {
            if guard.generation == generation {
                if let Some(mut child) = guard.video.take() {
                    kill(&mut child);
                    killed = true;
                }
            }
        }
        crate::debug_log(&format!(
            "live attempt stalled kind={kind} after_ms={} killed={} stderr_bytes={bytes} tail={tail}",
            started.elapsed().as_millis(),
            u8::from(killed)
        ));
    });
}

/// Load the room's 头图 and publish it as the player's buffering poster. Two
/// candidates: the window-sized variant first, the URL exactly as the API gave
/// it second (a 320px card cover still beats a blank wallpaper).
pub fn live_load_poster(
    session: Arc<bilibili::Session>,
    window: slint::Weak<PlayerWindow>,
    cover: String,
) {
    let base = cover.trim();
    if base.len() < 12 {
        return;
    }
    let base = if let Some(rest) = base.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        base.to_string()
    };
    let (stem, already_sized) = match base.split_once('@') {
        Some((stem, rest)) => (stem.to_string(), !rest.is_empty()),
        None => (base.clone(), false),
    };
    let mut urls: Vec<String> = vec![format!("{stem}@960w_540h_1c.webp")];
    if already_sized {
        urls.push(base.clone());
    }
    std::thread::spawn(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        for url in urls {
            match runtime.block_on(session.fetch_thumb(&url)) {
                Ok((bytes, w, h)) => {
                    if bytes.len() < (w as usize) * (h as usize) * 4 {
                        continue;
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = window.upgrade() {
                            let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
                            buffer.make_mut_bytes().copy_from_slice(&bytes);
                            window.set_live_poster(slint::Image::from_rgba8(buffer));
                        }
                    });
                    crate::debug_log(&format!("live poster ok {w}x{h} url_len={}", url.len()));
                    return;
                }
                Err(err) => crate::debug_log(&format!(
                    "live poster err url_len={} err={err:#}",
                    url.len()
                )),
            }
        }
    });
}

/// `1.2万` for the card and the LIVE badge. Mirrors the feed's own style.
fn format_viewers(online: u64) -> String {
    if online >= 10_000 {
        let value = online as f64 / 10_000.0;
        if value >= 10.0 {
            format!("{value:.0}万")
        } else {
            format!("{value:.1}万")
        }
    } else {
        online.to_string()
    }
}

/// `BILI_LIVE_BREAK`: kill the current live decoder once, exactly like a
/// dropped stream, so the reconnect/backoff path is measured on the real code.
static LIVE_BREAK: AtomicBool = AtomicBool::new(false);

/// Ask the live pump to end its current session on the next frame.
pub fn live_test_break() {
    LIVE_BREAK.store(true, Ordering::SeqCst);
    crate::debug_log("live test break armed");
}

/// Why the pump of one attempt stopped. `Break` is the QA hook's kill (the
/// stream was fine, it was taken away); `Eof` is the decoder ending on its own
/// (the source failed or the upstream dropped it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LivePumpEnd {
    Eof,
    Break,
    Closed,
}

fn live_pump(
    stdout: &mut std::process::ChildStdout,
    size: FrameSize,
    mailbox: &Arc<FrameMailbox>,
    control: &Arc<PlaybackControl>,
    stats: &Arc<LiveStats>,
    first_at: &mut Option<Instant>,
    first_frame: &Arc<AtomicBool>,
    started: Instant,
) -> LivePumpEnd {
    let nv12 = pipe_is_nv12();
    let nv12_len = nv12_bytes(size);
    let rgba_len = size.bytes();
    let mut pipe = vec![0u8; if nv12 { nv12_len } else { rgba_len }];
    let mut last_log = Instant::now();
    // Realtime presentation grid: the decoder is asked for `fps=30`, so one
    // frame every 33.3 ms is the stream's own rate. Presenting them as fast as
    // they arrive is what made this run 14 % fast before the pacer.
    let period = Duration::from_millis(33);
    let mut next_at = Instant::now();
    loop {
        if control.is_closed() {
            return LivePumpEnd::Closed;
        }
        if LIVE_BREAK.swap(false, Ordering::SeqCst) {
            crate::debug_log("live test break fired");
            return LivePumpEnd::Break;
        }
        match stdout.read_exact(&mut pipe) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return LivePumpEnd::Eof, // EOF / killed child: the session decides.
        }
        // Re-check after the blocking read: a frame that arrived while the
        // break was armed must not be presented (it would look like a recovery
        // that never happened).
        if LIVE_BREAK.swap(false, Ordering::SeqCst) {
            crate::debug_log("live test break fired after read");
            return LivePumpEnd::Break;
        }
        // Pace against the wall clock. A frame that arrives very late (a
        // network gap) resyncs the grid instead of being pushed immediately
        // and then followed by a rush.
        let now = Instant::now();
        // Only a real stall (2 s) resets the grid: a burst of HLS frames is
        // normal, and resetting on it threw away the time already paced
        // (measured: 19.9 fps instead of 30).
        if now > next_at + Duration::from_secs(2) {
            next_at = now;
        } else if now < next_at {
            std::thread::sleep(next_at - now);
        } else {
            next_at = now;
        }
        next_at += period;
        let mut frame = mailbox.pool.take(size);
        if nv12 {
            nv12_to_rgba(frame.make_mut_bytes(), &pipe, size.w as usize, size.h as usize);
        } else {
            frame.make_mut_bytes().copy_from_slice(&pipe);
        }
        mailbox.push(MailFrame {
            buffer: frame,
            pooled: true,
        });
        let frames = stats.frames.fetch_add(1, Ordering::SeqCst) + 1;
        if first_at.is_none() {
            *first_at = Some(Instant::now());
            first_frame.store(true, Ordering::SeqCst);
            let ms = started.elapsed().as_millis() as u64;
            stats.first_frame_ms.store(ms, Ordering::SeqCst);
            crate::debug_log(&format!("live first frame after_ms={ms} size={}x{}", size.w, size.h));
            control.prof.first_seen.store(true, Ordering::Relaxed);
            control.prof.presented.fetch_add(1, Ordering::Relaxed);
            control.release_audio_hold_now();
            let window = mailbox.window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let view = crate::player_view::SlintPlayerView(window);
                let mut presentation = crate::player_view::Presentation::new(&view);
                presentation.buffering(false, 1.0);
                presentation.resume();
            });
        } else {
            control.prof.presented.fetch_add(1, Ordering::Relaxed);
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            last_log = Instant::now();
            let (pushed, coalesced, posted, shown) = mailbox.counters();
            crate::debug_log(&format!(
                "live fps={:.1} frames={frames} pushed={pushed} shown={shown} coalesced={coalesced} posted={posted} audio_pid={}",
                frames as f64 / started.elapsed().as_secs_f64().max(0.001),
                control.audio_pid()
            ));
        }
    }
}

fn live_wait(control: &Arc<PlaybackControl>, ms: u64) -> bool {
    let steps = (ms / 100).max(1);
    for _ in 0..steps {
        if control.is_closed() {
            return false;
        }
        if control.paused() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// The live session loop: resolve → spawn → pump → back off → repeat.
fn live_run(
    session: Arc<bilibili::Session>,
    source: LiveSource,
    window: slint::Weak<PlayerWindow>,
    control: Arc<PlaybackControl>,
    stats: Arc<LiveStats>,
    mailbox: Arc<FrameMailbox>,
    children: Arc<std::sync::Mutex<LiveChildren>>,
) {
    let Some(runtime) = bilibili::shared_runtime() else {
        return;
    };
    // Orientation decides the pipe's canvas (a 1080x1920 portrait room must not
    // be padded into a 16:9 frame). The index payload does not carry it, so the
    // first thing this thread does is ask `get_info` once - the same call the
    // 30 s viewer refresh uses, and the same answer B站 itself renders with.
    let mut source = source;
    crate::debug_log(&format!(
        "live session begin room={} title={} uname={} area={}",
        source.room_id, source.title, source.uname, source.area
    ));
    match runtime.block_on(session.live_room_info(source.room_id)) {
        Ok(info) => {
            source.is_portrait = info.is_portrait;
            stats.live_status.store(info.live_status, Ordering::SeqCst);
            live_set_viewers(&window, info.online, info.live_status);
            crate::debug_log(&format!(
                "live orientation room={} portrait={} title={} cover={}",
                source.room_id,
                u8::from(info.is_portrait),
                info.title,
                info.cover.len()
            ));
            // The room's own 头图 is a better poster than the index card's 320px
            // cover; fall back to it only when the card had nothing.
            if source.cover.is_empty() && !info.cover.is_empty() {
                source.cover = info.cover;
                live_load_poster(session.clone(), window.clone(), source.cover.clone());
            }
        }
        Err(err) => crate::debug_log(&format!(
            "live orientation fallback room={} err={err:#}",
            source.room_id
        )),
    }
    let size = source.frame_size();
    let mut attempt = 0u32;
    let mut first_at: Option<Instant> = None;
    // Whether the CURRENT attempt produced a frame. `stats.first_frame_ms` is
    // per window (it must survive a reconnect), so a fresh attempt needs its own
    // flag to tell "this source never opened" from "it played and then dropped".
    let first_frame = Arc::new(AtomicBool::new(false));
    // Which delivery this session is on (`live_candidates` order). A delivery
    // that produces no frame rotates to the next one; a delivery that played
    // and then dropped keeps its place and only reconnects.
    let mut rank = 0usize;
    // Full passes through the candidate list without a single frame.
    let mut empty_passes = 0u32;
    let forced = live_forced_kind();
    if let Some(kind) = forced {
        crate::debug_log(&format!("live force kind={kind}"));
    }
    // Attempts since the last successful frame burst. `0` = healthy.
    loop {
        if control.is_closed() {
            return;
        }
        if control.paused() {
            attempt = 0;
            while control.paused() && !control.is_closed() {
                std::thread::sleep(Duration::from_millis(120));
            }
            if control.is_closed() {
                return;
            }
            // A resume starts a fresh live edge; nothing is "continued".
            control.clear_resume_request();
        }
        let resolve_started = Instant::now();
        let play = match runtime.block_on(session.live_play_url(source.room_id)) {
            Ok(play) => play,
            Err(err) => {
                crate::debug_log(&format!(
                    "live playurl err room={} err={err:#}",
                    source.room_id
                ));
                stats.live_status.store(0, Ordering::SeqCst);
                if !live_backoff(&control, &window, &stats, &mut attempt, &err.to_string()) {
                    return;
                }
                continue;
            }
        };
        stats.live_status.store(play.live_status, Ordering::SeqCst);
        if play.live_status != 1 {
            let window_toast = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window_toast.upgrade() {
                    crate::toast(&window, "主播不在直播中", crate::TOAST_KIND_INFO);
                }
            });
            if !live_backoff(&control, &window, &stats, &mut attempt, "not_live") {
                return;
            }
            continue;
        }
        if play.locked {
            let window_toast = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window_toast.upgrade() {
                    crate::toast(&window, "该直播间已加密，暂不支持播放", crate::TOAST_KIND_ERROR);
                }
            });
            return;
        }
        let mut candidates = live_candidates(&play);
        if let Some(kind) = forced {
            // Start on the forced delivery, keep the rest behind it: a QA run
            // that forces a dead rendition must still reach the picture.
            match candidates.iter().position(|(candidate, _)| *candidate == kind) {
                Some(pos) => {
                    candidates.rotate_left(pos);
                    crate::debug_log(&format!(
                        "live force kind={kind} start_rank={pos} n={}",
                        candidates.len()
                    ));
                }
                None => crate::debug_log(&format!(
                    "live force kind={kind} missing n={}",
                    candidates.len()
                )),
            }
        }
        if candidates.is_empty() {
            crate::debug_log(&format!("live playurl empty room={}", source.room_id));
            if !live_backoff(&control, &window, &stats, &mut attempt, "no-source") {
                return;
            }
            continue;
        }
        if rank >= candidates.len() {
            rank = 0;
        }
        let (kind, url) = candidates[rank].clone();
        let url_for_audio = url.clone();
        let api_ms = resolve_started.elapsed().as_millis();
        // One stderr window per attempt: the deadline below logs it when the
        // delivery stalls, the drain thread logs it again at EOF.
        let sink = Arc::new(Mutex::new(LiveStderr::default()));
        crate::debug_log(&format!(
            "live session room={} attempt={} kind={kind} api_ms={api_ms} url_len={} rank={rank}/{}",
            source.room_id,
            attempt + 1,
            url.len(),
            candidates.len()
        ));
        if std::env::var_os("BILI_LIVE_DEBUG_URL").is_some() {
            crate::debug_log(&format!("live url kind={kind} {url}"));
        }
        let Ok(video) = spawn_live_video(&url, size, kind, &sink) else {
            crate::debug_log("live ffmpeg spawn failed");
            if !live_backoff(&control, &window, &stats, &mut attempt, "spawn") {
                return;
            }
            continue;
        };
        control.decoder_live.store(true, Ordering::SeqCst);
        // The sound starts at the same live edge. A failure here is not fatal:
        // the picture plays silently and the next reconnect retries both.
        // The sound goes through the local relay (see [`start_live_relay`]);
        // if the listener could not bind, fall back to the CDN URL directly.
        // A relay must forward a BODY, not a playlist: the FLV rendition is the
        // continuous stream (the HLS URL would only deliver the m3u8 text).
        let relay_source = play
            .flv
            .clone()
            .unwrap_or_else(|| url_for_audio.clone());
        let relay = start_live_relay(session.clone(), relay_source);
        let audio_url = relay
            .as_ref()
            .map(LiveRelay::url)
            .unwrap_or_else(|| url_for_audio.clone());
        let audio = spawn_live_audio(&audio_url).ok();
        if let Some(audio) = audio.as_ref() {
            control.prof.decoded.store(0, Ordering::Relaxed);
            control.set_audio_pid(audio.id());
            // The gain/mute the user set belongs to the new session; honour it
            // as soon as the session exists, but hold it silent until the first
            // frame (released by `release_audio_hold_now`).
            control.audio_hold_silent.store(true, Ordering::SeqCst);
            control.audio_session_pending.store(true, Ordering::SeqCst);
        }
        let generation = match children.lock() {
            Ok(mut guard) => {
                guard.generation = guard.generation.wrapping_add(1);
                guard.video = Some(video);
                guard.audio = audio;
                guard.generation
            }
            Err(_) => return,
        };
        let started = Instant::now();
        first_frame.store(false, Ordering::SeqCst);
        live_arm_frame_deadline(
            children.clone(),
            control.clone(),
            first_frame.clone(),
            sink.clone(),
            generation,
            kind,
        );
        let mut stdout = {
            let mut guard = match children.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            guard.video.as_mut().and_then(|child| child.stdout.take())
        };
        let ended = match stdout.as_mut() {
            Some(stdout) => live_pump(
                stdout,
                size,
                &mailbox,
                &control,
                &stats,
                &mut first_at,
                &first_frame,
                started,
            ),
            None => LivePumpEnd::Eof,
        };
        control.decoder_live.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = children.lock() {
            if let Some(mut child) = guard.video.take() {
                // `try_wait` first: a decoder that exited on its own (the common
                // failure) must report its real code, not the kill we add.
                let code = child.try_wait().ok().flatten().and_then(|status| status.code());
                kill(&mut child);
                crate::debug_log(&format!(
                    "live ffmpeg exit code={code:?} kind={kind} ran_ms={} first_frame={}",
                    started.elapsed().as_millis(),
                    u8::from(first_frame.load(Ordering::SeqCst))
                ));
            }
            if let Some(mut child) = guard.audio.take() {
                kill(&mut child);
            }
        }
        control.set_audio_pid(0);
        if let Some(relay) = relay.as_ref() {
            relay.stop();
        }
        if ended == LivePumpEnd::Closed || control.is_closed() {
            return;
        }
        // A decoder that ran for a while is a healthy session that dropped, not
        // a failing one: its backoff starts from 1 s again.
        if started.elapsed() >= Duration::from_secs(10) {
            attempt = 0;
        }
        // `Break` is the QA hook taking the stream away: the delivery was fine,
        // so it must read as a normal reconnect and never as "this source cannot
        // open" (that would rotate the session off a working rendition).
        if ended == LivePumpEnd::Eof && !first_frame.load(Ordering::SeqCst) {
            // This delivery never produced a picture. Rotate to the next one with
            // a short gap instead of the reconnect backoff: the room is live, it
            // is the rendition that cannot open here.
            let from = kind;
            rank += 1;
            if rank >= candidates.len() {
                rank = 0;
                empty_passes += 1;
            }
            let to = candidates
                .get(rank)
                .map(|(candidate, _)| *candidate)
                .unwrap_or("none");
            crate::debug_log(&format!(
                "live switch {from}->{to} reason=no-first-frame ran_ms={} pass={empty_passes}",
                started.elapsed().as_millis()
            ));
            if empty_passes >= 2 {
                // Two full passes with no picture anywhere: not a source
                // problem. Hand it to the backoff (and its toast) so the window
                // stops burning connections.
                empty_passes = 0;
                stats.reconnects.fetch_add(1, Ordering::SeqCst);
                if !live_backoff(&control, &window, &stats, &mut attempt, "no-source-frame") {
                    return;
                }
            } else if !live_wait(&control, LIVE_SWITCH_MS) {
                return;
            }
            continue;
        }
        empty_passes = 0;
        stats.reconnects.fetch_add(1, Ordering::SeqCst);
        if !live_backoff(&control, &window, &stats, &mut attempt, "stream-ended") {
            return;
        }
    }
}

/// Wait `1, 2, 4, 8, 16` s between attempts; after five failures show one toast
/// and idle until the user presses play (or the room API says "live" again).
/// Returns `false` when the window closed.
fn live_backoff(
    control: &Arc<PlaybackControl>,
    window: &slint::Weak<PlayerWindow>,
    stats: &Arc<LiveStats>,
    attempt: &mut u32,
    reason: &str,
) -> bool {
    *attempt = attempt.saturating_add(1);
    if let Ok(mut list) = stats.attempts.lock() {
        list.push(format!("{reason}"));
        if list.len() > 16 {
            list.remove(0);
        }
    }
    if *attempt <= 5 {
        let delay = 1u64 << (*attempt - 1).min(4);
        crate::debug_log(&format!(
            "live reconnect attempt={} delay_s={delay} reason={reason}",
            *attempt
        ));
        return live_wait(control, delay * 1000);
    }
    crate::debug_log(&format!(
        "live reconnect exhausted attempts={} reason={reason}",
        *attempt
    ));
    *attempt = 0;
    set_playing(window, false);
    let window_toast = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = window_toast.upgrade() {
            crate::toast(&window, "直播已结束或网络中断", crate::TOAST_KIND_ERROR);
        }
    });
    // Idle until the user asks for play (the ▶ button or the API reporting the
    // room live again). `resume_requested` is what the VOD path uses for the
    // same "no decoder is running" press.
    loop {
        for _ in 0..20 {
            if control.is_closed() {
                return false;
            }
            if control.resume_requested() {
                control.clear_resume_request();
                crate::debug_log("live retry after play press");
                return true;
            }
            if stats.live_status.load(Ordering::SeqCst) == 1 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Keep waiting in 2 s steps; the room-info thread can flip live_status.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_sources_are_ordered_flv_then_fmp4_then_ts() {
        let all = bilibili::LivePlay {
            hls: Some("https://cdn/live/ts.m3u8?sig=1".into()),
            hls_fmp4: Some("https://cdn/live/fmp4.m3u8?sig=2".into()),
            hls_ts: Some("https://cdn/live/ts.m3u8?sig=1".into()),
            flv: Some("https://cdn/live/stream.flv?sig=3".into()),
            live_status: 1,
            locked: false,
        };
        let kinds: Vec<&str> = live_candidates(&all).into_iter().map(|(kind, _)| kind).collect();
        // FLV is the delivery that opens here (2.7 s); the ts rendition never
        // opens, so it must be last, never first.
        assert_eq!(kinds, vec!["flv", "hls_fmp4", "hls_ts"]);

        // A room with no FLV still tries fmp4 before ts.
        let no_flv = bilibili::LivePlay {
            flv: None,
            ..all.clone()
        };
        let kinds: Vec<&str> = live_candidates(&no_flv).into_iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, vec!["hls_fmp4", "hls_ts"]);

        // Only the merged field: the room stays playable instead of rotating
        // through an empty list.
        let merged = bilibili::LivePlay {
            hls: Some("https://cdn/live/only.m3u8".into()),
            ..Default::default()
        };
        let kinds: Vec<&str> = live_candidates(&merged).into_iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, vec!["hls"]);

        // A short/empty URL is not a candidate (the API can answer "").
        let empty = bilibili::LivePlay {
            flv: Some(String::new()),
            ..Default::default()
        };
        assert!(live_candidates(&empty).is_empty());
    }

    #[test]
    fn live_stderr_tail_is_one_line_and_keeps_the_end() {
        let long = "x".repeat(LIVE_STDERR_LIMIT * 2);
        let tail = live_stderr_tail(format!("Error opening input\n\n   {long}").as_bytes());
        assert!(!tail.contains('\n'), "one log line");
        // The marker plus exactly the budget: the head is what gets dropped.
        assert_eq!(tail.chars().count(), LIVE_STDERR_LIMIT + 1);
        assert!(tail.starts_with('…'));
        assert!(tail.ends_with('x'));
        // Short input is only whitespace-collapsed.
        assert_eq!(live_stderr_tail(b"  boom \r\n\n next  "), "boom next");
        assert_eq!(live_stderr_tail(b""), "");
    }

    #[test]
    fn startup_gate_releases_only_after_a_healthy_start() {
        // Nothing before the first frame, whatever the buffer looks like.
        assert!(!startup_gate_ready(0, 60_000));
        // Frames for 5 s but the stream stalled 1 s ago: still held.
        assert!(!startup_gate_ready(GATE_PLAYING_MS, GATE_HEALTHY_MS - 1));
        // Healthy for 3 s but only 4 s of playback: still held.
        assert!(!startup_gate_ready(GATE_PLAYING_MS - 1, GATE_HEALTHY_MS));
        // Both conditions met: released, and it stays released.
        assert!(startup_gate_ready(GATE_PLAYING_MS, GATE_HEALTHY_MS));
        assert!(startup_gate_ready(60_000, 55_000));
    }

    #[test]
    fn startup_ready_bytes_is_the_smaller_of_the_cap_and_two_seconds() {
        // A typical 1080p DASH track (2.5 Mbit/s) needs far less than 8 MiB for
        // two seconds, so the time rule wins.
        assert_eq!(startup_ready_bytes(2_500_000), 625_000);
        // 2 s of 20 Mbit/s would be 5 MB, still under the cap.
        assert_eq!(startup_ready_bytes(20_000_000), 5_000_000);
        // A very high bitrate is capped at 8 MiB.
        assert_eq!(startup_ready_bytes(100_000_000), STARTUP_READY_BYTES);
        // Unknown bitrate: only the byte cap applies.
        assert_eq!(startup_ready_bytes(0), STARTUP_READY_BYTES);
        // No threshold ever goes to zero (a zero target would open at once).
        assert!(startup_ready_bytes(1) > 0);
    }

    #[test]
    fn cache_progress_reaches_full_exactly_at_the_target() {
        assert_eq!(cache_progress_milli(0, 8 << 20), 0);
        assert_eq!(cache_progress_milli(1 << 20, 8 << 20), 125);
        assert_eq!(cache_progress_milli(8 << 20, 8 << 20), 1000);
        // A cache overshoot still reads full, and a zero target cannot divide.
        assert_eq!(cache_progress_milli(99 << 20, 8 << 20), 1000);
        assert_eq!(cache_progress_milli(0, 0), 0);
    }

    #[test]
    fn cache_gate_thresholds_are_the_contract() {
        // The overlays the user agreed to wait through: at most 6 s, 8 MiB.
        assert_eq!(STARTUP_READY_TIMEOUT_MS, 6_000);
        assert_eq!(STARTUP_READY_BYTES, 8 << 20);
        assert_eq!(STARTUP_READY_SECONDS, 2);
        // A ring gate keeps its old 4 s / 2 s ceilings.
        assert_eq!(GateKind::Initial.timeout_ms(), PRE_ROLL_TIMEOUT_MS);
    }

    #[test]
    fn pre_roll_target_is_clamped_to_the_ring() {
        // The first session asks for `PRE_ROLL_START_FRAMES` (~0.5 s at 30 fps).
        assert_eq!(GateKind::Initial.target_frames(32), PRE_ROLL_START_FRAMES);
        // A 1080p ring holds only 7 frames: the gate never asks for more than
        // the ring can physically deliver.
        assert_eq!(GateKind::Initial.target_frames(7), 7);
        assert_eq!(GateKind::Initial.target_frames(1), 1);
        assert_eq!(GateKind::Initial.target_frames(0), 1);
        // Recovery waits for the whole ring.
        assert_eq!(GateKind::Recovery.target_frames(16), 16);
        assert_eq!(GateKind::Recovery.target_frames(4), 4);
        assert_eq!(GateKind::Initial.timeout_ms(), PRE_ROLL_TIMEOUT_MS);
        assert_eq!(
            GateKind::Recovery.timeout_ms(),
            PRE_ROLL_RECOVERY_TIMEOUT_MS
        );
    }

    #[test]
    fn target_frame_size_is_source_based_and_preserves_aspect() {
        // A 1080p source decodes at 1080p whatever the window: the fullscreen
        // transition cannot change the target, so it cannot restart anything.
        assert_eq!(
            target_frame_size(1920, 1080),
            FrameSize { w: 1920, h: 1080 }
        );
        // f176: a 4K source decodes at 4K — the pipe follows the stream.
        assert_eq!(
            target_frame_size(3840, 2160),
            FrameSize { w: 3840, h: 2160 }
        );
        // 8K is the only thing still scaled down (the 4K cap).
        assert_eq!(
            target_frame_size(7680, 4320),
            FrameSize { w: 3840, h: 2160 }
        );
        // Never upscale a small source.
        assert_eq!(target_frame_size(1280, 720), FrameSize { w: 1280, h: 720 });
        // A 4:3 source keeps its aspect, capped at 1080 high.
        assert_eq!(
            target_frame_size(1440, 1080),
            FrameSize { w: 1440, h: 1080 }
        );
        // An ultrawide source keeps its width (under the 4K cap), aspect
        // preserved by the height clamp.
        assert_eq!(target_frame_size(2560, 1080), FrameSize { w: 2560, h: 1080 });
        // Odd source/clamp values are made even, and the width comes from the
        // source aspect, not from an independent rounding.
        let odd = target_frame_size(1921, 1081);
        assert_eq!(odd.w % 2, 0);
        assert_eq!(odd.h % 2, 0);
        assert!(odd.w <= 1921 && odd.h <= 1081);
        // Unknown source geometry (not in the manifest): the 720p default, and
        // never zero.
        assert_eq!(
            target_frame_size(0, 0),
            FrameSize {
                w: DEFAULT_DISPLAY_W,
                h: DEFAULT_DISPLAY_H
            }
        );
        let tiny = target_frame_size(1, 1);
        assert!(tiny.w >= 2 && tiny.h >= 2);
    }

    #[test]
    fn ring_capacity_follows_the_frame_budget() {
        // 1920x1080 RGBA = 8.3 MB: 128 MB / 8.3 MB = 16 frames (~133 MB).
        let uhd = FrameSize { w: 1920, h: 1080 };
        assert_eq!(ring_capacity_for(uhd.bytes()), 16);
        // 1124x632 = 2.8 MB fits 45 frames, capped at the 32-frame ceiling.
        let qa = FrameSize { w: 1124, h: 632 };
        assert_eq!(ring_capacity_for(qa.bytes()), RING_MAX_CAPACITY);
        // The old 960x540 frame (2.1 MB) is also ceiling-bound.
        assert_eq!(
            ring_capacity_for(FrameSize { w: 960, h: 540 }.bytes()),
            RING_MAX_CAPACITY
        );
        // A tiny frame is bounded by the ceiling, never unbounded.
        assert_eq!(
            ring_capacity_for(FrameSize { w: 2, h: 2 }.bytes()),
            RING_MAX_CAPACITY
        );
        // Nonsense input still yields a usable ring.
        assert_eq!(ring_capacity_for(0), RING_MAX_CAPACITY);
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

    fn play_info(tracks: Vec<bilibili::VideoTrack>, default_qn: i64) -> bilibili::PlayInfo {
        bilibili::PlayInfo {
            video_tracks: tracks,
            qualities: Vec::new(),
            default_qn,
            audio_tracks: Vec::new(),
            duration: 60,
        }
    }

    #[test]
    fn a_stream_s_own_cache_key_names_the_tier_and_the_codec() {
        // One `.part` per raw stream: the same video at 1080P AVC, 1080P AV1 and
        // 720P is three different bitstreams at three different byte offsets, so
        // a switch must never hand one file to another.
        let avc = stream_cache_key("BV1xx411c7mD", 42, 80, 7);
        let av1 = stream_cache_key("BV1xx411c7mD", 42, 80, 13);
        let hd = stream_cache_key("BV1xx411c7mD", 42, 64, 7);
        assert_eq!(avc, "BV1xx411c7mD-42-80-7");
        assert_ne!(avc, av1);
        assert_ne!(avc, hd);
        // Same tier and codec on another video is another file.
        assert_ne!(avc, stream_cache_key("BV1yy411c7mD", 42, 80, 7));
        // The audio file of the same video can never collide with a video one.
        assert_ne!(avc, format!("BV1xx411c7mD-42-audio-30280"));
    }

    #[test]
    fn automatic_open_picks_the_highest_playable_source() {
        // f176: the owner asked for "the best source available" on open, so
        // 自动 (0) takes the top playable tier — 4K when the account may open it.
        let info = play_info(
            vec![
                video_track(120, 7, 20_000_000),
                video_track(116, 7, 8_000_000),
                video_track(80, 7, 2_900_000),
                video_track(64, 7, 1_300_000),
                video_track(32, 7, 700_000),
                video_track(16, 7, 430_000),
            ],
            32,
        );
        assert_eq!(initial_quality(&info, 0), 120);
        // An explicit stored tier still wins.
        assert_eq!(initial_quality(&info, 80), 80);
        // A stored tier the video does not have falls back to the automatic rule.
        assert_eq!(initial_quality(&info, 74), 120);
        // With 4K locked out of the list (大会员), the highest playable wins.
        let locked = play_info(
            vec![video_track(80, 7, 2_900_000), video_track(64, 7, 1_300_000)],
            32,
        );
        assert_eq!(initial_quality(&locked, 0), 80);
    }

    #[test]
    fn automatic_open_takes_the_highest_playable_below_the_cap() {
        // A 720P upload: 720P, not the 480P `default_qn` fallback and not 360P.
        let info = play_info(
            vec![video_track(64, 7, 1_300_000), video_track(16, 7, 430_000)],
            32,
        );
        assert_eq!(initial_quality(&info, 0), 64);
        // A 1080P60-only source still plays: the lowest tier above the cap is
        // better than nothing.
        let p60 = play_info(vec![video_track(116, 7, 8_000_000)], 0);
        assert_eq!(initial_quality(&p60, 0), 116);
        // A durl-only response (qn 0) keeps the decoder's own fallback.
        let durl = play_info(vec![video_track(0, 0, 0)], 0);
        assert_eq!(initial_quality(&durl, 0), 0);
    }

    #[test]
    fn a_fresh_install_opens_the_best_source_at_its_own_size() {
        // f176: 自动 (0) = the highest playable source, decoded at the stream's
        // own resolution.
        assert_eq!(crate::settings::Settings::default().player_quality, 0);
        assert!(crate::settings::Settings::default().player_native_size);
        // Any concrete tier is preserved (the owner may pin 4K); nonsense falls
        // back to 自动.
        for qn in [0, 32, 64, 80, 116, 120] {
            assert_eq!(crate::settings::normalise_player_quality(qn), qn);
        }
        assert_eq!(crate::settings::normalise_player_quality(-4), 0);
    }

    #[test]
    fn frame_size_follows_the_window_unless_native_is_on() {
        let decode = target_frame_size(1920, 1080);
        // 默认: the window's picture band, aspect preserved.
        assert_eq!(
            reader_output_size(decode, (1216, 684), false),
            FrameSize { w: 1216, h: 684 }
        );
        // A big window never upscales: the source size is the ceiling.
        assert_eq!(reader_output_size(decode, (2560, 1440), false), decode);
        // 原始分辨率 keeps the source size whatever the window says.
        assert_eq!(reader_output_size(decode, (1216, 684), true), decode);
        // A window taller than 16:9 is bounded by the width.
        assert_eq!(
            reader_output_size(decode, (1920, 1440), false),
            FrameSize { w: 1920, h: 1080 }
        );
        // A 4:3 source in a wide window keeps its own aspect.
        let four_three = target_frame_size(1440, 1080);
        assert_eq!(
            reader_output_size(four_three, (1200, 900), false),
            FrameSize { w: 1200, h: 900 }
        );
        // Without a published window size (0x0) the source size is used.
        assert_eq!(reader_output_size(decode, (0, 0), false), decode);
        // Odd window sizes are made even, never zero.
        let odd = reader_output_size(decode, (1217, 685), false);
        assert_eq!(odd.w % 2, 0);
        assert_eq!(odd.h % 2, 0);
        assert!(odd.w > 0 && odd.h > 0);
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
            vec![video_track(80, 7, 1_180_000), video_track(80, 12, 900_000)],
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

    #[test]
    fn auto_pause_thresholds_keep_hysteresis_inside_the_ring() {
        let frame_ms = 1000.0 / 30.0;
        // A 16-frame ring holds ~533 ms: the 150 ms pause and a release above
        // it, but the release still fits the ring.
        let (pause, resume) = auto_buffer_thresholds(frame_ms, 16);
        assert!((pause - AUTO_PAUSE_LEAD_MS).abs() < 0.5);
        assert!(resume > pause + 100.0);
        assert!(resume <= AUTO_RESUME_LEAD_MS);
        assert!(resume <= 16.0 * frame_ms);

        // A 1080p ring holds only 8 frames (~267 ms). A fixed 600 ms release
        // could never be reached; the cap keeps the guard releasable.
        let (pause, resume) = auto_buffer_thresholds(frame_ms, 8);
        assert!(pause > 0.0);
        assert!(resume > pause);
        assert!(resume < 8.0 * frame_ms);

        // Nonsense input still yields ordered thresholds and at least one frame.
        let (pause, resume) = auto_buffer_thresholds(f64::NAN, 1);
        assert!(pause > 0.0 && resume > pause);
        assert_eq!(auto_resume_frames(resume, frame_ms, 1), 1);
        assert_eq!(
            auto_resume_frames(resume, frame_ms, 16),
            16.min((resume / frame_ms).ceil() as usize)
        );
    }

    #[test]
    fn av_guard_pauses_only_above_the_window_and_resumes_when_caught_up() {
        let t0 = Instant::now();
        let mut guard = AvGuard::new();
        // Inside the window: never touch the audio.
        assert_eq!(guard.step(AV_LEAD_MS, false, t0), AvStep::None);
        assert_eq!(
            guard.step(AV_LEAD_MS + 1, false, t0),
            AvStep::Pause,
            "a lead beyond the window freezes the sound"
        );
        guard.note_paused(t0);
        // Suspended: the picture closes the gap at its own rate, so the guard
        // waits instead of restarting anything.
        assert_eq!(
            guard.step(AV_LEAD_MS, true, t0 + Duration::from_millis(200)),
            AvStep::None
        );
        assert_eq!(
            guard.step(1, true, t0 + Duration::from_millis(400)),
            AvStep::None,
            "still ahead by a millisecond: keep waiting"
        );
        assert_eq!(
            guard.step(0, true, t0 + Duration::from_millis(420)),
            AvStep::Resume,
            "no longer ahead: release in place"
        );
        assert_eq!(guard.restarts, 0);
    }

    #[test]
    fn av_guard_restarts_once_when_the_picture_cannot_catch_up() {
        let t0 = Instant::now();
        let mut guard = AvGuard::new();
        guard.note_paused(t0);
        // The picture is frozen (no frames), so the lead never shrinks: after
        // the bounded window one restart at the picture position.
        assert_eq!(guard.step(900, true, t0), AvStep::None);
        assert_eq!(
            guard.step(900, true, t0 + Duration::from_millis(AV_CATCHUP_MS - 1)),
            AvStep::None
        );
        assert_eq!(
            guard.step(900, true, t0 + Duration::from_millis(AV_CATCHUP_MS)),
            AvStep::Restart
        );
        assert_eq!(guard.restarts, 1);
        // A second catch-up failure inside the cooldown must not restart again.
        guard.note_paused(t0 + Duration::from_millis(AV_CATCHUP_MS));
        assert_eq!(
            guard.step(
                900,
                true,
                t0 + Duration::from_millis(AV_CATCHUP_MS) + Duration::from_millis(AV_CATCHUP_MS)
            ),
            AvStep::None,
            "one restart per cooldown"
        );
        assert_eq!(guard.restarts, 1);
        assert_eq!(
            guard.step(
                900,
                true,
                t0 + Duration::from_millis(AV_CATCHUP_MS) + AV_RESTART_COOLDOWN
            ),
            AvStep::Restart,
            "the cooldown over, the last resort is available again"
        );
    }

    #[test]
    fn av_guard_stops_restarting_after_two_big_leads() {
        let t0 = Instant::now();
        let mut guard = AvGuard::new();
        let first = t0;
        assert_eq!(guard.step(1_500, false, first), AvStep::Pause);
        guard.note_paused(first);
        assert_eq!(
            guard.step(0, true, first + Duration::from_millis(200)),
            AvStep::Resume
        );
        let second = first + Duration::from_secs(1);
        assert_eq!(guard.step(1_500, false, second), AvStep::Pause);
        guard.note_paused(second);
        assert_eq!(
            guard.step(0, true, second + Duration::from_millis(200)),
            AvStep::Resume
        );
        let third = second + Duration::from_secs(1);
        assert_eq!(
            guard.step(2_000, false, third),
            AvStep::Thrash,
            "two big leads within 30 s after a correction: stop correcting"
        );
        assert!(guard.thrash);
        // From here the picture corrects: no pause, no restart, whatever the
        // lead. (The pacer's delta logic holds or skips instead.)
        assert_eq!(
            guard.step(5_000, false, third + Duration::from_millis(500)),
            AvStep::None
        );
        assert_eq!(guard.restarts, 0);
    }

    #[test]
    fn av_guard_counts_big_leads_outside_the_window_again() {
        let t0 = Instant::now();
        let mut guard = AvGuard::new();
        // One big lead, then a quiet minute: the counter starts a new window,
        // so a single later episode is not "twice in 30 s".
        guard.step(1_500, false, t0);
        guard.note_paused(t0);
        guard.step(0, true, t0 + Duration::from_millis(100));
        let later = t0 + Duration::from_secs(40);
        guard.step(1_500, false, later);
        assert!(!guard.thrash);
    }

    #[test]
    fn seek_dedupes_the_same_target_and_clears_the_end_state() {
        let control = PlaybackControl::new();
        control.set_duration_ms(120_000);
        control.seek(10_000);
        let first = control.seek_seq();
        assert_eq!(control.pending_seek_ms.load(Ordering::SeqCst), 10_000);

        // The release seek that follows a drag seek for the same position is a
        // cheap no-op: no second decoder restart.
        control.seek(10_000);
        assert_eq!(control.seek_seq(), first);

        // A different target replaces the pending one instead of queueing.
        control.seek(20_000);
        assert_eq!(control.seek_seq(), first + 1);
        assert_eq!(control.pending_seek_ms.load(Ordering::SeqCst), 20_000);

        // An ended track is not a pause: a seek clears the latch.
        control.set_position_ms(120_000);
        control.note_stream_finished();
        assert!(control.track_ended());
        control.seek(5_000);
        assert!(!control.track_ended());
        assert!(!control.paused());
    }

    #[test]
    fn ffmpeg_duration_line_parses_to_ms() {
        // The real shape of the input dump line.
        assert_eq!(
            parse_ffmpeg_duration("  Duration: 00:01:24.00, start: 0.000000, bitrate: 2049 kb/s"),
            Some(84_000)
        );
        assert_eq!(
            parse_ffmpeg_duration("Duration: 01:02:03.45, start: 0.0, bitrate: 1 kb/s"),
            Some(3_723_450)
        );
        // `N/A` (live / fragmented input with no declared length) and garbage
        // keep the API value in force.
        assert_eq!(parse_ffmpeg_duration("  Duration: N/A, start: 0.0, bitrate: 1 kb/s"), None);
        assert_eq!(parse_ffmpeg_duration("Stream #0:0: Video: h264"), None);
        assert_eq!(parse_ffmpeg_duration("Duration: 00:xx:00.00, start: 0"), None);
    }

    #[test]
    fn duration_prefers_the_decoder_only_when_the_api_is_off_by_more_than_2pct() {
        let control = PlaybackControl::new();
        assert_eq!(control.duration_ms(), 0);
        control.set_duration_ms(84_000);
        assert_eq!(control.duration_ms(), 84_000);
        assert_eq!(control.duration_source(), "api");

        // Within 2 %: the API keeps the word (whole seconds vs the media).
        control.set_decoder_duration_ms(84_800, true);
        assert_eq!(control.duration_ms(), 84_000);
        assert_eq!(control.duration_source(), "api");

        // Beyond 2 %: the stream's own length wins (a short API duration used
        // to end a still-playing video).
        control.set_decoder_duration_ms(95_000, true);
        assert_eq!(control.duration_ms(), 95_000);
        assert_eq!(control.duration_source(), "decoder");

        // A partial cache-file session may not publish a duration at all.
        control.clear_decoder_duration();
        control.set_decoder_duration_ms(8_000, false);
        assert_eq!(control.duration_ms(), 84_000);
        assert_eq!(control.duration_source(), "api");

        // Absurd values are parse artefacts, not durations.
        control.set_decoder_duration_ms(u64::MAX, true);
        assert_eq!(control.duration_ms(), 84_000);
    }

    #[test]
    fn a_playhead_at_the_duration_does_not_end_a_moving_picture() {
        let control = PlaybackControl::new();
        control.set_duration_ms(84_000);
        // The API duration is 300 ms short of the media; the playhead crosses
        // it while frames are still being presented.
        control.set_position_ms(84_000);
        control.note_frame_presented();
        assert!(!control.track_ended(), "a presented frame keeps the track alive");
        assert!(!control.ended.load(Ordering::SeqCst));

        // No frame for longer than the idle window: now it really is over.
        control.presented_at_ms.store(
            monotonic_ms().saturating_sub(END_POSITION_IDLE_MS + 1),
            Ordering::SeqCst,
        );
        assert!(control.track_ended());
        assert!(control.ended.load(Ordering::SeqCst));

        // Sticky, and cleared by a seek like before.
        assert!(control.track_ended());
        control.seek(1_000);
        assert!(!control.track_ended());
    }

    #[test]
    fn eof_latches_only_once_the_ring_is_drained() {
        let control = PlaybackControl::new();
        control.set_duration_ms(84_000);
        control.set_position_ms(83_900);
        // The decoder closed with frames still queued: the picture has not been
        // shown yet, so this is not the end.
        control.note_ring_len(2);
        control.note_stream_finished();
        assert!(!control.ended.load(Ordering::SeqCst));
        // Drained: the end is real.
        control.note_ring_len(0);
        control.note_stream_finished();
        assert!(control.ended.load(Ordering::SeqCst));

        // A read error far from the end never ends the track.
        let control = PlaybackControl::new();
        control.set_duration_ms(600_000);
        control.set_position_ms(120_000);
        control.note_stream_finished();
        assert!(!control.ended.load(Ordering::SeqCst));
    }

    #[test]
    fn a_play_press_restarts_a_dead_session() {
        let control = PlaybackControl::new();
        control.set_duration_ms(600_000);
        control.set_position_ms(120_000);
        // No decoder session is running and the track was declared over.
        control.decoder_live.store(false, Ordering::SeqCst);
        control.ended.store(true, Ordering::SeqCst);
        let seq = control.seek_seq();
        // A play press must not latch a pause on a stopped picture; it asks for
        // a fresh session at the position the picture stopped at.
        assert!(!control.toggle_paused());
        assert!(!control.paused.load(Ordering::SeqCst));
        assert_eq!(control.seek_seq(), seq + 1);
        assert_eq!(control.pending_seek_ms.load(Ordering::SeqCst), 120_000);
        assert!(control.resume_requested());
        assert!(!control.track_ended());

        // A press at the real end starts over instead of replaying 0 frames.
        let control = PlaybackControl::new();
        control.set_duration_ms(84_000);
        control.set_position_ms(84_000);
        control.decoder_live.store(false, Ordering::SeqCst);
        control.ended.store(true, Ordering::SeqCst);
        assert!(!control.toggle_paused());
        assert_eq!(control.pending_seek_ms.load(Ordering::SeqCst), 0);

        // Before the first frame a press is an ordinary pause: the decoder is
        // not "dead" yet, it simply has not started.
        let control = PlaybackControl::new();
        control.set_duration_ms(84_000);
        control.decoder_live.store(false, Ordering::SeqCst);
        assert!(control.toggle_paused());
        assert!(control.paused.load(Ordering::SeqCst));
    }

    /// One raw DASH stream for the 画质 row tests.
    fn quality_option(qn: i64, codecid: i64, bandwidth: u64) -> bilibili::QualityOption {        let (hdr, dolby_vision, eight_k) = (qn == 125, qn == 126, qn == 127);
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
        assert!(quality_rows(&specs, None).iter().all(|row| !row.current));
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
    fn startup_stages_fill_the_bar_in_order_and_never_read_full() {
        // Nothing measured yet.
        assert_eq!(startup_overall_milli(&[0, 0, 0, 0]), 0);
        // Each stage owns its slice: Bytes 0-40 %, Decode 40-55 %, Audio
        // 55-75 %, PreRoll 75-99 %.
        assert_eq!(startup_overall_milli(&[500, 0, 0, 0]), 200);
        assert_eq!(startup_overall_milli(&[1000, 0, 0, 0]), 400);
        assert_eq!(startup_overall_milli(&[1000, 1000, 0, 0]), 550);
        assert_eq!(startup_overall_milli(&[1000, 1000, 1000, 0]), 750);
        assert_eq!(startup_overall_milli(&[1000, 1000, 1000, 1000]), 990);
        // The bar never leaves the slice of the stage the text names: an
        // unfinished earlier stage owns it, whatever a later one already knows.
        assert_eq!(startup_overall_milli(&[0, 0, 1000, 0]), 0);
        assert_eq!(startup_overall_milli(&[500, 1000, 1000, 1000]), 200);
        // 100 % belongs to `finish_startup` only.
        assert!(startup_overall_milli(&[1000, 1000, 1000, 1000]) < 1000);
    }

    #[test]
    fn startup_progress_moves_visibly_across_the_whole_bar() {
        // A realistic sweep: 20 cache polls, 10 decode, 15 audio, 15 ring polls.
        let mut values: Vec<u32> = Vec::new();
        let mut subs = [0u32; 4];
        for (stage, steps) in [(0usize, 20u32), (1, 10), (2, 15), (3, 15)] {
            for step in 1..=steps {
                subs[stage] = (step * 1000 / steps).min(1000);
                values.push(startup_overall_milli(&subs));
            }
        }
        let mut distinct: Vec<u32> = values
            .iter()
            .copied()
            .filter(|value| (50..950).contains(value))
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() >= 15,
            "only {} distinct values between 0.05 and 0.95: {distinct:?}",
            distinct.len()
        );
        // Monotonic throughout: no stage can ever drag the bar backwards.
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]), "{values:?}");
    }

    #[test]
    fn startup_detail_names_the_stage_that_is_still_running() {
        assert_eq!(startup_detail(&[420, 0, 0, 0], 0.0), "取流 42%");
        assert_eq!(startup_detail(&[1000, 0, 0, 0], 0.0), "解码中");
        assert_eq!(startup_detail(&[1000, 1000, 0, 0], 0.0), "准备音频");
        assert_eq!(startup_detail(&[1000, 1000, 1000, 480], 0.8), "缓冲 0.8s");
        assert_eq!(
            startup_detail(&[1000, 1000, 1000, 1000], 0.8),
            "即将播放"
        );
    }

    /// Publish a stage and let the bar's slew settle on the new target.
    fn settle(control: &Arc<PlaybackControl>, stage: StartupStage, sub: f32) {
        control.publish_startup(stage, sub);
        std::thread::sleep(Duration::from_millis(160));
        control.publish_startup(stage, sub);
    }

    #[test]
    fn startup_machine_publishes_a_monotonic_bar_and_opens_on_finish() {
        let control = Arc::new(PlaybackControl::new());
        assert!(!control.buffering());
        assert_eq!(control.buffer_progress(), 1.0);

        let _guard = control.begin_startup("test");
        assert!(control.buffering());
        assert_eq!(control.buffer_progress(), 0.0);
        assert_eq!(control.buffer_detail(), "取流 0%");

        // Bytes at half: the bar reaches 20 % of the bar (half of 0-40 %), and
        // the text reports the same stage.
        settle(&control, StartupStage::Bytes, 0.5);
        assert!((control.buffer_progress() - 0.20).abs() < 0.02);
        assert_eq!(control.buffer_detail(), "取流 50%");
        // A late, lower measurement cannot pull it back.
        settle(&control, StartupStage::Bytes, 0.2);
        assert!((control.buffer_progress() - 0.20).abs() < 0.02);

        // The cache is done: the bar now lives in the Decode slice.
        settle(&control, StartupStage::Bytes, 1.0);
        assert!((control.buffer_progress() - 0.40).abs() < 0.02);
        assert_eq!(control.buffer_detail(), "解码中");
        settle(&control, StartupStage::Decode, 1.0);
        assert!((control.buffer_progress() - 0.55).abs() < 0.02);

        // The audio clock and the ring.
        settle(&control, StartupStage::Audio, 1.0);
        assert!((control.buffer_progress() - 0.75).abs() < 0.02);
        assert_eq!(control.buffer_detail(), "缓冲 0.0s");
        settle(&control, StartupStage::PreRoll, 1.0);
        assert!((control.buffer_progress() - 0.99).abs() < 0.02);

        control.finish_startup("test");
        assert!(!control.startup_active());
        assert_eq!(control.buffer_progress(), 1.0);
        assert_eq!(control.buffer_detail(), "");
    }

    #[test]
    fn buffering_api_publishes_progress_and_full_when_open() {
        let control = PlaybackControl::new();
        // Never armed: not buffering and progress reads full.
        assert!(!control.buffering());
        assert_eq!(control.buffer_progress(), 1.0);

        control.begin_buffering("test", 16);
        assert!(control.buffering());
        assert_eq!(control.buffer_progress(), 0.0);
        control.note_frame_decoded(8);
        assert_eq!(control.buffer_progress(), 0.5);

        control.end_buffering("test");
        assert!(!control.buffering());
        assert_eq!(control.buffer_progress(), 1.0);
    }

    #[test]
    fn cache_gate_publishes_cache_progress_not_ring_occupancy() {
        let control = PlaybackControl::new();
        control.begin_cache_buffering("test", 8 << 20);
        assert!(control.buffering());
        // The ring is irrelevant to a cache gate: a full ring still reads 0%
        // until the proxy has the bytes.
        control.note_frame_decoded(16);
        assert_eq!(control.buffer_progress(), 0.0);
        control.set_buffer_pct(250);
        assert_eq!(control.buffer_progress(), 0.25);
        control.set_buffer_pct(1000);
        assert_eq!(control.buffer_progress(), 1.0);
        // The detail line reports the same percentage the gate opens at.
        assert!(control.buffer_detail().starts_with("加载中 100%"));
        control.end_buffering("test");
        assert_eq!(control.buffer_progress(), 1.0);
        assert_eq!(control.buffer_detail(), "");
        // A later ring/seek overlay must not inherit the cache target.
        control.begin_buffering("test", 4);
        control.note_frame_decoded(2);
        assert_eq!(control.buffer_progress(), 0.5);
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
        control.begin_buffering("test", 16);
        assert_eq!(control.playback_position_ms(), 1234);
        control.end_buffering("test");
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

    /// The master clock follows `ffplay`'s `-stats` samples by *rate*, not by
    /// step: a sample that lands behind the extrapolation must not move the
    /// clock back, because the pacer pays every backward step with a hold of the
    /// picture (the measured 26–28-frame second). A guard-scale backward
    /// discontinuity is still adopted at once.
    #[test]
    fn audio_clock_slews_a_late_sample_instead_of_stepping_back() {
        let control = PlaybackControl::new();
        let generation = control.begin_audio_clock_generation();
        assert!(control.set_audio_clock_ms(generation, 10_000));
        // 60 ms behind the extrapolation: jitter. The clock stays where it is
        // and is slowed down instead of jumping back to the sample.
        assert!(control.set_audio_clock_ms(generation, 9_940));
        let after = control.audio_clock().unwrap_or(0);
        assert!(after >= 10_000, "the clock stepped back to {after}");
        assert!(
            control.audio_clock_ppm.load(Ordering::SeqCst) < 0,
            "a late sample must slow the clock"
        );
        // A forward jump is not adopted on one sample: the clock keeps its
        // trajectory and the report becomes a candidate, because one report
        // cannot tell a real skip inside ffplay from the audio *device* position
        // a just-resumed process reports (the measured 629 ms artifact).
        assert!(control.set_audio_clock_ms(generation, 10_500));
        let after = control.audio_clock().unwrap_or(0);
        assert!(
            after < 10_100,
            "a single forward jump must not step the clock ({after})"
        );
        // The next burst at the same base confirms the skip: re-seed there
        // instead of holding the picture for a slew that would take seconds.
        assert!(control.set_audio_clock_ms(generation, 10_600));
        let after = control.audio_clock().unwrap_or(0);
        assert!(
            after >= 10_600,
            "a confirmed forward jump must re-seed the clock ({after})"
        );
        // A guard-scale backward discontinuity is adopted as well: the sound
        // really is that far behind, and slewing it would take tens of seconds.
        assert!(control.set_audio_clock_ms(generation, 10_100));
        let after = control.audio_clock().unwrap_or(0);
        assert!(after <= 10_110, "a backward lurch must be adopted ({after})");

        // The dip itself: a suspension froze the sound at 12_000, the release
        // rebased the clock there, and `ffplay`'s first report is the *device*
        // position 600 ms ahead (frozen + the suspension's length). That report
        // must be dropped, not adopted: the clock has to stay with the picture,
        // or the A/V guard suspends the sound for a lead that never existed and
        // the ring drains (17 frames in a 1009 ms second).
        control.freeze_audio_clock_at(12_000);
        control.rebase_audio_clock(true);
        assert!(control.set_audio_clock_ms(generation, 12_629));
        let after = control.audio_clock().unwrap_or(0);
        assert!(
            after < 12_100,
            "the resumed process' device position was adopted ({after})"
        );
        // The next report is the content position again, right after the freeze
        // point: it is inside the jitter band and is taken by the slew path.
        assert!(control.set_audio_clock_ms(generation, 12_040));
        let after = control.audio_clock().unwrap_or(0);
        assert!(
            (12_000..12_100).contains(&after),
            "the content position must be followed ({after})"
        );
    }

    // ---- frame-blending interpolation ------------------------------------

    fn interp_frame(w: u32, h: u32) -> FrameSize {
        FrameSize { w, h }
    }

    #[test]
    fn interp_plan_caps_every_mode_and_size_at_60hz() {
        // The decision: 平滑 outputs at most 60 fps, whatever the window shows
        // and whatever the mode. A 120/144/500 Hz display is read (it is what
        // makes generation possible and it sets the phase grid) but the output
        // grid is min(60, display_hz).
        for display_hz in [90u32, 120, 144, 165, 240, 500] {
            let plan = interp_plan_for(display_hz, 30.0).expect("a 120 Hz class display blends");
            assert_eq!(plan.out_hz, INTERP_MAX_OUT_HZ, "display {display_hz} Hz");
            assert_eq!(plan.out_hz, 60);
            assert_eq!(plan.reason, Some("60fps_cap"));
        }
        // At or below 60 the grid is the display's own rate, unclamped.
        let plan = interp_plan_for(60, 24.0).expect("60 Hz above a 24 fps track");
        assert_eq!(plan.out_hz, 60);
        assert_eq!(plan.reason, None);
        let plan = interp_plan_for(50, 24.0).expect("50 Hz above a 24 fps track");
        assert_eq!(plan.out_hz, 50);
        assert_eq!(plan.reason, None);
        // Degenerate readings: no display, no plan.
        assert!(interp_plan_for(0, 24.0).is_none());
        assert!(interp_plan_for(1, 24.0).is_none());
    }

    /// Every generating mode runs on the same 60 Hz grid at 120 Hz: the mode
    /// chooses which pass draws a slot (blend / block match / OFA), never the
    /// rate, and the frame size is not part of the resolution at all.
    #[test]
    fn interp_plan_never_exceeds_60hz_for_any_mode() {
        let plan = interp_plan_for(120, 30.0).expect("a 30 fps track on a 120 Hz display");
        assert_eq!(plan.out_hz, 60);
        for mode in [InterpMode::Blend, InterpMode::BlockMatch, InterpMode::Ofa] {
            let ticker = InterpTicker::new(plan.out_hz, 1.0, 0.0);
            assert_eq!(
                ticker.step_ms,
                1000.0 / 60.0,
                "mode {} must step a 60 Hz grid",
                mode.as_u8()
            );
        }
    }

    #[test]
    fn interp_plan_requires_a_faster_display_than_the_track() {
        assert!(interp_plan_for(60, 60.0).is_none());
        assert!(interp_plan_for(60, 59.94).is_none());
        assert!(interp_plan_for(30, 30.0).is_none());
        assert!(interp_plan_for(24, 25.0).is_none());
        assert!(interp_plan_for(60, 0.0).is_none());
        assert!(interp_plan_for(60, f64::NAN).is_none());
        // 60 Hz exists above a 24 fps track.
        assert_eq!(interp_plan_for(60, 24.0).map(|p| p.out_hz), Some(60));
    }

    /// The pacer produces display frames on the ticker grid: from a 30 fps
    /// track at a 60 Hz output exactly 60 display frames are generated per
    /// second, and none is requested from the decoder.
    #[test]
    fn interp_ticker_generates_two_frames_per_source_frame_at_60hz() {
        let frame_ms = 1000.0 / 30.0;
        let mut ticker = InterpTicker::new(60, 1.0, 0.0);
        let mut pushed = 0u64;
        // Ten seconds, so a single floating-point boundary at the one-second
        // mark cannot decide the result.
        for index in 0..300u64 {
            let start = index as f64 * frame_ms;
            let end = start + frame_ms;
            while ticker.next_ms < end {
                if ticker.next_ms >= start {
                    pushed += 1;
                }
                ticker.next_ms += ticker.step_ms;
            }
        }
        let per_s = pushed as f64 / 10.0;
        assert!(
            (per_s - 60.0).abs() <= 0.2,
            "10 s of a 30 fps track at 60 Hz generated {per_s:.1}/s"
        );
    }

    /// A different display changes the resolved rate mid-session (50 vs 60 on
    /// the same track). The grid has to be rebuilt for the new step: reusing a
    /// 50 Hz grid would keep pushing 50 frames/s while the panel said 60,
    /// because the step is what carries the rate.
    #[test]
    fn interp_ticker_grid_follows_the_resolved_rate() {
        let frame_ms = 1000.0 / 30.0;
        let ticks_over_ten_seconds = |hz: u32| {
            let mut ticker = InterpTicker::new(hz, 1.0, 0.0);
            let mut pushed = 0u64;
            for index in 0..300u64 {
                let start = index as f64 * frame_ms;
                let end = start + frame_ms;
                while ticker.next_ms < end {
                    if ticker.next_ms >= start {
                        pushed += 1;
                    }
                    ticker.next_ms += ticker.step_ms;
                }
            }
            pushed as f64 / 10.0
        };
        // Both grids are available below the 60 fps ceiling; only the step
        // differs, and the step is what carries the rate. (The 30 fps source
        // divides 60 exactly, so the counted grid is asserted there alone; a
        // 50 Hz grid against 30 fps is 5 ticks per 3 frames and its boundary
        // samples are not what this test is about.)
        assert!((ticks_over_ten_seconds(60) - 60.0).abs() <= 0.2);
        let slow_step = InterpTicker::new(50, 1.0, 0.0).step_ms;
        let fast_step = InterpTicker::new(60, 1.0, 0.0).step_ms;
        assert!(
            fast_step < slow_step,
            "a 60 Hz grid must step faster than a 50 Hz one ({fast_step} vs {slow_step})"
        );
    }

    #[test]
    fn interp_ticker_skips_late_ticks_instead_of_bursting() {
        let mut ticker = InterpTicker::new(60, 1.0, 0.0);
        let out_ms = ticker.step_ms;
        assert_eq!(ticker.skip_late(out_ms * 0.25), 0);
        // 2.5 periods late: two whole slots were missed, never replayed.
        assert_eq!(ticker.skip_late(out_ms * 2.5), 2);
        assert!(ticker.next_ms >= out_ms * 2.0 - 1.0e-9);
        assert!(ticker.next_ms <= out_ms * 2.5);
    }

    /// The display grid lives in the video timeline, so its step grows with
    /// the playback speed: 2× still pushes at the display rate, not twice it.
    #[test]
    fn interp_ticker_step_follows_playback_speed() {
        let normal = InterpTicker::new(60, 1.0, 0.0);
        let out_ms = 1000.0 / 60.0;
        assert!((normal.step_ms - out_ms).abs() < 1.0e-9);
        let fast = InterpTicker::new(60, 2.0, 0.0);
        assert!((fast.step_ms - out_ms * 2.0).abs() < 1.0e-9);
        let slow = InterpTicker::new(60, 0.5, 0.0);
        assert!((slow.step_ms - out_ms * 0.5).abs() < 1.0e-9);
        // A nonsense/zero speed falls back to real-time pacing.
        assert!((InterpTicker::new(60, 0.0, 0.0).step_ms - out_ms).abs() < 1.0e-9);
    }

    #[test]
    fn blend_endpoints_are_exact_and_alpha_stays_opaque() {
        let a = [0u8, 0, 0, 0, 255, 10, 20, 30];
        let b = [255u8, 255, 255, 255, 0, 200, 100, 50];
        let mut out = [0u8; 8];

        blend_rgba_into(&mut out, &a, &b, 0);
        assert_eq!(out, [0, 0, 0, 255, 255, 10, 20, 255]);
        blend_rgba_into(&mut out, &a, &b, 256);
        assert_eq!(out, [255, 255, 255, 255, 0, 200, 100, 255]);

        // Midpoint: 127/128 with the +128 rounding, alpha always 255.
        blend_rgba_into(&mut out, &a, &b, 128);
        assert_eq!(out[0], 128);
        assert_eq!(out[1], 128);
        assert_eq!(out[4], 128);
        assert_eq!(out[5], 105);
        assert_eq!(out[3], 255);
        assert_eq!(out[7], 255);

        // A long frame exercises the SSE2 body plus the scalar tail and must
        // agree with the scalar formula.
        let size = interp_frame(34, 2); // 272 bytes: 17 blocks of 16, no tail
        let mut a = vec![0u8; size.bytes()];
        let mut b = vec![0u8; size.bytes()];
        for (index, pixel) in a.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[index as u8, 40, 80, 255]);
        }
        for pixel in b.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[200, 160, 120, 255]);
        }
        let mut out = vec![0u8; size.bytes()];
        blend_rgba_into(&mut out, &a, &b, 64);
        for (index, pixel) in out.chunks_exact(4).enumerate() {
            let expected_r = ((index as u32 * 192 + 200 * 64 + 128) >> 8) as u8;
            assert_eq!(pixel[0], expected_r);
            assert_eq!(pixel[3], 255);
        }
    }

    /// SPIKE measurement (not part of the normal suite): the sustained output
    /// rate the pacer can actually push at the shipped ceiling, 60 Hz, for each
    /// frame size the decoder produces.
    ///
    /// It drives the real production functions on the real two-thread shape:
    /// a pacer thread that takes a pooled buffer and calls [`blend_rgba_into`],
    /// hands it to a UI thread, and the UI thread runs exactly the event-loop
    /// body of [`push_recycled_frame`] — `SharedPixelBuffer::new` + one
    /// `copy_from_slice` + `Image::from_rgba8` — then recycles the buffer into
    /// the free list. The sustainable rate is `min(pacer, ui)`: whichever side
    /// is slower is the ceiling, so both are reported.
    ///
    /// Run: `cargo test --release player::tests::spike_blend_pipe_at_60hz
    /// -- --ignored --nocapture --test-threads=1` with `BILI_DEBUG_LOG` set.
    #[test]
    #[ignore = "spike measurement: ~20 s; run explicitly with --ignored"]
    fn spike_blend_pipe_at_60hz() {
        use std::sync::mpsc;
        use std::sync::Mutex as StdMutex;

        let out_hz = INTERP_MAX_OUT_HZ;
        let period = Duration::from_secs_f64(1.0 / f64::from(out_hz));
        let run = Duration::from_secs(6);
        let warmup = Duration::from_millis(700);

        for (w, h, label) in [
            (1920u32, 1080u32, "1080p"),
            (1280, 720, "720p"),
            (960, 540, "540p"),
            (852, 480, "480p"),
        ] {
            let size = interp_frame(w, h);
            let bytes = size.bytes();
            let a = vec![0x20u8; bytes];
            let b = vec![0xC0u8; bytes];

            let free: Arc<StdMutex<Vec<Vec<u8>>>> =
                Arc::new(StdMutex::new((0..4).map(|_| vec![0u8; bytes]).collect()));
            let ui_times: Arc<StdMutex<Vec<f64>>> = Arc::new(StdMutex::new(Vec::new()));
            let ui_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            let ui_thread = {
                let free = free.clone();
                let ui_times = ui_times.clone();
                let ui_count = ui_count.clone();
                std::thread::spawn(move || {
                    // Held like the window property does, so the previous image
                    // is dropped only when the next one replaces it.
                    let mut kept: Option<slint::Image> = None;
                    let mut local: Vec<f64> = Vec::new();
                    while let Ok(bytes) = rx.recv() {
                        let started = Instant::now();
                        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
                        buffer.make_mut_bytes().copy_from_slice(&bytes);
                        let image = slint::Image::from_rgba8(buffer);
                        kept = Some(image);
                        local.push(started.elapsed().as_secs_f64() * 1000.0);
                        ui_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Recycle exactly like `BlendPool::recycle`.
                        if let Ok(mut free) = free.lock() {
                            free.push(bytes);
                        }
                    }
                    std::hint::black_box(&kept);
                    if let Ok(mut all) = ui_times.lock() {
                        all.extend(local);
                    }
                })
            };

            let mut pacer_times: Vec<f64> = Vec::new();
            let mut misses = 0u64;
            let mut generated = 0u64;
            let mut pushed = 0u64;
            let started = Instant::now();
            let mut tick = 0u64;
            loop {
                let elapsed = started.elapsed();
                if elapsed >= run {
                    break;
                }
                let deadline = started + period * (tick as u32);
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }
                tick += 1;
                // After warm-up, only the steady state is measured.
                let measure = started.elapsed() > warmup;

                let mut buffer = match free.lock().ok().and_then(|mut f| f.pop()) {
                    Some(buffer) => buffer,
                    None => {
                        misses += 1;
                        vec![0u8; bytes]
                    }
                };
                let blend_started = Instant::now();
                blend_rgba_into(&mut buffer, &a, &b, 128);
                std::hint::black_box(&buffer);
                let blend_ms = blend_started.elapsed().as_secs_f64() * 1000.0;
                if measure {
                    pacer_times.push(blend_ms);
                    generated += 1;
                }
                if tx.send(buffer).is_ok() {
                    pushed += 1;
                }
            }
            let elapsed = started.elapsed().as_secs_f64();
            drop(tx);
            let _ = ui_thread.join();
            let ui = ui_times.lock().map(|v| v.clone()).unwrap_or_default();

            let p95 = |mut v: Vec<f64>| -> f64 {
                if v.is_empty() {
                    return 0.0;
                }
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)]
            };
            let mean = |v: &[f64]| -> f64 {
                if v.is_empty() {
                    0.0
                } else {
                    v.iter().sum::<f64>() / v.len() as f64
                }
            };
            let pacer_avg = mean(&pacer_times);
            let pacer_p95 = p95(pacer_times.clone());
            let ui_avg = mean(&ui);
            let ui_p95 = p95(ui.clone());
            let ui_done = ui_count.load(std::sync::atomic::Ordering::Relaxed);
            let budget = period.as_secs_f64() * 1000.0;
            let worst = pacer_avg.max(ui_avg);
            // `pacer_times` only starts after the warm-up, so divide by the
            // measured part of the window, not by the whole run.
            let measured_s = (elapsed - warmup.as_secs_f64()).max(1.0e-9);
            let gen_hz = generated as f64 / measured_s;
            let ui_hz = ui_done as f64 / elapsed;
            // Bytes the pipe actually moves: one write + one read per blend,
            // then the UI copy out of the pool.
            let blend_gbs = (bytes as f64 * 3.0 * generated as f64) / measured_s / 1.0e9;
            let ui_gbs = (bytes as f64 * ui_done as f64) / elapsed / 1.0e9;
            crate::debug_log(&format!(
                "spike pipe size={label} {w}x{h} target_hz={out_hz} \
                 generated={gen_hz:.1}/s out_hz={:.1} pacer_avg_ms={pacer_avg:.3} pacer_p95_ms={pacer_p95:.3} \
                 ui_avg_ms={ui_avg:.3} ui_p95_ms={ui_p95:.3} ui_hz={ui_hz:.1} \
                 budget_ms={budget:.2} headroom_x={:.2} miss={misses} \
                 blend_gbs={blend_gbs:.2} ui_gbs={ui_gbs:.2} \
                 pushed={pushed} generated_n={generated} ui_n={ui_done} window_s={elapsed:.1}",
                gen_hz.min(f64::from(out_hz)),
                budget / worst.max(1.0e-9),
            ));
        }
    }

    /// Per-frame blend cost at the two sizes the player actually decodes.
    ///
    /// The QA target is < 3 ms for 1080p. That number is measured in a
    /// dedicated run (`cargo test --release player::tests::blend_cost...` with
    /// `BILI_DEBUG_LOG`), because this suite runs many memory-heavy tests in
    /// parallel and the pacer thread is regularly preempted: the best-of-N and
    /// the mean are both logged, but a hard 3 ms assertion here would measure
    /// the test scheduler, not the blend. The guard below is the frame budget
    /// (8 ms, the same bar the existing UI-copy test uses) and still catches a
    /// real regression such as falling back to a scalar loop (~15-20 ms).
    #[test]
    fn blend_cost_is_within_the_1080p_budget() {
        for (w, h, label) in [(1920u32, 1080u32, "1080p"), (1280, 720, "720p")] {
            let size = interp_frame(w, h);
            let a = vec![0x20u8; size.bytes()];
            let b = vec![0xC0u8; size.bytes()];
            let mut out = vec![0u8; size.bytes()];
            let mut copy_out = vec![0u8; size.bytes()];
            let mut best = f64::MAX;
            for _ in 0..12 {
                let started = Instant::now();
                blend_rgba_into(&mut out, &a, &b, 128);
                std::hint::black_box(&out);
                best = best.min(started.elapsed().as_secs_f64() * 1000.0);
            }
            // Steady-state cost: every frame writes a pooled buffer (which the
            // runtime rotates), so the mean is the honest per-frame figure the
            // pacer pays; the best run only shows the arithmetic ceiling.
            let mut times = Vec::with_capacity(30);
            for _ in 0..30 {
                let started = Instant::now();
                blend_rgba_into(&mut out, &a, &b, 128);
                std::hint::black_box(&out);
                times.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let avg = times.iter().sum::<f64>() / times.len() as f64;
            let median = times[times.len() / 2];
            // Reference: the same frame moved by `memcpy`, to tell a busy
            // machine (both slow) from a slow blend (only the blend slow).
            let mut reference = f64::MAX;
            for _ in 0..12 {
                let started = Instant::now();
                copy_out.copy_from_slice(&a);
                std::hint::black_box(&copy_out);
                reference = reference.min(started.elapsed().as_secs_f64() * 1000.0);
            }
            let mb = size.bytes() as f64 / 1.0e6;
            crate::debug_log(&format!(
                "interp blend ms_best={best:.3} ms_median={median:.3} ms_avg={avg:.3} ref_copy_ms={reference:.3} target_3ms_met={} size={w}x{h} class={label} bytes_mb={mb:.1}",
                u8::from(best < 3.0)
            ));
            assert!(
                best < 8.0,
                "{label} blend best {best:.3} ms exceeds the 8 ms frame-budget guard \
                 (the 3 ms QA target is measured separately)"
            );
        }
    }

    #[test]
    fn interp_mode_api_normalises_and_reports_stats() {
        let control = PlaybackControl::new();
        assert_eq!(control.interp_mode(), 0);
        assert_eq!(control.interp_stats(), (0, 0, 0.0));
        assert_eq!(control.interp_bm_ms(), 0.0);
        assert_eq!(control.interp_mode_text(), "关闭");

        control.set_interp_mode(1);
        assert_eq!(control.interp_mode(), 1);
        control.note_interp_frame(2_000);
        control.note_interp_dropped(3);
        let (generated, dropped, avg_ms) = control.interp_stats();
        assert_eq!((generated, dropped), (1, 3));
        assert!((avg_ms - 2.0).abs() < 1.0e-6);
        // Blend runs no motion pairs.
        assert_eq!(control.interp_bm_ms(), 0.0);

        // Re-selecting the same mode is a no-op and keeps the episode counters.
        control.set_interp_mode(1);
        assert_eq!(control.interp_mode(), 1);
        assert_eq!(control.interp_stats(), (1, 3, 2.0));

        // Mode 2 is a mode of its own: its label, its reset, and a pair cost
        // that is tracked separately from the per-display-frame average.
        control.set_interp_mode(2);
        assert_eq!(control.interp_mode(), 2);
        assert_eq!(control.interp_mode_text(), "块匹配");
        assert_eq!(control.interp_stats(), (0, 0, 0.0));
        assert_eq!(control.interp_bm_ms(), 0.0);
        control.note_motion_pair(6.0);
        control.note_motion_pair(8.0);
        assert!((control.interp_bm_ms() - 7.0).abs() < 1.0e-6);
        // Percentiles come from the 0.5 ms histogram (bucket upper edges).
        let (p50, p95) = control.interp_bm_percentiles();
        assert!((p50 - 6.5).abs() < 1.0e-6, "bm p50 {p50}");
        assert!((p95 - 8.5).abs() < 1.0e-6, "bm p95 {p95}");

        // Mode 3 is the hardware optical-flow path: its own label, its own
        // counters and its own reset, independent of the block-matching ones.
        control.set_interp_mode(3);
        assert_eq!(control.interp_mode(), 3);
        assert_eq!(control.interp_mode_text(), "OFA");
        assert_eq!(control.interp_stats(), (0, 0, 0.0));
        assert_eq!(control.interp_ofa_ms(), 0.0);
        assert_eq!(control.interp_ofa_flow_ms(), 0.0);
        control.note_ofa_submit();
        control.note_ofa_submit();
        assert_eq!(control.interp_ofa_submits(), 2);
        control.note_ofa_pair(9.5, 0.4);
        control.note_ofa_pair(10.5, 0.6);
        assert!((control.interp_ofa_ms() - 10.0).abs() < 1.0e-6);
        assert!((control.interp_ofa_flow_ms() - 0.5).abs() < 1.0e-6);
        let (p50, p95) = control.interp_ofa_percentiles();
        assert!((p50 - 10.0).abs() < 1.0e-6, "ofa p50 {p50}");
        assert!((p95 - 11.0).abs() < 1.0e-6, "ofa p95 {p95}");
        // The block-matching counters stayed at zero: the two modes never mix.
        assert_eq!(control.interp_bm_ms(), 0.0);

        // Corrupt / future values fall back to off, and switching off clears the
        // stats so the disabled state honestly reports zero work.
        control.set_interp_mode(255);
        assert_eq!(control.interp_mode(), 0);
        assert_eq!(control.interp_stats(), (0, 0, 0.0));
        assert_eq!(control.interp_bm_ms(), 0.0);
        assert_eq!(control.interp_ofa_ms(), 0.0);
        assert_eq!(control.interp_ofa_submits(), 0);
        control.set_interp_mode(1);
        control.note_interp_frame(1_000);
        control.set_interp_mode(0);
        assert_eq!(control.interp_mode(), 0);
        assert_eq!(control.interp_stats(), (0, 0, 0.0));
    }

    #[test]
    fn interp_mode_cycle_matches_the_ui() {
        // The window cycles `(mode + 1) % 4`; every step must land on a mode the
        // pacer implements, and back at off after four clicks. Mode 3 is
        // skipped by the window while the runtime is missing (see `main.rs`),
        // which this cycle helper does not model: `from_u8` still has to agree
        // with the numbering.
        let mut mode = 0u8;
        let mut seen = Vec::new();
        for _ in 0..4 {
            mode = (mode + 1) % 4;
            seen.push(InterpMode::from_u8(mode));
        }
        assert_eq!(
            seen,
            vec![
                InterpMode::Blend,
                InterpMode::BlockMatch,
                InterpMode::Ofa,
                InterpMode::Off
            ]
        );
        assert_eq!(InterpMode::Off.text(), "关闭");
        assert_eq!(InterpMode::Blend.text(), "混合");
        assert_eq!(InterpMode::BlockMatch.text(), "块匹配");
        assert_eq!(InterpMode::Ofa.text(), "OFA");
    }

    #[test]
    fn blockmatch_clamps_to_blend_above_720p() {
        assert!(interp_motion_supported(interp_frame(1280, 720)));
        assert!(interp_motion_supported(interp_frame(960, 540)));
        assert!(!interp_motion_supported(interp_frame(1920, 1080)));
        assert!(!interp_motion_supported(interp_frame(1280, 722)));
        // The mode keeps its label and stats above the cap; only the per-slot
        // path falls back to a plain blend (logged once per session).
        assert_eq!(InterpMode::from_u8(2), InterpMode::BlockMatch);
        assert_eq!(InterpMode::from_u8(3), InterpMode::Ofa);
        assert_eq!(InterpMode::from_u8(4), InterpMode::Off);
    }

    #[test]
    fn ofa_budget_backs_off_and_probes_again() {
        let mut budget = OfaBudget::new();
        // The warm-up pairs always run, whatever they cost.
        for _ in 0..OFA_WARMUP_PAIRS {
            assert!(budget.should_use(8.0));
            budget.record(20.0, 8.0);
        }
        // A measured over-budget pair starts the backoff: the first refusal
        // schedules four skipped opportunities, and the fourth skip after it is
        // the probe.
        assert!(!budget.should_use(8.0));
        assert!(!budget.should_use(8.0));
        assert!(!budget.should_use(8.0));
        assert!(!budget.should_use(8.0));
        assert!(budget.should_use(8.0));
        // A pair that fits again clears it.
        budget.record(3.0, 8.0);
        assert_eq!(budget.backoff, 0);
        assert_eq!(budget.skip_left, 0);
        for _ in 0..6 {
            budget.record(3.0, 8.0);
        }
        assert!(budget.should_use(8.0));
    }

    #[test]
    fn ofa_clamps_to_blend_above_1080p() {
        // The size half of the mode-3 gate is pure policy and testable without a
        // GPU. The engine half is resolved only after the first frame, so this
        // function deliberately answers from the frame size alone.
        assert!(interp_ofa_size_ok(interp_frame(1920, 1080)));
        assert!(!interp_ofa_size_ok(interp_frame(2560, 1440)));
        assert!(!interp_ofa_size_ok(interp_frame(3840, 2160)));
    }

    #[test]
    fn interp_stats_text_reports_rate_and_cost_or_a_dash() {
        let control = PlaybackControl::new();
        assert_eq!(control.interp_stats_text(), "—");
        control.set_interp_mode(1);
        // Nothing generated yet: still a dash, not a fake 0 ms.
        assert_eq!(control.interp_stats_text(), "—");
        control.set_interp_out_hz(60);
        control.note_interp_frame(2_400);
        assert_eq!(control.interp_stats_text(), "60 fps · 2.4 ms");
        // Off reports a dash even though the last episode's numbers remain.
        control.set_interp_mode(0);
        assert_eq!(control.interp_stats_text(), "—");
    }

    #[test]
    fn nv12_conversion_matches_the_scalar_reference_exactly() {
        // Widths below and above one SSE2 step (8 px), so the vector body and
        // the scalar tail are both exercised, plus every Y/U/V extreme. NV12
        // subsamples chroma by two in both directions, so frames (and these
        // fixtures) are always even.
        for w in [4usize, 8, 12, 40] {
            let h = 6usize;
            let size = FrameSize {
                w: w as u32,
                h: h as u32,
            };
            let mut src = vec![0u8; nv12_bytes(size)];
            for (index, byte) in src.iter_mut().enumerate() {
                *byte = ((index * 37) % 256) as u8;
            }
            // Paint the planar extremes into the first pixels.
            src[0] = 16; // video black
            src[1] = 235; // video white
            src[w * h] = 16; // U
            src[w * h + 1] = 240; // V
            let mut vector = vec![0u8; size.bytes()];
            let mut scalar = vec![0u8; size.bytes()];
            nv12_to_rgba(&mut vector, &src, w, h);
            nv12_to_rgba_scalar(&mut scalar, &src, w, h);
            assert_eq!(vector, scalar, "w={w}");
            // Alpha is opaque everywhere, which the UI blits without blending.
            assert!(vector.chunks_exact(4).all(|px| px[3] == 255));
        }
    }

    #[test]
    fn nv12_conversion_keeps_the_1080p_reader_cost_small() {
        let size = interp_frame(1920, 1080);
        let src = vec![0x80u8; nv12_bytes(size)];
        let mut out = vec![0u8; size.bytes()];
        let mut times = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            nv12_to_rgba(&mut out, &src, size.w as usize, size.h as usize);
            std::hint::black_box(&out);
            times.push(started.elapsed().as_secs_f32() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = times[times.len() / 2];
        crate::debug_log(&format!(
            "nv12 conv ms_median={median:.3} ms_best={:.3} frame={}x{}",
            times[0], size.w, size.h
        ));
        // The reader thread has a whole frame period (33 ms at 30 fps) for this;
        // the swscale conversion it replaces cost ffmpeg 0.36 cores at this size.
        assert!(median < 6.0, "nv12 conversion too slow: {median:.3} ms");
    }

    /// Decode one frame of a **real** DASH file to NV12 through the app's pipe,
    /// convert it with [`nv12_to_rgba`], and compare the result with **ffmpeg's
    /// own** NV12→RGBA conversion of the same bytes. This is the format/colour
    /// check for the pipe change: it holds the upsampling to the definition of
    /// correct instead of to a different pipeline.
    ///
    /// `BILI_SAMPLE_M4S=<file>` is required (a real `.m4s`/`.mp4`); without it
    /// the test is skipped, so a normal `cargo test` needs no media fixture.
    #[test]
    #[ignore = "needs a real media file via BILI_SAMPLE_M4S"]
    fn nv12_pipe_matches_swscales_rgba() {
        let Ok(sample) = std::env::var("BILI_SAMPLE_M4S") else {
            crate::debug_log("nv12 pipe check skipped reason=no-sample");
            return;
        };
        // The shipped path: the scaler emits the decoder's own 4:2:0 at the
        // source size, so nothing is downsampled here.
        let size = interp_frame(320, 180);
        let run_ffmpeg = |args: &[&str], stdin_raw: Option<&[u8]>| -> Vec<u8> {
            let mut child = base_command("ffmpeg.exe")
                .args(args)
                .stdin(if stdin_raw.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("ffmpeg");
            if let Some(bytes) = stdin_raw {
                use std::io::Write;
                let mut pipe = child.stdin.take().expect("stdin");
                pipe.write_all(bytes).expect("write nv12");
                drop(pipe);
            }
            let out = child.wait_with_output().expect("ffmpeg wait");
            out.stdout
        };
        let nv12 = run_ffmpeg(
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-i",
                &sample,
                "-an",
                "-frames:v",
                "1",
                "-vf",
                &format!("scale={}:{}", size.w, size.h),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "nv12",
                "-",
            ],
            None,
        );
        assert_eq!(nv12.len(), nv12_bytes(size), "nv12 frame size");
        // ffmpeg's own NV12 -> RGBA of exactly those bytes.
        let reference = run_ffmpeg(
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "nv12",
                "-s",
                &format!("{}x{}", size.w, size.h),
                "-i",
                "-",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "-",
            ],
            Some(&nv12),
        );
        assert_eq!(reference.len(), size.bytes(), "rgba frame size");
        let mut converted = vec![0u8; size.bytes()];
        nv12_to_rgba(&mut converted, &nv12, size.w as usize, size.h as usize);
        let mut worst = 0i32;
        let mut total = 0i64;
        for (a, b) in converted.chunks_exact(4).zip(reference.chunks_exact(4)) {
            for channel in 0..3 {
                let delta = (a[channel] as i32 - b[channel] as i32).abs();
                worst = worst.max(delta);
                total += delta as i64;
            }
            assert_eq!(a[3], 255);
        }
        let mean = total as f64 / (size.w as f64 * size.h as f64 * 3.0);
        crate::debug_log(&format!(
            "nv12 pipe check frame={}x{} max_delta={worst} mean_delta={mean:.3}",
            size.w, size.h
        ));
        // Integer 8.8 maths against swscale's tables. A wrong matrix, range or
        // chroma layout shows up as a mean above 5 (it was 67 when a biased and
        // an unbiased chroma sample were mixed). The residual ~2.5 mean / 33 max
        // is chroma-upsampling phase on sharp edges: swscale interpolates where
        // this duplicates, and no integer implementation matches it exactly.
        assert!(worst <= 40, "colour delta too large: {worst}");
        assert!(mean < 3.0, "mean colour delta too large: {mean:.3}");
    }
}
