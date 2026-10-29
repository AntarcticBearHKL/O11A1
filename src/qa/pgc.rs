//! 番剧/连续观看 QA (f171 + the PGC regression walks).

use crate::*;

/// `BILI_PGC_F171=1`: drive the two f171 features through their real callbacks
/// and capture the evidence:
///
///   f171_sort_follows.png / _update.png / _views.png / _score.png
///       one 分区索引 first page per verified sort, with the first card's title
///       and the sort's own `order` string logged next to it
///   f171_continue_player.png
///       the player on episode 3, 选集 open, the highlight on that row
///   f171_continue_badge.png
///       番剧 grid after closing the player: 「看到第 3 集」 on the card
///   f171_continue_resume.png
///       the card reopened: it starts on episode 3 again (same highlight)
///
/// Variants: `BILI_PGC_F171_ROW=<n>` picks which 分区索引 card the 继续观看 run
/// opens (0 = the 追番最多 top card); `BILI_PGC_F171_RESTART=1` only reads back
/// what a FRESH process loaded from `settings.json` (dropdown + first page);
/// `BILI_PGC_F171_REGRESS=1` opens 点播视频 / 直播 / 养鱼 once each.
///
/// Everything goes through `invoke_*`, and every number in the log is read back
/// from the UI models / the progress table, never assumed.
pub(crate) fn run_pgc_f171_test(weak: Weak<AppWindow>, session_slot: SessionSlot, host: PgcHost) {
    std::thread::spawn(move || {
        for _ in 0..600 {
            if active_session(&session_slot).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let wait_ui = |pred: fn(&AppWindow) -> bool, timeout_ms: u64| -> bool {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            while Instant::now() < deadline {
                if f145_on_ui(&weak, move |ui| pred(ui)).unwrap_or(false) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            false
        };
        let shot = |tag: &str| {
            let tag = tag.to_string();
            let _ = f145_on_ui(&weak, move |ui| {
                let window = ui.window();
                f155_shot(&window, &tag)
            });
        };
        let first_card = || -> Option<(String, String, String, String)> {
            f145_on_ui(&weak, |ui| {
                let rows = ui.get_pgc_items();
                let row = rows.row_data(0)?;
                Some((
                    row.title.to_string(),
                    row.follows.to_string(),
                    row.progress.to_string(),
                    ui.get_pgc_sort().to_string(),
                ))
            })
            .flatten()
        };
        let store_dump = || -> String {
            let store = continue_store();
            let Ok(guard) = store.lock() else {
                return String::new();
            };
            let rows: Vec<String> = guard
                .seasons
                .iter()
                .map(|(season_id, entry)| {
                    format!(
                        "sid={season_id} ep={} idx={} cid={} title={:?} at={}",
                        entry.ep_id, entry.index, entry.cid, entry.title, entry.at
                    )
                })
                .collect();
            rows.join(" | ")
        };
        let log_store = |label: &str| {
            let path = settings::ContinueStore::path();
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .replace('\n', " ")
                .replace("  ", " ");
            crate::debug_log(&format!(
                "f171 progress {label} path={} json_len={} rows={} json={raw}",
                path.display(),
                raw.len(),
                store_dump()
            ));
        };

        wait_ui(|ui| ui.get_logged_in(), 60_000);
        std::thread::sleep(Duration::from_millis(700));

        // Which 分区索引 card the 继续观看 run opens (0 = the 追番最多 top card).
        // `BILI_PGC_F171_ROW` lets a run name a season whose 3rd episode is
        // playable through the verified DASH route.
        let open_row: i32 = env::var("BILI_PGC_F171_ROW")
            .and_then(|value| value.trim().parse::<i32>().ok())
            .unwrap_or(0)
            .max(0);

        // `BILI_PGC_F171_RESTART=1`: only read back what a FRESH process loaded
        // from `settings.json` (the dropdown + the current first page).
        if env::flag("BILI_PGC_F171_RESTART")
        {
            let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(1));
            let ready = wait_ui(
                |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
                40_000,
            );
            std::thread::sleep(Duration::from_millis(1200));
            let stored = host
                .settings
                .lock()
                .map(|prefs| prefs.pgc_sorts.clone())
                .unwrap_or_default();
            crate::debug_log(&format!(
                "f171 restart ready={ready} sort_ui={} stored={stored:?} first={:?}",
                f145_on_ui(&weak, |ui| ui.get_pgc_sort()).unwrap_or(-1),
                first_card()
            ));
            shot("f171_sort_restart");
            return;
        }

        // `BILI_PGC_F171_REGRESS=1`: the three unrelated paths this change could
        // touch, each opened once through its own real entry point.
        if env::flag("BILI_PGC_F171_REGRESS")
        {
            let wait_frames = |timeout_ms: u64| -> u64 {
                let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                while Instant::now() < deadline {
                    let frames = f145_on_ui(&weak, |_| {
                        f145_latest_player().map(|session| session.control.presented_frames())
                    })
                    .flatten()
                    .unwrap_or(0);
                    if frames > 0 {
                        return frames;
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
                0
            };
            let close = || {
                let _ = f145_on_ui(&weak, |_| {
                    if let Some(session) = f145_latest_player() {
                        session.window.invoke_player_closed();
                    }
                });
                std::thread::sleep(Duration::from_millis(1500));
            };

            // 1) 点播普通视频: the home feed's first card through `card_open`.
            let _ = f145_on_ui(&weak, |ui| ui.invoke_home_section(0));
            wait_ui(|ui| ui.get_view_mode() == 0 && ui.get_feed_items().row_count() > 0, 30_000);
            std::thread::sleep(Duration::from_millis(600));
            let vod_bytes = f145_on_ui(&weak, |ui| {
                ui.get_feed_items().row_data(0).map(|row| row.title.to_string())
            })
            .flatten();
            let _ = f145_on_ui(&weak, |ui| ui.invoke_card_open(0, 0));
            let vod_frames = wait_frames(40_000);
            let vod_player = f145_on_ui(&weak, |_| {
                f145_latest_player().map(|session| {
                    (
                        session.window.get_video_title().to_string(),
                        session.window.get_series_kind(),
                        session.window.get_time_total().to_string(),
                    )
                })
            })
            .flatten();
            crate::debug_log(&format!(
                "f171 regress vod feed_row={vod_bytes:?} frames={vod_frames} player={vod_player:?}"
            ));
            let _ = f145_on_ui(&weak, |_| {
                if let Some(session) = f145_latest_player() {
                    let window = session.window.window();
                    f155_shot(&window, "f171_regress_vod");
                }
            });
            close();

            // 2) 直播: the 直播 page, then its first room.
            let _ = f145_on_ui(&weak, |ui| ui.invoke_live_open());
            let live_ready = wait_ui(
                |ui| ui.get_view_mode() == 23 && ui.get_live_cards().row_count() > 0,
                40_000,
            );
            std::thread::sleep(Duration::from_millis(800));
            let live_rows = f145_on_ui(&weak, |ui| (ui.get_live_cards().row_count(), ui.get_live_modules().row_count()))
                .unwrap_or((0, 0));
            let _ = f145_on_ui(&weak, |ui| ui.invoke_live_room_open(0));
            let live_frames = wait_frames(40_000);
            let live_player = f145_on_ui(&weak, |_| {
                f145_latest_player().map(|session| {
                    (
                        session.window.get_video_title().to_string(),
                        session.window.get_live(),
                        session.window.get_time_total().to_string(),
                    )
                })
            })
            .flatten();
            crate::debug_log(&format!(
                "f171 regress live ready={live_ready} rows={live_rows:?} frames={live_frames} player={live_player:?}"
            ));
            let _ = f145_on_ui(&weak, |_| {
                if let Some(session) = f145_latest_player() {
                    let window = session.window.window();
                    f155_shot(&window, "f171_regress_live");
                }
            });
            close();

            // 3) 养鱼: the page itself (its engine is untouched by this change).
            let _ = f145_on_ui(&weak, |ui| ui.invoke_fish_open());
            let fish_ready = wait_ui(|ui| ui.get_view_mode() == 20, 20_000);
            std::thread::sleep(Duration::from_millis(1200));
            let fish = f145_on_ui(&weak, |ui| {
                (
                    ui.get_view_mode(),
                    ui.get_fish_rows().row_count(),
                    ui.get_fish_now().to_string(),
                    ui.get_fish_last_line().to_string(),
                    ui.get_fish_error_text().to_string(),
                )
            });
            crate::debug_log(&format!(
                "f171 regress fish ready={fish_ready} view/rows/now/last/err={fish:?}"
            ));
            shot("f171_regress_fish");
            // 4) back home: nothing left on the 番剧 page.
            let _ = f145_on_ui(&weak, |ui| ui.invoke_home_section(0));
            wait_ui(|ui| ui.get_view_mode() == 0, 10_000);
            crate::debug_log(&format!(
                "f171 regress home view={}",
                f145_on_ui(&weak, |ui| ui.get_view_mode()).unwrap_or(-1)
            ));
            crate::debug_log("f171 regress done");
            return;
        }

        // ---- 1: the four verified sorts, one first page each ----
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(1));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            40_000,
        );
        for index in 0..4i32 {
            let key = pgc_sort_key_at(index);
            // Scroll the grid away from the top first (twice, so pages of 20
            // cards have something to scroll), then prove a sort switch takes it
            // back to 0 and installs the new first page.
            let _ = f145_on_ui(&weak, |ui| ui.set_pgc_grid_scroll(-420.0));
            std::thread::sleep(Duration::from_millis(250));
            let scrolled = f145_on_ui(&weak, |ui| f32::from(ui.get_pgc_grid_scroll())).unwrap_or(0.0);
            let _ = f145_on_ui(&weak, move |ui| ui.invoke_pgc_sort_pick(index));
            let ready = wait_ui(
                |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
                40_000,
            );
            std::thread::sleep(Duration::from_millis(1200));
            let after = f145_on_ui(&weak, |ui| f32::from(ui.get_pgc_grid_scroll())).unwrap_or(0.0);
            let card = first_card();
            let state = host.state.lock().map(|state| {
                (
                    state.tab,
                    state.sort.clone(),
                    state.index_items.first().map(|item| item.title.clone()),
                    state.index_items.first().map(|item| item.order.clone()),
                    state.index_items.first().map(|item| item.index_show.clone()),
                    state.index_items.first().map(|item| item.season_id),
                )
            });
            crate::debug_log(&format!(
                "f171 sort key={key} order={} ready={ready} scroll_before={scrolled:.1} scroll_after={after:.1} page={:?} first={card:?} state={state:?}",
                pgc_sort_order(key),
                f145_on_ui(&weak, |ui| ui.get_qa_pgc_page()).unwrap_or(0)
            ));
            shot(&format!("f171_sort_{key}"));
        }

        // ---- 2: the choice is remembered per season_type ----
        // 国创 (season_type 4) gets its own sort; going back to 番剧 must restore
        // the 番剧 one, and re-entering 国创 must restore its own.
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(2));
        wait_ui(
            |ui| ui.get_pgc_tab() == 2 && !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            40_000,
        );
        crate::debug_log(&format!(
            "f171 restore guochuang_pick sort_ui={}",
            f145_on_ui(&weak, |ui| ui.get_pgc_sort()).unwrap_or(-1)
        ));
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_sort_pick(1));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            40_000,
        );
        std::thread::sleep(Duration::from_millis(900));
        crate::debug_log(&format!(
            "f171 restore guochuang sort_ui={} first={:?}",
            f145_on_ui(&weak, |ui| ui.get_pgc_sort()).unwrap_or(-1),
            first_card()
        ));
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(1));
        wait_ui(
            |ui| ui.get_pgc_tab() == 1 && !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            40_000,
        );
        crate::debug_log(&format!(
            "f171 restore back_to_bangumi sort_ui={} first={:?}",
            f145_on_ui(&weak, |ui| ui.get_pgc_sort()).unwrap_or(-1),
            first_card()
        ));
        let stored = host
            .settings
            .lock()
            .map(|prefs| prefs.pgc_sorts.clone())
            .unwrap_or_default();
        crate::debug_log(&format!("f171 restore stored={stored:?}"));

        // Back to 追番最多 so the continue run opens a well-known season.
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_sort_pick(0));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            40_000,
        );
        std::thread::sleep(Duration::from_millis(1200));
        let first = host
            .state
            .lock()
            .ok()
            .and_then(|state| state.index_items.get(open_row as usize).cloned());
        let Some(first) = first else {
            crate::debug_log("f171 continue skipped reason=no-index-row");
            return;
        };
        crate::debug_log(&format!(
            "f171 continue row={open_row} season={} title={:?}",
            first.season_id, first.title
        ));

        // ---- 3: watch episode 3 of that season ----
        let _ = f145_on_ui(&weak, move |ui| ui.invoke_pgc_open(open_row));
        let mut frames = 0u64;
        for _ in 0..400 {
            frames = f145_on_ui(&weak, |_| {
                f145_latest_player().map(|session| session.control.presented_frames())
            })
            .flatten()
            .unwrap_or(0);
            if frames > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        crate::debug_log(&format!("f171 continue player frames={frames}"));
        // The first-frame write is for episode 1; wait until the table has it.
        for _ in 0..50 {
            if continue_store()
                .lock()
                .map(|store| store.get(first.season_id).is_some())
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log_store("after-first-frame");

        // Pick the third 正片 row: the highlight AND the stored row must follow.
        let picked = f145_on_ui(&weak, |_| {
            let session = f145_latest_player()?;
            let rows = session.window.get_series_rows();
            let mut seen = 0;
            for row_index in 0..rows.row_count() {
                let row = rows.row_data(row_index)?;
                if row.header {
                    continue;
                }
                seen += 1;
                if seen == 3 {
                    return Some((row_index as i32, row.title.to_string(), row.label.to_string()));
                }
            }
            None
        })
        .flatten();
        let Some((row_index, row_title, row_label)) = picked else {
            crate::debug_log("f171 continue skipped reason=no-third-row");
            return;
        };
        crate::debug_log(&format!(
            "f171 continue pick row={row_index} label={row_label} title={row_title:?}"
        ));
        let _ = f145_on_ui(&weak, move |_| {
            if let Some(session) = f145_latest_player() {
                session.window.invoke_series_pick(row_index);
            }
        });
        let mut stored_index = 0u32;
        for _ in 0..200 {
            stored_index = continue_store()
                .lock()
                .ok()
                .and_then(|store| store.get(first.season_id).map(|entry| entry.index))
                .unwrap_or(0);
            if stored_index == 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        crate::debug_log(&format!(
            "f171 continue after-pick stored_index={stored_index} frames={}",
            f145_on_ui(&weak, |_| {
                f145_latest_player().map(|session| session.control.presented_frames())
            })
            .flatten()
            .unwrap_or(0)
        ));
        log_store("after-pick");

        // 选集 sidebar open, highlight + scroll read back from the window.
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                session.window.set_qa_hold_chrome(true);
                session.window.set_right_panel(1);
                session.window.set_right_tab(4);
                session.window.invoke_panel_changed();
            }
        });
        std::thread::sleep(Duration::from_millis(1200));
        let sidebar = f145_on_ui(&weak, |_| {
            let session = f145_latest_player()?;
            let rows = session.window.get_series_rows();
            let current = (0..rows.row_count()).find(|index| {
                rows.row_data(*index)
                    .map(|row| row.current && !row.header)
                    .unwrap_or(false)
            });
            Some((
                session.window.get_series_scroll(),
                current,
                current.and_then(|index| rows.row_data(index)).map(|row| {
                    (row.label.to_string(), row.title.to_string())
                }),
                session.control.source().bvid,
            ))
        })
        .flatten();
        crate::debug_log(&format!(
            "f171 continue sidebar scroll/current/row/bvid={sidebar:?}"
        ));
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                let window = session.window.window();
                f155_shot(&window, "f171_continue_player");
            }
        });

        // ---- 4: close the player through the real ✕ callback ----
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                session.window.invoke_player_closed();
            }
        });
        std::thread::sleep(Duration::from_millis(1500));
        crate::debug_log(&format!(
            "f171 continue closed players={}",
            f145_on_ui(&weak, |_| f145_latest_player().is_some()).unwrap_or(false)
        ));
        // Badge on the card behind the player.
        let badge = f145_on_ui(&weak, |ui| {
            let rows = ui.get_pgc_items();
            (0..rows.row_count())
                .filter_map(|index| rows.row_data(index))
                .find(|row| !row.progress.is_empty())
                .map(|row| {
                    (
                        row.title.to_string(),
                        row.progress.to_string(),
                        ui.get_view_mode(),
                        ui.get_pgc_tab(),
                        ui.get_pgc_sub(),
                    )
                })
        })
        .flatten();
        crate::debug_log(&format!("f171 continue badge={badge:?}"));
        shot("f171_continue_badge");
        log_store("after-close");

        // ---- 5: reopen the same card: it must resume on episode 3 ----
        let _ = f145_on_ui(&weak, move |ui| ui.invoke_pgc_open(open_row));
        // The gate + season fetch land the toast a beat later; poll for it, and
        // photograph the main window while it is up.
        let mut toast = String::new();
        for _ in 0..80 {
            toast = f145_on_ui(&weak, |ui| ui.get_toast_text().to_string()).unwrap_or_default();
            if !toast.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !toast.is_empty() {
            shot("f171_continue_toast");
        }
        let mut resumed_frames = 0u64;
        for _ in 0..400 {
            resumed_frames = f145_on_ui(&weak, |_| {
                f145_latest_player().map(|session| session.control.presented_frames())
            })
            .flatten()
            .unwrap_or(0);
            if resumed_frames > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(800));
        let resume = f145_on_ui(&weak, |_| {
            let session = f145_latest_player()?;
            let rows = session.window.get_series_rows();
            let current = (0..rows.row_count()).find(|index| {
                rows.row_data(*index)
                    .map(|row| row.current && !row.header)
                    .unwrap_or(false)
            });
            Some((
                session.window.get_video_title().to_string(),
                session.window.get_series_scroll(),
                current,
                current.and_then(|index| rows.row_data(index)).map(|row| {
                    (row.label.to_string(), row.title.to_string())
                }),
                session.control.source().bvid,
            ))
        })
        .flatten();
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                session.window.set_qa_hold_chrome(true);
                session.window.set_right_panel(1);
                session.window.set_right_tab(4);
                session.window.invoke_panel_changed();
            }
        });
        std::thread::sleep(Duration::from_millis(1000));
        crate::debug_log(&format!(
            "f171 continue resume frames={resumed_frames} toast={toast:?} title/scroll/current/row/bvid={resume:?}"
        ));
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                let window = session.window.window();
                f155_shot(&window, "f171_continue_resume");
            }
        });
        log_store("final");
        // Close this player, then leave the page on 评分最高: the restart run
        // proves a NON-default choice survived (settings.json + the dropdown).
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                session.window.invoke_player_closed();
            }
        });
        std::thread::sleep(Duration::from_millis(1200));
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_sort_pick(3));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            40_000,
        );
        std::thread::sleep(Duration::from_millis(1000));
        let stored = host
            .settings
            .lock()
            .map(|prefs| prefs.pgc_sorts.clone())
            .unwrap_or_default();
        crate::debug_log(&format!(
            "f171 final sort_ui={} stored={stored:?} first={:?}",
            f145_on_ui(&weak, |ui| ui.get_pgc_sort()).unwrap_or(-1),
            first_card()
        ));
        shot("f171_sort_final_score");
        crate::debug_log("f171 done");
    });
}

/// `BILI_PGC_TEST=1`: drive the whole 番剧 feature through its real callbacks
/// (no synthetic input, no fixtures) and capture the five screenshots:
///
///   f156_timeline.png       每日放送 with today selected
///   f156_index_bangumi.png  番剧 推荐
///   f156_index_guochuang.png 国创 推荐
///   f156_rank.png           番剧 排行榜
///   f156_player_bangumi.png player + the season's full 选集 panel
///
/// It also logs the real per-day counts, the index totals, page=2 after a
/// scroll-append, the click→first-frame and episode-switch→first-frame times,
/// and probes a few seasons for a genuine `-10403` (region lock).
pub(crate) fn run_pgc_test(weak: Weak<AppWindow>, session_slot: SessionSlot, host: PgcHost) {
    std::thread::spawn(move || {
        // Wait for the stored account to resolve.
        for _ in 0..600 {
            if active_session(&session_slot).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let wait_ui = |pred: fn(&AppWindow) -> bool, timeout_ms: u64| -> bool {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            while Instant::now() < deadline {
                if f145_on_ui(&weak, move |ui| pred(ui)).unwrap_or(false) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            false
        };
        let shot = |tag: &str| {
            let tag = tag.to_string();
            let _ = f145_on_ui(&weak, move |ui| {
                let window = ui.window();
                f155_shot(&window, &tag);
            });
        };
        let log_state = |label: &str| {
            if let Ok(state) = host.state.lock() {
                let per_day = state
                    .timeline
                    .iter()
                    .map(|day| format!("{}{}={}", day.date, if day.is_today { "*" } else { "" }, day.episodes.len()))
                    .collect::<Vec<_>>()
                    .join(",");
                crate::debug_log(&format!(
                    "pgc-test {label} tab={} sub={} day={} days={} per_day=[{per_day}] index_items={} rank_items={} total={} page={}",
                    state.tab,
                    state.sub,
                    state.day,
                    state.timeline.len(),
                    state.index_items.len(),
                    state.rank_items.len(),
                    state.total,
                    state.page
                ));
            }
        };

        // The main window finishes its logged-in transition after the account
        // handshake; the page is only rendered on that side of it.
        wait_ui(|ui| ui.get_logged_in(), 60_000);
        std::thread::sleep(Duration::from_millis(600));

        // ---- 1: 每日放送 ----
        let began = Instant::now();
        let _ = f145_on_ui(&weak, |ui| ui.invoke_home_section(3));
        let ready = wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_eps().row_count() > 0,
            30_000,
        );
        crate::debug_log(&format!(
            "pgc-test timeline ready={ready} ms={} days={} eps={} today_index={}",
            began.elapsed().as_millis(),
            f145_on_ui(&weak, |ui| ui.get_pgc_days().row_count()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_pgc_eps().row_count()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_pgc_day()).unwrap_or(-1)
        ));
        log_state("timeline");
        std::thread::sleep(Duration::from_millis(900));
        shot("f156_timeline");

        // ---- 2: 番剧 index ----
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(1));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            30_000,
        );
        log_state("index_bangumi");
        crate::debug_log(&format!(
            "pgc-test index_bangumi rows={} total={} page={}",
            f145_on_ui(&weak, |ui| ui.get_pgc_items().row_count()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_pgc_total()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_qa_pgc_page()).unwrap_or(0)
        ));
        std::thread::sleep(Duration::from_millis(1200));
        shot("f156_index_bangumi");

        // ---- 3: 国创 index ----
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(2));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            30_000,
        );
        log_state("index_guochuang");
        crate::debug_log(&format!(
            "pgc-test index_guochuang rows={} total={} page={}",
            f145_on_ui(&weak, |ui| ui.get_pgc_items().row_count()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_pgc_total()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_qa_pgc_page()).unwrap_or(0)
        ));
        std::thread::sleep(Duration::from_millis(1200));
        shot("f156_index_guochuang");

        // ---- 4: 排行榜 (back on 番剧) ----
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(1));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            30_000,
        );
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_sub_pick(1));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            30_000,
        );
        crate::debug_log(&format!(
            "pgc-test rank rows={} page={}",
            f145_on_ui(&weak, |ui| ui.get_pgc_items().row_count()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_qa_pgc_page()).unwrap_or(0)
        ));
        std::thread::sleep(Duration::from_millis(1200));
        shot("f156_rank");

        // ---- 5: scroll paging: request page 2 and prove it landed ----
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_sub_pick(0));
        wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_pgc_items().row_count() > 0,
            30_000,
        );
        let first_rows = f145_on_ui(&weak, |ui| ui.get_pgc_items().row_count()).unwrap_or(0);
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_more());
        let paged = wait_ui(
            |ui| !ui.get_pgc_loading() && ui.get_qa_pgc_page() >= 2,
            30_000,
        );
        std::thread::sleep(Duration::from_millis(900));
        crate::debug_log(&format!(
            "pgc-test paging first_rows={first_rows} page2_ready={paged} rows={} page={}",
            f145_on_ui(&weak, |ui| ui.get_pgc_items().row_count()).unwrap_or(0),
            f145_on_ui(&weak, |ui| ui.get_qa_pgc_page()).unwrap_or(0)
        ));
        log_state("paged");

        // ---- 6: open a card and measure to the first frame ----
        // 每日放送, first day (10-24): the first season there is playable in
        // BOTH of its first episodes, so the switch below has a live target.
        // (Today's first rows can be region-locked, and one such gate IS the
        // -10403 evidence this run logs separately.)
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(0));
        wait_ui(
            |ui| ui.get_pgc_tab() == 0
                && !ui.get_pgc_loading()
                && ui.get_pgc_eps().row_count() > 0,
            30_000,
        );
        std::thread::sleep(Duration::from_millis(500));
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_day_pick(0));
        let day_ok = wait_ui(|ui| ui.get_pgc_day() == 0, 5_000);
        crate::debug_log(&format!(
            "pgc-test day pick ok={day_ok} ui_day={} state_day={}",
            f145_on_ui(&weak, |ui| ui.get_pgc_day()).unwrap_or(-1),
            host.state.lock().map(|state| state.day).unwrap_or(-1)
        ));
        std::thread::sleep(Duration::from_millis(800));
        let click_at = Instant::now();
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_open(0));
        let mut first_frame_ms = 0u128;
        for _ in 0..300 {
            let frames = f145_on_ui(&weak, |_| {
                f145_latest_player().map(|session| session.control.presented_frames())
            })
            .flatten()
            .unwrap_or(0);
            if frames > 0 {
                first_frame_ms = click_at.elapsed().as_millis();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        crate::debug_log(&format!(
            "pgc-test card-first-frame ms={first_frame_ms} player={}",
            f145_on_ui(&weak, |_| f145_latest_player().is_some()).unwrap_or(false)
        ));
        let series_info = f145_on_ui(&weak, |_| {
            let session = f145_latest_player()?;
            Some((
                session.window.get_series_kind(),
                session.window.get_series_count(),
                session.window.get_series_rows().row_count(),
            ))
        })
        .flatten();
        crate::debug_log(&format!("pgc-test series kind/count/rows={series_info:?}"));

        // Open the 选集 panel for the capture.
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                session.window.set_qa_hold_chrome(true);
                session.window.set_right_panel(1);
                session.window.set_right_tab(4);
                session.window.invoke_panel_changed();
            }
        });
        std::thread::sleep(Duration::from_millis(1200));
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                let window = session.window.window();
                f155_shot(&window, "f156_player_bangumi");
            }
        });

        // The info tab carries score / 类型 / 首播 / 状态 / 简介 for a 番剧.
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                session.window.set_right_tab(0);
                session.window.invoke_panel_changed();
            }
        });
        std::thread::sleep(Duration::from_millis(900));
        let info = f145_on_ui(&weak, |_| {
            let session = f145_latest_player()?;
            Some((
                session.window.get_info_owner().to_string(),
                session.window.get_info_stats().to_string(),
                session.window.get_info_desc().chars().count(),
            ))
        })
        .flatten();
        crate::debug_log(&format!("pgc-test info owner/stats/desc_len={info:?}"));
        let _ = f145_on_ui(&weak, |_| {
            if let Some(session) = f145_latest_player() {
                let window = session.window.window();
                f155_shot(&window, "f156_player_info");
            }
        });

        // ---- 7: pick the SECOND episode and measure to its first frame ----
        let pick = f145_on_ui(&weak, |_| {
            let session = f145_latest_player()?;
            let rows = session.window.get_series_rows();
            let mut seen = 0;
            let mut index = None;
            let mut title = String::new();
            for row_index in 0..rows.row_count() {
                if let Some(row) = rows.row_data(row_index) {
                    if row.header {
                        continue;
                    }
                    seen += 1;
                    if seen == 2 {
                        index = Some(row_index as i32);
                        title = row.title.to_string();
                        break;
                    }
                }
            }
            let frames = session.control.presented_frames();
            let bvid = session.control.source().bvid;
            index.map(|index| (index, title, frames, bvid))
        })
        .flatten();
        if let Some((index, title, frames_before, bvid_before)) = pick {
            let switch_at = Instant::now();
            let _ = f145_on_ui(&weak, move |_| {
                if let Some(session) = f145_latest_player() {
                    session.window.invoke_series_pick(index);
                }
            });
            let mut switch_ms = 0u128;
            let mut bvid_now = bvid_before.clone();
            for _ in 0..300 {
                let state = f145_on_ui(&weak, |_| {
                    f145_latest_player().map(|session| {
                        (
                            session.control.presented_frames(),
                            session.control.source().bvid,
                        )
                    })
                })
                .flatten();
                if let Some((frames, bvid)) = state {
                    bvid_now = bvid;
                    if frames > frames_before + 2 && bvid_now != bvid_before {
                        switch_ms = switch_at.elapsed().as_millis();
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            crate::debug_log(&format!(
                "pgc-test switch source before={bvid_before} after={bvid_now}"
            ));
            let now_title = f145_on_ui(&weak, |_| {
                f145_latest_player()
                    .map(|session| session.window.get_video_title().to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
            crate::debug_log(&format!(
                "pgc-test switch-first-frame ms={switch_ms} row={index} target={title:?} title_now={now_title:?}"
            ));
        } else {
            crate::debug_log("pgc-test switch skipped reason=no-second-episode");
        }

        // ---- 8: a blocked row: B站's own reason on the main window ----
        // Today's 每日放送 rows are the ones this run actually saw refused; the
        // click goes through the same gate, so the toast text is the site's
        // reason, never a claim that playback started.
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_tab_pick(0));
        wait_ui(
            |ui| ui.get_pgc_tab() == 0 && !ui.get_pgc_loading(),
            30_000,
        );
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_day_pick(3));
        wait_ui(|ui| ui.get_pgc_day() == 3, 5_000);
        std::thread::sleep(Duration::from_millis(500));
        let _ = f145_on_ui(&weak, |ui| ui.invoke_pgc_open(0));
        std::thread::sleep(Duration::from_millis(1000));
        let blocked = f145_on_ui(&weak, |ui| {
            (
                ui.get_toast_text().to_string(),
                ui.get_toast_kind(),
                ui.get_view_mode(),
            )
        })
        .unwrap_or_default();
        crate::debug_log(&format!(
            "pgc-test blocked toast text={:?} kind={} view={}",
            blocked.0, blocked.1, blocked.2
        ));
        shot("f156_blocked");

        // ---- 9: a real region-gate sample (best effort, no invention) ----
        let candidates: Vec<(i64, String)> = host
            .state
            .lock()
            .map(|state| {
                state
                    .rank_items
                    .iter()
                    .take(10)
                    .map(|item| (item.season_id, item.title.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(session) = active_session(&session_slot) {
            if let Some(runtime) = crate::bilibili::shared_runtime() {
                runtime.block_on(async move {
                    for (season_id, title) in candidates {
                        let Ok(season) = session.pgc_season(season_id).await else {
                            continue;
                        };
                        let now = unix_now();
                        let Some(episode) = season.first_episode(now) else {
                            continue;
                        };
                        let gate = session.pgc_gate(episode.ep_id, episode.cid).await;
                        match gate {
                            Ok(gate) => {
                                crate::debug_log(&format!(
                                    "pgc-test gate season={season_id} title={title:?} ep={} code={} message={:?}",
                                    episode.ep_id, gate.code, gate.message
                                ));
                                if gate.code == -10403 {
                                    crate::debug_log(&format!(
                                        "pgc-test gate sample region-lock season={season_id} title={title:?} ep={} code=-10403",
                                        episode.ep_id
                                    ));
                                    break;
                                }
                            }
                            Err(err) => {
                                crate::debug_log(&format!(
                                    "pgc-test gate season={season_id} err={err:#}"
                                ));
                            }
                        }
                    }
                });
            }
        }
        crate::debug_log("pgc-test done");
    });
}
