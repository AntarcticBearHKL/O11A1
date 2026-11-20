//! The agent's tool surface, described for the 养鱼 工具台.
//!
//! Every entry is one [`StepId`] and is built through the same `make_step`
//! factory the real watch plan uses, so the label, timeout and availability
//! shown in the UI cannot drift from the engine. The catalog is static: the
//! per-call result belongs to `fish::run_tool`.

use serde::Serialize;

use super::steps::StepId;
use super::{make_step, WatchConfig};

/// One tool the agent may call while watching a video.
#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub timeout_s: u64,
    /// `bilibili` | `local` | `deepseek-vision`.
    pub kind: &'static str,
    /// Long-running steps that deserve a warning in the test UI.
    pub heavy: bool,
    /// Needs a DeepSeek key (`frames` asks the vision model to read frames).
    pub needs_key: bool,
}

/// All six watch-plan channels, in execution order.
pub fn tool_catalog() -> Vec<ToolSpec> {
    let config = WatchConfig::default();
    StepId::ALL
        .into_iter()
        .map(|id| ToolSpec {
            id: id.as_str(),
            label: id.label(),
            description: description(id),
            timeout_s: make_step(id, &config).timeout().as_secs(),
            kind: kind(id),
            heavy: matches!(id, StepId::Asr | StepId::Frames),
            needs_key: matches!(id, StepId::Frames),
        })
        .collect()
}

fn kind(id: StepId) -> &'static str {
    match id {
        StepId::Meta | StepId::Subtitle | StepId::Outline | StepId::Comments => "bilibili",
        StepId::Asr => "local",
        StepId::Frames => "deepseek-vision",
    }
}

fn description(id: StepId) -> &'static str {
    match id {
        StepId::Meta => "读取标题、UP、cid、时长、分区和简介；其它工具都靠这里拿到的 cid。",
        StepId::Subtitle => "拉取 B 站官方 AI 字幕 / CC 字幕，返回文本、来源和覆盖率。",
        StepId::Outline => "调用 conclusion/get 生成 AI 提纲；只走 B 站接口，不消耗 DeepSeek tokens。",
        StepId::Comments => "取热门评论和弹幕峰值，作为观众反馈证据。",
        StepId::Asr => "下载音频并用本地 Whisper 转写；耗时最长，不消耗 DeepSeek tokens。",
        StepId::Frames => "按间隔抽取画面并调用 DeepSeek vision 观察；消耗 vision tokens。",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_watch_step() {
        let catalog = tool_catalog();
        let ids: Vec<&str> = catalog.iter().map(|tool| tool.id).collect();
        assert_eq!(
            ids,
            vec!["meta", "subtitle", "outline", "comments", "asr", "frames"]
        );
        assert!(catalog.iter().all(|tool| tool.timeout_s >= 20));
        assert!(catalog
            .iter()
            .all(|tool| !tool.description.trim().is_empty()));
        assert_eq!(catalog.iter().filter(|tool| tool.heavy).count(), 2);
        assert_eq!(catalog.iter().filter(|tool| tool.needs_key).count(), 1);
    }
}
