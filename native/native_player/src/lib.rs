//! O11A1 native player — a Slint-free video window and the library API the
//! future main UI embeds.
//!
//! ```no_run
//! use native_player::{Player, PlayerConfig};
//!
//! let player = Player::open(PlayerConfig {
//!     bvid: Some("BV1mhxo6BEG4".into()),
//!     ..Default::default()
//! })?;
//! player.wait();
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! The window is one thread: it owns the DirectComposition tree, both
//! swapchains, the pacer and the message pump. Commands are sent through a
//! channel; position/duration/state are read from atomics, so calling
//! [`Player::position_ms`] from a UI thread never blocks on the video thread.

mod audio;
mod clipboard;
mod convert;
mod danmaku;
mod decode;
mod detail;
mod engine;
mod enhance;
mod format;
pub mod gdi_text;
mod gpu;
mod heartbeat;
mod interp;
mod log;
mod overlay;
mod proc;
mod source;
mod state;
mod storyboard;
mod window;

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Start the native window-move loop for a host window (Electron player).
pub fn drag_window(hwnd: isize) {
    window::drag_window(windows::Win32::Foundation::HWND(hwnd as *mut _));
}

pub use danmaku::DanmakuHit;
pub use engine::Command;
pub use enhance::EnhanceMode;
pub use format::{format_time, format_time_ms, parse_time_ms};
pub use gpu::AspectMode;
pub use interp::InterpMode;
pub use state::PlayerState;
use engine::SharedState;

/// What to open.
///
/// `direct_url` (or the `BILI_PLAYER_URL` environment variable) wins over the
/// Bilibili fields; it may be an HTTP(S) URL or a local file path. With neither,
/// `BILI_OPEN_BVID` (default `BV1mhxo6BEG4`) is resolved through `video_view` +
/// `playurl` using the saved account when one exists.
/// Electron embedding: the player paints into an owned `WS_POPUP` window
/// positioned by the host instead of creating its own chrome. The native
/// overlay is suppressed; video and danmaku stay on the Rust side.
#[derive(Clone, Debug, Default)]
pub struct PlayerHost {
    /// Electron `BrowserWindow` HWND (the popup becomes its owned window).
    pub parent: isize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Default)]
pub struct PlayerConfig {
    pub bvid: Option<String>,
    pub cid: Option<u64>,
    pub direct_url: Option<String>,
    /// Separate audio URL for a DASH direct source (PGC fallback). When empty,
    /// the direct URL itself carries both streams (combined durl).
    pub direct_audio_url: Option<String>,
    pub title: String,
    pub start_ms: u64,
    pub quality_qn: Option<i64>,
    /// When set, create an owned video-only window at this rectangle instead of
    /// the standalone player window with its native UI.
    pub host: Option<PlayerHost>,
    /// Video codec preference for the initial open (`7` AVC, `12` HEVC,
    /// `13` AV1, `0` automatic). Defaults to AVC.
    pub quality_codecid: Option<i64>,
    /// Initial DASH audio track index; `None` picks the best non-VIP stream.
    pub audio_index: Option<usize>,
}

/// A running native player window.
pub struct Player {
    commands: Sender<Command>,
    shared: Arc<SharedState>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Resolve the source and create the window. Blocks until the composition
    /// tree is up (or reports why it could not be built).
    pub fn open(config: PlayerConfig) -> Result<Self> {
        let state = PlayerState::load();
        log::line(&format!(
            "player state file={} volume={} muted={} speed={:.2} aspect={} brightness={:.2} audio_only={} ontop={} danmaku={} qn={:?} codecid={:?} audio_index={:?}",
            state::state_path().display(),
            state.volume,
            u8::from(state.muted),
            state.speed,
            state.aspect,
            state.brightness,
            u8::from(state.audio_only),
            u8::from(state.topmost),
            u8::from(state.danmaku_visible),
            state.quality_qn,
            state.quality_codecid,
            state.audio_index
        ));
        // The stored stream preference only fills fields the caller left open;
        // an explicit --quality/quality_qn still wins. A video that lacks the
        // stored tier falls back inside `source::resolve`.
        let mut config = config;
        if config.quality_qn.is_none() {
            config.quality_qn = state.quality_qn;
        }
        if config.quality_codecid.is_none() {
            config.quality_codecid = state.quality_codecid;
        }
        if config.audio_index.is_none() {
            config.audio_index = state.audio_index;
        }
        let resolved = source::resolve(&config).context("resolve media source")?;
        let source = resolved.source;
        let session = resolved.session.map(Arc::new);
        log::line(&format!(
            "open title={:?} duration_ms={} start_ms={} qn={} codec={}",
            source.title,
            source.duration_ms,
            clamp_start(config.start_ms, source.duration_ms),
            source.qn,
            source.codec
        ));
        let (command_tx, command_rx) = mpsc::channel();
        let (danmaku_tx, danmaku_rx) = mpsc::channel();
        let engine_danmaku_tx = danmaku_tx.clone();
        let bvid = source.bvid.clone();
        let cid = source.cid;
        if let (Some(session), Some(cid)) = (session.clone(), cid) {
            let duration_ms = source.duration_ms;
            log::line(&format!(
                "danmaku fetch queued bvid={} cid={cid} duration_ms={duration_ms}",
                source.bvid.as_deref().unwrap_or("-")
            ));
            let sender = danmaku_tx.clone();
            match std::thread::Builder::new()
                .name("player-danmaku-fetch".into())
                .spawn(move || {
                    let items = danmaku::fetch_with_session(session, cid, duration_ms);
                    danmaku::send_result(&sender, items);
                }) {
                Ok(_) => {}
                Err(error) => {
                    log::line(&format!("danmaku fetch thread failed: {error}"));
                    let _ = danmaku_tx.send(Vec::new());
                }
            }
        } else {
            let _ = danmaku_tx.send(Vec::new());
        }
        drop(danmaku_tx);
        // The right-strip worker shares the same session as the player. Without
        // a Bilibili source (direct URL) it still gets channels so the engine
        // never has to branch on Option.
        let engine_session = session.clone();
        let (detail_tx, detail_rx) = match (session, bvid) {
            (Some(session), Some(bvid)) => {
                detail::spawn(session, bvid, cid.unwrap_or(0))
            }
            _ => {
                let (tx, rx) = mpsc::channel();
                drop(rx);
                (tx, mpsc::channel().1)
            }
        };
        let shared = Arc::new(SharedState::default());
        shared
            .duration_ms
            .store(source.duration_ms, std::sync::atomic::Ordering::SeqCst);
        shared
            .position_ms
            .store(config.start_ms, std::sync::atomic::Ordering::SeqCst);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread_config = config.clone();
        let thread_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("native-player".into())
            .spawn(move || {
                engine::run(
                    thread_config,
                    source,
                    thread_shared,
                    command_rx,
                    danmaku_rx,
                    engine_danmaku_tx,
                    detail_tx,
                    detail_rx,
                    engine_session,
                    thread_state,
                    ready_tx,
                );
            })
            .context("spawn player thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(())) => Ok(Self {
                commands: command_tx,
                shared,
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                let _ = thread.join();
                Err(anyhow!("player setup failed: {reason}"))
            }
            Err(error) => {
                let _ = command_tx.send(Command::Close);
                let _ = thread.join();
                Err(anyhow!("player setup timed out: {error}"))
            }
        }
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn play(&self) {
        self.send(Command::Play);
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn toggle_play(&self) {
        self.send(Command::TogglePlay);
    }

    /// Move/resize the embedded popup (physical pixels, screen coordinates).
    pub fn set_rect(&self, x: i32, y: i32, width: u32, height: u32) {
        self.send(Command::SetRect {
            x,
            y,
            width,
            height,
        });
    }

    /// Show/hide the embedded popup (default player window ignores it).
    pub fn set_visible(&self, visible: bool) {
        self.send(Command::SetVisible(visible));
    }

    /// Hovered seek time for the native storyboard thumbnail; `None` clears it.
    pub fn set_preview(&self, time_ms: Option<u64>) {
        self.send(Command::Preview(time_ms));
    }

    /// Put a freshly sent comment on screen immediately.
    pub fn send_danmaku(&self, text: &str, mode: u32) {
        self.send(Command::SendDanmaku {
            text: text.to_string(),
            mode,
        });
    }

    /// Seek to `position_ms`; clamped to the media duration and its safe tail.
    pub fn seek(&self, position_ms: u64) {
        self.send(Command::Seek(position_ms));
    }

    /// 0..=100.
    pub fn set_volume(&self, volume: u8) {
        self.send(Command::SetVolume(volume.min(100)));
    }

    /// 0.5×..2.0× (the audio uses ffmpeg's `atempo`, the picture reopens at the
    /// same position on the new pacing).
    pub fn set_speed(&self, speed: f64) {
        self.send(Command::SetSpeed(speed));
    }

    pub fn set_interp(&self, mode: InterpMode) {
        self.send(Command::SetInterp(mode));
    }

    pub fn set_enhance(&self, mode: EnhanceMode) {
        self.send(Command::SetEnhance(mode));
    }

    /// Toggle the stats panel (the `P` key does the same in the window).
    pub fn toggle_stats(&self) {
        self.send(Command::ToggleStats);
    }

    /// 弹幕 on/off (`D` in the window; `BILI_PLAYER_DANMAKU=0` starts off).
    pub fn set_danmaku_visible(&self, visible: bool) {
        self.send(Command::SetDanmaku(visible));
    }

    pub fn toggle_danmaku(&self) {
        self.send(Command::ToggleDanmaku);
    }

    /// 迷你窗: shrink to a 480×270 topmost player and back to the exact saved
    /// rect. Ctrl+M / Alt+M and double-click do the same in the window.
    pub fn toggle_mini(&self) {
        self.send(Command::ToggleMini);
    }

    pub fn is_mini(&self) -> bool {
        self.shared
            .mini
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 比例: 适应 / 填充 / 拉伸 (`A` in the window).
    pub fn set_aspect(&self, aspect: AspectMode) {
        self.send(Command::SetAspect(aspect));
    }

    pub fn cycle_aspect(&self) {
        self.send(Command::CycleAspect);
    }

    pub fn aspect(&self) -> AspectMode {
        AspectMode::from_u8(self.shared.aspect.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Always-on-top (`T`). Mini is always topmost; its restore returns to this
    /// state.
    pub fn set_always_on_top(&self, ontop: bool) {
        self.send(Command::SetOnTop(ontop));
    }

    pub fn toggle_always_on_top(&self) {
        self.send(Command::ToggleOnTop);
    }

    pub fn is_topmost(&self) -> bool {
        self.shared
            .topmost
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 0.4..=2.0 picture-only brightness; 1.0 is 100% (`-` / `=`).
    pub fn set_brightness(&self, brightness: f32) {
        self.send(Command::SetBrightness(brightness));
    }

    pub fn brightness(&self) -> f32 {
        self.shared
            .brightness_milli
            .load(std::sync::atomic::Ordering::SeqCst) as f32
            / 1000.0
    }

    /// 仅音频: keep the sound (and danmaku) while the video layer is black
    /// (`V`).
    pub fn set_audio_only(&self, audio_only: bool) {
        self.send(Command::SetAudioOnly(audio_only));
    }

    pub fn toggle_audio_only(&self) {
        self.send(Command::ToggleAudioOnly);
    }

    pub fn is_audio_only(&self) -> bool {
        self.shared
            .audio_only
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Capture the current composited window to
    /// `<o11a1 paths root>/screenshots/O11A1_<timestamp>.png` (`S`).
    pub fn screenshot(&self) {
        self.send(Command::Screenshot);
    }

    /// Step through 0.5/0.75/1.0/1.25/1.5/2.0× (`[` / `]`).
    pub fn speed_down(&self) {
        self.send(Command::CycleSpeed(-1));
    }

    pub fn speed_up(&self) {
        self.send(Command::CycleSpeed(1));
    }

    pub fn danmaku_visible(&self) -> bool {
        self.shared
            .danmaku_visible
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of danmaku comments fetched for this video.
    pub fn danmaku_count(&self) -> u32 {
        self.shared
            .danmaku_total
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The picture position on the media timeline, in ms.
    pub fn position_ms(&self) -> u64 {
        self.shared
            .position_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared
            .duration_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_playing(&self) -> bool {
        self.shared
            .playing
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hosted-video click counters (Electron turns them into play/pause).
    pub fn click_seq(&self) -> u64 {
        self.shared
            .click_seq
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn double_click_seq(&self) -> u64 {
        self.shared
            .double_click_seq
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn move_seq(&self) -> u64 {
        self.shared
            .move_seq
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Pop one right-clicked danmaku, if the native hit test found one.
    pub fn take_danmaku_hit(&self) -> Option<DanmakuHit> {
        self.shared
            .danmaku_hits
            .lock()
            .ok()
            .and_then(|mut queue| queue.pop_front())
    }

    pub fn long_press_seq(&self) -> u64 {
        self.shared
            .long_press_seq
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn take_long_press_hit(&self) -> Option<DanmakuHit> {
        self.shared
            .long_press_hits
            .lock()
            .ok()
            .and_then(|mut queue| queue.pop_front())
    }

    pub fn volume(&self) -> u8 {
        self.shared.volume.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn muted(&self) -> bool {
        self.shared.muted.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn speed(&self) -> f64 {
        self.shared
            .speed_milli
            .load(std::sync::atomic::Ordering::SeqCst) as f64
            / 1000.0
    }

    pub fn interp(&self) -> InterpMode {
        InterpMode::from_u8(self.shared.interp.load(std::sync::atomic::Ordering::SeqCst))
    }

    pub fn enhance(&self) -> EnhanceMode {
        EnhanceMode::from_u8(self.shared.enhance.load(std::sync::atomic::Ordering::SeqCst))
    }

    pub fn is_open(&self) -> bool {
        !self.shared
            .closed
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// True when the decoder reached the natural end of the stream.
    pub fn ended(&self) -> bool {
        self.shared
            .ended
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Real presented frames per second over the last second.
    pub fn fps(&self) -> f32 {
        self.shared
            .fps_tenths
            .load(std::sync::atomic::Ordering::SeqCst) as f32
            / 10.0
    }

    /// Block until the window is closed (by the user or a QA auto-exit).
    pub fn wait(&self) {
        while self.is_open() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Send the close command and join the player thread. Idempotent.
    pub fn close(&mut self) {
        self.send(Command::Close);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.shared
            .closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.close();
    }
}

fn clamp_start(start_ms: u64, duration_ms: u64) -> u64 {
    format::clamp_seek(start_ms, duration_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_to_the_brief_video() {
        let config = PlayerConfig::default();
        assert!(config.bvid.is_none());
        assert_eq!(config.start_ms, 0);
        assert!(config.title.is_empty());
    }
}
