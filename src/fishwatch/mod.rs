//! 看片 steps: the auditable skeleton between "a feed card" and "a verdict".
//!
//! The design rule is that every channel the fish uses is one step with an id, a
//! label, an estimate, an outcome and a reason when it did not run. A tier is
//! only a preset of step ids; the UI may edit each switch, which makes the run
//! "自定义" without changing the code path.
//!
//! This module owns no UI and reads no settings: [`WatchConfig`] is built by
//! `fish.rs` from `settings.json`, and the UI only ever sees [`WatchOutcome`]
//! (specifically its [`Evidence`]).

// The module surface is consumed by `fish.rs` and (from phase 1's UI pass) by
// `main.rs`; a few helpers exist for the next phase before their call site.

pub mod asr;
pub mod comments;
pub mod cost;
pub mod coverage;
pub mod evidence;
pub mod frames;
pub mod proc;
pub mod steps;
pub mod transcript;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bilibili::{Conclusion, OutlineSection, Session};
use crate::fishwatch::comments::{CommentItem, DanmakuPeak};
use crate::fishwatch::cost::CostBook;
use crate::fishwatch::evidence::{Evidence, StepRecord};
use crate::fishwatch::steps::{Step, StepFuture, StepId, StepOutput, StepReport};
use crate::fishwatch::transcript::{ConclusionGate, Segment};

// ---------------------------------------------------------------------------
// Tiers and per-step selection
// ---------------------------------------------------------------------------

/// Which steps one run uses. The five tiers are presets; `custom` is whatever
/// the owner left switched on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepSelection {
    pub meta: bool,
    pub subtitle: bool,
    pub outline: bool,
    pub comments: bool,
    pub asr: bool,
    pub frames: bool,
}

impl Default for StepSelection {
    /// T1, the shipped default.
    fn default() -> Self {
        Self::for_tier(1)
    }
}

impl StepSelection {
    /// The preset for one tier id (`0..=4`); an unknown id is T1.
    pub fn for_tier(tier: u32) -> Self {
        match tier {
            0 => StepSelection {
                meta: true,
                ..StepSelection::none()
            },
            1 => StepSelection {
                meta: true,
                subtitle: true,
                comments: true,
                ..StepSelection::none()
            },
            2 => StepSelection {
                meta: true,
                subtitle: true,
                outline: true,
                comments: true,
                ..StepSelection::none()
            },
            3 => StepSelection {
                meta: true,
                comments: true,
                asr: true,
                ..StepSelection::none()
            },
            4 => StepSelection {
                meta: true,
                comments: true,
                asr: true,
                frames: true,
                ..StepSelection::none()
            },
            _ => StepSelection::for_tier(1),
        }
    }

    pub fn none() -> Self {
        StepSelection {
            meta: false,
            subtitle: false,
            outline: false,
            comments: false,
            asr: false,
            frames: false,
        }
    }

    /// The tier this exact combination matches, if any. `None` = 自定义.
    #[cfg(test)]
    pub fn tier(&self) -> Option<u32> {
        (0..=4).find(|tier| &StepSelection::for_tier(*tier) == self)
    }

    pub fn get(&self, id: StepId) -> bool {
        match id {
            StepId::Meta => self.meta,
            StepId::Subtitle => self.subtitle,
            StepId::Outline => self.outline,
            StepId::Comments => self.comments,
            StepId::Asr => self.asr,
            StepId::Frames => self.frames,
        }
    }

    #[cfg(test)]
    pub fn set(&mut self, id: StepId, on: bool) {
        match id {
            StepId::Meta => self.meta = on,
            StepId::Subtitle => self.subtitle = on,
            StepId::Outline => self.outline = on,
            StepId::Comments => self.comments = on,
            StepId::Asr => self.asr = on,
            StepId::Frames => self.frames = on,
        }
    }

    /// Step ids in execution order. Meta first because every other step reads
    /// `aid`/`cid` from it; ASR last in the text tiers because it is the slow
    /// one; frames after that.
    pub fn ids(&self) -> Vec<StepId> {
        StepId::ALL
            .into_iter()
            .filter(|id| self.get(*id))
            .collect()
    }

    /// A run with no steps would still ask DeepSeek for a verdict on nothing;
    /// this keeps the UI from producing that state.
    pub fn is_empty(&self) -> bool {
        self.ids().is_empty()
    }
}

// ---------------------------------------------------------------------------
// Configuration (built by fish.rs from settings.json)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsrOptions {
    pub enabled: bool,
    /// `auto` | `faster-whisper` | `whisper.cpp`.
    pub engine: String,
    /// `gpu` | `cpu`.
    pub device: String,
    /// `small` | `turbo` | `medium`.
    pub model: String,
    pub threads: u32,
}

impl Default for AsrOptions {
    fn default() -> Self {
        AsrOptions {
            enabled: false,
            engine: "auto".to_string(),
            device: "gpu".to_string(),
            model: "small".to_string(),
            threads: 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisionOptions {
    /// `low` | `original` for the OpenAI-compatible image payload.
    pub detail: String,
    /// Images per request (the API can take more; batching keeps each prompt
    /// small and makes a bad batch auditable).
    pub batch: usize,
}

impl Default for VisionOptions {
    fn default() -> Self {
        VisionOptions {
            detail: "low".to_string(),
            batch: 10,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepseekOptions {
    pub key: String,
    pub model: String,
    pub endpoint: String,
}

impl Default for DeepseekOptions {
    fn default() -> Self {
        DeepseekOptions {
            key: String::new(),
            model: "deepseek-flash".to_string(),
            endpoint: "https://api.deepseek.com/chat/completions".to_string(),
        }
    }
}

/// Everything a run may use, snapshotted once. `online` reflects whether a
/// Bilibili session exists; the steps that need one pre-flight-skip with
/// `未登录` instead of failing mid-run.
#[derive(Clone, Debug)]
pub struct WatchConfig {
    pub selection: StepSelection,
    pub online: bool,
    pub asr: AsrOptions,
    pub vision: VisionOptions,
    pub deepseek: DeepseekOptions,
    pub model_dir: PathBuf,
    pub audio_dir: PathBuf,
    pub work_dir: PathBuf,
    pub ffmpeg: String,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            selection: StepSelection::default(),
            online: false,
            asr: AsrOptions::default(),
            vision: VisionOptions::default(),
            deepseek: DeepseekOptions::default(),
            model_dir: crate::paths::root().join("models/whisper"),
            audio_dir: crate::paths::root().join("cache/fishaudio"),
            work_dir: std::env::temp_dir().join("o11a1-fishwatch"),
            ffmpeg: "ffmpeg".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Materials
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub bvid: String,
    pub cid: u64,
    pub aid: i64,
    pub title: String,
    pub up_mid: u64,
    pub up_name: String,
    pub duration_s: u64,
    pub view: u64,
    pub pub_ts: i64,
    pub cover: String,
    pub tname: String,
    pub desc: String,
}

/// One video's material, accumulated by the steps.
#[derive(Clone, Debug, Default)]
pub struct Gathered {
    pub meta: Meta,
    pub summary: String,
    pub outline: Vec<OutlineSection>,
    pub transcript: String,
    pub transcript_source: String,
    pub transcript_segments: Vec<Segment>,
    pub comments: Vec<CommentItem>,
    pub danmaku: Vec<DanmakuPeak>,
    pub frames: Option<FrameGathered>,
    /// `conclusion/get` payload, cached for the outline step.
    pub conclusion: Option<Conclusion>,
}


/// What the frame step produced (phase 3; kept here so coverage and the prompt
/// share one shape).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameGathered {
    pub frames: usize,
    pub interval_s: f64,
    pub video_bytes: u64,
    pub image_bytes: u64,
    pub observations: String,
    pub batches: usize,
    pub tokens: u64,
}

// ---------------------------------------------------------------------------
// Runtime context
// ---------------------------------------------------------------------------

/// Mutable context one run threads through its steps. Owned (not borrowed) so
/// the boxed step futures keep ordinary lifetimes.
#[derive(Clone)]
pub struct WatchCtx {
    pub config: WatchConfig,
    pub session: Option<Arc<Session>>,
    pub http: reqwest::Client,
    pub conclusion_gate: Arc<ConclusionGate>,
    /// True while any player is playing/buffering. ASR polls this and waits.
    pub playback_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    pub gathered: Gathered,
}

impl WatchCtx {
    pub fn new(config: WatchConfig, meta: Meta) -> Self {
        WatchCtx {
            config,
            session: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            conclusion_gate: Arc::new(ConclusionGate::default()),
            playback_gate: None,
            gathered: Gathered {
                meta,
                ..Gathered::default()
            },
        }
    }

    pub fn with_session(mut self, session: Arc<Session>) -> Self {
        self.session = Some(session);
        self
    }

    pub fn with_gate(mut self, gate: Arc<ConclusionGate>) -> Self {
        self.conclusion_gate = gate;
        self
    }

    pub fn with_playback_gate(mut self, gate: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.playback_gate = Some(gate);
        self
    }
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// The steps this run will execute, plus the ones that were skipped before the
/// run started (with the reason that will be shown in the evidence panel).
pub struct WatchPlan {
    steps: Vec<Box<dyn Step>>,
    pre_skips: Vec<StepReport>,
}

impl WatchPlan {
    /// Resolve a config into an executable plan. Steps whose pre-flight check
    /// fails become `skipped` reports — never a silently shorter list.
    pub fn build(config: &WatchConfig) -> WatchPlan {
        let mut steps: Vec<Box<dyn Step>> = Vec::new();
        let mut pre_skips: Vec<StepReport> = Vec::new();
        for id in config.selection.ids() {
            let step = make_step(id, config);
            if let Some(reason) = step.skip(config) {
                crate::debug_log(&format!(
                    "fish step skipped id={} reason={}",
                    id.as_str(),
                    reason
                ));
                pre_skips.push(StepReport::skipped(id, reason.0));
                continue;
            }
            steps.push(step);
        }
        WatchPlan { steps, pre_skips }
    }

    #[cfg(test)]
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    #[cfg(test)]
    pub fn skipped_count(&self) -> usize {
        self.pre_skips.len()
    }

    /// Ceiling for the whole run: every step's own timeout plus a small margin
    /// for persistence. ASR and frames make this much larger than the old
    /// fixed 75 s, which is the point.
    pub fn timeout(&self) -> Duration {
        let total: u64 = self
            .steps
            .iter()
            .map(|step| step.timeout().as_secs())
            .sum();
        Duration::from_secs(total.saturating_add(30))
    }
}

/// `754` → `12:34`, `3725` → `1:02:05`.
fn mmss(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let rest = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{rest:02}")
    } else {
        format!("{minutes}:{rest:02}")
    }
}

fn make_step(id: StepId, config: &WatchConfig) -> Box<dyn Step> {
    match id {
        StepId::Meta => Box::new(MetaStep),
        StepId::Subtitle => Box::new(transcript::SubtitleStep),
        StepId::Outline => Box::new(transcript::OutlineStep),
        StepId::Comments => Box::new(comments::CommentsStep),
        StepId::Asr => Box::new(asr::AsrStep::new(config)),
        StepId::Frames => Box::new(frames::FramesStep::new(config)),
    }
}

// ---------------------------------------------------------------------------
// Steps that live in this file
// ---------------------------------------------------------------------------

/// `meta`: feed metadata first, then `x/web-interface/wbi/view` when online.
pub struct MetaStep;

impl Step for MetaStep {
    fn id(&self) -> StepId {
        StepId::Meta
    }


    fn timeout(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a> {
        Box::pin(async move {
            let Some(session) = ctx.session.clone() else {
                return Ok(StepOutput {
                    detail: "feed 元数据".to_string(),
                    ..StepOutput::default()
                });
            };
            match session.fish_meta(&ctx.gathered.meta.bvid).await {
                Ok(meta) => {
                    let bytes = (meta.title.len()
                        + meta.up_name.len()
                        + meta.cover.len()
                        + meta.tname.len()
                        + meta.desc.len()) as u64;
                    let gathered = &mut ctx.gathered;
                    if !meta.title.trim().is_empty() {
                        gathered.meta.title = meta.title;
                    }
                    if meta.cid > 0 {
                        gathered.meta.cid = meta.cid;
                    }
                    if meta.aid != 0 {
                        gathered.meta.aid = meta.aid;
                    }
                    if meta.up_mid > 0 {
                        gathered.meta.up_mid = meta.up_mid;
                    }
                    if !meta.up_name.trim().is_empty() {
                        gathered.meta.up_name = meta.up_name;
                    }
                    if meta.duration_s > 0 {
                        gathered.meta.duration_s = meta.duration_s;
                    }
                    if meta.view > 0 {
                        gathered.meta.view = meta.view;
                    }
                    if meta.pub_ts > 0 {
                        gathered.meta.pub_ts = meta.pub_ts;
                    }
                    if !meta.cover.is_empty() {
                        gathered.meta.cover = meta.cover;
                    }
                    if !meta.tname.trim().is_empty() {
                        gathered.meta.tname = meta.tname;
                    }
                    if !meta.desc.trim().is_empty() {
                        gathered.meta.desc = meta.desc;
                    }
                    crate::debug_log(&format!(
                        "fish step id=meta cid={} duration_s={} tname={}",
                        ctx.gathered.meta.cid, ctx.gathered.meta.duration_s, ctx.gathered.meta.tname
                    ));
                    Ok(StepOutput {
                        detail: format!(
                            "cid={} · {} · {}",
                            ctx.gathered.meta.cid,
                            mmss(ctx.gathered.meta.duration_s),
                            if ctx.gathered.meta.tname.is_empty() {
                                "未知分区"
                            } else {
                                ctx.gathered.meta.tname.as_str()
                            }
                        ),
                        bytes,
                        ..StepOutput::default()
                    })
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "fish metadata bvid={} failed: {err:#}",
                        ctx.gathered.meta.bvid
                    ));
                    Ok(StepOutput {
                        detail: "feed 元数据（接口失败）".to_string(),
                        note: format!("元数据接口失败：{err:#}"),
                        ..StepOutput::default()
                    })
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

pub struct WatchRun {
    plan: WatchPlan,
    ctx: WatchCtx,
    started: Instant,
}

pub struct WatchOutcome {
    pub gathered: Gathered,
    pub evidence: Evidence,
}

impl WatchRun {
    pub fn new(ctx: WatchCtx) -> Self {
        let plan = WatchPlan::build(&ctx.config);
        WatchRun {
            plan,
            ctx,
            started: Instant::now(),
        }
    }

    pub fn timeout(&self) -> Duration {
        self.plan.timeout()
    }

    /// Execute every planned step. A step error is recorded and the run
    /// continues: one unavailable channel must not throw away the channels that
    /// did work.
    pub async fn execute(mut self) -> WatchOutcome {
        let mut reports: Vec<StepReport> = self.plan.pre_skips.clone();
        let steps = std::mem::take(&mut self.plan.steps);
        for step in steps {
            let id = step.id();
            let started = Instant::now();
            let result = tokio::time::timeout(step.timeout(), step.run(&mut self.ctx)).await;
            let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            let mut report = match result {
                Ok(Ok(output)) => {
                    if let Some(reason) = output.skip_reason.clone() {
                        let mut report = StepReport::skipped(id, reason);
                        report.bytes = output.bytes;
                        report.tokens = output.tokens;
                        report.detail = output.detail;
                        crate::debug_log(&format!(
                            "fish step id={} state=skipped ms={ms} reason={}",
                            id.as_str(),
                            report.reason
                        ));
                        report.ms = ms;
                        reports.push(report);
                        continue;
                    }
                    let mut report = StepReport::ok(id);
                    report.bytes = output.bytes;
                    report.tokens = output.tokens;
                    report.detail = output.detail;
                    if !output.note.is_empty() {
                        report.reason = output.note;
                    }
                    crate::debug_log(&format!(
                        "fish step id={} state=ok ms={ms} bytes={} tokens={} detail={}",
                        id.as_str(),
                        report.bytes,
                        report.tokens,
                        report.detail
                    ));
                    report
                }
                Ok(Err(err)) => {
                    let reason = format!("{err:#}");
                    crate::debug_log(&format!(
                        "fish step id={} state=failed ms={ms} reason={reason}",
                        id.as_str()
                    ));
                    StepReport::failed(id, reason)
                }
                Err(_) => {
                    let reason = format!("{} 超时（{}s）", id.label(), step.timeout().as_secs());
                    crate::debug_log(&format!(
                        "fish step id={} state=failed ms={ms} reason={reason}",
                        id.as_str()
                    ));
                    StepReport::failed(id, reason)
                }
            };
            report.ms = ms;
            // A failed step still contributes its measured time/bytes.
            reports.push(report);
        }
        reports.sort_by_key(|report| {
            StepId::ALL
                .iter()
                .position(|id| *id == report.id)
                .unwrap_or(usize::MAX)
        });

        let coverage = coverage::compute(&self.ctx.gathered, &reports);
        let channels: Vec<&str> = reports
            .iter()
            .filter(|report| report.is_ok())
            .map(|report| report.id.as_str())
            .collect();
        let mut book = CostBook::default();
        for report in &reports {
            book.add_step(report);
        }
        let evidence = Evidence {
            channels: channels.join(","),
            coverage: coverage.fraction,
            coverage_note: coverage.note,
            cost_ms: book.ms,
            cost_bytes: book.bytes,
            cost_tokens: book.tokens,
            steps: reports.iter().map(StepRecord::from_report).collect(),
        };
        crate::debug_log(&format!(
            "fish watch done steps={} skipped={} channels={} coverage={:.2} bytes={} ms={}",
            reports.iter().filter(|report| report.is_ok()).count(),
            reports.iter().filter(|report| !report.is_ok()).count(),
            channels.join(","),
            coverage.fraction,
            evidence.cost_bytes,
            self.started.elapsed().as_millis()
        ));
        WatchOutcome {
            gathered: self.ctx.gathered,
            evidence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_match_the_design_table() {
        let t0 = StepSelection::for_tier(0);
        assert!(t0.meta && !t0.subtitle && !t0.outline && !t0.comments && !t0.asr && !t0.frames);
        let t1 = StepSelection::for_tier(1);
        assert!(t1.meta && t1.subtitle && t1.comments);
        assert!(!t1.outline && !t1.asr && !t1.frames);
        let t2 = StepSelection::for_tier(2);
        assert!(t2.outline && t2.subtitle && t2.comments && !t2.asr && !t2.frames);
        let t3 = StepSelection::for_tier(3);
        assert!(t3.meta && t3.comments && t3.asr);
        assert!(!t3.subtitle && !t3.outline && !t3.frames);
        let t4 = StepSelection::for_tier(4);
        assert!(t4.asr && t4.frames && t4.comments && !t4.subtitle);
        for tier in 0..=4 {
            assert_eq!(StepSelection::for_tier(tier).tier(), Some(tier));
        }
    }

    #[test]
    fn a_hand_edited_selection_reports_custom() {
        let mut selection = StepSelection::for_tier(1);
        selection.asr = true;
        assert_eq!(selection.tier(), None, "T1 + ASR is not a preset");
        assert_eq!(
            selection.ids(),
            vec![StepId::Meta, StepId::Subtitle, StepId::Comments, StepId::Asr]
        );
        selection.set(StepId::Asr, false);
        assert_eq!(selection.tier(), Some(1));
    }

    #[test]
    fn every_step_has_a_label() {
        for id in StepId::ALL {
            assert!(!id.label().is_empty());
            assert!(!id.as_str().is_empty());
            assert_eq!(StepId::from_str(id.as_str()), Some(id));
        }
    }

    #[test]
    fn plan_keeps_unavailable_steps_as_skipped() {
        let config = WatchConfig {
            selection: StepSelection::for_tier(4),
            online: false,
            ..WatchConfig::default()
        };
        let plan = WatchPlan::build(&config);
        // meta still runs offline; comments/asr/frames are skipped with reasons.
        assert_eq!(plan.step_count(), 1);
        assert_eq!(plan.skipped_count(), 3, "未登录 / asr / frames");
        assert!(plan.timeout() >= Duration::from_secs(30));
    }
}

