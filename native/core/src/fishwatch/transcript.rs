//! A1 官方字幕 / A2 AI 提纲.
//!
//! Both come from the same signed `conclusion/get` response, so the run caches
//! the decoded payload: asking for the outline after the subtitle must not pay
//! for a second Bilibili request (and must not break the risk-control spacing).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::bilibili::{Conclusion, Session};
use crate::fishwatch::steps::{SkipReason, Step, StepFuture, StepId, StepOutput};
use crate::fishwatch::{Gathered, WatchConfig, WatchCtx};

/// One timestamped piece of speech, from either the official transcript or ASR.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Segment {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}

impl Segment {
    pub fn text_of(segments: &[Segment]) -> String {
        segments
            .iter()
            .map(|segment| segment.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn chars_of(segments: &[Segment]) -> usize {
        segments
            .iter()
            .map(|segment| segment.text.trim().chars().count())
            .sum()
    }
}

/// Serialises `conclusion/get` calls across the whole engine: at least
/// [`MIN_GAP_MS`] between starts, with exponential backoff on `-352` / `-412`.
/// One instance lives in [`crate::fish::FishEngine`] for the life of the process.
pub struct ConclusionGate {
    last: Mutex<Option<Instant>>,
    min_gap: Duration,
    max_retries: u32,
}

/// Minimum spacing between two `conclusion/get` calls (Bilibili risk control).
const MIN_GAP_MS: u64 = 1_500;
const MAX_RETRIES: u32 = 3;

impl Default for ConclusionGate {
    fn default() -> Self {
        Self::new(Duration::from_millis(MIN_GAP_MS), MAX_RETRIES)
    }
}

impl ConclusionGate {
    pub fn new(min_gap: Duration, max_retries: u32) -> Self {
        ConclusionGate {
            last: Mutex::new(None),
            min_gap,
            max_retries,
        }
    }

    pub async fn fetch(
        &self,
        session: &Session,
        bvid: &str,
        cid: u64,
        up_mid: u64,
    ) -> Result<Conclusion> {
        let mut attempt = 0u32;
        loop {
            let wait = self
                .last
                .lock()
                .ok()
                .and_then(|guard| *guard)
                .map(|at| self.min_gap.saturating_sub(at.elapsed()));
            if let Some(wait) = wait {
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
            if let Ok(mut guard) = self.last.lock() {
                *guard = Some(Instant::now());
            }
            match session.conclusion(bvid, cid, up_mid).await {
                Ok(conclusion) => return Ok(conclusion),
                Err(err) => {
                    if !crate::bilibili::is_risk_control(&err) || attempt >= self.max_retries {
                        return Err(err);
                    }
                    attempt += 1;
                    let backoff = self.min_gap * 2u32.pow(attempt);
                    crate::debug_log(&format!(
                        "fish backoff bvid={bvid} attempt={attempt} wait_ms={}",
                        backoff.as_millis()
                    ));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
}

/// Shared `conclusion/get` payload cache for one run (see module docs).
fn cached_conclusion(ctx: &mut WatchCtx) -> Option<Conclusion> {
    ctx.gathered.conclusion.clone()
}

async fn fetch_conclusion(ctx: &mut WatchCtx) -> Result<Conclusion> {
    let Some(session) = ctx.session.clone() else {
        return Err(anyhow!("未登录"));
    };
    let bvid = ctx.gathered.meta.bvid.clone();
    let cid = ctx.gathered.meta.cid;
    let up_mid = ctx.gathered.meta.up_mid;
    let gate = ctx.conclusion_gate.clone();
    let conclusion = gate.fetch(&*session, &bvid, cid, up_mid).await?;
    ctx.gathered.conclusion = Some(conclusion.clone());
    Ok(conclusion)
}

/// A1: the official transcript (and the AI summary that arrives with it).
pub struct SubtitleStep;

impl Step for SubtitleStep {
    fn id(&self) -> StepId {
        StepId::Subtitle
    }


    fn timeout(&self) -> Duration {
        Duration::from_secs(45)
    }

    fn skip(&self, config: &WatchConfig) -> Option<SkipReason> {
        if config.online {
            None
        } else {
            Some(SkipReason::new("未登录"))
        }
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a> {
        Box::pin(async move {
            if ctx.session.is_none() {
                return Err(anyhow!("未登录"));
            }
            if ctx.gathered.meta.cid == 0 {
                return Err(anyhow!("缺少 cid"));
            }
            let conclusion = match cached_conclusion(ctx) {
                Some(conclusion) => conclusion,
                None => fetch_conclusion(ctx).await?,
            };
            if !conclusion.summary.trim().is_empty() {
                ctx.gathered.summary = conclusion.summary.clone();
            }
            let segments: Vec<Segment> = conclusion
                .cues
                .iter()
                .filter(|cue| !cue.text.trim().is_empty())
                .map(|cue| Segment {
                    start_s: cue.start_s as f64,
                    end_s: cue.end_s as f64,
                    text: cue.text.trim().to_string(),
                })
                .collect();
            let chars = Segment::chars_of(&segments);
            if !segments.is_empty() {
                ctx.gathered.transcript_source = "official".to_string();
                ctx.gathered.transcript = Segment::text_of(&segments);
                ctx.gathered.transcript_segments = segments.clone();
            }
            let bytes = conclusion.summary.len() as u64
                + conclusion
                    .cues
                    .iter()
                    .map(|cue| cue.text.len() as u64)
                    .sum::<u64>();
            Ok(StepOutput {
                detail: if segments.is_empty() {
                    "接口没有给出字幕".to_string()
                } else {
                    format!("{} 句 / {} 字", segments.len(), chars)
                },
                bytes,
                ..StepOutput::default()
            })
        })
    }
}

/// A2: the AI outline (timestamped sections). Absence is a normal state for
/// PGC / 番剧 clips, so an empty outline is `ok`, not an error.
pub struct OutlineStep;

impl Step for OutlineStep {
    fn id(&self) -> StepId {
        StepId::Outline
    }


    fn timeout(&self) -> Duration {
        Duration::from_secs(45)
    }

    fn skip(&self, config: &WatchConfig) -> Option<SkipReason> {
        if config.online {
            None
        } else {
            Some(SkipReason::new("未登录"))
        }
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a> {
        Box::pin(async move {
            if ctx.session.is_none() {
                return Err(anyhow!("未登录"));
            }
            if ctx.gathered.meta.cid == 0 {
                return Err(anyhow!("缺少 cid"));
            }
            let conclusion = match cached_conclusion(ctx) {
                Some(conclusion) => conclusion,
                None => fetch_conclusion(ctx).await?,
            };
            let sections = conclusion.outline.clone();
            let bytes = sections
                .iter()
                .map(|section| {
                    section.title.len() as u64
                        + section
                            .points
                            .iter()
                            .map(|point| point.content.len() as u64)
                            .sum::<u64>()
                })
                .sum::<u64>();
            if sections.is_empty() {
                return Ok(StepOutput {
                    detail: "这个视频没有提纲".to_string(),
                    bytes,
                    ..StepOutput::default()
                });
            }
            let points: usize = sections.iter().map(|section| section.points.len()).sum();
            ctx.gathered.outline = sections;
            Ok(StepOutput {
                detail: format!("{} 节 / {} 个要点", ctx.gathered.outline.len(), points),
                bytes,
                ..StepOutput::default()
            })
        })
    }
}

/// Attach a transcript produced by ASR. Kept here so official and ASR
/// transcripts share one representation and one coverage calculation.
pub fn attach_asr(
    gathered: &mut Gathered,
    segments: Vec<Segment>,
    source: &str,
) -> (usize, usize) {
    let chars = Segment::chars_of(&segments);
    let count = segments.len();
    gathered.transcript_source = source.to_string();
    gathered.transcript = Segment::text_of(&segments);
    gathered.transcript_segments = segments;
    (count, chars)
}
