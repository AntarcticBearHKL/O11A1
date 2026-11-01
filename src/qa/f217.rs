//! f217 QA: the five search tabs (视频 / 番剧 / 影视 / 直播 / 用户).
//!
//! Everything is driven through the same Slint callbacks the search page
//! fires (`do_search` / `search_tab_changed` / `open_search_item`), and every
//! number in the log is read back from the UI models or the search state, never
//! assumed. Inert unless `BILI_F217_TEST` is set.
//!
//!   `BILI_F217_TEST=1` (default)
//!     Three keywords × five tabs against the real endpoints. For each tab it
//!     logs the desensitised request URL, the settled row count, the wall time
//!     and the first three titles with what the click would open
//!     (`bvid` / `season` / `room`). It then captures
//!     `f217_search_bangumi.png` (番剧), `f217_search_live.png` (直播),
//!     `f217_search_video.png` / `f217_search_user.png` (the unchanged cards)
//!     and opens the first 番剧 season (选集 panel open) and the first 直播
//!     room through the real activation callback: both players are checked for
//!     presented frames. Finally `f217_search_empty.png` captures a tab that
//!     came back empty.
//!
//!   `BILI_F217_TEST=empty`
//!     Only the empty-result state (fast capture).
//!
//!   `BILI_F217_TEST=anon`
//!     The 番剧 / 影视 / 直播 indexes queried with no account in the slot (the
//!     anonymous session `run_search` builds). Logs the counts and captures
//!     the window as it really is (the login page): searching does not need an
//!     account, the account only decides what 清晰度 a season may play.
//!
//!   `BILI_F217_TEST=fail`
//!     A real network outage: the slot is cleared and `HTTPS_PROXY` points at a
//!     closed port before the search runs, so the anonymous session's request
//!     fails through the normal error path. Captures `f217_search_failed.png`.
//!
//! Captures land in `BILI_CAPTURE_DIR` (the script sets it to `docs`).

use crate::*;

fn f217_log(line: impl AsRef<str>) {
    crate::debug_log(&format!("f217 {}", line.as_ref()));
}

/// The three keywords the acceptance run searches on every tab.
const F217_KEYWORDS: [&str; 3] = ["名侦探柯南", "剑网3", "孤独摇滚"];
/// A keyword no tab can match, for the empty state.
const F217_EMPTY_KEYWORD: &str = "zzzz没有这个内容的番剧9999";

/// The request the app actually issues, minus cookies and the WBI signature.
///
/// This is the query string `search_data_referer` builds (`w_rid`/`wts` are the
/// signed fields and are replaced here), which is what makes the table in the
/// report provable rather than a restatement of the code.
fn f217_url(kind: bilibili::SearchKind, keyword: &str, page: i64, ps: i64) -> String {
    format!(
        "/x/web-interface/wbi/search/type?search_type={}&keyword={}&page={}&page_size={}&w_rid=<redacted>&wts=<redacted>",
        kind.search_type(),
        keyword,
        page.max(1),
        ps
    )
}

/// Read one tab's published rows: `(count, first three titles)`.
fn f217_rows(ui: &AppWindow, tab: i32) -> (usize, Vec<String>) {
    match bilibili::SearchKind::from_tab(tab) {
        bilibili::SearchKind::Video | bilibili::SearchKind::User => {
            let rows = ui.get_search_items();
            let total = rows.row_count();
            let titles = (0..total.min(3))
                .filter_map(|index| rows.row_data(index))
                .map(|row| row.title.to_string())
                .collect();
            (total, titles)
        }
        bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film => {
            let rows = ui.get_search_media_items();
            let total = rows.row_count();
            let titles = (0..total.min(3))
                .filter_map(|index| rows.row_data(index))
                .map(|row| format!("{} [{} | {}]", row.title, row.meta, row.badge))
                .collect();
            (total, titles)
        }
        bilibili::SearchKind::Live => {
            let rows = ui.get_search_live_items();
            let total = rows.row_count();
            let titles = (0..total.min(3))
                .filter_map(|index| rows.row_data(index))
                .map(|row| {
                    format!(
                        "{} [{} | {} | {}]",
                        row.title, row.uname, row.area, row.online
                    )
                })
                .collect();
            (total, titles)
        }
    }
}

/// The raw title of the first row (empty while the skeleton is up). Separate
/// from [`f217_rows`] because that one decorates the title with the card's own
/// meta line, so a placeholder row would still read as "non-empty".
fn f217_first_title(ui: &AppWindow, tab: i32) -> String {
    match bilibili::SearchKind::from_tab(tab) {
        bilibili::SearchKind::Video | bilibili::SearchKind::User => ui
            .get_search_items()
            .row_data(0)
            .map(|row| row.title.to_string())
            .unwrap_or_default(),
        bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film => ui
            .get_search_media_items()
            .row_data(0)
            .map(|row| row.title.to_string())
            .unwrap_or_default(),
        bilibili::SearchKind::Live => ui
            .get_search_live_items()
            .row_data(0)
            .map(|row| row.title.to_string())
            .unwrap_or_default(),
    }
}

/// The click target of the first three rows, aligned with [`f217_rows`].
fn f217_targets(
    tab: i32,
    search_state: &Arc<Mutex<SearchState>>,
    search_bvids: &Arc<Mutex<Vec<String>>>,
) -> Vec<String> {
    let state = match search_state.lock() {
        Ok(state) => state,
        Err(_) => return Vec::new(),
    };
    match bilibili::SearchKind::from_tab(tab) {
        bilibili::SearchKind::Video => search_bvids
            .lock()
            .map(|guard| guard.iter().take(3).cloned().collect())
            .unwrap_or_default(),
        bilibili::SearchKind::User => state
            .mids
            .iter()
            .take(3)
            .map(|mid| format!("mid={mid}"))
            .collect(),
        bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film => state
            .season_ids
            .iter()
            .take(3)
            .map(|season| format!("season={season}"))
            .collect(),
        bilibili::SearchKind::Live => state
            .rooms
            .iter()
            .take(3)
            .map(|room| format!("room={} uname={}", room.room_id, room.uname))
            .collect(),
    }
}

/// Wait (bounded) until the search that was just fired has settled.
///
/// "Settled" means the visible tab is `tab`, the refresh pill is off, and the
/// model stopped changing **and** carries real content: the tab switch publishes
/// one screen of empty placeholder rows first (the skeleton), so a bare
/// `row_count` would settle on the skeleton a few hundred milliseconds before
/// the API answers. A status line (empty result / failure) settles too, because
/// those tabs deliberately have no rows.
fn f217_wait_search(weak: &Weak<AppWindow>, tab: i32, budget: Duration) -> Duration {
    let started = Instant::now();
    let mut last = (usize::MAX, 0usize, 0usize, String::new());
    let mut stable = 0u32;
    while started.elapsed() < budget {
        let probe = f145_on_ui(weak, move |ui| {
            let (rows, _titles) = f217_rows(ui, tab);
            let first = usize::from(!f217_first_title(ui, tab).trim().is_empty());
            let status = ui.get_search_status().to_string();
            (
                rows,
                first,
                usize::from(ui.get_list_refreshing()),
                status,
                ui.get_search_tab() == tab,
            )
        });
        if let Some((rows, first, busy, status, on_tab)) = probe {
            let settled = busy == 0 && on_tab && (first > 0 || !status.trim().is_empty());
            let now = (rows, first, busy, status);
            if settled && now == last {
                stable += 1;
                if stable >= 3 {
                    break;
                }
            } else {
                stable = 0;
            }
            last = now;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    started.elapsed()
}

/// Covers that have arrived for the tab's first `want` rows, as `(shown, want)`.
fn f217_covers(ui: &AppWindow, tab: i32) -> (usize, usize) {
    match bilibili::SearchKind::from_tab(tab) {
        bilibili::SearchKind::Video | bilibili::SearchKind::User => {
            let rows = ui.get_search_items();
            let want = rows.row_count().min(8);
            let shown = (0..want)
                .filter_map(|index| rows.row_data(index))
                .filter(|row| row.thumb.size().width > 0)
                .count();
            (shown, want)
        }
        bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film => {
            let rows = ui.get_search_media_items();
            let want = rows.row_count().min(8);
            let shown = (0..want)
                .filter_map(|index| rows.row_data(index))
                .filter(|row| row.thumb.size().width > 0)
                .count();
            (shown, want)
        }
        bilibili::SearchKind::Live => {
            let rows = ui.get_search_live_items();
            let want = rows.row_count().min(8);
            let shown = (0..want)
                .filter_map(|index| rows.row_data(index))
                .filter(|row| row.cover.size().width > 0)
                .count();
            (shown, want)
        }
    }
}

/// Wait (bounded) for the first screen of covers to arrive: a capture taken the
/// moment the rows closed would show empty plates, because the covers stream
/// after the model lands.
fn f217_wait_covers(weak: &Weak<AppWindow>, tab: i32, budget: Duration) -> (usize, usize) {
    let started = Instant::now();
    let mut last = (0usize, 0usize);
    loop {
        let now = f145_on_ui(weak, move |ui| f217_covers(ui, tab)).unwrap_or(last);
        last = now;
        if (now.1 > 0 && now.0 >= now.1) || started.elapsed() >= budget {
            return now;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Capture the main window into `BILI_CAPTURE_DIR/<tag>.png`.
fn f217_shot(weak: &Weak<AppWindow>, tag: &str) -> Option<std::path::PathBuf> {
    let tag = tag.to_string();
    f145_on_ui(weak, move |ui| {
        let path = qa_capture_path(&tag);
        capture_window_png(&ui.window(), &path)?;
        Some(path)
    })
    .flatten()
}

fn f217_log_shot(tag: &str, path: Option<std::path::PathBuf>) {
    match path {
        Some(path) => f217_log(format!("shot {tag} path={}", path.display())),
        None => f217_log(format!("shot {tag} failed")),
    }
}

/// The page size the search actually asked for, read from the UI.
fn f217_page_size(weak: &Weak<AppWindow>) -> i64 {
    f145_on_ui(weak, |ui| ui.get_list_page_size()).unwrap_or(24) as i64
}

/// Run one keyword on one tab and log the evidence for it.
fn f217_run_tab(
    weak: &Weak<AppWindow>,
    tab: i32,
    keyword: &str,
    search_state: &Arc<Mutex<SearchState>>,
    search_bvids: &Arc<Mutex<Vec<String>>>,
) -> usize {
    let kind = bilibili::SearchKind::from_tab(tab);
    let ps = f217_page_size(weak);
    let tab_w = tab;
    let _ = f145_on_ui(weak, move |ui| ui.invoke_search_tab_changed(tab_w));
    let waited = f217_wait_search(weak, tab, Duration::from_secs(30));
    let rows = f145_on_ui(weak, move |ui| f217_rows(ui, tab)).unwrap_or_default();
    let targets = f217_targets(tab, search_state, search_bvids);
    let page = f145_on_ui(weak, |ui| ui.get_search_current_page()).unwrap_or(1);
    let total = f145_on_ui(weak, |ui| ui.get_search_total_pages()).unwrap_or(1);
    let status = f145_on_ui(weak, |ui| ui.get_search_status().to_string()).unwrap_or_default();
    crate::debug_log(&format!(
        "f217 tab={} type={} keyword={:?} page={}/{} ps={ps} items={} wait_ms={} status={:?} url={}",
        kind.label(),
        kind.search_type(),
        keyword,
        page,
        total,
        rows.0,
        waited.as_millis(),
        status,
        f217_url(kind, keyword, page as i64, ps)
    ));
    for (index, title) in rows.1.iter().enumerate() {
        let target = targets.get(index).cloned().unwrap_or_default();
        crate::debug_log(&format!("f217 top{} {} {target} {title}", index + 1, kind.label()));
    }
    rows.0
}

/// Wait for a player window to present frames, then report what it shows.
fn f217_player_open(weak: &Weak<AppWindow>, tag: &str, budget: Duration) -> bool {
    let started = Instant::now();
    let mut frames = 0u64;
    let mut last = String::new();
    while started.elapsed() < budget {
        let probe = f145_on_ui(weak, |_| {
            f145_latest_player().map(|session| {
                (
                    session.control.presented_frames(),
                    session.window.get_live(),
                    session.window.get_video_title().to_string(),
                    session.window.get_series_rows().row_count(),
                    session.window.get_live_sub().to_string(),
                    session.window.get_playing(),
                )
            })
        })
        .flatten();
        if let Some((presented, live, title, series, sub, playing)) = probe {
            frames = presented;
            last = format!(
                "live={live} playing={playing} series_rows={series} title={title:?} sub={sub:?} frames={presented}"
            );
            if presented > 0 {
                f217_log(format!("player {tag} ok {last}"));
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    f217_log(format!("player {tag} no-frames frames={frames} {last}"));
    false
}

/// Open the player's 选集 panel, so the season shot shows the episode list the
/// click-through is supposed to produce.
fn f217_open_series_panel(weak: &Weak<AppWindow>) {
    let _ = f145_on_ui(weak, |_| {
        if let Some(session) = f145_latest_player() {
            session.window.set_right_panel(1);
            session.window.set_right_tab(4);
        }
    });
    std::thread::sleep(Duration::from_millis(900));
}

/// Capture the newest player window (main-window captures never include it).
fn f217_shot_player(weak: &Weak<AppWindow>, tag: &str) -> Option<std::path::PathBuf> {
    let tag = tag.to_string();
    f145_on_ui(weak, move |_ui| {
        let session = f145_latest_player()?;
        let path = qa_capture_path(&tag);
        capture_window_png(&session.window.window(), &path)?;
        Some(path)
    })
    .flatten()
}

fn f217_close_player(weak: &Weak<AppWindow>) {
    let _ = f145_on_ui(weak, |_| {
        if let Some(session) = f145_latest_player() {
            session.window.invoke_player_closed();
        }
    });
    std::thread::sleep(Duration::from_millis(1200));
}

/// `BILI_F217_TEST`: the acceptance run.
pub(crate) fn start_f217_test_hook(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_bvids: Arc<Mutex<Vec<String>>>,
) {
    let Some(spec) = env::var("BILI_F217_TEST") else {
        return;
    };
    let spec = spec.trim().to_ascii_lowercase();
    let mode = if spec == "1" || spec.is_empty() {
        "all".to_string()
    } else {
        spec.clone()
    };
    std::thread::spawn(move || {
        // The window the report's screenshots are taken in.
        let _ = f145_on_ui(&weak, |ui| {
            ui.window()
                .set_size(slint::LogicalSize::new(1280.0, 800.0));
            ui.window()
                .set_position(slint::LogicalPosition::new(40.0, 40.0));
        });
        std::thread::sleep(Duration::from_millis(600));

        // Restart proof: what this process loaded before any search ran.
        let startup = f145_on_ui(&weak, |ui| {
            (ui.get_search_tab(), settings::load().search_tab)
        });
        if let Some((ui_tab, stored)) = startup {
            f217_log(format!(
                "startup tab={ui_tab} stored={stored} expected={}",
                bilibili::SEARCH_TAB_BANGUMI
            ));
        }

        if mode == "anon" {
            // Nothing is published in the slot: `run_search` builds the
            // anonymous session. The app is signed out, so the window shows the
            // login page; the search callbacks are the product path either way,
            // and the log carries what each tab answered.
            f217_log("anon start (no account in the slot)");
            for keyword in ["名侦探柯南", "剑网3"] {
                let keyword_w = keyword.to_string();
                let _ = f145_on_ui(&weak, move |ui| {
                    ui.invoke_do_search(keyword_w.clone().into());
                });
                std::thread::sleep(Duration::from_millis(900));
                for tab in 0..5 {
                    let tab_w = tab;
                    let _ = f145_on_ui(&weak, move |ui| ui.invoke_search_tab_changed(tab_w));
                    let waited = f217_wait_search(&weak, tab, Duration::from_secs(30));
                    let rows = f145_on_ui(&weak, move |ui| f217_rows(ui, tab)).unwrap_or_default();
                    let status =
                        f145_on_ui(&weak, |ui| ui.get_search_status().to_string()).unwrap_or_default();
                    let kind = bilibili::SearchKind::from_tab(tab);
                    crate::debug_log(&format!(
                        "f217 anon tab={} type={} keyword={keyword:?} items={} wait_ms={} status={:?}",
                        kind.label(),
                        kind.search_type(),
                        rows.0,
                        waited.as_millis(),
                        status
                    ));
                    for (index, title) in rows.1.iter().enumerate() {
                        crate::debug_log(&format!(
                            "f217 anon top{} {} {title}",
                            index + 1,
                            kind.label()
                        ));
                    }
                }
            }
            f217_log_shot("f217_search_anon", f217_shot(&weak, "f217_search_anon"));
            return;
        }

        if mode == "fail" {
            // A real outage: drop the account (so the anonymous path builds a
            // fresh client) and point HTTPS at a closed port. reqwest reads the
            // proxy environment when it builds that client, so the request
            // itself fails - no fabricated error text anywhere.
            if let Ok(mut guard) = session_slot.lock() {
                *guard = None;
            }
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
            std::env::set_var("ALL_PROXY", "http://127.0.0.1:1");
            f217_log("fail start proxy=127.0.0.1:1 session=cleared");
            // The start-up restore fails against the closed port and publishes
            // the login page itself; let it finish first, otherwise it would
            // overwrite the page below.
            std::thread::sleep(Duration::from_secs(6));
            // The login page would hide the search page. This only exposes it
            // so the failure state is visible; the failure itself is the real
            // request failing.
            let _ = f145_on_ui(&weak, |ui| {
                ui.set_logged_in(true);
                ui.set_view_mode(1);
            });
            std::thread::sleep(Duration::from_millis(400));
            let _ = f145_on_ui(&weak, |ui| {
                ui.set_search_text("名侦探柯南".into());
                ui.invoke_do_search("名侦探柯南".into());
            });
            let tab = f145_on_ui(&weak, |ui| ui.get_search_tab())
                .unwrap_or(bilibili::SEARCH_TAB_VIDEO);
            let waited = f217_wait_search(&weak, tab, Duration::from_secs(20));
            // A late start-up publish may have switched back to the login page.
            let _ = f145_on_ui(&weak, |ui| {
                ui.set_logged_in(true);
                ui.set_view_mode(1);
            });
            std::thread::sleep(Duration::from_millis(600));
            let status =
                f145_on_ui(&weak, |ui| ui.get_search_status().to_string()).unwrap_or_default();
            let rows = f145_on_ui(&weak, move |ui| f217_rows(ui, tab)).unwrap_or_default();
            crate::debug_log(&format!(
                "f217 fail tab={} items={} wait_ms={} status={:?}",
                bilibili::SearchKind::from_tab(tab).label(),
                rows.0,
                waited.as_millis(),
                status
            ));
            f217_log_shot("f217_search_failed", f217_shot(&weak, "f217_search_failed"));
            return;
        }

        // ---- the full run: wait for the account the app itself restores ----
        for _ in 0..600 {
            if active_session(&session_slot).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if active_session(&session_slot).is_none() {
            f217_log("no session after 60s: results would be the anonymous ones");
        }
        // The feed may still be loading; the search page does not depend on it.
        std::thread::sleep(Duration::from_millis(600));

        let empty_only = mode == "empty";
        let keywords: &[&str] = if empty_only {
            &[F217_KEYWORDS[0]]
        } else {
            &F217_KEYWORDS
        };

        for keyword in keywords {
            f217_log(format!("keyword {keyword:?} start"));
            let keyword_w = keyword.to_string();
            let _ = f145_on_ui(&weak, move |ui| {
                ui.set_search_text(keyword_w.clone().into());
                ui.invoke_do_search(keyword_w.clone().into());
            });
            std::thread::sleep(Duration::from_millis(400));
            for tab in 0..5 {
                f217_run_tab(&weak, tab, keyword, &search_state, &search_bvids);
                // The two shots whose content is the tab itself. The 番剧 and
                // 直播 shots use 剑网3: its 国创 seasons and its 80 rooms are
                // available on this network, while the Japanese-licensed
                // seasons ("名侦探柯南" / "孤独摇滚") are region-filtered out
                // of the PGC index here (logged as `items=0`).
                if *keyword == "剑网3" && tab == bilibili::SEARCH_TAB_BANGUMI {
                    let covers = f217_wait_covers(&weak, tab, Duration::from_secs(20));
                    f217_log(format!("bangumi covers={}/{}", covers.0, covers.1));
                    f217_log_shot(
                        "f217_search_bangumi",
                        f217_shot(&weak, "f217_search_bangumi"),
                    );
                }
                if *keyword == "剑网3" && tab == bilibili::SEARCH_TAB_LIVE {
                    let covers = f217_wait_covers(&weak, tab, Duration::from_secs(20));
                    f217_log(format!("live covers={}/{}", covers.0, covers.1));
                    f217_log_shot("f217_search_live", f217_shot(&weak, "f217_search_live"));
                }
                // 视频 / 用户 keep their old cards; these shots prove the tab
                // switch did not disturb the layout they already had.
                if *keyword == "孤独摇滚" && tab == bilibili::SEARCH_TAB_VIDEO {
                    let covers = f217_wait_covers(&weak, tab, Duration::from_secs(20));
                    f217_log(format!("video covers={}/{}", covers.0, covers.1));
                    f217_log_shot("f217_search_video", f217_shot(&weak, "f217_search_video"));
                }
                if *keyword == "剑网3" && tab == bilibili::SEARCH_TAB_USER {
                    let covers = f217_wait_covers(&weak, tab, Duration::from_secs(20));
                    f217_log(format!("user covers={}/{}", covers.0, covers.1));
                    f217_log_shot("f217_search_user", f217_shot(&weak, "f217_search_user"));
                }
                // Click-through: the first 番剧 result opens its season page in
                // the player (选集 = the whole season), the first 直播 result
                // opens its room. Both go through `open_search_item`, i.e. the
                // same callback a real click fires.
                if *keyword == "剑网3" && tab == bilibili::SEARCH_TAB_BANGUMI {
                    let opened = f145_on_ui(&weak, |ui| ui.invoke_open_search_item(0)).is_some();
                    if opened {
                        let ok = f217_player_open(&weak, "season", Duration::from_secs(30));
                        f217_log(format!("season click opened={opened} ok={ok}"));
                        f217_open_series_panel(&weak);
                        let panel = f145_on_ui(&weak, |_| {
                            f145_latest_player().map(|session| {
                                (
                                    session.window.get_right_panel(),
                                    session.window.get_series_rows().row_count(),
                                )
                            })
                        })
                        .flatten();
                        f217_log(format!("season series panel={panel:?}"));
                        f217_log_shot(
                            "f217_season_player",
                            f217_shot_player(&weak, "f217_season_player"),
                        );
                        f217_close_player(&weak);
                    } else {
                        f217_log("season click did not fire");
                    }
                }
                if *keyword == "剑网3" && tab == bilibili::SEARCH_TAB_LIVE {
                    let opened = f145_on_ui(&weak, |ui| ui.invoke_open_search_item(0)).is_some();
                    if opened {
                        let ok = f217_player_open(&weak, "live", Duration::from_secs(30));
                        f217_log(format!("live click opened={opened} ok={ok}"));
                        f217_log_shot(
                            "f217_live_player",
                            f217_shot_player(&weak, "f217_live_player"),
                        );
                        f217_close_player(&weak);
                    } else {
                        f217_log("live click did not fire");
                    }
                }
            }
        }

        // ---- empty state: the PGC index for a keyword nothing can match ----
        let _ = f145_on_ui(&weak, move |ui| {
            ui.set_search_text(F217_EMPTY_KEYWORD.into());
            ui.invoke_do_search(F217_EMPTY_KEYWORD.into());
        });
        std::thread::sleep(Duration::from_millis(400));
        f217_run_tab(
            &weak,
            bilibili::SEARCH_TAB_BANGUMI,
            F217_EMPTY_KEYWORD,
            &search_state,
            &search_bvids,
        );
        f217_log_shot("f217_search_empty", f217_shot(&weak, "f217_search_empty"));

        // ---- the tab survives a restart: the stored value, read back ----
        std::thread::sleep(Duration::from_millis(800));
        let stored = settings::load().search_tab;
        let live_tab = f145_on_ui(&weak, |ui| ui.get_search_tab()).unwrap_or(-1);
        f217_log(format!(
            "tab memory ui={live_tab} stored={stored} expected={}",
            bilibili::SEARCH_TAB_BANGUMI
        ));
        f217_log("done");
    });
}
