//! The 120 fps video surface: the picture on its own Win32 window.
//!
//! # Why this exists
//!
//! Everything below the screen is fast enough for a 120 fps grid: the pacer
//! produces 119.5 frames/s at 6.9 % GPU and the blend pass costs 1.0-1.6 ms at
//! 960x540. The screen is the wall. Slint repaints the **whole player window**
//! for every picture: the item tree is re-tessellated, the frame's RGBA texture
//! is created, uploaded and destroyed, and the result is presented through a
//! FIFO swapchain. That costs 16.7 ms per present at 60 fps and 12-20 ms when
//! a comment-heavy window is asked for 120 — one window, one present per
//! refresh, and the 8.3 ms a 120 Hz frame has do not fit inside it.
//!
//! This module takes the picture out of that window. The player window keeps
//! its chrome, its comments and its input; the picture is blitted into a
//! **separate top-level window** that sits directly behind it, on its own
//! thread, at whatever rate the pacer produces. The player window's video band
//! goes transparent (see `external_video` in `ui/app.slint`), so the picture
//! shows through it and everything the UI draws — comments, the pill, the
//! title band, panels — stays on top, because the player window is above the
//! surface. Nothing about input changes: the player window is still the hit
//! target for every click, drag and double-click, so seek, drag, fullscreen and
//! the occlusion pause all keep working exactly as they did.
//!
//! # Why GDI and not a D3D11 swapchain
//!
//! The transport is one `StretchDIBits` of an already-converted RGBA frame into
//! a window DC (the frames arrive as RGBA, the DC wants BGRA, so one SSE swap
//! per frame). Measured 1.1-2.4 ms per present at 960x540 on the surface
//! thread, which is 13-29 % of one core at 120 fps — and it is not the UI
//! thread. A flip-model swapchain would move that work to the GPU but needs a
//! quad shader to get 适配/填充/拉伸 right; GDI takes the fit mode and the crop
//! rectangle as arguments, so the three modes are three rectangles.
//!
//! # Ownership and lifetime
//!
//! The surface is created by the player window host (UI thread) and lives as
//! long as the window. `submit` is called from the UI thread with the frame the
//! pacer produced; it replaces any frame the surface has not shown yet (the
//! same one-slot mailbox the Slint path uses) and wakes the surface thread. The
//! surface thread does the blit, returns the buffer to the blend pool and
//! counts the presented frame, so `shown_s` for this path is a real count of
//! blits that reached the screen, not of pushes.
//!
//! Geometry is polled, never pushed: every ~4 ms the surface thread re-reads the
//! player window's client origin and compares the resulting band rectangle with
//! the one it has. A drag, a resize, a fullscreen toggle, a monitor DPI change
//! and a minimise therefore all follow without a single callback into
//! `win.rs`; the same poll hides the surface while the player is minimised,
//! invisible, or covered by the occlusion pause.

use super::*;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetDC,
    ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBRUSH, HDC,
    PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetWindow,
    IsIconic, IsWindow, IsWindowVisible, PeekMessageW, RegisterClassW, SetWindowPos, ShowWindow,
    TranslateMessage, GW_HWNDPREV, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOOWNERZORDER,
    SWP_NOSENDCHANGING, SWP_NOZORDER, SW_HIDE, SW_SHOWNA, WM_APP, WM_ERASEBKGND, WM_NCHITTEST,
    WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_DESTROY};

/// The one message the surface thread waits for besides its geometry poll.
const WM_SURFACE_WAKE: u32 = WM_APP + 0x51;
/// Class name of the picture window (one class for the process).
const CLASS_NAME: &str = "O11A1VideoSurface";

/// What the surface thread has to know about the picture inside the band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SurfaceLayout {
    /// The video band inside the player's client area, **logical** px.
    pub band: (i32, i32, i32, i32),
    /// The player window's scale factor (`band` is logical, the screen is not).
    pub scale: f32,
    /// `fit_mode`: 0 = 适配 (contain), 1 = 填充 (cover), 2 = 拉伸 (fill).
    pub fit: u8,
    /// Picture aspect (width / height) of the frames the decoder produces.
    pub aspect: f64,
    /// The band's own backdrop, `0xRRGGBB` — the letterbox colour.
    pub backdrop: u32,
}

impl Default for SurfaceLayout {
    fn default() -> Self {
        Self {
            band: (0, 0, 0, 0),
            scale: 1.0,
            fit: 0,
            aspect: 16.0 / 9.0,
            backdrop: 0x10_11_14,
        }
    }
}

/// One presented frame's worth of state on the surface thread.
struct Pending {
    buffer: FrameBuf,
    pooled: bool,
}

struct Inner {
    parent: isize,
    /// The picture window, `0` until the thread has created it.
    hwnd: AtomicI64,
    /// `Mutex` + `Condvar` rather than a channel: the mailbox semantic is "keep
    /// only the newest frame", and a channel would queue a backlog the screen
    /// cannot use.
    state: Mutex<Shared>,
    wake: Condvar,
    layout: Mutex<SurfaceLayout>,
    /// Frames handed to the surface and frames it actually blitted.
    pushed: AtomicU64,
    shown: AtomicU64,
    coalesced: AtomicU64,
    blit_us: AtomicU64,
    closed: AtomicBool,
    /// Set once the surface has a window, so `create` can return a usable
    /// handle (and the caller can tell a failure from a slow start).
    ready: Mutex<bool>,
    ready_wake: Condvar,
}

struct Shared {
    pending: Option<Pending>,
    close: bool,
}

impl Inner {
    fn take(&self) -> Option<Pending> {
        self.state.lock().ok().and_then(|mut state| state.pending.take())
    }
}

/// The picture surface for one player window.
pub(super) struct VideoSurface {
    inner: Arc<Inner>,
}

impl VideoSurface {
    /// Create the picture window for `parent` and start its thread.
    ///
    /// Returns `None` when the window could not be created (the caller then
    /// stays on the Slint path, which is the shipped behaviour anyway).
    pub(super) fn create(
        parent: isize,
        pool: Arc<BlendPool>,
        control: Arc<PlaybackControl>,
    ) -> Option<Arc<Self>> {
        if parent == 0 {
            return None;
        }
        let inner = Arc::new(Inner {
            parent,
            hwnd: AtomicI64::new(0),
            state: Mutex::new(Shared {
                pending: None,
                close: false,
            }),
            wake: Condvar::new(),
            layout: Mutex::new(SurfaceLayout::default()),
            pushed: AtomicU64::new(0),
            shown: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            blit_us: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            ready: Mutex::new(false),
            ready_wake: Condvar::new(),
        });
        let surface = Arc::new(Self {
            inner: inner.clone(),
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("video-surface".into())
            .spawn(move || {
                note_thread_role("video");
                run_surface(thread_inner, pool, control);
            })
            .ok()?;
        // The window is created before the first frame can arrive; waiting here
        // keeps `create` synchronous with respect to "the surface exists".
        let ready = inner.ready.lock().ok()?;
        let (ready, _) = inner
            .ready_wake
            .wait_timeout_while(ready, Duration::from_millis(400), |ready| !*ready)
            .ok()?;
        if !*ready || inner.hwnd.load(Ordering::SeqCst) == 0 {
            crate::debug_log("video surface create failed reason=window");
            return None;
        }
        crate::debug_log(&format!(
            "video surface create hwnd={} parent={parent}",
            inner.hwnd.load(Ordering::SeqCst)
        ));
        Some(surface)
    }

    /// Where the band is and how the picture fits inside it (UI thread).
    pub(super) fn set_layout(&self, layout: SurfaceLayout) {
        if let Ok(mut slot) = self.inner.layout.lock() {
            if *slot != layout {
                *slot = layout;
            }
        }
    }

    /// Hand one frame over. Replaces a frame that has not been shown, exactly
    /// like the Slint mailbox, and never blocks.
    pub(super) fn submit(&self, buffer: FrameBuf, pooled: bool) {
        self.inner.pushed.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut state) = self.inner.state.lock() {
            if let Some(replaced) = state.pending.replace(Pending { buffer, pooled }) {
                self.inner.coalesced.fetch_add(1, Ordering::Relaxed);
                let _ = replaced;
            }
            state.close = false;
        }
        self.inner.wake.notify_one();
        let hwnd = self.inner.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            // SAFETY: the handle is this thread's own window and only ever
            // posted to, never destroyed here (`close` does that).
            unsafe { PostMessageW(hwnd as HWND, WM_SURFACE_WAKE, 0, 0) };
        }
    }

    pub(super) fn counters(&self) -> (u64, u64, u64) {
        (
            self.inner.pushed.load(Ordering::Relaxed),
            self.inner.shown.load(Ordering::Relaxed),
            self.inner.coalesced.load(Ordering::Relaxed),
        )
    }

    pub(super) fn blit_us(&self) -> u64 {
        self.inner.blit_us.load(Ordering::Relaxed)
    }

    /// Stop the surface thread and destroy the window.
    pub(super) fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.close = true;
            state.pending = None;
        }
        self.inner.wake.notify_all();
        let hwnd = self.inner.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            // The window belongs to the surface thread, so it is destroyed
            // there (after its own loop exits); this only wakes it.
            unsafe { PostMessageW(hwnd as HWND, WM_SURFACE_WAKE, 0, 0) };
        }
    }
}

impl Drop for VideoSurface {
    fn drop(&mut self) {
        self.close();
    }
}

// ---- the surface thread ----------------------------------------------------

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // The picture window never wants a background erase (it is fully
        // painted by a blit) and never takes a click: the player window above
        // it owns every input.
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            // Validate: the blit is driven by the frame, not by this message.
            let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            unsafe {
                BeginPaint(hwnd, &mut paint);
                EndPaint(hwnd, &paint);
            }
            0
        }
        WM_NCHITTEST => (-1i32) as LRESULT, // HTTRANSPARENT
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn register_class() -> bool {
    static DONE: OnceLock<bool> = OnceLock::new();
    *DONE.get_or_init(|| {
        let name = wide(CLASS_NAME);
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            // SAFETY: a null instance handle is what GetModuleHandleW(NULL)
            // means, and the class outlives every window of it.
            hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: name.as_ptr(),
        };
        // SAFETY: `class` and its class-name string live for this call.
        unsafe { RegisterClassW(&class) != 0 }
    })
}

/// The picture rectangle inside `band`, in logical px, for one fit mode, plus
/// the source rectangle to read out of the frame.
///
/// `contain` letterboxes, `cover` fills and crops, `fill` stretches; this is the
/// same rule the Slint `ImageFit` follows, so switching the 120 mode on does not
/// move the picture.
pub(super) fn fit_rects(
    band: (i32, i32, i32, i32),
    aspect: f64,
    fit: u8,
) -> ((i32, i32, i32, i32), (i32, i32, i32, i32)) {
    let (bx, by, bw, bh) = band;
    if bw <= 0 || bh <= 0 || !(aspect.is_finite() && aspect > 0.05) {
        return ((bx, by, bw.max(0), bh.max(0)), (0, 0, 0, 0));
    }
    let band_aspect = bw as f64 / bh as f64;
    let full_src = (0, 0, 0, 0); // 0x0 = "the whole frame"
    match fit {
        1 => {
            // cover: the destination is the whole band, the source is cropped.
            if band_aspect > aspect {
                let keep = aspect / band_aspect;
                let h = (1.0e6 * keep) as i32;
                let y = (500_000 - h / 2).max(0);
                ((bx, by, bw, bh), (0, y, 1_000_000, h))
            } else {
                let keep = band_aspect / aspect;
                let w = (1.0e6 * keep) as i32;
                let x = (500_000 - w / 2).max(0);
                ((bx, by, bw, bh), (x, 0, w, 1_000_000))
            }
        }
        2 => ((bx, by, bw, bh), full_src),
        _ => {
            // contain: the destination is the aspect-correct box, centred.
            let (w, h) = if band_aspect > aspect {
                (((bh as f64 * aspect) as i32).max(1), bh)
            } else {
                (bw, ((bw as f64 / aspect) as i32).max(1))
            };
            ((bx + (bw - w) / 2, by + (bh - h) / 2, w, h), full_src)
        }
    }
}

/// The band's screen rectangle for the current client origin.
fn band_on_screen(parent: HWND, layout: &SurfaceLayout) -> Option<(i32, i32, i32, i32)> {
    let mut client: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetClientRect(parent, &mut client) } == 0 {
        return None;
    }
    let mut origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(parent, &mut origin) } == 0 {
        return None;
    }
    let scale = if layout.scale.is_finite() && layout.scale > 0.05 {
        layout.scale as f64
    } else {
        1.0
    };
    let (bx, by, bw, bh) = layout.band;
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    // The band is clamped to the client area: a layout in flight must never put
    // the picture outside the window.
    let x = (bx as f64 * scale).round() as i32;
    let y = (by as f64 * scale).round() as i32;
    let w = ((bw as f64 * scale).round() as i32).min((width - x).max(0));
    let h = ((bh as f64 * scale).round() as i32).min((height - y).max(0));
    Some((origin.x + x, origin.y + y, w, h))
}

/// Place `hwnd` on `rect`, immediately behind `parent`, without activating or
/// changing focus. A no-op when nothing changed.
fn place(hwnd: HWND, parent: HWND, current: &mut Option<(i32, i32, i32, i32)>, rect: (i32, i32, i32, i32)) {
    if *current == Some(rect) {
        return;
    }
    *current = Some(rect);
    let behind_ok = unsafe { GetWindow(parent, GW_HWNDPREV) } == hwnd;
    let flags = if behind_ok {
        SWP_NOACTIVATE | SWP_NOSENDCHANGING | SWP_NOZORDER | SWP_NOOWNERZORDER
    } else {
        SWP_NOACTIVATE | SWP_NOSENDCHANGING | SWP_NOOWNERZORDER
    };
    // `?HWND_TOPMOST, not "topmost": with SWP_NOZORDER the insert-after value is
    // ignored, and without it the picture must sit directly *behind* the player.
    let after = if behind_ok {
        std::ptr::null_mut()
    } else {
        parent
    };
    // SAFETY: both handles belong to this process; `after` is only read as a
    // z-order reference.
    unsafe {
        SetWindowPos(hwnd, after, rect.0, rect.1, rect.2, rect.3, flags);
    }
}

fn run_surface(inner: Arc<Inner>, pool: Arc<BlendPool>, control: Arc<PlaybackControl>) {
    if !register_class() {
        return;
    }
    let parent = inner.parent as HWND;
    // SAFETY: the class is registered and the name string lives for this call.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            wide(CLASS_NAME).as_ptr(),
            wide("O11A1 video").as_ptr(),
            WS_POPUP,
            0,
            0,
            16,
            16,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        if let Ok(mut ready) = inner.ready.lock() {
            *ready = true;
            inner.ready_wake.notify_all();
        }
        return;
    }
    inner.hwnd.store(hwnd as isize as i64, Ordering::SeqCst);
    if let Ok(mut ready) = inner.ready.lock() {
        *ready = true;
        inner.ready_wake.notify_all();
    }
    // The blit buffers: one BGRA scratch (GDI wants BGRX) reused for every
    // frame, and one BITMAPINFO whose header is rewritten per size.
    let mut scratch: Vec<u8> = Vec::new();
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB as u32;
    let mut backdrop_brush: Option<(u32, HBRUSH)> = None;
    let mut placed: Option<(i32, i32, i32, i32)> = None;
    let mut visible = false;
    let mut last_layout = SurfaceLayout::default();
    let mut blit_total_us = 0u64;
    let mut blit_count = 0u64;
    let mut presented_at: Option<Instant> = None;
    loop {
        if inner.closed.load(Ordering::SeqCst) {
            break;
        }
        // Pump: the window has to answer WM_PAINT / WM_NCHITTEST even though it
        // draws from its own loop.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Ok(layout) = inner.layout.lock() {
            last_layout = *layout;
        }
        let band = band_on_screen(parent, &last_layout);
        let show = matches!(
            unsafe { (IsIconic(parent), IsWindowVisible(parent), IsWindow(hwnd)) },
            (0, 1, 1)
        );
        let frame = inner.take();
        let painted = match (&frame, band) {
            (Some(pending), Some(band)) if show && band.2 > 2 && band.3 > 2 => {
                let started = Instant::now();
                paint_frame(
                    hwnd,
                    parent,
                    band,
                    &last_layout,
                    pending,
                    &mut scratch,
                    &mut info,
                    &mut backdrop_brush,
                    &mut placed,
                );
                let us = started.elapsed().as_micros() as u64;
                blit_total_us = blit_total_us.saturating_add(us);
                blit_count += 1;
                true
            }
            _ => false,
        };
        if painted {
            // Show only once there is a picture: an empty surface would flash
            // the letterbox colour over whatever the player window drew.
            if !visible {
                unsafe { ShowWindow(hwnd, SW_SHOWNA) };
                visible = true;
            }
            let shown = inner.shown.fetch_add(1, Ordering::Relaxed) + 1;
            // The present interval goes to the same histogram the Slint path
            // fills: this is the number the 120 fps self-check reads.
            if let Some(previous) = presented_at.replace(Instant::now()) {
                control.note_present_interval(
                    previous.elapsed().as_secs_f64() * 1000.0,
                );
            }
            if blit_count > 0 {
                let average = blit_total_us / blit_count;
                inner.blit_us.store(average, Ordering::Relaxed);
                // Twice a second is enough for the self-check's receipt and
                // costs nothing measurable.
                if shown % 60 == 0 {
                    control.set_surface_receipt(shown, average);
                }
            }
            // The frame goes back to the pool only after the blit, so the pacer
            // never has to copy on write for a picture still on its way out.
            if let Some(pending) = frame {
                if pending.pooled {
                    pool.recycle(pending.buffer);
                }
            }
            continue;
        }
        if visible && (!show || band.is_none()) {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            visible = false;
        }
        if let Some(pending) = frame {
            if pending.pooled {
                pool.recycle(pending.buffer);
            }
        }
        // 4 ms: fast enough that a drag never shows the picture lagging the
        // window, slow enough to leave the core to the pacer.
        // Wait for a frame, or 4 ms so the geometry poll above keeps up with a
        // drag. `park_timeout`-style: the guard is dropped right away.
        if let Ok(state) = inner.state.lock() {
            if !state.close && state.pending.is_none() {
                let _guard = inner
                    .wake
                    .wait_timeout(state, Duration::from_millis(4))
                    .map(|(state, _)| state);
            }
        }
    }
    if !hwnd.is_null() {
        unsafe { DestroyWindow(hwnd) };
    }
    inner.hwnd.store(0, Ordering::SeqCst);
    if let Some((_, brush)) = backdrop_brush {
        if !brush.is_null() {
            unsafe { DeleteObject(brush) };
        }
    }
    crate::debug_log(&format!(
        "video surface done blits={blit_count} avg_blit_us={}",
        if blit_count == 0 {
            0
        } else {
            blit_total_us / blit_count
        }
    ));
}

/// One blit: the letterbox colour over the band, then the picture into its
/// aspect-correct rectangle.
#[allow(clippy::too_many_arguments)]
fn paint_frame(
    hwnd: HWND,
    _parent: HWND,
    band: (i32, i32, i32, i32),
    layout: &SurfaceLayout,
    pending: &Pending,
    scratch: &mut Vec<u8>,
    info: &mut BITMAPINFO,
    backdrop_brush: &mut Option<(u32, HBRUSH)>,
    placed: &mut Option<(i32, i32, i32, i32)>,
) {
    place(hwnd, _parent, placed, band);
    let width = pending.buffer.width() as usize;
    let height = pending.buffer.height() as usize;
    if width == 0 || height == 0 {
        return;
    }
    let (dest, src) = fit_rects(band, layout.aspect, layout.fit);
    let hdc: HDC = unsafe { GetDC(hwnd) };
    if hdc.is_null() {
        return;
    }
    // Letterbox: fill the whole band with the player's own backdrop so a bar
    // looks like the Slint band's empty area, not like a black rectangle.
    let brush = match *backdrop_brush {
        Some((color, brush)) if color == layout.backdrop => brush,
        _ => {
            if let Some((_, old)) = backdrop_brush.take() {
                if !old.is_null() {
                    unsafe { DeleteObject(old) };
                }
            }
            let rgb = layout.backdrop;
            // COLORREF is 0x00BBGGRR, the source is 0xRRGGBB.
            let color = (((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)) as COLORREF;
            let brush = unsafe { CreateSolidBrush(color) };
            *backdrop_brush = Some((rgb, brush));
            brush
        }
    };
    if !brush.is_null() {
        let mut whole = RECT {
            left: 0,
            top: 0,
            right: band.2,
            bottom: band.3,
        };
        unsafe { FillRect(hdc, &mut whole, brush) };
    }
    // RGBA → BGRA. One pass over the frame, on this thread, into a buffer that
    // is reused for the process' life.
    let bytes = pending.buffer.as_bytes();
    let len = width * height * 4;
    if scratch.len() < len {
        scratch.resize(len, 0);
    }
    swap_rb(&bytes[..len], &mut scratch[..len]);
    info.bmiHeader.biWidth = width as i32;
    // A negative height means top-down rows, which is what our frames are.
    info.bmiHeader.biHeight = -(height as i32);
    info.bmiHeader.biSizeImage = len as u32;
    // Destination coordinates are relative to the surface window, whose own
    // origin is the band's.
    let dx = dest.0 - band.0;
    let dy = dest.1 - band.1;
    let (sx, sy, sw, sh) = if src.2 > 0 && src.3 > 0 {
        // Crop given in millionths of the frame; convert to pixels.
        let x = (src.0 as i64 * width as i64 / 1_000_000) as i32;
        let y = (src.1 as i64 * height as i64 / 1_000_000) as i32;
        let w = (src.2 as i64 * width as i64 / 1_000_000) as i32;
        let h = (src.3 as i64 * height as i64 / 1_000_000) as i32;
        (x, y, w.max(1), h.max(1))
    } else {
        (0, 0, width as i32, height as i32)
    };
    unsafe {
        StretchDIBits(
            hdc,
            dx,
            dy,
            dest.2.max(1),
            dest.3.max(1),
            sx,
            sy,
            sw,
            sh,
            scratch.as_ptr() as *const core::ffi::c_void,
            info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        ReleaseDC(hwnd, hdc);
    }
}

/// RGBA → BGRA, 8 bytes per iteration (SSE2) with a scalar tail.
///
/// The frames the pacer produces are RGBA (that is what Slint and the
/// screenshot writer take); a GDI `BI_RGB` 32-bit DIB is BGRX. Swapping here
/// keeps the conversion off the UI thread and off the pacer.
fn swap_rb(src: &[u8], dst: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: both slices are the same length, and the loop below stays
        // inside them (8 bytes per step).
        unsafe { swap_rb_sse2(src.as_ptr(), dst.as_mut_ptr(), src.len()) };
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        for (source, target) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            target[0] = source[2];
            target[1] = source[1];
            target[2] = source[0];
            target[3] = 255;
        }
    }
}

/// SSE2 body of [`swap_rb`]: four pixels per iteration, SSE2 only (baseline on
/// x86_64), so it runs everywhere the app does.
#[cfg(target_arch = "x86_64")]
unsafe fn swap_rb_sse2(src: *const u8, dst: *mut u8, len: usize) {
    use std::arch::x86_64::*;
    // SSE2 only (it is baseline on x86_64; `pshufb` would need SSSE3). One
    // 32-bit lane is a pixel `A<<24 | B<<16 | G<<8 | R` in memory order
    // R,G,B,A; the DIB wants B,G,R,X, i.e. `FF<<24 | R<<16 | G<<8 | B`.
    let low = _mm_set1_epi32(0x0000_00FFu32 as i32);
    let green = _mm_set1_epi32(0x0000_FF00u32 as i32);
    let alpha = _mm_set1_epi32(0xFF00_0000u32 as i32);
    let mut index = 0usize;
    while index + 16 <= len {
        let chunk = _mm_loadu_si128(src.add(index) as *const __m128i);
        let red = _mm_slli_epi32(_mm_and_si128(chunk, low), 16);
        let blue = _mm_and_si128(_mm_srli_epi32(chunk, 16), low);
        let green = _mm_and_si128(chunk, green);
        let out = _mm_or_si128(_mm_or_si128(red, blue), _mm_or_si128(green, alpha));
        _mm_storeu_si128(dst.add(index) as *mut __m128i, out);
        index += 16;
    }
    while index + 4 <= len {
        let source = src.add(index);
        let target = dst.add(index);
        *target = *source.add(2);
        *target.add(1) = *source.add(1);
        *target.add(2) = *source;
        *target.add(3) = 255;
        index += 4;
    }
}
