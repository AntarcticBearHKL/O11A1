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

pub(crate) struct SearchOutcome {
    pub(crate) page: i64,
    pub(crate) num_pages: i64,
    pub(crate) num_results: i64,
    pub(crate) rows: Vec<(String, SearchRowData)>,
    pub(crate) bvids: Vec<String>,
    /// Owner mids aligned with `rows`; the 用户 tab fills it with every row.
    pub(crate) mids: Vec<i64>,
}

/// Start a fresh video search: switch to the results view, clear suggestions
/// and refresh the 热搜 strip.
pub(crate) fn begin_search(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    search_state: Arc<Mutex<SearchState>>,
    search_seq: Arc<AtomicU64>,
    search_bvids: Arc<Mutex<Vec<String>>>,
    hot_keywords: Arc<Mutex<Vec<String>>>,
    keyword: String,
) {
    let keyword = keyword.trim().to_string();
    if keyword.is_empty() {
        return;
    }
    if let Ok(mut state) = search_state.lock() {
        state.keyword = keyword.clone();
        state.tab = 0;
        state.page = 1;
        state.num_pages = 1;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_search_text(keyword.clone().into());
        ui.set_view_mode(1);
        enter_paged_view(&ui);
        ui.set_search_tab(0);
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

pub(crate) fn active_parts(session_slot: &SessionSlot) -> Option<(Arc<bilibili::Session>, i64)> {
    let session = active_session(session_slot)?;
    let mid = accounts::load().map(|account| account.mid).unwrap_or(0);
    Some((session, mid))
}

/// Open the UP page for a search 用户 row.
///
/// The 用户 tab's rows carry mids and no bvids, so both the grid's activation
/// callback and a card click on that tab land here. `false` means the row is not
/// a user row (视频 tab, stale index) and the caller keeps its own path.
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
        if state.tab != 1 {
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
/// the row it was on. `false` means nothing was opened: no video rows (the 用户
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
    if tab != 0 {
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

/// Wrap a player-key mapping as a Slint model for the `[int]` maps.
pub(crate) fn key_map_model(keys: &[u32]) -> ModelRc<i32> {
    let rows: Vec<i32> = keys.iter().map(|key| *key as i32).collect();
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}
