//! 养鱼：一条住在鱼缸里的鲸鱼（DeepSeek）替鱼主人先看视频。
//!
//! The engine walks the home feed, gathers cheap **text** signals per video
//! (metadata → Bilibili's own AI summary/outline/transcript → top comments +
//! danmaku peaks), asks DeepSeek for a verdict, and stores everything in an
//! embedded SQLite database (`fish.db`, next to `settings.json`).
//!
//! Design rules that the rest of this module relies on:
//!
//! * **No ASR, no media download.** Only text ever leaves Bilibili: the feed
//!   item, `conclusion/get` (which includes Bilibili's own transcript), and —
//!   when there is no transcript — the top comments and per-minute danmaku
//!   counts. A video without any of those is judged on metadata alone and the
//!   prompt says so.
//! * **Never invent numbers.** Token counts come from DeepSeek's `usage`
//!   block; there is no cost estimate because there is no price table here.
//! * **One worker thread, one request in flight.** The worker `block_on`s the
//!   process-wide Tokio runtime, so Bilibili sees at most one conclusion call at
//!   a time with at least [`CONCLUSION_MIN_GAP_MS`] between starts.
//!
//! The UI layer consumes [`FishEngine::snapshot`], [`FishEngine::stats`] and
//! [`FishEngine::verdicts`]; it never touches SQLite itself.

// The engine's surface is consumed by the UI layer, which is wired in a later
// pass (`main.rs` and `ui/app.slint` belong to another change). Until then the
// whole module would be one long dead-code warning; remove this once
// `snapshot`/`stats`/`verdicts` are called from the app.
#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use md5::{Digest, Md5};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::bilibili::{Conclusion, FishMeta, Session};
use crate::settings::{self, Settings};

/// One feed card, as handed over by `bilibili::homepage_feed_from`.
pub type FeedItem = crate::bilibili::FeedVideo;

/// `fish.db` lives beside `settings.json`, i.e. in the project root.
const DB_FILE: &str = "fish.db";

// ---------------------------------------------------------------------------
// 食谱 (prompt presets)
// ---------------------------------------------------------------------------

/// One selectable "recipe": the instruction the owner can swap in one click
/// instead of writing a prompt from scratch.
#[derive(Clone, Copy, Debug)]
pub struct FishPreset {
    /// Stable id, so the picker can mark the active card after a restart.
    pub id: &'static str,
    /// Card title (2–4 characters read best).
    pub name: &'static str,
    /// One line under the title, explaining the taste.
    pub tagline: &'static str,
    /// What is written into `settings.fish_instruction`.
    pub body: &'static str,
}

/// The built-in recipes, in the order the picker shows them. The first one is
/// the default the app ships with.
pub const PRESETS: &[FishPreset] = &[
    FishPreset {
        id: "strict",
        name: "严格影评人",
        tagline: "默认。注水、复述、广告直接扣分",
        body: "按影评人的标准判断：值不值得花时间。注水、复述剧情、广告直接指出并扣分。",
    },
    FishPreset {
        id: "bait",
        name: "标题党克星",
        tagline: "标题与内容不符就吐刺",
        body: "只判断标题与内容是否一致。不一致就判不看，并引用最能证明不一致的那句话。",
    },
    FishPreset {
        id: "digest",
        name: "干货提取器",
        tagline: "只留可执行的步骤和参数",
        body: "只找可执行的东西：步骤、参数、结论、链接。找不到就判不看。",
    },
    FishPreset {
        id: "learner",
        name: "学习向导",
        tagline: "适不适合入门、缺什么前置",
        body: "判断适不适合入门：需要什么前置知识、能学到什么、哪一段讲得不清楚。",
    },
    FishPreset {
        id: "timebox",
        name: "时间管家",
        tagline: "给时间戳，只建议看哪几段",
        body: "估算最少需要看哪几段，给出时间戳；其余段落建议跳过。",
    },
    FishPreset {
        id: "debate",
        name: "争议雷达",
        tagline: "评论区在争什么，谁说得对",
        body: "评论区在争什么？两派各自最强的一句话是什么？视频有没有回避争议。",
    },
    FishPreset {
        id: "tech",
        name: "技术审稿",
        tagline: "数据自洽吗，结论站得住吗",
        body: "检查技术细节：数据是否自洽、结论是否被材料支持、有没有明显错误。",
    },
    FishPreset {
        id: "vibe",
        name: "下饭指数",
        tagline: "好不好看，适不适合下饭",
        body: "纯娱乐向：好不好看、笑点或爽点在哪、适不适合吃饭时看。",
    },
    FishPreset {
        id: "cold",
        name: "冷面吐槽",
        tagline: "毒舌模式，但句句有据",
        body: "毒舌一点，直接说难听的真话；但每一条都必须有材料里的依据。",
    },
];

/// Every built-in recipe.
pub fn presets() -> &'static [FishPreset] {
    PRESETS
}

/// The recipe whose body is exactly `instruction`, if any. The picker uses it
/// to mark the active card; an edited instruction simply highlights nothing.
pub fn active_preset(instruction: &str) -> Option<&'static FishPreset> {
    let wanted = instruction.trim();
    PRESETS.iter().find(|preset| preset.body == wanted)
}

/// OpenAI-compatible chat completions endpoint.
const DEEPSEEK_URL: &str = "https://api.deepseek.com/chat/completions";
/// Bump when [`SCHEMA`] changes; stored in `kv('schema_version')`.
const SCHEMA_VERSION: &str = "1";
/// Front of a transcript that is always kept, in characters.
const PROMPT_HEAD_CHARS: usize = 12_000;
/// Tail of a long transcript that is kept so the ending is never lost.
const PROMPT_TAIL_CHARS: usize = 800;
/// Description characters kept in the prompt (they arrive as one long line).
const PROMPT_DESC_CHARS: usize = 400;
/// One comment, after whitespace collapse, never exceeds this many characters.
const PROMPT_COMMENT_CHARS: usize = 240;
/// Total character budget for the comment section of the prompt.
const PROMPT_COMMENTS_BUDGET: usize = 2_000;
/// A video judged less than this long ago is not queued again.
const DEDUPE_WINDOW_S: i64 = 7 * 24 * 3600;
/// Hard ceiling on gathering one video's material (all Bilibili calls included).
const JOB_TIMEOUT_S: u64 = 75;
/// Minimum pause between two `conclusion/get` calls (Bilibili risk control).
const CONCLUSION_MIN_GAP_MS: u64 = 1_500;
/// Extra attempts after a `-352` / `-412` answer.
const CONCLUSION_MAX_RETRIES: u32 = 3;
/// Top-level comments kept as the "what is in it" fallback.
const COMMENT_LIMIT: usize = 20;
/// Busiest danmaku minutes kept in the fallback signal.
const DANMAKU_TOP_MINUTES: usize = 5;
/// Danmaku is only scanned for the first hour (10 segments), to bound traffic.
const DANMAKU_MAX_SECONDS: u64 = 3_600;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS kv (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS videos (
    bvid       TEXT PRIMARY KEY,
    cid        INTEGER NOT NULL DEFAULT 0,
    title      TEXT NOT NULL DEFAULT '',
    up_mid     INTEGER NOT NULL DEFAULT 0,
    up_name    TEXT NOT NULL DEFAULT '',
    duration_s INTEGER NOT NULL DEFAULT 0,
    view_count INTEGER NOT NULL DEFAULT 0,
    pub_ts     INTEGER NOT NULL DEFAULT 0,
    cover      TEXT NOT NULL DEFAULT '',
    tname      TEXT NOT NULL DEFAULT '',
    first_seen INTEGER NOT NULL DEFAULT 0,
    last_seen  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS signals (
    bvid              TEXT PRIMARY KEY REFERENCES videos(bvid) ON DELETE CASCADE,
    summary           TEXT NOT NULL DEFAULT '',
    outline_json      TEXT NOT NULL DEFAULT '[]',
    transcript        TEXT NOT NULL DEFAULT '',
    transcript_source TEXT NOT NULL DEFAULT 'none',
    transcript_chars  INTEGER NOT NULL DEFAULT 0,
    comments_json     TEXT NOT NULL DEFAULT '[]',
    danmaku_json      TEXT NOT NULL DEFAULT '[]',
    conclusion_code   INTEGER NOT NULL DEFAULT 0,
    fetched_at        INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS verdicts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    bvid         TEXT NOT NULL REFERENCES videos(bvid) ON DELETE CASCADE,
    at           INTEGER NOT NULL,
    model        TEXT NOT NULL DEFAULT '',
    prompt_hash  TEXT NOT NULL DEFAULT '',
    score        REAL NOT NULL DEFAULT 0,
    verdict      TEXT NOT NULL DEFAULT '也许',
    one_line     TEXT NOT NULL DEFAULT '',
    reasons_json TEXT NOT NULL DEFAULT '[]',
    tags_json    TEXT NOT NULL DEFAULT '[]',
    quote        TEXT NOT NULL DEFAULT '',
    tokens_in    INTEGER NOT NULL DEFAULT 0,
    tokens_out   INTEGER NOT NULL DEFAULT 0,
    elapsed_ms   INTEGER NOT NULL DEFAULT 0,
    error        TEXT
);
CREATE TABLE IF NOT EXISTS queue (
    bvid       TEXT PRIMARY KEY,
    state      TEXT NOT NULL DEFAULT 'pending',
    priority   INTEGER NOT NULL DEFAULT 0,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    updated_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_verdicts_at ON verdicts(at DESC);
CREATE INDEX IF NOT EXISTS idx_queue_state ON queue(state, priority DESC);
INSERT INTO kv (k, v) VALUES ('schema_version', '1') ON CONFLICT(k) DO NOTHING;
"#;

/// 养鱼出巡的流水线。
const SYSTEM_PROMPT: &str = "你是一条住在鱼缸里的鲸鱼，替鱼主人先看视频。你的职责是替他做决定：\
这个视频到底要不要花时间看。只依据材料判断，不编造数字、不编造情节。\
默认必须给出“看”或“不看”；只有材料确实不足以判断时才用“也许”，并在 reasons 里写清楚缺什么。\
严格输出 JSON 对象，不要输出 JSON 以外的任何文字。";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// What the engine is doing right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FishState {
    /// Nothing queued (or the last run finished its budget).
    Idle,
    /// `deepseek_key` is empty: no network at all until the user adds one.
    NeedsKey,
    Running,
    Paused,
    /// The engine stopped on a fatal problem (e.g. `fish.db` became unwritable).
    Error(String),
}

impl Default for FishState {
    fn default() -> Self {
        FishState::Idle
    }
}

/// One poll of the engine: what the fish panel paints.
#[derive(Clone, Debug, Default)]
pub struct FishSnapshot {
    pub state: FishState,
    /// `pending` + `running` rows.
    pub queued: usize,
    /// Videos with a successful verdict (all time).
    pub done: u32,
    /// Queue rows currently in `error` state.
    pub errors: u32,
    /// `(bvid, title)` of the video being judged.
    pub current: Option<(String, String)>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The newest successful verdict.
    pub last: Option<VerdictView>,
}

/// One stored verdict, flattened with the video it belongs to.
#[derive(Clone, Debug, Default)]
pub struct VerdictView {
    pub bvid: String,
    pub title: String,
    pub up_name: String,
    pub cover: String,
    pub score: f64,
    pub verdict: String,
    pub one_line: String,
    pub reasons: Vec<String>,
    pub tags: Vec<String>,
    pub quote: String,
    /// Unix seconds.
    pub at: i64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Aggregate view of the last `days` days (`0` = everything).
#[derive(Clone, Debug)]
pub struct FishStats {
    pub watched: u32,
    pub recommend: u32,
    pub skip: u32,
    pub maybe: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub top_tags: Vec<(String, u32)>,
    /// Bucket `i` counts verdicts whose score rounds to `i` (`0..=10`).
    pub score_histogram: [u32; 11],
}

impl Default for FishStats {
    fn default() -> Self {
        FishStats {
            watched: 0,
            recommend: 0,
            skip: 0,
            maybe: 0,
            tokens_in: 0,
            tokens_out: 0,
            top_tags: Vec::new(),
            score_histogram: [0; 11],
        }
    }
}

/// Callback invoked after every state change, for a UI that does not poll.
pub type SnapshotHook = Arc<dyn Fn(FishSnapshot) + Send + Sync>;

// ---------------------------------------------------------------------------
// Engine internals
// ---------------------------------------------------------------------------

/// How the worker gets its signals. `Stub` is the offline mode used by the
/// mock-server test; production always uses `Live`.
#[derive(Clone)]
enum Signals {
    Live(Arc<Session>),
    #[cfg(test)]
    Stub,
}

/// Everything the worker reads from `settings.json`, snapshotted per run so a
/// mid-run settings edit never half-applies.
#[derive(Clone, Debug)]
struct EngineConfig {
    key: String,
    model: String,
    instruction: String,
    budget_videos: u32,
    interval_ms: u64,
    endpoint: String,
}

impl EngineConfig {
    fn from_settings(settings: &Settings) -> Self {
        EngineConfig {
            key: settings.deepseek_key.trim().to_string(),
            model: settings::normalise_fish_model(&settings.fish_model),
            instruction: settings::normalise_fish_instruction(&settings.fish_instruction),
            budget_videos: settings::normalise_fish_budget(settings.fish_budget_videos),
            interval_ms: settings::normalise_fish_interval(settings.fish_interval_ms),
            endpoint: DEEPSEEK_URL.to_string(),
        }
    }
}

/// One queue row the worker picked up.
#[derive(Clone, Debug)]
struct Job {
    bvid: String,
    title: String,
    attempts: i64,
}

/// The text material of one video, gathered once and then used for the prompt,
/// the `signals` row and the verdict.
#[derive(Clone, Debug, Default)]
struct Materials {
    bvid: String,
    cid: u64,
    aid: i64,
    title: String,
    up_mid: u64,
    up_name: String,
    duration_s: u64,
    view: u64,
    pub_ts: i64,
    cover: String,
    tname: String,
    desc: String,
    summary: String,
    outline: Vec<crate::bilibili::OutlineSection>,
    transcript: String,
    /// `official` (Bilibili's AI transcript) or `none`.
    transcript_source: String,
    comments: Vec<StoredComment>,
    danmaku: Vec<Peak>,
    conclusion_code: i32,
}

/// One top comment kept as material.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredComment {
    like: u64,
    text: String,
}

/// One busy danmaku minute.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Peak {
    minute: u32,
    count: u32,
    sample: String,
}

/// A parsed model answer (before it is written to the database).
#[derive(Clone, Debug, Default)]
struct ParsedVerdict {
    score: f64,
    verdict: String,
    one_line: String,
    reasons: Vec<String>,
    tags: Vec<String>,
    quote: String,
}

/// Raw DeepSeek answer plus the usage numbers it reported.
struct Asked {
    content: String,
    tokens_in: u64,
    tokens_out: u64,
}

/// The 养鱼 engine. Build it once via [`FishEngine::open`] and share the `Arc`.
pub struct FishEngine {
    db: Mutex<Connection>,
    path: PathBuf,
    config: Mutex<EngineConfig>,
    state: Mutex<FishState>,
    current: Mutex<Option<(String, String)>>,
    signals: Mutex<Option<Signals>>,
    hook: Mutex<Option<SnapshotHook>>,
    last_conclusion: Mutex<Option<Instant>>,
    paused: AtomicBool,
    /// True while a worker thread exists (guards `start` against a second one).
    worker: AtomicBool,
    /// Whether [`FishEngine::set_instruction`] writes `settings.json`. Tests
    /// must never touch the user's file.
    persist: AtomicBool,
    self_ref: Mutex<Weak<FishEngine>>,
    http: reqwest::Client,
}

// ---------------------------------------------------------------------------
// Opening / schema
// ---------------------------------------------------------------------------

impl FishEngine {
    /// Open (or create) `fish.db` beside `settings.json`.
    pub fn open() -> Result<Arc<Self>> {
        Self::open_at(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DB_FILE))
    }

    /// Open a database at an explicit path (tests, and a future custom location).
    pub fn open_at(path: &Path) -> Result<Arc<Self>> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("创建养鱼数据目录失败：{}", dir.display()))?;
            }
        }
        let db = open_db(path)?;
        // A row left in `running` belongs to a previous process that died (crash,
        // kill, power loss). Nothing will ever finish it, and `pop_next` skips
        // it, so the queue would silently lose that video forever. Put it back.
        let requeued = db
            .execute(
                "UPDATE queue SET state = 'pending' WHERE state = 'running'",
                [],
            )
            .unwrap_or(0);
        if requeued > 0 {
            crate::debug_log(&format!("fish requeued running rows={requeued}"));
        }
        let settings = settings::load();
        let state = if settings.deepseek_key.trim().is_empty() {
            FishState::NeedsKey
        } else {
            FishState::Idle
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .context("创建 DeepSeek HTTP 客户端失败")?;
        let engine = Arc::new(FishEngine {
            db: Mutex::new(db),
            path: path.to_path_buf(),
            config: Mutex::new(EngineConfig::from_settings(&settings)),
            state: Mutex::new(state),
            current: Mutex::new(None),
            signals: Mutex::new(None),
            hook: Mutex::new(None),
            last_conclusion: Mutex::new(None),
            paused: AtomicBool::new(false),
            worker: AtomicBool::new(false),
            persist: AtomicBool::new(true),
            self_ref: Mutex::new(Weak::new()),
            http,
        });
        if let Ok(mut slot) = engine.self_ref.lock() {
            *slot = Arc::downgrade(&engine);
        }
        Ok(engine)
    }

    /// Where this engine stores its data.
    pub fn db_path(&self) -> &Path {
        &self.path
    }

    /// Optional push-style update for the UI (called after every state change).
    pub fn set_snapshot_hook(&self, hook: Option<SnapshotHook>) {
        if let Ok(mut slot) = self.hook.lock() {
            *slot = hook;
        }
    }

    /// The instruction that will be hashed into the next prompt.
    pub fn instruction(&self) -> String {
        self.config()
            .map(|config| config.instruction)
            .unwrap_or_else(|_| settings::default_fish_instruction())
    }

    // -- reading -----------------------------------------------------------

    /// Everything the fish panel needs, in one call.
    pub fn snapshot(&self) -> FishSnapshot {
        let state = self
            .state
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(FishState::Idle);
        let current = self.current.lock().ok().and_then(|guard| guard.clone());
        let mut snapshot = FishSnapshot {
            state,
            current,
            ..FishSnapshot::default()
        };
        let Ok(db) = self.db.lock() else {
            return snapshot;
        };
        snapshot.queued = count(&db, "SELECT COUNT(*) FROM queue WHERE state IN ('pending','running')")
            .unwrap_or(0) as usize;
        snapshot.done =
            count(&db, "SELECT COUNT(*) FROM verdicts WHERE error IS NULL").unwrap_or(0) as u32;
        snapshot.errors =
            count(&db, "SELECT COUNT(*) FROM queue WHERE state = 'error'").unwrap_or(0) as u32;
        if let Ok((tokens_in, tokens_out)) = db.query_row(
            "SELECT COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0) FROM verdicts",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ) {
            snapshot.tokens_in = tokens_in.max(0) as u64;
            snapshot.tokens_out = tokens_out.max(0) as u64;
        }
        snapshot.last = last_verdict(&db).ok().flatten();
        snapshot
    }

    /// Newest verdicts first (failed attempts are not shown).
    pub fn verdicts(&self, limit: usize) -> Result<Vec<VerdictView>> {
        let db = self.lock_db()?;
        let mut statement = db.prepare(VERDICT_SELECT)?;
        let rows = statement.query_map(params![limit as i64], verdict_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Aggregates for the last `days` days; `0` means "since the beginning".
    pub fn stats(&self, days: u32) -> Result<FishStats> {
        let since = if days == 0 {
            0
        } else {
            now() - days as i64 * 86_400
        };
        let db = self.lock_db()?;
        let mut statement = db.prepare(
            "SELECT score, verdict, tags_json, tokens_in, tokens_out FROM verdicts \
             WHERE error IS NULL AND at >= ?1",
        )?;
        let mut rows = statement.query(params![since])?;
        let mut stats = FishStats::default();
        let mut tags: HashMap<String, u32> = HashMap::new();
        while let Some(row) = rows.next()? {
            let score: f64 = row.get(0)?;
            let verdict: String = row.get(1)?;
            let tags_json: String = row.get(2)?;
            let tokens_in: i64 = row.get(3)?;
            let tokens_out: i64 = row.get(4)?;
            stats.watched += 1;
            match verdict.trim() {
                "看" => stats.recommend += 1,
                "不看" => stats.skip += 1,
                _ => stats.maybe += 1,
            }
            stats.tokens_in += tokens_in.max(0) as u64;
            stats.tokens_out += tokens_out.max(0) as u64;
            stats.score_histogram[score_bucket(score)] += 1;
            for tag in parse_string_list(&tags_json) {
                *tags.entry(tag).or_insert(0) += 1;
            }
        }
        let mut top: Vec<(String, u32)> = tags.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top.truncate(8);
        stats.top_tags = top;
        Ok(stats)
    }

    // -- writing -----------------------------------------------------------

    /// Queue feed cards for the next run.
    ///
    /// Dedupe rules, in order: an empty `bvid` is dropped; a video judged
    /// successfully within [`DEDUPE_WINDOW_S`] is skipped entirely; a row that
    /// is already `pending`/`running` keeps its state (only its priority may
    /// rise), so a second `enqueue` never duplicates work in flight.
    pub fn enqueue(&self, items: &[FeedItem]) {
        if items.is_empty() {
            return;
        }
        if let Err(err) = self.enqueue_inner(items) {
            crate::debug_log(&format!("fish enqueue failed: {err:#}"));
        }
        self.publish();
    }

    fn enqueue_inner(&self, items: &[FeedItem]) -> Result<()> {
        let at = now();
        let mut db = self.lock_db()?;
        let tx = db.transaction()?;
        let total = items.len() as i64;
        for (index, item) in items.iter().enumerate() {
            let bvid = item.bvid.trim();
            if bvid.is_empty() {
                continue;
            }
            let recent: Option<i64> = tx
                .query_row(
                    "SELECT at FROM verdicts WHERE bvid = ?1 AND error IS NULL \
                     ORDER BY at DESC LIMIT 1",
                    params![bvid],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(at) = recent {
                if at >= now() - DEDUPE_WINDOW_S {
                    continue;
                }
            }
            tx.execute(
                "INSERT INTO videos (bvid, cid, title, up_mid, up_name, duration_s, view_count, \
                 pub_ts, cover, tname, first_seen, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '', ?10, ?10) \
                 ON CONFLICT(bvid) DO UPDATE SET cid = excluded.cid, title = excluded.title, \
                 up_mid = excluded.up_mid, up_name = excluded.up_name, \
                 duration_s = excluded.duration_s, view_count = excluded.view_count, \
                 pub_ts = excluded.pub_ts, cover = excluded.cover, last_seen = excluded.last_seen",
                params![
                    bvid,
                    item.cid as i64,
                    item.title,
                    item.owner_mid,
                    item.owner,
                    item.duration as i64,
                    item.view as i64,
                    item.pub_ts,
                    item.pic,
                    at
                ],
            )?;
            // Earlier feed cards get higher priority: the fish walks the feed in
            // the order the user would have scrolled it.
            let priority = total - index as i64;
            tx.execute(
                "INSERT INTO queue (bvid, state, priority, attempts, last_error, updated_at) \
                 VALUES (?1, 'pending', ?2, 0, NULL, ?3) \
                 ON CONFLICT(bvid) DO UPDATE SET \
                 priority = MAX(queue.priority, excluded.priority), \
                 state = CASE queue.state WHEN 'running' THEN 'running' ELSE 'pending' END, \
                 updated_at = excluded.updated_at",
                params![bvid, priority, at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace the 鱼主人的指令 and persist it (settings, when this engine owns
    /// the user's file).
    pub fn set_instruction(&self, text: &str) {
        let value = settings::normalise_fish_instruction(text);
        if let Ok(mut config) = self.config.lock() {
            config.instruction = value.clone();
        }
        if self.persist.load(Ordering::SeqCst) {
            let mut stored = settings::load();
            stored.fish_instruction = value;
            if let Err(err) = settings::save(&stored) {
                crate::debug_log(&format!("fish instruction save failed: {err:#}"));
            }
        }
        self.publish();
    }

    /// Re-read `settings.json` (key, model, instruction, budget, interval).
    pub fn refresh_from_settings(&self) {
        let stored = settings::load();
        let next = EngineConfig::from_settings(&stored);
        if let Ok(mut config) = self.config.lock() {
            *config = next;
        }
    }

    // -- control -----------------------------------------------------------

    /// Start (or resume) the worker with a logged-in Bilibili session.
    ///
    /// One worker thread at a time: calling `start` while the worker sleeps
    /// between videos only replaces the session and clears the pause flag.
    pub fn start(&self, session: Arc<Session>) {
        self.refresh_from_settings();
        self.spawn_with(Signals::Live(session));
    }

    /// Ask the worker to stop taking new videos. The current video still
    /// finishes, so its verdict is never lost.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.set_state(FishState::Paused);
    }

    fn spawn_with(&self, signals: Signals) {
        if let Ok(mut slot) = self.signals.lock() {
            *slot = Some(signals);
        }
        self.paused.store(false, Ordering::SeqCst);
        if self.worker.swap(true, Ordering::SeqCst) {
            // A worker already exists; it will observe `paused = false`.
            return;
        }
        let Some(engine) = self.self_ref.lock().ok().and_then(|slot| slot.upgrade()) else {
            self.worker.store(false, Ordering::SeqCst);
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("fish-worker".to_string())
            .spawn(move || {
                engine.worker_loop();
                engine.worker.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.worker.store(false, Ordering::SeqCst);
            self.set_state(FishState::Error("无法启动养鱼线程".to_string()));
        }
    }

    // -- worker ------------------------------------------------------------

    fn worker_loop(&self) {
        let mut looked: u32 = 0;
        loop {
            if self.paused.load(Ordering::SeqCst) {
                self.set_state(FishState::Paused);
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            let Ok(config) = self.config() else {
                self.set_state(FishState::Error("养鱼配置不可用".to_string()));
                return;
            };
            if config.key.trim().is_empty() {
                self.set_state(FishState::NeedsKey);
                return;
            }
            if looked >= config.budget_videos.max(1) {
                crate::debug_log(&format!("fish run budget={} reached", looked));
                self.set_state(FishState::Idle);
                return;
            }
            let Some(job) = self.pop_next() else {
                self.set_state(FishState::Idle);
                return;
            };
            self.set_current(Some((job.bvid.clone(), job.title.clone())));
            self.set_state(FishState::Running);
            let started = Instant::now();
            // The stall trace reports `fish=`/`last=fish:step` while a step is
            // in flight, so a UI gap can be blamed on this worker (or cleared).
            crate::player::tag_here("fish:step");
            crate::player::note_fish_step(true);
            let outcome = self.process(&job, &config, started);
            crate::player::note_fish_step(false);
            looked += 1;
            match outcome {
                Ok(view) => {
                    crate::debug_log(&format!(
                        "fish done bvid={} verdict={} score={:.1} ms={} tokens={}+{}",
                        job.bvid,
                        view.verdict,
                        view.score,
                        started.elapsed().as_millis(),
                        view.tokens_in,
                        view.tokens_out
                    ));
                    self.set_current(None);
                    self.publish();
                }
                Err(err) => {
                    self.set_current(None);
                    if let Err(write_err) = self.store_error(&job, &err, started.elapsed()) {
                        self.set_state(FishState::Error(format!(
                            "写入 fish.db 失败：{write_err:#}"
                        )));
                        return;
                    }
                    crate::debug_log(&format!(
                        "fish error bvid={} attempts={} ms={} err={err:#}",
                        job.bvid,
                        job.attempts + 1,
                        started.elapsed().as_millis()
                    ));
                    self.publish();
                }
            }
            self.sleep_interval(config.interval_ms);
        }
    }

    fn sleep_interval(&self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms.max(1));
        loop {
            if self.paused.load(Ordering::SeqCst) {
                return;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    /// Claim the highest-priority pending row (and mark it `running`).
    fn pop_next(&self) -> Option<Job> {
        for _ in 0..8 {
            let at = now();
            let db = self.db.lock().ok()?;
            let job = db
                .query_row(
                    "SELECT q.bvid, COALESCE(v.title, ''), q.attempts FROM queue q \
                     LEFT JOIN videos v ON v.bvid = q.bvid \
                     WHERE q.state = 'pending' \
                     ORDER BY q.priority DESC, q.updated_at ASC, q.bvid ASC LIMIT 1",
                    [],
                    |row| {
                        Ok(Job {
                            bvid: row.get(0)?,
                            title: row.get(1)?,
                            attempts: row.get(2)?,
                        })
                    },
                )
                .optional()
                .ok()??;
            let changed = db
                .execute(
                    "UPDATE queue SET state = 'running', updated_at = ?2 \
                     WHERE bvid = ?1 AND state = 'pending'",
                    params![job.bvid, at],
                )
                .ok()?;
            drop(db);
            if changed > 0 {
                return Some(job);
            }
        }
        None
    }

    /// Gather signals → ask DeepSeek → parse → store.
    fn process(&self, job: &Job, config: &EngineConfig, started: Instant) -> Result<VerdictView> {
        // A single Bilibili call can be retried, and `danmaku_all` walks up to
        // ten segments; without a ceiling one slow video would hold the whole
        // queue (it did: a throttled run sat on one video for the full 45 s).
        let materials = runtime().block_on(async {
            match tokio::time::timeout(
                Duration::from_secs(JOB_TIMEOUT_S),
                self.gather(job),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(anyhow!("取材料超时（{JOB_TIMEOUT_S}s）")),
            }
        })?;
        let (system, user) = build_prompt(&materials, &config.instruction);
        let model = config.model.trim();
        let asked = runtime().block_on(self.ask(config, model, &system, &user))?;
        let parsed = parse_verdict(&asked.content);
        let view = VerdictView {
            bvid: materials.bvid.clone(),
            title: materials.title.clone(),
            up_name: materials.up_name.clone(),
            cover: materials.cover.clone(),
            score: parsed.score,
            verdict: parsed.verdict.clone(),
            one_line: parsed.one_line.clone(),
            reasons: parsed.reasons.clone(),
            tags: parsed.tags.clone(),
            quote: parsed.quote.clone(),
            at: now(),
            tokens_in: asked.tokens_in,
            tokens_out: asked.tokens_out,
        };
        let hash = prompt_hash(model, &system, &user);
        self.store_success(job, &materials, &view, model, &hash, started.elapsed())?;
        Ok(view)
    }

    // -- signals -----------------------------------------------------------

    async fn gather(&self, job: &Job) -> Result<Materials> {
        let mut materials = self.materials_from_db(&job.bvid)?;
        let signals = self.signals.lock().ok().and_then(|slot| slot.clone());
        match signals {
            Some(Signals::Live(session)) => self.gather_live(&session, job, &mut materials).await?,
            #[cfg(test)]
            Some(Signals::Stub) => stub_materials(&mut materials),
            None => return Err(anyhow!("养鱼引擎没有可用的 Bilibili 会话")),
        }
        Ok(materials)
    }

    /// Signal order: (1) feed metadata, (2) `conclusion/get`, (3) top comments
    /// and danmaku peaks — **always**, because the comment thread is the one
    /// signal that is about the video rather than inside it, and the owner
    /// wants it in the prompt even when an official transcript exists. The
    /// instruction (4) is added by [`build_prompt`].
    async fn gather_live(
        &self,
        session: &Session,
        job: &Job,
        materials: &mut Materials,
    ) -> Result<()> {
        match session.fish_meta(&job.bvid).await {
            Ok(meta) => {
                materials.absorb_meta(&meta);
                crate::debug_log(&format!(
                    "fish step bvid={} signal=metadata cid={} duration_s={} tname={}",
                    job.bvid, materials.cid, materials.duration_s, materials.tname
                ));
            }
            Err(err) => crate::debug_log(&format!(
                "fish metadata bvid={} failed: {err:#}",
                job.bvid
            )),
        }
        if materials.cid == 0 {
            return Ok(());
        }
        match self
            .conclusion_throttled(session, &job.bvid, materials.cid, materials.up_mid)
            .await
        {
            Ok(conclusion) => {
                materials.absorb_conclusion(conclusion);
                if materials.transcript.is_empty() {
                    crate::debug_log(&format!(
                        "fish step bvid={} signal=official transcript=empty",
                        job.bvid
                    ));
                } else {
                    crate::debug_log(&format!(
                        "fish step bvid={} signal=official transcript_chars={}",
                        job.bvid,
                        materials.transcript.chars().count()
                    ));
                }
            }
            Err(err) => {
                if crate::bilibili::is_risk_control(&err) {
                    return Err(err);
                }
                crate::debug_log(&format!(
                    "fish conclusion bvid={} failed: {err:#}",
                    job.bvid
                ));
            }
        }
        if materials.aid != 0 {
            match session.comments(materials.aid, true, "").await {
                Ok(page) => materials.absorb_comments(page),
                Err(err) => crate::debug_log(&format!(
                    "fish comments bvid={} failed: {err:#}",
                    job.bvid
                )),
            }
        }
        if materials.duration_s > 0 {
            let danmaku = session
                .danmaku_all(materials.cid, materials.duration_s.min(DANMAKU_MAX_SECONDS))
                .await;
            materials.absorb_danmaku(&danmaku);
        }
        crate::debug_log(&format!(
            "fish step bvid={} signal=comments comments={} danmaku_peaks={}",
            job.bvid,
            materials.comments.len(),
            materials.danmaku.len()
        ));
        Ok(())
    }

    /// `conclusion/get` with the required spacing and exponential backoff.
    async fn conclusion_throttled(
        &self,
        session: &Session,
        bvid: &str,
        cid: u64,
        up_mid: u64,
    ) -> Result<Conclusion> {
        let mut attempt = 0u32;
        loop {
            let wait = self
                .last_conclusion
                .lock()
                .ok()
                .and_then(|guard| *guard)
                .map(|at| Duration::from_millis(CONCLUSION_MIN_GAP_MS).saturating_sub(at.elapsed()));
            if let Some(wait) = wait {
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
            if let Ok(mut guard) = self.last_conclusion.lock() {
                *guard = Some(Instant::now());
            }
            match session.conclusion(bvid, cid, up_mid).await {
                Ok(conclusion) => return Ok(conclusion),
                Err(err) => {
                    if !crate::bilibili::is_risk_control(&err)
                        || attempt >= CONCLUSION_MAX_RETRIES
                    {
                        return Err(err);
                    }
                    attempt += 1;
                    let backoff = Duration::from_millis(CONCLUSION_MIN_GAP_MS * 2u64.pow(attempt));
                    crate::debug_log(&format!(
                        "fish backoff bvid={bvid} attempt={attempt} wait_ms={}",
                        backoff.as_millis()
                    ));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    fn materials_from_db(&self, bvid: &str) -> Result<Materials> {
        let db = self.lock_db()?;
        let row = db
            .query_row(
                "SELECT bvid, cid, title, up_mid, up_name, duration_s, view_count, pub_ts, \
                 cover, tname FROM videos WHERE bvid = ?1",
                params![bvid],
                |row| {
                    Ok(Materials {
                        bvid: row.get(0)?,
                        cid: row.get::<_, i64>(1)?.max(0) as u64,
                        title: row.get(2)?,
                        up_mid: row.get::<_, i64>(3)?.max(0) as u64,
                        up_name: row.get(4)?,
                        duration_s: row.get::<_, i64>(5)?.max(0) as u64,
                        view: row.get::<_, i64>(6)?.max(0) as u64,
                        pub_ts: row.get(7)?,
                        cover: row.get(8)?,
                        tname: row.get(9)?,
                        ..Materials::default()
                    })
                },
            )
            .optional()?;
        row.ok_or_else(|| anyhow!("fish.db 里没有 {bvid} 的元数据"))
    }

    // -- DeepSeek ----------------------------------------------------------

    async fn ask(
        &self,
        config: &EngineConfig,
        model: &str,
        system: &str,
        user: &str,
    ) -> Result<Asked> {
        let body = serde_json::json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "response_format": { "type": "json_object" },
            "temperature": 0.3,
            "max_tokens": 600,
            "stream": false,
        });
        let response = self
            .http
            .post(&config.endpoint)
            .bearer_auth(config.key.trim())
            .json(&body)
            .send()
            .await
            .context("请求 DeepSeek 失败")?;
        let status = response.status();
        let text = response.text().await.context("读取 DeepSeek 响应失败")?;
        if !status.is_success() {
            return Err(anyhow!(
                "DeepSeek 返回 HTTP {}：{}",
                status.as_u16(),
                clip(&text, 300)
            ));
        }
        let parsed: ChatResp =
            serde_json::from_str(&text).map_err(|err| anyhow!("DeepSeek 响应解析失败：{err}"))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message)
            .map(|message| message.content)
            .unwrap_or_default();
        if content.trim().is_empty() {
            return Err(anyhow!("DeepSeek 返回了空内容"));
        }
        let usage = parsed.usage.unwrap_or_default();
        Ok(Asked {
            content,
            tokens_in: usage.prompt_tokens,
            tokens_out: usage.completion_tokens,
        })
    }

    // -- persistence -------------------------------------------------------

    fn store_success(
        &self,
        job: &Job,
        materials: &Materials,
        view: &VerdictView,
        model: &str,
        prompt_hash: &str,
        elapsed: Duration,
    ) -> Result<()> {
        let at = view.at;
        let outline_json = json_or_empty(&materials.outline);
        let comments_json = json_or_empty(&materials.comments);
        let danmaku_json = json_or_empty(&materials.danmaku);
        let reasons_json = json_or_empty(&view.reasons);
        let tags_json = json_or_empty(&view.tags);
        let chars = materials.transcript.chars().count() as i64;
        let source = if materials.transcript.is_empty() {
            if materials.transcript_source.is_empty() {
                "none"
            } else {
                materials.transcript_source.as_str()
            }
        } else {
            "official"
        };
        let mut db = self.lock_db()?;
        let tx = db.transaction()?;
        upsert_video(&tx, materials, at)?;
        tx.execute(
            "INSERT INTO signals (bvid, summary, outline_json, transcript, transcript_source, \
             transcript_chars, comments_json, danmaku_json, conclusion_code, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(bvid) DO UPDATE SET summary = excluded.summary, \
             outline_json = excluded.outline_json, transcript = excluded.transcript, \
             transcript_source = excluded.transcript_source, \
             transcript_chars = excluded.transcript_chars, \
             comments_json = excluded.comments_json, danmaku_json = excluded.danmaku_json, \
             conclusion_code = excluded.conclusion_code, fetched_at = excluded.fetched_at",
            params![
                materials.bvid,
                materials.summary,
                outline_json,
                materials.transcript,
                source,
                chars,
                comments_json,
                danmaku_json,
                materials.conclusion_code,
                at
            ],
        )?;
        tx.execute(
            "INSERT INTO verdicts (bvid, at, model, prompt_hash, score, verdict, one_line, \
             reasons_json, tags_json, quote, tokens_in, tokens_out, elapsed_ms, error) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL)",
            params![
                materials.bvid,
                at,
                model,
                prompt_hash,
                view.score,
                view.verdict,
                view.one_line,
                reasons_json,
                tags_json,
                view.quote,
                view.tokens_in as i64,
                view.tokens_out as i64,
                elapsed.as_millis().min(i64::MAX as u128) as i64
            ],
        )?;
        tx.execute(
            "UPDATE queue SET state = 'done', updated_at = ?2 WHERE bvid = ?1",
            params![job.bvid, at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a failed attempt: `attempts + 1`, state `error`, and a verdict row
    /// carrying the error text (never counted as a watched video).
    fn store_error(&self, job: &Job, err: &anyhow::Error, elapsed: Duration) -> Result<()> {
        let at = now();
        let message = clip(&format!("{err:#}"), 800);
        let mut db = self.lock_db()?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO verdicts (bvid, at, model, prompt_hash, score, verdict, one_line, \
             reasons_json, tags_json, quote, tokens_in, tokens_out, elapsed_ms, error) \
             VALUES (?1, ?2, '', '', 0, '也许', '', '[]', '[]', '', 0, 0, ?3, ?4)",
            params![
                job.bvid,
                at,
                elapsed.as_millis().min(i64::MAX as u128) as i64,
                message
            ],
        )?;
        tx.execute(
            "UPDATE queue SET state = 'error', attempts = attempts + 1, last_error = ?2, \
             updated_at = ?3 WHERE bvid = ?1",
            params![job.bvid, message, at],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -- small helpers -----------------------------------------------------

    fn config(&self) -> Result<EngineConfig> {
        self.config
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| anyhow!("养鱼配置锁被污染"))
    }

    fn lock_db(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| anyhow!("fish.db 锁被污染"))
    }

    fn set_current(&self, value: Option<(String, String)>) {
        if let Ok(mut guard) = self.current.lock() {
            *guard = value;
        }
    }

    fn set_state(&self, next: FishState) {
        {
            match self.state.lock() {
                Ok(mut guard) => *guard = next,
                Err(poisoned) => *poisoned.into_inner() = next,
            }
        }
        self.publish();
    }

    fn publish(&self) {
        let hook = self.hook.lock().ok().and_then(|slot| slot.clone());
        if let Some(hook) = hook {
            hook(self.snapshot());
        }
    }

    #[cfg(test)]
    fn start_for_test(&self, signals: Signals, config: EngineConfig) {
        if let Ok(mut slot) = self.config.lock() {
            *slot = config;
        }
        self.spawn_with(signals);
    }
}

// ---------------------------------------------------------------------------
// SQL helpers
// ---------------------------------------------------------------------------

const VERDICT_SELECT: &str = "SELECT v.bvid, COALESCE(w.title, ''), COALESCE(w.up_name, ''), \
     COALESCE(w.cover, ''), v.score, v.verdict, v.one_line, v.reasons_json, v.tags_json, \
     v.quote, v.at, v.tokens_in, v.tokens_out \
     FROM verdicts v LEFT JOIN videos w ON w.bvid = v.bvid \
     WHERE v.error IS NULL ORDER BY v.at DESC, v.id DESC LIMIT ?1";

fn open_db(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)
        .with_context(|| format!("打开养鱼数据库失败：{}", path.display()))?;
    // WAL keeps a UI read from ever blocking the writer; the file lives beside
    // settings.json, so the extra `-wal` / `-shm` files sit next to it too.
    let mode: String = db
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap_or_default();
    if mode.to_ascii_lowercase() != "wal" {
        crate::debug_log(&format!("fish.db journal_mode={mode} (WAL unavailable)"));
    }
    db.execute_batch("PRAGMA foreign_keys = ON;")?;
    db.busy_timeout(Duration::from_millis(3_000))?;
    db.execute_batch(SCHEMA)
        .context("创建养鱼数据库表失败")?;
    let version: String = db
        .query_row("SELECT v FROM kv WHERE k = 'schema_version'", [], |row| row.get(0))
        .unwrap_or_else(|_| SCHEMA_VERSION.to_string());
    if version != SCHEMA_VERSION {
        // Future migrations hook in here; today only version 1 exists.
        crate::debug_log(&format!("fish.db schema_version={version}"));
    }
    Ok(db)
}

fn upsert_video(tx: &rusqlite::Transaction<'_>, materials: &Materials, at: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO videos (bvid, cid, title, up_mid, up_name, duration_s, view_count, pub_ts, \
         cover, tname, first_seen, last_seen) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
         ON CONFLICT(bvid) DO UPDATE SET cid = excluded.cid, title = excluded.title, \
         up_mid = excluded.up_mid, up_name = excluded.up_name, \
         duration_s = excluded.duration_s, view_count = excluded.view_count, \
         pub_ts = excluded.pub_ts, cover = excluded.cover, tname = excluded.tname, \
         last_seen = excluded.last_seen",
        params![
            materials.bvid,
            materials.cid as i64,
            materials.title,
            materials.up_mid as i64,
            materials.up_name,
            materials.duration_s as i64,
            materials.view as i64,
            materials.pub_ts,
            materials.cover,
            materials.tname,
            at
        ],
    )?;
    Ok(())
}

fn count(db: &Connection, sql: &str) -> rusqlite::Result<i64> {
    db.query_row(sql, [], |row| row.get(0))
}

fn last_verdict(db: &Connection) -> Result<Option<VerdictView>> {
    let mut statement = db.prepare(VERDICT_SELECT)?;
    let mut rows = statement.query(params![1i64])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(verdict_from_row(row)?))
}

fn verdict_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VerdictView> {
    let reasons: String = row.get(7)?;
    let tags: String = row.get(8)?;
    Ok(VerdictView {
        bvid: row.get(0)?,
        title: row.get(1)?,
        up_name: row.get(2)?,
        cover: row.get(3)?,
        score: row.get(4)?,
        verdict: row.get(5)?,
        one_line: row.get(6)?,
        reasons: parse_string_list(&reasons),
        tags: parse_string_list(&tags),
        quote: row.get(9)?,
        at: row.get(10)?,
        tokens_in: row.get::<_, i64>(11)?.max(0) as u64,
        tokens_out: row.get::<_, i64>(12)?.max(0) as u64,
    })
}

fn parse_string_list(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json).unwrap_or_default()
}

fn json_or_empty<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------
// Materials
// ---------------------------------------------------------------------------

impl Materials {
    fn absorb_meta(&mut self, meta: &FishMeta) {
        if !meta.title.trim().is_empty() {
            self.title = meta.title.clone();
        }
        if meta.cid > 0 {
            self.cid = meta.cid;
        }
        if meta.aid != 0 {
            self.aid = meta.aid;
        }
        if meta.up_mid > 0 {
            self.up_mid = meta.up_mid;
        }
        if !meta.up_name.trim().is_empty() {
            self.up_name = meta.up_name.clone();
        }
        if meta.duration_s > 0 {
            self.duration_s = meta.duration_s;
        }
        if meta.view > 0 {
            self.view = meta.view;
        }
        if meta.pub_ts > 0 {
            self.pub_ts = meta.pub_ts;
        }
        if !meta.cover.is_empty() {
            self.cover = meta.cover.clone();
        }
        if !meta.tname.trim().is_empty() {
            self.tname = meta.tname.clone();
        }
        if !meta.desc.trim().is_empty() {
            self.desc = meta.desc.clone();
        }
    }

    fn absorb_conclusion(&mut self, conclusion: Conclusion) {
        self.conclusion_code = conclusion.data_code;
        if !conclusion.summary.trim().is_empty() {
            self.summary = conclusion.summary;
        }
        if !conclusion.outline.is_empty() {
            self.outline = conclusion.outline;
        }
        if !conclusion.cues.is_empty() {
            let mut text = String::new();
            for cue in conclusion.cues {
                let piece = cue.text.trim();
                if piece.is_empty() {
                    continue;
                }
                text.push_str(piece);
                text.push('\n');
            }
            if !text.trim().is_empty() {
                self.transcript = text.trim_end().to_string();
                self.transcript_source = "official".to_string();
            }
        }
    }

    fn absorb_comments(&mut self, page: crate::bilibili::CommentPage) {
        let mut all = page.comments;
        if let Some(top) = page.top {
            all.push(top);
        }
        all.sort_by(|a, b| b.like.cmp(&a.like));
        self.comments = all
            .into_iter()
            .take(COMMENT_LIMIT)
            .filter_map(|comment| {
                let text = clip(comment.message.trim(), 300);
                (!text.is_empty()).then_some(StoredComment {
                    like: comment.like,
                    text,
                })
            })
            .collect();
    }

    fn absorb_danmaku(&mut self, items: &[crate::danmaku::Danmaku]) {
        self.danmaku = danmaku_peaks(items);
    }
}

/// Bucket danmaku by minute and keep the busiest ones (with a sample line).
fn danmaku_peaks(items: &[crate::danmaku::Danmaku]) -> Vec<Peak> {
    let mut buckets: BTreeMap<u32, (u32, String)> = BTreeMap::new();
    for item in items {
        let text = item.text.trim();
        if text.is_empty() {
            continue;
        }
        let minute = item.progress_ms / 60_000;
        let bucket = buckets.entry(minute).or_insert((0, String::new()));
        bucket.0 += 1;
        if bucket.1.is_empty() {
            bucket.1 = clip(text, 40);
        }
    }
    let mut peaks: Vec<Peak> = buckets
        .into_iter()
        .map(|(minute, (count, sample))| Peak {
            minute,
            count,
            sample,
        })
        .collect();
    peaks.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.minute.cmp(&b.minute)));
    peaks.truncate(DANMAKU_TOP_MINUTES);
    peaks
}

#[cfg(test)]
fn stub_materials(materials: &mut Materials) {
    materials.title = format!("{}（离线样例）", materials.title);
    materials.tname = "科技".to_string();
    materials.desc = "这是离线测试用的简介。".to_string();
    materials.summary = "这是离线测试用的摘要。".to_string();
    materials.transcript = "大家好，这是离线测试用的字幕。".to_string();
    materials.transcript_source = "official".to_string();
    materials.conclusion_code = 0;
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// Build `(system, user)` for one video. The material sections always appear in
/// the same order and the 鱼主人的指令 is the last one before the output schema.
fn build_prompt(materials: &Materials, instruction: &str) -> (String, String) {
    let instruction = settings::normalise_fish_instruction(instruction);
    let mut out = String::new();
    out.push_str("请替鱼主人判断：这个视频值不值得看。\n\n");

    out.push_str("## 视频元数据\n");
    out.push_str(&format!(
        "标题：{}\n",
        if materials.title.trim().is_empty() {
            ("（无标题）").to_string()
        } else {
            materials.title.trim().to_string()
        }
    ));
    out.push_str(&format!(
        "UP主：{}（mid {}）\n",
        if materials.up_name.trim().is_empty() {
            "未知"
        } else {
            materials.up_name.trim()
        },
        materials.up_mid
    ));
    out.push_str(&format!(
        "时长：{}（{} 秒）\n",
        mmss(materials.duration_s),
        materials.duration_s
    ));
    out.push_str(&format!("播放：{}\n", materials.view));
    out.push_str(&format!("发布：{}\n", ymd(materials.pub_ts)));
    out.push_str(&format!(
        "分区：{}\n",
        if materials.tname.trim().is_empty() {
            "未知"
        } else {
            materials.tname.trim()
        }
    ));
    out.push_str(&format!(
        "简介：{}\n",
        if materials.desc.trim().is_empty() {
            "（没有简介）".to_string()
        } else {
            clip(&collapse_ws(&materials.desc), PROMPT_DESC_CHARS)
        }
    ));

    out.push_str("\n## AI 摘要\n");
    if materials.summary.trim().is_empty() {
        out.push_str("（接口没有给出摘要。）\n");
    } else {
        out.push_str(materials.summary.trim());
        out.push('\n');
    }

    out.push_str("\n## AI 提纲\n");
    if materials.outline.is_empty() {
        out.push_str("（接口没有给出提纲。）\n");
    } else {
        for section in &materials.outline {
            out.push_str(&format!("[{}] {}\n", mmss(section.timestamp), section.title.trim()));
            for point in &section.points {
                out.push_str(&format!("  - [{}] {}\n", mmss(point.timestamp), point.content));
            }
        }
    }

    if !materials.transcript.trim().is_empty() {
        out.push_str("\n## AI 字幕\n");
        out.push_str(&clip_transcript(&materials.transcript));
        out.push('\n');
    }

    // The comment thread is always presented, transcript or not: it is what
    // the audience argues about, and the owner asked for it to be part of how
    // the fish "watches". Budgeted so a viral thread cannot crowd out the
    // transcript.
    out.push_str("\n## 热门评论（按点赞）\n");
    if materials.comments.is_empty() {
        out.push_str("（没有拿到评论。）\n");
    } else {
        let mut used = 0usize;
        for comment in &materials.comments {
            if comment.text.trim().is_empty() {
                continue;
            }
            let line = format!(
                "- [赞 {}] {}\n",
                comment.like,
                clip(&collapse_ws(&comment.text), PROMPT_COMMENT_CHARS)
            );
            used = used.saturating_add(line.chars().count());
            if used > PROMPT_COMMENTS_BUDGET {
                out.push_str("…\n");
                break;
            }
            out.push_str(&line);
        }
    }

    out.push_str("\n## 弹幕峰值\n");
    if materials.danmaku.is_empty() {
        out.push_str("（没有拿到弹幕。）\n");
    } else {
        for peak in &materials.danmaku {
            out.push_str(&format!(
                "- {}–{} 共 {} 条：{}\n",
                mmss(peak.minute as u64 * 60),
                mmss(peak.minute as u64 * 60 + 59),
                peak.count,
                peak.sample
            ));
        }
    }

    if materials.transcript.trim().is_empty() {
        out.push_str("\n（这个视频没有 AI 字幕，上面的评论和弹幕峰值就是能拿到的全部内容材料。）\n");
    }

    out.push_str("\n## 鱼主人的指令\n");
    out.push_str(&instruction);
    out.push('\n');

    out.push_str("\n## 输出\n");
    out.push_str(
        "只输出一个 JSON 对象，不要加解释、不要加代码块：\n\
         {\"score\": 0-10 的数字, \"verdict\": \"看\" 或 \"不看\" 或 \"也许\", \
         \"one_line\": \"一句话结论\", \"reasons\": [\"依据，只能来自材料\"], \
         \"tags\": [\"短标签\"], \"quote\": \"材料里最有代表性的一句原话\"}\n\n\
         评分锚点（必须按这个尺子给，不要都卡在 6 分）：\n\
         - 9-10：必看。信息密度高、有实测/一手经验、看完能直接用到。\n\
         - 7-8：值得看。有明确价值，少量注水。\n\
         - 5-6：看情况。有价值但可以只看片段，请在 one_line 里写出该跳到哪一段。\n\
         - 3-4：不值得。标题或封面与内容不符，或大段是流水账/广告。\n\
         - 0-2：别点开。纯标题党、纯引流、无有效信息。\n\n\
         判断规则：\n\
         1. 先给结论，再找依据。不要用“信息不足”当默认答案。\n\
         2. 标题党、封面党、前 30 秒纯引流 —— 直接判“不看”，并在 reasons 里指出证据。\n\
         3. 同一件事反复讲的，按实际有效信息量给分，不按时长给分。\n\
         4. reasons 写“为什么值/不值”，不要复述剧情；最多 4 条，每条不超过 60 字。\n\
         5. quote 必须是材料里真实出现过的一句原话，不要改写。\n",
    );
    (SYSTEM_PROMPT.to_string(), out)
}

/// Keep the first [`PROMPT_HEAD_CHARS`] characters and the last
/// [`PROMPT_TAIL_CHARS`], with a `…` marker in between.
fn clip_transcript(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= PROMPT_HEAD_CHARS {
        return text.to_string();
    }
    let head: String = chars[..PROMPT_HEAD_CHARS].iter().collect();
    let tail_from = chars.len().saturating_sub(PROMPT_TAIL_CHARS);
    let tail: String = chars[tail_from..].iter().collect();
    format!("{head}\n…\n{tail}")
}

// ---------------------------------------------------------------------------
// Verdict parsing
// ---------------------------------------------------------------------------

/// Parse the model's answer into a verdict.
///
/// Tolerant by design: fenced blocks are unwrapped, prose around the object is
/// ignored, a missing/unknown verdict becomes 也许 (never an error), the score
/// is clamped into `0..=10`, and anything unparseable still yields a usable
/// "也许" so the queue entry is not retried forever.
fn parse_verdict(raw: &str) -> ParsedVerdict {
    let Some(json) = extract_json_object(raw) else {
        return fallback_verdict();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return fallback_verdict();
    };
    let score = match value.get("score") {
        Some(serde_json::Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(text)) => text.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    let score = if score.is_finite() {
        score.clamp(0.0, 10.0)
    } else {
        0.0
    };
    let verdict = match value
        .get("verdict")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or_default()
    {
        "看" => "看",
        "不看" => "不看",
        _ => "也许",
    }
    .to_string();
    let one_line = value
        .get("one_line")
        .and_then(|value| value.as_str())
        .map(|text| clip(text.trim(), 200))
        .unwrap_or_default();
    let reasons = string_items(value.get("reasons"), 6, 200);
    let tags = string_items(value.get("tags"), 8, 24)
        .into_iter()
        .map(|tag| tag.trim_start_matches('#').trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    let tags = dedupe(tags);
    let quote = value
        .get("quote")
        .and_then(|value| value.as_str())
        .map(|text| clip(text.trim(), 200))
        .unwrap_or_default();
    ParsedVerdict {
        score,
        verdict,
        one_line,
        reasons,
        tags,
        quote,
    }
}

fn fallback_verdict() -> ParsedVerdict {
    ParsedVerdict {
        score: 0.0,
        verdict: "也许".to_string(),
        one_line: "模型没有按 JSON 回答，这条结论作废。".to_string(),
        reasons: Vec::new(),
        tags: Vec::new(),
        quote: String::new(),
    }
}

/// Strip ``` fences and take the first `{…}` block.
fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

fn string_items(value: Option<&serde_json::Value>, limit: usize, max_chars: usize) -> Vec<String> {
    let Some(serde_json::Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.as_str())
        .map(|text| clip(text.trim(), max_chars))
        .filter(|text| !text.is_empty())
        .take(limit)
        .collect()
}

fn dedupe(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

fn prompt_hash(model: &str, system: &str, user: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(model.as_bytes());
    hasher.update(b"\n");
    hasher.update(system.as_bytes());
    hasher.update(b"\n");
    hasher.update(user.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn score_bucket(score: f64) -> usize {
    if !score.is_finite() {
        return 0;
    }
    score.round().clamp(0.0, 10.0) as usize
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Clip to `max` characters, marking the cut with `…`.
fn clip(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Unix seconds → `YYYY-MM-DD` (UTC; `未知` for a missing stamp).
fn ymd(ts: i64) -> String {
    if ts <= 0 {
        return "未知".to_string();
    }
    // Howard Hinnant's civil_from_days.
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// The process-wide runtime the worker `block_on`s. Two worker threads are
/// enough: the engine is single-flight, and the runtime never owns the UI.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fish-rt")
            .enable_all()
            .build()
            .expect("养鱼运行时初始化失败")
    })
}

// -- DeepSeek payload --------------------------------------------------------

#[derive(Deserialize)]
struct ChatResp {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    message: Option<ChatMessage>,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    const TEST_INSTRUCTION: &str = "只看讲 Rust 的视频；别给我推唱歌的。";

    fn feed(bvid: &str, title: &str) -> FeedItem {
        FeedItem {
            bvid: bvid.to_string(),
            cid: 101,
            aid: 202,
            title: title.to_string(),
            pic: "https://example.com/cover.jpg".to_string(),
            owner: "测试UP".to_string(),
            owner_mid: 3096,
            view: 12_000,
            duration: 600,
            pub_ts: 1_700_000_000,
        }
    }

    fn temp_db(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "o11a1-fish-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(DB_FILE)
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn materials_fixture() -> Materials {
        Materials {
            bvid: "BV1TEST".to_string(),
            cid: 101,
            aid: 202,
            title: "Rust 所有权为什么这么难".to_string(),
            up_mid: 3096,
            up_name: "测试UP".to_string(),
            duration_s: 754,
            view: 12_345,
            pub_ts: 1_700_000_000,
            cover: "https://example.com/cover.jpg".to_string(),
            tname: "科技".to_string(),
            desc: "一期讲所有权的视频。".to_string(),
            summary: "讲清楚了借用检查。".to_string(),
            outline: vec![crate::bilibili::OutlineSection {
                timestamp: 12,
                title: "所有权规则".to_string(),
                points: vec![crate::bilibili::OutlinePoint {
                    timestamp: 15,
                    content: "每个值有唯一所有者".to_string(),
                }],
            }],
            ..Materials::default()
        }
    }

    // -- schema ------------------------------------------------------------

    #[test]
    fn schema_is_created_and_reopening_is_idempotent() {
        let path = temp_db("schema");
        {
            let engine = FishEngine::open_at(&path).expect("open");
            let db = engine.lock_db().expect("db");
            for table in ["videos", "signals", "verdicts", "queue", "kv"] {
                let found: i64 = db
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        params![table],
                        |row| row.get(0),
                    )
                    .expect("sqlite_master");
                assert_eq!(found, 1, "missing table {table}");
            }
            let version: String = db
                .query_row("SELECT v FROM kv WHERE k = 'schema_version'", [], |row| {
                    row.get(0)
                })
                .expect("version row");
            assert_eq!(version, SCHEMA_VERSION);
            let foreign_keys: i64 = db
                .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                .expect("foreign_keys pragma");
            assert_eq!(foreign_keys, 1);
            let journal: String = db
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .expect("journal_mode pragma");
            assert_eq!(journal.to_ascii_lowercase(), "wal");
            let indexes: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
                     AND name IN ('idx_verdicts_at', 'idx_queue_state')",
                    [],
                    |row| row.get(0),
                )
                .expect("indexes");
            assert_eq!(indexes, 2);
        }
        let engine = FishEngine::open_at(&path).expect("reopen");
        {
            let db = engine.lock_db().expect("db");
            let versions: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM kv WHERE k = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .expect("version count");
            assert_eq!(versions, 1, "re-open must not duplicate the version row");
        }
        drop(engine);
        cleanup(&path);
    }

    #[test]
    fn snapshot_starts_empty_and_reports_needs_key_without_one() {
        let path = temp_db("snapshot");
        let engine = FishEngine::open_at(&path).expect("open");
        let snapshot = engine.snapshot();
        if settings::load().deepseek_key.trim().is_empty() {
            assert_eq!(snapshot.state, FishState::NeedsKey);
        } else {
            assert_eq!(snapshot.state, FishState::Idle);
        }
        assert_eq!(snapshot.queued, 0);
        assert_eq!(snapshot.done, 0);
        assert_eq!(snapshot.errors, 0);
        assert!(snapshot.current.is_none());
        assert!(snapshot.last.is_none());
        drop(engine);
        cleanup(&path);
    }

    // -- prompt ------------------------------------------------------------

    #[test]
    fn prompt_orders_the_sections_and_puts_the_instruction_last() {
        let mut materials = materials_fixture();
        materials.transcript = "完整字幕正文。".to_string();
        materials.transcript_source = "official".to_string();
        let (system, user) = build_prompt(&materials, TEST_INSTRUCTION);
        assert!(system.contains("鲸鱼"));
        assert!(system.contains("JSON"));

        let order = [
            "## 视频元数据",
            "## AI 摘要",
            "## AI 提纲",
            "## AI 字幕",
            "## 鱼主人的指令",
        ];
        let mut cursor = 0;
        for section in order {
            let position = user
                .find(section)
                .unwrap_or_else(|| panic!("missing section {section}"));
            assert!(position >= cursor, "{section} is out of order");
            cursor = position;
        }
        assert!(
            user.find(TEST_INSTRUCTION).expect("instruction in prompt")
                > user.find("## AI 字幕").expect("transcript section"),
            "the instruction must come after the material"
        );
        assert!(user.find("## 输出").expect("output section") > cursor);
        assert!(user.contains("Rust 所有权为什么这么难"));
        assert!(user.contains("测试UP"));
    }

    #[test]
    fn prompt_uses_comments_and_danmaku_when_there_is_no_transcript() {
        let mut materials = materials_fixture();
        materials.comments = vec![StoredComment {
            like: 1_234,
            text: "讲得真清楚".to_string(),
        }];
        materials.danmaku = vec![Peak {
            minute: 12,
            count: 340,
            sample: "这里会考".to_string(),
        }];
        let (_, user) = build_prompt(&materials, TEST_INSTRUCTION);
        assert!(!user.contains("## AI 字幕"));
        assert!(user.contains("## 热门评论（按点赞）"));
        assert!(user.contains("## 弹幕峰值"));
        assert!(user.contains("[赞 1234] 讲得真清楚"));
        assert!(user.contains("12:00–12:59 共 340 条：这里会考"));
        assert!(
            user.find("## 弹幕峰值").expect("danmaku")
                < user.find("## 鱼主人的指令").expect("instruction")
        );
    }

    #[test]
    fn comments_are_part_of_the_prompt_even_with_a_transcript() {
        let mut materials = materials_fixture();
        materials.transcript = "第一句讲的是所有权，第二句讲的是借用。".to_string();
        assert!(!materials.transcript.is_empty(), "the prompt has a transcript");
        materials.comments = vec![
            StoredComment {
                like: 9_876,
                text: "这段演示我反复看了三遍".to_string(),
            },
            StoredComment {
                like: 12,
                text: "  ".to_string(),
            },
        ];
        let (_, user) = build_prompt(&materials, TEST_INSTRUCTION);
        assert!(user.contains("## AI 字幕"), "transcript section stays");
        assert!(user.contains("## 热门评论（按点赞）"), "comments are always included");
        assert!(user.contains("[赞 9876] 这段演示我反复看了三遍"));
        // The "no transcript" note only belongs to the fallback path.
        assert!(!user.contains("没有 AI 字幕"));
        assert!(
            user.find("## AI 字幕").expect("transcript")
                < user.find("## 热门评论（按点赞）").expect("comments"),
            "comments follow the transcript"
        );
        assert!(
            user.find("## 热门评论（按点赞）").expect("comments")
                < user.find("## 鱼主人的指令").expect("instruction"),
            "the instruction stays last"
        );
    }

    #[test]
    fn long_transcripts_keep_the_front_the_tail_and_a_marker() {
        let mut text = "A".repeat(PROMPT_HEAD_CHARS);
        text.push_str(&"B".repeat(5_000));
        text.push_str("结尾标记");
        let clipped = clip_transcript(&text);
        assert!(clipped.starts_with(&"A".repeat(64)));
        assert!(clipped.contains('…'));
        assert!(clipped.ends_with("结尾标记"));
        assert!(clipped.chars().count() <= PROMPT_HEAD_CHARS + PROMPT_TAIL_CHARS + 8);
        assert_eq!(clip_transcript("短字幕"), "短字幕");

        let mut materials = materials_fixture();
        materials.transcript = text;
        let (_, user) = build_prompt(&materials, TEST_INSTRUCTION);
        assert!(user.contains('…'));
        assert!(user.contains("结尾标记"));
    }

    // -- verdict parsing ---------------------------------------------------

    #[test]
    fn verdict_parses_plain_fenced_and_prose_wrapped_json() {
        let plain = r##"{"score": 8.5, "verdict": "看", "one_line": "值得看", "reasons": ["干货多", ""], "tags": ["#Rust", "教程", ""], "quote": "所有权"}"##;
        let parsed = parse_verdict(plain);
        assert_eq!(parsed.score, 8.5);
        assert_eq!(parsed.verdict, "看");
        assert_eq!(parsed.one_line, "值得看");
        assert_eq!(parsed.reasons, vec!["干货多"]);
        assert_eq!(parsed.tags, vec!["Rust", "教程"]);
        assert_eq!(parsed.quote, "所有权");

        let fenced = format!("```json\n{plain}\n```");
        assert_eq!(parse_verdict(&fenced).verdict, "看");

        let prose = format!("我看完了，结论如下：\n{plain}\n以上。");
        assert_eq!(parse_verdict(&prose).score, 8.5);

        let bare_fence = format!("```\n{plain}\n```");
        assert_eq!(parse_verdict(&bare_fence).verdict, "看");
    }

    #[test]
    fn verdict_falls_back_to_maybe_on_garbage() {
        for raw in ["这个视频不错", "", "{oops}", "[1, 2, 3]"] {
            let parsed = parse_verdict(raw);
            assert_eq!(parsed.verdict, "也许", "raw={raw:?}");
            assert_eq!(parsed.score, 0.0);
            assert!(!parsed.one_line.is_empty());
        }
        let unknown = r#"{"score": 6, "verdict": "推荐"}"#;
        assert_eq!(parse_verdict(unknown).verdict, "也许");
    }

    #[test]
    fn verdict_score_is_clamped_and_tolerates_strings() {
        assert_eq!(parse_verdict(r#"{"score": 15}"#).score, 10.0);
        assert_eq!(parse_verdict(r#"{"score": -3}"#).score, 0.0);
        assert_eq!(parse_verdict(r#"{"score": "7.5"}"#).score, 7.5);
        assert_eq!(parse_verdict(r#"{"score": null}"#).score, 0.0);
        assert_eq!(parse_verdict(r#"{"verdict": "不看"}"#).score, 0.0);
        assert_eq!(parse_verdict(r#"{"verdict": "不看"}"#).verdict, "不看");
    }

    // -- queue -------------------------------------------------------------

    fn queue_state(engine: &FishEngine, bvid: &str) -> Option<String> {
        let db = engine.lock_db().expect("db");
        db.query_row(
            "SELECT state FROM queue WHERE bvid = ?1",
            params![bvid],
            |row| row.get(0),
        )
        .optional()
        .expect("queue state")
    }

    fn queue_attempts(engine: &FishEngine, bvid: &str) -> i64 {
        let db = engine.lock_db().expect("db");
        db.query_row(
            "SELECT attempts FROM queue WHERE bvid = ?1",
            params![bvid],
            |row| row.get(0),
        )
        .expect("attempts")
    }

    /// Insert a video row plus a successful verdict at `at`, bypassing the
    /// network, so the dedupe window can be tested directly.
    fn insert_verdict(engine: &FishEngine, bvid: &str, at: i64) {
        let db = engine.lock_db().expect("db");
        db.execute(
            "INSERT INTO videos (bvid, title, first_seen, last_seen) VALUES (?1, ?1, ?2, ?2)",
            params![bvid, at],
        )
        .expect("video row");
        db.execute(
            "INSERT INTO verdicts (bvid, at, model, prompt_hash, score, verdict, one_line) \
             VALUES (?1, ?2, 'deepseek-chat', 'hash', 8, '看', '看过')",
            params![bvid, at],
        )
        .expect("verdict row");
    }

    #[test]
    fn enqueue_dedupes_within_seven_days_and_prioritises_feed_order() {
        let path = temp_db("queue");
        let engine = FishEngine::open_at(&path).expect("open");

        engine.enqueue(&[
            feed("BV1", "一"),
            feed("BV2", "二"),
            feed("BV3", "三"),
            feed("", "空"),
        ]);
        assert_eq!(engine.snapshot().queued, 3, "empty bvid dropped");

        // Feed order: the first card pops first.
        assert_eq!(engine.pop_next().expect("first").bvid, "BV1");
        assert_eq!(engine.pop_next().expect("second").bvid, "BV2");
        assert_eq!(queue_state(&engine, "BV1").as_deref(), Some("running"));

        // Dedupe: fresh verdict hides the video, stale one re-queues it.
        let at = now();
        insert_verdict(&engine, "BV9", at - 3_600);
        insert_verdict(&engine, "BV8", at - 8 * 86_400);
        engine.enqueue(&[feed("BV9", "九"), feed("BV8", "八")]);
        assert_eq!(queue_state(&engine, "BV9"), None, "fresh verdict deduped");
        assert_eq!(queue_state(&engine, "BV8").as_deref(), Some("pending"));

        // Re-enqueueing a pending row keeps one row and never resets attempts.
        engine.enqueue(&[feed("BV8", "八")]);
        let db = engine.lock_db().expect("db");
        let rows: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM queue WHERE bvid = 'BV8'",
                [],
                |row| row.get(0),
            )
            .expect("rows");
        assert_eq!(rows, 1);
        drop(db);

        // A failed attempt increments attempts and parks the row in `error`.
        let job = Job {
            bvid: "BV8".to_string(),
            title: "八".to_string(),
            attempts: 0,
        };
        engine
            .store_error(&job, &anyhow!("mock failure"), Duration::from_millis(5))
            .expect("store error");
        assert_eq!(queue_state(&engine, "BV8").as_deref(), Some("error"));
        assert_eq!(queue_attempts(&engine, "BV8"), 1);
        engine
            .store_error(&job, &anyhow!("mock failure again"), Duration::from_millis(5))
            .expect("store error");
        assert_eq!(queue_attempts(&engine, "BV8"), 2);
        assert_eq!(engine.snapshot().errors, 1);
        {
            let db = engine.lock_db().expect("db");
            let failed: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM verdicts WHERE bvid = 'BV8' AND error IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .expect("failed rows");
            assert_eq!(failed, 2, "both failed attempts are recorded");
        }
        assert_eq!(
            engine.verdicts(10).expect("verdicts").len(),
            2,
            "the API only shows successful verdicts (BV8 stale + BV9 fresh)"
        );
        // `stats(days)` windows by `at`: BV9 is one hour old, BV8 is eight days.
        assert_eq!(engine.stats(0).expect("stats all").watched, 2);
        assert_eq!(engine.stats(3).expect("stats 3d").watched, 1);

        drop(engine);
        cleanup(&path);
    }

    #[test]
    fn danmaku_peaks_keep_the_busiest_minutes() {
        let mut items: Vec<crate::danmaku::Danmaku> = Vec::new();
        let mut push = |progress_ms: u32, text: &str| {
            items.push(crate::danmaku::Danmaku {
                progress_ms,
                text: text.to_string(),
                ..crate::danmaku::Danmaku::default()
            });
        };
        push(60_000, "一分钟的第一条");
        push(61_000, "一分钟的第二条");
        push(120_000, "两分钟的一条");
        push(180_000, "三分钟一");
        push(180_500, "三分钟二");
        push(181_000, "三分钟三");
        push(182_000, "三分钟四");
        push(183_000, ""); // empty text is ignored
        let peaks = danmaku_peaks(&items);
        assert_eq!(peaks.len(), 3);
        assert_eq!(peaks[0].minute, 3);
        assert_eq!(peaks[0].count, 4);
        assert_eq!(peaks[0].sample, "三分钟一");
        assert_eq!(peaks[1].minute, 1);
        assert_eq!(peaks[1].count, 2);
        assert_eq!(peaks[2].minute, 2);
        assert_eq!(danmaku_peaks(&[]), Vec::new());
    }

    #[test]
    fn presets_are_unique_and_fit_the_instruction_field() {
        let presets = super::presets();
        assert!(presets.len() >= 8, "the picker needs a real choice");
        let mut ids = std::collections::BTreeSet::new();
        for preset in presets {
            assert!(ids.insert(preset.id), "duplicate id {}", preset.id);
            assert!(!preset.name.trim().is_empty(), "{} has no name", preset.id);
            assert!(
                !preset.tagline.trim().is_empty(),
                "{} has no tagline",
                preset.id
            );
            assert!(
                preset.body.chars().count() <= 80,
                "{} body is too long for the instruction field",
                preset.id
            );
        }
    }

    #[test]
    fn active_preset_matches_a_body_exactly() {
        let first = super::presets().first().expect("a default preset");
        assert_eq!(
            super::active_preset(first.body).map(|preset| preset.id),
            Some(first.id)
        );
        // `settings` normalises whitespace at the edges; the match must too.
        assert_eq!(
            super::active_preset(&format!("  {}  ", first.body)).map(|preset| preset.id),
            Some(first.id)
        );
        assert!(super::active_preset("我自己的口味").is_none());
    }

    #[test]
    fn time_helpers_format_minutes_and_dates() {
        assert_eq!(mmss(0), "0:00");
        assert_eq!(mmss(754), "12:34");
        assert_eq!(mmss(3_725), "1:02:05");
        assert_eq!(ymd(0), "未知");
        assert_eq!(ymd(1_700_000_000), "2023-11-14");
        assert_eq!(ymd(86_400), "1970-01-02");
        assert_eq!(clip("一二三四五", 3), "一二三…");
        assert_eq!(clip("一二三", 3), "一二三");
        assert_eq!(score_bucket(8.4), 8);
        assert_eq!(score_bucket(8.6), 9);
        assert_eq!(score_bucket(99.0), 10);
        assert_eq!(score_bucket(f64::NAN), 0);
    }

    // -- end to end --------------------------------------------------------
    /// A hand-written single-shot HTTP server (no test framework dependency).
    struct MockServer {
        port: u16,
        body: mpsc::Receiver<String>,
    }

    impl MockServer {
        fn start(reply: &str) -> Self {
            Self::start_n(reply, 1)
        }

        fn start_n(reply: &str, requests: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
            let port = listener.local_addr().expect("addr").port();
            let (tx, rx) = mpsc::channel();
            let reply = reply.to_string();
            std::thread::spawn(move || {
                for _ in 0..requests {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut head: Vec<u8> = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(read) => head.extend_from_slice(&buf[..read]),
                            Err(_) => break,
                        }
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    let content_length = text
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body: Vec<u8> = Vec::new();
                    if let Some(split) = text.find("\r\n\r\n") {
                        body.extend_from_slice(&head[split + 4..]);
                    }
                    while body.len() < content_length {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(read) => body.extend_from_slice(&buf[..read]),
                            Err(_) => break,
                        }
                    }
                    let _ = tx.send(String::from_utf8_lossy(&body).to_string());
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.len(),
                        reply
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            MockServer { port, body: rx }
        }

        fn endpoint(&self) -> String {
            format!("http://127.0.0.1:{}/chat/completions", self.port)
        }

        fn request_body(&self) -> String {
            self.body
                .recv_timeout(Duration::from_secs(5))
                .expect("mock server request body")
        }
    }

    fn mock_reply() -> String {
        let content = r#"{"score": 8, "verdict": "看", "one_line": "值得看", "reasons": ["讲得清楚"], "tags": ["Rust", "教程"], "quote": "所有权"}"#;
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1234, "completion_tokens": 56, "total_tokens": 1290 }
        })
        .to_string()
    }

    #[test]
    fn mock_run_writes_a_verdict_and_sends_the_instruction() {
        let path = temp_db("e2e");
        let server = MockServer::start(&mock_reply());
        let engine = FishEngine::open_at(&path).expect("open");
        engine.persist.store(false, Ordering::SeqCst);
        if let Ok(mut config) = engine.config.lock() {
            config.key = "sk-test-key".to_string();
            config.model = "deepseek-chat".to_string();
            config.interval_ms = 20;
            config.budget_videos = 10;
        }
        engine.set_instruction(TEST_INSTRUCTION);
        engine.enqueue(&[feed("BV1TEST", "Rust 所有权为什么这么难")]);

        let started = Instant::now();
        engine.start_for_test(
            Signals::Stub,
            EngineConfig {
                key: "sk-test-key".to_string(),
                model: "deepseek-chat".to_string(),
                instruction: TEST_INSTRUCTION.to_string(),
                budget_videos: 10,
                interval_ms: 20,
                endpoint: server.endpoint(),
            },
        );
        let deadline = started + Duration::from_secs(20);
        loop {
            let snapshot = engine.snapshot();
            if snapshot.queued == 0 && snapshot.done > 0 && snapshot.state == FishState::Idle {
                break;
            }
            assert!(Instant::now() < deadline, "mock run timed out: {snapshot:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let elapsed_ms = started.elapsed().as_millis();
        let verdicts = engine.verdicts(5).expect("verdicts");
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].bvid, "BV1TEST");
        assert_eq!(verdicts[0].verdict, "看");
        assert_eq!(verdicts[0].score, 8.0);
        assert_eq!(verdicts[0].tokens_in, 1234);
        assert_eq!(verdicts[0].tokens_out, 56);
        assert_eq!(verdicts[0].tags, vec!["Rust", "教程"]);

        let stats = engine.stats(30).expect("stats");
        assert_eq!(stats.watched, 1);
        assert_eq!(stats.recommend, 1);
        assert_eq!(stats.tokens_in, 1234);
        assert_eq!(stats.tokens_out, 56);
        assert_eq!(stats.score_histogram[8], 1);
        assert_eq!(
            stats.top_tags,
            vec![("Rust".to_string(), 1), ("教程".to_string(), 1)]
        );
        assert_eq!(queue_state(&engine, "BV1TEST").as_deref(), Some("done"));

        let snapshot = engine.snapshot();
        assert_eq!(snapshot.done, 1);
        assert_eq!(snapshot.tokens_in, 1234);
        assert!(snapshot.last.is_some());
        assert!(snapshot.current.is_none());
        assert_eq!(snapshot.state, FishState::Idle);

        let body = server.request_body();
        assert!(body.contains(TEST_INSTRUCTION), "instruction missing: {body}");
        assert!(body.contains("\"response_format\":{\"type\":\"json_object\"}"));
        assert!(body.contains("\"max_tokens\":600"));
        assert!(body.contains("\"temperature\":0.3"));
        assert!(body.contains("deepseek-chat"));
        assert!(body.contains("AI 字幕"));

        let signals: (String, String, String, i64) = {
            let db = engine.lock_db().expect("db");
            db.query_row(
                "SELECT summary, transcript, transcript_source, transcript_chars \
                 FROM signals WHERE bvid = 'BV1TEST'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("signals row")
        };
        assert_eq!(signals.0, "这是离线测试用的摘要。");
        assert_eq!(signals.2, "official");
        assert!(signals.3 > 0);

        eprintln!(
            "fish mock e2e: per-video {} ms (verdict row + signals written, one DeepSeek call)",
            elapsed_ms
        );
        drop(engine);
        cleanup(&path);
    }

    /// Five videos through the same worker, with the engine's own per-video
    /// timings read back from the debug log (`fish done ... ms=`).
    #[test]
    fn mock_run_reports_per_video_timings() {
        let path = temp_db("timings");
        let server = MockServer::start_n(&mock_reply(), 5);
        let log_path = path.parent().expect("temp dir").join("fish.log");
        std::env::set_var("BILI_DEBUG_LOG", &log_path);
        let engine = FishEngine::open_at(&path).expect("open");
        engine.persist.store(false, Ordering::SeqCst);
        let items: Vec<FeedItem> = (0..5)
            .map(|index| feed(&format!("BV1T{index}"), &format!("第 {index} 个视频")))
            .collect();
        engine.enqueue(&items);
        engine.start_for_test(
            Signals::Stub,
            EngineConfig {
                key: "sk-test-key".to_string(),
                model: "deepseek-chat".to_string(),
                instruction: TEST_INSTRUCTION.to_string(),
                budget_videos: 5,
                interval_ms: 10,
                endpoint: server.endpoint(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = engine.snapshot();
            if snapshot.done == 5 && snapshot.state == FishState::Idle {
                break;
            }
            assert!(Instant::now() < deadline, "timed out: {snapshot:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
        let text = std::fs::read_to_string(&log_path).expect("debug log");
        // `BILI_DEBUG_LOG` is process-global and read per call, so a worker
        // thread from an earlier fish test can still append its own
        // `fish done` line to this file; only this run's five videos count.
        let bvids: Vec<String> = (0..5).map(|index| format!("BV1T{index}")).collect();
        let timings: Vec<u128> = text
            .lines()
            .filter(|line| line.contains("fish done"))
            .filter(|line| {
                bvids
                    .iter()
                    .any(|bvid| line.contains(&format!("bvid={bvid} ")))
            })
            .filter_map(|line| {
                line.split("ms=")
                    .nth(1)?
                    .split_whitespace()
                    .next()?
                    .parse::<u128>()
                    .ok()
            })
            .collect();
        assert_eq!(timings.len(), 5, "one timing per video: {timings:?}");
        let total: u128 = timings.iter().sum();
        eprintln!(
            "fish mock per-video ms: {timings:?} (mean {} ms over 5 videos, interval 10 ms, \
             local mock server, no Bilibili traffic)",
            total / timings.len().max(1) as u128
        );
        std::env::remove_var("BILI_DEBUG_LOG");
        drop(engine);
        cleanup(&path);
    }

    /// Errors from DeepSeek are recorded as failed attempts, not verdicts.
    #[test]
    fn mock_run_records_http_errors_as_attempts() {
        let path = temp_db("e2e-error");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the whole request before answering: closing with unread
                // bytes makes Windows send an RST, and the client then reports
                // "connection reset" instead of the 401 this test is about.
                let mut head: Vec<u8> = Vec::new();
                let mut buf = [0u8; 2048];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(read) => head.extend_from_slice(&buf[..read]),
                        Err(_) => break,
                    }
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let body_len = text
                    .split("\r\n")
                    .find_map(|line| {
                        let line = line.to_ascii_lowercase();
                        line.strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut read_body = text
                    .find("\r\n\r\n")
                    .map(|split| head.len() - (split + 4))
                    .unwrap_or(0);
                while read_body < body_len {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(read) => read_body += read,
                        Err(_) => break,
                    }
                }
                let body = "{\"error\":{\"message\":\"bad key\"}}";
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        let engine = FishEngine::open_at(&path).expect("open");
        engine.persist.store(false, Ordering::SeqCst);
        engine.enqueue(&[feed("BV1FAIL", "会失败")]);
        engine.start_for_test(
            Signals::Stub,
            EngineConfig {
                key: "sk-test-key".to_string(),
                model: "deepseek-chat".to_string(),
                instruction: TEST_INSTRUCTION.to_string(),
                budget_videos: 1,
                interval_ms: 10,
                endpoint: format!("http://127.0.0.1:{port}/chat/completions"),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if queue_state(&engine, "BV1FAIL").as_deref() == Some("error") {
                break;
            }
            assert!(Instant::now() < deadline, "error run timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(queue_attempts(&engine, "BV1FAIL"), 1);
        assert_eq!(engine.snapshot().errors, 1);
        assert!(engine.verdicts(5).expect("verdicts").is_empty());
        let db = engine.lock_db().expect("db");
        let last_error: String = db
            .query_row(
                "SELECT last_error FROM queue WHERE bvid = 'BV1FAIL'",
                [],
                |row| row.get(0),
            )
            .expect("last_error");
        assert!(last_error.contains("401"), "unexpected error: {last_error}");
        drop(db);
        drop(engine);
        cleanup(&path);
    }

    // -- live smoke --------------------------------------------------------

    /// Real APIs, off by default: set `DEEPSEEK_API_KEY` and `BILI_FISH_LIVE=1`
    /// (optionally `BILI_FISH_BVID`) and run
    /// `cargo test --release -- --ignored live_smoke --nocapture`.
    #[test]
    #[ignore = "hits DeepSeek and Bilibili; needs DEEPSEEK_API_KEY and BILI_FISH_LIVE=1"]
    fn live_smoke() {
        let Ok(key) = std::env::var("DEEPSEEK_API_KEY") else {
            eprintln!("DEEPSEEK_API_KEY absent: skipped");
            return;
        };
        if std::env::var("BILI_FISH_LIVE").unwrap_or_default() != "1" {
            eprintln!("BILI_FISH_LIVE != 1: skipped");
            return;
        }
        let bvid = std::env::var("BILI_FISH_BVID").unwrap_or_else(|_| "BV1GJ411x7h7".to_string());
        let path = temp_db("live");
        let engine = FishEngine::open_at(&path).expect("open");
        engine.persist.store(false, Ordering::SeqCst);
        let session = runtime()
            .block_on(async {
                match crate::bilibili::load_token() {
                    Some(token) => crate::bilibili::Session::from_token(&token).await,
                    None => crate::bilibili::Session::new().await,
                }
            })
            .expect("bilibili session");
        engine.enqueue(&[feed(&bvid, "live smoke")]);
        engine.start_for_test(
            Signals::Live(Arc::new(session)),
            EngineConfig {
                key,
                model: "deepseek-chat".to_string(),
                instruction: settings::default_fish_instruction(),
                budget_videos: 1,
                interval_ms: 1_500,
                endpoint: DEEPSEEK_URL.to_string(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(180);
        while engine.snapshot().queued > 0 {
            assert!(Instant::now() < deadline, "live run timed out");
            std::thread::sleep(Duration::from_millis(500));
        }
        let verdicts = engine.verdicts(5).expect("verdicts");
        eprintln!("live verdicts={} snapshot={:?}", verdicts.len(), engine.snapshot());
        assert!(!verdicts.is_empty(), "live run produced no verdict");
        drop(engine);
        cleanup(&path);
    }
}
