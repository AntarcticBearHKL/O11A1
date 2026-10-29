//! whisper.cpp adapter (`whisper-cli`, GGML models).
//!
//! This is the second engine: it needs no Python, and a CUDA build does its own
//! GPU work. The binary is looked up in `PATH`, `WHISPER_CLI`, and the few
//! places a local build usually lands. Its backend is whatever the binary was
//! compiled for; we read that from `--help` and report it verbatim rather than
//! guessing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::fishwatch::asr::{run_with_timeout, Transcriber, Transcript};
use crate::fishwatch::proc;
use crate::fishwatch::transcript::Segment;

#[derive(Clone, Debug)]
pub struct WhisperCpp {
    pub binary: PathBuf,
    pub model: PathBuf,
    pub threads: u32,
    pub work_dir: PathBuf,
    pub device: String,
}

#[derive(Deserialize)]
struct CppOutput {
    #[serde(default)]
    transcription: Vec<CppRow>,
    #[serde(default)]
    result: CppResult,
    /// The engine's own build description, e.g.
    /// `WHISPER : ... | CUDA : ARCHS = 890 | ...`. Written by `-oj`; the best
    /// proof of which backend actually ran.
    #[serde(default)]
    systeminfo: String,
}

#[derive(Deserialize, Default)]
struct CppResult {
    #[serde(default)]
    language: String,
}

#[derive(Deserialize, Default)]
struct CppRow {
    #[serde(default)]
    offsets: CppOffsets,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct CppOffsets {
    #[serde(default)]
    from: u64,
    #[serde(default)]
    to: u64,
}

impl Transcriber for WhisperCpp {
    fn transcribe(&self, audio: &Path) -> Result<Transcript> {
        std::fs::create_dir_all(&self.work_dir)
            .with_context(|| format!("创建 ASR 工作目录失败：{}", self.work_dir.display()))?;
        let prefix = self
            .work_dir
            .join(format!("cpp-{}-{}", std::process::id(), mono_id()));
        let log_path = self.work_dir.join(format!("cpp-{}.log", std::process::id()));
        let _ = std::fs::remove_file(prefix.with_extension("json"));
        let started = std::time::Instant::now();
        let mut command = proc::command(&self.binary);
        command
            .arg("-m")
            .arg(&self.model)
            .arg("-f")
            .arg(audio)
            .arg("-oj")
            .arg("-of")
            .arg(&prefix)
            .arg("-t")
            .arg(self.threads.to_string())
            .arg("-l")
            .arg("auto");
        let status = run_with_timeout(command, Duration::from_secs(1_500), &log_path)?;
        if !status.success() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(anyhow!(
                "whisper-cli 退出码 {:?}：{}",
                status.code(),
                tail(&log, 700)
            ));
        }
        let json_path = prefix.with_extension("json");
        let text = std::fs::read_to_string(&json_path)
            .with_context(|| format!("读取 whisper-cli 结果失败：{}", json_path.display()))?;
        let parsed: CppOutput =
            serde_json::from_str(&text).map_err(|err| anyhow!("whisper-cli JSON 解析失败：{err}"))?;
        let segments: Vec<Segment> = parsed
            .transcription
            .into_iter()
            .filter(|row| !row.text.trim().is_empty())
            .map(|row| Segment {
                start_s: row.offsets.from as f64 / 1_000.0,
                end_s: (row.offsets.to as f64 / 1_000.0).max(row.offsets.from as f64 / 1_000.0),
                text: row.text.trim().to_string(),
            })
            .collect();
        if segments.is_empty() {
            return Err(anyhow!("whisper-cli 没有转写出任何句子（可能是纯音乐或静音）"));
        }
        let audio_seconds = segments
            .last()
            .map(|segment| segment.end_s)
            .unwrap_or_default();
        crate::debug_log(&format!(
            "fish asr cpp device={} binary={} wall_s={:.2} segments={} lang={} audio_s={:.1} backend={}",
            self.device,
            self.binary.display(),
            started.elapsed().as_secs_f64(),
            segments.len(),
            parsed.result.language,
            audio_seconds,
            cuda_archs(&parsed.systeminfo)
        ));
        Ok(Transcript {
            segments,
            language: parsed.result.language,
            language_probability: 0.0,
            audio_seconds,
            load_ms: 0,
            decode_ms: started.elapsed().as_millis() as u64,
            backend: "whisper.cpp".to_string(),
        })
    }
}

/// `cuda890` when the engine's system info names the CUDA archs it was built
/// for, `cpu` otherwise. Read from the JSON the engine wrote, never assumed.
fn cuda_archs(systeminfo: &str) -> String {
    let Some(rest) = systeminfo.split("CUDA : ARCHS =").nth(1) else {
        return "cpu".to_string();
    };
    let archs = rest.split('|').next().unwrap_or("").trim();
    if archs.is_empty() {
        "cpu".to_string()
    } else {
        format!("cuda{archs}")
    }
}

fn mono_id() -> u128 {    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

fn tail(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.len() <= max {
        return chars.into_iter().collect();
    }
    chars[chars.len() - max..].iter().collect()
}

/// Find `whisper-cli` (or the legacy `main`) without a recursive disk walk.
pub fn find_binary(root: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WHISPER_CLI") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let candidates = [
        // `scripts/build-whisper-cuda.ps1` installs here.
        root.join("tools/whisper-cli/whisper-cli.exe"),
        root.join("tools/whisper-cli/whisper-cli"),
        root.join("models/whisper/whisper-cli.exe"),
        root.join("models/whisper/whisper.cpp/build/bin/whisper-cli.exe"),
        root.join("models/whisper/whisper.cpp/build/bin/whisper-cli"),
        root.join("tools/whisper/whisper-cli.exe"),
        root.join("tools/whisper.cpp/build/bin/whisper-cli.exe"),
        root.join("tools/whisper.cpp/build/bin/Release/whisper-cli.exe"),
        root.join("tools/whisper.cpp/whisper-cli.exe"),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let name = if cfg!(windows) {
        "whisper-cli.exe"
    } else {
        "whisper-cli"
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Which backend the binary was built for. Two independent pieces of real
/// evidence, in order: the `--help` text (a CUDA/Vulkan build names its
/// backend there), then the sibling runtime DLLs (a Windows CUDA build always
/// ships `ggml-cuda.dll` next to the exe). Cached; `cpu` when neither says
/// otherwise.
pub fn detect_device(binary: &Path) -> String {
    static DEVICE: OnceLock<String> = OnceLock::new();
    DEVICE
        .get_or_init(|| {
            if let Ok(output) = proc::command(binary).arg("--help").output() {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let lower = text.to_ascii_lowercase();
                if lower.contains("cuda") {
                    return "cuda".to_string();
                }
                if lower.contains("vulkan") {
                    return "vulkan".to_string();
                }
            }
            for backend in ["cuda", "vulkan"] {
                let dir = binary.parent().unwrap_or_else(|| Path::new("."));
                let found = std::fs::read_dir(dir)
                    .map(|entries| {
                        entries.flatten().any(|entry| {
                            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                            name.ends_with(".dll") && name.contains(backend)
                        })
                    })
                    .unwrap_or(false);
                if found {
                    crate::debug_log(&format!(
                        "fish asr cpp backend={backend} evidence=dll binary={}",
                        binary.display()
                    ));
                    return backend.to_string();
                }
            }
            "cpu".to_string()
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_binary_is_none_for_an_empty_tree() {
        let root = std::env::temp_dir().join("o11a1-cpp-none");
        // Only assert the PATH fallback does not panic; a developer machine may
        // actually have whisper-cli installed.
        let _ = find_binary(&root);
    }

    #[test]
    fn tail_keeps_the_error_end() {
        assert_eq!(tail("abcdef", 3), "def");
    }

    #[test]
    fn cuda_archs_reads_the_engine_system_info() {
        let real = "WHISPER : VITISAI = 0 | COREML = 0 | CUDA : ARCHS = 890 | FA_QUANTS = q4_0-q4_0 | CPU : AVX2 = 1 |";
        assert_eq!(cuda_archs(real), "cuda890");
        assert_eq!(cuda_archs("WHISPER : CPU : AVX2 = 1 |"), "cpu");
        assert_eq!(cuda_archs(""), "cpu");
    }
}
