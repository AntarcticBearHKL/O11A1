//! Player-window state that should survive a restart.
//!
//! Everything here is *window* state (volume, mute, speed, aspect, brightness,
//! audio-only, topmost, 弹幕 visibility, and the last chosen stream). It lives
//! in its own `player_state.json` under [`o11a1::paths::root`] so a player
//! restart does not have to rewrite `settings.json` and so the 单曲循环 flag
//! stays in core settings where the shell owns it.
//!
//! Saves are debounced: [`SaveDebouncer`] is a pure deadline tracker the engine
//! ticks once per loop, so a brightness drag or a volume wheel burst writes one
//! file after the interaction settles instead of one file per event.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// File name inside `o11a1::paths::root()`.
pub const FILE_NAME: &str = "player_state.json";

/// Default debounce window for [`SaveDebouncer`].
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(450);

/// Persisted window/stream state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerState {
    /// 0..=100; the audio engine is started with this value.
    pub volume: u8,
    pub muted: bool,
    /// 0.5..=2.0. Applied before the decoder/audio sessions open.
    pub speed: f32,
    /// [`crate::gpu::AspectMode`] value (0 适应, 1 填充, 2 拉伸).
    pub aspect: u8,
    /// 0.4..=2.0 picture-only brightness, 1.0 = 100%.
    pub brightness: f32,
    pub audio_only: bool,
    pub topmost: bool,
    pub danmaku_visible: bool,
    /// Last explicitly selected video stream (`qn` + `codecid`).
    pub quality_qn: Option<i64>,
    pub quality_codecid: Option<i64>,
    /// Last explicitly selected DASH audio track index.
    pub audio_index: Option<usize>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            volume: 80,
            muted: false,
            speed: 1.0,
            aspect: 0,
            brightness: 1.0,
            audio_only: false,
            topmost: false,
            danmaku_visible: true,
            quality_qn: None,
            quality_codecid: None,
            audio_index: None,
        }
    }
}

impl PlayerState {
    /// Load and normalise; a missing or corrupt file yields the defaults.
    pub fn load() -> Self {
        Self::load_from(&state_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(data) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(mut state) = serde_json::from_str::<Self>(&data) else {
            return Self::default();
        };
        state.normalise();
        state
    }

    /// Write the current state under the app root.
    pub fn save(&self) -> Result<()> {
        self.save_to(&state_path())
    }

    /// Serialise to `path` through a sibling temporary file so a killed player
    /// cannot leave a half-written JSON document behind.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut state = self.clone();
        state.normalise();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create player state directory {}", parent.display())
                })?;
            }
        }
        let data = serde_json::to_string_pretty(&state).context("serialise player state")?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, data)
            .with_context(|| format!("write {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Clamp every value to the range the UI can actually name. Idempotent:
    /// loading a normalised file yields the same state again.
    pub fn normalise(&mut self) {
        self.volume = self.volume.min(100);
        if !self.speed.is_finite() {
            self.speed = 1.0;
        }
        self.speed = self.speed.clamp(0.5, 2.0);
        if !self.brightness.is_finite() {
            self.brightness = 1.0;
        }
        self.brightness = self.brightness.clamp(0.4, 2.0);
        if self.aspect > 2 {
            self.aspect = 0;
        }
        if !matches!(self.quality_codecid, Some(7 | 12 | 13)) {
            self.quality_codecid = None;
        }
        if !matches!(self.quality_qn, Some(qn) if qn > 0) {
            self.quality_qn = None;
        }
    }
}

/// `player_state.json` beside `settings.json`.
pub fn state_path() -> PathBuf {
    let mut path = o11a1::paths::root();
    path.push(FILE_NAME);
    path
}

/// Deadline tracker for debounced writes. Nothing here touches the disk, so
/// the deadline logic is unit-testable without a player window.
#[derive(Debug, Default)]
pub struct SaveDebouncer {
    dirty: bool,
    deadline: Option<Instant>,
}

impl SaveDebouncer {
    /// Record a change and push the write deadline out by `delay`. A burst of
    /// changes therefore produces one write after the last change.
    pub fn mark(&mut self, now: Instant, delay: Duration) {
        self.dirty = true;
        self.deadline = Some(now + delay);
    }

    /// True exactly once per marked change once the deadline has passed.
    pub fn due(&mut self, now: Instant) -> bool {
        if !self.dirty {
            return false;
        }
        if self.deadline.map(|deadline| now >= deadline).unwrap_or(true) {
            self.dirty = false;
            self.deadline = None;
            true
        } else {
            false
        }
    }

    /// Force the next [`Self::due`] to report true (used on shutdown).
    #[allow(dead_code)]
    pub fn flush_now(&mut self) {
        if self.dirty {
            self.deadline = Some(Instant::now());
        }
    }

    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_path(name: &str) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("o11a1-player-state-{}-{name}-{id}", std::process::id()))
            .join(FILE_NAME)
    }

    #[test]
    fn defaults_are_playable_values() {
        let state = PlayerState::default();
        assert_eq!(state.volume, 80);
        assert_eq!(state.speed, 1.0);
        assert_eq!(state.brightness, 1.0);
        assert_eq!(state.aspect, 0);
        assert!(state.danmaku_visible);
        assert!(!state.muted);
        assert!(!state.audio_only);
        assert!(!state.topmost);
        assert_eq!(state.quality_qn, None);
    }

    #[test]
    fn save_and_load_round_trip_every_field() {
        let path = temp_path("roundtrip");
        let state = PlayerState {
            volume: 37,
            muted: true,
            speed: 1.25,
            aspect: 2,
            brightness: 0.6,
            audio_only: true,
            topmost: true,
            danmaku_visible: false,
            quality_qn: Some(64),
            quality_codecid: Some(12),
            audio_index: Some(2),
        };
        state.save_to(&path).expect("save");
        let loaded = PlayerState::load_from(&path);
        assert_eq!(loaded, state);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = temp_path("missing");
        assert_eq!(PlayerState::load_from(&path), PlayerState::default());
    }

    #[test]
    fn corrupt_file_yields_defaults_instead_of_panicking() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(PlayerState::load_from(&path), PlayerState::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_fields_and_missing_fields_use_defaults() {
        let path = temp_path("partial");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"volume":55,"speed":9.0,"brightness":-4.0,"aspect":99,"quality_codecid":7,"future_field":true}"#,
        )
        .unwrap();
        let loaded = PlayerState::load_from(&path);
        assert_eq!(loaded.volume, 55);
        // Out-of-range values are normalised rather than trusted.
        assert_eq!(loaded.speed, 2.0);
        assert_eq!(loaded.brightness, 0.4);
        assert_eq!(loaded.aspect, 0);
        // A codec without a qn cannot be applied on open.
        assert_eq!(loaded.quality_qn, None);
        assert_eq!(loaded.quality_codecid, Some(7));
        assert_eq!(loaded.danmaku_visible, true);
        assert_eq!(loaded.volume, 55);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_numbers_are_normalised() {
        let mut state = PlayerState {
            speed: f32::NAN,
            brightness: f32::INFINITY,
            aspect: 7,
            volume: 250,
            quality_qn: Some(-1),
            quality_codecid: Some(42),
            ..Default::default()
        };
        state.normalise();
        assert_eq!(state.speed, 1.0);
        assert_eq!(state.brightness, 1.0);
        assert_eq!(state.aspect, 0);
        assert_eq!(state.volume, 100);
        assert_eq!(state.quality_qn, None);
        assert_eq!(state.quality_codecid, None);
    }

    #[test]
    fn state_path_is_under_the_app_root() {
        let path = state_path();
        assert_eq!(path.file_name().unwrap(), FILE_NAME);
        assert_eq!(path.parent().unwrap(), o11a1::paths::root());
    }

    #[test]
    fn debouncer_coalesces_a_burst_and_fires_once() {
        let start = Instant::now();
        let mut debouncer = SaveDebouncer::default();
        debouncer.mark(start, SAVE_DEBOUNCE);
        debouncer.mark(start + Duration::from_millis(100), SAVE_DEBOUNCE);
        assert!(!debouncer.due(start + Duration::from_millis(500)));
        assert!(debouncer.due(start + Duration::from_millis(600)));
        assert!(!debouncer.due(start + Duration::from_millis(600)));
        assert!(!debouncer.is_dirty());
    }

    #[test]
    fn debouncer_flush_is_immediate_and_idempotent() {
        let start = Instant::now();
        let mut debouncer = SaveDebouncer::default();
        debouncer.mark(start, Duration::from_secs(10));
        debouncer.flush_now();
        assert!(debouncer.due(Instant::now()));
        debouncer.flush_now();
        assert!(!debouncer.due(Instant::now()));
    }

    #[test]
    fn debouncer_ignores_dues_before_any_change() {
        let mut debouncer = SaveDebouncer::default();
        assert!(!debouncer.due(Instant::now()));
        assert!(!debouncer.is_dirty());
    }
}
