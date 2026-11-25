//! The Win32 window, its message pump and the input events the engine needs.
//!
//! The window has `WS_EX_NOREDIRECTIONBITMAP`: DWM owns no surface for it and
//! the only pixels it can show come from the DirectComposition tree built in
//! `gpu.rs`. It is created on the player thread and all messages are pumped
//! there, so there is exactly one thread touching the swapchains.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DONOTROUND, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateRectRgn, CreateRoundRectRgn,
    DeleteDC, DeleteObject, EndPaint, GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC,
    SelectObject, SetWindowRgn, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    CAPTUREBLT, DIB_RGB_COLORS, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST, SRCCOPY,
    AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, ReleaseCapture, SetActiveWindow, SetCapture, SetFocus,
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_LBUTTON, VK_SHIFT,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, IsZoomed, LoadCursorW, PeekMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, UpdateLayeredWindow, CS_DBLCLKS,
    GWLP_USERDATA, GWL_EXSTYLE, GWL_STYLE, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION,
    HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
    ICON_BIG, ICON_SMALL, IDC_ARROW, LR_DEFAULTCOLOR, MSG, NCCALCSIZE_PARAMS, PM_REMOVE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOREDRAW, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SETICON, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_THICKFRAME,
};

/// `WM_MOUSELEAVE` lives in `Win32::UI::Controls` upstream; defining it here
/// avoids pulling the whole controls feature for one message.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Player window movement; the shadow window follows it.
const WM_MOVE: u32 = 0x0003;
/// Outer shadow around the standalone player. The window itself is opaque and
/// region-clipped, so a separate click-through layered window carries the
/// soft shadow behind it.
const SHADOW_PAD: f32 = 26.0;
const SHADOW_RADIUS: f32 = 18.0;

struct ShadowSlot {
    hwnd: isize,
    hdc: isize,
    bitmap: isize,
    width: i32,
    height: i32,
    pad: i32,
}

fn shadows() -> &'static Mutex<HashMap<isize, ShadowSlot>> {
    static SHADOWS: OnceLock<Mutex<HashMap<isize, ShadowSlot>>> = OnceLock::new();
    SHADOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The app mark embedded in both runners, so the native video window never
/// falls back to the generic executable icon in the taskbar or Alt-Tab.
const APP_ICON: &[u8] = include_bytes!("../../../assets/o11a1.ico");

/// Pick the ICO entry closest to the requested square size. The bundled file
/// stores every size as a PNG, which `CreateIconFromResourceEx` decodes.
fn app_icon_image(size: u32) -> Option<&'static [u8]> {
    if APP_ICON.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([APP_ICON[4], APP_ICON[5]]) as usize;
    let mut best: Option<(u32, &'static [u8])> = None;
    for index in 0..count {
        let entry = 6 + index * 16;
        if entry + 16 > APP_ICON.len() {
            break;
        }
        let width = if APP_ICON[entry] == 0 {
            256
        } else {
            APP_ICON[entry] as u32
        };
        let height = if APP_ICON[entry + 1] == 0 {
            256
        } else {
            APP_ICON[entry + 1] as u32
        };
        let length = u32::from_le_bytes(APP_ICON[entry + 8..entry + 12].try_into().ok()?) as usize;
        let start = u32::from_le_bytes(APP_ICON[entry + 12..entry + 16].try_into().ok()?) as usize;
        if length == 0 || start + length > APP_ICON.len() {
            continue;
        }
        let distance = width.abs_diff(size) + height.abs_diff(size);
        if distance == 0 {
            return Some(&APP_ICON[start..start + length]);
        }
        if best.map(|(best, _)| distance < best).unwrap_or(true) {
            best = Some((distance, &APP_ICON[start..start + length]));
        }
    }
    best.map(|(_, image)| image)
}

unsafe fn app_icon(size: u32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    let image = app_icon_image(size)?;
    unsafe {
        CreateIconFromResourceEx(
            image,
            true,
            0x0003_0000,
            size as i32,
            size as i32,
            LR_DEFAULTCOLOR,
        )
        .ok()
    }
}

/// Attach the app mark to a top-level player window. The handles are created
/// once per process and intentionally live until process exit.
fn set_window_icon(hwnd: HWND) {
    // Store the raw handles as isize so the process-wide cache stays Send +
    // Sync. They are never destroyed and are valid for the whole process.
    static ICONS: OnceLock<(Option<isize>, Option<isize>)> = OnceLock::new();
    let icons = ICONS.get_or_init(|| unsafe {
        (
            app_icon(32).map(|icon| icon.0 as isize),
            app_icon(16).map(|icon| icon.0 as isize),
        )
    });
    unsafe {
        if let Some(icon) = icons.0 {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(icon)),
            );
        }
        if let Some(icon) = icons.1 {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(icon)),
            );
        }
    }
}

/// Cascade standalone player windows like the old native client.
static WINDOW_SEQ: AtomicUsize = AtomicUsize::new(0);

/// The Electron shell registers this AppUserModelID
/// (`app.setAppUserModelId` in `electron/main.cjs`, and the installer's appId).
/// Windows groups taskbar buttons by AppUserModelID, not by process, so the
/// sidecar engine must join the same group. Without this the standalone player
/// is a second O11A1 button next to the main window instead of folding into
/// its previews.
const APP_USER_MODEL_ID: &str = "com.o11a1.electron";

/// Join the Electron shell's taskbar group. Safe to call more than once: the
/// AppUserModelID is process-wide, so the first call wins and later ones are
/// no-ops.
pub fn join_shell_taskbar_group() {
    static JOINED: OnceLock<()> = OnceLock::new();
    JOINED.get_or_init(|| unsafe {
        let id: Vec<u16> = APP_USER_MODEL_ID
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let _ = SetCurrentProcessExplicitAppUserModelID(PCWSTR(id.as_ptr()));
    });
}

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Quit,
    /// The window became the foreground/active player. The engine mirrors a
    /// sequence number so the shell can target "the most recently focused
    /// player window" when a card is left-clicked.
    Focus,
    Key(u32),
    Char(u16),
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseLeave,
    LeftDown {
        x: i32,
        y: i32,
    },
    LeftUp,
    /// Right-button press on the picture: opens the danmaku context menu.
    RightDown {
        x: i32,
        y: i32,
    },
    /// Right-button release: completes a press-drag menu selection.
    RightUp {
        x: i32,
        y: i32,
    },
    DoubleClick {
        x: i32,
        y: i32,
    },
    Wheel(i32),
    Resize {
        width: u32,
        height: u32,
    },
    /// The user let go of a manual resize; `width`/`height` are the final
    /// client pixels. Persisted as the next player window size.
    ResizeEnded {
        width: u32,
        height: u32,
    },
    /// A non-modal window move/resize began or ended. The engine uses this to
    /// switch to the interaction fast path: only the video transform is
    /// updated, while the chrome, shadow and overlay layout settle at the end.
    Interaction {
        active: bool,
        resizing: bool,
    },
}

#[derive(Default)]
pub struct WindowEvents {
    queue: Mutex<VecDeque<InputEvent>>,
    /// A non-modal move/resize started on the player thread. Keeping the drag
    /// out of the Win32 modal loop is what lets the video keep presenting
    /// while the window moves or resizes.
    drag: Mutex<Option<DragState>>,
    /// True for the whole move/resize gesture. While set, the window proc
    /// skips the shadow upload and the rounded region update; the engine only
    /// follows with the video transform.
    interacting: AtomicBool,
    /// Whether the active gesture is a resize (the shadow is hidden and the
    /// rounded region is deferred) or a move (the cached shadow is moved).
    interaction_resizing: AtomicBool,
    /// Last time the non-modal drag applied a geometry update. Mouse moves
    /// arrive far faster than the display; coalescing them keeps the window
    /// thread from flooding DWM with `SetWindowPos` calls.
    last_drag_update: Mutex<Option<Instant>>,
    /// Current source aspect (width, height). Shift-resize locks the window
    /// client area to it, so the letterbox background disappears.
    video_aspect: Mutex<(u32, u32)>,
    /// Width currently reserved by the open sidebar lane. The video fills
    /// `client_width - inset`, so Shift-resize subtracts it from the aspect
    /// target.
    video_inset_right: Mutex<i32>,
}

#[derive(Clone, Copy)]
struct DragState {
    cursor: POINT,
    window: RECT,
    mode: DragMode,
    /// Client size and the non-client borders measured when the drag began.
    client: (i32, i32),
    border: (i32, i32),
    /// Source aspect (`width / height`) for Shift-locked resizing.
    aspect: f32,
    /// Right-hand video inset (open sidebar) in physical pixels.
    inset_right: i32,
}

#[derive(Clone, Copy, Debug)]
enum DragMode {
    Move,
    Resize(u8),
}

const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_TOP: u8 = 4;
const EDGE_BOTTOM: u8 = 8;

/// Resize border in 96-DPI units. Windows' own sizing frame is about 8 px;
/// the old 6 px band plus the 16 px rounded window region left almost no
/// reachable corner at the bottom right.
const RESIZE_EDGE_DIP: f32 = 8.0;
/// Bottom corner grip in 96-DPI units. It is larger than the 16 px window
/// corner radius, so the visible arc and the area just inside it both return
/// the diagonal resize code. The top corners stay at `edge` because the
/// 36x34 close button sits 8 px from the top-right edge.
const RESIZE_BOTTOM_CORNER_DIP: f32 = 24.0;

/// Hit-test a client point against the resize grips. Bottom corners are
/// deliberately larger than the top ones: the rounded window region clips
/// the exact corner, and the top-right one must not cover the close button.
fn resize_hit(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    edge: i32,
    bottom_corner: i32,
) -> Option<u32> {
    let edge = edge.max(1);
    let bottom_corner = bottom_corner.max(edge);
    let near_left = x < edge;
    let near_right = x >= width.saturating_sub(edge);
    let near_top = y < edge;
    let near_bottom = y >= height.saturating_sub(edge);
    let bottom_left = x < bottom_corner && y >= height.saturating_sub(bottom_corner);
    let bottom_right =
        x >= width.saturating_sub(bottom_corner) && y >= height.saturating_sub(bottom_corner);
    let hit = if bottom_left {
        HTBOTTOMLEFT
    } else if bottom_right {
        HTBOTTOMRIGHT
    } else if near_top && near_left {
        HTTOPLEFT
    } else if near_top && near_right {
        HTTOPRIGHT
    } else if near_left {
        HTLEFT
    } else if near_right {
        HTRIGHT
    } else if near_top {
        HTTOP
    } else if near_bottom {
        HTBOTTOM
    } else {
        return None;
    };
    Some(hit)
}

fn edges_for_hit(hit: u32) -> Option<u8> {
    if hit == HTLEFT as u32 {
        Some(EDGE_LEFT)
    } else if hit == HTRIGHT as u32 {
        Some(EDGE_RIGHT)
    } else if hit == HTTOP as u32 {
        Some(EDGE_TOP)
    } else if hit == HTBOTTOM as u32 {
        Some(EDGE_BOTTOM)
    } else if hit == HTTOPLEFT as u32 {
        Some(EDGE_LEFT | EDGE_TOP)
    } else if hit == HTTOPRIGHT as u32 {
        Some(EDGE_RIGHT | EDGE_TOP)
    } else if hit == HTBOTTOMLEFT as u32 {
        Some(EDGE_LEFT | EDGE_BOTTOM)
    } else if hit == HTBOTTOMRIGHT as u32 {
        Some(EDGE_RIGHT | EDGE_BOTTOM)
    } else {
        None
    }
}

/// Client size for a Shift-locked resize: one axis follows the pointer, the
/// other is derived from the video aspect, and both stay above the window's
/// minimum client area. Pure so the ratio math is testable without a window.
fn aspect_client_size(
    proposed_w: i32,
    proposed_h: i32,
    aspect: f32,
    floor_w: i32,
    floor_h: i32,
    inset_right: i32,
    width_primary: bool,
) -> (i32, i32) {
    let aspect = aspect.clamp(0.05, 20.0);
    let inset = inset_right.max(0);
    let video_floor_w = (floor_w - inset).max(1);
    if width_primary {
        let mut video_width = (proposed_w - inset).max(video_floor_w).max(1);
        let mut height = (video_width as f32 / aspect).round() as i32;
        if height < floor_h {
            height = floor_h;
            video_width = (height as f32 * aspect).round() as i32;
        }
        (video_width.max(1) + inset, height.max(1))
    } else {
        let mut height = proposed_h.max(floor_h).max(1);
        let mut video_width = (height as f32 * aspect).round() as i32;
        if video_width < video_floor_w {
            video_width = video_floor_w;
            height = (video_width as f32 / aspect).round() as i32;
        }
        (video_width.max(1) + inset, height.max(1))
    }
}

unsafe fn events_for(hwnd: HWND) -> Option<&'static WindowEvents> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowEvents;
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { &*pointer })
    }
}

unsafe fn start_drag(hwnd: HWND, events: Option<&WindowEvents>, mode: DragMode) -> bool {
    let Some(events) = events else {
        return false;
    };
    // Dragging a maximized window restores it first, the same way dragging the
    // system title bar does. Resizing cannot start while maximized.
    if matches!(mode, DragMode::Move) && unsafe { IsZoomed(hwnd) }.as_bool() {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    }
    let mut state = match events.drag.lock() {
        Ok(state) => state,
        Err(_) => return false,
    };
    if state.is_some() {
        return true;
    }
    let mut cursor = POINT::default();
    let mut window = RECT::default();
    let mut client = RECT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_err()
        || unsafe { GetWindowRect(hwnd, &mut window) }.is_err()
        || unsafe { GetClientRect(hwnd, &mut client) }.is_err()
    {
        return false;
    }
    let client_w = (client.right - client.left).max(1);
    let client_h = (client.bottom - client.top).max(1);
    let aspect = events
        .video_aspect
        .lock()
        .ok()
        .map(|value| *value)
        .filter(|(width, height)| *width > 0 && *height > 0)
        .map(|(width, height)| width as f32 / height as f32)
        .filter(|aspect| aspect.is_finite() && *aspect > 0.01)
        .unwrap_or(16.0 / 9.0);
    let inset_right = events
        .video_inset_right
        .lock()
        .ok()
        .map(|value| (*value).max(0))
        .unwrap_or(0);
    *state = Some(DragState {
        cursor,
        window,
        mode,
        client: (client_w, client_h),
        border: (
            (window.right - window.left - client_w).max(0),
            (window.bottom - window.top - client_h).max(0),
        ),
        aspect,
        inset_right,
    });
    let resizing = matches!(mode, DragMode::Resize(_));
    crate::log::line(&format!(
        "window drag start mode={:?} window={}x{} client={}x{} aspect={aspect:.4}",
        match mode {
            DragMode::Move => "move",
            DragMode::Resize(_) => "resize",
        },
        window.right - window.left,
        window.bottom - window.top,
        client_w,
        client_h
    ));
    drop(state);
    if let Ok(mut last) = events.last_drag_update.lock() {
        *last = None;
    }
    events.interacting.store(true, Ordering::SeqCst);
    events
        .interaction_resizing
        .store(resizing, Ordering::SeqCst);
    unsafe {
        begin_interaction(hwnd, resizing);
        SetCapture(hwnd);
    }
    events.push(InputEvent::Interaction {
        active: true,
        resizing,
    });
    true
}

unsafe fn update_drag(hwnd: HWND, events: Option<&WindowEvents>, force: bool) -> bool {
    let Some(events) = events else {
        return false;
    };
    let state = match events.drag.lock() {
        Ok(state) => *state,
        Err(_) => None,
    };
    let Some(state) = state else {
        return false;
    };
    if !force {
        if let Ok(mut last) = events.last_drag_update.lock() {
            let now = Instant::now();
            if last
                .map(|at| now.duration_since(at) < Duration::from_millis(8))
                .unwrap_or(false)
            {
                return true;
            }
            *last = Some(now);
        }
    } else if let Ok(mut last) = events.last_drag_update.lock() {
        *last = Some(Instant::now());
    }
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return false;
    }
    let dx = cursor.x - state.cursor.x;
    let dy = cursor.y - state.cursor.y;
    match state.mode {
        DragMode::Move => {
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    state.window.left + dx,
                    state.window.top + dy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        DragMode::Resize(edges) => {
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            let min_w = (480.0 * dpi).round() as i32;
            let min_h = (270.0 * dpi).round() as i32;
            let mut left = state.window.left;
            let mut top = state.window.top;
            let mut right = state.window.right;
            let mut bottom = state.window.bottom;
            if edges & EDGE_LEFT != 0 {
                left = (state.window.left + dx).min(state.window.right - min_w);
            }
            if edges & EDGE_RIGHT != 0 {
                right = (state.window.right + dx).max(state.window.left + min_w);
            }
            if edges & EDGE_TOP != 0 {
                top = (state.window.top + dy).min(state.window.bottom - min_h);
            }
            if edges & EDGE_BOTTOM != 0 {
                bottom = (state.window.bottom + dy).max(state.window.top + min_h);
            }
            // Shift locks the *client* area to the source aspect: the video
            // then fills the window edge to edge and the paper letterbox
            // disappears. The non-client border is measured at drag start so
            // the outer rectangle can be derived from the client target.
            // `GetAsyncKeyState`, not `GetKeyState`: the player is a background
            // process and may not own keyboard focus during a mouse-captured
            // drag, so the thread-local key state can be stale. The async state
            // reads the physical Shift key regardless of focus.
            let shift = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
            if shift {
                let border_w = state.border.0;
                let border_h = state.border.1;
                let proposed_w = (right - left - border_w).max(1);
                let proposed_h = (bottom - top - border_h).max(1);
                let horizontal = edges & (EDGE_LEFT | EDGE_RIGHT) != 0;
                let vertical = edges & (EDGE_TOP | EDGE_BOTTOM) != 0;
                let width_primary = if horizontal != vertical {
                    horizontal
                } else {
                    // A corner drag follows the axis the pointer moved more
                    // in, normalised to the same aspect.
                    let dw = (proposed_w - state.client.0).abs() as f32;
                    let dh = (proposed_h - state.client.1).abs() as f32 * state.aspect;
                    dw >= dh
                };
                let floor_w = (min_w - border_w).max(1);
                let floor_h = (min_h - border_h).max(1);
                let (client_w, client_h) = aspect_client_size(
                    proposed_w,
                    proposed_h,
                    state.aspect,
                    floor_w,
                    floor_h,
                    state.inset_right,
                    width_primary,
                );
                let outer_w = client_w + border_w;
                let outer_h = client_h + border_h;
                left = if edges & EDGE_LEFT != 0 {
                    state.window.right - outer_w
                } else {
                    state.window.left
                };
                top = if edges & EDGE_TOP != 0 {
                    state.window.bottom - outer_h
                } else {
                    state.window.top
                };
                right = left + outer_w;
                bottom = top + outer_h;
            }
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    left,
                    top,
                    (right - left).max(min_w),
                    (bottom - top).max(min_h),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
    }
    true
}

unsafe fn finish_drag(hwnd: HWND, events: &WindowEvents, state: DragState, release: bool) {
    if release {
        let _ = unsafe { ReleaseCapture() };
    }
    events.interacting.store(false, Ordering::SeqCst);
    events.interaction_resizing.store(false, Ordering::SeqCst);
    if let Ok(mut last) = events.last_drag_update.lock() {
        *last = None;
    }
    let resizing = matches!(state.mode, DragMode::Resize(_));
    unsafe { end_interaction(hwnd, resizing) };
    events.push(InputEvent::Interaction {
        active: false,
        resizing,
    });
    if resizing {
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
            let width = (client.right - client.left).max(1) as u32;
            let height = (client.bottom - client.top).max(1) as u32;
            crate::log::line(&format!(
                "window drag end resize client={width}x{height}"
            ));
            events.push(InputEvent::ResizeEnded { width, height });
        }
    } else {
        crate::log::line("window drag end move");
    }
}

unsafe fn end_drag(hwnd: HWND, events: Option<&WindowEvents>) -> bool {
    let Some(events) = events else {
        return false;
    };
    let active = events
        .drag
        .lock()
        .map(|mut state| state.take())
        .unwrap_or(None);
    let Some(state) = active else {
        return false;
    };
    unsafe { finish_drag(hwnd, events, state, true) };
    true
}

impl WindowEvents {
    fn push(&self, event: InputEvent) {
        if let Ok(mut queue) = self.queue.lock() {
            if queue.len() < 512 {
                queue.push_back(event);
            }
        }
    }

    /// Keep the source aspect available to the window proc so a Shift drag can
    /// lock the client area to it while resizing.
    pub fn set_video_aspect(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if let Ok(mut aspect) = self.video_aspect.lock() {
            *aspect = (width, height);
        }
    }

    /// Right lane reserved by the sidebar while it is open. Shift-resize locks
    /// the remaining video rectangle, not the whole window.
    pub fn set_video_inset_right(&self, inset: i32) {
        if let Ok(mut value) = self.video_inset_right.lock() {
            *value = inset.max(0);
        }
    }

    pub fn drain(&self) -> Vec<InputEvent> {
        self.queue
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let events = unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowEvents;
        if pointer.is_null() {
            None
        } else {
            Some(&*pointer)
        }
    };
    match message {
        WM_NCLBUTTONDOWN => {
            let hit = wparam.0 as u32;
            if let Some(edges) = edges_for_hit(hit) {
                if unsafe { start_drag(hwnd, events, DragMode::Resize(edges)) } {
                    return LRESULT(0);
                }
            }
            if hit == HTCAPTION as u32 && unsafe { start_drag(hwnd, events, DragMode::Move) } {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_NCLBUTTONUP => {
            if unsafe { end_drag(hwnd, events) } {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
        WM_CAPTURECHANGED => {
            if let Some(events) = events {
                let active = events
                    .drag
                    .lock()
                    .map(|mut state| state.take())
                    .unwrap_or(None);
                if let Some(state) = active {
                    // Capture can be stolen (Alt-Tab, another window). The
                    // shadow/rounding must not stay in interaction mode.
                    unsafe { finish_drag(hwnd, events, state, false) };
                }
            }
            LRESULT(0)
        }
        WM_NCCALCSIZE => {
            // Keep the client equal to the whole window: the custom top strip
            // and toolbar are drawn by the overlay, not by the non-client area.
            if wparam.0 != 0 {
                let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
                if unsafe { IsZoomed(hwnd).as_bool() } {
                    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    if unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
                        params.rgrc[0] = info.rcWork;
                    }
                }
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let x = loword(lparam.0);
            let y = hiword(lparam.0);
            let mut rect = RECT::default();
            unsafe {
                let _ = GetWindowRect(hwnd, &mut rect);
            }
            // A maximized window has no resize grips: its edges are the
            // screen edges, and the custom drag loop owns the top strip.
            if unsafe { IsZoomed(hwnd).as_bool() } {
                return LRESULT(1);
            }
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            let edge = (RESIZE_EDGE_DIP * dpi).round().max(4.0) as i32;
            let bottom_corner = (RESIZE_BOTTOM_CORNER_DIP * dpi).round().max(edge as f32) as i32;
            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);
            match resize_hit(
                x - rect.left,
                y - rect.top,
                width,
                height,
                edge,
                bottom_corner,
            ) {
                Some(hit) => LRESULT(hit as isize),
                None => LRESULT(1), // HTCLIENT
            }
        }
        WM_MOUSELEAVE => {
            if let Some(events) = events {
                events.push(InputEvent::MouseLeave);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = unsafe { std::mem::zeroed() };
            unsafe {
                let _ = BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(events) = events {
                events.push(InputEvent::Quit);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(events) = events {
                events.push(InputEvent::Quit);
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            unsafe { destroy_shadow(hwnd) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_MOVE => {
            let interacting = events
                .map(|events| events.interacting.load(Ordering::SeqCst))
                .unwrap_or(false);
            if interacting {
                let resizing = events
                    .map(|events| events.interaction_resizing.load(Ordering::SeqCst))
                    .unwrap_or(false);
                if !resizing {
                    // A move keeps the cached shadow: re-uploading the whole
                    // layered bitmap on every WM_MOVE was the dominant cost.
                    unsafe { move_shadow(hwnd) };
                }
            } else {
                update_shadow(hwnd);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let width = (lparam.0 as u32 & 0xffff).max(1);
            let height = ((lparam.0 as u32 >> 16) & 0xffff).max(1);
            let interacting = events
                .map(|events| events.interacting.load(Ordering::SeqCst))
                .unwrap_or(false);
            if !interacting {
                unsafe { apply_player_region(hwnd) };
                update_shadow(hwnd);
            }
            if let Some(events) = events {
                events.push(InputEvent::Resize { width, height });
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            // Report focus once per activation; the shell uses the newest
            // focus sequence to decide which player window a card replaces.
            if let Some(events) = events {
                events.push(InputEvent::Focus);
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wparam.0 as u32 == VK_SHIFT.0 as u32 {
                // Shift changes the resize rule; re-evaluate the drag rectangle
                // now so the aspect snap is visible before the next mouse move.
                unsafe { update_drag(hwnd, events, true) };
                return LRESULT(0);
            }
            if let Some(events) = events {
                events.push(InputEvent::Key(wparam.0 as u32));
            }
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if wparam.0 as u32 == VK_SHIFT.0 as u32 {
                // Releasing Shift returns the same drag to free resizing.
                unsafe { update_drag(hwnd, events, true) };
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_CHAR => {
            if let Some(events) = events {
                events.push(InputEvent::Char(wparam.0 as u16));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // A manual move/resize owns the mouse: no modal loop, so the player
            // thread keeps presenting frames. Still notify the engine of the
            // pointer position so the chrome stays awake during the gesture.
            if unsafe { update_drag(hwnd, events, false) } {
                if let Some(events) = events {
                    events.push(InputEvent::MouseMove {
                        x: loword(lparam.0),
                        y: hiword(lparam.0),
                    });
                }
                return LRESULT(0);
            }
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // Re-arms on every move; harmless and keeps WM_MOUSELEAVE coming.
            let _ = unsafe { TrackMouseEvent(&mut track) };
            if let Some(events) = events {
                events.push(InputEvent::MouseMove {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(events) = events {
                events.push(InputEvent::LeftDown {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // Apply the pointer's final position even when the last move was
            // throttled, then finish the gesture.
            let _ = unsafe { update_drag(hwnd, events, true) };
            if unsafe { end_drag(hwnd, events) } {
                if let Some(events) = events {
                    events.push(InputEvent::MouseMove {
                        x: loword(lparam.0),
                        y: hiword(lparam.0),
                    });
                }
                return LRESULT(0);
            }
            if let Some(events) = events {
                events.push(InputEvent::LeftUp);
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if let Some(events) = events {
                events.push(InputEvent::RightDown {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if let Some(events) = events {
                events.push(InputEvent::RightUp {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            if let Some(events) = events {
                events.push(InputEvent::DoubleClick {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as i16 as i32;
            if let Some(events) = events {
                events.push(InputEvent::Wheel(delta));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn loword(value: isize) -> i32 {
    (value as i32 & 0xffff) as i16 as i32
}

fn hiword(value: isize) -> i32 {
    ((value as i32 >> 16) & 0xffff) as i16 as i32
}

/// Large rounded corners for the standalone player. DWM only offers its
/// small default radius, so clip the window with a GDI region and refresh it
/// on every size change. Maximized/fullscreen stays square.
unsafe fn apply_player_region(hwnd: HWND) {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let monitor_rect = if unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
        info.rcMonitor
    } else {
        rect
    };
    let covers_monitor = rect.left <= monitor_rect.left
        && rect.top <= monitor_rect.top
        && rect.right >= monitor_rect.right
        && rect.bottom >= monitor_rect.bottom;
    let radius = if unsafe { IsZoomed(hwnd).as_bool() } || covers_monitor {
        0
    } else {
        let dpi = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
        (16.0 * dpi).round().max(1.0) as i32
    };
    let region = unsafe {
        if radius > 0 {
            CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2)
        } else {
            CreateRectRgn(0, 0, width + 1, height + 1)
        }
    };
    if region.0.is_null() {
        return;
    }
    if unsafe { SetWindowRgn(hwnd, Some(region), true) } == 0 {
        let _ = unsafe { DeleteObject(region.into()) };
    }
}

unsafe extern "system" fn shadow_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Create the click-through shadow window behind one standalone player.
fn attach_shadow(owner: HWND) -> Result<()> {
    unsafe {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        if REGISTERED.get().is_none() {
            let instance = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                style: Default::default(),
                lpfnWndProc: Some(shadow_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance.into(),
                hIcon: Default::default(),
                hCursor: Default::default(),
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: w!("O11A1Shadow"),
            };
            let atom = RegisterClassW(&wc);
            if atom == 0 {
                let error = windows::core::Error::from_thread();
                if error.code().0 as u32 != 0x8007_0582 {
                    return Err(anyhow!("RegisterClassW(shadow): {error}"));
                }
            }
            let _ = REGISTERED.set(());
        }
        let instance = GetModuleHandleW(None)?;
        let shadow = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("O11A1Shadow"),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        if shadow.0.is_null() {
            return Ok(());
        }
        let _ = ShowWindow(shadow, SW_HIDE);
        if let Ok(mut map) = shadows().lock() {
            map.insert(
                owner.0 as isize,
                ShadowSlot {
                    hwnd: shadow.0 as isize,
                    hdc: 0,
                    bitmap: 0,
                    width: 0,
                    height: 0,
                    pad: 0,
                },
            );
        }
        update_shadow(owner);
    }
    Ok(())
}

fn hide_shadow(owner: HWND) {
    if let Ok(map) = shadows().lock() {
        if let Some(slot) = map.get(&(owner.0 as isize)) {
            let _ = unsafe { ShowWindow(HWND(slot.hwnd as *mut c_void), SW_HIDE) };
        }
    }
}

fn destroy_shadow(owner: HWND) {
    let slot = shadows()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(&(owner.0 as isize)));
    let Some(slot) = slot else {
        return;
    };
    unsafe {
        if slot.hdc != 0 {
            let _ = DeleteDC(HDC(slot.hdc as *mut c_void));
        }
        if slot.bitmap != 0 {
            let _ = DeleteObject(HGDIOBJ(slot.bitmap as *mut c_void));
        }
        if slot.hwnd != 0 {
            let _ = DestroyWindow(HWND(slot.hwnd as *mut c_void));
        }
    }
}

unsafe fn regenerate_shadow(slot: &mut ShadowSlot, width: i32, height: i32, pad: i32, radius: i32) {
    unsafe {
        if slot.hdc != 0 {
            let _ = DeleteDC(HDC(slot.hdc as *mut c_void));
            slot.hdc = 0;
        }
        if slot.bitmap != 0 {
            let _ = DeleteObject(HGDIOBJ(slot.bitmap as *mut c_void));
            slot.bitmap = 0;
        }
        let total_w = width + pad * 2;
        let total_h = height + pad * 2;
        if total_w <= 0 || total_h <= 0 {
            return;
        }
        let memory = CreateCompatibleDC(None);
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: total_w,
            biHeight: -total_h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(_) => {
                let _ = DeleteDC(memory);
                return;
            }
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            return;
        }
        SelectObject(memory, HGDIOBJ(bitmap.0));
        let pixels = std::slice::from_raw_parts_mut(
            bits as *mut u8,
            (total_w as usize) * (total_h as usize) * 4,
        );
        let half_w = width as f32 * 0.5;
        let half_h = height as f32 * 0.5;
        let radius = (radius as f32).min(half_w).min(half_h).max(0.0);
        let pad_f = pad as f32;
        for y in 0..total_h {
            for x in 0..total_w {
                let px = x as f32 + 0.5 - pad_f - half_w;
                let py = y as f32 + 0.5 - pad_f - half_h;
                let qx = px.abs() - (half_w - radius);
                let qy = py.abs() - (half_h - radius);
                let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt()
                    + qx.max(qy).min(0.0)
                    - radius;
                if outside >= pad_f {
                    continue;
                }
                let t = (1.0 - outside.max(0.0) / pad_f).clamp(0.0, 1.0);
                let alpha = (0.34 * t.powi(3) + 0.10 * t).min(0.44);
                let a = (alpha * 255.0).round() as u8;
                let index = ((y as usize) * (total_w as usize) + x as usize) * 4;
                pixels[index] = 0;
                pixels[index + 1] = 0;
                pixels[index + 2] = 0;
                pixels[index + 3] = a;
            }
        }
        slot.hdc = memory.0 as isize;
        slot.bitmap = bitmap.0 as isize;
        slot.width = width;
        slot.height = height;
        slot.pad = pad;
    }
}

/// During a drag-resize the 16 px rounded silhouette is replaced by a large
/// square region. Updating the rounded region on every WM_SIZE forced a DWM
/// shape update for each message; the rounded region is applied once when the
/// gesture ends.
unsafe fn apply_player_region_square(hwnd: HWND) {
    let region = unsafe { CreateRectRgn(0, 0, 32767, 32767) };
    if region.0.is_null() {
        return;
    }
    if unsafe { SetWindowRgn(hwnd, Some(region), true) } == 0 {
        let _ = unsafe { DeleteObject(region.into()) };
    }
}

/// Enter the non-modal interaction fast path. A resize hides the shadow and
/// uses a square window region; a move keeps the cached shadow and only
/// repositions it. Neither path uploads the shadow bitmap or rebuilds the
/// rounded region per mouse message.
unsafe fn begin_interaction(hwnd: HWND, resizing: bool) {
    if resizing {
        hide_shadow(hwnd);
        unsafe { apply_player_region_square(hwnd) };
    } else {
        unsafe { move_shadow(hwnd) };
    }
}

/// Restore the normal chrome once the gesture ends: rounded region first,
/// then one shadow upload at the final rectangle.
unsafe fn end_interaction(hwnd: HWND, resizing: bool) {
    if resizing {
        unsafe { apply_player_region(hwnd) };
    }
    update_shadow(hwnd);
}

/// Move the already-uploaded layered shadow with its owner. Moving a layered
/// window with SetWindowPos keeps its bitmap; calling UpdateLayeredWindow on
/// every WM_MOVE re-uploaded the whole ARGB surface (about 34 MB at 4K).
unsafe fn move_shadow(owner: HWND) {
    let Some((shadow_key, pad)) = shadows()
        .lock()
        .ok()
        .and_then(|map| map.get(&(owner.0 as isize)).map(|slot| (slot.hwnd, slot.pad)))
    else {
        return;
    };
    if shadow_key == 0 || pad <= 0 {
        return;
    }
    unsafe {
        let shadow = HWND(shadow_key as *mut c_void);
        if !IsWindowVisible(shadow).as_bool() {
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(owner, &mut rect).is_err() {
            return;
        }
        let _ = SetWindowPos(
            shadow,
            Some(owner),
            rect.left - pad,
            rect.top - pad,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

fn update_shadow(owner: HWND) {
    let Some(shadow_key) = shadows()
        .lock()
        .ok()
        .and_then(|map| map.get(&(owner.0 as isize)).map(|slot| slot.hwnd))
    else {
        return;
    };
    unsafe {
        let shadow = HWND(shadow_key as *mut c_void);
        if !IsWindowVisible(owner).as_bool() || IsZoomed(owner).as_bool() {
            let _ = ShowWindow(shadow, SW_HIDE);
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(owner, &mut rect).is_err() {
            return;
        }
        let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut monitor_info).as_bool() {
            let screen = monitor_info.rcMonitor;
            if rect.left <= screen.left
                && rect.top <= screen.top
                && rect.right >= screen.right
                && rect.bottom >= screen.bottom
            {
                let _ = ShowWindow(shadow, SW_HIDE);
                return;
            }
        }
        let dpi = GetDpiForWindow(owner).max(96) as f32 / 96.0;
        let pad = (SHADOW_PAD * dpi).round() as i32;
        let radius = (SHADOW_RADIUS * dpi).round() as i32;
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        if let Ok(mut map) = shadows().lock() {
            let Some(slot) = map.get_mut(&(owner.0 as isize)) else {
                return;
            };
            if slot.width != width || slot.height != height || slot.pad != pad {
                regenerate_shadow(slot, width, height, pad, radius);
            }
            if slot.hdc == 0 || slot.bitmap == 0 {
                return;
            }
            let destination = POINT {
                x: rect.left - pad,
                y: rect.top - pad,
            };
            let size = SIZE {
                cx: width + pad * 2,
                cy: height + pad * 2,
            };
            let source = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let hdc = HDC(slot.hdc as *mut c_void);
            let _ = UpdateLayeredWindow(
                shadow,
                None,
                Some(&destination),
                Some(&size),
                Some(hdc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            // Directly under the player, never above it.
            let _ = SetWindowPos(
                shadow,
                Some(owner),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = ShowWindow(shadow, SW_SHOWNOACTIVATE);
        }
    }
}

/// The engine is a background process, so Windows usually denies a plain
/// `SetForegroundWindow`. Attaching to the current foreground thread for the
/// duration of the call lets the player take keyboard focus immediately rather
/// than waiting for the user to click the picture. Without this, Space goes to
/// the Electron window that launched the player.
/// Bring the player window to the foreground. Used on initial show and on a
/// card replacement: an inactive player swallows the first press as an
/// activation click, which made the loading card feel undraggable.
pub fn focus_player(hwnd: HWND) {
    force_foreground(hwnd);
}

fn force_foreground(hwnd: HWND) {
    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let own_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != own_thread
            && AttachThreadInput(foreground_thread, own_thread, true).as_bool();
        let set_ok = SetForegroundWindow(hwnd).as_bool();
        let active_ok = SetActiveWindow(hwnd).is_ok();
        let focus_ok = SetFocus(Some(hwnd)).is_ok();
        let after = GetForegroundWindow();
        if attached {
            let _ = AttachThreadInput(foreground_thread, own_thread, false);
        }
        crate::log::line(&format!(
            "window focus before={:?} after={:?} set={} active={} focus={} attached={}",
            foreground.0,
            after.0,
            u8::from(set_ok),
            u8::from(active_ok),
            u8::from(focus_ok),
            u8::from(attached)
        ));
    }
}

/// Create the player window. `client_width`/`client_height` are physical pixels.
pub fn create(
    title: &str,
    client_width: u32,
    client_height: u32,
) -> Result<(HWND, Arc<WindowEvents>)> {
    // Must happen before the first top-level window exists in this process, or
    // the taskbar has already decided the engine is its own app.
    join_shell_taskbar_group();
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("O11A1NativePlayer");
        static REGISTERED: OnceLock<()> = OnceLock::new();
        if REGISTERED.get().is_none() {
            let cursor = LoadCursorW(None, IDC_ARROW)?;
            let wc = WNDCLASSW {
                style: CS_DBLCLKS,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance.into(),
                hIcon: Default::default(),
                hCursor: cursor,
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class,
            };
            let atom = RegisterClassW(&wc);
            if atom == 0 {
                let error = windows::core::Error::from_thread();
                // ERROR_CLASS_ALREADY_EXISTS (1410) means a previous Player
                // already registered the class in this process.
                if error.code().0 as u32 != 0x8007_0582 {
                    return Err(anyhow!("RegisterClassW: {error}"));
                }
            }
            let _ = REGISTERED.set(());
        }
        let width = client_width.max(320) as i32;
        let height = client_height.max(180) as i32;
        let style = WS_POPUP | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
        let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let cascade = (WINDOW_SEQ.fetch_add(1, Ordering::SeqCst) % 6) as i32;
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            class,
            PCWSTR(title_wide.as_ptr()),
            style,
            80 + cascade * 46,
            60 + cascade * 46,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        if hwnd.0.is_null() {
            return Err(anyhow!("CreateWindowExW returned null"));
        }
        // Both the standalone runner and the Electron sidecar link this
        // module; setting the window icons here keeps the taskbar button on
        // the app mark even when the host executable has no icon resource.
        set_window_icon(hwnd);
        // The window is clipped by our own 16px GDI region. DWM's rounded
        // corner preference would draw a second, smaller arc (and its default
        // border) inside that region, leaving a dark line near each corner.
        // Keep DWM square and drop its border; the region owns the shape.
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const DWM_WINDOW_CORNER_PREFERENCE as *const _,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let border = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        let events = Arc::new(WindowEvents::default());
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Arc::as_ptr(&events) as *const WindowEvents as isize,
        );
        // Stay hidden until the composition tree has painted its first frame;
        // showing an empty WS_EX_NOREDIRECTIONBITMAP window would flash a bare
        // DWM frame before the loading card exists.
        apply_player_region(hwnd);
        let _ = attach_shadow(hwnd);
        Ok((hwnd, events))
    }
}

/// Reveal the standalone player after its first composed frame exists. The
/// topmost -> not-topmost bump raises it above the Electron window without
/// stealing focus; `create` deliberately leaves the HWND hidden.
pub fn show_player(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = UpdateWindow(hwnd);
        apply_player_region(hwnd);
        force_foreground(hwnd);
        update_shadow(hwnd);
    }
}

/// Message proc for the Electron-hosted video popup: no resize borders, no
/// keyboard/mouse routing (the Electron UI owns interaction), only the size
/// changes that keep the composition tree in sync and close/quit.
unsafe extern "system" fn hosted_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let events = unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowEvents;
        if pointer.is_null() {
            None
        } else {
            Some(&*pointer)
        }
    };
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = unsafe { std::mem::zeroed() };
            unsafe {
                let _ = BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(events) = events {
                events.push(InputEvent::Quit);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(events) = events {
                events.push(InputEvent::Quit);
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_SIZE => {
            let width = (lparam.0 as u32 & 0xffff).max(1);
            let height = ((lparam.0 as u32 >> 16) & 0xffff).max(1);
            if let Some(events) = events {
                events.push(InputEvent::Resize { width, height });
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(events) = events {
                events.push(InputEvent::MouseMove {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(events) = events {
                events.push(InputEvent::LeftDown {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if unsafe { end_drag(hwnd, events) } {
                return LRESULT(0);
            }
            if let Some(events) = events {
                events.push(InputEvent::LeftUp);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            if let Some(events) = events {
                events.push(InputEvent::DoubleClick {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if let Some(events) = events {
                events.push(InputEvent::RightDown {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if let Some(events) = events {
                events.push(InputEvent::RightUp {
                    x: loword(lparam.0),
                    y: hiword(lparam.0),
                });
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// Create the video-only owned popup used when Electron hosts the player.
/// `x`/`y`/`width`/`height` are physical screen pixels.
pub fn create_hosted(
    parent: HWND,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(HWND, Arc<WindowEvents>)> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("O11A1ElectronVideo");
        static REGISTERED: OnceLock<()> = OnceLock::new();
        if REGISTERED.get().is_none() {
            let cursor = LoadCursorW(None, IDC_ARROW)?;
            let wc = WNDCLASSW {
                style: CS_DBLCLKS,
                lpfnWndProc: Some(hosted_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance.into(),
                hIcon: Default::default(),
                hCursor: cursor,
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class,
            };
            let atom = RegisterClassW(&wc);
            if atom == 0 {
                let error = windows::core::Error::from_thread();
                if error.code().0 as u32 != 0x8007_0582 {
                    return Err(anyhow!("RegisterClassW(electron video): {error}"));
                }
            }
            let _ = REGISTERED.set(());
        }
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            PCWSTR::null(),
            WS_POPUP,
            x,
            y,
            width.max(320) as i32,
            height.max(180) as i32,
            Some(parent),
            None,
            Some(instance.into()),
            None,
        )?;
        if hwnd.0.is_null() {
            return Err(anyhow!("CreateWindowExW(electron video) returned null"));
        }
        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const DWM_WINDOW_CORNER_PREFERENCE as *const _,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let events = Arc::new(WindowEvents::default());
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Arc::as_ptr(&events) as *const WindowEvents as isize,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);
        Ok((hwnd, events))
    }
}

/// Show or hide the hosted popup without activating it.
pub fn set_visible(hwnd: HWND, visible: bool) {
    let command = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
    let _ = unsafe { ShowWindow(hwnd, command) };
    if visible {
        update_shadow(hwnd);
    } else {
        hide_shadow(hwnd);
    }
}

/// Reposition the hosted popup without touching z-order or activation.
pub fn move_rect(hwnd: HWND, x: i32, y: i32, width: u32, height: u32) {
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            y,
            width.max(320) as i32,
            height.max(180) as i32,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    update_shadow(hwnd);
}

pub fn pump() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Full monitor size in physical pixels. The composition layers are allocated
/// to this once, so opening the sidebar, maximizing or going fullscreen never
/// has to resize a swapchain (a resized composition swapchain can present one
/// blank frame).
pub fn monitor_size(hwnd: HWND) -> (u32, u32) {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
        let rect = info.rcMonitor;
        return (
            (rect.right - rect.left).max(1) as u32,
            (rect.bottom - rect.top).max(1) as u32,
        );
    }
    client_size(hwnd)
}

pub fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (
        (rect.right - rect.left).max(1) as u32,
        (rect.bottom - rect.top).max(1) as u32,
    )
}

pub fn dpi_scale(hwnd: HWND) -> f32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        (dpi as f32 / 96.0).clamp(1.0, 3.0)
    }
}

/// DPI scale before a window exists, used to turn the remembered logical
/// player size into physical pixels for `create`.
pub fn system_dpi_scale() -> f32 {
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 {
        1.0
    } else {
        (dpi as f32 / 96.0).clamp(1.0, 3.0)
    }
}

pub fn destroy(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Drag the frameless window by its custom top strip.
/// Begin a non-modal whole-window drag. The old implementation sent
/// `WM_NCLBUTTONDOWN/HTCAPTION`, which hands control to Windows' modal move
/// loop and freezes the engine thread (and therefore the video) until the mouse
/// is released. Here the window proc moves the window from its own message
/// pump, so playback and presentation continue for the whole gesture.
pub fn drag_window(hwnd: HWND) {
    unsafe {
        // Only a real left-button press may start a drag. `handle_click` runs
        // on LeftDown, but keeping the guard makes a stray release harmless.
        if GetKeyState(VK_LBUTTON.0 as i32) >= 0 {
            return;
        }
        let events = events_for(hwnd);
        let _ = start_drag(hwnd, events, DragMode::Move);
    }
}

pub fn minimize(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
}

pub fn toggle_maximize(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(
            hwnd,
            if IsZoomed(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            },
        );
    }
}

pub fn close_window(hwnd: HWND) {
    unsafe {
        // Hide on the click itself; decoder/audio joins may take a few frames
        // and the disappearing window must not wait for them.
        hide_shadow(hwnd);
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn is_maximized(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

/// Grow/shrink the window width from its left edge, keeping it on the work
/// area. Windowed mode opens the right strip outward instead of eating the
/// picture.
pub fn grow_width(hwnd: HWND, delta: i32) {
    if delta == 0 {
        return;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let mut width = (rect.right - rect.left + delta).max(720);
    let mut x = rect.left;
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
        let work = info.rcWork;
        let max_w = (work.right - work.left).max(720);
        width = width.min(max_w);
        if x + width > work.right {
            x = (work.right - width).max(work.left);
        }
        if x < work.left {
            x = work.left;
        }
    }
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            x,
            rect.top,
            width,
            rect.bottom - rect.top,
            // A no-redirection-bitmap window paints through DirectComposition,
            // not WM_PAINT. Suppressing the GDI invalidation avoids a blank
            // composition frame at the old/new width boundary.
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
        )
    };
}

/// Retitle the window once the deferred source reports its real title.
pub fn set_title(hwnd: HWND, title: &str) {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(wide.as_ptr())) };
}

/// Toggle between the decorated window and borderless monitor-filling mode.
pub fn toggle_fullscreen(
    hwnd: HWND,
    fullscreen: &mut bool,
    saved: &mut Option<(isize, RECT)>,
) -> bool {
    unsafe {
        if !*fullscreen {
            if IsZoomed(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            *saved = Some((style, rect));
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let target = info.rcMonitor;
                // Fullscreen drops the resize frame but keeps the frameless
                // popup identity; the custom chrome is hidden by the engine.
                SetWindowLongPtrW(
                    hwnd,
                    GWL_STYLE,
                    style
                        & !(WS_THICKFRAME.0 as isize
                            | WS_MINIMIZEBOX.0 as isize
                            | WS_MAXIMIZEBOX.0 as isize),
                );
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    target.left,
                    target.top,
                    target.right - target.left,
                    target.bottom - target.top,
                    SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
                *fullscreen = true;
            }
        } else {
            if let Some((style, rect)) = saved.take() {
                SetWindowLongPtrW(hwnd, GWL_STYLE, style);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                );
            }
            *fullscreen = false;
        }
        *fullscreen
    }
}

/// Outer window rect of `hwnd`.
pub fn outer_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    rect
}

fn outer_size(client_width: i32, client_height: i32) -> (i32, i32) {
    // The player is frameless (WM_NCCALCSIZE returns 0), so the outer size is
    // exactly the requested client size.
    (client_width.max(160), client_height.max(90))
}

/// Shrink the window to a 480×270 client (16:9), clamped to the monitor work
/// area and centered there, and make it topmost. The standard decorated frame
/// stays, so the title bar still drags the mini window.
pub fn set_mini(hwnd: HWND) -> Result<()> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        };
        let work_width = (work.right - work.left).max(200);
        let work_height = (work.bottom - work.top).max(120);
        let mut client_width = 480i32;
        let mut client_height = 270i32;
        let (mut outer_width, mut outer_height) = outer_size(client_width, client_height);
        if outer_width > work_width || outer_height > work_height {
            let scale = (work_width as f32 / outer_width as f32)
                .min(work_height as f32 / outer_height as f32)
                .min(1.0);
            client_width = ((480.0 * scale).floor() as i32).max(240);
            client_height = ((client_width as f32 * 9.0 / 16.0).floor() as i32).max(135);
            let size = outer_size(client_width, client_height);
            outer_width = size.0;
            outer_height = size.1;
        }
        let x = work.left + ((work_width - outer_width) / 2).max(0);
        let y = work.top + ((work_height - outer_height) / 2).max(0);
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            outer_width,
            outer_height,
            SWP_SHOWWINDOW,
        )?;
        Ok(())
    }
}

/// Restore a previously saved outer window rect and drop topmost.
pub fn restore_rect(hwnd: HWND, rect: RECT) -> Result<()> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            rect.left,
            rect.top,
            (rect.right - rect.left).max(160),
            (rect.bottom - rect.top).max(90),
            SWP_SHOWWINDOW,
        )?;
    }
    Ok(())
}

/// Add or remove `HWND_TOPMOST` without moving, resizing or activating the
/// window. The edge style is verified separately with [`is_topmost`].
pub fn set_topmost(hwnd: HWND, topmost: bool) -> Result<()> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(if topmost {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
    }
    update_shadow(hwnd);
    Ok(())
}

/// Measured topmost state (`WS_EX_TOPMOST`), not what the caller asked for.
pub fn is_topmost(hwnd: HWND) -> bool {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    style & WS_EX_TOPMOST.0 != 0
}

/// GDI screen capture of the window rect. `PrintWindow` is useless for a
/// `WS_EX_NOREDIRECTIONBITMAP` window (there is no redirection surface), so the
/// pixels are read from the compositor's scanned-out desktop, the same recipe
/// the f192 prototype used.
pub fn capture(hwnd: HWND, path: &std::path::Path) -> Result<()> {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect)?;
        let width = (rect.right - rect.left).max(1) as usize;
        let height = (rect.bottom - rect.top).max(1) as usize;
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(anyhow!("GetDC failed"));
        }
        let memory = CreateCompatibleDC(Some(screen));
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            return Err(anyhow!("CreateDIBSection returned no bits"));
        }
        SelectObject(memory, HGDIOBJ(bitmap.0));
        let copied = BitBlt(
            memory,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            rect.left,
            rect.top,
            SRCCOPY | CAPTUREBLT,
        );
        let mut bgra = vec![0u8; width * height * 4];
        std::ptr::copy_nonoverlapping(bits as *const u8, bgra.as_mut_ptr(), bgra.len());
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        copied?;
        let mut rgba = vec![0u8; bgra.len()];
        for (source, target) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            target[0] = source[2];
            target[1] = source[1];
            target[2] = source[0];
            target[3] = 255;
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        image::save_buffer(
            path,
            &rgba,
            width as u32,
            height as u32,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(())
    }
}

/// A short sleep the engine's pacer uses between state polls. Message pumping
/// is separate and happens on every tick.
pub fn tiny_sleep() {
    std::thread::sleep(Duration::from_millis(1));
}

#[cfg(test)]
mod tests {
    use super::{
        aspect_client_size, resize_hit, HTBOTTOM, HTBOTTOMRIGHT, HTRIGHT, HTTOPRIGHT,
    };

    #[test]
    fn bottom_right_grip_survives_the_rounded_corner() {
        let (width, height) = (800, 600);
        let edge = 8;
        let corner = 24;
        // The visible arc is about 5 px inside the window corner; the old
        // 6 px square left only a tiny sliver of it reachable. The 24 px grip
        // must classify the whole visible corner as the diagonal resize.
        assert_eq!(
            resize_hit(width - 5, height - 5, width, height, edge, corner),
            Some(HTBOTTOMRIGHT as u32)
        );
        assert_eq!(
            resize_hit(width - 12, height - 12, width, height, edge, corner),
            Some(HTBOTTOMRIGHT as u32)
        );
        assert_eq!(
            resize_hit(width - 24, height - 24, width, height, edge, corner),
            Some(HTBOTTOMRIGHT as u32)
        );
        // Outside the corner grip the ordinary edge bands still work.
        assert_eq!(
            resize_hit(width - 4, height / 2, width, height, edge, corner),
            Some(HTRIGHT as u32)
        );
        assert_eq!(
            resize_hit(width / 2, height - 4, width, height, edge, corner),
            Some(HTBOTTOM as u32)
        );
        // The top-right corner stays at the 8 px edge band so it never
        // covers the close button, which sits 8 px from the window edge.
        assert_eq!(
            resize_hit(width - 4, 4, width, height, edge, corner),
            Some(HTTOPRIGHT as u32)
        );
        assert_eq!(
            resize_hit(width - 12, 20, width, height, edge, corner),
            None
        );
        assert_eq!(
            resize_hit(width / 2, height / 2, width, height, edge, corner),
            None
        );
    }

    fn ratio_ok(width: i32, height: i32, aspect: f32) -> bool {
        ((width as f32 / height as f32) - aspect).abs() < 0.01
    }

    #[test]
    fn shift_resize_derives_the_other_axis_from_video_aspect() {
        let (width, height) = aspect_client_size(1280, 400, 16.0 / 9.0, 464, 254, 0, true);
        assert_eq!(width, 1280);
        assert_eq!(height, 720);
        assert!(ratio_ok(width, height, 16.0 / 9.0));
    }

    #[test]
    fn shift_resize_respects_the_minimum_client_area() {
        let (width, height) = aspect_client_size(100, 100, 16.0 / 9.0, 464, 254, 0, true);
        assert!(width >= 464 && height >= 254);
        assert!(ratio_ok(width, height, 16.0 / 9.0));
    }

    #[test]
    fn shift_resize_handles_vertical_video() {
        let (width, height) = aspect_client_size(400, 900, 9.0 / 16.0, 464, 254, 0, false);
        assert!(width >= 464 && height >= 254);
        assert!(ratio_ok(width, height, 9.0 / 16.0));
    }

    #[test]
    fn shift_resize_locks_the_video_area_next_to_the_sidebar() {
        let inset = 360;
        let (width, height) = aspect_client_size(1600, 500, 16.0 / 9.0, 464, 254, inset, true);
        // The video area, not the whole client, carries the 16:9 ratio.
        assert!(ratio_ok(width - inset, height, 16.0 / 9.0));
        assert_eq!(width - inset, 1600 - inset);
    }
}
