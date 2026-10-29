//! Per-run accounting: milliseconds, network bytes and model tokens.
//!
//! Every number here is measured, never estimated: `ms` comes from `Instant`,
//! `bytes` from the bytes a step actually pulled, and `tokens` from DeepSeek's
//! own `usage` block. A step that reports 0 really spent 0 of that unit.


use crate::fishwatch::steps::StepReport;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CostBook {
    pub ms: u64,
    pub bytes: u64,
    pub tokens: u64,
}

impl CostBook {
    /// Add one finished step (ok, skipped or failed; skipped steps still spend
    /// the milliseconds it took to discover the failure).
    pub fn add_step(&mut self, report: &StepReport) {
        self.ms = self.ms.saturating_add(report.ms);
        self.bytes = self.bytes.saturating_add(report.bytes);
        self.tokens = self.tokens.saturating_add(report.tokens);
    }

}

/// `4.2s` / `820ms`.
pub fn human_ms(ms: u64) -> String {
    if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

/// `3.1k` / `842`.
pub fn human_tokens(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}
