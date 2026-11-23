//! Interactive frame lab: sample a video at a user-chosen interval, compose
//! the frames into contact sheets, and let DeepSeek read each sheet in detail.
//!
//! The watch step and the 画面 tab share this builder, so the agent and the
//! human see the same sampling. Sheets are at most 2048px on the long side —
//! six 960×540 tiles (landscape) or six 540×960 tiles (portrait) — which keeps
//! on-screen text legible for the vision model while staying inside one image
//! request per sheet. The temp movie is deleted before any vision request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures::StreamExt;
use image::codecs::jpeg::JpegEncoder;
use image::{imageops, ImageBuffer, Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::super::proc;
use crate::bilibili::Session;
use crate::fishwatch::{Meta, WatchConfig};

/// Cached `ffmpeg -version` probe; the answer cannot change while we run, and
/// probing on every settings rebuild would spawn a process.
pub fn ffmpeg_available(ffmpeg: &str) -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        proc::command(ffmpeg)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

const SOURCE_SHORT_SIDE: u32 = 720;
const MAX_FRAMES: usize = 60;
const SHEET_COLUMNS: u32 = 2;
const SHEET_ROWS: u32 = 3;
const SHEET_PADDING: u32 = 12;
const SHEET_GAP: u32 = 10;
const INTERVAL_MIN_S: u64 = 2;
const INTERVAL_MAX_S: u64 = 3_600;
const VISION_DETAIL: &str = "high";
const VISION_MAX_TOKENS: u32 = 1_800;

/// Progress for one build/observe job. Stages: `meta`, `download`, `extract`,
/// `compose`, `vision`, `done`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LabProgress {
    pub stage: String,
    pub message: String,
    pub pct: u32,
    pub current: usize,
    pub total: usize,
}

pub type ProgressFn<'a> = &'a (dyn Fn(LabProgress) + Send + Sync);

fn emit(progress: Option<ProgressFn<'_>>, stage: &str, message: String, pct: u32, current: usize, total: usize) {
    if let Some(progress) = progress {
        progress(LabProgress {
            stage: stage.to_string(),
            message,
            pct,
            current,
            total,
        });
    }
}

fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

#[derive(Clone, Debug, Serialize)]
pub struct LabVideo {
    pub bvid: String,
    pub cid: u64,
    pub title: String,
    pub up_name: String,
    pub cover: String,
    pub duration_s: u64,
}

impl LabVideo {
    fn from_meta(meta: &Meta) -> Self {
        LabVideo {
            bvid: meta.bvid.clone(),
            cid: meta.cid,
            title: meta.title.clone(),
            up_name: meta.up_name.clone(),
            cover: meta.cover.clone(),
            duration_s: meta.duration_s,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuiltSheet {
    pub index: usize,
    pub from_s: u64,
    pub to_s: u64,
    pub times: Vec<u64>,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub columns: u32,
    pub rows: u32,
}

#[derive(Clone, Debug)]
pub struct BuiltLab {
    pub lab_id: String,
    pub video: LabVideo,
    pub interval_s: u64,
    pub frame_count: usize,
    pub source: String,
    pub video_bytes: u64,
    pub image_bytes: u64,
    pub elapsed_ms: u64,
    pub dir: PathBuf,
    pub sheets: Vec<BuiltSheet>,
}

/// One downloaded 720P movie kept for the lifetime of an agent video, so each
/// clip request can cut from the local file instead of downloading the whole
/// video again.
#[derive(Clone, Debug)]
pub struct MovieSource {
    pub path: PathBuf,
    pub dir: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub source: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TileObservation {
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub scene: String,
    #[serde(default)]
    pub action: String,
    #[serde(default, alias = "screen_text", alias = "text")]
    pub onscreen_text: Vec<String>,
    #[serde(default)]
    pub details: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SheetObservation {
    #[serde(default)]
    pub tiles: Vec<TileObservation>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub flags: Vec<String>,
}

pub struct ObservedLab {
    pub lab_id: String,
    pub sheets: Vec<ObservedSheet>,
    pub tokens: u64,
    pub elapsed_ms: u64,
    pub failed: usize,
}

pub struct ObservedSheet {
    pub index: usize,
    pub from_s: u64,
    pub to_s: u64,
    pub times: Vec<u64>,
    pub observation: SheetObservation,
    pub tokens: u64,
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Auto interval used by the agent when the owner leaves the setting on
/// "自动": at least 8s, otherwise about 30 samples across the video.
pub fn auto_interval(duration_s: u64) -> u64 {
    ((duration_s.max(1) as f64 / 30.0).ceil() as u64).max(8)
}

pub fn frame_count(duration_s: u64, interval_s: u64) -> usize {
    if duration_s == 0 || interval_s == 0 {
        return 0;
    }
    ((duration_s / interval_s) as usize + 1).min(MAX_FRAMES)
}

pub fn resolve_interval(duration_s: u64, requested_s: u64) -> Result<u64> {
    let duration = duration_s.max(1);
    let interval = if requested_s == 0 {
        auto_interval(duration)
    } else {
        requested_s.clamp(INTERVAL_MIN_S, INTERVAL_MAX_S)
    };
    let count = (duration / interval) as usize + 1;
    if count > MAX_FRAMES {
        // `count = floor(duration / interval) + 1 <= MAX_FRAMES`.
        let minimum = ((duration as f64) / (MAX_FRAMES as f64 - 1.0))
            .ceil()
            .max(INTERVAL_MIN_S as f64) as u64;
        bail!(
            "间隔 {interval} 秒会生成 {count} 张画面，超过 {MAX_FRAMES} 张上限；请把间隔调到至少 {minimum} 秒"
        );
    }
    Ok(interval)
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Fetch the video meta, then build contact sheets. The UI's 画面 tab uses this
/// entry point; the agent reuses [`build_with_meta`] because it already has it.
pub async fn build(
    config: &WatchConfig,
    session: &Session,
    bvid: &str,
    interval_s: u64,
    progress: Option<ProgressFn<'_>>,
    cancel: Arc<AtomicBool>,
) -> Result<BuiltLab> {
    let bvid = bvid.trim();
    if bvid.is_empty() {
        bail!("缺少 bvid");
    }
    emit(progress, "meta", "读取视频信息…".to_string(), 1, 0, 0);
    let info = session
        .fish_meta(bvid)
        .await
        .with_context(|| format!("读取视频信息失败：{bvid}"))?;
    let meta = Meta {
        bvid: bvid.to_string(),
        cid: info.cid,
        aid: info.aid,
        title: info.title,
        up_mid: info.up_mid,
        up_name: info.up_name,
        duration_s: info.duration_s,
        view: info.view,
        pub_ts: info.pub_ts,
        cover: info.cover,
        tname: info.tname,
        desc: info.desc,
    };
    build_with_meta(config, session, meta, interval_s, progress, cancel).await
}

/// Download the 720P movie once and keep it for range cuts. The caller owns
/// `dir` and must remove it when the video is finished.
pub async fn download_movie(
    config: &WatchConfig,
    session: &Session,
    bvid: &str,
    cid: u64,
    dir: &Path,
    progress: Option<ProgressFn<'_>>,
    cancel: Arc<AtomicBool>,
) -> Result<MovieSource> {
    if !config.online {
        bail!("未登录");
    }
    if !ffmpeg_available(&config.ffmpeg) {
        bail!("ffmpeg 不在 PATH");
    }
    if cid == 0 {
        bail!("缺少 cid");
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建视频缓存目录失败：{}", dir.display()))?;
    let movie = dir.join("movie.mp4");
    let (bytes, track) =
        download_source(session, bvid, cid, &movie, progress, &cancel).await?;
    Ok(MovieSource {
        path: movie,
        dir: dir.to_path_buf(),
        width: track.0,
        height: track.1,
        bytes,
        source: format!("720P · {}×{}", track.0, track.1),
    })
}

/// The requested interval in seconds. The agent may ask for 1, 5, 10, 20, 30
/// or 60; other values are clamped into that range.
pub fn clip_interval(requested_s: u64) -> u64 {
    requested_s.clamp(1, 60)
}

/// Number of frames for a clip. A 60s clip at 1s yields 60 sample points
/// (0..59), not 61, so a 6×5 sheet packs it exactly.
pub fn clip_frame_count(duration_s: u64, interval_s: u64) -> usize {
    duration_s
        .max(1)
        .div_ceil(interval_s.max(1))
        .max(1) as usize
}

/// Cut `[start_s, end_s)` from an already downloaded movie and compose it into
/// contact sheets. The caller registers the sheet paths with the engine so the
/// transcript can show exactly what the model saw.
pub fn build_range_sheets(
    config: &WatchConfig,
    movie: &MovieSource,
    work: &Path,
    start_s: u64,
    end_s: u64,
    interval_s: u64,
    cancel: &AtomicBool,
) -> Result<Vec<BuiltSheet>> {
    let end_s = end_s.max(start_s.saturating_add(1));
    let duration = end_s.saturating_sub(start_s);
    let interval = clip_interval(interval_s);
    let expected = clip_frame_count(duration, interval);
    if expected == 0 {
        bail!("切片时长太短，无法抽帧");
    }
    std::fs::create_dir_all(work)
        .with_context(|| format!("创建切片目录失败：{}", work.display()))?;
    let layout = clip_layout_for(movie.width.max(1), movie.height.max(1));
    let frames = extract(
        config,
        &movie.path,
        work,
        interval,
        expected,
        layout,
        start_s,
        duration,
        cancel,
    )?;
    if frames.is_empty() {
        bail!("这个时间段没有抽出画面");
    }
    let (sheets, _) = compose_sheets(&frames, work, layout, None, cancel)?;
    Ok(sheets)
}

/// Cut a range directly from the signed CDN URL. This is the fast path for the
/// agent: only the requested seconds are pulled, so a 30-minute video does not
/// have to download before the first clip.
pub fn build_range_sheets_url(
    config: &WatchConfig,
    source: &RemoteSource,
    work: &Path,
    start_s: u64,
    end_s: u64,
    interval_s: u64,
    cancel: &AtomicBool,
) -> Result<Vec<BuiltSheet>> {
    let end_s = end_s.max(start_s.saturating_add(1));
    let duration = end_s.saturating_sub(start_s);
    let interval = clip_interval(interval_s);
    let expected = clip_frame_count(duration, interval);
    if expected == 0 {
        bail!("切片时长太短，无法抽帧");
    }
    std::fs::create_dir_all(work)
        .with_context(|| format!("创建切片目录失败：{}", work.display()))?;
    let layout = clip_layout_for(source.width.max(1), source.height.max(1));
    let frames = extract_remote(
        config,
        source,
        work,
        interval,
        expected,
        layout,
        start_s,
        duration,
        cancel,
    )?;
    if frames.is_empty() {
        bail!("这个时间段没有抽出画面");
    }
    let (sheets, _) = compose_sheets(&frames, work, layout, None, cancel)?;
    Ok(sheets)
}

fn extract_remote(
    config: &WatchConfig,
    source: &RemoteSource,
    work: &Path,
    interval_s: u64,
    expected: usize,
    layout: Layout,
    start_s: u64,
    duration_s: u64,
    cancel: &AtomicBool,
) -> Result<Vec<ExtractedFrame>> {
    let scale = if layout.landscape {
        format!("scale={}:-2", layout.cell_w)
    } else if layout.square {
        format!("scale={}:-2", layout.cell_w)
    } else {
        format!("scale=-2:{}", layout.cell_h)
    };
    let filter = format!("fps=1/{interval_s},{scale}");
    let pattern = work.join("f-%03d.jpg");
    let mut command = proc::command(&config.ffmpeg);
    command.args(["-v", "error", "-y"]);
    command.arg("-headers").arg(&source.headers);
    if start_s > 0 {
        command.arg("-ss").arg(start_s.to_string());
    }
    command.arg("-i").arg(&source.url);
    command.arg("-t").arg(duration_s.max(1).to_string());
    let status = command
        .args([
            "-vf",
            &filter,
            "-frames:v",
            &expected.max(1).to_string(),
            "-q:v",
            "3",
        ])
        .arg(&pattern)
        .status()
        .with_context(|| format!("启动 ffmpeg 失败：{}", config.ffmpeg))?;
    if !status.success() {
        bail!("远程切片 ffmpeg 退出码 {:?}", status.code());
    }
    let mut frames = Vec::new();
    for index in 1..=expected {
        if cancelled(cancel) {
            bail!("已取消");
        }
        let path = work.join(format!("f-{index:03}.jpg"));
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() < 500 {
            continue;
        }
        frames.push(ExtractedFrame {
            path,
            at_s: start_s + (index as u64 - 1) * interval_s,
        });
    }
    Ok(frames)
}

pub async fn build_with_meta(
    config: &WatchConfig,
    session: &Session,
    meta: Meta,
    interval_s: u64,
    progress: Option<ProgressFn<'_>>,
    cancel: Arc<AtomicBool>,
) -> Result<BuiltLab> {
    if !config.online {
        bail!("未登录");
    }
    if !ffmpeg_available(&config.ffmpeg) {
        bail!("ffmpeg 不在 PATH");
    }
    let interval = resolve_interval(meta.duration_s, interval_s)?;
    let expected = frame_count(meta.duration_s, interval);
    if expected == 0 {
        bail!("视频时长未知，无法抽帧");
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let lab_id = format!("{}-{stamp}", sanitise(&meta.bvid));
    let dir = std::env::temp_dir()
        .join("o11a1-fish-frames")
        .join(&lab_id);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建拼图目录失败：{}", dir.display()))?;
    let started = Instant::now();
    let result = build_inner(
        config, session, &meta, interval, expected, &lab_id, &dir, progress, &cancel,
    )
    .await;
    match result {
        Ok(mut built) => {
            built.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(built)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_inner(
    config: &WatchConfig,
    session: &Session,
    meta: &Meta,
    interval_s: u64,
    expected: usize,
    lab_id: &str,
    dir: &Path,
    progress: Option<ProgressFn<'_>>,
    cancel: &AtomicBool,
) -> Result<BuiltLab> {
    let movie = dir.join("movie.mp4");
    emit(
        progress,
        "download",
        "准备下载 720P 临时视频…".to_string(),
        3,
        0,
        expected,
    );
    let (video_bytes, track) = download_source(
        session,
        &meta.bvid,
        meta.cid,
        &movie,
        progress,
        cancel,
    )
    .await?;
    if cancelled(cancel) {
        bail!("已取消");
    }
    let (source_w, source_h, _) = track;
    let source = format!("720P · {source_w}×{source_h}");
    let layout = layout_for(source_w, source_h);
    crate::debug_log(&format!(
        "fish lab download bvid={} bytes={} source={} interval_s={} expected={}",
        meta.bvid, video_bytes, source, interval_s, expected
    ));

    emit(
        progress,
        "extract",
        format!("按 {interval_s}s 间隔抽帧（最多 {} 张）…", expected),
        68,
        0,
        expected,
    );
    let frames = extract(
        config,
        &movie,
        dir,
        interval_s,
        expected,
        layout,
        0,
        meta.duration_s,
        cancel,
    )?;
    let _ = std::fs::remove_file(&movie);
    if frames.is_empty() {
        bail!("没有抽出任何画面");
    }
    let actual = frames.len();

    emit(
        progress,
        "compose",
        format!("把 {} 张画面拼成 {} 张大图…", actual, actual.div_ceil(6)),
        84,
        0,
        actual.div_ceil(6),
    );
    let (sheets, image_bytes) = compose_sheets(&frames, dir, layout, progress, cancel)?;
    crate::debug_log(&format!(
        "fish lab sheets bvid={} frames={} sheets={} image_bytes={}",
        meta.bvid,
        actual,
        sheets.len(),
        image_bytes
    ));
    emit(
        progress,
        "done",
        format!("拼图完成：{actual} 张画面 / {} 张大图", sheets.len()),
        100,
        sheets.len(),
        sheets.len(),
    );
    Ok(BuiltLab {
        lab_id: lab_id.to_string(),
        video: LabVideo::from_meta(meta),
        interval_s,
        frame_count: actual,
        source,
        video_bytes,
        image_bytes,
        elapsed_ms: 0,
        dir: dir.to_path_buf(),
        sheets,
    })
}

async fn download_source(
    session: &Session,
    bvid: &str,
    cid: u64,
    path: &Path,
    progress: Option<ProgressFn<'_>>,
    cancel: &AtomicBool,
) -> Result<(u64, (u32, u32, i64))> {
    let info = session.playurl(bvid, cid).await?;
    let track = pick_video_track(&info)?;
    let feature = (track.width, track.height, track.id);
    let referer = format!("https://www.bilibili.com/video/{bvid}");
    let mut last = None;
    for url in track.urls.iter().chain(std::iter::once(&track.url)) {
        match stream_to_file(session, url, &referer, path, progress, cancel).await {
            Ok(bytes) if bytes > 10_000 => return Ok((bytes, feature)),
            Ok(bytes) => last = Some(anyhow!("视频流只有 {bytes} 字节")),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("没有可用的视频 CDN")))
}

/// Pick the 720P-class track used by the frame lab and the agent.
fn pick_video_track(info: &crate::bilibili::PlayInfo) -> Result<&crate::bilibili::VideoTrack> {
    if info.video_tracks.is_empty() {
        bail!("没有可下载的视频流");
    }
    let short_side = |track: &crate::bilibili::VideoTrack| track.width.min(track.height);
    let mut candidates: Vec<&crate::bilibili::VideoTrack> = info
        .video_tracks
        .iter()
        .filter(|track| short_side(track) <= SOURCE_SHORT_SIDE)
        .collect();
    if candidates.is_empty() {
        candidates = info.video_tracks.iter().collect();
    }
    candidates.sort_by(|a, b| {
        short_side(b)
            .cmp(&short_side(a))
            .then_with(|| a.bandwidth.cmp(&b.bandwidth))
    });
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("没有可用的视频流"))
}

/// One remote video source, used to cut a clip without downloading the whole
/// movie. `headers` is the CRLF block ffmpeg expects in `-headers`.
#[derive(Clone, Debug)]
pub struct RemoteSource {
    pub url: String,
    pub headers: String,
    pub width: u32,
    pub height: u32,
}

/// Resolve a playable 720P URL and the session headers for ffmpeg.
pub async fn remote_source(
    session: &Session,
    bvid: &str,
    cid: u64,
) -> Result<RemoteSource> {
    let info = session.playurl(bvid, cid).await?;
    let track = pick_video_track(&info)?;
    let url = track
        .urls
        .first()
        .cloned()
        .unwrap_or_else(|| track.url.clone());
    if url.trim().is_empty() {
        bail!("没有可用的视频 CDN");
    }
    let referer = format!("https://www.bilibili.com/video/{bvid}");
    Ok(RemoteSource {
        url,
        headers: session.ffmpeg_headers(&referer),
        width: track.width,
        height: track.height,
    })
}

async fn stream_to_file(
    session: &Session,
    url: &str,
    referer: &str,
    path: &Path,
    progress: Option<ProgressFn<'_>>,
    cancel: &AtomicBool,
) -> Result<u64> {
    let response = session.open_stream(url, referer).await?;
    if !response.status().is_success() {
        bail!("视频 HTTP {}", response.status().as_u16());
    }
    let total = response.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(path)?;
    let mut stream = response.bytes_stream();
    let mut bytes = 0u64;
    let mut last_emit = Instant::now();
    use std::io::Write;
    while let Some(chunk) = stream.next().await {
        if cancelled(cancel) {
            bail!("已取消");
        }
        let chunk = chunk?;
        file.write_all(&chunk)?;
        bytes += chunk.len() as u64;
        if last_emit.elapsed().as_millis() >= 400 {
            last_emit = Instant::now();
            let pct = if total > 0 {
                5 + ((bytes.min(total) as f64 / total as f64) * 60.0) as u32
            } else {
                12
            };
            emit(
                progress,
                "download",
                if total > 0 {
                    format!("下载 720P 临时视频 · {} / {}", human_bytes(bytes), human_bytes(total))
                } else {
                    format!("下载 720P 临时视频 · {}", human_bytes(bytes))
                },
                pct,
                bytes.min(u64::MAX) as usize,
                total as usize,
            );
        }
    }
    file.flush()?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Extraction and composition
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Layout {
    columns: u32,
    rows: u32,
    cell_w: u32,
    cell_h: u32,
    landscape: bool,
    square: bool,
}

fn layout_for(width: u32, height: u32) -> Layout {
    let aspect = width.max(1) as f32 / height.max(1) as f32;
    if aspect >= 1.2 {
        Layout {
            columns: SHEET_COLUMNS,
            rows: SHEET_ROWS,
            cell_w: 960,
            cell_h: 540,
            landscape: true,
            square: false,
        }
    } else if aspect <= 0.83 {
        Layout {
            columns: 3,
            rows: 2,
            cell_w: 540,
            cell_h: 960,
            landscape: false,
            square: false,
        }
    } else {
        Layout {
            columns: 2,
            rows: 3,
            cell_w: 620,
            cell_h: 620,
            landscape: false,
            square: true,
        }
    }
}

/// The agent's clip sheets pack up to 30 frames (6×5) into one 2048px-wide
/// image. Benchmarks against deepseek-flash show this keeps ~93% text
/// similarity; 48 tiles drops to ~88% and larger canvases do not increase the
/// model's fixed image-token budget.
fn clip_layout_for(width: u32, height: u32) -> Layout {
    let aspect = width.max(1) as f32 / height.max(1) as f32;
    if aspect >= 1.2 {
        Layout {
            columns: 6,
            rows: 5,
            cell_w: 328,
            cell_h: 184,
            landscape: true,
            square: false,
        }
    } else if aspect <= 0.83 {
        Layout {
            columns: 6,
            rows: 5,
            cell_w: 186,
            cell_h: 330,
            landscape: false,
            square: false,
        }
    } else {
        Layout {
            columns: 6,
            rows: 5,
            cell_w: 300,
            cell_h: 300,
            landscape: false,
            square: true,
        }
    }
}

struct ExtractedFrame {
    path: PathBuf,
    at_s: u64,
}

fn extract(
    config: &WatchConfig,
    movie: &Path,
    work: &Path,
    interval_s: u64,
    expected: usize,
    layout: Layout,
    start_s: u64,
    duration_s: u64,
    cancel: &AtomicBool,
) -> Result<Vec<ExtractedFrame>> {
    let scale = if layout.landscape {
        format!("scale={}:-2", layout.cell_w)
    } else if layout.square {
        format!("scale={}:-2", layout.cell_w)
    } else {
        format!("scale=-2:{}", layout.cell_h)
    };
    let filter = format!("fps=1/{interval_s},{scale}");
    let pattern = work.join("f-%03d.jpg");
    let mut command = proc::command(&config.ffmpeg);
    command.args(["-v", "error", "-y"]);
    if start_s > 0 {
        command.arg("-ss").arg(start_s.to_string());
    }
    command.arg("-i").arg(movie);
    command.arg("-t").arg(duration_s.max(1).to_string());
    let status = command
        .args([
            "-vf",
            &filter,
            "-frames:v",
            &expected.max(1).to_string(),
            "-q:v",
            "3",
        ])
        .arg(&pattern)
        .status()
        .with_context(|| format!("启动 ffmpeg 失败：{}", config.ffmpeg))?;
    if !status.success() {
        bail!("ffmpeg 抽帧退出码 {:?}", status.code());
    }
    let mut frames = Vec::new();
    for index in 1..=expected {
        if cancelled(cancel) {
            bail!("已取消");
        }
        let path = work.join(format!("f-{index:03}.jpg"));
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() < 500 {
            continue;
        }
        frames.push(ExtractedFrame {
            path,
            at_s: start_s + (index as u64 - 1) * interval_s,
        });
    }
    Ok(frames)
}

fn compose_sheets(
    frames: &[ExtractedFrame],
    work: &Path,
    layout: Layout,
    progress: Option<ProgressFn<'_>>,
    cancel: &AtomicBool,
) -> Result<(Vec<BuiltSheet>, u64)> {
    let background = Rgb([18, 19, 24]);
    let mut sheets = Vec::new();
    let mut image_bytes = 0u64;
    let per_sheet = (layout.columns * layout.rows) as usize;
    let total_sheets = frames.len().div_ceil(per_sheet);
    for (sheet_index, group) in frames.chunks(per_sheet).enumerate() {
        if cancelled(cancel) {
            bail!("已取消");
        }
        // A short last sheet uses only the rows it needs: two frames become a
        // 2×1 sheet instead of a 2×3 sheet with four empty cells. This keeps
        // the preview tight and gives the vision model fewer wasted pixels.
        let columns = layout.columns.min(group.len() as u32).max(1);
        let rows = (group.len() as u32).div_ceil(columns);
        let width = columns * layout.cell_w
            + columns.saturating_sub(1) * SHEET_GAP
            + SHEET_PADDING * 2;
        let height = rows * layout.cell_h
            + rows.saturating_sub(1) * SHEET_GAP
            + SHEET_PADDING * 2;
        let mut canvas: RgbImage = ImageBuffer::from_pixel(width, height, background);
        let mut times = Vec::with_capacity(group.len());
        for (slot, frame) in group.iter().enumerate() {
            let column = slot as u32 % columns;
            let row = slot as u32 / columns;
            let x = SHEET_PADDING + column * (layout.cell_w + SHEET_GAP);
            let y = SHEET_PADDING + row * (layout.cell_h + SHEET_GAP);
            let source = image::open(&frame.path)
                .with_context(|| format!("读取抽帧失败：{}", frame.path.display()))?
                .to_rgb8();
            let fitted = fit_into(&source, layout.cell_w, layout.cell_h);
            let mut cell: RgbImage =
                ImageBuffer::from_pixel(layout.cell_w, layout.cell_h, Rgb([10, 11, 14]));
            let ox = (layout.cell_w - fitted.width()) / 2;
            let oy = (layout.cell_h - fitted.height()) / 2;
            imageops::overlay(&mut cell, &fitted, ox as i64, oy as i64);
            let label_scale = if layout.cell_w < 400 { 3 } else { 4 };
            draw_label(&mut cell, 10, 10, &mmss(frame.at_s), label_scale);
            imageops::overlay(&mut canvas, &cell, x as i64, y as i64);
            times.push(frame.at_s);
        }
        let path = work.join(format!("sheet-{:02}.jpg", sheet_index + 1));
        save_jpeg(&canvas, &path, 80)?;
        let bytes = std::fs::metadata(&path)?.len();
        image_bytes += bytes;
        let from_s = times.first().copied().unwrap_or(0);
        let to_s = times.last().copied().unwrap_or(from_s);
        sheets.push(BuiltSheet {
            index: sheet_index + 1,
            from_s,
            to_s,
            times,
            path,
            width,
            height,
            columns,
            rows,
        });
        emit(
            progress,
            "compose",
            format!("拼接大图 {}/{}…", sheet_index + 1, total_sheets),
            84 + ((sheet_index + 1) as f32 / total_sheets as f32 * 14.0) as u32,
            sheet_index + 1,
            total_sheets,
        );
    }
    Ok((sheets, image_bytes))
}

fn fit_into(source: &RgbImage, max_w: u32, max_h: u32) -> RgbImage {
    let sw = source.width().max(1);
    let sh = source.height().max(1);
    let scale = (max_w as f32 / sw as f32)
        .min(max_h as f32 / sh as f32)
        .max(0.0001);
    let width = ((sw as f32 * scale).round() as u32).clamp(1, max_w);
    let height = ((sh as f32 * scale).round() as u32).clamp(1, max_h);
    if width == sw && height == sh {
        return source.clone();
    }
    imageops::resize(source, width, height, imageops::FilterType::Lanczos3)
}

fn save_jpeg(image: &RgbImage, path: &Path, quality: u8) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut writer = std::io::BufWriter::new(file);
    JpegEncoder::new_with_quality(&mut writer, quality).encode_image(image)?;
    Ok(())
}

/// A tiny 5×7 bitmap font for `0-9` and `:`. Timestamps stay legible in the
/// sheet without shipping a font file to the sidecar.
fn draw_label(cell: &mut RgbImage, x: u32, y: u32, text: &str, scale: u32) {
    let glyphs: Vec<[u8; 7]> = text.chars().filter_map(glyph).collect();
    if glyphs.is_empty() {
        return;
    }
    let pad = 6 * scale / 4;
    let glyph_w = 5 * scale;
    let gap = scale;
    let label_w = glyphs.len() as u32 * glyph_w + (glyphs.len() as u32 - 1) * gap + pad * 2;
    let label_h = 7 * scale + pad * 2;
    fill_rect(cell, x, y, label_w, label_h, Rgb([12, 13, 17]));
    let mut cursor = x + pad;
    for glyph in glyphs {
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    fill_rect(
                        cell,
                        cursor + column * scale,
                        y + pad + row as u32 * scale,
                        scale,
                        scale,
                        Rgb([244, 241, 234]),
                    );
                }
            }
        }
        cursor += glyph_w + gap;
    }
}

fn fill_rect(image: &mut RgbImage, x: u32, y: u32, width: u32, height: u32, color: Rgb<u8>) {
    for py in y..y.saturating_add(height).min(image.height()) {
        for px in x..x.saturating_add(width).min(image.width()) {
            image.put_pixel(px, py, color);
        }
    }
}

fn glyph(ch: char) -> Option<[u8; 7]> {
    let rows = match ch {
        '0' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        _ => return None,
    };
    Some(rows)
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct LabSheetView {
    pub index: usize,
    pub from_s: u64,
    pub to_s: u64,
    pub times: Vec<u64>,
    pub columns: u32,
    pub rows: u32,
    pub width: u32,
    pub height: u32,
    /// `data:image/jpeg;base64,…`
    pub image: String,
}

#[derive(Serialize)]
pub struct LabBuildView {
    pub lab_id: String,
    pub video: LabVideo,
    pub interval_s: u64,
    pub frame_count: usize,
    pub sheet_count: usize,
    pub source: String,
    pub video_bytes: u64,
    pub image_bytes: u64,
    pub elapsed_ms: u64,
    pub sheets: Vec<LabSheetView>,
}

pub fn build_view(built: &BuiltLab) -> Result<LabBuildView> {
    let mut sheets = Vec::with_capacity(built.sheets.len());
    for sheet in &built.sheets {
        let bytes = std::fs::read(&sheet.path)
            .with_context(|| format!("读取拼图失败：{}", sheet.path.display()))?;
        sheets.push(LabSheetView {
            index: sheet.index,
            from_s: sheet.from_s,
            to_s: sheet.to_s,
            times: sheet.times.clone(),
            columns: sheet.columns,
            rows: sheet.rows,
            width: sheet.width,
            height: sheet.height,
            image: format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
        });
    }
    Ok(LabBuildView {
        lab_id: built.lab_id.clone(),
        video: built.video.clone(),
        interval_s: built.interval_s,
        frame_count: built.frame_count,
        sheet_count: built.sheets.len(),
        source: built.source.clone(),
        video_bytes: built.video_bytes,
        image_bytes: built.image_bytes,
        elapsed_ms: built.elapsed_ms,
        sheets,
    })
}

pub fn observation_view(observed: &ObservedLab) -> Value {
    json!({
        "lab_id": observed.lab_id,
        "tokens": observed.tokens,
        "elapsed_ms": observed.elapsed_ms,
        "failed": observed.failed,
        "sheets": observed.sheets.iter().map(|sheet| json!({
            "index": sheet.index,
            "from_s": sheet.from_s,
            "to_s": sheet.to_s,
            "times": sheet.times,
            "tokens": sheet.tokens,
            "summary": sheet.observation.summary,
            "flags": sheet.observation.flags,
            "tiles": sheet.observation.tiles,
        })).collect::<Vec<_>>(),
    })
}

/// Compact, timestamped text for the agent's verdict prompt.
pub fn observations_text(observed: &ObservedLab) -> String {
    let mut out = String::new();
    for sheet in &observed.sheets {
        out.push_str(&format!(
            "第 {} 张拼图（{}–{}）：{}\n",
            sheet.index,
            mmss(sheet.from_s),
            mmss(sheet.to_s),
            sheet.observation.summary.trim()
        ));
        for tile in &sheet.observation.tiles {
            let at = if tile.at.trim().is_empty() {
                String::new()
            } else {
                format!("[{}] ", tile.at.trim())
            };
            let mut line = format!("- {at}");
            if !tile.scene.trim().is_empty() {
                line.push_str(&format!("{}；", tile.scene.trim()));
            }
            if !tile.action.trim().is_empty() {
                line.push_str(&format!("{}；", tile.action.trim()));
            }
            if !tile.onscreen_text.is_empty() {
                line.push_str(&format!("屏幕：{}；", tile.onscreen_text.join(" / ")));
            }
            if !tile.details.trim().is_empty() {
                line.push_str(tile.details.trim());
            }
            out.push_str(&clip(&line, 420));
            out.push('\n');
        }
        if !sheet.observation.flags.is_empty() {
            out.push_str(&format!("线索：{}\n", sheet.observation.flags.join(" / ")));
        }
    }
    clip(&out, 12_000)
}

// ---------------------------------------------------------------------------
// Vision
// ---------------------------------------------------------------------------

/// The engine has no direct `reqwest` dependency; this wrapper owns a client
/// with the same timeout the watch context uses.
pub async fn observe_standalone(
    config: &WatchConfig,
    built: &BuiltLab,
    focus: &str,
    progress: Option<ProgressFn<'_>>,
    cancel: Arc<AtomicBool>,
) -> Result<ObservedLab> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .unwrap_or_default();
    observe(config, &http, built, focus, progress, cancel).await
}

pub async fn observe(
    config: &WatchConfig,
    http: &reqwest::Client,
    built: &BuiltLab,
    focus: &str,
    progress: Option<ProgressFn<'_>>,
    cancel: Arc<AtomicBool>,
) -> Result<ObservedLab> {
    if config.deepseek.key.trim().is_empty() {
        bail!("未配置 DeepSeek key");
    }
    if built.sheets.is_empty() {
        bail!("没有可观察的拼图");
    }
    let started = Instant::now();
    let focus = focus.trim();
    let mut observed = Vec::with_capacity(built.sheets.len());
    let mut tokens = 0u64;
    let mut failed = 0usize;
    for (position, sheet) in built.sheets.iter().enumerate() {
        if cancelled(&cancel) {
            bail!("已取消");
        }
        emit(
            progress,
            "vision",
            format!(
                "AI 细看第 {}/{} 张拼图 · {}–{}",
                position + 1,
                built.sheets.len(),
                mmss(sheet.from_s),
                mmss(sheet.to_s)
            ),
            8 + ((position as f32 / built.sheets.len() as f32) * 90.0) as u32,
            position + 1,
            built.sheets.len(),
        );
        match ask_sheet(
            config,
            http,
            &built.video,
            built.interval_s,
            sheet,
            position + 1,
            built.sheets.len(),
            focus,
        )
        .await
        {
            Ok((observation, sheet_tokens)) => {
                tokens += sheet_tokens;
                observed.push(ObservedSheet {
                    index: sheet.index,
                    from_s: sheet.from_s,
                    to_s: sheet.to_s,
                    times: sheet.times.clone(),
                    observation,
                    tokens: sheet_tokens,
                });
            }
            Err(error) => {
                failed += 1;
                crate::debug_log(&format!(
                    "fish lab vision sheet={} failed: {error:#}",
                    sheet.index
                ));
            }
        }
    }
    if observed.is_empty() {
        bail!("{failed}/{} 张拼图观察全部失败", built.sheets.len());
    }
    emit(
        progress,
        "done",
        format!("AI 已细看 {}/{} 张拼图", observed.len(), built.sheets.len()),
        100,
        observed.len(),
        built.sheets.len(),
    );
    Ok(ObservedLab {
        lab_id: built.lab_id.clone(),
        sheets: observed,
        tokens,
        elapsed_ms: started.elapsed().as_millis() as u64,
        failed,
    })
}

#[allow(clippy::too_many_arguments)]
async fn ask_sheet(
    config: &WatchConfig,
    http: &reqwest::Client,
    video: &LabVideo,
    interval_s: u64,
    sheet: &BuiltSheet,
    index: usize,
    total: usize,
    focus: &str,
) -> Result<(SheetObservation, u64)> {
    let bytes = std::fs::read(&sheet.path)
        .with_context(|| format!("读取拼图失败：{}", sheet.path.display()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let times = sheet
        .times
        .iter()
        .map(|seconds| mmss(*seconds))
        .collect::<Vec<_>>()
        .join("、");
    let focus = if focus.is_empty() {
        "画面事实、屏幕文字、图表数据、广告/恰饭/重复画面线索"
    } else {
        focus
    };
    let prompt = format!(
        "视频《{}》（UP：{}），按每 {interval_s} 秒抽帧。\n\
         这是第 {index}/{total} 张拼图，{columns} 列 × {rows} 行，共 {} 格；阅读顺序从左到右、从上到下。\n\
         每格对应时间依次是：{times}。左上角时间码是抽帧标记，不要当作视频内容。\n\
         观察重点：{focus}。\n\
         请逐格细看，尽量逐字抄录屏幕文字、字幕、菜单、数字、价格、代码、图表标签；描述人物、物体、场景、动作、UI 状态和明显的剪辑/广告痕迹。\n\
         只写画面能确认的事实，看不清就写“看不清”，不要推断剧情，不要评价好不好看。\n\
         严格输出 JSON：{{\"tiles\":[{{\"at\":\"0:00\",\"scene\":\"\",\"action\":\"\",\"onscreen_text\":[\"\"],\"details\":\"\"}}],\"summary\":\"\",\"flags\":[\"\"]}}\n\
         必须为每一格输出一条 tile，顺序和时间必须与上面一致。",
        video.title,
        video.up_name,
        sheet.times.len(),
        columns = sheet.columns,
        rows = sheet.rows,
    );
    let body = json!({
        "model": VISION_MODEL,
        "messages": [
            { "role": "system", "content": SYSTEM_OBSERVER },
            { "role": "user", "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": {
                    "url": format!("data:image/jpeg;base64,{encoded}"),
                    "detail": VISION_DETAIL,
                }},
            ]},
        ],
        "thinking": { "type": "disabled" },
        "temperature": 0.1,
        "max_tokens": VISION_MAX_TOKENS,
        "stream": false,
    });
    let response = http
        .post(VISION_URL)
        .bearer_auth(config.deepseek.key.trim())
        .json(&body)
        .send()
        .await
        .context("请求 DeepSeek 画面观察失败")?;
    let status = response.status();
    let text = response.text().await.context("读取画面观察响应失败")?;
    if !status.is_success() {
        bail!("画面观察 HTTP {}：{}", status.as_u16(), clip(&text, 300));
    }
    let value: Value = serde_json::from_str(&text).map_err(|err| anyhow!("画面观察响应解析失败：{err}"))?;
    let raw = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        bail!("画面观察返回了空内容");
    }
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    let mut observation = parse_observation(&raw, &sheet.times);
    if observation.summary.trim().is_empty() {
        observation.summary = clip(&collapse(&raw), 360);
    }
    crate::debug_log(&format!(
        "fish lab vision sheet={}/{} tokens={} tiles={} usage={}",
        index,
        total,
        tokens,
        observation.tiles.len(),
        clip(&usage.to_string(), 300)
    ));
    Ok((observation, tokens))
}

const VISION_URL: &str = "https://api.deepseek.com/chat/completions";
const VISION_MODEL: &str = "deepseek-flash";
const SYSTEM_OBSERVER: &str = "你是视频画面审片员。只描述截图拼图中能确认的事实，尽量读清屏幕上的文字和数字；不编造、不推断剧情、不评价好坏。严格输出 JSON 对象。";

fn parse_observation(raw: &str, times: &[u64]) -> SheetObservation {
    let parsed = extract_json(raw)
        .and_then(|text| serde_json::from_str::<SheetObservation>(text).ok());
    let mut observation = parsed.unwrap_or_else(|| SheetObservation {
        summary: clip(&collapse(raw), 360),
        ..SheetObservation::default()
    });
    for (index, tile) in observation.tiles.iter_mut().enumerate() {
        if tile.at.trim().is_empty() {
            if let Some(seconds) = times.get(index) {
                tile.at = mmss(*seconds);
            }
        }
    }
    if observation.flags.len() > 8 {
        observation.flags.truncate(8);
    }
    observation
}

fn extract_json(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

pub fn mmss(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let rest = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
    }
}

fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.2} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{:.0} KB", value / 1024.0)
    }
}

fn sanitise(bvid: &str) -> String {
    bvid.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_planning_rejects_too_many_frames() {
        assert_eq!(resolve_interval(60, 0).unwrap(), 8);
        assert_eq!(resolve_interval(600, 30).unwrap(), 30);
        assert_eq!(frame_count(600, 30), 21);
        assert!(resolve_interval(3_600, 10).is_err());
        assert_eq!(resolve_interval(60, 1).unwrap(), 2);
        assert_eq!(frame_count(600, 2), 60);
    }

    #[test]
    fn auto_interval_scales_with_length() {
        assert_eq!(auto_interval(1), 8);
        assert_eq!(auto_interval(240), 8);
        assert_eq!(auto_interval(600), 20);
        assert_eq!(auto_interval(1_800), 60);
    }

    #[test]
    fn clip_planning_packs_thirty_frames_into_2048px() {
        assert_eq!(clip_frame_count(60, 1), 60);
        assert_eq!(clip_frame_count(60, 5), 12);
        assert_eq!(clip_frame_count(5, 5), 1);
        let layout = clip_layout_for(1920, 1080);
        assert_eq!((layout.columns, layout.rows), (6, 5));
        let sheet_width =
            layout.columns * layout.cell_w + (layout.columns - 1) * SHEET_GAP + SHEET_PADDING * 2;
        assert!(sheet_width <= 2048, "sheet width {sheet_width}");
    }

    #[test]
    fn layouts_stay_under_the_vision_image_budget() {
        for (width, height) in [(1280, 720), (720, 1280), (800, 800)] {
            let layout = layout_for(width, height);
            let sheet_w = layout.columns * layout.cell_w
                + layout.columns.saturating_sub(1) * SHEET_GAP
                + SHEET_PADDING * 2;
            let sheet_h = layout.rows * layout.cell_h
                + layout.rows.saturating_sub(1) * SHEET_GAP
                + SHEET_PADDING * 2;
            assert!(sheet_w <= 2048 && sheet_h <= 2048, "{width}x{height}: {sheet_w}x{sheet_h}");
        }
    }

    #[test]
    fn timestamp_font_has_all_digits() {
        for ch in "0123456789:".chars() {
            assert!(glyph(ch).is_some(), "missing {ch}");
        }
    }

    #[test]
    fn vision_json_fills_missing_times() {
        let observation = parse_observation(
            r#"{"tiles":[{"scene":"桌面","onscreen_text":["价格 29"]}],"summary":"一张桌面","flags":["广告"]}"#,
            &[0, 30],
        );
        assert_eq!(observation.tiles.len(), 1);
        assert_eq!(observation.tiles[0].at, "0:00");
        assert_eq!(observation.tiles[0].onscreen_text, vec!["价格 29"]);
        assert_eq!(observation.summary, "一张桌面");
    }
}
