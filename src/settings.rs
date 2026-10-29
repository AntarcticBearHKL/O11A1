//! Non-secret UI preferences persisted in `settings.json` beside `token.json`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

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
/// 1000 (1000 = 1.0x). `glass_transparency`: the main window's frosted-glass
/// tint, percent (0 = fully opaque tint, 100 = no tint; default 45).
/// `backdrop_blur`: the wallpaper's diffusion radius in image pixels
/// (`0..=160`, default 60; see `backdrop::set_blur`). `glass_blur` is the
/// retired capture-engine slot, kept only so old files still round-trip.
/// `page_size`: cards per page in the
/// no-scroll lists, one of [`PAGE_SIZES`]. `close_action`: what ✕ / Alt+F4 /
/// the taskbar's 关闭 does, one of [`CLOSE_ACTIONS`]. `feed_prefetch_batches`:
/// how many recommendation batches the home feed keeps preloaded
/// (`0..=FEED_PREFETCH_MAX`). `cdn_select` / `prefetch_proxy`: player-side
/// opt-outs that are persisted here and read by a later pass. `seek_points`:
/// how many evenly spaced positions of the stream the prefetch proxy warms in
/// the background, so dragging the seek bar near one of them resumes from
/// memory (`4..=SEEK_POINTS_MAX`; see [`normalise_seek_points`]).
/// `prefetch_connections`: how many parallel upstream fetchers the proxy opens
/// while the startup gate fills the cache (`4..=PREFETCH_CONNECTIONS_MAX`,
/// default 16; see [`normalise_prefetch_connections`]).
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
    /// Starting playback quality (`qn`), i.e. 设置 → 播放 → 默认画质: one of
    /// `0` (自动 = `min(highest playable, 1080P)`), 1080P, 720P or 480P. The
    /// track selector then takes the sharpest stream (highest bitrate, or the
    /// preferred codec) at that quality. A stored value outside that set is
    /// normalised to 自动, so a stale 4K pick can never be every video's default
    /// (4K/8K are only opened from the 画质 menu).
    #[serde(default = "default_player_quality")]
    pub player_quality: i64,
    /// Decode at the source's own resolution instead of the picture the window
    /// actually shows. `false` (default) asks the decoder for the window-sized
    /// picture — a 1080p source in a 1216×684 window decodes 1216×684, which is
    /// a third of the pipe bytes and the same picture on screen. `true` is the
    /// "原始分辨率" switch: the old behaviour, at the old cost.
    #[serde(default)]
    pub player_native_size: bool,
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
    /// blur layer at all, 100 = strongest; retired capture-engine slot).
    #[serde(default = "default_glass_blur")]
    pub glass_blur: f32,
    /// 背景模糊: the generated wallpaper's blur radius in **image pixels**
    /// (1024-space), `0..=160`, default 60. `0` leaves the coloured balls
    /// crisp, `60` melts them into a mesh gradient, `160` is a near-flat wash.
    /// Applied through `backdrop::set_blur` and persisted here.
    #[serde(default = "default_backdrop_blur")]
    pub backdrop_blur: u32,
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
    /// 预取代理的上游并发连接数（`PREFETCH_CONNECTIONS_MIN..=PREFETCH_CONNECTIONS_MAX`，
    /// 默认 16）。开始播放前由 `player.rs` 读入 `PrefetchConfig::connections`：
    /// 连接越多，开头几秒的缓存越快填满。`0` 表示"未设置"（旧文件或新建的
    /// 字段），回落到默认值；其余越界值夹到区间内。见
    /// [`normalise_prefetch_connections`]。
    #[serde(default = "default_prefetch_connections")]
    pub prefetch_connections: u32,
    /// 网络线程池的常驻线程数（`NET_THREADS_MIN..=NET_THREADS_MAX`，默认 12）。
    /// 整个进程只创建一次，见 `netpool::init`：所有一次性网络请求（封面、
    /// 头像、点赞/投币/收藏、评论分页……）都排进这个池；视频播放时预取代理
    /// 只是把它的连接槽位绑定到这些已有线程上，开视频不再创建线程。
    /// `BILI_NET_THREADS` 可在启动时覆盖本字段（用于 A/B）。
    /// `0` 表示"未设置"，回落到默认值；其余越界值夹到区间内。见
    /// [`normalise_net_threads`]。
    #[serde(default = "default_net_threads")]
    pub net_threads: u32,
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
    /// 帧插值模式：0 = 关闭, 1 = 帧混合（blend）, 2 = 块匹配运动（motion）。
    /// 未知值在加载时归一为 0。见 [`normalise_interp_mode`]。
    #[serde(default)]
    pub interp_mode: u8,
    /// DeepSeek API key（`sk-...`），养鱼引擎用它替用户看视频。空串 = 未配置，
    /// 此时引擎只报 `NeedsKey`、不发任何网络请求。界面上只显示
    /// [`mask_key`] 的脱敏结果。
    #[serde(default)]
    pub deepseek_key: String,
    /// 养鱼使用的 DeepSeek 模型（默认 `deepseek-flash`）。空串在加载时回落到
    /// [`default_fish_model`]。
    #[serde(default = "default_fish_model")]
    pub fish_model: String,
    /// 鱼主人的指令：拼在每条养鱼提示词的最后一段，告诉鲸鱼该怎么看视频。
    /// 空白值在加载时回落到 [`default_fish_instruction`]。
    #[serde(default = "default_fish_instruction")]
    pub fish_instruction: String,
    /// 每次出巡最多看多少条视频（`1..=FISH_BUDGET_MAX`，默认 40）。见
    /// [`normalise_fish_budget`]。
    #[serde(default = "default_fish_budget_videos")]
    pub fish_budget_videos: u32,
    /// 两条视频之间的休息毫秒数（`FISH_INTERVAL_MIN_MS..=FISH_INTERVAL_MAX_MS`，
    /// 默认 1500）。见 [`normalise_fish_interval`]。
    #[serde(default = "default_fish_interval_ms")]
    pub fish_interval_ms: u64,
    /// 养鱼开关；关闭时引擎不起线程。
    #[serde(default)]
    pub fish_enabled: bool,
    /// 自动喂食：每天到 [`fish_feed_time`] 自己开饭。仍然需要
    /// [`fish_enabled`]，关掉大肥鱼就什么都不会跑。
    #[serde(default)]
    pub fish_auto_feed: bool,
    /// 自动喂食的时间，`HH:MM`（本机时间，默认 `20:00`）。加载时由
    /// [`normalise_fish_time`] 归一，坏值回落到默认。
    #[serde(default = "default_fish_feed_time")]
    pub fish_feed_time: String,
    /// 喂食来源位掩码：`1` 首页 / `2` 关注 / `4` 收藏夹。加载时由
    /// [`normalise_fish_sources`] 去掉未知位，空掩码回落到首页。
    #[serde(default = "default_fish_sources")]
    pub fish_sources: u32,
    /// 播放器选用的音频音质档位（Bilibili 的 `audio` id）。`0` = 自动（视频
    /// 提供的最好一档）；其余见 [`AUDIO_QUALITIES`]。见
    /// [`normalise_audio_quality`]。
    #[serde(default)]
    pub audio_quality: u32,
    /// 防干扰：隐藏首页工具栏的「直播」入口。默认关闭 = 与改动前完全一致
    /// （推荐 / 收藏 / 分区 / 番剧 / 直播 / 养鱼 六个 pill）。打开后只渲染五个
    /// pill，左侧胶囊簇的宽度推导同步收窄；若当前正停在直播页（view_mode 23）
    /// 打开本开关，会退回推荐页——那是它唯一的入口。见
    /// [`normalise_hide_live_tab`]。
    #[serde(default)]
    pub hide_live_tab: bool,
    /// 防干扰：搜索框展开时不显示热搜列表。默认关闭 = 空输入显示热搜。
    /// 打开后空输入直接显示联想（没有联想就什么都不显示），不再出现一块空白
    /// 的热搜面板。见 [`normalise_hide_hot_search`]。
    #[serde(default)]
    pub hide_hot_search: bool,
    /// 看片力度：`0..=4` 是 T0–T4 预设，`5` = 自定义（下面六个开关为准）。
    /// 未知值在加载时回落到 T1。见 [`normalise_fish_tier`]。
    #[serde(default = "default_fish_tier")]
    pub fish_tier: u32,
    /// 自定义档位下的步骤开关。预设档位（`fish_tier < 5`）忽略这六个值，
    /// 由 `fish.rs` 按档位展开，避免手改文件后档位与开关互相矛盾。
    #[serde(default = "default_true")]
    pub fish_step_meta: bool,
    #[serde(default = "default_true")]
    pub fish_step_subtitle: bool,
    #[serde(default)]
    pub fish_step_outline: bool,
    #[serde(default = "default_true")]
    pub fish_step_comments: bool,
    #[serde(default)]
    pub fish_step_asr: bool,
    #[serde(default)]
    pub fish_step_frames: bool,
    /// ASR 总开关。关闭时 T3/T4 的 asr 步骤记 `skipped(ASR 未开启)`，不下载音频。
    #[serde(default)]
    pub fish_asr_enabled: bool,
    /// `auto` | `faster-whisper` | `whisper.cpp`。见 [`normalise_fish_asr_engine`]。
    #[serde(default = "default_fish_asr_engine")]
    pub fish_asr_engine: String,
    /// `gpu` | `cpu`。见 [`normalise_fish_asr_device`]。
    #[serde(default = "default_fish_asr_device")]
    pub fish_asr_device: String,
    /// `small` | `turbo` | `medium`。见 [`normalise_fish_asr_model`]。
    #[serde(default = "default_fish_asr_model")]
    pub fish_asr_model: String,
    /// whisper.cpp / faster-whisper 的线程数（`1..=FISH_ASR_THREADS_MAX`）。
    #[serde(default = "default_fish_asr_threads")]
    pub fish_asr_threads: u32,
    /// 模型目录；空串 = 项目根下的 `models/whisper`。
    #[serde(default)]
    pub fish_asr_model_dir: String,
    /// DeepSeek 视觉的图片精度：`low`（先缩到 512×512，省 token）或 `original`。
    #[serde(default = "default_fish_vision_detail")]
    pub fish_vision_detail: String,
    /// 番剧索引的排序，按 season_type 分别记住：`{"1": "follows", "4": "score"}`
    /// （键是 `pgc_season_type` 的十进制 season_type，值是 [`PGC_SORT_KEYS`] 之一）。
    /// 缺省 = 该分区沿用 B站 的「追番人数」排序（`order=3`），即改动前的行为。
    /// 见 [`normalise_pgc_sorts`]。
    #[serde(default)]
    pub pgc_sorts: BTreeMap<String, String>,
}

/// Number of remappable player actions.
pub const PLAYER_KEY_ACTIONS: usize = 11;
/// Size of the candidate key pool offered in the 设置 dropdown.
pub const PLAYER_KEY_CANDIDATES: usize = 15;
/// Cards-per-page steps offered by the 设置 menu and accepted by
/// [`normalise_page_size`].
pub const PAGE_SIZES: [i64; 4] = [12, 24, 36, 48];
/// 番剧索引的排序键，按这个顺序就是 UI 下拉里的顺序（见 [`Settings::pgc_sorts`]）。
///
/// 每一个键都对应 `pgc/season/index/result` 的一个 `order` 值，而且语义是对着
/// 真实响应逐条核对过的（首条 + 整页的 `order` 文案单调递减、翻页接续不断）：
///
/// | key       | order | 该页 `order` 文案      | 真实含义 |
/// |-----------|-------|------------------------|----------|
/// | `follows` | 3     | `553.2万追番`          | 追番人数 |
/// | `update`  | 0     | `8月14日更新`          | 最近更新 |
/// | `views`   | 2     | `1.6亿次播放`          | 播放量   |
/// | `score`   | 4     | `9.9分`                | 评分     |
///
/// `order=1`（响应里 `order` 为空，实际按弹幕数降序）和 `order=5`（`X月X日
/// 开播`，按开播时间）也验证过，但没有做进 UI：前者不是用户能理解的排序，后者
/// 的「敬请期待」行混在日期序列里，语义不够干净。
pub const PGC_SORT_KEYS: [&str; 4] = ["follows", "update", "views", "score"];
/// 一个 season_type 没存过排序时用的键（B站 的「追番人数」，`order=3`）。
pub const PGC_SORT_DEFAULT: &str = "follows";

/// Snap a stored 番剧排序 key to one of [`PGC_SORT_KEYS`], else empty.
///
/// The value arrives from JSON and may be hand-edited: an unknown string is
/// dropped by [`normalise_pgc_sorts`] instead of being kept and later mapped to
/// a guessed API `order`.
pub fn normalise_pgc_sort(value: &str) -> String {
    let value = value.trim();
    if PGC_SORT_KEYS.contains(&value) {
        value.to_string()
    } else {
        String::new()
    }
}

/// Clean a stored `season_type -> sort key` map.
///
/// Keys are the decimal season_type (B站's own numbers: 1 番剧, 4 国创, ...);
/// anything that is not a positive integer is dropped, and a value that is not
/// in [`PGC_SORT_KEYS`] is dropped too, so the page falls back to
/// [`PGC_SORT_DEFAULT`] for that tab rather than to a wrong sort.
pub fn normalise_pgc_sorts(stored: BTreeMap<String, String>) -> BTreeMap<String, String> {
    stored
        .into_iter()
        .filter_map(|(season_type, key)| {
            let season_type = season_type.trim().to_string();
            if season_type.parse::<i64>().map(|id| id > 0).unwrap_or(false) {
                let key = normalise_pgc_sort(&key);
                if !key.is_empty() {
                    return Some((season_type, key));
                }
            }
            None
        })
        .collect()
}
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
/// Smallest number of upstream connections the prefetch proxy may use.
pub const PREFETCH_CONNECTIONS_MIN: u32 = 1;
/// Largest number of upstream connections the prefetch proxy may use.
pub const PREFETCH_CONNECTIONS_MAX: u32 = 32;
/// Smallest number of threads the network pool may own.
pub const NET_THREADS_MIN: u32 = 2;
/// Largest number of threads the network pool may own (see
/// [`normalise_net_threads`]).
pub const NET_THREADS_MAX: u32 = 32;
/// Pool size when the field is absent or `0`: large enough to keep a dozen
/// covers and a UI request in flight, small enough to leave the cores to the
/// picture.
pub const NET_THREADS_DEFAULT: u32 = 12;
/// 播放列表最多保留多少条；超出时丢掉最旧的（见
/// [`normalise_playlist_state`] 和 `PlaybackQueue::push`）。
pub const PLAYLIST_MAX: usize = 200;
/// 循环模式：0 = 手动, 1 = 自动, 2 = 随机.
pub const PLAY_MODES: [u32; 3] = [0, 1, 2];
/// 帧插值模式：0 = 关闭, 1 = 帧混合（blend）, 2 = 块匹配（block-match motion）.
pub const INTERP_MODES: [u8; 4] = [0, 1, 2, 3];
/// 播放器可选的音频档位（`audio` id）：0 = 自动。
pub const AUDIO_QUALITIES: [u32; 6] = [0, 30216, 30232, 30280, 30250, 30251];
/// 每次养鱼出巡最多看多少条视频的上限。
pub const FISH_BUDGET_MAX: u32 = 500;
/// 养鱼两条视频之间的最小休息间隔（毫秒）。
pub const FISH_INTERVAL_MIN_MS: u64 = 800;
/// 养鱼两条视频之间的最大休息间隔（毫秒）。
pub const FISH_INTERVAL_MAX_MS: u64 = 10_000;
/// 喂食来源：首页推荐（`feed_store` 里已经有的那一页）。
pub const FISH_SOURCE_HOME: u32 = 1;
/// 喂食来源：关注动态。
pub const FISH_SOURCE_FOLLOW: u32 = 2;
/// 喂食来源：收藏夹。
pub const FISH_SOURCE_FAV: u32 = 4;
/// 已知喂食来源的全部位。
pub const FISH_SOURCES_ALL: u32 = FISH_SOURCE_HOME | FISH_SOURCE_FOLLOW | FISH_SOURCE_FAV;
/// 看片力度里的「自定义」档位值；`0..=4` 是 T0–T4。
pub const FISH_TIER_CUSTOM: u32 = 5;
/// ASR 引擎的可选值。
pub const FISH_ASR_ENGINES: [&str; 3] = ["auto", "faster-whisper", "whisper.cpp"];
/// ASR 设备。
pub const FISH_ASR_DEVICES: [&str; 2] = ["gpu", "cpu"];
/// ASR 模型档位。
pub const FISH_ASR_MODELS: [&str; 3] = ["small", "turbo", "medium"];
/// ASR 线程数上限（whisper.cpp 的 `-t`，faster-whisper 的 `cpu_threads`）。
pub const FISH_ASR_THREADS_MAX: u32 = 16;
/// DeepSeek 视觉的两种图片精度。
pub const FISH_VISION_DETAILS: [&str; 2] = ["low", "original"];

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

/// Default starting quality: 自动 (`0`) = the highest tier the source offers.
/// f176: the owner asked for "open on the best source", so 4K is no longer
/// something you have to pick by hand.
fn default_player_quality() -> i64 {
    0
}

pub fn normalise_player_quality(qn: i64) -> i64 {
    if qn < 0 {
        0
    } else {
        qn
    }
}

/// 背景透明度 default: 45 % see-through. The window tint keeps 55 % alpha
/// (0x8C over the wallpaper), which is the exact alpha this window shipped
/// with before the slider existed.
pub fn default_glass_transparency() -> f32 {
    45.0
}

/// 背景模糊 default: 60 image px. Soft enough to read as a mesh gradient,
/// sharp enough that the blobs still have a direction.
pub fn default_backdrop_blur() -> u32 {
    60
}

/// Normalise the stored wallpaper blur: clamp into the slider's range, so a
/// hand-edited `9999` can never ask `backdrop` for a radius outside `0..=160`.
pub fn normalise_backdrop_blur(value: u32) -> u32 {
    value.min(160)
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

/// 默认 16 路并发上游连接：足够在开头几秒把缓存推到 8 MiB 上限，
/// 又不至于把带宽切得过碎（见 `player.rs` 的启动门）。
pub fn default_prefetch_connections() -> u32 {
    16
}

/// Normalise the prefetch connection count.
///
/// The value arrives both from JSON (a hand-edited or older file can hold
/// anything) and from the future 设置 menu. `0` is the one special case: it
/// means "unset" and falls back to [`default_prefetch_connections`] (16).
/// Everything else is clamped into
/// `PREFETCH_CONNECTIONS_MIN..=PREFETCH_CONNECTIONS_MAX`, so a corrupt `99` can
/// never ask for more sockets than the UI offers.
pub fn normalise_prefetch_connections(value: u32) -> u32 {
    if value == 0 {
        return default_prefetch_connections();
    }
    value.clamp(PREFETCH_CONNECTIONS_MIN, PREFETCH_CONNECTIONS_MAX)
}

/// Default size of the process-wide network thread pool.
pub fn default_net_threads() -> u32 {
    NET_THREADS_DEFAULT
}

/// Normalise the network pool size.
///
/// `0` means "unset" and falls back to [`default_net_threads`] (12); anything
/// else is clamped into `NET_THREADS_MIN..=NET_THREADS_MAX`, so a hand-edited
/// `999` can never ask for hundreds of threads.
pub fn normalise_net_threads(value: u32) -> u32 {
    if value == 0 {
        return default_net_threads();
    }
    value.clamp(NET_THREADS_MIN, NET_THREADS_MAX)
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

/// Normalise the 帧插值 mode.
///
/// Only the modes this build implements ([`INTERP_MODES`]) are accepted; a
/// hand-edited or newer `settings.json` value falls back to off. Off is always
/// safe: it costs one atomic load per presented source frame and changes
/// nothing else.
pub fn normalise_interp_mode(value: u8) -> u8 {
    if INTERP_MODES.contains(&value) {
        value
    } else {
        0
    }
}

fn default_fish_model() -> String {
    // The model the API docs serve for `deepseek-flash` (text + vision). The
    // legacy `deepseek-chat` name is an alias for the same model and is folded
    // into this one so the field never has to be typed by hand.
    "deepseek-flash".to_string()
}

/// 默认的鱼主人指令：只说材料里有的内容。
pub fn default_fish_instruction() -> String {
    "看到什么就说什么；只说材料里有的内容。".to_string()
}

/// 自动喂食时间（`HH:MM`，本机时间）。
pub fn default_fish_feed_time() -> String {
    "20:00".to_string()
}

/// 喂食来源默认只有首页：关注 / 收藏夹还没有独立的入队路径。
pub fn default_fish_sources() -> u32 {
    FISH_SOURCE_HOME
}

fn default_fish_budget_videos() -> u32 {
    40
}

fn default_true() -> bool {
    true
}

/// 默认档位 T1：字幕 + 评论，等于改动前的能力。
pub fn default_fish_tier() -> u32 {
    1
}

pub fn default_fish_asr_engine() -> String {
    "auto".to_string()
}

pub fn default_fish_asr_device() -> String {
    "gpu".to_string()
}

pub fn default_fish_asr_model() -> String {
    "small".to_string()
}

pub fn default_fish_asr_threads() -> u32 {
    4
}

pub fn default_fish_vision_detail() -> String {
    "low".to_string()
}

/// Normalise the 看片力度 tier: `0..=4` presets and `5` custom; anything else
/// falls back to T1 (the shipped default, never "no steps at all").
pub fn normalise_fish_tier(value: u32) -> u32 {
    if value <= FISH_TIER_CUSTOM {
        value
    } else {
        default_fish_tier()
    }
}

pub fn normalise_fish_asr_engine(value: &str) -> String {
    let trimmed = value.trim();
    if FISH_ASR_ENGINES.contains(&trimmed) {
        trimmed.to_string()
    } else {
        default_fish_asr_engine()
    }
}

pub fn normalise_fish_asr_device(value: &str) -> String {
    let trimmed = value.trim();
    if FISH_ASR_DEVICES.contains(&trimmed) {
        trimmed.to_string()
    } else {
        default_fish_asr_device()
    }
}

pub fn normalise_fish_asr_model(value: &str) -> String {
    let trimmed = value.trim();
    if FISH_ASR_MODELS.contains(&trimmed) {
        trimmed.to_string()
    } else {
        default_fish_asr_model()
    }
}

/// Threads: 0 means "unset" and falls back to the default; anything else is
/// clamped into `1..=FISH_ASR_THREADS_MAX`.
pub fn normalise_fish_asr_threads(value: u32) -> u32 {
    if value == 0 {
        default_fish_asr_threads()
    } else {
        value.clamp(1, FISH_ASR_THREADS_MAX)
    }
}

pub fn normalise_fish_vision_detail(value: &str) -> String {
    let trimmed = value.trim();
    if FISH_VISION_DETAILS.contains(&trimmed) {
        trimmed.to_string()
    } else {
        default_fish_vision_detail()
    }
}

fn default_fish_interval_ms() -> u64 {
    1500
}

/// Normalise the 养鱼 model name.
///
/// A blank value (hand-edited or older file) falls back to
/// [`default_fish_model`]; anything else is kept verbatim, so a newer model
/// name typed into `settings.json` is never silently replaced.
pub fn normalise_fish_model(value: &str) -> String {
    let trimmed = value.trim();
    // `deepseek-chat` is the retired alias for the same model the docs now
    // call `deepseek-flash`; fold it in so old settings.json files migrate.
    if trimmed.is_empty() || trimmed == "deepseek-chat" {
        default_fish_model()
    } else {
        trimmed.to_string()
    }
}

/// Normalise the 鱼主人的指令.
///
/// A blank instruction would leave the prompt's last section empty, so it falls
/// back to [`default_fish_instruction`]; otherwise the text is kept verbatim
/// (only trimmed at both ends).
pub fn normalise_fish_instruction(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default_fish_instruction()
    } else {
        trimmed.to_string()
    }
}

/// Normalise the per-run video budget (clamped into `1..=FISH_BUDGET_MAX`).
///
/// `0` is not a valid budget — it would mean "look at nothing" — so it clamps to
/// the minimum of 1, exactly like a corrupt large value clamps to 500.
pub fn normalise_fish_budget(value: u32) -> u32 {
    value.clamp(1, FISH_BUDGET_MAX)
}

/// Normalise the pause between two videos (clamped into
/// `FISH_INTERVAL_MIN_MS..=FISH_INTERVAL_MAX_MS`).
pub fn normalise_fish_interval(value: u64) -> u64 {
    value.clamp(FISH_INTERVAL_MIN_MS, FISH_INTERVAL_MAX_MS)
}

/// Snap an `HH:MM` string to a valid local time, falling back to `20:00`.
///
/// Accepts `H:MM` and `HH:MM` (24-hour); anything else — an empty field, a
/// pasted date, `25:00` — falls back to [`default_fish_feed_time`] instead of
/// silently scheduling nothing.
pub fn normalise_fish_time(value: &str) -> String {
    let trimmed = value.trim();
    let mut parts = trimmed.split(':');
    let (Some(hour), Some(minute), None) = (parts.next(), parts.next(), parts.next()) else {
        return default_fish_feed_time();
    };
    let (Ok(hour), Ok(minute)) = (hour.trim().parse::<u32>(), minute.trim().parse::<u32>())
    else {
        return default_fish_feed_time();
    };
    if hour > 23 || minute > 59 {
        return default_fish_feed_time();
    }
    format!("{hour:02}:{minute:02}")
}

/// Drop unknown source bits; an empty mask falls back to 首页, because a source
/// set of nothing would make every 开饭 a no-op.
pub fn normalise_fish_sources(value: u32) -> u32 {
    let known = value & FISH_SOURCES_ALL;
    if known == 0 {
        default_fish_sources()
    } else {
        known
    }
}

/// Parse an `HH:MM` time into minutes since midnight (`None` when invalid).
pub fn fish_time_minutes(value: &str) -> Option<u32> {
    let normalised = normalise_fish_time(value);
    let mut parts = normalised.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    Some(hour * 60 + minute)
}

/// Normalise the player's audio tier.
///
/// Only the ids the player can actually request ([`AUDIO_QUALITIES`]) are kept;
/// anything else (a hand-edited or older file) becomes `0` = 自动, which always
/// plays something.
pub fn normalise_audio_quality(value: u32) -> u32 {
    if AUDIO_QUALITIES.contains(&value) {
        value
    } else {
        0
    }
}

/// 隐藏直播默认关闭：工具栏保持改动前的六个 pill。
pub fn default_hide_live_tab() -> bool {
    false
}

/// Normalise the 防干扰 隐藏直播 switch.
///
/// A `bool` has only two states, so there is nothing to clamp — the function
/// exists so [`load`] routes every persisted flag through a named normaliser
/// (the project convention) and so tests can pin the default: a file written
/// before this field existed parses through `#[serde(default)]` and lands here
/// as [`default_hide_live_tab`] (off).
pub fn normalise_hide_live_tab(value: bool) -> bool {
    value
}

/// 隐藏搜索热搜默认关闭：空输入时热搜面板照旧出现。
pub fn default_hide_hot_search() -> bool {
    false
}

/// Normalise the 防干扰 隐藏热搜 switch (see [`normalise_hide_live_tab`]).
pub fn normalise_hide_hot_search(value: bool) -> bool {
    value
}

/// Human-readable name of one audio tier (see [`AUDIO_QUALITIES`]).
pub fn audio_tier_name(id: u32) -> &'static str {
    match id {
        30216 => "64K",
        30232 => "132K",
        30280 => "192K",
        30250 => "杜比全景声",
        30251 => "Hi-Res 无损",
        _ => "自动",
    }
}

/// Mask a DeepSeek key for display: `sk-…abcd`.
///
/// Only the `sk-` prefix and the last four characters are shown, so a
/// screenshot or a shared log never leaks a usable key. Short or blank values
/// degrade to `sk-…` instead of revealing themselves.
pub fn mask_key(key: &str) -> String {
    let key = key.trim();
    let last: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if key.chars().count() <= 8 || last.chars().count() < 4 {
        "sk-…".to_string()
    } else {
        format!("sk-…{last}")
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
    let mut current =
        current.and_then(|wanted| kept.iter().position(|(index, _)| *index == wanted));
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
            // f176: decode at the stream's own size unless the owner opts out.
            player_native_size: true,
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
            backdrop_blur: default_backdrop_blur(),
            page_size: default_page_size(),
            close_action: default_close_action(),
            player_layout: None,
            feed_prefetch_batches: default_feed_prefetch_batches(),
            cdn_select: default_cdn_select(),
            prefetch_proxy: default_prefetch_proxy(),
            seek_points: default_seek_points(),
            prefetch_connections: default_prefetch_connections(),
            net_threads: default_net_threads(),
            playlist: Vec::new(),
            playlist_current: None,
            play_mode: default_play_mode(),
            interp_mode: 0,
            deepseek_key: String::new(),
            fish_model: default_fish_model(),
            fish_instruction: default_fish_instruction(),
            fish_budget_videos: default_fish_budget_videos(),
            fish_interval_ms: default_fish_interval_ms(),
            fish_enabled: false,
            fish_auto_feed: false,
            fish_feed_time: default_fish_feed_time(),
            fish_sources: default_fish_sources(),
            audio_quality: 0,
            hide_live_tab: default_hide_live_tab(),
            hide_hot_search: default_hide_hot_search(),
            fish_tier: default_fish_tier(),
            fish_step_meta: true,
            fish_step_subtitle: true,
            fish_step_outline: false,
            fish_step_comments: true,
            fish_step_asr: false,
            fish_step_frames: false,
            fish_asr_enabled: false,
            fish_asr_engine: default_fish_asr_engine(),
            fish_asr_device: default_fish_asr_device(),
            fish_asr_model: default_fish_asr_model(),
            fish_asr_threads: default_fish_asr_threads(),
            fish_asr_model_dir: String::new(),
            fish_vision_detail: default_fish_vision_detail(),
            pgc_sorts: BTreeMap::new(),
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
        player_quality: normalise_player_quality(settings.player_quality),
        player_native_size: settings.player_native_size,
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
        backdrop_blur: normalise_backdrop_blur(settings.backdrop_blur),
        page_size: normalise_page_size(settings.page_size),
        close_action: normalise_close_action(settings.close_action),
        player_layout: normalise_player_layout(settings.player_layout),
        feed_prefetch_batches: normalise_feed_prefetch_batches(settings.feed_prefetch_batches),
        cdn_select: settings.cdn_select,
        prefetch_proxy: settings.prefetch_proxy,
        seek_points: normalise_seek_points(settings.seek_points),
        prefetch_connections: normalise_prefetch_connections(settings.prefetch_connections),
        net_threads: normalise_net_threads(settings.net_threads),
        playlist,
        playlist_current,
        play_mode: normalise_play_mode(settings.play_mode),
        interp_mode: normalise_interp_mode(settings.interp_mode),
        deepseek_key: settings.deepseek_key.trim().to_string(),
        fish_model: normalise_fish_model(&settings.fish_model),
        fish_instruction: normalise_fish_instruction(&settings.fish_instruction),
        fish_budget_videos: normalise_fish_budget(settings.fish_budget_videos),
        fish_interval_ms: normalise_fish_interval(settings.fish_interval_ms),
        fish_enabled: settings.fish_enabled,
        fish_auto_feed: settings.fish_auto_feed,
        fish_feed_time: normalise_fish_time(&settings.fish_feed_time),
        fish_sources: normalise_fish_sources(settings.fish_sources),
        audio_quality: normalise_audio_quality(settings.audio_quality),
        hide_live_tab: normalise_hide_live_tab(settings.hide_live_tab),
        hide_hot_search: normalise_hide_hot_search(settings.hide_hot_search),
        fish_tier: normalise_fish_tier(settings.fish_tier),
        fish_step_meta: settings.fish_step_meta,
        fish_step_subtitle: settings.fish_step_subtitle,
        fish_step_outline: settings.fish_step_outline,
        fish_step_comments: settings.fish_step_comments,
        fish_step_asr: settings.fish_step_asr,
        fish_step_frames: settings.fish_step_frames,
        fish_asr_enabled: settings.fish_asr_enabled,
        fish_asr_engine: normalise_fish_asr_engine(&settings.fish_asr_engine),
        fish_asr_device: normalise_fish_asr_device(&settings.fish_asr_device),
        fish_asr_model: normalise_fish_asr_model(&settings.fish_asr_model),
        fish_asr_threads: normalise_fish_asr_threads(settings.fish_asr_threads),
        fish_asr_model_dir: settings.fish_asr_model_dir.trim().to_string(),
        fish_vision_detail: normalise_fish_vision_detail(&settings.fish_vision_detail),
        pgc_sorts: normalise_pgc_sorts(settings.pgc_sorts),
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

// ---- 继续观看 (PGC progress) -----------------------------------------------
//
// `settings.json` carries preferences, and rewriting it for every episode
// watched would make the file churn for something that is pure progress. This
// is a separate small table in its own file instead, one row per season:
//
//   { "seasons": { "835": { "ep_id": 216351, "bvid": "BV1…", "cid": 12345,
//                           "index": 4, "title": "第 4 话 …", "at": 1761… } } }
//
// `index` is the 1-based position of that episode inside the season's 正片 list
// at the time it was watched, so the 番剧 page can print 「看到第 N 集」 from the
// table alone (no season fetch on the card path). The whole table is capped at
// [`CONTINUE_MAX_SEASONS`] rows, oldest `at` dropped first.

/// Where one season was last watched (the episode that actually reached its
/// first frame).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinueEntry {
    /// The episode's `ep_id`.
    #[serde(default)]
    pub ep_id: i64,
    #[serde(default)]
    pub bvid: String,
    #[serde(default)]
    pub cid: u64,
    /// 1-based position in the season's 正片 list (0 = unknown).
    #[serde(default)]
    pub index: u32,
    /// The episode line the 选集 shows (`long_title` first).
    #[serde(default)]
    pub title: String,
    /// Unix seconds of the write.
    #[serde(default)]
    pub at: i64,
}

impl ContinueEntry {
    /// 「看到第 N 集」 for the card badge; empty when `index` is unknown.
    pub fn badge_text(&self) -> String {
        if self.index > 0 {
            format!("看到第 {} 集", self.index)
        } else {
            String::new()
        }
    }
}

/// The whole 继续观看 table, keyed by season_id.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContinueStore {
    #[serde(default)]
    pub seasons: BTreeMap<i64, ContinueEntry>,
}

/// One row per season; the table is a convenience, not a history.
pub const CONTINUE_MAX_SEASONS: usize = 200;

impl ContinueStore {
    /// `continue.json` beside `settings.json`.
    pub fn path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("continue.json")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// Read one table. A missing or corrupt file is an empty table, never an
    /// error: losing progress must not break the 番剧 page.
    pub fn load_from(path: &Path) -> Self {
        let Some(data) = std::fs::read_to_string(path).ok() else {
            return Self::default();
        };
        let store: Self = serde_json::from_str(&data).unwrap_or_default();
        store.pruned()
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// Write the table atomically: a temp file in the same directory, then a
    /// rename over the destination, so a crash mid-write can never leave a
    /// truncated table behind.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, season_id: i64) -> Option<&ContinueEntry> {
        self.seasons.get(&season_id)
    }

    pub fn len(&self) -> usize {
        self.seasons.len()
    }

    /// Record one season's episode and drop the oldest rows over the cap.
    pub fn set(&mut self, season_id: i64, entry: ContinueEntry) {
        if season_id <= 0 {
            return;
        }
        self.seasons.insert(season_id, entry);
        while self.seasons.len() > CONTINUE_MAX_SEASONS {
            let Some(oldest) = self
                .seasons
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.seasons.remove(&oldest);
        }
    }

    /// Same, then persist. A failed write is logged by the caller, not here.
    pub fn set_and_save(&mut self, season_id: i64, entry: ContinueEntry) -> Result<()> {
        self.set(season_id, entry);
        self.save()
    }

    fn pruned(mut self) -> Self {
        self.seasons.retain(|id, entry| *id > 0 && entry.ep_id > 0);
        while self.seasons.len() > CONTINUE_MAX_SEASONS {
            let Some(oldest) = self
                .seasons
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.seasons.remove(&oldest);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_continue_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "o11a1-continue-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn pgc_sort_keys_are_the_verified_ones() {
        // The four keys the dropdown offers, in dropdown order. Every one of
        // them was checked against the live index endpoint; see PGC_SORT_KEYS.
        assert_eq!(PGC_SORT_KEYS, ["follows", "update", "views", "score"]);
        for key in PGC_SORT_KEYS {
            assert_eq!(normalise_pgc_sort(key), key);
        }
        assert_eq!(normalise_pgc_sort("  score "), "score");
    }

    #[test]
    fn pgc_sort_normalisation_drops_unknown_keys_and_values() {
        assert_eq!(normalise_pgc_sort("danmaku"), "");
        assert_eq!(normalise_pgc_sort(""), "");
        let mut stored = BTreeMap::new();
        stored.insert("1".to_string(), "update".to_string());
        stored.insert("4".to_string(), "nonsense".to_string());
        stored.insert("anime".to_string(), "score".to_string());
        stored.insert("-3".to_string(), "score".to_string());
        let clean = normalise_pgc_sorts(stored);
        assert_eq!(clean.len(), 1);
        assert_eq!(clean.get("1").map(String::as_str), Some("update"));
    }

    #[test]
    fn pgc_sorts_survive_a_json_round_trip() {
        let mut settings = Settings::default();
        assert!(settings.pgc_sorts.is_empty());
        settings
            .pgc_sorts
            .insert("1".to_string(), "views".to_string());
        settings
            .pgc_sorts
            .insert("4".to_string(), "score".to_string());
        let json = serde_json::to_string(&settings).expect("serialise settings");
        let back: Settings = serde_json::from_str(&json).expect("parse settings");
        assert_eq!(back.pgc_sorts, settings.pgc_sorts);
        // A file written before this field existed still loads.
        let old: Settings = serde_json::from_str(r#"{ "page_size": 24 }"#).expect("old file");
        assert!(old.pgc_sorts.is_empty());
    }

    #[test]
    fn continue_store_round_trips_through_a_file() {
        let path = temp_continue_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut store = ContinueStore::default();
        store.set(
            835,
            ContinueEntry {
                ep_id: 216351,
                bvid: "BV1xx411c7mD".to_string(),
                cid: 4321,
                index: 4,
                title: "第 4 话 秘密基地".to_string(),
                at: 1_760_000_000,
            },
        );
        store.save_to(&path).expect("save");
        // Real read-back from the file, not the in-memory value.
        let back = ContinueStore::load_from(&path);
        assert_eq!(back.len(), 1);
        let entry = back.get(835).expect("season 835");
        assert_eq!(entry.ep_id, 216351);
        assert_eq!(entry.cid, 4321);
        assert_eq!(entry.index, 4);
        assert_eq!(entry.badge_text(), "看到第 4 集");
        assert_eq!(entry.at, 1_760_000_000);
        assert_eq!(
            serde_json::from_str::<ContinueStore>(&std::fs::read_to_string(&path).unwrap())
                .map(|store| store.len())
                .unwrap_or(0),
            1
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn continue_store_drops_the_oldest_rows_over_the_cap() {
        let mut store = ContinueStore::default();
        for index in 0..(CONTINUE_MAX_SEASONS as i64 + 5) {
            store.set(
                index + 1,
                ContinueEntry {
                    ep_id: index + 1,
                    at: index,
                    ..ContinueEntry::default()
                },
            );
        }
        assert_eq!(store.len(), CONTINUE_MAX_SEASONS);
        assert!(store.get(1).is_none(), "oldest row must be dropped");
        assert!(store.get(5).is_none(), "the five oldest rows are gone");
        assert!(store.get(6).is_some());
        assert!(store.get(CONTINUE_MAX_SEASONS as i64 + 5).is_some());
    }

    #[test]
    fn continue_store_tolerates_a_missing_or_corrupt_file() {
        let missing = temp_continue_path("missing");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(ContinueStore::load_from(&missing).len(), 0);

        let corrupt = temp_continue_path("corrupt");
        std::fs::write(&corrupt, "{ not json").expect("write corrupt file");
        assert_eq!(ContinueStore::load_from(&corrupt).len(), 0);

        // A row without an ep_id can never be resumed, so it is dropped.
        let partial = temp_continue_path("partial");
        std::fs::write(
            &partial,
            r#"{ "seasons": { "1": { "bvid": "BV1" }, "2": { "ep_id": 9, "index": 2 } } }"#,
        )
        .expect("write partial file");
        let store = ContinueStore::load_from(&partial);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(2).map(|entry| entry.index), Some(2));
        let _ = std::fs::remove_file(&corrupt);
        let _ = std::fs::remove_file(&partial);
    }

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
        let settings = match serde_json::from_str::<Settings>(r#"{ "seek_points": 99 }"#) {
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
        assert_eq!(normalise_seek_points(decoded.seek_points), SEEK_POINTS_MAX);
        Ok(())
    }

    #[test]
    fn net_threads_defaults_to_twelve_and_normalises() {
        assert_eq!(default_net_threads(), NET_THREADS_DEFAULT);
        assert_eq!(Settings::default().net_threads, NET_THREADS_DEFAULT);
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(settings.net_threads, NET_THREADS_DEFAULT);
        assert_eq!(normalise_net_threads(0), NET_THREADS_DEFAULT);
        assert_eq!(normalise_net_threads(1), NET_THREADS_MIN);
        assert_eq!(normalise_net_threads(12), 12);
        assert_eq!(normalise_net_threads(999), NET_THREADS_MAX);
        assert_eq!(normalise_net_threads(u32::MAX), NET_THREADS_MAX);
        let encoded = match serde_json::to_string(&Settings::default()) {
            Ok(encoded) => encoded,
            Err(_) => String::new(),
        };
        assert!(encoded.contains("\"net_threads\":12"), "{encoded}");
    }

    #[test]
    fn prefetch_connections_defaults_to_sixteen() {
        assert_eq!(default_prefetch_connections(), 16);
        assert_eq!(Settings::default().prefetch_connections, 16);
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(settings.prefetch_connections, 16);
    }

    #[test]
    fn prefetch_connections_keeps_every_offered_value() {
        for value in PREFETCH_CONNECTIONS_MIN..=PREFETCH_CONNECTIONS_MAX {
            assert_eq!(normalise_prefetch_connections(value), value);
        }
    }

    #[test]
    fn prefetch_connections_zero_means_unset() {
        assert_eq!(
            normalise_prefetch_connections(0),
            default_prefetch_connections()
        );
        let settings = match serde_json::from_str::<Settings>(r#"{ "prefetch_connections": 0 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(
            normalise_prefetch_connections(settings.prefetch_connections),
            default_prefetch_connections()
        );
    }

    #[test]
    fn prefetch_connections_clamps_out_of_range_values() {
        for value in 1..PREFETCH_CONNECTIONS_MIN {
            assert_eq!(
                normalise_prefetch_connections(value),
                PREFETCH_CONNECTIONS_MIN
            );
        }
        for value in [PREFETCH_CONNECTIONS_MAX + 1, 33, 99, u32::MAX] {
            assert_eq!(
                normalise_prefetch_connections(value),
                PREFETCH_CONNECTIONS_MAX
            );
        }
    }

    #[test]
    fn prefetch_connections_round_trips_through_json() -> Result<()> {
        let settings = Settings {
            prefetch_connections: PREFETCH_CONNECTIONS_MAX,
            ..Settings::default()
        };
        let encoded = serde_json::to_string(&settings)?;
        let decoded = serde_json::from_str::<Settings>(&encoded)?;
        assert_eq!(
            normalise_prefetch_connections(decoded.prefetch_connections),
            PREFETCH_CONNECTIONS_MAX
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
        let (items, current) = normalise_playlist_state(decoded.playlist, decoded.playlist_current);
        assert_eq!(items, vec![queued("BV1"), queued("BV2")]);
        assert_eq!(current, Some(1));
        assert_eq!(normalise_play_mode(decoded.play_mode), 2);
        Ok(())
    }

    #[test]
    fn playlist_drops_empty_and_duplicate_bvids() {
        let (items, current) = normalise_playlist_state(
            vec![queued("BV1"), queued(""), queued("BV1"), queued("BV2")],
            Some(3),
        );
        assert_eq!(
            items
                .iter()
                .map(|entry| entry.bvid.as_str())
                .collect::<Vec<_>>(),
            vec!["BV1", "BV2"]
        );
        assert_eq!(current, Some(1), "BV2 moved from slot 3 to slot 1");
    }

    #[test]
    fn playlist_cap_keeps_the_newest_and_remaps_current() {
        let all: Vec<QueueEntry> = (0..PLAYLIST_MAX + 5)
            .map(|index| queued(&format!("BV{index}")))
            .collect();
        let (items, current) = normalise_playlist_state(all.clone(), Some(PLAYLIST_MAX + 2));
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
        let (items, current) = normalise_playlist_state(vec![queued("BV1")], Some(4));
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

    #[test]
    fn interp_mode_defaults_to_off() {
        assert_eq!(Settings::default().interp_mode, 0);
        let settings = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(settings.interp_mode, 0);
    }

    #[test]
    fn interp_mode_keeps_the_implemented_modes() {
        for value in INTERP_MODES {
            assert_eq!(normalise_interp_mode(value), value);
        }
        assert_eq!(normalise_interp_mode(1), 1);
        assert_eq!(normalise_interp_mode(2), 2);
        assert_eq!(normalise_interp_mode(3), 3);
    }

    #[test]
    fn interp_mode_rejects_corrupt_and_future_values() {
        // 0..=3 are implemented (off / blend / block match / OFA); anything else is not.
        for value in [4u8, 9, 99, u8::MAX] {
            assert_eq!(normalise_interp_mode(value), 0);
            assert!(!INTERP_MODES.contains(&value));
        }
    }

    #[test]
    fn interp_mode_round_trips_through_json() -> Result<()> {
        for mode in INTERP_MODES {
            let settings = Settings {
                interp_mode: mode,
                ..Settings::default()
            };
            let encoded = serde_json::to_string(&settings)?;
            let decoded = serde_json::from_str::<Settings>(&encoded)?;
            assert_eq!(normalise_interp_mode(decoded.interp_mode), mode);
        }
        Ok(())
    }

    #[test]
    fn audio_quality_keeps_every_offered_id() {
        for id in AUDIO_QUALITIES {
            assert_eq!(normalise_audio_quality(id), id);
        }
        assert_eq!(Settings::default().audio_quality, 0);
    }

    #[test]
    fn audio_quality_rejects_unknown_ids() {
        for id in [1u32, 999, 30217, 30252, u32::MAX] {
            assert_eq!(normalise_audio_quality(id), 0);
        }
    }

    #[test]
    fn audio_tier_names_match_the_ids() {
        assert_eq!(audio_tier_name(0), "自动");
        assert_eq!(audio_tier_name(30216), "64K");
        assert_eq!(audio_tier_name(30232), "132K");
        assert_eq!(audio_tier_name(30280), "192K");
        assert_eq!(audio_tier_name(30250), "杜比全景声");
        assert_eq!(audio_tier_name(30251), "Hi-Res 无损");
        assert_eq!(audio_tier_name(999), "自动");
        for id in AUDIO_QUALITIES {
            assert!(!audio_tier_name(id).is_empty());
        }
    }

    #[test]
    fn audio_quality_missing_from_a_legacy_file_loads_as_auto() -> Result<()> {
        // A settings.json written before this field existed.
        let legacy = r#"{ "page_size": 36, "play_mode": 1, "theme": 2 }"#;
        let decoded = serde_json::from_str::<Settings>(legacy)?;
        assert_eq!(decoded.audio_quality, 0);
        assert_eq!(normalise_audio_quality(decoded.audio_quality), 0);
        Ok(())
    }

    #[test]
    fn audio_quality_round_trips_through_json() -> Result<()> {
        for id in AUDIO_QUALITIES {
            let settings = Settings {
                audio_quality: id,
                ..Settings::default()
            };
            let encoded = serde_json::to_string(&settings)?;
            let decoded = serde_json::from_str::<Settings>(&encoded)?;
            assert_eq!(normalise_audio_quality(decoded.audio_quality), id);
        }
        Ok(())
    }

    #[test]
    fn fish_defaults_are_off_with_a_usable_prompt() {
        let defaults = Settings::default();
        assert!(defaults.deepseek_key.is_empty());
        assert!(!defaults.fish_enabled);
        assert_eq!(defaults.fish_model, "deepseek-flash");
        assert_eq!(defaults.fish_instruction, default_fish_instruction());
        assert_eq!(defaults.fish_budget_videos, 40);
        assert_eq!(defaults.fish_interval_ms, 1500);
        let legacy = r#"{ "page_size": 36 }"#;
        let decoded = serde_json::from_str::<Settings>(legacy).expect("legacy file parses");
        assert_eq!(decoded.fish_model, "deepseek-flash");
        assert_eq!(decoded.fish_instruction, default_fish_instruction());
        assert_eq!(decoded.fish_budget_videos, 40);
        assert_eq!(decoded.fish_interval_ms, 1500);
        assert!(!decoded.fish_enabled);
    }

    #[test]
    fn fish_budget_and_interval_clamp_into_range() {
        assert_eq!(normalise_fish_budget(0), 1);
        assert_eq!(normalise_fish_budget(1), 1);
        assert_eq!(normalise_fish_budget(40), 40);
        assert_eq!(normalise_fish_budget(FISH_BUDGET_MAX), FISH_BUDGET_MAX);
        assert_eq!(normalise_fish_budget(FISH_BUDGET_MAX + 1), FISH_BUDGET_MAX);
        assert_eq!(normalise_fish_budget(u32::MAX), FISH_BUDGET_MAX);
        assert_eq!(normalise_fish_interval(0), FISH_INTERVAL_MIN_MS);
        assert_eq!(normalise_fish_interval(1500), 1500);
        assert_eq!(normalise_fish_interval(u64::MAX), FISH_INTERVAL_MAX_MS);
    }

    #[test]
    fn fish_model_and_instruction_never_stay_blank() {
        assert_eq!(normalise_fish_model("  "), "deepseek-flash");
        assert_eq!(normalise_fish_model("deepseek-chat"), "deepseek-flash");
        assert_eq!(normalise_fish_model("deepseek-reasoner"), "deepseek-reasoner");
        assert_eq!(normalise_fish_instruction("   "), default_fish_instruction());
        assert_eq!(normalise_fish_instruction(" 只看教程 "), "只看教程");
    }

    #[test]
    fn mask_key_shows_only_the_last_four_characters() {
        assert_eq!(mask_key("sk-1234567890abcd"), "sk-…abcd");
        assert_eq!(mask_key("  sk-1234567890abcd  "), "sk-…abcd");
        assert_eq!(mask_key(""), "sk-…");
        assert_eq!(mask_key("sk-"), "sk-…");
        assert_eq!(mask_key("sk-1234"), "sk-…");
        assert!(!mask_key("sk-1234567890abcd").contains("1234567890"));
    }

    #[test]
    fn guard_switches_default_off() {
        let defaults = Settings::default();
        assert!(!defaults.hide_live_tab);
        assert!(!defaults.hide_hot_search);
        assert!(!default_hide_live_tab());
        assert!(!default_hide_hot_search());
        // A settings.json written before 防干扰 existed: the missing fields take
        // the serde default, so both switches come up off (the pre-change app).
        let legacy = r#"{ "page_size": 36, "theme": 2 }"#;
        let decoded = serde_json::from_str::<Settings>(legacy).expect("legacy file parses");
        assert_eq!(decoded.hide_live_tab, default_hide_live_tab());
        assert_eq!(decoded.hide_hot_search, default_hide_hot_search());
        assert!(!decoded.hide_live_tab);
        assert!(!decoded.hide_hot_search);
    }

    #[test]
    fn guard_switches_keep_both_boolean_states() {
        for value in [false, true] {
            assert_eq!(normalise_hide_live_tab(value), value);
            assert_eq!(normalise_hide_hot_search(value), value);
        }
    }

    #[test]
    fn guard_switches_round_trip_through_json() -> Result<()> {
        let settings = Settings {
            hide_live_tab: true,
            hide_hot_search: true,
            ..Settings::default()
        };
        let encoded = serde_json::to_string(&settings)?;
        assert!(encoded.contains("\"hide_live_tab\":true"), "{encoded}");
        assert!(encoded.contains("\"hide_hot_search\":true"), "{encoded}");
        let decoded = serde_json::from_str::<Settings>(&encoded)?;
        assert!(
            normalise_hide_live_tab(decoded.hide_live_tab),
            "隐藏直播 survives a round trip"
        );
        assert!(
            normalise_hide_hot_search(decoded.hide_hot_search),
            "隐藏热搜 survives a round trip"
        );
        let encoded_default = serde_json::to_string(&Settings::default())?;
        assert!(
            encoded_default.contains("\"hide_live_tab\":false"),
            "{encoded_default}"
        );
        assert!(
            encoded_default.contains("\"hide_hot_search\":false"),
            "{encoded_default}"
        );
        Ok(())
    }

    #[test]
    fn watch_tier_defaults_to_t1_and_normalises() {
        let defaults = Settings::default();
        assert_eq!(defaults.fish_tier, 1);
        assert!(defaults.fish_step_meta);
        assert!(defaults.fish_step_subtitle);
        assert!(defaults.fish_step_comments);
        assert!(!defaults.fish_step_outline);
        assert!(!defaults.fish_step_asr);
        assert!(!defaults.fish_step_frames);
        for tier in 0..=FISH_TIER_CUSTOM {
            assert_eq!(normalise_fish_tier(tier), tier);
        }
        for tier in [6u32, 99, u32::MAX] {
            assert_eq!(normalise_fish_tier(tier), default_fish_tier());
        }
        let legacy = match serde_json::from_str::<Settings>(r#"{ "page_size": 36 }"#) {
            Ok(settings) => settings,
            Err(_) => Settings::default(),
        };
        assert_eq!(legacy.fish_tier, 1);
    }

    #[test]
    fn asr_settings_default_to_small_gpu_and_normalise() {
        let defaults = Settings::default();
        assert!(!defaults.fish_asr_enabled);
        assert_eq!(defaults.fish_asr_engine, "auto");
        assert_eq!(defaults.fish_asr_device, "gpu");
        assert_eq!(defaults.fish_asr_model, "small");
        assert_eq!(defaults.fish_asr_threads, 4);
        assert_eq!(defaults.fish_vision_detail, "low");
        for engine in FISH_ASR_ENGINES {
            assert_eq!(normalise_fish_asr_engine(engine), engine);
        }
        for device in FISH_ASR_DEVICES {
            assert_eq!(normalise_fish_asr_device(device), device);
        }
        for model in FISH_ASR_MODELS {
            assert_eq!(normalise_fish_asr_model(model), model);
        }
        for detail in FISH_VISION_DETAILS {
            assert_eq!(normalise_fish_vision_detail(detail), detail);
        }
        assert_eq!(normalise_fish_asr_engine("nope"), "auto");
        assert_eq!(normalise_fish_asr_device("tpu"), "gpu");
        assert_eq!(normalise_fish_asr_model("large"), "small");
        assert_eq!(normalise_fish_vision_detail("high"), "low");
        assert_eq!(normalise_fish_asr_threads(0), 4);
        assert_eq!(normalise_fish_asr_threads(99), FISH_ASR_THREADS_MAX);
    }

    #[test]
    fn watch_and_asr_settings_survive_a_json_round_trip() -> Result<()> {
        let settings = Settings {
            fish_tier: FISH_TIER_CUSTOM,
            fish_step_meta: true,
            fish_step_subtitle: false,
            fish_step_outline: true,
            fish_step_comments: true,
            fish_step_asr: true,
            fish_step_frames: false,
            fish_asr_enabled: true,
            fish_asr_engine: "faster-whisper".to_string(),
            fish_asr_device: "cpu".to_string(),
            fish_asr_model: "medium".to_string(),
            fish_asr_threads: 8,
            fish_vision_detail: "original".to_string(),
            ..Settings::default()
        };
        let encoded = serde_json::to_string(&settings)?;
        let decoded = serde_json::from_str::<Settings>(&encoded)?;
        assert_eq!(decoded.fish_tier, FISH_TIER_CUSTOM);
        assert!(decoded.fish_step_asr);
        assert!(!decoded.fish_step_subtitle);
        assert_eq!(decoded.fish_asr_engine, "faster-whisper");
        assert_eq!(decoded.fish_asr_device, "cpu");
        assert_eq!(decoded.fish_asr_model, "medium");
        assert_eq!(decoded.fish_asr_threads, 8);
        assert_eq!(decoded.fish_vision_detail, "original");
        Ok(())
    }
}
