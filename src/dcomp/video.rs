//! The video layer's thread: one mailbox, one presenter.
//!
//! The picture does not go through Slint any more (that was the 30 fps wall):
//! the pacer's frame is handed to this module, uploaded into a staging texture
//! and presented on the composition swapchain at whatever rate the pacer
//! produces. It is the prototype's measured loop
//! (`examples/dcomp_proto.rs`, 117.6-119.97 fps at 1080p), with the deterministic
//! pattern replaced by the real frame.
//!
//! Two rules from that prototype are load-bearing here:
//!
//! * `stage` (the CPU copy + RGBA->BGRA swizzle) runs **before** the compositor
//!   wait. Doing it after is the difference between 86 and 120 presents/s.
//! * the mailbox keeps only the newest frame. A channel would queue a backlog
//!   the screen cannot use; a coalesced frame is counted, not shown late.
//!
//! # f200: the optical-flow warp lives here too
//!
//! Mode 3 used to hand this module a *finished* frame: the CPU warped the pair
//! (`ofa::warp`), then `stage` copied it into a staging texture, then two
//! `CopyResource`s put it in the backbuffer - ~41 MB of memory traffic per 1080p
//! display frame, which is what capped the picture at ~60/s.
//!
//! Now a [`Job::Warp`] hands over the pair and the phase instead, and this
//! thread draws the warp with [`super::warp`] straight into the backbuffer: the
//! two source frames and the flow field are already on this device (they are the
//! optical-flow session's own textures, and the session is created *here*, on
//! this device, from the sink's own `ID3D11Device`/immediate context), so the
//! CPU only issues commands. `fence`/`wait` gate the driver's asynchronous
//! execute so the field can drive the very pair it came from (one interval less
//! latency than f199's read-one-pair-late scheme).
//!
//! The swapchain and its D3D11 device are created on the UI thread (the
//! composition tree needs the swapchain object) and then moved here, because
//! this is the only thread that touches them afterwards. The visual that
//! references the swapchain stays on the UI thread: geometry is a DComp
//! property, not a per-frame one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use slint::{Rgba8Pixel, SharedPixelBuffer};
use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11ShaderResourceView};

use super::enhance::{self, Enhance, Source};
use super::gpu::{Gpu, Layer};
use super::nv12::{BlitPipeline, GpuPicture, Nv12Converter};
use super::warp::{Params, WarpPipeline};

/// The picture the decode pipeline produces (`player::FrameBuf`).
pub(crate) type FrameBuf = SharedPixelBuffer<Rgba8Pixel>;

/// f209: where an MF session reads its bytes from. The decoder owns the
/// transport; the sink only names the stream the pacer asked for.
#[derive(Clone, Debug)]
pub(crate) enum MfSource {
    /// A B 站 DASH fMP4 stream, with the headers the CDN needs.
    Url {
        url: String,
        headers: Vec<(String, String)>,
    },
    /// A local file (`scripts/qa-f209.ps1`'s sample: real H.264 1080p30).
    Path(String),
}

impl MfSource {
    /// Identity of the stream, for "can this session seek instead of reopening".
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Url { url, .. } => url.clone(),
            Self::Path(path) => path.clone(),
        }
    }
}

/// What state the MF engine is in, read by the pacer each session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MfState {
    /// No session has been asked for.
    Idle,
    /// Opening (or seeking): the pacer waits for a verdict.
    Opening,
    /// Decoding; [`VideoSink::submit_tick`] draws.
    Ready,
    /// Sticky for this session: the reason is in the log and
    /// [`VideoSink::mf_reason`].
    Failed,
}

impl MfState {
    fn code(self) -> u64 {
        match self {
            Self::Idle => 0,
            Self::Opening => 1,
            Self::Ready => 2,
            Self::Failed => 3,
        }
    }

    fn from_code(code: u64) -> Self {
        match code {
            1 => Self::Opening,
            2 => Self::Ready,
            3 => Self::Failed,
            _ => Self::Idle,
        }
    }
}

/// One thing the pacer hands to the presenting thread.
pub(crate) enum Job {
    /// A finished frame: stage it and present it (blend / no interpolation).
    Frame(FrameBuf, bool),
    /// Warp `a`/`b` at `phase` straight into the backbuffer. `pair` identifies
    /// the source interval: every display slot of one interval carries the same
    /// id, so the pair is uploaded and executed once.
    Warp {
        a: FrameBuf,
        b: FrameBuf,
        phase: f32,
        pair: u64,
    },
    /// f209: one display slot of an MF session, at `video_ms` of the stream.
    ///
    /// There is no frame in here on purpose: the pictures are the decoder's own
    /// D3D11 surfaces, which this thread holds, converts and draws. The pacer
    /// contributes only its clock (which is the audio clock), so the whole
    /// hand-off between decode and display carries no pixels.
    Tick { video_ms: u64, pair: u64 },
}

/// A command from the pacer: session control, never pixels.
enum SinkCommand {
    MfOpen { source: MfSource, start_ms: u64 },
    MfSeek { start_ms: u64 },
    MfClose,
}

struct Shared {
    /// The newest job the pacer handed over.
    pending: Option<Job>,
    /// f209: display slots waiting to be drawn, oldest first.
    ///
    /// A slot is a *timing* event, not a picture: replacing one with the next
    /// (what the one-slot `pending` mailbox does for frames) would hand the
    /// compositor two presents per source interval instead of four. The queue is
    /// bounded: a sink that cannot keep up must drop slots, not grow a backlog.
    ticks: std::collections::VecDeque<Job>,
    /// f209: session control from the pacer (open / seek / close). Ordered, and
    /// never coalesced: a lost `Open` would leave the picture black.
    commands: std::collections::VecDeque<SinkCommand>,
    /// f202: re-present the frame already staged, without a new one. A tier
    /// change while paused has to be visible, and a paused player produces no
    /// frames to run it on; the staging texture still holds the picture.
    repaint: bool,
    /// Frames whose staging copy is done; the UI thread recycles them into the
    /// blend pool. Without this drain the pool would run dry and every frame
    /// would allocate.
    retired: Vec<(FrameBuf, bool)>,
    close: bool,
}

/// Counters the QA hooks read; all monotonic, all cheap.
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) pushed: AtomicU64,
    pub(crate) presented: AtomicU64,
    pub(crate) coalesced: AtomicU64,
    pub(crate) stage_us: AtomicU64,
    /// f200: frames drawn by the GPU warp instead of staged from the CPU.
    pub(crate) warped: AtomicU64,
    /// CPU time one warp job costs this thread, µs (upload + draw + present).
    pub(crate) warp_us: AtomicU64,
    /// Time the same job spent waiting for the optical-flow engine, µs.
    pub(crate) gate_us: AtomicU64,
    /// Whether the GPU warp is built. Read by the pacer through the sink handle.
    pub(crate) warp_ready: AtomicBool,
    /// f202: frames the 画质增强 chain drew, the desired mode it ran, and the
    /// GPU cost of the last one (µs). `enhance_us` is `u64::MAX` until the first
    /// timestamp query completes, so a zero can never be mistaken for "free".
    pub(crate) enhance_frames: AtomicU64,
    pub(crate) enhance_us: AtomicU64,
    pub(crate) enhance_mode: AtomicU64,
    /// f209: the MF engine's counters. `mf_decoded` pictures came out of the
    /// decoder as textures, `mf_ticks` display slots were drawn from them, and
    /// `upload_bytes` is what the CPU moved into the GPU for those - the number
    /// this whole path exists to make 0.
    pub(crate) mf_decoded: AtomicU64,
    pub(crate) mf_ticks: AtomicU64,
    pub(crate) mf_dropped: AtomicU64,
    pub(crate) upload_bytes: AtomicU64,
    /// Pictures the engine is holding (the player's ring readout).
    pub(crate) mf_queued: AtomicU64,
    /// Whether the pacer wants interpolation (set by it, read by the sink).
    pub(crate) mf_interp_hint: AtomicU64,
    /// Time one `IMFSourceReader::ReadSample` took on the sink thread, µs.
    pub(crate) mf_decode_us: AtomicU64,
    /// Ticks the sink could not cover with a decoded picture (counted so a
    /// starved decoder is visible instead of showing up as a low rate).
    pub(crate) mf_misses: AtomicU64,
    /// Time the decode/choose step took on the sink thread, µs.
    pub(crate) mf_pre_us: AtomicU64,
    /// Whether the GPU warp is up for the MF path (the pacer caps its output
    /// grid on this: no warp means 2x, not 4x).
    pub(crate) mf_warp: AtomicBool,
    /// The MF session's state and the reason it failed, for the pacer.
    pub(crate) mf_state: AtomicU64,
}

/// The UI thread's handle to the video layer.
pub(crate) struct VideoSink {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    pub(crate) counters: Arc<Counters>,
    closed: AtomicBool,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// f209: the stream the MF session was opened on. A later session at the
    /// same stream (a seek, a pause/resume) seeks the decoder instead of
    /// re-opening it.
    mf_key: Mutex<Option<String>>,
}

/// The presenting thread's optical-flow state.
enum WarpState {
    /// Nothing has been asked for yet.
    Cold,
    /// Shaders and session are up; `engine` owns the session, `pipeline` the
    /// draw and `views` the two input textures plus the flow field.
    Ready {
        pipeline: WarpPipeline,
        engine: crate::player::ofa::Ofa,
        views: [ID3D11ShaderResourceView; 3],
    },
    /// Sticky: the reason the GPU path is not usable, logged once.
    Off(String),
}

impl VideoSink {
    /// Spawn the presenting thread for a layer that was created (and composed)
    /// on the UI thread.
    ///
    /// `control` is the player's clock and frame-size handle (f209): the MF
    /// engine publishes the surface size it decoded at so the window's geometry
    /// and the layer agree, and the pacer reads its counters through the player.
    pub(crate) fn start(
        gpu: Gpu,
        layer: Layer,
        control: Option<Arc<crate::player::PlaybackControl>>,
    ) -> Arc<Self> {
        let shared = Arc::new((
            Mutex::new(Shared {
                pending: None,
                ticks: std::collections::VecDeque::new(),
                commands: std::collections::VecDeque::new(),
                repaint: false,
                retired: Vec::new(),
                close: false,
            }),
            Condvar::new(),
        ));
        let counters = Arc::new(Counters::default());
        // Armed, not ready: the pacer only ever hands a pair over when this is
        // set, and the thread is what builds the pipeline + session on the first
        // one (and clears it again if that fails, which puts the pacer back on
        // staged CPU frames). Starting false would deadlock the two: nobody
        // sends a pair, so nobody builds, so nobody is ready.
        counters
            .warp_ready
            .store(warp_enabled(), Ordering::Relaxed);
        let thread_shared = shared.clone();
        let thread_counters = counters.clone();
        let handle = std::thread::Builder::new()
            .name("dcomp-video".into())
            .spawn(move || {
                crate::media_hooks::note_thread_role("video");
                // f209: the MF path resizes the swapchain to the decoded size,
                // so the layer is mutable here.
                let mut layer = layer;
                // This thread owns a 3-4 ms copy and a vblank deadline: it must
                // not be preempted by the UI thread or the rest of the desktop.
                unsafe {
                    use windows::Win32::System::Threading::{
                        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
                    };
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                }
                let mut warp_state = WarpState::Cold;
                // Step-by-step trace for the first warp job (`BILI_WARP_TRACE`
                // is always on for the first two, so a crash has a last line).
                let mut warp_trace = true;
                let mut last_pair = u64::MAX;
                let mut scratch: Vec<u8> = Vec::new();
                // f202: the enhancement tier, built on the first frame that
                // needs it (and dropped for the session if building it fails).
                let mut enhance_state = EnhanceState::Cold;
                // Whether `layer.staging()` holds a picture a repaint can reuse.
                let mut has_frame = false;
                // f209: the Media Foundation session this thread owns. It is
                // created here (and only here) because the decoder writes into
                // *this* device's textures.
                let mut mf = MfEngine::default();
                // The plain-copy pipeline, built the first time a tick needs it.
                let mut blit: Option<BlitPipeline> = None;
                // Whether the pair the ticks are drawing with is the one the
                // optical-flow engine has (so a slot does not resubmit it).
                let mut pair_warped = false;
                loop {
                    // Wait for a job (or a close). The timeout is only a safety
                    // net for a lost wake-up.
                    let (job, commands, repaint) = {
                        let (lock, wake) = &*thread_shared;
                        let mut guard = match lock.lock() {
                            Ok(guard) => guard,
                            Err(_) => return,
                        };
                        while guard.pending.is_none()
                            && guard.ticks.is_empty()
                            && !guard.close
                            && !guard.repaint
                            && guard.commands.is_empty()
                        {
                            let (next, _) = match wake.wait_timeout(guard, Duration::from_millis(100))
                            {
                                Ok(pair) => pair,
                                Err(_) => return,
                            };
                            guard = next;
                        }
                        if guard.close {
                            // Release anything still queued so the pool sees it.
                            guard.retired.clear();
                            guard.pending = None;
                            guard.ticks.clear();
                            guard.commands.clear();
                            return;
                        }
                        let commands: Vec<SinkCommand> = guard.commands.drain(..).collect();
                        let repaint = std::mem::replace(&mut guard.repaint, false);
                        let job = match guard.ticks.pop_front() {
                            Some(tick) => Some(tick),
                            None => guard.pending.take(),
                        };
                        (job, commands, repaint)
                    };
                for command in commands {
                    mf.command(
                        command,
                        &gpu,
                        &mut layer,
                        control.as_ref(),
                        &thread_counters,
                    );
                }
                let Some(job) = job else {
                    if repaint && has_frame {
                        // f202: a repaint request. Nothing new arrived, so the
                        // staging texture is the frame on screen; run the tier
                        // (or the plain copy) over it again and present.
                        present_staged(
                            &mut enhance_state,
                            &gpu,
                            &layer,
                            &thread_counters,
                        );
                    }
                    continue;
                };

                match job {
                        Job::Frame(frame, pooled) => {
                            let bytes = frame.as_bytes();
                            let staged = if frame.width() == layer.width
                                && frame.height() == layer.height
                            {
                                layer.stage(bytes, true)
                            } else {
                                // Frames are a fixed 960x540 in this player, but a
                                // live stream or a future decoder may hand over
                                // another size. Letterbox it into the layer instead
                                // of rebuilding the whole composition tree from a
                                // worker thread.
                                let width = layer.width as usize;
                                let height = layer.height as usize;
                                scratch.resize(width * height * 4, 0);
                                let copy_w = (frame.width() as usize).min(width);
                                let copy_h = (frame.height() as usize).min(height);
                                let src_row = frame.width() as usize * 4;
                                let dst_row = width * 4;
                                for y in 0..copy_h {
                                    scratch[y * dst_row..y * dst_row + copy_w * 4]
                                        .copy_from_slice(&bytes[y * src_row..y * src_row + copy_w * 4]);
                                }
                                layer.stage(&scratch, true)
                            };

                            // The CPU copy is done: the buffer belongs to the pool.
                            if let Ok(mut guard) = thread_shared.0.lock() {
                                guard.retired.push((frame, pooled));
                            }

                            match staged {
                                Ok(ms) => {
                                    has_frame = true;
                                    thread_counters
                                        .stage_us
                                        .store((ms * 1000.0) as u64, Ordering::Relaxed);
                                }
                                Err(error) => {
                                    crate::debug_log(&format!("dcomp video stage failed: {error}"));
                                    continue;
                                }
                            }

                            // f202: present it through the tier (or the plain
                            // copy) - the same call a paused repaint uses.
                            present_staged(
                                &mut enhance_state,
                                &gpu,
                                &layer,
                                &thread_counters,
                            );
                        }
                        Job::Warp { a, b, phase, pair } => {
                            if matches!(warp_state, WarpState::Cold) {
                                warp_state = build_warp(
                                    &gpu,
                                    &thread_counters,
                                    layer.width,
                                    layer.height,
                                );
                            }
                            let new_pair = pair != last_pair;
                            let WarpState::Ready { pipeline, engine, views } = &mut warp_state
                            else {
                                let reason = match &warp_state {
                                    WarpState::Off(reason) => reason.clone(),
                                    _ => "cold".to_string(),
                                };
                                crate::debug_log(&format!("dcomp warp skip reason={reason}"));
                                continue;
                            };
                            let started = std::time::Instant::now();
                            let Some((grid_w, grid_h)) = engine.grid() else {
                                continue;
                            };
                            // The views belong to this session; `submit` swaps
                            // the textures' roles, so the views are swapped with
                            // them below (`views[0]` is always the pair's A).
                            let srv_a = views[0].clone();
                            let srv_b = views[1].clone();
                            let srv_flow = views[2].clone();
                            if new_pair {
                                // Both frames are uploaded every pair: the
                                // continuous-pair optimisation trades one upload
                                // for a stale-texture risk the pacer cannot verify
                                // per frame, and 2 x 8 MB at 30 pairs/s is 0.5 GB/s
                                // of a several-hundred GB/s bus.
                                engine.reset();
                                if !engine.submit(a.as_bytes(), b.as_bytes(), a.width(), a.height()) {
                                    let reason = engine.off_reason().unwrap_or("submit_failed");
                                    crate::debug_log(&format!(
                                        "dcomp warp submit failed reason={reason}"
                                    ));
                                    warp_state = WarpState::Off(reason.to_string());
                                    thread_counters.warp_ready.store(false, Ordering::Relaxed);
                                    continue;
                                }
                                // The engine runs asynchronously on its own queue:
                                // this event is the fence that says the field is
                                // complete (f199's third structural obstacle).
                                pipeline.fence();
                                let gated = pipeline.wait(50);
                                if !gated {
                                    crate::debug_log("dcomp warp gate timeout ms=50");
                                }
                                thread_counters.gate_us.store(pipeline.gate_us, Ordering::Relaxed);
                                // `submit` swaps the session's texture roles
                                // (b of this pair is a of the next one), so the
                                // views have to swap with them or the next draw
                                // warps in the wrong direction.
                                views.swap(0, 1);
                                if warp_trace {
                                    warp_trace = false;
                                    crate::debug_log(&format!(
                                        "dcomp warp trace pair={pair} gate_ok={} gate_us={}                                          w={} h={} grid={grid_w}x{grid_h} phase={phase:.3}",
                                        u8::from(gated),
                                        pipeline.gate_us,
                                        a.width(),
                                        a.height()
                                    ));
                                }
                                last_pair = pair;
                            }
                            let swap_rb = engine.input_is_rgba();
                            let width = layer.width;
                            let height = layer.height;
                            let swap = layer.swap.clone();
                            // f202: with a tier on, the warp cannot draw into
                            // the backbuffer it will be sampled from - it draws
                            // into the tier's target and the chain finishes the
                            // frame. If anything in that path fails the warp is
                            // drawn straight into the backbuffer again, so the
                            // frame is still a picture and not a black rectangle.
                            let enhance_mode = enhance::desired();
                            let mut enhanced = false;
                            let mut enhanced_cost = u64::MAX;
                            let drawn = (|| -> Result<(), String> {
                                let params = Params {
                                    phase,
                                    grid: crate::player::ofa::GRID,
                                    grid_w,
                                    grid_h,
                                    width,
                                    height,
                                    swap_rb,
                                };
                                // No frame-latency wait here on purpose: it
                                // paces the loop to the compositor's *own*
                                // signal, and f196 measured that this wait plus
                                // a non-blocking present halves the rate (the
                                // wait consumes the signal, the present then has
                                // nothing to hand over). `Present(DO_NOT_WAIT)`
                                // already drops a frame the compositor cannot
                                // take, which is the behaviour we want.
                                if enhance_mode == enhance::OFF {
                                    pipeline.draw(&swap, &srv_a, &srv_b, &srv_flow, params)
                                } else {
                                    let (rtv, srv) = warp_target(&mut enhance_state, &gpu, &layer)?;
                                    pipeline.draw_into(&rtv, &srv_a, &srv_b, &srv_flow, params)?;
                                    drop(rtv);
                                    enhanced_cost =
                                        enhance_warp(&mut enhance_state, srv, &layer)?;
                                    enhanced = true;
                                    Ok(())
                                }
                            })();
                            if drawn.is_ok() && warp_trace {
                                warp_trace = false;
                                crate::debug_log("dcomp warp trace draw ok");
                            }
                            if enhanced_cost != u64::MAX {
                                thread_counters
                                    .enhance_us
                                    .store(enhanced_cost, Ordering::Relaxed);
                                thread_counters
                                    .enhance_mode
                                    .store(enhance_mode as u64, Ordering::Relaxed);
                            }
                            if let Err(error) = drawn {
                                crate::debug_log(&format!("dcomp warp draw failed: {error}"));
                                if enhance_mode != enhance::OFF {
                                    // The tier is the only thing that broke:
                                    // keep the pair, drop the tier, and draw the
                                    // warp the ordinary way.
                                    enhance::note_failure(&error);
                                    enhance::set_desired(enhance::OFF);
                                    let params = Params {
                                        phase,
                                        grid: crate::player::ofa::GRID,
                                        grid_w,
                                        grid_h,
                                        width,
                                        height,
                                        swap_rb,
                                    };
                                    if let Err(retry) =
                                        pipeline.draw(&swap, &srv_a, &srv_b, &srv_flow, params)
                                    {
                                        crate::debug_log(&format!(
                                            "dcomp warp draw failed twice: {retry}"
                                        ));
                                        warp_state = WarpState::Off(retry);
                                        thread_counters.warp_ready.store(false, Ordering::Relaxed);
                                        continue;
                                    }
                                } else {
                                    warp_state = WarpState::Off(error);
                                    thread_counters.warp_ready.store(false, Ordering::Relaxed);
                                    continue;
                                }
                            }
                            match layer.present_backbuffer() {
                                Ok(hr) if hr.0 == 0 => {
                                    thread_counters.presented.fetch_add(1, Ordering::Relaxed);
                                    thread_counters.warped.fetch_add(1, Ordering::Relaxed);
                                    if enhanced {
                                        thread_counters
                                            .enhance_frames
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Ok(hr) => {
                                    crate::debug_log(&format!(
                                        "dcomp warp present hr=0x{:08X}",
                                        hr.0 as u32
                                    ));
                                }
                                Err(error) => {
                                    crate::debug_log(&format!("dcomp warp present failed: {error}"));
                                }
                            }
                            thread_counters
                                .warp_us
                                .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                        }
                        Job::Tick { video_ms, pair: _pair } => {
                            // f209: one display slot of an MF session. Everything
                            // a picture needs is already on the GPU when this
                            // runs; the pacer contributed only the clock.
                            thread_counters.mf_ticks.fetch_add(1, Ordering::Relaxed);
                            let pre = std::time::Instant::now();
                            let Some(tick) = mf.tick(video_ms, &thread_counters) else {
                                continue;
                            };
                            thread_counters
                                .mf_pre_us
                                .store(pre.elapsed().as_micros() as u64, Ordering::Relaxed);
                            let started = std::time::Instant::now();
                            let width = layer.width;
                            let height = layer.height;
                            let tier = enhance::desired() != enhance::OFF;
                            // A new source pair: convert both sides, then let the
                            // optical-flow engine see it. The conversion is the
                            // only place the picture changes form, and it happens
                            // on the GPU (video processor), never on the CPU.
                            if tick.new_pair {
                                if matches!(warp_state, WarpState::Cold) && mf.want_interp {
                                    warp_state = build_warp(
                                        &gpu,
                                        &thread_counters,
                                        width,
                                        height,
                                    );
                                }
                                let ofa_ready = matches!(warp_state, WarpState::Ready { .. });
                                thread_counters
                                    .mf_warp
                                    .store(ofa_ready, Ordering::Relaxed);
                                let format = match &warp_state {
                                    WarpState::Ready { engine, .. } if ofa_ready => {
                                        if engine.input_is_rgba() {
                                            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM
                                        } else {
                                            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM
                                        }
                                    }
                                    _ => windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                                };
                                if let Err(error) = mf.convert_pair(&gpu, format) {
                                    crate::debug_log(&format!("nv12 convert failed: {error}"));
                                    warp_state = WarpState::Off(error);
                                    pair_warped = false;
                                    continue;
                                }
                                pair_warped = false;
                                if ofa_ready {
                                    let submitted = {
                                        let WarpState::Ready { engine, .. } = &mut warp_state
                                        else {
                                            continue;
                                        };
                                        match (mf.slot_texture(0), mf.slot_texture(1)) {
                                            (Some(a), Some(b)) => {
                                                // SAFETY: both textures were built
                                                // here, on this device, in this
                                                // session's input format and size.
                                                let (raw_a, raw_b) = (
                                                    windows::core::Interface::as_raw(a)
                                                        as *mut std::ffi::c_void,
                                                    windows::core::Interface::as_raw(b)
                                                        as *mut std::ffi::c_void,
                                                );
                                                unsafe { engine.submit_textures(raw_a, raw_b) }
                                            }
                                            _ => false,
                                        }
                                    };
                                    let WarpState::Ready {
                                        engine,
                                        pipeline,
                                        views,
                                    } = &mut warp_state
                                    else {
                                        continue;
                                    };
                                    if submitted {
                                        pipeline.fence();
                                        let gated = pipeline.wait(50);
                                        thread_counters
                                            .gate_us
                                            .store(pipeline.gate_us, Ordering::Relaxed);
                                        if !gated {
                                            crate::debug_log("dcomp mf warp gate timeout ms=50");
                                        }
                                        // `submit_textures` swaps the textures'
                                        // roles (`b` becomes the next pair's `a`),
                                        // so the views swap with them.
                                        views.swap(0, 1);
                                        pair_warped = true;
                                    } else {
                                        let reason =
                                            engine.off_reason().unwrap_or("ofa_submit_failed");
                                        crate::debug_log(&format!(
                                            "dcomp mf warp off reason={reason}"
                                        ));
                                        warp_state = WarpState::Off(reason.to_string());
                                        thread_counters
                                            .mf_warp
                                            .store(false, Ordering::Relaxed);
                                    }
                                }
                            }
                            if !pair_warped && blit.is_none() {
                                match BlitPipeline::new(&gpu.device, &gpu.context) {
                                    Ok(pipeline) => blit = Some(pipeline),
                                    Err(error) => {
                                        crate::debug_log(&format!(
                                            "dcomp mf blit off reason={error}"
                                        ));
                                        continue;
                                    }
                                }
                            }
                            // One render target for both branches: the tier's
                            // target when a tier is on, the backbuffer otherwise.
                            // Either way the view is dropped before `Present`.
                            let target = if tier {
                                match warp_target(&mut enhance_state, &gpu, &layer) {
                                    Ok((rtv, srv)) => Some((rtv, Some(srv))),
                                    Err(error) => {
                                        crate::debug_log(&format!(
                                            "dcomp mf tier target failed: {error}"
                                        ));
                                        enhance::set_desired(enhance::OFF);
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            let back_view = match &target {
                                Some((rtv, _)) => Some(rtv.clone()),
                                None => backbuffer_view(&gpu, &layer),
                            };
                            let mut drawn = Err("no render target".to_string());
                            if let Some(rtv) = back_view.as_ref() {
                                drawn = draw_mf_slot(
                                    &mut warp_state,
                                    blit.as_ref(),
                                    &mut mf,
                                    rtv,
                                    width,
                                    height,
                                    tick.phase,
                                    pair_warped,
                                );
                            }
                            drop(back_view);
                            if let Some((rtv, srv)) = target {
                                drop(rtv);
                                match drawn {
                                    Ok(()) => {
                                        if let Some(srv) = srv {
                                            match enhance_warp(&mut enhance_state, srv, &layer) {
                                                Ok(cost) => {
                                                    thread_counters
                                                        .enhance_us
                                                        .store(cost, Ordering::Relaxed);
                                                    thread_counters.enhance_mode.store(
                                                        enhance::desired() as u64,
                                                        Ordering::Relaxed,
                                                    );
                                                    thread_counters
                                                        .enhance_frames
                                                        .fetch_add(1, Ordering::Relaxed);
                                                }
                                                Err(error) => {
                                                    crate::debug_log(&format!(
                                                        "dcomp mf enhance failed: {error}"
                                                    ));
                                                    enhance::note_failure(&error);
                                                    enhance::set_desired(enhance::OFF);
                                                }
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        crate::debug_log(&format!(
                                            "dcomp mf draw failed: {error}"
                                        ));
                                        enhance::note_failure(&error);
                                        enhance::set_desired(enhance::OFF);
                                        // The tier is off now: draw the slot into
                                        // the backbuffer so the frame is a picture.
                                        if let Some(view) = backbuffer_view(&gpu, &layer) {
                                            let _ = draw_mf_slot(
                                                &mut warp_state,
                                                blit.as_ref(),
                                                &mut mf,
                                                &view,
                                                width,
                                                height,
                                                tick.phase,
                                                pair_warped,
                                            );
                                        }
                                    }
                                }
                            } else if let Err(error) = drawn {
                                crate::debug_log(&format!("dcomp mf draw failed: {error}"));
                            }
                            match layer.present_backbuffer() {
                                Ok(hr) if hr.0 == 0 => {
                                    thread_counters.presented.fetch_add(1, Ordering::Relaxed);
                                    if pair_warped {
                                        thread_counters.warped.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Ok(hr) => crate::debug_log(&format!(
                                    "dcomp mf present hr=0x{:08X}",
                                    hr.0 as u32
                                )),
                                Err(error) => {
                                    crate::debug_log(&format!("dcomp mf present failed: {error}"))
                                }
                            }
                            // What the tick cost this thread: the number the
                            // 120 fps budget is judged on.
                            thread_counters.warp_us.store(
                                started.elapsed().as_micros() as u64,
                                Ordering::Relaxed,
                            );
                        }
                    }
                }
            })
            .ok();

        Arc::new(Self {
            shared,
            counters,
            closed: AtomicBool::new(false),
            thread: Mutex::new(handle),
            mf_key: Mutex::new(None),
        })
    }

    /// Hand one frame over, replacing one that has not been shown yet.
    ///
    /// Never blocks: the caller is the pacer and must not wait for a
    /// presentation.
    pub(crate) fn submit(&self, buffer: FrameBuf, pooled: bool) {
        self.counters.pushed.fetch_add(1, Ordering::Relaxed);
        self.push(Job::Frame(buffer, pooled));
    }

    /// f200: hand over one display slot of an optical-flow pair.
    ///
    /// Returns whether the sink took it. `false` means the GPU path is not up
    /// (no runtime, session refused, a draw failed): the caller must then produce
    /// a frame the ordinary way. Nothing is queued in that case.
    pub(crate) fn submit_warp(&self, a: FrameBuf, b: FrameBuf, phase: f32, pair: u64) -> bool {
        if !self.warp_ready() || self.is_closed() {
            return false;
        }
        self.counters.pushed.fetch_add(1, Ordering::Relaxed);
        self.push(Job::Warp { a, b, phase, pair });
        true
    }

    /// f209: ask for an MF session on this sink's own device.
    ///
    /// The open itself runs on the presenting thread (the decoder must write
    /// into that thread's device), so this only queues the request: the pacer
    /// then polls [`Self::mf_state`] for the verdict and falls back to the
    /// ffmpeg pipe if it is `Failed`.
    pub(crate) fn mf_open(&self, source: MfSource, start_ms: u64) {
        if let Ok(mut slot) = self.mf_key.lock() {
            *slot = Some(source.key());
        }
        self.command(SinkCommand::MfOpen { source, start_ms });
    }

    /// Whether the session can seek the decoder it already has instead of
    /// opening a new one (`key` = [`MfSource::key`] of the stream).
    pub(crate) fn mf_reuse(&self, key: &str) -> bool {
        self.mf_state() == MfState::Ready
            && self
                .mf_key
                .lock()
                .map(|slot| slot.as_deref() == Some(key))
                .unwrap_or(false)
    }

    /// f209: restart the MF session at `start_ms` (a seek).
    pub(crate) fn mf_seek(&self, start_ms: u64) {
        self.counters
            .mf_state
            .store(MfState::Opening.code(), Ordering::Relaxed);
        self.command(SinkCommand::MfSeek { start_ms });
    }

    /// f209: drop the MF session (the pacer is leaving this path).
    pub(crate) fn mf_close(&self) {
        self.command(SinkCommand::MfClose);
    }

    /// f209: one display slot at `video_ms` of the stream.
    ///
    /// Returns whether it was queued. The picture is chosen from the decoder's
    /// own surfaces by point in time, so nothing about it crosses this call.
    pub(crate) fn submit_tick(&self, video_ms: u64, pair: u64) -> bool {
        if self.is_closed() || self.mf_state() != MfState::Ready {
            return false;
        }
        self.counters.pushed.fetch_add(1, Ordering::Relaxed);
        self.push(Job::Tick { video_ms, pair });
        true
    }

    pub(crate) fn mf_state(&self) -> MfState {
        MfState::from_code(self.counters.mf_state.load(Ordering::Relaxed))
    }

    /// Whether the GPU warp is up for the MF path (the pacer caps its grid on
    /// this: no warp means 60, not 120).
    pub(crate) fn mf_warp(&self) -> bool {
        self.counters.mf_warp.load(Ordering::Relaxed)
    }

    /// Tell the engine whether the pacer wants interpolation this session.
    pub(crate) fn set_mf_interp(&self, on: bool) {
        self.counters
            .mf_interp_hint
            .store(u64::from(on), Ordering::Relaxed);
    }

    fn command(&self, command: SinkCommand) {
        if self.is_closed() {
            return;
        }
        if let Ok(mut state) = self.shared.0.lock() {
            state.commands.push_back(command);
        }
        self.shared.1.notify_one();
    }

    fn push(&self, job: Job) {
        if let Ok(mut state) = self.shared.0.lock() {
            if matches!(job, Job::Tick { .. }) {
                // Slots are ordered events: queue them, and drop the *oldest*
                // when the sink is more than a burst behind.
                if state.ticks.len() >= 8 {
                    state.ticks.pop_front();
                    self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
                }
                state.ticks.push_back(job);
                drop(state);
                self.shared.1.notify_one();
                return;
            }
            // The job that was waiting is dropped in favour of this one - but a
            // frame still has to go back to the blend pool, or the pool starves
            // and every frame allocates.
            if let Some(replaced) = state.pending.take() {
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
                // Never shown, so this allocation is ours alone.
                if let Job::Frame(frame, pooled) = replaced {
                    state.retired.push((frame, pooled));
                }
            }
            state.pending = Some(job);
        }
        self.shared.1.notify_one();
    }

    /// Whether [`Self::close`] has run: a pacer that kept the handle must stop
    /// submitting instead of filling a mailbox nobody drains.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// f202: re-present the frame already staged through the tier now selected.
    ///
    /// Used when the 画质增强 row changes while the picture is not running (a
    /// paused player produces no frames): without it the menu would appear to do
    /// nothing until playback resumes.
    pub(crate) fn repaint(&self) {
        if self.is_closed() {
            return;
        }
        if let Ok(mut state) = self.shared.0.lock() {
            state.repaint = true;
        }
        self.shared.1.notify_one();
    }

    /// Whether the GPU warp is built (any thread; one relaxed load).
    pub(crate) fn warp_ready(&self) -> bool {
        self.counters.warp_ready.load(Ordering::Relaxed)
    }

    /// Take back the frames whose staging copy is done (UI thread, each frame).
    ///
    /// The caller recycles them into the blend pool. Bounded by two (the
    /// pending slot is one frame deep), so this never grows.
    pub(crate) fn take_retired(&self) -> Vec<(FrameBuf, bool)> {
        match self.shared.0.lock() {
            Ok(mut state) => state.retired.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Stop the thread and wait for it (the swapchain is dropped there).
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut state) = self.shared.0.lock() {
            state.close = true;
            state.pending = None;
        }
        self.shared.1.notify_all();
        if let Ok(mut slot) = self.thread.lock() {
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
        }
    }

    /// Ask the thread to stop **without** waiting for it (f212).
    ///
    /// [`Self::close`] joins, and the thread can be inside a wait that has
    /// nothing to do with the window: an MF byte stream stuck on a dead mirror
    /// is a 30 s timeout (measured), and a window teardown that joined it froze
    /// the whole application for its length — the 27 s `open_ms` of the window
    /// opened right after a close, and the reason "关掉再开" could stall.
    ///
    /// What a teardown has to guarantee is not that the thread is gone but that
    /// it stops being *asked* to draw: the device, swapchain and decoder stay
    /// owned by that thread and are released there when it exits, and
    /// DirectComposition reference-counts the visual's content, so dropping the
    /// composition tree after this call is safe. The join handle is dropped with
    /// the sink (the thread keeps its own share of the state alive), and a later
    /// [`Self::close`] returns at once because the flag is already set.
    pub(crate) fn close_detached(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut state) = self.shared.0.lock() {
            state.close = true;
            state.pending = None;
        }
        self.shared.1.notify_all();
        if let Ok(mut slot) = self.thread.lock() {
            slot.take();
        }
    }
}

/// Build the shaders and the optical-flow session **on this thread's device**.
///
/// Runs on the first warp job, never before: a machine without `nvofapi64.dll`
/// plays the whole session on the staged path and only pays one branch.
/// Whether the GPU warp may be built at all (`BILI_DCOMP_WARP=0` turns it off,
/// which is the A/B handle for "is the backbuffer path the thing that broke?").
fn warp_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !std::env::var("BILI_DCOMP_WARP")
            .is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
    })
}

fn build_warp(gpu: &Gpu, counters: &Arc<Counters>, width: u32, height: u32) -> WarpState {
    let started = std::time::Instant::now();
    let pipeline = match WarpPipeline::new(&gpu.device, &gpu.context) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            crate::debug_log(&format!("dcomp warp off reason=pipeline error={error}"));
            return WarpState::Off(format!("pipeline: {error}"));
        }
    };
    // SAFETY: both interfaces belong to `gpu`, which this thread owns for the
    // rest of the sink's life; the engine borrows them and never releases them.
    let mut engine = unsafe {
        use windows::core::Interface;
        crate::player::ofa::Ofa::attach(
            gpu.device.as_raw() as *mut std::ffi::c_void,
            gpu.context.as_raw() as *mut std::ffi::c_void,
        )
    };
    // The session is created here, on the sink's own device: this is f199's
    // first structural obstacle (three D3D11 devices and a shared texture that
    // cannot be a render target) removed at the root.
    if !engine.ensure(width.max(1), height.max(1)) {
        let reason = engine.off_reason().unwrap_or("session_failed");
        crate::debug_log(&format!(
            "dcomp warp off reason=session error={reason} frame={width}x{height}"
        ));
        return WarpState::Off(reason.to_string());
    }
    let views = match engine.take_views() {
        Some((a, b, flow)) => {
            let adopted = (|| -> Result<_, String> {
                Ok([
                    WarpPipeline::adopt(a)?,
                    WarpPipeline::adopt(b)?,
                    WarpPipeline::adopt(flow)?,
                ])
            })();
            match adopted {
                Ok(views) => views,
                Err(error) => {
                    crate::debug_log(&format!("dcomp warp views failed: {error}"));
                    return WarpState::Off(error);
                }
            }
        }
        None => {
            crate::debug_log("dcomp warp off reason=no_views");
            return WarpState::Off("no_views".into());
        }
    };
    counters.warp_ready.store(true, Ordering::Relaxed);
    crate::debug_log(&format!(
        "dcomp warp ready frame={width}x{height} device=video-sink ms={:.2}",
        started.elapsed().as_secs_f64() * 1000.0
    ));
    WarpState::Ready {
        pipeline,
        engine,
        views,
    }
}

/// f202: the enhancement pipeline's lifecycle on the presenting thread.
///
/// Built on the first frame that asks for a tier, never before: a session that
/// keeps 关闭 pays one branch and nothing else. `Off` is sticky for the session
/// (the reason is logged once and shown by the UI).
enum EnhanceState {
    Cold,
    Ready(Box<Enhance>),
    Off(String),
}

fn ensure_enhance(
    state: &mut EnhanceState,
    gpu: &Gpu,
    width: u32,
    height: u32,
) -> Result<(), String> {
    match state {
        EnhanceState::Ready(_) => Ok(()),
        EnhanceState::Off(reason) => Err(reason.clone()),
        EnhanceState::Cold => {
            match Enhance::new(&gpu.device, &gpu.context, width, height) {
                Ok(pipeline) => {
                    crate::debug_log(&format!(
                        "enhance ready frame={width}x{height} device=video-sink mode={}",
                        super::enhance::desired()
                    ));
                    *state = EnhanceState::Ready(Box::new(pipeline));
                    Ok(())
                }
                Err(error) => {
                    crate::debug_log(&format!("enhance off reason=build error={error}"));
                    *state = EnhanceState::Off(error.clone());
                    Err(error)
                }
            }
        }
    }
}

/// Draw the tier over a CPU frame and present it.
fn enhance_frame(
    state: &mut EnhanceState,
    gpu: &Gpu,
    layer: &Layer,
) -> Result<(windows::core::HRESULT, u64, u32), String> {
    ensure_enhance(state, gpu, layer.width, layer.height)?;
    let mode = super::enhance::desired();
    let EnhanceState::Ready(pipeline) = state else {
        return Err("enhance not ready".to_string());
    };
    // Same order as `Layer::submit`: the CPU copy already happened, and the
    // compositor wait comes right before the GPU work that lands in the buffer
    // about to be presented.
    layer.wait_frame();
    pipeline.draw(
        mode,
        Source::Staging(layer.staging()),
        &layer.swap,
    )?;
    let cost = pipeline.cost_us;
    if pipeline.frames < 3 || pipeline.frames % 60 == 1 {
        crate::debug_log(&format!(
            "enhance mode={mode} label={} cost_us={cost} frames={} size={}x{}",
            super::enhance::label(mode),
            pipeline.frames,
            pipeline.size.0,
            pipeline.size.1
        ));
    }
    let hr = layer
        .present_backbuffer()
        .map_err(|error| format!("present: {error}"))?;
    Ok((hr, cost, mode))
}

/// Present the picture already in `layer.staging()`: through the 画质增强 tier
/// when one is selected, otherwise through the two copies the player has always
/// used. Every failure falls through to the copies, so the frame on screen is
/// never black.
fn present_staged(
    enhance_state: &mut EnhanceState,
    gpu: &Gpu,
    layer: &Layer,
    counters: &Counters,
) {
    if enhance::desired() != enhance::OFF {
        match enhance_frame(enhance_state, gpu, layer) {
            Ok((hr, cost_us, mode)) => {
                if hr.0 == 0 {
                    counters.presented.fetch_add(1, Ordering::Relaxed);
                    counters.enhance_frames.fetch_add(1, Ordering::Relaxed);
                } else {
                    crate::debug_log(&format!(
                        "enhance mode={mode} present hr=0x{:08X}",
                        hr.0 as u32
                    ));
                }
                counters.enhance_us.store(cost_us, Ordering::Relaxed);
                counters.enhance_mode.store(mode as u64, Ordering::Relaxed);
                return;
            }
            Err(error) => {
                crate::debug_log(&format!(
                    "enhance mode={} failed: {error}",
                    enhance::label(enhance::desired())
                ));
                // The tier is off for the session (the menu row snaps back to
                // 关闭), but the pipeline stays: a build failure already
                // recorded `Off` inside `ensure_enhance`, and anything else is
                // per-tier, so the next pick must be able to build its chain.
                enhance::note_failure(&error);
                enhance::set_desired(enhance::OFF);
            }
        }
    }
    match layer.submit() {
        Ok(hr) if hr.0 == 0 => {
            counters.presented.fetch_add(1, Ordering::Relaxed);
        }
        Ok(hr) => {
            crate::debug_log(&format!("dcomp video present hr=0x{:08X}", hr.0 as u32));
        }
        Err(error) => {
            crate::debug_log(&format!("dcomp video present failed: {error}"));
        }
    }
}

/// The target the optical-flow warp draws into while a tier is on.
fn warp_target(
    state: &mut EnhanceState,
    gpu: &Gpu,
    layer: &Layer,
) -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView), String> {
    ensure_enhance(state, gpu, layer.width, layer.height)?;
    let EnhanceState::Ready(pipeline) = state else {
        return Err("enhance not ready".to_string());
    };
    pipeline.warp_target()
}

/// Run the tier over what the warp just drew (no present: the caller presents).
fn enhance_warp(
    state: &mut EnhanceState,
    srv: ID3D11ShaderResourceView,
    layer: &Layer,
) -> Result<u64, String> {
    let mode = super::enhance::desired();
    let EnhanceState::Ready(pipeline) = state else {
        return Err("enhance not ready".to_string());
    };
    pipeline.draw(mode, Source::View(srv), &layer.swap)?;
    let cost = pipeline.cost_us;
    if pipeline.frames < 3 || pipeline.frames % 60 == 1 {
        crate::debug_log(&format!(
            "enhance mode={mode} label={} cost_us={cost} warped={} frames={}",
            super::enhance::label(mode),
            pipeline.frames,
            pipeline.size.0
        ));
    }
    Ok(cost)
}

/// f209: draw one MF display slot into `rtv`.
///
/// `warped` means the optical-flow engine already holds this pair and the flow
/// field is complete, so the slot is the GPU warp (motion-compensated). Without
/// it the slot is a plain copy of the converted picture: no interpolation, but
/// still zero CPU bytes and one draw.
#[allow(clippy::too_many_arguments)]
fn draw_mf_slot(
    warp: &mut WarpState,
    blit: Option<&BlitPipeline>,
    engine: &mut MfEngine,
    rtv: &ID3D11RenderTargetView,
    width: u32,
    height: u32,
    phase: f32,
    warped: bool,
) -> Result<(), String> {
    if warped {
        let WarpState::Ready {
            pipeline,
            engine: ofa,
            views,
        } = warp
        else {
            return Err("warp not ready".to_string());
        };
        let Some((grid_w, grid_h)) = ofa.grid() else {
            return Err("warp grid unavailable".to_string());
        };
        let params = Params {
            phase,
            grid: crate::player::ofa::GRID,
            grid_w,
            grid_h,
            width,
            height,
            swap_rb: ofa.input_is_rgba(),
        };
        return pipeline.draw_into(rtv, &views[0], &views[1], &views[2], params);
    }
    let view = engine
        .slot_view(0)
        .ok_or_else(|| "convert slot missing".to_string())?;
    let blit = blit.ok_or_else(|| "blit pipeline missing".to_string())?;
    blit.draw_into(rtv, view, width, height)
}

/// The swapchain's backbuffer as a render target, dropped before `Present`.
fn backbuffer_view(gpu: &Gpu, layer: &Layer) -> Option<ID3D11RenderTargetView> {
    unsafe {
        let back: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D =
            layer.swap.GetBuffer(0).ok()?;
        let mut view: Option<ID3D11RenderTargetView> = None;
        gpu.device
            .CreateRenderTargetView(&back, None, Some(&mut view))
            .ok()?;
        view
    }
}

/// One display slot's worth of decoded pictures.
struct MfTick {
    a: GpuPicture,
    b: GpuPicture,
    phase: f32,
    /// The pair is not the one currently on the GPU (convert + flow needed).
    new_pair: bool,
}

/// The Media Foundation session, owned by the presenting thread.
///
/// The decoder writes into *this* device's textures, so it cannot live anywhere
/// else: creating it here is what makes the zero-upload hand-off possible at
/// all. It is driven by the pacer's ticks (the audio clock) rather than a clock
/// of its own, so 音画同步 stays the pacer's business as before.
#[derive(Default)]
struct MfEngine {
    playback: Option<MfPlayback>,
    /// Why the last session failed (surfaced to the pacer, which then falls
    /// back to the ffmpeg pipe).
    reason: Option<String>,
    /// The pacer asked for interpolation: 4x only happens when the warp is also
    /// up, which is why this is a hint rather than a guarantee.
    want_interp: bool,
    /// The pair last handed to [`Self::convert_pair`].
    current: Option<MfTick>,
}

impl MfEngine {
    /// Execute one session command (pacer thread's request, this thread's work).
    fn command(
        &mut self,
        command: SinkCommand,
        gpu: &Gpu,
        layer: &mut Layer,
        control: Option<&Arc<crate::player::PlaybackControl>>,
        counters: &Counters,
    ) {
        match command {
            SinkCommand::MfOpen { source, start_ms } => {
                self.playback = None;
                self.current = None;
                self.reason = None;
                counters.mf_state.store(MfState::Opening.code(), Ordering::Relaxed);
                let started = std::time::Instant::now();
                match MfPlayback::open(gpu, &source, start_ms) {
                    Ok(playback) => {
                        let (width, height) = playback.size;
                        let fps = playback.fps;
                        // The layer is the decoder's own size: the picture is
                        // drawn 1:1 and DComp scales it into the band, so the
                        // warp never has to resample (its flow field is in source
                        // pixels). The window's geometry follows through the
                        // control's frame size.
                        if let Err(error) = layer.resize(gpu, width, height) {
                            crate::debug_log(&format!("dcomp mf layer resize failed: {error}"));
                        }
                        if let Some(control) = control {
                            control.set_frame_size(crate::player::FrameSize {
                                w: width,
                                h: height,
                            });
                        }
                        self.playback = Some(playback);
                        counters.mf_state.store(MfState::Ready.code(), Ordering::Relaxed);
                        crate::debug_log(&format!(
                            "decode=mf hw=1 size={width}x{height} fps={fps:.2} open_ms={:.0} start_ms={start_ms} source={}",
                            started.elapsed().as_secs_f64() * 1000.0,
                            match &source {
                                MfSource::Url { .. } => "url",
                                MfSource::Path(_) => "file",
                            }
                        ));
                    }
                    Err(reason) => {
                        crate::debug_log(&format!("decode fallback reason={reason}"));
                        self.reason = Some(reason);
                        counters.mf_state.store(MfState::Failed.code(), Ordering::Relaxed);
                    }
                }
            }
            SinkCommand::MfSeek { start_ms } => {
                let Some(playback) = self.playback.as_mut() else {
                    return;
                };
                self.current = None;
                let started = std::time::Instant::now();
                match playback.seek(start_ms) {
                    Ok(()) => {
                        crate::debug_log(&format!(
                            "decode=mf seek target_ms={start_ms} seek_ms={:.1}",
                            started.elapsed().as_secs_f64() * 1000.0
                        ));
                        counters.mf_state.store(MfState::Ready.code(), Ordering::Relaxed);
                    }
                    Err(reason) => {
                        crate::debug_log(&format!("decode fallback reason={reason}"));
                        playback.failed = Some(reason);
                        counters.mf_state.store(MfState::Failed.code(), Ordering::Relaxed);
                    }
                }
            }
            SinkCommand::MfClose => {
                self.playback = None;
                self.current = None;
                counters.mf_state.store(MfState::Idle.code(), Ordering::Relaxed);
            }
        }
    }

    /// The pictures for `video_ms`, decoded ahead as needed.
    fn tick(&mut self, video_ms: u64, counters: &Counters) -> Option<MfTick> {
        self.want_interp = counters.mf_interp_hint.load(Ordering::Relaxed) != 0;
        let playback = self.playback.as_mut()?;
        if playback.failed.is_some() {
            return None;
        }
        playback.pump(video_ms, counters);
        counters
            .mf_queued
            .store(playback.frames.len() as u64, Ordering::Relaxed);
        let Some((a, b, phase)) = playback.choose(video_ms) else {
            let misses = counters.mf_misses.fetch_add(1, Ordering::Relaxed) + 1;
            if misses % 60 == 1 {
                crate::debug_log(&format!(
                    "mf tick no-picture video_ms={video_ms} queued={} front={} back={}",
                    playback.frames.len(),
                    playback.frames.front().map(|f| f.pts_ms).unwrap_or(0),
                    playback.frames.back().map(|f| f.pts_ms).unwrap_or(0)
                ));
            }
            return None;
        };
        let new_pair = self
            .current
            .as_ref()
            .map_or(true, |current| current.a.pts_ms != a.pts_ms);
        playback.retire(a.pts_ms);
        let tick = MfTick {
            a,
            b,
            phase,
            new_pair,
        };
        self.current = Some(MfTick {
            a: tick.a.clone_picture(),
            b: tick.b.clone_picture(),
            phase,
            new_pair,
        });
        Some(tick)
    }

    /// Convert the current pair into the converter's two slots.
    fn convert_pair(
        &mut self,
        gpu: &Gpu,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<(), String> {
        let Some(playback) = self.playback.as_mut() else {
            return Err("没有 MF 会话".to_string());
        };
        let Some(tick) = self.current.as_ref() else {
            return Err("没有可转换的画面".to_string());
        };
        let (width, height) = (tick.a.width, tick.a.height);
        if playback.converter.is_none() {
            playback.converter = Some(super::nv12::Nv12Converter::new(
                &gpu.device,
                &gpu.context,
                format,
            )?);
        }
        let converter = playback.converter.as_mut().expect("created above");
        if converter.format() != format {
            return Err(format!(
                "视频处理器无法输出 {}（回落到 {}），插帧关闭",
                super::nv12::format_name(format),
                super::nv12::format_name(converter.format())
            ));
        }
        converter.ensure(width, height)?;
        converter.convert(0, &tick.a)?;
        // A single frame (no next picture yet) is its own B: the warp then reads
        // one frame twice and the blend is exact.
        converter.convert(1, &tick.b)?;
        Ok(())
    }

    fn slot_view(&self, index: usize) -> Option<&ID3D11ShaderResourceView> {
        self.playback
            .as_ref()?
            .converter
            .as_ref()?
            .view(index)
    }

    fn slot_texture(&self, index: usize) -> Option<&windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        self.playback.as_ref()?.converter.as_ref()?.texture(index)
    }
}

/// One open MF session on the presenting thread.
struct MfPlayback {
    decoder: crate::mediadec::mf::MfVideoDecoder,
    /// Decoded pictures, oldest first. Never more than a handful: the decoder
    /// rotates through ~15 surfaces and holding them stalls it.
    frames: std::collections::VecDeque<GpuPicture>,
    converter: Option<Nv12Converter>,
    start_ms: u64,
    frame_ms: f64,
    fps: f32,
    size: (u32, u32),
    eos: bool,
    /// The first tick of a session waits for a picture at its start position
    /// (a seek lands on the keyframe *before* the target).
    primed: bool,
    dropped: u64,
    failures: u32,
    failed: Option<String>,
}

impl MfPlayback {
    fn open(gpu: &Gpu, source: &MfSource, start_ms: u64) -> Result<Self, String> {
        let decoder = match source {
            MfSource::Url { url, headers } => {
                crate::mediadec::mf::MfVideoDecoder::open(url, headers, &gpu.device, &gpu.context)?
            }
            MfSource::Path(path) => crate::mediadec::mf::MfVideoDecoder::open_path(
                std::path::Path::new(path),
                &gpu.device,
                &gpu.context,
            )?,
        };
        let (width, height, fps) = decoder.info();
        if width == 0 || height == 0 {
            return Err("MF 解码器没有报告画面尺寸".to_string());
        }
        let frame_ms = if fps > 0.5 { 1000.0 / fps as f64 } else { 33.367 };
        Ok(Self {
            decoder,
            frames: std::collections::VecDeque::new(),
            converter: None,
            start_ms,
            frame_ms,
            fps,
            size: (width, height),
            eos: false,
            primed: false,
            dropped: 0,
            failures: 0,
            failed: None,
        })
    }

    fn seek(&mut self, start_ms: u64) -> Result<(), String> {
        self.decoder.seek(Duration::from_millis(start_ms))?;
        self.frames.clear();
        self.start_ms = start_ms;
        self.eos = false;
        self.primed = false;
        Ok(())
    }

    /// Decode until the queue covers `video_ms`, dropping what is behind it.
    ///
    /// The first call of a session is allowed a real budget (the decoder has to
    /// land the keyframe and skip to the target); every later call decodes at
    /// most a few frames with a short timeout so a slow CDN cannot stall the
    /// display.
    fn pump(&mut self, video_ms: u64, counters: &Counters) {
        let first = !self.primed;
        // One frame ahead is what a 4x grid needs: A is the picture being shown
        // and B is the one it interpolates towards. Decoding further ahead only
        // makes the queue deeper than the pool wants and turns pictures that were
        // never needed into "dropped" ones.
        let lookahead = (self.frame_ms * 1.2).round() as u64 + 8;
        let budget = if first {
            Duration::from_millis(400)
        } else {
            Duration::from_millis(8)
        };
        let max_frames = if first { 400 } else { 4 };
        // A bound on the *whole* priming loop, not only on one read: a session
        // that has to skip a long way to its keyframe must not hold the
        // presenting thread for the sum of the per-read budgets.
        let deadline = if first {
            Some(std::time::Instant::now() + Duration::from_millis(1500))
        } else {
            None
        };
        for _ in 0..max_frames {
            if self.eos {
                break;
            }
            if deadline.is_some_and(|deadline| std::time::Instant::now() > deadline) {
                break;
            }
            let covered = self.frames.back().map_or(false, |frame| {
                frame.pts_ms + lookahead >= video_ms
            });
            if covered && !self.frames.is_empty() {
                break;
            }
            let read_started = std::time::Instant::now();
            let read = self.decoder.next_frame(budget);
            let read_us = read_started.elapsed().as_micros() as u64;
            counters.mf_decode_us.store(read_us, Ordering::Relaxed);
            match read {
                Ok(Some(frame)) => {
                    counters.mf_decoded.fetch_add(1, Ordering::Relaxed);
                    self.frames.push_back(GpuPicture::from_mf(frame));
                }
                Ok(None) => self.eos = true,
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    if self.failures <= 8 || self.failures % 120 == 0 {
                        crate::debug_log(&format!("mf decode: {error} read_us={read_us}"));
                    }
                    if self.failures > 20 {
                        self.failed = Some(error);
                    }
                    break;
                }
            }
        }
        if first && self.frames.back().map_or(false, |frame| frame.pts_ms >= video_ms) {
            self.primed = true;
        }
        // Drop everything behind the picture that will be shown: a seek's
        // keyframe and any frames the decoder produced before the target.
        let keep = (self.frame_ms * 1.5) as u64 + 20;
        while self.frames.len() > 2 && self.frames[1].pts_ms + keep < video_ms {
            self.frames.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            counters.mf_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The pair that brackets `video_ms`, and where in it the clock stands.
    fn choose(&mut self, video_ms: u64) -> Option<(GpuPicture, GpuPicture, f32)> {
        if self.frames.is_empty() {
            return None;
        }
        let index = self.frames.iter().rposition(|frame| frame.pts_ms <= video_ms);
        let Some(index) = index else {
            // The first picture is still after the clock: waiting is correct (a
            // seek's keyframe lands before the target, never after it).
            return None;
        };
        let a = &self.frames[index];
        let b = self.frames.get(index + 1).unwrap_or(a);
        let span = b.pts_ms.saturating_sub(a.pts_ms).max(1);
        let phase = ((video_ms.saturating_sub(a.pts_ms)) as f32 / span as f32).clamp(0.0, 0.999);
        Some((a.clone_picture(), b.clone_picture(), phase))
    }

    /// Release what the queue no longer needs (surfaces return to the pool).
    fn retire(&mut self, used_pts: u64) {
        while self.frames.len() > 2 {
            let Some(front) = self.frames.front() else { break };
            if front.pts_ms < used_pts {
                self.frames.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Drop for VideoSink {
    fn drop(&mut self) {
        self.close();
    }
}

/// The composited picture's geometry for one player window, in **physical**
/// pixels relative to the window's client area.
///
/// DirectComposition works in client-area pixels: the UI layer already relies
/// on that (its swapchain is built at `GetClientRect`'s physical size and is
/// displayed 1:1 at any DPI). The video visual must therefore be placed and
/// clipped in the same space, and the swapchain (one pixel per frame pixel)
/// scaled by a 2D transform. Everything here is physical px; the caller hands
/// the band in logical px and its scale factor ([`Self::fit`] converts once,
/// exactly once).
///
/// `scale_x`/`scale_y` are the DComp transform factors: "target width / frame
/// width" in physical px. 适配 and 填充 are uniform; 拉伸 is allowed to distort.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoGeometry {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) scale_x: f32,
    pub(crate) scale_y: f32,
    /// The band in physical px. The picture is only ever visible inside this
    /// rect: 填充 draws wider/taller and is cropped by it.
    pub(crate) clip: (f32, f32, f32, f32),
}

impl VideoGeometry {
    /// The slot the video visual sits in for a band and a fit mode.
    ///
    /// `band` is `(x, y, w, h)` logical (Slint's client-relative layout),
    /// `scale` the window's scale factor, `frame` the decoded picture's pixel
    /// size, `fit` 0 = 适配, 1 = 填充, 2 = 拉伸.
    pub(crate) fn fit(
        band: (f32, f32, f32, f32),
        scale: f32,
        frame: (u32, u32),
        fit: u8,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.05 {
            scale
        } else {
            1.0
        };
        let (bx, by, bw, bh) = band;
        let frame_w = frame.0.max(1) as f32;
        let frame_h = frame.1.max(1) as f32;
        // The one place logical -> physical happens. The offsets and the clip
        // live in this same space; nothing below divides by `scale` again.
        let band_px = (bx * scale, by * scale, bw * scale, bh * scale);
        let (band_w, band_h) = (band_px.2.max(1.0), band_px.3.max(1.0));
        let (scale_x, scale_y) = match fit {
            // 填充: cover the band and crop the overflow (clipped below).
            1 => {
                let factor = (band_w / frame_w).max(band_h / frame_h);
                (factor, factor)
            }
            // 拉伸: fill the band, distort.
            2 => (band_w / frame_w, band_h / frame_h),
            // 适配: contain, letterbox.
            _ => {
                let factor = (band_w / frame_w).min(band_h / frame_h);
                (factor, factor)
            }
        };
        let drawn_w = frame_w * scale_x;
        let drawn_h = frame_h * scale_y;
        Self {
            x: band_px.0 + (band_w - drawn_w) / 2.0,
            y: band_px.1 + (band_h - drawn_h) / 2.0,
            width: drawn_w,
            height: drawn_h,
            scale_x,
            scale_y,
            clip: band_px,
        }
    }

    /// The part of the picture that can actually reach the screen (the drawn
    /// rect intersected with the clip).
    pub(crate) fn visible(&self) -> (f32, f32, f32, f32) {
        let left = self.x.max(self.clip.0);
        let top = self.y.max(self.clip.1);
        let right = (self.x + self.width).min(self.clip.0 + self.clip.2);
        let bottom = (self.y + self.height).min(self.clip.1 + self.clip.3);
        (left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }

    /// How far the visible picture extends outside a `client` rect (physical
    /// px), summed over the four edges. Zero is the invariant this layer owes
    /// the window: nothing may paint past the client area.
    pub(crate) fn overflow(&self, client: (u32, u32)) -> f32 {
        let (x, y, w, h) = self.visible();
        let cw = client.0 as f32;
        let ch = client.1 as f32;
        let left = (-x).max(0.0);
        let top = (-y).max(0.0);
        let right = (x + w - cw).max(0.0);
        let bottom = (y + h - ch).max(0.0);
        left + top + right + bottom
    }
}

/// Clamp a logical band to the window's client area.
///
/// Slint publishes the band from its own layout, which is one event behind a
/// resize: `WM_SIZE` updates the client rect before the next tick re-lays the
/// band out. Clamping here (a) keeps a stale band from ever placing the picture
/// outside the window and (b) turns a not-yet-laid-out (0×0) band into the full
/// client area instead of an empty visual.
pub(crate) fn clamp_band(
    band: (f32, f32, f32, f32),
    scale: f32,
    client: (u32, u32),
) -> (f32, f32, f32, f32) {
    let scale = if scale.is_finite() && scale > 0.05 {
        scale
    } else {
        1.0
    };
    let cw = client.0 as f32 / scale;
    let ch = client.1 as f32 / scale;
    if cw <= 1.0 || ch <= 1.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let x = band.0.clamp(0.0, cw);
    let y = band.1.clamp(0.0, ch);
    let w = band.2.clamp(0.0, cw - x);
    let h = band.3.clamp(0.0, ch - y);
    if w < 2.0 || h < 2.0 {
        return (0.0, 0.0, cw, ch);
    }
    (x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_letterboxes_and_centres() {
        // 1000x1000 band, 16:9 frame, 适配.
        let geometry = VideoGeometry::fit((0.0, 0.0, 1000.0, 1000.0), 1.0, (960, 540), 0);
        assert!((geometry.width - 1000.0).abs() < 0.01);
        assert!((geometry.height - 562.5).abs() < 0.01);
        assert!(geometry.y > 200.0 && geometry.y < 220.0);
        assert_eq!(geometry.clip, (0.0, 0.0, 1000.0, 1000.0));
        assert_eq!(geometry.overflow((1000, 1000)), 0.0);
    }

    #[test]
    fn cover_overflows_and_is_clipped_to_the_band() {
        // A tall band over a 16:9 frame: 填充 must cover the height and crop
        // the sides, and the clip keeps it inside the band.
        let geometry = VideoGeometry::fit((0.0, 0.0, 1000.0, 1000.0), 1.0, (960, 540), 1);
        assert!((geometry.height - 1000.0).abs() < 0.01);
        assert!(geometry.width > 1700.0);
        assert!(geometry.x < 0.0);
        assert_eq!(geometry.clip, (0.0, 0.0, 1000.0, 1000.0));
        // The drawn rect is wider than the band, the *visible* one is the band.
        let (vx, vy, vw, vh) = geometry.visible();
        assert_eq!((vx, vy, vw, vh), (0.0, 0.0, 1000.0, 1000.0));
        assert_eq!(geometry.overflow((1000, 1000)), 0.0);
    }

    #[test]
    fn stretch_fills_the_band() {
        let geometry = VideoGeometry::fit((10.0, 20.0, 800.0, 300.0), 1.0, (960, 540), 2);
        assert_eq!((geometry.x, geometry.y), (10.0, 20.0));
        assert_eq!((geometry.width, geometry.height), (800.0, 300.0));
    }

    /// The three acceptance sizes, 16:9 but also 4:3 and 21:9, with the band
    /// narrowed by the right panel: the picture must stay inside the client on
    /// every axis. This is the f195 regression for "video larger than window".
    #[test]
    fn never_overflows_the_client_at_any_band_or_aspect() {
        let cases = [
            // (client, band, frame, fit)
            ((960, 540), (0.0, 0.0, 960.0, 540.0), (960, 540), 0u8),
            ((1280, 800), (0.0, 0.0, 1280.0, 800.0), (1280, 720), 0),
            ((1920, 1080), (0.0, 0.0, 1920.0, 1080.0), (1920, 1080), 0),
            // Right panel open: the band is narrower than the window.
            ((1280, 800), (0.0, 0.0, 920.0, 800.0), (920, 518), 0),
            // 4:3 source in a 16:9 band, and 16:9 source in a 4:3 band.
            ((1280, 800), (0.0, 0.0, 1280.0, 800.0), (960, 720), 0),
            ((1280, 800), (0.0, 0.0, 800.0, 800.0), (960, 540), 0),
            // 21:9 source.
            ((1280, 800), (0.0, 0.0, 1280.0, 800.0), (1280, 548), 0),
            // Cover and stretch must also stay inside.
            ((1280, 800), (0.0, 0.0, 1280.0, 800.0), (960, 720), 1),
            ((1280, 800), (0.0, 0.0, 1280.0, 800.0), (960, 720), 2),
        ];
        for (client, band, frame, fit) in cases {
            let geometry = VideoGeometry::fit(band, 1.0, frame, fit);
            assert_eq!(
                geometry.overflow(client),
                0.0,
                "client={client:?} band={band:?} frame={frame:?} fit={fit} geometry={geometry:?}"
            );
            let (x, y, w, h) = geometry.visible();
            assert!(x >= -0.01 && y >= -0.01);
            assert!(x + w <= client.0 as f32 + 0.01 && y + h <= client.1 as f32 + 0.01);
        }
    }

    /// DPI must be applied exactly once: the band doubles, the picture doubles,
    /// and the transform factor (which maps frame px to target px) stays in
    /// physical space.
    #[test]
    fn dpi_scales_once() {
        let at_100 = VideoGeometry::fit((0.0, 0.0, 800.0, 600.0), 1.0, (960, 540), 0);
        let at_150 = VideoGeometry::fit((0.0, 0.0, 800.0, 600.0), 1.5, (960, 540), 0);
        assert!((at_150.width - at_100.width * 1.5).abs() < 0.01);
        assert!((at_150.height - at_100.height * 1.5).abs() < 0.01);
        assert!((at_150.x - at_100.x * 1.5).abs() < 0.01);
        assert!((at_150.y - at_100.y * 1.5).abs() < 0.01);
        // 800x600 logical at 150% is 1200x900 px; contain 16:9 -> 1200x675.
        assert!((at_150.width - 1200.0).abs() < 0.01);
        assert!((at_150.height - 675.0).abs() < 0.01);
        assert_eq!(at_150.overflow((1200, 900)), 0.0);
    }

    #[test]
    fn clamp_band_keeps_a_stale_band_inside_the_client() {
        // 1280x800 window, a band from before the right panel opened.
        assert_eq!(
            clamp_band((0.0, 0.0, 1280.0, 800.0), 1.0, (920, 800)),
            (0.0, 0.0, 920.0, 800.0)
        );
        // A 0x0 band (layout not ready) becomes the whole client.
        assert_eq!(
            clamp_band((0.0, 0.0, 0.0, 0.0), 1.0, (960, 540)),
            (0.0, 0.0, 960.0, 540.0)
        );
        // Offset bands are clamped on both axes.
        assert_eq!(
            clamp_band((100.0, 50.0, 1000.0, 1000.0), 1.0, (960, 540)),
            (100.0, 50.0, 860.0, 490.0)
        );
        // At 150% the client is 640x360 logical.
        assert_eq!(
            clamp_band((0.0, 0.0, 1280.0, 800.0), 1.5, (960, 540)),
            (0.0, 0.0, 640.0, 360.0)
        );
    }
}
