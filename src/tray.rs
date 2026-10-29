//! Windows system-tray icon, global hotkey (老板键), and balloon notifications.
//!
//! Slint owns the process event loop and does not expose raw Win32 message
//! pumping, so this module runs its own thread with a hidden window and a
//! `GetMessageW` loop. That thread owns the tray icon, the registered hotkey
//! and the notification balloon. It never touches Slint directly: it signals
//! the UI thread through the caller-supplied [`Actions`] closures, which are
//! expected to hop onto the UI thread with `slint::invoke_from_event_loop`.
//! `notify` is thread-safe and only queues a string plus a window message.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
};
use windows_sys::Win32::System::LibraryLoader as kernel32;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    LoadImageW, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, HICON,
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, MF_SEPARATOR, MF_STRING, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, TrackPopupMenu,
    TranslateMessage, IDI_APPLICATION, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
    WM_DESTROY, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_NOTIFY_REQ: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;
const TRAY_ID: u32 = 1;
const VK_B: i32 = 0x42;
const MENU_SHOW: usize = 1;
const MENU_HIDE: usize = 2;
const MENU_QUIT: usize = 4;

/// UI-side operations the tray thread invokes. Only `Send` is required: the
/// boxed closures are moved onto the tray thread and called from its loop.
pub struct Actions {
    pub toggle: Box<dyn Fn() + Send>,
    pub show: Box<dyn Fn() + Send>,
    pub hide: Box<dyn Fn() + Send>,
    pub quit: Box<dyn Fn() + Send>,
}

/// State shared with `notify` callers on other threads.
struct Shared {
    hwnd: AtomicIsize,
    queue: Mutex<VecDeque<(String, String)>>,
}

impl Shared {
    fn push(&self, title: String, body: String) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back((title, body));
        }
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_NOTIFY_REQ, 0, 0);
            }
        }
    }
}

/// Handle to the running tray thread. Cheap to clone (`Arc`-wrapped by callers).
pub struct TrayHandle {
    shared: Arc<Shared>,
}

impl TrayHandle {
    /// Queue a balloon notification; safe from any thread.
    pub fn notify(&self, title: &str, body: &str) {
        self.shared.push(title.to_string(), body.to_string());
    }

    /// Ask the tray thread to remove the icon and exit its loop.
    pub fn shutdown(&self) {
        let hwnd = self.shared.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
    }
}

static GLOBAL: OnceLock<Arc<TrayHandle>> = OnceLock::new();
static STATE: AtomicIsize = AtomicIsize::new(0);

/// Publish the process-wide handle so [`notify`] can be called from anywhere.
pub fn set_global(handle: Arc<TrayHandle>) {
    let _ = GLOBAL.set(handle);
}

/// Send a balloon notification through the global handle, if the tray is up.
pub fn notify(title: &str, body: &str) {
    if let Some(handle) = GLOBAL.get() {
        handle.notify(title, body);
    }
}

/// Whether the one-time "hidden to tray" hint has already been queued.
static HIDDEN_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Queue the "hidden to the tray" hint, at most once per process.
///
/// The first hide explains where the window went and how to get it back;
/// every later hide stays silent, so the balloon never becomes noise. Does
/// nothing (and keeps the flag unset) while the tray is not running yet, so
/// an early call cannot swallow the hint.
pub fn notify_hidden_once() {
    if GLOBAL.get().is_none() {
        return;
    }
    if HIDDEN_NOTICE_SHOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    notify(
        "O11A1",
        "已隐藏在系统托盘，播放继续。双击托盘图标可恢复。",
    );
}

/// Tear the tray icon down through the global handle.
pub fn shutdown() {
    if let Some(handle) = GLOBAL.get() {
        handle.shutdown();
    }
}

/// Spawn the tray thread and return a handle immediately (does not block).
pub fn start(actions: Actions, tooltip: &str) -> Result<Arc<TrayHandle>, String> {
    let shared = Arc::new(Shared {
        hwnd: AtomicIsize::new(0),
        queue: Mutex::new(VecDeque::new()),
    });
    let handle = Arc::new(TrayHandle {
        shared: shared.clone(),
    });
    let tooltip = tooltip.to_string();
    std::thread::Builder::new()
        .name("bili-tray".into())
        .spawn(move || run_message_loop(actions, shared, tooltip))
        .map_err(|err| format!("无法启动托盘线程：{err}"))?;
    Ok(handle)
}

struct TrayState {
    hwnd: HWND,
    actions: Actions,
    shared: Arc<Shared>,
}

fn run_message_loop(actions: Actions, shared: Arc<Shared>, tooltip: String) {
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class_name = wide("BilibiliSlintTrayWindow");
    let wndclass = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    unsafe {
        RegisterClassW(&wndclass);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        crate::debug_log("tray: CreateWindowExW failed");
        return;
    }
    shared.hwnd.store(hwnd as isize, Ordering::SeqCst);

    let state = Box::new(TrayState {
        hwnd,
        actions,
        shared: shared.clone(),
    });
    STATE.store(Box::into_raw(state) as isize, Ordering::SeqCst);

    add_tray_icon(hwnd, &tooltip);
    let hotkey_ok = unsafe {
        RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, VK_B as u32) != 0
    };
    crate::debug_log(&format!("tray: hotkey Ctrl+Shift+B registered={hotkey_ok}"));

    if shared.queue.lock().map(|queue| !queue.is_empty()).unwrap_or(false) {
        unsafe {
            PostMessageW(hwnd, WM_NOTIFY_REQ, 0, 0);
        }
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        UnregisterHotKey(hwnd, HOTKEY_ID);
    }
    crate::debug_log("tray: message loop exited");
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = STATE.load(Ordering::SeqCst);
    if ptr == 0 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &*(ptr as *const TrayState);
    match msg {
        WM_HOTKEY => {
            (state.actions.toggle)();
            0
        }
        WM_TRAY => {
            match lparam as u32 {
                WM_RBUTTONUP => show_menu(state, hwnd),
                // Single left click and double click both bring the window
                // back, so the icon behaves like a normal taskbar entry. A
                // double click delivers LBUTTONUP first, which is harmless.
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => (state.actions.show)(),
                _ => {}
            }
            0
        }
        WM_NOTIFY_REQ => {
            drain_notifications(state);
            0
        }
        WM_CLOSE => {
            let data = base_icon(hwnd);
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &data);
            }
            unsafe {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn show_menu(state: &TrayState, hwnd: HWND) {
    let mut point = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut point);
    }
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    let show_label = wide("显示主窗口");
    let hide_label = wide("隐藏主窗口");
    let quit_label = wide("退出");
    unsafe {
        AppendMenuW(menu, MF_STRING, MENU_SHOW, show_label.as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_HIDE, hide_label.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit_label.as_ptr());
        SetForegroundWindow(hwnd);
    }
    let cmd = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        )
    };
    unsafe {
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
    match cmd as usize {
        MENU_SHOW => (state.actions.show)(),
        MENU_HIDE => (state.actions.hide)(),
        MENU_QUIT => (state.actions.quit)(),
        _ => {}
    }
}

fn drain_notifications(state: &TrayState) {
    let items: Vec<(String, String)> = match state.shared.queue.lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    for (title, body) in items {
        let data = balloon_data(state.hwnd, &title, &body);
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
}

fn add_tray_icon(hwnd: HWND, tooltip: &str) {
    let mut data = base_icon(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    // f178: our own icon, embedded in the .exe by `build.rs` (winres) with
    // resource id 1. Loading it by id keeps the tray identical to the taskbar /
    // Explorer icon without shipping a second file; the generic application
    // icon stays as a fallback so a stripped build still gets a tray entry.
    data.hIcon = {
        let module = unsafe { kernel32::GetModuleHandleW(std::ptr::null()) };
        let loaded = unsafe {
            LoadImageW(
                module,
                1 as *const u16,
                IMAGE_ICON,
                0,
                0,
                LR_DEFAULTSIZE | LR_SHARED,
            )
        };
        if loaded.is_null() {
            unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) }
        } else {
            loaded as HICON
        }
    };
    fill_wide(&mut data.szTip, tooltip);
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };

    let mut identifier: NOTIFYICONIDENTIFIER = unsafe { std::mem::zeroed() };
    identifier.cbSize = std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32;
    identifier.hWnd = hwnd;
    identifier.uID = TRAY_ID;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let hr = unsafe { Shell_NotifyIconGetRect(&identifier, &mut rect) };
    crate::debug_log(&format!(
        "tray: shell_add={added} get_rect_hr={hr} rect=({},{})-({},{})",
        rect.left, rect.top, rect.right, rect.bottom
    ));
}

fn base_icon(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    data
}

fn balloon_data(hwnd: HWND, title: &str, body: &str) -> NOTIFYICONDATAW {
    let mut data = base_icon(hwnd);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_INFO;
    fill_wide(&mut data.szInfoTitle, title);
    fill_wide(&mut data.szInfo, body);
    data
}

fn wide(text: &str) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    buffer.push(0);
    buffer
}

fn fill_wide<const N: usize>(dst: &mut [u16; N], text: &str) {
    for slot in dst.iter_mut() {
        *slot = 0;
    }
    for (index, unit) in text.encode_utf16().take(N.saturating_sub(1)).enumerate() {
        dst[index] = unit;
    }
}
