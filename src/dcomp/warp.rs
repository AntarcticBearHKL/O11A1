//! f200: the optical-flow warp as a GPU pass, straight into the video layer.
//!
//! # Why this exists
//!
//! f199 measured the mode-3 (OFA) path end to end at 1080p: the flow field is
//! produced by the driver's optical-flow engine, but everything after it ran on
//! the CPU - a full-frame warp (`ofa::warp`, ~25 MB of reads and writes per
//! display frame), then `Layer::stage` swizzling and copying the result into a
//! staging texture (another ~17 MB), then two `CopyResource`s. At 120 display
//! frames a second that is ~5 GB/s of memory traffic in and out of one core, and
//! the picture topped out around 60/s however much the pacer was tuned.
//!
//! Here the warp is three D3D11 draws of a fullscreen triangle: the two source
//! frames are already on the GPU (they are the optical-flow session's own input
//! textures), the flow field never leaves the GPU (it is the session's output
//! texture), and the result is written **directly** into the composition
//! swapchain's backbuffer. No readback, no upload, no CPU pass - the CPU only
//! issues commands.
//!
//! # What it guarantees
//!
//! * the arithmetic is `ofa::warp`'s: `a` is sampled at `p - flow * (1 - phase)`,
//!   `b` at `p + flow * phase`, then blended with the phase weights; both are
//!   point-sampled at `grid` spacing, exactly like the CPU table;
//! * the outputs are written as BGRA with alpha forced opaque, which is what the
//!   video layer's `ALPHA_MODE_IGNORE` swapchain expects;
//! * every step reports the HRESULT it failed on, because "the GPU path is not
//!   available" has to be a number in the log, not a guess.
//!
//! # Why a query and not a fence
//!
//! The OFA execute is asynchronous on the device. The D3D11 immediate context
//! orders commands issued on it, but `nvOFExecute` is not one of those commands -
//! the driver submits it on its own queue. f199's workaround was to consume the
//! field *one pair late* (submit pair i, read at the start of interval i+1),
//! which costs a pair of latency and cannot be used when the field must drive the
//! pair it came from. A `D3D11_QUERY_EVENT` inserted after `nvOFExecute` and
//! polled with `GetData(..., 0)` is the documented way to know the engine has
//! finished: [`WarpPipeline::fence`] issues it, [`WarpPipeline::wait`] spins on
//! it. Measured cost of that wait is in the sink's `warp_gate_us` counter.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::{Interface, BOOL};
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer,
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// One warp draw's parameters. Mirrors the CPU `warp` arguments.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    pub(crate) phase: f32,
    pub(crate) grid: u32,
    pub(crate) grid_w: u32,
    pub(crate) grid_h: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The source frames are RGBA and the backbuffer is BGRA.
    pub(crate) swap_rb: bool,
}

impl Params {
    fn pack(&self) -> [f32; 8] {
        [
            self.phase,
            self.grid.max(1) as f32,
            self.grid_w.max(1) as f32,
            self.grid_h.max(1) as f32,
            self.width.max(1) as f32,
            self.height.max(1) as f32,
            if self.swap_rb { 1.0 } else { 0.0 },
            0.0,
        ]
    }
}

/// The shaders, compiled once per device and cached for the process (the source
/// never changes; a second player window reuses the bytecode).
fn shader_bytecode() -> Result<(&'static [u8], &'static [u8]), String> {
    static BYTECODE: OnceLock<Result<(&'static [u8], &'static [u8]), String>> = OnceLock::new();
    BYTECODE
        .get_or_init(|| {
            let vs = compile(SHADER, "vs_main", "vs_5_0")?;
            let ps = compile(SHADER, "ps_main", "ps_5_0")?;
            Ok((Box::leak(vs.into_boxed_slice()), Box::leak(ps.into_boxed_slice())))
        })
        .clone()
}

/// The warp, in one file, with no HLSL include and no macro gymnastics.
///
/// `Load` is used everywhere on purpose: an integer texture cannot be sampled
/// with a sampler, and point addressing is exactly what the CPU table does (one
/// flow cell covers `grid`x`grid` pixels, and every pixel of a cell reads the
/// same vector).
const SHADER: &str = r#"
cbuffer Params : register(b0)
{
    // x: phase, y: grid, z: grid_w, w: grid_h
    float4 p0;
    // x: width, y: height, z: swap red/blue (RGBA source), w: unused
    float4 p1;
};

Texture2D<float4> tex_a : register(t0);
Texture2D<float4> tex_b : register(t1);
Texture2D<int2>   flow  : register(t2);

struct Varyings
{
    float4 pos : SV_Position;
};

Varyings vs_main(uint id : SV_VertexID)
{
    // One triangle that covers the viewport; no vertex buffer, no input layout.
    float2 uv = float2((id << 1) & 2, id & 2);
    Varyings o;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

float4 ps_main(Varyings input) : SV_Target
{
    int2 p = int2(input.pos.xy);
    int2 size = int2((int)p1.x, (int)p1.y);
    int grid = max((int)p0.y, 1);
    int2 cells = int2((int)p0.z, (int)p0.w);
    int2 cell = clamp(p / grid, int2(0, 0), cells - int2(1, 1));

    // S10.5 fixed point, one vector per cell - the same field the CPU warp reads.
    int2 v = flow.Load(int3(cell, 0));
    float2 delta = float2(v) / 32.0;

    float phase = saturate(p0.x);
    float mate = 1.0 - phase;
    // a is warped forward, b backward (see `ofa::warp`).
    int2 pa = clamp(p - int2(round(delta * mate)), int2(0, 0), size - int2(1, 1));
    int2 pb = clamp(p + int2(round(delta * phase)), int2(0, 0), size - int2(1, 1));

    float4 a = tex_a.Load(int3(pa, 0));
    float4 b = tex_b.Load(int3(pb, 0));
    float4 c = a * mate + b * phase;
    c.a = 1.0;
    if (p1.z > 0.5)
    {
        c = float4(c.b, c.g, c.r, c.a);
    }
    return c;
}
"#;

/// The warp pipeline for one device.
pub(crate) struct WarpPipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    params: ID3D11Buffer,
    query: ID3D11Query,
    /// Draws issued (QA).
    pub(crate) draws: u64,
    /// Time spent waiting for the flow field in the last frame, µs.
    pub(crate) gate_us: u64,
}

/// `d3dcompiler_47.dll`'s `D3DCompile`, resolved once and kept for the process.
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

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn vtbl<F: Copy>(obj: *mut c_void, index: usize) -> F {
    let table = *(obj as *const *const usize);
    std::mem::transmute_copy(&*table.add(index))
}

/// `pub(crate)` for `enhance.rs`: the enhancement tiers compile their own HLSL
/// through the same loader, so `d3dcompiler_47.dll` is resolved (and reported)
/// in exactly one place.
pub(crate) fn compile(source: &str, entry: &str, target: &str) -> Result<Vec<u8>, String> {
    static ENTRY: OnceLock<Result<usize, String>> = OnceLock::new();
    let resolved = ENTRY.get_or_init(|| {
        // SAFETY: fixed NUL-terminated module and symbol names; the module stays
        // loaded for the process (the driver's own DLLs are treated the same way).
        unsafe {
            let module = LoadLibraryW(wide("d3dcompiler_47.dll").as_ptr());
            if module.is_null() {
                return Err("d3dcompiler_47.dll missing".to_string());
            }
            match GetProcAddress(module, b"D3DCompile\0".as_ptr()) {
                Some(function) => Ok(function as usize),
                None => Err("D3DCompile entry missing".to_string()),
            }
        }
    });
    let address = resolved.as_ref().map_err(|error| error.clone())?;
    let compile: D3DCompileFn = unsafe { std::mem::transmute_copy(address) };
    let entry_c = format!("{entry}\0");
    let target_c = format!("{target}\0");
    let mut code: *mut c_void = std::ptr::null_mut();
    let mut errors: *mut c_void = std::ptr::null_mut();
    // SAFETY: the entry point fills both blob pointers on success; the source
    // outlives the call, and the blobs are released below.
    let hr = unsafe {
        compile(
            source.as_ptr() as *const c_void,
            source.len(),
            b"dcomp_warp\0".as_ptr() as *const i8,
            std::ptr::null(),
            std::ptr::null_mut(),
            entry_c.as_ptr() as *const i8,
            target_c.as_ptr() as *const i8,
            // Optimise hard: this shader runs 120 times a second at 1080p.
            1 << 15,
            0,
            &mut code,
            &mut errors,
        )
    };
    let message = if errors.is_null() {
        String::new()
    } else {
        // SAFETY: `errors` is an `ID3DBlob`; its two getters are vtable slots 3
        // and 4 (IUnknown is 0..=2).
        unsafe {
            let get_pointer: unsafe extern "system" fn(*mut c_void) -> *const c_void =
                vtbl(errors, 3);
            let get_size: unsafe extern "system" fn(*mut c_void) -> usize = vtbl(errors, 4);
            let size = get_size(errors);
            let text = std::slice::from_raw_parts(get_pointer(errors) as *const u8, size);
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = vtbl(errors, 2);
            let text = String::from_utf8_lossy(text).into_owned();
            release(errors);
            text
        }
    };
    if hr < 0 || code.is_null() {
        return Err(format!("D3DCompile {entry} hr=0x{:08X} {message}", hr as u32));
    }
    // SAFETY: `code` is an `ID3DBlob`; copy its bytes out and release it.
    let bytes = unsafe {
        let get_pointer: unsafe extern "system" fn(*mut c_void) -> *const c_void = vtbl(code, 3);
        let get_size: unsafe extern "system" fn(*mut c_void) -> usize = vtbl(code, 4);
        let size = get_size(code);
        let bytes = std::slice::from_raw_parts(get_pointer(code) as *const u8, size).to_vec();
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = vtbl(code, 2);
        release(code);
        bytes
    };
    crate::debug_log(&format!(
        "dcomp warp shader compiled entry={entry} target={target} bytes={}",
        bytes.len()
    ));
    Ok(bytes)
}

/// Whether `D3DCompile` could be resolved. Cached, no side effects: the menu
/// asks this to decide whether the shader tiers can be offered at all.
pub(crate) fn compiler_available() -> Result<(), String> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            compile(
                "// f202: probe\ntexture2D t0;\nfloat4 probe(float4 p : SV_Position) : SV_Target { return 0; }\n",
                "probe",
                "ps_5_0",
            )
            .map(|_| ())
            .map_err(|error| format!("D3DCompile unavailable: {error}"))
        })
        .clone()
}

impl WarpPipeline {
    /// Build the pipeline on `device`/`context`.
    ///
    /// Both must be the ones the sink presents with: the pipeline writes the
    /// swapchain backbuffer, so it has to run on the device that owns it.
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self, String> {
        let (vs_code, ps_code) = shader_bytecode()?;
        unsafe {
            let mut vs: Option<ID3D11VertexShader> = None;
            device
                .CreateVertexShader(
                    vs_code,
                    None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                    Some(&mut vs),
                )
                .map_err(|error| format!("CreateVertexShader hr={error}"))?;
            let mut ps: Option<ID3D11PixelShader> = None;
            device
                .CreatePixelShader(
                    ps_code,
                    None::<&windows::Win32::Graphics::Direct3D11::ID3D11ClassLinkage>,
                    Some(&mut ps),
                )
                .map_err(|error| format!("CreatePixelShader hr={error}"))?;
            let params = [0f32; 8];
            let description = D3D11_BUFFER_DESC {
                ByteWidth: 32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let initial = D3D11_SUBRESOURCE_DATA {
                pSysMem: params.as_ptr() as *const c_void,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut buffer: Option<ID3D11Buffer> = None;
            device
                .CreateBuffer(&description, Some(&initial), Some(&mut buffer))
                .map_err(|error| format!("CreateBuffer hr={error}"))?;
            let query: Option<ID3D11Query> = {
                let mut query: Option<ID3D11Query> = None;
                device
                    .CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: D3D11_QUERY_EVENT,
                            MiscFlags: 0,
                        },
                        Some(&mut query),
                    )
                    .map_err(|error| format!("CreateQuery hr={error}"))?;
                query
            };
            let (Some(vs), Some(ps), Some(buffer), Some(query)) = (vs, ps, buffer, query) else {
                return Err("shader creation returned no object".into());
            };
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vs,
                ps,
                params: buffer,
                query,
                draws: 0,
                gate_us: 0,
            })
        }
    }

    /// Adopt a shader-resource view that the optical-flow session created.
    ///
    /// The pointer's reference is transferred to this pipeline, which releases it
    /// when the session is replaced. Nothing is created here on purpose: see the
    /// note in `ofa::fill_session` (reconstructing a `windows` interface over a
    /// raw pointer and calling through it faults on this machine).
    pub(crate) fn adopt(
        view: *mut c_void,
    ) -> Result<ID3D11ShaderResourceView, String> {
        if view.is_null() {
            return Err("null shader resource view".into());
        }
        // SAFETY: the caller created the view on this device and handed the
        // reference over; nothing else releases it.
        Ok(unsafe { ID3D11ShaderResourceView::from_raw(view) })
    }

    /// Insert the "the optical-flow engine is done" event. Call right after
    /// `nvOFExecute`.
    pub(crate) fn fence(&self) {
        unsafe {
            self.context.End(&self.query);
        }
    }

    /// Wait for the event [`Self::fence`] inserted, up to `timeout_ms`.
    ///
    /// `GetData` with no flush flags returns `S_FALSE` while the event is still
    /// pending; that is the only way to poll it without stalling the device.
    /// Returns whether it completed (and records the wait in `gate_us`).
    pub(crate) fn wait(&mut self, timeout_ms: u64) -> bool {
        let started = Instant::now();
        let mut done = false;
        loop {
            let mut value: BOOL = BOOL(0);
            // SAFETY: the query is this device's, and the buffer is a BOOL for a
            // D3D11_QUERY_EVENT.
            let result = unsafe {
                self.context.GetData(
                    &self.query,
                    Some(&mut value as *mut BOOL as *mut c_void),
                    std::mem::size_of::<BOOL>() as u32,
                    0,
                )
            };
            if result.is_ok() {
                done = true;
                break;
            }
            if started.elapsed().as_millis() as u64 >= timeout_ms {
                break;
            }
            std::hint::spin_loop();
        }
        self.gate_us = started.elapsed().as_micros() as u64;
        done
    }

    /// Draw one warped frame into `swap`'s current backbuffer.
    ///
    /// f202: when a 画质增强 tier is on, the caller uses [`Self::draw_into`] with
    /// the tier's render target instead, because a pass cannot read the
    /// backbuffer it is writing.
    pub(crate) fn draw(
        &mut self,
        swap: &IDXGISwapChain1,
        a: &ID3D11ShaderResourceView,
        b: &ID3D11ShaderResourceView,
        flow: &ID3D11ShaderResourceView,
        params: Params,
    ) -> Result<(), String> {
        // SAFETY: every object below belongs to the device the sink presents on.
        let back: ID3D11Texture2D = unsafe {
            swap.GetBuffer(0)
                .map_err(|error| format!("GetBuffer(0) hr={error}"))?
        };
        if super::warp_trace() {
            crate::debug_log("dcomp warp draw backbuffer ok");
        }
        // The view is created per frame and dropped before `Present`: a flip
        // model swapchain refuses to present while the application still
        // holds a reference to the backbuffer (`DXGI_ERROR_INVALID_CALL`), and
        // an `ID3D11RenderTargetView` *is* such a reference. Creating one is
        // a few microseconds; a broken present is a black picture.
        let mut view: Option<ID3D11RenderTargetView> = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&back, None, Some(&mut view))
                .map_err(|error| format!("CreateRenderTargetView hr={error}"))?;
        }
        let Some(rtv) = view else {
            return Err("no render target view".into());
        };
        self.draw_into(&rtv, a, b, flow, params)
    }

    /// Draw one warped frame into `rtv` (the backbuffer, or an enhancement
    /// target of the same size).
    pub(crate) fn draw_into(
        &mut self,
        rtv: &ID3D11RenderTargetView,
        a: &ID3D11ShaderResourceView,
        b: &ID3D11ShaderResourceView,
        flow: &ID3D11ShaderResourceView,
        params: Params,
    ) -> Result<(), String> {
        // SAFETY: every object below belongs to the device the sink presents on.
        unsafe {
            let packed = params.pack();
            self.context.UpdateSubresource(
                &self.params,
                0,
                None,
                packed.as_ptr() as *const c_void,
                0,
                0,
            );
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: params.width as f32,
                Height: params.height as f32,
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
            self.context.VSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            self.context.PSSetShaderResources(
                0,
                Some(&[Some(a.clone()), Some(b.clone()), Some(flow.clone())]),
            );
            if super::warp_trace() {
                crate::debug_log("dcomp warp draw state ok");
            }
            self.context.Draw(3, 0);
            if super::warp_trace() {
                crate::debug_log("dcomp warp draw issued");
            }
            // Unbind the views: a bound SRV keeps the source texture busy, and
            // the optical-flow engine writes those textures on the next pair.
            self.context.PSSetShaderResources(0, Some(&[None, None, None]));
            self.context.OMSetRenderTargets(
                None,
                None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
            );
            self.draws = self.draws.saturating_add(1);
        }
        Ok(())
    }
}
