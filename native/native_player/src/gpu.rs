//! D3D11 device, the two composition swapchains and the DirectComposition tree.
//!
//! The same recipe the f192 prototype proved on this machine:
//! `WS_EX_NOREDIRECTIONBITMAP` window, `DCompositionCreateDevice` +
//! `CreateTargetForHwnd`, a video swapchain (`DXGI_ALPHA_MODE_IGNORE`,
//! `FLIP_SEQUENTIAL`, 2 buffers, frame-latency waitable) below a UI swapchain
//! (`DXGI_ALPHA_MODE_PREMULTIPLIED`).

use std::ffi::c_void;

use anyhow::{anyhow, Result};
use windows::core::{Interface, IUnknown, BOOL};
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
    ID3D11Texture2D, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE, DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1, IDXGISwapChain2,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::{WaitForSingleObject, THREAD_PRIORITY_HIGHEST};
use windows_numerics::Matrix3x2;

use crate::enhance::{EnhanceMode, EnhancePipeline};
use crate::log;

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
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
            let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
            let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
            // The pacer thread does a full-frame CPU copy per present; highest
            // priority keeps it from being preempted by ordinary UI work.
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
            let swap = self
                .factory
                .CreateSwapChainForComposition(&unknown, &description, None::<&IDXGIOutput>)?;
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

/// The video layer: composition swapchain + the staging texture a BGRA frame is
/// written into. Frames are always the decoded picture size; DirectComposition
/// scales them into the window.
pub struct VideoSurface {
    pub swap: IDXGISwapChain1,
    pub waiter: isize,
    pub staging: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub presents: u64,
    failures: u64,
}

impl VideoSurface {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        let (swap, waiter) = gpu.create_swapchain(width, height, DXGI_ALPHA_MODE_IGNORE, true)?;
        let staging = unsafe {
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
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            gpu.device.CreateTexture2D(&description, None, Some(&mut texture))?;
            texture.ok_or_else(|| anyhow!("video staging texture missing"))?
        };
        log::line(&format!("video surface swapchain={width}x{height} flip-seq alpha=ignore waitable=1"));
        Ok(Self {
            swap,
            waiter,
            staging,
            width,
            height,
            presents: 0,
            failures: 0,
        })
    }

    pub fn wait_frame(&self) -> bool {
        let result = unsafe { WaitForSingleObject(HANDLE(self.waiter as *mut c_void), 200) };
        result == WAIT_OBJECT_0
    }

    /// Upload one opaque BGRA frame into the staging texture. The caller has
    /// already converted NV12 into exactly `width*height*4` bytes.
    pub fn stage(&self, gpu: &Gpu, bgra: &[u8]) -> Result<()> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() < expected {
            return Err(anyhow!(
                "video frame {} bytes, expected {expected}",
                bgra.len()
            ));
        }
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gpu.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let row = self.width as usize * 4;
            let destination = mapped.pData as *mut u8;
            for y in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    bgra.as_ptr().add(y * row),
                    destination.add(y * pitch),
                    row,
                );
            }
            gpu.context.Unmap(&self.staging, 0);
        }
        Ok(())
    }

    /// Returns `(enhanced, presented)`. `enhanced` is false when an enhancement
    /// pass failed and the plain copy was used (the caller then switches its
    /// mode to Off); `presented` counts a real `S_OK` from `Present` only.
    ///
    /// `brightness` is applied by the video pixel shader, after the optional
    /// enhance chain, so the UI and danmaku layers are never affected.
    pub fn present(
        &mut self,
        gpu: &Gpu,
        enhance: &mut Option<EnhancePipeline>,
        mode: &mut EnhanceMode,
        brightness: f32,
    ) -> Result<(bool, bool)> {
        // Wait until the compositor has room for this frame. The upload has
        // already happened (`stage`), so the copy is overlapped with the
        // previous composition interval.
        let signaled = self.wait_frame();
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0)? };
        let brightness = brightness.clamp(0.4, 2.0);
        let mut enhanced = false;
        if *mode != EnhanceMode::Off {
            let result = (|| -> Result<()> {
                let pipeline = enhance
                    .as_mut()
                    .ok_or_else(|| anyhow!("enhance pipeline unavailable"))?;
                let mut view: Option<ID3D11RenderTargetView> = None;
                unsafe {
                    gpu.device
                        .CreateRenderTargetView(&back, None, Some(&mut view))?;
                }
                let view = view.ok_or_else(|| anyhow!("no backbuffer RTV"))?;
                pipeline.draw(*mode, brightness, &self.staging, &view)
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
        if !enhanced && (brightness - 1.0).abs() > 0.0005 {
            if let Some(pipeline) = enhance.as_mut() {
                let result = (|| -> Result<()> {
                    let mut view: Option<ID3D11RenderTargetView> = None;
                    unsafe {
                        gpu.device
                            .CreateRenderTargetView(&back, None, Some(&mut view))?;
                    }
                    let view = view.ok_or_else(|| anyhow!("no backbuffer RTV"))?;
                    pipeline.draw_brightness(&self.staging, &view, brightness)
                })();
                match result {
                    Ok(()) => enhanced = true,
                    Err(error) => log::line(&format!(
                        "brightness fallback brightness={brightness:.2} reason={error:#}"
                    )),
                }
            }
        }
        if !enhanced {
            unsafe {
                gpu.context.CopyResource(&back, &self.staging);
            }
        }
        unsafe {
            gpu.context.OMSetRenderTargets(None, None);
        }
        // Flip-model swapchains refuse Present while a reference to the back
        // buffer is held; release it first.
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
                    hr.0 as u32, self.failures, u8::from(signaled)
                ));
            }
            Ok(false)
        }
    }
}

/// DirectComposition device, target and the two visuals.
pub struct Composition {
    device: IDCompositionDevice,
    target: IDCompositionTarget,
    root: IDCompositionVisual,
    pub video_visual: IDCompositionVisual,
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
            })
        }
    }

    /// Scale the decoded picture into the window with independent axes and
    /// place its top-left at (`x`, `y`) in window pixels. Fill mode uses a
    /// uniform scale larger than the client; the root clip crops the overflow.
    pub fn set_video_transform(
        &self,
        x: f32,
        y: f32,
        scale_x: f32,
        scale_y: f32,
    ) -> Result<()> {
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
    /// changes the decoded picture size rebuilds the swapchain). The rest of the
    /// composition tree is untouched, so the UI/danmaku layers keep their order.
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
    unsafe { swap.Present(0, DXGI_PRESENT_DO_NOT_WAIT).0 == 0 }
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
            ((cw - vw * scale) * 0.5, (ch - vh * scale) * 0.5, scale, scale)
        }
        AspectMode::Fill => {
            let scale = (cw / vw).max(ch / vh).max(0.0001);
            ((cw - vw * scale) * 0.5, (ch - vh * scale) * 0.5, scale, scale)
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
}
