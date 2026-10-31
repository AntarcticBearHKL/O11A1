//! Where the app keeps its files.
//!
//! A development run keeps everything at the repository root, which is what
//! `CARGO_MANIFEST_DIR` points at and what every path in this crate used to
//! assume. A **packaged** run is a folder the user unzipped somewhere else, so
//! that path does not exist on their machine - everything has to live beside the
//! executable instead.
//!
//! The rule is therefore: if the running executable sits under a `target`
//! directory (cargo build/test/run), use the manifest directory; otherwise the
//! executable's own folder is the home. `O11A1_HOME` overrides both, which is
//! what the QA scripts use to keep test runs out of the real settings.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Repository-or-distribution root: where settings, caches and models live.
pub fn root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(resolve_root).clone()
}

fn resolve_root() -> PathBuf {
    if let Ok(dir) = std::env::var("O11A1_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return manifest,
    };

    // `target\release\o11a1.exe`, `target\debug\deps\...` - a dev build.
    let under_target = exe.components().any(|part| {
        part.as_os_str().eq_ignore_ascii_case("target")
    });
    if under_target && manifest.is_dir() {
        return manifest;
    }

    exe.parent().map(Path::to_path_buf).unwrap_or(manifest)
}

/// Folder holding `ffmpeg.exe` / `ffprobe.exe` / `ffplay.exe`.
///
/// `BILI_FFMPEG_DIR` wins when set (that is how the QA scripts point at a debug
/// build). A packaged folder is next: `bin\ffmpeg.exe` beside the executable.
/// Then `bin\` / `tools\ffmpeg\bin\` at the app root, and only if none of those
/// exist the developer machine's unpacked build.
pub fn ffmpeg_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BILI_FFMPEG_DIR") {
        return PathBuf::from(dir);
    }
    let root = root();
    for candidate in [
        root.join("bin"),
        root.join("tools").join("ffmpeg").join("bin"),
        root.join("tools").join("ffmpeg"),
    ] {
        if candidate.join("ffmpeg.exe").is_file() {
            return candidate;
        }
    }
    PathBuf::from(r"C:\Programs\ffmpeg-unpack\bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_dir_honours_the_environment_override() {
        // A packaged folder must never be able to override an explicit path.
        std::env::set_var("BILI_FFMPEG_DIR", r"D:\somewhere\bin");
        assert_eq!(ffmpeg_dir(), PathBuf::from(r"D:\somewhere\bin"));
        std::env::remove_var("BILI_FFMPEG_DIR");
    }

    #[test]
    fn root_is_the_repository_while_running_tests() {
        // Tests run out of `target\...`, so the manifest directory wins and the
        // shipped data files are the ones the suite has always used.
        assert_eq!(root(), PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    }
}
