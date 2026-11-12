//! One-line logging, no `log` facade and no Slint.
//!
//! Every line starts with `player ` so a QA run can grep the exact events the
//! brief names (`player first-frame`, `player audio started`, `player fps`,
//! `player av-drift`, `player seek`, `enhance fallback`).

use std::sync::OnceLock;
use std::time::Instant;

fn started() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Log one event. The elapsed stamp trails the message so the prefix stays
/// stable and greppable.
pub fn line(text: &str) {
    let elapsed = started().elapsed().as_secs_f64();
    eprintln!("player {text} t={elapsed:.3}s");
}
