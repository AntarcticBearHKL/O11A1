//! f208: the Media Foundation decoder's test face.
//!
//! The decoder is not wired into the player yet, so nothing here is reachable
//! from a `BILI_*` hook; these are ordinary `cargo test` cases. What can be
//! tested without a B 站 URL is the parts that decide *where* bytes come from
//! (the `Seek` arithmetic and the `Content-Range` parse) plus the one process
//! wide side effect (`MFStartup`). The end-to-end path - real DASH URL, real
//! hardware decoder - is `examples/mf_probe.rs`, driven by
//! `scripts/qa-f208-mf.ps1`, because it needs the network and a GPU.

#[cfg(test)]
mod tests {
    use crate::mediadec::mf::{self, SEEK_BEGIN, SEEK_CURRENT, SEEK_END};

    #[test]
    fn mf_startup_is_idempotent() {
        // One `OnceLock<Result<..>>`: the second call must return the cached
        // answer (a second MFStartup is not an error, but it must not happen).
        assert_eq!(mf::startup(), Ok(()));
        assert_eq!(mf::startup(), Ok(()));
    }

    #[test]
    fn content_range_total_is_read_from_both_forms() {
        // A satisfied range reports `start-end/total`.
        assert_eq!(
            mf::parse_content_range_total("bytes 0-1048575/123456789"),
            Some(123_456_789)
        );
        // A 416 reports `*/total`, which is how the length of a resource is
        // learnt when the requested window starts past the end.
        assert_eq!(
            mf::parse_content_range_total("bytes */8388608"),
            Some(8_388_608)
        );
        // No total (a range on an unknown-length resource) is not a length.
        assert_eq!(mf::parse_content_range_total("bytes 0-99/*"), None);
        assert_eq!(mf::parse_content_range_total("bytes 0-99/"), None);
        assert_eq!(mf::parse_content_range_total("garbage"), None);
        // Whitespace around the total is tolerated (header folding).
        assert_eq!(
            mf::parse_content_range_total("bytes 0-0/ 4096 "),
            Some(4096)
        );
    }

    #[test]
    fn seek_origins_resolve_to_absolute_positions() {
        // From the beginning the offset is the position itself.
        assert_eq!(mf::seek_target(SEEK_BEGIN, 512, Some(4096), 0), Some(0));
        assert_eq!(mf::seek_target(SEEK_BEGIN, 512, Some(4096), 100), Some(100));
        // Relative seeks go back and forth from the current position.
        assert_eq!(
            mf::seek_target(SEEK_CURRENT, 512, Some(4096), 256),
            Some(768)
        );
        assert_eq!(
            mf::seek_target(SEEK_CURRENT, 512, Some(4096), -512),
            Some(0)
        );
        // Before the start is refused, not clamped: MF would otherwise read a
        // wrapped u64 offset.
        assert_eq!(mf::seek_target(SEEK_CURRENT, 512, Some(4096), -513), None);
        // From the end needs the length, which is why the byte stream asks for
        // it first.
        assert_eq!(mf::seek_target(SEEK_END, 0, Some(4096), -1024), Some(3072));
        assert_eq!(mf::seek_target(SEEK_END, 0, None, 0), None);
        // Seeking exactly to the end is legal (that position reads EOF).
        assert_eq!(mf::seek_target(SEEK_END, 0, Some(4096), 0), Some(4096));
    }

    /// f209: can an NV12 decoder surface be read by a shader at all, and how?
    ///
    /// Run by hand against the QA sample:
    ///
    /// ```text
    /// BILI_QA_MF_PATH=target/f209/sample1080p30.mp4 \
    ///   cargo test --release nv12_probe -- --ignored --nocapture
    /// ```
    ///
    /// D3D11 has no `PlaneSlice` (that is a D3D12 SRV field), so the three
    /// candidates are: a `R8_UNORM` view of the luma plane, a `R8G8_UNORM` view
    /// of the chroma plane, and a plain `NV12` view. Which one the driver
    /// accepts decides whether the sink converts in a shader or through the
    /// video processor, so it is measured rather than assumed.
    #[test]
    #[ignore = "needs a GPU and BILI_QA_MF_PATH"]
    fn nv12_probe() {
        let Ok(path) = std::env::var("BILI_QA_MF_PATH") else {
            return;
        };
        let path = std::path::Path::new(&path);
        let (device, context) = probe_device().expect("d3d11 device");
        let mut decoder = mf::MfVideoDecoder::open_path(path, &device, &context)
            .expect("mf open_path");
        let frame = decoder
            .next_frame(std::time::Duration::from_secs(10))
            .expect("next_frame")
            .expect("frame");
        use windows::Win32::Graphics::Direct3D::{
            D3D_SRV_DIMENSION_TEXTURE2D, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
        };
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11ShaderResourceView, D3D11_SHADER_RESOURCE_VIEW_DESC,
            D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_ARRAY_SRV, D3D11_TEX2D_SRV,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
        };
        let formats = [
            ("NV12", DXGI_FORMAT_NV12),
            ("R8", DXGI_FORMAT_R8_UNORM),
            ("R8G8", DXGI_FORMAT_R8G8_UNORM),
        ];
        for (name, format) in formats {
            for array in [false, true] {
                let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: if array {
                        D3D_SRV_DIMENSION_TEXTURE2DARRAY
                    } else {
                        D3D_SRV_DIMENSION_TEXTURE2D
                    },
                    Anonymous: if array {
                        D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                                MostDetailedMip: 0,
                                MipLevels: 1,
                                FirstArraySlice: frame.subresource,
                                ArraySize: 1,
                            },
                        }
                    } else {
                        D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: 1,
                            },
                        }
                    },
                };
                let mut view: Option<ID3D11ShaderResourceView> = None;
                let hr = unsafe {
                    device.CreateShaderResourceView(&frame.texture, Some(&desc), Some(&mut view))
                };
                eprintln!(
                    "nv12 probe srv format={name} array={} subresource={} hr={:?} created={}",
                    u8::from(array),
                    frame.subresource,
                    hr,
                    view.is_some()
                );
            }
        }
        let texture = (frame.width, frame.height, frame.texture_width, frame.texture_height);
        eprintln!("nv12 probe texture={texture:?}");
    }

    fn probe_device() -> Option<(
        windows::Win32::Graphics::Direct3D11::ID3D11Device,
        windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    )> {
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            D3D11_SDK_VERSION,
        };
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .ok()?;
        }
        Some((device?, context?))
    }

    /// f209: the video-processor conversion, checked against a CPU NV12 -> RGBA
    /// conversion of the **same** decoder surface.
    ///
    /// ```text
    /// BILI_QA_MF_PATH=target/f209/sample1080p30.mp4 \
    ///   cargo test --release nv12_convert -- --ignored --nocapture
    /// ```
    ///
    /// The CPU reference is only reading a staging copy (a QA-only readback):
    /// the product path never maps a decoder surface. If the two pictures agree
    /// on average to a couple of levels per channel, the GPU conversion is doing
    /// the BT.709 limited-range conversion this path relies on.
    #[test]
    #[ignore = "needs a GPU and BILI_QA_MF_PATH"]
    fn nv12_convert() {
        let Ok(path) = std::env::var("BILI_QA_MF_PATH") else {
            return;
        };
        let (device, context) = probe_device().expect("d3d11 device");
        let mut decoder = mf::MfVideoDecoder::open_path(
            std::path::Path::new(&path),
            &device,
            &context,
        )
        .expect("mf open_path");
        let frame = decoder
            .next_frame(std::time::Duration::from_secs(10))
            .expect("next_frame")
            .expect("frame");
        let picture = crate::dcomp::nv12::GpuPicture::from_mf(frame);
        let (width, height) = (picture.width, picture.height);
        eprintln!(
            "nv12 convert input={width}x{height} surface={}x{} sub={}",
            picture.tex_width, picture.tex_height, picture.subresource
        );
        let mut converter = crate::dcomp::nv12::Nv12Converter::new(
            &device,
            &context,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        )
        .expect("converter");
        converter.ensure(width, height).expect("ensure");
        let started = std::time::Instant::now();
        for _ in 0..30 {
            converter.convert(0, &picture).expect("convert");
        }
        let per_blit_us = started.elapsed().as_micros() as f64 / 30.0;

        // GPU result -> CPU (staging copy; QA only).
        let gpu = read_back(
            &device,
            &context,
            converter.texture(0).expect("slot"),
            width,
            height,
        );
        // The same surface as raw NV12 (staging copy of the decoder's slice).
        let (nv12, pitch) = read_nv12(&device, &context, &picture);
        let cpu = cpu_nv12_to_bgra(
            &nv12,
            pitch,
            width as usize,
            height as usize,
            picture.tex_height as usize,
        );
        let mut total = 0u64;
        let mut count = 0u64;
        let mut worst = 0u32;
        for index in (0..gpu.len()).step_by(4 * 97) {
            for channel in 0..3 {
                let a = gpu[index + channel] as i32;
                let b = cpu[index + channel] as i32;
                let delta = (a - b).unsigned_abs();
                total += delta as u64;
                worst = worst.max(delta);
                count += 1;
            }
        }
        let mean = total as f64 / count.max(1) as f64;
        eprintln!(
            "nv12 convert blit_us={per_blit_us:.1} mean_abs_delta={mean:.2} max_delta={worst} samples={count}"
        );
        assert!(mean < 12.0, "GPU 转换与 CPU 参考差异过大: mean={mean}");
    }

    #[cfg(test)]
    fn read_back(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
            D3D11_TEXTURE2D_DESC,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
        let desc = D3D11_TEXTURE2D_DESC {
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
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.expect("staging");
        let staging = staging.expect("staging");
        let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
            texture.cast().expect("cast");
        unsafe { context.CopyResource(&staging, &resource) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.expect("map");
        let mut out = vec![0u8; (width * height * 4) as usize];
        let pitch = mapped.RowPitch as usize;
        for row in 0..height as usize {
            let src = unsafe { (mapped.pData as *const u8).add(row * pitch) };
            let dst = &mut out[row * width as usize * 4..(row + 1) * width as usize * 4];
            unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len()) };
        }
        unsafe { context.Unmap(&staging, 0) };
        out
    }

    #[cfg(test)]
    fn read_nv12(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        picture: &crate::dcomp::nv12::GpuPicture,
    ) -> (Vec<u8>, usize) {
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_USAGE_STAGING,
            D3D11_TEXTURE2D_DESC,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
        let desc = D3D11_TEXTURE2D_DESC {
            Width: picture.tex_width,
            Height: picture.tex_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.expect("staging nv12");
        let staging = staging.expect("staging");
        let sub = picture.subresource;
        let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
            picture.texture.cast().expect("cast");
        unsafe { context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &resource, sub, None) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.expect("map nv12");
        let pitch = mapped.RowPitch as usize;
        // NV12: one full luma plane, then one chroma plane at half height.
        let rows = picture.tex_height as usize * 3 / 2;
        let mut out = vec![0u8; pitch * rows];
        for row in 0..rows {
            let src = unsafe { (mapped.pData as *const u8).add(row * pitch) };
            let dst = &mut out[row * pitch..(row + 1) * pitch];
            unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), pitch) };
        }
        unsafe { context.Unmap(&staging, 0) };
        (out, pitch)
    }

    /// BT.709 limited range -> BGRA, the reference the GPU conversion is
    /// compared against (the same coefficients as `player::nv12_to_rgba`).
    #[cfg(test)]
    fn cpu_nv12_to_bgra(
        nv12: &[u8],
        pitch: usize,
        width: usize,
        height: usize,
        surface_height: usize,
    ) -> Vec<u8> {
        let mut out = vec![0u8; width * height * 4];
        for row in 0..height {
            let y_row = &nv12[row * pitch..row * pitch + width];
            let uv_row =
                &nv12[(surface_height + row / 2) * pitch..(surface_height + row / 2) * pitch + width];
            for col in 0..width {
                let yv = y_row[col] as f32 - 16.0;
                let y = 1.164 * yv;
                let u = uv_row[(col / 2) * 2] as f32 - 128.0;
                let v = uv_row[(col / 2) * 2 + 1] as f32 - 128.0;
                let r = (y + 1.5748 * v).clamp(0.0, 255.0) as u8;
                let g = (y - 0.1873 * u - 0.4681 * v).clamp(0.0, 255.0) as u8;
                let b = (y + 1.8556 * u).clamp(0.0, 255.0) as u8;
                let at = (row * width + col) * 4;
                out[at] = b;
                out[at + 1] = g;
                out[at + 2] = r;
                out[at + 3] = 255;
            }
        }
        out
    }
}
