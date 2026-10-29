//! faster-whisper adapter: the GPU engine verified on this machine.
//!
//! The engine is a local Python process (CTranslate2 under the hood). Keeping it
//! external means the app does not have to ship a second CUDA runtime, and the
//! exact backend/device/wall time can be logged without guessing. The script is
//! written once into the fish audio cache and then reused.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::fishwatch::asr::{run_with_timeout, Transcriber, Transcript};
use crate::fishwatch::proc;
use crate::fishwatch::transcript::Segment;

/// Embedded helper. It prints nothing to stdout; the JSON result goes to
/// `--out`, so a warning on stderr can never corrupt the parse.
const FW_SCRIPT: &str = r#"
import argparse, json, sys, time, traceback

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--audio", required=True)
    parser.add_argument("--device", default="cuda")
    parser.add_argument("--compute", default="float16")
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    try:
        from faster_whisper import WhisperModel
    except Exception as exc:
        print("faster_whisper import failed: %r" % (exc,), file=sys.stderr)
        return 2
    try:
        started = time.time()
        model = WhisperModel(
            args.model,
            device=args.device,
            compute_type=args.compute,
            cpu_threads=max(1, args.threads),
            num_workers=1,
        )
        load_s = time.time() - started
        started = time.time()
        segments, info = model.transcribe(
            args.audio,
            language=None,
            beam_size=5,
            vad_filter=True,
            condition_on_previous_text=False,
        )
        rows = []
        for segment in segments:
            text = (segment.text or "").strip()
            if not text:
                continue
            rows.append({
                "start": float(segment.start or 0.0),
                "end": float(segment.end or 0.0),
                "text": text,
            })
        payload = {
            "segments": rows,
            "language": getattr(info, "language", ""),
            "language_probability": float(getattr(info, "language_probability", 0.0) or 0.0),
            "duration": float(getattr(info, "duration", 0.0) or 0.0),
            "load_s": load_s,
            "wall_s": time.time() - started,
            "device": args.device,
            "compute": args.compute,
        }
        with open(args.out, "w", encoding="utf-8") as handle:
            json.dump(payload, handle, ensure_ascii=False)
        return 0
    except Exception:
        traceback.print_exc()
        return 3

if __name__ == "__main__":
    sys.exit(main())
"#;

#[derive(Clone, Debug)]
pub struct ExternalFw {
    pub python: String,
    pub script: PathBuf,
    pub model_dir: PathBuf,
    pub device: String,
    pub compute: String,
    pub threads: u32,
    pub work_dir: PathBuf,
}

#[derive(Deserialize)]
struct FwOutput {
    #[serde(default)]
    segments: Vec<FwSegment>,
    #[serde(default)]
    language: String,
    #[serde(default)]
    language_probability: f32,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    load_s: f64,
    #[serde(default)]
    wall_s: f64,
}

#[derive(Deserialize)]
struct FwSegment {
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    text: String,
}

impl Transcriber for ExternalFw {
    fn transcribe(&self, audio: &Path) -> Result<Transcript> {
        std::fs::create_dir_all(&self.work_dir)
            .with_context(|| format!("创建 ASR 工作目录失败：{}", self.work_dir.display()))?;
        if let Some(parent) = self.script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Rewrite the helper every run: it is 2 KB, and this guarantees a new
        // build never executes an old script left behind by a previous version.
        std::fs::write(&self.script, FW_SCRIPT)
            .with_context(|| format!("写 faster-whisper 脚本失败：{}", self.script.display()))?;
        let out_path = self
            .work_dir
            .join(format!("fw-{}.json", std::process::id()));
        let log_path = self
            .work_dir
            .join(format!("fw-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&out_path);

        let started = std::time::Instant::now();
        let mut command = proc::command(&self.python);
        command
            .arg(&self.script)
            .arg("--model")
            .arg(&self.model_dir)
            .arg("--audio")
            .arg(audio)
            .arg("--device")
            .arg(&self.device)
            .arg("--compute")
            .arg(&self.compute)
            .arg("--threads")
            .arg(self.threads.to_string())
            .arg("--out")
            .arg(&out_path)
            .current_dir(&self.work_dir);
        let status = run_with_timeout(command, Duration::from_secs(1_500), &log_path)?;
        if !status.success() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(anyhow!(
                "faster-whisper 退出码 {:?}：{}",
                status.code(),
                tail(&log, 700)
            ));
        }
        let text = std::fs::read_to_string(&out_path)
            .with_context(|| format!("读取 faster-whisper 结果失败：{}", out_path.display()))?;
        let parsed: FwOutput =
            serde_json::from_str(&text).map_err(|err| anyhow!("ASR JSON 解析失败：{err}"))?;
        let segments: Vec<Segment> = parsed
            .segments
            .into_iter()
            .filter(|segment| !segment.text.trim().is_empty())
            .map(|segment| Segment {
                start_s: segment.start.max(0.0),
                end_s: segment.end.max(segment.start),
                text: segment.text.trim().to_string(),
            })
            .collect();
        if segments.is_empty() {
            return Err(anyhow!("faster-whisper 没有转写出任何句子（可能是纯音乐或静音）"));
        }
        crate::debug_log(&format!(
            "fish asr fw device={} compute={} load_s={:.2} decode_s={:.2} wall_s={:.2} segments={} lang={} p={:.2} audio_s={:.1}",
            self.device,
            self.compute,
            parsed.load_s,
            parsed.wall_s,
            started.elapsed().as_secs_f64(),
            segments.len(),
            parsed.language,
            parsed.language_probability,
            parsed.duration
        ));
        Ok(Transcript {
            segments,
            language: parsed.language,
            language_probability: parsed.language_probability,
            audio_seconds: parsed.duration,
            load_ms: (parsed.load_s * 1_000.0) as u64,
            decode_ms: (parsed.wall_s * 1_000.0) as u64,
            backend: "faster-whisper".to_string(),
        })
    }
}

fn tail(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.len() <= max {
        return chars.into_iter().collect();
    }
    chars[chars.len() - max..].iter().collect()
}

/// Locate a Python interpreter that can import `faster_whisper`. Cached: the
/// probe costs ~0.4 s and the answer cannot change mid-run.
pub fn find_python() -> Option<String> {
    static PYTHON: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PYTHON
        .get_or_init(|| {
            let candidates: Vec<String> = std::env::var("O11A1_PYTHON")
                .ok()
                .into_iter()
                .chain(["python".to_string(), "python3".to_string()])
                .collect();
            for candidate in candidates {
                let output = proc::command(&candidate)
                    .args(["-c", "import faster_whisper; print('ok')"])
                    .output();
                if let Ok(output) = output {
                    if output.status.success() {
                        return Some(candidate);
                    }
                }
            }
            None
        })
        .clone()
}

/// `cuda` is only reported when CTranslate2 can see a device.
pub fn cuda_device_count(python: &str) -> u32 {
    let output = proc::command(python)
        .args([
            "-c",
            "import ctranslate2; print(ctranslate2.get_cuda_device_count())",
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_script_sets_every_argument_it_is_given() {
        for flag in ["--model", "--audio", "--device", "--compute", "--threads", "--out"] {
            assert!(FW_SCRIPT.contains(flag), "missing {flag}");
        }
        assert!(FW_SCRIPT.contains("faster_whisper"));
        assert!(FW_SCRIPT.contains("json.dump"));
    }

    #[test]
    fn tail_keeps_the_end_of_a_long_error() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("abc", 10), "abc");
    }
}
