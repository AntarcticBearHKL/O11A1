//! Slint-free core: Bilibili endpoints, sessions, accounts, settings, paths,
//! danmaku decoding, theme resolution and the 养鱼 engine.
//!
//! This crate is the only remaining home of the shared business source. The
//! native UI and native player link a dependency graph that contains no Slint.
//!
//! ## 养鱼 (fish) engine
//!
//! [`fish::FishEngine`] is the SQLite-backed watch engine; [`fishwatch`] is the
//! per-video step plan (metadata / subtitle / outline / comments / ASR /
//! frames); [`models_dl`] downloads the Whisper models. The engine is opened
//! once with [`fish::FishEngine::open`] (or `open_at` for an explicit path),
//! fed with [`fish::FishEngine::enqueue`], started with
//! [`fish::FishEngine::start`] and parked with [`fish::FishEngine::pause`].
//! Pushed updates arrive through [`fish::FishEngine::set_snapshot_hook`] on the
//! worker thread; the UI can also poll [`fish::FishEngine::snapshot`],
//! [`fish::FishEngine::verdicts`] and [`fish::FishEngine::dashboard`].
//!
//! The `player` module below only supplies the two tracing hooks `fish.rs`
//! calls. It is deliberately a no-op shim, not a player implementation.

use std::io::Write;

pub mod accounts;
pub mod bilibili;
pub mod danmaku;
pub mod fish;
pub mod fishwatch;
pub mod models_dl;
pub mod paths;
pub mod settings;
pub mod theme;
pub mod wbi;

// ---- 养鱼 (fish) -----------------------------------------------------------
// The watch plan (metadata / subtitle / outline / comments / ASR / frames),
// the Whisper model downloader and the SQLite-backed engine. None of these
// touch Slint; the only UI-adjacent call is `player::tag_here` /
// `player::note_fish_step`, which the shim below turns into no-ops.

/// The subset of the legacy player module the fish engine calls: tracing
/// hooks only (`fish:step` spans and the per-step counter). The native player
/// has its own instrumentation, so core keeps them as no-ops rather than
/// pulling Slint or a player dependency in.
pub mod player {
    pub fn tag_here(_: &str) {}
    pub fn note_fish_step(_: bool) {}
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
/// `bilibili::wbi` tests and the legacy account page share this helper.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The one network hook the Bilibili layer calls: request/response accounting.
/// The native UI has no prefetch pool, so this is intentionally a no-op; the
/// legacy app keeps its own real implementation in `src/net.rs`.
pub mod net {
    pub fn note_response_bytes(_bytes: u64) {}
}

/// Debug logging shared by the core modules. `BILI_DEBUG_LOG` is the same file
/// the legacy app writes; without it the line is dropped (the native release
/// has no console).
pub fn debug_log(line: &str) {
    let Some(path) = std::env::var_os("BILI_DEBUG_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod fish_smoke;
