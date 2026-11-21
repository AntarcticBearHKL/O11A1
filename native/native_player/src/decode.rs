//! ffmpeg video pipe: one child process per decode session, one reader thread
//! that turns the raw NV12 byte stream into timestamped frames.
//!
//! The reader paces admission at the track's frame rate, so ffmpeg cannot race
//! through the whole file in a burst and leave the ring holding the *end* of
//! the video. The ring itself is bounded and drops the oldest frame on
//! overflow; it never makes the decoder wait forever (the pipe's own backpressure
//! is the real brake).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::log;
use crate::proc;

/// One decoded picture on the pipe. `ts_ms` is on the media timeline.
pub struct DecodedFrame {
    /// Shared so the reader can recycle the buffer once the engine drops the
    /// last reference. At 4K a fresh 12 MB allocation per frame costs enough
    /// page faults to starve the 60 fps deadline.
    pub data: Arc<Vec<u8>>,
    pub ts_ms: u64,
}

struct QueueInner {
    frames: VecDeque<DecodedFrame>,
    closed: bool,
    dropped: u64,
}

/// Bounded frame ring shared by the reader and the presentation loop.
pub struct FrameQueue {
    inner: Mutex<QueueInner>,
    ready: Condvar,
    /// Signalled when a consumer frees a slot, so a full ring backpressures
    /// the ffmpeg reader instead of discarding a frame that is about to be due.
    space: Condvar,
    capacity: usize,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(QueueInner {
                frames: VecDeque::with_capacity(capacity.max(1)),
                closed: false,
                dropped: 0,
            }),
            ready: Condvar::new(),
            space: Condvar::new(),
            capacity: capacity.max(1),
        })
    }

    fn push(&self, frame: DecodedFrame) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        loop {
            if inner.closed {
                return;
            }
            if inner.frames.len() < self.capacity {
                break;
            }
            // The ring is full: hold this frame and let the pipe push back on
            // ffmpeg. Dropping the oldest frame here used to throw away the
            // very frame the presentation clock was about to need when the
            // audio clock ran a little behind the decoder.
            inner = match self.space.wait(inner) {
                Ok(guard) => guard,
                Err(_) => return,
            };
        }
        inner.frames.push_back(frame);
        self.ready.notify_all();
    }

    /// Take the oldest frame that is due, presenting the queue strictly in
    /// order. The audio clock updates in coarse steps, so a single step can
    /// make two pictures due at once; taking the *newest* threw the older one
    /// away and read as a stutter. When the picture is already more than
    /// `max_lag_ms` behind the clock, the queue jumps to the newest due frame
    /// instead, so latency can never accumulate.
    pub fn take_due_in_order(
        &self,
        now_ms: u64,
        tolerance_ms: u64,
        max_lag_ms: u64,
    ) -> Option<DecodedFrame> {
        let mut inner = self.inner.lock().ok()?;
        let front_ts = inner.frames.front().map(|frame| frame.ts_ms)?;
        let horizon = now_ms.saturating_add(tolerance_ms);
        if front_ts > horizon {
            return None;
        }
        let frame = if now_ms.saturating_sub(front_ts) > max_lag_ms {
            let mut newest: Option<DecodedFrame> = None;
            while let Some(front) = inner.frames.front() {
                if front.ts_ms > horizon {
                    break;
                }
                if newest.is_some() {
                    inner.dropped = inner.dropped.saturating_add(1);
                }
                newest = inner.frames.pop_front();
            }
            newest
        } else {
            inner.frames.pop_front()
        };
        let freed = frame.is_some();
        drop(inner);
        if freed {
            self.space.notify_all();
        }
        frame
    }

    /// Take the oldest frame when it is at or before `horizon_ms`. Unlike
    /// [`Self::take_due`], this keeps the queue in order and returns one frame
    /// at a time, so the interpolation scheduler can hold the next source
    /// picture while it generates the ticks in between.
    pub fn take_front_until(&self, horizon_ms: u64) -> Option<DecodedFrame> {
        let mut inner = self.inner.lock().ok()?;
        let due = inner
            .frames
            .front()
            .map(|frame| frame.ts_ms <= horizon_ms)?;
        if !due {
            return None;
        }
        let frame = inner.frames.pop_front();
        drop(inner);
        if frame.is_some() {
            self.space.notify_all();
        }
        frame
    }

    /// Take the first frame regardless of timestamp (the post-seek preview: the
    /// clock has not been re-anchored yet).
    pub fn take_first(&self) -> Option<DecodedFrame> {
        let mut inner = self.inner.lock().ok()?;
        let frame = inner.frames.pop_front();
        let freed = frame.is_some();
        drop(inner);
        if freed {
            self.space.notify_all();
        }
        frame
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.frames.len())
            .unwrap_or(0)
    }

    /// Timestamp of the oldest queued frame, for stall diagnostics.
    pub fn front_ts(&self) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.frames.front().map(|frame| frame.ts_ms))
    }

    pub fn dropped(&self) -> u64 {
        self.inner.lock().map(|inner| inner.dropped).unwrap_or(0)
    }

    fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.closed = true;
            self.ready.notify_all();
            self.space.notify_all();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().map(|inner| inner.closed).unwrap_or(true)
    }
}

/// Everything a decode session is opened for.
pub struct VideoSpec {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub remote: bool,
    /// Source size reported by the manifest/probe.
    pub source_width: u32,
    pub source_height: u32,
    /// The size ffmpeg is asked to scale to (== source unless capped).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Playback speed: the reader admits `fps * speed` frames per second so a
    /// 1.5×/2× session is not starved by its own pacing.
    pub speed: f64,
    /// Stable identity for the disk cache. `None` keeps the original URL
    /// (local files, live streams and direct sources the cache could not
    /// range-probe).
    pub cache_key: Option<String>,
    /// The GPU decoder this stream must use (`h264_cuvid`, `hevc_cuvid`,
    /// `av1_cuvid`, `vp9_cuvid`). Empty means no GPU path exists and the
    /// session is refused — there is no software fallback.
    pub hw_decoder: String,
}

/// One running ffmpeg video decoder + its reader.
pub struct VideoDecoder {
    child: Child,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// Closed while paused/seeking: the reader pushes one frame and then waits
    /// here instead of racing through the file (or dropping the first frames
    /// during a long audio warm-up).
    pub gate: Arc<AtomicBool>,
    pub queue: Arc<FrameQueue>,
    pub generation: u64,
    /// Last stderr lines from ffmpeg, so a hardware decoder that refuses a
    /// profile can be reported instead of a generic "decoder ended".
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl VideoDecoder {
    /// PID of the child ffmpeg process, used for the live network read
    /// counter in the debug HUD.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn spawn(
        spec: &VideoSpec,
        start_ms: u64,
        generation: u64,
        gate_open: bool,
    ) -> Result<Self> {
        if spec.hw_decoder.trim().is_empty() {
            return Err(anyhow!("this stream has no GPU decoder"));
        }
        let target_w = spec.width.max(2) & !1;
        let target_h = spec.height.max(2) & !1;
        let frame_bytes = target_w as usize * target_h as usize * 3 / 2;
        let url = if spec.remote {
            spec.cache_key
                .as_deref()
                .and_then(|key| crate::mediacache::cached_url(&spec.url, &spec.headers, key))
                .unwrap_or_else(|| spec.url.clone())
        } else {
            spec.url.clone()
        };
        let cached = url != spec.url;
        let scale = target_w != spec.source_width || target_h != spec.source_height;
        let mut command = proc::command("ffmpeg.exe");
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-probesize")
            .arg("1000000")
            .arg("-analyzeduration")
            .arg("1000000")
            .arg("-fflags")
            .arg("+discardcorrupt");
        if spec.remote {
            proc::network_args(&mut command);
            let headers = proc::header_string(&spec.headers);
            if !headers.is_empty() {
                command.arg("-headers").arg(headers);
            }
        }
        // GPU-only: the decoder runs on NVDEC (CUDA). There is deliberately no
        // `-c:v <software decoder>` behind this: an unsupported stream fails
        // instead of dragging the CPU pipeline back in. `hwdownload` is the one
        // unavoidable copy — ffmpeg is an external process, so the frame has to
        // cross the pipe — but the 1.5 bytes/pixel NV12 upload replaces the old
        // 4 bytes/pixel BGRA conversion and the 4K NV12→BGRA loop is gone.
        command
            .arg("-hwaccel")
            .arg("cuda")
            .arg("-hwaccel_output_format")
            .arg("cuda")
            .arg("-c:v")
            .arg(&spec.hw_decoder);
        if start_ms > 0 {
            command.arg("-ss").arg(proc::seconds(start_ms));
        }
        command
            .arg("-i")
            .arg(&url)
            .arg("-an")
            .arg("-sn")
            .arg("-dn");
        // One GPU chain for every stream. `format=nv12` handles both cases:
        // an 8-bit CUDA frame already is NV12 and `scale_cuda` passes it
        // through untouched, while a 10-bit (P010/P016) frame is converted on
        // the GPU instead of failing `hwdownload,format=nv12` (the old chain
        // refused every 4K HDR / 10-bit HEVC or AV1 stream).
        command
            .arg("-vf")
            .arg(format!(
                "scale_cuda={target_w}:{target_h}:format=nv12,hwdownload,format=nv12"
            ));
        command
            .arg("-fps_mode")
            .arg("passthrough")
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("nv12")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        log::line(&format!(
            "decoder spawn gen={generation} start_ms={start_ms} size={target_w}x{target_h} frame_bytes={frame_bytes} fps={:.3} remote={} cached={} hw={} scale={}",
            spec.fps,
            spec.remote,
            u8::from(cached),
            spec.hw_decoder,
            u8::from(scale)
        ));
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn ffmpeg for {url}"))?;
        // The 4K pipe has ~2x realtime headroom. Let the decoder win over the
        // background cache workers (which run below normal) instead of losing
        // frames to scheduling noise.
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::Threading::{
                SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS,
            };
            let handle = HANDLE(child.as_raw_handle() as _);
            if let Err(error) = SetPriorityClass(handle, ABOVE_NORMAL_PRIORITY_CLASS) {
                log::line(&format!("decoder priority failed: {error:#}"));
            }
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;
        let stderr = child.stderr.take();
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AtomicBool::new(gate_open));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        // A 250 ms ring gives the presenter a real jitter buffer. The old six
        // slots were exactly 100 ms at 60 fps, so one cache/disk/CPU hiccup
        // emptied the ring and `take_due_in_order` threw away five pictures in
        // a row (the 4K cold-cache stutter). Deeper rings cost memory only:
        // 15 x 12.4 MB at 4K60, ~50 MB at 1080p60.
        let (frame_period, queue_depth) = {
            let fps = if spec.fps.is_finite() && spec.fps > 1.0 {
                spec.fps
            } else {
                30.0
            };
            let speed = if spec.speed.is_finite() && spec.speed > 0.0 {
                spec.speed
            } else {
                1.0
            };
            let speed = crate::speed::clamp(speed as f32) as f64;
            let wall_fps = fps.min(240.0) * speed;
            (
                Duration::from_secs_f64(1.0 / wall_fps),
                (wall_fps * 0.25).ceil().clamp(6.0, 15.0) as usize,
            )
        };
        let queue = FrameQueue::new(queue_depth);
        log::line(&format!(
            "decoder ring gen={generation} depth={queue_depth} frame_bytes={frame_bytes}"
        ));
        // QA hook: cap decode throughput to reproduce the 4K / slow-decoder
        // case where the picture cannot be produced in real time. Unset in
        // normal runs, so the reader never sleeps.
        let qa_delay = std::env::var("O11A1_QA_DECODE_DELAY_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or_default();
        if !qa_delay.is_zero() {
            log::line(&format!(
                "decoder qa throttle gen={generation} delay_ms={}",
                qa_delay.as_millis()
            ));
        }
        let reader = {
            let queue = queue.clone();
            let stop = stop.clone();
            let gate = gate.clone();
            std::thread::Builder::new()
                .name(format!("player-video-{generation}"))
                .spawn(move || {
                    read_frames(
                        stdout,
                        queue,
                        stop,
                        gate,
                        frame_bytes,
                        start_ms,
                        frame_period,
                        qa_delay,
                    )
                })
                .context("spawn video reader")?
        };
        let stderr_thread = stderr.map(|stderr| {
            let generation = generation;
            let tail_out = stderr_tail.clone();
            std::thread::Builder::new()
                .name(format!("player-video-err-{generation}"))
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    let mut tail: VecDeque<String> = VecDeque::new();
                    for line in reader.lines().map_while(Result::ok) {
                        let text = line.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        tail.push_back(text);
                        if tail.len() > 6 {
                            tail.pop_front();
                        }
                    }
                    if !tail.is_empty() {
                        log::line(&format!(
                            "decoder stderr gen={generation} [{}]",
                            tail.iter().cloned().collect::<Vec<_>>().join(" | ")
                        ));
                    }
                    if let Ok(mut stored) = tail_out.lock() {
                        *stored = tail;
                    }
                })
                .ok()
        });
        Ok(Self {
            child,
            reader: Some(reader),
            stderr: stderr_thread.flatten(),
            stop,
            gate,
            queue,
            generation,
            stderr_tail,
        })
    }

    /// The last ffmpeg error lines, used to explain a failed hardware decode.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|tail| {
                tail.iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default()
    }

    /// Ask the child to stop, close its pipe and join both threads. Safe to call
    /// twice and from `Drop`; a killed child can never linger.
    pub fn stop(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.queue.close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bytes read by one process (network + local file), used by the debug HUD
/// to show the live ffmpeg download rate rather than a bitrate estimate.
pub fn process_read_bytes(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetProcessIoCounters, OpenProcess, IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut counters = IO_COUNTERS::default();
        let ok = GetProcessIoCounters(handle, &mut counters).is_ok();
        let _ = CloseHandle(handle);
        ok.then_some(counters.ReadTransferCount)
    }
}

/// Windows pipes deliver data in `ReadFile`-sized chunks, and a single huge
/// read is pathologically slow (a 12 MB ask measured ~10 fps while 64 KB asks
/// reached ~1.1 GB/s for the same 4K NV12 stream). The reader therefore fills
/// one frame with fixed-size reads instead of `read_exact` on the whole slice.
const READ_CHUNK: usize = 64 * 1024;

fn read_frames(
    mut stdout: ChildStdout,
    queue: Arc<FrameQueue>,
    stop: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
    frame_bytes: usize,
    start_ms: u64,
    frame_period: Duration,
    qa_delay: Duration,
) {
    // The pipe reader is on the 4K critical path (12.4 MB per picture at
    // 60 fps). Keep it above the background cache workers.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    // Reusable frame buffers. A buffer is free once only this list holds it;
    // the engine drops its reference when it presents/replaces the picture.
    let mut pool: Vec<Arc<Vec<u8>>> = Vec::new();
    let mut index: u64 = 0;
    let mut window_start = std::time::Instant::now();
    let mut window_frames: u64 = 0;
    loop {
        if stop.load(Ordering::SeqCst) || queue.is_closed() {
            break;
        }
        let mut buffer = match pool.iter().position(|slot| Arc::strong_count(slot) == 1) {
            Some(free) => pool.swap_remove(free),
            None => Arc::new(vec![0u8; frame_bytes]),
        };
        let target = Arc::get_mut(&mut buffer).expect("free buffer must be unique");
        let mut filled = 0usize;
        let mut eof: Option<String> = None;
        while filled < frame_bytes {
            let want = (frame_bytes - filled).min(READ_CHUNK);
            match stdout.read(&mut target[filled..filled + want]) {
                Ok(0) => {
                    eof = Some("eof".to_string());
                    break;
                }
                Ok(read) => filled += read,
                Err(error) => {
                    eof = Some(error.to_string());
                    break;
                }
            }
        }
        if let Some(reason) = eof {
            if !stop.load(Ordering::SeqCst) {
                log::line(&format!("decoder eof after {index} frames reason={reason}"));
            }
            break;
        }
        if !qa_delay.is_zero() {
            std::thread::sleep(qa_delay);
        }
        let ts_ms = start_ms
            .saturating_add((index as f64 * frame_period.as_secs_f64() * 1000.0).round() as u64);
        // Hand the frame over without a full-frame copy: the buffer goes back
        // to the pool once the engine is done with it.
        queue.push(DecodedFrame {
            data: buffer.clone(),
            ts_ms,
        });
        pool.push(buffer);
        index = index.saturating_add(1);
        window_frames = window_frames.saturating_add(1);
        // Hardware decode throughput, reported once a second: the pacer can
        // only blend what this pipe delivers, so this is the first number to
        // look at when 4K interpolation feels short.
        if window_start.elapsed() >= Duration::from_secs(1) {
            log::line(&format!(
                "decoder read fps={:.1} frames={index} queue={}",
                window_frames as f64 / window_start.elapsed().as_secs_f64(),
                queue.len()
            ));
            window_start = std::time::Instant::now();
            window_frames = 0;
        }
        // A closed gate (pause, a seek waiting for the new picture, or the
        // startup audio warm-up) stops admission after this frame. The pipe's
        // backpressure then holds ffmpeg itself, so nothing is decoded ahead
        // into a ring that would drop it.
        while !gate.load(Ordering::SeqCst) {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Keep the ring fed so the presenter can look one source frame ahead.
        // `push` itself blocks once all six slots are occupied.
    }
    queue.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ts_ms: u64) -> DecodedFrame {
        DecodedFrame {
            data: Arc::new(Vec::new()),
            ts_ms,
        }
    }

    #[test]
    fn due_frames_are_taken_in_order_when_the_clock_steps() {
        let queue = FrameQueue::new(8);
        for ts in [0u64, 16, 33, 50] {
            queue.push(frame(ts));
        }
        // A coarse audio-clock step makes 0 and 16 due together: the older
        // picture is presented first and 16 follows on the next loop turn.
        assert_eq!(queue.take_due_in_order(20, 8, 50).map(|f| f.ts_ms), Some(0));
        assert_eq!(queue.take_due_in_order(20, 8, 50).map(|f| f.ts_ms), Some(16));
        assert_eq!(queue.take_due_in_order(20, 8, 50).map(|f| f.ts_ms), None);
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn a_late_picture_jumps_to_the_newest_due_frame() {
        let queue = FrameQueue::new(8);
        for ts in [0u64, 16, 33, 50] {
            queue.push(frame(ts));
        }
        // 120 ms behind: skip straight to the newest frame <= 128 ms.
        assert_eq!(
            queue.take_due_in_order(120, 8, 40).map(|f| f.ts_ms),
            Some(50)
        );
        assert_eq!(queue.dropped(), 3);
    }

    #[test]
    fn nothing_is_due_before_the_first_picture() {
        let queue = FrameQueue::new(8);
        queue.push(frame(500));
        assert!(queue.take_due_in_order(100, 8, 40).is_none());
        assert_eq!(queue.len(), 1);
    }
}
