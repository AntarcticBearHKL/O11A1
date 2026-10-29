//! The step contract every 看片 step implements.
//!
//! A step is deliberately tiny: it owns no storage, reads no settings and never
//! touches the UI. Everything it needs arrives through [`WatchCtx`]; everything
//! it produced ends up in that same context plus the returned [`StepOutput`].
//! A step that cannot run is *skipped with a reason* instead of quietly
//! disappearing from the run — the evidence line depends on knowing that the
//! channel was not used.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;

use super::WatchCtx;

/// Stable identity of one 看片 step. The string form is what `channels`,
/// `steps_json` and the UI use, so it must never be renamed silently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepId {
    #[default]
    Meta,
    Subtitle,
    Outline,
    Comments,
    Asr,
    Frames,
}

impl StepId {
    pub const ALL: [StepId; 6] = [
        StepId::Meta,
        StepId::Subtitle,
        StepId::Outline,
        StepId::Comments,
        StepId::Asr,
        StepId::Frames,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StepId::Meta => "meta",
            StepId::Subtitle => "subtitle",
            StepId::Outline => "outline",
            StepId::Comments => "comments",
            StepId::Asr => "asr",
            StepId::Frames => "frames",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StepId::Meta => "元数据",
            StepId::Subtitle => "官方字幕",
            StepId::Outline => "AI 提纲",
            StepId::Comments => "热门评论",
            StepId::Asr => "语音转写",
            StepId::Frames => "抽样画面",
        }
    }

    pub fn from_str(value: &str) -> Option<StepId> {
        StepId::ALL
            .into_iter()
            .find(|step| step.as_str() == value.trim())
    }
}

/// Why a step is not part of this run. The text is user-facing: the evidence
/// panel shows it verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkipReason(pub String);

impl SkipReason {
    pub fn new(text: impl Into<String>) -> Self {
        SkipReason(text.into())
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What one successful step produced, in accounting terms.
#[derive(Clone, Debug, Default)]
pub struct StepOutput {
    /// One line for the evidence detail (`字幕 87% · 2148 字`).
    pub detail: String,
    /// Network bytes the step pulled (page bytes, audio, video, JSON).
    pub bytes: u64,
    /// Model tokens the step spent (0 for every Bilibili-side step).
    pub tokens: u64,
    /// Optional note that belongs in `coverage_note` (e.g. "ASR 专名需回查").
    pub note: String,
    /// A step that ran but decided it could not produce material (missing
    /// engine/model, failed with a named reason). It is recorded as
    /// `skipped(reason)` rather than `failed`, exactly like a pre-flight skip.
    pub skip_reason: Option<String>,
}

/// The step contract. `run` is a boxed future because the engine is async and
/// the trait must stay object-safe (one `Vec<Box<dyn Step>>` per plan).
pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<StepOutput>> + Send + 'a>>;

pub trait Step: Send + Sync {
    fn id(&self) -> StepId;

    /// Per-step ceiling. A step that blows its budget becomes
    /// `skipped(<id>_timeout)`; the rest of the run continues.
    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }

    /// Pre-flight check: a cheap, deterministic reason the step cannot run at
    /// all (no session, engine not installed, model missing). Checked while the
    /// plan is built, before any network call.
    fn skip(&self, _config: &super::WatchConfig) -> Option<SkipReason> {
        None
    }

    fn run<'a>(&'a self, ctx: &'a mut WatchCtx) -> StepFuture<'a>;
}

/// The outcome of one step, stored per run and serialised into `steps_json`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    pub id: StepId,
    /// `ok` | `skipped` | `failed`.
    pub state: &'static str,
    /// Why it was skipped or failed; empty for `ok`.
    pub reason: String,
    pub ms: u64,
    pub bytes: u64,
    pub tokens: u64,
    pub detail: String,
}

impl StepReport {
    pub fn ok(id: StepId) -> Self {
        StepReport {
            id,
            state: "ok",
            ..StepReport::default()
        }
    }

    pub fn skipped(id: StepId, reason: impl Into<String>) -> Self {
        StepReport {
            id,
            state: "skipped",
            reason: reason.into(),
            ..StepReport::default()
        }
    }

    pub fn failed(id: StepId, reason: impl Into<String>) -> Self {
        StepReport {
            id,
            state: "failed",
            reason: reason.into(),
            ..StepReport::default()
        }
    }

    pub fn is_ok(&self) -> bool {
        self.state == "ok"
    }
}
