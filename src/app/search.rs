//! 搜索: query, 热搜/联想 and the result/users pages.

use crate::*;

/// Total pages the search bar shows: `ceil(num_results / page size)`, capped by
/// the API's own page count so a page size the endpoint ignored cannot page
/// past its end. Out-of-range page sizes (the endpoint's default applies) keep
/// the API's own count.
pub(crate) fn search_total(num_results: i64, num_pages: i64, size: i64) -> i64 {
    let api = num_pages.max(1);
    match bilibili::page_size_param(size) {
        Some(size) if num_results > 0 => ((num_results + size - 1) / size).clamp(1, api),
        _ => api,
    }
}

/// Total pages for one tab.
///
/// 番剧 / 影视 / 直播 indexes do not deep-page: they answer a short page list
/// and an out-of-range `page` comes back empty. For those the API's own
/// `num_pages` is the only truth, so the bar stops at the last page it can
/// actually fetch instead of deriving a bigger count from `num_results`.
pub(crate) fn search_total_for(
    kind: bilibili::SearchKind,
    num_results: i64,
    num_pages: i64,
    size: i64,
) -> i64 {
    if kind.shallow_paging() {
        num_pages.max(1)
    } else {
        search_total(num_results, num_pages, size)
    }
}

/// The status line for a failed search: a network failure gets a sentence the
/// user can act on, everything else keeps the API's own message (a risk-control
/// refusal, a non-zero code, a malformed body).
pub(crate) fn search_error_text(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let offline = lower.contains("error sending request")
        || lower.contains("connection")
        || lower.contains("dns error")
        || lower.contains("timed out")
        || lower.contains("network");
    if offline {
        "无法连接 B站，检查网络后重试".to_string()
    } else if message.starts_with("搜索失败") {
        // The search layer's own sentences already carry the prefix.
        message.to_string()
    } else {
        format!("搜索失败：{message}")
    }
}

/// Log one reading of the search field's live rect (bar-local x/width plus the
/// window-space x/right, so the table is readable against the window).
///
/// The values come from `HomeBar`'s published `morph` geometry, i.e. the
/// animation's actual rect at the instant of the call.
pub(crate) fn log_search_field(ui: &AppWindow, label: &str, start: Instant) {
    let x = ui.get_qa_search_field_x();
    let width = ui.get_qa_search_field_w();
    let bar_w = ui.get_home_bar_w();
    let window_w = ui.window().size().width as f32;
    let bar_x = (window_w - bar_w) / 2.0;
    crate::debug_log(&format!(
        "f133 search state={label} t_ms={} field_x={x:.2} field_w={width:.2} window_x={:.2} window_right={:.2} bar_x={bar_x:.2} bar_w={bar_w:.2} window_w={window_w:.2} open={}",
        start.elapsed().as_millis(),
        bar_x + x,
        bar_x + x + width,
        u8::from(ui.get_search_open()),
    ));
}

#[derive(Default)]
pub(crate) struct SearchState {
    pub(crate) keyword: String,
    pub(crate) tab: i32,
    pub(crate) page: i64,
    /// Page count the API last reported (the bar's `N / M` is derived from the
    /// result count and the page size, capped by this).
    pub(crate) num_pages: i64,
    /// Owner mids of the rows currently on screen, aligned with `search_bvids`
    /// (the 用户 tab carries mids and no bvids). A card's UP name resolves
    /// through this, never through another view's list.
    pub(crate) mids: Vec<i64>,
    /// `season_id` of the 番剧/影视 rows on screen, aligned with `search_media_items`.
    /// A click opens that season through the PGC entry (`continue.json` +
    /// 选集), exactly like the 番剧 page's own posters.
    pub(crate) season_ids: Vec<i64>,
    /// Rooms of the 直播 rows on screen, aligned with `search_live_items`. A
    /// click opens the live player with this payload.
    pub(crate) rooms: Vec<bilibili::LiveRoom>,
}

impl SearchState {
    /// The tab this state was last loaded for.
    pub(crate) fn kind(&self) -> bilibili::SearchKind {
        bilibili::SearchKind::from_tab(self.tab)
    }

    /// Forget everything that belongs to the previous tab (a tab switch may
    /// not leave a season / room list behind that its own rows do not have).
    pub(crate) fn clear_targets(&mut self) {
        self.mids.clear();
        self.season_ids.clear();
        self.rooms.clear();
    }
}

pub(crate) struct SearchRowData {
    pub(crate) pixels: Option<(Vec<u8>, u32, u32)>,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) sub: String,
    pub(crate) badge: String,
    /// The home card's own fields, so a search result is not a second layout:
    /// UP name (clickable), exact play count for the heat bulb, and the publish
    /// stamp for the freshness chip.
    pub(crate) owner: String,
    pub(crate) owner_mid: i64,
    pub(crate) view_count: u64,
    pub(crate) pub_ts: i64,
}

/// One 番剧 / 影视 search poster, before it becomes a Slint `PgcCard` (Slint
/// models are not `Send`, so the worker publishes plain data and the UI thread
/// builds the cards).
pub(crate) struct MediaRowData {
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) badge: String,
    pub(crate) follows: String,
}

/// One 直播 search card, same split as [`MediaRowData`].
pub(crate) struct LiveRowData {
    pub(crate) room_id: i32,
    pub(crate) title: String,
    pub(crate) uname: String,
    pub(crate) area: String,
    pub(crate) online: String,
    pub(crate) live: bool,
}

pub(crate) struct SearchOutcome {
    pub(crate) kind: bilibili::SearchKind,
    pub(crate) page: i64,
    pub(crate) num_pages: i64,
    pub(crate) num_results: i64,
    /// Another page exists (the API's own page count).
    pub(crate) has_more: bool,
    /// 视频 / 用户 cards: `(cover url, row)`.
    pub(crate) rows: Vec<(String, SearchRowData)>,
    /// 番剧 / 影视 posters: `(cover url, card)`.
    pub(crate) posters: Vec<(String, MediaRowData)>,
    /// 直播 cards: `(cover url, card)`.
    pub(crate) rooms: Vec<(String, LiveRowData)>,
    pub(crate) bvids: Vec<String>,
    /// Owner mids aligned with `rows`; the 用户 tab fills it with every row.
    pub(crate) mids: Vec<i64>,
    /// Season ids aligned with `posters`.
    pub(crate) season_ids: Vec<i64>,
    /// Room payloads aligned with `rooms`.
    pub(crate) live_rooms: Vec<bilibili::LiveRoom>,
}

impl SearchOutcome {
    /// The cover URLs of the list this tab shows.
    pub(crate) fn pics(&self) -> Vec<String> {
        match self.kind {
            bilibili::SearchKind::Video | bilibili::SearchKind::User => {
                self.rows.iter().map(|(url, _)| url.clone()).collect()
            }
            bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film => {
                self.posters.iter().map(|(url, _)| url.clone()).collect()
            }
            bilibili::SearchKind::Live => {
                self.rooms.iter().map(|(url, _)| url.clone()).collect()
            }
        }
    }

    /// How many cards this tab published.
    pub(crate) fn loaded(&self) -> usize {
        match self.kind {
            bilibili::SearchKind::Video | bilibili::SearchKind::User => self.rows.len(),
            bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film => self.posters.len(),
            bilibili::SearchKind::Live => self.rooms.len(),
        }
    }
}

/// The tab a new search starts on: whatever the search page last showed (the
/// value restored from `settings.json` at startup or picked since), 视频 when
/// there is no window yet.
pub(crate) fn current_search_tab(weak: &Weak<AppWindow>) -> i32 {
    weak.upgrade()
        .map(|ui| ui.get_search_tab())
        .unwrap_or(bilibili::SEARCH_TAB_VIDEO)
}

/// Map one page of API hits onto the rows the three result grids render.
///
/// Every tab fills its own list and leaves the other two empty: the three
/// models are published together, so a stale poster can never sit under the
/// 直播 tab. The target lists (`bvids` / `mids` / `season_ids` / `live_rooms`)
/// are built in the same order as the cards, which is what makes a click
/// resolve to the row it was on.
pub(crate) fn search_outcome(
    kind: bilibili::SearchKind,
    page: bilibili::SearchHitPage,
) -> SearchOutcome {
    let mut rows: Vec<(String, SearchRowData)> = Vec::new();
    let mut posters: Vec<(String, MediaRowData)> = Vec::new();
    let mut rooms: Vec<(String, LiveRowData)> = Vec::new();
    let mut bvids: Vec<String> = Vec::new();
    let mut mids: Vec<i64> = Vec::new();
    let mut season_ids: Vec<i64> = Vec::new();
    let mut live_rooms: Vec<bilibili::LiveRoom> = Vec::new();
    // One line per PGC page: the season/media ids the index answered plus the
    // classification the poster does not print (styles / areas / 角标), which
    // is how a 番剧 page is told from a 国创 / 影视 one in a log.
    let mut media_note: Vec<String> = Vec::new();
    let has_more = page.has_more();
    let (page_no, num_pages, num_results) = (page.page, page.num_pages, page.num_results);
    for hit in page.items {
        match hit {
            bilibili::SearchHit::Video(item) => {
                let meta = format!(
                    "{} · {}播放 · {}",
                    item.author,
                    format_view(item.play),
                    item.duration
                );
                rows.push((
                    item.pic.clone(),
                    SearchRowData {
                        pixels: None,
                        title: item.title.clone(),
                        meta,
                        sub: item.description.clone(),
                        badge: item.duration.clone(),
                        owner: item.author.clone(),
                        owner_mid: item.mid,
                        view_count: item.play,
                        pub_ts: item.pub_ts,
                    },
                ));
                mids.push(item.mid);
                bvids.push(item.bvid);
            }
            bilibili::SearchHit::User(item) => {
                let meta = format!("粉丝 {} · 投稿 {}", format_view(item.fans), item.videos);
                rows.push((
                    item.upic.clone(),
                    SearchRowData {
                        pixels: None,
                        title: item.uname.clone(),
                        meta,
                        sub: item.usign.clone(),
                        badge: String::new(),
                        owner: String::new(),
                        owner_mid: item.mid,
                        view_count: 0,
                        pub_ts: 0,
                    },
                ));
                mids.push(item.mid);
            }
            bilibili::SearchHit::Media(item) => {
                // Line 2 of the poster card: the score, then the site's own
                // episode phrase (`全1005话`), else the raw count.
                let mut parts: Vec<String> = Vec::new();
                if item.score > 0.0 {
                    parts.push(format!("{} 分", item.score_text));
                } else {
                    parts.push("暂无评分".to_string());
                }
                if !item.index_show.trim().is_empty() {
                    parts.push(item.index_show.trim().to_string());
                } else if item.eps > 0 {
                    parts.push(format!("{} 集", item.eps));
                }
                let meta = parts.join(" · ");
                let label = if item.season_type_name.trim().is_empty() {
                    kind.label().to_string()
                } else {
                    item.season_type_name.clone()
                };
                // The poster's bottom-right line: how many people rated it (the
                // PGC index sends `media_score.user_count`, not a 追番 count).
                let follows = if item.follow > 0 {
                    format!("{} 人评分", format_view(item.follow))
                } else {
                    String::new()
                };
                posters.push((
                    item.cover.clone(),
                    MediaRowData {
                        title: item.title.clone(),
                        meta,
                        badge: label,
                        follows,
                    },
                ));
                season_ids.push(item.season_id);
                if media_note.len() < 3 {
                    media_note.push(format!(
                        "01:{} season={} media={} type={} styles={} areas={} badge={} hl={}",
                        item.title,
                        item.season_id,
                        item.media_id,
                        item.season_type_name,
                        item.styles,
                        item.areas,
                        item.badge,
                        item.highlights.len()
                    ));
                }
            }
            bilibili::SearchHit::Live(item) => {
                rooms.push((
                    item.cover.clone(),
                    LiveRowData {
                        room_id: saturating_i32(item.room_id),
                        title: item.title.clone(),
                        uname: item.uname.clone(),
                        area: item.area_name.clone(),
                        online: format!("{} 人气", live_online_text(item.online)),
                        live: item.live_status == 1,
                    },
                ));
                live_rooms.push(item.to_room());
            }
        }
    }
    if !media_note.is_empty() {
        crate::debug_log(&format!(
            "search media page={page_no} first={}",
            media_note.join(" | ")
        ));
    }
    SearchOutcome {
        kind,
        page: page_no,
        num_pages,
        num_results,
        has_more,
        rows,
        posters,
        rooms,
        bvids,
        mids,
        season_ids,
        live_rooms,
    }
}


pub(crate) fn begin_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_seq: Arc<AtomicU64>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    hot_keywords: Arc<Mutex<Vec<String>>>,
    keyword: String,
    tab: i32,
) {
    let keyword = keyword.trim().to_string();
    if keyword.is_empty() {
        return;
    }
    // Canonical round-trip: a stale/hand-edited index becomes the tab the UI
    // actually has (0 when it is out of range).
    let kind = bilibili::SearchKind::from_tab(tab);
    let tab = kind.tab();
    if let Ok(mut state) = search_state.lock() {
        state.keyword = keyword.clone();
        state.tab = tab;
        state.page = 1;
        state.num_pages = 1;
        state.clear_targets();
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_search_text(keyword.clone().into());
        ui.set_view_mode(1);
        enter_paged_view(&ui);
        ui.set_search_tab(tab);
        ui.set_search_status(String::new().into());
        // A committed search is exactly what the field's Enter does: the text
        // lands in the box and the popup under the bar goes away. Clicking a
        // hot keyword or a 联想 used to stop one step short (it ran the search
        // but left the panel open over the results), so both paths now close
        // here instead of at each call site.
        ui.set_search_open(false);
        ui.set_suggest_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<SharedString>::new(),
        ))));
    }
    run_search(
        weak.clone(),
        session_slot.clone(),
        search_state,
        search_seq,
        search_bvids,
    );
    load_hot_search(weak, session_slot, hot_keywords);
}

/// One screen of empty cards for the tab about to load.
///
/// A switch clears the old rows first: keeping them would show the previous
/// tab's covers under the new tab's labels for the whole request. The empty
/// rows are exactly `list_page_size` long, so the grid paints its own
/// `SkeletonPlate`s (`thumb` is empty) in the shape the results will land in.
pub(crate) fn search_show_skeleton(ui: &AppWindow) {
    let cards = list_page_size(ui).max(1) as usize;
    let empty_rows: Vec<SearchRow> = (0..cards).map(|_| SearchRow::default()).collect();
    let empty_posters: Vec<PgcCard> = (0..cards).map(|_| PgcCard::default()).collect();
    let empty_rooms: Vec<LiveCard> = (0..cards).map(|_| LiveCard::default()).collect();
    ui.set_search_items(ModelRc::from(std::rc::Rc::new(VecModel::from(empty_rows))));
    ui.set_search_media_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
        empty_posters,
    ))));
    ui.set_search_live_items(ModelRc::from(std::rc::Rc::new(VecModel::from(empty_rooms))));
    skeleton_arm(ui, cards as u64);
}

/// Switch the search tab (视频 / 番剧 / 影视 / 直播 / 用户).
///
/// The keyword is kept, the old rows are dropped for the skeleton, the choice
/// is written to `settings.json` (so a restart opens the same tab), and the
/// tab's own endpoint is queried from page 1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_switch_tab(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_seq: Arc<AtomicU64>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    tab: i32,
) {
    let kind = bilibili::SearchKind::from_tab(tab);
    let tab = kind.tab();
    let (keyword, previous) = match search_state.lock() {
        Ok(mut state) => {
            let previous = state.tab;
            state.tab = tab;
            state.page = 1;
            state.num_pages = 1;
            state.clear_targets();
            (state.keyword.clone(), previous)
        }
        Err(_) => return,
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_search_tab(tab);
        ui.set_search_status(String::new().into());
        set_client_page(1, 0);
        search_show_skeleton(&ui);
    }
    let snapshot = update_settings(&settings_store, |prefs| prefs.search_tab = tab);
    crate::debug_log(&format!(
        "search tab switch from={previous} to={tab} ({}) keyword={keyword:?} stored={}",
        bilibili::SearchKind::from_tab(tab).label(),
        snapshot.search_tab
    ));
    if keyword.trim().is_empty() {
        if let Some(ui) = weak.upgrade() {
            ui.set_search_status("输入关键词开始搜索".into());
            publish_refresh(&ui, false, 1.0);
        }
        return;
    }
    run_search(
        weak,
        session_slot,
        search_state,
        search_seq,
        search_bvids,
    );
}

pub(crate) fn active_parts(session_slot: &SessionSlot) -> Option<(Arc<bilibili::Session>, i64)> {
    let session = active_session(session_slot)?;
    let mid = accounts::load().map(|account| account.mid).unwrap_or(0);
    Some((session, mid))
}

/// Open the UP page for a search 用户 row.
///
/// The 用户 tab's rows carry mids and no bvids, so both the grid's activation
/// callback and a card click on that tab land here. `false` means the row is not
/// a user row (another tab, stale index) and the caller keeps its own path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_search_user(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    index: i32,
    origin: &str,
) -> bool {
    let mid = search_state.lock().ok().and_then(|state| {
        if state.tab != bilibili::SEARCH_TAB_USER {
            return None;
        }
        usize::try_from(index)
            .ok()
            .and_then(|index| state.mids.get(index).copied())
            .filter(|mid| *mid > 0)
    });
    let Some(mid) = mid else {
        return false;
    };
    crate::debug_log(&format!(
        "up open index={index} source=search-{origin} mid={mid}"
    ));
    open_up_mid(weak, session_slot, up_page, up_seq, up_store, mid);
    true
}

/// Open search result `index` through `search_bvids`, the list the result grid
/// was built from, by resolving it into the detail API (which starts the
/// player).
///
/// One implementation for both entry points — the grid's card click
/// (`card_open`, origin `card`) and the stock activation callback
/// (`open_search_item`, origin `activate`) — so a click can never disagree with
/// the row it was on. `false` means nothing was opened: no video rows (another
/// tab), a stale index, or no session.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_search_index(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    detail_state: Arc<Mutex<Option<DetailState>>>,
    detail_seq: Arc<AtomicU64>,
    related_bvids: Arc<Mutex<Vec<String>>>,
    index: i32,
    origin: &str,
) -> bool {
    let tab = search_state.lock().map(|state| state.tab).unwrap_or(0);
    if tab != bilibili::SEARCH_TAB_VIDEO {
        crate::debug_log(&format!(
            "search open ignored origin={origin} index={index} tab={tab}"
        ));
        return false;
    }
    let index = index.max(0) as usize;
    let bvid = search_bvids
        .lock()
        .ok()
        .and_then(|guard| guard.get(index).cloned());
    let Some(bvid) = bvid else {
        return false;
    };
    crate::debug_log(&format!(
        "search open origin={origin} index={index} bvid={bvid}"
    ));
    open_detail(
        weak,
        session_slot,
        detail_state,
        detail_seq,
        related_bvids,
        bvid,
    );
    true
}

/// Open the 番剧 / 影视 result at `index`: the season goes through the PGC
/// entry the 番剧 page uses, so a season that was already started resumes on
/// the stored episode (`continue.json`) and the player's 选集 is the whole
/// season. `false` means the row is not a season.
pub(crate) fn open_search_season(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    index: i32,
    origin: &str,
) -> bool {
    let season_id = search_state.lock().ok().and_then(|state| {
        if !matches!(
            state.kind(),
            bilibili::SearchKind::Bangumi | bilibili::SearchKind::Film
        ) {
            return None;
        }
        usize::try_from(index)
            .ok()
            .and_then(|index| state.season_ids.get(index).copied())
            .filter(|season_id| *season_id > 0)
    });
    let Some(season_id) = season_id else {
        return false;
    };
    crate::debug_log(&format!(
        "search season open origin={origin} index={index} season={season_id}"
    ));
    // 继续观看: the same entry the 番剧 grid's poster uses. Nothing is stored for
    // a season watched for the first time, so it opens on its first episode.
    let resume = pgc_progress_entry(season_id).filter(|entry| entry.ep_id > 0);
    pgc_open_season(weak, session_slot, season_id, resume);
    true
}

/// Open the 直播 result at `index` in the live player. `false` means the row is
/// not a room.
pub(crate) fn open_search_live(
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    player_state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    index: i32,
    origin: &str,
) -> bool {
    let room = search_state.lock().ok().and_then(|state| {
        if state.tab != bilibili::SEARCH_TAB_LIVE {
            return None;
        }
        usize::try_from(index)
            .ok()
            .and_then(|index| state.rooms.get(index).cloned())
            .filter(|room| room.room_id > 0)
    });
    let Some(room) = room else {
        return false;
    };
    let Some(session) = active_session(&session_slot) else {
        crate::debug_log(&format!(
            "search live open skipped origin={origin} index={index} reason=no-session"
        ));
        return false;
    };
    crate::debug_log(&format!(
        "search live open origin={origin} index={index} room={} uname={}",
        room.room_id, room.uname
    ));
    open_live_player(session, room, player_state, settings_store);
    true
}

/// One entry point for a click on a search result, whichever tab is on screen.
///
/// Order matters: the user / season / live rows are only present on their own
/// tab, so each helper declines on the others and the video path is the
/// fallback. `false` means the index did not resolve (stale row, no session).
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_search_hit(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    detail_state: Arc<Mutex<Option<DetailState>>>,
    detail_seq: Arc<AtomicU64>,
    related_bvids: Arc<Mutex<Vec<String>>>,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    player_state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
    index: i32,
    origin: &str,
) -> bool {
    if open_search_user(
        weak.clone(),
        session_slot.clone(),
        search_state.clone(),
        up_page,
        up_seq,
        up_store,
        index,
        origin,
    ) {
        return true;
    }
    if open_search_season(
        weak.clone(),
        session_slot.clone(),
        search_state.clone(),
        index,
        origin,
    ) {
        return true;
    }
    if open_search_live(
        session_slot.clone(),
        search_state.clone(),
        player_state,
        settings_store,
        index,
        origin,
    ) {
        return true;
    }
    open_search_index(
        weak,
        session_slot,
        search_state,
        search_bvids,
        detail_state,
        detail_seq,
        related_bvids,
        index,
        origin,
    )
}

/// Wrap a player-key mapping as a Slint model for the `[int]` maps.
pub(crate) fn key_map_model(keys: &[u32]) -> ModelRc<i32> {
    let rows: Vec<i32> = keys.iter().map(|key| *key as i32).collect();
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}

#[cfg(test)]
mod tests {
    use super::{search_error_text, search_total_for};
    use crate::bilibili::SearchKind;

    /// 番剧 / 影视 / 直播: the API's own page count is the only truth, even when
    /// `num_results` would suggest more pages.
    #[test]
    fn shallow_tabs_stop_at_the_api_page_count() {
        assert_eq!(search_total_for(SearchKind::Bangumi, 5, 1, 24), 1);
        assert_eq!(search_total_for(SearchKind::Film, 1, 1, 24), 1);
        assert_eq!(search_total_for(SearchKind::Live, 759, 32, 24), 32);
        // An index that omits the paging metadata has exactly one page.
        assert_eq!(search_total_for(SearchKind::Bangumi, 0, 0, 24), 1);
        // The video index still derives its pages from the result count.
        assert_eq!(search_total_for(SearchKind::Video, 1000, 42, 24), 42);
    }

    #[test]
    fn network_failures_read_as_a_sentence() {
        assert_eq!(
            search_error_text("error sending request for url (https://api.bilibili.com/x/web-interface/nav)"),
            "无法连接 B站，检查网络后重试"
        );
        assert_eq!(
            search_error_text("搜索被 B站 风控拦截，请稍后重试（或登录后再搜）"),
            "搜索失败：搜索被 B站 风控拦截，请稍后重试（或登录后再搜）"
        );
        assert_eq!(
            search_error_text("搜索失败：响应缺少 data"),
            "搜索失败：响应缺少 data"
        );
        assert_eq!(
            search_error_text("响应解析失败（HTTP 200）：missing field `result`"),
            "搜索失败：响应解析失败（HTTP 200）：missing field `result`"
        );
    }
}
