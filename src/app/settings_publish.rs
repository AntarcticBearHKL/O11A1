//! Settings: reading, publishing, and pushing them into the player.

use crate::*;

/// Merge the player's playable stream catalogue with the tiers the video
/// advertises but this account cannot play.
///
/// The player's `quality_specs` list is one row per DASH stream (1080P as AVC /
/// HEVC / AV1, …). `tiers` is the API's `accept_quality` list, where a `qn`
/// with no returned stream is a 大会员-only tier. Rows are ordered high→low by
/// `qn`; the sort is stable, so the catalogue's own bandwidth order is kept
/// inside a tier. A tier that already has stream rows never gets a second,
/// locked row.
pub(crate) fn build_quality_menu(
    specs: &[player::QualitySpec],
    tiers: &[bilibili::QualityTier],
    current: Option<(i64, i64, u64)>,
) -> Vec<QualityMenuItem> {
    let mut items: Vec<QualityMenuItem> = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| QualityMenuItem {
            row: QualityMenuRow {
                label: spec.label.clone().into(),
                detail: spec.detail.clone().into(),
                badge: spec.badge.clone().into(),
                current: current == Some((spec.qn, spec.codecid, spec.bandwidth)),
                locked: false,
                qn: spec.qn as i32,
            },
            spec: Some(index),
        })
        .collect();
    for tier in tiers.iter().filter(|tier| tier.locked) {
        if specs.iter().any(|spec| spec.qn == tier.qn) {
            continue;
        }
        items.push(QualityMenuItem {
            row: QualityMenuRow {
                label: tier.label.clone().into(),
                detail: tier.detail.clone().into(),
                badge: "大会员".into(),
                current: false,
                locked: true,
                qn: tier.qn as i32,
            },
            spec: None,
        });
    }
    items.sort_by(|left, right| right.row.qn.cmp(&left.row.qn));
    items
}

/// Re-publish the 画质 menu so `current` follows the decoder.
///
/// `specs` is the catalogue `player::run` installed from `PlayInfo::qualities`
/// (one entry per raw DASH stream), and `tiers` is the full advertised tier
/// list from the same `playurl` response. The published row index is what
/// `quality_pick(index)` reports back; [`build_quality_menu`] is the only
/// mapping, so a locked row can never resolve to a stream.
pub(crate) fn publish_quality_menu(
    window: &PlayerWindow,
    specs: &[player::QualitySpec],
    tiers: &[bilibili::QualityTier],
    current: Option<(i64, i64, u64)>,
) {
    let items = build_quality_menu(specs, tiers, current);
    let locked = items.iter().filter(|item| item.row.locked).count();
    let marked = items
        .iter()
        .find(|item| item.row.current)
        .map(|item| item.row.label.to_string())
        .unwrap_or_else(|| "-".to_string());
    let list = items
        .iter()
        .map(|item| {
            format!(
                "{}:{}{}",
                item.row.qn,
                item.row.label,
                if item.row.locked { "[lock]" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let rows: Vec<QualityMenuRow> = items.into_iter().map(|item| item.row).collect();
    crate::debug_log(&format!(
        "quality rows n={} locked={locked} current={marked} list={list}",
        rows.len()
    ));
    window.set_quality_menu_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// Publish the freshly built session to the shared slot.
pub(crate) fn publish_session(session_slot: &SessionSlot, session: Arc<bilibili::Session>) {
    if let Ok(mut guard) = session_slot.lock() {
        *guard = Some(session);
    }
}

/// Publish the top-left identity strip (nickname + numeric mid) as soon as the
/// account is known.
///
/// Called right after a successful `nav()` for both a saved-account restore and
/// a fresh QR login, before the avatar/relation fetches in [`show_user`], so the
/// strip is filled before `show_home_after_login` reveals the home view and never
/// shows an empty name.
pub(crate) fn publish_identity(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    user: &bilibili::NavUser,
    source: &str,
) {
    let uname = user.uname.clone();
    let mid = user.mid.to_string();
    crate::debug_log(&format!("identity user={uname} mid={mid} source={source}"));
    post(weak, generation, my, move |ui| {
        ui.set_user_name(uname.into());
        ui.set_user_id(mid.into());
    });
}

/// Fetch best-effort extras and push the full account panel to the UI.
pub(crate) async fn show_user(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    session: &bilibili::Session,
    user: bilibili::NavUser,
) {
    let relation = session.relation(user.mid).await.ok();
    let _ = session.myinfo().await; // best-effort; result intentionally unused
    let avatar = if user.face.is_empty() {
        None
    } else {
        session.fetch_avatar(&user.face).await.ok()
    };

    let uname = user.uname.clone();
    let mid = user.mid.to_string();
    let level = format!("Lv{}", user.level);
    let coins = format!("{:.1}", user.money);
    let moral = format!("{}", user.moral as i64);
    let vip = user.vip_text.clone();
    let vip_active = user.vip_status;
    let (following, follower) = match &relation {
        Some(r) => (r.following.to_string(), r.follower.to_string()),
        None => ("—".to_string(), "—".to_string()),
    };

    post(weak, generation, my, move |ui| {
        ui.set_user_name(uname.clone().into());
        ui.set_user_id(mid.clone().into());
        ui.set_p_uname(uname.into());
        ui.set_p_mid(mid.into());
        ui.set_p_level(level.into());
        ui.set_p_coins(coins.into());
        ui.set_p_moral(moral.into());
        ui.set_p_vip(vip.into());
        ui.set_p_vip_active(vip_active);
        ui.set_p_following(following.into());
        ui.set_p_follower(follower.into());

        if let Some((bytes, w, h)) = avatar {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
            buffer.make_mut_bytes().copy_from_slice(&bytes);
            ui.set_avatar(Image::from_rgba8(buffer));
        }

        // The home view is revealed by `show_home_after_login`, once a feed
        // batch is ready (or after at most `FEED_HOME_WAIT`), so the user does
        // not land on an empty page. Account fields above are already set.
        ui.set_status("已登录".into());
        update_page_bar(ui);
    });
}

/// Fetch the saved account's face for the login chooser.
///
/// The face is a public CDN image, so this needs no session and runs long
/// before the auto-login call finishes. One attempt per launch; a failure just
/// leaves the circular placeholder (the account's name glyph) in place.
pub(crate) fn load_saved_avatar(weak: Weak<AppWindow>, url: String) {
    crate::netpool::submit(crate::netpool::Prio::Low, Box::new(move || {
        let Some(rt) = bilibili::shared_runtime() else {
            return;
        };
        let fetched = rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .build()
                .ok()?;
            let bytes = client
                .get(&url)
                .header(
                    reqwest::header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
                )
                .header(reqwest::header::REFERER, "https://www.bilibili.com/")
                .send()
                .await
                .ok()?
                .bytes()
                .await
                .ok()?;
            let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
            let (width, height) = decoded.dimensions();
            Some((decoded.into_raw(), width, height))
        });
        let Some((bytes, width, height)) = fetched else {
            crate::debug_log("saved avatar fetch failed");
            return;
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
            buffer.make_mut_bytes().copy_from_slice(&bytes);
            ui.set_saved_account_avatar(Image::from_rgba8(buffer));
        });
    }));
}

/// Push a preferences snapshot into the settings panel's bound properties.
pub(crate) fn publish_settings(ui: &AppWindow, snapshot: &settings::Settings) {
    ui.set_set_quality(snapshot.player_quality as i32);
    ui.set_set_speed_milli(snapshot.player_speed as i32);
    ui.set_set_hwaccel(snapshot.hwaccel);
    ui.set_set_codec(snapshot.codec_pref as i32);
    ui.set_set_dm_opacity(snapshot.danmaku_opacity as i32);
    ui.set_set_dm_font(snapshot.danmaku_font as i32);
    ui.set_set_dm_speed(snapshot.danmaku_speed as i32);
    ui.set_set_dm_area(snapshot.danmaku_area as i32);
    ui.set_set_block_scroll(snapshot.block_scroll);
    ui.set_set_block_top(snapshot.block_top);
    ui.set_set_block_bottom(snapshot.block_bottom);
    ui.set_set_ai_on(snapshot.ai_on);
    ui.set_set_ai_level(snapshot.ai_level as i32);
    ui.set_set_theme(snapshot.theme as i32);
    // The transparency slider lives on the theme global (it drives the window
    // tint's alpha), not on a settings-panel property.
    ui.global::<AppTheme>()
        .set_glass_transparency(snapshot.glass_transparency);
    ui.global::<AppTheme>()
        .set_backdrop_blur(snapshot.backdrop_blur as i32);
    // The cards-per-page setting is Rust-driven: the UI only reports menu
    // changes (`page_size_changed`), so every publish pushes the stored value
    // into the paging property.
    ui.set_list_page_size(snapshot.page_size as i32);
    ui.set_set_autostart(snapshot.autostart);
    // 优化: 下载连接数, the prefetch proxy's parallel fetcher count.
    ui.set_set_prefetch_connections(snapshot.prefetch_connections as i32);
    // 关闭主窗口的行为 (0 = 最小化到任务栏, 1 = 隐藏到系统托盘, 2 = 直接退出),
    // so the panel opens on the stored value.
    ui.set_set_close_action(snapshot.close_action as i32);
    // 防干扰: the panel's two switches and the live state the toolbar / search
    // panel render from. Publishing both keeps a restart, a toggle and a hand
    // edited settings.json on the same path.
    ui.set_set_hide_live(snapshot.hide_live_tab);
    ui.set_set_hide_hot_search(snapshot.hide_hot_search);
    ui.set_hide_live_tab(snapshot.hide_live_tab);
    ui.set_hide_hot_search(snapshot.hide_hot_search);
    ui.set_set_key_map(key_map_model(&snapshot.player_keys));
    let rows: Vec<SharedString> = snapshot
        .blocked_senders
        .iter()
        .map(|hash| SharedString::from(hash.clone()))
        .collect();
    ui.set_set_blocked_senders(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    let keywords: Vec<SharedString> = snapshot
        .blocked_keywords
        .iter()
        .map(|word| SharedString::from(word.clone()))
        .collect();
    ui.set_set_blocked_keywords(ModelRc::from(std::rc::Rc::new(VecModel::from(keywords))));
    let regexes: Vec<SharedString> = snapshot
        .blocked_regex
        .iter()
        .map(|pattern| SharedString::from(pattern.clone()))
        .collect();
    ui.set_set_blocked_regex(ModelRc::from(std::rc::Rc::new(VecModel::from(regexes))));
}

/// Mutate the stored preferences, persist them off-thread, and return a snapshot.
pub(crate) fn update_settings<F>(store: &Arc<Mutex<settings::Settings>>, mutate: F) -> settings::Settings
where
    F: FnOnce(&mut settings::Settings),
{
    let snapshot = match store.lock() {
        Ok(mut guard) => {
            mutate(&mut guard);
            guard.clone()
        }
        Err(_) => return settings::Settings::default(),
    };
    let to_save = snapshot.clone();
    std::thread::spawn(move || {
        let _ = settings::save(&to_save);
    });
    snapshot
}

/// Apply a preferences snapshot to every live player window.
pub(crate) fn apply_settings_to_player(state: &Rc<PlayerState>, snapshot: &settings::Settings) {
    let sessions = player_sessions(state);
    let mut codec_changed = None;
    for session in &sessions {
        let control = &session.control;
        control.set_danmaku_font(snapshot.danmaku_font);
        control.set_danmaku_speed(snapshot.danmaku_speed);
        control.set_danmaku_area(snapshot.danmaku_area);
        control.set_type_blocks(
            snapshot.block_scroll,
            snapshot.block_top,
            snapshot.block_bottom,
        );
        control.set_blocked_senders(snapshot.blocked_senders.clone());
        control.set_block_rules(&snapshot.blocked_keywords, &snapshot.blocked_regex);
        control.set_hwaccel(snapshot.hwaccel);
        control.set_native_frame_size(snapshot.player_native_size);
        let new_codec = snapshot.codec_pref as i64;
        if control.codec_pref() != new_codec {
            codec_changed = Some(new_codec);
        }
        control.set_codec_pref(new_codec);
        control.set_speed(snapshot.player_speed as f64 / 1000.0);
    }
    if let Some(codec) = codec_changed {
        if let Some(session) = focused_session(state) {
            let label = if codec == 0 {
                "自动".to_string()
            } else {
                bilibili::codec_label(codec).to_string()
            };
            toast(
                &session.window,
                format!("编码偏好：{label}"),
                TOAST_KIND_INFO,
            );
        }
    }
    for session in &sessions {
        let window = &session.window;
        window.set_danmaku_opacity(snapshot.danmaku_opacity as f32 / 100.0);
        window.set_danmaku_opacity_text(snapshot.danmaku_opacity.to_string().into());
        window.set_danmaku_font(snapshot.danmaku_font as i32);
        window.set_danmaku_speed(snapshot.danmaku_speed as i32);
        window.set_danmaku_area(snapshot.danmaku_area as i32);
        window.set_block_scroll(snapshot.block_scroll);
        window.set_block_top(snapshot.block_top);
        window.set_block_bottom(snapshot.block_bottom);
        window.set_key_map(key_map_model(&snapshot.player_keys));
    }
}
