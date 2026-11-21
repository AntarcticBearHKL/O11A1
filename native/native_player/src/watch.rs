//! Per-video watch progress, keyed by `bvid:cid`.
//!
//! Bilibili resumes a video on the last watched position, but the official
//! history endpoint only returns the most recent rows and is a network round
//! trip. The player therefore keeps its own progress file beside
//! `player_state.json` and updates it while a video plays; the history
//! endpoint stays as the fallback for videos this install has never opened.
//!
//! Both paths share the same rule: a position under three seconds is just the
//! opening credits, and a position within five seconds of the end means the
//! video was finished, so the next start is from zero (watching it again).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// File name inside `o11a1::paths::root()`.
pub const FILE_NAME: &str = "watch_progress.json";

/// Do not resume below this position: the video barely started.
pub const MIN_RESUME_MS: u64 = 3_000;

/// Do not resume when this close to the end: the video was finished.
pub const END_GUARD_MS: u64 = 5_000;

/// Bound the file so years of viewing cannot grow it without limit. Oldest
/// entries are dropped first when the cap is exceeded.
const MAX_ENTRIES: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchEntry {
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Unix seconds of the last update.
    pub updated_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WatchFile {
    entries: BTreeMap<String, WatchEntry>,
}

impl WatchFile {
    fn load(path: &Path) -> Self {
        let Ok(data) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, data)?;
        std::fs::rename(&temporary, path)
    }
}

fn key(bvid: &str, cid: u64) -> String {
    format!("{bvid}:{cid}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// True when a stored position is worth resuming from.
pub fn usable_resume(position_ms: u64, duration_ms: u64) -> Option<u64> {
    if position_ms < MIN_RESUME_MS {
        return None;
    }
    if duration_ms > 0 && position_ms.saturating_add(END_GUARD_MS) >= duration_ms {
        return None;
    }
    Some(position_ms)
}

/// Path of the progress file beside `player_state.json`.
pub fn progress_path() -> PathBuf {
    o11a1::paths::root().join(FILE_NAME)
}

/// Last stored position for one part, already filtered by the resume rules.
pub fn lookup(bvid: &str, cid: u64, duration_ms: u64) -> Option<u64> {
    let path = progress_path();
    let _guard = LOCK.lock().ok()?;
    lookup_from(&path, bvid, cid, duration_ms)
}

fn lookup_from(path: &Path, bvid: &str, cid: u64, duration_ms: u64) -> Option<u64> {
    if bvid.trim().is_empty() || cid == 0 {
        return None;
    }
    let file = WatchFile::load(path);
    let entry = file.entries.get(&key(bvid, cid))?;
    let duration = if duration_ms > 0 {
        duration_ms
    } else {
        entry.duration_ms
    };
    usable_resume(entry.position_ms, duration)
}

/// Store the current playback position for one part.
pub fn record(bvid: &str, cid: u64, position_ms: u64, duration_ms: u64) {
    let path = progress_path();
    let Ok(_guard) = LOCK.lock() else {
        return;
    };
    if let Err(error) = record_into(&path, bvid, cid, position_ms, duration_ms) {
        crate::log::line(&format!(
            "watch progress save failed bvid={bvid} cid={cid} err={error:#}"
        ));
    }
}

fn record_into(
    path: &Path,
    bvid: &str,
    cid: u64,
    position_ms: u64,
    duration_ms: u64,
) -> anyhow::Result<()> {
    if bvid.trim().is_empty() || cid == 0 || position_ms == 0 {
        return Ok(());
    }
    let position_ms = if duration_ms > 0 {
        position_ms.min(duration_ms)
    } else {
        position_ms
    };
    let mut file = WatchFile::load(path);
    file.entries.insert(
        key(bvid, cid),
        WatchEntry {
            position_ms,
            duration_ms,
            updated_at: now_secs(),
        },
    );
    if file.entries.len() > MAX_ENTRIES {
        let mut order: Vec<(u64, String)> = file
            .entries
            .iter()
            .map(|(key, entry)| (entry.updated_at, key.clone()))
            .collect();
        order.sort_unstable();
        let drop_count = file.entries.len() - MAX_ENTRIES;
        for (_, key) in order.into_iter().take(drop_count) {
            file.entries.remove(&key);
        }
    }
    file.save(path)?;
    Ok(())
}

static LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_path() -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "o11a1-watch-{}-{id}.json",
            std::process::id()
        ))
    }

    #[test]
    fn resume_thresholds_match_the_official_player() {
        assert_eq!(usable_resume(0, 0), None);
        assert_eq!(usable_resume(2_999, 600_000), None);
        assert_eq!(usable_resume(3_000, 600_000), Some(3_000));
        assert_eq!(usable_resume(594_999, 600_000), Some(594_999));
        assert_eq!(usable_resume(595_000, 600_000), None);
        assert_eq!(usable_resume(599_000, 600_000), None);
        assert_eq!(usable_resume(3_000, 0), Some(3_000));
        assert_eq!(usable_resume(400_000, 0), Some(400_000));
    }

    #[test]
    fn progress_round_trips_per_part() {
        let path = temp_path();
        record_into(&path, "BV1test", 11, 42_500, 600_000).unwrap();
        record_into(&path, "BV1test", 22, 90_000, 700_000).unwrap();
        assert_eq!(lookup_from(&path, "BV1test", 11, 600_000), Some(42_500));
        assert_eq!(lookup_from(&path, "BV1test", 22, 700_000), Some(90_000));
        // A different part of the same video keeps its own position.
        assert_eq!(lookup_from(&path, "BV1test", 33, 600_000), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn finished_video_does_not_resume() {
        let path = temp_path();
        record_into(&path, "BV1done", 7, 598_000, 600_000).unwrap();
        assert_eq!(lookup_from(&path, "BV1done", 7, 600_000), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let path = temp_path();
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(lookup_from(&path, "BV1bad", 1, 600_000), None);
        record_into(&path, "BV1bad", 1, 50_000, 600_000).unwrap();
        assert_eq!(lookup_from(&path, "BV1bad", 1, 600_000), Some(50_000));
        let _ = std::fs::remove_file(path);
    }
}
