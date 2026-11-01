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
    // f216: the cover soak. `BILI_COVER_WATCH` alone only observes; with
    // `BILI_COVER_SOAK` the same run also walks the pages.
    if let Some(every) = env::var("BILI_COVER_WATCH") {
        let mut parts = every.split(',');
        let every_ms = parts
            .next()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(1000)
            .clamp(200, 60_000);
        let mut guard = env::var("BILI_COVER_SOAK").is_some();
        for token in parts.map(str::trim) {
            if token.eq_ignore_ascii_case("guard") {
                guard = true;
            } else if token.eq_ignore_ascii_case("noguard") {
                guard = false;
            }
        }
        let weak = weak.clone();
        std::thread::spawn(move || run_cover_watch(weak, Duration::from_millis(every_ms), guard));
    }
    if let Some(spec) = env::var("BILI_COVER_SOAK") {
        let mut minutes = 10u64;
        let mut page_ms = 15_000u64;
        let mut with_player = false;
        for (index, token) in spec.split(',').map(str::trim).enumerate() {
            if token.is_empty() {
                continue;
            }
            if token.eq_ignore_ascii_case("player") {
                with_player = true;
                continue;
            }
            match token.parse::<u64>() {
                Ok(value) if index == 0 => minutes = value.clamp(1, 120),
                Ok(value) => page_ms = value.clamp(2_000, 120_000),
                Err(_) => {}
            }
        }
        let weak = weak.clone();
        std::thread::spawn(move || run_cover_soak(weak, minutes, page_ms, with_player));
        return;
    }
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

// ---------------------------------------------------------------------------
// f216: cover soak — every page, one reading per second, and the reason a page
// went blank named instead of guessed.
// ---------------------------------------------------------------------------

/// Every cover list the app renders, as `(name, covers, total)`. Read on the UI
/// thread; a list with no rows is omitted so a line stays short.
pub(crate) fn cover_lists(ui: &AppWindow) -> Vec<(&'static str, usize, usize)> {
    fn count<T: Clone + 'static>(
        rows: &ModelRc<T>,
        thumb: fn(&T) -> &Image,
    ) -> (usize, usize) {
        let total = rows.row_count();
        let mut covers = 0usize;
        for index in 0..total {
            if let Some(row) = rows.row_data(index) {
                covers += usize::from(thumb(&row).size().width > 0);
            }
        }
        (covers, total)
    }
    let mut out: Vec<(&'static str, usize, usize)> = Vec::new();
    let mut push = |name: &'static str, (covers, total): (usize, usize)| {
        if total > 0 {
            out.push((name, covers, total));
        }
    };
    push("feed", count(&ui.get_feed_items(), |row| &row.thumb));
    push("search", count(&ui.get_search_items(), |row| &row.thumb));
    push("region", count(&ui.get_region_items(), |row| &row.thumb));
    push("pgc", count(&ui.get_pgc_items(), |row| &row.thumb));
    push("live", count(&ui.get_live_cards(), |row| &row.cover));
    push("hist", count(&ui.get_history_items(), |row| &row.thumb));
    push("toview", count(&ui.get_toview_items(), |row| &row.thumb));
    push("folder", count(&ui.get_folder_items(), |row| &row.thumb));
    push("dynamic", count(&ui.get_dynamic_items(), |row| &row.cover));
    out
}

/// `view_mode` as the page the user is looking at.
pub(crate) fn page_name(mode: i32) -> &'static str {
    match mode {
        1 => "search",
        2 => "detail",
        3 => "favorites",
        5 => "history",
        8 => "region",
        20 => "fish",
        21 => "settings",
        22 => "pgc",
        23 => "live",
        _ => "feed",
    }
}

/// `BILI_COVER_WATCH=<ms>`: one line per tick with the covers of every list and
/// every counter that could explain them. Written for the f216 timeline.
///
/// With `guard` (the soak's default), a player window that was not asked for —
/// a stray click from another QA run on the same desktop, or the user opening a
/// video mid-run — is closed again: covers pause while a player is open by
/// design, so an unasked-for player would make the whole run measure nothing.
pub(crate) fn run_cover_watch(weak: Weak<AppWindow>, every: Duration, guard: bool) {
    crate::debug_log(&format!(
        "covers watch start every_ms={}",
        every.as_millis()
    ));
    let started = Instant::now();
    let mut last = bilibili::ThumbStats::default();
    let mut last_counters = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut peak: Vec<(&'static str, usize, usize)> = Vec::new();
    let mut leak_logged = false;
    loop {
        std::thread::sleep(every);
        // The counters are read BEFORE the models: a row can only be blank in
        // this snapshot if no fetch landed before it, so "no activity + fewer
        // covers" is a real loss instead of a sampling race.
        let t = started.elapsed().as_secs();
        let decodes = cover_decode_leases();
        let oldest = decodes.first().map(|(_, _, _, ms)| *ms).unwrap_or(0);
        let stats = bilibili::thumb_stats();
        let (hits, misses, fails, disp, warm, first, skipped) = (
            cover_cache_hits(),
            cover_cache_misses(),
            cover_failures(),
            cover_fetches_display(),
            cover_fetches_warm(),
            cover_fetches_first_screen(),
            cover_skip_player(),
        );
        let patched = cover_patched();
        let empty = cover_empty_patches();
        let Some((mode, lists, players)) = on_ui(&weak, |ui| {
            (
                ui.get_view_mode(),
                cover_lists(ui),
                PLAYER_OPEN_COUNT.load(Ordering::SeqCst),
            )
        }) else {
            return;
        };
        if guard && players > 0 {
            let closed = on_ui(&weak, |_ui| f147_close_players()).unwrap_or(0);
            crate::debug_log(&format!(
                "covers guard players={players} unasked close_invoked={closed}"
            ));
        }
        let pool = crate::netpool::stats();
        let cache = cover_cache()
            .lock()
            .map(|cache| (cache.entries.len(), cache.bytes))
            .unwrap_or((0, 0));
        let lists_text = lists
            .iter()
            .map(|(name, covers, total)| format!("{name}={covers}/{total}"))
            .collect::<Vec<_>>()
            .join(" ");
        crate::debug_log(&format!(
            "covers t={t}s page={} players={players} {lists_text} decodes={} oldest_ms={oldest} \
             d_hit={} d_miss={} d_fail={} d_disp={} d_warm={} d_first={} d_skip={} \
             patched={} empty={} dropped={} cache={}/{}B \
             http2={} http3={} http4={} http5={} net={} decode_fail={} ok={} ms_avg={} ms_max={} \
             pool={}/{} q={}",
            page_name(mode),
            decodes.len(),
            hits.saturating_sub(last_counters.0),
            misses.saturating_sub(last_counters.1),
            fails.saturating_sub(last_counters.2),
            disp.saturating_sub(last_counters.3),
            warm.saturating_sub(last_counters.4),
            first.saturating_sub(last_counters.5),
            skipped.saturating_sub(last_counters.6),
            patched,
            empty,
            cover_dropped_guards(),
            cache.0,
            cache.1,
            stats.req_2xx.saturating_sub(last.req_2xx),
            stats.req_3xx.saturating_sub(last.req_3xx),
            stats.req_4xx.saturating_sub(last.req_4xx),
            stats.req_5xx.saturating_sub(last.req_5xx),
            stats.net_err.saturating_sub(last.net_err),
            stats.decode_fail.saturating_sub(last.decode_fail),
            stats.ok.saturating_sub(last.ok),
            stats.ms_total.saturating_sub(last.ms_total),
            stats.ms_max,
            pool.running,
            pool.threads,
            pool.queued,
        ));
        last = stats;
        last_counters = (hits, misses, fails, disp, warm, first, skipped);
        // A cover that was there and is not any more, with nothing left loading
        // for it: the exact failure the user reported, named per list and with
        // the counters at that moment. The transient text-only republish of a
        // page that is (re)loading has decodes in flight, so it is not reported.
        let idle = decodes.is_empty()
            && hits == last_counters.0
            && misses == last_counters.1;
        for (name, covers, total) in &lists {
            let entry = peak
                .iter_mut()
                .find(|(other, _, _)| other == name);
            let peak_covers = match entry {
                Some(slot) => {
                    if *covers > slot.1 {
                        slot.1 = *covers;
                    }
                    slot.1
                }
                None => {
                    peak.push((name, *covers, *total));
                    *covers
                }
            };
            if *covers < peak_covers && idle && peak_covers > 0 {
                crate::debug_log(&format!(
                    "covers lost t={t}s page={} list={name} peak={peak_covers}/{total} \
                     now={covers}/{total} decodes=0 players={players}",
                    page_name(mode)
                ));
            }
        }
        // A decode that outlives any plausible request is stuck: name it.
        if let Some((_, url, path, ms)) = decodes.first() {
            if *ms > 60_000 && !leak_logged {
                leak_logged = true;
                crate::debug_log(&format!(
                    "covers decode leak n={} oldest_ms={ms} path={path} url={url}",
                    decodes.len()
                ));
            }
        } else {
            leak_logged = false;
        }
    }
}

/// `BILI_COVER_SOAK=<minutes>[,<page_ms>][,player]`: walk the six user-facing
/// cover pages for `minutes`, holding the home feed at the 0/5/10 minute marks
/// so the acceptance screenshots are of the same page. `player` also opens a
/// real player from the feed and closes it through the ✕ path once per cycle:
/// that is the "开播放器 → 关播放器 → 所有封面都没了" regression.
pub(crate) fn run_cover_soak(weak: Weak<AppWindow>, minutes: u64, page_ms: u64, with_player: bool) {
    let total = Duration::from_secs(minutes.saturating_mul(60));
    crate::debug_log(&format!(
        "covers soak start minutes={minutes} page_ms={page_ms} player={}",
        u8::from(with_player)
    ));
    wait_feed_ready(&weak);
    let started = Instant::now();
    let pages = [
        "feed", "region", "pgc", "live", "search", "history", "favorites",
    ];
    let mut index = 0usize;
    let mut next_hold = Duration::from_secs(0);
    // A little past the deadline, so the last 5-minute hold (the 10-minute
    // acceptance shot) is still taken on the home feed.
    while started.elapsed() < total + Duration::from_secs(60) {
        let t = started.elapsed();
        // At every 5-minute mark, sit on the feed long enough for the shot: the
        // 0/5/10 minute screenshots are the same page.
        let slot = Duration::from_secs((t.as_secs() / 300) * 300);
        if slot >= next_hold {
            next_hold = slot + Duration::from_secs(300);
            let _ = on_ui(&weak, |ui| ui.invoke_back_to_feed());
            crate::debug_log(&format!(
                "covers soak hold t={}s page=feed",
                started.elapsed().as_secs()
            ));
            std::thread::sleep(Duration::from_millis(45_000));
            index = 1;
            continue;
        }
        let page = pages[index % pages.len()];
        let label = page.to_string();
        let _ = on_ui(&weak, move |ui| match label.as_str() {
            "region" => ui.invoke_open_region(0),
            "pgc" => ui.invoke_pgc_tab_pick(1),
            "live" => ui.invoke_live_open(),
            "search" => {
                ui.set_search_text("原神".into());
                ui.invoke_do_search("原神".into());
            }
            "history" => ui.invoke_nav_history(),
            "favorites" => ui.invoke_nav_favorites(),
            _ => ui.invoke_back_to_feed(),
        });
        crate::debug_log(&format!(
            "covers soak page={page} t={}s",
            started.elapsed().as_secs()
        ));
        std::thread::sleep(Duration::from_millis(page_ms));
        index = (index + 1) % pages.len();
        if with_player && index == 0 {
            player_cycle(&weak);
        }
    }
    let _ = on_ui(&weak, |ui| ui.invoke_back_to_feed());
    crate::debug_log(&format!(
        "covers soak done t={}s lists={:?}",
        started.elapsed().as_secs(),
        on_ui(&weak, cover_lists).unwrap_or_default()
    ));
    if !env::flag("BILI_F216_HOLD") {
        std::thread::sleep(Duration::from_millis(800));
        request_quit("f216 cover soak done");
    }
}
