//! The tool surface of the continuous 养鱼 agent.
//!
//! This is deliberately separate from the six fixed watch-plan steps in
//! [`super::tool`]: the agent now chooses its own video, reads the full comment
//! thread, cuts short audio/video slices and rates the result whenever it has
//! seen enough. The functions here are pure Bilibili/transcode operations; the
//! loop that talks to DeepSeek and persists the verdict lives in `fish.rs`.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::frames::lab::{self, BuiltSheet, MovieSource};
use super::transcript::{ConclusionGate, Segment};
use super::WatchConfig;
use crate::bilibili::{Comment, Session};

/// One tool advertised to DeepSeek and painted by the 养鱼 header.
#[derive(Clone, Debug, Serialize)]
pub struct AgentToolSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub timeout_s: u64,
}

pub fn tool_catalog() -> Vec<AgentToolSpec> {
    vec![
        AgentToolSpec {
            name: "list_videos",
            label: "候选列表",
            description: "从首页推荐或收藏夹取回一批视频，按标题挑一个。",
            timeout_s: 60,
        },
        AgentToolSpec {
            name: "open_video",
            label: "视频与评论",
            description: "读取视频信息、完整简介和评论，决定要不要继续看。",
            timeout_s: 90,
        },
        AgentToolSpec {
            name: "watch_clip",
            label: "分段切片",
            description: "按 1/5/10/20/30/60 秒抽帧，打包成 6×5 拼图，同时返回该段字幕与弹幕。",
            timeout_s: 600,
        },
        AgentToolSpec {
            name: "rate_video",
            label: "点评打分",
            description: "给出这个视频的最终评分、结论和依据。",
            timeout_s: 10,
        },
    ]
}

pub fn tool_label(name: &str) -> &'static str {
    match name {
        "list_videos" => "候选列表",
        "open_video" => "视频与评论",
        "watch_clip" => "分段切片",
        "rate_video" => "点评打分",
        _ => "工具",
    }
}

/// The OpenAI-compatible function schemas. Keep names and argument keys in
/// sync with `execute_tool` in `fish.rs`.
pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "list_videos",
                "description": "取回一批还没看过的首页候选视频。先用它，按标题/UP/时长挑一个。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "minimum": 4,
                            "maximum": 20,
                            "description": "返回多少条候选，默认 10。"
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "open_video",
                "description": "选中一个视频，读取完整信息、简介和评论。返回后决定继续看还是直接打分。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "bvid": {
                            "type": "string",
                            "description": "list_videos 返回的 bvid。"
                        }
                    },
                    "required": ["bvid"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "watch_clip",
                "description": "看指定视频的一段：按 interval_s 抽帧并打包成 6×5 联系表（每张最多 30 帧，自动分页），同时返回该段的全部字幕和弹幕，图片随后附上。一次最多 120 秒。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "bvid": {
                            "type": "string",
                            "description": "当前正在看的 bvid；省略时使用 open_video 打开的视频。"
                        },
                        "start_s": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "切片开始时间，秒。"
                        },
                        "end_s": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "切片结束时间，秒；与 start_s 相差 5-120 秒。"
                        },
                        "interval_s": {
                            "type": "integer",
                            "enum": [1, 5, 10, 20, 30, 60],
                            "description": "抽帧间隔，默认 1 秒。1 秒最细；只想看节奏/长片段时用 5/10/20/30/60 秒，工具自动决定分几张拼图。"
                        },
                        "focus": {
                            "type": "string",
                            "description": "这一段你最想确认什么，用一句话写下来。"
                        }
                    },
                    "required": ["start_s", "end_s"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "rate_video",
                "description": "结束这个视频并打分。看到任何时候都可以调用；调用后系统会自动进入下一个视频。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "bvid": {
                            "type": "string",
                            "description": "当前视频的 bvid；省略时使用 open_video 打开的视频。"
                        },
                        "score": {
                            "type": "number",
                            "minimum": 0,
                            "maximum": 10
                        },
                        "verdict": {
                            "type": "string",
                            "enum": ["看", "不看", "也许"]
                        },
                        "one_line": {
                            "type": "string",
                            "description": "一句话结论。"
                        },
                        "reasons": {
                            "type": "array",
                            "items": { "type": "string" },
                            "maxItems": 5,
                            "description": "最多 5 条依据，必须来自看过的材料。"
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "maxItems": 8
                        },
                        "quote": {
                            "type": "string",
                            "description": "材料里最有代表性的一句原话。"
                        }
                    },
                    "required": ["score", "verdict", "one_line"]
                }
            }
        }
    ])
}

/// The stable system prompt. The owner's instruction is appended by `fish.rs`,
/// so the transcript can show exactly what was sent.
pub const SYSTEM_PROMPT: &str = "你是 O11A1「养鱼」栏目里的看片 agent。鱼主人让你替他刷视频，最后只回答一件事：这个视频值不值得花时间看。\n\n\
你可以连续工作。每看完一个视频，必须调用 rate_video 给出结论；系统会自动给你下一批候选，你不需要问鱼主人。\n\n\
工作流程：\n\
1. 先调用 list_videos，从首页推荐里按标题、UP 主、时长和播放量挑一个最有意思的。不要随机挑，说明你为什么选它。\n\
2. 调用 open_video 读取视频信息、完整简介和评论。看标题与内容是否一致、评论区在争什么；如果一眼就是标题党、广告或完全不感兴趣，可以直接 rate_video 低分结束。\n\
3. 决定继续看时，调用 watch_clip 分段看。默认先用 interval_s=1 看 30-60 秒；只有确认是纯口播、动作慢或只需要看节奏时，才放宽到 5/10/20/30/60 秒。一次 30-90 秒最合适。工具会把这一段的字幕、弹幕和按间隔抽出的画面打包成 6×5 联系表（每张最多 30 帧，自动分页），图片随后附上。先看开头，再根据兴趣看中段/高潮/结尾，最多看 4 段。\n\
4. 任何时候觉得够了，调用 rate_video。把分数、看/不看、一句话结论、依据、标签和一句原话写清楚。\n\n\
规则：\n\
- 只依据工具返回的材料判断，不编造剧情、数据或评论。\n\
- 每条依据都要能指出来源：画面里看到的、字幕里说的、弹幕/评论里写的。\n\
- 弹幕和评论是观众反应，不是事实本身；不要因为弹幕多就判好看。\n\
- 没字幕时不要假装听过口播，只能依据画面和评论判断，并在结论里说明。\n\
- 标题党、封面党、前 30 秒纯引流：直接判「不看」，并引用证据。\n\
- 同一件事反复讲，按有效信息量给分，不按时长给分。\n\
- 评分锚点：9-10 必看；7-8 值得看；5-6 看情况、可只看片段；3-4 不值得；0-2 别点开。\n\
- 最后必须调用 rate_video。不要用普通文本代替工具调用，也不要在 rate_video 之后再输出别的结论。";

#[derive(Clone, Debug, Default, Serialize)]
pub struct Candidate {
    pub bvid: String,
    pub title: String,
    pub up_name: String,
    pub duration_s: u64,
    pub view: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CandidateList {
    pub source: String,
    pub label: String,
    pub videos: Vec<Candidate>,
}

/// Fetch fresh candidates from the configured source. The caller filters
/// watched/queued rows and enqueues what it keeps.
pub async fn fetch_candidates(
    session: &Session,
    source: &str,
    folder_id: i64,
    limit: usize,
) -> Result<CandidateList> {
    let wanted = limit.clamp(4, 20);
    if source == "favorites" {
        if folder_id <= 0 {
            bail!("还没有选择收藏夹");
        }
        let mut videos = Vec::new();
        let mut label = String::new();
        for page in 1..=4i64 {
            let list = session
                .fav_list(folder_id, page, 20)
                .await
                .with_context(|| "读取收藏夹失败")?;
            if label.is_empty() {
                label = list.title;
            }
            videos.extend(list.items.into_iter().map(|item| Candidate {
                bvid: item.bvid,
                title: item.title,
                up_name: item.upper,
                duration_s: item.duration,
                view: item.play,
            }));
            if !list.has_more || videos.len() >= wanted * 2 {
                break;
            }
        }
        dedupe_candidates(&mut videos);
        videos.truncate(wanted * 2);
        return Ok(CandidateList {
            source: "favorites".to_string(),
            label: if label.trim().is_empty() {
                "收藏夹".to_string()
            } else {
                label
            },
            videos,
        });
    }

    let rows = session
        .homepage_feed_count(0, wanted * 2, &|_, _| {})
        .await
        .context("读取首页推荐失败")?;
    let mut videos: Vec<Candidate> = rows
        .into_iter()
        .map(|item| Candidate {
            bvid: item.bvid,
            title: item.title,
            up_name: item.owner,
            duration_s: item.duration,
            view: item.view,
        })
        .collect();
    dedupe_candidates(&mut videos);
    videos.truncate(wanted * 2);
    Ok(CandidateList {
        source: "home".to_string(),
        label: "首页推荐".to_string(),
        videos,
    })
}

fn dedupe_candidates(videos: &mut Vec<Candidate>) {
    let mut seen = std::collections::HashSet::new();
    videos.retain(|item| !item.bvid.trim().is_empty() && seen.insert(item.bvid.clone()));
    videos.retain(|item| !item.title.trim().is_empty());
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AgentComment {
    pub user: String,
    pub like: u64,
    pub replies: i64,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct OpenedVideo {
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
    pub comments: Vec<AgentComment>,
    pub comments_total: i64,
    pub cues: Vec<Segment>,
    pub bytes: u64,
}

/// Read the complete information card and (up to) `comment_limit` top-level
/// comments. The official subtitle is cached here because every clip needs it.
pub async fn open_video(
    session: &Session,
    gate: &ConclusionGate,
    bvid: &str,
    comment_limit: usize,
) -> Result<OpenedVideo> {
    let info = session
        .fish_meta(bvid)
        .await
        .with_context(|| format!("读取视频信息失败：{bvid}"))?;
    let mut bytes = (info.title.len()
        + info.up_name.len()
        + info.desc.len()
        + info.tname.len()
        + info.cover.len()) as u64;

    let limit = comment_limit.clamp(20, 120);
    let mut comments: Vec<Comment> = Vec::new();
    let mut total = 0i64;
    let mut cursor = String::new();
    for _ in 0..8 {
        let page = match session.comments(info.aid, true, &cursor).await {
            Ok(page) => page,
            Err(err) => {
                crate::debug_log(&format!("fish agent comments bvid={bvid} failed: {err:#}"));
                break;
            }
        };
        total = page.total.max(total);
        if let Some(top) = page.top {
            comments.push(top);
        }
        comments.extend(page.comments);
        if page.is_end || page.cursor.trim().is_empty() || comments.len() >= limit {
            break;
        }
        cursor = page.cursor;
    }
    let mut seen = std::collections::HashSet::new();
    comments.retain(|comment| seen.insert(comment.rpid));
    comments.sort_by(|a, b| b.like.cmp(&a.like));
    comments.truncate(limit);
    let comments: Vec<AgentComment> = comments
        .into_iter()
        .filter_map(|comment| {
            let text = clip(comment.message.trim(), 320);
            if text.is_empty() {
                return None;
            }
            bytes += text.len() as u64 + comment.user.uname.len() as u64;
            Some(AgentComment {
                user: comment.user.uname,
                like: comment.like,
                replies: comment.rcount,
                text,
            })
        })
        .collect();

    let mut cues = Vec::new();
    match gate
        .fetch(session, bvid, info.cid, info.up_mid)
        .await
    {
        Ok(conclusion) => {
            cues = conclusion
                .cues
                .into_iter()
                .filter(|cue| !cue.text.trim().is_empty() && cue.end_s > cue.start_s)
                .map(|cue| Segment {
                    start_s: cue.start_s as f64,
                    end_s: cue.end_s as f64,
                    text: cue.text.trim().to_string(),
                })
                .collect();
            bytes += cues.iter().map(|cue| cue.text.len() as u64).sum::<u64>();
        }
        Err(err) => {
            crate::debug_log(&format!("fish agent conclusion bvid={bvid} failed: {err:#}"));
        }
    }

    Ok(OpenedVideo {
        bvid: bvid.to_string(),
        cid: info.cid,
        aid: info.aid,
        title: info.title,
        up_mid: info.up_mid,
        up_name: info.up_name,
        duration_s: info.duration_s,
        view: info.view,
        pub_ts: info.pub_ts,
        cover: info.cover,
        tname: info.tname,
        desc: info.desc,
        comments,
        comments_total: total.max(0),
        cues,
        bytes,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DanmakuLine {
    /// Seconds from the start of the video (fractional for precision).
    pub at_s: f64,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct ClipData {
    pub start_s: u64,
    pub end_s: u64,
    pub subtitles: Vec<Segment>,
    pub danmaku: Vec<DanmakuLine>,
    pub danmaku_total: usize,
    pub sheets: Vec<BuiltSheet>,
    pub frame_count: usize,
    pub image_bytes: u64,
    pub bytes: u64,
}

/// Cut one range from the cached movie and filter the cached cues/danmaku.
pub async fn build_clip(
    config: &WatchConfig,
    session: &Session,
    movie: &MovieSource,
    cid: u64,
    work: &Path,
    start_s: u64,
    end_s: u64,
    interval_s: u64,
    cues: &[Segment],
    cancel: &AtomicBool,
) -> Result<ClipData> {
    let sheets = lab::build_range_sheets(
        config,
        movie,
        work,
        start_s,
        end_s,
        interval_s,
        cancel,
    )?;
    clip_from_sheets(session, cid, start_s, end_s, cues, sheets).await
}

/// Shared post-processing once the contact sheets exist, whether they came from
/// a cached movie or directly from the CDN.
pub async fn clip_from_sheets(
    session: &Session,
    cid: u64,
    start_s: u64,
    end_s: u64,
    cues: &[Segment],
    sheets: Vec<BuiltSheet>,
) -> Result<ClipData> {
    let image_bytes = sheets
        .iter()
        .filter_map(|sheet| std::fs::metadata(&sheet.path).ok())
        .map(|meta| meta.len())
        .sum::<u64>();
    let frame_count = sheets.iter().map(|sheet| sheet.times.len()).sum::<usize>();
    let subtitles: Vec<Segment> = cues
        .iter()
        .filter(|cue| cue.end_s >= start_s as f64 && cue.start_s <= end_s as f64)
        .cloned()
        .collect();
    let danmaku_raw = session.danmaku_range(cid, start_s, end_s).await;
    let danmaku_total = danmaku_raw.len();
    let danmaku: Vec<DanmakuLine> = sample_danmaku(&danmaku_raw, 160)
        .into_iter()
        .map(|item| DanmakuLine {
            at_s: item.progress_ms as f64 / 1_000.0,
            text: clip(item.text.trim(), 100),
        })
        .filter(|item| !item.text.is_empty())
        .collect();
    let bytes = image_bytes
        + subtitles.iter().map(|cue| cue.text.len() as u64).sum::<u64>()
        + danmaku.iter().map(|line| line.text.len() as u64).sum::<u64>();
    Ok(ClipData {
        start_s,
        end_s,
        subtitles,
        danmaku,
        danmaku_total,
        sheets,
        frame_count,
        image_bytes,
        bytes,
    })
}

/// The movie does not carry the cid; the caller-stored `OpenedVideo` is the
/// source of truth. This helper exists so the signature stays explicit.
fn sample_danmaku(
    items: &[crate::danmaku::Danmaku],
    limit: usize,
) -> Vec<crate::danmaku::Danmaku> {
    if items.len() <= limit {
        return items.to_vec();
    }
    // Evenly sample the window instead of returning only the first 160 lines.
    let step = items.len() as f64 / limit as f64;
    (0..limit)
        .filter_map(|index| items.get((index as f64 * step).floor() as usize))
        .cloned()
        .collect()
}

fn clip(text: &str, max: usize) -> String {
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
