//! GPU-only decode policy.
//!
//! The player no longer falls back to software decoding. This module owns the
//! two decisions that follow from that:
//!
//! * which NVDEC decoder a stream needs (`h264_cuvid` / `hevc_cuvid` /
//!   `av1_cuvid` / `vp9_cuvid`), and
//! * whether this machine can actually run that decoder.
//!
//! The answer to the second question is not guessed from the GPU name. A tiny
//! embedded clip (10 frames, 256×144, generated from `testsrc2`) is decoded by
//! the real NVDEC decoder once per process: exit code 0 means the hardware and
//! driver accept the codec, anything else means the streams with that codec
//! must never be offered. That also covers profiles the *advertised* codec does
//! not promise (10-bit HEVC on an older NVDEC, AV1 on a pre-Ampere part).
//!
//! `source.rs` filters the playurl track list through [`decoder_for_codecid`]
//! + [`supports`], so an unsupported stream disappears from the 画质 menu
//! instead of failing after the user picks it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use crate::log;
use crate::proc;

const PROBE_H264: &[u8] = include_bytes!("probe/h264.mp4");
const PROBE_HEVC: &[u8] = include_bytes!("probe/hevc.mp4");
const PROBE_AV1: &[u8] = include_bytes!("probe/av1.mp4");
const PROBE_VP9: &[u8] = include_bytes!("probe/vp9.webm");

/// Bilibili DASH `codecid` → the only decoder allowed to open it.
pub fn decoder_for_codecid(codecid: i64) -> Option<&'static str> {
    match codecid {
        7 => Some("h264_cuvid"),
        12 => Some("hevc_cuvid"),
        13 => Some("av1_cuvid"),
        14 => Some("vp9_cuvid"),
        _ => None,
    }
}

/// ffprobe `codec_name` → the only decoder allowed to open a direct URL or a
/// local file.
pub fn decoder_for_codec_name(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "h264" | "avc" | "avc1" => Some("h264_cuvid"),
        "hevc" | "h265" => Some("hevc_cuvid"),
        "av1" | "av01" => Some("av1_cuvid"),
        "vp9" => Some("vp9_cuvid"),
        _ => None,
    }
}

/// Whether a Bilibili `codecid` can be decoded on this GPU right now.
pub fn supports_codecid(codecid: i64) -> bool {
    match decoder_for_codecid(codecid) {
        Some(decoder) => supports(decoder),
        None => false,
    }
}

fn cache() -> &'static Mutex<HashMap<String, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `decoder` works on this machine. One ffmpeg run per decoder per
/// process; the result is final for the session (a driver does not change
/// mid-playback).
pub fn supports(decoder: &str) -> bool {
    let cache = cache();
    if let Ok(map) = cache.lock() {
        if let Some(value) = map.get(decoder) {
            return *value;
        }
    }
    let value = probe(decoder);
    if let Ok(mut map) = cache.lock() {
        map.insert(decoder.to_string(), value);
    }
    value
}

fn cached(decoder: &str) -> Option<bool> {
    cache()
        .lock()
        .ok()
        .and_then(|map| map.get(decoder).copied())
}

/// Probe every codec in `codecids` that has not been tested yet, in parallel.
/// The playurl response usually carries AVC/HEVC/AV1 at once; probing them
/// one after another added ~0.7 s to the first open, in parallel it is one
/// decoder start-up.
pub fn prewarm<I: IntoIterator<Item = i64>>(codecids: I) {
    let mut missing: Vec<&'static str> = Vec::new();
    for codecid in codecids {
        let Some(decoder) = decoder_for_codecid(codecid) else {
            continue;
        };
        if cached(decoder).is_none() && !missing.contains(&decoder) {
            missing.push(decoder);
        }
    }
    if missing.len() <= 1 {
        for decoder in missing {
            supports(decoder);
        }
        return;
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = missing
            .iter()
            .map(|decoder| scope.spawn(move || supports(decoder)))
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    });
}

fn probe_clip(decoder: &str) -> Option<(&'static [u8], &'static str)> {
    match decoder {
        "h264_cuvid" => Some((PROBE_H264, "mp4")),
        "hevc_cuvid" => Some((PROBE_HEVC, "mp4")),
        "av1_cuvid" => Some((PROBE_AV1, "mp4")),
        "vp9_cuvid" => Some((PROBE_VP9, "webm")),
        _ => None,
    }
}

/// Decode the embedded clip with the real hardware decoder. A missing CUDA
/// runtime, an unsupported codec or a driver that refuses the profile all make
/// ffmpeg exit non-zero (or write no frames), which is exactly the answer we
/// need before offering the stream.
fn probe(decoder: &str) -> bool {
    let Some((bytes, extension)) = probe_clip(decoder) else {
        return false;
    };
    let path = std::env::temp_dir().join(format!("o11a1-hwprobe-{decoder}.{extension}"));
    if std::fs::write(&path, bytes).is_err() {
        log::line(&format!(
            "hwdecode probe decoder={decoder} supported=0 reason=temp-write"
        ));
        return false;
    }
    let mut command = proc::command("ffmpeg.exe");
    command
        .arg("-v")
        .arg("error")
        .arg("-nostdin")
        .arg("-c:v")
        .arg(decoder)
        .arg("-i")
        .arg(&path)
        .arg("-frames:v")
        .arg("3")
        .arg("-f")
        .arg("null")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let supported = command
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    log::line(&format!(
        "hwdecode probe decoder={decoder} supported={}",
        u8::from(supported)
    ));
    supported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codecs_map_to_hardware_decoders_only() {
        assert_eq!(decoder_for_codecid(7), Some("h264_cuvid"));
        assert_eq!(decoder_for_codecid(12), Some("hevc_cuvid"));
        assert_eq!(decoder_for_codecid(13), Some("av1_cuvid"));
        assert_eq!(decoder_for_codecid(14), Some("vp9_cuvid"));
        assert_eq!(decoder_for_codecid(0), None);
        assert_eq!(decoder_for_codecid(21), None);
    }

    #[test]
    fn codec_names_cover_the_ffprobe_spelling() {
        assert_eq!(decoder_for_codec_name("h264"), Some("h264_cuvid"));
        assert_eq!(decoder_for_codec_name("HEVC"), Some("hevc_cuvid"));
        assert_eq!(decoder_for_codec_name("av1"), Some("av1_cuvid"));
        assert_eq!(decoder_for_codec_name("vp9"), Some("vp9_cuvid"));
        assert_eq!(decoder_for_codec_name("mpeg2video"), None);
    }

    #[test]
    fn every_embedded_probe_clip_is_an_mp4_or_webm_container() {
        assert!(PROBE_H264.len() > 1_000);
        assert!(PROBE_H264.starts_with(&[0, 0, 0]));
        assert!(PROBE_VP9.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
    }
}
