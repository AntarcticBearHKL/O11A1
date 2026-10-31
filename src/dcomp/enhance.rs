//! f202: 画质增强 — the real-time enhancement tiers in the video sink.
//!
//! # Where this sits
//!
//! The picture reaches the sink as a CPU frame ([`super::video::Job::Frame`]) or
//! as a GPU warp result ([`super::video::Job::Warp`]). Both end up as a texture
//! on the sink's device; this module turns that texture into the backbuffer:
//!
//! ```text
//!   frame (CPU) --stage--> staging --copy--> source SRV --\
//!                                                          >-- chain --> backbuffer
//!   warp (GPU) ---------------------------> warped RT -----/
//! ```
//!
//! `关闭` (mode 0) never comes through here: `video.rs` keeps the old
//! `staging -> texture -> backbuffer` copies, byte for byte, so the default tier
//! costs exactly what it cost before this module existed.
//!
//! # Tiers
//!
//! | mode | tier | what runs |
//! |------|------|-----------|
//! | 1 | 锐化 | AMD CAS, one pass, strength 0.6 |
//! | 2 | Anime4K | `Clamp_Highlights` (3 passes) + `Restore_CNN_M` (8 passes) |
//! | 3 | FSR | FSR1 RCAS, one pass |
//!
//! RTX VSR was tier 4 and has been **removed** (f207): the driver's NGX core
//! initialises (`0x1`) but reports `ImageSuperResolution.Available = 0` and
//! `VideoSuperResolution.Available = 0` - and so does every other NGX feature,
//! including the DLSS baseline (`CreateFeature` = `0xBAD0000B`). The
//! `nvngx_dlisr.dll` that looked promising is a CUDA-only feature module whose
//! D3D11 entry is a six-byte stub returning a constant. There is nothing left
//! for a tier to call, so the detection code is gone too; the evidence stays in
//! `docs/f206_ngx_features.log`. A `player_enhance: 4` left in an old
//! settings.json is clamped back to 关闭 by `settings.rs`.
//!
//! Anime4K's chain also contains `Upscale_CNN_x2_M`, but that shader's own
//! `//!WHEN OUTPUT.w MAIN.w / 1.200 >` gate means upstream skips it whenever the
//! output is not at least 1.2x the source — and here the video layer *is* the
//! frame size (the decoder follows the window, see `player::reader_output_size`),
//! so the ratio is 1.0 and the chain is Clamp + Restore. The same reasoning is
//! why FSR reduces to RCAS at 1:1. A resolution jump inside a session rebuilds
//! the layer, and the ratio is 1.0 again: there is no configuration where the x2
//! stage would run, so enabling it here would be dead code pretending to be a
//! feature.
//!
//! # Failure policy
//!
//! Every step returns the HRESULT it failed on; `video.rs` logs it, drops the
//! tier back to `关闭` for the rest of the session and keeps presenting through
//! the staged copy. There is no path where a failed tier black-screens the
//! window.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
    D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;

/// 关闭: the historical path, untouched.
pub(crate) const OFF: u32 = 0;
/// 锐化: AMD CAS.
pub(crate) const SHARPEN: u32 = 1;
/// Anime4K v4.0.1 CNN restore.
pub(crate) const ANIME4K: u32 = 2;
/// FSR1 spatial sharpening (RCAS at this build's 1:1 ratio).
pub(crate) const FSR: u32 = 3;

/// Everything the menu offers, in menu order.
///
/// RTX VSR used to be tier 4. It is gone: this machine's driver reports
/// `Available = 0` for every NGX feature (even the DLSS baseline fails
/// `CreateFeature` with `0xBAD0000B`), and `nvngx_dlisr.dll` turns out to be a
/// CUDA-only module whose D3D11 entry is a six-byte stub that returns a
/// constant - so the tier could never do anything here. Evidence:
/// `docs/f206_ngx_features.log`. The old value `4` in a settings.json is
/// clamped back to 关闭 by `settings.rs`.
pub(crate) const MODES: [u32; 4] = [OFF, SHARPEN, ANIME4K, FSR];

/// The menu label of a tier (logs and the UI use the same words).
pub(crate) fn label(mode: u32) -> &'static str {
    match mode {
        SHARPEN => "锐化",
        ANIME4K => "Anime4K",
        FSR => "FSR",
        _ => "关闭",
    }
}

/// CAS strength (0..1). 0.6 is the tier's fixed value: high enough to read as
/// "sharper" on a 1080p picture, low enough not to ring on line art.
const CAS_STRENGTH: f32 = 0.6;
/// FSR1 RCAS sharpness in stops (0 = the reference maximum). 0.25 is the
/// "definitely visible, not crunchy" point for this pipeline.
const RCAS_STOPS: f32 = 0.25;

/// The tier the UI asked for. Written on the UI thread, read by the video
/// thread once per frame (one relaxed atomic load), so a menu pick takes effect
/// on the next picture with no handshake with the sink.
static DESIRED: AtomicU32 = AtomicU32::new(OFF);

/// Why the last tier fell back to 关闭 (see [`note_failure`]).
static FALLBACK: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn set_desired(mode: u32) {
    DESIRED.store(if MODES.contains(&mode) { mode } else { OFF }, Ordering::Relaxed);
}

pub(crate) fn desired() -> u32 {
    DESIRED.load(Ordering::Relaxed)
}

/// Whether the shader compiler the tiers need is usable (`d3dcompiler_47.dll`,
/// already a hard dependency of [`super::warp`]). Cached: the answer cannot
/// change inside one process.
pub(crate) fn shader_runtime() -> Result<(), String> {
    super::warp::compiler_available()
}

/// Whether each tier can run, for the menu and for `BILI_SR_PROBE`.
#[derive(Clone, Debug)]
pub(crate) struct Availability {
    /// 锐化 / Anime4K / FSR — shader compiler plus the DComp video layer.
    pub(crate) shaders: bool,
    /// Why `shaders` is false (empty when it is true).
    pub(crate) shader_reason: String,
}

pub(crate) fn availability() -> Availability {
    let shader = shader_runtime();
    Availability {
        shaders: shader.is_ok(),
        shader_reason: shader.err().unwrap_or_default(),
    }
}

/// `BILI_SR_TRACE=1`: log every chain build and pass size (the first frames do
/// it unconditionally, the way the warp trace does).
pub(crate) fn trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("BILI_SR_TRACE").is_ok_and(|value| {
            matches!(value.trim(), "1" | "true" | "yes" | "on")
        })
    })
}

/// The last reason a tier fell back to 关闭, for the 画质增强 row's hint. A
/// failure is sticky evidence, not a state: the UI reads it once and the video
/// thread never rewrites it, so "why did my tier turn itself off" is answerable
/// for the rest of the session.
pub(crate) fn note_failure(reason: &str) {
    if let Ok(mut guard) = FALLBACK.lock() {
        *guard = Some(reason.to_string());
    }
    crate::debug_log(&format!("enhance fallback to 关闭 reason={reason}"));
}

pub(crate) fn last_failure() -> Option<String> {
    FALLBACK.lock().ok().and_then(|guard| guard.clone())
}

/// Whether to run the `_M` variant: `_S` is 4 passes (three convolutions), `_M`
/// 8 passes (seven convolutions). `BILI_SR_ANIME4K=M|S` pins it.
///
/// **The default is `_S`**, and that is a measurement, not a preference
/// (f202, 960x540 one-to-one and 1216x684 captures, `scripts/qa-f202-*.ps1`):
///
/// | variant | PSNR | SSIM | gradient |
/// |---------|------|------|----------|
/// | `_S`    | 38.78 dB | 0.9939 | +9.7 % |
/// | `_M`    | 31.34 dB | 0.7394 | -2.5 % |
///
/// `_M` visibly washes this content out (lifts the blacks, loses contrast), and
/// the effect grows with depth: 7 convolutions instead of 3. The transcription
/// is mechanical and every pass compiles, so this build does not claim `_M` is
/// correct - it ships the variant whose output measures faithful and keeps `_M`
/// reachable behind the environment variable for the A/B. See the f202 report.
fn anime4k_medium() -> bool {
    static MEDIUM: OnceLock<bool> = OnceLock::new();
    *MEDIUM.get_or_init(|| match std::env::var("BILI_SR_ANIME4K") {
        Ok(value) => !matches!(value.trim().to_ascii_lowercase().as_str(), "s" | "small"),
        Err(_) => false,
    })
}

// ---------------------------------------------------------------- sources

/// Where this frame's pixels come from.
pub(crate) enum Source<'a> {
    /// The layer's staging texture, already written by this thread's CPU copy
    /// (`Layer::stage`). Copied into the shader-readable source texture first.
    Staging(&'a ID3D11Texture2D),
    /// A texture an earlier GPU pass rendered into (the optical-flow warp), so
    /// no copy is needed at all.
    View(ID3D11ShaderResourceView),
}

// ---------------------------------------------------------------- targets

/// One off-screen target: the texture, the view a pass writes through and the
/// view the next pass reads.
struct Target {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    rtv: ID3D11RenderTargetView,
}

impl Target {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<Self, String> {
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
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&description, None, Some(&mut tex))
                .map_err(|error| format!("CreateTexture2D {format:?} hr={error}"))?;
            let Some(tex) = tex else {
                return Err(format!("CreateTexture2D {format:?} returned no texture"));
            };
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))
                .map_err(|error| format!("CreateShaderResourceView {format:?} hr={error}"))?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device
                .CreateRenderTargetView(&tex, None, Some(&mut rtv))
                .map_err(|error| format!("CreateRenderTargetView {format:?} hr={error}"))?;
            let (Some(srv), Some(rtv)) = (srv, rtv) else {
                return Err(format!("{format:?} views missing"));
            };
            Ok(Self { tex, srv, rtv })
        }
    }
}

// ---------------------------------------------------------------- shaders

fn bytecode(source: &'static str, entry: &'static str) -> Result<Vec<u8>, String> {
    static CACHE: OnceLock<Mutex<HashMap<(&'static str, &'static str), Result<Vec<u8>, String>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(&(source, entry)) {
            return hit.clone();
        }
    }
    let compiled = super::warp::compile(source, entry, "ps_5_0");
    if let Err(error) = &compiled {
        crate::debug_log(&format!("sr shader {entry} failed: {error}"));
    }
    if let Ok(mut map) = cache.lock() {
        map.insert((source, entry), compiled.clone());
    }
    compiled
}

const ENHANCE_HLSL: &str = include_str!("shaders/enhance.hlsl");
const ANIME4K_M_HLSL: &str = include_str!("shaders/anime4k_restore_m.hlsl");
const ANIME4K_S_HLSL: &str = include_str!("shaders/anime4k_restore_s.hlsl");

fn pixel_shader(
    device: &ID3D11Device,
    code: &[u8],
    name: &str,
) -> Result<ID3D11PixelShader, String> {
    unsafe {
        let mut ps: Option<ID3D11PixelShader> = None;
        device
            .CreatePixelShader(
                code,
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                Some(&mut ps),
            )
            .map_err(|error| format!("CreatePixelShader {name} hr={error}"))?;
        ps.ok_or_else(|| format!("CreatePixelShader {name} returned no shader"))
    }
}

/// The tier's compiled passes, built on first use and dropped on a tier change.
enum Chain {
    Sharpen {
        cas: ID3D11PixelShader,
    },
    Fsr {
        rcas: ID3D11PixelShader,
    },
    Anime4k {
        stats_x: ID3D11PixelShader,
        stats_y: ID3D11PixelShader,
        apply: ID3D11PixelShader,
        /// The generated `Restore_CNN` passes, `ps0..` in file order.
        restore: Vec<ID3D11PixelShader>,
        /// `"M"` (8 passes) or `"S"` (4 passes); logged once per build.
        variant: &'static str,
    },
}

impl Chain {
    /// How many `RGBA16F` convolution targets the chain needs.
    fn conv_targets(&self) -> usize {
        match self {
            Chain::Anime4k { variant, .. } => {
                if *variant == "M" {
                    7
                } else {
                    3
                }
            }
            _ => 0,
        }
    }
}

fn build_chain(device: &ID3D11Device, mode: u32) -> Result<Chain, String> {
    match mode {
        SHARPEN => Ok(Chain::Sharpen {
            cas: pixel_shader(device, &bytecode(ENHANCE_HLSL, "cas_main")?, "cas")?,
        }),
        FSR => Ok(Chain::Fsr {
            rcas: pixel_shader(device, &bytecode(ENHANCE_HLSL, "rcas_main")?, "rcas")?,
        }),
        ANIME4K => {
            let (source, count, variant) = if anime4k_medium() {
                (ANIME4K_M_HLSL, 8usize, "M")
            } else {
                (ANIME4K_S_HLSL, 4usize, "S")
            };
            const ENTRIES: [&str; 8] = ["ps0", "ps1", "ps2", "ps3", "ps4", "ps5", "ps6", "ps7"];
            let mut restore = Vec::with_capacity(count);
            for &entry in ENTRIES.iter().take(count) {
                restore.push(pixel_shader(device, &bytecode(source, entry)?, entry)?);
            }
            Ok(Chain::Anime4k {
                stats_x: pixel_shader(
                    device,
                    &bytecode(ENHANCE_HLSL, "clamp_stats_x")?,
                    "clamp_stats_x",
                )?,
                stats_y: pixel_shader(
                    device,
                    &bytecode(ENHANCE_HLSL, "clamp_stats_y")?,
                    "clamp_stats_y",
                )?,
                apply: pixel_shader(
                    device,
                    &bytecode(ENHANCE_HLSL, "clamp_apply")?,
                    "clamp_apply",
                )?,
                restore,
                variant,
            })
        }
        other => Err(format!("no chain for mode {other}")),
    }
}

// ---------------------------------------------------------------- timing

/// GPU cost of the last frame's chain, µs. The queries are read one frame late
/// on purpose: reading them on the frame that issued them would stall the queue.
struct Timing {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
    armed: bool,
    cost_us: u64,
}

impl Timing {
    fn new(device: &ID3D11Device) -> Result<Self, String> {
        unsafe {
            let make = |kind| -> Result<ID3D11Query, String> {
                let mut query: Option<ID3D11Query> = None;
                device
                    .CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: kind,
                            MiscFlags: 0,
                        },
                        Some(&mut query),
                    )
                    .map_err(|error| format!("CreateQuery {kind:?} hr={error}"))?;
                query.ok_or_else(|| format!("CreateQuery {kind:?} returned nothing"))
            };
            Ok(Self {
                disjoint: make(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                begin: make(D3D11_QUERY_TIMESTAMP)?,
                end: make(D3D11_QUERY_TIMESTAMP)?,
                armed: false,
                cost_us: u64::MAX,
            })
        }
    }

    /// Read the previous frame's timestamps. Never blocks: `S_FALSE` means "not
    /// done yet" and the tier keeps the previous number.
    fn collect(&mut self, context: &ID3D11DeviceContext) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
        let mut begin = 0u64;
        let mut end = 0u64;
        unsafe {
            if context
                .GetData(
                    &self.disjoint,
                    Some(&mut disjoint as *mut _ as *mut c_void),
                    std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32,
                    0,
                )
                .is_err()
            {
                return;
            }
            if context
                .GetData(
                    &self.begin,
                    Some(&mut begin as *mut u64 as *mut c_void),
                    std::mem::size_of::<u64>() as u32,
                    0,
                )
                .is_err()
            {
                return;
            }
            if context
                .GetData(
                    &self.end,
                    Some(&mut end as *mut u64 as *mut c_void),
                    std::mem::size_of::<u64>() as u32,
                    0,
                )
                .is_err()
            {
                return;
            }
        }
        if disjoint.Disjoint.as_bool() || disjoint.Frequency == 0 || end < begin {
            return;
        }
        let ticks = (end - begin) as f64;
        self.cost_us = (ticks * 1_000_000.0 / disjoint.Frequency as f64).round() as u64;
        self.armed = true;
    }

    fn start(&mut self, context: &ID3D11DeviceContext) {
        unsafe {
            context.Begin(&self.disjoint);
            context.End(&self.begin);
        }
    }

    fn stop(&mut self, context: &ID3D11DeviceContext) {
        unsafe {
            context.End(&self.end);
            context.End(&self.disjoint);
        }
        self.armed = true;
    }
}

// ---------------------------------------------------------------- pipeline

/// The whole enhancement tier for one video layer.
pub(crate) struct Enhance {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    vs: ID3D11VertexShader,
    dims: ID3D11Buffer,
    /// CPU-written frames land here (`staging -> source`); every chain reads it.
    source: Target,
    /// The warp's destination when a tier is on: a pass cannot read the
    /// backbuffer it is writing, so the warp is redirected here and the chain
    /// samples it. Built on first use (interpolation only).
    warped: Option<Target>,
    /// The luma-clamped picture: the CNN's `MAIN`.
    color: Target,
    /// The chain's intermediate targets (`RGBA16F`: the first convolution's
    /// output is signed and only rectified by the next pass, so 8-bit would
    /// destroy half the network).
    conv: Vec<Target>,
    chain: Option<(u32, Chain)>,
    timing: Timing,
    /// Frames drawn through a chain (QA).
    pub(crate) frames: u64,
    /// GPU cost of the last frame, µs (`u64::MAX` before the first read).
    pub(crate) cost_us: u64,
    /// The size this pipeline was built for.
    pub(crate) size: (u32, u32),
}

impl Enhance {
    /// Build the constant buffer and the always-needed targets, but no chain:
    /// which chain is needed is decided by the first frame's desired tier.
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let width = width.max(2);
        let height = height.max(2);
        let vs_code = super::warp::compile(ENHANCE_HLSL, "vs_main", "vs_5_0")?;
        let vs = unsafe {
            let mut vs: Option<ID3D11VertexShader> = None;
            device
                .CreateVertexShader(
                    &vs_code,
                    None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                    Some(&mut vs),
                )
                .map_err(|error| format!("CreateVertexShader hr={error}"))?;
            vs.ok_or_else(|| "CreateVertexShader returned no shader".to_string())?
        };
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
                .map_err(|error| format!("CreateBuffer hr={error}"))?;
            buffer.ok_or_else(|| "CreateBuffer returned nothing".to_string())?
        };
        let source = Target::new(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let color = Target::new(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let timing = Timing::new(device)?;
        if trace() {
            crate::debug_log(&format!(
                "sr pipeline targets={width}x{height} source=BGRA8 color=BGRA8 conv=RGBA16F"
            ));
        }
        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            width,
            height,
            vs,
            dims,
            source,
            warped: None,
            color,
            conv: Vec::new(),
            chain: None,
            timing,
            frames: 0,
            cost_us: u64::MAX,
            size: (width, height),
        })
    }

    /// The warp pass's destination when a tier is on: its output has to be
    /// readable by the chain, so it cannot be the backbuffer.
    pub(crate) fn warp_target(&mut self) -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView), String> {
        if self.warped.is_none() {
            let target = Target::new(&self.device, self.width, self.height, DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(|error| format!("warp target: {error}"))?;
            self.warped = Some(target);
        }
        let target = self.warped.as_ref().expect("just built");
        Ok((target.rtv.clone(), target.srv.clone()))
    }

    fn ensure_chain(&mut self, mode: u32) -> Result<(), String> {
        if matches!(&self.chain, Some((cached, _)) if *cached == mode) {
            return Ok(());
        }
        // Drop the old chain (and its targets) before building the new one: a
        // tier change is rare and holding two chains' worth of RGBA16F would
        // double the VRAM for a menu click.
        self.chain = None;
        self.conv.clear();
        let chain = build_chain(&self.device, mode)?;
        let wanted = chain.conv_targets();
        for index in 0..wanted {
            self.conv.push(
                Target::new(
                    &self.device,
                    self.width,
                    self.height,
                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                )
                .map_err(|error| format!("conv target {index}: {error}"))?,
            );
        }
        crate::debug_log(&format!(
            "enhance mode={mode} label={} size={}x{} conv_targets={wanted}{}",
            label(mode),
            self.width,
            self.height,
            match &chain {
                Chain::Anime4k {
                    variant, restore, ..
                } => {
                    format!(" anime4k={variant} passes={}", restore.len())
                }
                _ => String::new(),
            }
        ));
        self.chain = Some((mode, chain));
        Ok(())
    }

    fn set_dims(&self, strength: f32, con: f32) {
        let packed = [
            self.width.saturating_sub(1) as f32,
            self.height.saturating_sub(1) as f32,
            self.width as f32,
            self.height as f32,
            strength,
            con,
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

    /// One full-screen pass from `inputs` into `rtv`.
    fn pass_to(
        &self,
        ps: &ID3D11PixelShader,
        inputs: &[&ID3D11ShaderResourceView],
        rtv: &ID3D11RenderTargetView,
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
            self.context.OMSetRenderTargets(
                Some(&[Some(rtv.clone())]),
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
            );
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context
                .IASetInputLayout(None::<&windows::Win32::Graphics::Direct3D11::ID3D11InputLayout>);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(ps, None);
            self.context
                .VSSetConstantBuffers(0, Some(&[Some(self.dims.clone())]));
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.dims.clone())]));
            self.context.PSSetShaderResources(0, Some(&views));
            self.context.Draw(3, 0);
            // Unbind: a bound SRV keeps its texture busy, and the next pass
            // writes over exactly those textures.
            let empty: Vec<Option<ID3D11ShaderResourceView>> =
                inputs.iter().map(|_| None).collect();
            self.context.PSSetShaderResources(0, Some(&empty));
        }
    }

    /// Run the chain. `&self` only: the chain and every target it reads are
    /// borrowed for the whole match, which keeps the pass list statically
    /// checked against the textures it needs.
    fn draw_chain(
        &self,
        source: &ID3D11ShaderResourceView,
        back: &ID3D11RenderTargetView,
    ) -> Result<(), String> {
        let Some((_, chain)) = self.chain.as_ref() else {
            return Err("no chain".into());
        };
        match chain {
            Chain::Sharpen { cas } => {
                self.set_dims(CAS_STRENGTH, 0.0);
                self.pass_to(cas, &[source], back);
            }
            Chain::Fsr { rcas } => {
                // FsrRcasCon: con.x = exp2(-sharpness).
                self.set_dims(0.0, (-RCAS_STOPS).exp2());
                self.pass_to(rcas, &[source], back);
            }
            Chain::Anime4k {
                stats_x,
                stats_y,
                apply,
                restore,
                variant,
            } => {
                let conv = &self.conv;
                let needed = if *variant == "M" { 7 } else { 3 };
                if conv.len() < needed || restore.len() < needed + 1 {
                    return Err(format!(
                        "anime4k {variant} chain incomplete: conv={} passes={}",
                        conv.len(),
                        restore.len()
                    ));
                }
                self.set_dims(0.0, 0.0);
                // Clamp_Highlights: 5x5 max of luma in two sweeps, then clamp.
                // `clamp_stats_y` reads the first sweep through `tex_aux`, i.e.
                // t1, so the sweep is bound twice (t0 is unused there).
                self.pass_to(stats_x, &[source], &conv[0].rtv);
                self.pass_to(stats_y, &[&conv[0].srv, &conv[0].srv], &conv[1].rtv);
                self.pass_to(apply, &[source, &conv[1].srv], &self.color.rtv);
                if *variant == "M" {
                    // Restore_CNN: conv2d_tf, conv2d_1_tf .. conv2d_6_tf.
                    self.pass_to(&restore[0], &[&self.color.srv], &conv[0].rtv);
                    self.pass_to(&restore[1], &[&conv[0].srv], &conv[1].rtv);
                    for index in 2..7 {
                        self.pass_to(&restore[index], &[&conv[index - 1].srv], &conv[index].rtv);
                    }
                    // The last pass reads MAIN plus all seven convolutions.
                    self.pass_to(
                        &restore[7],
                        &[
                            &self.color.srv,
                            &conv[0].srv,
                            &conv[1].srv,
                            &conv[2].srv,
                            &conv[3].srv,
                            &conv[4].srv,
                            &conv[5].srv,
                            &conv[6].srv,
                        ],
                        back,
                    );
                } else {
                    self.pass_to(&restore[0], &[&self.color.srv], &conv[0].rtv);
                    self.pass_to(&restore[1], &[&conv[0].srv], &conv[1].rtv);
                    self.pass_to(&restore[2], &[&conv[1].srv], &conv[2].rtv);
                    self.pass_to(&restore[3], &[&self.color.srv, &conv[2].srv], back);
                }
            }
        }
        Ok(())
    }

    /// Run the tier for one frame and leave the result in `swap`'s backbuffer.
    /// The caller presents.
    ///
    /// On `Err`, the backbuffer may be partially written, so the caller falls
    /// back to the staged copy *and* retries presenting: the staged path
    /// overwrites every pixel, so the frame the user sees is correct even on the
    /// frame a tier failed.
    pub(crate) fn draw(
        &mut self,
        mode: u32,
        source: Source,
        swap: &IDXGISwapChain1,
    ) -> Result<(), String> {
        let source_view = match source {
            Source::Staging(staging) => {
                // One copy: the staging texture is already BGRA and already
                // swizzled by `Layer::stage`.
                unsafe {
                    self.context.CopyResource(&self.source.tex, staging);
                }
                self.source.srv.clone()
            }
            Source::View(view) => view,
        };
        let back_texture: ID3D11Texture2D = unsafe {
            swap.GetBuffer(0)
                .map_err(|error| format!("GetBuffer(0) hr={error}"))?
        };
        let mut back_view: Option<ID3D11RenderTargetView> = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&back_texture, None, Some(&mut back_view))
                .map_err(|error| format!("CreateRenderTargetView backbuffer hr={error}"))?;
        }
        let Some(back) = back_view else {
            return Err("no backbuffer view".to_string());
        };
        self.ensure_chain(mode)?;
        self.timing.collect(&self.context);
        self.timing.start(&self.context);
        let result = self.draw_chain(&source_view, &back);
        self.timing.stop(&self.context);
        // The backbuffer reference must be gone before `Present` (a flip model
        // swapchain refuses to present while one is held).
        unsafe {
            self.context.OMSetRenderTargets(
                None,
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
            );
        }
        drop(back);
        drop(back_texture);
        result?;
        self.frames = self.frames.saturating_add(1);
        self.cost_us = self.timing.cost_us;
        Ok(())
    }
}
