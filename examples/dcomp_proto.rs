//! f192 step 1 — the DirectComposition prototype, on its own.
//!
//! This is not the player. It is the smallest window that proves the three
//! things the player window would need before any product code is touched:
//!
//! 1. **No redirection bitmap.** The window is created with
//!    `WS_EX_NOREDIRECTIONBITMAP`, so DWM does not own a surface for it, and a
//!    `DCompositionCreateDevice` → `CreateTargetForHwnd` tree is the only thing
//!    it can show. If that fails the window stays black and nothing below
//!    matters.
//! 2. **A video layer that really runs at 120.** A
//!    `CreateSwapChainForComposition` swapchain (`DXGI_ALPHA_MODE_IGNORE`,
//!    `FLIP_SEQUENTIAL`, frame-latency waitable) is fed one 1920x1080 CPU frame
//!    per present and presented with `Present(0, DXGI_PRESENT_DO_NOT_WAIT)`.
//!    The count of presentations that actually returned `S_OK` is the number in
//!    the report — not an estimate and not a queue depth.
//! 3. **A UI layer with real alpha.** A second swapchain
//!    (`DXGI_ALPHA_MODE_PREMULTIPLIED`) draws a translucent toolbar, a
//!    translucent panel, a 50 % red block and antialiased text over the video
//!    layer. It is presented on demand (a much lower rate than the video), which
//!    is how the player's Slint layer would be driven.
//!
//! The proof that the alpha is *correct* is not "it looks right": the video
//! frame and the UI frame are both deterministic buffers, so every sampled pixel
//! of the composed window has a predicted value
//! `ui_premul + video * (1 - a)`. The screenshot is compared against that
//! prediction, channel by channel, and the deltas are printed.
//!
//! Why a GDI `BitBlt` of the screen for the screenshot: `PrintWindow` captures
//! the *redirection surface* of a window, and this window deliberately has
//! none. `PrintWindow` is measured and reported rather than trusted; the screen
//! blit is what the compositor actually scanned out, so it is the only capture
//! that can prove the blend.
//!
//! Run:
//!   cargo run --release --example dcomp_proto -- --seconds 60
//!
//! Reading the numbers:
//! - `--ui-hz 10` is the on-demand mode (the UI redraws when its content
//!   changes); `--ui-hz 60` measures a 60 Hz UI layer.
//! - `--slint-ui` swaps the hand-drawn overlay for a real Slint component
//!   rendered by `SoftwareRenderer` (`set_platform` is process-global, hence a
//!   separate process).
//! - `--no-swizzle` models a decoder that already hands over BGRA; the default
//!   swizzles RGBA→BGRA per frame with SSE2, which is what the player's pacer
//!   produces.

#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{Interface, IUnknown, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE, DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory2, IDXGIOutput,
    IDXGISwapChain1, IDXGISwapChain2, DXGI_ADAPTER_DESC1, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT,
    DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    EndPaint, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW, UpdateWindow,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, ROP_CODE, SRCCOPY, TRANSPARENT,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, WaitForSingleObject, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetForegroundWindow,
    GetWindowRect, IsWindowVisible, PeekMessageW, RegisterClassW, SetForegroundWindow, ShowWindow,
    TranslateMessage, MSG, PM_REMOVE, SW_SHOW, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_PAINT, WNDCLASSW, WNDCLASS_STYLES, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

/// `PW_RENDERFULLCONTENT`: not re-exported next to `PrintWindow` in 0.62.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);

// ---- command line ----------------------------------------------------------

struct Args {
    seconds: u64,
    warmup: u64,
    ui_hz: f64,
    window: (u32, u32),
    video: (u32, u32),
    shot: String,
    report: String,
    swizzle: bool,
    vsync_secs: u64,
    probe_upload: bool,
    slint_ui: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seconds: 60,
            warmup: 3,
            ui_hz: 10.0,
            window: (1920, 1080),
            video: (1920, 1080),
            shot: "target/qa/f192_proto_120.png".into(),
            report: "target/qa/f192_proto_120.txt".into(),
            swizzle: true,
            vsync_secs: 3,
            probe_upload: false,
            slint_ui: false,
        }
    }
}

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < argv.len() {
        let key = argv[index].clone();
        let next = argv.get(index + 1).cloned().unwrap_or_default();
        match key.as_str() {
            "--seconds" => {
                args.seconds = next.parse().unwrap_or(args.seconds);
                index += 1;
            }
            "--warmup" => {
                args.warmup = next.parse().unwrap_or(args.warmup);
                index += 1;
            }
            "--ui-hz" => {
                args.ui_hz = next.parse().unwrap_or(args.ui_hz);
                index += 1;
            }
            "--window" => {
                args.window = parse_size(&next).unwrap_or(args.window);
                index += 1;
            }
            "--video" => {
                args.video = parse_size(&next).unwrap_or(args.video);
                index += 1;
            }
            "--shot" => {
                args.shot = next;
                index += 1;
            }
            "--report" => {
                args.report = next;
                index += 1;
            }
            "--vsync-secs" => {
                args.vsync_secs = next.parse().unwrap_or(args.vsync_secs);
                index += 1;
            }
            "--no-swizzle" => args.swizzle = false,
            "--probe-upload" => args.probe_upload = true,
            "--slint-ui" => args.slint_ui = true,
            other => eprintln!("proto: ignoring unknown argument {other}"),
        }
        index += 1;
    }
    args
}

// ---- statistics ------------------------------------------------------------

/// Present accounting for the video layer. The worker thread owns the swapchain
/// and fills this in; the main thread only reads it.
#[derive(Default)]
struct Counters {
    ok: std::sync::atomic::AtomicU64,
    attempts: std::sync::atomic::AtomicU64,
    busy: std::sync::atomic::AtomicU64,
    occluded: std::sync::atomic::AtomicU64,
    other: std::sync::atomic::AtomicU64,
    other_code: std::sync::atomic::AtomicU32,
    wait_timeouts: std::sync::atomic::AtomicU64,
    stage_ms: std::sync::Mutex<Vec<f64>>,
    intervals_ms: std::sync::Mutex<Vec<f64>>,
    waits_ms: std::sync::Mutex<Vec<f64>>,
}

impl Counters {
    fn note_hr(&self, hr: windows::core::HRESULT) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        match hr.0 as u32 {
            0 => {
                self.ok.fetch_add(1, Ordering::Relaxed);
            }
            // DXGI_ERROR_WAS_STILL_DRAWING
            0x887A_000A => {
                self.busy.fetch_add(1, Ordering::Relaxed);
            }
            // DXGI_STATUS_OCCLUDED
            0x087A_0001 => {
                self.occluded.fetch_add(1, Ordering::Relaxed);
            }
            code => {
                self.other.fetch_add(1, Ordering::Relaxed);
                self.other_code.store(code, Ordering::Relaxed);
            }
        }
    }

    fn clear(&self) {
        for slot in [&self.stage_ms, &self.intervals_ms, &self.waits_ms] {
            if let Ok(mut values) = slot.lock() {
                values.clear();
            }
        }
    }
}

fn pct(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((values.len() as f64 - 1.0) * fraction).round() as usize;
    values[index]
}

// ---- window ----------------------------------------------------------------

static QUIT: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE | WM_DESTROY => {
            QUIT.store(true, Ordering::SeqCst);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == 0x1B => {
            QUIT.store(true, Ordering::SeqCst);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // Nothing is ever painted through GDI: the DComp tree is the window.
            let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            unsafe {
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn create_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let class = wide("O11A1DCompProto");
    let title = wide("O11A1 · f192 DComp prototype");
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: HINSTANCE(instance.0),
            hIcon: Default::default(),
            hCursor: Default::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class.as_ptr()),
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            width as i32,
            height as i32,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        Ok(hwnd)
    }
}

fn pump_messages() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---- GPU ---------------------------------------------------------------

struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    adapter_name: String,
    adapter_note: String,
}

impl Gpu {
    fn new() -> windows::core::Result<Self> {
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            // Prefer the adapter that actually drives a desktop output, then the
            // one with more dedicated memory. On a hybrid machine the first
            // rule is the one that matters: a video layer on the *other* adapter
            // would be composed through a cross-adapter copy every frame.
            let mut best: Option<(IDXGIAdapter1, DXGI_ADAPTER_DESC1, bool)> = None;
            let mut index = 0;
            let mut notes = Vec::new();
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                index += 1;
                let Ok(desc) = adapter.GetDesc1() else { continue };
                let name: String = desc
                    .Description
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
                    .collect();
                let attached = match adapter.EnumOutputs(0) {
                    Ok(output) => output.GetDesc().map(|d| d.AttachedToDesktop.as_bool()).unwrap_or(false),
                    Err(_) => false,
                };
                notes.push(format!(
                    "{name} ({:.0} MB{})",
                    desc.DedicatedVideoMemory as f64 / (1024.0 * 1024.0),
                    if attached { ", desktop" } else { "" }
                ));
                let better = match &best {
                    None => true,
                    Some((_, current, current_attached)) => {
                        (attached && !*current_attached)
                            || (attached == *current_attached
                                && desc.DedicatedVideoMemory > current.DedicatedVideoMemory)
                    }
                };
                if better {
                    best = Some((adapter, desc, attached));
                }
            }
            let Some((adapter, _, attached)) = best else {
                return Err(windows::core::Error::from_thread());
            };
            let adapter_base: IDXGIAdapter = adapter.cast()?;
            let desc = adapter.GetDesc1()?;
            let name: String = desc
                .Description
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
                .collect();
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                Some(&adapter_base),
                // With an explicit adapter, D3D11 only accepts UNKNOWN.
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                7, // D3D11_SDK_VERSION
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
            Ok(Self {
                device: device.ok_or_else(windows::core::Error::from_thread)?,
                context: context.ok_or_else(windows::core::Error::from_thread)?,
                factory,
                adapter_name: name.trim().to_string(),
                adapter_note: format!(
                    "{} chosen (desktop={attached}); candidates: {}",
                    name.trim(),
                    notes.join(" | ")
                ),
            })
        }
    }
}

/// One composed layer: a composition swapchain, the staging texture a frame is
/// written into, and the frame-latency waitable that paces presentation.
struct Layer {
    swap: IDXGISwapChain1,
    context: ID3D11DeviceContext,
    /// The frame-latency waitable, kept as an integer so the layer can move
    /// to the presenting thread (HANDLE is a raw pointer and is not Send).
    waiter: isize,
    /// CPU-writable staging texture. D3D11 refuses `USAGE_DYNAMIC` +
    /// `CPU_ACCESS_WRITE` on this machine (E_INVALIDARG at every size, see
    /// `--probe-upload`), so a staging texture plus one `CopyResource` is the
    /// upload path.
    staging: ID3D11Texture2D,
    /// The GPU-side copy that the backbuffer is filled from. Staging stays in
    /// system memory, which keeps the CPU copy off the compositor's critical
    /// path.
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl Layer {
    fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        alpha: DXGI_ALPHA_MODE,
    ) -> windows::core::Result<Self> {
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: BOOL(0),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: alpha,
                // The waitable is what turns "present as fast as possible and
                // lose half the frames to the compositor" into one present per
                // composition interval: the wait returns exactly when the
                // compositor has room for the next frame.
                Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            };
            let unknown: IUnknown = gpu.device.cast()?;
            let swap = gpu
                .factory
                .CreateSwapChainForComposition(&unknown, &desc, None::<&IDXGIOutput>)?;
            let swap2: IDXGISwapChain2 = swap.cast()?;
            swap2.SetMaximumFrameLatency(1)?;
            let waiter = swap2.GetFrameLatencyWaitableObject().0 as isize;

            let mut staging: Option<ID3D11Texture2D> = None;
            let mut texture: Option<ID3D11Texture2D> = None;
            for (slot, usage, cpu) in [
                (&mut staging, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32),
                (&mut texture, D3D11_USAGE_DEFAULT, 0u32),
            ] {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: usage,
                    BindFlags: 0,
                    CPUAccessFlags: cpu,
                    MiscFlags: 0,
                };
                gpu.device.CreateTexture2D(&desc, None, Some(slot))?;
            }
            Ok(Self {
                swap,
                context: gpu.context.clone(),
                waiter,
                staging: staging.ok_or_else(windows::core::Error::from_thread)?,
                texture: texture.ok_or_else(windows::core::Error::from_thread)?,
                width,
                height,
            })
        }
    }

    /// Wait until the compositor can take another frame; the value is the wait
    /// in ms (a timeout means it did not come back in 200 ms).
    fn wait_frame(&self) -> (f64, bool) {
        let started = Instant::now();
        let result = unsafe { WaitForSingleObject(HANDLE(self.waiter as *mut c_void), 200) };
        (
            started.elapsed().as_secs_f64() * 1000.0,
            result == WAIT_OBJECT_0,
        )
    }

    /// Copy one frame into the staging texture. `src` is RGBA when `swizzle`
    /// is set (the player's pacer output), BGRA premultiplied otherwise (the UI
    /// buffer). This is the 3-4 ms of CPU work at 1920x1080 and it happens
    /// *before* the compositor wait, so it is overlapped with the composition
    /// interval instead of adding to it.
    fn stage(&self, src: &[u8], swizzle: bool) -> windows::core::Result<f64> {
        unsafe {
            let started = Instant::now();
            let row = self.width as usize * 4;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(
                &self.staging,
                0,
                D3D11_MAP_WRITE,
                0,
                Some(&mut mapped),
            )?;
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData as *mut u8;
            if swizzle {
                // One pass: RGBA in the frame → BGRA in the mapped staging
                // memory. Doing it in two (scratch then copy) doubles the
                // memory traffic and costs ~4 ms at 1920x1080.
                for y in 0..self.height as usize {
                    swap_rb_ptr(src.as_ptr().add(y * row), dst.add(y * pitch), row);
                }
            } else {
                for y in 0..self.height as usize {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(y * row),
                        dst.add(y * pitch),
                        row,
                    );
                }
            }
            self.context.Unmap(&self.staging, 0);
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        }
    }

    /// Wait for the compositor, hand the staged frame to the GPU and present it.
    /// Returns the present HRESULT and the time spent waiting.
    fn submit(&self) -> windows::core::Result<(windows::core::HRESULT, f64, bool)> {
        let (waited, signaled) = self.wait_frame();
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.context.CopyResource(&self.texture, &self.staging);
            self.context.CopyResource(&back, &self.texture);
        }
        Ok((
            self.present(DXGI_PRESENT_DO_NOT_WAIT),
            waited,
            signaled,
        ))
    }

    /// Present without waiting for the compositor: used for the UI layer, which
    /// must never stall the video layer (they share this thread in the
    /// prototype; the player will have one thread per layer).
    fn pulse(&self) -> windows::core::Result<windows::core::HRESULT> {
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.context.CopyResource(&self.texture, &self.staging);
            self.context.CopyResource(&back, &self.texture);
        }
        Ok(self.present(DXGI_PRESENT_DO_NOT_WAIT))
    }

    /// Blocking present: guarantees the content is queued, used for the
    /// deterministic capture at the end.
    fn submit_blocking(&self) -> windows::core::Result<()> {
        let _ = self.wait_frame();
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.context.CopyResource(&self.texture, &self.staging);
            self.context.CopyResource(&back, &self.texture);
        }
        let _ = self.present(DXGI_PRESENT(0));
        Ok(())
    }

    fn present(&self, flags: DXGI_PRESENT) -> windows::core::HRESULT {
        unsafe { self.swap.Present(0, flags) }
    }
}

/// RGBA → BGRA, four pixels per iteration (SSE2, baseline on x86_64) —
/// straight from the frame to the mapped staging memory.
unsafe fn swap_rb_ptr(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        swap_rb_sse2(src, dst, len);
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    for index in (0..len).step_by(4) {
        *dst.add(index) = *src.add(index + 2);
        *dst.add(index + 1) = *src.add(index + 1);
        *dst.add(index + 2) = *src.add(index);
        *dst.add(index + 3) = *src.add(index + 3);
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn swap_rb_sse2(src: *const u8, dst: *mut u8, len: usize) {
    use std::arch::x86_64::*;
    let low = _mm_set1_epi32(0x0000_00FFu32 as i32);
    let green = _mm_set1_epi32(0x0000_FF00u32 as i32);
    let alpha = _mm_set1_epi32(0xFF00_0000u32 as i32);
    let mut index = 0usize;
    while index + 16 <= len {
        let chunk = _mm_loadu_si128(src.add(index) as *const __m128i);
        let red = _mm_slli_epi32(_mm_and_si128(chunk, low), 16);
        let blue = _mm_and_si128(_mm_srli_epi32(chunk, 16), low);
        let gray = _mm_and_si128(chunk, green);
        let keep = _mm_and_si128(chunk, alpha);
        // Alpha is carried through untouched: the video layer's frames are
        // opaque anyway, and the UI layer's premultiplied alpha must survive.
        let out = _mm_or_si128(_mm_or_si128(red, blue), _mm_or_si128(gray, keep));
        _mm_storeu_si128(dst.add(index) as *mut __m128i, out);
        index += 16;
    }
    while index + 4 <= len {
        let source = src.add(index);
        let target = dst.add(index);
        *target = *source.add(2);
        *target.add(1) = *source.add(1);
        *target.add(2) = *source;
        *target.add(3) = *source.add(3);
        index += 4;
    }
}

// ---- the video layer's frames ---------------------------------------------

const PATTERNS: usize = 8;

/// One deterministic video frame (RGBA). Everything here is chosen so that a
/// screenshot can be checked against arithmetic instead of eyeballed.
fn build_pattern(index: usize, width: usize, height: usize) -> Vec<u8> {
    let mut frame = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let p = (y * width + x) * 4;
            frame[p] = (8 + (x * 24 / width)) as u8;
            frame[p + 1] = (10 + (y * 30 / height)) as u8;
            frame[p + 2] = (48 + (x * 60 / width)) as u8;
            frame[p + 3] = 255;
        }
    }
    // A solid green test square: the UI's red 50 % block lands on it, so the
    // blended pixel has a value that can be computed on paper.
    fill(
        frame.as_mut_slice(),
        width,
        0,
        0,
        width * 3 / 10,
        height * 37 / 100,
        [0, 255, 0, 255],
    );
    // A block that moves with the frame counter: proof that the video layer is
    // presenting new content, not a still.
    let x0 = width / 3 + index * (width / 40);
    fill(
        frame.as_mut_slice(),
        width,
        x0,
        height * 55 / 100,
        (x0 + width / 5).min(width),
        height * 92 / 100,
        [255, 64, 0, 255],
    );
    // A sweeping line under the moving block.
    let line = height * 96 / 100 + index * 2;
    fill(
        frame.as_mut_slice(),
        width,
        0,
        line,
        width,
        (line + 4).min(height),
        [255, 255, 255, 255],
    );
    frame
}

fn fill(
    frame: &mut [u8],
    width: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    rgba: [u8; 4],
) {
    let height = frame.len() / (width * 4);
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let p = (y * width + x) * 4;
            frame[p..p + 4].copy_from_slice(&rgba);
        }
    }
}

// ---- the UI layer ---------------------------------------------------------

#[derive(Clone, Copy)]
struct UiLayout {
    toolbar: (usize, usize, usize, usize),
    block: (usize, usize, usize, usize),
    panel: (usize, usize, usize, usize),
    danmaku: (usize, usize),
    counter: (usize, usize),
    title: (usize, usize),
}

impl UiLayout {
    fn new(width: usize, height: usize) -> Self {
        Self {
            toolbar: (0, 0, width, (height * 9 / 100).max(48)),
            block: (
                width * 12 / 100,
                height * 16 / 100,
                width * 27 / 100,
                height * 33 / 100,
            ),
            panel: (
                width * 81 / 100,
                height * 17 / 100,
                width * 97 / 100,
                height * 52 / 100,
            ),
            danmaku: (width * 33 / 100, height * 44 / 100),
            counter: (width * 58 / 100, height * 3 / 100),
            title: (width * 2 / 100, height * 3 / 100),
        }
    }

    /// The four pixels the alpha check samples: two where the UI is
    /// translucent over a known video colour, one where opaque content sits on
    /// translucent content, and one where the UI must be fully transparent.
    fn samples(&self, width: usize, height: usize) -> [(&'static str, (usize, usize)); 4] {
        [
            (
                "toolbar 62% over green",
                ((self.toolbar.0 + width / 20), self.toolbar.1 + self.toolbar.3 / 2),
            ),
            (
                "red 50% block over green",
                ((self.block.0 + self.block.2) / 2, (self.block.1 + self.block.3) / 2),
            ),
            (
                "panel 55% over video",
                ((self.panel.0 + self.panel.2) / 2, (self.panel.1 + self.panel.3) / 2),
            ),
            ("video only (ui transparent)", (width * 72 / 100, height * 80 / 100)),
        ]
    }
}

/// The UI layer's CPU buffer: BGRA, **premultiplied**, exactly what a
/// `DXGI_ALPHA_MODE_PREMULTIPLIED` swapchain wants.
struct UiLayer {
    width: usize,
    height: usize,
    buffer: Vec<u8>,
    layout: UiLayout,
    dc: HDC,
    mask: Mask,
}

const MASK_WIDTH: usize = 1024;
const MASK_HEIGHT: usize = 72;

/// A 32-bit top-down DIB that GDI paints white-on-black text into. The grey
/// level of a pixel is the glyph coverage, so the mask doubles as the
/// premultiplied white pixel.
struct Mask {
    bitmap: HBITMAP,
    bits: *mut u8,
}

impl Drop for Mask {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
        }
    }
}

impl UiLayer {
    fn new(width: usize, height: usize) -> windows::core::Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.0.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let info = bitmap_info(MASK_WIDTH, MASK_HEIGHT);
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            if bits.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            SelectObject(dc, HGDIOBJ(bitmap.0));
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            Ok(Self {
                width,
                height,
                buffer: vec![0u8; width * height * 4],
                layout: UiLayout::new(width, height),
                dc,
                mask: Mask {
                    bitmap,
                    bits: bits as *mut u8,
                },
            })
        }
    }

    /// Rebuild the whole UI frame. Only the counter text changes frame to
    /// frame, but the prototype rebuilds everything: this is the worst case a
    /// Slint software-rendered layer would ever hit.
    fn draw(&mut self, frame: u64) {
        self.buffer.fill(0);
        let (width, height) = (self.width, self.height);
        let layout = self.layout;
        rect(&mut self.buffer, width, layout.toolbar, [14, 16, 20], 158);
        rect(&mut self.buffer, width, layout.panel, [18, 20, 26], 140);
        rect(
            &mut self.buffer,
            width,
            (layout.panel.0, layout.panel.1, layout.panel.2, layout.panel.1 + 2),
            [235, 240, 250],
            200,
        );
        // The 50 % red block over the solid green test square, with a fully
        // opaque one-pixel top edge: translucent and opaque content in the same
        // place, both over the video.
        rect(&mut self.buffer, width, layout.block, [255, 0, 0], 128);
        rect(
            &mut self.buffer,
            width,
            (layout.block.0, layout.block.1, layout.block.2, layout.block.1 + 2),
            [255, 255, 255],
            255,
        );
        self.text(layout.danmaku.0, layout.danmaku.1, "这条弹幕位于视频之上 · danmaku over video", 32);
        self.text(layout.title.0, layout.title.1, "O11A1 · DirectComposition 覆盖层", 30);
        let counter = format!("video 120 Hz · ui on demand · frame {frame}");
        self.text(layout.counter.0, layout.counter.1.min(height / 40), &counter, 30);
    }

    /// Draw one antialiased text run into the premultiplied buffer.
    fn text(&mut self, x: usize, y: usize, text: &str, px: i32) {
        unsafe {
            let mask_len = MASK_WIDTH * MASK_HEIGHT * 4;
            std::ptr::write_bytes(self.mask.bits, 0, mask_len);
            let face = wide("Microsoft YaHei UI");
            let font = CreateFontW(
                px,
                0,
                0,
                0,
                500,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                Default::default(),
                Default::default(),
                CLEARTYPE_QUALITY,
                0,
                PCWSTR(face.as_ptr()),
            );
            if font.0.is_null() {
                return;
            }
            let previous = SelectObject(self.dc, HGDIOBJ(font.0));
            let units = wide(text);
            let _ = TextOutW(self.dc, 0, 0, &units);
            SelectObject(self.dc, previous);
            let _ = DeleteObject(HGDIOBJ(font.0));

            let mask = std::slice::from_raw_parts(self.mask.bits, mask_len);
            for row in 0..MASK_HEIGHT {
                let ty = y + row;
                if ty >= self.height {
                    break;
                }
                for column in 0..MASK_WIDTH {
                    let tx = x + column;
                    if tx >= self.width {
                        break;
                    }
                    let coverage = mask[(row * MASK_WIDTH + column) * 4] as u32;
                    if coverage == 0 {
                        continue;
                    }
                    // Premultiplied white over the premultiplied destination.
                    let inverse = 255 - coverage;
                    let p = (ty * self.width + tx) * 4;
                    for channel in 0..4 {
                        let destination = self.buffer[p + channel] as u32;
                        self.buffer[p + channel] =
                            (coverage + (destination * inverse + 127) / 255).min(255) as u8;
                    }
                }
            }
        }
    }
}

impl Drop for UiLayer {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Fill a rectangle of the premultiplied BGRA buffer with `rgb` at `alpha`,
/// compositing over whatever is already there (source-over, premultiplied).
fn rect(
    buffer: &mut [u8],
    width: usize,
    area: (usize, usize, usize, usize),
    rgb: [u8; 3],
    alpha: u8,
) {
    let (x0, y0, x1, y1) = area;
    let a = alpha as u32;
    let source = [
        (rgb[2] as u32 * a / 255) as u8,
        (rgb[1] as u32 * a / 255) as u8,
        (rgb[0] as u32 * a / 255) as u8,
        alpha,
    ];
    let height = buffer.len() / (width * 4);
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let p = (y * width + x) * 4;
            if alpha == 255 {
                buffer[p..p + 4].copy_from_slice(&source);
                continue;
            }
            let inverse = 255 - a;
            for channel in 0..4 {
                let destination = buffer[p + channel] as u32;
                buffer[p + channel] =
                    (source[channel] as u32 + (destination * inverse + 127) / 255).min(255) as u8;
            }
        }
    }
}

fn bitmap_info(width: usize, height: usize) -> BITMAPINFO {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        // Top-down rows: the buffers are written that way.
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0 as u32,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    info
}

// ---- capture ---------------------------------------------------------------

/// One captured image: BGRA, top-down.
struct Shot {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Shot {
    fn bgra(&self, x: usize, y: usize) -> [u8; 4] {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let p = (y * self.width + x) * 4;
        [
            self.pixels[p],
            self.pixels[p + 1],
            self.pixels[p + 2],
            self.pixels[p + 3],
        ]
    }

    fn nonblack_fraction(&self) -> f64 {
        let mut lit = 0usize;
        let mut total = 0usize;
        for chunk in self.pixels.chunks_exact(4) {
            total += 1;
            if chunk[0] as u32 + chunk[1] as u32 + chunk[2] as u32 > 12 {
                lit += 1;
            }
        }
        lit as f64 / total.max(1) as f64
    }
}

fn dib_shot(
    draw: impl FnOnce(HDC) -> bool,
    width: usize,
    height: usize,
) -> windows::core::Result<Shot> {
    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let memory = CreateCompatibleDC(Some(screen));
        let info = bitmap_info(width, height);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        SelectObject(memory, HGDIOBJ(bitmap.0));
        let drawn = draw(memory);
        let mut pixels = vec![0u8; width * height * 4];
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits as *const u8, pixels.as_mut_ptr(), pixels.len());
        }
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        if !drawn {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Shot {
            width,
            height,
            pixels,
        })
    }
}

fn capture_rect(rect: RECT) -> windows::core::Result<Shot> {
    let width = (rect.right - rect.left).max(1) as usize;
    let height = (rect.bottom - rect.top).max(1) as usize;
    dib_shot(
        |memory| {
            let screen = unsafe { GetDC(None) };
            let rop = ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0);
            let result = unsafe {
                BitBlt(
                    memory,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    Some(screen),
                    rect.left,
                    rect.top,
                    rop,
                )
            };
            unsafe { ReleaseDC(None, screen) };
            result.is_ok()
        },
        width,
        height,
    )
}

fn print_window(hwnd: HWND, width: usize, height: usize) -> windows::core::Result<Shot> {
    dib_shot(
        |memory| unsafe { PrintWindow(hwnd, memory, PW_RENDERFULLCONTENT).as_bool() },
        width,
        height,
    )
}

fn save_png(shot: &Shot, path: &str) -> bool {
    let mut rgba = vec![0u8; shot.pixels.len()];
    for (source, target) in shot.pixels.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
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
        shot.width as u32,
        shot.height as u32,
        image::ExtendedColorType::Rgba8,
    )
    .is_ok()
}

// ---- composition -----------------------------------------------------------

struct Composition {
    device: IDCompositionDevice,
    target: IDCompositionTarget,
    root: IDCompositionVisual,
}

impl Composition {
    fn new(hwnd: HWND, gpu: &Gpu, layers: &[&Layer]) -> windows::core::Result<Self> {
        unsafe {
            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let device: IDCompositionDevice = DCompositionCreateDevice(Some(&dxgi))?;
            let target = device.CreateTargetForHwnd(hwnd, true)?;
            let root = device.CreateVisual()?;
            target.SetRoot(&root)?;
            let mut previous: Option<IDCompositionVisual> = None;
            for layer in layers {
                let visual = device.CreateVisual()?;
                let unknown: IUnknown = layer.swap.cast()?;
                visual.SetContent(&unknown)?;
                visual.SetOffsetX2(0.0)?;
                visual.SetOffsetY2(0.0)?;
                match &previous {
                    Some(reference) => root.AddVisual(&visual, true, reference)?,
                    None => root.AddVisual(&visual, true, None::<&IDCompositionVisual>)?,
                }
                previous = Some(visual);
            }
            device.Commit()?;
            Ok(Self {
                device,
                target,
                root,
            })
        }
    }
}

impl Drop for Composition {
    fn drop(&mut self) {
        unsafe {
            let _ = self.target.SetRoot(None::<&IDCompositionVisual>);
            let _ = self.root.RemoveAllVisuals();
            let _ = self.device.Commit();
        }
    }
}

// ---- the run ---------------------------------------------------------------

/// Diagnostic: which texture shapes this driver will create, and how fast each
/// CPU→GPU upload path is for one 1920x1080 frame. Run with `--probe-upload`.
fn probe_upload_paths(gpu: &Gpu) {
    use windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ;
    unsafe {
        let shapes = [
            ("1920x1080", 1920u32, 1080u32),
            ("960x540", 960, 540),
            ("64x64", 64, 64),
        ];
        for (label, usage, cpu) in [
            ("dynamic+write", D3D11_USAGE_DYNAMIC, D3D11_CPU_ACCESS_WRITE.0 as u32),
            ("staging+read", D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32),
            ("default", D3D11_USAGE_DEFAULT, 0u32),
        ] {
            for (size, width, height) in shapes {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: usage,
                    BindFlags: 0,
                    CPUAccessFlags: cpu,
                    MiscFlags: 0,
                };
                let mut texture: Option<ID3D11Texture2D> = None;
                match gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) {
                    Ok(()) => println!("proto: probe texture {label} {size} ok"),
                    Err(error) => println!(
                        "proto: probe texture {label} {size} FAIL 0x{:08X}",
                        error.code().0 as u32
                    ),
                }
            }
        }
        // What one 1920x1080 frame costs through each path.
        let width = 1920usize;
        let height = 1080usize;
        let row = width * 4;
        let source = vec![0x40u8; row * height];
        let mut plain_dst = vec![0u8; row * height];
        let mut times = Vec::new();
        for _ in 0..60 {
            let started = Instant::now();
            plain_dst.copy_from_slice(&source);
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "proto: probe upload raw-memcpy p50={:.2} p95={:.2} ms",
            times[times.len() / 2],
            times[times.len() * 95 / 100]
        );

        for (label, usage, cpu, non_temporal) in [
            ("staging+map", D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32, false),
            ("staging+map-nt", D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32, true),
            ("default+update", D3D11_USAGE_DEFAULT, 0u32, false),
        ] {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width as u32,
                Height: height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: usage,
                BindFlags: 0,
                CPUAccessFlags: cpu,
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            if gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)).is_err() {
                continue;
            }
            let texture = texture.unwrap();
            let mut target_desc = desc;
            target_desc.Usage = D3D11_USAGE_DEFAULT;
            target_desc.CPUAccessFlags = 0;
            let mut target: Option<ID3D11Texture2D> = None;
            if gpu.device.CreateTexture2D(&target_desc, None, Some(&mut target)).is_err() {
                continue;
            }
            let target = target.unwrap();
            let mut times = Vec::new();
            let mut pitch_seen = 0usize;
            for _ in 0..60 {
                let started = Instant::now();
                if usage == D3D11_USAGE_DEFAULT {
                    gpu.context
                        .UpdateSubresource(&texture, 0, None, source.as_ptr() as *const c_void, row as u32, 0);
                } else {
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    if gpu
                        .context
                        .Map(&texture, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                        .is_ok()
                    {
                        let pitch = mapped.RowPitch as usize;
                        pitch_seen = pitch;
                        let dst = mapped.pData as *mut u8;
                        if non_temporal && (dst as usize) % 16 == 0 && pitch % 16 == 0 {
                            for y in 0..height {
                                nt_copy(dst.add(y * pitch), source.as_ptr().add(y * row), row);
                            }
                        } else {
                            for y in 0..height {
                                std::ptr::copy_nonoverlapping(
                                    source.as_ptr().add(y * row),
                                    dst.add(y * pitch),
                                    row,
                                );
                            }
                        }
                        gpu.context.Unmap(&texture, 0);
                    }
                    gpu.context.CopyResource(&target, &texture);
                }
                times.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "proto: probe upload {label} p50={:.2} p95={:.2} ms (pitch={pitch_seen}, base={})",
                times[times.len() / 2],
                times[times.len() * 95 / 100],
                if usage == D3D11_USAGE_DEFAULT { "n/a".to_string() } else { "via map".to_string() }
            );
        }
    }
}

/// Non-temporal 16-byte stores: the fast way to fill memory the CPU should not
/// cache (a mapped staging texture on most drivers).
unsafe fn nt_copy(dst: *mut u8, src: *const u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        let mut index = 0usize;
        while index + 64 <= len {
            let a = _mm_loadu_si128(src.add(index) as *const __m128i);
            let b = _mm_loadu_si128(src.add(index + 16) as *const __m128i);
            let c = _mm_loadu_si128(src.add(index + 32) as *const __m128i);
            let d = _mm_loadu_si128(src.add(index + 48) as *const __m128i);
            _mm_stream_si128(dst.add(index) as *mut __m128i, a);
            _mm_stream_si128(dst.add(index + 16) as *mut __m128i, b);
            _mm_stream_si128(dst.add(index + 32) as *mut __m128i, c);
            _mm_stream_si128(dst.add(index + 48) as *mut __m128i, d);
            index += 64;
        }
        while index < len {
            *dst.add(index) = *src.add(index);
            index += 1;
        }
        _mm_sfence();
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::copy_nonoverlapping(src, dst, len);
}

fn main() {
    let args = parse_args();
    if args.probe_upload {
        unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
        match Gpu::new() {
            Ok(gpu) => {
                println!("proto: adapter {}", gpu.adapter_name);
                probe_upload_paths(&gpu);
            }
            Err(error) => println!("proto: probe FAIL {error:?}"),
        }
        return;
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        timeBeginPeriod(1);
    }
    let code = match run(&args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("proto: FAIL {error:?}");
            println!("proto: FAIL code=0x{:08X} {error:?}", error.code().0 as u32);
            1
        }
    };
    unsafe { timeEndPeriod(1) };
    std::process::exit(code);
}

fn run(args: &Args) -> windows::core::Result<()> {
    let (width, height) = args.window;
    let (video_width, video_height) = args.video;
    if video_width != width || video_height != height {
        println!(
            "proto: --video {video_width}x{video_height} is drawn 1:1 and must equal --window {width}x{height}"
        );
        return Ok(());
    }
    let hwnd = create_window(width, height)?;
    let mut client: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
    }
    println!(
        "proto: window hwnd={:?} client={}x{} ex=WS_EX_NOREDIRECTIONBITMAP",
        hwnd.0, client.right, client.bottom
    );

    // Two devices, one per layer: a D3D11 immediate context is not thread-safe,
    // and the video layer presents on its own thread while the UI layer presents
    // on this one. That is the shape the player window needs, so the prototype
    // measures it rather than a single-threaded simplification.
    let video_gpu = Gpu::new()?;
    let ui_gpu = Gpu::new()?;
    println!("proto: adapter {}", video_gpu.adapter_note);
    let video = Layer::new(&video_gpu, video_width, video_height, DXGI_ALPHA_MODE_IGNORE)?;
    let ui = Layer::new(&ui_gpu, width, height, DXGI_ALPHA_MODE_PREMULTIPLIED)?;
    println!(
        "proto: swapchains video={video_width}x{video_height} IGNORE ui={width}x{height} PREMULTIPLIED flip-seq 2 buffers, waitable latency 1"
    );
    let composition = Composition::new(hwnd, &video_gpu, &[&video, &ui])?;
    println!("proto: dcomp device+target+root+2 visuals committed");

    let mut patterns: Vec<Vec<u8>> = (0..PATTERNS)
        .map(|index| build_pattern(index, video_width as usize, video_height as usize))
        .collect();
    if !args.swizzle {
        // `--no-swizzle` models a decoder that already hands over BGRA, so the
        // frames themselves are swapped here and the upload is a pure copy.
        for frame in &mut patterns {
            for pixel in frame.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
    }
    let patterns = Arc::new(patterns);
    let mut ui_layer: Box<dyn UiSource> = if args.slint_ui {
        match SlintUi::new(width as usize, height as usize) {
            Ok(ui) => {
                println!("proto: ui layer = Slint (software renderer, premultiplied)");
                Box::new(ui)
            }
            Err(error) => {
                println!("proto: slint ui FAILED {error}");
                return Ok(());
            }
        }
    } else {
        println!("proto: ui layer = synthetic GDI overlay (premultiplied)");
        Box::new(UiLayer::new(width as usize, height as usize)?)
    };

    // --- the video layer's own thread ---------------------------------------
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let freeze = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let counters = Arc::new(Counters::default());
    let worker = {
        let stop = stop.clone();
        let measuring = measuring.clone();
        let freeze = freeze.clone();
        let counters = counters.clone();
        let patterns = patterns.clone();
        let swizzle = args.swizzle;
        std::thread::Builder::new()
            .name("video-present".into())
            .spawn(move || -> windows::core::Result<Layer> {
                // The presenting thread owns a 3-5 ms copy and a vblank
                // deadline; it must not be preempted by the UI thread or the
                // rest of the desktop. The player window's pacer thread needs
                // the same treatment.
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                }
                let mut index = 1 % patterns.len();
                let mut stage_ms = video.stage(&patterns[0], swizzle)?;
                let mut last_present: Option<Instant> = None;
                while !stop.load(Ordering::SeqCst) {
                    let (hr, waited, signaled) = video.submit()?;
                    if measuring.load(Ordering::SeqCst) {
                        counters.note_hr(hr);
                        if !signaled {
                            counters.wait_timeouts.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Ok(mut slot) = counters.stage_ms.lock() {
                            slot.push(stage_ms);
                        }
                        if let Ok(mut slot) = counters.waits_ms.lock() {
                            slot.push(waited);
                        }
                        if hr.0 == 0 {
                            if let Some(previous) = last_present.replace(Instant::now()) {
                                if let Ok(mut slot) = counters.intervals_ms.lock() {
                                    slot.push(previous.elapsed().as_secs_f64() * 1000.0);
                                }
                            }
                        }
                    }
                    // The pacer's next frame. While frozen the same pattern is
                    // re-presented, so a capture has a known picture.
                    let frozen = freeze.load(Ordering::Relaxed);
                    if frozen < patterns.len() {
                        stage_ms = video.stage(&patterns[frozen], swizzle)?;
                    } else {
                        stage_ms = video.stage(&patterns[index], swizzle)?;
                        index = (index + 1) % patterns.len();
                    }
                }
                Ok(video)
            })?
    };

    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
    let mut frame_no = 0u64;
    // --- warmup -------------------------------------------------------------
    let _ = run_ui(
        &ui,
        ui_layer.as_mut(),
        args,
        Duration::from_secs(args.warmup),
        &mut frame_no,
        &mut Vec::new(),
    );
    // --- the measured phase -------------------------------------------------
    println!("proto: measuring {} s", args.seconds);
    counters.clear();
    measuring.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let mut ui_frame_ms: Vec<f64> = Vec::new();
    let ui_frames = run_ui(
        &ui,
        ui_layer.as_mut(),
        args,
        Duration::from_secs(args.seconds),
        &mut frame_no,
        &mut ui_frame_ms,
    );
    let elapsed = started.elapsed().as_secs_f64();
    measuring.store(false, Ordering::SeqCst);

    // --- the 4 ms probe -----------------------------------------------------
    let mut probe = Vec::new();
    for _ in 0..100 {
        let probe_started = Instant::now();
        std::thread::sleep(Duration::from_millis(4));
        probe.push(probe_started.elapsed().as_secs_f64() * 1000.0);
    }

    // --- freeze on a known frame, capture, verify ---------------------------
    freeze.store(0, Ordering::SeqCst);
    ui_layer.build(999_999);
    let (bytes, swizzle) = ui_layer.upload_bytes();
    let _ = ui.stage(bytes, swizzle);
    let _ = ui.submit_blocking();
    std::thread::sleep(Duration::from_millis(200));
    pump_messages();

    let mut frame_rect: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        let _ = GetWindowRect(hwnd, &mut frame_rect);
    }
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let foreground = unsafe { GetForegroundWindow() } == hwnd;
    println!("proto: capture visible={visible} foreground={foreground}");
    let shot = capture_rect(frame_rect);
    let samples = ui_layer.samples();
    let mut checks: Vec<(String, [u8; 3], [u8; 3])> = Vec::new();
    let mut shot_saved = false;
    let mut shot_pixels = (0usize, 0usize);
    let video_frame = &patterns[0];
    let expected_at = |index: usize| -> [u8; 3] {
        let (_, (x, y)) = samples[index];
        let p = (y * width as usize + x) * 4;
        let ui_pixel = ui_layer.sample(x, y);
        let alpha = ui_pixel[3] as u32;
        let inverse = 255 - alpha;
        let video_pixel = &video_frame[p..p + 4];
        // The video layer's settled channel order: a swizzled frame is BGRA on
        // the GPU, a `--no-swizzle` frame is already BGRA in memory.
        let video_bgra = if args.swizzle {
            [video_pixel[2], video_pixel[1], video_pixel[0]]
        } else {
            [video_pixel[0], video_pixel[1], video_pixel[2]]
        };
        let mut expected = [0u8; 3];
        for channel in 0..3 {
            expected[channel] = (ui_pixel[channel] as u32
                + (video_bgra[channel] as u32 * inverse + 127) / 255)
                .min(255) as u8;
        }
        expected
    };
    match &shot {
        Ok(image) => {
            for (index, (label, (x, y))) in samples.iter().enumerate() {
                let actual = image.bgra(*x, *y);
                checks.push((
                    label.to_string(),
                    expected_at(index),
                    [actual[0], actual[1], actual[2]],
                ));
            }
            shot_pixels = (image.width, image.height);
            shot_saved = save_png(image, &args.shot);
            println!(
                "proto: shot path={} size={}x{} nonblack={:.1}% saved={shot_saved}",
                args.shot,
                image.width,
                image.height,
                image.nonblack_fraction() * 100.0
            );
        }
        Err(error) => println!(
            "proto: shot FAILED code=0x{:08X} {error:?}",
            error.code().0 as u32
        ),
    }

    // --- PrintWindow --------------------------------------------------------
    let print_window_text = match print_window(hwnd, width as usize, height as usize) {
        Ok(image) => {
            let fraction = image.nonblack_fraction();
            let path = args.shot.replace(".png", "_printwindow.png");
            let expected = expected_at(1);
            let actual = image.bgra(samples[1].1 .0, samples[1].1 .1);
            let delta = (0..3)
                .map(|channel| actual[channel] as i32 - expected[channel] as i32)
                .map(i32::abs)
                .max()
                .unwrap_or(99);
            let _ = save_png(&image, &path);
            format!(
                "ok=1 nonblack={:.1}% alpha_delta={delta} path={path}",
                fraction * 100.0
            )
        }
        Err(error) => format!("ok=0 code=0x{:08X}", error.code().0 as u32),
    };

    // --- stop the video thread, then compare with a vsync-locked present -----
    stop.store(true, Ordering::SeqCst);
    let video = match worker.join() {
        Ok(Ok(layer)) => Some(layer),
        Ok(Err(error)) => {
            println!("proto: video thread failed {error:?}");
            None
        }
        Err(_) => None,
    };
    let vsync_fps = match (&video, args.vsync_secs) {
        (Some(video), seconds) if seconds > 0 => {
            let end = Instant::now() + Duration::from_secs(seconds);
            let mut ok = 0u64;
            let mut index = 0usize;
            let vsync_started = Instant::now();
            while Instant::now() < end {
                pump_messages();
                let _ = video.stage(&patterns[index % PATTERNS], args.swizzle);
                index += 1;
                if let Ok((hr, _, _)) = video.submit() {
                    if hr.0 == 0 {
                        ok += 1;
                    }
                }
            }
            ok as f64 / vsync_started.elapsed().as_secs_f64()
        }
        _ => 0.0,
    };

    // --- report -------------------------------------------------------------
    let stats = &counters;
    let ok = stats.ok.load(Ordering::Relaxed);
    let fps = ok as f64 / elapsed;
    let mut stage = stats
        .stage_ms
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default();
    let mut intervals = stats
        .intervals_ms
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default();
    let mut waits = stats
        .waits_ms
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default();
    let mut probe_values = probe.clone();
    let attempts = stats.attempts.load(Ordering::Relaxed);
    let busy = stats.busy.load(Ordering::Relaxed);
    let occluded = stats.occluded.load(Ordering::Relaxed);
    let other = stats.other.load(Ordering::Relaxed);
    let other_code = stats.other_code.load(Ordering::Relaxed);
    let wait_timeouts = stats.wait_timeouts.load(Ordering::Relaxed);
    println!(
        "proto: video ok={ok} attempts={attempts} busy={busy} occluded={occluded} other={other} (last=0x{other_code:08X}) wait_timeouts={wait_timeouts}"
    );
    println!("proto: video fps={fps:.2} over {elapsed:.3} s");
    let interval_p50 = pct(&mut intervals, 0.5);
    let interval_p95 = pct(&mut intervals, 0.95);
    let stage_p50 = pct(&mut stage, 0.5);
    let stage_p95 = pct(&mut stage, 0.95);
    println!("proto: video interval p50={interval_p50:.2} p95={interval_p95:.2} ms");
    println!(
        "proto: video stage p50={stage_p50:.2} p95={stage_p95:.2} ms (swizzle={})",
        args.swizzle
    );
    println!(
        "proto: video frame-wait p50={:.2} p95={:.2} ms",
        pct(&mut waits, 0.5),
        pct(&mut waits, 0.95)
    );
    let mut ui_frame_ms_sorted = ui_frame_ms.clone();
    println!(
        "proto: ui frames={ui_frames} rate={:.2}/s (target {:.0}/s) build+stage p50={:.2} p95={:.2} ms",
        ui_frames as f64 / elapsed,
        args.ui_hz,
        pct(&mut ui_frame_ms_sorted, 0.5),
        pct(&mut ui_frame_ms_sorted, 0.95)
    );
    println!(
        "proto: 4ms probe p50={:.2} p95={:.2} ms",
        pct(&mut probe_values, 0.5),
        pct(&mut probe_values, 0.95)
    );
    println!("proto: printwindow {print_window_text}");
    let mut worst = 0i32;
    for (label, expected, actual) in &checks {
        let delta: Vec<i32> = (0..3)
            .map(|i| actual[i] as i32 - expected[i] as i32)
            .collect();
        let max = delta.iter().map(|d| d.abs()).max().unwrap_or(99);
        worst = worst.max(max);
        println!(
            "proto: alpha {label}: expected=({},{},{}) actual=({},{},{}) delta=({},{},{})",
            expected[0], expected[1], expected[2], actual[0], actual[1], actual[2],
            delta[0], delta[1], delta[2]
        );
    }
    println!("proto: alpha worst_delta={worst} samples={}", checks.len());
    println!("proto: vsync presents={vsync_fps:.2}/s");

    let pass = fps >= 115.0 && worst <= 6 && checks.len() == 4 && shot_saved;
    println!("proto: VERDICT {}", if pass { "PASS" } else { "FAIL" });

    let mut text = String::new();
    text.push_str("f192 dcomp prototype\n");
    text.push_str(&format!("adapter={}\n", video_gpu.adapter_name));
    text.push_str(&format!(
        "window={width}x{height} client={}x{}\n",
        client.right, client.bottom
    ));
    text.push_str(&format!(
        "shot={} size={}x{}\n",
        args.shot, shot_pixels.0, shot_pixels.1
    ));
    text.push_str(&format!("video_presents_ok={ok}\n"));
    text.push_str(&format!("video_fps={fps:.2}\n"));
    text.push_str(&format!(
        "video_attempts={attempts} busy={busy} occluded={occluded} other={other}\n"
    ));
    text.push_str(&format!("video_stage_p50={stage_p50:.2}\n"));
    text.push_str(&format!("video_interval_p50={interval_p50:.2}\n"));
    text.push_str(&format!(
        "ui_frames={ui_frames} ui_rate={:.2}\n",
        ui_frames as f64 / elapsed
    ));
    text.push_str(&format!("alpha_worst_delta={worst}\n"));
    text.push_str(&format!("printwindow={print_window_text}\n"));
    text.push_str(&format!("vsync_fps={vsync_fps:.2}\n"));
    text.push_str(&format!("verdict={}\n", if pass { "PASS" } else { "FAIL" }));
    if let Some(parent) = std::path::Path::new(&args.report).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&args.report, text);
    drop(composition);
    Ok(())
}

/// The UI thread's loop: rebuild the overlay on its own clock, present it
/// without ever waiting on the compositor (a present that lands is a bonus, the
/// next tick retries), and keep the window's message pump alive.
fn run_ui(
    ui: &Layer,
    ui_layer: &mut dyn UiSource,
    args: &Args,
    duration: Duration,
    frame_no: &mut u64,
    frame_ms: &mut Vec<f64>,
) -> u64 {
    let end = Instant::now() + duration;
    let period = if args.ui_hz > 0.0 {
        Duration::from_secs_f64(1.0 / args.ui_hz)
    } else {
        Duration::from_secs(3600)
    };
    let mut frames = 0u64;
    let mut next = Instant::now();
    let mut pending = false;
    while Instant::now() < end && !QUIT.load(Ordering::SeqCst) {
        pump_messages();
        let now = Instant::now();
        if now >= next {
            *frame_no += 1;
            let started = Instant::now();
            ui_layer.build(*frame_no);
            let (bytes, swizzle) = ui_layer.upload_bytes();
            let _ = ui.stage(bytes, swizzle);
            frame_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            match ui.pulse() {
                Ok(hr) if hr.0 == 0 => {
                    frames += 1;
                    pending = false;
                }
                _ => pending = true,
            }
            next = now + period;
        } else if pending {
            if let Ok(hr) = ui.pulse() {
                if hr.0 == 0 {
                    frames += 1;
                    pending = false;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    frames
}

// ---- the Slint UI layer ----------------------------------------------------
//
// The second half of the plan: the overlay is not hand-drawn, it is a real
// Slint component (`default-font-family` and the bundled fonts included, like
// `ui/app.slint`), rendered by `SoftwareRenderer` into a **premultiplied** RGBA
// buffer and uploaded as the UI swapchain. The buffer is the same kind of thing
// the player window would produce.
//
// `slint::platform::set_platform` is process-global and one-shot, which is why
// this prototype has its own process (`--slint-ui`). That constraint is the one
// thing that decides how the player window can be wired in — see the f192
// report.

slint::slint! {
    export component PlayerOverlay inherits Window {
        in property <int> tick: 0;
        in property <length> shift: 0px;
        in property <string> label: "";
        default-font-family: "O11A1 Round";
        background: transparent;

        // Toolbar band, 65 % opaque, over the video's green test square.
        Rectangle {
            x: 0; y: 0;
            width: parent.width;
            height: parent.height * 9 / 100;
            background: #0e1014a6;
        }
        // The 50 % red block: the alpha test with a known value.
        Rectangle {
            x: parent.width * 12 / 100;
            y: parent.height * 16 / 100;
            width: parent.width * 15 / 100;
            height: parent.height * 17 / 100;
            background: #ff000080;
        }
        // Info panel, 55 % opaque, over the video gradient.
        Rectangle {
            x: parent.width * 81 / 100;
            y: parent.height * 17 / 100;
            width: parent.width * 16 / 100;
            height: parent.height * 35 / 100;
            background: #12141a8c;
            border-radius: 14px;
        }
        Text {
            x: 24px; y: 18px;
            text: "O11A1 · 播放器 · DComp 覆盖层（Slint 软件渲染）";
            color: #f2f4f8;
            font-size: 26px;
        }
        Text {
            x: parent.width * 58 / 100; y: 20px;
            text: root.label;
            color: #cfd6e4;
            font-size: 24px;
        }
        for i in 5 : Text {
            x: 200px + root.shift + i * 30px;
            y: parent.height * 40 / 100 + i * 40px;
            text: "这条弹幕位于视频之上 · danmaku over video";
            color: #ffffff;
            font-size: 28px;
        }
    }
}

/// The UI layer's contract: rebuild yourself for `tick`, expose premultiplied
/// **BGRA** pixels, and say where your four alpha-test pixels are.
trait UiSource {
    fn build(&mut self, tick: u64);
    /// The buffer to upload, as premultiplied bytes, and whether it still has
    /// to be swizzled RGB→BGR on the way into the staging texture.
    fn upload_bytes(&self) -> (&[u8], bool);
    /// One pixel, premultiplied BGRA (the alpha test's common currency).
    fn sample(&self, x: usize, y: usize) -> [u8; 4];
    fn samples(&self) -> [(&'static str, (usize, usize)); 4];
}

struct SoftPlatform {
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
}

impl slint::platform::Platform for SoftPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

struct SlintUi {
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    ui: PlayerOverlay,
    width: usize,
    /// Premultiplied RGBA, exactly what `SoftwareRenderer` writes.
    buffer: Vec<slint::platform::software_renderer::PremultipliedRgbaColor>,
    painted_once: bool,
    points: [(&'static str, (usize, usize)); 4],
}

impl SlintUi {
    fn new(width: usize, height: usize) -> Result<Self, String> {
        use slint::platform::WindowAdapter as _;
        use slint::ComponentHandle as _;
        let window =
            slint::platform::software_renderer::MinimalSoftwareWindow::new(
                slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
            );
        slint::platform::set_platform(Box::new(SoftPlatform {
            window: window.clone(),
        }))
        .map_err(|error| error.to_string())?;
        let ui = PlayerOverlay::new().map_err(|error| error.to_string())?;
        // The player's faces: app.slint imports these into the component, an
        // example has to register them at run time. Registering them is enough
        // for the software renderer to shape CJK text.
        for path in [
            "assets/fonts/O11A1Round-Regular.ttf",
            "assets/fonts/O11A1Round-Bold.ttf",
        ] {
            let _ = window
                .renderer()
                .register_font_from_path(std::path::Path::new(path));
        }
        ui.window()
            .set_size(slint::PhysicalSize::new(width as u32, height as u32));
        ui.show().map_err(|error| error.to_string())?;
        let points = [
            ("slint toolbar 65% over green", (width / 20, height * 9 / 200)),
            (
                "slint red 50% block over green",
                (width * 195 / 1000, height * 245 / 1000),
            ),
            (
                "slint panel 55% over video",
                (width * 89 / 100, height * 345 / 1000),
            ),
            ("video only (ui transparent)", (width * 72 / 100, height * 80 / 100)),
        ];
        Ok(Self {
            window,
            ui,
            width,
            buffer: vec![Default::default(); width * height],
            painted_once: false,
            points,
        })
    }
}

impl UiSource for SlintUi {
    fn build(&mut self, tick: u64) {
        use slint::platform::software_renderer::RepaintBufferType;
        self.ui.set_tick(tick as i32);
        self.ui.set_shift((tick % 90) as f32 * 3.0);
        self.ui
            .set_label(format!("frame {tick} · 120 Hz").as_str().into());
        let window = self.window.clone();
        let mut buffer = std::mem::take(&mut self.buffer);
        let width = self.width;
        let first = !self.painted_once;
        window.draw_if_needed(|renderer| {
            if first {
                // The very first paint has to cover the whole window: the
                // buffer is fresh and the damage tracker has no history for it.
                renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            }
            renderer.render(&mut buffer, width);
            if first {
                renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
            }
        });
        self.painted_once = true;
        self.buffer = buffer;
    }

    fn upload_bytes(&self) -> (&[u8], bool) {
        // `PremultipliedRgbaColor` is four bytes in R,G,B,A order; the staging
        // upload swizzles it to the swapchain's B,G,R,A in the same pass, so no
        // intermediate conversion buffer is needed.
        debug_assert_eq!(
            std::mem::size_of::<slint::platform::software_renderer::PremultipliedRgbaColor>(),
            4
        );
        let bytes = unsafe {
            std::slice::from_raw_parts(
                self.buffer.as_ptr() as *const u8,
                self.buffer.len() * 4,
            )
        };
        (bytes, true)
    }

    fn sample(&self, x: usize, y: usize) -> [u8; 4] {
        let pixel = self.buffer[y * self.width + x];
        [pixel.blue, pixel.green, pixel.red, pixel.alpha]
    }

    fn samples(&self) -> [(&'static str, (usize, usize)); 4] {
        self.points
    }
}

impl UiSource for UiLayer {
    fn build(&mut self, tick: u64) {
        UiLayer::draw(self, tick);
    }

    fn upload_bytes(&self) -> (&[u8], bool) {
        (&self.buffer, false)
    }

    fn sample(&self, x: usize, y: usize) -> [u8; 4] {
        let p = (y * self.width + x) * 4;
        [
            self.buffer[p],
            self.buffer[p + 1],
            self.buffer[p + 2],
            self.buffer[p + 3],
        ]
    }

    fn samples(&self) -> [(&'static str, (usize, usize)); 4] {
        self.layout.samples(self.width, self.height)
    }
}
