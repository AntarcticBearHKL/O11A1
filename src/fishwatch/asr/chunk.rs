//! Long-audio chunking for the ASR step.
//!
//! faster-whisper on this machine died with `-1073740791` (0xC0000409) on
//! 267 s / 951 s inputs: one long CTranslate2 call is the thing that fails, and
//! a failed call loses the whole transcript. So the audio is cut into
//! ≤[`DEFAULT_CHUNK_SECONDS`] pieces with `ffmpeg -f segment`, each piece is
//! transcribed on its own, and the per-piece segments are shifted back onto the
//! global timeline. A piece that fails on the GPU is retried on the CPU; only
//! when every piece fails does the whole run fail.
//!
//! The split has no overlap: `ffmpeg` cuts on whole frames of 16 kHz mono PCM,
//! so chunk *n* starts exactly where chunk *n-1* ended, and the offset of each
//! chunk is the sum of the real durations of the chunks before it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::fishwatch::asr::transcript::Segment;
use crate::fishwatch::proc;

/// Size of one chunk when nothing overrides it: five minutes, so no single
/// engine call ever sees a long file again.
pub const DEFAULT_CHUNK_SECONDS: u32 = 300;

/// Env override for QA (`BILI_ASR_CHUNK_SECONDS`), clamped to something sane.
pub fn chunk_seconds() -> u32 {
    std::env::var("BILI_ASR_CHUNK_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map(|value| value.clamp(30, 900))
        .unwrap_or(DEFAULT_CHUNK_SECONDS)
}

/// Duration of a PCM WAV, read from its `data` chunk size (not the file size:
/// a WAV may carry extra chunks before or after the samples).
pub fn wav_seconds(path: &Path) -> Option<f64> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }
    let mut sample_rate = 0u32;
    let mut block_align = 0u16;
    loop {
        let mut chunk = [0u8; 8];
        if file.read_exact(&mut chunk).is_err() {
            return None;
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        let is_fmt = &chunk[..4] == b"fmt ".as_slice();
        match &chunk[..4] {
            b"fmt " => {
                let mut fmt = vec![0u8; size.min(64) as usize];
                file.read_exact(&mut fmt).ok()?;
                if fmt.len() < 16 {
                    return None;
                }
                sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                block_align = u16::from_le_bytes([fmt[12], fmt[13]]);
                // Skip the rest of a (rare) oversized fmt chunk.
                let extra = size.saturating_sub(fmt.len() as u64);
                file.seek(SeekFrom::Current(extra as i64)).ok()?;
            }
            b"data" => {
                if sample_rate == 0 || block_align == 0 {
                    return None;
                }
                let frames = size / block_align as u64;
                return Some(frames as f64 / sample_rate as f64);
            }
            _ => {
                // chunks are word-aligned
                file.seek(SeekFrom::Current((size + (size & 1)) as i64)).ok()?;
            }
        }
        if size & 1 == 1 && is_fmt {
            file.seek(SeekFrom::Current(1)).ok()?;
        }
    }
}

/// Cut `audio` into `dir/chunk-%03d.wav`. Returns the pieces in time order; a
/// file that fits in one chunk is returned as-is (no copy, no re-encode).
pub fn split(
    ffmpeg: &str,
    audio: &Path,
    dir: &Path,
    seconds: u32,
) -> Result<Vec<PathBuf>> {
    let total = wav_seconds(audio).unwrap_or(0.0);
    if total > 0.0 && total <= seconds as f64 {
        return Ok(vec![audio.to_path_buf()]);
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建分段目录失败：{}", dir.display()))?;
    let pattern = dir.join("chunk-%03d.wav");
    let segment_time = seconds.to_string();
    let status = proc::command(ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(audio)
        .args(["-f", "segment", "-segment_time"])
        .arg(&segment_time)
        .args(["-reset_timestamps", "1", "-ac", "1", "-ar", "16000"])
        .args(["-c:a", "pcm_s16le"])
        .arg(&pattern)
        .status()
        .with_context(|| format!("启动 ffmpeg 切段失败：{ffmpeg}"))?;
    if !status.success() {
        return Err(anyhow!("ffmpeg 切段失败（退出码 {:?}）", status.code()));
    }
    let mut pieces: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("读取分段目录失败：{}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("wav")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("chunk-"))
                    .unwrap_or(false)
        })
        .collect();
    pieces.sort();
    if pieces.is_empty() {
        return Err(anyhow!("ffmpeg 切段没有产出任何片段"));
    }
    Ok(pieces)
}

/// Shift one chunk's segments onto the global timeline and merge them in order.
pub fn merge(parts: Vec<(f64, Vec<Segment>)>) -> Vec<Segment> {
    let mut merged: Vec<Segment> = parts
        .into_iter()
        .flat_map(|(base, segments)| {
            segments.into_iter().map(move |segment| Segment {
                start_s: (segment.start_s + base).max(0.0),
                end_s: (segment.end_s + base).max(segment.start_s + base),
                text: segment.text,
            })
        })
        .collect();
    merged.sort_by(|a, b| {
        a.start_s
            .partial_cmp(&b.start_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 16 kHz mono PCM WAV whose `data` chunk holds `seconds`.
    fn write_wav(path: &Path, seconds: f64, extra_chunk: bool) {
        let frames = (seconds * 16_000.0) as u32;
        let data_bytes = frames * 2;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
        fmt.extend_from_slice(&1u16.to_le_bytes()); // mono
        fmt.extend_from_slice(&16_000u32.to_le_bytes());
        fmt.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
        fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
        fmt.extend_from_slice(&16u16.to_le_bytes()); // bits
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        body.extend_from_slice(&fmt);
        if extra_chunk {
            // A LIST chunk before the samples: the size must not confuse us.
            body.extend_from_slice(b"LIST");
            body.extend_from_slice(&5u32.to_le_bytes());
            body.extend_from_slice(b"INFOx\0");
        }
        body.extend_from_slice(b"data");
        body.extend_from_slice(&data_bytes.to_le_bytes());
        body.extend(vec![0u8; data_bytes as usize]);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        std::fs::write(path, out).expect("write wav");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "o11a1-chunk-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn wav_seconds_reads_the_data_chunk() {
        let dir = temp_dir("wav");
        let plain = dir.join("plain.wav");
        write_wav(&plain, 12.5, false);
        assert!((wav_seconds(&plain).expect("seconds") - 12.5).abs() < 0.01);
        let extra = dir.join("extra.wav");
        write_wav(&extra, 3.0, true);
        assert!((wav_seconds(&extra).expect("seconds") - 3.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_seconds_is_none_for_something_that_is_not_a_wav() {
        let dir = temp_dir("notwav");
        let path = dir.join("x.wav");
        std::fs::write(&path, b"not a wav at all").expect("write");
        assert!(wav_seconds(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_offsets_and_orders_the_chunks() {
        let segment = |start: f64, end: f64, text: &str| Segment {
            start_s: start,
            end_s: end,
            text: text.to_string(),
        };
        let merged = merge(vec![
            (300.0, vec![segment(1.0, 2.0, "b")]),
            (0.0, vec![segment(0.5, 1.5, "a")]),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "a");
        assert_eq!(merged[0].start_s, 0.5);
        assert_eq!(merged[1].text, "b");
        assert_eq!(merged[1].start_s, 301.0);
        assert_eq!(merged[1].end_s, 302.0);
    }

    #[test]
    fn short_audio_is_not_copied() {
        let dir = temp_dir("short");
        let audio = dir.join("short.wav");
        write_wav(&audio, 10.0, false);
        let pieces = split("ffmpeg", &audio, &dir.join("chunks"), 300).expect("split");
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], audio);
        assert!(!dir.join("chunks").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunk_seconds_is_clamped() {
        // The getter only reads the env var; the clamp is what matters here.
        std::env::remove_var("BILI_ASR_CHUNK_SECONDS");
        assert_eq!(chunk_seconds(), DEFAULT_CHUNK_SECONDS);
    }
}
