//! The Win32 window, its message pump and the input events the engine needs.
//!
//! The window has `WS_EX_NOREDIRECTIONBITMAP`: DWM owns no surface for it and
//! the only pixels it can show come from the DirectComposition tree built in
//! `gpu.rs`. It is created on the player thread and all messages are pumped
//! there, so there is exactly one thread touching the swapchains.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject, UpdateWindow, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetWindowLongPtrW, GetWindowRect, IsZoomed, LoadCursorW, PeekMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, GWLP_USERDATA,
    GWL_EXSTYLE, GWL_STYLE, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IDC_ARROW, MSG,
    PM_REMOVE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER,
    SWP_SHOWWINDOW, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW,
    WM_CHAR, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCALCSIZE, WM_NCDESTROY,
    WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN, WM_SIZE, WM_SYSKEYDOWN, NCCALCSIZE_PARAMS,
    WNDCLASSW, CS_DBLCLKS,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
    WS_POPUP, WS_THICKFRAME, SW_HIDE, SW_SHOWNOACTIVATE,
};

/// `WM_MOUSELEAVE` lives in `Win32::UI::Controls` upstream; defining it here
/// avoids pulling the whole controls feature for one message.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Cascade standalone player windows like the old native client.
static WINDOW_SEQ: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Quit,
    Key(u32),
    Char(u16),
    MouseMove { x: i32, y: i32 },
    MouseLeave,
    LeftDown { x: i32, y: i32 },
    LeftUp,
    /// Right-button press on the picture: opens the danmaku context menu.
    RightDown { x: i32, y: i32 },
    DoubleClick { x: i32, y: i32 },
    Wheel(i32),
    Resize { width: u32, height: u32 },
}

#[derive(Default)]
pub struct WindowEvents {
    queue: Mutex<VecDeque<InputEvent>>,
}

impl WindowEvents {
    fn push(&self, event: InputEvent) {
        if let Ok(mut queue) = self.queue.lock() {
            if queue.len() < 512 {
                queue.push_back(event);
            }
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
            let border = (6.0 * unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0).round() as i32;
            let border = border.max(4);
            let left = x < rect.left + border;
            let right = x >= rect.right - border;
            let top = y < rect.top + border;
            let bottom = y >= rect.bottom - border;
            let hit = if top && left {
                HTTOPLEFT
            } else if top && right {
                HTTOPRIGHT
            } else if bottom && left {
                HTBOTTOMLEFT
            } else if bottom && right {
                HTBOTTOMRIGHT
            } else if left {
                HTLEFT
            } else if right {
                HTRIGHT
            } else if top {
                HTTOP
            } else if bottom {
                HTBOTTOM
            } else {
                return LRESULT(1); // HTCLIENT
            };
            LRESULT(hit as isize)
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
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if let Some(events) = events {
                events.push(InputEvent::Key(wparam.0 as u32));
            }
            LRESULT(0)
        }
        WM_CHAR => {
            if let Some(events) = events {
                events.push(InputEvent::Char(wparam.0 as u16));
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

/// Create the player window. `client_width`/`client_height` are physical pixels.
pub fn create(
    title: &str,
    client_width: u32,
    client_height: u32,
) -> Result<(HWND, Arc<WindowEvents>)> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("O11A1NativePlayer");
        static REGISTERED: OnceLock<()> = OnceLock::new();
        if REGISTERED.get().is_none() {
            let cursor = LoadCursorW(None, IDC_ARROW)?;
            let wc = WNDCLASSW {
                style: Default::default(),
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
        // DWM's rounded corners on the frameless window. The attribute is
        // ignored on older Windows versions; the chrome still paints.
        let corner = DWMWCP_ROUND;
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
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        Ok((hwnd, events))
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

pub fn destroy(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Drag the frameless window by its custom top strip.
pub fn drag_window(hwnd: HWND) {
    unsafe {
        let _ = ReleaseCapture();
        let _ = SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            Some(LPARAM(0)),
        );
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
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn is_maximized(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
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
                    style & !(WS_THICKFRAME.0 as isize | WS_MINIMIZEBOX.0 as isize
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
            Some(if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
    }
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
