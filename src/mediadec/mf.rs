//! Media Foundation hardware decode into D3D11 textures.
//!
//! # What this is
//!
//! A source-reader pipeline whose *output* is an `ID3D11Texture2D`, never a CPU
//! frame:
//!
//! ```text
//! B 站 DASH fMP4 URL
//!   -> HttpByteStream   (IMFByteStream over reqwest + Range, 1 MB windows)
//!   -> IMFSourceReader  (MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS)
//!   -> IMFDXGIDeviceManager -> the caller's ID3D11Device
//!   -> IMFSample -> IMFDXGIBuffer -> ID3D11Texture2D (NV12) + subresource
//! ```
//!
//! The one thing that must never happen is the source reader handing back a CPU
//! buffer: that means the decoder (or the video processor) fell back to system
//! memory, and the whole point of this module - zero bytes per frame to the
//! upper layer - is gone. That case is an explicit `Err`, not a silent copy.
//!
//! # Threading
//!
//! `ReadSample` blocks, sometimes on a CDN that has gone quiet, and the API
//! contract here has a per-frame budget. So the source reader lives on its own
//! thread ("mf-decode") and the public calls are request/reply over channels:
//! `next_frame(budget)` waits at most `budget` for the reply and returns a
//! timeout error instead of pinning the caller. One thread owns the reader, so
//! no MF object is ever touched concurrently.
//!
//! The caller's `ID3D11Device` is only *used* through the DXGI device manager
//! (MF locks it on its decode threads), which is why `open` calls
//! `ID3D10Multithread::SetMultithreadProtected` first. The device must be
//! created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`; that is the caller's job
//! (see `examples/mf_probe.rs`) and `ResetDevice` fails loudly if it was not.
//!
//! # Samples are a pool
//!
//! MF hands out textures from the decoder's own array; the returned
//! `MfFrame.texture` is AddRef'ed, but holding many frames stalls the decoder.
//! Consume and drop, as the probe does.
//!
//! The array is the DXVA picture pool: an 852x480 stream comes back as a
//! 864x480 NV12 texture with `ArraySize` around 15, and `subresource` names the
//! slice. The surface is *padded* (864, not 852), so a sampler has to use
//! `MfFrame.texture_width` / `texture_height` for the NV12 plane geometry and
//! `width` / `height` only for the aspect ratio.
//!
//! # Two traps worth remembering
//!
//! * `MFGetService(buffer, MR_BUFFER_SERVICE, IID_IMFDXGIBuffer)` - the call the
//!   samples in the docs use - returns `E_NOINTERFACE` (0x80004002) for the
//!   in-box DXVA surface, while a plain `QueryInterface(IID_IMFDXGIBuffer)` on
//!   the same buffer succeeds. Use the QI first; the service call is kept only
//!   as a fallback for a decoder that answers the other way round.
//! * A CPU sample and a padded D3D sample are not distinguishable by size:
//!   `GetCurrentLength()` says 622,080 for both. Only the QI decides, which is
//!   why the "no hardware decode" error is raised after the interface check and
//!   never inferred from a byte count.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use windows::core::{implement, Interface, Ref, BOOL, GUID, IUnknown};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, E_POINTER};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFByteStream, IMFByteStream_Impl,
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFSourceReader, IMFTransform,
    MFBYTESTREAM_IS_READABLE, MFBYTESTREAM_IS_SEEKABLE, MFBYTESTREAM_SEEK_ORIGIN,
    MFCreateAsyncResult, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromByteStream, MFCreateSourceReaderFromURL, MFGetService,
    MFMediaType_Video, MFStartup,
    MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoFormat_P010, MFVideoFormat_RGB32, MFVideoFormat_VP90, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SA_D3D11_AWARE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_DISABLE_DXVA, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MFSTARTUP_LITE, MFT_ENUM_HARDWARE_URL_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MF_VERSION, MR_BUFFER_SERVICE,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;

/// One `Range` request's worth of bytes. Small enough that a seek is cheap,
/// large enough that sequential decode is not one request per 32 KB read.
const RANGE_WINDOW: u64 = 1024 * 1024;
/// A server that answers a ranged request with `200 OK` and the whole body is
/// only tolerated when it is this small; B 站 CDNs answer `206`.
const MAX_FULL_BODY: u64 = 32 * 1024 * 1024;
/// Network call ceiling for one range request (far above the per-frame budget:
/// this bounds a hung CDN, `next_frame` bounds the caller).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `open` waits for the worker to negotiate a stream.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// `ReadSample` may report a stream tick without a picture; do not spin forever.
const MAX_TICKS_PER_FRAME: u32 = 64;

// MFBYTESTREAM_SEEK_ORIGIN has no named members in the bindings (they are the
// `msoBegin` / `msoCurrent` table), so name the three values here once.
pub(crate) const SEEK_BEGIN: MFBYTESTREAM_SEEK_ORIGIN = MFBYTESTREAM_SEEK_ORIGIN(0);
pub(crate) const SEEK_CURRENT: MFBYTESTREAM_SEEK_ORIGIN = MFBYTESTREAM_SEEK_ORIGIN(1);
pub(crate) const SEEK_END: MFBYTESTREAM_SEEK_ORIGIN = MFBYTESTREAM_SEEK_ORIGIN(2);

static STARTUP: OnceLock<Result<(), String>> = OnceLock::new();

/// `MFStartup` exactly once per process. The platform is not shut down on exit
/// on purpose: MF teardown at process exit is not required, and a `MFShutdown`
/// racing a live source reader is worse than not calling it.
pub(crate) fn startup() -> Result<(), String> {
    STARTUP
        .get_or_init(|| {
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }
                .map_err(|err| format!("MFStartup 失败: {err}"))
        })
        .clone()
}

/// A decoded picture that is still on the GPU.
///
/// `subresource` is the array slice MF put the picture in: hardware decoders
/// keep a texture array and rotate through its slices, so the sink has to
/// sample `texture` with this subresource rather than slice 0.
pub(crate) struct MfFrame {
    pub texture: ID3D11Texture2D,
    pub subresource: u32,
    /// Visible picture size, i.e. the stream's own `MF_MT_FRAME_SIZE`.
    pub width: u32,
    pub height: u32,
    /// The texture's own size. Hardware decoders pad the surface (852 becomes
    /// 864 here), and NV12's chroma plane starts at `texture_height` rows of
    /// `texture_width` bytes, so a sampler must use these, not `width/height`.
    pub texture_width: u32,
    pub texture_height: u32,
    pub pts: Duration,
}

/// What the negotiated stream actually is. Reported by the probe and the log so
/// a HEVC-without-codec machine can be told apart from a working one.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DecoderInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    /// What the container carries (H264 / HEVC / AV1 / ...).
    pub native_subtype: GUID,
    /// What the reader outputs (NV12 / P010).
    pub output_subtype: GUID,
    /// `MF_SA_D3D11_AWARE` on the decoder MFT. `true` means the decoder can
    /// take the DXGI device manager and allocate its surfaces on the GPU, which
    /// is what the in-box H.264 decoder reports.
    pub d3d11_aware: bool,
    /// `MFT_ENUM_HARDWARE_URL_Attribute`: `Some(true)` means MF picked a
    /// hardware *MFT* (a GPU-vendor decoder object). The in-box DXVA decoders
    /// are software MFTs that set `d3d11_aware` instead, so `Some(false)` here
    /// with `d3d11_aware = true` is the normal DXVA case.
    pub hardware_mft: Option<bool>,
    /// `DXGI_FORMAT` of the first texture handed out, once one has been seen.
    pub texture_format: Option<i32>,
}

/// The decoder the player will own in f209.
pub(crate) struct MfVideoDecoder {
    commands: mpsc::Sender<Command>,
    info: Arc<Mutex<DecoderInfo>>,
    frames: Arc<AtomicU64>,
    cpu_frames: Arc<AtomicU64>,
    /// The device/context stay referenced for as long as the worker may use
    /// them (the manager holds its own reference; this is belt and braces and
    /// keeps the public lifetime honest).
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
}

impl MfVideoDecoder {
    /// Open `url` (a B 站 DASH fMP4 video stream) on `device`.
    ///
    /// `headers` must carry at least `Referer` and `User-Agent`; a logged-in
    /// `Cookie` is needed for tiers above 1080p. `device` must have been created
    /// with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`.
    pub(crate) fn open(
        url: &str,
        headers: &[(String, String)],
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("MF 解码器: 视频 URL 为空".to_string());
        }
        Self::open_source(
            Source::Url {
                url: url.to_string(),
                headers: headers.to_vec(),
            },
            device,
            context,
        )
    }

    /// Open a local file through the same source reader (f209 QA).
    ///
    /// The acceptance sample is `ffmpeg`'s own H.264 1080p30 (see
    /// `scripts/qa-f209.ps1`), so the decode path, the surface pool and the
    /// NV12 textures are exactly the ones a DASH stream produces - only the
    /// byte stream is a file instead of HTTP ranges. Nothing else changes:
    /// the decoder still has to hand back D3D11 pictures or it fails.
    pub(crate) fn open_path(
        path: &std::path::Path,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self, String> {
        let text = path.to_string_lossy().to_string();
        if !path.is_file() {
            return Err(format!("MF 解码器: 文件不存在 {text}"));
        }
        Self::open_source(Source::Path(text), device, context)
    }

    fn open_source(
        source: Source,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self, String> {
        startup()?;
        // MF's device manager locks the device from its own decode threads.
        // Without this the hardware transform can corrupt its own state.
        unsafe {
            let _ = device
                .cast::<ID3D10Multithread>()
                .map_err(|err| format!("MF 解码器: 设备不支持 ID3D10Multithread: {err}"))?
                .SetMultithreadProtected(true);
        }
        // A context from another device would make f209's texture handoff read
        // the wrong device; say so now, once, but do not refuse to decode.
        match unsafe { context.GetDevice() } {
            Ok(own_device) if Interface::as_raw(&own_device) != Interface::as_raw(device) => {
                crate::debug_log("mf decoder: device/context mismatch (textures are on device)");
            }
            _ => {}
        }

        let info = Arc::new(Mutex::new(DecoderInfo::default()));
        let frames = Arc::new(AtomicU64::new(0));
        let cpu_frames = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (commands, command_rx) = mpsc::channel();

        let worker_device = device.clone();
        let worker_source = source.clone();
        let worker_info = info.clone();
        let worker_frames = frames.clone();
        let worker_cpu_frames = cpu_frames.clone();
        let _worker = std::thread::Builder::new()
            .name("mf-decode".to_string())
            .spawn(move || {
                worker_main(
                    worker_device,
                    worker_source,
                    worker_info,
                    worker_frames,
                    worker_cpu_frames,
                    ready_tx,
                    command_rx,
                )
            })
            .map_err(|err| format!("MF 解码器: 启动解码线程失败: {err}"))?;

        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("MF 解码器: 打开视频流超时（30s）".to_string()),
        }

        Ok(Self {
            commands,
            info,
            frames,
            cpu_frames,
            _device: device.clone(),
            _context: context.clone(),
        })
    }

    /// The next picture, or `Ok(None)` at end of stream.
    ///
    /// Waits at most `budget` for the decoder; a slow CDN or a stalled decoder
    /// is an error, never an unbounded block. A timeout does not poison the
    /// decoder: the read that was still running is dropped when it finishes and
    /// the next call reads the next sample.
    pub(crate) fn next_frame(&mut self, budget: Duration) -> Result<Option<MfFrame>, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::Read { reply: reply_tx })
            .map_err(|_| "MF 解码器: 解码线程已退出".to_string())?;
        let frame = match reply_rx.recv_timeout(budget) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!("MF 解码器: 等待画面超时（{} ms）", budget.as_millis()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("MF 解码器: 解码线程在读取画面时退出".to_string())
            }
        };
        let Some(raw) = frame else {
            return Ok(None);
        };
        let (width, height) = {
            let info = self.info.lock().map_err(|_| "MF 解码器: 信息锁损坏")?;
            (info.width, info.height)
        };
        Ok(Some(MfFrame {
            texture: raw.texture,
            subresource: raw.subresource,
            width,
            height,
            texture_width: raw.texture_width,
            texture_height: raw.texture_height,
            pts: Duration::from_nanos(raw.pts.max(0) as u64 * 100),
        }))
    }

    /// Seek to `to` (100 ns resolution, MF's native time unit).
    pub(crate) fn seek(&mut self, to: Duration) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::Seek { to, reply: reply_tx })
            .map_err(|_| "MF 解码器: 解码线程已退出".to_string())?;
        match reply_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err("MF 解码器: 定位超时（30s）".to_string()),
        }
    }

    /// Width, height, frames per second of the negotiated stream.
    pub(crate) fn info(&self) -> (u32, u32, f32) {
        match self.info.lock() {
            Ok(info) => (info.width, info.height, info.fps),
            Err(_) => (0, 0, 0.0),
        }
    }

    /// Everything `info()` does not say: codecs, hardware-ness, texture format.
    pub(crate) fn details(&self) -> DecoderInfo {
        self.info.lock().map(|info| *info).unwrap_or_default()
    }

    /// Pictures handed out so far, and how many of them arrived as CPU memory
    /// (which is an error path here, so this staying 0 is the contract).
    pub(crate) fn stats(&self) -> (u64, u64) {
        (
            self.frames.load(Ordering::Relaxed),
            self.cpu_frames.load(Ordering::Relaxed),
        )
    }
}

impl Drop for MfVideoDecoder {
    fn drop(&mut self) {
        // Deliberately not joined: the worker may be inside a range request
        // against a slow CDN, and a UI thread must not wait 20 s for a
        // decoder it is throwing away. The thread owns the reader and ends
        // when it sees `Stop` or the closed channel.
        let _ = self.commands.send(Command::Stop);
    }
}

/// Where the bytes come from. One enum so the worker and the reader setup have
/// exactly one shape: the file variant exists for the QA sample (f209), and the
/// URL variant is the product path.
#[derive(Clone)]
enum Source {
    Url {
        url: String,
        headers: Vec<(String, String)>,
    },
    Path(String),
}

enum Command {
    Read {
        reply: mpsc::Sender<Result<Option<RawFrame>, String>>,
    },
    Seek {
        to: Duration,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop,
}

struct RawFrame {
    texture: ID3D11Texture2D,
    subresource: u32,
    texture_width: u32,
    texture_height: u32,
    /// 100 ns, MF's sample time.
    pts: i64,
}

fn worker_main(
    device: ID3D11Device,
    source: Source,
    info: Arc<Mutex<DecoderInfo>>,
    frames: Arc<AtomicU64>,
    cpu_frames: Arc<AtomicU64>,
    ready: mpsc::Sender<Result<(), String>>,
    commands: mpsc::Receiver<Command>,
) {
    // MF objects are COM objects; the decoder runs on this thread, so this is
    // where the apartment belongs. An STA elsewhere in the process does not
    // matter: this thread takes the MTA and MF's own work queues stay agile.
    let com = ComGuard::init();
    let (reader, _device_manager) = match open_reader(&device, &source, &info) {
        Ok(opened) => opened,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    while let Ok(command) = commands.recv() {
        match command {
            Command::Read { reply } => {
                let result = read_one(&reader, &info, &frames, &cpu_frames);
                let _ = reply.send(result);
            }
            Command::Seek { to, reply } => {
                let _ = reply.send(seek_reader(&reader, to));
            }
            Command::Stop => break,
        }
    }
    drop(com);
}

/// `CoInitializeEx(MTA)` for the worker thread; uninitializes only when this
/// thread did the initialisation.
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn init() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK means *this* call initialised the apartment; S_FALSE means
        // somebody already did (not possible on a fresh thread, but the
        // distinction is what makes the CoUninitialize below balanced).
        Self {
            owned: hr == windows::Win32::Foundation::S_OK,
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

// ---------------------------------------------------------------------------
// Source reader
// ---------------------------------------------------------------------------

fn open_reader(
    device: &ID3D11Device,
    source: &Source,
    info: &Mutex<DecoderInfo>,
) -> Result<(IMFSourceReader, IMFDXGIDeviceManager), String> {
    let mut reset_token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
        .map_err(|err| format!("MFCreateDXGIDeviceManager 失败: {err}"))?;
    let manager = manager.ok_or_else(|| "MFCreateDXGIDeviceManager 返回空指针".to_string())?;
    let device_unknown: IUnknown = device
        .cast()
        .map_err(|err| format!("ID3D11Device 转 IUnknown 失败: {err}"))?;
    // Fails when the device was created without
    // D3D11_CREATE_DEVICE_VIDEO_SUPPORT, which is exactly the mistake worth
    // naming out loud.
    unsafe { manager.ResetDevice(&device_unknown, reset_token) }.map_err(|err| {
        format!("IMFDXGIDeviceManager::ResetDevice 失败（设备需要用 D3D11_CREATE_DEVICE_VIDEO_SUPPORT 创建）: {err}")
    })?;

    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 8) }
        .map_err(|err| format!("MFCreateAttributes 失败: {err}"))?;
    let attributes = attributes.ok_or_else(|| "MFCreateAttributes 返回空指针".to_string())?;
    let set_u32 = |key: &GUID, value: u32| -> Result<(), String> {
        unsafe { attributes.SetUINT32(key, value) }.map_err(|err| format!("设置 MF 属性失败: {err}"))
    };
    set_u32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    set_u32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    // FALSE = DXVA is allowed. Spelled out because the default has changed
    // between Windows versions and this path depends on it being allowed.
    set_u32(&MF_SOURCE_READER_DISABLE_DXVA, 0)?;
    let manager_unknown: IUnknown = manager
        .cast()
        .map_err(|err| format!("IMFDXGIDeviceManager 转 IUnknown 失败: {err}"))?;
    unsafe { attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager_unknown) }
        .map_err(|err| format!("MF_SOURCE_READER_D3D_MANAGER 设置失败: {err}"))?;

    let reader = match source {
        Source::Url { url, headers } => {
            let byte_stream: IMFByteStream = HttpByteStream::new(url, headers)?.into();
            unsafe { MFCreateSourceReaderFromByteStream(&byte_stream, &attributes) }
                .map_err(|err| format!("MFCreateSourceReaderFromByteStream 失败: {err}"))?
        }
        Source::Path(path) => {
            // A local file: MF's own file byte stream. Same attributes, so the
            // decoder still gets the DXGI device manager and the hardware
            // transform policy.
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                MFCreateSourceReaderFromURL(
                    windows::core::PCWSTR(wide.as_ptr()),
                    &attributes,
                )
            }
            .map_err(|err| format!("MFCreateSourceReaderFromURL({path}) 失败: {err}"))?
        }
    };
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    unsafe { reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false) }
        .map_err(|err| format!("SetStreamSelection(all, false) 失败: {err}"))?;
    unsafe { reader.SetStreamSelection(first_video, true) }
        .map_err(|err| format!("SetStreamSelection(video, true) 失败: {err}"))?;

    let native = unsafe { reader.GetNativeMediaType(first_video, 0) }
        .map_err(|err| format!("GetNativeMediaType 失败: {err}"))?;
    let native_subtype = unsafe { native.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();

    // Leave the decoder whatever it wants unless that is not a D3D11 texture
    // format; only then ask for NV12 (and P010 for 10-bit, which some HEVC
    // decoders insist on).
    let mut output_subtype = current_subtype(&reader).unwrap_or_default();
    if output_subtype != MFVideoFormat_NV12 && output_subtype != MFVideoFormat_P010 {
        output_subtype = negotiate_output(&reader, native_subtype)?;
    }
    if output_subtype != MFVideoFormat_NV12 && output_subtype != MFVideoFormat_P010 {
        return Err(format!(
            "MF 解码器: 解码器没有输出 NV12/P010（原生 {} -> 输出 {}）；硬件解码不可用",
            subtype_name(&native_subtype),
            subtype_name(&output_subtype)
        ));
    }

    let decoder = inspect_decoder(&reader);
    let (d3d11_aware, hardware_mft) = decoder.unwrap_or((false, None));
    {
        let mut slot = info.lock().map_err(|_| "MF 解码器: 信息锁损坏")?;
        slot.native_subtype = native_subtype;
        slot.output_subtype = output_subtype;
        slot.d3d11_aware = d3d11_aware;
        slot.hardware_mft = hardware_mft;
    }
    refresh_info(&reader, info)?;
    crate::debug_log(&format!(
        "mf decoder opened codec={} output={} size={}x{} fps={:.2} d3d11_aware={} hardware_mft={}",
        subtype_name(&native_subtype),
        subtype_name(&output_subtype),
        info.lock().map(|i| i.width).unwrap_or(0),
        info.lock().map(|i| i.height).unwrap_or(0),
        info.lock().map(|i| i.fps).unwrap_or(0.0),
        d3d11_aware,
        match hardware_mft {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    ));
    Ok((reader, manager))
}

fn current_subtype(reader: &IMFSourceReader) -> Option<GUID> {
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let media_type = unsafe { reader.GetCurrentMediaType(first_video) }.ok()?;
    unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.ok()
}

/// Ask the reader for a D3D11-compatible output type and return what it agreed
/// to (the current type if neither request was accepted).
fn negotiate_output(reader: &IMFSourceReader, native_subtype: GUID) -> Result<GUID, String> {
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    for candidate in [MFVideoFormat_NV12, MFVideoFormat_P010] {
        let media_type = unsafe { MFCreateMediaType() }
            .map_err(|err| format!("MFCreateMediaType 失败: {err}"))?;
        unsafe { media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
            .map_err(|err| format!("设置主类型失败: {err}"))?;
        unsafe { media_type.SetGUID(&MF_MT_SUBTYPE, &candidate) }
            .map_err(|err| format!("设置子类型失败: {err}"))?;
        match unsafe { reader.SetCurrentMediaType(first_video, None, &media_type) } {
            Ok(()) => {
                let got = current_subtype(reader).unwrap_or_default();
                if got == candidate {
                    return Ok(got);
                }
            }
            Err(err) => crate::debug_log(&format!(
                "mf decoder: SetCurrentMediaType({}) refused for {}: {err}",
                subtype_name(&candidate),
                subtype_name(&native_subtype)
            )),
        }
    }
    Ok(current_subtype(reader).unwrap_or_default())
}

/// Does the decoder MFT take the GPU, and is it a hardware MFT?
///
/// The decoder for a stream is reachable through `GetServiceForStream` with the
/// *stream* index (the media-source index returns the demuxer, which has none of
/// these attributes). Hardware MFTs publish `MFT_ENUM_HARDWARE_URL_Attribute`;
/// the in-box DXVA decoders are software MFTs that set `MF_SA_D3D11_AWARE`
/// instead, which is why both are reported.
fn inspect_decoder(reader: &IMFSourceReader) -> Option<(bool, Option<bool>)> {
    let (name, d3d11_aware, hardware_url) = decoder_attributes(reader)?;
    crate::debug_log(&format!(
        "mf decoder transform name=\"{name}\" d3d11_aware={d3d11_aware} hardware_url={hardware_url}"
    ));
    Some((d3d11_aware, Some(hardware_url)))
}

fn decoder_attributes(reader: &IMFSourceReader) -> Option<(String, bool, bool)> {
    let mut raw = std::ptr::null_mut();
    unsafe {
        reader.GetServiceForStream(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            &GUID::zeroed(),
            &IMFTransform::IID,
            &mut raw,
        )
    }
    .ok()?;
    if raw.is_null() {
        return None;
    }
    let transform: IMFTransform = unsafe { Interface::from_raw(raw) };
    let attributes = unsafe { transform.GetAttributes() }.ok()?;
    let mut name = windows::core::PWSTR::null();
    let mut name_len = 0u32;
    let name = if unsafe {
        attributes.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut name_len)
    }
    .is_ok()
    {
        let text = unsafe { name.to_string() }.unwrap_or_default();
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const core::ffi::c_void));
        }
        text
    } else {
        String::new()
    };
    let d3d11_aware = unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
    let hardware_url =
        unsafe { attributes.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute) }.is_ok();
    Some((name, d3d11_aware, hardware_url))
}

fn refresh_info(reader: &IMFSourceReader, info: &Mutex<DecoderInfo>) -> Result<(), String> {
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let current = unsafe { reader.GetCurrentMediaType(first_video) }
        .map_err(|err| format!("GetCurrentMediaType 失败: {err}"))?;
    // The *native* type carries the stream's own frame size (852x480); the
    // current/output type carries the decoder surface, which is padded to a
    // macroblock boundary (864x480). The decoder's size is the frame's
    // `texture_width`, not its visible size, so prefer the native one.
    let native = unsafe { reader.GetNativeMediaType(first_video, 0) }.ok();
    let mut slot = info.lock().map_err(|_| "MF 解码器: 信息锁损坏")?;
    if let Ok(subtype) = unsafe { current.GetGUID(&MF_MT_SUBTYPE) } {
        slot.output_subtype = subtype;
    }
    if let Some((width, height)) = native
        .as_ref()
        .and_then(media_size)
        .or_else(|| media_size(&current))
    {
        slot.width = width;
        slot.height = height;
    }
    if let Some(fps) = native
        .as_ref()
        .and_then(media_fps)
        .or_else(|| media_fps(&current))
    {
        slot.fps = fps;
    }
    Ok(())
}

fn media_size(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    let width = (packed >> 32) as u32;
    let height = (packed & 0xffff_ffff) as u32;
    (width > 0 && height > 0).then_some((width, height))
}

fn media_fps(media_type: &IMFMediaType) -> Option<f32> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.ok()?;
    let numerator = (packed >> 32) as u32;
    let denominator = packed as u32;
    (numerator > 0 && denominator > 0).then(|| numerator as f32 / denominator as f32)
}

fn subtype_name(subtype: &GUID) -> &'static str {
    if *subtype == MFVideoFormat_H264 {
        "H264"
    } else if *subtype == MFVideoFormat_HEVC {
        "HEVC"
    } else if *subtype == MFVideoFormat_AV1 {
        "AV1"
    } else if *subtype == MFVideoFormat_VP90 {
        "VP9"
    } else if *subtype == MFVideoFormat_NV12 {
        "NV12"
    } else if *subtype == MFVideoFormat_P010 {
        "P010"
    } else if *subtype == MFVideoFormat_RGB32 {
        "RGB32"
    } else {
        "unknown"
    }
}

fn read_one(
    reader: &IMFSourceReader,
    info: &Mutex<DecoderInfo>,
    frames: &AtomicU64,
    cpu_frames: &AtomicU64,
) -> Result<Option<RawFrame>, String> {
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    for _ in 0..MAX_TICKS_PER_FRAME {
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                first_video,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .map_err(|err| format!("ReadSample 失败: {err}"))?;

        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err("MF 解码器: 源读取器报告 MF_SOURCE_READERF_ERROR（流损坏或解码器故障）".to_string());
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            refresh_info(reader, info)?;
        }
        // A stream tick (or a type change) carries no picture: ask again.
        let Some(sample) = sample else {
            continue;
        };

        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|err| format!("IMFSample::GetBufferByIndex 失败: {err}"))?;
        // A hardware-decoded sample's buffer is an IMFDXGIBuffer; a CPU buffer
        // cannot be one. Two ways in, and only one of them works everywhere:
        //
        //   * `QueryInterface(IID_IMFDXGIBuffer)` - what MF itself uses;
        //   * `MFGetService(..., MR_BUFFER_SERVICE, ...)` - documented, but the
        //     in-box DXVA surface answers E_NOINTERFACE (0x80004002) to the
        //     service call while the QI succeeds. Tried second so a machine
        //     where the reverse holds still works.
        let dxgi: IMFDXGIBuffer = match buffer.cast::<IMFDXGIBuffer>() {
            Ok(dxgi) => dxgi,
            Err(_) => {
                let mut raw = std::ptr::null_mut();
                let serviced = unsafe {
                    MFGetService(&buffer, &MR_BUFFER_SERVICE, &IMFDXGIBuffer::IID, &mut raw)
                }
                .is_ok()
                    && !raw.is_null();
                if serviced {
                    unsafe { IMFDXGIBuffer::from_raw(raw) }
                } else {
                    cpu_frames.fetch_add(1, Ordering::Relaxed);
                    let bytes = unsafe { buffer.GetCurrentLength() }.unwrap_or(0);
                    return Err(format!(
                        "MF 解码器: 解码器返回了 CPU 缓冲（{bytes} 字节），没有走硬件解码；本模块不做 CPU→GPU 上传"
                    ));
                }
            }
        };

        let mut raw_resource = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw_resource) }
            .map_err(|err| format!("IMFDXGIBuffer::GetResource 失败: {err}"))?;
        if raw_resource.is_null() {
            return Err("MF 解码器: IMFDXGIBuffer::GetResource 返回空纹理".to_string());
        }
        let texture: ID3D11Texture2D = unsafe { Interface::from_raw(raw_resource) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }
            .map_err(|err| format!("IMFDXGIBuffer::GetSubresourceIndex 失败: {err}"))?;

        let (texture_width, texture_height) = check_texture(info, &texture)?;
        let pts = unsafe { sample.GetSampleTime() }.unwrap_or(timestamp);
        frames.fetch_add(1, Ordering::Relaxed);
        return Ok(Some(RawFrame {
            texture,
            subresource,
            texture_width,
            texture_height,
            pts,
        }));
    }
    Err("MF 解码器: 连续收到流事件但没有画面".to_string())
}

/// The texture must be an NV12/P010 array slice, or the zero-copy contract is
/// already broken (an RGB texture here would mean a video processor did a
/// conversion the sink cannot sample as YUV).
fn check_texture(
    info: &Mutex<DecoderInfo>,
    texture: &ID3D11Texture2D,
) -> Result<(u32, u32), String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_NV12 && desc.Format != DXGI_FORMAT_P010 {
        return Err(format!(
            "MF 解码器: 解码纹理格式是 DXGI_FORMAT {}，不是 NV12/P010",
            desc.Format.0
        ));
    }
    if let Ok(mut slot) = info.lock() {
        slot.texture_format = Some(desc.Format.0);
    }
    Ok((desc.Width, desc.Height))
}

fn seek_reader(reader: &IMFSourceReader, to: Duration) -> Result<(), String> {
    let ticks = (to.as_nanos() / 100).min(i64::MAX as u128) as i64;
    let mut position = PROPVARIANT::default();
    unsafe {
        let inner = &mut *position.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = ticks;
    }
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    // Discard anything the decoder has queued for the old position first;
    // otherwise the next ReadSample can still hand back a pre-seek picture.
    if let Err(err) = unsafe { reader.Flush(first_video) } {
        crate::debug_log(&format!("mf decoder: Flush before seek failed: {err}"));
    }
    unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &position) }
        .map_err(|err| format!("IMFSourceReader::SetCurrentPosition 失败: {err}"))
}

// ---------------------------------------------------------------------------
// IMFByteStream over HTTP Range
// ---------------------------------------------------------------------------

/// The source reader wants seek/read over a URL; B 站 DASH is a plain fMP4
/// resource behind a signed URL, so ranges are all it takes.
///
/// One 1 MB window is kept in memory per stream. `BeginRead` completes
/// synchronously (perform the read, then invoke the callback): legal per the
/// `IMFByteStream` contract, and it keeps the whole thing free of completion
/// ports and extra threads.
#[implement(IMFByteStream)]
struct HttpByteStream {
    url: String,
    headers: Vec<(String, String)>,
    client: reqwest::Client,
    state: Mutex<StreamState>,
}

struct StreamState {
    position: u64,
    length: Option<u64>,
    window_start: u64,
    window: Vec<u8>,
    last_read: u32,
    closed: bool,
}

impl HttpByteStream {
    fn new(url: &str, headers: &[(String, String)]) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent(crate::bilibili::UA)
            .connect_timeout(Duration::from_secs(5))
            .timeout(HTTP_TIMEOUT)
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|err| format!("MF 字节流: HTTP 客户端创建失败: {err}"))?;
        Ok(Self {
            url: url.to_string(),
            headers: headers.to_vec(),
            client,
            state: Mutex::new(StreamState {
                position: 0,
                length: None,
                window_start: 0,
                window: Vec::new(),
                last_read: 0,
                closed: false,
            }),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, StreamState>, String> {
        self.state
            .lock()
            .map_err(|_| "MF 字节流: 状态锁损坏".to_string())
    }

    /// Copy from the window into `buffer`, pulling windows as needed.
    fn read_into(&self, buffer: &mut [u8]) -> Result<usize, String> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock()?;
        if state.closed {
            return Err("MF 字节流: 流已关闭".to_string());
        }
        let mut written = 0usize;
        while written < buffer.len() {
            let position = state.position;
            if let Some(length) = state.length {
                if position >= length {
                    break;
                }
            }
            let window_end = state.window_start + state.window.len() as u64;
            let inside = position >= state.window_start && position < window_end;
            if !inside {
                self.fill_window(&mut state, position)?;
                if state.window.is_empty() {
                    break; // end of stream
                }
                continue;
            }
            let offset = (position - state.window_start) as usize;
            let available = state.window.len() - offset;
            let take = available.min(buffer.len() - written);
            buffer[written..written + take]
                .copy_from_slice(&state.window[offset..offset + take]);
            written += take;
            state.position = position + take as u64;
        }
        Ok(written)
    }

    fn fill_window(&self, state: &mut StreamState, at: u64) -> Result<(), String> {
        let end = at.saturating_add(RANGE_WINDOW - 1);
        let (bytes, total) = self.fetch(at, end)?;
        if let Some(total) = total {
            state.length = Some(total);
        }
        state.window_start = at;
        state.window = bytes;
        if state.window.is_empty() && state.length.is_none() {
            // A 206 for the tail with no Content-Range: nothing after `at`.
            state.length = Some(at);
        }
        Ok(())
    }

    fn length(&self, state: &mut StreamState) -> Result<u64, String> {
        if let Some(length) = state.length {
            return Ok(length);
        }
        let (_probe, total) = self.fetch(0, 0)?;
        let length = total.ok_or_else(|| {
            "MF 字节流: 服务器没有返回内容长度（Content-Range 缺失），无法定位".to_string()
        })?;
        state.length = Some(length);
        Ok(length)
    }

    /// One ranged GET. Returns the bytes and the total resource length when the
    /// server reports it.
    fn fetch(&self, start: u64, end: u64) -> Result<(Vec<u8>, Option<u64>), String> {
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        let runtime = crate::bilibili::shared_runtime()
            .ok_or_else(|| "MF 字节流: 网络运行时不可用".to_string())?;
        let response = block_on(runtime, async move {
            let mut request = client
                .get(&url)
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let response = request
                .send()
                .await
                .map_err(|err| format!("MF 字节流: HTTP 请求失败: {err}"))?;
            let status = response.status();
            let total = response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range_total);
            let length = response.content_length();
            let body = response
                .bytes()
                .await
                .map_err(|err| format!("MF 字节流: 读取响应体失败: {err}"))?;
            Ok::<_, String>((status.as_u16(), total, length, body.to_vec()))
        })?;
        let (status, total, content_length, body) = response;

        match status {
            // Partial content: exactly the window we asked for.
            206 => Ok((body, total)),
            // Range not understood and the whole resource came back.
            200 => {
                if start > 0 {
                    return Err(format!(
                        "MF 字节流: 服务器忽略了 Range（HTTP 200），无法随机访问（请求 {start}-{end}）"
                    ));
                }
                let whole = content_length.unwrap_or(body.len() as u64);
                if whole > MAX_FULL_BODY {
                    return Err(format!(
                        "MF 字节流: 服务器返回了完整文件（{whole} 字节）而不是分段内容，拒绝整段下载"
                    ));
                }
                Ok((body, Some(whole)))
            }
            // Requested range is past the end: no bytes, but often a length.
            416 => Ok((Vec::new(), total)),
            other => Err(format!("MF 字节流: HTTP {other}（Range {start}-{end}）")),
        }
    }
}

impl IMFByteStream_Impl for HttpByteStream_Impl {
    fn GetCapabilities(&self) -> windows::core::Result<u32> {
        Ok(MFBYTESTREAM_IS_READABLE | MFBYTESTREAM_IS_SEEKABLE)
    }

    fn GetLength(&self) -> windows::core::Result<u64> {
        let mut state = self.lock().map_err(stream_error)?;
        match state.length {
            Some(length) => Ok(length),
            None => {
                let length = self.length(&mut state).map_err(stream_error)?;
                Ok(length)
            }
        }
    }

    fn SetLength(&self, _length: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetCurrentPosition(&self) -> windows::core::Result<u64> {
        Ok(self.lock().map_err(stream_error)?.position)
    }

    fn SetCurrentPosition(&self, position: u64) -> windows::core::Result<()> {
        let mut state = self.lock().map_err(stream_error)?;
        if state.closed {
            return Err(stream_error("MF 字节流: 流已关闭"));
        }
        state.position = position;
        Ok(())
    }

    fn IsEndOfStream(&self) -> windows::core::Result<BOOL> {
        let mut state = self.lock().map_err(stream_error)?;
        let length = match state.length {
            Some(length) => length,
            None => self.length(&mut state).map_err(stream_error)?,
        };
        Ok(BOOL::from(state.position >= length))
    }

    fn Read(&self, buffer: *mut u8, count: u32, read: *mut u32) -> windows::core::Result<()> {
        if count == 0 || buffer.is_null() {
            if !read.is_null() {
                unsafe { *read = 0 };
            }
            return Ok(());
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(buffer, count as usize) };
        let written = self.read_into(slice).map_err(stream_error)?;
        if !read.is_null() {
            unsafe { *read = written as u32 };
        }
        Ok(())
    }

    fn BeginRead(
        &self,
        buffer: *mut u8,
        count: u32,
        callback: Ref<IMFAsyncCallback>,
        state: Ref<IUnknown>,
    ) -> windows::core::Result<()> {
        let Some(callback) = callback.as_ref() else {
            return Err(E_POINTER.into());
        };
        let written = if count == 0 || buffer.is_null() {
            0
        } else {
            let slice = unsafe { std::slice::from_raw_parts_mut(buffer, count as usize) };
            self.read_into(slice).map_err(stream_error)?
        };
        self.lock().map_err(stream_error)?.last_read = written as u32;
        // The read is done: complete the operation now. The callback runs on
        // this thread, so the state lock above is already released.
        let no_object: Option<&IUnknown> = None;
        let result = unsafe { MFCreateAsyncResult(no_object, callback, state.as_ref()) }
            .map_err(windows::core::Error::from)?;
        unsafe { callback.Invoke(&result) }
    }

    fn EndRead(&self, _result: Ref<IMFAsyncResult>) -> windows::core::Result<u32> {
        Ok(self.lock().map_err(stream_error)?.last_read)
    }

    fn Write(&self, _buffer: *const u8, _count: u32) -> windows::core::Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn BeginWrite(
        &self,
        _buffer: *const u8,
        _count: u32,
        _callback: Ref<IMFAsyncCallback>,
        _state: Ref<IUnknown>,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndWrite(&self, _result: Ref<IMFAsyncResult>) -> windows::core::Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn Seek(
        &self,
        origin: MFBYTESTREAM_SEEK_ORIGIN,
        offset: i64,
        _flags: u32,
    ) -> windows::core::Result<u64> {
        let mut state = self.lock().map_err(stream_error)?;
        let end = if origin == SEEK_END {
            Some(match state.length {
                Some(length) => length,
                None => self.length(&mut state).map_err(stream_error)?,
            })
        } else {
            state.length
        };
        let target = seek_target(origin, state.position, end, offset)
            .ok_or_else(|| stream_error("MF 字节流: 定位结果在文件起点之前或长度未知"))?;
        state.position = target;
        Ok(target)
    }

    fn Flush(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Close(&self) -> windows::core::Result<()> {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.window = Vec::new();
        }
        Ok(())
    }
}

/// Resolve a `Seek` to an absolute position. `None` when the caller asked for a
/// position before the start, or seeked from the end of an unknown length.
pub(crate) fn seek_target(
    origin: MFBYTESTREAM_SEEK_ORIGIN,
    current: u64,
    end: Option<u64>,
    offset: i64,
) -> Option<u64> {
    let base: i64 = if origin == SEEK_BEGIN {
        0
    } else if origin == SEEK_CURRENT {
        i64::try_from(current).ok()?
    } else {
        i64::try_from(end?).ok()?
    };
    let target = base.checked_add(offset)?;
    (target >= 0).then_some(target as u64)
}

/// `bytes 0-0/1234567` or `bytes */1234567` -> `Some(1234567)`.
pub(crate) fn parse_content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    let total = total.trim();
    if total.is_empty() || total == "*" {
        None
    } else {
        total.parse().ok()
    }
}

/// `E_FAIL` plus a log line: the reason ends up in the debug log, which is the
/// only place a failing byte stream can be explained (the HRESULT itself has no
/// room for a message).
fn stream_error(reason: impl AsRef<str>) -> windows::core::Error {
    crate::debug_log(&format!("mf byte stream: {}", reason.as_ref()));
    windows::core::Error::from(E_FAIL)
}

/// Run `future` on the app's network runtime from a thread that is not one of
/// its workers (MF calls the byte stream on its own threads). The fallback
/// exists because a tokio worker may never `block_on` its own runtime.
fn block_on<F>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| match scope.spawn(|| runtime.block_on(future)).join() {
            Ok(value) => value,
            Err(_) => panic!("MF 字节流: 网络线程 panic"),
        })
    } else {
        runtime.block_on(future)
    }
}
