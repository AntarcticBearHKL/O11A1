//! The read-only evidence bundle handed to the UI and stored in `verdicts`.
//!
//! This is the ONLY thing the UI needs to render "字幕 87% · 评论 20 条 · 未跑
//! ASR · 4.2s · 3.1k tok" and its per-step detail. It is `Serialize` so the
//! database keeps the same shape the panel reads.

use serde::{Deserialize, Serialize};

use crate::fishwatch::cost::{human_ms, human_tokens};
use crate::fishwatch::steps::StepReport;
#[cfg(test)]
use crate::fishwatch::steps::StepId;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    /// `meta` / `subtitle` / `outline` / `comments` / `asr` / `frames` /
    /// `verdict` (the last one is appended by `fish.rs`, not a watch step).
    pub id: String,
    pub label: String,
    /// `ok` | `skipped` | `failed`.
    pub state: String,
    pub reason: String,
    pub ms: u64,
    pub bytes: u64,
    pub tokens: u64,
    pub detail: String,
}

impl StepRecord {
    pub fn from_report(report: &StepReport) -> Self {
        StepRecord {
            id: report.id.as_str().to_string(),
            label: report.id.label().to_string(),
            state: report.state.to_string(),
            reason: report.reason.clone(),
            ms: report.ms,
            bytes: report.bytes,
            tokens: report.tokens,
            detail: report.detail.clone(),
        }
    }

    /// `ok · 0.8s · 字幕 87%` / `skipped · 引擎未安装`.
    pub fn line(&self) -> String {
        if self.state == "ok" {
            let mut line = format!("{} · ok · {:.1}s", self.label, self.ms as f64 / 1_000.0);
            if !self.detail.is_empty() {
                line.push_str(" · ");
                line.push_str(&self.detail);
            }
            line
        } else {
            format!("{} · {} · {}", self.label, self.state, self.reason)
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    /// Comma-separated ids of the channels that actually produced material.
    pub channels: String,
    pub coverage: f64,
    pub coverage_note: String,
    pub cost_ms: u64,
    pub cost_bytes: u64,
    pub cost_tokens: u64,
    pub steps: Vec<StepRecord>,
}

impl Evidence {
    #[cfg(test)]
    pub fn channel_list(&self) -> Vec<String> {
        self.channels
            .split(',')
            .map(str::trim)
            .filter(|channel| !channel.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[cfg(test)]
    pub fn has_channel(&self, id: StepId) -> bool {
        self.channel_list().iter().any(|channel| channel == id.as_str())
    }

    fn step(&self, id: &str) -> Option<&StepRecord> {
        self.steps.iter().find(|step| step.id == id)
    }

    pub fn coverage_percent(&self) -> u32 {
        (self.coverage * 100.0).round().clamp(0.0, 100.0) as u32
    }

    /// The one-line evidence row under a verdict card.
    pub fn line(&self) -> String {
        if self.steps.is_empty() {
            // A verdict written before phase 1 has no evidence at all; the card
            // says "未记录" instead of inventing "未跑 ASR".
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        let asr_ok = self.step("asr").map_or(false, |step| step.state == "ok");
        let subtitle_ok = self
            .step("subtitle")
            .map_or(false, |step| step.state == "ok");
        if self.coverage > 0.0 {
            if asr_ok && !subtitle_ok {
                parts.push(format!("ASR {}%", self.coverage_percent()));
            } else {
                parts.push(format!("字幕 {}%", self.coverage_percent()));
            }
        } else if subtitle_ok || asr_ok {
            parts.push("口播 0%".to_string());
        } else {
            parts.push("无口播".to_string());
        }
        if let Some(comments) = self.step("comments") {
            if comments.state == "ok" {
                parts.push(format!("评论 {} 条", leading_number(&comments.detail)));
            }
        }
        if !asr_ok {
            parts.push("未跑 ASR".to_string());
        }
        if let Some(frames) = self.step("frames") {
            if frames.state == "ok" {
                parts.push(format!("画面 {} 帧", leading_number(&frames.detail)));
            } else {
                parts.push("画面未取到".to_string());
            }
        }
        parts.push(human_ms(self.cost_ms));
        parts.push(format!("{} tok", human_tokens(self.cost_tokens)));
        parts.join(" · ")
    }

    /// The expanded panel: one line per step, then the blind-spot note.
    pub fn detail(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            out.push_str(&step.line());
            out.push('\n');
        }
        if !self.coverage_note.trim().is_empty() {
            out.push_str("盲区：");
            out.push_str(&self.coverage_note);
        }
        out
    }

    /// Record the verdict call itself. `fish.rs` calls this after DeepSeek
    /// answered, so the evidence line's time and token totals include the model
    /// that actually made the decision.
    pub fn append_verdict(&mut self, ms: u64, tokens_in: u64, tokens_out: u64) {
        self.cost_ms = self.cost_ms.saturating_add(ms);
        self.cost_tokens = self
            .cost_tokens
            .saturating_add(tokens_in)
            .saturating_add(tokens_out);
        self.steps.push(StepRecord {
            id: "verdict".to_string(),
            label: "判定".to_string(),
            state: "ok".to_string(),
            reason: String::new(),
            ms,
            bytes: 0,
            tokens: tokens_in.saturating_add(tokens_out),
            detail: format!("{} tok", tokens_in.saturating_add(tokens_out)),
        });
    }

    /// `subtitle,comments` for the sqlite `channels` column.
    pub fn channels_text(&self) -> String {
        self.channels.clone()
    }

    pub fn steps_json(&self) -> String {
        serde_json::to_string(&self.steps).unwrap_or_else(|_| "[]".to_string())
    }

    /// Rebuild from the database row. The step array is the source of truth;
    /// the flat columns are what the feed sorts/filters on without parsing it.
    pub fn from_columns(
        channels: &str,
        coverage: f64,
        coverage_note: &str,
        cost_ms: u64,
        cost_bytes: u64,
        cost_tokens: u64,
        steps_json: &str,
    ) -> Self {
        Evidence {
            channels: channels.trim().to_string(),
            coverage,
            coverage_note: coverage_note.to_string(),
            cost_ms,
            cost_bytes,
            cost_tokens,
            steps: serde_json::from_str::<Vec<StepRecord>>(steps_json).unwrap_or_default(),
        }
    }
}

/// `20 条 · 弹幕峰值 3` → `20`; a detail without a number → `0`.
fn leading_number(detail: &str) -> u64 {
    detail
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, label: &str, state: &str, detail: &str) -> StepRecord {
        StepRecord {
            id: id.to_string(),
            label: label.to_string(),
            state: state.to_string(),
            reason: String::new(),
            ms: 10,
            bytes: 0,
            tokens: 0,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn the_evidence_line_matches_the_design_example() {
        let evidence = Evidence {
            channels: "meta,subtitle,comments".to_string(),
            coverage: 0.87,
            coverage_note: "官方字幕覆盖 87% 的时长".to_string(),
            cost_ms: 4_200,
            cost_bytes: 12_000,
            cost_tokens: 3_100,
            steps: vec![
                step("meta", "元数据", "ok", "cid=1"),
                step("subtitle", "官方字幕", "ok", "120 句 / 2148 字"),
                step("comments", "热门评论", "ok", "20 条 · 弹幕峰值 3"),
                StepRecord {
                    id: "asr".to_string(),
                    label: "语音转写".to_string(),
                    state: "skipped".to_string(),
                    reason: "ASR 未开启".to_string(),
                    ..StepRecord::default()
                },
            ],
        };
        assert_eq!(
            evidence.line(),
            "字幕 87% · 评论 20 条 · 未跑 ASR · 4.2s · 3.1k tok"
        );
        assert!(evidence.detail().contains("语音转写 · skipped · ASR 未开启"));
    }

    #[test]
    fn asr_and_frames_change_the_line_only_when_they_ran() {
        let mut evidence = Evidence {
            channels: "meta,asr,frames,comments".to_string(),
            coverage: 0.93,
            cost_ms: 65_000,
            cost_tokens: 15_400,
            steps: vec![
                step("asr", "语音转写", "ok", "640 句 / 12k 字"),
                step("comments", "热门评论", "ok", "8 条 · 弹幕峰值 2"),
                step("frames", "抽样画面", "ok", "40 张 · 5 批"),
            ],
            ..Evidence::default()
        };
        assert_eq!(evidence.line(), "ASR 93% · 评论 8 条 · 画面 40 帧 · 65.0s · 15.4k tok");
        evidence.steps.retain(|step| step.id != "frames");
        assert!(!evidence.line().contains("画面"));
    }

    #[test]
    fn columns_round_trip_through_json() {
        let evidence = Evidence {
            channels: "meta,subtitle".to_string(),
            coverage: 0.5,
            coverage_note: "半部".to_string(),
            cost_ms: 123,
            cost_bytes: 456,
            cost_tokens: 789,
            steps: vec![step("subtitle", "官方字幕", "ok", "10 句")],
        };
        let restored = Evidence::from_columns(
            &evidence.channels,
            evidence.coverage,
            &evidence.coverage_note,
            evidence.cost_ms,
            evidence.cost_bytes,
            evidence.cost_tokens,
            &evidence.steps_json(),
        );
        assert_eq!(restored, evidence);
        assert_eq!(restored.channel_list(), vec!["meta", "subtitle"]);
        assert!(restored.has_channel(StepId::Subtitle));
    }
}

