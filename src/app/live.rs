//! 直播: the live index, area grid and the live room window.

use crate::*;

/// The page host, kept for the QA hook only: `BILI_LIVE_ROOM=<id>` must open a
/// NAMED room, and the input-free driver can only address a card index. Inert
/// (and the hook inert) unless the env var is set.
pub(crate) static LIVE_QA_HOST: Mutex<Option<Arc<LiveHost>>> = Mutex::new(None);

/// `1.2万` / `875` — the same shape the feed uses. Formatted here so the card
/// never runs a formatter.
pub(crate) fn live_online_text(online: u64) -> String {
    if online >= 10_000 {
        let value = online as f64 / 10_000.0;
        if value >= 10.0 {
            format!("{value:.0}万")
        } else {
            format!("{value:.1}万")
        }
    } else {
        online.to_string()
    }
}

pub(crate) fn live_card_from(room: &bilibili::LiveRoom) -> LiveCard {
    let area = if !room.area_name.is_empty() {
        room.area_name.clone()
    } else {
        room.parent_area_name.clone()
    };
    LiveCard {
        room_id: room.room_id.min(i32::MAX as i64) as i32,
        title: room.title.clone().into(),
        uname: room.uname.clone().into(),
        area: area.into(),
        cover: Image::default(),
        online: live_online_text(room.online).into(),
        live: room.live_status == 1,
    }
}

/// Publish the aggregate's module rows (and the area chips) from a parsed
/// index. Runs on the UI thread.
pub(crate) fn live_publish_index_cached(
    ui: &AppWindow,
    host: &Arc<LiveHost>,
    modules: &[bilibili::LiveModule],
    areas: &[bilibili::LiveArea],
) {
    let mut rows: Vec<LiveCard> = Vec::new();
    let mut meta: Vec<LiveModuleRow> = Vec::new();
    let mut rooms: Vec<bilibili::LiveRoom> = Vec::new();
    for module in modules {
        if module.rooms.is_empty() {
            continue;
        }
        let start = rows.len();
        for room in &module.rooms {
            rows.push(live_card_from(room));
            rooms.push(room.clone());
        }
        meta.push(LiveModuleRow {
            title: module.title.clone().into(),
            start: start.min(i32::MAX as usize) as i32,
            count: module.rooms.len().min(i32::MAX as usize) as i32,
        });
    }
    let ids: Vec<String> = rooms.iter().map(|room| room.room_id.to_string()).collect();
    if let Ok(mut guard) = host.ids.lock() {
        *guard = ids.clone();
    }
    if let Ok(mut guard) = host.covers.lock() {
        *guard = ids;
    }
    if let Ok(mut state) = host.state.lock() {
        state.modules = modules.to_vec();
        state.areas = areas.to_vec();
        state.rooms = rooms;
        state.area_sel = 0;
        state.page = 1;
        state.has_more = false;
        state.loading = false;
        state.risk.clear();
    }
    let mut chips: Vec<SharedString> = Vec::with_capacity(areas.len() + 1);
    chips.push("推荐".into());
    for area in areas {
        chips.push(area.title.clone().into());
    }
    let card_count = rows.len();
    let module_count = meta.len();
    ui.set_live_cards(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    ui.set_live_modules(ModelRc::from(std::rc::Rc::new(VecModel::from(meta))));
    ui.set_live_chips(ModelRc::from(std::rc::Rc::new(VecModel::from(chips))));
    ui.set_live_area_sel(0);
    ui.set_live_status(String::new().into());
    ui.set_live_risk(String::new().into());
    ui.set_live_loading(false);
    ui.set_live_has_more(false);
    ui.set_live_page(1);
    ui.set_live_shown(card_count.min(i32::MAX as usize) as i32);
    crate::debug_log(&format!(
        "live publish index modules={} cards={card_count} areas={}",
        module_count,
        areas.len()
    ));
}

/// Publish one area page (`append` keeps the rows on screen). Runs on the UI
/// thread.
pub(crate) fn live_publish_area(
    ui: &AppWindow,
    host: &Arc<LiveHost>,
    rooms: &[bilibili::LiveRoom],
    page: i64,
    has_more: bool,
    append: bool,
) {
    // Keep the rows already on screen WITH their loaded covers (the model is
    // re-published, so a plain `else` would flash every cover back to empty).
    let mut rows: Vec<LiveCard> = if append {
        let model = ui.get_live_cards();
        (0..model.row_count())
            .filter_map(|index| model.row_data(index))
            .collect()
    } else {
        Vec::new()
    };
    rows.extend(rooms.iter().map(live_card_from));
    let total = rows.len();
    let new_ids: Vec<String> = rooms.iter().map(|room| room.room_id.to_string()).collect();
    if let Ok(mut guard) = host.ids.lock() {
        if !append {
            guard.clear();
        }
        guard.extend(new_ids.iter().cloned());
    }
    if let Ok(mut guard) = host.covers.lock() {
        if !append {
            guard.clear();
        }
        guard.extend(new_ids);
    }
    if let Ok(mut state) = host.state.lock() {
        if append {
            state.rooms.extend(rooms.iter().cloned());
        } else {
            state.rooms = rooms.to_vec();
        }
        state.page = page;
        state.has_more = has_more;
        state.loading = false;
        state.risk.clear();
    }
    ui.set_live_cards(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    ui.set_live_has_more(has_more);
    ui.set_live_page(page.min(i32::MAX as i64) as i32);
    ui.set_live_loading(false);
    ui.set_live_risk(String::new().into());
    ui.set_live_shown(total.min(i32::MAX as usize) as i32);
    ui.set_live_status(if total == 0 {
        "这个分区暂无直播间".into()
    } else {
        String::new().into()
    });
    crate::debug_log(&format!(
        "live publish area page={page} append={} rows={} total={total} has_more={has_more}",
        u8::from(append),
        rooms.len()
    ));
}

/// The homepage aggregate: fetch, publish, then stream the first screen of
/// covers. `force` bypasses nothing (there is no cache below the page state) —
/// it only changes the status copy, so a retry reads as a fresh load.
pub(crate) fn live_load_index(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    host: Arc<LiveHost>,
    force: bool,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let my = host.seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        ui.set_live_loading(true);
        ui.set_live_status("正在加载直播…".into());
        ui.set_live_risk(String::new().into());
        if force {
            ui.set_live_area_sel(0);
        }
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = session.live_index().await;
            let index = match result {
                Ok(index) => index,
                Err(err) => {
                    crate::debug_log(&format!("live index err={err:#}"));
                    let weak = weak.clone();
                    let host = host.clone();
                    let message = format!("直播首页加载失败：{err}（点击重试）");
                    let _ = slint::invoke_from_event_loop(move || {
                        if host.seq.load(Ordering::SeqCst) != my {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            if let Ok(mut state) = host.state.lock() {
                                state.loading = false;
                            }
                            ui.set_live_loading(false);
                            ui.set_live_status(message.into());
                        }
                    });
                    return;
                }
            };
            let modules = index.modules.clone();
            let areas = index.areas.clone();
            // Covers: at most the first 60 rooms of the aggregate (the module
            // rows the first screen can reach), streamed in with the guard that
            // drops a patch from a superseded load.
            let pics: Vec<String> = modules
                .iter()
                .flat_map(|module| module.rooms.iter())
                .take(60)
                .map(|room| room.cover.clone())
                .collect();
            let keys: Vec<String> = modules
                .iter()
                .flat_map(|module| module.rooms.iter())
                .take(60)
                .map(|room| room.room_id.to_string())
                .collect();
            let weak_a = weak.clone();
            let host_a = host.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if host_a.seq.load(Ordering::SeqCst) != my {
                    return;
                }
                if let Some(ui) = weak_a.upgrade() {
                    live_publish_index_cached(&ui, &host_a, &modules, &areas);
                }
            });
            let covers = host.covers.clone();
            let seq = host.seq.clone();
            let my_seq = my;
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| {
                    if seq.load(Ordering::SeqCst) == my_seq {
                        ThumbGuard::Ids(covers.clone(), key)
                    } else {
                        ThumbGuard::Seq(seq.clone(), u64::MAX)
                    }
                },
                |ui| ui.get_live_cards(),
                |row, image| row.cover = image,
                "live index".to_string(),
            )
            .await;
        });
    }));
}

/// One 分区 page. The legacy endpoint answers `code: 0`; a non-zero code with no
/// rooms is a refusal (or an empty area) and the page names the code and the
/// endpoint that produced it, with the retry, and no card invented.
pub(crate) fn live_load_area(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    host: Arc<LiveHost>,
    sel: i64,
    page: i64,
    append: bool,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let area = host.state.lock().ok().and_then(|state| {
        state
            .areas
            .get((sel - 1).max(0) as usize)
            .cloned()
    });
    let Some(area) = area else {
        return;
    };
    let my = host.seq.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut state = match host.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if append && state.loading {
            return;
        }
        state.loading = true;
        state.area_sel = sel as i32;
    }
    let prior_rows = if append {
        host.state.lock().map(|state| state.rooms.len()).unwrap_or(0)
    } else {
        0
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_live_loading(true);
        ui.set_live_area_sel(sel as i32);
        ui.set_live_status(if append {
            "正在加载更多…".into()
        } else {
            "正在加载…".into()
        });
        ui.set_live_risk(String::new().into());
        if !append {
            ui.set_live_cards(ModelRc::from(std::rc::Rc::new(VecModel::from(
                Vec::<LiveCard>::new(),
            ))));
            ui.set_live_shown(0);
            ui.set_live_has_more(false);
        }
    }
    let criterion = area.clone();
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = session
                .live_area_rooms(criterion.parent_id, criterion.id, page)
                .await;
            let page_data = match result {
                Ok(page_data) => page_data,
                Err(err) => {
                    crate::debug_log(&format!(
                        "live area err area={} page={page} err={err:#}",
                        criterion.id
                    ));
                    let weak = weak.clone();
                    let host = host.clone();
                    let message = format!("分区加载失败：{err}（点击重试）");
                    let _ = slint::invoke_from_event_loop(move || {
                        if host.seq.load(Ordering::SeqCst) != my {
                            return;
                        }
                        if let Some(ui) = weak.upgrade() {
                            if let Ok(mut state) = host.state.lock() {
                                state.loading = false;
                            }
                            ui.set_live_loading(false);
                            ui.set_live_status(message.into());
                        }
                    });
                    return;
                }
            };
            if page_data.code != 0 || page_data.rooms.is_empty() {
                // Name the endpoint AND the API's own code: `-352` is risk
                // control on that specific path, not "this area has no rooms".
                let source = page_data.source;
                let elapsed = page_data.elapsed_ms;
                let risk = page_data.code != 0;
                let message = if page_data.code == -352 {
                    format!("分区列表被 B站风控（-352，{source}）")
                } else if page_data.code != 0 {
                    format!("分区列表暂不可用（code={}，{source}）", page_data.code)
                } else {
                    "这个分区暂无直播间".to_string()
                };
                crate::debug_log(&format!(
                    "live area refused area={} page={page} code={} source={source} signed={} ms={elapsed} risk={}",
                    criterion.id,
                    page_data.code,
                    u8::from(page_data.signed),
                    u8::from(risk)
                ));
                let weak = weak.clone();
                let host = host.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if host.seq.load(Ordering::SeqCst) != my {
                        return;
                    }
                    if let Some(ui) = weak.upgrade() {
                        if let Ok(mut state) = host.state.lock() {
                            state.loading = false;
                            state.has_more = false;
                            state.risk = if page_data.code != 0 {
                                message.clone()
                            } else {
                                String::new()
                            };
                        }
                        ui.set_live_loading(false);
                        ui.set_live_has_more(false);
                        if page_data.code != 0 {
                            ui.set_live_status(String::new().into());
                            ui.set_live_risk(message.into());
                        } else {
                            ui.set_live_status(message.into());
                        }
                    }
                });
                return;
            }
            let rooms = page_data.rooms.clone();
            let has_more = page_data.has_more;
            let pics: Vec<String> = rooms.iter().map(|room| room.cover.clone()).collect();
            let keys: Vec<String> = rooms.iter().map(|room| room.room_id.to_string()).collect();
            let weak_a = weak.clone();
            let host_a = host.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if host_a.seq.load(Ordering::SeqCst) != my {
                    return;
                }
                if let Some(ui) = weak_a.upgrade() {
                    live_publish_area(&ui, &host_a, &rooms, page, has_more, append);
                }
            });
            let covers = host.covers.clone();
            let seq = host.seq.clone();
            let my_seq = my;
            let offset = prior_rows;
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                offset,
                move |_index, key| {
                    if seq.load(Ordering::SeqCst) == my_seq {
                        ThumbGuard::Ids(covers.clone(), key)
                    } else {
                        ThumbGuard::Seq(seq.clone(), u64::MAX)
                    }
                },
                |ui| ui.get_live_cards(),
                |row, image| row.cover = image,
                "live area".to_string(),
            )
            .await;
        });
    }));
}

/// The newest live player's counters, for the QA driver (the window's own
/// `control` is not Send, the stats are).
pub(crate) static LIVE_STATS_LAST: Mutex<Option<Arc<player::LiveStats>>> = Mutex::new(None);

/// Build and drive a 直播 room window.
///
/// Same `PlayerWindow`, same `PlaybackControl`, same close/pointer/layout
/// plumbing as a VOD window; the picture and sound come from
/// [`player::spawn_live`] instead of the DASH pacer. Nothing in this path
/// fetches a `PlayInfo`, touches the prefetch proxy, the queue or the 选集.
pub(crate) fn open_live_player(
    session: Arc<bilibili::Session>,
    room: bilibili::LiveRoom,
    state: &Rc<PlayerState>,
    settings_store: &Arc<Mutex<settings::Settings>>,
) {
    let open_started = Instant::now();
    // One live room at a time: a second card click replaces the first window
    // (the same Close → Open order the VOD Replace mode uses).
    let previous = state
        .players
        .borrow()
        .iter()
        .find(|record| record.live)
        .cloned();
    if let Some(old) = previous {
        close_player_window(
            state,
            &old.window.as_weak(),
            &Rc::downgrade(&old),
            settings_store,
            "replace",
        );
    }

    let player = match PlayerWindow::new() {
        Ok(player) => player,
        Err(_) => return,
    };
    let prefs = settings_store
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    player.global::<AppTheme>().set_dark(state.theme_dark.get());
    player
        .global::<AppTheme>()
        .set_glass_transparency(prefs.glass_transparency);
    player.set_live(true);
    player.set_live_on(room.live_status == 1);
    player.set_live_status(room.live_status.min(i32::MAX as i64) as i32);
    player.set_live_viewers(
        format!("{} 人气", live_online_text(room.online)).into(),
    );
    player.set_video_title(room.title.clone().into());
    let area = if room.area_name.is_empty() {
        room.parent_area_name.clone()
    } else {
        room.area_name.clone()
    };
    player.set_live_sub(
        if area.is_empty() {
            room.uname.clone()
        } else {
            format!("{} · {}", room.uname, area)
        }
        .into(),
    );
    // The VOD-only surfaces are empty by construction, but the models are seeded
    // so the window's bindings never read an unset model.
    player.set_info_title(room.title.clone().into());
    player.set_info_owner(format!("直播 · {}", room.uname).into());
    player.set_playing(true);
    player.set_buffering(true);
    player.set_buffer_progress(0.0);
    player.set_volume(100.0);
    player.set_volume_text("100".into());
    player.set_muted(false);
    player.set_danmaku_on(false);
    player.set_danmaku_text("弹幕关".into());
    player.set_fullscreen(false);
    player.set_fullscreen_text("全屏".into());
    player.set_fit_mode(0);
    player.set_right_panel(0);
    player.set_toasts(ModelRc::from(std::rc::Rc::new(VecModel::<ToastItem>::from(
        Vec::<ToastItem>::new(),
    ))));

    let weak = player.as_weak();
    let player_prepared = win::prepare_frameless(player.window());
    let saved_layout = prefs.player_layout;
    let layout_target = restore_player_layout(&player, saved_layout, 0);
    set_player_backdrop(&player, state.theme_dark.get());
    if player.show().is_err() {
        return;
    }
    if !player_prepared {
        win::prepare_frameless(player.window());
    }
    let (wall_x, wall_y) = publish_wallpaper(player.global::<AppTheme>(), player.window());
    crate::debug_log(&format!(
        "wallpaper origin win=player stage=show-live x={wall_x:.1} y={wall_y:.1}"
    ));
    player_min_track(player.window(), 0, 0, 0);
    verify_player_layout(player.as_weak(), layout_target, 120);
    player_opened();
    if let Some(main) = state.main.upgrade() {
        skeleton_halt(&main);
    }
    retry_subclass(state);

    let control = player::PlaybackControl::new();
    control.set_speed(1.0);
    control.set_hwaccel(false);
    control.set_interp_mode(0);
    player::set_active_control(Some(control.clone()));

    // ---- window plumbing: drag / resize / close / fullscreen / minimise ----
    let panel_geom: Rc<RefCell<PanelGeom>> = Rc::new(RefCell::new(PanelGeom::default()));
    let player_drag = Rc::new(win::WindowDrag::new());
    let player_drag_stats = Rc::new(DragFlushStats::new());
    let player_drag_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let drag = player_drag.clone();
        let timer = player_drag_timer.clone();
        let stats = player_drag_stats.clone();
        player.on_window_drag_start(move || {
            let Some(player) = weak.upgrade() else {
                return;
            };
            if let Err(reason) = drag.start(&player.window()) {
                crate::debug_log(&format!("drag denied reason={reason} source=live"));
                return;
            }
            stats.begin();
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_drag = drag.clone();
            let tick_timer = Rc::downgrade(&timer);
            let tick_stats = stats.clone();
            timer.start(slint::TimerMode::Repeated, DRAG_FRAME, move || {
                let Some(timer) = tick_timer.upgrade() else {
                    return;
                };
                let Some(player) = tick_weak.upgrade() else {
                    timer.stop();
                    return;
                };
                if !tick_drag.is_dragging() {
                    timer.stop();
                    return;
                }
                if win::is_visible(&player.window()) == Some(false) {
                    tick_stats.finish("live");
                    tick_drag.end();
                    timer.stop();
                    return;
                }
                let moved = tick_drag.flush(&player.window());
                if moved {
                    tick_stats.placement();
                }
                if !win::left_button_down() {
                    crate::debug_log(&format!(
                        "drag end reason=pointer-up deferred=0 moved={moved} source=live"
                    ));
                    tick_stats.finish("live");
                    tick_drag.end();
                    timer.stop();
                }
            });
        });
    }
    {
        let drag = player_drag.clone();
        let stats = player_drag_stats.clone();
        player.on_window_drag_move(move || {
            stats.event();
            drag.move_to();
        });
    }
    {
        let weak = weak.clone();
        let drag = player_drag.clone();
        let timer = player_drag_timer.clone();
        let geom = panel_geom.clone();
        let store = settings_store.clone();
        let stats = player_drag_stats.clone();
        player.on_window_drag_end(move || {
            if let Some(player) = weak.upgrade() {
                if !drag.end_on_signal(win::left_button_down()) {
                    crate::debug_log("drag end reason=lost-grab deferred=1 source=live");
                    return;
                }
                timer.stop();
                if drag.flush(&player.window()) {
                    stats.placement();
                }
                persist_player_layout(&geom, player.window(), &store, 0, "live-drag-end");
            }
            timer.stop();
            drag.end();
            stats.finish("live");
        });
    }
    let player_resize = Rc::new(win::WindowResize::new());
    player_resize.set_min(480, 270);
    let player_resize_timer = Rc::new(slint::Timer::default());
    {
        let weak = weak.clone();
        let resize = player_resize.clone();
        let timer = player_resize_timer.clone();
        player.on_resize_start(move |edge: i32| {
            let Some(player) = weak.upgrade() else {
                return;
            };
            if player.window().is_fullscreen() || player.window().is_maximized() {
                return;
            }
            if let Err(reason) = resize.start(&player.window(), edge) {
                crate::debug_log(&format!(
                    "resize denied reason={reason} edge={edge} source=live"
                ));
                return;
            }
            if timer.running() {
                return;
            }
            let tick_weak = weak.clone();
            let tick_resize = resize.clone();
            let tick_timer = Rc::downgrade(&timer);
            timer.start(slint::TimerMode::Repeated, DRAG_FRAME, move || {
                let Some(timer) = tick_timer.upgrade() else {
                    return;
                };
                let Some(player) = tick_weak.upgrade() else {
                    timer.stop();
                    return;
                };
                if !tick_resize.is_active() {
                    timer.stop();
                    return;
                }
                if win::is_visible(&player.window()) == Some(false) {
                    tick_resize.end();
                    timer.stop();
                    return;
                }
                tick_resize.update(&player.window());
                if !win::left_button_down() {
                    tick_resize.end();
                    timer.stop();
                }
            });
        });
    }
    {
        let resize = player_resize.clone();
        player.on_resize_move(move || resize.move_to());
    }
    {
        let weak = weak.clone();
        let resize = player_resize.clone();
        let timer = player_resize_timer.clone();
        let geom = panel_geom.clone();
        let store = settings_store.clone();
        player.on_resize_end(move || {
            if resize.is_active() && win::left_button_down() {
                return;
            }
            if let Some(player) = weak.upgrade() {
                resize.update(&player.window());
                persist_player_layout(&geom, player.window(), &store, 0, "live-resize-end");
            }
            timer.stop();
            resize.end();
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_toggle_pause(move || {
            let paused = control.toggle_paused();
            if let Some(window) = window.upgrade() {
                window.set_playing(!paused);
            }
            crate::debug_log(&format!("live pause paused={paused}"));
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_volume_changed(move |volume| {
            control.set_volume(volume as i32);
            if let Some(window) = window.upgrade() {
                window.set_volume(volume);
                window.set_volume_text(control.volume().to_string().into());
            }
        });
    }
    {
        let control = control.clone();
        let window = weak.clone();
        player.on_toggle_mute(move || {
            let muted = control.toggle_mute();
            if let Some(window) = window.upgrade() {
                window.set_muted(muted);
                window.set_volume_text(control.volume().to_string().into());
            }
        });
    }
    {
        let window = weak.clone();
        player.on_toggle_fullscreen(move || {
            if let Some(window) = window.upgrade() {
                let next = !window.window().is_fullscreen();
                window.window().set_fullscreen(next);
                window.set_fullscreen(next);
                window.set_fullscreen_text(if next { "退出全屏" } else { "全屏" }.into());
                crate::debug_log(&format!("live fullscreen next={next}"));
            }
        });
    }
    {
        let window = weak.clone();
        player.on_toggle_minimize(move || {
            if let Some(window) = window.upgrade() {
                window.window().set_minimized(true);
            }
        });
    }

    let session_rec = Rc::new(PlayerSession {
        id: next_player_session_id(),
        window: player.clone_strong(),
        control: control.clone(),
        panel_geom: panel_geom.clone(),
        hwnd: Cell::new(win::hwnd_of(player.window()).unwrap_or(0)),
        closed: Cell::new(false),
        loops: RefCell::new(None),
        mini_slot: Arc::new(Mutex::new(None)),
        video: bilibili::FeedVideo {
            bvid: format!("live-{}", room.room_id),
            cid: 0,
            aid: room.uid.max(0) as u64,
            title: room.title.clone(),
            pic: room.cover.clone(),
            owner: room.uname.clone(),
            owner_mid: room.uid,
            view: room.online,
            duration: 0,
            pub_ts: 0,
        },
        cascade: 0,
        queued: Cell::new(false),
        deferred: RefCell::new(None),
        live: true,
    });
    state.players.borrow_mut().push(session_rec.clone());
    {
        let weak = weak.clone();
        let state = state.clone();
        let session_rec = session_rec.clone();
        let store = settings_store.clone();
        player.on_player_closed(move || {
            close_player_window(&state, &weak, &Rc::downgrade(&session_rec), &store, "button");
        });
    }
    {
        let state = state.clone();
        let session_rec = session_rec.clone();
        let store = settings_store.clone();
        player.window().on_close_requested(move || {
            close_player_window(
                &state,
                &session_rec.window.as_weak(),
                &Rc::downgrade(&session_rec),
                &store,
                "native",
            );
            slint::CloseRequestResponse::HideWindow
        });
    }
    let loops = start_player_loops(state, &session_rec, settings_store);
    *session_rec.loops.borrow_mut() = Some(loops);

    // 人气 is refreshed by the live thread; the title/UP lines are already set.
    let stats = player::LiveStats::new();
    if let Ok(mut guard) = LIVE_STATS_LAST.lock() {
        *guard = Some(stats.clone());
    }
    // 头图: the room's own cover is the poster the window shows while nothing
    // has been decoded yet (it fades out with the 缓冲 overlay on the first
    // frame). Loaded off-thread, so the window is never held up by the image.
    player::live_load_poster(session.clone(), weak.clone(), room.cover.clone());
    player::spawn_live(
        session,
        player::LiveSource {
            room_id: room.room_id,
            title: room.title.clone(),
            uname: room.uname.clone(),
            area,
            is_portrait: false,
            cover: room.cover.clone(),
        },
        weak.clone(),
        session_rec.mini_slot.clone(),
        control.clone(),
        stats,
    );
    crate::debug_log(&format!(
        "live player open win={} room={} prepare_ms={} title={}",
        session_rec.id,
        room.room_id,
        open_started.elapsed().as_millis(),
        room.title
    ));
}
