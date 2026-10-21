//! Non-secret UI preferences persisted in `settings.json` beside `token.json`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/// One queued video in the app-wide 播放列表 (see `PlaybackQueue` in `main.rs`).
///
/// Deliberately flat and fully `serde`-defaulted: a hand-edited or older
/// `settings.json` may hold any subset of these fields, and every one of them
/// must survive a round trip without failing the whole file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    #[serde(default)]
    pub bvid: String,
    #[serde(default)]
    pub cid: u64,
    #[serde(default)]
    pub aid: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub duration: String,
    /// Cover URL, exactly as the feed/card handed it out (empty = no cover).
    #[serde(default)]
    pub cover: String,
}

/// Danmaku, playback and window-appearance preferences.
///
/// `danmaku_font`: 0 = 小, 1 = 标准, 2 = 大. `danmaku_speed`: scroll-speed
/// percent (50..=200). `danmaku_area`: the top percentage of the video that
/// danmaku may occupy (25 / 50 / 75 / 100). `player_speed`: playback speed times
/// 1000 (1000 = 1.0x). `glass_transparency` / `glass_blur`: the main window's
/// frosted-glass sliders, percent (0..=100). `page_size`: cards per page in the
/// no-scroll lists, one of [`PAGE_SIZES`]. `close_action`: what ✕ / Alt+F4 /
/// the taskbar's 关闭 does, one of [`CLOSE_ACTIONS`]. `feed_prefetch_batches`:
/// how many recommendation batches the home feed keeps preloaded
/// (`0..=FEED_PREFETCH_MAX`). `cdn_select` / `prefetch_proxy`: player-side
/// opt-outs that are persisted here and read by a later pass. `seek_points`:
/// how many evenly spaced positions of the stream the prefetch proxy warms in
/// the background, so dragging the seek bar near one of them resumes from
/// memory (`4..=SEEK_POINTS_MAX`; see [`normalise_seek_points`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_opacity")]
    pub danmaku_opacity: u32,
    #[serde(default = "default_font")]
    pub danmaku_font: u32,
    #[serde(default = "default_danmaku_speed")]
    pub danmaku_speed: u32,
    #[serde(default = "default_danmaku_area")]
    pub danmaku_area: u32,
    #[serde(default)]
    pub block_scroll: bool,
    #[serde(default)]
    pub block_top: bool,
    #[serde(default)]
    pub block_bottom: bool,
    #[serde(default)]
    pub blocked_senders: Vec<String>,
    /// Danmaku whose text contains any of these (case-insensitive substring) is
    /// not rendered. Edited from the 设置 panel; applied in the player filter.
    #[serde(default)]
    pub blocked_keywords: Vec<String>,
    /// Danmaku whose text matches any of these regex patterns is not rendered.
    /// Invalid patterns are ignored at compile time.
    #[serde(default)]
    pub blocked_regex: Vec<String>,
    /// Starting playback quality (`qn`). Defaults to 1080P; the track selector
    /// then takes the lowest-bitrate codec available at that quality.
    #[serde(default = "default_player_quality")]
    pub player_quality: i64,
    #[serde(default = "default_player_speed")]
    pub player_speed: u32,
    #[serde(default = "default_hwaccel")]
    pub hwaccel: bool,
    /// Preferred video codec when a quality offers several: 0 = 自动,
    /// 7 = AVC, 12 = HEVC, 13 = AV1.
    #[serde(default)]
    pub codec_pref: u32,
    #[serde(default)]
    pub ai_on: bool,
    #[serde(default)]
    pub ai_level: u32,
    /// UI theme: 1 = 浅色, 2 = 深色 ([`crate::theme::THEME_LIGHT`] /
    /// [`crate::theme::THEME_DARK`]). There is no stored 跟随系统 state: a legacy
    /// `0` (or any corrupt value) is resolved once against the Windows setting
    /// at load and stored as the concrete mode, so the app never reports a third
    /// state. See [`normalise_theme`].
    #[serde(default)]
    pub theme: u32,
    /// Launch the app on Windows sign-in (HKCU Run key).
    #[serde(default)]
    pub autostart: bool,
    /// Player shortcut remap: one candidate index per action (see
    /// [`default_player_keys`]). Always normalised to [`PLAYER_KEY_ACTIONS`].
    #[serde(default)]
    pub player_keys: Vec<u32>,
    /// Main-window frosted glass: how see-through the window tint is, in
    /// percent (0 = matte, 100 = maximally see-through).
    #[serde(default = "default_glass_transparency")]
    pub glass_transparency: f32,
    /// Main-window frosted glass: Gaussian blur strength in percent (0 = no
    /// blur layer at all, 100 = strongest; mapped to `glass::SIGMA_MAX`).
    #[serde(default = "default_glass_blur")]
    pub glass_blur: f32,
    /// Cards per page in the no-scroll lists (首页推荐 / 搜索 / 历史 / ...):
    /// the 设置 menu offers [`PAGE_SIZES`]; Rust drives the UI's
    /// `list_page_size` property and the paging helpers from this value.
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    /// 关闭主窗口（✕ / Alt+F4 / 任务栏关闭）后的行为：0 = 最小化到任务栏,
    /// 1 = 隐藏到系统托盘, 2 = 直接退出程序.
    #[serde(default = "default_close_action")]
    pub close_action: u32,
    /// 播放器窗口上次的位置与大小，逻辑像素 `[x, y, w, h]`，描述的是**基础**
    /// 窗口——即没有任何外扩面板打开时的窗口（`x`/`y` 是画面区域的左上角，
    /// 与 `apply_panel_geometry` 的 `home` 同一含义）。打开播放器时先与某块
    /// 屏幕的工作区求交：仍落在某块屏幕上的位置才会被恢复，否则回落到系统
    /// 默认摆放（`None`）。面板打开/关闭以及拖动、缩放结束后写回。
    #[serde(default)]
    pub player_layout: Option<[i32; 4]>,
    /// 后台预加载的首页推荐批次数量（`0..=FEED_PREFETCH_MAX`）。刷新时若环
    /// 中有成品批次就直接上屏，不再走网络；0 关闭预加载。见
    /// [`normalise_feed_prefetch_batches`]。
    #[serde(default = "default_feed_prefetch_batches")]
    pub feed_prefetch_batches: u32,
    /// 是否启用播放器的 CDN 择优层。本次只持久化并暴露给后续读取，暂不接线。
    #[serde(default = "default_cdn_select")]
    pub cdn_select: bool,
    /// 是否启用本机回环的预取代理。本次只持久化并暴露给后续读取，暂不接线。
    #[serde(default = "default_prefetch_proxy")]
    pub prefetch_proxy: bool,
    /// 预取代理后台预热的跳转位置数量：把整段媒体按 `i / seek_points`
    /// 均分，每个位置缓存一小块（默认 4 MiB），拖到这些位置附近时直接从
    /// 内存出画，不必等上游。范围 `4..=SEEK_POINTS_MAX`，见
    /// [`normalise_seek_points`]。**0 不是合法值**：最少 4 个点，想更省流量
    /// 就取 4，不能关到 0。
    #[serde(default = "default_seek_points")]
    pub seek_points: u32,
    /// 播放列表：整个应用共用的一个队列（见 `main.rs` 的 `PlaybackQueue`）。
    /// 队首最早，队尾最新；重启后恢复但不自动播放。加载时用
    /// [`normalise_playlist_state`] 去空、去重（同一 bvid 只留第一次出现）、
    /// 截到 [`PLAYLIST_MAX`] 条（丢掉最旧的）。
    #[serde(default)]
    pub playlist: Vec<QueueEntry>,
    /// 播放列表里当前播放项的下标；`None` = 队列里没有正在播放的条目。
    /// 与 [`normalise_playlist_state`] 一起校正，永远落在 `playlist` 范围内。
    #[serde(default)]
    pub playlist_current: Option<usize>,
    /// 循环模式：0 = 手动（播完即停）, 1 = 自动（顺序播下一条）,
    /// 2 = 随机（播完随机跳一条未播过的）。见 [`normalise_play_mode`]。
    #[serde(default = "default_play_mode")]
    pub play_mode: u32,
}

/// Number of remappable player actions.
pub const PLAYER_KEY_ACTIONS: usize = 11;
/// Size of the candidate key pool offered in the 设置 dropdown.
pub const PLAYER_KEY_CANDIDATES: usize = 15;
/// Cards-per-page steps offered by the 设置 menu and accepted by
/// [`normalise_page_size`].
pub const PAGE_SIZES: [i64; 4] = [12, 24, 36, 48];
/// 关闭主窗口后的行为（见 [`Settings::close_action`]）：0 = 最小化到任务栏,
/// 1 = 隐藏到系统托盘, 2 = 直接退出程序.
pub const CLOSE_ACTIONS: [u32; 3] = [0, 1, 2];
/// Upper bound for [`Settings::feed_prefetch_batches`].
pub const FEED_PREFETCH_MAX: u32 = 4;
/// Smallest accepted count of warmed seek positions. **0 is not allowed**:
/// disabling point pre-warming is not offered, because 4 points already cost
/// only `4 × chunk` and are what makes seeking feel instant.
pub const SEEK_POINTS_MIN: u32 = 4;
/// Largest accepted count of warmed seek positions. The point cache is capped
/// at 80 MiB, so 20 points × 4 MiB still fits ([`crate::prefetch`] enforces it).
pub const SEEK_POINTS_MAX: u32 = 20;
/// 播放列表最多保留多少条；超出时丢掉最旧的（见
/// [`normalise_playlist_state`] 和 `PlaybackQueue::push`）。
pub const PLAYLIST_MAX: usize = 200;
/// 循环模式：0 = 手动, 1 = 自动, 2 = 随机.
pub const PLAY_MODES: [u32; 3] = [0, 1, 2];

/// Default mapping, one candidate index per action:
/// 播放/暂停, 快退, 快进, 音量+, 音量-, 静音, 全屏, 退出全屏, 弹幕, 点赞, 截图.
pub fn default_player_keys() -> Vec<u32> {
    vec![3, 4, 5, 6, 7, 0, 8, 12, 9, 10, 11]
}

fn normalize_player_keys(mut keys: Vec<u32>) -> Vec<u32> {
    let defaults = default_player_keys();
    keys.truncate(PLAYER_KEY_ACTIONS);
    while keys.len() < PLAYER_KEY_ACTIONS {
        keys.push(defaults[keys.len()]);
    }
    for key in keys.iter_mut() {
        *key = (*key).min((PLAYER_KEY_CANDIDATES - 1) as u32);
    }
    keys
}


fn default_opacity() -> u32 {
    100
}

fn default_font() -> u32 {
    1
}

fn default_danmaku_speed() -> u32 {
    100
}

fn default_danmaku_area() -> u32 {
    100
}

fn default_player_speed() -> u32 {
    1000
}

fn default_hwaccel() -> bool {
    true
}

/// Default starting quality: 1080P (`qn` 80).
fn default_player_quality() -> i64 {
    80
}

/// 20% see-through: a light tint that leaves the frosted backdrop clearly
/// visible (the blur below does the work).
pub fn default_glass_transparency() -> f32 {
    20.0
}

/// 100% blur: the strongest the slider offers, mapped to the full
/// `glass::SIGMA_MAX` at the current DPI scale.
pub fn default_glass_blur() -> f32 {
    100.0
}

pub fn default_page_size() -> i64 {
    24
}

/// 默认关闭行为：最小化到任务栏（保留任务栏按钮，永远不会让窗口无法找回）。
pub fn default_close_action() -> u32 {
    0
}

/// 默认预加载两个推荐批次：刷新时总能立刻拿到一个，同时再补一个。
pub fn default_feed_prefetch_batches() -> u32 {
    2
}

/// CDN 择优默认开启（择优层本身早已存在，这里只留开关）。
pub fn default_cdn_select() -> bool {
    true
}

/// 预取代理默认开启（见 `prefetch`，后续再接线到这个开关）。
pub fn default_prefetch_proxy() -> bool {
    true
}

/// 默认预热 10 个跳转位置（10 × 4 MiB ≈ 40 MiB）。
pub fn default_seek_points() -> u32 {
    10
}

/// 默认循环模式：自动（播完顺序播下一条）。
pub fn default_play_mode() -> u32 {
    1
}

/// Normalise the 循环模式.
///
/// Accepts exactly the three offered modes ([`PLAY_MODES`]); anything else
/// (a hand-edited or legacy value) falls back to [`default_play_mode`].
pub fn normalise_play_mode(value: u32) -> u32 {
    if PLAY_MODES.contains(&value) {
        value
    } else {
        default_play_mode()
    }
}

/// Normalise a restored 播放列表 together with its current index.
///
/// The queue arrives from JSON and may be arbitrarily corrupt:
///
/// * an entry without a `bvid` is dropped (it could never be played);
/// * a repeated `bvid` is dropped after its first occurrence, so the panel can
///   never show the same video twice;
/// * more than [`PLAYLIST_MAX`] entries keep the NEWEST ones, because the live
///   queue drops its oldest item when it is full;
/// * `current` is remapped through those edits and becomes `None` when the item
///   it pointed at is gone, so it always indexes the returned list.
pub fn normalise_playlist_state(
    items: Vec<QueueEntry>,
    current: Option<usize>,
) -> (Vec<QueueEntry>, Option<usize>) {
    let mut kept: Vec<(usize, QueueEntry)> = Vec::with_capacity(items.len());
    let mut seen: HashSet<String> = HashSet::new();
    for (index, entry) in items.into_iter().enumerate() {
        if entry.bvid.trim().is_empty() {
            continue;
        }
        if !seen.insert(entry.bvid.clone()) {
            continue;
        }
        kept.push((index, entry));
    }
    let mut current = current.and_then(|wanted| {
        kept.iter()
            .position(|(index, _)| *index == wanted)
    });
    if kept.len() > PLAYLIST_MAX {
        let dropped = kept.len() - PLAYLIST_MAX;
        kept.drain(0..dropped);
        current = current.and_then(|index| index.checked_sub(dropped));
    }
    (kept.into_iter().map(|(_, entry)| entry).collect(), current)
}

/// Normalise the count of warmed seek positions.
///
/// The value arrives both from JSON (a hand-edited or older file can hold
/// anything) and from the 设置 menu. Every value in
/// `SEEK_POINTS_MIN..=SEEK_POINTS_MAX` is kept; anything else — including the
/// tempting `0` — is corrupt for this field and falls back to
/// [`default_seek_points`]. Use [`SEEK_POINTS_MIN`] for "as little traffic as
/// possible", not 0.
pub fn normalise_seek_points(value: u32) -> u32 {
    if (SEEK_POINTS_MIN..=SEEK_POINTS_MAX).contains(&value) {
        value
    } else {
        default_seek_points()
    }
}

/// Normalise the preloaded-batch count.
///
/// The value arrives both from JSON (a hand-edited or older file can hold
/// anything) and from the 设置 menu: every value in `0..=FEED_PREFETCH_MAX` is
/// kept (0 = preloading off), anything above the cap is corrupt and falls back
/// to [`default_feed_prefetch_batches`].
pub fn normalise_feed_prefetch_batches(value: u32) -> u32 {
    if value <= FEED_PREFETCH_MAX {
        value
    } else {
        default_feed_prefetch_batches()
    }
}

/// Normalise the 关闭主窗口 behaviour.
///
/// The value arrives both from JSON (a corrupt or hand-edited file can hold
/// anything) and from the 设置 menu: only the offered actions ([`CLOSE_ACTIONS`])
/// are kept, anything else falls back to [`default_close_action`].
pub fn normalise_close_action(value: u32) -> u32 {
    if CLOSE_ACTIONS.contains(&value) {
        value
    } else {
        default_close_action()
    }
}

/// Normalise the cards-per-page setting.
///
/// The value arrives both from JSON (a hand-edited or older file can hold
/// anything) and from the 设置 menu: only the offered steps ([`PAGE_SIZES`])
/// are kept, anything else falls back to [`default_page_size`].
pub fn normalise_page_size(value: i64) -> i64 {
    if PAGE_SIZES.contains(&value) {
        value
    } else {
        default_page_size()
    }
}

/// Normalise a frosted-glass slider value.
///
/// The values are persisted as floats and arrive both from JSON (where a
/// hand-edited or older file can hold anything) and from the UI sliders:
/// finite values in `0..=100` are kept, anything else — out of range, JSON
/// `null`, `NaN` — falls back to `default`. The theme global must never hold a
/// value outside the slider's range.
pub fn normalise_glass(value: f32, default: f32) -> f32 {
    if (0.0..=100.0).contains(&value) {
        value
    } else {
        default
    }
}

/// Validate a stored player layout.
///
/// A hand-edited or older file can hold anything: `[x, y, w, h]` in logical px
/// is kept only when it is a usable rect (positive size, no absurd magnitude).
/// Anything else becomes `None`, which means "use the default placement".
pub fn normalise_player_layout(layout: Option<[i32; 4]>) -> Option<[i32; 4]> {
    let [x, y, w, h] = layout?;
    const LIMIT: i32 = 100_000;
    let sane = w > 0 && h > 0 && x.abs() < LIMIT && y.abs() < LIMIT && w <= LIMIT && h <= LIMIT;
    sane.then_some([x, y, w, h])
}

/// Snap a stored theme mode to one of the two explicit modes.
///
/// The only accepted values are [`crate::theme::THEME_LIGHT`] (1) and
/// [`crate::theme::THEME_DARK`] (2). Anything else — the legacy 跟随系统 `0`, or
/// a hand-edited corrupt value — is resolved ONCE against the Windows light/dark
/// setting ([`crate::theme::system_mode`]) and kept as that concrete mode, so a
/// follow-system state is never stored or reported again.
pub fn normalise_theme(value: u32) -> u32 {
    match value {
        crate::theme::THEME_LIGHT => crate::theme::THEME_LIGHT,
        crate::theme::THEME_DARK => crate::theme::THEME_DARK,
        _ => crate::theme::system_mode(),
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            danmaku_opacity: default_opacity(),
            danmaku_font: default_font(),
            danmaku_speed: default_danmaku_speed(),
            danmaku_area: default_danmaku_area(),
            block_scroll: false,
            block_top: false,
            block_bottom: false,
            blocked_senders: Vec::new(),
            blocked_keywords: Vec::new(),
            blocked_regex: Vec::new(),
            player_quality: default_player_quality(),
            player_speed: default_player_speed(),
            hwaccel: default_hwaccel(),
            codec_pref: 0,
            ai_on: false,
            ai_level: 0,
            theme: normalise_theme(0),
            autostart: false,
            player_keys: default_player_keys(),
            glass_transparency: default_glass_transparency(),
            glass_blur: default_glass_blur(),
            page_size: default_page_size(),
            close_action: default_close_action(),
            player_layout: None,
            feed_prefetch_batches: default_feed_prefetch_batches(),
            cdn_select: default_cdn_select(),
            prefetch_proxy: default_prefetch_proxy(),
            seek_points: default_seek_points(),
            playlist: Vec::new(),
            playlist_current: None,
            play_mode: default_play_mode(),
        }
    }
}

pub fn settings_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("settings.json")
}

/// Load preferences, clamping corrupted values and falling back to defaults.
///
/// The theme is the one value that MIGRATES rather than merely clamping: a
/// legacy 跟随系统 `0` (or a corrupt value) is resolved once against the Windows
/// light/dark setting and written straight back, so a stored file never keeps a
/// third state. The write is best-effort — a read-only file still yields the
/// migrated in-memory value.
pub fn load() -> Settings {
    let Some(settings) = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|data| serde_json::from_str::<Settings>(&data).ok())
    else {
        return Settings::default();
    };
    let stored_theme = settings.theme;
    let (playlist, playlist_current) =
        normalise_playlist_state(settings.playlist, settings.playlist_current);
    let loaded = Settings {
        danmaku_opacity: settings.danmaku_opacity.min(100),
        danmaku_font: settings.danmaku_font.min(2),
        danmaku_speed: settings.danmaku_speed.clamp(50, 200),
        danmaku_area: clamp_area(settings.danmaku_area),
        block_scroll: settings.block_scroll,
        block_top: settings.block_top,
        block_bottom: settings.block_bottom,
        blocked_senders: settings.blocked_senders,
        blocked_keywords: settings.blocked_keywords,
        blocked_regex: settings.blocked_regex,
        player_quality: settings.player_quality,
        player_speed: settings.player_speed.clamp(500, 2000),
        hwaccel: settings.hwaccel,
        codec_pref: match settings.codec_pref {
            7 => 7,
            12 => 12,
            13 => 13,
            _ => 0,
        },
        ai_on: settings.ai_on,
        ai_level: settings.ai_level.min(10),
        theme: normalise_theme(settings.theme),
        autostart: settings.autostart,
        player_keys: normalize_player_keys(settings.player_keys),
        glass_transparency: normalise_glass(
            settings.glass_transparency,
            default_glass_transparency(),
        ),
        glass_blur: normalise_glass(settings.glass_blur, default_glass_blur()),
        page_size: normalise_page_size(settings.page_size),
        close_action: normalise_close_action(settings.close_action),
        player_layout: normalise_player_layout(settings.player_layout),
        feed_prefetch_batches: normalise_feed_prefetch_batches(settings.feed_prefetch_batches),
        cdn_select: settings.cdn_select,
        prefetch_proxy: settings.prefetch_proxy,
        seek_points: normalise_seek_points(settings.seek_points),
        playlist,
        playlist_current,
        play_mode: normalise_play_mode(settings.play_mode),
    };
    if loaded.theme != stored_theme {
        let _ = save(&loaded);
    }
    loaded
}

/// Snap a display-area percent to the nearest supported step (25/50/75/100).
pub fn clamp_area(value: u32) -> u32 {
    match value {
        0..=37 => 25,
        38..=62 => 50,
        63..=87 => 75,
        _ => 100,
    }
}

pub fn save(settings: &Settings) -> Result<()> {
    // Several writers exist (the settings panel, the player layout and the
    // playback queue, each with its own snapshot). Serialising the write keeps
    // the file from being interleaved by two of them; the last complete
    // snapshot still wins, exactly as before.
    static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = match SAVE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(settings_path(), data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_action_accepts_every_offered_value() {
        for value in CLOSE_ACTIONS {
            assert_eq!(normalise_close_action(value), value);
        }
    }

    #[test]
    fn close_action_rejects_corrupt_values() {
        for value in [3u32, 4, 99, u32::MAX] {
            assert_eq!(normalise_close_action(value), default_close_action());
        }
    }

    #[test]
    fn default_settings_use_the_default_close_action() {
        assert_eq!(Settings::default().close_action, default_close_action());
    }

    #[test]
    fn corrupt_json_falls_back_to_the_default_close_action() {
        let value = match serde_json::from_str::<Settings>(r#"{ "close_action": 99 }"#) {
            Ok(settings) => settings.close_action,
            Err(_) => default_close_action(),
        };
        assert_eq!(normalise_close_action(value), default_close_action());
    }

    #[test]
    fn missing_close_action_uses_the_serde_default() {
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(settings.close_action, default_close_action());
    }

    #[test]
    fn feed_prefetch_batches_keeps_every_offered_value() {
        for value in 0..=FEED_PREFETCH_MAX {
            assert_eq!(normalise_feed_prefetch_batches(value), value);
        }
        assert_eq!(
            Settings::default().feed_prefetch_batches,
            default_feed_prefetch_batches()
        );
    }

    #[test]
    fn feed_prefetch_batches_corrupt_value_falls_back_to_default() {
        for value in [FEED_PREFETCH_MAX + 1, 5, 99, u32::MAX] {
            assert_eq!(
                normalise_feed_prefetch_batches(value),
                default_feed_prefetch_batches()
            );
        }
        let settings = match serde_json::from_str::<Settings>(r#"{ "feed_prefetch_batches": 99 }"#)
        {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(
            normalise_feed_prefetch_batches(settings.feed_prefetch_batches),
            default_feed_prefetch_batches()
        );
    }

    #[test]
    fn feed_prefetch_batches_missing_field_uses_the_serde_default() {
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(
            settings.feed_prefetch_batches,
            default_feed_prefetch_batches()
        );
    }

    #[test]
    fn cdn_select_and_prefetch_proxy_default_on() {
        let defaults = Settings::default();
        assert!(defaults.cdn_select);
        assert!(defaults.prefetch_proxy);
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert!(settings.cdn_select);
        assert!(settings.prefetch_proxy);
        let off = match serde_json::from_str::<Settings>(
            r#"{ "cdn_select": false, "prefetch_proxy": false }"#,
        ) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert!(!off.cdn_select);
        assert!(!off.prefetch_proxy);
    }

    #[test]
    fn seek_points_keeps_every_offered_value() {
        for value in SEEK_POINTS_MIN..=SEEK_POINTS_MAX {
            assert_eq!(normalise_seek_points(value), value);
        }
        assert_eq!(Settings::default().seek_points, default_seek_points());
        assert_eq!(default_seek_points(), 10);
    }

    #[test]
    fn seek_points_zero_and_corrupt_values_fall_back() {
        // 0 is not a valid count (use SEEK_POINTS_MIN for the cheapest setting).
        for value in [0u32, 1, 2, 3] {
            assert_eq!(normalise_seek_points(value), default_seek_points());
            assert!(value < SEEK_POINTS_MIN);
        }
        for value in [SEEK_POINTS_MAX + 1, 21, 99, u32::MAX] {
            assert_eq!(normalise_seek_points(value), default_seek_points());
        }
        let settings =
            match serde_json::from_str::<Settings>(r#"{ "seek_points": 99 }"#) {
                Ok(settings) => settings,
                Err(_) => Settings::default(),
            };
        assert_eq!(
            normalise_seek_points(settings.seek_points),
            default_seek_points()
        );
    }

    #[test]
    fn seek_points_missing_field_uses_the_serde_default() {
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(settings.seek_points, default_seek_points());
    }

    #[test]
    fn seek_points_survive_a_json_round_trip() -> Result<()> {
        let settings = Settings {
            seek_points: SEEK_POINTS_MAX,
            ..Settings::default()
        };
        let encoded = serde_json::to_string(&settings)?;
        let decoded = serde_json::from_str::<Settings>(&encoded)?;
        assert_eq!(
            normalise_seek_points(decoded.seek_points),
            SEEK_POINTS_MAX
        );
        Ok(())
    }

    fn queued(bvid: &str) -> QueueEntry {
        QueueEntry {
            bvid: bvid.to_string(),
            cid: 1,
            aid: 1,
            title: bvid.to_string(),
            owner: "UP".to_string(),
            duration: "01:00".to_string(),
            cover: String::new(),
        }
    }

    #[test]
    fn playlist_defaults_to_empty_manual_queue() {
        let defaults = Settings::default();
        assert!(defaults.playlist.is_empty());
        assert_eq!(defaults.playlist_current, None);
        assert_eq!(defaults.play_mode, default_play_mode());
    }

    #[test]
    fn play_mode_keeps_the_three_offered_modes() {
        for mode in PLAY_MODES {
            assert_eq!(normalise_play_mode(mode), mode);
        }
        for mode in [3u32, 4, 99, u32::MAX] {
            assert_eq!(normalise_play_mode(mode), default_play_mode());
        }
    }

    #[test]
    fn playlist_missing_fields_use_serde_defaults() {
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert!(settings.playlist.is_empty());
        assert_eq!(settings.playlist_current, None);
        assert_eq!(settings.play_mode, default_play_mode());
    }

    #[test]
    fn playlist_round_trips_through_json() -> Result<()> {
        let settings = Settings {
            playlist: vec![queued("BV1"), queued("BV2")],
            playlist_current: Some(1),
            play_mode: 2,
            ..Settings::default()
        };
        let encoded = serde_json::to_string(&settings)?;
        let decoded = serde_json::from_str::<Settings>(&encoded)?;
        let (items, current) =
            normalise_playlist_state(decoded.playlist, decoded.playlist_current);
        assert_eq!(items, vec![queued("BV1"), queued("BV2")]);
        assert_eq!(current, Some(1));
        assert_eq!(normalise_play_mode(decoded.play_mode), 2);
        Ok(())
    }

    #[test]
    fn playlist_drops_empty_and_duplicate_bvids() {
        let (items, current) = normalise_playlist_state(
            vec![
                queued("BV1"),
                queued(""),
                queued("BV1"),
                queued("BV2"),
            ],
            Some(3),
        );
        assert_eq!(
            items.iter().map(|entry| entry.bvid.as_str()).collect::<Vec<_>>(),
            vec!["BV1", "BV2"]
        );
        assert_eq!(current, Some(1), "BV2 moved from slot 3 to slot 1");
    }

    #[test]
    fn playlist_cap_keeps_the_newest_and_remaps_current() {
        let all: Vec<QueueEntry> = (0..PLAYLIST_MAX + 5)
            .map(|index| queued(&format!("BV{index}")))
            .collect();
        let (items, current) =
            normalise_playlist_state(all.clone(), Some(PLAYLIST_MAX + 2));
        let newest = format!("BV{}", PLAYLIST_MAX + 4);
        assert_eq!(items.len(), PLAYLIST_MAX);
        assert_eq!(items.first().map(|entry| entry.bvid.as_str()), Some("BV5"));
        assert_eq!(
            items.last().map(|entry| entry.bvid.as_str()),
            Some(newest.as_str())
        );
        assert_eq!(current, Some(PLAYLIST_MAX - 3));
        let (_, dropped_current) = normalise_playlist_state(all, Some(0));
        assert_eq!(dropped_current, None);
    }

    #[test]
    fn playlist_current_out_of_range_becomes_none() {
        let (items, current) =
            normalise_playlist_state(vec![queued("BV1")], Some(4));
        assert_eq!(items.len(), 1);
        assert_eq!(current, None);
        let (_, current) = normalise_playlist_state(vec![queued("BV1")], Some(0));
        assert_eq!(current, Some(0));
        let (_, none) = normalise_playlist_state(vec![queued("BV1")], None);
        assert_eq!(none, None);
    }

    #[test]
    fn playlist_corrupt_json_falls_back_to_the_defaults() {
        let (items, current) =
            normalise_playlist_state(vec![queued("BV1"), queued("BV1")], Some(0));
        assert_eq!(items.len(), 1);
        assert_eq!(current, Some(0));
        let settings = match serde_json::from_str::<Settings>(
            r#"{ "playlist": [{ "title": "no bvid" }], "playlist_current": 7, "play_mode": 99 }"#,
        ) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        let (items, current) =
            normalise_playlist_state(settings.playlist, settings.playlist_current);
        assert!(items.is_empty());
        assert_eq!(current, None);
        assert_eq!(normalise_play_mode(settings.play_mode), default_play_mode());
    }
}
