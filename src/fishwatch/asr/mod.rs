//! 语音转写（ASR）：engine choice, model readiness, audio cache and the step.
//!
//! Engines are external processes on purpose:
//! * `faster-whisper` (CTranslate2) is the verified GPU path on this machine;
//! * `whisper.cpp` is the no-Python path, for machines with a CUDA/Vulkan build.
//!
//! The run never pretends one is present: [`detect`] probes the real
//! interpreter/binary, the model manifest is sha256-checked, and the step is
//! `skipped(reason)` when either is missing. While any player is playing or
//! buffering the step waits before touching the GPU (`fish asr paused
//! reason=playback` → `fish asr resumed waited_ms=…`).

pub mod chunk;
pub mod external_fw;
pub mod models;
pub mod whispercpp;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;

use crate::bilibili::Session;
use crate::fishwatch::proc;
use crate::fishwatch::steps::{SkipReason, Step, StepFuture, StepId, StepOutput};
use crate::fishwatch::transcript;
use crate::fishwatch::{WatchConfig, WatchCtx};
#[cfg(test)]
use crate::fishwatch::Gathered;

/// Audio cache cap: 2 GB, oldest files deleted first.
pub const CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// One transcript produced by any engine.
#[derive(Clone, Debug)]
pub struct Transcript {
    pub segments: Vec<transcript::Segment>,
    pub language: String,
    pub language_probability: f32,
    pub audio_seconds: f64,
    pub load_ms: u64,
    pub decode_ms: u64,
    pub backend: String,
}

pub trait Transcriber: Send + Sync {
    fn transcribe(&self, audio: &Path) -> Result<Transcript>;
}

/// What the machine can actually run right now.
#[derive(Clone, Debug)]
pub struct Availability {
    /// `faster-whisper` | `whisper.cpp`.
    pub engine: &'static str,
    /// `cuda` | `vulkan` | `cpu`.
    pub device: String,
    pub detail: String,
    pub python: Option<String>,
    pub cpp_binary: Option<PathBuf>,
}

/// CTranslate2 device count probe (once; ~0.8 s).
pub fn cuda_count(python: &str) -> u32 {
    static COUNT: OnceLock<u32> = OnceLock::new();
    *COUNT.get_or_init(|| external_fw::cuda_device_count(python))
}

/// GPU name for logs/UI (once; `nvidia-smi` is already a dependency of the OS).
pub fn gpu_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let output = proc::command("nvidia-smi")
            .args(["--query-gpu=name", "--format=csv,noheader"])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            _ => String::new(),
        }
    })
    .clone()
}

/// Pick the requested engine, falling back to the other one only for `auto`.
/// `device` is the *effective* device (a `gpu` request on a machine without
/// CUDA comes back as `cpu`, with the reason in `detail`).
/// The real probe (blocking): spawns python / nvidia-smi. Records its answer in
/// the cache so the UI can read it later without paying again.
pub fn detect(config: &WatchConfig) -> Option<Availability> {
    let value = detect_inner(config);
    let key = probe_key(config);
    if let Ok(mut slot) = probe_slot().lock() {
        *slot = Some(Probe { key, value: value.clone() });
    }
    value
}

fn detect_inner(config: &WatchConfig) -> Option<Availability> {
    let python = external_fw::find_python();
    let cpp = whispercpp::find_binary(&PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let requested = config.asr.engine.trim();
    let (engine, python, cpp_binary) = match requested {
        "faster-whisper" => ("faster-whisper", python, None),
        "whisper.cpp" => ("whisper.cpp", None, cpp),
        _ => {
            if python.is_some() {
                ("faster-whisper", python, None)
            } else if cpp.is_some() {
                ("whisper.cpp", None, cpp)
            } else {
                return None;
            }
        }
    };
    let gpu = gpu_name();
    let device;
    let detail;
    if engine == "faster-whisper" {
        let python = python.as_deref()?;
        let count = cuda_count(python);
        if config.asr.device == "gpu" && count > 0 {
            detail = format!("CTranslate2 可见 {count} 块 GPU{}", if gpu.is_empty() { String::new() } else { format!("（{gpu}）") });
            device = "cuda".to_string();
        } else {
            detail = if config.asr.device == "gpu" {
                "未检测到可用的 CUDA 设备，退回 CPU（int8）".to_string()
            } else {
                "按设置使用 CPU（int8）".to_string()
            };
            device = "cpu".to_string();
        }
    } else {
        let Some(binary) = cpp_binary.as_deref() else {
            return None;
        };
        device = whispercpp::detect_device(binary);
        detail = format!(
            "whisper-cli 构建后端：{device}{}",
            if gpu.is_empty() {
                String::new()
            } else {
                format!("（{gpu}）")
            }
        );
    }
    Some(Availability {
        engine,
        device,
        detail,
        python,
        cpp_binary,
    })
}

// ---------------------------------------------------------------------------
// Non-blocking probe for the UI
// ---------------------------------------------------------------------------
//
// `detect` shells out (`python -c "import ctranslate2"`, `nvidia-smi`) and can
// take seconds on a cold CUDA load. It must never run on the UI thread: the
// settings panel used to call it while the first window was still being built,
// which left the app invisible behind a python console. The panel now reads
// [`snapshot`] (cheap, may be `Unknown` once) and [`warm`] fills it off-thread.

/// One finished probe, tagged with the config it answered for.
struct Probe {
    key: String,
    value: Option<Availability>,
}

fn probe_slot() -> &'static std::sync::Mutex<Option<Probe>> {
    static SLOT: OnceLock<std::sync::Mutex<Option<Probe>>> = OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

fn probe_key(config: &WatchConfig) -> String {
    format!(
        "{}|{}|{}",
        config.asr.engine.trim(),
        config.asr.device.trim(),
        config.asr.model.trim()
    )
}

/// `None` = this config has not been probed yet; `Some(None)` = probed and
/// nothing is installed; `Some(Some(a))` = ready.
pub fn snapshot(config: &WatchConfig) -> Option<Option<Availability>> {
    let key = probe_key(config);
    let slot = probe_slot().lock().ok()?;
    slot.as_ref()
        .filter(|probe| probe.key == key)
        .map(|probe| probe.value.clone())
}

/// Start the probe in the background unless it already ran (or is running).
///
/// Nothing is spawned while ASR is switched off: the user's default state must
/// not pay for a `python -c "import ctranslate2"` (or a console window) they
/// never asked for.
pub fn warm(config: &WatchConfig) {
    if !config.asr.enabled {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARMING: AtomicBool = AtomicBool::new(false);
    if snapshot(config).is_some() || WARMING.swap(true, Ordering::SeqCst) {
        return;
    }
    let config = config.clone();
    std::thread::spawn(move || {
        let value = detect(&config);
        let key = probe_key(&config);
        if let Ok(mut slot) = probe_slot().lock() {
            *slot = Some(Probe { key, value });
        }
        WARMING.store(false, Ordering::SeqCst);
    });
}

/// Which engine's model manifest applies. This answers from the probe cache and
/// **never spawns anything**: it is read while the first window is being built,
/// and a `python -c "import faster_whisper"` here used to hold the whole app
/// invisible behind a console for as long as the import took.
fn engine_hint(config: &WatchConfig) -> &'static str {
    match config.asr.engine.trim() {
        "whisper.cpp" => "whisper.cpp",
        "faster-whisper" => "faster-whisper",
        // `auto`: whatever the background probe found; faster-whisper (the
        // verified path on this machine) until then.
        _ => snapshot(config)
            .flatten()
            .map(|probe| probe.engine)
            .unwrap_or("faster-whisper"),
    }
}

/// The model this config would use, if the engine resolves to one.
pub fn spec_for(config: &WatchConfig) -> Option<&'static models::ModelSpec> {
    models::spec(&config.asr.model, engine_hint(config))
}

pub fn model_ready(config: &WatchConfig) -> bool {
    spec_for(config)
        .map(|spec| models::model_ready(&config.model_dir, spec))
        .unwrap_or(false)
}

/// One sentence for the settings panel. Never claims a device it did not probe.
pub fn status_text(config: &WatchConfig) -> String {
    let Some(available) = detect(config) else {
        return "引擎未安装：需要本机 faster-whisper（Python）或 whisper-cli".to_string();
    };
    let spec = spec_for(config);
    let model = spec.map(|spec| spec.id).unwrap_or(config.asr.model.as_str());
    let (ready, size) = match spec {
        Some(spec) if models::model_ready(&config.model_dir, spec) => {
            (true, models::human_bytes(spec.total_bytes()))
        }
        Some(spec) => (false, models::human_bytes(spec.total_bytes())),
        None => (false, "?".to_string()),
    };
    format!(
        "{} · {} · {} 模型 {} {} · {}",
        available.engine,
        available.device,
        model,
        size,
        if ready { "已校验" } else { "未下载" },
        available.detail
    )
}

// ---------------------------------------------------------------------------
// Audio cache
// ---------------------------------------------------------------------------

pub struct AudioFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub reused: bool,
}

/// Download the audio of one video and normalise it to 16 kHz mono PCM WAV,
/// cached as `cache/fishaudio/<bvid>-<cid>.wav`. The cache is separate from the
/// player's video cache and capped at [`CACHE_MAX_BYTES`] (LRU by mtime).
pub async fn ensure_audio(
    config: &WatchConfig,
    session: &Session,
    bvid: &str,
    cid: u64,
) -> Result<AudioFile> {
    let dir = &config.audio_dir;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建音频缓存目录失败：{}", dir.display()))?;
    let wav = dir.join(format!("{}-{cid}.wav", sanitise(bvid)));
    if let Ok(meta) = std::fs::metadata(&wav) {
        if meta.len() > 1_000 {
            return Ok(AudioFile {
                path: wav,
                bytes: 0,
                reused: true,
            });
        }
    }

    let info = session.playurl(bvid, cid).await?;
    let urls = info.audio_urls_for(0);
    if urls.is_empty() {
        return Err(anyhow!("没有可下载的音轨"));
    }
    let referer = format!("https://www.bilibili.com/video/{bvid}");
    let temp = dir.join(format!("{}-{cid}.m4a.part", sanitise(bvid)));
    let mut last_error = None;
    let mut downloaded = 0u64;
    for url in &urls {
        match download_stream(session, url, &referer, &temp).await {
            Ok(bytes) => {
                downloaded = bytes;
                last_error = None;
                break;
            }
            Err(err) => {
                last_error = Some(err);
                let _ = std::fs::remove_file(&temp);
            }
        }
    }
    if let Some(err) = last_error {
        return Err(anyhow!("下载音频失败：{err:#}"));
    }

    // ffmpeg normalises the container and the sample rate; whisper.cpp only
    // accepts 16 kHz mono PCM, and faster-whisper decodes this faster too.
    let wav_part = dir.join(format!("{}-{cid}.wav.part", sanitise(bvid)));
    let status = proc::command(&config.ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(&temp)
        .args(["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", "-f", "wav"])
        .arg(&wav_part)
        .status()
        .with_context(|| format!("启动 ffmpeg 失败：{}", config.ffmpeg))?;
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        return Err(anyhow!("ffmpeg 转码失败（退出码 {:?}）", status.code()));
    }
    let _ = std::fs::remove_file(&temp);
    std::fs::rename(&wav_part, &wav)?;
    let size = std::fs::metadata(&wav).map(|meta| meta.len()).unwrap_or(0);
    crate::debug_log(&format!(
        "fish audio cached path={} bytes={size} downloaded={downloaded}",
        wav.display()
    ));
    trim_cache(dir, &wav);
    Ok(AudioFile {
        path: wav,
        bytes: downloaded,
        reused: false,
    })
}

async fn download_stream(
    session: &Session,
    url: &str,
    referer: &str,
    path: &Path,
) -> Result<u64> {
    let response = session
        .open_stream(url, referer)
        .await
        .context("请求音频流失败")?;
    if !response.status().is_success() {
        return Err(anyhow!("音频 HTTP {}", response.status().as_u16()));
    }
    let mut file = std::fs::File::create(path)
        .with_context(|| format!("创建音频临时文件失败：{}", path.display()))?;
    let mut stream = response.bytes_stream();
    let mut bytes = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("读取音频流失败")?;
        file.write_all(&chunk)?;
        bytes += chunk.len() as u64;
    }
    file.flush()?;
    if bytes < 1_000 {
        return Err(anyhow!("音频流只有 {bytes} 字节"));
    }
    Ok(bytes)
}

fn sanitise(bvid: &str) -> String {
    bvid.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect()
}

/// Keep the newest files under [`CACHE_MAX_BYTES`]; the file just written is
/// never deleted, even if a single video is larger than the cap.
pub fn trim_cache(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("wav") {
            continue;
        }
        if path == keep {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        files.push((path, meta.len(), modified));
    }
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    let keep_size = std::fs::metadata(keep).map(|meta| meta.len()).unwrap_or(0);
    if total + keep_size <= CACHE_MAX_BYTES {
        return;
    }
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in files {
        if total + keep_size <= CACHE_MAX_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            crate::debug_log(&format!(
                "fish audio cache evict path={} bytes={size}",
                path.display()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Process helper
// ---------------------------------------------------------------------------

/// Run a child with a hard deadline, stdout/stderr going to `log_path` so a
/// chatty engine can never deadlock on a full pipe. On timeout the child is
/// killed and the call fails.
pub fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    log_path: &Path,
) -> Result<ExitStatus> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::File::create(log_path)
        .with_context(|| format!("创建 ASR 日志失败：{}", log_path.display()))?;
    let err = log.try_clone()?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(err));
    let mut child = command.spawn().context("启动 ASR 引擎失败")?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "ASR 引擎超时（{}s，已结束进程）",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------------------
// Step
// ---------------------------------------------------------------------------

pub struct AsrStep {
    config: WatchConfig,
}

/// What one ASR run produced, plus what it cost and how it was scheduled.
struct LongRun {
    transcript: Transcript,
    /// `cuda` / `cpu` / `cuda→cpu` when at least one chunk had to degrade.
    used_device: String,
    /// A sentence for the step note; empty when nothing degraded.
    note: String,
    chunks: usize,
    gpu_chunks: usize,
    cpu_chunks: usize,
    /// Wall time of the first chunk (it also pays the model load).
    first_chunk_ms: u64,
    wall_ms: u64,
}

impl AsrStep {
    pub fn new(config: &WatchConfig) -> Self {
        AsrStep {
            config: config.clone(),
        }
    }

    fn build(&self, availability: &Availability, device: &str) -> Result<Box<dyn Transcriber>> {
        let spec = spec_for(&self.config).ok_or_else(|| anyhow!("未知的模型档位"))?;
        let model_dir = spec.path(&self.config.model_dir);
        match availability.engine {
            "faster-whisper" => {
                let python = availability
                    .python
                    .clone()
                    .ok_or_else(|| anyhow!("找不到能 import faster_whisper 的 Python"))?;
                Ok(Box::new(external_fw::ExternalFw {
                    python,
                    script: self.config.audio_dir.join("fw_transcribe.py"),
                    model_dir,
                    device: device.to_string(),
                    compute: if device == "cuda" {
                        "float16".to_string()
                    } else {
                        "int8".to_string()
                    },
                    threads: self.config.asr.threads.max(1),
                    work_dir: self.config.work_dir.clone(),
                }))
            }
            "whisper.cpp" => {
                let binary = availability
                    .cpp_binary
                    .clone()
                    .ok_or_else(|| anyhow!("找不到 whisper-cli"))?;
                let model = self.config.model_dir.join(spec.primary().name);
                // whisper.cpp specs put the GGML file in the spec directory.
                let model = if model.exists() {
                    model
                } else {
                    model_dir.join(spec.primary().name)
                };
                Ok(Box::new(whispercpp::WhisperCpp {
                    binary,
                    model,
                    threads: self.config.asr.threads.max(1),
                    work_dir: self.config.work_dir.clone(),
                    device: device.to_string(),
                }))
            }
            other => Err(anyhow!("未知引擎 {other}")),
        }
    }

    /// Transcribe one file, splitting long audio so no single engine call sees
    /// a long input. Per chunk: GPU first, CPU on failure; only when every
    /// chunk fails does the run fail. Short audio becomes a single chunk that
    /// *is* the original file, so the common path is unchanged.
    fn transcribe_long(
        &self,
        availability: &Availability,
        audio: &Path,
        ctx: &WatchCtx,
    ) -> Result<LongRun> {
        let device = availability.device.clone();
        let total_s = chunk::wav_seconds(audio).unwrap_or(0.0);
        let chunk_s = chunk::chunk_seconds();
        let dir = self
            .config
            .work_dir
            .join(format!("asr-chunks-{}-{}", std::process::id(), monotonic_id()));
        let started = Instant::now();
        let pieces = match chunk::split(&self.config.ffmpeg, audio, &dir, chunk_s) {
            Ok(pieces) => pieces,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(err);
            }
        };
        let split_ms = started.elapsed().as_millis() as u64;
        crate::debug_log(&format!(
            "fish asr chunked audio_s={total_s:.1} chunk_s={chunk_s} chunks={} split_ms={split_ms} dir={}",
            pieces.len(),
            dir.display()
        ));

        let mut parts: Vec<(f64, Vec<transcript::Segment>)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        let mut gpu_chunks = 0usize;
        let mut cpu_chunks = 0usize;
        let mut first_chunk_ms = 0u64;
        let mut load_ms = 0u64;
        let mut decode_ms = 0u64;
        let mut language = String::new();
        let mut language_probability = 0.0f32;
        let mut base = 0.0f64;

        for (index, piece) in pieces.iter().enumerate() {
            let chunk_started = Instant::now();
            // The player shares this GPU: a long run must not hold it hostage.
            let _ = wait_for_playback(ctx);
            let attempt = self
                .build(availability, &device)
                .and_then(|transcriber| transcriber.transcribe(piece));
            let (transcript, chunk_device) = match attempt {
                Ok(transcript) => {
                    gpu_chunks += 1;
                    (transcript, device.clone())
                }
                Err(gpu_error) if device != "cpu" => {
                    let reason = reason_of(&gpu_error);
                    crate::debug_log(&format!(
                        "fish asr chunk={index}/{} device=gpu->cpu reason={reason} detail={gpu_error:#}",
                        pieces.len()
                    ));
                    crate::debug_log(&format!(
                        "fish asr fallback device=cpu reason={gpu_error:#}"
                    ));
                    match self
                        .build(availability, "cpu")
                        .and_then(|transcriber| transcriber.transcribe(piece))
                    {
                        Ok(transcript) => {
                            cpu_chunks += 1;
                            (transcript, "cpu".to_string())
                        }
                        Err(cpu_error) => {
                            failures.push(format!(
                                "chunk {index}/{} GPU {gpu_error:#}；CPU {cpu_error:#}",
                                pieces.len()
                            ));
                            base += chunk::wav_seconds(piece).unwrap_or(chunk_s as f64);
                            continue;
                        }
                    }
                }
                Err(error) => {
                    failures.push(format!("chunk {index}/{} {error:#}", pieces.len()));
                    base += chunk::wav_seconds(piece).unwrap_or(chunk_s as f64);
                    continue;
                }
            };
            let chunk_ms = chunk_started.elapsed().as_millis() as u64;
            if index == 0 {
                first_chunk_ms = chunk_ms;
            }
            crate::debug_log(&format!(
                "fish asr chunk={index}/{} device={chunk_device} ok wall_ms={chunk_ms} segments={} chars={} audio_s={:.1}",
                pieces.len(),
                transcript.segments.len(),
                transcript::Segment::chars_of(&transcript.segments),
                transcript.audio_seconds
            ));
            load_ms += transcript.load_ms;
            decode_ms += transcript.decode_ms;
            if language.is_empty() {
                language = transcript.language.clone();
                language_probability = transcript.language_probability;
            }
            parts.push((base, transcript.segments));
            base += chunk::wav_seconds(piece).unwrap_or(chunk_s as f64);
        }

        let wall_ms = started.elapsed().as_millis() as u64;
        let _ = std::fs::remove_dir_all(&dir);
        if parts.is_empty() {
            return Err(anyhow!("所有分段都失败：{}", failures.join("；")));
        }
        let segments = chunk::merge(parts);
        if segments.is_empty() {
            return Err(anyhow!(
                "所有分段转写都没有产出句子（可能是纯音乐或静音）"
            ));
        }
        let used_device = if cpu_chunks == 0 {
            device.clone()
        } else if gpu_chunks == 0 {
            "cpu".to_string()
        } else {
            format!("{device}→cpu")
        };
        let note = if cpu_chunks == 0 && failures.is_empty() {
            String::new()
        } else {
            format!(
                "{} 段中 {} 段退回 CPU{}",
                pieces.len(),
                cpu_chunks,
                if failures.is_empty() {
                    String::new()
                } else {
                    format!("，{} 段失败：{}", failures.len(), failures.join("；"))
                }
            )
        };
        let transcript = Transcript {
            segments,
            language,
            language_probability,
            audio_seconds: if total_s > 0.0 { total_s } else { base },
            load_ms,
            decode_ms,
            backend: if availability.engine == "whisper.cpp" {
                "whisper.cpp".to_string()
            } else {
                "faster-whisper".to_string()
            },
        };
        crate::debug_log(&format!(
            "fish asr chunked done chunks={} gpu={gpu_chunks} cpu={cpu_chunks} failed={} wall_ms={wall_ms} first_chunk_ms={first_chunk_ms} device={used_device}",
            pieces.len(),
            failures.len()
        ));
        Ok(LongRun {
            transcript,
            used_device,
            note,
            chunks: pieces.len(),
            gpu_chunks,
            cpu_chunks,
            first_chunk_ms,
            wall_ms,
        })
    }
}

/// A compact reason for the chunk log: `exit<code>`, `timeout`, or the head of
/// the message. `None` never happens; kept as a `String` for formatting.
fn reason_of(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if let Some(position) = text.find("退出码") {
        let rest = &text[position + "退出码".len()..];
        for token in rest.split(|ch: char| !(ch.is_ascii_digit() || ch == '-')) {
            if !token.is_empty() && token != "-" {
                return format!("exit{token}");
            }
        }
    }
    if text.contains("超时") {
        return "timeout".to_string();
    }
    text.trim().chars().take(60).collect()
}

fn monotonic_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

impl Step for AsrStep {
    fn id(&self) -> StepId {
        StepId::Asr
    }


    fn timeout(&self) -> Duration {
        Duration::from_secs(1_800)
    }

    fn skip(&self, config: &WatchConfig) -> Option<SkipReason> {
        if !config.asr.enabled {
            return Some(SkipReason::new("ASR 未开启（设置 → 耳与眼）"));
        }
        if !config.online {
            return Some(SkipReason::new("未登录"));
        }
        let availability = detect(config)?;
        let spec = spec_for(config)?;
        if !models::model_ready(&config.model_dir, spec) {
            return Some(SkipReason::new(format!(
                "模型未下载（{}，{}）",
                spec.id,
                models::human_bytes(spec.total_bytes())
            )));
        }
        let _ = availability;
        None
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a> {
        Box::pin(async move {
            let Some(session) = ctx.session.clone() else {
                return Ok(StepOutput {
                    skip_reason: Some("未登录".to_string()),
                    ..StepOutput::default()
                });
            };
            let Some(availability) = detect(&ctx.config) else {
                return Ok(StepOutput {
                    skip_reason: Some("引擎未安装".to_string()),
                    ..StepOutput::default()
                });
            };
            let spec = match spec_for(&ctx.config) {
                Some(spec) => spec,
                None => {
                    return Ok(StepOutput {
                        skip_reason: Some("未知模型档位".to_string()),
                        ..StepOutput::default()
                    })
                }
            };
            if !models::model_ready(&ctx.config.model_dir, spec) {
                return Ok(StepOutput {
                    skip_reason: Some(format!("模型未下载（{}）", spec.id)),
                    ..StepOutput::default()
                });
            }
            let bvid = ctx.gathered.meta.bvid.clone();
            let cid = ctx.gathered.meta.cid;
            let duration = ctx.gathered.meta.duration_s;

            let audio = match ensure_audio(&ctx.config, &session, &bvid, cid).await {
                Ok(audio) => audio,
                Err(err) => {
                    return Ok(StepOutput {
                        skip_reason: Some(format!("asr_failed: 取音频失败：{err:#}")),
                        ..StepOutput::default()
                    })
                }
            };
            crate::debug_log(&format!(
                "fish asr audio bvid={bvid} cid={cid} path={} bytes={} reused={}",
                audio.path.display(),
                audio.bytes,
                audio.reused as i32
            ));

            // The GPU is shared with the player: wait while anything is playing
            // or buffering. This is the scheduling rule the owner asked for.
            let waited_ms = wait_for_playback(ctx);

            let run = match self.transcribe_long(&availability, &audio.path, ctx) {
                Ok(run) => run,
                Err(err) => {
                    return Ok(StepOutput {
                        skip_reason: Some(format!("asr_failed: {err:#}")),
                        bytes: audio.bytes,
                        ..StepOutput::default()
                    })
                }
            };
            let LongRun {
                transcript,
                used_device,
                note,
                chunks,
                gpu_chunks,
                cpu_chunks,
                first_chunk_ms,
                wall_ms,
            } = run;
            let (count, chars) = transcript::attach_asr(
                &mut ctx.gathered,
                transcript.segments.clone(),
                "asr",
            );
            let covered: f64 = transcript
                .segments
                .iter()
                .map(|segment| (segment.end_s - segment.start_s).max(0.0))
                .sum();
            let coverage = if duration > 0 {
                (covered / duration as f64).min(1.0)
            } else {
                0.0
            };
            let audio_s = if transcript.audio_seconds > 0.0 {
                transcript.audio_seconds
            } else {
                duration as f64
            };
            crate::debug_log(&format!(
                "fish asr done bvid={bvid} engine={} device={} model={} wall_ms={wall_ms} segments={count} chars={chars} coverage={:.2} audio_s={audio_s:.1} lang={} p={:.2} waited_ms={waited_ms} chunks={chunks} gpu_chunks={gpu_chunks} cpu_chunks={cpu_chunks} first_chunk_ms={first_chunk_ms}",
                transcript.backend,
                used_device,
                spec.id,
                coverage,
                transcript.language,
                transcript.language_probability
            ));
            Ok(StepOutput {
                detail: format!(
                    "{} · {} · {} 段 / {} 字 · 音频 {} · {:.1}s",
                    transcript.backend,
                    used_device,
                    count,
                    chars,
                    models::human_bytes(audio.bytes.max(
                        std::fs::metadata(&audio.path).map(|m| m.len()).unwrap_or(0)
                    )),
                    wall_ms as f64 / 1_000.0
                ),
                bytes: audio.bytes,
                note,
                ..StepOutput::default()
            })
        })
    }
}

/// Block the worker until playback stops. Returns how long it waited so the
/// log can prove the scheduling rule fired.
fn wait_for_playback(ctx: &WatchCtx) -> u64 {
    let Some(gate) = ctx.playback_gate.clone() else {
        return 0;
    };
    let started = Instant::now();
    let mut logged = false;
    while gate() {
        if !logged {
            logged = true;
            crate::debug_log("fish asr paused reason=playback");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if logged {
        let waited = started.elapsed().as_millis() as u64;
        crate::debug_log(&format!("fish asr resumed waited_ms={waited}"));
        return waited;
    }
    0
}

/// `Gathered` is where the transcript lands; this helper exists so the unit
/// tests can exercise the step's attachment without a process.
#[cfg(test)]
pub fn attach_test_transcript(gathered: &mut crate::fishwatch::Gathered, segments: Vec<transcript::Segment>) -> usize {
    let (count, _) = transcript::attach_asr(gathered, segments, "asr");
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fishwatch::transcript::Segment;

    #[test]
    fn cache_never_evicts_the_file_just_written() {
        let dir = std::env::temp_dir().join(format!(
            "o11a1-asr-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let keep = dir.join("keep.wav");
        std::fs::write(&keep, vec![0u8; 2_000]).expect("keep");
        trim_cache(&dir, &keep);
        assert!(keep.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playback_gate_waits_until_the_player_stops() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let busy = Arc::new(AtomicBool::new(true));
        let gate_flag = busy.clone();
        let config = WatchConfig::default();
        let mut ctx = WatchCtx::new(config, crate::fishwatch::Meta::default());
        ctx.playback_gate = Some(Arc::new(move || gate_flag.load(Ordering::Relaxed)));
        let release = busy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1_200));
            release.store(false, Ordering::Relaxed);
        });
        let started = Instant::now();
        let waited = wait_for_playback(&ctx);
        assert!(waited >= 1_000, "waited_ms={waited}");
        assert!(started.elapsed() >= Duration::from_millis(1_200));
        assert!(!busy.load(Ordering::Relaxed));
        // Already stopped: no second pause.
        assert_eq!(wait_for_playback(&ctx), 0);
    }

    #[test]
    fn reason_of_reads_the_exit_code_and_timeouts() {
        let exit = anyhow!("faster-whisper 退出码 Some(-1073740791)：boom");
        assert_eq!(reason_of(&exit), "exit-1073740791");
        let timeout = anyhow!("ASR 引擎超时（1500s，已结束进程）");
        assert_eq!(reason_of(&timeout), "timeout");
        let other = anyhow!("模型文件缺失");
        assert_eq!(reason_of(&other), "模型文件缺失");
    }

    #[test]
    fn attach_transcript_sets_source_and_text() {        let mut gathered = Gathered::default();
        let count = attach_test_transcript(
            &mut gathered,
            vec![
                Segment {
                    start_s: 0.0,
                    end_s: 1.0,
                    text: "第一句".to_string(),
                },
                Segment {
                    start_s: 1.0,
                    end_s: 2.0,
                    text: "第二句".to_string(),
                },
            ],
        );
        assert_eq!(count, 2);
        assert_eq!(gathered.transcript_source, "asr");
        assert_eq!(gathered.transcript, "第一句\n第二句");
    }

    #[test]
    fn status_never_invents_an_engine() {
        let config = WatchConfig {
            asr: crate::fishwatch::AsrOptions {
                enabled: true,
                engine: "whisper.cpp".to_string(),
                ..crate::fishwatch::AsrOptions::default()
            },
            ..WatchConfig::default()
        };
        let text = status_text(&config);
        // Either a real binary is installed (text names it) or the text says so.
        assert!(!text.trim().is_empty());
    }

    /// Real network + GPU, opt-in. Needs `BILI_ASR_BVID` and `BILI_ASR_CID` (or
    /// `BILI_ASR_BVID` whose metadata supplies the cid) and the `small` model.
    /// Prints official-subtitle presence, audio bytes, GPU vs CPU wall time,
    /// segments and characters for both.
    #[test]
    #[ignore = "downloads audio, runs Whisper on GPU and CPU; needs BILI_ASR_BVID"]
    fn live_asr_gpu_vs_cpu() {
        let Ok(bvid) = std::env::var("BILI_ASR_BVID") else {
            eprintln!("BILI_ASR_BVID absent: skipped");
            return;
        };
        let root = std::env::var_os("BILI_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/whisper"));
        assert!(
            models::model_ready(&root, &models::FW_SMALL_SPEC),
            "run live_download_small_model first"
        );
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
        let cid = std::env::var("BILI_ASR_CID")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(meta.cid);
        eprintln!(
            "video bvid={bvid} cid={cid} duration_s={} title_len={}",
            meta.duration_s,
            meta.title.chars().count()
        );
        match runtime.block_on(session.conclusion(&bvid, cid, meta.up_mid)) {
            Ok(conclusion) => eprintln!(
                "official subtitle cues={} outline={} data_code={}",
                conclusion.cues.len(),
                conclusion.outline.len(),
                conclusion.data_code
            ),
            Err(err) => eprintln!("conclusion failed: {err:#}"),
        }
        if std::env::var("BILI_ASR_PROBE").as_deref() == Ok("1") {
            return;
        }
        let config = WatchConfig {
            model_dir: root.clone(),
            audio_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("cache/fishaudio"),
            work_dir: std::env::temp_dir().join("o11a1-asr-live"),
            ..WatchConfig::default()
        };
        let audio = runtime
            .block_on(ensure_audio(&config, &session, &bvid, cid))
            .expect("audio");
        let audio_meta = std::fs::metadata(&audio.path).expect("audio file");
        eprintln!(
            "audio path={} downloaded={} wav_bytes={} reused={}",
            audio.path.display(),
            audio.bytes,
            audio_meta.len(),
            audio.reused as i32
        );
        let python = external_fw::find_python().expect("python faster-whisper");
        let make = |device: &str, compute: &str| external_fw::ExternalFw {
            python: python.clone(),
            script: config.audio_dir.join("fw_transcribe.py"),
            model_dir: models::FW_SMALL_SPEC.path(&root),
            device: device.to_string(),
            compute: compute.to_string(),
            threads: 4,
            work_dir: config.work_dir.clone(),
        };
        let gpu = make("cuda", "float16");
        let started = Instant::now();
        let gpu_result = gpu.transcribe(&audio.path);
        let gpu_ms = started.elapsed().as_millis();
        let cpu = make("cpu", "int8");
        let started = Instant::now();
        let cpu_result = cpu.transcribe(&audio.path);
        let cpu_ms = started.elapsed().as_millis();
        match (&gpu_result, &cpu_result) {
            (Ok(gpu), Ok(cpu)) => eprintln!(
                "gpu_ms={gpu_ms} cpu_ms={cpu_ms} ratio={:.2}x gpu_segments={} cpu_segments={} gpu_chars={} cpu_chars={} lang={}",
                cpu_ms as f64 / gpu_ms.max(1) as f64,
                gpu.segments.len(),
                cpu.segments.len(),
                gpu.segments.iter().map(|s| s.text.chars().count()).sum::<usize>(),
                cpu.segments.iter().map(|s| s.text.chars().count()).sum::<usize>(),
                gpu.language
            ),
            _ => eprintln!("gpu={gpu_result:?} cpu={cpu_result:?}"),
        }
        let gpu = gpu_result.expect("gpu transcription");
        assert!(!gpu.segments.is_empty());
        let text: String = gpu
            .segments
            .iter()
            .take(3)
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        eprintln!("gpu head: {text}");
    }

    /// Real audio + GPU, opt-in: the long-audio path, end to end.
    ///
    /// * `BILI_ASR_WAV` — use this 16 kHz mono WAV (no network);
    /// * otherwise `BILI_ASR_BVID` (+ optional `BILI_ASR_CID`) is downloaded and
    ///   normalised by [`ensure_audio`], exactly like the app does.
    ///
    /// `BILI_ASR_ENGINE` picks `faster-whisper` (default) or `whisper.cpp`.
    /// Prints the numbers the acceptance report needs: chunks, GPU/CPU counts,
    /// total wall time, first-chunk wall time, segments, characters, coverage.
    #[test]
    #[ignore = "downloads audio and runs the engine on the GPU; needs BILI_ASR_WAV or BILI_ASR_BVID"]
    fn live_long_audio_chunked() {
        let root = std::env::var_os("BILI_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/whisper"));
        let engine = std::env::var("BILI_ASR_ENGINE").unwrap_or_else(|_| "faster-whisper".to_string());
        let config = WatchConfig {
            model_dir: root.clone(),
            audio_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("cache/fishaudio"),
            work_dir: std::env::temp_dir().join("o11a1-asr-live"),
            asr: crate::fishwatch::AsrOptions {
                enabled: true,
                engine: engine.clone(),
                device: "gpu".to_string(),
                model: "small".to_string(),
                ..crate::fishwatch::AsrOptions::default()
            },
            ..WatchConfig::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let audio = match std::env::var_os("BILI_ASR_WAV") {
            Some(path) => PathBuf::from(path),
            None => {
                let bvid = std::env::var("BILI_ASR_BVID").expect("BILI_ASR_WAV or BILI_ASR_BVID");
                let token = crate::bilibili::load_token().expect("token.json");
                let session = runtime
                    .block_on(crate::bilibili::Session::from_token(&token))
                    .expect("session");
                let meta = runtime
                    .block_on(session.fish_meta(&bvid))
                    .expect("fish meta");
                let cid = std::env::var("BILI_ASR_CID")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(meta.cid);
                runtime
                    .block_on(ensure_audio(&config, &session, &bvid, cid))
                    .expect("audio")
                    .path
            }
        };
        let seconds = chunk::wav_seconds(&audio).expect("wav duration");
        eprintln!(
            "audio={} seconds={seconds:.1} chunk_s={} engine={engine}",
            audio.display(),
            chunk::chunk_seconds()
        );
        let availability = detect(&config).expect("engine available");
        eprintln!(
            "engine={} device={} detail={}",
            availability.engine, availability.device, availability.detail
        );
        let step = AsrStep::new(&config);
        let ctx = WatchCtx::new(config.clone(), crate::fishwatch::Meta::default());
        let run = step
            .transcribe_long(&availability, &audio, &ctx)
            .expect("transcription");
        let covered: f64 = run
            .transcript
            .segments
            .iter()
            .map(|segment| (segment.end_s - segment.start_s).max(0.0))
            .sum();
        let chars = transcript::Segment::chars_of(&run.transcript.segments);
        eprintln!(
            "RESULT chunks={} gpu_chunks={} cpu_chunks={} wall_ms={} first_chunk_ms={} segments={} chars={} coverage={:.3} device={} lang={} note={}",
            run.chunks,
            run.gpu_chunks,
            run.cpu_chunks,
            run.wall_ms,
            run.first_chunk_ms,
            run.transcript.segments.len(),
            chars,
            (covered / seconds).min(1.0),
            run.used_device,
            run.transcript.language,
            run.note
        );
        let head: Vec<&str> = run
            .transcript
            .segments
            .iter()
            .take(4)
            .map(|segment| segment.text.as_str())
            .collect();
        eprintln!("head: {}", head.join(" / "));
        assert!(!run.transcript.segments.is_empty());
    }
}

