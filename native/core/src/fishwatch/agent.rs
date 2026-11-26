//! The 看片 run as a chat transcript.
//!
//! The engine already reports a step checklist (`FishProgressView`). This
//! module turns the *same* events into the richer transcript the 养鱼 Agent
//! panel paints: one card per gathered channel, a thinking card for the
//! DeepSeek call and a reply card with the verdict. It owns no storage and no
//! UI: `fish.rs` keeps one [`AgentView`] per current (or most recent) video and
//! serialises it inside the snapshot, so a polling page needs no second call.
//!
//! The shape follows the chat surface the owner already knows: every row has a
//! stable id, a kind (`tool` / `thinking`), a title, a small meta line, a state
//! (`running` / `ok` / `skipped` / `failed`), a one-line detail, and the cost
//! numbers the card footer prints.

use serde::Serialize;

use super::steps::{StepId, StepProgress};

/// Stable id of the DeepSeek judgement row. Tool rows use a [`StepId`] string.
pub const THINK_STEP_ID: &str = "deepseek";

/// One contact-sheet image produced by a tool. The bytes stay on disk; the
/// transcript only carries this descriptor and loads the image on demand.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentImageView {
    pub key: String,
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

/// One row of the agent transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentStepView {
    pub id: String,
    /// `tool`, `thinking` or `note`.
    pub kind: String,
    pub title: String,
    /// The small line beside the title (`完成`, the model name, …).
    pub meta: String,
    /// `running` | `ok` | `skipped` | `failed`.
    pub state: String,
    /// Preview and expanded body share this line; tool cards add the cost.
    pub detail: String,
    /// Tool-call arguments (pretty JSON): what the watch step asked Bilibili
    /// for, or the raw DeepSeek request body for the thinking row.
    pub arguments: String,
    /// Tool result (pretty JSON): the material the step actually produced, or
    /// the raw DeepSeek response for the thinking row. Empty while running.
    pub output: String,
    /// Function name (`list_videos`, `open_video`, `watch_clip`, `rate_video`).
    pub tool: String,
    /// Assistant text / reasoning for a thinking row (full, unlike `detail`).
    pub thinking: String,
    /// Contact sheets the tool produced.
    pub images: Vec<AgentImageView>,
    pub ms: u64,
    pub bytes: u64,
    pub tokens: u64,
}

/// The final answer card.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AgentReplyView {
    pub score: f64,
    pub verdict: String,
    pub one_line: String,
    pub reasons: Vec<String>,
    pub tags: Vec<String>,
    pub quote: String,
    /// DeepSeek request duration (not the whole watch run).
    pub ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// One watched video, as the chat panel shows it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AgentView {
    pub bvid: String,
    pub title: String,
    pub up_name: String,
    pub cover: String,
    pub duration_s: u64,
    /// The instruction the model was asked to follow.
    pub instruction: String,
    /// The exact `system` message sent to DeepSeek.
    pub system_prompt: String,
    /// The exact `user` message: video materials + the owner's instruction.
    pub user_prompt: String,
    pub model: String,
    /// `running` | `done` | `cancelled` | `error`.
    pub state: String,
    /// Unix seconds.
    pub started_at: i64,
    pub updated_at: i64,
    /// One line for the run header (`材料已齐 …`, a skip reason, …).
    pub note: String,
    pub steps: Vec<AgentStepView>,
    pub reply: Option<AgentReplyView>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Watch-plan totals accumulated from the step events.
    pub cost_ms: u64,
    pub cost_bytes: u64,
    pub cost_tokens: u64,
}

impl AgentView {
    /// Open an empty transcript for a continuous run. The video card fills in
    /// once the agent picks a candidate and calls `open_video`.
    pub fn begin(instruction: &str, model: &str) -> Self {
        let at = unix_now();
        AgentView {
            instruction: instruction.to_string(),
            model: model.to_string(),
            state: "running".to_string(),
            started_at: at,
            updated_at: at,
            note: "正在挑选候选视频…".to_string(),
            ..AgentView::default()
        }
    }

    /// Start a conversational transcript. Unlike [`AgentView::begin`], this
    /// does not assume a video is being watched: the thread stays idle until
    /// the owner sends a message.
    pub fn begin_chat(model: &str) -> Self {
        let at = unix_now();
        AgentView {
            model: model.to_string(),
            state: "idle".to_string(),
            note: "直接说你想看什么。".to_string(),
            started_at: at,
            updated_at: at,
            ..AgentView::default()
        }
    }

    /// Fill in the video card after `open_video` returns.
    pub fn set_video(
        &mut self,
        bvid: &str,
        title: &str,
        up_name: &str,
        cover: &str,
        duration_s: u64,
    ) {
        self.bvid = bvid.to_string();
        self.title = title.to_string();
        self.up_name = up_name.to_string();
        self.cover = cover.to_string();
        self.duration_s = duration_s;
        self.updated_at = unix_now();
    }

    /// Append one chronological event. Tool rows have a stable id supplied by
    /// the caller so a running row can be replaced when the tool returns.
    pub fn push_step(&mut self, step: AgentStepView) {
        if !step.detail.trim().is_empty() {
            self.note = step.detail.clone();
        }
        self.steps.push(step);
        self.updated_at = unix_now();
    }

    /// Replace an existing row by id, keeping its position in the thread.
    pub fn update_step(&mut self, step: AgentStepView) {
        if !step.detail.trim().is_empty() {
            self.note = step.detail.clone();
        }
        match self.steps.iter().position(|row| row.id == step.id) {
            Some(index) => self.steps[index] = step,
            None => self.steps.push(step),
        }
        self.updated_at = unix_now();
    }

    /// A unique, readable id for the next transcript row.
    pub fn next_step_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.steps.len() + 1)
    }

    /// Merge one watch-step event into the tool cards (legacy tool tester).
    pub fn apply_step(&mut self, event: &StepProgress) {
        let label = match event.state {
            "ok" => "完成",
            "skipped" => "跳过",
            "failed" => "失败",
            _ => "运行中",
        };
        let step = AgentStepView {
            id: event.step.as_str().to_string(),
            kind: "tool".to_string(),
            title: event.step.label().to_string(),
            meta: label.to_string(),
            state: event.state.to_string(),
            detail: event.detail.clone(),
            arguments: event.arguments.clone(),
            output: event.output.clone(),
            tool: event.step.as_str().to_string(),
            ms: event.ms,
            bytes: event.bytes,
            tokens: event.tokens,
            ..AgentStepView::default()
        };
        match self.steps.iter().position(|row| row.id == step.id) {
            Some(index) => self.steps[index] = step,
            None => self.steps.push(step),
        }
        self.sort_steps();
        if !event.detail.trim().is_empty() {
            self.note = event.detail.clone();
        }
        if event.state != "running" {
            self.cost_ms = event.cost_ms;
            self.cost_bytes = event.cost_bytes;
            self.cost_tokens = event.cost_tokens;
        }
        self.updated_at = unix_now();
    }

    /// The material gathering finished; the DeepSeek call is now in flight.
    /// The exact system/user messages and the serialized request body are kept
    /// on the transcript so the UI can open the whole prompt, not a preview.
    pub fn begin_thinking(&mut self, model: &str, system: &str, user: &str, request: &str) {
        let prompt_chars = system.chars().count() + user.chars().count();
        self.system_prompt = system.to_string();
        self.user_prompt = user.to_string();
        let detail = format!("材料已齐 · 提示词 {prompt_chars} 字符，等待 DeepSeek 判定…");
        self.model = model.to_string();
        let step = AgentStepView {
            id: THINK_STEP_ID.to_string(),
            kind: "thinking".to_string(),
            title: "思考".to_string(),
            meta: model.to_string(),
            state: "running".to_string(),
            detail: detail.clone(),
            arguments: request.to_string(),
            ..AgentStepView::default()
        };
        match self.steps.iter().position(|row| row.id == step.id) {
            Some(index) => self.steps[index] = step,
            None => self.steps.push(step),
        }
        self.note = detail;
        self.state = "running".to_string();
        self.updated_at = unix_now();
    }

    /// DeepSeek answered. The thinking row keeps the timing, usage numbers and
    /// the raw response body.
    pub fn finish_thinking(
        &mut self,
        ms: u64,
        tokens_in: u64,
        tokens_out: u64,
        answer_chars: usize,
        response: &str,
    ) {
        let detail =
            format!("收到 {answer_chars} 字符 · 输入 {tokens_in} tok · 输出 {tokens_out} tok");
        if let Some(step) = self.steps.iter_mut().find(|row| row.id == THINK_STEP_ID) {
            step.state = "ok".to_string();
            step.meta = "完成".to_string();
            step.detail = detail.clone();
            step.output = response.to_string();
            step.ms = ms;
            step.tokens = tokens_in.saturating_add(tokens_out);
        }
        self.tokens_in = self.tokens_in.saturating_add(tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(tokens_out);
        self.cost_tokens = self
            .cost_tokens
            .saturating_add(tokens_in.saturating_add(tokens_out));
        self.note = detail;
        self.updated_at = unix_now();
    }

    /// Attach the verdict as the final answer card.
    pub fn finish_reply(&mut self, reply: AgentReplyView) {
        self.note = format!(
            "完成 · {} · {:.1} 分 · {}",
            reply.verdict, reply.score, reply.one_line
        );
        self.reply = Some(reply);
        self.state = "done".to_string();
        self.updated_at = unix_now();
    }

    /// Close a run that ended without a verdict (cancel / error). Rows still
    /// marked running become skipped or failed with the same note, so the chat
    /// never leaves a spinner behind.
    pub fn finish(&mut self, state: &str, note: &str) {
        if self.state == "done" {
            // finish_reply already closed the thread; keep the richer note.
            self.note = note.to_string();
            self.updated_at = unix_now();
            return;
        }
        self.state = state.to_string();
        self.note = note.to_string();
        let (row_state, row_meta) = if state == "error" {
            ("failed", "失败")
        } else {
            ("skipped", "取消")
        };
        for step in &mut self.steps {
            if step.state == "running" {
                step.state = row_state.to_string();
                step.meta = row_meta.to_string();
                step.detail = note.to_string();
            }
        }
        self.updated_at = unix_now();
    }

    /// Update the one-line run note without touching the cards.
    pub fn set_note(&mut self, note: &str) {
        self.note = note.to_string();
        self.updated_at = unix_now();
    }

    fn sort_steps(&mut self) {
        self.steps.sort_by_key(|row| step_rank(&row.id));
    }
}

/// StepId order first, the DeepSeek row last (`usize::MAX`).
fn step_rank(id: &str) -> usize {
    StepId::ALL
        .iter()
        .position(|candidate| candidate.as_str() == id)
        .unwrap_or(usize::MAX)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(step: StepId, state: &'static str, detail: &str) -> StepProgress {
        StepProgress {
            step,
            index: 1,
            total: 3,
            state,
            detail: detail.to_string(),
            arguments: String::new(),
            output: String::new(),
            ms: 120,
            bytes: 2_048,
            tokens: 0,
            coverage: 0.4,
            cost_ms: 300,
            cost_bytes: 4_096,
            cost_tokens: 0,
        }
    }

    fn agent_for_test(model: &str) -> AgentView {
        let mut agent = AgentView::begin("指令", model);
        agent.set_video("BV1", "标题", "UP", "", 90);
        agent
    }

    #[test]
    fn steps_keep_the_watch_plan_order() {
        let mut agent = agent_for_test("deepseek-flash");
        agent.apply_step(&progress(StepId::Comments, "ok", "30 条"));
        agent.apply_step(&progress(StepId::Meta, "ok", "12 万播放"));
        agent.begin_thinking("deepseek-flash", "system", "user", "{\"model\":\"x\"}");

        let ids: Vec<&str> = agent.steps.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["meta", "comments", THINK_STEP_ID]);
        assert_eq!(agent.steps[0].title, "元数据");
        assert_eq!(agent.steps[1].meta, "完成");
        assert_eq!(agent.steps[2].kind, "thinking");
        assert_eq!(agent.steps[2].state, "running");
    }

    #[test]
    fn thinking_and_reply_carry_usage() {
        let mut agent = agent_for_test("deepseek-chat");
        agent.begin_thinking("deepseek-chat", "system", "user", "{\"model\":\"x\"}");
        agent.finish_thinking(1_500, 1_234, 56, 180, "{\"score\":8}");
        agent.finish_reply(AgentReplyView {
            score: 8.0,
            verdict: "看".to_string(),
            one_line: "值得看".to_string(),
            reasons: vec!["讲得清楚".to_string()],
            tags: vec!["Rust".to_string()],
            quote: "所有权".to_string(),
            ms: 1_500,
            tokens_in: 1_234,
            tokens_out: 56,
        });

        assert_eq!(agent.state, "done");
        assert_eq!(agent.tokens_in, 1_234);
        assert_eq!(agent.tokens_out, 56);
        assert_eq!(agent.cost_tokens, 1_290);
        let think = agent
            .steps
            .iter()
            .find(|row| row.id == THINK_STEP_ID)
            .unwrap();
        assert_eq!(think.state, "ok");
        assert_eq!(think.tokens, 1_290);
        assert!(think.detail.contains("收到 180 字符"));
        assert_eq!(agent.reply.as_ref().unwrap().score, 8.0);
    }

    #[test]
    fn thinking_keeps_the_exact_prompts_and_raw_bodies() {
        let mut agent = agent_for_test("deepseek-flash");
        agent.begin_thinking(
            "deepseek-flash",
            "系统提示",
            "用户材料",
            "{\"messages\":[]}",
        );
        assert_eq!(agent.system_prompt, "系统提示");
        assert_eq!(agent.user_prompt, "用户材料");
        let think = agent
            .steps
            .iter()
            .find(|row| row.id == THINK_STEP_ID)
            .unwrap();
        assert_eq!(think.arguments, "{\"messages\":[]}");
        assert!(think.output.is_empty());
        agent.finish_thinking(10, 1, 2, 3, "{\"score\":8}");
        let think = agent
            .steps
            .iter()
            .find(|row| row.id == THINK_STEP_ID)
            .unwrap();
        assert_eq!(think.output, "{\"score\":8}");
    }

    #[test]
    fn a_failed_run_closes_the_running_row() {
        let mut agent = agent_for_test("deepseek-chat");
        agent.apply_step(&progress(StepId::Meta, "ok", "打开"));
        agent.apply_step(&progress(StepId::Subtitle, "running", "拉取字幕"));
        agent.finish("error", "DeepSeek 返回 HTTP 401");

        assert_eq!(agent.state, "error");
        assert_eq!(agent.steps[1].state, "failed");
        assert_eq!(agent.steps[1].meta, "失败");
        assert!(agent.steps[1].detail.contains("401"));
        assert!(agent.reply.is_none());
    }

    #[test]
    fn a_done_run_keeps_its_reply_note() {
        let mut agent = agent_for_test("deepseek-chat");
        agent.finish_reply(AgentReplyView {
            score: 2.0,
            verdict: "划走".to_string(),
            one_line: "标题党".to_string(),
            ..AgentReplyView::default()
        });
        agent.finish("done", "完成 · 划走 · 2.0 分 · 覆盖 10%");
        assert_eq!(agent.state, "done");
        assert!(agent.note.contains("覆盖 10%"));
    }
}
