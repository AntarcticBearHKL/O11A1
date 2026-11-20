//! 画面通道：720P 整片临时下载 → 按用户间隔抽帧 → 拼成 6 格大图 →
//! DeepSeek 逐张细看.
//!
//! The whole video is a **temporary** file: it is deleted as soon as the frames
//! are extracted, before any vision request. The interactive 画面 tab and the
//! watch step share [`lab`], so both show the same timestamps and sheets. The
//! lab exposes six 960×540 tiles per sheet (landscape) or six 540×960 tiles
//! (portrait), one image per vision request.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use super::steps::{SkipReason, Step, StepFuture, StepId, StepOutput};
use super::{FrameGathered, WatchConfig, WatchCtx};

pub mod lab;

pub struct FramesStep;

/// Why the frames channel cannot run, or `None` when it can. Shared by the
/// step's pre-flight check and the settings row so they never disagree.
pub fn preflight(config: &WatchConfig) -> Option<String> {
    if !config.online {
        return Some("未登录".to_string());
    }
    if config.deepseek.key.trim().is_empty() {
        return Some("未配置 DeepSeek key（视觉要用同一个 key）".to_string());
    }
    if !lab::ffmpeg_available(&config.ffmpeg) {
        return Some("ffmpeg 不在 PATH".to_string());
    }
    None
}

impl FramesStep {
    pub fn new(_config: &WatchConfig) -> Self {
        FramesStep
    }
}

impl Step for FramesStep {
    fn id(&self) -> StepId {
        StepId::Frames
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(1_800)
    }

    fn skip(&self, config: &WatchConfig) -> Option<SkipReason> {
        preflight(config).map(SkipReason::new)
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a> {
        Box::pin(async move {
            let Some(session) = ctx.session.clone() else {
                return Ok(StepOutput {
                    skip_reason: Some("未登录".to_string()),
                    ..StepOutput::default()
                });
            };
            let meta = ctx.gathered.meta.clone();
            if meta.cid == 0 {
                return Ok(StepOutput {
                    skip_reason: Some("缺少 cid".to_string()),
                    ..StepOutput::default()
                });
            }
            if ctx.config.deepseek.key.trim().is_empty() {
                return Ok(StepOutput {
                    skip_reason: Some("未配置 DeepSeek key".to_string()),
                    ..StepOutput::default()
                });
            }
            let cancel = ctx
                .cancel
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
            let interval = ctx.config.vision.interval_s;
            ctx.note(format!(
                "准备抽帧 · {}",
                if interval == 0 {
                    "自动间隔".to_string()
                } else {
                    format!("每 {interval}s")
                }
            ));
            let built = match lab::build_with_meta(
                &ctx.config,
                &session,
                meta,
                interval,
                None,
                cancel.clone(),
            )
            .await
            {
                Ok(built) => built,
                Err(error) => {
                    return Ok(StepOutput {
                        skip_reason: Some(format!("frames_failed: {error:#}")),
                        ..StepOutput::default()
                    });
                }
            };
            if ctx.cancelled() {
                let _ = std::fs::remove_dir_all(&built.dir);
                return Ok(StepOutput {
                    skip_reason: Some("已取消".to_string()),
                    bytes: built.video_bytes,
                    ..StepOutput::default()
                });
            }
            ctx.note(format!(
                "AI 细看 {} 张拼图 · {} 帧",
                built.sheets.len(),
                built.frame_count
            ));
            let observed = lab::observe(
                &ctx.config,
                &ctx.http,
                &built,
                "按看片判断需要，重点记录画面事实、屏幕文字、广告/恰饭线索、重复画面和明显的剪辑结构。",
                None,
                cancel,
            )
            .await;
            let _ = std::fs::remove_dir_all(&built.dir);
            let observed = match observed {
                Ok(observed) => observed,
                Err(error) => {
                    return Ok(StepOutput {
                        skip_reason: Some(format!("frames_failed: {error:#}")),
                        bytes: built.video_bytes + built.image_bytes,
                        ..StepOutput::default()
                    });
                }
            };
            let observations = lab::observations_text(&observed);
            let tokens = observed.tokens;
            let frames = built.frame_count;
            let sheets = built.sheets.len();
            let interval_s = built.interval_s;
            let video_bytes = built.video_bytes;
            let image_bytes = built.image_bytes;
            ctx.gathered.frames = Some(FrameGathered {
                frames,
                interval_s: interval_s as f64,
                video_bytes,
                image_bytes,
                observations,
                batches: sheets,
                tokens,
            });
            crate::debug_log(&format!(
                "fish frames done frames={frames} sheets={sheets} interval_s={interval_s} video_bytes={video_bytes} jpeg_bytes={image_bytes} tokens={tokens}"
            ));
            Ok(StepOutput {
                detail: format!(
                    "{frames} 帧 · {sheets} 张拼图 · {} · {tokens} tok",
                    crate::fishwatch::asr::models::human_bytes(image_bytes)
                ),
                bytes: video_bytes + image_bytes,
                tokens,
                note: if observed.failed > 0 {
                    format!("画面有 {}/{} 张拼图观察失败", observed.failed, sheets)
                } else {
                    String::new()
                },
                ..StepOutput::default()
            })
        })
    }
}
