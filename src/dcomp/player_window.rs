//! The player window: our own HWND, a Slint UI layer over a DComp video layer.
//!
//! # What this replaces
//!
//! The player used to be a winit window whose picture was a Slint `Image`: the
//! whole window (chrome, comments, panels, picture) was re-tessellated and
//! presented once per refresh, which capped the picture at the window's rate and
//! spent 12-20 ms per frame doing it (`src/video_surface.rs` has the numbers).
//!
//! Here the window is an `WS_EX_NOREDIRECTIONBITMAP` HWND with one
//! `IDCompositionTarget` and two visuals:
//!
//! * the **video** layer (`ALPHA_MODE_IGNORE`), presented by
//!   [`super::video::VideoSink`] on its own thread at the pacer's rate;
//! * the **UI** layer (`ALPHA_MODE_PREMULTIPLIED`), which is this file: Slint
//!   rendered by `SoftwareRenderer` into a premultiplied buffer, uploaded into
//!   the layer's staging texture and presented on the UI thread, only when
//!   Slint says something changed.
//!
//! # WindowAdapter, and what the app already relies on
//!
//! `slint::Window` calls into [`WindowAdapter`] for everything it needs, and the
//! app's own code (`win.rs`) goes the other way: it resolves the native handle
//! through `window.window_handle()` and then speaks Win32. That is why this
//! adapter has to report a real `HWND` (see [`WindowAdapter::window_handle_06`]):
//! frameless styling, the 8 ms drag loop, minimum track size, occlusion tests,
//! fullscreen, taskbar bookkeeping and the captures all keep working through
//! the same functions they used before.
//!
//! # Input
//!
//! The window procedure translates Win32 messages into
//! `slint::platform::WindowEvent`s and dispatches them on the same window:
//! pointer press/release/move/scroll/leave, keys (named keys plus `WM_CHAR` for
//! text), focus, resize, DPI, close. Nothing is synthesized: the cursor position
//! in a message is the physical client position Windows reported, converted with
//! the window's scale factor, which is exactly what the winit backend did.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::num::NonZeroIsize;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use slint::platform::software_renderer::{PremultipliedRgbaColor, RepaintBufferType, SoftwareRenderer};
use slint::platform::{PlatformError, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalPosition, PhysicalSize, WindowSize};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::DirectComposition::{IDCompositionRectangleClip, IDCompositionVisual};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, MonitorFromWindow, ScreenToClient, UpdateWindow,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    GetWindowRect, LoadCursorW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, SetWindowTextW, ShowWindow, CS_HREDRAW, CS_VREDRAW, GWL_STYLE,
    GWLP_USERDATA, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, MINMAXINFO, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_MAXIMIZE, SW_MINIMIZE, SW_SHOW, WM_CHAR, WM_CLOSE, WM_DPICHANGED, WM_DESTROY,
    WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_NOREDIRECTIONBITMAP, WS_POPUP,
    WS_THICKFRAME,
};

use super::danmaku::DanmakuLayer;
use super::gpu::{self, Composition, Gpu, Layer};
use super::video::{FrameBuf, VideoGeometry, VideoSink};

/// `BILI_DCOMP_UI_FORCE=1`: repaint the whole UI layer on every tick (QA).
fn ui_force() -> bool {
    static FORCE: OnceLock<bool> = OnceLock::new();
    *FORCE.get_or_init(|| {
        std::env::var("BILI_DCOMP_UI_FORCE").is_ok_and(|value| {
            matches!(value.trim(), "1" | "true" | "yes" | "on")
        })
    })
}

/// `WM_MOUSELEAVE` (winuser.h; not in the `windows` metadata's names).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Cross-thread view of the video sinks, for the QA soak thread: the window map
/// below is thread-local (the window procedure runs on the window's thread), so
/// a counter read from a worker cannot use it.
static SINKS: OnceLock<Mutex<Vec<(isize, Arc<VideoSink>)>>> = OnceLock::new();

fn sinks() -> &'static Mutex<Vec<(isize, Arc<VideoSink>)>> {
    SINKS.get_or_init(|| Mutex::new(Vec::new()))
}

/// f212: the player windows that have their composition tree attached, by HWND.
///
/// This is the answer a *decoder* thread needs, and it must not go through the
/// event loop: the window is built on the UI thread during the open, long before
/// any session asks, so "is there a window for this handle" is already true when
/// the question arrives. The value is the window's own identity
/// ([`Weak::as_ptr`]): Windows reuses HWND values, and an old window's teardown
/// must never unhook the window that inherited its handle.
static ATTACHED: OnceLock<Mutex<Vec<(isize, usize)>>> = OnceLock::new();

fn attached_list() -> &'static Mutex<Vec<(isize, usize)>> {
    ATTACHED.get_or_init(|| Mutex::new(Vec::new()))
}

/// Whether `hwnd` is a player window whose composition tree is attached (any
/// thread, no event loop involved).
pub(crate) fn attached(hwnd: isize) -> bool {
    hwnd != 0
        && attached_list()
            .lock()
            .map(|list| list.iter().any(|(handle, _)| *handle == hwnd))
            .unwrap_or(false)
}

/// The UI layer's counters, also readable from another thread.
static UI_HWND: AtomicIsize = AtomicIsize::new(0);
static UI_PAINTS: AtomicU64 = AtomicU64::new(0);
/// Centi-milliseconds (an `AtomicU64` of f32 bits would be less obvious).
static UI_PAINT_MS: AtomicU64 = AtomicU64::new(0);

/// f212: the UI thread's last paint-timer tick, in ms since [`TICK_EPOCH`].
///
/// The 4 ms timer is the UI thread's heartbeat: it runs whether or not anything
/// is dirty, so a gap means the event loop itself was blocked (a synchronous
/// call on the main thread), not that nothing needed repainting. `BILI_UI_STALL=1`
/// starts the watchdog below, which reports every gap over 250 ms — the
/// instrument that answers "why did a window take 900 ms to answer a prime".
static TICK_AT: AtomicU64 = AtomicU64::new(0);
static TICK_EPOCH: OnceLock<std::time::Instant> = OnceLock::new();

fn tick_now_ms() -> u64 {
    TICK_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

fn touch_ui_tick() {
    TICK_AT.store(tick_now_ms(), Ordering::Relaxed);
}

/// `BILI_UI_STALL=1`: one line per UI-thread stall (`ui stall ms=…`).
fn start_stall_watchdog() {
    use std::sync::Once;
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        if std::env::var("BILI_UI_STALL").is_err() {
            return;
        }
        std::thread::spawn(|| {
            let mut reported = 0u64;
            let mut next_at = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
                let last = TICK_AT.load(Ordering::Relaxed);
                if last == 0 {
                    continue;
                }
                let gap = tick_now_ms().saturating_sub(last);
                // One line per stall, then a heartbeat every second while it
                // lasts: "the UI thread stopped for 27 s" and "stopped for
                // 250 ms" are different bugs.
                let due = reported != last || gap > next_at;
                if gap >= 250 && due {
                    reported = last;
                    next_at = gap + 1_000;
                    crate::debug_log(&format!("ui stall ms={gap}"));
                }
            }
        });
    });
}

/// Window class of the player window (one per process).
const CLASS_NAME: &str = "O11A1DcompPlayer";
/// The client area a player window starts at, before Slint sets its size.
const INITIAL_WIDTH: i32 = 960;
const INITIAL_HEIGHT: i32 = 540;
/// UI-layer repaint clock. Slint decides whether anything is dirty; this only
/// bounds how long a dirty window can wait. 4 ms is a quarter of a 120 Hz frame:
/// the two layers presented from this thread (the Slint UI and the comment
/// overlay) each want a 60 Hz cadence, and at 8 ms the two 16.7 ms schedules
/// beat against the tick and one paint in eight was coalesced into the next
/// (measured: 52-56 comment paints/s). At 4 ms the same paint is serviced
/// within a quarter frame and both layers hold their full 60 Hz.
const UI_TICK_MS: u64 = 4;

thread_local! {
    /// `HWND` -> the adapter that owns it. The window procedure runs on the
    /// thread that created the window, so a thread-local map needs no locking.
    static WINDOWS: RefCell<HashMap<isize, Weak<DcompPlayerWindow>>> =
        RefCell::new(HashMap::new());
    static CLASS: Cell<bool> = const { Cell::new(false) };
}

/// One player window's compositor objects. All of them live on the UI thread;
/// the video layer's device/swapchain were moved to the sink at creation.
#[derive(Default)]
struct Compositor {
    gpu: Option<Gpu>,
    ui: Option<Layer>,
    composition: Option<Composition>,
    ui_visual: Option<IDCompositionVisual>,
    video_visual: Option<IDCompositionVisual>,
    clip: Option<IDCompositionRectangleClip>,
    /// Geometry last applied to the video visual (physical client px).
    geometry: Option<VideoGeometry>,
}

impl Compositor {
    fn apply_geometry(&mut self, geometry: VideoGeometry) {
        if self.geometry == Some(geometry) {
            return;
        }
        self.geometry = Some(geometry);
        let (Some(visual), Some(device)) = (&self.video_visual, self.composition.as_ref()) else {
            return;
        };
        unsafe {
            // The transform is what makes the swapchain fit at all: a
            // composition swapchain is displayed at its own pixel size, so
            // without it the picture is drawn 1:1 at the client origin and the
            // clip below only crops it (`video larger than window`). Offset and
            // transform are in client-area pixels, which is also the space
            // `geometry` is in.
            let matrix = windows_numerics::Matrix3x2 {
                M11: geometry.scale_x,
                M12: 0.0,
                M21: 0.0,
                M22: geometry.scale_y,
                M31: 0.0,
                M32: 0.0,
            };
            let _ = visual.SetOffsetX2(geometry.x);
            let _ = visual.SetOffsetY2(geometry.y);
            let _ = visual.SetTransform2(&matrix);
            if let Some(clip) = &self.clip {
                // The clip is applied *before* the offset/transform, so it is
                // expressed in the swapchain's own pixel space. Mapping the
                // band back through the transform puts the visible edge exactly
                // on the band. (Setting the band here directly was the second
                // half of the bug: those values were read as content pixels.)
                let (bx, by, bw, bh) = geometry.clip;
                let sx = geometry.scale_x.max(1e-6);
                let sy = geometry.scale_y.max(1e-6);
                let _ = clip.SetLeft2((bx - geometry.x) / sx);
                let _ = clip.SetTop2((by - geometry.y) / sy);
                let _ = clip.SetRight2((bx + bw - geometry.x) / sx);
                let _ = clip.SetBottom2((by + bh - geometry.y) / sy);
            }
            let _ = device.device.Commit();
        }
    }
}

/// The player window.
pub(crate) struct DcompPlayerWindow {
    /// The Slint side. Created with a weak reference to *this* adapter (not to
    /// an inner `MinimalSoftwareWindow`): every `WindowAdapter` call has to land
    /// here, because this is the type that owns the HWND.
    window: slint::Window,
    renderer: SoftwareRenderer,
    needs_redraw: Cell<bool>,
    me: RefCell<Weak<Self>>,
    hwnd: Cell<isize>,
    compositor: RefCell<Compositor>,
    buffer: RefCell<Vec<PremultipliedRgbaColor>>,
    size: Cell<PhysicalSize>,
    scale: Cell<f32>,
    visible: Cell<bool>,
    fullscreen: Cell<bool>,
    windowed_rect: Cell<Option<(i32, i32, i32, i32)>>,
    windowed_style: Cell<i32>,
    min_size: Cell<(i32, i32)>,
    max_size: Cell<(i32, i32)>,
    /// The very next paint must cover the whole buffer (first frame, or any
    /// resize, where the damage tracker has no history for the new buffer).
    full_paint: Cell<bool>,
    timer: slint::Timer,
    video: RefCell<Option<std::sync::Arc<VideoSink>>>,
    /// f196: 弹幕 on their own composition layer, over the picture only.
    danmaku: DanmakuLayer,
    /// Last layout published by the player, so `WM_SIZE` / `WM_DPICHANGED` can
    /// recompute the geometry against the *new* client rect instead of waiting
    /// for the next UI tick.
    layout: Cell<Option<VideoLayout>>,
    /// The frame size the video swapchain was created with.
    video_frame: Cell<(u32, u32)>,
    /// QA: the last UI render's duration, in milliseconds.
    paint_ms: Cell<f32>,
    paints: Cell<u64>,
}

/// What the player publishes about the picture slot, logical px.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VideoLayout {
    pub(crate) band: (f32, f32, f32, f32),
    pub(crate) scale: f32,
    pub(crate) fit: u8,
    pub(crate) frame: (u32, u32),
}

impl DcompPlayerWindow {
    /// Build the window, its composition tree and its video layer.
    ///
    /// Called from `Platform::create_window_adapter` while the player component
    /// is being constructed. Any failure is reported as an error so the caller
    /// can fall back to winit.
    pub(crate) fn create() -> Result<Rc<Self>, PlatformError> {
        register_class().map_err(PlatformError::Other)?;
        let hwnd =
            create_hwnd(INITIAL_WIDTH, INITIAL_HEIGHT).map_err(PlatformError::Other)?;
        let scale = unsafe {
            let dpi = GetDpiForWindow(HWND(hwnd as *mut core::ffi::c_void));
            if dpi == 0 {
                1.0
            } else {
                dpi as f32 / 96.0
            }
        };
        let window = Rc::new_cyclic(|me: &Weak<Self>| Self {
            window: slint::Window::new(me.clone() as Weak<dyn slint::platform::WindowAdapter>),
            renderer: SoftwareRenderer::new_with_repaint_buffer_type(
                RepaintBufferType::ReusedBuffer,
            ),
            needs_redraw: Cell::new(true),
            me: RefCell::new(me.clone()),
            hwnd: Cell::new(hwnd),
            compositor: RefCell::new(Compositor::default()),
            buffer: RefCell::new(Vec::new()),
            size: Cell::new(PhysicalSize::new(INITIAL_WIDTH as u32, INITIAL_HEIGHT as u32)),
            scale: Cell::new(scale),
            visible: Cell::new(false),
            fullscreen: Cell::new(false),
            windowed_rect: Cell::new(None),
            windowed_style: Cell::new(0),
            min_size: Cell::new((0, 0)),
            max_size: Cell::new((0, 0)),
            full_paint: Cell::new(true),
            timer: slint::Timer::default(),
            video: RefCell::new(None),
            danmaku: DanmakuLayer::new(),
            layout: Cell::new(None),
            video_frame: Cell::new((0, 0)),
            paint_ms: Cell::new(0.0),
            paints: Cell::new(0),
        });

        if let Err(error) = window.build_compositor() {
            crate::debug_log(&format!("dcomp player compositor failed: {error}"));
            let _ = unsafe { DestroyWindow(HWND(hwnd as *mut core::ffi::c_void)) };
            return Err(PlatformError::Other(format!("dcomp player: {error}")));
        }
        WINDOWS.with(|map| {
            if let Ok(mut map) = map.try_borrow_mut() {
                map.insert(hwnd, Rc::downgrade(&window));
            }
        });
        // f212: the composition tree exists from here on, so any thread may ask
        // this window whether it can take a video layer without waiting for an
        // event-loop round trip.
        let window_id = window.me.borrow().as_ptr() as usize;
        if let Ok(mut list) = attached_list().lock() {
            list.retain(|(handle, id)| *handle != hwnd || *id != window_id);
            list.push((hwnd, window_id));
        }
        UI_HWND.store(hwnd, Ordering::Relaxed);
        start_stall_watchdog();
        DanmakuLayer::register(hwnd);
        unsafe {
            // Diagnostic only: lets a debugger/`BILI_*` probe find the adapter
            // from the handle. The wndproc itself uses the thread-local map.
            let _ = SetWindowLongPtrW(
                HWND(hwnd as *mut core::ffi::c_void),
                GWLP_USERDATA,
                window.as_ref() as *const Self as isize,
            );
        }
        // Seed the size Slint will see before any property is written.
        window
            .window
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        window.size.set(PhysicalSize::new(
            INITIAL_WIDTH as u32,
            INITIAL_HEIGHT as u32,
        ));
        window.window.dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(INITIAL_WIDTH as f32 / scale, INITIAL_HEIGHT as f32 / scale),
        });
        // Which monitor the window is on, what Windows reports as its DPI, and
        // what the monitor's real mode is. A DirectComposition target's DIPs are
        // physical pixels divided by the *real* monitor DPI, so a mismatch with
        // `GetDpiForWindow` (which a DPI-unaware process gets virtualised) is
        // exactly what makes a composition layer come out at the wrong size.
        {
            let monitor = unsafe {
                MonitorFromWindow(
                    HWND(hwnd as *mut core::ffi::c_void),
                    MONITOR_DEFAULTTONEAREST,
                )
            };
            let mut info: windows::Win32::Graphics::Gdi::MONITORINFOEXW =
                unsafe { std::mem::zeroed() };
            info.monitorInfo.cbSize =
                std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFOEXW>() as u32;
            let mut real = (0u32, 0u32, 0u16);
            let mut name = String::new();
            if unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) }.as_bool()
            {
                name = info
                    .szDevice
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
                    .collect();
                let mut mode: windows::Win32::Graphics::Gdi::DEVMODEW =
                    unsafe { std::mem::zeroed() };
                mode.dmSize = std::mem::size_of::<windows::Win32::Graphics::Gdi::DEVMODEW>() as u16;
                let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                if unsafe {
                    windows::Win32::Graphics::Gdi::EnumDisplaySettingsW(
                        PCWSTR(wide.as_ptr()),
                        windows::Win32::Graphics::Gdi::ENUM_CURRENT_SETTINGS,
                        &mut mode,
                    )
                }
                .as_bool()
                {
                    real = (mode.dmPelsWidth, mode.dmPelsHeight, mode.dmLogPixels);
                }
            }
            crate::debug_log(&format!(
                "dcomp monitor name={name} rect={}x{} dpi={:.0} mode={}x{} logpix={}",
                info.monitorInfo.rcMonitor.right - info.monitorInfo.rcMonitor.left,
                info.monitorInfo.rcMonitor.bottom - info.monitorInfo.rcMonitor.top,
                unsafe { GetDpiForWindow(HWND(hwnd as *mut core::ffi::c_void)) },
                real.0,
                real.1,
                real.2
            ));
        }
        crate::debug_log(&format!(
            "dcomp player window hwnd={hwnd} size={INITIAL_WIDTH}x{INITIAL_HEIGHT} scale={scale:.2}"
        ));
        Ok(window)
    }

    /// Create the UI device/layer and the composition tree.
    ///
    /// The video layer is NOT built here: the decoder's frame size is only known
    /// once the first frame exists (the engine asks for 1080p, and 4K streams
    /// exist), and a swapchain is fixed at the size it was created with. The
    /// layer is therefore created on the first frame
    /// ([`Self::ensure_video_layer`]) and inserted at the bottom of the tree.
    fn build_compositor(&self) -> Result<(), String> {
        let size = self.size.get();
        let ui_gpu = Gpu::new().map_err(|error| format!("ui device: {error}"))?;
        let ui_layer = Layer::new(&ui_gpu, size.width, size.height, gpu::UI)
            .map_err(|error| format!("ui layer: {error}"))?;
        let (composition, mut visuals) = Composition::new(self.hwnd.get(), &ui_gpu, &[&ui_layer])
            .map_err(|error| format!("composition: {error}"))?;
        let ui_visual = visuals.pop();
        let mut compositor = self.compositor.borrow_mut();
        compositor.gpu = Some(ui_gpu);
        compositor.ui = Some(ui_layer);
        compositor.composition = Some(composition);
        compositor.ui_visual = ui_visual;
        Ok(())
    }

    /// Build the video layer at `frame`'s size, put it under the UI visual and
    /// start its presenting thread. Idempotent for the same size.
    fn ensure_video_layer(&self, frame: (u32, u32)) -> Result<(), String> {
        let frame = (frame.0.max(1), frame.1.max(1));
        if self.video.borrow().is_some() && self.video_frame.get() == frame {
            return Ok(());
        }
        // Only reachable if a stream changes resolution mid-session: stop the
        // old thread and rebuild. Rare enough to pay for a fresh device.
        if let Some(old) = self.video.borrow_mut().take() {
            old.close();
        }
        let video_gpu = Gpu::new().map_err(|error| format!("video device: {error}"))?;
        let video_layer = Layer::new(&video_gpu, frame.0, frame.1, gpu::VIDEO)
            .map_err(|error| format!("video layer: {error}"))?;
        let mut compositor = self.compositor.borrow_mut();
        let ui_visual = compositor.ui_visual.clone();
        let Some(composition) = compositor.composition.as_mut() else {
            return Err("composition missing".into());
        };
        // f196: order the visual tree explicitly. A NULL reference visual does
        // *not* mean "the bottom" in practice (measured: the video ended up
        // ABOVE the UI visual, so every UI pixel inside the picture rect - the
        // danmaku above all - was hidden behind the picture). Inserting directly
        // under the UI visual by reference is unambiguous.
        let visual = unsafe {
            let visual = composition
                .device
                .CreateVisual()
                .map_err(|error| format!("video visual: {error}"))?;
            let unknown: windows::core::IUnknown = video_layer
                .swap
                .cast()
                .map_err(|error| format!("video content: {error}"))?;
            visual
                .SetContent(&unknown)
                .map_err(|error| format!("video content: {error}"))?;
            composition
                .root
                .AddVisual(&visual, false, ui_visual.as_ref())
                .map_err(|error| format!("video visual: {error}"))?;
            visual
        };
        // The danmaku layer (if it was built before the first frame) sits
        // between the video and the UI: re-insert it above the new video visual
        // so the two never swap places.
        if let Some(surface) = self.danmaku.surface_ref() {
            unsafe {
                let _ = composition.root.RemoveVisual(&surface);
                let _ = composition.root.AddVisual(&surface, true, Some(&visual));
            }
        }
        let clip = unsafe { composition.device.CreateRectangleClip().ok() };
        if let Some(clip) = &clip {
            unsafe {
                let _ = visual.SetClip(clip);
            }
        }
        let _ = unsafe { composition.device.Commit() };
        compositor.video_visual = Some(visual);
        compositor.clip = clip;
        compositor.geometry = None;
        drop(compositor);
        self.video_frame.set(frame);
        let sink = VideoSink::start(
            video_gpu,
            video_layer,
            crate::player::active_control(),
        );
        if let Ok(mut list) = sinks().lock() {
            list.retain(|(hwnd, _)| *hwnd != self.hwnd.get());
            list.push((self.hwnd.get(), sink.clone()));
        }
        *self.video.borrow_mut() = Some(sink);
        crate::debug_log(&format!(
            "dcomp video layer frame={}x{}",
            frame.0, frame.1
        ));
        Ok(())
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd.get() as *mut core::ffi::c_void)
    }

    /// Render the UI if Slint says something changed and present the layer.
    ///
    /// Returns whether a paint happened. `MinimalSoftwareWindow::draw_if_needed`
    /// is the dirty-region gate: a 1080p full repaint costs ~8 ms, a dirty one
    /// ~3 ms (see `Cargo.toml`'s renderer table and the f192 probe).
    fn paint(&self) -> bool {
        if !self.visible.get() || self.hwnd.get() == 0 {
            return false;
        }
        // f209: the picture can change size *after* the layer was built - the
        // MF path primes the layer and the decoder then reports its own size -
        // and the visual's transform must follow it. `sync_layout` early-outs
        // when the geometry is unchanged, so this is a comparison per paint.
        if self.video.borrow().is_some() {
            if let (Some(layout), Some(control)) =
                (self.layout.get(), crate::player::active_control())
            {
                let frame = (control.frame_width(), control.frame_height());
                if frame.0 > 0 && frame.1 > 0 && layout.frame != frame {
                    self.sync_layout(VideoLayout { frame, ..layout });
                }
            }
        }
        let size = self.size.get();
        if size.width == 0 || size.height == 0 {
            return false;
        }
        // f196: the comments move on their own clock and their own layer, so
        // they are presented first and unconditionally; the UI layer below only
        // runs when Slint says the chrome changed.
        self.present_danmaku();
        // f200 QA only (`BILI_DCOMP_UI_FORCE=1`): force a FULL UI repaint on
        // every tick. The layer normally presents only when Slint reports a
        // change (0/s on an idle paused frame), so this is the only way to read
        // its ceiling from the same counters. Inert otherwise.
        if ui_force() {
            self.full_paint.set(true);
            self.needs_redraw.set(true);
        }
        let started = std::time::Instant::now();
        let width = size.width as usize;
        let height = size.height as usize;
        // A resize invalidates the whole buffer: the damage tracker's history
        // describes the old one.
        let mut full = self.full_paint.get();
        let mut damaged = (0usize, 0usize, width, height);
        {
            let mut buffer = self.buffer.borrow_mut();
            if buffer.len() != width * height {
                buffer.clear();
                buffer.resize(width * height, PremultipliedRgbaColor::default());
                full = true;
            }
            // The dirty-region gate: Slint marks the window from `request_redraw`
            // (property changes, animations, timers), and a resize forces one
            // full pass because the damage tracker has no history for the new
            // buffer.
            if !full && !self.needs_redraw.replace(false) {
                return false;
            }
            let buffer = &mut *buffer;
            if full {
                self.renderer
                    .set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            }
            let region = self.renderer.render(buffer, width);
            if full {
                self.renderer
                    .set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
            }
            if !full {
                // Only the damaged rectangle is uploaded: a mostly-static
                // 1080p UI then costs a few hundred KB per repaint instead of
                // the whole 8 MB buffer.
                let origin = region.bounding_box_origin();
                let damage = region.bounding_box_size();
                if damage.width == 0 || damage.height == 0 {
                    self.full_paint.set(false);
                    return false;
                }
                let x = (origin.x.max(0) as usize).min(width - 1);
                let y = (origin.y.max(0) as usize).min(height - 1);
                let w = (damage.width as usize).min(width - x);
                let h = (damage.height as usize).min(height - y);
                damaged = (x, y, w, h);
            }
            self.full_paint.set(false);
        }

        let mut compositor = self.compositor.borrow_mut();
        let Compositor { gpu, ui, .. } = &mut *compositor;
        let (Some(gpu), Some(ui)) = (gpu.as_ref(), ui.as_mut()) else {
            return false;
        };
        if ui.resize(gpu, size.width, size.height).unwrap_or(false) {
            // New buffers: the next paint must be a full one.
            self.full_paint.set(true);
        }
        let buffer = self.buffer.borrow();
        let bytes = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr() as *const u8,
                buffer.len() * 4,
            )
        };
        // Developer hook (`BILI_DCOMP_DUMP=<path>`): the UI layer's own buffer,
        // exactly what the swapchain is being fed. Invaluable when the composite
        // does not look like the buffer.
        if let Ok(path) = std::env::var("BILI_DCOMP_DUMP") {
            if self.paints.get() % 60 == 5 {
                let mut rgba = bytes.to_vec();
                for pixel in rgba.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                if let Some(image) = image::RgbaImage::from_raw(size.width, size.height, rgba) {
                    let _ = image.save(&path);
                    crate::debug_log(&format!("dcomp ui dump {path}"));
                }
            }
        }
        let render_ms = started.elapsed().as_secs_f32() * 1000.0;
        let staged = if full {
            ui.stage(bytes, true).map(|_| ())
        } else {
            let (x, y, w, h) = damaged;
            ui.stage_rect(bytes, width, x as u32, y as u32, w as u32, h as u32)
        };
        drop(buffer);
        match staged {
            Ok(()) => {}
            Err(error) => {
                crate::debug_log(&format!("dcomp ui stage failed: {error}"));
                return false;
            }
        }
        if let Err(error) = ui.pulse() {
            crate::debug_log(&format!("dcomp ui present failed: {error}"));
            return false;
        }
        self.paint_ms
            .set(started.elapsed().as_secs_f32() * 1000.0);
        let paints = self.paints.get() + 1;
        self.paints.set(paints);
        UI_PAINTS.store(paints, Ordering::Relaxed);
        UI_PAINT_MS.store(
            (self.paint_ms.get() * 100.0) as u64,
            Ordering::Relaxed,
        );
        if paints <= 5 || paints % 120 == 0 {
            crate::debug_log(&format!(
                "dcomp ui paint n={paints} size={}x{} scale={:.2} render_ms={render_ms:.2}                  damage={}x{} at {},{} ms={:.2}",
                size.width,
                size.height,
                self.scale.get(),
                damaged.2,
                damaged.3,
                damaged.0,
                damaged.1,
                self.paint_ms.get()
            ));
        }
        true
    }

    /// Present the danmaku layer, sized and placed on the picture rect.
    ///
    /// The rect is [`VideoGeometry::visible`]: the drawn picture intersected
    /// with the band. For 适配 that is the letterboxed picture, for 填充/拉伸
    /// the band itself - in every case exactly the pixels the video covers, so
    /// a comment cannot land on a letterbox bar or outside the window.
    fn present_danmaku(&self) {
        if !super::danmaku::layer_enabled() {
            return;
        }
        let Some(geometry) = self.compositor.borrow().geometry else {
            return;
        };
        let (x, y, width, height) = geometry.visible();
        if width < 2.0 || height < 2.0 {
            return;
        }
        let left = x.round();
        let top = y.round();
        let rect = (
            left,
            top,
            ((x + width).round() - left).max(1.0) as u32,
            ((y + height).round() - top).max(1.0) as u32,
        );
        let mut compositor = self.compositor.borrow_mut();
        let Compositor {
            gpu,
            composition,
            ui_visual,
            ..
        } = &mut *compositor;
        let (Some(gpu), Some(composition)) = (gpu.as_ref(), composition.as_ref()) else {
            return;
        };
        self.danmaku
            .present(gpu, composition, ui_visual.as_ref(), rect);
    }

    /// Start the UI clock (idempotent).
    fn start_timer(&self) {
        if self.timer.running() {
            return;
        }
        let weak = self.me.borrow().clone();
        self.timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(UI_TICK_MS),
            move || {
                touch_ui_tick();
                if let Some(window) = weak.upgrade() {
                    window.paint();
                }
            },
        );
    }

    /// Apply a published picture slot to the video visual.
    ///
    /// The band arrives in logical px from Slint. It is clamped to the current
    /// client rect first (a resize/panel change publishes the band one tick
    /// later than `WM_SIZE`), then turned into physical pixels exactly once by
    /// [`VideoGeometry::fit`].
    fn sync_layout(&self, layout: VideoLayout) {
        self.layout.set(Some(layout));
        let client = (self.size.get().width, self.size.get().height);
        let band = super::video::clamp_band(layout.band, layout.scale, client);
        let geometry = VideoGeometry::fit(band, layout.scale, layout.frame, layout.fit);
        let before = self.compositor.borrow().geometry;
        self.compositor.borrow_mut().apply_geometry(geometry);
        if before == Some(geometry) {
            return;
        }
        let overflow = geometry.overflow(client);
        let window_px = client;
        let mut outer: RECT = unsafe { std::mem::zeroed() };
        let _ = unsafe { GetWindowRect(self.hwnd(), &mut outer) };
        let outer_px = (outer.right - outer.left, outer.bottom - outer.top);
        let (vx, vy, vw, vh) = geometry.visible();
        crate::debug_log(&format!(
            "dcomp video geom window={}x{} client={}x{} title_dx={} title_dy={} scale={:.2} fit={} frame={}x{} band={:.0}x{:.0}@({:.0},{:.0}) video_rect={:.0}x{:.0}@({:.0},{:.0}) drawn={:.0}x{:.0}@({:.0},{:.0}) xform={:.4},{:.4} clip={:.0},{:.0} {:.0}x{:.0} overflow={:.1}",
            outer_px.0,
            outer_px.1,
            window_px.0,
            window_px.1,
            outer_px.0 - window_px.0 as i32,
            outer_px.1 - window_px.1 as i32,
            layout.scale,
            layout.fit,
            layout.frame.0,
            layout.frame.1,
            band.2 * layout.scale,
            band.3 * layout.scale,
            band.0 * layout.scale,
            band.1 * layout.scale,
            vw,
            vh,
            vx,
            vy,
            geometry.width,
            geometry.height,
            geometry.x,
            geometry.y,
            geometry.scale_x,
            geometry.scale_y,
            geometry.clip.0,
            geometry.clip.1,
            geometry.clip.2,
            geometry.clip.3,
            overflow
        ));
    }

    fn set_size_native(&self, width: i32, height: i32) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }

    fn apply_fullscreen(&self, on: bool) {
        let hwnd = self.hwnd();
        if on == self.fullscreen.get() {
            return;
        }
        unsafe {
            if on {
                let mut rect: RECT = std::mem::zeroed();
                if GetWindowRect(hwnd, &mut rect).is_err() {
                    return;
                }
                self.windowed_rect
                    .set(Some((rect.left, rect.top, rect.right, rect.bottom)));
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as i32;
                self.windowed_style.set(style);
                let _ = SetWindowLongPtrW(
                    hwnd,
                    GWL_STYLE,
                    ((style as u32) & !WS_THICKFRAME.0) as isize,
                );
                let (x, y, w, h) = monitor_rect(hwnd);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    w,
                    h,
                    SWP_FRAMECHANGED | SWP_NOACTIVATE,
                );
                let _ = SetForegroundWindow(hwnd);
                self.fullscreen.set(true);
            } else {
                let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, self.windowed_style.get() as isize);
                if let Some((left, top, right, bottom)) = self.windowed_rect.get() {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_NOTOPMOST),
                        left,
                        top,
                        right - left,
                        bottom - top,
                        SWP_FRAMECHANGED | SWP_NOACTIVATE,
                    );
                }
                self.fullscreen.set(false);
            }
        }
        crate::debug_log(&format!("dcomp player fullscreen on={on}"));
    }
}

impl Drop for DcompPlayerWindow {
    fn drop(&mut self) {
        let dropped_at = std::time::Instant::now();
        self.timer.stop();
        let hwnd = self.hwnd.get();
        // f212: this window's own identity. Windows reuses HWND values, and a
        // window closed during the open of the next one can hand its handle to
        // that new window: every registry below is therefore unhooked only when
        // the entry is still *this* window, never by handle alone.
        let window_id = self.me.borrow().as_ptr() as usize;
        if hwnd != 0 {
            WINDOWS.with(|map| {
                if let Ok(mut map) = map.try_borrow_mut() {
                    if map
                        .get(&hwnd)
                        .is_some_and(|weak| weak.as_ptr() as usize == window_id)
                    {
                        map.remove(&hwnd);
                    }
                }
            });
            unsafe {
                let _ = DestroyWindow(self.hwnd());
            }
            self.hwnd.set(0);
        }
        if let Ok(mut list) = attached_list().lock() {
            list.retain(|(handle, id)| *handle != hwnd || *id != window_id);
        }
        // The video thread must stop before the composition tree that
        // references its swapchain goes away.
        let own_sink = self.video.borrow_mut().take();
        if let (Some(sink), Ok(mut list)) = (own_sink.as_ref(), sinks().lock()) {
            list.retain(|(_, entry)| !Arc::ptr_eq(entry, sink));
        }
        if UI_HWND.load(Ordering::Relaxed) == hwnd {
            UI_HWND.store(0, Ordering::Relaxed);
        }
        if let Some(sink) = own_sink {
            sink.close_detached();
        }
        let sink_done = dropped_at.elapsed().as_millis();
        // Drop the tree (and with it both swapchains) on this thread.
        self.danmaku.clear();
        DanmakuLayer::unregister(hwnd);
        let mut compositor = self.compositor.borrow_mut();
        compositor.ui = None;
        compositor.ui_visual = None;
        compositor.video_visual = None;
        compositor.clip = None;
        compositor.composition = None;
        compositor.gpu = None;
        crate::debug_log(&format!(
            "dcomp player closed paints={} paint_ms={:.2} sink_ms={sink_done} total_ms={}",
            self.paints.get(),
            self.paint_ms.get(),
            dropped_at.elapsed().as_millis()
        ));
    }
}

impl WindowAdapter for DcompPlayerWindow {
    fn window(&self) -> &slint::Window {
        &self.window
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        &self.renderer
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: WindowSize) {
        let physical = size.to_physical(self.scale.get());
        if physical.width == 0 || physical.height == 0 {
            return;
        }
        self.set_size_native(physical.width as i32, physical.height as i32);
    }

    fn position(&self) -> Option<PhysicalPosition> {
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetWindowRect(self.hwnd(), &mut rect) }.is_err() {
            return None;
        }
        Some(PhysicalPosition::new(rect.left, rect.top))
    }

    fn set_position(&self, position: slint::WindowPosition) {
        let physical = position.to_physical(self.scale.get());
        unsafe {
            let _ = SetWindowPos(
                self.hwnd(),
                None,
                physical.x,
                physical.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }

    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {
        if visible == self.visible.get() {
            if visible {
                self.paint();
            }
            return Ok(());
        }
        self.visible.set(visible);
        unsafe {
            if visible {
                let _ = ShowWindow(self.hwnd(), SW_SHOW);
                let _ = UpdateWindow(self.hwnd());
            } else {
                let _ = ShowWindow(self.hwnd(), SW_HIDE);
            }
        }
        if visible {
            self.full_paint.set(true);
            self.start_timer();
            self.paint();
        } else {
            self.timer.stop();
        }
        crate::debug_log(&format!("dcomp player visible={visible}"));
        Ok(())
    }

    fn request_redraw(&self) {
        self.needs_redraw.set(true);
    }

    fn update_window_properties(&self, properties: slint::platform::WindowProperties<'_>) {
        let title = properties.title();
        if !title.is_empty() {
            let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                let _ = SetWindowTextW(self.hwnd(), PCWSTR(wide.as_ptr()));
            }
        }
        let constraints = properties.layout_constraints();
        let scale = self.scale.get();
        if let Some(min) = constraints.min {
            if min.width > 0.0 && min.height > 0.0 {
                self.min_size.set((
                    (min.width * scale).round() as i32,
                    (min.height * scale).round() as i32,
                ));
            }
        }
        if let Some(max) = constraints.max {
            if max.width.is_finite() && max.height.is_finite() {
                self.max_size.set((
                    (max.width * scale).round() as i32,
                    (max.height * scale).round() as i32,
                ));
            }
        }
        if properties.is_fullscreen() {
            self.apply_fullscreen(true);
        } else {
            self.apply_fullscreen(false);
        }
        if properties.is_minimized() {
            unsafe {
                let _ = ShowWindow(self.hwnd(), SW_MINIMIZE);
            }
        } else if properties.is_maximized() {
            unsafe {
                let _ = ShowWindow(self.hwnd(), SW_MAXIMIZE);
            }
        }
    }

    fn window_handle_06(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let hwnd = self.hwnd.get();
        let handle = raw_window_handle::Win32WindowHandle::new(
            NonZeroIsize::new(hwnd).ok_or(raw_window_handle::HandleError::Unavailable)?,
        );
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(
                handle,
            ))
        })
    }
}

// ---- Win32 plumbing --------------------------------------------------------

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn register_class() -> Result<(), String> {
    if CLASS.with(|flag| flag.get()) {
        return Ok(());
    }
    let class = wide(CLASS_NAME);
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(|error| error.to_string())?;
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance.into(),
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class.as_ptr()),
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        return Err(format!("RegisterClassW failed: {}", windows::core::Error::from_thread()));
    }
    CLASS.with(|flag| flag.set(true));
    Ok(())
}

fn create_hwnd(width: i32, height: i32) -> Result<isize, String> {
    let class = wide(CLASS_NAME);
    let title = wide("O11A1");
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    // The window must be per-monitor DPI aware: DirectComposition composites a
    // visual tree in the target's *client pixels* (the UI layer's swapchain is
    // built at `GetClientRect`'s physical size and shown 1:1), so a DPI-unaware
    // HWND would be bitmap-stretched by DWM and the video layer would land at
    // 96/realDpi of the rect the UI reserved for it.
    let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            // No WS_VISIBLE: the component's `show()` decides. WS_THICKFRAME is
            // added by `win::normalise_frameless` (the app's own frameless
            // pass), which is what gives the window a resize-capable shell and
            // a DWM shadow while WM_NCCALCSIZE keeps the client area whole.
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
    };
    unsafe {
        let _ = SetThreadDpiAwarenessContext(previous);
    }
    let hwnd = hwnd.map_err(|error| format!("CreateWindowExW failed: {error}"))?;
    Ok(hwnd.0 as isize)
}

/// The monitor's full rect (physical px) for `hwnd`.
fn monitor_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.0.is_null() {
            return (0, 0, 1920, 1080);
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return (0, 0, 1920, 1080);
        }
        let rect = info.rcMonitor;
        (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    }
}

fn window_for(hwnd: HWND) -> Option<Rc<DcompPlayerWindow>> {
    WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .and_then(|map| map.get(&(hwnd.0 as isize)).and_then(|weak| weak.upgrade()))
    })
}

fn loword_signed(value: isize) -> i32 {
    (value as u16 as i16) as i32
}

fn hiword_signed(value: isize) -> i32 {
    ((value >> 16) as u16 as i16) as i32
}

fn named_key(vk: u32) -> Option<slint::platform::Key> {
    use slint::platform::Key;
    Some(match vk {
        0x20 => Key::Space,
        0x1B => Key::Escape,
        0x0D => Key::Return,
        0x25 => Key::LeftArrow,
        0x26 => Key::UpArrow,
        0x27 => Key::RightArrow,
        0x28 => Key::DownArrow,
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x2E => Key::Delete,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        _ => return None,
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(window) = window_for(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let scale = window.scale.get();
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            unsafe {
                let _ = BeginPaint(hwnd, &mut paint);
            }
            window.paint();
            unsafe {
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let mut client: RECT = unsafe { std::mem::zeroed() };
            if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
                let width = (client.right - client.left).max(0) as u32;
                let height = (client.bottom - client.top).max(0) as u32;
                if width > 0 && height > 0 {
                    let size = PhysicalSize::new(width, height);
                    if window.size.get() != size {
                        window.size.set(size);
                        window.full_paint.set(true);
                        window
                            .window
                            .dispatch_event(WindowEvent::Resized { size: size.to_logical(window.scale.get()) });
                        // The band in Slint is re-laid out on the next tick, but
                        // the video layer must not spend that tick in the old
                        // slot: recompute against the new client rect now (the
                        // clamp keeps a stale band inside it).
                        if let Some(layout) = window.layout.get() {
                            window.sync_layout(layout);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xFFFF) as f32;
            if dpi > 0.0 {
                let scale = dpi / 96.0;
                window.scale.set(scale);
                window
                    .window
                    .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
                let suggested = lparam.0 as *const RECT;
                if !suggested.is_null() {
                    let rect = unsafe { *suggested };
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            rect.left,
                            rect.top,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                }
                // The scale factor changed: the band is logical and the video
                // layer is physical, so the geometry has to be rebuilt with the
                // new factor even if the client size happens to be identical.
                if let Some(layout) = window.layout.get() {
                    window.sync_layout(layout);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut track);
            }
            let x = loword_signed(lparam.0);
            let y = hiword_signed(lparam.0);
            window
                .window
                .dispatch_event(WindowEvent::PointerMoved {
                    position: LogicalPosition::new(x as f32 / scale, y as f32 / scale),
                });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            window
                .window
                .dispatch_event(WindowEvent::PointerExited);
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP => {
            use slint::platform::PointerEventButton;
            let x = loword_signed(lparam.0);
            let y = hiword_signed(lparam.0);
            let position = LogicalPosition::new(x as f32 / scale, y as f32 / scale);
            let button = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => PointerEventButton::Left,
                WM_RBUTTONDOWN | WM_RBUTTONUP => PointerEventButton::Right,
                _ => PointerEventButton::Middle,
            };
            let down = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
            window
                .window
                .dispatch_event(if down {
                    WindowEvent::PointerPressed { position, button }
                } else {
                    WindowEvent::PointerReleased { position, button }
                });
            if down {
                let _ = unsafe { SetForegroundWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut point = POINT {
                x: loword_signed(lparam.0),
                y: hiword_signed(lparam.0),
            };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut point);
            }
            let steps = delta as f32 / 120.0;
            window
                .window
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(
                        point.x as f32 / scale,
                        point.y as f32 / scale,
                    ),
                    delta_x: 0.0,
                    // One wheel notch is 50 logical px in the winit backend.
                    delta_y: -steps * 50.0,
                });
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let vk = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let text: Option<slint::SharedString> = named_key(vk).map(|key| key.into());
            if let Some(text) = text {
                window.window.dispatch_event(if down {
                    WindowEvent::KeyPressed { text }
                } else {
                    WindowEvent::KeyReleased { text }
                });
            }
            // Let the default handler run so Alt+F4 / system keys still work.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CHAR => {
            if let Some(ch) = char::from_u32(wparam.0 as u32) {
                if !ch.is_control() {
                    window
                        .window
                        .dispatch_event(WindowEvent::KeyPressed {
                            text: slint::SharedString::from(ch.to_string()),
                        });
                }
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            window
                .window
                .dispatch_event(WindowEvent::WindowActiveChanged(true));
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            window
                .window
                .dispatch_event(WindowEvent::WindowActiveChanged(false));
            LRESULT(0)
        }
        WM_SETCURSOR => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        WM_GETMINMAXINFO => {
            let info = lparam.0 as *mut MINMAXINFO;
            if !info.is_null() {
                let (min_w, min_h) = window.min_size.get();
                let (max_w, max_h) = window.max_size.get();
                unsafe {
                    if min_w > 0 && min_h > 0 {
                        (*info).ptMinTrackSize.x = min_w;
                        (*info).ptMinTrackSize.y = min_h;
                    }
                    if max_w > 0 && max_h > 0 {
                        (*info).ptMaxTrackSize.x = max_w;
                        (*info).ptMaxTrackSize.y = max_h;
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Slint decides: the close-requested callback may keep the window.
            // `request_close` hides the window through `set_visible(false)`,
            // which is where the timer stops.
            window
                .window
                .dispatch_event(WindowEvent::CloseRequested);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        WM_NCDESTROY => {
            WINDOWS.with(|map| {
                if let Ok(mut map) = map.try_borrow_mut() {
                    map.remove(&(hwnd.0 as isize));
                }
            });
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Public hooks used by `player.rs` (which must not know about the adapter).

/// Whether `hwnd` is a player window built by this module.
pub(crate) fn is_player_window(hwnd: isize) -> bool {
    WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .is_some_and(|map| map.contains_key(&hwnd))
    })
}

/// Whether this window has a video layer to take frames.
pub(crate) fn accepts_video(hwnd: isize) -> bool {
    WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .is_some_and(|map| map.contains_key(&hwnd))
    })
}

/// Hand one frame to the video layer of `hwnd`.
///
/// `Ok(retired)` means the frame was accepted (the caller recycles the retired
/// buffers into the blend pool); `Err` hands the frame back untouched so the
/// caller can take the Slint path instead.
pub(crate) fn submit_frame(
    hwnd: isize,
    frame: FrameBuf,
    pooled: bool,
    layout: VideoLayout,
) -> Result<Vec<(FrameBuf, bool)>, (FrameBuf, bool)> {
    let Some(window) = WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .and_then(|map| map.get(&hwnd).and_then(|weak| weak.upgrade()))
    }) else {
        return Err((frame, pooled));
    };
    let frame_size = (frame.width().max(1), frame.height().max(1));
    if let Err(error) = window.ensure_video_layer(frame_size) {
        crate::debug_log(&format!("dcomp video layer failed: {error}"));
        return Err((frame, pooled));
    }
    let Some(sink) = window.video.borrow().clone() else {
        return Err((frame, pooled));
    };
    window.sync_layout(layout);
    sink.submit(frame, pooled);
    Ok(sink.take_retired())
}

/// f209: create the video layer for `hwnd` **without a frame**, so the MF
/// session can start before the first picture exists.
///
/// The ordinary path builds the layer from the first frame the pacer delivers
/// (`submit_frame`). That order is impossible when the picture never touches the
/// CPU: the decoder has to be opened on the layer's own device, and the layer
/// only exists after the UI thread has built it. So the pacer asks for the layer
/// first (with the size the stream will be decoded at) and only then opens the
/// decoder.
///
/// Returns the sink when the window has (or just got) its video layer.
pub(crate) fn prime_video_layer(
    hwnd: isize,
    frame: (u32, u32),
    layout: VideoLayout,
) -> Option<Arc<VideoSink>> {
    let window = WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .and_then(|map| map.get(&hwnd).and_then(|weak| weak.upgrade()))
    });
    let Some(window) = window else {
        crate::debug_log(&format!("dcomp prime: hwnd={hwnd} is not a live player window"));
        return None;
    };
    if let Err(error) = window.ensure_video_layer(frame) {
        crate::debug_log(&format!("dcomp prime video layer failed: {error}"));
        return None;
    }
    window.sync_layout(layout);
    sink_for(hwnd)
}

/// Keep the video visual's geometry in step with the UI even when no frames
/// arrive (a paused player whose panel opens must not leave the picture in the
/// old slot). Returns whether this window has a video layer.
pub(crate) fn sync_layout(hwnd: isize, layout: VideoLayout) -> bool {
    match WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .and_then(|map| map.get(&hwnd).and_then(|weak| weak.upgrade()))
    }) {
        Some(window) if window.video.borrow().is_some() => {
            window.sync_layout(layout);
            true
        }
        _ => false,
    }
}

/// f212: UI-layer paints of the newest player window, from any thread.
///
/// The prime path logs it before and after its posted callback so a QA log can
/// tell "the UI thread was busy" from "the event was never delivered".
pub(crate) fn ui_paints_total() -> u64 {
    UI_PAINTS.load(Ordering::Relaxed)
}

/// QA: `(paints, last_paint_ms)` of the UI layer for this window.
pub(crate) fn ui_counters(hwnd: isize) -> Option<(u64, f32)> {
    if UI_HWND.load(Ordering::Relaxed) != hwnd {
        return None;
    }
    Some((
        UI_PAINTS.load(Ordering::Relaxed),
        UI_PAINT_MS.load(Ordering::Relaxed) as f32 / 100.0,
    ))
}

/// f196: the model the danmaku motion lane must publish into, when `hwnd` is a
/// DComp player window (its comments are drawn by a layer of their own).
pub(crate) fn danmaku_model(
    hwnd: isize,
) -> Option<Rc<slint::VecModel<crate::DanmakuItem>>> {
    if !super::danmaku::layer_enabled() {
        return None;
    }
    WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .and_then(|map| map.get(&hwnd).and_then(|weak| weak.upgrade()))
            .map(|window| window.danmaku.model())
    })
}

/// f196: `(paints, last_paint_ms)` of the danmaku layer for this window.
pub(crate) fn danmaku_counters(hwnd: isize) -> Option<(u64, f32)> {
    DanmakuLayer::counters(hwnd)
}

/// f200: the danmaku layer's full counter set (paint / render / stage / rows).
pub(crate) fn danmaku_stats(hwnd: isize) -> Option<super::danmaku::DanmakuStats> {
    DanmakuLayer::stats(hwnd)
}

/// f196: 弹幕 on/off for the separate danmaku layer (no-op for other windows).
pub(crate) fn set_danmaku_visible(hwnd: isize, on: bool) {
    let Some(window) = WINDOWS.with(|map| {
        map.try_borrow()
            .ok()
            .and_then(|map| map.get(&hwnd).and_then(|weak| weak.upgrade()))
    }) else {
        return;
    };
    let composition = window.compositor.borrow();
    window.danmaku.set_visible(on, composition.composition.as_ref());
}

/// f202: present the frame already staged through the tier now selected. A
/// paused player produces no frames, so a 画质增强 change needs this to be
/// visible. Returns whether the window had a video layer.
pub(crate) fn repaint(hwnd: isize) -> bool {
    match sink_for(hwnd) {
        Some(sink) => {
            sink.repaint();
            true
        }
        None => false,
    }
}

/// The video sink for `hwnd`, if that window has one (any thread).
pub(crate) fn sink_for(hwnd: isize) -> Option<Arc<VideoSink>> {
    sinks()
        .lock()
        .ok()?
        .iter()
        .find(|(handle, _)| *handle == hwnd)
        .map(|(_, sink)| sink.clone())
}

/// The video layer's presentation counters (any thread).
pub(crate) fn video_counters(hwnd: isize) -> Option<VideoCounters> {
    let sink = sinks()
        .lock()
        .ok()?
        .iter()
        .find(|(handle, _)| *handle == hwnd)
        .map(|(_, sink)| sink.clone())?;
    Some(VideoCounters {
        pushed: sink.counters.pushed.load(std::sync::atomic::Ordering::Relaxed),
        presented: sink
            .counters
            .presented
            .load(std::sync::atomic::Ordering::Relaxed),
        coalesced: sink
            .counters
            .coalesced
            .load(std::sync::atomic::Ordering::Relaxed),
        stage_us: sink
            .counters
            .stage_us
            .load(std::sync::atomic::Ordering::Relaxed),
        warped: sink
            .counters
            .warped
            .load(std::sync::atomic::Ordering::Relaxed),
        warp_us: sink
            .counters
            .warp_us
            .load(std::sync::atomic::Ordering::Relaxed),
        gate_us: sink
            .counters
            .gate_us
            .load(std::sync::atomic::Ordering::Relaxed),
        enhance_frames: sink
            .counters
            .enhance_frames
            .load(std::sync::atomic::Ordering::Relaxed),
        enhance_us: sink
            .counters
            .enhance_us
            .load(std::sync::atomic::Ordering::Relaxed),
        enhance_mode: sink
            .counters
            .enhance_mode
            .load(std::sync::atomic::Ordering::Relaxed),
        mf_decoded: sink
            .counters
            .mf_decoded
            .load(std::sync::atomic::Ordering::Relaxed),
        mf_ticks: sink
            .counters
            .mf_ticks
            .load(std::sync::atomic::Ordering::Relaxed),
        mf_dropped: sink
            .counters
            .mf_dropped
            .load(std::sync::atomic::Ordering::Relaxed),
        upload_bytes: sink
            .counters
            .upload_bytes
            .load(std::sync::atomic::Ordering::Relaxed),
        mf_decode_us: sink
            .counters
            .mf_decode_us
            .load(std::sync::atomic::Ordering::Relaxed),
        mf_pre_us: sink
            .counters
            .mf_pre_us
            .load(std::sync::atomic::Ordering::Relaxed),
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VideoCounters {
    pub(crate) pushed: u64,
    pub(crate) presented: u64,
    pub(crate) coalesced: u64,
    pub(crate) stage_us: u64,
    /// f200: frames presented by the GPU warp, and what one cost the thread.
    pub(crate) warped: u64,
    pub(crate) warp_us: u64,
    pub(crate) gate_us: u64,
    /// f202: frames the 画质增强 chain drew, the GPU cost of the last one (µs,
    /// `u64::MAX` before the first timestamp query lands) and the mode it ran.
    pub(crate) enhance_frames: u64,
    pub(crate) enhance_us: u64,
    pub(crate) enhance_mode: u64,
    /// f209: pictures the MF decoder produced as textures, display slots drawn
    /// from them, pictures dropped to keep up, and bytes the CPU moved into the
    /// GPU for them (the zero-copy claim: 0).
    pub(crate) mf_decoded: u64,
    pub(crate) mf_ticks: u64,
    pub(crate) mf_dropped: u64,
    pub(crate) upload_bytes: u64,
    /// f209: one `ReadSample` on the sink thread, µs.
    pub(crate) mf_decode_us: u64,
    /// f209: the decode/choose step before the draw, µs.
    pub(crate) mf_pre_us: u64,
}
