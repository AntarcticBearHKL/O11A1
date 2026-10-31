//! D3D11 + DXGI + DirectComposition, as measured by `examples/dcomp_proto.rs`.
//!
//! Everything in this file is a straight port of the prototype's verified parts;
//! the comments record which decision came from which measurement so the next
//! reader does not have to re-run the spike:
//!
//! * a window with `WS_EX_NOREDIRECTIONBITMAP` plus a
//!   `DCompositionCreateDevice` -> `CreateTargetForHwnd` tree is the only thing
//!   that can show content in it;
//! * composition swapchains are `FLIP_SEQUENTIAL`, 2 buffers, frame-latency
//!   waitable, latency 1. The video layer is `ALPHA_MODE_IGNORE`, the UI layer
//!   `ALPHA_MODE_PREMULTIPLIED`;
//! * D3D11 on this machine refuses `USAGE_DYNAMIC + CPU_ACCESS_WRITE`
//!   (`E_INVALIDARG` for every size), so uploads are `STAGING` + `Map` +
//!   `CopyResource`;
//! * the CPU copy (`stage`) happens **before** the compositor wait. Doing it
//!   after is the 86 -> 120 fps difference;
//! * the RGBA -> BGRA swizzle is fused into the upload and never touches alpha:
//!   the UI layer's premultiplied alpha has to survive it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use windows::core::{Interface, IUnknown, BOOL};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_ADAPTER_DESC1, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT,
    DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory2,
    IDXGIOutput, IDXGISwapChain1, IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObject;

/// One D3D11 device with the DXGI factory that created it.
///
/// Two of these exist in the process (one for the UI layer, one for the video
/// layer): a D3D11 immediate context is not thread-safe, and the two layers
/// present on different threads. The prototype measured this exact shape.
pub(crate) struct Gpu {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
    pub(crate) factory: IDXGIFactory2,
}

impl Gpu {
    pub(crate) fn new() -> windows::core::Result<Self> {
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            // Prefer the adapter that drives a desktop output, then the one with
            // more dedicated memory: a video layer on the other adapter of a
            // hybrid machine would be composed through a cross-adapter copy on
            // every frame.
            let mut best: Option<(IDXGIAdapter1, DXGI_ADAPTER_DESC1, bool)> = None;
            let mut index = 0;
            let mut notes = Vec::new();
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                index += 1;
                let Ok(description) = adapter.GetDesc1() else {
                    continue;
                };
                let name: String = description
                    .Description
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
                    .collect();
                let attached = match adapter.EnumOutputs(0) {
                    Ok(output) => output
                        .GetDesc()
                        .map(|d| d.AttachedToDesktop.as_bool())
                        .unwrap_or(false),
                    Err(_) => false,
                };
                notes.push(format!(
                    "{name} ({:.0} MB{})",
                    description.DedicatedVideoMemory as f64 / (1024.0 * 1024.0),
                    if attached { ", desktop" } else { "" }
                ));
                let better = match &best {
                    None => true,
                    Some((_, current, current_attached)) => {
                        (attached && !*current_attached)
                            || (attached == *current_attached
                                && description.DedicatedVideoMemory
                                    > current.DedicatedVideoMemory)
                    }
                };
                if better {
                    best = Some((adapter, description, attached));
                }
            }
            let Some((adapter, description, _)) = best else {
                return Err(windows::core::Error::from_thread());
            };
            let adapter_base: IDXGIAdapter = adapter.cast()?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            // f209: `VIDEO_SUPPORT` is what MF's DXGI device manager insists on
            // (`IMFDXGIDeviceManager::ResetDevice` fails without it) and what the
            // video processor needs; the zero-copy decode path lives on this
            // device, so both layers get it. `BGRA_SUPPORT` stays for DComp and
            // the D3D11on12 consumers.
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
            D3D11CreateDevice(
                Some(&adapter_base),
                // With an explicit adapter, D3D11 only accepts UNKNOWN.
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                7, // D3D11_SDK_VERSION
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
            let name: String = description
                .Description
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
                .collect();
            crate::debug_log(&format!(
                "dcomp gpu adapter={} candidates={}",
                name.trim(),
                notes.join(" | ")
            ));
            // f210: start the 1 Hz device / upload / video-memory trace here,
            // once per process, because this is the only place that has the
            // factory the memory query needs.
            super::f210::register_adapter(&factory);
            Ok(Self {
                device: device.ok_or_else(windows::core::Error::from_thread)?,
                context: context.ok_or_else(windows::core::Error::from_thread)?,
                factory,
            })
        }
    }
}

/// One composition swapchain plus the staging texture its frames are written
/// into and the frame-latency waitable that paces presentation.
pub(crate) struct Layer {
    pub(crate) swap: IDXGISwapChain1,
    context: ID3D11DeviceContext,
    /// Kept for the f210 trace's `GetDeviceRemovedReason` (a device-level query,
    /// thread-safe by contract) and so a role change does not need the `Gpu`.
    device: ID3D11Device,
    /// Which of the three composition layers this is, for the trace counters.
    /// [`super::f210::UI`] by default (the danmaku layer shares the UI device
    /// and overrides it after construction).
    role: usize,
    /// Kept as an integer so the layer can move to the presenting thread
    /// (`HANDLE` is a raw pointer and is not `Send`).
    waiter: isize,
    staging: ID3D11Texture2D,
    texture: ID3D11Texture2D,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// The alpha mode decides what the compositor does with the layer's alpha.
mod alpha {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE, DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED,
    };

    /// The video layer: opaque, drawn under the UI layer.
    pub(crate) const VIDEO: DXGI_ALPHA_MODE = DXGI_ALPHA_MODE_IGNORE;
    /// The Slint UI layer: premultiplied, composited over the video.
    pub(crate) const UI: DXGI_ALPHA_MODE = DXGI_ALPHA_MODE_PREMULTIPLIED;
}

pub(crate) use self::alpha::{UI, VIDEO};

/// Fresh `B8G8R8A8_UNORM` texture with the given usage.
unsafe fn texture(
    gpu: &Gpu,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    cpu: u32,
) -> windows::core::Result<ID3D11Texture2D> {
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
        Usage: usage,
        BindFlags: 0,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut slot: Option<ID3D11Texture2D> = None;
    gpu.device.CreateTexture2D(&description, None, Some(&mut slot))?;
    slot.ok_or_else(windows::core::Error::from_thread)
}

impl Layer {
    pub(crate) fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        alpha: DXGI_ALPHA_MODE,
    ) -> windows::core::Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        unsafe {
            let description = DXGI_SWAP_CHAIN_DESC1 {
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
                // The waitable turns "present as fast as possible and lose half
                // the frames to the compositor" into one present per composition
                // interval.
                Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            };
            let unknown: IUnknown = gpu.device.cast()?;
            let swap = gpu
                .factory
                .CreateSwapChainForComposition(&unknown, &description, None::<&IDXGIOutput>)?;
            if let Ok(actual) = swap.GetDesc1() {
                crate::debug_log(&format!(
                    "dcomp swapchain asked={width}x{height} got={}x{} alpha={:?}",
                    actual.Width, actual.Height, alpha
                ));
            }
            let swap2: IDXGISwapChain2 = swap.cast()?;
            swap2.SetMaximumFrameLatency(1)?;
            let waiter = swap2.GetFrameLatencyWaitableObject().0 as isize;
            let layer = Self {
                swap,
                context: gpu.context.clone(),
                device: gpu.device.clone(),
                role: if alpha.0 == VIDEO.0 {
                    super::f210::VIDEO
                } else {
                    super::f210::UI
                },
                waiter,
                staging: texture(gpu, width, height, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32)?,
                texture: texture(gpu, width, height, D3D11_USAGE_DEFAULT, 0)?,
                width,
                height,
            };
            layer.trace_born();
            Ok(layer)
        }
    }

    /// f210: name the layer for the trace (the danmaku layer shares the UI
    /// device, so its role cannot be derived from the alpha mode).
    pub(crate) fn set_role(&mut self, role: usize) {
        self.role = role;
        self.trace_born();
    }

    fn trace_born(&self) {
        super::f210::note_layer(
            self.role,
            windows::core::Interface::as_raw(&self.device) as usize,
            windows::core::Interface::as_raw(&self.context) as usize,
            self.width,
            self.height,
        );
    }

    /// Resize the swapchain in place. The compositor keeps the same object, so
    /// the visual that references it does not have to be touched.
    pub(crate) fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) -> windows::core::Result<bool> {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return Ok(false);
        }
        unsafe {
            self.swap.ResizeBuffers(
                2,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )?;
            let swap2: IDXGISwapChain2 = self.swap.cast()?;
            let _ = swap2.SetMaximumFrameLatency(1);
            self.staging = texture(
                gpu,
                width,
                height,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_WRITE.0 as u32,
            )?;
            self.texture = texture(gpu, width, height, D3D11_USAGE_DEFAULT, 0)?;
            self.width = width;
            self.height = height;
        }
        super::f210::note_resize(self.role, width, height);
        Ok(true)
    }

    /// Copy one frame into the staging texture.
    ///
    /// `src` is RGBA when `swizzle` is set (the pacer's frames and the software
    /// renderer's premultiplied buffer), BGRA otherwise. The swizzle is fused
    /// into the copy: doing it in two passes doubles the memory traffic and
    /// costs ~4 ms at 1920x1080.
    pub(crate) fn stage(&self, src: &[u8], swizzle: bool) -> windows::core::Result<f64> {
        let started = Instant::now();
        let row = self.width as usize * 4;
        debug_assert!(src.len() >= row * self.height as usize);
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData as *mut u8;
            for y in 0..self.height as usize {
                let source = src.as_ptr().add(y * row);
                let target = dst.add(y * pitch);
                if swizzle {
                    swap_rb_ptr(source, target, row);
                } else {
                    std::ptr::copy_nonoverlapping(source, target, row);
                }
            }
            self.context.Unmap(&self.staging, 0);
        }
        super::f210::note_map(
            self.role,
            (self.width as u64) * 4 * self.height as u64,
        );
        Ok(started.elapsed().as_secs_f64() * 1000.0)
    }

    /// Copy one rectangle of `src` into the staging texture.
    ///
    /// `src` is RGBA with `stride_px` pixels per row. Only the damaged rows are
    /// touched, which is what keeps a mostly-static 1080p UI at a fraction of a
    /// millisecond per repaint instead of a full 8 MB pass.
    pub(crate) fn stage_rect(
        &self,
        src: &[u8],
        stride_px: usize,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> windows::core::Result<()> {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let width = width.min(self.width - x);
        let height = height.min(self.height - y);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let src_row = stride_px * 4;
        let row_bytes = width as usize * 4;
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData as *mut u8;
            for row in y..y + height {
                let source = src
                    .as_ptr()
                    .add(row as usize * src_row + x as usize * 4);
                let target = dst.add(row as usize * pitch + x as usize * 4);
                swap_rb_ptr(source, target, row_bytes);
            }
            self.context.Unmap(&self.staging, 0);
        }
        super::f210::note_map(self.role, row_bytes as u64 * height as u64);
        Ok(())
    }

    /// Wait for the compositor, hand the staged frame to the GPU, present it.
    ///
    /// The order matters: the CPU work (`stage`) must already have happened, and
    /// the copy is issued *after* the wait so it lands in the buffer the
    /// compositor is about to take.
    pub(crate) fn submit(&self) -> windows::core::Result<windows::core::HRESULT> {
        wait(self.waiter, 200);
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.context.CopyResource(&self.texture, &self.staging);
            self.context.CopyResource(&back, &self.texture);
            super::f210::note_copy(self.role, 2);
        }
        Ok(self.present(DXGI_PRESENT_DO_NOT_WAIT))
    }

    /// Present without waiting for the compositor: the UI layer must never
    /// stall the video layer, and a busy compositor is allowed to drop a UI
    /// frame.
    ///
    /// f196 note: this is the only path the UI and danmaku layers use. Waiting
    /// on the frame-latency object first (`submit_within`, tried and reverted)
    /// **stopped the layer from reaching the screen at all** - the wait
    /// consumed the signal and the following `Present(DO_NOT_WAIT)` then had
    /// nothing to hand over, so the painted UI never appeared. Measured, not
    /// assumed: with 97 comments in the model and a 800x319 damage rect per
    /// paint, the window showed none of them.
    pub(crate) fn pulse(&self) -> windows::core::Result<windows::core::HRESULT> {
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.context.CopyResource(&self.texture, &self.staging);
            self.context.CopyResource(&back, &self.texture);
            super::f210::note_copy(self.role, 2);
        }
        Ok(self.present(DXGI_PRESENT_DO_NOT_WAIT))
    }

    /// f200: present a backbuffer that was drawn by someone else (the warp
    /// pipeline). No staging copy - the pixels are already in the swapchain.
    pub(crate) fn present_backbuffer(
        &self,
    ) -> windows::core::Result<windows::core::HRESULT> {
        Ok(self.present(DXGI_PRESENT_DO_NOT_WAIT))
    }

    /// f202: the staging texture the presenting thread wrote this frame's CPU
    /// copy into. The 画质增强 chain copies it into its own shader-readable
    /// texture; nothing else reads it.
    pub(crate) fn staging(&self) -> &ID3D11Texture2D {
        &self.staging
    }

    /// f202: the compositor wait `submit` performs, exposed so the enhanced path
    /// can keep the same order (CPU copy first, wait, then the GPU work that
    /// lands in the buffer about to be presented) without the two copies.
    pub(crate) fn wait_frame(&self) -> bool {
        wait(self.waiter, 200)
    }

    fn present(&self, flags: DXGI_PRESENT) -> windows::core::HRESULT {
        let hr = unsafe { self.swap.Present(0, flags) };
        if super::f210::enabled() {
            super::f210::note_present(self.role, hr.0);
            // `GetDeviceRemovedReason` is a device-level query with no GPU work
            // behind it, but it is still only worth one call a second: the
            // presenting thread must not pay for the trace.
            static TICK: AtomicU64 = AtomicU64::new(0);
            let n = TICK.fetch_add(1, Ordering::Relaxed);
            if n % 120 == 0 {
                let reason = unsafe { self.device.GetDeviceRemovedReason() };
                super::f210::note_removed(
                    self.role,
                    match reason {
                        Ok(()) => 0,
                        Err(error) => error.code().0,
                    },
                );
            }
        }
        hr
    }
}

/// Wait on a frame-latency waitable; returns whether it signalled.
pub(crate) fn wait(waiter: isize, timeout_ms: u32) -> bool {
    unsafe { WaitForSingleObject(HANDLE(waiter as *mut c_void), timeout_ms) == WAIT_OBJECT_0 }
}

/// The composition target for one HWND: device, target and root visual.
pub(crate) struct Composition {
    pub(crate) device: IDCompositionDevice,
    target: IDCompositionTarget,
    pub(crate) root: IDCompositionVisual,
}

impl Composition {
    /// Build the tree for `hwnd` from the layers, bottom first, and hand back
    /// one visual per layer (in the same order) so the caller can place and
    /// clip them.
    ///
    /// The device comes from the *UI* GPU in the prototype and works for
    /// swapchains from another device too: DWM composites both, which is why
    /// the two layers can live on two threads with two devices.
    pub(crate) fn new(
        hwnd: isize,
        gpu: &Gpu,
        layers: &[&Layer],
    ) -> windows::core::Result<(Self, Vec<IDCompositionVisual>)> {
        unsafe {
            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let device: IDCompositionDevice = DCompositionCreateDevice(Some(&dxgi))?;
            let target = device.CreateTargetForHwnd(windows::Win32::Foundation::HWND(hwnd as *mut c_void), true)?;
            let root = device.CreateVisual()?;
            target.SetRoot(&root)?;
            let mut previous: Option<IDCompositionVisual> = None;
            let mut visuals = Vec::with_capacity(layers.len());
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
                visuals.push(visual.clone());
                previous = Some(visual);
            }
            device.Commit()?;
            Ok((
                Self {
                    device,
                    target,
                    root,
                },
                visuals,
            ))
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

/// RGBA -> BGRA, four pixels per iteration (SSE2, baseline on x86_64).
///
/// Alpha is carried through untouched: the video layer's frames are opaque
/// anyway, and the UI layer's premultiplied alpha must survive.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The swizzle must be a pure R/B exchange: alpha is what makes the UI
    /// layer composite correctly and the prototype asserted per-channel deltas
    /// against a prediction.
    #[test]
    fn swizzle_preserves_alpha() {
        let source: Vec<u8> = (0..64u8).collect();
        let mut target = vec![0u8; source.len()];
        unsafe { swap_rb_ptr(source.as_ptr(), target.as_mut_ptr(), source.len()) };
        for (index, pixel) in source.chunks_exact(4).enumerate() {
            let out = &target[index * 4..index * 4 + 4];
            assert_eq!(out[0], pixel[2]);
            assert_eq!(out[1], pixel[1]);
            assert_eq!(out[2], pixel[0]);
            assert_eq!(out[3], pixel[3]);
        }
    }
}
