//! f196: 弹幕 as its own composition visual, over the picture only.
//!
//! # Why a third layer
//!
//! Before this module the comments were part of the Slint UI layer, and the
//! motion lane rewrites their x every display frame. That made the *whole*
//! window dirty 120 times a second, so the software-rendered UI layer - chrome,
//! panels, thumbnails, everything - was re-tessellated at the danmaku's rate.
//! f195 measured 45-56 UI updates/s on an 800x800 window; the expensive half of
//! each one was the band, not the chrome.
//!
//! Here the comments get their own swapchain, exactly as large as the picture
//! rect (`VideoGeometry::visible`), presented above the video visual and below
//! the UI visual. The UI layer then repaints only when it has something of its
//! own to say, and the danmaku layer paints at the motion lane's rate.
//!
//! # Why a Slint component and not a text rasteriser
//!
//! The comments must look identical to the winit path (same font, same shaping,
//! same bold/colour rules), and `DanmakuOverlay` in `ui/app.slint` is the same
//! element tree the in-band copy uses. It is instantiated against a
//! [`MinimalSoftwareWindow`] - a `WindowAdapter` that owns a
//! `SoftwareRenderer` and no OS window ([`super::platform::with_buffer_scope`]
//! hands it to the component) - so its renderer writes into a plain pixel
//! buffer that is then staged into the third layer.
//!
//! # f200: why this stays on the UI thread
//!
//! The obvious next step - give this layer a thread of its own, like the video
//! layer has had since f194 - **cannot be done with Slint 1.18**. A component
//! can only be instantiated in the thread that installed the platform:
//! `i_slint_core::context::with_global_context` refuses a second thread outright
//! (`EVENTLOOP_PROXY` is a process-wide `OnceLock`), and the failure is exact:
//!
//! ```text
//! dcomp danmaku component failed: The Slint platform was initialized in another thread
//! ```
//!
//! Measured on this tree, not assumed. The `MinimalSoftwareWindow`, its
//! `SoftwareRenderer` and the component's `Rc` item tree are all `!Send`, so
//! only the present half could have moved - and the present half is ~1 ms of a
//! ~7 ms paint. What actually limits this layer at 1280x720 is therefore the
//! software rasterisation of the moving text, on the thread that also draws the
//! chrome; the numbers are in [`DanmakuStats`].
//!
//! # Geometry
//!
//! The component's own size *is* the picture rect in physical pixels. The
//! engine lays comments out in a fixed 960x540 space (`player.rs::VIDEO_W/H`),
//! and the component maps that space onto its own size, so a comment can never
//! reach the letterbox however the window, the panel or the DPI changes. The
//! visual is positioned at the picture rect's top-left, and its content is a
//! swapchain of exactly its size, so DWM clips it there by construction.
//!
//! # f207: the strips path
//!
//! The software renderer re-rasters the whole layer on every tick because the
//! comments move: 10-32 ms at 1080p, which caps the layer at 37-59 presents a
//! second (`DanmakuStats::render_ms` is that measurement). [`super::strips`]
//! replaces the per-frame render with a cached per-comment alpha mask and a
//! blit: the text is rasterised once, the frame is one clear plus one draw per
//! atlas page, and the Slint component below becomes the fallback.
//!
//! The fallback is real, not decorative: the strips path is the default since
//! f210 (`BILI_DANMAKU_STRIPS=0` puts the component back), and any failure in
//! it (font missing, shader compile, atlas create) is logged once and switches
//! this layer back to the component for the rest of the window's life - a
//! comment layer that can be slow is better than a black picture.
//!
//! The component below is still the path to measure when a 1080p picture looks
//! wrong: f209 measured it going from ~8 ms to ~180 ms per frame at t=19 s with
//! the same rows and a *smaller* damage rect (`docs/f210_1080p_60s.log` has the
//! f210 trace of the same seconds). Whatever the internal cause of that
//! transition, it is inside the software text path, and the strips path does
//! not have it: 0.14-0.45 ms per frame at 1080p over 60 s.


use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::{ComponentHandle, Model, ModelRc};
use windows::core::Interface;
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;

use super::gpu::{self, Composition, Gpu, Layer};
use super::platform;
use super::strips;

/// Whether the separate danmaku layer is armed at all.
///
/// **Off by default, and that is a finding, not caution.** The layer works at
/// the sizes the f196 acceptance run uses (800x800 window / 800x450 picture):
/// the buffer holds the comments, the visual sits above the video and the
/// window shows them clipped to the picture. At a 1920x1080 picture the very
/// same code path renders the exact same buffer (`BILI_DANMAKU_DUMP` proves it)
/// and presents it, yet the window shows nothing - at any z-order, with or
/// without the frame-latency wait. Until that is understood, routing the
/// comments here would *lose* them on a full-window picture, which is worse
/// than the in-band copy this replaces. `BILI_DCOMP_DANMAKU=1` arms it for
/// further work; the shipped behaviour is the in-band layer, which is now
/// clipped to the picture rect (`ui/app.slint`).
pub(crate) fn layer_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BILI_DCOMP_DANMAKU")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
    })
}

/// Cross-thread view of the layer's presentation counters (the UI thread owns
/// the layer itself). Same shape as the UI/video counters in `player_window`.
static DM_HWND: AtomicIsize = AtomicIsize::new(0);
static DM_PAINTS: AtomicU64 = AtomicU64::new(0);
/// Centi-milliseconds, like `UI_PAINT_MS`.
static DM_PAINT_MS: AtomicU64 = AtomicU64::new(0);
static DM_RENDER_MS: AtomicU64 = AtomicU64::new(0);
static DM_STAGE_MS: AtomicU64 = AtomicU64::new(0);
static DM_ROWS: AtomicU64 = AtomicU64::new(0);
/// f207: the strips path's own numbers - compose p50/p95 in µs, quads drawn,
/// cached masks and the atlas footprint.
static DM_COMPOSE_P50_US: AtomicU64 = AtomicU64::new(0);
static DM_COMPOSE_P95_US: AtomicU64 = AtomicU64::new(0);
static DM_QUADS: AtomicU64 = AtomicU64::new(0);
static DM_STRIPS: AtomicU64 = AtomicU64::new(0);
static DM_STRIP_BYTES: AtomicU64 = AtomicU64::new(0);
static DM_STRIP_HITS: AtomicU64 = AtomicU64::new(0);
static DM_STRIP_MISSES: AtomicU64 = AtomicU64::new(0);
/// 1 while the strips path is the one presenting, 0 for the Slint component.
static DM_STRIPS_ON: AtomicU64 = AtomicU64::new(0);
/// Frames composed (the old path's `dm_s` counts these); `DM_PAINTS` above is
/// frames the compositor actually took.
static DM_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static DM_SKIPS: AtomicU64 = AtomicU64::new(0);

/// The picture rect the layer covers: `(x, y, width, height)` in physical px,
/// client-relative.
pub(crate) type Rect = (f32, f32, u32, u32);

/// What the QA hooks read about the layer (any thread).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DanmakuStats {
    /// Frames the compositor took (`Present` returned `S_OK`).
    pub(crate) paints: u64,
    pub(crate) paint_ms: f32,
    /// f195-f200 path: time in the software renderer. f207 path: the same slot
    /// holds the compose time (build the quads + draw + present).
    pub(crate) render_ms: f32,
    /// Upload/present time. Zero in the strips path: nothing is staged.
    pub(crate) stage_ms: f32,
    pub(crate) rows: u64,
    /// f207: compose cost percentiles in µs over the last 256 frames.
    pub(crate) compose_p50_us: u32,
    pub(crate) compose_p95_us: u32,
    /// Quads drawn in the last composed frame.
    pub(crate) quads: u32,
    /// Cached comment masks and the atlas bytes they live in.
    pub(crate) strips: u32,
    pub(crate) strip_bytes: u64,
    pub(crate) strip_hits: u64,
    pub(crate) strip_misses: u64,
    /// Whether the strips path is the one presenting (false = the Slint
    /// component, i.e. the fallback or `BILI_DANMAKU_STRIPS=0`).
    pub(crate) strips_on: bool,
    /// Frames composed, whether or not the compositor took them.
    pub(crate) attempts: u64,
    /// Composed frames the compositor refused (a `DO_NOT_WAIT` present that
    /// found DWM busy); they are retried on the next tick.
    pub(crate) skips: u64,
}

/// The danmaku layer of one player window.
///
/// The model exists from construction (the motion lane publishes into it), the
/// Slint component and the swapchain are built on the first paint, once the
/// player component that owns this window is fully constructed.
pub(crate) struct DanmakuLayer {
    model: Rc<slint::VecModel<crate::DanmakuItem>>,
    surface: RefCell<Option<Surface>>,
    /// 弹幕开关 (`player.danmaku_on`). Off detaches the visual's content, so a
    /// comment left in the model is not drawn (the in-band layer hides itself
    /// through the same flag).
    visible: std::cell::Cell<bool>,
    /// The last rect the visual was placed at, so a still window does not
    /// re-commit the composition every tick.
    placed: RefCell<Option<Rect>>,
}

struct Surface {
    window: Rc<MinimalSoftwareWindow>,
    ui: crate::DanmakuOverlay,
    layer: Layer,
    visual: IDCompositionVisual,
    /// The swapchain as visual content, kept so the visual can be detached
    /// (弹幕 off) and re-attached without rebuilding anything.
    content: windows::core::IUnknown,
    attached: bool,
    buffer: Vec<PremultipliedRgbaColor>,
    size: (u32, u32),
    /// The next render must cover the whole buffer (first frame, resize).
    full: bool,
    paints: u64,
    /// Composed frames and the ones the compositor refused (see `DM_SKIPS`).
    attempts: u64,
    skips: u64,
    paint_ms: f32,
    render_ms: f32,
    stage_ms: f32,
    /// f207: the strip cache + its GPU blit. `None` while the Slint component
    /// is drawing, either because `BILI_DANMAKU_STRIPS=0` or because the strips
    /// path failed (then `strips_failed` keeps it from retrying every tick).
    strips: Option<strips::Compositor>,
    strips_failed: bool,
    /// The model snapshot the strips path draws, reused every frame.
    rows: Vec<crate::DanmakuItem>,
    /// Paints since the last percentile refresh (the sort is not free).
    since_stats: u32,
    /// f210: characters in the model at the last painted frame, and the damage
    /// rectangle that frame covered. `render_ms / chars` is what separates a
    /// per-glyph collapse from a per-pixel one.
    trace_chars: usize,
    /// Distinct characters in the same snapshot (see `f210::DM_UNIQ`).
    trace_uniq: usize,
    /// Whether the last frame of this layer was already reported as slow (the
    /// 1080p software-text collapse f209 measured at t=19 s).
    trace_slow: bool,
    /// 1 Hz gate for the trace sample.
    trace_at: std::time::Instant,
}

impl Default for DanmakuLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl DanmakuLayer {
    pub(crate) fn new() -> Self {
        Self {
            model: Rc::new(slint::VecModel::from(Vec::<crate::DanmakuItem>::new())),
            surface: RefCell::new(None),
            visible: std::cell::Cell::new(true),
            placed: RefCell::new(None),
        }
    }

    /// The model the motion lane must publish into instead of the player
    /// component's own `danmaku_items`.
    pub(crate) fn model(&self) -> Rc<slint::VecModel<crate::DanmakuItem>> {
        self.model.clone()
    }

    pub(crate) fn register(hwnd: isize) {
        DM_HWND.store(hwnd, Ordering::Relaxed);
    }

    pub(crate) fn unregister(hwnd: isize) {
        if DM_HWND.load(Ordering::Relaxed) == hwnd {
            DM_HWND.store(0, Ordering::Relaxed);
            DM_PAINTS.store(0, Ordering::Relaxed);
            DM_PAINT_MS.store(0, Ordering::Relaxed);
            DM_RENDER_MS.store(0, Ordering::Relaxed);
            DM_STAGE_MS.store(0, Ordering::Relaxed);
            DM_ROWS.store(0, Ordering::Relaxed);
            DM_COMPOSE_P50_US.store(0, Ordering::Relaxed);
            DM_COMPOSE_P95_US.store(0, Ordering::Relaxed);
            DM_QUADS.store(0, Ordering::Relaxed);
            DM_STRIPS.store(0, Ordering::Relaxed);
            DM_STRIP_BYTES.store(0, Ordering::Relaxed);
            DM_STRIP_HITS.store(0, Ordering::Relaxed);
            DM_STRIP_MISSES.store(0, Ordering::Relaxed);
            DM_ATTEMPTS.store(0, Ordering::Relaxed);
            DM_SKIPS.store(0, Ordering::Relaxed);
            DM_STRIPS_ON.store(0, Ordering::Relaxed);
        }
    }

    /// 弹幕 off: detach the visual's content instead of stopping the layer, so
    /// the comments left in the model are not drawn and nothing repaints until
    /// the switch goes back on. Comparisons are cheap and idempotent, so the
    /// 33 ms tick can call this unconditionally.
    pub(crate) fn set_visible(&self, on: bool, composition: Option<&Composition>) {
        if self.visible.get() == on {
            return;
        }
        self.visible.set(on);
        let mut slot = self.surface.borrow_mut();
        let Some(surface) = slot.as_mut() else {
            return;
        };
        if surface.attached == on {
            return;
        }
        let Some(composition) = composition else {
            return;
        };
        unsafe {
            let result = if on {
                surface.visual.SetContent(&surface.content)
            } else {
                surface.visual.SetContent(None::<&windows::core::IUnknown>)
            };
            if result.is_ok() {
                surface.attached = on;
                let _ = composition.device.Commit();
                if on {
                    // Re-attaching shows whatever the swapchain held when the
                    // switch went off; the model may have been republished since
                    // (a seek), so ask for one fresh frame from whichever path
                    // is drawing.
                    if let Some(compositor) = surface.strips.as_mut() {
                        compositor.invalidate();
                    }
                    surface.ui.window().request_redraw();
                }
            }
        }
    }

    /// `(paints, last_paint_ms)` of the danmaku layer for `hwnd` (any thread).
    pub(crate) fn counters(hwnd: isize) -> Option<(u64, f32)> {
        if DM_HWND.load(Ordering::Relaxed) != hwnd {
            return None;
        }
        Some((
            DM_PAINTS.load(Ordering::Relaxed),
            DM_PAINT_MS.load(Ordering::Relaxed) as f32 / 100.0,
        ))
    }

    /// The full counter set (any thread). f200: the render / stage split is what
    /// says whether a slow paint is rasterisation or upload; f207 adds the
    /// strip cache and the compose percentiles.
    pub(crate) fn stats(hwnd: isize) -> Option<DanmakuStats> {
        if DM_HWND.load(Ordering::Relaxed) != hwnd {
            return None;
        }
        Some(DanmakuStats {
            paints: DM_PAINTS.load(Ordering::Relaxed),
            paint_ms: DM_PAINT_MS.load(Ordering::Relaxed) as f32 / 100.0,
            render_ms: DM_RENDER_MS.load(Ordering::Relaxed) as f32 / 100.0,
            stage_ms: DM_STAGE_MS.load(Ordering::Relaxed) as f32 / 100.0,
            rows: DM_ROWS.load(Ordering::Relaxed),
            compose_p50_us: DM_COMPOSE_P50_US.load(Ordering::Relaxed) as u32,
            compose_p95_us: DM_COMPOSE_P95_US.load(Ordering::Relaxed) as u32,
            quads: DM_QUADS.load(Ordering::Relaxed) as u32,
            strips: DM_STRIPS.load(Ordering::Relaxed) as u32,
            strip_bytes: DM_STRIP_BYTES.load(Ordering::Relaxed),
            strip_hits: DM_STRIP_HITS.load(Ordering::Relaxed),
            strip_misses: DM_STRIP_MISSES.load(Ordering::Relaxed),
            strips_on: DM_STRIPS_ON.load(Ordering::Relaxed) != 0,
            attempts: DM_ATTEMPTS.load(Ordering::Relaxed),
            skips: DM_SKIPS.load(Ordering::Relaxed),
        })
    }

    /// Draw and present the comments if anything moved, and place the visual at
    /// `rect`.
    ///
    /// Returns whether a frame reached the swapchain. Cheap when nothing
    /// changed: the component's own damage tracking decides, so a paused or
    /// comment-free picture costs one branch.
    pub(crate) fn present(
        &self,
        gpu: &Gpu,
        composition: &Composition,
        ui_visual: Option<&IDCompositionVisual>,
        rect: Rect,
    ) -> bool {
        if rect.2 == 0 || rect.3 == 0 {
            return false;
        }
        let mut slot = self.surface.borrow_mut();
        if let Err(error) = self.ensure(&mut slot, gpu, composition, ui_visual, rect) {
            crate::debug_log(&format!("dcomp danmaku layer failed: {error}"));
            return false;
        }
        let Some(surface) = slot.as_mut() else {
            return false;
        };
        if !self.visible.get() {
            return false;
        }
        // Place the visual: an integer offset (the rect is already physical px)
        // keeps the text on the same pixel grid as the picture underneath.
        let placed = (rect.0.round(), rect.1.round(), rect.2, rect.3);
        let moved = self.placed.borrow().map(|last| {
            (last.0.round(), last.1.round(), last.2, last.3) != placed
        }) != Some(false);
        if moved {
            unsafe {
                let _ = surface.visual.SetOffsetX2(placed.0);
                let _ = surface.visual.SetOffsetY2(placed.1);
                let _ = composition.device.Commit();
            }
            *self.placed.borrow_mut() = Some(rect);
        }

        // f210: one 1 Hz sample of the model, so a paint's cost can be
        // normalised by how much text it had to move. Counting characters is
        // O(rows) per *second*, not per frame.
        let trace_due = super::f210::enabled() && surface.trace_at.elapsed().as_millis() >= 1000;
        if trace_due {
            surface.trace_at = std::time::Instant::now();
            let mut chars = 0usize;
            let mut uniq: std::collections::HashSet<char> = std::collections::HashSet::new();
            for item in self.model.iter() {
                for ch in item.text.chars() {
                    chars += 1;
                    uniq.insert(ch);
                }
            }
            surface.trace_chars = chars;
            surface.trace_uniq = uniq.len();
        }

        // f207: the strips path. The text is already cached as alpha masks, so
        // a frame here is a clear plus one draw per atlas page - no shaping and
        // no CPU copy. Anything that goes wrong is logged once and this layer
        // falls back to the Slint component below for the rest of its life.
        if strips::enabled() && !surface.strips_failed {
            if surface.strips.is_none() {
                match strips::Compositor::new(&gpu.device, &gpu.context) {
                    Ok(compositor) => surface.strips = Some(compositor),
                    Err(error) => {
                        surface.strips_failed = true;
                        crate::debug_log(&format!(
                            "danmaku strips unavailable: {error} — using the Slint danmaku layer"
                        ));
                    }
                }
            }
            if surface.strips.is_some() {
                surface.rows.clear();
                for item in self.model.iter() {
                    surface.rows.push(item);
                }
                let started = std::time::Instant::now();
                let composed = surface
                    .strips
                    .as_mut()
                    .expect("strips checked above")
                    .compose(&surface.rows, surface.size, &surface.layer);
                match composed {
                    Ok(Some(frame)) => {
                        // The strips compositor waits for the compositor, draws
                        // and presents in one place, so the layer only has to
                        // count what came back.
                        let (compose_us, presented) = (frame.us, frame.presented);
                        let paint_ms = started.elapsed().as_secs_f32() * 1000.0;
                        let compose_ms = compose_us as f32 / 1000.0;
                        surface.paint_ms = paint_ms;
                        surface.render_ms = compose_ms;
                        surface.stage_ms = (paint_ms - compose_ms).max(0.0);
                        surface.attempts += 1;
                        if presented {
                            surface.paints += 1;
                        } else {
                            // A flip-model present that DWM would not take is a
                            // frame that never reached the screen. Ask for the
                            // same frame again next tick (the model has not
                            // moved, so this is the only reason to redraw) -
                            // otherwise the comment layer would quietly run at
                            // the rate of its *failures*.
                            surface.skips += 1;
                            if let Some(compositor) = surface.strips.as_mut() {
                                compositor.invalidate();
                            }
                        }
                        let (stats, percentiles) = {
                            let compositor =
                                surface.strips.as_ref().expect("strips checked above");
                            (compositor.stats(), compositor.percentiles())
                        };
                        surface.since_stats += 1;
                        DM_PAINTS.store(surface.paints, Ordering::Relaxed);
                        DM_ATTEMPTS.store(surface.attempts, Ordering::Relaxed);
                        DM_SKIPS.store(surface.skips, Ordering::Relaxed);
                        DM_PAINT_MS.store((surface.paint_ms * 100.0) as u64, Ordering::Relaxed);
                        DM_RENDER_MS.store((compose_ms * 100.0) as u64, Ordering::Relaxed);
                        DM_STAGE_MS.store((surface.stage_ms * 100.0) as u64, Ordering::Relaxed);
                        DM_ROWS.store(self.model.row_count() as u64, Ordering::Relaxed);
                        DM_QUADS.store(stats.quads as u64, Ordering::Relaxed);
                        DM_STRIPS.store(stats.strips as u64, Ordering::Relaxed);
                        DM_STRIP_BYTES.store(stats.bytes, Ordering::Relaxed);
                        DM_STRIP_HITS.store(stats.hits, Ordering::Relaxed);
                        DM_STRIP_MISSES.store(stats.misses, Ordering::Relaxed);
                        DM_STRIPS_ON.store(1, Ordering::Relaxed);
                        if trace_due {
                            let size = surface.size;
                            self.trace_frame(surface, size, true, true);
                        }
                        // The percentile sort is ~256 u32s; at 1 in 16 paints it
                        // is invisible and the QA reader never sees a stale
                        // number by more than a quarter of a second.
                        if surface.since_stats >= 16 {
                            surface.since_stats = 0;
                            DM_COMPOSE_P50_US.store(percentiles.0 as u64, Ordering::Relaxed);
                            DM_COMPOSE_P95_US.store(percentiles.1 as u64, Ordering::Relaxed);
                        }
                    }
                    Ok(None) => return false,
                    Err(error) => {
                        // A device-level failure: keep the window's comments by
                        // going back to the component, once.
                        surface.strips = None;
                        surface.strips_failed = true;
                        DM_STRIPS_ON.store(0, Ordering::Relaxed);
                        // The component's damage tracking has no history for
                        // this buffer: force a full repaint so the comments it
                        // draws are complete.
                        surface.full = true;
                        crate::debug_log(&format!(
                            "danmaku strips failed: {error} — falling back to the Slint \
                             danmaku layer"
                        ));
                    }
                }
                if surface.strips.is_some() {
                    return true;
                }
            }
        }

        let width = surface.size.0 as usize;
        let height = surface.size.1 as usize;
        let started = std::time::Instant::now();
        let mut buffer = std::mem::take(&mut surface.buffer);
        if buffer.len() != width * height {
            buffer.clear();
            buffer.resize(width * height, PremultipliedRgbaColor::default());
            surface.full = true;
        }
        let mut damage = (0usize, 0usize, width, height);
        let full = surface.full;
        let mut drawn = false;
        surface.window.draw_if_needed(|renderer| {
            if full {
                renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            }
            let region = renderer.render(&mut buffer, width);
            if full {
                renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
            }
            drawn = true;
            if !full {
                let origin = region.bounding_box_origin();
                let size = region.bounding_box_size();
                let x = (origin.x.max(0) as usize).min(width - 1);
                let y = (origin.y.max(0) as usize).min(height - 1);
                let w = (size.width as usize).min(width - x);
                let h = (size.height as usize).min(height - y);
                if w == 0 || h == 0 {
                    drawn = false;
                }
                damage = (x, y, w, h);
            }
        });
        surface.buffer = buffer;
        if !drawn {
            return false;
        }
        surface.full = false;
        let render_ms = started.elapsed().as_secs_f32() * 1000.0;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                surface.buffer.as_ptr() as *const u8,
                surface.buffer.len() * 4,
            )
        };
        // Developer hook (`BILI_DANMAKU_DUMP=<path>`): the layer's own buffer,
        // exactly what the swapchain is fed. QA only.
        if let Ok(path) = std::env::var("BILI_DANMAKU_DUMP") {
            if surface.paints % 40 == 20 {
                let mut rgba: Vec<u8> = Vec::with_capacity(surface.buffer.len() * 4);
                for pixel in &surface.buffer {
                    rgba.extend_from_slice(&[
                        pixel.blue,
                        pixel.green,
                        pixel.red,
                        pixel.alpha,
                    ]);
                }
                if let Some(image) =
                    image::RgbaImage::from_raw(width as u32, height as u32, rgba)
                {
                    let _ = image.save(&path);
                    crate::debug_log(&format!(
                        "dcomp danmaku dump n={} {path} rows={}",
                        surface.paints,
                        self.model.row_count()
                    ));
                }
            }
        }
        let staged = if full {
            surface.layer.stage(bytes, true).map(|_| ())
        } else {
            surface
                .layer
                .stage_rect(bytes, width, damage.0 as u32, damage.1 as u32, damage.2 as u32, damage.3 as u32)
        };
        if let Err(error) = staged {
            crate::debug_log(&format!("dcomp danmaku stage failed: {error}"));
            return false;
        }
        let stage_ms = started.elapsed().as_secs_f32() * 1000.0 - render_ms;
        // Same rule as the strips path: `dm_s` is frames the compositor took,
        // so the A/B compares two identical measurements. `attempts` keeps the
        // old "how often did we draw" number visible next to it.
        let presented = match surface.layer.pulse() {
            Ok(hr) => hr.0 == 0,
            Err(error) => {
                crate::debug_log(&format!("dcomp danmaku present failed: {error}"));
                false
            }
        };
        surface.paint_ms = started.elapsed().as_secs_f32() * 1000.0;
        surface.render_ms = render_ms;
        surface.stage_ms = stage_ms;
        surface.attempts += 1;
        if presented {
            surface.paints += 1;
        } else {
            surface.skips += 1;
        }
        DM_PAINTS.store(surface.paints, Ordering::Relaxed);
        DM_ATTEMPTS.store(surface.attempts, Ordering::Relaxed);
        DM_SKIPS.store(surface.skips, Ordering::Relaxed);
        DM_PAINT_MS.store((surface.paint_ms * 100.0) as u64, Ordering::Relaxed);
        DM_RENDER_MS.store((render_ms * 100.0) as u64, Ordering::Relaxed);
        DM_STAGE_MS.store((stage_ms * 100.0) as u64, Ordering::Relaxed);
        DM_ROWS.store(self.model.row_count() as u64, Ordering::Relaxed);
        if trace_due {
            let (w, h) = if full {
                surface.size
            } else {
                (damage.2 as u32, damage.3 as u32)
            };
            self.trace_frame(surface, (w, h), full, false);
        }
        let paints = surface.paints;
        if paints <= 5 || paints % 300 == 0 {
            crate::debug_log(&format!(
                "dcomp danmaku paint n={paints} size={}x{} at {:.0},{:.0} full={full} \
                 damage={}x{} at {},{} rows={} ms={:.2} render_ms={render_ms:.2} \
                 stage_ms={stage_ms:.2} present_ms={:.2}",
                width,
                height,
                placed.0,
                placed.1,
                damage.2,
                damage.3,
                damage.0,
                damage.1,
                self.model.row_count(),
                surface.paint_ms,
                surface.paint_ms - render_ms - stage_ms
            ));
        }
        true
    }

    /// f210: hand one frame's shape and cost to the trace, and say once when
    /// the software text path crosses the cliff f209 measured.
    ///
    /// `chars` is the number of characters in the model at that frame, so
    /// `render_ms / chars` answers the only question the t=19 s transition
    /// leaves open: did the frame get bigger (damage rect) or did each glyph
    /// get more expensive (the software renderer's 1 MB glyph cache).
    fn trace_frame(
        &self,
        surface: &mut Surface,
        damage: (u32, u32),
        full: bool,
        strips: bool,
    ) {
        let frame = super::f210::DanmakuFrame {
            rows: if strips {
                surface.rows.len()
            } else {
                self.model.row_count()
            },
            chars: surface.trace_chars,
            uniq: surface.trace_uniq,
            damage,
            full,
            render_ms: surface.render_ms,
            stage_ms: surface.stage_ms,
            strips,
        };
        if !surface.trace_slow && frame.render_ms > 40.0 {
            surface.trace_slow = true;
            let per_char = if frame.chars > 0 {
                frame.render_ms * 1000.0 / frame.chars as f32
            } else {
                0.0
            };
            // The model's own text at the moment of the transition: a new
            // script, an emoji or a font fallback shows up here and nowhere
            // else.
            let sample: Vec<String> = self
                .model
                .iter()
                .take(10)
                .map(|item| item.text.to_string())
                .collect();
            crate::debug_log(&format!(
                "f210 danmaku slow layer render_ms={:.1} damage={}x{} rows={} chars={}                  uniq={} us_per_char={per_char:.1} strips={} - the Slint software text                  path got expensive, not the pixels; sample={:?}",
                frame.render_ms,
                damage.0,
                damage.1,
                frame.rows,
                frame.chars,
                frame.uniq,
                strips,
                sample
            ));
        }
        super::f210::note_danmaku(&frame);
    }

    /// Build the component and its swapchain the first time, then keep the
    /// swapchain at the picture rect's size.
    fn ensure(
        &self,
        slot: &mut Option<Surface>,
        gpu: &Gpu,
        composition: &Composition,
        ui_visual: Option<&IDCompositionVisual>,
        rect: Rect,
    ) -> Result<(), String> {
        let size = (rect.2.max(1), rect.3.max(1));
        match slot {
            Some(surface) => {
                if surface.size == size {
                    return Ok(());
                }
                surface
                    .layer
                    .resize(gpu, size.0, size.1)
                    .map_err(|error| error.to_string())?;
                surface.size = size;
                surface.window.set_size(slint::PhysicalSize::new(size.0, size.1));
                surface.full = true;
                crate::debug_log(&format!(
                    "dcomp danmaku layer resize {}x{}",
                    size.0, size.1
                ));
                Ok(())
            }
            None => {
                // The component asks the platform for a window adapter on its
                // first `window()`: arm that adapter for exactly this
                // constructor, so the component is bound to a buffer window
                // instead of an OS window.
                let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
                let adapter: Rc<dyn slint::platform::WindowAdapter> = window.clone();
                let ui = platform::with_buffer_scope(adapter, || -> Result<_, slint::PlatformError> {
                    let ui = crate::DanmakuOverlay::new()?;
                    // First touch of the window: binds the adapter now, inside
                    // the scope, and makes the tree renderable.
                    ui.show()?;
                    Ok(ui)
                })
                .map_err(|error| error.to_string())?;
                // QA A/B (`BILI_DANMAKU_EMPTY=1`): an empty model leaves the
                // same window, swapchain, stage and present work with no text,
                // so the two halves of the cost can be separated.
                let empty = std::env::var("BILI_DANMAKU_EMPTY").is_ok_and(|value| value == "1");
                if empty {
                    ui.set_items(ModelRc::from(Rc::new(
                        slint::VecModel::<crate::DanmakuItem>::from(
                            Vec::<crate::DanmakuItem>::new(),
                        ),
                    )));
                } else {
                    ui.set_items(ModelRc::from(self.model.clone()));
                }
                window.set_size(slint::PhysicalSize::new(size.0, size.1));
                crate::debug_log(&format!(
                    "dcomp danmaku component scale={:.2} window={}x{}",
                    ui.window().scale_factor(),
                    ui.window().size().width,
                    ui.window().size().height
                ));
                let mut layer = Layer::new(gpu, size.0, size.1, gpu::UI)
                    .map_err(|error| error.to_string())?;
                // f210: this layer shares the UI device, so the trace cannot
                // tell them apart by alpha mode - name it explicitly.
                layer.set_role(super::f210::DANMAKU);
                let content: windows::core::IUnknown =
                    layer.swap.cast().map_err(|error| error.to_string())?;
                let visible = self.visible.get();
                let visual = unsafe {
                    let visual = composition
                        .device
                        .CreateVisual()
                        .map_err(|error| error.to_string())?;
                    if visible {
                        visual
                            .SetContent(&content)
                            .map_err(|error| error.to_string())?;
                    }
                    visual.SetOffsetX2(rect.0.round()).map_err(|e| e.to_string())?;
                    visual.SetOffsetY2(rect.1.round()).map_err(|e| e.to_string())?;
                    // Between the video and the UI: insert *below* the UI visual
                    // when there is one, else on top (nothing to stay under).
                    let on_top = std::env::var("BILI_DANMAKU_ON_TOP").is_ok_and(|v| v == "1");
                    let (above, reference) = match (on_top, ui_visual) {
                        (true, Some(ui)) => (true, Some(ui)),
                        (true, None) => (true, None),
                        (false, Some(ui)) => (false, Some(ui)),
                        (false, None) => (true, None),
                    };
                    composition
                        .root
                        .AddVisual(&visual, above, reference)
                        .map_err(|error| error.to_string())?;
                    composition.device.Commit().map_err(|e| e.to_string())?;
                    visual
                };
                *self.placed.borrow_mut() = Some(rect);
                *slot = Some(Surface {
                    window,
                    ui,
                    layer,
                    visual,
                    content,
                    attached: visible,
                    buffer: Vec::new(),
                    size,
                    full: true,
                    paints: 0,
                    attempts: 0,
                    skips: 0,
                    paint_ms: 0.0,
                    render_ms: 0.0,
                    stage_ms: 0.0,
                    strips: None,
                    strips_failed: false,
                    rows: Vec::new(),
                    since_stats: 0,
                    trace_chars: 0,
                    trace_uniq: 0,
                    trace_slow: false,
                    trace_at: std::time::Instant::now(),
                });
                crate::debug_log(&format!(
                    "dcomp danmaku layer created size={}x{} visual=below_ui",
                    size.0, size.1
                ));
                Ok(())
            }
        }
    }

    /// The layer's visual, once built (used to keep the z-order explicit).
    pub(crate) fn surface_ref(&self) -> Option<IDCompositionVisual> {
        self.surface
            .borrow()
            .as_ref()
            .map(|surface| surface.visual.clone())
    }

    /// Drop the component and its swapchain (window teardown, on the UI
    /// thread, before the composition device goes away).
    pub(crate) fn clear(&self) {
        self.surface.borrow_mut().take();
        self.placed.borrow_mut().take();
    }
}
