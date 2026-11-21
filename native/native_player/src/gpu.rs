//! D3D11 device, the two composition swapchains and the DirectComposition tree.
//!
//! The same recipe the f192 prototype proved on this machine:
//! `WS_EX_NOREDIRECTIONBITMAP` window, `DCompositionCreateDevice` +
//! `CreateTargetForHwnd`, a video swapchain (`DXGI_ALPHA_MODE_IGNORE`,
//! `FLIP_SEQUENTIAL`, 2 buffers, frame-latency waitable) below a UI swapchain
//! (`DXGI_ALPHA_MODE_PREMULTIPLIED`).

use std::ffi::c_void;

use anyhow::{anyhow, Result};
use windows::core::{IUnknown, Interface, BOOL};
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11ClassLinkage, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE, DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1, IDXGISwapChain2,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::{WaitForSingleObject, THREAD_PRIORITY_HIGHEST};
use windows_numerics::Matrix3x2;

use crate::enhance::{EnhanceMode, EnhancePipeline, VideoPasses};
use crate::interp::{self, Signature, SIG_COLS, SIG_LEN, SIG_ROWS};
use crate::log;
use crate::zerocopy::HWFrame;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub factory: IDXGIFactory2,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
            let mut result = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            );
            if result.is_err() {
                // VIDEO_SUPPORT is only needed by the in-process decoder. Keep
                // the window and the external fallback working on drivers that
                // reject the flag.
                device = None;
                context = None;
                result = D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    Default::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut level),
                    Some(&mut context),
                );
            }
            result?;
            let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
            let context =
                context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
            // libavcodec's D3D11VA decoder runs on its reader thread while the
            // pacer keeps submitting video/UI work on the window thread. The
            // D3D10 multithread interface serialises the immediate context for
            // exactly this case (FFmpeg's own device creation enables it too).
            match device.cast::<ID3D10Multithread>() {
                Ok(multithread) => {
                    let _ = multithread.SetMultithreadProtected(true);
                }
                Err(error) => log::line(&format!(
                    "D3D11 multithread protection unavailable: {error}"
                )),
            }
            // The pacer does one GPU submission per output tick; highest
            // priority keeps it ahead of ordinary UI work.
            let _ = windows::Win32::System::Threading::SetThreadPriority(
                windows::Win32::System::Threading::GetCurrentThread(),
                THREAD_PRIORITY_HIGHEST,
            );
            Ok(Self {
                device,
                context,
                factory,
            })
        }
    }

    pub fn create_swapchain(
        &self,
        width: u32,
        height: u32,
        alpha: DXGI_ALPHA_MODE,
        waitable: bool,
    ) -> Result<(IDXGISwapChain1, isize)> {
        unsafe {
            let description = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
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
                Flags: if waitable {
                    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32
                } else {
                    0
                },
            };
            let unknown: IUnknown = self.device.cast()?;
            let swap = self.factory.CreateSwapChainForComposition(
                &unknown,
                &description,
                None::<&IDXGIOutput>,
            )?;
            let mut waiter = 0isize;
            if waitable {
                let swap2: IDXGISwapChain2 = swap.cast()?;
                swap2.SetMaximumFrameLatency(1)?;
                waiter = swap2.GetFrameLatencyWaitableObject().0 as isize;
            }
            Ok((swap, waiter))
        }
    }
}

/// 32×18 luma probe for scene-cut detection on hardware frames. The full
/// picture never leaves the GPU; one tiny pass writes the sampled grid to an R8
/// target, a 576-byte staging readback moves it to the CPU.
pub struct SignatureProbe {
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    dims: ID3D11Buffer,
    target: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    staging: ID3D11Texture2D,
}

impl SignatureProbe {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self> {
        let source = include_str!("shaders/enhance.hlsl");
        let vs_code = crate::enhance::compile(source, "vs_main", "vs_5_0")?;
        let vertex = unsafe {
            let mut shader: Option<ID3D11VertexShader> = None;
            device
                .CreateVertexShader(&vs_code, None::<&ID3D11ClassLinkage>, Some(&mut shader))
                .map_err(|error| anyhow!("signature vertex shader: {error}"))?;
            shader.ok_or_else(|| anyhow!("signature vertex shader missing"))?
        };
        let pixel = crate::enhance::pixel_shader(device, source, "sig_main")?;
        let initial = [0f32; 8];
        let dims = unsafe {
            let description = D3D11_BUFFER_DESC {
                ByteWidth: 32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: initial.as_ptr() as *const c_void,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut buffer: Option<ID3D11Buffer> = None;
            device
                .CreateBuffer(&description, Some(&data), Some(&mut buffer))
                .map_err(|error| anyhow!("signature constant buffer: {error}"))?;
            buffer.ok_or_else(|| anyhow!("signature constant buffer missing"))?
        };
        let target_description = D3D11_TEXTURE2D_DESC {
            Width: SIG_COLS as u32,
            Height: SIG_ROWS as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let target = unsafe {
            let mut texture: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&target_description, None, Some(&mut texture))
                .map_err(|error| anyhow!("signature target: {error}"))?;
            texture.ok_or_else(|| anyhow!("signature target missing"))?
        };
        let rtv = unsafe {
            let mut view: Option<ID3D11RenderTargetView> = None;
            device
                .CreateRenderTargetView(&target, None, Some(&mut view))
                .map_err(|error| anyhow!("signature RTV: {error}"))?;
            view.ok_or_else(|| anyhow!("signature RTV missing"))?
        };
        let staging = unsafe {
            let description = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..target_description
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&description, None, Some(&mut texture))
                .map_err(|error| anyhow!("signature staging: {error}"))?;
            texture.ok_or_else(|| anyhow!("signature staging missing"))?
        };
        Ok(Self {
            context: context.clone(),
            vertex,
            pixel,
            dims,
            target,
            rtv,
            staging,
        })
    }

    pub fn measure(
        &self,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
    ) -> Result<Signature> {
        if width < 2 || height < 2 {
            return Ok([0u8; SIG_LEN]);
        }
        let dims = [
            width.saturating_sub(1) as f32,
            height.saturating_sub(1) as f32,
            width as f32,
            height as f32,
            0.0,
            1.0,
            0.0,
            0.0,
        ];
        unsafe {
            self.context.UpdateSubresource(
                &self.dims,
                0,
                None,
                dims.as_ptr() as *const c_void,
                0,
                0,
            );
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: SIG_COLS as f32,
                Height: SIG_ROWS as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(None);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context
                .VSSetConstantBuffers(0, Some(&[Some(self.dims.clone())]));
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.dims.clone())]));
            self.context
                .PSSetShaderResources(0, Some(&[Some(luma.clone())]));
            self.context.Draw(3, 0);
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(None, None);
            self.context.CopyResource(&self.staging, &self.target);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|error| anyhow!("map signature staging: {error}"))?;
            let mut signature = [0u8; SIG_LEN];
            let start = mapped.pData as *const u8;
            for row in 0..SIG_ROWS {
                let source = start.add(row * mapped.RowPitch as usize);
                for col in 0..SIG_COLS {
                    signature[row * SIG_COLS + col] = *source.add(col);
                }
            }
            self.context.Unmap(&self.staging, 0);
            Ok(signature)
        }
    }
}

/// How the decoded picture is fitted into the client area. The decode size
/// never changes; only the DirectComposition transform and the UI bars do.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AspectMode {
    /// Whole picture visible, letterboxed (the historical behaviour).
    #[default]
    Fit = 0,
    /// Fill the client, cropping the overflow while preserving the aspect.
    Fill = 1,
    /// Fill the client on both axes, ignoring the source aspect.
    Stretch = 2,
}

impl AspectMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Fill,
            2 => Self::Stretch,
            _ => Self::Fit,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "适应",
            Self::Fill => "填充",
            Self::Stretch => "拉伸",
        }
    }

    /// Cycle 适应 → 填充 → 拉伸 → 适应.
    pub fn next(self) -> Self {
        match self {
            Self::Fit => Self::Fill,
            Self::Fill => Self::Stretch,
            Self::Stretch => Self::Fit,
        }
    }
}

/// One decoded picture living entirely on the GPU: the NV12 planes stay as two
/// shader-readable textures so a 4K frame costs a 1.5 bytes/pixel upload
/// instead of the old 4 bytes/pixel BGRA conversion, and the BGRA picture the
/// enhance/blend passes consume is produced by `nv12_main`.
#[derive(Clone, Copy, Debug)]
pub struct GpuFrame {
    pub slot: usize,
    pub ts_ms: u64,
    /// Coarse luma fingerprint for scene-cut detection (no readback).
    pub signature: Signature,
}

struct Plane {
    y: ID3D11Texture2D,
    uv: ID3D11Texture2D,
    y_srv: ID3D11ShaderResourceView,
    uv_srv: ID3D11ShaderResourceView,
}

struct Picture {
    /// Kept alive by `srv`/`rtv`; held so the pool owns the resource.
    _tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    rtv: ID3D11RenderTargetView,
}

/// A small ring of decoded pictures plus the GPU passes that turn them into
/// presentation-ready textures. The engine keeps at most two live frames
/// (`cur` and `next`), so four slots make a dropped frame's slot reusable
/// without ever recycling a picture the pacer still needs.
const FRAME_SLOTS: usize = 4;

struct FramePool {
    width: u32,
    height: u32,
    planes: Vec<Plane>,
    pictures: Vec<Picture>,
    /// Result of the 插帧 blend for the current tick.
    mix: Picture,
    /// 32×18 luma probe for hardware frames. Created once; a failure only
    /// disables cut detection, never playback.
    signature: Option<SignatureProbe>,
    next: usize,
    passes: VideoPasses,
}

impl FramePool {
    fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        let mut planes = Vec::with_capacity(FRAME_SLOTS);
        let mut pictures = Vec::with_capacity(FRAME_SLOTS);
        for _ in 0..FRAME_SLOTS {
            planes.push(Plane::new(&gpu.device, width, height)?);
            pictures.push(Picture::new(&gpu.device, width, height)?);
        }
        let mix = Picture::new(&gpu.device, width, height)?;
        let passes = VideoPasses::new(&gpu.device, &gpu.context, width, height)?;
        let signature = match SignatureProbe::new(&gpu.device, &gpu.context) {
            Ok(probe) => Some(probe),
            Err(error) => {
                log::line(&format!("signature probe unavailable: {error:#}"));
                None
            }
        };
        log::line(&format!(
            "video frames slots={FRAME_SLOTS} size={width}x{height} nv12=R8+R8G8 picture=BGRA"
        ));
        Ok(Self {
            width,
            height,
            planes,
            pictures,
            mix,
            signature,
            next: 0,
            passes,
        })
    }

    /// Convert a D3D11VA picture into the BGRA frame ring. The decoder's NV12
    /// or P010 texture is sampled directly; there is no `UpdateSubresource`.
    fn upload_hw(&mut self, hw: &HWFrame, ts_ms: u64) -> Result<GpuFrame> {
        let slot = self.next;
        self.next = (self.next + 1) % self.pictures.len();
        self.passes.convert_hw(
            &hw.y_srv,
            &hw.uv_srv,
            hw.p010,
            &self.pictures[slot].rtv,
        );
        let signature = self
            .signature
            .as_ref()
            .and_then(|probe| probe.measure(&hw.y_srv, hw.width, hw.height).ok())
            .unwrap_or(hw.signature);
        Ok(GpuFrame {
            slot,
            ts_ms,
            signature,
        })
    }

    /// Upload one packed NV12 frame and convert it to BGRA on the GPU.
    fn upload(&mut self, gpu: &Gpu, nv12: &[u8], ts_ms: u64) -> Result<GpuFrame> {
        let width = self.width as usize;
        let height = self.height as usize;
        let needed = width * height * 3 / 2;
        if nv12.len() < needed {
            return Err(anyhow!(
                "video frame {} bytes, need {needed} for {width}x{height} NV12",
                nv12.len()
            ));
        }
        let slot = self.next;
        self.next = (self.next + 1) % self.planes.len();
        let plane = &self.planes[slot];
        upload_plane(&gpu.context, &plane.y, &nv12[..width * height], width, height)?;
        upload_plane(
            &gpu.context,
            &plane.uv,
            &nv12[width * height..],
            width,
            height / 2,
        )?;
        self.passes
            .convert_nv12(&plane.y_srv, &plane.uv_srv, &self.pictures[slot].rtv);
        Ok(GpuFrame {
            slot,
            ts_ms,
            signature: interp::luma_signature(nv12, width, height),
        })
    }
}

impl Plane {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let y = plane_texture(device, width, height, DXGI_FORMAT_R8_UNORM)?;
        let uv = plane_texture(device, width / 2, height / 2, DXGI_FORMAT_R8G8_UNORM)?;
        let y_srv = shader_view(device, &y)?;
        let uv_srv = shader_view(device, &uv)?;
        Ok(Self {
            y,
            uv,
            y_srv,
            uv_srv,
        })
    }
}

impl Picture {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let tex = unsafe {
            let description = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
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
            device.CreateTexture2D(&description, None, Some(&mut texture))?;
            texture.ok_or_else(|| anyhow!("video picture texture missing"))?
        };
        let srv = shader_view(device, &tex)?;
        let rtv = unsafe {
            let mut view: Option<ID3D11RenderTargetView> = None;
            device.CreateRenderTargetView(&tex, None, Some(&mut view))?;
            view.ok_or_else(|| anyhow!("video picture RTV missing"))?
        };
        Ok(Self {
            _tex: tex,
            srv,
            rtv,
        })
    }
}

/// A plane texture the decoder's bytes are uploaded into.
fn plane_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    unsafe {
        let description = D3D11_TEXTURE2D_DESC {
            Width: width.max(1),
            Height: height.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&description, None, Some(&mut texture))
            .map_err(|error| anyhow!("CreateTexture2D {format:?}: {error}"))?;
        texture.ok_or_else(|| anyhow!("CreateTexture2D {format:?}: no texture"))
    }
}

fn shader_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView> {
    unsafe {
        let mut view: Option<ID3D11ShaderResourceView> = None;
        device
            .CreateShaderResourceView(texture, None, Some(&mut view))
            .map_err(|error| anyhow!("CreateShaderResourceView: {error}"))?;
        view.ok_or_else(|| anyhow!("CreateShaderResourceView: no view"))
    }
}

/// Upload `rows` rows of `pitch` bytes into a plane. `UpdateSubresource` lets
/// the driver schedule the copy; the previous Map + row-by-row loop cost more
/// CPU time than the actual decode at 4K.
fn upload_plane(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    source: &[u8],
    pitch: usize,
    rows: usize,
) -> Result<()> {
    if source.len() < pitch.saturating_mul(rows) {
        return Err(anyhow!("plane upload source too short"));
    }
    unsafe {
        context.UpdateSubresource(
            texture,
            0,
            None,
            source.as_ptr() as *const c_void,
            pitch as u32,
            0,
        );
    }
    Ok(())
}

/// The video layer: composition swapchain plus a GPU frame pool. Frames are
/// always the decoded picture size; DirectComposition scales them into the
/// window and the UI/danmaku layers sit above.
pub struct VideoSurface {
    pub swap: IDXGISwapChain1,
    pub waiter: isize,
    /// Frame-latency wait timeout. The normal path tolerates a slow compositor
    /// for 200 ms; during a window drag/resize it must not block the message
    /// pump, so the engine switches to a 2 ms wait.
    wait_ms: u32,
    /// Decoded picture size (the pool's size). Public for diagnostics.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    pub presents: u64,
    failures: u64,
    pool: FramePool,
}

impl VideoSurface {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        let (swap, waiter) = gpu.create_swapchain(width, height, DXGI_ALPHA_MODE_IGNORE, true)?;
        let pool = FramePool::new(gpu, width, height)?;
        log::line(&format!(
            "video surface swapchain={width}x{height} flip-seq alpha=ignore waitable=1 gpu=1"
        ));
        Ok(Self {
            swap,
            waiter,
            wait_ms: 200,
            width,
            height,
            presents: 0,
            failures: 0,
            pool,
        })
    }

    /// Interaction fast path. A window move/resize must never stop the player
    /// thread on the compositor: wait at most 2 ms for a free frame and allow
    /// two frames in flight. Restore the normal 200 ms / one-frame policy
    /// after the gesture.
    pub fn set_interaction(&mut self, active: bool) {
        self.wait_ms = if active { 2 } else { 200 };
        if self.waiter == 0 {
            return;
        }
        let latency = if active { 2 } else { 1 };
        if let Ok(swap2) = self.swap.cast::<IDXGISwapChain2>() {
            if let Err(error) = unsafe { swap2.SetMaximumFrameLatency(latency) } {
                log::line(&format!(
                    "video frame latency {latency} failed: {error:#}"
                ));
            }
        }
    }

    /// Upload a decoded NV12 picture and convert it to BGRA on the GPU. The
    /// returned handle is a slot in the ring; the engine keeps at most two, so
    /// no release bookkeeping is needed.
    pub fn upload(&mut self, gpu: &Gpu, nv12: &[u8], ts_ms: u64) -> Result<GpuFrame> {
        self.pool.upload(gpu, nv12, ts_ms)
    }

    /// Present a D3D11VA picture without a CPU download. The picture is
    /// converted by the same shader ring the external pipe feeds.
    pub fn upload_hw(&mut self, hw: &HWFrame, ts_ms: u64) -> Result<GpuFrame> {
        self.pool.upload_hw(hw, ts_ms)
    }

    pub fn wait_frame(&self) -> bool {
        if self.waiter == 0 {
            return false;
        }
        let result =
            unsafe { WaitForSingleObject(HANDLE(self.waiter as *mut c_void), self.wait_ms) };
        result == WAIT_OBJECT_0
    }

    /// Present one decoded picture. Returns `(enhanced, presented)`.
    pub fn present_frame(
        &mut self,
        gpu: &Gpu,
        enhance: &mut Option<EnhancePipeline>,
        mode: &mut EnhanceMode,
        frame: &GpuFrame,
        brightness: f32,
    ) -> Result<(bool, bool)> {
        let source = self.pool.pictures[frame.slot].srv.clone();
        self.present_source(gpu, enhance, mode, &source, brightness)
    }

    /// Blend two decoded pictures on the GPU (插帧) and present the result.
    /// `weight` is the weight of `b` in `0..=256`.
    pub fn present_blend(
        &mut self,
        gpu: &Gpu,
        enhance: &mut Option<EnhancePipeline>,
        mode: &mut EnhanceMode,
        a: &GpuFrame,
        b: &GpuFrame,
        weight: u16,
        brightness: f32,
    ) -> Result<(bool, bool)> {
        let first = self.pool.pictures[a.slot].srv.clone();
        let second = self.pool.pictures[b.slot].srv.clone();
        let weight = weight.min(256) as f32 / 256.0;
        // The common path (画质增强 关闭, brightness 1.0) can blend straight
        // into the backbuffer: one 4K pass per output tick instead of a blend
        // plus a copy, which matters on the 120 Hz grid.
        if *mode == EnhanceMode::Off && (brightness - 1.0).abs() <= 0.0005 {
            let signaled = self.wait_frame();
            let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
            let mut view: Option<ID3D11RenderTargetView> = None;
            unsafe {
                gpu.device
                    .CreateRenderTargetView(&back, None, Some(&mut view))?;
            }
            let view = view.ok_or_else(|| anyhow!("no backbuffer RTV"))?;
            self.pool
                .passes
                .blend(&first, &second, weight, &view);
            unsafe {
                gpu.context.OMSetRenderTargets(None, None);
            }
            drop(view);
            drop(back);
            let presented = self.present_raw(signaled)?;
            return Ok((false, presented));
        }
        self.pool
            .passes
            .blend(&first, &second, weight, &self.pool.mix.rtv);
        let source = self.pool.mix.srv.clone();
        self.present_source(gpu, enhance, mode, &source, brightness)
    }

    /// `brightness` is applied by the video pixel shader, after the optional
    /// enhance chain, so the UI and danmaku layers are never affected.
    fn present_source(
        &mut self,
        gpu: &Gpu,
        enhance: &mut Option<EnhancePipeline>,
        mode: &mut EnhanceMode,
        source: &ID3D11ShaderResourceView,
        brightness: f32,
    ) -> Result<(bool, bool)> {
        // Wait until the compositor has room for this frame. The upload has
        // already happened (`upload`), so the copy is overlapped with the
        // previous composition interval.
        let signaled = self.wait_frame();
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        let brightness = brightness.clamp(0.4, 2.0);
        let mut view: Option<ID3D11RenderTargetView> = None;
        unsafe {
            gpu.device
                .CreateRenderTargetView(&back, None, Some(&mut view))?;
        }
        let view = view.ok_or_else(|| anyhow!("no backbuffer RTV"))?;
        let mut enhanced = false;
        if *mode != EnhanceMode::Off {
            let result = (|| -> Result<()> {
                let pipeline = enhance
                    .as_mut()
                    .ok_or_else(|| anyhow!("enhance pipeline unavailable"))?;
                pipeline.draw(*mode, brightness, source, &view)
            })();
            match result {
                Ok(()) => enhanced = true,
                Err(error) => {
                    log::line(&format!(
                        "enhance fallback mode={} label={} reason={error:#}",
                        mode.as_u8(),
                        mode.label()
                    ));
                    *mode = EnhanceMode::Off;
                }
            }
        }
        if !enhanced {
            // Plain path: one GPU copy that also carries the brightness
            // multiplier, so no enhancement failure can leave a black surface.
            self.pool.passes.copy(source, brightness, &view);
        }
        unsafe {
            gpu.context.OMSetRenderTargets(None, None);
        }
        // Flip-model swapchains refuse Present while a reference to the back
        // buffer is held; release it first.
        drop(view);
        drop(back);
        let presented = self.present_raw(signaled)?;
        Ok((enhanced, presented))
    }

    /// Present an all-black video layer while the audio keeps playing. The
    /// danmaku swapchain sits above this layer, so comments stay visible.
    pub fn present_black(&mut self, gpu: &Gpu) -> Result<bool> {
        let signaled = self.wait_frame();
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        let mut view: Option<ID3D11RenderTargetView> = None;
        unsafe {
            gpu.device
                .CreateRenderTargetView(&back, None, Some(&mut view))?;
        }
        let view = view.ok_or_else(|| anyhow!("no backbuffer RTV"))?;
        unsafe {
            gpu.context
                .ClearRenderTargetView(&view, &[0.0, 0.0, 0.0, 1.0]);
            gpu.context.OMSetRenderTargets(None, None);
        }
        drop(view);
        drop(back);
        self.present_raw(signaled)
    }

    /// Present the current backbuffer with `DO_NOT_WAIT`; only a real `S_OK`
    /// counts as a presented frame.
    pub fn present_raw(&mut self, signaled: bool) -> Result<bool> {
        let hr = unsafe { self.swap.Present(0, DXGI_PRESENT_DO_NOT_WAIT) };
        if hr.0 == 0 {
            self.presents = self.presents.saturating_add(1);
            Ok(true)
        } else {
            self.failures = self.failures.saturating_add(1);
            if self.failures <= 3 || self.failures % 120 == 0 {
                log::line(&format!(
                    "present-fail hr=0x{:08X} count={} signaled={}",
                    hr.0 as u32,
                    self.failures,
                    u8::from(signaled)
                ));
            }
            Ok(false)
        }
    }
}

/// DirectComposition device, target and the three layered visuals.
pub struct Composition {
    device: IDCompositionDevice,
    target: IDCompositionTarget,
    root: IDCompositionVisual,
    pub video_visual: IDCompositionVisual,
    /// Danmaku and UI live above the video. During a drag-resize these two
    /// visuals are scaled with the window so the frozen last frames follow
    /// the frame; they are reset and redrawn when the gesture settles.
    danmaku_visual: IDCompositionVisual,
    ui_visual: IDCompositionVisual,
}

impl Composition {
    pub fn new(
        hwnd: HWND,
        gpu: &Gpu,
        video: &IDXGISwapChain1,
        danmaku: &IDXGISwapChain1,
        ui: &IDXGISwapChain1,
    ) -> Result<Self> {
        unsafe {
            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let device: IDCompositionDevice = DCompositionCreateDevice(Some(&dxgi))?;
            let target = device.CreateTargetForHwnd(hwnd, true)?;
            let root = device.CreateVisual()?;
            target.SetRoot(&root)?;
            let video_visual = device.CreateVisual()?;
            let video_unknown: IUnknown = video.cast()?;
            video_visual.SetContent(&video_unknown)?;
            let danmaku_visual = device.CreateVisual()?;
            let danmaku_unknown: IUnknown = danmaku.cast()?;
            danmaku_visual.SetContent(&danmaku_unknown)?;
            let ui_visual = device.CreateVisual()?;
            let ui_unknown: IUnknown = ui.cast()?;
            ui_visual.SetContent(&ui_unknown)?;
            root.AddVisual(&video_visual, true, None::<&IDCompositionVisual>)?;
            root.AddVisual(&danmaku_visual, true, &video_visual)?;
            root.AddVisual(&ui_visual, true, &danmaku_visual)?;
            device.Commit()?;
            Ok(Self {
                device,
                target,
                root,
                video_visual,
                danmaku_visual,
                ui_visual,
            })
        }
    }

    /// Scale the decoded picture into the window with independent axes and
    /// place its top-left at (`x`, `y`) in window pixels. Fill mode uses a
    /// uniform scale larger than the client; the root clip crops the overflow.
    pub fn set_video_transform(&self, x: f32, y: f32, scale_x: f32, scale_y: f32) -> Result<()> {
        unsafe {
            let matrix = Matrix3x2 {
                M11: scale_x,
                M12: 0.0,
                M21: 0.0,
                M22: scale_y,
                M31: x,
                M32: y,
            };
            self.video_visual.SetTransform2(&matrix)?;
            self.device.Commit()?;
        }
        Ok(())
    }

    /// Swap the video layer's swapchain content in place (a 画质 switch that
    /// changes the decoded picture size rebuilds the swapchain). The rest of
    /// the composition tree is untouched, so the UI/danmaku layers keep their
    /// order.
    /// Scale the danmaku and UI layers with the window during a drag-resize.
    /// Their swapchains are monitor-sized and hold the last frame; scaling the
    /// visuals from the gesture's starting client size makes the danmaku,
    /// loading card, toolbar and HUD follow the window instead of freezing at
    /// the old rectangle. The engine resets this to 1:1 and redraws on settle.
    pub fn set_overlay_scale(&self, scale_x: f32, scale_y: f32) -> Result<()> {
        let matrix = Matrix3x2 {
            M11: scale_x,
            M12: 0.0,
            M21: 0.0,
            M22: scale_y,
            M31: 0.0,
            M32: 0.0,
        };
        unsafe {
            self.danmaku_visual.SetTransform2(&matrix)?;
            self.ui_visual.SetTransform2(&matrix)?;
            self.device.Commit()?;
        }
        Ok(())
    }

    pub fn set_video_content(&self, video: &IDXGISwapChain1) -> Result<()> {
        unsafe {
            let content: IUnknown = video.cast()?;
            self.video_visual.SetContent(&content)?;
            self.device.Commit()?;
        }
        Ok(())
    }

    /// Clip the whole composition tree to the client area. A decorated window
    /// with `WS_EX_NOREDIRECTIONBITMAP` otherwise lets the swapchain content
    /// bleed a few pixels under the non-client frame.
    pub fn set_root_clip(&self, width: u32, height: u32) -> Result<()> {
        let rect = windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: width.max(1) as f32,
            bottom: height.max(1) as f32,
        };
        unsafe {
            self.root.SetClip2(&rect)?;
            self.device.Commit()?;
        }
        Ok(())
    }

    /// One commit for the interaction fast path: clip the composition to the
    /// new client size and move/scale the video layer in the same transaction.
    pub fn set_video_layout(
        &self,
        width: u32,
        height: u32,
        x: f32,
        y: f32,
        scale_x: f32,
        scale_y: f32,
    ) -> Result<()> {
        let rect = windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: width.max(1) as f32,
            bottom: height.max(1) as f32,
        };
        let matrix = Matrix3x2 {
            M11: scale_x,
            M12: 0.0,
            M21: 0.0,
            M22: scale_y,
            M31: x,
            M32: y,
        };
        unsafe {
            self.root.SetClip2(&rect)?;
            self.video_visual.SetTransform2(&matrix)?;
            self.device.Commit()?;
        }
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        unsafe {
            self.device.Commit()?;
        }
        Ok(())
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

/// Present a UI swapchain without waiting (the next state change redraws).
pub fn present_ui(swap: &IDXGISwapChain1) -> bool {
    // UI is a composition layer above the video; present on the monitor's
    // vblank so the toolbar, sidebar and every animation share the display's
    // own 120 Hz cadence instead of racing and dropping queued frames.
    unsafe { swap.Present(1, DXGI_PRESENT(0)).0 == 0 }
}

/// Present a UI swapchain and wait for room in the composition queue. Used
/// once when the sidebar lane is pre-drawn outside the window: the window may
/// only grow after DWM actually has those pixels.
pub fn present_ui_sync(swap: &IDXGISwapChain1) -> bool {
    unsafe { swap.Present(0, DXGI_PRESENT(0)).0 == 0 }
}

/// Position and independent scales for one aspect mode.
pub fn video_transform(
    client_width: f32,
    client_height: f32,
    video_width: u32,
    video_height: u32,
    aspect: AspectMode,
) -> (f32, f32, f32, f32) {
    let cw = client_width.max(1.0);
    let ch = client_height.max(1.0);
    let vw = video_width.max(2) as f32;
    let vh = video_height.max(2) as f32;
    match aspect {
        AspectMode::Fit => {
            let scale = (cw / vw).min(ch / vh).max(0.0001);
            (
                (cw - vw * scale) * 0.5,
                (ch - vh * scale) * 0.5,
                scale,
                scale,
            )
        }
        AspectMode::Fill => {
            let scale = (cw / vw).max(ch / vh).max(0.0001);
            (
                (cw - vw * scale) * 0.5,
                (ch - vh * scale) * 0.5,
                scale,
                scale,
            )
        }
        AspectMode::Stretch => (0.0, 0.0, cw / vw, ch / vh),
    }
}

/// The on-screen video rect in client pixels. Fill and Stretch cover the whole
/// client (Fill crops outside it), so the UI can skip its letterbox bars.
pub fn video_bounds(
    client_width: f32,
    client_height: f32,
    video_width: u32,
    video_height: u32,
    aspect: AspectMode,
) -> (f32, f32, f32, f32) {
    let (x, y, sx, sy) = video_transform(
        client_width,
        client_height,
        video_width,
        video_height,
        aspect,
    );
    (
        x,
        y,
        video_width.max(2) as f32 * sx,
        video_height.max(2) as f32 * sy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn aspect_cycles_fit_fill_stretch() {
        assert_eq!(AspectMode::Fit.next(), AspectMode::Fill);
        assert_eq!(AspectMode::Fill.next(), AspectMode::Stretch);
        assert_eq!(AspectMode::Stretch.next(), AspectMode::Fit);
        assert_eq!(AspectMode::Fit.label(), "适应");
        assert_eq!(AspectMode::from_u8(2), AspectMode::Stretch);
    }

    #[test]
    fn fit_letterboxes_and_fill_crops_a_16_9_picture_in_a_square() {
        let (fx, fy, fw, fh) = video_bounds(1000.0, 1000.0, 1600, 900, AspectMode::Fit);
        assert!(close(fx, 0.0));
        assert!(close(fy, 218.75));
        assert!(close(fw, 1000.0));
        assert!(close(fh, 562.5));

        let (rx, ry, rw, rh) = video_bounds(1000.0, 1000.0, 1600, 900, AspectMode::Fill);
        assert!(close(rx, -388.8889));
        assert!(close(ry, 0.0));
        assert!(close(rw, 1777.7778));
        assert!(close(rh, 1000.0));
    }

    #[test]
    fn stretch_ignores_the_source_aspect() {
        let (x, y, w, h) = video_bounds(1000.0, 1000.0, 1600, 900, AspectMode::Stretch);
        assert!(close(x, 0.0));
        assert!(close(y, 0.0));
        assert!(close(w, 1000.0));
        assert!(close(h, 1000.0));
    }

    /// Read the pool's BGRA picture back to the CPU. Tests only: the player
    /// never maps a decoded picture.
    fn read_picture(gpu: &Gpu, picture: &Picture, width: u32, height: u32) -> Vec<[u8; 4]> {
        unsafe {
            let description = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            gpu.device
                .CreateTexture2D(&description, None, Some(&mut staging))
                .expect("staging texture");
            let staging = staging.expect("staging texture");
            gpu.context.CopyResource(&staging, &picture._tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(
                    &staging,
                    0,
                    windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )
                .expect("map staging");
            let mut pixels = Vec::with_capacity((width * height) as usize);
            let start = mapped.pData as *const u8;
            for row in 0..height as usize {
                let row_start = start.add(row * mapped.RowPitch as usize);
                for col in 0..width as usize {
                    let p = row_start.add(col * 4);
                    pixels.push([*p, *p.add(1), *p.add(2), *p.add(3)]);
                }
            }
            gpu.context.Unmap(&staging, 0);
            pixels
        }
    }

    #[test]
    fn gpu_frame_pool_converts_a_real_4k_nv12_dump() {
        let Some(path) = std::env::var_os("O11A1_NV12_DUMP") else {
            return;
        };
        let Ok(data) = std::fs::read(&path) else {
            return;
        };
        let (width, height) = (3840u32, 2160u32);
        if data.len() != (width * height * 3 / 2) as usize {
            return;
        }
        let gpu = Gpu::new().expect("gpu");
        let mut pool = FramePool::new(&gpu, width, height).expect("frame pool");
        let frame = pool.upload(&gpu, &data, 0).expect("upload");
        // Upload sanity: the Y texture must hold the dump's luma plane.
        let y_rows = read_r8(&gpu, &pool.planes[frame.slot].y, width, height);
        let y_plane = &data[..(width * height) as usize];
        let y_mismatch = y_rows
            .iter()
            .zip(y_plane.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(y_mismatch, 0, "Y plane upload mismatch");
        // Diagnostic: copy the luma plane through the same pass machinery.
        pool.passes
            .copy(&pool.planes[frame.slot].y_srv, 1.0, &pool.pictures[frame.slot].rtv);
        let grey = read_picture(&gpu, &pool.pictures[frame.slot], width, height);
        eprintln!("copy pass corner {:?}", grey[0]);
        // And re-run the real conversion.
        pool.passes.convert_nv12(
            &pool.planes[frame.slot].y_srv,
            &pool.planes[frame.slot].uv_srv,
            &pool.pictures[frame.slot].rtv,
        );
        let pixels = read_picture(&gpu, &pool.pictures[frame.slot], width, height);
        let mut bgra = Vec::with_capacity(pixels.len() * 3);
        for pixel in &pixels {
            bgra.extend_from_slice(&pixel[..3]);
        }
        let out = std::path::PathBuf::from(&path).with_extension("bgra");
        std::fs::write(&out, bgra).expect("write bgra");
        // The dump's top-left corner is dark: no all-white regression.
        let corner = pixels[0];
        let centre = pixels[(height / 2 * width + width / 2) as usize];
        eprintln!("corner {corner:?} centre {centre:?} wrote {}", out.display());
        assert!(
            corner[0] < 200 && corner[1] < 200 && corner[2] < 200,
            "corner {corner:?}"
        );
        assert!(
            centre[0] < 250 || centre[1] < 250 || centre[2] < 250,
            "centre {centre:?}"
        );
    }

    fn read_r8(gpu: &Gpu, texture: &ID3D11Texture2D, width: u32, height: u32) -> Vec<u8> {
        unsafe {
            let description = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: windows::Win32::Graphics::Direct3D11::D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            gpu.device
                .CreateTexture2D(&description, None, Some(&mut staging))
                .expect("staging");
            let staging = staging.expect("staging");
            gpu.context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(
                    &staging,
                    0,
                    windows::Win32::Graphics::Direct3D11::D3D11_MAP_READ,
                    0,
                    Some(&mut mapped),
                )
                .expect("map");
            let mut rows = Vec::with_capacity((width * height) as usize);
            let start = mapped.pData as *const u8;
            for row in 0..height as usize {
                let row_start = start.add(row * mapped.RowPitch as usize);
                for col in 0..width as usize {
                    rows.push(*row_start.add(col));
                }
            }
            gpu.context.Unmap(&staging, 0);
            rows
        }
    }

    #[test]
    fn gpu_frame_pool_converts_nv12_and_blends_on_the_gpu() {
        let gpu = match Gpu::new() {
            Ok(gpu) => gpu,
            Err(error) => {
                eprintln!("skipping GPU test: {error:#}");
                return;
            }
        };
        let (width, height) = (64u32, 36u32);
        let mut pool = FramePool::new(&gpu, width, height).expect("frame pool");

        // BT.709 limited white: Y=235, chroma neutral.
        let mut white = vec![128u8; (width * height * 3 / 2) as usize];
        white[..(width * height) as usize].fill(235);
        let white_frame = pool.upload(&gpu, &white, 10).expect("upload white");
        let pixels = read_picture(&gpu, &pool.pictures[white_frame.slot], width, height);
        let centre = pixels[(height / 2 * width + width / 2) as usize];
        assert!(centre[0] >= 250 && centre[1] >= 250 && centre[2] >= 250, "{centre:?}");
        assert_eq!(centre[3], 255);

        // Limited black stays black.
        let mut black = vec![128u8; (width * height * 3 / 2) as usize];
        black[..(width * height) as usize].fill(16);
        let black_frame = pool.upload(&gpu, &black, 20).expect("upload black");
        let pixels = read_picture(&gpu, &pool.pictures[black_frame.slot], width, height);
        let centre = pixels[(height / 2 * width + width / 2) as usize];
        assert!(centre[0] <= 2 && centre[1] <= 2 && centre[2] <= 2, "{centre:?}");

        // A half-way blend of the two pictures is mid grey.
        pool.passes.blend(
            &pool.pictures[white_frame.slot].srv,
            &pool.pictures[black_frame.slot].srv,
            0.5,
            &pool.mix.rtv,
        );
        let pixels = read_picture(&gpu, &pool.mix, width, height);
        let centre = pixels[(height / 2 * width + width / 2) as usize];
        assert!((centre[0] as i32 - 128).abs() <= 2, "{centre:?}");
    }
}
