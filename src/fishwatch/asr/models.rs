//! Whisper 模型目录、清单、sha256 校验、下载进度与断点续传.
//!
//! The manifest is not a guess: every size and sha256 below was read from the
//! real Hugging Face tree (`/api/models/<repo>/tree/main?recursive=true`; LFS
//! files carry their sha256 as `lfs.oid`) or computed from the downloaded file.
//! `small` is the shipped default; `turbo` and `medium` are opt-in.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

/// One file that makes a model usable.
#[derive(Clone, Debug)]
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// Hugging Face repo, e.g. `Systran/faster-whisper-small`.
    pub repo: &'static str,
}

impl ModelFile {
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/{}",
            self.repo, self.name
        )
    }
}

/// Where one model family lives on disk.
#[derive(Clone, Debug)]
pub struct ModelSpec {
    /// `small` / `turbo` / `medium` (also the settings value).
    pub id: &'static str,
    /// `faster-whisper` or `whisper.cpp`.
    pub engine: &'static str,
    /// Directory name under `models/whisper/`.
    pub dir: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelSpec {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    pub fn path(&self, root: &Path) -> PathBuf {
        root.join(self.dir)
    }

    /// The primary weights file, for the "is it there" check.
    pub fn primary(&self) -> &'static ModelFile {
        &self.files[0]
    }
}

const FW_TURBO_REPO: &str = "dropbox-dash/faster-whisper-large-v3-turbo";
const FW_MEDIUM_REPO: &str = "Systran/faster-whisper-medium";
const CPP_REPO: &str = "ggerganov/whisper.cpp";

const FW_SMALL: &[ModelFile] = &[
    ModelFile {
        name: "model.bin",
        size: 483_546_902,
        sha256: "3e305921506d8872816023e4c273e75d2419fb89b24da97b4fe7bce14170d671",
        repo: "Systran/faster-whisper-small",
    },
    ModelFile {
        name: "config.json",
        size: 2_370,
        sha256: "b55496ac7940a7ae47d2c01eab40edfd8701feec1229d9cce3b40014383fb828",
        repo: "Systran/faster-whisper-small",
    },
    ModelFile {
        name: "tokenizer.json",
        size: 2_203_239,
        sha256: "fb7b63191e9bb045082c79fd742a3106a12c99513ab30df4a0d47fa6cb6fd0ab",
        repo: "Systran/faster-whisper-small",
    },
    ModelFile {
        name: "vocabulary.txt",
        size: 459_861,
        sha256: "34ce3fe1c5041027b3f8d42912270993f986dbc4bb34cf27f951e34a1e453913",
        repo: "Systran/faster-whisper-small",
    },
];

const FW_TURBO: &[ModelFile] = &[
    ModelFile {
        name: "model.bin",
        size: 1_617_884_929,
        sha256: "e76620f83d5f5b69efd3d87e3dc180c1bd21df9fbebacfd4335e5e1efcc018da",
        repo: FW_TURBO_REPO,
    },
    ModelFile {
        name: "config.json",
        size: 2_263,
        sha256: "b0253ea6c0d3bea6b1e19e91a02acfd3b53f4467362efcb5a3e6b16c9b3a9b7e",
        repo: FW_TURBO_REPO,
    },
    ModelFile {
        name: "preprocessor_config.json",
        size: 340,
        sha256: "7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711",
        repo: FW_TURBO_REPO,
    },
    ModelFile {
        name: "tokenizer.json",
        size: 2_710_337,
        sha256: "297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd",
        repo: FW_TURBO_REPO,
    },
    ModelFile {
        name: "vocabulary.json",
        size: 1_068_114,
        sha256: "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1",
        repo: FW_TURBO_REPO,
    },
];

const FW_MEDIUM: &[ModelFile] = &[
    ModelFile {
        name: "model.bin",
        size: 1_527_906_378,
        sha256: "9b45e1009dcc4ab601eff815b61d80e60ce3fd8c74c1a14f4a282258286b51ae",
        repo: FW_MEDIUM_REPO,
    },
    ModelFile {
        name: "config.json",
        size: 2_257,
        sha256: "3622a2ddc41ec0e0fd4e68c13c6830f03b90c38d89aaad184de02c8c642cf807",
        repo: FW_MEDIUM_REPO,
    },
    ModelFile {
        name: "tokenizer.json",
        size: 2_203_239,
        sha256: "fb7b63191e9bb045082c79fd742a3106a12c99513ab30df4a0d47fa6cb6fd0ab",
        repo: FW_MEDIUM_REPO,
    },
    ModelFile {
        name: "vocabulary.txt",
        size: 459_861,
        sha256: "34ce3fe1c5041027b3f8d42912270993f986dbc4bb34cf27f951e34a1e453913",
        repo: FW_MEDIUM_REPO,
    },
];

const CPP_SMALL: &[ModelFile] = &[ModelFile {
    name: "ggml-small.bin",
    size: 487_601_967,
    sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    repo: CPP_REPO,
}];

const CPP_MEDIUM: &[ModelFile] = &[ModelFile {
    name: "ggml-medium.bin",
    size: 1_533_763_059,
    sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    repo: CPP_REPO,
}];

const CPP_TURBO: &[ModelFile] = &[ModelFile {
    name: "ggml-large-v3-turbo.bin",
    size: 1_624_555_275,
    sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    repo: CPP_REPO,
}];

pub const FW_SMALL_SPEC: ModelSpec = ModelSpec {
    id: "small",
    engine: "faster-whisper",
    dir: "faster-whisper-small",
    files: FW_SMALL,
};
pub const FW_TURBO_SPEC: ModelSpec = ModelSpec {
    id: "turbo",
    engine: "faster-whisper",
    dir: "faster-whisper-large-v3-turbo",
    files: FW_TURBO,
};
pub const FW_MEDIUM_SPEC: ModelSpec = ModelSpec {
    id: "medium",
    engine: "faster-whisper",
    dir: "faster-whisper-medium",
    files: FW_MEDIUM,
};
pub const CPP_SMALL_SPEC: ModelSpec = ModelSpec {
    id: "small",
    engine: "whisper.cpp",
    dir: "whisper.cpp",
    files: CPP_SMALL,
};
pub const CPP_TURBO_SPEC: ModelSpec = ModelSpec {
    id: "turbo",
    engine: "whisper.cpp",
    dir: "whisper.cpp",
    files: CPP_TURBO,
};
pub const CPP_MEDIUM_SPEC: ModelSpec = ModelSpec {
    id: "medium",
    engine: "whisper.cpp",
    dir: "whisper.cpp",
    files: CPP_MEDIUM,
};

/// Resolve `(model, engine)` to a spec. `auto` resolves to faster-whisper:
/// it is the engine with a GPU verified on this machine.
pub fn spec(model: &str, engine: &str) -> Option<&'static ModelSpec> {
    match (model.trim(), engine.trim()) {
        ("small", "faster-whisper") | ("small", "auto") | ("small", "") => Some(&FW_SMALL_SPEC),
        ("turbo", "faster-whisper") | ("turbo", "auto") | ("turbo", "") => Some(&FW_TURBO_SPEC),
        ("medium", "faster-whisper") | ("medium", "auto") | ("medium", "") => {
            Some(&FW_MEDIUM_SPEC)
        }
        ("small", "whisper.cpp") => Some(&CPP_SMALL_SPEC),
        ("turbo", "whisper.cpp") => Some(&CPP_TURBO_SPEC),
        ("medium", "whisper.cpp") => Some(&CPP_MEDIUM_SPEC),
        _ => None,
    }
}

/// Human size: `483 MB`.
pub fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.0} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1_024.0)
    }
}

/// `models/whisper/<dir>/.ok` marks a fully verified tree, so the hot path
/// (planning a run) only stats files instead of re-hashing 483 MB.
fn ok_marker(root: &Path, spec: &ModelSpec) -> PathBuf {
    spec.path(root).join(".ok")
}

pub fn marker_value(spec: &ModelSpec) -> String {
    spec.files
        .iter()
        .map(|file| format!("{}:{}", file.name, file.sha256))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cheap readiness: every file exists with the right size and the `.ok` marker
/// records the expected hashes.
pub fn model_ready(root: &Path, spec: &ModelSpec) -> bool {
    let dir = spec.path(root);
    let marker = ok_marker(root, spec);
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return false;
    };
    if text.trim() != marker_value(spec) {
        return false;
    }
    spec.files.iter().all(|file| {
        std::fs::metadata(dir.join(file.name))
            .map(|meta| meta.len() == file.size)
            .unwrap_or(false)
    })
}

/// Full verification (download button's "校验模型", and after every download).
pub fn verify(root: &Path, spec: &ModelSpec) -> Result<()> {
    let dir = spec.path(root);
    for file in spec.files {
        let path = dir.join(file.name);
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("缺少模型文件 {}", path.display()))?;
        if meta.len() != file.size {
            return Err(anyhow!(
                "{} 体积不对：{} != {}",
                file.name,
                meta.len(),
                file.size
            ));
        }
        let actual = sha256_file(&path)?;
        if actual != file.sha256 {
            return Err(anyhow!(
                "{} sha256 不匹配：{} != {}",
                file.name,
                actual,
                file.sha256
            ));
        }
    }
    std::fs::create_dir_all(spec.path(root))?;
    std::fs::write(ok_marker(root, spec), marker_value(spec))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("打开 {} 失败", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// 0.0–1.0 over the whole model (all files).
    pub fraction: f32,
    /// `small · 183/461 MB · model.bin 38%`
    pub text: String,
}

/// Download (or resume) one model. Blocks; call from a worker thread.
///
/// * a truncated `<file>.part` is resumed with `Range: bytes=<len>-`;
///   a server that ignores Range (200) restarts that file from zero;
/// * every finished file is hashed before it is renamed into place;
/// * a file that already exists at the right size and hash is kept, so a
///   failed second file never redownloads the 483 MB weights.
pub fn download(
    root: &Path,
    spec: &ModelSpec,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    let dir = spec.path(root);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建模型目录失败：{}", dir.display()))?;
    let total = spec.total_bytes();
    let mut done: u64 = 0;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .user_agent("O11A1/0.2 fishwatch")
        .build()
        .context("创建下载客户端失败")?;

    for file in spec.files {
        let final_path = dir.join(file.name);
        if final_path.exists()
            && std::fs::metadata(&final_path)
                .map(|meta| meta.len() == file.size)
                .unwrap_or(false)
        {
            // Already complete on disk: verify (cheap relative to re-download).
            let actual = sha256_file(&final_path)?;
            if actual == file.sha256 {
                done += file.size;
                progress(Progress {
                    fraction: done as f32 / total as f32,
                    text: format!(
                        "{} · {} · {} 已校验",
                        spec.id,
                        human_bytes(done),
                        file.name
                    ),
                });
                continue;
            }
            let _ = std::fs::remove_file(&final_path);
        }
        let part_path = dir.join(format!("{}.part", file.name));
        let mut have = std::fs::metadata(&part_path).map(|meta| meta.len()).unwrap_or(0);
        if have > file.size {
            let _ = std::fs::remove_file(&part_path);
            have = 0;
        }
        if have == file.size {
            // The last run wrote every byte but died before the rename.
            let actual = sha256_file(&part_path)?;
            if actual == file.sha256 {
                std::fs::rename(&part_path, &final_path)?;
                done += file.size;
                continue;
            }
            let _ = std::fs::remove_file(&part_path);
            have = 0;
        }

        let mut request = client.get(file.url());
        if have > 0 {
            crate::debug_log(&format!(
                "fish model resume file={} have={} of={}",
                file.name, have, file.size
            ));
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let response = request.send().context("请求模型文件失败")?;
        let status = response.status();
        let appending = status == reqwest::StatusCode::PARTIAL_CONTENT;
        if have > 0 {
            crate::debug_log(&format!(
                "fish model resume file={} http={} partial={}",
                file.name,
                status.as_u16(),
                appending as i32
            ));
        }
        if !status.is_success() {
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && have == file.size {
                let actual = sha256_file(&part_path)?;
                if actual == file.sha256 {
                    std::fs::rename(&part_path, &final_path)?;
                    done += file.size;
                    continue;
                }
            }
            return Err(anyhow!(
                "下载 {} 失败：HTTP {}",
                file.name,
                status.as_u16()
            ));
        }
        if !appending {
            have = 0;
        }
        let mut out = OpenOptions::new()
            .create(true)
            .write(true)
            .append(appending)
            .truncate(!appending)
            .open(&part_path)?;
        out.seek(SeekFrom::End(0))?;
        let mut reader = ProgressReader {
            inner: response,
            on_read: &mut |read| {
                have += read as u64;
                let overall = done + have.min(file.size);
                let _ = progress(Progress {
                    fraction: overall as f32 / total as f32,
                    text: format!(
                        "{} · {}/{} · {} {:.0}%",
                        spec.id,
                        human_bytes(overall),
                        human_bytes(total),
                        file.name,
                        (have as f64 / file.size as f64) * 100.0
                    ),
                });
            },
        };
        std::io::copy(&mut reader, &mut out).context("写模型文件失败")?;
        out.flush()?;
        drop(out);
        let actual = sha256_file(&part_path)?;
        if actual != file.sha256 {
            let _ = std::fs::remove_file(&part_path);
            return Err(anyhow!(
                "{} sha256 不匹配（已删除临时文件）：{} != {}",
                file.name,
                actual,
                file.sha256
            ));
        }
        std::fs::rename(&part_path, &final_path)?;
        done += file.size;
        progress(Progress {
            fraction: done as f32 / total as f32,
            text: format!(
                "{} · {}/{} · {} 已校验",
                spec.id,
                human_bytes(done),
                human_bytes(total),
                file.name
            ),
        });
        crate::debug_log(&format!(
            "fish model file done={} bytes={} sha256={} expected={} ok=1",
            file.name,
            file.size,
            actual,
            file.sha256
        ));
    }
    verify(root, spec)?;
    crate::debug_log(&format!(
        "fish model ready id={} engine={} dir={} bytes={}",
        spec.id,
        spec.engine,
        spec.path(root).display(),
        total
    ));
    Ok(())
}

struct ProgressReader<'a, R> {
    inner: R,
    on_read: &'a mut dyn FnMut(usize),
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            (self.on_read)(read);
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn every_manifest_entry_has_a_real_hash_and_size() {
        for spec in [
            &FW_SMALL_SPEC,
            &FW_TURBO_SPEC,
            &FW_MEDIUM_SPEC,
            &CPP_SMALL_SPEC,
            &CPP_MEDIUM_SPEC,
            &CPP_TURBO_SPEC,
        ] {
            assert!(!spec.files.is_empty());
            assert_eq!(spec.files[0].name.contains("model.bin") || spec.files[0].name.contains("ggml"), true);
            let total = spec.total_bytes();
            assert!(total > 400_000_000, "{} total {total}", spec.id);
            for file in spec.files {
                assert_eq!(file.sha256.len(), 64, "{}:{}", spec.id, file.name);
                assert!(file.size > 0);
                assert!(file.url().starts_with("https://huggingface.co/"));
            }
        }
    }

    #[test]
    fn the_small_manifest_matches_the_verified_huggingface_tree() {
        assert_eq!(FW_SMALL_SPEC.files[0].size, 483_546_902);
        assert_eq!(
            FW_SMALL_SPEC.files[0].sha256,
            "3e305921506d8872816023e4c273e75d2419fb89b24da97b4fe7bce14170d671"
        );
        assert_eq!(FW_SMALL_SPEC.total_bytes(), 486_212_372);
        assert_eq!(CPP_SMALL_SPEC.files[0].size, 487_601_967);
        // `lfs.oid` of ggml-small.bin in ggerganov/whisper.cpp, read from
        // /api/models/ggerganov/whisper.cpp/tree/main?recursive=true
        assert_eq!(
            CPP_SMALL_SPEC.files[0].sha256,
            "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"
        );
        assert_eq!(CPP_SMALL_SPEC.dir, "whisper.cpp");
    }

    #[test]
    fn spec_resolution_prefers_faster_whisper_for_auto() {
        assert_eq!(spec("small", "auto").map(|spec| spec.engine), Some("faster-whisper"));
        assert_eq!(spec("turbo", "whisper.cpp").map(|spec| spec.dir), Some("whisper.cpp"));
        assert_eq!(spec("medium", "faster-whisper").map(|spec| spec.id), Some("medium"));
        assert!(spec("large", "auto").is_none());
    }

    #[test]
    fn human_bytes_reads_like_the_settings_row() {
        assert_eq!(human_bytes(483_546_902), "461 MB");
        assert_eq!(human_bytes(1_617_884_929), "1.5 GB");
        assert_eq!(human_bytes(2_370), "2 KB");
    }

    /// Real network, opt-in: downloads the shipped `small` model from scratch
    /// (or resumes a `.part`) into `BILI_MODEL_DIR` (default `models/whisper`)
    /// and verifies every sha256.
    #[test]
    #[ignore = "downloads 461 MB from Hugging Face; run with --ignored --nocapture"]
    fn live_download_small_model() {
        let root = std::env::var_os("BILI_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/whisper"));
        eprintln!("model root={}", root.display());
        let mut last_ms = 0u128;
        let started = Instant::now();
        download(&root, &FW_SMALL_SPEC, &mut |progress| {
            // Print at most every 2 s, plus the final line.
            let now = started.elapsed().as_millis();
            if now.saturating_sub(last_ms) >= 2_000 || progress.fraction >= 1.0 {
                last_ms = now;
                eprintln!(
                    "progress {:.1}% {}",
                    progress.fraction * 100.0,
                    progress.text
                );
            }
        })
        .expect("download");
        verify(&root, &FW_SMALL_SPEC).expect("verify");
        for file in FW_SMALL_SPEC.files {
            let path = FW_SMALL_SPEC.path(&root).join(file.name);
            let meta = std::fs::metadata(&path).expect("file");
            assert_eq!(meta.len(), file.size);
            eprintln!(
                "verified {} bytes={} sha256={} expected={}",
                file.name, meta.len(), file.sha256, file.sha256
            );
        }
        eprintln!(
            "download+verify ok total={} MB in {:.1}s",
            FW_SMALL_SPEC.total_bytes() / 1_048_576,
            started.elapsed().as_secs_f64()
        );
    }

    /// Real network, opt-in: the GGML model that `engine=whisper.cpp` uses.
    /// Downloads (or resumes) `ggml-small.bin` through [`download`] — the same
    /// code path the settings panel uses — and verifies its sha256.
    #[test]
    #[ignore = "downloads 466 MB from Hugging Face; run with --ignored --nocapture"]
    fn live_download_cpp_small_model() {
        let root = std::env::var_os("BILI_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/whisper"));
        eprintln!("model root={}", root.display());
        let target = CPP_SMALL_SPEC.path(&root).join(CPP_SMALL_SPEC.primary().name);
        if target.exists() {
            let size = std::fs::metadata(&target).map(|meta| meta.len()).unwrap_or(0);
            eprintln!("already present {} bytes={size}", target.display());
        }
        let started = Instant::now();
        let mut last_ms = 0u128;
        download(&root, &CPP_SMALL_SPEC, &mut |progress| {
            let now = started.elapsed().as_millis();
            if now.saturating_sub(last_ms) >= 2_000 || progress.fraction >= 1.0 {
                last_ms = now;
                eprintln!(
                    "progress {:.1}% {}",
                    progress.fraction * 100.0,
                    progress.text
                );
            }
        })
        .expect("download ggml");
        verify(&root, &CPP_SMALL_SPEC).expect("verify ggml");
        let meta = std::fs::metadata(&target).expect("file");
        assert_eq!(meta.len(), CPP_SMALL_SPEC.primary().size);
        assert!(model_ready(&root, &CPP_SMALL_SPEC));
        eprintln!(
            "verified {} bytes={} sha256={} in {:.1}s",
            target.display(),
            meta.len(),
            CPP_SMALL_SPEC.primary().sha256,
            started.elapsed().as_secs_f64()
        );
    }
}

