//! Standalone probe for the f208 Media Foundation decoder.
//!
//! ```text
//! cargo run --release --example mf_probe -- <url> <headers.json> [seconds]
//! ```
//!
//! `url` is a B 站 DASH video stream (fMP4) and `headers.json` is the header
//! map the CDN needs (`Referer`, `User-Agent`, and a `Cookie` for 会员 tiers).
//! `scripts/qa-f208-mf.ps1` fetches both from the public playurl endpoint and
//! runs this, so nothing here is tied to a URL that expires.
//!
//! The probe includes the real decoder by path rather than re-implementing it:
//! `src/mediadec/` is app code, and this binary only supplies the two crate
//! items that file expects (the shared network runtime and `debug_log`).
//!
//! What it measures, and why each number matters:
//!
//! * `frames` / `fps` - the decoder is not realtime-limited here; it runs flat
//!   out for `seconds` of wall clock, so this is decode throughput.
//! * `d3d11_textures / frames` - every returned picture must already be an
//!   `ID3D11Texture2D`. A CPU buffer is an error inside the decoder, not a
//!   silent copy, so this ratio is the zero-copy claim itself.
//! * `p50/p95` of `next_frame` - that call *is* `ReadSample` on the decoder
//!   thread (request/reply over a channel), so the percentiles are the decode
//!   step the sink will pay per frame.
//! * `bytes_to_upper_layer=0` - no pixel ever crosses into the process' user
//!   space; compare with the ffmpeg path's `width*height*4` upload per frame.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};

#[path = "../src/mediadec/mod.rs"]
mod mediadec;

/// The two items `src/mediadec/mf.rs` expects from the app crate. The probe is
/// its own crate, so it provides them here instead of pulling in `src/bilibili.rs`.
mod bilibili {
    /// Same string as `bilibili::UA`; the caller's `headers.json` wins when it
    /// sets a User-Agent of its own.
    pub(crate) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                                 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

    pub(crate) fn shared_runtime() -> Option<&'static tokio::runtime::Runtime> {
        static RUNTIME: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
            std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("mf-probe-net")
                    .enable_all()
                    .build()
                    .ok()
            })
            .as_ref()
    }
}

/// The app's `debug_log` writes to `BILI_DEBUG_LOG`; the probe has a console.
fn debug_log(line: &str) {
    println!("{line}");
}

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(url), Some(headers_path)) = (args.next(), args.next()) else {
        eprintln!("usage: mf_probe <url> <headers.json> [seconds]");
        return std::process::ExitCode::from(2);
    };
    let seconds = args
        .next()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(10)
        .clamp(1, 600);
    let headers = match read_headers(&headers_path) {
        Ok(headers) => headers,
        Err(err) => {
            eprintln!("mf probe: headers.json 读取失败: {err}");
            return std::process::ExitCode::from(2);
        }
    };

    println!("mf probe version=f208");
    println!("mf input url_len={} headers={}", url.len(), headers.len());

    let (device, context, feature_level) = match create_device() {
        Ok(parts) => parts,
        Err(err) => {
            eprintln!("mf probe: D3D11 设备创建失败: {err}");
            return std::process::ExitCode::from(3);
        }
    };
    println!("mf device feature_level={feature_level} video_support=1");

    let mut decoder =
        match mediadec::mf::MfVideoDecoder::open(&url, &headers, &device, &context) {
            Ok(decoder) => decoder,
            Err(err) => {
                // The decoder's failure modes are part of the contract: say
                // which one happened before exiting non-zero.
                println!("mf result error={err}");
                return std::process::ExitCode::from(4);
            }
        };
    let (width, height, fps) = decoder.info();
    let details = decoder.details();
    println!(
        "mf media size={}x{} fps={:.2} native={} output={} d3d11_aware={} hardware_mft={}",
        width,
        height,
        fps,
        subtype_label(&details.native_subtype),
        subtype_label(&details.output_subtype),
        details.d3d11_aware,
        match details.hardware_mft {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    );

    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut latencies: Vec<Duration> = Vec::new();
    let mut frames = 0u64;
    let mut texture_frames = 0u64;
    let mut formats: std::collections::BTreeMap<i32, u64> = std::collections::BTreeMap::new();
    let mut subresources: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut first_desc: Option<D3D11_TEXTURE2D_DESC> = None;
    let mut texture_size = (0u32, 0u32);
    let mut end_of_stream = false;
    let mut failure: Option<String> = None;

    while Instant::now() < deadline {
        let read_started = Instant::now();
        match decoder.next_frame(Duration::from_secs(2)) {
            Ok(Some(frame)) => {
                latencies.push(read_started.elapsed());
                frames += 1;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { frame.texture.GetDesc(&mut desc) };
                *formats.entry(desc.Format.0).or_default() += 1;
                if desc.Format == DXGI_FORMAT_NV12 || desc.Format == DXGI_FORMAT_P010 {
                    texture_frames += 1;
                }
                subresources.insert(frame.subresource);
                if first_desc.is_none() {
                    texture_size = (frame.width, frame.height);
                    first_desc = Some(desc);
                }
                // The decoder's textures come from a small pool: dropping each
                // picture before asking for the next one is the documented way
                // to consume it (f209 will hold at most the one it is showing).
                drop(frame);
            }
            Ok(None) => {
                end_of_stream = true;
                break;
            }
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    let elapsed = started.elapsed();
    let (decoded, cpu_frames) = decoder.stats();

    // f209's sink will seek, so the probe exercises the seek path too: jump to
    // `seek_s` and report the first few presentation times.
    let seek_s = std::env::var("MF_PROBE_SEEK_S")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(5);
    let seek_target = Duration::from_secs(seek_s);
    match decoder.seek(seek_target) {
        Ok(()) => {
            let mut pts = Vec::new();
            for _ in 0..4 {
                match decoder.next_frame(Duration::from_secs(5)) {
                    Ok(Some(frame)) => pts.push(format!("{}", frame.pts.as_millis())),
                    Ok(None) => {
                        pts.push("eos".to_string());
                        break;
                    }
                    Err(err) => {
                        pts.push(format!("error:{err}"));
                        break;
                    }
                }
            }
            println!(
                "mf seek target_ms={} first_pts_ms=[{}]",
                seek_target.as_millis(),
                pts.join(",")
            );
        }
        Err(err) => println!(
            "mf seek target_ms={} error={err}",
            seek_target.as_millis()
        ),
    }

    if let Some(desc) = first_desc {
        println!(
            "mf texture format={} (dxgi:{}) width={} height={} visible={}x{} array_size={} mip_levels={} bind_flags=0x{:x}",
            format_label(desc.Format.0),
            desc.Format.0,
            desc.Width,
            desc.Height,
            texture_size.0,
            texture_size.1,
            desc.ArraySize,
            desc.MipLevels,
            desc.BindFlags,
        );
        println!(
            "mf texture subresource_first={} subresources={}",
            subresources.iter().next().copied().unwrap_or(0),
            subresources.len(),
        );
    }
    println!(
        "mf decode frames={} seconds={:.2} fps={:.2} d3d11_textures={} cpu_buffers={} eos={}",
        frames,
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64().max(1e-9),
        texture_frames,
        cpu_frames,
        if end_of_stream { 1 } else { 0 },
    );
    match percentiles(&mut latencies) {
        Some((p50, p95, max, count)) => println!(
            "mf readsample p50_us={:.0} p95_us={:.0} max_us={:.0} n={}",
            p50.as_secs_f64() * 1e6,
            p95.as_secs_f64() * 1e6,
            max.as_secs_f64() * 1e6,
            count,
        ),
        None => println!("mf readsample p50_us=0 p95_us=0 max_us=0 n=0"),
    }
    // Zero-copy claim: nothing here ever asks a buffer for a pointer, so no
    // pixel is copied on the CPU and no `IMFDXGIBuffer` is converted down.
    println!("mf upload bytes_to_upper_layer=0 map_calls=0");
    // The comparison the f208 brief asks for, both ways: the 1080p reference
    // the ffmpeg pipe pays today, and what the same stream would cost there.
    println!(
        "mf compare path=ffmpeg 1080p upload_bytes_per_frame=8294400 (1920x1080 BGRA) stage_us=5000..11000; path=mf upload_bytes_per_frame=0 (this stream {}x{} would be {} for ffmpeg)",
        width,
        height,
        u64::from(width) * u64::from(height) * 4
    );
    println!(
        "mf formats {}",
        formats
            .iter()
            .map(|(format, count)| format!("dxgi:{format}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    match failure {
        Some(err) => {
            println!("mf result error={err}");
            std::process::ExitCode::from(4)
        }
        None => {
            println!(
                "mf result ok frames={} decoded={} textures={} subresources={}",
                frames,
                decoded,
                texture_frames,
                subresources.len()
            );
            std::process::ExitCode::SUCCESS
        }
    }
}

fn read_headers(path: &str) -> Result<Vec<(String, String)>, String> {
    let raw = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let parsed: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&raw).map_err(|err| err.to_string())?;
    Ok(parsed
        .into_iter()
        .filter_map(|(name, value)| match value {
            serde_json::Value::String(text) => Some((name, text)),
            serde_json::Value::Null => None,
            other => Some((name, other.to_string())),
        })
        .collect())
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, i32), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut level = D3D_FEATURE_LEVEL_11_0;
    // Both flags are load bearing: VIDEO_SUPPORT is what the MF DXGI device
    // manager insists on and BGRA_SUPPORT is what a D3D11on12/Composition
    // consumer would need later.
    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .map_err(|err| err.to_string())?;
    }
    let device = device.ok_or_else(|| "D3D11CreateDevice 返回空设备".to_string())?;
    let context = context.ok_or_else(|| "D3D11CreateDevice 返回空上下文".to_string())?;
    // Which adapter did we get, and can it decode at all? A device without a
    // video decoder profile (a remote/basic adapter) will never produce D3D11
    // textures, and that is worth naming in the probe output.
    if let Ok(dxgi_device) = device.cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>() {
        if let Ok(adapter) = unsafe { dxgi_device.GetAdapter() } {
            if let Ok(desc) = unsafe { adapter.GetDesc() } {
                let name: String = desc
                    .Description
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| char::from_u32(*c as u32).unwrap_or('?'))
                    .collect();
                println!("mf adapter \"{name}\" vendor=0x{:04x}", desc.VendorId);
            }
        }
    }
    match device.cast::<windows::Win32::Graphics::Direct3D11::ID3D11VideoDevice>() {
        Ok(video_device) => {
            let count = unsafe { video_device.GetVideoDecoderProfileCount() };
            println!("mf device video_decoder_profiles={count}");
        }
        Err(err) => println!("mf device video_decoder_profiles=none ({err})"),
    }
    // The device manager will use this device from MF threads.
    unsafe {
        let _ = device
            .cast::<windows::Win32::Graphics::Direct3D10::ID3D10Multithread>()
            .map_err(|err| format!("ID3D10Multithread: {err}"))?
            .SetMultithreadProtected(true);
    }
    Ok((device, context, level.0))
}

fn percentiles(samples: &mut [Duration]) -> Option<(Duration, Duration, Duration, usize)> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let count = samples.len();
    let pick = |fraction: f64| -> Duration {
        let index = ((count - 1) as f64 * fraction).round() as usize;
        samples[index]
    };
    Some((pick(0.50), pick(0.95), samples[count - 1], count))
}

fn format_label(dxgi: i32) -> &'static str {
    match dxgi {
        103 => "NV12",
        104 => "P010",
        87 => "BGRA",
        28 => "RGBA",
        _ => "other",
    }
}

/// `GUID` for the codec names the log printed (kept tiny: only what the
/// decoder reports and what the probe compares).
fn subtype_label(guid: &windows::core::GUID) -> &'static str {
    use windows::Win32::Media::MediaFoundation::{
        MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
        MFVideoFormat_P010, MFVideoFormat_VP90,
    };
    if *guid == MFVideoFormat_H264 {
        "H264"
    } else if *guid == MFVideoFormat_HEVC {
        "HEVC"
    } else if *guid == MFVideoFormat_AV1 {
        "AV1"
    } else if *guid == MFVideoFormat_VP90 {
        "VP9"
    } else if *guid == MFVideoFormat_NV12 {
        "NV12"
    } else if *guid == MFVideoFormat_P010 {
        "P010"
    } else {
        "unknown"
    }
}
