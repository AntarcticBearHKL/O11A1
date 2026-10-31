//! NVIDIA hardware optical flow: the mode-3 alternative to block matching.
//!
//! Turing and later NVIDIA GPUs carry a dedicated optical-flow engine (OFA).
//! The driver exposes it through `nvofapi64.dll`; this module loads that runtime
//! *dynamically*, creates a D3D11 device and a persistent OF session, feeds it
//! the two source frames of a pair and reads back a coarse flow field. The
//! interpolation itself (warp + blend) runs on the CPU right here, next to the
//! flow readback, so no HLSL is involved.
//!
//! # Provenance of the ABI below
//!
//! The structs, enum values and entry points are transcribed from the NVIDIA
//! Optical-Flow SDK 5.0.7 headers, as mirrored by `mbucchia/Optical-Flow-SDK`:
//! `NvOFInterface/nvOpticalFlowCommon.h` and `NvOFInterface/nvOpticalFlowD3D11.h`.
//! Only what this file calls is transcribed, and the NVIDIA headers themselves
//! are **not** vendored into the repository. Verified against the runtime that
//! ships with the display driver on the development machine:
//! `C:\Windows\System32\nvofapi64.dll` 32.0.16.1088, API version `0x50`.
//!
//! Deviation from a literal reading of the docs: the SDK exposes exactly two
//! exported symbols, `NvOFGetMaxSupportedApiVersion` and
//! `NvOFAPICreateInstanceD3D11`. Everything else (`nvOFInit`, `nvOFExecute`,
//! `nvOFDestroy`, ...) arrives through the `NV_OF_D3D11_API_FUNCTION_LIST` that
//! `NvOFAPICreateInstanceD3D11` fills in, in header order. That is what is
//! resolved below.
//!
//! D3D11 and DXGI are loaded the same way (`d3d11.dll`, `dxgi.dll` +
//! `GetProcAddress`) and their two interfaces are called by vtable index, taken
//! from the Windows SDK (`um/d3d11.h`, `shared/dxgi.h`): the `windows-sys`
//! version pinned by this crate carries no `Direct3D11`/`Dxgi` feature, and
//! adding one is not possible with that release, so the three calls we need
//! (`CreateTexture2D`, `UpdateSubresource`, `CopyResource`/`Map`/`Unmap`) and
//! adapter enumeration are declared here instead of pulling in a second
//! graphics binding.
//!
//! # The pipeline, and why it is one pair late
//!
//! Per pair `(A, B)` at 1080p the spike measured flow 0.84 ms and a GPU warp
//! 0.11 ms, but 5.45 ms of frame upload and (for a full-frame output) 3.15 ms
//! of readback: the frame transport dominates. Warping on the CPU removes the
//! full-frame readback; only the grid-4 flow field (~1/16 of the frame, 0.5 MB
//! at 1080p) crosses the bus back.
//!
//! The D3D11 binding has **no fence**, so reading the flow texture in the same
//! pair that submitted it returns zeros or the previous pair's field. The
//! verified fix is to consume it one pair later: submit pair *i*, read its flow
//! at the start of interval *i+1*. That costs one pair of latency, which the
//! pacer pays by drawing every slot of an interval from the previous pair (see
//! `decode_loop`); `submit` copies the pair into the engine, `try_flow` reads
//! the field of the pair submitted one interval ago.
//!
//! Consecutive pairs share a frame (`b` of pair *i* becomes `a` of pair *i+1*),
//! and `submit` relies on that: the shared frame is already on the GPU, so only
//! the new one is uploaded and the two textures swap roles. That halves the
//! upload, which is half of the pair's total cost at 1080p (measured 1.6 ms of
//! 4.9 ms). [`Ofa::reset`] tells the engine the chain broke (a skipped pair, a
//! seek, a mode change) and the next submit uploads both frames again.
//!
//! # Settings
//!
//! Grid 4, `NV_OF_PERF_LEVEL_FAST`, `NV_OF_PRED_DIRECTION_FORWARD`, and
//! `disableTemporalHints = TRUE` (the hints help a continuous video stream;
//! here every pair is submitted to the engine independently and the pacer can
//! skip a pair, so the hint chain would be stale). Input is the RGBA frame the
//! decoder produces, in whichever 8-bit packed format the engine lists
//! (`R8G8B8A8_UNORM` when offered, else `B8G8R8A8_UNORM` — the channel order is
//! irrelevant to luma-dominated motion estimation and swapping costs a
//! full-frame pass). Output is `R16G16_SINT` read as raw `i16` pairs in S10.5
//! fixed point (5 fractional bits), which is exactly `NV_OF_FLOW_VECTOR`.
//!
//! # Failure policy
//!
//! Every failure path is terminal for the session and cheap: the reason is
//! recorded once in `off`, the D3D11/OF objects are released, and every later
//! call returns `false`/`None` after one branch. Nothing here panics, and no
//! call blocks for more than a Map of a 0.5 MB staging texture.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// `NV_OF_API_VERSION` from the header: `(major << 4) | minor` = 5.0.
const NV_OF_API_VERSION: u32 = 0x50;

// ---- NvOF enum values used (nvOpticalFlowCommon.h) -------------------------
const NV_OF_SUCCESS: i32 = 0;
const NV_OF_TRUE: i32 = 1;
const NV_OF_MODE_OPTICALFLOW: i32 = 1;
const NV_OF_PERF_LEVEL_FAST: i32 = 20;
const NV_OF_OUTPUT_VECTOR_GRID_SIZE_4: i32 = 4;
const NV_OF_PRED_DIRECTION_FORWARD: i32 = 0;
const NV_OF_BUFFER_USAGE_INPUT: i32 = 1;
const NV_OF_BUFFER_USAGE_OUTPUT: i32 = 2;

/// `NV_OF_CAPS` members queried for the log line and the size check.
const NV_OF_CAPS_WIDTH_MIN: i32 = 4;
const NV_OF_CAPS_HEIGHT_MIN: i32 = 5;
const NV_OF_CAPS_WIDTH_MAX: i32 = 6;
const NV_OF_CAPS_HEIGHT_MAX: i32 = 7;

/// Vector grid the field is read back on (one vector per 4x4 pixels).
pub const GRID: u32 = 4;

/// Largest field width this module can warp: 1920 / 4, plus the partial cell.
/// The pacer caps mode 3 at 1920x1080, so 512 cells is the hard requirement and
/// the stack tables in [`warp`] are sized by it.
const MAX_GRID_W: usize = 512;

/// Worker threads over row bands in the CPU warp, matching `motion.rs`.
const MAX_THREADS: usize = 4;

// ---- DXGI_FORMAT / D3D11 values used (dxgiformat.h, d3d11.h) ---------------
const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 28;
const DXGI_FORMAT_R16G16_SINT: u32 = 38;
const DXGI_FORMAT_B8G8R8A8_UNORM: u32 = 87;
const D3D11_USAGE_DEFAULT: u32 = 0;
const D3D11_USAGE_STAGING: u32 = 3;
const D3D11_BIND_SHADER_RESOURCE: u32 = 0x8;
const D3D11_BIND_UNORDERED_ACCESS: u32 = 0x80;
const D3D11_CPU_ACCESS_READ: u32 = 0x2_0000;
const D3D11_MAP_READ: u32 = 1;
const D3D11_CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
const D3D_DRIVER_TYPE_UNKNOWN: u32 = 0;
const D3D_DRIVER_TYPE_HARDWARE: u32 = 1;
const D3D11_SDK_VERSION: u32 = 7;
/// 11.0, 10.1, 10.0: an OF engine needs 11.0 in practice, the rest only make the
/// failure explicit instead of a device-creation error.
const FEATURE_LEVELS: [u32; 3] = [0xb000, 0xa100, 0xa000];
const VENDOR_NVIDIA: u32 = 0x10DE;

// ---- the SDK structs, in header order --------------------------------------

type Com = *mut c_void;

/// `NV_OF_INIT_PARAMS`. Field types are the C enums widened to `i32`; the
/// layout is what the driver reads, so the order and the `usize`-sized pointer
/// matter, and a unit test pins the size at 64 bytes.
#[repr(C)]
struct InitParams {
    width: u32,
    height: u32,
    out_grid_size: i32,
    hint_grid_size: i32,
    mode: i32,
    perf_level: i32,
    enable_external_hints: i32,
    enable_output_cost: i32,
    priv_data: Com,
    disparity_range: i32,
    enable_roi: i32,
    pred_direction: i32,
    enable_global_flow: i32,
    input_buffer_format: i32,
}

/// `NV_OF_EXECUTE_INPUT_PARAMS` (56 bytes).
#[repr(C)]
struct ExecuteInput {
    input_frame: Com,
    reference_frame: Com,
    external_hints: Com,
    disable_temporal_hints: i32,
    padding: u32,
    priv_data: Com,
    padding2: u32,
    num_rois: u32,
    roi_data: *const Roi,
}

/// `NV_OF_ROI_RECT`; only the pointer type is needed, never a value.
#[repr(C)]
struct Roi {
    start_x: u32,
    start_y: u32,
    width: u32,
    height: u32,
}

/// `NV_OF_EXECUTE_OUTPUT_PARAMS` (48 bytes).
#[repr(C)]
struct ExecuteOutput {
    output_buffer: Com,
    output_cost_buffer: Com,
    priv_data: Com,
    bwd_output_buffer: Com,
    bwd_output_cost_buffer: Com,
    global_flow_buffer: Com,
}

// `NV_OF_D3D11_API_FUNCTION_LIST` (10 pointers, header order).
type CreateFn = unsafe extern "system" fn(Com, Com, *mut Com) -> i32;
type InitFn = unsafe extern "system" fn(Com, *const InitParams) -> i32;
type FormatCountFn = unsafe extern "system" fn(Com, i32, i32, *mut u32) -> i32;
type FormatFn = unsafe extern "system" fn(Com, i32, i32, *mut i32) -> i32;
type RegisterFn = unsafe extern "system" fn(Com, Com, *mut Com) -> i32;
type UnregisterFn = unsafe extern "system" fn(Com) -> i32;
type ExecuteFn = unsafe extern "system" fn(Com, *const ExecuteInput, *mut ExecuteOutput) -> i32;
type DestroyFn = unsafe extern "system" fn(Com) -> i32;
type LastErrorFn = unsafe extern "system" fn(Com, *mut i8, *mut u32) -> i32;
type CapsFn = unsafe extern "system" fn(Com, i32, *mut u32, *mut u32) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Api {
    create: CreateFn,
    init: InitFn,
    surface_format_count: FormatCountFn,
    surface_format: FormatFn,
    register: RegisterFn,
    unregister: UnregisterFn,
    execute: ExecuteFn,
    destroy: DestroyFn,
    last_error: LastErrorFn,
    caps: CapsFn,
}

// ---- the two exported entry points ----------------------------------------

type GetMaxApiVersionFn = unsafe extern "system" fn(*mut u32) -> i32;
type CreateInstanceFn = unsafe extern "system" fn(u32, *mut Api) -> i32;

// ---- D3D11 / DXGI declarations used here -----------------------------------

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `IID_IDXGIFactory1`, from `dxgi.h`.
const IID_IDXGIFACTORY1: Guid = Guid {
    data1: 0x770a_ae78,
    data2: 0xf26f,
    data3: 0x4dba,
    data4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
};

/// `DXGI_ADAPTER_DESC` (304 bytes; the `LUID` is inlined as two `u32`s).
#[repr(C)]
struct AdapterDesc {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    subsys_id: u32,
    revision: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    luid_low: u32,
    luid_high: i32,
}

/// `D3D11_TEXTURE2D_DESC` (44 bytes; `DXGI_SAMPLE_DESC` inlined).
#[repr(C)]
struct TexDesc {
    width: u32,
    height: u32,
    mip_levels: u32,
    array_size: u32,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    usage: u32,
    bind: u32,
    cpu_access: u32,
    misc: u32,
}

/// `D3D11_MAPPED_SUBRESOURCE`.
#[repr(C)]
struct Mapped {
    data: Com,
    row_pitch: u32,
    depth_pitch: u32,
}

type CreateFactory1Fn = unsafe extern "system" fn(*const Guid, *mut Com) -> i32;
type EnumAdapters1Fn = unsafe extern "system" fn(Com, u32, *mut Com) -> i32;
type AdapterDescFn = unsafe extern "system" fn(Com, *mut AdapterDesc) -> i32;
type CreateDeviceFn = unsafe extern "system" fn(
    Com,
    u32,
    usize,
    u32,
    *const u32,
    u32,
    u32,
    *mut Com,
    *mut u32,
    *mut Com,
) -> i32;
type CreateTexture2DFn = unsafe extern "system" fn(Com, *const TexDesc, *const c_void, *mut Com) -> i32;
type UpdateSubresourceFn =
    unsafe extern "system" fn(Com, Com, u32, *const c_void, *const c_void, u32, u32);
type CopyResourceFn = unsafe extern "system" fn(Com, Com, Com);
type MapFn = unsafe extern "system" fn(Com, Com, u32, u32, u32, *mut Mapped) -> i32;
type UnmapFn = unsafe extern "system" fn(Com, Com, u32);

/// Call the `index`-th method of a COM interface.
///
/// Every index used in this file is the method's position in the interface as
/// declared by the Windows SDK header (`IUnknown` is always 0..=2, then the
/// base interface's methods, then the derived one's), which is stable ABI. The
/// ones this module needs are named in the call sites.
///
/// # Safety
/// `obj` must be a live COM pointer of the interface whose vtable layout the
/// index refers to, and `F` must be that method's exact signature.
unsafe fn vtbl<F: Copy>(obj: Com, index: usize) -> F {
    let table = *(obj as *const *const usize);
    std::mem::transmute_copy(&*table.add(index))
}

/// `IUnknown::Release`.
///
/// # Safety
/// `obj` must be a live COM pointer or null.
unsafe fn release(obj: Com) {
    if obj.is_null() {
        return;
    }
    let f: unsafe extern "system" fn(Com) -> u32 = vtbl(obj, 2);
    f(obj);
}

unsafe fn proc(module: Com, name: &[u8]) -> Result<*const c_void, &'static str> {
    match GetProcAddress(module, name.as_ptr()) {
        Some(function) => Ok(function as usize as *const c_void),
        None => Err("entry_missing"),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Resolve one exported function of a system DLL, once per process.
///
/// The module is loaded and deliberately **not** unloaded: the function
/// pointers must outlive every caller, the driver expects its runtime to stay
/// resident for the process, and the `windows-sys` version pinned here does not
/// expose `FreeLibrary` at all.
fn system_entry(module_name: &str, entry: &[u8]) -> Result<usize, &'static str> {
    // SAFETY: `module_name` is a NUL-terminated UTF-16 string and `entry` one of
    // the ASCII names in this file; the handle stays loaded for the process.
    unsafe {
        let handle = LoadLibraryW(wide(module_name).as_ptr());
        if handle.is_null() {
            return Err(if module_name == "nvofapi64.dll" {
                "dll_missing"
            } else {
                "system_dll_missing"
            });
        }
        proc(handle, entry).map(|function| function as usize)
    }
}

fn function_at<T: Copy>(address: usize) -> T {
    // The caller only ever passes the address of a symbol with exactly this
    // signature, taken from the same DLL the header declares it in.
    unsafe { std::mem::transmute_copy::<usize, T>(&address) }
}

/// The NVIDIA optical-flow API, filled in once per process by
/// `NvOFAPICreateInstanceD3D11`. `Api` is `Copy`, so callers get their own copy
/// and no lifetime travels with it.
fn api() -> Result<Api, &'static str> {
    static API: OnceLock<Result<Api, &'static str>> = OnceLock::new();
    *API.get_or_init(load_api)
}

fn load_api() -> Result<Api, &'static str> {
    // QA hook: behave exactly like a machine without the runtime, so the
    // fallback path (one log line, one toast, blend) can be verified live:
    // `BILI_OFA_OFF=1 BILI_INTERP_TEST=3`.
    if std::env::var("BILI_OFA_OFF").as_deref() == Ok("1") {
        return Err("dll_missing");
    }
    let module_address = system_entry("nvofapi64.dll", b"NvOFGetMaxSupportedApiVersion\0")?;
    let max_version: GetMaxApiVersionFn = function_at(module_address);
    // SAFETY: the symbol is `NV_OF_STATUS NVOFAPI NvOFGetMaxSupportedApiVersion(uint32_t*)`.
    let mut version = 0u32;
    if unsafe { max_version(&mut version) } != NV_OF_SUCCESS || version < NV_OF_API_VERSION {
        return Err("api_unsupported");
    }
    let create: CreateInstanceFn = function_at(system_entry(
        "nvofapi64.dll",
        b"NvOFAPICreateInstanceD3D11\0",
    )?);
    let mut api = std::mem::MaybeUninit::<Api>::uninit();
    // SAFETY: the entry point fills the whole function list, in header order.
    if unsafe { create(NV_OF_API_VERSION, api.as_mut_ptr()) } != NV_OF_SUCCESS {
        return Err("api_create_failed");
    }
    crate::debug_log(&format!(
        "ofa api dll=nvofapi64.dll max_version=0x{version:x} header_version=0x{NV_OF_API_VERSION:x}"
    ));
    // SAFETY: `create` reported success, so every pointer was written.
    Ok(unsafe { api.assume_init() })
}

/// One initialized OF session, with its textures and registered buffers.
struct Session {
    handle: Com,
    w: u32,
    h: u32,
    /// Flow field size, one vector per [`GRID`]x[`GRID`] pixels.
    gw: u32,
    gh: u32,
    input_format: u32,
    /// The two input textures, as roles rather than identities: `tex_cur` holds
    /// the frame the last submitted pair used as its reference, `tex_next`
    /// receives the new frame. They swap after every submit, so a continuous
    /// run of pairs uploads exactly one frame per pair instead of two — the
    /// upload is the dominant cost of the whole path (measured 1.5 ms of a
    /// 1080p pair per frame, and the pair shares one frame with its neighbour).
    tex_cur: Com,
    tex_next: Com,
    tex_flow: Com,
    tex_read: Com,
    buf_cur: Com,
    buf_next: Com,
    buf_flow: Com,
    /// f200: shader-resource views of the three textures above, created here
    /// because this file is the one that owns the raw handles. The video sink's
    /// warp pipeline takes ownership of them (see `Ofa::views`).
    srv_cur: Com,
    srv_next: Com,
    srv_flow: Com,
    /// Readback target: `gw * gh` vectors of two `i16`.
    flow: Vec<i16>,
}

impl Session {
    fn empty() -> Self {
        Self {
            handle: std::ptr::null_mut(),
            w: 0,
            h: 0,
            gw: 0,
            gh: 0,
            input_format: 0,
            tex_cur: std::ptr::null_mut(),
            tex_next: std::ptr::null_mut(),
            tex_flow: std::ptr::null_mut(),
            tex_read: std::ptr::null_mut(),
            buf_cur: std::ptr::null_mut(),
            buf_next: std::ptr::null_mut(),
            buf_flow: std::ptr::null_mut(),
            srv_cur: std::ptr::null_mut(),
            srv_next: std::ptr::null_mut(),
            srv_flow: std::ptr::null_mut(),
            flow: Vec::new(),
        }
    }
}

/// The engine for one decode loop.
pub struct Ofa {
    /// D3D11 device and immediate context, created on the first `ensure`.
    device: Com,
    context: Com,
    /// f200: the device/context were handed over by their owner (the video
    /// sink) and must not be released here.
    borrowed: bool,
    adapter: String,
    session: Option<Session>,
    /// A pair has been submitted whose flow has not been read yet.
    pending: bool,
    /// The pair that produced `pending` is followed immediately by the pair the
    /// next `submit` will build (the caller's pair overlap by one frame). Only
    /// then can the shared frame stay on the GPU; `reset` and any failed
    /// submit clear it.
    continuous: bool,
    /// Sticky reason this engine is off; the first failure wins.
    off: Option<&'static str>,
    /// Upload + submit cost of the last pair, ms.
    last_ms: f32,
    /// Readback cost of the last pair's field, ms.
    flow_ms: f32,
    /// Pairs submitted (QA rate).
    submits: u64,
}

/// The engine is built on the attach thread (so a slow driver cannot block the
/// pacer) and then moved to the pacer thread through a channel. Only one thread
/// ever calls into it: the channel hand-off happens before the first use, and
/// D3D11's device/immediate context are free-threaded COM objects, so moving
/// them between threads is sound. The raw COM pointers are why `Send` has to be
/// asserted rather than derived.
unsafe impl Send for Ofa {}

/// The pacer's handle to one attach attempt: the result channel plus the
/// cancellation flag the worker checks before it touches D3D11.
pub struct AttachHandle {
    rx: std::sync::mpsc::Receiver<Result<Ofa, &'static str>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AttachHandle {
    /// The worker's verdict, `None` while it is still building (or sleeping).
    pub fn try_recv(
        &self,
    ) -> Result<Result<Ofa, &'static str>, std::sync::mpsc::TryRecvError> {
        self.rx.try_recv()
    }

    /// Tell the worker to stop before its next step (its result is ignored).
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Ofa {
    /// Cheap constructor: allocates nothing and touches no DLL.
    ///
    /// This used to resolve the runtime here, which loaded `nvofapi64.dll` on
    /// the decode thread **before the first frame** — one slow driver init and
    /// every mode (not just OFA) sat behind it. The runtime and the device are
    /// resolved by [`Self::ensure`], which the pacer only calls after a frame is
    /// on screen.
    pub fn new() -> Self {
        Self {
            device: std::ptr::null_mut(),
            context: std::ptr::null_mut(),
            borrowed: false,
            adapter: String::new(),
            session: None,
            pending: false,
            continuous: false,
            off: None,
            last_ms: 0.0,
            flow_ms: 0.0,
            submits: 0,
        }
    }

    /// Build a ready-to-use engine for `w`x`h` on a helper thread.
    ///
    /// The handle yields exactly one result. The pacer polls it with
    /// `try_recv` while it keeps presenting blended frames, so a DLL load, a
    /// device creation or a driver that never answers cannot stall the picture:
    /// after its budget the caller cancels the handle (so the worker does not
    /// even start D3D11) and drops it, and the half-built engine is dropped on
    /// that thread.
    ///
    /// `BILI_OFA_ATTACH_MS=<n>` sleeps `n` ms before the work starts, which is
    /// the QA hook for the budget path (`ofa disabled reason=attach_timeout`).
    pub fn spawn_attach(w: u32, h: u32) -> AttachHandle {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let spawned = std::thread::Builder::new()
            .name("ofa-attach".into())
            .spawn(move || {
                crate::player::note_thread_role("ofa-attach");
                let cancelled = || worker_cancel.load(std::sync::atomic::Ordering::Relaxed);
                if let Some(ms) = std::env::var("BILI_OFA_ATTACH_MS")
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|ms| *ms > 0)
                {
                    std::thread::sleep(std::time::Duration::from_millis(ms.min(60_000)));
                }
                // The pacer may have given up while this thread was asleep or
                // waiting for the driver. Do not start a D3D11 device for a
                // session nobody will use: that init is expensive enough to
                // cost the running picture a frame.
                if cancelled() {
                    let _ = tx.send(Err("cancelled"));
                    return;
                }
                let mut engine = Ofa::new();
                let result = match api() {
                    Err(reason) => Err(reason),
                    Ok(_) => {
                        if cancelled() {
                            Err("cancelled")
                        } else if engine.ensure(w, h) {
                            Ok(engine)
                        } else {
                            Err(engine.off.unwrap_or("session_failed"))
                        }
                    }
                };
                // A dropped receiver (the attach budget expired) is not an
                // error: the engine is released with this thread.
                let _ = tx.send(result);
            });
        if let Err(err) = spawned {
            crate::debug_log(&format!("ofa attach spawn failed error={err}"));
        }
        AttachHandle { rx, cancel }
    }

    /// Whether the runtime is importable at all (no device, ~0.2 ms, cached).
    ///
    /// This is the check a UI click can afford: it answers "can mode 3 do
    /// anything on this machine" without creating a device.
    pub fn present() -> bool {
        api().is_ok()
    }

    /// Whether this machine can run the engine end to end (DLL + device +
    /// session), cached for the process. Costs a device creation on the first
    /// call (~50 ms), so tests and offline diagnostics use it; the pacer never
    /// does (it attaches on a worker thread, see [`Self::spawn_attach`]).
    #[cfg(test)]
    pub fn available() -> bool {
        probe().is_ok()
    }

    /// Why [`Self::available`] says no (`"ok"` when it says yes).
    #[cfg(test)]
    pub fn unavailable_reason() -> &'static str {
        probe().err().unwrap_or("ok")
    }

    /// Sticky reason this engine is off, if it is.
    pub fn off_reason(&self) -> Option<&'static str> {
        self.off
    }

    pub fn last_ms(&self) -> f32 {
        self.last_ms
    }

    pub fn flow_ms(&self) -> f32 {
        self.flow_ms
    }

    /// f200: build the engine on a device/context the caller already owns.
    ///
    /// The video sink presents on its own D3D11 device and immediate context, and
    /// the whole point of f200 is that the flow field, the two source frames and
    /// the warped output all stay on that one device: no cross-device shared
    /// texture, no staging round trip, no third device in the process. The
    /// pointers are **borrowed** - they are released by their owner.
    ///
    /// # Safety
    /// `device`/`context` must be a live `ID3D11Device`/`ID3D11DeviceContext`
    /// pair that lives longer than the returned engine, and only one thread may
    /// use the immediate context at a time.
    pub unsafe fn attach(device: *mut c_void, context: *mut c_void) -> Self {
        let mut engine = Self::new();
        engine.device = device;
        engine.context = context;
        engine.borrowed = true;
        engine
    }

    /// The three shader-resource views of the live session, **ownership
    /// transferred** (the caller releases them; this engine forgets them).
    pub fn take_views(&mut self) -> Option<(*mut c_void, *mut c_void, *mut c_void)> {
        let session = self.session.as_mut()?;
        let views = (session.srv_cur, session.srv_next, session.srv_flow);
        if views.0.is_null() || views.1.is_null() || views.2.is_null() {
            return None;
        }
        session.srv_cur = std::ptr::null_mut();
        session.srv_next = std::ptr::null_mut();
        session.srv_flow = std::ptr::null_mut();
        Some(views)
    }

    /// `(grid_w, grid_h)` of the flow field.
    pub fn grid(&self) -> Option<(u32, u32)> {
        self.session.as_ref().map(|session| (session.gw, session.gh))
    }

    /// Whether the session's input textures hold RGBA (so a BGRA output needs a
    /// red/blue swap).
    pub fn input_is_rgba(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.input_format == DXGI_FORMAT_R8G8B8A8_UNORM)
    }

    /// Drop the pair in flight without reading it (a seek, a mode change or a
    /// frame-size change makes the field meaningless), and forget which frame
    /// the GPU currently holds.
    pub fn reset(&mut self) {
        self.pending = false;
        self.continuous = false;
        self.last_ms = 0.0;
        self.flow_ms = 0.0;
    }

    /// Prepare a session for `w`x`h`, creating the device and the OF session on
    /// first use and re-creating the session when the frame size changes.
    pub fn ensure(&mut self, w: u32, h: u32) -> bool {
        if self.off.is_some() {
            return false;
        }
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.w == w && session.h == h)
        {
            return true;
        }
        // Resolve the runtime before touching D3D11: a machine without it should
        // fail on one branch, not on a device it will immediately drop.
        let api = match api() {
            Ok(api) => api,
            Err(reason) => {
                self.fail(reason);
                return false;
            }
        };
        if let Err(reason) = self.init_device() {
            self.fail(reason);
            return false;
        }
        self.close_session();
        // SAFETY: the device and context are live (checked above), `api` belongs
        // to the runtime that produced them, and session creation only touches
        // its own freshly created engine handle.
        match unsafe { open_session(&api, self.device, self.context, w, h) } {
            Ok(session) => {
                crate::debug_log(&format!(
                    "ofa session frame={}x{} grid={} field={}x{} in_format={} out_format={} adapter=\"{}\"",
                    w,
                    h,
                    GRID,
                    session.gw,
                    session.gh,
                    session.input_format,
                    DXGI_FORMAT_R16G16_SINT,
                    self.adapter
                ));
                self.session = Some(session);
                self.pending = false;
                self.continuous = false;
                true
            }
            Err(reason) => {
                self.fail(reason);
                false
            }
        }
    }

    /// Upload one pair and kick off the OF engine. The field is **not** read
    /// here: see the module docs, the D3D11 binding has no fence.
    ///
    /// Consecutive pairs from the pacer share a frame (`b` of pair *i* is `a` of
    /// pair *i+1*); after the first pair that shared frame is already on the GPU
    /// and only the new one is uploaded. [`Self::reset`] — called whenever a
    /// pair is skipped — makes the next submit upload both frames again.
    pub fn submit(&mut self, a: &[u8], b: &[u8], w: u32, h: u32) -> bool {
        if self.off.is_some() {
            return false;
        }
        let api = match api() {
            Ok(api) => api,
            Err(reason) => {
                self.fail(reason);
                return false;
            }
        };
        let context = self.context;
        let started = Instant::now();
        let continuous = self.continuous;
        let session = match self.session.as_mut() {
            Some(session) if session.w == w && session.h == h => session,
            _ => return false,
        };
        let bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if a.len() < bytes || b.len() < bytes {
            return false;
        }
        // SAFETY: textures and registered buffers belong to this live session;
        // both slices hold a whole frame of the size the textures were made for.
        let result = unsafe { submit_pair(&api, context, session, a, b, continuous) };
        if let Err(reason) = result {
            self.continuous = false;
            self.fail(reason);
            return false;
        }
        self.pending = true;
        self.continuous = true;
        self.submits = self.submits.saturating_add(1);
        self.last_ms = started.elapsed().as_secs_f32() * 1000.0;
        true
    }

    /// f209: run the engine on a pair that is **already on the GPU**.
    ///
    /// `a` and `b` are `ID3D11Texture2D` pointers (as raw COM) holding the two
    /// pictures in this session's own input format and size - the video sink
    /// converted the decoder's NV12 surfaces into them. Nothing is uploaded: the
    /// two textures are copied into the session's input textures with
    /// `CopyResource`, which is a GPU copy, so the zero-upload contract of the
    /// NV12 path survives into the optical-flow engine.
    ///
    /// The pair is always taken as non-continuous: the caller converts both
    /// frames anyway (they come from two different decoder surfaces), so
    /// skipping one copy would only risk a stale frame.
    ///
    /// # Safety
    /// `a`/`b` must be live `ID3D11Texture2D` objects created on this session's
    /// device, with this session's input format and size.
    pub unsafe fn submit_textures(&mut self, a: *mut c_void, b: *mut c_void) -> bool {
        if self.off.is_some() || a.is_null() || b.is_null() {
            return false;
        }
        let api = match api() {
            Ok(api) => api,
            Err(reason) => {
                self.fail(reason);
                return false;
            }
        };
        let context = self.context;
        let started = Instant::now();
        let session = match self.session.as_mut() {
            Some(session) => session,
            None => return false,
        };
        // ID3D11DeviceContext::CopyResource is index 47 (IUnknown 0..=2,
        // ID3D11DeviceChild 3..=6, then the context methods).
        let copy: CopyResourceFn = vtbl(context, 47);
        copy(context, session.tex_cur, a);
        copy(context, session.tex_next, b);
        let input = ExecuteInput {
            input_frame: session.buf_cur,
            reference_frame: session.buf_next,
            external_hints: std::ptr::null_mut(),
            disable_temporal_hints: NV_OF_TRUE,
            padding: 0,
            priv_data: std::ptr::null_mut(),
            padding2: 0,
            num_rois: 0,
            roi_data: std::ptr::null(),
        };
        let mut output = ExecuteOutput {
            output_buffer: session.buf_flow,
            output_cost_buffer: std::ptr::null_mut(),
            priv_data: std::ptr::null_mut(),
            bwd_output_buffer: std::ptr::null_mut(),
            bwd_output_cost_buffer: std::ptr::null_mut(),
            global_flow_buffer: std::ptr::null_mut(),
        };
        let status = (api.execute)(session.handle, &input, &mut output);
        if status != NV_OF_SUCCESS {
            crate::debug_log(&format!(
                "ofa execute status={status} err={}",
                error_text(&api, session.handle)
            ));
            self.fail("execute_failed");
            return false;
        }
        std::mem::swap(&mut session.tex_cur, &mut session.tex_next);
        std::mem::swap(&mut session.buf_cur, &mut session.buf_next);
        self.pending = true;
        self.continuous = false;
        self.submits = self.submits.saturating_add(1);
        self.last_ms = started.elapsed().as_secs_f32() * 1000.0;
        true
    }

    /// The flow field of the pair submitted one interval ago:
    /// `(flow, grid_w, grid_h)` with two `i16` per cell (x, y in S10.5).
    ///
    /// `None` when nothing is pending (the previous interval did not submit) or
    /// when the engine failed; the caller then blends that interval.
    pub fn try_flow(&mut self) -> Option<(Vec<i16>, u32, u32)> {        if self.off.is_some() || !self.pending {
            return None;
        }
        self.pending = false;
        let context = self.context;
        let started = Instant::now();
        let session = self.session.as_mut()?;
        // SAFETY: the readback texture and the field buffer belong to this live
        // session (same size), and the context is the one the device created.
        let result = unsafe { read_flow(context, session) };
        if let Err(reason) = result {
            self.fail(reason);
            return None;
        }
        self.flow_ms = started.elapsed().as_secs_f32() * 1000.0;
        let (gw, gh) = (session.gw, session.gh);
        Some((session.flow.clone(), gw, gh))
    }

    fn init_device(&mut self) -> Result<(), &'static str> {
        if !self.device.is_null() {
            return Ok(());
        }
        // SAFETY: called once per engine; the device is released in `Drop`.
        let (device, context, adapter) = unsafe { create_device() }?;
        self.device = device;
        self.context = context;
        self.adapter = adapter;
        Ok(())
    }

    /// Release everything and remember why. The first reason is the one kept.
    fn fail(&mut self, reason: &'static str) {
        if self.off.is_none() {
            self.off = Some(reason);
            crate::debug_log(&format!("ofa off reason={reason}"));
        }
        self.pending = false;
        self.close_session();
    }

    fn close_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            if let Ok(api) = api() {
                // SAFETY: every handle in `session` was created through this API.
                unsafe { close_session(&api, &mut session) };
            }
        }
        self.pending = false;
    }
}

impl Drop for Ofa {
    fn drop(&mut self) {
        self.close_session();
        // SAFETY: each interface pointer is released once, in reverse creation
        // order. The runtime stays loaded for the process (see `system_entry`),
        // so no function pointer can outlive its module.
        if !self.borrowed {
            // SAFETY: an owned pair was created by `create_device` and released
            // once; a borrowed one belongs to the caller (the video sink).
            unsafe {
                release(self.context);
                release(self.device);
            }
        }
        self.context = std::ptr::null_mut();
        self.device = std::ptr::null_mut();
    }
}

impl Default for Ofa {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide capability probe (tests/diagnostics only; the pacer attaches on
/// a worker thread).
#[cfg(test)]
fn probe() -> Result<(), &'static str> {
    static PROBE: OnceLock<Result<(), &'static str>> = OnceLock::new();
    *PROBE.get_or_init(|| {
        let mut engine = Ofa::new();
        if let Some(reason) = engine.off {
            return Err(reason);
        }
        // A real, small session is the only honest check: it validates the
        // adapter, the D3D11 device and the driver's init parameters in one go.
        if engine.ensure(256, 144) {
            Ok(())
        } else {
            Err(engine.off.unwrap_or("session_failed"))
        }
    })
}

// ---- D3D11 / DXGI ----------------------------------------------------------

unsafe fn create_device() -> Result<(Com, Com, String), &'static str> {
    let create_factory: CreateFactory1Fn = function_at(system_entry("dxgi.dll", b"CreateDXGIFactory1\0")?);
    let mut factory: Com = std::ptr::null_mut();
    if create_factory(&IID_IDXGIFACTORY1, &mut factory) < 0 || factory.is_null() {
        return Err("dxgi_factory_failed");
    }
    // Prefer the NVIDIA adapter: on a machine with an integrated and a discrete
    // GPU the default adapter can be the one without an OF engine.
    let enum_adapters: EnumAdapters1Fn = vtbl(factory, 12);
    let mut chosen: Com = std::ptr::null_mut();
    let mut name = String::new();
    for index in 0..8u32 {
        let mut adapter: Com = std::ptr::null_mut();
        if enum_adapters(factory, index, &mut adapter) < 0 || adapter.is_null() {
            break;
        }
        let get_desc: AdapterDescFn = vtbl(adapter, 8);
        let mut desc: AdapterDesc = std::mem::zeroed();
        if get_desc(adapter, &mut desc) >= 0 && desc.vendor_id == VENDOR_NVIDIA {
            name = utf16_name(&desc.description);
            chosen = adapter;
            break;
        }
        release(adapter);
    }
    release(factory);

    let create: CreateDeviceFn = match system_entry("d3d11.dll", b"D3D11CreateDevice\0") {
        Ok(address) => function_at(address),
        Err(reason) => {
            release(chosen);
            return Err(reason);
        }
    };
    let mut device: Com = std::ptr::null_mut();
    let mut context: Com = std::ptr::null_mut();
    let mut level = 0u32;
    // An explicit adapter needs D3D_DRIVER_TYPE_UNKNOWN; without one, ask for
    // the default hardware adapter.
    let (adapter, driver) = if chosen.is_null() {
        (std::ptr::null_mut(), D3D_DRIVER_TYPE_HARDWARE)
    } else {
        (chosen, D3D_DRIVER_TYPE_UNKNOWN)
    };
    let hr = create(
        adapter,
        driver,
        0,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        FEATURE_LEVELS.as_ptr(),
        FEATURE_LEVELS.len() as u32,
        D3D11_SDK_VERSION,
        &mut device,
        &mut level,
        &mut context,
    );
    release(chosen);
    if hr < 0 || device.is_null() || context.is_null() {
        release(context);
        release(device);
        return Err("device_failed");
    }
    Ok((device, context, name))
}

fn utf16_name(raw: &[u16]) -> String {
    let end = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

unsafe fn texture(
    device: Com,
    width: u32,
    height: u32,
    format: u32,
    usage: u32,
    bind: u32,
    cpu_access: u32,
) -> Result<Com, &'static str> {
    let desc = TexDesc {
        width,
        height,
        mip_levels: 1,
        array_size: 1,
        format,
        sample_count: 1,
        sample_quality: 0,
        usage,
        bind,
        cpu_access,
        misc: 0,
    };
    let create: CreateTexture2DFn = vtbl(device, 5);
    let mut tex: Com = std::ptr::null_mut();
    let hr = create(device, &desc, std::ptr::null(), &mut tex);
    if hr < 0 || tex.is_null() {
        crate::debug_log(&format!(
            "ofa create_texture failed size={width}x{height} format={format} usage={usage} bind={bind:#x} cpu={cpu_access} hr={hr:#x}"
        ));
        return Err("texture_failed");
    }
    Ok(tex)
}

/// First value of one `NV_OF_CAPS` query.
unsafe fn caps_first(api: &Api, handle: Com, cap: i32) -> Option<u32> {
    let mut size = 0u32;
    if (api.caps)(handle, cap, std::ptr::null_mut(), &mut size) != NV_OF_SUCCESS || size == 0 {
        return None;
    }
    let mut values = vec![0u32; size.min(16) as usize];
    if (api.caps)(handle, cap, values.as_mut_ptr(), &mut size) != NV_OF_SUCCESS {
        return None;
    }
    values.first().copied()
}

/// `nvOFGetLastError`: the driver's description of the last failure.
unsafe fn error_text(api: &Api, handle: Com) -> String {
    let mut buffer = [0i8; 128];
    let mut size = buffer.len() as u32;
    if (api.last_error)(handle, buffer.as_mut_ptr(), &mut size) != NV_OF_SUCCESS {
        return String::new();
    }
    let bytes: Vec<u8> = buffer
        .iter()
        .take((size as usize).min(buffer.len()))
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// DXGI formats the driver lists for one buffer usage, or an error.
unsafe fn surface_formats(api: &Api, handle: Com, usage: i32) -> Result<Vec<u32>, &'static str> {
    let mut count = 0u32;
    if (api.surface_format_count)(handle, usage, NV_OF_MODE_OPTICALFLOW, &mut count) != NV_OF_SUCCESS
        || count == 0
        || count > 32
    {
        return Err("format_count_failed");
    }
    let mut formats = vec![0i32; count as usize];
    if (api.surface_format)(handle, usage, NV_OF_MODE_OPTICALFLOW, formats.as_mut_ptr())
        != NV_OF_SUCCESS
    {
        return Err("format_list_failed");
    }
    Ok(formats.into_iter().map(|format| format as u32).collect())
}

unsafe fn open_session(
    api: &Api,
    device: Com,
    context: Com,
    w: u32,
    h: u32,
) -> Result<Session, &'static str> {
    let mut session = Session::empty();
    session.w = w;
    session.h = h;
    session.gw = (w + GRID - 1) / GRID;
    session.gh = (h + GRID - 1) / GRID;
    if session.gw as usize > MAX_GRID_W {
        return Err("frame_too_wide");
    }
    match fill_session(api, device, context, &mut session) {
        Ok(()) => Ok(session),
        Err(reason) => {
            close_session(api, &mut session);
            Err(reason)
        }
    }
}

unsafe fn fill_session(
    api: &Api,
    device: Com,
    context: Com,
    session: &mut Session,
) -> Result<(), &'static str> {
    let mut handle: Com = std::ptr::null_mut();
    if (api.create)(device, context, &mut handle) != NV_OF_SUCCESS || handle.is_null() {
        return Err("create_failed");
    }
    session.handle = handle;

    let params = InitParams {
        width: session.w,
        height: session.h,
        out_grid_size: NV_OF_OUTPUT_VECTOR_GRID_SIZE_4,
        // Only read when external hints are enabled; the header asks for a grid
        // at least as coarse as the output grid.
        hint_grid_size: NV_OF_OUTPUT_VECTOR_GRID_SIZE_4,
        mode: NV_OF_MODE_OPTICALFLOW,
        perf_level: NV_OF_PERF_LEVEL_FAST,
        enable_external_hints: 0,
        // No cost buffer: the warp treats a zero vector as "blend here", which
        // is what a low-confidence cell wants anyway, and the extra plane would
        // double the readback for a decision the field already expresses.
        enable_output_cost: 0,
        priv_data: std::ptr::null_mut(),
        disparity_range: 0,
        enable_roi: 0,
        pred_direction: NV_OF_PRED_DIRECTION_FORWARD,
        enable_global_flow: 0,
        input_buffer_format: 0,
    };
    if (api.init)(handle, &params) != NV_OF_SUCCESS {
        crate::debug_log(&format!("ofa init failed err={}", error_text(api, handle)));
        return Err("init_failed");
    }

    // The documented caps: every size this module is asked for must be inside
    // the device's range, before any texture is created.
    let width_min = caps_first(api, handle, NV_OF_CAPS_WIDTH_MIN).unwrap_or(32);
    let height_min = caps_first(api, handle, NV_OF_CAPS_HEIGHT_MIN).unwrap_or(32);
    let width_max = caps_first(api, handle, NV_OF_CAPS_WIDTH_MAX).unwrap_or(8192);
    let height_max = caps_first(api, handle, NV_OF_CAPS_HEIGHT_MAX).unwrap_or(8192);
    if session.w < width_min
        || session.h < height_min
        || session.w > width_max
        || session.h > height_max
    {
        return Err("frame_size_unsupported");
    }

    let inputs = surface_formats(api, handle, NV_OF_BUFFER_USAGE_INPUT)?;
    // The decoder hands over RGBA; take the exactly-matching format when the
    // driver lists it, otherwise the packed BGRA one (same bytes, R and B
    // swapped for the estimator, which only shifts luma weights).
    let input_format = if inputs.contains(&DXGI_FORMAT_R8G8B8A8_UNORM) {
        DXGI_FORMAT_R8G8B8A8_UNORM
    } else if inputs.contains(&DXGI_FORMAT_B8G8R8A8_UNORM) {
        DXGI_FORMAT_B8G8R8A8_UNORM
    } else {
        crate::debug_log(&format!("ofa input formats unsupported list={inputs:?}"));
        return Err("input_format_unsupported");
    };
    let outputs = surface_formats(api, handle, NV_OF_BUFFER_USAGE_OUTPUT)?;
    if !outputs.contains(&DXGI_FORMAT_R16G16_SINT) {
        crate::debug_log(&format!("ofa output formats unsupported list={outputs:?}"));
        return Err("output_format_unsupported");
    }
    session.input_format = input_format;
    session.flow = vec![0i16; session.gw as usize * session.gh as usize * 2];

    session.tex_cur = texture(
        device,
        session.w,
        session.h,
        input_format,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_SHADER_RESOURCE,
        0,
    )?;
    session.tex_next = texture(
        device,
        session.w,
        session.h,
        input_format,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_SHADER_RESOURCE,
        0,
    )?;
    // The engine writes this one: it is an output resource, so it is created
    // UAV-capable (the driver's own view decides how it is touched) and
    // readable, because `CopyResource` from it is the only way back.
    session.tex_flow = texture(
        device,
        session.gw,
        session.gh,
        DXGI_FORMAT_R16G16_SINT,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE,
        0,
    )?;
    session.tex_read = texture(
        device,
        session.gw,
        session.gh,
        DXGI_FORMAT_R16G16_SINT,
        D3D11_USAGE_STAGING,
        0,
        D3D11_CPU_ACCESS_READ,
    )?;

    // f200: the views the GPU warp binds. Created here, through the same raw
    // vtable the textures were created with (ID3D11Device::CreateShaderResourceView
    // is index 7), because that is the only path proven to reach these objects:
    // reconstructing a `windows` interface over the raw pointer and calling
    // through it faults (measured: access violation on the very first call).
    type CreateSrvFn =
        unsafe extern "system" fn(Com, Com, *const c_void, *mut Com) -> i32;
    let create_srv: CreateSrvFn = vtbl(device, 7);
    for (texture, slot) in [
        (session.tex_cur, &mut session.srv_cur),
        (session.tex_next, &mut session.srv_next),
        (session.tex_flow, &mut session.srv_flow),
    ] {
        let mut view: Com = std::ptr::null_mut();
        let hr = create_srv(device, texture, std::ptr::null(), &mut view);
        if hr < 0 || view.is_null() {
            crate::debug_log(&format!(
                "ofa create_srv failed hr={hr:#x} texture={texture:p}"
            ));
            return Err("shader_resource_view_failed");
        }
        *slot = view;
    }

    // Registering an app texture is what makes it usable in `nvOFExecute`.
    // `ID3D11Texture2D` derives from `ID3D11Resource`, so the pointer is passed
    // as-is (single inheritance: same vtable prefix, no QueryInterface needed).
    let register: RegisterFn = api.register;
    if register(handle, session.tex_cur, &mut session.buf_cur) != NV_OF_SUCCESS
        || session.buf_cur.is_null()
    {
        return Err("register_input_failed");
    }
    if register(handle, session.tex_next, &mut session.buf_next) != NV_OF_SUCCESS
        || session.buf_next.is_null()
    {
        return Err("register_input_failed");
    }
    if register(handle, session.tex_flow, &mut session.buf_flow) != NV_OF_SUCCESS
        || session.buf_flow.is_null()
    {
        return Err("register_output_failed");
    }
    crate::debug_log(&format!(
        "ofa caps w={width_min}..{width_max} h={height_min}..{height_max} in_formats={inputs:?} out_formats={outputs:?}"
    ));
    Ok(())
}

unsafe fn close_session(api: &Api, session: &mut Session) {
    let unregister: UnregisterFn = api.unregister;
    for buffer in [session.buf_flow, session.buf_next, session.buf_cur] {
        if !buffer.is_null() {
            unregister(buffer);
        }
    }
    if !session.handle.is_null() {
        (api.destroy)(session.handle);
    }
    for view in [session.srv_flow, session.srv_next, session.srv_cur] {
        release(view);
    }
    for tex in [
        session.tex_read,
        session.tex_flow,
        session.tex_next,
        session.tex_cur,
    ] {
        release(tex);
    }
    *session = Session::empty();
}

/// Upload the pair and run the engine once.
///
/// `continuous` means the caller's previous pair ended on `a` (the pacer always
/// submits overlapping pairs), so `tex_cur` already holds that frame: only the
/// new frame goes up. The roles swap afterwards, which keeps every pair's
/// `cur`/`next` pointing at the two frames the engine has to compare.
unsafe fn submit_pair(
    api: &Api,
    context: Com,
    session: &mut Session,
    a: &[u8],
    b: &[u8],
    continuous: bool,
) -> Result<(), &'static str> {
    let update: UpdateSubresourceFn = vtbl(context, 48);
    let row_pitch = session.w * 4;
    if !continuous {
        update(
            context,
            session.tex_cur,
            0,
            std::ptr::null(),
            a.as_ptr() as *const c_void,
            row_pitch,
            0,
        );
    }
    update(
        context,
        session.tex_next,
        0,
        std::ptr::null(),
        b.as_ptr() as *const c_void,
        row_pitch,
        0,
    );
    let input = ExecuteInput {
        input_frame: session.buf_cur,
        reference_frame: session.buf_next,
        external_hints: std::ptr::null_mut(),
        // Pairs are independent here (the pacer can skip one), so the driver's
        // temporal hint chain must not link them.
        disable_temporal_hints: NV_OF_TRUE,
        padding: 0,
        priv_data: std::ptr::null_mut(),
        padding2: 0,
        num_rois: 0,
        roi_data: std::ptr::null(),
    };
    let mut output = ExecuteOutput {
        output_buffer: session.buf_flow,
        output_cost_buffer: std::ptr::null_mut(),
        priv_data: std::ptr::null_mut(),
        bwd_output_buffer: std::ptr::null_mut(),
        bwd_output_cost_buffer: std::ptr::null_mut(),
        global_flow_buffer: std::ptr::null_mut(),
    };
    let status = (api.execute)(session.handle, &input, &mut output);
    if status != NV_OF_SUCCESS {
        crate::debug_log(&format!(
            "ofa execute status={status} err={}",
            error_text(api, session.handle)
        ));
        return Err("execute_failed");
    }
    // The pair's reference is the next pair's input frame (`b` becomes `a`), so
    // swap the roles: the next continuous submit then uploads only its own new
    // frame. Handles travel with their textures.
    std::mem::swap(&mut session.tex_cur, &mut session.tex_next);
    std::mem::swap(&mut session.buf_cur, &mut session.buf_next);
    Ok(())
}

unsafe fn read_flow(context: Com, session: &mut Session) -> Result<(), &'static str> {
    // ID3D11DeviceContext vtable: CopyResource is index 47, Map 14, Unmap 15
    // (IUnknown 0..=2, ID3D11DeviceChild 3..=6, then the context methods).
    let copy: CopyResourceFn = vtbl(context, 47);
    copy(context, session.tex_read, session.tex_flow);
    let map: MapFn = vtbl(context, 14);
    let mut mapped: Mapped = std::mem::zeroed();
    if map(
        context,
        session.tex_read,
        0,
        D3D11_MAP_READ,
        0,
        &mut mapped,
    ) < 0
        || mapped.data.is_null()
    {
        return Err("map_failed");
    }
    let cells = session.gw as usize * 2;
    let row_bytes = cells * 2;
    let pitch = mapped.row_pitch as usize;
    // Byte-wise copy: the mapped pointer is not guaranteed to be 2-byte aligned,
    // and `i16` would make that UB.
    for y in 0..session.gh as usize {
        let from = (mapped.data as *const u8).add(y * pitch);
        let to = session.flow.as_mut_ptr().add(y * cells) as *mut u8;
        std::ptr::copy_nonoverlapping(from, to, row_bytes);
    }
    let unmap: UnmapFn = vtbl(context, 15);
    unmap(context, session.tex_read, 0);
    Ok(())
}

// ---- CPU warp --------------------------------------------------------------

/// Warp and blend one display frame out of the pair the flow belongs to.
///
/// `flow` is the grid-4 field of `(a, b)`: two `i16` per cell in S10.5 fixed
/// point, cell `(x / 4, y / 4)`. This is the same warp `motion.rs` applies to
/// its block field, generalised to the finer grid and with the per-cell offsets
/// folded into a row table:
///
/// * `a` is warped forward by `phase * u`, `b` backward by `(1 - phase) * u`,
///   and the two warped samples are blended with the phase weights;
/// * no confidence term is needed: a cell whose flow is zero warps to the very
///   sample a plain blend would read, so flat or unmatchable areas degrade to
///   the blend by construction;
/// * rows are split across at most [`MAX_THREADS`] bands, like the block-match
///   warp, and nothing is allocated per call.
///
/// Returns `false` (leaving `out` untouched) when the inputs are unusable, so
/// the caller blends instead.
pub fn warp(
    out: &mut [u8],
    a: &[u8],
    b: &[u8],
    flow: &[i16],
    gw: u32,
    gh: u32,
    w: u32,
    h: u32,
    phase: f32,
) -> bool {
    let (wu, hu) = (w as usize, h as usize);
    let (guw, guh) = (gw as usize, gh as usize);
    if guw == 0 || guh == 0 || guw > MAX_GRID_W || wu < 2 || hu == 0 {
        return false;
    }
    let bytes = wu.saturating_mul(hu).saturating_mul(4);
    if out.len() < bytes
        || a.len() < bytes
        || b.len() < bytes
        || flow.len() < guw.saturating_mul(guh).saturating_mul(2)
    {
        return false;
    }
    let out = &mut out[..bytes];
    let a = &a[..bytes];
    let b = &b[..bytes];
    let phase = if phase.is_finite() {
        phase.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let tw = (phase * 256.0).round().clamp(0.0, 256.0) as u32;
    let wa = 256 - tw;
    let (xa, xb) = (1.0 - phase, phase);
    let threads = warp_threads().min(hu);
    let band = (hu + threads - 1) / threads;
    let wmax = (wu - 1) as i32;
    let hmax = (hu - 1) as i32;
    let stride = GRID as usize;
    std::thread::scope(|scope| {
        for (index, chunk) in out.chunks_mut(band * wu * 4).enumerate() {
            let y_start = index * band;
            let rows = chunk.len() / (wu * 4);
            scope.spawn(move || {
                // SAFETY: every offset is clamped into the frame before use;
                // `chunk` covers `rows` whole rows of `out`, `a` and `b` are
                // whole frames of the same size, and the three buffers never
                // overlap (`out` is borrowed exclusively by this call).
                let out_ptr = chunk.as_mut_ptr();
                let a_ptr = a.as_ptr();
                let b_ptr = b.as_ptr();
                // Per-cell offsets, rebuilt once per grid cell row (four pixel
                // rows share it): the arithmetic is per cell, not per pixel.
                let mut dxa = [0i32; MAX_GRID_W];
                let mut dya = [0i32; MAX_GRID_W];
                let mut dxb = [0i32; MAX_GRID_W];
                let mut dyb = [0i32; MAX_GRID_W];
                let mut table_row = usize::MAX;
                for row in 0..rows {
                    let y = y_start + row;
                    let cell_row_index = (y / stride).min(guh - 1);
                    if cell_row_index != table_row {
                        table_row = cell_row_index;
                        let cell_row = cell_row_index * guw;
                        for bx in 0..guw {
                            let at = (cell_row + bx) * 2;
                            let vx = flow[at] as f32 * (1.0 / 32.0);
                            let vy = flow[at + 1] as f32 * (1.0 / 32.0);
                            dxa[bx] = (vx * xa).round() as i32;
                            dya[bx] = (vy * xa).round() as i32;
                            dxb[bx] = (vx * xb).round() as i32;
                            dyb[bx] = (vy * xb).round() as i32;
                        }
                    }
                    for bx in 0..guw {
                        let x0 = bx * stride;
                        let x1 = (x0 + stride).min(wu);
                        let ay = ((y as i32) - dya[bx]).clamp(0, hmax) as usize;
                        let byy = ((y as i32) + dyb[bx]).clamp(0, hmax) as usize;
                        let row_a = ay * wu;
                        let row_b = byy * wu;
                        let out_row = row * wu;
                        // Interior cell: all four pixels sample inside both
                        // frames, so one 16-byte fixed-point blend produces the
                        // whole cell (the same arithmetic `blend_rgba_sse2` in
                        // `player.rs` uses, four pixels at a time). This is the
                        // common case and the only reason a 1080p warp fits a
                        // display slot on a CPU.
                        if x1 - x0 == stride {
                            let ax0 = x0 as i32 - dxa[bx];
                            let bx0 = x0 as i32 + dxb[bx];
                            if ax0 >= 0 && ax0 + 3 <= wmax && bx0 >= 0 && bx0 + 3 <= wmax {
                                #[cfg(target_arch = "x86_64")]
                                unsafe {
                                    let o = out_ptr.add((out_row + x0) * 4);
                                    let pa = a_ptr.add((row_a + ax0 as usize) * 4);
                                    let pb = b_ptr.add((row_b + bx0 as usize) * 4);
                                    blend16(o, pa, pb, wa as i16, tw as i16);
                                }
                                #[cfg(not(target_arch = "x86_64"))]
                                {
                                    for x in x0..x1 {
                                        let ax = (x as i32 - dxa[bx]).clamp(0, wmax) as usize;
                                        let bxx = (x as i32 + dxb[bx]).clamp(0, wmax) as usize;
                                        let pa = (row_a + ax) * 4;
                                        let pb = (row_b + bxx) * 4;
                                        let o = unsafe { out_ptr.add((out_row + x) * 4) };
                                        for channel in 0..3 {
                                            let sum = unsafe {
                                                *a_ptr.add(pa + channel) as u32 * wa
                                                    + *b_ptr.add(pb + channel) as u32 * tw
                                            };
                                            unsafe {
                                                *o.add(channel) = ((sum + 128) >> 8) as u8;
                                            }
                                        }
                                        unsafe { *o.add(3) = 255 };
                                    }
                                }
                                continue;
                            }
                        }
                        for x in x0..x1 {
                            let ax = ((x as i32) - dxa[bx]).clamp(0, wmax) as usize;
                            let bxx = ((x as i32) + dxb[bx]).clamp(0, wmax) as usize;
                            let pa = (row_a + ax) * 4;
                            let pb = (row_b + bxx) * 4;
                            let o = unsafe { out_ptr.add((out_row + x) * 4) };
                            for channel in 0..3 {
                                // SAFETY: `pa`/`pb` come from clamped indices of
                                // two full frames, `o` from a clamped row/column.
                                let sum = unsafe {
                                    *a_ptr.add(pa + channel) as u32 * wa
                                        + *b_ptr.add(pb + channel) as u32 * tw
                                };
                                unsafe {
                                    *o.add(channel) = ((sum + 128) >> 8) as u8;
                                }
                            }
                            unsafe { *o.add(3) = 255 };
                        }
                    }
                }
            });
        }
    });
    true
}

fn warp_threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, MAX_THREADS)
    })
}

/// Blend four pixels (16 bytes) in place: `out = (a * wa + b * tw) >> 8`, alpha
/// forced opaque.
///
/// The same fixed-point arithmetic as `blend_rgba_sse2` in `player.rs`, reduced
/// to one 16-byte block because the warp's source offsets change per grid cell.
/// `wa + tw == 256`, so no intermediate exceeds `255 * 256 + 128` and the
/// unsigned 16-bit multiply cannot overflow.
///
/// # Safety
/// `out`, `a` and `b` must each be readable/writable for 16 bytes.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn blend16(out: *mut u8, a: *const u8, b: *const u8, wa: i16, tw: i16) {
    use std::arch::x86_64::*;
    let zero = _mm_setzero_si128();
    let av = _mm_loadu_si128(a as *const __m128i);
    let bv = _mm_loadu_si128(b as *const __m128i);
    let wav = _mm_set1_epi16(wa);
    let wbv = _mm_set1_epi16(tw);
    let round = _mm_set1_epi16(128);
    let alpha = _mm_set1_epi32(0xFF00_0000u32 as i32);
    let blend = |a8: __m128i, b8: __m128i| -> __m128i {
        let alo = _mm_unpacklo_epi8(a8, zero);
        let ahi = _mm_unpackhi_epi8(a8, zero);
        let blo = _mm_unpacklo_epi8(b8, zero);
        let bhi = _mm_unpackhi_epi8(b8, zero);
        let lo = _mm_srli_epi16(
            _mm_add_epi16(
                _mm_add_epi16(_mm_mullo_epi16(alo, wav), _mm_mullo_epi16(blo, wbv)),
                round,
            ),
            8,
        );
        let hi = _mm_srli_epi16(
            _mm_add_epi16(
                _mm_add_epi16(_mm_mullo_epi16(ahi, wav), _mm_mullo_epi16(bhi, wbv)),
                round,
            ),
            8,
        );
        let packed = _mm_packus_epi16(lo, hi);
        _mm_or_si128(_mm_andnot_si128(alpha, packed), alpha)
    };
    let result = blend(av, bv);
    _mm_storeu_si128(out as *mut __m128i, result);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(x: usize, y: usize, seed: u32) -> u32 {
        let mut n = (x as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add((y as u32).wrapping_mul(0x85EB_CA6B))
            .wrapping_add(seed.wrapping_mul(0xC2B2_AE35));
        n ^= n >> 13;
        n = n.wrapping_mul(0x27D4_EB2F);
        n ^ (n >> 16)
    }

    /// A smooth, high-contrast texture: an optical-flow engine tracks motion on
    /// gradients and structure, not on per-pixel noise.
    fn texture(w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let cell = hash((x / 48) * 7, (y / 48) * 11, 3) & 0x3F;
                let ramp_x = (x * 255 / (w as usize).max(1)) as u32;
                let ramp_y = (y * 255 / (h as usize).max(1)) as u32;
                let i = (y * w as usize + x) * 4;
                out[i] = (cell as u32 * 3 + ramp_x / 2) as u8;
                out[i + 1] = (cell as u32 * 2 + ramp_y / 2) as u8;
                out[i + 2] = ((cell ^ ramp_x as u32) & 0xFF) as u8;
                out[i + 3] = 255;
            }
        }
        out
    }

    /// `dst(x, y) = src(x - shift, y)`, with edge pixels replicated.
    fn shift(src: &[u8], w: u32, h: u32, shift: i32) -> Vec<u8> {
        let mut out = vec![0u8; src.len()];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let sx = ((x as i32 - shift).clamp(0, w as i32 - 1)) as usize;
                let si = (y * w as usize + sx) * 4;
                let di = (y * w as usize + x) * 4;
                out[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
        out
    }

    fn crossfade(out: &mut [u8], a: &[u8], b: &[u8]) {
        for i in 0..out.len() {
            out[i] = ((a[i] as u32 + b[i] as u32 + 1) >> 1) as u8;
        }
        for pixel in out.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }

    fn mae(a: &[u8], b: &[u8]) -> f64 {
        let mut sum = 0u64;
        for (x, y) in a.iter().zip(b.iter()) {
            sum += (*x as i32 - *y as i32).unsigned_abs() as u64;
        }
        sum as f64 / a.len().max(1) as f64
    }

    #[test]
    fn the_transcribed_structs_match_the_header_layout() {
        // The driver reads these by offset, so a wrong layout is a wrong ABI.
        assert_eq!(std::mem::size_of::<InitParams>(), 64);
        assert_eq!(std::mem::size_of::<ExecuteInput>(), 56);
        assert_eq!(std::mem::size_of::<ExecuteOutput>(), 48);
        assert_eq!(
            std::mem::size_of::<Api>(),
            10 * std::mem::size_of::<usize>()
        );
        assert_eq!(std::mem::size_of::<TexDesc>(), 44);
        assert_eq!(std::mem::size_of::<AdapterDesc>(), 304);
        assert_eq!(std::mem::size_of::<Mapped>(), 16);
        assert_eq!(std::mem::align_of::<ExecuteOutput>(), 8);
    }

    #[test]
    fn the_warp_of_a_zero_field_is_the_plain_blend() {
        let (w, h) = (64u32, 48u32);
        let a = texture(w, h);
        let b = shift(&a, w, h, 6);
        let (gw, gh) = (w / GRID, h / GRID);
        let flow = vec![0i16; gw as usize * gh as usize * 2];
        let mut warped = vec![0u8; a.len()];
        assert!(warp(&mut warped, &a, &b, &flow, gw, gh, w, h, 0.5));
        let mut blended = vec![0u8; a.len()];
        crossfade(&mut blended, &a, &b);
        let difference = mae(&warped, &blended);
        assert!(
            difference <= 1.0,
            "a zero flow warped to MAE {difference:.3} away from the cross-fade"
        );
    }

    #[test]
    fn the_warp_moves_a_frame_by_the_flow() {
        // A constant flow of +8 px with phase 0.5: A is sampled at `x - 4` and B
        // at `x + 4`, so a pair shifted by 8 px resolves to the half-way picture.
        let (w, h) = (64u32, 48u32);
        let a = texture(w, h);
        let b = shift(&a, w, h, 8);
        let truth = shift(&a, w, h, 4);
        let (gw, gh) = (w / GRID, h / GRID);
        let mut flow = vec![0i16; gw as usize * gh as usize * 2];
        for cell in flow.chunks_exact_mut(2) {
            cell[0] = 8 * 32;
        }
        let mut out = vec![0u8; a.len()];
        assert!(warp(&mut out, &a, &b, &flow, gw, gh, w, h, 0.5));
        let err = mae(&out, &truth);
        crate::debug_log(&format!("ofa warp constant mae={err:.3}"));
        assert!(err < 1.0, "constant flow warp MAE {err:.3}");
    }

    #[test]
    fn ofa_midpoint_beats_the_crossfade_at_1080p() {
        if !Ofa::available() {
            crate::debug_log(&format!(
                "ofa test skipped reason={}",
                Ofa::unavailable_reason()
            ));
            return;
        }
        let (w, h) = (1920u32, 1080u32);
        let a = texture(w, h);
        let b = shift(&a, w, h, 16);
        let truth = shift(&a, w, h, 8);
        let mut engine = Ofa::new();
        assert!(engine.ensure(w, h), "ensure failed: {:?}", engine.off_reason());
        assert!(engine.submit(&a, &b, w, h), "submit failed");
        // No fence in this binding: the field of a pair is readable one pair
        // later, so give the engine a full interval of wall time.
        std::thread::sleep(std::time::Duration::from_millis(40));
        let (flow, gw, gh) = engine.try_flow().expect("flow readback");
        assert_eq!((gw, gh), (w / GRID, h / GRID));
        // The centre of a 16 px rightward pan must report ^16 px (+512 in S10.5).
        let centre = ((gh / 2) * gw + gw / 2) as usize * 2;
        let vx = flow[centre] as f32 / 32.0;
        let vy = flow[centre + 1] as f32 / 32.0;
        crate::debug_log(&format!(
            "ofa flow centre=({vx:.2},{vy:.2}) px submit_ms={:.3} flow_ms={:.3}",
            engine.last_ms(),
            engine.flow_ms()
        ));
        assert!(
            (vx - 16.0).abs() < 2.5 && vy.abs() < 2.5,
            "centre flow ({vx:.2},{vy:.2}) is not the 16 px pan"
        );
        let mut out = vec![0u8; a.len()];
        assert!(warp(&mut out, &a, &b, &flow, gw, gh, w, h, 0.5));
        let ofa_err = mae(&out, &truth);
        let mut blended = vec![0u8; a.len()];
        crossfade(&mut blended, &a, &b);
        let blend_err = mae(&blended, &truth);
        crate::debug_log(&format!(
            "ofa quality shifted16 mae_ofa={ofa_err:.3} mae_blend={blend_err:.3}"
        ));
        assert!(
            ofa_err < 1.0,
            "OFA midpoint MAE {ofa_err:.3} is not under 1"
        );
        assert!(
            blend_err > ofa_err * 3.0,
            "cross-fade MAE {blend_err:.3} is not clearly worse than OFA {ofa_err:.3}"
        );
    }

    /// Steady-state cost of one pair, the number the report quotes: the upload +
    /// exec, the field readback and the CPU warp, at the two sizes the pacer
    /// uses. Also the QA guard against a regression that would not fit a slot.
    ///
    /// The +1-pair latency is not part of a pair's cost (it is pipelined), so
    /// each iteration sleeps one interval before reading the field — the same
    /// wall-time relationship the pacer has.
    #[test]
    fn ofa_pair_cost_at_1080p_and_720p() {
        if !Ofa::available() {
            crate::debug_log(&format!(
                "ofa cost skipped reason={}",
                Ofa::unavailable_reason()
            ));
            return;
        }
        for (w, h, label) in [(1920u32, 1080u32, "1080p"), (1280, 720, "720p")] {
            let a = texture(w, h);
            let b = shift(&a, w, h, 16);
            let mut engine = Ofa::new();
            assert!(engine.ensure(w, h), "ensure {label}: {:?}", engine.off_reason());
            let mut out = vec![0u8; a.len()];
            let mut submit = Vec::new();
            let mut flow_t = Vec::new();
            let mut warp_t = Vec::new();
            for round in 0..14 {
                // Consecutive pairs share a frame, exactly like the pacer's:
                // alternate the two frames so only one upload happens per pair.
                let (first, second) = if round % 2 == 0 { (&a, &b) } else { (&b, &a) };
                let started = Instant::now();
                assert!(engine.submit(first, second, w, h), "submit {label}");
                let submit_ms = started.elapsed().as_secs_f64() * 1000.0;
                std::thread::sleep(std::time::Duration::from_millis(33));
                let (flow, gw, gh) = engine.try_flow().expect("flow");
                let flow_ms = engine.flow_ms() as f64;
                let started = Instant::now();
                assert!(warp(&mut out, first, second, &flow, gw, gh, w, h, 0.5));
                let warp_ms = started.elapsed().as_secs_f64() * 1000.0;
                if round >= 2 {
                    submit.push(submit_ms);
                    flow_t.push(flow_ms);
                    warp_t.push(warp_ms);
                }
            }
            let stats = |mut values: Vec<f64>| -> (f64, f64, f64) {
                values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let p50 = values[values.len() / 2];
                let p95 = values[(values.len() as f64 * 0.95) as usize % values.len()];
                (values[0], p50, p95)
            };
            let (sub_min, sub_p50, sub_p95) = stats(submit);
            let (flow_min, flow_p50, flow_p95) = stats(flow_t);
            let (warp_min, warp_p50, warp_p95) = stats(warp_t);
            crate::debug_log(&format!(
                "ofa cost {label} submit_ms min={sub_min:.2} p50={sub_p50:.2} p95={sub_p95:.2} \
                 flow_ms min={flow_min:.2} p50={flow_p50:.2} p95={flow_p95:.2} \
                 warp_ms min={warp_min:.2} p50={warp_p50:.2} p95={warp_p95:.2} \
                 total_p50={:.2}",
                sub_p50 + flow_p50 + warp_p50
            ));
            // A display slot at 60 Hz is 16.7 ms; the whole pair has to leave
            // room for the ticks of the interval it runs in. Measured values are
            // well inside this; the bound only catches a real regression.
            #[cfg(not(debug_assertions))]
            assert!(
                sub_p50 + flow_p50 + warp_p50 < 16.0,
                "{label} pair p50 {:.2} ms does not fit a 60 Hz slot",
                sub_p50 + flow_p50 + warp_p50
            );
        }
    }

    #[test]
    fn a_missing_engine_degrades_instead_of_panicking() {
        // Whatever this machine has, the constructor and the probes must be
        // total: no panic, and a failed engine answers every call cheaply.
        let mut engine = Ofa::new();
        let _ = engine.ensure(1920, 1080);
        let frame = vec![0u8; 64 * 64 * 4];
        let submitted = engine.submit(&frame, &frame, 64, 64);
        if !submitted {
            assert!(engine.try_flow().is_none());
        } else {
            let _ = engine.try_flow();
        }
        engine.reset();
        assert!(engine.try_flow().is_none());
        let _ = Ofa::present();
        let _ = Ofa::available();
        let _ = Ofa::unavailable_reason();
    }
}
