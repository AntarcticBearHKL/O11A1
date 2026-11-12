//! 画质增强 — CAS (通用) and Anime4K Restore-CNN-S (番剧).
//!
//! Ported from the shipped `src/dcomp/enhance.rs`; the shaders are byte-for-byte
//! the ones that were verified there (`shaders/enhance.hlsl`, Anime4K v4.0.1
//! Restore-CNN-S). The only difference is where the source comes from: this
//! player decodes NV12 on the CPU, converts to BGRA and uploads into a staging
//! texture, so the chain always begins from a texture copy.
//!
//! Failure policy: every entry point returns a reason; the engine logs
//! `enhance fallback` and presents through the plain copy for the rest of the
//! session, so a missing `d3dcompiler_47.dll` or a driver that refuses RGBA16F
//! can never black-screen the window.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Result};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIEWPORT, ID3D11Buffer, ID3D11ClassLinkage, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::log;

/// CAS strength, the same 0.6 the shipped tier uses.
const CAS_STRENGTH: f32 = 0.6;

/// Modes accepted by [`crate::Player::set_enhance`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnhanceMode {
    Off = 0,
    Cas = 1,
    Anime = 2,
}

impl EnhanceMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Cas,
            2 => Self::Anime,
            _ => Self::Off,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Cas => "通用",
            Self::Anime => "番剧",
        }
    }

    /// Cycle 关闭 → 通用 → 番剧 → 关闭.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Cas,
            Self::Cas => Self::Anime,
            Self::Anime => Self::Off,
        }
    }
}

// ------------------------------------------------------------------ targets

struct Target {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    rtv: ID3D11RenderTargetView,
}

impl Target {
    fn new(device: &ID3D11Device, width: u32, height: u32, format: DXGI_FORMAT) -> Result<Self> {
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
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&description, None, Some(&mut tex))
                .map_err(|error| anyhow!("CreateTexture2D {format:?}: {error}"))?;
            let tex = tex.ok_or_else(|| anyhow!("CreateTexture2D {format:?}: no texture"))?;
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))
                .map_err(|error| anyhow!("CreateShaderResourceView {format:?}: {error}"))?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device
                .CreateRenderTargetView(&tex, None, Some(&mut rtv))
                .map_err(|error| anyhow!("CreateRenderTargetView {format:?}: {error}"))?;
            Ok(Self {
                tex,
                srv: srv.ok_or_else(|| anyhow!("no SRV {format:?}"))?,
                rtv: rtv.ok_or_else(|| anyhow!("no RTV {format:?}"))?,
            })
        }
    }
}

// ------------------------------------------------------------------ shaders

/// `D3DCompile` from `d3dcompiler_47.dll`, resolved once per process.
type D3DCompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    *const i8,
    *const c_void,
    *mut c_void,
    *const i8,
    *const i8,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> i32;

unsafe fn vtbl<F: Copy>(object: *mut c_void, index: usize) -> F {
    let table = *(object as *const *const usize);
    std::mem::transmute_copy(&*table.add(index))
}

fn d3d_module() -> Result<HMODULE> {
    static MODULE: OnceLock<Result<usize, String>> = OnceLock::new();
    let value = MODULE.get_or_init(|| unsafe {
        let module = LoadLibraryW(windows::core::w!("d3dcompiler_47.dll"))
            .map_err(|error| format!("d3dcompiler_47.dll: {error}"))?;
        Ok(module.0 as usize)
    });
    match value {
        Ok(address) => Ok(HMODULE(*address as *mut c_void)),
        Err(reason) => Err(anyhow!(reason.clone())),
    }
}

fn compile(source: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), Result<Vec<u8>, String>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (entry.to_string(), target.to_string());
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(&key) {
            return hit.clone().map_err(|error| anyhow!(error));
        }
    }
    let compiled = compile_uncached(source, entry, target);
    if let Ok(mut map) = cache.lock() {
        let stored = match &compiled {
            Ok(bytes) => Ok(bytes.clone()),
            Err(error) => Err(error.to_string()),
        };
        map.insert(key, stored);
    }
    compiled
}

fn compile_uncached(source: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    let module = d3d_module()?;
    unsafe {
        let address = GetProcAddress(module, PCSTR(b"D3DCompile\0".as_ptr()));
        let Some(address) = address else {
            return Err(anyhow!("D3DCompile entry missing"));
        };
        let compile: D3DCompileFn = std::mem::transmute_copy(&address);
        let entry_c = format!("{entry}\0");
        let target_c = format!("{target}\0");
        let mut code: *mut c_void = std::ptr::null_mut();
        let mut errors: *mut c_void = std::ptr::null_mut();
        let hr = compile(
            source.as_ptr() as *const c_void,
            source.len(),
            b"native_player\0".as_ptr() as *const i8,
            std::ptr::null(),
            std::ptr::null_mut(),
            entry_c.as_ptr() as *const i8,
            target_c.as_ptr() as *const i8,
            1 << 15,
            0,
            &mut code,
            &mut errors,
        );
        let message = if errors.is_null() {
            String::new()
        } else {
            let get_pointer: unsafe extern "system" fn(*mut c_void) -> *const c_void =
                vtbl(errors, 3);
            let get_size: unsafe extern "system" fn(*mut c_void) -> usize = vtbl(errors, 4);
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = vtbl(errors, 2);
            let size = get_size(errors);
            let text =
                String::from_utf8_lossy(std::slice::from_raw_parts(get_pointer(errors) as *const u8, size))
                    .into_owned();
            release(errors);
            text
        };
        if hr < 0 || code.is_null() {
            return Err(anyhow!("D3DCompile {entry} hr=0x{:08X} {message}", hr as u32));
        }
        let get_pointer: unsafe extern "system" fn(*mut c_void) -> *const c_void = vtbl(code, 3);
        let get_size: unsafe extern "system" fn(*mut c_void) -> usize = vtbl(code, 4);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = vtbl(code, 2);
        let size = get_size(code);
        let bytes = std::slice::from_raw_parts(get_pointer(code) as *const u8, size).to_vec();
        release(code);
        Ok(bytes)
    }
}

const ENHANCE_HLSL: &str = include_str!("shaders/enhance.hlsl");
const ANIME4K_S_HLSL: &str = include_str!("shaders/anime4k_restore_s.hlsl");

fn pixel_shader(device: &ID3D11Device, source: &str, entry: &str) -> Result<ID3D11PixelShader> {
    let code = compile(source, entry, "ps_5_0")?;
    unsafe {
        let mut shader: Option<ID3D11PixelShader> = None;
        device
            .CreatePixelShader(&code, None::<&ID3D11ClassLinkage>, Some(&mut shader))
            .map_err(|error| anyhow!("CreatePixelShader {entry}: {error}"))?;
        shader.ok_or_else(|| anyhow!("CreatePixelShader {entry}: no shader"))
    }
}

enum Chain {
    Cas {
        cas: ID3D11PixelShader,
    },
    Anime4k {
        stats_x: ID3D11PixelShader,
        stats_y: ID3D11PixelShader,
        apply: ID3D11PixelShader,
        /// `ps0..ps3` in file order: Conv-4x3x3x3, two 8-channel layers, output.
        restore: Vec<ID3D11PixelShader>,
    },
}

impl Chain {
    fn conv_targets(&self) -> usize {
        match self {
            Chain::Anime4k { .. } => 3,
            Chain::Cas { .. } => 0,
        }
    }
}

fn build_chain(device: &ID3D11Device, mode: EnhanceMode) -> Result<Chain> {
    match mode {
        EnhanceMode::Cas => Ok(Chain::Cas {
            cas: pixel_shader(device, ENHANCE_HLSL, "cas_main")?,
        }),
        EnhanceMode::Anime => {
            let mut restore = Vec::with_capacity(4);
            for entry in ["ps0", "ps1", "ps2", "ps3"] {
                restore.push(pixel_shader(device, ANIME4K_S_HLSL, entry)?);
            }
            Ok(Chain::Anime4k {
                stats_x: pixel_shader(device, ENHANCE_HLSL, "clamp_stats_x")?,
                stats_y: pixel_shader(device, ENHANCE_HLSL, "clamp_stats_y")?,
                apply: pixel_shader(device, ENHANCE_HLSL, "clamp_apply")?,
                restore,
            })
        }
        EnhanceMode::Off => Err(anyhow!("no chain for Off")),
    }
}

// ------------------------------------------------------------------ pipeline

/// The enhancement chain for one video surface size.
pub struct EnhancePipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    vertex: ID3D11VertexShader,
    brightness: ID3D11PixelShader,
    dims: ID3D11Buffer,
    /// CPU frame staging -> shader-readable copy.
    source: Target,
    /// The luma-clamped picture, the CNN's MAIN input.
    color: Target,
    /// Intermediate RGBA16F convolution targets.
    conv: Vec<Target>,
    chain: Option<(EnhanceMode, Chain)>,
    /// Frames drawn through a chain (stats overlay).
    pub frames: u64,
}

impl EnhancePipeline {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let width = width.max(2);
        let height = height.max(2);
        let vs_code = compile(ENHANCE_HLSL, "vs_main", "vs_5_0")?;
        let vertex = unsafe {
            let mut vertex: Option<ID3D11VertexShader> = None;
            device
                .CreateVertexShader(&vs_code, None::<&ID3D11ClassLinkage>, Some(&mut vertex))
                .map_err(|error| anyhow!("CreateVertexShader: {error}"))?;
            vertex.ok_or_else(|| anyhow!("CreateVertexShader: no shader"))?
        };
        let brightness = pixel_shader(device, ENHANCE_HLSL, "brightness_main")?;
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
                .map_err(|error| anyhow!("CreateBuffer: {error}"))?;
            buffer.ok_or_else(|| anyhow!("CreateBuffer: no buffer"))?
        };
        let source = Target::new(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let color = Target::new(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        log::line(&format!(
            "enhance pipeline size={width}x{height} source=BGRA8 conv=RGBA16F"
        ));
        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            width,
            height,
            vertex,
            brightness,
            dims,
            source,
            color,
            conv: Vec::new(),
            chain: None,
            frames: 0,
        })
    }

    fn ensure_chain(&mut self, mode: EnhanceMode) -> Result<()> {
        if matches!(&self.chain, Some((cached, _)) if *cached == mode) {
            return Ok(());
        }
        self.chain = None;
        self.conv.clear();
        self.frames = 0;
        let chain = build_chain(&self.device, mode)?;
        for index in 0..chain.conv_targets() {
            self.conv.push(Target::new(
                &self.device,
                self.width,
                self.height,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
            ).map_err(|error| anyhow!("conv target {index}: {error}"))?);
        }
        log::line(&format!(
            "enhance mode={} label={} passes={}",
            mode.as_u8(),
            mode.label(),
            match &chain {
                Chain::Cas { .. } => 1,
                Chain::Anime4k { .. } => 7,
            }
        ));
        self.chain = Some((mode, chain));
        Ok(())
    }

    fn set_dims(&self, strength: f32, brightness: f32) {
        let packed = [
            self.width.saturating_sub(1) as f32,
            self.height.saturating_sub(1) as f32,
            self.width as f32,
            self.height as f32,
            strength,
            brightness.clamp(0.4, 2.0),
            0.0,
            0.0,
        ];
        unsafe {
            self.context.UpdateSubresource(
                &self.dims,
                0,
                None,
                packed.as_ptr() as *const c_void,
                0,
                0,
            );
        }
    }

    fn pass_to(
        &self,
        shader: &ID3D11PixelShader,
        inputs: &[&ID3D11ShaderResourceView],
        target: &ID3D11RenderTargetView,
    ) {
        let views: Vec<Option<ID3D11ShaderResourceView>> =
            inputs.iter().map(|view| Some((*view).clone())).collect();
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(None);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(shader, None);
            self.context.VSSetConstantBuffers(0, Some(&[Some(self.dims.clone())]));
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.dims.clone())]));
            self.context.PSSetShaderResources(0, Some(&views));
            self.context.Draw(3, 0);
            let empty: Vec<Option<ID3D11ShaderResourceView>> =
                inputs.iter().map(|_| None).collect();
            self.context.PSSetShaderResources(0, Some(&empty));
        }
    }

    /// Run one frame's chain. `staging` is the CPU-written BGRA texture, `back`
    /// is the current swapchain backbuffer view.
    pub fn draw(
        &mut self,
        mode: EnhanceMode,
        brightness: f32,
        staging: &ID3D11Texture2D,
        back: &ID3D11RenderTargetView,
    ) -> Result<()> {
        if mode == EnhanceMode::Off {
            return Err(anyhow!("enhance draw called with Off"));
        }
        unsafe {
            self.context.CopyResource(&self.source.tex, staging);
        }
        self.ensure_chain(mode)?;
        let result = {
            let Some((_, chain)) = self.chain.as_ref() else {
                return Err(anyhow!("no chain"));
            };
            match chain {
                Chain::Cas { cas } => {
                    self.set_dims(CAS_STRENGTH, brightness);
                    self.pass_to(cas, &[&self.source.srv], back);
                    Ok(())
                }
                Chain::Anime4k {
                    stats_x,
                    stats_y,
                    apply,
                    restore,
                } => {
                    if self.conv.len() < 3 || restore.len() < 4 {
                        Err(anyhow!("anime4k chain incomplete"))
                    } else {
                        self.set_dims(0.0, brightness);
                        // Clamp_Highlights: a 5x5 luma max in two sweeps, then
                        // clamp every pixel to its local ceiling.
                        self.pass_to(stats_x, &[&self.source.srv], &self.conv[0].rtv);
                        // stats_y reads the first sweep through t1 as well as
                        // t0; the same view is bound twice, as upstream does.
                        self.pass_to(stats_y, &[&self.conv[0].srv, &self.conv[0].srv], &self.conv[1].rtv);
                        self.pass_to(apply, &[&self.source.srv, &self.conv[1].srv], &self.color.rtv);
                        self.pass_to(&restore[0], &[&self.color.srv], &self.conv[0].rtv);
                        self.pass_to(&restore[1], &[&self.conv[0].srv], &self.conv[1].rtv);
                        self.pass_to(&restore[2], &[&self.conv[1].srv], &self.conv[2].rtv);
                        self.pass_to(
                            &restore[3],
                            &[&self.color.srv, &self.conv[2].srv],
                            back,
                        );
                        Ok(())
                    }
                }
            }
        };
        // Never leave views bound: the next frame writes those textures and
        // Present refuses a bound render target.
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.context.PSSetShaderResources(0, Some(&[None, None, None, None, None, None, None, None]));
        }
        if result.is_ok() {
            if self.frames == 0 {
                log::line(&format!(
                    "enhance first-frame mode={} label={} size={}x{}",
                    mode.as_u8(),
                    mode.label(),
                    self.width,
                    self.height
                ));
            }
            self.frames = self.frames.saturating_add(1);
        }
        result
    }

    /// 关闭 enhancement but still apply the brightness multiplier through the
    /// video pixel shader. The staging texture is copied to the shader-readable
    /// source first, exactly like the enhance chains' entry point.
    pub fn draw_brightness(
        &mut self,
        staging: &ID3D11Texture2D,
        back: &ID3D11RenderTargetView,
        brightness: f32,
    ) -> Result<()> {
        unsafe {
            self.context.CopyResource(&self.source.tex, staging);
        }
        self.set_dims(brightness, 1.0);
        self.pass_to(&self.brightness, &[&self.source.srv], back);
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.context
                .PSSetShaderResources(0, Some(&[None, None, None, None]));
        }
        Ok(())
    }
}
