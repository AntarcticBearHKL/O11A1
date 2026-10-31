//! f212: the repeatable cycle behind the two entries that lost zero-copy in
//! f211 — "搜索入口进播放器" and "同进程关掉再开".
//!
//! ```text
//! BILI_ENTRY_CYCLE=<kind>[,<rounds>][,<hold_ms>]
//! ```
//!
//! `kind` is `search` (a real 搜索, then the first result's card opens the
//! player — the entry that failed) or `reopen` (the 推荐流's card, so the only
//! thing changing between rounds is the previous window being closed). Each
//! round opens through the UI's own callback, holds, and closes through the ✕
//! path, so the decode decision the run exercises is the one a user's click
//! makes; the verdicts themselves are read from the player's own log lines
//! (`decode=mf` / `decode fallback reason=…`).
//!
//! Nothing here runs unless the variable is set.

use crate::qa::env;
use crate::*;

/// `BILI_ENTRY_CYCLE`: arm the cycle once (same shape as the other cover hooks).
pub(crate) fn maybe_start_entry_cycle(weak: &Weak<AppWindow>) {
    static ARMED: AtomicBool = AtomicBool::new(false);
    if ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(raw) = env::var("BILI_ENTRY_CYCLE") else {
        return;
    };
    let mut parts = raw.split(',').map(str::trim);
    let kind = parts.next().unwrap_or_default().to_ascii_lowercase();
    if kind != "search" && kind != "reopen" {
        crate::debug_log(&format!("f212 cycle ignored value={raw}"));
        return;
    }
    let rounds = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(10)
        .clamp(1, 40);
    let hold_ms = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(4_000)
        .clamp(800, 60_000);
    // `[,<stall_ms>]`: hold the UI thread inside a callback for this long on the
    // first round only, i.e. exactly when the session asks for its video layer.
    // That is the f211 failure reproduced on purpose (the 搜索 entry does the
    // same thing by publishing a feed page), and the point of the run is that
    // the decoder decision survives it.
    let stall_ms = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .min(30_000);
    let weak = weak.clone();
    std::thread::spawn(move || run(weak, kind, rounds, hold_ms, stall_ms));
}

/// Wait for the player registry to reach `want` (opens and closes are UI-side).
fn wait_players(want: impl Fn(usize) -> bool) -> usize {
    let mut count = PLAYER_OPEN_COUNT.load(Ordering::SeqCst);
    for _ in 0..400 {
        count = PLAYER_OPEN_COUNT.load(Ordering::SeqCst);
        if want(count) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    count
}

fn run(weak: Weak<AppWindow>, kind: String, rounds: u32, hold_ms: u64, stall_ms: u64) {
    // The first round must not race the startup screen.
    for _ in 0..240 {
        if on_ui(&weak, |ui| ui.get_feed_items().row_count()).unwrap_or(0) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    std::thread::sleep(Duration::from_millis(1_200));
    crate::debug_log(&format!(
        "f212 cycle start kind={kind} rounds={rounds} hold_ms={hold_ms} stall_ms={stall_ms}"
    ));
    for round in 1..=rounds {
        let started = Instant::now();
        // Never open onto a window that is still closing: that is the user's
        // "关掉再开", not a stack of three players.
        wait_players(|count| count == 0);
        if kind == "search" {
            let _ = on_ui(&weak, |ui| {
                ui.set_search_text("原神".into());
                ui.invoke_do_search("原神".into());
            });
            // The result grid, i.e. the same rows the user clicks. The first
            // search of a run pays for every cover in it, so the budget is the
            // page's, not a round's.
            for _ in 0..600 {
                let rows = on_ui(&weak, |ui| ui.get_search_items().row_count()).unwrap_or(0);
                if rows > 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        let open_at = Instant::now();
        let _ = on_ui(&weak, |ui| ui.invoke_card_open(1, 0));
        if round == 1 && stall_ms > 0 {
            // The UI thread is held *after* the click, while the new session is
            // fetching its stream and asking for the video layer.
            let _ = on_ui(&weak, move |_ui| {
                crate::debug_log(&format!("f212 cycle ui stall begin ms={stall_ms}"));
                std::thread::sleep(Duration::from_millis(stall_ms));
                crate::debug_log("f212 cycle ui stall end");
            });
        }
        let opened = wait_players(|count| count > 0);
        let open_ms = open_at.elapsed().as_millis();
        std::thread::sleep(Duration::from_millis(hold_ms));
        let close_at = Instant::now();
        let invoked = on_ui(&weak, |_ui| f147_close_players()).unwrap_or(0);
        let closed = wait_players(|count| count == 0);
        crate::debug_log(&format!(
            "f212 cycle i={round} kind={kind} opened={opened} open_ms={open_ms} invoked={invoked} closed={closed} close_ms={} total_ms={}",
            close_at.elapsed().as_millis(),
            started.elapsed().as_millis()
        ));
        if kind == "search" {
            let _ = on_ui(&weak, |ui| ui.invoke_back_to_feed());
            std::thread::sleep(Duration::from_millis(600));
        }
        // The beat a user takes between two opens is not zero either; keeping
        // it small is the point of the test.
        std::thread::sleep(Duration::from_millis(250));
    }
    crate::debug_log(&format!("f212 cycle done rounds={rounds}"));
    if env::flag("BILI_F212_HOLD") {
        return;
    }
    std::thread::sleep(Duration::from_millis(800));
    request_quit("f212 entry cycle done");
}
