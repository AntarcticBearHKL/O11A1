//! 语音识别模型（whisper.cpp GGML）的本地状态、下载、断点续传与删除。
//!
//! 发行包不再带模型（文件夹包保持 254 MB），模型改为在「设置 → 语音识别模型」
//! 里按需下载。本模块只做三件事，全部围绕磁盘上真实的文件：
//!
//! * **状态**：[`list`] 对 tiny/base/small/medium 逐个 `stat`
//!   `<模型目录>/whisper.cpp/ggml-<name>.bin`，体积与 [`ModelInfo::bytes`]
//!   相差 ±1% 以内即算已安装（上游偶尔重新量化，差百分之几是正常的）。
//! * **下载**：[`download`] 流式写入 `<file>.part`，有 `.part` 就用
//!   `Range: bytes=N-` 续传，服务器不支持就从头重下；完成后 `rename` 成正式
//!   文件名并校验体积。失败或取消都保留 `.part`，下一次从断点继续。
//! * **删除**：[`delete`] 只删这一个模型的文件（正式文件与 `.part`）。
//!
//! 源站：`settings.json` 的 `fish_model_mirror` 选 `auto` / `official` /
//! `mirror`。`auto` 先走官方，连续 3 次失败或 10 秒没有任何新字节就切镜像。
//!
//! 这个模块不认识 UI：进度通过 [`Event`] 回调交给调用方（`app::models_ui`
//! 负责在事件循环里发布），日志写 `debug_log`，一行一个事实。

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;

/// 一个可下载的 whisper.cpp 模型。
#[derive(Clone, Copy, Debug)]
pub struct ModelInfo {
    /// 档位名，也是设置里的值：`tiny` / `base` / `small` / `medium`。
    pub name: &'static str,
    /// 官方文件名，`models/whisper/whisper.cpp/<file>`。
    pub file: &'static str,
    /// 官方 `Content-Length`（字节）。断点续传的百分比以它为准。
    pub bytes: u64,
}

impl ModelInfo {
    /// 官方仓库里的相对路径。
    pub fn rel_path(&self) -> String {
        format!("ggerganov/whisper.cpp/resolve/main/{}", self.file)
    }

    pub fn size_text(&self) -> String {
        size_text(self.bytes)
    }
}

/// 设置页里的四档，按体积从小到大。
///
/// 体积取自 Hugging Face 上 `ggerganov/whisper.cpp` 的 LFS 元数据
/// （`ggml-tiny.bin` 74 MB / `ggml-base.bin` 141 MB / `ggml-small.bin`
/// 466 MB / `ggml-medium.bin` 1.5 GB），不是估算值：进度条的百分比、
/// 下载完的体积校验和这一行文案用的是同一个数字。
pub const MODELS: [ModelInfo; 4] = [
    ModelInfo {
        name: "tiny",
        file: "ggml-tiny.bin",
        bytes: 77_691_713,
    },
    ModelInfo {
        name: "base",
        file: "ggml-base.bin",
        bytes: 147_951_465,
    },
    ModelInfo {
        name: "small",
        file: "ggml-small.bin",
        bytes: 487_601_967,
    },
    ModelInfo {
        name: "medium",
        file: "ggml-medium.bin",
        bytes: 1_533_763_059,
    },
];

/// 官方站与国内镜像（整条 URL 只差主机名）。
pub const OFFICIAL_HOST: &str = "https://huggingface.co";
pub const MIRROR_HOST: &str = "https://hf-mirror.com";

/// 体积容差：±1%。上游重新量化会改体积，但不该是成倍的差别。
pub const SIZE_TOLERANCE: f64 = 0.01;

/// 一次读取最多给 10 秒（不是整个请求）：太久没有新字节就当这一路卡死。
const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// 同一源站连续失败多少次之后切镜像（`auto` 模式）。
const FAILURES_BEFORE_SWITCH: u32 = 3;

/// 进度回报的最小间隔；界面 1 Hz 就够。
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

pub fn info(name: &str) -> Option<&'static ModelInfo> {
    let name = name.trim();
    MODELS.iter().find(|model| model.name == name)
}

/// 模型根目录：`settings.json` 的 `fish_asr_model_dir`，空串 = 项目根下的
/// `models/whisper`（与 [`crate::fish::watch_config`] 完全一致）。
pub fn root_dir(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if configured.is_empty() {
        crate::paths::root().join("models/whisper")
    } else {
        PathBuf::from(configured)
    }
}

/// whisper.cpp 的模型子目录（GGML 文件都放这里）。
pub fn cpp_dir(root: &Path) -> PathBuf {
    root.join("whisper.cpp")
}

pub fn model_path(root: &Path, model: &ModelInfo) -> PathBuf {
    cpp_dir(root).join(model.file)
}

pub fn part_path(root: &Path, model: &ModelInfo) -> PathBuf {
    cpp_dir(root).join(format!("{}.part", model.file))
}

/// 体积是否落在 [`SIZE_TOLERANCE`] 内。
pub fn size_ok(have: u64, expected: u64) -> bool {
    if expected == 0 {
        return false;
    }
    let delta = have as f64 - expected as f64;
    delta.abs() <= expected as f64 * SIZE_TOLERANCE
}

/// 磁盘上的真实状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// 没有正式文件，也没有 `.part`。
    Missing,
    /// 只有 `.part`，`bytes` 是已经下到的字节数（断点）。
    Partial,
    /// 正式文件在，体积对。
    Installed,
}

#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub model: ModelInfo,
    pub state: State,
    /// 已下到的字节数（`Installed` 时是文件体积）。
    pub have: u64,
}

impl Status {
    pub fn total(&self) -> u64 {
        self.model.bytes
    }

    /// 下载中时这一行的百分比（0–100），否则 `None`。
    pub fn pct(&self) -> Option<u32> {
        job_progress(self.model.name).map(|(pct, _)| pct)
    }

    pub fn speed(&self) -> Option<f64> {
        job_progress(self.model.name).map(|(_, speed)| speed)
    }

    pub fn downloading(&self) -> bool {
        job_progress(self.model.name).is_some()
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// 一个模型的磁盘状态。只 `stat`，不读内容、不联网。
pub fn status(root: &Path, model: &ModelInfo) -> Status {
    let final_path = model_path(root, model);
    let have = file_len(&final_path);
    if have > 0 && size_ok(have, model.bytes) {
        return Status {
            model: *model,
            state: State::Installed,
            have,
        };
    }
    let part = file_len(&part_path(root, model));
    if part > 0 {
        return Status {
            model: *model,
            state: State::Partial,
            have: part,
        };
    }
    Status {
        model: *model,
        state: State::Missing,
        have: 0,
    }
}

/// 四档一起读（设置页一次刷新用）。
pub fn list(root: &Path) -> Vec<Status> {
    MODELS.iter().map(|model| status(root, model)).collect()
}

/// 已安装里最大的一档：设置里存的档位没装时，用它当「当前使用」。
pub fn largest_installed(root: &Path) -> Option<&'static ModelInfo> {
    MODELS
        .iter()
        .filter(|model| status(root, model).state == State::Installed)
        .max_by_key(|model| model.bytes)
}

// ---------------------------------------------------------------------------
// 正在进行的那一个下载（只有一条：设置页一次只下一档）
// ---------------------------------------------------------------------------

struct Job {
    name: &'static str,
    done: u64,
    total: u64,
    speed: f64,
    cancel: Arc<AtomicBool>,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);

fn job_progress(name: &str) -> Option<(u32, f64)> {
    let slot = JOB.lock().ok()?;
    let job = slot.as_ref()?;
    if job.name != name {
        return None;
    }
    let pct = if job.total == 0 {
        0
    } else {
        ((job.done as f64 / job.total as f64) * 100.0).round() as u32
    };
    Some((pct.min(100), job.speed))
}

/// 正在下载哪一档（`None` = 空闲）。
pub fn running() -> Option<&'static str> {
    JOB.lock().ok()?.as_ref().map(|job| job.name)
}

/// 取消正在进行的下载；返回是否真的发了取消。
///
/// 只是一个原子标志：下载线程在下一个 chunk 退出，已经写进 `.part` 的字节
/// 全部保留，下一次点「下载」就是断点续传。
pub fn cancel() -> bool {
    let slot = match JOB.lock() {
        Ok(slot) => slot,
        Err(_) => return false,
    };
    match slot.as_ref() {
        Some(job) => {
            job.cancel.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

/// 删除一个模型的正式文件与 `.part`，返回删掉的字节数。
pub fn delete(root: &Path, name: &str) -> Result<u64> {
    let model = info(name).ok_or_else(|| anyhow!("未知模型 {name}"))?;
    if running() == Some(model.name) {
        return Err(anyhow!("正在下载，先取消再删除"));
    }
    let mut freed = 0;
    for path in [model_path(root, model), part_path(root, model)] {
        let len = file_len(&path);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("删除失败：{}", path.display()))?;
            freed += len;
        }
    }
    crate::debug_log(&format!(
        "models dl delete name={} bytes={freed}",
        model.name
    ));
    Ok(freed)
}

// ---------------------------------------------------------------------------
// 源站
// ---------------------------------------------------------------------------

/// `fish_model_mirror` 的三个值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mirror {
    /// 先官方，连续失败或卡住就切镜像。
    Auto,
    Official,
    Mirror,
}

impl Mirror {
    pub fn from_setting(value: &str) -> Self {
        match value.trim() {
            "official" => Mirror::Official,
            "mirror" => Mirror::Mirror,
            _ => Mirror::Auto,
        }
    }

    /// `auto` 的尝试顺序；单选时只有一段。
    fn hosts(self) -> Vec<&'static str> {
        match self {
            Mirror::Auto => vec![OFFICIAL_HOST, MIRROR_HOST],
            Mirror::Official => vec![OFFICIAL_HOST],
            Mirror::Mirror => vec![MIRROR_HOST],
        }
    }
}

fn host_name(host: &str) -> &'static str {
    if host == MIRROR_HOST {
        "mirror"
    } else {
        "official"
    }
}

// ---------------------------------------------------------------------------
// 下载
// ---------------------------------------------------------------------------

/// 下载进度的一次回报。数值都在 [`list`] 里（1 Hz 重画一次整页）。
///
/// 没有 `Started` 变体：开头的 `models dl start` 由本模块直接写日志，
/// 界面在第一次 [`Event::Progress`] 时重画就够了。
#[derive(Clone, Debug)]
pub enum Event {
    /// 到了 1 Hz 的回报点。
    Progress,
    Done { bytes: u64 },
    /// 用户取消（或窗口关了）：`.part` 保留。
    Paused { kept: u64 },
    Failed { error: String },
}

/// 一次下载的结局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 文件已经在磁盘上并且体积正确。
    Done { bytes: u64 },
    /// 取消，`.part` 保留在断点。
    Paused { kept: u64 },
}

impl Outcome {
    pub fn paused(&self) -> bool {
        matches!(self, Outcome::Paused { .. })
    }
}

/// `3.2 MB/s` / `480 KB/s`。
pub fn speed_text(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "0 KB/s".to_string();
    }
    if bytes_per_second >= 1_048_576.0 {
        format!("{:.1} MB/s", bytes_per_second / 1_048_576.0)
    } else {
        format!("{:.0} KB/s", bytes_per_second / 1_024.0)
    }
}

/// `466 MB` / `1.5 GB`（与 `fishwatch::asr::models::human_bytes` 同一套口径）。
pub fn size_text(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.0} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1_024.0)
    }
}

/// 上一次断掉的那一路：取消、卡住、还是普通失败。
enum Failure {
    Cancel,
    /// 10 秒没有新字节。
    Stalled,
    Failed(String),
}

impl Failure {
    fn text(&self) -> String {
        match self {
            Failure::Cancel => "已取消".to_string(),
            Failure::Stalled => "10 秒没有新数据".to_string(),
            Failure::Failed(reason) => reason.clone(),
        }
    }
}

/// 下载一档模型。阻塞；在调用方自己的线程里跑（`app::models_ui` 开一个）。
///
/// 完成时返回 [`Outcome::Done`]；用户取消返回 [`Outcome::Paused`]，`.part`
/// 保留。两种情况下 [`Event`] 都已经把最后状态发出去，日志也已经写下。
pub fn download(
    root: &Path,
    name: &str,
    mirror: Mirror,
    cancel: Arc<AtomicBool>,
    sink: &mut dyn FnMut(Event),
) -> Result<Outcome> {
    let model = info(name).ok_or_else(|| anyhow!("未知模型 {name}"))?;
    let dir = cpp_dir(root);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建模型目录失败：{}", dir.display()))?;
    let final_path = model_path(root, model);
    let part = part_path(root, model);
    let total = model.bytes;

    // 已经在磁盘上：不联网，直接报完成。
    let have = file_len(&final_path);
    if have > 0 && size_ok(have, total) {
        crate::debug_log(&format!(
            "models dl done name={} bytes={have} cached=1",
            model.name
        ));
        sink(Event::Done { bytes: have });
        return Ok(Outcome::Done { bytes: have });
    }

    // 上次写完了但没来得及 rename。
    let partial = file_len(&part);
    if partial > 0 && size_ok(partial, total) {
        std::fs::rename(&part, &final_path)
            .with_context(|| format!("重命名失败：{}", final_path.display()))?;
        crate::debug_log(&format!(
            "models dl resume from={partial} name={} complete=1",
            model.name
        ));
        crate::debug_log(&format!(
            "models dl done name={} bytes={partial}",
            model.name
        ));
        sink(Event::Done { bytes: partial });
        return Ok(Outcome::Done { bytes: partial });
    }

    let runtime = crate::bilibili::shared_runtime()
        .ok_or_else(|| anyhow!("网络运行时不可用"))?;
    let client = reqwest::Client::builder()
        .user_agent(crate::bilibili::UA)
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .gzip(false)
        .brotli(false)
        .build()
        .context("创建下载客户端失败")?;

    let hosts = mirror.hosts();
    let mut host_index = 0usize;
    let mut failures: u32 = 0;
    let mut last_reason = String::new();
    let mut started_any = false;

    while host_index < hosts.len() {
        let host = hosts[host_index];
        let source = host_name(host);
        if !started_any {
            crate::debug_log(&format!(
                "models dl start name={} source={source}",
                model.name
            ));
            started_any = true;
        }
        if let Ok(mut slot) = JOB.lock() {
            if slot.is_none() {
                *slot = Some(Job {
                    name: model.name,
                    done: file_len(&part),
                    total,
                    speed: 0.0,
                    cancel: cancel.clone(),
                });
            }
        }

        let url = format!("{host}/{}", model.rel_path());
        let attempt = runtime.block_on(attempt_once(
            &client,
            &url,
            &part,
            total,
            &cancel,
            &mut |done, speed| {
                if let Ok(mut slot) = JOB.lock() {
                    if let Some(job) = slot.as_mut() {
                        job.done = done;
                        job.speed = speed;
                    }
                }
                let pct = if total == 0 {
                    0
                } else {
                    ((done as f64 / total as f64) * 100.0).round() as u32
                };
                sink(Event::Progress);
                crate::debug_log(&format!(
                    "models dl progress pct={} speed={}",
                    pct.min(100),
                    speed_text(speed)
                ));
            },
        ));

        match attempt {
            Ok(bytes) => {
                if let Ok(mut slot) = JOB.lock() {
                    slot.take();
                }
                if !size_ok(bytes, total) {
                    let reason = format!(
                        "体积不对：{} != {}",
                        size_text(bytes),
                        size_text(total)
                    );
                    crate::debug_log(&format!(
                        "models dl failed name={} err={reason}",
                        model.name
                    ));
                    sink(Event::Failed {
                        error: reason.clone(),
                    });
                    return Err(anyhow!(reason));
                }
                std::fs::rename(&part, &final_path)
                    .with_context(|| format!("重命名失败：{}", final_path.display()))?;
                crate::debug_log(&format!(
                    "models dl done name={} bytes={bytes}",
                    model.name
                ));
                sink(Event::Done { bytes });
                return Ok(Outcome::Done { bytes });
            }
            Err(Failure::Cancel) => {
                let kept = file_len(&part);
                if let Ok(mut slot) = JOB.lock() {
                    slot.take();
                }
                crate::debug_log(&format!(
                    "models dl cancel name={} kept={kept}",
                    model.name
                ));
                sink(Event::Paused { kept });
                return Ok(Outcome::Paused { kept });
            }
            Err(failure) => {
                last_reason = failure.text();
                failures += 1;
                crate::debug_log(&format!(
                    "models dl attempt failed name={} source={source} n={failures} err={last_reason}",
                    model.name
                ));
                let switch = match failure {
                    Failure::Stalled => mirror == Mirror::Auto,
                    _ => failures >= FAILURES_BEFORE_SWITCH,
                };
                if switch && host_index + 1 < hosts.len() {
                    host_index += 1;
                    failures = 0;
                    crate::debug_log(&format!(
                        "models dl source switch from={source} to={}",
                        host_name(hosts[host_index])
                    ));
                } else if !switch {
                    // 同一路再试一次：`.part` 里有断点，续传不浪费。
                } else {
                    break;
                }
            }
        }
    }

    if let Ok(mut slot) = JOB.lock() {
        slot.take();
    }
    let reason = if last_reason.is_empty() {
        "下载失败".to_string()
    } else {
        last_reason
    };
    crate::debug_log(&format!(
        "models dl failed name={} err={reason}",
        model.name
    ));
    sink(Event::Failed {
        error: reason.clone(),
    });
    Err(anyhow!(reason))
}

/// 一次 HTTP 尝试：从 `.part` 的当前长度续传，读到 EOF 或出错为止。
///
/// 返回写完之后 `.part` 的字节数。这个数字由调用方按 [`SIZE_TOLERANCE`] 校验。
async fn attempt_once(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    total: u64,
    cancel: &AtomicBool,
    on_tick: &mut dyn FnMut(u64, f64),
) -> std::result::Result<u64, Failure> {
    let mut have = file_len(part);
    if have > 0 && !size_ok(have, total) && have > total {
        // 比官方文件还大：只可能是另一半写坏了，从头来。
        let _ = std::fs::remove_file(part);
        have = 0;
    }

    let mut request = client.get(url);
    if have > 0 {
        request = request.header(
            reqwest::header::RANGE,
            format!("bytes={have}-"),
        );
    }
    let response = request
        .send()
        .await
        .map_err(|err| Failure::Failed(format!("请求失败：{err}")))?;
    let status = response.status();
    let appending = status == reqwest::StatusCode::PARTIAL_CONTENT;
    if !status.is_success() {
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && have > 0 {
            // `.part` 已经覆盖整个文件，交给调用方按体积收尾。
            return Ok(have);
        }
        return Err(Failure::Failed(format!("HTTP {}", status.as_u16())));
    }
    if have > 0 && !appending {
        // 服务器不认 Range（回了 200）：这一路只能从头写。
        have = 0;
    } else if have > 0 {
        crate::debug_log(&format!("models dl resume from={have}"));
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(appending)
        .truncate(!appending)
        .open(part)
        .map_err(|err| Failure::Failed(format!("打开临时文件失败：{err}")))?;
    if !appending {
        let _ = file.seek(SeekFrom::Start(0));
    }

    let mut stream = response.bytes_stream();
    let mut window_bytes = have;
    let mut window_start = Instant::now();
    let mut last_report = Instant::now() - PROGRESS_INTERVAL;
    let mut speed = 0.0f64;

    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = file.flush();
            return Err(Failure::Cancel);
        }
        let chunk = tokio::time::timeout(STALL_TIMEOUT, stream.next()).await;
        match chunk {
            Err(_) => {
                let _ = file.flush();
                return Err(Failure::Stalled);
            }
            Ok(None) => break,
            Ok(Some(Err(err))) => {
                let _ = file.flush();
                return Err(Failure::Failed(format!("读取失败：{err}")));
            }
            Ok(Some(Ok(bytes))) => {
                if bytes.is_empty() {
                    continue;
                }
                if let Err(err) = file.write_all(&bytes) {
                    return Err(Failure::Failed(format!("写入失败：{err}")));
                }
                have += bytes.len() as u64;
                if last_report.elapsed() >= PROGRESS_INTERVAL {
                    let elapsed = window_start.elapsed().as_secs_f64();
                    if elapsed > 0.05 {
                        speed = (have.saturating_sub(window_bytes)) as f64 / elapsed;
                    }
                    window_bytes = have;
                    window_start = Instant::now();
                    last_report = Instant::now();
                    on_tick(have, speed);
                }
            }
        }
    }
    let _ = file.flush();
    Ok(have)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "o11a1-models-dl-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_four_tiers_are_the_shipped_order() {
        let names: Vec<&str> = MODELS.iter().map(|model| model.name).collect();
        assert_eq!(names, vec!["tiny", "base", "small", "medium"]);
        // Sizes must match the real whisper.cpp LFS objects (466 MB is the
        // one the folder package used to carry).
        assert_eq!(info("small").map(|model| model.bytes), Some(487_601_967));
        assert_eq!(info("medium").map(|model| model.bytes), Some(1_533_763_059));
        assert_eq!(info("small").map(|model| model.file), Some("ggml-small.bin"));
        assert!(info("large").is_none());
        for model in MODELS {
            assert_eq!(model.size_text(), size_text(model.bytes));
            assert!(model.rel_path().ends_with(model.file));
        }
    }

    #[test]
    fn size_tolerance_is_one_percent_both_ways() {
        let total = 487_601_967u64;
        assert!(size_ok(total, total));
        assert!(size_ok(total - total / 200, total));
        assert!(size_ok(total + total / 200, total));
        assert!(!size_ok(total - total / 50, total));
        assert!(!size_ok(total / 2, total));
        assert!(!size_ok(0, total));
        assert!(!size_ok(10, 0));
    }

    #[test]
    fn status_reads_the_part_file_as_a_resume_point() {
        let dir = temp_dir("status");
        let small = info("small").unwrap();
        assert_eq!(status(&dir, small).state, State::Missing);
        let cpp = cpp_dir(&dir);
        std::fs::create_dir_all(&cpp).unwrap();
        std::fs::write(part_path(&dir, small), vec![0u8; 1024]).unwrap();
        let partial = status(&dir, small);
        assert_eq!(partial.state, State::Partial);
        assert_eq!(partial.have, 1024);
        // A file at the right size is installed, and `largest_installed` sees it.
        std::fs::write(model_path(&dir, small), vec![0u8; 16]).unwrap();
        std::fs::write(part_path(&dir, small), Vec::<u8>::new()).unwrap();
        let tuned = ModelInfo {
            bytes: 16,
            ..*small
        };
        assert_eq!(status(&dir, &tuned).state, State::Installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_reports_every_tier_and_the_largest_installed() {
        let dir = temp_dir("list");
        let rows = list(&dir);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row.state == State::Missing));
        assert!(largest_installed(&dir).is_none());
        let base = cpp_dir(&dir);
        std::fs::create_dir_all(&base).unwrap();
        // tiny: a file at exactly its size -> installed; base: absent.
        let tiny = info("tiny").unwrap();
        let file = std::fs::File::create(model_path(&dir, tiny)).unwrap();
        file.set_len(tiny.bytes).unwrap();
        drop(file);
        let rows = list(&dir);
        assert_eq!(rows[0].state, State::Installed);
        assert_eq!(rows[1].state, State::Missing);
        assert_eq!(largest_installed(&dir).map(|model| model.name), Some("tiny"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mirror_names_and_speed_text_read_like_the_ui() {
        assert_eq!(Mirror::from_setting("official"), Mirror::Official);
        assert_eq!(Mirror::from_setting("mirror"), Mirror::Mirror);
        assert_eq!(Mirror::from_setting("auto"), Mirror::Auto);
        assert_eq!(Mirror::from_setting("nonsense"), Mirror::Auto);
        assert_eq!(host_name(OFFICIAL_HOST), "official");
        assert_eq!(host_name(MIRROR_HOST), "mirror");
        assert_eq!(speed_text(3.2 * 1_048_576.0), "3.2 MB/s");
        assert_eq!(speed_text(480.0 * 1_024.0), "480 KB/s");
        assert_eq!(speed_text(0.0), "0 KB/s");
        assert_eq!(size_text(487_601_967), "465 MB");
    }

    #[test]
    fn delete_removes_both_the_model_and_its_part() {
        let dir = temp_dir("delete");
        let small = info("small").unwrap();
        let cpp = cpp_dir(&dir);
        std::fs::create_dir_all(&cpp).unwrap();
        std::fs::write(part_path(&dir, small), vec![0u8; 100]).unwrap();
        let freed = delete(&dir, "small").expect("delete");
        assert_eq!(freed, 100);
        assert!(!part_path(&dir, small).exists());
        assert!(!model_path(&dir, small).exists());
        assert!(delete(&dir, "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_only_sets_the_flag_and_keeps_the_part_file() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!cancel(), "no job, nothing to cancel");
        *JOB.lock().unwrap() = Some(Job {
            name: "small",
            done: 10,
            total: 100,
            speed: 1.0,
            cancel: flag.clone(),
        });
        assert_eq!(running(), Some("small"));
        assert_eq!(job_progress("small"), Some((10, 1.0)));
        assert_eq!(job_progress("tiny"), None);
        assert!(cancel());
        assert!(flag.load(Ordering::SeqCst));
        // `cancel` is only the flag: the job is still the caller's to clear.
        *JOB.lock().unwrap() = None;
        assert!(running().is_none());
    }
}
