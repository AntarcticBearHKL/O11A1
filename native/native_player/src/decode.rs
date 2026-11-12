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
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::log;
use crate::proc;

/// One decoded picture on the pipe. `ts_ms` is on the media timeline.
pub struct DecodedFrame {
    pub data: Vec<u8>,
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

    /// Take the newest frame whose timestamp is due at `now_ms` (with
    /// `tolerance_ms` of look-ahead) and discard every older due frame. `None`
    /// means the queue has nothing presentable yet.
    pub fn take_due(&self, now_ms: u64, tolerance_ms: u64) -> Option<DecodedFrame> {
        let mut inner = self.inner.lock().ok()?;
        let horizon = now_ms.saturating_add(tolerance_ms);
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
        let freed = newest.is_some();
        drop(inner);
        if freed {
            self.space.notify_all();
        }
        newest
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
        self.inner.lock().map(|inner| inner.frames.len()).unwrap_or(0)
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
}

impl VideoDecoder {
    pub fn spawn(spec: &VideoSpec, start_ms: u64, generation: u64, gate_open: bool) -> Result<Self> {
        let target_w = spec.width.max(2) & !1;
        let target_h = spec.height.max(2) & !1;
        let frame_bytes = target_w as usize * target_h as usize * 3 / 2;
        let mut command = proc::command("ffmpeg.exe");
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-threads")
            .arg("0")
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
        if start_ms > 0 {
            command.arg("-ss").arg(proc::seconds(start_ms));
        }
        command.arg("-i").arg(&spec.url).arg("-an").arg("-sn").arg("-dn");
        if target_w != spec.source_width || target_h != spec.source_height {
            command
                .arg("-vf")
                .arg(format!("scale={target_w}:{target_h}:flags=bilinear"));
        }
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
            "decoder spawn gen={generation} start_ms={start_ms} size={target_w}x{target_h} frame_bytes={frame_bytes} fps={:.3} remote={}",
            spec.fps,
            spec.remote
        ));
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn ffmpeg for {}", spec.url))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;
        let stderr = child.stderr.take();
        let queue = FrameQueue::new(6);
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AtomicBool::new(gate_open));
        let frame_period = {
            let fps = if spec.fps.is_finite() && spec.fps > 1.0 {
                spec.fps
            } else {
                30.0
            };
            let speed = if spec.speed.is_finite() && spec.speed > 0.1 {
                spec.speed
            } else {
                1.0
            };
            Duration::from_secs_f64(1.0 / (fps.min(240.0) * speed.clamp(0.25, 4.0)))
        };
        let reader = {
            let queue = queue.clone();
            let stop = stop.clone();
            let gate = gate.clone();
            std::thread::Builder::new()
                .name(format!("player-video-{generation}"))
                .spawn(move || {
                    read_frames(stdout, queue, stop, gate, frame_bytes, start_ms, frame_period)
                })
                .context("spawn video reader")?
        };
        let stderr_thread = stderr.map(|stderr| {
            let generation = generation;
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
                            tail.into_iter().collect::<Vec<_>>().join(" | ")
                        ));
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
        })
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

fn read_frames(
    mut stdout: ChildStdout,
    queue: Arc<FrameQueue>,
    stop: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
    frame_bytes: usize,
    start_ms: u64,
    frame_period: Duration,
) {
    let mut buffer = vec![0u8; frame_bytes];
    let mut index: u64 = 0;
    let mut next_admit = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) || queue.is_closed() {
            break;
        }
        match stdout.read_exact(&mut buffer) {
            Ok(()) => {}
            Err(error) => {
                if !stop.load(Ordering::SeqCst) {
                    log::line(&format!(
                        "decoder eof after {index} frames reason={error}"
                    ));
                }
                break;
            }
        }
        let ts_ms = start_ms.saturating_add(
            (index as f64 * frame_period.as_secs_f64() * 1000.0).round() as u64,
        );
        queue.push(DecodedFrame {
            data: buffer.clone(),
            ts_ms,
        });
        index = index.saturating_add(1);
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
        // Admission pacing: at most one frame per source period. If the
        // presentation thread ever fell far behind, reset the grid instead of
        // bursting a catch-up (the queue drops the oldest anyway).
        next_admit += frame_period;
        let now = Instant::now();
        if next_admit > now {
            let mut wait = next_admit - now;
            while !wait.is_zero() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let chunk = wait.min(Duration::from_millis(5));
                std::thread::sleep(chunk);
                wait = match wait.checked_sub(chunk) {
                    Some(rest) => rest,
                    None => Duration::ZERO,
                };
            }
        } else if now.duration_since(next_admit) > frame_period * 4 {
            next_admit = now;
        }
    }
    queue.close();
}
