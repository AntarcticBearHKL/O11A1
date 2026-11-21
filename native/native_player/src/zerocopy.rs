//! In-process zero-copy video decode.
//!
//! The old pipeline ran `ffmpeg.exe`, downloaded every picture to system
//! memory (`hwdownload`) and re-uploaded it through `UpdateSubresource`. This
//! module keeps the picture on the player's D3D11 device from the decoder all
//! the way to the BGRA conversion / enhancement / blend passes:
//!
//! * libavformat demuxes the same URL the external pipe used (including the
//!   disk cache's direct path or loopback URL);
//! * libavcodec decodes through D3D11VA on the player's own `ID3D11Device`;
//! * each `AVFrame` carries an `ID3D11Texture2D` array slice (NV12 or P010),
//!   which becomes two plane SRVs and is sampled by `nv12_hw_main` /
//!   `p010_hw_main` straight into the existing GPU picture ring.
//!
//! No decoded bytes cross the CPU boundary. The only CPU work per picture is
//! creating two views (cheap, and the driver reuses the same textures) plus a
//! 32×18 luma probe for scene-cut detection.
//!
//! Failure policy: opening or decoding the first picture returns an error to
//! `decode::VideoDecoder::spawn`, which then falls back to the external ffmpeg
//! pipe. The fallback keeps live streams, exotic codecs and old drivers
//! playable without a second player build.

use std::ffi::{c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ffmpeg_sys_next as ff;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device3, ID3D11ShaderResourceView, ID3D11ShaderResourceView1,
    ID3D11Texture2D, D3D11_SHADER_RESOURCE_VIEW_DESC1, D3D11_TEX2D_ARRAY_SRV1,
};
use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2DARRAY;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
};

use crate::decode::{DecodedFrame, FrameData, FrameQueue};
use crate::gpu::Gpu;
use crate::interp::Signature;
use crate::log;

const AV_NOPTS_VALUE: i64 = i64::MIN;
const AVERROR_EAGAIN: c_int = -11;
const AVERROR_EOF: c_int = -541_478_725;
const AV_TIME_BASE_Q_DEN: i64 = 1_000;

/// The same `AVD3D11VADeviceContext` FFmpeg's `hwcontext_d3d11va.h` declares.
/// `ffmpeg-sys-next` does not bind the Windows-specific header, so keep the
/// ABI here. The struct is only ever read by libavutil/libavcodec.
#[repr(C)]
struct D3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    /// `D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE`.
    bind_flags: u32,
    misc_flags: u32,
}

/// One reference-counted `AVFrame`. The decoder thread creates these and the
/// engine thread drops them after the GPU conversion pass has been submitted;
/// the AVBuffer refcount keeps FFmpeg from recycling the D3D11 slice while the
/// picture is still queued.
pub struct AVFrameRef(*mut ff::AVFrame);

// The AVFrame is only used by one thread at a time. AVBuffer references are
// atomic, so moving the owning pointer between the decode and presentation
// threads is sound.
unsafe impl Send for AVFrameRef {}
unsafe impl Sync for AVFrameRef {}

impl AVFrameRef {
    fn clone_from(source: *mut ff::AVFrame) -> Result<Self> {
        let frame = unsafe { ff::av_frame_clone(source) };
        if frame.is_null() {
            return Err(anyhow!("av_frame_clone returned null"));
        }
        Ok(Self(frame))
    }

    fn texture_ptr(&self) -> *mut ID3D11Texture2D {
        unsafe { (*self.0).data[0] as *mut ID3D11Texture2D }
    }

    fn slice(&self) -> u32 {
        unsafe { (*self.0).data[1] as usize as u32 }
    }

    fn sw_format(&self) -> ff::AVPixelFormat {
        unsafe {
            let hw = (*self.0).hw_frames_ctx;
            if hw.is_null() {
                return ff::AVPixelFormat::AV_PIX_FMT_NONE;
            }
            let frames = (*hw).data as *mut ff::AVHWFramesContext;
            if frames.is_null() {
                return ff::AVPixelFormat::AV_PIX_FMT_NONE;
            }
            (*frames).sw_format
        }
    }

    fn width(&self) -> u32 {
        unsafe { (*self.0).width.max(0) as u32 }
    }

    fn height(&self) -> u32 {
        unsafe { (*self.0).height.max(0) as u32 }
    }
}

impl Drop for AVFrameRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let mut frame = self.0;
                ff::av_frame_free(&mut frame);
            }
            self.0 = ptr::null_mut();
        }
    }
}

/// A decoded D3D11 picture: the AVFrame keeps the texture slice alive, the two
/// SRVs are the luma / chroma views the conversion shader samples.
pub struct HWFrame {
    frame: AVFrameRef,
    pub y_srv: ID3D11ShaderResourceView,
    pub uv_srv: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
    pub p010: bool,
    pub signature: Signature,
    pub bytes: usize,
}

impl HWFrame {
    fn new(
        frame: AVFrameRef,
        device: &ID3D11Device,
        signature: Signature,
    ) -> Result<Self> {
        let width = frame.width();
        let height = frame.height();
        if width < 2 || height < 2 {
            return Err(anyhow!("zero-copy frame has invalid size {width}x{height}"));
        }
        let sw_format = frame.sw_format();
        let (p010, y_format, uv_format, bytes_per_pixel) = match sw_format {
            ff::AVPixelFormat::AV_PIX_FMT_NV12 => (false, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, 3usize),
            // P010 is the only 10-bit D3D11VA output the DXVA2 helpers expose.
            ff::AVPixelFormat::AV_PIX_FMT_P010LE => (true, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM, 3),
            other => {
                return Err(anyhow!(
                    "zero-copy D3D11 frames have unsupported software format {}",
                    unsafe { CStr::from_ptr(ff::av_get_pix_fmt_name(other)) }
                        .to_string_lossy()
                ));
            }
        };
        let texture = frame.texture_ptr();
        if texture.is_null() {
            return Err(anyhow!("D3D11VA frame has no texture"));
        }
        let slice = frame.slice();
        let view = |format, plane: u32| -> Result<ID3D11ShaderResourceView> {
            let device3: ID3D11Device3 = device
                .cast()
                .map_err(|error| anyhow!("ID3D11Device3 unavailable: {error}"))?;
            let description = D3D11_SHADER_RESOURCE_VIEW_DESC1 {
                Format: format,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_SHADER_RESOURCE_VIEW_DESC1_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_SRV1 {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        PlaneSlice: plane,
                    },
                },
            };
            // The AVFrame owns the texture ref; this wrapper must not release
            // it when the function returns.
            let borrowed = std::mem::ManuallyDrop::new(unsafe {
                ID3D11Texture2D::from_raw(texture as *mut c_void)
            });
            let mut out: Option<ID3D11ShaderResourceView1> = None;
            unsafe {
                device3
                    .CreateShaderResourceView1(&*borrowed, Some(&description), Some(&mut out))
                    .map_err(|error| anyhow!("CreateShaderResourceView1 plane {plane}: {error}"))?;
            }
            let out = out.ok_or_else(|| anyhow!("CreateShaderResourceView1 plane {plane}: no view"))?;
            out.cast::<ID3D11ShaderResourceView>()
                .map_err(|error| anyhow!("cast plane {plane} SRV: {error}"))
        };
        let y_srv = view(y_format, 0)?;
        let uv_srv = view(uv_format, 1)?;
        let bytes = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(bytes_per_pixel);
        Ok(Self {
            frame,
            y_srv,
            uv_srv,
            width,
            height,
            p010,
            signature,
            bytes,
        })
    }
}

unsafe extern "C" fn interrupt(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let stop = &*(opaque as *const AtomicBool);
    i32::from(stop.load(Ordering::Relaxed))
}

/// A demuxer + D3D11VA decoder pair. All FFmpeg pointers live on the decode
/// thread; `Drop` runs there as well.
struct NativeDecoder {
    format: *mut ff::AVFormatContext,
    codec: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    stream_index: c_int,
    time_base: ff::AVRational,
    origin_ms: u64,
    start_ms: u64,
    fps: f64,
    fallback_ms: u64,
    eof: bool,
    flushed: bool,
}

unsafe impl Send for NativeDecoder {}

impl NativeDecoder {
    fn open(
        url: &str,
        headers: &[(String, String)],
        remote: bool,
        live: bool,
        start_ms: u64,
        fps: f64,
        stop: &Arc<AtomicBool>,
        gpu: &Gpu,
    ) -> Result<Self> {
        static NETWORK: Once = Once::new();
        NETWORK.call_once(|| unsafe {
            let _ = ff::avformat_network_init();
        });

        let url_c = CString::new(url).context("media URL contains NUL")?;
        let mut options: *mut ff::AVDictionary = ptr::null_mut();
        let mut set = |key: &str, value: &str| {
            let key = CString::new(key).unwrap_or_default();
            let value = CString::new(value).unwrap_or_default();
            unsafe {
                ff::av_dict_set(&mut options, key.as_ptr(), value.as_ptr(), 0);
            }
        };
        if remote {
            set("reconnect", "1");
            set("reconnect_streamed", "1");
            set("reconnect_delay_max", "5");
            set("rw_timeout", "15000000");
            set("probesize", "1000000");
            set("analyzeduration", "1000000");
            let header_text = crate::proc::header_string(headers);
            if !header_text.is_empty() {
                set("headers", &header_text);
            }
        }

        let mut format = unsafe { ff::avformat_alloc_context() };
        if format.is_null() {
            unsafe { ff::av_dict_free(&mut options) };
            return Err(anyhow!("avformat_alloc_context returned null"));
        }
        unsafe {
            (*format).interrupt_callback = ff::AVIOInterruptCB {
                callback: Some(interrupt),
                opaque: Arc::as_ptr(stop) as *mut c_void,
            };
        }
        // The interrupt callback must be installed before open: a stopped seek
        // has to be able to abort a stalled HTTP read.
        let opened = unsafe {
            ff::avformat_open_input(
                &mut format,
                url_c.as_ptr(),
                ptr::null(),
                &mut options,
            )
        };
        unsafe { ff::av_dict_free(&mut options) };
        if opened < 0 {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!(
                "avformat_open_input failed: {}",
                ff_error(opened)
            ));
        }
        let info = unsafe { ff::avformat_find_stream_info(format, ptr::null_mut()) };
        if info < 0 {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!(
                "avformat_find_stream_info failed: {}",
                ff_error(info)
            ));
        }
        let mut codec_ptr: *const ff::AVCodec = ptr::null();
        let stream_index = unsafe {
            ff::av_find_best_stream(
                format,
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                ptr::null_mut(),
                0,
            )
        };
        if stream_index < 0 {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!("no video stream found"));
        }
        let stream = unsafe { *(*format).streams.add(stream_index as usize) };
        if stream.is_null() {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!("video stream pointer is null"));
        }
        let parameters = unsafe { (*stream).codecpar };
        if parameters.is_null() {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!("video stream parameters are null"));
        }
        let time_base = unsafe { (*stream).time_base };
        if time_base.num <= 0 || time_base.den <= 0 {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!("video stream has invalid time base"));
        }
        let start_time = unsafe { (*stream).start_time };
        let origin_ms = if start_time == AV_NOPTS_VALUE {
            0
        } else {
            unsafe {
                ff::av_rescale_q(
                    start_time,
                    time_base,
                    ff::AVRational {
                        num: 1,
                        den: AV_TIME_BASE_Q_DEN as c_int,
                    },
                )
            }
            .max(0) as u64
        };
        // The default decoder can be libdav1d (AV1) or another software-only
        // implementation. Explicitly try the native decoder for the stream's
        // codec, which is the one carrying D3D11VA hw configs.
        codec_ptr = unsafe { ff::avcodec_find_decoder((*parameters).codec_id) };
        if codec_ptr.is_null() || !codec_supports_d3d11(codec_ptr) {
            let native_name = match unsafe { (*parameters).codec_id } {
                ff::AVCodecID::AV_CODEC_ID_H264 => Some("h264"),
                ff::AVCodecID::AV_CODEC_ID_HEVC => Some("hevc"),
                ff::AVCodecID::AV_CODEC_ID_AV1 => Some("av1"),
                ff::AVCodecID::AV_CODEC_ID_VP9 => Some("vp9"),
                ff::AVCodecID::AV_CODEC_ID_MPEG2VIDEO => Some("mpeg2video"),
                ff::AVCodecID::AV_CODEC_ID_VC1 => Some("vc1"),
                _ => None,
            };
            if let Some(name) = native_name {
                let c_name = CString::new(name).unwrap_or_default();
                codec_ptr = unsafe { ff::avcodec_find_decoder_by_name(c_name.as_ptr()) };
            }
        }
        if codec_ptr.is_null() || !codec_supports_d3d11(codec_ptr) {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!("codec has no D3D11VA decode configuration"));
        }
        if std::env::var("O11A1_ZC_DEBUG").ok().as_deref() == Some("1") {
            let mut index = 0;
            loop {
                let config = unsafe { ff::avcodec_get_hw_config(codec_ptr, index) };
                if config.is_null() {
                    break;
                }
                unsafe {
                    log::line(&format!(
                        "zc hwconfig codec={:?} device={:?} methods=0x{:x} pix_fmt={:?}",
                        (*parameters).codec_id,
                        (*config).device_type,
                        (*config).methods,
                        (*config).pix_fmt
                    ));
                }
                index += 1;
            }
        }
        let mut codec = unsafe { ff::avcodec_alloc_context3(codec_ptr) };
        if codec.is_null() {
            unsafe { ff::avformat_close_input(&mut format) };
            return Err(anyhow!("avcodec_alloc_context3 returned null"));
        }
        let copied = unsafe { ff::avcodec_parameters_to_context(codec, parameters) };
        if copied < 0 {
            unsafe {
                ff::avcodec_free_context(&mut codec);
                ff::avformat_close_input(&mut format);
            }
            return Err(anyhow!(
                "avcodec_parameters_to_context failed: {}",
                ff_error(copied)
            ));
        }
        unsafe {
            (*codec).pkt_timebase = time_base;
            (*codec).get_format = Some(get_format);
            // The decoder's internal pool is static for D3D11VA. A 12-picture
            // presenter ring plus B-frame reordering must never exhaust it.
            (*codec).extra_hw_frames = 24;
        }

        // Hand FFmpeg our own device. FFmpeg takes ownership of these COM
        // references and releases them when the codec context is freed.
        let device_ref = gpu.device.clone();
        let context_ref = gpu.context.clone();
        let hw = unsafe { ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA) };
        if hw.is_null() {
            unsafe {
                ff::avcodec_free_context(&mut codec);
                ff::avformat_close_input(&mut format);
            }
            return Err(anyhow!("av_hwdevice_ctx_alloc(D3D11VA) returned null"));
        }
        unsafe {
            let device_ctx = (*hw).data as *mut ff::AVHWDeviceContext;
            let hwctx = (*device_ctx).hwctx as *mut D3D11VADeviceContext;
            if hwctx.is_null() {
                let mut hw = hw;
                ff::av_buffer_unref(&mut hw);
                ff::avcodec_free_context(&mut codec);
                ff::avformat_close_input(&mut format);
                return Err(anyhow!("D3D11VA device context has no hwctx"));
            }
            (*hwctx).device = device_ref.into_raw();
            (*hwctx).device_context = context_ref.into_raw();
            (*hwctx).video_device = ptr::null_mut();
            (*hwctx).video_context = ptr::null_mut();
            (*hwctx).lock = None;
            (*hwctx).unlock = None;
            (*hwctx).lock_ctx = ptr::null_mut();
            // The decoder flags are OR'd into the frames context by
            // `ff_dxva2_common_frame_params`; SHADER_RESOURCE makes the
            // texture sampleable by the plane SRVs.
            (*hwctx).bind_flags = windows::Win32::Graphics::Direct3D11::D3D11_BIND_DECODER.0 as u32
                | windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0 as u32;
            (*hwctx).misc_flags = 0;
        }
        let initialized = unsafe { ff::av_hwdevice_ctx_init(hw) };
        if initialized < 0 {
            let mut hw = hw;
            unsafe {
                ff::av_buffer_unref(&mut hw);
                ff::avcodec_free_context(&mut codec);
                ff::avformat_close_input(&mut format);
            }
            return Err(anyhow!(
                "av_hwdevice_ctx_init(D3D11VA) failed: {}",
                ff_error(initialized)
            ));
        }
        unsafe {
            // Ownership of the initialized device ref passes to the codec
            // context; `avcodec_free_context` releases it.
            (*codec).hw_device_ctx = hw;
        }
        let opened = unsafe { ff::avcodec_open2(codec, codec_ptr, ptr::null_mut()) };
        if opened < 0 {
            unsafe {
                ff::avcodec_free_context(&mut codec);
                ff::avformat_close_input(&mut format);
            }
            return Err(anyhow!("avcodec_open2(D3D11VA) failed: {}", ff_error(opened)));
        }
        if std::env::var("O11A1_ZC_DEBUG").ok().as_deref() == Some("1") {
            unsafe {
                log::line(&format!(
                    "zc opened codec={} pix_fmt={:?} sw_pix={:?} hw_frames={} hw_device={}",
                    (*codec).codec_id as i32,
                    (*codec).pix_fmt,
                    (*codec).sw_pix_fmt,
                    u8::from(!(*codec).hw_frames_ctx.is_null()),
                    u8::from(!(*codec).hw_device_ctx.is_null())
                ));
            }
        }

        let packet = unsafe { ff::av_packet_alloc() };
        let frame = unsafe { ff::av_frame_alloc() };
        if packet.is_null() || frame.is_null() {
            unsafe {
                if !packet.is_null() {
                    ff::av_packet_free(&mut { packet });
                }
                if !frame.is_null() {
                    ff::av_frame_free(&mut { frame });
                }
                ff::avcodec_free_context(&mut codec);
                ff::avformat_close_input(&mut format);
            }
            return Err(anyhow!("FFmpeg packet/frame allocation failed"));
        }

        let mut decoder = Self {
            format,
            codec,
            packet,
            frame,
            stream_index,
            time_base,
            origin_ms,
            start_ms,
            fps,
            fallback_ms: start_ms,
            eof: false,
            flushed: false,
        };

        if start_ms > 0 && !live {
            decoder.seek(start_ms)?;
        }
        Ok(decoder)
    }

    fn seek(&mut self, target_ms: u64) -> Result<()> {
        let target = unsafe {
            ff::av_rescale_q(
                target_ms as i64,
                ff::AVRational {
                    num: 1,
                    den: AV_TIME_BASE_Q_DEN as c_int,
                },
                self.time_base,
            )
        };
        let result = unsafe {
            ff::avformat_seek_file(
                self.format,
                self.stream_index,
                i64::MIN,
                target,
                i64::MAX,
                ff::AVSEEK_FLAG_BACKWARD,
            )
        };
        if result < 0 {
            let fallback = unsafe {
                ff::av_seek_frame(
                    self.format,
                    self.stream_index,
                    target,
                    ff::AVSEEK_FLAG_BACKWARD,
                )
            };
            if fallback < 0 {
                return Err(anyhow!(
                    "seek to {target_ms} ms failed: {} / {}",
                    ff_error(result),
                    ff_error(fallback)
                ));
            }
        }
        unsafe { ff::avcodec_flush_buffers(self.codec) };
        self.eof = false;
        self.flushed = false;
        self.fallback_ms = target_ms;
        unsafe { ff::av_packet_unref(self.packet) };
        Ok(())
    }

    fn timestamp_ms(&mut self, frame: *mut ff::AVFrame) -> u64 {
        let pts = unsafe {
            let best = (*frame).best_effort_timestamp;
            let raw = (*frame).pts;
            if best != AV_NOPTS_VALUE {
                best
            } else if raw != AV_NOPTS_VALUE {
                raw
            } else {
                let fallback = self.fallback_ms;
                self.fallback_ms = self.fallback_ms.saturating_add(
                    (1000.0 / self.fps.max(1.0)).round() as u64,
                );
                return fallback;
            }
        };
        let ms = unsafe {
            ff::av_rescale_q(
                pts,
                self.time_base,
                ff::AVRational {
                    num: 1,
                    den: AV_TIME_BASE_Q_DEN as c_int,
                },
            )
        };
        (ms.max(0) as u64).saturating_sub(self.origin_ms)
    }

    /// Next decoded picture, dropping the keyframe tail before the requested
    /// seek target. Returns `None` at EOF or when the session was stopped.
    fn next_frame(&mut self, stop: &AtomicBool) -> Result<Option<(AVFrameRef, u64)>> {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let received = unsafe { ff::avcodec_receive_frame(self.codec, self.frame) };
            if received == 0 {
                let timestamp = self.timestamp_ms(self.frame);
                let frame = AVFrameRef::clone_from(self.frame)?;
                unsafe { ff::av_frame_unref(self.frame) };
                if timestamp.saturating_add(8) < self.start_ms {
                    continue;
                }
                return Ok(Some((frame, timestamp)));
            }
            if received == AVERROR_EOF {
                return Ok(None);
            }
            if received != AVERROR_EAGAIN {
                return Err(anyhow!(
                    "avcodec_receive_frame failed: {}",
                    ff_error(received)
                ));
            }
            if self.eof {
                return Ok(None);
            }
            let read = unsafe { ff::av_read_frame(self.format, self.packet) };
            if read < 0 {
                self.eof = true;
                if !self.flushed {
                    self.flushed = true;
                    unsafe { ff::avcodec_send_packet(self.codec, ptr::null()) };
                }
                continue;
            }
            let is_video = unsafe { (*self.packet).stream_index } == self.stream_index;
            if !is_video {
                unsafe { ff::av_packet_unref(self.packet) };
                continue;
            }
            let sent = unsafe { ff::avcodec_send_packet(self.codec, self.packet) };
            unsafe { ff::av_packet_unref(self.packet) };
            if sent < 0 && sent != AVERROR_EAGAIN {
                return Err(anyhow!(
                    "avcodec_send_packet failed: {}",
                    ff_error(sent)
                ));
            }
        }
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.packet.is_null() {
                ff::av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                ff::av_frame_free(&mut self.frame);
            }
            if !self.codec.is_null() {
                ff::avcodec_free_context(&mut self.codec);
            }
            if !self.format.is_null() {
                ff::avformat_close_input(&mut self.format);
            }
        }
    }
}

unsafe extern "C" fn get_format(
    context: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    if formats.is_null() || context.is_null() {
        return ff::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut index = 0isize;
    loop {
        let format = *formats.offset(index);
        if format == ff::AVPixelFormat::AV_PIX_FMT_NONE {
            return ff::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        if format == ff::AVPixelFormat::AV_PIX_FMT_D3D11 {
            unsafe {
                // AV1 (and some HEVC profiles) expect a frames context rather
                // than deriving one from hw_device_ctx. Build it here while
                // `sw_pix_fmt` is known; H.264/HEVC are happy with the same
                // explicit pool.
                if !(*context).hw_frames_ctx.is_null() {
                    return format;
                }
                if std::env::var("O11A1_ZC_DEBUG").ok().as_deref() == Some("1") {
                    log::line(&format!(
                        "zc get_format codec={} sw={:?} coded={}x{} display={}x{} offered0={:?}",
                        (*context).codec_id as i32,
                        (*context).sw_pix_fmt,
                        (*context).coded_width,
                        (*context).coded_height,
                        (*context).width,
                        (*context).height,
                        *formats
                    ));
                }
                let device = (*context).hw_device_ctx;
                if device.is_null() {
                    return format;
                }
                let sw_format = match (*context).sw_pix_fmt {
                    ff::AVPixelFormat::AV_PIX_FMT_YUV420P
                    | ff::AVPixelFormat::AV_PIX_FMT_NV12 => {
                        ff::AVPixelFormat::AV_PIX_FMT_NV12
                    }
                    ff::AVPixelFormat::AV_PIX_FMT_YUV420P10LE
                    | ff::AVPixelFormat::AV_PIX_FMT_P010LE => {
                        ff::AVPixelFormat::AV_PIX_FMT_P010LE
                    }
                    _ => return format,
                };
                let frames_ref = ff::av_hwframe_ctx_alloc(device);
                if frames_ref.is_null() {
                    return format;
                }
                let frames = (*frames_ref).data as *mut ff::AVHWFramesContext;
                if frames.is_null() {
                    let mut frames_ref = frames_ref;
                    ff::av_buffer_unref(&mut frames_ref);
                    return format;
                }
                let align = if matches!(
                    (*context).codec_id,
                    ff::AVCodecID::AV_CODEC_ID_HEVC | ff::AVCodecID::AV_CODEC_ID_AV1
                ) {
                    128
                } else {
                    16
                };
                let width = (*context).coded_width.max((*context).width).max(2);
                let height = (*context).coded_height.max((*context).height).max(2);
                (*frames).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
                (*frames).sw_format = sw_format;
                (*frames).width = (width + align - 1) / align * align;
                (*frames).height = (height + align - 1) / align * align;
                (*frames).initial_pool_size = 24;
                if ff::av_hwframe_ctx_init(frames_ref) < 0 {
                    let mut frames_ref = frames_ref;
                    ff::av_buffer_unref(&mut frames_ref);
                    return format;
                }
                (*context).hw_frames_ctx = frames_ref;
            }
            return format;
        }
        index += 1;
    }
}

fn codec_supports_d3d11(codec: *const ff::AVCodec) -> bool {
    let mut index = 0;
    loop {
        let config = unsafe { ff::avcodec_get_hw_config(codec, index) };
        if config.is_null() {
            return false;
        }
        unsafe {
            if (*config).device_type == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA
                && ((*config).methods
                    & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int)
                    != 0
            {
                return true;
            }
        }
        index += 1;
    }
}

fn ff_error(code: c_int) -> String {
    let mut buffer = [0i8; 256];
    unsafe {
        ff::av_strerror(code, buffer.as_mut_ptr(), buffer.len());
        CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
    }
}

/// A running in-process decoder. The engine sees only this handle; all FFmpeg
/// resources belong to the reader thread and are freed when it exits.
pub struct ZeroCopySession {
    stop: Arc<AtomicBool>,
    queue: Arc<FrameQueue>,
    reader: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<String>>,
}

impl ZeroCopySession {
    pub fn open(
        url: &str,
        headers: &[(String, String)],
        remote: bool,
        live: bool,
        start_ms: u64,
        fps: f64,
        width: u32,
        height: u32,
        generation: u64,
        queue: Arc<FrameQueue>,
        gate: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        gpu: &Gpu,
    ) -> Result<Self> {
        let mut decoder = NativeDecoder::open(url, headers, remote, live, start_ms, fps, &stop, gpu)
            .with_context(|| format!("open zero-copy decoder for {url}"))?;

        let device = gpu.device.clone();
        let (first, first_ts) = decode_one(&mut decoder, &stop, &device)
            .with_context(|| format!("decode first zero-copy frame at {start_ms} ms"))?
            .ok_or_else(|| anyhow!("zero-copy decoder reached EOF before its first picture"))?;

        log::line(&format!(
            "zerocopy decoder gen={generation} start_ms={start_ms} size={}x{} source={}x{} p010={} live={}",
            first.width,
            first.height,
            width,
            height,
            u8::from(first.p010),
            u8::from(live)
        ));

        let last_error = Arc::new(Mutex::new(String::new()));
        let reader = {
            let queue = queue.clone();
            let gate = gate.clone();
            let stop = stop.clone();
            let last_error = last_error.clone();
            let device = device.clone();
            std::thread::Builder::new()
                .name(format!("player-video-zc-{generation}"))
                .spawn(move || {
                    run_reader(
                        decoder,
                        device,
                        Some((first, first_ts)),
                        queue,
                        gate,
                        stop,
                        last_error,
                        fps,
                        start_ms,
                        generation,
                    )
                })
                .context("spawn zero-copy video reader")?
        };
        Ok(Self {
            stop,
            queue,
            reader: Some(reader),
            last_error,
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake a reader blocked in `FrameQueue::push` before joining it.
        self.queue.close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    pub fn last_error(&self) -> String {
        self.last_error
            .lock()
            .map(|message| message.clone())
            .unwrap_or_default()
    }
}

impl Drop for ZeroCopySession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn decode_one(
    decoder: &mut NativeDecoder,
    stop: &AtomicBool,
    device: &ID3D11Device,
) -> Result<Option<(HWFrame, u64)>> {
    let Some((frame, ts_ms)) = decoder.next_frame(stop)? else {
        return Ok(None);
    };
    let hw = HWFrame::new(frame, device, [0u8; crate::interp::SIG_LEN])?;
    Ok(Some((hw, ts_ms)))
}

fn run_reader(
    mut decoder: NativeDecoder,
    device: ID3D11Device,
    first: Option<(HWFrame, u64)>,
    queue: Arc<FrameQueue>,
    gate: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    last_error: Arc<Mutex<String>>,
    _fps: f64,
    _start_ms: u64,
    generation: u64,
) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut index: u64 = 0;
    let mut first = first;
    let mut window = Instant::now();
    let mut window_frames = 0u64;
    loop {
        if stop.load(Ordering::SeqCst) || queue.is_closed() {
            break;
        }
        let item = if let Some((frame, ts_ms)) = first.take() {
            Some((frame, ts_ms))
        } else {
            match decode_one(&mut decoder, &stop, &device) {
                Ok(frame) => frame,
                Err(error) => {
                    if let Ok(mut slot) = last_error.lock() {
                        *slot = format!("{error:#}");
                    }
                    log::line(&format!("zerocopy decode error gen={generation}: {error:#}"));
                    break;
                }
            }
        };
        let Some((frame, ts_ms)) = item else {
            break;
        };
        queue.push(DecodedFrame {
            data: FrameData::Gpu(Arc::new(frame)),
            ts_ms,
        });
        if index == 0 {
            log::line(&format!(
                "zerocopy first queued gen={generation} ts_ms={ts_ms} queue={} gate={}",
                queue.len(),
                u8::from(gate.load(Ordering::SeqCst))
            ));
        }
        index = index.saturating_add(1);
        window_frames = window_frames.saturating_add(1);
        if window.elapsed() >= Duration::from_secs(1) {
            log::line(&format!(
                "zerocopy read fps={:.1} frames={index} queue={}",
                window_frames as f64 / window.elapsed().as_secs_f64(),
                queue.len()
            ));
            window = Instant::now();
            window_frames = 0;
        }
        while !gate.load(Ordering::SeqCst) {
            if stop.load(Ordering::SeqCst) {
                queue.close();
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    queue.close();
}

/// The decoder's own timestamps run on the media timeline. `decode_one` already
/// filtered the seek's keyframe tail, so this is only a fallback for a stream
/// that omits `pts` entirely (rare; DASH always carries it).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_d3d11_context_layout_is_stable() {
        assert_eq!(std::mem::size_of::<D3D11VADeviceContext>(), 64);
        assert_eq!(
            std::mem::offset_of!(D3D11VADeviceContext, bind_flags),
            56
        );
        assert_eq!(
            std::mem::offset_of!(D3D11VADeviceContext, misc_flags),
            60
        );
    }

    #[test]
    fn error_helpers_do_not_panic() {
        assert!(!ff_error(-1).is_empty());
        assert_eq!(AVERROR_EOF, -541_478_725);
    }
}
