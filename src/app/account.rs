//! Login, identity, 动态/私信, UP 主空间, 收藏夹 and the account pages.

use crate::*;

/// UI thread: the client page of `view` (0-based).
pub(crate) fn client_page(view: i32) -> i64 {
    CLIENT_PAGES.with(|cell| cell.borrow().get(view))
}

/// UI thread: set the client page of `view` (clamped at 0).
pub(crate) fn set_client_page(view: i32, page: i64) {
    CLIENT_PAGES.with(|cell| cell.borrow_mut().set(view, page));
}

/// Client page size of the section on screen: the UI's `list_page_size`, which
/// Rust drives from the stored cards-per-page setting. An early read before the
/// first publish (or a stray non-positive value) is clamped to one card, so the
/// `page × size` arithmetic below can never divide by zero.
pub(crate) fn list_page_size(ui: &AppWindow) -> i64 {
    i64::from(ui.get_list_page_size()).max(1)
}

/// UI thread: the page size to fetch with, read before a worker thread starts
/// (the property is UI-thread only and follows the 设置 menu, not a resize).
pub(crate) fn current_page_size(weak: &Weak<AppWindow>) -> i64 {
    weak.upgrade().map(|ui| list_page_size(&ui)).unwrap_or(1)
}

/// Number of client pages a list of `len` rows spans at `size` per page (at
/// least one, so an empty list still reads "1 / 1").
pub(crate) fn page_count(size: i64, len: usize) -> i64 {
    let size = size.max(1);
    if len == 0 {
        1
    } else {
        ((len as i64) + size - 1) / size
    }
}

/// Absolute first-card index of client `page` over `len` loaded rows: `page ×
/// size`, pulled back so the last page of a fully loaded list is full when
/// possible. While the section can still fetch (`more`) a partial last page
/// stays where it is, because the next fetch extends it.
pub(crate) fn client_offset(page: i64, size: i64, len: usize, more: bool) -> i64 {
    let size = size.max(1);
    let raw = page.max(0).saturating_mul(size);
    let len = len as i64;
    if len <= 0 {
        return 0;
    }
    if raw + size <= len {
        return raw;
    }
    if more {
        return raw.min(len - 1);
    }
    (len - size).max(0)
}

/// Whether a further client page exists over `len` loaded rows. `more` says the
/// section can still fetch data, so the next page always exists.
pub(crate) fn has_client_next(page: i64, size: i64, len: usize, more: bool) -> bool {
    more || page.max(0) + 1 < page_count(size, len)
}

/// UI thread: mirror the client page into the counter property the page bar
/// reads for `view` (the sections whose count no other property carries).
pub(crate) fn publish_page_counter(ui: &AppWindow, view: i32, page: i64) {
    let shown = (page + 1).clamp(1, i64::from(i32::MAX)) as i32;
    match view {
        0 => ui.set_feed_page(shown),
        5 => ui.set_history_page(shown),
        13 => ui.set_dynamic_page(shown),
        _ => {}
    }
}

/// UI thread: write `list_offset` for the section on screen and log it, so the
/// `offset = page × size` relation is visible in `BILI_DEBUG_LOG`. `len` is the
/// rows loaded in that section's model, `more` whether it can still fetch.
pub(crate) fn apply_client_offset(ui: &AppWindow, view: i32, len: usize, more: bool) {
    let size = list_page_size(ui);
    let pages = page_count(size, len);
    let mut page = client_page(view);
    if page >= pages {
        // The data no longer covers the page (a shorter reload); keep the
        // offset on the last page instead of pointing past the end.
        page = (pages - 1).max(0);
        set_client_page(view, page);
    }
    let offset = client_offset(page, size, len, more);
    ui.set_list_offset(offset as i32);
    publish_page_counter(ui, view, page);
    // The data page is the API page for the sections fetched with
    // `ps = list_page_size`; their client page stays 0, so log both.
    let data_page = match view {
        1 => ui.get_search_current_page(),
        4 => ui.get_folder_current_page(),
        8 => ui.get_region_current_page(),
        11 => ui.get_space_current_page(),
        _ => 0,
    };
    crate::debug_log(&format!(
        "page bar view={view} page={page} offset={offset} size={size} items={len} more={} data_page={data_page}",
        u8::from(more)
    ));
}

/// UI thread: re-publish the offset of the section on screen and refresh the
/// pill. `list_offset` is one window property shared by every grid, so this runs
/// after each load, on every view switch and on every client page move (it also
/// picks up a page size that moved with a resize).
pub(crate) fn publish_list_offset(ui: &AppWindow) {
    match ui.get_view_mode() {
        0 => apply_client_offset(ui, 0, ui.get_feed_items().row_count(), true),
        1 => apply_client_offset(ui, 1, ui.get_search_items().row_count(), true),
        4 => apply_client_offset(ui, 4, ui.get_folder_items().row_count(), true),
        5 => apply_client_offset(
            ui,
            5,
            ui.get_history_items().row_count(),
            ui.get_history_has_more(),
        ),
        6 => apply_client_offset(ui, 6, ui.get_toview_items().row_count(), false),
        8 => apply_client_offset(ui, 8, ui.get_region_items().row_count(), true),
        11 => apply_client_offset(ui, 11, ui.get_space_items().row_count(), true),
        13 => apply_client_offset(
            ui,
            13,
            ui.get_dynamic_items().row_count(),
            ui.get_dynamic_has_more(),
        ),
        _ => {}
    }
    // A client page move changes the offset without a load, so the pill is
    // refreshed here as well: the counter and the arrows follow the page.
    update_page_bar(ui);
}

/// UI thread: the section on screen was just entered by a navigation switch, so
/// it starts at its first client page (a refresh keeps the page instead).
pub(crate) fn enter_paged_view(ui: &AppWindow) {
    set_client_page(ui.get_view_mode(), 0);
    publish_list_offset(ui);
}

/// Page bar state for a client-paged section (历史 / 动态): the counter is the
/// client page over the loaded pages and prev walks back through them.
pub(crate) fn client_page_bar(
    ui: &AppWindow,
    view: i32,
    len: usize,
    more: bool,
    busy: bool,
) -> (String, bool, bool, bool, bool, bool) {
    let size = list_page_size(ui);
    let page = client_page(view);
    (
        (page + 1).to_string(),
        page > 0,
        page > 0,
        has_client_next(page, size, len, more),
        busy,
        true,
    )
}

/// Page bar state for a single-shot list (稍后再看 / 排行榜): the
/// client pages span the loaded rows and there is nothing left to fetch.
pub(crate) fn single_shot_bar(
    ui: &AppWindow,
    view: i32,
    len: usize,
    busy: bool,
) -> (String, bool, bool, bool, bool, bool) {
    let size = list_page_size(ui);
    let page = client_page(view);
    let pages = page_count(size, len);
    (
        format!("{} / {}", (page + 1).min(pages), pages),
        page > 0,
        page > 0,
        page + 1 < pages,
        busy,
        true,
    )
}

/// Push the floating page bar's state from the section that is on screen.
/// Every loader and navigation handler calls this after it changes its
/// section's state, so the pill always mirrors what the view shows. The counter
/// is the client page of the section: the sections fetched with
/// `ps = list_page_size` keep exactly one API page per screen, so their counter
/// is that API page; the client-paged ones read the shared client page.
pub(crate) fn update_page_bar(ui: &AppWindow) {
    let mode = ui.get_view_mode();
    let (text, has_first, has_prev, has_next, busy, can_refresh) = match mode {
        // 首页推荐: a rolling client page over the loaded batch; another batch
        // can always be fetched, so next is always live.
        0 => {
            let page = client_page(0) + 1;
            (
                page.to_string(),
                page > 1,
                page > 1,
                true,
                ui.get_list_refreshing(),
                true,
            )
        }
        // 搜索: `N / M`, M = ceil(num_results / page size) from the API count.
        1 => {
            let page = ui.get_search_current_page().max(1);
            let total = ui.get_search_total_pages().max(1);
            (
                format!("{page} / {total}"),
                page > 1,
                page > 1,
                page < total,
                ui.get_list_refreshing(),
                true,
            )
        }
        // 收藏夹内容 / 分区: a rolling API page, one page per screen.
        4 => {
            let page = ui.get_folder_current_page().max(1);
            (
                page.to_string(),
                page > 1,
                page > 1,
                ui.get_folder_has_next(),
                ui.get_list_refreshing(),
                true,
            )
        }
        8 => {
            let page = ui.get_region_current_page().max(1);
            (
                page.to_string(),
                page > 1,
                page > 1,
                ui.get_region_has_next(),
                ui.get_list_refreshing(),
                true,
            )
        }
        // 历史 / 动态: append-only cursors; the counter is the client page over
        // the loaded pages and prev walks back through them.
        5 => client_page_bar(
            ui,
            5,
            ui.get_history_items().row_count(),
            ui.get_history_has_more(),
            ui.get_list_refreshing(),
        ),
        13 => client_page_bar(
            ui,
            13,
            ui.get_dynamic_items().row_count(),
            ui.get_dynamic_has_more(),
            ui.get_list_refreshing(),
        ),
        // Single-shot lists: `N / M` over the loaded rows, nothing to fetch.
        6 => single_shot_bar(ui, 6, ui.get_toview_items().row_count(), ui.get_list_refreshing()),
        // 消息: its grids render every row in their own ScrollView (no
        // `page-offset`), so the pill stays refresh-only here.
        14 => (
            "1 / 1".to_string(),
            false,
            false,
            false,
            ui.get_msg_loading(),
            true,
        ),
        // Non-list views: the bar is hidden, leave it inert.
        _ => (String::new(), false, false, false, false, false),
    };
    ui.set_page_bar_text(text.into());
    ui.set_page_bar_has_first(has_first);
    ui.set_page_bar_has_prev(has_prev);
    ui.set_page_bar_has_next(has_next);
    ui.set_page_bar_busy(busy);
    ui.set_page_bar_can_refresh(can_refresh);
}

/// Publish the refresh state the 刷新 button renders: `refreshing` gates the
/// fill's opacity and `progress` is its 0..1 width. The feed publishes pages
/// done over pages total; every other section publishes 0.0 at the start and
/// 1.0 when the load settles, so its fill is a small completion pulse rather
/// than an indeterminate sweep. There is no second progress line anywhere -
/// the bar's 刷新 pill is the only surface. Replaces the old per-section
/// "正在加载…" text lines, so nothing writes those any more.
pub(crate) fn publish_refresh(ui: &AppWindow, refreshing: bool, progress: f32) {
    ui.set_list_refreshing(refreshing);
    ui.set_list_refresh_progress(if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    });
}

/// Supersede any in-flight attempt and start a login on a freshly bumped
/// generation. `saved` selects the saved-account flow, otherwise QR login.
pub(crate) fn start_login(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    saved: bool,
) {
    let my = generation.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(ui) = weak.upgrade() {
        ui.set_logged_in(false);
        ui.set_qr_image(Image::default());
        ui.set_login_state(1);
        ui.set_login_step("准备中…".into());
        ui.set_login_percent(0);
        ui.set_login_progress(0.0);
        ui.set_login_failed(false);
        ui.set_login_error("".into());
        ui.set_login_countdown("".into());
    }
    spawn_attempt(
        weak,
        generation,
        session_slot,
        feed_store,
        feed_ring,
        settings_store,
        my,
        saved,
    );
}

/// Spawn a worker thread owning its own current-thread tokio runtime.
pub(crate) fn spawn_attempt(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    my: u64,
    auto: bool,
) {
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            let message = "初始化网络运行时失败".to_string();
            post(&weak, &generation, my, move |ui| ui.set_status(message.into()));
            return;
        };

        runtime.block_on(async move {
            if let Err(err) = run_attempt(
                weak.clone(),
                generation.clone(),
                session_slot,
                feed_store,
                feed_ring,
                settings_store,
                my,
                auto,
            )
            .await
            {
                let message = format!("发生错误：{err}");
                post(&weak, &generation, my, move |ui| ui.set_status(message.into()));
            }
        });
    }));
}

pub(crate) async fn run_attempt(
    weak: Weak<AppWindow>,
    generation: Arc<AtomicU64>,
    session_slot: SessionSlot,
    feed_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_ring: Arc<Mutex<FeedPrefetchRing>>,
    settings_store: Arc<Mutex<settings::Settings>>,
    my: u64,
    auto: bool,
) -> Result<()> {
    if auto {
        post_step(&weak, &generation, my, "read-account", "读取本地账号", 10);
        let account = match accounts::load() {
            Some(account) => Some(account),
            None => accounts::migrate_from_token().await,
        };
        let Some(account) = account else {
            post_login_choice(&weak, &generation, my, "没有已保存的账号，请扫码登录");
            return Ok(());
        };
        post_step(
            &weak,
            &generation,
            my,
            "fingerprint",
            "获取设备指纹（buvid3/buvid4/b_nut/bili_ticket）",
            25,
        );
        let session = match bilibili::Session::from_token(&accounts::to_token(&account)).await {
            Ok(session) => Arc::new(session),
            Err(err) => {
                post_login_choice(
                    &weak,
                    &generation,
                    my,
                    &format!("无法恢复已保存的登录（{err}），请重新登录"),
                );
                return Ok(());
            }
        };
        post_step(&weak, &generation, my, "nav", "校验登录状态（nav）", 45);
        match session.nav().await {
            Ok(bilibili::NavOutcome::LoggedIn(user)) => {
                post_step(&weak, &generation, my, "account-info", "获取账号信息", 65);
                publish_identity(&weak, &generation, my, &user, "restore");
                publish_session(&session_slot, session.clone());
                show_user(&weak, &generation, my, &session, user).await;
                post_step(&weak, &generation, my, "feed", "加载首页推荐", 90);
                show_home_after_login(
                    &weak,
                    &generation,
                    my,
                    &session_slot,
                    &feed_store,
                    &feed_ring,
                    session.clone(),
                    feed_prefetch_capacity(&settings_store),
                )
                .await;
                post_step(&weak, &generation, my, "done", "完成", 100);
                start_tab_warm(
                    generation.clone(),
                    my,
                    session_slot.clone(),
                    settings_store.clone(),
                );
                return Ok(());
            }
            Ok(bilibili::NavOutcome::NotLoggedIn { code, message }) => {
                post_login_choice(
                    &weak,
                    &generation,
                    my,
                    &format!("已保存账号登录失效（code={code} {message}），请重新登录"),
                );
            }
            Err(err) => {
                post_login_choice(
                    &weak,
                    &generation,
                    my,
                    &format!("校验登录状态失败（{err}），请重新登录"),
                );
            }
        }
        return Ok(());
    }

    // ---- QR login --------------------------------------------------------
    post_step(
        &weak,
        &generation,
        my,
        "fingerprint",
        "获取设备指纹（buvid3/buvid4/b_nut/bili_ticket）",
        10,
    );
    let session = Arc::new(bilibili::Session::new().await?);
    post_step(&weak, &generation, my, "qr-gen", "请求二维码", 25);

    let qr = session.qr_generate().await?;
    let (pixels, width, height) = build_qr_rgba(&qr.url)?;
    let qr_side = width as f32;

    post(&weak, &generation, my, move |ui| {
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
        buffer.make_mut_bytes().copy_from_slice(&pixels);
        ui.set_qr_image(Image::from_rgba8(buffer));
        ui.set_qr_size(qr_side);
        ui.set_logged_in(false);
    });
    // Waiting-for-scan: QR centered + progress 25% -> 70% + ~180s countdown.
    post_login_qr(
        &weak,
        &generation,
        my,
        "等待扫码…",
        25,
        &format!(
            "二维码有效期约 {} 秒",
            MAX_POLLS as u64 * POLL_INTERVAL.as_secs()
        ),
    );

    let mut last_state: i64 = -1;
    let mut success: Option<bilibili::PollResult> = None;

    for poll_index in 0..MAX_POLLS {
        if generation.load(Ordering::SeqCst) != my {
            return Ok(()); // superseded
        }

        let poll = match session.qr_poll(&qr.key).await {
            Ok(p) => p,
            // Transient network hiccup: keep polling.
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        match poll.state_code {
            0 => {
                success = Some(poll);
                break;
            }
            86038 => {
                post_login_fail(
                    &weak,
                    &generation,
                    my,
                    "waiting",
                    "等待扫码",
                    70,
                    "二维码已过期（code=86038），请点击「刷新二维码」",
                );
                return Ok(());
            }
            86090 => {
                if last_state != 86090 {
                    last_state = 86090;
                    post_step(&weak, &generation, my, "scanned", "已扫码，等待手机确认", 75);
                }
            }
            _ => {
                last_state = poll.state_code;
                let frac = (poll_index + 1) as f32 / MAX_POLLS as f32;
                let percent = 25 + (frac * 45.0) as i32;
                let remaining = (MAX_POLLS - poll_index) as u64 * POLL_INTERVAL.as_secs();
                post_login_qr(
                    &weak,
                    &generation,
                    my,
                    "等待扫码…",
                    percent,
                    &format!("二维码有效期剩余约 {remaining} 秒"),
                );
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let poll = match success {
        Some(p) => p,
        None => {
            post_login_fail(
                &weak,
                &generation,
                my,
                "waiting",
                "等待扫码",
                70,
                "二维码超时，请点击「刷新二维码」",
            );
            return Ok(());
        }
    };

    post_step(&weak, &generation, my, "confirmed", "手机已确认，正在校验", 80);

    // ---- SSO cross-domain jump (best-effort; may set extra cookies) ------
    if !poll.cross_url.is_empty() {
        let _ = session.visit(&poll.cross_url).await;
    }

    // ---- account info (nav required, the rest best-effort) ---------------
    let user = match session.nav().await? {
        bilibili::NavOutcome::LoggedIn(user) => user,
        bilibili::NavOutcome::NotLoggedIn { code, message } => {
            post_login_fail(
                &weak,
                &generation,
                my,
                "nav",
                "获取账号信息（nav）",
                95,
                &format!("登录校验失败：code={code} {message}"),
            );
            return Ok(());
        }
    };

    // ---- build the account and persist it --------------------------------
    post_step(&weak, &generation, my, "save", "保存登录凭证", 88);
    publish_identity(&weak, &generation, my, &user, "login");
    let account = accounts::Account {
        mid: user.mid,
        uname: user.uname.clone(),
        face: user.face.clone(),
        cookies: poll.cookies,
        refresh_token: poll.refresh_token,
        added_at: accounts::now_secs(),
        vip: user.vip_status,
    };
    if let Err(err) = accounts::save(&account) {
        post_status(
            &weak,
            &generation,
            my,
            &format!("提示：登录成功但保存账号失败：{err}"),
        );
    }
    publish_session(&session_slot, session.clone());

    post_step(&weak, &generation, my, "nav", "获取账号信息（nav）", 95);
    show_user(&weak, &generation, my, &session, user).await;
    post_step(&weak, &generation, my, "feed", "加载首页推荐", 100);
    show_home_after_login(
        &weak,
        &generation,
        my,
        &session_slot,
        &feed_store,
        &feed_ring,
        session.clone(),
        feed_prefetch_capacity(&settings_store),
    )
    .await;
    post_step(&weak, &generation, my, "done", "完成", 100);
    start_tab_warm(
        generation.clone(),
        my,
        session_slot.clone(),
        settings_store.clone(),
    );
    Ok(())
}

/// Load the current 大会员 state (`privilege/my`) without blocking the UI thread.
pub(crate) fn vip_refresh(weak: Weak<AppWindow>, session_slot: SessionSlot) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_vip_busy(false);
            ui.set_vip_status_text("未登录".into());
        }
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_vip_busy(true);
        ui.set_vip_status_text("加载中…".into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let result = runtime.block_on(session.vip_privilege_my());
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_vip_busy(false);
                match result {
                    Ok(vip) => {
                        let text = if vip.message.is_empty() || vip.message == "0" {
                            format!("code={}", vip.code)
                        } else {
                            format!("code={} {}", vip.code, vip.message)
                        };
                        ui.set_vip_status_text(text.into());
                        ui.set_vip_privilege_text(vip.summary.into());
                    }
                    Err(err) => ui.set_vip_status_text(format!("获取失败：{err}").into()),
                }
            }
        });
    }));
}

/// Run one 会员中心 write action and surface the returned API code.
pub(crate) fn vip_action(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    action: VipAction,
) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_vip_action_status("未登录".into());
        }
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_vip_busy(true);
        ui.set_vip_action_status("请求中…".into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        let result = runtime.block_on(async move {
            match action {
                VipAction::Experience => session.vip_experience_add().await,
                VipAction::Receive => session.vip_privilege_receive().await,
                VipAction::Sign => session.vip_task_sign().await,
            }
        });
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_vip_busy(false);
                match result {
                    Ok(vip) => {
                        let text = if vip.message.is_empty() {
                            format!("code={}", vip.code)
                        } else {
                            format!("code={} {}", vip.code, vip.message)
                        };
                        ui.set_vip_action_status(text.into());
                    }
                    Err(err) => ui.set_vip_action_status(format!("失败：{err}").into()),
                }
            }
        });
    }));
}

/// Clamp an exact count / Unix timestamp to the `int` a Slint row carries.
pub(crate) fn saturating_i32(value: i64) -> i32 {
    value.clamp(0, i64::from(i32::MAX)) as i32
}

pub(crate) fn open_up_mid(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    mid: i64,
) {
    if mid <= 0 {
        crate::debug_log(&format!("up open skipped mid={mid} reason=no-mid"));
        return;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_up_page_open(true);
        ui.set_up_name(String::new().into());
        ui.set_up_fans(String::new().into());
        ui.set_up_videos_text(String::new().into());
        ui.set_up_level(String::new().into());
        ui.set_up_sign(String::new().into());
        ui.set_up_avatar(Image::default());
        ui.set_up_avatar_text(String::new().into());
        ui.set_up_loading(true);
        ui.set_up_status(String::new().into());
        ui.set_up_follow_state(0);
        ui.set_up_follow_text("关注".into());
        ui.set_up_follow_busy(false);
        ui.set_up_sort(0);
        ui.set_up_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
            Vec::<FeedRow>::new(),
        ))));
    }
    if let Ok(mut store) = up_store.lock() {
        store.clear();
    }
    if let Ok(mut state) = up_page.lock() {
        *state = UpPageState {
            mid,
            ..UpPageState::default()
        };
    }
    load_up_page(
        weak,
        session_slot,
        up_page,
        up_seq,
        up_store,
        mid,
        0,
        true,
    );
}

/// Close the UP page and forget its page list, so a later index can never
/// resolve to a video of the closed page.
pub(crate) fn close_up_page(
    ui: &AppWindow,
    up_page: &Arc<Mutex<UpPageState>>,
    up_seq: &Arc<AtomicU64>,
    up_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
) {
    // Bump first: any load still in flight is superseded before its publish.
    up_seq.fetch_add(1, Ordering::SeqCst);
    ui.set_up_page_open(false);
    ui.set_up_sort(0);
    ui.set_up_name(String::new().into());
    ui.set_up_fans(String::new().into());
    ui.set_up_videos_text(String::new().into());
    ui.set_up_level(String::new().into());
    ui.set_up_sign(String::new().into());
    ui.set_up_avatar(Image::default());
    ui.set_up_avatar_text(String::new().into());
    ui.set_up_loading(true);
    ui.set_up_status(String::new().into());
    ui.set_up_follow_state(0);
    ui.set_up_follow_text("关注".into());
    ui.set_up_follow_busy(false);
    ui.set_up_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<FeedRow>::new(),
    ))));
    if let Ok(mut store) = up_store.lock() {
        store.clear();
    }
    if let Ok(mut state) = up_page.lock() {
        *state = UpPageState::default();
    }
    crate::debug_log("up close");
}

/// Follow / unfollow the UP主 whose page is open, through the same
/// `x/relation/modify` path the space page uses. The button is pill-shaped in
/// both states; only its fill and label change.
pub(crate) fn toggle_up_follow(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    up_page: Arc<Mutex<UpPageState>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (mid, current) = match up_page.lock() {
        Ok(state) => (state.mid, state.attribute),
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        if ui.get_up_follow_busy() {
            return;
        }
        ui.set_up_follow_busy(true);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let act = if current == 2 || current == 6 { 2 } else { 1 };
            let result = session.relation_modify(mid, act).await;
            let (attribute, status) = match result {
                Ok(()) => {
                    let attribute = session
                        .relation_state(mid)
                        .await
                        .unwrap_or(if act == 1 { 2 } else { 0 });
                    (attribute, String::new())
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "up follow mid={mid} act={act} err={err:#}"
                    ));
                    (current, format!("关注操作失败：{err}"))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut state) = up_page.lock() {
                        state.attribute = attribute;
                    }
                    ui.set_up_follow_state(attribute as i32);
                    ui.set_up_follow_text(follow_text(attribute).into());
                    ui.set_up_follow_busy(false);
                    ui.set_up_status(status.into());
                    crate::debug_log(&format!(
                        "up follow mid={mid} attribute={attribute}"
                    ));
                }
            });
        });
    }));
}

/// Fetch one UP page (`sort` 0 = 播放最多, 1 = 最新发布) and publish it.
///
/// `announce` is the open path's `up open …` line; the per-load `up sort=… n=…`
/// line is always logged, so a sort switch is visible in the debug log too. The
/// 5 minute caches in `bilibili.rs` make a sort switch between the two orders
/// cheap, and a repeated switch a cache hit.
pub(crate) fn load_up_page(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    up_page: Arc<Mutex<UpPageState>>,
    up_seq: Arc<AtomicU64>,
    up_store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    mid: i64,
    sort: i32,
    announce: bool,
) {
    let sort = if sort == 1 { 1 } else { 0 };
    let Some(session) = active_session(&session_slot) else {
        crate::debug_log(&format!("up open skipped mid={mid} reason=no-session"));
        return;
    };
    let my = up_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut state) = up_page.lock() {
        state.mid = mid;
        state.published = None;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_up_page_open(true);
        ui.set_up_sort(sort);
        if announce {
            ui.set_up_loading(true);
            ui.set_up_status(String::new().into());
            ui.set_up_level(String::new().into());
            ui.set_up_sign(String::new().into());
            ui.set_up_avatar(Image::default());
            ui.set_up_follow_state(0);
            ui.set_up_follow_text("关注".into());
            ui.set_up_follow_busy(false);
        }
    }
    let order = if sort == 1 {
        bilibili::UpOrder::Newest
    } else {
        bilibili::UpOrder::Plays
    };
    // The header only changes when another UP主 is opened: a sort switch reuses
    // the card already in `up_page`, so it never spends a request on it.
    let need_card = announce
        || up_page
            .lock()
            .map(|state| state.mid != mid || state.name.is_empty())
            .unwrap_or(true);
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            // Header first: the card's own 5 minute cache makes a re-open free.
            let card = if need_card {
                match session.user_card(mid).await {
                    Ok(card) => Some(card),
                    Err(err) => {
                        crate::debug_log(&format!("up mid={mid} card err={err:#}"));
                        None
                    }
                }
            } else {
                None
            };
            // The space info carries the level and the signature; the card
            // above carries the avatar and the counts. Both only on a real
            // open (a sort switch reuses the header already on screen).
            let profile = if need_card {
                session.space_info(mid).await.ok()
            } else {
                None
            };
            let attribute = if need_card {
                session.relation_state(mid).await.unwrap_or(-1)
            } else {
                -1
            };
            let avatar_url = card
                .as_ref()
                .map(|(_, avatar, _, _)| avatar.clone())
                .unwrap_or_default();
            let avatar_image = if need_card && !avatar_url.is_empty() {
                session.fetch_avatar(&avatar_url).await.ok()
            } else {
                None
            };
            let (items, has_more) = match session.up_videos(mid, order, 1).await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!(
                        "up mid={mid} order={} err={err:#}",
                        order.param()
                    ));
                    let message = format!("投稿加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_up_loading(false);
                            ui.set_up_status(message.into());
                        }
                    });
                    return;
                }
            };
            let name = card
                .as_ref()
                .map(|(name, _, _, _)| name.clone())
                .unwrap_or_default();
            if announce {
                let (avatar, fans, videos) = card
                    .as_ref()
                    .map(|(_, avatar, fans, videos)| (u8::from(!avatar.is_empty()), *fans, *videos))
                    .unwrap_or((0, 0, 0));
                crate::debug_log(&format!(
                    "up open mid={mid} name={name:?} items={} fans={fans} videos={videos} sort={sort} avatar={avatar}",
                    items.len()
                ));
            }
            crate::debug_log(&format!(
                "up sort={sort} n={} mid={mid} order={} more={}",
                items.len(),
                order.param(),
                u8::from(has_more)
            ));

            let row_count = items.len();
            let page = items.clone();
            let rows_weak = weak.clone();
            let rows_seq = up_seq.clone();
            let rows_store = up_store.clone();
            let rows_state = up_page.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if rows_seq.load(Ordering::SeqCst) != my {
                    return;
                }
                let Some(ui) = rows_weak.upgrade() else {
                    return;
                };
                if let Ok(mut guard) = rows_store.lock() {
                    *guard = page.clone();
                }
                let rows: Vec<FeedRow> = page.iter().map(build_feed_row_text).collect();
                if let Some(row) = rows.first() {
                    crate::debug_log(&format!(
                        "up row index=0 bvid={} pub_ts={} view_count={} owner_mid={}",
                        page.first().map(|video| video.bvid.as_str()).unwrap_or("-"),
                        row.pub_ts,
                        row.view_count,
                        row.owner_mid
                    ));
                }
                // Fold the freshly fetched header into the page state, then
                // publish the header from that state (a sort switch has none of
                // its own and republishes what is already there).
                if let Some((name, avatar, fans, videos)) = card {
                    if let Ok(mut state) = rows_state.lock() {
                        state.mid = mid;
                        state.name = name;
                        state.avatar = avatar;
                        state.fans = fans;
                        state.videos = videos;
                    }
                }
                if let Some(profile) = profile {
                    if let Ok(mut state) = rows_state.lock() {
                        state.level = if profile.level > 0 {
                            format!("Lv{}", profile.level)
                        } else {
                            String::new()
                        };
                        state.sign = if profile.sign.trim().is_empty() {
                            "（暂无签名）".to_string()
                        } else {
                            profile.sign.trim().to_string()
                        };
                    }
                }
                if attribute >= 0 {
                    if let Ok(mut state) = rows_state.lock() {
                        state.attribute = attribute;
                    }
                }
                let header = rows_state
                    .lock()
                    .map(|state| {
                        (
                            state.name.clone(),
                            state.fans,
                            state.videos,
                            u8::from(!state.avatar.is_empty()),
                            state.level.clone(),
                            state.sign.clone(),
                            state.attribute,
                        )
                    })
                    .unwrap_or_default();
                ui.set_up_name(header.0.into());
                ui.set_up_fans(format_view(header.1).into());
                ui.set_up_videos_text(format_view(header.2).into());
                ui.set_up_level(header.4.clone().into());
                ui.set_up_sign(header.5.clone().into());
                ui.set_up_follow_state(header.6 as i32);
                ui.set_up_follow_text(follow_text(header.6).into());
                if let Some((bytes, w, h)) = avatar_image {
                    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
                    buffer.make_mut_bytes().copy_from_slice(&bytes);
                    ui.set_up_avatar(Image::from_rgba8(buffer));
                }
                ui.set_up_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
                ui.set_up_page_open(true);
                ui.set_up_loading(false);
                ui.set_up_status(String::new().into());
                ui.set_up_sort(sort);
                if let Ok(mut state) = rows_state.lock() {
                    state.published = Some(sort);
                }
                crate::debug_log(&format!(
                    "up page rendered mid={mid} sort={sort} items={row_count} avatar={} level={:?} sign={}",
                    header.3,
                    header.4,
                    u8::from(!header.5.is_empty())
                ));
            });

            // Covers stream in after the rows are live, through the same cache
            // and guard the feed uses; a closed or re-ordered page drops them.
            let covers = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let pics: Vec<String> = items.iter().take(covers).map(|item| item.pic.clone()).collect();
            let keys: Vec<String> = items
                .iter()
                .take(covers)
                .map(|item| item.bvid.clone())
                .collect();
            let guard_seq = up_seq.clone();
            let guard_store = up_store.clone();
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| {
                    ThumbGuard::GenFeed(guard_seq.clone(), my, guard_store.clone(), key)
                },
                |ui| ui.get_up_items(),
                |row, image| row.thumb = image,
                format!("up mid={mid} sort={sort}"),
            )
            .await;
        });
    }));
}

/// The UP主 behind the card at `index` on the view that is on screen.
///
/// The view is read the same way `card_open` reads it, so a click on a card's UP
/// name can never open another view's row:
///   0  feed (`feed_store` carries the owner directly) — or, on the 收藏推荐 home
///      tab, the masonry's own list (`masonry`, `fav_videos`)
///   1  search, 视频 or 用户 tab (`SearchState::mids`, aligned with its rows)
///   4  one favourite folder (`FolderState::mids`)
///   5  history (`HistoryPageState::mids`, append-extended)
/// While the UP overlay is open, its own page list answers instead.
/// Views whose rows carry no owner mid (稍后再看, the discovery grids, the old
/// space view) answer `None` rather than guess.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_card_mid(
    view: i32,
    index: i32,
    up_open: bool,
    masonry: bool,
    up_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    feed_store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    fav_videos: &Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    search_state: &Arc<Mutex<SearchState>>,
    folder_state: &Arc<Mutex<FolderState>>,
    history_page: &Arc<Mutex<HistoryPageState>>,
) -> Option<i64> {
    let index = usize::try_from(index).ok()?;
    let mid_of = |mids: &[i64]| mids.get(index).copied().unwrap_or(0);
    let owner_at = |store: &Arc<Mutex<Vec<bilibili::FeedVideo>>>| {
        store
            .lock()
            .ok()
            .and_then(|guard| guard.get(index).map(|video| video.owner_mid))
            .unwrap_or(0)
    };
    let mid = if up_open {
        owner_at(up_store)
    } else if view == 0 {
        if masonry {
            owner_at(fav_videos)
        } else {
            owner_at(feed_store)
        }
    } else {
        match view {
            1 => search_state.lock().map(|state| mid_of(&state.mids)).unwrap_or(0),
            4 => folder_state.lock().map(|state| mid_of(&state.mids)).unwrap_or(0),
            5 => history_page.lock().map(|state| mid_of(&state.mids)).unwrap_or(0),
            _ => 0,
        }
    };
    if mid > 0 {
        Some(mid)
    } else {
        None
    }
}

/// Open a UP主's space: switch to the space view and load profile + 投稿 page 1.
pub(crate) fn open_space(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
    space_prev_mode: Arc<Mutex<i64>>,
    space_bvids: Arc<Mutex<Vec<String>>>,
    mid: i64,
) {
    crate::debug_log(&format!("open_space mid={mid}"));
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        let prev = ui.get_view_mode();
        if prev != 11 {
            if let Ok(mut mode) = space_prev_mode.lock() {
                *mode = prev as i64;
            }
        }
        ui.set_view_mode(11);
        ui.set_space_status(String::new().into());
        ui.set_space_list_loading(true);
        ui.set_space_page_text(String::new().into());
        ui.set_space_has_prev(false);
        ui.set_space_has_next(false);
        ui.set_space_follow_busy(false);
        // The space grid is paged too (its own pager row walks the same
        // pages): enter at page 0 and re-publish the shared offset.
        set_client_page(11, 0);
        publish_list_offset(&ui);
    }
    if let Ok(mut state) = space_state.lock() {
        *state = SpaceState {
            mid,
            page: 1,
            has_more: false,
            attribute: 0,
        };
    }
    load_space_profile(weak.clone(), session_slot.clone(), space_state.clone());
    load_space_videos(weak, session_slot, space_state, space_bvids, 1);
}

/// Return from the space view to the view that opened it.
pub(crate) fn space_back(weak: Weak<AppWindow>, space_prev_mode: Arc<Mutex<i64>>) {
    let mode = space_prev_mode.lock().map(|mode| *mode).unwrap_or(0);
    let mode = if mode == 11 { 0 } else { mode };
    if let Some(ui) = weak.upgrade() {
        ui.set_view_mode(mode as i32);
        // Returning to where the user was: re-publish that section's offset
        // (the space pager moved the shared property) but keep its page.
        publish_list_offset(&ui);
        update_page_bar(&ui);
    }
}

/// Human label for a `x/relation` `attribute`.
pub(crate) fn follow_text(attribute: i64) -> String {
    match attribute {
        2 => "已关注".to_string(),
        6 => "互相关注".to_string(),
        _ => "关注".to_string(),
    }
}

pub(crate) fn load_space_profile(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let mid = match space_state.lock() {
        Ok(state) => state.mid,
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_space_loading(true);
        ui.set_space_name(String::new().into());
        ui.set_space_sign(String::new().into());
        ui.set_space_stats(String::new().into());
        ui.set_space_avatar(Image::default());
        ui.set_space_follow_status(String::new().into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let info = session.space_info(mid).await;
            let stat = session.relation(mid).await.ok();
            let attribute = session.relation_state(mid).await.unwrap_or(-1);
            let avatar = match &info {
                Ok(info) if !info.face.is_empty() => session.fetch_avatar(&info.face).await.ok(),
                _ => None,
            };
            let (name, sign, stats, status) = match info {
                Ok(info) => {
                    let sign = if info.sign.trim().is_empty() {
                        "（暂无签名）".to_string()
                    } else {
                        info.sign.clone()
                    };
                    let (fans, following) = match &stat {
                        Some(stat) => (stat.follower.max(0) as u64, stat.following.max(0) as u64),
                        None => (info.fans.max(0) as u64, info.friend.max(0) as u64),
                    };
                    let mut stats = format!(
                        "Lv{} · 粉丝 {} · 关注 {}",
                        info.level,
                        format_view(fans),
                        format_view(following),
                    );
                    if !info.official.is_empty() {
                        stats.push_str(&format!(" · {}", info.official));
                    }
                    if !info.vip.is_empty() {
                        stats.push_str(&format!(" · {}", info.vip));
                    }
                    (info.name, sign, stats, String::new())
                }
                Err(err) => {
                    crate::debug_log(&format!("space_info mid={mid} err={err:#}"));
                    (
                        String::new(),
                        String::new(),
                        String::new(),
                        format!("UP主信息加载失败：{err}"),
                    )
                }
            };
            let weak_profile = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_profile.upgrade() {
                    if let Some((bytes, w, h)) = avatar {
                        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
                        buffer.make_mut_bytes().copy_from_slice(&bytes);
                        ui.set_space_avatar(Image::from_rgba8(buffer));
                    }
                    if !name.is_empty() {
                        ui.set_space_name(name.into());
                    }
                    if !sign.is_empty() {
                        ui.set_space_sign(sign.into());
                    }
                    if !stats.is_empty() {
                        ui.set_space_stats(stats.into());
                    }
                    ui.set_space_loading(false);
                    ui.set_space_status(status.into());
                }
            });
            if attribute >= 0 {
                if let Ok(mut state) = space_state.lock() {
                    state.attribute = attribute;
                }
                let weak_attr = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_attr.upgrade() {
                        ui.set_space_follow_state(attribute as i32);
                        ui.set_space_follow_text(follow_text(attribute).into());
                    }
                });
            }
        });
    }));
}

pub(crate) fn load_space_videos(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
    space_bvids: Arc<Mutex<Vec<String>>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (mid, page) = match space_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.mid, state.page)
        }
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    // One API page per screen: the space grid is client-paged too, and its own
    // pager row walks the same pages.
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        ui.set_space_list_loading(true);
        ui.set_space_current_page(page as i32);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = session.space_videos(mid, page, ps).await;
            let (rows, bvids, num, has_more, status, pics, covers) = match result {
                Ok(page_data) => {
                    let items = page_data.items;
                    let pics: Vec<String> = items.iter().map(|item| item.pic.clone()).collect();
                    let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
                    let rows: Vec<(String, CardRowData)> = items
                        .iter()
                        .map(|item| {
                            let meta = format!(
                                "{}播放 · {}弹幕 · {}",
                                format_view(item.play),
                                format_view(item.danmaku),
                                item.length
                            );
                            (
                                item.bvid.clone(),
                                CardRowData {
                                    pixels: None,
                                    title: item.title.clone(),
                                    meta,
                                    badge: item.length.clone(),
                                },
                            )
                        })
                        .collect();
                    let bvids: Vec<String> = rows.iter().map(|(bvid, _)| bvid.clone()).collect();
                    (
                        rows,
                        bvids,
                        page_data.page,
                        page_data.has_more,
                        String::new(),
                        pics,
                        covers,
                    )
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "space_videos mid={mid} page={page} err={err:#}"
                    ));
                    (
                        Vec::new(),
                        Vec::new(),
                        page,
                        false,
                        format!("投稿列表加载失败：{err}"),
                        Vec::new(),
                        0,
                    )
                }
            };
            let loaded = rows.len();
            let keys: Vec<String> = bvids.iter().take(covers).cloned().collect();
            let mut pics = pics;
            pics.truncate(covers);
            let weak_a = weak.clone();
            let space_bvids_a = space_bvids.clone();
            let space_state_a = space_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut guard) = space_bvids_a.lock() {
                        *guard = bvids;
                    }
                    if let Ok(mut state) = space_state_a.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    let items: Vec<CardRow> =
                        rows.iter().map(|(_, row)| build_card_row(row)).collect();
                    ui.set_space_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_space_list_loading(false);
                    ui.set_space_page_text(format!("第 {num} 页").into());
                    ui.set_space_current_page(num as i32);
                    ui.set_space_has_prev(num > 1);
                    ui.set_space_has_next(has_more);
                    if !status.is_empty() {
                        ui.set_space_status(status.into());
                    }
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(11, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    crate::debug_log(&format!("space load rendered items={loaded}"));
                }
            });
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| ThumbGuard::Ids(space_bvids.clone(), key),
                |ui| ui.get_space_items(),
                |row, image| row.thumb = image,
                format!("space mid={mid} page={num}"),
            )
            .await;
        });
    }));
}

pub(crate) fn toggle_space_follow(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    space_state: Arc<Mutex<SpaceState>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let mid = match space_state.lock() {
        Ok(state) => state.mid,
        Err(_) => return,
    };
    if mid <= 0 {
        return;
    }
    if let Some(ui) = weak.upgrade() {
        if ui.get_space_follow_busy() {
            return;
        }
        ui.set_space_follow_busy(true);
        ui.set_space_follow_status(String::new().into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let current = space_state.lock().map(|state| state.attribute).unwrap_or(0);
            let act = if current == 2 || current == 6 { 2 } else { 1 };
            let result = session.relation_modify(mid, act).await;
            let (attribute, message) = match result {
                Ok(()) => {
                    let attribute = session
                        .relation_state(mid)
                        .await
                        .unwrap_or(if act == 1 { 2 } else { 0 });
                    (attribute, format!("操作成功（act={act}）"))
                }
                Err(err) => {
                    crate::debug_log(&format!(
                        "relation_modify mid={mid} act={act} err={err:#}"
                    ));
                    (current, format!("操作失败：{err}"))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut state) = space_state.lock() {
                        state.attribute = attribute;
                    }
                    ui.set_space_follow_state(attribute as i32);
                    ui.set_space_follow_text(follow_text(attribute).into());
                    ui.set_space_follow_status(message.into());
                    ui.set_space_follow_busy(false);
                }
            });
        });
    }));
}

/// Log the account out server-side (only while the login is live), clear the
/// saved record, and return to the login page.
pub(crate) fn logout_account(weak: Weak<AppWindow>, generation: Arc<AtomicU64>, session_slot: SessionSlot) {
    let Some(session) = active_session(&session_slot) else {
        if let Some(ui) = weak.upgrade() {
            ui.set_status("没有可退出的账号".into());
        }
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_status("正在退出登录…".into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let live = matches!(
                session.nav().await,
                Ok(bilibili::NavOutcome::LoggedIn(_))
            );
            let status = if live {
                match session.logout().await {
                    Ok(()) => "已退出登录".to_string(),
                    Err(err) => {
                        crate::debug_log(&format!("logout err={err:#}"));
                        format!("退出请求失败（已清除本地账号）：{err}")
                    }
                }
            } else {
                "登录已失效，已清除本地账号".to_string()
            };
            accounts::clear();
            if let Ok(mut guard) = session_slot.lock() {
                *guard = None;
            }
            let my = generation.fetch_add(1, Ordering::SeqCst) + 1;
            crate::debug_log(&format!("logout cleared saved record status={status}"));
            crate::debug_log("identity cleared reason=logout");
            post(&weak, &generation, my, move |ui| {
                ui.set_logged_in(false);
                ui.set_qr_image(Image::default());
                ui.set_saved_account_name("".into());
                ui.set_user_name("".into());
                ui.set_user_id("".into());
                ui.set_login_state(0);
                ui.set_login_step("请选择登录方式".into());
                ui.set_login_percent(0);
                ui.set_login_progress(0.0);
                ui.set_login_failed(false);
                ui.set_login_error("".into());
                ui.set_login_countdown("".into());
                ui.set_status(status.into());
            });
        });
    }));
}

/// How long a built 收藏 tab (mixed, per folder, or library) stays valid.
pub(crate) const FAV_VIEW_TTL: Duration = Duration::from_secs(5 * 60);

/// Folders the mixed stream draws from (largest first).
pub(crate) const FAV_MIX_MAX_FOLDERS: usize = 8;

/// `related()` calls the mixed stream may issue (2 picks × 8 folders).
pub(crate) const FAV_MIX_RELATED: usize = 16;

/// Network work in flight while building a tab.
pub(crate) const FAV_MIX_CONCURRENCY: usize = 4;

/// Cards a tab publishes at most.
pub(crate) const FAV_CARDS_MAX: usize = 45;

/// The neutral 16:9 height/width ratio the card falls back to.
pub(crate) const FAV_DEFAULT_ASPECT: f32 = 0.5625;

pub(crate) static FAV_VIEW_CACHE: OnceLock<Mutex<FavViewCache>> = OnceLock::new();

pub(crate) fn fav_view_cache() -> &'static Mutex<FavViewCache> {
    FAV_VIEW_CACHE.get_or_init(|| Mutex::new(FavViewCache::default()))
}

pub(crate) fn fav_cache_ns(cache: &mut FavViewCache, mode: FavMode) -> &mut FavCacheNs {
    match mode {
        FavMode::Stream => &mut cache.stream,
        FavMode::Library => &mut cache.library,
    }
}

/// A cached `(mode, folder)` tab, if it is still inside [`FAV_VIEW_TTL`].
pub(crate) fn fav_cached(mode: FavMode, folder: Option<i64>, key: &[i64]) -> Option<FavCacheEntry> {
    let mut guard = fav_view_cache().lock().ok()?;
    let ns = fav_cache_ns(&mut guard, mode);
    let entry = match folder {
        Some(media_id) => ns.folders.get(&media_id)?,
        None => {
            if ns.key != key {
                return None;
            }
            ns.all.as_ref()?
        }
    };
    if entry.at.elapsed() >= FAV_VIEW_TTL {
        return None;
    }
    Some(entry.clone())
}

pub(crate) fn fav_store(
    mode: FavMode,
    folder: Option<i64>,
    key: Vec<i64>,
    cards: &[bilibili::FeedVideo],
    sources: usize,
) {
    if let Ok(mut guard) = fav_view_cache().lock() {
        let ns = fav_cache_ns(&mut guard, mode);
        ns.folders.retain(|_, entry| entry.at.elapsed() < FAV_VIEW_TTL);
        let entry = FavCacheEntry {
            at: Instant::now(),
            cards: cards.to_vec(),
            sources,
        };
        match folder {
            Some(media_id) => {
                ns.folders.insert(media_id, entry);
            }
            None => {
                ns.key = key;
                ns.all = Some(entry);
            }
        }
    }
}

/// splitmix64 step. Same generator the data layer uses; the crate has no `rand`
/// dependency and this is only used for shuffling, not for anything secret.
pub(crate) fn fav_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Clock seed, so repeated visits to 全部 sample different videos.
pub(crate) fn fav_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ 0x9E37_79B9_7F4A_7C15
}

pub(crate) fn fav_shuffle<T>(items: &mut [T]) {
    let mut state = fav_seed();
    for index in (1..items.len()).rev() {
        let pick = (fav_rand(&mut state) % (index as u64 + 1)) as usize;
        items.swap(index, pick);
    }
}

/// Two distinct picks from one folder's page (one when it holds a single item).
pub(crate) fn fav_pick_two(items: &[bilibili::FeedVideo]) -> Vec<bilibili::FeedVideo> {
    let mut picks = Vec::with_capacity(2);
    if items.is_empty() {
        return picks;
    }
    let mut state = fav_seed();
    let len = items.len();
    let first = (fav_rand(&mut state) % len as u64) as usize;
    picks.push(items[first].clone());
    if len > 1 {
        let second = (fav_rand(&mut state) % len as u64) as usize;
        let second = if second == first {
            (second + 1) % len
        } else {
            second
        };
        picks.push(items[second].clone());
    }
    picks
}

/// Append `items` to `target`, skipping any bvid already in the list.
pub(crate) fn fav_push_unique(
    target: &mut Vec<bilibili::FeedVideo>,
    seen: &mut HashSet<String>,
    items: &[bilibili::FeedVideo],
) {
    for video in items {
        if video.bvid.is_empty() {
            continue;
        }
        if seen.insert(video.bvid.clone()) {
            target.push(video.clone());
        }
    }
}

/// The mixed stream's folder set: largest first, capped.
pub(crate) fn fav_largest_folders(folders: &[bilibili::FavFolder]) -> Vec<bilibili::FavFolder> {
    let mut picked = folders.to_vec();
    picked.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.id.cmp(&b.id)));
    picked.truncate(FAV_MIX_MAX_FOLDERS);
    picked
}

/// Height/width of a decoded cover, or the neutral ratio when a dimension is
/// unusable.
pub(crate) fn fav_aspect(pixels: &CoverPixels) -> f32 {
    let (_, width, height) = pixels;
    if *width == 0 || *height == 0 {
        FAV_DEFAULT_ASPECT
    } else {
        *height as f32 / *width as f32
    }
}

pub(crate) fn fav_aspect_stats(rows: &[MasonryCard]) -> (f32, f32, f32) {
    if rows.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f32;
    for row in rows {
        min = min.min(row.aspect);
        max = max.max(row.aspect);
        sum += row.aspect;
    }
    (min, sum / rows.len() as f32, max)
}

pub(crate) async fn fav_fetch_pages(
    session: &Arc<bilibili::Session>,
    folders: &[bilibili::FavFolder],
) -> Vec<(Vec<bilibili::FeedVideo>, Option<String>)> {
    let fetches = folders.iter().map(|folder| {
        let session = session.clone();
        let media_id = folder.id;
        async move {
            match session.fav_items(media_id, 1).await {
                Ok((items, _)) => (items, None),
                Err(err) => (
                    Vec::new(),
                    Some(format!("folder {media_id}: {err:#}")),
                ),
            }
        }
    });
    let mut stream = futures::stream::iter(fetches).buffer_unordered(FAV_MIX_CONCURRENCY);
    let mut pages = Vec::new();
    while let Some(page) = stream.next().await {
        pages.push(page);
    }
    pages
}

/// `related()` for every pick, at most 16 calls and 4 in flight; failures are
/// skipped (an empty list).
pub(crate) async fn fav_related_batch(
    session: &Arc<bilibili::Session>,
    picks: &[bilibili::FeedVideo],
) -> Vec<Vec<bilibili::FeedVideo>> {
    let fetches = picks.iter().take(FAV_MIX_RELATED).map(|video| {
        let session = session.clone();
        let bvid = video.bvid.clone();
        async move {
            match session.related(&bvid).await {
                Ok(items) => items,
                Err(_) => Vec::new(),
            }
        }
    });
    let mut stream = futures::stream::iter(fetches).buffer_unordered(FAV_MIX_CONCURRENCY);
    let mut related = Vec::new();
    while let Some(items) = stream.next().await {
        related.push(items);
    }
    related
}

/// Build the mixed 全部 stream. Returns the cards, how many folders contributed
/// and the first folder error (only used when nothing at all came back).
pub(crate) async fn fav_build_mixed(
    session: &Arc<bilibili::Session>,
    folders: &[bilibili::FavFolder],
) -> (Vec<bilibili::FeedVideo>, usize, Option<String>) {
    let picked = fav_largest_folders(folders);
    let pages = fav_fetch_pages(session, &picked).await;
    let mut sources = 0usize;
    let mut first_error: Option<String> = None;
    let mut cards: Vec<bilibili::FeedVideo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut picks: Vec<bilibili::FeedVideo> = Vec::new();
    for (items, error) in &pages {
        if let Some(error) = error {
            if first_error.is_none() {
                first_error = Some(error.clone());
            }
        }
        if items.is_empty() {
            continue;
        }
        sources += 1;
        fav_push_unique(&mut cards, &mut seen, items);
        picks.extend(fav_pick_two(items));
    }
    picks.truncate(FAV_MIX_RELATED);
    for related in fav_related_batch(session, &picks).await {
        fav_push_unique(&mut cards, &mut seen, &related);
    }
    fav_shuffle(&mut cards);
    cards.truncate(FAV_CARDS_MAX);
    (cards, sources, first_error)
}

/// Build one folder tab: its own page plus two picks' related videos.
pub(crate) async fn fav_build_folder(
    session: &Arc<bilibili::Session>,
    media_id: i64,
) -> Result<Vec<bilibili::FeedVideo>> {
    let (items, _) = session.fav_items(media_id, 1).await?;
    let picks = fav_pick_two(&items);
    let related = fav_related_batch(session, &picks).await;
    let mut cards: Vec<bilibili::FeedVideo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    fav_push_unique(&mut cards, &mut seen, &items);
    for list in &related {
        fav_push_unique(&mut cards, &mut seen, list);
    }
    fav_shuffle(&mut cards);
    cards.truncate(FAV_CARDS_MAX);
    Ok(cards)
}

/// 收藏夹 + one folder: that folder's own videos, in the folder's order, filled
/// page by page up to [`FAV_CARDS_MAX`]. No `related()` expansion and no
/// shuffle - this tab is the plain listing, and the 推荐流 tab is where the
/// recommendations live.
pub(crate) async fn fav_build_library_folder(
    session: &Arc<bilibili::Session>,
    media_id: i64,
) -> Result<Vec<bilibili::FeedVideo>> {
    let mut cards: Vec<bilibili::FeedVideo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut page = 1u32;
    loop {
        let (items, has_more) = session.fav_items(media_id, page).await?;
        if items.is_empty() {
            break;
        }
        let before = cards.len();
        fav_push_unique(&mut cards, &mut seen, &items);
        // The cap, the folder's last page, or a page that brought nothing new.
        if !has_more || cards.len() >= FAV_CARDS_MAX || cards.len() == before {
            break;
        }
        page += 1;
    }
    cards.truncate(FAV_CARDS_MAX);
    Ok(cards)
}

/// 收藏夹 + 全部: every folder's first page, merged in folder order, deduped and
/// capped. Plain listing: no `related()`, no shuffle. `buffered` keeps the
/// folder order while still running [`FAV_MIX_CONCURRENCY`] fetches at a time.
pub(crate) async fn fav_build_library_all(
    session: &Arc<bilibili::Session>,
    folders: &[bilibili::FavFolder],
) -> (Vec<bilibili::FeedVideo>, usize, Option<String>) {
    let fetches = folders.iter().map(|folder| {
        let session = session.clone();
        let media_id = folder.id;
        async move {
            session
                .fav_items(media_id, 1)
                .await
                .map(|(items, _)| items)
                .map_err(|err| format!("folder {media_id}: {err:#}"))
        }
    });
    let results: Vec<Result<Vec<bilibili::FeedVideo>, String>> = futures::stream::iter(fetches)
        .buffered(FAV_MIX_CONCURRENCY)
        .collect()
        .await;
    let mut cards: Vec<bilibili::FeedVideo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut sources = 0usize;
    let mut first_error: Option<String> = None;
    for result in results {
        match result {
            Ok(items) => {
                if items.is_empty() {
                    continue;
                }
                sources += 1;
                fav_push_unique(&mut cards, &mut seen, &items);
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    cards.truncate(FAV_CARDS_MAX);
    (cards, sources, first_error)
}

/// Build the cards of one `(mode, tab)` pair.
pub(crate) async fn fav_build_tab(
    session: &Arc<bilibili::Session>,
    folders: &[bilibili::FavFolder],
    mode: FavMode,
    folder: Option<&bilibili::FavFolder>,
) -> (Vec<bilibili::FeedVideo>, usize, Option<String>) {
    match (mode, folder) {
        (FavMode::Stream, None) => fav_build_mixed(session, folders).await,
        (FavMode::Stream, Some(folder)) => match fav_build_folder(session, folder.id).await {
            Ok(cards) => (cards, 1usize, None),
            Err(err) => (Vec::new(), 0usize, Some(format!("{err:#}"))),
        },
        (FavMode::Library, None) => fav_build_library_all(session, folders).await,
        (FavMode::Library, Some(folder)) => {
            match fav_build_library_folder(session, folder.id).await {
                Ok(cards) => (cards, 1usize, None),
                Err(err) => (Vec::new(), 0usize, Some(format!("{err:#}"))),
            }
        }
    }
}

/// Publish `videos` as the masonry, taking every already-decoded cover (and its
/// real aspect) from the shared cache. Returns how many covers were warm, the
/// rows still to stream and the published aspect range.
pub(crate) fn fav_publish(
    ui: &AppWindow,
    view: &Arc<Mutex<FavViewState>>,
    mode: FavMode,
    tab: i32,
    videos: Vec<bilibili::FeedVideo>,
) -> (usize, Vec<FavPendingCover>, (f32, f32, f32)) {
    let warm: Vec<Option<Arc<CoverPixels>>> = match cover_cache().lock() {
        Ok(mut cache) => videos
            .iter()
            .map(|video| {
                if video.pic.is_empty() {
                    None
                } else {
                    cache.get(&video.pic)
                }
            })
            .collect(),
        Err(_) => videos.iter().map(|_| None).collect(),
    };
    let mut rows = Vec::with_capacity(videos.len());
    let mut pending = Vec::new();
    let mut real = 0usize;
    for (index, video) in videos.iter().enumerate() {
        let (cover, aspect) = match warm.get(index).and_then(|hit| hit.as_ref()) {
            Some(pixels) => {
                real += 1;
                (to_thumb_shared(pixels), fav_aspect(pixels))
            }
            None => {
                if !video.pic.is_empty() {
                    pending.push(FavPendingCover {
                        index,
                        pic: video.pic.clone(),
                        bvid: video.bvid.clone(),
                    });
                }
                (Image::default(), FAV_DEFAULT_ASPECT)
            }
        };
        rows.push(MasonryCard {
            bvid: video.bvid.clone().into(),
            title: video.title.clone().into(),
            owner: video.owner.clone().into(),
            duration: format_duration(video.duration).into(),
            cover,
            aspect,
        });
    }
    let stats = fav_aspect_stats(&rows);
    if let Ok(mut state) = view.lock() {
        if let Ok(mut store) = state.videos.lock() {
            *store = videos;
        }
        state.published = Some((mode, tab));
        state.loading = false;
    }
    // A settled non-empty tab clears the page's quiet empty state (the skeleton
    // takes over again the moment the next load clears the model).
    ui.set_fav_empty(false);
    ui.set_masonry(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    (real, pending, stats)
}

/// Empty the masonry while a load runs: the UI paints its skeleton blocks.
pub(crate) fn fav_clear_cards(ui: &AppWindow, view: &Arc<Mutex<FavViewState>>) {
    ui.set_masonry(ModelRc::from(std::rc::Rc::new(VecModel::from(
        Vec::<MasonryCard>::new(),
    ))));
    ui.set_fav_empty(false);
    if let Ok(mut state) = view.lock() {
        if let Ok(mut store) = state.videos.lock() {
            store.clear();
        }
        state.published = None;
    }
}

/// Publish the genuine empty state (no folders / a folder with nothing in it).
pub(crate) fn fav_publish_empty(ui: &AppWindow, view: &Arc<Mutex<FavViewState>>, mode: FavMode, tab: i32) {
    fav_clear_cards(ui, view);
    ui.set_fav_empty(true);
    if let Ok(mut state) = view.lock() {
        state.published = Some((mode, tab));
        state.loading = false;
    }
}

pub(crate) fn fav_publish_tabs(ui: &AppWindow, folders: &[bilibili::FavFolder]) {
    let names: Vec<SharedString> = folders
        .iter()
        .map(|folder| folder.name.clone().into())
        .collect();
    ui.set_fav_tabs(ModelRc::from(std::rc::Rc::new(VecModel::from(names))));
}

/// First few bvids of a published tab, for the log line the owner reads:
/// `fav folder=… tab=… bvids=…`.
pub(crate) fn fav_bvid_summary(cards: &[bilibili::FeedVideo]) -> String {
    let shown: Vec<&str> = cards.iter().take(4).map(|video| video.bvid.as_str()).collect();
    if shown.is_empty() {
        "-".to_string()
    } else if cards.len() > shown.len() {
        format!("{}+{}", shown.join(","), cards.len() - shown.len())
    } else {
        shown.join(",")
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fav_log_tab(
    mode: FavMode,
    tab: i32,
    folder: Option<i64>,
    cards: usize,
    bvids: &str,
    real: usize,
    aspect: (f32, f32, f32),
    ms: u128,
    hit: bool,
    sources: usize,
) {
    let folder = folder
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string());
    crate::debug_log(&format!(
        "fav folder={folder} tab={tab} mode={} cards={cards} bvids={bvids} real_aspect={real}/{cards} sources={sources} ms={ms} hit={} aspect={:.3}/{:.3}/{:.3}",
        mode.label(),
        u8::from(hit),
        aspect.0,
        aspect.1,
        aspect.2
    ));
}

pub(crate) fn fav_log_mix(cards: usize, real: usize, sources: usize, ms: u128, hit: bool) {
    crate::debug_log(&format!(
        "fav mix cards={cards} real_aspect={real}/{cards} sources={sources} ms={ms} hit={}",
        u8::from(hit)
    ));
}

/// Stream a published tab's missing covers and patch each row as it decodes.
pub(crate) fn fav_spawn_covers(
    weak: Weak<AppWindow>,
    session: Option<Arc<bilibili::Session>>,
    generation: Arc<AtomicU64>,
    store: Arc<Mutex<Vec<bilibili::FeedVideo>>>,
    my: u64,
    tab: i32,
    pending: Vec<FavPendingCover>,
) {
    let Some(session) = session else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            return;
        };
        let began = Instant::now();
        let total = pending.len();
        let pics: Vec<String> = pending.iter().map(|cover| cover.pic.clone()).collect();
        let indices: Vec<usize> = pending.iter().map(|cover| cover.index).collect();
        let bvids: Vec<String> = pending.iter().map(|cover| cover.bvid.clone()).collect();
        let mut delivered = 0usize;
        runtime.block_on(async {
            stream_thumbs(&session, pics, |_| {}, |slot, pixels| {
                delivered += 1;
                let Some(pixels) = pixels else {
                    return;
                };
                let index = indices.get(slot).copied().unwrap_or(usize::MAX);
                let bvid = bvids.get(slot).cloned().unwrap_or_default();
                let guard = ThumbGuard::Fav(generation.clone(), my, store.clone(), bvid);
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        if guard.allows(index) {
                            // Decode on the UI thread: `Image` is not `Send`.
                            let aspect = fav_aspect(&pixels);
                            patch_masonry_row(ui.get_masonry(), index, to_thumb(&Some(pixels)), aspect);
                        }
                    }
                });
            })
            .await;
        });
        crate::debug_log(&format!(
            "fav covers n={}/{} ms={} tab={tab}",
            delivered,
            total,
            began.elapsed().as_millis()
        ));
    }));
}

/// Load one `(mode, tab)` pair (respecting both caches) and publish it. Must run
/// on the UI thread: it reads the folder list, checks the cache and touches the
/// model.
pub(crate) fn fav_load_tab(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    view: Arc<Mutex<FavViewState>>,
    tab: i32,
    mode: FavMode,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let (folders, generation, store, my, tab) = {
        let Ok(mut state) = view.lock() else {
            return;
        };
        let tab = tab.max(0);
        let tab = if tab as usize > state.folders.len() {
            0
        } else {
            tab
        };
        state.tab = tab;
        state.mode = mode;
        state.loading = true;
        let my = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
        (
            state.folders.clone(),
            state.generation.clone(),
            state.videos.clone(),
            my,
            tab,
        )
    };
    ui.set_fav_tab(tab);

    let folder = if tab == 0 {
        None
    } else {
        folders.get((tab - 1) as usize).cloned()
    };
    if tab > 0 && folder.is_none() {
        fav_publish_empty(&ui, &view, mode, tab);
        crate::debug_log("fav view empty reason=no_folders");
        return;
    }

    let key: Vec<i64> = folders.iter().map(|folder| folder.id).collect();
    let folder_id = folder.as_ref().map(|folder| folder.id);
    let cached = fav_cached(mode, folder_id, &key);
    if let Some(entry) = cached {
        let cards = entry.cards;
        let total = cards.len();
        let bvids = fav_bvid_summary(&cards);
        let (real, pending, aspect) = fav_publish(&ui, &view, mode, tab, cards);
        fav_log_tab(
            mode,
            tab,
            folder_id,
            total,
            &bvids,
            real,
            aspect,
            0,
            true,
            entry.sources,
        );
        if mode == FavMode::Stream && folder.is_none() {
            fav_log_mix(total, real, entry.sources, 0, true);
        }
        fav_spawn_covers(
            weak,
            active_session(&session_slot),
            generation,
            store,
            my,
            tab,
            pending,
        );
        return;
    }

    // Cache miss: clear first, so no row from the previous tab survives.
    fav_clear_cards(&ui, &view);
    let Some(session) = active_session(&session_slot) else {
        fav_publish_empty(&ui, &view, mode, tab);
        crate::debug_log("fav view empty reason=error:no session");
        return;
    };
    let key_c = key;
    let folders_c = folders;
    let folder_c = folder.clone();
    let weak_w = weak.clone();
    let view_w = view.clone();
    let generation_w = generation.clone();
    let store_w = store.clone();
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = bilibili::shared_runtime() else {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_w.upgrade() {
                    fav_publish_empty(&ui, &view_w, mode, tab);
                }
            });
            return;
        };
        let began = Instant::now();
        let (cards, sources, error) = runtime.block_on(fav_build_tab(
            &session,
            &folders_c,
            mode,
            folder_c.as_ref(),
        ));
        let ms = began.elapsed().as_millis();
        if !cards.is_empty() {
            fav_store(mode, folder_c.as_ref().map(|folder| folder.id), key_c, &cards, sources);
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak_w.upgrade() else {
                return;
            };
            if generation_w.load(Ordering::SeqCst) != my {
                return;
            }
            let current = view_w.lock().map(|state| (state.mode, state.tab)).unwrap_or((
                FavMode::Stream,
                -1,
            ));
            if current != (mode, tab) {
                return;
            }
            if cards.is_empty() {
                fav_publish_empty(&ui, &view_w, mode, tab);
                match &error {
                    Some(error) => {
                        crate::debug_log(&format!("fav view empty reason=error:{error}"))
                    }
                    None => crate::debug_log(&format!(
                        "fav view mode={} tab={tab} cards=0 ms={ms} hit=0",
                        mode.label()
                    )),
                }
                return;
            }
            let total = cards.len();
            let bvids = fav_bvid_summary(&cards);
            let (real, pending, aspect) = fav_publish(&ui, &view_w, mode, tab, cards);
            fav_log_tab(
                mode,
                tab,
                folder_c.as_ref().map(|folder| folder.id),
                total,
                &bvids,
                real,
                aspect,
                ms,
                false,
                sources,
            );
            if mode == FavMode::Stream && folder_c.is_none() {
                fav_log_mix(total, real, sources, ms, false);
            }
            fav_spawn_covers(
                weak_w.clone(),
                Some(session),
                generation_w.clone(),
                store_w.clone(),
                my,
                tab,
                pending,
            );
        });
    }));
}

/// UI thread: fetch the folders (10 min cached in the data layer), publish the
/// tab strip and load the tab the view is on.
pub(crate) fn fav_fetch_folders(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    view: Arc<Mutex<FavViewState>>,
) {
    {
        let Ok(mut state) = view.lock() else {
            return;
        };
        if state.folders_loading {
            return;
        }
        state.folders_loading = true;
    }
    std::thread::spawn(move || {
        let mut session = None;
        for _ in 0..240 {
            session = active_session(&session_slot);
            if session.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let slot = session_slot.clone();
        let Some(session) = session else {
            let weak = weak.clone();
            let view = view.clone();
            let _ = slint::invoke_from_event_loop(move || {
                fav_apply_folders(
                    &weak,
                    &slot,
                    &view,
                    Vec::new(),
                    Some("no session".to_string()),
                );
            });
            return;
        };
        let result = match bilibili::shared_runtime() {
            Some(runtime) => runtime.block_on(session.fav_folders()),
            None => Err(anyhow!("no runtime")),
        };
        let (folders, error) = match result {
            Ok(folders) => (folders, None),
            Err(err) => {
                crate::debug_log(&format!("fav view folders err={err:#}"));
                (Vec::new(), Some(format!("{err:#}")))
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            fav_apply_folders(&weak, &slot, &view, folders, error);
        });
    });
}

/// Which of the two folder-scoped 收藏 tabs is on screen. `view_mode` is the
/// Rust-owned routing truth: 3 is the library route (收藏夹), 0 + home_tab 5 is
/// 推荐流 (see `open_collection` in `ui/app.slint`).
pub(crate) fn fav_page_mode(ui: &AppWindow) -> FavMode {
    if ui.get_view_mode() == 3 {
        FavMode::Library
    } else {
        FavMode::Stream
    }
}

/// UI thread: publish the folder list (and its strip) and reload when the set
/// changed or the shown tab is not the requested one.
pub(crate) fn fav_apply_folders(
    weak: &Weak<AppWindow>,
    session_slot: &SessionSlot,
    view: &Arc<Mutex<FavViewState>>,
    folders: Vec<bilibili::FavFolder>,
    error: Option<String>,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let (folders, tab, published, changed) = {
        let Ok(mut state) = view.lock() else {
            return;
        };
        state.folders_loaded = true;
        state.folders_loading = false;
        let had = !state.folders.is_empty();
        let changed = state
            .folders
            .iter()
            .map(|folder| folder.id)
            .ne(folders.iter().map(|folder| folder.id));
        // A failed refetch keeps the list the strip is already showing.
        if !folders.is_empty() || !had {
            state.folders = folders;
        }
        if state.tab > state.folders.len() as i32 {
            state.tab = 0;
            ui.set_fav_tab(0);
        }
        (
            state.folders.clone(),
            state.tab,
            state.published,
            changed,
        )
    };

    fav_publish_tabs(&ui, &folders);
    // One status line for the page (both tabs read it in the empty state).
    ui.set_fav_list_status(
        match &error {
            Some(err) => format!("收藏夹加载失败：{err}"),
            None => String::new(),
        }
        .into(),
    );
    if error.is_none() {
        crate::debug_log(&format!("fav view folders n={}", folders.len()));
    }
    let mode = fav_page_mode(&ui);
    if folders.is_empty() {
        fav_publish_empty(&ui, view, mode, tab);
        match &error {
            Some(error) => crate::debug_log(&format!("fav view empty reason=error:{error}")),
            None => crate::debug_log("fav view empty reason=no_folders"),
        }
        return;
    }
    if changed || published != Some((mode, tab)) {
        fav_load_tab(weak.clone(), session_slot.clone(), view.clone(), tab, mode);
    }
}

/// Enter the 收藏 page (either tab): publish the known strip, load the selected
/// folder in the current tab's mode and refresh the folders in the background.
pub(crate) fn fav_start(weak: Weak<AppWindow>, session_slot: SessionSlot, view: Arc<Mutex<FavViewState>>) {
    let (folders, tab, published) = match view.lock() {
        Ok(state) => (state.folders.clone(), state.tab, state.published),
        Err(_) => return,
    };
    let mode = weak
        .upgrade()
        .map(|ui| {
            fav_publish_tabs(&ui, &folders);
            fav_page_mode(&ui)
        })
        .unwrap_or(FavMode::Stream);
    if !folders.is_empty() && published != Some((mode, tab)) {
        fav_load_tab(weak.clone(), session_slot.clone(), view.clone(), tab, mode);
    }
    fav_fetch_folders(weak, session_slot, view);
}

pub(crate) fn load_folder_page(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    folder_state: Arc<Mutex<FolderState>>,
    folder_bvids: Arc<Mutex<Vec<String>>>,
    page: i64,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let (media_id, page, title) = match folder_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.media_id, state.page, state.title.clone())
        }
        Err(_) => return,
    };
    // One API page per screen (see `load_region`).
    let ps = current_page_size(&weak);
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_folder_title(title.into());
        ui.set_folder_current_page(page as i32);
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let list = match session.fav_list(media_id, page, ps).await {
                Ok(list) => list,
                Err(err) => {
                    crate::debug_log(&format!("fav_list err={err:#}"));
                    let message = format!("收藏内容加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_folder_items(ModelRc::from(std::rc::Rc::new(VecModel::from(
                                Vec::<FolderItemRow>::new(),
                            ))));
                            ui.set_folder_page_text(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };

            let items = list.items;
            let mut pics: Vec<String> = items.iter().map(|item| item.cover.clone()).collect();
            let covers = pics.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, FolderRowData)> = items
                .iter()
                .map(|item| {
                    let meta = format!(
                        "{} · {}播放 · {}",
                        item.upper,
                        format_view(item.play),
                        format_duration(item.duration)
                    );
                    (
                        item.bvid.clone(),
                        FolderRowData {
                            pixels: None,
                            title: item.title.clone(),
                            meta,
                            badge: format_duration(item.duration),
                        },
                    )
                })
                .collect();
            let bvids: Vec<String> = rows.iter().map(|(bvid, _)| bvid.clone()).collect();
            let mids: Vec<i64> = items.iter().map(|item| item.owner_mid).collect();
            let page = list.page;
            let has_more = list.has_more;
            let title = list.title;
            let loaded = rows.len();
            let weak_a = weak.clone();
            let folder_bvids_a = folder_bvids.clone();
            let folder_state_a = folder_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    if let Ok(mut guard) = folder_bvids_a.lock() {
                        *guard = bvids;
                    }
                    if let Ok(mut state) = folder_state_a.lock() {
                        state.title = title.clone();
                        state.page = page;
                        state.has_more = has_more;
                        state.mids = mids;
                    }
                    let items: Vec<FolderItemRow> = rows
                        .iter()
                        .map(|(_, row)| FolderItemRow {
                            thumb: to_thumb(&row.pixels),
                            title: row.title.clone().into(),
                            meta: row.meta.clone().into(),
                            badge: row.badge.clone().into(),
                        })
                        .collect();
                    ui.set_folder_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    publish_refresh(&ui, false, 1.0);
                    if !title.is_empty() {
                        ui.set_folder_title(title.into());
                    }
                    ui.set_folder_page_text(format!("第 {page} 页").into());
                    ui.set_folder_current_page(page as i32);
                    ui.set_folder_has_prev(page > 1);
                    ui.set_folder_has_next(has_more);
                    // One API page is one screen: the offset restarts at 0.
                    set_client_page(4, 0);
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("folder load rendered items={loaded}"));
                }
            });
            pics.truncate(covers);
            let keys: Vec<String> = items.iter().take(covers).map(|item| item.bvid.clone()).collect();
            stream_cover_list(
                &weak,
                &session,
                pics,
                keys,
                0,
                move |_index, key| ThumbGuard::Ids(folder_bvids.clone(), key),
                |ui| ui.get_folder_items(),
                |row, image| row.thumb = image,
                format!("folder page={page}"),
            )
            .await;
        });
    }));
}

pub(crate) fn format_view(view: u64) -> String {
    if view >= 100_000_000 {
        format!("{:.1}亿", view as f64 / 100_000_000.0)
    } else if view >= 10_000 {
        format!("{:.1}万", view as f64 / 10_000.0)
    } else {
        view.to_string()
    }
}

pub(crate) fn format_duration(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

pub(crate) fn format_mmss(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

pub(crate) fn format_speed(speed: f64) -> String {
    let label = if (speed - speed.round()).abs() < 1e-9 {
        format!("{speed:.1}")
    } else {
        let text = format!("{speed:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    format!("{label}x")
}

/// `mm:ss` for a 睡眠定时 countdown.
pub(crate) fn fmt_countdown(ms: u64) -> String {
    format_mmss(ms)
}

/// Destination PNG path for 截图: the user's Pictures folder when present,
/// otherwise the project directory.
pub(crate) fn screenshot_path() -> PathBuf {
    let dir = std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Pictures"))
        .filter(|path| path.is_dir())
        .or_else(|| settings::settings_path().parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    dir.join(format!("bilibili_shot_{stamp}.png"))
}

/// Run `f` on the UI thread, but only if `my` is still the current generation.
pub(crate) fn post<F>(weak: &Weak<AppWindow>, generation: &Arc<AtomicU64>, my: u64, f: F)
where
    F: FnOnce(&AppWindow) + Send + 'static,
{
    let weak = weak.clone();
    let generation = generation.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if generation.load(Ordering::SeqCst) != my {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            f(&ui);
        }
    });
}

pub(crate) fn post_status(weak: &Weak<AppWindow>, generation: &Arc<AtomicU64>, my: u64, text: &str) {
    let text = text.to_string();
    post(weak, generation, my, move |ui| ui.set_status(text.into()));
}

/// Emit one login step: switch the login view to the progress state, set the
/// step label and percentage, and log `id` so the real network stage is visible
/// in `BILI_DEBUG_LOG`. `id` names the code stage; `label` is what the user reads.
pub(crate) fn post_step(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    id: &str,
    label: &str,
    percent: i32,
) {
    crate::debug_log(&format!("login-step id={id} pct={percent} label={label}"));
    let label = label.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_login_state(1);
        ui.set_login_step(label.into());
        ui.set_login_percent(percent);
        ui.set_login_progress(percent as f32 / 100.0);
        ui.set_login_countdown("".into());
        ui.set_login_failed(false);
        ui.set_login_error("".into());
    });
}

/// Switch the login view to the "waiting for scan" state (QR centered) and
/// refresh the live countdown/progress while the poll loop runs.
pub(crate) fn post_login_qr(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    step: &str,
    percent: i32,
    countdown: &str,
) {
    crate::debug_log(&format!(
        "login-step id=waiting pct={percent} label={step} countdown={countdown}"
    ));
    let step = step.to_string();
    let countdown = countdown.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_login_state(2);
        ui.set_login_step(step.into());
        ui.set_login_percent(percent);
        ui.set_login_progress(percent as f32 / 100.0);
        ui.set_login_countdown(countdown.into());
        ui.set_login_failed(false);
        ui.set_login_error("".into());
    });
}

/// Report a failed login step: keep the progress view but surface the failed
/// step label plus the API/error code, and mark the attempt failed.
pub(crate) fn post_login_fail(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    id: &str,
    label: &str,
    percent: i32,
    code: &str,
) {
    crate::debug_log(&format!(
        "login-step id={id} pct={percent} label={label} FAILED code={code}"
    ));
    let label = label.to_string();
    let code = code.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_login_state(1);
        ui.set_login_step(label.into());
        ui.set_login_percent(percent);
        ui.set_login_progress(percent as f32 / 100.0);
        ui.set_login_countdown("".into());
        ui.set_login_failed(true);
        ui.set_login_error(code.into());
    });
}

/// Fall back to the login choice page (auto-login failed or nothing saved).
pub(crate) fn post_login_choice(
    weak: &Weak<AppWindow>,
    generation: &Arc<AtomicU64>,
    my: u64,
    reason: &str,
) {
    crate::debug_log(&format!("login-choice shown reason={reason}"));
    let reason = reason.to_string();
    post(weak, generation, my, move |ui| {
        ui.set_logged_in(false);
        ui.set_qr_image(Image::default());
        ui.set_login_state(0);
        ui.set_login_step("请选择登录方式".into());
        ui.set_login_percent(0);
        ui.set_login_progress(0.0);
        ui.set_login_failed(false);
        ui.set_login_error(reason.into());
        ui.set_login_countdown("".into());
    });
}

/// Load one page of the 动态 feed and (append to) the card list.
///
/// `append` loads the next cursor page on top of the loaded rows; `target_page`
/// is the client page the list lands on afterwards (`None` keeps the current
/// one, and a non-append load restarts at 0 because the data is replaced).
pub(crate) fn load_dynamic(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    dynamic_ids: Arc<Mutex<Vec<String>>>,
    dynamic_offset: Arc<Mutex<String>>,
    dynamic_has_more: Arc<Mutex<bool>>,
    append: bool,
    target_page: Option<i64>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    let offset = dynamic_offset
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    if let Some(ui) = weak.upgrade() {
        publish_refresh(&ui, true, 0.0);
        ui.set_dynamic_status(String::new().into());
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.dynamic_feed(&offset, "all").await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("dynamic_feed err={err:#}"));
                    let message = format!("动态加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            publish_refresh(&ui, false, 1.0);
                            ui.set_dynamic_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let items = page.items;
            let covers = items.len().min(bilibili::THUMB_MAX_PER_VIEW);
            let rows: Vec<(String, DynamicRowData)> = items
                .iter()
                .map(|item| {
                    let stats = format!(
                        "赞 {} · 评论 {} · 转发 {}",
                        format_view(item.like_count),
                        format_view(item.comment_count),
                        format_view(item.forward_count)
                    );
                    let liked = item.liked;
                    let has_cover = !item.archive_cover.is_empty() || !item.images.is_empty();
                    (
                        item.id_str.clone(),
                        DynamicRowData {
                            id: item.id_str.clone(),
                            author: item.author_name.clone(),
                            face: None,
                            time: item.pub_time.clone(),
                            text: item.text.clone(),
                            has_cover,
                            cover: None,
                            title: item.archive_title.clone(),
                            meta: item.archive_meta.clone(),
                            liked,
                            like_text: if liked {
                                "已赞".to_string()
                            } else {
                                "点赞".to_string()
                            },
                            stats,
                        },
                    )
                })
                .collect();
            let new_offset = page.offset;
            let has_more = page.has_more;
            let base_len = if append {
                dynamic_ids.lock().map(|ids| ids.len()).unwrap_or(0)
            } else {
                0
            };
            let loaded = rows.len();
            let weak_a = weak.clone();
            let dynamic_ids_a = dynamic_ids.clone();
            let dynamic_offset_a = dynamic_offset.clone();
            let dynamic_has_more_a = dynamic_has_more.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_a.upgrade() {
                    let mut all_rows: Vec<DynamicRow> = if append {
                        let model = ui.get_dynamic_items();
                        (0..model.row_count())
                            .filter_map(|index| model.row_data(index))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut all_ids: Vec<String> = if append {
                        dynamic_ids_a
                            .lock()
                            .map(|ids| ids.clone())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    for (id, row) in rows {
                        all_ids.push(id);
                        all_rows.push(DynamicRow {
                            id: row.id.into(),
                            author: row.author.into(),
                            face: to_thumb(&row.face),
                            time: row.time.into(),
                            text: row.text.into(),
                            has_cover: row.has_cover,
                            cover: to_thumb(&row.cover),
                            title: row.title.into(),
                            meta: row.meta.into(),
                            liked: row.liked,
                            like_text: row.like_text.into(),
                            stats: row.stats.into(),
                        });
                    }
                    if let Ok(mut guard) = dynamic_ids_a.lock() {
                        *guard = all_ids;
                    }
                    if let Ok(mut guard) = dynamic_offset_a.lock() {
                        *guard = new_offset;
                    }
                    if let Ok(mut guard) = dynamic_has_more_a.lock() {
                        *guard = has_more;
                    }
                    ui.set_dynamic_items(ModelRc::from(std::rc::Rc::new(VecModel::from(all_rows))));
                    publish_refresh(&ui, false, 1.0);
                    ui.set_dynamic_has_more(has_more);
                    ui.set_dynamic_status(String::new().into());
                    // The bar's counter is the client page over the loaded
                    // rows: a reset reload starts at 0, an append lands on the
                    // page the bar asked for (or keeps the current one).
                    if !append {
                        set_client_page(13, 0);
                    } else if let Some(target) = target_page {
                        set_client_page(13, target);
                    }
                    publish_list_offset(&ui);
                    skeleton_arm(&ui, covers as u64);
                    update_page_bar(&ui);
                    crate::debug_log(&format!("dynamic load rendered items={loaded}"));
                }
            });

            let sync = ThumbSync::new(format!("dynamic append={append}"), covers);
            let thumbs_started = Instant::now();
            let fallbacks_before = bilibili::thumb_fallbacks();
            let mut delivered = 0usize;
            let fetches = items.iter().take(covers).enumerate().map(|(index, item)| {
                let session = session.clone();
                let face = item.author_face.clone();
                let cover = if !item.archive_cover.is_empty() {
                    item.archive_cover.clone()
                } else {
                    item.images.first().cloned().unwrap_or_default()
                };
                let id = item.id_str.clone();
                async move {
                    let face_img = image_gated(&session, &face).await;
                    let cover_img = thumb_gated(&session, &cover).await;
                    (index, id, face_img, cover_img)
                }
            });
            let mut stream = futures::stream::iter(fetches).buffer_unordered(THUMB_CONCURRENCY);
            while let Some((index, id, face_img, cover_img)) = stream.next().await {
                if player_active() {
                    continue;
                }
                delivered += 1;
                let row = base_len + index;
                let weak_c = weak.clone();
                let dynamic_ids_c = dynamic_ids.clone();
                let sync_c = sync.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_c.upgrade() {
                        let applied = ThumbGuard::Ids(dynamic_ids_c, id).allows(row);
                        if applied {
                            patch_model_thumb(
                                ui.get_dynamic_items(),
                                row,
                                to_thumb(&face_img),
                                |item_row, image| item_row.face = image,
                            );
                            patch_model_thumb(
                                ui.get_dynamic_items(),
                                row,
                                to_thumb(&cover_img),
                                |item_row, image| item_row.cover = image,
                            );
                        }
                        skeleton_settle(&ui, 1);
                        sync_c.settle(applied);
                    }
                });
            }
            crate::debug_log(&format!(
                "dynamic thumbs n={} fallback={} ms={}",
                delivered,
                bilibili::thumb_fallbacks().saturating_sub(fallbacks_before),
                thumbs_started.elapsed().as_millis()
            ));
            flush_missing(&weak, &sync, covers.saturating_sub(delivered));
        });
    }));
}

/// Refresh the 消息 unread badge.
pub(crate) fn load_unread(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let unread = session.msg_unread().await.unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_msg_unread_total(to_i32(unread.total));
                    ui.set_msg_unread_text(
                        format!("回复 {} · @ {} · 赞 {}", unread.reply, unread.at, unread.like)
                            .into(),
                    );
                }
            });
        });
    }));
}

/// Load one 回复我的 / @我的 / 收到的赞 list.
pub(crate) fn load_msg_list(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    tab: i32,
) {
    let kind = match tab {
        0 => bilibili::MsgKind::Reply,
        1 => bilibili::MsgKind::At,
        _ => bilibili::MsgKind::Like,
    };
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_msg_loading(true);
        ui.set_msg_status(String::new().into());
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let page = match session.msg_list(kind).await {
                Ok(page) => page,
                Err(err) => {
                    crate::debug_log(&format!("msg_list err={err:#}"));
                    let message = format!("消息加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_msg_loading(false);
                            ui.set_msg_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let items = page.items;
            let avatars = futures::stream::iter(items.iter().map(|item| {
                let session = session.clone();
                let url = item.avatar.clone();
                async move {
                    image_gated(&session, &url).await
                }
            }))
            .buffered(THUMB_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
            let rows: Vec<MsgRowData> = items
                .into_iter()
                .zip(avatars)
                .map(|(item, avatar)| MsgRowData {
                    nickname: item.nickname,
                    avatar,
                    title: item.title,
                    detail: item.detail,
                    time: time_ago(item.time),
                    count: if item.count > 0 {
                        format!("{}条", item.count)
                    } else {
                        String::new()
                    },
                })
                .collect();
            let total = rows.len();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let items: Vec<MsgRow> = rows
                        .into_iter()
                        .map(|row| MsgRow {
                            nickname: row.nickname.into(),
                            avatar: to_thumb(&row.avatar),
                            title: row.title.into(),
                            detail: row.detail.into(),
                            time: row.time.into(),
                            count: row.count.into(),
                        })
                        .collect();
                    ui.set_msg_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_msg_loading(false);
                    ui.set_msg_status(
                        if total == 0 {
                            "暂无消息".into()
                        } else {
                            format!("共 {total} 条").into()
                        },
                    );
                    update_page_bar(&ui);
                }
            });
        });
    }));
}

/// Load the 私信 session list (names resolved via `space_info`).
pub(crate) fn load_sessions(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    session_meta: Arc<Mutex<Vec<(i64, String)>>>,
) {
    let Some(session) = active_session(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_msg_loading(true);
        ui.set_msg_status(String::new().into());
        update_page_bar(&ui);
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let sessions = match session.dm_sessions().await {
                Ok(list) => list,
                Err(err) => {
                    crate::debug_log(&format!("dm_sessions err={err:#}"));
                    let message = format!("私信会话加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_msg_loading(false);
                            ui.set_msg_status(message.into());
                            update_page_bar(&ui);
                        }
                    });
                    return;
                }
            };
            let sessions: Vec<_> = sessions
                .into_iter()
                .take(bilibili::DM_SESSION_LIMIT)
                .collect();
            let fetched = futures::stream::iter(sessions.iter().map(|entry| {
                let session = session.clone();
                let talker = entry.talker_id;
                async move {
                    match session.space_info(talker).await {
                        Ok(info) => {
                            let face = image_gated(&session, &info.face).await;
                            let name = if info.name.is_empty() {
                                format!("用户 {talker}")
                            } else {
                                info.name
                            };
                            (name, face)
                        }
                        Err(_) => (format!("用户 {talker}"), None),
                    }
                }
            }))
            .buffered(4)
            .collect::<Vec<_>>()
            .await;
            let mut meta: Vec<(i64, String)> = Vec::with_capacity(sessions.len());
            let rows: Vec<SessionRowData> = sessions
                .into_iter()
                .zip(fetched)
                .map(|(entry, (name, face))| {
                    meta.push((entry.talker_id, name.clone()));
                    SessionRowData {
                        talker_id: entry.talker_id.to_string(),
                        name,
                        avatar: face,
                        preview: entry.preview,
                        time: time_ago(entry.at_time / 1000),
                        unread: if entry.unread > 0 {
                            format!("{}", entry.unread)
                        } else {
                            String::new()
                        },
                    }
                })
                .collect();
            let total = rows.len();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut guard) = session_meta.lock() {
                        *guard = meta;
                    }
                    let items: Vec<SessionRow> = rows
                        .into_iter()
                        .map(|row| SessionRow {
                            talker_id: row.talker_id.into(),
                            name: row.name.into(),
                            avatar: to_thumb(&row.avatar),
                            preview: row.preview.into(),
                            time: row.time.into(),
                            unread: row.unread.into(),
                        })
                        .collect();
                    ui.set_session_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_msg_loading(false);
                    ui.set_msg_status(
                        if total == 0 {
                            "暂无会话".into()
                        } else {
                            format!("显示最近 {total} 个会话").into()
                        },
                    );
                    update_page_bar(&ui);
                }
            });
        });
    }));
}

/// Load one 私信 session's messages (read-only).
pub(crate) fn load_dm_messages(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    talker_id: i64,
) {
    let Some((session, my_mid)) = active_parts(&session_slot) else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_dm_loading(true);
        ui.set_dm_status(String::new().into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let messages = match session.dm_messages(talker_id).await {
                Ok(list) => list,
                Err(err) => {
                    crate::debug_log(&format!("dm_messages err={err:#}"));
                    let message = format!("私信加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_dm_loading(false);
                            ui.set_dm_status(message.into());
                        }
                    });
                    return;
                }
            };
            let rows: Vec<DmRow> = messages
                .into_iter()
                .map(|message| DmRow {
                    from_me: message.sender_uid == my_mid,
                    text: if message.text.is_empty() {
                        "[非文本消息]".to_string()
                    } else {
                        message.text
                    }
                    .into(),
                    time: time_ago(message.timestamp).into(),
                })
                .collect();
            let total = rows.len();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_dm_items(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
                    ui.set_dm_loading(false);
                    if total == 0 {
                        ui.set_dm_status("暂无消息".into());
                    }
                }
            });
        });
    }));
}

/// Load one page of the active account's 关注 / 粉丝 list.
pub(crate) fn load_relation(
    weak: Weak<AppWindow>,
    session_slot: SessionSlot,
    relation_state: Arc<Mutex<RelationState>>,
    page: i64,
) {
    let Some((session, mid)) = active_parts(&session_slot) else {
        return;
    };
    let (kind, page) = match relation_state.lock() {
        Ok(mut state) => {
            state.page = page.max(1);
            (state.kind, state.page)
        }
        Err(_) => return,
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_relation_loading(true);
        ui.set_relation_status(String::new().into());
    }
    crate::netpool::submit(crate::netpool::Prio::Normal, Box::new(move || {
        let Some(runtime) = crate::bilibili::shared_runtime() else {
            return;
        };
        runtime.block_on(async move {
            let result = if kind == 1 {
                session.fans(mid, page).await
            } else {
                session.followings(mid, page).await
            };
            let page_data = match result {
                Ok(data) => data,
                Err(err) => {
                    crate::debug_log(&format!("relation kind={kind} err={err:#}"));
                    let message = format!("用户列表加载失败：{err}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_relation_loading(false);
                            ui.set_relation_status(message.into());
                        }
                    });
                    return;
                }
            };
            let users = page_data.items;
            let faces = futures::stream::iter(users.iter().map(|user| {
                let session = session.clone();
                let url = user.face.clone();
                async move {
                    image_gated(&session, &url).await
                }
            }))
            .buffered(THUMB_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
            let rows: Vec<RelationRowData> = users
                .into_iter()
                .zip(faces)
                .map(|(user, face)| RelationRowData {
                    mid: user.mid.to_string(),
                    uname: user.uname,
                    face,
                    sign: user.sign,
                    official: user.official,
                })
                .collect();
            let total = page_data.total;
            let num = page_data.page;
            let has_more = page_data.has_more;
            let title = if kind == 1 { "粉丝" } else { "关注" };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut state) = relation_state.lock() {
                        state.page = num;
                        state.has_more = has_more;
                    }
                    let items: Vec<RelationRow> = rows
                        .into_iter()
                        .map(|row| RelationRow {
                            mid: row.mid.into(),
                            uname: row.uname.into(),
                            face: to_thumb(&row.face),
                            sign: row.sign.into(),
                            official: row.official.into(),
                        })
                        .collect();
                    ui.set_relation_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
                    ui.set_relation_loading(false);
                    ui.set_relation_title(title.into());
                    ui.set_relation_total(
                        format!("共 {} 人", format_view(total.max(0) as u64)).into(),
                    );
                    ui.set_relation_current_page(num as i32);
                    ui.set_relation_has_prev(num > 1);
                    ui.set_relation_has_next(has_more);
                }
            });
        });
    }));
}

pub(crate) fn to_i32(value: i64) -> i32 {
    value.clamp(0, i32::MAX as i64) as i32
}

/// Current Unix time in whole seconds, clamped to `i32`.
///
/// The UI's freshness chips (`now_ts`) compare this against a published
/// timestamp; seconds fit in `i32` until 2038, which is well past the useful
/// life of that indicator, and Slint's `int` is 32-bit.
pub(crate) fn now_unix_secs() -> i32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i32::MAX as u64) as i32)
        .unwrap_or(0)
}

/// A short relative time for a unix-seconds timestamp.
pub(crate) fn time_ago(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let diff = (now - secs).max(0);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{}分钟前", diff / 60)
    } else if diff < 86_400 {
        format!("{}小时前", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}天前", diff / 86_400)
    } else {
        let (_, month, day) = civil_from_days(secs.div_euclid(86_400));
        format!("{month:02}-{day:02}")
    }
}

/// Civil (year, month, day) for a day count since the Unix epoch
/// (Howard Hinnant's algorithm).
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod fav_view_tests {
    use super::*;

    pub(crate) fn video(bvid: &str) -> bilibili::FeedVideo {
        bilibili::FeedVideo {
            bvid: bvid.to_string(),
            ..bilibili::FeedVideo::default()
        }
    }

    pub(crate) fn folder(id: i64, count: u32) -> bilibili::FavFolder {
        bilibili::FavFolder {
            id,
            name: format!("folder-{id}"),
            count,
        }
    }

    #[test]
    pub(crate) fn pick_two_is_distinct_and_in_bounds() {
        let items: Vec<bilibili::FeedVideo> = (0..3).map(|i| video(&format!("BV{i}"))).collect();
        for _ in 0..32 {
            let picks = fav_pick_two(&items);
            assert_eq!(picks.len(), 2);
            assert_ne!(picks[0].bvid, picks[1].bvid);
            for pick in &picks {
                assert!(items.iter().any(|item| item.bvid == pick.bvid));
            }
        }
    }

    #[test]
    pub(crate) fn pick_two_handles_empty_and_single_pages() {
        assert!(fav_pick_two(&[]).is_empty());
        let one = [video("BV1")];
        let picks = fav_pick_two(&one);
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].bvid, "BV1");
    }

    #[test]
    pub(crate) fn push_unique_keeps_the_first_occurrence() {
        let mut cards = vec![video("BV1")];
        let mut seen: HashSet<String> = cards.iter().map(|video| video.bvid.clone()).collect();
        let more = [video("BV1"), video("BV2"), video(""), video("BV2"), video("BV3")];
        fav_push_unique(&mut cards, &mut seen, &more);
        let bvids: Vec<&str> = cards.iter().map(|video| video.bvid.as_str()).collect();
        assert_eq!(bvids, vec!["BV1", "BV2", "BV3"]);
    }

    #[test]
    pub(crate) fn largest_folders_prefers_count_and_caps_at_eight() {
        let folders: Vec<bilibili::FavFolder> = (0..10).map(|id| folder(id, id as u32)).collect();
        let picked = fav_largest_folders(&folders);
        assert_eq!(picked.len(), FAV_MIX_MAX_FOLDERS);
        // 9..2 by count, descending.
        assert_eq!(picked[0].count, 9);
        assert_eq!(picked[7].count, 2);
        assert!(fav_largest_folders(&[]).is_empty());
    }

    #[test]
    pub(crate) fn aspect_comes_from_the_decoded_cover() {
        let square: CoverPixels = (vec![0; 4], 100, 100);
        assert!((fav_aspect(&square) - 1.0).abs() < f32::EPSILON);
        let tall: CoverPixels = (vec![0; 4], 200, 300);
        assert!((fav_aspect(&tall) - 1.5).abs() < 0.0001);
        // Unknown dimensions fall back to the neutral 16:9.
        assert_eq!(fav_aspect(&(vec![0; 4], 0, 120)), FAV_DEFAULT_ASPECT);
        assert_eq!(fav_aspect(&(vec![0; 4], 120, 0)), FAV_DEFAULT_ASPECT);
    }

    #[test]
    pub(crate) fn shuffle_keeps_every_item() {
        let mut items: Vec<bilibili::FeedVideo> =
            (0..45).map(|i| video(&format!("BV{i}"))).collect();
        fav_shuffle(&mut items);
        let mut bvids: Vec<String> = items.iter().map(|video| video.bvid.clone()).collect();
        bvids.sort();
        let mut expected: Vec<String> = (0..45).map(|i| format!("BV{i}")).collect();
        expected.sort();
        assert_eq!(bvids, expected);
        // Degenerate lengths must not panic.
        let mut empty: Vec<bilibili::FeedVideo> = Vec::new();
        fav_shuffle(&mut empty);
        let mut one = vec![video("BV1")];
        fav_shuffle(&mut one);
        assert_eq!(one.len(), 1);
    }

    #[test]
    pub(crate) fn view_test_tabs_cover_all_or_one() {
        assert_eq!(fav_view_test_tabs("all", 3), vec![0, 1, 2, 3]);
        assert_eq!(fav_view_test_tabs("2", 3), vec![2]);
        assert_eq!(fav_view_test_tabs("9", 3), vec![3]);
        assert_eq!(fav_view_test_tabs("", 2), vec![0]);
    }

    #[test]
    pub(crate) fn mix_key_changes_invalidate_the_cached_stream() {
        // The cache is process-wide; use ids no other test touches and clear the
        // entries afterwards so ordering cannot leak between tests.
        let key = vec![-9001, -9002];
        fav_store(FavMode::Stream, None, key.clone(), &[video("BV1")], 2);
        assert!(fav_cached(FavMode::Stream, None, &key).is_some());
        assert!(fav_cached(FavMode::Stream, None, &[-9001]).is_none());
        let hit = fav_cached(FavMode::Stream, None, &key);
        assert_eq!(hit.map(|entry| entry.sources), Some(2));

        // The two 收藏 tabs build different cards from the same folder (the
        // 推荐流 feed seeded by it vs its plain 收藏夹 listing), so one mode's
        // entry must never answer the other's lookup.
        let folder = -9003i64;
        fav_store(
            FavMode::Stream,
            Some(folder),
            Vec::new(),
            &[video("BVstream")],
            1,
        );
        assert!(fav_cached(FavMode::Library, Some(folder), &[]).is_none());
        fav_store(
            FavMode::Library,
            Some(folder),
            Vec::new(),
            &[video("BVlibrary")],
            1,
        );
        let stream = fav_cached(FavMode::Stream, Some(folder), &[]);
        let library = fav_cached(FavMode::Library, Some(folder), &[]);
        assert_eq!(
            stream.map(|entry| entry.cards[0].bvid.clone()),
            Some("BVstream".to_string())
        );
        assert_eq!(
            library.map(|entry| entry.cards[0].bvid.clone()),
            Some("BVlibrary".to_string())
        );

        if let Ok(mut guard) = fav_view_cache().lock() {
            guard.stream.all = None;
            guard.stream.key.clear();
            guard.stream.folders.remove(&-9001);
            guard.stream.folders.remove(&folder);
            guard.library.folders.remove(&folder);
        }
    }
}
