//! Resolve what to play: a Bilibili `bvid`/`cid` pair or a direct media URL,
//! plus the size, duration, frame rate and CDN headers the decoder needs.

use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CYSCREEN};

use crate::format::fit_capped;
use crate::hwdecode;
use crate::proc;
use crate::{log, PlayerConfig};

/// Hard decoder ceiling even in 原始分辨率 mode. 8K tracks are downscaled so the
/// NV12 pipe and the GPU upload stay bounded; 4K passes through untouched.
const DECODE_MAX_W: u32 = 3840;
const DECODE_MAX_H: u32 = 2160;

/// Decode size budget for one resolve.
///
/// `按屏幕分辨率解码` on: follow the stream up to 4K but never above the
/// display's own height. off: the 1080p cap. The picture now stays on the GPU
/// for conversion/interpolation/enhancement, so the cap is about the NV12
/// pipe, upload bandwidth and CPU, not decode capability.
fn decode_cap_for() -> (u32, u32) {
    let native_size = o11a1::settings::load().player_native_size;
    if !native_size {
        return (1920, 1080);
    }
    // Never decode above the display's own height. On a 1080p screen a 4K
    // source was being decoded at 3840x2160 and then downscaled for display:
    // four times the NV12 pipe, upload and interpolation work for pixels the
    // panel cannot show. A 4K display still gets the full 2160p path.
    let display_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let cap_h = if display_h > 0 {
        (display_h as u32).clamp(720, DECODE_MAX_H)
    } else {
        DECODE_MAX_H
    };
    let cap = (DECODE_MAX_W, cap_h);
    log::line(&format!(
        "decode cap native=1 display_h={display_h} cap={}x{}",
        cap.0, cap.1
    ));
    cap
}

/// One selectable DASH video representation, copied out of `PlayInfo` so the
/// player can rebuild the 画质 menu without holding the whole playurl response.
#[derive(Clone, Debug, Default)]
pub struct VideoTrackInfo {
    pub qn: i64,
    pub codecid: i64,
    pub width: u32,
    pub height: u32,
    pub frame_rate: f64,
    /// `1080P · AVC` style menu label.
    pub label: String,
}

/// One selectable DASH audio track.
#[derive(Clone, Debug, Default)]
pub struct AudioTrackInfo {
    pub bandwidth: u64,
    /// `192K (30280)` style menu label.
    pub label: String,
}

/// One 画质 row: every tier the video advertises, including the 大会员 tiers a
/// non-VIP response does not return a stream for (`locked = true`).
#[derive(Clone, Debug, Default)]
pub struct QualityTierInfo {
    pub qn: i64,
    pub label: String,
    pub detail: String,
    pub locked: bool,
    pub note: String,
}

/// Source-side description handed to the decoder/engine.
#[derive(Clone, Debug)]
pub struct MediaSource {
    pub title: String,
    pub bvid: Option<String>,
    pub cid: Option<u64>,
    pub video_url: String,
    pub audio_url: Option<String>,
    pub headers: Vec<(String, String)>,
    pub duration_ms: u64,
    /// Selected video/audio stream bitrates in bits per second (0 when a
    /// direct URL did not advertise one).
    pub video_bitrate: u64,
    pub audio_bitrate: u64,
    /// Native size as declared by the manifest/probe.
    pub source_width: u32,
    pub source_height: u32,
    /// The size the decode pipe is scaled to (source unless capped).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub qn: i64,
    /// DASH `codecid` of the selected video track (7 AVC, 12 HEVC, 13 AV1).
    pub codecid: i64,
    /// Index into [`MediaSource::audio_tracks`] of the selected audio stream.
    pub audio_index: usize,
    pub qn_label: String,
    pub audio_label: String,
    pub codec: String,
    /// The GPU decoder every session for this source must use
    /// (`h264_cuvid`, ...). Empty only for the loading placeholder; a real
    /// source without one is refused before a window can open.
    pub hw_decoder: String,
    /// Every playable video representation returned by `playurl`.
    pub video_tracks: Vec<VideoTrackInfo>,
    /// Every playable DASH audio track, high→low bitrate.
    pub audio_tracks: Vec<AudioTrackInfo>,
    /// The upload's advertised tier list (locked 大会员 rows included).
    pub quality_tiers: Vec<QualityTierInfo>,
    pub remote: bool,
}

impl MediaSource {
    /// Whether this source exposes a Bilibili playurl menu (`false` for 直链).
    pub fn has_track_menu(&self) -> bool {
        !self.video_tracks.is_empty()
    }
    /// The stand-in source the window opens with while `resolve` runs on a
    /// worker thread. It has no URL, so the engine skips the decoder and paints
    /// the loading card until [`crate::DeferredOpen`] replaces it.
    pub(crate) fn placeholder(title: String, bvid: Option<String>, cid: Option<u64>) -> Self {
        Self {
            title,
            bvid,
            cid,
            video_url: String::new(),
            audio_url: None,
            headers: Vec::new(),
            duration_ms: 0,
            video_bitrate: 0,
            audio_bitrate: 0,
            source_width: 1280,
            source_height: 720,
            width: 1280,
            height: 720,
            fps: 30.0,
            qn: 0,
            codecid: 0,
            audio_index: 0,
            qn_label: String::new(),
            audio_label: String::new(),
            codec: String::new(),
            hw_decoder: String::new(),
            video_tracks: Vec::new(),
            audio_tracks: Vec::new(),
            quality_tiers: Vec::new(),
            remote: true,
        }
    }

    /// True for the stand-in used before the real source is resolved.
    pub(crate) fn is_placeholder(&self) -> bool {
        self.video_url.is_empty()
    }
}

/// Stable identity for the disk cache. Bilibili sources key by video/cid and
/// the exact selected representation, so the same episode opened again (or
/// prefetched from the queue) reuses one complete file. Direct remote URLs
/// fall back to a URL hash; a combined audio+video URL deliberately shares the
/// video key so the file is not downloaded twice.
pub fn media_cache_key(source: &MediaSource, audio: bool) -> Option<String> {
    if !source.remote || source.video_url.trim().is_empty() {
        return None;
    }
    let base = source
        .bvid
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| source.cid.map(|cid| format!("cid{cid}")));
    if let Some(base) = base {
        let cid = source.cid.unwrap_or(0);
        return Some(if audio {
            format!(
                "{base}-{cid}-a{}-{}",
                source.audio_index, source.audio_bitrate
            )
        } else {
            format!(
                "{base}-{cid}-v{}-{}-{}x{}",
                source.qn, source.codecid, source.width, source.height
            )
        });
    }
    let (url, tag) = if audio {
        match source.audio_url.as_deref() {
            Some(url) if url != source.video_url => (url, "a"),
            _ => (source.video_url.as_str(), "v"),
        }
    } else {
        (source.video_url.as_str(), "v")
    };
    Some(format!("url-{:016x}-{tag}", fnv1a(url.as_bytes())))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The resolved media plus the session that resolved it. `Player::open` hands
/// the session to the danmaku fetch thread, so the comment pass inherits the
/// same login/cookies instead of re-authenticating.
pub struct ResolvedSource {
    pub source: MediaSource,
    pub session: Option<o11a1::bilibili::Session>,
}

/// `BILI_PLAYER_URL` wins over everything (direct URL or local path). Otherwise
/// `BILI_OPEN_BVID` / `config.bvid` / the brief's default bvid is resolved
/// through `video_view` + `playurl`.
pub fn resolve(config: &PlayerConfig) -> Result<ResolvedSource> {
    let direct = std::env::var("BILI_PLAYER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            config
                .direct_url
                .clone()
                .filter(|value| !value.trim().is_empty())
        });
    if let Some(url) = direct {
        return resolve_direct(url.trim(), config);
    }
    let bvid = std::env::var("BILI_OPEN_BVID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.bvid.clone().filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| "BV1mhxo6BEG4".to_string());
    resolve_bilibili(&bvid, config)
}

fn resolve_direct(url: &str, config: &PlayerConfig) -> Result<ResolvedSource> {
    let remote = !proc::is_local(url);
    let mut headers = Vec::new();
    if remote {
        headers = vec![
            (
                "User-Agent".to_string(),
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36".to_string(),
            ),
            ("Referer".to_string(), "https://www.bilibili.com/".to_string()),
        ];
    }
    let probe = ffprobe(url, &headers, remote);
    let (source_width, source_height, mut duration_ms, fps, codec_name) = match &probe {
        Some(probe) => (
            probe.width,
            probe.height,
            probe.duration_ms,
            probe.fps,
            probe.codec_name.clone(),
        ),
        None => (1280, 720, 0, 30.0, String::new()),
    };
    if duration_ms == 0 && config.start_ms > 0 {
        duration_ms = 0;
    }
    // GPU-only: the codec decides whether this source exists at all. An
    // unrecognised or unsupported codec is refused here, before a decoder
    // process is spawned, so nothing can silently fall back to the CPU.
    let hw_decoder = hwdecode::decoder_for_codec_name(&codec_name).ok_or_else(|| {
        anyhow!(
            "该视频编码不支持 GPU 硬解（{}）",
            if codec_name.trim().is_empty() {
                "无法探测编码"
            } else {
                codec_name.as_str()
            }
        )
    })?;
    let cap = decode_cap_for();
    let (width, height) = fit_capped(source_width, source_height, cap.0, cap.1);
    let title = if !config.title.trim().is_empty() {
        config.title.clone()
    } else {
        url.rsplit(['/', '\\'])
            .next()
            .unwrap_or("本地视频")
            .to_string()
    };
    log::line(&format!(
        "source direct url={}{} codec={} decoder={hw_decoder} size={}x{} -> {}x{} fps={:.3} duration_ms={} remote={}",
        if remote { "" } else { "(local) " },
        short_url(url),
        if codec_name.is_empty() { "-" } else { codec_name.as_str() },
        source_width,
        source_height,
        width,
        height,
        fps,
        duration_ms,
        remote
    ));
    Ok(ResolvedSource {
        source: MediaSource {
            title,
            bvid: None,
            cid: None,
            video_url: url.to_string(),
            audio_url: Some(
                config
                    .direct_audio_url
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| url.to_string()),
            ),
            headers,
            duration_ms,
            video_bitrate: 0,
            audio_bitrate: 0,
            source_width,
            source_height,
            width,
            height,
            fps,
            qn: 0,
            codecid: 0,
            audio_index: 0,
            qn_label: "直链".to_string(),
            audio_label: "直链".to_string(),
            codec: "自动".to_string(),
            hw_decoder: hw_decoder.to_string(),
            video_tracks: Vec::new(),
            audio_tracks: Vec::new(),
            quality_tiers: Vec::new(),
            remote,
        },
        session: None,
    })
}

fn resolve_bilibili(bvid: &str, config: &PlayerConfig) -> Result<ResolvedSource> {
    let runtime =
        o11a1::bilibili::shared_runtime().ok_or_else(|| anyhow!("network runtime unavailable"))?;
    let bvid_owned = bvid.to_string();
    let account = o11a1::accounts::load();
    let logged_in = account.is_some();
    let resolved: Result<(MediaSource, Option<o11a1::bilibili::Session>)> = runtime.block_on(async move {
        use o11a1::bilibili::Session;
        let session = match &account {
            Some(account) => Session::from_token(&o11a1::accounts::to_token(account)).await?,
            None => Session::new().await?,
        };
        let detail = session
            .video_view(&bvid_owned)
            .await
            .with_context(|| format!("video_view {bvid_owned}"))?;
        let cid = config.cid.unwrap_or(detail.cid as u64);
        let info = session
            .playurl(&bvid_owned, cid)
            .await
            .with_context(|| format!("playurl {bvid_owned} cid={cid}"))?;
        // GPU-only: keep exactly the representations NVDEC accepted in this
        // process. The quality menu and the selection both read this list, so
        // an unsupported stream can neither be shown nor switched to.
        let tracks = supported_tracks(&info);
        if tracks.is_empty() {
            bail!(
                "该视频没有 GPU 可硬解的流（{}）",
                unsupported_codec_summary(&info)
            );
        }
        let requested_codec = config.quality_codecid.unwrap_or(7);
        let track = if let Some(height) = config.quality_height {
            preferred_height_track(&tracks, height, requested_codec)
        } else {
            let requested_qn = config.quality_qn.unwrap_or(80);
            track_for_qn(&tracks, requested_qn, requested_codec)
                .or_else(|| track_for_qn(&tracks, info.default_qn, requested_codec))
                .or_else(|| tracks.first().copied())
        }
        .ok_or_else(|| anyhow!("playurl returned no supported video tracks"))?;
        let hw_decoder = hwdecode::decoder_for_codecid(track.codecid)
            .ok_or_else(|| anyhow!("codec {} has no GPU decoder", track.codecid))?;
        let audio_index = match config.audio_index {
            Some(index) if index < info.audio_tracks.len() => index,
            _ => best_audio_index(&info),
        };
        let audio = info.audio_tracks.get(audio_index);
        let headers = session.media_headers();
        let duration_ms = if info.duration > 0 {
            info.duration * 1000
        } else {
            detail.duration * 1000
        };
        let title = if !config.title.trim().is_empty() {
            config.title.clone()
        } else {
            detail.title.clone()
        };
        let mut source_width = track.width;
        let mut source_height = track.height;
        let mut fps = track.frame_rate;
        let mut duration_ms = duration_ms;
        if source_width == 0 || source_height == 0 {
            if let Some(probe) = ffprobe(&track.url, &headers, true) {
                source_width = probe.width;
                source_height = probe.height;
                if fps <= 0.0 || !fps.is_finite() {
                    fps = probe.fps;
                }
                if duration_ms == 0 {
                    duration_ms = probe.duration_ms;
                }
            }
        }
        if !(fps.is_finite() && fps > 1.0) {
            fps = 30.0;
        }
        let cap = decode_cap_for();
        let (width, height) = fit_capped(source_width, source_height, cap.0, cap.1);
        let video_tracks = video_track_infos(&tracks);
        let audio_tracks = audio_track_infos(&info);
        let quality_tiers = quality_tier_infos(&bvid_owned, cid);
        log::line(&format!(
            "source bilibili bvid={} cid={cid} login={} title={:?} qn={} codec={} decoder={} size={}x{} -> {}x{} fps={:.3} duration_ms={} audio={} audio_index={} tracks={} tiers={} headers={}",
            bvid_owned,
            u8::from(logged_in),
            title,
            track.id,
            o11a1::bilibili::codec_label(track.codecid),
            hw_decoder,
            source_width,
            source_height,
            width,
            height,
            fps,
            duration_ms,
            audio
                .map(|track| format!("{}/{}", track.id, track.bandwidth))
                .unwrap_or_else(|| "-".to_string()),
            audio_index,
            video_tracks.len(),
            quality_tiers.len(),
            headers.len()
        ));
        Ok((
            MediaSource {
                title,
                bvid: Some(bvid_owned.clone()),
                cid: Some(cid),
                video_url: track.url.clone(),
                audio_url: audio.map(|track| track.url.clone()),
                headers,
                duration_ms,
                video_bitrate: track.bandwidth,
                audio_bitrate: audio.map(|track| track.bandwidth).unwrap_or(0),
                source_width,
                source_height,
                width,
                height,
                fps,
                qn: track.id,
                codecid: track.codecid,
                audio_index,
                qn_label: o11a1::bilibili::qn_label(track.id).to_string(),
                audio_label: audio
                    .map(|track| audio_menu_label(track.id, track.bandwidth, track.vip))
                    .unwrap_or_else(|| "无音轨".to_string()),
                codec: o11a1::bilibili::codec_label(track.codecid).to_string(),
                hw_decoder: hw_decoder.to_string(),
                video_tracks,
                audio_tracks,
                quality_tiers,
                remote: true,
            },
            Some(session),
        ))
    });
    let (source, session) = resolved?;
    Ok(ResolvedSource { source, session })
}

struct ProbeInfo {
    width: u32,
    height: u32,
    fps: f64,
    duration_ms: u64,
    /// ffprobe `codec_name` (`h264`, `hevc`, `av1`, `vp9`). Empty when the
    /// probe could not read it.
    codec_name: String,
}

/// The DASH audio track the initial open prefers: highest-bitrate non-VIP
/// stream, then the first non-VIP one, then whatever is left.
pub(crate) fn best_audio_index(info: &o11a1::bilibili::PlayInfo) -> usize {
    info.audio_tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| !track.vip && !track.dolby && !track.hi_res)
        .max_by_key(|(_, track)| track.bandwidth)
        .or_else(|| {
            info.audio_tracks
                .iter()
                .enumerate()
                .filter(|(_, track)| !track.vip)
                .max_by_key(|(_, track)| track.bandwidth)
        })
        .or_else(|| info.audio_tracks.iter().enumerate().next())
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn audio_menu_label(id: i64, bandwidth: u64, vip: bool) -> String {
    format!(
        "{} · {} kbps{}",
        o11a1::bilibili::audio_label(id, bandwidth),
        bandwidth / 1000,
        if vip { " · 大会员" } else { "" }
    )
}

fn preferred_height_track<'a>(
    tracks: &[&'a o11a1::bilibili::VideoTrack],
    height: u32,
    codec: i64,
) -> Option<&'a o11a1::bilibili::VideoTrack> {
    if tracks.is_empty() {
        return None;
    }
    let key = |track: &&&o11a1::bilibili::VideoTrack| {
        (
            track.height,
            u8::from(track.codecid == codec),
            track.bandwidth,
        )
    };
    if height == 0 {
        return tracks.iter().max_by_key(key).copied();
    }
    tracks
        .iter()
        .filter(|track| track.height <= height)
        .max_by_key(key)
        .copied()
        .or_else(|| {
            tracks
                .iter()
                .min_by_key(|track| (track.height, track.bandwidth))
                .copied()
        })
}

/// The `PlayInfo::video_track_for` policy restricted to the streams this GPU
/// accepted: an explicit codec wins, otherwise the smallest stream at that
/// quality (fewer stalls at the same resolution).
fn track_for_qn<'a>(
    tracks: &[&'a o11a1::bilibili::VideoTrack],
    qn: i64,
    codec: i64,
) -> Option<&'a o11a1::bilibili::VideoTrack> {
    if codec != 0 {
        if let Some(track) = tracks
            .iter()
            .filter(|track| track.id == qn && track.codecid == codec)
            .max_by_key(|track| track.bandwidth)
        {
            return Some(*track);
        }
    }
    tracks
        .iter()
        .filter(|track| track.id == qn)
        .min_by_key(|track| track.bandwidth)
        .copied()
}

/// Every playurl representation whose codec NVDEC accepted in this process.
fn supported_tracks(info: &o11a1::bilibili::PlayInfo) -> Vec<&o11a1::bilibili::VideoTrack> {
    hwdecode::prewarm(info.video_tracks.iter().map(|track| track.codecid));
    info.video_tracks
        .iter()
        .filter(|track| hwdecode::supports_codecid(track.codecid))
        .collect()
}

/// `AVC/HEVC/AV1` list for the "nothing is decodable" error message.
fn unsupported_codec_summary(info: &o11a1::bilibili::PlayInfo) -> String {
    let mut labels: Vec<&str> = Vec::new();
    for track in &info.video_tracks {
        let label = o11a1::bilibili::codec_label(track.codecid);
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        "播放地址未返回视频流".to_string()
    } else {
        format!("{} 均无 GPU 硬解", labels.join("/"))
    }
}

fn video_track_infos(tracks: &[&o11a1::bilibili::VideoTrack]) -> Vec<VideoTrackInfo> {
    tracks
        .iter()
        .map(|track| VideoTrackInfo {
            qn: track.id,
            codecid: track.codecid,
            width: track.width,
            height: track.height,
            frame_rate: track.frame_rate,
            label: format!(
                "{} · {}",
                o11a1::bilibili::qn_label(track.id),
                o11a1::bilibili::codec_label(track.codecid)
            ),
        })
        .collect()
}

fn audio_track_infos(info: &o11a1::bilibili::PlayInfo) -> Vec<AudioTrackInfo> {
    info.audio_tracks
        .iter()
        .map(|track| AudioTrackInfo {
            bandwidth: track.bandwidth,
            label: audio_menu_label(track.id, track.bandwidth, track.vip),
        })
        .collect()
}

fn quality_tier_infos(bvid: &str, cid: u64) -> Vec<QualityTierInfo> {
    o11a1::bilibili::quality_tiers_for(bvid, cid)
        .unwrap_or_default()
        .into_iter()
        .map(|tier| {
            let note = tier.note().to_string();
            QualityTierInfo {
                qn: tier.qn,
                label: tier.label,
                detail: tier.detail,
                locked: tier.locked,
                note,
            }
        })
        .collect()
}

/// What a 画质/音轨 menu selection asks playurl for.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackRequest {
    pub qn: i64,
    pub codecid: i64,
    /// `Some` is strict (音轨): an unavailable index is an error. `None` lets a
    /// 画质 switch keep the best non-VIP audio the new response offers.
    pub audio_index: Option<usize>,
}

impl TrackRequest {
    pub fn quality(qn: i64, codecid: i64) -> Self {
        Self {
            qn,
            codecid,
            audio_index: None,
        }
    }

    pub fn audio(index: usize) -> Self {
        Self {
            qn: 0,
            codecid: 0,
            audio_index: Some(index),
        }
    }
}

/// Re-resolve playurl on the shared session for one menu selection.
///
/// Runs on the caller's background thread; the returned source is applied by
/// the player thread without ever blocking the presentation loop. `qn = 0`
/// keeps the currently playing quality (the 音轨 path), and a strict codec
/// match is required so a 大会员-only row cannot silently fall back to 1080P.
pub fn resolve_track_switch(
    session: &o11a1::bilibili::Session,
    bvid: &str,
    cid: u64,
    title: &str,
    current_qn: i64,
    current_codecid: i64,
    request: TrackRequest,
    fallback_duration_ms: u64,
) -> Result<MediaSource> {
    let runtime =
        o11a1::bilibili::shared_runtime().ok_or_else(|| anyhow!("network runtime unavailable"))?;
    let info = runtime
        .block_on(session.playurl(bvid, cid))
        .with_context(|| format!("playurl {bvid} cid={cid}"))?;
    let wanted_qn = if request.qn > 0 {
        request.qn
    } else {
        current_qn
    };
    let wanted_codecid = if request.qn > 0 {
        request.codecid
    } else {
        current_codecid
    };
    // GPU-only: the menu only ever shows supported rows, but the response is
    // re-resolved here, so filter again before choosing.
    let tracks = supported_tracks(&info);
    let track = tracks
        .iter()
        .filter(|track| {
            track.id == wanted_qn && (wanted_codecid == 0 || track.codecid == wanted_codecid)
        })
        .max_by_key(|track| track.bandwidth)
        .or_else(|| {
            // 音轨-only switch: keep the exact current representation.
            tracks
                .iter()
                .filter(|track| track.id == current_qn && track.codecid == current_codecid)
                .max_by_key(|track| track.bandwidth)
        })
        .copied()
        .ok_or_else(|| {
            anyhow!(
                "该画质不可用或没有 GPU 硬解（{} · {}）",
                o11a1::bilibili::qn_label(wanted_qn),
                o11a1::bilibili::codec_label(wanted_codecid)
            )
        })?;
    let audio_index = match request.audio_index {
        Some(index) if index < info.audio_tracks.len() => index,
        Some(_) => bail!("该音轨不可用（可能需要大会员）"),
        None => best_audio_index(&info),
    };
    let audio = info.audio_tracks.get(audio_index);
    let mut source_width = track.width;
    let mut source_height = track.height;
    let mut fps = track.frame_rate;
    let headers = session.media_headers();
    if source_width == 0 || source_height == 0 {
        if let Some(probe) = ffprobe(&track.url, &headers, true) {
            source_width = probe.width;
            source_height = probe.height;
            if fps <= 0.0 || !fps.is_finite() {
                fps = probe.fps;
            }
        }
    }
    if !(fps.is_finite() && fps > 1.0) {
        fps = 30.0;
    }
    let cap = decode_cap_for();
    let (width, height) = fit_capped(source_width, source_height, cap.0, cap.1);
    let duration_ms = if info.duration > 0 {
        info.duration * 1000
    } else {
        fallback_duration_ms
    };
    let source = MediaSource {
        title: title.to_string(),
        bvid: Some(bvid.to_string()),
        cid: Some(cid),
        video_url: track.url.clone(),
        audio_url: audio.map(|track| track.url.clone()),
        headers,
        duration_ms,
        video_bitrate: track.bandwidth,
        audio_bitrate: audio.map(|track| track.bandwidth).unwrap_or(0),
        source_width,
        source_height,
        width,
        height,
        fps,
        qn: track.id,
        codecid: track.codecid,
        audio_index,
        qn_label: o11a1::bilibili::qn_label(track.id).to_string(),
        audio_label: audio
            .map(|track| audio_menu_label(track.id, track.bandwidth, track.vip))
            .unwrap_or_else(|| "无音轨".to_string()),
        codec: o11a1::bilibili::codec_label(track.codecid).to_string(),
        hw_decoder: hwdecode::decoder_for_codecid(track.codecid)
            .unwrap_or_default()
            .to_string(),
        video_tracks: video_track_infos(&tracks),
        audio_tracks: audio_track_infos(&info),
        quality_tiers: quality_tier_infos(bvid, cid),
        remote: true,
    };
    log::line(&format!(
        "source track switch bvid={bvid} cid={cid} qn={} codec={} audio_index={} audio={} size={}x{} -> {}x{} fps={:.3} duration_ms={}",
        source.qn,
        source.codec,
        source.audio_index,
        source.audio_label,
        source.source_width,
        source.source_height,
        source.width,
        source.height,
        source.fps,
        source.duration_ms
    ));
    Ok(source)
}

/// Best-effort ffprobe; a missing probe is not fatal (the manifest usually
/// already told us the size, and 720p is a safe fallback).
fn ffprobe(url: &str, headers: &[(String, String)], remote: bool) -> Option<ProbeInfo> {
    let mut command = proc::command("ffprobe.exe");
    command
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=codec_name,width,height,avg_frame_rate")
        .arg("-of")
        .arg("csv=p=0");
    if remote {
        let text = proc::header_string(headers);
        if !text.is_empty() {
            command.arg("-headers").arg(text);
        }
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let parts: Vec<&str> = first.trim().split(',').collect();
    let codec_name = parts.first()?.trim().to_string();
    let width: u32 = parts.get(1)?.trim().parse().ok()?;
    let height: u32 = parts.get(2)?.trim().parse().ok()?;
    let fps = parts
        .get(3)
        .and_then(|value| {
            let (num, den) = value.split_once('/')?;
            let num: f64 = num.parse().ok()?;
            let den: f64 = den.parse().ok()?;
            (den > 0.0).then_some(num / den)
        })
        .unwrap_or(30.0);
    // A second, tiny call for the container duration. `N/A` (live/fragmented)
    // is "unknown", never zero-duration evidence.
    let mut duration_command = proc::command("ffprobe.exe");
    duration_command
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1");
    if remote {
        let text = proc::header_string(headers);
        if !text.is_empty() {
            duration_command.arg("-headers").arg(text);
        }
    }
    duration_command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let duration_ms = duration_command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|seconds| (seconds * 1000.0).round() as u64)
        })
        .unwrap_or(0);
    Some(ProbeInfo {
        width: width.max(2),
        height: height.max(2),
        fps: if fps.is_finite() && fps > 1.0 {
            fps
        } else {
            30.0
        },
        duration_ms,
        codec_name,
    })
}

fn short_url(url: &str) -> String {
    if url.len() <= 96 {
        return url.to_string();
    }
    format!("{}…", &url[..96])
}

#[cfg(test)]
mod tests {
    use super::*;
    use o11a1::bilibili::{AudioTrack, PlayInfo};

    fn audio(id: i64, bandwidth: u64, vip: bool, hi_res: bool) -> AudioTrack {
        AudioTrack {
            id,
            bandwidth,
            url: format!("https://example.invalid/{id}"),
            urls: Vec::new(),
            vip,
            dolby: false,
            hi_res,
        }
    }

    fn play_info(audio_tracks: Vec<AudioTrack>) -> PlayInfo {
        PlayInfo {
            video_tracks: Vec::new(),
            qualities: Vec::new(),
            default_qn: 80,
            audio_tracks,
            duration: 0,
        }
    }

    #[test]
    fn best_audio_skips_vip_and_prefers_the_highest_bitrate() {
        let info = play_info(vec![
            audio(30216, 64_000, false, false),
            audio(30280, 192_000, false, false),
            audio(30251, 1_500_000, true, true),
        ]);
        assert_eq!(best_audio_index(&info), 1);
    }

    #[test]
    fn all_vip_audio_still_selects_the_first_stream() {
        let info = play_info(vec![
            audio(30250, 448_000, true, false),
            audio(30251, 1_500_000, true, true),
        ]);
        assert_eq!(best_audio_index(&info), 0);
        assert_eq!(best_audio_index(&play_info(vec![])), 0);
    }

    #[test]
    fn track_requests_are_strict_only_for_audio() {
        let quality = TrackRequest::quality(116, 12);
        assert_eq!(quality.qn, 116);
        assert_eq!(quality.codecid, 12);
        assert_eq!(quality.audio_index, None);
        let audio = TrackRequest::audio(2);
        assert_eq!(audio.qn, 0);
        assert_eq!(audio.audio_index, Some(2));
    }
}
