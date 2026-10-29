//! 番剧: the PGC page, 排序/继续观看 state and the series picker.

use crate::*;

/// Row height of one pickable 选集 row; the auto-scroll offset is derived from
/// it, so the list lands on the current row when the panel opens.
pub(crate) const SERIES_ROW_H: f32 = 44.0;

/// Turn one `video_view` result into the player's 选集 model.
///
/// 合集 wins when `ugc_season` parsed (section header rows + one row per
/// episode); otherwise more than one 分P becomes the list; a single-page video
/// is `kind = 0` and leaves the toolbar cell dim. `current` matches by cid (and
/// by bvid inside a 合集).
pub(crate) fn series_from_detail(detail: &bilibili::VideoDetail, current_cid: u64, open_bvid: &str) -> SeriesState {
    if let Some(season) = detail.season.as_ref() {
        let mut state = SeriesState {
            kind: 2,
            title: if season.title.is_empty() {
                "合集".to_string()
            } else {
                season.title.clone()
            },
            ..SeriesState::default()
        };
        let mut number = 0usize;
        for section in &season.sections {
            let mut header = SeriesRow::default();
            header.header = true;
            header.title = SharedString::from(section.title.clone());
            state.rows.push(header);
            state.targets.push(None);
            for episode in &section.episodes {
                number += 1;
                let bvid = if episode.bvid.is_empty() {
                    open_bvid.to_string()
                } else {
                    episode.bvid.clone()
                };
                state.rows.push(SeriesRow {
                    label: SharedString::from(format!("{number:02}")),
                    title: SharedString::from(if episode.title.is_empty() {
                        format!("第 {number} 集")
                    } else {
                        episode.title.clone()
                    }),
                    duration_text: SharedString::from(if episode.duration > 0 {
                        format_duration(episode.duration)
                    } else {
                        String::new()
                    }),
                    current: episode.cid as u64 == current_cid
                        && (episode.bvid.is_empty() || episode.bvid == open_bvid),
                    header: false,
                });
                state.targets.push(Some(SeriesTarget {
                    bvid,
                    cid: episode.cid.max(0) as u64,
                    aid: episode.aid,
                    title: if episode.title.is_empty() {
                        format!("第 {number} 集")
                    } else {
                        episode.title.clone()
                    },
                    ep_id: 0,
                }));
            }
        }
        // `ep_count` is the site's own total; it can exceed the rows kept after
        // the parser's cap, so it is only used as the header's count.
        state.count = if season.ep_count > 0 {
            season.ep_count.min(i32::MAX as u32) as i32
        } else {
            number as i32
        };
        return state;
    }
    if detail.pages.len() > 1 {
        let mut state = SeriesState {
            kind: 1,
            title: format!("分P · {} 集", detail.pages.len()),
            count: detail.pages.len() as i32,
            ..SeriesState::default()
        };
        for part in &detail.pages {
            state.rows.push(SeriesRow {
                label: SharedString::from(format!("P{}", part.page.max(1))),
                title: SharedString::from(part.title.clone()),
                duration_text: SharedString::from(if part.duration > 0 {
                    format_duration(part.duration)
                } else {
                    String::new()
                }),
                current: part.cid as u64 == current_cid,
                header: false,
            });
            state.targets.push(Some(SeriesTarget {
                bvid: open_bvid.to_string(),
                cid: part.cid.max(0) as u64,
                aid: detail.aid,
                title: part.title.clone(),
                ep_id: 0,
            }));
        }
        return state;
    }
    SeriesState::default()
}

/// The 番剧 选集 list: `episodes` first (the 正片), then each `section` group.
/// Section titles become non-clickable header rows; `current_ep` (an ep_id)
/// marks the episode playing now.
pub(crate) fn series_from_pgc(season: &bilibili::PgcSeason, current_ep: i64) -> SeriesState {
    let mut state = SeriesState {
        kind: 3,
        title: if season.title.trim().is_empty() {
            "番剧".to_string()
        } else {
            format!("番剧 · {}", season.title.trim())
        },
        ..SeriesState::default()
    };
    let mut number = 0usize;
    if !season.episodes.is_empty() {
        // A header only helps when the list also has section groups; a plain
        // season is one unlabelled run of episodes.
        if !season.sections.is_empty() {
            let mut header = SeriesRow::default();
            header.header = true;
            header.title = SharedString::from("正片");
            state.rows.push(header);
            state.targets.push(None);
        }
        for episode in &season.episodes {
            number += 1;
            push_pgc_episode(&mut state, episode, current_ep, number);
        }
    }
    for section in &season.sections {
        let mut header = SeriesRow::default();
        header.header = true;
        header.title = SharedString::from(if section.title.trim().is_empty() {
            "其他".to_string()
        } else {
            section.title.trim().to_string()
        });
        state.rows.push(header);
        state.targets.push(None);
        for episode in &section.episodes {
            number += 1;
            push_pgc_episode(&mut state, episode, current_ep, number);
        }
    }
    // The header counts the 正片, not the PV/花絮 rows: "67 集" over a 34-episode
    // season reads as a bug. Sections stay in the list below.
    state.count = if season.episodes.is_empty() {
        number as i32
    } else {
        season.episodes.len() as i32
    };
    state
}

pub(crate) fn push_pgc_episode(
    state: &mut SeriesState,
    episode: &bilibili::PgcEpisode,
    current_ep: i64,
    number: usize,
) {
    let title = if !episode.long_title.trim().is_empty() {
        episode.long_title.trim().to_string()
    } else if !episode.title.trim().is_empty()
        && episode
            .title
            .trim()
            .chars()
            .all(|ch| ch.is_ascii_digit())
    {
        format!("第 {} 话", episode.title.trim())
    } else {
        episode.display_title()
    };
    state.rows.push(SeriesRow {
        label: SharedString::from(format!("{number:02}")),
        title: SharedString::from(title),
        duration_text: SharedString::from(if episode.duration_ms > 0 {
            format_duration(episode.duration_secs())
        } else {
            String::new()
        }),
        current: episode.ep_id > 0 && episode.ep_id == current_ep,
        header: false,
    });
    state.targets.push(Some(SeriesTarget {
        bvid: episode.bvid.clone(),
        cid: episode.cid,
        aid: episode.aid,
        title: episode.display_title(),
        ep_id: episode.ep_id,
    }));
}

/// The real switch shared by every 选集 pick (ptype 1/2 and the gated 番剧
/// path). Split out of the callback so the PGC branch can run its region check
/// on a worker and call exactly the same code on success.
pub(crate) fn apply_series_pick(
    window: &PlayerWindow,
    series_state: &Arc<Mutex<SeriesState>>,
    control: &Arc<player::PlaybackControl>,
    comments: &CommentsSlot,
    session: &Arc<bilibili::Session>,
    open_bvid: &str,
    index: usize,
    target: SeriesTarget,
    kind: i32,
) {
    let from = control.source();
    let to_bvid = if target.bvid.is_empty() {
        open_bvid.to_string()
    } else {
        target.bvid.clone()
    };
    crate::debug_log(&format!(
        "series switch from={} to={to_bvid} cid={} kind={kind}",
        from.bvid, target.cid
    ));
    if !control.switch_to(
        to_bvid,
        target.cid,
        target.aid,
        target.title.clone(),
    ) {
        crate::debug_log(&format!(
            "series switch ignored cid={} reason=same-or-closed",
            target.cid
        ));
        return;
    }
    // Title strip + info header follow the episode immediately; the picture
    // stays the old one (frozen) until the new first frame.
    window.set_video_title(target.title.clone().into());
    window.set_info_title(target.title.clone().into());
    // 选集 list: the highlight moves to the picked row, the header keeps the
    // collection name (分P keeps `分P · N 集`).
    {
        let mut state = match series_state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        for (row_index, row) in state.rows.iter_mut().enumerate() {
            if !row.header {
                row.current = row_index == index;
            }
        }
        let rows = state.rows.clone();
        let title = state.title.clone();
        let scroll = -(index as f32 * SERIES_ROW_H - SERIES_ROW_H).max(0.0);
        let kind = state.kind;
        drop(state);
        window.set_series_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        window.set_series_title(title.into());
        window.set_series_scroll(scroll);
        crate::debug_log(&format!(
            "series list highlight row={index} kind={kind} current_cid={} scroll={scroll:.1}",
            target.cid
        ));
    }
    // 评论 belongs to the new archive: reset the slot and refetch.
    if target.aid > 0 {
        if let Ok(mut guard) = comments.lock() {
            let ui = guard.ui.clone();
            *guard = CommentsState {
                aid: target.aid,
                sort_hot: true,
                ui,
                ..CommentsState::default()
            };
        }
        publish_comments(comments);
        on_comments_load(session.clone(), comments.clone(), target.aid, true);
    } else {
        if let Ok(mut guard) = comments.lock() {
            guard.aid = 0;
        }
        publish_comments(comments);
        crate::debug_log("series switch comments skipped aid=0");
    }
}

/// Scroll offset (negative = down) that puts the current row one row below the
/// header. `0` when nothing is current.
pub(crate) fn series_scroll_for(state: &SeriesState) -> f32 {
    let index = state
        .rows
        .iter()
        .position(|row| row.current)
        .unwrap_or(0) as f32;
    -(index * SERIES_ROW_H - SERIES_ROW_H).max(0.0)
}

pub(crate) fn install_pgc_page(ui: &Weak<AppWindow>, host: &PgcHost) {
    PGC_PAGE.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = Some(PgcPageHost {
                ui: ui.clone(),
                host: host.clone(),
            });
        }
    });
}

pub(crate) const PGC_SEASON_TYPES: [i64; 6] = [1, 4, 2, 3, 5, 7];

pub(crate) const PGC_PAGE_SIZE: i64 = 20;

/// The 分区索引 sorts the page offers: `(settings key, dropdown label, API
/// order)`, in dropdown order. Only values whose meaning was proven against the
/// live endpoint are here; see [`settings::PGC_SORT_KEYS`] and `PGC_INDEX_URL`
/// in `bilibili.rs` for the evidence table.
pub(crate) const PGC_SORTS: [(&str, &str, i64); 4] = [
    ("follows", "追番最多", 3),
    ("update", "最近更新", 0),
    ("views", "播放最多", 2),
    ("score", "评分最高", 4),
];

pub(crate) fn pgc_sort_labels() -> Vec<SharedString> {
    PGC_SORTS
        .iter()
        .map(|(_, label, _)| SharedString::from(*label))
        .collect()
}

/// Dropdown index of a stored key (`0` = 追番最多, also the fallback).
pub(crate) fn pgc_sort_index(key: &str) -> i32 {
    PGC_SORTS
        .iter()
        .position(|(candidate, _, _)| *candidate == key.trim())
        .unwrap_or(0) as i32
}

pub(crate) fn pgc_sort_key_at(index: i32) -> &'static str {
    PGC_SORTS
        .get(index.max(0) as usize)
        .map(|(key, _, _)| *key)
        .unwrap_or(PGC_SORTS[0].0)
}

/// The API `order` behind a settings key; an unknown key is 追番最多.
pub(crate) fn pgc_sort_order(key: &str) -> i64 {
    PGC_SORTS
        .iter()
        .find(|(candidate, _, _)| *candidate == key.trim())
        .map(|(_, _, order)| *order)
        .unwrap_or(PGC_SORTS[0].2)
}

/// The sort this tab was last left on, straight from `settings.json`. A missing
/// or corrupt entry is 追番最多, i.e. exactly the pre-existing behaviour.
pub(crate) fn pgc_stored_sort(prefs: &settings::Settings, season_type: i64) -> String {
    let key = prefs
        .pgc_sorts
        .get(&season_type.to_string())
        .map(String::as_str)
        .unwrap_or(settings::PGC_SORT_DEFAULT);
    pgc_sort_key_at(pgc_sort_index(key)).to_string()
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

pub(crate) fn pgc_tab_labels() -> Vec<SharedString> {
    ["每日放送", "番剧", "国创", "电影", "纪录片", "电视剧", "综艺"]
        .into_iter()
        .map(SharedString::from)
        .collect()
}

pub(crate) fn pgc_weekday(day_of_week: i64) -> &'static str {
    match day_of_week {
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        6 => "周六",
        _ => "周日",
    }
}

/// The season type behind a tab (tab 0 has none).
pub(crate) fn pgc_season_type(tab: i32) -> Option<i64> {
    if tab <= 0 {
        return None;
    }
    PGC_SEASON_TYPES.get((tab - 1) as usize).copied()
}

/// The loaded 继续观看 table. Loaded once, on first use.
pub(crate) fn continue_store() -> Arc<Mutex<settings::ContinueStore>> {
    static STORE: OnceLock<Arc<Mutex<settings::ContinueStore>>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let store = settings::ContinueStore::load();
            crate::debug_log(&format!(
                "pgc progress load path={} seasons={}",
                settings::ContinueStore::path().display(),
                store.len()
            ));
            Arc::new(Mutex::new(store))
        })
        .clone()
}

/// The stored row for one season, if any.
pub(crate) fn pgc_progress_entry(season_id: i64) -> Option<settings::ContinueEntry> {
    continue_store()
        .lock()
        .ok()
        .and_then(|store| store.get(season_id).cloned())
}

/// Record a season's episode as watched and persist the table. Returns the
/// entry written, so the caller can log exactly what landed.
pub(crate) fn pgc_write_progress(
    season_id: i64,
    episode: &bilibili::PgcEpisode,
    index: u32,
) -> Option<settings::ContinueEntry> {
    if season_id <= 0 || episode.ep_id <= 0 {
        return None;
    }
    let entry = settings::ContinueEntry {
        ep_id: episode.ep_id,
        bvid: episode.bvid.clone(),
        cid: episode.cid,
        index,
        title: episode.display_title(),
        at: unix_now(),
    };
    let store = continue_store();
    let saved = match store.lock() {
        Ok(mut store) => store.set_and_save(season_id, entry.clone()).is_ok(),
        Err(_) => false,
    };
    crate::debug_log(&format!(
        "pgc progress write season={season_id} ep={} index={index} cid={} bvid={} saved={saved} at={}",
        entry.ep_id, entry.cid, entry.bvid, entry.at
    ));
    Some(entry)
}

/// Republish the 分区索引 cards so their 「看到第 N 集」 corner follows a write
/// made while a player was on top of this page. UI thread only, and only when
/// that exact model is the one on screen (排行榜 / 每日放送 keep theirs).
pub(crate) fn pgc_refresh_badges() {
    let Some(page) = PGC_PAGE.with(|cell| cell.borrow().clone()) else {
        return;
    };
    let Some(ui) = page.ui.upgrade() else {
        return;
    };
    if ui.get_view_mode() != 22 || ui.get_pgc_tab() == 0 || ui.get_pgc_sub() != 0 {
        return;
    }
    let items = page
        .host
        .state
        .lock()
        .map(|state| state.index_items.clone())
        .unwrap_or_default();
    if items.is_empty() {
        return;
    }
    // Patch the rows already on screen instead of rebuilding them: the covers
    // are streamed into the model after the page lands, and a fresh
    // `PgcCard::default` would drop every one of them.
    let model = ui.get_pgc_items();
    let mut cards: Vec<PgcCard> = (0..model.row_count())
        .filter_map(|index| model.row_data(index))
        .collect();
    let mut marked = 0usize;
    for (index, item) in items.iter().enumerate() {
        let Some(card) = cards.get_mut(index) else {
            break;
        };
        let progress = pgc_progress_entry(item.season_id)
            .map(|entry| entry.badge_text())
            .unwrap_or_default();
        if !progress.is_empty() {
            marked += 1;
        }
        card.progress = progress.into();
    }
    ui.set_pgc_items(ModelRc::from(std::rc::Rc::new(VecModel::from(cards))));
    crate::debug_log(&format!(
        "pgc badge refresh rows={} with_progress={marked}",
        items.len()
    ));
}

/// One index card's sub-line: the site's 集数 text, plus its score when known.
pub(crate) fn pgc_index_meta(item: &bilibili::PgcIndexItem) -> String {
    let show = item.index_show.trim();
    let score = item.score.trim();
    match (show.is_empty(), score.is_empty()) {
        (false, false) => format!("{show} · {score}分"),
        (false, true) => show.to_string(),
        (true, false) => format!("{score}分"),
        (true, true) => "暂无评分".to_string(),
    }
}

pub(crate) fn pgc_card_from_index(item: &bilibili::PgcIndexItem) -> PgcCard {
    // 继续观看: 「看到第 N 集」 comes from the small table, not from the API.
    let progress = pgc_progress_entry(item.season_id)
        .map(|entry| entry.badge_text())
        .unwrap_or_default();
    PgcCard {
        thumb: Image::default(),
        title: item.title.clone().into(),
        meta: pgc_index_meta(item).into(),
        badge: item.badge.clone().into(),
        follows: item.order.clone().into(),
        rank: 0,
        progress: progress.into(),
    }
}

pub(crate) fn pgc_card_from_rank(item: &bilibili::PgcRankItem) -> PgcCard {
    let meta = if item.index_show.trim().is_empty() {
        if item.rating.trim().is_empty() {
            "暂无评分".to_string()
        } else {
            item.rating.clone()
        }
    } else if item.rating.trim().is_empty() {
        item.index_show.clone()
    } else {
        format!("{} · {}", item.index_show, item.rating)
    };
    PgcCard {
        thumb: Image::default(),
        title: item.title.clone().into(),
        meta: meta.into(),
        // The rank list names the same 大会员 badge, but the ranking's own
        // 序号 is the card's marker.
        badge: item.badge.clone().into(),
        follows: if item.follow > 0 {
            format!("{}追番", format_view(item.follow)).into()
        } else {
            SharedString::new()
        },
        rank: item.rank as i32,
        // 排行榜 rows never carry 继续观看: the badge belongs to the 分区索引
        // grid, and the rank list is the site's own ordering.
        progress: SharedString::new(),
    }
}

pub(crate) fn pgc_day_rows(days: &[bilibili::PgcTimelineDay]) -> Vec<PgcDayRow> {
    days.iter()
        .map(|day| PgcDayRow {
            week: pgc_weekday(day.day_of_week).into(),
            date: day.date.clone().into(),
            count: day.episodes.len() as i32,
            today: day.is_today,
        })
        .collect()
}

pub(crate) fn pgc_ep_rows(day: Option<&bilibili::PgcTimelineDay>) -> Vec<PgcEpRow> {
    day.map(|day| {
        day.episodes
            .iter()
            .map(|episode| PgcEpRow {
                thumb: Image::default(),
                title: episode.title.clone().into(),
                index: episode.pub_index.clone().into(),
                time: episode.pub_time.clone().into(),
                follows: episode.follows.clone().into(),
            })
            .collect()
    })
    .unwrap_or_default()
}

pub(crate) fn pgc_set_status(ui: &AppWindow, text: impl Into<SharedString>, loading: bool) {
    ui.set_pgc_status(text.into());
    ui.set_pgc_loading(loading);
}

/// Publish the selected day's rows (and its cover keys) from the stored
/// timeline. UI thread only.
pub(crate) fn pgc_publish_day(ui: &AppWindow, host: &PgcHost, day: i32) {
    let (days, day) = {
        let Ok(mut state) = host.state.lock() else {
            return;
        };
        let day = if state.timeline.is_empty() {
            0
        } else {
            day.clamp(0, state.timeline.len() as i32 - 1)
        };
        state.day = day;
        (state.timeline.clone(), day)
    };
    let selected = days.get(day as usize);
    let ids: Vec<String> = selected
        .map(|day| {
            day.episodes
                .iter()
                .map(|episode| episode.season_id.to_string())
                .collect()
        })
        .unwrap_or_default();
    if let Ok(mut covers) = host.ep_covers.lock() {
        *covers = ids;
    }
    ui.set_pgc_day(day);
    ui.set_pgc_eps(ModelRc::from(std::rc::Rc::new(VecModel::from(pgc_ep_rows(
        selected,
    )))));
    let count = selected.map(|day| day.episodes.len()).unwrap_or(0);
    if days.is_empty() {
        pgc_set_status(ui, "每日放送暂无数据", false);
    } else if count == 0 {
        pgc_set_status(ui, "这一天没有更新", false);
    } else {
        pgc_set_status(ui, "", false);
    }
}

/// Stream the timeline rows' covers after they are on screen.
pub(crate) async fn pgc_stream_ep_covers(
    weak: &Weak<AppWindow>,
    session: &Arc<bilibili::Session>,
    host: &PgcHost,
    pics: Vec<String>,
    keys: Vec<String>,
) {
    if pics.is_empty() {
        return;
    }
    let covers = host.ep_covers.clone();
    stream_cover_list(
        weak,
        session,
        pics,
        keys,
        0,
        move |_index, key| ThumbGuard::Ids(covers.clone(), key),
        |ui| ui.get_pgc_eps(),
        |row, image| row.thumb = image,
        "pgc timeline".to_string(),
    )
    .await;
}

pub(crate) fn pgc_load_timeline(weak: Weak<AppWindow>, session_slot: SessionSlot, host: PgcHost) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let my = host.seq.fetch_add(1, Ordering::SeqCst) + 1;
    let keep_date = host
        .state
        .lock()
        .ok()
        .and_then(|state| {
            state
                .timeline
                .get(state.day.max(0) as usize)
                .map(|day| day.date.clone())
        })
        .unwrap_or_default();
    if let Some(ui) = weak.upgrade() {
        pgc_set_status(&ui, "正在加载每日放送…", true);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let days = match session.pgc_timeline().await {
                Ok(days) => days,
                Err(err) => {
                    crate::debug_log(&format!("pgc timeline err={err:#}"));
                    let weak = weak.clone();
                    let host = host.clone();
                    let message = format!("每日放送加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if host.seq.load(Ordering::SeqCst) != my {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            if let Ok(mut state) = host.state.lock() {
                                state.timeline.clear();
                            }
                            ui.set_pgc_days(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<PgcDayRow>::new(),
                            ))));
                            ui.set_pgc_eps(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<PgcEpRow>::new(),
                            ))));
                            pgc_set_status(&ui, message, false);
                        }
                    });
                    return;
                }
            };
            let total: usize = days.iter().map(|day| day.episodes.len()).sum();
            crate::debug_log(&format!(
                "pgc timeline days={} episodes={} per_day=[{}]",
                days.len(),
                total,
                days.iter()
                    .map(|day| format!(
                        "{}{}={}",
                        day.date,
                        if day.is_today { "*" } else { "" },
                        day.episodes.len()
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            let today = days.iter().position(|day| day.is_today).unwrap_or(0);
            let day = days
                .iter()
                .position(|day| !keep_date.is_empty() && day.date == keep_date)
                .unwrap_or(today) as i32;
            let weak_a = weak.clone();
            let host_a = host.clone();
            let days_for_ui = days.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if host_a.seq.load(Ordering::SeqCst) != my {
                    return;
                }
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut state) = host_a.state.lock() {
                        state.timeline = days_for_ui.clone();
                        state.day = day;
                    }
                    ui.set_pgc_days(ModelRc::from(std::rc::Rc::new(VecModel::from(
                        pgc_day_rows(&days_for_ui),
                    ))));
                    pgc_publish_day(&ui, &host_a, day);
                }
            });
            let (pics, keys) = days
                .get(day as usize)
                .map(|day| {
                    (
                        day.episodes
                            .iter()
                            .map(|episode| episode.cover.clone())
                            .collect::<Vec<_>>(),
                        day.episodes
                            .iter()
                            .map(|episode| episode.season_id.to_string())
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_default();
            pgc_stream_ep_covers(&weak, &session, &host, pics, keys).await;
        });
    }));
}

/// Publish one index/rank page. `append` keeps the rows already on screen and
/// adds the freshly fetched ones (the 20-per-page scroll path).
pub(crate) fn pgc_publish_index_page(
    ui: &AppWindow,
    host: &PgcHost,
    page: bilibili::PgcIndexPage,
    append: bool,
) {
    let mut cards: Vec<PgcCard> = if append {
        let model = ui.get_pgc_items();
        (0..model.row_count())
            .filter_map(|index| model.row_data(index))
            .collect()
    } else {
        Vec::new()
    };
    cards.extend(page.items.iter().map(pgc_card_from_index));
    let new_ids: Vec<String> = page.items
        .iter()
        .map(|item| item.season_id.to_string())
        .collect();
    {
        let mut covers = match host.covers.lock() {
            Ok(covers) => covers,
            Err(_) => return,
        };
        if !append {
            covers.clear();
        }
        covers.extend(new_ids.iter().cloned());
    }
    if let Ok(mut state) = host.state.lock() {
        if append {
            state.index_items.extend(page.items.iter().cloned());
        } else {
            state.index_items = page.items.clone();
        }
        state.page = page.page;
        state.total = page.total;
        state.has_more = page.has_next;
        state.loading = false;
    }
    ui.set_pgc_items(ModelRc::from(std::rc::Rc::new(VecModel::from(cards))));
    ui.set_pgc_total(page.total.min(i32::MAX as i64) as i32);
    ui.set_pgc_has_more(page.has_next);
    ui.set_qa_pgc_page(page.page as i32);
    if page.items.is_empty() {
        pgc_set_status(ui, "这个分区暂无内容", false);
    } else {
        pgc_set_status(ui, "", false);
    }
}

pub(crate) async fn pgc_stream_item_covers(
    weak: &Weak<AppWindow>,
    session: &Arc<bilibili::Session>,
    host: &PgcHost,
    items: &[bilibili::PgcIndexItem],
    offset: usize,
) {
    if items.is_empty() {
        return;
    }
    let pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
    let keys: Vec<String> = items
        .iter()
        .map(|item| item.season_id.to_string())
        .collect();
    let covers = host.covers.clone();
    stream_cover_list(
        weak,
        session,
        pics,
        keys,
        offset,
        move |_index, key| ThumbGuard::Ids(covers.clone(), key),
        |ui| ui.get_pgc_items(),
        |row, image| row.thumb = image,
        "pgc index".to_string(),
    )
    .await;
}

pub(crate) fn pgc_load_index(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    host: PgcHost,
    page: i64,
    append: bool,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (tab, sub) = match host.state.lock() {
        Ok(mut state) => {
            if state.loading && append {
                return;
            }
            state.loading = true;
            (state.tab, state.sub)
        }
        Err(_) => return,
    };
    let Some(season_type) = pgc_season_type(tab) else {
        return;
    };
    if sub != 0 {
        return;
    }
    // 排序: the tab's own remembered key, mapped to the verified API `order`.
    let sort = host
        .settings
        .lock()
        .map(|prefs| pgc_stored_sort(&prefs, season_type))
        .unwrap_or_else(|_| settings::PGC_SORT_DEFAULT.to_string());
    let order = pgc_sort_order(&sort);
    if let Ok(mut state) = host.state.lock() {
        state.sort = sort.clone();
    }
    let my = host.seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        if !append {
            ui.set_pgc_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                Vec::<PgcCard>::new(),
            ))));
            ui.set_pgc_total(0);
            ui.set_pgc_has_more(false);
        }
        pgc_set_status(&ui, if append { "正在加载更多…" } else { "正在加载…" }, true);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = page.max(1);
            let result = session
                .pgc_index(season_type, page, PGC_PAGE_SIZE, order)
                .await;
            let page = match result {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!(
                        "pgc index err season_type={season_type} page={page} order={order} err={err:#}"
                    ));
                    let weak = weak.clone();
                    let host = host.clone();
                    let message = format!("番剧列表加载失败：{err}（点击重试）");
                    let _ = slint::invoke_from_event_loop(move || {
                        if host.seq.load(Ordering::SeqCst) != my {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            if let Ok(mut state) = host.state.lock() {
                                state.loading = false;
                                if !append {
                                    state.index_items.clear();
                                    state.total = 0;
                                    state.has_more = false;
                                }
                            }
                            if !append {
                                ui.set_pgc_items(ModelRc::from(std::rc::Rc::new(
                                    VecModel::from(Vec::<PgcCard>::new()),
                                )));
                                ui.set_pgc_total(0);
                                ui.set_pgc_has_more(false);
                            }
                            pgc_set_status(&ui, message, false);
                        }
                    });
                    return;
                }
            };
            crate::debug_log(&format!(
                "pgc index season_type={season_type} page={} total={} has_next={} items={} append={append} sort={sort} order={order} first={:?} first_order={:?}",
                page.page,
                page.total,
                page.has_next,
                page.items.len(),
                page.items.first().map(|item| item.title.clone()).unwrap_or_default(),
                page.items.first().map(|item| item.order.clone()).unwrap_or_default()
            ));
            let items = page.items.clone();
            let offset = if append {
                host.state
                    .lock()
                    .map(|state| state.index_items.len())
                    .unwrap_or(0)
            } else {
                0
            };
            let weak_a = weak.clone();
            let host_a = host.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if host_a.seq.load(Ordering::SeqCst) != my {
                    return;
                }
                if let Some(ui) = weak_a.upgrade() {
                    pgc_publish_index_page(&ui, &host_a, page, append);
                }
            });
            pgc_stream_item_covers(&weak, &session, &host, &items, offset).await;
        });
    }));
}

pub(crate) fn pgc_publish_rank(ui: &AppWindow, host: &PgcHost, items: Vec<bilibili::PgcRankItem>) {
    let ids: Vec<String> = items
        .iter()
        .map(|item| item.season_id.to_string())
        .collect();
    if let Ok(mut covers) = host.covers.lock() {
        *covers = ids;
    }
    if let Ok(mut state) = host.state.lock() {
        state.rank_items = items.clone();
        state.loading = false;
        state.has_more = false;
        state.page = 1;
        state.total = items.len() as i64;
    }
    ui.set_pgc_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
        items.iter().map(pgc_card_from_rank).collect::<Vec<_>>(),
    ))));
    ui.set_pgc_total(items.len().min(i32::MAX as usize) as i32);
    ui.set_pgc_has_more(false);
    ui.set_qa_pgc_page(1);
    if items.is_empty() {
        pgc_set_status(ui, "排行榜暂无数据", false);
    } else {
        pgc_set_status(ui, "", false);
    }
}

pub(crate) fn pgc_load_rank(weak: Weak<AppWindow>, session_slot: SessionSlot, host: PgcHost) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let tab = match host.state.lock() {
        Ok(mut state) => {
            state.loading = true;
            state.tab
        }
        Err(_) => return,
    };
    let Some(season_type) = pgc_season_type(tab) else {
        return;
    };
    let my = host.seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        ui.set_pgc_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<PgcCard>::new(),
        ))));
        ui.set_pgc_total(0);
        ui.set_pgc_has_more(false);
        pgc_set_status(&ui, "正在加载排行榜…", true);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = session.pgc_rank(season_type, 3).await;
            match result {
                Ok(items) => {
                    crate::debug_log(&format!(
                        "pgc rank season_type={season_type} day=3 items={}",
                        items.len()
                    ));
                    let total = items.len();
                    let pics: Vec<String> =
                        items.iter().map(|item| item.cover.clone()).collect();
                    let keys: Vec<String> = items
                        .iter()
                        .map(|item| item.season_id.to_string())
                        .collect();
                    let weak_a = weak.clone();
                    let host_a = host.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if host_a.seq.load(Ordering::SeqCst) != my {
                            return;
                        }
                        if let Some(ui) = weak_a.upgrade() {
                            pgc_publish_rank(&ui, &host_a, items);
                        }
                    });
                    if total > 0 {
                        let covers = host.covers.clone();
                        stream_cover_list(
                            &weak,
                            &session,
                            pics,
                            keys,
                            0,
                            move |_index, key| ThumbGuard::Ids(covers.clone(), key),
                            |ui| ui.get_pgc_items(),
                            |row, image| row.thumb = image,
                            "pgc rank".to_string(),
                        )
                        .await;
                    }
                }
                Err(err) => {
                    crate::debug_log(&format!("pgc rank err season_type={season_type} err={err:#}"));
                    let weak = weak.clone();
                    let host = host.clone();
                    let message = format!("排行榜加载失败：{err}（点击重试）");
                    let _ = slint::invoke_from_event_loop(move || {
                        if host.seq.load(Ordering::SeqCst) != my {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            if let Ok(mut state) = host.state.lock() {
                                state.loading = false;
                                state.rank_items.clear();
                            }
                            pgc_set_status(&ui, message, false);
                        }
                    });
                }
            }
        });
    }));
}

/// Show the 番剧 page and load whatever tab it is on.
pub(crate) fn pgc_open_page(weak: Weak<AppWindow>, session_slot: SessionSlot, host: PgcHost) {
    let (tab, sub) = host
        .state
        .lock()
        .map(|state| (state.tab, state.sub))
        .unwrap_or((0, 0));
    let sort = pgc_season_type(tab)
        .and_then(|season_type| {
            host.settings
                .lock()
                .ok()
                .map(|prefs| pgc_stored_sort(&prefs, season_type))
        })
        .unwrap_or_else(|| settings::PGC_SORT_DEFAULT.to_string());
    if let Some(ui) = weak.upgrade() {
        ui.set_view_mode(22);
        ui.set_pgc_tab(tab);
        ui.set_pgc_sub(sub);
        ui.set_pgc_sort(pgc_sort_index(&sort));
        ui.set_pgc_sort_open(false);
    }
    pgc_reload(weak, session_slot, host);
}

pub(crate) fn pgc_reload(weak: Weak<AppWindow>, session_slot: SessionSlot, host: PgcHost) {
    let (tab, sub) = host
        .state
        .lock()
        .map(|state| (state.tab, state.sub))
        .unwrap_or((0, 0));
    if tab == 0 {
        pgc_load_timeline(weak, session_slot, host);
    } else if sub == 0 {
        pgc_load_index(weak, session_slot, host, 1, false);
    } else {
        pgc_load_rank(weak, session_slot, host);
    }
}

pub(crate) fn pgc_select_day(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    host: PgcHost,
    index: i32,
) {
    crate::debug_log(&format!("pgc day select index={index}"));
    let Some(session) = active_session(&session_slot) else {
        crate::debug_log("pgc day select skipped reason=no-session");
        return;
    };
    let (pics, keys, label) = {
        let Ok(state) = host.state.lock() else {
            return;
        };
        let Some(day) = state.timeline.get(index.max(0) as usize) else {
            return;
        };
        (
            day.episodes
                .iter()
                .map(|episode| episode.cover.clone())
                .collect::<Vec<_>>(),
            day.episodes
                .iter()
                .map(|episode| episode.season_id.to_string())
                .collect::<Vec<_>>(),
            format!("day={} date={} eps={}", index, day.date, day.episodes.len()),
        )
    };
    crate::debug_log(&format!("pgc timeline pick {label}"));
    if let Some(ui) = weak.upgrade() {
        pgc_publish_day(&ui, &host, index);
    }
    crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            pgc_stream_ep_covers(&weak, &session, &host, pics, keys).await;
        });
    }));
}

/// Resolve one published row back to a season id and open it.
pub(crate) fn pgc_open_row(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    host: PgcHost,
    index: i32,
) {
    let index = index.max(0) as usize;
    let (season_id, kind) = {
        let Ok(state) = host.state.lock() else {
            return;
        };
        if state.tab == 0 {
            let season_id = state
                .timeline
                .get(state.day.max(0) as usize)
                .and_then(|day| day.episodes.get(index))
                .map(|episode| episode.season_id)
                .unwrap_or(0);
            (season_id, "timeline")
        } else if state.sub == 1 {
            let season_id = state
                .rank_items
                .get(index)
                .map(|item| item.season_id)
                .unwrap_or(0);
            (season_id, "rank")
        } else {
            let season_id = state
                .index_items
                .get(index)
                .map(|item| item.season_id)
                .unwrap_or(0);
            (season_id, "index")
        }
    };
    if season_id <= 0 {
        crate::debug_log(&format!("pgc open miss index={index} from={kind}"));
        if let Some(ui) = weak.upgrade() {
            ui.set_toast_text("番剧条目已失效，请刷新".into());
            ui.set_toast_kind(TOAST_KIND_ERROR);
        }
        return;
    }
    let picked = host.state.lock().map(|state| {
        (
            state.day,
            state
                .timeline
                .get(state.day.max(0) as usize)
                .map(|day| day.date.clone())
                .unwrap_or_default(),
        )
    });
    crate::debug_log(&format!(
        "pgc open season={season_id} row={index} from={kind} day={picked:?}"
    ));
    // 继续观看: a stored episode for this season is played instead of episode 1.
    let resume = pgc_progress_entry(season_id).filter(|entry| entry.ep_id > 0);
    if let Some(entry) = resume.as_ref() {
        crate::debug_log(&format!(
            "pgc resume request season={season_id} from={kind} ep={} index={} title={:?}",
            entry.ep_id, entry.index, entry.title
        ));
    }
    pgc_open_season(weak, session_slot, season_id, resume);
}

/// Fetch one season, gate its first aired episode, then open the player with
/// the whole season as its 选集 list. A refused gate never opens a player: the
/// main window shows the site's own reason instead.
///
/// `resume` is the season's 继续观看 row when there is one: that episode (not
/// episode 1) is played, and the main window is told so.
pub(crate) fn pgc_open_season(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    season_id: i64,
    resume: Option<settings::ContinueEntry>,
) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_toast_text("尚未登录，无法播放番剧".into());
            ui.set_toast_kind(TOAST_KIND_ERROR);
        }
        return;
    };
    crate::netpool::submit(crate::netpool::Prio::High, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let season = match session.pgc_season(season_id).await {
                Ok(season) => season,
                Err(err) => {
                    crate::debug_log(&format!("pgc season err season={season_id} err={err:#}"));
                    let weak = weak.clone();
                    let message = format!("番剧加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_toast_text(message.into());
                            ui.set_toast_kind(TOAST_KIND_ERROR);
                        }
                    });
                    return;
                }
            };
            crate::debug_log(&format!(
                "pgc season season={} title={:?} episodes={} sections={} index={:?}",
                season.season_id,
                season.title,
                season.episodes.len(),
                season.sections.len(),
                season.index_show
            ));
            let now = unix_now();
            // 继续观看 wins when the stored episode still exists in the season
            // (its 正片 list, which is what 选集 and the badge count).
            let resumed = resume
                .as_ref()
                .and_then(|entry| {
                    season
                        .episodes
                        .iter()
                        .position(|episode| episode.ep_id == entry.ep_id)
                        .map(|position| (position, entry))
                })
                .map(|(position, _entry)| (season.episodes[position].clone(), position as u32 + 1));
            if let Some((episode, number)) = resumed.as_ref() {
                crate::debug_log(&format!(
                    "pgc resume hit season={season_id} ep={} index={number} bvid={} cid={} stored_index={}",
                    episode.ep_id, episode.bvid, episode.cid, resume.as_ref().map(|e| e.index).unwrap_or(0)
                ));
            } else if let Some(entry) = resume.as_ref() {
                crate::debug_log(&format!(
                    "pgc resume miss season={season_id} ep={} reason=not-in-season episodes={}",
                    entry.ep_id,
                    season.episodes.len()
                ));
            }
            let picked = resumed;
            let Some((episode, resume_number)) = picked.or_else(|| {
                season
                    .first_episode(now)
                    .cloned()
                    .map(|episode| (episode, 0u32))
            }) else {
                crate::debug_log(&format!("pgc season empty season={season_id}"));
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_toast_text("该番剧没有可播放的分集".into());
                        ui.set_toast_kind(TOAST_KIND_ERROR);
                    }
                });
                return;
            };
            if episode.bvid.is_empty() || episode.cid == 0 {
                crate::debug_log(&format!(
                    "pgc season unusable season={season_id} ep={} bvid={:?} cid={}",
                    episode.ep_id, episode.bvid, episode.cid
                ));
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_toast_text("该分集没有可用的播放地址".into());
                        ui.set_toast_kind(TOAST_KIND_ERROR);
                    }
                });
                return;
            }
            let gate = session.pgc_gate(episode.ep_id, episode.cid).await;
            let gate = match gate {
                Ok(gate) => gate,
                Err(err) => {
                    // The gate itself failing is a network problem, not a
                    // refusal: log it and open anyway (the player has its own
                    // error path) instead of inventing a reason.
                    crate::debug_log(&format!(
                        "pgc gate err season={season_id} ep={} err={err:#}",
                        episode.ep_id
                    ));
                    crate::bilibili::PgcGate::default()
                }
            };
            if gate.code != 0 {
                let reason = crate::bilibili::pgc_reason(gate.code)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if gate.message.trim().is_empty() {
                            format!("B站返回 code={}", gate.code)
                        } else {
                            format!("{}（code={}）", gate.message.trim(), gate.code)
                        }
                    });
                crate::debug_log(&format!(
                    "pgc gate blocked season={season_id} ep={} cid={} code={} reason={reason}",
                    episode.ep_id, episode.cid, gate.code
                ));
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_toast_text(reason.into());
                        ui.set_toast_kind(TOAST_KIND_ERROR);
                    }
                });
                return;
            }
            crate::debug_log(&format!(
                "pgc gate ok season={season_id} ep={} cid={} bvid={} resume_index={resume_number}",
                episode.ep_id, episode.cid, episode.bvid
            ));
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                // 继续第 N 集: the same episode the player is about to start,
                // said on the window the user came from.
                if resume_number > 0 {
                    ui.set_toast_text(format!("继续第 {resume_number} 集").into());
                    ui.set_toast_kind(TOAST_KIND_INFO);
                    crate::debug_log(&format!(
                        "pgc resume toast season={season_id} ep={} index={resume_number} text={:?}",
                        episode.ep_id,
                        ui.get_toast_text().to_string()
                    ));
                }
                let host = PGC_PLAYER_HOST.with(|cell| cell.borrow().clone());
                if let Some(host) = host {
                    open_player_for_pgc(
                        &session_slot,
                        &host.state,
                        &host.settings,
                        season,
                        episode,
                    );
                }
            });
        });
    }));
}
