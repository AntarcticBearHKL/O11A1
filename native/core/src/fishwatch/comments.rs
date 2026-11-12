//! 热评 + 弹幕峰值.
//!
//! The audience signal is the one channel that is about the video rather than
//! inside it, which is why the design keeps it in every text tier. Danmaku is
//! bucketed by minute here (not in the UI) so the prompt and the evidence line
//! agree on what "热度" means.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::anyhow;

use crate::fishwatch::steps::{SkipReason, Step, StepFuture, StepId, StepOutput};
use crate::fishwatch::{WatchConfig, WatchCtx};

/// Top-level comments kept as material.
pub const COMMENT_LIMIT: usize = 20;
/// One comment is clipped to this many characters.
pub const COMMENT_CHARS: usize = 300;
/// Busiest danmaku minutes kept.
pub const DANMAKU_TOP_MINUTES: usize = 5;
/// Danmaku is only scanned for the first hour, to bound traffic.
pub const DANMAKU_MAX_SECONDS: u64 = 3_600;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentItem {
    pub like: u64,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DanmakuPeak {
    pub minute: u32,
    pub count: u32,
    pub sample: String,
}

/// 评论 + 弹幕. One step because both answer the same question ("what did the
/// audience get out of it") and both are skipped together when the aid/cid is
/// missing.
pub struct CommentsStep;

impl Step for CommentsStep {
    fn id(&self) -> StepId {
        StepId::Comments
    }


    fn timeout(&self) -> Duration {
        Duration::from_secs(40)
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
            let Some(session) = ctx.session.clone() else {
                return Err(anyhow!("未登录"));
            };
            let aid = ctx.gathered.meta.aid;
            let cid = ctx.gathered.meta.cid;
            let duration = ctx.gathered.meta.duration_s;
            let mut comments_ok = false;
            let mut danmaku_ok = false;
            let mut notes: Vec<String> = Vec::new();
            let mut bytes = 0u64;

            if aid != 0 {
                match session.comments(aid, true, "").await {
                    Ok(page) => {
                        let mut all = page.comments;
                        if let Some(top) = page.top {
                            all.push(top);
                        }
                        all.sort_by(|a, b| b.like.cmp(&a.like));
                        ctx.gathered.comments = all
                            .into_iter()
                            .take(COMMENT_LIMIT)
                            .filter_map(|comment| {
                                let text = clip(comment.message.trim(), COMMENT_CHARS);
                                bytes += text.len() as u64;
                                (!text.is_empty()).then_some(CommentItem {
                                    like: comment.like,
                                    text,
                                })
                            })
                            .collect();
                        comments_ok = true;
                    }
                    Err(err) => notes.push(format!("评论失败：{err:#}")),
                }
            } else {
                notes.push("缺少 aid".to_string());
            }

            if duration > 0 && cid != 0 {
                let items = session
                    .danmaku_all(cid, duration.min(DANMAKU_MAX_SECONDS))
                    .await;
                if items.is_empty() {
                    notes.push("没有拿到弹幕".to_string());
                } else {
                    ctx.gathered.danmaku = danmaku_peaks(&items);
                    bytes += items.iter().map(|item| item.text.len() as u64).sum::<u64>();
                    danmaku_ok = true;
                }
            }

            let count = ctx.gathered.comments.len();
            let peaks = ctx.gathered.danmaku.len();
            if !comments_ok && !danmaku_ok {
                return Err(anyhow!(notes.join("；")));
            }
            Ok(StepOutput {
                detail: format!("{} 条 · 弹幕峰值 {}", count, peaks),
                bytes,
                note: notes.join("；"),
                ..StepOutput::default()
            })
        })
    }
}

/// Bucket danmaku by minute and keep the busiest ones with a sample line.
pub fn danmaku_peaks(items: &[crate::danmaku::Danmaku]) -> Vec<DanmakuPeak> {
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
    let mut peaks: Vec<DanmakuPeak> = buckets
        .into_iter()
        .map(|(minute, (count, sample))| DanmakuPeak {
            minute,
            count,
            sample,
        })
        .collect();
    peaks.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.minute.cmp(&b.minute)));
    peaks.truncate(DANMAKU_TOP_MINUTES);
    peaks
}

pub fn clip(text: &str, max: usize) -> String {
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
