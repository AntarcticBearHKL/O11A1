//! Minimal Win32 window helpers.
//!
//! Three jobs: flip `WS_EX_TOPMOST` on a Slint window's native `HWND` so the
//! main player and the mini (PiP) window can stay above everything else; apply
//! the frameless shape + the compositor alpha that lets the desktop show through
//! the main window (the Gaussian frost itself is drawn in-process by
//! `glass.rs`); and run a `SetWindowSubclass` hook (`install_subclass`) so the
//! non-client area never exists on a frameless window — no caption to
//! re-appear and no native frame to flash during drag / maximise / DPI changes.
//! `WS_THICKFRAME` is kept, but `WM_NCHITTEST` reports `HTCLIENT` for the whole
//! window: moving and resizing are app-driven ([`WindowDrag`], [`WindowResize`])
//! so neither enters an OS modal loop. Engine-provided edge snapping
//! (`HTLEFT`/`HTTOPRIGHT`/...) is therefore given up by design.
//!
//! A fourth, unrelated job lives at the end of the file: the per-process audio
//! session fader ([`audio_session_volume`]) that moves `ffplay`'s WASAPI
//! session volume inside the mixer, so a volume change no longer restarts the
//! player.
//!
//! The handle is obtained through Slint's `raw-window-handle` integration, so no
//! window lookup by title (which would be ambiguous) is needed. The window
//! functions run on the UI thread (Slint's event-loop thread); the audio-session
//! functions are callable from any thread. Nothing blocks and nothing panics.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Media::Audio::{eConsole, eRender};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// `SetWindowPos` insert-after sentinels (winuser.h).
const HWND_TOPMOST: isize = -1;
const HWND_NOTOPMOST: isize = -2;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_FRAMECHANGED: u32 = 0x0020;
const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOPMOST: isize = 0x0000_0008;
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const SW_MINIMIZE: i32 = 6;
const SW_RESTORE: i32 = 9;

// Window style bits (winuser.h).
const GWL_STYLE: i32 = -16;
const WS_CAPTION: isize = 0x00C0_0000;
const WS_DLGFRAME: isize = 0x0040_0000;
const WS_BORDER: isize = 0x0080_0000;
const WS_THICKFRAME: isize = 0x0004_0000;

// Extended window style bits (winuser.h). These are the four non-client edge
// treatments: a raised outer edge (`WS_EX_WINDOWEDGE`), a sunken inner edge
// (`WS_EX_CLIENTEDGE`), a 3-D static edge (`WS_EX_STATICEDGE`) and the modal
// dialog frame (`WS_EX_DLGMODALFRAME`). A frameless window must carry none of
// them — `WS_EX_WINDOWEDGE` in particular is what leaves a 1px raised rim that
// reads as an incomplete title bar at the top edge.
const WS_EX_DLGMODALFRAME: isize = 0x0000_0001;
const WS_EX_WINDOWEDGE: isize = 0x0000_0100;
const WS_EX_CLIENTEDGE: isize = 0x0000_0200;
const WS_EX_STATICEDGE: isize = 0x0002_0000;

// Thread-local `WH_CBT` hook codes (winuser.h): the create-notification hook
// is what lets us strip the decoration bits from `CREATESTRUCTW` BEFORE the
// window exists, so the native title bar is never painted at all.
const WH_CBT: i32 = 5;
const HCBT_CREATEWND: i32 = 3;

// Window messages the subclass hook handles (winuser.h).
/// Sent after a window has moved; the wallpaper parallax hook hangs off it.
const WM_MOVE: u32 = 0x0003;
const WM_CLOSE: u32 = 0x0010;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_STYLECHANGING: u32 = 0x007C;
const WM_STYLECHANGED: u32 = 0x007D;
const WM_NCDESTROY: u32 = 0x0082;
const WM_NCCALCSIZE: u32 = 0x0083;
const WM_NCHITTEST: u32 = 0x0084;
// Non-client paint/activation messages. `WM_NCPAINT` paints the (now
// zero-sized) non-client area; `WM_NCACTIVATE` is delivered on every
// activation and deactivation change and its default handler repaints the
// caption/frame — the exact path that makes a title bar appear when another
// window is focused.
const WM_NCPAINT: u32 = 0x0085;
const WM_NCACTIVATE: u32 = 0x0086;
// Undocumented theme/DWM messages (winuser.h names them
// `WM_NCUAHDRAWCAPTION` / `WM_NCUAHDRAWFRAME`). uxtheme/DWM sends these to
// draw a themed caption / glass frame treatment; a frameless window must
// swallow them or Windows paints a fake title bar over the client area
// (this is the primary cause of "a title bar appears when I focus another
// window").
const WM_NCUAHDRAWCAPTION: u32 = 0x00AE;
const WM_NCUAHDRAWFRAME: u32 = 0x00AF;

// `WM_NCHITTEST` return code (winuser.h). Only `HTCLIENT` is ever returned:
// every press must reach Slint, which drives the app's own move/resize.
const HTCLIENT: isize = 1;

/// `uIdSubclass` for our hook; exactly one per window.
const SUBCLASS_ID: usize = 0x8181;
const MONITOR_DEFAULTTONEAREST: u32 = 2;

// DWM window attributes (dwmapi.h). `DWMWA_USE_IMMERSIVE_DARK_MODE` and
// `DWMWA_WINDOW_CORNER_PREFERENCE` are Windows 11 22000+; on older builds
// `DwmSetWindowAttribute` fails and the caller ignores it.
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWA_BORDER_COLOR: u32 = 34;
const DWMWCP_ROUND: u32 = 2;
/// `DWMWA_COLOR_NONE` (dwmapi.h): telling DWM the window border colour is
/// "none" removes the 1px border DWM draws inside the window edge on
/// Windows 11. Pre-Win11 builds reject `DWMWA_BORDER_COLOR` and the call just
/// fails; the caller ignores the failure.
const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
/// `DWM_BB_ENABLE` (dwmapi.h): the `enable` field of `DWM_BLURBEHIND` is set.
const DWM_BB_ENABLE: u32 = 0x0000_0001;

/// `DWM_BLURBEHIND` (dwmapi.h) for the legacy blur-behind call.
///
/// With `DWM_BB_ENABLE` set and a null `region`, DWM applies the whole-window
/// blur-behind treatment, which on this stack is what makes the compositor
/// honour the window alpha - the desktop shows through. It does NOT blur: the
/// Gaussian frost is `glass.rs`'s in-process snapshot.
#[repr(C)]
struct BlurBehind {
    flags: u32,
    enable: i32,
    region: isize,
    transition_on_maximized: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

/// `MINMAXINFO` (winuser.h), filled in for `WM_GETMINMAXINFO`.
#[repr(C)]
struct MinMaxInfo {
    pt_reserved: Point,
    pt_max_size: Point,
    pt_max_position: Point,
    pt_min_track_size: Point,
    pt_max_track_size: Point,
}

/// `RECT` (windef.h).
#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `STYLESTRUCT` (winuser.h) as delivered by `WM_STYLECHANGING` / `-CHANGED`.
#[repr(C)]
struct StyleStruct {
    style_old: u32,
    style_new: u32,
}

/// `CREATESTRUCTW` (winuser.h) as delivered in `CBT_CREATEWNDW`. Field order is
/// ABI-significant: editing `style` / `dw_ex_style` here changes the window that
/// is about to be created.
#[repr(C)]
struct CreateStructW {
    lp_create_params: *mut core::ffi::c_void,
    h_instance: isize,
    h_menu: isize,
    hwnd_parent: isize,
    cy: i32,
    cx: i32,
    y: i32,
    x: i32,
    style: i32,
    lpsz_name: *const u16,
    lpsz_class: *const u16,
    dw_ex_style: u32,
}

/// `CBT_CREATEWNDW` (winuser.h), the `lparam` of an `HCBT_CREATEWND` callback.
#[repr(C)]
struct CbtCreateWnd {
    lpcs: *mut CreateStructW,
    hwnd_insert_after: isize,
}

/// `MONITORINFO` (winuser.h).
#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: Rect,
    rc_work: Rect,
    dw_flags: u32,
}

/// `SUBCLASSPROC` signature from comctl32.
type SubclassProc =
    unsafe extern "system" fn(isize, u32, usize, isize, usize, usize) -> isize;

/// `HOOKPROC` signature (winuser.h).
type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;

/// Windows we have already subclassed, so repeated (watchdog) calls are cheap
/// and we never double-install the hook.
static SUBCLASSED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// Windows whose DWM attributes (rounded corner, dark chrome, no border) have
/// already been applied. A window can be subclassed by the `WH_CBT` hook before
/// `CreateWindowExW` returns, i.e. before DWM will accept those attributes, so
/// "subclass exists" and "glass applied" are two different facts.
static GLASS_READY: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// The process-lifetime `WH_CBT` hook. Never unhooked: the player and mini
/// windows are created long after `main` starts and must be born frameless too.
static CREATE_HOOK: AtomicIsize = AtomicIsize::new(0);

/// The window that owns the enforced minimum track size, plus that size. Only
/// the main window installs a minimum; the player and the mini stay freely
/// resizable down to whatever the user picks.
static MIN_TRACK_HWND: AtomicIsize = AtomicIsize::new(0);
static MIN_TRACK_W: AtomicI32 = AtomicI32::new(0);
static MIN_TRACK_H: AtomicI32 = AtomicI32::new(0);

/// Height (physical px) of the main window's old native top drag strip.
///
/// Since `WM_NCHITTEST` now answers `HTCLIENT` everywhere this value no longer
/// changes any hit test; the startup call is kept so the strip's measured height
/// stays in one place and a future native special-case can read it again.
static TOP_DRAG_STRIP: AtomicI32 = AtomicI32::new(0);

/// The main window's `WM_CLOSE` interception: `(hwnd, hook)`.
///
/// `None` (or a different `hwnd`) means the normal `DefSubclassProc` close
/// behaviour applies, which is what the player and mini windows must keep.
/// The hook is `Arc`-cloned out of the guard before it runs, because it calls
/// back into Slint and must never execute under this lock.
static CLOSE_HOOK: Mutex<Option<(isize, Arc<dyn Fn() + Send + Sync>)>> = Mutex::new(None);

/// Per-window `WM_MOVE` observers, keyed by handle.
///
/// The wallpaper is anchored to the window's SCREEN position, so every native
/// move - the frameless drag path, a snap, a maximize, a programmatic
/// `set_position` - has to republish the tile origin. Nothing here runs on a
/// timer: `WM_MOVE` is the only trigger, which is why an idle window costs
/// nothing. The hook is cloned out of the lock before it runs, exactly like
/// [`CLOSE_HOOK`], because it re-enters Slint.
static MOVE_HOOKS: Mutex<Vec<(isize, Arc<dyn Fn() + Send + Sync>)>> = Mutex::new(Vec::new());

#[link(name = "user32")]
extern "system" {
    fn SetWindowPos(
        hwnd: isize,
        insert_after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;

    fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;

    fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;

    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;

    fn IsWindowVisible(hwnd: isize) -> i32;

    fn IsZoomed(hwnd: isize) -> i32;

    fn IsIconic(hwnd: isize) -> i32;

    fn SetForegroundWindow(hwnd: isize) -> i32;

    fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;

    fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;

    fn GetCursorPos(point: *mut Point) -> i32;

    fn GetAsyncKeyState(vkey: i32) -> i16;

    fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;

    fn MonitorFromRect(rect: *const Rect, flags: u32) -> isize;

    fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;

    fn SetWindowsHookExW(id_hook: i32, lpfn: HookProc, h_mod: isize, dw_thread_id: u32) -> isize;

    fn UnhookWindowsHookEx(hhk: isize) -> i32;

    fn CallNextHookEx(hhk: isize, code: i32, wparam: usize, lparam: isize) -> isize;

    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: isize, attribute: u32, value: *const u8, size: u32) -> i32;

    fn DwmEnableBlurBehindWindow(hwnd: isize, data: *const BlurBehind) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;

    fn GetCurrentThread() -> isize;

    fn GetCurrentProcess() -> isize;

    fn SetThreadPriority(thread: isize, priority: i32) -> i32;

    fn SetPriorityClass(process: isize, class: u32) -> i32;
}

/// `THREAD_PRIORITY_HIGHEST` (winbase.h).
const THREAD_PRIORITY_HIGHEST: i32 = 2;
/// `BELOW_NORMAL_PRIORITY_CLASS` (winbase.h).
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Hand the machine back to the rest of the desktop, and keep our own UI on top
/// of our own workers.
///
/// The app used to run at `NORMAL_PRIORITY_CLASS` with every thread at normal
/// priority: 16 network workers, three decode threads, the fish worker and the
/// Slint event loop all competed as equals, so a playing video could starve the
/// window that draws it *and* every other application on the machine. Setting
/// the **process** class below normal fixes the second half (any other app wins
/// against us), and raising the **UI thread** inside that class fixes the first
/// (the event loop always wins against our network/decode threads, which stay at
/// the class default, i.e. below normal in absolute terms).
///
/// Call once, from the thread that runs the Slint event loop.
pub fn tune_priorities() {
    unsafe {
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

#[link(name = "comctl32")]
extern "system" {
    fn SetWindowSubclass(hwnd: isize, proc_: SubclassProc, id: usize, ref_data: usize) -> i32;

    fn DefSubclassProc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;

    fn RemoveWindowSubclass(hwnd: isize, proc_: SubclassProc, id: usize) -> i32;
}

/// Native `HWND` of a Slint window, or `None` on a non-Win32/unsupported backend.
///
/// Shared with the glass loop in `main.rs`, which hands the handle to
/// `glass::Glass` for its GDI screen grab.
pub(crate) fn hwnd_of(window: &slint::Window) -> Option<isize> {
    let handle = window.window_handle();
    let raw = HasWindowHandle::window_handle(&handle).ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

/// Low word of an `LPARAM`, sign-extended (`GET_X_LPARAM`).
fn loword_signed(value: isize) -> i32 {
    (value as u16 as i16) as i32
}

/// High word of an `LPARAM`, sign-extended (`GET_Y_LPARAM`).
fn hiword_signed(value: isize) -> i32 {
    ((value >> 16) as u16 as i16) as i32
}

/// `WM_NCHITTEST` answer for a frameless window: always `HTCLIENT`.
///
/// The 8px rim used to report the eight native resize codes (`HTLEFT`,
/// `HTTOPLEFT`, ...), which made Windows enter its MODAL resize loop: that loop
/// owns the thread's message pump until the button is released, so the app's
/// timer-driven video pacer and every Slint timer froze for the whole gesture.
/// The rim is now client area instead, and `ui/app.slint` draws its own 8px hot
/// zones that call back into [`WindowResize`]; placement happens on the same
/// 8ms frame timer as [`WindowDrag`], so playback keeps running while the user
/// resizes.
///
/// What is lost by design: Windows' own edge snapping (Aero snap when a border
/// is dragged to a screen edge), the system resize cursor, and the caption
/// double-click. The seven-argument `HTMAXBUTTON`/`HTTRANSPARENT` forms do not
/// apply here either - this window has no non-client area at all
/// (`WM_NCCALCSIZE` returns 0).
///
/// Kept as a pure function (and the width/height/position parameters with it)
/// so the unit tests pin the contract and the `WM_NCHITTEST` path keeps one
/// obvious seam.
fn border_hit_test(x: i32, y: i32, width: i32, height: i32) -> isize {
    let _ = (x, y, width, height);
    HTCLIENT
}

/// Apply our minimum track size (main window only) and clamp the maximised
/// bounds to the monitor's work area so a frameless window never covers the
/// taskbar. Called after the original procedure has filled in its defaults.
unsafe fn apply_min_max(hwnd: isize, lparam: isize) {
    if lparam == 0 {
        return;
    }
    let info = lparam as *mut MinMaxInfo;
    let min_hwnd = MIN_TRACK_HWND.load(Ordering::Relaxed);
    if min_hwnd != 0 && min_hwnd == hwnd {
        let min_w = MIN_TRACK_W.load(Ordering::Relaxed);
        let min_h = MIN_TRACK_H.load(Ordering::Relaxed);
        if min_w > 0 {
            (*info).pt_min_track_size.x = min_w;
        }
        if min_h > 0 {
            (*info).pt_min_track_size.y = min_h;
        }
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor != 0 {
        let mut monitor_info = MonitorInfo {
            cb_size: std::mem::size_of::<MonitorInfo>() as u32,
            rc_monitor: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rc_work: Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dw_flags: 0,
        };
        if GetMonitorInfoW(monitor, &mut monitor_info) != 0 {
            (*info).pt_max_position.x = monitor_info.rc_work.left;
            (*info).pt_max_position.y = monitor_info.rc_work.top;
            (*info).pt_max_size.x = monitor_info.rc_work.right - monitor_info.rc_work.left;
            (*info).pt_max_size.y = monitor_info.rc_work.bottom - monitor_info.rc_work.top;
        }
    }
}

/// Force the window into the frameless-but-resizable shape.
///
/// Removes `WS_CAPTION` / `WS_DLGFRAME` / `WS_BORDER` from `GWL_STYLE` and the
/// three clearable non-client edge styles (`WS_EX_CLIENTEDGE`,
/// `WS_EX_STATICEDGE`, `WS_EX_DLGMODALFRAME`) from `GWL_EXSTYLE`. `WS_THICKFRAME`
/// is kept (and re-added if a stale instance dropped it): it is part of the
/// measured winit style and removing it would change the window's layering, but
/// it no longer drives resizing - the app's own rim does.
///
/// `WS_EX_WINDOWEDGE` is written out too (a raised 1px outer edge) but USER32
/// re-adds it to every top-level window and rejects its removal — verified on
/// this host with a bare `CreateWindowExW` (created ex `0x100`, cleared back to
/// `0x100`, and `WS_POPUP` does not help). It is therefore excluded from the
/// change test: including it would make this function non-idempotent and its
/// `SetWindowLongPtrW`+`SetWindowPos` pair would run (and log) every tick. It is
/// also visually inert once the non-client area is collapsed (`WM_NCCALCSIZE`
/// returns 0 and `WM_NCPAINT` is swallowed), so leaving it set draws nothing.
///
/// Idempotent and cheap: when the style words already have the desired value it
/// issues no `SetWindowLongPtrW`/`SetWindowPos` call at all, so the per-tick
/// watchdog can call it unconditionally. The `SWP_FRAMECHANGED` flag makes the
/// window manager recompute the (empty) frame after a change.
fn normalise_frameless(hwnd: isize) {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    let want_style = (style & !(WS_CAPTION | WS_DLGFRAME | WS_BORDER)) | WS_THICKFRAME;
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let want_ex = ex_style
        & !(WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_DLGMODALFRAME);
    let ex_needs_change =
        ex_style & (WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_DLGMODALFRAME) != 0;
    if want_style == style && !ex_needs_change {
        return;
    }
    crate::debug_log(&format!(
        "normalise hwnd={hwnd} style 0x{style:08X}->0x{want_style:08X} \
         exstyle 0x{ex_style:08X}->0x{want_ex:08X}"
    ));
    unsafe {
        if want_style != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, want_style);
        }
        if want_ex != ex_style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want_ex);
        }
        SetWindowPos(
            hwnd,
            0,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Re-assert the frameless style on a Slint window (no-op without a handle).
///
/// Public entry point for the startup/self-healing watchdog in `main.rs`.
pub fn reassert_frameless(window: &slint::Window) {
    if let Some(hwnd) = hwnd_of(window) {
        normalise_frameless(hwnd);
    }
}

/// Make a window frameless synchronously, right before it becomes visible.
///
/// Single-call guard for the player and mini windows so neither is ever shown
/// decorated: resolves the native handle, installs the non-client subclass (so
/// any later style change — including winit re-applying `WS_CAPTION` after
/// `show()` — is filtered by `WM_STYLECHANGING`) and re-asserts the frameless
/// style (`normalise_frameless`). Returns whether the native handle existed:
/// `false` means the window is created by `show()` itself, and the caller must
/// repeat the call on the very next statement after `show()`.
pub fn prepare_frameless(window: &slint::Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    install_subclass(window);
    normalise_frameless(hwnd);
    true
}

/// The window subclass that makes the non-client area never exist.
///
/// * `WM_NCCALCSIZE` returns 0 for BOTH branches: with `wparam != 0` the
///   `NCCALCSIZE_PARAMS` rectangle is kept as proposed, and with `wparam == 0`
///   the proposed window rect in `lparam` is left untouched. In both cases the
///   client area ends up equal to the whole window, so there is no caption or
///   frame to re-appear and no reserved non-client strip to leave unpainted.
/// * `WM_STYLECHANGING` strips `WS_CAPTION`/`WS_DLGFRAME`/`WS_BORDER` from the
///   incoming style so the caption can never be re-added.
/// * `WM_STYLECHANGED` re-asserts the frameless style after the change was
///   applied, in case the change did not go through our `-ING` filter.
/// * `WM_NCACTIVATE`/`WM_NCPAINT`/`WM_NCUAHDRAWCAPTION`/`WM_NCUAHDRAWFRAME`
///   are swallowed so the system never paints a caption, frame or activation
///   treatment (this is what a frameless window must do to stop a title bar
///   flashing in when focus moves).
/// * `WM_NCHITTEST` reports `HTCLIENT` for every point, the 8px rim included,
///   so a rim press reaches Slint's own resize hot zones and never enters the
///   OS modal resize loop. Native edge snapping is lost by design.
/// * `WM_GETMINMAXINFO` keeps the main window's minimum size and the work-area
///   maximised bounds.
/// * `WM_CLOSE` is intercepted ONLY for the window a hook was registered for
///   through [`set_close_hook`]: the hook runs and the window stays alive
///   (returning 0 suppresses the default destroy). The player and mini windows
///   have no registered hook and keep their normal close behaviour.
unsafe extern "system" fn subclass_proc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _data: usize,
) -> isize {
    match msg {
        // Client area == whole window for both the "proposed parameters" and
        // the plain window-rect form. Falling through to DefWindowProc here
        // would compute the standard caption + border frame.
        WM_NCCALCSIZE => return 0,
        // Claim every activation change as handled WITHOUT calling
        // DefSubclassProc: the default handler repaints the caption/frame on
        // both activate and deactivate, which is exactly the native title bar
        // that otherwise flashes in when another window is focused. Returning
        // TRUE (1, not the isize-1 that would be -1) says "no repaint needed".
        WM_NCACTIVATE => {
            normalise_frameless(hwnd);
            return 1;
        }
        // There is no non-client area to paint (see WM_NCCALCSIZE).
        WM_NCPAINT => return 0,
        // Undocumented theme/DWM caption and frame treatments; swallow them or
        // Windows draws a fake title bar over the client area.
        WM_NCUAHDRAWCAPTION => return 0,
        WM_NCUAHDRAWFRAME => return 0,
        WM_STYLECHANGING if lparam != 0 && wparam as i32 == GWL_STYLE => {
            let style = lparam as *mut StyleStruct;
            (*style).style_new &= !((WS_CAPTION | WS_DLGFRAME | WS_BORDER) as u32);
        }
        // Strip the clearable non-client edge styles from any incoming
        // extended-style change. (USER32 still re-adds WS_EX_WINDOWEDGE on its
        // own for a top-level window, which is why `normalise_frameless`
        // tolerates that one bit — but the other three must not be applied.)
        WM_STYLECHANGING if lparam != 0 && wparam as i32 == GWL_EXSTYLE => {
            let style = lparam as *mut StyleStruct;
            (*style).style_new &= !((WS_EX_WINDOWEDGE
                | WS_EX_CLIENTEDGE
                | WS_EX_STATICEDGE
                | WS_EX_DLGMODALFRAME) as u32);
        }
        // Re-assert after the change has been applied: a caller that bypasses
        // the -ING filter (or a stale instance) must not be able to leave the
        // window decorated.
        WM_STYLECHANGED => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            normalise_frameless(hwnd);
            return result;
        }
        WM_NCHITTEST => {
            // A maximised frameless window must not offer resize hot zones
            // either (the UI hides/greys them, but the client answer would be
            // the same anyway).
            if IsZoomed(hwnd) != 0 {
                return HTCLIENT;
            }
            let mut rect = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd, &mut rect) != 0 {
                let width = rect.right - rect.left;
                let height = rect.bottom - rect.top;
                let x = loword_signed(lparam) - rect.left;
                let y = hiword_signed(lparam) - rect.top;
                return border_hit_test(x, y, width, height);
            }
        }
        WM_GETMINMAXINFO => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            apply_min_max(hwnd, lparam);
            return result;
        }
        // Close interception, scoped to the one window that registered a hook.
        // The `Arc` is cloned out and the guard dropped BEFORE the call: the
        // hook runs on this (UI) thread and re-enters Slint.
        WM_CLOSE => {
            let hook = match CLOSE_HOOK.lock() {
                Ok(guard) => match guard.as_ref() {
                    Some((close_hwnd, hook)) if *close_hwnd == hwnd => Some(Arc::clone(hook)),
                    _ => None,
                },
                Err(_) => None,
            };
            if let Some(hook) = hook {
                hook();
                return 0;
            }
        }
        // Wallpaper parallax: the moved window republishes its tile origin.
        // Falling through (no `return`) keeps the default move bookkeeping.
        WM_MOVE => {
            let hook = MOVE_HOOKS.lock().ok().and_then(|hooks| {
                hooks
                    .iter()
                    .find(|(move_hwnd, _)| *move_hwnd == hwnd)
                    .map(|(_, hook)| Arc::clone(hook))
            });
            if let Some(hook) = hook {
                hook();
            }
        }
        WM_NCDESTROY => {
            if let Ok(mut slot) = CLOSE_HOOK.lock() {
                if matches!(slot.as_ref(), Some((close_hwnd, _)) if *close_hwnd == hwnd) {
                    *slot = None;
                }
            }
            if let Ok(mut hooks) = MOVE_HOOKS.lock() {
                hooks.retain(|(move_hwnd, _)| *move_hwnd != hwnd);
            }
            if let Ok(mut set) = SUBCLASSED.lock() {
                set.retain(|entry| *entry != hwnd);
            }
            RemoveWindowSubclass(hwnd, subclass_proc as SubclassProc, SUBCLASS_ID);
            return DefSubclassProc(hwnd, msg, wparam, lparam);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Strip every caption/frame bit from a pending `CREATESTRUCTW` style pair.
///
/// Pure so the exact measured winit style (`0x16CF0000`, decorated + visible)
/// is pinned by a unit test: the create-time hook must turn it into
/// `0x160F0000` before Windows ever builds the window.
fn strip_create_styles(style: i32, ex_style: u32) -> (i32, u32) {
    (
        style & !((WS_CAPTION | WS_DLGFRAME | WS_BORDER) as i32),
        ex_style
            & !((WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_DLGMODALFRAME)
                as u32),
    )
}

/// `WH_CBT` hook: make every window this thread creates frameless at birth.
///
/// Runs inside `CreateWindowExW`, before the window object exists, so the
/// window never has a non-client area and no caption frame is ever composited.
/// The style edit is the documented `HCBT_CREATEWND` contract; everything is
/// passed on to the next hook unconditionally.
unsafe extern "system" fn cbt_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HCBT_CREATEWND && lparam != 0 {
        let create = &mut *(lparam as *mut CbtCreateWnd);
        if !create.lpcs.is_null() {
            let cs = &mut *create.lpcs;
            let raw_style = cs.style;
            let (style, ex_style) = strip_create_styles(cs.style, cs.dw_ex_style);
            if style != cs.style || ex_style != cs.dw_ex_style {
                crate::debug_log(&format!(
                    "create-hook hwnd={wparam} style 0x{:08X}->0x{:08X} \
                     exstyle 0x{:08X}->0x{:08X}",
                    cs.style as u32, style as u32, cs.dw_ex_style, ex_style
                ));
                cs.style = style;
                cs.dw_ex_style = ex_style;
            }
            // Any window this thread creates that carries a native caption or a
            // resize frame is one of the app's Slint windows; the tray's helper
            // window is created with style 0 and is left alone. The caption bit
            // is what identifies the player/mini windows: winit seeds
            // `WS_CAPTION` on every top-level window but only adds
            // `WS_SIZEBOX` in a post-create update, so a `WS_THICKFRAME`-only
            // test misses them. The subclass must exist before that update so
            // its `WS_CAPTION` re-application is filtered by `WM_STYLECHANGING`.
            let app_window = raw_style & ((WS_CAPTION | WS_THICKFRAME) as i32) != 0;
            if app_window && wparam != 0 {
                install_subclass_hwnd(wparam as isize, false);
            }
        }
    }
    CallNextHookEx(0, code, wparam, lparam)
}

/// Install the process-lifetime `WH_CBT` create hook on the current thread.
///
/// Must run before the first Slint window is created, on the thread that will
/// create it. Idempotent; a failure is logged and left non-fatal so the
/// continue-frameless layers (subclass + watchdog) still apply.
pub fn install_create_hook() {
    // Diagnostic: `BILI_AUDIO_SESSION_TEST` starts its own thread here, once,
    // before the first window exists (it needs no window at all).
    maybe_start_audio_session_selftest();
    if CREATE_HOOK.load(Ordering::Relaxed) != 0 {
        return;
    }
    let tid = unsafe { GetCurrentThreadId() };
    let hook = unsafe { SetWindowsHookExW(WH_CBT, cbt_proc, 0, tid) };
    if hook == 0 {
        crate::debug_log(&format!("create-hook install failed tid={tid}"));
        return;
    }
    CREATE_HOOK.store(hook, Ordering::Relaxed);
    crate::debug_log(&format!("create-hook installed hhook={hook} tid={tid}"));
}

/// Release the create hook on normal shutdown.
///
/// Idempotent and a no-op when installation failed. Kept separate from
/// `install_create_hook` because the hook must survive for the whole event
/// loop: player and mini windows are created long after startup.
pub fn unhook_create_hook() {
    let hook = CREATE_HOOK.swap(0, Ordering::Relaxed);
    if hook != 0 {
        unsafe { UnhookWindowsHookEx(hook) };
    }
}

/// Install the frameless subclass hook on a window's native handle.
///
/// Idempotent and best-effort: repeated (watchdog) calls are cheap, and a window
/// whose handle does not exist yet simply returns `false` so the caller can
/// retry on a later event-loop turn. The style is re-normalised on EVERY call
/// (not only the first) so this doubles as the self-healing path: from the
/// watchdog, any re-decoration is reverted within one tick. Must be called on
/// the window's own thread (Slint's event loop).
pub fn install_subclass(window: &slint::Window) -> bool {
    match hwnd_of(window) {
        Some(hwnd) => install_subclass_hwnd(hwnd, true),
        None => false,
    }
}

/// Subclass one raw handle and re-normalise its style.
///
/// Split out of `install_subclass` so the `WH_CBT` create hook can book a window
/// in *before* `CreateWindowExW` returns: winit re-applies its decorated style
/// immediately after creation, and only a subclass that already exists can
/// filter that `WM_STYLECHANGING` message. `remove_border` is false on the
/// creation path because DWM rejects attribute writes before the window is
/// composited; `setup_glass` removes the border later anyway.
fn install_subclass_hwnd(hwnd: isize, remove_border: bool) -> bool {
    let already = match SUBCLASSED.lock() {
        Ok(set) => set.contains(&hwnd),
        Err(_) => false,
    };
    if !already {
        let proc_ = subclass_proc as SubclassProc;
        let ok = unsafe { SetWindowSubclass(hwnd, proc_, SUBCLASS_ID, 0) };
        if ok == 0 {
            crate::debug_log(&format!("subclass install failed hwnd={hwnd}"));
            return false;
        }
        if let Ok(mut set) = SUBCLASSED.lock() {
            set.push(hwnd);
        }
        let proc_addr = proc_ as usize;
        crate::debug_log(&format!("subclass installed hwnd={hwnd} proc={proc_addr:#x}"));
        if remove_border {
            // Once per window: drop the 1px Win11 DWM border (best-effort, fails
            // harmlessly on older builds). Covers main, player and mini, which
            // are all booked in through this function.
            let _ = dwm_remove_border(hwnd);
        }
        normalise_frameless(hwnd);
        // Report "installed" only on the installation itself. The player's
        // 1 Hz retry timer used to re-run `normalise_frameless` + three DWM
        // attribute writes + a frame-state log **every second for every open
        // window**, which showed up as 0.2-1.0 s UI hitches during playback.
        // Once the subclass exists, `WM_STYLECHANGING` keeps winit from
        // re-decorating, so the retry only has to wait for the HWND.
        return true;
    }
    // Already subclassed (possibly by the create hook, before DWM accepts
    // attributes). Ask the caller once more so `setup_glass` actually runs.
    !glass_ready(hwnd)
}

fn glass_ready(hwnd: isize) -> bool {
    GLASS_READY
        .lock()
        .map(|set| set.contains(&hwnd))
        .unwrap_or(false)
}

/// Raise/lower the given window in the Z-order by toggling `WS_EX_TOPMOST`.
///
/// Returns `true` when the native call succeeded. It is deliberately
/// best-effort: an unsupported backend (or a window not created yet) simply
/// yields `false` and playback is unaffected.
pub fn set_topmost(window: &slint::Window, on: bool) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    let insert_after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    let result = unsafe {
        SetWindowPos(
            hwnd,
            insert_after,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    result != 0
}

/// Whether the window currently carries `WS_EX_TOPMOST`, or `None` when the
/// native handle is not available yet (e.g. right after `show()`).
pub fn is_topmost(window: &slint::Window) -> Option<bool> {
    let hwnd = hwnd_of(window)?;
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    Some(style & WS_EX_TOPMOST != 0)
}

/// Show or hide the window through the raw Win32 `ShowWindow` call.
///
/// This deliberately bypasses Slint's own `show`/`hide` bookkeeping: the boss
/// key hides the main window while the Slint event loop must keep running, so
/// nothing is told to the framework. Showing goes through the same path as
/// [`restore`], so a hidden-and-previously-maximised window comes back
/// maximised instead of merely visible. Returns `true` when the native call
/// ran.
pub fn set_visible(window: &slint::Window, visible: bool) -> bool {
    if visible {
        return restore(window);
    }
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    true
}

/// Register the close hook for one window's native handle.
///
/// Records `hwnd` plus the hook in [`CLOSE_HOOK`]; from then on that window's
/// `WM_CLOSE` (✕, Alt+F4, the taskbar's 关闭) invokes the hook instead of
/// destroying the window. The hook runs on the UI thread and must therefore be
/// cheap and must not block — it typically reads the stored close preference
/// and calls [`minimize`] / [`set_visible`] / asks Slint to quit.
///
/// The subclass is installed (idempotently) so the hook actually fires. Returns
/// `false` when the native handle is not available yet, in which case the
/// caller should retry once the window exists.
pub fn set_close_hook(window: &slint::Window, hook: impl Fn() + Send + Sync + 'static) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    install_subclass_hwnd(hwnd, false);
    match CLOSE_HOOK.lock() {
        Ok(mut slot) => {
            *slot = Some((hwnd, Arc::new(hook)));
            crate::debug_log(&format!("close-hook registered hwnd={hwnd}"));
            true
        }
        Err(_) => false,
    }
}

/// Register an observer that runs after every native move (`WM_MOVE`).
///
/// One observer per window; registering again replaces the previous one. Purely
/// event-driven - the callback is the wallpaper republish, so no timer polls the
/// window position. Returns `false` when the native handle does not exist yet.
pub fn set_move_hook(window: &slint::Window, hook: impl Fn() + Send + Sync + 'static) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    install_subclass_hwnd(hwnd, false);
    match MOVE_HOOKS.lock() {
        Ok(mut hooks) => {
            let fresh = !hooks.iter().any(|(move_hwnd, _)| *move_hwnd == hwnd);
            hooks.retain(|(move_hwnd, _)| *move_hwnd != hwnd);
            hooks.push((hwnd, Arc::new(hook)));
            if fresh {
                crate::debug_log(&format!("move-hook registered hwnd={hwnd}"));
            }
            true
        }
        Err(_) => false,
    }
}

/// Post `WM_CLOSE` to a window, as the ✕ / Alt+F4 / taskbar close would.///
/// Diagnostic only (`BILI_PLAYER_CLOSE_TEST=native`): no synthetic input is
/// involved, the message goes straight to the window procedure. Best-effort.
pub fn post_close(hwnd: isize) -> bool {
    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) != 0 }
}

/// The window's outer rect in physical screen pixels: `(x, y, w, h)`.
pub fn window_rect(window: &slint::Window) -> Option<(i32, i32, i32, i32)> {
    let hwnd = hwnd_of(window)?;
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some((
        rect.left,
        rect.top,
        rect.right.saturating_sub(rect.left),
        rect.bottom.saturating_sub(rect.top),
    ))
}

/// The live cursor in physical screen pixels.
pub fn cursor_px() -> Option<(i32, i32)> {
    cursor_position().map(|point| (point.x, point.y))
}

/// `VK_LBUTTON` (winuser.h).
const VK_LBUTTON: i32 = 0x01;

/// Whether the physical left mouse button is currently held down.
///
/// A READ, never a write: no synthetic input is produced. Used to tell a real
/// pointer-up from the `cancel`/leave Slint emits when the window it is
/// dragging jumps out from under the cursor - a `SetWindowPos` on every frame
/// makes that happen constantly, and treating it as a release was why a drag
/// sometimes died after a few pixels and needed several attempts.
pub fn left_button_down() -> bool {
    // The high bit is the current physical state; the low bit would be
    // "pressed since the last call" and is deliberately not used.
    unsafe { GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000 != 0 }
}

/// Whether the cursor is anywhere inside the window's outer rect.
///
/// One `GetCursorPos` + one `GetWindowRect`: the 60Hz pointer tick behind
/// `pointer_inside` uses exactly this. `None` without a native handle.
pub fn cursor_inside(window: &slint::Window) -> Option<bool> {
    let (wx, wy, ww, wh) = window_rect(window)?;
    let (cx, cy) = cursor_px()?;
    Some(cx >= wx && cx < wx.saturating_add(ww) && cy >= wy && cy < wy.saturating_add(wh))
}

/// Work area (physical px, taskbar excluded) of the monitor a rect lands on:
/// `(x, y, w, h)`.
///
/// `MONITOR_DEFAULTTONEAREST` also answers for a rect that is off every
/// monitor, so the caller must still intersect the result with its own rect:
/// an empty intersection means "that monitor is gone".
pub fn work_area_for_rect(x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
    let rect = Rect {
        left: x,
        top: y,
        right: x.saturating_add(w),
        bottom: y.saturating_add(h),
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    if monitor == 0 {
        return None;
    }
    let mut info = MonitorInfo {
        cb_size: std::mem::size_of::<MonitorInfo>() as u32,
        rc_monitor: Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rc_work: Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dw_flags: 0,
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    Some((
        info.rc_work.left,
        info.rc_work.top,
        info.rc_work.right.saturating_sub(info.rc_work.left),
        info.rc_work.bottom.saturating_sub(info.rc_work.top),
    ))
}

/// Native `IsIconic` state, or `None` without a handle.
pub fn is_minimized(window: &slint::Window) -> Option<bool> {
    let hwnd = hwnd_of(window)?;
    Some(unsafe { IsIconic(hwnd) } != 0)
}

/// Minimise the window to the taskbar (`SW_MINIMIZE`).
///
/// Unlike [`set_visible`]`(false)` this keeps the taskbar button, so the window
/// can always be brought back the normal way. Returns `false` without a native
/// handle.
pub fn minimize(window: &slint::Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
    true
}

/// Bring the window back and to the foreground, PRESERVING its maximise state.
///
/// `SW_RESTORE` also un-maximises a maximised window, so it is used only when
/// the window is actually minimised (`IsIconic`); otherwise `SW_SHOW` makes a
/// hidden window visible without touching its placement. A window that was
/// maximised before it was hidden or minimised therefore returns maximised.
pub fn restore(window: &slint::Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    show_preserving_zoom(hwnd);
    true
}

/// Show `hwnd` as-is, restoring it only when it is minimised.
fn show_preserving_zoom(hwnd: isize) {
    let cmd = if unsafe { IsIconic(hwnd) } != 0 {
        SW_RESTORE
    } else {
        SW_SHOW
    };
    unsafe {
        ShowWindow(hwnd, cmd);
        SetForegroundWindow(hwnd);
    }
}

/// Native visibility (`IsWindowVisible`), or `None` without a handle.
pub fn is_visible(window: &slint::Window) -> Option<bool> {
    let hwnd = hwnd_of(window)?;
    Some(unsafe { IsWindowVisible(hwnd) } != 0)
}

/// Enforce the smallest size this window can be resized to (physical pixels).
///
/// Records the minimum and installs the subclass (which applies it from
/// `WM_GETMINMAXINFO`). Best-effort; returns `false` without a native handle.
pub fn set_min_track_size(window: &slint::Window, width: i32, height: i32) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    MIN_TRACK_HWND.store(hwnd, Ordering::Relaxed);
    MIN_TRACK_W.store(width, Ordering::Relaxed);
    MIN_TRACK_H.store(height, Ordering::Relaxed);
    let installed = install_subclass(window);
    crate::debug_log(&format!(
        "min-track hwnd={hwnd} want={width}x{height} subclass={installed}"
    ));
    installed
}

/// Declare the height (physical px) of the app's top drag strip.
///
/// The strip is implemented in `ui/app.slint` and needs no native support: the
/// whole client is `HTCLIENT` now, so the press reaches Slint's drag area anyway.
/// Kept as the one record of the strip's height (startup already calls it).
pub fn set_top_drag_strip(height_px: i32) {
    TOP_DRAG_STRIP.store(height_px.max(0), Ordering::Relaxed);
}

/// Apply the frameless + compositor-alpha treatment to a shown window.
///
/// Deliberately no DWM system backdrop: `DwmExtendFrameIntoClientArea` +
/// `DWMWA_SYSTEMBACKDROP_TYPE` move the window onto DWM's backdrop composition
/// path, which then composites OPAQUELY - and DWM also paints its caption band
/// across the top ~30px of the client, a hard horizontal edge that cannot be
/// recoloured or switched off. `DwmEnableBlurBehindWindow` with a null blur
/// region instead makes the whole window show what is behind it (the
/// compositor honours the window alpha), so the desktop genuinely shows
/// through. The Gaussian frost is NOT this call's doing - `glass.rs` draws the
/// blurred snapshot in-process and `ui/app.slint` paints it under the tint.
///
/// Every OS call is best-effort: an older Windows build or a window whose
/// handle is not ready yet simply fails and is logged, never panicking.
/// Returns `false` only when the native handle is not available yet, so the
/// caller can retry on a later event-loop turn.
pub fn setup_glass(window: &slint::Window, dark: bool) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };

    let blur = BlurBehind {
        flags: DWM_BB_ENABLE,
        enable: 1,
        region: 0,
        transition_on_maximized: 0,
    };
    let blur_hr = unsafe { DwmEnableBlurBehindWindow(hwnd, &blur) };

    // Rounded corners + dark DWM chrome, and no 1px border. `DWMWCP_ROUND` is
    // intentional design, not a Windows title-bar decoration, so only the
    // border colour is cleared.
    let corner_hr = dwm_set_u32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
    let dark_hr = dwm_set_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(dark));
    let border_hr = dwm_remove_border(hwnd);

    crate::debug_log(&format!(
        "glass hwnd={hwnd} blur_behind={blur_hr} corner_set={corner_hr} dark_set={dark_hr} \
         border_set={border_hr}"
    ));
    if let Ok(mut set) = GLASS_READY.lock() {
        if !set.contains(&hwnd) {
            set.push(hwnd);
        }
    }
    true
}

/// Flip `DWMWA_USE_IMMERSIVE_DARK_MODE` so the DWM chrome matches the theme.
pub fn set_immersive_dark(window: &slint::Window, dark: bool) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    dwm_set_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(dark)) == 0
}

/// Log the native frame style: whether `WS_CAPTION` is gone and `WS_THICKFRAME`
/// is present (the frameless-but-resizable shape our subclass produces).
pub fn log_frame_state(window: &slint::Window, tag: &str) {
    let Some((style, ex_style)) = style_bits(window) else {
        crate::debug_log(&format!("frame {tag}: no hwnd"));
        return;
    };
    crate::debug_log(&format!(
        "frame {tag} style=0x{style:08X} exstyle=0x{ex_style:08X} caption={} thickframe={} topmost={}",
        style & WS_CAPTION != 0,
        style & WS_THICKFRAME != 0,
        ex_style & WS_EX_TOPMOST != 0,
    ));
}

/// `(GWL_STYLE, GWL_EXSTYLE)` for the raw window, or `None` without a handle.
pub fn style_bits(window: &slint::Window) -> Option<(isize, isize)> {
    let hwnd = hwnd_of(window)?;
    Some(unsafe { (GetWindowLongPtrW(hwnd, GWL_STYLE), GetWindowLongPtrW(hwnd, GWL_EXSTYLE)) })
}

/// Whether `BILI_DRAG_PROFILE=1` asked for the drag placement profile.
///
/// Read once and cached: the flag is a diagnostic switch, not a live control,
/// and the drag path must not pay for an env lookup per `flush`.
pub fn drag_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BILI_DRAG_PROFILE")
            .map(|value| value == "1")
            .unwrap_or(false)
    })
}

/// Whether `BILI_CLOSE_PROFILE=1` asked for the player close timeline.
///
/// Read once and cached, like [`drag_profile_enabled`]: the close path must not
/// pay for an env lookup per step.
pub fn close_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BILI_CLOSE_PROFILE")
            .map(|value| value == "1")
            .unwrap_or(false)
    })
}

/// Nearest-rank percentile of a sample set, `p` in `0.0..=1.0`.
///
/// `None` for an empty set. Deliberately naive (a clone + sort) - it only runs
/// once, at drag end, on at most a few thousand samples.
fn percentile(samples: &[u32], p: f64) -> Option<u32> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(rank.min(sorted.len() - 1)).copied()
}

/// How many windows are being moved by the self-driven drag loop right now.
///
/// One mouse drag moves exactly one window, so this is normally 0 or 1; it is
/// a counter (not a flag) because each [`WindowDrag`] must balance its own
/// transition. The video pacer reads it through
/// [`window_drag_active`] to suspend frame interpolation, and both frosted-glass
/// loops read it to skip a desktop capture: generating display frames and
/// reading back the screen are both expensive enough to be visible on top of
/// the window manager's own move, which is exactly where the judder came from.
static DRAGS_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);

/// Whether a window is currently being dragged by the app's own move loop.
///
/// Cheap (one relaxed load) so the pacer can call it once per source frame.
pub fn window_drag_active() -> bool {
    DRAGS_IN_FLIGHT.load(Ordering::Relaxed) > 0
}

/// Self-driven, frame-locked window move state for one Slint window.
///
/// Deliberately never hands the drag to the OS (no `SC_MOVE` modal loop, which
/// would re-evaluate the frame and restore the caption). The press records an
/// absolute anchor: the physical screen cursor and the window's physical outer
/// top-left. Pointer-move events only record that a placement is pending
/// ([`WindowDrag::move_to`] is pure bookkeeping); the caller's 16ms timer calls
/// [`WindowDrag::flush`], which samples the cursor once and applies at most one
/// absolute `SetWindowPos`. The target is always computed from the anchor, so
/// repeating a flush for the same cursor is a no-op and the window can never
/// accumulate motion.
pub struct WindowDrag {
    dragging: Cell<bool>,
    anchor_cursor: Cell<(i32, i32)>,
    anchor_window: Cell<(i32, i32)>,
    /// `true` when a pointer-move event asked for a placement that no
    /// [`WindowDrag::flush`] has applied yet. The target itself is the live
    /// cursor, sampled once per flush, so a 1000 Hz pointer costs one
    /// `GetCursorPos` per FRAME and no window call per event.
    pending_cursor: Cell<bool>,
    /// Explicit pending target for the diagnostic path (`BILI_DRAG_TEST`),
    /// which must not depend on where the real cursor happens to be.
    pending_target: Cell<Option<(i32, i32)>>,
    /// Absolute top-left last written by [`WindowDrag::flush`]. A target equal
    /// to this one is skipped entirely: below one physical pixel of motion
    /// there is nothing to move.
    applied: Cell<Option<(i32, i32)>>,
    /// Pointer-move events recorded for the current drag (`BILI_DRAG_PROFILE`).
    events: Cell<u64>,
    /// Window placements applied for the current drag.
    placements: Cell<u64>,
    /// Duration of each placement in microseconds, in submission order.
    samples: RefCell<Vec<u32>>,
}

/// Absolute window top-left for a cursor position, relative to a drag anchor.
///
/// Pure on purpose: this is the math the regression tests pin. The window
/// follows the cursor delta exactly once, independent of how many move events
/// have already been delivered.
pub fn drag_target(
    anchor_window: (i32, i32),
    anchor_cursor: (i32, i32),
    cursor: (i32, i32),
) -> (i32, i32) {
    (
        anchor_window.0 + (cursor.0 - anchor_cursor.0),
        anchor_window.1 + (cursor.1 - anchor_cursor.1),
    )
}

impl WindowDrag {
    pub fn new() -> Self {
        Self {
            dragging: Cell::new(false),
            anchor_cursor: Cell::new((0, 0)),
            anchor_window: Cell::new((0, 0)),
            pending_cursor: Cell::new(false),
            pending_target: Cell::new(None),
            applied: Cell::new(None),
            events: Cell::new(0),
            placements: Cell::new(0),
            samples: RefCell::new(Vec::new()),
        }
    }

    /// Record the absolute drag anchor. Never moves the window.
    ///
    /// Both the cursor and the native window rect must be readable; otherwise
    /// the drag is left inactive so a stale/default anchor is never used.
    /// Returns the reason it was refused so the caller can log it.
    pub fn start(&self, window: &slint::Window) -> Result<(), &'static str> {
        self.reset_profile();
        self.pending_cursor.set(false);
        self.pending_target.set(None);
        self.applied.set(None);
        let Some(hwnd) = hwnd_of(window) else {
            self.set_dragging(false);
            return Err("no-hwnd");
        };
        let Some(cursor) = cursor_position() else {
            self.set_dragging(false);
            return Err("no-cursor");
        };
        let Some(origin) = window_origin_hwnd(hwnd) else {
            self.set_dragging(false);
            return Err("no-origin");
        };
        self.anchor_cursor.set((cursor.x, cursor.y));
        self.anchor_window.set(origin);
        self.applied.set(Some(origin));
        self.set_dragging(true);
        Ok(())
    }

    /// Set the drag flag and keep [`DRAGS_IN_FLIGHT`] in step with it.
    ///
    /// Only a real transition counts, so a refused `start` on an already
    /// dragging window (or a repeated `end`) can never unbalance the counter
    /// the pacer and the glass loops read.
    fn set_dragging(&self, active: bool) {
        if self.dragging.replace(active) == active {
            return;
        }
        if active {
            DRAGS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        } else {
            DRAGS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Whether a drag is in progress.
    pub fn is_dragging(&self) -> bool {
        self.dragging.get()
    }

    /// Whether a UI "drag ended" signal must be HONOURED.
    ///
    /// The Slint pointer-up is authoritative while the button is already
    /// released. A cancel/leave that arrives while `VK_LBUTTON` is still down
    /// is not a release: the window moved out from under the cursor, so the
    /// drag must survive and keep tracking until the real up. Pure, so the
    /// `BILI_DRAG_TEST` reliability cycle drives exactly this decision.
    pub fn end_on_signal(&self, button_down: bool) -> bool {
        !(self.dragging.get() && button_down)
    }

    /// Record a pointer-move event. Deliberately does NOTHING else.
    ///
    /// The old path ran `GetCursorPos` + `SetWindowPos` (and, on the main
    /// window, a backdrop re-place) per event, which at 125-1000 Hz pointer
    /// sampling is dozens of full-window invalidations per displayed frame.
    /// The cursor is instead sampled once by [`WindowDrag::flush`], so the
    /// window moves at the display's cadence and never faster.
    pub fn move_to(&self) {
        if !self.dragging.get() {
            return;
        }
        self.events.set(self.events.get() + 1);
        self.pending_cursor.set(true);
    }

    /// Diagnostic entry point: record an explicit cursor position.
    ///
    /// Sends no synthetic input and reads no window position, so the
    /// `BILI_DRAG_TEST` hook exercises exactly the production math (record
    /// here, apply in [`WindowDrag::flush`]).
    pub fn move_to_cursor(&self, cursor: (i32, i32)) {
        if !self.dragging.get() {
            return;
        }
        self.events.set(self.events.get() + 1);
        self.pending_target.set(Some(cursor));
    }

    /// Apply at most ONE `SetWindowPos` for the pending target.
    ///
    /// Returns whether the window actually moved: `false` when no event is
    /// pending, when the target equals the last applied position (below one
    /// physical pixel of motion) or when the handle is not readable. This is
    /// the only place a drag places the window, and the caller drives it from
    /// a 16ms timer, so placements track the display refresh rate instead of
    /// the pointer sampling rate.
    pub fn flush(&self, window: &slint::Window) -> bool {
        if !self.dragging.get() {
            self.pending_cursor.set(false);
            self.pending_target.set(None);
            return false;
        }
        let cursor = match self.pending_target.take() {
            Some(target) => target,
            None => {
                if !self.pending_cursor.replace(false) {
                    return false;
                }
                let Some(cursor) = cursor_position() else {
                    return false;
                };
                (cursor.x, cursor.y)
            }
        };
        let target = drag_target(self.anchor_window.get(), self.anchor_cursor.get(), cursor);
        if self.applied.get() == Some(target) {
            return false;
        }
        let Some(hwnd) = hwnd_of(window) else {
            return false;
        };
        self.place(hwnd, target);
        self.applied.set(Some(target));
        true
    }

    /// One timed `SetWindowPos`, counted for the drag profile.
    ///
    /// The flags are `SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE`: this path
    /// never carries `SWP_FRAMECHANGED`, so a move can never make the window
    /// manager recompute the (removed) non-client frame on every frame.
    fn place(&self, hwnd: isize, target: (i32, i32)) {
        let started = Instant::now();
        set_origin_hwnd(hwnd, target);
        self.placements.set(self.placements.get() + 1);
        if drag_profile_enabled() {
            let micros = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
            if let Ok(mut samples) = self.samples.try_borrow_mut() {
                samples.push(micros);
            }
        }
    }

    /// Clear the counters at drag start.
    fn reset_profile(&self) {
        self.events.set(0);
        self.placements.set(0);
        if let Ok(mut samples) = self.samples.try_borrow_mut() {
            samples.clear();
        }
    }

    /// Log `drag profile ...` once per drag when `BILI_DRAG_PROFILE=1`.
    fn log_profile(&self) {
        if !drag_profile_enabled() {
            return;
        }
        let samples = match self.samples.try_borrow() {
            Ok(samples) => samples,
            Err(_) => return,
        };
        let p50 = percentile(&samples, 0.50).unwrap_or(0);
        let p95 = percentile(&samples, 0.95).unwrap_or(0);
        let max = samples.iter().copied().max().unwrap_or(0);
        crate::debug_log(&format!(
            "drag profile events={} placements={} p50_us={p50} p95_us={p95} max_us={max}",
            self.events.get(),
            self.placements.get(),
        ));
    }

    pub fn end(&self) {
        self.log_profile();
        self.set_dragging(false);
        self.pending_cursor.set(false);
        self.pending_target.set(None);
        self.applied.set(None);
    }

    /// The captured `(cursor, window)` anchor, for diagnostics only.
    pub fn anchor(&self) -> ((i32, i32), (i32, i32)) {
        (self.anchor_cursor.get(), self.anchor_window.get())
    }

    /// Diagnostic-only single absolute move (current origin plus a delta).
    ///
    /// Kept for the `BILI_DRAG_TEST` hook; it never reads a stale drag anchor
    /// and never uses a relative `set_position`.
    pub fn nudge(&self, window: &slint::Window, dx: i32, dy: i32) {
        let Some(hwnd) = hwnd_of(window) else {
            return;
        };
        let Some((x, y)) = window_origin_hwnd(hwnd) else {
            return;
        };
        set_origin_hwnd(hwnd, (x.saturating_add(dx), y.saturating_add(dy)));
    }
}

fn set_origin_hwnd(hwnd: isize, (x, y): (i32, i32)) {
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn window_origin_hwnd(hwnd: isize) -> Option<(i32, i32)> {
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some((rect.left, rect.top))
}

/// Physical top-left of a Slint window's native window rect.
pub fn window_origin(window: &slint::Window) -> Option<(i32, i32)> {
    hwnd_of(window).and_then(window_origin_hwnd)
}

fn cursor_position() -> Option<Point> {
    let mut point = Point { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some(point)
}

// ===================== self-driven window resize ==========================
//
// `WM_NCHITTEST` answers `HTCLIENT` everywhere (see `border_hit_test`), so the
// OS never starts its modal resize loop. The native cursor test cannot drag a
// framed window's border any more; instead `ui/app.slint` lays 8px hot zones
// over the rim (14px at the corners, 18px on the bottom-right grip) and calls
// the `resize_start` / `resize_move` / `resize_end` callbacks. This section is
// the state behind those callbacks: anchored edge math, one `SetWindowPos` per
// display frame, and a minimum/work-area clamp.
//
// The shape matches `WindowDrag` on purpose. `start` records an absolute anchor
// (cursor + the four sides of the window rect); `move_to` only marks that a
// placement is due; the caller's 8ms timer calls `update`, which samples the
// cursor ONCE and applies at most one absolute rect. Every rect is recomputed
// from the anchor, so repeating a tick cannot accumulate motion and a 1000 Hz
// pointer still costs one placement per frame.

/// Whether `BILI_RESIZE_PROFILE=1` (or the `BILI_RESIZE_TEST` hook) asked for
/// per-placement resize logging and the end-of-resize timing profile.
///
/// Read once and cached: the placement path must not pay for an env lookup at
/// 120 Hz.
pub fn resize_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("BILI_RESIZE_PROFILE")
            .map(|value| value == "1")
            .unwrap_or(false)
            || std::env::var("BILI_RESIZE_TEST").is_ok()
    })
}

/// Clamp `value` into `lo..=hi`, preferring `lo` when the range is empty.
///
/// A minimum logical size can be larger than a tiny work area (or a monitor
/// smaller than the app's floor). `Ord::clamp` panics on an empty range; this
/// helper never does, and the caller's own work-area clamp decides the rest.
fn clamp_between(value: i32, lo: i32, hi: i32) -> i32 {
    if hi < lo {
        return lo;
    }
    value.max(lo).min(hi)
}

/// New outer rect for one anchored edge and the cursor's delta from the anchor.
///
/// `anchor` is `(left, top, right, bottom)` in physical screen px, `min` the
/// minimum size in physical px and `work` the monitor work area `(x, y, w, h)`.
/// Only the edges named by `edge` move; the opposite edges are copied from the
/// anchor unchanged, so the window can never creep. Pure, so the regression
/// tests pin every edge and the no-opposite-movement invariant.
pub fn resized_rect(
    anchor: (i32, i32, i32, i32),
    anchor_cursor: (i32, i32),
    cursor: (i32, i32),
    edge: i32,
    min: (i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = anchor;
    let dx = cursor.0.saturating_sub(anchor_cursor.0);
    let dy = cursor.1.saturating_sub(anchor_cursor.1);
    let (work_x, work_y, work_w, work_h) = work;
    let work_right = work_x.saturating_add(work_w);
    let work_bottom = work_y.saturating_add(work_h);

    let left_edge = matches!(edge, 1 | 5 | 7);
    let right_edge = matches!(edge, 2 | 6 | 8);
    let top_edge = matches!(edge, 3 | 5 | 6);
    let bottom_edge = matches!(edge, 4 | 7 | 8);

    // Horizontal: one edge moves inside [work origin, opposite edge - min].
    let (new_left, new_right) = if left_edge {
        (
            clamp_between(
                left.saturating_add(dx),
                work_x,
                right.saturating_sub(min.0),
            ),
            right,
        )
    } else if right_edge {
        (
            left,
            clamp_between(
                right.saturating_add(dx),
                left.saturating_add(min.0),
                work_right,
            ),
        )
    } else {
        (left, right)
    };

    // Vertical: same contract, never moving the opposite side.
    let (new_top, new_bottom) = if top_edge {
        (
            clamp_between(
                top.saturating_add(dy),
                work_y,
                bottom.saturating_sub(min.1),
            ),
            bottom,
        )
    } else if bottom_edge {
        (
            top,
            clamp_between(
                bottom.saturating_add(dy),
                top.saturating_add(min.1),
                work_bottom,
            ),
        )
    } else {
        (top, bottom)
    };

    (new_left, new_top, new_right, new_bottom)
}

/// Fit an arbitrary target rect into the minimum size and the work area.
///
/// Used by the cursor-independent diagnostic entry point
/// ([`WindowResize::update_to_rect`]), which must go through the same placement
/// path as a real edge drag. Work-area bounds win over the minimum when a
/// monitor is smaller than the floor (a degenerate case; nothing panics).
fn fit_rect(
    target: (i32, i32, i32, i32),
    min: (i32, i32),
    work: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = target;
    let (work_x, work_y, work_w, work_h) = work;
    let work_right = work_x.saturating_add(work_w);
    let work_bottom = work_y.saturating_add(work_h);
    // The minimum size wins over a small target, but never over the monitor:
    // a work area smaller than the floor yields the whole work area.
    let width = right
        .saturating_sub(left)
        .max(min.0)
        .min(work_w)
        .max(1);
    let height = bottom
        .saturating_sub(top)
        .max(min.1)
        .min(work_h)
        .max(1);
    let left = clamp_between(left, work_x, work_right.saturating_sub(width));
    let top = clamp_between(top, work_y, work_bottom.saturating_sub(height));
    (left, top, left + width, top + height)
}

/// Self-driven, frame-locked window resize state for one Slint window.
pub struct WindowResize {
    active: Cell<bool>,
    /// Which edges move: 1 left, 2 right, 3 top, 4 bottom, 5 top-left,
    /// 6 top-right, 7 bottom-left, 8 bottom-right.
    edge: Cell<i32>,
    anchor_cursor: Cell<(i32, i32)>,
    /// `(left, top, right, bottom)` of the native window rect at press time.
    anchor_rect: Cell<(i32, i32, i32, i32)>,
    /// Minimum size in LOGICAL px, supplied by the caller (the UI's own floor).
    min: Cell<(i32, i32)>,
    /// `true` when a pointer move asked for a placement no tick has applied yet.
    pending: Cell<bool>,
    /// Last rect written by [`WindowResize::place_if_changed`].
    applied: Cell<Option<(i32, i32, i32, i32)>>,
    /// Last rect the diagnostics reported, so `end()` can log it.
    last: Cell<Option<(i32, i32, i32, i32)>>,
    placements: Cell<u64>,
    samples: RefCell<Vec<u32>>,
}

impl WindowResize {
    pub fn new() -> Self {
        Self {
            active: Cell::new(false),
            edge: Cell::new(0),
            anchor_cursor: Cell::new((0, 0)),
            anchor_rect: Cell::new((0, 0, 0, 0)),
            min: Cell::new((1, 1)),
            pending: Cell::new(false),
            applied: Cell::new(None),
            last: Cell::new(None),
            placements: Cell::new(0),
            samples: RefCell::new(Vec::new()),
        }
    }

    /// The smallest size this window may be resized to, in LOGICAL px.
    pub fn set_min(&self, width: i32, height: i32) {
        self.min.set((width.max(1), height.max(1)));
    }

    /// Record the anchor for one resize gesture. Never moves the window.
    ///
    /// Reads the cursor and the native rect once; refuses (leaving the gesture
    /// inactive) when either is unavailable or `edge` is not 1..=8.
    pub fn start(&self, window: &slint::Window, edge: i32) -> Result<(), &'static str> {
        self.placements.set(0);
        if let Ok(mut samples) = self.samples.try_borrow_mut() {
            samples.clear();
        }
        self.last.set(None);
        self.pending.set(false);
        self.applied.set(None);
        if !(1..=8).contains(&edge) {
            self.active.set(false);
            return Err("bad-edge");
        }
        let Some(hwnd) = hwnd_of(window) else {
            self.active.set(false);
            return Err("no-hwnd");
        };
        let Some(cursor) = cursor_position() else {
            self.active.set(false);
            return Err("no-cursor");
        };
        let Some(rect) = window_rect_hwnd(hwnd) else {
            self.active.set(false);
            return Err("no-rect");
        };
        self.edge.set(edge);
        self.anchor_cursor.set((cursor.x, cursor.y));
        self.anchor_rect.set(rect);
        self.applied.set(Some(rect));
        self.last.set(Some(rect));
        self.active.set(true);
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active.get()
    }

    /// Record a pointer move. Deliberately places nothing: the caller's frame
    /// timer samples the cursor and applies one placement per tick.
    pub fn move_to(&self) {
        if self.active.get() {
            self.pending.set(true);
        }
    }

    /// Apply at most ONE placement for the pending cursor, then clear it.
    ///
    /// Returns whether the window actually changed size/position. The cursor is
    /// sampled once here, never per pointer event, so a 1000 Hz pointer costs
    /// one `GetCursorPos` and at most one `SetWindowPos` per displayed frame.
    pub fn update(&self, window: &slint::Window) -> bool {
        if !self.active.get() || !self.pending.replace(false) {
            return false;
        }
        let Some(cursor) = cursor_position() else {
            return false;
        };
        self.apply_from_cursor(window, (cursor.x, cursor.y))
    }

    /// Cursor-independent placement, for `BILI_RESIZE_TEST`.
    ///
    /// Drives the same active/anchor state and the same single
    /// `SetWindowPos` path as [`WindowResize::update`], but with an explicit
    /// target rect instead of a sampled cursor: the diagnostic must not depend
    /// on where the real pointer happens to be (and must never move it).
    pub fn update_to_rect(&self, window: &slint::Window, rect: (i32, i32, i32, i32)) -> bool {
        if !self.active.get() {
            return false;
        }
        let Some(work) = self.work_area() else {
            return false;
        };
        let target = fit_rect(rect, self.min_physical(window), work);
        self.place_if_changed(window, target)
    }

    /// Compute the anchored rect for `cursor` and place it if it changed.
    fn apply_from_cursor(&self, window: &slint::Window, cursor: (i32, i32)) -> bool {
        let Some(work) = self.work_area() else {
            return false;
        };
        let target = resized_rect(
            self.anchor_rect.get(),
            self.anchor_cursor.get(),
            cursor,
            self.edge.get(),
            self.min_physical(window),
            work,
        );
        self.place_if_changed(window, target)
    }

    /// One `SetWindowPos` when the target differs from what this gesture wrote.
    fn place_if_changed(&self, window: &slint::Window, target: (i32, i32, i32, i32)) -> bool {
        if self.applied.get() == Some(target) {
            return false;
        }
        let Some(hwnd) = hwnd_of(window) else {
            return false;
        };
        let started = Instant::now();
        set_rect_hwnd(hwnd, target);
        let micros = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        self.placements.set(self.placements.get() + 1);
        self.applied.set(Some(target));
        self.last.set(Some(target));
        if resize_profile_enabled() {
            if let Ok(mut samples) = self.samples.try_borrow_mut() {
                samples.push(micros);
            }
            crate::debug_log(&format!(
                "resize edge={} rect={},{} {}x{} us={micros}",
                self.edge.get(),
                target.0,
                target.1,
                target.2.saturating_sub(target.0),
                target.3.saturating_sub(target.1),
            ));
        }
        true
    }

    /// The work area of the monitor the anchored rect sits on.
    fn work_area(&self) -> Option<(i32, i32, i32, i32)> {
        let (left, top, right, bottom) = self.anchor_rect.get();
        let width = right.saturating_sub(left);
        let height = bottom.saturating_sub(top);
        work_area_for_rect(left, top, width, height)
    }

    /// The caller's logical minimum, converted to this window's physical px.
    fn min_physical(&self, window: &slint::Window) -> (i32, i32) {
        let scale = f64::from(window.scale_factor().max(0.2));
        let (width, height) = self.min.get();
        (
            (f64::from(width) * scale).round().max(1.0) as i32,
            (f64::from(height) * scale).round().max(1.0) as i32,
        )
    }

    /// Close the gesture: log one summary plus the optional timing profile.
    pub fn end(&self) {
        if !self.active.get() {
            return;
        }
        let last = self.last.get();
        crate::debug_log(&format!(
            "resize end edge={} placements={}{}",
            self.edge.get(),
            self.placements.get(),
            match last {
                Some((left, top, right, bottom)) => format!(
                    " rect={left},{top} {}x{}",
                    right.saturating_sub(left),
                    bottom.saturating_sub(top)
                ),
                None => String::new(),
            },
        ));
        if resize_profile_enabled() {
            if let Ok(samples) = self.samples.try_borrow() {
                let p50 = percentile(&samples, 0.50).unwrap_or(0);
                let p95 = percentile(&samples, 0.95).unwrap_or(0);
                let max = samples.iter().copied().max().unwrap_or(0);
                crate::debug_log(&format!(
                    "resize profile placements={} p50_us={p50} p95_us={p95} max_us={max}",
                    self.placements.get(),
                ));
            }
        }
        self.active.set(false);
        self.pending.set(false);
        self.applied.set(None);
    }
}

fn window_rect_hwnd(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    (unsafe { GetWindowRect(hwnd, &mut rect) } != 0)
        .then_some((rect.left, rect.top, rect.right, rect.bottom))
}

/// One absolute `SetWindowPos` carrying both origin and size.
///
/// `SWP_NOZORDER | SWP_NOACTIVATE` only: no `SWP_FRAMECHANGED`, so a frame of
/// resizing can never make the window manager recompute the removed non-client
/// frame, and no `SWP_NOMOVE`/`SWP_NOSIZE`, because the anchored edge moves one
/// side independently.
fn set_rect_hwnd(hwnd: isize, (left, top, right, bottom): (i32, i32, i32, i32)) {
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            left,
            top,
            right.saturating_sub(left),
            bottom.saturating_sub(top),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Log the frameless invariant: `WS_CAPTION` clear and the client rect equal to
/// the whole window rect (so no non-client strip is left unpainted).
pub fn log_frameless_invariant(window: &slint::Window, tag: &str) {
    let Some(hwnd) = hwnd_of(window) else {
        crate::debug_log(&format!("frameless-invariant tag={tag}: no hwnd"));
        return;
    };
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    let mut client = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut outer = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let client_ok = unsafe { GetClientRect(hwnd, &mut client) } != 0;
    let outer_ok = unsafe { GetWindowRect(hwnd, &mut outer) } != 0;
    let client_w = client.right - client.left;
    let client_h = client.bottom - client.top;
    let outer_w = outer.right - outer.left;
    let outer_h = outer.bottom - outer.top;
    let caption = u8::from(style & WS_CAPTION != 0);
    let thickframe = u8::from(style & WS_THICKFRAME != 0);
    let matched = client_ok && outer_ok && client_w == outer_w && client_h == outer_h;
    let ok = caption == 0 && matched;
    crate::debug_log(&format!(
        "frameless-invariant tag={tag} caption={caption} client={client_w}x{client_h} \
         window={outer_w}x{outer_h} match={} thickframe={thickframe} ok={}",
        u8::from(matched),
        u8::from(ok),
    ));
}

fn dwm_set_u32(hwnd: isize, attribute: u32, value: u32) -> i32 {
    unsafe { DwmSetWindowAttribute(hwnd, attribute, (&value as *const u32).cast(), 4) }
}

/// Remove the 1px Windows 11 DWM border drawn inside the window edge.
///
/// Best-effort: pre-Win11 builds reject `DWMWA_BORDER_COLOR` and the call just
/// returns a failure HRESULT, which is ignored (nothing panics).
fn dwm_remove_border(hwnd: isize) -> i32 {
    dwm_set_u32(hwnd, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE)
}

// ===================== per-process audio session volume =====================
//
// The player's sound comes from `ffplay.exe`, which owns a Windows audio
// session. WASAPI's per-session `ISimpleAudioVolume` moves that session's fader
// inside the mixer, so the already-running `ffplay` changes volume instantly --
// the old path restarted `ffplay` on every wheel step, which was audible as
// "instant mute, then the new level".
//
// `windows-sys` ships the Core Audio *functions* but no COM interface
// definitions, so the four vtables used below are declared locally, method for
// method in the SDK's declaration order (mmdeviceapi.h, audiopolicy.h,
// audioclient.h). Methods we never call still occupy a blank slot
// (`VtblSlot`) so the offsets of the ones we do call stay exact.
//
// Everything here is best-effort and cheap: no device, no session yet (ffplay
// between two spawns) or any COM failure yields `false`/`None` -- the caller
// falls back to restarting the child -- and never a panic or a retry loop.

/// `CLSID_MMDeviceEnumerator` (mmdeviceapi.h).
const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID {
    data1: 0xBCDE_0395,
    data2: 0xE52F,
    data3: 0x467C,
    data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
};

/// `IID_IMMDeviceEnumerator` (mmdeviceapi.h).
const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID {
    data1: 0xA956_64D2,
    data2: 0x9614,
    data3: 0x4F35,
    data4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
};

/// `IID_IAudioSessionManager2` (audiopolicy.h).
const IID_IAUDIO_SESSION_MANAGER2: GUID = GUID {
    data1: 0x77AA_99A0,
    data2: 0x1BD6,
    data3: 0x484F,
    data4: [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F],
};

/// `IID_IAudioSessionControl2` (audiopolicy.h).
const IID_IAUDIO_SESSION_CONTROL2: GUID = GUID {
    data1: 0xBFB7_FF88,
    data2: 0x7239,
    data3: 0x4FC9,
    data4: [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D],
};

/// `IID_ISimpleAudioVolume` (audioclient.h).
const IID_ISIMPLE_AUDIO_VOLUME: GUID = GUID {
    data1: 0x87CE_5498,
    data2: 0x68D6,
    data3: 0x44E5,
    data4: [0x92, 0x15, 0x6D, 0xA4, 0x7E, 0xF8, 0x83, 0xD8],
};

/// `RPC_E_CHANGED_MODE` (winerror.h): the thread is already in another COM
/// apartment. COM still works there, so this is not a failure.
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;

/// `CREATE_NO_WINDOW` (winbase.h), for the diagnostic `ffplay` spawn.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One unused vtable entry (a function pointer slot).
type VtblSlot = *const c_void;

/// The first three vtable entries of every COM interface (`IUnknown`).
#[repr(C)]
struct ComVtblHead {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IMMDeviceEnumerator`: `EnumAudioEndpoints`, `GetDefaultAudioEndpoint`,
/// `GetDevice`, `RegisterEndpointNotificationCallback`,
/// `UnregisterEndpointNotificationCallback`.
#[repr(C)]
struct MmDeviceEnumeratorVtbl {
    head: ComVtblHead,
    enum_audio_endpoints: VtblSlot,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> i32,
    get_device: VtblSlot,
    register_endpoint_notification_callback: VtblSlot,
    unregister_endpoint_notification_callback: VtblSlot,
}

/// `IMMDevice`: `Activate`, `OpenPropertyStore`, `GetId`, `GetState`.
#[repr(C)]
struct MmDeviceVtbl {
    head: ComVtblHead,
    activate: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        u32,
        *const c_void,
        *mut *mut c_void,
    ) -> i32,
    open_property_store: VtblSlot,
    get_id: VtblSlot,
    get_state: VtblSlot,
}

/// `IAudioSessionManager2`, after its `IAudioSessionManager` base
/// (`GetAudioSessionControl`, `GetSimpleAudioVolume`): `GetSessionEnumerator`,
/// `RegisterSessionNotification`, `UnregisterSessionNotification`,
/// `RegisterDuckNotification`, `UnregisterDuckNotification`.
#[repr(C)]
struct SessionManager2Vtbl {
    head: ComVtblHead,
    get_audio_session_control: VtblSlot,
    get_simple_audio_volume: VtblSlot,
    get_session_enumerator: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    register_session_notification: VtblSlot,
    unregister_session_notification: VtblSlot,
    register_duck_notification: VtblSlot,
    unregister_duck_notification: VtblSlot,
}

/// `IAudioSessionEnumerator`: `GetCount`, `GetSession`.
#[repr(C)]
struct SessionEnumeratorVtbl {
    head: ComVtblHead,
    get_count: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
    get_session: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32,
}

/// `IAudioSessionControl2`, after its `IAudioSessionControl` base (nine
/// unused methods): `GetSessionIdentifier`, `GetSessionInstanceIdentifier`,
/// `GetProcessId`, `IsSystemSoundsSession`, `SetDuckingPreference`.
#[repr(C)]
struct SessionControl2Vtbl {
    head: ComVtblHead,
    base: [VtblSlot; 9],
    get_session_identifier: VtblSlot,
    get_session_instance_identifier: VtblSlot,
    get_process_id: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    is_system_sounds_session: VtblSlot,
    set_ducking_preference: VtblSlot,
}

/// `ISimpleAudioVolume`: `SetMasterVolume`, `GetMasterVolume`, `SetMute`,
/// `GetMute`.
#[repr(C)]
struct SimpleAudioVolumeVtbl {
    head: ComVtblHead,
    set_master_volume: unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> i32,
    get_master_volume: unsafe extern "system" fn(*mut c_void, *mut f32) -> i32,
    set_mute: unsafe extern "system" fn(*mut c_void, i32, *const GUID) -> i32,
    get_mute: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
}

/// The vtable of a COM interface pointer: an object's first word points at it.
///
/// The returned reference is `'static` because a vtable is static data owned by
/// the class, not by the object.
unsafe fn vtbl<T>(object: *mut c_void) -> &'static T {
    &**(object as *mut *const T)
}

/// `Release` through the shared `IUnknown` prefix.
unsafe fn com_release(object: *mut c_void) {
    if object.is_null() {
        return;
    }
    (vtbl::<ComVtblHead>(object).release)(object);
}

/// `AddRef` through the shared `IUnknown` prefix.
unsafe fn com_add_ref(object: *mut c_void) {
    if !object.is_null() {
        (vtbl::<ComVtblHead>(object).add_ref)(object);
    }
}

/// `QueryInterface` for `iid`; `None` when the object does not implement it.
unsafe fn com_query_interface(object: *mut c_void, iid: &GUID) -> Option<*mut c_void> {
    let mut out: *mut c_void = std::ptr::null_mut();
    let hr = (vtbl::<ComVtblHead>(object).query_interface)(object, iid, &mut out);
    (hr >= 0 && !out.is_null()).then_some(out)
}

/// An owned COM reference that releases itself, so every early return in the
/// session walk is leak-free.
struct ComRef(*mut c_void);

impl ComRef {
    /// Take ownership of a `+1` reference; `None` for a null pointer.
    unsafe fn from_raw(pointer: *mut c_void) -> Option<Self> {
        (!pointer.is_null()).then_some(Self(pointer))
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for ComRef {
    fn drop(&mut self) {
        unsafe { com_release(self.0) };
    }
}

/// Per-thread COM apartment. Initialised once per thread (no per-call
/// `CoInitializeEx`); `CoUninitialize` is balanced from `Drop` only when THIS
/// thread's own `CoInitializeEx` succeeded.
struct ComInit {
    ready: bool,
    owned: bool,
}

impl ComInit {
    fn new() -> Self {
        let flags = (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32;
        let hr = unsafe { CoInitializeEx(std::ptr::null(), flags) };
        if hr >= 0 {
            // `S_OK` or `S_FALSE` (already initialised in this mode): both must
            // be balanced by one `CoUninitialize`.
            Self {
                ready: true,
                owned: true,
            }
        } else if hr == RPC_E_CHANGED_MODE {
            // Already an MTA (or another mode): COM is usable as-is, and this
            // thread must not uninitialise someone else's apartment.
            Self {
                ready: true,
                owned: false,
            }
        } else {
            Self {
                ready: false,
                owned: false,
            }
        }
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    /// The calling thread's COM apartment, initialised at most once.
    static COM: ComInit = ComInit::new();
}

/// Whether COM is usable on the calling thread.
fn com_ready() -> bool {
    COM.with(|state| state.ready)
}

/// The session we last resolved: `(pid, owning thread, ISimpleAudioVolume)`.
///
/// The pointer is kept as a `usize` so the static stays `Sync`; the cache holds
/// one `AddRef` of it. It is only reused from the thread that opened it, since
/// a COM interface belongs to its apartment.
static AUDIO_TARGET: Mutex<Option<(u32, u32, usize)>> = Mutex::new(None);

/// Last volume written: `(pid, level bits, ok)`. A repeated identical request
/// (a wheel that clamps at 0 % or 100 %, or the same step twice) is answered
/// from here without touching COM at all.
static AUDIO_LAST_VOLUME: Mutex<Option<(u32, u32, bool)>> = Mutex::new(None);

/// Clamp a requested level into `0.0..=1.0`; `None` for NaN / infinity.
fn normalise_level(volume: f32) -> Option<f32> {
    volume.is_finite().then(|| volume.clamp(0.0, 1.0))
}

/// Find `pid`'s `ISimpleAudioVolume` in the default render device's session
/// list. Returns one owned reference.
unsafe fn find_session_volume(pid: u32) -> Option<ComRef> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    let hr = CoCreateInstance(
        &CLSID_MM_DEVICE_ENUMERATOR,
        std::ptr::null_mut(),
        CLSCTX_ALL,
        &IID_IMM_DEVICE_ENUMERATOR,
        &mut raw,
    );
    if hr < 0 {
        return None;
    }
    let enumerator = ComRef::from_raw(raw)?;

    let mut raw: *mut c_void = std::ptr::null_mut();
    let hr = (vtbl::<MmDeviceEnumeratorVtbl>(enumerator.as_ptr()).get_default_audio_endpoint)(
        enumerator.as_ptr(),
        eRender,
        eConsole,
        &mut raw,
    );
    if hr < 0 {
        return None;
    }
    let device = ComRef::from_raw(raw)?;

    let mut raw: *mut c_void = std::ptr::null_mut();
    let hr = (vtbl::<MmDeviceVtbl>(device.as_ptr()).activate)(
        device.as_ptr(),
        &IID_IAUDIO_SESSION_MANAGER2,
        CLSCTX_ALL,
        std::ptr::null(),
        &mut raw,
    );
    if hr < 0 {
        return None;
    }
    let manager = ComRef::from_raw(raw)?;

    let manager_vtbl = vtbl::<SessionManager2Vtbl>(manager.as_ptr());
    let mut raw: *mut c_void = std::ptr::null_mut();
    let hr = (manager_vtbl.get_session_enumerator)(manager.as_ptr(), &mut raw);
    if hr < 0 {
        return None;
    }
    let sessions = ComRef::from_raw(raw)?;

    let sessions_vtbl = vtbl::<SessionEnumeratorVtbl>(sessions.as_ptr());
    let mut count = 0i32;
    if (sessions_vtbl.get_count)(sessions.as_ptr(), &mut count) < 0 {
        return None;
    }
    for index in 0..count {
        let mut raw: *mut c_void = std::ptr::null_mut();
        if (sessions_vtbl.get_session)(sessions.as_ptr(), index, &mut raw) < 0 {
            continue;
        }
        let Some(control) = ComRef::from_raw(raw) else {
            continue;
        };
        let Some(raw) = com_query_interface(control.as_ptr(), &IID_IAUDIO_SESSION_CONTROL2) else {
            continue;
        };
        let Some(control2) = ComRef::from_raw(raw) else {
            continue;
        };
        let mut session_pid = 0u32;
        let hr = (vtbl::<SessionControl2Vtbl>(control2.as_ptr()).get_process_id)(
            control2.as_ptr(),
            &mut session_pid,
        );
        if hr < 0 || session_pid != pid {
            continue;
        }
        // QI from the base interface: both come from the same session object.
        if let Some(raw) = com_query_interface(control.as_ptr(), &IID_ISIMPLE_AUDIO_VOLUME) {
            return ComRef::from_raw(raw);
        }
        continue;
    }
    None
}

/// One owned `ISimpleAudioVolume` reference for `pid`, reusing the cached one
/// when it was opened on this thread.
fn acquire_session(pid: u32) -> Option<*mut c_void> {
    let thread = unsafe { GetCurrentThreadId() };
    let cached = match AUDIO_TARGET.lock() {
        Ok(guard) => *guard,
        Err(_) => None,
    };
    if let Some((cached_pid, cached_thread, pointer)) = cached {
        if pointer != 0 && cached_pid == pid && cached_thread == thread {
            let pointer = pointer as *mut c_void;
            unsafe { com_add_ref(pointer) };
            return Some(pointer);
        }
    }
    if !com_ready() {
        return None;
    }
    let found = unsafe { find_session_volume(pid) }?;
    let pointer = found.as_ptr();
    // Transfer the reference we just acquired to the caller, and hold a second
    // one in the cache so the next call is a single virtual call.
    std::mem::forget(found);
    unsafe { com_add_ref(pointer) };
    let previous = match AUDIO_TARGET.lock() {
        Ok(mut guard) => guard.replace((pid, thread, pointer as usize)),
        Err(_) => None,
    };
    if let Some((_, _, old)) = previous {
        if old != 0 {
            unsafe { com_release(old as *mut c_void) };
        }
    }
    Some(pointer)
}

/// Drop the cached session reference (after a call failed: the child exited or
/// the default device changed, so the cached pointer is dead).
fn forget_session() {
    let stale = match AUDIO_TARGET.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => None,
    };
    if let Some((_, _, old)) = stale {
        if old != 0 {
            unsafe { com_release(old as *mut c_void) };
        }
    }
}

/// Run `call` against `pid`'s session fader. `None` when the process has no
/// audio session (yet) or the device is unavailable.
fn with_session<F>(pid: u32, call: F) -> Option<i32>
where
    F: FnOnce(*mut c_void) -> i32,
{
    let pointer = acquire_session(pid)?;
    let result = call(pointer);
    unsafe { com_release(pointer) };
    if result < 0 {
        forget_session();
    }
    Some(result)
}

/// Set the master volume of the audio session owned by `pid`, `0.0..=1.0`.
///
/// Returns `false` when that process has no session yet (or on any COM
/// failure); the player then keeps its old restart-the-child fallback.
pub fn audio_session_volume(pid: u32, volume: f32) -> bool {
    let Some(level) = normalise_level(volume) else {
        return false;
    };
    let bits = level.to_bits();
    if let Ok(guard) = AUDIO_LAST_VOLUME.lock() {
        if matches!(*guard, Some((cached, cached_bits, true)) if cached == pid && cached_bits == bits)
        {
            return true;
        }
    }
    let result = with_session(pid, |pointer| unsafe {
        let set_master_volume = vtbl::<SimpleAudioVolumeVtbl>(pointer).set_master_volume;
        set_master_volume(pointer, level, std::ptr::null())
    });
    let ok = matches!(result, Some(hr) if hr >= 0);
    if let Ok(mut guard) = AUDIO_LAST_VOLUME.lock() {
        *guard = Some((pid, bits, ok));
    }
    ok
}

/// Mute or unmute the audio session owned by `pid`.
pub fn audio_session_mute(pid: u32, muted: bool) -> bool {
    let mute: i32 = if muted { 1 } else { 0 };
    let result = with_session(pid, |pointer| unsafe {
        let set_mute = vtbl::<SimpleAudioVolumeVtbl>(pointer).set_mute;
        set_mute(pointer, mute, std::ptr::null())
    });
    // The level was not touched, but a mute toggle is cheap and rare; the
    // cached "last volume" answer stays valid either way.
    matches!(result, Some(hr) if hr >= 0)
}

/// Read back the session's current `(volume, muted)` for `pid`.
///
/// Used by the QA hook and by callers that want to confirm a write landed.
pub fn audio_session_get(pid: u32) -> Option<(f32, bool)> {
    let pointer = acquire_session(pid)?;
    let mut level = 0.0f32;
    let mut mute = 0i32;
    let (level_hr, mute_hr) = unsafe {
        let fader = vtbl::<SimpleAudioVolumeVtbl>(pointer);
        (
            (fader.get_master_volume)(pointer, &mut level),
            (fader.get_mute)(pointer, &mut mute),
        )
    };
    unsafe { com_release(pointer) };
    if level_hr < 0 || mute_hr < 0 {
        forget_session();
        return None;
    }
    Some((level, mute != 0))
}

/// `ffplay.exe` processes whose parent is this process (diagnostics).
fn ffplay_child_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return pids;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while found {
        if entry.th32ParentProcessID == std::process::id() {
            let name_len = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .map_or(entry.szExeFile.len(), |index| index);
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
            if name.eq_ignore_ascii_case("ffplay.exe") {
                pids.push(entry.th32ProcessID);
            }
        }
        found = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    pids
}

/// Spawn a private `ffplay` on a generated sine tone, so the self-test has an
/// audio session of its own without touching the real player.
fn spawn_selftest_ffplay() -> Option<std::process::Child> {
    use std::os::windows::process::CommandExt;
    let dir = std::env::var("BILI_FFMPEG_DIR")
        .map(std::path::PathBuf::from)
        .map_or_else(
            |_| std::path::PathBuf::from(r"C:\Programs\ffmpeg-unpack\bin"),
            |path| path,
        );
    let executable = dir.join("ffplay.exe");
    let executable = if executable.is_file() {
        executable
    } else {
        std::path::PathBuf::from("ffplay.exe")
    };
    let mut command = std::process::Command::new(executable);
    command
        .arg("-nodisp")
        .arg("-autoexit")
        .arg("-loglevel")
        .arg("error")
        .arg("-volume")
        .arg("50")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("sine=frequency=440:duration=120")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    command.spawn().ok()
}

/// A `u64` from the environment, or `fallback`.
fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(fallback, |value| value)
}

/// Poll until `pid` has a render session, up to `wait_ms`.
fn wait_for_session(pid: u32, wait_ms: u64) -> Option<(f32, bool)> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        if let Some(state) = audio_session_get(pid) {
            return Some(state);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll for this process's first `ffplay.exe` child, up to `wait_ms`.
fn wait_for_ffplay_child(wait_ms: u64) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        if let Some(pid) = ffplay_child_pids().into_iter().next() {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The `BILI_AUDIO_SESSION_TEST` body: prove the fader on one real session.
///
/// Writes 80 / 40 / 0 / 65 / 100 %, mutes on and off, reads every step back,
/// then pushes 200 distinct levels back to back and reports the elapsed time --
/// the exact pattern a wheel spin produces. The session's original state is
/// restored at the end.
fn audio_session_proof(pid: u32, wait_ms: u64) -> Option<(f32, bool)> {
    let Some((original_level, original_muted)) = wait_for_session(pid, wait_ms) else {
        crate::debug_log(&format!(
            "audio-session selftest pid={pid} found=false wait_ms={wait_ms}"
        ));
        return None;
    };
    crate::debug_log(&format!(
        "audio-session selftest pid={pid} found=true volume={original_level:.3} \
         muted={original_muted} wait_ms={wait_ms}"
    ));

    let mut mismatches = 0u32;
    for wanted in [0.80f32, 0.40, 0.00, 0.65, 1.00] {
        let ok = audio_session_volume(pid, wanted);
        let state = audio_session_get(pid);
        let readback = state.map_or(-1.0, |(level, _)| level);
        let muted = state.map_or(false, |(_, muted)| muted);
        let matched = state.map_or(false, |(level, _)| (level - wanted).abs() < 0.01);
        if !matched {
            mismatches += 1;
        }
        crate::debug_log(&format!(
            "audio-session set pid={pid} want={wanted:.3} ok={ok} readback={readback:.3} \
             muted={muted} match={matched}"
        ));
    }
    for wanted in [true, false] {
        let ok = audio_session_mute(pid, wanted);
        let state = audio_session_get(pid);
        let readback = state.map_or(false, |(_, muted)| muted);
        let matched = state.map_or(false, |(_, muted)| muted == wanted);
        if !matched {
            mismatches += 1;
        }
        crate::debug_log(&format!(
            "audio-session mute pid={pid} want={wanted} ok={ok} readback={readback} match={matched}"
        ));
    }

    let started = Instant::now();
    let mut accepted = 0u32;
    for step in 0..200u32 {
        let level = (step % 101) as f32 / 100.0;
        if audio_session_volume(pid, level) {
            accepted += 1;
        }
    }
    let elapsed = started.elapsed();
    let readback = audio_session_get(pid).map_or(-1.0, |(level, _)| level);
    crate::debug_log(&format!(
        "audio-session rapid sets=200 accepted={accepted} elapsed_ms={:.1} per_call_us={:.1} \
         readback={readback:.3}",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1_000_000.0 / 200.0,
    ));

    let started = Instant::now();
    for _ in 0..200 {
        let _ = audio_session_volume(pid, 0.42);
    }
    let elapsed = started.elapsed();
    crate::debug_log(&format!(
        "audio-session repeat sets=200 value=0.420 elapsed_ms={:.1} per_call_us={:.1} \
         (cache hit, no COM)",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1_000_000.0 / 200.0,
    ));

    // Cost of a cold call: dropping the cached session forces a fresh device +
    // session enumeration, which is what a newly spawned `ffplay` pays once.
    forget_session();
    let started = Instant::now();
    let cold_ok = audio_session_volume(pid, 0.55);
    let cold = started.elapsed();
    crate::debug_log(&format!(
        "audio-session cold pid={pid} ok={cold_ok} enum_ms={:.3}",
        cold.as_secs_f64() * 1000.0,
    ));

    let restored = audio_session_volume(pid, original_level);
    let unmuted = audio_session_mute(pid, original_muted);
    let state = audio_session_get(pid);
    crate::debug_log(&format!(
        "audio-session restore pid={pid} volume={original_level:.3} muted={original_muted} \
         accepted={} readback_volume={:.3} readback_muted={} mismatches={mismatches}",
        restored && unmuted,
        state.map_or(-1.0, |(level, _)| level),
        state.map_or(false, |(_, muted)| muted),
    ));
    Some((original_level, original_muted))
}

/// Second-thread pass: COM is per thread and the session cache is keyed by the
/// thread that opened it, so prove the fader also works from another thread --
/// the production case is the Slint event-loop thread, not this QA thread.
fn audio_session_thread_proof(pid: u32, restore_level: f32) {
    let worker = std::thread::Builder::new()
        .name("audio-session-qa-2".into())
        .spawn(move || {
            let ok = audio_session_volume(pid, 0.30);
            let state = audio_session_get(pid);
            let readback = state.map_or(-1.0, |(level, _)| level);
            let matched = state.map_or(false, |(level, _)| (level - 0.30f32).abs() < 0.01);
            crate::debug_log(&format!(
                "audio-session other-thread pid={pid} want=0.300 ok={ok} readback={readback:.3} \
                 match={matched}"
            ));
            audio_session_volume(pid, restore_level);
        });
    match worker {
        Ok(handle) => {
            let _ = handle.join();
        }
        Err(_) => crate::debug_log("audio-session other-thread spawn failed"),
    }
}

/// `BILI_AUDIO_SESSION_TEST=<pid>|ffplay|spawn` driver (diagnostics only).
///
/// `<pid>` attaches to an existing session, `ffplay` waits for this process's
/// first `ffplay.exe` child, `spawn` starts a private `ffplay` on a sine tone
/// and kills it afterwards. `BILI_AUDIO_SESSION_TEST_WAIT_MS` bounds the wait
/// for a session (default 30 s).
fn run_audio_session_selftest(spec: &str) {
    let wait_ms = env_u64("BILI_AUDIO_SESSION_TEST_WAIT_MS", 30_000);
    let mut owned: Option<std::process::Child> = None;
    let pid = if spec.eq_ignore_ascii_case("spawn") {
        match spawn_selftest_ffplay() {
            Some(child) => {
                let pid = child.id();
                owned = Some(child);
                // Self-check of the `=ffplay` resolution path: the child we just
                // spawned must be visible as `ffplay.exe` under this process.
                let walked = ffplay_child_pids().contains(&pid);
                crate::debug_log(&format!(
                    "audio-session child-walk pid={pid} parent_ok={walked}"
                ));
                Some(pid)
            }
            None => {
                crate::debug_log("audio-session selftest mode=spawn spawned=false");
                None
            }
        }
    } else if spec.eq_ignore_ascii_case("ffplay") {
        wait_for_ffplay_child(wait_ms)
    } else {
        spec.parse::<u32>().ok()
    };
    let Some(pid) = pid else {
        crate::debug_log(&format!(
            "audio-session selftest spec={spec} resolved_pid=none"
        ));
        return;
    };

    if let Some((original_level, _)) = audio_session_proof(pid, wait_ms) {
        audio_session_thread_proof(pid, original_level);
    }

    if let Some(child) = owned.as_mut() {
        // The proof above wrote 200+ levels; the same PID must still be the one
        // running, i.e. no restart happened under the volume changes.
        let alive = matches!(child.try_wait(), Ok(None));
        crate::debug_log(&format!(
            "audio-session process pid={pid} alive={alive} restarts=0"
        ));
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Start the `BILI_AUDIO_SESSION_TEST` thread once, at startup.
fn maybe_start_audio_session_selftest() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Ok(spec) = std::env::var("BILI_AUDIO_SESSION_TEST") else {
        return;
    };
    let spec = spec.trim().to_string();
    if spec.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("audio-session-qa".into())
        .spawn(move || run_audio_session_selftest(&spec));
    if spawned.is_err() {
        crate::debug_log("audio-session selftest thread spawn failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_target_follows_cursor_delta() {
        assert_eq!(
            drag_target((100, 100), (500, 500), (510, 500)),
            (110, 100)
        );
    }

    #[test]
    fn drag_target_never_accumulates_prior_events() {
        assert_eq!(
            drag_target((100, 100), (500, 500), (520, 500)),
            (120, 100)
        );
    }

    #[test]
    fn drag_target_is_idempotent_for_the_same_cursor() {
        let anchor_window = (100, 100);
        let anchor_cursor = (500, 500);
        let cursor = (530, 520);
        let first = drag_target(anchor_window, anchor_cursor, cursor);
        for _ in 0..5 {
            assert_eq!(
                drag_target(anchor_window, anchor_cursor, cursor),
                first
            );
        }
    }

    #[test]
    fn drag_target_without_movement_keeps_window_anchor() {
        assert_eq!(
            drag_target((100, 100), (500, 500), (500, 500)),
            (100, 100)
        );
    }

    #[test]
    fn drag_target_supports_negative_deltas() {
        assert_eq!(
            drag_target((100, 100), (500, 500), (480, 470)),
            (80, 70)
        );
    }

    #[test]
    fn hit_test_is_client_for_the_whole_window() {
        // The rim and the corners are client area now: the OS must never start
        // its modal resize loop (that is what froze the video).
        for (x, y) in [
            (0, 0),
            (799, 0),
            (0, 599),
            (799, 599),
            (2, 300),
            (798, 300),
            (400, 4),
            (400, 596),
            (400, 300),
        ] {
            assert_eq!(border_hit_test(x, y, 800, 600), HTCLIENT);
        }
    }

    #[test]
    fn right_edge_moves_only_the_right_side() {
        let anchor = (100, 100, 900, 700);
        let rect = resized_rect(anchor, (500, 400), (600, 400), 2, (480, 270), (0, 0, 1920, 1080));
        assert_eq!(rect, (100, 100, 1000, 700));
    }

    #[test]
    fn left_edge_moves_only_the_left_side() {
        let anchor = (100, 100, 900, 700);
        let rect = resized_rect(anchor, (500, 400), (450, 400), 1, (480, 270), (0, 0, 1920, 1080));
        assert_eq!(rect, (50, 100, 900, 700));
        // Dragging the left edge right can shrink to the minimum but no more:
        // the right edge stays where it was.
        let rect = resized_rect(anchor, (500, 400), (900, 400), 1, (480, 270), (0, 0, 1920, 1080));
        assert_eq!(rect, (420, 100, 900, 700));
    }

    #[test]
    fn resize_stays_inside_the_work_area() {
        let anchor = (100, 100, 900, 700);
        // Left edge cannot leave the monitor origin.
        let rect = resized_rect(anchor, (500, 400), (-900, 400), 1, (480, 270), (0, 0, 1920, 1080));
        assert_eq!(rect.0, 0);
        // Right/bottom edges cannot leave the work area's far side.
        let rect = resized_rect(
            anchor,
            (500, 400),
            (4000, 4000),
            8,
            (480, 270),
            (0, 0, 1920, 1080),
        );
        assert_eq!(rect, (100, 100, 1920, 1080));
    }

    #[test]
    fn every_edge_keeps_the_opposite_side_anchored() {
        let anchor = (100, 100, 900, 700);
        let moved = resized_rect(
            anchor,
            (500, 400),
            (560, 460),
            0,
            (480, 270),
            (0, 0, 1920, 1080),
        );
        assert_eq!(moved, anchor);
        // An invalid edge must be a no-op, never a guess.
        for edge in 1..=8 {
            let rect = resized_rect(
                anchor,
                (500, 400),
                (560, 460),
                edge,
                (480, 270),
                (0, 0, 1920, 1080),
            );
            let fixed = match edge {
                1 => (rect.1, rect.3) == (anchor.1, anchor.3) && rect.2 == anchor.2,
                2 => (rect.1, rect.3) == (anchor.1, anchor.3) && rect.0 == anchor.0,
                3 => (rect.0, rect.2) == (anchor.0, anchor.2) && rect.3 == anchor.3,
                4 => (rect.0, rect.2) == (anchor.0, anchor.2) && rect.1 == anchor.1,
                5 => rect.2 == anchor.2 && rect.3 == anchor.3,
                6 => rect.0 == anchor.0 && rect.3 == anchor.3,
                7 => rect.2 == anchor.2 && rect.1 == anchor.1,
                _ => rect.0 == anchor.0 && rect.1 == anchor.1,
            };
            assert!(fixed, "edge {edge} moved an anchored side: {rect:?}");
        }
    }

    #[test]
    fn fit_rect_applies_the_minimum_and_the_work_area() {
        assert_eq!(
            fit_rect((500, 500, 510, 510), (480, 270), (0, 0, 1920, 1080)),
            (500, 500, 980, 770)
        );
        assert_eq!(
            fit_rect((1900, 500, 2100, 800), (480, 270), (0, 0, 1920, 1080)),
            (1440, 500, 1920, 800)
        );
    }

    #[test]
    fn create_hook_installs_and_unhooks() {
        let tid = unsafe { GetCurrentThreadId() };
        let hook = unsafe { SetWindowsHookExW(WH_CBT, cbt_proc, 0, tid) };
        assert_ne!(hook, 0);
        assert_ne!(unsafe { UnhookWindowsHookEx(hook) }, 0);
    }

    #[test]
    fn create_hook_strips_the_measured_winit_style() {
        // Measured at t=499ms before the fix: 0x16CF0000 (visible + caption +
        // thickframe + min/max/sysmenu). The create hook must yield 0x160F0000,
        // i.e. the same window with WS_CAPTION gone, before creation.
        let (style, ex_style) = strip_create_styles(0x16CF_0000u32 as i32, 0x0000_0100);
        assert_eq!(style as u32, 0x160F_0000);
        assert_eq!(style & WS_CAPTION as i32, 0);
        assert_eq!(style & WS_THICKFRAME as i32, WS_THICKFRAME as i32);
        assert_eq!(ex_style & WS_EX_WINDOWEDGE as u32, 0);
    }
}
