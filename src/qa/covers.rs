//! Cover-loading QA (f181): the same process walks the same page several times
//! and reports how many rows actually got a cover, so a cover path that stops
//! working after N visits is visible as `covers=0/N` instead of "the user says
//! it does not load".
//!
//! Two switches, both read from [`super::env`]:
//!
//! * `BILI_SEARCH_LOOP=<N>[,<delay_ms>][,player]` — N real search rounds:
//!   `do_search` → wait for the covers → `back_to_feed` → next round. The
//!   keywords alternate, so each round is a different set of cover URLs (the
//!   cache/LRU path the user's "多进几次" hits). `player` additionally opens a
//!   real player, closes it through the ✕ path and only then searches, which is
//!   the "开播放器 → 关播放器 → 进搜索" regression.
//! * `BILI_COVER_PAGE_TEST=<rounds>` — 推荐流 / 搜索 / 分区 / 番剧 / 直播 / 养鱼,
//!   each entered `rounds` times, reporting the covers every list actually has.
//!
//! Everything here drives the UI's own callbacks (`do_search`, `back_to_feed`,
//! `open_region`, `pgc_tab_pick`, `live_open`, `fish_open`, `card_open`) and
//! reads the models the user sees. No synthetic input, no invented data.

use crate::*;

/// One reading of a cover list on the UI thread.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CoverSnap {
    pub(crate) covers: usize,
    pub(crate) total: usize,
    pub(crate) pages: i64,
    pub(crate) seq: u64,
    pub(crate) players: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) skipped: u64,
    pub(crate) failed: u64,
}

impl CoverSnap {
    fn cache(&self, hits_before: u64, misses_before: u64) -> String {
        format!(
            "{}/{}",
            self.hits.saturating_sub(hits_before),
            self.misses.saturating_sub(misses_before)
        )
    }
}

/// `BILI_SEARCH_LOOP` parsed: rounds, the pause between rounds, and whether the
/// run first opens + closes a real player.
pub(crate) struct SearchLoopSpec {
    pub(crate) rounds: u32,
    pub(crate) gap: Duration,
    pub(crate) with_player: bool,
}

/// Parse `N[,<delay_ms>][,player]`. `None` when the variable is unset or `N` is
/// not a number; a bad delay falls back to [`SEARCH_LOOP_GAP`].
pub(crate) fn search_loop_spec() -> Option<SearchLoopSpec> {
    let raw = env::var("BILI_SEARCH_LOOP")?;
    let mut rounds = 0u32;
    let mut gap = SEARCH_LOOP_GAP;
    let mut with_player = false;
    for (index, token) in raw.split(',').map(str::trim).enumerate() {
        if token.is_empty() {
            continue;
        }
        if token.eq_ignore_ascii_case("player") {
            with_player = true;
            continue;
        }
        match (index, token.parse::<u64>()) {
            (_, Ok(value)) if index == 0 => rounds = value.min(200) as u32,
            (_, Ok(value)) => gap = Duration::from_millis(value.clamp(50, 60_000)),
            (_, Err(_)) if index == 0 => return None,
            _ => {}
        }
    }
    if rounds == 0 {
        return None;
    }
    Some(SearchLoopSpec {
        rounds,
        gap,
        with_player,
    })
}

/// How long a round waits for its covers before it reports what it has.
pub(crate) const SEARCH_LOOP_WAIT: Duration = Duration::from_secs(25);

/// The pause between two rounds (the "back, then search again" beat).
pub(crate) const SEARCH_LOOP_GAP: Duration = Duration::from_millis(800);

/// How long one list in the multi-page walk may take to show its rows.
pub(crate) const PAGE_ROUND_WAIT: Duration = Duration::from_secs(20);

/// The two queries a search round alternates between: two different result
/// sets, i.e. two different batches of cover URLs.
pub(crate) const SEARCH_LOOP_KEYWORDS: [&str; 2] = ["bilibili", "原神"];

/// Arm the f181 hooks once. Called from the startup feed path, which is the
/// first point where the main window's callbacks are wired.
pub(crate) fn maybe_start_cover_hooks(weak: &Weak<AppWindow>) {
    static ARMED: AtomicBool = AtomicBool::new(false);
    if ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(spec) = search_loop_spec() {
        let weak = weak.clone();
        std::thread::spawn(move || run_search_loop(weak, spec));
        return;
    }
    // f212: the entry cycle has its own variable and may be armed instead of
    // (never together with) the search loop above.
    maybe_start_entry_cycle(weak);
    if let Some(rounds) = env::var("BILI_COVER_PAGE_TEST").and_then(|raw| raw.parse::<u32>().ok()) {
        let weak = weak.clone();
        std::thread::spawn(move || run_cover_page_test(weak, rounds.min(20)));
    }
}

/// One UI-thread reading of the search result grid.
pub(crate) fn search_snap(ui: &AppWindow) -> CoverSnap {
    let (covers, total) = search_covers(ui);
    CoverSnap {
        covers,
        total,
        pages: ui.get_search_total_pages() as i64,
        seq: search_seq_now(),
        players: PLAYER_OPEN_COUNT.load(Ordering::SeqCst),
        hits: cover_cache_hits(),
        misses: cover_cache_misses(),
        skipped: cover_skip_player(),
        failed: cover_failures(),
    }
}

pub(crate) fn on_ui<T: Send + 'static>(
    weak: &Weak<AppWindow>,
    f: impl FnOnce(&AppWindow) -> T + Send + 'static,
) -> Option<T> {
    f145_on_ui(weak, f)
}

/// Wait until the main window is on a page with rows, so the first round does
/// not race the startup screen.
fn wait_feed_ready(weak: &Weak<AppWindow>) {
    for _ in 0..240 {
        let rows = on_ui(weak, |ui| ui.get_feed_items().row_count()).unwrap_or(0);
        if rows > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    // The startup routine ends by publishing its own `view_mode = 0`; give the
    // last step its time before a search takes the window over.
    std::thread::sleep(Duration::from_millis(1200));
}

/// Poll `read` until the covers have settled: `covers == total` (and there is
/// something to cover), or the value stopped moving for `stable` polls.
fn wait_covers(
    weak: &Weak<AppWindow>,
    read: fn(&AppWindow) -> (usize, usize),
    budget: Duration,
    stable: usize,
) -> (usize, usize) {
    let started = Instant::now();
    let mut last = (0usize, 0usize);
    let mut same = 0usize;
    loop {
        let now = on_ui(weak, read).unwrap_or(last);
        if now.1 > 0 && now.0 >= now.1 {
            return now;
        }
        if now == last && now.1 > 0 {
            same += 1;
        } else {
            same = 0;
        }
        last = now;
        if started.elapsed() >= budget || same >= stable {
            return now;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The search grid's cover count, as the QA reader sees it.
fn search_covers(ui: &AppWindow) -> (usize, usize) {
    let rows = ui.get_search_items();
    let total = rows.row_count();
    let mut covers = 0usize;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            covers += usize::from(row.thumb.size().width > 0);
        }
    }
    (covers, total)
}

/// Run one round's navigation and report what the list ended up with.
fn search_round(weak: &Weak<AppWindow>, keyword: &str) -> (CoverSnap, u64, u64) {
    let keyword = keyword.to_string();
    let hits_before = cover_cache_hits();
    let misses_before = cover_cache_misses();
    let _ = on_ui(weak, move |ui| {
        ui.set_search_text(keyword.clone().into());
        ui.invoke_do_search(keyword.into());
    });
    let _ = wait_covers(weak, search_covers, SEARCH_LOOP_WAIT, 10);
    let snap = on_ui(weak, search_snap).unwrap_or_default();
    (snap, hits_before, misses_before)
}

/// `BILI_SEARCH_LOOP`: N rounds of 搜索 → 等封面 → 返回 → 再搜索.
pub(crate) fn run_search_loop(weak: Weak<AppWindow>, spec: SearchLoopSpec) {
    let tag = if spec.with_player {
        "f181_after_player"
    } else {
        "f181_search_loop"
    };
    crate::debug_log(&format!(
        "search loop start rounds={} gap_ms={} player={} keywords={:?}",
        spec.rounds,
        spec.gap.as_millis(),
        u8::from(spec.with_player),
        SEARCH_LOOP_KEYWORDS
    ));
    wait_feed_ready(&weak);
    let mut covers_min = usize::MAX;
    let mut covers_max = 0usize;
    for round in 1..=spec.rounds {
        if spec.with_player && round == 1 {
            player_cycle(&weak);
            let _ = on_ui(&weak, |ui| ui.invoke_back_to_feed());
            std::thread::sleep(Duration::from_millis(600));
        }
        let keyword = SEARCH_LOOP_KEYWORDS[(round as usize - 1) % SEARCH_LOOP_KEYWORDS.len()];
        let (snap, hits_before, misses_before) = search_round(&weak, keyword);
        covers_min = covers_min.min(snap.covers);
        covers_max = covers_max.max(snap.covers);
        let pool = crate::netpool::stats();
        crate::debug_log(&format!(
            "search loop i={round} keyword={keyword} covers={}/{} pages={} seq={} players={} cache={} skipped={} failed={} pool={}/{}",
            snap.covers,
            snap.total,
            snap.pages,
            snap.seq,
            snap.players,
            snap.cache(hits_before, misses_before),
            snap.skipped,
            snap.failed,
            pool.running,
            pool.threads,
        ));
        if round == spec.rounds {
            // The proof shot: the last round, still on the results page.
            if let Some(path) = on_ui(&weak, move |ui| {
                capture_window_png(ui.window(), &qa_capture_path(tag));
                qa_capture_path(tag)
            }) {
                crate::debug_log(&format!("search loop capture tag={tag} path={}", path.display()));
            }
        }
        // 返回: the same callback the back button fires, then the beat that
        // makes the next round a real re-entry rather than a repaint.
        let _ = on_ui(&weak, |ui| ui.invoke_back_to_feed());
        std::thread::sleep(spec.gap);
    }
    crate::debug_log(&format!(
        "search loop done rounds={} covers_min={} covers_max={} players={}",
        spec.rounds,
        covers_min,
        covers_max,
        PLAYER_OPEN_COUNT.load(Ordering::SeqCst),
    ));
    if !env::flag("BILI_F181_HOLD") {
        std::thread::sleep(Duration::from_millis(800));
        request_quit("f181 search loop done");
    }
}

/// Open a real player from the 推荐流, then close it through the ✕ path, and
/// report the counter on both sides. With covers broken by a leaked gate the
/// `players=` numbers are the evidence.
fn player_cycle(weak: &Weak<AppWindow>) {
    let _ = on_ui(weak, |ui| ui.invoke_card_open(1, 0));
    let mut opened = 0usize;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(250));
        opened = PLAYER_OPEN_COUNT.load(Ordering::SeqCst);
        if opened > 0 {
            break;
        }
    }
    crate::debug_log(&format!("search loop player opened players={opened}"));
    // Let the window settle (playurl + first frame) so the close is a real one.
    std::thread::sleep(Duration::from_millis(2500));
    let closed = on_ui(weak, |_ui| f147_close_players()).unwrap_or(0);
    let mut players = PLAYER_OPEN_COUNT.load(Ordering::SeqCst);
    for _ in 0..40 {
        if players == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        players = PLAYER_OPEN_COUNT.load(Ordering::SeqCst);
    }
    crate::debug_log(&format!(
        "search loop player closed invoked={closed} players={players}"
    ));
}

/// `BILI_COVER_PAGE_TEST`: every list that shows covers, entered `rounds` times.
pub(crate) fn run_cover_page_test(weak: Weak<AppWindow>, rounds: u32) {
    crate::debug_log(&format!("cover page test start rounds={rounds}"));
    wait_feed_ready(&weak);
    let mut worst: Vec<(&'static str, usize, usize)> = Vec::new();
    for round in 1..=rounds {
        for page in 0..PAGE_COUNT {
            let (name, covers, total) = cover_page_round(&weak, page);
            if total == 0 || covers < total {
                worst.push((name, covers, total));
            }
            crate::debug_log(&format!(
                "cover page round={round} page={name} covers={covers}/{total}"
            ));
        }
    }
    let feed = on_ui(&weak, |ui| ui.invoke_back_to_feed()).is_some();
    crate::debug_log(&format!(
        "cover page test done rounds={rounds} back_to_feed={} short={} {:?}",
        u8::from(feed),
        worst.len(),
        worst
    ));
    if !env::flag("BILI_F181_HOLD") {
        std::thread::sleep(Duration::from_millis(800));
        request_quit("f181 cover page test done");
    }
}

const PAGE_COUNT: usize = 6;

/// Enter one page and report its covers. Returns `(name, covers, total)`.
fn cover_page_round(weak: &Weak<AppWindow>, page: usize) -> (&'static str, usize, usize) {
    match page {
        0 => {
            let _ = on_ui(weak, |ui| ui.invoke_back_to_feed());
            let (covers, total) = wait_covers(
                weak,
                |ui| feed_covers(ui),
                PAGE_ROUND_WAIT,
                8,
            );
            ("feed", covers, total)
        }
        1 => {
            let (snap, _, _) = search_round(weak, SEARCH_LOOP_KEYWORDS[0]);
            ("search", snap.covers, snap.total)
        }
        2 => {
            let _ = on_ui(weak, |ui| ui.invoke_open_region(0));
            let (covers, total) = wait_covers(weak, region_covers, PAGE_ROUND_WAIT, 8);
            ("region", covers, total)
        }
        3 => {
            let _ = on_ui(weak, |ui| ui.invoke_pgc_tab_pick(1));
            let (covers, total) = wait_covers(weak, pgc_covers, PAGE_ROUND_WAIT, 8);
            ("pgc", covers, total)
        }
        4 => {
            let _ = on_ui(weak, |ui| ui.invoke_live_open());
            let (covers, total) = wait_covers(weak, live_covers, PAGE_ROUND_WAIT, 8);
            ("live", covers, total)
        }
        _ => {
            let _ = on_ui(weak, |ui| ui.invoke_fish_open());
            let (covers, total) = wait_covers(weak, fish_covers, PAGE_ROUND_WAIT, 8);
            ("fish", covers, total)
        }
    }
}

fn feed_covers(ui: &AppWindow) -> (usize, usize) {
    let rows = ui.get_feed_items();
    let total = rows.row_count();
    let mut covers = 0usize;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            covers += usize::from(row.thumb.size().width > 0);
        }
    }
    (covers, total)
}

fn region_covers(ui: &AppWindow) -> (usize, usize) {
    let rows = ui.get_region_items();
    let total = rows.row_count();
    let mut covers = 0usize;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            covers += usize::from(row.thumb.size().width > 0);
        }
    }
    (covers, total)
}

fn pgc_covers(ui: &AppWindow) -> (usize, usize) {
    let rows = ui.get_pgc_items();
    let total = rows.row_count();
    let mut covers = 0usize;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            covers += usize::from(row.thumb.size().width > 0);
        }
    }
    (covers, total)
}

fn live_covers(ui: &AppWindow) -> (usize, usize) {
    let rows = ui.get_live_cards();
    let total = rows.row_count();
    let mut covers = 0usize;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            covers += usize::from(row.cover.size().width > 0);
        }
    }
    (covers, total)
}

fn fish_covers(ui: &AppWindow) -> (usize, usize) {
    let rows = ui.get_fish_rows();
    let total = rows.row_count();
    let mut covers = 0usize;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            covers += usize::from(row.cover.size().width > 0);
        }
    }
    (covers, total)
}
