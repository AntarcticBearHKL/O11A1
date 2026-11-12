//! No-network smoke test for the 养鱼 engine as a Slint-free core module.
//!
//! The engine is opened at an explicit scratch path (`open_at`), never through
//! [`crate::fish::FishEngine::open`], so the owner's real `fish.db` and
//! `settings.json` are untouched. Everything exercised here is SQLite-only:
//! schema creation, queueing, snapshot, dashboard, verdicts and stats.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::fish::{FeedItem, FishEngine, FishState};

fn temp_db() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "o11a1-core-fish-smoke-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("fish.db")
}

fn feed_item(bvid: &str) -> FeedItem {
    FeedItem {
        bvid: bvid.to_string(),
        cid: 101,
        aid: 202,
        title: "smoke".to_string(),
        pic: String::new(),
        owner: "up".to_string(),
        owner_mid: 1,
        view: 2,
        duration: 3,
        pub_ts: 4,
    }
}

#[test]
fn engine_opens_writes_and_reads_without_slint_or_network() {
    let path = temp_db();
    let engine = FishEngine::open_at(&path).expect("open scratch fish.db");
    assert_eq!(engine.db_path(), path.as_path());

    // Fresh database: empty queue/history, and no DeepSeek key in a scratch
    // home means NeedsKey (Idle if a key happens to exist in the environment).
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.queued, 0);
    assert_eq!(snapshot.done, 0);
    assert_eq!(snapshot.errors, 0);
    assert!(matches!(
        snapshot.state,
        FishState::NeedsKey | FishState::Idle
    ));
    assert!(engine.verdicts(10).expect("verdicts").is_empty());
    assert_eq!(engine.dashboard().expect("dashboard").today.watched, 0);
    assert_eq!(engine.stats(30).expect("stats").watched, 0);

    // Queueing and clearing are pure SQLite paths (no worker, no network).
    engine.enqueue(&[feed_item("BV1SMOKE")]);
    assert_eq!(engine.snapshot().queued, 1);
    let cleared = engine.clear_queue().expect("clear queue");
    assert_eq!(cleared.cleared, 1);
    assert_eq!(cleared.verdicts_kept, 0);
    assert_eq!(engine.snapshot().queued, 0);
    assert!(engine.cleared_at() > 0);

    // The Settings -> WatchConfig hand-off the UI needs for the ASR panel.
    let watch = crate::fish::watch_config(&crate::settings::load());
    assert!(watch.selection.meta);

    drop(engine);
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}
