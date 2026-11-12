//! Offline downloads: resolve DASH tracks, run ffmpeg `-c copy` into MP4, and
//! report progress through the polled `downloads.list` RPC.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

use o11a1::bilibili::{PlayInfo, Session};
use o11a1::settings::Settings;
use serde_json::{json, Value};

use crate::{e, text, unum, Rpc};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One process-wide job object with KILL_ON_JOB_CLOSE. Every ffmpeg child is
/// assigned to it, so an engine crash or a hard kill cannot leave an orphan
/// merge process behind.
fn download_job() -> Option<HANDLE> {
    static JOB: OnceLock<Option<isize>> = OnceLock::new();
    let stored = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(job.0 as isize)
    });
    stored.map(|value| HANDLE(value as *mut _))
}

fn assign_to_job(child: &Child) {
    let Some(job) = download_job() else {
        return;
    };
    unsafe {
        let Ok(process) = OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE,
            false,
            child.id(),
        ) else {
            return;
        };
        let _ = AssignProcessToJobObject(job, process);
        let _ = CloseHandle(process);
    }
}

#[derive(Clone)]
struct Task {
    id: String,
    bvid: String,
    cid: u64,
    title: String,
    cover: String,
    status: String,
    progress: f64,
    speed: String,
    path: String,
    error: String,
    created: u64,
    cancel: Arc<AtomicBool>,
}

struct Pending {
    id: String,
    video_urls: Vec<String>,
    audio_urls: Vec<String>,
    headers: Vec<(String, String)>,
    path: PathBuf,
    duration: u64,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    tasks: Vec<Task>,
    queue: VecDeque<Pending>,
    active: usize,
}

/// At most two ffmpeg merges at a time; the rest wait in `queue`.
const MAX_ACTIVE: usize = 2;

pub struct Downloads {
    inner: Arc<Mutex<Inner>>,
    seq: Mutex<u64>,
}

impl Downloads {
    pub fn new() -> Self {
        let tasks = load_tasks();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                tasks,
                queue: VecDeque::new(),
                active: 0,
            })),
            seq: Mutex::new(1),
        }
    }

    pub fn list(&self) -> Value {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        json!(guard.tasks.iter().map(task_json).collect::<Vec<_>>())
    }

    pub async fn create(
        &self,
        session: Arc<Session>,
        params: &Value,
        settings: &Settings,
    ) -> Rpc<Value> {
        let bvid = text(params, "bvid");
        let cid = unum(params, "cid");
        if bvid.is_empty() || cid == 0 {
            return Err("下载缺少 bvid/cid".to_string());
        }
        let title = text(params, "title");
        let cover = text(params, "cover");
        let play: PlayInfo = session.playurl(&bvid, cid).await.map_err(e)?;
        let track = pick_video(&play, settings.player_quality)
            .ok_or_else(|| "没有可下载的视频轨道".to_string())?;
        let audio = play
            .audio_tracks
            .first()
            .ok_or_else(|| "没有可下载的音频轨道".to_string())?;
        let duration = play.duration;
        let urls = track.urls.clone();
        let audio_urls = audio.urls.clone();
        let headers = session.media_headers();
        let path = output_path(&title, &bvid);
        let id = {
            let mut seq = self.seq.lock().unwrap_or_else(|p| p.into_inner());
            let id = format!("dl-{}-{}", now(), *seq);
            *seq += 1;
            id
        };
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.tasks.insert(
                0,
                Task {
                    id: id.clone(),
                    bvid: bvid.clone(),
                    cid,
                    title: title.clone(),
                    cover: cover.clone(),
                    status: "queued".to_string(),
                    progress: 0.0,
                    speed: String::new(),
                    path: path.display().to_string(),
                    error: String::new(),
                    created: now(),
                    cancel: cancel.clone(),
                },
            );
            save_tasks(&guard.tasks);
        }
        let inner = self.inner.clone();
        {
            let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.queue.push_back(Pending {
                id: id.clone(),
                video_urls: urls,
                audio_urls,
                headers,
                path,
                duration,
                cancel,
            });
        }
        start_pending(inner);
        Ok(json!(id))
    }

    pub fn cancel(&self, id: &str) {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(task) = guard.tasks.iter().find(|task| task.id == id) {
            task.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn remove(&self, id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(position) = guard.tasks.iter().position(|task| task.id == id) {
            let task = guard.tasks.remove(position);
            task.cancel.store(true, Ordering::SeqCst);
            let path = PathBuf::from(&task.path);
            if path.is_file() {
                let _ = std::fs::remove_file(path);
            }
            let part = part_path(&PathBuf::from(&task.path));
            if part.is_file() {
                let _ = std::fs::remove_file(part);
            }
            save_tasks(&guard.tasks);
        }
    }

    pub fn open_folder(&self, path: &str) {
        if path.is_empty() {
            return;
        }
        let path = PathBuf::from(path);
        let dir = if path.is_file() {
            path.parent().map(Path::to_path_buf)
        } else {
            Some(path)
        };
        let Some(dir) = dir else { return };
        let _ = Command::new("explorer.exe")
            .arg(dir)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}

fn pick_video(play: &PlayInfo, quality: i64) -> Option<&o11a1::bilibili::VideoTrack> {
    let mut tracks: Vec<&o11a1::bilibili::VideoTrack> = play.video_tracks.iter().collect();
    if quality > 0 {
        if let Some(track) = tracks.iter().find(|track| track.id == quality) {
            return Some(track);
        }
    }
    tracks.sort_by_key(|track| (track.height, track.bandwidth, (track.codecid == 7) as u8));
    tracks.pop()
}

fn start_pending(inner: Arc<Mutex<Inner>>) {
    loop {
        let job = {
            let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
            if guard.active >= MAX_ACTIVE {
                return;
            }
            let Some(job) = guard.queue.pop_front() else {
                return;
            };
            guard.active += 1;
            job
        };
        let thread_inner = inner.clone();
        let id = job.id.clone();
        if std::thread::Builder::new()
            .name("o11a1-download".into())
            .spawn(move || {
                run_task(
                    thread_inner.clone(),
                    job.id,
                    job.video_urls,
                    job.audio_urls,
                    job.headers,
                    job.path,
                    job.duration,
                    job.cancel,
                );
                let mut guard = thread_inner.lock().unwrap_or_else(|p| p.into_inner());
                guard.active = guard.active.saturating_sub(1);
                drop(guard);
                start_pending(thread_inner);
            })
            .is_err()
        {
            set_status(&inner, &id, "error", 0.0, "", "无法启动下载线程");
            let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.active = guard.active.saturating_sub(1);
        }
    }
}

fn run_task(
    inner: Arc<Mutex<Inner>>,
    id: String,
    video_urls: Vec<String>,
    audio_urls: Vec<String>,
    headers: Vec<(String, String)>,
    path: PathBuf,
    duration: u64,
    cancel: Arc<AtomicBool>,
) {
    set_status(&inner, &id, "running", 0.0, "", "");
    let Some(video) = video_urls.first() else {
        set_status(&inner, &id, "error", 0.0, "", "没有视频地址");
        return;
    };
    let Some(audio) = audio_urls.first() else {
        set_status(&inner, &id, "error", 0.0, "", "没有音频地址");
        return;
    };
    let header_text = headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}\r\n"))
        .collect::<String>();
    let part = part_path(&path);
    if let Some(parent) = part.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ffmpeg = o11a1::paths::ffmpeg_dir().join("ffmpeg.exe");
    let mut command = Command::new(&ffmpeg);
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("info")
        .arg("-stats")
        .arg("-headers")
        .arg(&header_text)
        .arg("-i")
        .arg(video)
        .arg("-headers")
        .arg(&header_text)
        .arg("-i")
        .arg(audio)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-f")
        .arg("mp4")
        .arg(&part)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            set_status(&inner, &id, "error", 0.0, "", &format!("启动 ffmpeg 失败：{error}"));
            return;
        }
    };
    assign_to_job(&child);
    let stderr = child.stderr.take();
    let mut last_save = Instant::now();
    if let Some(stderr) = stderr {
        for line in BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                set_status(&inner, &id, "cancelled", 0.0, "", "已取消");
                let _ = std::fs::remove_file(&part);
                return;
            }
            let progress = parse_time(&line)
                .map(|seconds| {
                    if duration > 0 {
                        (seconds / duration as f64 * 100.0).clamp(0.0, 99.9)
                    } else {
                        0.0
                    }
                })
                .unwrap_or(0.0);
            let speed = line
                .split("speed=")
                .nth(1)
                .and_then(|tail| tail.split_whitespace().next())
                .unwrap_or("")
                .to_string();
            if progress > 0.0 {
                set_status(&inner, &id, "running", progress, &speed, "");
            }
            if last_save.elapsed() >= Duration::from_secs(2) {
                last_save = Instant::now();
                save_current(&inner);
            }
        }
    }
    match child.wait() {
        Ok(status) if status.success() => {
            if let Err(error) = std::fs::rename(&part, &path) {
                set_status(&inner, &id, "error", 0.0, "", &format!("重命名失败：{error}"));
                return;
            }
            set_status(&inner, &id, "done", 100.0, "", "");
        }
        Ok(status) => {
            set_status(
                &inner,
                &id,
                "error",
                0.0,
                "",
                &format!("ffmpeg 退出：{status}"),
            );
            let _ = std::fs::remove_file(&part);
        }
        Err(error) => {
            set_status(&inner, &id, "error", 0.0, "", &format!("等待 ffmpeg 失败：{error}"));
        }
    }
}

fn parse_time(line: &str) -> Option<f64> {
    let value = line.split("time=").nth(1)?.split_whitespace().next()?;
    let mut parts = value.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn task_json(task: &Task) -> Value {
    json!({
        "id": task.id,
        "bvid": task.bvid,
        "cid": task.cid,
        "title": task.title,
        "cover": task.cover,
        "status": task.status,
        "progress": task.progress,
        "speed": task.speed,
        "path": task.path,
        "error": task.error,
        "created": task.created,
    })
}

fn set_status(
    inner: &Arc<Mutex<Inner>>,
    id: &str,
    status: &str,
    progress: f64,
    speed: &str,
    error: &str,
) {
    let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(task) = guard.tasks.iter_mut().find(|task| task.id == id) {
        task.status = status.to_string();
        task.progress = progress;
        if !speed.is_empty() {
            task.speed = speed.to_string();
        }
        task.error = error.to_string();
    }
    save_tasks(&guard.tasks);
}

fn save_current(inner: &Arc<Mutex<Inner>>) {
    let guard = inner.lock().unwrap_or_else(|p| p.into_inner());
    save_tasks(&guard.tasks);
}

fn state_path() -> PathBuf {
    o11a1::paths::root().join("downloads.json")
}

fn save_tasks(tasks: &[Task]) {
    let rows: Vec<Value> = tasks
        .iter()
        .map(|task| {
            json!({
                "id": task.id,
                "bvid": task.bvid,
                "cid": task.cid,
                "title": task.title,
                "cover": task.cover,
                "status": task.status,
                "progress": task.progress,
                "speed": task.speed,
                "path": task.path,
                "error": task.error,
                "created": task.created,
            })
        })
        .collect();
    if let Ok(text) = serde_json::to_string_pretty(&rows) {
        let _ = std::fs::write(state_path(), text);
    }
}

fn load_tasks() -> Vec<Task> {
    let Ok(text) = std::fs::read_to_string(state_path()) else {
        return Vec::new();
    };
    let Ok(rows) = serde_json::from_str::<Vec<Value>>(&text) else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|row| {
            let mut status = row
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            if status == "running" || status == "queued" {
                status = "error".to_string();
            }
            Task {
                id: row.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                bvid: row.get("bvid").and_then(Value::as_str).unwrap_or_default().to_string(),
                cid: row.get("cid").and_then(Value::as_u64).unwrap_or(0),
                title: row.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
                cover: row.get("cover").and_then(Value::as_str).unwrap_or_default().to_string(),
                status,
                progress: row.get("progress").and_then(Value::as_f64).unwrap_or(0.0),
                speed: String::new(),
                path: row.get("path").and_then(Value::as_str).unwrap_or_default().to_string(),
                error: if row.get("status").and_then(Value::as_str) == Some("running") {
                    "上次运行被中断".to_string()
                } else {
                    row.get("error").and_then(Value::as_str).unwrap_or_default().to_string()
                },
                created: row.get("created").and_then(Value::as_u64).unwrap_or(0),
                cancel: Arc::new(AtomicBool::new(false)),
            }
        })
        .collect()
}

fn output_path(title: &str, bvid: &str) -> PathBuf {
    let dir = o11a1::paths::root().join("downloads");
    let _ = std::fs::create_dir_all(&dir);
    let mut safe: String = title
        .chars()
        .map(|ch| if "<>:\"/\\|?*".contains(ch) { '_' } else { ch })
        .collect();
    safe = safe.trim().trim_end_matches('.').to_string();
    if safe.is_empty() {
        safe = bvid.to_string();
    }
    if safe.chars().count() > 80 {
        safe = safe.chars().take(80).collect();
    }
    dir.join(format!("{safe} [{bvid}].mp4"))
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.mp4".to_string());
    name.push_str(".part.mp4");
    path.with_file_name(name)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
