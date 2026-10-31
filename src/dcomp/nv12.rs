//! f209: an NV12 decoder surface on the GPU, as a picture the sink can draw.
//!
//! # What this file is
//!
//! `mediadec::mf` hands back one array slice of the decoder's NV12 surface per
//! frame and nothing else - no CPU copy exists anywhere in that path. Two things
//! then have to happen on the GPU, and both live here:
//!
//! * **YUV -> RGB.** A D3D11 resource cannot be shader-sampled as YUV: the
//!   `CreateShaderResourceView` probes in `src/qa/mf.rs` (`nv12_probe`) ask for
//!   the luma plane (`R8_UNORM`), the chroma plane (`R8G8_UNORM`) and the raw
//!   `NV12` view, as a plain 2D view and as a single-slice array view - every
//!   one of them comes back `E_INVALIDARG` on this driver. `PlaneSlice` is a
//!   D3D12 SRV field and has no D3D11 equivalent, so the conversion goes
//!   through the unit Microsoft ships for exactly this job: `ID3D11VideoDevice`
//!   / `ID3D11VideoProcessor`, with the stream's own colour space
//!   (`YCbCr studio BT.709 -> RGB full BT.709`). The video processor *is* the
//!   hardware YUV->RGB conversion - it reads the decoder's slice in place (an
//!   input view names the array slice), so no CPU byte is involved either.
//!
//! * **A picture the warp/enhance chain can sample.** The blit writes into a
//!   persistent `RGBA8`/`BGRA8` render target, and that texture's SRV is what
//!   `warp.rs` and `enhance.rs` already know how to read.
//!
//! # Why the array slice matters
//!
//! The decoder keeps ~15 surfaces in one texture array and rotates through
//! them; `MfFrame::subresource` is the slice. The input view is built per frame
//! from that index (`Texture2DArray { ArraySlice: subresource }`), which is the
//! one thing a caller gets wrong by sampling slice 0.
//!
//! # Cost
//!
//! At 1920x1080 one blit measured ~0.2-0.4 ms of GPU time in the f209 QA run,
//! and it happens once per *source* frame (30/s), not once per display slot.

use std::collections::HashMap;

use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, ID3D11VertexShader,
    D3D11_TEXTURE2D_DESC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

/// One decoded picture that never left the GPU.
///
/// `Send` is asserted for the hand-off between the MF reader (where the frame is
/// produced) and the sink thread (where it is converted and drawn): a D3D11
/// texture is a free-threaded COM object, and the sink's device is the one the
/// decoder wrote it with, so moving the handle is sound. Nothing here touches
/// the texture's contents from the CPU.
pub(crate) struct GpuPicture {
    pub texture: ID3D11Texture2D,
    /// The array slice MF put this picture in (`0..ArraySize`).
    pub subresource: u32,
    /// Visible picture size (`MF_MT_FRAME_SIZE`).
    pub width: u32,
    pub height: u32,
    /// The surface's own size: macroblock padded (1920x1088 for 1080p). Used
    /// for the NV12 plane geometry, never for the aspect ratio.
    pub tex_width: u32,
    pub tex_height: u32,
    /// Presentation time, ms since the stream's start.
    pub pts_ms: u64,
}

unsafe impl Send for GpuPicture {}

impl GpuPicture {
    pub(crate) fn from_mf(frame: crate::mediadec::mf::MfFrame) -> Self {
        Self {
            texture: frame.texture,
            subresource: frame.subresource,
            width: frame.width.max(1),
            height: frame.height.max(1),
            tex_width: frame.texture_width.max(1),
            tex_height: frame.texture_height.max(1),
            pts_ms: frame.pts.as_millis().min(u64::MAX as u128) as u64,
        }
    }

    pub(crate) fn clone_picture(&self) -> Self {
        Self {
            texture: self.texture.clone(),
            subresource: self.subresource,
            width: self.width,
            height: self.height,
            tex_width: self.tex_width,
            tex_height: self.tex_height,
            pts_ms: self.pts_ms,
        }
    }
}

/// How one source frame becomes a shader-readable picture.
///
/// Two textures (A/B) so a warp can read both sides of a pair at once. The
/// output format is the one the caller asks for: the optical-flow session
/// accepts `RGBA8` or `BGRA8` depending on the driver, and both are valid video
/// processor outputs when the enumerator says so.
pub(crate) struct Nv12Converter {
    device: ID3D11Device,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    video_context1: Option<ID3D11VideoContext1>,
    enumerator: Option<ID3D11VideoProcessorEnumerator>,
    processor: Option<ID3D11VideoProcessor>,
    format: DXGI_FORMAT,
    /// Output size (`width`, `height` of the source), i.e. what the sink draws.
    size: (u32, u32),
    slots: Vec<Converted>,
    /// Input views, cached by `(texture pointer, subresource)`: creating one per
    /// frame cost more than the blit on this driver. A view holds a reference to
    /// its texture, so the cache is bounded: it is cleared whenever the count
    /// would exceed the surface pool, because holding *all* of the decoder's
    /// slices would stall the decoder itself.
    views: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
    /// Blits issued (QA).
    blits: u64,
    /// Last blit's wall time, µs (queue time included: the GPU work is ordered
    /// behind whatever the ring is doing).
    last_us: u64,
}

struct Converted {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
}

impl Nv12Converter {
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        format: DXGI_FORMAT,
    ) -> Result<Self, String> {
        let video: ID3D11VideoDevice = device
            .cast()
            .map_err(|error| format!("ID3D11VideoDevice 不可用: {error}"))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|error| format!("ID3D11VideoContext 不可用: {error}"))?;
        let video_context1 = context.cast::<ID3D11VideoContext1>().ok();
        Ok(Self {
            device: device.clone(),
            video,
            video_context,
            video_context1,
            enumerator: None,
            processor: None,
            format,
            size: (0, 0),
            slots: Vec::new(),
            views: HashMap::new(),
            blits: 0,
            last_us: 0,
        })
    }

    /// (Re)build the processor and the output textures for `width`x`height`.
    ///
    /// The enumerator is created for the *visible* size: the decoder's surface
    /// is padded (1080 becomes 1088 rows), and a source rect of the visible part
    /// is what keeps the padding out of the picture.
    pub(crate) fn ensure(&mut self, width: u32, height: u32) -> Result<(), String> {
        let size = (width.max(2), height.max(2));
        if self.size == size && self.processor.is_some() {
            return Ok(());
        }
        self.views.clear();
        self.slots.clear();
        self.processor = None;
        self.enumerator = None;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: size.0,
            InputHeight: size.1,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: size.0,
            OutputHeight: size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { self.video.CreateVideoProcessorEnumerator(&desc) }
            .map_err(|error| format!("CreateVideoProcessorEnumerator({}x{}) 失败: {error}", size.0, size.1))?;
        let input = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }.unwrap_or(0);
        if input & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
            return Err("视频处理器不接受 NV12 输入".to_string());
        }
        let wants = self.format;
        let supported = unsafe { enumerator.CheckVideoProcessorFormat(wants) }.unwrap_or(0);
        if supported & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            // RGBA8 is not a required output format; fall back to BGRA and let
            // the caller swizzle when it needs RGBA.
            self.format = DXGI_FORMAT_B8G8R8A8_UNORM;
            let bgra = unsafe { enumerator.CheckVideoProcessorFormat(self.format) }.unwrap_or(0);
            if bgra & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                return Err("视频处理器不接受 BGRA 输出".to_string());
            }
            crate::debug_log(&format!(
                "nv12 convert output fallback wants={} using=BGRA",
                wants.0
            ));
        }
        let processor = unsafe { self.video.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|error| format!("CreateVideoProcessor 失败: {error}"))?;
        // One input stream, progressive, with the stream's own colour space.
        unsafe {
            self.video_context
                .VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            let src = RECT {
                left: 0,
                top: 0,
                right: size.0 as i32,
                bottom: size.1 as i32,
            };
            self.video_context
                .VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            self.video_context
                .VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&src));
            self.video_context
                .VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }
        // B 站 DASH is BT.709 limited range ("studio swing") in both directions
        // of the conversion: the stream is YCbCr studio BT.709, the compositor
        // wants RGB full BT.709. Set both explicitly - the driver's default
        // depends on the media type it was handed, which for a DXVA surface is
        // not the stream's own metadata.
        let mut colour_space = "explicit";
        if let Some(context1) = self.video_context1.as_ref() {
            unsafe {
                context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
                context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
            }
        } else {
            colour_space = "bitfield";
            // D3D11_VIDEO_PROCESSOR_COLOR_SPACE bitfield: Usage=0 (playback),
            // RGB_Range=0 (full, 0-255), YCbCr_Matrix=1 (BT.709),
            // YCbCr_xvYCC=0, Nominal_Range=1 (16-235).
            let space = windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            };
            unsafe {
                self.video_context
                    .VideoProcessorSetStreamColorSpace(&processor, 0, &space);
                self.video_context
                    .VideoProcessorSetOutputColorSpace(&processor, &space);
            }
        }
        for _ in 0..2 {
            self.slots.push(Self::make_slot(
                &self.device,
                self.format,
                size.0,
                size.1,
            )?);
        }
        crate::debug_log(&format!(
            "nv12 convert ready size={}x{} format={} colorspace={colour_space}",
            size.0, size.1, self.format.0
        ));
        self.size = size;
        self.enumerator = Some(enumerator);
        self.processor = Some(processor);
        Ok(())
    }

    fn make_slot(
        device: &ID3D11Device,
        format: DXGI_FORMAT,
        width: u32,
        height: u32,
    ) -> Result<Converted, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|error| format!("CreateTexture2D(convert) 失败: {error}"))?;
        let texture = texture.ok_or_else(|| "CreateTexture2D 返回空纹理".to_string())?;
        let mut view: Option<ID3D11ShaderResourceView> = None;
        unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut view)) }
            .map_err(|error| format!("CreateShaderResourceView(convert) 失败: {error}"))?;
        let view = view.ok_or_else(|| "CreateShaderResourceView 返回空视图".to_string())?;
        Ok(Converted { texture, view })
    }

    pub(crate) fn format(&self) -> DXGI_FORMAT {
        self.format
    }

    /// The picture in slot `index` (0 = A, 1 = B) after [`Self::convert`].
    pub(crate) fn view(&self, index: usize) -> Option<&ID3D11ShaderResourceView> {
        self.slots.get(index).map(|slot| &slot.view)
    }

    pub(crate) fn texture(&self, index: usize) -> Option<&ID3D11Texture2D> {
        self.slots.get(index).map(|slot| &slot.texture)
    }

    /// Convert `picture` into slot `index`. No CPU pixel is read or written.
    pub(crate) fn convert(&mut self, index: usize, picture: &GpuPicture) -> Result<(), String> {
        let started = std::time::Instant::now();
        let (Some(processor), Some(enumerator)) = (self.processor.as_ref(), self.enumerator.as_ref())
        else {
            return Err("转换器未初始化".to_string());
        };
        let slot = self
            .slots
            .get(index)
            .ok_or_else(|| "转换槽不存在".to_string())?;

        // A P010 surface (10-bit HEVC) is not NV12: the same processor handles
        // it, but the format has to be named correctly.
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { picture.texture.GetDesc(&mut desc) };
        let input_format = if desc.Format == DXGI_FORMAT_P010 {
            DXGI_FORMAT_P010
        } else if desc.Format == DXGI_FORMAT_NV12 {
            DXGI_FORMAT_NV12
        } else {
            return Err(format!("转换器收到非 NV12 纹理 dxgi={}", desc.Format.0));
        };

        // The input view names the decoder's array slice.
        let key = (Interface::as_raw(&picture.texture) as usize, picture.subresource);
        let input_view = match self.views.get(&key) {
            Some(view) => view.clone(),
            None => {
                let view = self.create_input_view(enumerator, picture, input_format)?;
                // A view pins its texture. The decoder rotates through ~15
                // slices, so caching more than that would pin the whole pool -
                // which is precisely how a decoder is stalled.
                if self.views.len() >= 8 {
                    self.views.clear();
                }
                self.views.insert(key, view.clone());
                view
            }
        };
        let output: ID3D11VideoProcessorOutputView = {
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPOV {
                        MipSlice: 0,
                    },
                },
            };
            let mut view: Option<ID3D11VideoProcessorOutputView> = None;
            unsafe {
                self.video.CreateVideoProcessorOutputView(
                    &slot.texture,
                    enumerator,
                    &out_desc,
                    Some(&mut view),
                )
            }
            .map_err(|error| format!("CreateVideoProcessorOutputView 失败: {error}"))?;
            view.ok_or_else(|| "输出视图为空".to_string())?
        };
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        }];
        unsafe { self.video_context.VideoProcessorBlt(processor, &output, 0, &mut streams) }
            .map_err(|error| format!("VideoProcessorBlt 失败: {error}"))?;
        self.blits = self.blits.saturating_add(1);
        self.last_us = started.elapsed().as_micros() as u64;
        if self.blits % 300 == 1 {
            crate::debug_log(&format!(
                "nv12 convert blits={} last_us={} size={}x{}",
                self.blits, self.last_us, self.size.0, self.size.1
            ));
        }
        Ok(())
    }

    fn create_input_view(
        &self,
        enumerator: &ID3D11VideoProcessorEnumerator,
        picture: &GpuPicture,
        _format: DXGI_FORMAT,
    ) -> Result<ID3D11VideoProcessorInputView, String> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: picture.subresource,
                },
            },
        };
        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            self.video.CreateVideoProcessorInputView(
                &picture.texture,
                enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .map_err(|error| format!("CreateVideoProcessorInputView 失败: {error}"))?;
        view.ok_or_else(|| "输入视图为空".to_string())
    }
}

/// The one draw the sink needs when it is *not* interpolating: copy one
/// converted picture into the backbuffer (or into a tier's target).
///
/// It is the same fullscreen triangle `warp.rs` uses, with a plain sample: the
/// warp needs a flow field, and a session with 插帧 off has none.
pub(crate) struct BlitPipeline {
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
}

const BLIT: &str = r#"
Texture2D<float4> tex_src : register(t0);

struct Varyings
{
    float4 pos : SV_Position;
};

Varyings vs_main(uint id : SV_VertexID)
{
    float2 uv = float2((id << 1) & 2, id & 2);
    Varyings o;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float4 ps_main(Varyings input) : SV_Target
{
    // Point load at the pixel's own coordinate: the picture is already at the
    // layer's size, so this is a 1:1 copy - a filter would only blur it.
    int2 p = int2(input.pos.xy);
    float4 c = tex_src.Load(int3(p, 0));
    c.a = 1.0;
    return c;
}
"#;

impl BlitPipeline {
    pub(crate) fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self, String> {
        let vs_code = super::warp::compile(BLIT, "vs_main", "vs_5_0")?;
        let ps_code = super::warp::compile(BLIT, "ps_main", "ps_5_0")?;
        unsafe {
            let mut vs: Option<ID3D11VertexShader> = None;
            device
                .CreateVertexShader(vs_code.as_slice(), None, Some(&mut vs))
                .map_err(|error| format!("CreateVertexShader(blit) 失败: {error}"))?;
            let mut ps: Option<ID3D11PixelShader> = None;
            device
                .CreatePixelShader(ps_code.as_slice(), None, Some(&mut ps))
                .map_err(|error| format!("CreatePixelShader(blit) 失败: {error}"))?;
            Ok(Self {
                context: context.clone(),
                vs: vs.ok_or_else(|| "空顶点着色器".to_string())?,
                ps: ps.ok_or_else(|| "空像素着色器".to_string())?,
            })
        }
    }

    pub(crate) fn draw_into(
        &self,
        rtv: &ID3D11RenderTargetView,
        source: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.OMSetRenderTargets(
                Some(&[Some(rtv.clone())]),
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
            );
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context
                .IASetInputLayout(None::<&windows::Win32::Graphics::Direct3D11::ID3D11InputLayout>);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetShaderResources(0, Some(&[Some(source.clone())]));
            self.context.Draw(3, 0);
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(
                None,
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
            );
        }
        Ok(())
    }
}

/// RGBA8/BGRA8 as a name, for the log lines.
pub(crate) fn format_name(format: DXGI_FORMAT) -> &'static str {
    if format == DXGI_FORMAT_R8G8B8A8_UNORM {
        "RGBA8"
    } else if format == DXGI_FORMAT_B8G8R8A8_UNORM {
        "BGRA8"
    } else {
        "other"
    }
}
