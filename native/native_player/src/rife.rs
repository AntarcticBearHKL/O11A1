//! Manual RIFE frame interpolation.
//!
//! The player keeps the original GPU linear blend as the `blend` choice. When
//! the user picks a `rife_*` algorithm this module owns a worker thread that
//! turns two decoded NV12 pictures plus a timestep into one BGRA picture with
//! an ONNX Runtime CUDA session. There is no automatic model choice and no
//! fallback to the blend: the selected model is used for every pair, and a
//! late worker simply leaves the presenter on the source frame.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{anyhow, Result};
use ort::ep::{CUDA, DirectML, TensorRT};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

/// Stable ids stored in `settings.json`. `blend` is handled by `engine.rs`;
/// every other id has a matching ONNX file in the RIFE model directory.
pub const MODELS: [(&str, &str, &str); 7] = [
    ("rife_4_25_lite", "RIFE 4.25 Lite", "rife_4_25_lite.onnx"),
    ("rife_4_26", "RIFE 4.26", "rife_4_26.onnx"),
    ("rife_4_18", "RIFE 4.18", "rife_4_18.onnx"),
    ("rife_4_7", "RIFE 4.7", "rife_4_7.onnx"),
    ("rife_4_8", "RIFE 4.8", "rife_4_8.onnx"),
    ("rife_4_9", "RIFE 4.9", "rife_4_9.onnx"),
    ("rife_4_10", "RIFE 4.10", "rife_4_10.onnx"),
];

pub fn model_label(id: &str) -> &'static str {
    MODELS
        .iter()
        .find(|(model, _, _)| *model == id)
        .map(|(_, label, _)| *label)
        .unwrap_or("RIFE")
}

fn model_file(id: &str) -> Option<&'static str> {
    MODELS
        .iter()
        .find(|(model, _, _)| *model == id)
        .map(|(_, _, file)| *file)
}

fn model_directory() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("O11A1_RIFE_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    for candidate in [
        exe_dir.join("rife").join("models"),
        exe_dir.join("..").join("runtime").join("rife").join("models"),
    ] {
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    if let Ok(home) = std::env::var("O11A1_HOME") {
        let candidate = PathBuf::from(home).join("models").join("rife");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

fn engine_cache_directory() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("O11A1_RIFE_CACHE") {
        let path = PathBuf::from(path);
        let _ = std::fs::create_dir_all(&path);
        return Some(path);
    }
    if let Ok(home) = std::env::var("O11A1_HOME") {
        let path = PathBuf::from(home).join("models").join("rife").join("cache");
        let _ = std::fs::create_dir_all(&path);
        return Some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("rife").join("cache");
    let _ = std::fs::create_dir_all(&path);
    Some(path)
}

fn onnxruntime_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("O11A1_ORT_DYLIB") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = std::env::var("ORT_DYLIB_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    for candidate in [
        exe_dir.join("rife").join("onnxruntime.dll"),
        exe_dir
            .join("..")
            .join("runtime")
            .join("rife")
            .join("onnxruntime.dll"),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Put the ORT directory (which also holds the CUDA provider and the cuDNN /
/// cuBLAS DLLs in the packaged layout) on the DLL search path before ORT loads.
fn prepare_dll_path(dll: Option<&PathBuf>) {
    if let Some(dir) = dll.and_then(|path| path.parent()) {
        let dir = dir.to_string_lossy();
        let current = std::env::var("PATH").unwrap_or_default();
        if !current.split(';').any(|part| part.eq_ignore_ascii_case(&dir)) {
            std::env::set_var("PATH", format!("{dir};{current}"));
        }
    }
}

static ENVIRONMENT: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();

fn environment() -> Result<&'static ort::environment::Environment, String> {
    ENVIRONMENT
        .get_or_init(|| {
            let dll = onnxruntime_path();
            prepare_dll_path(dll.as_ref());
            let builder = match dll {
                Some(path) => ort::init_from(path).map_err(|error| error.to_string())?,
                None => ort::init(),
            };
            builder.build().map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| error.clone())
}

struct Job {
    a: Arc<Vec<u8>>,
    b: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    t: f32,
    media_ts: u64,
}

pub struct RifeFrame {
    pub bgra: Vec<u8>,
    pub media_ts: u64,
    pub t: f32,
    pub infer_us: u32,
    pub total_us: u32,
    pub first: bool,
}

pub enum RifeEvent {
    Ready,
    Error(String),
    Frame(RifeFrame),
}

pub struct RifeWorker {
    pending: Arc<(Mutex<Option<Job>>, Condvar)>,
    events: Mutex<Receiver<RifeEvent>>,
    stop: Arc<AtomicBool>,
    id: &'static str,
    label: &'static str,
    width: u32,
    height: u32,
}

impl RifeWorker {
    /// Spawn the worker. The ONNX session is created on the worker thread so
    /// the player window never blocks on CUDA / cuDNN initialization.
    pub fn new(id: &str, width: u32, height: u32) -> Result<Self> {
        let file = model_file(id).ok_or_else(|| anyhow!("unknown RIFE model {id}"))?;
        let directory = model_directory()
            .ok_or_else(|| anyhow!("RIFE model directory not found (set O11A1_RIFE_DIR)"))?;
        let model_path = directory.join(file);
        if !model_path.is_file() {
            return Err(anyhow!("RIFE model missing: {}", model_path.display()));
        }
        let pending = Arc::new((Mutex::new(None), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel();
        let worker_pending = Arc::clone(&pending);
        let worker_stop = Arc::clone(&stop);
        let label = model_label(id);
        let model_path_for_thread = model_path.clone();
        std::thread::Builder::new()
            .name("o11a1-rife".to_string())
            .spawn(move || {
                run_worker(
                    model_path_for_thread,
                    width,
                    height,
                    worker_pending,
                    worker_stop,
                    event_tx,
                )
            })
            .map_err(|error| anyhow!("spawn RIFE worker: {error}"))?;
        crate::log::line(&format!(
            "rife worker start model={} path={} size={}x{}",
            label,
            model_path.display(),
            width,
            height
        ));
        Ok(Self {
            pending,
            events: Mutex::new(event_rx),
            stop,
            id: MODELS
                .iter()
                .find(|(model, _, _)| *model == id)
                .map(|(model, _, _)| *model)
                .unwrap_or("rife"),
            label,
            width,
            height,
        })
    }

    pub fn model_id(&self) -> &'static str {
        self.id
    }

    pub fn label(&self) -> &'static str {
        self.label
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn submit(&self, a: Arc<Vec<u8>>, b: Arc<Vec<u8>>, t: f32, media_ts: u64) {
        let (lock, ready) = &*self.pending;
        if let Ok(mut slot) = lock.lock() {
            *slot = Some(Job {
                a,
                b,
                width: self.width,
                height: self.height,
                t: t.clamp(0.0, 1.0),
                media_ts,
            });
            ready.notify_one();
        }
    }

    pub fn try_recv(&self) -> Option<RifeEvent> {
        self.events.lock().ok()?.try_recv().ok()
    }
}

impl Drop for RifeWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let (lock, ready) = &*self.pending;
        if let Ok(mut slot) = lock.lock() {
            *slot = None;
            ready.notify_one();
        }
    }
}

fn run_worker(
    model_path: PathBuf,
    width: u32,
    height: u32,
    pending: Arc<(Mutex<Option<Job>>, Condvar)>,
    stop: Arc<AtomicBool>,
    events: Sender<RifeEvent>,
) {
    let env = match environment() {
        Ok(env) => env,
        Err(error) => {
            crate::log::line(&format!("rife ORT init failed: {error}"));
            let _ = events.send(RifeEvent::Error(error));
            return;
        }
    };
    let backend = std::env::var("O11A1_RIFE_BACKEND")
        .unwrap_or_else(|_| "dml".to_string())
        .to_ascii_lowercase();
    let session = (|| -> std::result::Result<Session, String> {
        let builder = Session::builder(env).map_err(|error| error.to_string())?;
        let builder = builder
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|error| error.to_string())?;
        let builder = match backend.as_str() {
            "tensorrt" | "trt" => {
                let profile = format!("input:1x7x{height}x{width}");
                let mut tensorrt = TensorRT::default()
                    .with_device_id(0)
                    .with_fp16(true)
                    .with_builder_optimization_level(3)
                    .with_max_workspace_size(4usize << 30)
                    .with_profile_min_shapes(profile.clone())
                    .with_profile_opt_shapes(profile.clone())
                    .with_profile_max_shapes(profile);
                if let Some(cache) = engine_cache_directory() {
                    tensorrt = tensorrt
                        .with_engine_cache(true)
                        .with_engine_cache_path(cache.display().to_string())
                        .with_timing_cache(true);
                }
                builder.with_execution_providers([
                    tensorrt.build(),
                    CUDA::default().with_device_id(0).build(),
                ])
            }
            "cuda" => builder
                .with_execution_providers([CUDA::default().with_device_id(0).build()]),
            _ => builder.with_execution_providers([DirectML::default()
                .with_device_id(0)
                .build()]),
        }
        .map_err(|error| error.to_string())?;
        let mut builder = builder;
        builder
            .commit_from_file(&model_path)
            .map_err(|error| error.to_string())
    })();
    let mut session = match session {
        Ok(session) => session,
        Err(text) => {
            crate::log::line(&format!("rife session failed: {text}"));
            let _ = events.send(RifeEvent::Error(text));
            return;
        }
    };
    let input_name = session.inputs()[0].name().to_string();
    let output_name = session.outputs()[0].name().to_string();
    crate::log::line(&format!(
        "rife session ready model={} backend={} providers={:?} input={} output={}",
        model_path.display(),
        backend,
        session
            .inputs()
            .iter()
            .map(|input| input.name().to_string())
            .collect::<Vec<_>>(),
        input_name,
        output_name
    ));
    let _ = events.send(RifeEvent::Ready);

    let mut first = true;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let job = {
            let (lock, ready) = &*pending;
            let Ok(mut slot) = lock.lock() else {
                break;
            };
            while slot.is_none() && !stop.load(Ordering::SeqCst) {
                let Ok(next) = ready.wait(slot) else {
                    return;
                };
                slot = next;
            }
            slot.take()
        };
        let Some(job) = job else {
            continue;
        };
        if job.width != width || job.height != height {
            crate::log::line(&format!(
                "rife job size change {}x{} -> {}x{} ignored",
                width, height, job.width, job.height
            ));
            continue;
        }
        let started = Instant::now();
        let plane = job.width as usize * job.height as usize;
        if job.a.len() < plane * 3 / 2 || job.b.len() < plane * 3 / 2 {
            crate::log::line("rife job dropped: short NV12 buffer");
            continue;
        }
        let mut input = vec![0.0f32; plane * 7];
        nv12_to_rgb(&job.a, job.width as usize, job.height as usize, &mut input);
        nv12_to_rgb(
            &job.b,
            job.width as usize,
            job.height as usize,
            &mut input[plane * 3..],
        );
        let timestep = job.t.clamp(0.0, 1.0);
        for value in &mut input[plane * 6..plane * 7] {
            *value = timestep;
        }
        let tensor = match Tensor::from_array((
            [1usize, 7, job.height as usize, job.width as usize],
            input.into_boxed_slice(),
        )) {
            Ok(tensor) => tensor,
            Err(error) => {
                crate::log::line(&format!("rife tensor failed: {error}"));
                continue;
            }
        };
        let run_started = Instant::now();
        let outputs = match session.run(ort::inputs![input_name.as_str() => tensor]) {
            Ok(outputs) => outputs,
            Err(error) => {
                crate::log::line(&format!("rife run failed: {error}"));
                continue;
            }
        };
        let infer_us = run_started.elapsed().as_micros().min(u32::MAX as u128) as u32;
        let (shape, data) = match outputs[0].try_extract_tensor::<f32>() {
            Ok(tensor) => tensor,
            Err(error) => {
                crate::log::line(&format!("rife output failed: {error}"));
                continue;
            }
        };
        let need = plane * 3;
        if data.len() < need {
            crate::log::line(&format!(
                "rife output short shape={shape:?} data={} need={need}",
                data.len()
            ));
            continue;
        }
        let mut bgra = vec![0u8; plane * 4];
        for index in 0..plane {
            let r = (data[index] * 255.0).clamp(0.0, 255.0) as u8;
            let g = (data[plane + index] * 255.0).clamp(0.0, 255.0) as u8;
            let b = (data[plane * 2 + index] * 255.0).clamp(0.0, 255.0) as u8;
            let out = index * 4;
            bgra[out] = b;
            bgra[out + 1] = g;
            bgra[out + 2] = r;
            bgra[out + 3] = 255;
        }
        let total_us = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
        if first {
            crate::log::line(&format!(
                "rife first frame shape={shape:?} infer_ms={:.1} total_ms={:.1} t={:.3}",
                infer_us as f32 / 1000.0,
                total_us as f32 / 1000.0,
                timestep
            ));
            first = false;
        }
        let _ = events.send(RifeEvent::Frame(RifeFrame {
            bgra,
            media_ts: job.media_ts,
            t: timestep,
            infer_us,
            total_us,
            first: false,
        }));
    }
    crate::log::line("rife worker stop");
}

/// BT.709 limited-range NV12 -> RGB, matching `enhance.hlsl::nv12_main`.
/// The output is three planar f32 channels at `target[0..3*plane]`.
fn nv12_to_rgb(nv12: &[u8], width: usize, height: usize, target: &mut [f32]) {
    let plane = width * height;
    let uv_offset = plane;
    let uv_stride = width;
    let mut y_table = [0.0f32; 256];
    let mut cb_table = [0.0f32; 256];
    let mut cr_table = [0.0f32; 256];
    for value in 0..256 {
        y_table[value] = ((value as f32 - 16.0) * (298.0 / 256.0)).clamp(0.0, 255.0);
        cb_table[value] = value as f32 - 128.0;
        cr_table[value] = value as f32 - 128.0;
    }
    for y in 0..height {
        let uv_row = (y / 2) * uv_stride;
        let row = y * width;
        for x in 0..width {
            let index = row + x;
            let uv_index = uv_offset + uv_row + (x & !1);
            let luma = y_table[nv12[index] as usize];
            let cb = cb_table[nv12[uv_index] as usize];
            let cr = cr_table[nv12[uv_index + 1] as usize];
            let r = (luma + cr * (403.0 / 256.0)) / 255.0;
            let g = (luma - (48.0 * cb + 120.0 * cr) / 256.0) / 255.0;
            let b = (luma + cb * (475.0 / 256.0)) / 255.0;
            target[index] = r.clamp(0.0, 1.0);
            target[plane + index] = g.clamp(0.0, 1.0);
            target[plane * 2 + index] = b.clamp(0.0, 1.0);
        }
    }
}
