//! 画面通道：480P 整片临时下载 → ffmpeg 均匀抽帧 → DeepSeek 视觉分批观察.
//!
//! The whole video is a **temporary** file: it is deleted as soon as the frames
//! are extracted, before any vision request. Frames are sampled evenly
//! (`interval = max(15s, duration / 39)`, at most 40) and sent in batches of at
//! most 10, so one bad batch is visible instead of hiding in a 40-image prompt.
//! Every token number in the evidence comes from DeepSeek's `usage` block.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::bilibili::Session;
use crate::fishwatch::proc;
use crate::fishwatch::steps::{SkipReason, Step, StepFuture, StepId, StepOutput};
use crate::fishwatch::{FrameGathered, WatchConfig, WatchCtx};

/// The verified endpoint and model for vision (same key as the verdict).
const VISION_URL: &str = "https://api.deepseek.com/chat/completions";
const VISION_MODEL: &str = "deepseek-flash";
/// Hard cap from the design.
pub const MAX_FRAMES: usize = 40;
/// Images per request: small enough that a single bad batch is auditable.
pub const VISION_BATCH: usize = 10;
pub struct FramesStep;

/// Why the frames channel cannot run, or `None` when it can. Shared by the
/// step's pre-flight check and the settings row so they never disagree.
pub fn preflight(config: &WatchConfig) -> Option<String> {
    if !config.online {
        return Some("未登录".to_string());
    }
    if config.deepseek.key.trim().is_empty() {
        return Some("未配置 DeepSeek key（视觉要用同一个 key）".to_string());
    }
    if !ffmpeg_available(&config.ffmpeg) {
        return Some("ffmpeg 不在 PATH".to_string());
    }
    None
}

impl FramesStep {
    pub fn new(_config: &WatchConfig) -> Self {
        FramesStep
    }
}

impl Step for FramesStep {
    fn id(&self) -> StepId {
        StepId::Frames
    }


    fn timeout(&self) -> Duration {
        Duration::from_secs(1_200)
    }

    fn skip(&self, config: &WatchConfig) -> Option<SkipReason> {
        preflight(config).map(SkipReason::new)
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a> {
        Box::pin(async move {
            let Some(session) = ctx.session.clone() else {
                return Ok(StepOutput {
                    skip_reason: Some("未登录".to_string()),
                    ..StepOutput::default()
                });
            };
            let bvid = ctx.gathered.meta.bvid.clone();
            let cid = ctx.gathered.meta.cid;
            let duration = ctx.gathered.meta.duration_s;
            if cid == 0 {
                return Ok(StepOutput {
                    skip_reason: Some("缺少 cid".to_string()),
                    ..StepOutput::default()
                });
            }
            if ctx.config.deepseek.key.trim().is_empty() {
                return Ok(StepOutput {
                    skip_reason: Some("未配置 DeepSeek key".to_string()),
                    ..StepOutput::default()
                });
            }
            let work = ctx
                .config
                .work_dir
                .join(format!("frames-{}-{cid}", sanitise(&bvid)));
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work)
                .with_context(|| format!("创建抽帧目录失败：{}", work.display()))?;

            let interval = sample_interval(duration);
            let expected = expected_frames(duration, interval);
            let movie = work.join("movie.mp4");
            let started = Instant::now();

            let (video_bytes, track) = match download_video(
                &ctx.config,
                &session,
                &bvid,
                cid,
                &movie,
            )
            .await
            {
                Ok(result) => result,
                Err(err) => {
                    let _ = std::fs::remove_dir_all(&work);
                    return Ok(StepOutput {
                        skip_reason: Some(format!("frames_failed: 下载视频失败：{err:#}")),
                        ..StepOutput::default()
                    });
                }
            };
            crate::debug_log(&format!(
                "fish frames video bvid={bvid} track={}x{} qn={} bytes={} interval_s={interval:.2} expected={expected}",
                track.0, track.1, track.2, video_bytes
            ));

            let extracted = match extract(&ctx.config.ffmpeg, &movie, &work, interval) {
                Ok(frames) => frames,
                Err(err) => {
                    let _ = std::fs::remove_dir_all(&work);
                    return Ok(StepOutput {
                        skip_reason: Some(format!("frames_failed: 抽帧失败：{err:#}")),
                        bytes: video_bytes,
                        ..StepOutput::default()
                    });
                }
            };
            // The whole-movie temp file is done with; delete it before vision.
            let _ = std::fs::remove_file(&movie);
            let image_bytes: u64 = extracted.iter().map(|frame| frame.bytes).sum();
            if extracted.is_empty() {
                let _ = std::fs::remove_dir_all(&work);
                return Ok(StepOutput {
                    skip_reason: Some("frames_failed: 没有抽出任何画面".to_string()),
                    bytes: video_bytes,
                    ..StepOutput::default()
                });
            }
            crate::debug_log(&format!(
                "fish frames extracted bvid={bvid} frames={} jpeg_bytes={} dir={}",
                extracted.len(),
                image_bytes,
                work.display()
            ));

            let batch_size = ctx.config.vision.batch.clamp(1, VISION_BATCH);
            let batches = extracted.len().div_ceil(batch_size).max(1);
            let mut observations: Vec<String> = Vec::new();
            let mut tokens = 0u64;
            let mut failed_batches = 0usize;
            for batch in 0..batches {
                let from = batch * batch_size;
                let to = (from + batch_size).min(extracted.len());
                let group = &extracted[from..to];
                match ask_vision(&ctx.config, &ctx.http, group, batch, batches).await {
                    Ok(observed) => {
                        tokens = tokens.saturating_add(observed.tokens);
                        observations.push(observed.text);
                    }
                    Err(err) => {
                        failed_batches += 1;
                        crate::debug_log(&format!(
                            "fish vision batch={} failed: {err:#}",
                            batch + 1
                        ));
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&work);
            if observations.is_empty() {
                return Ok(StepOutput {
                    skip_reason: Some(format!(
                        "frames_failed: {} 批画面观察全部失败",
                        batches
                    )),
                    bytes: video_bytes + image_bytes,
                    ..StepOutput::default()
                });
            }
            let observations = observations.join("\n");
            ctx.gathered.frames = Some(FrameGathered {
                frames: extracted.len(),
                interval_s: interval,
                video_bytes,
                image_bytes,
                observations: observations.clone(),
                batches,
                tokens,
            });
            let wall_ms = started.elapsed().as_millis() as u64;
            crate::debug_log(&format!(
                "fish frames done bvid={bvid} frames={} batches={} failed={failed_batches} video_bytes={video_bytes} jpeg_bytes={image_bytes} tokens={tokens} wall_ms={wall_ms}",
                extracted.len(),
                batches
            ));
            Ok(StepOutput {
                detail: format!(
                    "{} 张 · {} 批 · {} · {} tok",
                    extracted.len(),
                    batches,
                    crate::fishwatch::asr::models::human_bytes(image_bytes),
                    tokens
                ),
                bytes: video_bytes + image_bytes,
                tokens,
                note: if failed_batches > 0 {
                    format!("画面有 {failed_batches}/{batches} 批失败")
                } else {
                    String::new()
                },
                ..StepOutput::default()
            })
        })
    }
}

/// `max(15s, duration / 39)` — the design's even sampling.
pub fn sample_interval(duration_s: u64) -> f64 {
    let duration = duration_s.max(1) as f64;
    (duration / 39.0).max(15.0)
}

/// How many frames ffmpeg should produce (never above [`MAX_FRAMES`]).
pub fn expected_frames(duration_s: u64, interval: f64) -> usize {
    if duration_s == 0 || interval <= 0.0 {
        return 0;
    }
    (((duration_s as f64) / interval).floor() as usize + 1).min(MAX_FRAMES)
}

fn ffmpeg_available(ffmpeg: &str) -> bool {
    // `publish_watch_steps` asks on the UI thread at startup; a probe per call
    // would spawn a process (and, before `proc::command`, a console) every
    // time the panel is rebuilt. The answer cannot change while we run.
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

struct ExtractedFrame {
    path: PathBuf,
    /// Seconds into the video.
    at_s: f64,
    bytes: u64,
}

async fn download_video(
    _config: &WatchConfig,
    session: &Session,
    bvid: &str,
    cid: u64,
    path: &Path,
) -> Result<(u64, (u32, u32, i64))> {
    let info = session.playurl(bvid, cid).await?;
    if info.video_tracks.is_empty() {
        return Err(anyhow!("没有可下载的视频流"));
    }
    // <= 480P measured by the SHORT side: a portrait 480x854 stream is 480P,
    // while filtering on `height` would wrongly accept/reject by orientation.
    let short_side = |track: &crate::bilibili::VideoTrack| track.width.min(track.height);
    let mut candidates: Vec<&crate::bilibili::VideoTrack> = info
        .video_tracks
        .iter()
        .filter(|track| short_side(track) <= 480)
        .collect();
    if candidates.is_empty() {
        candidates = info.video_tracks.iter().collect();
    }
    candidates.sort_by(|a, b| {
        short_side(b)
            .cmp(&short_side(a))
            .then_with(|| a.bandwidth.cmp(&b.bandwidth))
    });
    let track = candidates[0];
    let feature = (track.width, track.height, track.id);
    let referer = format!("https://www.bilibili.com/video/{bvid}");
    let mut last = None;
    for url in track.urls.iter().chain(std::iter::once(&track.url)) {
        match stream_to_file(session, url, &referer, path).await {
            Ok(bytes) if bytes > 10_000 => return Ok((bytes, feature)),
            Ok(bytes) => last = Some(anyhow!("视频流只有 {bytes} 字节")),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("没有可用的视频 CDN")))
}

async fn stream_to_file(
    session: &Session,
    url: &str,
    referer: &str,
    path: &Path,
) -> Result<u64> {
    let response = session.open_stream(url, referer).await?;
    if !response.status().is_success() {
        return Err(anyhow!("视频 HTTP {}", response.status().as_u16()));
    }
    let mut file = std::fs::File::create(path)?;
    let mut stream = response.bytes_stream();
    let mut bytes = 0u64;
    use std::io::Write;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        bytes += chunk.len() as u64;
    }
    file.flush()?;
    Ok(bytes)
}

/// One ffmpeg pass samples the whole file (`fps=1/interval`), scales the short
/// side to [`SHORT_SIDE`] and writes q≈85 JPEGs.
fn extract(ffmpeg: &str, movie: &Path, work: &Path, interval: f64) -> Result<Vec<ExtractedFrame>> {
    const SHORT_SIDE: u32 = 480;
    let filter = format!(
        "fps=1/{interval:.3},scale='if(gte(iw,ih),-2,{SHORT_SIDE})':'if(gte(iw,ih),{SHORT_SIDE},-2)'"
    );
    let pattern = work.join("f-%03d.jpg");
    let status = proc::command(ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(movie)
        .args(["-vf", &filter, "-frames:v", &MAX_FRAMES.to_string(), "-q:v", "3"])
        .arg(&pattern)
        .status()
        .with_context(|| format!("启动 ffmpeg 失败：{ffmpeg}"))?;
    if !status.success() {
        return Err(anyhow!("ffmpeg 退出码 {:?}", status.code()));
    }
    let mut frames: Vec<ExtractedFrame> = Vec::new();
    for index in 1..=MAX_FRAMES {
        let path = work.join(format!("f-{index:03}.jpg"));
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() < 500 {
            continue;
        }
        frames.push(ExtractedFrame {
            path,
            at_s: (index - 1) as f64 * interval,
            bytes: meta.len(),
        });
    }
    Ok(frames)
}

struct VisionResult {
    text: String,
    tokens: u64,
}

async fn ask_vision(
    config: &WatchConfig,
    http: &reqwest::Client,
    frames: &[ExtractedFrame],
    batch: usize,
    batches: usize,
) -> Result<VisionResult> {
    let times = frames
        .iter()
        .map(|frame| mmss(frame.at_s.max(0.0) as u64))
        .collect::<Vec<_>>()
        .join("、");
    let prompt = format!(
        "这是同一个视频按时间均匀抽取的画面，第 {}/{} 批，共 {} 张，时间点依次是：{}。\n\
         只描述画面里能确认的事实：场景、人物在做什么、屏幕上的文字/图表/字幕、明显的剪辑或广告痕迹。\n\
         不要推断剧情，不要评价好不好看；看不清就写“不确定”。\n\
         输出 JSON：{{\"observations\":[\"每条一句，最多 6 条\"],\"onscreen_text\":[\"屏幕上能读出的文字，最多 8 条\"]}}",
        batch + 1,
        batches,
        frames.len(),
        times
    );
    let mut content: Vec<Value> = Vec::with_capacity(frames.len() + 1);
    content.push(json!({ "type": "text", "text": prompt }));
    for frame in frames {
        let bytes = std::fs::read(&frame.path)
            .with_context(|| format!("读取抽帧失败：{}", frame.path.display()))?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:image/jpeg;base64,{encoded}"),
                "detail": config.vision.detail,
            }
        }));
    }
    let body = json!({
        "model": VISION_MODEL,
        "messages": [
            { "role": "system", "content": SYSTEM_VISION },
            { "role": "user", "content": content },
        ],
        "thinking": { "type": "disabled" },
        "temperature": 0.2,
        "max_tokens": 700,
        "stream": false,
    });
    let started = Instant::now();
    let response = http
        .post(VISION_URL)
        .bearer_auth(config.deepseek.key.trim())
        .json(&body)
        .send()
        .await
        .context("请求 DeepSeek 视觉失败")?;
    let status = response.status();
    let text = response.text().await.context("读取视觉响应失败")?;
    if !status.is_success() {
        return Err(anyhow!(
            "视觉 HTTP {}：{}",
            status.as_u16(),
            clip(&text, 300)
        ));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|err| anyhow!("视觉响应解析失败：{err}"))?;
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
        return Err(anyhow!("视觉返回了空内容"));
    }
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let image_tokens = find_image_tokens(&usage);
    let tokens = prompt_tokens + completion_tokens;
    let text = summarise(&raw, frames, batch);
    crate::debug_log(&format!(
        "fish vision batch={}/{} images={} detail={} ms={} prompt_tokens={} completion_tokens={} image_tokens={} usage={}",
        batch + 1,
        batches,
        frames.len(),
        config.vision.detail,
        started.elapsed().as_millis(),
        prompt_tokens,
        completion_tokens,
        image_tokens
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string()),
        clip(&usage.to_string(), 400)
    ));
    Ok(VisionResult { text, tokens })
}

const SYSTEM_VISION: &str = "你是画面观察员，只负责如实描述抽样画面，不替鱼主人做看/不看的决定。\
不编造画面里没有的东西；屏幕文字看不清就说看不清。严格输出 JSON 对象。";

/// `prompt_tokens_details` is an open shape (`cached_tokens`, image budget, …);
/// find any image-ish counter so the log keeps what DeepSeek actually said.
fn find_image_tokens(usage: &Value) -> Option<u64> {
    fn walk(value: &Value, depth: usize) -> Option<u64> {
        if depth > 4 {
            return None;
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let key_lower = key.to_ascii_lowercase();
                    if key_lower.contains("image") && key_lower.contains("token") {
                        if let Some(number) = value.as_u64() {
                            return Some(number);
                        }
                    }
                    if let Some(found) = walk(value, depth + 1) {
                        return Some(found);
                    }
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|item| walk(item, depth + 1)),
            _ => None,
        }
    }
    walk(usage, 0)
}

/// Turn the model's JSON (or prose) into the text that goes into the verdict
/// prompt, with its time range so the final model can cite it.
fn summarise(raw: &str, frames: &[ExtractedFrame], batch: usize) -> String {
    let from = frames.first().map(|frame| frame.at_s).unwrap_or(0.0);
    let to = frames.last().map(|frame| frame.at_s).unwrap_or(from);
    let mut lines = vec![format!(
        "第 {} 批（{}–{}）：",
        batch + 1,
        mmss(from.max(0.0) as u64),
        mmss(to.max(0.0) as u64)
    )];
    let json_text = extract_json(raw);
    let parsed = json_text.and_then(|text| serde_json::from_str::<Value>(text).ok());
    match parsed {
        Some(value) => {
            if let Some(items) = value.get("observations").and_then(|value| value.as_array()) {
                for item in items.iter().filter_map(|item| item.as_str()).take(6) {
                    lines.push(format!("- {}", clip(item.trim(), 120)));
                }
            }
            if let Some(items) = value.get("onscreen_text").and_then(|value| value.as_array()) {
                let texts: Vec<String> = items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|text| clip(text.trim(), 80))
                    .filter(|text| !text.is_empty())
                    .take(8)
                    .collect();
                if !texts.is_empty() {
                    lines.push(format!("屏幕文字：{}", texts.join(" / ")));
                }
            }
        }
        None => lines.push(format!("- {}", clip(&collapse(raw), 400))),
    }
    if lines.len() == 1 {
        lines.push(format!("- {}", clip(&collapse(raw), 400)));
    }
    lines.join("\n")
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

fn mmss(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let rest = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
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
    fn sampling_follows_the_design_formula() {
        assert_eq!(sample_interval(0), 15.0);
        assert_eq!(sample_interval(176), 15.0);
        assert!((sample_interval(1_000) - 25.641_025_6).abs() < 0.001);
        assert_eq!(expected_frames(176, 15.0), 12);
        assert_eq!(expected_frames(3_600, 15.0), MAX_FRAMES);
        assert!(expected_frames(3_600, sample_interval(3_600)) <= MAX_FRAMES);
    }

    #[test]
    fn image_tokens_are_found_in_the_usage_tree() {
        let usage = json!({
            "prompt_tokens": 1200,
            "completion_tokens": 80,
            "prompt_tokens_details": { "image_tokens": 1024, "cached_tokens": 0 }
        });
        assert_eq!(find_image_tokens(&usage), Some(1_024));
        assert_eq!(find_image_tokens(&json!({"prompt_tokens": 1})), None);
    }

    #[test]
    fn summary_is_citable_and_timestamped() {
        let frames = vec![
            ExtractedFrame {
                path: PathBuf::new(),
                at_s: 15.0,
                bytes: 1,
            },
            ExtractedFrame {
                path: PathBuf::new(),
                at_s: 30.0,
                bytes: 1,
            },
        ];
        let text = summarise(
            r#"{"observations":["桌上有一杯咖啡","字幕写着 第一章"],"onscreen_text":["第一章"]}"#,
            &frames,
            0,
        );
        assert!(text.starts_with("第 1 批（0:15–0:30）："));
        assert!(text.contains("桌上有一杯咖啡"));
        assert!(text.contains("屏幕文字：第一章"));
    }

    #[test]
    fn prose_fallback_is_kept_not_dropped() {
        let frames = vec![ExtractedFrame {
            path: PathBuf::new(),
            at_s: 0.0,
            bytes: 1,
        }];
        let text = summarise("一个男人在讲台上说话，背景有红色横幅", &frames, 0);
        assert!(text.contains("一个男人在讲台上说话"));
    }

    /// Real network + DeepSeek vision, opt-in. Uses the key in `settings.json`
    /// (or `DEEPSEEK_API_KEY`) and a real video's 480P stream.
    #[test]
    #[ignore = "downloads a video and calls DeepSeek vision; needs BILI_FRAME_BVID"]
    fn live_vision_frames() {
        let Ok(bvid) = std::env::var("BILI_FRAME_BVID") else {
            eprintln!("BILI_FRAME_BVID absent: skipped");
            return;
        };
        let key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_else(|| crate::settings::load().deepseek_key);
        assert!(!key.trim().is_empty(), "no DeepSeek key");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let token = crate::bilibili::load_token().expect("token.json");
        let session = runtime
            .block_on(crate::bilibili::Session::from_token(&token))
            .expect("session");
        let meta = runtime
            .block_on(session.fish_meta(&bvid))
            .expect("fish meta");
        let config = WatchConfig {
            online: true,
            deepseek: crate::fishwatch::DeepseekOptions {
                key,
                ..crate::fishwatch::DeepseekOptions::default()
            },
            work_dir: std::env::temp_dir().join("o11a1-frames-live"),
            ..WatchConfig::default()
        };
        let mut ctx = WatchCtx::new(config, crate::fishwatch::Meta {
            bvid: bvid.clone(),
            cid: meta.cid,
            aid: meta.aid,
            title: meta.title.clone(),
            duration_s: meta.duration_s,
            ..crate::fishwatch::Meta::default()
        })
        .with_session(std::sync::Arc::new(session));
        let step = FramesStep::new(&ctx.config);
        let started = Instant::now();
        let output = runtime
            .block_on(step.run(&mut ctx))
            .expect("frames step");
        let frames = ctx.gathered.frames.as_ref().expect("frames gathered");
        let dir = ctx.config.work_dir.join(format!("frames-{}-{}", bvid, meta.cid));
        eprintln!(
            "frames={} batches={} interval_s={:.2} video_bytes={} jpeg_bytes={} tokens={} wall_ms={} temp_dir_exists={}",
            frames.frames,
            frames.batches,
            frames.interval_s,
            frames.video_bytes,
            frames.image_bytes,
            frames.tokens,
            started.elapsed().as_millis(),
            dir.exists() as i32
        );
        eprintln!("step: {}", output.detail);
        eprintln!("observations:\n{}", frames.observations);
        assert!(frames.frames >= 2);
        assert!(!frames.observations.trim().is_empty());
        assert!(!dir.exists(), "temporary frame directory must be deleted");
    }
}
