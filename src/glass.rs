//! Blurred backdrop for the frosted main window.
//!
//! Windows does not blur what is behind a normal HWND on this machine: the
//! legacy `DwmEnableBlurBehindWindow` path (see `win.rs::setup_glass`) measured
//! as pure transparency (0.18 transmittance = exactly the window alpha;
//! high-frequency detail kept at ~100%), and the modern DWM backdrop paths
//! either composite opaquely or are absent. The blur is therefore computed
//! in-process instead:
//!
//! 1. `capture` grabs the WHOLE VIRTUAL DESKTOP with GDI, after marking the
//!    window itself `WDA_EXCLUDEFROMCAPTURE` for the duration of the blit, so
//!    the grab contains what is *behind* the window rather than the previous
//!    frame of the glass (self-feedback). A probe on this machine showed GDI
//!    capture honours the affinity immediately and that clearing it again
//!    restores normal captures, so the flag is toggled around each blit and
//!    screenshots taken by other tools still see the window.
//! 2. `render` box-downscales the grab (CPU cost stays proportional to the
//!    blurred image, not the window), runs three box-blur passes per axis
//!    (the classic Gaussian approximation - three boxes of radius r have
//!    sigma ~= r), and hands the result to Slint, whose renderer upscales it.
//!
//! The grab covers the desktop, not the window: [`place_for`] pins it to the
//! desktop by expressing the grab's origin minus the window's origin, so a move
//! or resize only changes that placement - four properties, no capture and no
//! blur. That is what keeps a dragged window from showing a snapshot taken one
//! position ago.
//!
//! The whole pipeline - capture, downscale, blur, pixel-buffer build - runs on
//! ONE dedicated worker thread created by [`GlassWorker::spawn`]: a
//! whole-desktop grab and render measured ~93ms on this machine, which must not
//! sit in the UI thread's refresh timer. The UI thread sends a [`GlassRequest`]
//! (the worker drains its channel and serves only the LATEST one) and receives
//! a [`GlassFrame`] back through `slint::invoke_from_event_loop`; every timer
//! tick and every drag event only re-places the last frame with the cheap
//! [`place_for`].
//!
//! `Glass::frame` is the worker's one entry point: it decides whether the
//! cached grab has to be refreshed (`recapture`) or re-blurred (the radius
//! moved), and reports both the frame it produced and the reason a capture
//! failed.
//!
//! # Never flat
//!
//! GDI screen capture does not fail loudly when the desktop is temporarily
//! unviewable - a locked session, a display that went to sleep, a TDR/display
//! mode change or a driver reset make `BitBlt` either fail or "succeed" with an
//! all-black grab for as long as the condition lasts. Delivering such a frame
//! makes the window a flat tint (the UI has no other backdrop), so this module
//! is built so that a bad grab can only ever *delay* a new frame:
//!
//! * A failed or black capture never replaces the cached grab. A pending blur
//!   change is still served from the last good pixels.
//! * The worker keeps the last good [`GlassFrame`] and re-sends it (re-placed to
//!   the window's current position, at most every [`RETAIN_RESEND_MS`]) while
//!   captures keep failing, so a window whose backdrop was cleared (the blur
//!   slider turned off and on, a re-shown window) gets a backdrop back.
//! * Failures are retried with a 400 -> 800ms backoff, driven by the worker
//!   itself (`recv_timeout`), not just by the caller's 400ms cadence, so a
//!   capture recovers within ~800ms of the condition ending even when the UI
//!   stops asking.
//! * After [`REBUILD_AFTER`] consecutive failures the whole capture stack (DC,
//!   DIB, display affinity, cached grab) is rebuilt - the documented cure for a
//!   stale desktop DC - and each further rebuild doubles its wait (5, 10, 20,
//!   40, 80 failures, then 80) so a stuck state is retried about once a minute
//!   instead of hammering GDI.
//! * A wall-clock/monotonic skew of more than [`RESUME_SKEW_MS`] means the
//!   machine slept or the clock jumped: the capture stack is rebuilt and the
//!   next request grabs immediately, so resume-from-sleep heals on its own.
//! * The worker survives a panic in the pipeline (`catch_unwind`): it logs the
//!   panic and restarts with a fresh capture stack instead of leaving the app
//!   with a dead thread and no frames ever again.
//! * A hidden or minimised window is *paused*, not failed: nothing is due, the
//!   retained frame keeps the window frosted, and the first visible request
//!   grabs again.
//!
//! [`GlassWorker::healthy`] / [`GlassWorker::last_frame_age`] expose the state
//! for a future wiring pass; the recovery itself needs no help from the caller.
//!
//! `BILI_GLASS_LOG=1` (plus `BILI_DEBUG_LOG=<file>` for the sink) logs the
//! failure/retain/rebuild/resume decisions. `BILI_GLASS_FAILTEST=<n>` makes the
//! first `n` capture attempts after the first delivered frame fail - every third
//! one as an all-black grab, the rest as a GDI error - which exercises the black
//! detector, retention, backoff and rebuild end to end.
//!
//! Ported from the working reference implementation,
//! `asset-library-desktop/src/glass.rs`; its opt-in `ALD_GLASS_TRACE` timing
//! instrument and its unit tests were left behind on purpose (the crate's test
//! count stays fixed).
//!
//! This crate is Windows-only (see `win.rs`), so there is no stub build.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Blur sigma at slider 100, in physical pixels.
pub const SIGMA_MAX: f32 = 48.0;

/// Fastest self-driven retry after a failed capture: the caller's own cadence,
/// so a transient failure costs at most one skipped frame.
const RETRY_MIN_MS: u64 = 400;
/// Slowest self-driven retry. Deliberately below the 1s recovery budget: a
/// capture blocked by a lock screen, display mode change or resume is picked up
/// again within this window (plus one 400ms tick) once the condition ends.
const RETRY_MAX_MS: u64 = 800;
/// Consecutive capture failures before the capture stack is rebuilt.
const REBUILD_AFTER: u32 = 5;
/// A retained frame is re-sent at most this often while captures fail. The UI
/// keeps the image itself, so this is only a safety net against a cleared
/// backdrop - re-sending every 400ms would push a full image into the event
/// loop for nothing.
const RETAIN_RESEND_MS: u64 = 5_000;
/// Wall-clock vs monotonic skew that means the machine slept (or the clock was
/// stepped): the capture stack is rebuilt and the next grab forced.
const RESUME_SKEW_MS: u64 = 2_000;
/// `GetWindowRect` reports this origin (both axes) while a window is minimised;
/// placing a grab against it would put the image ~32000px off the window and the
/// window would show nothing but its tint.
const ICONIC_ORIGIN: i32 = -32_000;

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `BITMAPINFOHEADER` plus one palette entry - a 32bpp BI_RGB DIB ignores
/// the palette, but GDI requires the struct to carry at least one entry.
#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 3],
}

#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, dc: isize) -> i32;
    fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
    fn SetWindowDisplayAffinity(hwnd: isize, affinity: u32) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    /// Error code of the last failed GDI call, for the capture diagnostics.
    fn GetLastError() -> u32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(dc: isize) -> isize;
    fn DeleteDC(dc: isize) -> i32;
    fn CreateDIBSection(
        dc: isize,
        bmi: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut std::ffi::c_void,
        section: isize,
        offset: u32,
    ) -> isize;
    fn SelectObject(dc: isize, obj: isize) -> isize;
    fn DeleteObject(obj: isize) -> i32;
    fn BitBlt(
        dst: isize,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        src: isize,
        sx: i32,
        sy: i32,
        rop: u32,
    ) -> i32;
}

/// `SRCCOPY`, plus `CAPTUREBLT` so layered windows (many overlays) are part of
/// the grab as well.
const ROP: u32 = 0x00CC_0020 | 0x4000_0000;
const WDA_NONE: u32 = 0x0000_0000;
/// Windows 10 2004+: the window stays visible on screen but is left out of
/// capture APIs - GDI screen capture included, as measured on this machine.
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x0000_0011;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

/// Opt-in detail log: `BILI_GLASS_LOG=1` turns it on, the sink is
/// `BILI_DEBUG_LOG` (through [`crate::debug_log`]). Inert otherwise.
fn trace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("BILI_GLASS_LOG") {
        Ok(value) => {
            let value = value.trim();
            !value.is_empty() && value != "0"
        }
        Err(_) => false,
    })
}

/// One detail line (failures, retention, rebuilds, resume). The per-grab timing
/// line is logged unconditionally through `debug_log` and stays as it was.
fn trace(line: &str) {
    if trace_enabled() {
        crate::debug_log(line);
    }
}

/// Milliseconds, saturating (a `Duration` can exceed `u64` only after ~584M
/// years, but the cast must not be `as` on a `u128` without a bound).
fn ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// Worker health, shared between the worker thread and [`GlassWorker`].
///
/// Atomics only: [`GlassWorker::last_frame_age`] may be called from the UI
/// thread while the worker is between captures.
struct Health {
    /// Start of the worker, the epoch `last_frame_ms` is measured from.
    born: Instant,
    /// Cleared when the worker thread exits (never while it is serving).
    alive: AtomicBool,
    /// Frames produced by a REAL capture (retained re-sends excluded).
    frames: AtomicU64,
    /// Consecutive failed captures; 0 after a successful one.
    failures: AtomicU32,
    /// Capture-stack rebuilds since the worker started.
    rebuilds: AtomicU32,
    /// `frames`-free age marker of the last real frame, ms since `born`;
    /// 0 = never.
    last_frame_ms: AtomicU64,
}

impl Health {
    fn new() -> Health {
        Health {
            born: Instant::now(),
            alive: AtomicBool::new(true),
            frames: AtomicU64::new(0),
            failures: AtomicU32::new(0),
            rebuilds: AtomicU32::new(0),
            last_frame_ms: AtomicU64::new(0),
        }
    }

    /// Record a frame built from a fresh grab.
    fn note_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.last_frame_ms
            .store(ms(self.born.elapsed()), Ordering::Relaxed);
        self.failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed capture and return the new consecutive count.
    fn note_failure(&self) -> u32 {
        self.failures.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Time since the last REAL capture, `None` before the first.
    fn age(&self) -> Option<Duration> {
        let last = self.last_frame_ms.load(Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        Some(Duration::from_millis(
            ms(self.born.elapsed()).saturating_sub(last),
        ))
    }

    fn healthy(&self) -> bool {
        self.alive.load(Ordering::Relaxed) && self.failures.load(Ordering::Relaxed) == 0
    }
}

/// What `BILI_GLASS_FAILTEST` injects on one attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Injection {
    /// The capture call fails outright (`Fail::Injected`).
    Fail,
    /// The blit "succeeds" but the DIB is all zero - what a locked session, a
    /// slept display or a reset driver actually hand back. Exercises the real
    /// black-frame detector and the retention path.
    Black,
}

/// `BILI_GLASS_FAILTEST=<n>`: the first `n` capture attempts after the first
/// delivered frame fail - every third one as an all-black grab, the rest as a
/// GDI failure - so retention, backoff and the rebuild path can be exercised on
/// a real window without breaking anything.
///
/// The countdown lives outside [`Glass`] so a rebuild does not re-arm it.
struct Injector {
    total: u32,
    remaining: AtomicU32,
    armed: AtomicBool,
}

impl Injector {
    fn from_env() -> Arc<Injector> {
        let total = std::env::var("BILI_GLASS_FAILTEST")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);
        Arc::new(Injector {
            total,
            remaining: AtomicU32::new(total),
            armed: AtomicBool::new(false),
        })
    }

    /// Arm the countdown once the first real frame has been delivered, so there
    /// is a last-good frame to retain. Returns true only on the arming call.
    fn arm(&self) -> bool {
        self.total > 0 && !self.armed.swap(true, Ordering::SeqCst)
    }

    /// Consume one injected failure, if any are left.
    fn take(&self) -> Option<Injection> {
        if !self.armed.load(Ordering::Relaxed) {
            return None;
        }
        loop {
            let left = self.remaining.load(Ordering::Relaxed);
            if left == 0 {
                return None;
            }
            if self
                .remaining
                .compare_exchange_weak(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(if left % 3 == 0 {
                    Injection::Black
                } else {
                    Injection::Fail
                });
            }
        }
    }

    fn exhausted(&self) -> bool {
        self.total > 0 && self.remaining.load(Ordering::Relaxed) == 0
    }
}

/// Why a capture produced no usable pixels.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// The window is hidden (tray) - an expected pause, not a fault.
    Hidden,
    /// The window is minimised - an expected pause, not a fault.
    Iconic,
    /// The virtual desktop reported no area (display mode change, unlock).
    NoArea,
    /// `GetDC(NULL)` returned null.
    ScreenDc,
    /// `CreateCompatibleDC` failed.
    MemDc,
    /// The DIB could not be (re)created; the previous one is kept.
    Dib,
    /// `BitBlt` failed; carries `GetLastError`.
    BitBlt(u32),
    /// The blit reported success but the whole grab is black: a locked session,
    /// a powered-off display or a reset driver. Never a real desktop.
    Black,
    /// `BILI_GLASS_FAILTEST` injected failure.
    Injected,
    /// The grab was fine but the frame could not be placed (no window rect).
    Render,
}

impl Fail {
    /// Short name for the log.
    fn label(self) -> String {
        match self {
            Fail::Hidden => "hidden".to_string(),
            Fail::Iconic => "iconic".to_string(),
            Fail::NoArea => "no-area".to_string(),
            Fail::ScreenDc => "screen-dc".to_string(),
            Fail::MemDc => "mem-dc".to_string(),
            Fail::Dib => "dib".to_string(),
            Fail::BitBlt(code) => format!("bitblt({code})"),
            Fail::Black => "black".to_string(),
            Fail::Injected => "inject".to_string(),
            Fail::Render => "render".to_string(),
        }
    }

    /// A pause is the window being unavailable by design: no frame is due, and
    /// nothing is retried or counted as a fault.
    fn is_pause(self) -> bool {
        matches!(self, Fail::Hidden | Fail::Iconic)
    }
}

/// Result of one refresh step: the frame (if one was produced), whether it came
/// from a fresh grab, and the capture failure (if any).
///
/// A step can carry both: a blur change is still served from the cached grab
/// while the capture that was due failed.
struct Step {
    frame: Option<GlassFrame>,
    fresh: bool,
    fail: Option<Fail>,
}

pub struct Glass {
    hwnd: isize,
    /// DIB the screen is blitted into; sized to the virtual desktop and
    /// re-created when that changes (a monitor change is rare, keeping it
    /// makes every grab a single BitBlt into memory GDI already owns).
    dib: isize,
    bits: *mut u8,
    dib_w: i32,
    dib_h: i32,
    /// True once `bits` holds a live grab. The pixels are read in place (see
    /// `grab`), so this flag plus `dib_w`/`dib_h` is all the state the grab
    /// needs. Cleared whenever the pixels can no longer be trusted (a new,
    /// zeroed DIB; a black grab).
    have_shot: bool,
    /// Origin of the grabbed desktop in physical screen pixels: the virtual
    /// desktop's origin, negative when a monitor sits left of or above the
    /// primary.
    shot_origin: (i32, i32),
    /// Size of the grabbed desktop in physical pixels.
    shot_w: i32,
    shot_h: i32,
    /// Sigma the cached pixels were last blurred at; negative = never.
    rendered_sigma: f32,
    /// Optional `BILI_GLASS_FAILTEST` countdown, owned by the worker so a
    /// rebuild never re-arms it.
    inject: Option<Arc<Injector>>,
}

impl Glass {
    fn new(hwnd: isize) -> Glass {
        Glass {
            hwnd,
            dib: 0,
            bits: std::ptr::null_mut(),
            dib_w: 0,
            dib_h: 0,
            have_shot: false,
            shot_origin: (0, 0),
            shot_w: 0,
            shot_h: 0,
            rendered_sigma: -1.0,
            inject: None,
        }
    }

    /// Hand the fail-test countdown to a freshly built engine.
    fn set_injector(&mut self, inject: Arc<Injector>) {
        self.inject = Some(inject);
    }

    /// Throw the capture stack away: delete the DIB, forget the grab, and clear
    /// the capture affinity the window may have kept from an interrupted grab.
    ///
    /// The last delivered frame is NOT touched - it is retained by the worker,
    /// not by this engine, which is what keeps the window frosted across a
    /// rebuild.
    fn reset(&mut self) {
        unsafe {
            if self.dib != 0 {
                DeleteObject(self.dib);
            }
            SetWindowDisplayAffinity(self.hwnd, WDA_NONE);
        }
        self.dib = 0;
        self.bits = std::ptr::null_mut();
        self.dib_w = 0;
        self.dib_h = 0;
        self.have_shot = false;
        self.rendered_sigma = -1.0;
    }

    /// One refresh step, on the worker thread. `recapture` forces a fresh grab
    /// (the cadence keeps the desktop behind the window live); a blur change
    /// re-renders from the cached grab. A window move is NOT a reason to grab:
    /// the grab covers the whole desktop and the placement keeps it registered
    /// with the desktop, so the pixels stay valid wherever the window goes.
    ///
    /// A failed grab leaves the cached grab in place, so a pending blur change
    /// is still served (from slightly stale pixels) rather than dropped.
    fn frame(&mut self, sigma: f32, scale: f32, recapture: bool) -> Step {
        let grabbed = recapture || !self.have_shot;
        let mut fail = None;
        let mut fresh = false;
        if grabbed {
            let inject = self.inject.clone();
            match self.capture(inject.as_deref()) {
                Ok(()) => fresh = true,
                Err(why) => fail = Some(why),
            }
        }
        let resigma = (sigma - self.rendered_sigma).abs() > 0.25 || self.rendered_sigma < 0.0;
        if fresh || resigma {
            if let Some(out) = self.render(sigma, scale) {
                self.rendered_sigma = sigma;
                return Step {
                    frame: Some(out),
                    fresh,
                    fail,
                };
            }
            // The grab was fine but nothing could be placed: report it so the
            // worker backs off and retries instead of staying silent.
            if fresh && fail.is_none() {
                fail = Some(Fail::Render);
            }
        }
        Step {
            frame: None,
            fresh: false,
            fail,
        }
    }

    /// Physical top-left of the window, or `None` when the read fails.
    fn window_origin(&self) -> Option<(i32, i32)> {
        window_origin(self.hwnd)
    }

    /// The cached grab as BGRA bytes, top-down, or `None` before the first
    /// capture. The pixels are read straight out of the DIB - the section
    /// lives as long as this `Glass` and is only rewritten by `capture`, which
    /// takes `&mut self` - so no full-desktop copy is kept around.
    fn grab(&self) -> Option<&[u8]> {
        if !self.have_shot || self.bits.is_null() || self.dib_w < 2 || self.dib_h < 2 {
            return None;
        }
        let len = (self.dib_w as usize) * (self.dib_h as usize) * 4;
        // Safety: `bits` points at `dib_w * dib_h * 4` readable bytes for as
        // long as the DIB section exists, i.e. until the next `ensure_dib`
        // (only reachable through `capture`) or `Drop`.
        Some(unsafe { std::slice::from_raw_parts(self.bits as *const u8, len) })
    }

    /// Grab the whole virtual desktop.
    ///
    /// A hidden or minimised window is a pause (`Fail::Hidden`/`Fail::Iconic`):
    /// there is nothing to show and GDI would grab a desktop the window is not
    /// part of. Every other error leaves the previous grab untouched.
    fn capture(&mut self, inject: Option<&Injector>) -> Result<(), Fail> {
        unsafe {
            if IsWindowVisible(self.hwnd) == 0 {
                return Err(Fail::Hidden);
            }
            if IsIconic(self.hwnd) != 0 {
                return Err(Fail::Iconic);
            }
            let injection = inject.and_then(Injector::take);
            if matches!(injection, Some(Injection::Fail)) {
                return Err(Fail::Injected);
            }
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if w < 2 || h < 2 {
                return Err(Fail::NoArea);
            }
            if !self.ensure_dib(w, h) {
                return Err(Fail::Dib);
            }
            let screen = GetDC(0);
            if screen == 0 {
                return Err(Fail::ScreenDc);
            }
            let mem = CreateCompatibleDC(screen);
            if mem == 0 {
                ReleaseDC(0, screen);
                return Err(Fail::MemDc);
            }
            let old = SelectObject(mem, self.dib);
            // Excluded only around the blit: outside this moment the window
            // must stay capturable, or every screenshot of the app itself
            // would come out without the app. The guard restores it on every
            // path, including an unwind.
            let guard = AffinityGuard::new(self.hwnd);
            // Source coordinates are virtual-screen coordinates - negative
            // when a monitor sits left of / above the primary - and the
            // desktop DC covers the whole virtual screen.
            let ok = BitBlt(mem, 0, 0, w, h, screen, x, y, ROP);
            drop(guard);
            SelectObject(mem, old);
            DeleteDC(mem);
            ReleaseDC(0, screen);
            if ok == 0 {
                return Err(Fail::BitBlt(GetLastError()));
            }
            self.shot_origin = (x, y);
            self.shot_w = w;
            self.shot_h = h;
            self.have_shot = true;
            if matches!(injection, Some(Injection::Black)) {
                // Simulate the driver handing back a zeroed surface: the real
                // black-frame path, detector included.
                let len = (w as usize) * (h as usize) * 4;
                std::ptr::write_bytes(self.bits, 0, len);
            }
            if self.black() {
                // Do not keep a black grab: blurring it into the window is
                // exactly the "flat solid colour" this module must never show.
                self.have_shot = false;
                return Err(Fail::Black);
            }
            Ok(())
        }
    }

    /// True when every sampled pixel of the current grab is exactly 0.
    ///
    /// A real desktop is never all-zero (the compositor's own dithering and the
    /// tint below the blur make that impossible in practice), while an all-zero
    /// grab is what GDI returns for a locked session, a display that slept or a
    /// display driver reset.
    fn black(&self) -> bool {
        let Some(src) = self.grab() else {
            return false;
        };
        let w = self.dib_w as usize;
        let h = self.dib_h as usize;
        if w == 0 || h == 0 {
            return false;
        }
        let step_x = (w / 64).max(1);
        let step_y = (h / 64).max(1);
        let mut y = 0;
        while y < h {
            let row = y * w * 4;
            let mut x = 0;
            while x < w {
                let i = row + x * 4;
                if src[i] != 0 || src[i + 1] != 0 || src[i + 2] != 0 {
                    return false;
                }
                x += step_x;
            }
            y += step_y;
        }
        true
    }

    /// Blur the cached grab (`sigma` in physical pixels) and place the
    /// result against the desktop. `scale` is the window's scale factor: the
    /// placement comes back in logical pixels, which is what Slint lays out
    /// in. Runs on the worker thread; the UI thread receives the result as a
    /// [`GlassFrame`].
    fn render(&mut self, sigma: f32, scale: f32) -> Option<GlassFrame> {
        // A minimised window reports a -32000 rect: rendering here would place
        // the image ~32000px away and the window would show only its tint.
        if window_iconic(self.hwnd) {
            return None;
        }
        let src = self.grab()?;
        let w = self.dib_w as usize;
        let h = self.dib_h as usize;
        // Blur scale: enough downscale that even the strongest radius stays
        // a small number of pixels wide here, no more (the renderer's
        // linear upscale hides the rest of the sampling).
        let factor = if sigma < 3.0 {
            1
        } else {
            ((sigma / 8.0).round() as usize).clamp(1, 4)
        };
        let (mut rgb, bw, bh) = downscale(src, w, h, factor);
        // Three box passes per axis approximate a Gaussian whose sigma is
        // close to the box radius; below sigma 1 the blur is skipped so the
        // slider's zero point can stay a true "no blur" state.
        let radius = ((sigma / factor as f32).round() as usize).max(1);
        if sigma >= 1.0 {
            for _ in 0..3 {
                box_pass(&mut rgb, bw, bh, radius);
            }
        }
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(bw as u32, bh as u32);
        let dst = buffer.make_mut_bytes();
        for (i, px) in rgb.chunks_exact(3).enumerate() {
            dst[i * 4] = px[0];
            dst[i * 4 + 1] = px[1];
            dst[i * 4 + 2] = px[2];
            dst[i * 4 + 3] = 255;
        }
        let (wx, wy) = self.window_origin()?;
        if wx <= ICONIC_ORIGIN && wy <= ICONIC_ORIGIN {
            return None;
        }
        Some(GlassFrame {
            buffer,
            placed: place_for(self.shot_origin, self.shot_w, self.shot_h, wx, wy, scale),
            shot_origin: self.shot_origin,
            shot_w: self.shot_w,
            shot_h: self.shot_h,
            sigma,
        })
    }

    /// (Re)create the DIB when the desktop size changed.
    ///
    /// Create-then-swap: a transient allocation failure keeps the previous DIB
    /// (and its grab) usable instead of destroying the last good backdrop.
    fn ensure_dib(&mut self, w: i32, h: i32) -> bool {
        if self.dib != 0 && self.dib_w == w && self.dib_h == h {
            return true;
        }
        let (dib, bits) = create_dib(w, h);
        if dib == 0 || bits.is_null() {
            return false;
        }
        unsafe {
            if self.dib != 0 {
                DeleteObject(self.dib);
            }
        }
        self.dib = dib;
        self.bits = bits;
        self.dib_w = w;
        self.dib_h = h;
        // A new DIB starts zeroed, so the old grab is gone. Leaving `have_shot`
        // set would blur a black image if the blit that follows fails - the
        // exact flat-colour failure this module guards against.
        self.have_shot = false;
        self.rendered_sigma = -1.0;
        true
    }
}

impl Drop for Glass {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Create a top-down 32bpp DIB sized `w`x`h`, returning the bitmap and its
/// pixel pointer, or `(0, null)` on failure.
fn create_dib(w: i32, h: i32) -> (isize, *mut u8) {
    let bmi = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: w,
            // Negative height = top-down rows: row 0 is the top of the
            // desktop, matching the image Slint expects.
            height: -h,
            planes: 1,
            bit_count: 32,
            compression: 0, // BI_RGB
            size_image: 0,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [0; 3],
    };
    unsafe {
        let screen = GetDC(0);
        if screen == 0 {
            return (0, std::ptr::null_mut());
        }
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(screen, &bmi, 0, &mut bits, 0, 0);
        ReleaseDC(0, screen);
        (dib, bits as *mut u8)
    }
}

/// Keeps the window out of capture APIs for exactly one blit and restores it
/// afterwards - on every return path and while unwinding, so an interrupted
/// grab can never leave the app invisible to screenshots.
struct AffinityGuard {
    hwnd: isize,
}

impl AffinityGuard {
    fn new(hwnd: isize) -> AffinityGuard {
        unsafe {
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        AffinityGuard { hwnd }
    }
}

impl Drop for AffinityGuard {
    fn drop(&mut self) {
        unsafe {
            SetWindowDisplayAffinity(self.hwnd, WDA_NONE);
        }
    }
}

/// One request from the UI thread to the glass worker.
pub enum GlassRequest {
    /// Capture (when `recapture`) and/or re-blur the cached grab at `sigma`.
    /// `scale` only expresses the returned placement in logical pixels.
    Grab {
        sigma: f32,
        scale: f32,
        recapture: bool,
    },
    /// Stop the worker: sent on app exit, served without another grab.
    Shutdown,
}

/// A finished grab + blur, built on the worker thread and handed to the UI
/// thread.
///
/// The pixel buffer is `Send`; the UI thread turns it into a `slint::Image`,
/// which is not `Send` because it caches per-thread state - the same split the
/// cover thumbnails use.
///
/// Clone shares the pixel buffer (the worker keeps the last good frame and
/// re-sends it while captures fail); it never copies the pixels.
#[derive(Clone)]
pub struct GlassFrame {
    /// The blurred grab, RGBA, opaque.
    pub buffer: SharedPixelBuffer<Rgba8Pixel>,
    /// Placement of the grab inside the window, logical pixels, at the window
    /// origin read during this render (the UI thread re-places from the raw
    /// origin/size below on every move).
    pub placed: (f32, f32, f32, f32),
    /// Origin of the grab in physical desktop pixels: the virtual desktop's
    /// origin, negative when a monitor sits left of or above the primary.
    pub shot_origin: (i32, i32),
    /// Size of the grab in physical pixels.
    pub shot_w: i32,
    pub shot_h: i32,
    /// Sigma the delivered pixels were rendered at, physical pixels.
    pub sigma: f32,
}

/// Placement of a grab inside the window, in logical pixels: the grab origin
/// relative to the window origin, then the grab size.
///
/// Pure math on values already read - the drag path calls this with one fresh
/// `GetWindowRect` in the same pointer event that moved the window, so the
/// image follows the window immediately instead of waiting for a capture.
pub fn place_for(
    shot_origin: (i32, i32),
    shot_w: i32,
    shot_h: i32,
    wx: i32,
    wy: i32,
    scale: f32,
) -> (f32, f32, f32, f32) {
    let scale = if scale <= 0.0 { 1.0 } else { scale };
    (
        (shot_origin.0 - wx) as f32 / scale,
        (shot_origin.1 - wy) as f32 / scale,
        shot_w as f32 / scale,
        shot_h as f32 / scale,
    )
}

/// Physical top-left of a window, or `None` when the read fails.
fn window_origin(hwnd: isize) -> Option<(i32, i32)> {
    let mut r = Rect::default();
    (unsafe { GetWindowRect(hwnd, &mut r) } != 0).then_some((r.left, r.top))
}

/// True while the window is minimised: its `-32000` rect would place the grab
/// nowhere, so placement is skipped and the previous one survives the restore.
pub fn window_iconic(hwnd: isize) -> bool {
    unsafe { IsIconic(hwnd) != 0 }
}

/// The dedicated glass thread: owns the [`Glass`] (HWND, DIB, shot
/// origin/size, last rendered sigma) and serves coalesced grab requests.
///
/// Created on the UI thread; dropping it asks the worker to stop and waits for
/// it, which is only ever reached at app exit - never on a timer or drag path.
pub struct GlassWorker {
    tx: mpsc::Sender<GlassRequest>,
    join: Option<JoinHandle<()>>,
    health: Arc<Health>,
}

/// One queued grab, after coalescing.
#[derive(Clone, Copy)]
struct Job {
    sigma: f32,
    scale: f32,
    recapture: bool,
}

impl Default for Job {
    fn default() -> Job {
        Job {
            sigma: -1.0,
            scale: 1.0,
            recapture: true,
        }
    }
}

impl GlassWorker {
    /// Spawn the worker. `deliver` runs on the worker thread for every finished
    /// frame; the app hops to the UI thread from there
    /// (`slint::invoke_from_event_loop`) - it must not block the worker.
    ///
    /// A frame is also re-delivered from the worker's retained copy while
    /// captures fail (see the module docs), so the callback can fire without a
    /// new capture having happened.
    pub fn spawn(hwnd: isize, deliver: impl Fn(GlassFrame) + Send + 'static) -> GlassWorker {
        let (tx, rx) = mpsc::channel();
        let health = Arc::new(Health::new());
        let worker_health = health.clone();
        match std::thread::Builder::new()
            .name("glass".to_string())
            .spawn(move || worker_loop(hwnd, rx, deliver, worker_health))
        {
            Ok(join) => GlassWorker {
                tx,
                join: Some(join),
                health,
            },
            Err(err) => {
                // No worker: requests fail silently and no frame is ever
                // delivered; the window keeps its tint-only look.
                crate::debug_log(&format!("glass worker spawn err={err}"));
                health.alive.store(false, Ordering::Relaxed);
                GlassWorker {
                    tx,
                    join: None,
                    health,
                }
            }
        }
    }

    /// Queue a request. Never blocks and never fails loudly: the channel is
    /// unbounded and a gone worker just drops it.
    pub fn request(&self, request: GlassRequest) {
        let _ = self.tx.send(request);
    }

    /// Whether the worker thread is alive and its last capture did not fail.
    ///
    /// Purely additive: the recovery paths (retention, retry, rebuild, resume)
    /// run inside the worker, so callers need not poll this for the backdrop to
    /// stay alive. A `false` result is worth logging in a future wiring pass
    /// together with [`GlassWorker::last_frame_age`].
    #[allow(dead_code)]
    pub fn healthy(&self) -> bool {
        self.health.healthy()
    }

    /// Time since the last frame built from a REAL capture, or `None` before
    /// the first one.
    ///
    /// A growing age while [`GlassWorker::healthy`] is true means the window is
    /// paused (minimised or hidden), which is expected and finishes with the
    /// next visible request. A growing age while `healthy()` is false means the
    /// worker is retrying a failing capture and is still serving the retained
    /// frame.
    #[allow(dead_code)]
    pub fn last_frame_age(&self) -> Option<Duration> {
        self.health.age()
    }

    /// Consecutive failed captures, 0 when the last capture succeeded.
    #[allow(dead_code)]
    pub fn consecutive_failures(&self) -> u32 {
        self.health.failures.load(Ordering::Relaxed)
    }

    /// Ask the worker to stop and wait for it. App exit only.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(GlassRequest::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for GlassWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Worker main loop: serve grab requests until `Shutdown` or a closed channel.
///
/// A panic in the pipeline ends one session only: it is logged and a fresh
/// [`Worker`] (new capture stack, new state) takes over on the same thread and
/// the same channel, so a bug can never leave the app with a dead worker and no
/// frames ever again.
fn worker_loop(
    hwnd: isize,
    rx: mpsc::Receiver<GlassRequest>,
    deliver: impl Fn(GlassFrame),
    health: Arc<Health>,
) {
    let inject = Injector::from_env();
    if inject.total > 0 {
        crate::debug_log(&format!("glass failtest enabled n={}", inject.total));
    }
    let mut worker = Worker::new(hwnd, &inject, &health);
    let mut panics: u32 = 0;
    loop {
        let session = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker.run(&rx, &deliver);
        }));
        if session.is_ok() {
            break;
        }
        panics = panics.saturating_add(1);
        crate::debug_log(&format!("glass worker panic: respawn n={panics}"));
        // Never turn a deterministic panic into a hot loop.
        std::thread::sleep(Duration::from_millis(RETRY_MIN_MS));
        worker = Worker::new(hwnd, &inject, &health);
    }
    health.alive.store(false, Ordering::Relaxed);
}

/// Worker state: the capture stack, the last good frame, the retry/rebuild
/// counters and the clocks used to notice a resume from sleep.
struct Worker {
    hwnd: isize,
    engine: Glass,
    inject: Arc<Injector>,
    health: Arc<Health>,
    /// Last frame that came from a real grab, re-sent while captures fail.
    last: Option<GlassFrame>,
    /// Consecutive failures at the start of the current step, for the
    /// "recovered after N" log line.
    failures: u32,
    /// Capture-stack rebuilds since the worker started.
    rebuilds: u32,
    /// Absolute failure count at which the next rebuild is due.
    next_rebuild: u32,
    /// Last job, repeated by the self-driven retry.
    last_job: Job,
    /// When the self-driven retry is due; `None` while captures are healthy.
    retry_at: Option<Instant>,
    /// Last wall clock read, to notice a sleep between two requests.
    last_wall: SystemTime,
    /// Matching monotonic read. `Instant` is QPC-backed and does not advance
    /// while the machine sleeps, so wall minus monotonic exposes a resume.
    last_mono: Instant,
    /// When a request last arrived, for the "requests resumed after idle" line.
    last_request: Instant,
    /// Last retention re-send, so the retained frame is not pushed on every
    /// 400ms tick.
    retained_at: Option<Instant>,
    /// Set while the window is unavailable by design (hidden/minimised), with
    /// the moment it was first noticed, so one line covers the whole pause.
    paused: Option<(Instant, Fail)>,
    /// Set once the fail-test countdown has been reported as exhausted.
    injected_done: bool,
}

impl Worker {
    fn new(hwnd: isize, inject: &Arc<Injector>, health: &Arc<Health>) -> Worker {
        let mut engine = Glass::new(hwnd);
        engine.set_injector(inject.clone());
        Worker {
            hwnd,
            engine,
            inject: inject.clone(),
            health: health.clone(),
            last: None,
            failures: 0,
            rebuilds: 0,
            next_rebuild: REBUILD_AFTER,
            last_job: Job::default(),
            retry_at: None,
            last_wall: SystemTime::now(),
            last_mono: Instant::now(),
            last_request: Instant::now(),
            retained_at: None,
            paused: None,
            injected_done: false,
        }
    }

    /// Serve requests until `Shutdown` or a closed channel.
    ///
    /// While captures fail the wait is bounded by the retry deadline, so the
    /// worker retries on its own even if the UI stopped sending requests.
    fn run(&mut self, rx: &mpsc::Receiver<GlassRequest>, deliver: &dyn Fn(GlassFrame)) {
        loop {
            let request = match self.retry_at {
                Some(deadline) => {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(request) => Some(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match rx.recv() {
                    Ok(request) => Some(request),
                    Err(_) => return,
                },
            };

            let mut job = match request.as_ref() {
                Some(GlassRequest::Grab {
                    sigma,
                    scale,
                    recapture,
                }) => Job {
                    sigma: *sigma,
                    scale: *scale,
                    recapture: *recapture,
                },
                Some(GlassRequest::Shutdown) => return,
                // The retry deadline fired: repeat the last job with a forced
                // grab so a capture blocked by a lock screen, a display mode
                // change or a resumed session is picked up without the caller.
                None => Job {
                    recapture: true,
                    ..self.last_job
                },
            };

            if request.is_some() {
                // Coalesce: everything queued while the previous grab was being
                // served is drained, and only the latest request survives (the
                // `recapture` flag is OR-ed so a capture that was due is never
                // dropped by a later blur-only request).
                loop {
                    match rx.try_recv() {
                        Ok(GlassRequest::Grab {
                            sigma,
                            scale,
                            recapture,
                        }) => {
                            job.sigma = sigma;
                            job.scale = scale;
                            job.recapture |= recapture;
                        }
                        Ok(GlassRequest::Shutdown) => return,
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                let idle = self.last_request.elapsed();
                if idle >= Duration::from_secs(2) {
                    trace(&format!(
                        "glass worker requests resumed idle_ms={}",
                        ms(idle)
                    ));
                }
                self.last_request = Instant::now();
            }

            self.step(job, deliver);
        }
    }

    /// One request (or one self-driven retry).
    fn step(&mut self, job: Job, deliver: &dyn Fn(GlassFrame)) {
        self.check_resume();
        self.last_job = job;
        let started = Instant::now();
        let step = self.engine.frame(job.sigma, job.scale, job.recapture);
        let delivered = step.frame.is_some();

        if let Some(frame) = step.frame {
            let (sig, range) = fingerprint(&frame.buffer);
            crate::debug_log(&format!(
                "glass worker grab ms={:.1} recapture={} sigma={:.1} t_ms={} sig={sig:#x} \
                 range={range}",
                started.elapsed().as_secs_f64() * 1000.0,
                u8::from(job.recapture),
                job.sigma,
                ms(self.health.born.elapsed()),
            ));
            if step.fresh {
                if let Some((paused_at, kind)) = self.paused.take() {
                    trace(&format!(
                        "glass capture resumed paused_ms={} kind={} t_ms={}",
                        ms(paused_at.elapsed()),
                        kind.label(),
                        ms(self.health.born.elapsed()),
                    ));
                }
                if self.failures > 0 {
                    trace(&format!(
                        "glass capture recovered after_failures={} frames={}",
                        self.failures,
                        self.health.frames.load(Ordering::Relaxed),
                    ));
                    self.failures = 0;
                }
                self.health.note_frame();
                self.rebuilds = 0;
                self.next_rebuild = REBUILD_AFTER;
                self.retry_at = None;
                self.retained_at = None;
                if self.inject.arm() {
                    trace(&format!(
                        "glass failtest armed injections={}",
                        self.inject.total
                    ));
                }
            }
            deliver(frame.clone());
            // Whatever was just delivered is the picture to keep serving: a
            // stale-grab re-blur at a new radius is as good as a fresh frame for
            // that purpose.
            self.last = Some(frame);
        }

        if let Some(fail) = step.fail {
            self.note_failure(fail, job, deliver, delivered);
        }
    }

    /// Handle a failed capture: pause, or count, log, back off, rebuild and
    /// keep the retained frame in front of the user. `delivered` says a frame
    /// already went out for this step (a re-blur of the cached grab), so the
    /// retained frame must not overwrite it.
    fn note_failure(
        &mut self,
        fail: Fail,
        job: Job,
        deliver: &dyn Fn(GlassFrame),
        delivered: bool,
    ) {
        if fail.is_pause() {
            // Hidden or minimised: nothing is due, nothing is wrong. The caller
            // keeps its cadence and the retained frame stays on screen; the
            // first visible request grabs again. Logged once per pause, not per
            // tick (the caller asks every 400ms).
            if self.paused.is_none() {
                trace(&format!(
                    "glass capture paused kind={} t_ms={}",
                    fail.label(),
                    ms(self.health.born.elapsed()),
                ));
                self.paused = Some((Instant::now(), fail));
            }
            self.retry_at = None;
            return;
        }
        let failures = self.health.note_failure();
        self.failures = failures;
        let backoff = backoff_ms(failures);
        self.retry_at = Some(Instant::now() + Duration::from_millis(backoff));
        trace(&format!(
            "glass capture fail kind={} consec={} backoff_ms={} age_ms={} t_ms={}",
            fail.label(),
            failures,
            backoff,
            self.health.age().map_or(0, ms),
            ms(self.health.born.elapsed()),
        ));
        if failures >= self.next_rebuild {
            self.rebuild(failures);
        }
        if self.inject.exhausted() && !self.injected_done {
            self.injected_done = true;
            trace(&format!(
                "glass failtest injections exhausted total={}",
                self.inject.total
            ));
        }
        if !delivered {
            self.retain(job.scale, failures, fail, deliver);
        }
    }

    /// Replace the whole capture stack (DC, DIB, affinity, cached grab). The
    /// retained frame lives here, not in the engine, so the window stays
    /// frosted across the rebuild.
    fn rebuild(&mut self, failures: u32) {
        self.engine = Glass::new(self.hwnd);
        self.engine.set_injector(self.inject.clone());
        self.rebuilds = self.rebuilds.saturating_add(1);
        self.next_rebuild = failures.saturating_add(rebuild_gap(self.rebuilds));
        self.health
            .rebuilds
            .store(self.rebuilds, Ordering::Relaxed);
        crate::debug_log(&format!(
            "glass worker rebuild n={} total={} after_failures={failures} next_at={} t_ms={}",
            self.rebuilds,
            self.health.rebuilds.load(Ordering::Relaxed),
            self.next_rebuild,
            ms(self.health.born.elapsed()),
        ));
    }

    /// Re-send the last good frame while captures fail, re-placed to the
    /// window's current position so a window that moved during the outage still
    /// shows the blur in the right place.
    fn retain(&mut self, scale: f32, failures: u32, fail: Fail, deliver: &dyn Fn(GlassFrame)) {
        if let Some(at) = self.retained_at {
            if at.elapsed() < Duration::from_millis(RETAIN_RESEND_MS) {
                return;
            }
        }
        let Some(last) = self.last.as_ref() else {
            return;
        };
        let frame = self.relay(last, scale);
        self.retained_at = Some(Instant::now());
        let (sig, range) = fingerprint(&frame.buffer);
        trace(&format!(
            "glass retain reason={} consec={} age_ms={} sigma={:.1} sig={sig:#x} range={range} \
             t_ms={}",
            fail.label(),
            failures,
            self.health.age().map_or(0, ms),
            frame.sigma,
            ms(self.health.born.elapsed()),
        ));
        deliver(frame);
    }

    /// A retained frame with the placement recomputed for where the window is
    /// now; the pixels and the shot metadata are shared (no copy).
    fn relay(&self, frame: &GlassFrame, scale: f32) -> GlassFrame {
        let current = match window_origin(self.hwnd) {
            Some((wx, wy)) if !window_iconic(self.hwnd) && wx > ICONIC_ORIGIN => Some((wx, wy)),
            _ => None,
        };
        let placed = match current {
            Some((wx, wy)) => place_for(
                frame.shot_origin,
                frame.shot_w,
                frame.shot_h,
                wx,
                wy,
                scale,
            ),
            _ => frame.placed,
        };
        GlassFrame {
            buffer: frame.buffer.clone(),
            placed,
            shot_origin: frame.shot_origin,
            shot_w: frame.shot_w,
            shot_h: frame.shot_h,
            sigma: frame.sigma,
        }
    }

    /// Notice a resume from sleep (or a clock step) and rebuild the capture
    /// stack so the next grab does not reuse a desktop DC from before the
    /// sleep. Logged because it is the one "the machine was away" signal this
    /// module has.
    fn check_resume(&mut self) {
        let mono = Instant::now();
        let wall = SystemTime::now();
        let mono_ms = mono.saturating_duration_since(self.last_mono).as_millis();
        let wall_ms = match wall.duration_since(self.last_wall) {
            Ok(delta) => delta.as_millis(),
            // Clock stepped backwards.
            Err(_) => u128::MAX,
        };
        self.last_mono = mono;
        self.last_wall = wall;
        let skew = u128::from(RESUME_SKEW_MS);
        if wall_ms > mono_ms.saturating_add(skew) || wall_ms.saturating_add(skew) < mono_ms {
            self.engine = Glass::new(self.hwnd);
            self.engine.set_injector(self.inject.clone());
            self.rebuilds = 0;
            self.next_rebuild = REBUILD_AFTER;
            crate::debug_log(&format!(
                "glass resume wall_ms={wall_ms} monotonic_ms={mono_ms} capture rebuilt"
            ));
        }
    }
}

/// Retry delay after `consecutive` failed captures: the caller's own cadence
/// first, then the cap. Never a busy loop, never slower than the recovery
/// budget.
fn backoff_ms(consecutive: u32) -> u64 {
    if consecutive <= 1 {
        RETRY_MIN_MS
    } else {
        RETRY_MAX_MS
    }
}

/// Cheap fingerprint of a delivered frame for the log: a sampled luminance hash
/// plus the observed luminance range.
///
/// A retained frame can then be shown to be exactly the pixels of the last good
/// capture (same `sig`) and to be a real picture rather than a flat colour
/// (`range` > 0). Sampled on a 64x64 grid, so the cost is negligible next to
/// the blur.
fn fingerprint(buffer: &SharedPixelBuffer<Rgba8Pixel>) -> (u64, u8) {
    let bytes = buffer.as_bytes();
    let w = buffer.width() as usize;
    let h = buffer.height() as usize;
    if w == 0 || h == 0 || bytes.len() < w * h * 4 {
        return (0, 0);
    }
    let step_x = (w / 64).max(1);
    let step_y = (h / 64).max(1);
    let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
    let mut min = u8::MAX;
    let mut max = 0u8;
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let i = (y * w + x) * 4;
            let lum = ((bytes[i] as u32 + bytes[i + 1] as u32 + bytes[i + 2] as u32) / 3) as u8;
            min = min.min(lum);
            max = max.max(lum);
            sig ^= u64::from(lum);
            sig = sig.wrapping_mul(0x0000_0100_0000_01b3);
            x += step_x;
        }
        y += step_y;
    }
    (sig, max.saturating_sub(min))
}

/// Failures between rebuilds after the `rebuilds`-th one: 5, 10, 20, 40, 80,
/// then 80 - a stuck state still gets a fresh capture stack about once a
/// minute instead of hammering GDI.
fn rebuild_gap(rebuilds: u32) -> u32 {
    let doublings = rebuilds.saturating_sub(1).min(4);
    REBUILD_AFTER.saturating_mul(1u32 << doublings)
}

/// Box-downscale BGRA to RGB. `factor` 1 is a plain repack (no averaging),
/// which keeps the sharp slider position genuinely sharp.
fn downscale(src: &[u8], w: usize, h: usize, factor: usize) -> (Vec<u8>, usize, usize) {
    let bw = w.div_ceil(factor);
    let bh = h.div_ceil(factor);
    let mut out = vec![0u8; bw * bh * 3];
    for by in 0..bh {
        for bx in 0..bw {
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in (by * factor)..((by + 1) * factor).min(h) {
                for x in (bx * factor)..((bx + 1) * factor).min(w) {
                    let i = (y * w + x) * 4;
                    b += src[i] as u32;
                    g += src[i + 1] as u32;
                    r += src[i + 2] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let o = (by * bw + bx) * 3;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
        }
    }
    (out, bw, bh)
}

/// One separable box-blur pass over interleaved RGB. Edges clamp (a pixel
/// with no neighbour on one side counts itself), which keeps the blur from
/// darkening the image border the way zero-padding would.
fn box_pass(rgb: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let rad = r as isize;
    let win = (2 * r + 1) as i32;
    let mut tmp = vec![0u8; w * h * 3];
    // horizontal: rgb -> tmp
    for y in 0..h {
        let base = y * w * 3;
        for c in 0..3 {
            let mut sum: i32 = 0;
            for k in -rad..=rad {
                let x = k.clamp(0, w as isize - 1) as usize;
                sum += rgb[base + x * 3 + c] as i32;
            }
            for x in 0..w {
                tmp[base + x * 3 + c] = (sum / win) as u8;
                let add = (x as isize + rad + 1).clamp(0, w as isize - 1) as usize;
                let sub = (x as isize - rad).clamp(0, w as isize - 1) as usize;
                sum += rgb[base + add * 3 + c] as i32 - rgb[base + sub * 3 + c] as i32;
            }
        }
    }
    // vertical: tmp -> rgb
    for x in 0..w {
        for c in 0..3 {
            let mut sum: i32 = 0;
            for k in -rad..=rad {
                let y = k.clamp(0, h as isize - 1) as usize;
                sum += tmp[(y * w + x) * 3 + c] as i32;
            }
            for y in 0..h {
                rgb[(y * w + x) * 3 + c] = (sum / win) as u8;
                let add = (y as isize + rad + 1).clamp(0, h as isize - 1) as usize;
                let sub = (y as isize - rad).clamp(0, h as isize - 1) as usize;
                sum += tmp[(add * w + x) * 3 + c] as i32 - tmp[(sub * w + x) * 3 + c] as i32;
            }
        }
    }
}
