//! 离线下载 (best-effort, personal use).
//!
//! Resolves a video's DASH video + audio streams, downloads each with a pool of
//! concurrent HTTP range requests (the transport ffmpeg's single-connection
//! HTTP reader cannot use), then muxes the two local files with `ffmpeg -c copy`.
//! Both media URLs are referer-gated, so every fetch uses the session's own
//! pooled client, which already carries the `Referer`/`User-Agent` and cookies
//! the player uses. A `BILI_DOWNLOAD_LEGACY=1` diagnostic switches back to the
//! original one-ffmpeg-over-network path; `BILI_DOWNLOAD_BVID` reroutes a job to
//! a fixed video. Runs on a background thread; the UI thread only sees progress
//! strings pushed through `slint::invoke_from_event_loop`.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::future::join_all;
use futures::future::{select, Either};
use futures::pin_mut;
use reqwest::header::{HeaderMap, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;

use crate::{bilibili, AppWindow};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 480P, the sensible default quality for a personal-use download.
const DOWNLOAD_QN: i64 = 32;
/// Character cap for the filename stem (keeps the OS path limit comfortable).
const STEM_MAX_CHARS: usize = 80;
/// Concurrent range requests issued per media stream.
const DOWNLOAD_CONNECTIONS: usize = 8;
/// Ceiling on concurrent range requests for a very large stream.
const MAX_CONNECTIONS: usize = 16;
/// Keep each range request small enough to finish well inside the 25s total
/// request timeout even on a slow (~1 MB/s) CDN edge, by widening the pool.
const MAX_SEGMENT_BYTES: u64 = 6 * 1024 * 1024;
/// Attempts per segment, rotating through the mirror list, before giving up.
const SEGMENT_ATTEMPTS: usize = 3;
/// How often the transfer polls the cancel flag (also bounds cancel latency).
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Shared cancel flag and child handle for the in-flight download.
pub struct DownloadControl {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl DownloadControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            child: Mutex::new(None),
        })
    }

    /// Request cancellation and kill the running ffmpeg (idempotent).
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Why a download did not finish successfully.
enum DownloadError {
    Cancelled,
    Failed(String),
}

/// Which transport to use for the media bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    /// Parallel HTTP range requests + local mux (the default).
    Parallel,
    /// The original single ffmpeg process reading both URLs over the network.
    Legacy,
}

fn default_transport() -> Transport {
    match std::env::var("BILI_DOWNLOAD_LEGACY").as_deref() {
        Ok("1") => Transport::Legacy,
        _ => Transport::Parallel,
    }
}

/// A completed job: the muxed file and the title used for its name/notification.
struct Finished {
    out_path: PathBuf,
    title: String,
}

/// Fetch the play addresses then mux to `.mp4` on a private thread.
pub fn spawn(
    session: Arc<bilibili::Session>,
    bvid: String,
    cid: u64,
    title: String,
    window: slint::Weak<AppWindow>,
    control: Arc<DownloadControl>,
) {
    std::thread::spawn(move || run(session, bvid, cid, title, window, control));
}

fn run(
    session: Arc<bilibili::Session>,
    bvid: String,
    cid: u64,
    title: String,
    window: slint::Weak<AppWindow>,
    control: Arc<DownloadControl>,
) {
    set_busy(&window, true);
    set_status(Some(&window), "正在解析下载地址…");

    match execute(&session, bvid, cid, title, None, Some(&window), &control, default_transport()) {
        Ok(finished) => {
            let size = std::fs::metadata(&finished.out_path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            set_busy(&window, false);
            set_status(
                Some(&window),
                &format!(
                    "下载完成：{}（{}）",
                    finished.out_path.display(),
                    format_size(size)
                ),
            );
            let saved = finished.out_path.display().to_string();
            crate::tray::notify("下载完成", &format!("{}\n{saved}", finished.title));
        }
        Err(DownloadError::Cancelled) => finish_cancelled(&window),
        Err(DownloadError::Failed(message)) => finish_error(&window, &control, &message),
    }
}

/// Resolve the media URLs and write `out_path` (or a path derived from the
/// title when `forced_out` is `None`). `window` is `None` for the headless
/// measurement harness, which suppresses UI status updates.
#[allow(clippy::too_many_arguments)]
fn execute(
    session: &bilibili::Session,
    mut bvid: String,
    mut cid: u64,
    mut title: String,
    forced_out: Option<&Path>,
    window: Option<&slint::Weak<AppWindow>>,
    control: &Arc<DownloadControl>,
    transport: Transport,
) -> Result<Finished, DownloadError> {
    let Some(runtime) = bilibili::shared_runtime() else {
        return Err(DownloadError::Failed("初始化下载运行时失败".into()));
    };

    // Diagnostic: reroute the job to a fixed video, resolving its cid.
    if let Some(forced) = download_bvid_override() {
        if forced != bvid {
            match runtime.block_on(session.video_view(&forced)) {
                Ok(detail) => {
                    if !detail.title.is_empty() {
                        title = detail.title;
                    }
                    cid = detail.cid.max(0) as u64;
                    bvid = forced;
                }
                Err(err) => {
                    crate::debug_log(&format!("download override bvid={forced} err={err:#}"));
                }
            }
        }
    }

    let prep_started = Instant::now();
    let info = runtime
        .block_on(session.playurl(&bvid, cid))
        .map_err(|err| DownloadError::Failed(format!("获取播放地址失败：{err}")))?;
    crate::debug_log(&format!(
        "download playurl bvid={bvid} cid={cid} ms={} video_tracks={} audio_tracks={} duration={}",
        prep_started.elapsed().as_millis(),
        info.video_tracks.len(),
        info.audio_tracks.len(),
        info.duration
    ));
    if control.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    // Prefer the 480P track; otherwise fall back to the player's default quality.
    let preferred_qn = if info.video_tracks.iter().any(|track| track.id == DOWNLOAD_QN) {
        DOWNLOAD_QN
    } else {
        info.default_qn
    };
    let Some(track) = info.video_track_for(preferred_qn, 0) else {
        return Err(DownloadError::Failed("未找到可用视频流".into()));
    };
    let video_urls = if track.urls.is_empty() {
        vec![track.url.clone()]
    } else {
        track.urls.clone()
    };
    let label = bilibili::quality_label(track.id);
    let audio_urls = info.audio_urls_for(0);
    let duration_ms = info.duration.saturating_mul(1000).max(1);

    let out_path = match forced_out {
        Some(path) => path.to_path_buf(),
        None => output_path(&title, &bvid)
            .map_err(|err| DownloadError::Failed(format!("无法创建下载目录：{err}")))?,
    };

    set_status(window, &format!("下载中（{label}）0%"));

    let result = match transport {
        Transport::Parallel => download_parallel(
            session,
            &runtime,
            &video_urls,
            &audio_urls,
            &out_path,
            control,
            window,
            &label,
        ),
        Transport::Legacy => {
            let started = Instant::now();
            let result = run_ffmpeg(
                video_urls.first().map(String::as_str).unwrap_or_default(),
                audio_urls.first().map(String::as_str),
                &out_path,
                control,
                window,
                duration_ms,
                &label,
            );
            crate::debug_log(&format!(
                "download legacy ms={} ok={} out_bytes={}",
                started.elapsed().as_millis(),
                result.is_ok(),
                std::fs::metadata(&out_path).map(|meta| meta.len()).unwrap_or(0)
            ));
            result
        }
    };

    match result {
        Ok(()) => Ok(Finished { out_path, title }),
        Err(err) => {
            let _ = std::fs::remove_file(&out_path);
            Err(err)
        }
    }
}

/// A per-stream range job once its size and range support are known.
struct StreamJob<'a> {
    urls: &'a [String],
    path: &'a Path,
    len: u64,
    ranged: bool,
}

/// Download both streams to a temp dir, then mux them locally. The temp dir
/// (and its partial files) is removed on every exit path via [`TempDir`].
#[allow(clippy::too_many_arguments)]
fn download_parallel(
    session: &bilibili::Session,
    runtime: &tokio::runtime::Runtime,
    video_urls: &[String],
    audio_urls: &[String],
    out_path: &Path,
    control: &Arc<DownloadControl>,
    window: Option<&slint::Weak<AppWindow>>,
    label: &str,
) -> Result<(), DownloadError> {
    let temps = TempDir::create(out_path)
        .map_err(|err| DownloadError::Failed(format!("无法创建临时目录：{err}")))?;
    let video_tmp = temps.file("video.part");
    let audio_tmp = temps.file("audio.part");
    let has_audio = !audio_urls.is_empty();

    let client = session.raw_client();
    let progress = Progress::new();

    let transfer_started = Instant::now();
    let transfer = runtime.block_on(async {
        let work = download_streams(
            client, video_urls, audio_urls, &video_tmp, &audio_tmp, control, window, label,
            &progress,
        );
        let cancel = wait_cancel(control);
        pin_mut!(work, cancel);
        match select(cancel, work).await {
            Either::Left((_, _work)) => Err(DownloadError::Cancelled),
            Either::Right((result, _cancel)) => result,
        }
    });
    let transfer_ms = transfer_started.elapsed().as_millis();

    let video_bytes = std::fs::metadata(&video_tmp).map(|meta| meta.len()).unwrap_or(0);
    let audio_bytes = if has_audio {
        std::fs::metadata(&audio_tmp).map(|meta| meta.len()).unwrap_or(0)
    } else {
        0
    };
    let transfer_mbps = mbps(video_bytes + audio_bytes, transfer_ms);

    if let Err(err) = transfer {
        crate::debug_log(&format!(
            "download transfer ms={transfer_ms} mbps={transfer_mbps:.2} video_bytes={video_bytes} \
             audio_bytes={audio_bytes} cancelled={}",
            matches!(err, DownloadError::Cancelled)
        ));
        return Err(err);
    }
    if control.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    set_status(window, "合并中…");
    let mux_started = Instant::now();
    let mux = run_mux(
        &video_tmp,
        if has_audio { Some(&audio_tmp) } else { None },
        out_path,
        control,
    );
    let mux_ms = mux_started.elapsed().as_millis();
    let out_bytes = std::fs::metadata(out_path).map(|meta| meta.len()).unwrap_or(0);
    crate::debug_log(&format!(
        "download parallel transfer_ms={transfer_ms} transfer_mbps={transfer_mbps:.2} \
         video_bytes={video_bytes} audio_bytes={audio_bytes} mux_ms={mux_ms} out_bytes={out_bytes} \
         faststart={}",
        use_faststart()
    ));
    mux
}

/// Probe both streams for size and range support, then download them one after
/// the other so the connection count stays at [`DOWNLOAD_CONNECTIONS`].
#[allow(clippy::too_many_arguments)]
async fn download_streams(
    client: &reqwest::Client,
    video_urls: &[String],
    audio_urls: &[String],
    video_path: &Path,
    audio_path: &Path,
    control: &DownloadControl,
    window: Option<&slint::Weak<AppWindow>>,
    label: &str,
    progress: &Progress,
) -> Result<(), DownloadError> {
    let (video_len, video_ranged) = probe_stream(client, video_urls).await?;
    let audio_probe = if audio_urls.is_empty() {
        None
    } else {
        Some(probe_stream(client, audio_urls).await?)
    };
    let audio_len = audio_probe.as_ref().map(|(len, _)| *len).unwrap_or(0);
    progress.total.store(video_len + audio_len, Ordering::Relaxed);
    report_progress(progress, window, label);

    let video_job = StreamJob {
        urls: video_urls,
        path: video_path,
        len: video_len,
        ranged: video_ranged,
    };
    download_one(client, &video_job, control, window, label, progress).await?;

    if let Some((audio_len, audio_ranged)) = audio_probe {
        let audio_job = StreamJob {
            urls: audio_urls,
            path: audio_path,
            len: audio_len,
            ranged: audio_ranged,
        };
        download_one(client, &audio_job, control, window, label, progress).await?;
    }
    Ok(())
}

/// One stream: preallocate, then range-download in parallel (or stream
/// sequentially when the server refuses ranges).
async fn download_one(
    client: &reqwest::Client,
    job: &StreamJob<'_>,
    control: &DownloadControl,
    window: Option<&slint::Weak<AppWindow>>,
    label: &str,
    progress: &Progress,
) -> Result<(), DownloadError> {
    preallocate(job.path, job.len)?;
    if job.len == 0 {
        return Ok(());
    }
    if !job.ranged {
        return download_sequential(client, job, control, window, label, progress).await;
    }

    let connections = connection_count(job.len);
    let segment = job.len.div_ceil(connections as u64);
    crate::debug_log(&format!(
        "download segment bytes={} connections={} segment_bytes={}",
        job.len, connections, segment
    ));

    let mut futures = Vec::with_capacity(connections);
    for index in 0..connections {
        let start = index as u64 * segment;
        if start >= job.len {
            break;
        }
        let end = ((index as u64 + 1) * segment - 1).min(job.len - 1);
        futures.push(fetch_segment(
            client,
            job.urls,
            start,
            end,
            job.path,
            control,
            window,
            label,
            progress,
        ));
    }
    for result in join_all(futures).await {
        result?;
    }
    Ok(())
}

fn connection_count(len: u64) -> usize {
    let requested = connection_setting();
    let by_size = len.div_ceil(MAX_SEGMENT_BYTES).max(1) as usize;
    by_size.clamp(requested, MAX_CONNECTIONS.max(requested))
}

/// Default pool size, overridable with `BILI_DOWNLOAD_CONNECTIONS` (clamped
/// `1..=32`) for measurement.
fn connection_setting() -> usize {
    std::env::var("BILI_DOWNLOAD_CONNECTIONS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|count| count.clamp(1, 32))
        .unwrap_or(DOWNLOAD_CONNECTIONS)
}

/// One `bytes=0-0` request: `(total_len, supports_ranges)`. Falls back to
/// `Content-Length` when the server ignores the range header.
async fn probe_stream(
    client: &reqwest::Client,
    urls: &[String],
) -> Result<(u64, bool), DownloadError> {
    let mut last = String::from("没有可用的媒体地址");
    for url in urls {
        match client.get(url).header(RANGE, "bytes=0-0").send().await {
            Ok(response) => {
                let status = response.status();
                if status == StatusCode::PARTIAL_CONTENT {
                    if let Some(total) = content_range_total(response.headers()) {
                        return Ok((total, true));
                    }
                }
                if status.is_success() {
                    if let Some(len) = response.content_length() {
                        return Ok((len, false));
                    }
                }
                last = format!("HTTP {}", status.as_u16());
            }
            Err(err) => last = err.to_string(),
        }
    }
    Err(DownloadError::Failed(format!("探测媒体大小失败：{last}")))
}

fn content_range_total(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// Download one segment, retrying against successive mirrors on failure.
#[allow(clippy::too_many_arguments)]
async fn fetch_segment(
    client: &reqwest::Client,
    urls: &[String],
    start: u64,
    end: u64,
    path: &Path,
    control: &DownloadControl,
    window: Option<&slint::Weak<AppWindow>>,
    label: &str,
    progress: &Progress,
) -> Result<(), DownloadError> {
    let mut last = String::from("无可用地址");
    for attempt in 0..SEGMENT_ATTEMPTS {
        if control.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let url = &urls[attempt % urls.len()];
        match stream_segment(client, url, start, end, path, control, window, label, progress).await {
            Ok(()) => return Ok(()),
            Err(SegmentError::Cancelled) => return Err(DownloadError::Cancelled),
            Err(SegmentError::Failed(message)) => {
                crate::debug_log(&format!(
                    "download segment retry range={start}-{end} attempt={} err={message}",
                    attempt + 1
                ));
                last = message;
            }
        }
    }
    Err(DownloadError::Failed(format!(
        "分段 {start}-{end} 下载失败：{last}"
    )))
}

enum SegmentError {
    Cancelled,
    Failed(String),
}

/// Stream one range response straight to its offset in the preallocated file,
/// counting bytes into `progress`. On failure the partial byte count is rolled
/// back so a retry does not double-count.
#[allow(clippy::too_many_arguments)]
async fn stream_segment(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    path: &Path,
    control: &DownloadControl,
    window: Option<&slint::Weak<AppWindow>>,
    label: &str,
    progress: &Progress,
) -> Result<(), SegmentError> {
    let expected = end - start + 1;
    let response = client
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|err| SegmentError::Failed(err.to_string()))?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(SegmentError::Failed(format!(
            "HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|err| SegmentError::Failed(err.to_string()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|err| SegmentError::Failed(err.to_string()))?;

    let mut written = 0u64;
    let mut response = response;
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => {
                progress.done.fetch_sub(written, Ordering::Relaxed);
                return Err(SegmentError::Failed(err.to_string()));
            }
        };
        if control.is_cancelled() {
            progress.done.fetch_sub(written, Ordering::Relaxed);
            return Err(SegmentError::Cancelled);
        }
        if let Err(err) = file.write_all(&chunk) {
            progress.done.fetch_sub(written, Ordering::Relaxed);
            return Err(SegmentError::Failed(err.to_string()));
        }
        written += chunk.len() as u64;
        progress.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        report_progress(progress, window, label);
    }
    if written != expected {
        progress.done.fetch_sub(written, Ordering::Relaxed);
        return Err(SegmentError::Failed(format!(
            "字节数不符：期望 {expected}，实际 {written}"
        )));
    }
    Ok(())
}

/// Single-connection fallback for a server that ignores `Range`.
async fn download_sequential(
    client: &reqwest::Client,
    job: &StreamJob<'_>,
    control: &DownloadControl,
    window: Option<&slint::Weak<AppWindow>>,
    label: &str,
    progress: &Progress,
) -> Result<(), DownloadError> {
    let url = job
        .urls
        .first()
        .ok_or_else(|| DownloadError::Failed("没有可用的媒体地址".into()))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|err| DownloadError::Failed(format!("下载失败：{err}")))?;
    if !response.status().is_success() {
        return Err(DownloadError::Failed(format!(
            "下载失败：HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(job.path)
        .map_err(|err| DownloadError::Failed(format!("无法写入临时文件：{err}")))?;

    let mut written = 0u64;
    loop {
        if control.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => return Err(DownloadError::Failed(format!("下载失败：{err}"))),
        };
        file.write_all(&chunk)
            .map_err(|err| DownloadError::Failed(format!("写入失败：{err}")))?;
        written += chunk.len() as u64;
        progress.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        report_progress(progress, window, label);
    }
    if written != job.len {
        return Err(DownloadError::Failed(format!(
            "字节数不符：期望 {}，实际 {written}",
            job.len
        )));
    }
    Ok(())
}

/// Byte counters shared by every in-flight segment, for one accurate percentage.
struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    last_percent: AtomicU64,
}

impl Progress {
    fn new() -> Self {
        Self {
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            last_percent: AtomicU64::new(u64::MAX),
        }
    }
}

fn report_progress(progress: &Progress, window: Option<&slint::Weak<AppWindow>>, label: &str) {
    if window.is_none() {
        return;
    }
    let total = progress.total.load(Ordering::Relaxed);
    if total == 0 {
        return;
    }
    let done = progress.done.load(Ordering::Relaxed).min(total);
    let percent = done.saturating_mul(100) / total;
    let last = progress.last_percent.load(Ordering::Relaxed);
    if percent != last
        && progress
            .last_percent
            .compare_exchange(last, percent, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        set_status(window, &format!("下载中（{label}）{percent}%"));
    }
}

fn preallocate(path: &Path, len: u64) -> Result<(), DownloadError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|err| DownloadError::Failed(format!("无法创建临时文件：{err}")))?;
    file.set_len(len)
        .map_err(|err| DownloadError::Failed(format!("无法预分配临时文件：{err}")))?;
    Ok(())
}

/// Resolves as soon as cancellation is requested; raced against the transfer so
/// in-flight requests are dropped (aborted) promptly.
async fn wait_cancel(control: &DownloadControl) {
    while !control.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

/// A scratch directory beside the output that removes itself (and any partial
/// files) on drop, covering every success, failure and cancellation path.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(base_out: &Path) -> std::io::Result<Self> {
        let parent = base_out
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(download_dir);
        std::fs::create_dir_all(&parent)?;
        let path = parent.join(format!(".bili-dl-{}", unique_suffix()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn mbps(bytes: u64, ms: u128) -> f64 {
    if ms == 0 {
        return 0.0;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / (ms as f64 / 1000.0)
}

/// `+faststart` rewrites the finished file to move the moov atom to the front
/// (enables progressive playback). `BILI_DOWNLOAD_NO_FASTSTART=1` disables it.
fn use_faststart() -> bool {
    !matches!(
        std::env::var("BILI_DOWNLOAD_NO_FASTSTART").as_deref(),
        Ok("1")
    )
}

/// `BILI_DOWNLOAD_BVID` reroutes a job to a fixed video (diagnostic/measurement).
fn download_bvid_override() -> Option<String> {
    std::env::var("BILI_DOWNLOAD_BVID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Mux two local temp files with `ffmpeg -c copy` (never re-encodes).
fn run_mux(
    video: &Path,
    audio: Option<&Path>,
    out: &Path,
    control: &Arc<DownloadControl>,
) -> Result<(), DownloadError> {
    let mut command = base_command("ffmpeg.exe");
    command
        .arg("-hide_banner")
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(video);
    if let Some(audio) = audio {
        command.arg("-i").arg(audio);
    }
    command.arg("-map").arg("0:v:0");
    if audio.is_some() {
        command.arg("-map").arg("1:a:0");
    }
    command.arg("-c").arg("copy");
    if use_faststart() {
        command.arg("-movflags").arg("+faststart");
    }
    command
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Err(DownloadError::Failed(format!("无法启动 ffmpeg：{err}"))),
    };
    crate::job::assign(&child);
    let stderr = child.stderr.take();
    if let Ok(mut guard) = control.child.lock() {
        *guard = Some(child);
    }

    let stderr_handle = stderr.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text.trim().to_string()
        })
    });

    let exit = wait_for_exit(control);
    let stderr_text = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    if control.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    match exit {
        Some(status) if status.success() => Ok(()),
        _ => {
            let detail = stderr_text.chars().take(200).collect::<String>();
            Err(DownloadError::Failed(if detail.is_empty() {
                "合并失败".to_string()
            } else {
                format!("合并失败：{detail}")
            }))
        }
    }
}

/// Legacy transport: one ffmpeg process reading both DASH URLs over the network
/// and copying them into `out_path`, parsing `-progress pipe:1` for a percentage.
fn run_ffmpeg(
    video_url: &str,
    audio_url: Option<&str>,
    out_path: &Path,
    control: &Arc<DownloadControl>,
    window: Option<&slint::Weak<AppWindow>>,
    duration_ms: u64,
    label: &str,
) -> Result<(), DownloadError> {
    let mut command = base_command("ffmpeg.exe");
    command
        .arg("-hide_banner")
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats");

    let headers = media_headers();
    command.arg("-headers").arg(&headers).arg("-i").arg(video_url);
    if let Some(audio_url) = audio_url {
        command
            .arg("-headers")
            .arg(&headers)
            .arg("-i")
            .arg(audio_url);
        command.arg("-map").arg("0:v:0").arg("-map").arg("1:a:0");
    }
    command
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(out_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Err(DownloadError::Failed(format!("无法启动 ffmpeg：{err}"))),
    };
    crate::job::assign(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Ok(mut guard) = control.child.lock() {
        *guard = Some(child);
    }

    let stderr_handle = stderr.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text.trim().to_string()
        })
    });

    let mut cancelled = false;
    if let Some(stdout) = stdout {
        let mut last_percent = u64::MAX;
        for line in BufReader::new(stdout).lines() {
            if control.is_cancelled() {
                cancelled = true;
                break;
            }
            let Ok(line) = line else { break };
            let Some(value) = line.strip_prefix("out_time_us=") else {
                continue;
            };
            let Ok(us) = value.trim().parse::<u64>() else {
                continue;
            };
            let percent = (us.saturating_mul(100) / duration_ms).min(100);
            if percent != last_percent {
                last_percent = percent;
                set_status(window, &format!("下载中（{label}）{percent}%"));
            }
        }
    }

    let exit = wait_for_exit(control);
    let stderr_text = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();

    if cancelled || control.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }
    match exit {
        Some(status) if status.success() => Ok(()),
        _ => {
            let detail = stderr_text.chars().take(200).collect::<String>();
            Err(DownloadError::Failed(if detail.is_empty() {
                "ffmpeg 下载失败".to_string()
            } else {
                format!("ffmpeg 下载失败：{detail}")
            }))
        }
    }
}

/// Poll the child to completion without holding its lock (so `cancel` can kill
/// it) — stdout is already at EOF, so ffmpeg is about to exit.
fn wait_for_exit(control: &Arc<DownloadControl>) -> Option<ExitStatus> {
    loop {
        let status = control.child.lock().ok().and_then(|mut guard| {
            guard
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
        });
        if let Some(status) = status {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The header block attached to each legacy ffmpeg input. Same UA as `bilibili::UA`.
fn media_headers() -> String {
    format!(
        "Referer: https://www.bilibili.com/\r\nUser-Agent: {}\r\n",
        bilibili::UA
    )
}

/// `%USERPROFILE%\Videos\bilibili-downloads\` (falling back to the CWD).
fn download_dir() -> PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.trim().is_empty() {
            return PathBuf::from(profile)
                .join("Videos")
                .join("bilibili-downloads");
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Downloads")
        .join("bilibili-downloads")
}

fn output_path(title: &str, bvid: &str) -> std::io::Result<PathBuf> {
    let dir = download_dir();
    std::fs::create_dir_all(&dir)?;
    let stem = sanitize(title);
    let stem = if stem.is_empty() {
        bvid.to_string()
    } else {
        stem
    };
    Ok(dir.join(format!("{stem}_{bvid}.mp4")))
}

/// Strip characters Windows rejects in filenames and collapse whitespace.
fn sanitize(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for ch in title.chars() {
        match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|c: char| c == '.' || c == ' ')
        .chars()
        .take(STEM_MAX_CHARS)
        .collect()
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

fn finish_error(window: &slint::Weak<AppWindow>, control: &Arc<DownloadControl>, message: &str) {
    set_busy(window, false);
    if control.is_cancelled() {
        set_status(Some(window), "已取消下载");
        return;
    }
    set_status(
        Some(window),
        &format!("{message}（媒体地址可能已过期，请重新播放后再试）"),
    );
}

fn finish_cancelled(window: &slint::Weak<AppWindow>) {
    set_busy(window, false);
    set_status(Some(window), "已取消下载");
}

fn set_status(window: Option<&slint::Weak<AppWindow>>, text: &str) {
    let Some(window) = window else {
        return;
    };
    let window = window.clone();
    let text = text.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = window.upgrade() {
            ui.set_download_status(text.into());
        }
    });
}

fn set_busy(window: &slint::Weak<AppWindow>, busy: bool) {
    let window = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = window.upgrade() {
            ui.set_download_busy(busy);
        }
    });
}

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

#[cfg(test)]
mod tests {
    use super::{
        connection_setting, download_bvid_override, execute, mbps, use_faststart, DownloadControl,
        DownloadError, Transport,
    };
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn measure_session() -> Option<crate::bilibili::Session> {
        let runtime = crate::bilibili::shared_runtime()?;
        runtime.block_on(crate::bilibili::Session::new()).ok()
    }

    /// `BILI_DOWNLOAD_BVID` when set, else the longest item on the 热门 first
    /// page (a larger file makes throughput differences readable).
    fn pick_target(
        session: &crate::bilibili::Session,
        runtime: &tokio::runtime::Runtime,
    ) -> Option<(String, u64, String)> {
        let bvid = match download_bvid_override() {
            Some(bvid) => bvid,
            None => runtime
                .block_on(session.popular(1, 20))
                .ok()?
                .items
                .iter()
                .max_by_key(|card| card.duration)?
                .bvid
                .clone(),
        };
        let detail = runtime.block_on(session.video_view(&bvid)).ok()?;
        Some((bvid, detail.cid.max(0) as u64, detail.title.clone()))
    }

    fn temp_dirs(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with(".bili-dl-"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Headless, same-video A/B of the legacy vs parallel transports. Ignored by
    /// default (real network + ffmpeg work). Run with:
    /// `BILI_DOWNLOAD_BVID=<bvid> BILI_DEBUG_LOG=<file> cargo test --release -- --ignored measure_download_transports`
    #[test]
    #[ignore = "performs real downloads; run explicitly with --ignored"]
    fn measure_download_transports() {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            crate::debug_log("measure: no runtime");
            return;
        };
        let Some(session) = measure_session() else {
            crate::debug_log("measure: no session");
            return;
        };
        let Some((bvid, cid, title)) = pick_target(&session, runtime) else {
            crate::debug_log("measure: no target");
            return;
        };
        crate::debug_log(&format!("measure target bvid={bvid} cid={cid} title={title}"));

        let dir: PathBuf = std::env::temp_dir().join(format!("bili-measure-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        crate::debug_log(&format!(
            "measure connections={} faststart={}",
            connection_setting(),
            use_faststart()
        ));

        for round in 0..3 {
            let mut order = [Transport::Legacy, Transport::Parallel];
            if round % 2 == 1 {
                order.swap(0, 1);
            }
            for transport in order {
                let name = match transport {
                    Transport::Legacy => "legacy",
                    Transport::Parallel => "parallel",
                };
                let out = dir.join(format!("{name}-{round}.mp4"));
                let _ = std::fs::remove_file(&out);
                let control = DownloadControl::new();
                let started = Instant::now();
                let result = execute(
                    &session,
                    bvid.clone(),
                    cid,
                    title.clone(),
                    Some(&out),
                    None,
                    &control,
                    transport,
                );
                let ms = started.elapsed().as_millis();
                let bytes = std::fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0);
                crate::debug_log(&format!(
                    "measure round={round} transport={transport:?} ok={} ms={ms} bytes={bytes} mbps={:.2}",
                    result.is_ok(),
                    mbps(bytes, ms)
                ));
                if let Err(DownloadError::Failed(message)) = &result {
                    crate::debug_log(&format!(
                        "measure round={round} transport={transport:?} err={message}"
                    ));
                }
            }
        }
    }

    /// Start a parallel download, cancel it after `BILI_CANCEL_MS` (default
    /// 400), and report whether the run ended cancelled with no output file or
    /// scratch directory left behind. Set `BILI_CANCEL_MS` past the transfer to
    /// land inside the mux stage. Run with:
    /// `BILI_DEBUG_LOG=<file> cargo test --release -- --ignored cancel_parallel_download`
    #[test]
    #[ignore = "performs a real download and cancels it; run explicitly with --ignored"]
    fn cancel_parallel_download() {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            crate::debug_log("cancel: no runtime");
            return;
        };
        let Some(session) = measure_session() else {
            crate::debug_log("cancel: no session");
            return;
        };
        let Some((bvid, cid, title)) = pick_target(&session, runtime) else {
            crate::debug_log("cancel: no target");
            return;
        };
        let delay_ms: u64 = std::env::var("BILI_CANCEL_MS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(400);
        let dir: PathBuf = std::env::temp_dir().join(format!("bili-cancel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("cancelled.mp4");
        let _ = std::fs::remove_file(&out);
        let control = DownloadControl::new();

        let result = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                execute(
                    &session,
                    bvid.clone(),
                    cid,
                    title.clone(),
                    Some(&out),
                    None,
                    &control,
                    Transport::Parallel,
                )
            });
            std::thread::sleep(Duration::from_millis(delay_ms));
            control.cancel();
            worker.join()
        });

        let cancelled = matches!(result, Ok(Err(DownloadError::Cancelled)));
        crate::debug_log(&format!(
            "cancel delay_ms={delay_ms} cancelled={cancelled} out_exists={} temp_dirs_left={}",
            out.exists(),
            temp_dirs(&dir)
        ));
        if let Ok(Err(DownloadError::Failed(message))) = &result {
            crate::debug_log(&format!("cancel err={message}"));
        }
    }
}
