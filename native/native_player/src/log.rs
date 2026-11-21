//! One-line logging, no `log` facade and no Slint.
//!
//! Every line starts with `player ` so a QA run can grep the exact events the
//! brief names (`player first-frame`, `player audio started`, `player fps`,
//! `player av-drift`, `player seek`, `enhance fallback`).
//!
//! Packaged builders start the engine without a console, so stderr is drained
//! and discarded. Keep the same lines in `O11A1_HOME/logs/player.log`; a
//! playback stall that only reproduces after a long session is otherwise
//! impossible to diagnose after the fact.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

fn started() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn sink() -> Option<&'static Mutex<File>> {
    static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let dir = o11a1::paths::root().join("logs");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("player.log");
        // Keep two 8 MB generations. The fps line alone is ~120 bytes/s, so a
        // normal week of playback stays inside one or two files.
        if std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) > 8 * 1024 * 1024 {
            let old = dir.join("player.log.1");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

/// Log one event. The elapsed stamp trails the message so the prefix stays
/// stable and greppable.
pub fn line(text: &str) {
    let elapsed = started().elapsed().as_secs_f64();
    eprintln!("player {text} t={elapsed:.3}s");
    if let Some(sink) = sink() {
        if let Ok(mut file) = sink.lock() {
            let _ = writeln!(file, "player {text} t={elapsed:.3}s");
        }
    }
}
