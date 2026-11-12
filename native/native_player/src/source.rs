//! Resolve what to play: a Bilibili `bvid`/`cid` pair or a direct media URL,
//! plus the size, duration, frame rate and CDN headers the decoder needs.

use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};

use crate::format::fit_capped;
use crate::proc;
use crate::{log, PlayerConfig};

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
        .or_else(|| config.direct_url.clone().filter(|value| !value.trim().is_empty()));
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
    let (source_width, source_height, mut duration_ms, fps) = match probe {
        Some(probe) => (probe.width, probe.height, probe.duration_ms, probe.fps),
        None => (1280, 720, 0, 30.0),
    };
    if duration_ms == 0 && config.start_ms > 0 {
        duration_ms = 0;
    }
    let (width, height) = fit_capped(source_width, source_height, 1920, 1080);
    let title = if !config.title.trim().is_empty() {
        config.title.clone()
    } else {
        url.rsplit(['/', '\\']).next().unwrap_or("本地视频").to_string()
    };
    log::line(&format!(
        "source direct url={}{} size={}x{} -> {}x{} fps={:.3} duration_ms={} remote={}",
        if remote { "" } else { "(local) " },
        short_url(url),
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
            video_tracks: Vec::new(),
            audio_tracks: Vec::new(),
            quality_tiers: Vec::new(),
            remote,
        },
        session: None,
    })
}

fn resolve_bilibili(bvid: &str, config: &PlayerConfig) -> Result<ResolvedSource> {
    let runtime = o11a1::bilibili::shared_runtime()
        .ok_or_else(|| anyhow!("network runtime unavailable"))?;
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
        let requested_qn = config.quality_qn.unwrap_or(80);
        let requested_codec = config.quality_codecid.unwrap_or(7);
        let track = info
            .video_track_for(requested_qn, requested_codec)
            .or_else(|| info.video_track_for(info.default_qn, requested_codec))
            .or_else(|| info.video_tracks.first())
            .ok_or_else(|| anyhow!("playurl returned no video tracks"))?;
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
        let (width, height) = fit_capped(source_width, source_height, 1920, 1080);
        let video_tracks = video_track_infos(&info);
        let audio_tracks = audio_track_infos(&info);
        let quality_tiers = quality_tier_infos(&bvid_owned, cid);
        log::line(&format!(
            "source bilibili bvid={} cid={cid} login={} title={:?} qn={} codec={} size={}x{} -> {}x{} fps={:.3} duration_ms={} audio={} audio_index={} tracks={} tiers={} headers={}",
            bvid_owned,
            u8::from(logged_in),
            title,
            track.id,
            o11a1::bilibili::codec_label(track.codecid),
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
                video_tracks,
                audio_tracks,
                quality_tiers,
                remote: true,
            },
            Some(session),
        ))
    });
    let (source, session) = resolved?;
    Ok(ResolvedSource {
        source,
        session,
    })
}

struct ProbeInfo {
    width: u32,
    height: u32,
    fps: f64,
    duration_ms: u64,
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

fn video_track_infos(info: &o11a1::bilibili::PlayInfo) -> Vec<VideoTrackInfo> {
    info.video_tracks
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
    let runtime = o11a1::bilibili::shared_runtime()
        .ok_or_else(|| anyhow!("network runtime unavailable"))?;
    let info = runtime
        .block_on(session.playurl(bvid, cid))
        .with_context(|| format!("playurl {bvid} cid={cid}"))?;
    let wanted_qn = if request.qn > 0 { request.qn } else { current_qn };
    let wanted_codecid = if request.qn > 0 {
        request.codecid
    } else {
        current_codecid
    };
    let track = info
        .video_tracks
        .iter()
        .filter(|track| {
            track.id == wanted_qn
                && (wanted_codecid == 0 || track.codecid == wanted_codecid)
        })
        .max_by_key(|track| track.bandwidth)
        .or_else(|| {
            // 音轨-only switch: keep the exact current representation.
            info.video_tracks
                .iter()
                .filter(|track| track.id == current_qn && track.codecid == current_codecid)
                .max_by_key(|track| track.bandwidth)
        })
        .ok_or_else(|| {
            anyhow!(
                "该画质不可用（{} · {}，可能需要大会员）",
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
    let (width, height) = fit_capped(source_width, source_height, 1920, 1080);
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
        video_tracks: video_track_infos(&info),
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
        .arg("stream=width,height,avg_frame_rate")
        .arg("-of")
        .arg("csv=p=0");
    if remote {
        let text = proc::header_string(headers);
        if !text.is_empty() {
            command.arg("-headers").arg(text);
        }
    }
    command.arg(url).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let parts: Vec<&str> = first.trim().split(',').collect();
    let width: u32 = parts.first()?.trim().parse().ok()?;
    let height: u32 = parts.get(1)?.trim().parse().ok()?;
    let fps = parts
        .get(2)
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
        fps: if fps.is_finite() && fps > 1.0 { fps } else { 30.0 },
        duration_ms,
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
