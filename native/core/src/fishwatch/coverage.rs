//! 覆盖率与盲区文案: how much of the video the fish actually read, and what it
//! could not read.
//!
//! Coverage is measured in **seconds of the timeline covered by timed
//! material**, never in characters: a transcript with 4 000 characters that
//! only covers the first two minutes of a 40-minute video is 5 %, not 100 %.
//! Sampled frames are reported separately and never inflate the speech number.

use crate::fishwatch::steps::{StepId, StepReport};
use crate::fishwatch::Gathered;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    /// 0.0–1.0: union of timed speech intervals / duration.
    pub fraction: f64,
    /// 0.0–1.0: sampled-frame span / duration (visual only).
    pub visual_fraction: f64,
    /// User-facing blind-spot sentence(s).
    pub note: String,
}

#[cfg(test)]
impl Coverage {
    pub fn percent(&self) -> u32 {
        (self.fraction * 100.0).round().clamp(0.0, 100.0) as u32
    }
}

/// Union of `[start, end]` intervals clipped to `duration`, in seconds.
pub fn covered_seconds(intervals: &[(f64, f64)], duration: f64) -> f64 {
    if duration <= 0.0 {
        return 0.0;
    }
    let mut spans: Vec<(f64, f64)> = intervals
        .iter()
        .map(|(start, end)| (start.max(0.0), end.min(duration)))
        .filter(|(start, end)| end > start)
        .collect();
    if spans.is_empty() {
        return 0.0;
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
    let mut total = 0.0;
    let (mut start, mut end) = spans[0];
    for (next_start, next_end) in spans.into_iter().skip(1) {
        if next_start <= end {
            end = end.max(next_end);
        } else {
            total += end - start;
            start = next_start;
            end = next_end;
        }
    }
    total += end - start;
    total.min(duration)
}

/// Compute the coverage line from the gathered material and the step reports.
pub fn compute(gathered: &Gathered, steps: &[StepReport]) -> Coverage {
    let duration = gathered.meta.duration_s as f64;
    let speech_intervals: Vec<(f64, f64)> = gathered
        .transcript_segments
        .iter()
        .map(|segment| (segment.start_s, segment.end_s))
        .collect();
    let speech = if duration > 0.0 {
        covered_seconds(&speech_intervals, duration) / duration
    } else {
        0.0
    };
    let source = gathered.transcript_source.trim();
    let ok = |id: StepId| steps.iter().any(|step| step.id == id && step.is_ok());
    let selected = |id: StepId| steps.iter().any(|step| step.id == id);

    let mut visual = 0.0;
    if let Some(frames) = &gathered.frames {
        if duration > 0.0 {
            visual = ((frames.frames as f64) * frames.interval_s / duration).min(1.0);
        }
    }

    let mut notes: Vec<String> = Vec::new();
    if speech > 0.0 {
        let label = if source == "asr" {
            "ASR 转写"
        } else {
            "官方字幕"
        };
        notes.push(format!(
            "{label}覆盖 {:.0}% 的时长（{} 段逐句）",
            speech * 100.0,
            gathered.transcript_segments.len()
        ));
        if source == "asr" {
            notes.push("ASR 专名需回查".to_string());
        }
    } else if ok(StepId::Subtitle) {
        notes.push("官方接口没有给出字幕".to_string());
    } else if selected(StepId::Asr) {
        notes.push("没有取到口播（ASR 未完成）".to_string());
    } else {
        notes.push("未取到口播，只按标题与评论判断".to_string());
    }

    if ok(StepId::Frames) {
        if let Some(frames) = &gathered.frames {
            notes.push(format!(
                "画面为抽样：{} 张 / 每 {:.0} 秒 1 帧，不代表完整观看",
                frames.frames, frames.interval_s
            ));
        }
        if let Some(report) = steps
            .iter()
            .find(|step| step.id == StepId::Frames && !step.reason.is_empty())
        {
            notes.push(report.reason.clone());
        }
    } else if selected(StepId::Frames) {
        notes.push("画面未取到".to_string());
    }

    if gathered.comments.is_empty() {
        notes.push("没有拿到评论".to_string());
    }

    Coverage {
        fraction: speech.clamp(0.0, 1.0),
        visual_fraction: visual,
        note: notes.join("；"),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::fishwatch::comments::{CommentItem, DanmakuPeak};
    use crate::fishwatch::steps::StepReport;
    use crate::fishwatch::transcript::Segment;
    use crate::fishwatch::{FrameGathered, Gathered, Meta};

    fn gathered(duration_s: u64) -> Gathered {
        Gathered {
            meta: Meta {
                duration_s,
                ..Meta::default()
            },
            ..Gathered::default()
        }
    }

    #[test]
    fn intervals_are_unioned_not_summed() {
        assert_eq!(covered_seconds(&[(0.0, 30.0), (20.0, 60.0)], 100.0), 60.0);
        assert_eq!(covered_seconds(&[(0.0, 30.0), (40.0, 60.0)], 100.0), 50.0);
        assert_eq!(covered_seconds(&[(0.0, 200.0)], 100.0), 100.0);
        assert_eq!(covered_seconds(&[], 100.0), 0.0);
        assert_eq!(covered_seconds(&[(0.0, 10.0)], 0.0), 0.0);
    }

    #[test]
    fn subtitle_coverage_uses_the_timeline() {
        let mut material = gathered(100);
        material.transcript_source = "official".to_string();
        material.transcript_segments = vec![
            Segment {
                start_s: 0.0,
                end_s: 40.0,
                text: "前半".to_string(),
            },
            Segment {
                start_s: 50.0,
                end_s: 90.0,
                text: "后半".to_string(),
            },
        ];
        let reports = vec![StepReport::ok(StepId::Subtitle), StepReport::ok(StepId::Comments)];
        let coverage = compute(&material, &reports);
        assert_eq!(coverage.percent(), 80);
        assert!(coverage.note.contains("官方字幕覆盖 80%"));
    }

    #[test]
    fn a_missing_transcript_is_a_blind_spot_not_zero_confidence() {
        let material = gathered(100);
        let reports = vec![StepReport::ok(StepId::Meta), StepReport::ok(StepId::Comments)];
        let coverage = compute(&material, &reports);
        assert_eq!(coverage.percent(), 0);
        assert!(coverage.note.contains("未取到口播"));
    }

    #[test]
    fn sampled_frames_are_reported_as_samples() {
        let mut material = gathered(300);
        material.frames = Some(FrameGathered {
            frames: 20,
            interval_s: 15.0,
            ..FrameGathered::default()
        });
        let reports = vec![StepReport::ok(StepId::Frames)];
        let coverage = compute(&material, &reports);
        assert!(coverage.visual_fraction > 0.99);
        assert_eq!(coverage.fraction, 0.0, "frames never inflate speech coverage");
        assert!(coverage.note.contains("不代表完整观看"));
    }

    #[test]
    fn comment_only_material_says_so() {
        let mut material = gathered(60);
        material.comments = vec![CommentItem {
            like: 1,
            text: "好看".to_string(),
        }];
        material.danmaku = vec![DanmakuPeak {
            minute: 1,
            count: 3,
            sample: "哈哈哈".to_string(),
        }];
        let reports = vec![StepReport::ok(StepId::Meta), StepReport::ok(StepId::Comments)];
        let coverage = compute(&material, &reports);
        assert!(coverage.note.contains("未取到口播"));
        assert!(!coverage.note.contains("没有拿到评论"));
    }
}

