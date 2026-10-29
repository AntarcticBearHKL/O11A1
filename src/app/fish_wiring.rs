//! 养鱼 wiring: the engine's callbacks, rows and settings hand-off.

use crate::*;

pub(crate) fn fish_state_code(state: &fish::FishState) -> i32 {
    match state {
        fish::FishState::Idle => 0,
        fish::FishState::NeedsKey => 1,
        fish::FishState::Running => 2,
        fish::FishState::Paused => 3,
        fish::FishState::Error(_) => 4,
    }
}

pub(crate) fn local_clock() -> (u32, u32) {
    let mut time = WinLocalTime {
        year: 0,
        month: 1,
        day_of_week: 0,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut time) };
    (
        time.hour as u32 * 60 + time.minute as u32,
        time.month as u32 * 32 + time.day as u32,
    )
}

pub(crate) fn local_day_of_year() -> u32 {
    local_clock().1
}

/// Apply one engine snapshot to the 养鱼 page.
///
/// Scalars are cheap and always written; the two models are gated:
/// * verdict rows only when `(bvid, at)` of the newest verdict changed, so a
///   token-count update never rebuilds the 8 rows;
/// * stats (histogram + tag chips) at most twice a second.
///
/// `force` bypasses both gates (page open, key/instruction save, refresh).
pub(crate) fn apply_fish(ui: &AppWindow, fish: &FishUi, snap: fish::FishSnapshot, force: bool) {
    ui.set_fish_state(fish_state_code(&snap.state));
    ui.set_fish_queued(snap.queued as i32);
    ui.set_fish_done(snap.done as i32);
    ui.set_fish_errors(snap.errors as i32);
    ui.set_fish_now(
        snap.current
            .as_ref()
            .map(|(_, title)| SharedString::from(title.clone()))
            .unwrap_or_default(),
    );
    ui.set_fish_tokens_in(snap.tokens_in.min(i32::MAX as u64) as i32);
    ui.set_fish_tokens_out(snap.tokens_out.min(i32::MAX as u64) as i32);
    ui.set_fish_error_text(fish.error_text().into());
    if let fish::FishState::Error(text) = &snap.state {
        ui.set_fish_error_text(text.clone().into());
    }
    ui.set_fish_last_line(
        snap.last
            .as_ref()
            .map(|last| SharedString::from(last.one_line.clone()))
            .unwrap_or_default(),
    );
    let (has_key, masked) = fish.key_state.borrow().clone();
    ui.set_fish_has_key(has_key);
    ui.set_fish_key_masked(masked.as_str().into());

    let Some(engine) = fish.engine.as_ref() else {
        publish_fish_recipes(ui, "");
        return;
    };

    // The card lists are rebuilt when the newest verdict changes, when the
    // toolbar's query changes, after a 清空, or at local midnight; a
    // token-count tick never rebuilds 200 cards.
    let sort = fish.feed_sort.get();
    let filter = fish.feed_filter.get();
    let (now, offset) = fish::local_now();
    let day = fish::day_key(now, offset);
    let key = snap.last.as_ref().map(|last| {
        (
            last.bvid.clone(),
            last.at,
            sort,
            filter,
            engine.cleared_at(),
            day,
        )
    });
    if force || key != *fish.last_verdict.borrow() {
        *fish.last_verdict.borrow_mut() = key;
        publish_fish_feed(ui, fish, engine);
    }

    let stats_due = force
        || fish
            .last_stats
            .borrow()
            .map_or(true, |at| at.elapsed() >= Duration::from_millis(500));
    if stats_due {
        *fish.last_stats.borrow_mut() = Some(Instant::now());
        if let Ok(board) = engine.dashboard() {
            ui.set_fish_hist(ModelRc::from(std::rc::Rc::new(VecModel::from(
                board
                    .score_histogram
                    .iter()
                    .map(|n| *n as i32)
                    .collect::<Vec<_>>(),
            ))));
            ui.set_fish_tags(ModelRc::from(std::rc::Rc::new(VecModel::from(
                board
                    .top_tags
                    .iter()
                    .take(8)
                    .map(|(tag, _)| SharedString::from(tag.clone()))
                    .collect::<Vec<_>>(),
            ))));
            ui.set_fish_recommend(board.today.recommend as i32);
            ui.set_fish_skip(board.today.skip as i32);
            ui.set_fish_maybe(board.today.maybe as i32);
            // The header is 今日-first; the 7/30-day totals are the small print
            // under it. Every number is formatted here (equal-width text in the
            // UI) so a digit count never reflows the row.
            ui.set_fish_today_count(board.today.watched as i32);
            ui.set_fish_today_tokens(fmt_tokens(board.today.tokens()).as_str().into());
            ui.set_fish_week_line(fmt_window("最近 7 天", &board.week).as_str().into());
            ui.set_fish_month_line(fmt_window("最近 30 天", &board.month).as_str().into());
            // Growth = adopted verdicts: 看 seen so far plus the cards the owner
            // opened from the page this session (deduped by bvid).
            ui.set_fish_adopted(
                (board.month.recommend as i32).saturating_add(fish.adopted.borrow().len() as i32),
            );
        }
    }

    let instruction = engine.instruction();
    if force || ui.get_fish_instruction().as_str() != instruction {
        ui.set_fish_instruction(instruction.as_str().into());
        // The instruction field follows the saved value (a fresh page, a preset
        // click or a save). Unsaved typing is never overwritten by a mere
        // snapshot tick.
        if force {
            ui.set_fish_instruction_draft(instruction.as_str().into());
        }
        publish_fish_recipes(ui, &instruction);
    }
}

/// Rebuild the 鱼缸's two card lists for the current sort/filter.
///
/// * 推荐流 (`fish_rows`) is **today**: the verdicts judged since local midnight
///   (and since the last 清空), filtered and sorted by the toolbar.
/// * 历史 (`fish_hist`) is every verdict, newest first, split into local
///   calendar days. Each row carries its own height, so the page's
///   `VerticalLayout` gets the 8px grid exactly: 132 = 120 card + 12 gap, the
///   last card of a day is 120, the first day header is 24, later ones 48
///   (24 section gap + a 24px label row).
///
/// Sorting happens in Rust (Slint has no sort), and the same pass starts the
/// one-shot cover fetches for rows that just entered a list.
pub(crate) fn publish_fish_feed(ui: &AppWindow, fish: &FishUi, engine: &fish::FishEngine) {
    let sort = fish.feed_sort.get();
    let filter = fish.feed_filter.get();
    let (now, offset) = fish::local_now();
    let today = fish::day_key(now, offset);
    let cleared = engine.cleared_at();
    let all = engine.verdicts(200).unwrap_or_default();
    let mut today_rows: Vec<fish::VerdictView> = all
        .iter()
        .filter(|verdict| fish::day_key(verdict.at, offset) == today && verdict.at >= cleared)
        .cloned()
        .collect();
    today_rows.retain(|verdict| match filter {
        1 => verdict.verdict.trim() == "看",
        2 => verdict.verdict.trim() == "不看",
        3 => !matches!(verdict.verdict.trim(), "看" | "不看"),
        _ => true,
    });
    let rank = |verdict: &fish::VerdictView| match verdict.verdict.trim() {
        "看" => 2,
        "也许" => 1,
        _ => 0,
    };
    match sort {
        0 => today_rows.sort_by(|a, b| {
            rank(b)
                .cmp(&rank(a))
                .then_with(|| b.score.total_cmp(&a.score))
                .then_with(|| b.at.cmp(&a.at))
        }),
        1 => today_rows.sort_by(|a, b| b.at.cmp(&a.at)),
        _ => today_rows.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.at.cmp(&a.at))
        }),
    }
    // Older rows can have an empty cover in `fish.db`. The in-memory home feed
    // is the first fallback: it already holds the same bvid's `pic` URL, so a
    // verdict fished before covers were stored still gets a real thumbnail.
    let feed_covers: std::collections::HashMap<String, String> = fish
        .feed_store
        .lock()
        .map(|feed| {
            feed.iter()
                .filter(|video| !video.pic.trim().is_empty())
                .map(|video| (video.bvid.clone(), video.pic.clone()))
                .collect()
        })
        .unwrap_or_default();
    let cover_of = |verdict: &fish::VerdictView| -> (String, bool) {
        if !verdict.cover.trim().is_empty() {
            (verdict.cover.clone(), false)
        } else if let Some(pic) = feed_covers.get(&verdict.bvid) {
            (pic.clone(), true)
        } else {
            (String::new(), false)
        }
    };

    let mut cover_urls: Vec<String> = Vec::with_capacity(today_rows.len());
    let mut fallback = 0usize;
    let feed: Vec<FishRow> = today_rows
        .iter()
        .map(|verdict| {
            let (url, fell_back) = cover_of(verdict);
            if fell_back {
                fallback += 1;
            }
            cover_urls.push(url.clone());
            fish_feed_row(verdict, &url, now, offset)
        })
        .collect();

    // 历史: newest first (the query already is), grouped by local day. The
    // heights are the page's grid, computed here so the layout is data-driven
    // and the probe table has one source of truth.
    let groups = fish::group_by_day(all, now, offset);
    let mut hist: Vec<FishHistRow> = Vec::with_capacity(groups.iter().map(|group| group.count() + 1).sum());
    let mut hist_covers: Vec<String> = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        hist.push(FishHistRow {
            kind: 0,
            h: if group_index == 0 { 24.0 } else { 48.0 },
            head: group.label.as_str().into(),
            item: FishRow::default(),
        });
        let last = group.count().saturating_sub(1);
        for (index, verdict) in group.rows.iter().enumerate() {
            let (url, _) = cover_of(verdict);
            hist_covers.push(url.clone());
            hist.push(FishHistRow {
                kind: 1,
                h: if index == last { 120.0 } else { 132.0 },
                head: SharedString::new(),
                item: fish_feed_row(verdict, &url, now, offset),
            });
        }
    }

    ui.set_fish_rows(ModelRc::from(std::rc::Rc::new(VecModel::from(feed))));
    ui.set_fish_hist_count(hist.iter().filter(|row| row.kind == 1).count() as i32);
    ui.set_fish_hist_days(groups.len() as i32);
    ui.set_fish_hist_rows(ModelRc::from(std::rc::Rc::new(VecModel::from(hist))));
    let feed_hits = today_rows
        .iter()
        .filter(|verdict| feed_covers.contains_key(&verdict.bvid))
        .count();
    crate::debug_log(&format!(
        "fish feed sort={sort} filter={filter} today={} history={} days={} cleared_at={cleared} covers={} feed_hits={feed_hits} feed_fallback={fallback}",
        today_rows.len(),
        ui.get_fish_hist_count(),
        groups.len(),
        cover_urls.iter().filter(|url| !url.is_empty()).count()
    ));
    // Covers: the same fetch path the feed uses, one attempt per URL. A landing
    // cover republishes the rows once (see `fish_fetch_cover`). The attempt is
    // only recorded there, AFTER a session exists: a startup publish (before
    // login) must never poison the list for the page's real open. `on_fish_open`
    // refreshes FISH_SESSION first, so the page's open is what makes these run.
    for url in cover_urls.iter().chain(hist_covers.iter()) {
        if url.is_empty() {
            continue;
        }
        fish_fetch_cover(ui.as_weak(), url.clone());
    }
}

/// One engine verdict as a 鱼缸 card. `cover_url` is `verdict.cover` or the
/// home feed's `pic` fallback (see `publish_fish_feed`); `when` is the local
/// wall clock, formatted here so the card never reads a clock itself.
pub(crate) fn fish_feed_row(verdict: &fish::VerdictView, cover_url: &str, now: i64, offset: i64) -> FishRow {
    let tag_line = verdict
        .tags
        .iter()
        .take(2)
        .map(|tag| format!("#{tag}"))
        .collect::<Vec<_>>()
        .join(" ");
    FishRow {
        bvid: verdict.bvid.as_str().into(),
        title: verdict.title.as_str().into(),
        up_name: verdict.up_name.as_str().into(),
        cover: cached_cover(cover_url),
        score: verdict.score as f32,
        score_text: if verdict.score >= 10.0 {
            "10".into()
        } else if verdict.score <= 0.0 {
            "—".into()
        } else {
            format!("{:.1}", verdict.score).into()
        },
        verdict: verdict.verdict.as_str().into(),
        one_line: verdict.one_line.as_str().into(),
        reason1: verdict
            .reasons
            .first()
            .map(|reason| SharedString::from(reason.as_str()))
            .unwrap_or_default(),
        reason2: verdict
            .reasons
            .get(1)
            .map(|reason| SharedString::from(reason.as_str()))
            .unwrap_or_default(),
        // The old chip fields are kept for the model shape, but the rebuilt
        // card reads `tag_line` (footer) instead of four pills.
        tag1: verdict
            .tags
            .first()
            .map(|tag| SharedString::from(tag.as_str()))
            .unwrap_or_default(),
        tag2: verdict
            .tags
            .get(1)
            .map(|tag| SharedString::from(tag.as_str()))
            .unwrap_or_default(),
        tag3: verdict
            .tags
            .get(2)
            .map(|tag| SharedString::from(tag.as_str()))
            .unwrap_or_default(),
        tag4: verdict
            .tags
            .get(3)
            .map(|tag| SharedString::from(tag.as_str()))
            .unwrap_or_default(),
        age: fish_age_text(verdict.at).into(),
        time_text: fish_clock_text(verdict.at).into(),
        when: fish::verdict_time_text(verdict.at, now, offset).into(),
        tag_line: tag_line.into(),
        up_glyph: verdict
            .up_name
            .chars()
            .next()
            .map(|glyph| SharedString::from(glyph.to_string()))
            .unwrap_or_default(),
        evidence: verdict.evidence.line().into(),
        evidence_steps: verdict.evidence.detail().into(),
    }
}

/// The verdict's local wall clock as `HH:MM`, for the card footer's
/// `12:00 起看`. `at` is stored in Unix seconds; the offset comes from the two
/// clocks the process already reads (UTC epoch vs local `GetLocalTime`), so no
/// timezone dependency is pulled in for one line.
pub(crate) fn fish_clock_text(at: i64) -> String {
    if at <= 0 {
        return String::new();
    }
    let now_utc = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let (local_minutes, _) = local_clock();
    let utc_minutes = (now_utc.rem_euclid(86_400)) / 60;
    let local = (at + (local_minutes as i64 - utc_minutes) * 60).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3_600, (local % 3_600) / 60)
}

/// A relative publish age for a feed card ("3 小时前"). The card never runs a
/// clock: the age is formatted once per model rebuild.
pub(crate) fn fish_age_text(at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let seconds = now.saturating_sub(at).max(0);
    if seconds < 60 {
        "刚刚".to_string()
    } else if seconds < 3_600 {
        format!("{} 分钟前", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} 小时前", seconds / 3_600)
    } else if seconds < 86_400 * 30 {
        format!("{} 天前", seconds / 86_400)
    } else {
        format!("{} 个月前", seconds / (86_400 * 30))
    }
}

/// Publish the 食谱 grid. `active` is the backend's exact-body match, so an
/// edited instruction highlights nothing; the UI still clears it on the first
/// keystroke (see `fish_instruction_dirty`).
pub(crate) fn publish_fish_recipes(ui: &AppWindow, instruction: &str) {
    let active = fish::active_preset(instruction).map(|preset| preset.id);
    let rows: Vec<RecipeRow> = fish::presets()
        .iter()
        .map(|preset| RecipeRow {
            id: preset.id.into(),
            name: preset.name.into(),
            tagline: preset.tagline.into(),
            active: active == Some(preset.id),
        })
        .collect();
    // The folded 食谱 group shows the active taste on one line, so the page
    // reads "严格影评人" without opening the grid.
    let name = fish::presets()
        .iter()
        .find(|preset| active == Some(preset.id))
        .map(|preset| preset.name)
        .unwrap_or("自定义指令");
    ui.set_fish_recipe_active(name.into());
    ui.set_fish_recipes(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
}

/// Save an instruction through the ONE path: the engine (which persists and
/// hashes it into the next prompt), the draft field, then a forced republish so
/// the recipe highlight follows immediately. Used by both the free-text save and
/// `fish_apply_preset`.
pub(crate) fn fish_save_instruction_text(ui: &AppWindow, fish: &FishUi, text: &str) {
    if let Some(engine) = fish.engine.as_ref() {
        engine.set_instruction(text);
        crate::debug_log(&format!(
            "fish instruction len={}",
            text.chars().count()
        ));
    }
    ui.set_fish_instruction_draft(text.into());
    if let Some(engine) = fish.engine.as_ref() {
        apply_fish(ui, fish, engine.snapshot(), true);
    }
}

/// Fetch one 养鱼 row cover through the app's normal thumb path and republish
/// the dashboard when it lands. One attempt per URL (see `tried_covers`).
pub(crate) fn fish_fetch_cover(weak: Weak<AppWindow>, url: String) {
    if url.is_empty() {
        return;
    }
    if let Ok(mut cache) = cover_cache().lock() {
        if cache.get(&url).is_some() {
            return;
        }
    }
    let Some(session) = FISH_SESSION.with(|slot| slot.borrow().clone()) else {
        return;
    };
    // Record the attempt only once there is a session: a pre-login publish must
    // not mark every cover tried and leave the feed blank for the whole run.
    let Some(fish) = FISH_UI.with(|slot| slot.borrow().clone()) else {
        return;
    };
    if !fish.tried_covers.borrow_mut().insert(url.clone()) {
        return;
    }
    crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        let Ok(pixels) = runtime.block_on(session.fetch_thumb(&url)) else {
            crate::debug_log(&format!("fish cover failed url={url}"));
            return;
        };
        if let Ok(mut cache) = cover_cache().lock() {
            cache.put(&url, Arc::new(pixels));
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Some(fish) = FISH_UI.with(|slot| slot.borrow().clone()) {
                let snap = fish
                    .engine
                    .as_ref()
                    .map(|engine| engine.snapshot())
                    .unwrap_or_default();
                apply_fish(&ui, &fish, snap, true);
            }
        });
    }));
}

/// Resolve `bvid` and open the player for it (the same `open_player_for` the
/// feed/detail paths use). The network lookup runs off the UI thread; only the
/// `Weak`, the `SessionSlot` and plain values cross the thread boundary.
pub(crate) fn open_fish_video(weak: Weak<AppWindow>, bvid: String) {
    let session_slot: SessionSlot = {
        let session = FISH_SESSION.with(|slot| slot.borrow().clone());
        match session {
            Some(session) => Arc::new(Mutex::new(Some(session))),
            None => {
                crate::debug_log(&format!("fish open bvid={bvid} no-session"));
                return;
            }
        }
    };
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        let Some(session) = active_session(&session_slot) else {
            return;
        };
        match runtime.block_on(session.video_view(&bvid)) {
            Ok(detail) => {
                let cid = if detail.cid > 0 {
                    detail.cid as u64
                } else {
                    detail.pages.first().map(|page| page.cid as u64).unwrap_or(0)
                };
                let title = detail.title.clone();
                let aid = detail.aid;
                crate::debug_log(&format!(
                    "fish open resolved bvid={bvid} cid={cid} title_len={}",
                    title.len()
                ));
                let _ = weak.upgrade_in_event_loop(move |_ui| {
                    let (Some(player_state), Some(store)) = (
                        FISH_PLAYER.with(|cell| cell.borrow().clone()),
                        FISH_SETTINGS.with(|cell| cell.borrow().clone()),
                    ) else {
                        return;
                    };
                    open_player_for(&session_slot, &player_state, &store, bvid, cid, title, aid);
                });
            }
            Err(err) => crate::debug_log(&format!("fish open bvid={bvid} err={err}")),
        }
    }));
}
